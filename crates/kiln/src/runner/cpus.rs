//! A leg's CPUs of its own (docs/ci.md W05-R03). In resource mode a leg asks for CPUs, and the
//! runner gives it that many of the CPUs it may run on itself (its affinity, within its cgroup's
//! cpuset), as a cpuset and not a quota: `nproc`, and the tools that size their work by it
//! (`make -j$(nproc)`, a test runner's workers), see the leg's CPUs, as on a hosted runner's
//! machine of that size, and a quota's stalls under many threads don't come. Legs at once take
//! CPUs no other leg holds while there are any; past that, the least held. Linux only: elsewhere
//! a leg sees the machine's.

use std::collections::BTreeMap;
use std::sync::LazyLock;

/// The CPUs legs hold, each with how many hold it.
#[derive(Debug, Default)]
pub struct Pool {
    held: BTreeMap<usize, usize>,
}

impl Pool {
    /// `n` of `usable`: those the fewest legs hold, the lowest first.
    pub fn take(&mut self, usable: &[usize], n: usize) -> Vec<usize> {
        let mut by: Vec<(usize, usize)> = usable.iter().map(|c| (self.held.get(c).copied().unwrap_or(0), *c)).collect();
        by.sort_unstable();
        let mut got: Vec<usize> = by.into_iter().take(n).map(|(_, c)| c).collect();
        got.sort_unstable();
        for c in &got {
            *self.held.entry(*c).or_insert(0) += 1;
        }
        got
    }

    /// `cpus`, held no more by the leg that had them.
    pub fn give(&mut self, cpus: &[usize]) {
        for c in cpus {
            if let Some(h) = self.held.get_mut(c) {
                *h = h.saturating_sub(1);
                if *h == 0 {
                    self.held.remove(c);
                }
            }
        }
    }
}

static POOL: LazyLock<parking_lot::Mutex<Pool>> = LazyLock::new(Default::default);

/// A leg's CPUs, given back when it ends.
#[derive(Debug)]
pub struct Lease {
    cpus: Vec<usize>,
}

impl Lease {
    /// As Linux and Docker write a CPU list: `0-3,8`.
    pub fn list(&self) -> String {
        list(&self.cpus)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        POOL.lock().give(&self.cpus);
    }
}

/// `n` of the runner's CPUs for a leg: none with none asked for, or where the runner can't say
/// which it may run on (not Linux).
pub fn take(n: usize) -> Option<Lease> {
    let usable = usable().filter(|u| !u.is_empty() && n > 0)?;
    Some(Lease { cpus: POOL.lock().take(&usable, n.min(usable.len())) })
}

/// The CPUs this process may run on: its `Cpus_allowed_list`, its affinity within its cgroup's
/// cpuset.
fn usable() -> Option<Vec<usize>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse(status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?.trim())
}

/// A CPU list (`0-3,8`) as its CPUs.
pub fn parse(s: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                if b < a {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse().ok()?),
        }
    }
    Some(out)
}

/// Sorted CPUs as a list of ranges: `0-3,8`.
pub fn list(cpus: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut it = cpus.iter().copied().peekable();
    while let Some(start) = it.next() {
        let mut end = start;
        while it.peek() == Some(&(end + 1)) {
            end += 1;
            it.next();
        }
        out.push(if end == start { start.to_string() } else { format!("{start}-{end}") });
    }
    out.join(",")
}

/// `taskset`, which starts a host step within its leg's CPUs (Linux).
pub fn taskset() -> Option<std::path::PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("taskset")).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W05-R03: legs at once take CPUs no other holds while there are any, then the least held;
    /// a leg's CPUs go back when it ends.
    #[test]
    fn w05_r03_legs_take_cpus_of_their_own() {
        let usable: Vec<usize> = (0..8).collect();
        let mut p = Pool::default();
        let a = p.take(&usable, 4);
        let b = p.take(&usable, 4);
        assert_eq!((a.clone(), b.clone()), (vec![0, 1, 2, 3], vec![4, 5, 6, 7]), "disjoint while they last");
        let c = p.take(&usable, 2);
        assert_eq!(c, vec![0, 1], "then shared, the least held first");
        p.give(&a);
        assert_eq!(p.take(&usable, 4), vec![0, 1, 2, 3], "given back, they're taken again: 2 and 3 free, then 0 and 1, held once");
        p.give(&b);
        p.give(&c);
        assert_eq!(p.take(&[0, 1], 4), vec![0, 1], "no more than there are");
    }

    #[test]
    fn cpu_lists_as_linux_writes_them() {
        assert_eq!(parse("0-3,8,10-11"), Some(vec![0, 1, 2, 3, 8, 10, 11]));
        assert_eq!(parse("5"), Some(vec![5]));
        assert_eq!(parse("3-1"), None);
        assert_eq!(parse("x"), None);
        assert_eq!(list(&[0, 1, 2, 3, 8, 10, 11]), "0-3,8,10-11");
        assert_eq!(list(&[7]), "7");
        assert_eq!(list(&[]), "");
    }
}
