#!/usr/bin/env bash
# Step 1's test (plan 01a1cf48e8157494ac9a51f7a564f526): every build a pin.json pins downloads from
# its URL with the SHA-256 and size the pin, and so the signed manifest, gives it. Downloads go under
# ~/.cache, one at a time, each removed once hashed.
#
#   docs/research/s19-claude-agentfile/step1-check-downloads.sh PIN.json

set -euo pipefail
pin=${1:?pin.json to check}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache"
d=$(mktemp -d "$cache/s19-check-downloads.XXXXXX")
trap 'rm -rf "$d"' EXIT

n=0
ok=0
while IFS=$'\t' read -r platform variant url sha size; do
    n=$((n + 1))
    f="$d/$n"
    curl -fsSL --proto '=https' --tlsv1.2 -o "$f" "$url"
    got_sha=$(sha256sum "$f" | cut -d' ' -f1)
    got_size=$(stat -c %s "$f")
    if [[ $got_sha == "$sha" && $got_size == "$size" ]]; then
        ok=$((ok + 1))
        echo "MATCH $platform $variant sha256=$got_sha size=$got_size $url"
    else
        echo "MISMATCH $platform $variant sha256=$got_sha (pin $sha) size=$got_size (pin $size) $url"
    fi
    rm -f "$f"
done < <(jq -r '.builds[] | .platform as $p | (.variant // "") as $v | .files[] | [$p, $v, .url, .sha256, (.size | tostring)] | @tsv' "$pin")
echo "DOWNLOADS_MATCHING $ok of $n"
[[ $ok == "$n" && $n == 4 ]]
