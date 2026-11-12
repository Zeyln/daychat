#!/usr/bin/env bash
# Sign a draft release that CI built, then publish it.
#
# Run this on the machine that holds the minisign secret key, from a
# checkout of the repo. Needs: gh (logged in), minisign.
#
#   scripts/sign-release.sh v0.4.0
#
# Only sign releases you tagged yourself: your signature is what makes
# every daychat client willing to install it.
set -euo pipefail

tag="${1:?usage: scripts/sign-release.sh vX.Y.Z}"
repo="Zeyln/daychat"
here="$(cd "$(dirname "$0")/.." && pwd)"
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT

gh release view "$tag" --repo "$repo" --json isDraft --jq '.isDraft' | grep -qx true \
  || { echo "$tag is not a draft release; nothing to sign" >&2; exit 1; }

gh release download "$tag" --repo "$repo" --pattern SHA256SUMS --dir "$dir"
echo "Signing these checksums for daychat $tag:"
cat "$dir/SHA256SUMS"
echo

# The trusted comment is part of the signature; clients require it to name
# this exact release, so an old signature can't be reused for a new tag.
minisign -S -m "$dir/SHA256SUMS" -t "daychat $tag" ${MINISIGN_KEY:+-s "$MINISIGN_KEY"}
minisign -V -p "$here/packaging/release.pub" -m "$dir/SHA256SUMS"

gh release upload "$tag" "$dir/SHA256SUMS.minisig" --repo "$repo"
gh release edit "$tag" --draft=false --latest --repo "$repo"
echo "Published daychat $tag."
