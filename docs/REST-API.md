# REST API Reference

Base URL: `http://127.0.0.1:19384` (configurable via `--listen`)

All endpoints accept and return JSON.

**Browser access is restricted.** The API is unauthenticated but can inject
keystrokes into terminal sessions, so the daemon refuses browser-originated
cross-site requests: any request (HTTP or WebSocket upgrade) carrying an
`Origin` header that isn't `http://127.0.0.1:<port>`, `http://localhost:<port>`,
or `http://[::1]:<port>` for the daemon's own port gets **403 Forbidden**, and
no CORS headers are served. When bound to loopback (the default) the `Host`
header is validated the same way to block DNS rebinding. Native local clients
(curl, scripts, third-party tools) send no `Origin` header and are unaffected;
the daemon's own settings page is same-origin and unaffected.

## Locking Semantics

- **Read-only endpoints** that don't touch the device (`GET /api/status`, `GET /api/wrappers`, `GET /api/hooks/status`, `GET /api/soft-keys/presets`) always work.
- **Mutating endpoints** — and the GETs that query the device (`GET /api/version`, `GET /api/soft-keys`, `GET /api/theme`) — check for the WebSocket exclusive lock:
  - If a WS client holds the lock: returns **409 Conflict** with `{"error": "device locked by WebSocket client"}`.
  - If no WS client is connected: the endpoint uses the daemon's persistent HID handle (the daemon keeps the device open for its lifetime, reopening on demand if a recent disconnect left it closed).
- If the device is not physically available, device-touching endpoints return **503 Service Unavailable** with `{"error": "Device not available"}` — or, when the device is present but opening it fails, the underlying open-failure message in the same `{"error": ...}` shape.

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

If the device is open but the version query itself fails (send error,
bad response, firmware without the command), the endpoint still
returns **200** with `{"version": "unknown"}` — it never 500s.

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

An unrecognized `event_type` is not an error — the daemon logs it at
debug level and returns **200** with an empty body.

**PermissionRequest flow:** This is the one hook whose response carries a decision. Three outcomes are possible:

1. **Auto-approve.** Requires *all three* of: the device physically connected, the Auto-approve (YOLO) hardware switch on, and the requesting session's wrapper enrolled in auto-approve (per-wrapper opt-in — the wrapper focused when the switch flips on is auto-enrolled; others get an "Auto-approve this tab?" prompt first). When all hold, the daemon immediately responds:

   ```json
   {
     "hookSpecificOutput": {
       "hookEventName": "PermissionRequest",
       "decision": {"behavior": "allow"}
     }
   }
   ```

2. **Interactive decision.** Otherwise the daemon parks the HTTP response and shows the prompt on the device — **the POST blocks until the user decides, up to the daemon's internal 5-minute timeout**. Allow on the device returns the allow payload above; Deny returns:

   ```json
   {
     "hookSpecificOutput": {
       "hookEventName": "PermissionRequest",
       "decision": {"behavior": "deny", "message": "Rejected from CoreDeck device"}
     }
   }
   ```

3. **Passthrough.** An empty body `{}` is returned when the daemon has no decision to make — timeout, no device to show the prompt on (or another alert already live), the wrapper opted out of auto-approve, or a missing `session_id`. Claude Code then falls back to its own terminal prompt.

> **Note:** Claude Code sends snake_case in request payloads but expects camelCase in the PermissionRequest response.

**Hook installation:** Run `coredeck hooks install` to write hook config to `~/.claude/settings.json`. Run `coredeck hooks uninstall` to remove it. The same operations are also exposed over HTTP — see `/api/hooks/*` below.

---

### GET /api/hooks/status

Report whether Claude Code hooks are currently installed in `~/.claude/settings.json`.

**Response: 200 OK**

```json
{
  "installed": true
}
```

---

### POST /api/hooks/install

Install Claude Code hooks (equivalent to `coredeck hooks install`). Writes a curl-shim script to `~/.claude/coredeck-hook.sh` and a SessionStart correlation script to `~/.claude/coredeck-register.sh`, merges hook entries into `~/.claude/settings.json`, and sets the `statusLine`/`subagentStatusLine` keys. Existing user-defined hook entries for the same event are preserved; only blocks the daemon previously wrote get replaced.

**Response: 200 OK** with an empty body, or **500** with an [ApiError](Types.md#apierror) on failure.

---

### POST /api/hooks/uninstall

Remove the daemon's hook entries from `~/.claude/settings.json`. Symmetric with install — strips only entries the daemon owns and leaves user blocks alone.

**Response: 200 OK** with an empty body, or **500** with an [ApiError](Types.md#apierror) on failure.

---

### POST /wrapper/register

Bind a Claude `session_id` to a connected wrapper. Posted by the
SessionStart hook script (`~/.claude/coredeck-register.sh`), which
reads `$COREDECK_WRAPPER_ID` from its inherited environment (set by
`coredeck-claude`) and `session_id` from the hook's stdin payload.
This is the linchpin of session correlation — the daemon never has to
guess which wrapper a session belongs to. Not subject to the WS lock.

**Request body:** [WrapperRegisterSession](Types.md)

```json
{
  "wrapper_id": "01HX...",
  "session_id": "ba8fc727-..."
}
```

On success the daemon also pushes a `SessionBound` message to the
wrapper (so it can echo the binding back after a daemon restart) and
promotes the session to active on the device.

**Response codes:**

| Code | Condition |
|------|-----------|
| 200 | Bound (empty body) |
| 404 | No connected wrapper with that `wrapper_id` |

---

### GET /api/wrappers

Debug view of the connected wrappers. Returns a bare JSON **array** of
rows — *not* the [WrapperTabList](Types.md#wrappertablist) type (the
full hook-derived snapshot is only available as the `WrapperTabList`
event on the main WS).

**Response: 200 OK**

```json
[
  {
    "wrapper_id": "01HX...",
    "pid": 49321,
    "cwd": "/Users/vden/work/agentdeck/app",
    "started_at_unix": 1746360000,
    "session_id": "ba8fc727-...",
    "host_terminal_kind": "ITerm2",
    "active": true
  }
]
```

---

### GET /api/soft-keys

Read all three soft-key configurations from the device.

**Response: 200 OK** — a bare JSON array (no envelope)

```json
[
  {"index": 0, "key_type": "Default", "data": []},
  {"index": 1, "key_type": "Keycode", "data": [0, 40]},
  {"index": 2, "key_type": "String", "data": [1, 104, 105]}
]
```

Also returns **409** while a WS client holds the lock, **503** when no
device is available, and **500** if a key read fails.

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

**Response: 200 OK** — the post-reset configurations, same bare-array
shape as `GET /api/soft-keys`, so clients can refresh without a
follow-up GET.

---

### GET /api/theme

Dump the device's display-theme palette (10 HSV slots). Each slot
controls a region of the on-device display — session text, task
lines, tab indicators, alert frame, etc. See
[ThemePalette](Types.md#themepalette) for the slot table.

**Response: 200 OK**

```json
{
  "colors": [
    {"slot": 0, "hue": 0, "sat": 0, "val": 255},
    {"slot": 1, "hue": 170, "sat": 160, "val": 255},
    ...
  ]
}
```

---

### PUT /api/theme/{slot}

Set one theme slot (0–9). `save=false` updates the live frame only
(useful for previewing a color on the device without committing it);
`save=true` also persists to EEPROM. The settings page previews
edits locally in the browser and PUTs each dirty slot with
`save=true` when the user clicks "Save to device".

```json
{
  "hue": 30,
  "sat": 225,
  "val": 215,
  "save": false
}
```

**Response: 200 OK** — echoes the slot's new HSV as a single
[ThemeColor](Types.md#themecolor).

---

### POST /api/theme/reset

Reset the whole palette to firmware defaults and persist to EEPROM.

**Response: 200 OK** — same shape as `GET /api/theme`, reflecting
the defaults that were just applied.

---

### GET /api/soft-keys/presets

List soft-key preset bundles — both the hardcoded built-ins and the
user's saved presets. A preset names a full configuration of all three
keys.

**Response: 200 OK**

```json
{
  "builtin": [
    {"name": "Default",
     "description": "Esc+Esc (clear input), Ctrl+O (verbose), /model (models)",
     "keys": [{"key_type": "Sequence", "data": [41, 41]},
              {"key_type": "Keycode", "data": [1, 18]},
              {"key_type": "String", "data": [1, 47, 109, 111, 100, 101, 108]}]}
  ],
  "user": [
    {"name": "git workflow",
     "keys": [{"key_type": "String", "data": [1, 103, 115]},
              {"key_type": "String", "data": [1, 103, 100]},
              {"key_type": "Keycode", "data": [0, 40]}]}
  ]
}
```

---

### POST /api/soft-keys/presets/apply

Apply a named preset (built-in or user) to the device. EEPROM is
committed once on the last key write.

```json
{"name": "git workflow"}
```

**Response: 200 OK** on success; **404** when no preset has that name;
**409**/**503**/**500** with [ApiError](Types.md#apierror) for
lock/device/write failures.

---

### POST /api/soft-keys/presets/save

Save a named user preset. The body must carry the full three-key
configuration — the server does **not** capture the device's current
keys; read them via `GET /api/soft-keys` first (this is what the
settings page does).

```json
{
  "name": "git workflow",
  "description": "status / diff / enter",
  "keys": [
    {"key_type": "String", "data": [1, 103, 115]},
    {"key_type": "String", "data": [1, 103, 100]},
    {"key_type": "Keycode", "data": [0, 40]}
  ]
}
```

`description` is optional. Upserts by name.

**Response: 200 OK** on success; **400** with [ApiError](Types.md#apierror)
for an empty name or a name colliding with a built-in preset.

---

### DELETE /api/soft-keys/presets/{name}

Delete a saved user preset.

**Response: 200 OK** on success; **400** for built-in names (immutable);
**404** when the preset doesn't exist.

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
