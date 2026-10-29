//! Live tail (`docs/observe.md` §6): a search's log records as a replica applies them, as Datadog's
//! Live Tail shows them: every one while they are few, a uniformly random sample once they come
//! faster than they can be read ("the Live Tail output is sampled when too many logs matching the
//! query are flowing in … The sampling applied is uniformly random", docs.datadoghq.com, "Live
//! Tail").
//!
//! The store hands the [`Feed`] each batch's rows as it applies them, while anyone follows: copied
//! once, into a ring every [`Follower`] reads at its own pace, a chunk kept until the slowest has
//! read it or the ring passes its bound (a follower that fell that far behind is told how many rows
//! it missed; it never holds a writer back). With no one following, applying costs one load. An
//! entry is published once at the most: a replica opened again applies again the entries after its
//! last cut, and none its log held as it opened is published (one it had applied before a crash
//! may have been, and a record shown twice reads as two).
//!
//! A follower's [`Sample`] is Vitter's Algorithm R ("Random sampling with a reservoir", ACM TOMS
//! 11(1), 1985): of the rows a tick matches, `max` kept uniformly at random in one pass, each with
//! probability `max / matched`, shown in the order they were applied.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::record::{Record, RecordRef, RowError, Rows};
use super::search::{RowScratch, Search};

/// Bytes of rows the ring holds at the most, read or not. A follower reads every few tens of
/// milliseconds; this holds a third of a second of rows at the most kiln applies on one core
/// (`docs/observe.md` §12.8: 106 MB/s of lines), so a follower that keeps up misses none.
pub const RING_BYTES: usize = 32 << 20;
/// Followers a feed takes at once. Each matches every row applied: at the most kiln applies on a
/// core, some 580,000 rows a second, a search costs 5–6% of a core (82–109 ns a row, §12.14), so
/// sixteen cost about one.
pub const MAX_FOLLOWERS: usize = 16;

/// The rows a replica applies, for whoever follows them.
#[derive(Debug)]
pub struct Feed {
    following: AtomicUsize,
    /// The newest entry the feed was told of (its index).
    newest: AtomicU64,
    capacity: usize,
    ring: Mutex<Ring>,
}

#[derive(Debug, Default)]
struct Ring {
    chunks: VecDeque<Chunk>,
    bytes: usize,
    /// The place of the next row published: rows published since the feed began.
    next: u64,
    /// Each follower's place, by its slot: the next row it reads.
    cursors: Vec<Option<u64>>,
}

#[derive(Debug, Clone)]
struct Chunk {
    first: u64,
    rows: u32,
    raw: Arc<[u8]>,
}

impl Chunk {
    fn end(&self) -> u64 {
        self.first + u64::from(self.rows)
    }
}

fn lock(m: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Ring {
    /// Chunks every follower has read go.
    fn trim(&mut self) {
        let Some(min) = self.cursors.iter().flatten().min().copied() else {
            self.chunks.clear();
            self.bytes = 0;
            return;
        };
        while self.chunks.front().is_some_and(|c| c.end() <= min) {
            if let Some(c) = self.chunks.pop_front() {
                self.bytes -= c.raw.len();
            }
        }
    }
}

impl Feed {
    pub fn new() -> Arc<Self> {
        Self::with_capacity(RING_BYTES)
    }

    /// A feed whose ring holds `bytes` at the most (one chunk, whatever its size, always fits).
    pub fn with_capacity(bytes: usize) -> Arc<Self> {
        Arc::new(Self { following: AtomicUsize::new(0), newest: AtomicU64::new(0), capacity: bytes, ring: Mutex::new(Ring::default()) })
    }

    /// Whether anyone follows.
    pub fn following(&self) -> bool {
        self.following.load(Ordering::Acquire) > 0
    }

    /// Entry `index` is being applied: whether it is new to the feed, its rows to be published.
    pub fn entry(&self, index: u64) -> bool {
        self.newest.fetch_max(index, Ordering::AcqRel) < index
    }

    /// Entries through `index` may have been published already (the log held them when its replica
    /// opened: those applied before a restart are applied again), and are not published again.
    pub fn seen(&self, index: u64) {
        self.newest.fetch_max(index, Ordering::AcqRel);
    }

    /// Another group's entries from here on: their indexes start again.
    pub fn reset(&self) {
        self.newest.store(0, Ordering::Release);
    }

    /// Rows applied (their encoding, `rows` of them), for whoever follows.
    pub fn publish(&self, raw: &[u8], rows: u32) {
        if rows == 0 {
            return;
        }
        let mut ring = lock(&self.ring);
        // The last follower went since the caller looked.
        if ring.cursors.iter().all(Option::is_none) {
            return;
        }
        let first = ring.next;
        ring.next += u64::from(rows);
        ring.bytes += raw.len();
        ring.chunks.push_back(Chunk { first, rows, raw: Arc::from(raw) });
        while ring.bytes > self.capacity && ring.chunks.len() > 1 {
            if let Some(c) = ring.chunks.pop_front() {
                ring.bytes -= c.raw.len();
            }
        }
    }

    /// Follows the rows published from now on; none while [`MAX_FOLLOWERS`] do.
    pub fn follow(self: &Arc<Self>) -> Option<Follower> {
        let mut ring = lock(&self.ring);
        let slot = match ring.cursors.iter().position(Option::is_none) {
            Some(s) => s,
            None if ring.cursors.len() < MAX_FOLLOWERS => {
                ring.cursors.push(None);
                ring.cursors.len() - 1
            }
            None => return None,
        };
        let next = ring.next;
        if let Some(c) = ring.cursors.get_mut(slot) {
            *c = Some(next);
        }
        self.following.fetch_add(1, Ordering::AcqRel);
        Some(Follower { feed: self.clone(), slot, chunks: Vec::new() })
    }

    /// Bytes the ring holds now.
    pub fn held_bytes(&self) -> usize {
        lock(&self.ring).bytes
    }
}

/// One who follows a feed. Dropped, it follows no more.
#[derive(Debug)]
pub struct Follower {
    feed: Arc<Feed>,
    slot: usize,
    /// What a read takes from the ring, kept across reads.
    chunks: Vec<Chunk>,
}

impl Follower {
    /// Whether rows were published since the last read.
    pub fn pending(&self) -> bool {
        let ring = lock(&self.feed.ring);
        ring.cursors.get(self.slot).copied().flatten().is_some_and(|at| at < ring.next)
    }

    /// Reads the rows published since the last read into `sample`: each counted, those `search`
    /// matches offered to it; rows the ring let go unread counted as missed.
    pub fn read(&mut self, search: &Search, sample: &mut Sample) -> Result<(), RowError> {
        self.chunks.clear();
        {
            let mut ring = lock(&self.feed.ring);
            let next = ring.next;
            let at = ring.cursors.get(self.slot).copied().flatten().unwrap_or(next);
            let front = ring.chunks.front().map_or(next, |c| c.first);
            sample.missed += front.saturating_sub(at);
            self.chunks.extend(ring.chunks.iter().filter(|c| c.first >= at).cloned());
            if let Some(c) = ring.cursors.get_mut(self.slot) {
                *c = Some(next);
            }
            ring.trim();
        }
        let mut read = Ok(());
        for c in &self.chunks {
            let mut rows = Rows::new(&c.raw, c.rows as usize);
            let mut place = c.first;
            loop {
                match rows.next_row() {
                    Ok(Some(r)) => {
                        sample.read += 1;
                        if search.row(&r, &mut sample.scratch) {
                            sample.offer(place, &r);
                        }
                        place += 1;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        read = Err(e);
                        break;
                    }
                }
            }
        }
        // The ring's bytes are let go of now, not at the next read.
        self.chunks.clear();
        read
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let mut ring = lock(&self.feed.ring);
        if let Some(c) = ring.cursors.get_mut(self.slot) {
            *c = None;
        }
        self.feed.following.fetch_sub(1, Ordering::AcqRel);
        ring.trim();
    }
}

/// A tick's rows as a follower shows them: how many it read and matched, and `max` of the matched
/// at the most, uniformly at random. Its records' buffers are kept from tick to tick.
#[derive(Debug)]
pub struct Sample {
    max: usize,
    /// The reservoir: each record's place (the order rows were applied in) and the record; those
    /// past `len` are buffers kept for the next tick.
    slots: Vec<(u64, Record)>,
    len: usize,
    /// Rows this tick read, matched, and missed (the ring let them go before they were read).
    pub read: u64,
    pub matched: u64,
    pub missed: u64,
    rng: u64,
    scratch: RowScratch,
    /// Attribute pairs the records let go of, for those that need more.
    spare: Vec<(String, String)>,
}

impl Sample {
    /// Keeps `max` records a tick at the most (none: only counts); `seed` seeds its choices.
    pub fn new(max: usize, seed: u64) -> Self {
        Self { max, slots: Vec::new(), len: 0, read: 0, matched: 0, missed: 0, rng: seed, scratch: RowScratch::default(), spare: Vec::new() }
    }

    fn offer(&mut self, place: u64, r: &RecordRef<'_>) {
        self.matched += 1;
        let slot = if self.len < self.max {
            self.len += 1;
            self.len - 1
        } else {
            // Algorithm R: the n-th of n matched takes a slot with probability max / n.
            let j = below(&mut self.rng, self.matched);
            match usize::try_from(j) {
                Ok(j) if j < self.max => j,
                _ => return,
            }
        };
        match self.slots.get_mut(slot) {
            Some((p, rec)) => {
                *p = place;
                rec.set_from(r, &mut self.spare);
            }
            None => self.slots.push((place, r.to_owned())),
        }
    }

    /// The tick's records, in the order they were applied, each with its place.
    pub fn kept(&mut self) -> &[(u64, Record)] {
        let kept = self.slots.get_mut(..self.len).unwrap_or_default();
        kept.sort_unstable_by_key(|(p, _)| *p);
        kept
    }

    /// The tick's records, in the order they were applied.
    pub fn records(&mut self) -> impl Iterator<Item = &Record> {
        self.kept().iter().map(|(_, r)| r)
    }

    /// The next tick starts: nothing read, matched or kept; `max` its bound. The records' buffers
    /// stay, for ticks to come (a tick's bound moves with what the ticks before it showed).
    pub fn clear(&mut self, max: usize) {
        self.max = max;
        self.len = 0;
        self.read = 0;
        self.matched = 0;
        self.missed = 0;
    }
}

/// SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number generators", OOPSLA
/// 2014), a number in `[0, n)` by Lemire's multiply-shift ("Fast random integer generation in an
/// interval", ACM TOMACS 29(1), 2019; its bias, at most n / 2^64, is nothing here).
fn below(state: &mut u64, n: u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^= z >> 31;
    ((u128::from(z) * u128::from(n)) >> 64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::put_row;
    use crate::logs::search::parse;

    fn rec(i: u64, service: &str) -> Record {
        Record { ts: i64::try_from(i).unwrap() * 1_000, severity: 9, service: service.into(), host: "h".into(), source: "s".into(), message: format!("line {i}"), ..Record::default() }
    }

    fn chunk(records: &[Record]) -> (Vec<u8>, u32) {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in records {
            put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        (raw, u32::try_from(records.len()).unwrap())
    }

    fn publish(feed: &Feed, records: &[Record]) {
        let (raw, n) = chunk(records);
        feed.publish(&raw, n);
    }

    fn messages(s: &mut Sample) -> Vec<String> {
        s.records().map(|r| r.message.clone()).collect()
    }

    #[test]
    fn a_follower_reads_what_is_published_after_it_follows_in_order_and_once() {
        let feed = Feed::new();
        publish(&feed, &[rec(0, "a")]);
        let mut f = feed.follow().unwrap();
        let all = parse("*").unwrap();
        let mut s = Sample::new(100, 1);
        publish(&feed, &[rec(1, "a"), rec(2, "b")]);
        publish(&feed, &[rec(3, "a")]);
        f.read(&all, &mut s).unwrap();
        assert_eq!(messages(&mut s), ["line 1", "line 2", "line 3"]);
        assert_eq!((s.read, s.matched, s.missed), (3, 3, 0));
        s.clear(100);
        f.read(&all, &mut s).unwrap();
        assert_eq!((s.read, s.matched), (0, 0));
        // Read by every follower, the ring holds nothing.
        assert_eq!(feed.held_bytes(), 0);
    }

    #[test]
    fn nothing_is_kept_while_no_one_follows_and_the_last_to_go_lets_the_ring_go() {
        let feed = Feed::new();
        assert!(!feed.following());
        publish(&feed, &[rec(0, "a")]);
        assert_eq!(feed.held_bytes(), 0);
        let f = feed.follow().unwrap();
        let g = feed.follow().unwrap();
        publish(&feed, &[rec(1, "a")]);
        assert!(feed.held_bytes() > 0);
        drop(f);
        assert!(feed.held_bytes() > 0, "the other has not read it");
        drop(g);
        assert_eq!(feed.held_bytes(), 0);
        assert!(!feed.following());
    }

    #[test]
    fn a_search_narrows_what_is_offered_and_every_row_read_is_counted() {
        let feed = Feed::new();
        let mut f = feed.follow().unwrap();
        publish(&feed, &(0..10).map(|i| rec(i, if i % 3 == 0 { "api" } else { "web" })).collect::<Vec<_>>());
        let mut s = Sample::new(100, 1);
        f.read(&parse("service:api").unwrap(), &mut s).unwrap();
        assert_eq!(messages(&mut s), ["line 0", "line 3", "line 6", "line 9"]);
        assert_eq!((s.read, s.matched), (10, 4));
        // A tick that may show none still counts.
        s.clear(0);
        publish(&feed, &(0..10).map(|i| rec(i, "api")).collect::<Vec<_>>());
        f.read(&parse("service:api").unwrap(), &mut s).unwrap();
        assert!(messages(&mut s).is_empty());
        assert_eq!((s.read, s.matched), (10, 10));
    }

    #[test]
    fn a_follower_too_far_behind_is_told_what_it_missed_and_never_holds_the_ring_past_its_bound() {
        let one = chunk(&[rec(0, "a")]).0.len();
        let feed = Feed::with_capacity(one * 3);
        let mut slow = feed.follow().unwrap();
        for i in 0..10 {
            publish(&feed, &[rec(i, "a")]);
            assert!(feed.held_bytes() <= one * 3 + 8);
        }
        let mut s = Sample::new(100, 1);
        slow.read(&parse("*").unwrap(), &mut s).unwrap();
        assert_eq!(s.missed + s.read, 10);
        assert!(s.missed >= 6, "{}", s.missed);
        assert_eq!(messages(&mut s).last().map(String::as_str), Some("line 9"));
    }

    #[test]
    fn an_entry_applied_again_is_published_once_and_another_group_starts_again() {
        let feed = Feed::new();
        assert!(feed.entry(5));
        assert!(!feed.entry(5));
        assert!(!feed.entry(3));
        assert!(feed.entry(6));
        feed.seen(9);
        assert!(!feed.entry(8));
        assert!(feed.entry(10));
        feed.reset();
        assert!(feed.entry(1));
    }

    #[test]
    fn at_most_so_many_follow_at_once_and_one_who_goes_makes_room() {
        let feed = Feed::new();
        let mut all: Vec<Follower> = (0..MAX_FOLLOWERS).map(|_| feed.follow().unwrap()).collect();
        assert!(feed.follow().is_none());
        all.pop();
        assert!(feed.follow().is_some());
    }

    /// Each matched row is kept with probability max / matched whatever its place: over many ticks,
    /// every place is kept about as often (χ² against the uniform), and a service's share of what
    /// is kept is its share of what came.
    #[test]
    fn the_sample_is_uniform_over_the_rows_matched_and_keeps_a_stream_s_mix() {
        let (n, max, ticks) = (200_u64, 10_usize, 4_000);
        let records: Vec<Record> = (0..n).map(|i| rec(i, if i % 10 == 0 { "rare" } else { "common" })).collect();
        let (raw, rows) = chunk(&records);
        let feed = Feed::new();
        let mut f = feed.follow().unwrap();
        let all = parse("*").unwrap();
        let mut s = Sample::new(max, 7);
        let mut kept = vec![0_u64; usize::try_from(n).unwrap()];
        let mut rare = 0_u64;
        for _ in 0..ticks {
            feed.publish(&raw, rows);
            f.read(&all, &mut s).unwrap();
            assert_eq!(s.matched, n);
            let mut count = 0;
            for r in s.records() {
                let i: usize = r.message.trim_start_matches("line ").parse().unwrap();
                kept[i] += 1;
                rare += u64::from(r.service == "rare");
                count += 1;
            }
            assert_eq!(count, max);
            s.clear(max);
        }
        let expected = (ticks * max) as f64 / n as f64;
        let chi2: f64 = kept.iter().map(|&k| (k as f64 - expected).powi(2) / expected).sum();
        // 199 degrees of freedom: the 0.999 quantile is 267.5.
        assert!(chi2 < 267.5, "χ² {chi2:.1}");
        let share = rare as f64 / (ticks * max) as f64;
        assert!((share - 0.1).abs() < 0.01, "{share}");
    }

    /// The records a tick keeps reuse the buffers of those the last tick kept.
    #[test]
    fn a_tick_s_records_are_shown_in_the_order_applied_their_buffers_reused() {
        let feed = Feed::new();
        let mut f = feed.follow().unwrap();
        let all = parse("*").unwrap();
        let mut s = Sample::new(4, 3);
        for t in 0..3 {
            publish(&feed, &(0..50).map(|i| rec(t * 100 + i, "a")).collect::<Vec<_>>());
            f.read(&all, &mut s).unwrap();
            let places: Vec<i64> = s.records().map(|r| r.ts).collect();
            assert_eq!(places.len(), 4);
            assert!(places.windows(2).all(|w| w[0] < w[1]), "{places:?}");
            s.clear(4);
        }
        assert_eq!(s.slots.len(), 4);
    }
}
