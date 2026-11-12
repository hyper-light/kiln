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
        // The machine's load on this process's CPUs of those `/proc/stat` lists (`load_of`, W05-R03).
        let (stat, cpus) = (read("/proc/stat"), b.cpus);
        let load = read("/proc/loadavg");
        let load1 = load_of(&load, &stat, cpus);
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

/// The machine's CPUs, as `/proc/stat` lists them: a `cpuN` line for each it has online.
fn online(stat: &str) -> u32 {
    let n = stat.lines().filter(|l| l.strip_prefix("cpu").is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))).count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The machine's 1-minute load (`/proc/loadavg`'s first figure) on this process's `cpus` of the CPUs `/proc/stat` lists
/// (`HostSample::load_on`, W05-R03).
fn load_of(loadavg: &str, stat: &str, cpus: u32) -> f32 {
    let load1 = loadavg.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    HostSample::load_on(load1, cpus, online(stat))
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

/// The CPU time, µs, those of `pids` that run have used so far, all told (`/proc/<pid>/stat`'s
/// `utime` and `stime`): none when none of them runs. What a group Shards keeps lists of its
/// microVMs' processes is charged so (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 32).
pub fn cpu_us_of(pids: &[u32]) -> Option<u64> {
    let tck = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK).ok().flatten().and_then(|v| u64::try_from(v).ok()).filter(|v| *v > 0).unwrap_or(100);
    let mut ticks: Option<u64> = None;
    for pid in pids {
        let Some(f) = stat_fields(&pid.to_string()) else { continue };
        let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        ticks = Some(ticks.unwrap_or(0).saturating_add(n(11)).saturating_add(n(12)));
    }
    ticks.map(|t| u64::try_from(u128::from(t).saturating_mul(1_000_000) / u128::from(tck)).unwrap_or(u64::MAX))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// W05-R03: a runner confined to 4 of a busy machine's 32 CPUs (an affinity, a cgroup's quota) reads the machine's load
    /// on its own 4, so judged against them it's the machine's against its cores. At the machine's load 16, half its cores,
    /// it has half its own idle and isn't overloaded, by its room (v1's and reach's) and by the engine's headroom alike; it
    /// read the machine's whole 16 against its 4, past twice them, with none idle. Past twice the machine's cores, it's
    /// overloaded as the machine is; and a runner on all 32 reads the machine's load as it is.
    #[test]
    fn w05_r03_a_confined_runner_on_a_busy_machine_reads_the_machines_load_on_its_own_cpus() {
        const GIB: u64 = 1 << 30;
        let stat: String = std::iter::once("cpu  4 0 4 8 0 0 0 0 0 0\n".to_owned()).chain((0..32).map(|n| format!("cpu{n} 1 0 1 1 0 0 0 0 0 0\n"))).chain(["intr 1\n".to_owned(), "ctxt 1\n".to_owned()]).collect();
        let loadavg = |load: f32| format!("{load:.2} 10.00 10.00 3/900 12345\n");
        assert_eq!(online(&stat), 32, "a cpuN line each, not the machine's total line");
        let load = load_of(&loadavg(16.0), &stat, 4);
        assert!((load - 2.0).abs() < 1e-6, "the machine's 16 over 32 CPUs, on 4 of them: {load}");
        // The engine's measure of the same sample: half its own CPUs idle, room for more. The load
        // figure is carried, and placement reads none of it (W05, docs/reach.md §2).
        let sample = kiln_ci::sched::state::Sample { cpus: 4, busy: 0.5, load1: f64::from(load), mem_total: 64 * GIB, mem_used: 16 * GIB, io_full: None, mem_full: None };
        let headroom = kiln_ci::sched::state::Headroom::of(&sample);
        assert!((headroom.cpus - 2.0).abs() < 1e-9 && headroom.within(&kiln_ci::sched::state::Resources::default(), 0.85), "the engine's headroom: {headroom:?}");
        assert!((load_of(&loadavg(16.0), &stat, 32) - 16.0).abs() < 1e-6, "a runner on all the machine's CPUs: its load as it is");
    }
}
