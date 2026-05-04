#!/bin/bash
# Build and bundle Core Deck for macOS
# Creates a proper .app bundle ready for signing

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
MACOS_DIR="$PROJECT_ROOT/macos"

APP_NAME="Core Deck"
BUNDLE_ID="com.coredeck.CoreDeck"
EXECUTABLE_NAME="coredeck"
WRAPPER_NAME="coredeck-claude"

# Parse arguments
BUILD_TYPE="release"
ARCH=""
UNIVERSAL=false
LIPO_ONLY=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --debug)
            BUILD_TYPE="debug"
            shift
            ;;
        --arch)
            ARCH="$2"
            shift 2
            ;;
        --universal)
            UNIVERSAL=true
            shift
            ;;
        --lipo-only)
            # Just combine existing binaries with lipo (for CI)
            LIPO_ONLY=true
            UNIVERSAL=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--debug] [--arch arm64|x86_64] [--universal] [--lipo-only]"
            echo ""
            echo "Options:"
            echo "  --debug       Build debug instead of release"
            echo "  --arch ARCH   Build for specific architecture (arm64 or x86_64)"
            echo "  --universal   Build universal binary (requires native libs for both archs)"
            echo "  --lipo-only   Skip build, just combine existing binaries with lipo (for CI)"
            exit 1
            ;;
    esac
done

# Determine output directory
if [ "$BUILD_TYPE" = "release" ]; then
    BUILD_FLAGS="--release"
    TARGET_DIR="$PROJECT_ROOT/target/release"
else
    BUILD_FLAGS=""
    TARGET_DIR="$PROJECT_ROOT/target/debug"
fi

# Build directory for the app bundle
DIST_DIR="$PROJECT_ROOT/dist"
APP_BUNDLE="$DIST_DIR/$APP_NAME.app"

echo "=== Building Core Deck ==="
echo "Build type: $BUILD_TYPE"
echo "Universal binary: $UNIVERSAL"

# Build the binary
cd "$PROJECT_ROOT"

lipo_binary() {
    local name="$1"
    local arm="$PROJECT_ROOT/target/aarch64-apple-darwin/$BUILD_TYPE/$name"
    local x86="$PROJECT_ROOT/target/x86_64-apple-darwin/$BUILD_TYPE/$name"
    if [ ! -f "$arm" ]; then
        echo "Error: ARM64 binary not found at $arm"
        exit 1
    fi
    if [ ! -f "$x86" ]; then
        echo "Error: x86_64 binary not found at $x86"
        exit 1
    fi
    mkdir -p "$TARGET_DIR"
    lipo -create "$arm" "$x86" -output "$TARGET_DIR/$name"
}

if [ "$LIPO_ONLY" = true ]; then
    echo "Combining existing binaries with lipo..."
    lipo_binary "$EXECUTABLE_NAME"
    lipo_binary "$WRAPPER_NAME"
    echo "Universal binaries created"

elif [ "$UNIVERSAL" = true ]; then
    echo "Building universal binaries..."

    cargo build $BUILD_FLAGS --workspace --target aarch64-apple-darwin
    cargo build $BUILD_FLAGS --workspace --target x86_64-apple-darwin

    lipo_binary "$EXECUTABLE_NAME"
    lipo_binary "$WRAPPER_NAME"

    echo "Universal binaries created"
elif [ -n "$ARCH" ]; then
    echo "Building for architecture: $ARCH"
    cargo build $BUILD_FLAGS --workspace --target "${ARCH}-apple-darwin"
    TARGET_DIR="$PROJECT_ROOT/target/${ARCH}-apple-darwin/$BUILD_TYPE"
else
    echo "Building for native architecture..."
    cargo build $BUILD_FLAGS --workspace
fi

# Verify both binaries exist
for binary in "$EXECUTABLE_NAME" "$WRAPPER_NAME"; do
    if [ ! -f "$TARGET_DIR/$binary" ]; then
        echo "Error: Binary not found at $TARGET_DIR/$binary"
        exit 1
    fi
done

echo "=== Creating App Bundle ==="

# Clean previous bundle
rm -rf "$APP_BUNDLE"

# Create bundle structure
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# Copy daemon and wrapper executables
cp "$TARGET_DIR/$EXECUTABLE_NAME" "$APP_BUNDLE/Contents/MacOS/"
cp "$TARGET_DIR/$WRAPPER_NAME" "$APP_BUNDLE/Contents/MacOS/"

# Copy Info.plist
cp "$MACOS_DIR/Info.plist" "$APP_BUNDLE/Contents/"

# Copy icon if it exists
if [ -f "$MACOS_DIR/AppIcon.icns" ]; then
    cp "$MACOS_DIR/AppIcon.icns" "$APP_BUNDLE/Contents/Resources/"
else
    echo "Warning: AppIcon.icns not found. Run generate-icon.sh first."
fi

# Create PkgInfo
echo -n "APPL????" > "$APP_BUNDLE/Contents/PkgInfo"

echo "=== Bundle Created ==="
echo "App bundle: $APP_BUNDLE"
echo ""
echo "Next steps:"
echo "  1. Generate icon:  ./macos/scripts/generate-icon.sh"
echo "  2. Sign the app:   ./macos/scripts/sign.sh"
echo "  3. Create DMG:     ./macos/scripts/create-dmg.sh"
