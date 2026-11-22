//! Keyset pagination on a WITHOUT ROWID table, as kiln's records import pages records_versions
//! (`20261023-1300_p6_records.sql`, `records/import.rs`): the first page in the primary key's order
//! and each next page from a row value read the primary key, which is the table itself and holds
//! every column, so neither scans the table to sort it: SQLite 3.54.0 plans them so, and a pass
//! reads each row once (the unify audit's finding 3 on the server's records; AC-T1.13.4: history
//! reads at their latency; T-T1.7.1: every record read back).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own, WITHOUT ROWID enabled as Amphora's Db opens its databases.
fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect))
        .db_opts(DatabaseOpts::new().with_without_rowid(true));
    let name = format!(
        "without_rowid_seek_{}.db",
        N.fetch_add(1, Ordering::Relaxed)
    );
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// kiln's records_versions and its feed index, with `rows` versions: three revisions of each key,
/// the keys dealt round seven families.
fn records(rows: i64) -> Arc<Connection> {
    let conn = open();
    conn.execute("CREATE TABLE records_versions(family TEXT NOT NULL,key BLOB NOT NULL,rev INTEGER NOT NULL,seq INTEGER NOT NULL,value BLOB,PRIMARY KEY(family,key,rev)) WITHOUT ROWID").unwrap();
    conn.execute("CREATE INDEX records_versions_feed ON records_versions(family,rev,seq)")
        .unwrap();
    conn.execute(format!("WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < {}) INSERT INTO records_versions SELECT 'fam' || (x / 3 % 7), CAST('k' || (1000000 + x / 3) AS BLOB), x % 3, x, zeroblob(16) FROM s", rows - 1)).unwrap();
    conn
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

const COLUMNS: &str = "SELECT family,key,rev,seq,value FROM records_versions";

/// Do: plan the import's first page and its next page by a row value, both in reverse, a range on
/// the key's first column, a row value over a prefix of the key, an equality then a row value, and
/// an ORDER BY of a column outside the key. Expect: SQLite 3.54.0's plan for each, word for word:
/// the table read in its key's order, scanned or searched from the bound, with no sort; a sort
/// only for the column outside the key; and the next page's program opening the table once and
/// never finding a row again, the key read through the table's own cursor (decision 15;
/// AC-T1.13.4).
#[test]
fn a_without_rowid_tables_key_orders_and_seeks_its_pages() {
    let conn = records(4_000);
    let order = "ORDER BY family,key,rev LIMIT 8";
    let desc = "ORDER BY family DESC,key DESC,rev DESC LIMIT 8";
    for (sql, want) in [
        (format!("{COLUMNS} {order}"), vec!["SCAN records_versions"]),
        (
            format!("{COLUMNS} WHERE (family,key,rev) > (?1,?2,?3) {order}"),
            vec!["SEARCH records_versions USING PRIMARY KEY ((family,key,rev)>(?,?,?))"],
        ),
        (format!("{COLUMNS} {desc}"), vec!["SCAN records_versions"]),
        (
            format!("{COLUMNS} WHERE (family,key,rev) < (?1,?2,?3) {desc}"),
            vec!["SEARCH records_versions USING PRIMARY KEY ((family,key,rev)<(?,?,?))"],
        ),
        (
            format!("{COLUMNS} WHERE family > ?1 {order}"),
            vec!["SEARCH records_versions USING PRIMARY KEY (family>?)"],
        ),
        (
            format!("{COLUMNS} WHERE (family,key) > (?1,?2) {order}"),
            vec!["SEARCH records_versions USING PRIMARY KEY ((family,key)>(?,?))"],
        ),
        (
            format!("{COLUMNS} WHERE family = ?1 AND (key,rev) > (?2,?3) {order}"),
            vec!["SEARCH records_versions USING PRIMARY KEY (family=? AND (key,rev)>(?,?))"],
        ),
        (
            format!("{COLUMNS} ORDER BY seq LIMIT 8"),
            vec!["SCAN records_versions", "USE SORTER FOR ORDER BY"],
        ),
    ] {
        assert_eq!(plan(&conn, &sql), want, "{sql}");
    }
    let next = format!("{COLUMNS} WHERE (family,key,rev) > (?1,?2,?3) {order}");
    let code: Vec<String> = rows(&conn, &format!("EXPLAIN {next}"))
        .into_iter()
        .map(|row| row[1].to_string())
        .collect();
    assert_eq!(
        code.iter().filter(|op| *op == "OpenRead").count(),
        1,
        "{code:?}"
    );
    assert!(code.iter().all(|op| op != "Found"), "{code:?}");
}

/// One full keyset pass over the records, `page` rows at a time, in key order or (with `desc`) its
/// reverse, as kiln's import pages: the first page unbounded, each next one bounded by the row value
/// of the last row it read. Returns the rows read, the bounded pages and the cursor positionings
/// they made (their rewinds, steps and seeks, as the statement's counters count them).
fn keyset_pass(conn: &Arc<Connection>, page: usize, desc: bool) -> (Vec<Vec<Value>>, u64, u64) {
    let (dir, op) = if desc { (" DESC", "<") } else { ("", ">") };
    let order = format!("ORDER BY family{dir},key{dir},rev{dir} LIMIT {page}");
    let mut seen = rows(conn, &format!("{COLUMNS} {order}"));
    let mut next = conn
        .prepare(format!(
            "{COLUMNS} WHERE (family,key,rev) {op} (?1,?2,?3) {order}"
        ))
        .unwrap();
    let mut last = seen.last().cloned();
    let mut pages = 0u64;
    while let Some(row) = last {
        next.reset().unwrap();
        for (i, value) in row.into_iter().take(3).enumerate() {
            next.bind_at(NonZero::new(i + 1).unwrap(), value).unwrap();
        }
        let got = next.run_collect_rows().unwrap();
        pages += 1;
        last = got.last().cloned();
        seen.extend(got);
    }
    let metrics = next.metrics();
    (seen, pages, metrics.rows_read + metrics.btree_seeks)
}

/// Do: page through 4,000 versions eight at a time by the row value of each page's last row,
/// forwards and backwards, as kiln's import does. Expect: every row once, in key order (or its
/// reverse), and each bounded page positions the cursor at most LIMIT + 2 times (a seek to the
/// cursor's own row, which the bound leaves out, a step to each row it returns and one past), so
/// the pass is linear in its rows, where a scan and a sort of the table for each page made 250
/// positionings a row at 2,000 versions (AC-T1.13.4, T-T1.7.1).
#[test]
fn a_keyset_pass_over_a_without_rowid_table_reads_each_row_once() {
    let total = 4_000;
    let page = 8;
    let conn = records(total);
    for desc in [false, true] {
        let dir = if desc { " DESC" } else { "" };
        let want = rows(
            &conn,
            &format!("{COLUMNS} ORDER BY family{dir},key{dir},rev{dir}"),
        );
        assert_eq!(want.len(), total as usize);
        let (seen, pages, positioned) = keyset_pass(&conn, page, desc);
        assert!(
            seen == want,
            "desc {desc}: the pass read {} rows",
            seen.len()
        );
        let bound = pages * (page as u64 + 2);
        assert!(
            positioned <= bound,
            "desc {desc}: {positioned} cursor positionings for {pages} bounded pages over {total} rows, over the {bound} of a linear pass"
        );
    }
}

/// SQLite's row-value comparison (sqlite.org/rowvalue.html, section 2.1; `codeVectorCompare` in
/// sqlite3.c): element by element from the left, a NULL on either side before the result is
/// decided makes it NULL, the first unequal pair decides it, and rows equal throughout compare by
/// whether the operator includes equality.
fn row_compare(row: &[i64], op: &str, bound: &[Option<i64>]) -> Option<bool> {
    for (l, r) in row.iter().zip(bound) {
        let r = (*r)?;
        if *l != r {
            return Some(if op.starts_with('>') { *l > r } else { *l < r });
        }
    }
    Some(op.ends_with('='))
}

/// A bound's elements: NULL, each integer the key holds, and one past them.
const BOUNDS: [Option<i64>; 5] = [None, Some(0), Some(1), Some(2), Some(3)];

/// The rows `sql` returns with `binds` bound, each as its integers.
fn ints(conn: &Arc<Connection>, sql: &str, binds: &[Option<i64>]) -> Vec<Vec<i64>> {
    let mut stmt = conn.prepare(sql).unwrap();
    for (i, value) in binds.iter().enumerate() {
        stmt.bind_at(
            NonZero::new(i + 1).unwrap(),
            value.map_or(Value::Null, Value::from_i64),
        )
        .unwrap();
    }
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row.iter().map(|v| v.to_string().parse().unwrap()).collect())
        .collect()
}

/// Do: on `w (a, b, c, v)` WITHOUT ROWID holding every combination of 0, 1 and 2 in its primary
/// key (a, b, c), declared ascending, descending and mixed, read `(a, b, c) op (?1, ?2, ?3)` for
/// each operator and each of 125 bounds (NULL in each place among them), ordered by the key both
/// ways; and `a = ?1 AND (b, c) op (?2, ?3)`. Expect: the rows SQLite's row-value comparison keeps
/// (`row_compare`), in order, and the same rows as the comparison written so that no key can seek
/// it (`+a`) (AC-T1.8.6: the differential suite's answers).
#[test]
fn a_row_value_range_on_a_without_rowid_key_answers_as_sqlite_does() {
    for key in ["a, b, c", "a DESC, b DESC, c DESC", "a, b DESC, c"] {
        let conn = open();
        conn.execute(format!("CREATE TABLE w (a INTEGER NOT NULL, b INTEGER NOT NULL, c INTEGER NOT NULL, v TEXT, PRIMARY KEY ({key})) WITHOUT ROWID")).unwrap();
        let mut table = Vec::new();
        for a in 0..3i64 {
            for b in 0..3i64 {
                for c in 0..3i64 {
                    conn.execute(format!("INSERT INTO w VALUES ({a}, {b}, {c}, '{a}{b}{c}')"))
                        .unwrap();
                    table.push(vec![a, b, c]);
                }
            }
        }
        for op in [">", ">=", "<", "<="] {
            for dir in ["", " DESC"] {
                let order = format!("ORDER BY a{dir}, b{dir}, c{dir}");
                for x in BOUNDS {
                    for y in BOUNDS {
                        for z in BOUNDS {
                            let bound = [x, y, z];
                            let mut want: Vec<Vec<i64>> = table
                                .iter()
                                .filter(|row| row_compare(row, op, &bound) == Some(true))
                                .cloned()
                                .collect();
                            if !dir.is_empty() {
                                want.reverse();
                            }
                            let sql = format!(
                                "SELECT a, b, c FROM w WHERE (a, b, c) {op} (?1, ?2, ?3) {order}"
                            );
                            assert_eq!(ints(&conn, &sql, &bound), want, "{key}: {sql} {bound:?}");
                            let filtered = format!(
                                "SELECT a, b, c FROM w WHERE (+a, b, c) {op} (?1, ?2, ?3) {order}"
                            );
                            assert_eq!(
                                ints(&conn, &filtered, &bound),
                                want,
                                "{key}: {filtered} {bound:?}"
                            );
                        }
                    }
                }
                for a in BOUNDS {
                    for y in BOUNDS {
                        for z in BOUNDS {
                            let mut want: Vec<Vec<i64>> = table
                                .iter()
                                .filter(|row| {
                                    a.is_some_and(|a| row[0] == a)
                                        && row_compare(&row[1..], op, &[y, z]) == Some(true)
                                })
                                .cloned()
                                .collect();
                            if !dir.is_empty() {
                                want.reverse();
                            }
                            let sql = format!(
                                "SELECT a, b, c FROM w WHERE a = ?1 AND (b, c) {op} (?2, ?3) {order}"
                            );
                            assert_eq!(
                                ints(&conn, &sql, &[a, y, z]),
                                want,
                                "{key}: {sql} {:?}",
                                [a, y, z]
                            );
                        }
                    }
                }
            }
        }
    }
}

/// The rows the plan of `sql` estimates it returns: its own `output_rows`, as the oracles' CERT
/// and order checks read them.
fn estimate(conn: &Arc<Connection>, sql: &str) -> f64 {
    let rows = rows(conn, &format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"));
    let text = rows[0][0].to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"))
}

/// The rows of `sql` with `binds` bound.
fn bound_rows(conn: &Arc<Connection>, sql: &str, binds: &[Value]) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    for (i, value) in binds.iter().enumerate() {
        stmt.bind_at(NonZero::new(i + 1).unwrap(), value.clone())
            .unwrap();
    }
    stmt.run_collect_rows().unwrap()
}

/// Do: estimate the import's next page and a row value after an equality, sought on the primary
/// key and read NOT INDEXED; and read the next page sought and NOT INDEXED with bounds of other
/// types: a TEXT and a REAL rev, an INTEGER family, the key given as TEXT. Expect: one estimate
/// each way (docs/design/planner.md, decision 13: an access method changes only the cost), and the
/// sought rows the rows read NOT INDEXED, each element compared as its column's affinity makes it
/// (AC-T1.1.2, AC-T1.8.6).
#[test]
fn a_without_rowid_key_seeks_or_filters_to_one_estimate_and_answer() {
    let conn = records(400);
    for (seek, scan) in [
        (
            "SELECT family FROM records_versions WHERE (family,key,rev) > (?1,?2,?3)",
            "SELECT family FROM records_versions NOT INDEXED WHERE (family,key,rev) > (?1,?2,?3)",
        ),
        (
            "SELECT family FROM records_versions WHERE family = ?1 AND (key,rev) <= (?2,?3)",
            "SELECT family FROM records_versions NOT INDEXED WHERE family = ?1 AND (key,rev) <= (?2,?3)",
        ),
    ] {
        let (s, f) = (estimate(&conn, seek), estimate(&conn, scan));
        assert!(
            (s - f).abs() <= 1e-9 * s.abs().max(f.abs()),
            "{seek} estimated {s}, {scan} {f}"
        );
    }
    let key = |s: &str| Value::Blob(s.as_bytes().to_vec());
    let text = |s: &str| Value::Text(s.into());
    for bound in [
        vec![text("fam3"), key("k1000050"), Value::from_i64(1)],
        vec![text("fam3"), key("k1000050"), text("1")],
        vec![text("fam3"), key("k1000050"), Value::from_f64(1.5)],
        vec![Value::from_i64(3), key("k1000050"), Value::from_i64(1)],
        vec![text("fam3"), text("k1000050"), Value::from_i64(1)],
        vec![text("fam3"), Value::Null, Value::from_i64(1)],
    ] {
        let seek = "SELECT family,key,rev FROM records_versions WHERE (family,key,rev) > (?1,?2,?3) ORDER BY family,key,rev";
        let scan = "SELECT family,key,rev FROM records_versions NOT INDEXED WHERE (family,key,rev) > (?1,?2,?3) ORDER BY family,key,rev";
        assert_eq!(
            bound_rows(&conn, seek, &bound),
            bound_rows(&conn, scan, &bound),
            "{bound:?}"
        );
    }
}

/// Do: on a WITHOUT ROWID table whose key's columns neither lead it nor keep its column order
/// (`v, c, b, a`, PRIMARY KEY (a, b, c)), read every column through the key: a scan in its order,
/// a row-value range, an equality then a range, and the key whole. Expect: each row and each column
/// what the table NOT INDEXED returns, in the key's order, since the key is read through the
/// table's own cursor, every column at the table's own position (decision 15; AC-T1.8.6).
#[test]
fn a_without_rowid_key_reads_every_column_where_its_columns_do_not_lead() {
    let conn = open();
    conn.execute("CREATE TABLE w (v TEXT, c INTEGER NOT NULL, b INTEGER NOT NULL, a INTEGER NOT NULL, PRIMARY KEY (a, b, c)) WITHOUT ROWID").unwrap();
    for a in 0..3i64 {
        for b in 0..3i64 {
            for c in 0..3i64 {
                conn.execute(format!("INSERT INTO w VALUES ('{a}{b}{c}', {c}, {b}, {a})"))
                    .unwrap();
            }
        }
    }
    let order = "ORDER BY a, b, c";
    for (filter, binds) in [
        ("", vec![]),
        (
            "WHERE (a, b, c) > (?1, ?2, ?3)",
            vec![Value::from_i64(1), Value::from_i64(0), Value::from_i64(2)],
        ),
        (
            "WHERE a = ?1 AND (b, c) >= (?2, ?3)",
            vec![Value::from_i64(2), Value::from_i64(1), Value::from_i64(1)],
        ),
        (
            "WHERE a = ?1 AND b = ?2 AND c = ?3",
            vec![Value::from_i64(0), Value::from_i64(2), Value::from_i64(1)],
        ),
    ] {
        let sought = format!("SELECT * FROM w {filter} {order}");
        let scanned = format!("SELECT * FROM w NOT INDEXED {filter} {order}");
        let want = bound_rows(&conn, &scanned, &binds);
        assert!(!want.is_empty(), "{scanned}");
        assert_eq!(bound_rows(&conn, &sought, &binds), want, "{sought}");
    }
}

/// Do: on WITHOUT ROWID keys that declare a collation (the column's own, and one named in the key
/// alone), order by the key's columns in and out of that collation, range over them, and filter
/// on a column outside the key under a LIMIT. Expect: the key read in its order only where the
/// ORDER BY names the key's collation, a range sought only in the key's collation, a sort
/// otherwise, and every answer as SQLite 3.54.0 gives it: rows ordered and compared by the
/// collation each term names, the filter applied before the LIMIT (decision 15; AC-T1.13.4).
#[test]
fn a_without_rowid_keys_collation_decides_its_order_and_its_ranges() {
    let conn = open();
    conn.execute("CREATE TABLE n(k TEXT COLLATE NOCASE, v INTEGER, PRIMARY KEY(k)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO n VALUES ('a',1),('B',2),('c',3),('D',4)")
        .unwrap();
    conn.execute(
        "CREATE TABLE p(k TEXT, r INTEGER, v TEXT, PRIMARY KEY(k COLLATE NOCASE, r)) WITHOUT ROWID",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO p VALUES ('a',1,'x'),('a',2,'y'),('B',1,'x'),('c',1,'y'),('c',2,'x')",
    )
    .unwrap();
    let text = |sql: &str| -> Vec<String> {
        rows(&conn, sql)
            .into_iter()
            .map(|row| {
                row.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    };
    let sorted = vec!["SCAN p", "USE SORTER FOR ORDER BY"];
    for (sql, want_plan, want_rows) in [
        (
            "SELECT k FROM n ORDER BY k",
            vec!["SCAN n"],
            vec!["a", "B", "c", "D"],
        ),
        (
            "SELECT k FROM n ORDER BY k COLLATE BINARY",
            vec!["SCAN n", "USE SORTER FOR ORDER BY"],
            vec!["B", "D", "a", "c"],
        ),
        (
            "SELECT k FROM n ORDER BY k COLLATE NOCASE DESC",
            vec!["SCAN n"],
            vec!["D", "c", "B", "a"],
        ),
        (
            "SELECT k FROM n WHERE k > 'b' ORDER BY k",
            vec!["SEARCH n USING PRIMARY KEY (k>?)"],
            vec!["c", "D"],
        ),
        (
            "SELECT k FROM n WHERE k > 'b' COLLATE BINARY ORDER BY k",
            vec!["SCAN n"],
            vec!["c"],
        ),
        // The key's NOCASE is the key's alone: the column, and so its ORDER BY, is BINARY.
        (
            "SELECT k, r FROM p ORDER BY k, r",
            sorted.clone(),
            vec!["B|1", "a|1", "a|2", "c|1", "c|2"],
        ),
        (
            "SELECT k, r FROM p WHERE (k, r) > ('a', 1) ORDER BY k, r LIMIT 3",
            sorted.clone(),
            vec!["a|2", "c|1", "c|2"],
        ),
        (
            "SELECT k, r FROM p WHERE (k, r) > ('a', 1) AND v = 'x' ORDER BY k, r LIMIT 2",
            sorted.clone(),
            vec!["c|2"],
        ),
        (
            "SELECT k, r FROM p WHERE k > 'a' AND v = 'x' ORDER BY k, r",
            sorted,
            vec!["c|2"],
        ),
    ] {
        assert_eq!(plan(&conn, sql), want_plan, "{sql}");
        assert_eq!(text(sql), want_rows, "{sql}");
    }
}
