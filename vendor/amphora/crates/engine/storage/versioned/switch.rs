//! Amphora: the switch that puts a connection's B-trees on the versioned storage (G7 step 2,
//! `AC-NS.2.1`; Amphora's `docs/design/engine.md`, decision 13). Off by default: the pager's
//! B-trees stay the engine's storage until the engine's suites pass on this one.
//!
//! A database's connections share one versioned storage, made by the first connection that asks
//! for it: [`ConnectOpts::with_versioned_storage`](crate::ConnectOpts::with_versioned_storage), or
//! `AMPHORA_STORAGE=versioned` in the environment (so the engine's suites run on it unchanged).
//! With it on, the executor's cursors are [`VersionedCursor`]s on its trees, a tree's id is its
//! root page in `sqlite_schema` (tree 1 is `sqlite_schema` itself), and each SQL transaction is one
//! transaction of the storage, begun by its first cursor and committed or rolled back where the
//! pager's ends. The pager still runs beside it for what the trees don't hold yet: the database
//! header (the schema cookie, the user version). An index tree's order is recorded in the storage
//! when its first cursor brings it ([`encode_order`]), so a reopened database orders the tree as
//! before it replays a row of it.

use super::blink::{Comparator, Descender};
use super::cursor::{Access, KeyOrder, SharedTxn, VersionedCursor};
use super::disk::{Disk, Files, Simulated};
use super::durable::Database as VDatabase;
use super::memory::Source;
use super::pool::Room;
use super::version::Savepoint;
use crate::error::LimboError;
use crate::sync::{Arc, Mutex, OnceLock};
use crate::translate::collate::CollationSeq;
use crate::types::{IndexInfo, KeyInfo};
use crate::{Pager, Result};
use amphora_engine_parser::ast::{NullsOrder, SortOrder};
use std::collections::HashMap;

/// The environment variable that turns the switch on for every connection: `versioned`.
pub const ENV: &str = "AMPHORA_STORAGE";

/// Whether the environment asks for the versioned storage.
pub fn asked() -> bool {
    std::env::var(ENV).is_ok_and(|v| v == "versioned")
}

/// Whether a test of the pager itself stands aside (G7 step 2, `AC-NS.2.1`): with the switch on,
/// the run that judges the engine's suites on the versioned storage, a test whose subject is the
/// pager (its log, its MVCC store, its coordination between processes, cursors over its pages)
/// says why on stderr and checks nothing; with it off, it runs.
#[cfg(test)]
pub fn pager_only(why: &str) -> bool {
    let on = asked();
    if on {
        eprintln!("pager-only: {why}");
    }
    on
}

/// The trees a storage is made with: tree 0, which no root page names (SQLite's page numbers start
/// at 1), and tree 1, `sqlite_schema` (root page 1). The overflow tree comes after them (2), and
/// every tree made later takes the next id, which is its root page.
const TREES: usize = 2;

/// A database's versioned storage, shared by its connections, and the order of each index tree made
/// while it runs: known once a cursor first opens the tree with its index's key (an index's tree is
/// empty until then, so nothing is compared before), and kept for every later cursor and the
/// tree's own comparison, which share it.
pub struct Storage {
    db: Arc<VDatabase>,
    orders: Mutex<HashMap<u64, Arc<OnceLock<KeyOrder>>>>,
}

/// The versioned storage of the database at `path`: in memory for a memory database or one whose
/// files the engine keeps in memory (`memory`), else in the directory `<path>-versioned`. It holds
/// what its memory source grants (the shell's process budget, else [`super::memory::machine`]),
/// pages and row versions alike, as they grow: no part is set aside for either. Its redo log takes
/// a lane per thread the machine runs (decision 11).
pub fn open(path: &str, memory: bool, source: Option<Arc<dyn Source>>) -> Result<Arc<Storage>> {
    let source = source.unwrap_or_else(super::memory::machine);
    let lanes = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let disk: Arc<dyn Disk> = if memory || crate::database::is_memory_like(path) {
        Arc::new(Simulated::new(0, lanes)?)
    } else {
        let dir = std::path::PathBuf::from(format!("{path}-versioned"));
        std::fs::create_dir_all(&dir).map_err(|e| {
            LimboError::InternalError(format!("versioned storage: making {}: {e}", dir.display()))
        })?;
        Arc::new(Files::open(&dir, lanes)?)
    };
    // The index trees' orders the storage recorded, decoded once each as recovery reads them.
    let recovered: Mutex<HashMap<u64, Arc<OnceLock<KeyOrder>>>> = Mutex::new(HashMap::new());
    let db = Arc::new(VDatabase::open_in(
        disk,
        Room::Shared(source),
        TREES,
        || KeyOrder::rowid().comparator(),
        |tree, bytes| {
            // The tree's comparison and its cursors share the order (its mark of a record it
            // can't read).
            let order = KeyOrder::index(Arc::new(decode_order(bytes)?));
            let slot = Arc::new(OnceLock::new());
            let _ = slot.set(order.clone());
            recovered.lock().insert(tree, slot);
            Ok(order.comparator())
        },
    )?);
    Ok(Arc::new(Storage {
        db,
        orders: Mutex::new(recovered.into_inner()),
    }))
}

impl Storage {
    /// The trees the storage holds now, dropped ones not counted.
    #[cfg(test)]
    pub fn live_trees(&self) -> usize {
        self.db.v.roots().iter().filter(|&&r| r != 0).count()
    }
}

/// The encoding of an index's order the storage records (G7 step 2, `AC-NS.2.1`; a format of
/// this storage's own): a version (`u8`, 1), whether the key ends in a rowid (`u8`), whether it is
/// unique (`u8`), the column count (`u64`), the key column count (`u32`), and each key column's
/// sort order (`u8`: 0 ascending, 1 descending), nulls order (`u8`: 0 none, 1 first, 2 last) and
/// collation (`u8`: 0 unset, 1 binary, 2 nocase, 3 rtrim, 4 a locale's, 5 a connection's own,
/// those two then their name's length (`u32`) and name). A collation goes by name, not by the
/// process's id for it.
pub fn encode_order(info: &IndexInfo) -> Result<Vec<u8>> {
    let mut out = vec![1u8, u8::from(info.has_rowid), u8::from(info.is_unique)];
    out.extend_from_slice(
        &u64::try_from(info.num_cols)
            .map_err(|_| LimboError::IntegerOverflow)?
            .to_le_bytes(),
    );
    let n = u32::try_from(info.key_info.len()).map_err(|_| LimboError::IntegerOverflow)?;
    out.extend_from_slice(&n.to_le_bytes());
    for k in info.key_info.iter() {
        out.push(match k.sort_order {
            SortOrder::Asc => 0,
            SortOrder::Desc => 1,
        });
        out.push(match k.nulls_order {
            None => 0,
            Some(NullsOrder::First) => 1,
            Some(NullsOrder::Last) => 2,
        });
        let named = |kind: u8, out: &mut Vec<u8>| -> Result<()> {
            let name = k.collation.name();
            out.push(kind);
            let len = u32::try_from(name.len()).map_err(|_| LimboError::IntegerOverflow)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            Ok(())
        };
        match k.collation {
            CollationSeq::Unset => out.push(0),
            CollationSeq::Binary => out.push(1),
            CollationSeq::NoCase => out.push(2),
            CollationSeq::Rtrim => out.push(3),
            CollationSeq::Locale(_) => named(4, &mut out)?,
            CollationSeq::Custom(_) => named(5, &mut out)?,
        }
    }
    Ok(out)
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(n).ok_or(LimboError::IntegerOverflow)?;
    let got = bytes.get(*at..end).ok_or_else(|| {
        LimboError::Corrupt("versioned storage: a tree's recorded order is cut short".into())
    })?;
    *at = end;
    Ok(got)
}

fn take_u8(bytes: &[u8], at: &mut usize) -> Result<u8> {
    take(bytes, at, 1)?
        .first()
        .copied()
        .ok_or(LimboError::IntegerOverflow)
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Result<usize> {
    let b: [u8; 4] = take(bytes, at, 4)?
        .try_into()
        .map_err(|_| LimboError::IntegerOverflow)?;
    usize::try_from(u32::from_le_bytes(b)).map_err(|_| LimboError::IntegerOverflow)
}

/// The index order [`encode_order`] encoded; damage refused (`Corrupt`).
pub fn decode_order(bytes: &[u8]) -> Result<IndexInfo> {
    let bad = |what: &str| {
        LimboError::Corrupt(format!(
            "versioned storage: a tree's recorded order: {what}"
        ))
    };
    let mut at = 0;
    if take_u8(bytes, &mut at)? != 1 {
        return Err(bad("an encoding this build doesn't read"));
    }
    let has_rowid = take_u8(bytes, &mut at)? != 0;
    let is_unique = take_u8(bytes, &mut at)? != 0;
    let num: [u8; 8] = take(bytes, &mut at, 8)?
        .try_into()
        .map_err(|_| LimboError::IntegerOverflow)?;
    let num_cols =
        usize::try_from(u64::from_le_bytes(num)).map_err(|_| LimboError::IntegerOverflow)?;
    let n = take_u32(bytes, &mut at)?;
    // Each key column takes at least three bytes: a count past what the bytes hold is damage.
    if n > bytes.len() / 3 {
        return Err(bad("more key columns than it holds"));
    }
    let mut keys = Vec::with_capacity(n);
    for _ in 0..n {
        let sort_order = match take_u8(bytes, &mut at)? {
            0 => SortOrder::Asc,
            1 => SortOrder::Desc,
            _ => return Err(bad("a sort order")),
        };
        let nulls_order = match take_u8(bytes, &mut at)? {
            0 => None,
            1 => Some(NullsOrder::First),
            2 => Some(NullsOrder::Last),
            _ => return Err(bad("a nulls order")),
        };
        let kind = take_u8(bytes, &mut at)?;
        let collation = match kind {
            0 => CollationSeq::Unset,
            1 => CollationSeq::Binary,
            2 => CollationSeq::NoCase,
            3 => CollationSeq::Rtrim,
            4 | 5 => {
                let len = take_u32(bytes, &mut at)?;
                let name = std::str::from_utf8(take(bytes, &mut at, len)?)
                    .map_err(|_| bad("a collation's name"))?;
                if kind == 4 {
                    CollationSeq::new(name)?
                } else {
                    CollationSeq::custom(name)
                }
            }
            _ => return Err(bad("a collation")),
        };
        keys.push(KeyInfo {
            sort_order,
            collation,
            nulls_order,
        });
    }
    if at != bytes.len() {
        return Err(bad("bytes past its end"));
    }
    IndexInfo::new(keys, has_rowid, num_cols, is_unique).map_err(|_| {
        LimboError::InternalError("versioned storage: no memory for an index's order".into())
    })
}

/// A comparison that orders as `slot`'s order once it is known, and by bytes before (when the tree
/// holds nothing to compare).
fn late(slot: Arc<OnceLock<KeyOrder>>) -> Comparator {
    Box::new(move |a: &[u8], b: &[u8]| match slot.get() {
        Some(order) => order.compare(a, b),
        None => a.cmp(b),
    })
}

fn tree_of(root_page: i64) -> Result<u64> {
    u64::try_from(root_page).map_err(|_| {
        LimboError::InternalError(format!(
            "versioned storage: no tree at root page {root_page}"
        ))
    })
}

/// One connection's side of the switch: the storage, and the transaction its cursors share.
pub struct Switch {
    storage: Arc<Storage>,
    db: Arc<VDatabase>,
    txn: Mutex<Option<SharedTxn>>,
    /// The savepoints of the write statements under way in the transaction, newest last: one a
    /// statement, so bounded by the statements nested in one another.
    statements: Mutex<Vec<(Savepoint, Mark)>>,
    /// Trees the transaction made, dropped if it rolls back, and trees it dropped, dropped once it
    /// commits (making and dropping a tree is durable alone, decision 12): one a DDL statement.
    made: Mutex<Vec<u64>>,
    dropped: Mutex<Vec<u64>>,
    /// Trees made by a statement or savepoint since rolled back: no schema row names them, so
    /// they are dropped as the transaction ends, however it ends (G7 step 2: no orphan trees).
    undone: Mutex<Vec<u64>>,
    /// SQL's named savepoints (`SAVEPOINT name`), oldest first, beside the connection's frames.
    named: Mutex<Vec<(String, Savepoint, Mark)>>,
    d: Mutex<Descender>,
}

/// How many trees the transaction had made and dropped when a savepoint was taken: rolling back
/// to it undoes the DDL after it too.
#[derive(Clone, Copy)]
struct Mark {
    made: usize,
    dropped: usize,
}

impl Switch {
    /// Where the transaction's DDL stands now.
    fn mark(&self) -> Mark {
        Mark {
            made: self.made.lock().len(),
            dropped: self.dropped.lock().len(),
        }
    }

    /// Undoes the DDL after `mark`: the trees made since are kept to drop as the transaction ends
    /// (a tree its rows were written to can't be dropped while the transaction is open), and the
    /// drops asked since are forgotten, so a commit keeps those trees.
    fn undo_to(&self, mark: Mark) {
        let mut made = self.made.lock();
        let from = mark.made.min(made.len());
        let gone: Vec<u64> = made.drain(from..).collect();
        self.undone.lock().extend(gone);
        self.dropped.lock().truncate(mark.dropped);
    }

    pub fn new(storage: Arc<Storage>) -> Self {
        Self {
            db: storage.db.clone(),
            storage,
            txn: Mutex::new(None),
            statements: Mutex::new(Vec::new()),
            made: Mutex::new(Vec::new()),
            dropped: Mutex::new(Vec::new()),
            undone: Mutex::new(Vec::new()),
            named: Mutex::new(Vec::new()),
            d: Mutex::new(Descender::new()),
        }
    }

    /// The connection's transaction, begun if none is open.
    fn txn(&self) -> Result<SharedTxn> {
        let mut slot = self.txn.lock();
        if let Some(txn) = slot.as_ref() {
            return Ok(txn.clone());
        }
        let txn = Arc::new(Mutex::new(Some(self.db.begin()?)));
        *slot = Some(txn.clone());
        Ok(txn)
    }

    /// A cursor on the tree at `root_page`, in the connection's transaction: keyed by rowid, or by
    /// record for `index` (an index's or a WITHOUT ROWID table's key).
    pub fn cursor(
        &self,
        root_page: i64,
        index: Option<Arc<IndexInfo>>,
        pager: Arc<Pager>,
    ) -> Result<VersionedCursor> {
        let tree = tree_of(root_page)?;
        let order = match index {
            Some(info) => {
                let slot = self.storage.orders.lock().get(&tree).cloned();
                match slot {
                    Some(slot) => match slot.get() {
                        Some(order) => order.clone(),
                        None => {
                            // The tree's first cursor: its order recorded, durable, before a row
                            // of it is written, then kept.
                            self.db
                                .order_tree(&mut self.d.lock(), tree, encode_order(&info)?)?;
                            slot.get_or_init(|| KeyOrder::index(info)).clone()
                        }
                    },
                    None => KeyOrder::index(info),
                }
            }
            None => KeyOrder::rowid(),
        };
        VersionedCursor::new(
            self.db.clone(),
            tree,
            order,
            Access::Write(self.txn()?),
            pager,
        )
    }

    /// Makes a tree: a table's, keyed by rowid, or an index's (or a WITHOUT ROWID table's), keyed
    /// by record in the order its first cursor brings. Its root page.
    pub fn make_tree(&self, table: bool) -> Result<i64> {
        let mut d = self.d.lock();
        let id = if table {
            self.db.make_tree(&mut d, KeyOrder::rowid().comparator())?
        } else {
            let slot = Arc::new(OnceLock::new());
            let mut orders = self.storage.orders.lock();
            let id = self.db.make_tree(&mut d, late(slot.clone()))?;
            orders.insert(id, slot);
            id
        };
        self.made.lock().push(id);
        i64::try_from(id).map_err(|_| LimboError::IntegerOverflow)
    }

    /// Drops the tree at `root_page` (DROP TABLE, DROP INDEX) once the transaction commits, so a
    /// rollback keeps it.
    pub fn drop_tree(&self, root_page: i64) -> Result<()> {
        self.dropped.lock().push(tree_of(root_page)?);
        Ok(())
    }

    /// Drops `trees` now, each durable before the next.
    fn drop_now(&self, trees: Vec<u64>) -> Result<()> {
        for tree in trees {
            self.db.drop_tree(&mut self.d.lock(), tree)?;
            self.storage.orders.lock().remove(&tree);
        }
        Ok(())
    }

    /// `PRAGMA integrity_check` on the versioned storage (G7 step 2, `AC-NS.2.1`): reads every
    /// committed row of each tree at `roots` and checks each key follows the one before in the
    /// tree's order. What it found wrong, at most `most`, as the pragma's lines. It reads a batch
    /// of rows at a time, as many as the pool has frames, so what it holds is bounded by the pool.
    pub fn check(&self, roots: &[i64], most: usize) -> Result<Vec<String>> {
        let batch = self.db.v.pool.capacity().max(2);
        let snap = self.db.snapshot()?;
        let mut d = Descender::new();
        let mut found = Vec::new();
        let checked = (|| -> Result<()> {
            for &root in roots {
                let tree = tree_of(root)?;
                let slot = self.storage.orders.lock().get(&tree).cloned();
                let order = match slot.as_ref().and_then(|s| s.get()) {
                    Some(order) => order.clone(),
                    None => KeyOrder::rowid(),
                };
                let mut last: Option<Vec<u8>> = None;
                let mut row = 0u64;
                loop {
                    let rows = match self.db.scan(&mut d, &snap, tree, last.as_deref(), batch) {
                        Ok(rows) => rows,
                        Err(e) => {
                            found.push(format!("tree at root page {root}: unreadable: {e}"));
                            break;
                        }
                    };
                    let mut fresh = 0usize;
                    for (key, _) in rows {
                        if let Some(prev) = &last {
                            match order.compare(prev, &key) {
                                std::cmp::Ordering::Less => {}
                                // The batch's first row is the last one read before.
                                std::cmp::Ordering::Equal if fresh == 0 => continue,
                                _ => found.push(format!(
                                    "tree at root page {root}: row {row} out of order"
                                )),
                            }
                        }
                        fresh = fresh.saturating_add(1);
                        row = row.saturating_add(1);
                        last = Some(key);
                        if found.len() >= most {
                            return Ok(());
                        }
                    }
                    if fresh == 0 {
                        break;
                    }
                }
            }
            Ok(())
        })();
        self.db.close_snapshot(snap);
        checked?;
        found.truncate(most);
        Ok(found)
    }

    /// Removes every row of the tree at `root_page` in the connection's transaction: how many.
    pub fn clear(&self, root_page: i64, pager: Arc<Pager>) -> Result<u64> {
        let tree = tree_of(root_page)?;
        let slot = self.storage.orders.lock().get(&tree).cloned();
        let order = match slot.as_ref().and_then(|s| s.get()) {
            Some(order) => order.clone(),
            // A table's tree, or an index's no cursor has opened: empty.
            None => KeyOrder::rowid(),
        };
        let mut cursor = VersionedCursor::new(
            self.db.clone(),
            tree,
            order,
            Access::Write(self.txn()?),
            pager,
        )?;
        cursor.clear_rows()
    }

    /// Opens a savepoint for a write statement in an explicit transaction (SQLite's statement
    /// journal), so the statement can be undone alone (`AC-NS.2.3`'s savepoints).
    pub fn begin_statement(&self) -> Result<()> {
        let shared = self.txn()?;
        let sp = shared
            .lock()
            .as_ref()
            .ok_or_else(|| LimboError::TxError("versioned storage: no transaction".into()))?
            .savepoint();
        let mark = self.mark();
        self.statements.lock().push((sp, mark));
        Ok(())
    }

    /// Opens SQL's savepoint `name` (`SAVEPOINT name`) in the connection's transaction.
    pub fn push_named(&self, name: &str) -> Result<()> {
        let shared = self.txn()?;
        let sp = shared
            .lock()
            .as_ref()
            .ok_or_else(|| LimboError::TxError("versioned storage: no transaction".into()))?
            .savepoint();
        let mark = self.mark();
        self.named.lock().push((name.to_string(), sp, mark));
        Ok(())
    }

    /// Releases the newest savepoint `name` and those opened after it (`RELEASE name`).
    pub fn release_named(&self, name: &str) {
        let mut named = self.named.lock();
        if let Some(i) = named.iter().rposition(|(n, _, _)| n == name) {
            named.truncate(i);
        }
    }

    /// Rolls the transaction back to the newest savepoint `name` (`ROLLBACK TO name`), which
    /// stays open; those opened after it end.
    pub fn rollback_named(&self, name: &str) -> Result<()> {
        let sp = {
            let mut named = self.named.lock();
            let Some(i) = named.iter().rposition(|(n, _, _)| n == name) else {
                return Ok(());
            };
            named.truncate(i.checked_add(1).ok_or(LimboError::IntegerOverflow)?);
            match named.get(i) {
                Some((_, sp, mark)) => {
                    self.undo_to(*mark);
                    *sp
                }
                None => return Ok(()),
            }
        };
        let Some(shared) = self.txn.lock().clone() else {
            return Ok(());
        };
        let mut guard = shared.lock();
        let Some(txn) = guard.as_mut() else {
            return Ok(());
        };
        self.db.rollback_to(&mut self.d.lock(), txn, sp)
    }

    /// Ends the newest statement's savepoint: kept, or rolled back to (its changes undone, the
    /// transaction's earlier ones kept).
    pub fn end_statement(&self, rollback: bool) -> Result<()> {
        let Some((sp, mark)) = self.statements.lock().pop() else {
            return Ok(());
        };
        if !rollback {
            return Ok(());
        }
        self.undo_to(mark);
        let Some(shared) = self.txn.lock().clone() else {
            return Ok(());
        };
        let mut guard = shared.lock();
        let Some(txn) = guard.as_mut() else {
            return Ok(());
        };
        self.db.rollback_to(&mut self.d.lock(), txn, sp)
    }

    /// Ends the connection's transaction, if one is open: commits it (durable before it returns),
    /// or rolls it back. A cursor still holding it is refused its next write.
    /// Trees the transaction made are dropped if it rolls back; trees it dropped, once it commits.
    pub fn finish(&self, rollback: bool) -> Result<()> {
        self.statements.lock().clear();
        self.named.lock().clear();
        let made = std::mem::take(&mut *self.made.lock());
        let dropped = std::mem::take(&mut *self.dropped.lock());
        let mut gone = std::mem::take(&mut *self.undone.lock());
        let shared = self.txn.lock().take();
        let txn = shared.and_then(|s| s.lock().take());
        let ended = match txn {
            Some(txn) => {
                let mut d = self.d.lock();
                if rollback {
                    self.db.abort(txn, &mut d)
                } else {
                    self.db
                        .commit(txn, &mut d, self.db.lane(), true)
                        .map(|_| ())
                }
            }
            None => Ok(()),
        };
        // A transaction that didn't commit leaves the trees it made named by no schema row: they
        // are dropped, a failed commit's too. A commit whose flush failed may be durable, but its
        // log is then refused (the redo log is poisoned), so its trees stay for recovery to judge.
        if rollback || ended.is_err() {
            gone.extend(made);
        } else {
            gone.extend(dropped);
        }
        let dropping = self.drop_now(gone);
        ended?;
        dropping
    }
}
