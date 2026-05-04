#!/bin/bash
# Create a signed DMG for Core Deck distribution
# Run this after sign.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
DIST_DIR="$PROJECT_ROOT/dist"

APP_NAME="Core Deck"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
VERSION=$(grep '^version' "$PROJECT_ROOT/crates/coredeck/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')
DMG_NAME="CoreDeck-${VERSION}"
DMG_PATH="$DIST_DIR/$DMG_NAME.dmg"

# Signing identity (optional, for signed DMG)
SIGNING_IDENTITY="${SIGNING_IDENTITY:-}"

# Notarization credentials (optional)
TEAM_ID="${TEAM_ID:-}"
KEYCHAIN_PROFILE="${KEYCHAIN_PROFILE:-}"
APPLE_ID="${APPLE_ID:-}"
APP_PASSWORD="${APP_PASSWORD:-}"

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --identity IDENTITY      Code signing identity (optional)"
    echo "  --team-id TEAM_ID        Apple Developer Team ID"
    echo "  --keychain-profile NAME  Notarytool keychain profile"
    echo "  --skip-notarize          Skip DMG notarization"
    echo "  --output PATH            Custom output path for DMG"
}

SKIP_NOTARIZE=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --identity)
            SIGNING_IDENTITY="$2"
            shift 2
            ;;
        --team-id)
            TEAM_ID="$2"
            shift 2
            ;;
        --keychain-profile)
            KEYCHAIN_PROFILE="$2"
            shift 2
            ;;
        --skip-notarize)
            SKIP_NOTARIZE=true
            shift
            ;;
        --output)
            DMG_PATH="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
done

# Check if app bundle exists
if [ ! -d "$APP_BUNDLE" ]; then
    echo "Error: App bundle not found at $APP_BUNDLE"
    echo "Run bundle.sh and sign.sh first"
    exit 1
fi

echo "=== Creating DMG ==="
echo "Version: $VERSION"
echo "Output: $DMG_PATH"

# Create temporary directory for DMG contents
DMG_TEMP="$DIST_DIR/dmg-temp"
rm -rf "$DMG_TEMP"
mkdir -p "$DMG_TEMP"

# Copy app to temp directory
cp -R "$APP_BUNDLE" "$DMG_TEMP/"

# Create Applications symlink
ln -s /Applications "$DMG_TEMP/Applications"

# Create DMG
# Remove existing DMG if present
rm -f "$DMG_PATH"

# Create DMG with hdiutil
echo "Creating DMG..."
hdiutil create \
    -volname "$APP_NAME" \
    -srcfolder "$DMG_TEMP" \
    -ov \
    -format UDZO \
    -fs HFS+ \
    "$DMG_PATH"

# Clean up temp directory
rm -rf "$DMG_TEMP"

echo "DMG created: $DMG_PATH"

# Sign DMG if identity is provided
if [ -n "$SIGNING_IDENTITY" ]; then
    echo "Signing DMG..."
    codesign --force --sign "$SIGNING_IDENTITY" --timestamp "$DMG_PATH"
    codesign --verify --verbose "$DMG_PATH"
fi

# Notarize DMG
if [ "$SKIP_NOTARIZE" = true ]; then
    echo "Skipping notarization as requested"
elif [ -n "$KEYCHAIN_PROFILE" ] || [ -n "$APPLE_ID" ]; then
    echo "=== Notarizing DMG ==="

    if [ -n "$KEYCHAIN_PROFILE" ]; then
        xcrun notarytool submit "$DMG_PATH" \
            --keychain-profile "$KEYCHAIN_PROFILE" \
            --wait
    else
        xcrun notarytool submit "$DMG_PATH" \
            --apple-id "$APPLE_ID" \
            --team-id "$TEAM_ID" \
            --password "$APP_PASSWORD" \
            --wait
    fi

    # Staple the notarization ticket to the DMG
    echo "Stapling notarization ticket..."
    xcrun stapler staple "$DMG_PATH"
    xcrun stapler validate "$DMG_PATH"

    echo "=== DMG Notarized ==="
else
    echo "No notarization credentials provided, skipping notarization"
fi

echo ""
echo "=== Done ==="
echo "DMG ready for distribution: $DMG_PATH"
echo ""
echo "File size: $(du -h "$DMG_PATH" | cut -f1)"
