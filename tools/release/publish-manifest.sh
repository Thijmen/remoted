#!/usr/bin/env bash
# Uploads an update manifest (desktop.json / agent.json) to the `updates` pre-release, where
# rd Desktop and the agent read it (docs/updates.md), replacing the previous one. Creates that
# release the first time. It is a pre-release so that GitHub never shows it as the latest
# release. Needs GH_TOKEN (contents: write) and GITHUB_REPOSITORY.
#
#   bash tools/release/publish-manifest.sh desktop.json
set -euo pipefail

file="${1:?usage: publish-manifest.sh <manifest.json>}"
repo="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is not set}"
tag=updates

# The apps only install files whose signature checks out, but a broken manifest would still
# stop every update: make sure it parses and names a version before replacing the old one.
jq -e '.version and .platforms and (.platforms | length > 0)' "$file" >/dev/null \
  || { echo "publish-manifest: $file is not a valid update manifest" >&2; exit 1; }

if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
  # Two release workflows may get here at the same time; the loser finds the winner's release.
  gh release create "$tag" --repo "$repo" --prerelease --latest=false \
    --target "${GITHUB_SHA:-main}" --title "Update manifests" \
    --notes "Update manifests for rd Desktop (desktop.json) and the rd agent (agent.json), read by the apps to find new versions: see docs/updates.md. Nothing to download here; the releases are the desktop-v* and agent-v* ones." \
    || gh release view "$tag" --repo "$repo" >/dev/null
fi
gh release upload "$tag" "$file" --repo "$repo" --clobber
echo "published $(basename "$file") ($(jq -r .version "$file")) to the $tag release"
