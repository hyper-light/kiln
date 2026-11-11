# Origin

W05-R23 / W13-R01. Taken as kiln's own from hyper-light/hex. The initial provider
pin was `37021c35e64b2a8bab1d35f20f92a0042d874a44`; the native Apple provider and
full identity update is pinned to `c2da38cc363e7d53f6df8fb82ab7bbda5e2f1efe`.
All 13 files under `src/` are byte-identical to that newer Hex source; hashes and
source paths are in `HEX-SOURCE.json`. The package name/workspace metadata are
kiln's, and the hardware fixture imports `kiln_gpu`. Hex's native adapter light
checks do not establish Kiln native behavior; Kiln validates the combined port.

NVIDIA uses retained NVML handles, AMD retained amdgpu descriptors or an ADL
context, and Apple Silicon uses native Metal registry IDs/working-set budgets,
exact-entry IOKit mapped-allocation counters and IOReport GPUPH residency. Apple's
registry ID is scoped to the host boot. Its memory budget is not physical VRAM
or host RAM, and its observed allocation can exceed the budget. A missing counter
stays a typed error; package-wide load is not copied across ambiguous identities.
There is no periodic worker, command-based Apple inventory or guessed idle value.

Foreign calls remain isolated in `src/nvml/sys.rs`, `src/adl/sys.rs` and the
ported `src/apple/sys.rs`. Their original SAFETY contracts are retained. Apple's
new boundary owns and releases its native references and subscription; its
snapshots may allocate, so the Linux/Windows allocation-free sample claim does
not extend to Apple. Workspace unsafe remains denied elsewhere. No installed
driver is redistributed, replaced or installed.

The same cuda-rust and all-smi pins, licenses, adapted files and patches remain
in kiln's `vendor/` tree. Apple's adapter retains its Apache-2.0 all-smi license
in `src/apple/LICENSE.all-smi`; it adapts all-smi `fe479ae675e39475a2eab1cfbb3f5432f705e12b`
with owned subscription cleanup and native Metal/IOKit identity and memory reads.
The original Hex decision record is `docs/gpu-telemetry-origin.md`. No Shards code,
physical attachment bridge or incomplete runtime recovery path is copied here.
