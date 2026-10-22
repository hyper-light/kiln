# hyper-raft's shared crates, vendored

The crates Focal's bus is built on (`vendor/focal-bus`), at the revision it takes.

- **Upstream:** https://github.com/hyper-light/hyper-raft, commit
  `1af65f7b9564c9ba296993dd30bdda27bc97fa03` (2026-10-20), MIT (`LICENSE`): the revision Focal's
  `bus-replicated` at `b408a3b` takes all fifteen shared crates at (Focal's `vendor/README.md`).
- **What is here:** upstream's root manifest and the fifteen crates, whole, at upstream's paths:
  `hyper-block`, `hyper-datagram`, `hyper-durable`, `hyper-liveness`, `hyper-log`, `hyper-measure`,
  `hyper-quic`, `hyper-raft`, `hyper-rt`, `hyper-rt-transport`, `hyper-seal`, `hyper-sim`,
  `hyper-timing`, `hyper-tls`, `hyper-transport`.
- **How it's built:** Quiver's root `Cargo.toml` points the names Focal's bus takes them by at these
  folders (`[patch.crates-io]`); kiln, vendoring Quiver, holds the same revision with the same
  changes, its own crates built on it too.

## Changes from upstream

Each change is recorded in `vendor/patches/hyper-raft/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `crates/hyper-durable/src/lib.rs`, `crates/hyper-durable/Cargo.toml` | `RamStore` compiled only for the crate's own tests or under the `test-store` feature, which a dev-dependency asks for (the crate's own tests ask through one on itself) | the write-ahead log is mandatory in Quiver and kiln: `GroupStore` on a real device, `RamStore` test-only, and a build that names it outside a test fails (kiln's CP-T2.1, T-T2.1.2; Quiver's `crates/quiver/tests/ram_store.rs`) | `0001-ramstore-for-tests-alone-the-write-ahead-log-mandatory.patch` |

The change is Quiver's and kiln's policy, not hyper-raft's: slates' store has `RamStore`'s shape
(upstream's `docs/durable.md`), so upstream keeps it in every build. It's offered to hyper-raft as
an opt-in feature, on the person's go-ahead.

## Taking a newer upstream

`python3 scripts/vendor.py hyper-raft <commit>` takes these crates at that commit and re-applies the
change above by three-way merge; take it at the revision Focal's bus takes.
