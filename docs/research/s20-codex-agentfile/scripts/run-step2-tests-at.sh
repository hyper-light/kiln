#!/usr/bin/env bash
# Plan step 20 [kiln], step 2's tests at one commit (kiln plan 01a1ce705d7076648c1d9f9ee26214d3), on a
# Linux runner: a --shared clone of the runner's own kiln checkout (read only), the branch fetched
# from BUNDLE. At HEAD (the bundle's branch) it runs:
# - clippy -D warnings on crates/kiln;
# - the agentfiles module's tests;
# - main's real-Shards check of every built-in (KILN_TEST_SHARDS: step 23's Shards v0.7.0, held to its
#   SHA256SUMS);
# - the agentfiles integration test, at HEAD and at MAIN;
# - two source mutations, each then undone.
# Every word goes into OUT/tests.txt, the exit codes into OUT/done.
#
#   run-step2-tests-at.sh OUT BUNDLE MAIN
set -u

out=${1:?usage: run-step2-tests-at.sh OUT BUNDLE MAIN}
bundle=${2:?usage: run-step2-tests-at.sh OUT BUNDLE MAIN}
main=${3:?usage: run-step2-tests-at.sh OUT BUNDLE MAIN}
cache=${XDG_CACHE_HOME:-$HOME/.cache}
clone=$cache/s20-step2/kiln
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$cache/s20-step2/target CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=4 CARGO_PROFILE_DEV_DEBUG=line-tables-only
mkdir -p "$out"
codes=()
quiet() { grep --line-buffered -v -E '^\s+(Compiling|Checking|Downloaded|Downloading|Locking|Adding|Updating) '; }
step() {
    local name=$1
    shift
    echo "## $name: $* (from $(date -u +%T))"
    (cd "$clone" && nice -n 10 "$@") 2>&1 | quiet
    local rc=${PIPESTATUS[0]}
    echo "${name}_EXIT=$rc at $(date -u +%T)"
    codes+=("$name=$rc")
}
at() { git -C "$clone" checkout -q -f --detach "$1" && echo "# HEAD $(git -C "$clone" rev-parse HEAD): $(git -C "$clone" log -1 --format=%s | cut -c1-110)"; }
{
    echo "# Plan step 20 [kiln] step 2's tests on $(hostname), $(uname -sr) $(uname -m), $(date -u +%FT%TZ), $(cargo --version)"
    [ -d "$clone/.git" ] || git clone -q --shared --no-checkout "$HOME/Projects/kiln" "$clone"
    git -C "$clone" bundle verify "$bundle" >/dev/null 2>&1 || { echo "BUNDLE_UNVERIFIED"; exit 1; }
    git -C "$clone" fetch -q "$bundle" "+s20-codex-agentfile:refs/remotes/bundle/s20-codex-agentfile"
    head=$(git -C "$clone" rev-parse refs/remotes/bundle/s20-codex-agentfile)
    at "$head"
    git -C "$clone" cat-file -e "$main^{commit}" && echo "# MAIN $(git -C "$clone" rev-parse "$main^{commit}")"
    step CLIPPY cargo clippy --locked -p kiln --all-targets -- -D warnings
    step UNIT cargo test --locked -p kiln --lib agentfiles
    S=$cache/s23-shards-0.7.0
    if (cd "$S" && grep ' shards-linux-amd64$' SHA256SUMS | sha256sum -c - >/dev/null 2>&1); then
        echo "SHARDS $("$S/shards-linux-amd64" --version 2>&1 | head -1), sha256 $(sha256sum "$S/shards-linux-amd64" | cut -d' ' -f1), as v0.7.0's SHA256SUMS lists it"
        export KILN_TEST_SHARDS=$S/shards-linux-amd64
        step SHARDS cargo test --locked -p kiln --lib real_shards -- --nocapture
        unset KILN_TEST_SHARDS
    else
        echo "SHARDS_UNCHECKED: $S holds no shards matching its SHA256SUMS"
        codes+=("SHARDS=unchecked")
    fi
    step INTEG cargo test --locked -p kiln --test agentfiles
    # Mutations, each alone at HEAD: the test that must fail, then the tree as it was.
    sed -i 's|^    vec!\["app-server".to_owned()\]$|    vec!["app-server".to_owned(), "--strict".to_owned()]|' "$clone/crates/kiln/src/harness/codex.rs"
    echo "M1 diff: $(git -C "$clone" diff --numstat -- crates/kiln/src/harness/codex.rs)"
    step M1 cargo test --locked -p kiln --lib agentfiles::builtin::codex_tests::codex_harness_json
    at "$head"
    printf 'ENV OPENAI_API_KEY=x\n' >>"$clone/.kiln/images/harness-codex/Agentfile"
    echo "M2 diff: $(git -C "$clone" diff --numstat -- .kiln/images/harness-codex/Agentfile)"
    step M2 cargo test --locked -p kiln --lib agentfiles::builtin
    at "$head"
    touch "$clone/crates/kiln/src/harness/codex.rs" "$clone/.kiln/images/harness-codex/Agentfile"
    # kiln main as it is, whose surface test this branch's third commit fixes.
    at "$main"
    step INTEG_MAIN cargo test --locked -p kiln --test agentfiles step30_agentfiles_on_every_surface
    at "$head"
    echo "# exits: ${codes[*]}"
    echo "# done $(date -u +%FT%TZ), tree clean: $(git -C "$clone" status --porcelain | wc -l) changes"
} >"$out/tests.txt" 2>&1
echo "${codes[*]}" >"$out/done"
