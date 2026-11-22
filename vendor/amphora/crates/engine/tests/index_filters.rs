//! A loop over a non-covering index tests the WHERE terms that read only the index's columns
//! before the terms that read the table's, so a row those terms drop is never fetched from the
//! table: its deferred seek never runs (SQLite's `sqlite3WhereCodeOneLoopStart`, its first pass
//! over the terms an index covers; docs/design/executor.md decision 7; T-QT.3.4).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::sync::Arc;

const ROWS: i64 = 4_000;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("if.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, x INTEGER, y TEXT)")
        .unwrap();
    conn.execute("CREATE INDEX t_kx ON t (k, x)").unwrap();
    conn.execute("BEGIN").unwrap();
    for first in (1..=ROWS).step_by(500) {
        let values: Vec<String> = (first..(first + 500).min(ROWS + 1))
            .map(|id| {
                let y = if id % 3 == 0 {
                    "NULL".to_string()
                } else {
                    format!("'y{id}'")
                };
                format!("({id}, {}, {}, {y})", id % 200, id % 10)
            })
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();
    conn
}

/// The rows a query returns and the table rows it fetched through deferred seeks.
fn run(conn: &Arc<Connection>, sql: &str) -> (Vec<Vec<Value>>, u64) {
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    (rows, stmt.metrics().btree_deferred_seeks)
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Numeric(Numeric::Integer(i)) => *i,
        other => panic!("{other:?}"),
    }
}

/// Do: select a table column under a range on the index's first column, with a term on the
/// table's column written before a term on the index's second column. Expect: the answer the
/// model gives, and a table row fetched only for each index entry the index's term keeps, not
/// for every entry in the range (T-QT.3.4).
#[test]
fn a_term_the_index_covers_runs_before_the_table_row_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let sql = "SELECT id, y FROM t WHERE y IS NOT NULL AND k BETWEEN 10 AND 19 AND x = 3 \
               ORDER BY id";
    let (rows, seeks) = run(&conn, sql);
    let in_range = |id: i64| (10..=19).contains(&(id % 200));
    let kept: Vec<i64> = (1..=ROWS)
        .filter(|&id| in_range(id) && id % 10 == 3)
        .collect();
    let want: Vec<i64> = kept.iter().copied().filter(|id| id % 3 != 0).collect();
    assert_eq!(rows.iter().map(|r| int(&r[0])).collect::<Vec<_>>(), want);
    let entries_in_range = (1..=ROWS).filter(|&id| in_range(id)).count() as u64;
    assert!(
        kept.len() as u64 * 10 <= entries_in_range,
        "the index's term must drop most of the range for the test to mean anything"
    );
    assert_eq!(
        seeks,
        kept.len() as u64,
        "table rows fetched: {seeks}; the index's term keeps {} of the range's {entries_in_range}",
        kept.len()
    );
}

/// Do: the same query with its terms written in the other order, and one whose every term reads
/// the table. Expect: the same answer as the model's, and the first fetches as few rows as above
/// (T-QT.3.4).
#[test]
fn the_order_the_terms_are_written_in_does_not_change_the_rows_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let (forward, forward_seeks) = run(
        &conn,
        "SELECT id, y FROM t WHERE k BETWEEN 10 AND 19 AND x = 3 AND y IS NOT NULL ORDER BY id",
    );
    let (backward, backward_seeks) = run(
        &conn,
        "SELECT id, y FROM t WHERE y IS NOT NULL AND x = 3 AND k BETWEEN 10 AND 19 ORDER BY id",
    );
    assert_eq!(forward, backward);
    assert_eq!(forward_seeks, backward_seeks);
    let (table_only, _) = run(
        &conn,
        "SELECT id FROM t WHERE k BETWEEN 10 AND 19 AND y LIKE 'y1%' ORDER BY id",
    );
    let want: Vec<i64> = (1..=ROWS)
        .filter(|&id| (10..=19).contains(&(id % 200)) && id % 3 != 0)
        .filter(|id| id.to_string().starts_with('1'))
        .collect();
    assert_eq!(
        table_only.iter().map(|r| int(&r[0])).collect::<Vec<_>>(),
        want
    );
}
