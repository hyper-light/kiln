//! Host and process statistics where there is no procfs (macOS, Windows), through `sysinfo`.
//! A job's footprint is its process tree (the step's shell and every descendant), the closest
//! portable equivalent of a Unix process group.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sysinfo::{Disks, Networks, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::host::ProcStats;
use crate::metrics::HostSample;

/// Stateful sampler: CPU% is a delta between calls; network totals are cumulative.
pub struct Sampler {
    sys: System,
    nets: Networks,
    disks: Disks,
    work_dir: Option<PathBuf>,
}

impl Sampler {
    pub fn new(work_dir: Option<&Path>) -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        Self {
            sys,
            nets: Networks::new_with_refreshed_list(),
            disks: Disks::new_with_refreshed_list(),
            work_dir: work_dir.map(Path::to_path_buf),
        }
    }

    pub fn sample(&mut self) -> HostSample {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        let mem_total = self.sys.total_memory();
        let mem_used = mem_total.saturating_sub(self.sys.available_memory());
        #[allow(clippy::cast_possible_truncation)]
        let load1 = System::load_average().one as f32;

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

        let (disk_total, disk_used) = self.work_dir_disk();

        self.sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
        HostSample {
            ts_ms: crate::util::now_ms(),
            cpu_pct: self.sys.global_cpu_usage(),
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)).unwrap_or(1),
            load1,
            mem_total,
            mem_used,
            disk_total,
            disk_used,
            net_rx,
            net_tx,
            procs: u32::try_from(self.sys.processes().len()).unwrap_or(u32::MAX),
        }
    }

    /// Size and use of the volume holding the work dir: the disk with the longest mount point
    /// that is a prefix of it.
    fn work_dir_disk(&mut self) -> (u64, u64) {
        let Some(dir) = self.work_dir.as_deref() else { return (0, 0) };
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.disks.refresh(true);
        self.disks
            .list()
            .iter()
            .filter(|d| dir.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
            .map(|d| (d.total_space(), d.total_space().saturating_sub(d.available_space())))
            .unwrap_or((0, 0))
    }
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

/// Aggregate CPU time and RSS of the process tree rooted at `root`: the footprint of one job.
pub fn process_group(root: i32) -> ProcStats {
    let mut acc = ProcStats::default();
    let Ok(root) = u32::try_from(root) else { return acc };
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_memory().with_cpu().without_tasks());
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut seen = HashSet::new();
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
