#!/bin/sh
# Builds Crossdesk.app on a Mac. Needs Rust: curl https://sh.rustup.rs -sSf | sh
# The app is signed ad hoc so macOS can remember its Accessibility permission.
# After a rebuild, macOS may ask for the permission again.
set -e
cd "$(dirname "$0")/.."
cargo build --release

APP=target/Crossdesk.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/crossdesk "$APP/Contents/MacOS/crossdesk"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Crossdesk</string>
  <key>CFBundleDisplayName</key><string>Crossdesk</string>
  <key>CFBundleIdentifier</key><string>com.crossdesk.app</string>
  <key>CFBundleExecutable</key><string>crossdesk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Crossdesk finds and talks to your other computers on the local network.</string>
</dict>
</plist>
PLIST
codesign --force --deep -s - "$APP"
echo "Built $APP. Move it to /Applications and open it."
