//! Claude Code hook HTTP endpoints.
//!
//! Claude Code posts structured JSON events to these endpoints via its hooks system.
//! Hooks provide supplementary structured data (YOLO auto-approve, cost/context info,
//! tool activity). Display/mode management remains with the app.
//!
//! Endpoints are public (no WS lock needed) since hooks come from Claude Code, not the app.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use coredeck_protocol::{WsEventTag, encode_ws_frame};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info};

use crate::DaemonState;
use crate::alerts::{self, DecisionOutcome};
use crate::state::SubagentRow;

/// Common fields present in all Claude Code hook events.
/// Claude Code sends snake_case field names which match Rust naming directly.
#[derive(Debug, Deserialize)]
struct HookEvent {
    /// The hook event name (e.g. "PreToolUse", "PostToolUse", "PermissionRequest", "Stop")
    #[serde(default)]
    #[allow(dead_code)]
    hook_event_name: Option<String>,
    /// Claude Code session ID
    #[serde(default)]
    session_id: Option<String>,
    /// Current permission mode ("plan", "acceptEdits", "default", "dontAsk", "bypassPermissions")
    #[serde(default)]
    permission_mode: Option<String>,
    /// Tool name (for PreToolUse, PostToolUse, PermissionRequest)
    #[serde(default)]
    tool_name: Option<String>,
    /// Tool input (arbitrary JSON)
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
    /// TaskCreated / TaskCompleted: identifier of the task.
    #[serde(default)]
    task_id: Option<String>,
    /// TaskCreated / TaskCompleted: short title of the task.
    #[serde(default)]
    task_subject: Option<String>,
    /// TaskCreated: longer task description (optional).
    #[serde(default)]
    #[allow(dead_code)]
    task_description: Option<String>,
    /// Notification message (for Notification events)
    #[serde(default)]
    message: Option<String>,
    /// Notification type (for Notification events): "permission_prompt", "idle_prompt", etc.
    #[serde(default)]
    notification_type: Option<String>,
    /// User's prompt text (for UserPromptSubmit events).
    #[serde(default)]
    prompt: Option<String>,
    /// SessionStart `source` field — how the session began.
    /// Known values: `startup`, `resume`, `clear`, `compact`.
    #[serde(default)]
    source: Option<String>,
    /// PreCompact `trigger` field — what initiated compaction.
    /// Known values: `manual`, `auto`.
    #[serde(default)]
    trigger: Option<String>,
    /// SessionEnd `reason` field — how the session terminated
    /// (e.g. `clear`, `logout`, `prompt_input_exit`, `other`).
    #[serde(default)]
    reason: Option<String>,
}

/// Statusline data from Claude Code (snake_case fields).
/// Many more fields are documented (see `docs/`); we deserialize only
/// the ones the daemon currently uses. `serde(default)` ignores the rest.
#[derive(Debug, Deserialize)]
struct StatuslineData {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    session_name: Option<String>,
    #[serde(default)]
    context_window: Option<ContextWindow>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    model: Option<Model>,
    #[serde(default)]
    effort: Option<Effort>,
    #[serde(default)]
    thinking: Option<Thinking>,
}

#[derive(Debug, Deserialize)]
struct Effort {
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Thinking {
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ContextWindow {
    #[serde(default)]
    used_percentage: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Cost {
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Model {
    #[serde(default)]
    display_name: Option<String>,
}

/// Payload Claude Code pipes to the `subagentStatusLine` script every
/// refresh tick. `tasks` is the complete visible list — empty/missing
/// means no subagents are running.
#[derive(Debug, Deserialize)]
struct SubagentStatuslineData {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tasks: Option<Vec<SubagentTaskRaw>>,
}

#[derive(Debug, Deserialize)]
struct SubagentTaskRaw {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// Per docs the field is `startTime`. Format isn't pinned down — accept
    /// it as a JSON Value and coerce later (number = ms, string = ISO).
    #[serde(default, rename = "startTime")]
    start_time: Option<serde_json::Value>,
    #[serde(default, rename = "tokenCount")]
    token_count: Option<u64>,
}

/// Response for PermissionRequest when YOLO is enabled.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionResponse {
    hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HookSpecificOutput {
    hook_event_name: String,
    decision: Decision,
}

#[derive(Debug, Serialize)]
struct Decision {
    behavior: String,
    /// Only used when behavior is "deny" — explains why the request was
    /// denied. Distinct from PreToolUse's `permissionDecisionReason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// POST /hooks/{event_type}
///
/// Handles all Claude Code hook events and statusline data. After any
/// state mutation, emits an updated `WrapperTabList` snapshot to the app.
pub async fn handle_hook(
    Path(event_type): Path<String>,
    State(state): State<Arc<DaemonState>>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Log every hook payload at info level for observability
    if event_type == "statusline" {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
            info!("HOOK statusline: {}", serde_json::to_string(&v).unwrap_or_default());
        }
        let resp = handle_statusline(&state, &body).await;
        crate::wrapper::emit_tab_list(&state).await;
        return resp;
    }

    if event_type == "subagent-statusline" {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
            info!(
                "HOOK subagent-statusline: {}",
                serde_json::to_string(&v).unwrap_or_default()
            );
        }
        let resp = handle_subagent_statusline(&state, &body).await;
        crate::wrapper::emit_tab_list(&state).await;
        return resp;
    }

    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
        let json = serde_json::to_string(&v).unwrap_or_default();
        if json.len() > 200 {
            info!("HOOK {}: {}...", event_type, &json[..200]);
        } else {
            info!("HOOK {}: {}", event_type, json);
        }
    }

    forward_hook_to_app(&state, &event_type, &body).await;
    let resp = handle_claude_hook(&state, &event_type, &body).await;
    crate::wrapper::emit_tab_list(&state).await;
    resp
}

/// Handle statusline data (context window, cost, model info).
async fn handle_statusline(
    state: &DaemonState,
    body: &[u8],
) -> axum::response::Response {
    let data: StatuslineData = match serde_json::from_slice(body) {
        Ok(d) => d,
        Err(e) => {
            debug!("Failed to parse statusline: {}", e);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let session_id = data.session_id.clone().unwrap_or_default();
    if session_id.is_empty() {
        debug!("statusline payload missing session_id; skipping per-session update");
    } else {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(&session_id);
        if let Some(ref cw) = data.context_window {
            s.context_window_percent = cw.used_percentage;
        }
        if let Some(ref cost) = data.cost {
            s.cost_usd = cost.total_cost_usd;
        }
        if let Some(ref model) = data.model {
            s.model = model.display_name.clone();
        }
        if let Some(ref name) = data.session_name {
            s.session_name = Some(name.clone());
        }
        if let Some(ref e) = data.effort {
            if let Some(level) = &e.level {
                s.effort_level = Some(level.clone());
            }
        }
        if let Some(ref t) = data.thinking {
            if let Some(enabled) = t.enabled {
                s.thinking_enabled = enabled;
            }
        }
        info!(
            "Statusline updated [{}]: context={}%, cost=${:.4}, model={:?}, effort={:?}",
            session_id,
            s.context_window_percent.unwrap_or(0.0),
            s.cost_usd.unwrap_or(0.0),
            s.model,
            s.effort_level,
        );
    }

    forward_hook_to_app(state, "statusline", body).await;

    StatusCode::OK.into_response()
}

/// Handle a `subagentStatusLine` refresh tick. Replaces the per-session
/// `subagents` list wholesale with whatever Claude Code reports (the
/// payload is always the complete visible list). Returns 200 with an
/// EMPTY body — the script's stdout is interpreted by Claude Code as
/// JSON-line row overrides, and we want the default rendering.
async fn handle_subagent_statusline(
    state: &DaemonState,
    body: &[u8],
) -> axum::response::Response {
    let data: SubagentStatuslineData = match serde_json::from_slice(body) {
        Ok(d) => d,
        Err(e) => {
            debug!("Failed to parse subagent-statusline: {}", e);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    let session_id = data.session_id.clone().unwrap_or_default();
    if session_id.is_empty() {
        debug!("subagent-statusline payload missing session_id; skipping");
        return StatusCode::OK.into_response();
    }

    let rows: Vec<SubagentRow> = data
        .tasks
        .unwrap_or_default()
        .into_iter()
        .map(|t| SubagentRow {
            id: t.id,
            name: t.name,
            status: t.status,
            description: t.description,
            label: t.label,
            start_time_unix: t.start_time.and_then(coerce_start_time_unix),
            token_count: t.token_count,
        })
        .collect();

    {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(&session_id);
        s.subagents = rows;
        info!(
            "Subagents updated [{}]: {} row(s)",
            session_id,
            s.subagents.len(),
        );
    }

    forward_hook_to_app(state, "subagent-statusline", body).await;

    StatusCode::OK.into_response()
}

/// Best-effort `startTime` → unix-seconds. Accepts either a number
/// (assumed ms epoch when > 10^12, else seconds) or an ISO-8601 string.
/// Returns `None` for anything else.
fn coerce_start_time_unix(v: serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => {
            let f = n.as_f64()?;
            let secs = if f > 1e12 { f / 1000.0 } else { f };
            Some(secs as u64)
        }
        serde_json::Value::String(_) => None,
        _ => None,
    }
}

/// Handle a Claude Code hook event (PreToolUse, PostToolUse, PermissionRequest, Stop, etc.)
async fn handle_claude_hook(
    state: &DaemonState,
    event_type: &str,
    body: &[u8],
) -> axum::response::Response {
    let event: HookEvent = match serde_json::from_slice(body) {
        Ok(e) => e,
        Err(e) => {
            debug!("Failed to parse hook event {}: {}", event_type, e);
            return StatusCode::BAD_REQUEST.into_response();
        }
    };

    // Touch the session and stash permission_mode (every hook carries it).
    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        if event.permission_mode.is_some() {
            s.permission_mode = event.permission_mode.clone();
        }
    }

    // Stale-alert cleanup. Two flavours of "Claude has progressed":
    //   - UserPromptSubmit: the user has just sent input, so even an idle
    //     "Claude is waiting…" alert is no longer accurate — drop any
    //     alert for this session.
    //   - Other activity hooks (PreToolUse / PostToolUse / Stop /
    //     Notification): only stale *Pending* permission alerts need
    //     dropping (user probably answered in their terminal). Idle
    //     alerts must persist — under YOLO, Claude chains many tool
    //     calls without user input, and clearing on every PreToolUse
    //     would erase the idle alert before the user sees it.
    //   - PermissionRequest itself never cancels — it's the install path.
    if let Some(ref sid) = event.session_id {
        match event_type {
            "PermissionRequest" => {}
            "UserPromptSubmit" => {
                crate::alerts::cancel_for_session_progress(state, sid).await;
            }
            _ => {
                crate::alerts::cancel_pending_for_session(state, sid).await;
            }
        }
    }

    match event_type {
        "PreToolUse" => handle_pre_tool_use(state, &event).await,
        "PostToolUse" => {
            handle_post_tool_use(state, &event).await;
            StatusCode::OK.into_response()
        }
        "PermissionRequest" => handle_permission_request(state, &event).await,
        "Stop" => {
            handle_stop(state, &event).await;
            StatusCode::OK.into_response()
        }
        "Notification" => {
            handle_notification(state, &event).await;
            StatusCode::OK.into_response()
        }
        "UserPromptSubmit" => {
            handle_user_prompt_submit(state, &event).await;
            StatusCode::OK.into_response()
        }
        "SessionStart" => {
            handle_session_start(state, &event).await;
            StatusCode::OK.into_response()
        }
        "TaskCreated" => {
            handle_task_created(state, &event).await;
            StatusCode::OK.into_response()
        }
        "TaskCompleted" => {
            handle_task_completed(state, &event).await;
            StatusCode::OK.into_response()
        }
        "SessionEnd" => {
            handle_session_end(state, &event).await;
            StatusCode::OK.into_response()
        }
        "PreCompact" => {
            handle_pre_compact(state, &event).await;
            StatusCode::OK.into_response()
        }
        other => {
            debug!("Unknown hook event type: {}", other);
            StatusCode::OK.into_response()
        }
    }
}

/// UserPromptSubmit: the user just hit enter. Claude has the prompt and
/// is generating its first response. Mark the session as active so the
/// device shows WORKING immediately, not after the first tool call.
/// New turn = clear per-turn counters and last-tool memory. Capture a
/// short summary of the prompt for use as a session-title fallback.
async fn handle_user_prompt_submit(state: &DaemonState, event: &HookEvent) {
    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.active = true;
        s.current_tool = None;
        // Preserve the active task's subject — a mid-task user prompt
        // is still part of the same task, not a blank "Thinking…".
        if s.active_task_id.is_none() {
            s.current_task = Some("Thinking…".to_string());
        }
        s.phase_started_at_unix = Some(now_unix());
        s.last_tool_summary = None;
        s.tool_count_this_turn = 0;
        s.current_todo = None;
        if let Some(p) = event.prompt.as_deref() {
            let summary = summarize_prompt(p);
            if !summary.is_empty() {
                s.prompt_summary = Some(summary);
            }
        }
    }
}

/// Squeeze whitespace and truncate a prompt to a single short line.
fn summarize_prompt(prompt: &str) -> String {
    let collapsed: String = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, 60)
}

/// Truncate to at most `max` chars (Unicode-safe), appending an ellipsis
/// when the input was longer.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Pull the in-progress entry out of a TodoWrite tool_input payload.
/// Prefers `activeForm` (gerund phrasing, friendlier on the device) and
/// falls back to `content`. Returns None when no in_progress entry exists.
fn extract_in_progress_todo(tool_input: Option<&serde_json::Value>) -> Option<String> {
    let arr = tool_input?.get("todos")?.as_array()?;
    let entry = arr
        .iter()
        .find(|t| t.get("status").and_then(|s| s.as_str()) == Some("in_progress"))?;
    let text = entry
        .get("activeForm")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("content").and_then(|v| v.as_str()))?;
    Some(truncate_chars(text.trim(), 60))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Extract a short task description from tool_name + tool_input for HID display.
fn extract_task_text(tool_name: Option<&str>, tool_input: Option<&serde_json::Value>) -> String {
    let name = tool_name.unwrap_or("Working");
    let detail = tool_input
        .and_then(|v| {
            // Try common fields that give useful context
            v.get("command")
                .or_else(|| v.get("file_path"))
                .or_else(|| v.get("pattern"))
                .or_else(|| v.get("query"))
                .or_else(|| v.get("description"))
                .and_then(|f| f.as_str())
        })
        .unwrap_or("");

    if detail.is_empty() {
        name.to_string()
    } else {
        // Truncate for HID display (device has limited width)
        let detail_short = if detail.len() > 40 {
            &detail[..40]
        } else {
            detail
        };
        format!("{}: {}", name, detail_short)
    }
}

/// PreToolUse: mark the session as active and record current tool/task.
/// TodoWrite is special-cased: instead of noisy "TodoWrite: ..." text,
/// we extract the in-progress todo (if any) and keep the previous
/// tool's summary intact — TodoWrite is a meta-tool that doesn't
/// itself represent productive work.
async fn handle_pre_tool_use(
    state: &DaemonState,
    event: &HookEvent,
) -> axum::response::Response {
    let is_todo_write = event.tool_name.as_deref() == Some("TodoWrite");

    if is_todo_write {
        let todo = extract_in_progress_todo(event.tool_input.as_ref());
        debug!("PreToolUse TodoWrite: in_progress={:?}", todo);
        if let Some(ref sid) = event.session_id {
            let mut claude = state.claude_state.write().await;
            let s = claude.touch_session(sid);
            s.current_todo = todo;
            s.active = true;
        }
        return StatusCode::OK.into_response();
    }

    // `TaskUpdate(status: in_progress)` is the agent's "now starting
    // task X" signal. The hook payload only carries `task_id`, so the
    // human-readable subject comes from the registry populated at
    // `TaskCreated`. Other TaskUpdate variants (renaming a subject,
    // marking complete, etc.) are handled elsewhere or ignored here.
    let is_task_update_in_progress = event.tool_name.as_deref() == Some("TaskUpdate")
        && event
            .tool_input
            .as_ref()
            .and_then(|v| v.get("status"))
            .and_then(|v| v.as_str())
            == Some("in_progress");
    if is_task_update_in_progress {
        let task_id = event
            .tool_input
            .as_ref()
            .and_then(|v| v.get("task_id"))
            .and_then(|v| v.as_str());
        if let (Some(sid), Some(id)) = (event.session_id.as_ref(), task_id) {
            let mut claude = state.claude_state.write().await;
            let s = claude.touch_session(sid);
            if let Some(subject) = s.task_registry.get(id).cloned() {
                debug!(task_id = %id, subject = %subject, "TaskUpdate in_progress");
                s.active_task_id = Some(id.to_string());
                s.current_task = Some(subject);
                s.active = true;
                s.phase_started_at_unix = Some(now_unix());
            } else {
                debug!(task_id = %id, "TaskUpdate in_progress for unregistered task");
            }
        }
        return StatusCode::OK.into_response();
    }

    let task = extract_task_text(event.tool_name.as_deref(), event.tool_input.as_ref());
    debug!("PreToolUse: {}", task);

    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = event.tool_name.clone();
        // When a task is the headline, keep the task subject visible
        // and let the tool detail flow into `last_tool_summary` (the
        // device's second line) — otherwise the task name flickers
        // away on every Read/Bash/Edit.
        if s.active_task_id.is_none() {
            s.current_task = Some(task.clone());
        }
        s.last_tool_summary = Some(task);
        s.tool_count_this_turn = s.tool_count_this_turn.saturating_add(1);
        s.active = true;
        s.phase_started_at_unix = Some(now_unix());
    }

    StatusCode::OK.into_response()
}

/// PostToolUse: tool finished, but Claude is now thinking again (composing
/// the next response or deciding the next tool). Stay `active`, swap task
/// text back to "Thinking…" until the next PreToolUse or Stop, and reset
/// the phase timer so the device shows seconds-since-this-phase.
async fn handle_post_tool_use(state: &DaemonState, event: &HookEvent) {
    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = None;
        // Keep the active task's subject pinned across tool calls;
        // only fall back to "Thinking…" when no task owns the line.
        if s.active_task_id.is_none() {
            s.current_task = Some("Thinking…".to_string());
        }
        s.phase_started_at_unix = Some(now_unix());
    }
}

/// PermissionRequest: if YOLO is enabled, auto-approve immediately
/// (except ExitPlanMode — the user should explicitly approve the plan).
/// Otherwise, ship the request to the device as an interactive alert and
/// park the HTTP response on a oneshot. The next HID input from the
/// device resolves it (Esc / Ctrl-C → deny, anything else → allow).
/// Falls back to Claude's own terminal prompt on timeout, missing
/// device, or when another alert is already live.
async fn handle_permission_request(
    state: &DaemonState,
    event: &HookEvent,
) -> axum::response::Response {
    let tool = event.tool_name.as_deref().unwrap_or("unknown");
    let task = extract_task_text(
        Some(tool),
        event.tool_input.as_ref(),
    );

    debug!("PermissionRequest: {}", task);

    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = event.tool_name.clone();
        s.current_task = Some(task.clone());
    }

    // Check YOLO flag — but never auto-approve ExitPlanMode.
    // The whole point of plan mode is human review before execution.
    let yolo = state.device_status.read().await.yolo;
    if yolo && tool != "ExitPlanMode" {
        info!("YOLO: auto-approving {}", tool);
        return Json(allow_response()).into_response();
    }

    if yolo {
        info!("YOLO: NOT auto-approving {} — requires explicit approval", tool);
    }

    let session_id = match event.session_id.clone() {
        Some(s) => s,
        None => return Json(serde_json::json!({})).into_response(),
    };

    // Stash request details for the Notification(permission_prompt) handler
    // (still useful for the GUI app's WS alert path when no device is
    // available).
    {
        let mut claude = state.claude_state.write().await;
        claude.pending_permissions.insert(session_id.clone(), crate::state::PendingPermission {
            tool_name: event.tool_name.clone(),
            tool_input: event.tool_input.clone(),
        });
    }

    let session_label = compute_session_label(state, &session_id).await;
    let alert_text = format!("Allow {}?", tool);

    let rx = match alerts::install_pending_alert(
        state,
        session_id.clone(),
        event.tool_name.clone(),
        &session_label,
        &alert_text,
        Some(&task),
    )
    .await
    {
        Some(rx) => rx,
        None => {
            // Couldn't park (no device, or another alert live) — let
            // Claude show its own prompt.
            return Json(serde_json::json!({})).into_response();
        }
    };

    info!(tool, session = %session_id, "permission prompt parked on device");

    // 5-minute timeout — long enough for the user to walk away and come
    // back, short enough that abandoned requests don't pin a tokio task
    // forever. On timeout we drop back to "no decision" → Claude's
    // terminal prompt.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), rx).await;

    // Make sure the device clears whatever it was showing, even if the
    // sender was dropped (timeout / cancellation).
    alerts::clear_alert(state).await;

    // Clean up the pending_permissions entry — we won't be needing the
    // notification path now.
    {
        let mut claude = state.claude_state.write().await;
        claude.pending_permissions.remove(&session_id);
    }

    match outcome {
        Ok(Ok(DecisionOutcome::Allow)) => Json(allow_response()).into_response(),
        Ok(Ok(DecisionOutcome::Deny)) => Json(deny_response()).into_response(),
        Ok(Err(_)) => {
            debug!("permission oneshot dropped without decision");
            Json(serde_json::json!({})).into_response()
        }
        Err(_) => {
            info!("permission decision timed out for {}", tool);
            Json(serde_json::json!({})).into_response()
        }
    }
}

fn allow_response() -> PermissionResponse {
    PermissionResponse {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PermissionRequest".to_string(),
            decision: Decision {
                behavior: "allow".to_string(),
                message: None,
            },
        },
    }
}

fn deny_response() -> PermissionResponse {
    PermissionResponse {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PermissionRequest".to_string(),
            decision: Decision {
                behavior: "deny".to_string(),
                message: Some("Rejected from CoreDeck device".to_string()),
            },
        },
    }
}

/// Best-effort label for an alert's session line — prefers the session's
/// `session_name`, then the user's prompt summary, then the wrapper's
/// cwd basename, then "Claude".
async fn compute_session_label(state: &DaemonState, session_id: &str) -> String {
    let claude = state.claude_state.read().await;
    if let Some(s) = claude.sessions.get(session_id) {
        if let Some(name) = s.session_name.as_ref() {
            return name.clone();
        }
        if let Some(p) = s.prompt_summary.as_ref() {
            return p.clone();
        }
    }
    drop(claude);

    let wrappers = state.wrappers.read().await;
    if let Some(w) = wrappers
        .values()
        .find(|w| w.session_id.as_deref() == Some(session_id))
    {
        let cwd = w.cwd.trim_end_matches('/').rsplit('/').next().unwrap_or(&w.cwd);
        if !cwd.is_empty() {
            return cwd.to_string();
        }
    }

    "Claude".to_string()
}

/// Notification: handle idle_prompt (Claude is waiting for user input)
/// by raising a device alert. permission_prompt is forwarded to a
/// connected GUI app for legacy alert display, but the interactive
/// `PermissionRequest` handler is the primary path now.
async fn handle_notification(state: &DaemonState, event: &HookEvent) {
    let kind = event.notification_type.as_deref().unwrap_or("");
    let message = event.message.as_deref().unwrap_or("");

    match kind {
        "idle_prompt" => {
            let session_id = match event.session_id.as_deref() {
                Some(s) if !s.is_empty() => s,
                _ => {
                    debug!("idle_prompt without session_id; skipping alert");
                    return;
                }
            };
            let session_label = compute_session_label(state, session_id).await;
            let text = if message.is_empty() {
                "Waiting for input"
            } else {
                message
            };
            info!(session = %session_id, "idle prompt: {}", text);
            alerts::show_idle_alert(state, session_id, &session_label, text).await;
        }
        "permission_prompt" => {
            // Look up stored PermissionRequest details for this session.
            let details = if let Some(ref sid) = event.session_id {
                let claude = state.claude_state.read().await;
                claude.pending_permissions.get(sid).map(|p| {
                    extract_task_text(p.tool_name.as_deref(), p.tool_input.as_ref())
                })
            } else {
                None
            };

            info!("Permission prompt: {} (details: {:?})", message, details);

            // Forward enriched envelope to the GUI app (if any) — kept
            // for compat with the legacy WS-alert path.
            let envelope = serde_json::json!({
                "event": "permission_prompt",
                "data": {
                    "session_id": event.session_id,
                    "message": message,
                    "details": details,
                }
            });
            let payload = serde_json::to_vec(&envelope).unwrap_or_default();
            let frame = encode_ws_frame(WsEventTag::ClaudeHookEvent as u8, 0, &payload);

            let guard = state.ws_client.lock().await;
            if let Some(client) = guard.as_ref() {
                let _ = client.tx.send(frame);
            }
        }
        _ => {
            debug!("Notification ({}): {:?}", kind, event.message);
        }
    }
}

/// Stop: Claude Code has finished. Clear active state for this session.
async fn handle_stop(state: &DaemonState, event: &HookEvent) {
    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = None;
        s.current_task = None;
        s.active = false;
        s.phase_started_at_unix = None;
        s.last_tool_summary = None;
        s.tool_count_this_turn = 0;
        s.current_todo = None;
        s.subagents.clear();
        claude.pending_permissions.remove(sid);
    }
}

/// SessionStart: Claude has begun a new session (or resumed/cleared/compacted
/// one). Touch the entry — `handle_claude_hook` already did that — and record
/// the `source` so callers can tell why we have this session_id. The wrapper
/// correlation is handled separately by the `coredeck-register.sh` command
/// hook hitting `POST /wrapper/register`.
async fn handle_session_start(state: &DaemonState, event: &HookEvent) {
    let Some(ref sid) = event.session_id else {
        return;
    };
    let source = event.source.as_deref().unwrap_or("?");
    info!(session = %sid, source = %source, "SessionStart");

    // For `clear` / `compact` / `resume`, the session_id may differ from the
    // prior turn's. Don't preemptively promote to active — wait for an
    // explicit focus signal (OSC 1004) or wrapper-register bootstrap. But do
    // ensure the entry exists so subsequent hooks find it.
    let mut claude = state.claude_state.write().await;
    let _ = claude.touch_session(sid);
}

/// SessionEnd: Claude has terminated this session. Drop everything we know
/// about it so long-running daemons don't accumulate state per turn-of-the-
/// week. Wrapper bookkeeping is independent — wrappers go away when their
/// WS connection closes.
async fn handle_session_end(state: &DaemonState, event: &HookEvent) {
    let Some(ref sid) = event.session_id else {
        return;
    };
    let reason = event.reason.as_deref().unwrap_or("?");
    info!(session = %sid, reason = %reason, "SessionEnd");

    // Cancel any device alert tied to this session before we drop the
    // backing state — the alert subsystem reads session_id from claude_state
    // for label rendering on relocation.
    crate::alerts::cancel_for_session_progress(state, sid).await;

    let mut claude = state.claude_state.write().await;
    claude.sessions.remove(sid);
    claude.pending_permissions.remove(sid);
    if claude.active_session_id.as_deref() == Some(sid.as_str()) {
        claude.active_session_id = None;
    }
}

/// PreCompact: Claude is about to compact (manual `/compact` or auto when
/// context fills up). The session continues across compaction with a new
/// `session_id` — old session gets a SessionEnd, then SessionStart fires
/// with `source: compact`. Today we just log; if compaction-aware UX
/// becomes useful we can flag the session as `active` and update the
/// device with a "Compacting…" task.
async fn handle_pre_compact(_state: &DaemonState, event: &HookEvent) {
    let trigger = event.trigger.as_deref().unwrap_or("?");
    let session = event.session_id.as_deref().unwrap_or("?");
    info!(session = %session, trigger = %trigger, "PreCompact");
}

/// TaskCreated: a task entry was added to the agent's task list. We
/// just record `task_id → task_subject` in the per-session registry.
/// We do NOT promote it to `current_task` — multiple tasks can be
/// created up front and only one runs at a time. The actual switch
/// to in-progress is signaled by `PreToolUse(TaskUpdate, in_progress)`.
async fn handle_task_created(state: &DaemonState, event: &HookEvent) {
    let (Some(sid), Some(id), Some(subject)) =
        (event.session_id.as_ref(), event.task_id.as_ref(), event.task_subject.as_ref())
    else {
        return;
    };
    debug!(session = %sid, task_id = %id, subject = %subject, "TaskCreated");
    let mut claude = state.claude_state.write().await;
    let s = claude.touch_session(sid);
    s.task_registry.insert(id.clone(), subject.clone());
}

/// TaskCompleted: a task was marked complete via `TaskUpdate`. Drop
/// the registry entry; if it was the active one, clear the pointer so
/// the next PreToolUse / Thinking phase repaints `current_task`.
async fn handle_task_completed(state: &DaemonState, event: &HookEvent) {
    let (Some(sid), Some(id)) = (event.session_id.as_ref(), event.task_id.as_ref()) else {
        return;
    };
    debug!(session = %sid, task_id = %id, "TaskCompleted");
    let mut claude = state.claude_state.write().await;
    let s = claude.touch_session(sid);
    s.task_registry.remove(id);
    if s.active_task_id.as_deref() == Some(id.as_str()) {
        s.active_task_id = None;
        // Don't fall back to another task — without an explicit "now
        // executing X" signal we'd be guessing. Let PostToolUse paint
        // "Thinking…" or the next TaskUpdate(in_progress) repaint it.
        s.current_task = None;
    }
}

/// Forward a hook event to the connected app via WS (best-effort).
/// The app can use this to react to Claude Code events (e.g., PermissionRequest for YOLO UI).
async fn forward_hook_to_app(state: &DaemonState, event_type: &str, body: &[u8]) {
    let guard = state.ws_client.lock().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => return, // no app connected
    };

    // Build a compact JSON envelope: {"event":"PreToolUse","data":{...}}
    let envelope = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(data) => serde_json::json!({
            "event": event_type,
            "data": data,
        }),
        Err(_) => return,
    };

    let payload = serde_json::to_vec(&envelope).unwrap_or_default();
    let frame = encode_ws_frame(WsEventTag::ClaudeHookEvent as u8, 0, &payload);
    let _ = client.tx.send(frame);
}

// ── Hook configuration install/uninstall ────────────────────────────

/// SessionStart command hook script — embedded so the daemon binary is
/// self-contained. Written to `~/.claude/coredeck-register.sh` on install.
const REGISTER_SCRIPT: &str = include_str!("../scripts/coredeck-register.sh");

/// Install hooks, returning Ok or an error message. Single source of truth.
pub fn install_hooks_result(listen_addr: &str) -> Result<(), String> {
    let base_url = format!("http://{}", listen_addr);
    let settings_path = claude_settings_path();

    let script_path = write_register_script()?;
    let script_path_str = script_path.to_string_lossy().to_string();

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("Failed to read {}: {}", settings_path.display(), e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // HTTP hooks don't support the "async" field (that's command-only).
    // HTTP hooks are non-blocking on failure/timeout per Claude Code docs.
    //
    // Tool-matching hooks (PreToolUse, PostToolUse, PermissionRequest) require
    // a "matcher" field — without it they silently don't fire. Use "*" for all.
    // Non-tool hooks (Stop, Notification) don't use matchers.
    let tool_hook_events = ["PreToolUse", "PostToolUse", "PermissionRequest"];
    let plain_hook_events = [
        "Stop",
        "Notification",
        "UserPromptSubmit",
        "SessionEnd",
        "PreCompact",
        // TaskCreated/TaskCompleted don't support matchers per the
        // Claude Code hooks docs and fire on every occurrence.
        "TaskCreated",
        "TaskCompleted",
    ];

    let mut hooks_obj = serde_json::Map::new();

    for event_name in &tool_hook_events {
        hooks_obj.insert(
            event_name.to_string(),
            serde_json::json!([{
                "matcher": "*",
                "hooks": [{
                    "type": "http",
                    "url": format!("{}/hooks/{}", base_url, event_name),
                }]
            }]),
        );
    }

    for event_name in &plain_hook_events {
        hooks_obj.insert(
            event_name.to_string(),
            serde_json::json!([{
                "hooks": [{
                    "type": "http",
                    "url": format!("{}/hooks/{}", base_url, event_name),
                }]
            }]),
        );
    }

    // SessionStart: command hook for wrapper correlation, plus http for
    // daemon state updates. Both fire in parallel on every session start.
    hooks_obj.insert(
        "SessionStart".to_string(),
        serde_json::json!([{
            "hooks": [
                { "type": "command", "command": script_path_str },
                { "type": "http", "url": format!("{}/hooks/SessionStart", base_url) },
            ]
        }]),
    );

    settings["hooks"] = serde_json::Value::Object(hooks_obj);
    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!(
            "curl -s -X POST {}/hooks/statusline -H 'Content-Type: application/json' -d \"$(cat)\"",
            base_url
        ),
    });
    // subagentStatusLine: stdout is parsed as JSON-line row overrides,
    // so suppress curl's response output (our endpoint returns empty,
    // but redirect defensively in case of transport errors).
    settings["subagentStatusLine"] = serde_json::json!({
        "type": "command",
        "command": format!(
            "curl -s -X POST {}/hooks/subagent-statusline -H 'Content-Type: application/json' -d \"$(cat)\" >/dev/null 2>&1",
            base_url
        ),
    });

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let json = serde_json::to_string_pretty(&settings).expect("Failed to serialize settings");
    std::fs::write(&settings_path, json)
        .map_err(|e| format!("Failed to write {}: {}", settings_path.display(), e))
}

/// Uninstall hooks, returning Ok or an error message. Single source of truth.
pub fn uninstall_hooks_result() -> Result<(), String> {
    // Always try to remove the register script — it's safe even if hooks were
    // never installed.
    let script_path = coredeck_register_script_path();
    if script_path.exists() {
        let _ = std::fs::remove_file(&script_path);
    }

    let settings_path = claude_settings_path();
    if !settings_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&settings_path)
        .map_err(|e| format!("Failed to read {}: {}", settings_path.display(), e))?;
    let mut settings: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", settings_path.display(), e))?;

    let mut changed = false;

    if let Some(hooks) = settings.get_mut("hooks") {
        if let Some(obj) = hooks.as_object_mut() {
            let keys_to_remove: Vec<String> = obj
                .iter()
                .filter(|(_, v)| {
                    let s = v.to_string();
                    s.contains("127.0.0.1:19384/hooks/")
                        || s.contains("localhost:19384/hooks/")
                        || s.contains("coredeck-register.sh")
                })
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys_to_remove {
                obj.remove(&key);
                changed = true;
            }
            if obj.is_empty() {
                settings.as_object_mut().unwrap().remove("hooks");
            }
        }
    }

    if let Some(sl) = settings.get("statusLine") {
        let s = sl.to_string();
        if s.contains("127.0.0.1:19384") || s.contains("localhost:19384") {
            settings.as_object_mut().unwrap().remove("statusLine");
            changed = true;
        }
    }

    if let Some(sl) = settings.get("subagentStatusLine") {
        let s = sl.to_string();
        if s.contains("127.0.0.1:19384") || s.contains("localhost:19384") {
            settings.as_object_mut().unwrap().remove("subagentStatusLine");
            changed = true;
        }
    }

    if changed {
        let json = serde_json::to_string_pretty(&settings).expect("Failed to serialize settings");
        std::fs::write(&settings_path, json)
            .map_err(|e| format!("Failed to write {}: {}", settings_path.display(), e))?;
    }
    Ok(())
}

/// CLI entry point — install hooks, exit on error.
pub fn install_claude_hooks(listen_addr: &str) {
    match install_hooks_result(listen_addr) {
        Ok(()) => println!("Hooks installed: {}", claude_settings_path().display()),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

/// CLI entry point — uninstall hooks, exit on error.
pub fn uninstall_claude_hooks() {
    match uninstall_hooks_result() {
        Ok(()) => println!("Hooks uninstalled: {}", claude_settings_path().display()),
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

/// Check if CoreDeck hooks are installed in ~/.claude/settings.json.
pub fn are_hooks_installed() -> bool {
    let settings_path = claude_settings_path();
    let content = match std::fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let settings: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if let Some(hooks) = settings.get("hooks") {
        let s = hooks.to_string();
        if s.contains("127.0.0.1:19384/hooks/") || s.contains("localhost:19384/hooks/") {
            return true;
        }
    }
    false
}

/// Path to ~/.claude/settings.json (user-global settings).
/// Note: ~/.claude/settings.local.json is NOT a valid Claude Code settings location.
/// Only ~/.claude/settings.json, .claude/settings.json, and .claude/settings.local.json are recognized.
fn claude_settings_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("settings.json")
}

/// Path to the SessionStart correlation script.
fn coredeck_register_script_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("coredeck-register.sh")
}

/// Write the embedded register script to disk and make it executable.
fn write_register_script() -> Result<std::path::PathBuf, String> {
    let path = coredeck_register_script_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    std::fs::write(&path, REGISTER_SCRIPT)
        .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&path, perms)
            .map_err(|e| format!("Failed to chmod {}: {}", path.display(), e))?;
    }
    Ok(path)
}
