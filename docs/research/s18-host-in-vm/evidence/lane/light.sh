#!/bin/bash
cd ~/g121-tmp/s18/kiln
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
KILN_TEST_SHARDS=$HOME/g121-tmp/s18/shards-target/debug/shards nice -n 10 cargo test -p kiln --lib -- --test-threads 8 step18 step23_a_real_shards_checks_every_builtin agentfiles:: wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated harness::lineage mcp:: reach::mcp > ../light.log 2>&1
echo "TEST rc=$? $(date -u +%FT%TZ)"
grep -E '^test result|FAILED|^failures:' ../light.log | head -20
