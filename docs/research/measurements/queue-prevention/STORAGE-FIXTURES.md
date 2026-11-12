# Storage reservation fixture source (uncompiled/unrun)

POLICY_READ implementer: current Shards origin/main
`7d7e0855a5073ed5163b4fe7ed3355c9ed7c2506`; full AGENTS/rules/evidence/CLAUDE
receipt retained, rules SHA-256
`df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3`.
Current ROOT, complete corrected storage.rs and SQLITE-STORAGE.md read before
authoring. Kiln base `f555736bd887a4196c8841564cb9b4d6c1ede95f`, isolated
`/private/tmp/kiln-queue-prevention-20261111`, macOS arm64. Ownership is only new
`crates/kiln/src/store/storage_tests.rs` plus this evidence. Root registers its
cfg(test) nested module in storage.rs and owns production reserve changes. All
Told/wait/handoff SQL work remains frozen.

The coordinator's actual large-database counterexample and corrected Btree-path
derivation are in [SQLITE-STORAGE.md](SQLITE-STORAGE.md). This lane added no
production formula, floor, fallback, tunable allowance or architecture. Three
bounded real SQLite/WAL fixture sources now exercise the existing reserve_rows
with measured schema/layout inputs:

- `overflow_payload_and_spilled_wal_fit_reserved_scratch`: 512-byte pages,
  one 8-MiB body and a 2-MiB unrelated baseline. The single changed row keeps
  path-only scratch small enough to make omission of overflow payload a useful
  coordinator guard-removal mutation. It requires actual WAL bytes before commit,
  so merely setting cache_spill is not reported as proof that spill happened.
- `indexed_large_keys_and_many_inserts_fit_actual_schema_reserve`: 4096-byte
  pages, 32 deterministic out-of-order rows, 64-KiB bodies, 8-KiB primary keys
  and 4-KiB sender keys, plus a 2-MiB unrelated baseline. App payload remains
  below 8 MiB. Actual schema roots are the table, primary-key index, two explicit
  indexes and sqlite_sequence; actual maximum table width is three columns.
  The fixture verifies all inserted bodies' lengths and complete key/sender data.
- `shrinking_old_overflow_body_and_keys_with_secure_delete_fit_prior_bound`:
  4096-byte pages, one old 1-MiB body and two 128-KiB keys, below roughly 2 MiB
  baseline including index copies. Secure delete and a small page cache exercise
  old overflow/free-page writes despite a tiny new value. Reservation receives
  the complete prior payload bound; the final row is verified exactly.

Numbers above are static bounded test workloads and the coordinator's requested
fixture limits, not new runtime capacities. The small cache is a test condition;
no production cache was added. Each private tempfile database uses bundled
rusqlite, WAL, disabled automatic checkpointing, FULL synchronous mode and secure
delete. No production database, service, native provider or other actor is used.

Each case checkpoints/truncates baseline WAL, reads actual page_size/page_count,
actual sqlite_schema Btree roots and pragma_table_info column count, then computes
reserve_rows **before** its transaction. It records actual WAL length during the
transaction and after commit, new SHM length, and database length after manual
checkpoint. The assertion is stronger than the requested pair: actual WAL growth
plus checkpoint database growth plus new SHM bytes must fit the reserve. Database
length/page-count consistency and source-data assertions are separate witnesses.

These are logical file-growth checks, not allocated-block, filesystem-free-space,
OS reservation, throughput, latency, RSS or fault measurements. Summing WAL peak
and subsequent checkpoint database growth conservatively covers their overlap.
Separate private directories and connection-before-directory field drop order
provide scoped cleanup; actual cleanup is not reported passed before execution.
No waiter task, probe, retry timer, benchmark or profiling loop is introduced.

Root can register without changing production visibility:

```rust
#[cfg(test)]
#[path = "storage_tests.rs"]
mod wal_tests;
```

This belongs inside storage.rs, because the fixture imports its private
`super::reserve_rows`. After root compiles the snapshot, an optional test-only
`KILN_STORAGE_FIXTURE_JSON` environment flag emits one JSON line per case with
actual bundled SQLite version, layout, payload/row counts, reserve and observed
growth. No raw body, key or temporary path is printed. Root owns execution and
retained runtime output, guard-removal mutation, resource accounting and review.

Actual checks in this lane: `rustfmt --edition 2024 --emit stdout
crates/kiln/src/store/storage_tests.rs` exit 0 and named-file whitespace check
exit 0. Source SHA-256:
`7f5592c883b3351f0629d58fdafcff8a86e412b630470ff49e38674ae39eef51`.
**No compile or runtime test was run.** There is no passing physical-space,
mutation, supported-platform or performance claim at this checkpoint. Only
512/4096 page-size cases are authored; other page sizes and supported systems
remain unverified. No own builds, full suite, audit, actual benchmark/profiling,
actors, children, production edits or commits occurred.
