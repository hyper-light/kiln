//! ADL's answers as fixtures: what jupiter's ADL answered (Radeon 8060S, driver 25.20.32.06,
//! 2026-11-09: five adapter rows for one GPU at bus 196, each `iPresent` 0 and inactive, ADL's vendor id
//! `1002` in decimal; Overdrive refused with -8 yet a 2052-byte PMLog table with GFX activity 0;
//! `MemoryInfo2` 4294967296 bytes of LPDDR5; VRAM in use 73 MB; an adapter index past the five refused
//! with -5) and the codes `adl_defines.h` documents: a disabled adapter, a GPU in a power state that
//! answers nothing, a card that doesn't publish GFX activity (`AC-T7.3.1`).

use std::ffi::c_int;

use super::ffi::{
    ADL_MAX_PATH, AdapterInfo, AdapterInfoArray, AdlMemoryInfo2, AdlPmLogDataBuffer,
    AdlVersionsInfoX2,
};
use super::{Calls, Found, open as native_open};
use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};

const INVALID_ADL_IDX: c_int = -5;
const DISABLED_ADAPTER: c_int = -10;
const INVALID_POWER_STATE: c_int = -25;
const ERR: c_int = -1;

/// jupiter's PMLog table: (sensor, value) for each sensor its 8060S publishes.
const PMLOG: [(usize, c_int); 16] = [
    (1, 600),
    (2, 1000),
    (3, 600),
    (7, 6),
    (19, 0),
    (23, 35),
    (28, 51),
    (29, 53),
    (30, 0),
    (32, 0),
    (33, 1),
    (34, 3245),
    (38, 0),
    (39, 0),
    (71, 800),
    (72, 24),
];

fn text(value: &str) -> [u8; ADL_MAX_PATH] {
    let mut field = [0; ADL_MAX_PATH];
    field[..value.len()].copy_from_slice(value.as_bytes());
    field
}

/// One of jupiter's adapter rows: index `n`, its PNP and UDID suffixes as ADL wrote them.
fn row(index: c_int, bus: c_int) -> AdapterInfo {
    let suffix = if index == 0 {
        String::new()
    } else {
        format!("&{:02}", index + 1)
    };
    // Other buses below are synthetic distinct PNP instances, not renamed Jupiter aliases.
    let instance_tail = if bus == 196 {
        "0041".to_owned()
    } else {
        format!("0041B{bus}")
    };
    AdapterInfo {
        i_adapter_index: index,
        str_udid: text(&format!(
            "PCI_VEN_1002&DEV_1586&SUBSYS_1FB31043&REV_C1_4&35FE04F8&0&{instance_tail}{suffix}A"
        )),
        i_bus_number: bus,
        i_device_number: 0,
        i_function_number: 0,
        i_vendor_id: 1002,
        str_adapter_name: text("AMD Radeon(TM) 8060S Graphics"),
        str_display_name: text(""),
        _i_exist: 1,
        str_pnp_string: text(&format!(
            "PCI\\VEN_1002&DEV_1586&SUBSYS_1FB31043&REV_C1\\4&35FE04F8&0&{instance_tail}{suffix}"
        )),
        ..AdapterInfo::default()
    }
}

/// ADL, answering from a recording, counting every call made to it.
struct Adl {
    count: Result<c_int, c_int>,
    rows: Vec<AdapterInfo>,
    memory: Result<i64, c_int>,
    versions: Result<AdlVersionsInfoX2, c_int>,
    table: Result<Vec<(usize, c_int)>, c_int>,
    table_size: c_int,
    vram_mb: Result<c_int, c_int>,
    calls: u32,
    native: Vec<(String, Result<PciAddress, Unread>)>,
}

/// jupiter's ADL as recorded.
fn jupiter() -> Adl {
    let versions = AdlVersionsInfoX2 {
        str_driver_ver: text("25.20.32.06-251214a-197030C-Asus"),
        _str_catalyst_version: text("15.8"),
        str_crimson_version: text("25.20.32.06"),
        ..AdlVersionsInfoX2::default()
    };
    Adl {
        count: Ok(5),
        rows: (0..5).map(|index| row(index, 196)).collect(),
        memory: Ok(4_294_967_296),
        versions: Ok(versions),
        table: Ok(PMLOG.to_vec()),
        table_size: 2052,
        vram_mb: Ok(73),
        calls: 0,
        native: Vec::new(),
    }
}

impl Adl {
    /// An adapter this recording numbers, or ADL's answer to one it doesn't.
    fn adapter(&self, adapter: c_int) -> Result<(), c_int> {
        let numbered = usize::try_from(adapter).is_ok_and(|a| a < self.rows.len());
        if numbered {
            Ok(())
        } else {
            Err(INVALID_ADL_IDX)
        }
    }
}

// The recording supplies native properties separately from ADL. Production never uses this
// fixture-only single-segment value; it always calls Configuration Manager.
fn open(mut adl: Adl) -> Result<Found<Adl>, Absent> {
    if adl.native.is_empty() {
        adl.native = adl
            .rows
            .iter()
            .map(|row| {
                let instance = super::ffi::adl_string(&row.str_pnp_string)
                    .unwrap_or("")
                    .to_owned();
                let pci = (|| {
                    Some(PciAddress {
                        domain: 0,
                        bus: u8::try_from(row.i_bus_number).ok()?,
                        device: u8::try_from(row.i_device_number).ok()?,
                        function: u8::try_from(row.i_function_number).ok()?,
                    })
                })()
                .ok_or(Unread::Malformed);
                (instance, pci)
            })
            .collect();
    }
    native_open(adl)
}

impl Calls for Adl {
    fn physical_address(&mut self, instance: &str) -> Result<PciAddress, Unread> {
        self.native
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(instance))
            .map(|(_, result)| *result)
            .unwrap_or(Err(Unread::Gone))
    }
    fn count(&mut self) -> Result<c_int, c_int> {
        self.calls += 1;
        self.count
    }

    fn adapters(&mut self, rows: &mut AdapterInfoArray) -> Result<(), c_int> {
        self.calls += 1;
        for (slot, row) in rows.requested_entries_mut().iter_mut().zip(&self.rows) {
            *slot = *row;
        }
        Ok(())
    }

    fn memory(&mut self, adapter: c_int, out: &mut AdlMemoryInfo2) -> Result<(), c_int> {
        self.calls += 1;
        self.adapter(adapter)?;
        out.i_memory_size = self.memory?;
        out._str_memory_type = text("LPDDR5");
        Ok(())
    }

    fn versions(&mut self, out: &mut AdlVersionsInfoX2) -> Result<(), c_int> {
        self.calls += 1;
        *out = self.versions?;
        Ok(())
    }

    fn pmlog(&mut self, adapter: c_int, out: &mut AdlPmLogDataBuffer) -> Result<(), c_int> {
        self.calls += 1;
        self.adapter(adapter)?;
        out.output.ul_size = self.table_size;
        for (sensor, value) in self.table.clone()? {
            out.output.sensors[sensor].supported = 1;
            out.output.sensors[sensor].value = value;
        }
        Ok(())
    }

    fn vram_used_mb(&mut self, adapter: c_int) -> Result<c_int, c_int> {
        self.calls += 1;
        self.adapter(adapter)?;
        self.vram_mb
    }
}

fn sample(found: &mut Found<Adl>) -> Vec<Load> {
    let mut loads = Vec::new();
    found.amd.sample(&mut loads);
    loads
}

fn the_8060s() -> Gpu {
    Gpu {
        vendor: Vendor::Amd,
        model: "AMD Radeon(TM) 8060S Graphics".to_owned(),
        memory_total_bytes: 4_294_967_296,
        identity: Identity::Pci(PciAddress::parse("0000:c4:00.0").unwrap()),
        driver_version: "25.20.32.06".to_owned(),
    }
}

/// AC-T7.3.1: do open jupiter's recorded ADL; expect its five adapter rows to be one GPU, the 8060S at
/// bus 196 (none present or active, so neither is a filter), with its memory and the driver's release,
/// read by the first row's adapter.
#[test]
fn jupiters_five_adapters_are_one_gpu() {
    let found = open(jupiter()).unwrap();
    assert_eq!(found.gpus, vec![the_8060s()]);
    assert_eq!(
        found
            .amd
            .adapters
            .iter()
            .map(|a| a.index)
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert!(found.skipped.is_empty());
}

/// AC-T7.3.1: do sample jupiter's GPU; expect GFX activity from the PMLog table and the VRAM in use in
/// bytes (73 MB of 2^20), for exactly two ADL calls, and nothing read at open asked again.
#[test]
fn a_sample_is_two_adl_calls_a_gpu() {
    let mut found = open(jupiter()).unwrap();
    let opened = found.amd.calls.calls;
    assert_eq!(
        sample(&mut found),
        vec![Load {
            busy_percent: Ok(0),
            memory_used_bytes: Ok(73 * 1_048_576)
        }]
    );
    assert_eq!(found.amd.calls.calls - opened, 2);
}

/// AC-T7.3.1, hot-unplug: do sample, then take the GPU away so ADL no longer numbers its adapter (as it
/// answered jupiter's index past its five), then disable it; expect its counters `Gone` for one ADL
/// call, not two.
#[test]
fn a_gpu_gone_or_disabled_reads_gone() {
    let mut found = open(jupiter()).unwrap();
    assert!(sample(&mut found)[0].busy_percent.is_ok());
    found.amd.calls.rows.clear();
    let before = found.amd.calls.calls;
    let gone = Load {
        busy_percent: Err(Unread::Gone),
        memory_used_bytes: Err(Unread::Gone),
    };
    assert_eq!(sample(&mut found), vec![gone]);
    assert_eq!(found.amd.calls.calls - before, 1);
    found.amd.calls.rows = jupiter().rows;
    found.amd.calls.table = Err(DISABLED_ADAPTER);
    assert_eq!(sample(&mut found), vec![gone]);
}

/// AC-T7.3.1: do sample a GPU whose table ADL refuses for its power state (`ADL_ERR_INVALID_POWER_STATE`);
/// expect it `Asleep` and its VRAM still read.
#[test]
fn an_asleep_gpu_reads_asleep_and_its_memory() {
    let mut found = open(jupiter()).unwrap();
    found.amd.calls.table = Err(INVALID_POWER_STATE);
    assert_eq!(
        sample(&mut found),
        vec![Load {
            busy_percent: Err(Unread::Asleep),
            memory_used_bytes: Ok(73 * 1_048_576)
        }]
    );
}

/// AC-T7.3.1, CLAUDE.md §3: do sample a card that doesn't publish GFX activity, one whose activity
/// reads 101, one whose table reports another layout's size, and VRAM that reads negative; expect each
/// refused by type, never clamped or guessed.
#[test]
fn what_adl_doesnt_document_is_refused_by_type() {
    let mut found = open(jupiter()).unwrap();
    found.amd.calls.table = Ok(PMLOG.iter().copied().filter(|(s, _)| *s != 19).collect());
    assert_eq!(sample(&mut found)[0].busy_percent, Err(Unread::Unsupported));
    found.amd.calls.table = Ok(vec![(19, 101)]);
    assert_eq!(sample(&mut found)[0].busy_percent, Err(Unread::Malformed));
    found.amd.calls.table = Ok(PMLOG.to_vec());
    found.amd.calls.table_size = 2052 + 8;
    assert_eq!(sample(&mut found)[0].busy_percent, Err(Unread::Malformed));
    found.amd.calls.table_size = 0;
    assert_eq!(sample(&mut found)[0].busy_percent, Ok(0));
    found.amd.calls.vram_mb = Err(ERR);
    assert_eq!(
        sample(&mut found)[0].memory_used_bytes,
        Err(Unread::Adl { code: ERR })
    );
    found.amd.calls.vram_mb = Ok(-1);
    assert_eq!(
        sample(&mut found)[0].memory_used_bytes,
        Err(Unread::Malformed)
    );
    found.amd.calls.vram_mb = Ok(c_int::MAX);
    assert_eq!(
        sample(&mut found)[0].memory_used_bytes,
        Ok(u64::try_from(c_int::MAX).unwrap() * 1_048_576)
    );
}

/// AC-T7.3.1, CLAUDE.md §3: do open rows of two GPUs (the second's bus first), an Intel adapter as ADL
/// lists one on a hybrid machine (vendor 32902, 0x8086 in decimal) and a row whose strings aren't
/// strings (a layout that doesn't match); expect both GPUs in PCI order, Intel's passed over, and the
/// garbled row skipped as malformed by its position.
#[test]
fn rows_are_grouped_other_vendors_passed_over_and_garbage_skipped() {
    let mut adl = jupiter();
    let mut intel = row(5, 0);
    intel.i_vendor_id = 32902;
    let mut garbled = row(6, 3);
    garbled.str_adapter_name = [0xAA; ADL_MAX_PATH];
    adl.rows = vec![row(0, 197), row(1, 196), row(2, 197), intel, garbled];
    adl.count = Ok(5);
    let found = open(adl).unwrap();
    let order: Vec<String> = found
        .gpus
        .iter()
        .map(|g| g.pci().unwrap().to_string())
        .collect();
    assert_eq!(order, ["0000:c4:00.0", "0000:c5:00.0"]);
    assert_eq!(
        found
            .amd
            .adapters
            .iter()
            .map(|a| a.index)
            .collect::<Vec<_>>(),
        vec![1, 0]
    );
    assert_eq!(
        found.skipped,
        vec![Skipped::Adl {
            adapter: 4,
            why: Unread::Malformed
        }]
    );
}

/// AC-T7.3.1, CLAUDE.md §2: do open ADL counting one adapter past its own `ADL_MAX_ADAPTERS`, and
/// ADL counting none; expect the first refused as malformed before any row is asked for, and the
/// second no GPU, for one call.
#[test]
fn a_count_past_adls_bound_is_refused_and_none_is_none() {
    let mut past = jupiter();
    past.count = Ok(251);
    assert_eq!(open(past).err(), Some(Absent::Malformed));
    let mut none = jupiter();
    none.count = Ok(0);
    let found = open(none).unwrap();
    assert!(found.gpus.is_empty() && found.skipped.is_empty());
    assert_eq!(found.amd.calls.calls, 1);
}

/// AC-T7.3.1: do open ADL whose count, versions, or a GPU's memory fails; expect the first two refused
/// with ADL's code and the third GPU skipped with it, and a negative memory size skipped as malformed.
#[test]
fn adls_refusals_at_open_are_kept() {
    let mut count = jupiter();
    count.count = Err(ERR);
    assert_eq!(open(count).err(), Some(Absent::Adl { code: ERR }));
    let mut versions = jupiter();
    versions.versions = Err(ERR);
    assert_eq!(open(versions).err(), Some(Absent::Adl { code: ERR }));
    let mut memory = jupiter();
    memory.memory = Err(ERR);
    let found = open(memory).unwrap();
    assert!(found.gpus.is_empty());
    assert_eq!(
        found.skipped,
        vec![Skipped::Adl {
            adapter: 0,
            why: Unread::Adl { code: ERR }
        }]
    );
    let mut negative = jupiter();
    negative.memory = Ok(-1);
    assert_eq!(
        open(negative).unwrap().skipped,
        vec![Skipped::Adl {
            adapter: 0,
            why: Unread::Malformed
        }]
    );
}

/// AC-T7.3.1: do read a driver whose release field is empty; expect its packaging version, and with
/// neither, the open refused as malformed.
#[test]
fn the_drivers_release_else_its_packaging_version() {
    let mut adl = jupiter();
    let mut versions = adl.versions.unwrap();
    versions.str_crimson_version = text("");
    adl.versions = Ok(versions);
    assert_eq!(
        open(adl).unwrap().gpus[0].driver_version,
        "25.20.32.06-251214a-197030C-Asus"
    );
    let mut neither = jupiter();
    neither.versions = Ok(AdlVersionsInfoX2::default());
    assert_eq!(open(neither).err(), Some(Absent::Malformed));
}

/// AC-T7.3.1, a missing library: do load ADL under a name no machine has, through the OS's own
/// loader; expect it refused with the name tried.
#[cfg(target_os = "windows")]
#[test]
fn a_missing_adl_is_refused_with_what_was_tried() {
    const NAMES: &[&str] = &["hex-gpu-absent-adl.dll"];
    let error = crate::nvml::load::load_api(NAMES, super::sys::Api::open).unwrap_err();
    assert!(matches!(
        super::sys::absent(error),
        Absent::NoLibrary { tried, .. } if tried == NAMES
    ));
}

/// AC-T7.3.1: two full native identities with the same segment-less BDF remain independent.
#[test]
fn native_segments_precede_grouping_and_keep_exact_alias_counter_pairs() {
    let mut adl = jupiter();
    adl.count = Ok(2);
    adl.rows.truncate(2);
    adl.native = adl
        .rows
        .iter()
        .enumerate()
        .map(|(n, row)| {
            (
                super::ffi::adl_string(&row.str_pnp_string)
                    .unwrap()
                    .to_owned(),
                Ok(PciAddress {
                    domain: u32::try_from(n).unwrap() + 41,
                    bus: 196,
                    device: 0,
                    function: 0,
                }),
            )
        })
        .collect();
    let mut found = open(adl).unwrap();
    assert_eq!(found.gpus.len(), 2);
    for (n, gpu) in found.gpus.iter().enumerate() {
        let aliases = found.amd.aliases(gpu.identity).unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(
            aliases[0].instance(),
            super::ffi::adl_string(&found.amd.calls.rows[n].str_pnp_string).unwrap()
        );
        assert_eq!(
            aliases[0].udid(),
            super::ffi::adl_string(&found.amd.calls.rows[n].str_udid).unwrap()
        );
        assert_eq!(found.amd.adapters[n].index, i32::try_from(n).unwrap());
    }
    assert_eq!(sample(&mut found).len(), 2);
}

/// AC-T7.3.1: missing native correspondence is per-device uncertainty, never a guessed segment.
#[test]
fn missing_or_conflicting_native_properties_cannot_publish_a_guessed_gpu() {
    let mut missing = jupiter();
    missing.native = missing
        .rows
        .iter()
        .map(|row| {
            (
                super::ffi::adl_string(&row.str_pnp_string)
                    .unwrap()
                    .to_owned(),
                Err(Unread::Gone),
            )
        })
        .collect();
    let found = open(missing).unwrap();
    assert!(found.gpus.is_empty());
    assert_eq!(found.skipped.len(), 5);
    let mut conflict = jupiter();
    conflict.rows[1].str_adapter_name = text("contradictory physical metadata");
    let found = open(conflict).unwrap();
    assert!(found.gpus.is_empty());
    assert_eq!(
        found.skipped,
        vec![Skipped::Adl {
            adapter: 0,
            why: Unread::Ambiguous
        }]
    );
}

/// AC-T7.3.1: sampling keeps the same identity/aliases; a changed native join never repicks.
#[test]
fn native_alias_loss_withholds_fresh_counters_and_retains_the_original_receipt() {
    let mut found = open(jupiter()).unwrap();
    let identity = found.gpus[0].identity;
    assert_eq!(found.amd.aliases(identity).unwrap().len(), 5);
    found.amd.calls.native[2].1 = Err(Unread::WindowsCm { code: 37 });
    let before = found.amd.calls.calls;
    assert_eq!(
        sample(&mut found),
        vec![Load {
            busy_percent: Err(Unread::WindowsCm { code: 37 }),
            memory_used_bytes: Err(Unread::WindowsCm { code: 37 }),
        }]
    );
    assert_eq!(found.amd.calls.calls, before);
    assert_eq!(found.gpus[0].identity, identity);
    assert_eq!(
        found.amd.aliases(identity).unwrap_err(),
        Unread::WindowsCm { code: 37 }
    );
    found.amd.calls.native[2].1 = Ok(PciAddress {
        domain: 9,
        bus: 196,
        device: 0,
        function: 0,
    });
    assert_eq!(sample(&mut found)[0].busy_percent, Err(Unread::Ambiguous));
}
