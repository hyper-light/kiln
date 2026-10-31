//! What a transaction writes into the log when its pages outgrow its connection's cache, so they
//! spill into the log before it commits (Amphora's `docs/design/store.md`, decision 30): a spill
//! writes a page the transaction already wrote back over its frame, so the log holds a frame for
//! each page the transaction dirtied, and another for each its commit writes, however many of its
//! statements change those pages (AC-T1.1.1).

use amphora_engine::{
    CheckpointMode, ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions,
    PlatformIO, SqliteDialect, Statement, Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HEADER: u64 = 32;
const FRAME: u64 = 24 + 4096;

fn open(path: &Path) -> Arc<Database> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    Database::open(io, path.to_str().unwrap(), options).unwrap()
}

/// A connection as Amphora's writer keeps one: no checkpoint on commit, no statement journal
/// (`docs/design/store.md`, decisions 4 and 7), and the fewest pages a cache keeps (200).
fn connect(db: &Arc<Database>) -> Arc<Connection> {
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn.execute("PRAGMA cache_size = 200").unwrap();
    conn.set_statement_journals(false);
    conn
}

fn wal_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", path.display()))
}

/// The frames the log's file holds room for.
fn frames(path: &Path) -> u64 {
    let len = std::fs::metadata(wal_path(path)).map_or(0, |m| m.len());
    len.saturating_sub(HEADER) / FRAME
}

fn run(stmt: &mut Statement, params: &[i64]) {
    run_values(stmt, params.iter().map(|&p| Value::from_i64(p)));
}

fn run_values(stmt: &mut Statement, params: impl Iterator<Item = Value>) {
    stmt.reset().unwrap();
    for (i, p) in params.enumerate() {
        stmt.bind_at(NonZero::new(i + 1).unwrap(), p).unwrap();
    }
    stmt.run_ignore_rows().unwrap();
}

fn pairs(conn: &Arc<Connection>, sql: &str) -> Vec<(i64, i64)> {
    let int = |v: &Value| match v {
        Value::Numeric(Numeric::Integer(n)) => *n,
        other => panic!("not an integer: {other:?}"),
    };
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .iter()
        .map(|row| (int(&row[0]), int(&row[1])))
        .collect()
}

fn integrity(conn: &Arc<Connection>) {
    let rows = conn
        .prepare("PRAGMA integrity_check")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(
        matches!(&rows[..], [row] if matches!(&row[0], Value::Text(t) if t.as_str() == "ok")),
        "integrity_check: {rows:?}"
    );
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A trigram's code as kiln's log search makes one (three characters of 21 bits each).
fn gram(rng: &mut Rng, grams: u64) -> i64 {
    let n = rng.next() % grams;
    let (a, b, c) = (0x20 + n / 9025, 0x20 + n / 95 % 95, 0x20 + n % 95);
    ((a << 42) | (b << 21) | c) as i64
}

/// Do: with the log cut first, in one transaction on a connection caching 200 pages, index 1,200
/// documents of 25 postings each as kiln's log search does (a row into a rowid table with two
/// indexes and an upsert of its term's count for each posting, a savepoint for each document inside
/// one for each 40), one document rolled back to its savepoint, so the same pages are dirtied and
/// spilled into the log again and again (60,000 statements on a few hundred pages). Expect: past
/// its last commit the log holds no more frames than the pages the transaction dirtied before it
/// commits, and no more than twice that after (a frame its spills wrote and one its commit wrote
/// for each); every row reads back as the model has it, on a fresh connection and after opening the
/// database again.
#[test]
fn a_transactions_log_is_bounded_by_the_pages_it_dirtied_not_its_statements() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("spill.db");
    let mut postings: BTreeSet<(i64, i64)> = BTreeSet::new();
    let mut terms: BTreeMap<i64, i64> = BTreeMap::new();
    {
        let db = open(&path);
        let conn = connect(&db);
        conn.execute(
            "CREATE TABLE grams (gram INTEGER NOT NULL, doc INTEGER NOT NULL, PRIMARY KEY (gram, doc))",
        )
        .unwrap();
        conn.execute("CREATE INDEX grams_doc ON grams (doc, gram)")
            .unwrap();
        conn.execute("CREATE TABLE terms (gram INTEGER PRIMARY KEY, documents INTEGER NOT NULL)")
            .unwrap();
        // The log cut first: the transaction's frames begin its generation, the first of them
        // chained from the log's header.
        conn.checkpoint(CheckpointMode::Truncate {
            upper_bound_inclusive: None,
        })
        .unwrap();
        let before = frames(&path);

        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut statements = 0u64;
        let mut posting = conn
            .prepare("INSERT INTO grams (gram, doc) VALUES (?1, ?2)")
            .unwrap();
        let mut term = conn
            .prepare(
                "INSERT INTO terms (gram, documents) VALUES (?1, 1) \
                 ON CONFLICT (gram) DO UPDATE SET documents = documents + 1",
            )
            .unwrap();
        conn.execute("BEGIN").unwrap();
        for doc in 0..1200i64 {
            if doc % 40 == 0 {
                if doc > 0 {
                    conn.execute("RELEASE step").unwrap();
                }
                conn.execute("SAVEPOINT step").unwrap();
            }
            conn.execute("SAVEPOINT doc").unwrap();
            let mut grams: BTreeSet<i64> = BTreeSet::new();
            while grams.len() < 25 {
                grams.insert(gram(&mut rng, 4000));
            }
            for &g in &grams {
                run(&mut posting, &[g, doc]);
                run(&mut term, &[g]);
                statements += 2;
            }
            if doc == 700 {
                conn.execute("ROLLBACK TO doc").unwrap();
            } else {
                for &g in &grams {
                    postings.insert((g, doc));
                    *terms.entry(g).or_default() += 1;
                }
            }
            conn.execute("RELEASE doc").unwrap();
        }
        conn.execute("RELEASE step").unwrap();
        drop((posting, term));
        let dirtied = conn.dirty_pages() as u64;
        let during = frames(&path) - before;
        conn.execute("COMMIT").unwrap();
        let after = frames(&path) - before;
        eprintln!(
            "{statements} statements dirtied {dirtied} pages: {during} frames before the commit, \
             {after} after"
        );
        assert!(
            during <= dirtied,
            "before its commit the transaction's log holds {during} frames for the {dirtied} pages it \
             dirtied ({statements} statements)"
        );
        assert!(
            after <= 2 * dirtied,
            "after its commit the transaction's log holds {after} frames for the {dirtied} pages it \
             dirtied ({statements} statements)"
        );

        let want_postings: Vec<(i64, i64)> = postings.iter().copied().collect();
        let want_terms: Vec<(i64, i64)> = terms.iter().map(|(&g, &n)| (g, n)).collect();
        let fresh = connect(&db);
        assert_eq!(
            pairs(&fresh, "SELECT gram, doc FROM grams ORDER BY gram, doc"),
            want_postings
        );
        assert_eq!(
            pairs(&fresh, "SELECT gram, documents FROM terms ORDER BY gram"),
            want_terms
        );
    }
    // Opened again, the log is recovered from its frames: the commit's chain holds to its end.
    let db = open(&path);
    let conn = connect(&db);
    assert_eq!(
        pairs(&conn, "SELECT gram, doc FROM grams ORDER BY gram, doc"),
        postings.into_iter().collect::<Vec<_>>()
    );
    assert_eq!(
        pairs(&conn, "SELECT gram, documents FROM terms ORDER BY gram"),
        terms.into_iter().collect::<Vec<_>>()
    );
    integrity(&conn);
}

/// The tables of kiln's log search (kiln-unify's u05 migration), as the unify audit's harness
/// (finding data-1) makes them.
const LOG_SEARCH: &str = "
CREATE TABLE ci_log_indexed (leg TEXT PRIMARY KEY, at INTEGER NOT NULL, bytes INTEGER NOT NULL, first_row INTEGER, last_row INTEGER);
CREATE TABLE ci_log_docs (id INTEGER PRIMARY KEY, text TEXT NOT NULL, leg TEXT NOT NULL, step INTEGER NOT NULL, line INTEGER NOT NULL);
CREATE INDEX ci_log_docs_leg ON ci_log_docs (leg, id);
CREATE TABLE ci_log_grams (gram INTEGER NOT NULL, doc INTEGER NOT NULL, PRIMARY KEY (gram, doc));
CREATE INDEX ci_log_grams_doc ON ci_log_grams (doc, gram);
CREATE TABLE ci_log_terms (gram INTEGER PRIMARY KEY, documents INTEGER NOT NULL);
CREATE TABLE ci_log_search_ready (doc INTEGER PRIMARY KEY);
CREATE TABLE ci_log_search_pending (doc INTEGER PRIMARY KEY)";

/// kiln's log search's trigrams of a segment, as the audit's harness copies them.
fn trigrams(text: &str) -> BTreeSet<i64> {
    let lower = text.to_lowercase();
    let mut chars = lower.chars().map(|c| i64::from(u32::from(c)));
    let mut out = BTreeSet::new();
    let (Some(mut a), Some(mut b)) = (chars.next(), chars.next()) else {
        return out;
    };
    for c in chars {
        out.insert((a << 42) | (b << 21) | c);
        a = b;
        b = c;
    }
    out
}

/// One leg as the audit's harness indexes it: the corpus repeated to `mib` MiB, segments of 100
/// lines, each a row of `ci_log_docs` and its trigrams' postings and term counts, all in one
/// transaction (the step a savepoint in it), on a connection as Amphora's writer keeps one (1,024
/// pages, synchronous NORMAL, no statement journal), printed: the pages it dirtied, and the log's
/// frames past the last commit before and after its commit.
fn log_search_leg(corpus: &str, mib: usize) -> (u64, u64, u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs.db");
    let db = open(&path);
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
    conn.execute("PRAGMA synchronous = NORMAL").unwrap();
    conn.execute("PRAGMA cache_size = 1024").unwrap();
    conn.set_statement_journals(false);
    for sql in LOG_SEARCH.split(';') {
        conn.execute(sql.trim()).unwrap();
    }
    let before = frames(&path);
    let mut text = String::new();
    while text.len() < mib << 20 {
        text.push_str(corpus);
    }
    let mut end = mib << 20;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);

    let mut doc = conn
        .prepare("INSERT INTO ci_log_docs (text, leg, step, line) VALUES (?1, ?2, ?3, ?4)")
        .unwrap();
    let mut posting = conn
        .prepare("INSERT INTO ci_log_grams (gram, doc) VALUES (?1, ?2)")
        .unwrap();
    let mut term = conn
        .prepare(
            "INSERT INTO ci_log_terms (gram, documents) VALUES (?1, 1) \
             ON CONFLICT (gram) DO UPDATE SET documents = documents + 1",
        )
        .unwrap();
    let mut ready = conn
        .prepare("INSERT INTO ci_log_search_ready (doc) VALUES (?1)")
        .unwrap();
    let mut pending = conn
        .prepare("DELETE FROM ci_log_search_pending WHERE doc = ?1")
        .unwrap();
    let mut statements = 0u64;
    conn.execute("BEGIN").unwrap();
    conn.execute("SAVEPOINT step").unwrap();
    let lines: Vec<&str> = text.split('\n').collect();
    for (n, chunk) in lines.chunks(100).enumerate() {
        let body = chunk.join("\n");
        if body.trim().is_empty() {
            continue;
        }
        let line = (n * 100 + 1) as i64;
        run_values(
            &mut doc,
            [
                Value::from_text(body.clone()),
                Value::from_text("leg-0001"),
                Value::from_i64(0),
                Value::from_i64(line),
            ]
            .into_iter(),
        );
        let id = conn.last_insert_rowid();
        for g in trigrams(&body) {
            run(&mut posting, &[g, id]);
            run(&mut term, &[g]);
            statements += 2;
        }
        run(&mut ready, &[id]);
        run(&mut pending, &[id]);
        statements += 3;
    }
    conn.execute("RELEASE step").unwrap();
    drop((doc, posting, term, ready, pending));
    conn.execute(
        "INSERT OR REPLACE INTO ci_log_indexed (leg, at, bytes, first_row, last_row) \
         VALUES ('leg-0001', 0, 0, NULL, NULL)",
    )
    .unwrap();
    let dirtied = conn.dirty_pages() as u64;
    let during = frames(&path) - before;
    conn.execute("COMMIT").unwrap();
    let after = frames(&path) - before;
    let file = std::fs::metadata(&path).unwrap().len();
    eprintln!(
        "leg {mib} MiB: {statements} statements dirtied {dirtied} pages; the log holds {during} \
         frames past the last commit before its commit, {after} after ({:.1} MiB); the file {:.1} MiB",
        (after * FRAME) as f64 / 1048576.0,
        file as f64 / 1048576.0
    );
    (dirtied, during, after)
}

/// Do: index 1, 2, 4 and 8 MiB of kiln's build logs (the unify audit's corpus, named by
/// AMPHORA_LOG_CORPUS) as kiln's log search does, the audit harness's SQL, each leg in one
/// transaction on a connection as Amphora's writer keeps one. Expect: each leg's log past its last
/// commit within the pages it dirtied before its commit, and twice that after; each leg's numbers
/// printed (AC-T1.1.1).
#[test]
#[ignore = "the audit's corpus, named by AMPHORA_LOG_CORPUS"]
fn the_audits_log_search_legs_log_within_their_pages() {
    let corpus = std::fs::read_to_string(
        std::env::var("AMPHORA_LOG_CORPUS").expect("AMPHORA_LOG_CORPUS names the audit's corpus"),
    )
    .unwrap();
    // Every leg measured before any is checked: on a build without decision 30 each is a number to
    // record.
    let legs: Vec<(usize, (u64, u64, u64))> = [1, 2, 4, 8]
        .into_iter()
        .map(|mib| (mib, log_search_leg(&corpus, mib)))
        .collect();
    for (mib, (dirtied, during, after)) in legs {
        assert!(
            during <= dirtied && after <= 2 * dirtied,
            "leg {mib} MiB: {during} frames before its commit and {after} after, for {dirtied} pages"
        );
    }
}
