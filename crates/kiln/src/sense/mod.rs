//! What this runner can take, measured as it changes (docs/needs.md; W05, W13-R01).
//!
//! A runner owns its capacity: it decides admission itself, from its own measurements, the last
//! one read when its machine changed. [`Sensor::measure`] is that read. It takes no timer and keeps no
//! history beyond the counters of its previous read: each figure that changes is an exact average
//! over the interval since then (cumulative counters subtracted), whatever that interval was, and
//! each figure that is a level (memory available, free space) is the level now.
//!
//! **The place core's runner admission calls [`Sensor::measure`] on events, never when it
//! decides** (W05-R26): a run's start or end here, the OS's pressure, power and thermal events
//! ([`pace`]'s), and the host sample's arrival; a decision (a request to place, a leg offered)
//! reads the last measure, so it costs no read. Nothing else measures on its behalf: the runner's
//! 5-second [`crate::host::Sampler`]'s figures feed its page and the server's charts, and placement
//! takes none of them.
//! A caller that wants its own window (say, the last second before an admission) reads once at
//! the window's start and again at its end; the second read's figures cover just that window.
//!
//! It costs microseconds. On Linux a read is a handful of small procfs/sysfs files, each read
//! into a stack buffer no bigger than the lines it needs (the first line of `/proc/stat`, the
//! first lines of `/proc/meminfo`, the three pressure files and the runner's own cgroup's), one
//! `statvfs`, and the work volume's device counters (one line of `/proc/diskstats`). macOS: two
//! Mach calls, one `statfs`, one `sysctl`, one Core Foundation property and the device's IOKit
//! `Statistics`. Windows: `GetSystemTimes`, `GlobalMemoryStatusEx`, `GetDiskFreeSpaceExW` and one
//! `IOCTL_DISK_PERFORMANCE`. No process is enumerated, nothing is started.
//!
//! The per-OS reads are in `crate::os::sense`; this module turns two of them into a [`Measure`]
//! (pure: [`Measure::between`]), so the arithmetic is tested on every platform.

pub mod disk;
pub mod health;
pub mod pace;
pub mod run;

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

pub use crate::metrics::MemoryLevel;

/// Cumulative CPU time, all CPUs together, in the OS's own unit (Linux's clock ticks, Mach's
/// ticks, Windows' 100 ns): only ratios of its deltas are used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTicks {
    /// Time the CPUs were idle (Linux: `idle` and `iowait`, a CPU waiting on I/O being free to run
    /// anything else).
    pub idle: u64,
    /// All time, idle included.
    pub total: u64,
}

/// One line of a pressure stall file (Linux's PSI): its 10-second average in percent as the
/// kernel keeps it, and its cumulative stall time in microseconds, from which any interval's
/// share is exact.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PsiLine {
    pub avg10: f64,
    pub total_us: u64,
}

/// A pressure stall file's two lines (`some`: at least one task stalled; `full`: all non-idle
/// tasks at once). A kernel without `full` for CPU (before 5.13) leaves it out.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PsiFile {
    pub some: Option<PsiLine>,
    pub full: Option<PsiLine>,
}

/// Linux's three pressure files, for the machine (`/proc/pressure/*`) or a cgroup (`*.pressure`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PsiRaw {
    pub cpu: Option<PsiFile>,
    pub memory: Option<PsiFile>,
    pub io: Option<PsiFile>,
}

/// The runner's own cgroup, as read now (Linux, cgroup v2; none for a runner in the root cgroup).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CgroupRaw {
    /// Its path under the cgroup mount (`/user.slice/…/kiln.scope`).
    pub path: String,
    pub psi: PsiRaw,
    /// The least `memory.max` of it and the cgroups above it.
    pub mem_limit: Option<u64>,
    /// `memory.current` less `inactive_file`, the working set (as [`crate::os::cgroup`] counts it).
    pub mem_used: Option<u64>,
    /// The CPUs its least `cpu.max` allows (quota over period).
    pub cpu_limit: Option<f64>,
    /// `cpu.stat`'s `usage_usec`: CPU time its tasks used, cumulative.
    pub cpu_usage_us: Option<u64>,
}

/// The work directory's file system, as read now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Space {
    pub total: u64,
    /// What this user may still write (`f_bavail`, `lpFreeBytesAvailableToCaller`).
    pub free: u64,
    /// macOS (APFS): what the system says can be had for important use
    /// (`volumeAvailableCapacityForImportantUsage`), purgeable space it would free for it
    /// included; at least `free`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub important: Option<u64>,
}

/// One read of everything [`Measure`] is made of. Counters are cumulative; the rest are levels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Raw {
    pub cpu: Option<CpuTicks>,
    pub cpus: u32,
    pub mem_total: u64,
    pub mem_available: u64,
    pub space: Option<Space>,
    pub psi: PsiRaw,
    pub cgroup: Option<CgroupRaw>,
    pub memory_level: Option<MemoryLevel>,
    /// The work volume's device's counters, as [`disk::Counters`] (step 2, docs/needs.md).
    pub device: Option<disk::Counters>,
    /// kiln's other services beside the runner (`kiln*.service` next to its own cgroup), each
    /// with its pressure files: what the gate's guard watches (`ci/gate.sh`).
    pub services: Vec<(String, PsiRaw)>,
}

/// A stall's shares over the interval, each 0 to 1: `some` (at least one task stalled), `full`
/// (all non-idle tasks at once, so no task advanced).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub some: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full: Option<f64>,
}

/// Linux's pressure stall information over the interval, for CPU, memory and I/O.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pressure {
    pub cpu: Stall,
    pub memory: Stall,
    pub io: Stall,
}

impl Pressure {
    /// No figure at all: an OS without stall information.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One of kiln's services beside the runner, and its pressure over the interval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Service {
    pub name: String,
    pub pressure: Pressure,
}

/// The runner's own cgroup over the interval.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cgroup {
    pub path: String,
    pub pressure: Pressure,
    /// The most memory it may hold, and what it may still take (`limit - used`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available: Option<u64>,
    /// The CPUs its quota allows, and the CPUs its tasks used on average over the interval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_used: Option<f64>,
}

/// What this runner can take, measured now (docs/needs.md). Every share is 0 to 1 and covers
/// exactly [`Measure::interval_us`]; every level is as read at its end.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Measure {
    /// The interval the shares cover: since the previous read, in microseconds (monotonic clock).
    pub interval_us: u64,
    /// Logical CPUs this process may run on.
    pub cpus: u32,
    /// The share of all CPUs' time that was idle over the interval; none when the OS's CPU
    /// counters didn't move (an interval shorter than one of their ticks) or can't be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_idle: Option<f64>,
    pub mem_total: u64,
    /// Memory available to start new work without swapping (Linux `MemAvailable`; macOS its free
    /// pages and the inactive ones beyond what its compressor holds,
    /// [`kiln_ci::facts::VmPages::room`]; Windows `ullAvailPhys`), no more than the runner's cgroup
    /// still allows.
    pub mem_available: u64,
    /// The work directory's file system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<Space>,
    /// The machine's pressure stall information (Linux); empty elsewhere.
    pub pressure: Pressure,
    /// The runner's own cgroup (Linux, when it has one of its own).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<Cgroup>,
    /// macOS's memory pressure level (`kern.memorystatus_vm_pressure_level`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_level: Option<MemoryLevel>,
    /// The work volume's device over the interval: throughput, latency, queue and errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<disk::Activity>,
    /// kiln's other services beside the runner, each with its pressure over the interval (Linux).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<Service>,
    /// The most bytes per second (read and written) the device was seen to move over any
    /// interval this sensor measured: a lower bound on what it can, from the I/O the machine did
    /// (passive: busy time says nothing of a parallel device's capacity, research-disk.md).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_peak_bps: Option<f64>,
}

/// `later - earlier` of a cumulative counter as a share of `span`, 0 to 1; none without both, a
/// counter that went back (a reset) or an empty span.
fn share(earlier: Option<u64>, later: Option<u64>, span: u64) -> Option<f64> {
    let (e, l) = (earlier?, later?);
    let d = l.checked_sub(e)?;
    (span > 0).then(|| (d as f64 / span as f64).clamp(0.0, 1.0))
}

/// A PSI line's share over `span_us`: its stall time's delta when there is an earlier read,
/// else the kernel's own 10-second average.
fn stall_line(earlier: Option<PsiLine>, later: Option<PsiLine>, span_us: u64) -> Option<f64> {
    let l = later?;
    share(earlier.map(|e| e.total_us), Some(l.total_us), span_us).or_else(|| (l.avg10.is_finite() && l.avg10 >= 0.0).then(|| (l.avg10 / 100.0).clamp(0.0, 1.0)))
}

fn stall(earlier: Option<PsiFile>, later: Option<PsiFile>, span_us: u64) -> Stall {
    let (e, l) = (earlier.unwrap_or_default(), later.unwrap_or_default());
    Stall { some: stall_line(e.some, l.some, span_us), full: stall_line(e.full, l.full, span_us) }
}

fn pressure(earlier: &PsiRaw, later: &PsiRaw, span_us: u64) -> Pressure {
    Pressure { cpu: stall(earlier.cpu, later.cpu, span_us), memory: stall(earlier.memory, later.memory, span_us), io: stall(earlier.io, later.io, span_us) }
}

impl Measure {
    /// The measure of the interval from `earlier` to `later`, `interval_us` long. Pure.
    pub fn between(earlier: &Raw, later: &Raw, interval_us: u64) -> Measure {
        let cpu_idle = match (earlier.cpu, later.cpu) {
            (Some(e), Some(l)) => share(Some(e.idle), Some(l.idle), l.total.saturating_sub(e.total)).filter(|_| l.total >= e.total),
            _ => None,
        };
        let cgroup = later.cgroup.as_ref().map(|c| {
            // An earlier read of the same cgroup, if the runner wasn't moved in between.
            let before = earlier.cgroup.as_ref().filter(|e| e.path == c.path);
            let mem_available = match (c.mem_limit, c.mem_used) {
                (Some(limit), Some(used)) => Some(limit.saturating_sub(used)),
                _ => None,
            };
            let cpu_used = match (before.and_then(|b| b.cpu_usage_us), c.cpu_usage_us) {
                (Some(b), Some(n)) if n >= b && interval_us > 0 => Some((n - b) as f64 / interval_us as f64),
                _ => None,
            };
            Cgroup {
                path: c.path.clone(),
                pressure: pressure(&before.map(|b| b.psi).unwrap_or_default(), &c.psi, interval_us),
                mem_limit: c.mem_limit,
                mem_available,
                cpu_limit: c.cpu_limit,
                cpu_used,
            }
        });
        let mem_available = cgroup.as_ref().and_then(|c| c.mem_available).map_or(later.mem_available, |room| room.min(later.mem_available));
        let device = match (&earlier.device, &later.device) {
            (Some(e), Some(l)) => disk::Activity::between(e, l, interval_us),
            _ => None,
        };
        Measure {
            interval_us,
            cpus: later.cpus,
            cpu_idle,
            mem_total: later.mem_total,
            mem_available,
            space: later.space,
            pressure: pressure(&earlier.psi, &later.psi, interval_us),
            cgroup,
            memory_level: later.memory_level,
            device,
            services: later.services.iter().map(|(name, p)| Service { name: name.clone(), pressure: pressure(&earlier.services.iter().find(|(n, _)| n == name).map(|(_, e)| *e).unwrap_or_default(), p, interval_us) }).collect(),
            device_peak_bps: None,
        }
    }

    /// The runner's budget as a host sample says it (AUDIT M6, E6): its CPUs and memory as its
    /// cgroup bounds them, not the machine's, and the share of those CPUs in use over the
    /// interval (none idle said: none in use).
    pub fn budget(&self) -> Budget {
        let quota = self.cgroup.as_ref().and_then(|c| c.cpu_limit).filter(|q| *q > 0.0);
        let all = quota.map_or(f64::from(self.cpus), |q| q.min(f64::from(self.cpus)));
        let cpus = u32::try_from(all.ceil() as u64).unwrap_or(u32::MAX).max(1);
        let busy = match self.idle_cpus() {
            Some(idle) if all > 0.0 => (1.0 - idle / all).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let mem_total = self.cgroup.as_ref().and_then(|c| c.mem_limit).map_or(self.mem_total, |l| l.min(self.mem_total));
        Budget { cpus, busy, mem_total, mem_used: mem_total.saturating_sub(self.mem_available.min(mem_total)) }
    }

    /// Idle CPUs over the interval: CPUs times their idle share, no more than the runner's
    /// cgroup quota leaves (its limit less what its tasks used).
    pub fn idle_cpus(&self) -> Option<f64> {
        let machine = self.cpu_idle.map(|s| s * f64::from(self.cpus))?;
        let quota = self.cgroup.as_ref().and_then(|c| Some((c.cpu_limit? - c.cpu_used.unwrap_or(0.0)).max(0.0)));
        Some(quota.map_or(machine, |q| q.min(machine)))
    }
}

/// The runner's CPUs and memory as its cgroup bounds them ([`Measure::budget`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Budget {
    pub cpus: u32,
    /// The share of `cpus` in use over the interval, 0 to 1.
    pub busy: f64,
    pub mem_total: u64,
    pub mem_used: u64,
}

/// The runner's sensor: the previous read, and what the OS needs kept between reads (the work
/// volume's device, found once). The work volume's static facts (its file system, its devices'
/// kinds and health) are [`storage`]'s, gathered with the runner's other facts.
pub struct Sensor {
    reader: crate::os::sense::Reader,
    last: Raw,
    at: Instant,
    peak_bps: Option<f64>,
    /// The read when the load it measures last changed (placed work started or ended), and when:
    /// strain is measured from there ([`window`]).
    anchor: (Raw, Instant),
    /// Its machine changed since its last read, so a measure is due ([`Sensor::changed`]); and its
    /// load did, so that read is the anchor too ([`Sensor::anchor`]).
    due: bool,
    anchor_due: bool,
    /// The last measure's pressure over that window.
    strain: Option<Measure>,
    /// How many times it read the machine (W05-R26: a placement decision reads it none).
    reads: u64,
}

/// The kernel's shortest pressure average, `avg10` (Documentation/accounting/psi.rst: the
/// averages are over 10, 60 and 300 second windows): also the statistic the gate's guard yields on
/// (`ci/gate.sh`). Not picked: the kernel's and the guard's.
pub const PSI_SHORTEST: std::time::Duration = std::time::Duration::from_secs(10);

/// The pressure a placement decision weighs strain by (AUDIT L1, with no picked smoothing): over
/// the window since the load on the machine last changed (`anchor`, `age` ago; every stall since
/// then is the current load's, none the previous load's), once that window is at least
/// [`PSI_SHORTEST`]; until then the kernel's own `avg10`, the shortest window it keeps and the
/// one the gate's guard decides by. A window shorter than that says less than the guard's own
/// statistic, which is why consecutive decisions a millisecond apart flickered.
pub fn window(anchor: &Raw, age: std::time::Duration, now: &Raw, interval_us: u64) -> Measure {
    if age >= PSI_SHORTEST {
        Measure::between(anchor, now, u64::try_from(age.as_micros()).unwrap_or(u64::MAX))
    } else {
        // No earlier read: each stall share is the kernel's `avg10`.
        Measure::between(&Raw::default(), now, interval_us)
    }
}

impl Sensor {
    /// A sensor of the machine and of the file system holding `work_dir`. It reads once now, so
    /// the first [`Sensor::measure`] covers the interval since.
    pub fn new(work_dir: &Path) -> Sensor {
        let mut reader = crate::os::sense::Reader::new(work_dir);
        let last = reader.read();
        let at = Instant::now();
        Sensor { reader, anchor: (last.clone(), at), last, at, peak_bps: None, due: false, anchor_due: false, strain: None, reads: 1 }
    }

    /// The load it measures changed now (placed work started or ended here): a measure is due, and
    /// strain is measured from its read on ([`window`]). No read of its own: one read is both the
    /// measure and the anchor (W05-R26).
    pub fn anchor(&mut self) {
        (self.due, self.anchor_due) = (true, true);
    }

    /// Its machine changed now (an event: the OS's pressure, power or thermal; W05-R26): a measure
    /// is due. No read of its own.
    pub fn changed(&mut self) {
        self.due = true;
    }

    /// Whether a measure is due: its machine or its load changed since its last read.
    pub fn due(&self) -> bool {
        self.due
    }

    /// How many times it has read the machine.
    pub fn reads(&self) -> u64 {
        self.reads
    }

    /// The last measure's pressure over the window strain is weighed by ([`window`]).
    pub fn strain(&self) -> Option<&Measure> {
        self.strain.as_ref()
    }

    /// What this runner can take now, over the interval since the previous read.
    pub fn measure(&mut self) -> Measure {
        let raw = self.reader.read();
        self.reads += 1;
        self.due = false;
        let now = Instant::now();
        if std::mem::take(&mut self.anchor_due) {
            self.anchor = (raw.clone(), now);
        }
        let interval_us = u64::try_from(now.duration_since(self.at).as_micros()).unwrap_or(u64::MAX);
        let mut m = Measure::between(&self.last, &raw, interval_us);
        if let Some(a) = &m.device {
            let bps = a.read_bytes_per_s + a.write_bytes_per_s;
            self.peak_bps = Some(self.peak_bps.map_or(bps, |p| p.max(bps))).filter(|p| *p > 0.0);
        }
        m.device_peak_bps = self.peak_bps;
        self.strain = Some(window(&self.anchor.0, now.duration_since(self.anchor.1), &raw, interval_us));
        (self.last, self.at) = (raw, now);
        m
    }

    /// The work device's counters as last read.
    pub fn last_device(&self) -> Option<disk::Counters> {
        self.last.device
    }

    /// The work directory it measures the file system and device of.
    pub fn work_dir(&self) -> &Path {
        self.reader.work_dir()
    }
}

/// What the work directory's volume is made of (docs/needs.md, W13-R01): `RunnerFacts.storage`.
pub fn storage(work_dir: &Path) -> Option<kiln_ci::facts::Storage> {
    crate::os::sense::storage(work_dir)
}

/// A path's file system, canonical where it can be.
pub(crate) fn canonical(dir: &Path) -> PathBuf {
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
}

/// Parsing of Linux's text files, pure so it's tested on every platform.
pub mod parse {
    use super::{CpuTicks, PsiFile, PsiLine};

    /// `/proc/stat`'s first line (`cpu  user nice system idle iowait irq softirq steal guest
    /// guest_nice`): idle is `idle + iowait`; all time is the first eight (guest time is already
    /// in `user` and `nice`).
    pub fn proc_stat_cpu(text: &str) -> Option<CpuTicks> {
        let line = text.lines().next()?;
        let mut words = line.split_whitespace();
        if words.next() != Some("cpu") {
            return None;
        }
        let v: Vec<u64> = words.take(8).map(|w| w.parse::<u64>()).collect::<Result<_, _>>().ok()?;
        let at = |i: usize| v.get(i).copied().unwrap_or(0);
        (v.len() >= 4).then(|| CpuTicks { idle: at(3) + at(4), total: v.iter().sum() })
    }

    /// The `key:  N kB` lines of `/proc/meminfo` asked for, in bytes.
    pub fn meminfo(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(':')?.split_whitespace().next()?.parse::<u64>().ok()).map(|kb| kb.saturating_mul(1024))
    }

    fn psi_line(line: &str) -> Option<PsiLine> {
        let mut avg10 = None;
        let mut total = None;
        for w in line.split_whitespace().skip(1) {
            if let Some(v) = w.strip_prefix("avg10=") {
                avg10 = v.parse::<f64>().ok();
            } else if let Some(v) = w.strip_prefix("total=") {
                total = v.parse::<u64>().ok();
            }
        }
        Some(PsiLine { avg10: avg10.unwrap_or(f64::NAN), total_us: total? })
    }

    /// A pressure stall file (`some avg10=… avg60=… avg300=… total=…`, then `full …`).
    pub fn psi(text: &str) -> Option<PsiFile> {
        let mut f = PsiFile::default();
        for line in text.lines() {
            match line.split_whitespace().next() {
                Some("some") => f.some = psi_line(line),
                Some("full") => f.full = psi_line(line),
                _ => {}
            }
        }
        (f.some.is_some() || f.full.is_some()).then_some(f)
    }

    /// A cgroup limit file's value: `max` is none.
    pub fn limit(text: &str) -> Option<u64> {
        let t = text.trim();
        if t == "max" {
            return None;
        }
        t.parse::<u64>().ok().filter(|l| *l < 1 << 60)
    }

    /// `cpu.max` (`quota period`, or `max period`): the CPUs it allows.
    pub fn cpu_max(text: &str) -> Option<f64> {
        let mut w = text.split_whitespace();
        let quota = w.next()?.parse::<u64>().ok()?;
        let period = w.next()?.parse::<u64>().ok().filter(|p| *p > 0)?;
        Some(quota as f64 / period as f64)
    }

    /// A flat keyed file's value (`memory.stat`, `cpu.stat`).
    pub fn keyed(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|l| l.split_once(' ').filter(|(k, _)| *k == key).and_then(|(_, v)| v.trim().parse().ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn psi_file(some_us: u64, full_us: u64) -> Option<PsiFile> {
        Some(PsiFile { some: Some(PsiLine { avg10: 50.0, total_us: some_us }), full: Some(PsiLine { avg10: 50.0, total_us: full_us }) })
    }

    /// AUDIT L1, strain without flicker: a decision weighs strain over the window since the load
    /// last changed once that's at least the kernel's shortest average (10 s), exactly; before, by
    /// the kernel's `avg10` (the gate guard's statistic), however short the interval since the
    /// last read, so two decisions a millisecond apart see the same figure.
    #[test]
    fn l1_strain_is_weighed_over_the_window_since_the_load_changed() {
        let at = |full_us: u64| Raw { psi: PsiRaw { memory: Some(PsiFile { some: None, full: Some(PsiLine { avg10: 5.0, total_us: full_us }) }), ..PsiRaw::default() }, ..Raw::default() };
        let (anchor, now) = (at(0), at(4_000_000));
        // 4 s stalled of the 20 s since the load changed: 20 %.
        let long = window(&anchor, std::time::Duration::from_secs(20), &now, 1_000);
        assert_eq!(long.pressure.memory.full, Some(0.2));
        // 5 s since it changed: the kernel's avg10 (5 %), not the 1 ms interval's share.
        let short = window(&anchor, std::time::Duration::from_secs(5), &now, 1_000);
        assert_eq!(short.pressure.memory.full, Some(0.05));
        assert_eq!(window(&anchor, std::time::Duration::from_secs(5), &now, 7).pressure, short.pressure);
    }

    /// W05-R26, W05-R03: the load changing reads nothing of its own; the measure it makes due is
    /// one read, and that read is the anchor strain is weighed from (L1).
    #[test]
    fn w05_r26_an_anchor_is_the_next_measures_own_read() {
        let mut s = Sensor::new(Path::new(env!("CARGO_MANIFEST_DIR")));
        assert!(s.reads() == 1 && !s.due(), "made with one read, nothing due");
        s.changed();
        s.anchor();
        assert!(s.reads() == 1 && s.due(), "a change read the machine, or made no measure due");
        let _ = s.measure();
        assert!(s.reads() == 2 && !s.due(), "the measure after a change is one read, and takes what was due");
        assert_eq!(s.anchor.1, s.at, "the anchor is that read");
        let anchored = s.anchor.1;
        s.changed();
        let _ = s.measure();
        assert!(s.reads() == 3 && s.anchor.1 == anchored, "an event's measure, the load unchanged, keeps the anchor");
    }

    /// docs/needs.md: CPU idle is exact over any interval: the idle ticks' delta over all ticks'.
    #[test]
    fn w05_cpu_idle_is_the_intervals_exact_share() {
        let a = Raw { cpu: Some(CpuTicks { idle: 1_000, total: 4_000 }), cpus: 4, ..Raw::default() };
        let b = Raw { cpu: Some(CpuTicks { idle: 1_300, total: 4_400 }), cpus: 4, ..Raw::default() };
        let m = Measure::between(&a, &b, 1_000_000);
        assert_eq!(m.cpu_idle, Some(0.75));
        assert_eq!(m.idle_cpus(), Some(3.0));
        // Counters that didn't move, or went back (a reset): no figure rather than a made-up one.
        assert_eq!(Measure::between(&a, &a, 10).cpu_idle, None);
        assert_eq!(Measure::between(&b, &a, 10).cpu_idle, None);
        assert_eq!(Measure::between(&Raw::default(), &b, 10).cpu_idle, None);
    }

    /// Pressure over the interval is its stall time's delta over the interval's length; on the
    /// first read, with nothing earlier, the kernel's own 10-second average.
    #[test]
    fn w05_pressure_is_the_intervals_stall_share() {
        let a = Raw { psi: PsiRaw { io: psi_file(1_000_000, 500_000), ..PsiRaw::default() }, ..Raw::default() };
        let b = Raw { psi: PsiRaw { io: psi_file(1_250_000, 600_000), ..PsiRaw::default() }, ..Raw::default() };
        let m = Measure::between(&a, &b, 1_000_000);
        assert_eq!(m.pressure.io, Stall { some: Some(0.25), full: Some(0.1) });
        assert_eq!(m.pressure.cpu, Stall::default());
        let first = Measure::between(&Raw::default(), &b, 1_000_000);
        assert_eq!(first.pressure.io, Stall { some: Some(0.5), full: Some(0.5) });
    }

    /// The runner's own cgroup bounds what it offers: memory by its limit less its working set,
    /// CPUs by its quota less what its tasks used.
    #[test]
    fn w05_r03_the_runners_cgroup_bounds_its_measure() {
        let g = 1u64 << 30;
        let cg = |usage_us: u64| CgroupRaw { path: "/kiln.scope".into(), mem_limit: Some(40 * g), mem_used: Some(30 * g), cpu_limit: Some(8.0), cpu_usage_us: Some(usage_us), ..CgroupRaw::default() };
        let a = Raw { cpu: Some(CpuTicks { idle: 0, total: 0 }), cpus: 32, mem_total: 124 * g, mem_available: 60 * g, cgroup: Some(cg(0)), ..Raw::default() };
        let b = Raw { cpu: Some(CpuTicks { idle: 3_000, total: 3_200 }), cpus: 32, mem_total: 124 * g, mem_available: 60 * g, cgroup: Some(cg(6_000_000)), ..Raw::default() };
        let m = Measure::between(&a, &b, 1_000_000);
        assert_eq!(m.mem_available, 10 * g, "the cgroup's 10 GiB, not the machine's 60");
        let c = m.cgroup.as_ref().map(|c| (c.cpu_used, c.mem_available));
        assert_eq!(c, Some((Some(6.0), Some(10 * g))));
        assert_eq!(m.idle_cpus(), Some(2.0), "8 CPUs of quota, 6 used; the machine's 30 idle don't count");
        // AUDIT M6: a host sample says the runner's budget, not the machine's.
        let budget = m.budget();
        assert_eq!((budget.cpus, budget.mem_total, budget.mem_used), (8, 40 * g, 30 * g));
        assert!((budget.busy - 0.75).abs() < 1e-9, "{budget:?}");
        // A runner moved to another cgroup: no delta across the move.
        let mut moved = b.clone();
        if let Some(c) = moved.cgroup.as_mut() {
            c.path = "/elsewhere".into();
        }
        assert_eq!(Measure::between(&a, &moved, 1_000_000).cgroup.and_then(|c| c.cpu_used), None);
    }

    #[test]
    fn w05_linux_files_parse() {
        let stat = "cpu  100 5 50 800 40 3 2 0 7 0\ncpu0 1 2 3 4 5 6 7 8 9 10\n";
        assert_eq!(parse::proc_stat_cpu(stat), Some(CpuTicks { idle: 840, total: 1000 }));
        assert_eq!(parse::proc_stat_cpu("intr 1 2 3"), None);
        let mem = "MemTotal:       65554812 kB\nMemFree:         1000000 kB\nMemAvailable:   48000000 kB\n";
        assert_eq!(parse::meminfo(mem, "MemAvailable"), Some(48_000_000 * 1024));
        assert_eq!(parse::meminfo(mem, "Mem"), None);
        let io = "some avg10=62.54 avg60=46.90 avg300=34.08 total=242189566596\nfull avg10=42.80 avg60=25.37 avg300=18.41 total=171556488716\n";
        let f = parse::psi(io);
        assert_eq!(f.and_then(|f| f.full).map(|l| (l.avg10, l.total_us)), Some((42.8, 171_556_488_716)));
        assert_eq!(parse::psi("some avg10=1.00 avg60=0.00 avg300=0.00 total=1\n").map(|f| f.full.is_none()), Some(true));
        assert_eq!(parse::psi(""), None);
        assert_eq!(parse::cpu_max("800000 100000\n"), Some(8.0));
        assert_eq!(parse::cpu_max("max 100000\n"), None);
        assert_eq!(parse::limit("max\n"), None);
        assert_eq!(parse::keyed("usage_usec 1234\nuser_usec 1000\n", "usage_usec"), Some(1234));
    }

    /// This machine measures itself: two reads, a share for each figure its OS keeps, levels
    /// within the machine's size.
    #[test]
    fn w05_this_machine_measures_itself() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut s = Sensor::new(dir.path());
        // Some CPU work, so the counters move even on a coarse clock.
        let mut x = 0u64;
        let t = Instant::now();
        while t.elapsed() < std::time::Duration::from_millis(30) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let m = s.measure();
        assert!(m.interval_us > 0 && m.cpus > 0, "{m:?}");
        assert!(m.cpu_idle.is_some_and(|i| (0.0..=1.0).contains(&i)), "{m:?}");
        assert!(m.mem_total > 0 && m.mem_available > 0 && m.mem_available <= m.mem_total, "{m:?}");
        let space = m.space.ok_or_else(|| anyhow::anyhow!("no space: {m:?}"))?;
        assert!(space.total > 0 && space.free <= space.total, "{m:?}");
        if cfg!(target_os = "macos") {
            assert!(m.memory_level.is_some() && m.pressure.is_empty(), "{m:?}");
            assert!(space.important.is_some_and(|i| i >= space.free), "{m:?}");
        }
        if cfg!(target_os = "linux") && Path::new("/proc/pressure/io").exists() {
            for v in [m.pressure.cpu.some, m.pressure.memory.full, m.pressure.io.full] {
                assert!(v.is_some_and(|v| (0.0..=1.0).contains(&v)), "{m:?}");
            }
        }
        if cfg!(windows) {
            assert!(m.pressure.is_empty() && m.memory_level.is_none(), "{m:?}");
        }
        // What a read costs here, said for the record (no bound asserted: a loaded machine is
        // slower, and that's no failure).
        let reads = 200u32;
        let t = Instant::now();
        for _ in 0..reads {
            std::hint::black_box(s.measure());
        }
        eprintln!("sense: one measure() took {:.1} µs on average over {reads} reads", t.elapsed().as_secs_f64() * 1e6 / f64::from(reads));
        // Its parts, for where the time goes: the free space and the device's counters.
        let t = Instant::now();
        for _ in 0..reads {
            std::hint::black_box(crate::os::disk_space(dir.path()));
        }
        eprintln!("sense: of which free space {:.1} µs", t.elapsed().as_secs_f64() * 1e6 / f64::from(reads));
        let mut r = crate::os::sense::Reader::new(dir.path());
        let t = Instant::now();
        for _ in 0..reads {
            std::hint::black_box(r.read());
        }
        eprintln!("sense: one raw read {:.1} µs", t.elapsed().as_secs_f64() * 1e6 / f64::from(reads));
        Ok(())
    }
}
