//! Daemon shared state

use coredeck_protocol::{DaemonToWrapper, DeviceMode, HostTerminal, WrapperTabList};
use tokio::sync::mpsc;

/// Events emitted by the HID subsystem to the daemon core
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    /// HID device connected (interface opened, communicating)
    HidConnected {
        device_name: String,
        firmware_version: String,
    },
    /// HID device disconnected (interface closed or lost)
    HidDisconnected,
    /// USB device physically available (enumerated, not opened)
    DeviceAvailable { device_name: String },
    /// USB device physically removed
    DeviceUnavailable,
    /// Device state changed (mode button / YOLO switch)
    DeviceStateChanged { mode: DeviceMode, yolo: bool },
    /// Single key event from device
    HidKeyEvent { keycode: u16 },
    /// Type string from device
    HidTypeString { text: String, send_enter: bool },
}

/// Sender for daemon events — wraps a tokio unbounded channel.
/// Unlike the app's EventSender, no winit proxy is needed.
#[derive(Clone)]
pub struct DaemonEventSender {
    tx: mpsc::UnboundedSender<DaemonEvent>,
}

impl DaemonEventSender {
    pub fn new(tx: mpsc::UnboundedSender<DaemonEvent>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: DaemonEvent) -> Result<(), mpsc::error::SendError<DaemonEvent>> {
        self.tx.send(event)
    }
}

/// Current device status (shared across daemon subsystems)
#[derive(Debug, Clone, Default)]
pub struct DeviceStatus {
    /// Device physically present (USB enumerated)
    pub available: bool,
    /// HID interface open and communicating
    pub connected: bool,
    pub device_name: Option<String>,
    pub firmware_version: Option<String>,
    pub mode: DeviceMode,
    pub yolo: bool,
    /// True once we've seen at least one StateReport from the device.
    /// Gates the mode-change → Shift+Tab injection so the firmware's
    /// initial state-on-connect packet doesn't get treated as a tap.
    pub mode_initialized: bool,
}

/// Per-Claude-session state derived from hook + statusline events.
/// Keyed by `session_id` in `ClaudeState::sessions`.
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    pub session_id: String,
    pub current_tool: Option<String>,
    pub current_task: Option<String>,
    pub permission_mode: Option<String>,
    /// True between UserPromptSubmit and Stop — Claude is doing something.
    pub active: bool,
    pub context_window_percent: Option<f64>,
    pub cost_usd: Option<f64>,
    pub model: Option<String>,
    pub session_name: Option<String>,
    /// Reasoning effort level from statusline ("low", "medium", "high",
    /// "xhigh", "max"). Used to enrich the "Thinking…" placeholder.
    pub effort_level: Option<String>,
    /// Whether extended thinking is enabled for the session.
    pub thinking_enabled: bool,
    /// Last hook arrival time — used as recency hint for "active" selection.
    pub last_update_unix: u64,
    /// Unix timestamp when the current phase started (UserPromptSubmit,
    /// PreToolUse, PostToolUse re-stamp; Stop clears). The daemon's
    /// 1Hz ticker re-emits the tab list so the device shows live seconds.
    pub phase_started_at_unix: Option<u64>,
    /// Most recent tool's task summary (e.g. "Bash: npm test"). Set on
    /// every PreToolUse, kept across the following Thinking phase, and
    /// cleared on UserPromptSubmit (next turn) and Stop. Used as `task2`
    /// on the device so the most recent tool stays visible while Claude
    /// thinks about the next step.
    pub last_tool_summary: Option<String>,
    /// Number of tool calls this turn — reset on UserPromptSubmit, ticked
    /// on every PreToolUse. Useful for "Thinking · 3 tools · 14s" displays.
    pub tool_count_this_turn: u32,
    /// Short summary of the user's most recent prompt (truncated). Used as
    /// the session title when `session_name` is unset.
    pub prompt_summary: Option<String>,
    /// Currently in-progress todo from the most recent TodoWrite call.
    /// Cleared on Stop and when no in_progress entry exists.
    pub current_todo: Option<String>,
    /// Tasks created via Claude Code's `TaskCreate` tool that haven't
    /// completed yet — keyed by `task_id`, value is `task_subject`.
    /// Populated by the `TaskCreated` hook, drained by `TaskCompleted`.
    /// Cleared wholesale on `SessionEnd` (the session is dropped).
    pub task_registry: std::collections::HashMap<String, String>,
    /// `task_id` of the most recently created uncompleted task. Drives
    /// the device's `current_task` display so a long-running task's
    /// subject stays visible across the tool calls it makes.
    pub active_task_id: Option<String>,
    /// Subagent rows last reported by Claude Code's `subagentStatusLine`
    /// refresh tick. Each entry is one visible row in the agent panel.
    /// Empty when no subagents are running. Cleared on `SessionEnd`;
    /// otherwise replaced wholesale on every tick (Claude Code sends
    /// the complete visible list each time).
    pub subagents: Vec<SubagentRow>,
}

/// One row from Claude Code's subagent status panel — populated from
/// the `subagentStatusLine` script tick.
#[derive(Debug, Clone, Default)]
pub struct SubagentRow {
    pub id: String,
    pub name: Option<String>,
    pub status: Option<String>,
    pub description: Option<String>,
    /// The display label Claude Code would have rendered by default.
    /// Usually the most informative single-string summary of the row.
    pub label: Option<String>,
    pub start_time_unix: Option<u64>,
    pub token_count: Option<u64>,
}

/// State derived from Claude Code hooks and statusline.
///
/// The daemon stores a `SessionState` per `session_id` (multiple Claude
/// instances can run concurrently). `active_session_id` tracks which one
/// is currently "focused" for HID display purposes.
#[derive(Debug, Clone, Default)]
pub struct ClaudeState {
    pub sessions: std::collections::HashMap<String, SessionState>,
    pub active_session_id: Option<String>,
    /// Pending permission request details, keyed by Claude session_id.
    /// Stored on PermissionRequest, consumed on Notification(permission_prompt).
    pub pending_permissions: std::collections::HashMap<String, PendingPermission>,
}

impl ClaudeState {
    /// Get-or-insert a session entry and bump its recency timestamp.
    /// Does NOT change `active_session_id` — focus, not hook activity,
    /// owns "which session the device shows". Use `set_active_session`
    /// for explicit promotion (OSC 1004 focus-in, tab-click in tray,
    /// first wrapper register).
    pub fn touch_session(&mut self, session_id: &str) -> &mut SessionState {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = self.sessions.entry(session_id.to_string()).or_insert_with(|| {
            SessionState {
                session_id: session_id.to_string(),
                ..SessionState::default()
            }
        });
        entry.last_update_unix = now;
        entry
    }

    /// Promote `session_id` to active. Caller decides when this is
    /// warranted (focus event, manual selection).
    pub fn set_active_session(&mut self, session_id: &str) {
        self.active_session_id = Some(session_id.to_string());
    }
}

/// Details from a PermissionRequest hook, waiting for the corresponding Notification.
#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub tool_name: Option<String>,
    pub tool_input: Option<serde_json::Value>,
}

/// One active `coredeck-claude` wrapper. Created on `/wrapper-ws` connect,
/// removed on disconnect. The `session_id` is filled in by the SessionStart
/// command hook calling `POST /wrapper/register`.
#[derive(Debug, Clone)]
pub struct Wrapper {
    pub wrapper_id: String,
    pub pid: u32,
    pub cwd: String,
    pub started_at_unix: u64,
    /// Set once the SessionStart hook correlates the Claude session to this wrapper.
    pub session_id: Option<String>,
    /// Host terminal context captured at wrapper startup. Used to dispatch
    /// "raise terminal" requests (F20) to the right adapter. `None` if the
    /// wrapper is older than the host-terminal protocol field.
    pub host_terminal: Option<HostTerminal>,
    /// Last OSC 9 title hint sniffed from the child claude's PTY output.
    /// Sits between `session_name` (statusline) and `cwd` in the device's
    /// label fallback chain — informative when claude or a tool sets it,
    /// silent (None) the rest of the time.
    pub terminal_title: Option<String>,
    /// True when the wrapper is in `--ssh` mode (claude lives on a remote
    /// box, hooks come back through an SSH reverse tunnel). Used by tray
    /// + device label code to prefix the title with `↗`.
    pub is_remote: bool,
    /// Channel for sending DaemonToWrapper messages back over the wrapper's WS.
    pub tx: mpsc::UnboundedSender<DaemonToWrapper>,
}

/// Tray updates sent from async code to the main thread
#[derive(Debug, Clone)]
pub enum TrayUpdate {
    /// HID interface opened and communicating
    DeviceConnected { name: String, firmware: String },
    /// HID interface closed or lost
    DeviceDisconnected,
    /// USB device physically available (plugged in, not opened)
    DeviceAvailable(String),
    /// USB device physically removed
    DeviceUnavailable,
    /// Wrapper tab list snapshot for the tray menu to render.
    Tabs(WrapperTabList),
    /// Whether Claude Code hooks are installed in ~/.claude/settings.json.
    /// Drives the "Install Claude Code hooks…" tray menu item.
    HooksInstalled(bool),
}
