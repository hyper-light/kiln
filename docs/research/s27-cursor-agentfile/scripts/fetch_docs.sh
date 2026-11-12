#!/bin/bash
# Fetch each Cursor docs page into its own empty directory; record HTTP status and size.
set -u
ROOT="$1"; shift
for p in "$@"; do
  d="$ROOT/$(echo "$p" | tr '/' '_')"
  mkdir -p "$d"
  code=$(curl -sS -L -A 'Mozilla/5.0 (compatible; research-fetch)' -o "$d/page.html" -w '%{http_code} %{url_effective}' "https://cursor.com/docs/$p")
  echo "$p -> $code $(wc -c < "$d/page.html")"
  code2=$(curl -sS -L -A 'Mozilla/5.0 (compatible; research-fetch)' -o "$d/page.md" -w '%{http_code} %{content_type}' "https://cursor.com/docs/$p.md")
  echo "   .md -> $code2 $(wc -c < "$d/page.md")"
done
