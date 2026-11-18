# Shards, vendored

The microVMs kiln's harnesses run in (`docs/shards.md`; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16), at
the one Shards revision kiln and Hex build on.

- **Upstream:** https://github.com/hyper-light/shards, commit `04481e36be379b1c0a26575e66ef9c254be87865`
  (2026-11-17), MIT (`LICENSE`): its `main`, the one revision kiln's copy of Hex pins too (Hex `955e15c06`, which
  makes no change to Shards): its guests' console is `shards_api::CONSOLE` (hvc0) since it deleted the 16550
  (`35e2de91e`), and `host_check` says whether a host runs its microVMs. Since `1389dc0b6` it has a member's run-time arguments (D256), and Hex's image
  class drives its agent-control link's client (`shards-remote`, whose QUIC is kiln's hyper-raft's). Beside
  what `e82b9403` had (D237's `shards` built to be carried, `shards-afunix`'s fix of a read that slept through its
  peer's end, PM M536), it has each member's reach of the destinations kept (D246) and the run host started
  holding its three standard streams alone (D248).
- **How kiln uses it:**
  - its Agentfile parser and build checks, `shards-agentfile`, linked: every Agentfile kiln keeps is checked in
    kiln's own process (`crates/kiln/src/agentfiles/check.rs`);
  - its orchestrator's API, `shards-api`, linked: Hex's client of Shards (`vendor/hex`), and kiln's own;
  - its binary, `shards`, built to be carried, and its VM and network processes: built from here by
    `ci/shards/pack.sh`, embedded in kiln's binary by `crates/kiln/build.rs`, written out and signed by kiln
    (`crates/kiln/src/shards.rs`).

  kiln's workspace excludes `vendor/shards`, which keeps its own workspace, lock file, toolchain
  (`rust-toolchain.toml`: its builds here take its Rust and the musl targets its guests need) and lints. kiln's
  root `[patch.crates-io]` points `shards-api` and `shards-agentfile` here, and `rquickjs-sys` at
  `vendor/rquickjs-sys`.
- **What is here:** upstream's tree less what no build reads: `docs/`, `examples/`, `fuzz/`, `.github/`, `.kiln/`;
  its scripts but `zig-cc`, `zig-ar`, `install-zig` and `zig-pin` (the C compiler its guests' build names) and `hvf-run`
  (cargo's runner on macOS, `.cargo/config.toml`); its crates' recorded test data (each `testdata/`, which only
  their tests include); and its copies of aws-lc-rs and aws-lc-sys (the change below). `vendor/rquickjs-sys`
  (crates.io's rquickjs-sys 0.14.0, QuickJS-NG's C) stays: no other project kiln vendors holds it, so it is kiln's
  one copy, which kiln's own `rquickjs` builds too.

## Changes from upstream

Each change is recorded in `vendor/patches/shards/`, applied in this order on upstream's tree (docs/vendoring.md).

| File | Change | Why | Recorded as |
|---|---|---|---|
| `Cargo.toml`, `Cargo.lock` | aws-lc-rs 1.18.1 and aws-lc-sys 0.45.0 from crates.io, their lock entries holding the SHA-256s upstream's `vendor/README.md` lists, instead of from `vendor/` | upstream's copies are those crates.io packages byte for byte, and kiln vendors aws-lc-rs itself (`vendor/aws-lc-rs`, 1.18.2 with Mantle's change): kiln's tree holds one copy of each project. Shards' own crates name 1.18.1 and aws-lc-sys 0.45.0 (`shards-sigstore` calls it), so its build keeps those versions; as T4 took it on 2026-10-21 | `0001-aws-lc-rs-and-aws-lc-sys-from-crates-io.patch` |
| `crates/api/src/image.rs`, `crates/api/src/image/{wire,pool,session}.rs`, `crates/ipc/src/lib.rs` | Shards' D286, its API side: an image run given shares another server serves (`ImageSpec::served`, `ServedShare`, `ServedStream`; image channel 52, `wire::Request::served`, `ServedAt`; the streams handed beside the request, on Unix on the run host's links socket, `shards_ipc::kind::SERVED_SHARES` (117), on Windows duplicated into its process; no pool's, no save) | kiln's microVM engine shares each agent's Slates volume into its guest over Slates' own virtio-fs device, a stream handed to Shards with no host mount between (kiln goal 01a1ec34599974448f6b7663ddb07001). Hex's copy carries the same change, byte for byte (`vendor/hex/vendor/patches/shards/`). Upstream on Shards' branch `agents/g265-kiln-shards-microvm-served-image-share` (4ff40fc99 on its main 4776b2e56; e4e0118c8 the same change on this revision), which ROOT lands after Shards' v0.8.0 tag; dropped once kiln and Hex take a Shards revision holding it | `0001-shards-api-shards-ipc-an-image-run-takes-shares-another.patch` |
| `crates/shards/src/{api_run.rs,cli/mod.rs,daemon.rs}`, `crates/shards/tests/{api_image,member_exec,nesting}.rs` | Shards' D286, its run host's side: `api_run::served_streams` takes the streams with the run's request, the microVM's shape counts their slots, `hosted::serve` gives each the slot after the run's own shares' and mounts it as a bind's, the share process passes each to the VM as it is (D228's path; on Windows the run host hands the VM the pipe), and a save of a run holding one is refused | the `shards` kiln carries (`ci/shards/pack.sh`) is the run host kiln's `shards-api` starts, so both sides of the image channel's version 52 are built from here. `Kiln-only:` in form alone: Hex's API-only copy takes no `crates/shards`, so no product holds it; upstream with the change above | `0002-shards-api-run-an-image-run-s-run-host-takes-shares-another.patch` |

The first change is kiln's layout alone (`Kiln-only:`), so it isn't offered upstream. The two D286 changes are
upstream-bound, carried until ROOT lands them on Shards' main. Hex's two earlier changes to Shards' image API are
upstream's own since D246 and D248, so neither kiln's copy nor Hex's carries them.

## Taking a newer upstream

`scripts/vendor.py shards <commit>` takes the tree at that commit and re-applies the change above by three-way
merge. Hex's copy pins the same commit (`vendor/hex/vendor/MANIFEST.toml`, as kiln's copy of Hex holds it), so the
two move together: the re-vendor of Shards, then the change that moves Hex's pin and whatever of Shards' API Hex
must take (`vendor/hex/UPSTREAM.md`), then `scripts/vendor.py check`. ci/shards/pack.sh builds what kiln carries
from the new tree.
