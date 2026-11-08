//! The server's CPU and memory through a window, as the OS counts them: its whole process tree's (a server that
//! runs under a wrapper or starts helpers is all of them), and its root process's alone. Linux reads `/proc`;
//! macOS asks `ps` (no `/proc` there, and `ps` needs nothing of the lab's own); Windows isn't measured.
//!
//! On Linux also what the host did to the tree's threads: how long they waited, runnable, for a CPU (the kernel's
//! run delay), and the scheduling policy and nice value each ran at. A host can demote a server on its own: on
//! adacachy (CachyOS) ananicy-cpp's rule for `node` puts every Node process in the idle policy, so T3 Code's server
//! got a CPU only when nothing else wanted one, and at 100 requests a second it answered 5 (the rate's smoke test,
//! 2026-11-06). The lab says so rather than charge the server.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Page faults since each process started, all of the tree's (Linux; 0 elsewhere): minor and major.
    pub minflt: u64,
    pub majflt: u64,
}

/// A process: its parent, its CPU seconds, its resident bytes and its page faults, minor and major.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Proc {
    ppid: u32,
    cpu_s: f64,
    rss: u64,
    minflt: u64,
    majflt: u64,
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
            u.minflt += p.minflt;
            u.majflt += p.majflt;
        }
    }
    if let Some(r) = all.get(&root) {
        (u.root_cpu_s, u.root_rss) = (r.cpu_s, r.rss);
    }
    u
}

/// A Linux `/proc/<pid>/stat` line: its parent, CPU seconds (`utime` and `stime`, in `ticks`), resident bytes
/// (`rss`, in pages of `page`) and page faults (`minflt` and `majflt`). The command's name (in parentheses) may
/// hold spaces and parentheses: the fields are counted from its last `)`.
fn proc_stat(line: &str, ticks: f64, page: u64) -> Option<Proc> {
    let after = line.get(line.rfind(')')? + 1..)?;
    let f: Vec<&str> = after.split_whitespace().collect();
    // Fields from the 3rd (state) on: ppid is the 4th, minflt the 10th, majflt the 12th, utime the 14th, stime the
    // 15th, rss the 24th.
    let field = |n: usize| f.get(n - 3).copied();
    let ppid = field(4)?.parse().ok()?;
    let (minflt, majflt) = (field(10)?.parse().ok()?, field(12)?.parse().ok()?);
    let ticks_used = field(14)?.parse::<u64>().ok()? + field(15)?.parse::<u64>().ok()?;
    let rss_pages = field(24)?.parse::<i64>().ok()?.max(0).unsigned_abs();
    #[allow(clippy::cast_precision_loss)]
    let cpu_s = ticks_used as f64 / ticks;
    Some(Proc { ppid, cpu_s, rss: rss_pages * page, minflt, majflt })
}

/// A thread of the tree as Linux says it: its scheduling policy, real-time priority and nice value
/// (`/proc/<pid>/task/<tid>/stat`'s 41st, 40th and 19th fields), and its time on a CPU and waiting, runnable, for
/// one (`schedstat`, the kernel's `proc_pid_schedstat`: `<ran ns> <waited ns> <runs>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Thread {
    policy: u32,
    rt_priority: u32,
    nice: i64,
    /// None where the kernel keeps no thread's times (no `CONFIG_SCHED_INFO`: it says zeros).
    ran_ns: Option<u64>,
    waited_ns: Option<u64>,
}

/// Linux's `SCHED_IDLE` (sched(7)): a thread that gets a CPU only when nothing else wants it.
const SCHED_IDLE: u32 = 5;

/// A policy's name (sched(7), `linux/sched.h`).
fn policy_name(p: u32) -> String {
    match p {
        0 => "SCHED_OTHER".into(),
        1 => "SCHED_FIFO".into(),
        2 => "SCHED_RR".into(),
        3 => "SCHED_BATCH".into(),
        SCHED_IDLE => "SCHED_IDLE".into(),
        6 => "SCHED_DEADLINE".into(),
        p => format!("policy {p}"),
    }
}

/// A task's `stat` line: its policy, real-time priority and nice value, counted from the last `)` as
/// [`proc_stat`] counts.
fn task_stat(line: &str) -> Option<(u32, u32, i64)> {
    let after = line.get(line.rfind(')')? + 1..)?;
    let f: Vec<&str> = after.split_whitespace().collect();
    let field = |n: usize| f.get(n - 3).copied();
    Some((field(41)?.parse().ok()?, field(40)?.parse().ok()?, field(19)?.parse().ok()?))
}

/// A `schedstat` line's time on a CPU and waiting for one, ns: none where the kernel keeps none (all zeros), as
/// kiln's own reader takes it (`crates/kiln/src/nudge.rs`, `run_wait_of`).
fn sched_times(line: &str) -> Option<(u64, u64)> {
    let mut w = line.split_ascii_whitespace().map(|x| x.parse::<u64>().ok());
    let (ran, waited, runs) = (w.next()??, w.next()??, w.next()??);
    (ran > 0 || runs > 0).then_some((ran, waited))
}

/// Every thread of `pids` (Linux; nothing elsewhere), by its id.
fn threads(pids: &BTreeSet<u32>) -> BTreeMap<u32, Thread> {
    let mut out = BTreeMap::new();
    if !cfg!(target_os = "linux") {
        return out;
    }
    for pid in pids {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for t in tasks.flatten() {
            let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            let Some((policy, rt_priority, nice)) = std::fs::read_to_string(t.path().join("stat")).ok().as_deref().and_then(task_stat) else { continue };
            let times = std::fs::read_to_string(t.path().join("schedstat")).ok().as_deref().and_then(sched_times);
            out.insert(tid, Thread { policy, rt_priority, nice, ran_ns: times.map(|t| t.0), waited_ns: times.map(|t| t.1) });
        }
    }
    out
}

/// What the host did to the tree's threads between two looks `span_s` apart: their wait for a CPU (CPUs' worth,
/// and seconds of it for each second they ran; a thread born between the looks counts from its start), the share
/// of its time wanting a CPU that the neediest thread (the most time on one and waiting for one) waited, and their
/// policies at the second look, most first.
fn held(first: &BTreeMap<u32, Thread>, last: &BTreeMap<u32, Thread>, span_s: f64, w: &mut Window) {
    let (mut ran, mut waited, mut known) = (0u64, 0u64, false);
    let mut neediest: Option<(u64, u64)> = None;
    for (tid, t) in last {
        let before = first.get(tid);
        if let (Some(r), Some(q)) = (t.ran_ns, t.waited_ns) {
            known = true;
            let (r, q) = (r.saturating_sub(before.and_then(|b| b.ran_ns).unwrap_or(0)), q.saturating_sub(before.and_then(|b| b.waited_ns).unwrap_or(0)));
            (ran, waited) = (ran + r, waited + q);
            if neediest.is_none_or(|(nr, nq)| r + q > nr + nq) {
                neediest = Some((r, q));
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let (ran_s, waited_s) = (ran as f64 / 1e9, waited as f64 / 1e9);
    if known && span_s > 0.0 {
        w.run_wait_share = Some(waited_s / span_s);
        w.run_wait_per_cpu_s = (ran_s > 0.0).then(|| waited_s / ran_s);
        #[allow(clippy::cast_precision_loss)]
        let need = neediest.filter(|(r, q)| r + q > 0).map(|(r, q)| q as f64 / (r + q) as f64);
        w.neediest_wait_share = need;
    }
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for t in last.values() {
        let how = match t.policy {
            1 | 2 => format!("{} priority {}", policy_name(t.policy), t.rt_priority),
            6 => policy_name(t.policy),
            _ => format!("{} nice {}", policy_name(t.policy), t.nice),
        };
        *groups.entry(how).or_default() += 1;
    }
    let mut groups: Vec<(String, usize)> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    w.sched = (!groups.is_empty()).then(|| groups.iter().map(|(how, n)| format!("{n} {how}")).collect::<Vec<_>>().join(", "));
    w.idle_threads = last.values().filter(|t| t.policy == SCHED_IDLE).count();
    w.threads = last.len();
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
            Some((pid, Proc { ppid, cpu_s, rss: rss_kib * 1024, minflt: 0, majflt: 0 }))
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

    /// Each of the tree's processes' page faults now, minor and major (Linux; none elsewhere).
    fn faults(&self) -> BTreeMap<u32, (u64, u64)> {
        if !cfg!(target_os = "linux") {
            return BTreeMap::new();
        }
        processes(Some(&self.pids)).into_iter().map(|(pid, p)| (pid, (p.minflt, p.majflt))).collect()
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
    /// Linux: the tree's threads' wait, runnable, for a CPU through the window, CPUs' worth (1.0: one thread
    /// waiting throughout), and the seconds they waited for each second they ran: the host's other work holding
    /// the server back.
    #[serde(default)]
    pub run_wait_share: Option<f64>,
    #[serde(default)]
    pub run_wait_per_cpu_s: Option<f64>,
    /// Linux: of the thread that wanted a CPU the most through the window (its time on one and waiting for one,
    /// together), the share of that it waited: the hold on what is likely the server's bottleneck (a single-threaded
    /// server's loop), which its other threads' waits can hide (Node's own workers, not demoted with its loop).
    #[serde(default)]
    pub neediest_wait_share: Option<f64>,
    /// Linux: the tree's threads at the window's end by scheduling policy and nice value, most first
    /// (`16 SCHED_OTHER nice 0, 2 SCHED_IDLE nice 16`), how many there were, and how many of them in the idle
    /// policy, which gets a CPU only when nothing else wants one.
    #[serde(default)]
    pub sched: Option<String>,
    #[serde(default)]
    pub threads: usize,
    #[serde(default)]
    pub idle_threads: usize,
    /// Linux: the tree's page faults through the window, minor (a page the kernel had: a fresh one zeroed, or one
    /// cached or shared mapped in) and major (one read in from disk): what touching new memory cost the server in
    /// the kernel (Ada, 2026-11-07: "allocs, reallocs, page faults").
    #[serde(default)]
    pub minor_faults: Option<u64>,
    #[serde(default)]
    pub major_faults: Option<u64>,
    /// The first and last samples' moments on the wall clock, Unix seconds: what lines a sampler outside the lab
    /// (the server's cgroup's counters) up with the window.
    #[serde(default)]
    pub from_unix_s: Option<f64>,
    #[serde(default)]
    pub to_unix_s: Option<f64>,
}

/// Samples `tree` from `from` to `to`, every [`EVERY`], and sums them up, with what the host did to its threads
/// between the window's ends.
pub fn window(tree: &mut Tree, from: Instant, to: Instant) -> Window {
    std::thread::sleep(from.saturating_duration_since(Instant::now()));
    tree.find();
    let (first, looked) = (threads(&tree.pids), Instant::now());
    let mut points: Vec<(f64, Usage)> = Vec::new();
    let unix = || SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs_f64());
    let (mut first_wall, mut last_wall) = (None, None);
    let mut next = Instant::now();
    // Each process's faults at the window's start and as last seen: one that ends mid-window keeps what it made until then,
    // where a sum over those still there would drop it (and read less than nothing, so none).
    let (mut faults_from, mut faults_seen) = (None::<BTreeMap<u32, (u64, u64)>>, BTreeMap::new());
    loop {
        let now = Instant::now();
        if let Some(u) = tree.now() {
            points.push((now.duration_since(from).as_secs_f64(), u));
            last_wall = unix();
            first_wall = first_wall.or(last_wall);
        }
        let faults = tree.faults();
        faults_from.get_or_insert_with(|| faults.clone());
        faults_seen.extend(faults);
        if now >= to {
            break;
        }
        next += EVERY;
        std::thread::sleep(next.min(to).saturating_duration_since(Instant::now()));
    }
    let mut w = summarize(&points);
    (w.from_unix_s, w.to_unix_s) = (first_wall, last_wall);
    if let Some(from) = faults_from.filter(|_| cfg!(target_os = "linux")) {
        let made = |pick: fn(&(u64, u64)) -> u64| from.iter().map(|(pid, f)| faults_seen.get(pid).map_or(0, |s| pick(s).saturating_sub(pick(f)))).sum::<u64>();
        (w.minor_faults, w.major_faults) = (Some(made(|f| f.0)), Some(made(|f| f.1)));
    }
    held(&first, &threads(&tree.pids), looked.elapsed().as_secs_f64(), &mut w);
    w
}

fn summarize(points: &[(f64, Usage)]) -> Window {
    #[allow(clippy::cast_precision_loss)]
    let mb = |b: u64| b as f64 / 1e6;
    let mut w = Window { samples: points.len(), ..Window::default() };
    let (Some((t0, first)), Some((t1, last))) = (points.first(), points.last()) else { return w };
    let span = t1 - t0;
    if cfg!(target_os = "linux") {
        (w.minor_faults, w.major_faults) = (Some(last.minflt.saturating_sub(first.minflt)), Some(last.majflt.saturating_sub(first.majflt)));
    }
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
        assert_eq!(p, Some(Proc { ppid: 4000, cpu_s: 3.0, rss: 2500 * 4096, minflt: 100, majflt: 0 }));
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
        let at = |t: f64, cpu: f64, rss: u64| (t, Usage { cpu_s: cpu, rss, procs: 2, root_cpu_s: cpu / 2.0, root_rss: rss / 2, ..Usage::default() });
        let w = summarize(&[at(0.0, 10.0, 100_000_000), at(1.0, 11.0, 200_000_000), at(2.0, 13.0, 300_000_000)]);
        assert_eq!((w.cpu_share, w.cpu_share_max, w.root_cpu_share, w.samples, w.procs), (Some(1.5), Some(2.0), Some(0.75), 3, 2));
        assert_eq!((w.rss_mb, w.rss_mb_max, w.root_rss_mb_max), (Some(200.0), Some(300.0), Some(150.0)));
        assert_eq!(summarize(&[]).cpu_share, None);
        // This process, as the OS says it now.
        let me = Tree::of(std::process::id());
        assert!(cfg!(not(unix)) || me.now().is_some_and(|u| u.procs >= 1 && u.rss > 0), "{:?}", me.now());
    }

    /// Goal 10 (Ada's fixed-rate benchmark; the rate mode's smoke test, 2026-11-06): what the host did to a server's
    /// threads through a window. A task's `stat` line gives its policy, real-time priority and nice value; its
    /// `schedstat` its time on a CPU and waiting for one (none where the kernel keeps none); between two looks the
    /// threads' waits are summed (a thread born between them from its start), as CPUs' worth and as seconds for each
    /// second they ran, and their policies are said most first, the idle policy's counted.
    #[test]
    fn rate_the_hosts_hold_on_a_servers_threads_is_seen() {
        let line = |nice: &str, rt: &str, policy: &str| {
            let mut f: Vec<String> = (3..=52).map(|n| n.to_string()).collect();
            for (n, v) in [(3, "S"), (19, nice), (40, rt), (41, policy)] {
                if let Some(x) = f.get_mut(n - 3) {
                    *x = v.to_owned();
                }
            }
            format!("1150491 (node (Main) x) {}", f.join(" "))
        };
        assert_eq!(task_stat(&line("16", "0", "5")), Some((SCHED_IDLE, 0, 16)));
        assert_eq!(task_stat(&line("-4", "1", "2")), Some((2, 1, -4)));
        assert_eq!(task_stat("12 (short) S 1 2 3"), None);
        assert_eq!(sched_times("1500 3000 7\n"), Some((1500, 3000)));
        assert_eq!(sched_times("0 0 0\n"), None, "no CONFIG_SCHED_INFO: the kernel says zeros");
        assert_eq!(sched_times("x"), None);
        let t = |policy: u32, rt_priority: u32, nice: i64, ran: f64, waited: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let ns = |s: f64| Some((s * 1e9) as u64);
            Thread { policy, rt_priority, nice, ran_ns: ns(ran), waited_ns: ns(waited) }
        };
        let first = BTreeMap::from([(1, t(0, 0, 0, 1.0, 0.5)), (2, t(SCHED_IDLE, 0, 16, 2.0, 1.0)), (9, t(0, 0, 0, 5.0, 5.0))]);
        // 1 ran 0.5 s and waited 1; 2 ran 0.5 and waited 3; 3, born since, ran 0.25 and waited 0.5; 9 ended.
        let last = BTreeMap::from([(1, t(0, 0, 0, 1.5, 1.5)), (2, t(SCHED_IDLE, 0, 16, 2.5, 4.0)), (3, t(SCHED_IDLE, 0, 16, 0.25, 0.5)), (4, t(2, 1, 0, 0.0, 0.0))]);
        let mut w = Window::default();
        held(&first, &last, 2.0, &mut w);
        let near = |a: Option<f64>, b: f64| a.is_some_and(|a| (a - b).abs() < 1e-9);
        assert!(near(w.run_wait_share, 2.25) && near(w.run_wait_per_cpu_s, 3.6), "4.5 s waited over 2 s, against 1.25 s ran: {w:?}");
        assert!(near(w.neediest_wait_share, 3.0 / 3.5), "the neediest, 2, wanted a CPU 3.5 s and waited 3 of them: {w:?}");
        assert_eq!(w.sched.as_deref(), Some("2 SCHED_IDLE nice 16, 1 SCHED_OTHER nice 0, 1 SCHED_RR priority 1"));
        assert_eq!((w.threads, w.idle_threads), (4, 2));
        // A kernel that keeps no thread's times: the policies, and no wait.
        let blind = BTreeMap::from([(1, Thread { ran_ns: None, waited_ns: None, ..t(0, 0, 10, 0.0, 0.0) })]);
        let mut w = Window::default();
        held(&BTreeMap::new(), &blind, 2.0, &mut w);
        assert_eq!((w.run_wait_share, w.run_wait_per_cpu_s, w.neediest_wait_share, w.sched.as_deref()), (None, None, None, Some("1 SCHED_OTHER nice 10")));
        // This process's own threads, as Linux says them now.
        if cfg!(target_os = "linux") {
            let mine = threads(&BTreeSet::from([std::process::id()]));
            assert!(mine.contains_key(&std::process::id()), "its main thread among {:?}", mine.keys().collect::<Vec<_>>());
        }
    }
}
