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
sudo apt install build-essential pkg-config libudev-dev libhidapi-dev \
                 libgtk-3-dev libxdo-dev libayatana-appindicator3-dev

# Fedora
sudo dnf install gcc pkg-config systemd-devel hidapi-devel gtk3-devel \
                 libxdo-devel libayatana-appindicator3-gtk3
```

`libgtk-3-dev` / `gtk3-devel` is required because `tray-icon` pulls
in `atk-sys`, `gdk-pixbuf-sys`, and `glib-sys` on Linux; `libxdo-dev`
/ `libxdo-devel` is the X11 menu/window helper `muda` links against;
`libayatana-appindicator3-*` is the StatusNotifierItem backend that
makes the tray icon actually show up under GNOME / KDE Plasma.

Ubuntu Desktop boxes typically already have the runtime libs from a
default install; headless servers will need them at runtime too
(`libgtk-3-0 libxdo3 libayatana-appindicator3-1` /
`gtk3 libxdo libayatana-appindicator3-gtk3`).

The daemon also draws the tray icon via `tray-icon`/`winit`, which on Linux needs an X11 or Wayland session at runtime but no extra build packages beyond what is listed above.

After building, see [linux-setup.md](linux-setup.md) for the runtime
setup — udev rules, `coredeck setup` (systemd user unit + Claude
Code hooks), and the `claude` alias.

## Workspace Structure

The project is a Cargo workspace with 3 crates:

```
crates/
  coredeck-protocol/   # Shared types & wire format (serde only, no system deps)
  coredeck/            # Background daemon (HID, tray icon, axum server, hooks)
  coredeck-claude/     # `claude` PTY wrapper + daemon session registration
```

The default member is `coredeck`, so a bare `cargo build` builds the daemon.

## Build Commands

### Build everything

```bash
cargo build --workspace
```

### Build individual crates

```bash
# Daemon (default)
cargo build -p coredeck
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
| `coredeck` | `target/release/coredeck` |
| `coredeck-claude` | `target/release/coredeck-claude` |

### Run

```bash
# Run the daemon
cargo run -p coredeck

# Run the daemon on a custom port
cargo run -p coredeck -- --listen 127.0.0.1:9000

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
RUST_LOG=debug cargo run -p coredeck
RUST_LOG=debug cargo run -p coredeck-claude
```
