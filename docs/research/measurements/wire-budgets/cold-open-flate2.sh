#!/bin/bash
# What the deflate backend kiln's build puts flate2 on does to the agent page's cold opens (crates/kiln/tests/wire_budgets.rs,
# w21_r15_agent_page_cold_open_budget, COLD_WIRE). Since db0f283ab kiln depends on vendored shards-image, whose flate2 asks for
# zlib-rs, and Cargo's feature unification puts every flate2 in kiln's build on it, the server's packer
# (crates/kiln/src/harness/packed.rs) included. Run from a kiln checkout's root on a machine kiln's tests run on:
#   1. the cold open as built (zlib-rs), RUNS times;
#   2. the same with each REV's crates/kiln/src as the test's corpus (it reads kiln's own source at run time), HEAD's put back;
#   3. the same kiln with every vendored Shards crate's flate2 on flate2's Rust backend (miniz_oxide), RUNS times, then the
#      checkout's Cargo.toml files and Cargo.lock as they were (the next build is zlib-rs again).
# Prints each run's cold-open budget lines. Usage: cold-open-flate2.sh [REV...]  (RUNS=2 by default; CARGO_TARGET_DIR honoured)
set -u
RUNS=${RUNS:-2}
T=${CARGO_TARGET_DIR:-$PWD/target}
[ -d crates/kiln/src ] && [ -d vendor/shards/crates ] || { echo "run from a kiln checkout's root"; exit 2; }
[ -z "$(git status --porcelain -- crates/kiln/src vendor/shards Cargo.lock)" ] || { echo "crates/kiln/src, vendor/shards or Cargo.lock has changes: stop"; exit 2; }
build() { cargo test "$@" -p kiln --test wire_budgets --no-run >/dev/null 2>&1 || { echo "build failed"; exit 1; }; }
cold() {
	local bin
	bin=$(ls -t "$T"/debug/deps/wire_budgets-* | grep -v '\.d$' | head -1)
	echo "--- $1"
	"$bin" w21_r15_agent_page_cold_open_budget --nocapture --test-threads 1 2>&1 | grep -aE '^cold open .*: wire bytes  '
}
echo "kiln $(git rev-parse --short=9 HEAD), $(uname -sm), $(sw_vers -productVersion 2>/dev/null || uname -r), $(date -u +%FT%TZ)"
build --locked
for i in $(seq 1 "$RUNS"); do cold "zlib-rs, run $i"; done
if [ $# -gt 0 ]; then
	mv crates/kiln/src crates/kiln/src.head || exit 1
	trap 'rm -rf crates/kiln/src; mv crates/kiln/src.head crates/kiln/src' EXIT
	for rev in "$@"; do
		mkdir -p crates/kiln/src
		git archive "$rev" crates/kiln/src | tar -x || exit 1
		cold "zlib-rs, $rev's source as its corpus"
		rm -rf crates/kiln/src
	done
	mv crates/kiln/src.head crates/kiln/src
	trap - EXIT
fi
for f in $(grep -l 'features = \["zlib-rs", "runtime_detection"\]' vendor/shards/crates/*/Cargo.toml); do
	sed -i.orig 's/features = \["zlib-rs", "runtime_detection"\]/features = ["rust_backend"]/' "$f" && rm -f "$f.orig"
done
trap 'git checkout -q -- vendor/shards Cargo.lock' EXIT
# Not --locked: with no flate2 on zlib-rs, Cargo.lock may lose what only it needed; it's put back on exit.
build --offline
for i in $(seq 1 "$RUNS"); do cold "miniz_oxide, run $i"; done
