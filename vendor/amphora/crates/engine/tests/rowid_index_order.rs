//! An ordered read under a LIMIT on a rowid table's index (the unify audit's E5, open at 327a8f9):
//! `ORDER BY family, key, rev LIMIT 8` with a row-value range, a range on the first column, or no
//! WHERE at all, reads the primary key's autoindex or an explicit index in its order and stops at
//! the LIMIT, as SQLite 3.54.0 plans it, where a scan of the table and a sort read every row for
//! eight (docs/design/planner.md, decisions 2 and 14: no notion of a LIMIT stopping an ordered
//! scan; AC-T1.13.4: history reads at their latency; T-T1.7.1: every record read back).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
    StatementStatusCounter as Counter, Value,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own: one opened by the same name is shared across a process's tests.
fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let name = format!("rowid_index_order_{}.db", N.fetch_add(1, Ordering::Relaxed));
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// The rows of `sql`, as the engine returns them.
fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    conn.prepare(sql).unwrap().run_collect_rows().unwrap()
}

/// The plan's steps, as EXPLAIN QUERY PLAN words them.
fn plan(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    rows(conn, &format!("EXPLAIN QUERY PLAN {sql}"))
        .into_iter()
        .map(|row| row[3].to_string())
        .collect()
}

/// Do: on a rowid table keyed by (family, key, rev), whose key is its autoindex, and on one with
/// the same columns and an explicit index on them, 4,000 rows each and no statistics, read eight
/// rows in key order three ways: past a row value, past a family, and from the start. Every read
/// takes the value column the index lacks. Expect: SQLite 3.54.0's plan for each, word for word (a
/// search of the index from its bound, or a scan of the index, and no sort), at most 16 rows read
/// for the eight, and the same eight rows as the table scanned and sorted (AC-T1.13.4, T-T1.7.1).
#[test]
fn an_ordered_read_under_a_limit_uses_a_rowid_tables_index() {
    let conn = open();
    for ddl in [
        "CREATE TABLE rp(family TEXT NOT NULL, key BLOB NOT NULL, rev INTEGER NOT NULL, seq INTEGER NOT NULL, value BLOB, PRIMARY KEY (family, key, rev))",
        "CREATE TABLE ri(family TEXT NOT NULL, key BLOB NOT NULL, rev INTEGER NOT NULL, seq INTEGER NOT NULL, value BLOB)",
        "CREATE INDEX ri_k ON ri(family, key, rev)",
    ] {
        conn.execute(ddl).unwrap();
    }
    for table in ["rp", "ri"] {
        conn.execute(format!("WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < 3999) INSERT INTO {table} SELECT 'fam' || (x / 3 % 7), CAST('k' || (1000000 + x / 3) AS BLOB), x % 3, x, zeroblob(16) FROM s")).unwrap();
    }
    for (table, index) in [("rp", "sqlite_autoindex_rp_1"), ("ri", "ri_k")] {
        let order = "ORDER BY family, key, rev LIMIT 8";
        for (filter, want) in [
            (
                "WHERE (family, key, rev) > ('fam3', CAST('k1000500' AS BLOB), 1)",
                format!("SEARCH {table} USING INDEX {index} ((family,key,rev)>(?,?,?))"),
            ),
            ("", format!("SCAN {table} USING INDEX {index}")),
            (
                "WHERE family > 'fam3'",
                format!("SEARCH {table} USING INDEX {index} (family>?)"),
            ),
        ] {
            let sql = format!("SELECT * FROM {table} {filter} {order}");
            assert_eq!(plan(&conn, &sql), [want], "{sql}");
            let mut statement = conn.prepare(&sql).unwrap();
            let got = statement.run_collect_rows().unwrap();
            let read = statement.stmt_status(Counter::RowsRead);
            assert!(read <= 16, "{sql}: {read} rows read for 8");
            let scanned = rows(
                &conn,
                &format!("SELECT * FROM {table} NOT INDEXED {filter} {order}"),
            );
            assert_eq!(got, scanned, "{sql}");
            assert_eq!(got.len(), 8, "{sql}");
        }
    }
}
