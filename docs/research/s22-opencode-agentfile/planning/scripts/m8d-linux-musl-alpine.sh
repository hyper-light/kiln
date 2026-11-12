#!/bin/bash
# m8d (any Linux x86_64 runner, self-contained): opencode 2.2.7's musl build in an Alpine 3.22 root
# (minirootfs from Alpine's CDN, its digest checked) with a /proc and a /tmp of its own, as a Shards
# domain has: without libstdc++, then after `apk add libstdc++`; then `debug paths` with XDG on a volume dir.
set -u
W=$HOME/.cache/s22-opencode-research; mkdir -p "$W/dl" && cd "$W/dl" || exit 1
V=2.2.7; AV=3.22.2
echo "M8D run $(date -u +%Y-%m-%dT%H:%M:%SZ) host $(hostname) $(uname -srm)"
f=opencode-linux-x64-musl.tar.gz
[ -f "$f" ] || curl -fsSL -o "$f" "https://github.com/anomalyco/opencode/releases/download/v$V/$f"
echo "ARCHIVE $f sha256=$(sha256sum "$f" | cut -d' ' -f1)"
m=alpine-minirootfs-$AV-x86_64.tar.gz
[ -f "$m" ] || curl -fsSL -o "$m" "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$m"
want=$(curl -fsSL "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/$m.sha256" | cut -d' ' -f1)
got=$(sha256sum "$m" | cut -d' ' -f1); echo "ROOTFS $m sha256=$got $([ "$got" = "$want" ] && echo match || echo MISMATCH)"
R=$W/alpine-d; rm -rf "$R"; mkdir -p "$R" && tar -xzf "$m" -C "$R" 2>/dev/null
mkdir -p "$R/proc" "$R/tmp" "$R/oc" "$R/home/opencode" && chmod 1777 "$R/tmp" && cp /etc/resolv.conf "$R/etc/resolv.conf"
tar -xzf "$f" -C "$R/oc"
in_alpine() { unshare -rfpm --mount-proc="$R/proc" chroot "$R" /usr/bin/env -i PATH=/usr/sbin:/usr/bin:/sbin:/bin HOME=/tmp "$@"; }
echo "--- musl build on Alpine, no libstdc++"
in_alpine /oc/opencode --version 2>&1 | head -2; echo "EXIT ${PIPESTATUS[0]}"
echo "--- apk add libstdc++"
in_alpine /sbin/apk add --no-cache libstdc++ 2>&1 | grep -E 'Installing|OK:'
echo "--- musl build on Alpine with libstdc++, /proc and /tmp"
in_alpine /oc/opencode --version; echo "EXIT $?"
echo "--- debug paths with the XDG roots on /home/opencode"
in_alpine XDG_CONFIG_HOME=/home/opencode/.config XDG_DATA_HOME=/home/opencode/.local/share XDG_STATE_HOME=/home/opencode/.local/state XDG_CACHE_HOME=/home/opencode/.cache /oc/opencode debug paths; echo "EXIT $?"
