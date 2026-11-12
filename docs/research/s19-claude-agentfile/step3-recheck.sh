#!/usr/bin/env bash
# Step 3's recheck after clippy's cmp_owned (plan 01a1cf48e8157494ac9a51f7a564f526): 751a7efb9 plus step3.patch,
# the agentfiles and harness::claude tests (live context skipped: r7 ran it), then clippy on crates/kiln.
set -uo pipefail
carrier=$PWD
K=$HOME/.cache/s19-kiln
export CARGO_TARGET_DIR=$HOME/.cache/s19-target
cd "$K" || exit 1
git reset -q --hard && git clean -fdq
git fetch -q "$carrier/s19.bundle" refs/heads/s19-claude-agentfile && git checkout -q -B s19-claude-agentfile FETCH_HEAD || exit 1
git apply "$carrier/step3.patch" || exit 1
echo "TREE $(git rev-parse HEAD) plus step3.patch $(sha256sum "$carrier/step3.patch" | cut -d' ' -f1)"
nice -n 10 cargo test -p kiln --lib -- agentfiles:: harness::claude:: >"$carrier/recheck-tests.log" 2>&1; echo "TEST_EXIT $?"; grep -E '^test result' "$carrier/recheck-tests.log"
nice -n 10 cargo clippy -q -p kiln --all-targets -- -D warnings >"$carrier/recheck-clippy.log" 2>&1; echo "CLIPPY_EXIT $?"; tail -30 "$carrier/recheck-clippy.log"
