#!/usr/bin/env bash
# Step 4's pre-landing check (plan 01a1cf48e8157494ac9a51f7a564f526): s19-claude-agentfile's head, exactly, in the
# runner's kiln clone: every agentfiles and harness::claude test, with the real Shards (v0.7.0, checked against its
# SHA256SUMS before) and the live context on, then clippy on crates/kiln.
set -uo pipefail
carrier=$PWD
K=$HOME/.cache/s19-kiln
export CARGO_TARGET_DIR=$HOME/.cache/s19-target
cd "$K" || exit 1
git reset -q --hard && git clean -fdq
git fetch -q "$carrier/s19.bundle" refs/heads/s19-claude-agentfile && git checkout -q -B s19-claude-agentfile FETCH_HEAD || exit 1
echo "TREE $(git rev-parse HEAD) clean: $(git status --porcelain | wc -l) changes"
shards=$HOME/.cache/s19-shards/shards
echo "SHARDS $(sha256sum "$shards" | cut -d' ' -f1) $("$shards" --version 2>&1 | head -1)"
live=$HOME/.cache/s19-live; mkdir -p "$live"
start=$(date +%s)
KILN_TEST_SHARDS=$shards KILN_TEST_LIVE_CONTEXT=$live nice -n 10 cargo test -p kiln --lib -- agentfiles:: harness::claude:: --nocapture >"$carrier/step4-tests.log" 2>&1; echo "TEST_EXIT $? after $(( $(date +%s) - start )) s"
grep -E '^test result|^test .*(step19|step23|every_builtin|FAILED)|^MATCH |skipped:|panicked|^Error' "$carrier/step4-tests.log" | head -60
start=$(date +%s)
nice -n 10 cargo clippy -q -p kiln --all-targets -- -D warnings >"$carrier/step4-clippy.log" 2>&1; echo "CLIPPY_EXIT $? after $(( $(date +%s) - start )) s"; tail -40 "$carrier/step4-clippy.log"
echo "DISK $(df -h ~/.cache | tail -1 | awk '{print $4}') free; live left $(find "$live" -mindepth 1 | wc -l) entries"
