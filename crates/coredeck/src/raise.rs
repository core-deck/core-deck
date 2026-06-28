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
//!     - Linux/KDE (X11 or Wayland): `gdbus` script load into KWin's
//!       Scripting interface, activates by `resourceClass`. Handles
//!       both Plasma 5 (`activeClient`) and Plasma 6 (`activeWindow`).
//!     - Linux/GNOME Wayland: nothing built-in. Mutter doesn't expose
//!       window management to clients (security stance). Users can
//!       install the Window Calls extension or run an X11 session.
//!     - Linux/Wayland (other compositors): no portable primitive —
//!       falls through to wmctrl which won't help on pure Wayland.
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
        HostTerminalKind::Ghostty | HostTerminalKind::AppleTerminal | HostTerminalKind::Unknown => {
            activate_outer(host).await
        }
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

/// Linux raise-by-class. Tries three paths in order, falling through
/// on each failure (debug-logged) so a session that's e.g. KDE
/// Wayland still has a working raise after `wmctrl -ia` errors out:
///
/// 1. `$WINDOWID + wmctrl -ia` — X11 / XWayland sessions where the
///    terminal captured its X11 window id.
/// 2. KWin DBus scripting — KDE Plasma, X11 or Wayland.
/// 3. `wmctrl -x -a <class>` — pure X11 sessions without WINDOWID.
///
/// GNOME Wayland and other compositors without a published window-
/// management API will end up at step 3 (which won't help on pure
/// Wayland). That's documented in `docs/linux-setup.md`.
#[cfg(target_os = "linux")]
async fn raise_linux_by_class(host: &HostTerminal, class: &str) -> Result<(), String> {
    if let Some(wid) = host.window_id.as_deref() {
        if !wid.is_empty() {
            match run("wmctrl", &["-ia", wid]).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    debug!(error = %e, "wmctrl -ia $WINDOWID failed; trying KWin/class fallbacks")
                }
            }
        }
    }
    if is_kde() {
        match raise_kwin_dbus(class).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                debug!(error = %e, "KWin DBus raise failed; falling back to wmctrl class lookup")
            }
        }
    }
    run("wmctrl", &["-x", "-a", class]).await
}

/// True when this user's session is KDE Plasma. `XDG_CURRENT_DESKTOP`
/// is typically `KDE` or a colon-separated chain like `KDE:Plasma`;
/// match any segment case-insensitively.
#[cfg(target_os = "linux")]
fn is_kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|s| s.split(':').any(|p| p.eq_ignore_ascii_case("KDE")))
        .unwrap_or(false)
}

/// Activate a window by `resourceClass` via KWin's Scripting DBus
/// interface. Works on both X11 and Wayland KDE sessions — KWin is
/// the compositor in both cases. Uses `gdbus` (ships with glib2,
/// which we already depend on for the GTK tray) to avoid pulling in
/// a Rust DBus crate just for this path.
///
/// The JS snippet handles both Plasma 5 and 6:
/// - Plasma 5 exposes `workspace.clientList()` / `workspace.activeClient`
/// - Plasma 6 exposes `workspace.windowList()` / `workspace.activeWindow`
///
/// Best-effort cleanup: after running we ask KWin to stop the script
/// so loaded-script slots don't accumulate over a long session. The
/// stop call's success isn't checked.
#[cfg(target_os = "linux")]
async fn raise_kwin_dbus(class: &str) -> Result<(), String> {
    let class_lower = class.to_lowercase();
    let script = format!(
        r#"
const list = workspace.windowList ? workspace.windowList() : workspace.clientList();
for (const c of list) {{
    const cls = (c.resourceClass || "").toString().toLowerCase();
    if (cls === "{class_lower}") {{
        if ('activeWindow' in workspace) {{
            workspace.activeWindow = c;
        }} else {{
            workspace.activeClient = c;
        }}
        break;
    }}
}}
"#
    );

    let load_output = output_with_timeout(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest",
        "org.kde.KWin",
        "--object-path",
        "/Scripting",
        "--method",
        "org.kde.kwin.Scripting.loadScriptFromText",
        &script,
        "coredeck-raise",
    ]))
    .await
    .map_err(|e| format!("gdbus {e}"))?;

    if !load_output.status.success() {
        return Err(format!(
            "gdbus loadScriptFromText exited {}: {}",
            load_output.status,
            String::from_utf8_lossy(&load_output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&load_output.stdout);
    let id = parse_kwin_script_id(&stdout)
        .ok_or_else(|| format!("couldn't parse KWin script id from: {}", stdout.trim()))?;
    let object_path = format!("/Scripting/Script{id}");

    let run_output = output_with_timeout(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest",
        "org.kde.KWin",
        "--object-path",
        &object_path,
        "--method",
        "org.kde.kwin.Script.run",
    ]))
    .await
    .map_err(|e| format!("gdbus run-call {e}"))?;

    if !run_output.status.success() {
        return Err(format!(
            "gdbus Script.run exited {}: {}",
            run_output.status,
            String::from_utf8_lossy(&run_output.stderr).trim()
        ));
    }

    // Best-effort: ask KWin to drop the loaded-script slot. Failure
    // here doesn't matter — KWin will tidy up on its own restart and
    // a few orphan scripts are cheap.
    let _ = output_with_timeout(Command::new("gdbus").args([
        "call",
        "--session",
        "--dest",
        "org.kde.KWin",
        "--object-path",
        &object_path,
        "--method",
        "org.kde.kwin.Script.stop",
    ]))
    .await;

    Ok(())
}

/// Parse the reply from `gdbus call` to `loadScriptFromText`. Looks
/// like `(int32 5,)` (or `(uint32 5,)` on some versions); return the
/// number. Returns `None` on anything we don't recognise.
#[cfg(target_os = "linux")]
fn parse_kwin_script_id(reply: &str) -> Option<i64> {
    let trimmed = reply.trim();
    let inner = trimmed.strip_prefix('(')?.strip_suffix(",)")?.trim();
    inner.split_whitespace().next_back()?.parse().ok()
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

/// Cap on how long any raise helper command may run. These run from the
/// HID event loop's spawned tasks, so a hung `osascript` (e.g. blocked
/// on a permission dialog), stalled `gdbus`/`wmctrl`, or wedged terminal
/// CLI must not pin a task forever.
pub(crate) const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run a command to completion under `COMMAND_TIMEOUT`.
pub(crate) async fn output_with_timeout(cmd: &mut Command) -> Result<std::process::Output, String> {
    match tokio::time::timeout(COMMAND_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => Ok(o),
        Ok(Err(e)) => Err(format!("spawn failed: {e}")),
        Err(_) => Err(format!("timed out after {COMMAND_TIMEOUT:?}")),
    }
}

async fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let output = output_with_timeout(Command::new(program).args(args))
        .await
        .map_err(|e| format!("{program} {e}"))?;
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parses_int32_reply() {
        assert_eq!(parse_kwin_script_id("(int32 5,)\n"), Some(5));
        assert_eq!(parse_kwin_script_id("(int32 0,)"), Some(0));
        assert_eq!(parse_kwin_script_id("(int32 12345,)"), Some(12345));
    }

    #[test]
    fn parses_uint32_reply() {
        // Some KWin versions return uint32 instead of int32.
        assert_eq!(parse_kwin_script_id("(uint32 7,)"), Some(7));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_kwin_script_id(""), None);
        assert_eq!(parse_kwin_script_id("()"), None);
        assert_eq!(parse_kwin_script_id("(int32 notanumber,)"), None);
        assert_eq!(parse_kwin_script_id("int32 5"), None);
    }
}
