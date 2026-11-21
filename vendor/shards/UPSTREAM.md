# Shards, vendored

The microVMs kiln's harnesses run in (`docs/shards.md`; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16), at
the one Shards revision kiln and Hex build on.

- **Upstream:** https://github.com/hyper-light/shards, commit `7158020b61d35277e74a6c06da1418dd0794b04e`
  (2026-11-18), MIT (`LICENSE`): its tag `v0.8.0`, the one revision kiln's copy of Hex pins too (Hex `84c870b0f`, which
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
| `crates/vmm/src/devices/virtio/fs/{mod,resync}.rs`, `crates/ipc/src/{lib,unix}.rs`, `crates/api/src/image.rs`, `crates/api/src/image/{wire,session,pool}.rs`, `crates/shards/src/{api_run,share,warm}.rs` | Shards' D288 (its branch `agents/w-k90-microvm-resync-d288`, b9006561b): a served share's device takes slates' resync channel and replaces a failed stream with the one a successor daemon hands over; the image channel's version 53 (`ServedAt::resync`), `kind::SERVED_RESYNCS` and `kind::SHARE_RESYNC` carry it from the orchestrator to the VM's slot; Windows refuses one by field | kiln goal 90: a Slates daemon restart must not end an agent microVM's share; held here until Shards' coordinator merges D288, then dropped at the next re-vendor | `0002-a-served-share-outlives-its-server-s-daemon-the-device-takes.patch` |
| `crates/shards/src/{vm_run.rs,cli/mod.rs}`, `crates/testguest/src/linux.rs`, `crates/shards/tests/slates_fs.rs` | `shards restore` takes a served share's stream as `run` does (`--slates-fs-fd`; on Windows `--slates-fs-handle`, to the VM as `--slates-fs`): the VM is restored held, each of its snapshot's shares is given its stream (`Slot::attach`: its saved tables first, its DAX ranges mapped again), one for each share, then it is let go unless `--hold`; a guest scenario `slates_fs_restore` and the real-VM test `a_restored_microvm_keeps_its_slates_share_through_the_stream_its_restore_is_given` | goal 92, Shards' owed item 6: a direct restore refused the flag, so a restored guest's slates share had no server; held here until Shards takes it, then dropped at the next re-vendor | `0003-a-restored-microvm-is-given-its-served-shares-streams.patch` |
| `crates/vmm/src/devices/virtio/fs/{acl.rs,mod.rs,server.rs}`, `crates/testguest/src/linux.rs`, `crates/shards/tests/boot.rs` | Shards' own file server takes `FUSE_POSIX_ACL` with `FUSE_DONT_MASK` and `FUSE_SETXATTR_EXT` where its guest's kernel asks: an access ACL sets the mode from its owner, mask and other entries and is kept only where it says more than a mode, shown after a chmod as the mode says, its set clearing set-group-ID where the kernel asks; a directory's default ACL (a directory's alone, else `EACCES`) gives what is made in it in place of the umask, which the server applies otherwise; what was negotiated is kept last in its saved tables (older tables read as none); `acl.rs` holds slates' rules (its A-302/A-314). Tests: `acl::tests`, `server::tests::g92_*`, and the real guest `boot::a_direct_boots_share_enforces_its_guests_posix_acls` | goal 92, Shards' owed item 2: a guest's ACL on a share Shards serves was kept as data and enforced by nothing; held here until Shards takes it, then dropped at the next re-vendor | `0004-its-own-file-server-enforces-a-guest-s-posix-acls.patch` |
| `crates/vmm/src/devices/virtio/fs/{window_whp.rs,window_whp_guest.rs}`, `crates/vmm/src/hv/whp/mod.rs` | The WHP DAX window's three edge cases (PM M620, Shards baf69d54..c2e4b6bb, docs aside): a range followed is counted before its lock is let go, so a vCPU faulting at it as it is cut or replaced sees neither a stale view nor a lost one; a vCPU waiting at WHP's SIPI wait is let out of it (`leave_wait_for_sipi`); and real-WHP guest tests (`window::guest`) for a view shorter than its range, a section ending within its range, a section viewed from each granule, and several vCPUs at a view followed and cut or replaced. Proven on jupiter (Windows x86-64 WHP): red without the fix 4 passed 2 failed, green 6 passed. | Kiln's vendored Shards predates M620; carried until it takes it (goal 92 item 5). | `0005-the-whp-dax-window-s-three-edge-cases-pm-m620.patch` |

The change is kiln's layout alone (`Kiln-only:`), so it isn't offered upstream. The served-shares change kiln
carried as two more (an image run given shares another server serves) is upstream's own since Shards' main
`7158020b61` (its D287, `kind::SERVED_SHARES`, image channel 52), so neither kiln's copy nor Hex's carries it; nor
Hex's two earlier changes to Shards' image API, upstream's since D246 and D248.

## Taking a newer upstream

`scripts/vendor.py shards <commit>` takes the tree at that commit and re-applies the change above by three-way
merge. Hex's copy pins the same commit (`vendor/hex/vendor/MANIFEST.toml`, as kiln's copy of Hex holds it), so the
two move together: the re-vendor of Shards, then the change that moves Hex's pin and whatever of Shards' API Hex
must take (`vendor/hex/UPSTREAM.md`), then `scripts/vendor.py check`. ci/shards/pack.sh builds what kiln carries
from the new tree.
