#!/bin/bash
# Downloads each pinned linux/amd64 artifact (args: NAME URL SHA256 ...), checks its SHA-256, unpacks
# tarballs, and prints the hosts its files name: every https?:// URL's host, and bare names under the
# vendors' own domains, each with how many times it appears.
set -u
root=~/g121-tmp/s18/bins
mkdir -p "$root"
while [ $# -ge 3 ]; do
  name=$1 url=$2 sum=$3; shift 3
  d="$root/$name"; mkdir -p "$d"; f="$d/$(basename "$url")"
  [ -s "$f" ] || nice -n 10 curl -sSL --max-time 900 -o "$f" "$url"
  got=$(sha256sum "$f" | cut -d' ' -f1)
  echo "== $name $(basename "$url") sha256=$got $( [ "$got" = "$sum" ] && echo MATCH || echo MISMATCH)"
  [ "$got" = "$sum" ] || continue
  case "$f" in *.tar.gz) mkdir -p "$d/x" && tar -xzf "$f" -C "$d/x" ;; esac
done
for d in "$root"/*; do
  name=$(basename "$d")
  echo "#### $name: hosts in https?:// URLs (count host)"
  find "$d" -type f ! -name '*.tar.gz' -print0 | xargs -0 cat | grep -aoE 'https?://[A-Za-z0-9.-]+\.[A-Za-z]{2,}' | sed -E 's#https?://##' | tr 'A-Z' 'a-z' | sort | uniq -c | sort -rn | head -150
  echo "#### $name: bare names under vendor domains"
  find "$d" -type f ! -name '*.tar.gz' -print0 | xargs -0 cat | grep -aoE '\b[a-z0-9.-]+\.(ampcode\.com|sourcegraph\.com|codeium\.com|windsurf\.com|devin\.ai|cognition\.ai|cursor\.sh|cursor\.com|cursorapi\.com|googleapis\.com|antigravity\.google)\b' | sort | uniq -c | sort -rn | head -80
done
echo "SCAN DONE"
