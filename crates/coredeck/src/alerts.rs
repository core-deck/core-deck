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
        /// True when this prompt asked the user to enroll the wrapper
        /// in Auto-approve (text "Auto-approve this tab?" rather than
        /// per-tool "Allow Bash?"). On resolution we apply the decision
        /// to the wrapper's enrollment state *before* draining the
        /// queue, so sibling PRs from the same wrapper auto-resolve
        /// without re-prompting.
        is_enrollment: bool,
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

/// A Pending permission alert waiting in the queue because another
/// alert was already showing when its hook fired. Holds everything we
/// need to install it later, plus the same `oneshot::Sender` the live
/// path uses — so the hook handler awaits its decision the same way
/// regardless of whether the alert was instant or queued.
pub struct QueuedPending {
    pub session_id: String,
    pub tool_name: Option<String>,
    pub label: String,
    pub text: String,
    pub details: Option<String>,
    pub tx: oneshot::Sender<DecisionOutcome>,
    /// See `AlertState::Pending::is_enrollment`. Carried through the
    /// queue so a queued enrollment prompt that sits behind a live one
    /// still applies enrollment semantics when it pops to the front.
    pub is_enrollment: bool,
}

impl std::fmt::Debug for QueuedPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedPending")
            .field("session_id", &self.session_id)
            .field("tool_name", &self.tool_name)
            .field("label", &self.label)
            .field("text", &self.text)
            .field("details", &self.details)
            .finish()
    }
}

/// Cap on the queue depth so a stuck user can't pin unbounded memory.
/// Hits in practice would mean four parallel sessions all waiting on
/// permission prompts at once — anything beyond that falls back to
/// Claude's terminal prompt.
const PENDING_QUEUE_CAP: usize = 4;

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
/// should await.
///
/// Behaviour when another alert is already live:
///   - Queue the request (up to `PENDING_QUEUE_CAP` deep). When the
///     active alert resolves the queue head pops to the front. The
///     hook handler awaits exactly the same way as for an immediate
///     install — the difference is invisible past this function.
///   - If the queue is full or the device is offline, return `None`
///     so the caller falls back to Claude's terminal prompt.
pub async fn install_pending_alert(
    state: &DaemonState,
    session_id: String,
    tool_name: Option<String>,
    session_label: &str,
    text: &str,
    details: Option<&str>,
    is_enrollment: bool,
) -> Option<oneshot::Receiver<DecisionOutcome>> {
    let busy = {
        let guard = state.alert_state.lock().await;
        guard.is_some()
    };

    if busy {
        let mut queue = state.pending_queue.lock().await;
        if queue.len() >= PENDING_QUEUE_CAP {
            debug!("Permission alert dropped; queue full");
            return None;
        }
        let (tx, rx) = oneshot::channel();
        queue.push_back(QueuedPending {
            session_id: session_id.clone(),
            tool_name: tool_name.clone(),
            label: session_label.to_string(),
            text: text.to_string(),
            details: details.map(|s| s.to_string()),
            tx,
            is_enrollment,
        });
        debug!(
            session = %session_id,
            depth = queue.len(),
            "permission alert queued behind a live alert",
        );
        return Some(rx);
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
        is_enrollment,
    };
    Some(rx)
}

/// Pop the next queued Pending alert and install it on the device.
/// Called whenever the active alert clears. If the device is offline
/// or the send fails, the queued tx is dropped (which wakes the hook
/// handler with `Err` and falls it back to Claude's terminal prompt),
/// and we move on to the next entry.
///
/// Before installing, re-checks the wrapper's enrollment state — if
/// the live alert that just resolved enrolled the wrapper, sibling
/// queued PRs auto-resolve as Allow without re-prompting; if it
/// declined, queued entries from the same wrapper drop their tx
/// (parked hook handlers fall back to Claude's terminal prompt). This
/// is the layer that keeps a single Allow tap from turning into N
/// taps when claude fires N parallel tool calls.
pub async fn try_install_next_pending(state: &DaemonState) {
    loop {
        let q = {
            let mut queue = state.pending_queue.lock().await;
            queue.pop_front()
        };
        let Some(q) = q else {
            return;
        };

        // Skip past queued entries the user has already implicitly
        // answered via the active alert's enrollment decision.
        let wrapper_id = crate::wrapper::wrapper_id_for_session(state, &q.session_id).await;
        if let Some(ref wid) = wrapper_id {
            if crate::wrapper::is_yolo_opted_in(state, wid).await {
                debug!(
                    session = %q.session_id,
                    "queued PR auto-allowed: wrapper now enrolled in Auto-approve",
                );
                let _ = q.tx.send(DecisionOutcome::Allow);
                continue;
            }
            if crate::wrapper::is_yolo_opted_out(state, wid).await {
                debug!(
                    session = %q.session_id,
                    "queued PR dropped: wrapper opted out of Auto-approve",
                );
                // Drop tx → hook handler sees Err → falls back to
                // Claude's terminal prompt for this PR.
                drop(q.tx);
                continue;
            }
        }

        let tab_index = crate::wrapper::tab_index_for_session(state, &q.session_id)
            .await
            .unwrap_or(0);

        let installed = {
            let hid = state.hid.lock().await;
            if !hid.is_connected() {
                debug!("queued permission alert dropped: device not connected");
                false
            } else if let Err(e) = hid.send_alert(tab_index, &q.label, &q.text, q.details.as_deref())
            {
                warn!(error = %e, "send_alert failed for queued permission prompt");
                false
            } else {
                true
            }
        };

        if !installed {
            // q.tx drops here, rx errs, hook handler falls back. Try next.
            continue;
        }

        let mut guard = state.alert_state.lock().await;
        if guard.is_some() {
            // Race: a fresh install landed while we worked. Drop ours;
            // the hook handler falls back. Don't keep popping in case
            // we'd evict the live one.
            debug!("queued install raced with a fresh install; dropped");
            return;
        }
        *guard = AlertState::Pending {
            session_id: q.session_id,
            tool_name: q.tool_name,
            tx: q.tx,
            tab_index,
            label: q.label,
            text: q.text,
            details: q.details,
            is_enrollment: q.is_enrollment,
        };
        debug!("queued permission alert promoted to active");
        return;
    }
}

/// Drop every queued entry whose `session_id` matches. Their `tx`
/// senders go away with the entries, waking the parked hook handlers
/// with `Err` so they fall back. Used when a session has progressed
/// past the permission point on its own (UserPromptSubmit) or its
/// wrapper has gone away.
pub async fn drop_queued_for_session(state: &DaemonState, session_id: &str) {
    let mut queue = state.pending_queue.lock().await;
    let before = queue.len();
    queue.retain(|q| q.session_id != session_id);
    let dropped = before - queue.len();
    if dropped > 0 {
        debug!(session = %session_id, dropped, "queued permission alerts dropped");
    }
}

/// Clear any currently-showing alert (sends `clear_alert` to the device
/// and resets the state). If a Pending alert is replaced, its oneshot
/// is dropped, which causes the awaiting hook handler to fall back.
/// Pops the queued Pending head into the freshly empty slot.
pub async fn clear_alert(state: &DaemonState) {
    let cleared = {
        let mut guard = state.alert_state.lock().await;
        if !guard.is_some() {
            None
        } else {
            let tab_index = guard.tab_index();
            *guard = AlertState::None;
            Some(tab_index)
        }
    };

    let Some(tab_index) = cleared else {
        return;
    };

    {
        let hid = state.hid.lock().await;
        if let Err(e) = hid.clear_alert(tab_index) {
            debug!(error = %e, "clear_alert failed");
        }
    }
    try_install_next_pending(state).await;
}

/// Cancel any alert (Idle or Pending) for `session_id`. Use on
/// `UserPromptSubmit` — the user has just provided input, so even an
/// idle alert is no longer accurate.
pub async fn cancel_for_session_progress(state: &DaemonState, session_id: &str) {
    let was_active = {
        let mut guard = state.alert_state.lock().await;
        if guard.session_id() == Some(session_id) {
            let tab_index = guard.tab_index();
            // Replacing the variant drops the oneshot::Sender if it was
            // Pending, which wakes the parked hook handler with Err and
            // lets it fall back to Claude's terminal prompt.
            *guard = AlertState::None;
            Some(tab_index)
        } else {
            None
        }
    };

    if let Some(tab_index) = was_active {
        let hid = state.hid.lock().await;
        if let Err(e) = hid.clear_alert(tab_index) {
            debug!(error = %e, "clear_alert (progress) failed");
        }
        debug!(session = %session_id, "alert cancelled by session progress");
    }

    // The session has moved past its permission point; any queued
    // Pending requests for the same session are stale too.
    drop_queued_for_session(state, session_id).await;

    if was_active.is_some() {
        // Slot opened up — show whatever is queued for *other* sessions.
        try_install_next_pending(state).await;
    }
}

/// Cancel only an *Idle* alert for `session_id`. Use on focus-in
/// (the user has just looked at the right window, so an "idle prompt"
/// or "AskUserQuestion" notice has done its job) — but a Pending
/// permission alert must persist until the user actually answers,
/// since looking at the window isn't a decision.
pub async fn cancel_idle_for_session(state: &DaemonState, session_id: &str) {
    let cleared = {
        let mut guard = state.alert_state.lock().await;
        let should_clear = matches!(
            &*guard,
            AlertState::Idle { session_id: sid, .. } if sid == session_id,
        );
        if !should_clear {
            None
        } else {
            let tab_index = guard.tab_index();
            *guard = AlertState::None;
            Some(tab_index)
        }
    };

    let Some(tab_index) = cleared else {
        return;
    };

    {
        let hid = state.hid.lock().await;
        if let Err(e) = hid.clear_alert(tab_index) {
            debug!(error = %e, "clear_alert (idle) failed");
        }
    }
    debug!(session = %session_id, "idle alert cancelled by focus-in");
    // Slot opened — any queued Pending alert from another session
    // should now show.
    try_install_next_pending(state).await;
}

/// Cancel a *Pending* permission alert for `session_id`. Use on
/// coarse session-progress hooks (Stop, SessionStart/End, PreCompact)
/// where the user has clearly moved past the permission point.
///
/// Tool-scoped progress hooks (PreToolUse, PostToolUse) must call
/// `cancel_pending_for_tool` instead — they fire per-tool and would
/// otherwise evict an active alert when a *sibling* parallel tool
/// call advances. See the function for the parallel-tools rationale.
///
/// Idle alerts must persist through all of these — they only become
/// stale when the user actually submits a new prompt.
pub async fn cancel_pending_for_session(state: &DaemonState, session_id: &str) {
    cancel_pending_inner(state, session_id, /* require_tool_match */ None).await;
}

/// Variant of `cancel_pending_for_session` that only cancels when the
/// hook's `tool_name` matches the active Pending's `tool_name`.
///
/// Why: Claude Code fires PreToolUse and PostToolUse per-tool, and for
/// parallel tool calls it interleaves them with the matching
/// PermissionRequests — e.g. `PreA → PR_A → PreB → PR_B`. An
/// unconditional cancel would evict alert A when PreB fires (same
/// session, different tool), forcing the user back to claude's
/// terminal prompt for A even though our Pending is still valid.
///
/// `tool` of `None` is treated as "no information" — never cancels.
/// Callers that genuinely want session-wide cancel should use
/// `cancel_pending_for_session` instead.
pub async fn cancel_pending_for_tool(
    state: &DaemonState,
    session_id: &str,
    tool: Option<&str>,
) {
    let Some(tool) = tool else {
        return;
    };
    cancel_pending_inner(state, session_id, Some(tool)).await;
}

async fn cancel_pending_inner(
    state: &DaemonState,
    session_id: &str,
    require_tool_match: Option<&str>,
) {
    let cleared = {
        let mut guard = state.alert_state.lock().await;
        let should_clear = match &*guard {
            AlertState::Pending {
                session_id: sid,
                tool_name,
                ..
            } if sid == session_id => match require_tool_match {
                None => true,
                Some(want) => tool_name.as_deref() == Some(want),
            },
            _ => false,
        };
        if !should_clear {
            None
        } else {
            let tab_index = guard.tab_index();
            *guard = AlertState::None;
            Some(tab_index)
        }
    };

    if let Some(tab_index) = cleared {
        let hid = state.hid.lock().await;
        if let Err(e) = hid.clear_alert(tab_index) {
            debug!(error = %e, "clear_alert (pending) failed");
        }
        debug!(session = %session_id, "pending alert cancelled by session progress");
    }

    // Coarse-progress callers (require_tool_match=None) also drop
    // queued PRs for this session — they're stale. Tool-scoped callers
    // leave the queue alone; sibling tool calls aren't a stale signal.
    if require_tool_match.is_none() {
        drop_queued_for_session(state, session_id).await;
    }

    if cleared.is_some() {
        try_install_next_pending(state).await;
    }
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
            // Slot is free — promote any queued Pending alert.
            try_install_next_pending(state).await;
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
            session_id: alert_sid,
            tool_name,
            tx,
            tab_index,
            is_enrollment,
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
            // For enrollment prompts, apply the decision to the
            // wrapper's opt-in/opt-out state *before* waking the
            // parked hook handler — so the queue drain that runs
            // synchronously below picks up the new state and
            // auto-resolves any sibling parallel-tool PRs instead of
            // re-prompting the user once per parallel call. The hook
            // handler still re-marks idempotently on its own resume.
            if is_enrollment {
                let still_yolo = state.device_status.read().await.yolo;
                if still_yolo {
                    if let Some(wid) =
                        crate::wrapper::wrapper_id_for_session(state, &alert_sid).await
                    {
                        match outcome {
                            DecisionOutcome::Allow => {
                                crate::wrapper::mark_yolo_opt_in(state, &wid).await;
                            }
                            DecisionOutcome::Deny => {
                                crate::wrapper::mark_yolo_opt_out(state, &wid).await;
                            }
                        }
                    }
                }
            }
            let _ = tx.send(outcome);
            (true, tab_index)
        }
    };

    {
        let hid = state.hid.lock().await;
        if let Err(e) = hid.clear_alert(tab_index) {
            debug!(error = %e, "clear_alert after decision failed");
        }
    }
    // Slot is free — promote a queued Pending alert from any other
    // session into the active position so the user sees it next.
    try_install_next_pending(state).await;

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
    // Drop any queued Pending alerts whose session no longer has a
    // wrapper backing it — their oneshots wake the parked hook
    // handlers with `Err`, causing them to fall back to Claude's
    // terminal prompt instead of being silently lost in the queue.
    {
        let mut queue = state.pending_queue.lock().await;
        let before = queue.len();
        queue.retain(|q| {
            snapshot
                .tabs
                .iter()
                .any(|t| t.session_id.as_deref() == Some(q.session_id.as_str()))
        });
        let dropped = before - queue.len();
        if dropped > 0 {
            debug!(dropped, "queued permission alerts dropped — wrapper(s) gone");
        }
    }

    // Snapshot what we need under the lock.
    let snapshot_session = {
        let guard = state.alert_state.lock().await;
        let Some(sid) = guard.session_id() else {
            // No active alert — but a queued one might now be installable
            // (if a wrapper just appeared while we had nothing showing).
            drop(guard);
            try_install_next_pending(state).await;
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
            drop(hid);
            debug!(session = %snapshot_session, "alert cancelled — wrapper for session is gone");
            // Slot opened up; promote a queued Pending if any.
            try_install_next_pending(state).await;
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
