#!/usr/bin/env bash
# Plan step 27, step 2: clippy on crates/kiln through kiln's gate (ci/gate.sh: cargo clippy --locked
# -p kiln --all-targets -- -D warnings), held to what it asks, on a runner (kiln run).
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH" KILN_UI_SERVER=off
echo "== host: $(uname -srm) $(hostname) $(date -u +%Y-%m-%dT%H:%M:%SZ)"
rustc --version; cargo --version; cargo clippy --version
echo "== tree: $(git rev-parse HEAD)"; git status --short
for f in crates/kiln/src/agentfiles/unpack.rs crates/kiln/src/agentfiles/unpack/tests.rs crates/kiln/src/agentfiles/mod.rs; do
  echo "== $f $( (sha256sum "$f" 2>/dev/null || shasum -a 256 "$f") | cut -d' ' -f1)"
done
ci/gate.sh -p kiln --cpus "${CPUS:-4}" --mem "${MEM:-12g}" --target-dir "$HOME/.cache/s27-unpack/target" clippy 2>&1
echo "== gate exit: $?"
