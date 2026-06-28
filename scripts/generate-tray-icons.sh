#!/bin/bash
# Generate macOS tray icons from CoreDeckTray.png
# Requires ImageMagick (magick command)

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# Icons live in the daemon crate; the old repo-root assets/icons path
# was stranded when the crate moved.
ICONS_DIR="$SCRIPT_DIR/../crates/coredeck/assets/icons"
SOURCE="$ICONS_DIR/CoreDeckTray.png"

if [ ! -f "$SOURCE" ]; then
    echo "Error: Source file not found: $SOURCE"
    exit 1
fi

if ! command -v magick &> /dev/null; then
    echo "Error: ImageMagick not found. Install with: brew install imagemagick"
    exit 1
fi

echo "Generating tray icons from $SOURCE..."

# Size for macOS retina menu bar (22pt @ 2x = 44px)
SIZE=44

# Disconnected state opacity (40%)
DISCONNECTED_OPACITY=0.4

# Dark menu bar icons (white)
echo "  -> tray_connected.png (white, for dark menu bar)"
magick "$SOURCE" -resize ${SIZE}x${SIZE} -filter Lanczos "$ICONS_DIR/tray_connected.png"

echo "  -> tray_disconnected.png (faded white, for dark menu bar)"
magick "$SOURCE" -resize ${SIZE}x${SIZE} -filter Lanczos \
    -channel A -evaluate Multiply $DISCONNECTED_OPACITY +channel \
    "$ICONS_DIR/tray_disconnected.png"

# Light menu bar icons (black/inverted)
echo "  -> tray_connected_light.png (black, for light menu bar)"
magick "$SOURCE" -resize ${SIZE}x${SIZE} -filter Lanczos -negate \
    "$ICONS_DIR/tray_connected_light.png"

echo "  -> tray_disconnected_light.png (faded black, for light menu bar)"
magick "$SOURCE" -resize ${SIZE}x${SIZE} -filter Lanczos -negate \
    -channel A -evaluate Multiply $DISCONNECTED_OPACITY +channel \
    "$ICONS_DIR/tray_disconnected_light.png"

echo ""
echo "Generated icons:"
ls -la "$ICONS_DIR"/tray_*.png
echo ""
magick identify "$ICONS_DIR"/tray_*.png
