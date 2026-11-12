#!/bin/bash
# m8b: m8's Alpine root again (libstdc++ installed): was /tmp there, and does the musl build print its version with one?
set -u
R=$HOME/.cache/s22-opencode-research/alpine
echo "M8B run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname)"
echo "TMP_BEFORE $(ls -ld "$R/tmp" 2>&1)"
mkdir -p "$R/tmp" && chmod 1777 "$R/tmp"
unshare -r chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/home/opencode /oc/opencode-musl --version; echo "EXIT $?"
unshare -r chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/home/opencode /oc/opencode-musl debug paths | head -4; echo "EXIT $?"
