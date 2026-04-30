// Hide console window on Windows release builds
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

//! coredeck-claude — thin PTY wrapper around the `claude` CLI that
//! registers each session with the local CoreDeck daemon.
//!
//! The user runs this in place of `claude` (typically via shell alias).
//! The wrapper:
//!
//! - spawns `claude` in a PTY and proxies stdin/stdout transparently,
//! - sets `COREDECK_WRAPPER_ID` in claude's environment so the
//!   SessionStart hook script can correlate the Claude `session_id` to
//!   this wrapper instance,
//! - opens a WebSocket to the daemon to accept `Write` commands — bytes
//!   pushed back into claude's PTY stdin (used by the daemon for
//!   HID-driven mode toggles and soft-key text injection).
//!
//! Daemon failures never block claude; the wrapper degrades to a plain
//! PTY passthrough.

use std::io::{IsTerminal, Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use coredeck_protocol::{
    DaemonToWrapper, HostTerminal, HostTerminalKind, WrapperToDaemon, DEFAULT_DAEMON_ADDR,
    WRAPPER_ENV_VAR,
};
use crossterm::terminal;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// OSC 1004 focus tracking: ask the host terminal to send `ESC [ I` on
/// focus-in and `ESC [ O` on focus-out.
const FOCUS_REPORTING_ENABLE: &[u8] = b"\x1b[?1004h";
const FOCUS_REPORTING_DISABLE: &[u8] = b"\x1b[?1004l";

/// Focus event flowing from the stdin parser thread to the WS task.
#[derive(Debug, Clone, Copy)]
enum FocusEvent {
    In,
    Out,
}

/// Strip OSC 1004 focus events out of `buf` and emit them on `tx`.
/// Returns the bytes that should still be forwarded to the PTY. Only
/// recognises the patterns when they appear contiguously inside the same
/// read — avoids the "bare ESC stuck waiting for next byte" pitfall when
/// the user just presses Escape.
fn extract_focus_events(buf: &[u8], tx: &mpsc::UnboundedSender<FocusEvent>) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' {
            match buf[i + 2] {
                b'I' => {
                    let _ = tx.send(FocusEvent::In);
                    i += 3;
                    continue;
                }
                b'O' => {
                    let _ = tx.send(FocusEvent::Out);
                    i += 3;
                    continue;
                }
                _ => {}
            }
        }
        out.push(buf[i]);
        i += 1;
    }
    out
}

const DEFAULT_CLAUDE_BINARY: &str = "claude";
const CLAUDE_BINARY_ENV: &str = "COREDECK_CLAUDE_BIN";
const DAEMON_ADDR_ENV: &str = "COREDECK_DAEMON_ADDR";

/// Best-effort detection of which terminal application hosts this wrapper,
/// from environment variables set by the host. Order matters: tmux is
/// checked first because it nests inside another terminal — when `$TMUX`
/// is set we always claim Tmux, regardless of `TERM_PROGRAM`.
///
/// `$WINDOWID` is captured for every kind: it's the X11 window id used
/// by `wmctrl` on Linux to raise a specific window. Most X11-native
/// terminals set it (xterm, urxvt, some others); Wayland-native and
/// macOS terminals don't.
fn detect_host_terminal() -> HostTerminal {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let term_program = env("TERM_PROGRAM").unwrap_or_default();
    let program_version = env("TERM_PROGRAM_VERSION");
    let window_id = env("WINDOWID");

    if let Some(tmux) = env("TMUX") {
        return HostTerminal {
            kind: HostTerminalKind::Tmux,
            pane_id: env("TMUX_PANE"),
            program_version,
            tmux_socket: Some(tmux),
            window_id,
        };
    }
    if let Some(pane) = env("WEZTERM_PANE") {
        return HostTerminal {
            kind: HostTerminalKind::WezTerm,
            pane_id: Some(pane),
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    if let Some(win) = env("KITTY_WINDOW_ID") {
        return HostTerminal {
            kind: HostTerminalKind::Kitty,
            pane_id: Some(win),
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    if let Some(sess) = env("ITERM_SESSION_ID") {
        return HostTerminal {
            kind: HostTerminalKind::ITerm2,
            pane_id: Some(sess),
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    let kind = match term_program.as_str() {
        "ghostty" | "Ghostty" => HostTerminalKind::Ghostty,
        "iTerm.app" => HostTerminalKind::ITerm2,
        "WezTerm" => HostTerminalKind::WezTerm,
        "Apple_Terminal" => HostTerminalKind::AppleTerminal,
        _ => HostTerminalKind::Unknown,
    };
    HostTerminal {
        kind,
        pane_id: None,
        program_version,
        tmux_socket: None,
        window_id,
    }
}

/// Shared writer to the child PTY's master end. Both the user's stdin
/// thread and the daemon-WS task write into this.
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
/// Shared owner of the master PTY. Required so the SIGWINCH task can
/// call `resize()` while reader/writer halves stay alive elsewhere.
type SharedMaster = Arc<Mutex<Box<dyn MasterPty + Send>>>;

fn main() {
    init_tracing();
    let exit_code = match run() {
        Ok(code) => code,
        Err(e) => {
            // Make sure the user always sees the error even after we toggled raw mode.
            let _ = terminal::disable_raw_mode();
            eprintln!("coredeck-claude: {:#}", e);
            1
        }
    };
    std::process::exit(exit_code);
}

fn init_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
    // Default to silent. The wrapper shares a tty with claude — any stdout
    // noise corrupts the user's view. Set COREDECK_LOG=debug to enable.
    let filter = EnvFilter::try_from_env("COREDECK_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
}

fn run() -> Result<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let wrapper_id = Uuid::new_v4().to_string();
    let claude_bin =
        std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_| DEFAULT_CLAUDE_BINARY.to_string());
    let daemon_addr =
        std::env::var(DAEMON_ADDR_ENV).unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_string());
    let cwd = std::env::current_dir().context("getting cwd")?;
    let pid = std::process::id();
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let host_terminal = detect_host_terminal();
    debug!(
        wrapper_id = %wrapper_id,
        claude_bin = %claude_bin,
        host_terminal = ?host_terminal.kind,
        "starting wrapper",
    );

    // PTY size from the user's terminal; fall back to a sane default.
    let (cols, rows) = terminal::size().unwrap_or((80, 24));

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = CommandBuilder::new(&claude_bin);
    for a in &args {
        cmd.arg(a);
    }
    cmd.env(WRAPPER_ENV_VAR, &wrapper_id);
    cmd.cwd(&cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {}", claude_bin))?;
    // Close our handle to the slave end; the child still owns its own.
    drop(pair.slave);

    // Wrap master so reader/writer/resize can be shared safely.
    let master: SharedMaster = Arc::new(Mutex::new(pair.master));
    let master_reader = master
        .lock()
        .expect("master lock poisoned")
        .try_clone_reader()
        .context("cloning master reader")?;
    let master_writer: SharedWriter = Arc::new(Mutex::new(
        master
            .lock()
            .expect("master lock poisoned")
            .take_writer()
            .context("taking master writer")?,
    ));

    let stdin_is_tty = std::io::stdin().is_terminal();
    if stdin_is_tty {
        terminal::enable_raw_mode().context("enable_raw_mode")?;
        // Enable OSC 1004 focus reporting in the host terminal. Modern
        // terminals (iTerm2, Kitty, WezTerm, Ghostty, modern xterm)
        // honour this; older ones ignore it and we silently degrade to
        // no focus tracking.
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(FOCUS_REPORTING_ENABLE);
        let _ = handle.flush();
    }

    // Channel from the stdin parser thread to the WS task carrying
    // focus events. Unbounded keeps the stdin path lock-free.
    let (focus_tx, focus_rx) = mpsc::unbounded_channel::<FocusEvent>();

    // PTY → user stdout (sync thread, tight loop).
    let pty_to_stdout = std::thread::spawn(move || {
        let mut reader = master_reader;
        let mut buf = [0u8; 8192];
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if handle.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = handle.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // user stdin → PTY (sync thread, tight loop). Inline-strips OSC 1004
    // focus reports (ESC [ I / ESC [ O) and forwards them to the WS
    // task; everything else passes through to claude.
    let writer_for_stdin: SharedWriter = Arc::clone(&master_writer);
    let focus_tx_for_stdin = focus_tx.clone();
    let _stdin_to_pty = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let forward = extract_focus_events(&buf[..n], &focus_tx_for_stdin);
                    if forward.is_empty() {
                        continue;
                    }
                    let mut w = match writer_for_stdin.lock() {
                        Ok(w) => w,
                        Err(_) => break,
                    };
                    if w.write_all(&forward).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    // Tokio runtime hosts the WS connection, SIGWINCH watcher, and child wait.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;

    let exit_code: i32 = rt.block_on(async move {
        let ws_handle = tokio::spawn(run_ws(
            daemon_addr.clone(),
            wrapper_id.clone(),
            pid,
            cwd.to_string_lossy().to_string(),
            started_at_unix,
            host_terminal,
            Arc::clone(&master_writer),
            focus_rx,
        ));

        #[cfg(unix)]
        let _sigwinch_handle = {
            let master_for_signal = Arc::clone(&master);
            tokio::spawn(async move {
                let mut sig = match tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::window_change(),
                ) {
                    Ok(s) => s,
                    Err(_) => return,
                };
                while sig.recv().await.is_some() {
                    if let Ok((c, r)) = terminal::size() {
                        if let Ok(m) = master_for_signal.lock() {
                            let _ = m.resize(PtySize {
                                rows: r,
                                cols: c,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                        }
                    }
                }
            })
        };

        // Wait for child exit on a blocking task.
        let exit_status = tokio::task::spawn_blocking(move || {
            let mut child = child;
            child.wait()
        })
        .await
        .context("joining child wait task")?
        .context("child.wait")?;

        // Best-effort goodbye: abort cleanly. Daemon also treats WS close as unregister.
        ws_handle.abort();

        Ok::<i32, anyhow::Error>(exit_status.exit_code() as i32)
    })?;

    if stdin_is_tty {
        // Best-effort: undo OSC 1004 so the host terminal stops emitting
        // focus events at us after exit.
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(FOCUS_REPORTING_DISABLE);
        let _ = handle.flush();
        drop(handle);
        let _ = terminal::disable_raw_mode();
    }
    // The stdin reader thread is still blocked on stdin.read; let process exit reap it.
    let _ = pty_to_stdout.join();

    Ok(exit_code)
}

async fn run_ws(
    daemon_addr: String,
    wrapper_id: String,
    pid: u32,
    cwd: String,
    started_at_unix: u64,
    host_terminal: HostTerminal,
    pty_writer: SharedWriter,
    mut focus_rx: mpsc::UnboundedReceiver<FocusEvent>,
) {
    let url = format!("ws://{}/wrapper-ws", daemon_addr);
    let mut backoff_ms: u64 = 0;
    let mut announced_offline = false;

    'reconnect: loop {
        if backoff_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }

        let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(v) => v,
            Err(e) => {
                if !announced_offline {
                    warn!(error = %e, url = %url, "wrapper WS connect failed; running without daemon (will keep retrying)");
                    announced_offline = true;
                } else {
                    debug!(error = %e, "wrapper WS reconnect attempt failed");
                }
                backoff_ms = next_backoff_ms(backoff_ms);
                continue 'reconnect;
            }
        };
        if announced_offline {
            info!(url = %url, "wrapper WS reconnected");
            announced_offline = false;
        } else {
            info!(url = %url, "wrapper WS connected");
        }

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        let reg = WrapperToDaemon::Register {
            wrapper_id: wrapper_id.clone(),
            pid,
            cwd: cwd.clone(),
            started_at_unix,
            host_terminal: Some(host_terminal.clone()),
        };
        let txt = match serde_json::to_string(&reg) {
            Ok(s) => s,
            Err(_) => return,
        };
        if ws_tx.send(Message::Text(txt.into())).await.is_err() {
            backoff_ms = next_backoff_ms(0);
            continue 'reconnect;
        }

        loop {
            tokio::select! {
                msg = ws_rx.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            debug!(error = %e, "wrapper WS read error");
                            break;
                        }
                        None => break,
                    };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    let cmd: DaemonToWrapper = match serde_json::from_str(text.as_str()) {
                        Ok(c) => c,
                        Err(e) => {
                            debug!(error = %e, "wrapper got malformed daemon msg");
                            continue;
                        }
                    };
                    match cmd {
                        DaemonToWrapper::Registered { .. } => {
                            debug!("wrapper registered with daemon");
                        }
                        DaemonToWrapper::Write { bytes } => {
                            if let Ok(mut w) = pty_writer.lock() {
                                if w.write_all(&bytes).is_err() {
                                    debug!("writing daemon-supplied bytes to PTY failed");
                                } else {
                                    let _ = w.flush();
                                }
                            }
                        }
                    }
                }
                ev = focus_rx.recv() => {
                    let Some(ev) = ev else { continue };
                    let focused = matches!(ev, FocusEvent::In);
                    let msg = WrapperToDaemon::FocusChanged {
                        wrapper_id: wrapper_id.clone(),
                        focused,
                    };
                    let txt = match serde_json::to_string(&msg) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                        break;
                    }
                    debug!(focused, "wrapper sent focus update");
                }
            }
        }

        debug!("wrapper WS disconnected; will reconnect");
        // Don't hot-loop if the daemon hangs up immediately after Register.
        backoff_ms = 1000;
    }
}

/// Bounded exponential backoff: 1s → 2s → 4s → … → 30s cap.
fn next_backoff_ms(prev: u64) -> u64 {
    if prev == 0 {
        1000
    } else {
        prev.saturating_mul(2).min(30_000)
    }
}
