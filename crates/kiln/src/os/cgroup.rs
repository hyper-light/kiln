//! The memory of this process's cgroup (its container, under Kubernetes or Docker) as the kernel
//! counts it when it picks what to kill: what it uses, its limit, its peak, and how many processes
//! in it the OOM killer killed. A server killed inside its container (kiln's supervisor starts it
//! again there) doesn't show in the pod's status; it shows here (docs/cd-targets.md §10).
//!
//! - cgroup v2: `memory.current` less `inactive_file` (of `memory.stat`), the working set as the
//!   kubelet counts it; `memory.max`; `memory.peak`; `oom_kill` of `memory.events`.
//! - cgroup v1: `memory.usage_in_bytes` less `total_inactive_file`; `memory.limit_in_bytes`;
//!   `memory.max_usage_in_bytes`; `oom_kill` of `memory.oom_control`.
//! - Elsewhere: none.

use serde::Serialize;

/// A cgroup's memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Memory {
    /// Bytes in use, less file cache the kernel can drop: the working set.
    pub used: u64,
    /// Its limit; none when it has none.
    pub limit: Option<u64>,
    /// The most it ever used (with cache), when the kernel says.
    pub peak: Option<u64>,
    /// Processes in it the OOM killer killed.
    pub oom_kills: u64,
}

impl Memory {
    /// `used` as a share of `limit` (0 to 1), to three places; none without a limit.
    pub fn share(&self) -> Option<f64> {
        self.limit.filter(|l| *l > 0).map(|l| (self.used as f64 / l as f64 * 1000.0).round() / 1000.0)
    }
}

/// A limit as a cgroup writes it: `max` (v2), or v1's page-rounded `i64::MAX`, is none.
fn limit(text: &str) -> Option<u64> {
    let t = text.trim();
    if t == "max" {
        return None;
    }
    t.parse::<u64>().ok().filter(|l| *l < 1 << 60)
}

/// A `key value` line's value, of a flat keyed file (`memory.stat`, `memory.events`).
fn keyed(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|l| l.split_once(' ').filter(|(k, _)| *k == key).and_then(|(_, v)| v.trim().parse().ok()))
}

/// cgroup v2's files: `memory.current`, `memory.max`, `memory.stat`, `memory.events` and (since
/// Linux 5.19) `memory.peak`.
pub fn from_v2(current: &str, max: &str, stat: &str, events: &str, peak: Option<&str>) -> Option<Memory> {
    let current: u64 = current.trim().parse().ok()?;
    let inactive = keyed(stat, "inactive_file").unwrap_or(0);
    Some(Memory {
        used: current.saturating_sub(inactive),
        limit: limit(max),
        peak: peak.and_then(|p| p.trim().parse().ok()),
        oom_kills: keyed(events, "oom_kill").unwrap_or(0),
    })
}

/// cgroup v1's files: `memory.usage_in_bytes`, `memory.limit_in_bytes`, `memory.stat`,
/// `memory.oom_control` and `memory.max_usage_in_bytes`.
pub fn from_v1(usage: &str, limit_in_bytes: &str, stat: &str, oom_control: &str, max_usage: Option<&str>) -> Option<Memory> {
    let usage: u64 = usage.trim().parse().ok()?;
    let inactive = keyed(stat, "total_inactive_file").unwrap_or(0);
    Some(Memory {
        used: usage.saturating_sub(inactive),
        limit: limit(limit_in_bytes),
        peak: max_usage.and_then(|p| p.trim().parse().ok()),
        oom_kills: keyed(oom_control, "oom_kill").unwrap_or(0),
    })
}

/// This process's cgroup's memory; none where there's no cgroup to read (macOS, Windows, a
/// process in the root cgroup).
pub fn memory() -> Option<Memory> {
    imp::memory()
}

/// The most memory this process may use as its cgroups hold it: the least limit of its own
/// cgroup and those above it (a slice's `MemoryMax=` holds the services under it, whose own is
/// `max`); none without one, or off Linux.
pub fn memory_limit() -> Option<u64> {
    imp::memory_limit()
}

/// cgroup v2: the least `memory.max` of `dir` and the cgroups above it, up to `root` (the
/// hierarchy's mount, a container's own cgroup namespace's root among them).
#[cfg(any(target_os = "linux", test))]
fn least_limit(root: &std::path::Path, dir: &std::path::Path) -> Option<u64> {
    dir.ancestors().take_while(|d| d.starts_with(root)).filter_map(|d| std::fs::read_to_string(d.join("memory.max")).ok()).filter_map(|t| limit(&t)).min()
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs::read_to_string;
    use std::path::{Path, PathBuf};

    /// The cgroup v2 directory this process is in: `/proc/self/cgroup`'s `0::<path>` under
    /// `/sys/fs/cgroup` (`/` in a container with its own cgroup namespace).
    fn v2_dir() -> Option<PathBuf> {
        let own = read_to_string("/proc/self/cgroup").ok()?;
        let path = own.lines().find_map(|l| l.strip_prefix("0::"))?;
        Some(Path::new("/sys/fs/cgroup").join(path.trim().trim_start_matches('/')))
    }

    pub fn memory() -> Option<super::Memory> {
        if let Some(dir) = v2_dir().filter(|d| d.join("memory.current").exists()) {
            let f = |n: &str| read_to_string(dir.join(n)).ok();
            return super::from_v2(&f("memory.current")?, &f("memory.max").unwrap_or_default(), &f("memory.stat").unwrap_or_default(), &f("memory.events").unwrap_or_default(), f("memory.peak").as_deref());
        }
        let dir = Path::new("/sys/fs/cgroup/memory");
        let f = |n: &str| read_to_string(dir.join(n)).ok();
        super::from_v1(&f("memory.usage_in_bytes")?, &f("memory.limit_in_bytes").unwrap_or_default(), &f("memory.stat").unwrap_or_default(), &f("memory.oom_control").unwrap_or_default(), f("memory.max_usage_in_bytes").as_deref())
    }

    pub fn memory_limit() -> Option<u64> {
        if let Some(dir) = v2_dir().filter(|d| d.join("memory.current").exists()) {
            return super::least_limit(Path::new("/sys/fs/cgroup"), &dir);
        }
        super::limit(&read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes").ok()?)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn memory() -> Option<super::Memory> {
        None
    }

    pub fn memory_limit() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_containers_memory_from_cgroup_v2() {
        // As kiln's pod read on 2026-10-18 while its server was killed every minute.
        let stat = "anon 1803550720\nfile 412000256\nkernel 31000000\nactive_file 120000000\ninactive_file 292000256\n";
        let events = "low 0\nhigh 0\nmax 1290\noom 41\noom_kill 41\noom_group_kill 0\n";
        let m = from_v2("2215550976\n", "2147483648\n", stat, events, Some("2147483648\n"));
        assert_eq!(m, Some(Memory { used: 2215550976 - 292000256, limit: Some(2147483648), peak: Some(2147483648), oom_kills: 41 }));
        assert_eq!(m.and_then(|m| m.share()), Some(0.896));
        // No limit, and a kernel without memory.peak.
        let m = from_v2("104857600", "max\n", "inactive_file 4857600\n", "oom_kill 0\n", None);
        assert_eq!(m, Some(Memory { used: 100_000_000, limit: None, peak: None, oom_kills: 0 }));
        assert_eq!(m.and_then(|m| m.share()), None);
        assert_eq!(from_v2("", "max", "", "", None), None);
    }

    #[test]
    fn a_containers_memory_from_cgroup_v1() {
        let m = from_v1("536870912\n", "9223372036854771712\n", "cache 100\ntotal_inactive_file 36870912\n", "oom_kill_disable 0\nunder_oom 0\noom_kill 2\n", Some("1073741824"));
        assert_eq!(m, Some(Memory { used: 500_000_000, limit: None, peak: Some(1073741824), oom_kills: 2 }));
        let m = from_v1("536870912", "1073741824", "", "", None);
        assert_eq!(m.as_ref().map(|m| m.limit), Some(Some(1073741824)));
        assert_eq!(m.and_then(|m| m.share()), Some(0.5));
    }

    /// W05-R03: the most a runner may use is the least limit above it: its scope's over its own
    /// `max`, a container's at its own root, none where nothing sets one.
    #[test]
    fn w05_r03_a_runners_memory_is_the_least_limit_above_it() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("host");
        let own = root.join("user.slice/kiln-lab.scope/kiln");
        std::fs::create_dir_all(&own)?;
        std::fs::write(root.join("user.slice/memory.max"), "max\n")?;
        std::fs::write(root.join("user.slice/kiln-lab.scope/memory.max"), "42949672960\n")?;
        std::fs::write(own.join("memory.max"), "max\n")?;
        assert_eq!(least_limit(&root, &own), Some(42949672960), "the scope's, over its own max");
        std::fs::write(own.join("memory.max"), "1073741824\n")?;
        assert_eq!(least_limit(&root, &own), Some(1073741824), "the least of them");
        let ns = tmp.path().join("container");
        std::fs::create_dir_all(&ns)?;
        std::fs::write(ns.join("memory.max"), "17179869184\n")?;
        assert_eq!(least_limit(&ns, &ns), Some(17179869184), "a container's own cgroup namespace");
        let none = tmp.path().join("none");
        std::fs::create_dir_all(none.join("a"))?;
        std::fs::write(none.join("a/memory.max"), "max\n")?;
        assert_eq!(least_limit(&none, &none.join("a")), None);
        Ok(())
    }
}
