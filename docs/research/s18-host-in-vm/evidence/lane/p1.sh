#!/bin/bash
# Step 18 part 1's landing checks on the merged tree (light tests, then clippy), the step's lane.
cd ~/g121-tmp/s18/kiln || exit 2
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
KILN_TEST_SHARDS=$HOME/g121-tmp/s18/shards-target/debug/shards nice -n 10 cargo test -j 8 -p kiln --lib -- --test-threads 8 step18 step23_a_real_shards_checks_every_builtin agentfiles:: wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated harness::lineage mcp:: reach::mcp > ../p1-light.log 2>&1
echo "TEST rc=$? $(date -u +%FT%TZ)"
grep -E '^test result|FAILED|panicked' ../p1-light.log | head -20
nice -n 10 cargo clippy --locked -j 8 -p kiln -p kiln-ci --all-targets --message-format short -- -D warnings > ../p1-clippy.log 2>&1
echo "CLIPPY rc=$? $(date -u +%FT%TZ)"
grep -E '^(warning|error)' ../p1-clippy.log | sort | uniq -c | head -20
