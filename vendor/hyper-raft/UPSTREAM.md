# hyper-raft's shared crates, vendored

The crates Focal's bus is built on (`vendor/focal-bus`), at hyper-raft's `main`.

- **Upstream:** https://github.com/hyper-light/hyper-raft, commit
  `4d72e19b9533851e19602eee57053a5a7337b2cd` (2026-10-28), MIT (`LICENSE`): hyper-raft's `main` since
  its `main` and `fate-commit` lines met (the merge `7d5e536`). Focal's exact `f8052a5` base
  originally used `25cf889`; its recorded compatibility patch adapts the bus to this shared pin.
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

| `crates/hyper-rt/src/kqueue.rs` | macOS FIFO readiness shares the existing reactor wait through Rustix unlimited select over its kqueue descriptor and bounded FIFO registrations; critical timers, kicks, one-shot ownership and typed invalid-FD refusals remain intact | CP-M1: native Darwin probes show quiet writer-close EOF is readable but neither kqueue nor poll signals it; native r57 validates backend fixtures, original two-second coalition/recovery, terminal and process lifecycle tests without threads or timeout polling | `0003-cp-m1-darwin-named-fifo-eof-in-the-existing-reactor.patch` |

| `crates/hyper-transport/src/{lib,endpoint}.rs`, `tests/exchange.rs` | Frames retain their actual connection epoch; an exporter lookup accepts exactly that peer/epoch, sharing the existing live-connection lookup | Authenticate delayed control capabilities and readiness against their true TLS origin without selecting a replacement connection, changing transport wire bytes, or adding another connection owner | `0004-cp-t2-2-frame-connection-provenance-and-exact-tls-exporter.patch` |

The first change is Quiver's and kiln's policy, not hyper-raft's, so it isn't offered upstream: hyper-durable
calls `RamStore` slates' case (each write complete as it's submitted), and gating it in hyper-raft
would reach slates. Focal's agent agrees (2026-10-22), and notes that Focal names `RamStore` only in a
test module (`focal-consensus`'s `shell_tests.rs`): sharing this copy, it would ask for `test-store`
as a dev-dependency, as kiln's sync tests do.

The former IOCP patch `0002` is superseded by upstream at `53a28a0`, with its regressions, and is dropped.
The Darwin FIFO patch `0003` remains: upstream kqueue code is unchanged at this pin.

## Taking a newer upstream

`python3 scripts/vendor.py hyper-raft <commit>` takes these crates at that commit and re-applies the
changes above by three-way merge; coordinate the consumer compatibility patch and the identical pin/patch set in every product.
