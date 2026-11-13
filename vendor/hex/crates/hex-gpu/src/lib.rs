//! A node's NVIDIA, AMD and Apple Silicon GPUs and their load (`AC-T7.3.1`). NVIDIA uses NVML;
//! AMD uses amdgpu sysfs on Linux and ADL on Windows; Apple uses Metal identity/budgets, exact-entry
//! IOKit allocation counters and IOReport GPU residency. Sources: `docs/design/gpu-telemetry.md`
//! and `docs/research/measurements/hex-macos-gpu/README.md`.
//!
//! [`Gpus::open`] inventories native [`Identity`] values, models and memory once and retains native
//! handles for [`Gpus::sample`]. Inventory and samples have the same order: vendor, then identity.
//! [`NoGpus`] says why no inventory is readable; [`Skipped`] retains individual device failures,
//! and each [`Load`] counter has its own typed [`Unread`] instead of a fabricated idle/zero reading.
//!
//! Bounds: one entry per native enumerated device; NVML/ADL use their driver's documented count
//! bounds. Apple reserves fallibly from Metal's actual array count and subscribes only to its sole
//! GPUPH GPU-performance channel. Its previous raw sample is the sole residency baseline, replaced
//! on each sample with no timer/minimum interval. IOReport/IOKit allocate native snapshots; Apple's
//! adapter makes no allocation-free claim. Linux/Windows retain their existing per-sample bounds.
//!
//! Apple's memory total is Metal's native recommended working-set budget, not dedicated VRAM or
//! total RAM; its driver-reported mapped allocation can exceed that budget. The package-wide GPUPH
//! counter is attributed only when Metal enumerates a sole device; ambiguous attribution is typed.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

#[cfg(any(target_os = "windows", test))]
mod adl;
#[cfg(target_os = "linux")]
mod amd;
#[cfg(any(target_os = "macos", test))]
mod apple;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod nvml;
#[cfg(test)]
mod tests;
#[cfg(any(target_os = "windows", test))]
pub mod windows_identity;

use std::fmt;

/// Whose GPU it is, and so which driver interface reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Vendor {
    /// Read through NVML.
    Nvidia,
    /// Read through amdgpu's sysfs counters on Linux, ADL on Windows.
    Amd,
    /// Read through Metal, IOKit and IOReport on Apple Silicon.
    Apple,
}

/// A PCI function's address, `domain:bus:device.function`: the GPU's identity on its node, the same
/// whichever driver names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PciAddress {
    pub domain: u32,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciAddress {
    /// Parses an address in hex as the kernel names a PCI device (`0000:01:00.0`) and NVML its bus id
    /// (`00000000:01:00.0`): a domain of one to eight digits, a bus of two, a device of two below
    /// 0x20, a function of one below 8. `None` for anything else.
    pub fn parse(text: &str) -> Option<PciAddress> {
        let (domain, rest) = text.split_once(':')?;
        let (bus, rest) = rest.split_once(':')?;
        let (device, function) = rest.split_once('.')?;
        let hex = |s: &str, min: usize, max: usize| {
            (s.len() >= min && s.len() <= max && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| u32::from_str_radix(s, 16).ok())
                .flatten()
        };
        Some(PciAddress {
            domain: hex(domain, 1, 8)?,
            bus: u8::try_from(hex(bus, 2, 2)?).ok()?,
            device: u8::try_from(hex(device, 2, 2)?)
                .ok()
                .filter(|d| *d < 0x20)?,
            function: u8::try_from(hex(function, 1, 1)?).ok().filter(|f| *f < 8)?,
        })
    }
}

/// As the kernel writes it: `0000:01:00.0`.
impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.device, self.function
        )
    }
}

/// A native GPU identity. Metal's registry ID is global to tasks, not a PCI address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Identity {
    Pci(PciAddress),
    Metal(u64),
}

impl Identity {
    /// PCI's domain32/bus8/device5/function3 packed into 48 bits, or Metal's full native ID.
    /// The identity's variant must be retained when interpreting the key.
    pub fn key(self) -> u64 {
        match self {
            Self::Pci(pci) => {
                (u64::from(pci.domain) << 16)
                    | (u64::from(pci.bus) << 8)
                    | (u64::from(pci.device) << 3)
                    | u64::from(pci.function)
            }
            Self::Metal(id) => id,
        }
    }

    pub fn pci(self) -> Option<PciAddress> {
        match self {
            Self::Pci(pci) => Some(pci),
            Self::Metal(_) => None,
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pci(pci) => pci.fmt(f),
            Self::Metal(id) => write!(f, "metal:{id}"),
        }
    }
}

/// One GPU as [`Gpus::open`] found it: what doesn't change while it is plugged in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub vendor: Vendor,
    /// NVIDIA's name for it (`NVIDIA GeForce RTX 5060 Ti`); for AMD's, libdrm's marketing name where
    /// its table has one, else the PCI ids as lspci prints them (`1002:13c0 rev e5`).
    pub model: String,
    /// NVIDIA's framebuffer; AMD's VRAM (an APU's firmware carve-out); Apple's Metal
    /// `recommendedMaxWorkingSetSize`, a working-set budget, not dedicated VRAM or total host RAM.
    pub memory_total_bytes: u64,
    /// Where it is. Windows AMD joins the actual PNP devnode's full PCI identity to ADL's BDF.
    pub identity: Identity,
    /// NVIDIA's driver's version (`620.70.06`); amdgpu's: a DKMS module's own, else the kernel's
    /// release, the in-tree driver's; on Windows, AMD's driver release (`25.20.32.06`); on Apple,
    /// the native Darwin release of the OS-bundled driver (not a guessed Metal version).
    pub driver_version: String,
}

impl Gpu {
    pub fn key(&self) -> u64 {
        self.identity.key()
    }

    pub fn pci(&self) -> Option<PciAddress> {
        self.identity.pci()
    }
}

/// One GPU's load at one sample. Each counter is read on its own, so one can fail while the other
/// reads (a MIG-enabled NVIDIA GPU has no utilisation, an asleep AMD one no busy percent; both have
/// their memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Load {
    /// The percent of the time the GPU is busy, 0 to 100: for NVIDIA, of NVML's last sample period
    /// (1/6 s to 1 s by product) that a kernel ran; for AMD, the SMU firmware's load over the GPU's IP
    /// cores (`gpu_busy_percent` on Linux) or its graphics engine's activity (ADL's PMLog on Windows);
    /// Apple's active / total GPUPH residency between the previous observation and this one.
    pub busy_percent: Result<u8, Unread>,
    /// The bytes of its memory in use: NVML's `used` (allocated and reserved), amdgpu's
    /// `mem_info_vram_used`, ADL's VRAM usage (whole megabytes of 2^20 bytes), or the exact Apple
    /// registry entry's `PerformanceStatistics` / `Alloc system memory` mapped allocation. This
    /// last value is not whole-host RAM use or this process's Metal allocation.
    pub memory_used_bytes: Result<u64, Unread>,
}

/// Why a counter wasn't read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Unread {
    /// The GPU is gone: unplugged, fallen off the bus, or its driver unbound.
    #[error("the GPU is gone: unplugged, fallen off the bus or unbound from its driver")]
    Gone,
    /// The GPU is runtime-suspended: it runs nothing, and amdgpu answers its busy counter only awake.
    #[error("the GPU is asleep (runtime-suspended): it runs nothing")]
    Asleep,
    /// The driver doesn't offer this counter on this GPU.
    #[error("its driver doesn't offer this on this GPU")]
    Unsupported,
    /// What the driver answered isn't the value it documents (a percent past 100, a line that isn't a
    /// number, a name that isn't UTF-8).
    #[error("its driver answered with something that isn't the value it documents")]
    Malformed,
    /// NVML refused, with its `nvmlReturn_t`.
    #[error("NVML refused: {}", nvml_says(.code))]
    Nvml { code: i32 },
    /// The OS refused the read, with its error number (0: the error carried none).
    #[error("the OS refused the read: error {errno}")]
    Os { errno: i32 },
    /// Configuration Manager refused a current devnode/property query (its CONFIGRET namespace).
    #[error("Windows Configuration Manager refused: CONFIGRET {code}")]
    WindowsCm { code: u32 },
    /// ADL refused, with its code.
    #[error("ADL refused: {}", adl_says(.code))]
    Adl { code: i32 },
    /// No residency advanced in this window; it is not an idle reading.
    #[error("no GPU residency sample has advanced yet")]
    NoSample,
    /// A native allocation or a fallible Rust reservation was refused.
    #[error("there is no memory for the GPU reading")]
    Allocation,
    /// IOReport returned no subscription, sample or delta (it provides no status code).
    #[error("IOReport could not produce the GPU reading")]
    IoReport,
    /// A package-wide counter cannot be attributed to one of several native GPU identities.
    #[error("the package GPU counter cannot identify this GPU separately")]
    Ambiguous,
}

/// A device a driver counts that [`Gpus::open`] couldn't read, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// NVML's device at this index (`nvmlDeviceGetHandleByIndex_v2`'s numbering).
    Nvml { index: u32, why: Unread },
    /// The DRM class's `card<N>` (`/sys/class/drm`).
    Drm { card: u32, why: Unread },
    /// ADL's adapter row at this position (`ADL2_Adapter_AdapterInfo_Get`'s order).
    Adl { adapter: u32, why: Unread },
    /// Metal's device at this position in `MTLCopyAllDevices`.
    Metal { index: usize, why: Unread },
}

/// Why one vendor's GPUs aren't read on this node.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Absent {
    /// This platform has no interface Hex reads for them.
    #[error("this platform has no interface Hex reads for them")]
    Unsupported,
    /// Their library isn't installed: the names tried, and the loader's last error.
    #[error("their library isn't installed (tried {tried:?}: {error})")]
    NoLibrary {
        tried: &'static [&'static str],
        error: String,
    },
    /// Their library lacks an entry point Hex calls.
    #[error("their library has no {name}")]
    NoSymbol { name: &'static str },
    /// NVML refused to start or to list, with its `nvmlReturn_t`.
    #[error("NVML refused: {}", nvml_says(.code))]
    Nvml { code: i32 },
    /// Their devices couldn't be listed: the OS's error number.
    #[error("listing them failed: OS error {errno}")]
    Os { errno: i32 },
    /// The driver answered with something it doesn't document.
    #[error("their driver answered with something it doesn't document")]
    Malformed,
    /// The interface works and lists none this process can read ([`Gpus::skipped`] has any it saw).
    #[error("none is present that this process can read")]
    NoGpu,
    /// ADL refused to start or to list, with its code.
    #[error("ADL refused: {}", adl_says(.code))]
    Adl { code: i32 },
    #[error("Metal could not enumerate GPU devices")]
    Metal,
    #[error("there is no memory for the GPU inventory")]
    Allocation,
}

/// The node has no GPU this process can read: why, for each vendor.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no GPU to read: NVIDIA: {nvidia}; AMD: {amd}; Apple: {apple}")]
pub struct NoGpus {
    pub nvidia: Absent,
    pub amd: Absent,
    pub apple: Absent,
}

/// What nvml.h says each `nvmlReturn_t` means, for a person reading a refusal.
fn nvml_says(code: &i32) -> String {
    let says = match *code {
        1 => "NVML isn't initialised",
        2 => "an argument is invalid",
        3 => "the device doesn't support it",
        4 => "no permission",
        6 => "not found",
        9 => "NVIDIA's driver isn't loaded",
        12 => "NVML's library wasn't found",
        13 => "this NVML lacks the function",
        15 => "the GPU fell off the bus",
        17 => "the operating system or a cgroup blocks the GPU",
        18 => "the driver and NVML's library are different versions",
        _ => "",
    };
    if says.is_empty() {
        format!("nvmlReturn_t {code}")
    } else {
        format!("nvmlReturn_t {code} ({says})")
    }
}

/// What adl_defines.h says each ADL code means, for a person reading a refusal.
fn adl_says(code: &i32) -> String {
    let says = match *code {
        -1 => "a call to the driver failed",
        -2 => "ADL isn't initialised",
        -3 => "an argument is invalid",
        -5 => "no such adapter index",
        -8 => "the driver doesn't support it",
        -10 => "the adapter is disabled",
        -22 => "an incompatible driver is installed",
        -23 => "it needs an administrator",
        -25 => "the adapter is in a power state that answers nothing",
        -26 => "ADL's server is busy",
        _ => "",
    };
    if says.is_empty() {
        format!("ADL code {code}")
    } else {
        format!("ADL code {code} ({says})")
    }
}

/// A node's GPUs, found once, with every counter [`Gpus::sample`] reads kept open.
#[derive(Debug)]
pub struct Gpus {
    gpus: Vec<Gpu>,
    loads: Vec<Load>,
    skipped: Vec<Skipped>,
    /// Why there are none: set exactly when `gpus` is empty.
    none: Option<NoGpus>,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    nvidia: Vec<nvml::Device<&'static nvml::sys::Api>>,
    #[cfg(target_os = "linux")]
    amd: Vec<amd::Card<std::fs::File>>,
    #[cfg(target_os = "windows")]
    adl: Option<adl::Amd<adl::sys::Session>>,
    #[cfg(target_os = "macos")]
    apple: Option<apple::sys::Apple>,
}

impl Gpus {
    /// Finds the node's GPUs: NVML loaded and initialised (once for the process; a failure is tried
    /// again at the next open), amdgpu's cards listed (Linux) or ADL loaded and a context of its own
    /// made (Windows), each GPU's identity read and its counters opened.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub fn open() -> Gpus {
        let mut gpus = Vec::new();
        let mut skipped = Vec::new();

        let (nvidia, nvidia_absent) = {
            let found = nvml::load::nvml()
                .map_err(nvml::absent)
                .and_then(nvml::open);
            match found {
                Ok(found) => {
                    skipped.extend(found.skipped);
                    let (found, devices): (Vec<_>, Vec<_>) = found.gpus.into_iter().unzip();
                    gpus.extend(found);
                    (devices, Absent::NoGpu)
                }
                Err(why) => (Vec::new(), why),
            }
        };

        #[cfg(target_os = "linux")]
        let (amd, amd_absent) = match amd::open(&amd::Paths::system()) {
            Ok(found) => {
                skipped.extend(found.skipped);
                let (found, cards): (Vec<_>, Vec<_>) = found.gpus.into_iter().unzip();
                gpus.extend(found);
                (cards, Absent::NoGpu)
            }
            Err(why) => (Vec::new(), why),
        };
        #[cfg(target_os = "windows")]
        let (adl, amd_absent) = {
            let found = adl::sys::adl()
                .map_err(adl::sys::absent)
                .and_then(|api| adl::sys::Session::open(api).map_err(|code| Absent::Adl { code }))
                .and_then(adl::open);
            match found {
                Ok(found) => {
                    skipped.extend(found.skipped);
                    gpus.extend(found.gpus);
                    (Some(found.amd), Absent::NoGpu)
                }
                Err(why) => (None, why),
            }
        };

        let none = if gpus.is_empty() {
            Some(NoGpus {
                nvidia: nvidia_absent,
                amd: amd_absent,
                apple: Absent::Unsupported,
            })
        } else {
            None
        };
        Gpus {
            loads: Vec::with_capacity(gpus.len()),
            gpus,
            skipped,
            none,
            nvidia,
            #[cfg(target_os = "linux")]
            amd,
            #[cfg(target_os = "windows")]
            adl,
        }
    }

    /// Finds the node's GPUs: none here, where neither vendor's interface exists (NVIDIA's driver,
    /// amdgpu and ADL are Linux's and Windows').
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    pub fn open() -> Gpus {
        Gpus {
            gpus: Vec::new(),
            loads: Vec::new(),
            skipped: Vec::new(),
            none: Some(NoGpus {
                nvidia: Absent::Unsupported,
                amd: Absent::Unsupported,
                apple: Absent::Unsupported,
            }),
        }
    }

    /// Reads native Apple Silicon GPU identities and budgets, then opens the GPU-only IOReport
    /// subscription. Counter failure preserves the inventory and is returned per counter.
    #[cfg(target_os = "macos")]
    pub fn open() -> Gpus {
        match apple::sys::open() {
            Ok(found) => Gpus {
                gpus: found.gpus,
                loads: found.loads,
                skipped: found.skipped,
                none: None,
                apple: Some(found.apple),
            },
            Err(why) => Gpus {
                gpus: Vec::new(),
                loads: Vec::new(),
                skipped: why.skipped,
                none: Some(NoGpus {
                    nvidia: Absent::Unsupported,
                    amd: Absent::Unsupported,
                    apple: why.cause,
                }),
                apple: None,
            },
        }
    }

    /// The GPUs found at open in vendor/native-identity order. Or why there are none.
    pub fn inventory(&self) -> Result<&[Gpu], &NoGpus> {
        match &self.none {
            Some(none) => Err(none),
            None => Ok(&self.gpus),
        }
    }

    /// Each GPU's load now, in [`Gpus::inventory`]'s order: one NVML or ADL call, or one read of a
    /// kept-open counter, per counter, into room reserved at open. Apple creates native IOReport/
    /// IOKit snapshots; a zero residency window is `Unread::NoSample`. Or why there are none.
    pub fn sample(&mut self) -> Result<&[Load], &NoGpus> {
        if let Some(none) = &self.none {
            return Err(none);
        }
        // Within the capacity reserved at open, one load per GPU: no push here reallocates.
        self.loads.clear();
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        for device in &self.nvidia {
            self.loads.push(device.sample());
        }
        #[cfg(target_os = "linux")]
        for card in &self.amd {
            self.loads.push(card.sample());
        }
        #[cfg(target_os = "windows")]
        if let Some(adl) = &mut self.adl {
            adl.sample(&mut self.loads);
        }
        #[cfg(target_os = "macos")]
        if let Some(apple) = &mut self.apple {
            apple.sample(&mut self.loads);
        }
        Ok(&self.loads)
    }

    /// The devices a driver counts that open couldn't read, each with why.
    pub fn skipped(&self) -> &[Skipped] {
        &self.skipped
    }

    /// The Windows AMD counter receipt's actual verified PNP aliases. The last sample must still
    /// match every retained alias; an uncertain native identity never becomes a fresh grant.
    #[cfg(target_os = "windows")]
    pub fn windows_aliases(
        &self,
        identity: Identity,
    ) -> Result<&[windows_identity::WindowsAlias], Unread> {
        self.adl
            .as_ref()
            .ok_or(Unread::Unsupported)?
            .aliases(identity)
    }
}
