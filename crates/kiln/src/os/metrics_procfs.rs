//! Linux host and process statistics read straight from procfs. No daemons, no cgroup exporters: one
//! sampler call reads a handful of small files, so sampling every few seconds costs microseconds.

use std::path::Path;

use crate::host::ProcStats;
use crate::metrics::HostSample;

fn read(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn field_u64(s: &str, idx: usize) -> u64 {
    s.split_whitespace().nth(idx).and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[derive(Default, Clone, Copy)]
struct CpuTimes {
    busy: u64,
    total: u64,
}

fn cpu_times() -> CpuTimes {
    let stat = read("/proc/stat");
    let Some(line) = stat.lines().next() else { return CpuTimes::default() };
    let vals: Vec<u64> = line.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    let total: u64 = vals.iter().sum();
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    CpuTimes { busy: total.saturating_sub(idle), total }
}

/// Stateful sampler: CPU% and network are deltas between calls.
#[derive(Default)]
pub struct Sampler {
    last_cpu: CpuTimes,
    work_dir: Option<std::path::PathBuf>,
}

impl Sampler {
    pub fn new(work_dir: Option<&Path>) -> Self {
        Self { last_cpu: cpu_times(), work_dir: work_dir.map(Path::to_path_buf) }
    }

    pub fn sample(&mut self) -> HostSample {
        let now = cpu_times();
        let dt = now.total.saturating_sub(self.last_cpu.total);
        let db = now.busy.saturating_sub(self.last_cpu.busy);
        self.last_cpu = now;
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let cpu_pct = if dt == 0 { 0.0 } else { (db as f64 / dt as f64 * 100.0) as f32 };

        let mem = read("/proc/meminfo");
        let kb = |key: &str| -> u64 {
            mem.lines()
                .find(|l| l.starts_with(key))
                .map(|l| field_u64(l, 1) * 1024)
                .unwrap_or(0)
        };
        let mem_total = kb("MemTotal:");
        let mem_used = mem_total.saturating_sub(kb("MemAvailable:"));

        let load = read("/proc/loadavg");
        let load1 = load.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        let procs = load
            .split_whitespace()
            .nth(3)
            .and_then(|v| v.split('/').nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let (mut net_rx, mut net_tx) = (0u64, 0u64);
        for line in read("/proc/net/dev").lines().skip(2) {
            let Some((iface, rest)) = line.split_once(':') else { continue };
            let iface = iface.trim();
            if iface == "lo" || iface.starts_with("veth") || iface.starts_with("docker") {
                continue;
            }
            net_rx += field_u64(rest, 0);
            net_tx += field_u64(rest, 8);
        }

        let (disk_total, disk_used) = self
            .work_dir
            .as_deref()
            .and_then(|d| nix::sys::statvfs::statvfs(d).ok())
            .map(|s| {
                let frag = s.fragment_size();
                let total = s.blocks().saturating_mul(frag);
                let free = s.blocks_available().saturating_mul(frag);
                (total, total.saturating_sub(free))
            })
            .unwrap_or((0, 0));

        HostSample {
            ts_ms: crate::util::now_ms(),
            cpu_pct,
            cpus: u32::try_from(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1))
                .unwrap_or(1),
            load1,
            mem_total,
            mem_used,
            disk_total,
            disk_used,
            net_rx,
            net_tx,
            procs,
        }
    }
}

fn clk_tck() -> f64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .map(|v| {
            #[allow(clippy::cast_precision_loss)]
            let f = v as f64;
            f
        })
        .unwrap_or(100.0)
}

fn page_size() -> u64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::PAGE_SIZE)
        .ok()
        .flatten()
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(4096)
}

/// Parse `/proc/<pid>/stat` after the `comm` field (which may contain spaces).
fn stat_fields(pid: &str) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = s.rfind(')')?;
    Some(s.get(close + 2..)?.split_whitespace().map(str::to_owned).collect())
}

pub fn process_self() -> ProcStats {
    let Some(f) = stat_fields("self") else { return ProcStats::default() };
    // After comm: state=0 ppid=1 pgrp=2 ... utime=11 stime=12 ... num_threads=17 ... rss=21
    let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    #[allow(clippy::cast_precision_loss)]
    let cpu_seconds = (n(11) + n(12)) as f64 / clk_tck();
    let open_fds = std::fs::read_dir("/proc/self/fd").map(|d| d.count() as u64).unwrap_or(0);
    ProcStats { rss_bytes: n(21) * page_size(), cpu_seconds, open_fds, threads: n(17) }
}

/// Aggregate CPU time and RSS of every process in a process group: the footprint of one job.
pub fn process_group(pgid: i32) -> ProcStats {
    let mut acc = ProcStats::default();
    let Ok(dir) = std::fs::read_dir("/proc") else { return acc };
    let tck = clk_tck();
    let page = page_size();
    let want = pgid.to_string();
    for e in dir.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(f) = stat_fields(pid) else { continue };
        if f.get(2).map(String::as_str) != Some(want.as_str()) {
            continue;
        }
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        #[allow(clippy::cast_precision_loss)]
        {
            acc.cpu_seconds += (n(11) + n(12)) as f64 / tck;
        }
        acc.rss_bytes += n(21) * page;
        acc.threads += n(17);
    }
    acc
}
