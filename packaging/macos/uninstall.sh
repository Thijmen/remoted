#!/usr/bin/env bash
# Unloads the RD Agent LaunchAgent and removes it and the app.
#
#   packaging/macos/uninstall.sh [--purge]
#
# --purge also deletes the logs, the config file with the gateway token, a token left in the
# Keychain by older installs, and the app's Screen Recording / Accessibility grants.
set -euo pipefail

LABEL=dev.thijmen.rd.agent
DEST_APP="/Applications/RD Agent.app"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/rd-agent"
CONFIG_DIR="$HOME/Library/Application Support/$LABEL"
DOMAIN="gui/$(id -u)"

purge=0
case "${1:-}" in
  "") ;;
  --purge) purge=1 ;;
  *) echo "usage: $0 [--purge]" >&2; exit 1 ;;
esac

launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
rm -f "$PLIST"
rm -rf "$DEST_APP"
echo "removed $LABEL LaunchAgent and $DEST_APP"

if [ "$purge" -eq 1 ]; then
  rm -rf "$LOG_DIR" "$CONFIG_DIR"
  security delete-generic-password -s "$LABEL" -a gateway-token >/dev/null 2>&1 || true
  tccutil reset ScreenCapture "$LABEL" >/dev/null 2>&1 || true
  tccutil reset Accessibility "$LABEL" >/dev/null 2>&1 || true
  echo "purged logs, config (with the token), Keychain token and privacy grants"
fi
