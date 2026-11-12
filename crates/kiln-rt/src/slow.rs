//! Polls that held their shard past [`SLOW_POLL_NS`], by the code that held it (kiln's goal 9, plan
//! 01a1a87b2cec step 2): each such poll is counted against its task's spawn site (`#[track_caller]`, as the
//! registry records it for the watchdog, `crate::watch`) and the label the task's code set during the poll
//! ([`crate::futures::label`]: a request's route, say), with how long the polls held the shard in all and
//! the longest of them. The watchdog names a task only once it has held its shard a step's worth of polls
//! (16 ms on kiln's server); these are the far shorter holds that make up a shard's delay at its tail.
//!
//! **One writer, any reader, no lock.** Each shard keeps a table of its own on its registry entry, which only
//! the shard writes, after a poll past the threshold (one compare per poll otherwise). An entry's key is set
//! once ([`OnceLock`]: the shard is its only writer, so setting it never waits, and a reader only ever asks
//! whether it is set, which never waits either) and never changes; its counts are relaxed atomics, so a
//! reader may see a count and a total from moments apart, never a torn key. A table that is full counts what
//! doesn't fit in one entry with no key.

use std::panic::Location;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::{self, SlotHolder};

/// The hold a poll is counted past, nanoseconds: a tenth of the shard delay kiln's goal 9 holds a shard's
/// ready work to at p99.9 (1 ms, plan 01a1a87b2cec step 2), and the timer wheel's tick on kiln's server (100
/// µs), the finest wait a shard keeps.
pub const SLOW_POLL_NS: u64 = 100_000;

/// Format: entries in a shard's table. Each distinct pair of spawn site and label that held the shard takes
/// one; kiln's server spawns from a few dozen sites and labels its requests by a few dozen routes, so a pair
/// past this many is counted with no key rather than evicting one.
pub const ENTRIES: usize = 128;

/// What a task's code says it is doing, set for the poll it is in ([`crate::futures::label`]): a `'static`, so
/// a table keeps it by reference, as it keeps a spawn site's `Location`.
#[derive(Debug)]
pub struct Label {
    /// The name the table's readers show.
    pub name: &'static str,
}

/// An entry's key: the task's spawn site and the label set during its poll.
#[derive(Clone, Copy, Debug)]
struct Key {
    site: Option<&'static Location<'static>>,
    label: Option<&'static Label>,
}

impl Key {
    /// The same site (by its file, line and column) and the same label (by its address).
    fn is(&self, other: &Key) -> bool {
        self.site == other.site
            && match (self.label, other.label) {
                (Some(a), Some(b)) => std::ptr::eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }

    /// The entry its probe starts at, of `n`: the site's line and column and the label's address mixed by
    /// Knuth's multiplicative hash.
    fn start(&self, n: usize) -> usize {
        /// Format: 2^64 over the golden ratio.
        const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;
        let site = self.site.map_or(0, |s| u64::from(s.line()).wrapping_shl(32) | u64::from(s.column()));
        let label = self.label.map_or(0, |l| u64::try_from(std::ptr::from_ref(l).addr()).unwrap_or(0));
        let mixed = (site ^ label.rotate_left(29)).wrapping_mul(GOLDEN).wrapping_shr(32);
        usize::try_from(mixed).ok().and_then(|m| m.checked_rem(n)).unwrap_or(0)
    }
}

/// One entry: a key set once, then the polls it held and for how long, those of them the host held, the time on the CPU
/// of those whose thread could say it, and those past the step quantum.
#[derive(Debug, Default)]
struct Entry {
    key: OnceLock<Key>,
    polls: AtomicU64,
    total_ns: AtomicU64,
    longest_ns: AtomicU64,
    host_polls: AtomicU64,
    host_ns: AtomicU64,
    cpu_polls: AtomicU64,
    cpu_held_ns: AtomicU64,
    cpu_ns: AtomicU64,
    longest_cpu_ns: AtomicU64,
    past_polls: AtomicU64,
    past_cpu_polls: AtomicU64,
}

impl Entry {
    /// Counts one poll of `ns`, `cpu` of it on the CPU where its thread said, the host's when `host`, against a step
    /// quantum of `quantum_ns` (none: 0) (the writer's alone).
    fn count(&self, ns: u64, cpu: Option<u64>, host: bool, quantum_ns: u64) {
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.longest_ns.fetch_max(ns, Ordering::Relaxed);
        if quantum_ns > 0 && ns > quantum_ns {
            self.past_polls.fetch_add(1, Ordering::Relaxed);
        }
        if quantum_ns > 0 && cpu.is_some_and(|c| c.min(ns) > quantum_ns) {
            self.past_cpu_polls.fetch_add(1, Ordering::Relaxed);
        }
        if host {
            self.host_ns.fetch_add(ns, Ordering::Relaxed);
            self.host_polls.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(cpu) = cpu.map(|c| c.min(ns)) {
            self.cpu_polls.fetch_add(1, Ordering::Relaxed);
            self.cpu_held_ns.fetch_add(ns, Ordering::Relaxed);
            self.cpu_ns.fetch_add(cpu, Ordering::Relaxed);
            self.longest_cpu_ns.fetch_max(cpu, Ordering::Relaxed);
        }
        self.polls.fetch_add(1, Ordering::Relaxed);
    }

    /// The entry as a reader sees it: none while it is free or has counted nothing yet.
    fn read(&self) -> Option<SlowPoll> {
        let key = self.key.get().copied().unwrap_or(Key { site: None, label: None });
        let polls = self.polls.load(Ordering::Relaxed);
        (polls > 0).then(|| SlowPoll {
            site: key.site,
            label: key.label,
            polls,
            total_ns: self.total_ns.load(Ordering::Relaxed),
            longest_ns: self.longest_ns.load(Ordering::Relaxed),
            host_polls: self.host_polls.load(Ordering::Relaxed),
            host_ns: self.host_ns.load(Ordering::Relaxed),
            cpu_polls: self.cpu_polls.load(Ordering::Relaxed),
            cpu_held_ns: self.cpu_held_ns.load(Ordering::Relaxed),
            cpu_ns: self.cpu_ns.load(Ordering::Relaxed),
            longest_cpu_ns: self.longest_cpu_ns.load(Ordering::Relaxed),
            past_polls: self.past_polls.load(Ordering::Relaxed),
            past_cpu_polls: self.past_cpu_polls.load(Ordering::Relaxed),
        })
    }
}

/// A shard's table of slow polls (see the module's notes).
#[derive(Debug)]
pub(crate) struct SlowTable {
    entries: Box<[Entry]>,
    /// What a full table had no entry for.
    rest: Entry,
}

impl Default for SlowTable {
    fn default() -> Self {
        Self {
            entries: (0..ENTRIES).map(|_| Entry::default()).collect(),
            rest: Entry::default(),
        }
    }
}

impl SlowTable {
    /// One poll that held the shard `ns`, by the task spawned at `site` with `label` set, the host's when `host`
    /// (see [`SlowPoll::host_polls`]): the shard's own call.
    #[cfg(test)]
    pub(crate) fn note(
        &self,
        site: Option<&'static Location<'static>>,
        label: Option<&'static Label>,
        ns: u64,
        host: bool,
    ) {
        self.note_with_cpu(site, label, ns, None, host, 0);
    }

    /// [`SlowTable::note`], with `cpu` of the poll's hold on the CPU where the shard's thread said (`crate::task_clock`;
    /// kiln's goal 9, plan 01a1a87b2cec step 3), and the step quantum the shard ran it with (`quantum_ns`; none: 0).
    pub(crate) fn note_with_cpu(
        &self,
        site: Option<&'static Location<'static>>,
        label: Option<&'static Label>,
        ns: u64,
        cpu: Option<u64>,
        host: bool,
        quantum_ns: u64,
    ) {
        let key = Key { site, label };
        let n = self.entries.len();
        let start = key.start(n);
        let found = (0..n)
            .filter_map(|i| self.entries.get(start.wrapping_add(i).checked_rem(n)?))
            .find(|entry| entry.key.get().is_none_or(|k| k.is(&key)));
        match found {
            // The shard is the table's only writer: a free entry it finds is still free as it sets the key.
            Some(entry) => {
                let _ = entry.key.set(key);
                entry.count(ns, cpu, host, quantum_ns);
            }
            None => self.rest.count(ns, cpu, host, quantum_ns),
        }
    }

    /// Every entry with a poll counted, then what a full table couldn't keep (no site, no label), as they
    /// stand: any thread's.
    pub(crate) fn read(&self) -> Vec<SlowPoll> {
        self.entries
            .iter()
            .chain(std::iter::once(&self.rest))
            .filter_map(Entry::read)
            .collect()
    }
}

/// The spawn sites whose polls ran past the step quantum, as a shard learned them (kiln's goal 9, plan 01a1a87b2cec step
/// 3): where a poll's time on the CPU costs a call to read (macOS, `crate::shard_loop::THREAD_CPU_FOR_SLOW_POLLS`), the
/// shard reads it only for tasks spawned at one of these, from the poll after their site's first past the quantum, so a
/// site whose polls never ran past it costs none. The shard's own, one writer and no reader: open addressing over twice
/// [`ENTRIES`] places, taken as the first site is learned, and no more sites than [`ENTRIES`] (the slow table's bound).
#[derive(Debug, Default)]
pub(crate) struct CpuSites {
    places: Box<[Option<&'static Location<'static>>]>,
    learned: usize,
}

impl CpuSites {
    /// Whether `site` was learned.
    pub(crate) fn has(&self, site: &'static Location<'static>) -> bool {
        self.place_of(site)
            .and_then(|place| self.places.get(place))
            .is_some_and(Option::is_some)
    }

    /// Learns `site`: whether it is new and there was room for it.
    pub(crate) fn learn(&mut self, site: &'static Location<'static>) -> bool {
        if self.learned >= ENTRIES {
            return false;
        }
        if self.places.is_empty() {
            self.places = (0..ENTRIES.saturating_mul(2)).map(|_| None).collect();
        }
        let Some(place) = self.place_of(site).and_then(|place| self.places.get_mut(place)) else {
            return false;
        };
        if place.is_some() {
            return false;
        }
        *place = Some(site);
        self.learned = self.learned.saturating_add(1);
        true
    }

    /// The place `site` holds, else the free place it would take: probed from where its line and column hash to (as
    /// the slow table's keys are). Never more than half the places are taken, so a probe always ends.
    fn place_of(&self, site: &'static Location<'static>) -> Option<usize> {
        let n = self.places.len();
        let start = Key { site: Some(site), label: None }.start(n);
        (0..n)
            .filter_map(|i| start.wrapping_add(i).checked_rem(n))
            .find(|&place| {
                self.places
                    .get(place)
                    .is_some_and(|known| known.is_none_or(|known| same_site(known, site)))
            })
    }
}

/// Whether two spawn sites are one: the same file, line and column (one call site's `Location` may be two statics, one
/// per instance of a generic caller).
pub(crate) fn same_site(a: &Location<'_>, b: &Location<'_>) -> bool {
    std::ptr::eq(a, b) || (a.line() == b.line() && a.column() == b.column() && a.file() == b.file())
}

/// What held a shard: a spawn site and a label, with the polls past [`SLOW_POLL_NS`] they held it and for
/// how long. Counts are since the shard started.
#[derive(Clone, Copy, Debug)]
pub struct SlowPoll {
    /// Where the task was spawned; `None` for what a full table had no entry for, or a task with none recorded.
    pub site: Option<&'static Location<'static>>,
    /// The label its code set during the poll, if any.
    pub label: Option<&'static Label>,
    /// Polls past the threshold.
    pub polls: u64,
    /// Their holds added up, nanoseconds.
    pub total_ns: u64,
    /// The longest of them, nanoseconds.
    pub longest_ns: u64,
    /// Of them, the polls the host held: the shard's runtime found its thread runnable and off the CPU for the
    /// poll (preempted, `crate::attribution`), which it finds out only for a poll past twice its step quantum,
    /// so a shorter one is never counted here. Code didn't hold the shard in these: cores of its own would
    /// (kiln's goal 9, plan 01a1a87b2cec steps 3 and 4).
    pub host_polls: u64,
    /// Their holds added up, nanoseconds.
    pub host_ns: u64,
    /// Of them, the polls whose shard's thread said its run time (`crate::task_clock`: Linux, where a self-monitoring
    /// perf event could be opened, every one; macOS, its thread CPU clock, those of a site after its first poll past the
    /// quantum, [`CpuSites`]; kiln's goal 9, plan 01a1a87b2cec step 3).
    pub cpu_polls: u64,
    /// Their holds added up, nanoseconds.
    pub cpu_held_ns: u64,
    /// Their time on the CPU added up, nanoseconds: the rest of their holds their thread was off the CPU, the host's
    /// (preempted) or a call's that blocked.
    pub cpu_ns: u64,
    /// The longest any of them ran on the CPU, nanoseconds.
    pub longest_cpu_ns: u64,
    /// Of them, the polls that held their shard past the step quantum it ran them with: what a reader counts between two of
    /// its reads, where the longest (since the shard started) can't say when a poll passed the quantum (kiln's goal 9,
    /// plan 01a1a87b2cec step 3: the lab's runners connect before what it measures, and their first messages' polls stood
    /// for every round of it).
    pub past_polls: u64,
    /// Of them, the polls that ran on the CPU past the step quantum, where their thread said its time there.
    pub past_cpu_polls: u64,
}

/// The slow polls of the shard `holder` names, as they stand (any thread; nothing of the shard's own is
/// touched). Empty for a holder no longer registered.
pub fn slow_polls(holder: SlotHolder) -> Vec<SlowPoll> {
    registry::with_holder(holder, |entry| entry.slow.read()).unwrap_or_default()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    static ROUTE: Label = Label { name: "GET api/harness/sessions/:id" };
    static OTHER: Label = Label { name: "hub" };

    /// Goal 9, plan 01a1a87b2cec step 2: polls are counted by site and label, each pair its own entry with its
    /// count, total and longest; the same site under another label is another entry.
    #[test]
    fn g9_s2_slow_polls_count_by_site_and_label() {
        let table = SlowTable::default();
        let here = Location::caller();
        table.note(Some(here), Some(&ROUTE), 150_000, false);
        table.note(Some(here), Some(&ROUTE), 450_000, false);
        table.note(Some(here), Some(&OTHER), 200_000, false);
        table.note(Some(here), None, 120_000, false);
        let mut read = table.read();
        read.sort_by_key(|p| std::cmp::Reverse(p.total_ns));
        assert_eq!(read.len(), 3, "{read:?}");
        let first = read[0];
        assert_eq!((first.polls, first.total_ns, first.longest_ns), (2, 600_000, 450_000));
        assert_eq!(first.label.map(|l| l.name), Some(ROUTE.name));
        assert_eq!(first.site, Some(here));
        assert!(read.iter().any(|p| p.label.is_none() && p.polls == 1));
    }

    /// A table with every entry taken counts the rest in one entry with no key, and loses no poll.
    #[test]
    fn g9_s2_a_full_table_keeps_the_rest_unkeyed() {
        let table = SlowTable::default();
        let labels: Vec<&'static Label> = (0..ENTRIES + 5).map(|_| &*Box::leak(Box::new(Label { name: "a route" }))).collect();
        for label in &labels {
            table.note(None, Some(label), 100_001, false);
        }
        let read = table.read();
        assert_eq!(read.iter().map(|p| p.polls).sum::<u64>(), labels.len() as u64);
        let rest: Vec<&SlowPoll> = read.iter().filter(|p| p.label.is_none() && p.site.is_none()).collect();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].polls, 5);
    }

    /// Goal 9, plan 01a1a87b2cec steps 3 and 4: of an entry's polls, those the host held are counted apart, with
    /// their time, in its count and total too: what held the shard that no code of it did.
    #[test]
    fn g9_s3_the_hosts_polls_are_counted_apart() {
        let table = SlowTable::default();
        let here = Location::caller();
        table.note(Some(here), Some(&ROUTE), 600_000, true);
        table.note(Some(here), Some(&ROUTE), 150_000, false);
        table.note(Some(here), Some(&ROUTE), 900_000, true);
        let read = table.read();
        assert_eq!(read.len(), 1, "{read:?}");
        let p = read[0];
        assert_eq!((p.polls, p.total_ns, p.longest_ns), (3, 1_650_000, 900_000));
        assert_eq!((p.host_polls, p.host_ns), (2, 1_500_000));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: of an entry's polls, those whose thread said its time on the CPU are counted
    /// with their holds and that time, and the longest on the CPU; a poll's time on the CPU is never more than its
    /// hold; one without a reading counts in the hold alone.
    #[test]
    fn g9_s3_a_polls_time_on_the_cpu_is_counted_beside_its_hold() {
        let table = SlowTable::default();
        let here = Location::caller();
        table.note_with_cpu(Some(here), Some(&ROUTE), 2_000_000, Some(150_000), false, 0);
        table.note_with_cpu(Some(here), Some(&ROUTE), 300_000, Some(280_000), false, 0);
        table.note_with_cpu(Some(here), Some(&ROUTE), 400_000, Some(900_000), false, 0);
        table.note_with_cpu(Some(here), Some(&ROUTE), 500_000, None, false, 0);
        let read = table.read();
        let p = read[0];
        assert_eq!((p.polls, p.total_ns), (4, 3_200_000));
        assert_eq!((p.cpu_polls, p.cpu_held_ns, p.cpu_ns, p.longest_cpu_ns), (3, 2_700_000, 830_000, 400_000));
        assert_eq!((p.past_polls, p.past_cpu_polls), (0, 0), "no quantum, none past it");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: of an entry's polls, those past the step quantum they ran with are counted, by
    /// their hold and by their time on the CPU (never more than the hold), so a reader can tell when one passed it.
    #[test]
    fn g9_s3_polls_past_the_quantum_are_counted_by_hold_and_on_the_cpu() {
        let table = SlowTable::default();
        let here = Location::caller();
        let q = 266_666;
        table.note_with_cpu(Some(here), Some(&ROUTE), 2_000_000, Some(150_000), false, q);
        table.note_with_cpu(Some(here), Some(&ROUTE), 300_000, Some(280_000), false, q);
        table.note_with_cpu(Some(here), Some(&ROUTE), 260_000, Some(900_000), false, q);
        table.note_with_cpu(Some(here), Some(&ROUTE), 500_000, None, false, q);
        table.note_with_cpu(Some(here), Some(&ROUTE), 266_666, Some(266_666), false, q);
        let p = table.read()[0];
        assert_eq!(p.past_polls, 3, "held past the quantum: 2 ms, 300 µs and 500 µs");
        assert_eq!(p.past_cpu_polls, 1, "on the CPU past it: 280 µs (900 µs of a 260 µs hold is 260)");
    }

    /// One shard on this thread, its quantum `quantum_ns` (its step budget: it tracks no wake).
    fn one_shard(quantum_ns: u64) -> crate::runtime::LocalRuntime {
        let config = crate::runtime::RuntimeConfig {
            shards: 1,
            tasks_per_shard: 8,
            timers_per_shard: 8,
            interests_per_shard: 8,
            ring_entries: 16,
            step_budget_ns: quantum_ns,
            timer_tick_ns: 100_000,
            batch: 16,
            pin: false,
            cores: Vec::new(),
            page_bytes: 4096,
            spin_ns: 0,
            wake_tracking: None,
            latency_objective_ns: None,
        };
        crate::runtime::LocalRuntime::new(&config).unwrap()
    }

    /// Spins `spin_ns` on the CPU, by the thread's own clock where it has one (a loaded machine that preempts the spin
    /// stretches its wall time, not its time on the CPU), else by the wall clock; read between runs of plain arithmetic, so
    /// the spin's time is the task's own, not the clock's calls (Linux's task clock counts user time alone).
    fn spin_on_the_cpu(spin_ns: u64) {
        let began = std::time::Instant::now();
        let cpu_from = crate::attribution::thread_cpu_now();
        let mut spins = 0u64;
        loop {
            for _ in 0..u16::MAX {
                spins = std::hint::black_box(spins.wrapping_add(1));
            }
            let ran = match cpu_from {
                Some(from) => crate::attribution::thread_cpu_now().map_or(0, |now| now.saturating_sub(from)),
                None => u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX),
            };
            if ran >= spin_ns {
                break;
            }
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3 (lab-main, 2026-11-06): a shard's polls that run past the threshold on the CPU say
    /// their time there wherever the shard's thread has a CPU clock: Linux's task clock where its perf event opens, and
    /// macOS's thread CPU clock, which no task clock replaces there. The Mac lab's step 3 table (g9p31-mac) had every listed
    /// path's on-CPU column empty: the shard took a slow poll's CPU from the task clock alone, which opens on Linux only.
    /// The root spins twice, each far past the quantum, and at least its second spin is read where a clock is (on macOS a
    /// reading is a call, so a site's polls are read there from the one after its first past the quantum). With no clock
    /// (Windows, a Linux whose perf event is refused) the polls count in their holds alone.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn g9_s3_a_shards_slow_poll_says_its_time_on_the_cpu_where_its_thread_has_a_cpu_clock() {
        let clock = crate::task_clock::TaskClock::open().is_some()
            || (cfg!(target_os = "macos") && crate::attribution::thread_cpu_now().is_some());
        let mut rt = one_shard(1_000_000);
        let shard = rt.shard_id();
        // Each spin 200 times the threshold, 20 times the quantum.
        let spin_ns = SLOW_POLL_NS.saturating_mul(200);
        rt.block_on(async move {
            spin_on_the_cpu(spin_ns);
            crate::futures::yield_now().await;
            spin_on_the_cpu(spin_ns);
        })
        .unwrap();
        let polls = crate::registry::holder_of(shard.0).map(slow_polls).unwrap_or_default();
        let spun = polls.iter().find(|p| p.longest_ns >= spin_ns).copied();
        assert!(spun.is_some(), "no poll of {spin_ns} ns among the shard's slow polls: {polls:?}");
        let Some(spun) = spun else { return };
        assert_eq!(spun.polls, 2, "the root's two spins: {spun:?}");
        if !clock {
            assert_eq!(spun.cpu_polls, 0, "no CPU clock on this thread, yet a time on the CPU: {spun:?}");
            return;
        }
        assert!(spun.cpu_polls >= 1, "the spins' polls said no time on the CPU: {spun:?}");
        // At least half the spin: the shard's clock (Linux's task clock, user time alone) and the spin's (the thread's
        // CPU clock, the kernel's time in its calls too) are two clocks of one thread.
        assert!(spun.longest_cpu_ns >= spin_ns / 2 && spun.longest_cpu_ns <= spun.longest_ns, "spun {spin_ns} ns on the CPU: {spun:?}");
    }

    /// Spawns `future` on the current shard from the caller's site, and says that site: the slow table's key for its polls.
    #[track_caller]
    fn spawn_here(future: impl std::future::Future<Output = ()> + 'static) -> (crate::shard::TaskId, &'static Location<'static>) {
        (crate::futures::spawn(future).unwrap(), Location::caller())
    }

    /// Where the shard reads its thread's CPU clock for slow polls (macOS) and the thread has one.
    fn thread_clock_read() -> bool {
        crate::shard_loop::THREAD_CPU_FOR_SLOW_POLLS
            && crate::task_clock::TaskClock::open().is_none()
            && crate::attribution::thread_cpu_now().is_some()
    }

    /// Goal 9, plan 01a1a87b2cec step 3 (main's ask, 2026-11-06): a site is learned once; one never learned isn't had.
    #[test]
    fn g9_s3_cpu_sites_learn_a_site_once() {
        let mut sites = CpuSites::default();
        let (a, b) = (Location::caller(), Location::caller());
        assert!(!sites.has(a), "an empty set has no site");
        assert!(sites.learn(a));
        assert!(!sites.learn(a), "learned twice");
        assert!(sites.has(a) && !sites.has(b));
        assert!(sites.learn(b) && sites.has(b) && sites.has(a));
    }

    /// Goal 9, plan 01a1a87b2cec step 3 (main's ask, 2026-11-06): where a slow poll's time on the CPU costs a call to read
    /// (macOS: a Mach trap, 107 to 129 ns on Apple silicon), the polls of a site that never ran past the quantum read
    /// nothing. A task's thousand quick polls run beside a site the shard learned (three spins, the first teaching it), and
    /// the thread's clock is read four times, counted: the learned site's other two spins, at each one's start and end.
    /// Linux is as before: its task clock (a page, no call) reads every slow poll, and the thread's clock is never read.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn g9_s3_a_site_never_past_the_quantum_reads_no_cpu_clock() {
        // A quantum of 5 ms, which a quick poll passes only where the host holds its thread that long.
        let quantum_ns = 5_000_000;
        let mut rt = one_shard(quantum_ns);
        let shard = rt.shard_id();
        let spin_ns = SLOW_POLL_NS.saturating_mul(200);
        crate::shard_loop::CPU_READS.with(|reads| reads.set(0));
        let slow_site = rt
            .block_on(async move {
                let (slow, site) = spawn_here(async move {
                    spin_on_the_cpu(spin_ns);
                    crate::futures::yield_now().await;
                    spin_on_the_cpu(spin_ns);
                    crate::futures::yield_now().await;
                    spin_on_the_cpu(spin_ns);
                });
                let (quick, _) = spawn_here(async {
                    for _ in 0..1000 {
                        crate::futures::yield_now().await;
                    }
                });
                crate::futures::join(slow).await.unwrap();
                crate::futures::join(quick).await.unwrap();
                site
            })
            .unwrap();
        let reads = crate::shard_loop::CPU_READS.with(std::cell::Cell::get);
        let polls = crate::registry::holder_of(shard.0).map(slow_polls).unwrap_or_default();
        // The slow task's entry, by its site: a loaded host may hold a quick poll as long as a spin.
        let spun = polls.iter().find(|p| p.site == Some(slow_site)).copied().unwrap();
        assert_eq!(spun.polls, 3, "the slow task's three spins: {polls:?}");
        if crate::task_clock::TaskClock::open().is_some() {
            assert_eq!((reads, spun.cpu_polls), (0, 3), "Linux reads every slow poll on its task clock: {spun:?}");
            return;
        }
        if !thread_clock_read() {
            assert_eq!((reads, spun.cpu_polls), (0, 0), "no clock, yet a reading: {spun:?}");
            return;
        }
        assert_eq!(spun.cpu_polls, 2, "the first spin teaches the shard the site, the next two are read: {spun:?}");
        // A loaded host that held a quick poll (or the root's) past the quantum teaches the shard that site too: its
        // later polls are read, and the slow table shows it. Otherwise the reads are the slow site's alone.
        let others_past = polls.iter().any(|p| p.longest_ns > quantum_ns && p.site != spun.site);
        if others_past {
            assert!(reads >= 4, "{reads} reads: {polls:?}");
        } else {
            assert_eq!(reads, 4, "a site never past the quantum read the thread's clock: {polls:?}");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 3 (main's ask, 2026-11-06): once a poll of a site ran past the quantum, the polls of
    /// every task spawned there say their time on the CPU from the next on: a task live then (installed: its first poll
    /// yielded) and one installed after. Of three tasks from one site that spin, the first spin goes unread where a
    /// reading costs a call (macOS) and the other two are read. Linux, its task clock reading every poll, is as before:
    /// all three, the site's first among them.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn g9_s3_a_site_past_the_quantum_once_has_its_tasks_next_polls_on_the_cpu() {
        let mut rt = one_shard(1_000_000);
        let shard = rt.shard_id();
        let spin_ns = SLOW_POLL_NS.saturating_mul(200);
        let spawned_at = rt
            .block_on(async move {
                let mut tasks = Vec::new();
                let mut spawned_at = None;
                for turn in 0..3u8 {
                    // One site, this call: the first task yields before it spins, so it is installed when the second's
                    // spin teaches the shard the site; the third is installed after.
                    let (task, site) = spawn_here(async move {
                        if turn == 0 {
                            crate::futures::yield_now().await;
                        }
                        spin_on_the_cpu(spin_ns);
                    });
                    tasks.push(task);
                    spawned_at = Some(site);
                }
                for task in tasks {
                    crate::futures::join(task).await.unwrap();
                }
                spawned_at
            })
            .unwrap();
        let polls = crate::registry::holder_of(shard.0).map(slow_polls).unwrap_or_default();
        // The three tasks' entry, by their site: a loaded host may hold the root's quick polls as long as a spin.
        let site = polls.iter().find(|p| p.site.is_some() && p.site == spawned_at).copied().unwrap();
        // The three spins, and the first task's quick poll where a loaded host held it past the threshold.
        assert!(site.polls >= 3, "the three tasks' spins: {polls:?}");
        let quick_held = site.polls > 3;
        if crate::task_clock::TaskClock::open().is_some() {
            assert_eq!(site.cpu_polls, site.polls, "Linux reads every slow poll on its task clock: {site:?}");
        } else if thread_clock_read() {
            // The site's first poll past the quantum teaches the shard and its tasks' next are read: two of the spins, or
            // all three where the host held the quick poll past the quantum.
            if quick_held {
                assert!(site.cpu_polls >= 2, "{site:?}");
            } else {
                assert_eq!(site.cpu_polls, 2, "the site's first spin teaches the shard, its tasks' next two are read: {site:?}");
            }
            assert!(site.longest_cpu_ns >= spin_ns / 2 && site.longest_cpu_ns <= site.longest_ns, "{site:?}");
        } else {
            assert_eq!(site.cpu_polls, 0, "no clock, yet a reading: {site:?}");
        }
    }
}
