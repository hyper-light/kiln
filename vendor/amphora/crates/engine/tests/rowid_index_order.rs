//! An ordered read under a LIMIT on a rowid table's index (the unify audit's E5, open at 327a8f9):
//! `ORDER BY family, key, rev LIMIT 8` with a row-value range, a range on the first column, or no
//! WHERE at all, reads the primary key's autoindex or an explicit index in its order and stops at
//! the LIMIT, as SQLite 3.54.0 plans it, where a scan of the table and a sort read every row for
//! eight (docs/design/planner.md, decisions 2 and 14: no notion of a LIMIT stopping an ordered
//! scan; AC-T1.13.4: history reads at their latency; T-T1.7.1: every record read back). So does a
//! LIMIT bound as a parameter, as kiln binds every LIMIT it writes (decision 17), which sorted at
//! 8b902d7.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
    Statement, StatementStatusCounter as Counter, Value,
};
use std::num::NonZero;
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

/// Two tables of 4,000 rows each: `rp` keyed by (family, key, rev), its key the autoindex, and `ri`
/// with an explicit index on the same columns.
fn keyed_tables() -> Arc<Connection> {
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
    conn
}

/// Do: on a rowid table keyed by (family, key, rev), whose key is its autoindex, and on one with
/// the same columns and an explicit index on them, 4,000 rows each and no statistics, read eight
/// rows in key order three ways: past a row value, past a family, and from the start. Every read
/// takes the value column the index lacks. Expect: SQLite 3.54.0's plan for each, word for word (a
/// search of the index from its bound, or a scan of the index, and no sort), at most 16 rows read
/// for the eight, and the same eight rows as the table scanned and sorted (AC-T1.13.4, T-T1.7.1).
#[test]
fn an_ordered_read_under_a_limit_uses_a_rowid_tables_index() {
    let conn = keyed_tables();
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

/// The rows `statement` returns with `values` bound in order, and the rows it read for them (the
/// steps of its cursors, as its RowsRead counter counts them).
fn run_bound(statement: &mut Statement, values: &[Value]) -> (Vec<Vec<Value>>, u64) {
    statement.reset().unwrap();
    statement.reset_stmt_status(Counter::RowsRead);
    for (i, value) in values.iter().enumerate() {
        statement
            .bind_at(NonZero::new(i + 1).unwrap(), value.clone())
            .unwrap();
    }
    let got = statement.run_collect_rows().unwrap();
    (got, statement.stmt_status(Counter::RowsRead))
}

/// Do: the three reads of each table of the test above, with the LIMIT and the bound each reads past
/// bound as parameters, as kiln binds them. Expect: SQLite 3.54.0's plan for each, word for word, from
/// the statement's EXPLAIN QUERY PLAN before any value is bound; at most 16 rows read for the eight;
/// and the same eight rows as the table scanned and sorted (decision 17; AC-T1.13.4, T-T1.7.1).
#[test]
fn an_ordered_read_under_a_parameter_limit_uses_a_rowid_tables_index() {
    let conn = keyed_tables();
    for (table, index) in [("rp", "sqlite_autoindex_rp_1"), ("ri", "ri_k")] {
        let order = "ORDER BY family, key, rev";
        for (filter, mut values, want) in [
            (
                "WHERE (family, key, rev) > (?1, ?2, ?3)",
                vec![
                    Value::from_text("fam3"),
                    Value::from_slice(b"k1000500").unwrap(),
                    Value::from_i64(1),
                ],
                format!("SEARCH {table} USING INDEX {index} ((family,key,rev)>(?,?,?))"),
            ),
            ("", vec![], format!("SCAN {table} USING INDEX {index}")),
            (
                "WHERE family > ?1",
                vec![Value::from_text("fam3")],
                format!("SEARCH {table} USING INDEX {index} (family>?)"),
            ),
        ] {
            let limit = values.len() + 1;
            let sql = format!("SELECT * FROM {table} {filter} {order} LIMIT ?{limit}");
            assert_eq!(plan(&conn, &sql), [want], "{sql}");
            values.push(Value::from_i64(8));
            let (got, read) = run_bound(&mut conn.prepare(&sql).unwrap(), &values);
            assert!(read <= 16, "{sql}: {read} rows read for 8");
            let scanned =
                format!("SELECT * FROM {table} NOT INDEXED {filter} {order} LIMIT ?{limit}");
            let (scanned, _) = run_bound(&mut conn.prepare(&scanned).unwrap(), &values);
            assert_eq!(got, scanned, "{sql}");
            assert_eq!(got.len(), 8, "{sql}");
        }
    }
}

/// Do: page through each table's 4,000 rows eight at a time as kiln pages, every column read: the
/// first page from the start, each next one past the row value of the last row read, the LIMIT bound
/// as a parameter and one prepared statement re-bound for every page, forwards and backwards.
/// Expect: every row once, in key order (or its reverse), as the table scanned and sorted returns
/// them; and each page reads at most LIMIT + 2 rows (a step to each row it returns and one past), so
/// the pass is linear in its rows, where a scan and a sort read all 4,000 for each page (decision
/// 17; AC-T1.13.4, T-T1.7.1).
#[test]
fn a_keyset_pass_with_a_bound_limit_reads_each_row_once() {
    let page: u64 = 8;
    let conn = keyed_tables();
    for table in ["rp", "ri"] {
        for desc in [false, true] {
            let (dir, op) = if desc { ("DESC", "<") } else { ("", ">") };
            let order = format!("ORDER BY family {dir}, key {dir}, rev {dir}");
            let want = rows(&conn, &format!("SELECT * FROM {table} NOT INDEXED {order}"));
            assert_eq!(want.len(), 4_000);
            let limit = Value::from_i64(i64::try_from(page).unwrap());
            let mut first = conn
                .prepare(format!("SELECT * FROM {table} {order} LIMIT ?1"))
                .unwrap();
            let mut next = conn
                .prepare(format!(
                    "SELECT * FROM {table} WHERE (family, key, rev) {op} (?1, ?2, ?3) {order} LIMIT ?4"
                ))
                .unwrap();
            let (mut seen, mut most) = run_bound(&mut first, std::slice::from_ref(&limit));
            let mut last = seen.last().cloned();
            while let Some(row) = last {
                let mut values: Vec<Value> = row.into_iter().take(3).collect();
                values.push(limit.clone());
                let (got, read) = run_bound(&mut next, &values);
                most = most.max(read);
                last = got.last().cloned();
                seen.extend(got);
            }
            assert!(
                seen == want,
                "{table} desc {desc}: the pass read {} rows",
                seen.len()
            );
            assert!(
                most <= page + 2,
                "{table} desc {desc}: a page read {most} rows for {page}"
            );
        }
    }
}

/// Do: kiln's conversation feed on kiln's own DDL (`crates/kiln/src/convo/sql.rs`, kiln dc19ac9e52):
/// `records` keyed by (session, source, n) with `records_by_rev`, 4,000 record lines and no
/// statistics; a page of the lines past a revision (`FEED_RECORDS`) and of those below one, newest
/// first (`RECORDS_BELOW`), each LIMIT bound. Expect: SQLite 3.54.0's plan for both, a search of
/// `records_by_rev`, word for word; at most LIMIT + 2 rows read for a page of 50; and the rows of the
/// same read NOT INDEXED (decision 17; AC-T1.13.4).
#[test]
fn kilns_feed_pages_search_records_by_rev() {
    let conn = open();
    conn.execute("CREATE TABLE records (session TEXT NOT NULL, source TEXT NOT NULL, n INTEGER NOT NULL, line_end INTEGER NOT NULL, line TEXT NOT NULL, rev INTEGER NOT NULL, PRIMARY KEY (session, source, n))").unwrap();
    conn.execute("CREATE INDEX records_by_rev ON records (rev)")
        .unwrap();
    conn.execute("WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < 3999) INSERT INTO records SELECT 's' || (x % 13), 'claude', x, x, 'line ' || x, x * 2 + 1 FROM s").unwrap();
    for (columns, filter, order, want) in [
        (
            "session, source, n, line_end, line, rev",
            "rev > ?1",
            "rev",
            "SEARCH records USING INDEX records_by_rev (rev>?)",
        ),
        (
            "session, source, n, line, rev",
            "rev < ?1",
            "rev DESC",
            "SEARCH records USING INDEX records_by_rev (rev<?)",
        ),
    ] {
        let sql = format!("SELECT {columns} FROM records WHERE {filter} ORDER BY {order} LIMIT ?2");
        assert_eq!(plan(&conn, &sql), [want], "{sql}");
        let values = [Value::from_i64(4_001), Value::from_i64(50)];
        let (got, read) = run_bound(&mut conn.prepare(&sql).unwrap(), &values);
        assert!(read <= 52, "{sql}: {read} rows read for 50");
        let scanned = format!(
            "SELECT {columns} FROM records NOT INDEXED WHERE {filter} ORDER BY {order} LIMIT ?2"
        );
        let (scanned, _) = run_bound(&mut conn.prepare(&scanned).unwrap(), &values);
        assert_eq!(got, scanned, "{sql}");
        assert_eq!(got.len(), 50, "{sql}");
    }
}
