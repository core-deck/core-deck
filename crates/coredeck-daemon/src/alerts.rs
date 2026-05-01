//! Device alert lifecycle and interactive `PermissionRequest` decisions.
//!
//! Two flavours of alert can be on the device at any time:
//!
//! - **Idle**: shown when Claude raises a `Notification(idle_prompt)`.
//!   The next HID input from the device clears the alert and is then
//!   forwarded to the wrapper as normal.
//! - **Pending**: shown when a `PermissionRequest` hook fires and YOLO is
//!   off. The HTTP response is parked on a `oneshot`; the next HID input
//!   resolves it (Esc / Ctrl-C → deny, anything else → allow), the alert
//!   is cleared, and the input is **not** forwarded to the wrapper.
//!
//! Only one alert can be live at a time; later requests fall back to
//! Claude's own terminal prompt.
//!
//! Alert tab index is currently always 0 — multi-tab alerting can come
//! later when we have richer device-side affordances.
//!
//! Why this lives in its own module: both the hook handlers (which install
//! alerts) and the HID event handler (which consumes them) reach for the
//! same state machine. Keeping the transitions in one place is cheaper
//! than scattering them across two large files.

use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::DaemonState;
use crate::keymap::{KEYCODE_F20, KEYCODE_KNOB_NEXT, KEYCODE_KNOB_PREV};
use crate::state::DaemonEvent;

/// Outcome of an interactive permission decision from the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionOutcome {
    Allow,
    Deny,
}

/// What's currently displayed on the device's alert overlay.
///
/// Rendering content (`label`, `text`, `details`) is kept on the variant
/// so we can re-render the alert on a different tab if the tab list
/// reorders mid-alert.
#[derive(Debug, Default)]
pub enum AlertState {
    /// No alert showing.
    #[default]
    None,
    /// Notification-style alert (Claude is waiting for input). Cleared on
    /// the next HID input, which is then forwarded normally. Keyed by
    /// session so a hook from a different session can't clobber it.
    Idle {
        session_id: String,
        tab_index: usize,
        label: String,
        text: String,
    },
    /// Interactive permission prompt. Holds the oneshot the hook handler
    /// is awaiting; the next HID input resolves it.
    Pending {
        session_id: String,
        tool_name: Option<String>,
        tx: oneshot::Sender<DecisionOutcome>,
        tab_index: usize,
        label: String,
        text: String,
        details: Option<String>,
    },
}

impl AlertState {
    pub fn is_some(&self) -> bool {
        !matches!(self, AlertState::None)
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, AlertState::Idle { .. })
    }

    /// Session id the current alert belongs to, if any.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            AlertState::None => None,
            AlertState::Idle { session_id, .. } => Some(session_id.as_str()),
            AlertState::Pending { session_id, .. } => Some(session_id.as_str()),
        }
    }

    fn tab_index(&self) -> usize {
        match self {
            AlertState::None => 0,
            AlertState::Idle { tab_index, .. } => *tab_index,
            AlertState::Pending { tab_index, .. } => *tab_index,
        }
    }
}

/// What `consume_input_for_decision` decided about an HID input event.
#[derive(Debug, Clone)]
pub enum AlertOutcome {
    /// No alert active (or only an Idle alert was cleared) — caller
    /// should still route the input to the active wrapper.
    Passthrough,
    /// Alert consumed the input. Caller MUST NOT route further.
    Consumed,
    /// User wants to focus the alerting session (F20 while an alert is
    /// up). Caller should switch the active wrapper to this session,
    /// raise its host terminal, leave the alert up, and not route the
    /// input.
    FocusSession(String),
    /// F20 with no alert showing — caller should raise the host terminal
    /// of the currently-active session and not route the input. Active
    /// session selection is unchanged.
    RaiseActive,
}

// ── Installing alerts ──────────────────────────────────────────────

/// Show an Idle alert (Claude waiting for input). No-op if another alert
/// is already live — we don't want a slow idle prompt to clobber a
/// PermissionRequest that the user is in the middle of answering.
pub async fn show_idle_alert(
    state: &DaemonState,
    session_id: &str,
    session_label: &str,
    text: &str,
) {
    {
        let guard = state.alert_state.lock().await;
        if guard.is_some() {
            debug!("Idle alert suppressed; another alert is already live");
            return;
        }
    }

    let tab_index = crate::wrapper::tab_index_for_session(state, session_id)
        .await
        .unwrap_or(0);

    {
        let hid = state.hid.lock().await;
        if !hid.is_connected() {
            debug!("Idle alert suppressed; device not connected");
            return;
        }
        if let Err(e) = hid.send_alert(tab_index, session_label, text, None) {
            warn!(error = %e, "send_alert failed for idle prompt");
            return;
        }
    }

    let mut guard = state.alert_state.lock().await;
    *guard = AlertState::Idle {
        session_id: session_id.to_string(),
        tab_index,
        label: session_label.to_string(),
        text: text.to_string(),
    };
}

/// Try to install a Pending alert and return the receiver the caller
/// should await. Returns `None` if another alert is already live or the
/// device isn't connected — in those cases the caller should fall back
/// to Claude's own prompt.
pub async fn install_pending_alert(
    state: &DaemonState,
    session_id: String,
    tool_name: Option<String>,
    session_label: &str,
    text: &str,
    details: Option<&str>,
) -> Option<oneshot::Receiver<DecisionOutcome>> {
    {
        let guard = state.alert_state.lock().await;
        if guard.is_some() {
            debug!("Permission alert suppressed; another alert is already live");
            return None;
        }
    }

    let tab_index = crate::wrapper::tab_index_for_session(state, &session_id)
        .await
        .unwrap_or(0);

    {
        let hid = state.hid.lock().await;
        if !hid.is_connected() {
            debug!("Permission alert suppressed; device not connected");
            return None;
        }
        if let Err(e) = hid.send_alert(tab_index, session_label, text, details) {
            warn!(error = %e, "send_alert failed for permission prompt");
            return None;
        }
    }

    let (tx, rx) = oneshot::channel();
    let mut guard = state.alert_state.lock().await;
    *guard = AlertState::Pending {
        session_id,
        tool_name,
        tx,
        tab_index,
        label: session_label.to_string(),
        text: text.to_string(),
        details: details.map(|s| s.to_string()),
    };
    Some(rx)
}

/// Clear any currently-showing alert (sends `clear_alert` to the device
/// and resets the state). If a Pending alert is replaced, its oneshot
/// is dropped, which causes the awaiting hook handler to fall back.
pub async fn clear_alert(state: &DaemonState) {
    let mut guard = state.alert_state.lock().await;
    if !guard.is_some() {
        return;
    }
    let tab_index = guard.tab_index();
    *guard = AlertState::None;
    drop(guard);

    let hid = state.hid.lock().await;
    if let Err(e) = hid.clear_alert(tab_index) {
        debug!(error = %e, "clear_alert failed");
    }
}

/// Cancel any alert (Idle or Pending) for `session_id`. Use on
/// `UserPromptSubmit` — the user has just provided input, so even an
/// idle alert is no longer accurate.
pub async fn cancel_for_session_progress(state: &DaemonState, session_id: &str) {
    let mut guard = state.alert_state.lock().await;
    let should_clear = guard.session_id() == Some(session_id);
    if !should_clear {
        return;
    }
    let tab_index = guard.tab_index();
    // Replacing the variant drops the oneshot::Sender if it was Pending,
    // which wakes the parked hook handler with Err and lets it fall back.
    *guard = AlertState::None;
    drop(guard);

    let hid = state.hid.lock().await;
    if let Err(e) = hid.clear_alert(tab_index) {
        debug!(error = %e, "clear_alert (progress) failed");
    }
    debug!(session = %session_id, "alert cancelled by session progress");
}

/// Cancel only an *Idle* alert for `session_id`. Use on focus-in
/// (the user has just looked at the right window, so an "idle prompt"
/// or "AskUserQuestion" notice has done its job) — but a Pending
/// permission alert must persist until the user actually answers,
/// since looking at the window isn't a decision.
pub async fn cancel_idle_for_session(state: &DaemonState, session_id: &str) {
    let mut guard = state.alert_state.lock().await;
    let should_clear = matches!(
        &*guard,
        AlertState::Idle { session_id: sid, .. } if sid == session_id,
    );
    if !should_clear {
        return;
    }
    let tab_index = guard.tab_index();
    *guard = AlertState::None;
    drop(guard);

    let hid = state.hid.lock().await;
    if let Err(e) = hid.clear_alert(tab_index) {
        debug!(error = %e, "clear_alert (idle) failed");
    }
    debug!(session = %session_id, "idle alert cancelled by focus-in");
}

/// Cancel only a *Pending* permission alert for `session_id`. Use on
/// non-`UserPromptSubmit` activity hooks (PreToolUse, PostToolUse, Stop,
/// Notification): if Claude is moving past the permission point, the
/// pending prompt is stale (user probably answered in their terminal).
/// Idle alerts must persist through all of these — they only become
/// stale when the user actually submits a new prompt.
pub async fn cancel_pending_for_session(state: &DaemonState, session_id: &str) {
    let mut guard = state.alert_state.lock().await;
    let should_clear = matches!(
        &*guard,
        AlertState::Pending { session_id: sid, .. } if sid == session_id,
    );
    if !should_clear {
        return;
    }
    let tab_index = guard.tab_index();
    *guard = AlertState::None;
    drop(guard);

    let hid = state.hid.lock().await;
    if let Err(e) = hid.clear_alert(tab_index) {
        debug!(error = %e, "clear_alert (pending) failed");
    }
    debug!(session = %session_id, "pending alert cancelled by session progress");
}

// ── Consuming HID input ─────────────────────────────────────────────

/// Decide what should happen to an HID input event in the presence of an
/// active alert.
///
/// - No alert + F20 → `RaiseActive`; otherwise `Passthrough`.
/// - F20 + alert → `FocusSession(session_id)`. For Idle alerts the
///   alert is cleared inline (the user has attended); for Pending it
///   stays up — focus switch isn't a permission decision.
/// - Idle alert + Esc → clear alert, return `Passthrough`.
/// - Idle alert + anything else → leave the alert up, return
///   `Passthrough`. Notification dismissal is reserved for F20 / Esc;
///   stray knob rotation, soft-key strings, or random keys mustn't
///   make the alert vanish silently.
/// - Pending alert + Enter / `y` / `Y` → resolve Allow.
/// - Pending alert + `n` / `N` / Ctrl-C / Esc → resolve Deny.
/// - Pending alert + anything else → leave the alert up, return
///   `Passthrough`. The user must answer with one of the explicit
///   decision keys.
pub async fn consume_input_for_decision(
    state: &DaemonState,
    event: &DaemonEvent,
) -> AlertOutcome {
    let kind = classify_input(event);
    if matches!(kind, InputKind::None) {
        return AlertOutcome::Passthrough;
    }

    // Snapshot under the lock; bail fast when nothing is showing.
    let (alert_session_id, alert_is_idle) = {
        let guard = state.alert_state.lock().await;
        if !guard.is_some() {
            // No alert. F20 still has meaning — it raises the active
            // session's host terminal. Everything else falls through to
            // the wrapper PTY.
            return if matches!(kind, InputKind::Focus) {
                AlertOutcome::RaiseActive
            } else {
                AlertOutcome::Passthrough
            };
        }
        (guard.session_id().map(String::from), guard.is_idle())
    };

    // F20 with alert. Two cases:
    //   - Idle alert: the user has explicitly attended to the prompt by
    //     pressing the Claude button; clear the alert ourselves rather
    //     than waiting on a focus-in echo from the host terminal (some
    //     terminals don't emit OSC 1004 reliably on programmatic raise).
    //   - Pending alert: stays up — switching focus isn't a permission
    //     decision. Allow/Deny still requires explicit input.
    // In both cases the caller switches active to the alerting session
    // and raises that terminal.
    if matches!(kind, InputKind::Focus) {
        if alert_is_idle {
            let mut guard = state.alert_state.lock().await;
            let prev = std::mem::take(&mut *guard);
            drop(guard);
            if let AlertState::Idle { tab_index, .. } = prev {
                let hid = state.hid.lock().await;
                if let Err(e) = hid.clear_alert(tab_index) {
                    debug!(error = %e, "clear_alert (idle, F20) failed");
                }
            }
        }
        return match alert_session_id {
            Some(sid) => AlertOutcome::FocusSession(sid),
            None => AlertOutcome::Consumed,
        };
    }

    // Idle alerts only respond to Dismiss (Esc). Allow/Deny gestures are
    // for Pending prompts and have no meaning here — leave the alert up
    // and let the input flow through to the wrapper as if there were no
    // alert. Without this, rotating the knob (Up/Down arrows on Layer 0)
    // or hitting a soft-key during an idle prompt would clear the alert
    // before the user even saw it.
    if alert_is_idle && !matches!(kind, InputKind::Dismiss) {
        return AlertOutcome::Passthrough;
    }

    // Pending alerts only respond to Allow / Deny / Dismiss; everything
    // else passes through (including arbitrary keys and soft-key text).
    if !alert_is_idle
        && !matches!(
            kind,
            InputKind::Allow | InputKind::Deny | InputKind::Dismiss
        )
    {
        return AlertOutcome::Passthrough;
    }

    // The input is meaningful for the current alert; transition.
    let mut guard = state.alert_state.lock().await;
    let prev = std::mem::take(&mut *guard);
    drop(guard);

    let (consumed, tab_index) = match prev {
        AlertState::None => return AlertOutcome::Passthrough,
        AlertState::Idle { tab_index, .. } => (false, tab_index),
        AlertState::Pending {
            tool_name,
            tx,
            tab_index,
            ..
        } => {
            // Dismiss (Esc) on a Pending alert is treated as Deny —
            // the user explicitly bailed out of the prompt.
            let outcome = match kind {
                InputKind::Allow => DecisionOutcome::Allow,
                _ => DecisionOutcome::Deny,
            };
            info!(
                tool = tool_name.as_deref().unwrap_or("?"),
                outcome = ?outcome,
                "permission decision from device",
            );
            let _ = tx.send(outcome);
            (true, tab_index)
        }
    };

    let hid = state.hid.lock().await;
    if let Err(e) = hid.clear_alert(tab_index) {
        debug!(error = %e, "clear_alert after decision failed");
    }

    if consumed {
        AlertOutcome::Consumed
    } else {
        AlertOutcome::Passthrough
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    /// F20 / Claude button — reserved for daemon-level controls.
    Focus,
    /// Enter / `y` / `Y` — explicit "yes" reply. Pending alerts only.
    Allow,
    /// `n` / `N` / Ctrl-C — explicit "no" reply. Pending alerts only.
    Deny,
    /// Esc — dismisses Idle alerts, denies Pending prompts. Distinct
    /// from Deny so Idle can ignore n/N/Ctrl-C and only react to Esc.
    Dismiss,
    /// Anything else — knob rotation, soft-key strings, arbitrary keys.
    /// Alert (if any) stays up and the input passes through to the
    /// active wrapper untouched.
    None,
}

// HID Keyboard keycodes (USB HID Usage Page 0x07). Lower 8 bits of the
// QMK keycode; modifier flags are in the upper bits (LCTL=0x100,
// LSFT=0x200, LALT=0x400).
const HID_ENTER: u8 = 0x28;
const HID_ESC: u8 = 0x29;
const HID_Y: u8 = 0x1C; // y/Y depending on Shift
const HID_N: u8 = 0x11; // n/N depending on Shift
/// Pre-composed `LCTL + c` keycode (matches firmware's Stop binding).
const QMK_CTRL_C: u16 = 0x0106;

fn classify_input(event: &DaemonEvent) -> InputKind {
    match event {
        DaemonEvent::HidKeyEvent { keycode } => {
            if *keycode == KEYCODE_F20 {
                return InputKind::Focus;
            }
            if *keycode == KEYCODE_KNOB_NEXT || *keycode == KEYCODE_KNOB_PREV {
                // Knob press+rotate is a daemon control (cycle wrappers).
                // Falls through to main.rs's cycler.
                return InputKind::None;
            }
            if *keycode == QMK_CTRL_C {
                return InputKind::Deny;
            }
            // Strip modifier bits so y/Y and n/N both map regardless of
            // Shift. Don't accept Ctrl+y / Ctrl+n — Ctrl combos are not
            // decision gestures, only the Ctrl-C abort is.
            let base = (*keycode & 0xFF) as u8;
            let has_ctrl = (*keycode & 0x0100) != 0;
            if !has_ctrl {
                match base {
                    HID_ENTER | HID_Y => return InputKind::Allow,
                    HID_N => return InputKind::Deny,
                    HID_ESC => return InputKind::Dismiss,
                    _ => {}
                }
            }
            InputKind::None
        }
        // Soft-key text strings type into the wrapper; never resolve a
        // permission prompt for the user. They pass through unchanged.
        DaemonEvent::HidTypeString { .. } => InputKind::None,
        _ => InputKind::None,
    }
}

// ── Reacting to tab list changes ───────────────────────────────────

/// Reconcile any active alert with a freshly-built `WrapperTabList`.
///
/// - If the alerting session's wrapper is no longer in the list, the
///   session is gone — cancel the alert (and let the parked hook handler
///   fall back to Claude's own prompt for Pending alerts).
/// - If the alerting session is still around but its tab index moved
///   (because other wrappers came/went), clear the alert at the old tab
///   and re-render it at the new tab so the device highlights the right
///   row.
/// - Otherwise no-op.
///
/// Called from `wrapper::emit_tab_list` on every tab list change.
pub async fn refresh_for_tabs(
    state: &DaemonState,
    snapshot: &coredeck_protocol::WrapperTabList,
) {
    // Snapshot what we need under the lock.
    let snapshot_session = {
        let guard = state.alert_state.lock().await;
        let Some(sid) = guard.session_id() else {
            return;
        };
        sid.to_string()
    };

    let new_index = snapshot
        .tabs
        .iter()
        .position(|t| t.session_id.as_deref() == Some(snapshot_session.as_str()));

    let mut guard = state.alert_state.lock().await;
    let old_index = guard.tab_index();

    match new_index {
        None => {
            // Wrapper for this session is gone — cancel the alert.
            *guard = AlertState::None;
            drop(guard);

            let hid = state.hid.lock().await;
            if let Err(e) = hid.clear_alert(old_index) {
                debug!(error = %e, "clear_alert (wrapper gone) failed");
            }
            debug!(session = %snapshot_session, "alert cancelled — wrapper for session is gone");
        }
        Some(new_idx) if new_idx != old_index => {
            // Tab list reordered — re-render at the new index.
            // We need a copy of the rendering content; mutate in place.
            let (label, text, details) = match &mut *guard {
                AlertState::Idle { tab_index, label, text, .. } => {
                    *tab_index = new_idx;
                    (label.clone(), text.clone(), None)
                }
                AlertState::Pending { tab_index, label, text, details, .. } => {
                    *tab_index = new_idx;
                    (label.clone(), text.clone(), details.clone())
                }
                AlertState::None => return,
            };
            drop(guard);

            let hid = state.hid.lock().await;
            if let Err(e) = hid.clear_alert(old_index) {
                debug!(error = %e, "clear_alert (relocate) failed");
            }
            if let Err(e) = hid.send_alert(new_idx, &label, &text, details.as_deref()) {
                warn!(error = %e, "send_alert (relocate) failed");
            }
            debug!(
                session = %snapshot_session,
                from = old_index,
                to = new_idx,
                "alert relocated to new tab index",
            );
        }
        _ => { /* same tab; nothing to do */ }
    }
}
