//! Amphora: the executor's cursor on the versioned storage (Amphora's `docs/design/engine.md`,
//! decision 9; kiln plan 01a16b0f46fa72e384b8dd0065dc94f9, step 1, `NS.1`): [`CursorTrait`], the
//! interface the bytecode executor drives, on one B-link tree's rows as one snapshot sees them.
//!
//! A table's tree is keyed by rowid, 8 bytes big-endian with the sign bit flipped (a format value:
//! its bytes order as the integers do), with the row's record as the value. An index's tree, and a
//! WITHOUT ROWID table's, is keyed by the record itself with an empty value, ordered by the index's
//! columns (their collations and directions, `IndexInfo::key_info`) and, among records equal on
//! those, by their bytes: a cell's key is then exactly the record put, and a record that replaces
//! an equal one with other bytes (another spelling under NOCASE, a WITHOUT ROWID row's other
//! columns) replaces it by removal and insertion, so each snapshot sees the record it saw.
//!
//! The cursor holds no latch between calls. It reads a leaf at a time: the rows of the leaf its
//! snapshot sees, copied under the leaf's shared latch with the frame's version they were copied
//! at. While that version stays, the copy is the leaf's rows; once it moves (a write to the leaf,
//! the cursor's own or another's) the cursor finds its row again by its key, which is how it sees
//! its transaction's writes through other cursors, as SQLite's cursors do through
//! saveAllCursors. Moving on from a copy's last row goes to the leaf its right link names; moving
//! back from its first descends again to the leaf before its lower bound, as a B-link tree links
//! its leaves only to the right. Every walk is bounded by the pages handed out.
//!
//! A row larger than a page's cell keeps its tail in the database's overflow tree (`AC-NS.2.4`): the
//! cursor puts it back after copying a leaf, its latch released. What waits for the catalog
//! (step 2): dropping a tree, and a comparison per tree in one database.

use super::blink::{Comparator, Descender, Leaf};
use super::durable::Database;
use super::overflow;
use super::version::{Snapshot, Txn, VTree};
use crate::error::LimboError;
use crate::numeric::Numeric;
use crate::storage::btree::{BTreeKey, CursorTrait};
use crate::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use crate::sync::{Arc, Mutex};
use crate::types::{
    compare_immutable_iter, find_compare, get_tie_breaker_from_seek_op, AsValueRef, IOResult,
    IOResultOr, ImmutableRecord, ImmutableRecordRef, IndexInfo, RecordCompare, SeekKey, SeekOp,
    SeekResult, Value, ValueRef,
};
use crate::vdbe::Register;
use crate::{Pager, Result};
use std::cmp::Ordering;

/// A table's key for `rowid`: big-endian with the sign bit flipped, so the bytes order as the
/// integers do.
pub fn rowid_key(rowid: i64) -> [u8; 8] {
    let mut key = rowid.to_be_bytes();
    if let Some(top) = key.first_mut() {
        *top ^= 0x80;
    }
    key
}

/// The rowid a table's key holds.
pub fn key_rowid(key: &[u8]) -> Result<i64> {
    let mut bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| corrupt("a table's key not 8 bytes"))?;
    if let Some(top) = bytes.first_mut() {
        *top ^= 0x80;
    }
    Ok(i64::from_be_bytes(bytes))
}

fn corrupt(what: &str) -> LimboError {
    LimboError::Corrupt(format!("versioned cursor: {what}"))
}

/// How a tree's keys are ordered: by rowid, or by an index's columns and then the record's bytes.
/// Its [`KeyOrder::comparator`] orders the tree itself, which can't return an error: a record it
/// can't read is ordered by its bytes and marked, and the cursor's write that met it is refused
/// (`Corrupt`), as SQLite's record comparison notes a corrupt record in the key it compares.
#[derive(Clone)]
pub struct KeyOrder {
    index: Option<Arc<IndexInfo>>,
    unreadable: Arc<AtomicBool>,
}

impl KeyOrder {
    /// A table's: keys are [`rowid_key`]s.
    pub fn rowid() -> Self {
        Self {
            index: None,
            unreadable: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An index's, or a WITHOUT ROWID table's: keys are records.
    pub fn index(info: Arc<IndexInfo>) -> Self {
        Self {
            index: Some(info),
            unreadable: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The order of two keys of the tree.
    fn order(&self, a: &[u8], b: &[u8]) -> Result<Ordering> {
        let Some(info) = &self.index else {
            return Ok(a.cmp(b));
        };
        let columns = compare_immutable_iter(
            ImmutableRecordRef::from_bin_record(a).iter()?,
            ImmutableRecordRef::from_bin_record(b).iter()?,
            &info.key_info,
        )?;
        Ok(columns.then_with(|| a.cmp(b)))
    }

    /// Whether two records are equal on the index's columns.
    fn same_columns(info: &IndexInfo, a: &[u8], b: &[u8]) -> Result<bool> {
        Ok(compare_immutable_iter(
            ImmutableRecordRef::from_bin_record(a).iter()?,
            ImmutableRecordRef::from_bin_record(b).iter()?,
            &info.key_info,
        )? == Ordering::Equal)
    }

    /// The tree's comparison (`Database::open`'s): [`KeyOrder::order`], a record it can't read
    /// ordered by its bytes and marked.
    pub fn comparator(&self) -> Comparator {
        let order = self.clone();
        Box::new(move |a: &[u8], b: &[u8]| order.compare(a, b))
    }

    /// [`KeyOrder::order`], a record it can't read ordered by its bytes and marked.
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        match self.order(a, b) {
            Ok(o) => o,
            Err(_) => {
                self.unreadable.store(true, AtomicOrdering::Release);
                a.cmp(b)
            }
        }
    }

    /// Refuses once the tree's comparison has met a record it can't read.
    fn check(&self) -> Result<()> {
        if self.unreadable.load(AtomicOrdering::Acquire) {
            return Err(corrupt("a key in the tree isn't a readable record"));
        }
        Ok(())
    }

    /// Whether `key` comes before `point`.
    fn before(&self, point: &Point<'_>, key: &[u8]) -> Result<bool> {
        Ok(match point {
            Point::First => false,
            Point::Last => true,
            Point::Below(k) => self.order(key, k)? == Ordering::Less,
            Point::Above(k) => self.order(key, k)? != Ordering::Greater,
            Point::Probe {
                values,
                compare,
                tie,
            } => {
                let info = self
                    .index
                    .as_deref()
                    .ok_or_else(|| corrupt("a record's seek on a table's tree"))?;
                compare.compare_payload(key, values.iter(), info, *tie)? == Ordering::Less
            }
        })
    }
}

/// A point between keys that a move starts from.
enum Point<'a> {
    /// Before every key.
    First,
    /// Past every key.
    Last,
    /// Just short of this key.
    Below(&'a [u8]),
    /// Just past this key.
    Above(&'a [u8]),
    /// A seek's place among an index's records: past every record that compares below the seek's
    /// values, with records equal on them placed by the seek's tie (SQLite's `default_rc`).
    Probe {
        values: &'a [ValueRef<'a>],
        compare: RecordCompare,
        tie: Ordering,
    },
}

/// The rows of one leaf a cursor copied, as its snapshot sees them, in key order.
#[derive(Default)]
struct Rows {
    leaf: u64,
    version: u64,
    low: Option<Vec<u8>>,
    high: Option<Vec<u8>>,
    right: Option<u64>,
    /// Whether the copy runs to the leaf's last cell: not when copied going back from a point
    /// inside it.
    end: bool,
    bytes: Vec<u8>,
    /// Each row's start in `bytes`, its key's length and its value's length.
    at: Vec<(usize, usize, usize)>,
}

fn keep(slot: &mut Option<Vec<u8>>, from: Option<&[u8]>) {
    match (slot.as_mut(), from) {
        (Some(v), Some(b)) => {
            v.clear();
            v.extend_from_slice(b);
        }
        (None, Some(b)) => *slot = Some(b.to_vec()),
        (_, None) => *slot = None,
    }
}

impl Rows {
    fn len(&self) -> usize {
        self.at.len()
    }

    fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    fn row(&self, i: usize) -> Result<(&[u8], &[u8])> {
        let &(start, key, value) = self
            .at
            .get(i)
            .ok_or_else(|| corrupt("a row past the leaf's copy"))?;
        let mid = start.checked_add(key).ok_or(LimboError::IntegerOverflow)?;
        let end = mid.checked_add(value).ok_or(LimboError::IntegerOverflow)?;
        let k = self
            .bytes
            .get(start..mid)
            .ok_or_else(|| corrupt("a key past the copy"))?;
        let v = self
            .bytes
            .get(mid..end)
            .ok_or_else(|| corrupt("a value past the copy"))?;
        Ok((k, v))
    }

    fn key(&self, i: usize) -> Result<&[u8]> {
        self.row(i).map(|(k, _)| k)
    }

    /// Puts back the chunks of the copied rows that continue in the overflow tree (`AC-NS.2.4`), as
    /// `snap` sees them: after the copy, with the leaf's latch released.
    fn join_chunks(
        &mut self,
        db: &Database,
        d: &mut Descender,
        snap: &Snapshot,
        tree: u64,
    ) -> Result<()> {
        let mut long = false;
        for i in 0..self.len() {
            let (k, v) = self.row(i)?;
            long |= overflow::continues(k, v);
        }
        if !long {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(self.bytes.len());
        let mut at = Vec::with_capacity(self.at.len());
        for i in 0..self.len() {
            let (k, v) = self.row(i)?;
            let start = bytes.len();
            bytes.extend_from_slice(k);
            let mut value = v.to_vec();
            db.join(d, snap, tree, k, &mut value)?;
            bytes.extend_from_slice(&value);
            at.push((start, k.len(), value.len()));
        }
        self.bytes = bytes;
        self.at = at;
        Ok(())
    }

    /// Copies `leaf`'s rows on `after`'s side of the point it was read for, as `snap` sees them.
    fn copy(
        &mut self,
        vt: &VTree,
        db: &Database,
        snap: &Snapshot,
        leaf: Leaf<'_>,
        after: bool,
    ) -> Result<()> {
        self.leaf = leaf.no;
        self.version = leaf.version;
        keep(&mut self.low, leaf.low);
        keep(&mut self.high, super::page::high(leaf.page)?);
        self.right = super::page::right(leaf.page)?;
        self.bytes.clear();
        self.at.clear();
        let count = super::page::count(leaf.page)?;
        self.end = after || leaf.split >= count;
        let cells = if after {
            leaf.split..count
        } else {
            0..leaf.split
        };
        let (bytes, at) = (&mut self.bytes, &mut self.at);
        vt.leaf_rows(&db.v.clock, snap, leaf.page, cells, |key, value| {
            at.push((bytes.len(), key.len(), value.len()));
            bytes.extend_from_slice(key);
            bytes.extend_from_slice(value);
            Ok(())
        })
    }
}

/// A transaction the cursors of one connection write in: `None` once it has ended (committed or
/// rolled back while a cursor still held it), which a cursor's write is refused for.
pub type SharedTxn = Arc<Mutex<Option<Txn>>>;

/// What a cursor may do: read as a snapshot sees the tree, or also write in a transaction (whose
/// snapshot it reads as).
pub enum Access {
    Read(Snapshot),
    Write(SharedTxn),
}

fn ended() -> LimboError {
    LimboError::TxError("versioned cursor: its transaction has ended".into())
}

/// Where a cursor is.
#[derive(Debug)]
enum At {
    /// On no row: before the first, past the last, or not yet placed.
    Nowhere,
    /// On row `i` of its copy of a leaf.
    Row(usize),
    /// On the row under this key, put by the cursor since its leaf was copied.
    Put(Vec<u8>),
    /// Where the row under this key was, removed by the cursor: the next row is the first after
    /// it, the previous the last before it (SQLite's cursor after a delete).
    Gap(Vec<u8>),
}

/// The executor's cursor on one tree of a versioned database.
pub struct VersionedCursor {
    db: Arc<Database>,
    tree: u64,
    root_page: i64,
    order: KeyOrder,
    snap: Snapshot,
    txn: Option<SharedTxn>,
    /// The connection's pager, which the executor asks a cursor for; this storage keeps no page
    /// in it.
    pager: Arc<Pager>,
    d: Descender,
    rows: Rows,
    at: At,
    null_flag: bool,
    record: Option<ImmutableRecord>,
}

impl VersionedCursor {
    /// A cursor on tree `tree` of `db`, whose keys `order` orders (the order its comparison was
    /// opened with).
    pub fn new(
        db: Arc<Database>,
        tree: u64,
        order: KeyOrder,
        access: Access,
        pager: Arc<Pager>,
    ) -> Result<Self> {
        db.v.tree(tree)?;
        let root_page = i64::try_from(tree).map_err(|_| LimboError::IntegerOverflow)?;
        let (snap, txn) = match access {
            Access::Read(snap) => (snap, None),
            Access::Write(txn) => {
                let snap = txn.lock().as_ref().ok_or_else(ended)?.snapshot();
                (snap, Some(txn))
            }
        };
        Ok(Self {
            db,
            tree,
            root_page,
            order,
            snap,
            txn,
            pager,
            d: Descender::new(),
            rows: Rows::default(),
            at: At::Nowhere,
            null_flag: false,
            record: None,
        })
    }

    /// Whether the copy is still the leaf's rows.
    fn fresh(&self) -> Result<bool> {
        Ok(self.db.v.pool.version(self.rows.leaf)? == self.rows.version)
    }

    /// Places the cursor on the first row after `point`, reading from leaf `from` (with its lower
    /// bound) if given, else from the root; on no row if there is none. Whether it found one.
    fn first_after(
        &mut self,
        point: &Point<'_>,
        from: Option<(u64, Option<Vec<u8>>)>,
    ) -> Result<bool> {
        let Self {
            db,
            tree,
            order,
            snap,
            d,
            rows,
            ..
        } = self;
        let (db, order, snap): (&Database, &KeyOrder, &Snapshot) = (db, order, snap);
        let vt = db.v.tree(*tree)?;
        let pool = &db.v.pool;
        let mut before = |key: &[u8]| order.before(point, key);
        let (mut leaf, mut low) = match from {
            Some(from) => from,
            None => vt.tree().leaf_for(pool, d, &mut before)?,
        };
        let mut leaves = 0u64;
        loop {
            vt.tree().read_leaf(pool, leaf, low, &mut before, |l| {
                rows.copy(&vt, db, snap, l, true)
            })?;
            rows.join_chunks(db, d, snap, *tree)?;
            if !rows.is_empty() {
                self.at = At::Row(0);
                return Ok(true);
            }
            // Every row of the leaves right of this one is past the point, which is below this
            // leaf's upper bound.
            let Some(right) = rows.right else {
                self.at = At::Nowhere;
                return Ok(false);
            };
            leaves = leaves.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
            if leaves > pool.allocated() {
                return Err(corrupt("right links that don't end"));
            }
            leaf = right;
            low = rows.high.take();
        }
    }

    /// Places the cursor on the last row before `point`; on no row if there is none. Whether it
    /// found one.
    fn last_before(&mut self, point: &Point<'_>) -> Result<bool> {
        let Self {
            db,
            tree,
            order,
            snap,
            d,
            rows,
            ..
        } = self;
        let (db, order, snap): (&Database, &KeyOrder, &Snapshot) = (db, order, snap);
        let vt = db.v.tree(*tree)?;
        let pool = &db.v.pool;
        // The point, then just short of the lower bound of each leaf found with no row before it:
        // each lower than the last, so the walk ends within the leaves.
        let mut bound: Option<Vec<u8>> = None;
        let mut leaves = 0u64;
        loop {
            {
                let mut before = |key: &[u8]| match &bound {
                    None => order.before(point, key),
                    Some(b) => Ok(order.order(key, b)? == Ordering::Less),
                };
                let (leaf, low) = vt.tree().leaf_for(pool, d, &mut before)?;
                vt.tree().read_leaf(pool, leaf, low, &mut before, |l| {
                    rows.copy(&vt, db, snap, l, false)
                })?;
                rows.join_chunks(db, d, snap, *tree)?;
            }
            if let Some(last) = rows.len().checked_sub(1) {
                self.at = At::Row(last);
                return Ok(true);
            }
            let Some(low) = rows.low.take() else {
                self.at = At::Nowhere;
                return Ok(false);
            };
            leaves = leaves.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
            if leaves > pool.allocated() {
                return Err(corrupt("lower bounds that don't end"));
            }
            bound = Some(low);
        }
    }

    /// Finds the cursor's row again by its key where its copy is no longer the leaf's rows or it
    /// put the row since: on it if it is still there, else in its gap.
    fn settle(&mut self) -> Result<()> {
        let key = match std::mem::replace(&mut self.at, At::Nowhere) {
            At::Row(i) => {
                if self.fresh()? {
                    self.at = At::Row(i);
                    return Ok(());
                }
                self.rows.key(i)?.to_vec()
            }
            At::Put(key) => key,
            other => {
                self.at = other;
                return Ok(());
            }
        };
        let found = self.first_after(&Point::Below(&key), None)?;
        if !(found && self.rows.key(0)? == key.as_slice()) {
            self.at = At::Gap(key);
        }
        Ok(())
    }

    /// The key and value of the row the cursor is on, its copy current.
    fn current(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        self.settle()?;
        match self.at {
            At::Row(i) => self.rows.row(i).map(Some),
            _ => Ok(None),
        }
    }

    fn step_next(&mut self) -> Result<()> {
        self.record_stale();
        let key = match std::mem::replace(&mut self.at, At::Nowhere) {
            At::Nowhere => return Ok(()),
            At::Row(i) => {
                if self.fresh()? {
                    let next = i.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
                    if next < self.rows.len() {
                        self.at = At::Row(next);
                        return Ok(());
                    }
                    // Past the copy's last row, where the copy stops short of the leaf's end
                    // (copied going back): the rest of this leaf, then on (T-NS.1.1).
                    if !self.rows.end {
                        let key = self.rows.key(i)?.to_vec();
                        let from = (self.rows.leaf, self.rows.low.clone());
                        return self
                            .first_after(&Point::Above(&key), Some(from))
                            .map(|_| ());
                    }
                    // Else on to the leaf right of it, whose rows are all past the copy's upper
                    // bound.
                    let Some(right) = self.rows.right else {
                        return Ok(());
                    };
                    let key = self.rows.key(i)?.to_vec();
                    let low = self.rows.high.take();
                    return self
                        .first_after(&Point::Above(&key), Some((right, low)))
                        .map(|_| ());
                }
                self.rows.key(i)?.to_vec()
            }
            At::Put(key) | At::Gap(key) => key,
        };
        self.first_after(&Point::Above(&key), None).map(|_| ())
    }

    fn step_prev(&mut self) -> Result<()> {
        self.record_stale();
        let key = match std::mem::replace(&mut self.at, At::Nowhere) {
            At::Nowhere => return Ok(()),
            At::Row(i) => {
                if self.fresh()? {
                    if let Some(prev) = i.checked_sub(1) {
                        self.at = At::Row(prev);
                        return Ok(());
                    }
                }
                self.rows.key(i)?.to_vec()
            }
            At::Put(key) | At::Gap(key) => key,
        };
        self.last_before(&Point::Below(&key)).map(|_| ())
    }

    fn record_stale(&mut self) {
        if let Some(record) = self.record.as_mut() {
            record.invalidate();
        }
    }

    /// Places the cursor for `op` at `point`: on the first row after it going forward, the last
    /// before it going back. Whether it found one.
    fn place(&mut self, point: &Point<'_>, op: SeekOp) -> Result<bool> {
        self.null_flag = false;
        self.record_stale();
        match op {
            SeekOp::GE { .. } | SeekOp::GT => self.first_after(point, None),
            SeekOp::LE { .. } | SeekOp::LT => self.last_before(point),
        }
    }

    fn seek_rowid(&mut self, rowid: i64, op: SeekOp) -> Result<SeekResult> {
        if self.order.index.is_some() {
            return Err(corrupt("a rowid's seek on an index's tree"));
        }
        let key = rowid_key(rowid);
        let point = match op {
            SeekOp::GE { .. } | SeekOp::LT => Point::Below(&key),
            SeekOp::GT | SeekOp::LE { .. } => Point::Above(&key),
        };
        let found = self.place(&point, op)?;
        let hit = match (op, &self.at) {
            (SeekOp::GE { eq_only: true } | SeekOp::LE { eq_only: true }, At::Row(i)) => {
                self.rows.key(*i)? == key.as_slice()
            }
            _ => found,
        };
        Ok(if hit {
            SeekResult::Found
        } else {
            SeekResult::NotFound
        })
    }

    fn seek_values(&mut self, values: &[ValueRef<'_>], op: SeekOp) -> Result<SeekResult> {
        let info = self
            .order
            .index
            .clone()
            .ok_or_else(|| corrupt("a record's seek on a table's tree"))?;
        let compare = find_compare(values.iter().peekable(), &info);
        // An exact seek is placed as its inexact kind is (the first record at least the key, the
        // last at most it), then asked whether that record equals the key.
        let placing = match op {
            SeekOp::GE { .. } => SeekOp::GE { eq_only: false },
            SeekOp::LE { .. } => SeekOp::LE { eq_only: false },
            other => other,
        };
        let point = Point::Probe {
            values,
            compare,
            tie: get_tie_breaker_from_seek_op(placing),
        };
        let found = self.place(&point, op)?;
        let hit = match (op, &self.at) {
            (SeekOp::GE { eq_only: true } | SeekOp::LE { eq_only: true }, At::Row(i)) => {
                compare.compare_payload(
                    self.rows.key(*i)?,
                    values.iter(),
                    &info,
                    Ordering::Equal,
                )? == Ordering::Equal
            }
            _ => found,
        };
        Ok(if hit {
            SeekResult::Found
        } else {
            SeekResult::NotFound
        })
    }

    /// The transaction the cursor writes in: refused (`ReadOnly`) for a reading cursor.
    fn txn(&self) -> Result<SharedTxn> {
        self.txn.clone().ok_or(LimboError::ReadOnly)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        let txn = self.txn()?;
        let mut txn = txn.lock();
        let txn = txn.as_mut().ok_or_else(ended)?;
        self.db.put(&mut self.d, txn, self.tree, key, value)?;
        self.order.check()
    }

    fn remove(&mut self, key: &[u8]) -> Result<()> {
        let txn = self.txn()?;
        let mut txn = txn.lock();
        let txn = txn.as_mut().ok_or_else(ended)?;
        self.db.remove(&mut self.d, txn, self.tree, key)?;
        self.order.check()
    }

    fn insert_key(&mut self, key: &BTreeKey<'_>) -> Result<()> {
        self.record_stale();
        let (k, value): (Vec<u8>, &[u8]) = match (key, self.order.index.clone()) {
            (BTreeKey::TableRowId((rowid, record)), None) => {
                let record = record.ok_or_else(|| {
                    LimboError::InternalError("a table's row inserted without its record".into())
                })?;
                (rowid_key(*rowid).to_vec(), record.get_payload())
            }
            (BTreeKey::IndexKey(record), Some(info)) => {
                let new = record.get_payload();
                // A record equal on the index's columns to the one the cursor is on (where the
                // executor's seek placed it) replaces it: by removal first if its bytes differ,
                // as the tree orders such records apart (the module's note).
                let old = match self.current()? {
                    Some((old, _)) if old != new && KeyOrder::same_columns(&info, old, new)? => {
                        Some(old.to_vec())
                    }
                    _ => None,
                };
                if let Some(old) = old {
                    self.remove(&old)?;
                }
                (new.to_vec(), &[])
            }
            _ => {
                return Err(LimboError::InternalError(
                    "a key of the other kind of tree inserted".into(),
                ))
            }
        };
        self.put(&k, value)?;
        self.at = At::Put(k);
        Ok(())
    }

    fn delete_row(&mut self) -> Result<()> {
        self.record_stale();
        let key = match self.current()? {
            Some((key, _)) => key.to_vec(),
            None => {
                return Err(LimboError::InternalError(
                    "a delete with the cursor on no row".into(),
                ))
            }
        };
        self.remove(&key)?;
        self.at = At::Gap(key);
        Ok(())
    }

    /// The rows the snapshot sees, a leaf's copy at a time.
    fn count_rows(&mut self) -> Result<usize> {
        let mut n = 0usize;
        let mut found = self.first_after(&Point::First, None)?;
        while found {
            n = n
                .checked_add(self.rows.len())
                .ok_or(LimboError::IntegerOverflow)?;
            let last = self
                .rows
                .len()
                .checked_sub(1)
                .ok_or(LimboError::IntegerOverflow)?;
            let key = self.rows.key(last)?.to_vec();
            found = match self.rows.right {
                Some(right) => {
                    let low = self.rows.high.take();
                    self.first_after(&Point::Above(&key), Some((right, low)))?
                }
                None => false,
            };
        }
        self.at = At::Nowhere;
        Ok(n)
    }

    /// Removes every row the snapshot sees, in the cursor's transaction: how many.
    pub(crate) fn clear_rows(&mut self) -> Result<u64> {
        self.record_stale();
        let mut n = 0u64;
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut found = self.first_after(&Point::First, None)?;
        while found {
            keys.clear();
            for i in 0..self.rows.len() {
                keys.push(self.rows.key(i)?.to_vec());
            }
            for key in &keys {
                self.remove(key)?;
                n = n.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
            }
            let Some(last) = keys.last() else { break };
            found = self.first_after(&Point::Above(last), None)?;
        }
        self.at = At::Nowhere;
        Ok(n)
    }

    fn rowid_now(&mut self) -> Result<Option<i64>> {
        if self.null_flag {
            return Ok(None);
        }
        let is_index = self.order.index.is_some();
        let Some((key, _)) = self.current()? else {
            return Ok(None);
        };
        if !is_index {
            return key_rowid(key).map(Some);
        }
        match ImmutableRecordRef::from_bin_record(key).last_value() {
            Some(Ok(ValueRef::Numeric(Numeric::Integer(rowid)))) => Ok(Some(rowid)),
            Some(Err(e)) => Err(e),
            _ => Err(corrupt("an index record whose last column isn't a rowid")),
        }
    }

    fn payload_now(&mut self) -> Result<Option<&[u8]>> {
        if self.null_flag {
            return Ok(None);
        }
        let is_index = self.order.index.is_some();
        Ok(self
            .current()?
            .map(|(key, value)| if is_index { key } else { value }))
    }

    fn record_now(&mut self) -> Result<Option<&ImmutableRecord>> {
        if self.null_flag {
            return Ok(None);
        }
        let current = self.record.as_ref().is_some_and(|r| !r.is_invalidated());
        if !current {
            let is_index = self.order.index.is_some();
            let mut record = match self.record.take() {
                Some(r) => r,
                None => ImmutableRecord::new(0)?,
            };
            record.invalidate();
            if let Some((key, value)) = self.current()? {
                record.start_serialization(if is_index { key } else { value })?;
            }
            let empty = record.is_invalidated();
            self.record = Some(record);
            if empty {
                return Ok(None);
            }
        }
        Ok(self.record.as_ref())
    }
}

fn done<T>(r: Result<T>) -> IOResultOr<T> {
    Ok(IOResult::Done(r?))
}

impl CursorTrait for VersionedCursor {
    fn last(&mut self) -> IOResultOr<()> {
        done(
            self.place(&Point::Last, SeekOp::LE { eq_only: false })
                .map(|_| ()),
        )
    }

    fn next(&mut self) -> IOResultOr<()> {
        done(self.step_next())
    }

    fn prev(&mut self) -> IOResultOr<()> {
        done(self.step_prev())
    }

    fn rowid(&mut self) -> IOResultOr<Option<i64>> {
        done(self.rowid_now())
    }

    fn record(&mut self) -> IOResultOr<Option<&ImmutableRecord>> {
        Ok(IOResult::Done(self.record_now()?))
    }

    fn record_payload(&mut self) -> IOResultOr<Option<&[u8]>> {
        Ok(IOResult::Done(self.payload_now()?))
    }

    fn seek(&mut self, key: SeekKey<'_>, op: SeekOp) -> IOResultOr<SeekResult> {
        match key {
            SeekKey::TableRowId(rowid) => done(self.seek_rowid(rowid, op)),
            SeekKey::IndexKey(record) => {
                let values = record.get_values()?;
                done(self.seek_values(&values, op))
            }
        }
    }

    fn seek_unpacked(&mut self, registers: &[Register], op: SeekOp) -> IOResultOr<SeekResult> {
        let values: Vec<ValueRef<'_>> = registers
            .iter()
            .map(|r| r.get_value().as_value_ref())
            .collect();
        done(self.seek_values(&values, op))
    }

    fn insert(&mut self, key: &BTreeKey) -> IOResultOr<()> {
        done(self.insert_key(key))
    }

    fn delete(&mut self) -> IOResultOr<()> {
        done(self.delete_row())
    }

    fn set_null_flag(&mut self, flag: bool) {
        self.null_flag = flag;
    }

    fn get_null_flag(&self) -> bool {
        self.null_flag
    }

    fn exists(&mut self, key: &Value) -> IOResultOr<bool> {
        let Value::Numeric(Numeric::Integer(rowid)) = key else {
            return Err(
                LimboError::InternalError("a table's rowid that isn't an integer".into()).into(),
            );
        };
        let found = self.seek_rowid(*rowid, SeekOp::GE { eq_only: true })?;
        Ok(IOResult::Done(found == SeekResult::Found))
    }

    fn clear_btree(&mut self) -> IOResultOr<u64> {
        done(self.clear_rows())
    }

    fn btree_destroy(&mut self) -> IOResultOr<Option<usize>> {
        Err(LimboError::InternalError(
            "versioned storage: dropping a tree comes with its catalog (G7 step 2)".into(),
        )
        .into())
    }

    fn count(&mut self) -> IOResultOr<usize> {
        done(self.count_rows())
    }

    fn is_empty(&self) -> bool {
        !self.has_record()
    }

    fn root_page(&self) -> i64 {
        self.root_page
    }

    fn rewind(&mut self) -> IOResultOr<()> {
        done(
            self.place(&Point::First, SeekOp::GE { eq_only: false })
                .map(|_| ()),
        )
    }

    fn has_record(&self) -> bool {
        matches!(self.at, At::Row(_) | At::Put(_))
    }

    fn set_has_record(&mut self, has_record: bool) {
        // Only a cursor's own moves put it on a row; the executor clears it.
        if !has_record {
            self.at = At::Nowhere;
        }
    }

    fn index_info(&self) -> Option<&Arc<IndexInfo>> {
        self.order.index.as_ref()
    }

    fn seek_end(&mut self) -> IOResultOr<()> {
        // An append follows: the cursor puts by key wherever it is.
        self.record_stale();
        self.at = At::Nowhere;
        Ok(IOResult::Done(()))
    }

    fn seek_to_last(&mut self) -> IOResultOr<()> {
        self.last()
    }

    fn invalidate_record(&mut self) {
        self.record_stale();
    }

    fn has_rowid(&self) -> bool {
        self.order.index.as_ref().is_none_or(|info| info.has_rowid)
    }

    fn get_pager(&self) -> Arc<Pager> {
        self.pager.clone()
    }

    fn get_skip_advance(&self) -> bool {
        // A move after a delete starts from the removed row's key (`At::Gap`): nothing to skip.
        false
    }
}
