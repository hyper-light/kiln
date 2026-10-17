#!/bin/sh
# Re-fetches GitHub's expression test vectors (docs/ci.md W01-AC01).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone -q --depth 1 https://github.com/actions/languageservices.git "$tmp/ls"
rm -f "$here"/testdata/*.json
cp "$tmp"/ls/expressions/testdata/*.json "$here/testdata/"
cp "$tmp/ls/LICENSE" "$here/LICENSE"
echo "now at $(git -C "$tmp/ls" rev-parse HEAD): update README.md"
