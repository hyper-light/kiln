#!/usr/bin/env bash
# Plan step 26, step 2's record: kiln's module tests, its integration test, Shards' own checks of the
# Agentfiles and clippy,
# run from the committed tree of s26-agy-agentfile (s26.bundle over kiln as cloned from GitHub),
# nothing laid over it. Writes the record to $W/e_step2_tests.txt and prints it.
set -uo pipefail
here=$(pwd)
W="$HOME/.cache/s26-agy-step2"
want=$1
cp "$here/s26.bundle" "$W/s26.bundle" && cd "$W/kiln" || exit 1
R="$W/e_step2_tests.txt"
say() { printf '%s\n' "$*" | tee -a "$R"; }
: > "$R"
say "# Plan step 26, step 2 (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d): the module's tests, Shards' check and clippy"
say "# on $(hostname) ($(uname -m), $(uname -r)), $(date -u +%FT%TZ), from the committed tree of $want"
say "bundle sha256 $(sha256sum ../s26.bundle | cut -d' ' -f1)"
git fetch --quiet origin || exit 1
git bundle verify --quiet ../s26.bundle || exit 1
git fetch --quiet ../s26.bundle refs/heads/s26-agy-agentfile || exit 1
git checkout --quiet --force -B s26-agy-agentfile FETCH_HEAD || exit 1
git clean -fdxq -- .kiln/images/harness-antigravity crates/kiln/src/agentfiles || exit 1
head=$(git rev-parse HEAD)
say "HEAD $head"
[ "$head" = "$want" ] || { say "HEAD is not $want"; exit 1; }
dirty=$(git status --porcelain --untracked-files=all | wc -l)
say "working tree: $dirty paths differ from HEAD"
[ "$dirty" = 0 ] || exit 1
say "files at HEAD:"
git ls-tree -r HEAD -- .kiln/images/harness-antigravity crates/kiln/src/agentfiles/builtin.rs crates/kiln/src/agentfiles/builtin | while read -r mode type obj path; do say "  $path blob $obj sha256 $(sha256sum "$path" | cut -d' ' -f1)"; done
say "$(cargo --version); $(rustc --version); $(cargo clippy --version)"
export CARGO_TARGET_DIR="$W/target" CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=4
say ""
say "\$ cargo test --locked -p kiln --lib agentfiles::builtin"
nice -n 10 cargo test --locked -p kiln --lib agentfiles::builtin > "$W/test3.log" 2>&1; t=$?
grep -E '^(test |running |test result)|^Error|panicked' "$W/test3.log" | while IFS= read -r l; do say "$l"; done
say "exit $t"
say ""
say "\$ cargo test --locked -p kiln --test agentfiles"
nice -n 10 cargo test --locked -p kiln --test agentfiles > "$W/test3-it.log" 2>&1; i=$?
grep -E '^(test |running |test result)|^Error|panicked' "$W/test3-it.log" | while IFS= read -r l; do say "$l"; done
say "exit $i"
S="$W/shards-v0.7.0/shards-linux-amd64"
say ""
say "Shards v0.7.0's release binary, shards-linux-amd64, against its SHA256SUMS (https://github.com/hyper-light/shards/releases/download/v0.7.0/SHA256SUMS):"
(cd "$W/shards-v0.7.0" && grep ' shards-linux-amd64$' SHA256SUMS | sha256sum -c -) 2>&1 | while IFS= read -r l; do say "  $l"; done
home=$(mktemp -d "$W/shards-home.XXXXXX")
say "  $(SHARDS_HOME="$home" "$S" version 2>&1 | grep -E 'Version|Git commit' | tr -s ' ' | tr '\n' ';')"
say "\$ KILN_TEST_SHARDS=<that binary> SHARDS_HOME=<an empty directory> cargo test --locked -p kiln --lib -- antigravity_agentfile_passes_shards_own_check step23_a_real_shards_checks_every_builtin step30_a_real_shards_checks_without_resolving_bases"
KILN_TEST_SHARDS="$S" SHARDS_HOME="$home" nice -n 10 cargo test --locked -p kiln --lib -- antigravity_agentfile_passes_shards_own_check step23_a_real_shards_checks_every_builtin step30_a_real_shards_checks_without_resolving_bases > "$W/test3-shards.log" 2>&1; s=$?
grep -E '^(test |running |test result)|^Error|panicked' "$W/test3-shards.log" | while IFS= read -r l; do say "$l"; done
say "exit $s"
rm -rf "$home"
say ""
say "\$ cargo clippy --locked -p kiln --all-targets -- -D warnings"
nice -n 10 cargo clippy --locked -p kiln --all-targets -- -D warnings > "$W/clippy3.log" 2>&1; c=$?
grep -E '^(warning|error)|Finished' "$W/clippy3.log" | while IFS= read -r l; do say "$l"; done
say "exit $c"
say ""
say "verdict: module tests exit $t, integration test exit $i, Shards checks exit $s, clippy exit $c"
[ "$t" = 0 ] && [ "$i" = 0 ] && [ "$s" = 0 ] && [ "$c" = 0 ]
