//! macOS's reads for [`crate::sense::pace`] (W05-R26, W13-R01; docs/runners.md "Pace"), each open to
//! any user:
//!
//! - the thermal pressure level: notify(3)'s `com.apple.system.thermalpressurelevel` state
//!   (`OSThermalNotification.h`: nominal, moderate, heavy, trapping, sleeping; ProcessInfo's
//!   `thermalState` is the same levels as nominal, fair, serious and critical). macOS gives no
//!   unprivileged CPU clock (`powermetrics` needs root);
//! - the run queue: the 1-minute load average (`vm.loadavg`, the scheduler's exponentially decayed
//!   count of runnable threads);
//! - swap: `vm.swapusage` and the pages swapped in from the compressor's swap files, read with the
//!   host's pages ([`kiln_ci::facts::VmPages`]): live where the host keeps the count
//!   (`vm.compressor.swapper.swapins_total`), else `HOST_VM_INFO64`'s, none for a copy said before;
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

use crate::os::sense::{cf, sysctl};
use crate::sense::pace::Raw;

const THERMAL: &CStr = c"com.apple.system.thermalpressurelevel";
const POWER: &CStr = c"com.apple.system.powersources.source";
/// `kIOPSNotifyTimeRemaining` (IOPowerSources.h): posted "when percent remaining or time remaining
/// changes", and as the source does: the battery's charge as it moves.
const REMAINING: &CStr = c"com.apple.system.powersources.timeremaining";

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

/// Pages swapped in so far, from the one reading of the host's VM statistics
/// ([`kiln_ci::facts::VmPages`], [`crate::os::sense::memory_pages`]): the host's live count where it
/// keeps one, else the count of XNU's copy, with that copy.
fn swapins() -> Option<(u64, Option<u64>)> {
    crate::os::sense::memory_pages().map(|(pages, _)| (pages.swapins, (!pages.swapins_live).then_some(pages.copy)))
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

/// This machine's reads: the thermal level's token, the work device, and the copy of XNU's VM statistics
/// its last swap-in count was of.
pub struct Reader {
    thermal: Option<c_int>,
    device: Option<crate::os::sense_disk_macos::Device>,
    copy: Option<u64>,
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let mut token: c_int = 0;
        // SAFETY: a NUL-terminated name, a token written on success.
        let ok = unsafe { notify_register_check(THERMAL.as_ptr(), &raw mut token) } == 0;
        Reader { thermal: ok.then_some(token), device: crate::os::sense_disk_macos::Device::of(work_dir), copy: None }
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
            // A copy's count it said before says nothing of the time since: none then, never a rate of zero
            // (`kiln_ci::facts::VmPages::copy`).
            swapins: swapins().and_then(|(n, copy)| match copy {
                Some(c) => (self.copy.replace(c) != Some(c)).then_some(n),
                None => Some(n),
            }),
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
        // A reader's first count is always said; a later one is none where it would be a copy of XNU's
        // said before (another program, or a test beside this, read the host's VM statistics meanwhile).
        assert!(b.cpus > 0 && b.level.is_some() && b.runnable.is_some() && a.swapins.is_some(), "{a:?} then {b:?}");
        let mut kept = crate::sense::pace::Kept::default();
        let read = crate::sense::pace::between(&a, &b, 200_000, &mut kept);
        eprintln!("macos read: {} µs, then {} µs (swapins {} µs, swapusage {} µs, battery {} µs, loadavg {} µs); level {:?}, runnable {:?}, swap {:?}, battery {:?} {:?}; {read:?}", first.as_micros(), second.as_micros(), costs.0, costs.1, costs.2, costs.3, b.level, b.runnable, b.swap, b.on_battery, b.percent);
    }

    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        static mach_task_self_: u32;
    }

    /// The host's live count of pages swapped in, where it keeps one (`vm.compressor.swapper.swapins_total`:
    /// `vm_statistics_swapins`, the counter `HOST_VM_INFO64`'s `swapins` is read from, bsd/vm/vm_unix.c and
    /// osfmk/kern/host.c); none where it doesn't (macOS 26.2 has none).
    fn live_swapins() -> Option<u64> {
        sysctl::<u64>(c"vm.compressor.swapper.swapins_total")
    }

    /// Stops the other reader as it drops, however the test leaves its scope.
    struct Stop<'a>(&'a std::sync::atomic::AtomicBool);

    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// W05-R26: the swap-ins a Mac's pace reads while another program reads the host's VM statistics
    /// through `host_statistics64(HOST_VM_INFO64)`, as Macs' monitors and other kiln processes do. XNU
    /// answers that call from one copy every process that isn't Apple's shares, fresh for the first 2 to
    /// 10 calls of each second and that copy for the rest of it (`rate_limit_host_statistics`,
    /// osfmk/kern/host.c, xnu-12377.121.6): a read inside the window of the read before had that copy's
    /// count again, and the swap-ins a second between them read zero whatever the host swapped in. Each
    /// read's count is the host's own now (its live count, where it keeps one), or a copy's the host made
    /// since the read before (the other reader sees each copy as it's made), or none.
    #[test]
    fn w05_r26_a_swap_in_count_is_never_a_copy_said_again() -> anyhow::Result<()> {
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;
        let (stop, calls) = (AtomicBool::new(false), AtomicU64::new(0));
        let made: parking_lot::Mutex<Vec<Instant>> = parking_lot::Mutex::new(Vec::new());
        let reads = std::thread::scope(|s| -> anyhow::Result<Vec<(Instant, Raw, Instant, Option<u64>, Option<u64>)>> {
            let _stop = Stop(&stop);
            std::thread::Builder::new().name("other-reader".into()).spawn_scoped(s, || {
                // SAFETY: no arguments; a send right to this host, released below.
                let host = unsafe { mach_host_self() };
                let mut last = None;
                while !stop.load(Ordering::Relaxed) {
                    // SAFETY: plain integers, for which zero is a value.
                    let mut v: nix::libc::vm_statistics64 = unsafe { std::mem::zeroed() };
                    let mut count = nix::libc::HOST_VM_INFO64_COUNT;
                    // SAFETY: `v` is a whole `vm_statistics64` and `count` its length in `integer_t`s.
                    let r = unsafe { nix::libc::host_statistics64(host, nix::libc::HOST_VM_INFO64, (&raw mut v).cast(), &raw mut count) };
                    let counts = (v.faults, v.zero_fill_count, v.lookups, v.cow_faults, v.pageins, v.free_count, v.active_count, v.inactive_count, v.swapins);
                    if r == 0 && last.replace(counts) != Some(counts) {
                        made.lock().push(Instant::now());
                    }
                    calls.fetch_add(1, Ordering::Relaxed);
                }
                // SAFETY: the send right taken above, released once.
                unsafe {
                    mach_port_deallocate(mach_task_self_, host);
                }
            })?;
            // Past the most calls one of XNU's windows answers fresh (`HOST_STATISTICS_MAX_REQUESTS`).
            while calls.load(Ordering::Relaxed) <= 10 {
                std::thread::yield_now();
            }
            // Three reads one after another: within one of XNU's one-second windows, or across one boundary
            // at most, so one pair at least has no new copy between them.
            let mut r = Reader::new(Path::new("/"));
            let mut out = Vec::new();
            for _ in 0..3 {
                let before = live_swapins();
                let began = Instant::now();
                let raw = r.read();
                let ended = Instant::now();
                out.push((began, raw, ended, before, live_swapins()));
            }
            Ok(out)
        })?;
        let made = made.into_inner();
        eprintln!("macos swap-ins: the other reader called {} times and saw {} copies; live count {:?}", calls.into_inner(), made.len(), live_swapins());
        for pair in reads.windows(2) {
            let [(began, _, _, _, _), (_, raw, ended, before, after)] = pair else { continue };
            let Some(n) = raw.swapins else { continue };
            match (before, after) {
                (Some(b), Some(a)) => assert!((*b..=*a).contains(&n), "{n} isn't the host's live count, {b} to {a}"),
                _ => assert!(made.iter().any(|t| t > began && t <= ended), "{n}: a copy said again, none made since the read before"),
            }
        }
        Ok(())
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
