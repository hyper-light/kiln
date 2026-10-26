//! Which CPUs a leg runs on (docs/ci.md W05-R03), one ledger for all of a runner's legs.
//!
//! An exclusive leg holding a share of its runner gets whole cores of its own (every thread of
//! each), a whole L3 domain when its share covers one, and every other leg's processes run on the
//! rest. In resource mode a leg asks for CPUs and is lent that many of the rest, as a cpuset and not
//! a quota: `nproc`, and the tools that size their work by it (`make -j$(nproc)`, a test runner's
//! workers), see the leg's CPUs, as on a hosted runner's machine of that size. Legs at once take
//! CPUs no other leg holds while there are any; past that, the least held. A share taken later
//! leaves a leased leg what of its lease the share doesn't hold.
//!
//! A step is started on its leg's CPUs (the thread starting it takes them for that moment, so the
//! step's process inherits them from its first instruction, and what it starts from it; a
//! container gets a cpuset), and the steps already running move as a share is taken or let go. A
//! container started before a share was taken keeps its CPUs until it ends. Linux only: elsewhere
//! a share still holds its room in placement and a leg sees the machine's CPUs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use parking_lot::Mutex;

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

/// CPUs as a list of ranges, as Linux and Docker write one: `0-3,8`.
pub fn list(cpus: &[usize]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out: Vec<String> = Vec::new();
    let mut it = sorted.into_iter().peekable();
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

/// The CPUs resource-mode legs hold, each with how many hold it.
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

/// One core: its CPUs (its SMT threads) and the L3 cache it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Core {
    cpus: Vec<usize>,
    domain: usize,
}

/// A machine's CPUs as cores in L3 domains.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Topology {
    cores: Vec<Core>,
}

/// A list from /sys, sorted, nothing twice; nothing for one that isn't a list.
fn sorted(s: &str) -> Vec<usize> {
    let mut v = parse(s.trim()).unwrap_or_default();
    v.sort_unstable();
    v.dedup();
    v
}

impl Topology {
    /// From each CPU's siblings (`thread_siblings_list`) and the CPUs its L3 serves
    /// (`cache/index3/shared_cpu_list`; a CPU without one is a domain with its siblings).
    pub fn from(siblings: &[(usize, String)], l3: &[(usize, String)]) -> Self {
        let l3: BTreeMap<usize, Vec<usize>> = l3.iter().map(|(c, s)| (*c, sorted(s))).collect();
        let mut domains: Vec<Vec<usize>> = Vec::new();
        let mut cores: Vec<Core> = Vec::new();
        let mut seen = BTreeSet::new();
        let mut by_cpu = siblings.to_vec();
        by_cpu.sort_by_key(|(c, _)| *c);
        for (cpu, s) in &by_cpu {
            if seen.contains(cpu) {
                continue;
            }
            let mut threads = sorted(s);
            if !threads.contains(cpu) {
                threads.push(*cpu);
                threads.sort_unstable();
            }
            seen.extend(threads.iter().copied());
            let shares = l3.get(cpu).cloned().unwrap_or_else(|| threads.clone());
            let domain = match domains.iter().position(|d| *d == shares) {
                Some(d) => d,
                None => {
                    domains.push(shares);
                    domains.len() - 1
                }
            };
            cores.push(Core { cpus: threads, domain });
        }
        Topology { cores }
    }

    /// This machine's, from /sys (Linux).
    pub fn read() -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            let base = std::path::Path::new("/sys/devices/system/cpu");
            let online = sorted(&std::fs::read_to_string(base.join("online")).ok()?);
            let read = |cpu: usize, rel: &str| std::fs::read_to_string(base.join(format!("cpu{cpu}")).join(rel)).ok().map(|s| s.trim().to_owned());
            let siblings: Vec<(usize, String)> = online.iter().map(|&c| (c, read(c, "topology/thread_siblings_list").unwrap_or_else(|| c.to_string()))).collect();
            let l3: Vec<(usize, String)> = online.iter().filter_map(|&c| read(c, "cache/index3/shared_cpu_list").map(|s| (c, s))).collect();
            let t = Topology::from(&siblings, &l3);
            (!t.cores.is_empty()).then_some(t)
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    pub fn cpus(&self) -> Vec<usize> {
        let mut all: Vec<usize> = self.cores.iter().flat_map(|c| c.cpus.iter().copied()).collect();
        all.sort_unstable();
        all
    }

    /// Whole cores for a share of `threads` CPUs, none of them `taken`: a whole L3 domain when the
    /// share covers one; else the cores of the domain with the most free that holds them all;
    /// else as few domains as hold them, the freest first. `None` when there aren't that many.
    pub fn pick(&self, taken: &BTreeSet<usize>, threads: usize) -> Option<Vec<usize>> {
        if threads == 0 {
            return Some(Vec::new());
        }
        let per_core = self.cores.first().map_or(1, |c| c.cpus.len().max(1));
        let need = threads.div_ceil(per_core);
        let mut free: BTreeMap<usize, Vec<&Core>> = BTreeMap::new();
        for c in self.cores.iter().filter(|c| c.cpus.iter().all(|x| !taken.contains(x))) {
            free.entry(c.domain).or_default().push(c);
        }
        if free.values().map(Vec::len).sum::<usize>() < need {
            return None;
        }
        let size = |d: usize| self.cores.iter().filter(|c| c.domain == d).count();
        // The domains to take from: whole free domains the share covers, then the freest.
        let mut order: Vec<(usize, usize)> = free.iter().map(|(d, cs)| (*d, cs.len())).collect();
        order.sort_by(|a, b| {
            let whole = |(d, n): &(usize, usize)| *n == size(*d) && *n <= need;
            whole(b).cmp(&whole(a)).then(b.1.cmp(&a.1)).then(a.0.cmp(&b.0))
        });
        // One domain that holds it all, if any does: the smallest such, so bigger ones stay whole.
        if let Some((d, _)) = free.iter().filter(|(_, cs)| cs.len() >= need).min_by_key(|(d, cs)| (cs.len(), **d)).map(|(d, cs)| (*d, cs.len())) {
            let whole_first = order.first().is_some_and(|(w, n)| *n == size(*w) && *n == need);
            let from = if whole_first { order.first().map_or(d, |(w, _)| *w) } else { d };
            let mut out: Vec<usize> = free.get(&from).into_iter().flatten().take(need).flat_map(|c| c.cpus.iter().copied()).collect();
            out.sort_unstable();
            return Some(out);
        }
        let mut out = Vec::new();
        let mut left = need;
        for (d, _) in order {
            for c in free.get(&d).into_iter().flatten() {
                if left == 0 {
                    break;
                }
                out.extend(c.cpus.iter().copied());
                left -= 1;
            }
        }
        out.sort_unstable();
        Some(out)
    }
}

/// What this runner's legs hold of its CPUs (shares and leases), and the process groups of the
/// steps running now.
pub struct Pins {
    topo: Option<Topology>,
    /// The CPUs the runner may run on itself (its affinity, within its cgroup's cpuset).
    all: Vec<usize>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    shares: BTreeMap<String, Vec<usize>>,
    /// Each lease by its own number: its leg and its CPUs.
    leases: BTreeMap<u64, (String, Vec<usize>)>,
    next: u64,
    pool: Pool,
    groups: BTreeMap<String, Vec<i32>>,
}

impl State {
    fn shared(&self) -> BTreeSet<usize> {
        self.shares.values().flatten().copied().collect()
    }

    fn lent(&self) -> BTreeSet<usize> {
        self.leases.values().flat_map(|(_, c)| c.iter().copied()).collect()
    }

    fn lent_to(&self, job: &str) -> Vec<usize> {
        let mut cpus: Vec<usize> = self.leases.values().filter(|(j, _)| j == job).flat_map(|(_, c)| c.iter().copied()).collect();
        cpus.sort_unstable();
        cpus.dedup();
        cpus
    }
}

impl Pins {
    /// A ledger over `topo` (none: nothing pinned for a share) and the CPUs the runner may use (none
    /// said: the topology's, or nothing to lend).
    pub fn new(topo: Option<Topology>, usable: Option<Vec<usize>>) -> Self {
        let all = match (usable, &topo) {
            (Some(u), _) => u,
            (None, Some(t)) => t.cpus(),
            (None, None) => Vec::new(),
        };
        Pins { topo, all, state: Mutex::new(State::default()) }
    }

    /// A share for `job`: whole cores for `threads` CPUs that no other share holds, and none a lease
    /// holds while there are enough without. The steps running for other legs move off them. `None`
    /// when there's no topology or not that many free.
    pub fn hold(&self, job: &str, threads: usize) -> Option<Vec<usize>> {
        let topo = self.topo.as_ref()?;
        let mut st = self.state.lock();
        let others: BTreeSet<usize> = st.shares.iter().filter(|(j, _)| j.as_str() != job).flat_map(|(_, c)| c.iter().copied()).collect();
        let outside: BTreeSet<usize> = topo.cpus().into_iter().filter(|c| !self.all.contains(c)).collect();
        let taken: BTreeSet<usize> = others.union(&outside).copied().collect();
        let cpus = topo.pick(&taken.union(&st.lent()).copied().collect(), threads).or_else(|| topo.pick(&taken, threads))?;
        st.shares.insert(job.to_owned(), cpus.clone());
        for (j, groups) in &st.groups {
            if j != job
                && let Some(c) = self.allowed_in(&st, j)
            {
                for g in groups {
                    apply_group(*g, &c);
                }
            }
        }
        Some(cpus)
    }

    /// `job`'s share let go: the steps running for the other legs may use its cores again.
    pub fn release(&self, job: &str) {
        let mut st = self.state.lock();
        st.groups.remove(job);
        if st.shares.remove(job).is_none() {
            return;
        }
        for (j, groups) in &st.groups {
            let c = self.allowed_in(&st, j).unwrap_or_else(|| self.all.clone());
            for g in groups {
                apply_group(*g, &c);
            }
        }
    }

    /// `n` CPUs lent to `job` (resource mode), and the lease's number: of those no share holds, the
    /// fewest legs hold, the lowest first. `None` with none asked for, or none the runner can say
    /// it may use.
    pub fn lease(&self, job: &str, n: usize) -> Option<(u64, Vec<usize>)> {
        let mut st = self.state.lock();
        let shared = st.shared();
        let usable: Vec<usize> = self.all.iter().copied().filter(|c| !shared.contains(c)).collect();
        if usable.is_empty() || n == 0 {
            return None;
        }
        let cpus = st.pool.take(&usable, n.min(usable.len()));
        st.next += 1;
        let id = st.next;
        st.leases.insert(id, (job.to_owned(), cpus.clone()));
        Some((id, cpus))
    }

    /// Lease `id` given back.
    pub fn give(&self, id: u64) {
        let mut st = self.state.lock();
        if let Some((_, cpus)) = st.leases.remove(&id) {
            st.pool.give(&cpus);
        }
    }

    /// The CPUs `job`'s processes may run on: its own share; else what of its lease no share holds;
    /// else what no share holds. `None`: any (nothing held, nothing lent).
    pub fn allowed(&self, job: &str) -> Option<Vec<usize>> {
        self.allowed_in(&self.state.lock(), job)
    }

    fn allowed_in(&self, st: &State, job: &str) -> Option<Vec<usize>> {
        if let Some(c) = st.shares.get(job) {
            return Some(c.clone());
        }
        let shared = st.shared();
        let rest = || self.all.iter().copied().filter(|c| !shared.contains(c)).collect::<Vec<usize>>();
        let lent = st.lent_to(job);
        if !lent.is_empty() {
            let mine: Vec<usize> = lent.into_iter().filter(|c| !shared.contains(c)).collect();
            return Some(if mine.is_empty() { rest() } else { mine });
        }
        (!shared.is_empty()).then(rest)
    }

    /// A step of `job` started in process group `pgid`: on the CPUs its leg may use, and counted
    /// while it runs.
    pub fn started(&self, job: &str, pgid: i32) {
        if let Some(cpus) = self.allowed(job) {
            apply_group(pgid, &cpus);
        }
        self.state.lock().groups.entry(job.to_owned()).or_default().push(pgid);
    }

    /// That step ended.
    pub fn ended(&self, job: &str, pgid: i32) {
        let mut st = self.state.lock();
        if let Some(g) = st.groups.get_mut(job) {
            g.retain(|x| *x != pgid);
            if g.is_empty() {
                st.groups.remove(job);
            }
        }
    }
}

/// This runner's: its machine's topology and its own CPUs, read once.
pub fn pins() -> &'static Pins {
    static PINS: OnceLock<Pins> = OnceLock::new();
    PINS.get_or_init(|| Pins::new(Topology::read(), usable()))
}

/// A leg's CPUs, lent from this runner's (resource mode), given back when it ends.
#[derive(Debug)]
pub struct Lease {
    id: u64,
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
        pins().give(self.id);
    }
}

/// `n` of the runner's CPUs lent to leg `job`: none with none asked for, or where the runner can't
/// say which it may run on (not Linux).
pub fn take(job: &str, n: usize) -> Option<Lease> {
    pins().lease(job, n).map(|(id, cpus)| Lease { id, cpus })
}

/// The CPUs this process may run on: its `Cpus_allowed_list`, its affinity within its cgroup's
/// cpuset.
fn usable() -> Option<Vec<usize>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse(status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?.trim()).filter(|u| !u.is_empty())
}

/// Starts a step of `job` with `spawn` on the CPUs its leg may use: the calling thread takes them
/// for the moment it starts the process, which inherits them from its first instruction, then
/// takes its own back. With nothing held or lent, as `spawn` would.
pub fn spawn_on<T>(job: &str, spawn: impl FnOnce() -> T) -> T {
    #[cfg(target_os = "linux")]
    if let Some(set) = pins().allowed(job).as_deref().and_then(cpu_set) {
        let me = nix::unistd::Pid::from_raw(0);
        if let Ok(before) = nix::sched::sched_getaffinity(me)
            && nix::sched::sched_setaffinity(me, &set).is_ok()
        {
            let out = spawn();
            let _ = nix::sched::sched_setaffinity(me, &before);
            return out;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = job;
    spawn()
}

/// A step's process group, counted while it runs, so a share taken or let go meanwhile moves it.
pub struct Step {
    job: String,
    pgid: i32,
}

impl Step {
    pub fn new(job: &str, pgid: i32) -> Self {
        pins().started(job, pgid);
        Step { job: job.to_owned(), pgid }
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        pins().ended(&self.job, self.pgid);
    }
}

#[cfg(target_os = "linux")]
fn cpu_set(cpus: &[usize]) -> Option<nix::sched::CpuSet> {
    let mut set = nix::sched::CpuSet::new();
    for &c in cpus {
        set.set(c).ok()?;
    }
    (!cpus.is_empty()).then_some(set)
}

/// Every thread of every process in group `pgid` onto `cpus`: the steps already running as a
/// share is taken or let go. Those gone meanwhile are passed over.
fn apply_group(pgid: i32, cpus: &[usize]) {
    #[cfg(target_os = "linux")]
    {
        let Some(set) = cpu_set(cpus) else { return };
        let Ok(dir) = std::fs::read_dir("/proc") else { return };
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            if group_of(pid) != Some(pgid) {
                continue;
            }
            let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
            for t in tasks.flatten() {
                if let Some(tid) = t.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) {
                    let _ = nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(tid), &set);
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (pgid, cpus);
}

/// A process's group, from `/proc/<pid>/stat` (its fifth field, after the command in parentheses).
#[cfg(target_os = "linux")]
fn group_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.get(stat.rfind(')')? + 1..)?;
    after.split_whitespace().nth(2)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    /// adacachy: 16 cores, two threads each (cpu N and N+16), in two L3 domains of 8.
    fn adacachy() -> Topology {
        let siblings: Vec<(usize, String)> = (0..32).map(|c| (c, format!("{},{}", c % 16, c % 16 + 16))).collect();
        let l3: Vec<(usize, String)> = (0..32).map(|c| (c, if c % 16 < 8 { "0-7,16-23".to_owned() } else { "8-15,24-31".to_owned() })).collect();
        Topology::from(&siblings, &l3)
    }

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
        assert_eq!(list(&[16, 0, 1, 2, 3, 17, 18, 19, 5]), "0-3,5,16-19", "in any order");
    }

    /// A share of 16 on adacachy is exactly one L3 domain; the next 16, the other; a share of two,
    /// one core's two threads; a share bigger than what's left isn't to be had.
    #[test]
    fn a_share_takes_whole_cores_and_a_whole_l3_domain_when_it_covers_one() -> anyhow::Result<()> {
        let t = adacachy();
        assert_eq!(t.cpus().len(), 32);
        let none = BTreeSet::new();
        let first = t.pick(&none, 16).ok_or_else(|| anyhow!("no domain"))?;
        assert!(list(&first) == "0-7,16-23" || list(&first) == "8-15,24-31", "one L3 domain: {}", list(&first));
        let taken: BTreeSet<usize> = first.iter().copied().collect();
        let second = t.pick(&taken, 16).ok_or_else(|| anyhow!("no second domain"))?;
        assert!(second.iter().all(|c| !taken.contains(c)) && second.len() == 16, "the other domain: {}", list(&second));
        let two = t.pick(&none, 2).ok_or_else(|| anyhow!("no core"))?;
        assert_eq!(two.len(), 2);
        assert_eq!(two.get(1).copied(), two.first().map(|c| c + 16), "a core's two threads: {}", list(&two));
        // Three threads take two whole cores, never half of one.
        assert_eq!(t.pick(&none, 3).map(|c| c.len()), Some(4));
        let all: BTreeSet<usize> = (0..30).collect();
        assert_eq!(t.pick(&all, 4), None, "not that many whole cores free");
        // A small share stays in one domain and leaves the other whole for a bench.
        let small = t.pick(&none, 4).ok_or_else(|| anyhow!("no two cores"))?;
        let low = |c: &usize| *c % 16 < 8;
        assert!(small.iter().all(low) || !small.iter().any(low), "one domain: {}", list(&small));
        Ok(())
    }

    /// No SMT, one L3: cores are single CPUs, one domain; no L3 known: each core its own.
    #[test]
    fn other_machines_pick_whole_cores_too() {
        let flat = Topology::from(&(0..8).map(|c| (c, c.to_string())).collect::<Vec<_>>(), &(0..8).map(|c| (c, "0-7".to_owned())).collect::<Vec<_>>());
        assert_eq!(flat.pick(&BTreeSet::new(), 3).map(|c| list(&c)), Some("0-2".to_owned()));
        let bare = Topology::from(&(0..4).map(|c| (c, c.to_string())).collect::<Vec<_>>(), &[]);
        assert_eq!(bare.pick(&BTreeSet::new(), 2).map(|c| c.len()), Some(2));
    }

    /// The ledger: a bench's share, the rest for everyone else, and all again once it ends.
    #[test]
    fn a_share_is_its_legs_and_the_rest_everyone_elses() -> anyhow::Result<()> {
        let p = Pins::new(Some(adacachy()), None);
        assert_eq!(p.allowed("o"), None, "nothing held: any CPU");
        let bench = p.hold("bench", 16).ok_or_else(|| anyhow!("no share"))?;
        assert_eq!(p.allowed("bench"), Some(bench.clone()));
        let rest = p.allowed("o").ok_or_else(|| anyhow!("no rest"))?;
        assert_eq!(rest.len(), 16);
        assert!(rest.iter().all(|c| !bench.contains(c)), "no other leg on the bench's CPUs");
        p.release("bench");
        assert_eq!(p.allowed("o"), None);
        assert_eq!(Pins::new(None, None).hold("bench", 16), None, "no topology: placement holds, cores aren't pinned");
        Ok(())
    }

    /// One ledger for shares and leases (W05-R03): a lease comes from what no share holds; a share
    /// keeps off leased CPUs while there are enough others, and when it can't, the leased leg keeps
    /// what of its lease the share doesn't hold; a lease given back is the pool's again.
    #[test]
    fn leases_and_shares_keep_to_their_own_cpus() -> anyhow::Result<()> {
        let p = Pins::new(Some(adacachy()), None);
        let bench = p.hold("bench", 16).ok_or_else(|| anyhow!("no share"))?;
        let (first_id, lent) = p.lease("leg", 4).ok_or_else(|| anyhow!("no lease"))?;
        assert_eq!(lent.len(), 4);
        assert!(lent.iter().all(|c| !bench.contains(c)), "lent from the rest: {} (share {})", list(&lent), list(&bench));
        assert_eq!(p.allowed("leg"), Some(lent.clone()), "the leg runs on its lease");
        // The bench ends; a lease taken now may come from anywhere, and a new share keeps off it.
        p.release("bench");
        p.give(first_id);
        let (id, first) = p.lease("leg", 4).ok_or_else(|| anyhow!("no lease"))?;
        let share = p.hold("bench", 16).ok_or_else(|| anyhow!("no share"))?;
        assert!(share.iter().all(|c| !first.contains(c)), "the share keeps off the lease: {} and {}", list(&share), list(&first));
        assert_eq!(p.allowed("leg"), Some(first.clone()));
        // A share that must cover leased CPUs (all 32 asked): the leg keeps what's left of its lease,
        // or the rest when nothing is.
        p.release("bench");
        let whole = p.hold("whole", 32).ok_or_else(|| anyhow!("no whole share"))?;
        assert_eq!(whole.len(), 32);
        assert_eq!(p.allowed("leg"), Some(Vec::new()), "nothing left beside a share of every CPU");
        p.release("whole");
        p.give(id);
        assert_eq!(p.lease("other", 4).map(|(_, c)| c), Some(vec![0, 1, 2, 3]), "given back, the pool's again");
        // Two leases of one leg (a test's legs may share a name) are both its, each given back alone.
        let (a, _) = p.lease("twice", 2).ok_or_else(|| anyhow!("no lease"))?;
        let (_, b) = p.lease("twice", 2).ok_or_else(|| anyhow!("no lease"))?;
        p.give(a);
        assert_eq!(p.allowed("twice"), Some(b), "the other lease still its");
        assert_eq!(Pins::new(None, None).lease("leg", 2), None, "nothing to lend where the runner can't say its CPUs");
        Ok(())
    }

    /// On Linux, a step started beside a share runs on the rest, a running one moves off a share
    /// as it's taken and back once it's let go: the processes' own Cpus_allowed_list say so.
    #[cfg(target_os = "linux")]
    #[test]
    fn steps_run_beside_a_share_and_move_as_it_comes_and_goes() -> anyhow::Result<()> {
        use std::os::unix::process::CommandExt as _;
        let allowed = |pid: u32| {
            let s = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            s.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:")).map(sorted).unwrap_or_default()
        };
        let Some(topo) = Topology::read() else { return Ok(()) };
        let all = usable().unwrap_or_else(|| topo.cpus());
        if all.len() < 4 || all != topo.cpus() {
            return Ok(());
        }
        let sleep = || std::process::Command::new("sleep").arg("5").process_group(0).spawn();
        let p = Pins::new(Some(topo.clone()), Some(all.clone()));
        // An ordinary step, started with nothing held, then a share taken beside it.
        let mut before = sleep()?;
        p.started("ordinary", i32::try_from(before.id())?);
        let share = p.hold("bench", 2).ok_or_else(|| anyhow!("no share"))?;
        let moved = allowed(before.id());
        assert!(!moved.is_empty() && moved.iter().all(|c| !share.contains(c)), "moved off the share: {} (share {})", list(&moved), list(&share));
        // The bench's step runs on its share alone; one more ordinary step, on the rest.
        let mut bench = sleep()?;
        p.started("bench", i32::try_from(bench.id())?);
        assert_eq!(allowed(bench.id()), share);
        let mut after = sleep()?;
        p.started("ordinary", i32::try_from(after.id())?);
        assert_eq!(allowed(after.id()), moved);
        // Started on them from its first instruction: the thread that starts it lends it its CPUs,
        // and has its own back after.
        let mine = std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
        let lent = {
            let set = cpu_set(&share).ok_or_else(|| anyhow!("no set"))?;
            let me = nix::unistd::Pid::from_raw(0);
            let before = nix::sched::sched_getaffinity(me)?;
            nix::sched::sched_setaffinity(me, &set)?;
            let c = sleep();
            nix::sched::sched_setaffinity(me, &before)?;
            c?
        };
        assert_eq!(allowed(lent.id()), share, "born on the share");
        assert_eq!(std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default().lines().find(|l| l.starts_with("Cpus_allowed_list")), mine.lines().find(|l| l.starts_with("Cpus_allowed_list")), "the thread has its own back");
        let mut lent = lent;
        let _ = lent.kill();
        let _ = lent.wait();
        // The share let go: the ordinary steps may run anywhere again.
        p.release("bench");
        assert_eq!(allowed(before.id()), all);
        assert_eq!(allowed(after.id()), all);
        for c in [&mut before, &mut bench, &mut after] {
            let _ = c.kill();
            let _ = c.wait();
        }
        Ok(())
    }
}
