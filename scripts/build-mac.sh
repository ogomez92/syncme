#!/bin/sh
# Builds a universal (Apple Silicon + Intel) SyncMe binary and a menu-bar-only
# SyncMe.app around it. Run on a Mac from the project root.
set -e
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
mkdir -p dist
lipo -create -output dist/syncme \
  target/aarch64-apple-darwin/release/syncme \
  target/x86_64-apple-darwin/release/syncme

APP=dist/SyncMe.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp dist/syncme "$APP/Contents/MacOS/syncme"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>SyncMe</string>
  <key>CFBundleIdentifier</key><string>com.syncme.app</string>
  <key>CFBundleExecutable</key><string>syncme</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSUIElement</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
</dict></plist>
PLIST
# Ad-hoc signature so Gatekeeper and file-access prompts behave consistently.
codesign --force --deep --sign - "$APP" || true
echo "Built dist/syncme (universal) and $APP"
