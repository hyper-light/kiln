#!/usr/bin/env bash
# Plan step 27, step 2: the unpack module's unit tests on a runner (through kiln run, this worktree
# synced), cargo set as .kiln/ci.yaml's rust job sets it. Usage: run-tests.sh [cargo test filter]
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="$HOME/.cache/s27-unpack/target"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" CARGO_PROFILE_DEV_DEBUG=line-tables-only KILN_UI_SERVER=off
echo "== host: $(uname -srm) $(hostname) $(date -u +%Y-%m-%dT%H:%M:%SZ)"
rustc --version; cargo --version
echo "== tree: $(git rev-parse HEAD)"; git status --short
for f in crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/mod.rs; do
  echo "== $f $( (sha256sum "$f" 2>/dev/null || shasum -a 256 "$f") | cut -d' ' -f1)"
done
nice -n 10 cargo test --locked -p kiln --lib "${1:-agentfiles::unpack}" 2>&1
echo "== cargo test exit: $?"
