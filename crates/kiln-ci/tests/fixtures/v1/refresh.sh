#!/bin/sh
# Refreshes F-02 (docs/ci.md Appendix F): every hyper-light public repository's .kiln/*.yaml.
# One API call to list the repositories, then a sparse clone of each (no API quota).
set -eu
out=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "https://api.github.com/orgs/hyper-light/repos?per_page=100&type=public" > "$tmp/repos.json"
python3 -I -c 'import json, sys; [print(r["name"]) for r in json.load(open(sys.argv[1])) if not r.get("archived")]' "$tmp/repos.json" > "$tmp/repos.txt"
while read -r r; do
  d="$tmp/clones/$r"
  if git clone -q --depth 1 --filter=blob:none --sparse "https://github.com/hyper-light/$r.git" "$d" 2>/dev/null; then
    git -C "$d" sparse-checkout set --no-cone '/.kiln/*.yaml' '/.kiln/*.yml' >/dev/null 2>&1
    if ls "$d"/.kiln/*.y*ml >/dev/null 2>&1; then
      rm -rf "${out:?}/$r" && mkdir -p "$out/$r" && cp "$d"/.kiln/*.y*ml "$out/$r/"
      echo "| \`hyper-light/$r\` | \`$(git -C "$d" rev-parse HEAD)\` |"
    fi
  fi
done < "$tmp/repos.txt"
echo "Update the table in SOURCES.md with the lines above." >&2
