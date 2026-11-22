//! ANALYZE on a database that holds a WITHOUT ROWID table (bug 5adc6889: refused at 29be700 with "ANALYZE on tables
//! without rowid is not supported"). The table's primary key is its B-tree (docs/design/planner.md decision 15), and
//! SQLite 3.54.0 counts it as an index named after its table, `w|w|4000 572 3 1`, beside every other index's row.
//! kiln's records_versions is WITHOUT ROWID, so until then no statistics reached any table of kiln's database
//! (AC-T1.13.4: reads at their latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect, Value,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own: one opened by the same name is shared across a process's tests.
fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect))
        .db_opts(DatabaseOpts::new().with_without_rowid(true));
    let name = format!(
        "without_rowid_analyze_{}.db",
        N.fetch_add(1, Ordering::Relaxed)
    );
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// The rows of `sql`, each column as text.
fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<String>> {
    conn.prepare(sql)
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row.iter().map(Value::to_string).collect())
        .collect()
}

/// The rows the plan of `sql` estimates it returns (`output_rows`).
fn estimate(conn: &Arc<Connection>, sql: &str) -> f64 {
    let text = rows(conn, &format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))[0][0].clone();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"))
}

const FILL: &str = "WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < 3999)";

/// Do: ANALYZE a database holding a WITHOUT ROWID table keyed by (a, b, c) and a rowid table with an index on (a, b),
/// 4,000 rows each; then a database whose WITHOUT ROWID key, (c, a), neither leads its table nor keeps its order.
/// Expect: each ANALYZE succeeds, and sqlite_stat1 holds SQLite 3.54.0's rows for the same DDL and data, word for word
/// (`SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx` in its shell).
#[test]
fn analyze_counts_a_without_rowid_tables_key_as_sqlite_does() {
    for (ddl, fill, want) in [
        (
            &[
                "CREATE TABLE w(a TEXT NOT NULL, b BLOB NOT NULL, c INTEGER NOT NULL, v BLOB, PRIMARY KEY (a, b, c)) WITHOUT ROWID",
                "CREATE TABLE r(a TEXT NOT NULL, b BLOB NOT NULL, c INTEGER NOT NULL, v BLOB)",
                "CREATE INDEX r_ab ON r(a, b)",
            ][..],
            &[
                format!("{FILL} INSERT INTO w(a, b, c, v) SELECT 'f' || (x / 3 % 7), CAST('k' || (1000000 + x / 3) AS BLOB), x % 3, zeroblob(16) FROM s"),
                "INSERT INTO r SELECT * FROM w".to_owned(),
            ][..],
            &[["r", "r_ab", "4000 572 3"], ["w", "w", "4000 572 3 1"]][..],
        ),
        (
            &["CREATE TABLE w(a TEXT NOT NULL, b BLOB NOT NULL, c INTEGER NOT NULL, v BLOB, PRIMARY KEY (c, a)) WITHOUT ROWID"][..],
            &[format!("{FILL} INSERT INTO w(a, b, c, v) SELECT 'f' || x, CAST('k' || x AS BLOB), x % 3, zeroblob(16) FROM s")][..],
            &[["w", "w", "4000 1334 1"]][..],
        ),
    ] {
        let conn = open();
        for sql in ddl {
            conn.execute(sql).unwrap();
        }
        for sql in fill {
            conn.execute(sql).unwrap();
        }
        conn.execute("ANALYZE").unwrap();
        let got = rows(&conn, "SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx");
        let want: Vec<Vec<String>> = want
            .iter()
            .map(|row| row.iter().map(|value| value.to_string()).collect())
            .collect();
        assert_eq!(got, want, "{ddl:?}");
    }
}

/// Do: ANALYZE a WITHOUT ROWID table keyed by (a, b, c), a rowid table with the same rows and an index on (a, b), and a
/// table of 70 values of `a`, then estimate the join of each on `a`. Expect: the two estimates the same, `a`'s domain
/// the statistics' 572 rows a value, so the row ANALYZE wrote for the key reaches the plans that read it, as the index's
/// row does. A join's domain reads them (decision 18); an equality on `a` reads each table's own sample, which two
/// tables draw apart (decision 20).
#[test]
fn a_without_rowid_keys_statistics_reach_the_planner() {
    let conn = open();
    for sql in [
        "CREATE TABLE w(a TEXT NOT NULL, b BLOB NOT NULL, c INTEGER NOT NULL, v BLOB, PRIMARY KEY (a, b, c)) WITHOUT ROWID",
        "CREATE TABLE r(a TEXT NOT NULL, b BLOB NOT NULL, c INTEGER NOT NULL, v BLOB)",
        "CREATE INDEX r_ab ON r(a, b)",
        "CREATE TABLE j(k TEXT NOT NULL)",
    ] {
        conn.execute(sql).unwrap();
    }
    conn.execute(format!("{FILL} INSERT INTO w(a, b, c, v) SELECT 'f' || (x / 3 % 7), CAST('k' || (1000000 + x / 3) AS BLOB), x % 3, zeroblob(16) FROM s")).unwrap();
    conn.execute("INSERT INTO r SELECT * FROM w").unwrap();
    conn.execute("INSERT INTO j SELECT DISTINCT a FROM w")
        .unwrap();
    for _ in 0..4 {
        conn.execute("INSERT INTO j SELECT * FROM j").unwrap();
    }
    conn.execute("DELETE FROM j WHERE rowid > 70").unwrap();
    let join = |table: &str| format!("SELECT * FROM j JOIN {table} ON {table}.a = j.k");
    let before = estimate(&conn, &join("w"));
    conn.execute("ANALYZE").unwrap();
    let keyed = estimate(&conn, &join("w"));
    let indexed = estimate(&conn, &join("r"));
    assert!(
        (keyed - indexed).abs() <= 1e-9 * indexed.abs().max(1.0),
        "the key's estimate {keyed} is not the index's {indexed} (before ANALYZE: {before})"
    );
    assert!(
        (keyed - before).abs() > 1e-9 * before.abs().max(1.0),
        "ANALYZE left the key's estimate at {before}"
    );
}
