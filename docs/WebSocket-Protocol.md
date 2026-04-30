# WebSocket Protocol Reference

## Connection

**Endpoint:** `ws://127.0.0.1:19384/ws`

The WebSocket connection uses an exclusive lock model: only one client may be connected at a time. Attempting to connect while another client holds the lock returns HTTP **409 Conflict** on the upgrade request.

### Connection Lifecycle

**On connect:**
1. The daemon acquires the exclusive lock for this client.
2. If the USB device is available but not open, the daemon opens the HID interface.
3. If the device is already connected, the daemon immediately sends:
   - `DeviceConnected` event (tag `0x80`) with device name and firmware version
   - `StateChanged` event (tag `0x82`) with current mode/yolo state

**On disconnect:**
1. The exclusive lock is released.
2. The HID device interface is closed (keys route back to the system).
3. HTTP mutating endpoints become available again.

## Binary Frame Format

All WebSocket messages use binary frames with a 3-byte header:

```
[tag: u8][seq_lo: u8][seq_hi: u8][payload...]
```

| Field | Size | Description |
|-------|------|-------------|
| `tag` | 1 byte | Message type identifier |
| `seq_lo` | 1 byte | Sequence number, low byte (little-endian) |
| `seq_hi` | 1 byte | Sequence number, high byte (little-endian) |
| `payload` | variable | Tag-specific data (may be empty) |

### Sequence Number Rules

- **Events** (Daemon → App, unsolicited): always `seq = 0`
- **Commands** (App → Daemon): must use `seq > 0` (u16, range 1–65535)
- **Responses** (Daemon → App, to a command): echo the `seq` from the original command

## Commands (App → Daemon)

Commands are sent by the client to control the device. Each command receives either a `CommandAck` (success with no data) or a tag-specific response, or a `CommandError` on failure.

### 0x01 — UpdateDisplay

Update the TFT display content.

**Payload:** JSON-encoded [DisplayUpdate](Types.md#displayupdate)

```json
{"session":"my-project","task":"Building...","tabs":[0,2,1],"active":1,"context_percent":42.5,"cost_usd":1.23,"model":"Opus"}
```

Optional fields (`context_percent`, `cost_usd`, `model`) are omitted when `null`. See [DisplayUpdate](Types.md#displayupdate).

**Response:** `CommandAck` (0x87)

### 0x02 — Ping

Keep-alive ping. The daemon handles HID pinging internally; this simply acknowledges the client is alive.

**Payload:** empty

**Response:** `CommandAck` (0x87)

### 0x03 — SetBrightness

Set display backlight brightness.

**Payload:** 2 bytes

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Brightness level (0-255) |
| 1 | 1 | Save to EEPROM (0=no, 1=yes) |

**Response:** `CommandAck` (0x87)

### 0x04 — SetSoftKey

Configure a soft key assignment.

**Payload:** 3+ bytes

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Key index (0-2) |
| 1 | 1 | Key type: 0=Default, 1=Keycode, 2=String, 3=Sequence |
| 2 | 1 | Save to EEPROM (0=no, 1=yes) |
| 3.. | variable | Key data (max 128 bytes) |

**Response:** `CommandAck` (0x87)

### 0x05 — GetSoftKey

Read the current configuration of a soft key.

**Payload:** 1 byte — key index (0-2)

**Response:** `SoftKeyResponse` (0x85)

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Key index |
| 1 | 1 | Key type |
| 2.. | variable | Key data |

### 0x06 — ResetSoftKeys

Reset all soft keys to their keymap defaults.

**Payload:** empty

**Response:** `SoftKeyResponse` (0x85) — contains all 3 key configs concatenated:

For each key (repeated 3 times):

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Key index |
| 1 | 1 | Key type |
| 2 | 1 | Data length |
| 3.. | variable | Key data |

### 0x07 — SetMode

Set the device operating mode (LED indicator).

**Payload:** 1 byte — mode value: 0=Default, 1=Accept, 2=Plan

**Response:** `CommandAck` (0x87)

### 0x08 — Alert

Show an alert overlay for a specific tab.

**Payload:** JSON-encoded [AlertRequest](Types.md#alertrequest)

```json
{"tab":0,"session":"my-project","text":"Done!","details":"All tests passed"}
```

**Response:** `CommandAck` (0x87)

### 0x09 — GetVersion

Query the firmware version string.

**Payload:** empty

**Response:** `VersionResponse` (0x86) — payload is the version string as UTF-8 bytes

### 0x0A — ClearAlert

Clear the alert for a specific tab.

**Payload:** 1 byte — tab index. Alternatively, JSON-encoded [ClearAlertRequest](Types.md#clearalertrequest).

**Response:** `CommandAck` (0x87)

## Events (Daemon → App)

Events are unsolicited messages from the daemon. They always use `seq = 0`.

### 0x80 — DeviceConnected

The HID device interface was opened and is communicating.

**Payload:** JSON-encoded [DeviceInfo](Types.md#deviceinfo)

```json
{"name":"Core Deck","firmware":"1.0.0"}
```

### 0x81 — DeviceDisconnected

The HID device interface was closed or lost.

**Payload:** empty

### 0x82 — StateChanged

The device mode or YOLO switch changed (user pressed the mode button or toggled the switch).

**Payload:** 1 byte — state byte

| Bit | Description |
|-----|-------------|
| 1:0 | Mode (0=Default, 1=Accept, 2=Plan) |
| 2 | YOLO (0=off, 1=on) |
| 7:3 | Reserved (0) |

### 0x83 — KeyEvent

A key was pressed on the device.

**Payload:** 2 bytes — QMK keycode (big-endian)

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | High byte of keycode |
| 1 | 1 | Low byte of keycode |

### 0x84 — TypeString

A soft key configured as String type was pressed.

**Payload:** 1+ bytes

| Offset | Size | Description |
|--------|------|-------------|
| 0 | 1 | Flags: 1=send Enter after string |
| 1.. | variable | UTF-8 string bytes |

### 0x85 — ClaudeHookEvent

Claude Code hook event forwarded from the daemon. The daemon receives hook events via HTTP (`POST /hooks/{event_type}`) and forwards them to the connected app over WebSocket.

**Payload:** JSON-encoded envelope

```json
{
  "event": "PreToolUse",
  "data": {
    "hook_event_name": "PreToolUse",
    "session_id": "abc-123",
    "permission_mode": "default",
    "tool_name": "Bash",
    "tool_input": {"command": "ls"}
  }
}
```

| Field | Type | Description |
|-------|------|-------------|
| `event` | string | Hook event type: `"PreToolUse"`, `"PostToolUse"`, `"PermissionRequest"`, `"Stop"`, `"Notification"`, `"statusline"` |
| `data` | object | Raw JSON payload from Claude Code (snake_case field names) |

**Statusline data** (event = `"statusline"`):

```json
{
  "event": "statusline",
  "data": {
    "context_window": {"used_percentage": 42.5},
    "cost": {"total_cost_usd": 1.23},
    "model": {"display_name": "Opus"}
  }
}
```

> **Note:** Tag `0x85` is shared with `SoftKeyResponse` — they are distinguished by context (events use `seq = 0`, responses echo the command's `seq > 0`).

### 0x89 — AppControl

Tray menu action directed at the app.

**Payload:** 1 byte — action

| Value | Action |
|-------|--------|
| 0x01 | ShowWindow |
| 0x02 | HideWindow |

## Responses (Daemon → App)

Responses echo the sequence number from the command they reply to.

### 0x85 — SoftKeyResponse

Response to `GetSoftKey` or `ResetSoftKeys`. See command descriptions above for payload format.

### 0x86 — VersionResponse

Response to `GetVersion`. Payload is the firmware version string as UTF-8 bytes.

### 0x87 — CommandAck

Generic success acknowledgement. Payload is empty.

### 0x88 — CommandError

Command failed. Payload is the error message as UTF-8 bytes.

## Tag Summary

| Tag | Hex | Direction | Name |
|-----|-----|-----------|------|
| 0x01 | `01` | App → Daemon | UpdateDisplay |
| 0x02 | `02` | App → Daemon | Ping |
| 0x03 | `03` | App → Daemon | SetBrightness |
| 0x04 | `04` | App → Daemon | SetSoftKey |
| 0x05 | `05` | App → Daemon | GetSoftKey |
| 0x06 | `06` | App → Daemon | ResetSoftKeys |
| 0x07 | `07` | App → Daemon | SetMode |
| 0x08 | `08` | App → Daemon | Alert |
| 0x09 | `09` | App → Daemon | GetVersion |
| 0x0A | `0A` | App → Daemon | ClearAlert |
| 0x80 | `80` | Daemon → App | DeviceConnected |
| 0x81 | `81` | Daemon → App | DeviceDisconnected |
| 0x82 | `82` | Daemon → App | StateChanged |
| 0x83 | `83` | Daemon → App | KeyEvent |
| 0x84 | `84` | Daemon → App | TypeString |
| 0x85 | `85` | Daemon → App | ClaudeHookEvent / SoftKeyResponse |
| 0x86 | `86` | Daemon → App | VersionResponse |
| 0x87 | `87` | Daemon → App | CommandAck |
| 0x88 | `88` | Daemon → App | CommandError |
| 0x89 | `89` | Daemon → App | AppControl |
