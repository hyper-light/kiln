# Hex, vendored

The runtime and scheduler kiln's harness microVMs run under (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, steps 12,
16 and 17; Hex's `docs/design/`), built on kiln's one Shards and hyper-raft.

- **Upstream:** https://github.com/hyper-light/hex, commit `106c7b94c93edc0fd7e8fb9004503c26c32a0030`
  (2026-11-12), MIT (`LICENSE`): Hex's `main` as kiln found it, with the joint plan's steps 13 and 14 merged (over
  `d84730ee`, kiln's pin before, it adds only its design index, which kiln's copy leaves out). Its steps 4 and 12 aren't
  on it yet, and a re-vendor takes them once they land. Hex's lead
  is gone (2026-11-12), so kiln chose it: it pins Shards at `482c5d78` and hyper-raft at `0856e05`, both older than
  kiln's, and with the first change below its crates build on kiln's (`cargo check -p hex-node` in kiln's
  workspace, then kiln's clippy and tests, on macOS, Linux and Windows: docs/shards.md).
- **How kiln uses it:** `hex-node`, and through it Hex's runtime, client, QUIC wire, scheduler simulation, health
  and GPU telemetry, by name; kiln's root `[patch.crates-io]` points the name here, and `seccompiler` (third-party
  code only Hex vendors) at `vendor/seccompiler` here. Nothing of Hex runs in kiln yet: kiln's launch seam takes it
  up (the joint plan's step 17), its node given kiln's carried `shards` (`crate::shards`) as its `SHARDS_BIN`.
- **What is here:** Hex's crates, root manifest and license; the two records its crates include as they build
  (`bench/competitive/probes/mntns_default.txt`, `bench/native/guest/records.tsv`); its record of what it vendors
  (`vendor/MANIFEST.toml`, `vendor/patches`), which `scripts/vendor.py check` holds kiln's pins to as kiln's copy
  holds it; and seccompiler. Not its copies of hyper-raft, Shards, cuda-rust and all-smi, which are kiln's; nor its
  GPU renderer's build inputs and Firecracker's sources, which only Hex's own scripts build; nor its docs,
  benchmarks and CI, which are Hex's.

## Changes from upstream

Each change is recorded in `vendor/patches/hex/`, applied in this order on upstream's tree (docs/vendoring.md).

| File | Change | Why | Recorded as |
|---|---|---|---|
| `vendor/MANIFEST.toml`, `vendor/patches/hyper-raft/`, `crates/hex-runtime/src/grants.rs`, `crates/hex-runtime/src/grants/tests.rs`, `crates/hex-runtime/src/guest/image.rs`, `crates/hex-runtime/src/guest/image_tests.rs` | Hex's pins moved to kiln's Shards (`e82b9403`) and hyper-raft (`fb74b595`), its records of hyper-raft's changes made kiln's, and its image class on Shards' API there: a narrowing names its destinations (D236; none kept, as egress and MCP, default deny until the joint plan's step 15 programs them), a published port names whom it reaches (`Bound::to`), a spec asks no nesting grant (D234), a boot takes a patience (`Duration::MAX`, as D231's boot was before it took one: the box's own wait bounds the call), and a plug's refusal (D235) is the call's; its tests' model of Shards' image boot takes the same | kiln holds one copy of each project, and Hex must build on it: one Shards revision serves Hex and kiln (the joint plan's step 16) | `0001-hex-builds-on-kiln-s-shards-and-hyper-raft.patch` |
| `crates/hex-sim/src/lib.rs`, `crates/hex-gpu/src/nvml.rs`, `crates/hex-gpu/src/adl.rs`, `crates/hex-gpu/src/amd.rs` | the files Hex includes by path (hyper-swim's `coordinates.rs`, cuda-rust's loader, all-smi's AMD readers) named at kiln's copies (`vendor/hyper-raft`, `vendor/cuda-rust`, `vendor/all-smi`) | kiln's copy leaves Hex's own copies of them out | `0002-files-hex-includes-by-path-from-kiln-s-copies.patch` |
| `vendor/MANIFEST.toml` | its `shards-gpu` entry left out | it names the GPU renderer build Hex's `scripts/shards-gpu/build.sh` makes, which Hex compiles nothing of and kiln's copy holds nothing of; kiln carries no GPU VM process (docs/shards.md) | `0003-no-gpu-renderer-build-of-hex-s-in-kiln-s-copy.patch` |

The second and third changes are kiln's layout alone (`Kiln-only:`). The first is offered to Hex as it stands,
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
