#!/bin/sh
# Builds the release binary and assembles an ad-hoc signed Talus.app (plus a zip and the talus-server binary) in release/.
set -eu
cd "$(dirname "$0")/.."
cargo build --release -p talus-app -p talus-server
APP=release/Talus.app
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rm -rf "$APP"
mkdir -p release "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/talus "$APP/Contents/MacOS/Talus"
cp assets/Talus.icns "$APP/Contents/Resources/Talus.icns"
# The binary contains third-party code whose licenses require their texts to ship with it.
cp LICENSE "$APP/Contents/Resources/LICENSE"
cp THIRD_PARTY_LICENSES.md "$APP/Contents/Resources/THIRD_PARTY_LICENSES.md"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Talus</string>
  <key>CFBundleDisplayName</key><string>Talus</string>
  <key>CFBundleIdentifier</key><string>io.github.fallowlone.Talus</string>
  <key>CFBundleExecutable</key><string>Talus</string>
  <key>CFBundleIconFile</key><string>Talus</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHumanReadableCopyright</key><string>© 2026 Artem Hrechuk. MIT License; third-party licenses in Resources.</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$APP"
cp target/release/talus-server release/talus-server
ditto -c -k --keepParent "$APP" "release/Talus-${VERSION}-macos.zip"
echo "Built $APP ($(du -sh "$APP" | cut -f1))"
