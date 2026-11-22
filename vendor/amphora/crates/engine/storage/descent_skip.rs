//! Amphora: skipping the B-tree descent, two prototypes (G3 step 5, prototypes D and E; G3.5).
//!
//! A point read descends from the root: a binary search at every interior page, then one at the
//! leaf. Both prototypes name the leaf instead, and the caller searches only that leaf:
//! - [`AdaptiveHash`] (prototype D) is InnoDB's adaptive hash index (`btr0sea.cc`: a hash from a
//!   key to the leaf record, built for a page only once searches on it show it pays; Heikki Tuuri's
//!   design, MySQL 8.0 reference manual §15.5.3): here a hash from a key to its leaf page, built
//!   for one leaf at a time.
//! - [`LeafLocator`] (prototype E) is a learned, error-bounded index (Kraska et al., "The Case for
//!   Learned Index Structures", SIGMOD 2018: RMI; Ferragina and Vinciguerra, "The PGM-index",
//!   PVLDB 13(8) 2020: piecewise linear segments with an error bound) from a rowid to its leaf,
//!   over the fences (each leaf's first rowid) of a rowid table.
//!
//! Neither is trusted. A prototype names a page; the caller reads it and accepts the answer only
//! when the page proves it: the page is a leaf of the tree's kind, its first key is at most the
//! key and its last at least ([`LeafSpan::covers`]), and no page has left any tree since the
//! prototype learned the page (the caller's `stamp`, a count it raises whenever a page is freed or
//! changes tree, so a leaf of another table holding the same rowid can't answer). Leaves of one
//! B-tree hold disjoint ranges, so a leaf of this tree whose range holds the key is the only place
//! the key can be: the row it holds, or its absence, is what a descent returns (T-G3.5.1). A page
//! that doesn't prove it is refuted and forgotten, and the caller descends as today. Upkeep is
//! therefore paid where the tree changed and a search noticed: a refuted page is dropped, and the
//! descent that follows teaches the leaf it found.
//!
//! When D builds a leaf's hash: the ski-rental problem, as key heads decide (decision 38; Karlin,
//! Manasse, Rudolph, Sleator, Algorithmica 3, 1988: the deterministic break-even rule never pays
//! more than twice what knowing the future would). Each descent that ends at a leaf without a hash
//! is charged the probes it made above the leaf (what a hit would have saved: the caller counts
//! them as the descent makes them); the hash is built when the charges reach the leaf's cell count
//! (a build hashes each cell's key once). Rent and price are counted in key operations, measured
//! as the searches run, with no timed constant; that a hash insert costs what a probe does is the
//! rule's one assumption, and `descent_skip_bench` measures both.
//!
//! E's error bound is its own fit's: each segment is the line through its first and last fence, and
//! its bound is the largest distance between where the line puts a fence and where the fence is.
//! How many segments: a lookup pays a binary search over the segments and one over its window of
//! `2 * err + 3` fences. The worst segment is split at its worst fence until every segment is
//! exact, and the number of splits whose expected probes over every fence
//! (`bits(segments) * fences + sum(len * bits(window))`, all integers) were fewest is kept. Learning a leaf refits only the segments its fences touched (incremental);
//! [`LeafLocator::rebuild`] fits a whole table.
//!
//! Their memory, bounded. D holds at most `budget` entries (keys hashed plus leaves tracked); a
//! build that doesn't fit evicts the least recently used leaves, and a leaf larger than the whole
//! budget is refused ([`Refusal::Budget`]). E holds at most `budget` fences and refuses a leaf past
//! it. The caller derives both budgets from the memory it grants (its page cache's).
//!
//! The cursor hook ([`Hook`], held by each connection's pager), behind the switch
//! `AMPHORA_DESCENT_SKIP` = `off` | `hash` | `locator` (default `off`): a read-only cursor's point
//! seek asks the prototype for a leaf, takes it only if the page is clean in the connection's own
//! cache and proves the key ([`table_leaf_span`]), and then stands on that leaf alone; every other
//! move descends again first (`BTreeCursor::leave_stood_leaf`). The stamp is the connection's own
//! count of pages that left their tree (freed, or restored by a rollback) together with its
//! snapshot's identity (its WAL position), so a page learned under one snapshot is never trusted
//! under another; everything learned is dropped when the stamp moves.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::Hash;

use crate::storage::sqlite3_ondisk::PageContent;

/// A leaf as a read of it leaves it: its page, its first and last key, and its cell count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafSpan<K> {
    pub page: u64,
    pub first: K,
    pub last: K,
    pub cells: usize,
}

impl<K: Ord> LeafSpan<K> {
    /// Whether this leaf alone can hold `key`: B-tree leaves hold disjoint ranges, so a leaf of the
    /// tree whose first key is at most `key` and whose last is at least is the only place it can be.
    pub fn covers(&self, key: &K) -> bool {
        self.first <= *key && *key <= self.last
    }
}

/// Why a prototype didn't take a leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It needs more entries than the whole budget.
    Budget,
}

/// What a descent's report did to [`AdaptiveHash`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hashed {
    /// The leaf's charges are below its price: no hash yet.
    Renting,
    /// The leaf's hash was built now.
    Built,
    /// The leaf had a hash: the key was added to it.
    Learned,
    /// The key isn't in the leaf's range: nothing to learn.
    Uncovered,
    Refused(Refusal),
}

/// What a prototype counted: its non-vacuity and its upkeep, read by the tests and the bench.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Pages named that proved the answer.
    pub hits: u64,
    /// Pages named that didn't.
    pub refuted: u64,
    /// D: hashes built; E: fences learned.
    pub built: u64,
    /// D: keys hashed; E: fences fitted again.
    pub upkeep: u64,
    /// D: leaves evicted for room.
    pub evicted: u64,
    /// Times everything was forgotten because a page left its tree.
    pub stamp_drops: u64,
}

#[derive(Debug)]
struct HashLeaf<K> {
    /// Probes charged to this leaf by descents that ended at it without a hash.
    rent: u64,
    /// The keys hashed to it; empty without a hash.
    keys: Vec<K>,
    built: bool,
    /// When it was last used, on the hash's own clock (one tick a call).
    used: u64,
}

/// Prototype D: InnoDB's adaptive hash index, a key to its leaf, built leaf by leaf by a ski rental.
#[derive(Debug)]
pub struct AdaptiveHash<K> {
    entries: HashMap<K, u64>,
    leaves: HashMap<u64, HashLeaf<K>>,
    stamp: u64,
    budget: usize,
    clock: u64,
    counts: Counts,
}

impl<K: Hash + Eq + Ord + Clone> AdaptiveHash<K> {
    /// A hash holding at most `budget` entries (keys hashed plus leaves tracked).
    pub fn new(budget: usize, stamp: u64) -> Self {
        Self {
            entries: HashMap::new(),
            leaves: HashMap::new(),
            stamp,
            budget,
            clock: 0,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// Entries held: keys hashed plus leaves tracked.
    pub fn held(&self) -> usize {
        self.entries.len().saturating_add(self.leaves.len())
    }

    /// A page left its tree since the hash learned its pages: none of them is proof any more. The
    /// rent earned stays (a page id's heat is still the best guess), the hashes go.
    fn sync(&mut self, stamp: u64) {
        self.clock = self.clock.saturating_add(1);
        if stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        self.entries.clear();
        for leaf in self.leaves.values_mut() {
            leaf.keys.clear();
            leaf.built = false;
        }
        self.counts.stamp_drops = self.counts.stamp_drops.saturating_add(1);
    }

    /// The leaf the hash names for `key`, for the caller to prove; `None`: descend.
    pub fn lookup(&mut self, key: &K, stamp: u64) -> Option<u64> {
        self.sync(stamp);
        let page = *self.entries.get(key)?;
        if let Some(leaf) = self.leaves.get_mut(&page) {
            leaf.used = self.clock;
        }
        Some(page)
    }

    /// The page named proved the answer.
    pub fn confirm(&mut self) {
        self.counts.hits = self.counts.hits.saturating_add(1);
    }

    /// The page named didn't prove the answer: its hash is stale, dropped whole.
    pub fn refute(&mut self, page: u64) {
        self.counts.refuted = self.counts.refuted.saturating_add(1);
        self.drop_leaf(page);
    }

    fn drop_leaf(&mut self, page: u64) {
        if let Some(leaf) = self.leaves.remove(&page) {
            for key in &leaf.keys {
                if self.entries.get(key) == Some(&page) {
                    self.entries.remove(key);
                }
            }
        }
    }

    /// Evicts the least recently used leaves other than `keep` until `need` more entries fit.
    fn make_room(&mut self, need: usize, keep: u64) -> bool {
        while self.held().saturating_add(need) > self.budget {
            let coldest = self
                .leaves
                .iter()
                .filter(|(page, _)| **page != keep)
                .min_by_key(|(_, leaf)| leaf.used)
                .map(|(page, _)| *page);
            match coldest {
                Some(page) => {
                    self.drop_leaf(page);
                    self.counts.evicted = self.counts.evicted.saturating_add(1);
                }
                None => return false,
            }
        }
        true
    }

    /// A descent for `key` ended at `leaf` after `above` probes over interior pages. Charges the
    /// leaf the probes a hit would have saved, builds its hash once they reach its cell count
    /// (`keys` reads the leaf's keys), and adds `key` to a hash it already has.
    pub fn descended(
        &mut self,
        key: &K,
        leaf: &LeafSpan<K>,
        above: u64,
        stamp: u64,
        keys: impl FnOnce() -> Vec<K>,
    ) -> Hashed {
        self.sync(stamp);
        let clock = self.clock;
        if !self.leaves.contains_key(&leaf.page) && !self.make_room(1, leaf.page) {
            return Hashed::Refused(Refusal::Budget);
        }
        let state = self.leaves.entry(leaf.page).or_insert(HashLeaf {
            rent: 0,
            keys: Vec::new(),
            built: false,
            used: clock,
        });
        state.used = clock;
        if state.built {
            if !leaf.covers(key) {
                return Hashed::Uncovered;
            }
            if self.entries.contains_key(key) {
                return Hashed::Learned;
            }
            if !self.make_room(1, leaf.page) {
                return Hashed::Refused(Refusal::Budget);
            }
            self.entries.insert(key.clone(), leaf.page);
            if let Some(state) = self.leaves.get_mut(&leaf.page) {
                state.keys.push(key.clone());
            }
            return Hashed::Learned;
        }
        state.rent = state.rent.saturating_add(above);
        let price = u64::try_from(leaf.cells).unwrap_or(u64::MAX);
        if state.rent < price {
            return Hashed::Renting;
        }
        let keys = keys();
        // The leaf's own tracking entry is already held.
        if keys.len() >= self.budget || !self.make_room(keys.len(), leaf.page) {
            if let Some(state) = self.leaves.get_mut(&leaf.page) {
                state.rent = 0;
            }
            return Hashed::Refused(Refusal::Budget);
        }
        for key in &keys {
            if let Some(old) = self.entries.insert(key.clone(), leaf.page) {
                // A key another leaf claimed: that leaf's hash is stale.
                if old != leaf.page {
                    if let Some(other) = self.leaves.get_mut(&old) {
                        other.keys.retain(|held| held != key);
                    }
                }
            }
        }
        self.counts.built = self.counts.built.saturating_add(1);
        let hashed = u64::try_from(keys.len()).unwrap_or(u64::MAX);
        self.counts.upkeep = self.counts.upkeep.saturating_add(hashed);
        if let Some(state) = self.leaves.get_mut(&leaf.page) {
            state.keys = keys;
            state.built = true;
        }
        Hashed::Built
    }
}

/// What [`LeafLocator::learn`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Learned {
    /// The fence was already known.
    Unchanged,
    /// The fence was added, or moved, and the segments it touched refitted.
    Kept,
    Refused(Refusal),
}

/// A leaf's fence: its first and last rowid when last read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fence {
    first: i64,
    last: i64,
    page: u64,
}

/// A run of fences `start..start + len` predicted by the line through its first and last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Segment {
    start: usize,
    len: usize,
    /// The largest distance between a fence's predicted and actual index: the fit's own bound.
    err: usize,
}

/// The probes a binary search over `n` items makes at most: the bit length of `n`.
pub(crate) fn bits(n: usize) -> u64 {
    u64::from(usize::BITS.saturating_sub(n.leading_zeros()))
}

/// Prototype E: a learned, error-bounded locator from a rowid to its leaf, for one rowid table.
#[derive(Debug)]
pub struct LeafLocator {
    /// Sorted by rowid; their ranges are disjoint.
    fences: Vec<Fence>,
    /// Cover `0..fences.len()` in order, without gaps.
    segments: Vec<Segment>,
    /// Each fenced page's first rowid, to find its fence when it moves.
    by_page: HashMap<u64, i64>,
    stamp: u64,
    budget: usize,
    counts: Counts,
}

impl LeafLocator {
    /// A locator holding at most `budget` fences.
    pub fn new(budget: usize, stamp: u64) -> Self {
        Self {
            fences: Vec::new(),
            segments: Vec::new(),
            by_page: HashMap::new(),
            stamp,
            budget,
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    pub fn fences(&self) -> usize {
        self.fences.len()
    }

    pub fn segments(&self) -> usize {
        self.segments.len()
    }

    /// The largest error bound of any segment.
    pub fn max_err(&self) -> usize {
        self.segments.iter().map(|s| s.err).max().unwrap_or(0)
    }

    fn sync(&mut self, stamp: u64) {
        if stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        self.fences.clear();
        self.segments.clear();
        self.by_page.clear();
        self.counts.stamp_drops = self.counts.stamp_drops.saturating_add(1);
    }

    fn first_at(&self, index: usize) -> Option<i64> {
        self.fences.get(index).map(|f| f.first)
    }

    /// Where the segment's line puts `rowid`, clamped to the segment.
    fn predict(&self, segment: &Segment, rowid: i64) -> Option<usize> {
        let end = segment.start.checked_add(segment.len)?.checked_sub(1)?;
        let k0 = self.first_at(segment.start)?;
        let k1 = self.first_at(end)?;
        if rowid <= k0 || k1 <= k0 {
            return Some(segment.start);
        }
        if rowid >= k1 {
            return Some(end);
        }
        let offset = i128::from(rowid)
            .checked_sub(i128::from(k0))?
            .checked_mul(i128::try_from(end.checked_sub(segment.start)?).ok()?)?
            .checked_div(i128::from(k1).checked_sub(i128::from(k0))?)?;
        segment.start.checked_add(usize::try_from(offset).ok()?)
    }

    /// The fit's own error bound for the segment.
    fn fit(&self, start: usize, len: usize) -> Option<Segment> {
        let mut segment = Segment { start, len, err: 0 };
        let mut err = 0usize;
        for index in start..start.checked_add(len)? {
            let first = self.first_at(index)?;
            let predicted = self.predict(&segment, first)?;
            err = err.max(predicted.abs_diff(index));
        }
        segment.err = err;
        Some(segment)
    }

    /// The fence whose distance from the segment's line is largest, inside the segment.
    fn worst(&self, segment: &Segment) -> Option<usize> {
        let mut worst = None;
        let mut most = 0usize;
        for index in segment.start.checked_add(1)?..segment.start.checked_add(segment.len)? {
            let predicted = self.predict(segment, self.first_at(index)?)?;
            let off = predicted.abs_diff(index);
            if off > most {
                most = off;
                worst = Some(index);
            }
        }
        worst
    }

    /// The probes a lookup in a segment's window makes, weighted by the fences it serves.
    fn window_cost(segment: &Segment) -> u64 {
        let window = segment.err.saturating_mul(2).saturating_add(3);
        u64::try_from(segment.len)
            .unwrap_or(u64::MAX)
            .saturating_mul(bits(window))
    }

    /// Fits fences `start..start + len` as segments (`others` segments lie outside the run): the
    /// worst segment split at its worst fence, again and again until every segment is exact, and
    /// the split count whose expected probes over all fences were fewest kept. One split at a time
    /// can't see past a step that costs a bit (a binary search over one more segment) and saves
    /// one, which many together pay for.
    fn fit_run(&self, start: usize, len: usize, others: usize) -> Option<Vec<Segment>> {
        let (best, _) = self.split_run(start, len, others, None)?;
        let (_, run) = self.split_run(start, len, others, Some(best))?;
        Some(run)
    }

    /// Splits the run's worst segment at its worst fence `stop` times, or until every segment is
    /// exact; returns the split count with the fewest expected probes and the segments it ended with.
    fn split_run(
        &self,
        start: usize,
        len: usize,
        others: usize,
        stop: Option<usize>,
    ) -> Option<(usize, Vec<Segment>)> {
        if len == 0 {
            return Some((0, Vec::new()));
        }
        let total = u64::try_from(self.fences.len()).unwrap_or(u64::MAX);
        let probes = |count: usize, windows: u64| {
            bits(others.saturating_add(count))
                .saturating_mul(total)
                .saturating_add(windows)
        };
        let first = self.fit(start, len)?;
        let mut windows = Self::window_cost(&first);
        let mut exact = Vec::new();
        let mut open = BinaryHeap::new();
        if first.err > 0 {
            open.push((windows, Reverse(first.start), first.len, first.err));
        } else {
            exact.push(first);
        }
        let mut count = 1usize;
        let mut best = (probes(count, windows), 0usize);
        // Each split adds a segment of at least one fence: at most `len` of them.
        for steps in 1..=len {
            if stop.is_some_and(|stop| steps > stop) {
                break;
            }
            let Some((cost, Reverse(at), seg_len, err)) = open.pop() else {
                break;
            };
            let segment = Segment {
                start: at,
                len: seg_len,
                err,
            };
            let Some(cut) = self.worst(&segment) else {
                exact.push(segment);
                continue;
            };
            let left_len = cut.checked_sub(at)?;
            let left = self.fit(at, left_len)?;
            let right = self.fit(cut, seg_len.checked_sub(left_len)?)?;
            windows = windows
                .saturating_sub(cost)
                .saturating_add(Self::window_cost(&left))
                .saturating_add(Self::window_cost(&right));
            count = count.saturating_add(1);
            for part in [left, right] {
                if part.err > 0 {
                    open.push((
                        Self::window_cost(&part),
                        Reverse(part.start),
                        part.len,
                        part.err,
                    ));
                } else {
                    exact.push(part);
                }
            }
            let now = probes(count, windows);
            if now < best.0 {
                best = (now, steps);
            }
        }
        exact.extend(
            open.into_iter()
                .map(|(_, Reverse(start), len, err)| Segment { start, len, err }),
        );
        exact.sort_unstable_by_key(|s| s.start);
        Some((best.1, exact))
    }

    /// Fits every fence again: a whole table's leaves, in any order.
    pub fn rebuild(
        &mut self,
        leaves: impl IntoIterator<Item = LeafSpan<i64>>,
        stamp: u64,
    ) -> Learned {
        self.sync(stamp);
        let mut fences: Vec<Fence> = leaves
            .into_iter()
            .map(|leaf| Fence {
                first: leaf.first,
                last: leaf.last,
                page: leaf.page,
            })
            .collect();
        if fences.len() > self.budget {
            return Learned::Refused(Refusal::Budget);
        }
        fences.sort_unstable_by_key(|f| f.first);
        self.by_page = fences.iter().map(|f| (f.page, f.first)).collect();
        self.fences = fences;
        self.segments = self.fit_run(0, self.fences.len(), 0).unwrap_or_default();
        let fitted = u64::try_from(self.fences.len()).unwrap_or(u64::MAX);
        self.counts.upkeep = self.counts.upkeep.saturating_add(fitted);
        Learned::Kept
    }

    /// The leaf the locator names for `rowid`, for the caller to prove; `None`: descend.
    pub fn lookup(&mut self, rowid: i64, stamp: u64) -> Option<u64> {
        self.sync(stamp);
        let at = self
            .segments
            .partition_point(|s| self.first_at(s.start).is_some_and(|first| first <= rowid))
            .checked_sub(1)?;
        let segment = *self.segments.get(at)?;
        let predicted = self.predict(&segment, rowid)?;
        let reach = segment.err.saturating_add(1);
        let end = segment.start.checked_add(segment.len)?;
        let low = predicted.saturating_sub(reach).max(segment.start);
        let high = predicted.saturating_add(reach).saturating_add(1).min(end);
        let window = self.fences.get(low..high)?;
        let index = window
            .partition_point(|f| f.first <= rowid)
            .checked_sub(1)?;
        window.get(index).map(|f| f.page)
    }

    /// The page named proved the answer.
    pub fn confirm(&mut self) {
        self.counts.hits = self.counts.hits.saturating_add(1);
    }

    /// The page named didn't prove the answer: its fence is dropped.
    pub fn refute(&mut self, page: u64) {
        self.counts.refuted = self.counts.refuted.saturating_add(1);
        if let Some(first) = self.by_page.remove(&page) {
            let at = self.fences.partition_point(|f| f.first < first);
            if self.fences.get(at).is_some_and(|f| f.page == page) {
                self.splice(at, 1, None);
            }
        }
    }

    /// Replaces fences `at..at + removed` with `inserted`, then refits the segments the change
    /// touched, merged into one run, and shifts the segments after it.
    fn splice(&mut self, at: usize, removed: usize, inserted: Option<Fence>) {
        let old_len = self.fences.len();
        let Some(end) = at.checked_add(removed).filter(|end| *end <= old_len) else {
            return;
        };
        self.fences.splice(at..end, inserted);
        let added = usize::from(inserted.is_some());
        // The segments overlapping the change, or the one it lands beside.
        let touched_low = at.min(old_len.saturating_sub(1));
        let touched_high = end.max(touched_low.saturating_add(1));
        let first = self
            .segments
            .partition_point(|s| s.start.saturating_add(s.len) <= touched_low);
        let last = self.segments.partition_point(|s| s.start < touched_high);
        let run_start = self.segments.get(first).map_or(0, |s| s.start);
        let run_end_old = last
            .checked_sub(1)
            .and_then(|i| self.segments.get(i))
            .map_or(old_len, |s| s.start.saturating_add(s.len));
        let run_len = run_end_old
            .saturating_add(added)
            .saturating_sub(removed)
            .saturating_sub(run_start);
        let others = self
            .segments
            .len()
            .saturating_sub(last.saturating_sub(first));
        let fitted = self.fit_run(run_start, run_len, others);
        let Some(fitted) = fitted else {
            // A run that doesn't fit (it can't: the indices come from the fences) refits all.
            self.segments = self.fit_run(0, self.fences.len(), 0).unwrap_or_default();
            return;
        };
        for segment in self.segments.iter_mut().skip(last) {
            segment.start = segment.start.saturating_add(added).saturating_sub(removed);
        }
        self.segments.splice(first..last.max(first), fitted);
        let refit = u64::try_from(run_len).unwrap_or(u64::MAX);
        self.counts.upkeep = self.counts.upkeep.saturating_add(refit);
    }

    /// A descent found `leaf`: its fence replaces the fences it now overlaps and its own old one.
    pub fn learn(&mut self, leaf: &LeafSpan<i64>, stamp: u64) -> Learned {
        self.sync(stamp);
        let fence = Fence {
            first: leaf.first,
            last: leaf.last,
            page: leaf.page,
        };
        if let Some(&old) = self.by_page.get(&leaf.page) {
            let at = self.fences.partition_point(|f| f.first < old);
            if self.fences.get(at) == Some(&fence) {
                return Learned::Unchanged;
            }
            if self.fences.get(at).is_some_and(|f| f.page == leaf.page) {
                self.splice(at, 1, None);
            }
            self.by_page.remove(&leaf.page);
        }
        let low = self.fences.partition_point(|f| f.last < leaf.first);
        let high = self
            .fences
            .partition_point(|f| f.first <= leaf.last)
            .max(low);
        let removed = high.saturating_sub(low);
        if self.fences.len().saturating_sub(removed) >= self.budget {
            return Learned::Refused(Refusal::Budget);
        }
        for gone in self.fences.get(low..high).unwrap_or_default().to_vec() {
            self.by_page.remove(&gone.page);
        }
        self.splice(low, removed, Some(fence));
        self.by_page.insert(leaf.page, leaf.first);
        self.counts.built = self.counts.built.saturating_add(1);
        Learned::Kept
    }

    /// Whether every fence lies inside its segment's window: the invariant `lookup` relies on.
    #[cfg(test)]
    fn windows_hold(&self) -> bool {
        let mut next = 0usize;
        for segment in &self.segments {
            if segment.start != next || segment.len == 0 {
                return false;
            }
            for index in segment.start..segment.start + segment.len {
                let predicted = self.predict(segment, self.fences[index].first).unwrap();
                if predicted.abs_diff(index) > segment.err {
                    return false;
                }
            }
            next = segment.start + segment.len;
        }
        next == self.fences.len()
    }
}

/// A table leaf's span as its bytes say: the proof a named page must give before its answer is
/// taken. `None` for any other page (interior, an index's, an empty leaf) or one that doesn't decode.
pub fn table_leaf_span(contents: &PageContent, page: u64) -> Option<LeafSpan<i64>> {
    if !contents.is_table() || !contents.is_leaf() {
        return None;
    }
    let cells = contents.cell_count();
    let last = contents
        .cell_table_leaf_read_rowid(cells.checked_sub(1)?)
        .ok()?;
    let first = contents.cell_table_leaf_read_rowid(0).ok()?;
    Some(LeafSpan {
        page,
        first,
        last,
        cells,
    })
}

/// Which prototype the cursor hook consults: `AMPHORA_DESCENT_SKIP` = `off` | `hash` | `locator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Switch {
    Off,
    Hash,
    Locator,
}

/// The switch as the environment sets it, read once; anything but `hash` or `locator` is off.
pub(crate) fn switched() -> Switch {
    static ON: crate::sync::LazyLock<Switch> =
        crate::sync::LazyLock::new(|| match std::env::var("AMPHORA_DESCENT_SKIP").as_deref() {
            Ok("hash") => Switch::Hash,
            Ok("locator") => Switch::Locator,
            _ => Switch::Off,
        });
    *ON
}

/// A snapshot's identity and the connection's pages-left count: what a learned page is valid under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    left: u64,
    wal: (u32, u64),
}

/// What the cursor hook counted, for its tests and the bench.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HookCounts {
    /// Seeks that stood on a leaf the hash named, without descending.
    pub stood_hash: u64,
    /// Seeks that stood on a leaf the locator named, without descending.
    pub stood_locator: u64,
    /// Named pages not used because they weren't clean in the cache (not a refutation).
    pub uncached: u64,
    /// Learned leaves dropped because the stamp moved.
    pub stamp_drops: u64,
}

#[derive(Debug, Default)]
struct Store {
    stamp: Option<Stamp>,
    /// Raised each time the stamp moves: the prototypes' own stamp.
    generation: u64,
    /// Prototype D over every tree: a key is its tree's root and its rowid.
    hash: Option<AdaptiveHash<(i64, i64)>>,
    /// Prototype E, one locator a tree, by root page.
    locators: HashMap<i64, LeafLocator>,
    /// Fences the locators hold together, kept within the page budget.
    fences: usize,
    counts: HookCounts,
}

impl Store {
    /// Moves to `stamp`, dropping everything learned under another.
    fn sync(&mut self, stamp: Stamp) {
        if self.stamp == Some(stamp) {
            return;
        }
        if self.stamp.is_some() {
            self.counts.stamp_drops = self.counts.stamp_drops.saturating_add(1);
        }
        self.stamp = Some(stamp);
        self.generation = self.generation.saturating_add(1);
        self.hash = None;
        self.locators.clear();
        self.fences = 0;
    }
}

/// The cursor hook's state for one connection, held by its pager.
///
/// Its bounds come from the connection's page cache, since only a page clean in that cache is
/// ever used: the locators hold at most a fence per cached page together, and the hash at most
/// as many keys as the cached pages can hold cells (SQLite's `MX_CELL`, `(usable - 8) / 6`,
/// btreeInt.h: a cell takes at least 4 bytes plus its 2-byte pointer, after an 8-byte header).
#[derive(Debug, Default)]
pub struct Hook {
    /// Pages this connection freed or restored (a rollback), raised before the change.
    left: crate::sync::atomic::AtomicU64,
    store: crate::sync::Mutex<Store>,
}

impl Hook {
    /// A page leaves its tree (freed), or pages go back to what they were (a rollback).
    pub(crate) fn page_left(&self) {
        self.left
            .fetch_add(1, crate::sync::atomic::Ordering::AcqRel);
    }

    fn stamp(&self, wal: (u32, u64)) -> Stamp {
        Stamp {
            left: self.left.load(crate::sync::atomic::Ordering::Acquire),
            wal,
        }
    }

    pub fn counts(&self) -> HookCounts {
        self.store.lock().counts
    }

    /// The prototype's counts under `switch`: the hash's, or the locators' summed.
    pub fn prototype_counts(&self, switch: Switch) -> Counts {
        let store = self.store.lock();
        match switch {
            Switch::Off => Counts::default(),
            Switch::Hash => store.hash.as_ref().map(|h| h.counts()).unwrap_or_default(),
            Switch::Locator => store.locators.values().fold(Counts::default(), |sum, l| {
                let c = l.counts();
                Counts {
                    hits: sum.hits.saturating_add(c.hits),
                    refuted: sum.refuted.saturating_add(c.refuted),
                    built: sum.built.saturating_add(c.built),
                    upkeep: sum.upkeep.saturating_add(c.upkeep),
                    evicted: sum.evicted.saturating_add(c.evicted),
                    stamp_drops: sum.stamp_drops.saturating_add(c.stamp_drops),
                }
            }),
        }
    }

    /// The leaf the prototype names for `rowid` in the tree at `root`, for the cursor to prove.
    pub(crate) fn name(
        &self,
        switch: Switch,
        root: i64,
        rowid: i64,
        wal: (u32, u64),
    ) -> Option<u64> {
        let stamp = self.stamp(wal);
        let mut store = self.store.lock();
        store.sync(stamp);
        let generation = store.generation;
        match switch {
            Switch::Off => None,
            Switch::Hash => store.hash.as_mut()?.lookup(&(root, rowid), generation),
            Switch::Locator => store.locators.get_mut(&root)?.lookup(rowid, generation),
        }
    }

    /// The named page proved the key: the cursor stands on it.
    pub(crate) fn confirm(&self, switch: Switch, root: i64) {
        let mut store = self.store.lock();
        match switch {
            Switch::Off => {}
            Switch::Hash => {
                store.counts.stood_hash = store.counts.stood_hash.saturating_add(1);
                if let Some(hash) = store.hash.as_mut() {
                    hash.confirm();
                }
            }
            Switch::Locator => {
                store.counts.stood_locator = store.counts.stood_locator.saturating_add(1);
                if let Some(locator) = store.locators.get_mut(&root) {
                    locator.confirm();
                }
            }
        }
    }

    /// The named page wasn't clean in the cache: the cursor descends, the prototype keeps it.
    pub(crate) fn uncached(&self) {
        let mut store = self.store.lock();
        store.counts.uncached = store.counts.uncached.saturating_add(1);
    }

    /// The named page didn't prove the key: the prototype forgets it.
    pub(crate) fn refute(&self, switch: Switch, root: i64, page: u64) {
        let mut store = self.store.lock();
        match switch {
            Switch::Off => {}
            Switch::Hash => {
                if let Some(hash) = store.hash.as_mut() {
                    hash.refute(page);
                }
            }
            Switch::Locator => {
                let Some(locator) = store.locators.get_mut(&root) else {
                    return;
                };
                let before = locator.fences();
                locator.refute(page);
                let after = locator.fences();
                store.fences = store.fences.saturating_sub(before).saturating_add(after);
            }
        }
    }

    /// A plain descent for `rowid` in the tree at `root` ended at the clean `leaf` after `above`
    /// probes over interior pages: teaches the prototype. `pages` is the connection's cache
    /// capacity and `usable` its page's usable bytes, the bounds' sources.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn descended(
        &self,
        switch: Switch,
        root: i64,
        rowid: i64,
        leaf: &LeafSpan<i64>,
        above: u64,
        wal: (u32, u64),
        pages: usize,
        usable: usize,
        keys: impl FnOnce() -> Vec<i64>,
    ) {
        let stamp = self.stamp(wal);
        let mut store = self.store.lock();
        store.sync(stamp);
        let generation = store.generation;
        match switch {
            Switch::Off => {}
            Switch::Hash => {
                let cells = usable.saturating_sub(8).checked_div(6).unwrap_or(0);
                let budget = pages.saturating_mul(cells);
                let hash = store
                    .hash
                    .get_or_insert_with(|| AdaptiveHash::new(budget, generation));
                let span = LeafSpan {
                    page: leaf.page,
                    first: (root, leaf.first),
                    last: (root, leaf.last),
                    cells: leaf.cells,
                };
                hash.descended(&(root, rowid), &span, above, generation, || {
                    keys().into_iter().map(|key| (root, key)).collect()
                });
            }
            Switch::Locator => {
                let held = store.fences;
                if !store.locators.contains_key(&root) && held >= pages {
                    return;
                }
                let locator = store
                    .locators
                    .entry(root)
                    .or_insert_with(|| LeafLocator::new(pages, generation));
                let before = locator.fences();
                // The locators together hold at most a fence a cached page.
                let room = pages.saturating_sub(held).saturating_add(before);
                if before >= room {
                    return;
                }
                locator.learn(leaf, generation);
                let after = locator.fences();
                store.fences = held.saturating_sub(before).saturating_add(after);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    use super::{AdaptiveHash, LeafLocator, LeafSpan};

    /// A model B-tree's page: SQLite's table pages, rowids to rows at the leaves and separators
    /// over children inside (child `i` holds `seps[i - 1] <= rowid < seps[i]`).
    #[derive(Debug, Clone)]
    enum Node {
        Leaf(Vec<(i64, u64)>),
        Interior { seps: Vec<i64>, kids: Vec<u64> },
    }

    /// Pages shared by every tree: a freed page is reused by whichever tree allocates next, and
    /// `frees` is the stamp (it rises whenever a page leaves its tree).
    #[derive(Debug, Default)]
    struct Pages {
        nodes: Vec<Option<Node>>,
        free: Vec<u64>,
        frees: u64,
    }

    impl Pages {
        fn alloc(&mut self, node: Node) -> u64 {
            match self.free.pop() {
                Some(page) => {
                    self.nodes[page as usize] = Some(node);
                    page
                }
                None => {
                    self.nodes.push(Some(node));
                    (self.nodes.len() - 1) as u64
                }
            }
        }
        fn release(&mut self, page: u64) {
            self.nodes[page as usize] = None;
            self.free.push(page);
            self.frees += 1;
        }
        fn node(&self, page: u64) -> &Node {
            self.nodes[page as usize].as_ref().unwrap()
        }
        fn node_mut(&mut self, page: u64) -> &mut Node {
            self.nodes[page as usize].as_mut().unwrap()
        }
        /// The leaf's span, if `page` is a non-empty leaf.
        fn span(&self, page: u64) -> Option<LeafSpan<i64>> {
            match self.nodes.get(page as usize)?.as_ref()? {
                Node::Leaf(cells) => Some(LeafSpan {
                    page,
                    first: cells.first()?.0,
                    last: cells.last()?.0,
                    cells: cells.len(),
                }),
                Node::Interior { .. } => None,
            }
        }
        fn row(&self, leaf: u64, rowid: i64) -> Option<u64> {
            match self.node(leaf) {
                Node::Leaf(cells) => cells
                    .binary_search_by_key(&rowid, |c| c.0)
                    .ok()
                    .map(|i| cells[i].1),
                Node::Interior { .. } => panic!("not a leaf"),
            }
        }
    }

    fn bits(n: usize) -> u64 {
        super::bits(n)
    }

    /// A rowid table as a model B-tree: its root page never moves (SQLite's).
    struct Tree {
        root: u64,
        cap: usize,
    }

    impl Tree {
        fn new(pages: &mut Pages, cap: usize) -> Self {
            Self {
                root: pages.alloc(Node::Leaf(Vec::new())),
                cap,
            }
        }

        /// The plain descent: the path of (interior page, child taken), the leaf, and the probes
        /// made above it.
        fn descend(&self, pages: &Pages, rowid: i64) -> (Vec<(u64, usize)>, u64, u64) {
            let mut path = Vec::new();
            let mut page = self.root;
            let mut probes = 0;
            while let Node::Interior { seps, kids } = pages.node(page) {
                let at = seps.partition_point(|s| *s <= rowid);
                probes += bits(seps.len());
                path.push((page, at));
                page = kids[at];
            }
            (path, page, probes)
        }

        fn get(&self, pages: &Pages, rowid: i64) -> Option<u64> {
            let (_, leaf, _) = self.descend(pages, rowid);
            pages.row(leaf, rowid)
        }

        fn insert(&self, pages: &mut Pages, rowid: i64, row: u64) {
            let (mut path, leaf, _) = self.descend(pages, rowid);
            let Node::Leaf(cells) = pages.node_mut(leaf) else {
                unreachable!()
            };
            match cells.binary_search_by_key(&rowid, |c| c.0) {
                Ok(i) => cells[i].1 = row,
                Err(i) => cells.insert(i, (rowid, row)),
            }
            let mut page = leaf;
            loop {
                let (sep, right) = match pages.node_mut(page) {
                    Node::Leaf(cells) if cells.len() > self.cap => {
                        let right = cells.split_off(cells.len() / 2);
                        (right[0].0, Node::Leaf(right))
                    }
                    Node::Interior { seps, kids } if seps.len() > self.cap => {
                        let mid = seps.len() / 2;
                        let right_seps = seps.split_off(mid + 1);
                        let up = seps.pop().unwrap();
                        let right_kids = kids.split_off(mid + 1);
                        (
                            up,
                            Node::Interior {
                                seps: right_seps,
                                kids: right_kids,
                            },
                        )
                    }
                    _ => return,
                };
                let right = pages.alloc(right);
                match path.pop() {
                    Some((parent, at)) => {
                        let Node::Interior { seps, kids } = pages.node_mut(parent) else {
                            unreachable!()
                        };
                        seps.insert(at, sep);
                        kids.insert(at + 1, right);
                        page = parent;
                    }
                    None => {
                        // The root splits: its cells move to a new page, it becomes interior.
                        let left = pages.node(page).clone();
                        let left = pages.alloc(left);
                        *pages.node_mut(page) = Node::Interior {
                            seps: vec![sep],
                            kids: vec![left, right],
                        };
                        return;
                    }
                }
            }
        }

        fn delete(&self, pages: &mut Pages, rowid: i64) {
            let (mut path, leaf, _) = self.descend(pages, rowid);
            let Node::Leaf(cells) = pages.node_mut(leaf) else {
                unreachable!()
            };
            let Ok(i) = cells.binary_search_by_key(&rowid, |c| c.0) else {
                return;
            };
            cells.remove(i);
            let small = cells.len() < self.cap / 2;
            // A small leaf takes in its right sibling when both fit in one page.
            if small {
                if let Some(&(parent, at)) = path.last() {
                    let Node::Interior { kids, .. } = pages.node(parent) else {
                        unreachable!()
                    };
                    if let Some(&sibling) = kids.get(at + 1) {
                        let (Node::Leaf(mine), Node::Leaf(theirs)) =
                            (pages.node(leaf), pages.node(sibling))
                        else {
                            unreachable!()
                        };
                        if mine.len() + theirs.len() <= self.cap {
                            let theirs = theirs.clone();
                            let Node::Leaf(mine) = pages.node_mut(leaf) else {
                                unreachable!()
                            };
                            mine.extend(theirs);
                            let Node::Interior { seps, kids } = pages.node_mut(parent) else {
                                unreachable!()
                            };
                            seps.remove(at);
                            kids.remove(at + 1);
                            pages.release(sibling);
                        }
                    }
                }
            }
            // An empty page leaves its parent, up the path.
            let mut page = leaf;
            while let Some((parent, at)) = path.pop() {
                let empty = match pages.node(page) {
                    Node::Leaf(cells) => cells.is_empty(),
                    Node::Interior { kids, .. } => kids.is_empty(),
                };
                if !empty {
                    break;
                }
                let Node::Interior { seps, kids } = pages.node_mut(parent) else {
                    unreachable!()
                };
                kids.remove(at);
                if !seps.is_empty() {
                    seps.remove(at.saturating_sub(1).min(seps.len() - 1));
                }
                pages.release(page);
                page = parent;
            }
            // A root with one child takes the child's place.
            loop {
                let only = match pages.node(self.root) {
                    Node::Interior { kids, .. } if kids.len() == 1 => kids[0],
                    Node::Interior { kids, .. } if kids.is_empty() => {
                        *pages.node_mut(self.root) = Node::Leaf(Vec::new());
                        return;
                    }
                    _ => return,
                };
                let child = pages.node(only).clone();
                *pages.node_mut(self.root) = child;
                pages.release(only);
            }
        }

        /// Every non-empty leaf, in order.
        fn leaves(&self, pages: &Pages) -> Vec<LeafSpan<i64>> {
            let mut out = Vec::new();
            let mut stack = vec![self.root];
            while let Some(page) = stack.pop() {
                match pages.node(page) {
                    Node::Leaf(_) => out.extend(pages.span(page)),
                    Node::Interior { kids, .. } => stack.extend(kids.iter().rev()),
                }
            }
            out
        }
    }

    /// A lookup through the adaptive hash: the page it names if the page proves the answer, else
    /// the descent, reported to it.
    fn via_hash(
        hash: &mut AdaptiveHash<i64>,
        pages: &Pages,
        tree: &Tree,
        rowid: i64,
    ) -> Option<u64> {
        if let Some(page) = hash.lookup(&rowid, pages.frees) {
            match pages.span(page) {
                Some(span) if span.covers(&rowid) => {
                    hash.confirm();
                    return pages.row(page, rowid);
                }
                _ => hash.refute(page),
            }
        }
        let (_, leaf, above) = tree.descend(pages, rowid);
        if let Some(span) = pages.span(leaf) {
            hash.descended(&rowid, &span, above, pages.frees, || {
                match pages.node(leaf) {
                    Node::Leaf(cells) => cells.iter().map(|c| c.0).collect(),
                    Node::Interior { .. } => Vec::new(),
                }
            });
        }
        pages.row(leaf, rowid)
    }

    /// The same through the learned locator.
    fn via_locator(
        locator: &mut LeafLocator,
        pages: &Pages,
        tree: &Tree,
        rowid: i64,
    ) -> Option<u64> {
        if let Some(page) = locator.lookup(rowid, pages.frees) {
            match pages.span(page) {
                Some(span) if span.covers(&rowid) => {
                    locator.confirm();
                    return pages.row(page, rowid);
                }
                _ => locator.refute(page),
            }
        }
        let (_, leaf, _) = tree.descend(pages, rowid);
        if let Some(span) = pages.span(leaf) {
            locator.learn(&span, pages.frees);
        }
        pages.row(leaf, rowid)
    }

    /// T-G3.5.1: two rowid tables share pages (a page one frees, the other reuses), under random
    /// inserts (appends and fills, splitting leaves and the root), deletes (single and ranges,
    /// merging and freeing leaves) and lookups of present and absent rowids; every lookup through
    /// the adaptive hash and through the learned locator returns the row a plain descent returns,
    /// and each prototype answered lookups itself, was refuted, and dropped all on a free.
    #[test]
    fn t_g3_5_1_skips_return_what_a_descent_returns() {
        for seed in 0..8u64 {
            let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235 ^ seed);
            let mut pages = Pages::default();
            let cap = rng.random_range(4..12);
            let a = Tree::new(&mut pages, cap);
            let b = Tree::new(&mut pages, cap);
            let budget = rng.random_range(16..400);
            let mut hash = AdaptiveHash::new(budget, pages.frees);
            let mut locator = LeafLocator::new(budget, pages.frees);
            let mut next = 0i64;
            for step in 0..6000 {
                match rng.random_range(0..10) {
                    0..=2 => {
                        let tree = if rng.random_range(0..3) == 0 { &b } else { &a };
                        let rowid = if rng.random_range(0..2) == 0 {
                            next += rng.random_range(1..4);
                            next
                        } else {
                            rng.random_range(-5..next + 5)
                        };
                        tree.insert(&mut pages, rowid, rng.random());
                    }
                    3 => {
                        let tree = if rng.random_range(0..3) == 0 { &b } else { &a };
                        let from = rng.random_range(-5..next + 5);
                        let to = from + if rng.random_range(0..8) == 0 { 40 } else { 0 };
                        for rowid in from..=to {
                            tree.delete(&mut pages, rowid);
                        }
                    }
                    4 if step % 500 == 0 => {
                        locator.rebuild(a.leaves(&pages), pages.frees);
                        assert!(locator.windows_hold(), "seed {seed} step {step}: rebuilt");
                    }
                    _ => {
                        let rowid = rng.random_range(-5..next + 5);
                        let want = a.get(&pages, rowid);
                        assert_eq!(
                            via_hash(&mut hash, &pages, &a, rowid),
                            want,
                            "seed {seed} step {step} hash {rowid}"
                        );
                        assert_eq!(
                            via_locator(&mut locator, &pages, &a, rowid),
                            want,
                            "seed {seed} step {step} locator {rowid}"
                        );
                        assert!(locator.windows_hold(), "seed {seed} step {step}: learned");
                        assert!(hash.held() <= budget, "seed {seed}: over budget");
                        assert!(locator.fences() <= budget, "seed {seed}: over budget");
                    }
                }
            }
            let (h, l) = (hash.counts(), locator.counts());
            assert!(
                h.hits > 0 && h.built > 0 && h.refuted > 0 && h.stamp_drops > 0,
                "seed {seed}: hash vacuous: {h:?}"
            );
            assert!(
                l.hits > 0 && l.built > 0 && l.refuted > 0 && l.stamp_drops > 0,
                "seed {seed}: locator vacuous: {l:?}"
            );
        }
    }

    /// T-G3.5.2: over a table's complete fences, the locator names exactly the leaf a descent
    /// finds for every rowid, present or absent between leaves; and over a table whose rowids are
    /// dense for a while and then sparse (one line misses by a quarter of the leaves, two are
    /// near exact), its fit keeps more than one segment. (Rowids with random gaps spread evenly are
    /// a random walk around one line: there more segments don't pay, and the fit keeps one.)
    #[test]
    fn t_g3_5_2_locator_names_the_descents_leaf() {
        let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235_0002);
        let mut pages = Pages::default();
        let tree = Tree::new(&mut pages, 8);
        let mut next = 0i64;
        for row in 0..4000 {
            next += if row < 2000 {
                rng.random_range(1..3)
            } else {
                rng.random_range(40..60)
            };
            tree.insert(&mut pages, next, 0);
        }
        let mut locator = LeafLocator::new(usize::MAX, pages.frees);
        locator.rebuild(tree.leaves(&pages), pages.frees);
        assert!(locator.windows_hold());
        assert!(locator.segments() > 1, "one segment over uneven leaves");
        for rowid in 1..next {
            let (_, leaf, _) = tree.descend(&pages, rowid);
            let span = pages.span(leaf).unwrap();
            let named = locator.lookup(rowid, pages.frees);
            if span.covers(&rowid) {
                assert_eq!(named, Some(leaf), "rowid {rowid}");
            }
        }
    }

    /// A workload size from the environment, else `default`: the bench's input, not a tuning value.
    fn bench_size(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    }

    /// The micro-benchmark behind G3.5's measurements, over the model B-tree: lookup and upkeep
    /// costs of the plain descent, the adaptive hash (D) and the learned locator (E). Run it,
    /// release, nothing else building:
    /// `cargo test -p amphora-engine --release --lib descent_skip_bench -- --ignored --nocapture`
    /// (`AMPHORA_SKIP_BENCH_ROWS`, `_LOOKUPS`, `_WRITES` and `_CAP`, cells a leaf, size it). It
    /// prints, per mode:
    /// - `read`: nanoseconds a lookup over a read-only tree, warm, and the share it answered;
    /// - `build`: D's nanoseconds a key hashed, E's nanoseconds a fence fitted by `rebuild`, to set
    ///   against the descent's nanoseconds a probe (the ski rental's one assumption);
    /// - `mixed`: nanoseconds an operation over a mix of one write to every three lookups, and the
    ///   upkeep each write caused (refutes, rebuilds, keys hashed or fences refitted a write).
    ///
    /// It asserts nothing about time (each mode's answers are checked equal to the descent's).
    #[test]
    #[ignore = "micro-benchmark: run on a quiet machine, release"]
    fn descent_skip_bench() {
        use std::time::Instant;

        let rows = bench_size("AMPHORA_SKIP_BENCH_ROWS", 1_000_000);
        let lookups = bench_size("AMPHORA_SKIP_BENCH_LOOKUPS", 1_000_000);
        let writes = bench_size("AMPHORA_SKIP_BENCH_WRITES", 200_000);
        let cap = bench_size("AMPHORA_SKIP_BENCH_CAP", 100);
        let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235_4245);
        let mut pages = Pages::default();
        let tree = Tree::new(&mut pages, cap);
        for rowid in 0..rows as i64 {
            tree.insert(&mut pages, rowid, rowid as u64);
        }
        let keys: Vec<i64> = (0..lookups)
            .map(|_| rng.random_range(0..rows as i64))
            .collect();
        let budget = rows.saturating_mul(2);

        // Read-only: each mode warm, then timed.
        let started = Instant::now();
        let mut probes = 0u64;
        let mut want = Vec::with_capacity(keys.len());
        for &rowid in &keys {
            let (_, leaf, above) = tree.descend(&pages, rowid);
            probes += above + bits(pages.span(leaf).map_or(0, |s| s.cells));
            want.push(pages.row(leaf, rowid));
        }
        let descent_ns = started.elapsed().as_nanos() as f64 / keys.len() as f64;
        let probes_a_lookup = probes as f64 / keys.len() as f64;
        println!(
            "descent_skip_bench descent read: rows {rows} cap {cap} {descent_ns:.1} ns/lookup, \
             {probes_a_lookup:.2} probes/lookup, {:.2} ns/probe",
            descent_ns / probes_a_lookup
        );

        let mut hash = AdaptiveHash::new(budget, pages.frees);
        for &rowid in &keys {
            via_hash(&mut hash, &pages, &tree, rowid);
        }
        let warm = hash.counts();
        let started = Instant::now();
        for (&rowid, want) in keys.iter().zip(&want) {
            assert_eq!(via_hash(&mut hash, &pages, &tree, rowid), *want);
        }
        let hash_ns = started.elapsed().as_nanos() as f64 / keys.len() as f64;
        let hits = hash.counts().hits - warm.hits;
        // D's build cost: every leaf's keys hashed, timed.
        let leaves = tree.leaves(&pages);
        let mut fresh = AdaptiveHash::new(budget, pages.frees);
        let started = Instant::now();
        for leaf in &leaves {
            let first = leaf.first;
            fresh.descended(&first, leaf, u64::MAX, pages.frees, || {
                match pages.node(leaf.page) {
                    Node::Leaf(cells) => cells.iter().map(|c| c.0).collect(),
                    Node::Interior { .. } => Vec::new(),
                }
            });
        }
        let hash_build_ns =
            started.elapsed().as_nanos() as f64 / fresh.counts().upkeep.max(1) as f64;
        println!(
            "descent_skip_bench hash read: {hash_ns:.1} ns/lookup, answered {:.3}, entries {} | build {hash_build_ns:.2} ns/key",
            hits as f64 / keys.len() as f64,
            hash.held()
        );

        let mut locator = LeafLocator::new(budget, pages.frees);
        let started = Instant::now();
        locator.rebuild(leaves.iter().cloned(), pages.frees);
        let rebuild_ns = started.elapsed().as_nanos() as f64 / leaves.len().max(1) as f64;
        let warm = locator.counts();
        let started = Instant::now();
        for (&rowid, want) in keys.iter().zip(&want) {
            assert_eq!(via_locator(&mut locator, &pages, &tree, rowid), *want);
        }
        let locator_ns = started.elapsed().as_nanos() as f64 / keys.len() as f64;
        let hits = locator.counts().hits - warm.hits;
        println!(
            "descent_skip_bench locator read: {locator_ns:.1} ns/lookup, answered {:.3}, fences {} segments {} max err {} \
             | build {rebuild_ns:.2} ns/fence",
            hits as f64 / keys.len() as f64,
            locator.fences(),
            locator.segments(),
            locator.max_err()
        );

        // Mixed: a write (an append, a random insert or a delete) to every three lookups, each mode
        // on its own copy of the same tree and the same operations.
        let ops: Vec<(u8, i64)> = (0..writes * 4)
            .map(|i| {
                if i % 4 == 0 {
                    (rng.random_range(0..3), rng.random_range(0..rows as i64 * 2))
                } else {
                    (3, rng.random_range(0..rows as i64 * 2))
                }
            })
            .collect();
        for mode in ["descent", "hash", "locator"] {
            let mut pages = Pages::default();
            let tree = Tree::new(&mut pages, cap);
            for rowid in 0..rows as i64 {
                tree.insert(&mut pages, rowid, rowid as u64);
            }
            let mut hash = AdaptiveHash::new(budget, pages.frees);
            let mut locator = LeafLocator::new(budget, pages.frees);
            locator.rebuild(tree.leaves(&pages), pages.frees);
            let (hash_before, locator_before) = (hash.counts(), locator.counts());
            let mut appended = rows as i64;
            let started = Instant::now();
            for &(op, rowid) in &ops {
                match op {
                    0 => {
                        appended += 1;
                        tree.insert(&mut pages, appended, 0);
                    }
                    1 => tree.insert(&mut pages, rowid, 0),
                    2 => tree.delete(&mut pages, rowid),
                    _ => {
                        let got = match mode {
                            "hash" => via_hash(&mut hash, &pages, &tree, rowid),
                            "locator" => via_locator(&mut locator, &pages, &tree, rowid),
                            _ => tree.get(&pages, rowid),
                        };
                        std::hint::black_box(got);
                    }
                }
            }
            let ns = started.elapsed().as_nanos() as f64 / ops.len() as f64;
            let counts = match mode {
                "hash" => Some((hash_before, hash.counts())),
                "locator" => Some((locator_before, locator.counts())),
                _ => None,
            };
            let upkeep = counts.map_or(String::new(), |(before, after)| {
                format!(
                    " | a write: refuted {:.3} upkeep {:.2} stamp drops {:.4}; answered {:.3} of lookups",
                    (after.refuted - before.refuted) as f64 / writes as f64,
                    (after.upkeep - before.upkeep) as f64 / writes as f64,
                    (after.stamp_drops - before.stamp_drops) as f64 / writes as f64,
                    (after.hits - before.hits) as f64 / (writes * 3) as f64,
                )
            });
            println!("descent_skip_bench {mode} mixed: {ns:.1} ns/op{upkeep}");
        }
    }

    /// The engine's side: a real rowid table, its pages read through the pager.
    mod engine {
        use std::num::NonZero;
        use std::sync::Arc;

        use rand::{Rng, SeedableRng};
        use rand_chacha::ChaCha8Rng;

        use super::super::{table_leaf_span, AdaptiveHash, LeafLocator, LeafSpan};
        use super::bits;
        use crate::io::{MemoryIO, IO};
        use crate::storage::btree::{BTreeCursor, CursorTrait};
        use crate::storage::pager::Pager;
        use crate::types::{SeekKey, SeekOp, SeekResult, Value};
        use crate::util::IOExt;
        use crate::{ConnectOpts, Database, SqliteDialect};

        fn open() -> (Arc<Database>, Arc<crate::Connection>) {
            let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
            let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v)")
                .unwrap();
            (db, conn)
        }

        fn insert(conn: &Arc<crate::Connection>, rowid: i64, value: i64) {
            let mut stmt = conn
                .prepare("INSERT OR REPLACE INTO t (id, v) VALUES (?, ?)")
                .unwrap();
            stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(rowid))
                .unwrap();
            stmt.bind_at(NonZero::new(2).unwrap(), Value::from_i64(value))
                .unwrap();
            stmt.run_ignore_rows().unwrap();
        }

        fn root(conn: &Arc<crate::Connection>) -> i64 {
            let rows = conn
                .prepare("SELECT rootpage FROM sqlite_schema WHERE name = 't'")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            rows[0][0].as_int().unwrap()
        }

        fn span(pager: &Arc<Pager>, page: u64) -> Option<LeafSpan<i64>> {
            let (page_ref, _) = pager.io.block(|| pager.read_page(page as i64)).ok()?;
            while page_ref.is_locked() {
                pager.io.step().unwrap();
            }
            table_leaf_span(page_ref.get_contents(), page)
        }

        /// The row a lookup finds in a proven leaf: the cell holding `rowid`, if any.
        fn cell_in(pager: &Arc<Pager>, leaf: &LeafSpan<i64>, rowid: i64) -> Option<usize> {
            let (page_ref, _) = pager
                .io
                .block(|| pager.read_page(leaf.page as i64))
                .unwrap();
            let contents = page_ref.get_contents();
            let (mut low, mut high) = (0usize, leaf.cells);
            while low < high {
                let mid = (low + high) / 2;
                let at = contents.cell_table_leaf_read_rowid(mid).unwrap();
                match at.cmp(&rowid) {
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Greater => high = mid,
                    std::cmp::Ordering::Equal => return Some(mid),
                }
            }
            None
        }

        /// The plain descent: the leaf and cell the cursor stands on when it found `rowid`, the
        /// probes it made above the leaf, and the leaf's span.
        fn descend(
            pager: &Arc<Pager>,
            cursor: &mut BTreeCursor,
            rowid: i64,
        ) -> (Option<(u64, usize)>, u64, Option<LeafSpan<i64>>) {
            let found = pager
                .io
                .block(|| cursor.seek(SeekKey::TableRowId(rowid), SeekOp::GE { eq_only: true }))
                .unwrap();
            let levels = cursor.position();
            let (leaf, cell) = *levels.last().unwrap();
            let mut above = 0;
            for (page, _) in &levels[..levels.len() - 1] {
                let (page_ref, _) = pager.io.block(|| pager.read_page(*page as i64)).unwrap();
                above += bits(page_ref.get_contents().cell_count());
            }
            let at = matches!(found, SeekResult::Found).then(|| (leaf as u64, cell as usize));
            (at, above, span(pager, leaf as u64))
        }

        enum Mode<'a> {
            Hash(&'a mut AdaptiveHash<i64>),
            Locator(&'a mut LeafLocator),
        }

        /// A lookup through a prototype on the engine's pages: the page it names, proven by its
        /// bytes, else the descent, reported to it. The leaf and cell found, if any.
        fn skip(
            pager: &Arc<Pager>,
            cursor: &mut BTreeCursor,
            mode: &mut Mode<'_>,
            rowid: i64,
            stamp: u64,
        ) -> Option<(u64, usize)> {
            let named = match mode {
                Mode::Hash(hash) => hash.lookup(&rowid, stamp),
                Mode::Locator(locator) => locator.lookup(rowid, stamp),
            };
            if let Some(page) = named {
                match span(pager, page) {
                    Some(leaf) if leaf.covers(&rowid) => {
                        match mode {
                            Mode::Hash(hash) => hash.confirm(),
                            Mode::Locator(locator) => locator.confirm(),
                        }
                        return cell_in(pager, &leaf, rowid).map(|cell| (page, cell));
                    }
                    _ => match mode {
                        Mode::Hash(hash) => hash.refute(page),
                        Mode::Locator(locator) => locator.refute(page),
                    },
                }
            }
            let (at, above, leaf) = descend(pager, cursor, rowid);
            if let Some(leaf) = leaf {
                match mode {
                    Mode::Hash(hash) => {
                        hash.descended(&rowid, &leaf, above, stamp, || {
                            let (page_ref, _) = pager
                                .io
                                .block(|| pager.read_page(leaf.page as i64))
                                .unwrap();
                            let contents = page_ref.get_contents();
                            (0..leaf.cells)
                                .map(|i| contents.cell_table_leaf_read_rowid(i).unwrap())
                                .collect()
                        });
                    }
                    Mode::Locator(locator) => {
                        locator.learn(&leaf, stamp);
                    }
                }
            }
            at
        }

        /// A mode's lookups, warm, then timed: nanoseconds a lookup.
        fn timed(
            pager: &Arc<Pager>,
            cursor: &mut BTreeCursor,
            mode: &mut Mode<'_>,
            keys: &[i64],
            want: &[Option<(u64, usize)>],
        ) -> f64 {
            for &rowid in keys {
                skip(pager, cursor, mode, rowid, 0);
            }
            let started = std::time::Instant::now();
            for (&rowid, want) in keys.iter().zip(want) {
                assert_eq!(skip(pager, cursor, mode, rowid, 0), *want);
            }
            started.elapsed().as_nanos() as f64 / keys.len() as f64
        }

        /// T-G3.5.3: the same on the engine's own pages: a rowid table through committed rounds of
        /// inserts (splits) and range deletes (merges, freed pages), then lookups of present and
        /// absent rowids; each prototype's page, proven by its bytes (`table_leaf_span`), gives the
        /// leaf and cell a cursor's descent stands on, or none when the descent finds none. The
        /// stamp is the round: pages are freed only by a round's writes (the cursor hook's
        /// freed-page count stands in for it there).
        #[test]
        fn t_g3_5_3_skips_on_engine_pages_match_the_cursor() {
            let (_db, conn) = open();
            let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235_0003);
            let mut hash = AdaptiveHash::new(4096, 0);
            let mut locator = LeafLocator::new(4096, 0);
            let mut next = 0i64;
            for round in 1..=6u64 {
                conn.execute("BEGIN").unwrap();
                for _ in 0..1500 {
                    let rowid = if rng.random_range(0..2) == 0 {
                        next += rng.random_range(1..4);
                        next
                    } else {
                        rng.random_range(0..next + 1)
                    };
                    insert(&conn, rowid, rng.random());
                }
                let from = rng.random_range(0..next + 1);
                conn.execute(format!(
                    "DELETE FROM t WHERE id BETWEEN {from} AND {}",
                    from + 400
                ))
                .unwrap();
                conn.execute("COMMIT").unwrap();
                let root = root(&conn);
                let pager = conn.pager.load().clone();
                pager.begin_read_tx().unwrap();
                let mut cursor = BTreeCursor::new_table(pager.clone(), root, 2);
                let mut plain = BTreeCursor::new_table(pager.clone(), root, 2);
                for _ in 0..3000 {
                    let rowid = rng.random_range(-3..next + 3);
                    let (want, _, _) = descend(&pager, &mut plain, rowid);
                    let got = skip(
                        &pager,
                        &mut cursor,
                        &mut Mode::Hash(&mut hash),
                        rowid,
                        round,
                    );
                    assert_eq!(got, want, "round {round} hash {rowid}");
                    let got = skip(
                        &pager,
                        &mut cursor,
                        &mut Mode::Locator(&mut locator),
                        rowid,
                        round,
                    );
                    assert_eq!(got, want, "round {round} locator {rowid}");
                }
                pager.end_read_tx();
            }
            let (h, l) = (hash.counts(), locator.counts());
            assert!(h.hits > 0 && h.built > 0, "hash vacuous: {h:?}");
            assert!(l.hits > 0 && l.built > 0, "locator vacuous: {l:?}");
        }

        /// Steps `cursor` forward (`step` 0) or back (1), else not at all, and reads the rowid it
        /// reaches.
        fn step_once(
            pager: &Arc<Pager>,
            cursor: &mut BTreeCursor,
            step: u32,
        ) -> Option<Option<i64>> {
            match step {
                0 => pager.io.block(|| cursor.next()).unwrap(),
                1 => pager.io.block(|| cursor.prev()).unwrap(),
                _ => return None,
            }
            Some(pager.io.block(|| cursor.rowid()).unwrap())
        }

        /// T-G3.5.4: the cursor hook through the real cursor. Two connections to one WAL database
        /// take turns writing (random inserts, splits, range deletes freeing pages) while the
        /// other reads on at its older snapshot; on each connection a cursor with each switch
        /// setting (off, hash, locator) seeks random rowids, present and absent, and then steps
        /// once forward or back. Every seek stands where the plain descent stands (leaf and cell),
        /// with the same result, and every step after it reaches the same row: a cursor standing on
        /// a named leaf descends again before it moves. The hook's counts show seeks stood on a
        /// named leaf, under both prototypes, and that the stamp moved with the snapshots.
        #[test]
        fn t_g3_5_4_cursor_hook_matches_the_plain_descent() {
            use super::super::Switch;

            let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
            let db = Database::open_file(io, "descent_skip.db", Arc::new(SqliteDialect)).unwrap();
            let conns = [
                db.connect(ConnectOpts::new()).unwrap(),
                db.connect(ConnectOpts::new()).unwrap(),
            ];
            conns[0]
                .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v)")
                .unwrap();
            let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235_0004);
            let mut next = 0i64;
            let mut reading = [false, false];
            let switches = [Switch::Off, Switch::Hash, Switch::Locator];
            for round in 0..8usize {
                let writer = round % 2;
                let pager = conns[writer].pager.load().clone();
                if reading[writer] {
                    pager.end_read_tx();
                    reading[writer] = false;
                }
                conns[writer].execute("BEGIN").unwrap();
                for _ in 0..1200 {
                    let rowid = if rng.random_range(0..2) == 0 {
                        next += rng.random_range(1..4);
                        next
                    } else {
                        rng.random_range(0..next + 1)
                    };
                    insert(&conns[writer], rowid, rng.random());
                }
                let from = rng.random_range(0..next + 1);
                conns[writer]
                    .execute(format!(
                        "DELETE FROM t WHERE id BETWEEN {from} AND {}",
                        from + 300
                    ))
                    .unwrap();
                conns[writer].execute("COMMIT").unwrap();
                let root = root(&conns[writer]);
                pager.begin_read_tx().unwrap();
                reading[writer] = true;
                // Both connections read: the writer at the new snapshot, the other at its older one.
                for reader in 0..2 {
                    if !reading[reader] {
                        continue;
                    }
                    let pager = conns[reader].pager.load().clone();
                    let mut cursors: Vec<BTreeCursor> = switches
                        .iter()
                        .map(|&switch| {
                            let mut cursor = BTreeCursor::new_table(pager.clone(), root, 2);
                            cursor.set_descent_skip(switch);
                            cursor
                        })
                        .collect();
                    let mut plain = BTreeCursor::new_table(pager.clone(), root, 2);
                    for _ in 0..3000 {
                        let rowid = rng.random_range(-3..next + 3);
                        let step = rng.random_range(0..8);
                        let key = || SeekKey::TableRowId(rowid);
                        let op = SeekOp::GE { eq_only: true };
                        let want = pager.io.block(|| plain.seek(key(), op)).unwrap();
                        let want_at = plain.position().last().copied();
                        let want_row = pager.io.block(|| plain.rowid()).unwrap();
                        let want_step = step_once(&pager, &mut plain, step);
                        for (cursor, switch) in cursors.iter_mut().zip(switches) {
                            let got = pager.io.block(|| cursor.seek(key(), op)).unwrap();
                            let why = format!("round {round} reader {reader} {switch:?} {rowid}");
                            assert_eq!(got, want, "{why}");
                            assert_eq!(cursor.position().last().copied(), want_at, "{why}");
                            assert_eq!(
                                pager.io.block(|| cursor.rowid()).unwrap(),
                                want_row,
                                "{why}"
                            );
                            let got_step = step_once(&pager, cursor, step);
                            assert_eq!(got_step, want_step, "{why} step {step}");
                        }
                    }
                }
            }
            for (at, conn) in conns.iter().enumerate() {
                let hook = &conn.pager.load().descent_skip;
                let counts = hook.counts();
                let (hash, locator) = (
                    hook.prototype_counts(Switch::Hash),
                    hook.prototype_counts(Switch::Locator),
                );
                // The prototypes' own counts are the last stamp's; the hook's span every stamp.
                assert!(
                    counts.stood_hash > 0,
                    "connection {at}: hash never stood: {counts:?}"
                );
                assert!(
                    counts.stood_locator > 0,
                    "connection {at}: locator never stood: {counts:?}"
                );
                assert!(
                    counts.stamp_drops > 0,
                    "connection {at}'s stamp never moved: {counts:?}"
                );
                println!("connection {at}: {counts:?} hash {hash:?} locator {locator:?}");
            }
        }

        /// The lookup costs on the engine's pages, behind AC-G3.5.1: nanoseconds a point lookup by
        /// a cursor's descent, by the adaptive hash and by the learned locator (each naming a page
        /// read through the pager and proven by its bytes), warm, over a table of
        /// `AMPHORA_SKIP_BENCH_ROWS` rows; and E's rebuild, nanoseconds a leaf. Release, quiet:
        /// `cargo test -p amphora-engine --release --lib descent_skip_engine_bench -- --ignored --nocapture`.
        /// It asserts nothing about time (answers are checked equal to the descent's).
        #[test]
        #[ignore = "micro-benchmark: run on a quiet machine, release"]
        fn descent_skip_engine_bench() {
            use std::time::Instant;

            let size = |name: &str, default: usize| -> usize {
                std::env::var(name)
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(default)
            };
            let rows = size("AMPHORA_SKIP_BENCH_ROWS", 200_000);
            let lookups = size("AMPHORA_SKIP_BENCH_LOOKUPS", 200_000);
            let (_db, conn) = open();
            conn.execute("BEGIN").unwrap();
            for rowid in 0..rows as i64 {
                insert(&conn, rowid, rowid);
            }
            conn.execute("COMMIT").unwrap();
            let root = root(&conn);
            let pager = conn.pager.load().clone();
            pager.begin_read_tx().unwrap();
            let mut rng = ChaCha8Rng::seed_from_u64(0x0050_5235_4246);
            let keys: Vec<i64> = (0..lookups)
                .map(|_| rng.random_range(0..rows as i64))
                .collect();
            let mut cursor = BTreeCursor::new_table(pager.clone(), root, 2);
            let mut want = Vec::with_capacity(keys.len());
            for &rowid in &keys {
                want.push(descend(&pager, &mut cursor, rowid).0);
            }
            let started = Instant::now();
            for &rowid in &keys {
                pager
                    .io
                    .block(|| cursor.seek(SeekKey::TableRowId(rowid), SeekOp::GE { eq_only: true }))
                    .unwrap();
            }
            let descent_ns = started.elapsed().as_nanos() as f64 / keys.len() as f64;

            let mut hash = AdaptiveHash::new(rows.saturating_mul(2), 0);
            let mut locator = LeafLocator::new(rows, 0);
            let mut leaves: Vec<LeafSpan<i64>> = want
                .iter()
                .flatten()
                .filter_map(|(page, _)| span(&pager, *page))
                .collect();
            leaves.sort_unstable_by_key(|leaf| leaf.first);
            leaves.dedup_by_key(|leaf| leaf.page);
            let started = Instant::now();
            locator.rebuild(leaves.iter().cloned(), 0);
            let rebuild_ns = started.elapsed().as_nanos() as f64 / leaves.len().max(1) as f64;
            let hash_ns = timed(
                &pager,
                &mut cursor,
                &mut Mode::Hash(&mut hash),
                &keys,
                &want,
            );
            let locator_ns = timed(
                &pager,
                &mut cursor,
                &mut Mode::Locator(&mut locator),
                &keys,
                &want,
            );
            println!(
                "descent_skip_engine_bench rows {rows} lookups {lookups}: descent {descent_ns:.1} ns/lookup \
                 | hash {hash_ns:.1} ns/lookup ({:?}) | locator {locator_ns:.1} ns/lookup, {} fences \
                 {} segments max err {}, rebuild {rebuild_ns:.1} ns/leaf ({:?})",
                hash.counts(),
                locator.fences(),
                locator.segments(),
                locator.max_err(),
                locator.counts(),
            );
            pager.end_read_tx();
        }
    }
}
