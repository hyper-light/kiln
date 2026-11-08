//! Linux's reads for [`crate::sense::pace`] (W05-R26, W13-R01; docs/runners.md "Pace"), every one a
//! file any user may read:
//!
//! - pressure stall totals: `/proc/pressure/{cpu,memory,io}`, `some`'s `total` (psi.rst);
//! - per-CPU ticks (`/proc/stat`'s `cpuN` lines) and clocks (each CPU's cpufreq `scaling_cur_freq`
//!   against its `cpuinfo_max_freq`: none on a machine without cpufreq, a VM's often);
//! - swap: `SwapTotal` and `SwapFree` of `/proc/meminfo`, `pswpin` of `/proc/vmstat`, the runner
//!   cgroup's `memory.swap.current` where the memory controller is its;
//! - the battery: `/sys/class/power_supply` ([`crate::os::battery`]);
//! - the work device's counters ([`crate::os::sense_disk_linux`]).
//!
//! **Events** ([`watch`], a thread of its own blocked in poll(2)): a PSI trigger on each of
//! `/proc/pressure/{cpu,memory,io}`, which any user may arm since Linux 6.4 for a window that is a
//! whole number of 2 s (commit d82caa273565, "sched/psi: Allow unprivileged polling of N*2s
//! period"); `CAP_SYS_RESOURCE` may arm 500 ms. Its threshold is 1/1024 of the window: the least
//! stall that moves slowness by one of its units from none (d slowness / d share is 1 at no
//! stall). The kernel says it at most once a window while the stall lasts ("rate-limited to one
//! per tracking window"), so once it's quiet a window, the calm is read once more. And the
//! kernel's uevents of `power_supply` (NETLINK_KOBJECT_UEVENT, which any user may receive:
//! lib/kobject_uevent.c's `NL_CFG_F_NONROOT_RECV`): a charger in or out.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout};

use crate::sense::pace::{Raw, parse};

/// This machine's reads, with what they keep between reads: its CPUs' clock files and most
/// clocks, the runner cgroup's swap file, and the work device.
pub struct Reader {
    freq: Vec<(u32, PathBuf, u64)>,
    swap_own: Option<PathBuf>,
    device: Option<crate::os::sense_disk_linux::Device>,
}

fn number(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

impl Reader {
    pub fn new(work_dir: &Path) -> Reader {
        let mut freq = Vec::new();
        if let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu") {
            for e in dir.flatten() {
                let name = e.file_name();
                let Some(n) = name.to_str().and_then(|n| n.strip_prefix("cpu")).and_then(|n| n.parse::<u32>().ok()) else { continue };
                let f = e.path().join("cpufreq");
                if let Some(max) = number(&f.join("cpuinfo_max_freq")).filter(|m| *m > 0) {
                    freq.push((n, f.join("scaling_cur_freq"), max));
                }
            }
        }
        freq.sort_by_key(|(n, _, _)| *n);
        let swap_own = crate::os::cgroup::memory_dir().map(|d| d.join("memory.swap.current")).filter(|p| p.exists());
        Reader { freq, swap_own, device: crate::os::sense_disk_linux::Device::of(work_dir) }
    }

    pub fn read(&mut self) -> Raw {
        let text = |p: &str| std::fs::read_to_string(p).ok();
        let stall = |r: &str| text(&format!("/proc/pressure/{r}")).and_then(|t| crate::sense::parse::psi(&t)).and_then(|f| f.some).map(|l| l.total_us);
        let ticks = text("/proc/stat").map(|t| parse::per_cpu(&t)).unwrap_or_default();
        let clocks: BTreeMap<u32, (u64, u64)> = self.freq.iter().filter_map(|(n, cur, max)| Some((*n, (number(cur)?, *max)))).collect();
        let swap = text("/proc/meminfo").and_then(|m| {
            let total = crate::sense::parse::meminfo(&m, "SwapTotal")?;
            let free = crate::sense::parse::meminfo(&m, "SwapFree").unwrap_or(total);
            (total > 0).then(|| (total, total.saturating_sub(free)))
        });
        let battery = crate::os::battery::read();
        Raw {
            stall_us: [stall("cpu"), stall("memory"), stall("io")],
            cpus: u32::try_from(ticks.len()).unwrap_or(u32::MAX),
            ticks,
            clocks,
            swapins: text("/proc/vmstat").and_then(|t| parse::keyed(&t, "pswpin")),
            swap,
            swap_own: self.swap_own.as_deref().and_then(number),
            device: self.device.as_mut().and_then(crate::os::sense_disk_linux::Device::counters),
            on_battery: battery.as_ref().map(|b| !b.ac),
            percent: battery.map(|b| b.percent),
            ..Raw::default()
        }
    }
}

/// The shortest PSI window this process may arm, µs: 500 ms with `CAP_SYS_RESOURCE`, else 2 s
/// (psi.rst; Linux 6.4). `status` is `/proc/self/status`'s text.
pub fn window_us(status: &str) -> u64 {
    /// `CAP_SYS_RESOURCE`'s bit (linux/capability.h).
    const CAP_SYS_RESOURCE: u32 = 24;
    let effective = status.lines().find_map(|l| l.strip_prefix("CapEff:")).and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok()).unwrap_or(0);
    if (effective >> CAP_SYS_RESOURCE) & 1 == 1 { 500_000 } else { 2_000_000 }
}

/// A trigger of `window_us`: its threshold 1/1024 of the window, rounded up.
pub fn trigger(window_us: u64) -> String {
    format!("some {} {window_us}\0", window_us.div_ceil(u64::from(kiln_ci::pace::ONE)))
}

/// The OS's wakes of this runner's pace so far, whatever woke it.
static WAKES: AtomicU64 = AtomicU64::new(0);

pub fn wakes() -> u64 {
    WAKES.load(Ordering::Relaxed)
}

/// A thread waits on the kernel's events and calls `on` for each that bears on the pace: `true`
/// for a power supply's (its source or its charge: the kernel's power-supply uevents, which a
/// battery's driver sends as its status and capacity change).
pub fn watch(on: impl Fn(bool) + Send + 'static) {
    let window = window_us(&std::fs::read_to_string("/proc/self/status").unwrap_or_default());
    let line = trigger(window);
    let mut psi: Vec<std::fs::File> = Vec::new();
    for r in ["cpu", "memory", "io"] {
        let path = format!("/proc/pressure/{r}");
        match std::fs::OpenOptions::new().read(true).write(true).custom_flags(nix::libc::O_NONBLOCK).open(&path).and_then(|mut f| f.write_all(line.as_bytes()).map(|()| f)) {
            Ok(f) => psi.push(f),
            Err(e) => tracing::info!(error = %e, file = %path, "its stall isn't watched by a trigger here (Linux before 6.4 takes one only from CAP_SYS_RESOURCE): read at runs' starts and ends"),
        }
    }
    let uevent = uevents().map_err(|e| tracing::info!(error = %e, "power-supply changes aren't heard here: the battery is read at runs' starts and ends")).ok();
    if psi.is_empty() && uevent.is_none() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-pace".into()).spawn(move || {
        let mut quiet_at: Option<Instant> = None;
        let mut buf = vec![0u8; 8192];
        loop {
            let timeout = match quiet_at {
                Some(at) => PollTimeout::from(u16::try_from(at.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u16::MAX)),
                None => PollTimeout::NONE,
            };
            let mut fds: Vec<PollFd<'_>> = psi.iter().map(|f| PollFd::new(f.as_fd(), PollFlags::POLLPRI)).collect();
            if let Some(u) = &uevent {
                fds.push(PollFd::new(u.as_fd(), PollFlags::POLLIN));
            }
            let n = match nix::poll::poll(&mut fds, timeout) {
                Ok(n) => n,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "the pace's events stopped: read at runs' starts and ends from now on");
                    return;
                }
            };
            WAKES.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                // A window passed with no stall past the trigger: the calm, read once.
                quiet_at = None;
                on(false);
                continue;
            }
            let stalled = fds.iter().take(psi.len()).any(|f| f.revents().is_some_and(|r| r.contains(PollFlags::POLLPRI)));
            // A trigger the kernel let go of (its file says an error from now on) is let go of too.
            let broken: Vec<usize> = fds.iter().take(psi.len()).enumerate().filter(|(_, f)| f.revents().is_some_and(|r| r.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL))).map(|(i, _)| i).collect();
            let powered = fds.get(psi.len()).and_then(PollFd::revents).is_some_and(|r| r.contains(PollFlags::POLLIN));
            drop(fds);
            for i in broken.into_iter().rev() {
                psi.remove(i);
            }
            let mut power = false;
            if powered && let Some(u) = &uevent {
                while let Ok(len) = nix::sys::socket::recv(u.as_raw_fd(), &mut buf, nix::sys::socket::MsgFlags::MSG_DONTWAIT) {
                    if len == 0 {
                        break;
                    }
                    const POWER: &[u8] = b"SUBSYSTEM=power_supply";
                    power |= buf.get(..len).is_some_and(|m| m.windows(POWER.len()).any(|w| w == POWER));
                }
            }
            if stalled {
                quiet_at = Some(Instant::now() + Duration::from_micros(window));
            }
            if stalled || power {
                on(power);
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "the pace's events aren't watched: read at runs' starts and ends");
    }
}

/// The kernel's uevents, as any user may hear them.
fn uevents() -> nix::Result<OwnedFd> {
    use nix::sys::socket::{AddressFamily, NetlinkAddr, SockFlag, SockProtocol, SockType, bind, socket};
    let fd = socket(AddressFamily::Netlink, SockType::Datagram, SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK, SockProtocol::NetlinkKObjectUEvent)?;
    bind(fd.as_raw_fd(), &NetlinkAddr::new(0, 1))?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W05-R26: the window is the least the kernel takes from this process, and the trigger's
    /// threshold is 1/1024 of it.
    #[test]
    fn w05_r26_trigger_is_a_unit_of_slowness() {
        assert_eq!(window_us("CapEff:\t0000000000000000\n"), 2_000_000);
        assert_eq!(window_us("CapEff:\t0000000001000000\n"), 500_000);
        assert_eq!(trigger(2_000_000), "some 1954 2000000\0");
        assert_eq!(trigger(500_000), "some 489 500000\0");
    }

    /// W05-R26: a quick check of the reads on this machine, with what each costs (µs, the mean of
    /// `n` reads), and what the event thread's wakes cost: `cargo test -p kiln --lib
    /// os::pace::tests -- --nocapture`.
    #[test]
    fn w05_r26_reads_this_machine() {
        let n = 50u32;
        let cost = |f: &mut dyn FnMut()| {
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            t.elapsed().as_secs_f64() * 1e6 / f64::from(n)
        };
        let mut r = Reader::new(Path::new("/"));
        let psi = cost(&mut || { let _ = std::hint::black_box(["cpu", "memory", "io"].map(|x| std::fs::read_to_string(format!("/proc/pressure/{x}")))); });
        let freq = r.freq.clone();
        let clocks = cost(&mut || {
            let _ = std::hint::black_box(std::fs::read_to_string("/proc/stat").map(|t| parse::per_cpu(&t)));
            let _ = std::hint::black_box(freq.iter().map(|(_, p, _)| number(p)).collect::<Vec<_>>());
        });
        let swap = cost(&mut || {
            let _ = std::hint::black_box(std::fs::read_to_string("/proc/meminfo"));
            let _ = std::hint::black_box(std::fs::read_to_string("/proc/vmstat"));
        });
        let battery = cost(&mut || { let _ = std::hint::black_box(crate::os::battery::read()); });
        let mut dev = crate::os::sense_disk_linux::Device::of(Path::new("/"));
        let disk = cost(&mut || { let _ = std::hint::black_box(dev.as_mut().and_then(crate::os::sense_disk_linux::Device::counters)); });
        let a = r.read();
        let whole = cost(&mut || { let _ = std::hint::black_box(r.read()); });
        std::thread::sleep(Duration::from_millis(200));
        let b = r.read();
        let mut kept = crate::sense::pace::Kept::default();
        let wake = cost(&mut || {
            let read = crate::sense::pace::between(&a, &b, 200_000, &mut kept.clone());
            let w = kept.learner.weights();
            let _ = std::hint::black_box((w.slowness(&read.signals), w.speed_now(1024, &read.signals, None)));
        });
        assert!(b.cpus > 0 && !b.ticks.is_empty(), "{b:?}");
        let read = crate::sense::pace::between(&a, &b, 200_000, &mut kept);
        eprintln!("linux µs per read: pressure {psi:.0}, clocks ({} CPUs' cpufreq and /proc/stat) {clocks:.0}, swap {swap:.0}, battery {battery:.0}, disk {disk:.0}; whole read {whole:.0}; a wake's arithmetic {wake:.1}", freq.len());
        eprintln!("linux read: swap {:?}, swapins {:?}, stalls {:?}, battery {:?}; {read:?}", b.swap, b.swapins, b.stall_us, b.on_battery);
    }

    /// W05-R26: an idle runner's event thread sleeps: its wakes over a minute, with its triggers
    /// armed (`KILN_PACE_IDLE=1`; it takes the minute).
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
        std::thread::sleep(Duration::from_secs(60));
        let stall = |r: &str| std::fs::read_to_string(format!("/proc/pressure/{r}")).unwrap_or_default().lines().next().unwrap_or_default().to_owned();
        eprintln!("linux idle minute: {} wakes, {} reads; the machine's pressure: cpu {}, memory {}, io {}", wakes() - before, calls.load(Ordering::Relaxed), stall("cpu"), stall("memory"), stall("io"));
    }
}
