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
//! A serializable transaction reads through [`VTree::read`] and [`VTree::read_range`], which keep
//! what it read; its commit is validated by the database's [`Validation`] under the publish lock,
//! and refused with `SerializationFailure` where serializability asks (decision 5, and the
//! prototypes G8 step 1 measures against each other).
//!
//! Memory is bounded (decision 10, G8 step 2): collection keeps, of a row's older versions, only
//! those some open snapshot sees (Steam's pruning, so a long reader holds one version a row, not
//! one an update); and every version, transaction record and commit kept for validation is charged
//! to one [`Budget`], which refuses past it with a typed `MemoryBudget`. Commits are kept for
//! validation only while a transaction begun before them is open: a read-only snapshot never
//! validates. A transaction's reads past the budget are escalated to its whole tree (PostgreSQL's
//! lock escalation), a coarser record that refuses more but never less.
//!
//! A transaction rolls back to a [`Savepoint`] (SQLite's `ROLLBACK TO`; G7 step 2, `NS.2`) from
//! the same versions: a row it first changed after the savepoint gets back the version it
//! replaced, as its abort would put it back; a row it changed before the savepoint and again after
//! gets back, in place, the value its last change before the savepoint left (the transaction's own
//! changes are its redo record, so that value is in it). Its changes after the savepoint leave its
//! redo record and its rows; what it read after it stays kept for its validation, as PostgreSQL
//! keeps a subtransaction's predicate locks after its rollback (a superset refuses more, never less).
//!
//! Locks are always taken the publish lock, then the recent commits', then a page latch, then a
//! head's shard, then a transaction's state, then the open snapshots', then the budget's, so
//! readers, writers, rollbacks, cleanups, collections and validations never wait on each other in
//! a cycle.

use super::blink::{Cell, Change, Descender, Tree};
use super::memory::Source;
use super::page::{self, Flags};
use super::pool::Pool;
use super::redo::Op;
use crate::error::LimboError;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sync::{Arc, Mutex, RwLock};
use crate::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering as Order;
use std::collections::{BTreeMap, VecDeque};
use std::mem::size_of;

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
    open: Mutex<Open>,
    /// What versions, transactions' records and kept commits may hold.
    budget: Budget,
}

/// Open snapshots' timestamps, each with how many are open at it: all of them, and those of
/// transactions (the only ones a commit is validated for).
#[derive(Default)]
struct Open {
    all: BTreeMap<Ts, u64>,
    txns: BTreeMap<Ts, u64>,
}

fn count_in(map: &mut BTreeMap<Ts, u64>, at: Ts) -> Result<()> {
    let n = map.entry(at).or_insert(0);
    *n = n.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
    Ok(())
}

fn count_out(map: &mut BTreeMap<Ts, u64>, at: Ts) {
    if let Some(n) = map.get_mut(&at) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(&at);
        }
    }
}

/// The bytes the versioned storage may hold beside its pages (decision 10): row versions and
/// their heads, transactions' read and write records, and the commits kept for validation. A
/// reservation past `limit`, or one its memory source refuses, is refused, typed, and nothing is
/// taken.
pub struct Budget {
    limit: u64,
    held: Mutex<Held>,
    /// Where the bytes come from (G7 step 2, `AC-NS.2.1`): the process's budget, shared; None:
    /// within `limit` alone.
    source: Option<Arc<dyn Source>>,
}

/// What a budget holds, and what it has taken from its source for it.
#[derive(Default)]
struct Held {
    held: u64,
    taken: u64,
}

impl Budget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            held: Mutex::new(Held::default()),
            source: None,
        }
    }

    /// A budget whose bytes come from `source`, as much as it grants.
    pub fn shared(source: Arc<dyn Source>) -> Self {
        Self {
            limit: u64::MAX,
            held: Mutex::new(Held::default()),
            source: Some(source),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// The bytes held now.
    pub fn held(&self) -> u64 {
        self.held.lock().held
    }

    /// The bytes taken from the source now (0 without one).
    pub fn taken(&self) -> u64 {
        self.held.lock().taken
    }

    /// `n` bytes more for `what`. With a source, what is taken from it grows as a dynamic table
    /// does (Cormen et al., *Introduction to Algorithms*, 3rd ed., section 17.4): doubled when the
    /// bytes held would pass it, so a source is asked once per doubling, not once per row; just
    /// what is needed when the doubling is refused.
    fn reserve(&self, what: &'static str, n: usize) -> Result<u64> {
        let asked = u64::try_from(n).map_err(|_| LimboError::IntegerOverflow)?;
        let mut held = self.held.lock();
        let refused = |held: u64| LimboError::MemoryBudget {
            what,
            asked,
            held,
            limit: self.limit,
        };
        let after = held
            .held
            .checked_add(asked)
            .filter(|after| *after <= self.limit)
            .ok_or_else(|| refused(held.held))?;
        if let Some(source) = &self.source {
            if after > held.taken {
                let doubled = after.max(held.taken.saturating_mul(2));
                let more = doubled.saturating_sub(held.taken);
                let need = after.saturating_sub(held.taken);
                let grown = match source.take(what, more) {
                    Ok(()) => more,
                    Err(_) => {
                        source.take(what, need)?;
                        need
                    }
                };
                held.taken = held.taken.saturating_add(grown);
            }
        }
        held.held = after;
        Ok(asked)
    }

    /// `n` bytes given back. With a source, what is taken from it is halved down to twice what is
    /// held once the bytes held fall under a quarter of it (the dynamic table's contraction, as
    /// above): a reservation that went up and down by one row isn't asked of the source each time.
    fn release(&self, n: u64) {
        let mut held = self.held.lock();
        held.held = held.held.saturating_sub(n);
        if let Some(source) = &self.source {
            if held.held.saturating_mul(4) < held.taken {
                let keep = held.held.saturating_mul(2);
                source.give(held.taken.saturating_sub(keep));
                held.taken = keep;
            }
        }
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        if let Some(source) = &self.source {
            source.give(self.held.lock().taken);
        }
    }
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
    /// What it read through [`VTree::read`] and [`VTree::read_range`]: its commit's validation.
    reads: Reads,
    /// The bytes of `changed` and `ops` charged to the budget, and of `reads`.
    charged: u64,
    read_bytes: u64,
}

/// How a database validates a commit (decision 5). Each one is the floor, snapshot isolation with
/// the first writer winning, and what it adds; G8 step 1 measures the three that make it
/// serializable against each other on the same commit path, and keeps one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Validation {
    /// Nothing: snapshot isolation, which allows write skew. The Elle checks' control.
    Snapshot,
    /// Precision locking as HyPer adapts it (Neumann, Mühlbauer, Kemper, SIGMOD 2015): a
    /// transaction that wrote checks the rows and spans it read against the rows written by the
    /// commits since its snapshot; any match refuses it. One that only read commits as of its
    /// snapshot, unchecked.
    #[default]
    Precision,
    /// Serializable snapshot isolation as PostgreSQL makes it (Cahill, Röhm, Fekete, SIGMOD 2008;
    /// Ports, Grittner, PVLDB 5(12), 2012): read-write antidependencies among concurrent
    /// transactions, refusing one only where it would complete a dangerous structure
    /// `in -> pivot -> out` whose `out` committed first. Every commit's reads are kept while a
    /// concurrent transaction is open.
    Ssi,
    /// Optimistic validation as Silo makes it (Tu, Zheng, Kohler, Liskov, Madden, SOSP 2013): each
    /// row read is checked in its tree, by the version its head names, for a commit since the
    /// snapshot; each span is read again for one (Silo's node set, at row grain: an open writer's
    /// in-place change moves a leaf's version, which would refuse its own readers). Nothing is
    /// kept between commits.
    Occ,
}

/// What a transaction read: rows by key, and spans of keys, each in its tree.
#[derive(Default)]
struct Reads {
    points: Vec<(u64, Vec<u8>)>,
    spans: Vec<Span>,
}

/// The keys of a tree from `from` to `to`, both included (none: from the least, to the greatest).
struct Span {
    tree: u64,
    from: Option<Vec<u8>>,
    to: Option<Vec<u8>>,
}

impl Reads {
    /// Whether the whole of tree `tree` is read: no narrower read of it needs keeping.
    fn whole(&self, tree: u64) -> bool {
        self.spans
            .iter()
            .any(|s| s.tree == tree && s.from.is_none() && s.to.is_none())
    }

    /// Keeps a read the budget lets it keep; past it, every read of `tree` becomes the whole tree
    /// (PostgreSQL's escalation of predicate locks to the relation: a superset of what was read,
    /// so a validation refuses at least what it would have). `bytes` charged to `budget`.
    fn keep(
        &mut self,
        budget: &Budget,
        bytes: &mut u64,
        tree: u64,
        size: usize,
        add: impl FnOnce(&mut Self),
    ) -> Result<()> {
        if self.whole(tree) {
            return Ok(());
        }
        match budget.reserve("a transaction's reads", size) {
            Ok(n) => {
                add(self);
                *bytes = bytes.saturating_add(n);
                Ok(())
            }
            Err(LimboError::MemoryBudget { .. }) => {
                let mut freed = 0u64;
                self.points.retain(|(t, k)| {
                    if *t == tree {
                        freed = freed.saturating_add(point_bytes(k));
                    }
                    *t != tree
                });
                self.spans.retain(|s| {
                    if s.tree == tree {
                        freed = freed.saturating_add(s.bytes());
                    }
                    s.tree != tree
                });
                budget.release(freed);
                *bytes = bytes.saturating_sub(freed);
                let whole = Span {
                    tree,
                    from: None,
                    to: None,
                };
                let n = budget.reserve("a transaction's reads", size_of::<Span>())?;
                self.spans.push(whole);
                *bytes = bytes.saturating_add(n);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn is_empty(&self) -> bool {
        self.points.is_empty() && self.spans.is_empty()
    }

    /// Whether these reads saw `key` of tree `tree`: a row read under it, or a span holding it.
    fn covers(&self, v: &Versioned, tree: u64, key: &[u8]) -> Result<bool> {
        let mut points = self.points.iter().filter(|(t, _)| *t == tree).peekable();
        let mut spans = self.spans.iter().filter(|s| s.tree == tree).peekable();
        if points.peek().is_none() && spans.peek().is_none() {
            return Ok(false);
        }
        let t = v.tree(tree)?;
        let cmp = t.tree.compare();
        if points.any(|(_, k)| cmp(k, key) == Order::Equal) {
            return Ok(true);
        }
        Ok(spans.any(|s| {
            s.from
                .as_deref()
                .is_none_or(|f| cmp(f, key) != Order::Greater)
                && s.to
                    .as_deref()
                    .is_none_or(|t| cmp(key, t) != Order::Greater)
        }))
    }
}

/// The bytes a row read keeps (its key and its entry).
fn point_bytes(key: &[u8]) -> u64 {
    u64::try_from(size_of::<(u64, Vec<u8>)>().saturating_add(key.len())).unwrap_or(u64::MAX)
}

impl Span {
    /// The bytes a span read keeps.
    fn bytes(&self) -> u64 {
        let ends = self
            .from
            .as_ref()
            .map_or(0, Vec::len)
            .saturating_add(self.to.as_ref().map_or(0, Vec::len));
        u64::try_from(size_of::<Span>().saturating_add(ends)).unwrap_or(u64::MAX)
    }
}

/// A commit precision locking or SSI keeps while a transaction concurrent with it is open: its
/// timestamp, the rows it wrote, and for SSI what it read and the earliest commit it read past
/// that came before its own (its `out` edge to a transaction that committed first).
struct Committed {
    ts: Ts,
    writes: Vec<(u64, Vec<u8>)>,
    reads: Reads,
    out_first: Option<Ts>,
    /// Its bytes charged to the budget.
    bytes: u64,
}

impl Txn {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            start: self.start,
            own: Some(self.id),
        }
    }

    /// A savepoint here: [`Versioned::rollback_to`] puts the transaction's rows back as they are
    /// now.
    pub fn savepoint(&self) -> Savepoint {
        Savepoint {
            txn: self.id,
            ops: self.ops.len(),
            changed: self.changed.len(),
        }
    }
}

/// A place in a transaction to roll back to: how many changes it had made, and how many rows it
/// had changed, when it was taken. Good until the transaction rolls back to one taken before it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Savepoint {
    txn: u64,
    ops: usize,
    changed: usize,
}

fn no_tree(id: u64) -> LimboError {
    LimboError::InternalError(format!("versioned storage: no tree {id}"))
}

/// A row: its tree and its key.
type Row<'a> = (u64, &'a [u8]);

/// The bytes a change's record took from the budget ([`VTree::put`], [`VTree::remove`]).
fn op_bytes(op: &Op) -> u64 {
    let n = match op {
        Op::Put { key, value, .. } => size_of::<Op>()
            .saturating_add(key.len())
            .saturating_add(value.len()),
        Op::Remove { key, .. } => size_of::<Op>().saturating_add(key.len()),
        Op::Made { .. } | Op::Dropped { .. } => size_of::<Op>(),
        Op::Ordered { order, .. } => size_of::<Op>().saturating_add(order.len()),
    };
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// The bytes a transaction's note of a row it changed took from the budget ([`VTree::take`]).
fn noted_bytes(key: &[u8]) -> u64 {
    u64::try_from(size_of::<(u64, Vec<u8>)>().saturating_add(key.len())).unwrap_or(u64::MAX)
}

impl Clock {
    /// A clock whose versions and records may hold `budget` bytes.
    pub fn new(budget: u64) -> Self {
        Self::within(Budget::new(budget))
    }

    /// A clock whose versions and records are held to `budget`.
    pub fn within(budget: Budget) -> Self {
        Self {
            visible: AtomicU64::new(0),
            publish: Mutex::new(0),
            next_tx: AtomicU64::new(0),
            states: (0..SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
            open: Mutex::new(Open::default()),
            budget,
        }
    }

    /// What versions, transactions' records and kept commits hold, and may.
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    fn states(&self, id: u64) -> Result<&Mutex<FxHashMap<u64, State>>> {
        let at = usize::try_from(id % SHARDS_U64).map_err(|_| LimboError::IntegerOverflow)?;
        self.states.get(at).ok_or_else(|| {
            LimboError::InternalError("versioned storage: a missing state shard".into())
        })
    }

    /// A snapshot of every commit published now, open: read under the open snapshots' lock, so
    /// a low watermark taken at once either counts it or is at most its timestamp (read before
    /// the lock, a commit and a collection between the read and the count could drop versions it
    /// sees, and the commits a validation needs for it).
    /// A transaction's is counted among them too, in the same lock.
    fn open_now(&self, txn: bool) -> Result<Ts> {
        let mut open = self.open.lock();
        let start = self.visible.load(Ordering::Acquire);
        count_in(&mut open.all, start)?;
        if txn {
            if let Err(e) = count_in(&mut open.txns, start) {
                count_out(&mut open.all, start);
                return Err(e);
            }
        }
        Ok(start)
    }

    /// Ends a read-only snapshot that began at `start` ([`Clock::snapshot`]).
    pub fn close(&self, start: Ts) {
        count_out(&mut self.open.lock().all, start);
    }

    /// A read-only snapshot of every commit published now; [`Clock::close`] it.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let start = self.open_now(false)?;
        Ok(Snapshot { start, own: None })
    }

    /// A transaction on a snapshot of every commit published now.
    pub fn begin(&self) -> Result<Txn> {
        let n = self.next_tx.fetch_add(1, Ordering::AcqRel);
        if n >= TX {
            return Err(LimboError::IntegerOverflow);
        }
        let id = TX | n;
        let start = self.open_now(true)?;
        self.states(id)?.lock().insert(id, State::Open);
        Ok(Txn {
            id,
            start,
            changed: Vec::new(),
            ops: Vec::new(),
            reads: Reads::default(),
            charged: 0,
            read_bytes: 0,
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

    /// Ends `txn`: its state, its snapshot, and its records' charge (its reads' already moved to
    /// a kept commit when it committed).
    fn forget(&self, txn: &Txn) -> Result<()> {
        self.states(txn.id)?.lock().remove(&txn.id);
        {
            let mut open = self.open.lock();
            count_out(&mut open.all, txn.start);
            count_out(&mut open.txns, txn.start);
        }
        self.budget
            .release(txn.charged.saturating_add(txn.read_bytes));
        Ok(())
    }

    /// The oldest open snapshot's timestamp, or the last commit's with none open: versions older
    /// than the newest one committed at or before it no snapshot can see.
    pub fn low_watermark(&self) -> Ts {
        let open = self.open.lock();
        let visible = self.visible.load(Ordering::Acquire);
        open.all
            .keys()
            .next()
            .copied()
            .map_or(visible, |oldest| oldest.min(visible))
    }

    /// The oldest open transaction's start, or the last commit's with none open: no commit at or
    /// before it is concurrent with a transaction that may still commit.
    fn oldest_txn(&self) -> Ts {
        let open = self.open.lock();
        let visible = self.visible.load(Ordering::Acquire);
        open.txns
            .keys()
            .next()
            .copied()
            .map_or(visible, |oldest| oldest.min(visible))
    }

    /// Every open snapshot's start, oldest first, and the last commit's, read at once: a snapshot
    /// opened after begins at or after that commit (collection's pruning).
    fn open_starts(&self) -> (Vec<Ts>, Ts) {
        let open = self.open.lock();
        let visible = self.visible.load(Ordering::Acquire);
        (open.all.keys().copied().collect(), visible)
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

impl Older {
    fn bytes(&self) -> usize {
        size_of::<Older>().saturating_add(self.value.as_ref().map_or(0, Vec::len))
    }
}

/// The bytes a head under a key of `key_len` bytes holds, with no older versions: its map entry
/// and its key.
fn head_entry_bytes(key_len: usize) -> usize {
    size_of::<(Vec<u8>, Head)>().saturating_add(key_len)
}

impl Head {
    /// The bytes it holds under a key of `key_len` bytes, its versions with it.
    fn bytes(&self, key_len: usize) -> u64 {
        let older = self
            .older
            .iter()
            .fold(0usize, |n, o| n.saturating_add(o.bytes()));
        u64::try_from(head_entry_bytes(key_len).saturating_add(older)).unwrap_or(u64::MAX)
    }

    /// Drops the older versions no snapshot needs (Steam's pruning: Böttcher, Leis, Neumann,
    /// Kemper, PVLDB 12(6), 2019; interval collection, Lee et al., SIGMOD 2016): `open` the open
    /// snapshots' starts, oldest first, and `visible` the last commit's at the same moment, which
    /// any snapshot opened later begins at or after. Kept: for each snapshot, the newest version
    /// at or before it unless it sees the cell (`cell` the commit that wrote the cell, none while
    /// its writer is open); every version after `visible` and the newest at or before it (what a
    /// later snapshot may see); and the version an open writer replaced (its rollback's).
    fn prune(&mut self, cell: Option<Ts>, open: &[Ts], visible: Ts) {
        let mut keep: Vec<bool> = self.older.iter().map(|o| o.written > visible).collect();
        let mut mark = |at: Ts| {
            if cell.is_some_and(|c| c <= at) {
                return;
            }
            if let Some(slot) = self
                .older
                .iter()
                .position(|o| o.written <= at)
                .and_then(|i| keep.get_mut(i))
            {
                *slot = true;
            }
        };
        for &at in open {
            mark(at);
        }
        mark(visible);
        if cell.is_none() {
            if let Some(first) = keep.first_mut() {
                *first = true;
            }
        }
        let mut kept = keep.into_iter();
        self.older.retain(|_| kept.next().unwrap_or(true));
    }
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

    /// The row under `key` as `txn` sees it, the read kept for its commit's validation: a
    /// serializable read (decision 5). `get` with `txn`'s snapshot reads without keeping it.
    pub fn read(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        txn: &mut Txn,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let row = self.get(pool, d, clock, &txn.snapshot(), key)?;
        let size = size_of::<(u64, Vec<u8>)>().saturating_add(key.len());
        let tree = self.id;
        txn.reads
            .keep(&clock.budget, &mut txn.read_bytes, tree, size, |r| {
                r.points.push((tree, key.to_vec()));
            })?;
        Ok(row)
    }

    /// Up to `most` rows from `from` on as `txn` sees them, in key order, the span read kept for
    /// its commit's validation: up to the last row given when there are `most`, else to the end.
    pub fn read_range(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        txn: &mut Txn,
        from: Option<&[u8]>,
        most: usize,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if most == 0 {
            return Ok(Vec::new());
        }
        let rows = self.scan(pool, d, clock, &txn.snapshot(), from, most)?;
        let to = if rows.len() < most {
            None
        } else {
            rows.last().map(|(k, _)| k.clone())
        };
        let span = Span {
            tree: self.id,
            from: from.map(<[u8]>::to_vec),
            to,
        };
        let size = usize::try_from(span.bytes()).map_err(|_| LimboError::IntegerOverflow)?;
        txn.reads
            .keep(&clock.budget, &mut txn.read_bytes, self.id, size, |r| {
                r.spans.push(span);
            })?;
        Ok(rows)
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
        // What the version kept for the snapshots that see it, and `txn`'s note of the row, take
        // from the budget: reserved before anything changes, so a refusal leaves the row as it is.
        let older = Older {
            written: 0,
            value: before.clone(),
        };
        let noted = size_of::<(u64, Vec<u8>)>().saturating_add(key.len());
        let noted_bytes = u64::try_from(noted).map_err(|_| LimboError::IntegerOverflow)?;
        match heads.get_mut(key) {
            Some(head) => match clock.resolve(head.writer)? {
                Writer::Open(id) if id == txn.id => {}
                Writer::Open(_) => return Err(LimboError::WriteWriteConflict),
                Writer::Committed(ts) if ts > txn.start => {
                    return Err(LimboError::WriteWriteConflict)
                }
                Writer::Committed(ts) => {
                    clock
                        .budget
                        .reserve("a row's version", older.bytes().saturating_add(noted))?;
                    head.older.insert(
                        0,
                        Older {
                            written: ts,
                            ..older
                        },
                    );
                    head.writer = txn.id;
                    txn.changed.push((self.id, key.to_vec()));
                    txn.charged = txn.charged.saturating_add(noted_bytes);
                }
                Writer::Aborted => {
                    return Err(LimboError::InternalError(
                        "versioned storage: an aborted transaction's version still in place".into(),
                    ))
                }
            },
            None => {
                clock.budget.reserve(
                    "a row's version",
                    head_entry_bytes(key.len())
                        .saturating_add(older.bytes())
                        .saturating_add(noted),
                )?;
                heads.insert(
                    key.to_vec(),
                    Head {
                        writer: txn.id,
                        older: vec![older],
                    },
                );
                txn.changed.push((self.id, key.to_vec()));
                txn.charged = txn.charged.saturating_add(noted_bytes);
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
            let op = size_of::<Op>()
                .saturating_add(key.len())
                .saturating_add(value.len());
            let n = clock.budget.reserve("a transaction's changes", op)?;
            let before = match self.take(clock, txn, key, cell, &mut heads) {
                Ok(before) => before,
                Err(e) => {
                    clock.budget.release(n);
                    return Err(e);
                }
            };
            txn.charged = txn.charged.saturating_add(n);
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
            let op = size_of::<Op>().saturating_add(key.len());
            let n = clock.budget.reserve("a transaction's changes", op)?;
            let before = match self.take(clock, txn, key, cell, &mut heads) {
                Ok(before) => before,
                Err(e) => {
                    clock.budget.release(n);
                    return Err(e);
                }
            };
            txn.charged = txn.charged.saturating_add(n);
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
            Op::Made { .. } | Op::Dropped { .. } | Op::Ordered { .. } => {
                Err(LimboError::InternalError(
                    "versioned storage: a tree made, dropped or ordered replayed as a row change"
                        .into(),
                ))
            }
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

    /// The B-link tree under the versions: a cursor walks its leaves ([`super::cursor`]).
    pub(super) fn tree(&self) -> &Tree {
        &self.tree
    }

    /// `f` on each of cells `cells` of `leaf` (a page under its latch) whose row `snap` sees, in
    /// key order: its key and that row.
    pub(super) fn leaf_rows(
        &self,
        clock: &Clock,
        snap: &Snapshot,
        leaf: &[u8],
        cells: std::ops::Range<usize>,
        mut f: impl FnMut(&[u8], &[u8]) -> Result<()>,
    ) -> Result<()> {
        for i in cells {
            let key = page::key(leaf, i)?;
            let cell = Cell {
                flags: page::flags(leaf, i)?,
                value: page::value(leaf, i)?,
            };
            if cell.flags.versioned {
                let row = {
                    let heads = self.heads(key)?.lock();
                    Self::visible(clock, snap, Some(cell), heads.get(key))?
                };
                if let Some(row) = row {
                    f(key, &row)?;
                }
            } else if !cell.flags.removed {
                f(key, cell.value)?;
            }
        }
        Ok(())
    }

    /// Puts `value` (none: removed) back in place as `txn`'s version of the row under `key`, which
    /// it holds: a rollback to a savepoint after which it changed the row again.
    fn reset_own(
        &self,
        pool: &Pool,
        d: &mut Descender,
        txn: &Txn,
        key: &[u8],
        value: Option<&[u8]>,
    ) -> Result<()> {
        if let Some(v) = value {
            Tree::check_row(key, v)?;
        }
        self.tree.modify(pool, d, key, |cell| {
            let heads = self.heads(key)?.lock();
            if heads.get(key).is_none_or(|h| h.writer != txn.id) {
                return Err(LimboError::InternalError(
                    "versioned storage: a savepoint's row its transaction doesn't hold".into(),
                ));
            }
            let change = match (value, cell) {
                (Some(v), _) => Change::Put(
                    v.to_vec(),
                    Flags {
                        versioned: true,
                        removed: false,
                    },
                ),
                (None, Some(_)) => Change::Mark(Flags {
                    versioned: true,
                    removed: true,
                }),
                (None, None) => Change::Keep,
            };
            Ok((change, ()))
        })
    }

    /// Puts back the version `txn` replaced under `key` (its rollback).
    fn restore(
        &self,
        pool: &Pool,
        d: &mut Descender,
        clock: &Clock,
        txn: &Txn,
        key: &[u8],
    ) -> Result<()> {
        self.tree.modify(pool, d, key, |cell| {
            let mut heads = self.heads(key)?.lock();
            let Some(head) = heads.get_mut(key) else {
                return Ok((Change::Keep, ()));
            };
            if head.writer != txn.id || head.older.is_empty() {
                return Ok((Change::Keep, ()));
            }
            let held = head.bytes(key.len());
            let prior = head.older.remove(0);
            head.writer = prior.written;
            clock
                .budget
                .release(held.saturating_sub(head.bytes(key.len())));
            let change = if head.older.is_empty() && prior.written == 0 {
                // No snapshot needs anything older: the row is as it was before any.
                if let Some(gone) = heads.remove(key) {
                    clock.budget.release(gone.bytes(key.len()));
                }
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

/// A database's versioned trees, their pool, their clock, and how its commits are validated.
pub struct Versioned {
    pub pool: Pool,
    pub clock: Clock,
    /// Each tree by id; None once dropped (`AC-NS.2.5`). Trees are made and dropped while the
    /// database runs ([`Versioned::make_tree`], [`Versioned::set_tree`]); an operation holds its
    /// tree's `Arc`, so a drop never pulls a tree from under one already in it.
    trees: RwLock<Vec<Option<Arc<VTree>>>>,
    validation: Validation,
    /// The commits a validation may still check against, oldest first: each later than the low
    /// watermark, so bounded as the versions are (every row one of them wrote keeps its head
    /// until its newest version is at or below the watermark). Locked only under the publish
    /// lock.
    recent: Mutex<VecDeque<Committed>>,
}

/// The budget of a pool's versions and records (decision 10). A pool of a fixed count of frames
/// holds them to as many bytes as the pages they version, until the database is given its own
/// ([`Versioned::with_budget`]): InnoDB keeps undo in buffer-pool pages and LeanStore its version
/// records in the buffer manager (Alhomssi, Leis, PVLDB 2023), so in both the memory the database
/// was given for pages bounds its versions too.
/// A pool whose frames come from a memory source takes its versions' bytes from it too, as much
/// as it grants beside the pages (G7 step 2, `AC-NS.2.1`): no part of it is set aside for either.
fn pool_budget(pool: &Pool) -> Budget {
    match pool.source() {
        Some(source) => Budget::shared(source),
        None => Budget::new(
            u64::try_from(pool.capacity().saturating_mul(super::page::PAGE_SIZE))
                .unwrap_or(u64::MAX),
        ),
    }
}

impl Versioned {
    /// `trees` empty trees in a pool of `frames` pages, tree `i` ordered by `cmp(i)`.
    pub fn new(
        frames: usize,
        trees: usize,
        cmp: impl Fn(usize) -> super::blink::Comparator,
    ) -> Result<Self> {
        Self::new_in(Pool::new(frames), trees, cmp)
    }

    /// `trees` empty trees in `pool` (one over a disk evicts, G7 step 4), tree `i` ordered by
    /// `cmp(i)`.
    pub fn new_in(
        pool: Pool,
        trees: usize,
        cmp: impl Fn(usize) -> super::blink::Comparator,
    ) -> Result<Self> {
        let budget = pool_budget(&pool);
        let mut made = Vec::with_capacity(trees);
        for i in 0..trees {
            let id = u64::try_from(i).map_err(|_| LimboError::IntegerOverflow)?;
            made.push(Some(Arc::new(VTree::new(id, Tree::create(&pool, cmp(i))?))));
        }
        Ok(Self {
            pool,
            clock: Clock::within(budget),
            trees: RwLock::new(made),
            validation: Validation::default(),
            recent: Mutex::new(VecDeque::new()),
        })
    }

    /// The same database, its versions and records held to `bytes` (decision 10): the memory the
    /// process gives it, learned where it runs. Given before any transaction begins.
    pub fn with_budget(mut self, bytes: u64) -> Self {
        self.clock.budget = Budget::new(bytes);
        self
    }

    /// The same database, its commits validated by `validation`.
    pub fn with_validation(mut self, validation: Validation) -> Self {
        self.validation = validation;
        self
    }

    pub fn validation(&self) -> Validation {
        self.validation
    }

    /// The trees whose roots are `roots`, in `pool` (recovery), tree `i` ordered by `cmp(i)`.
    pub fn open(
        pool: Pool,
        roots: &[u64],
        cmp: impl Fn(usize) -> super::blink::Comparator,
    ) -> Result<Self> {
        let mut trees = Vec::with_capacity(roots.len());
        for (i, &root) in roots.iter().enumerate() {
            let id = u64::try_from(i).map_err(|_| LimboError::IntegerOverflow)?;
            // Root 0, the data file's header, is never a tree's: the tree was dropped.
            trees.push((root != 0).then(|| Arc::new(VTree::new(id, Tree::open(root, cmp(i))))));
        }
        let budget = pool_budget(&pool);
        Ok(Self {
            pool,
            clock: Clock::within(budget),
            trees: RwLock::new(trees),
            validation: Validation::default(),
            recent: Mutex::new(VecDeque::new()),
        })
    }

    /// Each tree's root page, by tree; 0 for a tree dropped (`AC-NS.2.5`).
    pub fn roots(&self) -> Vec<u64> {
        self.trees
            .read()
            .iter()
            .map(|t| t.as_ref().map_or(0, |t| t.root()))
            .collect()
    }

    /// The trees not dropped.
    fn live(&self) -> Vec<Arc<VTree>> {
        self.trees.read().iter().flatten().cloned().collect()
    }

    /// Makes an empty tree ordered by `cmp` under the next id, which it returns (`AC-NS.2.5`).
    pub fn make_tree(&self, cmp: super::blink::Comparator) -> Result<u64> {
        let mut trees = self.trees.write();
        let id = u64::try_from(trees.len()).map_err(|_| LimboError::IntegerOverflow)?;
        trees.push(Some(Arc::new(VTree::new(
            id,
            Tree::create(&self.pool, cmp)?,
        ))));
        Ok(id)
    }

    /// Recovery's replay of a tree made: an empty tree ordered by `cmp` under `id`, at or past the
    /// next (`AC-NS.2.5`). Ids between are of trees whose making failed before its record was
    /// durable: none of them is a tree.
    pub fn remake_tree(&self, id: u64, cmp: super::blink::Comparator) -> Result<()> {
        let mut trees = self.trees.write();
        let at = usize::try_from(id).map_err(|_| LimboError::IntegerOverflow)?;
        if at < trees.len() {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: tree {id} made twice"
            )));
        }
        // Each id between was handed out by a making that took a page for its root, and a
        // making that fails stops the database's durability (the redo log's poison), so a gap
        // wider than the pool's frames is damage, refused.
        if at.saturating_sub(trees.len()) > self.pool.capacity() {
            return Err(LimboError::Corrupt(format!(
                "versioned storage: tree {id} made past every tree the pages could hold"
            )));
        }
        trees.resize(at, None);
        trees.push(Some(Arc::new(VTree::new(
            id,
            Tree::create(&self.pool, cmp)?,
        ))));
        Ok(())
    }

    /// Drops tree `id` (`AC-NS.2.5`): later operations naming it are refused; its pages stay where
    /// they are, unreferenced.
    pub fn drop_tree(&self, id: u64) -> Result<()> {
        let mut trees = self.trees.write();
        let slot = usize::try_from(id)
            .ok()
            .and_then(|i| trees.get_mut(i))
            .filter(|t| t.is_some())
            .ok_or_else(|| no_tree(id))?;
        *slot = None;
        Ok(())
    }

    /// Whether a transaction still open has changed a row of tree `id`: a drop then waits for it
    /// (`AC-NS.2.5`).
    pub fn open_changes(&self, id: u64) -> Result<bool> {
        let t = self.tree(id)?;
        for shard in &t.heads {
            for head in shard.lock().values() {
                if matches!(self.clock.resolve(head.writer)?, Writer::Open(_)) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Clears the versions on every tree's cells, dropping removed rows' (recovery, which starts
    /// with no heads: each cell is every snapshot's).
    pub fn clear_versions(&self, d: &mut Descender) -> Result<()> {
        for t in &self.live() {
            t.tree.clear_versions(&self.pool, d)?;
        }
        Ok(())
    }

    /// Every tree's rows whose cell isn't their version as of `ts`, with that version (decision 7).
    pub fn corrections(&self, ts: Ts) -> Result<Vec<Op>> {
        let mut out = Vec::new();
        for t in &self.live() {
            t.corrections(&self.clock, ts, &mut out)?;
        }
        Ok(out)
    }

    pub fn tree(&self, id: u64) -> Result<Arc<VTree>> {
        usize::try_from(id)
            .ok()
            .and_then(|i| self.trees.read().get(i).cloned().flatten())
            .ok_or_else(|| no_tree(id))
    }

    /// Commits `txn`: its changes visible to every snapshot from its timestamp on, which it
    /// returns; its versions then name the timestamp, not the transaction. A commit that fails is
    /// rolled back.
    pub fn commit(&self, txn: Txn, d: &mut Descender) -> Result<Ts> {
        self.commit_logged(txn, d, |_, _| Ok(()))
    }

    /// [`Versioned::commit`], `log` given the timestamp and the changes under the publish lock
    /// (the redo log's append, in timestamp order); a failure of `log` rolls `txn` back. Validated
    /// first, under the same lock, so no commit lands between its check and its timestamp: a
    /// refusal (`SerializationFailure`) rolls `txn` back.
    pub fn commit_logged(
        &self,
        mut txn: Txn,
        d: &mut Descender,
        log: impl FnOnce(Ts, &[Op]) -> Result<()>,
    ) -> Result<Ts> {
        let reads = std::mem::take(&mut txn.reads);
        // The reads' charge moves to the kept commit, or is released here.
        let read_bytes = std::mem::take(&mut txn.read_bytes);
        let mut reached = false;
        let published = self.clock.commit(&txn, |ts, ops| {
            reached = true;
            let mut recent = self.recent.lock();
            let out_first = match self.validate(&txn, &reads, &recent, d) {
                Ok(out_first) => out_first,
                Err(e) => {
                    self.clock.budget.release(read_bytes);
                    return Err(e);
                }
            };
            self.let_go(&mut recent);
            let kept = self.kept(&txn, reads, read_bytes, ts, out_first)?;
            if let Err(e) = log(ts, ops) {
                if let Some(c) = kept {
                    self.clock.budget.release(c.bytes);
                }
                return Err(e);
            }
            if let Some(c) = kept {
                recent.push_back(c);
            }
            Ok(())
        });
        let ts = match published {
            Ok(ts) => ts,
            Err(e) => {
                if !reached {
                    self.clock.budget.release(read_bytes);
                }
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

    /// Whether `txn`'s commit may go ahead (decision 5), for SSI with the earliest commit it read
    /// past; refused with `SerializationFailure`. Under the publish lock, `recent` locked.
    fn validate(
        &self,
        txn: &Txn,
        reads: &Reads,
        recent: &VecDeque<Committed>,
        d: &mut Descender,
    ) -> Result<Option<Ts>> {
        // Since the snapshot, newest first.
        let since = || recent.iter().rev().take_while(|c| c.ts > txn.start);
        match self.validation {
            Validation::Snapshot => Ok(None),
            // One that only read is serialized at its snapshot, a prefix of the commit order.
            Validation::Precision | Validation::Occ if txn.changed.is_empty() => Ok(None),
            Validation::Precision => {
                if reads.is_empty() {
                    return Ok(None);
                }
                for c in since() {
                    for (tree, key) in &c.writes {
                        if reads.covers(self, *tree, key)? {
                            return Err(LimboError::SerializationFailure);
                        }
                    }
                }
                Ok(None)
            }
            Validation::Occ => {
                self.occ(txn, reads, d)?;
                Ok(None)
            }
            Validation::Ssi => {
                // `txn -> c` where it read past a row `c` wrote; `c -> txn` where `c` read past
                // one `txn` wrote. Each pair is seen at the later commit of the two, so only
                // committed transactions need checking (Fekete et al.'s theorem: every cycle
                // has a dangerous structure whose `out` commits first).
                let mut out_first: Option<Ts> = None;
                let mut in_last: Option<Ts> = None;
                for c in since() {
                    let mut out = false;
                    for (tree, key) in &c.writes {
                        if reads.covers(self, *tree, key)? {
                            out = true;
                            break;
                        }
                    }
                    if out {
                        // `txn -> c -> o` with `o` committed before `c`, and so before `txn`.
                        if c.out_first.is_some() {
                            return Err(LimboError::SerializationFailure);
                        }
                        out_first = Some(out_first.map_or(c.ts, |o| o.min(c.ts)));
                    }
                    for (tree, key) in &txn.changed {
                        if c.reads.covers(self, *tree, key)? {
                            in_last = Some(in_last.map_or(c.ts, |i| i.max(c.ts)));
                            break;
                        }
                    }
                }
                // `c -> txn -> o` with `o` committed no later than `c` (the same, a two-cycle).
                if let (Some(o), Some(i)) = (out_first, in_last) {
                    if i >= o {
                        return Err(LimboError::SerializationFailure);
                    }
                }
                Ok(out_first)
            }
        }
    }

    /// Silo's read-set check at row grain: refused if a row `txn` read, or one in a span it read
    /// (read again), has a version committed after its snapshot. A head no longer kept was last
    /// written at or below the low watermark, at or before the snapshot. The newest committed
    /// version is the head's own when its writer committed, else the newest older one: an open
    /// writer's change (another's, or `txn`'s own) stands over a commit made since the snapshot
    /// that `txn` read past, and must not hide it.
    fn occ(&self, txn: &Txn, reads: &Reads, d: &mut Descender) -> Result<()> {
        let moved = |head: &Head| -> Result<bool> {
            let newest = match self.clock.resolve(head.writer)? {
                Writer::Committed(ts) => ts,
                Writer::Open(_) | Writer::Aborted => head.older.first().map_or(0, |o| o.written),
            };
            Ok(newest > txn.start)
        };
        for (tree, key) in &reads.points {
            let t = self.tree(*tree)?;
            let heads = t.heads(key)?.lock();
            if let Some(head) = heads.get(key.as_slice()) {
                if moved(head)? {
                    return Err(LimboError::SerializationFailure);
                }
            }
        }
        for s in &reads.spans {
            let t = self.tree(s.tree)?;
            let cmp = t.tree.compare();
            let mut refused = false;
            t.tree
                .scan_with(&self.pool, d, s.from.as_deref(), |key, cell| {
                    if s.to
                        .as_deref()
                        .is_some_and(|to| cmp(key, to) == Order::Greater)
                    {
                        return Ok(false);
                    }
                    if cell.flags.versioned {
                        let heads = t.heads(key)?.lock();
                        if let Some(head) = heads.get(key) {
                            refused = moved(head)?;
                        }
                    }
                    Ok(!refused)
                })?;
            if refused {
                return Err(LimboError::SerializationFailure);
            }
        }
        Ok(())
    }

    /// Lets go of the kept commits no open transaction began before: none that may still commit
    /// is concurrent with them (a read-only snapshot never validates, so it keeps none).
    fn let_go(&self, recent: &mut VecDeque<Committed>) {
        let oldest = self.clock.oldest_txn();
        while recent.front().is_some_and(|c| c.ts <= oldest) {
            if let Some(c) = recent.pop_front() {
                self.clock.budget.release(c.bytes);
            }
        }
    }

    /// `txn`'s commit at `ts` as kept for the validations of transactions concurrent with it, if
    /// its validation keeps it: its writes' copy charged to the budget, its reads' charge
    /// (`read_bytes`) moved to it when kept and released when not. Refused past the budget, every
    /// charge released.
    fn kept(
        &self,
        txn: &Txn,
        reads: Reads,
        read_bytes: u64,
        ts: Ts,
        out_first: Option<Ts>,
    ) -> Result<Option<Committed>> {
        let (keep, reads) = match self.validation {
            Validation::Precision => (!txn.changed.is_empty(), Reads::default()),
            Validation::Ssi => (!txn.changed.is_empty() || !reads.is_empty(), reads),
            Validation::Snapshot | Validation::Occ => (false, Reads::default()),
        };
        let read_bytes = if keep && !reads.is_empty() {
            read_bytes
        } else {
            self.clock.budget.release(read_bytes);
            0
        };
        if !keep {
            return Ok(None);
        }
        let size = txn
            .changed
            .iter()
            .fold(size_of::<Committed>(), |n, (_, k)| {
                n.saturating_add(size_of::<(u64, Vec<u8>)>())
                    .saturating_add(k.len())
            });
        let writes = match self
            .clock
            .budget
            .reserve("a commit kept for validation", size)
        {
            Ok(n) => n,
            Err(e) => {
                self.clock.budget.release(read_bytes);
                return Err(e);
            }
        };
        Ok(Some(Committed {
            ts,
            writes: txn.changed.clone(),
            reads,
            out_first,
            bytes: writes.saturating_add(read_bytes),
        }))
    }

    /// Commits kept for validations: a measure of [`Versioned`]'s `recent`.
    pub fn recent_commits(&self) -> usize {
        self.recent.lock().len()
    }

    /// Rolls `txn` back: each row it changed as it was before, last change first.
    pub fn abort(&self, txn: Txn, d: &mut Descender) -> Result<()> {
        for (tree, key) in txn.changed.iter().rev() {
            self.tree(*tree)?
                .restore(&self.pool, d, &self.clock, &txn, key)?;
        }
        self.clock.set_aborted(&txn)?;
        self.clock.forget(&txn)
    }

    /// Rolls `txn` back to `sp`, which stays good (SQLite's `ROLLBACK TO` keeps its savepoint):
    /// each row it first changed after `sp` as it was before `txn` (as [`Versioned::abort`] puts
    /// it), each row it changed before and after as `sp` found it, its later changes dropped from
    /// its redo record. A failure leaves `txn` to [`Versioned::abort`], which still finds every
    /// row it changed.
    pub fn rollback_to(&self, txn: &mut Txn, sp: Savepoint, d: &mut Descender) -> Result<()> {
        if sp.txn != txn.id || sp.ops > txn.ops.len() || sp.changed > txn.changed.len() {
            return Err(LimboError::InternalError(
                "versioned storage: a savepoint not of this transaction, or rolled back past"
                    .into(),
            ));
        }
        let (kept, later) = txn.ops.split_at(sp.ops);
        let fresh = txn.changed.get(sp.changed..).unwrap_or_default();
        let fresh_rows: FxHashSet<Row<'_>> =
            fresh.iter().map(|(t, k)| (*t, k.as_slice())).collect();
        // Each row's last value before the savepoint (none: removed).
        let mut then: FxHashMap<Row<'_>, Option<&[u8]>> = FxHashMap::default();
        for op in kept {
            match op {
                Op::Put { tree, key, value } => {
                    then.insert((*tree, key.as_slice()), Some(value.as_slice()))
                }
                Op::Remove { tree, key } => then.insert((*tree, key.as_slice()), None),
                // A transaction's changes are rows only: trees are made and dropped alone.
                Op::Made { .. } | Op::Dropped { .. } | Op::Ordered { .. } => continue,
            };
        }
        // Rows changed before the savepoint and after it, each once, with that value.
        let mut again: Vec<(Row<'_>, Option<&[u8]>)> = Vec::new();
        let mut seen: FxHashSet<Row<'_>> = FxHashSet::default();
        for op in later {
            let row = match op {
                Op::Put { tree, key, .. } | Op::Remove { tree, key } => (*tree, key.as_slice()),
                Op::Made { .. } | Op::Dropped { .. } | Op::Ordered { .. } => continue,
            };
            if fresh_rows.contains(&row) || !seen.insert(row) {
                continue;
            }
            let value = then.get(&row).copied().ok_or_else(|| {
                LimboError::InternalError(
                    "versioned storage: a row changed before a savepoint with no change of it"
                        .into(),
                )
            })?;
            again.push((row, value));
        }
        for ((tree, key), then) in &again {
            self.tree(*tree)?
                .reset_own(&self.pool, d, txn, key, *then)?;
        }
        for (tree, key) in fresh.iter().rev() {
            self.tree(*tree)?
                .restore(&self.pool, d, &self.clock, txn, key)?;
        }
        let freed = later
            .iter()
            .map(op_bytes)
            .chain(fresh.iter().map(|(_, k)| noted_bytes(k)))
            .fold(0u64, u64::saturating_add);
        txn.ops.truncate(sp.ops);
        txn.changed.truncate(sp.changed);
        self.clock.budget.release(freed);
        txn.charged = txn.charged.saturating_sub(freed);
        Ok(())
    }

    /// Drops the versions no open snapshot can see: rows whose cell's version every snapshot sees
    /// lose their heads (and their cells, if removed); others keep only the older versions still
    /// visible to some snapshot. How many rows lost their heads.
    pub fn collect(&self, d: &mut Descender) -> Result<usize> {
        let (open, visible) = self.clock.open_starts();
        let lwm = open.first().map_or(visible, |&oldest| oldest.min(visible));
        let mut collected = 0usize;
        for t in &self.live() {
            let mut ripe = Vec::new();
            for shard in &t.heads {
                let mut heads = shard.lock();
                for (key, head) in heads.iter_mut() {
                    let cell = match self.clock.resolve(head.writer)? {
                        Writer::Committed(ts) if ts <= lwm => {
                            ripe.push(key.clone());
                            continue;
                        }
                        Writer::Committed(ts) => Some(ts),
                        Writer::Open(_) | Writer::Aborted => None,
                    };
                    let held = head.bytes(key.len());
                    head.prune(cell, &open, visible);
                    self.clock
                        .budget
                        .release(held.saturating_sub(head.bytes(key.len())));
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
                    if let Some(gone) = heads.remove(&key) {
                        self.clock.budget.release(gone.bytes(key.len()));
                    }
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
        self.live()
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
        Versioned::new(20_000, 1, |_| Box::new(|a: &[u8], b: &[u8]| a.cmp(b))).unwrap()
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

    const SERIALIZABLE: [Validation; 3] = [Validation::Precision, Validation::Ssi, Validation::Occ];
    const EVERY: [Validation; 4] = [
        Validation::Snapshot,
        Validation::Precision,
        Validation::Ssi,
        Validation::Occ,
    ];

    /// A database validating by `validation`, with rows `x` and `y` at 1.
    fn xy(validation: Validation) -> Versioned {
        let v = db().with_validation(validation);
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut setup = v.clock.begin().unwrap();
        for k in [b"x", b"y"] {
            t.put(&v.pool, &mut d, &v.clock, &mut setup, k, b"1")
                .unwrap();
        }
        v.commit(setup, &mut d).unwrap();
        v
    }

    fn refused(r: Result<Ts>) -> bool {
        matches!(r, Err(LimboError::SerializationFailure))
    }

    /// G8 step 1 (T-G8.1.2): do two transactions each read `x` and `y` and write the one the
    /// other didn't, both from the same snapshot. Expect: snapshot isolation commits both (write
    /// skew, G2); each serializable validation refuses the second, its write rolled back.
    #[test]
    fn g8_1_write_skew_is_refused_by_each_serializable_validation() {
        for validation in EVERY {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut a = v.clock.begin().unwrap();
            let mut b = v.clock.begin().unwrap();
            for txn in [&mut a, &mut b] {
                for k in [b"x", b"y"] {
                    t.read(&v.pool, &mut d, &v.clock, txn, k).unwrap();
                }
            }
            t.put(&v.pool, &mut d, &v.clock, &mut a, b"x", b"0")
                .unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"y", b"0")
                .unwrap();
            v.commit(a, &mut d).unwrap();
            let second = v.commit(b, &mut d);
            if validation == Validation::Snapshot {
                assert!(second.is_ok(), "{validation:?}: {second:?}");
            } else {
                assert!(refused(second), "{validation:?}");
                let snap = v.clock.snapshot().unwrap();
                assert_eq!(
                    t.get(&v.pool, &mut d, &v.clock, &snap, b"y").unwrap(),
                    Some(b"1".to_vec()),
                    "{validation:?}: the refused write rolled back"
                );
                v.clock.close(snap.start);
            }
        }
    }

    /// T-G8.1.2: do a transaction read every row from `a` on (fewer than it asked for), another
    /// read `x`, insert `m` into that span and commit, then the first write `x` and commit.
    /// Expect: refused by each serializable validation (each read past the other's write, a
    /// cycle through a phantom), committed under snapshot isolation.
    #[test]
    fn g8_1_a_phantom_in_a_span_read_is_refused() {
        for validation in EVERY {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut a = v.clock.begin().unwrap();
            let rows = t
                .read_range(&v.pool, &mut d, &v.clock, &mut a, Some(b"a"), 10)
                .unwrap();
            assert_eq!(rows.len(), 2);
            let mut b = v.clock.begin().unwrap();
            t.read(&v.pool, &mut d, &v.clock, &mut b, b"x").unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"m", b"1")
                .unwrap();
            v.commit(b, &mut d).unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut a, b"x", b"2")
                .unwrap();
            let r = v.commit(a, &mut d);
            assert_eq!(
                refused(r),
                validation != Validation::Snapshot,
                "{validation:?}"
            );
        }
    }

    /// T-G8.1.2: do a transaction read `x`, another write `x` and commit, then the first write
    /// `y` and commit. Expect: precision locking and OCC refuse it (a row it read changed since
    /// its snapshot); SSI commits it (one antidependency, no dangerous structure): what makes
    /// SSI's aborts fewer.
    #[test]
    fn g8_1_a_lone_antidependency_commits_under_ssi_alone() {
        for validation in SERIALIZABLE {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut a = v.clock.begin().unwrap();
            t.read(&v.pool, &mut d, &v.clock, &mut a, b"x").unwrap();
            let mut b = v.clock.begin().unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"x", b"2")
                .unwrap();
            v.commit(b, &mut d).unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut a, b"y", b"2")
                .unwrap();
            let r = v.commit(a, &mut d);
            assert_eq!(refused(r), validation != Validation::Ssi, "{validation:?}");
        }
    }

    /// T-G8.1.2: Fekete, O'Neil and O'Neil's read-only anomaly ("A Read-Only Transaction Anomaly
    /// Under Snapshot Isolation", SIGMOD Record 33(3), 2004). Do: `w` reads `x` and `y`; `d`
    /// writes `y` and commits; a reader begins after, reads both and commits; `w` writes `x` and
    /// commits. Expect: the reader commits under every validation; `w` is refused by each
    /// serializable one (under SSI as the pivot of `reader -> w -> d`, `d` committed first).
    #[test]
    fn g8_1_the_read_only_anomaly_is_refused() {
        for validation in EVERY {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut desc = Descender::new();
            let d = &mut desc;
            let mut w = v.clock.begin().unwrap();
            t.read(&v.pool, d, &v.clock, &mut w, b"x").unwrap();
            t.read(&v.pool, d, &v.clock, &mut w, b"y").unwrap();
            let mut deposit = v.clock.begin().unwrap();
            t.read(&v.pool, d, &v.clock, &mut deposit, b"y").unwrap();
            t.put(&v.pool, d, &v.clock, &mut deposit, b"y", b"21")
                .unwrap();
            v.commit(deposit, d).unwrap();
            let mut reader = v.clock.begin().unwrap();
            t.read(&v.pool, d, &v.clock, &mut reader, b"x").unwrap();
            t.read(&v.pool, d, &v.clock, &mut reader, b"y").unwrap();
            v.commit(reader, d).unwrap();
            t.put(&v.pool, d, &v.clock, &mut w, b"x", b"-10").unwrap();
            let r = v.commit(w, d);
            assert_eq!(
                refused(r),
                validation != Validation::Snapshot,
                "{validation:?}"
            );
        }
    }

    /// T-G8.1.2: do a transaction read `x` while another changes `x` and stays open, then the
    /// first write `y` and commit. Expect: committed under each validation: the open writer
    /// commits after it, so it reads before that writer in the serial order (Silo's refusal of a
    /// read row locked by a writer is for its validation outside a publish lock, which this
    /// commit path doesn't have).
    #[test]
    fn g8_1_an_open_writer_of_a_row_read_refuses_nothing() {
        for validation in EVERY {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut a = v.clock.begin().unwrap();
            t.read(&v.pool, &mut d, &v.clock, &mut a, b"x").unwrap();
            let mut b = v.clock.begin().unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"x", b"2")
                .unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut a, b"y", b"2")
                .unwrap();
            v.commit(a, &mut d).unwrap();
            v.commit(b, &mut d).unwrap();
        }
    }

    /// T-G8.1.2: do a transaction read `x`, another write `x` and commit, a third begin after and
    /// write `x` and stay open, then the first write `y` and commit. Expect: precision locking and
    /// OCC refuse it as they do with no third (the open writer's change stands over the commit it
    /// read past, and OCC finds that commit under it); SSI commits it (one antidependency).
    #[test]
    fn g8_1_an_open_writer_does_not_hide_a_commit_read_past() {
        for validation in SERIALIZABLE {
            let v = xy(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut a = v.clock.begin().unwrap();
            t.read(&v.pool, &mut d, &v.clock, &mut a, b"x").unwrap();
            let mut c = v.clock.begin().unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut c, b"x", b"2")
                .unwrap();
            v.commit(c, &mut d).unwrap();
            let mut b = v.clock.begin().unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut b, b"x", b"3")
                .unwrap();
            t.put(&v.pool, &mut d, &v.clock, &mut a, b"y", b"2")
                .unwrap();
            let r = v.commit(a, &mut d);
            assert_eq!(refused(r), validation != Validation::Ssi, "{validation:?}");
            v.commit(b, &mut d).unwrap();
        }
    }

    /// T-G8.1.2, T-G8.2.2: do 100 commits one after another, then 100 with a read-only snapshot
    /// held open, then 100 with a transaction held open from the first, then one after it ends.
    /// Expect: the commits kept for validation are the last one alone with nothing open and with
    /// only the read-only snapshot (it never validates), every one since the held transaction
    /// began while it is open, and the last alone again once it ends.
    #[test]
    fn g8_1_commits_are_kept_only_while_a_transaction_before_them_is_open() {
        for validation in [Validation::Precision, Validation::Ssi] {
            let v = db().with_validation(validation);
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            let mut write = |n: u8| {
                let mut txn = v.clock.begin().unwrap();
                t.read(&v.pool, &mut d, &v.clock, &mut txn, &[n]).unwrap();
                t.put(&v.pool, &mut d, &v.clock, &mut txn, &[n], &[n])
                    .unwrap();
                v.commit(txn, &mut d).unwrap();
            };
            for n in 0..100 {
                write(n);
            }
            assert_eq!(v.recent_commits(), 1, "{validation:?}");
            let snap = v.clock.snapshot().unwrap();
            for n in 0..100 {
                write(n);
            }
            assert_eq!(v.recent_commits(), 1, "{validation:?}");
            let held = v.clock.begin().unwrap();
            for n in 0..100 {
                write(n);
            }
            assert_eq!(v.recent_commits(), 100, "{validation:?}");
            v.abort(held, &mut Descender::new()).unwrap();
            write(0);
            assert_eq!(v.recent_commits(), 1, "{validation:?}");
            v.clock.close(snap.start);
        }
    }

    /// The bytes kept commits hold: with no transaction open, all the budget should hold.
    fn kept_bytes(v: &Versioned) -> u64 {
        v.recent.lock().iter().map(|c| c.bytes).sum()
    }

    /// Older versions kept beside the pages, in every tree.
    fn older_versions(v: &Versioned) -> usize {
        v.live()
            .iter()
            .flat_map(|t| t.heads.iter())
            .map(|s| s.lock().values().map(|h| h.older.len()).sum::<usize>())
            .sum()
    }

    /// T-G8.2.3: do a transaction read rows one by one under a budget too small for its reads,
    /// then a commit write one of them. Expect: no read refused (past the budget its reads become
    /// the whole tree, held to one span), and the reader's commit refused as one that read the
    /// row would be (escalation refuses at least what the finer record would).
    #[test]
    fn g8_2_reads_past_the_budget_escalate_to_the_whole_tree() {
        let span = u64::try_from(size_of::<Span>()).unwrap();
        let point = point_bytes(&[0]);
        let v = db().with_budget(64 * 1024);
        let t = v.tree(0).unwrap();
        let mut d = Descender::new();
        let mut setup = v.clock.begin().unwrap();
        for k in 0..=255u8 {
            t.put(&v.pool, &mut d, &v.clock, &mut setup, &[k], &[0])
                .unwrap();
        }
        v.commit(setup, &mut d).unwrap();
        v.collect(&mut d).unwrap();
        let room = v.clock.budget().limit() - v.clock.budget().held();
        let mut reader = v.clock.begin().unwrap();
        // Writes enough to stand between the reader's reads and the budget.
        let filler_rows = usize::try_from((room - 8 * point) / point).unwrap();
        let filler = v
            .clock
            .budget
            .reserve("test filler", filler_rows * usize::try_from(point).unwrap())
            .unwrap();
        for k in 0..=255u8 {
            t.read(&v.pool, &mut d, &v.clock, &mut reader, &[k])
                .unwrap();
        }
        v.clock.budget.release(filler);
        assert!(reader.reads.whole(0), "escalated");
        assert!(reader.reads.points.is_empty());
        assert_eq!(reader.read_bytes, span);
        t.put(&v.pool, &mut d, &v.clock, &mut reader, &[0], &[1])
            .unwrap();
        let mut w = v.clock.begin().unwrap();
        t.put(&v.pool, &mut d, &v.clock, &mut w, &[200], &[1])
            .unwrap();
        v.commit(w, &mut d).unwrap();
        assert!(matches!(
            v.commit(reader, &mut d),
            Err(LimboError::SerializationFailure)
        ));
    }

    /// T-G8.2.1, the soak (AC-G8.2.1): a reader holds one snapshot open while 32 writers change
    /// its rows (read two, write two, retry when refused) and a collector runs, for each
    /// validation that keeps commits; then one transaction changes every row. Expect: the budget
    /// never holds past its limit; each refusal is typed (a conflict, a serialization failure or
    /// `MemoryBudget`); the reader's every scan is its snapshot's rows; collected, each row keeps
    /// at most the reader's version, not one per update; the transaction changing every row is
    /// refused `MemoryBudget` and its rollback gives back every byte it took; once the reader
    /// closes, no versions stay and the budget holds only the commits kept for validation.
    /// `AMPHORA_G8_SOAK_SECONDS` runs the writers that long (the coordinator's hour-long run);
    /// unset, each writer runs 200 transactions.
    #[test]
    fn g8_2_a_long_reader_under_32_writers_keeps_memory_within_budget() {
        use std::sync::atomic::AtomicBool;
        let rows: u16 = 512;
        let width = 64usize;
        let soak = std::env::var("AMPHORA_G8_SOAK_SECONDS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let row = |r: u16| r.to_be_bytes().to_vec();
        let value = move |who: u16, n: u32| {
            let mut out = Vec::with_capacity(width);
            while out.len() < width {
                out.extend_from_slice(&who.to_be_bytes());
                out.extend_from_slice(&n.to_be_bytes());
            }
            out.truncate(width);
            out
        };
        // Room for the reader's version of every row, and as much again for the writers: a
        // budget the writers fill, so the refusals are exercised.
        let version = head_entry_bytes(2) + size_of::<Older>() + width;
        let budget = u64::try_from(version * usize::from(rows) * 2).unwrap();
        for validation in [Validation::Precision, Validation::Ssi] {
            let v = Arc::new(db().with_validation(validation).with_budget(budget));
            let t = v.tree(0).unwrap();
            let mut d = Descender::new();
            for chunk in 0..rows / 64 {
                let mut setup = v.clock.begin().unwrap();
                for r in chunk * 64..(chunk + 1) * 64 {
                    t.put(&v.pool, &mut d, &v.clock, &mut setup, &row(r), &value(r, 0))
                        .unwrap();
                }
                v.commit(setup, &mut d).unwrap();
                v.collect(&mut d).unwrap();
            }
            let reader = v.clock.snapshot().unwrap();
            let seen = t
                .scan(
                    &v.pool,
                    &mut d,
                    &v.clock,
                    &reader,
                    None,
                    usize::from(rows) + 1,
                )
                .unwrap();
            assert_eq!(seen.len(), usize::from(rows));
            let done = Arc::new(AtomicBool::new(false));
            let refused_budget = Arc::new(AtomicU64::new(0));
            let commits = Arc::new(AtomicU64::new(0));
            let began = std::time::Instant::now();
            let writers: Vec<_> = (0..32u16)
                .map(|who| {
                    let (v, refused_budget, commits) =
                        (v.clone(), refused_budget.clone(), commits.clone());
                    std::thread::spawn(move || {
                        let t = v.tree(0).unwrap();
                        let mut d = Descender::new();
                        let mut seed = u64::from(who) * 0x9e37_79b9 + 1;
                        let mut next = || {
                            seed = seed
                                .wrapping_mul(6_364_136_223_846_793_005)
                                .wrapping_add(1_442_695_040_888_963_407);
                            u16::try_from((seed >> 33) % u64::from(rows)).unwrap()
                        };
                        let mut n = 0u32;
                        loop {
                            n += 1;
                            let more = match soak {
                                Some(limit) => began.elapsed() < limit,
                                None => n <= 200,
                            };
                            if !more {
                                break;
                            }
                            let mut txn = v.clock.begin().unwrap();
                            let (a, b) = (next(), next());
                            let step = (|| {
                                for r in [a, b] {
                                    t.read(&v.pool, &mut d, &v.clock, &mut txn, &row(r))?;
                                    t.put(
                                        &v.pool,
                                        &mut d,
                                        &v.clock,
                                        &mut txn,
                                        &row(r),
                                        &value(who, n),
                                    )?;
                                }
                                Ok::<_, LimboError>(())
                            })();
                            let end = match step {
                                Ok(()) => v.commit(txn, &mut d).map(|_| ()),
                                Err(e) => {
                                    v.abort(txn, &mut d).unwrap();
                                    Err(e)
                                }
                            };
                            match end {
                                Ok(()) => {
                                    commits.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(LimboError::MemoryBudget { .. }) => {
                                    refused_budget.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(
                                    LimboError::WriteWriteConflict
                                    | LimboError::SerializationFailure,
                                ) => {}
                                Err(e) => panic!("an untyped refusal: {e}"),
                            }
                        }
                    })
                })
                .collect();
            let collector = {
                let (v, done) = (v.clone(), done.clone());
                std::thread::spawn(move || {
                    let mut d = Descender::new();
                    let mut rounds = 0u64;
                    while !done.load(Ordering::Acquire) {
                        v.collect(&mut d).unwrap();
                        assert!(v.clock.budget().held() <= v.clock.budget().limit());
                        rounds += 1;
                    }
                    rounds
                })
            };
            let reads = {
                let (v, done, seen) = (v.clone(), done.clone(), seen.clone());
                std::thread::spawn(move || {
                    let t = v.tree(0).unwrap();
                    let mut d = Descender::new();
                    let mut scans = 0u64;
                    while !done.load(Ordering::Acquire) {
                        let now = t
                            .scan(
                                &v.pool,
                                &mut d,
                                &v.clock,
                                &reader,
                                None,
                                usize::from(rows) + 1,
                            )
                            .unwrap();
                        assert!(now == seen, "the reader's rows moved");
                        scans += 1;
                    }
                    scans
                })
            };
            for w in writers {
                w.join().unwrap();
            }
            done.store(true, Ordering::Release);
            let rounds = collector.join().unwrap();
            let scans = reads.join().unwrap();
            assert!(
                commits.load(Ordering::Relaxed) > 0,
                "{validation:?}: nothing committed"
            );
            assert!(
                rounds > 0 && scans > 0,
                "{validation:?}: {rounds} collections, {scans} scans"
            );
            v.collect(&mut d).unwrap();
            assert!(
                older_versions(&v) <= usize::from(rows),
                "{validation:?}: {} older versions for {rows} rows",
                older_versions(&v)
            );

            // One transaction changing every row: past the budget, refused typed, given back.
            let held = v.clock.budget().held();
            let mut all = v.clock.begin().unwrap();
            let mut refusal = None;
            for r in 0..rows {
                if let Err(e) = t.put(&v.pool, &mut d, &v.clock, &mut all, &row(r), &value(999, 0))
                {
                    refusal = Some(e);
                    break;
                }
            }
            assert!(
                matches!(refusal, Some(LimboError::MemoryBudget { .. })),
                "{validation:?}: {refusal:?}"
            );
            v.abort(all, &mut d).unwrap();
            v.collect(&mut d).unwrap();
            assert_eq!(v.clock.budget().held(), held, "{validation:?}");
            let now = t
                .scan(
                    &v.pool,
                    &mut d,
                    &v.clock,
                    &reader,
                    None,
                    usize::from(rows) + 1,
                )
                .unwrap();
            assert!(now == seen, "{validation:?}: the reader's rows moved");

            v.clock.close(reader.start);
            v.collect(&mut d).unwrap();
            assert_eq!(v.versioned_rows(), 0, "{validation:?}");
            assert_eq!(v.clock.budget().held(), kept_bytes(&v), "{validation:?}");
            eprintln!(
                "{validation:?}: {} commits, {} refused by the budget, {rounds} collections, {scans} scans, budget {budget}",
                commits.load(Ordering::Relaxed),
                refused_budget.load(Ordering::Relaxed)
            );
        }
    }

    /// T-NS.2.10: a budget over the machine's memory source reserves and releases row bytes.
    /// Expect: what it takes from the source doubles as it grows (once per doubling), halves
    /// down to twice what is held once that falls under a quarter, and is all given back when the
    /// budget drops (`AC-NS.2.1`).
    #[test]
    fn ns2_a_shared_budget_takes_from_its_source_as_a_dynamic_table() {
        let machine = Arc::new(super::super::memory::Machine::new());
        let source: Arc<dyn Source> = machine.clone();
        let budget = Budget::shared(source);
        budget.reserve("a test's rows", 100).unwrap();
        assert_eq!((budget.held(), budget.taken()), (100, 100));
        budget.reserve("a test's rows", 10).unwrap();
        assert_eq!((budget.held(), budget.taken()), (110, 200));
        budget.reserve("a test's rows", 80).unwrap();
        assert_eq!((budget.held(), budget.taken()), (190, 200));
        assert_eq!(machine.held(), 200);
        budget.release(100);
        assert_eq!((budget.held(), budget.taken()), (90, 200));
        budget.release(50);
        assert_eq!((budget.held(), budget.taken()), (40, 80));
        budget.release(21);
        assert_eq!((budget.held(), budget.taken()), (19, 38));
        assert_eq!(machine.held(), 38);
        drop(budget);
        assert_eq!(machine.held(), 0);
    }
}
