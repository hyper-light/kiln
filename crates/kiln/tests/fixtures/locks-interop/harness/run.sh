#!/usr/bin/env bash
# W29-AC20/AC22 (kiln plan 01a19a37, step 12): run the pinned gh actions-lock over every case and
# write what it wrote and found under $WORK/out. With --write, also copy those results into each
# case's expected/ directory, which kiln's tests compare against.
#
# Needs go, a clone of github/gh-actions-lock holding the pinned commit ($UPSTREAM), and the Go
# module proxy for its dependencies. No token reaches the CLI and its fake host answers from the
# cases' facts, so no request leaves the process.
set -euo pipefail

here=$(dirname "$(realpath "$0")")
cases=$(realpath "$here/../cases")
upstream=${UPSTREAM:-$HOME/.cache/kiln-w29/gh-actions-lock}
pin=113f91d77e0af03838b6ae3bef0e038cbab4c696
work=${WORK:-$HOME/.cache/kiln-locks-interop}

rm -rf "$work/src" "$work/out"
mkdir -p "$work/src" "$work/out"
git -C "$upstream" archive "$pin" | tar -x -C "$work/src"
cp "$here/interop_test.go" "$work/src/cmd/gh-actions-lock/kiln_interop_test.go"

env -u GH_TOKEN -u GITHUB_TOKEN -u GH_ENTERPRISE_TOKEN -u GITHUB_ENTERPRISE_TOKEN -u GH_HOST \
  GOMODCACHE="$work/mod" GOCACHE="$work/build" GOTOOLCHAIN=local \
  KILN_INTEROP_CASES="$cases" KILN_INTEROP_OUT="$work/out" \
  go -C "$work/src" test ./cmd/gh-actions-lock -run '^TestKilnInterop$' -count=1 -v

if [ "${1:-}" = --write ]; then
  for out in "$work"/out/*/; do
    case=$(basename "$out")
    rm -rf "$cases/$case/expected"
    cp -R "$out" "$cases/$case/expected"
  done
fi
