#!/bin/sh
# Installs (or updates) the rd agent on Linux: the terminal and files of this machine in the
# browser (docs/setup-linux.md). Run it as the user whose shell you want, not as root:
#
#   curl -fsSL https://raw.githubusercontent.com/Thijmen/remoted/main/packaging/linux/install.sh | sh -s -- <setup code>
#
# - With a setup code (from Add device in the portal) it sets the agent up without questions.
# - Without one it asks (the setup questions), or, when the agent is set up already, updates
#   it and restarts it.
#
# Environment: RD_AGENT_VERSION (e.g. 1.11.0; default: the newest agent release),
# RD_INSTALL_DIR (default ~/.local/bin), RD_REPO (default Thijmen/remoted), RD_DOWNLOAD_BASE
# (where the release files are, for mirrors and tests; needs RD_AGENT_VERSION).
set -eu

repo="${RD_REPO:-Thijmen/remoted}"
dir="${RD_INSTALL_DIR:-$HOME/.local/bin}"
code="${1:-}"

say() { printf '%s\n' "$*"; }
die() { printf 'rd-agent install: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || die "this installer is for Linux (macOS and Windows: see docs/setup-mac.md, docs/setup-windows.md)"
if [ "$(id -u)" = 0 ] && [ "${RD_ALLOW_ROOT:-}" != 1 ]; then
  die "run this as the user whose shell you want in the browser, not as root (RD_ALLOW_ROOT=1 to insist)"
fi
command -v curl >/dev/null 2>&1 || die "needs curl"

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) die "no build for $(uname -m) (x86_64 and aarch64 only)" ;;
esac

version="${RD_AGENT_VERSION:-}"
[ -z "${RD_DOWNLOAD_BASE:-}" ] || [ -n "$version" ] || die "RD_DOWNLOAD_BASE needs RD_AGENT_VERSION"
if [ -z "$version" ]; then
  # The newest agent-v* release (the repository also has rd Desktop releases).
  version="$(curl -fsSL "https://api.github.com/repos/$repo/releases?per_page=50" \
    | grep -o '"tag_name": *"agent-v[0-9][^"]*"' | head -n 1 | sed 's/.*agent-v//; s/"$//')" \
    || die "cannot list the releases of $repo"
  [ -n "$version" ] || die "no agent release found in $repo"
fi

asset="rd-agent-$version-linux-$arch.tar.gz"
base="${RD_DOWNLOAD_BASE:-https://github.com/$repo/releases/download/agent-v$version}"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' INT TERM

say "downloading rd agent $version ($arch)"
curl -fsSL -o "$tmp/$asset" "$base/$asset" \
  || die "agent $version has no Linux build ($asset); there are Linux builds from agent 1.11.0 on"
curl -fsSL -o "$tmp/SHA256SUMS.txt" "$base/SHA256SUMS.txt" || die "cannot download SHA256SUMS.txt"

expected="$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS.txt" | cut -d' ' -f1)"
[ -n "$expected" ] || die "$asset is not in SHA256SUMS.txt"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp/$asset" | cut -d' ' -f1)"
else
  actual="$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1)"
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
# Replaced, not overwritten: a running agent keeps its file until it restarts.
install -m 755 "$tmp/rd-agent" "$dir/.rd-agent.new"
mv -f "$dir/.rd-agent.new" "$dir/rd-agent"
say "installed $dir/rd-agent ($("$dir/rd-agent" --version))"

config="${XDG_CONFIG_HOME:-$HOME/.config}/rd-agent/config.toml"
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/rd-agent.service"
if [ -n "$code" ]; then
  RD_SETUP_CODE="$code" "$dir/rd-agent" setup
elif [ -f "$config" ]; then
  if [ -f "$unit" ]; then
    systemctl --user restart rd-agent.service && say "updated; the agent restarted"
  else
    say "updated; start it with: $dir/rd-agent install"
  fi
elif (: </dev/tty) 2>/dev/null; then
  # Piped into sh: stdin is this script, so the questions come from the terminal.
  "$dir/rd-agent" setup </dev/tty
else
  say "now set it up: $dir/rd-agent setup (or rerun with the setup code from the portal)"
fi

case ":$PATH:" in
  *":$dir:"*) ;;
  *) say "note: $dir is not on your PATH; add it to run rd-agent by name" ;;
esac
