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
    /// TaskCreated: present-continuous form Claude Code shows in its
    /// spinner ("Wiring OSC 9 listener…"). Friendlier on the device
    /// than the noun-phrase `task_subject` while a task is mid-flight,
    /// so we prefer it when present.
    #[serde(default)]
    task_active_form: Option<String>,
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

    // Touch the session and stash permission_mode (every hook carries
    // it). When the hook is for the *active* session and the mode
    // actually changed, push it to the device's mode LED right after.
    let mode_changed_for_active = if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        let prev = s.permission_mode.clone();
        if event.permission_mode.is_some() {
            s.permission_mode = event.permission_mode.clone();
        }
        let is_active = claude.active_session_id.as_deref() == Some(sid.as_str());
        is_active && prev != event.permission_mode
    } else {
        false
    };
    if mode_changed_for_active {
        crate::wrapper::sync_active_mode_to_device(state).await;
    }

    // Stale-alert cleanup. Three flavours:
    //   - UserPromptSubmit: the user has just sent input, so even an idle
    //     "Claude is waiting…" alert is no longer accurate — drop any
    //     alert for this session.
    //   - Notification: the host terminal's bell / attention signal.
    //     Claude rings this *while still waiting* on a permission prompt
    //     (the device's Pending alert is for the same prompt) or while
    //     idle. Either way it's not progress — leave alerts alone.
    //   - PermissionRequest: never cancels — it's the install path.
    //   - Other activity hooks (PreToolUse / PostToolUse / Stop /
    //     TaskCreated / etc.): real progress past the permission point,
    //     so drop any stale *Pending* alert (user probably answered in
    //     their terminal). Idle alerts must persist — under YOLO,
    //     Claude chains many tool calls without user input, and
    //     clearing on every PreToolUse would erase the idle alert
    //     before the user sees it.
    if let Some(ref sid) = event.session_id {
        match event_type {
            "PermissionRequest" | "Notification" => {}
            "UserPromptSubmit" => {
                crate::alerts::cancel_for_session_progress(state, sid).await;
            }
            // Tool-scoped progress: PreToolUse / PostToolUse fire
            // per-tool. For parallel tool calls Claude interleaves them
            // with PermissionRequests (e.g. PreA → PR_A → PreB → PR_B);
            // an unconditional cancel here would evict alert A as soon
            // as PreB arrives, forcing the user back to claude's
            // terminal prompt. Match by tool_name so only the same-tool
            // signal cancels.
            "PreToolUse" | "PostToolUse" => {
                crate::alerts::cancel_pending_for_tool(
                    state,
                    sid,
                    event.tool_name.as_deref(),
                )
                .await;
            }
            // Coarse session-progress: Stop, SessionStart/End,
            // PreCompact, etc. — clear any pending alert and drain
            // queued ones for this session.
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
        // If compaction landed mid-task there's no hook between PreCompact
        // and the user's next prompt, so the "Compacting…" placeholder is
        // still pinned by `active_task_id`. Restore the task subject from
        // the registry now that the user is driving again.
        if matches!(s.current_task.as_deref(), Some("Compacting…")) {
            let restored = s
                .active_task_id
                .as_ref()
                .and_then(|id| s.task_registry.get(id).cloned());
            s.current_task = restored.or_else(|| Some("Thinking…".to_string()));
        }
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

/// Hard cap on the rendered task line, in characters (not bytes). The
/// device's TFT row fits 30 glyphs at the current font; the firmware's
/// 128-byte field is a separate, looser cap. We budget the whole
/// "{name}: {detail}" string against this so the prefix is accounted for.
const MAX_TASK_LINE_CHARS: usize = 30;

/// Tool-specific glyph prefix for the device. Some tools have long
/// names that would steal too many of the 30 task-line characters
/// from the actual detail; we replace `Tool: ` with a single-glyph
/// pictogram instead. Constrained to glyphs the firmware's Terminus
/// embedded font actually carries — `○`/`✓` for to-do create/complete,
/// `●` (filled disc) for Monitor's watch-and-wait. Returns `None` for
/// tools that should keep the standard `Name: detail` format.
fn glyph_prefix(tool: &str) -> Option<&'static str> {
    match tool {
        "TaskCreate" => Some("○ "),
        "TaskComplete" => Some("✓ "),
        "Monitor" => Some("● "),
        _ => None,
    }
}

/// Extract a short task description from tool_name + tool_input for HID display.
fn extract_task_text(tool_name: Option<&str>, tool_input: Option<&serde_json::Value>) -> String {
    let name = tool_name.unwrap_or("Working");
    let detail = tool_input.and_then(|v| pick_detail(name, v)).unwrap_or_default();
    if let Some(prefix) = glyph_prefix(name) {
        if detail.is_empty() {
            return prefix.trim_end().to_string();
        }
        let prefix_chars = prefix.chars().count();
        let detail_budget = MAX_TASK_LINE_CHARS.saturating_sub(prefix_chars);
        if detail_budget == 0 {
            return prefix.trim_end().to_string();
        }
        let detail_short = truncate_for_display(&detail, detail_budget);
        return format!("{prefix}{detail_short}");
    }
    if detail.is_empty() {
        return truncate_left(name, MAX_TASK_LINE_CHARS);
    }
    let prefix_chars = name.chars().count() + 2; // "{name}: "
    let detail_budget = MAX_TASK_LINE_CHARS.saturating_sub(prefix_chars);
    if detail_budget == 0 {
        // Tool name alone exceeds the budget — drop the detail and
        // truncate the bare prefix.
        return truncate_left(name, MAX_TASK_LINE_CHARS);
    }
    let detail_short = truncate_for_display(&detail, detail_budget);
    format!("{name}: {detail_short}")
}

/// Like `extract_task_text` but tuned for the device's task2 line
/// (`last_tool_summary`). For `Bash` and `WebFetch` the leading prefix
/// is dropped — the icon column already implies a tool task and the
/// prefix would steal characters from the command / URL itself. For
/// `WebFetch` we also strip the `http(s)://` scheme so the host gets
/// to the front of the truncation budget. Other tools fall through to
/// the standard formatter.
fn extract_tool_summary(tool_name: Option<&str>, tool_input: Option<&serde_json::Value>) -> String {
    let name = tool_name.unwrap_or("Working");
    if name == "Bash" || name == "WebFetch" {
        if let Some(mut detail) = tool_input.and_then(|v| pick_detail(name, v)) {
            if name == "WebFetch" {
                detail = strip_url_scheme(&detail).to_string();
            }
            if !detail.is_empty() {
                return truncate_for_display(&detail, MAX_TASK_LINE_CHARS);
            }
        }
        return truncate_left(name, MAX_TASK_LINE_CHARS);
    }
    extract_task_text(tool_name, tool_input)
}

/// Drop a leading `http://` / `https://` so the host gets prime real
/// estate on the device's 30-char line.
fn strip_url_scheme(s: &str) -> &str {
    s.strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s)
}

/// Build a richer `TaskUpdate` summary by joining the cached task
/// subject (populated on `TaskCreated`) with a status-specific glyph.
/// `None` when the event has no resolvable task_id or the registry
/// doesn't know it (e.g., out-of-order hooks). Caller falls back to
/// the generic summary in that case.
async fn enrich_task_update(state: &DaemonState, event: &HookEvent) -> Option<String> {
    let sid = event.session_id.as_deref()?;
    let input = event.tool_input.as_ref()?;
    let task_id = input.get("task_id").and_then(|v| v.as_str())?;
    let status = input.get("status").and_then(|v| v.as_str()).unwrap_or("");

    let subject = {
        let claude = state.claude_state.read().await;
        claude
            .sessions
            .get(sid)
            .and_then(|s| s.task_registry.get(task_id).cloned())?
    };

    // Glyphs constrained to Terminus' coverage on the device. `in_progress`
    // takes the dedicated `current_task` line via the earlier branch in
    // handle_pre_tool_use; this path covers status changes that surface
    // via `last_tool_summary` (line 2).
    let glyph = match status {
        "completed" | "complete" | "done" => "✓ ",
        "cancelled" | "canceled" | "failed" | "blocked" => "✗ ",
        // pending / unknown: same circle TaskCreate uses
        _ => "○ ",
    };
    let prefix_chars = glyph.chars().count();
    let detail_budget = MAX_TASK_LINE_CHARS.saturating_sub(prefix_chars);
    if detail_budget == 0 {
        return Some(glyph.trim_end().to_string());
    }
    let detail_short = truncate_for_display(&subject, detail_budget);
    Some(format!("{glyph}{detail_short}"))
}

/// Tool-specific picker: which `tool_input` field carries the most
/// informative single-string summary for the device. Falls through to
/// `generic_pick` for tools we don't special-case.
fn pick_detail(tool: &str, input: &serde_json::Value) -> Option<String> {
    match tool {
        "Bash" => {
            // Claude often supplies a one-line `description` that's already
            // display-perfect; prefer it. Fall back to the first stage of
            // the pipeline so `cd foo && pnpm i && pnpm test` doesn't
            // silently truncate to "cd foo && pnpm i && pn…".
            if let Some(desc) = non_empty_str(input, "description") {
                return Some(desc.to_string());
            }
            non_empty_str(input, "command").map(|c| first_command_stage(c).to_string())
        }
        "Grep" => {
            let pattern = non_empty_str(input, "pattern")?;
            match non_empty_str(input, "glob") {
                Some(glob) => Some(format!("{pattern} in {glob}")),
                None => Some(pattern.to_string()),
            }
        }
        // Domain hint when one is present — `react hooks (docs.*)` is more
        // useful than just the query when Claude is scoped to a site.
        "WebSearch" => {
            let query = non_empty_str(input, "query")?;
            let domain = input
                .get("allowed_domains")
                .and_then(|d| d.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            Some(match domain {
                Some(d) => format!("{query} ({d})"),
                None => query.to_string(),
            })
        }
        // /loop's tail/watch tool. The input schema isn't documented, so
        // this is a best-effort extractor: prefer the regex/pattern Claude
        // is waiting on, fall back to the background shell id. Adjust if
        // real payloads use different field names.
        "Monitor" => {
            if let Some(pattern) = non_empty_str(input, "pattern") {
                return Some(pattern.to_string());
            }
            if let Some(regex) = non_empty_str(input, "regex") {
                return Some(regex.to_string());
            }
            non_empty_str(input, "bash_id")
                .or_else(|| non_empty_str(input, "shell_id"))
                .map(|id| format!("shell {id}"))
                .or_else(|| generic_pick(input))
        }
        "AskUserQuestion" => input
            .get("questions")
            .and_then(|q| q.as_array())
            .and_then(|arr| arr.first())
            .and_then(|q| q.get("question"))
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .or_else(|| generic_pick(input)),
        // Subagent invocation. `description` is the human-readable task
        // ("Audit ship-readiness"); `subagent_type` ("general-purpose")
        // adds nothing on the device. Fall back through the generic
        // chain if description is missing.
        "Task" => non_empty_str(input, "description")
            .map(str::to_string)
            .or_else(|| generic_pick(input)),
        // Reads as a real slash command on the display: `/review` not
        // `review`. The hook payload's `command` field is the bare name.
        "SlashCommand" => non_empty_str(input, "command").map(|c| format!("/{c}")),
        // Plan approval prompt — show the first non-empty line of the
        // plan so the user has a chance to see what they're approving
        // without raising the terminal.
        "ExitPlanMode" => non_empty_str(input, "plan")
            .and_then(|p| p.lines().map(str::trim).find(|l| !l.is_empty()))
            .map(str::to_string),
        _ => generic_pick(input),
    }
}

/// Generic field-pick chain — the order encodes "which field is most
/// likely to be human-meaningful in isolation." Prepends path-flavored
/// fields so right-anchored truncation kicks in for them.
fn generic_pick(v: &serde_json::Value) -> Option<String> {
    const FIELDS: &[&str] = &[
        "file_path",      // Edit, Write, Read, NotebookEdit (when path is the right key)
        "notebook_path",  // NotebookEdit
        "url",            // WebFetch
        "pattern",        // Glob
        "query",          // WebSearch
        "skill",          // Skill
        "subagent_type",  // Agent / Task
        "subject",        // TaskCreate / TaskUpdate
        "description",    // generic last-resort summary
        "command",        // Bash already special-cased; harmless fallback
    ];
    FIELDS
        .iter()
        .find_map(|k| non_empty_str(v, k).map(str::to_string))
}

fn non_empty_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Return the first stage of a shell pipeline — splits on `&&`, `||`,
/// `|`, `;`. Trims surrounding whitespace.
fn first_command_stage(cmd: &str) -> &str {
    let mut end = cmd.len();
    for sep in ["&&", "||", ";", "|"] {
        if let Some(i) = cmd.find(sep) {
            if i < end {
                end = i;
            }
        }
    }
    cmd[..end].trim()
}

/// Truncate `s` to fit `max_chars` characters. Right-anchors (keeps the
/// tail with a leading `…`) for paths and URLs so the filename / last
/// URL segment stays visible; left-anchors otherwise. Operates on chars
/// throughout so multi-byte UTF-8 is safe.
fn truncate_for_display(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    if looks_like_path_or_url(s) {
        let keep = max_chars.saturating_sub(1);
        let skip = total.saturating_sub(keep);
        let tail: String = s.chars().skip(skip).collect();
        format!("…{tail}")
    } else {
        truncate_left(s, max_chars)
    }
}

/// Left-anchored char-based truncate: keep the head, ellipsis on the tail.
fn truncate_left(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let head: String = s.chars().take(keep).collect();
    format!("{head}…")
}

fn looks_like_path_or_url(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.contains("://")
}

/// Pull the first row's `question` text out of an `AskUserQuestion`
/// tool input — used to populate the device alert. Returns `None` for
/// missing / empty values.
fn extract_first_question(input: Option<&serde_json::Value>) -> Option<String> {
    input
        .and_then(|v| v.get("questions"))
        .and_then(|q| q.as_array())
        .and_then(|arr| arr.first())
        .and_then(|q| q.get("question"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

    let mut summary = extract_tool_summary(event.tool_name.as_deref(), event.tool_input.as_ref());

    // TaskUpdate (non-in_progress branches) carries only `task_id` +
    // `status`, so the generic summary collapses to a bare "TaskUpdate"
    // — useless on a 30-char line. Substitute the cached subject from
    // TaskCreated and prefix with a status glyph so the user can tell
    // what changed without raising the terminal.
    if event.tool_name.as_deref() == Some("TaskUpdate") {
        if let Some(rich) = enrich_task_update(state, event).await {
            summary = rich;
        }
    }

    debug!("PreToolUse: {} ({})", event.tool_name.as_deref().unwrap_or("?"), summary);

    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = event.tool_name.clone();
        // Line 1 stays a calm "Thinking…" through tool execution — the
        // tool detail belongs on line 2 via `last_tool_summary`. When
        // a TaskCreate task owns the headline, keep its subject pinned
        // instead.
        if s.active_task_id.is_none() {
            s.current_task = Some("Thinking…".to_string());
        }
        s.last_tool_summary = Some(summary);
        s.tool_count_this_turn = s.tool_count_this_turn.saturating_add(1);
        s.active = true;
        s.phase_started_at_unix = Some(now_unix());
    }

    // AskUserQuestion is the one tool whose firing is itself a "look at
    // me" event — Claude is blocking on the user. The hook can't supply
    // the answer (multi-option text response), so we surface it as an
    // Idle alert; the user answers in the terminal and PostToolUse(
    // AskUserQuestion) clears the alert.
    if event.tool_name.as_deref() == Some("AskUserQuestion") {
        if let Some(ref sid) = event.session_id {
            if let Some(question) = extract_first_question(event.tool_input.as_ref()) {
                let session_label = compute_session_label(state, sid).await;
                alerts::show_idle_alert(state, sid, &session_label, &question).await;
            }
        }
    }

    StatusCode::OK.into_response()
}

/// PostToolUse: tool finished, but Claude is now thinking again (composing
/// the next response or deciding the next tool). Stay `active`, swap task
/// text back to "Thinking…" until the next PreToolUse or Stop, and reset
/// the phase timer so the device shows seconds-since-this-phase.
async fn handle_post_tool_use(state: &DaemonState, event: &HookEvent) {
    let is_task = event.tool_name.as_deref() == Some("Task");

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

        // The Task tool returning to the parent means a subagent just
        // finished. Claude Code's `subagentStatusLine` tick is supposed
        // to refresh the panel as terminal — but it doesn't always fire
        // a final tick, so the device gets stuck on the subagent's task
        // line until the next subagent runs (or Stop). Clear here as
        // the authoritative completion signal. Parallel subagents will
        // be repopulated by the next status-line tick.
        if is_task {
            s.subagents.clear();
        }
    }

    // PostToolUse(AskUserQuestion) means the user just answered — drop
    // the Idle alert PreToolUse raised. The shared `cancel_pending_for_
    // session` only touches Pending alerts; for an Idle one we need the
    // wider sweep that UserPromptSubmit normally does.
    if event.tool_name.as_deref() == Some("AskUserQuestion") {
        if let Some(ref sid) = event.session_id {
            alerts::cancel_for_session_progress(state, sid).await;
        }
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
    let summary = extract_tool_summary(Some(tool), event.tool_input.as_ref());
    // Used as the alert details below, where the tool name DOES belong.
    let task = extract_task_text(Some(tool), event.tool_input.as_ref());

    debug!("PermissionRequest: {} ({})", tool, summary);

    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_tool = event.tool_name.clone();
        // Same line-1 / line-2 split as PreToolUse: line 1 stays
        // "Thinking…" (or a TaskCreate subject), the tool detail
        // flows into last_tool_summary for line 2. Without this
        // guard, PermissionRequest would clobber line 1 with the
        // prefixed "Bash: …" string and leave it duplicated on
        // line 2 once last_tool_summary updates.
        if s.active_task_id.is_none() {
            s.current_task = Some("Thinking…".to_string());
        }
        s.last_tool_summary = Some(summary);
    }

    // YOLO gating, with three guardrails:
    //   1. Tools whose whole purpose is to wait on the user
    //      (`ExitPlanMode`, `AskUserQuestion`) never auto-approve —
    //      "approving" them just lets Claude resume with an empty
    //      answer, which silently corrupts the turn.
    //   2. Only auto-approve while the device is actually connected.
    //      A cable pull / USB flake drops the kill-switch out of reach,
    //      so we fall back to an explicit prompt.
    //   3. Per-wrapper opt-in. Even with global YOLO on, only wrappers
    //      that have opted in (auto-opted on YOLO ON for the focused
    //      wrapper, or via Allow on a "YOLO {tool}?" prompt) get the
    //      silent treatment. Others see an opt-in alert on their first
    //      PermissionRequest — see the Allow handler below for the
    //      bookkeeping. This prevents a parallel claude session in
    //      another tab from silently blasting through approvals.
    let (yolo, connected) = {
        let s = state.device_status.read().await;
        (s.yolo, s.connected)
    };
    let wrapper_id = match event.session_id.as_deref() {
        Some(sid) => crate::wrapper::wrapper_id_for_session(state, sid).await,
        None => None,
    };
    let user_input_tool = matches!(tool, "ExitPlanMode" | "AskUserQuestion");
    if yolo && connected && !user_input_tool {
        let (opted_in, opted_out) = match &wrapper_id {
            Some(wid) => (
                crate::wrapper::is_yolo_opted_in(state, wid).await,
                crate::wrapper::is_yolo_opted_out(state, wid).await,
            ),
            None => (false, false),
        };
        if opted_in {
            info!("YOLO: auto-approving {}", tool);
            return Json(allow_response()).into_response();
        }
        if opted_out {
            info!(
                tool,
                wrapper = ?wrapper_id,
                "Auto-approve declined for this wrapper — passing to Claude's terminal prompt"
            );
            return Json(serde_json::json!({})).into_response();
        }
        info!(
            tool,
            wrapper = ?wrapper_id,
            "Auto-approve on but wrapper not enrolled — surfacing enrollment alert"
        );
    } else if yolo && !connected {
        info!("YOLO armed but device disconnected — NOT auto-approving {}", tool);
    } else if yolo {
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
    // Two different prompts share this code path. When global
    // Auto-approve is OFF, the alert is a per-tool decision —
    // "Allow Bash?" — and Allow/Deny apply just to this PR. When
    // Auto-approve is ON but this wrapper isn't enrolled yet, the
    // alert is asking about the wrapper as a whole — Allow enrolls
    // it (and implicitly approves this PR plus everything that
    // follows), Deny declines enrollment and falls back to Claude's
    // terminal prompt for this request. Naming a specific tool here
    // would be misleading because Auto-approve covers all tools.
    let alert_text = if yolo {
        "Auto-approve this tab?".to_string()
    } else {
        format!("Allow {}?", tool)
    };

    let rx = match alerts::install_pending_alert(
        state,
        session_id.clone(),
        event.tool_name.clone(),
        &session_label,
        &alert_text,
        Some(&task),
        yolo,
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

    // Capture the Auto-approve flag at alert-creation time. Whether
    // Allow/Deny mean "enroll/decline" or "allow this PR/deny this PR"
    // depends on what the user was being asked, not on the toggle's
    // state at resolution time.
    let was_enrollment_alert = yolo;
    match outcome {
        Ok(Ok(DecisionOutcome::Allow)) => {
            if was_enrollment_alert {
                // Re-check Auto-approve now — if it was turned off mid-
                // prompt, don't enroll (the user clearly changed intent).
                let still_yolo = state.device_status.read().await.yolo;
                if still_yolo {
                    if let Some(wid) = wrapper_id.as_deref() {
                        crate::wrapper::mark_yolo_opt_in(state, wid).await;
                        info!(wrapper = wid, "Auto-approve enrolled");
                    }
                }
            }
            Json(allow_response()).into_response()
        }
        Ok(Ok(DecisionOutcome::Deny)) => {
            if was_enrollment_alert {
                // Deny on enrollment ≠ deny this PR. The user said
                // "don't enroll this tab" — record the opt-out so we
                // don't re-prompt on every subsequent PR, and fall
                // back to Claude's terminal prompt for this request.
                // The opt-out clears when Auto-approve toggles, the
                // device disconnects, or the wrapper exits.
                if let Some(wid) = wrapper_id.as_deref() {
                    crate::wrapper::mark_yolo_opt_out(state, wid).await;
                    info!(wrapper = wid, "Auto-approve declined");
                }
                Json(serde_json::json!({})).into_response()
            } else {
                Json(deny_response()).into_response()
            }
        }
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
/// `session_name`, then a right-truncated cwd via `short_cwd_label`,
/// then "Claude". `prompt_summary` was deliberately dropped from this
/// chain — first-prompt snippets like "fix the bug" produced device
/// labels noisier than the cwd they replaced.
async fn compute_session_label(state: &DaemonState, session_id: &str) -> String {
    let claude = state.claude_state.read().await;
    if let Some(s) = claude.sessions.get(session_id) {
        if let Some(name) = s.session_name.as_ref() {
            return name.clone();
        }
    }
    drop(claude);

    let wrappers = state.wrappers.read().await;
    if let Some(w) = wrappers
        .values()
        .find(|w| w.session_id.as_deref() == Some(session_id))
    {
        let label = crate::wrapper::short_cwd_label_pub(&w.cwd);
        if !label.is_empty() {
            return label;
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
    let s = claude.touch_session(sid);

    // Carry the "Compacting…" placeholder forward when Claude resumes a
    // freshly compacted session. PreCompact already painted it on the
    // previous session_id; if compaction renumbers the session, the device
    // would otherwise drop back to a blank line until the next hook fires.
    // The next UserPromptSubmit / PreToolUse / Stop will overwrite this.
    if source == "compact" {
        s.current_task = Some("Compacting…".to_string());
        s.active = true;
        s.phase_started_at_unix = Some(now_unix());
    }
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
/// context fills up). Compaction blocks generation for several seconds —
/// without an explicit task line the device just shows the prior phase's
/// "Thinking…" frozen with a climbing elapsed counter. Replace it with
/// "Compacting…" so the user can tell why Claude is paused. The 1Hz
/// ticker keeps the elapsed suffix advancing; the next non-compact hook
/// (or SessionStart-source=compact's own paint) supersedes this.
async fn handle_pre_compact(state: &DaemonState, event: &HookEvent) {
    let trigger = event.trigger.as_deref().unwrap_or("?");
    let session = event.session_id.as_deref().unwrap_or("?");
    info!(session = %session, trigger = %trigger, "PreCompact");

    if let Some(ref sid) = event.session_id {
        let mut claude = state.claude_state.write().await;
        let s = claude.touch_session(sid);
        s.current_task = Some("Compacting…".to_string());
        s.current_tool = None;
        s.active = true;
        s.phase_started_at_unix = Some(now_unix());
    }
}

/// TaskCreated: a task entry was added to the agent's task list. We
/// record `task_id → display_string` in the per-session registry,
/// preferring `task_active_form` (the gerund Claude Code shows in its
/// spinner: "Wiring OSC 9 listener…") over the noun-phrase
/// `task_subject` because the gerund reads better as a live status
/// line on the device.
///
/// We do NOT promote it to `current_task` — multiple tasks can be
/// created up front and only one runs at a time. The actual switch
/// to in-progress is signaled by `PreToolUse(TaskUpdate, in_progress)`.
async fn handle_task_created(state: &DaemonState, event: &HookEvent) {
    let (Some(sid), Some(id), Some(subject)) =
        (event.session_id.as_ref(), event.task_id.as_ref(), event.task_subject.as_ref())
    else {
        return;
    };
    let label = event
        .task_active_form
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(subject.as_str())
        .to_string();
    debug!(session = %sid, task_id = %id, subject = %subject, label = %label, "TaskCreated");
    let mut claude = state.claude_state.write().await;
    let s = claude.touch_session(sid);
    s.task_registry.insert(id.clone(), label);
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

/// Generic hook shim. Wraps each Claude Code hook in a curl call that
/// swallows errors when the daemon is offline (otherwise every hook
/// fire prints ECONNREFUSED). Written to `~/.claude/coredeck-hook.sh`.
const HOOK_SHIM_SCRIPT: &str = include_str!("../scripts/coredeck-hook.sh");

/// Install hooks, returning Ok or an error message. Single source of truth.
///
/// Non-destructive: any pre-existing hooks the user has configured for
/// the same events are preserved; only blocks recognised as "ours"
/// (referencing the embedded scripts or the daemon URL) are replaced.
/// `statusLine` / `subagentStatusLine` are single-valued in Claude Code
/// so they're overwritten — but we warn first when the prior value is
/// non-empty and not ours, so the user can roll back if surprised.
pub fn install_hooks_result(listen_addr: &str) -> Result<(), String> {
    let base_url = format!("http://{}", listen_addr);
    let settings_path = claude_settings_path();

    let register_path = write_register_script()?;
    let register_path_str = register_path.to_string_lossy().to_string();
    let hook_shim_path = write_hook_shim_script()?;
    let hook_shim_path_str = hook_shim_path.to_string_lossy().to_string();

    let mut settings: serde_json::Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("Failed to read {}: {}", settings_path.display(), e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    apply_hook_entries(&mut settings, &register_path_str, &hook_shim_path_str, &base_url);

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }

    let json = serde_json::to_string_pretty(&settings).expect("Failed to serialize settings");
    std::fs::write(&settings_path, json)
        .map_err(|e| format!("Failed to write {}: {}", settings_path.display(), e))
}

/// Install hooks on a remote SSH host. Mirror of `install_hooks_result`
/// for the case where claude lives on `<user>@<host>` reached over the
/// reverse-tunnel that `coredeck-claude --ssh` opens.
///
/// `daemon_addr` is the *local* daemon's listen address (e.g.
/// `127.0.0.1:19384`). The remote shim and statusLine entries point
/// at `127.0.0.1:<port>` because the wrapper's reverse tunnel mirrors
/// that exact port on the remote.
pub fn install_hooks_remote_result(host: &str, daemon_addr: &str) -> Result<(), String> {
    // Probe remote $HOME so we can bake absolute paths into the merged
    // settings.json. (Matching what the local install does — Claude Code
    // wants absolute command paths in hook entries.)
    let remote_home = ssh_capture(host, "echo $HOME")?;
    let remote_home = remote_home.trim();
    if remote_home.is_empty() {
        return Err(format!("ssh {}: empty $HOME", host));
    }

    let claude_dir = format!("{}/.claude", remote_home);
    let register_path = format!("{}/coredeck-register.sh", claude_dir);
    let shim_path = format!("{}/coredeck-hook.sh", claude_dir);
    let settings_path = format!("{}/settings.json", claude_dir);

    // Ensure ~/.claude exists; push the two shim scripts; chmod +x.
    ssh_run(host, &format!("mkdir -p {}", sh_quote(&claude_dir)))?;
    ssh_push_file(host, &register_path, REGISTER_SCRIPT)?;
    ssh_push_file(host, &shim_path, HOOK_SHIM_SCRIPT)?;
    ssh_run(
        host,
        &format!(
            "chmod +x {} {}",
            sh_quote(&register_path),
            sh_quote(&shim_path)
        ),
    )?;

    // Read remote settings.json, default to {} if absent or unparseable.
    let raw = ssh_capture(
        host,
        &format!(
            "cat {} 2>/dev/null || echo '{{}}'",
            sh_quote(&settings_path)
        ),
    )
    .unwrap_or_else(|_| "{}".to_string());
    let mut settings: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|_| serde_json::json!({}));

    let base_url = format!("http://{}", daemon_addr);
    apply_hook_entries(&mut settings, &register_path, &shim_path, &base_url);

    let json = serde_json::to_string_pretty(&settings).expect("Failed to serialize settings");
    ssh_push_file(host, &settings_path, &json)?;

    println!("Remote hooks installed: {}:{}", host, settings_path);
    Ok(())
}

/// Single-quote `s` for safe inclusion in a POSIX shell command line.
/// (Same logic as the wrapper's `sh_quote` — duplicated here to keep
/// the dependency graph clean; both crates only need a few lines.)
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

/// Run `ssh <host> <cmd>`, capturing stdout. Stderr is forwarded so
/// auth prompts and errors surface to the user.
fn ssh_capture(host: &str, cmd: &str) -> Result<String, String> {
    use std::process::{Command, Stdio};
    let output = Command::new("ssh")
        .args(["-o", "BatchMode=no", "-o", "ConnectTimeout=10", host, cmd])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawning ssh: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "ssh {} `{}` exited with {}",
            host,
            cmd,
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `ssh <host> <cmd>`, ignore stdout. Used for mkdir/chmod where
/// only the exit status matters.
fn ssh_run(host: &str, cmd: &str) -> Result<(), String> {
    use std::process::Stdio;
    let status = std::process::Command::new("ssh")
        .args(["-o", "BatchMode=no", "-o", "ConnectTimeout=10", host, cmd])
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("spawning ssh: {}", e))?;
    if !status.success() {
        return Err(format!(
            "ssh {} `{}` exited with {}",
            host,
            cmd,
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// Pipe `contents` into `cat > <remote_path>` over ssh. Used to push
/// shim scripts and settings.json.
fn ssh_push_file(host: &str, remote_path: &str, contents: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=no",
            "-o",
            "ConnectTimeout=10",
            host,
            &format!("cat > {}", sh_quote(remote_path)),
        ])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning ssh: {}", e))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "ssh stdin missing".to_string())?;
        stdin
            .write_all(contents.as_bytes())
            .map_err(|e| format!("writing to ssh stdin: {}", e))?;
    }
    let status = child.wait().map_err(|e| format!("waiting on ssh: {}", e))?;
    if !status.success() {
        return Err(format!(
            "ssh {} push {} exited with {}",
            host,
            remote_path,
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

/// CLI entry point — install remote hooks, exit on error.
pub fn install_claude_hooks_remote(host: &str, daemon_addr: &str) {
    match install_hooks_remote_result(host, daemon_addr) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

/// Pure merge: take a settings.json `Value`, three absolute paths/URL
/// (where the shim scripts live + the base URL the curl-only
/// statusLine entries should hit), and rewrite the `hooks`,
/// `statusLine`, and `subagentStatusLine` keys to point at us.
///
/// Non-destructive against unrelated hook entries the user maintains
/// for the same events. Returns nothing — caller persists `settings`.
pub fn apply_hook_entries(
    settings: &mut serde_json::Value,
    register_script_path: &str,
    hook_shim_path: &str,
    base_url: &str,
) {
    if !settings.is_object() {
        *settings = serde_json::json!({});
    }

    // Tool-matching hooks (PreToolUse, PostToolUse) require a "matcher"
    // field — without it they silently don't fire. Use "*" for all.
    // PermissionRequest also takes a matcher but is installed
    // separately below because its shim needs a much longer curl
    // timeout (the user might take minutes to react) and a `timeout`
    // field so Claude's own 60s default doesn't bail first.
    let tool_hook_events = ["PreToolUse", "PostToolUse"];
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

    if !settings
        .get("hooks")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        settings["hooks"] = serde_json::json!({});
    }
    let hooks_map = settings
        .get_mut("hooks")
        .and_then(|v| v.as_object_mut())
        .expect("hooks just ensured to be an object");

    let shim_command = |event: &str| format!("{} {}", hook_shim_path, event);

    for event_name in &tool_hook_events {
        merge_managed_hook(
            hooks_map,
            event_name,
            serde_json::json!({
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": shim_command(event_name),
                }]
            }),
        );
    }

    // PermissionRequest needs human-scale wait times. The shim runs
    // `curl -m 1800` so curl doesn't bail before the user taps, and
    // the entry sets `timeout: 1800` so Claude itself doesn't give up
    // at its default 60s. The daemon's own 5-minute internal timeout
    // is the actual outer limit; the longer values above just keep
    // the transport from prematurely abandoning the request.
    merge_managed_hook(
        hooks_map,
        "PermissionRequest",
        serde_json::json!({
            "matcher": "*",
            "hooks": [{
                "type": "command",
                "command": format!("{} PermissionRequest 1800", hook_shim_path),
                "timeout": 1800,
            }]
        }),
    );

    for event_name in &plain_hook_events {
        merge_managed_hook(
            hooks_map,
            event_name,
            serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": shim_command(event_name),
                }]
            }),
        );
    }

    // SessionStart: register script for wrapper correlation, plus the
    // generic shim for daemon state updates. Both fire in parallel on
    // every session start.
    merge_managed_hook(
        hooks_map,
        "SessionStart",
        serde_json::json!({
            "hooks": [
                { "type": "command", "command": register_script_path },
                { "type": "command", "command": shim_command("SessionStart") },
            ]
        }),
    );

    // statusLine and subagentStatusLine bypass the shim: they're not
    // event-name keyed under /hooks/, they read JSON from stdin via
    // $(cat), and they need response-body pass-through (statusLine) or
    // explicit /dev/null silencing (subagentStatusLine, whose stdout is
    // parsed as row overrides). Add `|| true` so a daemon-offline curl
    // failure doesn't propagate as a non-zero exit.
    let new_statusline = serde_json::json!({
        "type": "command",
        "command": format!(
            "curl -s -m 5 -X POST {}/hooks/statusline -H 'Content-Type: application/json' --data-binary @- 2>/dev/null || true",
            base_url
        ),
    });
    let new_subagent_statusline = serde_json::json!({
        "type": "command",
        "command": format!(
            "curl -s -m 5 -X POST {}/hooks/subagent-statusline -H 'Content-Type: application/json' --data-binary @- >/dev/null 2>&1 || true",
            base_url
        ),
    });
    warn_if_clobbering("statusLine", settings.get("statusLine"), &new_statusline);
    warn_if_clobbering(
        "subagentStatusLine",
        settings.get("subagentStatusLine"),
        &new_subagent_statusline,
    );
    settings["statusLine"] = new_statusline;
    settings["subagentStatusLine"] = new_subagent_statusline;
}

/// True if a hook block (a single matcher entry inside an event's array)
/// is one we'd write — i.e. references our embedded scripts or our
/// daemon's HTTP endpoint. Used to dedupe re-installs without
/// disturbing user-defined hooks for the same event.
fn is_managed_hook_block(block: &serde_json::Value) -> bool {
    let Some(hooks) = block.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    hooks.iter().any(|entry| {
        let s = entry.to_string();
        s.contains("coredeck-hook.sh")
            || s.contains("coredeck-register.sh")
            || s.contains("127.0.0.1:19384/hooks/")
            || s.contains("localhost:19384/hooks/")
    })
}

/// Merge a fresh hook block for `event_name` into `hooks_map`, replacing
/// any prior block we owned and preserving anything else.
fn merge_managed_hook(
    hooks_map: &mut serde_json::Map<String, serde_json::Value>,
    event_name: &str,
    fresh_block: serde_json::Value,
) {
    let entry = hooks_map
        .entry(event_name.to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]));
    if !entry.is_array() {
        // Unexpected shape — replace defensively rather than corrupt it.
        *entry = serde_json::Value::Array(vec![fresh_block]);
        return;
    }
    let arr = entry.as_array_mut().expect("checked is_array");
    arr.retain(|block| !is_managed_hook_block(block));
    arr.push(fresh_block);
}

/// Print a one-line stderr warning when we're about to overwrite a
/// pre-existing single-valued setting (statusLine, subagentStatusLine)
/// with content that doesn't match what we'd recognise as ours. Empty
/// or already-ours values are silent.
fn warn_if_clobbering(key: &str, existing: Option<&serde_json::Value>, replacement: &serde_json::Value) {
    let Some(existing) = existing else { return };
    if existing.is_null() {
        return;
    }
    if existing == replacement {
        return;
    }
    let serialized = existing.to_string();
    if serialized.contains("coredeck-hook.sh")
        || serialized.contains("coredeck-register.sh")
        || serialized.contains("127.0.0.1:19384/hooks/")
        || serialized.contains("localhost:19384/hooks/")
    {
        return;
    }
    eprintln!(
        "warning: overwriting existing `{}` in settings.json — Claude Code only supports one. Prior value: {}",
        key, serialized
    );
}

/// Uninstall hooks, returning Ok or an error message. Single source of truth.
pub fn uninstall_hooks_result() -> Result<(), String> {
    // Always try to remove our scripts — safe even if hooks were never installed.
    for path in [coredeck_register_script_path(), coredeck_hook_shim_script_path()] {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
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
            // For each event, drop only the blocks we own; leave any
            // user-defined blocks for the same event untouched.
            let mut empty_keys: Vec<String> = Vec::new();
            for (key, value) in obj.iter_mut() {
                let Some(arr) = value.as_array_mut() else {
                    continue;
                };
                let before = arr.len();
                arr.retain(|block| !is_managed_hook_block(block));
                if arr.len() != before {
                    changed = true;
                }
                if arr.is_empty() {
                    empty_keys.push(key.clone());
                }
            }
            for key in empty_keys {
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
///
/// Matches against the `coredeck-hook.sh` shim path (what `install` writes
/// today) and the legacy direct-URL form (older installs that pointed at
/// `127.0.0.1:19384/hooks/...`). Either signal is enough — the shim is
/// what we ship now, but a user upgrading from a previous version may
/// still have the URL form.
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
        if s.contains("coredeck-hook.sh")
            || s.contains("127.0.0.1:19384/hooks/")
            || s.contains("localhost:19384/hooks/")
        {
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

/// Path to the generic hook shim script.
fn coredeck_hook_shim_script_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("coredeck-hook.sh")
}

/// Write the embedded register script to disk and make it executable.
fn write_register_script() -> Result<std::path::PathBuf, String> {
    write_script(coredeck_register_script_path(), REGISTER_SCRIPT)
}

/// Write the embedded hook shim script to disk and make it executable.
fn write_hook_shim_script() -> Result<std::path::PathBuf, String> {
    write_script(coredeck_hook_shim_script_path(), HOOK_SHIM_SCRIPT)
}

fn write_script(path: std::path::PathBuf, contents: &str) -> Result<std::path::PathBuf, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
    }
    std::fs::write(&path, contents)
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
