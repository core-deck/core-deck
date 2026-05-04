# CoreDeck

Companion software for the Core Deck macropad — a hardware control surface for Claude Code. Connects the USB macropad's TFT display, soft keys, mode LEDs, and rotary encoder to your terminal sessions.

## Architecture

The system consists of two binaries:

- **`coredeck`** — Background daemon that owns the HID device, runs the tray icon, and serves the HTTP REST + WebSocket APIs on `127.0.0.1:19384`. The settings UI is browser-based, served by the daemon. This is the only binary needed to drive the device.
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

Output binaries: `target/release/coredeck` and `target/release/coredeck-claude`.

See [docs/Building.md](docs/Building.md) for Linux dependencies, individual crate builds, and detailed notes.

## Install

The fastest path on macOS is via Homebrew:

```bash
brew install --cask core-deck/coredeck/coredeck
coredeck setup
```

`brew install` drops `Core Deck.app` into `/Applications` and exposes
both `coredeck` and `coredeck-claude` on your PATH. `coredeck setup`
installs the Claude Code hooks and registers the launchd auto-start
agent. After that, alias `claude` to the wrapper in your shell rc:

```bash
alias claude="coredeck-claude"
```

If you'd rather drive things by hand:

```bash
# One-time: register Claude Code hooks (writes ~/.claude/settings.json
# with HTTP hook entries pointing at the daemon, plus a tiny curl shim
# so claude doesn't error when the daemon isn't running).
coredeck hooks install

# Start the daemon
coredeck

# Install as launchd service for auto-start (macOS)
coredeck install

# In a separate terminal, launch Claude under the wrapper.
coredeck-claude
```

The wrapper sets `COREDECK_WRAPPER_ID` in claude's env; the SessionStart hook (installed by `hooks install`) correlates that wrapper to the live `session_id` so the daemon can route HID input and per-session state correctly.

## Remote Claude over SSH

Run claude on a remote dev box with the device still wired to your
laptop. The wrapper opens an interactive remote shell with hooks
plumbed back through an SSH reverse tunnel:

```bash
# One-time, from your laptop: install hooks on the remote box.
coredeck setup --remote user@dev-box

# Drop into a remote shell with COREDECK_WRAPPER_ID + COREDECK_DAEMON_URL
# pre-set and the tunnel up. Run claude (or `tmux new` then claude) from
# the prompt that appears.
coredeck-claude --ssh user@dev-box
```

Hooks fire from the remote claude back through the tunnel to the
daemon on your laptop, so soft keys, the rotary encoder, alerts, and
session state work the same as for local sessions. tmux is supported —
the wrapper propagates env into the running tmux server on connect, so
new windows and panes inherit the right ids.

Trust boundary is SSH itself; no tokens, no TLS. One wrapper per remote
host at a time (the tunnel mirrors the local daemon port and fails fast
on collision).

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
  coredeck/            # Background daemon (HID, tray, axum server, hooks)
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
- [Roadmap](docs/ROADMAP.md) — Shipped features and the open backlog

## License

GPL-3.0-or-later
