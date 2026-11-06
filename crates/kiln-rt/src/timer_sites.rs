//! What woke a parked shard by a timer, by where the timer was armed (kiln's goal 9, plan 01a1a87b2cec step 5: the
//! shards' timer wakes, ~457 a second in kiln's lab, g9p16, 234 of them its delay probe's; the rest is what to find).
//! Every sleep takes the place it was asked for at (`#[track_caller]`: [`crate::futures::sleep`],
//! [`crate::futures::within`], and [`crate::futures::sleep_from`] for a wrapper that passes its own caller's on), and
//! the timer it arms keeps it. A park the wheel's deadline ended is counted against the site of the first timer that
//! fell due in the step after it, or with no site where none had (a wait that returned before its deadline); a
//! quiet sleep's own wake ([`crate::quiet`]) against the quiet sleep's site. Their sum is the shard's
//! `woken_by_timer`.
//!
//! **One writer, any reader, no lock**, as [`crate::slow`]'s table: each shard keeps one on its registry entry, which
//! only the shard writes, once a timer wake; an entry's site is set once and its count is a relaxed atomic. A table
//! that is full counts what doesn't fit with no site.

use std::panic::Location;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::{self, SlotHolder};

/// Format: entries in a shard's table, as many as [`crate::slow`]'s: kiln's server arms its timers from a few dozen
/// places (its sleeps, deadlines and intervals), so a site past this many is counted with no site rather than evicting
/// one.
pub const ENTRIES: usize = crate::slow::ENTRIES;

/// One entry: a site set once, and the wakes it made.
#[derive(Debug, Default)]
struct Entry {
    site: OnceLock<&'static Location<'static>>,
    wakes: AtomicU64,
}

/// A shard's table of timer wakes by site (see the module's notes).
#[derive(Debug)]
pub(crate) struct TimerSites {
    entries: Box<[Entry]>,
    /// The wakes of a site a full table had no entry for, and those of no site.
    rest: AtomicU64,
}

impl Default for TimerSites {
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

impl TimerSites {
    /// One timer wake of the shard, by the timer armed at `site` (none: no timer fell due, or none was kept): the
    /// shard's own call.
    pub(crate) fn note(&self, site: Option<&'static Location<'static>>) {
        let Some(site) = site else {
            self.rest.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let n = self.entries.len();
        let first = start(site, n);
        let found = (0..n)
            .filter_map(|i| self.entries.get(first.wrapping_add(i).checked_rem(n)?))
            .find(|entry| entry.site.get().is_none_or(|s| *s == site));
        match found {
            // The shard is the table's only writer: a free entry it finds is still free as it sets the site.
            Some(entry) => {
                let _ = entry.site.set(site);
                entry.wakes.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.rest.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Every site with a wake counted, then the rest (no site), as they stand: any thread's.
    pub(crate) fn read(&self) -> Vec<TimerWakes> {
        let rest = self.rest.load(Ordering::Relaxed);
        self.entries
            .iter()
            .filter_map(|entry| {
                let wakes = entry.wakes.load(Ordering::Relaxed);
                (wakes > 0).then(|| TimerWakes { site: entry.site.get().copied(), wakes })
            })
            .chain((rest > 0).then_some(TimerWakes { site: None, wakes: rest }))
            .collect()
    }
}

/// The timer wakes of one site of a shard, since the shard started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerWakes {
    /// Where the timer was armed; none for the wakes no timer fell due for, and those a full table had no entry for.
    pub site: Option<&'static Location<'static>>,
    /// Parks it ended.
    pub wakes: u64,
}

/// The timer wakes of the shard `holder` names, by site, as they stand (any thread; nothing of the shard's own is
/// touched). Empty for a holder no longer registered.
pub fn timer_wakes(holder: SlotHolder) -> Vec<TimerWakes> {
    registry::with_holder(holder, |entry| entry.timer_sites.read()).unwrap_or_default()
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    impl TimerSites {
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
        let table = TimerSites::default();
        for site in [Some(a), Some(b), Some(a), None] {
            table.note(site);
        }
        let mut read = table.read();
        read.sort_by_key(|w| std::cmp::Reverse(w.wakes));
        assert_eq!(read, vec![TimerWakes { site: Some(a), wakes: 2 }, TimerWakes { site: Some(b), wakes: 1 }, TimerWakes { site: None, wakes: 1 }]);
        let full = TimerSites::with_entries(2);
        for site in [a, b, c, c] {
            full.note(Some(site));
        }
        let read = full.read();
        assert_eq!(read.iter().map(|w| w.wakes).sum::<u64>(), 4, "{read:?}");
        assert_eq!(read.iter().filter(|w| w.site.is_none()).map(|w| w.wakes).sum::<u64>(), 2, "{read:?}");
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
    fn at_line(table: &[TimerWakes], line: u32) -> u64 {
        table.iter().filter(|w| w.site.is_some_and(|s| s.file() == file!() && s.line() == line)).map(|w| w.wakes).sum()
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
        assert!(table.iter().any(|w| w.site == Some(wrapped) && w.wakes >= 1), "the wrapper's caller's: {table:?}");
        assert!(at_line(&table, quiet) >= 1, "the quiet sleep's: {table:?}");
        assert_eq!(table.iter().map(|w| w.wakes).sum::<u64>(), counters.woken_by_timer, "every timer wake counted: {table:?}, {counters:?}");
        rt.shutdown().unwrap();
    }
}
