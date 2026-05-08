# Linux Setup Guide

End-to-end install for the Core Deck daemon on Linux. Covers both the
prebuilt-tarball path and the from-source path. This file also ships
inside the release tarball as its README.

## 1. System Dependencies

The daemon needs libudev (USB hotplug) and libhidapi (HID I/O) at
runtime; the build adds `build-essential` + `pkg-config`.

### Debian / Ubuntu
```bash
sudo apt install libudev1 libhidapi-libusb0
# add build-essential pkg-config libudev-dev libhidapi-dev for source builds
```

### Fedora / RHEL
```bash
sudo dnf install systemd-libs hidapi
# add gcc pkg-config systemd-devel hidapi-devel for source builds
```

### Arch
```bash
sudo pacman -S hidapi
```

A graphical session (X11 or Wayland) is needed for the tray icon.

## 2. Install the Binaries

### Path A — Prebuilt tarball (recommended)

Download `coredeck-<version>-linux-x86_64.tar.gz` from
[Releases](https://github.com/core-deck/core-deck/releases), then:

```bash
tar -xzf coredeck-*-linux-x86_64.tar.gz
cd coredeck-*-linux-x86_64
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
