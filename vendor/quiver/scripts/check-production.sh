#!/usr/bin/env bash
# The no-panic rule (CLAUDE.md §1) on production code alone: libraries and binaries (cargo's default
# targets) built without cfg(test), so the test-only allowances at each crate root don't apply. The
# lints are forbidden, not just denied: an #[allow] or #[expect] for one in production code fails.
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
lints=(
  -D warnings
  -F clippy::panic
  -F clippy::unwrap_used
  -F clippy::expect_used
  -F clippy::unreachable
  -F clippy::todo
  -F clippy::unimplemented
  -F clippy::indexing_slicing
  -F clippy::arithmetic_side_effects
  -F clippy::string_slice
  -F clippy::cast_possible_truncation
  -F clippy::cast_sign_loss
  -F clippy::cast_possible_wrap
  -F clippy::disallowed_macros
  -F clippy::disallowed_methods
  -F clippy::disallowed_types
  -F clippy::dbg_macro
  -F clippy::exit
)
cargo clippy --workspace --locked "$@" -- "${lints[@]}"
