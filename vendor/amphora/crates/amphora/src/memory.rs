//! The memory this process may use, as the system says: the smallest memory limit of the control
//! groups it runs in (cgroup v2, else v1) and the machine's own memory, on Linux. Elsewhere nothing
//! is said, and a caller takes its stated fallback (docs/design/store.md, decision 26).
//!
//! The parsers are pure, so tests give them each file's text; [`limit`] reads the files.

/// A cgroup v1 limit at or past this is "no limit" (the kernel reports its page-rounded
/// `LONG_MAX`, 9,223,372,036,854,771,712 on 4 KiB pages).
const V1_UNLIMITED: u64 = 1 << 62;

/// The path of this process's cgroup v2 group, from `/proc/self/cgroup`'s `0::` line.
#[must_use]
pub fn v2_group(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
}

/// The path of this process's cgroup v1 memory group, from `/proc/self/cgroup`'s line whose
/// controllers include `memory`.
#[must_use]
pub fn v1_group(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let (_, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
        controllers
            .split(',')
            .any(|c| c == "memory")
            .then(|| path.trim())
    })
}

/// A cgroup's memory limit in bytes from its `memory.max` (v2) or `memory.limit_in_bytes` (v1):
/// None for "max" and for v1's no-limit value.
#[must_use]
pub fn group_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    text.parse::<u64>().ok().filter(|&n| n < V1_UNLIMITED)
}

/// The machine's memory in bytes, from `/proc/meminfo`'s `MemTotal:` line (in KiB).
#[must_use]
pub fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kib = line.split_whitespace().next()?.parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

/// Each group from `path` up to the hierarchy's root, deepest first: `/a/b` gives `/a/b`, `/a` and
/// `/` (a limit anywhere above a group bounds it too).
#[must_use]
pub fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = path.trim_end_matches('/');
    while !at.is_empty() {
        out.push(at.to_owned());
        at = match at.rfind('/') {
            Some(0) | None => "",
            Some(cut) => at.get(..cut).unwrap_or_default(),
        };
    }
    out.push("/".to_owned());
    out
}

/// The memory this process may use, in bytes: the smallest of its groups' limits and the machine's
/// memory. None where the system doesn't say (not Linux, or nothing readable).
#[cfg(target_os = "linux")]
#[must_use]
pub fn limit() -> Option<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let cgroup = read("/proc/self/cgroup").unwrap_or_default();
    let mut limits: Vec<u64> = Vec::new();
    if let Some(group) = v2_group(&cgroup) {
        for at in ancestors(group) {
            let dir = format!("/sys/fs/cgroup{}", if at == "/" { "" } else { at.as_str() });
            if let Some(n) = read(&format!("{dir}/memory.max"))
                .as_deref()
                .and_then(group_limit)
            {
                limits.push(n);
            }
        }
    }
    if let Some(group) = v1_group(&cgroup) {
        for at in ancestors(group) {
            let dir = format!(
                "/sys/fs/cgroup/memory{}",
                if at == "/" { "" } else { at.as_str() }
            );
            if let Some(n) = read(&format!("{dir}/memory.limit_in_bytes"))
                .as_deref()
                .and_then(group_limit)
            {
                limits.push(n);
            }
        }
    }
    if let Some(n) = read("/proc/meminfo").as_deref().and_then(mem_total) {
        limits.push(n);
    }
    limits.into_iter().min()
}

/// Not Linux: the system isn't asked (its interfaces need code this crate doesn't allow), so the
/// caller's fallback holds.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn limit() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: parse a cgroup v2 process's `/proc/self/cgroup`, a v1 process's, and a hybrid's. Expect:
    /// each memory group's path, none where there is none (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_process_finds_its_memory_group_in_either_hierarchy() {
        let v2 = "0::/user.slice/user-1000.slice/app.slice/amphora.service\n";
        assert_eq!(
            v2_group(v2),
            Some("/user.slice/user-1000.slice/app.slice/amphora.service")
        );
        assert_eq!(v1_group(v2), None);
        let v1 = "12:pids:/a\n11:memory:/docker/abc\n10:cpu,cpuacct:/docker/abc\n";
        assert_eq!(v1_group(v1), Some("/docker/abc"));
        assert_eq!(v2_group(v1), None);
        let hybrid = "11:memory:/k8s/pod1\n0::/k8s/pod1\n";
        assert_eq!(v1_group(hybrid), Some("/k8s/pod1"));
        assert_eq!(v2_group(hybrid), Some("/k8s/pod1"));
        assert_eq!(v2_group("0::/\n"), Some("/"));
    }

    /// Do: read limits as the kernel writes them. Expect: bytes for a number, none for "max" and
    /// for v1's no-limit value, none for text that isn't one (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_limit_is_a_number_of_bytes_or_none() {
        assert_eq!(group_limit("34359738368\n"), Some(34_359_738_368));
        assert_eq!(group_limit("max\n"), None);
        assert_eq!(group_limit("9223372036854771712\n"), None);
        assert_eq!(group_limit("garbage"), None);
        let meminfo = "MemTotal:       131072000 kB\nMemFree:        1000 kB\n";
        assert_eq!(mem_total(meminfo), Some(131_072_000 * 1024));
        assert_eq!(mem_total("MemFree: 1 kB\n"), None);
    }

    /// Do: list the groups above `/a/b/c`, `/a` and the root. Expect: deepest first, the root last,
    /// the root once (AC-T1.13.4: reads at their latency).
    #[test]
    fn a_groups_limit_is_bounded_by_every_group_above_it() {
        assert_eq!(ancestors("/a/b/c"), ["/a/b/c", "/a/b", "/a", "/"]);
        assert_eq!(ancestors("/a"), ["/a", "/"]);
        assert_eq!(ancestors("/"), ["/"]);
        assert_eq!(ancestors(""), ["/"]);
    }

    /// Do: ask this process's limit on Linux. Expect: some number of bytes, no more than the
    /// machine's memory (AC-T1.13.4: reads at their latency).
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_limit_no_larger_than_its_machine() {
        let machine = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .as_deref()
            .and_then(mem_total);
        let got = limit();
        assert!(got.is_some());
        assert!(got <= machine);
    }
}
