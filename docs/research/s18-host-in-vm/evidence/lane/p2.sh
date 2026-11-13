#!/bin/bash
# Step 18 part 2's landing checks (light tests, the port-derivation guard's mutant, clippy), the step's lane.
cd ~/g121-tmp/s18/kiln || exit 2
export CARGO_TARGET_DIR=~/g121-tmp/s18/target
T=agentfiles::builtin::grants_tests::step18_every_builtin_names_its_hosts_and_keeps_its_home_on_its_volume
echo "START $(date -u +%FT%TZ) $(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l)"
KILN_TEST_SHARDS=$HOME/g121-tmp/s18/shards-target/debug/shards nice -n 10 cargo test -j 8 -p kiln --lib -- --test-threads 8 step18 step23_a_real_shards_checks_every_builtin agentfiles:: wire::ts harness::homes harness::whose harness::moving harness::trouble harness::isolated harness::lineage mcp:: reach::mcp > ../p2-light.log 2>&1
echo "TEST rc=$? $(date -u +%FT%TZ)"
grep -E '^test result|FAILED|panicked' ../p2-light.log | head -20
# The guard's mutant (R10): kiln's default http_addr port moved; every image still grants 7070.
grep -c 'http_addr: "0.0.0.0:7070".into(),' crates/kiln/src/config.rs
sed -i 's|http_addr: "0.0.0.0:7070".into(),|http_addr: "0.0.0.0:7071".into(),|' crates/kiln/src/config.rs
echo "MUTANT $(git diff --stat | tail -1)"
nice -n 10 cargo test -j 8 -p kiln --lib -- --exact $T > ../p2-mutant.log 2>&1
echo "MUTANT TEST rc=$? $(date -u +%FT%TZ) (a failure is the guard holding)"
grep -E '^test |panicked|Error:|NETWORK lines' ../p2-mutant.log | cut -c1-400 | head -8
git checkout -- crates/kiln/src/config.rs && touch crates/kiln/src/config.rs
echo "RESTORED dirty=$(git status --porcelain | wc -l)"
nice -n 10 cargo test -j 8 -p kiln --lib -- --exact $T > ../p2-green.log 2>&1
echo "GREEN TEST rc=$? $(date -u +%FT%TZ)"
grep -E '^test result' ../p2-green.log
nice -n 10 cargo clippy --locked -j 8 -p kiln -p kiln-ci --all-targets --message-format short -- -D warnings > ../p2-clippy.log 2>&1
echo "CLIPPY rc=$? $(date -u +%FT%TZ)"
grep -E '^(warning|error)' ../p2-clippy.log | sort | uniq -c | head -20
