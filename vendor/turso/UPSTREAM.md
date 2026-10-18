# Turso Database, vendored

The engine of kiln's conversation store (`docs/conversations.md`).

- **Upstream:** https://github.com/tursodatabase/turso, commit `3e09a82750d8290ef96e3830896e7205b7c0b9e6`
  (2026-10-18, version 0.9.0-pre.1), MIT (`LICENSE.md`, `NOTICE.md`).
- **What is here:** the four crates kiln builds, laid out as upstream lays them out:
  - `core` (turso_core)
  - `sqlite/parser` (turso_parser)
  - `macros` (turso_macros)
  - `extensions/core` (turso_ext)
- **How kiln builds it:** `turso_core` with `default-features = false` and
  `features = ["fs", "json", "pure-rust-crypto"]`. That means no `simd` (simsimd is C), pure-Rust
  crypto (no libaegis C), and no `fts` (zstd-sys is C and fails on musl). No C is compiled.
- **Its own workspace:** `Cargo.toml` here is upstream's root manifest cut down to these four members,
  their `[workspace.dependencies]` and `[workspace.lints]`. kiln's workspace excludes `vendor/turso`,
  so kiln's lints don't apply to it, and the crates' `workspace = true` keys resolve here.

## Changes from upstream

| File | Change | Why |
|---|---|---|
| `Cargo.toml` | Cut down to the four crates above | only what kiln builds |
| `core/build.rs` | Never runs `git` (as with `SOURCE_DATE_EPOCH`) | vendored, it would embed kiln's commit, and builds stay reproducible |
| `core/alloc/collections/iterator.rs`, `iterator/nightly.rs` | The `nightly`-only specialization moved into a module of its own, unchanged | inline, stable Rust parses its `default fn` before the cfg drops it, and warns that the syntax will become a hard error |
| `core/storage/btree.rs`, `encryption.rs`, `pager.rs`, `mvcc/persistent_storage/logical_log.rs`, `mvcc/database/mod.rs`, `types.rs`, `vector/operations/text.rs` | `#[allow]`s for what kiln's build leaves unused, `fetch_update` (deprecated on the newest Rust, its replacement missing on older ones) and the aggregate's callback comparison; an elided lifetime spelled out | its build prints no warnings |

Taking a newer upstream means copying the four crates again from that commit, re-applying the
changes above, and updating this file.
