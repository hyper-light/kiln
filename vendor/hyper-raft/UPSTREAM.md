# hyper-raft's shared crates, vendored

The crates Focal's bus, Quiver and Mantle are built on (`vendor/focal-bus`, `vendor/quiver`,
`vendor/mantle`), at one revision of hyper-raft for all of them.

- **Upstream:** https://github.com/hyper-light/hyper-raft, commit
  `0856e05df3d136f0870a98808113294a73399206` (2026-11-02), MIT (`LICENSE`): the merge of hyper-raft's
  `main` (`22d5037`, kiln's CP-T2.2 line) and Mantle's `progress-notes-reachable` (`0a182ae`, the
  revision Mantle's `main` at `17eb8032` snapshots its hyper-* crates at), made for kiln's CP-T10.2 so
  kiln holds one copy (CP-M1). Before it, `25cf889` (2026-10-22), `main` since its `main` and
  `fate-commit` lines met (the merge `7d5e536`), which Focal's `bus-replicated` at `f8052a5` took.
- **What is here:** upstream's root manifest and sixteen crates, whole, at upstream's paths:
  `hyper-block`, `hyper-datagram`, `hyper-durable`, `hyper-liveness`, `hyper-log`, `hyper-measure`,
  `hyper-quic`, `hyper-raft`, `hyper-rt`, `hyper-rt-transport`, `hyper-seal`, `hyper-sim`,
  `hyper-swim` (Mantle's, since 0856e05), `hyper-timing`, `hyper-tls`, `hyper-transport`.
- **How it's built:** Quiver's root `Cargo.toml` points the names Focal's bus takes them by at these
  folders (`[patch.crates-io]`); kiln, vendoring Quiver and Mantle, holds the same revision with the
  same changes, its own crates built on it too, but for hyper-rt: kiln builds every crate here on its
  own copy of it, `crates/kiln-rt` (the second change below), and `crates/hyper-rt` stays upstream's.

## Changes from upstream

Each change is recorded in `vendor/patches/hyper-raft/`, applied in this order on upstream's tree.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `crates/hyper-durable/src/lib.rs`, `crates/hyper-durable/Cargo.toml` | `RamStore` compiled only for the crate's own tests or under the `test-store` feature, which a dev-dependency asks for (the crate's own tests ask through one on itself) | the write-ahead log is mandatory in Quiver and kiln: `GroupStore` on a real device, `RamStore` test-only, and a build that names it outside a test fails (kiln's CP-T2.1, T-T2.1.2; Quiver's `crates/quiver/tests/ram_store.rs`) | `0001-ramstore-for-tests-alone-the-write-ahead-log-mandatory.patch` |
| `crates/hyper-block/Cargo.toml`, `crates/hyper-rt-transport/Cargo.toml` | hyper-rt taken by path from kiln's own runtime (`../../../../crates/kiln-rt`), not from `../hyper-rt` | kiln's runtime is its own copy of hyper-rt (kiln's `crates/kiln-rt/ORIGIN.md`), which kiln's `[patch.crates-io]` gives every crate that names hyper-rt: these two take it by path, which a patch can't reach, and two path packages named `hyper-rt` 0.1.0 can't share a lock file. `crates/hyper-rt` here stays upstream's, and no crate kiln builds takes it (hyper-tls takes it only for its own tests, which kiln doesn't build) | `0002-hyper-rt-from-kiln-s-own-runtime-crates-kiln-rt.patch` |

The first change is Quiver's and kiln's policy, not hyper-raft's, so it isn't offered upstream: hyper-durable
calls `RamStore` slates' case (each write complete as it's submitted), and gating it in hyper-raft
would reach slates. Focal's agent agrees (2026-10-22), and notes that Focal names `RamStore` only in a
test module (`focal-consensus`'s `shell_tests.rs`): sharing this copy, it would ask for `test-store`
as a dev-dependency, as kiln's sync tests do.

The second change is kiln's layout alone, so it isn't offered upstream either: upstream's crates take
their sibling, as they should. A fix kiln makes in its copy of hyper-rt goes upstream as kiln-rt's
rules say (kiln's `crates/kiln-rt/ORIGIN.md`).

An earlier change, IOCP's widened AFD poll issued only once the old one's cancel completes (Jupiter,
2026-10-24), was a fix of hyper-raft's own: hyper-raft carries it since `53a28a0` (with its test,
`392407f`), so the re-vendor at `0856e05` dropped it.

## Taking a newer upstream

`python3 scripts/vendor.py hyper-raft <commit>` takes these crates at that commit and re-applies the
changes above by three-way merge; take it at the revision Quiver and Mantle take, which move with it.
