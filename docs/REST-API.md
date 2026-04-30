# REST API Reference

Base URL: `http://127.0.0.1:19384` (configurable via `--listen`)

All endpoints accept and return JSON. CORS is fully open (any origin, method, and headers).

## Locking Semantics

- **Read-only endpoints** (`GET /api/status`) always work.
- **Mutating endpoints** check for the WebSocket exclusive lock:
  - If a WS client holds the lock: returns **409 Conflict** with `{"error": "device locked by WebSocket client"}`.
  - If no WS client is connected: the endpoint transiently opens the HID device, performs the operation, then closes it.
- If the device is not physically available, mutating endpoints return **503 Service Unavailable** with `{"error": "Device not available"}`.

## Endpoints

### GET /api/status

Returns current daemon and device state. Always available regardless of lock state.

**Response: 200 OK**

```json
{
  "device_available": true,
  "device_connected": true,
  "device_name": "Core Deck",
  "firmware_version": "1.0.0",
  "device_mode": "Default",
  "device_yolo": false,
  "ws_locked": false
}
```

See [DaemonStatus](Types.md#daemonstatus) for field descriptions.

**Example:**

```bash
curl -s http://127.0.0.1:19384/api/status
```

---

### POST /api/display

Update the TFT display content (session name, task text, tab states).

**Request body:** [DisplayUpdateRequest](Types.md#displayupdaterequest)

```json
{
  "session": "my-project",
  "task": "Reading files",
  "task2": "",
  "tabs": [0, 2, 1],
  "active": 1
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `session` | string | yes | Session/project name (max 128 bytes) |
| `task` | string | no | Current task description (max 128 bytes) |
| `task2` | string | no | Second task line (max 128 bytes) |
| `tabs` | array of u8 | no | Tab state values: 0=inactive, 1=started, 2=working (max 16 entries) |
| `active` | integer | no | Index into `tabs` for the active tab |

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Display updated |
| 409 | WebSocket client holds the lock |
| 500 | HID communication error |
| 503 | Device not available |

**Example:**

```bash
curl -X POST http://127.0.0.1:19384/api/display \
  -H 'Content-Type: application/json' \
  -d '{"session": "my-project", "task": "Building...", "tabs": [0, 2], "active": 1}'
```

---

### POST /api/alert

Show an alert overlay on the device display for a specific tab.

**Request body:** [AlertRequest](Types.md#alertrequest)

```json
{
  "tab": 0,
  "session": "my-project",
  "text": "Task complete",
  "details": "All 42 tests passed"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tab` | integer | yes | Tab index (0-15) |
| `session` | string | yes | Session name for this tab (max 128 bytes) |
| `text` | string | yes | Alert text (max 128 bytes). Empty string clears the alert. |
| `details` | string | no | Extended details shown on hold (max 128 bytes) |

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Alert set |
| 409 | WebSocket client holds the lock |
| 500 | HID communication error |
| 503 | Device not available |

**Example:**

```bash
curl -X POST http://127.0.0.1:19384/api/alert \
  -H 'Content-Type: application/json' \
  -d '{"tab": 0, "session": "my-project", "text": "Done!", "details": "Built in 3.2s"}'
```

---

### POST /api/alert/clear

Clear the alert for a specific tab.

**Request body:** [ClearAlertRequest](Types.md#clearalertrequest)

```json
{
  "tab": 0
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tab` | integer | yes | Tab index to clear (0-15) |

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Alert cleared |
| 409 | WebSocket client holds the lock |
| 500 | HID communication error |
| 503 | Device not available |

**Example:**

```bash
curl -X POST http://127.0.0.1:19384/api/alert/clear \
  -H 'Content-Type: application/json' \
  -d '{"tab": 0}'
```

---

### POST /api/brightness

Set the TFT display backlight brightness.

**Request body:** [BrightnessRequest](Types.md#brightnessrequest)

```json
{
  "level": 200,
  "save": true
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `level` | integer | yes | Brightness level (0-255) |
| `save` | boolean | no | Persist to EEPROM (default: false) |

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Brightness set |
| 409 | WebSocket client holds the lock |
| 500 | HID communication error |
| 503 | Device not available |

**Example:**

```bash
curl -X POST http://127.0.0.1:19384/api/brightness \
  -H 'Content-Type: application/json' \
  -d '{"level": 255, "save": true}'
```

---

### POST /api/mode

Set the device operating mode (changes the LED indicator color).

> **Note:** This endpoint only controls the LED mode indicator. The YOLO (auto-approve) state is read-only — it is controlled exclusively by the physical toggle switch on the device and cannot be set via the API.

**Request body:** [SetModeRequest](Types.md#setmoderequest)

```json
{
  "mode": "Accept"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `mode` | string | yes | One of `"Default"`, `"Accept"`, `"Plan"` |

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Mode set |
| 409 | WebSocket client holds the lock |
| 500 | HID communication error |
| 503 | Device not available |

**Example:**

```bash
curl -X POST http://127.0.0.1:19384/api/mode \
  -H 'Content-Type: application/json' \
  -d '{"mode": "Plan"}'
```

---

### GET /api/version

Query the firmware version string from the device.

> **Note:** Unlike other GET endpoints, this requires device communication and is subject to locking.

**Response: 200 OK**

```json
{
  "version": "1.0.0"
}
```

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Version returned |
| 409 | WebSocket client holds the lock |
| 503 | Device not available |

**Example:**

```bash
curl -s http://127.0.0.1:19384/api/version
```

---

---

### POST /hooks/{event_type}

Receives Claude Code hook events and statusline data. These endpoints are **not** subject to the WebSocket lock — they are called by Claude Code's hooks system, not by the app.

Hook events are stored in daemon state, forwarded to the connected app via WebSocket (`ClaudeHookEvent`, tag `0x85`), and may trigger side effects (e.g., YOLO auto-approve).

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `event_type` | One of: `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`, `Notification`, `statusline` |

**Request body:** Raw JSON from Claude Code (snake_case field names).

Hook event example:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "ba8fc727-...",
  "permission_mode": "default",
  "tool_name": "Bash",
  "tool_input": {"command": "ls"}
}
```

Statusline example:

```json
{
  "context_window": {"used_percentage": 42.5},
  "cost": {"total_cost_usd": 1.23},
  "model": {"display_name": "Opus"}
}
```

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Event processed |
| 400 | Malformed JSON |

**PermissionRequest with YOLO:** When the device YOLO switch is on, the daemon responds with an auto-approve payload:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": {"behavior": "allow"}
  }
}
```

> **Note:** Claude Code sends snake_case in request payloads but expects camelCase in the PermissionRequest response.

**Hook installation:** Run `coredeck-daemon hooks install` to write hook config to `~/.claude/settings.json`. Run `coredeck-daemon hooks uninstall` to remove it.

---

## Error Response Format

All error responses use the [ApiError](Types.md#apierror) format:

```json
{
  "error": "device locked by WebSocket client"
}
```
