#!/usr/bin/env bash
# Installs build/"RD Agent.app" (from build-app.sh) into /Applications and (re)loads the
# LaunchAgent in the current user's GUI session. Run as the logged-in user, not root.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LABEL=dev.thijmen.rd.agent
SRC_APP="$HERE/build/RD Agent.app"
DEST_APP="/Applications/RD Agent.app"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/rd-agent"
DOMAIN="gui/$(id -u)"

die() { echo "install: $*" >&2; exit 1; }

[ "$(id -u)" -ne 0 ] || die "run as the logged-in user, not root"
[ -d "$SRC_APP" ] || die "$SRC_APP not found; run build-app.sh first"
codesign --verify --strict "$SRC_APP" || die "$SRC_APP has an invalid signature"

# Stop the running agent and wait until launchd has let go of it.
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
for _ in 1 2 3 4 5 6 7 8 9 10; do
  launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
  sleep 0.5
done

rm -rf "$DEST_APP"
ditto "$SRC_APP" "$DEST_APP"

mkdir -p "$LOG_DIR" "$(dirname "$PLIST")"
sed "s|@HOME@|$HOME|g" "$HERE/$LABEL.plist.template" > "$PLIST"
plutil -lint -s "$PLIST"

launchctl enable "$DOMAIN/$LABEL"
launchctl bootstrap "$DOMAIN" "$PLIST"

echo "installed $DEST_APP and loaded $LABEL in $DOMAIN"
launchctl print "$DOMAIN/$LABEL" | grep -E '^[[:space:]]*(state|pid|last exit code) =' || true
cat <<EOF
logs:    $LOG_DIR/
restart: launchctl kickstart -k $DOMAIN/$LABEL
First install: run "$DEST_APP/Contents/MacOS/rd-agent" setup in person to store the token
and grant Screen Recording + Accessibility.
EOF
