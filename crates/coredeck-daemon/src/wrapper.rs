//! Wrapper protocol — `coredeck-claude` ↔ daemon.
//!
//! Each `coredeck-claude` opens a long-lived WebSocket to `/wrapper-ws`,
//! sends a `Register` frame, and stays connected for the lifetime of its
//! child `claude` process. The daemon stores per-wrapper state and can
//! push `Write { bytes }` commands back to the wrapper, which writes the
//! bytes into the child's PTY stdin.
//!
//! Session correlation happens via a separate one-shot HTTP call from a
//! tiny `SessionStart` command hook script: `POST /wrapper/register`
//! with `{wrapper_id, session_id}`. The script reads `wrapper_id` from
//! `$COREDECK_WRAPPER_ID` (set by the wrapper before exec'ing claude)
//! and `session_id` from the hook's stdin payload.
//!
//! Multiple wrappers can be connected at once; there is no exclusivity
//! lock here (unlike `/ws`, which the GUI app holds).

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use coredeck_protocol::{
    encode_ws_frame, DaemonToWrapper, DeviceMode, DisplayUpdate, WrapperRegisterSession,
    WrapperTab, WrapperTabList, WrapperToDaemon, WsEventTag, TAB_STATE_STARTED, TAB_STATE_WORKING,
    TAB_STATE_INACTIVE,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::state::Wrapper;
use crate::DaemonState;

/// `GET /wrapper-ws` — accept a wrapper connection.
pub async fn wrapper_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<DaemonState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_wrapper_ws(socket, state))
}

async fn handle_wrapper_ws(socket: WebSocket, state: Arc<DaemonState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Wait for the first frame, which must be a Register.
    let register = loop {
        match ws_rx.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<WrapperToDaemon>(t.as_str())
            {
                Ok(m) => break m,
                Err(e) => {
                    debug!(error = %e, "wrapper sent unparseable JSON before Register");
                    return;
                }
            },
            Some(Ok(Message::Close(_))) | None => return,
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                warn!(error = %e, "wrapper WS read error before Register");
                return;
            }
        }
    };

    let (wrapper_id, pid, cwd, started_at_unix, host_terminal) = match register {
        WrapperToDaemon::Register {
            wrapper_id,
            pid,
            cwd,
            started_at_unix,
            host_terminal,
        } => (wrapper_id, pid, cwd, started_at_unix, host_terminal),
        WrapperToDaemon::Goodbye { .. } | WrapperToDaemon::FocusChanged { .. } => {
            debug!("wrapper sent non-Register frame first; ignoring");
            return;
        }
    };

    info!(
        wrapper_id = %wrapper_id,
        pid,
        cwd = %cwd,
        host = ?host_terminal.as_ref().map(|h| &h.kind),
        "wrapper connected",
    );

    // Channel that owns daemon→wrapper command flow.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<DaemonToWrapper>();

    // Insert into the registry. If the wrapper is reconnecting (same
    // wrapper_id), keep its prior session_id so a brief WS flap doesn't
    // unbind the Claude session — SessionStart only fires on session
    // creation/resume, never on raw reconnect, so we'd otherwise lose
    // the correlation until the user starts a new session.
    {
        let mut wrappers = state.wrappers.write().await;
        let preserved_session_id = wrappers
            .get(&wrapper_id)
            .and_then(|w| w.session_id.clone());
        if preserved_session_id.is_some() {
            debug!(wrapper_id = %wrapper_id, "wrapper reconnect preserved session_id");
        }
        wrappers.insert(
            wrapper_id.clone(),
            Wrapper {
                wrapper_id: wrapper_id.clone(),
                pid,
                cwd,
                started_at_unix,
                session_id: preserved_session_id,
                host_terminal,
                tx: cmd_tx.clone(),
            },
        );
    }

    // Ack.
    let _ = cmd_tx.send(DaemonToWrapper::Registered {
        wrapper_id: wrapper_id.clone(),
    });

    // Open the HID device if available. The daemon needs it to push
    // display updates and (later) to route HID input to wrappers.
    {
        let hid = state.hid.lock().await;
        if hid.is_device_available() && !hid.is_connected() {
            match hid.open_device() {
                Ok(()) => info!("HID device opened for wrapper"),
                Err(e) => warn!(error = %e, "wrapper-triggered HID open failed"),
            }
        }
    }

    emit_tab_list(&state).await;

    // Writer task — drains the command channel into the WS.
    let writer = tokio::spawn(async move {
        while let Some(msg) = cmd_rx.recv().await {
            let txt = match serde_json::to_string(&msg) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    // Reader loop — Goodbye, focus updates, then disconnect.
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Text(t)) => match serde_json::from_str::<WrapperToDaemon>(t.as_str()) {
                Ok(WrapperToDaemon::Goodbye { exit_code, .. }) => {
                    debug!(wrapper_id = %wrapper_id, ?exit_code, "wrapper said goodbye");
                    break;
                }
                Ok(WrapperToDaemon::FocusChanged { focused, .. }) => {
                    if focused {
                        handle_wrapper_focused(&state, &wrapper_id).await;
                    }
                    // Focus-out is currently informational only.
                }
                Ok(_) => {} // Stray Register frames; ignore.
                Err(e) => debug!(error = %e, "wrapper sent malformed JSON post-register"),
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(e) => {
                debug!(error = %e, "wrapper WS read error");
                break;
            }
        }
    }

    writer.abort();
    {
        let mut wrappers = state.wrappers.write().await;
        wrappers.remove(&wrapper_id);
    }
    emit_tab_list(&state).await;

    info!(wrapper_id = %wrapper_id, "wrapper disconnected");
}

/// `POST /wrapper/register` — bind a Claude `session_id` to a `wrapper_id`.
///
/// Called by the SessionStart command hook script. The script reads
/// `$COREDECK_WRAPPER_ID` from its inherited env (set by the wrapper)
/// and `session_id` from the hook's stdin payload.
pub async fn wrapper_register_session(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<WrapperRegisterSession>,
) -> impl IntoResponse {
    let bound = {
        let mut wrappers = state.wrappers.write().await;
        if let Some(w) = wrappers.get_mut(&req.wrapper_id) {
            w.session_id = Some(req.session_id.clone());
            true
        } else {
            false
        }
    };
    if bound {
        info!(
            wrapper_id = %req.wrapper_id,
            session_id = %req.session_id,
            "wrapper bound to session"
        );
        // Bootstrap active selection only when nothing is active yet —
        // typically the very first wrapper to register. Subsequent
        // wrappers wait for an OSC 1004 focus-in or tray click; we don't
        // want a new wrapper to steal the device while the user is
        // looking at something else.
        let became_active = {
            let mut claude = state.claude_state.write().await;
            claude.touch_session(&req.session_id);
            if claude.active_session_id.is_none() {
                claude.set_active_session(&req.session_id);
                true
            } else {
                false
            }
        };
        emit_tab_list(&state).await;
        if became_active {
            sync_active_mode_to_device(&state).await;
        }
        StatusCode::OK
    } else {
        warn!(wrapper_id = %req.wrapper_id, "register-session for unknown wrapper");
        StatusCode::NOT_FOUND
    }
}

/// Build the current `WrapperTabList` snapshot, push it to the GUI app
/// over `/ws`, project it directly onto the HID device display, and
/// hand it to the tray menu so the daemon's tray reflects live wrapper
/// state without any GUI app at all.
pub async fn emit_tab_list(state: &Arc<DaemonState>) {
    let snapshot = build_tab_list(state).await;

    // Reconcile any active alert with the new tab list — wrappers may
    // have come or gone, shifting the alerting session's tab index (or
    // removing the alerting wrapper entirely).
    crate::alerts::refresh_for_tabs(state, &snapshot).await;

    // Forward to the GUI app (no-op if none connected).
    if let Ok(payload) = serde_json::to_vec(&snapshot) {
        let frame = encode_ws_frame(WsEventTag::WrapperTabList as u8, 0, &payload);
        let guard = state.ws_client.lock().await;
        if let Some(client) = guard.as_ref() {
            let _ = client.tx.send(frame);
        }
    }

    // Mirror to the tray (main thread renders).
    state.send_tray_update(crate::state::TrayUpdate::Tabs(snapshot.clone()));

    // Drive the device display from the wrapper view. Skip when there's
    // nothing to show — let the GUI app keep rendering its idle/embedded
    // logo in that case.
    if !snapshot.tabs.is_empty() {
        push_to_device(state, &snapshot).await;
    }
}

/// Pick the best human-friendly label for a tab, matching the priority
/// the device uses: explicit session_name, then prompt summary, then a
/// short cwd basename.
pub fn tab_label(tab: &WrapperTab) -> String {
    tab.session_name
        .clone()
        .or_else(|| tab.prompt_summary.clone())
        .unwrap_or_else(|| short_cwd_label(&tab.cwd))
}

/// Return the current tab-list index of the wrapper bound to `session_id`,
/// or `None` if no wrapper is bound to that session right now. Iterates
/// the same sort order as `build_tab_list` so the index lines up with
/// what the device sees.
pub async fn tab_index_for_session(state: &DaemonState, session_id: &str) -> Option<usize> {
    let wrappers = state.wrappers.read().await;
    let mut sorted: Vec<&Wrapper> = wrappers.values().collect();
    sorted.sort_by(|a, b| {
        a.started_at_unix
            .cmp(&b.started_at_unix)
            .then_with(|| a.wrapper_id.cmp(&b.wrapper_id))
    });
    sorted
        .iter()
        .position(|w| w.session_id.as_deref() == Some(session_id))
}

async fn push_to_device(state: &Arc<DaemonState>, snapshot: &WrapperTabList) {
    let active_idx = snapshot
        .active_wrapper_id
        .as_ref()
        .and_then(|id| snapshot.tabs.iter().position(|t| &t.wrapper_id == id))
        .unwrap_or(0);

    let active = match snapshot.tabs.get(active_idx) {
        Some(t) => t,
        None => return,
    };

    let session_label = active
        .session_name
        .clone()
        .or_else(|| active.prompt_summary.clone())
        .unwrap_or_else(|| short_cwd_label(&active.cwd));
    // Line 1 priority: subagent label (parent's tools are subordinate while
    // a subagent is running) > the parent's current_task.
    let task = active
        .subagent_label
        .clone()
        .or_else(|| Some(active.current_task.clone().unwrap_or_default()))
        .unwrap_or_default();
    // Line 2 priority: when a subagent is taking line 1, use the parent's
    // own current_task (it'll usually be "Thinking…") on line 2 so the
    // user can still see the parent is alive. Otherwise keep the existing
    // todo > last_tool_summary order.
    let task2 = if active.subagent_label.is_some() {
        active.current_task.clone().unwrap_or_default()
    } else if let Some(todo) = active.current_todo.as_deref() {
        todo.to_string()
    } else if active.current_tool.is_none() {
        active.last_tool_summary.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let tabs_state: Vec<u8> = snapshot.tabs.iter().map(|t| t.tab_state).collect();

    let update = DisplayUpdate {
        session: session_label,
        task,
        task2,
        tabs: tabs_state,
        active: active_idx,
        context_percent: active.context_percent,
        cost_usd: active.cost_usd,
        model: active.model.clone(),
    };

    let hid = state.hid.lock().await;
    if let Err(e) = hid.send_display_update(&update) {
        debug!(error = %e, "daemon-side send_display_update failed");
    }
}

/// Last path segment of a cwd, falling back to the whole string. Empty
/// strings stay empty.
fn short_cwd_label(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(cwd)
        .to_string()
}

/// Pick the most informative subagent row for the device's task line.
/// Prefers the first one whose status looks "in flight" (anything that
/// isn't a known terminal state), so the display tracks live work even
/// when Claude Code keeps completed rows visible briefly. Returns
/// `None` when no subagents are reported.
fn subagent_label(s: &crate::state::SessionState) -> Option<String> {
    if s.subagents.is_empty() {
        return None;
    }
    let row = s
        .subagents
        .iter()
        .find(|r| !is_terminal_status(r.status.as_deref()))
        .or_else(|| s.subagents.first())?;
    let label = row
        .label
        .as_deref()
        .or(row.description.as_deref())
        .or(row.name.as_deref())
        .unwrap_or("Subagent")
        .to_string();
    let total = s.subagents.len();
    if total > 1 {
        Some(format!("({total}) {label}"))
    } else {
        Some(label)
    }
}

fn is_terminal_status(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed") | Some("done") | Some("succeeded") | Some("failed") | Some("cancelled")
    )
}

/// Render the `current_task` for a tab, enriching the generic "Thinking…"
/// placeholder with the session's effort level when the statusline has
/// reported one and appending phase elapsed seconds. Tool tasks
/// (`Bash: …`, `Edit: …`) get the elapsed suffix too.
fn decorate_task(s: &crate::state::SessionState) -> Option<String> {
    let raw = s.current_task.as_ref()?;
    let base = if raw == "Thinking…" {
        match (s.effort_level.as_deref(), s.thinking_enabled) {
            (Some(level), _) => format!("Thinking ({level})…"),
            (None, true) => "Thinking deeply…".to_string(),
            (None, false) => "Thinking…".to_string(),
        }
    } else {
        raw.clone()
    };

    if let Some(start) = s.phase_started_at_unix {
        let elapsed = now_unix().saturating_sub(start);
        Some(format!("{base} · {elapsed}s"))
    } else {
        Some(base)
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Re-emit the tab list once per second while at least one wrapper is
/// connected and at least one of its sessions is active. This keeps the
/// elapsed-time suffix advancing on the device between hook events.
pub async fn run_display_ticker(state: Arc<DaemonState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let should_tick = {
            let wrappers = state.wrappers.read().await;
            let claude = state.claude_state.read().await;
            !wrappers.is_empty() && claude.sessions.values().any(|s| s.active)
        };
        if should_tick {
            emit_tab_list(&state).await;
        }
    }
}

async fn build_tab_list(state: &Arc<DaemonState>) -> WrapperTabList {
    let wrappers = state.wrappers.read().await;
    let claude = state.claude_state.read().await;

    let mut tabs: Vec<WrapperTab> = wrappers
        .values()
        .map(|w| {
            let session = w
                .session_id
                .as_ref()
                .and_then(|sid| claude.sessions.get(sid));
            WrapperTab {
                wrapper_id: w.wrapper_id.clone(),
                session_id: w.session_id.clone(),
                cwd: w.cwd.clone(),
                pid: w.pid,
                started_at_unix: w.started_at_unix,
                session_name: session.and_then(|s| s.session_name.clone()),
                prompt_summary: session.and_then(|s| s.prompt_summary.clone()),
                current_todo: session.and_then(|s| s.current_todo.clone()),
                model: session.and_then(|s| s.model.clone()),
                current_tool: session.and_then(|s| s.current_tool.clone()),
                current_task: session.and_then(decorate_task),
                last_tool_summary: session.and_then(|s| s.last_tool_summary.clone()),
                permission_mode: session.and_then(|s| s.permission_mode.clone()),
                tab_state: match session {
                    None => TAB_STATE_INACTIVE,
                    Some(s) if s.active => TAB_STATE_WORKING,
                    Some(_) => TAB_STATE_STARTED,
                },
                context_percent: session.and_then(|s| s.context_window_percent),
                cost_usd: session.and_then(|s| s.cost_usd),
                subagent_label: session.and_then(subagent_label),
                subagent_count: session.map(|s| s.subagents.len() as u32).unwrap_or(0),
            }
        })
        .collect();

    // Stable ordering: oldest first, ties broken by wrapper_id.
    tabs.sort_by(|a, b| {
        a.started_at_unix
            .cmp(&b.started_at_unix)
            .then_with(|| a.wrapper_id.cmp(&b.wrapper_id))
    });

    // Active wrapper = the one bound to the most-recently-touched session.
    let active_wrapper_id = claude
        .active_session_id
        .as_ref()
        .and_then(|sid| {
            wrappers
                .values()
                .find(|w| w.session_id.as_deref() == Some(sid.as_str()))
                .map(|w| w.wrapper_id.clone())
        })
        .or_else(|| tabs.first().map(|t| t.wrapper_id.clone()));

    WrapperTabList {
        tabs,
        active_wrapper_id,
    }
}

/// Push the active session's `permission_mode` to the device's LED.
/// Reads only state we already have on hand — never waits for a new
/// hook. Safe to call from anywhere; short-circuits when the device is
/// already in the target mode (so it's also safe to call on every
/// active-session change without spamming HID writes).
///
/// Updates `device_status.mode` eagerly *before* the HID write so the
/// echo `StateReport` we'll get back doesn't get mistaken for a fresh
/// mode-button tap and trigger a Shift+Tab injection.
pub async fn sync_active_mode_to_device(state: &DaemonState) {
    let target_mode = {
        let claude = state.claude_state.read().await;
        let pm = claude
            .active_session_id
            .as_deref()
            .and_then(|sid| claude.sessions.get(sid))
            .and_then(|s| s.permission_mode.as_deref())
            .map(map_permission_mode);
        // No active session, or it has no permission_mode yet — leave
        // the device alone. Pushing Default here would fight whatever
        // the user just set with the mode button.
        match pm {
            Some(m) => m,
            None => return,
        }
    };

    let need_write = {
        let mut status = state.device_status.write().await;
        if status.mode == target_mode {
            false
        } else {
            status.mode = target_mode;
            true
        }
    };
    if !need_write {
        return;
    }

    let hid = state.hid.lock().await;
    if !hid.is_connected() {
        return;
    }
    if let Err(e) = hid.set_mode(target_mode) {
        debug!(error = %e, "set_mode failed");
    }
}

/// Map Claude Code's `permission_mode` string to the firmware's three
/// `DeviceMode` slots. `bypassPermissions` collapses to `Default` —
/// the device has no fourth color and "default" is the closest in
/// behaviour ("permissions are off but unlike acceptEdits we're not
/// committing to a specific posture").
fn map_permission_mode(pm: &str) -> DeviceMode {
    match pm {
        "plan" => DeviceMode::Plan,
        "acceptEdits" => DeviceMode::Accept,
        _ => DeviceMode::Default,
    }
}

/// Resolve a `wrapper_id` (empty = active) and queue a `Write { bytes }`.
pub async fn write_to_target(
    state: &Arc<DaemonState>,
    wrapper_id: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    if !wrapper_id.is_empty() {
        return write_to_wrapper(state, wrapper_id, bytes)
            .await
            .then_some(())
            .ok_or_else(|| format!("no wrapper with id {}", wrapper_id));
    }
    // Active wrapper.
    let snapshot = build_tab_list(state).await;
    let Some(target) = snapshot.active_wrapper_id else {
        return Err("no active wrapper".to_string());
    };
    write_to_wrapper(state, &target, bytes)
        .await
        .then_some(())
        .ok_or_else(|| format!("active wrapper {} disappeared", target))
}

/// Handle a focus-in report from a wrapper: make it active, clear any
/// alert tied to its session, and re-emit the tab list.
async fn handle_wrapper_focused(state: &Arc<DaemonState>, wrapper_id: &str) {
    let session_id = {
        let wrappers = state.wrappers.read().await;
        wrappers.get(wrapper_id).and_then(|w| w.session_id.clone())
    };

    // Always promote the wrapper, even if its session_id isn't yet known
    // (early focus-in before SessionStart). The wrapper id alone is
    // enough to set the active row in the tab list.
    if let Err(e) = set_active_wrapper(state, wrapper_id).await {
        debug!(error = %e, "set_active_wrapper from focus-in failed");
        return;
    }

    // If an *Idle* alert for this session is up, the user has just
    // looked at the right window — drop it. A *Pending* permission
    // alert must persist: switching window focus isn't a decision.
    if let Some(sid) = session_id {
        crate::alerts::cancel_idle_for_session(state, &sid).await;
    }
}

/// Mark the wrapper bound to `session_id` as active (if any) and emit a
/// tab list update. Returns `false` when no wrapper currently owns that
/// session.
pub async fn set_active_for_session(state: &Arc<DaemonState>, session_id: &str) -> bool {
    let wrapper_id = {
        let wrappers = state.wrappers.read().await;
        wrappers
            .values()
            .find(|w| w.session_id.as_deref() == Some(session_id))
            .map(|w| w.wrapper_id.clone())
    };
    let Some(wid) = wrapper_id else {
        return false;
    };
    set_active_wrapper(state, &wid).await.is_ok()
}

/// Cycle the active wrapper one step in the same order the device sees
/// (sorted by `started_at_unix`, ties broken by `wrapper_id`). `step`
/// is +1 for "next" (knob CW), -1 for "prev" (knob CCW). Wraps around
/// at the ends. No-op when zero or one wrapper is connected, or when
/// no wrapper has been bound to a session yet.
pub async fn cycle_active(state: &Arc<DaemonState>, step: i32) {
    // Snapshot the cycle order: only wrappers with a session are
    // selectable (an unbound wrapper has nothing to make active).
    let candidates: Vec<String> = {
        let wrappers = state.wrappers.read().await;
        let mut sorted: Vec<&Wrapper> = wrappers
            .values()
            .filter(|w| w.session_id.is_some())
            .collect();
        sorted.sort_by(|a, b| {
            a.started_at_unix
                .cmp(&b.started_at_unix)
                .then_with(|| a.wrapper_id.cmp(&b.wrapper_id))
        });
        sorted.iter().map(|w| w.wrapper_id.clone()).collect()
    };
    if candidates.len() < 2 {
        debug!(count = candidates.len(), "cycle_active: nothing to cycle");
        return;
    }

    let current_idx = {
        let claude = state.claude_state.read().await;
        let active_sid = claude.active_session_id.clone();
        drop(claude);
        let wrappers = state.wrappers.read().await;
        active_sid.and_then(|sid| {
            candidates.iter().position(|wid| {
                wrappers
                    .get(wid)
                    .and_then(|w| w.session_id.as_deref())
                    == Some(sid.as_str())
            })
        })
    };

    let len = candidates.len() as i32;
    // No active session yet → start at 0 for next, end for prev.
    let base = current_idx.map(|i| i as i32).unwrap_or(if step > 0 { -1 } else { 0 });
    let next_idx = ((base + step).rem_euclid(len)) as usize;
    let target = &candidates[next_idx];

    info!(
        from = ?current_idx,
        to = next_idx,
        wrapper = %target,
        "cycling active wrapper",
    );
    if let Err(e) = set_active_wrapper(state, target).await {
        debug!(error = %e, "cycle_active: set_active_wrapper failed");
    }
}

/// Mark `wrapper_id`'s bound session as active (if any) and emit a tab list update.
pub async fn set_active_wrapper(state: &Arc<DaemonState>, wrapper_id: &str) -> Result<(), String> {
    let session_id = {
        let wrappers = state.wrappers.read().await;
        match wrappers.get(wrapper_id) {
            Some(w) => w.session_id.clone(),
            None => return Err(format!("no wrapper with id {}", wrapper_id)),
        }
    };
    if let Some(sid) = session_id {
        let mut claude = state.claude_state.write().await;
        claude.set_active_session(&sid);
    } else {
        // Wrapper not yet bound to a session — best we can do is record
        // intent. For now, leave active_session_id alone; the next hook
        // event will set it.
    }
    emit_tab_list(state).await;
    sync_active_mode_to_device(state).await;
    Ok(())
}

/// Look up a wrapper by `session_id` and send a `Write { bytes }` to it.
/// Returns true if the wrapper was found and the write was queued.
#[allow(dead_code)]
pub async fn write_to_session(state: &Arc<DaemonState>, session_id: &str, bytes: Vec<u8>) -> bool {
    let wrappers = state.wrappers.read().await;
    let Some(w) = wrappers.values().find(|w| w.session_id.as_deref() == Some(session_id)) else {
        return false;
    };
    w.tx.send(DaemonToWrapper::Write { bytes }).is_ok()
}

/// Look up a wrapper by `wrapper_id` and send a `Write { bytes }` to it.
/// Returns true if the wrapper was found and the write was queued.
#[allow(dead_code)]
pub async fn write_to_wrapper(state: &Arc<DaemonState>, wrapper_id: &str, bytes: Vec<u8>) -> bool {
    let wrappers = state.wrappers.read().await;
    let Some(w) = wrappers.get(wrapper_id) else {
        return false;
    };
    w.tx.send(DaemonToWrapper::Write { bytes }).is_ok()
}

/// Translate a HID key event to terminal bytes and write it to the active
/// wrapper's PTY. Returns `true` when the key was handled (so the caller
/// can skip legacy WS forwarding); `false` when no wrapper is connected
/// or the keycode has no terminal meaning.
///
/// F20 (the device's "Claude" button) is reserved for daemon-level
/// controls (cycle wrappers, etc.) and intentionally not typed.
pub async fn route_hid_key(state: &Arc<DaemonState>, keycode: u16) -> bool {
    if state.wrappers.read().await.is_empty() {
        return false;
    }
    if keycode == crate::keymap::KEYCODE_F20 {
        return true; // swallow — wrapper-cycle control, not a literal F20
    }
    let bytes = match crate::keymap::qmk_keycode_to_terminal_bytes(keycode) {
        Some(b) => b,
        None => return true, // wrappers exist; key has no terminal output
    };
    match write_to_target(state, "", bytes).await {
        Ok(()) => true,
        Err(e) => {
            debug!(error = %e, keycode = format!("0x{:04X}", keycode), "wrapper key route failed");
            false
        }
    }
}

/// Write a TypeString event (text + optional Enter) into the active
/// wrapper's PTY. Returns `true` when handled.
pub async fn route_hid_type_string(state: &Arc<DaemonState>, text: &str, send_enter: bool) -> bool {
    if state.wrappers.read().await.is_empty() {
        return false;
    }
    let mut bytes = text.as_bytes().to_vec();
    if send_enter {
        bytes.push(b'\r');
    }
    match write_to_target(state, "", bytes).await {
        Ok(()) => true,
        Err(e) => {
            debug!(error = %e, "wrapper type-string route failed");
            false
        }
    }
}
