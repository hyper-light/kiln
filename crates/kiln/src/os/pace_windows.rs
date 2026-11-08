//! Windows' reads for [`crate::sense::pace`] (W05-R26, W13-R01; docs/runners.md "Pace"), each open to
//! a standard user:
//!
//! - the processors' clocks against their most: `CallNtPowerInformation(ProcessorInformation)`'s
//!   `MhzLimit` over `MaxMhz` ("the maximum specified processor clock frequency multiplied by the
//!   current processor thermal throttle limit"), summed over the processors;
//! - the run queue and paging (PDH): `\System\Processor Queue Length` (threads ready and waiting)
//!   with `\Processor(_Total)\% Processor Time` (the CPUs busy), and `\Memory\Pages Input/sec`
//!   (pages read from disk to resolve hard faults), PDH's rate over the interval since its
//!   previous read;
//! - the page files: `GlobalMemoryStatusEx`'s commit limit less physical memory;
//! - the battery: `GetSystemPowerStatus`;
//! - the work device's counters ([`crate::os::sense_disk_windows`]).
//!
//! **Events** ([`watch`]): `PowerSettingRegisterNotification` on `GUID_ACDC_POWER_SOURCE` (a
//! callback; no window or service needed). Thermal zones' temperatures are WMI's and an
//! administrator's (`MSAcpi_ThermalZoneTemperature`); the throttle limit is read at runs' starts
//! and ends.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, mpsc};

use crate::sense::pace::Raw;

/// `SYSTEM_POWER_STATUS`.
#[repr(C)]
#[derive(Default)]
struct PowerStatus {
    ac_line: u8,
    battery_flag: u8,
    percent: u8,
    system_status: u8,
    life_s: u32,
    full_life_s: u32,
}

/// `PROCESSOR_POWER_INFORMATION`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct ProcessorPower {
    number: u32,
    max_mhz: u32,
    current_mhz: u32,
    mhz_limit: u32,
    max_idle_state: u32,
    current_idle_state: u32,
}

/// `MEMORYSTATUSEX`.
#[repr(C)]
#[derive(Default)]
struct MemoryStatus {
    length: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

/// `PDH_FMT_COUNTERVALUE` read as a double.
#[repr(C)]
#[derive(Default)]
struct CounterValue {
    status: u32,
    value: f64,
}

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `GUID_ACDC_POWER_SOURCE` (winnt.h).
const ACDC_POWER_SOURCE: Guid = Guid { data1: 0x5d3e_9a59, data2: 0xe9d5, data3: 0x4b00, data4: [0xa6, 0xbd, 0xff, 0x34, 0xff, 0x51, 0x65, 0x48] };

type Callback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32;

/// `DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS`.
#[repr(C)]
struct Subscribe {
    callback: Option<Callback>,
    context: *mut c_void,
}

/// `ProcessorInformation` of `POWER_INFORMATION_LEVEL`.
const PROCESSOR_INFORMATION: i32 = 11;
/// `DEVICE_NOTIFY_CALLBACK`.
const DEVICE_NOTIFY_CALLBACK: u32 = 2;
/// `PDH_FMT_DOUBLE`.
const PDH_FMT_DOUBLE: u32 = 0x200;
/// `BATTERY_FLAG_NO_BATTERY`, `BATTERY_FLAG_UNKNOWN`.
const NO_BATTERY: u8 = 128;
const UNKNOWN: u8 = 255;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetSystemPowerStatus(s: *mut PowerStatus) -> i32;
    fn GlobalMemoryStatusEx(m: *mut MemoryStatus) -> i32;
}

#[link(name = "powrprof")]
unsafe extern "system" {
    fn CallNtPowerInformation(level: i32, input: *const c_void, input_len: u32, output: *mut c_void, output_len: u32) -> i32;
    fn PowerSettingRegisterNotification(guid: *const Guid, flags: u32, recipient: *mut c_void, handle: *mut *mut c_void) -> u32;
}

#[link(name = "pdh")]
unsafe extern "system" {
    fn PdhOpenQueryW(source: *const u16, user: usize, query: *mut isize) -> u32;
    fn PdhAddEnglishCounterW(query: isize, path: *const u16, user: usize, counter: *mut isize) -> u32;
    fn PdhCollectQueryData(query: isize) -> u32;
    fn PdhGetFormattedCounterValue(counter: isize, format: u32, kind: *mut u32, value: *mut CounterValue) -> u32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// PDH's query of the three counters.
struct Pdh {
    query: isize,
    queue: isize,
    busy: isize,
    pages_in: isize,
}

impl Pdh {
    fn open() -> Option<Pdh> {
        let mut query = 0isize;
        // SAFETY: a query handle written on success; counter paths NUL-terminated.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &raw mut query) != 0 {
                return None;
            }
            let add = |path: &str| {
                let mut c = 0isize;
                (PdhAddEnglishCounterW(query, wide(path).as_ptr(), 0, &raw mut c) == 0).then_some(c)
            };
            let p = Pdh { query, queue: add(r"\System\Processor Queue Length")?, busy: add(r"\Processor(_Total)\% Processor Time")?, pages_in: add(r"\Memory\Pages Input/sec")? };
            PdhCollectQueryData(query);
            Some(p)
        }
    }

    fn value(counter: isize) -> Option<f64> {
        let mut v = CounterValue::default();
        // SAFETY: a counter of this process's query; the value written on success.
        let rc = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, std::ptr::null_mut(), &raw mut v) };
        (rc == 0 && v.status == 0 && v.value.is_finite()).then_some(v.value)
    }

    /// Ready threads waiting, the CPUs' busy share, and pages read in a second.
    fn read(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        // SAFETY: this process's query.
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return (None, None, None);
        }
        (Self::value(self.queue), Self::value(self.busy).map(|b| (b / 100.0).clamp(0.0, 1.0)), Self::value(self.pages_in))
    }
}

/// The processors' throttle limit against their most: Σ `MhzLimit` / Σ `MaxMhz`.
fn clock(cpus: u32) -> Option<f64> {
    let mut info = vec![ProcessorPower::default(); usize::try_from(cpus).ok()?.max(1)];
    let len = u32::try_from(std::mem::size_of_val(info.as_slice())).ok()?;
    // SAFETY: an output buffer of `len` bytes, one record per processor.
    let rc = unsafe { CallNtPowerInformation(PROCESSOR_INFORMATION, std::ptr::null(), 0, info.as_mut_ptr().cast(), len) };
    if rc != 0 {
        return None;
    }
    let (limit, max) = info.iter().fold((0u64, 0u64), |(l, m), p| (l + u64::from(p.mhz_limit.min(p.max_mhz)), m + u64::from(p.max_mhz)));
    #[allow(clippy::cast_precision_loss)]
    (max > 0).then(|| limit as f64 / max as f64)
}

/// This machine's reads: PDH's query and the work device.
pub struct Reader {
    pdh: Option<Pdh>,
    device: Option<crate::os::sense_disk_windows::Device>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        Reader { pdh: Pdh::open(), device: crate::os::sense_disk_windows::Device::of(work_dir) }
    }

    pub fn read(&mut self) -> Raw {
        let cpus = u32::try_from(std::thread::available_parallelism().map(std::num::NonZero::get).unwrap_or(1)).unwrap_or(1);
        let (queue, busy, pages_in) = self.pdh.as_ref().map_or((None, None, None), Pdh::read);
        let mut power = PowerStatus::default();
        // SAFETY: a struct the call fills.
        let have_power = unsafe { GetSystemPowerStatus(&raw mut power) } != 0;
        let battery = have_power && power.battery_flag != NO_BATTERY && power.battery_flag != UNKNOWN;
        let mut mem = MemoryStatus { length: u32::try_from(std::mem::size_of::<MemoryStatus>()).unwrap_or(0), ..MemoryStatus::default() };
        // SAFETY: a struct the call fills, its length set.
        let swap = (unsafe { GlobalMemoryStatusEx(&raw mut mem) } != 0).then(|| {
            let total = mem.total_page_file.saturating_sub(mem.total_phys);
            let used = mem.total_page_file.saturating_sub(mem.avail_page_file).saturating_sub(mem.total_phys.saturating_sub(mem.avail_phys));
            (total, used.min(total))
        }).filter(|(t, _)| *t > 0);
        Raw {
            clock: clock(cpus),
            runnable: queue.zip(busy).map(|(q, b)| q + b * f64::from(cpus)),
            cpus,
            swapin_rate: pages_in,
            swap,
            device: self.device.as_mut().and_then(crate::os::sense_disk_windows::Device::counters),
            on_battery: battery.then_some(power.ac_line == 0),
            percent: (battery && power.percent <= 100).then_some(power.percent),
            ..Raw::default()
        }
    }
}

static WAKES: AtomicU64 = AtomicU64::new(0);

pub fn wakes() -> u64 {
    WAKES.load(Ordering::Relaxed)
}

/// Where the power callback says a change.
fn said() -> &'static Mutex<Option<mpsc::Sender<()>>> {
    static S: OnceLock<Mutex<Option<mpsc::Sender<()>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

unsafe extern "system" fn changed(_context: *mut c_void, _kind: u32, _setting: *mut c_void) -> u32 {
    if let Ok(s) = said().lock()
        && let Some(tx) = s.as_ref()
    {
        let _ = tx.send(());
    }
    0
}

/// A thread waits on the power source's changes and calls `on` for each.
pub fn watch(on: impl Fn() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    if let Ok(mut s) = said().lock() {
        *s = Some(tx);
    }
    // Kept for the process's life: the registration is never undone.
    let params = Box::leak(Box::new(Subscribe { callback: Some(changed), context: std::ptr::null_mut() }));
    let mut handle: *mut c_void = std::ptr::null_mut();
    // SAFETY: a GUID and parameters that outlive the registration (static, leaked).
    let rc = unsafe { PowerSettingRegisterNotification(&ACDC_POWER_SOURCE, DEVICE_NOTIFY_CALLBACK, std::ptr::from_mut(params).cast(), &raw mut handle) };
    if rc != 0 {
        tracing::info!(code = rc, "power source changes aren't heard here: the battery is read at runs' starts and ends");
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-pace".into()).spawn(move || {
        while rx.recv().is_ok() {
            WAKES.fetch_add(1, Ordering::Relaxed);
            on();
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "power source changes aren't heard: the battery is read at runs' starts and ends");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W05-R26: a quick check of the reads on this machine, with what each costs.
    #[test]
    fn w05_r26_reads_this_machine() {
        let mut r = Reader::new(Path::new("C:\\"));
        let t = std::time::Instant::now();
        let a = r.read();
        let first = t.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let t = std::time::Instant::now();
        let b = r.read();
        let second = t.elapsed();
        assert!(b.cpus > 0, "{b:?}");
        let mut kept = crate::sense::pace::Kept::default();
        let read = crate::sense::pace::between(&a, &b, 1_100_000, &mut kept);
        eprintln!("windows read: {} µs, then {} µs; clock {:?}, runnable {:?}, pages in/s {:?}, page file {:?}, battery {:?}; {read:?}", first.as_micros(), second.as_micros(), b.clock, b.runnable, b.swapin_rate, b.swap, b.on_battery);
    }

    /// W05-R26: an idle runner's event thread sleeps: its wakes over a minute (`KILN_PACE_IDLE=1`;
    /// it takes the minute).
    #[test]
    fn w05_r26_idle_wakes() {
        if std::env::var_os("KILN_PACE_IDLE").is_none() {
            return;
        }
        let calls = std::sync::Arc::new(AtomicU64::new(0));
        let c = std::sync::Arc::clone(&calls);
        let before = wakes();
        watch(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });
        std::thread::sleep(std::time::Duration::from_secs(60));
        eprintln!("windows idle minute: {} wakes, {} reads", wakes() - before, calls.load(Ordering::Relaxed));
    }
}
