# Linux Setup Guide

End-to-end install for the Core Deck daemon on Linux. Covers both the
prebuilt-tarball path and the from-source path. This file also ships
inside the release tarball as its README.

## 1. System Dependencies

The daemon needs libudev (USB hotplug) and libhidapi (HID I/O) at
runtime; the build adds `build-essential` + `pkg-config`.

### Debian / Ubuntu
```bash
sudo apt install libudev1 libhidapi-libusb0 libgtk-3-0 libxdo3 \
                 libayatana-appindicator3-1
# Source builds add: build-essential pkg-config libudev-dev libhidapi-dev
#                    libgtk-3-dev libxdo-dev libayatana-appindicator3-dev
```

### Fedora / RHEL
```bash
sudo dnf install systemd-libs hidapi gtk3 libxdo \
                 libayatana-appindicator3-gtk3
# Source builds add: gcc pkg-config systemd-devel hidapi-devel gtk3-devel
#                    libxdo-devel
```

### Arch
```bash
sudo pacman -S hidapi gtk3 xdotool libayatana-appindicator
```

`libgtk-3-0` is needed because the daemon's tray icon is backed by
`tray-icon` + GTK on Linux. `libxdo3` covers X11 window plumbing.
`libayatana-appindicator3-*` is the StatusNotifierItem backend that
makes the tray icon actually show up under KDE Plasma and GNOME (via
its AppIndicator extension). Ubuntu Desktop and other GNOME/GTK
desktops already have GTK and xdo; the appindicator runtime is
distro-dependent (Fedora KDE for instance does not preinstall it).

A graphical session (X11 or Wayland) is needed for the tray icon.

## 2. Install the Binaries

### Path A — Prebuilt tarball (recommended)

Two tarballs are published per release on
[Releases](https://github.com/core-deck/core-deck/releases):

- `coredeck-<version>-linux-x86_64.tar.gz` — Intel/AMD64
- `coredeck-<version>-linux-arm64.tar.gz` — aarch64 (Raspberry Pi
  64-bit, Apple Silicon under Asahi or a Linux VM, Ampere/AWS
  Graviton, etc.)

Pick the one matching your CPU (`uname -m` reports `x86_64` or
`aarch64`), then:

```bash
tar -xzf coredeck-*-linux-*.tar.gz
cd coredeck-*-linux-*
./install.sh
```

`install.sh` copies the binaries to `~/.local/bin/`, drops the udev
rule into `/etc/udev/rules.d/` (asks for sudo), reloads udev, and runs
`coredeck setup` for you. Skip ahead to **§5 Alias claude**.

### Path B — Build from source

See [Building.md](Building.md) for the full toolchain notes.

```bash
git clone https://github.com/core-deck/core-deck.git
cd core-deck
cargo build --workspace --release
install -m 0755 target/release/coredeck target/release/coredeck-claude ~/.local/bin/
```

Make sure `~/.local/bin` is on your `$PATH` — most distros do this
when the directory exists at login, but a fresh install may need a
re-login or an explicit `export PATH="$HOME/.local/bin:$PATH"` in
your shell rc.

## 3. udev Rules (Path B only)

By default Linux requires root to talk to a HID device. Add a udev
rule so the daemon (running as your user) can open it:

```bash
sudo tee /etc/udev/rules.d/99-coredeck.rules > /dev/null << 'EOF'
# Core Deck QMK Raw HID device — VID 0xFEED, PID 0x0803.
SUBSYSTEM=="hidraw", ATTRS{idVendor}=="feed", ATTRS{idProduct}=="0803", MODE="0666"
SUBSYSTEM=="usb",    ATTRS{idVendor}=="feed", ATTRS{idProduct}=="0803", MODE="0666"
EOF

sudo udevadm control --reload-rules
sudo udevadm trigger
```

Unplug and replug the device for the rules to take effect. If you've
flashed the firmware with custom VID/PID values, update the rules to
match.

## 4. Run `coredeck setup` (Path B only)

Registers the Claude Code hooks in `~/.claude/settings.json` and
installs a systemd user unit at `~/.config/systemd/user/coredeck.service`:

```bash
coredeck setup
```

The unit starts the daemon, journald handles its logs, and
`Restart=on-failure` brings it back if it crashes. It activates on
login (`WantedBy=default.target`).

## 5. Alias `claude`

Put this in your shell rc (`~/.zshrc`, `~/.bashrc`, …):

```bash
alias claude="coredeck-claude"
```

Then any new terminal that runs `claude` will run it under the
CoreDeck wrapper, which registers the session with the daemon over
WebSocket so the device can drive it.

## Day 2 — Managing the daemon

```bash
systemctl --user status coredeck.service       # is it running?
systemctl --user restart coredeck.service      # restart after upgrading binaries
journalctl --user -u coredeck.service -f       # tail logs
coredeck uninstall                             # disable + remove the systemd unit
coredeck hooks uninstall                       # remove Claude Code hook entries
```

Tray icon: provided by the `tray-icon` crate; needs an X11 or Wayland
graphical session. On GNOME ≥ 40 you may need the
[AppIndicator extension](https://extensions.gnome.org/extension/615/appindicator-support/)
to see status icons at all.

## F20 raise (window focusing) on Wayland

Tapping F20 on the device raises the active wrapper's terminal
window. The daemon picks a path automatically:

| Session                    | Mechanism                                |
|----------------------------|------------------------------------------|
| X11 (any DE)               | `wmctrl -ia $WINDOWID` / `wmctrl -x -a`  |
| KDE Plasma Wayland or X11  | KWin Scripting via `gdbus` (always works)|
| GNOME Wayland              | **No built-in support.** See below       |
| Sway / Hyprland / Wayfire  | **No built-in support yet.** See below   |

**KDE Plasma** is the friendly Wayland case — KWin exposes a Scripting
DBus interface that we drive via `gdbus` (which ships with glib2,
already pulled in by the tray). No extra packages needed.

**GNOME Wayland** deliberately doesn't expose window management to
clients (Mutter's security stance). Options if you're on GNOME
Wayland:
- Install the [Window Calls extension](https://github.com/ickyicky/window-calls)
  — re-exposes `Activate(windowId)` over DBus. CoreDeck doesn't talk
  to it yet, but the plumbing would slot in alongside the KWin path.
- Run an X11 session instead. Mutter still supports X11; `wmctrl`
  works there.

**Sway / Hyprland / Wayfire** each expose their own clean primitive
(`swaymsg`, `hyprctl`, `wlrctl`) — drop us an issue if you want one
wired up, the pattern is the same as the KWin adapter.

## Verification

```bash
# USB enumerated?
lsusb | grep -i feed

# hidraw nodes accessible without sudo?
ls -la /dev/hidraw*

# Daemon responding?
curl -s http://127.0.0.1:19384/api/status | jq .

# Tray menu shows the connected device + firmware version.
```

## Troubleshooting

### Device not found
- `lsusb | grep -i feed` — is the USB device enumerated?
- VID/PID matches your udev rules?
- udev rules reloaded after the file was created?

### Permission denied opening hidraw
- Verify the udev rule file at `/etc/udev/rules.d/99-coredeck.rules`.
- Re-run `sudo udevadm control --reload-rules && sudo udevadm trigger`.
- Some setups need a logout/login cycle for group changes to apply.

### `hidapi` initialization fails
- Ensure `libhidapi` (or `libhidapi-libusb0`) is installed.
- Check `journalctl --user -u coredeck.service` for the exact error.

### Tray icon missing on GNOME
- GNOME ≥ 40 hides legacy tray icons by default. Install the
  AppIndicator extension linked above.

### F20 doesn't raise the terminal
- On KDE Wayland or X11, check `journalctl --user -u coredeck.service`
  for the actual `gdbus` / `wmctrl` error (the daemon falls through
  several paths; the last one's message is what you'll see).
- On GNOME Wayland, raise isn't supported out of the box — see the
  "F20 raise" section above for the workaround.
- For non-tabbed cases (e.g. konsole with one window), KWin's
  `resourceClass` must match. Verify with `qdbus6 org.kde.KWin
  /KWin org.kde.KWin.activeWindow` or just `xprop` under X11.

### `coredeck setup` complains about `systemctl --user`
- Make sure your session is registered with logind:
  `loginctl show-user "$USER" --property=Linger`. If `Linger=no`
  and the daemon doesn't survive logout, run
  `sudo loginctl enable-linger "$USER"`.

## Notes for tarball users

The bundled `install.sh` is intentionally minimal — it installs to
`~/.local/bin/`, the udev rule into `/etc/udev/rules.d/`, then runs
`coredeck setup`. To uninstall, run `coredeck uninstall` (removes the
systemd unit) and `rm ~/.local/bin/coredeck{,-claude}` plus
`sudo rm /etc/udev/rules.d/99-coredeck.rules`.
