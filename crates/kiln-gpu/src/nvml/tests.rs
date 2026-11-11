//! NVML's return values as fixtures: what NVML answered on adacachy2 (GeForce RTX 5060 Ti, driver
//! 620.70.06, 2026-11-09: count 1, an index past it `NVML_ERROR_INVALID_ARGUMENT`, `nvmlMemory_t`
//! 8546942976 / 7839023104 / 707919872 bytes, `nvmlPciInfo_t` 68 bytes) and the errors nvml.h documents
//! each call answering: a device without permission, a MIG-enabled GPU's utilisation, a GPU lost
//! between samples, a driver not loaded and a library missing (`AC-T7.3.1`).

use std::cell::Cell;

use super::load::{self, DynLoadError};
use super::{BUS_ID_BYTES, Calls, DRIVER_VERSION_BYTES, Memory, NAME_BYTES, absent, open, sys};
use crate::{Absent, Gpu, Identity, Load, PciAddress, Skipped, Unread, Vendor};

const INVALID_ARGUMENT: i32 = 2;
const NOT_SUPPORTED: i32 = 3;
const NO_PERMISSION: i32 = 4;
const DRIVER_NOT_LOADED: i32 = 9;
const GPU_IS_LOST: i32 = 15;

/// One GPU as NVML answers for it: each call's value, or its `nvmlReturn_t`.
#[derive(Clone)]
struct Recorded {
    handle: Result<(), i32>,
    name: &'static [u8],
    bus_id: &'static [u8],
    total: u64,
    used: Result<u64, i32>,
    busy: Result<u32, i32>,
    /// Set to answer `NVML_ERROR_GPU_IS_LOST` from then on: the GPU fell off the bus.
    lost: Cell<bool>,
}

/// The RTX 5060 Ti as adacachy2's NVML answered for it.
fn rtx_5060_ti() -> Recorded {
    Recorded {
        handle: Ok(()),
        name: b"NVIDIA GeForce RTX 5060 Ti\0",
        bus_id: b"00000000:01:00.0\0",
        total: 8_546_942_976,
        used: Ok(707_919_872),
        busy: Ok(0),
        lost: Cell::new(false),
    }
}

/// NVML, answering from a recording, counting every call made to it.
struct Nvml {
    driver: Result<&'static [u8], i32>,
    count: Result<u32, i32>,
    gpus: Vec<Recorded>,
    calls: Cell<u32>,
}

impl Nvml {
    fn with(gpus: Vec<Recorded>) -> Nvml {
        Nvml {
            driver: Ok(b"620.70.06\0"),
            count: Ok(u32::try_from(gpus.len()).unwrap()),
            gpus,
            calls: Cell::new(0),
        }
    }

    fn called(&self) {
        self.calls.set(self.calls.get() + 1);
    }

    fn gpu(&self, device: usize) -> Result<&Recorded, i32> {
        let gpu = self.gpus.get(device).ok_or(INVALID_ARGUMENT)?;
        if gpu.lost.get() {
            Err(GPU_IS_LOST)
        } else {
            Ok(gpu)
        }
    }
}

/// Copies a recorded C string into NVML's buffer, as NVML writes one.
fn write(out: &mut [u8], bytes: &[u8]) {
    out.fill(0);
    let n = bytes.len().min(out.len());
    out[..n].copy_from_slice(&bytes[..n]);
}

impl Calls for &Nvml {
    type Device = usize;

    fn driver_version(self, out: &mut [u8; DRIVER_VERSION_BYTES]) -> Result<(), i32> {
        self.called();
        write(out, self.driver?);
        Ok(())
    }

    fn count(self) -> Result<u32, i32> {
        self.called();
        self.count
    }

    fn device(self, index: u32) -> Result<usize, i32> {
        self.called();
        let device = usize::try_from(index).unwrap();
        self.gpus.get(device).ok_or(INVALID_ARGUMENT)?.handle?;
        Ok(device)
    }

    fn name(self, device: usize, out: &mut [u8; NAME_BYTES]) -> Result<(), i32> {
        self.called();
        write(out, self.gpu(device)?.name);
        Ok(())
    }

    fn bus_id(self, device: usize, out: &mut [u8; BUS_ID_BYTES]) -> Result<(), i32> {
        self.called();
        write(out, self.gpu(device)?.bus_id);
        Ok(())
    }

    fn memory(self, device: usize) -> Result<Memory, i32> {
        self.called();
        let gpu = self.gpu(device)?;
        Ok(Memory {
            total: gpu.total,
            used: gpu.used?,
        })
    }

    fn utilization(self, device: usize) -> Result<u32, i32> {
        self.called();
        self.gpu(device)?.busy
    }
}

fn listed(found: &super::Found<&Nvml>) -> Vec<Gpu> {
    found.gpus.iter().map(|(gpu, _)| gpu.clone()).collect()
}

fn sample(found: &super::Found<&Nvml>) -> Vec<Load> {
    found
        .gpus
        .iter()
        .map(|(_, device)| device.sample())
        .collect()
}

/// AC-T7.3.1: do open adacachy2's recorded NVML; expect its one GPU with what doesn't change, as
/// nvidia-smi names it: model, total memory, PCI address and the driver's version.
#[test]
fn adacachy2s_gpu_is_listed_as_nvml_recorded_it() {
    let nvml = Nvml::with(vec![rtx_5060_ti()]);
    let found = open(&nvml).unwrap();
    assert_eq!(
        listed(&found),
        vec![Gpu {
            vendor: Vendor::Nvidia,
            model: "NVIDIA GeForce RTX 5060 Ti".to_owned(),
            memory_total_bytes: 8_546_942_976,
            identity: Identity::Pci(PciAddress::parse("0000:01:00.0").unwrap()),
            driver_version: "620.70.06".to_owned(),
        }]
    );
    assert!(found.skipped.is_empty());
}

/// AC-T7.3.1: do sample two GPUs; expect each one's busy percent and memory in use, for exactly two
/// NVML calls a GPU (the fewest NVML's interface allows), and nothing read at open asked again.
#[test]
fn a_sample_is_two_nvml_calls_a_gpu() {
    let mut busy = rtx_5060_ti();
    busy.bus_id = b"00000000:41:00.0\0";
    busy.busy = Ok(87);
    busy.used = Ok(6_000_000_000);
    let nvml = Nvml::with(vec![rtx_5060_ti(), busy]);
    let found = open(&nvml).unwrap();
    let opened = nvml.calls.get();
    assert_eq!(
        sample(&found),
        vec![
            Load {
                busy_percent: Ok(0),
                memory_used_bytes: Ok(707_919_872)
            },
            Load {
                busy_percent: Ok(87),
                memory_used_bytes: Ok(6_000_000_000)
            },
        ]
    );
    assert_eq!(nvml.calls.get() - opened, 4);
}

/// AC-T7.3.1: do open NVML whose index order isn't the bus's; expect the GPUs in PCI order (NVML's
/// order "has no guarantees of consistency between reboots", nvml.h).
#[test]
fn gpus_are_listed_in_pci_order() {
    let mut late = rtx_5060_ti();
    late.bus_id = b"00000000:c1:00.0\0";
    let nvml = Nvml::with(vec![late, rtx_5060_ti()]);
    let found = open(&nvml).unwrap();
    let order: Vec<String> = listed(&found)
        .iter()
        .map(|g| g.pci().unwrap().to_string())
        .collect();
    assert_eq!(order, ["0000:01:00.0", "0000:c1:00.0"]);
}

/// AC-T7.3.1, CLAUDE.md §3: do open NVML counting a GPU this process has no permission to (a
/// container given one of two); expect the other listed and that one skipped with NVML's code.
#[test]
fn a_gpu_nvml_counts_but_wont_give_is_skipped_with_why() {
    let mut hidden = rtx_5060_ti();
    hidden.handle = Err(NO_PERMISSION);
    let nvml = Nvml::with(vec![rtx_5060_ti(), hidden]);
    let found = open(&nvml).unwrap();
    assert_eq!(listed(&found).len(), 1);
    assert_eq!(
        found.skipped,
        vec![Skipped::Nvml {
            index: 1,
            why: Unread::Nvml {
                code: NO_PERMISSION
            }
        }]
    );
}

/// AC-T7.3.1, hot-unplug: do sample, then lose the GPU off the bus and sample again; expect its
/// counters `Gone` for one NVML call, not two, and the other GPU still read.
#[test]
fn a_gpu_lost_between_samples_reads_gone() {
    let mut other = rtx_5060_ti();
    other.bus_id = b"00000000:41:00.0\0";
    let nvml = Nvml::with(vec![rtx_5060_ti(), other]);
    let found = open(&nvml).unwrap();
    assert!(sample(&found).iter().all(|l| l.busy_percent.is_ok()));
    nvml.gpus[0].lost.set(true);
    let before = nvml.calls.get();
    let loads = sample(&found);
    assert_eq!(
        loads[0],
        Load {
            busy_percent: Err(Unread::Gone),
            memory_used_bytes: Err(Unread::Gone)
        }
    );
    assert!(loads[1].busy_percent.is_ok() && loads[1].memory_used_bytes.is_ok());
    assert_eq!(nvml.calls.get() - before, 3);
}

/// AC-T7.3.1: do sample a MIG-enabled GPU, whose utilisation NVML doesn't support (nvml.h); expect its
/// busy percent `Unsupported` and its memory in use still read.
#[test]
fn a_mig_gpu_still_reads_its_memory() {
    let mut mig = rtx_5060_ti();
    mig.busy = Err(NOT_SUPPORTED);
    let nvml = Nvml::with(vec![mig]);
    let found = open(&nvml).unwrap();
    assert_eq!(
        sample(&found),
        vec![Load {
            busy_percent: Err(Unread::Unsupported),
            memory_used_bytes: Ok(707_919_872)
        }]
    );
}

/// AC-T7.3.1, CLAUDE.md §3: do sample a GPU whose utilisation reads 101 percent; expect it refused
/// as malformed, not clamped to 100.
#[test]
fn a_percent_past_100_is_refused_not_clamped() {
    let mut odd = rtx_5060_ti();
    odd.busy = Ok(101);
    let nvml = Nvml::with(vec![odd]);
    let found = open(&nvml).unwrap();
    assert_eq!(sample(&found)[0].busy_percent, Err(Unread::Malformed));
}

/// AC-T7.3.1, hostile input: do open NVML answering with a name without its NUL, an empty name, a name
/// that isn't UTF-8 and bus ids that aren't addresses; expect each GPU skipped as malformed.
#[test]
fn what_nvml_doesnt_document_is_malformed() {
    let mut gpus = Vec::new();
    let cases: [(&'static [u8], &'static [u8]); 5] = [
        (&[b'A'; NAME_BYTES], b"00000000:01:00.0\0"),
        (b"\0", b"00000000:02:00.0\0"),
        (b"NVIDIA \xff\0", b"00000000:03:00.0\0"),
        (b"NVIDIA GeForce RTX 5060 Ti\0", b"00000000:04:00.9\0"),
        (b"NVIDIA GeForce RTX 5060 Ti\0", b"\0"),
    ];
    for (name, bus_id) in cases {
        let mut gpu = rtx_5060_ti();
        gpu.name = name;
        gpu.bus_id = bus_id;
        gpus.push(gpu);
    }
    let nvml = Nvml::with(gpus);
    let found = open(&nvml).unwrap();
    assert!(found.gpus.is_empty());
    assert_eq!(found.skipped.len(), 5);
    assert!(found.skipped.iter().all(|s| matches!(
        s,
        Skipped::Nvml {
            why: Unread::Malformed,
            ..
        }
    )));
}

/// AC-T7.3.1, CLAUDE.md §2: do open NVML counting one GPU past the NVIDIA driver's own 248; expect it
/// refused as malformed before any device is asked for.
#[test]
fn a_count_past_the_drivers_bound_is_refused() {
    let mut nvml = Nvml::with(vec![rtx_5060_ti()]);
    nvml.count = Ok(249);
    assert_eq!(open(&nvml).err(), Some(Absent::Malformed));
}

/// AC-T7.3.1: do open NVML with no GPU, and NVML whose driver version or count fails; expect none
/// listed, and the failures refused with NVML's code.
#[test]
fn no_gpu_and_nvmls_own_failures() {
    let none = Nvml::with(Vec::new());
    let found = open(&none).unwrap();
    assert!(found.gpus.is_empty() && found.skipped.is_empty());
    let mut failing = Nvml::with(vec![rtx_5060_ti()]);
    failing.driver = Err(DRIVER_NOT_LOADED);
    assert_eq!(
        open(&failing).err(),
        Some(Absent::Nvml {
            code: DRIVER_NOT_LOADED
        })
    );
    failing.driver = Ok(b"620.70.06\0");
    failing.count = Err(NO_PERMISSION);
    assert_eq!(
        open(&failing).err(),
        Some(Absent::Nvml {
            code: NO_PERMISSION
        })
    );
}

/// AC-T7.3.1: do turn each way NVML can fail to start into why NVIDIA's GPUs are absent; expect the
/// driver not loaded named, and the library's and symbol's names kept.
#[test]
fn why_nvml_didnt_start_is_kept() {
    assert_eq!(
        absent(DynLoadError::InitFailed {
            code: DRIVER_NOT_LOADED
        }),
        Absent::Nvml {
            code: DRIVER_NOT_LOADED
        }
    );
    assert!(
        Absent::Nvml {
            code: DRIVER_NOT_LOADED
        }
        .to_string()
        .contains("NVIDIA's driver isn't loaded")
    );
    assert_eq!(
        absent(DynLoadError::MissingSymbol {
            name: "nvmlDeviceGetCount_v2"
        }),
        Absent::NoSymbol {
            name: "nvmlDeviceGetCount_v2"
        }
    );
}

/// AC-T7.3.1, a missing library: do load NVML under a name no machine has, through the OS's own
/// loader; expect it refused with the names tried and the loader's word for the one that failed.
#[test]
fn a_missing_library_is_refused_with_what_was_tried() {
    #[cfg(target_os = "linux")]
    const NAMES: &[&str] = &["libhex-gpu-absent.so.1"];
    #[cfg(target_os = "windows")]
    const NAMES: &[&str] = &["hex-gpu-absent.dll"];
    let error = load::load_api(NAMES, sys::Api::open).unwrap_err();
    let DynLoadError::LoadFailed { names, source } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(*names, NAMES);
    #[cfg(target_os = "linux")]
    assert!(source.contains("libhex-gpu-absent.so.1"), "{source}");
    #[cfg(target_os = "windows")]
    assert!(source.contains("hex-gpu-absent.dll"), "{source}");
    assert!(matches!(absent(error), Absent::NoLibrary { tried, .. } if tried == NAMES));
}

/// AC-T7.3.1: do load from names the first of which isn't there; expect the next tried. Then from a
/// library that loads without a symbol; expect the search to stop there, the right library broken.
#[test]
fn the_loader_tries_each_name_and_stops_at_a_broken_library() {
    const NAMES: &[&str] = &["first", "second", "third"];
    let mut tried = Vec::new();
    let loaded = load::load_api(NAMES, |name| {
        tried.push(name);
        if name == "second" {
            Ok(name)
        } else {
            Err(DynLoadError::LoadFailed {
                names: &[],
                source: format!("{name}: not here"),
            })
        }
    });
    assert_eq!(loaded, Ok("second"));
    assert_eq!(tried, ["first", "second"]);

    let broken = load::load_api(NAMES, |name| match name {
        "first" => Err(DynLoadError::LoadFailed {
            names: &[],
            source: "first: not here".to_owned(),
        }),
        _ => Err::<(), _>(DynLoadError::MissingSymbol {
            name: "nvmlInit_v2",
        }),
    });
    assert_eq!(
        broken,
        Err(DynLoadError::MissingSymbol {
            name: "nvmlInit_v2"
        })
    );

    let none = load::load_api(NAMES, |name| {
        Err::<(), _>(DynLoadError::LoadFailed {
            names: &[],
            source: format!("{name}: not here"),
        })
    });
    assert_eq!(
        none,
        Err(DynLoadError::LoadFailed {
            names: NAMES,
            source: "third: not here".to_owned()
        })
    );
}
