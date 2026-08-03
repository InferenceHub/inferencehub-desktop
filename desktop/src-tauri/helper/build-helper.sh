#!/usr/bin/env bash
# Build the ih-stt-helper binary: whisper.cpp (Metal, static) + the Swift wrapper.
#
# Single source of truth for local builds and both CI workflows (ci.yml gate +
# build-macos.yml release). Produces src-tauri/resources/ih-stt-helper, signed
# with the hardened runtime + audio-input entitlement (Tauri only signs the
# main app binary — nested helpers need their own signature, the v0.2.9 lesson).
#
# The whisper.cpp checkout/build lands in $WHISPER_BUILD_DIR (default:
# .whisper-build next to this script, gitignored) so CI can cache it keyed on
# WHISPER_VERSION.
set -euo pipefail

WHISPER_VERSION="v1.9.1"
# Pin the deployment target: CI runners track the newest macOS, and an
# unpinned build links Swift runtime dylibs that don't exist on older systems
# (v0.2.11 shipped a helper "built for macOS 26.0" that dyld-crashed on 15).
MACOS_TARGET="14.0"
export MACOSX_DEPLOYMENT_TARGET="$MACOS_TARGET"
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC_TAURI="$(dirname "$HERE")"
BUILD_ROOT="${WHISPER_BUILD_DIR:-$HERE/.whisper-build}"
WHISPER_DIR="$BUILD_ROOT/whisper.cpp"
OUT="$SRC_TAURI/resources/ih-stt-helper"

# --- 1. whisper.cpp static libs (Metal on Apple Silicon by default) ----------
if [ ! -f "$WHISPER_DIR/build/src/libwhisper.a" ]; then
    mkdir -p "$BUILD_ROOT"
    if [ ! -d "$WHISPER_DIR" ]; then
        git clone --depth 1 --branch "$WHISPER_VERSION" \
            https://github.com/ggml-org/whisper.cpp "$WHISPER_DIR"
    fi
    cmake -S "$WHISPER_DIR" -B "$WHISPER_DIR/build" \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_OSX_DEPLOYMENT_TARGET="$MACOS_TARGET" \
        -DBUILD_SHARED_LIBS=OFF \
        -DWHISPER_BUILD_EXAMPLES=OFF \
        -DWHISPER_BUILD_TESTS=OFF \
        -DGGML_METAL_EMBED_LIBRARY=ON
    cmake --build "$WHISPER_DIR/build" -j --config Release
fi

# Static archives live in a few subdirs depending on ggml's layout; collect them.
LIB_DIRS=$(find "$WHISPER_DIR/build" -name "*.a" -exec dirname {} \; | sort -u)
LIB_FLAGS=""
for d in $LIB_DIRS; do LIB_FLAGS="$LIB_FLAGS -L$d"; done
LIBS=$(find "$WHISPER_DIR/build" -name "*.a" -exec basename {} \; | sed -e 's/^lib//' -e 's/\.a$//' | sort -u | sed 's/^/-l/' | tr '\n' ' ')

# --- 2. Swift wrapper ---------------------------------------------------------
# Module-map header paths resolve relative to the module map file, so stage it
# next to whisper.h.
cp "$HERE/whisper.modulemap" "$WHISPER_DIR/include/module.modulemap"
mkdir -p "$SRC_TAURI/resources"
swiftc -O "$HERE/stt-helper.swift" \
    -target "arm64-apple-macos$MACOS_TARGET" \
    -I "$WHISPER_DIR/include" \
    -I "$WHISPER_DIR/ggml/include" \
    $LIB_FLAGS $LIBS -lc++ \
    -framework Metal -framework MetalKit -framework Accelerate \
    -framework AVFoundation -framework Foundation -framework CoreAudio \
    -framework ScreenCaptureKit -framework CoreMedia -framework CoreGraphics \
    -Xlinker -sectcreate -Xlinker __TEXT -Xlinker __info_plist \
    -Xlinker "$HERE/helper-Info.plist" \
    -o "$OUT"

# --- 3. Sign (hardened runtime + mic entitlement) ------------------------------
codesign --force --options runtime \
    --entitlements "$SRC_TAURI/Entitlements.plist" -s - "$OUT"
codesign --verify "$OUT"
echo "built + signed: $OUT"
