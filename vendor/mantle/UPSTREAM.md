# Mantle, vendored

kiln's store of bytes (docs/mantle.md; kiln's CP-T10.2): Mantle's crates, built on kiln's one copy of
hyper-raft and of aws-lc-rs (CP-M1's one revision).

- **Upstream:** https://github.com/hyper-light/mantle, branch `main`, commit
  `87ab09e1c94149c7b669cb0de5975e228125c867` (2026-11-08), MIT (`LICENSE`): the head of Mantle's
  main as kiln took it (2026-11-12; `17eb8032` before, 2026-11-01).
- **What is here:** upstream's tree, at upstream's paths: its crates (`crates/*`), its root
  manifest and `Cargo.lock`, its toolchain, lint and format settings, its scripts, `README.md`,
  `CLAUDE.md` and `LICENSE`. Left out (`vendor/MANIFEST.toml`): its `vendor/`, its copies of
  hyper-raft's crates (at `5e40eb9`) and of aws-lc-rs 1.18.1 and aws-lc-sys 0.45.0 with its changes
  (upstream's `vendor/UPSTREAM.md`); its `docs/`, `loomdoc.html` and drawings; and its CI
  (`.github/`, `.kiln/`), which runs in Mantle's repository.
- **What it builds on:**
  - hyper-raft's shared crates at `fb74b59`, vendored apart at `vendor/hyper-raft`: hyper-raft's
    `main`, which holds what Mantle's `vendor/` takes (`5e40eb9`: Mantle's line `0a182ae` and the driver
    fix `main` landed as `fb74b59`), so kiln, Quiver, Focal's bus and Mantle link one revision; hyper-rt
    from kiln's own copy of it, `crates/kiln-rt`, with that revision's hyper-rt changes kiln takes (its
    `ORIGIN.md`; the third change below);
  - aws-lc-rs 1.18.2 with Mantle's changes, vendored apart at `vendor/aws-lc-rs`, and aws-lc-sys
    0.46.0 from crates.io, which lacks three of Mantle's changes to it
    (`vendor/aws-lc-rs/UPSTREAM.md` says which).
- **Its own workspace:** `Cargo.toml` is upstream's root manifest, its `[patch.crates-io]` changed
  (below). kiln's workspace excludes `vendor/mantle`, so kiln's lints don't apply here and Mantle's
  do. Run in this folder, cargo takes Mantle's toolchain (`rust-toolchain.toml`, 1.98.1); kiln builds
  on stable (1.99.1). With `RUSTUP_TOOLCHAIN=stable`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` passes here at this commit (2026-11-12). Its only warnings are
  deprecations in hyper-rt, which is kiln's copy and outside this workspace.

## Changes from upstream

Each change is recorded in `vendor/patches/mantle/`, applied in this order on upstream's tree
(`vendor/MANIFEST.toml` names the commit and what's left out; kiln's docs/vendoring.md). Each change
to Mantle's code is offered to Mantle as a branch, and Mantle's agent is told of it; a vendoring
change alone (the first and third below) isn't.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.toml`, `Cargo.lock`, the manifests of `crates/cert`, `chunk`, `client`, `disk`, `ec`, `engine`, `http`, `mantle`, `node`, `protocol` and `range` | each of hyper-raft's crates taken by path from kiln's copy (`../../../hyper-raft/crates/hyper-*`), not from `../../vendor/hyper-*`; the root's `[patch.crates-io]` points aws-lc-rs at kiln's copy (`../aws-lc-rs/aws-lc-rs`) and leaves aws-lc-sys crates.io's; `Cargo.lock` resolved on them | kiln holds one copy of each: two linked copies of hyper-raft fail CP-M1's one-revision test, and Mantle's own copies (its `vendor/`) aren't here. A path dependency can't be patched, so the paths change | `0001-its-crates-on-kiln-s-one-hyper-raft-and-aws-lc-rs.patch` |
| `crates/protocol/src/bounds.rs` | `Limits::acknowledgement` stated as `Acknowledgement::Decimated`, `Limits::fec_window` as `None` | hyper-raft's main, merged with Mantle's line for kiln's one copy, adds both (its docs/transport.md §4ak, §4an). A node's transport states them as every connection had them before: acknowledgements decimated for its streams of data, no erasure coding | `0002-hyper-transport-s-acknowledgement-and-erasure-coding-stated.patch` |
| the manifests of `crates/chunk`, `client`, `engine`, `mantle` and `node` | hyper-rt taken by path from kiln's own runtime (`../../../../crates/kiln-rt`), not from kiln's copy of hyper-raft | kiln's runtime is its own copy of hyper-rt (kiln's `crates/kiln-rt/ORIGIN.md`), which every crate kiln builds that names hyper-rt takes, hyper-raft's own crates too (`vendor/hyper-raft/UPSTREAM.md`); a path dependency can't be patched, and two path packages named `hyper-rt` 0.1.0 can't share a lock file. `Cargo.lock` doesn't say where a path package lives, so it is unchanged | `0003-hyper-rt-from-kiln-s-own-runtime-crates-kiln-rt.patch` |

## Taking a newer upstream

`python3 scripts/vendor.py mantle <commit>` takes Mantle at that commit, re-applies the changes
above by three-way merge, refreshes them, and runs the gate. A change that no longer applies stops
it, naming the change and the files, with nothing changed here. A new edit here is recorded with
`python3 scripts/vendor.py record mantle <title> --why <why>` and gets its row in the table;
`python3 scripts/vendor.py check` (in the gate) fails while an edit isn't recorded.

A newer commit of Mantle may take hyper-raft at a commit kiln's copy doesn't hold (upstream's
`vendor/UPSTREAM.md` names it). Merge that commit into the revision `vendor/hyper-raft` is at, as
`0856e05` merged `0a182ae`, take hyper-raft at the merge, and Mantle with it. Mantle's
`scripts/snapshot-hyper-raft.py` writes Mantle's own `vendor/`, which isn't here: it isn't how kiln
takes hyper-raft.
