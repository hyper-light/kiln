#!/usr/bin/env bash
# Step 5's shards (plan 01a1cf48e8157494ac9a51f7a564f526; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19): the
# whole `shards`, carrying its VM and network processes and its guest, built from Shards main at REV the way step 23
# built its own (kiln main, docs/research/s23-goose-agentfile/step4/build-shards.sh):
# - Shards' pinned Rust (rust-toolchain.toml), with both musl targets for the guest;
# - zig 0.16.0, checked by digest (scripts/install-zig);
# - the test-release profile (release without fat LTO), and no GPU renderer.
#
# REV holds 1da9132b7 (a harness starts from its harness.json) and 997f4e47e (a harness.json's run.env sets its
# HOME and PATH). v0.7.0 has neither, and no release carries them until ROOT cuts v0.8.0. SHARDS_GIT_COMMIT is
# REV's first seven characters, as release.yml sets it, so `shards --version` names the build. Everything goes
# under ~/.cache/s19-step5, never /tmp, which kiln's Linux machines keep in RAM.
#
#   build-shards.sh > ~/.cache/s19-step5/build-shards.log 2>&1
set -euo pipefail
REV=7ad28fab136fb72cac910de9c0f4bfc1db2d81f9
ROOT="$HOME/.cache/s19-step5"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
mkdir -p "$ROOT/tmp"
echo "# build-shards, host $(hostname) $(uname -srm), at $(date -u +%FT%TZ)"
if [ ! -d "$ROOT/shards/.git" ]; then
    git init -q "$ROOT/shards"
    git -C "$ROOT/shards" remote add origin https://github.com/hyper-light/shards
fi
git -C "$ROOT/shards" fetch -q --depth 1 origin "$REV"
git -C "$ROOT/shards" checkout -q -f --detach "$REV"
git -C "$ROOT/shards" log -1 --format='shards %H %cI'
test -z "$(git -C "$ROOT/shards" status --porcelain)"
cd "$ROOT/shards"
export TMPDIR="$ROOT/tmp"
zig=$(scripts/install-zig)
export PATH="$zig:$PATH"
echo "zig $(zig version) at $zig"
# rust-lld loads libLLVM only without DYLD_* (Shards' CLAUDE.md); the GPU's renderer and prebuilt helpers stay out.
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_INSERT_LIBRARIES
unset SHARDS_GPU_LIBS SHARDS_HELPERS SHARDS_HELPERS_DIR SHARDS_INIT_BINARY SHARDS_SERVER_BINARY
export CARGO_TARGET_DIR="$ROOT/shards-target" SHARDS_GIT_COMMIT="${REV:0:7}"
rustc --version
cargo --version
start=$(date +%s)
nice -n 10 cargo build --locked --profile test-release -p shards -j "${JOBS:-16}"
echo "built in $(($(date +%s) - start)) s"
bin="$CARGO_TARGET_DIR/test-release/shards"
"$bin" --version
if command -v sha256sum >/dev/null; then sha256sum "$bin"; else shasum -a 256 "$bin"; fi
echo BUILD_DONE
