//! G6 step 5's planned test (plans that read less: bitmap-style sorted row fetches, a prototype
//! behind `Connection::set_sorted_fetches`): a differential of the changed plans. Range, equality
//! and IN reads over a non-covering index, alone, ordered, limited and joined, run with sorted
//! fetches off and on; the answers must be identical, and with them on the planner must pick a
//! sorted fetch (a one-branch multi-index read) for a wide range.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::sync::Arc;

const ROWS: i64 = 4_000;

fn rows(conn: &Arc<Connection>, sql: &str, sorted: bool) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out: Vec<String> = stmt
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect();
    if !sorted {
        out.sort();
    }
    out
}

fn plan(conn: &Arc<Connection>, sql: &str) -> String {
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[3].to_string())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Do: run each read with sorted fetches off and on. Expect: one answer (in order where the query
/// orders it); with them on, a wide range is read as a sorted fetch, and with them off it is not.
#[test]
fn g6_s5_sorted_fetch_gives_the_plain_reads_answers() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("sorted.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a, b, pad)")
        .unwrap();
    conn.execute("CREATE INDEX t_a ON t (a)").unwrap();
    conn.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, a, c)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    // `a` is scattered over the rowids, so an index read visits table pages out of order; some
    // `a` are NULL and every value repeats.
    for id in 1..=ROWS {
        let a = if id % 17 == 0 {
            "NULL".to_string()
        } else {
            ((id * 7_919) % 1_000).to_string()
        };
        conn.execute(format!(
            "INSERT INTO t VALUES ({id}, {a}, {}, '{}')",
            id % 13,
            "x".repeat(60)
        ))
        .unwrap();
        if id % 4 == 0 {
            conn.execute(format!(
                "INSERT INTO u VALUES ({id}, {}, {})",
                id % 1_000,
                id % 5
            ))
            .unwrap();
        }
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();

    let wide = "SELECT id, b FROM t WHERE a BETWEEN 100 AND 700";
    let queries = [
        (wide, false),
        ("SELECT id, b, pad FROM t WHERE a > 900", false),
        ("SELECT id FROM t WHERE a = 42", false),
        ("SELECT id, b FROM t WHERE a IN (1, 2, 3, 500, 999)", false),
        (
            "SELECT id, a FROM t WHERE a BETWEEN 100 AND 700 ORDER BY a, id",
            true,
        ),
        (
            "SELECT id FROM t WHERE a BETWEEN 100 AND 700 ORDER BY id DESC LIMIT 30",
            true,
        ),
        (
            "SELECT count(*), sum(b) FROM t WHERE a < 300 AND b > 4",
            false,
        ),
        (
            "SELECT t.id, u.c FROM u JOIN t ON t.id = u.id WHERE t.a BETWEEN 200 AND 400",
            false,
        ),
        (
            "SELECT t.id, u.id FROM t JOIN u ON u.a = t.b WHERE t.a > 800",
            false,
        ),
        (
            "SELECT b, count(*) FROM t WHERE a BETWEEN 100 AND 700 GROUP BY b",
            false,
        ),
    ];

    let mut wrong = Vec::new();
    for (sql, ordered) in queries {
        conn.set_sorted_fetches(false);
        let off_plan = plan(&conn, sql);
        let off = rows(&conn, sql, ordered);
        conn.set_sorted_fetches(true);
        let on_plan = plan(&conn, sql);
        let on = rows(&conn, sql, ordered);
        if on != off {
            wrong.push(format!(
                "{sql}\n  on ({on_plan}) gives {} rows, off ({off_plan}) {}",
                on.len(),
                off.len()
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    // Non-vacuity: the switch does choose the sorted fetch for the wide range.
    conn.set_sorted_fetches(true);
    assert!(
        plan(&conn, wide).contains("MULTI-INDEX"),
        "on: {}",
        plan(&conn, wide)
    );
    conn.set_sorted_fetches(false);
    assert!(
        !plan(&conn, wide).contains("MULTI-INDEX"),
        "off: {}",
        plan(&conn, wide)
    );
    assert!(
        !rows(&conn, wide, false).is_empty(),
        "the wide range reads some rows"
    );
}
