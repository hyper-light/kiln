#!/usr/bin/env bash
# Plan step 26, step 4, phase A: a shards from Shards main at or after 997f4e47e (run.env HOME) and
# 1da9132b7 (harness.json startup), for the microVM build and smokes. Its commit is named at build
# (SHARDS_GIT_COMMIT, as release.yml names it), so `shards version` says it. Profile test-release:
# release's optimizations without fat LTO (Shards' own gates build so; this is a smoke, not a
# measurement of shards). Everything under ~/.cache/s26-agy-step4.
#   build-shards.sh FULL_SHA
set -uo pipefail
REV=$1
W="$HOME/.cache/s26-agy-step4"
mkdir -p "$W" || exit 1
echo "host $(hostname) $(uname -srm) at $(date -u +%FT%TZ)"
if [ ! -d "$W/shards/.git" ]; then
  git init -q "$W/shards" && git -C "$W/shards" remote add origin https://github.com/hyper-light/shards.git || exit 1
fi
git -C "$W/shards" fetch -q --depth 1 origin "$REV" || exit 1
git -C "$W/shards" checkout -q --force --detach "$REV" || exit 1
echo "shards source $(git -C "$W/shards" rev-parse HEAD) $(git -C "$W/shards" log -1 --format=%cI)"
cd "$W/shards" || exit 1
export SHARDS_GIT_COMMIT=${REV:0:7} CARGO_TARGET_DIR="$W/shards-target" PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
# zig 0.16.0 as Shards pins it: the host's own where it is that version, else scripts/install-zig's,
# its digest checked, into this directory.
if [ "$(zig version 2>/dev/null)" != 0.16.0 ]; then
  zigdir=$(RUNNER_TEMP="$W" sh scripts/install-zig) || exit 1
  export PATH="$zigdir:$PATH"
fi
echo "$(rustup show active-toolchain 2>&1 | head -1); $(zig version 2>&1)"
start=$(date +%s)
nice -n 10 cargo build --locked --profile test-release -p shards -j 8 > "$W/build-shards.log" 2>&1; rc=$?
echo "cargo build exit $rc in $(( $(date +%s) - start )) s"
tail -15 "$W/build-shards.log"
[ "$rc" = 0 ] || exit 1
bin="$CARGO_TARGET_DIR/test-release/shards"
echo "shards sha256 $( (sha256sum "$bin" 2>/dev/null || shasum -a 256 "$bin") | cut -d' ' -f1), $(wc -c < "$bin" | tr -d ' ') bytes"
h=$(mktemp -d "$W/version-home.XXXXXX")
SHARDS_HOME="$h" perl -e 'alarm 30; exec @ARGV' "$bin" version 2>&1 | head -8
rm -rf "$h"
du -sh "$CARGO_TARGET_DIR" 2>/dev/null
