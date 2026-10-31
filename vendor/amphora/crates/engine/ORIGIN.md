# Amphora's engine: where it came from

`crates/engine` (`amphora-engine`), `crates/engine-parser`, `crates/engine-macros` and
`crates/engine-ext` were Turso Database's four crates `core` (`turso_core`), `sqlite/parser`
(`turso_parser`), `macros` (`turso_macros`) and `extensions/core` (`turso_ext`), taken from
https://github.com/tursodatabase/turso at commit `3e09a82750d8290ef96e3830896e7205b7c0b9e6`
(2026-10-18, version 0.9.0-pre.1) under the MIT license (`LICENSE.md` and `NOTICE.md` here, kept as
it requires). Since 2026-10-20 they are Amphora's own code, and Amphora is a database of its own:
nothing is synced with Turso in either direction, and a change is an Amphora commit.

- **How Amphora builds it:** `amphora-engine` with `default-features = false` and
  `features = ["fs", "json", "pure-rust-crypto", "checksum"]` (each page checksummed:
  `docs/design/store.md`, decision 12): no `simd` (simsimd is C), pure-Rust crypto (no libaegis C),
  no `fts` (zstd-sys is C and fails on musl). Its own default features are Turso's without `simd`
  and with `pure-rust-crypto` and `checksum`, so its tests run as Amphora runs it. Nothing is
  compiled from C, its tests included.
- **Its symbols:** the extension macros export a function unmangled (`#[no_mangle]`) only outside
  the engine itself (`cfg(amphora_engine_core)`, set by its build script): its built-ins register
  by pointer, and a binary linking it beside another copy of Turso carries no symbol twice.
- **Its clippy configuration:** each of its crates holds a `clippy.toml` of its own (none of
  Amphora's disallowed lists, which `scripts/check-production.sh` counts it against instead).
- **Its rules:** its crates keep the edition (2021) and the lints they came with. CLAUDE.md's rules
  hold for every line Amphora writes or changes here; `scripts/check-production.sh` counts what is
  left of the inherited code that breaks them (CLAUDE.md §1), and that count may only fall.

## What changed from Turso 3e09a82

Amphora now supports persistent secondary indexes and ordinary mutations on
WITHOUT ROWID tables using composite primary-key locators, including UNIQUE,
UPSERT, REPLACE and foreign-key actions. The physical layout, compatibility
evidence and remaining explicit refusals are recorded in
[`docs/design/without-rowid.md`](../../docs/design/without-rowid.md).

When Amphora's engine was taken (2026-10-20): the crates renamed (`amphora-engine`,
`amphora-engine-parser`, `amphora-engine-macros`, `amphora-engine-ext`) and moved into Amphora's
workspace, its dependencies named in Amphora's root `Cargo.toml`; Turso's microbenchmarks dropped
(Amphora's `bench/` measures the engine), and with them the dev-dependencies only they used; the
tests that made a database file with SQLite through rusqlite (C) make it with the engine
(`storage::new_wal_database`), and the one that asked SQLite for its integrity check asks the engine.

Before that, kiln's changes (the first five) and Amphora's, on Turso's tree:

| File | Change | Why |
|---|---|---|
| `Cargo.toml` | Cut down to the four crates above | only what kiln builds |
| `core/build.rs` | Never runs `git` (as with `SOURCE_DATE_EPOCH`) | vendored, it would embed kiln's commit, and builds stay reproducible |
| `core/alloc/collections/iterator.rs`, `iterator/nightly.rs` | The `nightly`-only specialization moved into a module of its own, unchanged | inline, stable Rust parses its `default fn` before the cfg drops it, and warns that the syntax will become a hard error |
| `core/io/windows_lock.rs` | `#![allow(dead_code)]` | its shared-WAL locks are unused in kiln's build, so a Windows build warned 17 times |
| `core/storage/btree.rs`, `encryption.rs`, `pager.rs`, `mvcc/persistent_storage/logical_log.rs`, `mvcc/database/mod.rs`, `types.rs`, `vector/operations/text.rs` | `#[allow]`s for what kiln's build leaves unused, `fetch_update` (deprecated on the newest Rust, its replacement missing on older ones) and the aggregate's callback comparison; an elided lifetime spelled out | its build prints no warnings |
| `core/connection.rs`, `core/database.rs`, `core/vdbe/mod.rs` | `Connection::set_statement_journals` (on by default): off, a write statement in an explicit transaction keeps no statement journal, and one that fails or is abandoned unfinished poisons the transaction, so COMMIT rolls it back whole and says so | Amphora's writer rolls back on any failure; the journal was 13.6% of its time applying rows (Amphora's `docs/design/store.md`, decision 7) |
| `core/database.rs`, `core/storage/pager.rs`, `core/storage/database.rs` | Opening an initialized database defers page checksums on the bootstrap pager (no log attached yet) until the log is attached (`Pager::defer_checksum`, resumed in `Pager::set_wal`, and before a legacy header is rewritten); `IOContext::restore_checksum` | header validation reads page 1 from the file before the log is attached; after a crash during a checkpoint the file's page 1 can be torn while the log holds it whole, and with the `checksum` feature the open failed (Amphora's crash injection: 3-12 of 10,000 power cuts; Turso already clears what the bootstrap read once the log is attached) |
| `core/storage/sqlite3_ondisk.rs` | `checksum_wal` sums a page's worth in four lanes side by side and joins them with powers of the step's matrix (the same sums as one step after another; a test compares every length to 4,168 bytes, both byte orders) | a commit's frames are checksummed one after another on the writer: the chain of dependent adds was 5-6% of Amphora's writer's time |
| `core/storage/page_cache.rs`, `core/storage/pager.rs` | The spill check keeps the exact count of evictable pages its full scan makes (`needs_spill` and `check_spill` take `&mut self`, under the cache's write lock) | a commit cleans its pages in the WAL without telling the cache, so the tracked count only fell; once it was low, every page brought in scanned the whole cache (5% of Amphora's writer's time) |
| `core/io/mod.rs`, `core/io/unix.rs` | `FileSyncType::DataSync`: `fdatasync` on Unix systems but Apple's, `F_FULLFSYNC` on Apple's (as `FullFsync`), what `Fsync` does elsewhere | Amphora's flusher flushes the log's data only, as SQLite and PostgreSQL do (Amphora's `docs/design/store.md`, decision 13) |
| `core/storage/wal.rs` | A new log header cuts the log's file back to it only under the multi-process authority (`WalCoordination::truncates_orphans`); in one process the frames from before the restart stay, to be overwritten in place, as SQLite's are | a log cut back at every restart grows by appending again, so its flushes write metadata; the new header is still synced before any frame follows it, which is what makes the frames left behind safe (decision 13; crash injection, 10,000 power cuts) |
| `core/database.rs`, `core/storage/wal.rs`, `core/storage/sqlite3_ondisk.rs` | The log known durable (`WalSharedRuntime::durable`: a generation, the header's `checkpoint_seq`, and a frame), set by `Database::wal_durable` after a flush begun when `Database::wal_end` answered, read by `Database::wal_durable_frame`; a checkpoint flushes no log of its own for frames within it in the current generation (`WalCoordination::durable_through`, never under the multi-process authority) | Amphora's flusher already made those frames durable: its checkpointer copies only what a flush covered and skips one flush of the log a pass (Amphora's `docs/design/store.md`, decision 15) |

The query planner also offers the existing WITHOUT ROWID primary root for
leading-key seeks. Its descriptor is shared with FK resolution, contains only
PK comparison terms, and remains absent from schema index mutation lists.
Payload reads keep the existing composite-key table lookup and column mapping;
see `docs/design/without-rowid.md` for bounded-access regressions.
