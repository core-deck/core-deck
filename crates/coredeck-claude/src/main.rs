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
use tracing::debug;
use uuid::Uuid;

/// OSC 1004 focus tracking: ask the host terminal to send `ESC [ I` on
/// focus-in and `ESC [ O` on focus-out.
const FOCUS_REPORTING_ENABLE: &[u8] = b"\x1b[?1004h";
const FOCUS_REPORTING_DISABLE: &[u8] = b"\x1b[?1004l";

/// Out-of-band events flowing from the stdin / PTY parser threads to
/// the WS task.
#[derive(Debug, Clone)]
enum WrapperEvent {
    FocusIn,
    FocusOut,
    /// Title text sniffed from OSC 9 in claude's PTY output.
    TitleHint(String),
}

/// Strip OSC 1004 focus events out of `buf` and emit them on `tx`.
/// Returns the bytes that should still be forwarded to the PTY. Only
/// recognises the patterns when they appear contiguously inside the same
/// read — avoids the "bare ESC stuck waiting for next byte" pitfall when
/// the user just presses Escape.
fn extract_focus_events(buf: &[u8], tx: &mpsc::UnboundedSender<WrapperEvent>) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' {
            match buf[i + 2] {
                b'I' => {
                    let _ = tx.send(WrapperEvent::FocusIn);
                    i += 3;
                    continue;
                }
                b'O' => {
                    let _ = tx.send(WrapperEvent::FocusOut);
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

/// Stateful sniffer for OSC sequences in claude's PTY output. We do
/// not strip the bytes — the host terminal may also use them — we just
/// observe and emit the OSC 9 body as a title hint. State has to
/// persist across reads because OSC bodies routinely span buffers.
struct OscSniffer {
    state: OscState,
    param: u32,
    has_param: bool,
    body: Vec<u8>,
}

enum OscState {
    Outside,
    Esc,
    OscParam,
    OscBody,
    OscBodyEsc,
}

const OSC_BODY_CAP: usize = 512;

impl OscSniffer {
    fn new() -> Self {
        Self {
            state: OscState::Outside,
            param: 0,
            has_param: false,
            body: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.state = OscState::Outside;
        self.param = 0;
        self.has_param = false;
        self.body.clear();
    }

    fn feed<F: FnMut(u32, &[u8])>(&mut self, buf: &[u8], mut emit: F) {
        for &b in buf {
            self.process(b, &mut emit);
        }
    }

    fn process<F: FnMut(u32, &[u8])>(&mut self, b: u8, emit: &mut F) {
        match self.state {
            OscState::Outside => {
                if b == 0x1b {
                    self.state = OscState::Esc;
                }
            }
            OscState::Esc => {
                if b == b']' {
                    self.state = OscState::OscParam;
                    self.param = 0;
                    self.has_param = false;
                    self.body.clear();
                } else {
                    self.state = OscState::Outside;
                }
            }
            OscState::OscParam => {
                if b.is_ascii_digit() {
                    self.has_param = true;
                    self.param = self
                        .param
                        .saturating_mul(10)
                        .saturating_add((b - b'0') as u32);
                } else if b == b';' {
                    self.state = OscState::OscBody;
                } else if b == 0x07 {
                    if self.has_param {
                        emit(self.param, &self.body);
                    }
                    self.reset();
                } else if b == 0x1b {
                    self.state = OscState::OscBodyEsc;
                } else {
                    self.reset();
                }
            }
            OscState::OscBody => {
                if b == 0x07 {
                    if self.has_param {
                        emit(self.param, &self.body);
                    }
                    self.reset();
                } else if b == 0x1b {
                    self.state = OscState::OscBodyEsc;
                } else if self.body.len() < OSC_BODY_CAP {
                    self.body.push(b);
                }
            }
            OscState::OscBodyEsc => {
                if b == b'\\' {
                    if self.has_param {
                        emit(self.param, &self.body);
                    }
                    self.reset();
                } else {
                    self.reset();
                }
            }
        }
    }
}

/// Filter for OSC titles before they reach the daemon. Two normalisations:
///
/// 1. Strip Claude Code's animated spinner glyph (✻, ✷, ⠋, …) plus the
///    following whitespace. Claude cycles a glyph through every title
///    update for progress feedback; without this, the device's 24-char
///    label is crowded by a flickering symbol that adds no signal.
///
/// 2. Drop the default `<cwd> — Claude Code` titles entirely — those
///    add nothing beyond the cwd we already use as fallback.
fn keep_title_hint(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Strip any leading run of whitespace and non-title-start characters.
    // `is_alphanumeric` is Unicode-aware, so titles in non-Latin scripts
    // start at the first letter; `/` `~` `.` `(` `[` `"` `'` `<` cover
    // the obvious ASCII title-starts so paths and quoted text survive.
    let stripped = trimmed.trim_start_matches(|c: char| {
        c.is_whitespace()
            || (!c.is_alphanumeric()
                && !matches!(c, '/' | '~' | '.' | '(' | '[' | '"' | '\'' | '<'))
    });
    if stripped.is_empty() {
        return None;
    }
    if stripped == "Claude Code" || stripped.ends_with(" Claude Code") {
        return None;
    }
    Some(stripped.to_string())
}

const DEFAULT_CLAUDE_BINARY: &str = "claude";
const CLAUDE_BINARY_ENV: &str = "COREDECK_CLAUDE_BIN";
const DAEMON_ADDR_ENV: &str = "COREDECK_DAEMON_ADDR";
const SSH_HOST_ENV: &str = "COREDECK_SSH_HOST";

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

/// Pull `--ssh <host>` (or `--ssh=<host>`) out of `args`, returning the
/// host string. Anything else stays in `args` for pass-through to claude.
fn extract_ssh_host(args: &mut Vec<String>) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--ssh" {
            if i + 1 < args.len() {
                let host = args.remove(i + 1);
                args.remove(i);
                return Some(host);
            }
            args.remove(i);
            return None;
        }
        if let Some(value) = args[i].strip_prefix("--ssh=") {
            let host = value.to_string();
            args.remove(i);
            return Some(host);
        }
        i += 1;
    }
    None
}

/// Single-quote `s` for safe inclusion in a POSIX shell command. Embeds
/// any internal `'` via the `'\''` escape, the same way printf %q does
/// for sh/bash. We can't use shell-words/shlex here without pulling a
/// dep just for one helper.
fn sh_quote(s: &str) -> String {
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

/// Local TCP port the daemon listens on, parsed out of `daemon_addr`
/// (`host:port` form). Falls back to 19384 on a parse miss.
fn local_daemon_port(daemon_addr: &str) -> u16 {
    daemon_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(19384)
}

/// Build the `ssh -t -R <port>:localhost:<port> <host> "<cmd>"`
/// invocation for remote-claude mode.
///
/// The remote command opens the user's *interactive* login shell with
/// `COREDECK_WRAPPER_ID` and `COREDECK_DAEMON_URL` already exported.
/// From there the user runs whatever they like: bare `claude`, `tmux
/// new -s work` then claude (so disconnects don't kill the session),
/// `mosh-server` paired with another tool, etc. The wrapper provides
/// the tunnel and the env; the user picks the workflow.
///
/// The remote port mirrors the local daemon port (default 19384) so
/// that on disconnect-and-reconnect the tunnel rebinds to the same
/// number. claude processes inside tmux have `COREDECK_DAEMON_URL`
/// baked into their env at startup; if the port changed they'd be
/// stranded. The mirror is the price of resilience: it implies one
/// wrapper per remote host at a time. `ExitOnForwardFailure=yes`
/// surfaces a clean error if a second wrapper tries to bind.
///
/// If `claude_args` is non-empty, those args become the initial
/// command run by the interactive shell (so existing
/// `coredeck-claude --ssh host -- claude --resume xxx` still works).
/// An empty `claude_args` drops the user straight into the shell.
///
/// Inline env (vs. ssh's SendEnv/AcceptEnv) keeps us independent of
/// sshd's config — those vary wildly by distro and managed setup.
fn build_ssh_command(
    host: &str,
    wrapper_id: &str,
    claude_args: &[String],
    daemon_port: u16,
) -> (CommandBuilder, u16) {
    let remote_port = daemon_port;

    // -li gives us a login *and* interactive shell so the user's full PATH
    // (.zshrc, .bashrc — interactive-only on most setups) is available.
    // With no command, it drops into a prompt. With -c, it runs the
    // command and exits when it does.
    let exec_part = if claude_args.is_empty() {
        "exec \"$SHELL\" -li".to_string()
    } else {
        let argv = claude_args
            .iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!("exec \"$SHELL\" -lic {}", sh_quote(&argv))
    };

    // tmux propagation. tmux servers capture env at *first-server-start*
    // time and don't refresh it for subsequent client attaches. So if the
    // user already has a tmux server running on the remote (very common
    // — a tmux user's server outlives any single ssh session), claude
    // launched inside any tmux pane gets stale (or empty) COREDECK_*.
    //
    // On every wrapper connect we push the fresh values into the running
    // server's global session env *and* into every existing session. New
    // panes anywhere (incl. inside an attached session) see the fresh
    // env. Existing panes still have their original env — they need a
    // relaunch — but that's the standard tmux env-refresh limitation.
    //
    // The block is gated on `tmux info` so we don't accidentally start a
    // tmux server on a box that doesn't have one running. Errors are
    // swallowed so a misconfigured tmux doesn't break login.
    let tmux_propagate = r##"if command -v tmux >/dev/null 2>&1 && tmux info >/dev/null 2>&1; then
    tmux setenv -g COREDECK_WRAPPER_ID "$COREDECK_WRAPPER_ID" 2>/dev/null || true
    tmux setenv -g COREDECK_DAEMON_URL "$COREDECK_DAEMON_URL" 2>/dev/null || true
    for s in $(tmux ls -F "#{session_name}" 2>/dev/null); do
        tmux setenv -t "$s" COREDECK_WRAPPER_ID "$COREDECK_WRAPPER_ID" 2>/dev/null || true
        tmux setenv -t "$s" COREDECK_DAEMON_URL "$COREDECK_DAEMON_URL" 2>/dev/null || true
    done
fi"##;

    // `export VAR=val` (with no command after) exports the var into the
    // current shell so subsequent commands (and the eventually-exec'd
    // user shell) see it. POSIX-portable across bash/zsh/dash.
    let remote_cmd = format!(
        "export COREDECK_WRAPPER_ID={}; export COREDECK_DAEMON_URL=http://127.0.0.1:{}; {}; {}",
        sh_quote(wrapper_id),
        remote_port,
        tmux_propagate,
        exec_part,
    );

    let mut cmd = CommandBuilder::new("ssh");
    cmd.arg("-t");
    cmd.arg("-R");
    cmd.arg(format!("{}:localhost:{}", remote_port, daemon_port));
    cmd.arg("-o");
    cmd.arg("ExitOnForwardFailure=yes");
    // Quieter ssh: skip the "Welcome to Ubuntu…" MOTD and the banner. Auth
    // prompts (passwords, passphrases) still surface — they go through the
    // PTY, not stderr.
    cmd.arg("-o");
    cmd.arg("LogLevel=ERROR");
    cmd.arg(host);
    cmd.arg(remote_cmd);
    (cmd, remote_port)
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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let wrapper_id = Uuid::new_v4().to_string();
    let claude_bin =
        std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_| DEFAULT_CLAUDE_BINARY.to_string());
    let daemon_addr =
        std::env::var(DAEMON_ADDR_ENV).unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_string());
    // --ssh <host> on the CLI takes precedence; fall back to the env var
    // so a user can `export COREDECK_SSH_HOST=dev-box` and just type
    // `claude`. None = local mode (today's behaviour).
    let ssh_host = extract_ssh_host(&mut args).or_else(|| std::env::var(SSH_HOST_ENV).ok());
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
        ssh_host = ?ssh_host,
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

    let (mut cmd, spawn_label) = match &ssh_host {
        Some(host) => {
            let port = local_daemon_port(&daemon_addr);
            let (cmd, remote_port) = build_ssh_command(host, &wrapper_id, &args, port);
            debug!(
                ssh_host = %host,
                remote_port,
                local_daemon_port = port,
                interactive = args.is_empty(),
                "wrapper using SSH reverse tunnel for remote shell",
            );
            (cmd, format!("ssh {}", host))
        }
        None => {
            let mut cmd = CommandBuilder::new(&claude_bin);
            for a in &args {
                cmd.arg(a);
            }
            cmd.env(WRAPPER_ENV_VAR, &wrapper_id);
            (cmd, claude_bin.clone())
        }
    };
    cmd.cwd(&cwd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .with_context(|| format!("spawning {}", spawn_label))?;
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

    // Channel from the stdin / PTY parser threads to the WS task,
    // carrying focus reports and OSC 9 title hints. Unbounded keeps the
    // hot loops lock-free.
    let (focus_tx, focus_rx) = mpsc::unbounded_channel::<WrapperEvent>();
    let title_tx = focus_tx.clone();

    // PTY → user stdout (sync thread, tight loop). Bytes are passed
    // through unchanged; an OSC 9 sniffer runs alongside and emits
    // title hints to the WS task without touching the stdout stream.
    let pty_to_stdout = std::thread::spawn(move || {
        let mut reader = master_reader;
        let mut buf = [0u8; 8192];
        let mut sniffer = OscSniffer::new();
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
                    let mut titles: Vec<String> = Vec::new();
                    sniffer.feed(&buf[..n], |param, body| {
                        // OSC 0/1/2 are the universal title channels
                        // (xterm/ECMA-48: 0 sets icon+window title, 1
                        // sets icon, 2 sets window). OSC 9 is for
                        // notifications (iTerm2) / taskbar progress
                        // (ConEmu) — NOT titles, despite my earlier
                        // assumption. ConEmu's `9;4;0;...` progress
                        // updates would otherwise show up on the device
                        // as "4;0;" right after session start.
                        if !matches!(param, 0 | 1 | 2) {
                            return;
                        }
                        if let Ok(text) = std::str::from_utf8(body) {
                            if let Some(title) = keep_title_hint(text) {
                                titles.push(title);
                            }
                        }
                    });
                    for title in titles {
                        let _ = title_tx.send(WrapperEvent::TitleHint(title));
                    }
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
    mut focus_rx: mpsc::UnboundedReceiver<WrapperEvent>,
) {
    let url = format!("ws://{}/wrapper-ws", daemon_addr);
    let mut backoff_ms: u64 = 0;
    let mut announced_offline = false;
    // Cache the daemon-bound session_id from the most recent
    // SessionBound frame. Echoed back in every Register so a daemon
    // restart can re-attach without waiting for the user's next prompt.
    let mut cached_session_id: Option<String> = None;

    'reconnect: loop {
        if backoff_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
        }

        let (ws_stream, _) = match tokio_tungstenite::connect_async(&url).await {
            Ok(v) => v,
            Err(e) => {
                // Daemon-down is a routine state during restart; never
                // a WARN. Anyone debugging this sets COREDECK_LOG=debug
                // and gets the full retry chatter.
                if !announced_offline {
                    debug!(error = %e, url = %url, "wrapper WS connect failed; running without daemon (will keep retrying)");
                    announced_offline = true;
                } else {
                    debug!(error = %e, "wrapper WS reconnect attempt failed");
                }
                backoff_ms = next_backoff_ms(backoff_ms);
                continue 'reconnect;
            }
        };
        if announced_offline {
            debug!(url = %url, "wrapper WS reconnected");
            announced_offline = false;
        } else {
            debug!(url = %url, "wrapper WS connected");
        }

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        let reg = WrapperToDaemon::Register {
            wrapper_id: wrapper_id.clone(),
            pid,
            cwd: cwd.clone(),
            started_at_unix,
            host_terminal: Some(host_terminal.clone()),
            session_id: cached_session_id.clone(),
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
                        DaemonToWrapper::SessionBound { session_id } => {
                            debug!(session_id = %session_id, "wrapper cached session_id from daemon");
                            cached_session_id = Some(session_id);
                        }
                    }
                }
                ev = focus_rx.recv() => {
                    let Some(ev) = ev else { continue };
                    let msg = match ev {
                        WrapperEvent::FocusIn => WrapperToDaemon::FocusChanged {
                            wrapper_id: wrapper_id.clone(),
                            focused: true,
                        },
                        WrapperEvent::FocusOut => WrapperToDaemon::FocusChanged {
                            wrapper_id: wrapper_id.clone(),
                            focused: false,
                        },
                        WrapperEvent::TitleHint(title) => WrapperToDaemon::TitleHint {
                            wrapper_id: wrapper_id.clone(),
                            title,
                        },
                    };
                    let txt = match serde_json::to_string(&msg) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                        break;
                    }
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
