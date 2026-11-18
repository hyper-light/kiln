# Slates, vendored

Agents' workspaces of their own (docs/agentic.md, W; kiln plan 01a157fdefce75f698e67bc147f47586, CP-T4.1): an
agent's folder is a Slates volume laid over the real one. kiln drives Slates through its library (`slates-api`,
Slates' A-278), linked from here, never through its command line (the person, 2026-11-18):
`crates/kiln/src/workspace/slates.rs` calls it, and kiln's own executable runs the instance's anchor and daemon
(`kiln slates-role`, served by `slates_api::role::main`), so kiln carries no Slates binary and the daemon is the same
code as the client kiln links. kiln speaks the library's typed consumer contract (A-299) and pins the version it was
written against (`workspace::slates::CONTRACT`, 8 at this revision): a re-vendor that moves it fails kiln's unit test
`w1_kiln_speaks_the_contract_it_was_written_against`.

- **Upstream:** https://github.com/hyper-light/slates, branch `main`, commit
  `15f6b5b8e17f7d8fad520192d08333de0277e35d` (2026-11-18), MIT (`LICENSE`).
- **What is here:** upstream's tree, at upstream's paths: its crates (`crates/*`), its root manifest, its toolchain,
  lint and format settings with the budgets its own checks read (`clippy.toml`, `rustfmt.toml`,
  `rust-toolchain.toml`, `unsafe-budget.toml`, `ratchets.toml`), its `xtask`, its agents' skills (`skills/`, which its
  MCP crate includes as it builds), `README.md` and `LICENSE`. Left out (`vendor/MANIFEST.toml`): its `vendor/`, its
  copies of hyper-raft's crates (`hyper-datagram`, `hyper-seal`, `hyper-swim`, `hyper-timing` at hyper-raft
  `aa82ada`) and of aws-lc-rs 1.18.1 and aws-lc-sys 0.45.0 with Mantle's changes (upstream's `vendor/UPSTREAM.md`);
  its `Cargo.lock`, which kiln's own `Cargo.lock` stands for in every build of kiln; and its design record (`docs/`),
  CI (`.github/`, `ci/`), deployment (`deploy/`, `Dockerfile`) and agents' notes, which are Slates' and no part of what
  kiln builds.
- **What it builds on:**
  - hyper-raft's shared crates from kiln's one copy (`vendor/hyper-raft`, at `e0613f52`), which kiln, Quiver, Focal's
    bus and Mantle link too;
  - aws-lc-rs 1.18.2 with Mantle's changes, vendored apart at `vendor/aws-lc-rs`, and aws-lc-sys 0.46.0 from
    crates.io, which lacks the changes Slates' own copy of it carries (CPU jitter entropy off by default, the RNDR
    retry and the operating system's generator as its fallback; upstream's `vendor/UPSTREAM.md`).
- **Its own workspace:** `Cargo.toml` is upstream's root manifest, its hyper-raft paths and `[patch.crates-io]`
  changed (below). kiln's workspace excludes `vendor/slates`, so kiln's lints don't apply here and Slates' do. Run in
  this folder, cargo takes Slates' toolchain (`rust-toolchain.toml`, 1.98.1); kiln builds on stable.
- **How kiln links it:** `crates/kiln` takes `slates-api` by name, pointed here by the root manifest's
  `[patch.crates-io]`; the library takes Slates' other crates by path, as upstream's root manifest points them, so
  every kiln build on every platform builds them with kiln's toolchain and profile. kiln's release builds unwind on a
  panic where Slates' abort, so a Slates role's process (`kiln slates-role`) aborts once a panic is reported
  (`workspace::slates::role_main`).

## Changes from upstream

Each change is recorded in `vendor/patches/slates/`, applied in this order on upstream's tree (`vendor/MANIFEST.toml`
names the commit and what's left out; kiln's docs/vendoring.md). Each change to Slates' code is offered to Slates as a
branch, and Slates' agent is told of it; a vendoring change alone (the first below) isn't.

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.toml` | each of hyper-raft's crates taken by path from kiln's copy (`../hyper-raft/crates/hyper-*`), not from `vendor/hyper-raft/*`; the root's `[patch.crates-io]` points aws-lc-rs at kiln's copy (`../aws-lc-rs/aws-lc-rs`) and leaves aws-lc-sys crates.io's | kiln holds one copy of each: two linked copies of hyper-raft fail CP-M1's one-revision test, and Slates' own copies (its `vendor/`) aren't here. A path dependency can't be patched, so the paths change | `0001-its-crates-on-kiln-s-one-hyper-raft-and-aws-lc-rs.patch` |

## Taking a newer upstream

`python3 scripts/vendor.py slates <commit>` takes Slates at that commit, re-applies the changes above by three-way
merge, refreshes them, and runs the gate. A change that no longer applies stops it, naming the change and the files,
with nothing changed here. A new edit here is recorded with
`python3 scripts/vendor.py record slates <title> --why <why>` and gets its row in the table;
`python3 scripts/vendor.py check` (in the gate) fails while an edit isn't recorded.

A newer commit of Slates may take hyper-raft at a revision kiln's copy doesn't hold (its `vendor/hyper-raft/SNAPSHOT`
names it). Slates' crates build on kiln's copy here, so a revision whose API kiln's copy lacks stops the build, not
the re-vendor: take hyper-raft at a revision holding both (as Mantle's `UPSTREAM.md` says for Mantle), then Slates.
