//! The server's CPU and memory through a window, as the OS counts them: its whole process tree's (a server that
//! runs under a wrapper or starts helpers is all of them), and its root process's alone. Linux reads `/proc`;
//! macOS asks `ps` (no `/proc` there, and `ps` needs nothing of the lab's own); Windows isn't measured.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// How often the server is sampled. Linux: `/proc` counts CPU in clock ticks (`USER_HZ`, 100 a second), so a
/// tenth of a second holds ten ticks of a busy CPU. macOS: `ps` is a process of its own (milliseconds of CPU each
/// time), so once a second, ten samples in the shortest window (`super::MIN_WINDOW_S`).
pub const EVERY: Duration = if cfg!(target_os = "linux") { Duration::from_millis(100) } else { Duration::from_secs(1) };

/// What the OS said of the tree at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Seconds on the CPU since each process started, all of the tree's.
    pub cpu_s: f64,
    pub rss: u64,
    pub procs: usize,
    pub root_cpu_s: f64,
    pub root_rss: u64,
}

/// A process: its parent, its CPU seconds and its resident bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Proc {
    ppid: u32,
    cpu_s: f64,
    rss: u64,
}

/// `root` and every process under it, from each process's parent.
fn tree(all: &BTreeMap<u32, Proc>, root: u32) -> BTreeSet<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (pid, p) in all {
        children.entry(p.ppid).or_default().push(*pid);
    }
    let mut out = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if all.contains_key(&p) && out.insert(p) {
            stack.extend(children.get(&p).into_iter().flatten().copied());
        }
    }
    out
}

fn usage(all: &BTreeMap<u32, Proc>, pids: &BTreeSet<u32>, root: u32) -> Usage {
    let mut u = Usage::default();
    for pid in pids {
        if let Some(p) = all.get(pid) {
            u.cpu_s += p.cpu_s;
            u.rss += p.rss;
            u.procs += 1;
        }
    }
    if let Some(r) = all.get(&root) {
        (u.root_cpu_s, u.root_rss) = (r.cpu_s, r.rss);
    }
    u
}

/// A Linux `/proc/<pid>/stat` line: its parent, CPU seconds (`utime` and `stime`, in `ticks`) and resident bytes
/// (`rss`, in pages of `page`). The command's name (in parentheses) may hold spaces and parentheses: the fields
/// are counted from its last `)`.
fn proc_stat(line: &str, ticks: f64, page: u64) -> Option<Proc> {
    let after = line.get(line.rfind(')')? + 1..)?;
    let f: Vec<&str> = after.split_whitespace().collect();
    // Fields from the 3rd (state) on: ppid is the 4th, utime the 14th, stime the 15th, rss the 24th.
    let field = |n: usize| f.get(n - 3).copied();
    let ppid = field(4)?.parse().ok()?;
    let ticks_used = field(14)?.parse::<u64>().ok()? + field(15)?.parse::<u64>().ok()?;
    let rss_pages = field(24)?.parse::<i64>().ok()?.max(0).unsigned_abs();
    #[allow(clippy::cast_precision_loss)]
    let cpu_s = ticks_used as f64 / ticks;
    Some(Proc { ppid, cpu_s, rss: rss_pages * page })
}

/// `ps`'s CPU time (`[[dd-]hh:]mm:ss[.ss]`) in seconds.
fn ps_time(t: &str) -> Option<f64> {
    let (days, rest) = match t.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, t),
    };
    let parts: Vec<f64> = rest.split(':').map(|p| p.parse::<f64>().ok()).collect::<Option<_>>()?;
    let secs = parts.iter().fold(0.0, |acc, p| acc * 60.0 + p);
    (parts.len() <= 3).then_some(days * 86_400.0 + secs)
}

/// `ps -A -o pid=,ppid=,rss=,time=`'s lines (resident memory in KiB).
fn ps_lines(text: &str) -> BTreeMap<u32, Proc> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let pid = w.next()?.parse().ok()?;
            let ppid = w.next()?.parse().ok()?;
            let rss_kib: u64 = w.next()?.parse().ok()?;
            let cpu_s = ps_time(w.next()?)?;
            Some((pid, Proc { ppid, cpu_s, rss: rss_kib * 1024 }))
        })
        .collect()
}

/// Every process the OS will say of now (Linux: those in `pids`, or all when none are given).
fn processes(pids: Option<&BTreeSet<u32>>) -> BTreeMap<u32, Proc> {
    if cfg!(target_os = "linux") {
        #[cfg(unix)]
        let (ticks, page) = (super::os::clock_ticks().max(1), super::os::page_size());
        #[cfg(not(unix))]
        let (ticks, page) = (100, 4096);
        #[allow(clippy::cast_precision_loss)]
        let ticks = ticks as f64;
        let read = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|l| proc_stat(&l, ticks, page)).map(|p| (pid, p));
        match pids {
            Some(pids) => pids.iter().filter_map(|p| read(*p)).collect(),
            None => std::fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok()).filter_map(read).collect(),
        }
    } else if cfg!(unix) {
        std::process::Command::new("ps").args(["-A", "-o", "pid=,ppid=,rss=,time="]).output().map(|o| ps_lines(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
    } else {
        BTreeMap::new()
    }
}

/// A server's tree, sampled.
pub struct Tree {
    root: u32,
    /// Linux: the tree as last found (a sample reads only these); found again at each window's start.
    pids: BTreeSet<u32>,
}

impl Tree {
    pub fn of(root: u32) -> Self {
        let mut t = Self { root, pids: BTreeSet::new() };
        t.find();
        t
    }

    /// Finds the tree again (Linux; a sample elsewhere lists every process anyway).
    pub fn find(&mut self) {
        if cfg!(target_os = "linux") {
            self.pids = tree(&processes(None), self.root);
        }
    }

    /// The tree's usage now; none where the OS won't say, or the server is gone.
    pub fn now(&self) -> Option<Usage> {
        let u = if cfg!(target_os = "linux") {
            let all = processes(Some(&self.pids));
            usage(&all, &self.pids, self.root)
        } else {
            let all = processes(None);
            usage(&all, &tree(&all, self.root), self.root)
        };
        (u.procs > 0).then_some(u)
    }
}

/// The server's CPU and memory over a window.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Window {
    /// CPUs' worth of time it took, the window's mean (1.0: one CPU the whole window), and the most in any
    /// sampling interval; its root process's mean.
    pub cpu_share: Option<f64>,
    pub cpu_share_max: Option<f64>,
    pub root_cpu_share: Option<f64>,
    /// Resident memory, MB: the samples' mean and most; its root process's most.
    pub rss_mb: Option<f64>,
    pub rss_mb_max: Option<f64>,
    pub root_rss_mb_max: Option<f64>,
    pub procs: usize,
    pub samples: usize,
}

/// Samples `tree` from `from` to `to`, every [`EVERY`], and sums them up.
pub fn window(tree: &mut Tree, from: Instant, to: Instant) -> Window {
    std::thread::sleep(from.saturating_duration_since(Instant::now()));
    tree.find();
    let mut points: Vec<(f64, Usage)> = Vec::new();
    let mut next = Instant::now();
    loop {
        let now = Instant::now();
        if let Some(u) = tree.now() {
            points.push((now.duration_since(from).as_secs_f64(), u));
        }
        if now >= to {
            break;
        }
        next += EVERY;
        std::thread::sleep(next.min(to).saturating_duration_since(Instant::now()));
    }
    summarize(&points)
}

fn summarize(points: &[(f64, Usage)]) -> Window {
    #[allow(clippy::cast_precision_loss)]
    let mb = |b: u64| b as f64 / 1e6;
    let mut w = Window { samples: points.len(), ..Window::default() };
    let (Some((t0, first)), Some((t1, last))) = (points.first(), points.last()) else { return w };
    let span = t1 - t0;
    if span > 0.0 {
        w.cpu_share = Some((last.cpu_s - first.cpu_s).max(0.0) / span);
        w.root_cpu_share = Some((last.root_cpu_s - first.root_cpu_s).max(0.0) / span);
        w.cpu_share_max = points.windows(2).filter_map(|p| Some((p.first()?, p.get(1)?))).filter(|(a, b)| b.0 > a.0).map(|(a, b)| (b.1.cpu_s - a.1.cpu_s).max(0.0) / (b.0 - a.0)).reduce(f64::max);
    }
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f64;
    w.rss_mb = Some(points.iter().map(|(_, u)| mb(u.rss)).sum::<f64>() / n);
    w.rss_mb_max = points.iter().map(|(_, u)| mb(u.rss)).reduce(f64::max);
    w.root_rss_mb_max = points.iter().map(|(_, u)| mb(u.root_rss)).reduce(f64::max);
    w.procs = points.iter().map(|(_, u)| u.procs).max().unwrap_or(0);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10 (Ada's fixed-rate benchmark): a server's CPU and memory are its whole tree's: Linux's `/proc`
    /// lines (a name with spaces and parentheses), macOS's `ps` lines, and the tree under the root, not beside it.
    #[test]
    fn rate_a_servers_usage_is_its_trees() {
        let p = proc_stat("4242 (node (worker) x) S 4000 4242 4242 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 9 0 12345 999999 2500 18446744073709551615", 100.0, 4096);
        assert_eq!(p, Some(Proc { ppid: 4000, cpu_s: 3.0, rss: 2500 * 4096 }));
        assert_eq!(ps_time("0:01.50"), Some(1.5));
        assert!(ps_time("12:34.56").is_some_and(|t| (t - 754.56).abs() < 1e-9));
        assert_eq!(ps_time("1:02:03.00"), Some(3723.0));
        assert_eq!(ps_time("2-01:00:00"), Some(176_400.0));
        let all = ps_lines("  1     0  1000   1:00.00\n 10     1  2048   0:02.00\n 11    10  1024   0:01.00\n 12     1   512   0:09.00\n 13    11   100   0:00.50\nnonsense\n");
        let t = tree(&all, 10);
        assert_eq!(t.iter().copied().collect::<Vec<_>>(), vec![10, 11, 13], "10's tree, not its sibling 12");
        let u = usage(&all, &t, 10);
        assert!((u.cpu_s - 3.5).abs() < 1e-9 && u.rss == (2048 + 1024 + 100) * 1024 && u.procs == 3, "{u:?}");
        assert!((u.root_cpu_s - 2.0).abs() < 1e-9 && u.root_rss == 2048 * 1024, "{u:?}");
        // A window: 1.5 CPUs on average, 2 at the most, its memory's mean and most.
        let at = |t: f64, cpu: f64, rss: u64| (t, Usage { cpu_s: cpu, rss, procs: 2, root_cpu_s: cpu / 2.0, root_rss: rss / 2 });
        let w = summarize(&[at(0.0, 10.0, 100_000_000), at(1.0, 11.0, 200_000_000), at(2.0, 13.0, 300_000_000)]);
        assert_eq!((w.cpu_share, w.cpu_share_max, w.root_cpu_share, w.samples, w.procs), (Some(1.5), Some(2.0), Some(0.75), 3, 2));
        assert_eq!((w.rss_mb, w.rss_mb_max, w.root_rss_mb_max), (Some(200.0), Some(300.0), Some(150.0)));
        assert_eq!(summarize(&[]).cpu_share, None);
        // This process, as the OS says it now.
        let me = Tree::of(std::process::id());
        assert!(cfg!(not(unix)) || me.now().is_some_and(|u| u.procs >= 1 && u.rss > 0), "{:?}", me.now());
    }
}
