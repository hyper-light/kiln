#!/usr/bin/env bash
# Step 5's kiln (plan 01a1cf48e8157494ac9a51f7a564f526; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 19): the
# `kiln` binary at REV, kiln main with step 19's steps 1 to 4 landed (Claude Code's row and `kiln agentfiles
# context`), to write the context step 5 builds. It is cargo's default (debug) build, from a clone of GitHub's kiln
# that fetches only the blobs REV needs. Its target goes under ~/.cache/s19-step5, a target of its own, as kiln's
# AGENTS.md asks of anything heavy beside other work.
#
#   build-kiln.sh > ~/.cache/s19-step5/build-kiln.log 2>&1
set -euo pipefail
REV=ce63fc099977323d8e27cb7a035234515a3c1c74
ROOT="$HOME/.cache/s19-step5"
SRC="$ROOT/kiln"
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
mkdir -p "$ROOT/tmp"
echo "# build-kiln, host $(hostname) $(uname -srm), at $(date -u +%FT%TZ)"
if [ ! -d "$SRC/.git" ]; then
    git clone -q --no-checkout --filter=blob:none https://github.com/hyper-light/kiln.git "$SRC"
fi
git -C "$SRC" cat-file -e "$REV^{commit}" 2>/dev/null || git -C "$SRC" fetch -q origin main
git -C "$SRC" checkout -q -f --detach "$REV"
git -C "$SRC" log -1 --format='kiln %H %cI'
test -z "$(git -C "$SRC" status --porcelain)"
cd "$SRC"
export TMPDIR="$ROOT/tmp" CARGO_TARGET_DIR="$ROOT/kiln-target" KILN_COMMIT="$REV"
unset DYLD_LIBRARY_PATH DYLD_FALLBACK_LIBRARY_PATH DYLD_INSERT_LIBRARIES
rustc --version
cargo --version
start=$(date +%s)
nice -n 10 cargo build --locked -p kiln --bin kiln -j "${JOBS:-16}"
echo "built in $(($(date +%s) - start)) s"
bin="$CARGO_TARGET_DIR/debug/kiln"
"$bin" --version
if command -v sha256sum >/dev/null; then sha256sum "$bin"; else shasum -a 256 "$bin"; fi
echo BUILD_DONE
