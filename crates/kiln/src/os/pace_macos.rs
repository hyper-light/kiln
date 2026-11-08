//! macOS's reads for [`crate::sense::pace`] (W05-R26, W13-R01; docs/runners.md "Pace"), each open to
//! any user:
//!
//! - the thermal pressure level: notify(3)'s `com.apple.system.thermalpressurelevel` state
//!   (`OSThermalNotification.h`: nominal, moderate, heavy, trapping, sleeping; ProcessInfo's
//!   `thermalState` is the same levels as nominal, fair, serious and critical). macOS gives no
//!   unprivileged CPU clock (`powermetrics` needs root);
//! - the run queue: the 1-minute load average (`vm.loadavg`, the scheduler's exponentially decayed
//!   count of runnable threads);
//! - swap: `vm.swapusage` and `host_statistics64`'s `HOST_VM_INFO64` `swapins` (pages swapped in
//!   from the compressor's swap files);
//! - the battery: IOKit's power sources (`IOPSCopyPowerSourcesInfo`: the internal battery's `Power
//!   Source State` and capacity);
//! - the work device's counters ([`crate::os::sense_disk_macos`]).
//!
//! **Events** ([`watch`]): notify(3) file descriptors for the thermal level and the power source
//! (`com.apple.system.powersources.source`, IOKit's `kIOPSNotifyPowerSource`), a thread blocked in
//! poll(2) on them.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int};
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::poll::{PollFd, PollFlags, PollTimeout};

use crate::os::sense::cf;
use crate::sense::pace::Raw;

const THERMAL: &CStr = c"com.apple.system.thermalpressurelevel";
const POWER: &CStr = c"com.apple.system.powersources.source";
/// `kIOPSNotifyTimeRemaining` (IOPowerSources.h): posted "when percent remaining or time remaining
/// changes", and as the source does: the battery's charge as it moves.
const REMAINING: &CStr = c"com.apple.system.powersources.timeremaining";

type MachPort = u32;

/// `vm_statistics64` (`mach/vm_statistics.h`), 38 `integer_t`s.
#[repr(C)]
#[derive(Default)]
struct VmStatistics64 {
    free_count: u32,
    active_count: u32,
    inactive_count: u32,
    wire_count: u32,
    zero_fill_count: u64,
    reactivations: u64,
    pageins: u64,
    pageouts: u64,
    faults: u64,
    cow_faults: u64,
    lookups: u64,
    hits: u64,
    purges: u64,
    purgeable_count: u32,
    speculative_count: u32,
    decompressions: u64,
    compressions: u64,
    swapins: u64,
    swapouts: u64,
    compressor_page_count: u32,
    throttled_count: u32,
    external_page_count: u32,
    internal_page_count: u32,
    total_uncompressed_pages_in_compressor: u64,
}

const _: () = assert!(std::mem::size_of::<VmStatistics64>() == 152);
/// `HOST_VM_INFO64` (`mach/host_info.h`).
const HOST_VM_INFO64: i32 = 4;

/// `struct xsw_usage` (`sys/sysctl.h`).
#[repr(C)]
#[derive(Default)]
struct SwapUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: i32,
}

/// `struct loadavg` (`sys/resource.h`): fixed-point loads and their scale.
#[repr(C)]
#[derive(Default)]
struct LoadAvg {
    ldavg: [u32; 3],
    fscale: std::ffi::c_long,
}

unsafe extern "C" {
    fn mach_host_self() -> MachPort;
    fn host_statistics64(host: MachPort, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    fn mach_port_deallocate(task: MachPort, name: MachPort) -> i32;
    static mach_task_self_: MachPort;
    fn notify_register_check(name: *const c_char, out_token: *mut c_int) -> u32;
    fn notify_register_file_descriptor(name: *const c_char, notify_fd: *mut c_int, flags: c_int, out_token: *mut c_int) -> u32;
    fn notify_get_state(token: c_int, state: *mut u64) -> u32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> cf::CFTypeRef;
    fn IOPSCopyPowerSourcesList(blob: cf::CFTypeRef) -> cf::CFTypeRef;
    fn IOPSGetPowerSourceDescription(blob: cf::CFTypeRef, ps: cf::CFTypeRef) -> cf::CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(a: cf::CFTypeRef) -> cf::CFIndex;
    fn CFArrayGetValueAtIndex(a: cf::CFTypeRef, i: cf::CFIndex) -> cf::CFTypeRef;
}

/// A sysctl's value by name, as `T`.
fn sysctl<T: Default>(name: &CStr) -> Option<T> {
    let mut v = T::default();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `v` is `len` bytes, written by the kernel up to that.
    let rc = unsafe { nix::libc::sysctlbyname(name.as_ptr(), (&raw mut v).cast(), &raw mut len, std::ptr::null_mut(), 0) };
    (rc == 0 && len == std::mem::size_of::<T>()).then_some(v)
}

fn swapins() -> Option<u64> {
    // SAFETY: Mach calls with a buffer of the size they're told; the host port given back.
    unsafe {
        let host = mach_host_self();
        let mut v = VmStatistics64::default();
        let mut count = u32::try_from(std::mem::size_of::<VmStatistics64>() / std::mem::size_of::<i32>()).unwrap_or(0);
        let rc = host_statistics64(host, HOST_VM_INFO64, (&raw mut v).cast(), &raw mut count);
        mach_port_deallocate(mach_task_self_, host);
        (rc == 0).then_some(v.swapins)
    }
}

/// The internal battery: on battery or not, and how full; none without one.
fn battery() -> (Option<bool>, Option<u8>) {
    // SAFETY: Copy calls owned (released when dropped); Get calls borrowed from what's held.
    unsafe {
        let blob = cf::Owned(IOPSCopyPowerSourcesInfo());
        if blob.0.is_null() {
            return (None, None);
        }
        let list = cf::Owned(IOPSCopyPowerSourcesList(blob.0));
        if list.0.is_null() {
            return (None, None);
        }
        for i in 0..CFArrayGetCount(list.0) {
            let d = IOPSGetPowerSourceDescription(blob.0, CFArrayGetValueAtIndex(list.0, i));
            if d.is_null() || cf::text(cf::get(d, c"Type")).as_deref() != Some("InternalBattery") {
                continue;
            }
            let on = cf::text(cf::get(d, c"Power Source State")).map(|s| s == "Battery Power");
            let (cur, max) = (cf::number(cf::get(d, c"Current Capacity")), cf::number(cf::get(d, c"Max Capacity")));
            let pct = cur.zip(max).filter(|(_, m)| *m > 0).and_then(|(c, m)| u8::try_from((c * 100 / m).clamp(0, 100)).ok());
            return (on, pct);
        }
        (None, None)
    }
}

/// This machine's reads: the thermal level's token, and the work device.
pub struct Reader {
    thermal: Option<c_int>,
    device: Option<crate::os::sense_disk_macos::Device>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let mut token: c_int = 0;
        // SAFETY: a NUL-terminated name, a token written on success.
        let ok = unsafe { notify_register_check(THERMAL.as_ptr(), &raw mut token) } == 0;
        Reader { thermal: ok.then_some(token), device: crate::os::sense_disk_macos::Device::of(work_dir) }
    }

    pub fn read(&mut self) -> Raw {
        let level = self.thermal.and_then(|t| {
            let mut state = 0u64;
            // SAFETY: a token this process registered.
            (unsafe { notify_get_state(t, &raw mut state) } == 0).then(|| u8::try_from(state).unwrap_or(u8::MAX))
        });
        #[allow(clippy::cast_precision_loss)]
        let runnable = sysctl::<LoadAvg>(c"vm.loadavg").filter(|l| l.fscale > 0).and_then(|l| Some(f64::from(*l.ldavg.first()?) / l.fscale as f64));
        let swap = sysctl::<SwapUsage>(c"vm.swapusage").filter(|s| s.total > 0).map(|s| (s.total, s.used));
        let (on_battery, percent) = battery();
        Raw {
            level,
            runnable,
            cpus: u32::try_from(std::thread::available_parallelism().map(std::num::NonZero::get).unwrap_or(1)).unwrap_or(1),
            swapins: swapins(),
            swap,
            device: self.device.as_mut().and_then(crate::os::sense_disk_macos::Device::counters),
            on_battery,
            percent,
            ..Raw::default()
        }
    }
}

static WAKES: AtomicU64 = AtomicU64::new(0);

pub fn wakes() -> u64 {
    WAKES.load(Ordering::Relaxed)
}

/// A thread waits on the thermal level's, the power source's and the battery's charge's
/// notifications and calls `on` for each: `true` for the power source's or the charge's.
pub fn watch(on: impl Fn(bool) + Send + 'static) {
    let mut fds: Vec<c_int> = Vec::new();
    let mut powers: Vec<bool> = Vec::new();
    for name in [THERMAL, POWER, REMAINING] {
        let (mut fd, mut token): (c_int, c_int) = (-1, 0);
        // SAFETY: a NUL-terminated name; the fd and token are written on success.
        if unsafe { notify_register_file_descriptor(name.as_ptr(), &raw mut fd, 0, &raw mut token) } == 0 && fd >= 0 {
            fds.push(fd);
            powers.push(name != THERMAL);
        } else {
            tracing::info!(name = %name.to_string_lossy(), "not heard here: read at runs' starts and ends");
        }
    }
    if fds.is_empty() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-pace".into()).spawn(move || {
        loop {
            // SAFETY: notify(3)'s descriptors, open for the process's life (never cancelled).
            let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| unsafe { BorrowedFd::borrow_raw(*fd) }).collect();
            let mut polled: Vec<PollFd<'_>> = borrowed.iter().map(|fd| PollFd::new(*fd, PollFlags::POLLIN)).collect();
            match nix::poll::poll(&mut polled, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "the pace's notifications stopped: read at runs' starts and ends from now on");
                    return;
                }
            }
            WAKES.fetch_add(1, Ordering::Relaxed);
            let ready: Vec<bool> = polled.iter().map(|p| p.revents().is_some_and(|r| r.contains(PollFlags::POLLIN))).collect();
            drop(polled);
            // Each notification is its token, four bytes: taken, so the descriptor waits again.
            let mut power = false;
            for ((fd, r), is_power) in borrowed.iter().zip(ready).zip(&powers) {
                if r {
                    let mut token = [0u8; 4];
                    let _ = nix::unistd::read(fd, &mut token);
                    power |= *is_power;
                }
            }
            on(power);
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "the pace's notifications aren't heard: read at runs' starts and ends");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W05-R26: a quick check of the reads on this machine, with what each costs.
    #[test]
    fn w05_r26_reads_this_machine() {
        let mut r = Reader::new(Path::new("/"));
        let t = std::time::Instant::now();
        let a = r.read();
        let first = t.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let t = std::time::Instant::now();
        let b = r.read();
        let second = t.elapsed();
        let time = |f: &dyn Fn()| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_micros()
        };
        let costs = (time(&|| { let _ = std::hint::black_box(swapins()); }), time(&|| { let _ = std::hint::black_box(sysctl::<SwapUsage>(c"vm.swapusage")); }), time(&|| { let _ = std::hint::black_box(battery()); }), time(&|| { let _ = std::hint::black_box(sysctl::<LoadAvg>(c"vm.loadavg")); }));
        assert!(b.cpus > 0 && b.level.is_some() && b.runnable.is_some() && b.swapins.is_some(), "{b:?}");
        let mut kept = crate::sense::pace::Kept::default();
        let read = crate::sense::pace::between(&a, &b, 200_000, &mut kept);
        eprintln!("macos read: {} µs, then {} µs (swapins {} µs, swapusage {} µs, battery {} µs, loadavg {} µs); level {:?}, runnable {:?}, swap {:?}, battery {:?} {:?}; {read:?}", first.as_micros(), second.as_micros(), costs.0, costs.1, costs.2, costs.3, b.level, b.runnable, b.swap, b.on_battery, b.percent);
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
        watch(move |_| {
            c.fetch_add(1, Ordering::Relaxed);
        });
        std::thread::sleep(std::time::Duration::from_secs(60));
        eprintln!("macos idle minute: {} wakes, {} reads", wakes() - before, calls.load(Ordering::Relaxed));
    }
}
