#!/usr/bin/env bash
# Step 25's Devin tree pin (kiln plan 01a1ce705e4d7008993f5c7291ef4476, step 2): re-pin, pin tests, kiln tests, clippy.
set -uo pipefail
cd "$HOME/.cache/s25-devin-tree-pin/kiln"
export CARGO_TARGET_DIR=$HOME/.cache/s25-devin-tree-pin/target CARGO_BUILD_JOBS=16
step() {
    local name=$1
    shift
    echo "=== $name: $* ($(date -u +%FT%TZ))"
    "$@"
    local rc=$?
    echo "=== $name exit $rc ($(date -u +%FT%TZ))"
    echo "$name $rc" >>../results.txt
}
: >../results.txt
step pin-tests bash .kiln/images/harness-devin/pin-tests.sh
step pin bash .kiln/images/harness-devin/pin.sh 3000.12.43
step pin-verify bash .kiln/images/harness-devin/pin-tests.sh verify .kiln/images/harness-devin/pin.json
step test cargo test -p kiln --lib --locked agentfiles
step clippy cargo clippy -p kiln --all-targets --locked -- -D warnings
echo done >>../results.txt
