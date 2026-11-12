#!/usr/bin/env bash
# Step 2's checks (plan 01a1cf48e8157494ac9a51f7a564f526) at s19-claude-agentfile plus this step's patch,
# in a kiln clone of the runner's own under ~/.cache: the agentfiles and harness::claude tests (with a real
# shards where one is installed) and clippy on crates/kiln.
set -euo pipefail
carrier=$PWD
K=$HOME/.cache/s19-kiln
export CARGO_TARGET_DIR=$HOME/.cache/s19-target
if [[ ! -d $K/.git ]]; then git clone -q https://github.com/hyper-light/kiln.git "$K"; fi
cd "$K"
git fetch -q origin main
git cat-file -e 6fa9543635570e17c96fe8502c9db8e16e634003^{commit}
git fetch -q "$carrier/s19.bundle" refs/heads/s19-claude-agentfile
git checkout -q -B s19-claude-agentfile FETCH_HEAD
git reset -q --hard FETCH_HEAD
git clean -fdq
git apply "$carrier/step2.patch"
echo "TREE $(git rev-parse HEAD) plus step2.patch $(sha256sum "$carrier/step2.patch" | cut -d' ' -f1)"
git status --short
echo "TOOLS $(cargo --version) / $(rustc --version)"
if command -v shards >/dev/null; then export KILN_TEST_SHARDS=$(command -v shards); echo "SHARDS $KILN_TEST_SHARDS $(shards --version 2>&1 | head -1)"; else echo "SHARDS none"; fi
start=$(date +%s)
set +e
nice -n 10 cargo test -q -p kiln --lib -- agentfiles:: harness::claude:: >"$carrier/test.log" 2>&1; echo "TEST_EXIT $? after $(( $(date +%s) - start )) s"
grep -E '^test |test result|panicked|FAILED|error(\[|:)' "$carrier/test.log" | tail -80
start=$(date +%s)
nice -n 10 cargo clippy -q -p kiln --all-targets -- -D warnings >"$carrier/clippy.log" 2>&1; echo "CLIPPY_EXIT $? after $(( $(date +%s) - start )) s"
tail -40 "$carrier/clippy.log"
