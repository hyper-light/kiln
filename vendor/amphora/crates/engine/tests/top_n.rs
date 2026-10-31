//! An ORDER BY whose reader takes only some rows (a LIMIT, with any OFFSET) keeps only those rows in
//! its sorter, every other row dropped as it arrives, where every row was sorted
//! (`docs/design/executor.md`, decision 5). Every answer is the first rows of the whole sort, ties
//! in the order the whole sort gives them (AC-T1.1.1: reads as committed; AC-T1.13.4: reads at their
//! latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("t.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows().unwrap()
}

/// 3,000 rows: g in 0..40, v in 0..60 or NULL one row in 15, h a word in either case, w a real.
fn table(conn: &Arc<Connection>) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, v INTEGER, h TEXT, w REAL)")
        .unwrap();
    let words = ["ant", "Ant", "bee", "BEE", "cat", "dog", "Dog", "eel"];
    let mut x = 11u64;
    let mut next = || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 33
    };
    conn.execute("BEGIN").unwrap();
    for id in 1..=3_000 {
        let g = next() % 40;
        let v = if next() % 15 == 0 {
            "NULL".to_string()
        } else {
            (next() % 60).to_string()
        };
        let h = words[(next() % words.len() as u64) as usize];
        let w = (next() % 1000) as f64 / 4.0;
        conn.execute(format!("INSERT INTO t VALUES ({id}, {g}, {v}, '{h}', {w})"))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// Do: for ORDER BYs ascending and descending, NULLs first and last, under NOCASE, on two keys and
/// on an expression, with DISTINCT and over a GROUP BY, each with LIMITs and OFFSETs from 1 row to
/// all of them (and past the most a sorter keeps): ask for the rows with the LIMIT, and for the
/// whole ordering. Expect: the limited answer is the whole ordering's rows from the OFFSET on, as
/// many as the LIMIT, ties in the same order.
#[test]
fn rows_ordered_with_a_limit_are_the_first_rows_of_the_whole_ordering() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    table(&conn);
    let orderings = [
        "SELECT id, v FROM t ORDER BY v",
        "SELECT id, v FROM t ORDER BY v DESC",
        "SELECT id, v FROM t ORDER BY v NULLS LAST",
        "SELECT id, v FROM t ORDER BY v DESC NULLS FIRST",
        "SELECT id, h FROM t ORDER BY h COLLATE NOCASE",
        "SELECT id, g, v FROM t ORDER BY g, v DESC",
        "SELECT id FROM t ORDER BY (v * 7) % 13, w DESC",
        "SELECT DISTINCT g FROM t ORDER BY g DESC",
        "SELECT g, COUNT(*) c FROM t GROUP BY g ORDER BY c DESC",
        "SELECT g, SUM(w) s FROM t GROUP BY g ORDER BY s",
    ];
    for whole_sql in orderings {
        let whole = rows(&conn, whole_sql);
        for (limit, offset) in [
            (1, 0),
            (5, 0),
            (7, 3),
            (20, 15),
            (100, 0),
            (1_000, 2_500),
            (70_000, 0),
            (-1, 10),
        ] {
            let got = rows(&conn, &format!("{whole_sql} LIMIT {limit} OFFSET {offset}"));
            let from = offset.min(whole.len());
            let to = if limit < 0 {
                whole.len()
            } else {
                (from + limit as usize).min(whole.len())
            };
            assert_eq!(
                got,
                whole[from..to],
                "{whole_sql} LIMIT {limit} OFFSET {offset}"
            );
        }
        assert!(
            rows(&conn, &format!("{whole_sql} LIMIT 0")).is_empty(),
            "{whole_sql} LIMIT 0"
        );
    }
}

/// Do: a LIMIT and an OFFSET bound as parameters. Expect: the whole ordering's rows they name.
#[test]
fn a_limit_bound_as_a_parameter_keeps_the_rows_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    table(&conn);
    let whole = rows(&conn, "SELECT id, w FROM t ORDER BY w DESC, id");
    let mut stmt = conn
        .prepare("SELECT id, w FROM t ORDER BY w DESC, id LIMIT ? OFFSET ?")
        .unwrap();
    stmt.bind_at(NonZero::new(1).unwrap(), Value::from_i64(9))
        .unwrap();
    stmt.bind_at(NonZero::new(2).unwrap(), Value::from_i64(4))
        .unwrap();
    let got = stmt.run_collect_rows().unwrap();
    assert_eq!(got, whole[4..13]);
}

/// Do: explain an ORDER BY with a LIMIT and one without. Expect: the limited one's sorter is told
/// the register the LIMIT (and OFFSET) is in, the other's isn't.
#[test]
fn only_an_ordering_with_a_limit_tells_its_sorter_how_many_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    table(&conn);
    let told = |sql: &str| {
        rows(&conn, &format!("EXPLAIN {sql}")).iter().any(|r| {
            r.iter()
                .any(|v| matches!(v, Value::Text(t) if t.as_str().contains("keep=r[")))
        })
    };
    assert!(told("SELECT id FROM t ORDER BY w LIMIT 5"));
    assert!(told(
        "SELECT g, COUNT(*) c FROM t GROUP BY g ORDER BY c DESC LIMIT 3 OFFSET 2"
    ));
    assert!(!told("SELECT id FROM t ORDER BY w"));
}
