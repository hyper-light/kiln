# Focal's event bus, vendored

The bus Quiver is built on (docs/design/focal-bus.md, kiln's CP-T2.1).

- **Upstream:** https://github.com/hyper-light/focal, branch `bus-replicated`, commit
  `f8052a5eaf21f81709129583e535331c2e9722f3` (2026-10-22), MIT (`LICENSE`). The commit was agreed with Focal's agent
  (docs/design/focal-bus.md, "What Focal's agent said").
- **What is here:** the bus and the crates of Focal's it builds on, laid out as upstream lays them
  out:
  - `crates/focal-bus`: the event bus (Focal's docs 32 to 34): the batch, a part's log and streams,
    the node over QUIC, the replicated part over hyper-durable;
  - `crates/focal-memory`: the memory budget every byte of the bus is charged to;
  - `crates/focal-seal` and `crates/focal-platform`: the node log's keys and the files a part's
    image is installed through;
  - `crates/focal-timing`: the bus's tests' waits (a dev-dependency).
- **What it builds on:** hyper-raft's shared crates at `4d72e19b`, vendored apart at
  `vendor/hyper-raft`. This exact Focal base originally used `25cf889`; the recorded compatibility
  patch below adapts its selected bus crates while preserving that ahead Focal base.
  Focal's own copies of them (its `vendor/hyper-*`) and its other vendored crates (`aws-lc-rs`, `aws-lc-sys`) are not taken:
  the bus names the shared crates as from crates.io, and Quiver's root `Cargo.toml` (kiln's, in kiln)
  points each name at its one copy with `[patch.crates-io]`.
- **Its own workspace:** `Cargo.toml` is upstream's root manifest. Quiver's workspace excludes
  `vendor/`, so Quiver's lints don't apply here, and the crates' `workspace = true` keys resolve
  here.

## Changes from upstream

Each change is recorded in `vendor/patches/focal-bus/`, applied in this order on upstream's tree
(`vendor/MANIFEST.toml` names the commit and the paths taken; kiln's docs/vendoring.md). Each change to
the bus's code is offered to Focal as a branch, and Focal's agent is told of it; a vendoring change
alone (as the one below) isn't.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.toml` | The shared crates (`hyper-*`) named by version, without a path into Focal's `vendor/` | Quiver takes hyper-raft once, beside the bus (`vendor/hyper-raft`), and kiln holds one copy of each hyper-light product: whoever builds the bus points each name at its copy | `0001-the-shared-crates-by-name-at-the-copy-of-hyper-raft-beside.patch` |

| `crates/focal-bus/src/{limits,member,node}.rs` | Required Hyper-Raft API compatibility: explicit Flushed disk mode and Decimated acknowledgement policy, own replica allowance, disk/liveness facts, and advisory path events | Advance the single shared Hyper-Raft pin without changing the Focal base, weakening durability, or treating shared free memory as the replica allowance | `0002-cp-t2-2-required-hyper-raft-api-compatibility.patch` |

| `crates/focal-memory/src/{budget,disk,owner}.rs` and `benches/support/alloc_count.rs` | Use the MSRV-compatible atomic update name | Keep strict Rust 1.99 validation while preserving existing atomic behavior and Rust 1.98.1 support | `0003-cp-t2-2-use-msrv-compatible-atomic-update-name.patch` |

| `crates/focal-bus/src/{node,member}.rs` | Accept the additive shared transport frame connection epoch | Keep the exact Focal base compatible without changing Publish semantics or duplicating the transport owner | `0004-cp-t2-2-retain-shared-transport-frame-provenance.patch` |

## Taking a newer upstream

`python3 scripts/vendor.py focal-bus <commit>` takes these crates at that commit, re-applies the
changes above by three-way merge, refreshes them, and runs the gate. A change that no longer applies
stops it, naming the change and the files, with nothing changed here. A new edit here is recorded
with `python3 scripts/vendor.py record focal-bus <title> --why <why>` and gets its row in the table;
`python3 scripts/vendor.py check` (in the gate) fails while an edit isn't recorded. A newer commit of the
bus may take hyper-raft at another revision: coordinate the shared pin and its recorded compatibility changes with every consumer.
