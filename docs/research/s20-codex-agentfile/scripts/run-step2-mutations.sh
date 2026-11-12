#!/usr/bin/env bash
# Plan step 20 [kiln], step 2's source mutations (kiln plan 01a1ce705d7076648c1d9f9ee26214d3), in the
# tree run-step2-tests.sh built (OUT/src, its target directory warm): each change alone, then the test
# it must fail, then the file restored (and touched, so cargo builds it again):
# - codex::args() gains an argument: harness.json no longer runs what kiln starts for Kind codex;
# - the embedded Agentfile gains ENV OPENAI_API_KEY=x: the secrets scan, and the Agentfile's exact
#   instruction list, refuse it.
# A mutant passes when its test run exits non-zero with a failed test; the line after it names that test.
# Then the untouched tree's test runs again and must pass.
#
#   run-step2-mutations.sh OUT
set -uo pipefail

out=${1:?usage: run-step2-mutations.sh OUT}
src=$out/src
cache=${XDG_CACHE_HOME:-$HOME/.cache}
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR=$cache/s20-step2/target CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=4 CARGO_PROFILE_DEV_DEBUG=line-tables-only
tests=agentfiles::builtin::codex_tests
passed=0
failed=0

# LABEL FILE OLD NEW TEST: FILE (under src) with OLD, found once, made NEW; TEST run; FILE restored.
mutant() {
    local label=$1 file=$2 old=$3 new=$4 test=$5 rc verdict=FAIL log=$out/mutant-$1.log
    cp "$src/$file" "$out/orig"
    if ! python3 -I -c 'import sys
p, o, n = sys.argv[1:4]
s = open(p).read()
if s.count(o) != 1:
    sys.exit(f"{p}: {o!r} is there {s.count(o)} times, not once")
open(p, "w").write(s.replace(o, n))' "$src/$file" "$old" "$new"; then
        echo "MUTANT $label: not applied"
        failed=$((failed + 1))
        return
    fi
    (cd "$src" && nice -n 10 cargo test --locked -p kiln --lib "$test") >"$log" 2>&1
    rc=$?
    cp "$out/orig" "$src/$file" && touch "$src/$file"
    [ "$rc" -ne 0 ] && grep -q 'test result: FAILED' "$log" && verdict=PASS
    printf 'MUTANT %s exit=%s %s :: %s\n' "$label" "$rc" "$verdict" "$(grep -E '^    [a-z_:]+$|^test .* FAILED' "$log" | tr '\n' ' ' | cut -c1-300)"
    if [ "$verdict" = PASS ]; then passed=$((passed + 1)); else failed=$((failed + 1)); fi
}

{
    echo "# Plan step 20 [kiln] step 2's source mutations on $(hostname), $(date -u +%FT%TZ)"
    mutant codex-args crates/kiln/src/harness/codex.rs 'vec!["app-server".to_owned()]' \
        'vec!["app-server".to_owned(), "--strict".to_owned()]' "$tests::codex_harness_json"
    mutant agentfile-env-key .kiln/images/harness-codex/Agentfile 'VOLUME --target-kind=harness /workspace FOR codex' \
        'ENV OPENAI_API_KEY=x
VOLUME --target-kind=harness /workspace FOR codex' "$tests::codex_"
    echo "## the untouched tree again"
    (cd "$src" && nice -n 10 cargo test --locked -p kiln --lib "$tests") >"$out/mutant-none.log" 2>&1
    rc=$?
    echo "UNTOUCHED exit=$rc :: $(grep -E '^test result' "$out/mutant-none.log" | tail -1)"
    echo "MUTANTS passed=$passed failed=$failed"
    echo "# done $(date -u +%FT%TZ)"
} >"$out/mutants.txt" 2>&1
echo "passed=$passed failed=$failed untouched=$rc" >"$out/mutants.done"
