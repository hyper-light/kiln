//! Keyset pagination by a row value on kiln's versions table: `WHERE (family, key, rev) > (?1, ?2,
//! ?3) ORDER BY family, key, rev LIMIT 8` seeks the primary key to the first key past the cursor and
//! stops at the LIMIT, as SQLite 3.54.0 seeks it, so a pass over the table reads each row once where
//! a scan from its first row reads every row before the page: a quadratic pass (the unify audit's
//! finding 3; docs/design/planner.md, decision 14). A row-value range answers as SQLite's does, NULLs
//! included (AC-T1.13.4: history reads at their latency; T-T1.7.1: every record read back).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A database of its own: one opened by the same name is shared across a process's tests.
fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let name = format!("row_value_seek_{}.db", N.fetch_add(1, Ordering::Relaxed));
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Kiln's versions table and index (`records_plans.rs`), with `rows` versions: three revisions of
/// each key, the keys dealt round seven families, the rowids in key order within a family.
fn versions(rows: i64) -> Arc<Connection> {
    let conn = open();
    conn.execute("CREATE TABLE versions (family TEXT NOT NULL, key BLOB NOT NULL, rev INTEGER NOT NULL, n INTEGER NOT NULL, value BLOB, PRIMARY KEY (family, key, rev))").unwrap();
    conn.execute("CREATE INDEX versions_by_rev ON versions (family, rev, n)")
        .unwrap();
    conn.execute(format!("WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < {}) INSERT INTO versions SELECT 'fam' || (x / 3 % 7), CAST('k' || (1000000 + x / 3) AS BLOB), x % 3, 0, zeroblob(16) FROM s", rows - 1)).unwrap();
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

/// The program's instructions: each opcode and its comment.
fn program(conn: &Arc<Connection>, sql: &str) -> Vec<(String, String)> {
    rows(conn, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|row| (row[1].to_string(), row[7].to_string()))
        .collect()
}

/// How many registers a seek's key takes, from its comment (`key=[first..last]`).
fn key_registers(comment: &str) -> Option<usize> {
    let (first, last) = comment
        .strip_prefix("key=[")?
        .strip_suffix(']')?
        .split_once("..")?;
    Some(last.parse::<usize>().ok()? + 1 - first.parse::<usize>().ok()?)
}

/// Do: plan the keyset page by a row value with each operator, its columns on either side, in both
/// directions, a prefix of the key, an equality then a row value, both bounds, and reading the value
/// column the key's index lacks through that index: named, and after ANALYZE unnamed (without
/// statistics the cost model prices the index's lookups above a scan and a sort, decision 14).
/// Expect: SQLite 3.54.0's plan for each, word for word (its EXPLAIN QUERY PLAN): a search of the
/// key from the bound and no sort; and a program that seeks the key's index by the whole row, never
/// rewinding it to filter from its first entry (AC-T1.13.4).
#[test]
fn a_row_value_bound_seeks_the_key_from_the_cursor() {
    let conn = versions(4_000);
    let cover = "SEARCH versions USING COVERING INDEX sqlite_autoindex_versions_1";
    let order = "ORDER BY family, key, rev LIMIT 8";
    let desc = "ORDER BY family DESC, key DESC, rev DESC LIMIT 8";
    for (sql, want, key) in [
        (format!("SELECT family, key, rev FROM versions WHERE (family, key, rev) > (?1, ?2, ?3) {order}"), format!("{cover} ((family,key,rev)>(?,?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (family, key, rev) >= (?1, ?2, ?3) {order}"), format!("{cover} ((family,key,rev)>(?,?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (?1, ?2, ?3) < (family, key, rev) {order}"), format!("{cover} ((family,key,rev)>(?,?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (family, key, rev) < (?1, ?2, ?3) {desc}"), format!("{cover} ((family,key,rev)<(?,?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (family, key, rev) <= (?1, ?2, ?3) {desc}"), format!("{cover} ((family,key,rev)<(?,?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (family, key) > (?1, ?2) {order}"), format!("{cover} ((family,key)>(?,?))"), 2),
        (format!("SELECT family, key, rev FROM versions WHERE family = ?1 AND (key, rev) > (?2, ?3) {order}"), format!("{cover} (family=? AND (key,rev)>(?,?))"), 3),
        (format!("SELECT family, key, rev FROM versions WHERE (family, key, rev) > (?1, ?2, ?3) AND (family, key, rev) < (?4, ?5, ?6) {order}"), format!("{cover} ((family,key,rev)>(?,?,?) AND (family,key,rev)<(?,?,?))"), 3),
        (format!("SELECT family, key, rev, value FROM versions INDEXED BY sqlite_autoindex_versions_1 WHERE (family, key, rev) > (?1, ?2, ?3) {order}"), "SEARCH versions USING INDEX sqlite_autoindex_versions_1 ((family,key,rev)>(?,?,?))".to_owned(), 3),
    ] {
        assert_eq!(plan(&conn, &sql), [want], "{sql}");
        let code = program(&conn, &sql);
        assert!(
            code.iter().all(|(op, _)| op != "Rewind" && op != "Last"),
            "{sql}: {code:?}"
        );
        let seeks: Vec<Option<usize>> = code
            .iter()
            .filter(|(op, _)| op.starts_with("Seek") && op != "SeekRowid" && op != "SeekEnd")
            .map(|(_, comment)| key_registers(comment))
            .collect();
        assert_eq!(seeks, [Some(key)], "{sql}: {code:?}");
    }
    conn.execute("ANALYZE").unwrap();
    let sql = format!("SELECT family, key, rev, value FROM versions WHERE (family, key, rev) > (?1, ?2, ?3) {order}");
    assert_eq!(
        plan(&conn, &sql),
        ["SEARCH versions USING INDEX sqlite_autoindex_versions_1 ((family,key,rev)>(?,?,?))"],
        "{sql}"
    );
}

/// One full keyset pass over `conn`'s versions, `page` rows at a time, in key order or (with
/// `desc`) its reverse, as kiln pages: the first page unbounded, each next one bounded by the row
/// value of the last row it read. Returns the rows read and the cursor positionings the bounded
/// pages made (their rewinds, steps and seeks, as the statement's counters count them).
fn keyset_pass(conn: &Arc<Connection>, page: usize, desc: bool) -> (Vec<Vec<Value>>, u64) {
    let (dir, op) = if desc { ("DESC", "<") } else { ("", ">") };
    let order = format!("ORDER BY family {dir}, key {dir}, rev {dir} LIMIT {page}");
    let mut seen = rows(
        conn,
        &format!("SELECT family, key, rev FROM versions {order}"),
    );
    let mut next = conn
        .prepare(format!(
            "SELECT family, key, rev FROM versions WHERE (family, key, rev) {op} (?1, ?2, ?3) {order}"
        ))
        .unwrap();
    let mut last = seen.last().cloned();
    while let Some(cursor) = last {
        next.reset().unwrap();
        for (i, value) in cursor.into_iter().enumerate() {
            next.bind_at(NonZero::new(i + 1).unwrap(), value).unwrap();
        }
        let got = next.run_collect_rows().unwrap();
        last = got.last().cloned();
        seen.extend(got);
    }
    let metrics = next.metrics();
    (seen, metrics.rows_read + metrics.btree_seeks)
}

/// Do: page through 4,000 versions eight at a time by the row value of each page's last row,
/// forwards and backwards. Expect: every row once, in key order (or its reverse); and each bounded
/// page positions the cursor at most LIMIT + 2 times (a seek to the cursor's own row, which the
/// bound leaves out, then a step to each row it returns and one past), so the pass is linear in its
/// rows: 5,000 positionings at most for its 500 pages, where a scan from the first row makes about a
/// million (8p for page p; AC-T1.13.4, T-T1.7.1).
#[test]
fn a_keyset_pass_reads_each_row_once() {
    let total = 4_000;
    let page = 8;
    let conn = versions(total);
    for desc in [false, true] {
        let dir = if desc { "DESC" } else { "" };
        let want = rows(
            &conn,
            &format!(
                "SELECT family, key, rev FROM versions ORDER BY family {dir}, key {dir}, rev {dir}"
            ),
        );
        assert_eq!(want.len(), total as usize);
        let (seen, positioned) = keyset_pass(&conn, page, desc);
        assert!(
            seen == want,
            "desc {desc}: the pass read {} rows",
            seen.len()
        );
        let pages = (total as u64).div_ceil(page as u64);
        let bound = pages * (page as u64 + 2);
        assert!(
            positioned <= bound,
            "desc {desc}: {positioned} cursor positionings for {total} rows, over the {bound} of a linear pass"
        );
    }
}

/// SQLite's row-value comparison of integers and NULLs (sqlite.org/rowvalue.html, section 2.1;
/// `codeVectorCompare` in sqlite3.c): element by element from the left, a NULL on either side
/// before the result is decided makes it NULL, the first unequal pair decides it, and rows equal
/// throughout compare by whether the operator includes equality.
fn row_compare(row: &[Option<i64>], op: &str, bound: &[Option<i64>]) -> Option<bool> {
    for (l, r) in row.iter().zip(bound) {
        let (l, r) = ((*l)?, (*r)?);
        if l != r {
            return Some(if op.starts_with('>') { l > r } else { l < r });
        }
    }
    Some(op.ends_with('='))
}

/// What each column of `t` holds: NULL and three integers.
const DOMAIN: [Option<i64>; 4] = [None, Some(0), Some(1), Some(2)];

/// A bound's elements: NULL, each integer `t` holds, and one past them.
const BOUNDS: [Option<i64>; 5] = [None, Some(0), Some(1), Some(2), Some(3)];

/// A value as SQL binds it.
fn bind(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::from_i64)
}

/// A result row of integers and NULLs.
fn ints(row: &[Value]) -> Vec<Option<i64>> {
    row.iter()
        .map(|v| match v {
            Value::Null => None,
            v => Some(v.to_string().parse().unwrap()),
        })
        .collect()
}

/// Do: on `t (a, b, c)` holding every combination of NULL, 0, 1 and 2 twice, indexed on (a, b, c),
/// on all three descending, on (a, b DESC, c) and on (a, b), read `(a, b, c) op (?1, ?2, ?3)` through
/// the index (INDEXED BY) for each operator and each of 125 bounds (NULL in each place among them),
/// ordered both ways, under a LIMIT and written with the row value on the right; and
/// `a = ?1 AND (b, c) op (?2, ?3)`. Expect: the rows SQLite's row-value comparison keeps
/// (`row_compare`, which SQLite 3.54.0's shell answers alike for all 500 bounds and operators on the
/// first index), in order, as the table NOT INDEXED returns them; and the row value sought as a range of
/// the index where its columns lead it in one direction, two columns of it where the index has two,
/// none past a column of the other direction (AC-T1.8.6: the differential suite's answers).
#[test]
fn a_row_value_range_answers_as_sqlite_does() {
    for (index, seeks_row) in [
        ("(a, b, c)", true),
        ("(a DESC, b DESC, c DESC)", true),
        ("(a, b DESC, c)", false),
        ("(a, b)", true),
    ] {
        let conn = open();
        conn.execute("CREATE TABLE t (a INTEGER, b INTEGER, c INTEGER)")
            .unwrap();
        let mut table = Vec::new();
        for a in DOMAIN {
            for b in DOMAIN {
                for c in DOMAIN {
                    table.push(vec![a, b, c]);
                    table.push(vec![a, b, c]);
                }
            }
        }
        let sql = |v: &Option<i64>| v.map_or("NULL".to_owned(), |v| v.to_string());
        let values: Vec<String> = table
            .iter()
            .map(|r| format!("({}, {}, {})", sql(&r[0]), sql(&r[1]), sql(&r[2])))
            .collect();
        conn.execute(format!("INSERT INTO t VALUES {}", values.join(", ")))
            .unwrap();
        conn.execute(format!("CREATE INDEX t_i ON t {index}"))
            .unwrap();
        let mut row_seeks = 0;
        for op in [">", ">=", "<", "<="] {
            let flipped = op
                .replacen('>', "!", 1)
                .replacen('<', ">", 1)
                .replacen('!', "<", 1);
            let forms = [
                (
                    format!("(a, b, c) {op} (?1, ?2, ?3)"),
                    "a, b, c",
                    None,
                    false,
                ),
                (
                    format!("(a, b, c) {op} (?1, ?2, ?3)"),
                    "a DESC, b DESC, c DESC",
                    None,
                    true,
                ),
                (
                    format!("(a, b, c) {op} (?1, ?2, ?3)"),
                    "a, b, c",
                    Some(5),
                    false,
                ),
                (
                    format!("(?1, ?2, ?3) {flipped} (a, b, c)"),
                    "a, b, c",
                    None,
                    false,
                ),
                (
                    format!("a = ?1 AND (b, c) {op} (?2, ?3)"),
                    "a, b, c",
                    None,
                    false,
                ),
            ];
            for (cond, order, limit, desc) in forms {
                let limit_sql = limit.map_or(String::new(), |n| format!(" LIMIT {n}"));
                let read = |hint: &str| {
                    format!("SELECT a, b, c FROM t{hint} WHERE {cond} ORDER BY {order}{limit_sql}")
                };
                let (seek, scan) = (read(" INDEXED BY t_i"), read(" NOT INDEXED"));
                let steps = plan(&conn, &seek);
                if steps
                    .iter()
                    .any(|s| s.contains("(a,b") || s.contains("(b,c)"))
                {
                    row_seeks += 1;
                }
                let mut seek_stmt = conn.prepare(&seek).unwrap();
                let mut scan_stmt = conn.prepare(&scan).unwrap();
                for x in BOUNDS {
                    for y in BOUNDS {
                        for z in BOUNDS {
                            let bound = [x, y, z];
                            let mut want: Vec<Vec<Option<i64>>> = table
                                .iter()
                                .filter(|r| {
                                    if cond.starts_with("a = ") {
                                        r[0].is_some()
                                            && r[0] == x
                                            && row_compare(&r[1..], op, &[y, z]) == Some(true)
                                    } else {
                                        row_compare(r, op, &bound) == Some(true)
                                    }
                                })
                                .cloned()
                                .collect();
                            want.sort();
                            if desc {
                                want.reverse();
                            }
                            want.truncate(limit.unwrap_or(want.len()));
                            for (stmt, sql) in [(&mut seek_stmt, &seek), (&mut scan_stmt, &scan)] {
                                stmt.reset().unwrap();
                                for (i, v) in bound.iter().enumerate() {
                                    stmt.bind_at(NonZero::new(i + 1).unwrap(), bind(*v))
                                        .unwrap();
                                }
                                let got: Vec<Vec<Option<i64>>> = stmt
                                    .run_collect_rows()
                                    .unwrap()
                                    .iter()
                                    .map(|r| ints(r))
                                    .collect();
                                assert_eq!(got, want, "index {index}: {sql} with {bound:?}");
                            }
                        }
                    }
                }
            }
        }
        if seeks_row {
            assert!(row_seeks > 0, "index {index}: no row value sought");
        } else {
            assert_eq!(
                row_seeks, 0,
                "index {index}: a row value sought past a column of the other direction"
            );
        }
    }
}

/// Do: count and sum the rowids of kiln's versions past a row-value bound by each operator, the
/// bound's elements of other types than their columns' (a revision as text and as a real, a key as
/// text, a family as an integer) or NULL. Expect: SQLite 3.54.0's answer for each (computed by its
/// shell on the same rows), the bound sought on the key's index by all three columns, as SQLite
/// seeks it (AC-T1.8.6).
#[test]
fn a_row_value_bound_converts_as_its_comparison_does() {
    let conn = versions(4_000);
    let key = "CAST('k1000101' AS BLOB)";
    for (bound, answers) in [
        // Each operator's count and rowid sum, for >, >=, < and <=.
        (
            format!("('fam3', {key}, 1)"),
            [
                (2237, 4557196),
                (2238, 4557501),
                (1762, 3444499),
                (1763, 3444804),
            ],
        ),
        (
            format!("('fam3', {key}, '1')"),
            [
                (2237, 4557196),
                (2238, 4557501),
                (1762, 3444499),
                (1763, 3444804),
            ],
        ),
        (
            format!("('fam3', {key}, 1.5)"),
            [
                (2237, 4557196),
                (2237, 4557196),
                (1763, 3444804),
                (1763, 3444804),
            ],
        ),
        (
            format!("('fam3', {key}, 'x')"),
            [
                (2236, 4556890),
                (2236, 4556890),
                (1764, 3445110),
                (1764, 3445110),
            ],
        ),
        (
            "('fam3', 'k1000101', 1)".to_owned(),
            [
                (2281, 4564000),
                (2281, 4564000),
                (1719, 3438000),
                (1719, 3438000),
            ],
        ),
        (
            "(3, x'00', 0)".to_owned(),
            [(4000, 8002000), (4000, 8002000), (0, 0), (0, 0)],
        ),
        (
            "('fam3', NULL, 1)".to_owned(),
            [
                (1710, 3422565),
                (1710, 3422565),
                (1719, 3438000),
                (1719, 3438000),
            ],
        ),
        (
            "(NULL, x'', 0)".to_owned(),
            [(0, 0), (0, 0), (0, 0), (0, 0)],
        ),
    ] {
        for (op, (count, sum)) in [">", ">=", "<", "<="].into_iter().zip(answers) {
            let sql = format!(
                "SELECT count(*), total(rowid) FROM versions WHERE (family, key, rev) {op} {bound}"
            );
            let got = rows(&conn, &sql);
            assert_eq!(
                (got[0][0].to_string(), got[0][1].to_string()),
                (count.to_string(), format!("{sum}.0")),
                "{sql}"
            );
            let words = format!("((family,key,rev){}(?,?,?))", &op[..1]);
            let steps = plan(&conn, &sql);
            assert!(
                steps.iter().any(|s| s.ends_with(&words)),
                "{sql}: {steps:?}, not {words}"
            );
        }
    }
}

/// The rows the plan of `sql` estimates it returns: its own `output_rows`, as the oracles' CERT and
/// order checks read them.
fn estimate(conn: &Arc<Connection>, sql: &str) -> f64 {
    let rows = rows(conn, &format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"));
    let text = rows[0][0].to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"))
}

/// Do: estimate a row-value range sought on the key's index and read NOT INDEXED, alone and in a
/// join whose other table bounds it (its FROM list in both orders, and each order forced by CROSS
/// JOIN). Expect: one estimate each way (docs/design/planner.md, decision 13: an access method
/// changes only the cost; decision 14: the bound is implied by its term, so it counts for nothing
/// beside it; AC-T1.1.2).
#[test]
fn a_row_value_bound_seeks_or_filters_to_one_estimate() {
    let conn = versions(4_000);
    conn.execute("CREATE TABLE cursors (family TEXT, key BLOB, rev INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO cursors VALUES ('fam3', x'6b31303030313031', 1), ('fam5', x'6b31303030323030', 0)")
        .unwrap();
    for (seek, scan) in [
        (
            "SELECT family FROM versions WHERE (family, key, rev) > (?1, ?2, ?3)",
            "SELECT family FROM versions NOT INDEXED WHERE (family, key, rev) > (?1, ?2, ?3)",
        ),
        (
            "SELECT family FROM versions WHERE family = ?1 AND (key, rev) <= (?2, ?3)",
            "SELECT family FROM versions NOT INDEXED WHERE family = ?1 AND (key, rev) <= (?2, ?3)",
        ),
    ] {
        let (s, f) = (estimate(&conn, seek), estimate(&conn, scan));
        assert!(
            (s - f).abs() <= 1e-9 * s.abs().max(f.abs()),
            "{seek} estimated {s}, {scan} {f}"
        );
    }
    let cond = "(v.family, v.key, v.rev) > (c.family, c.key, c.rev)";
    let forms = [
        format!("SELECT v.rev FROM cursors AS c, versions AS v WHERE {cond}"),
        format!("SELECT v.rev FROM versions AS v, cursors AS c WHERE {cond}"),
        format!("SELECT v.rev FROM cursors AS c CROSS JOIN versions AS v WHERE {cond}"),
        format!("SELECT v.rev FROM versions AS v CROSS JOIN cursors AS c WHERE {cond}"),
    ];
    let want = estimate(&conn, &forms[0]);
    for sql in &forms {
        let got = estimate(&conn, sql);
        assert!(
            (got - want).abs() <= 1e-9 * got.abs().max(want.abs()),
            "{sql}: estimated {got}, {want} as first written"
        );
    }
}
