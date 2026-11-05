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

/// Stateful sampler: CPU and memory from the runner's sensor ([`crate::sense::Sensor`]), as its
/// cgroup bounds them (AUDIT M6: a runner given 8 of a machine's 32 CPUs says 8, and how busy
/// those are); load, processes and network from procfs. Each figure that changes is the exact
/// average since the previous sample.
pub struct Sampler {
    sensor: crate::sense::Sensor,
}

impl Sampler {
    pub fn new(work_dir: Option<&Path>) -> Self {
        Self { sensor: crate::sense::Sensor::new(work_dir.unwrap_or_else(|| Path::new("/"))) }
    }

    pub fn sample(&mut self) -> HostSample {
        let m = self.sensor.measure();
        let b = m.budget();
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
        let (disk_total, disk_used) = m.space.map_or((0, 0), |s| (s.total, s.total.saturating_sub(s.free)));
        #[allow(clippy::cast_possible_truncation)]
        let cpu_pct = (b.busy * 100.0) as f32;
        HostSample { ts_ms: crate::util::now_ms(), cpu_pct, cpus: b.cpus, load1, mem_total: b.mem_total, mem_used: b.mem_used, disk_total, disk_used, net_rx, net_tx, procs }
    }
}

/// This machine's pressure stall information over the last 10 seconds (docs/reach.md §2):
/// `/proc/pressure/cpu`'s `some`, and `/proc/pressure/memory`'s and `/proc/pressure/io`'s `full`.
/// None of what a kernel without it (or booted with `psi=0`) doesn't say.
pub fn pressure() -> crate::metrics::Pressure {
    let avg10 = |file: &str, kind: &str| std::fs::read_to_string(format!("/proc/pressure/{file}")).ok().and_then(|text| crate::metrics::psi_avg10(&text, kind));
    crate::metrics::Pressure { cpu_some: avg10("cpu", "some"), mem_full: avg10("memory", "full"), io_full: avg10("io", "full"), memory: None }
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
