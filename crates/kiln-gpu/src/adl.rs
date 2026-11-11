//! AMD's GPUs on Windows through ADL, AMD's Display Library (`docs/design/gpu-telemetry.md` G3).
//! [`open`] lists ADL's adapters, one physical GPU per PCI address (ADL numbers several adapters for
//! one GPU, as AMD's own samples group them), and reads each one's identity once; [`Amd::sample`] then
//! makes two ADL calls a GPU: the PMLog sensor table, whose GFX activity is the busy percent, and its
//! VRAM in use. The calls go through [`Calls`], ADL itself in production (`sys`) and jupiter's recorded
//! answers in the tests.

#[path = "../../../vendor/all-smi/src/device/readers/amd_adl/ffi.rs"]
mod ffi;
#[path = "../../../vendor/all-smi/src/device/readers/amd_adl/sensors.rs"]
mod sensors;
#[cfg(target_os = "windows")]
pub(crate) mod sys;
#[cfg(test)]
mod tests;

use std::ffi::c_int;

use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};
use ffi::{
    ADL_MAX_ADAPTERS, AdapterInfo, AdapterInfoArray, AdlMemoryInfo2, AdlPmLogDataBuffer,
    AdlVersionsInfoX2, adl_string,
};

/// AMD's PCI vendor id as ADL writes it, in decimal digits (AMD's samples compare `iVendorID` with
/// `1002`; jupiter's rows carry it so).
const AMD: c_int = 1002;

/// ADL's codes Hex tells apart (`adl_defines.h`): an index ADL no longer numbers, and an adapter it
/// can't call: the GPU is gone or its driver disabled; a function the driver doesn't offer; a GPU in a
/// power state that answers nothing.
const INVALID_ADL_IDX: c_int = -5;
const NOT_SUPPORTED: c_int = -8;
const DISABLED_ADAPTER: c_int = -10;
const INVALID_POWER_STATE: c_int = -25;

/// The ADL calls hex-gpu makes, each returning ADL's code when it isn't a success. Every call takes
/// `&mut self`: one ADL context serves one caller at a time.
pub(crate) trait Calls {
    fn count(&mut self) -> Result<c_int, c_int>;
    fn adapters(&mut self, rows: &mut AdapterInfoArray) -> Result<(), c_int>;
    fn memory(&mut self, adapter: c_int, out: &mut AdlMemoryInfo2) -> Result<(), c_int>;
    fn versions(&mut self, out: &mut AdlVersionsInfoX2) -> Result<(), c_int>;
    fn pmlog(&mut self, adapter: c_int, out: &mut AdlPmLogDataBuffer) -> Result<(), c_int>;
    /// The adapter's VRAM in use, in megabytes.
    fn vram_used_mb(&mut self, adapter: c_int) -> Result<c_int, c_int>;
}

/// The GPUs [`open`] found, each by the ADL adapter index that reads it, beside the context.
#[derive(Debug)]
pub(crate) struct Amd<C> {
    calls: C,
    adapters: Vec<c_int>,
}

impl<C: Calls> Amd<C> {
    /// Each GPU's load now, pushed in inventory order into `out`, within room reserved at open.
    pub(crate) fn sample(&mut self, out: &mut Vec<Load>) {
        for adapter in &self.adapters {
            out.push(sample(&mut self.calls, *adapter));
        }
    }
}

/// One GPU's load: its PMLog table, then its VRAM in use, one ADL call each; a GPU the first finds gone
/// isn't asked the second. The table is read into this frame (8 KiB with its headroom), not the heap.
fn sample<C: Calls>(calls: &mut C, adapter: c_int) -> Load {
    let mut table = AdlPmLogDataBuffer::default();
    let busy_percent = calls
        .pmlog(adapter, &mut table)
        .map_err(unread)
        .and_then(|()| table.validated().ok_or(Unread::Malformed))
        .and_then(sensors::gfx_activity);
    let memory_used_bytes = match busy_percent {
        Err(Unread::Gone) => Err(Unread::Gone),
        _ => calls
            .vram_used_mb(adapter)
            .map_err(unread)
            .and_then(megabytes),
    };
    Load {
        busy_percent,
        memory_used_bytes,
    }
}

/// What [`open`] found: the GPUs it reads, in PCI order, the adapter rows it couldn't read, and the
/// context they are read through.
pub(crate) struct Found<C> {
    pub(crate) gpus: Vec<Gpu>,
    pub(crate) amd: Amd<C>,
    pub(crate) skipped: Vec<Skipped>,
}

/// Lists ADL's adapters and reads what doesn't change: model, total memory, PCI address, and the
/// driver's version. ADL numbers at most `ADL_MAX_ADAPTERS` adapters (250, `adl_defines.h`); a count
/// past it is refused as malformed.
pub(crate) fn open<C: Calls>(mut calls: C) -> Result<Found<C>, Absent> {
    let count = calls.count().map_err(|code| Absent::Adl { code })?;
    if !(0..=ADL_MAX_ADAPTERS).contains(&count) {
        return Err(Absent::Malformed);
    }
    let requested = usize::try_from(count).map_err(|_| Absent::Malformed)?;
    let mut rows = AdapterInfoArray::for_count(requested).ok_or(Absent::Malformed)?;
    let mut gpus: Vec<(Gpu, c_int)> = Vec::new();
    let mut skipped = Vec::new();
    if requested > 0 {
        calls
            .adapters(&mut rows)
            .map_err(|code| Absent::Adl { code })?;
        let mut versions = AdlVersionsInfoX2::default();
        calls
            .versions(&mut versions)
            .map_err(|code| Absent::Adl { code })?;
        let driver_version = version(&versions).ok_or(Absent::Malformed)?;
        // The first row of each PCI address names its GPU; the rest are the same GPU's other adapters.
        let mut seen: Vec<PciAddress> = Vec::new();
        for (position, row) in rows.requested_entries().iter().enumerate() {
            let adapter = u32::try_from(position).map_err(|_| Absent::Malformed)?;
            let found = match identify(row) {
                // Another vendor's adapter, which ADL lists beside AMD's on a hybrid machine.
                Ok(None) => continue,
                Ok(Some(found)) => found,
                Err(why) => {
                    skipped.push(Skipped::Adl { adapter, why });
                    continue;
                }
            };
            let (pci, model) = found;
            if seen.contains(&pci) {
                continue;
            }
            seen.push(pci);
            let index = row.i_adapter_index;
            match total(&mut calls, index) {
                Ok(memory_total_bytes) => gpus.push((
                    Gpu {
                        vendor: Vendor::Amd,
                        model,
                        memory_total_bytes,
                        identity: Identity::Pci(pci),
                        driver_version: driver_version.clone(),
                    },
                    index,
                )),
                Err(why) => skipped.push(Skipped::Adl { adapter, why }),
            }
        }
    }
    gpus.sort_by_key(|(gpu, _)| gpu.identity);
    let (gpus, adapters) = gpus.into_iter().unzip();
    Ok(Found {
        gpus,
        amd: Amd { calls, adapters },
        skipped,
    })
}

/// One adapter row: its PCI address and model, `None` for another vendor's, or why it can't be read.
/// ADL names no PCI segment; Windows' AMD GPUs are read on segment 0.
fn identify(row: &AdapterInfo) -> Result<Option<(PciAddress, String)>, Unread> {
    if !row.looks_sane() {
        return Err(Unread::Malformed);
    }
    if row.i_vendor_id != AMD {
        return Ok(None);
    }
    let pci = PciAddress {
        domain: 0,
        bus: u8::try_from(row.i_bus_number).map_err(|_| Unread::Malformed)?,
        device: u8::try_from(row.i_device_number)
            .ok()
            .filter(|d| *d < 0x20)
            .ok_or(Unread::Malformed)?,
        function: u8::try_from(row.i_function_number)
            .ok()
            .filter(|f| *f < 8)
            .ok_or(Unread::Malformed)?,
    };
    let model = adl_string(&row.str_adapter_name)
        .filter(|name| !name.is_empty())
        .ok_or(Unread::Malformed)?;
    Ok(Some((pci, model.to_owned())))
}

/// The adapter's memory in bytes (`ADLMemoryInfo2::iMemorySize`), which a negative size isn't.
fn total<C: Calls>(calls: &mut C, adapter: c_int) -> Result<u64, Unread> {
    let mut memory = AdlMemoryInfo2::default();
    calls.memory(adapter, &mut memory).map_err(unread)?;
    u64::try_from(memory.i_memory_size).map_err(|_| Unread::Malformed)
}

/// The driver's release, as AMD's software names it (`strCrimsonVersion`, `25.20.32.06`), else its
/// packaging version (`strDriverVer`).
fn version(versions: &AdlVersionsInfoX2) -> Option<String> {
    [&versions.str_crimson_version, &versions.str_driver_ver]
        .into_iter()
        .filter_map(|field| adl_string(field))
        .find(|text| !text.is_empty())
        .map(str::to_owned)
}

/// ADL's megabyte: 2^20 bytes. On jupiter, Windows' own `\GPU Adapter Memory(*)\Dedicated Usage`
/// read 61386752 bytes as ADL's `DedicatedVRAMUsage`, which reads that counter, said 58 MB: 58.5 of
/// 2^20, floored (a decimal megabyte would make it 61).
const MEGABYTE: u64 = 1_048_576;

/// ADL's VRAM in use, in whole megabytes, in bytes.
fn megabytes(value: c_int) -> Result<u64, Unread> {
    let megabytes = u64::try_from(value).map_err(|_| Unread::Malformed)?;
    megabytes.checked_mul(MEGABYTE).ok_or(Unread::Malformed)
}

fn unread(code: c_int) -> Unread {
    match code {
        INVALID_ADL_IDX | DISABLED_ADAPTER => Unread::Gone,
        NOT_SUPPORTED => Unread::Unsupported,
        INVALID_POWER_STATE => Unread::Asleep,
        code => Unread::Adl { code },
    }
}
