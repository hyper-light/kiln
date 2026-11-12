#!/usr/bin/env bash
# Plan step 20 [kiln], step 2 (kiln plan 01a1ce705d7076648c1d9f9ee26214d3): every ELF file in each
# Codex package pin.json pins, with its machine, type, ELF interpreter and needed libraries (elf.py),
# so the record says which files need a loader or libraries from the image they run in. Each package
# is downloaded afresh, held to its pinned SHA-256, and unpacked into a new directory of its own.
#
#   elf-survey.sh PIN_JSON      (elf.py beside it)
set -euo pipefail

pin=${1:?usage: elf-survey.sh PIN_JSON}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
cache=${XDG_CACHE_HOME:-$HOME/.cache}
work=$(mktemp -d "$cache/s20-elf-survey.XXXXXX")
trap 'rm -rf "$work"' EXIT
while read -r plat url want; do
    d=$work/${plat//\//-}
    mkdir -p "$d/x"
    curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$d/a.tar.gz" "$url"
    got=$(sha256sum "$d/a.tar.gz" | cut -d' ' -f1)
    if [ "$got" != "$want" ]; then
        echo "SURVEY $plat $url: sha256 $got, the pin says $want"
        exit 1
    fi
    tar -xzf "$d/a.tar.gz" -C "$d/x" --no-same-owner --no-same-permissions
    n=0
    while IFS= read -r -d '' f; do
        if [ "$(head -c 4 "$d/x/$f" | od -An -tx1 | tr -d ' \n')" = 7f454c46 ]; then
            n=$((n + 1))
            echo "ELF $plat ${f#./} $(python3 -I "$here/elf.py" "$d/x/$f" 2>&1)"
        fi
    done < <(cd "$d/x" && find . -type f -print0 | sort -z)
    echo "SURVEY $plat $url: $n ELF files"
    rm -rf "$d"
done < <(jq -r '.builds[] | .platform as $p | .files[] | select(.path == ".") | "\($p) \(.url) \(.sha256)"' "$pin")
