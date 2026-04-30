# Building from Source

## Prerequisites

### Rust Toolchain

Rust 1.75 or later (stable). Install via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### System Dependencies

#### macOS

```bash
# Xcode command line tools (provides system frameworks: AppKit, CoreFoundation, IOKit)
xcode-select --install
```

No additional packages required. The macOS system frameworks (`cocoa`, `core-foundation`, `IOKit`) are accessed via Rust crate bindings.

The `hidapi` crate links against IOKit for USB HID access. No Homebrew packages needed.

#### Linux

```bash
# Debian/Ubuntu
sudo apt install build-essential pkg-config libudev-dev libhidapi-dev

# Fedora
sudo dnf install gcc pkg-config systemd-devel hidapi-devel
```

The daemon also draws the tray icon via `tray-icon`/`winit`, which on Linux needs an X11 or Wayland session at runtime but no extra build packages beyond what is listed above.

## Workspace Structure

The project is a Cargo workspace with 3 crates:

```
crates/
  coredeck-protocol/   # Shared types & wire format (serde only, no system deps)
  coredeck-daemon/     # Background daemon (HID, tray icon, axum server, hooks)
  coredeck-claude/     # `claude` PTY wrapper + daemon session registration
```

The default member is `coredeck-daemon`, so a bare `cargo build` builds the daemon.

## Build Commands

### Build everything

```bash
cargo build --workspace
```

### Build individual crates

```bash
# Daemon (default)
cargo build -p coredeck-daemon
# or just:
cargo build

# Claude wrapper
cargo build -p coredeck-claude

# Protocol crate only
cargo build -p coredeck-protocol
```

### Release build

```bash
cargo build --workspace --release
```

Release profile uses `opt-level = 3`, LTO, single codegen unit, and symbol stripping for minimal binary size.

Output binaries:

| Binary | Path |
|--------|------|
| `coredeck-daemon` | `target/release/coredeck-daemon` |
| `coredeck-claude` | `target/release/coredeck-claude` |

### Run

```bash
# Run the daemon
cargo run -p coredeck-daemon

# Run the daemon on a custom port
cargo run -p coredeck-daemon -- --listen 127.0.0.1:9000

# Run the Claude wrapper (daemon must be up)
cargo run -p coredeck-claude
```

### Tests

```bash
cargo test --workspace
```

## Notes

### Logging

Both binaries use `tracing` with `RUST_LOG` env filter:

```bash
RUST_LOG=debug cargo run -p coredeck-daemon
RUST_LOG=debug cargo run -p coredeck-claude
```
