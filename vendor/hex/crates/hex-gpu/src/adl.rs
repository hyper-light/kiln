//! AMD's GPUs on Windows through ADL, AMD's Display Library (`docs/design/gpu-telemetry.md` G3).
//! [`open`] lists ADL's adapters, one physical GPU per PCI address (ADL numbers several adapters for
//! one GPU, as AMD's own samples group them), and reads each one's identity once; [`Amd::sample`] then
//! makes two ADL calls a GPU: the PMLog sensor table, whose GFX activity is the busy percent, and its
//! VRAM in use. The calls go through [`Calls`], ADL itself in production (`sys`) and jupiter's recorded
//! answers in the tests.

#[path = "../../../../all-smi/src/device/readers/amd_adl/ffi.rs"]
mod ffi;
#[path = "../../../../all-smi/src/device/readers/amd_adl/sensors.rs"]
mod sensors;
#[cfg(target_os = "windows")]
pub(crate) mod sys;
#[cfg(test)]
mod tests;

use std::ffi::c_int;

use crate::windows_identity::WindowsAlias;
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
    /// A separate native property observation of this exact ADL-reported PNP instance.
    /// The ordinary production open itself verifies segment/BDF; no legacy guessed identity exists.
    fn physical_address(&mut self, instance: &str) -> Result<PciAddress, Unread> {
        #[cfg(target_os = "windows")]
        {
            crate::windows_identity::pci_for_instance(instance)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = instance;
            Err(Unread::Unsupported)
        }
    }
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
    adapters: Vec<Adapter>,
}

#[derive(Debug)]
struct Adapter {
    pci: PciAddress,
    index: c_int,
    aliases: Vec<WindowsAlias>,
    current: Result<(), Unread>,
}

impl<C: Calls> Amd<C> {
    /// Each GPU's load now, pushed in inventory order into `out`, within room reserved at open.
    pub(crate) fn sample(&mut self, out: &mut Vec<Load>) {
        for adapter in &mut self.adapters {
            adapter.current = current(&mut self.calls, adapter);
            out.push(match adapter.current {
                Ok(()) => sample(&mut self.calls, adapter.index),
                Err(why) => Load {
                    busy_percent: Err(why),
                    memory_used_bytes: Err(why),
                },
            });
        }
    }

    #[cfg(any(target_os = "windows", test))]
    pub(crate) fn aliases(&self, identity: Identity) -> Result<&[WindowsAlias], Unread> {
        let pci = identity.pci().ok_or(Unread::Unsupported)?;
        let position = self
            .adapters
            .binary_search_by_key(&pci, |adapter| adapter.pci)
            .map_err(|_| Unread::Gone)?;
        let adapter = self.adapters.get(position).ok_or(Unread::Malformed)?;
        adapter.current?;
        Ok(&adapter.aliases)
    }
}

fn current<C: Calls>(calls: &mut C, adapter: &Adapter) -> Result<(), Unread> {
    for alias in &adapter.aliases {
        if calls.physical_address(alias.instance())? != adapter.pci {
            return Err(Unread::Ambiguous);
        }
    }
    Ok(())
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
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    let mut gpus = Vec::new();
    let mut adapters = Vec::new();
    candidates
        .try_reserve_exact(requested)
        .map_err(|_| Absent::Allocation)?;
    skipped
        .try_reserve_exact(requested)
        .map_err(|_| Absent::Allocation)?;
    gpus.try_reserve_exact(requested)
        .map_err(|_| Absent::Allocation)?;
    adapters
        .try_reserve_exact(requested)
        .map_err(|_| Absent::Allocation)?;
    if requested == 0 {
        return Ok(Found {
            gpus,
            amd: Amd { calls, adapters },
            skipped,
        });
    }
    calls
        .adapters(&mut rows)
        .map_err(|code| Absent::Adl { code })?;
    let mut versions = AdlVersionsInfoX2::default();
    calls
        .versions(&mut versions)
        .map_err(|code| Absent::Adl { code })?;
    let driver_version = version(&versions).ok_or(Absent::Malformed)?;
    for (position, row) in rows.requested_entries().iter().enumerate() {
        let adapter = u32::try_from(position).map_err(|_| Absent::Malformed)?;
        match identify(&mut calls, row, adapter) {
            Ok(Some(candidate)) => candidates.push(candidate),
            Ok(None) => {}
            Err(why) => skipped.push(Skipped::Adl { adapter, why }),
        }
    }
    // At most ADL_MAX_ADAPTERS rows. Resolve before either sort: equal BDF on different
    // segments must never merge, and one PNP instance observed at two addresses poisons both.
    candidates.sort_unstable_by(|a, b| instance_cmp(a.alias.instance(), b.alias.instance()));
    let mut conflicts = Vec::new();
    let conflict_bound = requested.checked_mul(2).ok_or(Absent::Malformed)?;
    conflicts
        .try_reserve_exact(conflict_bound)
        .map_err(|_| Absent::Allocation)?;
    for pair in candidates.windows(2) {
        if let (Some(a), Some(b)) = (pair.first(), pair.last())
            && a.alias.instance().eq_ignore_ascii_case(b.alias.instance())
            && a.pci != b.pci
        {
            conflicts.push(a.pci);
            conflicts.push(b.pci);
        }
    }
    conflicts.sort_unstable();
    conflicts.dedup();
    candidates.sort_unstable_by_key(|c| (c.pci, c.position));
    let mut candidates = candidates.into_iter().peekable();
    while let Some(first) = candidates.next() {
        let pci = first.pci;
        let mut aliases = Vec::new();
        aliases
            .try_reserve_exact(1)
            .map_err(|_| Absent::Allocation)?;
        let mut refusal = if conflicts.binary_search(&pci).is_ok() {
            Some(Unread::Ambiguous)
        } else {
            None
        };
        aliases.push(first.alias);
        while candidates.peek().is_some_and(|next| next.pci == pci) {
            let next = candidates.next().ok_or(Absent::Malformed)?;
            if next.model != first.model {
                refusal = Some(Unread::Ambiguous);
            }
            if !aliases.iter().any(|alias| {
                alias.instance().eq_ignore_ascii_case(next.alias.instance())
                    && alias.udid() == next.alias.udid()
            }) {
                aliases.try_reserve(1).map_err(|_| Absent::Allocation)?;
                aliases.push(next.alias);
            }
        }
        let memory = match refusal {
            Some(why) => Err(why),
            None => total(&mut calls, first.index),
        };
        match memory {
            Ok(memory_total_bytes) => {
                gpus.push(Gpu {
                    vendor: Vendor::Amd,
                    model: first.model,
                    memory_total_bytes,
                    identity: Identity::Pci(pci),
                    driver_version: owned(driver_version).map_err(|_| Absent::Allocation)?,
                });
                adapters.push(Adapter {
                    pci,
                    index: first.index,
                    aliases,
                    current: Ok(()),
                });
            }
            Err(why) => skipped.push(Skipped::Adl {
                adapter: first.position,
                why,
            }),
        }
    }
    Ok(Found {
        gpus,
        amd: Amd { calls, adapters },
        skipped,
    })
}

struct Candidate {
    pci: PciAddress,
    position: u32,
    index: c_int,
    model: String,
    alias: WindowsAlias,
}

fn instance_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|b| b.to_ascii_uppercase())
        .cmp(b.bytes().map(|b| b.to_ascii_uppercase()))
}

fn owned(text: &str) -> Result<String, Unread> {
    let mut out = String::new();
    out.try_reserve_exact(text.len())
        .map_err(|_| Unread::Allocation)?;
    out.push_str(text);
    Ok(out)
}

/// ADL supplies a PNP instance and BDF. Only the same actual present devnode supplies the domain.
fn identify<C: Calls>(
    calls: &mut C,
    row: &AdapterInfo,
    position: u32,
) -> Result<Option<Candidate>, Unread> {
    if !row.looks_sane() {
        return Err(Unread::Malformed);
    }
    if row.i_vendor_id != AMD {
        return Ok(None);
    }
    let instance = adl_string(&row.str_pnp_string)
        .filter(|s| !s.is_empty())
        .ok_or(Unread::Malformed)?;
    if !instance
        .get(.."PCI\\VEN_1002&".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("PCI\\VEN_1002&"))
    {
        return Err(Unread::Malformed);
    }
    let pci = calls.physical_address(instance)?;
    if u8::try_from(row.i_bus_number).ok() != Some(pci.bus)
        || u8::try_from(row.i_device_number).ok().filter(|d| *d < 32) != Some(pci.device)
        || u8::try_from(row.i_function_number).ok().filter(|f| *f < 8) != Some(pci.function)
    {
        return Err(Unread::Ambiguous);
    }
    let model = adl_string(&row.str_adapter_name)
        .filter(|name| !name.is_empty())
        .ok_or(Unread::Malformed)?;
    let udid = adl_string(&row.str_udid)
        .filter(|text| !text.is_empty())
        .ok_or(Unread::Malformed)?;
    Ok(Some(Candidate {
        pci,
        position,
        index: row.i_adapter_index,
        model: owned(model)?,
        alias: WindowsAlias {
            instance: owned(instance)?,
            udid: owned(udid)?,
        },
    }))
}

/// The adapter's memory in bytes (`ADLMemoryInfo2::iMemorySize`), which a negative size isn't.
fn total<C: Calls>(calls: &mut C, adapter: c_int) -> Result<u64, Unread> {
    let mut memory = AdlMemoryInfo2::default();
    calls.memory(adapter, &mut memory).map_err(unread)?;
    u64::try_from(memory.i_memory_size).map_err(|_| Unread::Malformed)
}

/// The driver's release, as AMD's software names it (`strCrimsonVersion`, `25.20.32.06`), else its
/// packaging version (`strDriverVer`).
fn version(versions: &AdlVersionsInfoX2) -> Option<&str> {
    [&versions.str_crimson_version, &versions.str_driver_ver]
        .into_iter()
        .filter_map(|field| adl_string(field))
        .find(|text| !text.is_empty())
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
