//! Amphora: a table's columns, a copy derived from its rows (`docs/design/columnar.md`, decision 1;
//! G11 step 2, CS.2).
//!
//! The rows stay the record. A commit hands its redo record ([`Op`]s, as built) to the copy's
//! [`Feed`] under the publish lock, at the point it appends it to the redo log; the copy's own work
//! ([`Columns::catch_up`]) applies what is queued, in timestamp order, as much as is queued at a time,
//! with no timer. The copy is a generation of immutable blocks cut in key order, each current as of
//! the commit timestamp the generation was made at, and positional deltas: each change after it
//! filed by the block it falls in, its place in that block's key column, and its key, with every
//! version since (decision 1, parts 3 and 5). A scan at snapshot `S` reads a generation made at or
//! before `S`: each block's rows, overridden by the deltas' newest versions at or before `S`, and
//! for the keys the queue holds at or before `S`, the row B-tree's rows at `S`. It never waits for
//! the copy; a scan no generation can answer exactly reads the rows.
//!
//! Two prototypes behind one core, differing only in decision 1's part 4 ([`Format`]):
//! - A, [`Plain`]: blocks in memory only, uncompressed, never logged; rebuilt at open from a
//!   snapshot scan of the rows (SQLite3/HE, Oracle's dual format).
//! - B, [`Packed`]: blocks compressed (keys front-coded, lengths as LEB128) and kept, with their
//!   deltas, as a file outside the redo log ([`Shelf`]), reused at open when still current (Data
//!   Blocks, positional delta trees, Colibri).
//!
//! No size here is a number in the code (decision 1, part 6): a generation's block size is chosen
//! by [`Sizer`] from the cost per scanned row measured at each size tried, the sizes tried being
//! the table's row count halved; a rebuild is wanted once the scan time the deltas cost, as
//! measured, reaches the last rebuild's measured time; the queue's bound is the bytes of the
//! database's memory budget its caller gives it (CLAUDE.md §2).
//!
//! The copy orders keys bytewise: it serves trees whose comparator is bytewise.

use super::redo::Op;
use super::version::Ts;
use crate::error::LimboError;
use crate::sync::{Arc, Mutex, RwLock};
use crate::Result;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub mod scan;

/// A row change the copy keeps: the key and the row after it (None: removed).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Change {
    pub key: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

struct Queued {
    ts: Ts,
    /// The caller's tick when it was offered: what the freshness lag is measured from.
    at: u64,
    changes: Vec<Change>,
}

#[derive(Default)]
struct Queue {
    records: VecDeque<Queued>,
    bytes: usize,
    /// The oldest and newest commits refused for a full queue since the last rebuild.
    lost: Option<(Ts, Ts)>,
}

/// What the feed has counted: offers taken, offers refused for a full queue.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct FeedCounts {
    pub offered: u64,
    pub lost: u64,
}

/// The copy's queue: the commits' records for one tree, handed over under the publish lock.
pub struct Feed {
    tree: u64,
    /// The queue's bytes may not pass it: the share of the database's memory budget its caller
    /// charges the queue to (CLAUDE.md §2).
    bound: usize,
    /// The caller's clock, read at each offer: what the freshness lag is measured from.
    tick: Box<dyn Fn() -> u64 + Send + Sync>,
    queue: Mutex<Queue>,
    counts: Mutex<FeedCounts>,
}

fn change_bytes(c: &Change) -> usize {
    c.key
        .len()
        .saturating_add(c.value.as_ref().map_or(0, Vec::len))
}

impl Feed {
    /// A feed for tree `tree` whose queue holds at most `bound` bytes of keys and rows; `tick`
    /// the caller's clock, in the unit [`Columns::catch_up`] is given.
    pub fn new(tree: u64, bound: usize, tick: Box<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            tree,
            bound,
            tick,
            queue: Mutex::new(Queue::default()),
            counts: Mutex::new(FeedCounts::default()),
        }
    }

    /// The commit `ts`'s changes (call in timestamp order: under the publish lock). Never refuses the commit: a record that would pass the bound is not queued, and the
    /// copy is behind from it until a rebuild after it (counted in [`Feed::counts`]).
    pub fn offer(&self, ts: Ts, ops: &[Op]) {
        let mut changes = Vec::new();
        for op in ops {
            match op {
                Op::Put { tree, key, value } if *tree == self.tree => changes.push(Change {
                    key: key.clone(),
                    value: Some(value.clone()),
                }),
                Op::Remove { tree, key } if *tree == self.tree => changes.push(Change {
                    key: key.clone(),
                    value: None,
                }),
                _ => {}
            }
        }
        if changes.is_empty() {
            return;
        }
        let bytes = changes
            .iter()
            .fold(0usize, |n, c| n.saturating_add(change_bytes(c)));
        let at = (self.tick)();
        let mut q = self.queue.lock();
        let fits = q
            .bytes
            .checked_add(bytes)
            .is_some_and(|after| after <= self.bound);
        let mut counts = self.counts.lock();
        if fits {
            q.bytes = q.bytes.saturating_add(bytes);
            q.records.push_back(Queued { ts, at, changes });
            counts.offered = counts.offered.saturating_add(1);
        } else {
            q.lost = Some(q.lost.map_or((ts, ts), |(first, _)| (first, ts)));
            counts.lost = counts.lost.saturating_add(1);
        }
    }

    pub fn counts(&self) -> FeedCounts {
        *self.counts.lock()
    }

    /// The bytes queued now.
    pub fn queued_bytes(&self) -> usize {
        self.queue.lock().bytes
    }
}

/// Rows in key order: each key and its row.
pub type Rowset = Vec<(Vec<u8>, Vec<u8>)>;

/// What a block's rows are handed to: each row's position, key and row; false stops.
pub type RowVisit<'a> = dyn FnMut(usize, &[u8], &[u8]) -> Result<bool> + 'a;

/// One block's rows as a format holds them, cut in key order.
pub trait Block: Send + Sync + Sized {
    /// The block of `rows` (in key order, none empty of keys).
    fn make(rows: &[(Vec<u8>, Vec<u8>)]) -> Result<Self>;
    fn first(&self) -> &[u8];
    fn last(&self) -> &[u8];
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// How many of its keys sort before `key`: where `key` is, or would be, in the block.
    fn rank(&self, key: &[u8]) -> Result<usize>;
    /// Each row in order, with its position, while `f` returns true.
    fn rows(&self, f: &mut RowVisit<'_>) -> Result<()>;
    /// The bytes it keeps resident.
    fn resident(&self) -> usize;
}

/// Prototype A's block: the key and row columns in memory, uncompressed.
pub struct Plain {
    keys: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
}

/// What a `Vec<u8>`'s header takes beside its bytes: a format value (pointer, capacity, length).
const VEC_HEAD: usize = std::mem::size_of::<Vec<u8>>();

impl Block for Plain {
    fn make(rows: &[(Vec<u8>, Vec<u8>)]) -> Result<Self> {
        Ok(Self {
            keys: rows.iter().map(|(k, _)| k.clone()).collect(),
            values: rows.iter().map(|(_, v)| v.clone()).collect(),
        })
    }
    fn first(&self) -> &[u8] {
        self.keys.first().map_or(&[], Vec::as_slice)
    }
    fn last(&self) -> &[u8] {
        self.keys.last().map_or(&[], Vec::as_slice)
    }
    fn len(&self) -> usize {
        self.keys.len()
    }
    fn rank(&self, key: &[u8]) -> Result<usize> {
        Ok(self.keys.partition_point(|k| k.as_slice() < key))
    }
    fn rows(&self, f: &mut RowVisit<'_>) -> Result<()> {
        for (i, (k, v)) in self.keys.iter().zip(&self.values).enumerate() {
            if !f(i, k, v)? {
                break;
            }
        }
        Ok(())
    }
    fn resident(&self) -> usize {
        self.keys.iter().chain(&self.values).fold(0usize, |n, c| {
            n.saturating_add(c.len()).saturating_add(VEC_HEAD)
        })
    }
}

fn push_leb(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let low = u8::try_from(v & 0x7f).unwrap_or(0);
        v = v.checked_shr(7).unwrap_or(0);
        if v == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

fn corrupt(what: &str) -> LimboError {
    LimboError::Corrupt(format!("column copy: {what}"))
}

fn read_leb(bytes: &[u8], at: &mut usize) -> Result<usize> {
    let mut v = 0usize;
    let mut shift = 0u32;
    loop {
        let b = *bytes
            .get(*at)
            .ok_or_else(|| corrupt("a length past the end"))?;
        *at = at.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        let part = usize::from(b & 0x7f)
            .checked_shl(shift)
            .ok_or_else(|| corrupt("a length too long"))?;
        v |= part;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift = shift.checked_add(7).ok_or(LimboError::IntegerOverflow)?;
        if shift >= usize::BITS {
            return Err(corrupt("a length too long"));
        }
    }
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(n).ok_or(LimboError::IntegerOverflow)?;
    let s = bytes
        .get(*at..end)
        .ok_or_else(|| corrupt("bytes past the end"))?;
    *at = end;
    Ok(s)
}

/// Prototype B's block: the key column front-coded (each key the length it shares with the one
/// before and the rest), then the row column, lengths as LEB128, in one buffer; its first and last
/// keys whole, for finding a key's block.
pub struct Packed {
    bytes: Vec<u8>,
    /// Where the row column starts.
    values_at: usize,
    n: usize,
    first: Vec<u8>,
    last: Vec<u8>,
}

impl Packed {
    /// Each key in order, with its position, while `f` returns true; where the row column starts.
    fn keys(&self, f: &mut dyn FnMut(usize, &[u8]) -> Result<bool>) -> Result<()> {
        let mut at = 0usize;
        let mut key: Vec<u8> = Vec::new();
        for i in 0..self.n {
            let shared = read_leb(&self.bytes, &mut at)?;
            let rest = read_leb(&self.bytes, &mut at)?;
            key.truncate(shared);
            key.extend_from_slice(take(&self.bytes, &mut at, rest)?);
            if !f(i, &key)? {
                break;
            }
        }
        Ok(())
    }

    fn encode(&self, out: &mut Vec<u8>) {
        push_leb(out, self.n);
        push_leb(out, self.values_at);
        push_leb(out, self.bytes.len());
        out.extend_from_slice(&self.bytes);
        push_leb(out, self.first.len());
        out.extend_from_slice(&self.first);
        push_leb(out, self.last.len());
        out.extend_from_slice(&self.last);
    }

    fn decode(bytes: &[u8], at: &mut usize) -> Result<Self> {
        let n = read_leb(bytes, at)?;
        let values_at = read_leb(bytes, at)?;
        let len = read_leb(bytes, at)?;
        let body = take(bytes, at, len)?.to_vec();
        let fl = read_leb(bytes, at)?;
        let first = take(bytes, at, fl)?.to_vec();
        let ll = read_leb(bytes, at)?;
        let last = take(bytes, at, ll)?.to_vec();
        if values_at > body.len() {
            return Err(corrupt("a row column past the block's end"));
        }
        Ok(Self {
            bytes: body,
            values_at,
            n,
            first,
            last,
        })
    }
}

impl Block for Packed {
    fn make(rows: &[(Vec<u8>, Vec<u8>)]) -> Result<Self> {
        let mut bytes = Vec::new();
        let mut prev: &[u8] = &[];
        for (k, _) in rows {
            let shared = prev.iter().zip(k).take_while(|(a, b)| a == b).count();
            let rest = k.get(shared..).unwrap_or(&[]);
            push_leb(&mut bytes, shared);
            push_leb(&mut bytes, rest.len());
            bytes.extend_from_slice(rest);
            prev = k;
        }
        let values_at = bytes.len();
        for (_, v) in rows {
            push_leb(&mut bytes, v.len());
            bytes.extend_from_slice(v);
        }
        bytes.shrink_to_fit();
        Ok(Self {
            bytes,
            values_at,
            n: rows.len(),
            first: rows.first().map(|(k, _)| k.clone()).unwrap_or_default(),
            last: rows.last().map(|(k, _)| k.clone()).unwrap_or_default(),
        })
    }
    fn first(&self) -> &[u8] {
        &self.first
    }
    fn last(&self) -> &[u8] {
        &self.last
    }
    fn len(&self) -> usize {
        self.n
    }
    fn rank(&self, key: &[u8]) -> Result<usize> {
        let mut rank = self.n;
        self.keys(&mut |i, k| {
            if k >= key {
                rank = i;
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(rank)
    }
    fn rows(&self, f: &mut RowVisit<'_>) -> Result<()> {
        let mut vat = self.values_at;
        let bytes = &self.bytes;
        self.keys(&mut |i, k| {
            let len = read_leb(bytes, &mut vat)?;
            let v = take(bytes, &mut vat, len)?;
            f(i, k, v)
        })
    }
    fn resident(&self) -> usize {
        self.bytes
            .len()
            .saturating_add(self.first.len())
            .saturating_add(self.last.len())
            .saturating_add(VEC_HEAD.saturating_mul(3))
    }
}

/// A change's versions since its generation was made, oldest first (None: removed).
type Versions = Vec<(Ts, Option<Vec<u8>>)>;

/// The newest of `v`'s versions at or before `s`.
fn at(v: &Versions, s: Ts) -> Option<&Option<Vec<u8>>> {
    v.iter().rev().find(|(ts, _)| *ts <= s).map(|(_, r)| r)
}

fn usable<B>(g: &Option<Generation<B>>, s: Ts) -> Option<&Generation<B>> {
    g.as_ref().filter(|g| g.made <= s)
}

/// Immutable blocks as of commit `made`, and the positional deltas since: by block, place in the
/// block's key column, and key.
struct Generation<B> {
    /// Unique in the process: what a cache of its decoded blocks is keyed by.
    id: u64,
    made: Ts,
    blocks: Vec<B>,
    deltas: BTreeMap<(usize, usize, Vec<u8>), Versions>,
    delta_bytes: usize,
    /// The deltas' versions: what a rebuild folds.
    delta_versions: u64,
}

/// Generations made in this process so far: each one's id.
static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_generation() -> u64 {
    GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl<B: Block> Generation<B> {
    fn new(made: Ts, rows: &[(Vec<u8>, Vec<u8>)], size: usize) -> Result<Self> {
        let mut blocks = Vec::new();
        for chunk in rows.chunks(size.max(1)) {
            blocks.push(B::make(chunk)?);
        }
        // An empty table's generation still has a block, for its deltas to be filed under.
        if blocks.is_empty() {
            blocks.push(B::make(&[])?);
        }
        Ok(Self {
            id: next_generation(),
            made,
            blocks,
            deltas: BTreeMap::new(),
            delta_bytes: 0,
            delta_versions: 0,
        })
    }

    /// Where `key` is filed: its block (the first whose last key is at or after it, else the last)
    /// and its place there.
    fn locate(&self, key: &[u8]) -> Result<(usize, usize)> {
        let at = self.blocks.partition_point(|b| b.last() < key);
        let j = at.min(self.blocks.len().saturating_sub(1));
        let pos = match self.blocks.get(j) {
            Some(b) => b.rank(key)?,
            None => 0,
        };
        Ok((j, pos))
    }

    fn apply(&mut self, ts: Ts, c: Change) -> Result<()> {
        let (j, pos) = self.locate(&c.key)?;
        self.delta_bytes = self.delta_bytes.saturating_add(change_bytes(&c));
        self.delta_versions = self.delta_versions.saturating_add(1);
        self.deltas
            .entry((j, pos, c.key))
            .or_default()
            .push((ts, c.value));
        Ok(())
    }

    fn resident(&self) -> usize {
        self.blocks
            .iter()
            .fold(self.delta_bytes, |n, b| n.saturating_add(b.resident()))
    }

    fn rows(&self) -> usize {
        self.blocks
            .iter()
            .fold(0usize, |n, b| n.saturating_add(b.len()))
    }

    /// The rows at `s` (made at or before it), but for the keys in `skip`, into `out`, in key
    /// order: how many delta versions the scan looked at.
    fn scan(&self, s: Ts, skip: &BTreeSet<Vec<u8>>, out: &mut Rowset) -> Result<u64> {
        let mut looked = 0u64;
        for j in 0..self.blocks.len() {
            looked = looked.saturating_add(self.scan_block(j, s, skip, out)?);
        }
        Ok(looked)
    }

    /// The deltas filed under block `j`.
    fn block_deltas(
        &self,
        j: usize,
    ) -> std::collections::btree_map::Range<'_, (usize, usize, Vec<u8>), Versions> {
        self.deltas
            .range((j, 0, Vec::new())..(j.saturating_add(1), 0, Vec::new()))
    }

    /// Block `j`'s rows at `s`, with its deltas, but for the keys in `skip`, into `out`, in key
    /// order: how many delta versions it looked at.
    fn scan_block(
        &self,
        j: usize,
        s: Ts,
        skip: &BTreeSet<Vec<u8>>,
        out: &mut Rowset,
    ) -> Result<u64> {
        let Some(block) = self.blocks.get(j) else {
            return Ok(0);
        };
        let mut looked = 0u64;
        let emit = |k: &[u8], v: &[u8], out: &mut Rowset| {
            if !skip.contains(k) {
                out.push((k.to_vec(), v.to_vec()));
            }
        };
        let mut deltas = self.block_deltas(j).peekable();
        block.rows(&mut |i, k, v| {
            // Inserted keys filed before this row's place, or at it with a smaller key.
            let mut own: Option<&Versions> = None;
            while let Some(((_, pos, dk), versions)) = deltas.peek() {
                if *pos > i || (*pos == i && dk.as_slice() > k) {
                    break;
                }
                looked = looked.saturating_add(1);
                if *pos == i && dk.as_slice() == k {
                    own = Some(*versions);
                } else if let Some(Some(row)) = at(versions, s) {
                    emit(dk, row, out);
                }
                deltas.next();
            }
            match own.and_then(|v| at(v, s)) {
                Some(Some(row)) => emit(k, row, out),
                Some(None) => {}
                None => emit(k, v, out),
            }
            Ok(true)
        })?;
        for ((_, _, dk), versions) in deltas {
            looked = looked.saturating_add(1);
            if let Some(Some(row)) = at(versions, s) {
                emit(dk, row, out);
            }
        }
        Ok(looked)
    }
}

/// The block size, chosen per table from the cost per scanned row measured at each size tried
/// (decision 1, part 6). The sizes tried are the table's row count halved again and again: the
/// first generation is one block, the next tries half the best size's, and each after takes the
/// size of least measured cost, trying its unmeasured half when it is the smallest measured.
#[derive(Default)]
pub struct Sizer {
    /// By size: nanoseconds and rows scanned at it.
    measured: BTreeMap<usize, (u128, u128)>,
    current: Option<usize>,
}

impl Sizer {
    fn cost(&self, size: usize) -> Option<(u128, u128)> {
        self.measured
            .get(&size)
            .copied()
            .filter(|(_, rows)| *rows > 0)
    }

    /// The size to cut a generation of `rows` rows at.
    pub fn size(&mut self, rows: usize) -> usize {
        let rows = rows.max(1);
        // Least measured cost per row: a*rb < b*ra compares a/ra with b/rb without division.
        let best = self
            .measured
            .iter()
            .filter(|(size, (_, r))| **size <= rows && *r > 0)
            .min_by(|(_, (a, ra)), (_, (b, rb))| a.saturating_mul(*rb).cmp(&b.saturating_mul(*ra)))
            .map(|(size, _)| *size);
        let size = match best {
            None => rows,
            Some(best) => {
                let half = best.checked_shr(1).unwrap_or(0);
                if half > 0 && self.cost(half).is_none() {
                    half
                } else {
                    best
                }
            }
        };
        self.current = Some(size);
        size
    }

    /// A scan of `rows` rows took `nanos` at the current size.
    pub fn measure(&mut self, nanos: u128, rows: usize) {
        if let Some(size) = self.current {
            let e = self.measured.entry(size).or_insert((0, 0));
            e.0 = e.0.saturating_add(nanos);
            e.1 =
                e.1.saturating_add(u128::try_from(rows).unwrap_or(u128::MAX));
        }
    }

    /// The size generations are cut at now.
    pub fn current(&self) -> Option<usize> {
        self.current
    }
}

/// When a rebuild pays (decision 1, part 6): once the scan time the deltas cost since the last
/// rebuild, as measured, reaches that rebuild's measured time (a rebuild's cost against the cost
/// of not making it, both measured here; not a share of changed rows, not a timer).
#[derive(Default)]
struct Rebuilds {
    last_cost: u128,
    delta_cost: u128,
}

struct State<B> {
    current: Option<Generation<B>>,
    /// The generation before, for snapshots older than the current one (decision 1, part 3).
    previous: Option<Generation<B>>,
    /// Commits from the first to the last were refused by a full queue and no rebuild covers the
    /// last yet: scans of the current generation at or after the first read rows.
    behind: Option<(Ts, Ts)>,
    /// The same for the previous generation, as it stood when it was replaced: the commits it
    /// lacks are as much its own as they were when it was current.
    previous_behind: Option<(Ts, Ts)>,
    sizer: Sizer,
    rebuilds: Rebuilds,
    /// The freshness lag of each record applied, in the caller's ticks, since last taken.
    lags: Vec<u64>,
}

/// The rows a scan falls back to, at the scan's snapshot.
pub trait Rows {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn all(&self) -> Result<Rowset>;
}

/// Where a scan's rows came from.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ScanCounts {
    /// Rows from the copy's blocks and deltas.
    pub copy: u64,
    /// Rows read from the row B-tree because the queue held a change to them.
    pub queued: u64,
    /// Scans answered from the rows: no generation answered them exactly.
    pub fallbacks: u64,
    /// Delta versions the scan looked at.
    pub deltas: u64,
}

/// A table's column copy in format `B`: the core both prototypes share.
pub struct Columns<B> {
    feed: Arc<Feed>,
    state: RwLock<State<B>>,
}

impl<B: Block> Columns<B> {
    /// A copy fed by `feed`, empty until [`Columns::rebuild`] makes its first generation.
    pub fn new(feed: Arc<Feed>) -> Self {
        Self {
            feed,
            state: RwLock::new(State {
                current: None,
                previous: None,
                behind: None,
                previous_behind: None,
                sizer: Sizer::default(),
                rebuilds: Rebuilds::default(),
                lags: Vec::new(),
            }),
        }
    }

    pub fn feed(&self) -> &Arc<Feed> {
        &self.feed
    }

    /// Applies every queued record to the current generation, in timestamp order, at tick `now`;
    /// how many. The queue is drained under the copy's write lock, so a scan sees each record in
    /// the queue or in the copy.
    pub fn catch_up(&self, now: u64) -> Result<usize> {
        let mut st = self.state.write();
        let mut q = self.feed.queue.lock();
        let records: Vec<Queued> = q.records.drain(..).collect();
        q.bytes = 0;
        if let Some((first, last)) = q.lost.take() {
            st.behind = Some(
                st.behind
                    .map_or((first, last), |(f, l)| (f.min(first), l.max(last))),
            );
        }
        drop(q);
        let n = records.len();
        let st = &mut *st;
        for r in records {
            st.lags.push(now.saturating_sub(r.at));
            if let Some(g) = st.current.as_mut() {
                if r.ts > g.made {
                    for c in r.changes {
                        g.apply(r.ts, c)?;
                    }
                }
            }
        }
        Ok(n)
    }

    /// Whether a rebuild pays now (see [`Rebuilds`]); always, before the first generation or while
    /// the copy is behind.
    pub fn wants_rebuild(&self) -> bool {
        let st = self.state.read();
        st.current.is_none()
            || st.behind.is_some()
            || (st.rebuilds.delta_cost >= st.rebuilds.last_cost
                && st.current.as_ref().is_some_and(|g| g.delta_versions > 0))
    }

    /// Makes a new generation from `rows`, the tree's rows at snapshot `made`, which took `nanos`
    /// to read before this call; this call's own time is added by the caller through
    /// [`Columns::rebuilt`]. Changes after `made` already applied are carried over; the generation
    /// replaced is kept for older snapshots.
    pub fn rebuild(&self, made: Ts, rows: &[(Vec<u8>, Vec<u8>)], now: u64) -> Result<()> {
        // Records committed up to now go to the old generation first, so it still answers every
        // snapshot before `made`.
        self.catch_up(now)?;
        let mut st = self.state.write();
        let size = st.sizer.size(rows.len());
        let mut g = Generation::<B>::new(made, rows, size)?;
        if let Some(old) = st.current.take() {
            for ((_, _, key), versions) in &old.deltas {
                for (ts, value) in versions.iter().filter(|(ts, _)| *ts > made) {
                    g.apply(
                        *ts,
                        Change {
                            key: key.clone(),
                            value: value.clone(),
                        },
                    )?;
                }
            }
            st.previous = Some(old);
            st.previous_behind = st.behind;
        }
        st.current = Some(g);
        // Commits lost after `made` are missing from the new generation too: it answers no
        // snapshot after `made` until the next rebuild.
        st.behind = st
            .behind
            .filter(|(_, last)| *last > made)
            .map(|(_, last)| (made.saturating_add(1), last));
        st.rebuilds.delta_cost = 0;
        Ok(())
    }

    /// The last rebuild took `nanos`, its read of the rows included.
    pub fn rebuilt(&self, nanos: u128) {
        self.state.write().rebuilds.last_cost = nanos;
    }

    /// The generation that answers snapshot `s` exactly, and the keys the queue holds at or
    /// before `s` (read from the rows instead); None when none does, and the scan reads rows.
    fn pick<'a>(&self, st: &'a State<B>, s: Ts) -> Option<(&'a Generation<B>, BTreeSet<Vec<u8>>)> {
        let queue = self.feed.queue.lock();
        let lost = queue.lost.is_some_and(|(first, _)| first <= s);
        // The current generation answers every snapshot from its making on; the previous one,
        // those before the current's making (it was caught up to it before it was replaced), each
        // but for the commits it lacks.
        let (g, behind) = match usable(&st.current, s) {
            Some(g) => (Some(g), st.behind),
            None => (usable(&st.previous, s), st.previous_behind),
        };
        let behind = behind.is_some_and(|(first, _)| first <= s);
        let g = g.filter(|_| !behind && !lost)?;
        let mut skip = BTreeSet::new();
        if st.current.as_ref().is_some_and(|c| c.made <= s) {
            for r in queue.records.iter().filter(|r| r.ts <= s) {
                for c in &r.changes {
                    skip.insert(c.key.clone());
                }
            }
        }
        Some((g, skip))
    }

    /// The rows at snapshot `s`, in key order; `rows` the row B-tree at `s`. Never waits for the
    /// copy: what it hasn't applied yet is read from the rows.
    pub fn scan(&self, s: Ts, rows: &dyn Rows) -> Result<(Rowset, ScanCounts)> {
        let st = self.state.read();
        let mut counts = ScanCounts::default();
        let Some((g, skip)) = self.pick(&st, s) else {
            counts.fallbacks = 1;
            return Ok((rows.all()?, counts));
        };
        let mut out = Vec::new();
        counts.deltas = g.scan(s, &skip, &mut out)?;
        counts.copy = u64::try_from(out.len()).unwrap_or(u64::MAX);
        if skip.is_empty() {
            return Ok((out, counts));
        }
        let mut fresh = Vec::new();
        for key in skip {
            if let Some(v) = rows.get(&key)? {
                fresh.push((key, v));
            }
        }
        counts.queued = u64::try_from(fresh.len()).unwrap_or(u64::MAX);
        let mut merged = Vec::with_capacity(out.len().saturating_add(fresh.len()));
        let mut a = out.into_iter().peekable();
        let mut b = fresh.into_iter().peekable();
        loop {
            let take_a = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) => x.0 < y.0,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let next = if take_a { a.next() } else { b.next() };
            if let Some(row) = next {
                merged.push(row);
            }
        }
        Ok((merged, counts))
    }

    /// A scan of the copy took `nanos` (the caller's clock): charged to the block size it was cut
    /// at and, by the share of what it looked at that were deltas, to the deltas.
    pub fn measured(&self, nanos: u128, c: &ScanCounts) {
        if c.fallbacks > 0 {
            return;
        }
        let mut st = self.state.write();
        let rows = usize::try_from(c.copy).unwrap_or(usize::MAX);
        st.sizer.measure(nanos, rows);
        let seen = u128::from(c.copy.saturating_add(c.deltas));
        if seen > 0 {
            let share = nanos
                .saturating_mul(u128::from(c.deltas))
                .checked_div(seen)
                .unwrap_or(0);
            st.rebuilds.delta_cost = st.rebuilds.delta_cost.saturating_add(share);
        }
    }

    /// A vectorized scan ([`scan::Plan`]) over a table of `rows` rows took `nanos`: charged to
    /// the block size it was cut at, per row of the table, so the granules its summaries skipped
    /// count for the size that let them be skipped (decision 1, part 6).
    pub fn measured_scan(&self, nanos: u128, rows: usize) {
        self.state.write().sizer.measure(nanos, rows);
    }

    /// The copy's resident bytes: its generations' blocks and deltas, and the queue.
    pub fn resident(&self) -> usize {
        let st = self.state.read();
        let g = |g: &Option<Generation<B>>| g.as_ref().map_or(0, Generation::resident);
        g(&st.current)
            .saturating_add(g(&st.previous))
            .saturating_add(self.feed.queued_bytes())
    }

    /// The current generation's rows in blocks, its block size, and its block count.
    pub fn shape(&self) -> (usize, Option<usize>, usize) {
        let st = self.state.read();
        let rows = st.current.as_ref().map_or(0, Generation::rows);
        let blocks = st.current.as_ref().map_or(0, |g| g.blocks.len());
        (rows, st.sizer.current(), blocks)
    }

    /// The freshness lags of the records applied since last taken, in the caller's ticks.
    pub fn take_lags(&self) -> Vec<u64> {
        std::mem::take(&mut self.state.write().lags)
    }

    /// Drops the previous generation once no open snapshot is older than the current one's
    /// making (`oldest`: the clock's low watermark).
    pub fn release(&self, oldest: Ts) {
        let mut st = self.state.write();
        if st.current.as_ref().is_some_and(|g| g.made <= oldest) {
            st.previous = None;
            st.previous_behind = None;
        }
    }

    /// What the planner costs the copy by at snapshot `s` (G11 step 5,
    /// `translate::optimizer::column_path`): whether a generation answers `s` exactly, by the
    /// rule every scan of the copy reads by (`Columns::pick`); the rows that generation holds;
    /// the distinct keys the queue holds at or before `s`, read from the rows; and the copy's
    /// measured nanoseconds per row of the table at its current block size, as its scans charge
    /// it ([`Columns::measured`], [`Columns::measured_scan`]; None before a scan at that size).
    pub fn plan_view(&self, s: Ts) -> PlanView {
        let st = self.state.read();
        let picked = self.pick(&st, s);
        let nanos_per_row = st
            .sizer
            .current
            .and_then(|size| st.sizer.cost(size))
            .map(|(nanos, rows)| nanos as f64 / rows as f64);
        PlanView {
            exact: picked.is_some(),
            rows: picked.as_ref().map_or(0, |(g, _)| g.rows()),
            queued: picked.as_ref().map_or(0, |(_, skip)| skip.len()),
            nanos_per_row,
        }
    }
}

/// The copy at one snapshot, as the planner costs it ([`Columns::plan_view`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlanView {
    pub exact: bool,
    pub rows: usize,
    pub queued: usize,
    pub nanos_per_row: Option<f64>,
}

/// Where prototype B keeps its file, outside the redo log.
pub trait Shelf: Send + Sync {
    /// Replaces the file's bytes, durably.
    fn put(&self, bytes: &[u8]) -> Result<()>;
    fn get(&self) -> Result<Option<Vec<u8>>>;
}

/// A shelf in memory, for tests.
#[derive(Default)]
pub struct MemShelf(Mutex<Option<Vec<u8>>>);

impl Shelf for MemShelf {
    fn put(&self, bytes: &[u8]) -> Result<()> {
        *self.0.lock() = Some(bytes.to_vec());
        Ok(())
    }
    fn get(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.0.lock().clone())
    }
}

/// A shelf in a file: written beside, flushed, then renamed over.
pub struct FileShelf {
    path: std::path::PathBuf,
}

impl FileShelf {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

fn io(what: &str, e: std::io::Error) -> LimboError {
    LimboError::InternalError(format!("column copy: {what}: {e}"))
}

impl Shelf for FileShelf {
    fn put(&self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut next = self.path.clone().into_os_string();
        next.push(".next");
        let next = std::path::PathBuf::from(next);
        let mut f = std::fs::File::create(&next).map_err(|e| io("create", e))?;
        f.write_all(bytes).map_err(|e| io("write", e))?;
        f.sync_all().map_err(|e| io("flush", e))?;
        std::fs::rename(&next, &self.path).map_err(|e| io("rename", e))
    }
    fn get(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(&self.path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("read", e)),
        }
    }
}

const SHELF_MAGIC: &[u8; 4] = b"ACC1";

impl Columns<Packed> {
    /// Writes the current generation and its deltas, with every commit applied up to `ts` (call
    /// with the database's newest commit, none in flight: at a checkpoint or a close).
    pub fn persist(&self, ts: Ts, shelf: &dyn Shelf, now: u64) -> Result<()> {
        self.catch_up(now)?;
        let st = self.state.read();
        let Some(g) = st.current.as_ref() else {
            return Ok(());
        };
        if st.behind.is_some() {
            return Ok(());
        }
        let mut out = SHELF_MAGIC.to_vec();
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(&g.made.to_le_bytes());
        push_leb(&mut out, g.blocks.len());
        for b in &g.blocks {
            b.encode(&mut out);
        }
        let versions = g
            .deltas
            .values()
            .fold(0usize, |n, v| n.saturating_add(v.len()));
        push_leb(&mut out, versions);
        for ((_, _, key), vs) in &g.deltas {
            for (vts, value) in vs {
                out.extend_from_slice(&vts.to_le_bytes());
                push_leb(&mut out, key.len());
                out.extend_from_slice(key);
                match value {
                    Some(v) => {
                        out.push(1);
                        push_leb(&mut out, v.len());
                        out.extend_from_slice(v);
                    }
                    None => out.push(0),
                }
            }
        }
        shelf.put(&out)
    }

    /// The copy `shelf` holds, if it is current as of `newest` (the database's newest commit at
    /// open): every commit up to it applied. Else None: the caller rebuilds from the rows, as a
    /// checkpoint may have cut the redo that held the changes after it.
    pub fn reopen(feed: Arc<Feed>, shelf: &dyn Shelf, newest: Ts) -> Result<Option<Self>> {
        let Some(bytes) = shelf.get()? else {
            return Ok(None);
        };
        let mut at = 0usize;
        if take(&bytes, &mut at, SHELF_MAGIC.len())? != SHELF_MAGIC {
            return Err(corrupt("not a column copy's file"));
        }
        let u64_at = |at: &mut usize| -> Result<u64> {
            let b = take(&bytes, at, 8)?;
            let arr: [u8; 8] = b.try_into().map_err(|_| corrupt("a short timestamp"))?;
            Ok(u64::from_le_bytes(arr))
        };
        let ts = u64_at(&mut at)?;
        if ts != newest {
            return Ok(None);
        }
        let made = u64_at(&mut at)?;
        let n = read_leb(&bytes, &mut at)?;
        let mut blocks = Vec::new();
        for _ in 0..n {
            blocks.push(Packed::decode(&bytes, &mut at)?);
        }
        let mut g = Generation {
            id: next_generation(),
            made,
            blocks,
            deltas: BTreeMap::new(),
            delta_bytes: 0,
            delta_versions: 0,
        };
        let versions = read_leb(&bytes, &mut at)?;
        for _ in 0..versions {
            let vts = u64_at(&mut at)?;
            let kl = read_leb(&bytes, &mut at)?;
            let key = take(&bytes, &mut at, kl)?.to_vec();
            let flag = take(&bytes, &mut at, 1)?;
            let value = if flag == [1] {
                let vl = read_leb(&bytes, &mut at)?;
                Some(take(&bytes, &mut at, vl)?.to_vec())
            } else {
                None
            };
            g.apply(vts, Change { key, value })?;
        }
        let copy = Self::new(feed);
        copy.state.write().current = Some(g);
        Ok(Some(copy))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::versioned::blink::{Comparator, Descender};
    use crate::storage::versioned::disk::Simulated;
    use crate::storage::versioned::durable::Database;
    use crate::storage::versioned::version::Snapshot;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Instant;

    fn cmp() -> Comparator {
        Box::new(|a: &[u8], b: &[u8]| a.cmp(b))
    }

    struct At<'a> {
        db: &'a Database,
        snap: &'a Snapshot,
    }

    impl Rows for At<'_> {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.db.get(&mut Descender::new(), self.snap, 0, key)
        }
        fn all(&self) -> Result<Rowset> {
            self.db
                .scan(&mut Descender::new(), self.snap, 0, None, usize::MAX)
        }
    }

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    fn rebuild<B: Block>(db: &Database, copy: &Columns<B>, tick: u64) {
        let t = Instant::now();
        let snap = db.snapshot().unwrap();
        let rows = At { db, snap: &snap }.all().unwrap();
        copy.rebuild(snap.start, &rows, tick).unwrap();
        db.close_snapshot(snap);
        copy.rebuilt(t.elapsed().as_nanos());
    }

    /// Runs `f` when dropped, so a thread that fails still lets the others end (a failed assert
    /// ends the test instead of leaving it waiting).
    struct OnDrop<F: FnMut()>(F);

    impl<F: FnMut()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    /// What the scans counted across a run.
    #[derive(Default, Debug)]
    struct Seen {
        scans: u64,
        from_copy: u64,
        with_deltas: u64,
        with_queued: u64,
        fallbacks: u64,
        old_snapshots: u64,
    }

    /// Writers commit puts and removes on one tree while the copy's worker applies the feed and
    /// rebuilds when it pays, and a reader scans the copy at fresh snapshots and at snapshots held
    /// across rebuilds: every scan equals the rows at its snapshot.
    fn scans_equal_rows<B: Block>(bound: usize, seed: u64) -> Seen {
        let db = Database::open(Arc::new(Simulated::new(seed, 1).unwrap()), 4_000, 1, cmp).unwrap();
        let ticks = Arc::new(AtomicU64::new(0));
        let t2 = ticks.clone();
        let feed = Arc::new(Feed::new(
            0,
            bound,
            Box::new(move || t2.fetch_add(1, Ordering::Relaxed)),
        ));
        db.keep_columns(feed.clone()).unwrap();
        let copy = Columns::<B>::new(feed);
        // Rows before the copy's first generation.
        let mut d = Descender::new();
        let mut t = db.begin().unwrap();
        for k in 0..200u16 {
            db.put(&mut d, &mut t, 0, &k.to_be_bytes(), &[1; 8])
                .unwrap();
        }
        db.commit(t, &mut d, db.lane(), false).unwrap();
        rebuild(&db, &copy, 0);
        let stop = AtomicBool::new(false);
        let writers_left = AtomicU64::new(4);
        let mut seen = Seen::default();
        std::thread::scope(|sc| {
            for w in 0..4u64 {
                let db = &db;
                let writers_left = &writers_left;
                sc.spawn(move || {
                    let _left = OnDrop(|| {
                        writers_left.fetch_sub(1, Ordering::AcqRel);
                    });
                    let mut x = seed.wrapping_mul(31).wrapping_add(w) | 1;
                    let mut d = Descender::new();
                    for _ in 0..300 {
                        let mut t = db.begin().unwrap();
                        let mut ok = true;
                        for _ in 0..1 + rng(&mut x) % 4 {
                            let k = u16::try_from(rng(&mut x) % 400).unwrap().to_be_bytes();
                            let r = if rng(&mut x).is_multiple_of(4) {
                                db.remove(&mut d, &mut t, 0, &k).map(|_| ())
                            } else {
                                let v = rng(&mut x).to_le_bytes();
                                db.put(&mut d, &mut t, 0, &k, &v).map(|_| ())
                            };
                            if r.is_err() {
                                ok = false;
                                break;
                            }
                        }
                        if ok {
                            db.commit(t, &mut d, db.lane(), false).unwrap();
                        } else {
                            db.abort(t, &mut d).unwrap();
                        }
                    }
                });
            }
            let (db, copy, stop, ticks) = (&db, &copy, &stop, &ticks);
            sc.spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let now = ticks.load(Ordering::Relaxed);
                    copy.catch_up(now).unwrap();
                    if copy.wants_rebuild() {
                        rebuild(db, copy, now);
                    }
                    copy.release(db.v.clock.low_watermark());
                    std::thread::yield_now();
                }
            });
            let _stop = OnDrop(|| stop.store(true, Ordering::Release));
            let mut held: Vec<Snapshot> = Vec::new();
            let mut x = seed | 1;
            loop {
                let done = writers_left.load(Ordering::Acquire) == 0;
                let snap = db.snapshot().unwrap();
                let mut check = |snap: &Snapshot, old: bool| {
                    let rows = At { db, snap };
                    let t = Instant::now();
                    let (got, c) = copy.scan(snap.start, &rows).unwrap();
                    copy.measured(t.elapsed().as_nanos(), &c);
                    assert_eq!(got, rows.all().unwrap(), "a scan at {}", snap.start);
                    seen.scans += 1;
                    seen.from_copy += u64::from(c.fallbacks == 0);
                    seen.with_deltas += u64::from(c.deltas > 0);
                    seen.with_queued += u64::from(c.queued > 0);
                    seen.fallbacks += c.fallbacks;
                    seen.old_snapshots += u64::from(old && c.fallbacks == 0);
                };
                check(&snap, false);
                if rng(&mut x).is_multiple_of(8) && held.len() < 4 {
                    held.push(snap);
                } else {
                    db.close_snapshot(snap);
                }
                if !held.is_empty() && rng(&mut x).is_multiple_of(3) {
                    let i = usize::try_from(rng(&mut x)).unwrap() % held.len();
                    let s = held.swap_remove(i);
                    check(&s, true);
                    db.close_snapshot(s);
                }
                if done {
                    break;
                }
            }
            for s in held {
                db.close_snapshot(s);
            }
        });
        seen
    }

    /// Do: under concurrent writes, scan prototype A's copy at each snapshot. Expect: each scan
    /// equals the rows at its snapshot, most answered by the copy, some with deltas and some with
    /// queued changes read from the rows (T-CS.2.1).
    #[test]
    fn cs2_scan_at_each_snapshot_equals_rows_plain() {
        let seen = scans_equal_rows::<Plain>(usize::MAX, 7);
        assert!(seen.from_copy > 0 && seen.with_deltas > 0, "{seen:?}");
    }

    /// Do: the same with prototype B's compressed blocks. Expect: the same (T-CS.2.1).
    #[test]
    fn cs2_scan_at_each_snapshot_equals_rows_packed() {
        let seen = scans_equal_rows::<Packed>(usize::MAX, 11);
        assert!(seen.from_copy > 0 && seen.with_deltas > 0, "{seen:?}");
    }

    /// Do: the same with a queue too small for most records (the full-queue nemesis). Expect:
    /// commits are never refused, the copy goes behind, scans read rows while it is, and every
    /// scan still equals the rows at its snapshot (T-CS.2.2).
    #[test]
    fn cs2_full_queue_falls_back_to_rows_and_stays_exact() {
        for seed in [3, 5] {
            let seen = scans_equal_rows::<Plain>(24, seed);
            assert!(seen.scans > 0, "{seen:?}");
            let seen = scans_equal_rows::<Packed>(24, seed);
            assert!(seen.scans > 0, "{seen:?}");
        }
    }

    /// Do: commit through a copy kept by B, persist it at the newest commit, reopen it from its
    /// shelf at that commit and at a later one. Expect: at the same commit it is reused and scans
    /// equal the rows; at a later one it is refused, for a rebuild (T-CS.2.3).
    #[test]
    fn cs2_packed_copy_reused_at_open_only_when_current() {
        let db = Database::open(Arc::new(Simulated::new(9, 1).unwrap()), 2_000, 1, cmp).unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        db.keep_columns(feed.clone()).unwrap();
        let copy = Columns::<Packed>::new(feed);
        rebuild(&db, &copy, 0);
        let mut d = Descender::new();
        for round in 0..3u8 {
            let mut t = db.begin().unwrap();
            for k in 0..60u8 {
                if (k + round) % 5 == 0 {
                    db.remove(&mut d, &mut t, 0, &[k]).unwrap();
                } else {
                    db.put(&mut d, &mut t, 0, &[k], &[round, k]).unwrap();
                }
            }
            db.commit(t, &mut d, db.lane(), false).unwrap();
        }
        let shelf = MemShelf::default();
        copy.persist(db.newest(), &shelf, 0).unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        let again = Columns::<Packed>::reopen(feed.clone(), &shelf, db.newest())
            .unwrap()
            .expect("current");
        let snap = db.snapshot().unwrap();
        let rows = At {
            db: &db,
            snap: &snap,
        };
        let (got, c) = again.scan(snap.start, &rows).unwrap();
        assert_eq!(got, rows.all().unwrap());
        assert_eq!(c.fallbacks, 0);
        assert!(c.deltas > 0);
        db.close_snapshot(snap);
        let stale = Columns::<Packed>::reopen(feed, &shelf, db.newest() + 1).unwrap();
        assert!(stale.is_none());
    }

    /// Do: decode a packed block's bytes cut short, and a shelf of foreign bytes. Expect: typed
    /// refusals, no panic (hostile input).
    #[test]
    fn cs2_packed_refuses_damaged_bytes() {
        let rows: Vec<_> = (0..20u8).map(|k| (vec![k, k], vec![k; 3])).collect();
        let b = Packed::make(&rows).unwrap();
        let mut bytes = Vec::new();
        b.encode(&mut bytes);
        for cut in 0..bytes.len() {
            assert!(Packed::decode(&bytes[..cut], &mut 0).is_err());
        }
        let shelf = MemShelf::default();
        shelf.put(b"nope").unwrap();
        let feed = Arc::new(Feed::new(0, usize::MAX, Box::new(|| 0)));
        assert!(Columns::<Packed>::reopen(feed, &shelf, 0).is_err());
    }

    /// Do: give the sizer measurements where half the size costs less per row, then where it
    /// costs more. Expect: it moves to the cheaper half, then stays at the best measured size; no
    /// size it picks is other than the row count halved (CS.2, decision 1 part 6).
    #[test]
    fn cs2_block_size_follows_measured_cost() {
        let mut s = Sizer::default();
        assert_eq!(s.size(1_000), 1_000);
        s.measure(1_000_000, 1_000);
        assert_eq!(s.size(1_000), 500);
        s.measure(500_000, 1_000);
        assert_eq!(s.size(1_000), 250);
        s.measure(2_000_000, 1_000);
        assert_eq!(s.size(1_000), 500);
        assert_eq!(s.size(1_000), 500);
    }
}
