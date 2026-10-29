//! Containers in Linux's cgroups (`docs/observe.md` §10.10): found by walking the hierarchy for the
//! scopes container engines make (Docker's, containerd's, CRI-O's, Podman's: their 64-hex ids, in
//! systemd's scopes or cgroupfs's directories, Kubernetes' pods' among them, and containers nested
//! in one, a minikube node's), and read: v2's `cpu.stat`, `cpu.max`, `memory.current`, `memory.max`,
//! `memory.stat`, `memory.events`, `io.stat`, `pids.current`; v1's per-controller files where a
//! node still mounts them. Every function takes the hierarchy's root, so a test reads a tree of its
//! own.

use std::path::{Path, PathBuf};

/// Directories walked at the most, and how deep: a node's hierarchy (systemd's slices, Kubernetes'
/// pods, a nested node's) is some ten levels and a few thousand directories.
pub const WALK: usize = 100_000;
pub const DEPTH: usize = 12;
/// USER_HZ, v1's `cpuacct.stat` unit: 100 on every Linux kiln runs on (`sysconf(_SC_CLK_TCK)`).
const USER_HZ: u64 = 100;
/// v1's "no limit": a limit this high is none (the kernel's is `PAGE_COUNTER_MAX` pages).
const NO_LIMIT: u64 = 1 << 62;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    V2,
    V1,
}

/// Which the hierarchy at `root` is: v2's unified (its `cgroup.controllers`), or v1's
/// controllers each mounted apart (a hybrid's too: its controllers are v1's).
pub fn version(root: &Path) -> Option<Version> {
    if root.join("cgroup.controllers").is_file() {
        Some(Version::V2)
    } else if root.join("memory").is_dir() {
        Some(Version::V1)
    } else {
        None
    }
}

/// A container found: its id, its engine, its cgroup's place under the root (v1: under each
/// controller), its pod's uid and QoS class when Kubernetes runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub id: String,
    pub runtime: &'static str,
    pub path: PathBuf,
    pub pod_uid: Option<String>,
    pub qos: Option<&'static str>,
}

fn hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The container a directory is, by its name (and its parent's, cgroupfs's): `docker-<id>.scope`,
/// `cri-containerd-<id>.scope`, `crio-<id>.scope`, `libpod-<id>.scope` (not their `conmon`'s), or
/// a bare id under `docker` or a pod.
pub fn identify(name: &str, parent: &str) -> Option<(String, &'static str)> {
    if let Some(scope) = name.strip_suffix(".scope") {
        for (prefix, runtime) in [("docker-", "docker"), ("cri-containerd-", "containerd"), ("crio-", "cri-o"), ("libpod-", "podman")] {
            if let Some(id) = scope.strip_prefix(prefix)
                && hex64(id)
            {
                return Some((id.to_owned(), runtime));
            }
        }
        return None;
    }
    if hex64(name) {
        let runtime = if parent == "docker" {
            "docker"
        } else if parent.starts_with("pod") || parent.contains("-pod") {
            "kubernetes"
        } else {
            return None;
        };
        return Some((name.to_owned(), runtime));
    }
    None
}

/// A pod's uid and QoS class from a directory of Kubernetes' (systemd's
/// `kubepods-burstable-pod<uid>.slice`, its uid's dashes underscores; cgroupfs's `pod<uid>` under
/// `burstable`); a guaranteed pod's sits right under `kubepods`.
fn pod_of(name: &str, parent: &str) -> Option<(String, &'static str)> {
    let qos = |s: &str| {
        if s.contains("burstable") {
            "burstable"
        } else if s.contains("besteffort") {
            "besteffort"
        } else {
            "guaranteed"
        }
    };
    if let Some(slice) = name.strip_suffix(".slice")
        && let Some(at) = slice.rfind("-pod")
        && slice.starts_with("kubepods")
    {
        let uid = slice.get(at + 4..)?.replace('_', "-");
        return (uid.len() >= 32).then(|| (uid, qos(slice)));
    }
    let uid = name.strip_prefix("pod")?;
    (uid.len() >= 32 && uid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then(|| (uid.to_owned(), qos(parent)))
}

/// Every container under `root` (v1: under its memory controller), at most `bound`; the walk itself
/// bounded ([`WALK`] directories, [`DEPTH`] deep). A container's own directory is walked too (a
/// minikube node's pods run in its) when it has directories of its own: one without is not read,
/// nor its files listed ([`leaf`]).
pub fn discover(root: &Path, v: Version, bound: usize) -> Vec<Found> {
    let base = match v {
        Version::V2 => root.to_path_buf(),
        Version::V1 => root.join("memory"),
    };
    let mut out = Vec::new();
    // The pods found on the way: a directory under one knows it by its place here.
    let mut pods: Vec<(String, &'static str)> = Vec::new();
    let mut walked = 0usize;
    // (its place under the base, its depth, its pod's place in `pods`)
    let mut stack: Vec<(PathBuf, usize, Option<usize>)> = vec![(PathBuf::new(), 0, None)];
    let mut dir = PathBuf::new();
    while let Some((rel, depth, pod)) = stack.pop() {
        if walked >= WALK || out.len() >= bound {
            break;
        }
        walked += 1;
        dir.as_mut_os_string().clear();
        dir.push(&base);
        dir.push(&rel);
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let parent = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
        for e in entries.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let here = match pod_of(name, parent) {
                Some(p) => {
                    pods.push(p);
                    Some(pods.len() - 1)
                }
                None => pod,
            };
            if let Some((id, runtime)) = identify(name, parent)
                && out.len() < bound
            {
                let p = here.and_then(|i| pods.get(i));
                let runtime = if runtime == "kubernetes" { "containerd" } else { runtime };
                out.push(Found { id, runtime, path: rel.join(name), pod_uid: p.map(|p| p.0.clone()), qos: p.map(|p| p.1) });
            }
            if depth + 1 < DEPTH && !leaf(&e) {
                stack.push((rel.join(name), depth + 1, here));
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Whether a directory has none of its own: its links two (its parent's entry and its own `.`), as
/// cgroupfs, tmpfs and ext4 count a directory's, each of its directories' `..` one more. Most of a
/// hierarchy is such (services, containers' scopes), and reading one lists its 60-odd files; a file
/// system that counts otherwise (btrfs: one) is read whatever its count.
#[cfg(unix)]
fn leaf(e: &std::fs::DirEntry) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    e.metadata().is_ok_and(|m| m.nlink() == 2)
}

#[cfg(not(unix))]
fn leaf(_: &std::fs::DirEntry) -> bool {
    false
}

/// A container's counters at a look, against which the next's rates are taken.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub cpu_ns: u64,
    pub user_ns: u64,
    pub system_ns: u64,
    pub throttled_ns: u64,
    pub throttled_periods: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    pub oom_kills: u64,
}

/// What a container uses and may at a look: bytes, cores, processes; and its counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub counters: Counters,
    pub memory_usage: Option<u64>,
    /// Its usage less the page cache it could give back (`inactive_file`): what an OOM kill reads.
    pub memory_working_set: Option<u64>,
    pub memory_rss: Option<u64>,
    pub memory_cache: Option<u64>,
    pub memory_limit: Option<u64>,
    /// Cores its quota allows.
    pub cpu_limit: Option<f64>,
    pub pids: Option<u64>,
}

/// A file's bytes read at the most: a cgroup's longest, `memory.stat`, is some 3 KiB.
const FILE_MAX: u64 = 64 << 10;

/// Cgroups' files read into one buffer, their paths made in another: a look reads every
/// container's with nothing allocated once these have grown (std's `read_to_string` of a file asks
/// its size and place first, two calls more a file, and a cgroup's files have no size to give).
#[derive(Debug, Default)]
pub struct Reader {
    path: PathBuf,
    text: String,
}

impl Reader {
    /// The text of the file `name` in the directory `dir`'s parts make (v1's root, controller and
    /// cgroup), or none when it cannot be read.
    pub fn read(&mut self, dir: &[&Path], name: &str) -> Option<&str> {
        use std::io::Read as _;
        self.path.as_mut_os_string().clear();
        for p in dir {
            self.path.push(p);
        }
        self.path.push(name);
        self.text.clear();
        let f = std::fs::File::open(&self.path).ok()?;
        f.take(FILE_MAX).read_to_string(&mut self.text).ok()?;
        Some(&self.text)
    }
}

/// `key value` lines' values, by key.
fn values(text: &str) -> impl Iterator<Item = (&str, u64)> {
    text.lines().filter_map(|l| {
        let mut f = l.split_whitespace();
        Some((f.next()?, f.next()?.parse().ok()?))
    })
}

/// The values of `keys` in `key value` lines, in one pass: a value read only for a key asked.
fn pick<const N: usize>(text: &str, keys: [&str; N]) -> [Option<u64>; N] {
    let mut out = [None; N];
    for l in text.lines() {
        let Some((k, v)) = l.split_once(' ') else { continue };
        if let Some(slot) = keys.iter().position(|w| *w == k).and_then(|i| out.get_mut(i)) {
            *slot = v.trim().parse().ok();
        }
    }
    out
}

fn num(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// v2's `io.stat`: each device's `rbytes=… wbytes=… rios=… wios=…`, summed.
pub fn parse_io_stat(text: &str) -> (u64, u64, u64, u64) {
    let (mut rb, mut wb, mut ro, mut wo) = (0u64, 0u64, 0u64, 0u64);
    for kv in text.split_whitespace() {
        let Some((k, v)) = kv.split_once('=') else { continue };
        let v: u64 = v.parse().unwrap_or(0);
        match k {
            "rbytes" => rb = rb.saturating_add(v),
            "wbytes" => wb = wb.saturating_add(v),
            "rios" => ro = ro.saturating_add(v),
            "wios" => wo = wo.saturating_add(v),
            _ => {}
        }
    }
    (rb, wb, ro, wo)
}

/// v1's `blkio.throttle.io_service_bytes` (or `…io_serviced`): each device's `Read` and `Write`,
/// summed (its `Total` lines left out).
pub fn parse_blkio(text: &str) -> (u64, u64) {
    let (mut r, mut w) = (0u64, 0u64);
    for l in text.lines() {
        let mut f = l.split_whitespace();
        if let (Some(_), Some(op), Some(v), None) = (f.next(), f.next(), f.next(), f.next()) {
            let v: u64 = v.parse().unwrap_or(0);
            match op {
                "Read" => r = r.saturating_add(v),
                "Write" => w = w.saturating_add(v),
                _ => {}
            }
        }
    }
    (r, w)
}

/// A found container's stats, read from its cgroup (none when it is gone).
pub fn stats(root: &Path, v: Version, f: &Found, r: &mut Reader) -> Option<Stats> {
    match v {
        Version::V2 => stats_v2(&[root, &f.path], r),
        Version::V1 => stats_v1(root, &f.path, r),
    }
}

/// Whether a pod's container is its sandbox, the `pause` process Kubernetes' runtimes start to hold
/// a pod's namespaces (Datadog's check leaves it out): its first process's name, as `proc` says;
/// none when it has no process, or one this node's `/proc` does not show (another PID namespace's).
pub fn sandbox(root: &Path, v: Version, f: &Found, proc: &Path, r: &mut Reader) -> Option<bool> {
    let procs = match v {
        Version::V2 => r.read(&[root, &f.path], "cgroup.procs"),
        Version::V1 => r.read(&[root, Path::new("memory"), &f.path], "cgroup.procs"),
    }?;
    let pid: u32 = procs.lines().next()?.trim().parse().ok().filter(|&p| p > 0)?;
    let comm = r.read(&[proc, Path::new(&pid.to_string())], "comm")?;
    Some(comm.trim_end() == "pause")
}

fn stats_v2(dir: &[&Path], r: &mut Reader) -> Option<Stats> {
    let mut c = Counters::default();
    for (k, v) in values(r.read(dir, "cpu.stat")?) {
        match k {
            "usage_usec" => c.cpu_ns = v.saturating_mul(1_000),
            "user_usec" => c.user_ns = v.saturating_mul(1_000),
            "system_usec" => c.system_ns = v.saturating_mul(1_000),
            "throttled_usec" => c.throttled_ns = v.saturating_mul(1_000),
            "nr_throttled" => c.throttled_periods = v,
            _ => {}
        }
    }
    if let Some(io) = r.read(dir, "io.stat") {
        (c.read_bytes, c.write_bytes, c.read_ops, c.write_ops) = parse_io_stat(io);
    }
    let [oom_kill] = r.read(dir, "memory.events").map_or([None], |e| pick(e, ["oom_kill"]));
    c.oom_kills = oom_kill.unwrap_or(0);
    let usage = r.read(dir, "memory.current").and_then(num);
    let [anon, file, inactive] = r.read(dir, "memory.stat").map_or([None; 3], |s| pick(s, ["anon", "file", "inactive_file"]));
    let memory_limit = r.read(dir, "memory.max").and_then(num);
    let cpu_limit = r.read(dir, "cpu.max").and_then(super::super::host::parse_cpu_max);
    let pids = r.read(dir, "pids.current").and_then(num);
    Some(Stats { counters: c, memory_usage: usage, memory_working_set: usage.map(|u| u.saturating_sub(inactive.unwrap_or(0))), memory_rss: anon, memory_cache: file, memory_limit, cpu_limit, pids })
}

fn stats_v1(root: &Path, rel: &Path, r: &mut Reader) -> Option<Stats> {
    let (cpuacct, cpu, blkio, memory, pids) = (Path::new("cpuacct"), Path::new("cpu"), Path::new("blkio"), Path::new("memory"), Path::new("pids"));
    let mut c = Counters { cpu_ns: r.read(&[root, cpuacct, rel], "cpuacct.usage").and_then(num)?, ..Counters::default() };
    if let Some(s) = r.read(&[root, cpuacct, rel], "cpuacct.stat") {
        for (k, v) in values(s) {
            let ns = v.saturating_mul(1_000_000_000 / USER_HZ);
            match k {
                "user" => c.user_ns = ns,
                "system" => c.system_ns = ns,
                _ => {}
            }
        }
    }
    if let Some(s) = r.read(&[root, cpu, rel], "cpu.stat") {
        for (k, v) in values(s) {
            match k {
                "throttled_time" => c.throttled_ns = v,
                "nr_throttled" => c.throttled_periods = v,
                _ => {}
            }
        }
    }
    if let Some(b) = r.read(&[root, blkio, rel], "blkio.throttle.io_service_bytes") {
        (c.read_bytes, c.write_bytes) = parse_blkio(b);
    }
    if let Some(b) = r.read(&[root, blkio, rel], "blkio.throttle.io_serviced") {
        (c.read_ops, c.write_ops) = parse_blkio(b);
    }
    let [oom_kill] = r.read(&[root, memory, rel], "memory.oom_control").map_or([None], |e| pick(e, ["oom_kill"]));
    c.oom_kills = oom_kill.unwrap_or(0);
    let [total_inactive, inactive, rss, cache] = r.read(&[root, memory, rel], "memory.stat").map_or([None; 4], |s| pick(s, ["total_inactive_file", "inactive_file", "rss", "cache"]));
    let usage = r.read(&[root, memory, rel], "memory.usage_in_bytes").and_then(num);
    let quota = r.read(&[root, cpu, rel], "cpu.cfs_quota_us").and_then(|q| q.trim().parse::<i64>().ok()).filter(|&q| q > 0);
    let period = r.read(&[root, cpu, rel], "cpu.cfs_period_us").and_then(num).filter(|&p| p > 0);
    let memory_limit = r.read(&[root, memory, rel], "memory.limit_in_bytes").and_then(num).filter(|&l| l < NO_LIMIT);
    let pids = r.read(&[root, pids, rel], "pids.current").and_then(num);
    Some(Stats {
        counters: c,
        memory_usage: usage,
        memory_working_set: usage.map(|u| u.saturating_sub(total_inactive.or(inactive).unwrap_or(0))),
        memory_rss: rss,
        memory_cache: cache,
        memory_limit,
        cpu_limit: quota.zip(period).map(|(q, p)| q as f64 / p as f64),
        pids,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const A: &str = "56bca3739fe8bd187d49969e4a6a2af672a186b7830f64c9d7fd41c88ec8fdff";
    const B: &str = "60bf665dc3471d8b4526c38ccb40a0072a4736683a8554c2713ac8f963056d89";
    const C: &str = "774edafa40abbe1f8b3c7902d0e11c473a395eda0199044df7115090c8166bff";
    const D: &str = "0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";

    fn file(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// v2's hierarchy as a node has it: a Docker container in systemd's scope (and its podman
    /// `conmon`, not one), a Kubernetes pod's containerd container (its uid and class), a minikube
    /// node's pod nested in its container; each read as the kernel writes its files.
    #[test]
    fn containers_are_found_in_v2_s_hierarchy_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        file(r, "cgroup.controllers", "cpuset cpu io memory pids");
        let docker = format!("system.slice/docker-{A}.scope");
        file(r, &format!("{docker}/cpu.stat"), "usage_usec 2500000\nuser_usec 2000000\nsystem_usec 500000\nnr_periods 40\nnr_throttled 3\nthrottled_usec 120000\n");
        file(r, &format!("{docker}/cpu.max"), "50000 100000\n");
        file(r, &format!("{docker}/memory.current"), "104857600\n");
        file(r, &format!("{docker}/memory.max"), "268435456\n");
        file(r, &format!("{docker}/memory.stat"), "anon 52428800\nfile 41943040\ninactive_file 20971520\n");
        file(r, &format!("{docker}/memory.events"), "low 0\nhigh 0\nmax 2\noom 1\noom_kill 1\n");
        file(r, &format!("{docker}/io.stat"), "8:0 rbytes=4096 wbytes=8192 rios=1 wios=2 dbytes=0 dios=0\n259:0 rbytes=100 wbytes=200 rios=3 wios=4 dbytes=0 dios=0\n");
        file(r, &format!("{docker}/pids.current"), "7\n");
        file(r, &format!("machine.slice/libpod-conmon-{B}.scope/cpu.stat"), "usage_usec 1\n");
        let pod = "kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1f2e3d4c_5b6a_7980_a1b2_c3d4e5f60718.slice";
        file(r, &format!("{pod}/cri-containerd-{C}.scope/cpu.stat"), "usage_usec 10\n");
        file(r, &format!("{pod}/cri-containerd-{C}.scope/memory.max"), "max\n");
        file(r, &format!("{docker}/kubepods/besteffort/pod9a8b7c6d-5e4f-3a2b-1c0d-ffeeddccbbaa/{D}/cpu.stat"), "usage_usec 20\n");
        assert_eq!(version(r), Some(Version::V2));
        let found = discover(r, Version::V2, 100);
        let by = |id: &str| found.iter().find(|f| f.id == id).unwrap();
        assert_eq!(found.len(), 3, "{found:#?}");
        assert_eq!((by(A).runtime, by(A).pod_uid.as_deref()), ("docker", None));
        assert_eq!((by(C).runtime, by(C).pod_uid.as_deref(), by(C).qos), ("containerd", Some("1f2e3d4c-5b6a-7980-a1b2-c3d4e5f60718"), Some("burstable")));
        assert_eq!((by(D).pod_uid.as_deref(), by(D).qos), (Some("9a8b7c6d-5e4f-3a2b-1c0d-ffeeddccbbaa"), Some("besteffort")), "a nested node's pod");
        let mut rd = Reader::default();
        let s = stats(r, Version::V2, by(A), &mut rd).unwrap();
        assert_eq!(
            s.counters,
            Counters {
                cpu_ns: 2_500_000_000,
                user_ns: 2_000_000_000,
                system_ns: 500_000_000,
                throttled_ns: 120_000_000,
                throttled_periods: 3,
                read_bytes: 4196,
                write_bytes: 8392,
                read_ops: 4,
                write_ops: 6,
                oom_kills: 1
            }
        );
        assert_eq!((s.memory_usage, s.memory_working_set, s.memory_rss, s.memory_cache, s.memory_limit), (Some(104_857_600), Some(83_886_080), Some(52_428_800), Some(41_943_040), Some(268_435_456)));
        assert_eq!((s.cpu_limit, s.pids), (Some(0.5), Some(7)));
        let s = stats(r, Version::V2, by(C), &mut rd).unwrap();
        assert_eq!((s.memory_limit, s.cpu_limit, s.pids), (None, None, None), "no limit, files absent");
        assert_eq!(discover(r, Version::V2, 1).len(), 1, "bounded");
    }

    /// v1's controllers mounted apart, cgroupfs's names (`docker/<id>`, Kubernetes'
    /// `kubepods/burstable/pod<uid>/<id>`): found under the memory controller, read from each.
    #[test]
    fn containers_are_found_in_v1_s_controllers_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let rel = format!("docker/{A}");
        file(r, &format!("memory/{rel}/memory.usage_in_bytes"), "2097152\n");
        file(r, &format!("memory/{rel}/memory.limit_in_bytes"), "9223372036854771712\n");
        file(r, &format!("memory/{rel}/memory.stat"), "cache 1048576\nrss 524288\ntotal_inactive_file 262144\n");
        file(r, &format!("memory/{rel}/memory.oom_control"), "oom_kill_disable 0\nunder_oom 0\noom_kill 2\n");
        file(r, &format!("cpuacct/{rel}/cpuacct.usage"), "3000000000\n");
        file(r, &format!("cpuacct/{rel}/cpuacct.stat"), "user 150\nsystem 50\n");
        file(r, &format!("cpu/{rel}/cpu.stat"), "nr_periods 10\nnr_throttled 2\nthrottled_time 5000000\n");
        file(r, &format!("cpu/{rel}/cpu.cfs_quota_us"), "200000\n");
        file(r, &format!("cpu/{rel}/cpu.cfs_period_us"), "100000\n");
        file(r, &format!("blkio/{rel}/blkio.throttle.io_service_bytes"), "8:0 Read 1024\n8:0 Write 2048\n8:0 Total 3072\nTotal 3072\n");
        file(r, &format!("blkio/{rel}/blkio.throttle.io_serviced"), "8:0 Read 1\n8:0 Write 2\n8:0 Total 3\n");
        file(r, &format!("pids/{rel}/pids.current"), "3\n");
        file(r, &format!("memory/kubepods/burstable/pod1f2e3d4c-5b6a-7980-a1b2-c3d4e5f60718/{C}/memory.usage_in_bytes"), "1\n");
        assert_eq!(version(r), Some(Version::V1));
        let found = discover(r, Version::V1, 100);
        assert_eq!(found.iter().map(|f| (f.id.as_str(), f.runtime, f.qos)).collect::<Vec<_>>(), [(A, "docker", None), (C, "containerd", Some("burstable"))]);
        let mut rd = Reader::default();
        let s = stats(r, Version::V1, &found[0], &mut rd).unwrap();
        assert_eq!(
            s.counters,
            Counters {
                cpu_ns: 3_000_000_000,
                user_ns: 1_500_000_000,
                system_ns: 500_000_000,
                throttled_ns: 5_000_000,
                throttled_periods: 2,
                read_bytes: 1024,
                write_bytes: 2048,
                read_ops: 1,
                write_ops: 2,
                oom_kills: 2
            }
        );
        assert_eq!(
            (s.memory_usage, s.memory_working_set, s.memory_rss, s.memory_cache, s.memory_limit),
            (Some(2_097_152), Some(1_835_008), Some(524_288), Some(1_048_576), None),
            "v1's no limit is none"
        );
        assert_eq!((s.cpu_limit, s.pids), (Some(2.0), Some(3)));
        assert!(stats(r, Version::V1, &found[1], &mut rd).is_none(), "no cpuacct, nothing read");
        assert_eq!(identify("crio-conmon-abc.scope", ""), None);
        assert_eq!(identify(A, "system.slice"), None, "a bare id is cgroupfs's under docker or a pod alone");
    }
}
