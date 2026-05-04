# Daemon API Overview

The CoreDeck daemon (`coredeck-daemon`) is a background service that owns the HID connection to the CoreDeck macropad. It exposes HTTP REST and WebSocket APIs for controlling the device display, LEDs, alerts, and soft keys.

For build instructions and project setup, see the [README](../README.md) and [Building from Source](Building.md).

## Access Modes

The daemon provides three ways to communicate:

| Mode | Endpoint | Exclusivity | Use Case |
|------|----------|-------------|----------|
| **HTTP REST** | `http://127.0.0.1:19384/api/*` | Shared (with caveat) | Simple one-shot commands, status checks |
| **WebSocket** | `ws://127.0.0.1:19384/ws` | Exclusive (one client) | Real-time bidirectional control |
| **Hook endpoints** | `http://127.0.0.1:19384/hooks/*` | Not locked | Claude Code hook events and statusline |

When a WebSocket client is connected, it holds an exclusive lock on the device. Mutating HTTP `/api/*` endpoints return `409 Conflict` while the lock is held. The `GET /api/status` endpoint and all `/hooks/*` endpoints always work regardless of lock state.

When no WebSocket client is connected, mutating HTTP endpoints transiently open the HID device for the duration of the request.

### Claude Code Hooks

The daemon receives structured events from Claude Code via HTTP hooks (`POST /hooks/{event_type}`). Hook data is stored in per-session daemon state and broadcast to any connected WebSocket client (`ClaudeHookEvent`, tag `0x85`). The daemon also drives the device display directly from this state — a connected WS client is optional, not required. The `PermissionRequest` hook supports YOLO auto-approve when the device hardware switch is on.

The wrapper (`coredeck-claude`) connects on a separate WS endpoint (`/wrapper-ws`) for byte-injection and focus reporting; that channel is independent of the main `/ws` API.

Install hooks: `coredeck-daemon hooks install` (writes `~/.claude/settings.json`)
Uninstall hooks: `coredeck-daemon hooks uninstall`

## Quick Examples

### Check device status

```bash
curl -s http://127.0.0.1:19384/api/status | jq
```

```json
{
  "device_available": true,
  "device_connected": false,
  "device_name": "Core Deck",
  "firmware_version": null,
  "device_mode": "Default",
  "device_yolo": false,
  "ws_locked": false
}
```

### Update the display

```bash
curl -X POST http://127.0.0.1:19384/api/display \
  -H 'Content-Type: application/json' \
  -d '{"session": "my-project", "task": "Building...", "tabs": [0, 2, 1], "active": 1}'
```

### Show an alert

```bash
curl -X POST http://127.0.0.1:19384/api/alert \
  -H 'Content-Type: application/json' \
  -d '{"tab": 0, "session": "my-project", "text": "Task complete", "details": "All tests passed"}'
```

### Set brightness

```bash
curl -X POST http://127.0.0.1:19384/api/brightness \
  -H 'Content-Type: application/json' \
  -d '{"level": 200, "save": true}'
```

### Set device mode

```bash
curl -X POST http://127.0.0.1:19384/api/mode \
  -H 'Content-Type: application/json' \
  -d '{"mode": "Accept"}'
```

## API Reference

- [REST API Reference](REST-API.md) — All HTTP endpoints with full request/response schemas (includes hook endpoints)
- [WebSocket Protocol](WebSocket-Protocol.md) — Binary WS protocol for real-time control (includes ClaudeHookEvent)
- [Protocol Limits](Protocol-Limits.md) — Hard limits on text, tabs, brightness, and payloads
- [Shared Types](Types.md) — JSON schemas for all API types
