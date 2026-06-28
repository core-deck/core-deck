//! HID protocol definitions for Core Deck communication
//!
//! Protocol uses a chunked format with a 2-byte header:
//! - Byte 0: flags (START=0x80, END=0x40)
//! - Byte 1: command ID
//! - Bytes 2-31: payload (30 bytes per chunk)

// Re-export shared types from the protocol crate so existing code
// can continue to use `crate::hid::protocol::DeviceMode` etc.
#[cfg(test)]
pub use coredeck_protocol::DisplayUpdate;
pub use coredeck_protocol::{DeviceMode, DeviceState, SoftKeyConfig, SoftKeyType};

/// HID packet size in bytes
pub const PACKET_SIZE: usize = 32;

/// Header size (flags + command)
pub const HEADER_SIZE: usize = 2;

/// Maximum payload per chunk (packet size - header) — standalone mode
pub const MAX_PAYLOAD_SIZE: usize = PACKET_SIZE - HEADER_SIZE;

/// VIAL prefix byte prepended to every packet when VIAL mode is active
pub const VIAL_PREFIX: u8 = 0x80;

/// Protocol mode: standalone custom HID vs VIAL-wrapped
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProtocolMode {
    /// Raw custom HID: [flags, cmd, payload×30]
    Standalone = 0,
    /// VIAL-wrapped: [0x80, flags, cmd, payload×29]
    Vial = 1,
}

impl ProtocolMode {
    /// Maximum payload bytes per chunk in this mode
    pub fn max_payload_size(self) -> usize {
        match self {
            ProtocolMode::Standalone => MAX_PAYLOAD_SIZE, // 30
            ProtocolMode::Vial => MAX_PAYLOAD_SIZE - 1,   // 29
        }
    }

    pub fn from_byte(byte: u8) -> Self {
        match byte {
            1 => ProtocolMode::Vial,
            _ => ProtocolMode::Standalone,
        }
    }
}

/// Flag: this is the first packet of a message
pub const FLAG_START: u8 = 0x80;

/// Flag: this is the last packet of a message
pub const FLAG_END: u8 = 0x40;

/// HID commands supported by the Core Deck firmware
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HidCommand {
    /// Update display with JSON data
    UpdateDisplay = 0x01,
    /// Ping/Pong keep-alive
    Ping = 0x02,
    /// Set display brightness
    SetBrightness = 0x03,
    /// Set a soft key assignment
    SetSoftKey = 0x04,
    /// Get a soft key assignment
    GetSoftKey = 0x05,
    /// Reset all soft keys to defaults
    ResetSoftKeys = 0x06,
    /// Set device LED mode
    SetMode = 0x07,
    /// Show or clear an alert overlay on the device
    Alert = 0x08,
    /// Query firmware version (response: UTF-8 version string)
    GetVersion = 0x09,
    /// Signal host is disconnecting — firmware goes idle immediately
    Disconnect = 0x0A,
    /// Update one display-theme HSV slot (single packet, 5-byte payload)
    SetTheme = 0x0B,
    /// Read one slot or dump all slots (single-byte payload; 0xFF = all)
    GetTheme = 0x0C,
    /// Reset the whole palette to firmware defaults
    ResetTheme = 0x0D,
    /// Device state report (unsolicited from device)
    StateReport = 0x10,
    /// Type a string into the active terminal (unsolicited from device)
    TypeString = 0x11,
    /// Single key event (unsolicited from device)
    KeyEvent = 0x12,
    /// Error response from device
    Error = 0xFF,
}

impl HidCommand {
    /// Convert command to byte value
    pub fn as_byte(&self) -> u8 {
        *self as u8
    }

    /// Parse command from byte
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(HidCommand::UpdateDisplay),
            0x02 => Some(HidCommand::Ping),
            0x03 => Some(HidCommand::SetBrightness),
            0x04 => Some(HidCommand::SetSoftKey),
            0x05 => Some(HidCommand::GetSoftKey),
            0x06 => Some(HidCommand::ResetSoftKeys),
            0x07 => Some(HidCommand::SetMode),
            0x08 => Some(HidCommand::Alert),
            0x09 => Some(HidCommand::GetVersion),
            0x0A => Some(HidCommand::Disconnect),
            0x0B => Some(HidCommand::SetTheme),
            0x0C => Some(HidCommand::GetTheme),
            0x0D => Some(HidCommand::ResetTheme),
            0x10 => Some(HidCommand::StateReport),
            0x11 => Some(HidCommand::TypeString),
            0x12 => Some(HidCommand::KeyEvent),
            0xFF => Some(HidCommand::Error),
            _ => None,
        }
    }
}

/// A 32-byte HID packet for communication
#[derive(Debug, Clone)]
pub struct HidPacket {
    /// Raw packet data
    data: [u8; PACKET_SIZE],
}

impl Default for HidPacket {
    fn default() -> Self {
        Self::new()
    }
}

impl HidPacket {
    /// Create a new empty packet
    pub fn new() -> Self {
        Self {
            data: [0u8; PACKET_SIZE],
        }
    }

    /// Create a packet with flags and command set
    pub fn with_command(flags: u8, command: HidCommand) -> Self {
        let mut packet = Self::new();
        packet.data[0] = flags;
        packet.data[1] = command.as_byte();
        packet
    }

    /// Get the flags byte
    #[cfg(test)]
    pub fn flags(&self) -> u8 {
        self.data[0]
    }

    /// Check if this is the start of a message
    pub fn is_start(&self) -> bool {
        self.data[0] & FLAG_START != 0
    }

    /// Check if this is the end of a message
    pub fn is_end(&self) -> bool {
        self.data[0] & FLAG_END != 0
    }

    /// Get the command byte
    pub fn command_byte(&self) -> u8 {
        self.data[1]
    }

    /// Get the command as enum
    pub fn command(&self) -> Option<HidCommand> {
        HidCommand::from_byte(self.data[1])
    }

    /// Get the payload slice (bytes 2-31)
    pub fn payload(&self) -> &[u8] {
        &self.data[HEADER_SIZE..]
    }

    /// Set payload from bytes, truncating if necessary
    pub fn set_payload(&mut self, payload: &[u8]) {
        let len = payload.len().min(MAX_PAYLOAD_SIZE);
        self.data[HEADER_SIZE..HEADER_SIZE + len].copy_from_slice(&payload[..len]);
        // Zero remaining bytes
        for i in HEADER_SIZE + len..PACKET_SIZE {
            self.data[i] = 0;
        }
    }

    /// Get raw packet data for sending
    pub fn as_bytes(&self) -> &[u8; PACKET_SIZE] {
        &self.data
    }

    /// Create packet from raw bytes
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut packet = Self::new();
        let len = bytes.len().min(PACKET_SIZE);
        packet.data[..len].copy_from_slice(&bytes[..len]);
        packet
    }
}

/// Build chunked packets for a command with a payload.
///
/// Splits payload into chunks with correct START/END flags.
/// Chunk size depends on protocol mode: 30 bytes (standalone) or 29 bytes (VIAL).
/// Single-packet messages get flags START|END (0xC0).
pub fn build_chunked_packets(
    command: HidCommand,
    payload: &[u8],
    mode: ProtocolMode,
) -> Vec<HidPacket> {
    let chunk_size = mode.max_payload_size();

    if payload.is_empty() {
        // Single packet, no payload
        let packet = HidPacket::with_command(FLAG_START | FLAG_END, command);
        return vec![packet];
    }

    let chunks: Vec<&[u8]> = payload.chunks(chunk_size).collect();
    let last_idx = chunks.len() - 1;

    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut flags = 0u8;
            if i == 0 {
                flags |= FLAG_START;
            }
            if i == last_idx {
                flags |= FLAG_END;
            }
            let mut packet = HidPacket::with_command(flags, command);
            packet.set_payload(chunk);
            packet
        })
        .collect()
}

/// Parsed response from the device
#[derive(Debug, Clone)]
pub struct ResponsePacket {
    /// Status byte (first byte of reassembled payload)
    pub status: u8,
    /// Remaining data after status byte
    pub data: Vec<u8>,
}

/// Protocol error codes from firmware
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProtoError {
    /// Payload exceeded device buffer
    Overflow = 0x01,
    /// Received continuation without start
    BadSequence = 0x02,
    /// Unknown command ID
    UnknownCommand = 0x03,
}

impl ProtoError {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(ProtoError::Overflow),
            0x02 => Some(ProtoError::BadSequence),
            0x03 => Some(ProtoError::UnknownCommand),
            _ => None,
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ProtoError::Overflow => "payload overflow",
            ProtoError::BadSequence => "bad packet sequence",
            ProtoError::UnknownCommand => "unknown command",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_creation() {
        let packet = HidPacket::new();
        assert_eq!(packet.as_bytes().len(), PACKET_SIZE);
        assert!(packet.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn test_packet_with_command() {
        let packet = HidPacket::with_command(FLAG_START | FLAG_END, HidCommand::Ping);
        assert_eq!(packet.flags(), 0xC0);
        assert_eq!(packet.command(), Some(HidCommand::Ping));
        assert_eq!(packet.as_bytes()[0], 0xC0);
        assert_eq!(packet.as_bytes()[1], 0x02);
    }

    #[test]
    fn test_packet_flags() {
        let packet = HidPacket::with_command(FLAG_START, HidCommand::UpdateDisplay);
        assert!(packet.is_start());
        assert!(!packet.is_end());

        let packet = HidPacket::with_command(FLAG_END, HidCommand::UpdateDisplay);
        assert!(!packet.is_start());
        assert!(packet.is_end());

        let packet = HidPacket::with_command(FLAG_START | FLAG_END, HidCommand::UpdateDisplay);
        assert!(packet.is_start());
        assert!(packet.is_end());
    }

    #[test]
    fn test_packet_payload() {
        let mut packet = HidPacket::with_command(FLAG_START | FLAG_END, HidCommand::UpdateDisplay);
        let data = b"hello";
        packet.set_payload(data);
        assert_eq!(&packet.payload()[..5], b"hello");
        // Rest should be zero
        assert!(packet.payload()[5..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_build_chunked_single() {
        let packets = build_chunked_packets(HidCommand::Ping, &[], ProtocolMode::Standalone);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);
        assert_eq!(packets[0].command(), Some(HidCommand::Ping));
    }

    #[test]
    fn test_build_chunked_small_payload() {
        let payload = b"small";
        let packets =
            build_chunked_packets(HidCommand::UpdateDisplay, payload, ProtocolMode::Standalone);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);
        assert_eq!(&packets[0].payload()[..5], b"small");
    }

    #[test]
    fn test_build_chunked_multi_packet() {
        let payload = vec![0xAA; 70];
        let packets = build_chunked_packets(
            HidCommand::UpdateDisplay,
            &payload,
            ProtocolMode::Standalone,
        );
        assert_eq!(packets.len(), 3);
        assert!(packets[0].is_start());
        assert!(!packets[0].is_end());
        assert!(!packets[1].is_start());
        assert!(!packets[1].is_end());
        assert!(!packets[2].is_start());
        assert!(packets[2].is_end());
        for p in &packets {
            assert_eq!(p.command(), Some(HidCommand::UpdateDisplay));
        }
    }

    #[test]
    fn test_build_chunked_exact_boundary() {
        let payload = vec![0xBB; 30];
        let packets = build_chunked_packets(
            HidCommand::UpdateDisplay,
            &payload,
            ProtocolMode::Standalone,
        );
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);

        let payload = vec![0xBB; 60];
        let packets = build_chunked_packets(
            HidCommand::UpdateDisplay,
            &payload,
            ProtocolMode::Standalone,
        );
        assert_eq!(packets.len(), 2);
    }

    #[test]
    fn test_protocol_mode_payload_sizes() {
        assert_eq!(ProtocolMode::Standalone.max_payload_size(), 30);
        assert_eq!(ProtocolMode::Vial.max_payload_size(), 29);
    }

    #[test]
    fn test_protocol_mode_from_byte() {
        assert_eq!(ProtocolMode::from_byte(0), ProtocolMode::Standalone);
        assert_eq!(ProtocolMode::from_byte(1), ProtocolMode::Vial);
        assert_eq!(ProtocolMode::from_byte(99), ProtocolMode::Standalone);
    }

    #[test]
    fn test_build_chunked_vial_mode() {
        let payload = vec![0xAA; 29];
        let packets =
            build_chunked_packets(HidCommand::UpdateDisplay, &payload, ProtocolMode::Vial);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].flags(), FLAG_START | FLAG_END);

        let payload = vec![0xAA; 30];
        let packets =
            build_chunked_packets(HidCommand::UpdateDisplay, &payload, ProtocolMode::Vial);
        assert_eq!(packets.len(), 2);

        let payload = vec![0xAA; 58];
        let packets =
            build_chunked_packets(HidCommand::UpdateDisplay, &payload, ProtocolMode::Vial);
        assert_eq!(packets.len(), 2);

        let payload = vec![0xAA; 59];
        let packets =
            build_chunked_packets(HidCommand::UpdateDisplay, &payload, ProtocolMode::Vial);
        assert_eq!(packets.len(), 3);
    }

    #[test]
    fn test_display_update_json() {
        let update = DisplayUpdate {
            session: "my-project".to_string(),
            task: "Reading files".to_string(),
            task2: String::new(),
            tabs: vec![0, 1, 2],
            active: 1,
            context_percent: None,
            cost_usd: None,
            model: None,
        };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"session\":\"my-project\""));
        assert!(json.contains("\"task\":\"Reading files\""));
        assert!(json.contains("\"tabs\":[0,1,2]"));
        assert!(json.contains("\"active\":1"));
    }

    #[test]
    fn test_command_roundtrip() {
        let commands = [
            HidCommand::UpdateDisplay,
            HidCommand::Ping,
            HidCommand::SetBrightness,
            HidCommand::SetSoftKey,
            HidCommand::GetSoftKey,
            HidCommand::ResetSoftKeys,
            HidCommand::SetMode,
            HidCommand::Alert,
            HidCommand::GetVersion,
            HidCommand::StateReport,
            HidCommand::TypeString,
            HidCommand::KeyEvent,
            HidCommand::Error,
        ];
        for cmd in commands {
            assert_eq!(HidCommand::from_byte(cmd.as_byte()), Some(cmd));
        }
        assert_eq!(HidCommand::from_byte(0xFE), None);
    }

    #[test]
    fn test_device_state_from_byte() {
        let state = DeviceState::from_byte(0x00);
        assert_eq!(state.mode, DeviceMode::Default);
        assert!(!state.yolo);

        let state = DeviceState::from_byte(0x01);
        assert_eq!(state.mode, DeviceMode::Accept);
        assert!(!state.yolo);

        let state = DeviceState::from_byte(0x02);
        assert_eq!(state.mode, DeviceMode::Plan);
        assert!(!state.yolo);

        let state = DeviceState::from_byte(0x06);
        assert_eq!(state.mode, DeviceMode::Plan);
        assert!(state.yolo);

        let state = DeviceState::from_byte(0x05);
        assert_eq!(state.mode, DeviceMode::Accept);
        assert!(state.yolo);

        let state = DeviceState::from_byte(0x04);
        assert_eq!(state.mode, DeviceMode::Default);
        assert!(state.yolo);
    }

    #[test]
    fn test_proto_error() {
        assert_eq!(ProtoError::from_byte(0x01), Some(ProtoError::Overflow));
        assert_eq!(ProtoError::from_byte(0x02), Some(ProtoError::BadSequence));
        assert_eq!(
            ProtoError::from_byte(0x03),
            Some(ProtoError::UnknownCommand)
        );
        assert_eq!(ProtoError::from_byte(0x99), None);
    }

    #[test]
    fn test_soft_key_type() {
        assert_eq!(SoftKeyType::from_byte(0), Some(SoftKeyType::Default));
        assert_eq!(SoftKeyType::from_byte(1), Some(SoftKeyType::Keycode));
        assert_eq!(SoftKeyType::from_byte(2), Some(SoftKeyType::String));
        assert_eq!(SoftKeyType::from_byte(3), Some(SoftKeyType::Sequence));
        assert_eq!(SoftKeyType::from_byte(4), None);
    }
}
