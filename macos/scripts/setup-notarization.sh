#!/bin/bash
# One-time setup: Store notarization credentials in keychain
# This avoids passing credentials on every notarization

set -e

echo "=== Notarization Credentials Setup ==="
echo ""
echo "This will store your Apple ID credentials in the macOS keychain"
echo "for use with notarytool. You only need to do this once."
echo ""

# Prompt for values if not provided
if [ -z "$APPLE_ID" ]; then
    read -r -p "Apple ID (email): " APPLE_ID
fi

if [ -z "$TEAM_ID" ]; then
    read -r -p "Team ID: " TEAM_ID
fi

if [ -z "$APP_PASSWORD" ]; then
    echo "App-specific password (create at appleid.apple.com):"
    read -r -s APP_PASSWORD
    echo ""
fi

PROFILE_NAME="${1:-coredeck-notary}"

echo "Storing credentials with profile name: $PROFILE_NAME"

xcrun notarytool store-credentials "$PROFILE_NAME" \
    --apple-id "$APPLE_ID" \
    --team-id "$TEAM_ID" \
    --password "$APP_PASSWORD"

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Credentials stored in keychain with profile: $PROFILE_NAME"
echo ""
echo "Usage in other scripts:"
echo "  export KEYCHAIN_PROFILE='$PROFILE_NAME'"
echo "  ./sign.sh --keychain-profile '$PROFILE_NAME'"
echo ""
echo "Or add to your shell profile:"
echo "  echo 'export KEYCHAIN_PROFILE=\"$PROFILE_NAME\"' >> ~/.zshrc"
