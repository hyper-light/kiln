#!/bin/bash
# m8c: m8's Alpine root (libstdc++ installed) with a /proc of its own (as a Shards domain has): does the musl build print its version?
set -u
R=$HOME/.cache/s22-opencode-research/alpine
echo "M8C run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname)"
echo "PROC_BEFORE $(ls "$R/proc" 2>&1 | head -3 | tr '\n' ' ')"
unshare -rfpm --mount-proc="$R/proc" chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/home/opencode /oc/opencode-musl --version; echo "EXIT $?"
unshare -rfpm --mount-proc="$R/proc" chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/home/opencode /oc/opencode-musl debug paths 2>&1 | head -3; echo "EXIT ${PIPESTATUS[0]}"
