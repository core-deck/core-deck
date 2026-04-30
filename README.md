# CoreDeck

Companion software for the Core Deck macropad — a hardware control surface for Claude Code. Connects the USB macropad's TFT display, soft keys, mode LEDs, and rotary encoder to your terminal sessions.

## Architecture

The system consists of two binaries:

- **`coredeck-daemon`** — Background service that owns the HID device, runs the tray icon, and serves the HTTP REST + WebSocket APIs on `127.0.0.1:19384`. The settings UI is browser-based, served by the daemon. This is the only binary needed to drive the device.
- **`coredeck-claude`** — Thin wrapper that runs `claude` under a PTY and registers the session with the daemon (via `/wrapper-ws`) so soft keys, the rotary encoder, and tab cycling drive the active Claude session.

Third-party tools can also integrate with the daemon directly via its REST API.

## Building

Requires Rust 1.75+ (stable).

```bash
# macOS: ensure Xcode CLI tools are installed
xcode-select --install

# Build everything
cargo build --workspace

# Release build (LTO, stripped)
cargo build --workspace --release
```

Output binaries: `target/release/coredeck-daemon` and `target/release/coredeck-claude`.

See [docs/Building.md](docs/Building.md) for Linux dependencies, individual crate builds, and detailed notes.

## Running

```bash
# Start the daemon
coredeck-daemon

# Install as launchd service for auto-start
coredeck-daemon install

# In a separate terminal, launch Claude under the wrapper
coredeck-claude
```

## Quick API Test

With the daemon running:

```bash
# Check device status
curl -s http://127.0.0.1:19384/api/status | jq

# Update the display
curl -X POST http://127.0.0.1:19384/api/display \
  -H 'Content-Type: application/json' \
  -d '{"session": "my-project", "task": "Building...", "tabs": [0, 2, 1], "active": 1}'

# Show an alert
curl -X POST http://127.0.0.1:19384/api/alert \
  -H 'Content-Type: application/json' \
  -d '{"tab": 0, "session": "my-project", "text": "Done!", "details": "All tests passed"}'
```

## Workspace Structure

```
crates/
  coredeck-protocol/   # Shared types & wire format (serde only)
  coredeck-daemon/     # Background daemon (HID, tray, axum server, hooks)
  coredeck-claude/     # `claude` PTY wrapper + daemon session registration
docs/                   # API documentation
```

## Documentation

- [Building from Source](docs/Building.md) — Prerequisites, build commands, workspace layout
- [Daemon API Overview](docs/API.md) — How the daemon works, access modes, quick examples
- [REST API Reference](docs/REST-API.md) — All HTTP endpoints with request/response schemas
- [WebSocket Protocol](docs/WebSocket-Protocol.md) — Binary WS protocol for real-time control
- [Protocol Limits](docs/Protocol-Limits.md) — Hard limits on text, tabs, brightness, payloads
- [Shared Types](docs/Types.md) — JSON schemas for all API types

## License

GPL-3.0-or-later
