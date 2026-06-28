# Shared Type Reference

JSON schemas for all types used across the HTTP REST and WebSocket APIs. These types are defined in the `coredeck-protocol` crate.

## DaemonStatus

Returned by `GET /api/status`.

```json
{
  "daemon_version": "0.1.0",
  "device_available": true,
  "device_connected": true,
  "device_name": "Core Deck",
  "firmware_version": "1.0.0",
  "device_mode": "Default",
  "device_yolo": false,
  "ws_locked": false
}
```

| Field | Type | Description |
|-------|------|-------------|
| `daemon_version` | string | Daemon binary version (`CARGO_PKG_VERSION` at compile time). Empty string when missing from older daemons that predate the field |
| `device_available` | boolean | USB device is physically present (enumerated on the bus) |
| `device_connected` | boolean | HID interface is open and communicating |
| `device_name` | string \| null | Device product name (if available) |
| `firmware_version` | string \| null | Firmware version string (if connected) |
| `device_mode` | [DeviceMode](#devicemode) | Current operating mode |
| `device_yolo` | boolean | YOLO (auto-approve) hardware toggle state. Read-only — controlled exclusively by the physical switch on the device |
| `ws_locked` | boolean | Whether a WebSocket client holds the exclusive lock |

## DisplayUpdateRequest

Request body for `POST /api/display`.

```json
{
  "session": "my-project",
  "task": "Reading files",
  "task2": "",
  "tabs": [0, 2, 1],
  "active": 1
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `session` | string | required | Session/project name (max 128 bytes) |
| `task` | string | `""` | Current task description (max 128 bytes) |
| `task2` | string | `""` | Second task line, pre-split (max 128 bytes) |
| `tabs` | u8[] | `[]` | Tab state values (max 16 entries). See [tab states](#tab-states). |
| `active` | integer | `0` | Index into `tabs` for the active tab |

## DisplayUpdate

Used as the JSON payload for the WebSocket `UpdateDisplay` command (tag `0x01`). Same structure as `DisplayUpdateRequest` plus optional Claude Code statusline fields. Fields with `null`/absent values are omitted from the serialized JSON.

```json
{
  "session": "my-project",
  "task": "Reading files",
  "tabs": [0, 2, 1],
  "active": 1,
  "context_percent": 42.5,
  "cost_usd": 1.23,
  "model": "Opus"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `session` | string | Session/project name |
| `task` | string | Current task description |
| `task2` | string | Second task line (omitted from JSON when empty) |
| `tabs` | u8[] | Tab state values |
| `active` | integer | Active tab index |
| `context_percent` | float \| null | Context window usage percentage (from Claude Code statusline) |
| `cost_usd` | float \| null | Session cost in USD (from Claude Code statusline) |
| `model` | string \| null | Model display name (from Claude Code statusline) |

## AlertRequest

Request body for `POST /api/alert` and payload for WS `Alert` command (tag `0x08`).

```json
{
  "tab": 0,
  "session": "my-project",
  "text": "Task complete",
  "details": "All 42 tests passed"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `tab` | integer | Tab index (0-15) |
| `session` | string | Session name for this tab (max 128 bytes) |
| `text` | string | Alert text (max 128 bytes) |
| `details` | string \| null | Extended details shown on hold (max 128 bytes) |

## ClearAlertRequest

Request body for `POST /api/alert/clear`. (The WS `ClearAlert` command, tag `0x0A`, takes a raw 1-byte tab index, not this JSON shape.)

```json
{
  "tab": 0
}
```

| Field | Type | Description |
|-------|------|-------------|
| `tab` | integer | Tab index to clear (0-15) |

## BrightnessRequest

Request body for `POST /api/brightness`.

```json
{
  "level": 200,
  "save": true
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `level` | integer | required | Brightness level (0-255) |
| `save` | boolean | `false` | Persist setting to EEPROM |

## SetModeRequest

Request body for `POST /api/mode`.

```json
{
  "mode": "Accept"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `mode` | [DeviceMode](#devicemode) | Target mode |

## DeviceMode

String enum for the device operating mode.

| Value | Byte | Description |
|-------|------|-------------|
| `"Default"` | 0 | Normal operating mode |
| `"Accept"` | 1 | Accept/approve mode |
| `"Plan"` | 2 | Planning mode |

## DeviceState

Binary-encoded device state (used in WS `StateChanged` events).

Single byte with bit fields:

| Bits | Field | Values |
|------|-------|--------|
| 1:0 | mode | 0=Default, 1=Accept, 2=Plan |
| 2 | yolo | 0=off, 1=on (hardware switch, read-only) |
| 7:3 | reserved | 0 |

## DeviceInfo

JSON payload of the WS `DeviceConnected` event (tag `0x80`).

```json
{
  "name": "Core Deck",
  "firmware": "1.0.0"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Device product name |
| `firmware` | string | Firmware version string |

## SoftKeyType

Enum for soft key assignment types (used in WS `SetSoftKey`/`GetSoftKey` commands).

| Value | Name | Description |
|-------|------|-------------|
| 0 | Default | Use the keymap default action |
| 1 | Keycode | Single 16-bit QMK keycode |
| 2 | String | Type a string on press |
| 3 | Sequence | Tap a sequence of keycodes |

## SoftKeyConfig

Soft key configuration (used in WS `SoftKeyResponse`).

| Field | Type | Description |
|-------|------|-------------|
| `index` | u8 | Key index (0-2) |
| `key_type` | [SoftKeyType](#softkeytype) | Assignment type |
| `data` | u8[] | Type-specific data (max 128 bytes) |

## ThemeColor

One slot of the on-device display theme. HSV components match
Quantum Painter's color API on the firmware side (each 0–255).

| Field | Type | Description |
|-------|------|-------------|
| `slot` | u8 | Slot index (0–9), see [ThemePalette](#themepalette) |
| `hue` | u8 | Hue (0–255 maps to 0°–360°) |
| `sat` | u8 | Saturation (0 = grayscale, 255 = pure color) |
| `val` | u8 | Value / brightness (0 = black, 255 = full) |

## ThemePalette

Returned by `GET /api/theme` and `POST /api/theme/reset`. The 10
slots are:

| Slot | Name | Used for |
|------|------|----------|
| 0 | `Session` | Top line session name + alert overlay session header |
| 1 | `Task` | Task lines 2–3 |
| 2 | `TaskEmpty` | "No active task" placeholder |
| 3 | `Alert` | Alert frame, alert text, alert tab indicators |
| 4 | `CtxBar` | Context-window usage bar at the bottom |
| 5 | `Yolo` | YOLO diagonal hazard stripes |
| 6 | `TabActive` | Active tab circle |
| 7 | `TabInactive` | Inactive / loaded / working tab circles + overflow `>` |
| 8 | `SoftkeyLabel` | Softkey overlay label text + dot identifiers |
| 9 | `SoftkeySep` | Softkey overlay horizontal separators |

```json
{
  "colors": [
    {"slot": 0, "hue": 0, "sat": 0, "val": 255},
    {"slot": 1, "hue": 170, "sat": 160, "val": 255}
  ]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `colors` | [ThemeColor](#themecolor)[] | One entry per slot, in slot order |

## SetThemeRequest

Body for `PUT /api/theme/{slot}`.

| Field | Type | Description |
|-------|------|-------------|
| `hue` | u8 | New hue (0–255) |
| `sat` | u8 | New saturation (0–255) |
| `val` | u8 | New value (0–255) |
| `save` | boolean | When `true`, persist to EEPROM (slow — 100–500 ms) |

## ApiError

Error response returned by all REST endpoints on failure.

```json
{
  "error": "device locked by WebSocket client"
}
```

| Field | Type | Description |
|-------|------|-------------|
| `error` | string | Human-readable error message |

## Tab States

Constants for the tab state values used in `tabs` arrays:

| Value | Name | Description |
|-------|------|-------------|
| 0 | Inactive | Tab exists but no active process |
| 1 | Started | Process started, waiting |
| 2 | Working | Process actively running |

## WrapperTabList

Broadcast over the main WS as its own event (`WrapperTabList`, tag `0x8A`, seq=0) on every wrapper register/unregister, hook update, and active-tab change. Snapshot of every connected `coredeck-claude` wrapper, including the per-session metadata gathered from hooks.

> **Note:** `GET /api/wrappers` does **not** return this type — that endpoint returns a bare JSON array of debug rows (`{wrapper_id, pid, cwd, started_at_unix, session_id, host_terminal_kind, active}`). The WS event is the only source of the full hook-derived snapshot.

Fields whose value is `null` (and `subagent_count: 0` / `is_remote: false`) are **omitted** from the serialized JSON, not emitted as `null`.

```json
{
  "tabs": [
    {
      "wrapper_id": "01HX...",
      "session_id": "ba8fc727-...",
      "cwd": "/Users/vden/work/agentdeck/app",
      "pid": 49321,
      "started_at_unix": 1746360000,
      "session_name": "Coredeck Revive",
      "terminal_title": "Coredeck Revive",
      "model": "Sonnet",
      "current_tool": "Bash",
      "current_task": "Thinking…",
      "last_tool_summary": "rg foo",
      "permission_mode": "default",
      "tab_state": 2,
      "context_percent": 42.5,
      "cost_usd": 1.23
    }
  ],
  "active_wrapper_id": "01HX..."
}
```

| Field | Type | Description |
|-------|------|-------------|
| `tabs` | array of [WrapperTab](#wrappertab) | One entry per live wrapper, sorted by start time |
| `active_wrapper_id` | string \| null | Wrapper currently considered "focused" — target for HID input |

## WrapperTab

One row in the wrapper-tab snapshot. Most optional fields are `null` until the corresponding hook fires (e.g. `model` is unknown until the first statusline event).

| Field | Type | Description |
|-------|------|-------------|
| `wrapper_id` | string | Stable id assigned by the wrapper at startup |
| `session_id` | string \| null | Bound Claude session (set by `SessionStart` hook) |
| `cwd` | string | Working directory at wrapper start |
| `pid` | integer | Wrapper PID |
| `started_at_unix` | u64 | Wrapper start time (Unix seconds) |
| `session_name` | string \| null | Custom session name (`--name` flag or `/rename`) |
| `terminal_title` | string \| null | Most recent OSC 0/1/2 title sniffed from claude's PTY |
| `prompt_summary` | string \| null | Short summary of the most recent user prompt (kept for external consumers; no longer used as the device session label) |
| `current_todo` | string \| null | In-progress TodoWrite item, if any |
| `model` | string \| null | Model display name from the latest statusline |
| `current_tool` | string \| null | Most recent tool name from PreToolUse |
| `current_task` | string \| null | Headline activity ("Thinking…", or a TaskCreate subject) |
| `last_tool_summary` | string \| null | Most recent tool's no-prefix summary, used for the device's task2 line |
| `permission_mode` | string \| null | Latest hook-reported `permission_mode` |
| `tab_state` | integer | Firmware tab-state value: 0=Inactive, 1=Started, 2=Working |
| `context_percent` | float \| null | Context window usage percent |
| `cost_usd` | float \| null | Session cost in USD |
| `subagent_label` | string \| null | First in-flight subagent's label (with `(N)` prefix when more than one) |
| `subagent_count` | u32 | Number of subagent rows reported in the latest tick (omitted when 0) |
| `is_remote` | bool | True for `--ssh` wrappers; UI prefixes their labels with `↦` (omitted when false) |

## WrapperWriteRequest

Payload for the WS `WrapperWrite` command (tag `0x0B`). Injects raw bytes into a wrapper's PTY.

```json
{
  "wrapper_id": "",
  "bytes": [27, 91, 90]
}
```

| Field | Type | Description |
|-------|------|-------------|
| `wrapper_id` | string | Target wrapper id. Empty string targets the currently active wrapper |
| `bytes` | array of u8 | Raw bytes written to the wrapper's PTY master |

## SetActiveWrapperRequest

Payload for the WS `SetActiveWrapper` command (tag `0x0C`).

```json
{
  "wrapper_id": "01HX..."
}
```

| Field | Type | Description |
|-------|------|-------------|
| `wrapper_id` | string | Wrapper to mark active (HID input target) |
