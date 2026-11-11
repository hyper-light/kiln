# GPU telemetry: a node's GPUs and their load (hex-gpu)

Ada, 2026-11-09: "We should also make sure to incorporate GPU load in the balancing", then "Support NVIDIA
and AMD GPUs to start", from NVIDIA/cuda-rust and lablup/all-smi, vendored rather than depended on "so we
can make it properly performant". `crates/hex-gpu` gives the node what the scheduler balances on beside
CPU load (`AC-T7.3.1`: placement by CPU, memory, disk and devices; `scheduler-architecture.md` B3, the
node's room, and B4, its pace): each GPU's identity once (`Gpus::inventory`) and its load per sample
(`Gpus::sample`). The node-facts probe being ported into hex-sim will call both.

Sources, each read on 2026-11-09:
- **ADL:** AMD's ADL SDK (GPUOpen-LibrariesAndSDKs/display-library at `84c881cb`, MIT): its headers,
  its documented prototypes, its samples; and what jupiter's ADL (driver 25.20.32.06, a Radeon 8060S)
  answered to each call.
- **NVML:** NVIDIA's `nvml.h`, NVML 13.4.92, as NVIDIA/go-nvml carries it (`gen/nvml/nvml.h` at
  `f83235b8`); what adacachy2's NVML (driver 620.70.06, a GeForce RTX 5060 Ti) answered to each call.
- **NVIDIA's driver:** open-gpu-kernel-modules at 620.70.06,
  `kernel-open/common/inc/nv-chardev-numbers.h`.
- **Linux** (torvalds/linux master, `7475ea8b`): `drivers/gpu/drm/amd/pm/amdgpu_pm.c`
  (`gpu_busy_percent`, `amdgpu_pm_get_access_if_active`), `drivers/gpu/drm/amd/amdgpu/amdgpu_vram_mgr.c`
  (`mem_info_vram_*`), `fs/seq_file.c` (`seq_read_iter`), `fs/kernfs/file.c` (`kernfs_seq_start`,
  `kernfs_file_read_iter`), `drivers/gpu/drm/drm_drv.c` (`DRM_MINOR_LIMIT`, `DRM_EXTENDED_MINOR_LIMIT`),
  `include/uapi/linux/utsname.h` (`__NEW_UTS_LEN`).
- **libdrm** 2.4.134's `amdgpu.ids`, as adacachy2 has it.
- **The vendored sources:** NVIDIA/cuda-rust at `a0a4dc78`, lablup/all-smi at `fe479ae6`
  (`vendor/cuda-rust/UPSTREAM.md`, `vendor/all-smi/UPSTREAM.md` say what was taken and changed).
- **Portability:** Ada's rule (2026-11-09): "windows, macos, and linux (of most any major flavor)
  compatible. AMD 64 and ARM64 excepting MacOS x86_64": hex-gpu builds and lints for x86_64 and aarch64
  Linux (gnu and musl), aarch64 macOS, and x86_64 and aarch64 Windows.

## G1. NVIDIA through NVML, loaded at run time, two calls a GPU a sample

**Decision.** NVML is loaded at run time (`dlopen` of `libnvidia-ml.so.1`, then WSL's
`/usr/lib/wsl/lib/libnvidia-ml.so.1`; `nvml.dll` from System32 alone on Windows) by cuda-rust's loader
policy, adapted (`vendor/cuda-rust`), initialised once for the process and never shut down. Open reads
each GPU's name, PCI bus id, total memory and the driver's version once and keeps its handle. A sample
makes two calls a GPU, `nvmlDeviceGetUtilizationRates` and `nvmlDeviceGetMemoryInfo`, into the
caller's frame; a GPU the first finds lost isn't asked the second. GPUs are listed in PCI order. A device
NVML counts but won't give is skipped with NVML's code, and a count past the driver's own 248 devices
is refused.

**Why.** NVML is the only interface to a GPU's utilisation: the CUDA driver has none, and reads a
device's free memory only inside a context made on it (cuda-rust binds the driver and cuRAND, not NVML,
which is why only its loader is taken). Two calls is NVML's floor: no field value
(`nvmlDeviceGetFieldValues`, `NVML_FI_DEV_*`) carries utilisation or framebuffer use, and no call reads
two devices. A handle stays valid until `nvmlShutdown`, which Hex never calls, and NVML is thread-safe
(nvml.h), so handles kept from open serve every sample from any thread. `nvmlDeviceGetCount_v2` counts
devices this process may not use and `nvmlDeviceGetHandleByIndex_v2` then refuses them
(`NVML_ERROR_NO_PERMISSION`); its order "has no guarantees of consistency between reboots" (nvml.h), so
the inventory orders by PCI address. NVIDIA's Linux driver numbers GPUs on character-device minors 0 to
247 (`NV_MINOR_DEVICE_NUMBER_REGULAR_MAX`), so a larger count is malformed. `used` from
`nvmlDeviceGetMemoryInfo` is reserved and allocated together: what the next allocation can't have.
Loading at run time lets a node without NVIDIA's driver start and say why it reads no NVIDIA GPU;
a failed load isn't cached, so a driver loaded after the node started is found at its next open.

**Evidence.** On adacachy2, `tests/hardware.rs` listed the RTX 5060 Ti as nvidia-smi does (name, bus
id, total memory to the MiB, driver version) and sampled it twice for no allocation. The fixture tests
(`src/nvml/tests.rs`, adacachy2's recorded answers and nvml.h's documented errors) show two calls a GPU
a sample and nothing read at open asked again, a lost GPU costing one call, a MIG GPU's memory read
without its utilisation, a device without permission skipped with its code, and a missing library
refused with the names tried.

**What would falsify it.** An NVML that carries utilisation and framebuffer use in one call or one
field set: the sample takes it. A GPU nvidia-smi lists that hex-gpu neither lists nor skips.

## G2. AMD through amdgpu's sysfs counters, one `pread` a counter a sample

**Decision.** Open lists the DRM class's `card<N>` entries whose device amdgpu drives (its `driver`
link), reads each one's PCI address (its `device` link), ids, VRAM total and the driver's version once,
names it from libdrm's `amdgpu.ids` (else by its PCI ids, `1002:13c0 rev e5`), and opens
`gpu_busy_percent` and `mem_info_vram_used`. A sample makes one `pread` at offset 0 of each into a stack
buffer and parses the line in place, the reader all-smi's (`vendor/all-smi`), adapted. `EPERM` from the
busy counter reads as asleep, `ENODEV` from either as gone; a line that isn't a number, or a percent
past 100, is refused as malformed. amdgpu's version is a DKMS module's own
(`/sys/module/amdgpu/version`), else the kernel's release, which is the in-tree driver's.

**Why.** These are the counters amdgpu publishes for exactly this, world-readable: no device node, no
ioctl, no library. all-smi reads AMD GPUs through libamdgpu_top, which opens the device node (the
`render` or `video` group's) and goes through libdrm's ioctls; its one direct counter read is
`gpu_busy_percent`, the reader taken. sysfs runs the attribute's `show` again for every read from offset
0 (`seq_read_iter` resets at position 0), so a kept-open file gives the value now for one system call,
with nothing reopened, sought or walked. `gpu_busy_percent` is the SMU firmware's load over the GPU's IP
cores; amdgpu answers it `EPERM` for a runtime-suspended GPU rather than wake it
(`amdgpu_pm_get_access_if_active`), and that GPU runs nothing; `mem_info_vram_used` needs no wake. An
open file of a removed device answers `ENODEV` (kernfs). On an APU, VRAM is the firmware's carve-out
(adacachy2's Radeon 610M: 2 GiB); the system memory it maps besides (GTT) isn't counted.

**Evidence.** On adacachy2, `tests/hardware.rs` listed the Radeon 610M with the VRAM total sysfs gives
and sampled it twice for no allocation. The fixture tests (`src/amd/tests.rs`, adacachy2's DRM class as
captured) show the GeForce, connectors and render nodes passed over, the table's names and the
versions, fresh values each sample through the files opened once, an unplugged card gone for one read,
an asleep one still reading its memory, and hostile counters and tables refused without a panic.

**What would falsify it.** A kernel whose `gpu_busy_percent` disagrees with amd-smi's GFX activity at
one instant, or whose `pread` at offset 0 of a kept-open attribute returns a stale value, or that
answers a removed device's counter otherwise than `ENODEV`.

## G3. Windows' AMD through ADL, two calls a GPU a sample; platforms

**Decision.** On Windows, AMD's GPUs are read through ADL, AMD's Display Library (`atiadlxx.dll`,
which AMD's driver installs in System32), loaded at run time as NVML is and kept for the process, with
one ADL context per `Gpus`, made at open and ended with it. Open lists ADL's adapters: rows of AMD's
vendor id (`1002`, which ADL writes in decimal) are one GPU per PCI address, the first row's adapter
index reading it, as AMD's own Display sample groups them (`Sample/Display/Display/Adl.cpp`); another
vendor's rows are passed over and a row whose strings don't read is skipped as malformed. Each GPU's
model is ADL's adapter name, its memory `ADL2_Adapter_MemoryInfo2_Get`'s size, and the driver's
version its release (`ADL2_Graphics_VersionsX2_Get`'s Crimson version, else its packaging version). A
sample makes two calls a GPU: `ADL2_New_QueryPMLogData_Get`, whose sensor 19
(`ADL_PMLOG_INFO_ACTIVITY_GFX`) is the busy percent, read into a table on the stack, and
`ADL2_Adapter_VRAMUsage_Get`, the VRAM in use in megabytes of 2^20 bytes. ADL's codes map onto
`Unread`: an index ADL no longer numbers (-5) or a disabled adapter (-10) is gone, a power state that
answers nothing (-25) asleep, a function the driver doesn't offer (-8) unsupported. ADL names no PCI
segment, so Windows' AMD GPUs are read on segment 0. Linux reads both vendors (G1, G2); macOS reads
neither, `Absent::Unsupported` for both, NVIDIA's driver, amdgpu and ADL not running there; where a
vendor's library isn't installed (ARM64 Windows, which AMD's driver doesn't support) it is
`Absent::NoLibrary`.

**Why.** DXGI, the other source asked about, can't give a GPU's memory in use:
`IDXGIAdapter3::QueryVideoMemoryInfo` reports the calling process's own budget and use, not the GPU's
(all-smi's `windows_gpu_perf/dxgi.rs` says so and takes the system-wide figure from PDH), and DXGI has
no utilisation at all. WDDM's PDH counters (`\GPU Engine(*)\Utilization Percentage`, Task Manager's
source) serve any vendor but are a rate over two collections and a sum over engines and processes,
more work a sample than one call. ADLX, ADL's successor, is a C++ interface whose GPU metrics come
as a reference-counted object per read (`GetCurrentGPUMetrics`, `IPerformanceMonitoring.h`), then a
call per value and its release, under a licence agreement of its own; ADL's SDK headers are MIT. Through ADL,
activity and memory are one call each, the floor its interface allows: no call reads both, and PMLog
publishes no VRAM use. On jupiter (Radeon 8060S, an APU) ADL refuses Overdrive (`ADL2_Overdrive_Caps`:
-8) yet fills PMLog, so PMLog is read without asking Overdrive. Every row of jupiter's GPU has
`iPresent` 0 and reads inactive, so neither filters. ADL's megabyte is 2^20 bytes: PDH's
`\GPU Adapter Memory(*)\Dedicated Usage` read 61386752 bytes as `ADL2_Adapter_DedicatedVRAMUsage_Get`,
which reads that counter, said 58 MB (58.5 of 2^20, floored). `VRAMUsage` (73 MB there) is the
driver's own count and costs no counter query; ADL never called the allocator it is handed for
memory it returns, so Hex's refuses (all four of Hex's buffers are its own). The types and entry
points are all-smi's transcription of AMD's headers (`vendor/all-smi`, `src/device/readers/amd_adl/`),
checked against them (GPUOpen-LibrariesAndSDKs/display-library at `84c881cb`: `adl_sdk.h`,
`adl_structures.h`, `adl_defines.h`, and the documented prototypes of `adapter.h`, `overdrive8.h`,
`graphics.h`), their sizes pinned at compile time.

**Evidence.** On jupiter, `tests/hardware.rs` listed the 8060S by the name Windows' own video
controller list gives it and sampled it twice for no allocation. The fixture tests (`src/adl/tests.rs`,
jupiter's recorded rows, table, memory, versions and refusals) show five rows grouped into one GPU, two
calls a GPU a sample, a GPU gone or disabled costing one call, an asleep one still reading its memory,
an unpublished or out-of-range activity and a foreign table size refused by type, other vendors'
rows passed over and garbled ones skipped; they run on every platform.

**What would falsify it.** An AMD driver whose PMLog GFX activity disagrees with Task Manager's 3D
engine at one instant, or whose `VRAMUsage` drifts from PDH's dedicated usage beyond the megabyte;
an AMD GPU on Windows that ADL lists on a segment other than 0.

## G4. The interface

`Gpus::open()` does every costly thing once. `inventory()` gives `&[Gpu]` (vendor, model, total memory
in bytes, PCI address, driver version), NVIDIA's first and each vendor's in PCI order. `sample()` gives
`&[Load]` in that order, a `Result` per counter (`busy_percent: Result<u8, Unread>`,
`memory_used_bytes: Result<u64, Unread>`) so one can fail while the other reads, written into room
reserved at open: no allocation, spawn or path walk; ADL's PMLog table is read into the sampling
frame (8 KiB with its headroom), not the heap. Both return `Err(&NoGpus)`, each vendor's
`Absent` reason, when there is no GPU to read; `skipped()` lists the devices a driver counts that open
couldn't read, with why. A GPU unplugged reads `Unread::Gone`; a new `Gpus::open()` lists what is there
now. `sample()` takes `&mut self` and `Gpus` is `Send` and `Sync`, so the probe owns it on its thread.

**What would falsify it.** A probe that needs a GPU's load more often than NVML's sample period (1/6 s
to 1 s, nvml.h) or the SMU's refresh: it would read the same value twice, and the period becomes the
probe's.
