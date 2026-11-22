# Amphora, vendored

The database kiln keeps its records in (kiln's critical path T1; its master plan §4, CP-T1.1): KilnDB made a
product, at Amphora's commit `dab76dd9f0f04a8552dc9802c7c73221bc87a1e7`, MIT (`LICENSE`). Its engine
(`crates/engine*`, the crates `amphora-engine`, `amphora-engine-parser`, `amphora-engine-macros` and
`amphora-engine-ext`) is Amphora's own code, forked from Turso 3e09a82 (`crates/engine/ORIGIN.md`), so it can't
collide with kiln's `turso_core` (`vendor/turso`).

- **What is here:** upstream's tree less its own copy of hyper-raft (`vendor/hyper-raft`), which is kiln's
  (`vendor/MANIFEST.toml`), and its benchmarks, docs and CI (`bench`, `docs`, `.kiln`), which are Amphora's and no
  part of what kiln builds. Its record of what it vendors (`vendor/MANIFEST.toml`, `vendor/patches`) stays, so kiln's
  pins are checked against Amphora's as kiln's copy holds them (`scripts/vendor.py`'s `holds_its_pins`).
- **How kiln uses it:** the crate `amphora`, by name: kiln's root `[patch.crates-io]` points `amphora` and
  `amphora-api` here, and Amphora's crates take hyper-raft's by name, which the same patch points at kiln's one copy
  (`vendor/hyper-raft`). kiln builds Amphora's libraries, never its tests, whose records (its `docs`) stay upstream.

## Changes from upstream

Each change is recorded in `vendor/patches/amphora/`, applied in this order on upstream's tree (docs/vendoring.md).

| File | Change | Why | Recorded as |
|---|---|---|---|
| `vendor/MANIFEST.toml`, `vendor/patches/hyper-raft/` | Amphora's hyper-raft pin moved to kiln's, `e0613f52` (a descendant of Amphora's `d5d5f8f6`), and its records of hyper-raft's changes made kiln's, byte for byte: kiln's 0001 and 0003 to 0007 (its 0002 is `Kiln-only:`) | kiln holds one copy of each project, with one record of its changes. Amphora's own change kiln lacked, the held count fed on ticks (CP-T1.8), is kiln's hyper-raft 0007; its Darwin FIFO change is hyper-rt's, which kiln builds from `crates/kiln-rt`; its keying-material change (CP-T2.2) no Amphora crate calls | `0001-amphora-builds-on-kiln-s-hyper-raft.patch` |

The change is offered to Amphora as it stands: Amphora builds on kiln's hyper-raft with it (its replicated class
calls `Replica::suspect_held` and `trust_held`, kiln's hyper-raft 0007).
