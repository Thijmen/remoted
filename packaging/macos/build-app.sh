#!/usr/bin/env bash
# Builds and signs packaging/macos/build/"RD Agent.app".
#
#   packaging/macos/build-app.sh [--skip-build]
#
# Signs with the self-signed identity from make-signing-identity.sh (override with
# RD_SIGN_IDENTITY) so the designated requirement - and with it the Screen Recording /
# Accessibility grants - survives rebuilds. Falls back to ad-hoc signing
# if the identity is missing. Respects CARGO_TARGET_DIR.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
TARGET=aarch64-apple-darwin
BUNDLE_ID=dev.thijmen.rd.agent
IDENTITY="${RD_SIGN_IDENTITY:-RD Local Code Signing}"
APP="$HERE/build/RD Agent.app"

die() { echo "build-app: $*" >&2; exit 1; }

skip_build=0
case "${1:-}" in
  "") ;;
  --skip-build) skip_build=1 ;;
  *) die "usage: $0 [--skip-build]" ;;
esac

cd "$ROOT"
if [ "$skip_build" -eq 0 ]; then
  cargo build --release -p rd-agent --target "$TARGET"
fi
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/$TARGET/release/rd-agent"
[ -x "$BIN" ] || die "no binary at $BIN"

# The ScreenCaptureKit bridge loads the Swift runtime via @rpath (rd-agent's build script must
# add /usr/lib/swift), and the bundle must not depend on anything outside the OS.
if ! otool -l "$BIN" | grep -A2 LC_RPATH | grep -q '/usr/lib/swift'; then
  die "$BIN has no /usr/lib/swift rpath (add cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift to rd-agent's build.rs)"
fi
foreign="$(otool -L "$BIN" | tail -n +2 | awk '{print $1}' | grep -v -E '^(/usr/lib/|/System/|@rpath/libswift)' || true)"
[ -z "$foreign" ] || die "non-system dylib dependencies: $foreign"

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version *= *"\(.*\)"/\1/p' Cargo.toml)"
[ -n "$VERSION" ] || die "cannot read [workspace.package] version"
BUILD="$(git rev-list --count HEAD 2>/dev/null || date +%Y%m%d%H%M)"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/rd-agent"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@BUILD@/$BUILD/g" "$HERE/Info.plist.template" \
  > "$APP/Contents/Info.plist"
plutil -lint -s "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

if security find-identity -p codesigning | grep -Fq "\"$IDENTITY\""; then
  sign_as="$IDENTITY"
else
  cat >&2 <<EOF

  ************************************************************************************
  WARNING: code-signing identity "$IDENTITY" not found; signing AD-HOC.
  An ad-hoc signature changes with every build, so macOS will forget the Screen
  Recording / Accessibility grants after each rebuild.
  Run packaging/macos/make-signing-identity.sh once to create the identity.
  ************************************************************************************

EOF
  sign_as=-
fi
# No hardened runtime: the app is not notarized and needs no runtime restrictions.
codesign --force --sign "$sign_as" --identifier "$BUNDLE_ID" --timestamp=none "$APP"
codesign --verify --strict --verbose=1 "$APP"

echo "built $APP (version $VERSION, build $BUILD, signed by: $sign_as)"
echo "designated requirement:"
codesign -d -r- "$APP" 2>/dev/null | sed 's/^/  /'
