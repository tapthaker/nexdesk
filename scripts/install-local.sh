#!/bin/bash
set -euo pipefail

IDENTITY="Developer ID Application: Tapan Thaker (GAVLHA4J6J)"
BINARY="target/release/nexdesk"
DEST="$HOME/.local/bin/nexdesk"
DEST_DIR=$(dirname "$DEST")
TMP_DEST=""

cleanup() {
  if [[ -n "$TMP_DEST" && -e "$TMP_DEST" ]]; then
    rm -f "$TMP_DEST"
  fi
}
trap cleanup EXIT

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

mkdir -p "$DEST_DIR"
TMP_DEST=$(mktemp "$DEST_DIR/.nexdesk.tmp.XXXXXX")
cp "$BINARY" "$TMP_DEST"
chmod 755 "$TMP_DEST"
mv -f "$TMP_DEST" "$DEST"
TMP_DEST=""

if command -v launchctl >/dev/null 2>&1; then
  SERVICE="gui/$(id -u)/com.nexdesk.agent"
  if launchctl print "$SERVICE" >/dev/null 2>&1; then
    launchctl kickstart -k "$SERVICE"
  else
    echo "LaunchAgent $SERVICE is not loaded; skipping restart"
  fi
fi

echo "Installed ($(${DEST} --version))"
