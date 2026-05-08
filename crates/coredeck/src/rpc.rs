//! HTTP REST endpoints for third-party access.
//!
//! Read-only endpoints always work. Mutating endpoints return 409 when a WS
//! client holds the lock. The daemon keeps the HID handle open for its
//! lifetime — no transient open/close per request — so handlers just lock
//! `state.hid` and use it directly. (Older revisions cycled the handle to
//! release the keyboard to the system between calls; that complexity was
//! retired together with the GUI app, since the wrapper-driven flow has no
//! moments where another consumer needs the device.)

use coredeck_protocol::{
    AlertRequest, ApiError, BrightnessRequest, ClearAlertRequest, DaemonStatus,
    DisplayUpdateRequest, SetModeRequest, SoftKeyConfig, SoftKeyType,
};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::DaemonState;
use crate::hid::HidManager;
use crate::hooks;

/// Make sure the HID handle is open before a handler uses it. The daemon
/// normally opens the device on `DeviceAvailable` and keeps it open, but
/// hotplug ordering, an early request after startup, or a recent
/// disconnect can leave it briefly closed — recover here.
fn ensure_device_open(hid: &HidManager) -> Result<(), String> {
    if hid.is_connected() {
        return Ok(());
    }
    if !hid.is_device_available() {
        return Err("Device not available".into());
    }
    hid.open_device().map_err(|e| e.to_string())
}

/// GET /api/status — always available
pub async fn get_status(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    let status = state.device_status.read().await;
    let ws_locked = state.ws_client.lock().await.is_some();

    Json(DaemonStatus {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        device_available: status.available,
        device_connected: status.connected,
        device_name: status.device_name.clone(),
        firmware_version: status.firmware_version.clone(),
        device_mode: status.mode,
        device_yolo: status.yolo,
        ws_locked,
    })
}

/// POST /api/display
pub async fn post_display(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<DisplayUpdateRequest>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let update = coredeck_protocol::DisplayUpdate {
        session: req.session,
        task: req.task,
        task2: req.task2,
        tabs: req.tabs,
        active: req.active,
        context_percent: None,
        cost_usd: None,
        model: None,
    };
    let result = hid.send_display_update(&update);

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// POST /api/alert
pub async fn post_alert(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<AlertRequest>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.send_alert(req.tab, &req.session, &req.text, req.details.as_deref());

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// POST /api/alert/clear
pub async fn post_alert_clear(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ClearAlertRequest>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.clear_alert(req.tab);

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// POST /api/brightness
pub async fn post_brightness(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<BrightnessRequest>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.set_brightness(req.level, req.save);

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// POST /api/mode
pub async fn post_mode(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<SetModeRequest>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.set_mode(req.mode);

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// GET /api/version
pub async fn get_version(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (StatusCode::CONFLICT, Json(ApiError { error: "device locked by WebSocket client".into() })).into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let version = hid.query_version();

    drop(hid);

    Json(serde_json::json!({ "version": version })).into_response()
}

/// GET /api/hooks/status — check if CoreDeck hooks are installed
pub async fn get_hooks_status() -> impl IntoResponse {
    Json(serde_json::json!({ "installed": hooks::are_hooks_installed() }))
}

/// POST /api/hooks/install — install CoreDeck hooks into ~/.claude/settings.json
pub async fn post_hooks_install(
    State(state): State<Arc<DaemonState>>,
) -> impl IntoResponse {
    // Use the daemon's listen address from state
    let listen_addr = &state.listen_addr;
    let result = hooks::install_hooks_result(listen_addr);
    state.send_tray_update(crate::state::TrayUpdate::HooksInstalled(
        hooks::are_hooks_installed(),
    ));
    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response(),
    }
}

/// POST /api/hooks/uninstall — remove CoreDeck hooks from ~/.claude/settings.json
pub async fn post_hooks_uninstall(
    State(state): State<Arc<DaemonState>>,
) -> impl IntoResponse {
    let result = hooks::uninstall_hooks_result();
    state.send_tray_update(crate::state::TrayUpdate::HooksInstalled(
        hooks::are_hooks_installed(),
    ));
    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response(),
    }
}

// ── Soft-keys ──────────────────────────────────────────────────────

/// Body for `PUT /api/soft-keys/:index` — `index` is in the path, so the body
/// only carries type + data + optional persistence flag.
#[derive(Debug, Deserialize)]
pub struct SoftKeyUpdate {
    pub key_type: SoftKeyType,
    #[serde(default)]
    pub data: Vec<u8>,
    #[serde(default)]
    pub save: bool,
}

/// GET /api/soft-keys — return all 3 soft-key configurations from the device.
/// Returns 503 if the device isn't reachable, 409 if a WS client owns it.
pub async fn get_soft_keys(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ApiError { error: "device locked by WebSocket client".into() }),
        )
            .into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let mut configs: Vec<SoftKeyConfig> = Vec::with_capacity(3);
    let mut err: Option<String> = None;
    for index in 0..3u8 {
        match hid.get_soft_key(index) {
            Ok(c) => configs.push(c),
            Err(e) => {
                err = Some(format!("get_soft_key({index}) failed: {e}"));
                break;
            }
        }
    }

    drop(hid);

    if let Some(e) = err {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response();
    }
    Json(configs).into_response()
}

/// PUT /api/soft-keys/:index — set a single soft key.
pub async fn put_soft_key(
    State(state): State<Arc<DaemonState>>,
    Path(index): Path<u8>,
    Json(req): Json<SoftKeyUpdate>,
) -> impl IntoResponse {
    if index > 2 {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: format!("invalid soft-key index {index}; must be 0..=2") }),
        )
            .into_response();
    }
    if state.ws_client.lock().await.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ApiError { error: "device locked by WebSocket client".into() }),
        )
            .into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.set_soft_key(index, req.key_type, &req.data, req.save);

    drop(hid);

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

/// POST /api/soft-keys/reset — reset all 3 soft keys to firmware defaults.
/// Returns the post-reset configs so the client can refresh its UI without
/// a follow-up GET.
pub async fn post_soft_keys_reset(
    State(state): State<Arc<DaemonState>>,
) -> impl IntoResponse {
    if state.ws_client.lock().await.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ApiError { error: "device locked by WebSocket client".into() }),
        )
            .into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    let result = hid.reset_soft_keys();

    drop(hid);

    match result {
        Ok(configs) => Json(configs.to_vec()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e.to_string() })).into_response(),
    }
}

// ── Soft-key presets ───────────────────────────────────────────────

/// GET /api/soft-keys/presets — list built-in + user presets so the
/// settings page can render the dropdown without a second round-trip.
pub async fn get_soft_key_presets() -> impl IntoResponse {
    let user = crate::presets::UserPresetStore::load();
    Json(serde_json::json!({
        "builtin": crate::presets::builtin_presets(),
        "user": user.presets,
    }))
}

/// Body for `POST /api/soft-keys/presets/apply` — name of a built-in or
/// user preset.
#[derive(Debug, Deserialize)]
pub struct ApplyPresetRequest {
    pub name: String,
}

/// POST /api/soft-keys/presets/apply — set all three soft keys to the
/// configuration carried by the named preset.
pub async fn apply_soft_key_preset(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<ApplyPresetRequest>,
) -> impl IntoResponse {
    // Find preset by name (built-in first, then user).
    let preset = crate::presets::builtin_presets()
        .into_iter()
        .find(|p| p.name == req.name)
        .or_else(|| {
            let user = crate::presets::UserPresetStore::load();
            user.presets.into_iter().find(|p| p.name == req.name)
        });
    let Some(preset) = preset else {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError { error: format!("preset {:?} not found", req.name) }),
        )
            .into_response();
    };

    if state.ws_client.lock().await.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(ApiError { error: "device locked by WebSocket client".into() }),
        )
            .into_response();
    }

    let hid = state.hid.lock().await;
    if let Err(e) = ensure_device_open(&hid) {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(ApiError { error: e })).into_response();
    }

    // Only save EEPROM on the last set — committing after each of the
    // three writes triggers three sequential flash writes back-to-back,
    // which leaves the firmware unresponsive long enough for a
    // follow-up GetSoftKey to time out.
    let mut err: Option<String> = None;
    let last = preset.keys.len().saturating_sub(1);
    for (idx, key) in preset.keys.iter().enumerate() {
        let save = idx == last;
        if let Err(e) = hid.set_soft_key(idx as u8, key.key_type, &key.data, save) {
            err = Some(format!("key {idx}: {e}"));
            break;
        }
    }

    drop(hid);

    match err {
        None => StatusCode::OK.into_response(),
        Some(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response(),
    }
}

/// Body for `POST /api/soft-keys/presets/save` — save current keys as a
/// named user preset (upsert; conflicts with built-in names rejected).
#[derive(Debug, Deserialize)]
pub struct SavePresetRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub keys: [crate::presets::PresetKey; 3],
}

/// POST /api/soft-keys/presets/save — persist a user preset.
pub async fn save_soft_key_preset(
    Json(req): Json<SavePresetRequest>,
) -> impl IntoResponse {
    let trimmed = req.name.trim().to_string();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "preset name is required".into() }),
        )
            .into_response();
    }
    if crate::presets::is_builtin_name(&trimmed) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: format!("'{}' collides with a built-in preset name", trimmed) }),
        )
            .into_response();
    }

    let mut store = crate::presets::UserPresetStore::load();
    store.upsert(crate::presets::Preset {
        name: trimmed,
        description: req.description,
        keys: req.keys,
    });
    match store.save() {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response(),
    }
}

/// DELETE /api/soft-keys/presets/{name} — remove a user preset.
/// Built-ins are immutable; deleting one returns 400.
pub async fn delete_soft_key_preset(Path(name): Path<String>) -> impl IntoResponse {
    if crate::presets::is_builtin_name(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError { error: "built-in presets cannot be deleted".into() }),
        )
            .into_response();
    }
    let mut store = crate::presets::UserPresetStore::load();
    if !store.remove(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(ApiError { error: format!("preset {:?} not found", name) }),
        )
            .into_response();
    }
    match store.save() {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError { error: e })).into_response(),
    }
}

// ── Wrappers debug view ────────────────────────────────────────────

/// GET /api/wrappers — list connected wrappers and their bound sessions.
/// Read-only; useful for the settings page debug pane.
pub async fn get_wrappers(State(state): State<Arc<DaemonState>>) -> impl IntoResponse {
    let wrappers = state.wrappers.read().await;
    let claude = state.claude_state.read().await;
    let active_sid = claude.active_session_id.as_deref();

    let rows: Vec<serde_json::Value> = wrappers
        .values()
        .map(|w| {
            let active = w
                .session_id
                .as_deref()
                .zip(active_sid)
                .map(|(a, b)| a == b)
                .unwrap_or(false);
            serde_json::json!({
                "wrapper_id": w.wrapper_id,
                "pid": w.pid,
                "cwd": w.cwd,
                "started_at_unix": w.started_at_unix,
                "session_id": w.session_id,
                "host_terminal_kind": w.host_terminal.as_ref().map(|h| format!("{:?}", h.kind)),
                "active": active,
            })
        })
        .collect();

    Json(rows).into_response()
}

// ── Static settings page ───────────────────────────────────────────

/// HTML/CSS/JS for the settings page, embedded so the daemon binary is
/// self-contained.
const SETTINGS_HTML: &str = include_str!("../static/settings.html");

/// GET /settings (and /settings/) — serve the embedded settings page.
pub async fn get_settings_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        SETTINGS_HTML,
    )
}
