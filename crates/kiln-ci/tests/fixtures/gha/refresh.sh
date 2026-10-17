#!/bin/sh
# Refreshes F-03 (docs/ci.md Appendix F): GitHub's starter workflows, and up to 15 workflows of each
# repository listed in SOURCES.md. Sparse clones only (no API quota). Review the diff: a refresh
# that adds errors is a GitHub feature kiln doesn't read yet.
set -eu
out=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone -q --depth 1 https://github.com/actions/starter-workflows.git "$tmp/starter"
rm -rf "$out/starter-workflows" && mkdir -p "$out/starter-workflows"
(cd "$tmp/starter" && find automation ci code-scanning deployments pages -name '*.y*ml') | while read -r f; do
  cp "$tmp/starter/$f" "$out/starter-workflows/$(echo "$f" | tr / _)"
done
grep '^| `' "$out/SOURCES.md" | sed -E 's/^\| `([^`]+)`.*/\1/' | while read -r repo; do
  d="$tmp/$(echo "$repo" | tr / _)"
  git clone -q --depth 1 --filter=blob:none --sparse "https://github.com/$repo.git" "$d" && git -C "$d" sparse-checkout set .github/workflows >/dev/null 2>&1 || continue
  dest="$out/$(echo "$repo" | tr / _)"
  rm -rf "$dest" && mkdir -p "$dest"
  ls "$d/.github/workflows" | grep -E '\.ya?ml$' | sort | head -15 | while read -r f; do cp "$d/.github/workflows/$f" "$dest/$f"; done
  echo "$repo $(git -C "$d" rev-parse HEAD)"
done
echo "Update the commits in SOURCES.md with the lines above." >&2
