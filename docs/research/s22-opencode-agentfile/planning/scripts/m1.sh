#!/bin/bash
# m1: opencode v2.2.7 Linux archives: digests vs SHA256SUMS and GitHub's asset digests, GitHub attestations.
set -u
V=2.2.7
D=$HOME/.cache/s22-opencode-research/dl; mkdir -p "$D"
B=https://github.com/anomalyco/opencode/releases/download/v$V
cd "$D" || exit 1
echo "M1 run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(uname -sm) gh $(gh --version | head -1 | cut -d' ' -f3)"
curl -fsSL -o SHA256SUMS "$B/SHA256SUMS" && echo "SHA256SUMS sha256 $(shasum -a 256 SHA256SUMS | cut -d' ' -f1)"
for a in opencode-linux-x64 opencode-linux-arm64 opencode-linux-x64-musl opencode-linux-arm64-musl opencode-linux-x64-baseline opencode-linux-x64-baseline-musl; do
  f=$a.tar.gz
  [ -f "$f" ] || curl -fsSL -o "$f" "$B/$f" || { echo "DOWNLOAD_FAIL $f"; continue; }
  got=$(shasum -a 256 "$f" | cut -d' ' -f1)
  listed=$(awk -v n="$f" '$2==n{print $1}' SHA256SUMS)
  api=$(gh api "repos/anomalyco/opencode/releases/tags/v$V" --jq ".assets[] | select(.name==\"$f\") | .digest" | sed 's/^sha256://')
  size=$(stat -f %z "$f")
  echo "ARCHIVE $f size=$size sha256=$got sums=$([ "$got" = "$listed" ] && echo match || echo MISMATCH) api=$([ "$got" = "$api" ] && echo match || echo MISMATCH)"
done
