#!/usr/bin/env bash
# The no-panic rule (CLAUDE.md §1) on production code alone: libraries and binaries (cargo's default
# targets) built without cfg(test), so the test-only allowances at each crate root don't apply. The
# lints are forbidden, not just denied: an #[allow] or #[expect] for one in production code fails.
#
# The engine (crates/engine*) came from Turso with code that predates the rule: its crates are
# counted instead, lint by lint, against scripts/engine-ratchet.toml, and a count may only fall.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
rule=(
  clippy::panic
  clippy::unwrap_used
  clippy::expect_used
  clippy::unreachable
  clippy::todo
  clippy::unimplemented
  clippy::indexing_slicing
  clippy::arithmetic_side_effects
  clippy::string_slice
  clippy::cast_possible_truncation
  clippy::cast_sign_loss
  clippy::cast_possible_wrap
  clippy::disallowed_macros
  clippy::disallowed_methods
  clippy::disallowed_types
  clippy::dbg_macro
  clippy::exit
)
engine=(amphora-engine amphora-engine-parser amphora-engine-macros amphora-engine-ext)
forbid=(-D warnings)
count=()
for lint in "${rule[@]}"; do
  forbid+=(-F "$lint")
  count+=(-W "$lint")
done
apart=()
only=()
for crate in "${engine[@]}"; do
  apart+=(--exclude "$crate")
  only+=(-p "$crate")
done
# --no-deps: only the crates named are linted (else every workspace crate built, the engine's build
# script and macros among them).
cargo clippy --workspace "${apart[@]}" --locked --no-deps "$@" -- "${forbid[@]}"
# The engine's crates hold a clippy.toml of their own; this pass reads Amphora's.
CLIPPY_CONF_DIR="$PWD" cargo clippy "${only[@]}" --locked --no-deps --message-format=json "$@" -- "${count[@]}" |
  python3 scripts/engine-ratchet.py scripts/engine-ratchet.toml "$(rustc -vV | sed -n 's/^host: //p')" "${rule[@]}"
