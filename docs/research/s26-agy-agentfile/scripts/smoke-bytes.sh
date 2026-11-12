#!/usr/bin/env bash
# Plan step 26: the exact bytes `agy --version` writes, for the built-in's smoke line. Each pinned
# archive is fetched, held to pin.json's SHA-256 and size, its one member to the pin's, and run with
# a clean environment (and once more with no network) through od. arm64 runs through qemu-user
# where this machine has it. Big files under ~/.cache, removed at the end.
set -uo pipefail
pin=$(pwd)/pin.json
W=$(mktemp -d "$HOME/.cache/s26-agy-smoke.XXXXXX")
trap 'rm -rf "$W"' EXIT
echo "# on $(hostname) ($(uname -m), $(uname -r)), $(date -u +%FT%TZ); pin.json sha256 $(sha256sum "$pin" | cut -d' ' -f1)"
python3 -I -c 'import json, sys
p = json.load(open(sys.argv[1]))
for b in p["builds"]:
    f = b["files"][0]
    print(b["platform"], f["url"], f["sha256"], f["size"], f["unpack"]["sha256"], f["unpack"]["size"])' "$pin" > "$W/builds" || exit 1
qemu=$(command -v qemu-aarch64-static || command -v qemu-aarch64 || true)
echo "qemu-user for arm64: ${qemu:-none}"
while read -r platform url sha size msha msize; do
  d="$W/${platform#linux/}"; mkdir -p "$d/home"
  curl --proto '=https' --proto-redir '=https' -fsSL -o "$d/a.tar.gz" "$url" || { echo "$platform: fetch failed"; continue; }
  got=$(sha256sum "$d/a.tar.gz" | cut -d' ' -f1); n=$(stat -c %s "$d/a.tar.gz")
  [ "$got" = "$sha" ] && [ "$n" = "$size" ] || { echo "$platform: archive $got $n is not the pin's $sha $size"; continue; }
  tar -xzf "$d/a.tar.gz" -C "$d" antigravity || { echo "$platform: no member antigravity"; continue; }
  mgot=$(sha256sum "$d/antigravity" | cut -d' ' -f1); mn=$(stat -c %s "$d/antigravity")
  [ "$mgot" = "$msha" ] && [ "$mn" = "$msize" ] || { echo "$platform: member $mgot $mn is not the pin's $msha $msize"; continue; }
  echo "$platform: archive and member match the pin ($sha, member $msha)"
  run=("$d/antigravity")
  if [ "$platform" = linux/arm64 ]; then
    [ "$(uname -m)" = aarch64 ] || { [ -n "$qemu" ] && run=("$qemu" "$d/antigravity"); } || { echo "$platform: not run (no arm64 here, no qemu-user)"; continue; }
  fi
  out=$(env -i PATH=/usr/bin:/bin HOME="$d/home" AGY_CLI_DISABLE_AUTO_UPDATE=true "${run[@]}" --version 2>"$d/err" | od -An -tx1 | tr -s ' \n' ' '); rc=${PIPESTATUS[0]}
  echo "$platform: --version stdout bytes:$out (exit $rc; stderr $(wc -c < "$d/err") bytes)"
  if unshare -rn true 2>/dev/null; then
    out=$(unshare -rn env -i PATH=/usr/bin:/bin HOME="$d/home" AGY_CLI_DISABLE_AUTO_UPDATE=true "${run[@]}" --version 2>/dev/null | od -An -tx1 | tr -s ' \n' ' ')
    echo "$platform: with no network (unshare -rn):$out"
  fi
  echo "$platform: files it left under HOME: $(find "$d/home" -mindepth 1 | wc -l)"
done < "$W/builds"
