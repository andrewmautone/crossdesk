#!/bin/sh
# Builds target/Crossdesk.app on a Mac. Needs Rust: curl https://sh.rustup.rs -sSf | sh
#   UNIVERSAL=1                 one binary for Apple Silicon and Intel
#   CROSSDESK_SIGN_IDENTITY=... signing certificate name (default: ad hoc)
# macOS remembers the Accessibility permission per signature: with ad hoc
# signing it asks again after every rebuild, with a fixed certificate it does not.
set -e
cd "$(dirname "$0")/.."

if [ "$UNIVERSAL" = "1" ]; then
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  mkdir -p target/release
  lipo -create -output target/release/crossdesk \
    target/aarch64-apple-darwin/release/crossdesk \
    target/x86_64-apple-darwin/release/crossdesk
else
  cargo build --release
fi

APP=target/Crossdesk.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/crossdesk "$APP/Contents/MacOS/crossdesk"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Crossdesk</string>
  <key>CFBundleDisplayName</key><string>Crossdesk</string>
  <key>CFBundleIdentifier</key><string>com.crossdesk.app</string>
  <key>CFBundleExecutable</key><string>crossdesk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.${CROSSDESK_BUILD:-0}</string>
  <key>CFBundleVersion</key><string>${CROSSDESK_BUILD:-0}</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>Crossdesk finds and talks to your other computers on the local network.</string>
</dict>
</plist>
PLIST

IDENTITY="${CROSSDESK_SIGN_IDENTITY:--}"
if ! codesign --force --deep --identifier com.crossdesk.app -s "$IDENTITY" "$APP"; then
  echo "warning: signing with '$IDENTITY' failed, falling back to ad hoc" >&2
  codesign --force --deep --identifier com.crossdesk.app -s - "$APP"
fi
codesign -dv "$APP" 2>&1 | grep -E "Identifier|Authority|Signature" || true
echo "Built $APP"
