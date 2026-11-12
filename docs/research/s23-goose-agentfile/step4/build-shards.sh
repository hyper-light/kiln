#!/usr/bin/env bash
# Step 4's shards (plan 01a1ce705e3d72af83ef15ae83b62f45): the whole `shards`, carrying its VM and
# network processes and its guest, built from Shards main at REV. REV holds 1da9132b7 (a harness
# starts from its harness.json; v0.7.0 lacks it) and 997f4e47e (a harness.json's run.env sets its
# HOME and PATH). Built as Shards' release.yml builds it: Shards' pinned Rust (rust-toolchain.toml)
# with both musl targets for the guest, zig 0.16.0 by digest (scripts/install-zig), but with the
# test-release profile (release without fat LTO) and no GPU renderer (SHARDS_GPU_LIBS unset, so it
# carries no GPU VM process). Everything goes under ~/.cache/s23-step4, never /tmp (kiln's Linux
# machines keep /tmp in RAM).
#
#   build-shards.sh > ~/.cache/s23-step4/build.log 2>&1
set -euo pipefail
REV=8f3cd5b7411dfeea47956a5143eec2c3466b058f
ROOT="$HOME/.cache/s23-step4"
mkdir -p "$ROOT/tmp"
echo "# build-shards, host $(hostname) $(uname -srm), at $(date -u +%FT%TZ)"
if [ ! -d "$ROOT/shards/.git" ]; then
    git init -q "$ROOT/shards"
    git -C "$ROOT/shards" remote add origin https://github.com/hyper-light/shards
fi
git -C "$ROOT/shards" fetch -q --depth 1 origin "$REV"
git -C "$ROOT/shards" checkout -q -f --detach "$REV"
git -C "$ROOT/shards" log -1 --format='shards %H %cI'
cd "$ROOT/shards"
export TMPDIR="$ROOT/tmp"
zig=$(scripts/install-zig)
export PATH="$zig:$PATH"
echo "zig $(zig version) at $zig"
# rust-lld loads libLLVM only without DYLD_* (Shards' CLAUDE.md); the GPU's renderer and prebuilt
# helpers stay out.
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_INSERT_LIBRARIES
unset SHARDS_GPU_LIBS SHARDS_HELPERS SHARDS_HELPERS_DIR SHARDS_INIT_BINARY SHARDS_SERVER_BINARY
export CARGO_TARGET_DIR="$ROOT/target" SHARDS_GIT_COMMIT="${REV:0:7}"
rustc --version
cargo --version
start=$(date +%s)
nice -n 10 cargo build --locked --profile test-release -p shards -j 16
echo "built in $(($(date +%s) - start)) s"
bin="$CARGO_TARGET_DIR/test-release/shards"
"$bin" --version
if command -v sha256sum >/dev/null; then sha256sum "$bin"; else shasum -a 256 "$bin"; fi
echo BUILD_DONE
