// NOTE: do NOT set `windows_subsystem = "windows"` here. This binary is
// a console PTY wrapper that proxies the user's stdin/stdout to `claude`;
// detaching from the console (as the tray daemon does) would sever the
// whole proxy. It must stay a console subsystem program.

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
    /// Title text sniffed from OSC 0/1/2 in claude's PTY output.
    TitleHint(String),
}

/// Strips OSC 1004 focus reports (`ESC [ I` / `ESC [ O`) from the user's
/// stdin stream, emitting them on a channel and returning the bytes still
/// destined for the PTY.
///
/// Stateful across reads so a report split across a read boundary is
/// still recognised — but it carries over only a trailing `ESC [`
/// (two bytes), never a lone `ESC`. A bare Escape keypress is just `ESC`
/// with no `[`, so refusing to hold it keeps Escape responsive (vim et
/// al.), which is the pitfall the byte-at-a-time approach was avoiding.
/// The remaining un-handled case — a report split as `…ESC` | `[I…` — is
/// rare (focus reports arrive as their own terminal write, not byte-
/// interleaved with input) and degrades to leaking the bytes, never to
/// delaying user input.
#[derive(Default)]
struct FocusStripper {
    /// Carried prefix from the previous read: `[]`, or `[ESC, '[']`.
    pending: Vec<u8>,
}

impl FocusStripper {
    fn process(&mut self, buf: &[u8], tx: &mpsc::UnboundedSender<WrapperEvent>) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.pending.len() + buf.len());
        // prefix = how much of `ESC [` we currently hold: 0, 1, or 2.
        let mut prefix = self.pending.len();
        self.pending.clear();

        for &b in buf {
            match prefix {
                0 => {
                    if b == 0x1b {
                        prefix = 1;
                    } else {
                        out.push(b);
                    }
                }
                1 => {
                    // Have ESC.
                    if b == b'[' {
                        prefix = 2;
                    } else {
                        out.push(0x1b);
                        if b == 0x1b {
                            prefix = 1; // a fresh ESC
                        } else {
                            out.push(b);
                            prefix = 0;
                        }
                    }
                }
                _ => {
                    // Have ESC [.
                    match b {
                        b'I' => {
                            let _ = tx.send(WrapperEvent::FocusIn);
                            prefix = 0;
                        }
                        b'O' => {
                            let _ = tx.send(WrapperEvent::FocusOut);
                            prefix = 0;
                        }
                        _ => {
                            // Some other CSI (e.g. ESC [ Z) — flush ESC [
                            // and reprocess this byte.
                            out.push(0x1b);
                            out.push(b'[');
                            if b == 0x1b {
                                prefix = 1;
                            } else {
                                out.push(b);
                                prefix = 0;
                            }
                        }
                    }
                }
            }
        }

        // Carry over only a complete `ESC [`; flush a lone trailing ESC
        // immediately so the Escape key isn't delayed.
        match prefix {
            1 => out.push(0x1b),
            2 => self.pending.extend_from_slice(&[0x1b, b'[']),
            _ => {}
        }
        out
    }
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
    // JetBrains family (IntelliJ, Android Studio, PyCharm, Goland, …)
    // exposes `$TERMINAL_EMULATOR=JetBrains-JediTerm`. They don't emit
    // OSC 1004; the daemon uses this to suppress idle alerts that the
    // user couldn't otherwise clear by focusing the terminal. We
    // borrow `pane_id` to carry the macOS bundle id from
    // `__CFBundleIdentifier` so the daemon's F20-raise adapter can
    // bring the correct IDE forward (e.g. `com.google.android.studio`
    // vs `com.jetbrains.intellij`).
    if env("TERMINAL_EMULATOR").as_deref() == Some("JetBrains-JediTerm") {
        return HostTerminal {
            kind: HostTerminalKind::JetBrains,
            pane_id: env("__CFBundleIdentifier"),
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    // Linux terminals — order matters: more specific env vars first so
    // a Konsole shell with VTE_VERSION (rare but possible) is correctly
    // tagged as Konsole rather than the GNOME-Terminal catch-all.
    if env("KONSOLE_VERSION").is_some() {
        return HostTerminal {
            kind: HostTerminalKind::Konsole,
            pane_id: None,
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    if env("ALACRITTY_LOG").is_some() {
        return HostTerminal {
            kind: HostTerminalKind::Alacritty,
            pane_id: None,
            program_version,
            tmux_socket: None,
            window_id,
        };
    }
    if env("GNOME_TERMINAL_SCREEN").is_some() || env("VTE_VERSION").is_some() {
        return HostTerminal {
            kind: HostTerminalKind::GnomeTerminal,
            pane_id: None,
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
            // `--ssh` with no following value is a usage error — don't
            // silently degrade to local mode, which would surprise a user
            // who meant to connect to a remote box.
            eprintln!("coredeck-claude: --ssh requires a <user@host> argument");
            std::process::exit(2);
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

/// Stable wrapper id for `--ssh <host>`. The first invocation against a
/// host generates a fresh UUID and writes it under
/// `<data_dir>/wrappers/<sanitized-host>`; subsequent invocations read
/// it back so claude processes alive in remote tmux keep correlating to
/// the same wrapper across reconnects.
///
/// On any IO failure we silently fall back to a random UUID — the
/// reconnect-survival behaviour is a nice-to-have, not load-bearing for
/// the common path.
fn persistent_wrapper_id_for_host(host: &str) -> String {
    let path = match wrapper_id_cache_path(host) {
        Some(p) => p,
        None => return Uuid::new_v4().to_string(),
    };
    if let Ok(s) = std::fs::read_to_string(&path) {
        let trimmed = s.trim();
        if Uuid::parse_str(trimmed).is_ok() {
            return trimmed.to_string();
        }
    }
    let id = Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &id);
    id
}

fn wrapper_id_cache_path(host: &str) -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "coredeck", "CoreDeck")?;
    let key: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(dirs.data_dir().join("wrappers").join(key))
}

/// Local TCP port the daemon listens on, parsed out of `daemon_addr`
/// (`host:port` form). Falls back to the default daemon address's port on
/// a parse miss, so the wrapper's tunnel port can't silently diverge from
/// the daemon's actual default.
fn local_daemon_port(daemon_addr: &str) -> u16 {
    fn port_of(addr: &str) -> Option<u16> {
        addr.rsplit(':').next().and_then(|p| p.parse().ok())
    }
    port_of(daemon_addr)
        .or_else(|| port_of(DEFAULT_DAEMON_ADDR))
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
    // The block is gated on `tmux ls` (returns 0 only when a server
    // with sessions exists) so we don't accidentally spawn a tmux server
    // on a box that doesn't have one running. We can't use `tmux info`:
    // it exits 1 when invoked without an attached client (which is
    // exactly our case — sshd command form has no tmux client). Errors
    // are swallowed so a misconfigured tmux doesn't break login.
    let tmux_propagate = r##"if command -v tmux >/dev/null 2>&1 && tmux ls >/dev/null 2>&1; then
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

/// True while the host terminal is in raw mode with OSC 1004 focus
/// reporting enabled. Lets `restore_terminal` be idempotent and safe to
/// call from the panic hook, the signal task, and the normal exit path
/// in any order.
static TERMINAL_RAW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Best-effort terminal restore: undo OSC 1004 focus reporting and leave
/// raw mode. No-op unless raw mode was enabled; only the first caller
/// does work.
fn restore_terminal() {
    if !TERMINAL_RAW.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(FOCUS_REPORTING_DISABLE);
    let _ = handle.flush();
    drop(handle);
    let _ = terminal::disable_raw_mode();
}

fn main() {
    init_tracing();
    // Restore the user's terminal on *any* panic (any thread) — the
    // raw-mode + OSC 1004 state otherwise outlives us and garbles the
    // user's shell.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        prev_hook(info);
    }));
    let exit_code = match run() {
        Ok(code) => code,
        Err(e) => {
            // Make sure the user always sees the error even after we toggled raw mode.
            restore_terminal();
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
    let claude_bin =
        std::env::var(CLAUDE_BINARY_ENV).unwrap_or_else(|_| DEFAULT_CLAUDE_BINARY.to_string());
    let daemon_addr =
        std::env::var(DAEMON_ADDR_ENV).unwrap_or_else(|_| DEFAULT_DAEMON_ADDR.to_string());
    // --ssh <host> on the CLI takes precedence; fall back to the env var
    // so a user can `export COREDECK_SSH_HOST=dev-box` and just type
    // `claude`. None = local mode (today's behaviour).
    let ssh_host = extract_ssh_host(&mut args).or_else(|| std::env::var(SSH_HOST_ENV).ok());
    // Local mode rolls a fresh UUID per invocation. Remote mode uses a
    // host-keyed persistent id so claude alive in remote tmux keeps
    // correlating to the same wrapper across reconnect cycles.
    let wrapper_id = match &ssh_host {
        Some(host) => persistent_wrapper_id_for_host(host),
        None => Uuid::new_v4().to_string(),
    };
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
        TERMINAL_RAW.store(true, std::sync::atomic::Ordering::SeqCst);
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
    // through unchanged; an OSC 0/1/2 title sniffer runs alongside and
    // emits title hints to the WS task without touching the stdout stream.
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
                        if !matches!(param, 0..=2) {
                            return;
                        }
                        // Lossy decode: a body truncated mid-codepoint at
                        // the OSC_BODY_CAP would fail a strict from_utf8
                        // and drop an otherwise-good title. Replace the
                        // stray bytes instead of discarding the title.
                        let text = String::from_utf8_lossy(body);
                        if let Some(title) = keep_title_hint(&text) {
                            titles.push(title);
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
        let mut stripper = FocusStripper::default();
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let forward = stripper.process(&buf[..n], &focus_tx_for_stdin);
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

    let is_remote = ssh_host.is_some();
    let exit_code: i32 = rt.block_on(async move {
        let ws_handle = tokio::spawn(run_ws(
            daemon_addr.clone(),
            wrapper_id.clone(),
            pid,
            cwd.to_string_lossy().to_string(),
            started_at_unix,
            host_terminal,
            is_remote,
            Arc::clone(&master_writer),
            focus_rx,
        ));

        // SIGTERM/SIGHUP: restore the terminal before dying, otherwise
        // the user's shell is left in raw mode with focus reports
        // spraying `^[[I`/`^[[O` into the prompt. The child claude still
        // holds the PTY slave; our exit closes the master and the kernel
        // HUPs the child's process group, so it winds down on its own.
        #[cfg(unix)]
        let _signal_handle = tokio::spawn(async {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut hup)) = (
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            let code = tokio::select! {
                _ = term.recv() => 143, // 128 + SIGTERM
                _ = hup.recv() => 129,  // 128 + SIGHUP
            };
            restore_terminal();
            std::process::exit(code);
        });

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

        // The WS close itself is the unregister signal — the daemon tears
        // the wrapper entry down when the connection drops.
        ws_handle.abort();

        Ok::<i32, anyhow::Error>(exit_status.exit_code() as i32)
    })?;

    // No-op when stdin wasn't a tty (raw mode never enabled).
    restore_terminal();
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
    is_remote: bool,
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
            is_remote,
            wrapper_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };
        let txt = match serde_json::to_string(&reg) {
            Ok(s) => s,
            Err(_) => return,
        };
        if ws_tx.send(Message::Text(txt.into())).await.is_err() {
            backoff_ms = next_backoff_ms(0);
            continue 'reconnect;
        }

        // Coalesce any backlog that piled up while the daemon was down.
        // `focus_rx` isn't drained during the offline backoff, so hours
        // of per-keystroke title updates and stale focus flips can be
        // waiting. Collapse them to the latest focus state + latest
        // title and send just those, rather than replaying the history
        // (which would churn the daemon's active-wrapper/alert logic).
        {
            let mut latest_focus: Option<bool> = None;
            let mut latest_title: Option<String> = None;
            while let Ok(ev) = focus_rx.try_recv() {
                match ev {
                    WrapperEvent::FocusIn => latest_focus = Some(true),
                    WrapperEvent::FocusOut => latest_focus = Some(false),
                    WrapperEvent::TitleHint(t) => latest_title = Some(t),
                }
            }
            let mut coalesced: Vec<WrapperToDaemon> = Vec::new();
            if let Some(focused) = latest_focus {
                coalesced.push(WrapperToDaemon::FocusChanged {
                    wrapper_id: wrapper_id.clone(),
                    focused,
                });
            }
            if let Some(title) = latest_title {
                coalesced.push(WrapperToDaemon::TitleHint {
                    wrapper_id: wrapper_id.clone(),
                    title,
                });
            }
            let mut send_failed = false;
            for msg in coalesced {
                if let Ok(txt) = serde_json::to_string(&msg) {
                    if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                        send_failed = true;
                        break;
                    }
                }
            }
            if send_failed {
                backoff_ms = next_backoff_ms(backoff_ms);
                continue 'reconnect;
            }
        }

        let connected_at = std::time::Instant::now();

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
                        DaemonToWrapper::Registered { daemon_version, .. } => {
                            let daemon_version = daemon_version.as_deref().unwrap_or("<pre-0.2>");
                            debug!(daemon_version, "wrapper registered with daemon");
                            if daemon_version != env!("CARGO_PKG_VERSION") {
                                debug!(
                                    wrapper = env!("CARGO_PKG_VERSION"),
                                    daemon = daemon_version,
                                    "wrapper/daemon version mismatch"
                                );
                            }
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
        // A connection that lived a while then dropped is a normal flap —
        // reconnect promptly (1s). One that died almost immediately (the
        // daemon accepting then closing in a crash loop) must back off
        // exponentially instead of hammering at a fixed 1 Hz forever.
        if connected_at.elapsed() >= std::time::Duration::from_secs(10) {
            backoff_ms = 1000;
        } else {
            backoff_ms = next_backoff_ms(backoff_ms);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn drain(rx: &mut mpsc::UnboundedReceiver<WrapperEvent>) -> Vec<WrapperEvent> {
        let mut v = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            v.push(ev);
        }
        v
    }

    #[test]
    fn focus_stripper_contiguous() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FocusStripper::default();
        // FocusIn embedded between normal bytes.
        let out = s.process(b"ab\x1b[Icd", &tx);
        assert_eq!(out, b"abcd");
        assert!(matches!(drain(&mut rx)[..], [WrapperEvent::FocusIn]));
    }

    #[test]
    fn focus_stripper_split_across_reads() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FocusStripper::default();
        // "ESC [" ends read 1; "O" starts read 2 → FocusOut, no leaked bytes.
        let out1 = s.process(b"x\x1b[", &tx);
        assert_eq!(out1, b"x");
        assert!(drain(&mut rx).is_empty());
        let out2 = s.process(b"Oy", &tx);
        assert_eq!(out2, b"y");
        assert!(matches!(drain(&mut rx)[..], [WrapperEvent::FocusOut]));
    }

    #[test]
    fn focus_stripper_lone_escape_not_held() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FocusStripper::default();
        // A lone trailing ESC (Escape key) must be forwarded immediately,
        // not held — otherwise interactive Escape would be delayed.
        let out = s.process(b"\x1b", &tx);
        assert_eq!(out, b"\x1b");
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn focus_stripper_passes_other_csi() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FocusStripper::default();
        // Shift-Tab (ESC [ Z) must pass through untouched.
        let out = s.process(b"\x1b[Z", &tx);
        assert_eq!(out, b"\x1b[Z");
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn osc_sniffer_emits_title_bel_terminated() {
        let mut sniffer = OscSniffer::new();
        let mut titles = Vec::new();
        sniffer.feed(b"\x1b]2;my title\x07rest", |p, body| {
            if matches!(p, 0..=2) {
                titles.push(String::from_utf8_lossy(body).into_owned());
            }
        });
        assert_eq!(titles, vec!["my title".to_string()]);
    }

    #[test]
    fn osc_sniffer_st_terminated_and_split() {
        let mut sniffer = OscSniffer::new();
        let mut titles = Vec::new();
        let mut emit = |p: u32, body: &[u8]| {
            if matches!(p, 0..=2) {
                titles.push(String::from_utf8_lossy(body).into_owned());
            }
        };
        // Body split across two feeds, ST (ESC \) terminated.
        sniffer.feed(b"\x1b]0;hel", &mut emit);
        sniffer.feed(b"lo\x1b\\", &mut emit);
        assert_eq!(titles, vec!["hello".to_string()]);
    }

    #[test]
    fn keep_title_hint_strips_and_filters() {
        // Default cwd title dropped.
        assert_eq!(keep_title_hint("~/proj — Claude Code"), None);
        assert_eq!(keep_title_hint("Claude Code"), None);
        // Leading spinner glyph stripped.
        assert_eq!(
            keep_title_hint("✻ Building the thing"),
            Some("Building the thing".to_string())
        );
        // Plain title kept.
        assert_eq!(
            keep_title_hint("my session"),
            Some("my session".to_string())
        );
        // Empty/whitespace dropped.
        assert_eq!(keep_title_hint("   "), None);
    }

    #[test]
    fn local_daemon_port_falls_back_to_default() {
        assert_eq!(local_daemon_port("127.0.0.1:5000"), 5000);
        assert_eq!(local_daemon_port("garbage"), 19384);
    }
}
