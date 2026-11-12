#!/usr/bin/env bash
# A check-only `shards` from Shards main, on a Linux runner, for the Agentfile check of kiln plan
# 01a1ce705e4d7008993f5c7291ef4476 step 3 (`shards build --call=check,format=json`). Main at or
# after 1da9132b7 (v0.7.0 lacks harness.json startup). Everything under ~/.cache/s25-step3.
#
# A static check boots no guest, so this shards carries no VM or network process
# (SHARDS_HELPERS=skip, as scripts/lint builds one) and names a placeholder file for its two
# guests (SHARDS_INIT_BINARY, SHARDS_SERVER_BINARY: crates/shards/build.rs). The real images
# and smokes are step 4's, with a full shards.
set -euo pipefail
REV=ac87190d3ff78245e9fa9726c81a7392797bce30
ROOT="$HOME/.cache/s25-step3"
mkdir -p "$ROOT"
echo "host $(hostname) $(uname -srm) at $(date -u +%FT%TZ)"
if [ ! -d "$ROOT/shards/.git" ]; then
  git init -q "$ROOT/shards"
  git -C "$ROOT/shards" remote add origin https://github.com/hyper-light/shards
fi
git -C "$ROOT/shards" fetch -q --depth 1 origin "$REV"
git -C "$ROOT/shards" checkout -q --detach "$REV"
git -C "$ROOT/shards" log -1 --format='shards %H %cI'
git -C "$ROOT/shards" merge-base --is-ancestor 1da9132b7 HEAD 2>/dev/null \
  && echo "1da9132b7 is an ancestor" || echo "ancestry of 1da9132b7 not visible in a depth-1 clone (checked on the Mac: git merge-base --is-ancestor 1da9132b7 $REV)"
cd "$ROOT/shards"
printf 'placeholder: a check-only shards boots no guest\n' > "$ROOT/placeholder-guest"
export SHARDS_HELPERS=skip SHARDS_INIT_BINARY="$ROOT/placeholder-guest" SHARDS_SERVER_BINARY="$ROOT/placeholder-guest"
export CARGO_TARGET_DIR="$ROOT/target"
rustc --version
cargo --version
start=$(date +%s)
nice -n 10 cargo build -j 16 --locked -p shards 2>&1 | tail -25
echo "built in $(( $(date +%s) - start )) s"
"$CARGO_TARGET_DIR/debug/shards" --version
sha256sum "$CARGO_TARGET_DIR/debug/shards"
