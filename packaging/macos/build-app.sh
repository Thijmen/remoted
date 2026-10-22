#!/usr/bin/env bash
# Builds and signs packaging/macos/build/"RD Agent.app".
#
#   packaging/macos/build-app.sh [--skip-build] [--universal]
#
# --universal builds for Apple silicon and Intel and joins them with lipo (the released app);
# without it, Apple silicon only. --skip-build packages what the last build left.
#
# Signs with the self-signed identity from make-signing-identity.sh (override with
# RD_SIGN_IDENTITY; RD_SIGN_KEYCHAIN names the keychain that holds it) so the designated
# requirement - and with it the Screen Recording / Accessibility grants - survives rebuilds.
# Release builds (.github/workflows/agent.yml) use the project's "rd Code Signing" certificate,
# so updates keep the grants too (docs/updates.md). Falls back to ad-hoc signing if the identity
# is missing, unless RD_SIGN_REQUIRED=1. Respects CARGO_TARGET_DIR.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BUNDLE_ID=dev.thijmen.rd.agent
IDENTITY="${RD_SIGN_IDENTITY:-RD Local Code Signing}"
APP="$HERE/build/RD Agent.app"

die() { echo "build-app: $*" >&2; exit 1; }

skip_build=0
targets=(aarch64-apple-darwin)
for arg in "$@"; do
  case "$arg" in
    --skip-build) skip_build=1 ;;
    --universal) targets=(aarch64-apple-darwin x86_64-apple-darwin) ;;
    *) die "usage: $0 [--skip-build] [--universal]" ;;
  esac
done

cd "$ROOT"
OUT="${CARGO_TARGET_DIR:-$ROOT/target}"
bins=()
for target in "${targets[@]}"; do
  if [ "$skip_build" -eq 0 ]; then
    cargo build --locked --release -p rd-agent --target "$target"
  fi
  bin="$OUT/$target/release/rd-agent"
  [ -x "$bin" ] || die "no binary at $bin"
  # The ScreenCaptureKit bridge loads the Swift runtime via @rpath (rd-agent's build script
  # must add /usr/lib/swift), and the bundle must not depend on anything outside the OS.
  if ! otool -l "$bin" | grep -A2 LC_RPATH | grep -q '/usr/lib/swift'; then
    die "$bin has no /usr/lib/swift rpath (add cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift to rd-agent's build.rs)"
  fi
  foreign="$(otool -L "$bin" | tail -n +2 | awk '{print $1}' | grep -v -E '^(/usr/lib/|/System/|@rpath/libswift)' || true)"
  [ -z "$foreign" ] || die "non-system dylib dependencies in $bin: $foreign"
  bins+=("$bin")
done
if [ "${#bins[@]}" -gt 1 ]; then
  BIN="$OUT/rd-agent-universal"
  lipo -create -output "$BIN" "${bins[@]}"
else
  BIN="${bins[0]}"
fi

# rd-agent is versioned on its own (changesets; tools/release/sync-versions.ts).
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/rd-agent/Cargo.toml | head -n1)"
[ -n "$VERSION" ] || die "cannot read the version in crates/rd-agent/Cargo.toml"
BUILD="$(git rev-list --count HEAD 2>/dev/null || date +%Y%m%d%H%M)"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/rd-agent"
# The key tables derive from Chromium (BSD-3-Clause): ship the notice.
cp THIRD_PARTY_NOTICES.md "$APP/Contents/Resources/"
# The same icon as rd Desktop (Finder, Login Items, System Settings > Privacy & Security).
cp clients/desktop/src-tauri/icons/icon.icns "$APP/Contents/Resources/AppIcon.icns"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@BUILD@/$BUILD/g" "$HERE/Info.plist.template" \
  > "$APP/Contents/Info.plist"
plutil -lint -s "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"

keychain=()
[ -z "${RD_SIGN_KEYCHAIN:-}" ] || keychain=(--keychain "$RD_SIGN_KEYCHAIN")
if security find-identity -p codesigning ${RD_SIGN_KEYCHAIN:+"$RD_SIGN_KEYCHAIN"} | grep -Fq "\"$IDENTITY\""; then
  sign_as="$IDENTITY"
elif [ "${RD_SIGN_REQUIRED:-0}" = 1 ]; then
  die "code-signing identity \"$IDENTITY\" not found (RD_SIGN_REQUIRED=1)"
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
codesign --force --sign "$sign_as" ${keychain[@]+"${keychain[@]}"} --identifier "$BUNDLE_ID" --timestamp=none "$APP"
codesign --verify --strict --verbose=1 "$APP"

echo "built $APP (version $VERSION, build $BUILD, $(lipo -archs "$APP/Contents/MacOS/rd-agent"), signed by: $sign_as)"
echo "designated requirement:"
codesign -d -r- "$APP" 2>/dev/null | sed 's/^/  /'
