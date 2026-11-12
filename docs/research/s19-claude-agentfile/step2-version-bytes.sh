#!/usr/bin/env bash
# Step 2 (plan 01a1cf48e8157494ac9a51f7a564f526): the exact bytes the pinned glibc x86_64 build prints for
# --version, run with an empty HOME and no environment but PATH; and what it leaves in that HOME.
set -euo pipefail
pin=${1:?pin.json}
read -r url sha size < <(jq -r '.builds[] | select(.platform == "linux/amd64" and .variant == "gnu") | .files[0] | [.url, .sha256, (.size|tostring)] | @tsv' "$pin")
d=$(mktemp -d ~/.cache/s19-version.XXXXXX); trap 'rm -rf "$d"' EXIT
curl -fsSL --proto '=https' --tlsv1.2 -o "$d/claude" "$url"
got=$(sha256sum "$d/claude" | cut -d' ' -f1)
[[ $got == "$sha" && $(stat -c %s "$d/claude") == "$size" ]] || { echo "PIN_MISMATCH $got"; exit 1; }
echo "BINARY $url sha256=$got (as pinned)"
chmod 0755 "$d/claude"; mkdir -m 0700 "$d/home"
env -i HOME="$d/home" PATH=/usr/bin:/bin "$d/claude" --version >"$d/out" 2>"$d/err"; echo "VERSION_EXIT $?"
echo "VERSION_STDOUT_HEX $(od -An -tx1 -v "$d/out" | tr -s ' \n' ' ' | sed 's/^ //;s/ $//')"
echo "VERSION_STDOUT_TEXT $(cat "$d/out")"
echo "VERSION_STDERR_BYTES $(stat -c %s "$d/err")"
echo "HOME_ENTRIES_AFTER $(find "$d/home" -mindepth 1 | wc -l)"
find "$d/home" -mindepth 1 -printf 'HOME_ENTRY %P %s\n' | head -20
