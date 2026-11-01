# Quiver, vendored

kiln's event bus (`crates/kiln/src/bus.rs`), at Quiver's `main`.

- **Upstream:** https://github.com/hyper-light/quiver, commit
  `88409ddda5307abe166f310760bbe3bd51f27878`, Quiver's `main`.
- **What is here:** upstream's tree, but its own `vendor/` (kiln vendors the projects Quiver does,
  at the same revisions) and `bench/records`.

## Changes from upstream

Each change is recorded in `vendor/patches/quiver/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.lock` | `hyper-transport` depends on `aws-lc-rs` | kiln vendors hyper-raft at `4d72e19` (with Amphora), whose `hyper-transport` depends on it; Quiver's own lockfile says so too until Quiver re-vendors hyper-raft at `4d72e19` itself (asked of the Quiver agent on 2026-11-01, on top of `cp/t2.2-lab` `ccd091f`) | `0001-its-lockfile-follows-kiln-s-hyper-raft.patch` |
