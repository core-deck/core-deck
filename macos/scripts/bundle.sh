#!/bin/bash
# Build and bundle Core Deck for macOS
# Creates a proper .app bundle ready for signing

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
MACOS_DIR="$PROJECT_ROOT/macos"

APP_NAME="Core Deck"
BUNDLE_ID="com.coredeck.CoreDeck"
EXECUTABLE_NAME="coredeck-daemon"

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

if [ "$LIPO_ONLY" = true ]; then
    echo "Combining existing binaries with lipo..."

    ARM_BINARY="$PROJECT_ROOT/target/aarch64-apple-darwin/$BUILD_TYPE/$EXECUTABLE_NAME"
    X86_BINARY="$PROJECT_ROOT/target/x86_64-apple-darwin/$BUILD_TYPE/$EXECUTABLE_NAME"

    if [ ! -f "$ARM_BINARY" ]; then
        echo "Error: ARM64 binary not found at $ARM_BINARY"
        exit 1
    fi
    if [ ! -f "$X86_BINARY" ]; then
        echo "Error: x86_64 binary not found at $X86_BINARY"
        exit 1
    fi

    mkdir -p "$TARGET_DIR"
    lipo -create "$ARM_BINARY" "$X86_BINARY" -output "$TARGET_DIR/$EXECUTABLE_NAME"
    echo "Universal binary created"

elif [ "$UNIVERSAL" = true ]; then
    echo "Building universal binary..."

    # Build for both architectures
    cargo build $BUILD_FLAGS --target aarch64-apple-darwin
    cargo build $BUILD_FLAGS --target x86_64-apple-darwin

    # Create universal binary with lipo
    mkdir -p "$TARGET_DIR"
    lipo -create \
        "$PROJECT_ROOT/target/aarch64-apple-darwin/$BUILD_TYPE/$EXECUTABLE_NAME" \
        "$PROJECT_ROOT/target/x86_64-apple-darwin/$BUILD_TYPE/$EXECUTABLE_NAME" \
        -output "$TARGET_DIR/$EXECUTABLE_NAME"

    echo "Universal binary created"
elif [ -n "$ARCH" ]; then
    echo "Building for architecture: $ARCH"
    cargo build $BUILD_FLAGS --target "${ARCH}-apple-darwin"
    TARGET_DIR="$PROJECT_ROOT/target/${ARCH}-apple-darwin/$BUILD_TYPE"
else
    echo "Building for native architecture..."
    cargo build $BUILD_FLAGS
fi

# Verify binary exists
if [ ! -f "$TARGET_DIR/$EXECUTABLE_NAME" ]; then
    echo "Error: Binary not found at $TARGET_DIR/$EXECUTABLE_NAME"
    exit 1
fi

echo "=== Creating App Bundle ==="

# Clean previous bundle
rm -rf "$APP_BUNDLE"

# Create bundle structure
mkdir -p "$APP_BUNDLE/Contents/MacOS"
mkdir -p "$APP_BUNDLE/Contents/Resources"

# Copy executable
cp "$TARGET_DIR/$EXECUTABLE_NAME" "$APP_BUNDLE/Contents/MacOS/"

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
