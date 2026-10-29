#!/bin/bash
# Build "IQ Skimmer.app" for macOS.
#
# macOS only lets an app reach devices on the local network (the radio) after the user
# allows it, and it remembers that choice per app. A bundle with a fixed identifier and an
# NSLocalNetworkUsageDescription gets the standard "find devices on your local network"
# prompt on first launch; a bare binary run from a terminal can be blocked silently.
set -euo pipefail

BUNDLE_ID="io.iqskimmer.IQSkimmer"
APP_NAME="IQ Skimmer"

cd "$(dirname "$0")/.."
cargo build --release

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
APP="target/release/$APP_NAME.app"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/iqskimmer "$APP/Contents/MacOS/iqskimmer"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                  <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>           <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>            <string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key>            <string>iqskimmer</string>
    <key>CFBundlePackageType</key>           <string>APPL</string>
    <key>CFBundleShortVersionString</key>    <string>$VERSION</string>
    <key>CFBundleVersion</key>               <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>        <string>11.0</string>
    <key>LSApplicationCategoryType</key>     <string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key>       <true/>
    <key>NSLocalNetworkUsageDescription</key>
    <string>IQ Skimmer connects to your FlexRadio on the local network to receive I/Q data and tune slices.</string>
</dict>
</plist>
EOF

# Ad-hoc signature with a stable identifier (no Apple developer certificate needed).
codesign --force --sign - --identifier "$BUNDLE_ID" "$APP"

echo "Built $APP"
echo "Run it with:  open \"$APP\""
echo "Or install:   cp -R \"$APP\" /Applications/"
