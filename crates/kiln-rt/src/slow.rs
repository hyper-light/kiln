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

/// One entry: a key set once, then the polls it held and for how long, and those of them the host held.
#[derive(Debug, Default)]
struct Entry {
    key: OnceLock<Key>,
    polls: AtomicU64,
    total_ns: AtomicU64,
    longest_ns: AtomicU64,
    host_polls: AtomicU64,
    host_ns: AtomicU64,
}

impl Entry {
    /// Counts one poll of `ns`, the host's when `host` (the writer's alone).
    fn count(&self, ns: u64, host: bool) {
        self.total_ns.fetch_add(ns, Ordering::Relaxed);
        self.longest_ns.fetch_max(ns, Ordering::Relaxed);
        if host {
            self.host_ns.fetch_add(ns, Ordering::Relaxed);
            self.host_polls.fetch_add(1, Ordering::Relaxed);
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
    pub(crate) fn note(
        &self,
        site: Option<&'static Location<'static>>,
        label: Option<&'static Label>,
        ns: u64,
        host: bool,
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
                entry.count(ns, host);
            }
            None => self.rest.count(ns, host),
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
}
