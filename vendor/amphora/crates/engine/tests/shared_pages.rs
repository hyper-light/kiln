//! The pages a database's connections share (`storage/shared_pages.rs`, Amphora's
//! `docs/design/store.md` decision 20): a page one connection read in is read by the others from
//! memory, and none of them ever reads a version its snapshot doesn't see: not a write in progress
//! (a writer copies a shared page before it writes it), not a file version a checkpoint has since
//! written over, not a frame of a write rolled back (AC-T1.1.1, AC-T1.1.6).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::collections::BTreeMap;
use std::sync::Arc;

fn open(dir: &std::path::Path, share: bool) -> Arc<Database> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("shared.db").to_str().unwrap(), options).unwrap();
    if share {
        db.share_pages(4096).unwrap();
    }
    db
}

fn connect(db: &Arc<Database>) -> Arc<Connection> {
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<(i64, String)> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| {
            let id = match &row[0] {
                Value::Numeric(Numeric::Integer(id)) => *id,
                other => panic!("id {other:?}"),
            };
            (id, row[1].to_string())
        })
        .collect()
}

fn text(conn: &Arc<Connection>, sql: &str) -> String {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows().unwrap()[0][0].to_string()
}

/// A table of `n` rows of about 200 bytes, `v` the row's id spelled.
fn table(conn: &Arc<Connection>, n: i64) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for first in (1..=n).step_by(200) {
        let values: Vec<String> = (first..(first + 200).min(n + 1))
            .map(|id| format!("({id}, 'v{id}-{}')", "x".repeat(190)))
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// Do: one reader reads a table in, from the log and then, after a checkpoint, from the file;
/// a second reader reads it after each. Expect: the second reads every page from memory (the
/// shared pages' hits grow by at least the pages the first added), and reads what the first read.
#[test]
fn a_page_one_reader_read_in_the_others_read_from_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    let writer = connect(&db);
    table(&writer, 2000);
    for checkpoint in [false, true] {
        if checkpoint {
            writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        }
        let first = connect(&db);
        let second = connect(&db);
        let before = db.shared_page_counts().unwrap();
        let read_first = rows(&first, "SELECT id, v FROM t ORDER BY id");
        let middle = db.shared_page_counts().unwrap();
        let read_second = rows(&second, "SELECT id, v FROM t ORDER BY id");
        let after = db.shared_page_counts().unwrap();
        assert_eq!(read_first.len(), 2000);
        assert_eq!(read_first, read_second);
        let added = middle.added - before.added;
        assert!(
            added > 20,
            "the first reader added {added} pages (checkpointed: {checkpoint})"
        );
        assert!(
            after.hits - middle.hits >= added,
            "the second reader hit {} times for {added} pages added (checkpointed: {checkpoint})",
            after.hits - middle.hits
        );
    }
}

/// Do: a reader reads a row in; a writer that never held the page reads the same row (its page is
/// then the shared buffer) and updates it in a transaction it hasn't committed; a third connection
/// reads it for the first time. Expect: the reader and the third read the old row while the write
/// is open and after it is rolled back, and the new row once a write of it commits.
#[test]
fn a_writer_copies_a_shared_page_before_it_writes_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    table(&connect(&db), 500);
    let writer = connect(&db);
    let reader = connect(&db);
    let row = "SELECT v FROM t WHERE id = 250";
    let old = format!("v250-{}", "x".repeat(190));
    assert_eq!(text(&reader, row), old);
    let hits = db.shared_page_counts().unwrap().hits;
    assert_eq!(text(&writer, row), old);
    assert!(
        db.shared_page_counts().unwrap().hits > hits,
        "the writer read the shared page"
    );
    writer.execute("BEGIN").unwrap();
    writer
        .execute("UPDATE t SET v = 'changed' WHERE id = 250")
        .unwrap();
    assert_eq!(text(&writer, row), "changed");
    assert_eq!(
        text(&reader, row),
        old,
        "a write in progress seen through a shared page"
    );
    let third = connect(&db);
    assert_eq!(
        text(&third, row),
        old,
        "a write in progress kept as the shared page"
    );
    writer.execute("ROLLBACK").unwrap();
    assert_eq!(text(&reader, row), old);
    assert_eq!(text(&third, row), old);
    writer
        .execute("UPDATE t SET v = 'committed' WHERE id = 250")
        .unwrap();
    assert_eq!(text(&reader, row), "committed");
    assert_eq!(text(&third, row), "committed");
    assert_eq!(text(&connect(&db), row), "committed");
}

/// Do: a reader reads a table in, from the log and then, after a checkpoint, from the file; a writer
/// caching 200 pages updates every 10th row of 2,000 in a transaction (about 20 rows to a leaf, so
/// two updates a leaf, 100 leaves), then rolls it back, then makes the updates again and commits.
/// Expect: the writer's reads in its transaction are found in the shared pages (their hits grow by
/// at least half the leaves it updates), and the reader reads the old rows until the commit and
/// the new ones after (decision 27: a writer reads committed versions from memory too).
#[test]
fn a_writer_in_a_transaction_reads_committed_pages_from_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    table(&connect(&db), 2000);
    let reader = connect(&db);
    let writer = connect(&db);
    writer.execute("PRAGMA cache_size = 200").unwrap();
    for checkpoint in [false, true] {
        if checkpoint {
            writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        }
        let old = rows(&reader, "SELECT id, v FROM t ORDER BY id");
        assert_eq!(old.len(), 2000);
        let updated: Vec<i64> = (5..=2000).step_by(10).collect();
        for outcome in ["ROLLBACK", "COMMIT"] {
            writer.execute("BEGIN").unwrap();
            let hits = db.shared_page_counts().unwrap().hits;
            for id in &updated {
                writer
                    .execute(format!(
                        "UPDATE t SET v = '{outcome}-{checkpoint}-{id}' WHERE id = {id}"
                    ))
                    .unwrap();
            }
            let found = db.shared_page_counts().unwrap().hits - hits;
            assert!(
                found >= 50,
                "the writer found {found} of its pages in memory (checkpoint {checkpoint})"
            );
            assert_eq!(
                rows(&reader, "SELECT id, v FROM t ORDER BY id"),
                old,
                "a write in progress seen"
            );
            writer.execute(outcome).unwrap();
        }
        for (id, v) in rows(&reader, "SELECT id, v FROM t ORDER BY id") {
            if updated.contains(&id) {
                assert_eq!(v, format!("COMMIT-{checkpoint}-{id}"), "row {id}");
            }
        }
    }
}

/// Do: with nothing read into the shared pages, a writer caching 200 pages updates every 10th row
/// of 2,000 in a transaction (two updates a leaf, 100 leaves) and commits; then a reader reads the
/// table. Expect: the writer adds at most its schema read to the shared pages, not the leaves it
/// read (pages it is about to change), the reader adds the table's pages, and reads every new row
/// (decision 27).
#[test]
fn a_writer_keeps_nothing_it_reads_in_the_shared_pages() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    table(&connect(&db), 2000);
    let writer = connect(&db);
    writer.execute("PRAGMA cache_size = 200").unwrap();
    let before = db.shared_page_counts().unwrap().added;
    writer.execute("BEGIN").unwrap();
    for id in (5..=2000).step_by(10) {
        writer
            .execute(format!("UPDATE t SET v = 'w-{id}' WHERE id = {id}"))
            .unwrap();
    }
    writer.execute("COMMIT").unwrap();
    let kept = db.shared_page_counts().unwrap().added - before;
    assert!(kept < 10, "the writer kept {kept} pages it read");
    let reader = connect(&db);
    let read = rows(&reader, "SELECT id, v FROM t ORDER BY id");
    let added = db.shared_page_counts().unwrap().added - before - kept;
    assert!(added >= 100, "the reader kept {added} pages");
    for (id, v) in read {
        if id >= 5 && (id - 5) % 10 == 0 {
            assert_eq!(v, format!("w-{id}"), "row {id}");
        }
    }
}

/// Do: readers read a checkpointed table from the file (shared file versions), a writer rewrites
/// every row, and a checkpoint writes the new versions into the file and starts the log again.
/// Expect: a reader that reads afterwards, from the file, reads every new row.
#[test]
fn a_checkpoint_drops_the_file_versions_it_writes_over() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    let writer = connect(&db);
    table(&writer, 1000);
    writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let early = connect(&db);
    assert_eq!(rows(&early, "SELECT id, v FROM t ORDER BY id").len(), 1000);
    writer.execute("UPDATE t SET v = 'new-' || id").unwrap();
    writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let late = connect(&db);
    let read = rows(&late, "SELECT id, v FROM t ORDER BY id");
    assert_eq!(read.len(), 1000);
    for (id, v) in &read {
        assert_eq!(
            v,
            &format!("new-{id}"),
            "row {id} read from a file version written over"
        );
    }
    let again = rows(&early, "SELECT id, v FROM t ORDER BY id");
    assert_eq!(again, read);
}

/// Do: a writer caching 200 pages (the least a cache keeps) updates every 25th row of 10,000 in a
/// transaction, a page each, 400 pages, reading back the row it updated 40 updates before (its
/// pages spill into the log before it commits, and come back from there, each page's spilled
/// frame its last), and
/// rolls it back; then makes the same updates with other values, the same way, and commits: its
/// pages land on the frames the first write used. Expect: a reader reads every committed value,
/// never a rolled-back one kept from a frame used again.
#[test]
fn a_write_rolled_back_leaves_no_shared_page() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    table(&connect(&db), 10_000);
    connect(&db)
        .execute("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let writer = connect(&db);
    writer.execute("PRAGMA cache_size = 200").unwrap();
    // A commit first, so both rounds append to the log's same generation (an empty log starts a
    // new one at its first write, and with it new keys).
    writer.execute("UPDATE t SET v = v WHERE id = 1").unwrap();
    for (round, outcome) in [("first", "ROLLBACK"), ("second", "COMMIT")] {
        writer.execute("BEGIN").unwrap();
        for id in (8i64..=10_000).step_by(25) {
            writer
                .execute(format!("UPDATE t SET v = '{round}-{id}' WHERE id = {id}"))
                .unwrap();
            if let Some(back) = id.checked_sub(25 * 40).filter(|back| *back >= 8) {
                assert_eq!(
                    text(&writer, &format!("SELECT v FROM t WHERE id = {back}")),
                    format!("{round}-{back}")
                );
            }
        }
        writer.execute(outcome).unwrap();
    }
    let reader = connect(&db);
    for (id, v) in rows(&reader, "SELECT id, v FROM t ORDER BY id") {
        if id >= 8 && (id - 8) % 25 == 0 {
            assert_eq!(v, format!("second-{id}"), "row {id}");
        } else {
            assert_eq!(v, format!("v{id}-{}", "x".repeat(190)), "row {id}");
        }
    }
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn body(id: i64, version: u64, len: usize) -> String {
    let mut s = format!("{id}:{version}:");
    while s.len() < len {
        s.push(char::from(b'a' + ((id as u64 + version) % 26) as u8));
    }
    s
}

/// 300 random steps on a database whose four readers share pages: the writer, caching 200 pages
/// (the least a cache keeps: its larger writes spill frames before they commit), writes
/// transactions of one row to hundreds and
/// rolls some back; checkpoints of every mode run; a random reader reads the table or rows by key,
/// each read checked against a model.
fn model(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path(), true);
    let writer = connect(&db);
    writer.execute("PRAGMA cache_size = 200").unwrap();
    writer
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")
        .unwrap();
    let readers: Vec<Arc<Connection>> = (0..4).map(|_| connect(&db)).collect();
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut model: BTreeMap<i64, String> = BTreeMap::new();
    let (mut version, mut next_id) = (0u64, 1i64);
    for step in 0..300 {
        match rng.below(10) {
            0..=4 => {
                let writes = if rng.below(4) == 0 {
                    1 + rng.below(300)
                } else {
                    1 + rng.below(8)
                };
                // Half the time the writer reads first, outside its write: the pages it then
                // writes may be shared buffers.
                if rng.below(2) == 0 {
                    let _ = rows(&writer, "SELECT id, body FROM t ORDER BY id");
                }
                let before = model.clone();
                writer.execute("BEGIN").unwrap();
                for _ in 0..writes {
                    version += 1;
                    let len = if rng.below(8) == 0 {
                        2000 + rng.below(5000) as usize
                    } else {
                        30 + rng.below(300) as usize
                    };
                    let existing = (!model.is_empty()).then(|| {
                        *model
                            .keys()
                            .nth(rng.below(model.len() as u64) as usize)
                            .unwrap()
                    });
                    match (rng.below(4), existing) {
                        (0, Some(id)) => {
                            writer
                                .execute(format!("DELETE FROM t WHERE id = {id}"))
                                .unwrap();
                            model.remove(&id);
                        }
                        (1, Some(id)) => {
                            let b = body(id, version, len);
                            writer
                                .execute(format!("UPDATE t SET body = '{b}' WHERE id = {id}"))
                                .unwrap();
                            model.insert(id, b);
                        }
                        _ => {
                            let (id, b) = (next_id, body(next_id, version, len));
                            next_id += 1;
                            writer
                                .execute(format!("INSERT INTO t VALUES ({id}, '{b}')"))
                                .unwrap();
                            model.insert(id, b);
                        }
                    }
                }
                if rng.below(5) == 0 {
                    writer.execute("ROLLBACK").unwrap();
                    model = before;
                } else {
                    writer.execute("COMMIT").unwrap();
                }
            }
            5 => {
                let mode = ["PASSIVE", "FULL", "RESTART", "TRUNCATE"][rng.below(4) as usize];
                writer
                    .execute(format!("PRAGMA wal_checkpoint({mode})"))
                    .unwrap();
            }
            _ => {
                let reader = &readers[rng.below(4) as usize];
                if rng.below(2) == 0 {
                    let got = rows(reader, "SELECT id, body FROM t ORDER BY id");
                    let want: Vec<(i64, String)> =
                        model.iter().map(|(k, v)| (*k, v.clone())).collect();
                    assert!(
                        got == want,
                        "seed {seed} step {step}: the table read {} rows, the model has {}",
                        got.len(),
                        want.len()
                    );
                } else {
                    for _ in 0..4 {
                        let id = 1 + rng.below(next_id as u64) as i64;
                        let got = rows(reader, &format!("SELECT id, body FROM t WHERE id = {id}"));
                        let want: Vec<(i64, String)> = model
                            .get(&id)
                            .map(|b| (id, b.clone()))
                            .into_iter()
                            .collect();
                        assert_eq!(got, want, "seed {seed} step {step}: row {id}");
                    }
                }
            }
        }
    }
    let counts = db.shared_page_counts().unwrap();
    assert!(
        counts.hits > 0 && counts.added > 0,
        "seed {seed}: {counts:?}"
    );
}

/// Do: the model's 300 steps for 16 seeds. Expect: every read the model's, and pages shared.
#[test]
fn readers_sharing_pages_never_read_a_version_they_dont_see() {
    for seed in 0..16 {
        model(seed);
    }
}

/// Two tables page for page, their rows spelled from `prefix`: the same pages, other contents.
fn table_of(conn: &Arc<Connection>, n: i64, prefix: &str) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for first in (1..=n).step_by(200) {
        let values: Vec<String> = (first..(first + 200).min(n + 1))
            .map(|id| format!("({id}, '{prefix}{id}-{}')", "x".repeat(190)))
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// Do: two databases whose tables have the same pages, frame for frame, but other rows share one
/// pool (`Database::share_pages_in`, one budget for both); read each in turn twice, from the log
/// and after a checkpoint of one of them. Expect: each reads its own rows alone, the second reads
/// from memory, and the pool keeps no more than its bound (decision 20).
#[test]
fn databases_sharing_one_pool_read_only_their_own_pages() {
    use amphora_engine::storage::shared_pages::SharedPages;
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let pool = Arc::new(SharedPages::new(4096));
    let (a, b) = (open(dir_a.path(), false), open(dir_b.path(), false));
    a.share_pages_in(&pool).unwrap();
    b.share_pages_in(&pool).unwrap();
    table_of(&connect(&a), 2000, "a");
    table_of(&connect(&b), 2000, "b");
    for checkpoint in [false, true] {
        if checkpoint {
            connect(&a)
                .execute("PRAGMA wal_checkpoint(TRUNCATE)")
                .unwrap();
        }
        for round in 0..2 {
            let before = pool.counts();
            for (db, prefix) in [(&a, "a"), (&b, "b")] {
                let read = rows(&connect(db), "SELECT id, v FROM t ORDER BY id");
                assert_eq!(read.len(), 2000, "{prefix}, checkpointed {checkpoint}");
                assert!(
                    read.iter()
                        .all(|(id, v)| v.starts_with(&format!("{prefix}{id}-"))),
                    "{prefix} read another's rows (checkpointed {checkpoint}, round {round})"
                );
            }
            let after = pool.counts();
            if round == 1 {
                assert!(
                    after.hits - before.hits > 40,
                    "the second reads came from memory: {before:?} -> {after:?}"
                );
            }
        }
    }
    assert!(pool.len() <= pool.capacity());
    assert_eq!(a.shared_page_counts(), b.shared_page_counts());
}
