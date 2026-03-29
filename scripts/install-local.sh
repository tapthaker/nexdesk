#!/bin/bash
set -euo pipefail

IDENTITY="Developer ID Application: Tapan Thaker (GAVLHA4J6J)"
BINARY="target/release/nexdesk"
DEST="$HOME/.local/bin/nexdesk"

cargo build --release

codesign --force --sign "$IDENTITY" \
  --options runtime --timestamp \
  --entitlements entitlements.plist \
  "$BINARY"

echo "Submitting for notarization..."
ditto -c -k "$BINARY" "$BINARY.zip"
xcrun notarytool submit "$BINARY.zip" \
  --keychain-profile "nexdesk" \
  --wait
rm "$BINARY.zip"

cp "$BINARY" "$DEST"
launchctl kickstart -k "gui/$(id -u)/com.nexdesk.agent"
echo "Installed and restarted ($(${DEST} --version))"
