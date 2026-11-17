# Hex, vendored

The runtime and scheduler kiln's harness microVMs run under (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, steps 12,
16 and 17; Hex's `docs/design/`), built on kiln's one Shards and hyper-raft.

- **Upstream:** https://github.com/hyper-light/hex, commit `992c5a33ac4136890ab0fe9397aa91851f3d4158`
  (2026-11-17), MIT (`LICENSE`): its `main`, fast-forwarded to the v0.8.0 landing (`root/v080-all`). Its Shards guests on Shards' console (`shards_api::CONSOLE`, hvc0, since
  Shards deleted the 16550 at `35e2de91e`; Hex's D261, D262), its Shards image class in a process that links Hex
  (`hex_node::images::ImageRuntime::new`, and `ImageRuntime::leasing`, made with its orchestrator's lessors), a member's
  run-time arguments (`ImageMember::args`) and the host folders it sees (`ShardsImage::shares`), a box's host use at
  its end (`Runtime::delete_with_host_use`), a member's stdio and end over its wire (`Call::Member`, 0x0017), each peer
  link's negotiated suite and group (`Call::Links` 0x0018, `Answer::Links` 0x8010), a member's MCP bridge
  (`ImageMember::mcp`, `Runtime::mcp`: each connection its harness makes to `/run/shards/mcp` a program of its caller's;
  Shards' D254) and a member's own signals on its run's agent-control endpoint (`Runtime::signal_member`,
  `Call::MemberSignal`; Shards' D238); and, since that landing, a share given to the members it names as they start (`Mount::members`; Shards' D277), the image class as a class of
  its own (`Class::ShardsImage`) that Hex's scheduler admits, warm pools kiln names (`Runtime::keep_pool`, `let_go_pool`,
  `pools`, through Shards' `ImagePools`), an agent's volumes with their bounds (`ShardsImage::volumes`, `Runtime::volumes`,
  `clear_volume`; Shards' D281), a program in a running VM's member (`Runtime::exec_member`), a run that ends on its own
  keeping its final use for its delete, a run's agent-control link (`Runtime::link`), and on Windows a guest's streams
  on two named pipes (Shards' D272). It pins Shards at `8d6029e4d`, kiln's own, with no change of Hex's to it, and
  hyper-raft at `e0613f52`, kiln's own; with the changes below its crates build on kiln's (kiln's clippy and the tests
  of what kiln touches: docs/shards.md).
- **How kiln uses it:** `hex-node`, and through it Hex's runtime, client, QUIC wire, scheduler simulation, health
  and GPU telemetry, by name; kiln's root `[patch.crates-io]` points the name here, and `seccompiler` (third-party
  code only Hex vendors) at `vendor/seccompiler` here. A runner whose kiln carries Shards holds Hex's image runtime
  in its own process, over kiln's carried `shards` (`crate::shards`, `sandbox::hold`), and starts each isolated
  agent's harness as a member of one of its Shards image boxes (`sandbox::hexvm`; the joint plan's step 17;
  docs/isolation.md).
- **What is here:** Hex's crates, root manifest and license; the four records its crates include as they build
  (`bench/competitive/probes/mntns_default.txt`, `bench/native/guest/records.tsv`, and hex-sim's recorded traces
  `bench/sched/kiln-ci-20261022-20261105.trace` and `bench/sched/huawei-r5-day30.fleet.trace`); its record of what it vendors
  (`vendor/MANIFEST.toml`, `vendor/patches`), which `scripts/vendor.py check` holds kiln's pins to as kiln's copy
  holds it; and seccompiler. Not its copies of hyper-raft, Shards, cuda-rust and all-smi, which are kiln's; nor its
  GPU renderer's build inputs and Firecracker's sources, which only Hex's own scripts build; nor its docs,
  benchmarks and CI, which are Hex's.

## Changes from upstream

Each change is recorded in `vendor/patches/hex/`, applied in this order on upstream's tree (docs/vendoring.md).

| File | Change | Why | Recorded as |
|---|---|---|---|
| `vendor/patches/hyper-raft/` | Hex's records of hyper-raft's changes made kiln's. Its hyper-raft and Shards pins have been kiln's own at each re-vendor since Hex `414aa30d` (at this revision `e0613f52` and `8d6029e4d`), so this change's pin parts dropped out as upstream's own | kiln holds one copy of each project, with one record of its changes, and Hex must build on it: one Shards revision serves Hex and kiln (the joint plan's step 16) | `0001-hex-builds-on-kiln-s-shards-and-hyper-raft.patch` |
| `crates/hex-sim/src/lib.rs`, `crates/hex-gpu/src/nvml.rs`, `crates/hex-gpu/src/adl.rs`, `crates/hex-gpu/src/amd.rs` | the files Hex includes by path (hyper-swim's `coordinates.rs`, cuda-rust's loader, all-smi's AMD readers) named at kiln's copies (`vendor/hyper-raft`, `vendor/cuda-rust`, `vendor/all-smi`) | kiln's copy leaves Hex's own copies of them out | `0002-files-hex-includes-by-path-from-kiln-s-copies.patch` |
| `vendor/MANIFEST.toml` | its `shards-gpu` entry left out | it names the GPU renderer build Hex's `scripts/shards-gpu/build.sh` makes, which Hex compiles nothing of and kiln's copy holds nothing of; kiln carries no GPU VM process (docs/shards.md) | `0003-no-gpu-renderer-build-of-hex-s-in-kiln-s-copy.patch` |
| `crates/hex-runtime/Cargo.toml`, `crates/hex-client/Cargo.toml`, `crates/hex-node/Cargo.toml` | their path dependencies on Shards' crates (`shards-abi`, `shards-remote`, `shards-ipc`, `shards-afunix`, `shards-testdir`) named at kiln's copy (`vendor/shards`) | kiln's copy leaves Hex's own copy of Shards out, and a path dependency can't be patched | `0004-hex-s-path-dependencies-on-shards-crates-from-kiln-s-copy.patch` |

The second, third and fourth changes are kiln's layout alone (`Kiln-only:`). The first is offered to Hex as it stands,
with what it leaves for Hex and kiln to agree on:
- Hex's own hyper-raft changes beside the one kiln takes (`Endpoint::negotiated`, kiln's fifth): hyper-transport
  taking hyper-quic without its qlog traces, which would turn serde_json's `preserve_order` off for every crate
  of kiln's build, and its spare buffers grown as needed (`vendor/hyper-raft/UPSTREAM.md`). In kiln's build
  serde_json keeps `preserve_order`, so Hex's JSON maps keep their keys in the order they were written there.
- Hex's own tests at this revision with the change: kiln builds Hex's libraries, never its tests (cargo tests no
  package outside its workspace), so they are unrun here.

## Taking a newer upstream

`scripts/vendor.py hex <commit>` takes the tree at that commit and re-applies the changes above. Where Hex's own
pins then agree with kiln's, the first change's pins drop out of it as upstream's own; where they don't, the
re-vendor leaves the new tree in place and says which, for the change that moves them (scripts/vendor.py's
`holds_its_pins`).
