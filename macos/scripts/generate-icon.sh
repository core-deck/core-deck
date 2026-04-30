#!/bin/bash
# Generate macOS .icns icon from PNG
# Requires: sips (bundled with macOS) and iconutil (bundled with Xcode)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
SOURCE_PNG="${1:-$PROJECT_ROOT/crates/coredeck-daemon/assets/icons/CoreDeck.png}"
OUTPUT_DIR="$PROJECT_ROOT/macos"
ICONSET_DIR="$OUTPUT_DIR/AppIcon.iconset"

echo "Generating macOS icon from: $SOURCE_PNG"

# Check source file exists
if [ ! -f "$SOURCE_PNG" ]; then
    echo "Error: Source PNG not found: $SOURCE_PNG"
    exit 1
fi

# Check for required tools
if ! command -v sips &> /dev/null; then
    echo "Error: sips not found. This tool requires macOS."
    exit 1
fi

if ! command -v iconutil &> /dev/null; then
    echo "Error: iconutil not found. Install Xcode Command Line Tools."
    exit 1
fi

# Create iconset directory
rm -rf "$ICONSET_DIR"
mkdir -p "$ICONSET_DIR"

# Required icon sizes for macOS
# Format: size:filename
declare -a SIZES=(
    "16:icon_16x16.png"
    "32:icon_16x16@2x.png"
    "32:icon_32x32.png"
    "64:icon_32x32@2x.png"
    "128:icon_128x128.png"
    "256:icon_128x128@2x.png"
    "256:icon_256x256.png"
    "512:icon_256x256@2x.png"
    "512:icon_512x512.png"
    "1024:icon_512x512@2x.png"
)

# Generate each size
for entry in "${SIZES[@]}"; do
    size="${entry%%:*}"
    filename="${entry##*:}"
    output="$ICONSET_DIR/$filename"

    echo "  Generating ${size}x${size} -> $filename"
    sips -z "$size" "$size" "$SOURCE_PNG" --out "$output" >/dev/null 2>&1
done

# Convert iconset to icns
echo "Converting iconset to icns..."
iconutil -c icns "$ICONSET_DIR" -o "$OUTPUT_DIR/AppIcon.icns"

# Cleanup
rm -rf "$ICONSET_DIR"

echo "Icon generated: $OUTPUT_DIR/AppIcon.icns"
