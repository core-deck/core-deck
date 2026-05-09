//! HID command helpers
//!
//! Convenience functions for building common HID commands.
//! All builders return `Vec<HidPacket>` using the chunked protocol.

#![allow(dead_code)]

use super::protocol::{
    build_chunked_packets, DeviceMode, HidCommand, HidPacket, ProtocolMode, SoftKeyType,
};

/// Build a display update from a DisplayUpdate struct.
/// Serializes directly to JSON — all fields (including optional context/cost/model)
/// are included when present.
pub fn build_display_update(
    update: &coredeck_protocol::DisplayUpdate,
    mode: ProtocolMode,
) -> Vec<HidPacket> {
    let json = serde_json::to_string(update).expect("DisplayUpdate serialization");

    tracing::info!("HID display payload: {}", json);

    build_chunked_packets(HidCommand::UpdateDisplay, json.as_bytes(), mode)
}

/// Build a ping packet (single packet)
pub fn build_ping(mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::Ping, &[], mode)
}

/// Build a disconnect packet — tells firmware to go idle immediately
pub fn build_disconnect(mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::Disconnect, &[], mode)
}

/// Build a brightness control packet (single packet)
pub fn build_set_brightness(level: u8, save: bool, mode: ProtocolMode) -> Vec<HidPacket> {
    let payload = [level, if save { 0x01 } else { 0x00 }];
    build_chunked_packets(HidCommand::SetBrightness, &payload, mode)
}

/// Build a set soft key command (may be multi-packet for long string data)
pub fn build_set_soft_key(
    index: u8,
    key_type: SoftKeyType,
    data: &[u8],
    save: bool,
    mode: ProtocolMode,
) -> Vec<HidPacket> {
    let mut payload = vec![index, key_type as u8, if save { 0x01 } else { 0x00 }];
    payload.extend_from_slice(data);
    build_chunked_packets(HidCommand::SetSoftKey, &payload, mode)
}

/// Build a get soft key query (single packet)
pub fn build_get_soft_key(index: u8, mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::GetSoftKey, &[index], mode)
}

/// Build a reset soft keys command (single packet)
pub fn build_reset_soft_keys(mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::ResetSoftKeys, &[], mode)
}

/// Build a set mode command (single packet)
pub fn build_set_mode(device_mode: DeviceMode, mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::SetMode, &[device_mode as u8], mode)
}

/// Build an alert command to show an overlay on the device
pub fn build_alert(
    tab: usize,
    session: &str,
    text: &str,
    details: Option<&str>,
    mode: ProtocolMode,
) -> Vec<HidPacket> {
    let mut json = serde_json::json!({
        "tab": tab,
        "session": session,
        "text": text,
    });
    if let Some(d) = details {
        json["details"] = serde_json::Value::String(d.to_string());
    }
    tracing::info!("HID alert payload: {}", json);
    build_chunked_packets(HidCommand::Alert, json.to_string().as_bytes(), mode)
}

/// Build a get version query (single packet, no payload)
pub fn build_get_version(mode: ProtocolMode) -> Vec<HidPacket> {
    build_chunked_packets(HidCommand::GetVersion, &[], mode)
}

/// Build a clear alert command (no text field = clear)
pub fn build_clear_alert(tab: usize, mode: ProtocolMode) -> Vec<HidPacket> {
    let json = serde_json::json!({
        "tab": tab,
    });
    build_chunked_packets(HidCommand::Alert, json.to_string().as_bytes(), mode)
}

#[cfg(test)]
mod tests {
    use super::super::protocol::{ProtocolMode, FLAG_END, FLAG_START};
    use super::*;

    const S: ProtocolMode = ProtocolMode::Standalone;

    #[test]
    fn test_build_display_update() {
        let update = coredeck_protocol::DisplayUpdate {
            session: "my-session".to_string(),
            task: "Reading files".to_string(),
            task2: String::new(),
            tabs: vec![0, 1, 2],
            active: 1,
            context_percent: Some(42.0),
            cost_usd: Some(1.5),
            model: Some("Opus".to_string()),
        };
        let packets = build_display_update(&update, S);
        assert!(!packets.is_empty());
        assert!(packets[0].is_start());
        assert!(packets.last().unwrap().is_end());
        for p in &packets {
            assert_eq!(p.command(), Some(HidCommand::UpdateDisplay));
        }
    }

    #[test]
    fn test_build_display_update_no_task() {
        let update = coredeck_protocol::DisplayUpdate {
            session: "my-session".to_string(),
            task: String::new(),
            task2: String::new(),
            tabs: vec![1],
            active: 0,
            context_percent: None,
            cost_usd: None,
            model: None,
        };
        let packets = build_display_update(&update, S);
        assert!(!packets.is_empty());
        assert!(packets[0].is_start());
    }

    #[test]
    fn test_build_ping() {
        let packets = build_ping(S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);
        assert_eq!(packets[0].command(), Some(HidCommand::Ping));
    }

    #[test]
    fn test_build_set_brightness() {
        let packets = build_set_brightness(200, true, S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].command(), Some(HidCommand::SetBrightness));
        assert_eq!(packets[0].payload()[0], 200);
        assert_eq!(packets[0].payload()[1], 0x01);
    }

    #[test]
    fn test_build_set_soft_key() {
        let packets = build_set_soft_key(0, SoftKeyType::String, b"hello", true, S);
        assert!(!packets.is_empty());
        assert_eq!(packets[0].command(), Some(HidCommand::SetSoftKey));
        let payload = packets[0].payload();
        assert_eq!(payload[0], 0); // index
        assert_eq!(payload[1], 2); // SoftKeyType::String
        assert_eq!(payload[2], 1); // save
        assert_eq!(&payload[3..8], b"hello");
    }

    #[test]
    fn test_build_get_soft_key() {
        let packets = build_get_soft_key(2, S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].command(), Some(HidCommand::GetSoftKey));
        assert_eq!(packets[0].payload()[0], 2);
    }

    #[test]
    fn test_build_reset_soft_keys() {
        let packets = build_reset_soft_keys(S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].command(), Some(HidCommand::ResetSoftKeys));
    }

    #[test]
    fn test_build_set_mode() {
        let packets = build_set_mode(DeviceMode::Plan, S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].command(), Some(HidCommand::SetMode));
        assert_eq!(packets[0].payload()[0], 2); // Plan = 2
    }

    #[test]
    fn test_build_get_version() {
        let packets = build_get_version(S);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);
        assert_eq!(packets[0].command(), Some(HidCommand::GetVersion));
    }
}
