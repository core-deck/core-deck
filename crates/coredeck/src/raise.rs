//! Bring a wrapper's host-terminal window to the desktop foreground.
//!
//! Triggered by F20 on the device:
//! - F20 + alert → caller first promotes the alerting session to active,
//!   then calls `raise_for_session` so the user sees the alerting Claude.
//! - F20 + no alert → caller calls `raise_for_session` for the active
//!   session so the user lands on whatever the device is showing.
//!
//! The raise operation is split into two layers:
//! - **Inner** (CLI): terminal-specific commands like
//!   `wezterm cli activate-pane`, `kitty @ focus-window`,
//!   `tmux select-pane`. These already raise the right pane *within*
//!   their host and are cross-platform.
//! - **Outer** (OS): bring the host application's window to the
//!   desktop foreground. Platform-specific:
//!     - macOS: `osascript -e 'tell application "<X>" to activate'`
//!     - Linux/X11: `wmctrl -ia <WINDOWID>` if WINDOWID was captured
//!     - Linux/Wayland: no portable primitive — adapter no-ops
//!
//! Failures are non-fatal — F20 is a UX nicety, not load-bearing.

use coredeck_protocol::{HostTerminal, HostTerminalKind};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::DaemonState;

/// Raise the host terminal of the wrapper bound to `session_id`. No-op
/// when the session is unknown, the wrapper is older than the
/// host-terminal protocol field, or the adapter for that terminal has
/// no implementation yet.
pub async fn raise_for_session(state: &DaemonState, session_id: &str) {
    let host = {
        let wrappers = state.wrappers.read().await;
        wrappers
            .values()
            .find(|w| w.session_id.as_deref() == Some(session_id))
            .and_then(|w| w.host_terminal.clone())
    };
    let Some(host) = host else {
        debug!(session = %session_id, "raise: no wrapper or host_terminal for session");
        return;
    };
    raise_for_host(&host).await;
}

/// Raise the currently-active session's host terminal. No-op if no
/// active session is set.
pub async fn raise_active(state: &DaemonState) {
    let sid = {
        let claude = state.claude_state.read().await;
        claude.active_session_id.clone()
    };
    let Some(sid) = sid else {
        debug!("raise_active: no active session");
        return;
    };
    raise_for_session(state, &sid).await;
}

async fn raise_for_host(host: &HostTerminal) {
    info!(
        kind = ?host.kind,
        pane = ?host.pane_id,
        window = ?host.window_id,
        "raising host terminal",
    );
    let result = match host.kind {
        HostTerminalKind::Ghostty
        | HostTerminalKind::AppleTerminal
        | HostTerminalKind::Unknown => activate_outer(host).await,
        HostTerminalKind::WezTerm => raise_wezterm(host).await,
        HostTerminalKind::ITerm2 => raise_iterm2(host).await,
        HostTerminalKind::Kitty => raise_kitty(host).await,
        HostTerminalKind::Tmux => raise_tmux(host).await,
        // GNOME Terminal / Konsole / Alacritty don't expose a CLI to
        // raise a specific tab from outside, and they typically don't
        // set `$WINDOWID` either. Best-effort: ask wmctrl to activate
        // the first window matching the terminal's WM_CLASS; on a
        // pure-Wayland session this no-ops with a debug log.
        HostTerminalKind::GnomeTerminal => raise_linux_by_class(host, "gnome-terminal").await,
        HostTerminalKind::Konsole => raise_linux_by_class(host, "konsole").await,
        HostTerminalKind::Alacritty => raise_linux_by_class(host, "Alacritty").await,
        // JetBrains-family IDEs: focus the IDE app itself by its
        // macOS bundle id. The user's wrapper passes
        // `__CFBundleIdentifier` through `pane_id` (e.g.
        // `com.google.android.studio`). We don't try to focus the
        // specific terminal tool window — `activate` brings the IDE
        // forward and the embedded terminal usually retains keyboard
        // focus from where the user left it.
        HostTerminalKind::JetBrains => raise_jetbrains(host).await,
    };
    if let Err(e) = result {
        warn!(error = %e, kind = ?host.kind, "raise failed");
    }
}

// ── Per-terminal adapters ──────────────────────────────────────────

/// WezTerm exposes a rich CLI: `wezterm cli activate-pane --pane-id N`
/// brings the right pane forward within WezTerm. Combined with an OS-
/// level outer activate so WezTerm itself comes to the foreground if
/// it was behind another app.
async fn raise_wezterm(host: &HostTerminal) -> Result<(), String> {
    let _ = activate_outer(host).await;
    let Some(pane) = host.pane_id.as_deref() else {
        debug!("wezterm: no WEZTERM_PANE recorded; only outer-activated");
        return Ok(());
    };
    run("wezterm", &["cli", "activate-pane", "--pane-id", pane]).await
}

/// iTerm2 needs AppleScript with the saved session id to switch tabs.
/// `ITERM_SESSION_ID` is shaped like `w0t1p0:UUID` — the UUID is what
/// the AppleScript dictionary calls `unique id`. iTerm2 is macOS-only.
#[cfg(target_os = "macos")]
async fn raise_iterm2(host: &HostTerminal) -> Result<(), String> {
    let Some(raw) = host.pane_id.as_deref() else {
        return activate_macos_app("iTerm").await;
    };
    let uuid = raw.rsplit(':').next().unwrap_or(raw);
    let script = format!(
        r#"tell application "iTerm"
            activate
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        if unique id of s is "{uuid}" then
                            select t
                            select w
                            return
                        end if
                    end repeat
                end repeat
            end repeat
        end tell"#
    );
    run("osascript", &["-e", &script]).await
}

#[cfg(not(target_os = "macos"))]
async fn raise_iterm2(_host: &HostTerminal) -> Result<(), String> {
    debug!("iTerm2 raise requested on non-macOS — ignoring");
    Ok(())
}

/// Kitty's remote-control CLI: `kitty @ focus-window --match id:N`.
/// Requires `allow_remote_control` enabled in kitty.conf. On macOS we
/// also activate the app; on Linux the CLI raises the window itself.
async fn raise_kitty(host: &HostTerminal) -> Result<(), String> {
    let _ = activate_outer(host).await;
    let Some(win) = host.pane_id.as_deref() else {
        return Ok(());
    };
    let matcher = format!("id:{win}");
    run("kitty", &["@", "focus-window", "--match", &matcher]).await
}

/// JetBrains-family IDE: activate by macOS bundle id (passed through
/// `pane_id` on the wrapper side from `$__CFBundleIdentifier`). On
/// other platforms this is a no-op for now — Linux/Windows would need
/// a different focusing primitive (wmctrl by class, etc.).
#[cfg(target_os = "macos")]
async fn raise_jetbrains(host: &HostTerminal) -> Result<(), String> {
    let Some(bundle) = host.pane_id.as_deref().filter(|s| !s.is_empty()) else {
        debug!("raise: JetBrains terminal but no bundle id captured; skipping");
        return Ok(());
    };
    let script = format!(r#"tell application id "{bundle}" to activate"#);
    run("osascript", &["-e", &script]).await
}

#[cfg(not(target_os = "macos"))]
async fn raise_jetbrains(_host: &HostTerminal) -> Result<(), String> {
    debug!("JetBrains raise requested on non-macOS — ignoring");
    Ok(())
}

/// Linux raise-by-class: `wmctrl -x -a <class>` activates the first
/// window whose `WM_CLASS` second field matches `class`. Prefers
/// `$WINDOWID`-based activation when available (more precise — no
/// chance of grabbing a sibling window), falling back to class match.
/// Wayland sessions without an Xwayland-friendly compositor will see
/// `wmctrl` exit non-zero; we log and move on.
#[cfg(target_os = "linux")]
async fn raise_linux_by_class(host: &HostTerminal, class: &str) -> Result<(), String> {
    if let Some(wid) = host.window_id.as_deref() {
        if !wid.is_empty() {
            return run("wmctrl", &["-ia", wid]).await;
        }
    }
    run("wmctrl", &["-x", "-a", class]).await
}

#[cfg(not(target_os = "linux"))]
async fn raise_linux_by_class(_host: &HostTerminal, _class: &str) -> Result<(), String> {
    debug!("Linux class-based raise requested on non-Linux — ignoring");
    Ok(())
}

/// Tmux is nested inside another terminal. We can select the right tmux
/// pane from outside via the recorded socket, but we don't yet know the
/// *outer* terminal — the wrapper sees `$TMUX` and stops detecting at
/// Tmux. Best-effort: just select the pane; raising the outer app would
/// need a parent-process-tree walk we haven't implemented.
async fn raise_tmux(host: &HostTerminal) -> Result<(), String> {
    let Some(pane) = host.pane_id.as_deref() else {
        return Ok(());
    };
    // `$TMUX` is `<socket>,<pid>,<session>` — strip after the first comma.
    let socket = host
        .tmux_socket
        .as_deref()
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    if socket.is_empty() {
        return run("tmux", &["select-pane", "-t", pane]).await;
    }
    run("tmux", &["-S", socket, "select-pane", "-t", pane]).await
}

// ── Outer activate (OS-specific) ───────────────────────────────────

/// Bring the host terminal application to the desktop foreground.
/// Platform dispatch:
/// - macOS: `osascript ... activate` keyed by app name.
/// - Linux/X11: `wmctrl -ia $WINDOWID` if WINDOWID was captured.
/// - Other: no-op.
#[cfg(target_os = "macos")]
async fn activate_outer(host: &HostTerminal) -> Result<(), String> {
    let app = match host.kind {
        HostTerminalKind::Ghostty => "Ghostty",
        HostTerminalKind::WezTerm => "WezTerm",
        HostTerminalKind::ITerm2 => "iTerm",
        HostTerminalKind::Kitty => "kitty",
        HostTerminalKind::AppleTerminal => "Terminal",
        HostTerminalKind::Tmux
        | HostTerminalKind::Unknown
        | HostTerminalKind::JetBrains
        // Linux-native terminals: macOS doesn't run them, so the
        // activate-outer path is unreachable in practice. Ok'ing
        // keeps the match exhaustive without trying to translate.
        | HostTerminalKind::GnomeTerminal
        | HostTerminalKind::Konsole
        | HostTerminalKind::Alacritty => return Ok(()),
    };
    activate_macos_app(app).await
}

#[cfg(target_os = "linux")]
async fn activate_outer(host: &HostTerminal) -> Result<(), String> {
    let Some(wid) = host.window_id.as_deref() else {
        debug!("linux raise: no $WINDOWID captured; skipping outer activate");
        return Ok(());
    };
    activate_x11_window(wid).await
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn activate_outer(_host: &HostTerminal) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
async fn activate_macos_app(app: &str) -> Result<(), String> {
    let script = format!(r#"tell application "{app}" to activate"#);
    run("osascript", &["-e", &script]).await
}

/// `wmctrl -ia <WINDOWID>` — `-i` interprets the argument as a window
/// id (decimal or 0xhex), `-a` activates (raises and focuses) it.
/// Requires wmctrl installed and an X11 (or XWayland) session.
#[cfg(target_os = "linux")]
async fn activate_x11_window(window_id: &str) -> Result<(), String> {
    run("wmctrl", &["-ia", window_id]).await
}

// ── Helpers ────────────────────────────────────────────────────────

async fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{program} spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}
