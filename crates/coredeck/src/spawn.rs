//! Open a fresh `coredeck-claude` session in the host terminal of the
//! currently-focused wrapper.
//!
//! Triggered by the F20+Stop chord on the device — the firmware emits
//! `KC_F24` (0x0073) when Stop is tapped while the Claude button is
//! held, and the daemon's input dispatcher routes that keycode here.
//!
//! Resolution:
//! - Pick the *focused* wrapper (`is_focused == true`); fall back to
//!   the device's active wrapper. Skip remote (`--ssh`) wrappers for
//!   v1 — driving a fresh session over the existing tunnel needs a
//!   separate SSH-side spawn flow.
//! - Pull the wrapper's `cwd` and `host_terminal`.
//! - Dispatch to a per-terminal spawn command. New tab/pane preferred
//!   over a new window where the host supports it.
//!
//! The wrapper binary is the daemon's sibling — derive its absolute
//! path from `current_exe()` so dev builds run from the project tree
//! reuse the same binary that just launched the daemon, rather than
//! whatever happens to be on PATH.
//!
//! Failures are non-fatal — log and move on. The chord is a UX
//! shortcut, not load-bearing for any data path.

use std::path::PathBuf;

use coredeck_protocol::{HostTerminal, HostTerminalKind};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::DaemonState;

const WRAPPER_BIN_NAME: &str = "coredeck-claude";

/// Open a fresh claude session in the focused (or active) wrapper's
/// cwd, in a new tab/pane of its host terminal.
pub async fn fresh_session_in_focused_cwd(state: &DaemonState) {
    let pick = pick_target(state).await;
    let Some((cwd, host, label)) = pick else {
        debug!("F20+Stop: no eligible wrapper (no focused/active local session)");
        return;
    };

    let wrapper_path = match wrapper_binary_path() {
        Some(p) => p,
        None => {
            warn!("F20+Stop: cannot locate sibling coredeck-claude binary");
            return;
        }
    };

    info!(
        cwd = %cwd,
        host = ?host.kind,
        target = %label,
        wrapper = %wrapper_path.display(),
        "opening fresh claude session",
    );

    if let Err(e) = spawn_in(&host, &cwd, &wrapper_path).await {
        warn!(error = %e, kind = ?host.kind, "fresh-session spawn failed");
    }
}

/// `(cwd, host_terminal, label)` of the wrapper to open the fresh
/// session for. Prefers the OS-focused wrapper; falls back to the
/// device's active wrapper. Skips `is_remote` wrappers — opening a
/// fresh session on a remote box needs a separate SSH spawn path.
async fn pick_target(state: &DaemonState) -> Option<(String, HostTerminal, String)> {
    let wrappers = state.wrappers.read().await;
    let claude = state.claude_state.read().await;

    // Focused first: the user is most likely looking at this terminal.
    let focused = wrappers
        .values()
        .find(|w| w.is_focused && !w.is_remote && w.host_terminal.is_some());
    if let Some(w) = focused {
        if let Some(host) = w.host_terminal.clone() {
            return Some((w.cwd.clone(), host, format!("focused:{}", w.wrapper_id)));
        }
    }

    // Fall back to the active session's wrapper (device's selected tab).
    let active_sid = claude.active_session_id.as_deref();
    if let Some(sid) = active_sid {
        if let Some(w) = wrappers
            .values()
            .find(|w| w.session_id.as_deref() == Some(sid) && !w.is_remote)
        {
            if let Some(host) = w.host_terminal.clone() {
                return Some((w.cwd.clone(), host, format!("active:{}", w.wrapper_id)));
            }
        }
    }

    None
}

/// Absolute path to the `coredeck-claude` binary that ships next to
/// this daemon (signed bundle: `Contents/MacOS/coredeck-claude`; dev
/// build: `target/debug/coredeck-claude`). `None` when `current_exe()`
/// fails or the sibling binary doesn't exist.
fn wrapper_binary_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidate = dir.join(WRAPPER_BIN_NAME);
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

async fn spawn_in(host: &HostTerminal, cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    match host.kind {
        HostTerminalKind::WezTerm => spawn_wezterm(cwd, wrapper).await,
        HostTerminalKind::Kitty => spawn_kitty(cwd, wrapper).await,
        HostTerminalKind::Tmux => spawn_tmux(host, cwd, wrapper).await,
        HostTerminalKind::ITerm2 => spawn_iterm2(cwd, wrapper).await,
        HostTerminalKind::AppleTerminal => spawn_apple_terminal(cwd, wrapper).await,
        HostTerminalKind::Ghostty => spawn_ghostty(cwd, wrapper).await,
        // JetBrains IDEs embed the terminal in a non-scriptable pane —
        // no public CLI to ask the IDE to open a fresh tab. Skip; the
        // user can drop a fresh wrapper into a regular terminal app
        // if they need one.
        HostTerminalKind::Unknown | HostTerminalKind::JetBrains => {
            debug!(
                kind = ?host.kind,
                "fresh-session: no spawn adapter for this host terminal",
            );
            Ok(())
        }
    }
}

// ── Per-terminal adapters ──────────────────────────────────────────

async fn spawn_wezterm(cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    run(
        "wezterm",
        &[
            "cli",
            "spawn",
            "--cwd",
            cwd,
            "--",
            wrapper.to_string_lossy().as_ref(),
        ],
    )
    .await
}

async fn spawn_kitty(cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    let cwd_arg = format!("--cwd={}", cwd);
    run(
        "kitty",
        &[
            "@",
            "launch",
            "--type=tab",
            &cwd_arg,
            wrapper.to_string_lossy().as_ref(),
        ],
    )
    .await
}

/// Tmux: open a new window in the same session, pre-cd'd to `cwd` and
/// running the wrapper. The socket is recorded in the wrapper's
/// `tmux_socket` ($TMUX = "<socket>,<pid>,<session>" — we only need
/// the socket).
async fn spawn_tmux(host: &HostTerminal, cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    let socket = host
        .tmux_socket
        .as_deref()
        .and_then(|s| s.split(',').next())
        .unwrap_or("");
    let wrapper_str = wrapper.to_string_lossy().to_string();
    if socket.is_empty() {
        return run("tmux", &["new-window", "-c", cwd, &wrapper_str]).await;
    }
    run("tmux", &["-S", socket, "new-window", "-c", cwd, &wrapper_str]).await
}

/// iTerm2 via AppleScript: create a new tab in the current window,
/// then write `cd <cwd> && <wrapper>` plus a newline so the new shell
/// starts the wrapper. Quotes use AppleScript-compatible escapes.
#[cfg(target_os = "macos")]
async fn spawn_iterm2(cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    let cmd = format!(
        "cd {} && exec {}",
        shell_quote(cwd),
        shell_quote(wrapper.to_string_lossy().as_ref())
    );
    let escaped = applescript_quote(&cmd);
    let script = format!(
        r#"tell application "iTerm"
            activate
            tell current window
                create tab with default profile
                tell current session to write text "{escaped}"
            end tell
        end tell"#,
    );
    run("osascript", &["-e", &script]).await
}

#[cfg(not(target_os = "macos"))]
async fn spawn_iterm2(_cwd: &str, _wrapper: &PathBuf) -> Result<(), String> {
    debug!("iTerm2 spawn requested on non-macOS — ignoring");
    Ok(())
}

/// Terminal.app via AppleScript: `do script` opens a new window (or
/// reuses the front-most one if the user prefers — that's user-config
/// territory). Pre-cd to `cwd` and exec the wrapper.
#[cfg(target_os = "macos")]
async fn spawn_apple_terminal(cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    let cmd = format!(
        "cd {} && exec {}",
        shell_quote(cwd),
        shell_quote(wrapper.to_string_lossy().as_ref())
    );
    let escaped = applescript_quote(&cmd);
    let script = format!(
        r#"tell application "Terminal"
            activate
            do script "{escaped}"
        end tell"#,
    );
    run("osascript", &["-e", &script]).await
}

#[cfg(not(target_os = "macos"))]
async fn spawn_apple_terminal(_cwd: &str, _wrapper: &PathBuf) -> Result<(), String> {
    debug!("Terminal.app spawn requested on non-macOS — ignoring");
    Ok(())
}

/// Ghostty has no stable spawn-into-existing-instance CLI yet (as of
/// 2026). Best-effort: launch a new window via `open -na Ghostty` with
/// `--working-directory` and `-e` so it runs the wrapper. Falls back
/// to the user's default behaviour if `open` isn't available.
#[cfg(target_os = "macos")]
async fn spawn_ghostty(cwd: &str, wrapper: &PathBuf) -> Result<(), String> {
    let dir_arg = format!("--working-directory={}", cwd);
    run(
        "open",
        &[
            "-na",
            "Ghostty",
            "--args",
            &dir_arg,
            "-e",
            wrapper.to_string_lossy().as_ref(),
        ],
    )
    .await
}

#[cfg(not(target_os = "macos"))]
async fn spawn_ghostty(_cwd: &str, _wrapper: &PathBuf) -> Result<(), String> {
    debug!("Ghostty spawn requested on non-macOS — ignoring");
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────

/// POSIX single-quote shell escaping. Matches the helper used by
/// `coredeck-claude` for SSH command building.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Escape `s` for inclusion in an AppleScript string literal — only `"`
/// and `\` need escaping. The caller wraps the result in surrounding
/// double quotes themselves.
#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out
}

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
