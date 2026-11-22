#!/usr/bin/env bash
# Every gate of CLAUDE.md, in order; exits non-zero at the first failure. Run it before each commit;
# kiln CI runs it on every push and pull request (.kiln/ci.yaml).
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
gate() { printf '::: %s\n' "$*" >&2; "$@"; }
gate cargo fmt --all --check
gate cargo clippy --workspace --all-targets --locked -- -D warnings
# The same lints for macOS and Windows (rust-toolchain.toml installs their targets; checking needs
# no linker or SDK while the code is pure Rust).
gate cargo clippy --workspace --all-targets --locked --target aarch64-apple-darwin -- -D warnings
gate cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
gate bash scripts/check-production.sh
gate cargo test --workspace --locked
# T-PL.10.1: nested against unnested answers reads the engine's simulator unnesting control, so the
# test needs that feature (crates/engine/Cargo.toml) and the workspace run above skips it.
gate cargo test -p amphora-engine --locked --features simulator --test unnesting
# The corpus's gate subset (G2 step 7, T-G2.7.1): bench/corpus/subset's files run on Amphora by the
# runner cargo test just built, against SQLite's answers, held to the plan's "under 2 minutes" (fill
# files stop at the deadline this machine measures). Every record's time is kept, named by machine,
# for bench/corpus/subset.py to re-rank the pool from (.kiln/ci.yaml keeps them as artifacts).
corpus_times="target/corpus-times/${RUNNER_NAME:-$(uname -n)}"
mkdir -p "$corpus_times"
gate cargo run --locked -p amphora-corpus -- run bench/corpus/subset --budget-s 120 --times "$corpus_times/$(date -u +%Y%m%dT%H%M%SZ).jsonl"
gate python3 scripts/vendor.py check
