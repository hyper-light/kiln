//! Host and process statistics where there is no procfs (macOS, Windows): the machine's figures
//! from the runner's sensor ([`crate::sense`]), network totals through `sysinfo`. A job's footprint
//! is its process group on macOS (libproc), its process tree on Windows (the step's shell and every
//! descendant), the closest portable equivalent of a Unix process group.

use std::path::Path;

use sysinfo::{Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::host::ProcStats;
use crate::metrics::HostSample;

/// Stateful sampler: CPU and memory from the runner's sensor ([`crate::sense::Sensor`]: Mach's
/// and Win32's own counters, each figure the exact average since the previous sample), the work
/// directory's volume from it too, network totals (cumulative) from `sysinfo`, and the number of
/// processes by one call that lists none of them (AUDIT P3: no process table is walked per sample).
pub struct Sampler {
    sensor: crate::sense::Sensor,
    nets: Networks,
    /// The machine's CPUs (`sysinfo`'s list, once), for its load on this process's share of them
    /// (`HostSample::load_on`, W05-R03).
    host_cpus: u32,
}

impl Sampler {
    pub fn new(work_dir: Option<&Path>) -> Self {
        let dir = work_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
        let host_cpus = u32::try_from(System::new_with_specifics(sysinfo::RefreshKind::nothing().with_cpu(sysinfo::CpuRefreshKind::nothing())).cpus().len()).unwrap_or(u32::MAX);
        Self { sensor: crate::sense::Sensor::new(&dir), nets: Networks::new_with_refreshed_list(), host_cpus }
    }

    pub fn sample(&mut self) -> HostSample {
        let m = self.sensor.measure();
        let b = m.budget();
        // The machine's load on this process's CPUs of all the machine's (`HostSample::load_on`, W05-R03).
        let cpus = b.cpus;
        #[allow(clippy::cast_possible_truncation)]
        let load1 = HostSample::load_on(System::load_average().one as f32, cpus, self.host_cpus);

        self.nets.refresh(true);
        let (mut net_rx, mut net_tx) = (0u64, 0u64);
        for (name, data) in self.nets.list() {
            let lower = name.to_ascii_lowercase();
            if lower == "lo" || lower.starts_with("lo0") || lower.contains("loopback") {
                continue;
            }
            net_rx = net_rx.saturating_add(data.total_received());
            net_tx = net_tx.saturating_add(data.total_transmitted());
        }
        let (disk_total, disk_used) = m.space.map_or((0, 0), |s| (s.total, s.total.saturating_sub(s.free)));
        #[allow(clippy::cast_possible_truncation)]
        let cpu_pct = (b.busy * 100.0) as f32;
        HostSample { ts_ms: crate::util::now_ms(), cpu_pct, cpus: b.cpus, load1, mem_total: b.mem_total, mem_used: b.mem_used, disk_total, disk_used, net_rx, net_tx, procs: process_count() }
    }
}

/// How many processes run. On macOS it's asked of the kernel as a count: a `sysinfo` refresh reads
/// every process (`sysctl` and `proc_pidinfo` for each), and a runner samples inline on its event
/// loop. With 9,145 processes (8,009 of them leaked ssh-agents) that read took as long as the 5 s
/// between samples, so the loop never caught up, its server's commands went unanswered and its
/// agent sessions read offline (2026-11-04). On Windows, `GetPerformanceInfo`'s `ProcessCount`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn process_count() -> u32 {
    // SAFETY: a null buffer of size 0 asks only how much room the list needs (with slack);
    // nothing is written.
    let room = unsafe { nix::libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(room) = usize::try_from(room) else { return 0 };
    let mut pids = vec![0i32; room.saturating_add(64)];
    let bytes = i32::try_from(pids.len().saturating_mul(std::mem::size_of::<i32>())).unwrap_or(i32::MAX);
    // SAFETY: `pids` has room for `bytes` bytes; the kernel writes at most that many and
    // returns how many pids it wrote.
    let n = unsafe { nix::libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    u32::try_from(n).unwrap_or(0)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn process_count() -> u32 {
    /// `PERFORMANCE_INFORMATION`.
    #[repr(C)]
    #[derive(Default)]
    struct PerformanceInformation {
        cb: u32,
        sizes: [usize; 10],
        handle_count: u32,
        process_count: u32,
        thread_count: u32,
    }
    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(std::mem::size_of::<PerformanceInformation>() == 104 && std::mem::offset_of!(PerformanceInformation, process_count) == 92);
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetPerformanceInfo(info: *mut PerformanceInformation, cb: u32) -> i32;
    }
    let mut info = PerformanceInformation { cb: u32::try_from(std::mem::size_of::<PerformanceInformation>()).unwrap_or(0), ..PerformanceInformation::default() };
    let cb = info.cb;
    // SAFETY: a whole PERFORMANCE_INFORMATION, its size given.
    if unsafe { K32GetPerformanceInfo(&raw mut info, cb) } == 0 {
        return 0;
    }
    info.process_count
}

#[cfg(not(any(target_os = "macos", windows)))]
fn process_count() -> u32 {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
    u32::try_from(sys.processes().len()).unwrap_or(u32::MAX)
}

/// This machine's pressure (docs/reach.md §2): on macOS its memory pressure level, which any
/// process may read; it has no stall times. Windows says none.
pub fn pressure() -> crate::metrics::Pressure {
    crate::metrics::Pressure { memory: memory_level(), ..crate::metrics::Pressure::default() }
}

/// macOS's memory pressure level, `kern.memorystatus_vm_pressure_level` (XNU checks no privilege
/// for it on macOS), read without a process to start.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn memory_level() -> Option<crate::metrics::MemoryLevel> {
    use nix::libc;
    let mut level: libc::c_uint = 0;
    let mut len = std::mem::size_of::<libc::c_uint>();
    // SAFETY: the name is NUL-terminated; the out pointer and its length name one live local
    // `c_uint`, the sysctl's type (`CTLTYPE_INT`); no new value is written.
    let outcome = unsafe { libc::sysctlbyname(c"kern.memorystatus_vm_pressure_level".as_ptr(), (&raw mut level).cast::<libc::c_void>(), &raw mut len, std::ptr::null_mut(), 0) };
    (outcome == 0 && len == std::mem::size_of::<libc::c_uint>()).then_some(level).and_then(crate::metrics::MemoryLevel::of_dispatch)
}

/// No memory pressure level where there's no macOS kernel to say it.
#[cfg(not(target_os = "macos"))]
pub(crate) fn memory_level() -> Option<crate::metrics::MemoryLevel> {
    None
}

fn stats_of(p: &sysinfo::Process) -> ProcStats {
    #[allow(clippy::cast_precision_loss)]
    let cpu_seconds = p.accumulated_cpu_time() as f64 / 1000.0;
    ProcStats {
        rss_bytes: p.memory(),
        cpu_seconds,
        open_fds: p.open_files().map_or(0, |n| u64::try_from(n).unwrap_or(0)),
        threads: p.tasks().map_or(1, |t| u64::try_from(t.len()).unwrap_or(0).max(1)),
    }
}

pub fn process_self() -> ProcStats {
    let Ok(me) = sysinfo::get_current_pid() else { return ProcStats::default() };
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[me]), false, ProcessRefreshKind::nothing().with_memory().with_cpu());
    sys.process(me).map(stats_of).unwrap_or_default()
}

/// Aggregate CPU time and RSS of process group `pgid`: the footprint of one job, as Linux's
/// procfs one counts it. macOS lists the group's members alone (`proc_listpids(PROC_PGRP_ONLY)`)
/// and asks each for its task info, never walking the process table (AUDIT P3).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn process_group(pgid: i32) -> ProcStats {
    use nix::libc;
    /// `PROC_PGRP_ONLY` (`<libproc.h>`).
    const PROC_PGRP_ONLY: u32 = 2;
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
    }
    let mut acc = ProcStats::default();
    let Ok(group) = u32::try_from(pgid) else { return acc };
    // SAFETY: no buffer: the call returns the bytes the list needs.
    let need = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, std::ptr::null_mut(), 0) };
    let Ok(need) = usize::try_from(need) else { return acc };
    // Room for what joins between the two calls: as many again.
    let mut pids = vec![0 as libc::pid_t; need / std::mem::size_of::<libc::pid_t>() * 2 + 1];
    let bytes = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::pid_t>()).unwrap_or(0);
    // SAFETY: a buffer of `bytes` bytes of pids.
    let got = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes) };
    let n = usize::try_from(got).unwrap_or(0) / std::mem::size_of::<libc::pid_t>();
    let mut tb = [0u32; 2];
    // SAFETY: a mach_timebase_info_data_t (numer, denom).
    let ticks_to_ns = if unsafe { mach_timebase_info(&raw mut tb) } == 0 && tb[1] > 0 { f64::from(tb[0]) / f64::from(tb[1]) } else { 1.0 };
    for pid in pids.iter().take(n).copied().filter(|p| *p > 0) {
        // SAFETY: plain integers, for which zero is a value.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).unwrap_or(0);
        // SAFETY: a whole proc_taskinfo, its size given.
        let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
        if r != size {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            acc.cpu_seconds += (info.pti_total_user.saturating_add(info.pti_total_system)) as f64 * ticks_to_ns / 1e9;
        }
        acc.rss_bytes = acc.rss_bytes.saturating_add(info.pti_resident_size);
        acc.threads = acc.threads.saturating_add(u64::try_from(info.pti_threadnum).unwrap_or(0));
    }
    acc
}

/// The CPU time, µs, those of `pids` that run have used so far, all told (each one's task info,
/// in Mach's time base, as [`process_group`] reads it): none when none of them runs. What a group
/// Shards keeps lists of its microVMs' processes is charged so (kiln plan
/// 01a1cd8e862e7169a074db3d5fa33f3d step 32).
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn cpu_us_of(pids: &[u32]) -> Option<u64> {
    use nix::libc;
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
    }
    let mut tb = [0u32; 2];
    // SAFETY: a mach_timebase_info_data_t (numer, denom).
    let [numer, denom] = if unsafe { mach_timebase_info(&raw mut tb) } == 0 { tb } else { [1, 1] };
    let mut ticks: Option<u128> = None;
    for pid in pids.iter().filter_map(|p| i32::try_from(*p).ok()) {
        // SAFETY: plain integers, for which zero is a value.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_taskinfo>()).unwrap_or(0);
        // SAFETY: a whole proc_taskinfo, its size given.
        let r = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size) };
        if r != size {
            continue;
        }
        ticks = Some(ticks.unwrap_or(0).saturating_add(u128::from(info.pti_total_user.saturating_add(info.pti_total_system))));
    }
    ticks.map(|t| u64::try_from(t.saturating_mul(u128::from(numer)) / u128::from(denom.max(1)) / 1000).unwrap_or(u64::MAX))
}

/// The CPU time, µs, those of `pids` that run have used so far, all told (`sysinfo`'s, of each):
/// none when none of them runs. What a group Shards keeps lists of its microVMs' processes is
/// charged so (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32).
#[cfg(not(target_os = "macos"))]
pub fn cpu_us_of(pids: &[u32]) -> Option<u64> {
    let pids: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), false, ProcessRefreshKind::nothing().with_cpu());
    let ms = pids.iter().filter_map(|p| sys.process(*p)).map(sysinfo::Process::accumulated_cpu_time).reduce(u64::saturating_add)?;
    Some(ms.saturating_mul(1000))
}

/// Aggregate CPU time and RSS of the process tree rooted at `root`: the footprint of one job.
#[cfg(not(target_os = "macos"))]
pub fn process_group(root: i32) -> ProcStats {
    let mut acc = ProcStats::default();
    let Ok(root) = u32::try_from(root) else { return acc };
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_memory().with_cpu().without_tasks());
    let mut children: std::collections::HashMap<Pid, Vec<Pid>> = std::collections::HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut todo = vec![Pid::from_u32(root)];
    while let Some(pid) = todo.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let Some(p) = sys.process(pid) else { continue };
        let s = stats_of(p);
        acc.rss_bytes = acc.rss_bytes.saturating_add(s.rss_bytes);
        acc.cpu_seconds += s.cpu_seconds;
        acc.threads = acc.threads.saturating_add(s.threads);
        todo.extend(children.get(&pid).into_iter().flatten().copied());
    }
    acc
}

/// Whether process `pid` has `marker` (`KEY=value`) in its environment (same-user processes).
pub fn process_has_env(pid: u32, marker: &str) -> bool {
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, ProcessRefreshKind::nothing().with_environ(UpdateKind::Always));
    sys.process(pid).is_some_and(|p| p.environ().iter().any(|kv| kv.as_os_str() == std::ffi::OsStr::new(marker)))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// A Mac's process count comes from the kernel's pid list, not a read of every process (which,
    /// inline on a runner's loop, stalled it on a machine with 9,145 processes): it's the count
    /// `sysinfo` gets the long way, give or take what started or ended between the two.
    #[test]
    fn a_macs_process_count_is_the_kernels_without_reading_each_process() {
        let quick = process_count();
        let mut sys = System::new();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
        let slow = u32::try_from(sys.processes().len()).unwrap_or(u32::MAX);
        assert!(quick > 0, "no processes counted");
        assert!(quick.abs_diff(slow) <= slow / 20 + 5, "the kernel's count {quick} is far from sysinfo's {slow}");
    }
}
