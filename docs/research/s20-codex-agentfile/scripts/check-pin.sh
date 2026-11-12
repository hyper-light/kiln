#!/usr/bin/env bash
# Plan step 20 [kiln], step 1's test (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): every archive
# pin.json names, downloaded afresh, has the SHA-256 and size pin.json gives it; every member it pins
# (unpack.member) has, in that download, the member's own SHA-256 and size; and every tree it pins has
# the entries and unpacked bytes the pin gives (tar -t and tar -tv's totals, counted by tarfile).
#
#   check-pin.sh PIN_JSON
set -euo pipefail

pin=${1:?usage: check-pin.sh PIN_JSON}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
work=$(mktemp -d "$cache/s20-pin-check.XXXXXX")
trap 'rm -rf "$work"' EXIT
n=0
ok=0
members=0
mok=0
trees=0
tok=0
while read -r url want size; do
    n=$((n + 1))
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$work/$n" "$url"
    got=$(sha256sum "$work/$n" | cut -d' ' -f1)
    bytes=$(stat -c %s "$work/$n")
    verdict=MISMATCH
    if [ "$got" = "$want" ] && [ "$bytes" = "$size" ]; then
        verdict=MATCH
        ok=$((ok + 1))
    fi
    printf 'FRESH %s %s sha256=%s bytes=%s (pin: %s, %s)\n' "$verdict" "$url" "$got" "$bytes" "$want" "$size"
    while read -r member mwant msize; do
        members=$((members + 1))
        mgot=$(tar -xzOf "$work/$n" "$member" | sha256sum | cut -d' ' -f1) || mgot=unreadable
        mbytes=$(tar -xzOf "$work/$n" "$member" | wc -c) || mbytes=unreadable
        verdict=MISMATCH
        if [ "$mgot" = "$mwant" ] && [ "$mbytes" = "$msize" ]; then
            verdict=MATCH
            mok=$((mok + 1))
        fi
        printf 'FRESH MEMBER %s %s in %s sha256=%s bytes=%s (pin: %s, %s)\n' "$verdict" "$member" "$url" "$mgot" "$mbytes" "$mwant" "$msize"
    done < <(jq -r --arg u "$url" '[.builds[].files[] | select(.url == $u and .unpack.member != null)
        | "\(.unpack.member) \(.unpack.sha256) \(.unpack.size)"] | unique[]' "$pin")
    while read -r tentries tbytes; do
        trees=$((trees + 1))
        totals=$(python3 -I -c 'import sys, tarfile
e = b = 0
with tarfile.open(sys.argv[1], "r|gz") as t:
    for m in t:
        e += 1
        b += m.size if m.isreg() else 0
print(e, b)' "$work/$n") || totals=unreadable
        verdict=MISMATCH
        if [ "$totals" = "$tentries $tbytes" ]; then
            verdict=MATCH
            tok=$((tok + 1))
        fi
        printf 'FRESH TREE %s %s entries and bytes=%s (pin: %s %s)\n' "$verdict" "$url" "$totals" "$tentries" "$tbytes"
    done < <(jq -r --arg u "$url" '[.builds[].files[] | select(.url == $u and .unpack.member == null)
        | "\(.unpack.entries) \(.unpack.unpacked_bytes)"] | unique[]' "$pin")
    rm -f "$work/$n"
done < <(jq -r '[.builds[].files[] | "\(.url) \(.sha256) \(.size)"] | unique[]' "$pin")
printf 'FRESH archives=%s matching=%s members=%s matching=%s trees=%s matching=%s\n' "$n" "$ok" "$members" "$mok" "$trees" "$tok"
[ "$n" -gt 0 ] && [ "$ok" = "$n" ] && [ "$mok" = "$members" ] && [ "$tok" = "$trees" ]
