# Quiver, vendored

The topic store kiln's bus is (`docs/bus.md`), at Quiver's commit `966ea139a18d590afd64cc3e30ba3807302d42b1`, MIT
(`LICENSE`): upstream's tree less its own copies of Focal's bus and hyper-raft (`vendor/focal-bus`,
`vendor/hyper-raft`), which are kiln's (`vendor/MANIFEST.toml`), and its benchmarks' records. Its record of what it
vendors (`vendor/MANIFEST.toml`, `vendor/patches`) stays, so kiln's pins are checked against Quiver's as kiln's copy
holds them (`scripts/vendor.py`'s `holds_its_pins`).

## Changes from upstream

Each change is recorded in `vendor/patches/quiver/`, applied in this order on upstream's tree (docs/vendoring.md).

| File | Change | Why | Recorded as |
|---|---|---|---|
| `vendor/patches/hyper-raft/0005-hyper-transport-says-which-suite-and-group-a-connection.patch` | Quiver's record of kiln's fifth hyper-raft change, byte for byte | kiln's one hyper-raft carries Hex's `Endpoint::negotiated` (kiln's `vendor/hyper-raft/UPSTREAM.md`), and a product that vendors hyper-raft records kiln's changes to it, as Quiver records the rest (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 16) | `0001-quiver-records-kiln-s-hyper-raft-change-for-hex-s-suite.patch` |
| `vendor/MANIFEST.toml` | Quiver's hyper-raft pin moved to kiln's, `045cf3ff` | kiln holds one copy of hyper-raft, which Quiver builds on, and takes it at `045cf3ff`: Shards' agent-control link (`crates/remote`, which Hex's image class drives) reads the negotiated idle timeout hyper-quic's patch Q34 gives (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17) | `0002-quiver-s-hyper-raft-pin-moves-with-kiln-s.patch` |

The change is offered to Quiver as it stands: it is its record of a change it builds with already, through kiln's
copy of hyper-raft.
