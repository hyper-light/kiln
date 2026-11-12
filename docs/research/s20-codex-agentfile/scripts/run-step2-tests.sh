#!/usr/bin/env bash
# Plan step 20 [kiln], step 2's tests on a Linux runner (kiln plan 01a1ce705d7076648c1d9f9ee26214d3):
# kiln at BASE, exported from the runner's own clone by git archive (read only), with this step's files
# laid over it from OVERLAY (a tar.gz of the worktree's changed files); then clippy on crates/kiln with
# -D warnings and the agentfiles module's tests, as .kiln/ci.yaml's rust job runs them. Every word goes
# into OUT/tests.txt, the exit codes into OUT/done.
#
#   run-step2-tests.sh OUT CLONE BASE OVERLAY
set -uo pipefail

out=${1:?usage: run-step2-tests.sh OUT CLONE BASE OVERLAY}
clone=${2:?usage: run-step2-tests.sh OUT CLONE BASE OVERLAY}
base=${3:?usage: run-step2-tests.sh OUT CLONE BASE OVERLAY}
overlay=${4:?usage: run-step2-tests.sh OUT CLONE BASE OVERLAY}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
src=$out/src
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$cache/s20-step2/target CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=4 CARGO_PROFILE_DEV_DEBUG=line-tables-only
mkdir -p "$src"
codes=()
{
    echo "# Plan step 20 [kiln] step 2's tests on $(hostname), $(uname -sr) $(uname -m), $(date -u +%FT%TZ)"
    echo "# kiln $(git -C "$clone" rev-parse "$base^{commit}") (git archive of $clone), overlay sha256 $(sha256sum "$overlay" | cut -d' ' -f1)"
    git -C "$clone" archive --format=tar "$base" | tar -x -C "$src"
    codes+=("archive=$?")
    tar -xzf "$overlay" -C "$src"
    codes+=("overlay=$?")
    (cd "$src" && sha256sum .kiln/images/harness-codex/* crates/kiln/src/agentfiles/builtin.rs crates/kiln/src/agentfiles/builtin/codex_tests.rs Dockerfile .dockerignore)
    cargo --version
    rustc --version
    echo "## cargo clippy --locked -p kiln --all-targets -- -D warnings"
    (cd "$src" && KILN_COMMIT=$(git -C "$clone" rev-parse "$base^{commit}") nice -n 10 cargo clippy --locked -p kiln --all-targets -- -D warnings)
    codes+=("clippy=$?")
    echo "## cargo test --locked -p kiln --lib agentfiles"
    (cd "$src" && KILN_COMMIT=$(git -C "$clone" rev-parse "$base^{commit}") nice -n 10 cargo test --locked -p kiln --lib agentfiles)
    codes+=("test=$?")
    echo "# exits: ${codes[*]}"
    echo "# done $(date -u +%FT%TZ)"
} >"$out/tests.txt" 2>&1
echo "${codes[*]}" >"$out/done"
