//! NVIDIA's GPUs through NVML (`docs/design/gpu-telemetry.md` G1). [`open`] reads each GPU's identity
//! once and keeps its handle; [`Device::sample`] then makes two NVML calls per GPU, utilisation and
//! memory, the fewest NVML's interface allows: no field value (`nvmlDeviceGetFieldValues`) carries
//! either, and no call reads two devices. The calls go through [`Calls`], NVML itself in production
//! (`sys`) and recorded return values in the tests.

#[path = "../../../vendor/cuda-rust/cuda-bindings/src/dyn_load.rs"]
pub(crate) mod load;
pub(crate) mod sys;
#[cfg(test)]
mod tests;

use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};
use load::DynLoadError;

/// nvml.h's buffers: `NVML_SYSTEM_DRIVER_VERSION_BUFFER_SIZE`, `NVML_DEVICE_NAME_V2_BUFFER_SIZE` and
/// `NVML_DEVICE_PCI_BUS_ID_BUFFER_SIZE`, each its NUL included.
pub(crate) const DRIVER_VERSION_BYTES: usize = 80;
pub(crate) const NAME_BYTES: usize = 96;
pub(crate) const BUS_ID_BYTES: usize = 32;

/// `nvmlReturn_t`s Hex tells apart (nvml.h): the device doesn't offer the read (a MIG-enabled GPU's
/// utilisation), and the GPU fell off the bus or was otherwise lost.
const NOT_SUPPORTED: i32 = 3;
const GPU_IS_LOST: i32 = 15;

/// The NVIDIA driver's own bound on a node's GPUs: Linux numbers them on character-device minors 0 to
/// 247 (`NV_MINOR_DEVICE_NUMBER_REGULAR_MAX`, open-gpu-kernel-modules
/// `kernel-open/common/inc/nv-chardev-numbers.h`). A count past it is refused as malformed.
const MAX_GPUS: u32 = 248;

/// A device's memory as `nvmlDeviceGetMemoryInfo` reports it, in bytes: `used` is reserved and
/// allocated together, what the next allocation can't have.
pub(crate) struct Memory {
    pub(crate) total: u64,
    pub(crate) used: u64,
}

/// The NVML calls hex-gpu makes, each returning NVML's `nvmlReturn_t` when it isn't success.
pub(crate) trait Calls: Copy {
    type Device: Copy;
    fn driver_version(self, out: &mut [u8; DRIVER_VERSION_BYTES]) -> Result<(), i32>;
    fn count(self) -> Result<u32, i32>;
    fn device(self, index: u32) -> Result<Self::Device, i32>;
    fn name(self, device: Self::Device, out: &mut [u8; NAME_BYTES]) -> Result<(), i32>;
    fn bus_id(self, device: Self::Device, out: &mut [u8; BUS_ID_BYTES]) -> Result<(), i32>;
    fn memory(self, device: Self::Device) -> Result<Memory, i32>;
    /// The percent of the last sample period (1/6 s to 1 s by product, nvml.h) a kernel ran.
    fn utilization(self, device: Self::Device) -> Result<u32, i32>;
}

/// One GPU's handle, kept from [`open`].
#[derive(Debug)]
pub(crate) struct Device<C: Calls> {
    calls: C,
    device: C::Device,
}

impl<C: Calls> Device<C> {
    /// This GPU's load now: its utilisation, then its memory, one NVML call each; a GPU the first
    /// finds gone isn't asked the second.
    pub(crate) fn sample(&self) -> Load {
        let busy_percent = self
            .calls
            .utilization(self.device)
            .map_err(unread)
            .and_then(percent);
        let memory_used_bytes = match busy_percent {
            Err(Unread::Gone) => Err(Unread::Gone),
            _ => self
                .calls
                .memory(self.device)
                .map(|m| m.used)
                .map_err(unread),
        };
        Load {
            busy_percent,
            memory_used_bytes,
        }
    }
}

/// What [`open`] found: the GPUs it reads, in PCI order, each beside its kept handle, and the ones NVML
/// counts that it can't.
pub(crate) struct Found<C: Calls> {
    pub(crate) gpus: Vec<(Gpu, Device<C>)>,
    pub(crate) skipped: Vec<Skipped>,
}

/// Lists NVML's GPUs and reads what doesn't change: model, total memory, PCI address and driver
/// version. A GPU NVML counts but won't give (`nvmlDeviceGetCount_v2` counts devices this process has
/// no permission to, nvml.h) is skipped with why.
pub(crate) fn open<C: Calls>(calls: C) -> Result<Found<C>, Absent> {
    let mut version = [0; DRIVER_VERSION_BYTES];
    calls
        .driver_version(&mut version)
        .map_err(|code| Absent::Nvml { code })?;
    let driver_version = text(&version).ok_or(Absent::Malformed)?;
    let count = calls.count().map_err(|code| Absent::Nvml { code })?;
    if count > MAX_GPUS {
        return Err(Absent::Malformed);
    }
    let mut gpus = Vec::new();
    let mut skipped = Vec::new();
    for index in 0..count {
        match identify(calls, index, driver_version) {
            Ok(found) => gpus.push(found),
            Err(why) => skipped.push(Skipped::Nvml { index, why }),
        }
    }
    gpus.sort_by_key(|(gpu, _)| gpu.identity);
    Ok(Found { gpus, skipped })
}

fn identify<C: Calls>(
    calls: C,
    index: u32,
    driver_version: &str,
) -> Result<(Gpu, Device<C>), Unread> {
    let device = calls.device(index).map_err(unread)?;
    let mut name = [0; NAME_BYTES];
    calls.name(device, &mut name).map_err(unread)?;
    let mut bus_id = [0; BUS_ID_BYTES];
    calls.bus_id(device, &mut bus_id).map_err(unread)?;
    let memory = calls.memory(device).map_err(unread)?;
    let gpu = Gpu {
        vendor: Vendor::Nvidia,
        model: text(&name).ok_or(Unread::Malformed)?.to_owned(),
        memory_total_bytes: memory.total,
        identity: Identity::Pci(
            text(&bus_id)
                .and_then(PciAddress::parse)
                .ok_or(Unread::Malformed)?,
        ),
        driver_version: driver_version.to_owned(),
    };
    Ok((gpu, Device { calls, device }))
}

/// A C string NVML wrote: the bytes before its NUL, which must be there, as UTF-8 and not empty.
fn text(buffer: &[u8]) -> Option<&str> {
    let (bytes, _) = buffer.split_at_checked(buffer.iter().position(|b| *b == 0)?)?;
    std::str::from_utf8(bytes).ok().filter(|s| !s.is_empty())
}

fn unread(code: i32) -> Unread {
    match code {
        GPU_IS_LOST => Unread::Gone,
        NOT_SUPPORTED => Unread::Unsupported,
        code => Unread::Nvml { code },
    }
}

fn percent(value: u32) -> Result<u8, Unread> {
    u8::try_from(value)
        .ok()
        .filter(|p| *p <= 100)
        .ok_or(Unread::Malformed)
}

/// Why NVML gave no GPU, from why it didn't load.
pub(crate) fn absent(error: DynLoadError) -> Absent {
    match error {
        DynLoadError::LoadFailed { names, source } => Absent::NoLibrary {
            tried: names,
            error: source,
        },
        DynLoadError::MissingSymbol { name } => Absent::NoSymbol { name },
        DynLoadError::InitFailed { code } => Absent::Nvml { code },
    }
}
