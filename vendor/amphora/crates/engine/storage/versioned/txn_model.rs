//! Amphora: the shared model test of transactions (kiln plan 01a16b0f46fa72e384b8dd0065dc94f9,
//! step 2: `T-NS.2.1`): the same random transactions (puts, removes, reads, scans, savepoints,
//! rollbacks to them and releases of them, commits and rollbacks) run on the current engine,
//! through SQL on one table, and on the versioned storage, through its transactions on one tree;
//! every read and scan inside a transaction must answer the same on both.
//!
//! No anomaly: snapshots of the versioned storage opened at random points (inside a transaction
//! and between them) must each see exactly the rows committed when it opened, every time it is
//! read, however the transactions after it roll back, release or commit (no dirty or
//! non-repeatable read, no lost or phantom row); the committed rows are the current engine's,
//! read after each commit.
//!
//! `AMPHORA_NS2_SEEDS` sets how many seeds run (the coordinator's run: 10,000); a failing seed
//! replays alone with `AMPHORA_NS2_SEED`.

use super::blink::Descender;
use super::cursor::{rowid_key, KeyOrder};
use super::disk::Simulated;
use super::durable::Database as VDatabase;
use super::version::{Savepoint, Snapshot, Txn};
use crate::io::{MemoryIO, IO};
use crate::sync::Arc;
use crate::{ConnectOpts, Connection, Database, SqliteDialect};

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
}

type Rows = Vec<(i64, String)>;

fn rows(conn: &Arc<Connection>, sql: &str) -> Rows {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out = Vec::new();
    stmt.run_with_row_callback(|row| {
        out.push((row.get::<i64>(0)?, row.get::<String>(1)?));
        Ok(())
    })
    .unwrap();
    out
}

fn decode(found: Vec<(Vec<u8>, Vec<u8>)>) -> Rows {
    found
        .into_iter()
        .map(|(k, v)| {
            (
                super::cursor::key_rowid(&k).unwrap(),
                String::from_utf8(v).unwrap(),
            )
        })
        .collect()
}

/// One storage under test and the reference, side by side.
struct Pair {
    conn: Arc<Connection>,
    _db: Arc<Database>,
    vdb: VDatabase,
    d: Descender,
    txn: Option<Txn>,
    /// Open savepoints, oldest first: their SQL name and the versioned storage's.
    saves: Vec<(u64, Savepoint)>,
    names: u64,
    /// The rows committed, as the current engine reads them after each commit.
    committed: Rows,
    /// Open snapshots and the rows each must see.
    snaps: Vec<(Snapshot, Rows)>,
    /// What the run did, so a pass is known to have exercised it (non-vacuity).
    counts: Counts,
}

#[derive(Default, Debug)]
struct Counts {
    reads: u64,
    rollbacks_to: u64,
    rollbacks_to_rows: u64,
    snapshot_checks: u64,
    commits: u64,
    aborts: u64,
}

impl Pair {
    fn new(seed: u64) -> Self {
        #[allow(clippy::arc_with_non_send_sync)]
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        let order = KeyOrder::rowid();
        let vdb = VDatabase::open(Arc::new(Simulated::new(seed, 1).unwrap()), 4_000, 1, || {
            order.comparator()
        })
        .unwrap();
        Self {
            conn,
            _db: db,
            vdb,
            d: Descender::new(),
            txn: None,
            saves: Vec::new(),
            names: 0,
            committed: Vec::new(),
            snaps: Vec::new(),
            counts: Counts::default(),
        }
    }

    fn begin(&mut self) {
        if self.txn.is_none() {
            self.conn.execute("BEGIN").unwrap();
            self.txn = Some(self.vdb.begin().unwrap());
        }
    }

    fn txn(&mut self) -> &mut Txn {
        self.txn.as_mut().unwrap()
    }

    fn put(&mut self, k: i64, v: &str) {
        self.begin();
        self.conn
            .execute(format!("INSERT OR REPLACE INTO t VALUES ({k}, '{v}')"))
            .unwrap();
        let mut txn = self.txn.take().unwrap();
        self.vdb
            .put(&mut self.d, &mut txn, 0, &rowid_key(k), v.as_bytes())
            .unwrap();
        self.txn = Some(txn);
    }

    fn remove(&mut self, k: i64) {
        self.begin();
        self.conn
            .execute(format!("DELETE FROM t WHERE k = {k}"))
            .unwrap();
        let mut txn = self.txn.take().unwrap();
        self.vdb
            .remove(&mut self.d, &mut txn, 0, &rowid_key(k))
            .unwrap();
        self.txn = Some(txn);
    }

    /// The rows the open transaction (or, with none, the latest commit) sees, compared.
    fn scan(&mut self, k: Option<i64>, seed: u64, step: usize) {
        let sql = match k {
            Some(k) => format!("SELECT k, v FROM t WHERE k = {k}"),
            None => "SELECT k, v FROM t ORDER BY k".to_string(),
        };
        let reference = rows(&self.conn, &sql);
        let snap = match &self.txn {
            Some(txn) => txn.snapshot(),
            None => self.vdb.snapshot().unwrap(),
        };
        let found = match k {
            Some(k) => self
                .vdb
                .get(&mut self.d, &snap, 0, &rowid_key(k))
                .unwrap()
                .map(|v| vec![(rowid_key(k).to_vec(), v)])
                .unwrap_or_default(),
            None => self
                .vdb
                .scan(&mut self.d, &snap, 0, None, usize::MAX)
                .unwrap(),
        };
        if self.txn.is_none() {
            self.vdb.close_snapshot(snap);
        }
        assert_eq!(
            decode(found),
            reference,
            "seed {seed} step {step}: `{sql}` answers differently"
        );
        self.counts.reads += 1;
    }

    fn savepoint(&mut self) {
        self.begin();
        self.names += 1;
        let name = self.names;
        self.conn.execute(format!("SAVEPOINT s{name}")).unwrap();
        let sp = self.txn().savepoint();
        self.saves.push((name, sp));
    }

    fn rollback_to(&mut self, i: usize) {
        let Some(&(name, sp)) = self.saves.get(i) else {
            return;
        };
        self.conn.execute(format!("ROLLBACK TO s{name}")).unwrap();
        let mut txn = self.txn.take().unwrap();
        let undone = txn_changes(&txn, &sp);
        self.vdb.rollback_to(&mut self.d, &mut txn, sp).unwrap();
        self.txn = Some(txn);
        self.saves.truncate(i + 1);
        self.counts.rollbacks_to += 1;
        self.counts.rollbacks_to_rows += undone;
    }

    fn release(&mut self, i: usize) {
        let Some(&(name, _)) = self.saves.get(i) else {
            return;
        };
        // A release changes no row: the transaction began with BEGIN, so even the outermost
        // savepoint's release leaves it open.
        self.conn.execute(format!("RELEASE s{name}")).unwrap();
        self.saves.truncate(i);
    }

    fn commit(&mut self) {
        let Some(txn) = self.txn.take() else {
            return;
        };
        self.conn.execute("COMMIT").unwrap();
        self.vdb
            .commit(txn, &mut self.d, self.vdb.lane(), false)
            .unwrap();
        self.saves.clear();
        self.committed = rows(&self.conn, "SELECT k, v FROM t ORDER BY k");
        self.counts.commits += 1;
    }

    fn rollback(&mut self) {
        let Some(txn) = self.txn.take() else {
            return;
        };
        self.conn.execute("ROLLBACK").unwrap();
        self.vdb.abort(txn, &mut self.d).unwrap();
        self.saves.clear();
        self.counts.aborts += 1;
    }

    fn open_snapshot(&mut self) {
        let snap = self.vdb.snapshot().unwrap();
        self.snaps.push((snap, self.committed.clone()));
    }

    fn check_snapshots(&mut self, seed: u64, step: usize) {
        for (snap, want) in &self.snaps {
            let seen = decode(
                self.vdb
                    .scan(&mut self.d, snap, 0, None, usize::MAX)
                    .unwrap(),
            );
            assert_eq!(
                &seen, want,
                "seed {seed} step {step}: a snapshot sees other than what was committed when it opened"
            );
            self.counts.snapshot_checks += 1;
        }
    }

    fn close_snapshot(&mut self, i: usize) {
        if i < self.snaps.len() {
            let (snap, _) = self.snaps.remove(i);
            self.vdb.close_snapshot(snap);
        }
    }
}

/// Whether a rollback to `sp` takes back anything (non-vacuity: rollbacks that undo something).
fn txn_changes(txn: &Txn, sp: &Savepoint) -> u64 {
    u64::from(txn.savepoint() != *sp)
}

fn run(seed: u64, steps: usize) -> Counts {
    let mut rng = Rng(seed);
    let mut p = Pair::new(seed);
    for step in 0..steps {
        let k = i64::try_from(rng.below(48)).unwrap() - 8;
        match rng.below(100) {
            0..=29 => {
                let v = format!("v{}", rng.below(1_000));
                p.put(k, &v);
            }
            30..=41 => p.remove(k),
            42..=51 => p.scan(Some(k), seed, step),
            52..=56 => p.scan(None, seed, step),
            57..=64 => p.savepoint(),
            65..=71 => {
                let n = u64::try_from(p.saves.len()).unwrap();
                if n > 0 {
                    p.rollback_to(usize::try_from(rng.below(n)).unwrap());
                }
            }
            72..=74 => {
                let n = u64::try_from(p.saves.len()).unwrap();
                if n > 0 {
                    p.release(usize::try_from(rng.below(n)).unwrap());
                }
            }
            75..=81 => p.commit(),
            82..=84 => p.rollback(),
            85..=89 => p.open_snapshot(),
            90..=95 => p.check_snapshots(seed, step),
            _ => {
                let n = u64::try_from(p.snaps.len()).unwrap();
                if n > 0 {
                    p.close_snapshot(usize::try_from(rng.below(n)).unwrap());
                }
            }
        }
    }
    p.commit();
    p.scan(None, seed, steps);
    p.check_snapshots(seed, steps);
    p.counts
}

/// Do: the same random transactions with savepoints on the current engine and on the versioned
/// storage. Expect: every read and scan answers the same on both, and every snapshot sees exactly
/// the rows committed when it opened (`T-NS.2.1`).
#[test]
fn ns2_transactions_match_the_current_engine() {
    let seeds: u64 = std::env::var("AMPHORA_NS2_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let only: Option<u64> = std::env::var("AMPHORA_NS2_SEED")
        .ok()
        .and_then(|s| s.parse().ok());
    let mut total = Counts::default();
    for seed in only.map_or(0..seeds, |s| s..s + 1) {
        let c = run(seed, 400);
        total.reads += c.reads;
        total.rollbacks_to += c.rollbacks_to;
        total.rollbacks_to_rows += c.rollbacks_to_rows;
        total.snapshot_checks += c.snapshot_checks;
        total.commits += c.commits;
        total.aborts += c.aborts;
    }
    // Non-vacuity: the run did what it claims to check.
    assert!(total.reads > 0, "{total:?}");
    assert!(total.rollbacks_to_rows > 0, "{total:?}");
    assert!(total.snapshot_checks > 0, "{total:?}");
    assert!(total.commits > 0 && total.aborts > 0, "{total:?}");
}

/// Do: put a row, take a savepoint, change the row and add another, roll back to the savepoint,
/// commit. Expect: the first row as the savepoint found it, the second gone; a snapshot opened
/// before the commit sees neither (`AC-NS.2.3`).
#[test]
fn ns2_rollback_to_restores_rows_as_the_savepoint_found_them() {
    let order = KeyOrder::rowid();
    let vdb = VDatabase::open(Arc::new(Simulated::new(1, 1).unwrap()), 200, 1, || {
        order.comparator()
    })
    .unwrap();
    let mut d = Descender::new();
    let before = vdb.snapshot().unwrap();
    let mut txn = vdb.begin().unwrap();
    vdb.put(&mut d, &mut txn, 0, &rowid_key(1), b"a").unwrap();
    let sp = txn.savepoint();
    vdb.put(&mut d, &mut txn, 0, &rowid_key(1), b"b").unwrap();
    vdb.remove(&mut d, &mut txn, 0, &rowid_key(1)).unwrap();
    vdb.put(&mut d, &mut txn, 0, &rowid_key(2), b"c").unwrap();
    vdb.rollback_to(&mut d, &mut txn, sp).unwrap();
    let own = txn.snapshot();
    assert_eq!(
        vdb.scan(&mut d, &own, 0, None, usize::MAX).unwrap(),
        vec![(rowid_key(1).to_vec(), b"a".to_vec())]
    );
    // The savepoint stays good: change again, roll back again.
    vdb.put(&mut d, &mut txn, 0, &rowid_key(3), b"d").unwrap();
    vdb.rollback_to(&mut d, &mut txn, sp).unwrap();
    vdb.commit(txn, &mut d, vdb.lane(), false).unwrap();
    assert_eq!(
        vdb.scan(&mut d, &before, 0, None, usize::MAX).unwrap(),
        vec![]
    );
    let after = vdb.snapshot().unwrap();
    assert_eq!(
        vdb.scan(&mut d, &after, 0, None, usize::MAX).unwrap(),
        vec![(rowid_key(1).to_vec(), b"a".to_vec())]
    );
}

/// Do: roll a transaction back to a savepoint of another, and to one taken after a savepoint it
/// already rolled back to. Expect: both refused, the transaction's rows unchanged.
#[test]
fn ns2_rollback_to_refuses_a_savepoint_not_its_own() {
    let order = KeyOrder::rowid();
    let vdb = VDatabase::open(Arc::new(Simulated::new(2, 1).unwrap()), 200, 1, || {
        order.comparator()
    })
    .unwrap();
    let mut d = Descender::new();
    let other = vdb.begin().unwrap();
    let foreign = other.savepoint();
    let mut txn = vdb.begin().unwrap();
    let early = txn.savepoint();
    vdb.put(&mut d, &mut txn, 0, &rowid_key(1), b"a").unwrap();
    let late = txn.savepoint();
    assert!(vdb.rollback_to(&mut d, &mut txn, foreign).is_err());
    vdb.rollback_to(&mut d, &mut txn, early).unwrap();
    assert!(vdb.rollback_to(&mut d, &mut txn, late).is_err());
    assert_eq!(
        vdb.scan(&mut d, &txn.snapshot(), 0, None, usize::MAX)
            .unwrap(),
        vec![]
    );
    vdb.abort(txn, &mut d).unwrap();
    vdb.abort(other, &mut d).unwrap();
}
