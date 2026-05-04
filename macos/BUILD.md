# macOS Build & Distribution Guide

This directory contains everything needed to build, sign, and distribute Core Deck for macOS.

The macOS bundle ships two executables under `Contents/MacOS/`:

- `coredeck` — the daemon (HID, tray, HTTP/WS server, hooks).
  `CFBundleExecutable` points at this. `LSUIElement` is true so it
  runs as a pure menu-bar accessory with no Dock icon.
- `coredeck-claude` — the PTY wrapper. Users add it to their `PATH`
  (or alias `claude` to it) — see "Hooking up the wrapper" below.

Both binaries are signed inside-out with the hardened runtime
(`--options runtime`); the wrapper carries the same entitlements as
the daemon. Notarization runs against the outer bundle and staples
the ticket onto the `.app`.

## Prerequisites

1. **Xcode Command Line Tools**
   ```bash
   xcode-select --install
   ```

2. **Rust toolchains** (for universal binary)
   ```bash
   rustup target add aarch64-apple-darwin
   rustup target add x86_64-apple-darwin
   ```

3. **librsvg** (for icon generation)
   ```bash
   brew install librsvg
   ```

4. **Apple Developer Account** (for signing & notarization)
   - Enroll at https://developer.apple.com/programs/

## Directory Structure

```
macos/
├── Info.plist                 # App bundle metadata
├── entitlements.plist         # Entitlements for GitHub distribution
├── entitlements-appstore.plist # Entitlements for App Store
├── AppIcon.icns               # Generated app icon
├── BUILD.md                   # This file
└── scripts/
    ├── generate-icon.sh       # Create .icns from SVG
    ├── bundle.sh              # Build and create .app bundle
    ├── sign.sh                # Sign and notarize the app
    ├── create-dmg.sh          # Create distributable DMG
    └── setup-notarization.sh  # One-time credential setup
```

## Quick Start (After Developer Account Approval)

### 1. Generate App Icon

```bash
./macos/scripts/generate-icon.sh
# Or with a custom PNG:
./macos/scripts/generate-icon.sh path/to/your-icon.png
```

### 2. Build the App Bundle

```bash
# Native architecture (ARM or Intel based on your Mac)
./macos/scripts/bundle.sh

# For a specific architecture
./macos/scripts/bundle.sh --arch arm64
./macos/scripts/bundle.sh --arch x86_64
```

**Universal Binary Note:** Building a universal binary locally requires native C libraries
(cairo, freetype) for both architectures, which is complex to set up. The recommended
approach is to use GitHub Actions for universal builds - the workflow automatically
builds on separate runners (ARM and Intel) and combines them with `lipo`.

### 3. Set Up Notarization (One Time)

First, create an app-specific password:
1. Go to https://appleid.apple.com
2. Sign In → Security → App-Specific Passwords
3. Generate a password for "Core Deck Notarization"

Then store credentials:
```bash
./macos/scripts/setup-notarization.sh coredeck-notary
```

### 4. Sign and Notarize

```bash
# Find your signing identity
security find-identity -v -p codesigning

# Sign and notarize
./macos/scripts/sign.sh \
    --identity "Developer ID Application: Your Name (TEAM_ID)" \
    --keychain-profile coredeck-notary
```

### 5. Create DMG

```bash
./macos/scripts/create-dmg.sh \
    --identity "Developer ID Application: Your Name (TEAM_ID)" \
    --keychain-profile coredeck-notary
```

The final DMG will be at `dist/CoreDeck-X.Y.Z.dmg`

## Certificate Setup

### For GitHub/Website Distribution

In Apple Developer Portal (https://developer.apple.com/account/resources/certificates):

1. Create **Developer ID Application** certificate
2. Create **Developer ID Installer** certificate (optional, for pkg)
3. Download and install both certificates

### For App Store

1. Create **Mac App Distribution** certificate
2. Create **Mac Installer Distribution** certificate
3. Create an App ID with identifier `com.coredeck.CoreDeck`

## Environment Variables

You can set these to avoid passing arguments:

```bash
export SIGNING_IDENTITY="Developer ID Application: Your Name (ABC123)"
export TEAM_ID="ABC123"
export KEYCHAIN_PROFILE="coredeck-notary"
```

## Troubleshooting

### "errSecInternalComponent" during signing

Reset your keychain access:
```bash
security unlock-keychain ~/Library/Keychains/login.keychain-db
```

### Notarization fails with "Invalid signature"

Ensure you're using `--options runtime` (hardened runtime) during signing.

### "The signature is invalid" on another Mac

Make sure you stapled the notarization ticket:
```bash
xcrun stapler staple "dist/Core Deck.app"
```

### Gatekeeper still blocks the app

Check the detailed assessment:
```bash
spctl --assess --type execute -vvv "dist/Core Deck.app"
```

## Hooking up the wrapper

After dragging `Core Deck.app` to `/Applications`, expose
`coredeck-claude` on the user's `PATH` so it can replace the
`claude` command. The simplest path is a symlink:

```bash
sudo ln -sf "/Applications/Core Deck.app/Contents/MacOS/coredeck-claude" \
            /usr/local/bin/coredeck-claude

# Optional: alias claude to the wrapper
echo 'alias claude="coredeck-claude"' >> ~/.zshrc
```

The daemon is also accessible the same way (e.g. for `coredeck hooks
install` from the shell), but day-to-day it's launched by the bundle
or by launchd, not invoked manually.

## App Store

Direct App Store distribution is out of scope right now — the daemon
talks to a custom USB HID device and ships a CLI wrapper, neither of
which fit the sandbox-first model cleanly. The `entitlements-appstore.plist`
file is kept around in case that changes. Distribute via Developer ID
+ notarization (above) instead.

## Version Updates

When releasing a new version:

1. Update version in `crates/coredeck/Cargo.toml`
2. Update version in `macos/Info.plist` (CFBundleVersion and CFBundleShortVersionString)
3. Rebuild and re-sign
