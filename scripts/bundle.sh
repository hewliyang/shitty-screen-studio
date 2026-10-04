#!/usr/bin/env bash
# Builds "Shitty Screen Studio.app" so macOS grants Screen Recording to the app, not the terminal.
set -euo pipefail
cd "$(dirname "$0")/.."

NAME="Shitty Screen Studio"
BUNDLE_ID="dev.shitty.screenstudio"
DEST="${DEST:-$HOME/Applications}"
APP="$DEST/$NAME.app"
VERSION="${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"

cargo build --release
mkdir -p "$APP/Contents/MacOS"
cp target/release/shitty-screen-studio "$APP/Contents/MacOS/shitty-screen-studio"

ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET" "$APP/Contents/Resources"
for size in 16 32 128 256 512; do
  swift scripts/emoji-icon.swift "$size" "$ICONSET/icon_${size}x${size}.png"
  swift scripts/emoji-icon.swift "$((size * 2))" "$ICONSET/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key><string>shitty-screen-studio</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Records your screen.</string>
  <key>NSMicrophoneUsageDescription</key><string>Records your voice with the screen.</string>
  <key>NSCameraUsageDescription</key><string>Records your camera as an overlay.</string>
</dict>
</plist>
PLIST

# A stable identity keeps the Screen Recording grant across rebuilds; ad-hoc signatures lose it.
IDENTITY="${SIGN_IDENTITY:-Shitty Screen Studio Dev}"
if security find-identity -p codesigning | grep -q "\"$IDENTITY\""; then
  codesign --force --deep --sign "$IDENTITY" --identifier "$BUNDLE_ID" "$APP"
else
  echo "warning: signing ad-hoc; macOS will ask for Screen Recording again after each rebuild" >&2
  codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$APP"
fi
echo "$APP"
