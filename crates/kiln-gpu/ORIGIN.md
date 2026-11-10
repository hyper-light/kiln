# Origin

W05-R23 / W13-R01. Taken as kiln's own from hyper-light/hex at
`37021c35e64b2a8bab1d35f20f92a0042d874a44`, `crates/hex-gpu` (MIT).
Production source and unit tests are unchanged; the package is named `kiln-gpu`, its manifest
uses kiln's workspace metadata, and its hardware test imports the new name. The original
Hex decision record is retained in `docs/gpu-telemetry-origin.md`.

The existing native FFI boundaries remain `src/nvml/sys.rs` and `src/adl/sys.rs`, with their
original safety contracts. They load installed vendor drivers at runtime, retain handles and
contexts, and keep all foreign calls out of scheduling logic. These are the same NVML and ADL
blocks approved by Ada for Hex (2026-11-09), ported under her request to use the same providers
and GPU balancing. Every unsafe block carries its source SAFETY comment; workspace unsafe
remains denied elsewhere. No driver binary is redistributed or installed.

The same cuda-rust and all-smi pins, licenses, adapted files and patches are in kiln's one
`vendor/` tree. `scripts/vendor.py check cuda-rust all-smi` checks those against upstream.
No Shards code or policy-controlled dependency is copied or changed.
