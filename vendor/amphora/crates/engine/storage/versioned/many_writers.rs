//! Amphora: many writers through the SQL layer, checked against a serial replay (G7 step 5, CP-T1.3
//! stage 3, `T-G7.5.1`). N connections, each a thread, run the comprehensive suite's write classes
//! (the classes `bench/src/cc.rs` runs at the storage, here as SQL) on one database at once; then the
//! committed transactions run again, one at a time in commit order, on a fresh database, and each
//! statement of each transaction must answer as it did under concurrency, and the two databases must
//! end the same, their rows and their index.
//!
//! Commit order is a counter each transaction takes just before its `COMMIT`. That is a serial order
//! the claimed isolation (serializable, on both storages) must explain for these classes: every row a
//! class reads it also writes, so a later commit that touched the row either saw this one (began after
//! it was visible, so took the counter after) or was refused (the pager's single writer and stale
//! snapshot; the versioned storage's first writer's win). A lost update, a stale read or a torn index
//! shows as a statement that answers differently in the replay, or as databases that end apart.
//!
//! Ids come two ways: from each writer's own slice, as the suite's clients write (no writer contends
//! with another for a row), and from every row (they contend, and the refusals are what the check
//! is about). A refused transaction is rolled back, counted and not run again; only committed ones are
//! replayed. It runs on the pager's storage and on the versioned storage (the switch's
//! [`ConnectOpts::with_versioned_storage`](crate::ConnectOpts::with_versioned_storage)); with
//! `AMPHORA_STORAGE=versioned` set every connection is on the versioned storage, so it runs once.
//!
//! Sizes: `AMPHORA_MW_WRITERS` (connections; the step's 32 unless set), `AMPHORA_MW_TXNS`
//! (transactions a class runs across its writers; Elle's 256 a seed, `elle::TXNS`, unless set),
//! `AMPHORA_MW_ITEMS` (rows loaded; each writer's slice as many rows as the class runs transactions
//! unless set), `AMPHORA_MW_ROW_BYTES` (a row's filler; the suite's 600, engine.md decision 6) and
//! `AMPHORA_MW_SEED` (the writers' random ids). The suite's own sizes (200,000 items) are the
//! coordinator's run of the same test.

use super::elle::{splitmix, TXNS, WRITERS};
use crate::io::{MemoryIO, IO};
use crate::numeric::Numeric;
use crate::sync::Arc;
use crate::types::Value;
use crate::{ConnectOpts, Connection, Database, LimboError, SqliteDialect};
use std::sync::atomic::{AtomicU64, Ordering};

/// Which storage a database's connections are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Storage {
    Pager,
    Versioned,
}

/// The storages to run on: both, unless the environment puts every connection on the versioned one.
pub(super) fn storages() -> Vec<Storage> {
    if super::switch::asked() {
        vec![Storage::Versioned]
    } else {
        vec![Storage::Pager, Storage::Versioned]
    }
}

pub(super) fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .map_or(default, |v| v.parse().unwrap())
}

/// A database in memory (each its own).
pub(super) fn open() -> Arc<Database> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap()
}

pub(super) fn connect(db: &Arc<Database>, storage: Storage) -> Arc<Connection> {
    let opts = match storage {
        Storage::Pager => ConnectOpts::new(),
        Storage::Versioned => ConnectOpts::new().with_versioned_storage(),
    };
    db.connect(opts).unwrap()
}

pub(super) fn rows(conn: &Arc<Connection>, sql: &str) -> Result<Vec<Vec<Value>>, LimboError> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    stmt.run_with_row_callback(|row| {
        out.push(row.get_values().cloned().collect());
        Ok(())
    })?;
    Ok(out)
}

/// What refuses a transaction under many writers, for its caller to roll back: the pager's busy
/// writer lock and stale snapshot, the versioned storage's first writer's win and its validation.
pub(super) fn refused(e: &LimboError) -> bool {
    matches!(
        e,
        LimboError::Busy
            | LimboError::BusySnapshot
            | LimboError::WriteWriteConflict
            | LimboError::SerializationFailure
    )
}

/// Rolls back what a refusal left open (a refused statement may or may not have ended it).
pub(super) fn roll_back(conn: &Arc<Connection>) {
    if !conn.get_auto_commit() {
        conn.execute("ROLLBACK").unwrap();
    }
}

pub(super) fn int(v: &Value) -> i64 {
    match v {
        Value::Numeric(Numeric::Integer(n)) => *n,
        other => panic!("not an integer: {other:?}"),
    }
}

/// The suite's write classes (`bench/src/cc.rs`'s, without K0, which is its own workload).
const CLASSES: [&str; 8] = [
    "insert_one",
    "insert_batch_100",
    "update_plain",
    "update_indexed",
    "update_range",
    "delete_insert",
    "delete_range_insert",
    "txn_read_write",
];
/// The suite's span classes' rows and batch class's rows (`bench/src/cc.rs`).
const RANGE: i64 = 10;
const BATCH: u64 = 100;

/// One transaction of a class, its ids and values drawn before it runs, so the replay runs it alike.
#[derive(Clone, Debug)]
enum Txn {
    Insert(Vec<i64>),
    UpdatePlain(i64),
    UpdateIndexed(i64, i64),
    UpdateRange(i64),
    DeleteInsert(i64),
    DeleteRangeInsert(i64),
    ReadWrite(i64, i64, i64),
}

/// A statement's answer: its rows, and the rows it changed.
type Answer = (Vec<Vec<Value>>, i64);

#[derive(Clone, Copy)]
struct Sizes {
    writers: u64,
    txns: u64,
    items: u64,
    row_bytes: u64,
}

fn sizes() -> Sizes {
    let writers = env("AMPHORA_MW_WRITERS", WRITERS as u64).max(1);
    let txns = env("AMPHORA_MW_TXNS", TXNS as u64);
    Sizes {
        writers,
        txns,
        items: env("AMPHORA_MW_ITEMS", writers * txns.max(RANGE as u64)),
        row_bytes: env("AMPHORA_MW_ROW_BYTES", 600),
    }
}

/// The classes' table: `items` rows, `v` indexed, `n` counting the writes each row took.
fn load(conn: &Arc<Connection>, s: &Sizes) {
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, v INTEGER NOT NULL, n INTEGER NOT NULL, pad TEXT NOT NULL)").unwrap();
    conn.execute("CREATE INDEX items_v ON items (v)").unwrap();
    conn.execute("BEGIN").unwrap();
    for id in 0..s.items {
        conn.execute(format!(
            "INSERT INTO items VALUES ({id}, {id}, 0, printf('%.*c', {}, 'x'))",
            s.row_bytes
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// Draws writer `w`'s next transaction of `class`: ids from its own slice, or from every row.
fn draw(class: &str, s: &Sizes, w: u64, shared: bool, rng: &mut u64, next: &AtomicU64) -> Txn {
    let (first, span) = if shared {
        (0, s.items)
    } else {
        let per = (s.items / s.writers).max(1);
        (per * w, per)
    };
    let mut pick = |len: u64| -> i64 {
        let room = span.saturating_sub(len).max(1);
        i64::try_from(first + splitmix(rng) % room).unwrap()
    };
    let fresh = || i64::try_from(next.fetch_add(1, Ordering::Relaxed)).unwrap();
    match class {
        "insert_one" => Txn::Insert(vec![fresh()]),
        "insert_batch_100" => Txn::Insert((0..BATCH).map(|_| fresh()).collect()),
        "update_plain" => Txn::UpdatePlain(pick(1)),
        "update_indexed" => {
            let id = pick(1);
            Txn::UpdateIndexed(id, i64::try_from(splitmix(rng) >> 1).unwrap())
        }
        "update_range" => Txn::UpdateRange(pick(RANGE as u64)),
        "delete_insert" => Txn::DeleteInsert(pick(1)),
        "delete_range_insert" => Txn::DeleteRangeInsert(pick(RANGE as u64)),
        "txn_read_write" => {
            let (a, b) = (pick(1), pick(1));
            Txn::ReadWrite(a, b, fresh())
        }
        other => panic!("no class {other}"),
    }
}

/// Runs one statement, its answer added to `out`.
fn stmt(
    conn: &Arc<Connection>,
    sql: &str,
    out: &mut Vec<Answer>,
) -> Result<Vec<Vec<Value>>, LimboError> {
    let got = rows(conn, sql)?;
    // A read changes nothing; the count a connection last kept is its own previous write's.
    let changed = if sql.starts_with("SELECT") {
        0
    } else {
        conn.changes()
    };
    out.push((got.clone(), changed));
    Ok(got)
}

/// Runs `txn`'s statements in the open transaction, each one's answer in order. A statement built
/// from an earlier one's answer is built from this run's, so a replay that read otherwise also
/// writes otherwise, and its answers part from the concurrent run's there.
fn body(conn: &Arc<Connection>, txn: &Txn, pad: u64) -> Result<Vec<Answer>, LimboError> {
    let mut out = Vec::new();
    let reinsert = |out: &mut Vec<Answer>, row: &[Value]| -> Result<(), LimboError> {
        let (id, v, n) = (int(&row[0]), int(&row[1]), int(&row[2]));
        stmt(
            conn,
            &format!(
                "INSERT INTO items VALUES ({id}, {v}, {}, printf('%.*c', {pad}, 'y'))",
                n + 1
            ),
            out,
        )
        .map(drop)
    };
    match txn {
        Txn::Insert(ids) => {
            let values: Vec<String> = ids
                .iter()
                .map(|id| format!("({id}, {id}, 0, printf('%.*c', {pad}, 'x'))"))
                .collect();
            stmt(
                conn,
                &format!("INSERT INTO items VALUES {}", values.join(", ")),
                &mut out,
            )?;
        }
        Txn::UpdatePlain(id) => {
            stmt(
                conn,
                &format!("SELECT n FROM items WHERE id = {id}"),
                &mut out,
            )?;
            stmt(
                conn,
                &format!("UPDATE items SET n = n + 1, pad = pad WHERE id = {id}"),
                &mut out,
            )?;
        }
        Txn::UpdateIndexed(id, value) => {
            stmt(
                conn,
                &format!("SELECT v, n FROM items WHERE id = {id}"),
                &mut out,
            )?;
            stmt(
                conn,
                &format!("UPDATE items SET v = {value}, n = n + 1 WHERE id = {id}"),
                &mut out,
            )?;
        }
        Txn::UpdateRange(a) => {
            stmt(
                conn,
                &format!("SELECT id, n FROM items WHERE id >= {a} ORDER BY id LIMIT {RANGE}"),
                &mut out,
            )?;
            stmt(
                conn,
                &format!("UPDATE items SET n = n + 1 WHERE id IN (SELECT id FROM items WHERE id >= {a} ORDER BY id LIMIT {RANGE})"),
                &mut out,
            )?;
        }
        Txn::DeleteInsert(id) => {
            let got = stmt(
                conn,
                &format!("SELECT id, v, n FROM items WHERE id = {id}"),
                &mut out,
            )?;
            stmt(
                conn,
                &format!("DELETE FROM items WHERE id = {id}"),
                &mut out,
            )?;
            for row in &got {
                reinsert(&mut out, row)?;
            }
        }
        Txn::DeleteRangeInsert(a) => {
            let got = stmt(
                conn,
                &format!("SELECT id, v, n FROM items WHERE id >= {a} ORDER BY id LIMIT {RANGE}"),
                &mut out,
            )?;
            if let Some(last) = got.last() {
                stmt(
                    conn,
                    &format!(
                        "DELETE FROM items WHERE id >= {a} AND id <= {}",
                        int(&last[0])
                    ),
                    &mut out,
                )?;
            }
            for row in &got {
                reinsert(&mut out, row)?;
            }
        }
        Txn::ReadWrite(a, b, new) => {
            for id in [a, b] {
                stmt(
                    conn,
                    &format!("SELECT n FROM items WHERE id = {id}"),
                    &mut out,
                )?;
                stmt(
                    conn,
                    &format!("UPDATE items SET n = n + 1 WHERE id = {id}"),
                    &mut out,
                )?;
            }
            stmt(
                conn,
                &format!("INSERT INTO items VALUES ({new}, {new}, 0, printf('%.*c', {pad}, 'x'))"),
                &mut out,
            )?;
        }
    }
    Ok(out)
}

/// A committed transaction: its commit order, what it ran, and what each statement answered.
struct Committed {
    order: u64,
    writer: u64,
    txn: Txn,
    answers: Vec<Answer>,
}

/// The run's ending state: every row (its filler by length), and the index's order.
fn state(conn: &Arc<Connection>) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    (
        rows(conn, "SELECT id, v, n, length(pad) FROM items ORDER BY id").unwrap(),
        rows(
            conn,
            "SELECT v, id FROM items INDEXED BY items_v ORDER BY v, id",
        )
        .unwrap(),
    )
}

/// What a run counted.
struct Outcome {
    committed: usize,
    refused: u64,
}

/// Runs `class` with `s.writers` connections at once on `storage`, then replays its commits serially
/// and checks every answer and the ending state. Panics naming the first difference.
fn run_class(storage: Storage, class: &str, shared: bool, s: &Sizes, seed: u64) -> Outcome {
    let db = open();
    load(&connect(&db, storage), s);
    let order = Arc::new(AtomicU64::new(0));
    let next = Arc::new(AtomicU64::new(s.items));
    let per_writer = s.txns.div_ceil(s.writers);
    let threads: Vec<_> = (0..s.writers)
        .map(|w| {
            let (db, order, next, class) =
                (db.clone(), order.clone(), next.clone(), class.to_owned());
            let sz = *s;
            std::thread::spawn(move || {
                let conn = connect(&db, storage);
                let mut rng = seed ^ (w + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                let (mut done, mut refusals) = (Vec::new(), 0u64);
                for _ in 0..per_writer {
                    let txn = draw(&class, &sz, w, shared, &mut rng, &next);
                    conn.execute("BEGIN").unwrap();
                    let answers = match body(&conn, &txn, sz.row_bytes) {
                        Ok(a) => a,
                        Err(e) if refused(&e) => {
                            refusals += 1;
                            roll_back(&conn);
                            continue;
                        }
                        Err(e) => panic!("{storage:?} {class} writer {w}: {txn:?}: {e}"),
                    };
                    let at = order.fetch_add(1, Ordering::SeqCst);
                    match conn.execute("COMMIT") {
                        Ok(()) => done.push(Committed {
                            order: at,
                            writer: w,
                            txn,
                            answers,
                        }),
                        Err(e) if refused(&e) => {
                            refusals += 1;
                            roll_back(&conn);
                        }
                        Err(e) => panic!("{storage:?} {class} writer {w}: commit: {e}"),
                    }
                }
                (done, refusals)
            })
        })
        .collect();
    let (mut history, mut refusals) = (Vec::new(), 0);
    for t in threads {
        let (done, r) = t.join().unwrap();
        history.extend(done);
        refusals += r;
    }
    history.sort_by_key(|c| c.order);
    let concurrent = state(&connect(&db, storage));
    let replay_db = open();
    let replay = connect(&replay_db, storage);
    load(&replay, s);
    for c in &history {
        replay.execute("BEGIN").unwrap();
        let answers = body(&replay, &c.txn, s.row_bytes).unwrap();
        replay.execute("COMMIT").unwrap();
        assert_eq!(
            answers, c.answers,
            "{storage:?} {class} shared={shared}: writer {}'s commit #{} ({:?}) answered otherwise in the serial replay",
            c.writer, c.order, c.txn
        );
    }
    let serial = state(&replay);
    assert_eq!(
        concurrent.0, serial.0,
        "{storage:?} {class} shared={shared}: rows differ from the serial replay's"
    );
    assert_eq!(
        concurrent.1, serial.1,
        "{storage:?} {class} shared={shared}: the index differs from the serial replay's"
    );
    Outcome {
        committed: history.len(),
        refused: refusals,
    }
}

/// T-G7.5.1: do run each write class with many connections at once on each storage, ids from each
/// writer's slice and from every row; expect every committed transaction's every statement to answer
/// as it does in a serial replay in commit order, the same rows and index at the end, and commits in
/// each run (a run that committed nothing checked nothing).
#[test]
fn g7_5_many_writers_answer_as_a_serial_replay() {
    let s = sizes();
    let seed = env("AMPHORA_MW_SEED", 1);
    for storage in storages() {
        for class in CLASSES {
            for shared in [false, true] {
                let o = run_class(storage, class, shared, &s, seed);
                eprintln!(
                    "many_writers {storage:?} {class} shared={shared} writers={} committed={} refused={}",
                    s.writers, o.committed, o.refused
                );
                assert!(
                    o.committed > 0,
                    "{storage:?} {class} shared={shared}: nothing committed"
                );
            }
        }
    }
}
