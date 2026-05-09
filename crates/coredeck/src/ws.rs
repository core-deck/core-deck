//! WebSocket server — exclusive connection lock and bidirectional message relay
//!
//! Only one WebSocket client (the app) may be connected at a time.
//! While a WS client holds the lock, HTTP mutating endpoints return 409.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use coredeck_protocol::{
    decode_ws_frame, encode_ws_frame, DeviceInfo, DeviceMode, SetActiveWrapperRequest, SoftKeyType,
    WrapperWriteRequest, WsCommandTag, WsEventTag, WsResponseTag,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::state::DaemonEvent;
use crate::DaemonState;

/// Handle for the connected WS client
pub struct WsClientHandle {
    /// Send frames to the connected client
    pub tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// WebSocket upgrade handler
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<DaemonState>>,
) -> impl IntoResponse {
    // Check if a client already holds the lock
    {
        let guard = state.ws_client.lock().await;
        if guard.is_some() {
            return axum::http::StatusCode::CONFLICT.into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_ws_connection(socket, state))
}

async fn handle_ws_connection(socket: WebSocket, state: Arc<DaemonState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Create channel for sending frames to the client
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Set the lock
    {
        let mut guard = state.ws_client.lock().await;
        *guard = Some(WsClientHandle {
            tx: client_tx.clone(),
        });
    }
    state.notify_lock_change.notify_waiters();
    info!("WS client connected (lock acquired)");

    // Open HID device — keys now route through daemon → app
    {
        let hid = state.hid.lock().await;
        if hid.is_device_available() && !hid.is_connected() {
            if let Err(e) = hid.open_device() {
                warn!("Failed to open HID device: {}", e);
            }
        }
    }

    // Send current device status directly from HidManager (avoids race with event handler)
    {
        let hid = state.hid.lock().await;
        if hid.is_connected() {
            let name = hid.cached_device_name().unwrap_or_default();
            let fw = hid.query_version();
            let info = DeviceInfo { name, firmware: fw };
            let payload = serde_json::to_vec(&info).unwrap_or_default();
            let frame = encode_ws_frame(WsEventTag::DeviceConnected as u8, 0, &payload);
            let _ = client_tx.send(frame);

            // Also send current state
            let status = state.device_status.read().await;
            let state_byte = coredeck_protocol::DeviceState {
                mode: status.mode,
                yolo: status.yolo,
            }
            .to_byte();
            let frame = encode_ws_frame(WsEventTag::StateChanged as u8, 0, &[state_byte]);
            let _ = client_tx.send(frame);
        }
    }

    // Spawn writer task
    let writer = tokio::spawn(async move {
        while let Some(frame) = client_rx.recv().await {
            if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
    });

    // Read loop — process incoming commands from the app
    while let Some(msg) = ws_rx.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                handle_ws_command(&data, &state, &client_tx).await;
            }
            Ok(Message::Close(_)) => break,
            Err(e) => {
                warn!("WS read error: {}", e);
                break;
            }
            _ => {} // Ignore text/ping/pong
        }
    }

    // Connection closed — release lock
    writer.abort();
    {
        let mut guard = state.ws_client.lock().await;
        *guard = None;
    }
    state.notify_lock_change.notify_waiters();
    info!("WS client disconnected (lock released)");
}

/// Process a single WS binary command from the app
async fn handle_ws_command(
    data: &[u8],
    state: &Arc<DaemonState>,
    reply_tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let (tag, seq, payload) = match decode_ws_frame(data) {
        Some(v) => v,
        None => return,
    };

    let cmd = match WsCommandTag::from_byte(tag) {
        Some(c) => c,
        None => {
            let err = encode_ws_frame(WsResponseTag::CommandError as u8, seq, b"unknown command");
            let _ = reply_tx.send(err);
            return;
        }
    };

    let hid = state.hid.lock().await;

    let result: Result<Option<Vec<u8>>, String> = match cmd {
        WsCommandTag::UpdateDisplay => {
            match serde_json::from_slice::<coredeck_protocol::DisplayUpdate>(payload) {
                Ok(update) => hid
                    .send_display_update(&update)
                    .map(|_| None)
                    .map_err(|e| e.to_string()),
                Err(e) => Err(format!("invalid JSON: {}", e)),
            }
        }
        WsCommandTag::Ping => {
            // Just ack — HID ping is handled internally by daemon
            Ok(None)
        }
        WsCommandTag::SetBrightness => {
            if payload.len() >= 2 {
                let level = payload[0];
                let save = payload[1] != 0;
                hid.set_brightness(level, save)
                    .map(|_| None)
                    .map_err(|e| e.to_string())
            } else {
                Err("invalid payload".to_string())
            }
        }
        WsCommandTag::SetSoftKey => {
            if payload.len() >= 3 {
                let index = payload[0];
                let key_type = SoftKeyType::from_byte(payload[1]).unwrap_or(SoftKeyType::Default);
                let save = payload[2] != 0;
                let data = &payload[3..];
                hid.set_soft_key(index, key_type, data, save)
                    .map(|_| None)
                    .map_err(|e| e.to_string())
            } else {
                Err("invalid payload".to_string())
            }
        }
        WsCommandTag::GetSoftKey => {
            if !payload.is_empty() {
                let index = payload[0];
                match hid.get_soft_key(index) {
                    Ok(config) => {
                        let mut resp = vec![config.index, config.key_type as u8];
                        resp.extend_from_slice(&config.data);
                        Ok(Some(encode_ws_frame(
                            WsResponseTag::SoftKeyResponse as u8,
                            seq,
                            &resp,
                        )))
                    }
                    Err(e) => Err(e.to_string()),
                }
            } else {
                Err("missing index".to_string())
            }
        }
        WsCommandTag::ResetSoftKeys => {
            match hid.reset_soft_keys() {
                Ok(configs) => {
                    // Return all 3 configs serialized
                    let mut resp = Vec::new();
                    for config in &configs {
                        resp.push(config.index);
                        resp.push(config.key_type as u8);
                        let data_len = config.data.len() as u8;
                        resp.push(data_len);
                        resp.extend_from_slice(&config.data);
                    }
                    Ok(Some(encode_ws_frame(
                        WsResponseTag::SoftKeyResponse as u8,
                        seq,
                        &resp,
                    )))
                }
                Err(e) => Err(e.to_string()),
            }
        }
        WsCommandTag::SetMode => {
            if !payload.is_empty() {
                let mode = DeviceMode::from_byte(payload[0]);
                hid.set_mode(mode).map(|_| None).map_err(|e| e.to_string())
            } else {
                Err("missing mode".to_string())
            }
        }
        WsCommandTag::Alert => {
            match serde_json::from_slice::<coredeck_protocol::AlertRequest>(payload) {
                Ok(req) => hid
                    .send_alert(req.tab, &req.session, &req.text, req.details.as_deref())
                    .map(|_| None)
                    .map_err(|e| e.to_string()),
                Err(e) => Err(format!("invalid JSON: {}", e)),
            }
        }
        WsCommandTag::GetVersion => {
            let version = hid.query_version();
            Ok(Some(encode_ws_frame(
                WsResponseTag::VersionResponse as u8,
                seq,
                version.as_bytes(),
            )))
        }
        WsCommandTag::ClearAlert => {
            if !payload.is_empty() {
                let tab = payload[0] as usize;
                hid.clear_alert(tab)
                    .map(|_| None)
                    .map_err(|e| e.to_string())
            } else {
                match serde_json::from_slice::<coredeck_protocol::ClearAlertRequest>(payload) {
                    Ok(req) => hid
                        .clear_alert(req.tab)
                        .map(|_| None)
                        .map_err(|e| e.to_string()),
                    Err(e) => Err(format!("invalid payload: {}", e)),
                }
            }
        }
        WsCommandTag::WrapperWrite => {
            // Doesn't touch HID — drop the lock so the wrapper task can
            // acquire other state.
            drop(hid);
            match serde_json::from_slice::<WrapperWriteRequest>(payload) {
                Ok(req) => crate::wrapper::write_to_target(state, &req.wrapper_id, req.bytes)
                    .await
                    .map(|_| None),
                Err(e) => Err(format!("invalid payload: {}", e)),
            }
        }
        WsCommandTag::SetActiveWrapper => {
            drop(hid);
            match serde_json::from_slice::<SetActiveWrapperRequest>(payload) {
                Ok(req) => crate::wrapper::set_active_wrapper(state, &req.wrapper_id)
                    .await
                    .map(|_| None),
                Err(e) => Err(format!("invalid payload: {}", e)),
            }
        }
    };

    match result {
        Ok(Some(response_frame)) => {
            let _ = reply_tx.send(response_frame);
        }
        Ok(None) => {
            // Send ACK
            let ack = encode_ws_frame(WsResponseTag::CommandAck as u8, seq, &[]);
            let _ = reply_tx.send(ack);
        }
        Err(e) => {
            let err = encode_ws_frame(WsResponseTag::CommandError as u8, seq, e.as_bytes());
            let _ = reply_tx.send(err);
        }
    }
}

/// Forward a daemon event to the connected WS client (if any).
///
/// Also handles auto-opening the device when it becomes available while an app is connected.
pub async fn forward_event_to_ws(state: &Arc<DaemonState>, event: &DaemonEvent) {
    // Auto-open device when it becomes available and an app is connected
    if let DaemonEvent::DeviceAvailable { .. } = event {
        let ws_connected = state.ws_client.lock().await.is_some();
        if ws_connected {
            let hid = state.hid.lock().await;
            if hid.is_device_available() && !hid.is_connected() {
                info!("Device became available while app connected — auto-opening");
                if let Err(e) = hid.open_device() {
                    warn!("Failed to auto-open HID device: {}", e);
                }
            }
        }
    }

    let guard = state.ws_client.lock().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => return,
    };

    let frame = match event {
        DaemonEvent::HidConnected {
            device_name,
            firmware_version,
        } => {
            let info = DeviceInfo {
                name: device_name.clone(),
                firmware: firmware_version.clone(),
            };
            let payload = serde_json::to_vec(&info).unwrap_or_default();
            encode_ws_frame(WsEventTag::DeviceConnected as u8, 0, &payload)
        }
        DaemonEvent::HidDisconnected => {
            encode_ws_frame(WsEventTag::DeviceDisconnected as u8, 0, &[])
        }
        DaemonEvent::DeviceStateChanged { mode, yolo } => {
            let state_byte = coredeck_protocol::DeviceState {
                mode: *mode,
                yolo: *yolo,
            }
            .to_byte();
            encode_ws_frame(WsEventTag::StateChanged as u8, 0, &[state_byte])
        }
        DaemonEvent::HidKeyEvent { keycode } => {
            let hi = (*keycode >> 8) as u8;
            let lo = (*keycode & 0xFF) as u8;
            encode_ws_frame(WsEventTag::KeyEvent as u8, 0, &[hi, lo])
        }
        DaemonEvent::HidTypeString { text, send_enter } => {
            let mut payload = vec![if *send_enter { 1u8 } else { 0u8 }];
            payload.extend_from_slice(text.as_bytes());
            encode_ws_frame(WsEventTag::TypeString as u8, 0, &payload)
        }
        // DeviceAvailable/DeviceUnavailable are handled above (auto-open) and
        // in the event handler (tray updates). No WS frame needed.
        DaemonEvent::DeviceAvailable { .. } | DaemonEvent::DeviceUnavailable => return,
    };

    let _ = client.tx.send(frame);
}
