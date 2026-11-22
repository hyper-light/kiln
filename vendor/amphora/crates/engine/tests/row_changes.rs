//! Statistics kept as rows change, and statements planned again as they move
//! (docs/design/planner.md, decision (lane pl9); PL.9). ANALYZE's row count stood still between ANALYZE
//! runs, and a statement prepared on it kept its plan however far its tables then grew.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
    Statement, Value,
};
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("rows.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Rows `from..=to` of `t`, `v` the row's number mod 97, in statements of 500 rows.
fn fill(conn: &Arc<Connection>, from: i64, to: i64) {
    let rows: Vec<String> = (from..=to).map(|i| format!("({i}, {})", i % 97)).collect();
    for chunk in rows.chunks(500) {
        conn.execute(format!("INSERT INTO t VALUES {}", chunk.join(", ")))
            .unwrap();
    }
}

/// `t` of 1,000 rows, analyzed.
fn analyzed_table(conn: &Arc<Connection>) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    fill(conn, 1, 1_000);
    conn.execute("ANALYZE").unwrap();
}

/// The planner's estimated rows for `sql`, from its EXPLAIN QUERY PLAN FORMAT=JSON.
fn estimate(conn: &Arc<Connection>, sql: &str) -> f64 {
    let mut stmt = conn
        .prepare(format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))
        .unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    let text = rows[0][0].to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"))
}

fn run(stmt: &mut Statement) -> Vec<Vec<Value>> {
    let rows = stmt.run_collect_rows().unwrap();
    stmt.reset().unwrap();
    rows
}

const ALL: &str = "SELECT id FROM t";
const COUNT_SOME: &str = "SELECT count(*) FROM t WHERE v < 50";

/// T-PL.9.1. Do: analyze 1,000 rows, insert 9,000, delete 5,000, ANALYZE again. Expect: the
/// table's count since ANALYZE follows each write, the planner's rows for the table with it (10,000,
/// then 5,000), and ANALYZE starts the count from zero, keeping the 5,000 it counted.
#[test]
fn t_pl_9_1_the_row_count_follows_inserts_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    analyzed_table(&conn);
    assert_eq!(conn.changes_since_analyze("t"), 0);
    assert_eq!(estimate(&conn, ALL), 1_000.0);
    fill(&conn, 1_001, 10_000);
    assert_eq!(conn.changes_since_analyze("t"), 9_000);
    assert_eq!(estimate(&conn, ALL), 10_000.0);
    conn.execute("DELETE FROM t WHERE id > 5000").unwrap();
    assert_eq!(conn.changes_since_analyze("t"), 4_000);
    assert_eq!(estimate(&conn, ALL), 5_000.0);
    conn.execute("ANALYZE").unwrap();
    assert_eq!(conn.changes_since_analyze("t"), 0);
    assert_eq!(estimate(&conn, ALL), 5_000.0);
}

/// T-PL.9.1. Do: prepare a statement on 1,000 analyzed rows, run it, add a few rows (within its
/// sample's error, 1 / sqrt(1,000)), run it; grow the table tenfold, run it twice. Expect: no plan
/// again for the few rows; exactly one for the tenfold growth, at its next run; and its answers
/// those of a statement prepared afterwards.
#[test]
fn t_pl_9_1_a_statement_plans_again_once_its_table_grows_past_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    analyzed_table(&conn);
    let mut stmt = conn.prepare(COUNT_SOME).unwrap();
    run(&mut stmt);
    let start = stmt.metrics().reprepares;
    fill(&conn, 1_001, 1_010);
    run(&mut stmt);
    assert_eq!(
        stmt.metrics().reprepares,
        start,
        "planned again within its error"
    );
    fill(&conn, 1_011, 10_000);
    let grown = run(&mut stmt);
    assert_eq!(stmt.metrics().reprepares, start + 1, "planned again once");
    let again = run(&mut stmt);
    assert_eq!(
        stmt.metrics().reprepares,
        start + 1,
        "planned again only once"
    );
    let mut fresh = conn.prepare(COUNT_SOME).unwrap();
    let expected = run(&mut fresh);
    assert_eq!(grown, expected);
    assert_eq!(again, expected);
}

/// T-PL.9.1. Do: shrink an analyzed table to a tenth after preparing a statement on it. Expect: the
/// statement plans again, a table that shrinks moving as one that grows.
#[test]
fn t_pl_9_1_a_statement_plans_again_once_its_table_shrinks_past_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    analyzed_table(&conn);
    let mut stmt = conn.prepare(COUNT_SOME).unwrap();
    run(&mut stmt);
    let start = stmt.metrics().reprepares;
    conn.execute("DELETE FROM t WHERE id > 100").unwrap();
    run(&mut stmt);
    assert_eq!(stmt.metrics().reprepares, start + 1);
}

/// T-PL.9.2. Do: 20 INSERT statements of 500 rows, one DELETE of many, one DELETE of none, and a
/// SELECT. Expect: one add to the table's count for each write that changed rows (21), none for the
/// rest: the write path's overhead, counted.
#[test]
fn t_pl_9_2_each_write_adds_its_rows_once() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INT)")
        .unwrap();
    let before = conn.row_change_folds();
    fill(&conn, 1, 10_000);
    conn.execute("DELETE FROM t WHERE v = 3").unwrap();
    conn.execute("DELETE FROM t WHERE v = 1000").unwrap();
    let mut select = conn.prepare(ALL).unwrap();
    run(&mut select);
    assert_eq!(conn.row_change_folds() - before, 21);
    let deleted = (1..=10_000).filter(|i| i % 97 == 3).count() as i64;
    assert_eq!(conn.changes_since_analyze("t"), 10_000 - deleted);
}
