# WebSocket Protocol Reference

## Connection

**Endpoint:** `ws://127.0.0.1:19384/ws`

The WebSocket connection uses an exclusive lock model: only one client may be connected at a time. Attempting to connect while another client holds the lock returns HTTP **409 Conflict** on the upgrade request.

> **Note:** This is the public WS API for third-party clients (status dashboards, automation tooling). The `coredeck-claude` wrapper uses a separate non-locking endpoint (`/wrapper-ws`) with a JSON message protocol — see `WrapperToDaemon` / `DaemonToWrapper` in `coredeck-protocol`. The two WS surfaces are independent.

### Connection Lifecycle

**On connect:**
1. The daemon acquires the exclusive lock for this client.
2. If the USB device is available but not open, the daemon opens the HID interface.
3. If the device is already connected, the daemon immediately sends:
   - `DeviceConnected` event (tag `0x80`) with device name and firmware version
   - `StateChanged` event (tag `0x82`) with current mode/yolo state

**On disconnect:**
1. The exclusive lock is released.
2. HTTP mutating endpoints become available again.

(The HID handle itself stays open for the daemon's lifetime — earlier
revisions cycled it per-connection, but that behavior was retired.)

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

- **Events** (Daemon → Client, unsolicited): always `seq = 0`
- **Commands** (Client → Daemon): must use `seq > 0` (u16, range 1–65535). The daemon **enforces** this — a command with `seq = 0` is rejected with `CommandError`.
- **Responses** (Daemon → Client, to a command): echo the `seq` from the original command

## Commands (Client → Daemon)

Commands are sent by the connected WS client to control the device. Each command receives either a `CommandAck` (success with no data), a tag-specific response, or a `CommandError` on failure.

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

**Payload:** 1 byte — mode value: 0=Default, 1=Accept, 2=Plan, 3=Auto

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

**Payload:** 1 byte — tab index. Payloads longer than one byte are parsed as JSON-encoded [ClearAlertRequest](Types.md#clearalertrequest) (`{"tab": 0}`).

**Response:** `CommandAck` (0x87)

### 0x0B — WrapperWrite

Push raw bytes into a `coredeck-claude` wrapper's PTY (mode toggles,
soft-key text). The bytes reach the `claude` process as if typed.

**Payload:** JSON-encoded [WrapperWriteRequest](Types.md#wrapperwriterequest)

```json
{"wrapper_id":"","bytes":[27,91,90]}
```

| Field | Type | Description |
|-------|------|-------------|
| `wrapper_id` | string | Target wrapper. Empty string targets the active wrapper. |
| `bytes` | array of u8 | Raw bytes to inject into the PTY. |

**Response:** `CommandAck` (0x87), or `CommandError` (0x88) when no wrapper matches.

### 0x0C — SetActiveWrapper

Set which wrapper the daemon considers "active" (the target for HID
input and the highlighted tab on the device).

**Payload:** JSON-encoded [SetActiveWrapperRequest](Types.md#setactivewrapperrequest)

```json
{"wrapper_id":"5b8c…"}
```

**Response:** `CommandAck` (0x87), or `CommandError` (0x88) when the wrapper is unknown.

## Events (Daemon → Client)

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
| 1:0 | Mode (0=Default, 1=Accept, 2=Plan, 3=Auto) |
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

### 0x8B — ClaudeHookEvent

> **Changed in 0.2:** this event moved from tag `0x85` to `0x8B`. The old value collided with the `SoftKeyResponse` response tag and relied on seq disambiguation.

Claude Code hook event broadcast by the daemon. The daemon receives hook events via HTTP (`POST /hooks/{event_type}`) and broadcasts them to the connected WS client. The daemon also drives the device display directly from internal state, so this event is informational — clients can mirror or inspect activity but don't need to handle it for the device to work.

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
| `event` | string | Hook event type: `"PreToolUse"`, `"PostToolUse"`, `"PermissionRequest"`, `"Stop"`, `"Notification"`, `"UserPromptSubmit"`, `"SessionStart"`, `"SessionEnd"`, `"PreCompact"`, `"TaskCreated"`, `"TaskCompleted"`, `"statusline"`, `"subagent-statusline"`, or the daemon-synthesized `"permission_prompt"` |
| `data` | object | Raw JSON payload from Claude Code (snake_case field names; `subagent-statusline` carries `tasks: [{id, name, status, label, tokenCount, ...}]`) |

**Daemon-synthesized `permission_prompt`** — unlike the rest, this envelope is built by the daemon (not a raw hook body) when a permission prompt is shown, enriched from the pending request:

```json
{
  "event": "permission_prompt",
  "data": {
    "session_id": "abc-123",
    "message": "Allow Bash?",
    "details": "ls -la"
  }
}
```

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


### 0x89 — AppControl (deprecated, never emitted)

Reserved tag from the previous "embedded GUI app" architecture; the daemon no longer emits this event. Documented for completeness because the tag value is still in `coredeck-protocol` enums. Clients can safely ignore tag `0x89`.

### 0x8A — WrapperTabList

Snapshot of all live `coredeck-claude` wrappers and their per-session
state. Re-emitted on every change: wrapper register/unregister, any hook
update, and active-tab changes — expect this to be the highest-volume
event on the connection while sessions are working.

**Payload:** JSON-encoded [WrapperTabList](Types.md#wrappertablist)

```json
{
  "tabs": [
    {"wrapper_id": "5b8c…", "cwd": "/Users/me/proj", "pid": 4242,
     "started_at_unix": 1760000000, "session_name": "my-project",
     "tab_state": 2, "context_percent": 42.5}
  ],
  "active_wrapper_id": "5b8c…"
}
```

Fields with `null`/default values are omitted from the serialized JSON —
see [WrapperTab](Types.md#wrappertab) for the full field list.

## Responses (Daemon → Client, replies to commands)

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
| 0x01 | `01` | Client → Daemon | UpdateDisplay |
| 0x02 | `02` | Client → Daemon | Ping |
| 0x03 | `03` | Client → Daemon | SetBrightness |
| 0x04 | `04` | Client → Daemon | SetSoftKey |
| 0x05 | `05` | Client → Daemon | GetSoftKey |
| 0x06 | `06` | Client → Daemon | ResetSoftKeys |
| 0x07 | `07` | Client → Daemon | SetMode |
| 0x08 | `08` | Client → Daemon | Alert |
| 0x09 | `09` | Client → Daemon | GetVersion |
| 0x0A | `0A` | Client → Daemon | ClearAlert |
| 0x0B | `0B` | Client → Daemon | WrapperWrite |
| 0x0C | `0C` | Client → Daemon | SetActiveWrapper |
| 0x80 | `80` | Daemon → Client | DeviceConnected |
| 0x81 | `81` | Daemon → Client | DeviceDisconnected |
| 0x82 | `82` | Daemon → Client | StateChanged |
| 0x83 | `83` | Daemon → Client | KeyEvent |
| 0x84 | `84` | Daemon → Client | TypeString |
| 0x85 | `85` | Daemon → Client | SoftKeyResponse |
| 0x86 | `86` | Daemon → Client | VersionResponse |
| 0x87 | `87` | Daemon → Client | CommandAck |
| 0x88 | `88` | Daemon → Client | CommandError |
| 0x89 | `89` | (deprecated) | AppControl — never emitted |
| 0x8A | `8A` | Daemon → Client | WrapperTabList |
| 0x8B | `8B` | Daemon → Client | ClaudeHookEvent (was 0x85 before 0.2) |
