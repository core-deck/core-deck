#!/bin/bash
# Sign and notarize Core Deck for macOS distribution
# Run this after bundle.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
MACOS_DIR="$PROJECT_ROOT/macos"
DIST_DIR="$PROJECT_ROOT/dist"

APP_NAME="Core Deck"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"
ENTITLEMENTS="$MACOS_DIR/entitlements.plist"

# These should be set via environment variables or passed as arguments
# SIGNING_IDENTITY: "Developer ID Application: Your Name (TEAM_ID)"
# TEAM_ID: Your Apple Developer Team ID
# APPLE_ID: Your Apple ID email
# APP_PASSWORD: App-specific password for notarization

usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --identity IDENTITY    Code signing identity (required)"
    echo "  --team-id TEAM_ID      Apple Developer Team ID (required for notarization)"
    echo "  --apple-id EMAIL       Apple ID for notarization"
    echo "  --password PASSWORD    App-specific password for notarization"
    echo "  --keychain-profile     Use stored keychain profile instead of password"
    echo "  --skip-notarize        Skip notarization step"
    echo "  --appstore             Sign for App Store distribution"
    echo ""
    echo "Environment variables:"
    echo "  SIGNING_IDENTITY       Code signing identity"
    echo "  TEAM_ID                Apple Developer Team ID"
    echo "  APPLE_ID               Apple ID email"
    echo "  APP_PASSWORD           App-specific password"
    echo "  KEYCHAIN_PROFILE       Notarytool keychain profile name"
    echo ""
    echo "Example (GitHub distribution):"
    echo "  $0 --identity 'Developer ID Application: John Doe (ABC123)' \\"
    echo "     --team-id ABC123 --keychain-profile 'notary-profile'"
}

# Parse arguments
SKIP_NOTARIZE=false
APPSTORE=false
KEYCHAIN_PROFILE="${KEYCHAIN_PROFILE:-}"

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
        --apple-id)
            APPLE_ID="$2"
            shift 2
            ;;
        --password)
            APP_PASSWORD="$2"
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
        --appstore)
            APPSTORE=true
            ENTITLEMENTS="$MACOS_DIR/entitlements-appstore.plist"
            shift
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

# Validate required parameters
if [ -z "$SIGNING_IDENTITY" ]; then
    echo "Error: Signing identity is required"
    echo "Use --identity or set SIGNING_IDENTITY environment variable"
    usage
    exit 1
fi

# Check if app bundle exists
if [ ! -d "$APP_BUNDLE" ]; then
    echo "Error: App bundle not found at $APP_BUNDLE"
    echo "Run bundle.sh first"
    exit 1
fi

echo "=== Signing Core Deck ==="
echo "Identity: $SIGNING_IDENTITY"
echo "Entitlements: $ENTITLEMENTS"
echo "App Store: $APPSTORE"

# Sign nested binaries first (inside-out)
echo "Signing wrapper (coredeck-claude)..."
codesign --force --options runtime \
    --sign "$SIGNING_IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --timestamp \
    "$APP_BUNDLE/Contents/MacOS/coredeck-claude"

echo "Signing daemon (coredeck)..."
codesign --force --options runtime \
    --sign "$SIGNING_IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --timestamp \
    "$APP_BUNDLE/Contents/MacOS/coredeck"

# Sign the entire app bundle (entitlements applied to the main executable
# matching CFBundleExecutable; nested binaries keep theirs from above).
echo "Signing app bundle..."
codesign --force --options runtime \
    --sign "$SIGNING_IDENTITY" \
    --entitlements "$ENTITLEMENTS" \
    --timestamp \
    "$APP_BUNDLE"

# Verify signature
echo "Verifying signature..."
codesign --verify --deep --strict --verbose=2 "$APP_BUNDLE"

# Check Gatekeeper acceptance
echo "Checking Gatekeeper..."
spctl --assess --type execute --verbose "$APP_BUNDLE" || {
    echo "Warning: Gatekeeper assessment failed (expected before notarization)"
}

echo "=== Signing Complete ==="

# Notarization (only for non-App Store distribution)
if [ "$SKIP_NOTARIZE" = true ]; then
    echo "Skipping notarization as requested"
    exit 0
fi

if [ "$APPSTORE" = true ]; then
    echo "App Store builds don't need notarization"
    echo "Use Xcode or Transporter to upload to App Store Connect"
    exit 0
fi

# Check notarization requirements
if [ -z "$TEAM_ID" ]; then
    echo "Warning: Team ID not provided, skipping notarization"
    echo "Use --team-id or set TEAM_ID to enable notarization"
    exit 0
fi

if [ -z "$KEYCHAIN_PROFILE" ] && [ -z "$APPLE_ID" ]; then
    echo "Warning: No notarization credentials provided"
    echo "Use --keychain-profile or --apple-id/--password"
    exit 0
fi

echo "=== Notarizing App ==="

# Create a zip for notarization
ZIP_PATH="$DIST_DIR/CoreDeck-notarize.zip"
ditto -c -k --keepParent "$APP_BUNDLE" "$ZIP_PATH"

# Submit for notarization
echo "Submitting to Apple for notarization..."

if [ -n "$KEYCHAIN_PROFILE" ]; then
    xcrun notarytool submit "$ZIP_PATH" \
        --keychain-profile "$KEYCHAIN_PROFILE" \
        --wait
else
    xcrun notarytool submit "$ZIP_PATH" \
        --apple-id "$APPLE_ID" \
        --team-id "$TEAM_ID" \
        --password "$APP_PASSWORD" \
        --wait
fi

# Staple the notarization ticket
echo "Stapling notarization ticket..."
xcrun stapler staple "$APP_BUNDLE"

# Verify stapling
xcrun stapler validate "$APP_BUNDLE"

# Clean up zip
rm -f "$ZIP_PATH"

echo "=== Notarization Complete ==="
echo "App is signed and notarized: $APP_BUNDLE"

# Final Gatekeeper check
echo "Final Gatekeeper assessment..."
spctl --assess --type execute --verbose "$APP_BUNDLE"

echo ""
echo "Next step: Create DMG with ./macos/scripts/create-dmg.sh"
