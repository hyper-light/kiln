#!/bin/bash
# m8 (Linux x86_64 runner): which libc each opencode 2.2.7 build needs. Alpine 3.22's minirootfs (Alpine's
# own CDN) entered with `unshare -r chroot`: the musl build without libstdc++, then after
# `apk add libstdc++`; the glibc build there; the musl build on the glibc host.
set -u
W=$HOME/.cache/s22-opencode-research
cd "$W/dl" || exit 1
V=2.2.7; AV=3.22.2
echo "M8 run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname) $(uname -srm)"
f=opencode-linux-x64-musl.tar.gz
[ -f "$f" ] || curl -fsSL -o "$f" "https://github.com/anomalyco/opencode/releases/download/v$V/$f"
echo "ARCHIVE $f sha256=$(sha256sum "$f" | cut -d' ' -f1)"
rm -rf musl && mkdir musl && tar -xzf "$f" -C musl
m=alpine-minirootfs-$AV-x86_64.tar.gz
[ -f "$m" ] || curl -fsSL -o "$m" "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$m"
echo "ROOTFS $m sha256=$(sha256sum "$m" | cut -d' ' -f1) published=$(curl -fsSL "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$m.sha256" | cut -d' ' -f1)"
R=$W/alpine; rm -rf "$R"; mkdir -p "$R" && tar -xzf "$m" -C "$R" 2>/dev/null
cp /etc/resolv.conf "$R/etc/resolv.conf"
mkdir -p "$R/oc" "$R/home/opencode" && cp musl/opencode "$R/oc/opencode-musl" && cp gnu/opencode "$R/oc/opencode-gnu"
in_alpine() { unshare -r chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/home/opencode "$@"; }
echo "--- musl build on Alpine, no libstdc++"
in_alpine /oc/opencode-musl --version; echo "EXIT $?"
echo "--- apk add libstdc++"
in_alpine /sbin/apk add --no-cache libstdc++ 2>&1 | tail -4
in_alpine /sbin/apk info -R libstdc++ 2>&1 | head -6
echo "--- musl build on Alpine with libstdc++"
in_alpine /oc/opencode-musl --version; echo "EXIT $?"
echo "--- glibc build on Alpine"
in_alpine /oc/opencode-gnu --version; echo "EXIT $?"
echo "--- musl build on the glibc host"
env -i PATH=/usr/bin:/bin HOME=$W/scratch "$W/dl/musl/opencode" --version; echo "EXIT $?"
