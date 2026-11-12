#!/usr/bin/env bash
# Step 3's checks (plan 01a1cf48e8157494ac9a51f7a564f526) at s19-claude-agentfile (751a7efb9) plus step3.patch, in the
# runner's kiln clone, reset first: the agentfiles and harness::claude tests, the live context (KILN_TEST_LIVE_CONTEXT,
# under ~/.cache), and clippy on crates/kiln.
set -uo pipefail
carrier=$PWD
K=$HOME/.cache/s19-kiln
export CARGO_TARGET_DIR=$HOME/.cache/s19-target
cd "$K" || exit 1
git reset -q --hard && git clean -fdq
git fetch -q "$carrier/s19.bundle" refs/heads/s19-claude-agentfile && git checkout -q -B s19-claude-agentfile FETCH_HEAD || exit 1
git apply "$carrier/step3.patch" || exit 1
echo "TREE $(git rev-parse HEAD) plus step3.patch $(sha256sum "$carrier/step3.patch" | cut -d' ' -f1)"; git status --short
echo "DISK $(df -h ~/.cache | tail -1 | awk '{print $4}') free; target $(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)"
live=$HOME/.cache/s19-live; mkdir -p "$live"
start=$(date +%s)
KILN_TEST_LIVE_CONTEXT=$live nice -n 10 cargo test -p kiln --lib -- agentfiles:: harness::claude:: --nocapture >"$carrier/step3-tests.log" 2>&1; echo "STEP3_TEST_EXIT $? after $(( $(date +%s) - start )) s"
grep -E '^test result|^test .*(step19|FAILED)|^MATCH |skipped:|panicked|^Error|error(\[|:)' "$carrier/step3-tests.log" | head -60
start=$(date +%s)
nice -n 10 cargo clippy -q -p kiln --all-targets -- -D warnings >"$carrier/step3-clippy.log" 2>&1; echo "STEP3_CLIPPY_EXIT $? after $(( $(date +%s) - start )) s"; tail -60 "$carrier/step3-clippy.log"
echo "DISK $(df -h ~/.cache | tail -1 | awk '{print $4}') free after; live left $(find "$live" -mindepth 1 | wc -l) entries"
