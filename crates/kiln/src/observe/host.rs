//! A node's own figures (`docs/observe.md` §8): its host's CPU, load, memory, swap, disks, disk I/O
//! and network, the container or cgroup it runs in, and its own process, every [`EVERY`], each
//! node for itself, wherever kiln runs: bare metal, a VM, a container, a Kubernetes pod; Linux,
//! macOS, Windows.
//!
//! Names and units are Datadog's System Check's and container metrics' (docs.datadoghq.com
//! "System", "Disk", "Network", "Container"), so dashboards and monitors written for them read
//! here: `system.cpu.*` in percent of the interval, `system.mem.*` and `system.swap.*` in MiB,
//! `system.disk.*` in KiB (`in_use` a fraction), `system.net.*` and `system.io.*` per second,
//! `container.cpu.*` in nanocores, `container.memory.*` in bytes.
//!
//! - Linux reads procfs and sysfs (no daemon, microseconds a look): `/proc/stat`, `/proc/loadavg`,
//!   `/proc/meminfo`, `/proc/net/dev`, `/proc/diskstats`, the mounts the process sees
//!   (`/proc/self/mountinfo`, so a container reports its own) and their `statvfs`; and the
//!   process's cgroup (v2's `memory.max`, `cpu.max`, `cpu.stat`, else v1's), whose limits are the
//!   ones a container or a systemd unit gives it, not the machine's.
//! - macOS and Windows ask the OS through `sysinfo`: CPU (without the split of user and system time,
//!   which it does not give), memory, swap, disks and networks.
//! - A network file system (NFS, SMB, sshfs) is not looked at: a `statvfs` on a dead mount waits
//!   forever; the looking runs on a thread of its own besides, so nothing else waits with it.
//! - Tags: `host` (the node's name), and in a container `container_id`, in a pod
//!   `kube_namespace`, `pod_name` and `kube_node` (from the downward API's environment, when the
//!   pod states them).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use kiln_obs::batch::{Kind, SeriesKey};

use super::Observe;

/// How often: the Datadog Agent's checks' (`min_collection_interval`, 15 s).
pub const EVERY: Duration = Duration::from_secs(15);

/// One reading of a counter the next is taken against, with when it was read.
#[derive(Debug, Clone, Default)]
struct Prev {
    at: Option<Instant>,
    nets: HashMap<String, NetCounters>,
    #[cfg(target_os = "linux")]
    cpu: Option<CpuTimes>,
    #[cfg(target_os = "linux")]
    disks: HashMap<String, DiskCounters>,
    #[cfg(target_os = "linux")]
    cgroup_cpu: Option<CgroupCpu>,
}

/// `/proc/stat`'s first line, in USER_HZ ticks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTimes {
    fn total(&self) -> u64 {
        // Guest time is counted within user time by the kernel (`account_guest_time`).
        self.user + self.nice + self.system + self.idle + self.iowait + self.irq + self.softirq + self.steal
    }
}

/// `/proc/stat`: the aggregate CPU line and the number of CPUs.
pub fn parse_stat(text: &str) -> Option<(CpuTimes, u64)> {
    let mut cpus = 0u64;
    let mut times = None;
    for line in text.lines() {
        let mut f = line.split_whitespace();
        match f.next() {
            Some("cpu") => {
                let v: Vec<u64> = f.map(|x| x.parse().unwrap_or(0)).collect();
                let at = |i: usize| v.get(i).copied().unwrap_or(0);
                times = Some(CpuTimes { user: at(0), nice: at(1), system: at(2), idle: at(3), iowait: at(4), irq: at(5), softirq: at(6), steal: at(7), guest: at(8), guest_nice: at(9) });
            }
            Some(name) if name.starts_with("cpu") => cpus += 1,
            _ => {}
        }
    }
    times.map(|t| (t, cpus))
}

/// The CPU's split over the interval between two readings, in percent: user, system, iowait,
/// idle, stolen, guest.
pub fn cpu_percent(a: &CpuTimes, b: &CpuTimes) -> Option<[f64; 6]> {
    let total = b.total().checked_sub(a.total()).filter(|&t| t > 0)? as f64;
    let d = |x: u64, y: u64| y.saturating_sub(x) as f64 * 100.0 / total;
    let guest = d(a.guest + a.guest_nice, b.guest + b.guest_nice);
    let user = (d(a.user + a.nice, b.user + b.nice) - guest).max(0.0);
    Some([user, d(a.system + a.irq + a.softirq, b.system + b.irq + b.softirq), d(a.iowait, b.iowait), d(a.idle, b.idle), d(a.steal, b.steal), guest])
}

/// `/proc/meminfo`, in kB, by field.
pub fn parse_meminfo(text: &str) -> HashMap<&str, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

/// An interface's counters (`/proc/net/dev`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetCounters {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
}

pub fn parse_net_dev(text: &str) -> Vec<(String, NetCounters)> {
    text.lines()
        .skip(2)
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let v: Vec<u64> = rest.split_whitespace().map(|x| x.parse().unwrap_or(0)).collect();
            let at = |i: usize| v.get(i).copied().unwrap_or(0);
            Some((name.trim().to_owned(), NetCounters { rx_bytes: at(0), rx_packets: at(1), rx_errors: at(2), tx_bytes: at(8), tx_packets: at(9), tx_errors: at(10) }))
        })
        .filter(|(n, _)| n != "lo")
        .collect()
}

/// A block device's counters (`/proc/diskstats`): reads and writes done, sectors (512 bytes each,
/// whatever the device's own size: the kernel's unit), and the milliseconds it was busy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCounters {
    pub reads: u64,
    pub read_sectors: u64,
    pub writes: u64,
    pub write_sectors: u64,
    pub busy_ms: u64,
}

/// Whole disks and their partitions as `/proc/diskstats` lists them; loop, ram and device-mapper
/// zero devices are left out (a loop device's I/O is its file's, counted on its own disk).
pub fn parse_diskstats(text: &str) -> Vec<(String, DiskCounters)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let name = *f.get(2)?;
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
                return None;
            }
            let n = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
            let c = DiskCounters { reads: n(3), read_sectors: n(5), writes: n(7), write_sectors: n(9), busy_ms: n(12) };
            (c != DiskCounters::default()).then(|| (name.to_owned(), c))
        })
        .collect()
}

/// File system types not looked at: the kernel's own (no disk under them), and network ones (a
/// `statvfs` on a dead mount waits forever).
const SKIP_FS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "cgroup", "cgroup2", "securityfs", "pstore", "bpf", "debugfs", "tracefs", "configfs", "fusectl", "mqueue", "hugetlbfs", "binfmt_misc", "autofs", "nsfs", "rpc_pipefs", "efivarfs", "ramfs", "squashfs", "selinuxfs",
    "nfs", "nfs4", "cifs", "smb3", "smbfs", "fuse.sshfs", "fuse.rclone", "9p", "ceph", "glusterfs", "afs",
];

/// The mounts to look at (`/proc/self/mountinfo`): each mount point once, with its device and type.
pub fn parse_mountinfo(text: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for l in text.lines() {
        // id parent major:minor root point options [optional...] - type source super-options
        let Some((left, right)) = l.split_once(" - ") else { continue };
        let point = left.split_whitespace().nth(4).unwrap_or("");
        let mut r = right.split_whitespace();
        let (fstype, source) = (r.next().unwrap_or(""), r.next().unwrap_or(""));
        if point.is_empty() || SKIP_FS.contains(&fstype) || fstype.starts_with("fuse.") && fstype != "fuse.fuse-overlayfs" {
            continue;
        }
        // tmpfs is memory, but `/dev/shm` and `/run` filling up is a thing to see: kept.
        let point = point.replace("\\040", " ");
        if !out.iter().any(|(p, _, _)| *p == point) {
            out.push((point, source.to_owned(), fstype.to_owned()));
        }
    }
    out
}

/// The cgroup's CPU account (`cpu.stat` in µs, v2; v1's `cpuacct.usage` in ns).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CgroupCpu {
    pub usage_ns: u64,
    pub throttled_ns: u64,
    pub throttled_periods: u64,
}

/// A cgroup's limits and use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cgroup {
    pub memory_usage: Option<u64>,
    pub memory_limit: Option<u64>,
    /// CPUs the quota allows (`cpu.max`'s quota over its period).
    pub cpu_limit: Option<f64>,
    pub cpu: Option<CgroupCpu>,
}

/// v2's `cpu.max`: "max <period>" (no limit) or "<quota> <period>".
pub fn parse_cpu_max(text: &str) -> Option<f64> {
    let mut f = text.split_whitespace();
    let quota = f.next()?.parse::<f64>().ok()?;
    let period = f.next()?.parse::<f64>().ok().filter(|&p| p > 0.0)?;
    Some(quota / period)
}

/// v2's `cpu.stat`.
pub fn parse_cpu_stat(text: &str) -> CgroupCpu {
    let mut c = CgroupCpu::default();
    for l in text.lines() {
        let mut f = l.split_whitespace();
        let (k, v) = (f.next().unwrap_or(""), f.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0));
        match k {
            "usage_usec" => c.usage_ns = v.saturating_mul(1000),
            "throttled_usec" => c.throttled_ns = v.saturating_mul(1000),
            "nr_throttled" => c.throttled_periods = v,
            _ => {}
        }
    }
    c
}

/// The container a process runs in, from its cgroup paths (`/proc/self/cgroup`): the 64-hex id
/// Docker, containerd, CRI-O and Podman name their scopes with.
pub fn container_id(cgroup: &str) -> Option<String> {
    for line in cgroup.lines() {
        let path = line.rsplit(':').next().unwrap_or("");
        for part in path.split('/').rev() {
            let id = part.trim_end_matches(".scope").rsplit(['-', ':']).next().unwrap_or("");
            if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(id.to_owned());
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn cgroup() -> Option<Cgroup> {
    let read = |p: &str| std::fs::read_to_string(p).ok();
    let own = read("/proc/self/cgroup")?;
    // v2: "0::<path>", read under /sys/fs/cgroup (the process's own namespace's view: in a
    // container, its root is the container's cgroup).
    if let Some(path) = own.lines().find_map(|l| l.strip_prefix("0::")) {
        let base = std::path::Path::new("/sys/fs/cgroup");
        let dir = if base.join(path.trim_start_matches('/')).join("memory.current").exists() { base.join(path.trim_start_matches('/')) } else { base.to_path_buf() };
        let get = |f: &str| std::fs::read_to_string(dir.join(f)).ok();
        let memory_usage = get("memory.current").and_then(|v| v.trim().parse().ok());
        let memory_limit = get("memory.max").and_then(|v| v.trim().parse().ok());
        let cpu_limit = get("cpu.max").and_then(|v| parse_cpu_max(&v));
        let cpu = get("cpu.stat").map(|v| parse_cpu_stat(&v));
        return Some(Cgroup { memory_usage, memory_limit, cpu_limit, cpu });
    }
    // v1.
    let num = |p: &str| read(p).and_then(|v| v.trim().parse::<u64>().ok());
    let memory_limit = num("/sys/fs/cgroup/memory/memory.limit_in_bytes").filter(|&l| l < (1 << 62));
    let quota = read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").and_then(|v| v.trim().parse::<i64>().ok()).filter(|&q| q > 0);
    let period = num("/sys/fs/cgroup/cpu/cpu.cfs_period_us").filter(|&p| p > 0);
    let cpu_limit = quota.zip(period).map(|(q, p)| q as f64 / p as f64);
    let cpu = num("/sys/fs/cgroup/cpuacct/cpuacct.usage").map(|usage_ns| CgroupCpu { usage_ns, ..CgroupCpu::default() });
    Some(Cgroup { memory_usage: num("/sys/fs/cgroup/memory/memory.usage_in_bytes"), memory_limit, cpu_limit, cpu })
}

/// What a look found: points of series, in Datadog's names.
#[derive(Default)]
struct Points {
    ts: i64,
    out: Vec<(SeriesKey, Vec<(i64, f64)>)>,
}

impl Points {
    fn add(&mut self, metric: &str, tags: &[(&str, &str)], v: f64) {
        if v.is_finite()
            && let Ok(k) = SeriesKey::new(metric, tags.iter().copied(), Kind::Gauge)
        {
            self.out.push((k, vec![(self.ts, v)]));
        }
    }

    fn count(&mut self, metric: &str, tags: &[(&str, &str)], v: f64) {
        if v.is_finite()
            && let Ok(k) = SeriesKey::new(metric, tags.iter().copied(), Kind::Counter)
        {
            self.out.push((k, vec![(self.ts, v)]));
        }
    }
}

/// Looks every [`EVERY`] at this node, from now, on a thread of its own, and writes what it finds.
pub fn start(o: Observe, host: String, role: &'static str, every: Duration) {
    if every.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-observe-host".into()).spawn(move || {
        let mut c = Collector::new(host, role);
        loop {
            let started = Instant::now();
            let points = c.look();
            o.write(points);
            std::thread::sleep(every.saturating_sub(started.elapsed()));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "observe's host figures did not start");
    }
}

pub struct Collector {
    host: String,
    role: &'static str,
    prev: Prev,
    /// The container and pod this node runs in, if any, as tags.
    place: Vec<(String, String)>,
    #[cfg(not(target_os = "linux"))]
    sys: sysinfo::System,
    #[cfg(not(target_os = "linux"))]
    nets: sysinfo::Networks,
    #[cfg(not(target_os = "linux"))]
    disks: sysinfo::Disks,
}

impl Collector {
    pub fn new(host: String, role: &'static str) -> Self {
        let mut place = Vec::new();
        #[cfg(target_os = "linux")]
        if let Some(id) = std::fs::read_to_string("/proc/self/cgroup").ok().and_then(|c| container_id(&c)) {
            place.push(("container_id".to_owned(), id));
        }
        for (tag, var) in [("kube_namespace", "POD_NAMESPACE"), ("pod_name", "POD_NAME"), ("kube_node", "NODE_NAME")] {
            if let Ok(v) = std::env::var(var)
                && !v.is_empty()
            {
                place.push((tag.to_owned(), v));
            }
        }
        Self {
            host,
            role,
            prev: Prev::default(),
            place,
            #[cfg(not(target_os = "linux"))]
            sys: sysinfo::System::new(),
            #[cfg(not(target_os = "linux"))]
            nets: sysinfo::Networks::new_with_refreshed_list(),
            #[cfg(not(target_os = "linux"))]
            disks: sysinfo::Disks::new_with_refreshed_list(),
        }
    }

    /// One look: what changed since the last (rates, the CPU's split) and what is now.
    pub fn look(&mut self) -> Vec<(SeriesKey, Vec<(i64, f64)>)> {
        let now = Instant::now();
        let secs = self.prev.at.map(|t| now.duration_since(t).as_secs_f64()).filter(|&s| s > 0.0);
        let mut p = Points { ts: super::clock_ms(), out: Vec::new() };
        let host = self.host.clone();
        let place = std::mem::take(&mut self.place);
        let base: Vec<(&str, &str)> = std::iter::once(("host", host.as_str())).chain(place.iter().map(|(k, v)| (k.as_str(), v.as_str()))).collect();
        self.system(&mut p, &base, secs);
        self.process(&mut p, &base);
        drop(base);
        self.place = place;
        self.prev.at = Some(now);
        p.out
    }

    fn process(&self, p: &mut Points, base: &[(&str, &str)]) {
        let s = crate::host::process_self();
        let mut tags = base.to_vec();
        tags.push(("role", self.role));
        p.add("kiln.process.rss_bytes", &tags, s.rss_bytes as f64);
        p.count("kiln.process.cpu_seconds", &tags, s.cpu_seconds);
        p.add("kiln.process.open_fds", &tags, s.open_fds as f64);
        p.add("kiln.process.threads", &tags, s.threads as f64);
    }

    #[cfg(target_os = "linux")]
    fn system(&mut self, p: &mut Points, base: &[(&str, &str)], secs: Option<f64>) {
        let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
        if let Some((times, cpus)) = parse_stat(&read("/proc/stat")) {
            if let Some(prev) = &self.prev.cpu
                && let Some([user, system, iowait, idle, stolen, guest]) = cpu_percent(prev, &times)
            {
                for (m, v) in [("system.cpu.user", user), ("system.cpu.system", system), ("system.cpu.iowait", iowait), ("system.cpu.idle", idle), ("system.cpu.stolen", stolen), ("system.cpu.guest", guest)] {
                    p.add(m, base, v);
                }
            }
            p.add("system.cpu.num_cores", base, cpus as f64);
            let load = read("/proc/loadavg");
            let mut f = load.split_whitespace().map(|x| x.parse::<f64>().unwrap_or(f64::NAN));
            for m in ["system.load.1", "system.load.5", "system.load.15"] {
                let v = f.next().unwrap_or(f64::NAN);
                p.add(m, base, v);
                if cpus > 0 {
                    p.add(&format!("system.load.norm.{}", m.trim_start_matches("system.load.")), base, v / cpus as f64);
                }
            }
            self.prev.cpu = Some(times);
        }
        let mem_text = read("/proc/meminfo");
        let mem = parse_meminfo(&mem_text);
        let mib = |k: &str| mem.get(k).map(|&kb| kb as f64 / 1024.0);
        if let (Some(total), Some(free)) = (mib("MemTotal"), mib("MemFree")) {
            p.add("system.mem.total", base, total);
            p.add("system.mem.free", base, free);
            p.add("system.mem.used", base, total - free);
            if let Some(usable) = mib("MemAvailable") {
                p.add("system.mem.usable", base, usable);
                p.add("system.mem.pct_usable", base, usable / total);
            }
            if let Some(c) = mib("Cached") {
                p.add("system.mem.cached", base, c);
            }
            if let Some(b) = mib("Buffers") {
                p.add("system.mem.buffered", base, b);
            }
        }
        if let (Some(total), Some(free)) = (mib("SwapTotal"), mib("SwapFree")) {
            p.add("system.swap.total", base, total);
            p.add("system.swap.free", base, free);
            p.add("system.swap.used", base, total - free);
            if total > 0.0 {
                p.add("system.swap.pct_free", base, free / total);
            }
        }
        for (point, source, fstype) in parse_mountinfo(&read("/proc/self/mountinfo")) {
            let Ok(st) = nix::sys::statvfs::statvfs(point.as_str()) else { continue };
            let frag = st.fragment_size() as f64;
            let total = st.blocks() as f64 * frag / 1024.0;
            if total <= 0.0 {
                continue;
            }
            let free = st.blocks_available() as f64 * frag / 1024.0;
            let used = (st.blocks() - st.blocks_free()) as f64 * frag / 1024.0;
            let mut tags = base.to_vec();
            tags.extend([("device", source.as_str()), ("mount", point.as_str()), ("fs", fstype.as_str())]);
            p.add("system.disk.total", &tags, total);
            p.add("system.disk.free", &tags, free);
            p.add("system.disk.used", &tags, used);
            p.add("system.disk.in_use", &tags, used / (used + free).max(1.0));
            p.add("system.fs.inodes.total", &tags, st.files() as f64);
            p.add("system.fs.inodes.free", &tags, st.files_available() as f64);
        }
        let nets = parse_net_dev(&read("/proc/net/dev"));
        for (dev, c) in &nets {
            if let (Some(s), Some(prev)) = (secs, self.prev.nets.get(dev)) {
                let mut tags = base.to_vec();
                tags.push(("device", dev.as_str()));
                let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / s;
                p.add("system.net.bytes_rcvd", &tags, rate(prev.rx_bytes, c.rx_bytes));
                p.add("system.net.bytes_sent", &tags, rate(prev.tx_bytes, c.tx_bytes));
                p.add("system.net.packets_in.count", &tags, rate(prev.rx_packets, c.rx_packets));
                p.add("system.net.packets_out.count", &tags, rate(prev.tx_packets, c.tx_packets));
                p.add("system.net.packets_in.error", &tags, rate(prev.rx_errors, c.rx_errors));
                p.add("system.net.packets_out.error", &tags, rate(prev.tx_errors, c.tx_errors));
            }
        }
        self.prev.nets = nets.into_iter().collect();
        let disks = parse_diskstats(&read("/proc/diskstats"));
        for (dev, c) in &disks {
            if let (Some(s), Some(prev)) = (secs, self.prev.disks.get(dev)) {
                let mut tags = base.to_vec();
                tags.push(("device", dev.as_str()));
                let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / s;
                p.add("system.io.r_s", &tags, rate(prev.reads, c.reads));
                p.add("system.io.w_s", &tags, rate(prev.writes, c.writes));
                p.add("system.io.rkb_s", &tags, rate(prev.read_sectors, c.read_sectors) * 512.0 / 1024.0);
                p.add("system.io.wkb_s", &tags, rate(prev.write_sectors, c.write_sectors) * 512.0 / 1024.0);
                // Busy milliseconds a second, as a percent: iostat's %util.
                p.add("system.io.util", &tags, rate(prev.busy_ms, c.busy_ms) / 10.0);
            }
        }
        self.prev.disks = disks.into_iter().collect();
        if let Some(uptime) = read("/proc/uptime").split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
            p.add("system.uptime", base, uptime);
        }
        // The cgroup: what a container (or a unit) is allowed, and uses.
        if let Some(cg) = cgroup() {
            if let Some(u) = cg.memory_usage {
                p.add("container.memory.usage", base, u as f64);
            }
            if let Some(l) = cg.memory_limit {
                p.add("container.memory.limit", base, l as f64);
            }
            if let Some(l) = cg.cpu_limit {
                p.add("container.cpu.limit", base, l * 1e9);
            }
            if let Some(cpu) = cg.cpu {
                if let (Some(s), Some(prev)) = (secs, self.prev.cgroup_cpu) {
                    p.add("container.cpu.usage", base, cpu.usage_ns.saturating_sub(prev.usage_ns) as f64 / s);
                    p.add("container.cpu.throttled", base, cpu.throttled_ns.saturating_sub(prev.throttled_ns) as f64 / s);
                    p.add("container.cpu.throttled.periods", base, cpu.throttled_periods.saturating_sub(prev.throttled_periods) as f64);
                }
                self.prev.cgroup_cpu = Some(cpu);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn system(&mut self, p: &mut Points, base: &[(&str, &str)], secs: Option<f64>) {
        use sysinfo::System;
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        if self.prev.at.is_some() {
            let busy = f64::from(self.sys.global_cpu_usage());
            p.add("system.cpu.idle", base, 100.0 - busy);
        }
        p.add("system.cpu.num_cores", base, self.sys.cpus().len() as f64);
        let load = System::load_average();
        // Windows has no load average: sysinfo gives zeros, which are not one.
        if !cfg!(windows) {
            p.add("system.load.1", base, load.one);
            p.add("system.load.5", base, load.five);
            p.add("system.load.15", base, load.fifteen);
        }
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        let total = self.sys.total_memory();
        p.add("system.mem.total", base, mib(total));
        p.add("system.mem.free", base, mib(self.sys.free_memory()));
        p.add("system.mem.used", base, mib(self.sys.used_memory()));
        p.add("system.mem.usable", base, mib(self.sys.available_memory()));
        if total > 0 {
            p.add("system.mem.pct_usable", base, self.sys.available_memory() as f64 / total as f64);
        }
        p.add("system.swap.total", base, mib(self.sys.total_swap()));
        p.add("system.swap.free", base, mib(self.sys.free_swap()));
        p.add("system.swap.used", base, mib(self.sys.used_swap()));
        self.disks.refresh(true);
        for d in self.disks.list() {
            let total = d.total_space() as f64 / 1024.0;
            if total <= 0.0 {
                continue;
            }
            let free = d.available_space() as f64 / 1024.0;
            let point = d.mount_point().to_string_lossy().into_owned();
            let dev = d.name().to_string_lossy().into_owned();
            let fs = d.file_system().to_string_lossy().into_owned();
            let mut tags = base.to_vec();
            tags.extend([("device", dev.as_str()), ("mount", point.as_str()), ("fs", fs.as_str())]);
            p.add("system.disk.total", &tags, total);
            p.add("system.disk.free", &tags, free);
            p.add("system.disk.used", &tags, total - free);
            p.add("system.disk.in_use", &tags, (total - free) / total);
        }
        self.nets.refresh(true);
        let mut now: HashMap<String, NetCounters> = HashMap::new();
        for (name, n) in self.nets.list() {
            let lower = name.to_ascii_lowercase();
            if lower == "lo" || lower.starts_with("lo0") || lower.contains("loopback") {
                continue;
            }
            now.insert(name.clone(), NetCounters { rx_bytes: n.total_received(), rx_packets: n.total_packets_received(), rx_errors: n.total_errors_on_received(), tx_bytes: n.total_transmitted(), tx_packets: n.total_packets_transmitted(), tx_errors: n.total_errors_on_transmitted() });
        }
        for (dev, c) in &now {
            if let (Some(s), Some(prev)) = (secs, self.prev.nets.get(dev)) {
                let mut tags = base.to_vec();
                tags.push(("device", dev.as_str()));
                let rate = |a: u64, b: u64| b.saturating_sub(a) as f64 / s;
                p.add("system.net.bytes_rcvd", &tags, rate(prev.rx_bytes, c.rx_bytes));
                p.add("system.net.bytes_sent", &tags, rate(prev.tx_bytes, c.tx_bytes));
                p.add("system.net.packets_in.count", &tags, rate(prev.rx_packets, c.rx_packets));
                p.add("system.net.packets_out.count", &tags, rate(prev.tx_packets, c.tx_packets));
                p.add("system.net.packets_in.error", &tags, rate(prev.rx_errors, c.rx_errors));
                p.add("system.net.packets_out.error", &tags, rate(prev.tx_errors, c.tx_errors));
            }
        }
        self.prev.nets = now;
        p.add("system.uptime", base, System::uptime() as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_s_split_is_of_the_interval_and_guest_time_is_not_counted_twice() -> anyhow::Result<()> {
        let a = "cpu  100 0 50 800 10 0 0 0 0 0\ncpu0 50 0 25 400 5 0 0 0 0 0\ncpu1 50 0 25 400 5 0 0 0 0 0\nintr 1";
        // 200 ticks later: 60 user of which 20 guest, 20 system, 100 idle, 10 iowait, 10 steal.
        let b = "cpu  160 0 70 900 20 0 0 10 20 0\ncpu0 80 0 35 450 10 0 0 5 10 0\ncpu1 80 0 35 450 10 0 0 5 10 0";
        let (ta, cpus) = parse_stat(a).ok_or_else(|| anyhow::anyhow!("a"))?;
        let (tb, _) = parse_stat(b).ok_or_else(|| anyhow::anyhow!("b"))?;
        anyhow::ensure!(cpus == 2);
        let [user, system, iowait, idle, stolen, guest] = cpu_percent(&ta, &tb).ok_or_else(|| anyhow::anyhow!("a split"))?;
        let near = |x: f64, y: f64| (x - y).abs() < 1e-9;
        anyhow::ensure!(near(user, 20.0) && near(guest, 10.0) && near(system, 10.0) && near(idle, 50.0) && near(iowait, 5.0) && near(stolen, 5.0), "{user} {system} {iowait} {idle} {stolen} {guest}");
        anyhow::ensure!(near(user + system + iowait + idle + stolen + guest, 100.0));
        anyhow::ensure!(cpu_percent(&tb, &tb).is_none(), "no time passed: no split");
        Ok(())
    }

    #[test]
    fn proc_files_read_as_the_kernel_writes_them() -> anyhow::Result<()> {
        let mem = parse_meminfo("MemTotal:       65429768 kB\nMemFree:         1234567 kB\nMemAvailable:   40000000 kB\nHugePages_Total:       0\n");
        anyhow::ensure!(mem.get("MemTotal") == Some(&65_429_768) && mem.get("HugePages_Total") == Some(&0));
        let net = parse_net_dev("Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 100 1 0 0 0 0 0 0 100 1 0 0 0 0 0 0\n  eth0: 5000 40 2 0 0 0 0 0 7000 50 1 0 0 0 0 0\n");
        anyhow::ensure!(net == vec![("eth0".to_owned(), NetCounters { rx_bytes: 5000, rx_packets: 40, rx_errors: 2, tx_bytes: 7000, tx_packets: 50, tx_errors: 1 })], "{net:?}");
        let disks = parse_diskstats("   7       0 loop0 10 0 20 0 0 0 0 0 0 0 0\n 259       0 nvme0n1 1000 5 8000 300 2000 10 16000 900 0 1200 1500 0 0 0 0\n 259       1 nvme0n1p1 0 0 0 0 0 0 0 0 0 0 0\n");
        anyhow::ensure!(disks == vec![("nvme0n1".to_owned(), DiskCounters { reads: 1000, read_sectors: 8000, writes: 2000, write_sectors: 16000, busy_ms: 1200 })], "{disks:?}");
        let mounts = parse_mountinfo(
            "22 1 259:2 / / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw\n\
             23 22 0:21 / /proc rw - proc proc rw\n\
             24 22 0:5 / /dev/shm rw - tmpfs tmpfs rw\n\
             25 22 0:40 / /mnt/nas rw - nfs4 nas:/export rw\n\
             26 22 259:2 /home /home rw - btrfs /dev/nvme0n1p2 rw\n\
             27 22 0:50 / /mnt/my\\040disk rw - ext4 /dev/sdb1 rw\n",
        );
        let points: Vec<&str> = mounts.iter().map(|(p, _, _)| p.as_str()).collect();
        anyhow::ensure!(points == vec!["/", "/dev/shm", "/home", "/mnt/my disk"], "{points:?}");
        Ok(())
    }

    #[test]
    fn cgroups_say_a_container_s_limits_and_its_id() -> anyhow::Result<()> {
        anyhow::ensure!(parse_cpu_max("max 100000\n").is_none());
        anyhow::ensure!(parse_cpu_max("150000 100000\n") == Some(1.5));
        let s = parse_cpu_stat("usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\nnr_periods 50\nnr_throttled 7\nthrottled_usec 9000\n");
        anyhow::ensure!(s == CgroupCpu { usage_ns: 123_456_000, throttled_ns: 9_000_000, throttled_periods: 7 }, "{s:?}");
        let id = "3f1c5ad4b7e7f9e2a0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2a3";
        for line in [format!("0::/system.slice/docker-{id}.scope"), format!("0::/kubepods/burstable/pod1234/{id}"), format!("12:memory:/docker/{id}"), format!("0::/machine.slice/libpod-{id}.scope/container"), format!("0::/system.slice/crio-{id}.scope")] {
            anyhow::ensure!(container_id(&line).as_deref() == Some(id), "{line}");
        }
        anyhow::ensure!(container_id("0::/user.slice/user-1000.slice/session-2.scope").is_none());
        Ok(())
    }

    #[test]
    fn a_look_at_this_machine_gives_its_figures() -> anyhow::Result<()> {
        let mut c = Collector::new("here".into(), "test");
        let first = c.look();
        std::thread::sleep(Duration::from_millis(200));
        let second = c.look();
        let names = |v: &[(SeriesKey, Vec<(i64, f64)>)]| -> std::collections::BTreeSet<String> { v.iter().filter_map(|(k, _)| kiln_obs::batch::parse_key(&k.key).ok().map(|p| p.metric.to_owned())).collect() };
        let (a, b) = (names(&first), names(&second));
        for m in ["system.mem.total", "system.cpu.num_cores", "system.uptime", "kiln.process.rss_bytes"] {
            anyhow::ensure!(a.contains(m), "{m} in the first look: {a:?}");
        }
        // Rates and the CPU's split need two looks.
        #[cfg(target_os = "linux")]
        for m in ["system.cpu.idle", "system.cpu.user", "system.disk.total"] {
            anyhow::ensure!(b.contains(m), "{m} in the second look: {b:?}");
        }
        let _ = b;
        Ok(())
    }
}
