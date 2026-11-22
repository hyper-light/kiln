//! A reader keeps its page cache across other connections' commits, forgetting only the pages
//! they wrote (Amphora's `docs/design/store.md`, decision 18). Whatever the commits between its
//! reads, it never reads a row as it was before a commit its read transaction starts after:
//! updates that grow, shrink and spill rows to overflow pages, inserts that split pages, deletes
//! that merge them, transactions of one row and of hundreds, checkpoints of every mode and restarts
//! of the log, in page-a-frame logs and packed ones, and reader caches smaller and larger than
//! what each commit writes. Every read is checked against a model (AC-T1.1.1, AC-T1.1.6).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, LogFormat, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::collections::BTreeMap;
use std::sync::Arc;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A row's body: `len` bytes of `id`'s letter at `version`, so a stale row reads differently.
fn body(id: i64, version: u64, len: usize) -> String {
    let letter = char::from(b'a' + ((id as u64 + version) % 26) as u8);
    let mut s = format!("{id}:{version}:");
    while s.len() < len {
        s.push(letter);
    }
    s
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

fn run(seed: u64, format: LogFormat, reader_pages: i64) {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("cache.db").to_str().unwrap(), options).unwrap();
    db.wal_format(format).unwrap();
    let writer = db.connect(ConnectOpts::new()).unwrap();
    writer.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    let reader = db.connect(ConnectOpts::new()).unwrap();
    reader
        .execute(format!("PRAGMA cache_size = {reader_pages}"))
        .unwrap();
    writer
        .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")
        .unwrap();

    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut model: BTreeMap<i64, String> = BTreeMap::new();
    let mut version = 0u64;
    let mut next_id = 1i64;
    let mut reads = 0u32;
    for step in 0..400 {
        match rng.below(12) {
            // A transaction of one to many writes.
            0..=5 => {
                let writes = if rng.below(4) == 0 {
                    1 + rng.below(300)
                } else {
                    1 + rng.below(8)
                };
                let before = model.clone();
                writer.execute("BEGIN").unwrap();
                for _ in 0..writes {
                    version += 1;
                    // Mostly small rows, some a page and more (overflow pages).
                    let len = match rng.below(10) {
                        0 => 3000 + rng.below(6000) as usize,
                        1..=2 => 400 + rng.below(800) as usize,
                        _ => 20 + rng.below(100) as usize,
                    };
                    let existing = (!model.is_empty()).then(|| {
                        *model
                            .keys()
                            .nth(rng.below(model.len() as u64) as usize)
                            .unwrap()
                    });
                    match (rng.below(5), existing) {
                        (0, Some(id)) => {
                            writer
                                .execute(format!("DELETE FROM t WHERE id = {id}"))
                                .unwrap();
                            model.remove(&id);
                        }
                        (1..=2, Some(id)) => {
                            let b = body(id, version, len);
                            writer
                                .execute(format!("UPDATE t SET body = '{b}' WHERE id = {id}"))
                                .unwrap();
                            model.insert(id, b);
                        }
                        _ => {
                            let id = next_id;
                            next_id += 1;
                            let b = body(id, version, len);
                            writer
                                .execute(format!("INSERT INTO t VALUES ({id}, '{b}')"))
                                .unwrap();
                            model.insert(id, b);
                        }
                    }
                }
                if rng.below(10) == 0 {
                    writer.execute("ROLLBACK").unwrap();
                    model = before;
                } else {
                    writer.execute("COMMIT").unwrap();
                }
            }
            6 => {
                let mode = ["PASSIVE", "FULL", "RESTART", "TRUNCATE"][rng.below(4) as usize];
                writer
                    .execute(format!("PRAGMA wal_checkpoint({mode})"))
                    .unwrap();
            }
            // The reader: the whole table, or a few rows by key.
            _ => {
                reads += 1;
                if rng.below(2) == 0 {
                    let got = rows(&reader, "SELECT id, body FROM t ORDER BY id");
                    let want: Vec<(i64, String)> =
                        model.iter().map(|(k, v)| (*k, v.clone())).collect();
                    assert!(got == want, "seed {seed} {format:?} pages {reader_pages} step {step}: the table read {} rows, the model has {}", got.len(), want.len());
                } else {
                    for _ in 0..5 {
                        let id = 1 + rng.below(next_id as u64) as i64;
                        let got = rows(&reader, &format!("SELECT id, body FROM t WHERE id = {id}"));
                        let want: Vec<(i64, String)> = model
                            .get(&id)
                            .map(|b| (id, b.clone()))
                            .into_iter()
                            .collect();
                        assert_eq!(
                            got, want,
                            "seed {seed} {format:?} pages {reader_pages} step {step}: row {id}"
                        );
                    }
                }
            }
        }
    }
    assert!(reads > 50, "the reader read {reads} times");
}

/// Do: 400 random steps of writes, checkpoints and reads for each of 12 seeds. Expect: every read
/// the model's. A page-a-frame log, a reader that caches 200 pages (the least a cache keeps): the
/// larger commits write more than it holds and it forgets everything, the others page by page.
#[test]
fn a_small_cache_never_reads_a_row_from_before_a_commit_it_follows() {
    for seed in 0..12 {
        run(seed, LogFormat::Pages, 200);
    }
}

/// The same with a reader that caches 2,000 pages: it forgets page by page.
#[test]
fn a_large_cache_never_reads_a_row_from_before_a_commit_it_follows() {
    for seed in 0..12 {
        run(seed, LogFormat::Pages, 2000);
    }
}

/// The same in a packed log (decision 16), its records' pages listed at their first slots.
#[test]
fn a_small_cache_on_a_packed_log_never_reads_a_stale_row() {
    for seed in 0..12 {
        run(seed, LogFormat::Packed, 200);
    }
}

/// The same in a packed log, with a reader that caches 2,000 pages.
#[test]
fn a_large_cache_on_a_packed_log_never_reads_a_stale_row() {
    for seed in 0..12 {
        run(seed, LogFormat::Packed, 2000);
    }
}

/// The same in a mixed log (decision 37): its records listed at their first slots, its spills'
/// page frames at theirs.
#[test]
fn a_small_cache_on_a_mixed_log_never_reads_a_stale_row() {
    for seed in 0..12 {
        run(seed, LogFormat::Mixed, 200);
    }
}

/// The same in a mixed log, with a reader that caches 2,000 pages.
#[test]
fn a_large_cache_on_a_mixed_log_never_reads_a_stale_row() {
    for seed in 0..12 {
        run(seed, LogFormat::Mixed, 2000);
    }
}
