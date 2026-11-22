//! Amphora: the cursor conformance suite (kiln plan 01a16b0f46fa72e384b8dd0065dc94f9, step 1:
//! `AC-NS.1.1`, `T-NS.1.1`): random sequences of the executor's cursor operations driven through
//! `CursorTrait` on the current engine's B-tree cursor and on the versioned storage's, each step
//! compared: the seek's answer, whether the cursor is on a row, and that row's rowid and record.
//!
//! The sequences keep to what the executor does with a cursor: an insert follows a seek for its
//! key (`op_insert`, `op_idx_insert`), a delete is of the row the cursor is on, a step forward or
//! back starts from a row or from a write just made, and a seek's `TryAdvance` is followed by the
//! step its direction names (`seek_internal`). After an exact seek that finds nothing only the
//! answer is compared: the executor reads no row there.
//!
//! After a delete no position is compared until the next `next` or `prev`: SQLite documents the
//! cursor's place after `sqlite3BtreeDelete` as only good for the step that follows (the
//! executor's `OP_Delete` is followed by `OP_Next`/`OP_Prev`, never by a read or a second
//! delete). That step is checked against a plain ordered model of the rows too: the row after the
//! deleted one, or the one before it. Every step is: the versioned cursor must match the model;
//! the reference (btree.rs) is known to leave it on `prev` (after a delete, and from the first
//! row) and on an index's `next` from the last row, so there the model alone decides.

use super::cursor::{Access, KeyOrder, VersionedCursor};
use super::disk::Simulated;
use super::durable::Database as VDatabase;
use crate::io::{MemoryIO, IO};
use crate::schema::{Index, IndexColumn};
use crate::storage::btree::{BTreeCursor, BTreeKey, CursorTrait};
use crate::storage::pager::CreateBTreeFlags;
use crate::sync::{Arc, Mutex};
use crate::translate::collate::CollationSeq;
use crate::types::{
    compare_immutable_iter, ImmutableRecord, ImmutableRecordRef, IndexInfo, SeekKey, SeekOp,
    SeekResult,
};
use crate::util::IOExt;
use crate::vdbe::Register;
use crate::{ConnectOpts, Connection, Database, Pager, SqliteDialect, Value, WalAutoActions};
use amphora_engine_parser::ast::SortOrder;

/// splitmix64: a seeded sequence, so a failing seed replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// A database of the current engine in memory with a write transaction open, as the btree tests
/// make one (`empty_btree`).
fn engine() -> (Arc<Pager>, Arc<Database>, Arc<Connection>) {
    #[allow(clippy::arc_with_non_send_sync)]
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    let pager = conn.pager.load().clone();
    pager.begin_read_tx().unwrap();
    pager
        .io
        .block(|| pager.begin_write_tx(WalAutoActions::all_enabled()))
        .unwrap();
    pager
        .io
        .block(|| pager.commit_tx(&conn, conn.get_sync_mode(), true))
        .unwrap();
    pager.begin_read_tx().unwrap();
    pager
        .io
        .block(|| pager.begin_write_tx(WalAutoActions::all_enabled()))
        .unwrap();
    (pager, db, conn)
}

/// The kind of tree a run drives.
#[derive(Clone, Copy, Debug)]
enum Kind {
    Table,
    /// An index on (a, b) with the rowid last: `a` descending if `desc`, `b` under NOCASE if
    /// `nocase`.
    Index {
        desc: bool,
        nocase: bool,
    },
}

fn index_def(desc: bool, nocase: bool) -> Index {
    let mut columns = IndexColumn::new_many(["a", "b"]);
    if let Some(a) = columns.get_mut(0) {
        a.order = if desc {
            SortOrder::Desc
        } else {
            SortOrder::Asc
        };
    }
    if let Some(b) = columns.get_mut(1) {
        b.collation = if nocase {
            Some(CollationSeq::NoCase)
        } else {
            None
        };
    }
    Index {
        name: "conformance".to_string(),
        table_name: "t".to_string(),
        root_page: 0,
        columns,
        unique: false,
        ephemeral: false,
        has_rowid: true,
        where_clause: None,
        index_method: None,
        on_conflict: None,
        state: crate::schema::IndexState::Public,
    }
}

/// The two cursors on empty trees of `kind`, and the pager driving the current engine's.
/// The current engine's database and connection come back too: they must outlive its cursor.
#[allow(clippy::type_complexity)]
fn pair(
    kind: Kind,
    seed: u64,
) -> (
    Box<dyn CursorTrait>,
    Box<dyn CursorTrait>,
    Arc<Pager>,
    (Arc<Database>, Arc<Connection>),
) {
    let (pager, db, conn) = engine();
    let (btree, order): (Box<dyn CursorTrait>, KeyOrder) = match kind {
        Kind::Table => {
            let root = pager
                .io
                .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
                .unwrap();
            (
                Box::new(BTreeCursor::new_table(pager.clone(), i64::from(root), 3)),
                KeyOrder::rowid(),
            )
        }
        Kind::Index { desc, nocase } => {
            let root = pager
                .io
                .block(|| pager.btree_create(&CreateBTreeFlags::new_index()))
                .unwrap();
            let index = index_def(desc, nocase);
            let info = Arc::new(IndexInfo::new_from_index(&index).unwrap());
            (
                Box::new(
                    BTreeCursor::new_index(pager.clone(), i64::from(root), &index, 3).unwrap(),
                ),
                KeyOrder::index(info),
            )
        }
    };
    let vdb = Arc::new(
        VDatabase::open(Arc::new(Simulated::new(seed, 1).unwrap()), 4_000, 1, || {
            order.comparator()
        })
        .unwrap(),
    );
    let txn = Arc::new(Mutex::new(Some(vdb.begin().unwrap())));
    let versioned = VersionedCursor::new(vdb, 0, order, Access::Write(txn), pager.clone()).unwrap();
    (btree, Box::new(versioned), pager, (db, conn))
}

/// What the executor can see of a cursor: whether it is on a row, and that row's rowid and record.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    row: Option<(Option<i64>, Vec<u8>)>,
}

fn seen(c: &mut dyn CursorTrait, pager: &Pager) -> Seen {
    if !c.has_record() {
        return Seen { row: None };
    }
    let rowid = pager.io.block(|| c.rowid()).unwrap();
    let record = pager
        .io
        .block(|| {
            c.record()
                .map(|r| r.map(|r| r.map(|r| r.get_payload().to_vec())))
        })
        .unwrap();
    Seen {
        row: record.map(|r| (rowid, r)),
    }
}

/// A seek as the executor makes it (`seek_internal`): `TryAdvance` followed by the step its
/// direction names. Whether it found a row.
fn seek(c: &mut dyn CursorTrait, pager: &Pager, key: &Key, op: SeekOp) -> bool {
    let result = pager
        .io
        .block(|| match key {
            Key::Rowid(r) => c.seek(SeekKey::TableRowId(*r), op),
            Key::Record(rec) => c.seek(SeekKey::IndexKey(rec.as_record_ref()), op),
        })
        .unwrap();
    match result {
        SeekResult::Found => true,
        SeekResult::NotFound => false,
        SeekResult::TryAdvance => {
            match op {
                SeekOp::GE { .. } | SeekOp::GT => pager.io.block(|| c.next()).unwrap(),
                SeekOp::LE { .. } | SeekOp::LT => pager.io.block(|| c.prev()).unwrap(),
            }
            c.has_record()
        }
    }
}

/// A key as a failure names it: a rowid, or a record's bytes.
fn described(key: &Key) -> String {
    match key {
        Key::Rowid(r) => r.to_string(),
        Key::Record(rec) => format!("{:?}", rec.get_payload()),
    }
}

enum Key {
    Rowid(i64),
    Record(ImmutableRecord),
}

fn record(values: &[Value]) -> ImmutableRecord {
    let regs: Vec<Register> = values.iter().cloned().map(Register::Value).collect();
    ImmutableRecord::from_registers(&regs, regs.len()).unwrap()
}

fn text(rng: &mut Rng, len: u64) -> String {
    // A small alphabet in both cases, so NOCASE finds records equal on their columns.
    (0..len)
        .map(|_| {
            let c = b"abcABC"[usize::try_from(rng.below(6)).unwrap()];
            char::from(c)
        })
        .collect()
}

fn value(rng: &mut Rng) -> Value {
    match rng.below(5) {
        0 => Value::Null,
        1 => Value::from_i64(i64::try_from(rng.below(1_000)).unwrap() - 500),
        2 => Value::from_f64(f64::from(u32::try_from(rng.below(1_000)).unwrap()) / 7.0),
        3 => {
            let len = rng.below(300);
            Value::from_text(text(rng, len))
        }
        _ => {
            // Now and then one past a page, its record kept in overflow on both storages
            // (AC-NS.2.4).
            let len = if rng.chance(10) {
                usize::try_from(4_000 + rng.below(9_000)).unwrap()
            } else {
                usize::try_from(rng.below(200)).unwrap()
            };
            Value::from_slice(&vec![u8::try_from(rng.below(256)).unwrap(); len]).unwrap()
        }
    }
}

fn rowid(rng: &mut Rng) -> i64 {
    match rng.below(50) {
        0 => i64::MIN,
        1 => i64::MAX,
        2 => 0,
        _ => i64::try_from(rng.below(1_400)).unwrap() - 200,
    }
}

/// An index's column values: `a` from a few integers (and NULL), `b` a short text.
fn index_values(rng: &mut Rng) -> (Value, Value) {
    let a = if rng.chance(5) {
        Value::Null
    } else {
        Value::from_i64(i64::try_from(rng.below(40)).unwrap())
    };
    let len = 1 + rng.below(3);
    (a, Value::from_text(text(rng, len)))
}

fn seek_op(rng: &mut Rng) -> SeekOp {
    match rng.below(6) {
        0 => SeekOp::GE { eq_only: false },
        1 => SeekOp::GE { eq_only: true },
        2 => SeekOp::GT,
        3 => SeekOp::LE { eq_only: false },
        4 => SeekOp::LE { eq_only: true },
        _ => SeekOp::LT,
    }
}

/// Every row of a cursor from the first, in order.
fn all(c: &mut dyn CursorTrait, pager: &Pager) -> Vec<Seen> {
    let mut out = Vec::new();
    pager.io.block(|| c.rewind()).unwrap();
    while c.has_record() {
        out.push(seen(c, pager));
        pager.io.block(|| c.next()).unwrap();
    }
    out
}

/// The rows of a tree in order, kept apart from both storages: each row's key (a table's
/// [`rowid_key`](super::cursor::rowid_key), an index's record) and its record.
struct Model {
    /// The index whose columns order the rows, or `None` for a table (ordered by key bytes).
    index: Option<Arc<IndexInfo>>,
    rows: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Model {
    fn new(kind: Kind) -> Self {
        let index = match kind {
            Kind::Table => None,
            Kind::Index { desc, nocase } => Some(Arc::new(
                IndexInfo::new_from_index(&index_def(desc, nocase)).unwrap(),
            )),
        };
        Self {
            index,
            rows: Vec::new(),
        }
    }

    fn cmp(&self, a: &[u8], b: &[u8]) -> std::cmp::Ordering {
        match &self.index {
            None => a.cmp(b),
            // Equal on every column, the rowid among them, is the same row: an insert of it
            // replaces the old one, as both storages do.
            Some(info) => compare_immutable_iter(
                ImmutableRecordRef::from_bin_record(a).iter().unwrap(),
                ImmutableRecordRef::from_bin_record(b).iter().unwrap(),
                &info.key_info,
            )
            .unwrap(),
        }
    }

    fn find(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.rows.binary_search_by(|(k, _)| self.cmp(k, key))
    }

    fn insert(&mut self, key: Vec<u8>, record: Vec<u8>) {
        match self.find(&key) {
            Ok(i) => self.rows[i] = (key, record),
            Err(i) => self.rows.insert(i, (key, record)),
        }
    }

    fn remove(&mut self, key: &[u8]) {
        let i = self.find(key).expect("the model holds the row deleted");
        self.rows.remove(i);
    }

    /// The row a step from `key` (a row, or one just deleted) lands on: the first after it, or
    /// the last before it.
    fn step_from(&self, key: &[u8], forward: bool) -> Option<&[u8]> {
        let (i, held) = match self.find(key) {
            Ok(i) => (i, true),
            Err(i) => (i, false),
        };
        let at = match (forward, held) {
            (true, true) => i.checked_add(1),
            (true, false) => Some(i),
            (false, _) => i.checked_sub(1),
        };
        at.and_then(|i| self.rows.get(i)).map(|(_, r)| r.as_slice())
    }

    /// The key of the row a cursor shows.
    fn key_of(&self, row: &Seen) -> Vec<u8> {
        let (rowid, record) = row.row.as_ref().expect("a row");
        match self.index {
            None => super::cursor::rowid_key(rowid.expect("a table's row has a rowid")).to_vec(),
            Some(_) => record.clone(),
        }
    }
}

/// Where the driven cursors stand, as the executor would know it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// Nowhere a step or delete may follow: not on a row, or after a write that moved them.
    Unplaced,
    /// On the same row (compared).
    OnRow,
    /// Just after a delete: only a step may follow.
    Deleted,
}

/// `OnRow` with `current` the row's key if the cursors show a row, else `Unplaced`.
fn on(row: &Seen, model: &Model, current: &mut Vec<u8>) -> Place {
    if row.row.is_none() {
        return Place::Unplaced;
    }
    *current = model.key_of(row);
    Place::OnRow
}

/// One run of `ops` random operations on both cursors of `kind`, compared after each.
fn run(kind: Kind, seed: u64, ops: usize) {
    let mut rng = Rng(seed);
    let (mut b, mut v, pager, _keep) = pair(kind, seed);
    let (b, v) = (b.as_mut(), v.as_mut());
    let mut model = Model::new(kind);
    let mut place = Place::Unplaced;
    // The key of the row the cursors are on, or of the row just deleted.
    let mut current: Vec<u8> = Vec::new();
    // Steps where the reference left the model's row (btree.rs), counted so a run names them.
    let mut reference_diverged = 0usize;
    let mut next_rowid = 1i64;
    // The operations so far, the last few of which a failure names.
    let mut trail: Vec<String> = Vec::new();
    for step in 0..ops {
        let at = format!(
            "{kind:?} seed {seed} step {step} after {:?}",
            trail.get(trail.len().saturating_sub(8)..)
        );
        let roll = rng.below(100);
        if roll < 35 {
            // Insert: a seek for the key, then the insert at it.
            let (key, bkey_record) = match kind {
                Kind::Table => {
                    let r = rowid(&mut rng);
                    let n = 1 + rng.below(3);
                    let vals: Vec<Value> = (0..n).map(|_| value(&mut rng)).collect();
                    (Key::Rowid(r), record(&vals))
                }
                Kind::Index { .. } => {
                    let (a, bv) = index_values(&mut rng);
                    // A rowid seen before now and then: records equal on every column but the
                    // spelling NOCASE ignores.
                    let r = if rng.chance(20) && next_rowid > 1 {
                        1 + i64::try_from(rng.below(u64::try_from(next_rowid - 1).unwrap()))
                            .unwrap()
                    } else {
                        next_rowid += 1;
                        next_rowid
                    };
                    let vals = [a, bv, Value::from_i64(r)];
                    (Key::Record(record(&vals)), record(&vals))
                }
            };
            let op = SeekOp::GE { eq_only: true };
            trail.push(format!("insert {}", described(&key)));
            let fb = seek(b, &pager, &key, op);
            let fv = seek(v, &pager, &key, op);
            assert_eq!(fb, fv, "{at}: the insert's seek");
            let bkey = match &key {
                Key::Rowid(r) => BTreeKey::new_table_rowid(*r, Some(&bkey_record)),
                Key::Record(_) => BTreeKey::new_index_key(bkey_record.as_record_ref()),
            };
            let model_key = match &key {
                Key::Rowid(r) => super::cursor::rowid_key(*r).to_vec(),
                Key::Record(rec) => rec.get_payload().to_vec(),
            };
            model.insert(model_key, bkey_record.get_payload().to_vec());
            pager.io.block(|| b.insert(&bkey)).unwrap();
            pager.io.block(|| v.insert(&bkey)).unwrap();
            // sqlite3BtreeInsert leaves the cursor at an arbitrary place unless told to save its
            // position (BTREE_SAVEPOSITION), and the executor seeks before it reads or steps
            // again: no position is compared until the next placing move (T-NS.1.1).
            place = Place::Unplaced;
        } else if roll < 43 {
            if place != Place::OnRow {
                continue;
            }
            let row = seen(b, &pager);
            assert_eq!(row, seen(v, &pager), "{at}: the row deleted");
            trail.push("delete".into());
            pager.io.block(|| b.delete()).unwrap();
            pager.io.block(|| v.delete()).unwrap();
            current = model.key_of(&row);
            model.remove(&current);
            place = Place::Deleted;
        } else if roll < 63 {
            if place == Place::Unplaced {
                continue;
            }
            let forward = rng.chance(70);
            let name = if forward { "next" } else { "prev" };
            trail.push(name.into());
            if forward {
                pager.io.block(|| b.next()).unwrap();
                pager.io.block(|| v.next()).unwrap();
            } else {
                pager.io.block(|| b.prev()).unwrap();
                pager.io.block(|| v.prev()).unwrap();
            }
            let (sb, sv) = (seen(b, &pager), seen(v, &pager));
            let want = model.step_from(&current, forward);
            let record = |s: &Seen| s.row.as_ref().map(|(_, r)| r.clone());
            assert_eq!(
                record(&sv).as_deref(),
                want,
                "{at}: {name}, against the ordered model"
            );
            if record(&sb).as_deref() != want {
                // The reference's own divergences, known in these cases only (btree.rs, shared
                // engine code this suite doesn't change): its `prev` after a delete can land
                // elsewhere than the row before the deleted one, and from the first row on a
                // row; on an index its `next` from the last row can land on a row. Any other
                // step that leaves the model fails the run. Both cursors are placed afresh
                // before anything is compared again.
                assert!(
                    !forward || want.is_none(),
                    "{at}: the reference's {name} left the ordered model: reference {sb:?}, \
                     versioned {sv:?}"
                );
                eprintln!(
                    "{at}: reference {name} from {current:?} (deleted: {}) landed on {:?}, the \
                     model's row {want:?}",
                    place == Place::Deleted,
                    record(&sb)
                );
                reference_diverged += 1;
                place = Place::Unplaced;
                continue;
            }
            assert_eq!(sb, sv, "{at}: {name}");
            place = on(&sb, &model, &mut current);
        } else if roll < 88 {
            let op = seek_op(&mut rng);
            let key = match kind {
                Kind::Table => Key::Rowid(rowid(&mut rng)),
                Kind::Index { .. } => {
                    let (a, bv) = index_values(&mut rng);
                    // A prefix of the index's columns, or every one with a rowid.
                    let vals = match rng.below(3) {
                        0 => vec![a],
                        1 => vec![a, bv],
                        _ => vec![
                            a,
                            bv,
                            Value::from_i64(
                                i64::try_from(rng.below(u64::try_from(next_rowid).unwrap() + 1))
                                    .unwrap(),
                            ),
                        ],
                    };
                    Key::Record(record(&vals))
                }
            };
            trail.push(format!("seek {op:?} {}", described(&key)));
            let fb = seek(b, &pager, &key, op);
            let fv = seek(v, &pager, &key, op);
            assert_eq!(fb, fv, "{at}: seek {op:?}");
            let exact = matches!(
                op,
                SeekOp::GE { eq_only: true } | SeekOp::LE { eq_only: true }
            );
            if fb || !exact {
                assert_eq!(
                    seen(b, &pager),
                    seen(v, &pager),
                    "{at}: the row seek {op:?} found"
                );
            }
            place = if fb {
                on(&seen(v, &pager), &model, &mut current)
            } else {
                Place::Unplaced
            };
        } else if roll < 93 {
            trail.push("rewind".into());
            pager.io.block(|| b.rewind()).unwrap();
            pager.io.block(|| v.rewind()).unwrap();
            let (sb, sv) = (seen(b, &pager), seen(v, &pager));
            assert_eq!(sb, sv, "{at}: rewind");
            place = on(&sb, &model, &mut current);
        } else if roll < 98 {
            trail.push("last".into());
            pager.io.block(|| b.last()).unwrap();
            pager.io.block(|| v.last()).unwrap();
            let (sb, sv) = (seen(b, &pager), seen(v, &pager));
            assert_eq!(sb, sv, "{at}: last");
            place = on(&sb, &model, &mut current);
        } else {
            trail.push("count".into());
            let nb = pager.io.block(|| b.count()).unwrap();
            let nv = pager.io.block(|| v.count()).unwrap();
            assert_eq!(nb, nv, "{at}: count");
            place = Place::Unplaced;
        }
    }
    let (rows_b, rows_v) = (all(b, &pager), all(v, &pager));
    assert_eq!(rows_b, rows_v, "{kind:?} seed {seed}: every row at the end");
    let records: Vec<&[u8]> = rows_v
        .iter()
        .filter_map(|s| s.row.as_ref().map(|(_, r)| r.as_slice()))
        .collect();
    let modelled: Vec<&[u8]> = model.rows.iter().map(|(_, r)| r.as_slice()).collect();
    assert_eq!(
        records, modelled,
        "{kind:?} seed {seed}: the model's rows at the end"
    );
    if reference_diverged > 0 {
        eprintln!(
            "{kind:?} seed {seed}: the reference's step left the ordered model \
             {reference_diverged} times (versioned matched it)"
        );
    }
    assert!(
        rows_b.len() > 100,
        "{kind:?} seed {seed}: a run that left {} rows tests little",
        rows_b.len()
    );
}

/// Seeds a run takes: `AMPHORA_CURSOR_SEEDS` of them if set (the coordinator's long run), else a
/// handful.
fn seeds() -> u64 {
    std::env::var("AMPHORA_CURSOR_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4)
}

/// Do: 2,000 random operations (inserts after their seeks, deletes, steps, seeks of every kind,
/// rewinds, lasts, counts) on a table's tree through both storages' cursors, for each seed, some
/// records past a page. Expect: identical seek answers, rows, rowids and records after every step,
/// and the same rows at the end (T-NS.1.1, AC-NS.1.1, AC-NS.2.4).
#[test]
fn ns_1_1_table_cursors_agree_step_by_step() {
    for seed in 0..seeds() {
        run(Kind::Table, seed, 2_000);
    }
}

/// Do: as the table's run, on an index of (a, b, rowid), ascending and binary. Expect: the same
/// (T-NS.1.1, AC-NS.1.1).
#[test]
fn ns_1_1_index_cursors_agree_step_by_step() {
    for seed in 0..seeds() {
        run(
            Kind::Index {
                desc: false,
                nocase: false,
            },
            seed,
            2_000,
        );
    }
}

/// Do: as the table's run, on an index whose first column descends and whose second compares
/// under NOCASE, with records that differ only in a spelling NOCASE ignores put over each other.
/// Expect: the same (T-NS.1.1, AC-NS.1.1).
#[test]
fn ns_1_1_descending_nocase_index_cursors_agree_step_by_step() {
    for seed in 0..seeds() {
        run(
            Kind::Index {
                desc: true,
                nocase: true,
            },
            seed,
            2_000,
        );
    }
}

/// What a versioned cursor's call returned: it never waits on I/O.
fn now<T>(r: crate::types::IOResultOr<T>) -> T {
    match r.unwrap() {
        crate::IOResult::Done(t) => t,
        crate::IOResult::IO(_) => panic!("the versioned cursor waited on I/O"),
    }
}

/// The rows a reading cursor sees, forward from the first.
fn forward(c: &mut VersionedCursor) -> Vec<i64> {
    let mut out = Vec::new();
    now(c.rewind());
    while c.has_record() {
        out.push(now(c.rowid()).unwrap());
        now(c.next());
    }
    out
}

/// The rows a reading cursor sees, back from the last.
fn backward(c: &mut VersionedCursor) -> Vec<i64> {
    let mut out = Vec::new();
    now(c.last());
    while c.has_record() {
        out.push(now(c.rowid()).unwrap());
        now(c.prev());
    }
    out.reverse();
    out
}

/// Do: commit 3,000 even rowids, open a reading cursor on a snapshot, then let 4 writers commit odd
/// rowids and remove even ones in batches (splitting leaves under the reader) while the reader
/// walks forward and back and seeks. Expect: every walk and seek sees exactly the 3,000 even rows,
/// in order: rows written or removed after its snapshot never move it (decision 4 under the
/// cursor's leaf copies; T-NS.1.1).
#[test]
fn ns_1_1_a_reading_cursor_sees_its_snapshot_while_writers_split_leaves() {
    let order = KeyOrder::rowid();
    let vdb = Arc::new(
        VDatabase::open(Arc::new(Simulated::new(9, 1).unwrap()), 8_000, 1, || {
            order.comparator()
        })
        .unwrap(),
    );
    let (pager, _db, _conn) = engine();
    let mut d = super::blink::Descender::new();
    let rec = record(&[Value::from_text("x".repeat(40))]);
    let mut txn = vdb.begin().unwrap();
    let want: Vec<i64> = (0..3_000).map(|i| i * 2).collect();
    for r in &want {
        vdb.put(
            &mut d,
            &mut txn,
            0,
            &super::cursor::rowid_key(*r),
            rec.get_payload(),
        )
        .unwrap();
    }
    vdb.commit(txn, &mut d, vdb.lane(), false).unwrap();
    let snap = vdb.snapshot().unwrap();
    let mut reader =
        VersionedCursor::new(vdb.clone(), 0, order, Access::Read(snap), pager).unwrap();
    let writers: Vec<_> = (0..4i64)
        .map(|w| {
            let (vdb, rec) = (vdb.clone(), rec.get_payload().to_vec());
            std::thread::spawn(move || {
                let mut d = super::blink::Descender::new();
                for batch in 0..30i64 {
                    let mut txn = vdb.begin().unwrap();
                    for i in 0..50i64 {
                        let r = (w + 4 * (batch * 50 + i)) * 2 + 1;
                        vdb.put(&mut d, &mut txn, 0, &super::cursor::rowid_key(r), &rec)
                            .unwrap();
                    }
                    let gone = (w + 4 * batch) * 2;
                    vdb.remove(&mut d, &mut txn, 0, &super::cursor::rowid_key(gone))
                        .unwrap();
                    vdb.commit(txn, &mut d, vdb.lane(), false).unwrap();
                }
            })
        })
        .collect();
    let mut rng = Rng(77);
    for _ in 0..5 {
        assert_eq!(forward(&mut reader), want);
        assert_eq!(backward(&mut reader), want);
        for _ in 0..200 {
            let r = i64::try_from(rng.below(6_000)).unwrap();
            let found = now(reader.seek(SeekKey::TableRowId(r), SeekOp::GE { eq_only: false }));
            let expect = want.iter().copied().find(|&w| w >= r);
            let got = if found == SeekResult::Found {
                now(reader.rowid())
            } else {
                None
            };
            assert_eq!(got, expect, "seek >= {r}");
        }
    }
    for w in writers {
        w.join().unwrap();
    }
    assert_eq!(forward(&mut reader), want);
    vdb.close_snapshot(snap);
}

/// Do: in one transaction, put rows 0..200 through one cursor; stand a second cursor on row 10, and
/// through the first remove row 11, put row 10 again with a new record, and put row 13 (absent
/// till then). Expect: the second cursor's record of row 10 is the new one, its next row is
/// 12 then 13, and its previous from 12 is 10: a cursor sees its transaction's writes through
/// another cursor on the same tree, as SQLite's do after saveAllCursors (T-NS.1.1).
#[test]
fn ns_1_1_a_cursor_sees_writes_through_another_cursor_of_its_transaction() {
    let order = KeyOrder::rowid();
    let vdb = Arc::new(
        VDatabase::open(Arc::new(Simulated::new(3, 1).unwrap()), 2_000, 1, || {
            order.comparator()
        })
        .unwrap(),
    );
    let (pager, _db, _conn) = engine();
    let txn = Arc::new(Mutex::new(Some(vdb.begin().unwrap())));
    let mut w = VersionedCursor::new(
        vdb.clone(),
        0,
        order.clone(),
        Access::Write(txn.clone()),
        pager.clone(),
    )
    .unwrap();
    let mut r = VersionedCursor::new(vdb, 0, order, Access::Write(txn), pager).unwrap();
    let old = record(&[Value::from_text("old".repeat(30))]);
    let new = record(&[Value::from_text("new")]);
    for k in (0..200i64).filter(|k| *k != 13) {
        now(w.insert(&BTreeKey::new_table_rowid(k, Some(&old))));
    }
    assert_eq!(
        now(r.seek(SeekKey::TableRowId(10), SeekOp::GE { eq_only: true })),
        SeekResult::Found
    );
    assert_eq!(
        now(w.seek(SeekKey::TableRowId(11), SeekOp::GE { eq_only: true })),
        SeekResult::Found
    );
    now(w.delete());
    now(w.insert(&BTreeKey::new_table_rowid(10, Some(&new))));
    now(w.insert(&BTreeKey::new_table_rowid(13, Some(&new))));
    let payload = now(r.record()).map(|rec| rec.get_payload().to_vec());
    assert_eq!(payload, Some(new.get_payload().to_vec()));
    now(r.next());
    assert_eq!(now(r.rowid()), Some(12));
    now(r.next());
    assert_eq!(now(r.rowid()), Some(13));
    now(r.prev());
    now(r.prev());
    assert_eq!(now(r.rowid()), Some(10));
}

/// Do: a write through a cursor opened on a snapshot alone. Expect: `ReadOnly`, nothing written.
#[test]
fn ns_1_1_a_reading_cursor_refuses_writes() {
    let order = KeyOrder::rowid();
    let vdb = Arc::new(
        VDatabase::open(Arc::new(Simulated::new(4, 1).unwrap()), 100, 1, || {
            order.comparator()
        })
        .unwrap(),
    );
    let (pager, _db, _conn) = engine();
    let snap = vdb.snapshot().unwrap();
    let mut c = VersionedCursor::new(vdb, 0, order, Access::Read(snap), pager).unwrap();
    let rec = record(&[Value::from_i64(1)]);
    let refused = c.insert(&BTreeKey::new_table_rowid(1, Some(&rec)));
    assert!(matches!(
        refused.map(|_| ()).map_err(|e| *e),
        Err(crate::LimboError::ReadOnly)
    ));
    assert_eq!(forward(&mut c), Vec::<i64>::new());
}
