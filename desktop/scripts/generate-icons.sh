#!/bin/bash
# Icon generation script for InferenceHub Desktop
# Requires: ImageMagick (brew install imagemagick)
#
# IMPORTANT: every PNG is forced to 8-bit RGBA (`-depth 8 PNG32:`). magick renders SVGs at 16-bit
# by default, and a 16-bit icon makes Tauri panic at launch ("invalid icon: dimensions don't match
# the number of pixels supplied by the rgba argument" — it reads 2x the bytes), crashing the app.

set -e

ICON_DIR="src-tauri/icons"
SOURCE_SVG="$ICON_DIR/icon.svg"

# Check if ImageMagick is installed
if ! command -v magick &> /dev/null; then
    echo "ImageMagick not found. Install with: brew install imagemagick"
    exit 1
fi

echo "Generating icons from $SOURCE_SVG..."

# render <size> <output> — always 8-bit RGBA
render() { magick -background none "$SOURCE_SVG" -resize "$1x$1" -depth 8 "PNG32:$2"; }

# Generate PNG icons
render 32  "$ICON_DIR/32x32.png"
render 128 "$ICON_DIR/128x128.png"
render 256 "$ICON_DIR/128x128@2x.png"

# Generate macOS .icns
# Create iconset directory
ICONSET="$ICON_DIR/icon.iconset"
mkdir -p "$ICONSET"

render 16   "$ICONSET/icon_16x16.png"
render 32   "$ICONSET/icon_16x16@2x.png"
render 32   "$ICONSET/icon_32x32.png"
render 64   "$ICONSET/icon_32x32@2x.png"
render 128  "$ICONSET/icon_128x128.png"
render 256  "$ICONSET/icon_128x128@2x.png"
render 256  "$ICONSET/icon_256x256.png"
render 512  "$ICONSET/icon_256x256@2x.png"
render 512  "$ICONSET/icon_512x512.png"
render 1024 "$ICONSET/icon_512x512@2x.png"

# Convert to icns (macOS only)
if command -v iconutil &> /dev/null; then
    iconutil -c icns "$ICONSET" -o "$ICON_DIR/icon.icns"
    rm -rf "$ICONSET"
    echo "Generated icon.icns"
else
    echo "iconutil not found (not on macOS?), skipping .icns generation"
fi

# Generate Windows .ico (8-bit)
magick "$ICON_DIR/32x32.png" "$ICON_DIR/128x128.png" -depth 8 "$ICON_DIR/icon.ico"

echo "Done! Icons generated in $ICON_DIR/"
ls -la "$ICON_DIR/"
