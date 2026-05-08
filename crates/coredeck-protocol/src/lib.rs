//! Shared types and wire format for CoreDeck daemon ↔ app communication.
//!
//! This crate is intentionally lightweight (only `serde` + `serde_json`).
//! It defines:
//! - Device types shared between daemon and app (DeviceMode, DeviceState, etc.)
//! - WebSocket binary protocol (WsTag, frame encoding/decoding)
//! - HTTP REST request/response types

use serde::{Deserialize, Serialize};

// ── Device types (shared) ──────────────────────────────────────────

/// Device operating mode (LED indicator)
///
/// Cycle order on the device: Default -> Accept -> Plan -> Default
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum DeviceMode {
    #[default]
    Default = 0,
    Accept = 1,
    Plan = 2,
}

impl DeviceMode {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            1 => DeviceMode::Accept,
            2 => DeviceMode::Plan,
            _ => DeviceMode::Default,
        }
    }
}

impl std::fmt::Display for DeviceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceMode::Default => write!(f, "default"),
            DeviceMode::Accept => write!(f, "accept"),
            DeviceMode::Plan => write!(f, "plan"),
        }
    }
}

/// Device state parsed from state report
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceState {
    pub mode: DeviceMode,
    pub yolo: bool,
}

impl DeviceState {
    /// Parse from a single state byte.
    /// Bit layout: bits[1:0] = mode, bit[2] = yolo
    pub fn from_byte(byte: u8) -> Self {
        Self {
            mode: DeviceMode::from_byte(byte & 0x03),
            yolo: byte & 0x04 != 0,
        }
    }

    /// Encode to a single state byte.
    pub fn to_byte(&self) -> u8 {
        let mut b = self.mode as u8;
        if self.yolo {
            b |= 0x04;
        }
        b
    }
}

/// Display update data structure matching firmware JSON format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayUpdate {
    /// Session name
    pub session: String,
    /// Current task description (empty when idle)
    pub task: String,
    /// Second task line (pre-split by app), empty when unused
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task2: String,
    /// Tab states for all non-new-tab sessions
    pub tabs: Vec<u8>,
    /// Index into tabs array for the currently active tab
    pub active: usize,
    /// Context window usage percentage (from Claude Code statusline)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    /// Session cost in USD (from Claude Code statusline)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Model display name (from Claude Code statusline)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Tab state constants
pub const TAB_STATE_INACTIVE: u8 = 0;
pub const TAB_STATE_STARTED: u8 = 1;
pub const TAB_STATE_WORKING: u8 = 2;

/// Soft key type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SoftKeyType {
    Default = 0,
    Keycode = 1,
    String = 2,
    Sequence = 3,
}

impl SoftKeyType {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(SoftKeyType::Default),
            1 => Some(SoftKeyType::Keycode),
            2 => Some(SoftKeyType::String),
            3 => Some(SoftKeyType::Sequence),
            _ => None,
        }
    }
}

/// Soft key configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoftKeyConfig {
    pub index: u8,
    pub key_type: SoftKeyType,
    pub data: Vec<u8>,
}

// ── WebSocket binary protocol ──────────────────────────────────────
//
// Every binary WS frame: [tag: u8][seq_lo: u8][seq_hi: u8][payload...]
// seq is a 16-bit little-endian sequence number.
// Daemon→App events use seq=0. App→Daemon commands use seq>0.
// Daemon→App responses echo the seq from the original command.

/// WebSocket message tags: App → Daemon (commands)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WsCommandTag {
    UpdateDisplay = 0x01,
    Ping = 0x02,
    SetBrightness = 0x03,
    SetSoftKey = 0x04,
    GetSoftKey = 0x05,
    ResetSoftKeys = 0x06,
    SetMode = 0x07,
    Alert = 0x08,
    GetVersion = 0x09,
    ClearAlert = 0x0A,
    /// Push raw bytes into a `coredeck-claude` wrapper's PTY (mode toggle,
    /// soft-key text). JSON payload: `{"wrapper_id":"...","bytes":[..]}`.
    /// Empty `wrapper_id` targets the active wrapper.
    WrapperWrite = 0x0B,
    /// Set which wrapper the daemon considers "active" (focus). JSON
    /// payload: `{"wrapper_id":"..."}`.
    SetActiveWrapper = 0x0C,
}

impl WsCommandTag {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::UpdateDisplay),
            0x02 => Some(Self::Ping),
            0x03 => Some(Self::SetBrightness),
            0x04 => Some(Self::SetSoftKey),
            0x05 => Some(Self::GetSoftKey),
            0x06 => Some(Self::ResetSoftKeys),
            0x07 => Some(Self::SetMode),
            0x08 => Some(Self::Alert),
            0x09 => Some(Self::GetVersion),
            0x0A => Some(Self::ClearAlert),
            0x0B => Some(Self::WrapperWrite),
            0x0C => Some(Self::SetActiveWrapper),
            _ => None,
        }
    }
}

/// WebSocket message tags: Daemon → App (events, seq=0)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WsEventTag {
    DeviceConnected = 0x80,
    DeviceDisconnected = 0x81,
    StateChanged = 0x82,
    KeyEvent = 0x83,
    TypeString = 0x84,
    /// Claude Code hook event forwarded from daemon (JSON payload)
    ClaudeHookEvent = 0x85,
    AppControl = 0x89,
    /// Snapshot of all live `coredeck-claude` wrappers and their per-session
    /// state. JSON payload is `WrapperTabList`. Re-emitted on every change
    /// (wrapper register/unregister, hook update, active-tab change).
    WrapperTabList = 0x8A,
}

impl WsEventTag {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x80 => Some(Self::DeviceConnected),
            0x81 => Some(Self::DeviceDisconnected),
            0x82 => Some(Self::StateChanged),
            0x83 => Some(Self::KeyEvent),
            0x84 => Some(Self::TypeString),
            0x85 => Some(Self::ClaudeHookEvent),
            0x89 => Some(Self::AppControl),
            0x8A => Some(Self::WrapperTabList),
            _ => None,
        }
    }
}

/// WebSocket message tags: Daemon → App (responses, seq echoed)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WsResponseTag {
    SoftKeyResponse = 0x85,
    VersionResponse = 0x86,
    CommandAck = 0x87,
    CommandError = 0x88,
}

impl WsResponseTag {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x85 => Some(Self::SoftKeyResponse),
            0x86 => Some(Self::VersionResponse),
            0x87 => Some(Self::CommandAck),
            0x88 => Some(Self::CommandError),
            _ => None,
        }
    }
}

/// AppControl actions sent from daemon tray to app via WS
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AppControlAction {
    ShowWindow = 0x01,
    HideWindow = 0x02,
}

impl AppControlAction {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::ShowWindow),
            0x02 => Some(Self::HideWindow),
        _ => None,
        }
    }
}

/// Device info sent in DeviceConnected event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub firmware: String,
}

// ── Frame encoding/decoding helpers ────────────────────────────────

/// Build a binary WS frame: [tag][seq_lo][seq_hi][payload...]
pub fn encode_ws_frame(tag: u8, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(3 + payload.len());
    frame.push(tag);
    frame.push((seq & 0xFF) as u8);
    frame.push((seq >> 8) as u8);
    frame.extend_from_slice(payload);
    frame
}

/// Decode the header of a binary WS frame. Returns (tag, seq, payload_slice).
pub fn decode_ws_frame(data: &[u8]) -> Option<(u8, u16, &[u8])> {
    if data.len() < 3 {
        return None;
    }
    let tag = data[0];
    let seq = (data[1] as u16) | ((data[2] as u16) << 8);
    Some((tag, seq, &data[3..]))
}

// ── HTTP REST types ────────────────────────────────────────────────

/// Default daemon listen address
pub const DEFAULT_DAEMON_ADDR: &str = "127.0.0.1:19384";

/// Response for GET /api/status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Whether the USB device is physically present (enumerated)
    #[serde(default)]
    pub device_available: bool,
    /// Whether a HID device is connected (interface open, communicating)
    pub device_connected: bool,
    /// Device name (if available or connected)
    pub device_name: Option<String>,
    /// Firmware version (if connected)
    pub firmware_version: Option<String>,
    /// Current device mode
    pub device_mode: DeviceMode,
    /// YOLO mode active
    pub device_yolo: bool,
    /// Whether a WebSocket client (app) is connected (has the lock)
    pub ws_locked: bool,
}

/// Request body for POST /api/display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayUpdateRequest {
    pub session: String,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub task2: String,
    #[serde(default)]
    pub tabs: Vec<u8>,
    #[serde(default)]
    pub active: usize,
}

/// Request body for POST /api/alert
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRequest {
    pub tab: usize,
    pub session: String,
    pub text: String,
    pub details: Option<String>,
}

/// Request body for POST /api/alert/clear
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearAlertRequest {
    pub tab: usize,
}

/// Request body for POST /api/brightness
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrightnessRequest {
    pub level: u8,
    #[serde(default)]
    pub save: bool,
}

/// Request body for POST /api/mode
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetModeRequest {
    pub mode: DeviceMode,
}

/// Generic API error response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ── Wrapper protocol (coredeck-claude ↔ daemon) ────────────────────
//
// Each `coredeck-claude` wrapper opens a long-lived WebSocket to the
// daemon at `/wrapper-ws` and exchanges JSON text frames. Keep the
// schema simple — it's not in the hot path, and human-readable wins.
//
// The HTTP `/wrapper/register` endpoint correlates wrapper_id ↔ session_id;
// it's POSTed by a tiny SessionStart command hook script that reads
// $COREDECK_WRAPPER_ID from its inherited environment.

/// Environment variable the wrapper sets so the SessionStart hook
/// script can identify which wrapper a Claude session belongs to.
pub const WRAPPER_ENV_VAR: &str = "COREDECK_WRAPPER_ID";

/// Identification of the terminal application hosting the wrapper, captured
/// at startup from environment variables. The daemon uses this to dispatch
/// "raise terminal" requests (F20 button) to the right adapter.
///
/// Detection is best-effort and may report `Unknown` — in that case the
/// daemon's raise call is a no-op (or logs a warning).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostTerminalKind {
    Ghostty,
    WezTerm,
    ITerm2,
    Kitty,
    Tmux,
    AppleTerminal,
    /// GNOME Terminal (and the broader VTE family that exposes the
    /// same `--working-directory=… -- cmd` CLI: Tilix, gnome-console,
    /// xfce4-terminal in some configs). Detected via
    /// `$GNOME_TERMINAL_SCREEN` first, falling back to `$VTE_VERSION`.
    GnomeTerminal,
    /// KDE Konsole. Detected via `$KONSOLE_VERSION`.
    Konsole,
    /// Alacritty. Detected via `$ALACRITTY_LOG`.
    Alacritty,
    /// JetBrains-family embedded terminals (IntelliJ, Android Studio,
    /// PyCharm, Goland, …). Detected via `TERMINAL_EMULATOR=
    /// JetBrains-JediTerm`. They don't emit OSC 1004 focus reports, so
    /// the daemon suppresses idle alerts for sessions hosted in them
    /// (the user has no way to clear those alerts beyond F20).
    JetBrains,
    #[default]
    Unknown,
}

impl HostTerminalKind {
    /// True when the terminal is known to emit OSC 1004 focus reports.
    /// Drives whether the daemon raises an Idle alert for this session
    /// — without focus reporting, idle alerts can't be cleared by
    /// looking at the terminal, so they get stuck on the device until
    /// the user mashes F20. We err on the side of "supports" for
    /// `Unknown` because we'd rather show a clearable alert than miss
    /// one in a terminal we just haven't taught the daemon about.
    pub fn supports_focus_reporting(&self) -> bool {
        match self {
            HostTerminalKind::JetBrains => false,
            _ => true,
        }
    }
}

/// Snapshot of the wrapper's host-terminal context at startup.
/// `pane_id` semantics depend on `kind`:
/// - WezTerm: `$WEZTERM_PANE` (pane id, decimal)
/// - Kitty: `$KITTY_WINDOW_ID`
/// - iTerm2: `$ITERM_SESSION_ID`
/// - Tmux: `$TMUX_PANE` (e.g. `%12`); `tmux_socket` carries `$TMUX`
/// - Ghostty / Apple Terminal: typically `None` (no per-window env)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostTerminal {
    pub kind: HostTerminalKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_version: Option<String>,
    /// Tmux socket path from `$TMUX` (`<socket>,<pid>,<session>` form).
    /// Only populated when `kind == Tmux`. Useful so the daemon can run
    /// `tmux -S <socket> select-window …` from outside the wrapper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    /// X11 window id from `$WINDOWID` (decimal). Set by xterm, urxvt, and
    /// some other Linux terminals; used by the Linux raise path
    /// (`wmctrl -ia <id>`). Empty on Wayland / when the terminal doesn't
    /// publish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
}

/// Messages sent by a wrapper to the daemon over /wrapper-ws.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WrapperToDaemon {
    /// First frame after connect — tells the daemon who we are.
    Register {
        wrapper_id: String,
        pid: u32,
        cwd: String,
        /// Unix epoch seconds at wrapper start
        started_at_unix: u64,
        /// Host terminal context for raise-to-front. Optional for forward
        /// compat with older wrappers; daemon treats absence as `Unknown`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_terminal: Option<HostTerminal>,
        /// Cached Claude `session_id` from a prior `SessionBound` frame.
        /// Sent on reconnect so a daemon that lost its in-memory wrapper
        /// map across a restart can re-bind without waiting for the next
        /// SessionStart hook. Absent on the very first connect (wrapper
        /// hasn't bound a session yet) and across older wrappers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// True when the wrapper is running in `--ssh` mode (claude lives
        /// on a remote box, hooks fire through an SSH reverse tunnel).
        /// The daemon prefixes the session label with `↦` so users can
        /// tell remote tabs apart at a glance.
        #[serde(default, skip_serializing_if = "is_false")]
        is_remote: bool,
    },
    /// Best-effort signal that the child claude exited (wrapper is about to close).
    /// The daemon also treats WS close as unregister.
    Goodbye {
        wrapper_id: String,
        exit_code: Option<i32>,
    },
    /// Host terminal focus changed (parsed from OSC 1004 reports on the
    /// wrapper's stdin). Daemon uses focus-in to set the wrapper as
    /// active and clear any alert tied to its session.
    FocusChanged {
        wrapper_id: String,
        focused: bool,
    },
    /// Title hint sniffed from claude's PTY output (OSC 9). The wrapper
    /// passes the bytes through to the host terminal as well — this is
    /// purely a side channel for the daemon's session-label fallback.
    TitleHint {
        wrapper_id: String,
        title: String,
    },
}

/// Messages sent by the daemon to a wrapper over /wrapper-ws.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonToWrapper {
    /// Ack the Register frame.
    Registered { wrapper_id: String },
    /// Write raw bytes to the child's PTY stdin (e.g. keystroke escapes,
    /// soft-key text, mode-cycle key).
    Write { bytes: Vec<u8> },
    /// Sent after the daemon binds a Claude `session_id` to this
    /// wrapper (typically right after the SessionStart hook posts
    /// to `/wrapper/register`). The wrapper caches the value and
    /// echoes it in the next `Register` frame so a daemon restart
    /// can restore the binding without waiting on the next hook.
    SessionBound { session_id: String },
}

/// Body for `POST /wrapper/register` — fired from the SessionStart command
/// hook to bind a Claude session_id to a wrapper_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperRegisterSession {
    pub wrapper_id: String,
    pub session_id: String,
}

/// One row in the daemon's view of "what's running." Combines wrapper info
/// (cwd, pid) with whatever the latest hook payload told us about the
/// Claude session bound to that wrapper.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WrapperTab {
    pub wrapper_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub cwd: String,
    pub pid: u32,
    /// Wall-clock wrapper start time (unix seconds) — useful for stable ordering.
    pub started_at_unix: u64,
    /// Custom session name (`--name` flag or `/rename`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    /// Most recent OSC 9 title sniffed from claude's PTY output. Used as
    /// a label fallback between `session_name` and the cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    /// Short summary of the user's most recent prompt — was used as a
    /// fallback session label but is no longer in the device's chain
    /// (first-prompt snippets like "fix the bug" produced uninformative
    /// labels). Still emitted for any external consumer that wants it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_summary: Option<String>,
    /// Currently in-progress TodoWrite item, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_todo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Most recent tool name (from PreToolUse), cleared on PostToolUse/Stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    /// Short human-readable activity description (tool + key input field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_task: Option<String>,
    /// Most recent tool's task summary, kept across the following Thinking
    /// phase so the device's second line stays informative while Claude
    /// thinks about the next step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tool_summary: Option<String>,
    /// Permission mode reported by hooks: "default", "plan", "acceptEdits", "bypassPermissions", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Firmware tab-state value for this row — one of `TAB_STATE_INACTIVE`,
    /// `TAB_STATE_STARTED`, `TAB_STATE_WORKING`. Computed by the daemon from
    /// (a) whether a live `SessionState` backs this wrapper and (b) the
    /// session's `active` flag. Replaces the prior `active: bool` field —
    /// the bool collapsed INACTIVE and STARTED, leaving stale tabs on the
    /// device for the ~200ms between `SessionEnd` and wrapper disconnect.
    #[serde(default)]
    pub tab_state: u8,
    /// Statusline-derived numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Display string for the first visible subagent row (from
    /// `subagentStatusLine`), when one is running. Already includes a
    /// `(N)` count prefix when more than one subagent is visible.
    /// `None` when no subagents are reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_label: Option<String>,
    /// Number of subagent rows last reported by `subagentStatusLine`.
    /// Zero when none are running.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub subagent_count: u32,
    /// True when the underlying wrapper is in `--ssh` mode. UI surfaces
    /// (tray menu, device label) prefix the title with `↦` so the user
    /// can tell remote tabs apart at a glance.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_remote: bool,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Daemon → app snapshot of every live wrapper.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WrapperTabList {
    pub tabs: Vec<WrapperTab>,
    /// Wrapper currently considered "focused" — the target for HID input.
    /// Empty when no wrapper is connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_wrapper_id: Option<String>,
}

/// Body for `WrapperWrite` WS command (app → daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperWriteRequest {
    /// When empty, the daemon writes to whichever wrapper is currently active.
    #[serde(default)]
    pub wrapper_id: String,
    pub bytes: Vec<u8>,
}

/// Body for `SetActiveWrapper` WS command (app → daemon).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetActiveWrapperRequest {
    pub wrapper_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_mode_roundtrip() {
        for mode in [DeviceMode::Default, DeviceMode::Accept, DeviceMode::Plan] {
            assert_eq!(DeviceMode::from_byte(mode as u8), mode);
        }
    }

    #[test]
    fn test_device_state_byte_roundtrip() {
        let state = DeviceState { mode: DeviceMode::Plan, yolo: true };
        let byte = state.to_byte();
        let parsed = DeviceState::from_byte(byte);
        assert_eq!(parsed.mode, DeviceMode::Plan);
        assert!(parsed.yolo);
    }

    #[test]
    fn test_ws_frame_encode_decode() {
        let payload = b"hello";
        let frame = encode_ws_frame(0x01, 42, payload);
        let (tag, seq, data) = decode_ws_frame(&frame).unwrap();
        assert_eq!(tag, 0x01);
        assert_eq!(seq, 42);
        assert_eq!(data, b"hello");
    }

    #[test]
    fn test_ws_frame_empty_payload() {
        let frame = encode_ws_frame(0x82, 0, &[]);
        let (tag, seq, data) = decode_ws_frame(&frame).unwrap();
        assert_eq!(tag, 0x82);
        assert_eq!(seq, 0);
        assert!(data.is_empty());
    }

    #[test]
    fn test_ws_frame_too_short() {
        assert!(decode_ws_frame(&[0x01]).is_none());
        assert!(decode_ws_frame(&[0x01, 0x02]).is_none());
    }

    #[test]
    fn test_soft_key_type_roundtrip() {
        for t in [SoftKeyType::Default, SoftKeyType::Keycode, SoftKeyType::String, SoftKeyType::Sequence] {
            assert_eq!(SoftKeyType::from_byte(t as u8), Some(t));
        }
        assert_eq!(SoftKeyType::from_byte(4), None);
    }

    #[test]
    fn test_display_update_json() {
        let update = DisplayUpdate {
            session: "my-project".to_string(),
            task: "Reading files".to_string(),
            task2: String::new(),
            tabs: vec![0, 1, 2],
            active: 1,
            context_percent: Some(42.5),
            cost_usd: Some(1.23),
            model: Some("Opus".to_string()),
        };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"session\":\"my-project\""));
        assert!(json.contains("\"context_percent\":42.5"));
        assert!(json.contains("\"cost_usd\":1.23"));
        assert!(json.contains("\"model\":\"Opus\""));

        // Without optional fields — they should be omitted
        let update_minimal = DisplayUpdate {
            session: "test".to_string(),
            task: String::new(),
            task2: String::new(),
            tabs: vec![],
            active: 0,
            context_percent: None,
            cost_usd: None,
            model: None,
        };
        let json_min = serde_json::to_string(&update_minimal).unwrap();
        assert!(!json_min.contains("context_percent"));
        assert!(!json_min.contains("cost_usd"));
        assert!(!json_min.contains("model"));
    }

    #[test]
    fn test_daemon_status_json() {
        let status = DaemonStatus {
            device_available: true,
            device_connected: true,
            device_name: Some("Core Deck".to_string()),
            firmware_version: Some("1.0.0".to_string()),
            device_mode: DeviceMode::Default,
            device_yolo: false,
            ws_locked: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert!(parsed.device_connected);
        assert_eq!(parsed.device_name.as_deref(), Some("Core Deck"));
    }

    #[test]
    fn test_command_tags() {
        assert_eq!(WsCommandTag::from_byte(0x01), Some(WsCommandTag::UpdateDisplay));
        assert_eq!(WsCommandTag::from_byte(0x0A), Some(WsCommandTag::ClearAlert));
        assert_eq!(WsCommandTag::from_byte(0xFF), None);
    }

    #[test]
    fn test_event_tags() {
        assert_eq!(WsEventTag::from_byte(0x80), Some(WsEventTag::DeviceConnected));
        assert_eq!(WsEventTag::from_byte(0x89), Some(WsEventTag::AppControl));
        assert_eq!(WsEventTag::from_byte(0x00), None);
    }

    #[test]
    fn test_response_tags() {
        assert_eq!(WsResponseTag::from_byte(0x85), Some(WsResponseTag::SoftKeyResponse));
        assert_eq!(WsResponseTag::from_byte(0x88), Some(WsResponseTag::CommandError));
        assert_eq!(WsResponseTag::from_byte(0x00), None);
    }
}
