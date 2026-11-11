# Hex's changes to all-smi

all-smi (https://github.com/lablup/all-smi) is vendored (vendor/MANIFEST.toml) at
`fe479ae675e39475a2eab1cfbb3f5432f705e12b` (2026-11-09), only the files its `take` list names: `LICENSE`,
`crates/all-smi-amd-plugin/src/reader.rs`, `src/device/readers/amd_adl/ffi.rs` and
`src/device/readers/amd_adl/sensors.rs`. Each recorded change (`vendor/patches/all-smi/*.patch`,
applied in name order) is a row here. `scripts/vendor.py check all-smi` proves the tree is that upstream
plus these, byte for byte.

**What hex-gpu takes, and why only this.** hex-gpu reads AMD GPUs through amdgpu's sysfs counters
(`docs/design/gpu-telemetry.md` G2). all-smi's Linux AMD reader is a companion library around
libamdgpu_top (`crates/all-smi-amd-plugin`), which reads through libdrm's ioctls on the device node and
hands all-smi JSON; its one direct read of an amdgpu counter is `read_gpu_busy_percent`, the fallback when
`gpu_metrics` carries no activity. That reader is taken, and the VRAM counters join it, read the same
way: `mem_info_vram_used` and `mem_info_vram_total`, which all-smi reads by those names for Intel's
discrete GPUs (`src/device/readers/intel_gpu_sysfs.rs`, not taken: its other paths are i915's and xe's)
and amdgpu prints the same way. all-smi's AMD names come from libdrm's `amdgpu.ids` through
libamdgpu_top; hex-gpu reads that table itself (`crates/hex-gpu/src/amd.rs`).

On Windows hex-gpu reads AMD GPUs through ADL (decision G3). all-smi reads them through ADL for sensors
and through WDDM's PDH and DXGI for use (`src/device/readers/windows_gpu_perf/`, not taken: DXGI's
memory figures are the calling process's own, and PDH's are rates over two collections). Its ADL
module is taken in part: `ffi.rs`, its transcription of AMD's ADL SDK headers, and `sensors.rs`, its
read of the PMLog sensor table; its loader (`loader.rs`, libloading) and adapter grouping
(`adapters.rs`) are not, hex-gpu loading ADL as it loads NVML and grouping adapters by PCI address as
AMD's own Display sample does. The ADL calls hex-gpu adds (VRAM usage, memory, versions) are
transcribed from AMD's headers, which are MIT (GPUOpen-LibrariesAndSDKs/display-library at
`84c881cb2136f04287eb3af12de00a9e2867aa9e`: `include/adl_sdk.h`, `include/adl_structures.h`,
`include/adl_defines.h`, and the prototypes its `Public-Documents` give for `adapter.h`, `overdrive8.h`
and `graphics.h`); they are cited, not taken.

**License.** Apache-2.0 (`LICENSE`, taken unchanged; upstream has no NOTICE file). It allows copying and
changing the code inside Hex's MIT-licensed tree, given that the license travels with it (`LICENSE`), the
copyright and license lines stay in the file (they do), and a changed file says it was changed (its
header names Hex's change and this file).

| Files | Change | Why | Patch |
| --- | --- | --- | --- |
| `crates/all-smi-amd-plugin/src/reader.rs` | Keeps `read_gpu_busy_percent` and `parse_gpu_busy_percent`, reading from a file opened once with one `pread` at offset 0 into a stack buffer and parsing the line in place; a percent past 100 is refused rather than clamped; `EPERM` (a runtime-suspended GPU) reads as asleep and `ENODEV` (a removed device) as gone. Adds `read_vram_bytes` for `mem_info_vram_used` and `mem_info_vram_total`, read the same way. Removes the rest: libamdgpu_top's reader, its sensor, power and process paths, and the plugin glue. | A sample is the scheduler's hot path: no path joined, file opened or string built per read, and sysfs runs the attribute's `show` again for every read from offset 0 (fs/seq_file.c). CLAUDE.md §3: a value no counter can hold is refused, never altered. amdgpu answers a suspended GPU's busy counter `EPERM` rather than wake it (drivers/gpu/drm/amd/pm/amdgpu_pm.c), and kernfs answers an open file of a removed device `ENODEV` (fs/kernfs/file.c). Hex reads the counters directly and links neither libamdgpu_top nor libdrm. | `0001-reader-keep-the-gpu-busy-percent-reader-add-the-vram.patch` |
| `src/device/readers/amd_adl/ffi.rs`, `src/device/readers/amd_adl/sensors.rs` | Keeps ADL's context, adapter count, `AdapterInfo` (Windows layout, with its row sanity check and headroom array) and PMLog types and entry points, and the GFX activity sensor read. Adds, from AMD's headers, `ADL2_Adapter_VRAMUsage_Get`, `ADL2_Adapter_MemoryInfo2_Get` with `ADLMemoryInfo2`, `ADL2_Graphics_VersionsX2_Get` with `ADLVersionsInfoX2`, and `ADL_MAX_ADAPTERS`. Removes the temperature, power, fan and clock readings, Overdrive's capability call, the poison-fill row states and the tests. Layout assertions become compile-time array-length checks; fields hex-gpu never reads take a leading underscore; the activity reading answers a typed refusal (unpublished, or outside 0 to 100) instead of `None`; casts become checked conversions. | hex-gpu's Windows AMD reading (G3) calls exactly these: two calls a GPU a sample, PMLog's GFX activity and the VRAM in use, and at open the adapters, each one's memory and the driver's versions. Hex's clippy denies `assert!`, `as` casts that can narrow, and dead code (CLAUDE.md §1, §13); a value no counter can hold is refused, never altered (CLAUDE.md §3). Every declaration was checked against AMD's headers, and jupiter's driver (a Radeon 8060S) filled each as declared. | `0002-amd-adl-keep-adl-s-types-entry-points-and-gfx-activity-read.patch` |
