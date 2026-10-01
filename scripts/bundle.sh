#!/bin/sh
# Builds target/release/Remounty.app (menu bar only app, ad-hoc signed).
set -eu

cd "$(dirname "$0")/.."
cargo build --release

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
APP=target/release/Remounty.app

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/remounty "$APP/Contents/MacOS/remounty"
cp target/release/remounty-helper "$APP/Contents/MacOS/remounty-helper"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleDisplayName</key><string>Remounty</string>
    <key>CFBundleExecutable</key><string>remounty</string>
    <key>CFBundleIdentifier</key><string>io.github.remounty</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleName</key><string>Remounty</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHumanReadableCopyright</key><string>MIT License</string>
    <key>NSAppleEventsUsageDescription</key><string>Remounty asks Finder to open volumes in a normal window with toolbar and sidebar.</string>
</dict>
</plist>
PLIST

# Nested code must be signed before the bundle itself.
codesign --force --sign - "$APP/Contents/MacOS/remounty-helper" >/dev/null 2>&1 || echo "warning: signing the helper failed" >&2
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "warning: ad-hoc signing failed" >&2
echo "Built $APP"
