//! What woke a parked shard by a timer, by where the timer was armed (kiln's goal 9, plan 01a1a87b2cec step 5: the
//! shards' timer wakes, ~457 a second in kiln's lab, g9p16, 234 of them its delay probe's; the rest is what to find).
//! Every sleep takes the place it was asked for at (`#[track_caller]`: [`crate::futures::sleep`],
//! [`crate::futures::within`], and [`crate::futures::sleep_from`] for a wrapper that passes its own caller's on), and
//! the timer it arms keeps it. A park the wheel's deadline ended is counted against the site of the first timer that
//! fell due in the step after it, or with no site where none had (a wait that returned before its deadline); a
//! quiet sleep's own wake ([`crate::quiet`]) against the quiet sleep's site. Their sum is the shard's
//! `woken_by_timer`.
//!
//! The same table counts the kicks that found the shard parked by the task each woke, at its spawn site ([`kicked_tasks`]:
//! who another thread wakes on a parked shard; kiln's goal 9 step 5, the shards' ~1,750 kicks a second, g9p16).
//!
//! **No lock**, as [`crate::slow`]'s table: each shard keeps them on its registry entry; an entry's site is set once (a
//! site's first count, which may race another's first for a free entry where other threads write, the loser going on to
//! the next) and its count is a relaxed atomic. A table that is full counts what doesn't fit with no site.

use std::panic::Location;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::{self, SlotHolder};

/// Format: entries in a shard's table, as many as [`crate::slow`]'s: kiln's server arms its timers from a few dozen
/// places (its sleeps, deadlines and intervals), so a site past this many is counted with no site rather than evicting
/// one.
pub const ENTRIES: usize = crate::slow::ENTRIES;

/// One entry: a site set once, and its count.
#[derive(Debug, Default)]
struct Entry {
    site: OnceLock<&'static Location<'static>>,
    count: AtomicU64,
}

/// A shard's table of counts by site (see the module's notes).
#[derive(Debug)]
pub(crate) struct SiteCounts {
    entries: Box<[Entry]>,
    /// The counts of a site a full table had no entry for, and those of no site.
    rest: AtomicU64,
}

impl Default for SiteCounts {
    fn default() -> Self {
        Self {
            entries: (0..ENTRIES).map(|_| Entry::default()).collect(),
            rest: AtomicU64::new(0),
        }
    }
}

/// The entry `site`'s probe starts at, of `n`: its line and column mixed by Knuth's multiplicative hash.
fn start(site: &Location<'_>, n: usize) -> usize {
    /// Format: 2^64 over the golden ratio.
    const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;
    let key = u64::from(site.line()).wrapping_shl(32) | u64::from(site.column());
    let mixed = key.wrapping_mul(GOLDEN).wrapping_shr(32);
    usize::try_from(mixed).ok().and_then(|m| m.checked_rem(n)).unwrap_or(0)
}

impl SiteCounts {
    /// One more of `site`'s (none: no site, or none kept): a timer wake by the timer armed there, the shard's own call;
    /// a kick that found the shard parked by the task spawned there, any thread's.
    pub(crate) fn note(&self, site: Option<&'static Location<'static>>) {
        let Some(site) = site else {
            self.rest.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let n = self.entries.len();
        let first = start(site, n);
        for i in 0..n {
            let Some(entry) = first.wrapping_add(i).checked_rem(n).and_then(|at| self.entries.get(at)) else { continue };
            if entry.site.get().is_none() {
                let _ = entry.site.set(site);
            }
            // Set here, or by another writer's race for it to its own site: then on to the next.
            if entry.site.get().is_some_and(|s| *s == site) {
                entry.count.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.rest.fetch_add(1, Ordering::Relaxed);
    }

    /// Every site with a count, then the rest (no site), as they stand: any thread's.
    pub(crate) fn read(&self) -> Vec<SiteCount> {
        let rest = self.rest.load(Ordering::Relaxed);
        self.entries
            .iter()
            .filter_map(|entry| {
                let count = entry.count.load(Ordering::Relaxed);
                (count > 0).then(|| SiteCount { site: entry.site.get().copied(), count })
            })
            .chain((rest > 0).then_some(SiteCount { site: None, count: rest }))
            .collect()
    }
}

/// One site's count of a shard's, since the shard started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SiteCount {
    /// Where the timer was armed, or the woken task spawned; none for the counts of no site, and those a full table had
    /// no entry for.
    pub site: Option<&'static Location<'static>>,
    /// Timer wakes, or kicks claimed, since the shard started.
    pub count: u64,
}

/// The timer wakes of the shard `holder` names, by where each timer was armed, as they stand (any thread; nothing of the
/// shard's own is touched). Empty for a holder no longer registered.
pub fn timer_wakes(holder: SlotHolder) -> Vec<SiteCount> {
    registry::with_holder(holder, |entry| entry.timer_sites.read()).unwrap_or_default()
}

/// The kicks that found the shard `holder` names parked, by where the task each woke was spawned (none: a control
/// message's, or a task with no site kept), as they stand. Empty for a holder no longer registered.
pub fn kicked_tasks(holder: SlotHolder) -> Vec<SiteCount> {
    registry::with_holder(holder, |entry| entry.kicked_tasks.read()).unwrap_or_default()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    impl SiteCounts {
        /// A table of `n` entries (a full one, in a test, without `ENTRIES` sites).
        fn with_entries(n: usize) -> Self {
            Self { entries: (0..n).map(|_| Entry::default()).collect(), rest: AtomicU64::new(0) }
        }
    }

    #[track_caller]
    fn here() -> &'static Location<'static> {
        Location::caller()
    }

    /// Kiln's goal 9, plan 01a1a87b2cec step 5: wakes are counted by site, each its own entry, and a wake of no site
    /// with the rest; a table every entry of which is taken counts a new site with the rest, losing none.
    #[test]
    fn g9_s5_timer_wakes_count_by_site() {
        let a = here();
        let b = here();
        let c = here();
        let table = SiteCounts::default();
        for site in [Some(a), Some(b), Some(a), None] {
            table.note(site);
        }
        let mut read = table.read();
        read.sort_by_key(|w| std::cmp::Reverse(w.count));
        assert_eq!(read, vec![SiteCount { site: Some(a), count: 2 }, SiteCount { site: Some(b), count: 1 }, SiteCount { site: None, count: 1 }]);
        let full = SiteCounts::with_entries(2);
        for site in [a, b, c, c] {
            full.note(Some(site));
        }
        let read = full.read();
        assert_eq!(read.iter().map(|w| w.count).sum::<u64>(), 4, "{read:?}");
        assert_eq!(read.iter().filter(|w| w.site.is_none()).map(|w| w.count).sum::<u64>(), 2, "{read:?}");
    }

    fn config() -> crate::runtime::RuntimeConfig {
        crate::runtime::RuntimeConfig {
            shards: 1,
            tasks_per_shard: 16,
            timers_per_shard: 4,
            interests_per_shard: 4,
            ring_entries: 16,
            step_budget_ns: 1_000_000,
            timer_tick_ns: 100_000,
            batch: 16,
            pin: false,
            cores: Vec::new(),
            page_bytes: 4096,
            spin_ns: 0,
            wake_tracking: None,
            latency_objective_ns: None,
        }
    }

    /// Shape: a sleep long against a park's start and the host's wake (microseconds), so the shard parks for it.
    const NAP_NS: u64 = 5_000_000;

    /// Shape: how long the test waits on a shard's answer.
    const ANSWER: std::time::Duration = std::time::Duration::from_secs(10);

    /// The wakes `table` counted against the place in this file at `line`.
    fn at_line(table: &[SiteCount], line: u32) -> u64 {
        table.iter().filter(|w| w.site.is_some_and(|s| s.file() == file!() && s.line() == line)).map(|w| w.count).sum()
    }

    /// Kiln's goal 9, plan 01a1a87b2cec step 5: a parked shard's timer wake counts against where its sleep was asked
    /// for: a sleep's own call, a deadline's (`within`), a wrapper's caller (`sleep_from`), a quiet sleep's; and every
    /// timer wake is counted somewhere. Do: on a running runtime's idle shard, a task that sleeps, waits on a deadline
    /// that passes, sleeps through a wrapper, and sleeps quietly, each long enough that the shard parks, one at a time.
    /// Expect: a wake at each place, and the table's wakes the shard's timer wakes.
    #[test]
    fn g9_s5_a_parked_shards_timer_wake_counts_against_where_its_sleep_was_asked_for() {
        let rt = crate::runtime::Runtime::start(&config()).unwrap();
        let shard = rt.shard_ids()[0];
        let holder = rt.holder_of(shard).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let wrapped = here();
        rt.spawn_on(shard, async move {
            let slept = line!() + 1;
            let _ = crate::futures::sleep(NAP_NS).await;
            let deadline = line!() + 1;
            let _ = crate::futures::within(NAP_NS, std::future::pending::<()>()).await;
            let _ = crate::futures::sleep_from(NAP_NS, wrapped).await;
            let quiet = line!() + 1;
            let _ = crate::quiet::sleep_quiet(NAP_NS, NAP_NS).await;
            let counters = crate::registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
            let _ = tx.send((slept, deadline, quiet, counters));
        })
        .unwrap();
        let (slept, deadline, quiet, counters) = rx.recv_timeout(ANSWER).unwrap();
        let table = timer_wakes(holder);
        assert!(at_line(&table, slept) >= 1, "the sleep's: {table:?}");
        assert!(at_line(&table, deadline) >= 1, "the deadline's: {table:?}");
        assert!(table.iter().any(|w| w.site == Some(wrapped) && w.count >= 1), "the wrapper's caller's: {table:?}");
        assert!(at_line(&table, quiet) >= 1, "the quiet sleep's: {table:?}");
        assert_eq!(table.iter().map(|w| w.count).sum::<u64>(), counters.woken_by_timer, "every timer wake counted: {table:?}, {counters:?}");
        rt.shutdown().unwrap();
    }

    /// A future pending until `open` is set, handing its waker over each time it waits.
    struct Gate {
        open: std::sync::Arc<std::sync::atomic::AtomicBool>,
        wakers: std::sync::mpsc::Sender<std::task::Waker>,
    }

    impl std::future::Future for Gate {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
            if self.open.load(Ordering::SeqCst) {
                return std::task::Poll::Ready(());
            }
            let _ = self.wakers.send(cx.waker().clone());
            std::task::Poll::Pending
        }
    }

    /// Kiln's goal 9, plan 01a1a87b2cec step 5: a kick that finds a shard parked counts against where the task it wakes
    /// was spawned. Do: on a running runtime's idle shard, a task spawned at a known place waits on a gate; the test's
    /// thread opens it and wakes the task once the shard is announced parked. Expect: one kick at the task's spawn site
    /// (its claim: the parked shard's system call), and every kick the shard's senders claimed counted somewhere.
    #[test]
    fn g9_s5_a_kick_that_finds_a_shard_parked_counts_against_the_woken_tasks_spawn_site() {
        let rt = crate::runtime::Runtime::start(&config()).unwrap();
        let shard = rt.shard_ids()[0];
        let holder = rt.holder_of(shard).unwrap();
        let (wakers_tx, wakers) = std::sync::mpsc::channel();
        let (done_tx, done) = std::sync::mpsc::channel();
        let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = Gate { open: std::sync::Arc::clone(&open), wakers: wakers_tx };
        let spawned = line!() + 1;
        rt.spawn_on(shard, async move {
            gate.await;
            let _ = done_tx.send(());
        })
        .unwrap();
        let waker = wakers.recv_timeout(ANSWER).unwrap();
        while !crate::registry::with_entry(shard.0, |entry| entry.parking.parked()).unwrap_or(true) {
            std::hint::spin_loop();
        }
        open.store(true, Ordering::SeqCst);
        waker.wake();
        done.recv_timeout(ANSWER).unwrap();
        let table = kicked_tasks(holder);
        assert_eq!(at_line(&table, spawned), 1, "the woken task's: {table:?}");
        let claimed: u64 = crate::registry::kicks_from(holder).unwrap().iter().map(|(claimed, _)| claimed).sum();
        assert_eq!(table.iter().map(|w| w.count).sum::<u64>(), claimed, "every claimed kick counted: {table:?}");
        rt.shutdown().unwrap();
    }
}
