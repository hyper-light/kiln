//! A GLOB with a constant pattern on an indexed text column seeks the range of its literal prefix
//! (`docs/design/planner.md`, decision 3): what it reads is what a scan of the same rows reads, for
//! patterns of every shape, and a BLOB matches neither GLOB nor LIKE (AC-T1.13.4: reads at their
//! latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("glob.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Names of every shape: ASCII and its last characters, digits, GLOB's own characters, mixed case,
/// accents and other scripts, the empty string; and in each table some NULLs and BLOBs.
fn names() -> Vec<String> {
    let mut v = Vec::new();
    for word in [
        "apple",
        "apply",
        "app",
        "ap",
        "a",
        "b",
        "abc",
        "abd",
        "abc~",
        "abc\u{7f}",
        "Apple",
        "APPLE",
    ] {
        v.push(word.to_owned());
        v.push(format!("{word}-1"));
        v.push(format!("{word}*x"));
    }
    for odd in [
        "",
        "~",
        "~~",
        "\u{7f}",
        "\u{7f}a",
        "a*b",
        "a?b",
        "a[b",
        "[x]",
        "é",
        "éa",
        "e\u{301}",
        "日本",
        "日本語",
        "a日",
        "zzz",
        "0",
        "01",
        "-1",
        "  lead",
    ] {
        v.push(odd.to_owned());
    }
    v
}

fn tables(conn: &Arc<Connection>) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("CREATE INDEX t_name ON t (name)").unwrap();
    conn.execute("CREATE TABLE plain (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    for (id, name) in names().iter().enumerate() {
        for table in ["t", "plain"] {
            let mut stmt = conn
                .prepare(format!("INSERT INTO {table} VALUES (?, ?)"))
                .unwrap();
            stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(id as i64))
                .unwrap();
            stmt.bind_at(NonZero::new(2).unwrap(), Value::build_text(name.clone()))
                .unwrap();
            stmt.run_collect_rows().unwrap();
        }
    }
    for table in ["t", "plain"] {
        conn.execute(format!("INSERT INTO {table} VALUES (1000, NULL), (1001, x'6170706c65'), (1002, x'616263'), (1003, x'')")).unwrap();
    }
}

fn ids(conn: &Arc<Connection>, sql: &str, pattern: &str) -> Vec<i64> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.bind_at(
        NonZero::new(1).unwrap(),
        Value::build_text(pattern.to_owned()),
    )
    .unwrap();
    let mut out: Vec<i64> = stmt
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| match &row[0] {
            Value::Numeric(Numeric::Integer(i)) => *i,
            other => panic!("{other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn plan(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[3].to_string())
        .collect()
}

/// Do: a GLOB with a bound pattern on the indexed text column. Expect: a seek of the index's range,
/// the GLOB left to filter it.
#[test]
fn a_glob_on_an_indexed_text_column_seeks_its_range() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    tables(&conn);
    let steps = plan(&conn, "SELECT id FROM t WHERE name GLOB ?");
    assert_eq!(
        steps,
        vec!["SEARCH t USING COVERING INDEX t_name (name>=? AND name<?)".to_owned()]
    );
    let literal = plan(&conn, "SELECT id FROM t WHERE name GLOB 'app*'");
    assert_eq!(literal, steps, "a literal pattern too");
    let negated = plan(&conn, "SELECT id FROM t WHERE name NOT GLOB ?");
    assert!(
        !negated[0].contains("name>="),
        "NOT GLOB has no range: {negated:?}"
    );
}

/// Do: for every pattern shape, read the rows a GLOB matches from the indexed table (a seek) and
/// from a copy with no index (a scan). Expect: the same rows, and never a NULL or a BLOB.
#[test]
fn a_glob_seek_reads_what_a_scan_reads() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    tables(&conn);
    let patterns = [
        "app*",
        "appl?",
        "apple",
        "ap*1",
        "a*",
        "A*",
        "abc*",
        "abc~*",
        "abc\u{7f}*",
        "~*",
        "\u{7f}*",
        "*",
        "?",
        "",
        "[a-c]*",
        "a[*]b",
        "a[?]b",
        "a[[]b",
        "[[]x]",
        "é*",
        "e*",
        "日*",
        "日本?",
        "a日*",
        "0*",
        "-*",
        "  *",
        "zzz",
        "zz",
        "app[l]e",
        "app[^l]*",
        "*e",
        "a*b*c",
        "apple-1",
        "apple*x",
        "apple[*]x",
    ];
    for pattern in patterns {
        let seek = ids(&conn, "SELECT id FROM t WHERE name GLOB ?", pattern);
        let scan = ids(&conn, "SELECT id FROM plain WHERE name GLOB ?", pattern);
        assert_eq!(seek, scan, "pattern {pattern:?}");
        assert!(
            seek.iter().all(|id| *id < 1000),
            "pattern {pattern:?} matched a NULL or a BLOB"
        );
    }
}

/// Do: GLOB and LIKE a BLOB whose bytes spell the pattern, and the same as text. Expect: the BLOB
/// matches neither, the text both.
#[test]
fn a_blob_matches_neither_glob_nor_like() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let one = |sql: &str| conn.prepare(sql).unwrap().run_collect_rows().unwrap()[0][0].to_string();
    assert_eq!(one("SELECT x'616263' GLOB 'abc'"), "0");
    assert_eq!(one("SELECT x'616263' LIKE 'abc'"), "0");
    assert_eq!(one("SELECT 'abc' GLOB 'abc'"), "1");
    assert_eq!(one("SELECT 'abc' LIKE 'ABC'"), "1");
    assert_eq!(one("SELECT NULL GLOB 'abc' IS NULL"), "1");
}

/// Do: the bounds of patterns of every shape. Expect: the literal prefix below; above, the prefix
/// with its last ASCII character advanced and what follows it dropped, or an empty BLOB.
#[test]
fn a_glob_patterns_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let one = |sql: &str| conn.prepare(sql).unwrap().run_collect_rows().unwrap()[0][0].clone();
    let text = |v: Value| v.to_string();
    assert_eq!(text(one("SELECT glob_lower_bound('app*le')")), "app");
    assert_eq!(text(one("SELECT glob_upper_bound('app*le')")), "apq");
    assert_eq!(text(one("SELECT glob_lower_bound('a?c')")), "a");
    assert_eq!(text(one("SELECT glob_lower_bound('ab[c]')")), "ab");
    assert_eq!(text(one("SELECT glob_upper_bound('a~*')")), "a\u{7f}");
    assert_eq!(
        text(one("SELECT glob_upper_bound('a' || char(127) || '*')")),
        "b"
    );
    assert_eq!(text(one("SELECT glob_upper_bound('a日本*')")), "b");
    assert_eq!(text(one("SELECT glob_upper_bound(123)")), "124");
    assert!(matches!(one("SELECT glob_upper_bound('日*')"), Value::Blob(b) if b.is_empty()));
    assert!(matches!(one("SELECT glob_upper_bound('*')"), Value::Blob(b) if b.is_empty()));
    assert!(matches!(one("SELECT glob_lower_bound(NULL)"), Value::Null));
    assert!(matches!(one("SELECT glob_upper_bound(NULL)"), Value::Null));
}
