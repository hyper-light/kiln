#!/usr/bin/env bash
# Plan step 26, step 6's record: the built-in module's tests, the integration test that lists the
# built-ins, Shards' own checks of the Agentfiles with a shards built from Shards main at or after
# 997f4e47e (step 4's, scripts/step4/build-shards.sh), and clippy. Run from the committed tree of
# s26-agy-agentfile (s26.bundle over kiln as cloned from GitHub), nothing laid over it.
# Usage: bash step6-tests.sh <commit> [shards]. Writes the record to $W/e_step6_tests.txt and prints it.
set -uo pipefail
here=$(pwd)
W="$HOME/.cache/s26-agy-step6"
want=$1
S=${2:-"$HOME/.cache/s26-agy-step4/shards-target/test-release/shards"}
mkdir -p "$W" || exit 1
[ -d "$W/kiln/.git" ] || git clone --quiet https://github.com/hyper-light/kiln.git "$W/kiln" || exit 1
cp "$here/s26.bundle" "$W/s26.bundle" && cd "$W/kiln" || exit 1
R="$W/e_step6_tests.txt"
say() { printf '%s\n' "$*" | tee -a "$R"; }
: > "$R"
say "# Plan step 26, step 6 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): the built-in module's tests, Shards' checks and clippy"
say "# on $(hostname) ($(uname -m), $(uname -r)), $(date -u +%FT%TZ), from the committed tree of $want"
say "bundle sha256 $(sha256sum ../s26.bundle | cut -d' ' -f1)"
git fetch --quiet origin || exit 1
git bundle verify --quiet ../s26.bundle || exit 1
git fetch --quiet ../s26.bundle refs/heads/s26-agy-agentfile || exit 1
git checkout --quiet --force -B s26-agy-agentfile FETCH_HEAD || exit 1
git clean -fdxq -- .kiln/images crates/kiln || exit 1
head=$(git rev-parse HEAD)
say "HEAD $head"
[ "$head" = "$want" ] || { say "HEAD is not $want"; exit 1; }
say "origin/main $(git rev-parse origin/main); HEAD contains it: $(git merge-base --is-ancestor origin/main HEAD && echo yes || echo no)"
dirty=$(git status --porcelain --untracked-files=all | wc -l)
say "working tree: $dirty paths differ from HEAD"
[ "$dirty" = 0 ] || exit 1
say "files at HEAD:"
git ls-tree -r HEAD -- .kiln/images/harness-antigravity crates/kiln/src/agentfiles/builtin.rs crates/kiln/src/agentfiles/builtin crates/kiln/tests/agentfiles.rs | while read -r mode type obj path; do say "  $path blob $obj sha256 $(sha256sum "$path" | cut -d' ' -f1)"; done
say "$(cargo --version); $(rustc --version); $(cargo clippy --version)"
export CARGO_TARGET_DIR="$W/target" CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=4
say ""
say "\$ cargo test --locked -p kiln --lib agentfiles::builtin"
nice -n 10 cargo test --locked -p kiln --lib agentfiles::builtin > "$W/test.log" 2>&1; t=$?
grep -E '^(test |running |test result)|^Error|panicked' "$W/test.log" | while IFS= read -r l; do say "$l"; done
say "exit $t"
say ""
say "\$ cargo test --locked -p kiln --test agentfiles"
nice -n 10 cargo test --locked -p kiln --test agentfiles > "$W/test-it.log" 2>&1; i=$?
grep -E '^(test |running |test result)|^Error|panicked' "$W/test-it.log" | while IFS= read -r l; do say "$l"; done
say "exit $i"
say ""
say "the shards: $S, sha256 $(sha256sum "$S" | cut -d' ' -f1)"
home=$(mktemp -d "$W/shards-home.XXXXXX")
say "  $(SHARDS_HOME="$home" "$S" version 2>&1 | grep -E 'Version|Git commit' | tr -s ' ' | tr '\n' ';')"
say "\$ KILN_TEST_SHARDS=<that binary> SHARDS_HOME=<an empty directory> cargo test --locked -p kiln --lib -- antigravity_agentfile_passes_shards_own_check step23_a_real_shards_checks_every_builtin step30_a_real_shards_checks_without_resolving_bases"
KILN_TEST_SHARDS="$S" SHARDS_HOME="$home" nice -n 10 cargo test --locked -p kiln --lib -- antigravity_agentfile_passes_shards_own_check step23_a_real_shards_checks_every_builtin step30_a_real_shards_checks_without_resolving_bases > "$W/test-shards.log" 2>&1; s=$?
grep -E '^(test |running |test result)|^Error|panicked|^skipped' "$W/test-shards.log" | while IFS= read -r l; do say "$l"; done
say "exit $s"
rm -rf "$home"
say ""
say "\$ cargo clippy --locked -p kiln --all-targets -- -D warnings"
nice -n 10 cargo clippy --locked -p kiln --all-targets -- -D warnings > "$W/clippy.log" 2>&1; c=$?
grep -E '^(warning|error)|Finished' "$W/clippy.log" | while IFS= read -r l; do say "$l"; done
say "exit $c"
say ""
say "verdict: module tests exit $t, integration test exit $i, Shards checks exit $s, clippy exit $c"
[ "$t" = 0 ] && [ "$i" = 0 ] && [ "$s" = 0 ] && [ "$c" = 0 ]
