# Hex's changes to cuda-rust

cuda-rust (https://github.com/NVIDIA/cuda-rust) is vendored (vendor/MANIFEST.toml) at
`a0a4dc78a20e06ec90827f597e950bdc49f723e2` (2026-11-09), only the files its `take` list names: `LICENSE`
and `cuda-bindings/src/dyn_load.rs`. Each recorded change (`vendor/patches/cuda-rust/*.patch`, applied in
name order) is a row here. `scripts/vendor.py check cuda-rust` proves the tree is that upstream plus
these, byte for byte.

**What hex-gpu takes, and why only this.** hex-gpu reads NVIDIA GPUs through NVML
(`docs/design/gpu-telemetry.md` G1). cuda-rust binds the CUDA driver and cuRAND, not NVML, and the CUDA
driver has no utilisation counter, and reads a device's memory in use only from inside a context made on
it. What carries over is `cuda-bindings`' run-time loading policy: try each library name in turn, WSL's
own path among them, keep the library loaded for the process, and tell a library that didn't load from
one that loaded without a symbol. The rest of cuda-rust (bindgen's generated tables, the toolkit
discovery in `build.rs`, `cuda-core`'s contexts) needs the CUDA toolkit at build time, which Hex doesn't
have, and does nothing hex-gpu needs. NVML's eight calls are declared in `crates/hex-gpu/src/nvml/sys.rs`
from NVIDIA's NVML reference (`nvml.h`), not from cuda-rust.

**License.** Apache-2.0 (`LICENSE`, taken unchanged; upstream has no NOTICE file). It allows copying and
changing the code inside Hex's MIT-licensed tree, given that the license travels with it (`LICENSE`), the
copyright and license lines stay in the file (they do), and a changed file says it was changed (its
header names Hex's change and this file).

| Files | Change | Why | Patch |
| --- | --- | --- | --- |
| `cuda-bindings/src/dyn_load.rs` | Loads NVML (`libnvidia-ml.so.1`, then WSL's `/usr/lib/wsl/lib/libnvidia-ml.so.1`; `nvml.dll` from System32) in place of the CUDA driver and cuRAND, through hex-gpu's own FFI file instead of libloading; `load_api` keeps upstream's try-each-name loop with a typed error in place of its `expect`, and a library that loads without a symbol stops the search; `nvml()` initialises NVML once and keeps it, caching only success. `DynLoadError` gains `MissingSymbol` and `InitFailed` in place of `RuntimeTooOld`. | NVML is what reads a GPU's utilisation and memory; Hex links neither libloading nor bindgen's output (CLAUDE.md §12 keeps `unsafe` to named files); no panics (CLAUDE.md §1); handles from one initialisation stay valid for the process, so a sample never re-opens anything; a failed load is tried again, so a driver loaded after the node started is found at the next open. | `0001-dyn-load-load-nvml-in-place-of-the-cuda-driver-and-curand.patch` |
