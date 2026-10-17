#!/usr/bin/env bash
# Generates an agent token for one machine and the hash the gateway stores.
#
#   tools/new-agent-token.sh mbp
#
# - The TOKEN goes to the agent only: paste it into `rd-agent setup` on that machine (it is
#   kept in the macOS Keychain). Never store it in the cluster or in git.
# - The SHA-256 goes into the 1Password item `rd-agent-tokens` (vault k3s-vault), field named
#   after the machine; the ExternalSecret mounts it as /etc/rd/agents/<machine>.
# Rotating: run again, update the 1Password field, re-run setup on the machine.
set -euo pipefail

machine="${1:-}"
[[ "$machine" =~ ^[A-Za-z0-9_-]{1,64}$ ]] || { echo "usage: $0 <machine id: 1-64 of [A-Za-z0-9_-]>" >&2; exit 1; }

secret="$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')"
token="rd1.${machine}.${secret}"
hash="$(printf '%s' "$token" | shasum -a 256 | cut -d' ' -f1)"

cat <<EOF
machine: $machine
token (agent only, keep secret):
  $token
sha256 (1Password item rd-agent-tokens, field "$machine"):
  $hash
EOF
