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

Receives Claude Code hook events and statusline data. These endpoints are **not** subject to the WebSocket lock — they are called by Claude Code's hooks system, not by API consumers.

Hook events update per-session daemon state (which directly drives the device display), are broadcast to any connected WS client as `ClaudeHookEvent` (tag `0x85`), and may trigger side effects (e.g., YOLO auto-approve, raising the wrapper terminal on the active session).

**Path parameters:**

| Parameter | Description |
|-----------|-------------|
| `event_type` | One of: `PreToolUse`, `PostToolUse`, `PermissionRequest`, `Stop`, `Notification`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `PreCompact`, `TaskCreated`, `TaskCompleted`, `statusline`, `subagent-statusline` |

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

Subagent statusline example (one tick = the complete visible list of subagent rows for `session_id`):

```json
{
  "session_id": "ba8fc727-...",
  "columns": 80,
  "tasks": [
    {
      "id": "task-1",
      "name": "Edit",
      "status": "running",
      "label": "Edit: parser.rs",
      "tokenCount": 4231,
      "startTime": 1712598421000
    }
  ]
}
```

The daemon replaces the session's tracked subagent list wholesale on every tick and surfaces the first running row as the device's primary task line. The endpoint returns `200 OK` with an empty body so Claude Code uses default rendering for every row (its `subagentStatusLine` script reads stdout as JSON-line row overrides).

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

**Hook installation:** Run `coredeck hooks install` to write hook config to `~/.claude/settings.json`. Run `coredeck hooks uninstall` to remove it. The same operations are also exposed over HTTP — see `/api/hooks/*` below.

---

### GET /api/hooks/status

Report whether Claude Code hooks are currently installed in `~/.claude/settings.json`.

**Response: 200 OK**

```json
{
  "installed": true,
  "settings_path": "/Users/vden/.claude/settings.json"
}
```

---

### POST /api/hooks/install

Install Claude Code hooks (equivalent to `coredeck hooks install`). Writes a curl-shim script to `~/.claude/coredeck-hook.sh` and merges hook entries into `~/.claude/settings.json`. Existing user-defined hook entries for the same event are preserved; only blocks the daemon previously wrote get replaced.

**Response: 200 OK** with the new install status.

---

### POST /api/hooks/uninstall

Remove the daemon's hook entries from `~/.claude/settings.json`. Symmetric with install — strips only entries the daemon owns and leaves user blocks alone.

**Response: 200 OK** with the new install status.

---

### GET /api/wrappers

Report the daemon's current wrapper / tab list — same data the device uses to draw its tab indicator.

**Response: 200 OK** — see [WrapperTabList](Types.md#wrappertablist).

---

### GET /api/soft-keys

Read all three soft-key configurations from the device.

**Response: 200 OK**

```json
{
  "keys": [
    {"index": 0, "key_type": "Default", "data": []},
    {"index": 1, "key_type": "Keycode", "data": [0, 40]},
    {"index": 2, "key_type": "String", "data": [1, 104, 105]}
  ]
}
```

---

### PUT /api/soft-keys/{index}

Update a single soft key (index 0–2). Same payload semantics as the WebSocket `SetSoftKey` command but in JSON.

```json
{
  "key_type": "Keycode",
  "data": [0, 40],
  "save": true
}
```

---

### POST /api/soft-keys/reset

Reset all three soft keys to their keymap defaults.

---

### GET /api/soft-keys/presets

List saved soft-key preset bundles. Presets are user-defined named groupings of all three keys.

---

### POST /api/soft-keys/presets/apply

Apply a named preset to the device.

```json
{"name": "git workflow", "save": true}
```

---

### POST /api/soft-keys/presets/save

Save the current soft-key configuration as a named preset.

```json
{"name": "git workflow"}
```

---

### DELETE /api/soft-keys/presets/{name}

Delete a saved preset.

---

### GET / and GET /settings

Static settings page — single-file HTML/CSS/JS embedded in the daemon binary, opened from the tray menu's "Open Settings…" entry. Hosts the soft-key editor and other configuration UI.

---

## Error Response Format

All error responses use the [ApiError](Types.md#apierror) format:

```json
{
  "error": "device locked by WebSocket client"
}
```
