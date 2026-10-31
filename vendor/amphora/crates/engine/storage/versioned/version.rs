//! Amphora: row versions in the versioned storage (Amphora's `docs/design/engine.md`, decision 4).
//!
//! A row's newest version is its cell in the B-link tree, changed in place by its writer under the
//! leaf's exclusive latch. While a snapshot may still need an older one, the cell is flagged
//! versioned and the row's key maps, beside the pages, to a head: who wrote the cell's version (a
//! transaction while it is open, then its commit timestamp) and the versions before it, newest
//! first, each with the timestamp of the commit that wrote it (0: before any snapshot now open).
//! A removed row keeps its cell, flagged removed, until no snapshot can see the row.
//!
//! Commits take timestamps from one clock and are published in order under one short lock, so a
//! snapshot that begins at timestamp `t` sees exactly the commits at or before `t`. A reader sees,
//! for each row, the cell's version if its writer committed at or before its snapshot (or is its
//! own transaction), else the newest older version committed at or before it. A writer that finds a
//! row's newest version written by another open transaction, or committed after its own snapshot
//! began, is refused (`WriteWriteConflict`): the first writer wins. Versions no snapshot can see go:
//! the low watermark is the oldest open snapshot's timestamp ([`Clock::low_watermark`]).
//!
//! Locks are always taken page latch, then a head's shard, then a transaction's state, so readers,
//! writers, rollbacks, cleanups and collections never wait on each other in a cycle.

use super::blink::{Cell, Change, Descender, Tree};
use super::page::Flags;
use super::pool::Pool;
use super::redo::Op;
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::Mutex;
use crate::Result;
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;

/// Ids at or above this name a transaction; below it, a commit timestamp.
const TX: u64 = 1 << 63;

/// Shards of a tree's heads and of the clock's states: a lock each.
const SHARDS: usize = 64;
const SHARDS_U64: u64 = 64;

/// A commit timestamp.
pub type Ts = u64;

fn shard_of(bytes: &[u8]) -> usize {
    // FNV-1a: cheap, and spreads keys that differ in their last bytes.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    usize::try_from(h % SHARDS_U64).unwrap_or(0)
}

/// A transaction's state, by id, while any version names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Open,
    Committed(Ts),
    Aborted,
}

/// What a version's writer is, resolved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Writer {
    Committed(Ts),
    Open(u64),
    Aborted,
}

/// The commit clock, the transactions in flight and the snapshots open.
pub struct Clock {
    /// Every commit at or below it is published: snapshots begin here.
    visible: AtomicU64,
    /// Held to publish a commit: the last timestamp handed out.
    publish: Mutex<Ts>,
    next_tx: AtomicU64,
    states: Vec<Mutex<FxHashMap<u64, State>>>,
    /// Open snapshots' timestamps, each with how many are open at it.
    open: Mutex<BTreeMap<Ts, u64>>,
}

/// A read's view: the commits at or before `start`, and its own transaction's changes.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub start: Ts,
    own: Option<u64>,
}

/// A transaction: its id, its snapshot, and the rows it changed, each once, in order. It ends by
/// [`Versioned::commit`] or [`Versioned::abort`]; after any error of a change, by `abort`.
pub struct Txn {
    pub id: u64,
    pub start: Ts,
    changed: Vec<(u64, Vec<u8>)>,
    /// Its changes in the order made: its redo record (decision 6).
    ops: Vec<Op>,
}

impl Txn {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            start: self.start,
            own: Some(self.id),
        }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            visible: AtomicU64::new(0),
            publish: Mutex::new(0),
            next_tx: AtomicU64::new(0),
            states: (0..SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
            open: Mutex::new(BTreeMap::new()),
        }
    }

    fn states(&self, id: u64) -> Result<&Mutex<FxHashMap<u64, State>>> {
        let at = usize::try_from(id % SHARDS_U64).map_err(|_| LimboError::IntegerOverflow)?;
        self.states.get(at).ok_or_else(|| {
            LimboError::InternalError("versioned storage: a missing state shard".into())
        })
    }

    fn open_at(&self, start: Ts) -> Result<()> {
        let mut open = self.open.lock();
        let n = open.entry(start).or_insert(0);
        *n = n.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        Ok(())
    }

    /// Ends a snapshot that began at `start` ([`Clock::snapshot`], [`Clock::begin`]).
    pub fn close(&self, start: Ts) {
        let mut open = self.open.lock();
        if let Some(n) = open.get_mut(&start) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                open.remove(&start);
            }
        }
    }

    /// A read-only snapshot of every commit published now; [`Clock::close`] it.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let start = self.visible.load(Ordering::Acquire);
        self.open_at(start)?;
        Ok(Snapshot { start, own: None })
    }

    /// A transaction on a snapshot of every commit published now.
    pub fn begin(&self) -> Result<Txn> {
        let n = self.next_tx.fetch_add(1, Ordering::AcqRel);
        if n >= TX {
            return Err(LimboError::IntegerOverflow);
        }
        let id = TX | n;
        let start = self.visible.load(Ordering::Acquire);
        self.open_at(start)?;
        self.states(id)?.lock().insert(id, State::Open);
        Ok(Txn {
            id,
            start,
            changed: Vec::new(),
            ops: Vec::new(),
        })
    }

    fn resolve(&self, writer: u64) -> Result<Writer> {
        if writer < TX {
            return Ok(Writer::Committed(writer));
        }
        match self.states(writer)?.lock().get(&writer) {
            Some(State::Open) => Ok(Writer::Open(writer)),
            Some(State::Committed(ts)) => Ok(Writer::Committed(*ts)),
            Some(State::Aborted) => Ok(Writer::Aborted),
            None => Err(LimboError::InternalError(format!(
                "versioned storage: a version written by transaction {} the clock no longer knows",
                writer ^ TX
            ))),
        }
    }

    /// Publishes `txn`'s commit: its timestamp, after every commit before it; `log` given the
    /// timestamp first, under the same lock (so commits are logged in timestamp order), and a
    /// failure of it leaves the transaction open, for its caller to roll back.
    fn commit(&self, txn: &Txn, log: impl FnOnce(Ts, &[Op]) -> Result<()>) -> Result<Ts> {
        let mut last = self.publish.lock();
        let ts = last.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
        if ts >= TX {
            return Err(LimboError::IntegerOverflow);
        }
        log(ts, &txn.ops)?;
        self.states(txn.id)?
            .lock()
            .insert(txn.id, State::Committed(ts));
        *last = ts;
        self.visible.store(ts, Ordering::Release);
        Ok(ts)
    }

    /// `f` with the last commit's timestamp, while no commit can be published: a checkpoint's
    /// timestamp and the redo log's switch at it (decisions 6 and 7).
    pub fn at_publish<R>(&self, f: impl FnOnce(Ts) -> R) -> R {
        let last = self.publish.lock();
        f(*last)
    }

    /// After recovery: commits up to `ts` are in, published, and the next is after it.
    pub fn recovered(&self, ts: Ts) {
        let mut last = self.publish.lock();
        *last = (*last).max(ts);
        self.visible.store(*last, Ordering::Release);
    }

    fn set_aborted(&self, txn: &Txn) -> Result<()> {
        self.states(txn.id)?.lock().insert(txn.id, State::Aborted);
        Ok(())
    }

    fn forget(&self, txn: &Txn) -> Result<()> {
        self.states(txn.id)?.lock().remove(&txn.id);
        self.close(txn.start);
        Ok(())
    }

    /// The oldest open snapshot's timestamp, or the last commit's with none open: versions older
    /// than the newest one committed at or before it no snapshot can see.
    pub fn low_watermark(&self) -> Ts {
        let visible = self.visible.load(Ordering::Acquire);
        self.open
            .lock()
            .keys()
            .next()
            .copied()
            .map_or(visible, |oldest| oldest.min(visible))
    }
}

/// A row's versions before its cell's: who wrote the cell's version, and the ones before it.
struct Head {
    writer: u64,
    older: Vec<Older>,
}

/// A version a snapshot may still need: the commit that wrote it, and the row (None: absent).
struct Older {
    written: Ts,
    value: Option<Vec<u8>>,
}

/// A B-link tree whose rows have versions.
pub struct VTree {
    pub id: u64,
    tree: Tree,
    heads: Vec<Mutex<FxHashMap<Vec<u8>, Head>>>,
}

/// The row a cell holds (None: removed, or no cell).
fn row_of(cell: Option<Cell<'_>>) -> Option<Vec<u8>> {
    cell.filter(|c| !c.flags.removed).map(|c| c.value.to_vec())
}

impl VTree {
    pub fn new(id: u64, tree: Tree) -> Self {
        Self {
            id,
            tree,
            heads: (0..SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
        }
    }

    fn heads(&self, key: &[u8]) -> Result<&Mutex<FxHashMap<Vec<u8>, Head>>> {
        self.heads.get(shard_of(key)).ok_or_else(|| {
            LimboError::InternalError("versioned storage: a missing head shard".into())
        })
    }

    /// The version of a row `snap` sees, its cell and head given (the leaf latched).
    fn visible(
        clock: &Clock,
        snap: &Snapshot,
        cell: Option<Cell<'_>>,
        head: Option<&Head>,
    ) -> Result<Option<Vec<u8>>> {
        let Some(c) = cell else {
            return Ok(None);
        };
        if !c.flags.versioned {
            return Ok(row_of(Some(c)));
        }
        // Heads come and go under the leaf's exclusive latch with their cells' flags, and
        // recovery clears the flags it reads back: a versioned cell has a head.
        let Some(head) = head else {
            return Err(LimboError::InternalError(
                "versioned storage: a versioned cell with no versions beside it".into(),
            ));
        };
        let cell_seen = match clock.resolve(head.writer)? {
            Writer::Open(id) => snap.own == Some(id),
            Writer::Committed(ts) => ts <= snap.start,
            Writer::Aborted => false,
        };
        if cell_seen {
            return Ok(row_of(Some(c)));
        }
        Ok(head
            .older
            .iter()
            .find(|o| o.written <= snap.start)
            .and_then(|o| o.value.clone()))
    }

    /// The row under `key` as `snap` sees it.
    pub fn get(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        snap: &Snapshot,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.tree.read_with(pool, d, key, |cell| {
            let heads = self.heads(key)?.lock();
            Self::visible(clock, snap, cell, heads.get(key))
        })
    }

    /// Up to `most` rows from `from` on as `snap` sees them, in key order.
    pub fn scan(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        snap: &Snapshot,
        from: Option<&[u8]>,
        most: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        if most == 0 {
            return Ok(out);
        }
        self.tree.scan_with(pool, d, from, |key, cell| {
            let row = if cell.flags.versioned {
                let heads = self.heads(key)?.lock();
                Self::visible(clock, snap, Some(cell), heads.get(key))?
            } else {
                row_of(Some(cell))
            };
            if let Some(row) = row {
                out.push((key.to_vec(), row));
            }
            Ok(out.len() < most)
        })?;
        Ok(out)
    }

    /// `txn` takes the row under `key` (its leaf latched, `cell` its cell): refused if another
    /// transaction's change to it is open or committed after `txn`'s snapshot; the version `txn`
    /// replaces kept for the snapshots that see it. The row as `txn` saw it before.
    fn take(
        &self,
        clock: &Clock,
        txn: &mut Txn,
        key: &[u8],
        cell: Option<Cell<'_>>,
        heads: &mut FxHashMap<Vec<u8>, Head>,
    ) -> Result<Option<Vec<u8>>> {
        let before = row_of(cell);
        match heads.get_mut(key) {
            Some(head) => match clock.resolve(head.writer)? {
                Writer::Open(id) if id == txn.id => {}
                Writer::Open(_) => return Err(LimboError::WriteWriteConflict),
                Writer::Committed(ts) if ts > txn.start => {
                    return Err(LimboError::WriteWriteConflict)
                }
                Writer::Committed(ts) => {
                    head.older.insert(
                        0,
                        Older {
                            written: ts,
                            value: before.clone(),
                        },
                    );
                    head.writer = txn.id;
                    txn.changed.push((self.id, key.to_vec()));
                }
                Writer::Aborted => {
                    return Err(LimboError::InternalError(
                        "versioned storage: an aborted transaction's version still in place".into(),
                    ))
                }
            },
            None => {
                heads.insert(
                    key.to_vec(),
                    Head {
                        writer: txn.id,
                        older: vec![Older {
                            written: 0,
                            value: before.clone(),
                        }],
                    },
                );
                txn.changed.push((self.id, key.to_vec()));
            }
        }
        Ok(before)
    }

    /// `txn` puts `value` under `key`: the row it saw there before, if any.
    pub fn put(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        txn: &mut Txn,
        key: &[u8],
        value: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        Tree::check_row(key, value)?;
        self.tree.modify(pool, d, key, |cell| {
            let mut heads = self.heads(key)?.lock();
            let before = self.take(clock, txn, key, cell, &mut heads)?;
            let flags = Flags {
                versioned: true,
                removed: false,
            };
            txn.ops.push(Op::Put {
                tree: self.id,
                key: key.to_vec(),
                value: value.to_vec(),
            });
            Ok((Change::Put(value.to_vec(), flags), before))
        })
    }

    /// `txn` removes the row under `key`: the row it saw there, if any (none: nothing removed).
    pub fn remove(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        txn: &mut Txn,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        self.tree.modify(pool, d, key, |cell| {
            let mut heads = self.heads(key)?.lock();
            let seen = Self::visible(clock, &txn.snapshot(), cell, heads.get(key))?;
            if seen.is_none() {
                // Nothing to remove, unless the cell is another's change, which conflicts.
                if let Some(head) = heads.get(key) {
                    match clock.resolve(head.writer)? {
                        Writer::Open(id) if id != txn.id => {
                            return Err(LimboError::WriteWriteConflict)
                        }
                        Writer::Committed(ts) if ts > txn.start => {
                            return Err(LimboError::WriteWriteConflict)
                        }
                        _ => {}
                    }
                }
                return Ok((Change::Keep, None));
            }
            let before = self.take(clock, txn, key, cell, &mut heads)?;
            let flags = Flags {
                versioned: true,
                removed: true,
            };
            txn.ops.push(Op::Remove {
                tree: self.id,
                key: key.to_vec(),
            });
            Ok((Change::Mark(flags), before))
        })
    }

    /// `op` made directly, with no versions: recovery's corrections and replay (decision 8).
    pub fn apply_plain(&self, pool: &Pool, d: &mut Descender, op: &Op) -> Result<()> {
        match op {
            Op::Put { key, value, .. } => {
                Tree::check_row(key, value)?;
                self.tree.modify(pool, d, key, |_| {
                    Ok((Change::Put(value.clone(), Flags::default()), ()))
                })
            }
            Op::Remove { key, .. } => self.tree.modify(pool, d, key, |_| Ok((Change::Remove, ()))),
        }
    }

    /// The rows whose cell isn't their version as of `ts` (a change of a transaction still open,
    /// or committed after `ts`), each with that version: a checkpoint's list (decision 7). Asked
    /// while no page can change.
    fn corrections(&self, clock: &Clock, ts: Ts, out: &mut Vec<Op>) -> Result<()> {
        for shard in &self.heads {
            let heads = shard.lock();
            for (key, head) in heads.iter() {
                if matches!(clock.resolve(head.writer)?, Writer::Committed(w) if w <= ts) {
                    continue;
                }
                let then = head
                    .older
                    .iter()
                    .find(|o| o.written <= ts)
                    .and_then(|o| o.value.clone());
                out.push(match then {
                    Some(value) => Op::Put {
                        tree: self.id,
                        key: key.clone(),
                        value,
                    },
                    None => Op::Remove {
                        tree: self.id,
                        key: key.clone(),
                    },
                });
            }
        }
        Ok(())
    }

    /// The tree's root page.
    pub fn root(&self) -> u64 {
        self.tree.root()
    }

    /// Puts back the version `txn` replaced under `key` (its rollback).
    fn restore(&self, pool: &Pool, d: &mut Descender, txn: &Txn, key: &[u8]) -> Result<()> {
        self.tree.modify(pool, d, key, |cell| {
            let mut heads = self.heads(key)?.lock();
            let Some(head) = heads.get_mut(key) else {
                return Ok((Change::Keep, ()));
            };
            if head.writer != txn.id || head.older.is_empty() {
                return Ok((Change::Keep, ()));
            }
            let prior = head.older.remove(0);
            head.writer = prior.written;
            let change = if head.older.is_empty() && prior.written == 0 {
                // No snapshot needs anything older: the row is as it was before any.
                heads.remove(key);
                match (prior.value, cell) {
                    (Some(v), _) => Change::Put(v, Flags::default()),
                    (None, Some(_)) => Change::Remove,
                    (None, None) => Change::Keep,
                }
            } else {
                match prior.value {
                    Some(v) => Change::Put(
                        v,
                        Flags {
                            versioned: true,
                            removed: false,
                        },
                    ),
                    None => Change::Mark(Flags {
                        versioned: true,
                        removed: true,
                    }),
                }
            };
            Ok((change, ()))
        })
    }
}

/// A database's versioned trees, their pool and their clock.
pub struct Versioned {
    pub pool: Pool,
    pub clock: Clock,
    trees: Vec<VTree>,
}

impl Versioned {
    /// `trees` empty trees in a pool of `frames` pages.
    pub fn new(
        frames: usize,
        trees: usize,
        cmp: impl Fn() -> super::blink::Comparator,
    ) -> Result<Self> {
        let pool = Pool::new(frames);
        let mut made = Vec::with_capacity(trees);
        for id in 0..trees {
            let id = u64::try_from(id).map_err(|_| LimboError::IntegerOverflow)?;
            made.push(VTree::new(id, Tree::create(&pool, cmp())?));
        }
        Ok(Self {
            pool,
            clock: Clock::new(),
            trees: made,
        })
    }

    /// The trees whose roots are `roots`, in `pool` (recovery).
    pub fn open(
        pool: Pool,
        roots: &[u64],
        cmp: impl Fn() -> super::blink::Comparator,
    ) -> Result<Self> {
        let mut trees = Vec::with_capacity(roots.len());
        for (id, &root) in roots.iter().enumerate() {
            let id = u64::try_from(id).map_err(|_| LimboError::IntegerOverflow)?;
            trees.push(VTree::new(id, Tree::open(root, cmp())));
        }
        Ok(Self {
            pool,
            clock: Clock::new(),
            trees,
        })
    }

    /// Each tree's root page, by tree.
    pub fn roots(&self) -> Vec<u64> {
        self.trees.iter().map(VTree::root).collect()
    }

    /// Clears the versions on every tree's cells, dropping removed rows' (recovery, which starts
    /// with no heads: each cell is every snapshot's).
    pub fn clear_versions(&self, d: &mut Descender) -> Result<()> {
        for t in &self.trees {
            t.tree.clear_versions(&self.pool, d)?;
        }
        Ok(())
    }

    /// Every tree's rows whose cell isn't their version as of `ts`, with that version (decision 7).
    pub fn corrections(&self, ts: Ts) -> Result<Vec<Op>> {
        let mut out = Vec::new();
        for t in &self.trees {
            t.corrections(&self.clock, ts, &mut out)?;
        }
        Ok(out)
    }

    pub fn tree(&self, id: u64) -> Result<&VTree> {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.trees.get(i))
            .ok_or_else(|| LimboError::InternalError(format!("versioned storage: no tree {id}")))
    }

    /// Commits `txn`: its changes visible to every snapshot from its timestamp on, which it
    /// returns; its versions then name the timestamp, not the transaction. A commit that fails is
    /// rolled back.
    pub fn commit(&self, txn: Txn, d: &mut Descender) -> Result<Ts> {
        self.commit_logged(txn, d, |_, _| Ok(()))
    }

    /// [`Versioned::commit`], `log` given the timestamp and the changes under the publish lock
    /// (the redo log's append, in timestamp order); a failure of `log` rolls `txn` back.
    pub fn commit_logged(
        &self,
        txn: Txn,
        d: &mut Descender,
        log: impl FnOnce(Ts, &[Op]) -> Result<()>,
    ) -> Result<Ts> {
        let ts = match self.clock.commit(&txn, log) {
            Ok(ts) => ts,
            Err(e) => {
                self.abort(txn, d)?;
                return Err(e);
            }
        };
        for (tree, key) in &txn.changed {
            let t = self.tree(*tree)?;
            let mut heads = t.heads(key)?.lock();
            if let Some(head) = heads.get_mut(key) {
                if head.writer == txn.id {
                    head.writer = ts;
                }
            }
        }
        self.clock.forget(&txn)?;
        Ok(ts)
    }

    /// Rolls `txn` back: each row it changed as it was before, last change first.
    pub fn abort(&self, txn: Txn, d: &mut Descender) -> Result<()> {
        for (tree, key) in txn.changed.iter().rev() {
            self.tree(*tree)?.restore(&self.pool, d, &txn, key)?;
        }
        self.clock.set_aborted(&txn)?;
        self.clock.forget(&txn)
    }

    /// Drops the versions no open snapshot can see: rows whose cell's version every snapshot sees
    /// lose their heads (and their cells, if removed); others keep only the older versions still
    /// visible to some snapshot. How many rows lost their heads.
    pub fn collect(&self, d: &mut Descender) -> Result<usize> {
        let lwm = self.clock.low_watermark();
        let mut collected = 0usize;
        for t in &self.trees {
            let mut ripe = Vec::new();
            for shard in &t.heads {
                let mut heads = shard.lock();
                for (key, head) in heads.iter_mut() {
                    match self.clock.resolve(head.writer)? {
                        Writer::Committed(ts) if ts <= lwm => ripe.push(key.clone()),
                        _ => {
                            // The newest older version committed at or before the watermark is
                            // the last any snapshot needs.
                            if let Some(keep) = head.older.iter().position(|o| o.written <= lwm) {
                                head.older.truncate(keep.saturating_add(1));
                            }
                        }
                    }
                }
            }
            for key in ripe {
                let gone = t.tree.modify(&self.pool, d, &key, |cell| {
                    let mut heads = t.heads(&key)?.lock();
                    let ripe = match heads.get(&key) {
                        Some(head) => {
                            matches!(self.clock.resolve(head.writer)?, Writer::Committed(ts) if ts <= lwm)
                        }
                        None => false,
                    };
                    if !ripe {
                        return Ok((Change::Keep, false));
                    }
                    heads.remove(&key);
                    let change = match cell {
                        Some(c) if c.flags.removed => Change::Remove,
                        Some(_) => Change::Mark(Flags::default()),
                        None => Change::Keep,
                    };
                    Ok((change, true))
                })?;
                if gone {
                    collected = collected.saturating_add(1);
                }
            }
        }
        Ok(collected)
    }

    /// Rows with versions kept beside the pages, in every tree.
    pub fn versioned_rows(&self) -> usize {
        self.trees
            .iter()
            .map(|t| t.heads.iter().map(|s| s.lock().len()).sum::<usize>())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Arc;

    fn db() -> Versioned {
        Versioned::new(20_000, 1, || Box::new(|a: &[u8], b: &[u8]| a.cmp(b))).unwrap()
    }

    /// Do: a transaction changes a row and adds one while another, begun before, reads; then it
    /// commits. Expect: the earlier reader sees the rows as they were before and after the commit;
    /// a reader begun after sees the changes; the writer sees its own changes before it commits.
    #[test]
    fn a_snapshot_sees_the_commits_before_it_and_its_own_changes() {
        let v = db();
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut setup = v.clock.begin().unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut setup, b"a", b"1")
            .unwrap();
        v.commit(setup, &mut d).unwrap();

        let reader = v.clock.snapshot().unwrap();
        let mut w = v.clock.begin().unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, b"a", b"2")
            .unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, b"b", b"9")
            .unwrap();
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &w.snapshot(), b"a")
                .unwrap(),
            Some(b"2".to_vec())
        );
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &reader, b"a").unwrap(),
            Some(b"1".to_vec())
        );
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &reader, b"b").unwrap(),
            None
        );
        v.commit(w, &mut d).unwrap();
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &reader, b"a").unwrap(),
            Some(b"1".to_vec())
        );
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &reader, b"b").unwrap(),
            None
        );
        let later = v.clock.snapshot().unwrap();
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &later, b"a").unwrap(),
            Some(b"2".to_vec())
        );
        assert_eq!(
            t.scan(&v.pool, &mut d, &v.clock, &later, None, 10).unwrap(),
            vec![
                (b"a".to_vec(), b"2".to_vec()),
                (b"b".to_vec(), b"9".to_vec())
            ]
        );
        assert_eq!(
            t.scan(&v.pool, &mut d, &v.clock, &reader, None, 10)
                .unwrap(),
            vec![(b"a".to_vec(), b"1".to_vec())]
        );
        v.clock.close(reader.start);
        v.clock.close(later.start);
    }

    /// Do: two open transactions change the same row; then one commits and a transaction begun
    /// before that commit changes the row. Expect: the second writer refused each time
    /// (`WriteWriteConflict`); a transaction begun after the commit may change it.
    #[test]
    fn the_first_writer_wins() {
        let v = db();
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut a = v.clock.begin().unwrap();
        let mut b = v.clock.begin().unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut a, b"k", b"a")
            .unwrap();
        assert!(matches!(
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"k", b"b"),
            Err(LimboError::WriteWriteConflict)
        ));
        assert!(matches!(
            t.remove(&v.pool, &mut d, &v.clock, &mut b, b"k"),
            Err(LimboError::WriteWriteConflict)
        ));
        v.commit(a, &mut d).unwrap();
        assert!(matches!(
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"k", b"b"),
            Err(LimboError::WriteWriteConflict)
        ));
        v.abort(b, &mut d).unwrap();
        let mut c = v.clock.begin().unwrap();
        assert_eq!(
            t.put(&v.pool, &mut d, &v.clock, &mut c, b"k", b"c")
                .unwrap(),
            Some(b"a".to_vec())
        );
        v.commit(c, &mut d).unwrap();
    }

    /// Do: a transaction changes, adds and removes rows, then rolls back. Expect: every row as it
    /// was, to readers before and after, and no versions left once collected.
    #[test]
    fn a_rollback_puts_every_row_back() {
        let v = db();
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut setup = v.clock.begin().unwrap();
        for k in [b"a", b"b", b"c"] {
            t.put(&v.pool, &mut d, &v.clock, &mut setup, k, b"0")
                .unwrap();
        }
        v.commit(setup, &mut d).unwrap();
        let mut w = v.clock.begin().unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, b"a", b"1")
            .unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, b"a", b"2")
            .unwrap();
        t.remove(&v.pool, &mut d, &v.clock, &mut w, b"b").unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, b"z", b"1")
            .unwrap();
        v.abort(w, &mut d).unwrap();
        let snap = v.clock.snapshot().unwrap();
        let want = vec![
            (b"a".to_vec(), b"0".to_vec()),
            (b"b".to_vec(), b"0".to_vec()),
            (b"c".to_vec(), b"0".to_vec()),
        ];
        assert_eq!(
            t.scan(&v.pool, &mut d, &v.clock, &snap, None, 10).unwrap(),
            want
        );
        v.clock.close(snap.start);
        v.collect(&mut d).unwrap();
        assert_eq!(v.versioned_rows(), 0);
        let snap = v.clock.snapshot().unwrap();
        assert_eq!(
            t.scan(&v.pool, &mut d, &v.clock, &snap, None, 10).unwrap(),
            want
        );
        v.clock.close(snap.start);
    }

    /// Do: change and remove rows many times with a reader holding an early snapshot, then collect,
    /// close the reader and collect again. Expect: while the reader is open it still sees its
    /// rows, and versions the oldest snapshot needs stay; after, no versions remain, removed rows'
    /// cells are gone, and every row reads as last committed.
    #[test]
    fn versions_no_snapshot_sees_are_collected() {
        let v = db();
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut setup = v.clock.begin().unwrap();
        for k in 0..100u8 {
            t.put(&v.pool, &mut d, &v.clock, &mut setup, &[k], &[0])
                .unwrap();
        }
        v.commit(setup, &mut d).unwrap();
        let early = v.clock.snapshot().unwrap();
        for round in 1..5u8 {
            let mut w = v.clock.begin().unwrap();
            for k in 0..100u8 {
                if k.is_multiple_of(10) {
                    t.remove(&v.pool, &mut d, &v.clock, &mut w, &[k]).unwrap();
                } else {
                    t.put(&v.pool, &mut d, &v.clock, &mut w, &[k], &[round])
                        .unwrap();
                }
            }
            v.commit(w, &mut d).unwrap();
        }
        v.collect(&mut d).unwrap();
        assert_eq!(v.versioned_rows(), 100, "the early reader's versions stay");
        assert_eq!(
            t.scan(&v.pool, &mut d, &v.clock, &early, None, 1000)
                .unwrap()
                .len(),
            100
        );
        assert_eq!(
            t.get(&v.pool, &mut d, &v.clock, &early, &[3]).unwrap(),
            Some(vec![0])
        );
        v.clock.close(early.start);
        v.collect(&mut d).unwrap();
        assert_eq!(v.versioned_rows(), 0);
        let now = v.clock.snapshot().unwrap();
        let rows = t.scan(&v.pool, &mut d, &v.clock, &now, None, 1000).unwrap();
        assert_eq!(rows.len(), 90);
        assert!(rows.iter().all(|(_, val)| val == &vec![4]));
        let cells = t.tree.scan(&v.pool, &mut d, None, 1000).unwrap();
        assert_eq!(cells.len(), 90, "removed rows' cells are gone");
        v.clock.close(now.start);
    }

    /// Do: 16 threads move amounts between 50 accounts in transactions (read two, write both),
    /// retrying when refused, while 4 threads sum every account in a snapshot; collect at the end.
    /// Expect: every reader's sum is the total (a snapshot is consistent), and after all
    /// transfers the total and every account's existence hold.
    #[test]
    fn concurrent_transfers_keep_the_total_in_every_snapshot() {
        let v = Arc::new(db());
        let accounts = 50u8;
        let start = 1_000u64;
        {
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut setup = v.clock.begin().unwrap();
            for a in 0..accounts {
                t.put(
                    &v.pool,
                    &mut d,
                    &v.clock,
                    &mut setup,
                    &[a],
                    &start.to_be_bytes(),
                )
                .unwrap();
            }
            v.commit(setup, &mut d).unwrap();
        }
        let total = start * u64::from(accounts);
        let amount = |bytes: &[u8]| u64::from_be_bytes(bytes.try_into().unwrap());
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let (v, done) = (v.clone(), done.clone());
                std::thread::spawn(move || {
                    let t = v.tree(0).unwrap();
                    let mut d = Descender::new();
                    let mut sums = 0;
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        let snap = v.clock.snapshot().unwrap();
                        let rows = t
                            .scan(&v.pool, &mut d, &v.clock, &snap, None, 1000)
                            .unwrap();
                        v.clock.close(snap.start);
                        assert_eq!(rows.len(), usize::from(accounts));
                        assert_eq!(rows.iter().map(|(_, b)| amount(b)).sum::<u64>(), total);
                        sums += 1;
                    }
                    sums
                })
            })
            .collect();
        let writers: Vec<_> = (0..16u64)
            .map(|w| {
                let v = v.clone();
                std::thread::spawn(move || {
                    let t = v.tree(0).unwrap();
                    let mut d = Descender::new();
                    let mut seed = w + 1;
                    let mut moved = 0;
                    while moved < 300 {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        let from = u8::try_from((seed >> 33) % u64::from(accounts)).unwrap();
                        let to = u8::try_from((seed >> 40) % u64::from(accounts)).unwrap();
                        if from == to {
                            continue;
                        }
                        let mut txn = v.clock.begin().unwrap();
                        let snap = txn.snapshot();
                        let attempt = (|| -> Result<()> {
                            let a =
                                amount(&t.get(&v.pool, &mut d, &v.clock, &snap, &[from])?.unwrap());
                            let b =
                                amount(&t.get(&v.pool, &mut d, &v.clock, &snap, &[to])?.unwrap());
                            let x = a.min(7);
                            t.put(
                                &v.pool,
                                &mut d,
                                &v.clock,
                                &mut txn,
                                &[from],
                                &(a - x).to_be_bytes(),
                            )?;
                            t.put(
                                &v.pool,
                                &mut d,
                                &v.clock,
                                &mut txn,
                                &[to],
                                &(b + x).to_be_bytes(),
                            )?;
                            Ok(())
                        })();
                        match attempt {
                            Ok(()) => {
                                v.commit(txn, &mut d).unwrap();
                                moved += 1;
                            }
                            Err(LimboError::WriteWriteConflict) => v.abort(txn, &mut d).unwrap(),
                            Err(e) => panic!("{e}"),
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let sums: usize = readers.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(sums > 0, "the readers summed");
        let mut d = Descender::new();
        v.collect(&mut d).unwrap();
        assert_eq!(v.versioned_rows(), 0);
        let t = v.tree(0).unwrap();
        let snap = v.clock.snapshot().unwrap();
        let rows = t
            .scan(&v.pool, &mut d, &v.clock, &snap, None, 1000)
            .unwrap();
        assert_eq!(rows.iter().map(|(_, b)| amount(b)).sum::<u64>(), total);
        v.clock.close(snap.start);
    }
}
