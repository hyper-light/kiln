//! The CPU's own counters on one thread, by perf_event_open(2): cycles, instructions, cache
//! references and misses as the kernel maps them on this CPU, L1 data and last-level cache read
//! misses, and data-TLB read misses. User space only (`exclude_kernel`, `exclude_hv`): what an
//! unprivileged process may count of its own threads where `perf_event_paranoid` is 2. The
//! publisher's and each subscriber's thread opens its own when it takes its role, so a profile
//! counts those threads and no other (the publisher's pacing spin among its own). Each is read with
//! the times it was enabled and running, so a count the PMU multiplexed is scaled; one the kernel
//! refuses says why, one that never ran says so: never a zero in its place. On a hybrid CPU (Intel's
//! `cpu_core` and `cpu_atom`) each event is opened on each core type the thread may run on and
//! summed, since a generic event counts on one type alone; one a type it may run on refuses is
//! refused. Linux; elsewhere every count is refused, naming the platform.
//!
//! perf_event_open and read need `unsafe` (CLAUDE.md §12).
#![allow(unsafe_code)]

/// One counter.
pub struct Event {
    pub key: &'static str,
    pub label: &'static str,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    kind: u32,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    config: u64,
}

/// `PERF_TYPE_HARDWARE` and `PERF_TYPE_HW_CACHE` (`<linux/perf_event.h>`).
const HARDWARE: u32 = 0;
const CACHE: u32 = 3;
/// A cache event's configuration: the cache, a read (0), and its misses (1 << 16).
const fn read_misses(cache: u64) -> u64 {
    cache | (1 << 16)
}

pub const EVENTS: [Event; 7] = [
    Event {
        key: "cycles",
        label: "cycles",
        kind: HARDWARE,
        config: 0,
    },
    Event {
        key: "instructions",
        label: "instructions",
        kind: HARDWARE,
        config: 1,
    },
    Event {
        key: "cache_references",
        label: "cache references",
        kind: HARDWARE,
        config: 2,
    },
    Event {
        key: "cache_misses",
        label: "cache misses",
        kind: HARDWARE,
        config: 3,
    },
    Event {
        key: "l1d_read_misses",
        label: "L1 data cache read misses",
        kind: CACHE,
        config: read_misses(0),
    },
    Event {
        key: "llc_read_misses",
        label: "last-level cache read misses",
        kind: CACHE,
        config: read_misses(2),
    },
    Event {
        key: "dtlb_read_misses",
        label: "data TLB read misses",
        kind: CACHE,
        config: read_misses(3),
    },
];

/// One read of a counter: its count and how long it was enabled and running, nanoseconds.
#[derive(Clone, Copy, Default, Debug)]
pub struct Read {
    pub value: u64,
    pub enabled: u64,
    pub running: u64,
}

/// One thread's reading: per event, per core type, or why the kernel refused it.
pub type Reading = Vec<Result<Vec<Read>, String>>;

/// The count between two readings, scaled where the PMU shared its counters: `None` if it never
/// ran.
pub fn between(before: &[Read], after: &[Read]) -> Option<f64> {
    let mut total = 0.0;
    let mut ran = false;
    for (i, a) in after.iter().enumerate() {
        let b = before.get(i).copied().unwrap_or_default();
        let running = a.running.saturating_sub(b.running);
        if running == 0 {
            continue;
        }
        ran = true;
        let enabled = a.enabled.saturating_sub(b.enabled);
        total += a.value.saturating_sub(b.value) as f64 * enabled as f64 / running as f64;
    }
    ran.then_some(total)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    use super::{EVENTS, Read, Reading};

    /// `struct perf_event_attr` to its first version's end (`PERF_ATTR_SIZE_VER0`, 64 bytes); the
    /// kernel takes the rest as zero.
    #[repr(C)]
    #[derive(Default)]
    struct Attr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    const EXCLUDE_KERNEL: u64 = 1 << 5;
    const EXCLUDE_HV: u64 = 1 << 6;
    /// `PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_TOTAL_TIME_RUNNING`.
    const TIMES: u64 = 1 | 2;
    const FD_CLOEXEC: libc::c_ulong = 8;
    /// Where a hybrid CPU's core type goes in a generic event's configuration
    /// (`PERF_PMU_TYPE_SHIFT`).
    const PMU_SHIFT: u32 = 32;

    /// The CPUs a cpulist names (`0-7,16-23`).
    fn cpulist(text: &str) -> Vec<u32> {
        text.trim()
            .split(',')
            .filter(|r| !r.is_empty())
            .flat_map(|r| {
                let (a, b) = r.split_once('-').unwrap_or((r, r));
                match (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    (Ok(a), Ok(b)) if a <= b => (a..=b).collect(),
                    _ => Vec::new(),
                }
            })
            .collect()
    }

    /// The core types to open each event on: those of a hybrid CPU this thread may run on (its
    /// `Cpus_allowed_list`, as `taskset` left it), by their PMU's type and name; or the one PMU.
    fn pmus() -> Vec<(Option<u64>, &'static str)> {
        let allowed = std::fs::read_to_string("/proc/thread-self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                    .map(cpulist)
            });
        let hybrid: Vec<(Option<u64>, &'static str)> = ["cpu_core", "cpu_atom"]
            .into_iter()
            .filter_map(|pmu| {
                let dir = format!("/sys/bus/event_source/devices/{pmu}");
                let kind = std::fs::read_to_string(format!("{dir}/type"))
                    .ok()?
                    .trim()
                    .parse()
                    .ok()?;
                let cpus = std::fs::read_to_string(format!("{dir}/cpus"))
                    .ok()
                    .map(|c| cpulist(&c));
                let reachable = match (&allowed, &cpus) {
                    (Some(allowed), Some(cpus)) => cpus.iter().any(|c| allowed.contains(c)),
                    _ => true,
                };
                reachable.then_some((Some(kind), pmu))
            })
            .collect();
        if hybrid.is_empty() {
            vec![(None, "cpu")]
        } else {
            hybrid
        }
    }

    fn paranoid() -> String {
        std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .map_or_else(|_| "unknown".to_owned(), |p| p.trim().to_owned())
    }

    /// Why the kernel refused an event on `pmu`, from the error perf_event_open left.
    fn refusal(pmu: &str) -> String {
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOENT | libc::EOPNOTSUPP | libc::ENODEV | libc::EINVAL) => {
                format!("refused: {pmu}'s PMU has no such event ({e})")
            }
            Some(libc::EACCES | libc::EPERM) => format!(
                "refused: not permitted on {pmu}, perf_event_paranoid {} ({e})",
                paranoid()
            ),
            _ => format!("refused: on {pmu}, {e}"),
        }
    }

    fn open(kind: u32, config: u64, pmu: &str) -> Result<OwnedFd, String> {
        let attr = Attr {
            kind,
            size: u32::try_from(size_of::<Attr>()).unwrap_or(64),
            config,
            read_format: TIMES,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV,
            ..Attr::default()
        };
        // The calling thread, on any CPU, in no group.
        let (thread, cpu, group): (libc::pid_t, libc::c_int, libc::c_int) = (0, -1, -1);
        // SAFETY: `attr` is a valid `perf_event_attr` of the size it states, which outlives the
        // call, and the other arguments are perf_event_open's own types.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &raw const attr,
                thread,
                cpu,
                group,
                FD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(refusal(pmu));
        }
        let fd = libc::c_int::try_from(fd).map_err(|_| "refused: a descriptor out of range")?;
        // SAFETY: the kernel just gave this descriptor, and nothing else holds it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The calling thread's counters: per event, one per core type.
    pub struct Counters(Vec<Result<Vec<OwnedFd>, String>>);

    impl Counters {
        pub fn open() -> Self {
            let pmus = pmus();
            Self(
                EVENTS
                    .iter()
                    .map(|e| {
                        pmus.iter()
                            .map(|&(kind, name)| {
                                let config = kind.map_or(e.config, |t| e.config | (t << PMU_SHIFT));
                                open(e.kind, config, name)
                            })
                            .collect()
                    })
                    .collect(),
            )
        }

        pub fn read(&self) -> Reading {
            self.0
                .iter()
                .map(|fds| {
                    fds.as_ref().map_err(Clone::clone).and_then(|fds| {
                        fds.iter()
                            .map(|fd| {
                                let mut v = [0u64; 3];
                                // SAFETY: `v` is 24 writable bytes, what a read of a counter opened
                                // with both times gives, from a descriptor this owns.
                                let n = unsafe {
                                    libc::read(
                                        fd.as_raw_fd(),
                                        v.as_mut_ptr().cast(),
                                        size_of_val(&v),
                                    )
                                };
                                if usize::try_from(n).ok() == Some(size_of_val(&v)) {
                                    Ok(Read {
                                        value: v[0],
                                        enabled: v[1],
                                        running: v[2],
                                    })
                                } else {
                                    Err(format!("unreadable: {}", std::io::Error::last_os_error()))
                                }
                            })
                            .collect()
                    })
                })
                .collect()
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::Counters;

/// No counters off Linux: every one refused, naming the platform.
#[cfg(not(target_os = "linux"))]
pub struct Counters;

#[cfg(not(target_os = "linux"))]
impl Counters {
    pub fn open() -> Self {
        Self
    }

    pub fn read(&self) -> Reading {
        EVENTS
            .iter()
            .map(|_| Err("refused: perf_event_open is Linux's".to_owned()))
            .collect()
    }
}
