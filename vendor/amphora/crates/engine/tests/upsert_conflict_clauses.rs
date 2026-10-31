//! An upsert beside constraints that declare ON CONFLICT REPLACE (the unify audit's E3, a panic at
//! e2e67247): a conflict on the constraint an upsert names goes to that upsert, never to the
//! constraint's own clause, and a constraint no upsert names keeps its clause, as SQLite 3.54.0 does
//! (sqlite.org/lang_upsert.html; T-T1.7.1: every record read back as written).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let name = format!(
        "upsert_conflict_clauses_{}.db",
        N.fetch_add(1, Ordering::Relaxed)
    );
    let db = Database::open(io, &name, options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Each row of `sql`, its values joined by `|`.
fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| {
            row.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Do: run sqlite3 3.54.0's script of upserts on a table whose INTEGER PRIMARY KEY declares ON
/// CONFLICT REPLACE beside a plain UNIQUE column, and on one whose UNIQUE column declares it.
/// Expect: each statement's rows and refusals as sqlite3 3.54.0 gave them (its output is the
/// comment beside each): no panic (E3; T-T1.7.1).
#[test]
fn an_upsert_beside_replace_constraints_answers_as_sqlite_does() {
    let conn = open();
    conn.execute("CREATE TABLE w(a INTEGER PRIMARY KEY ON CONFLICT REPLACE, b UNIQUE)")
        .unwrap();
    // A1|1|2: no conflict, the row goes in.
    conn.execute("INSERT INTO w VALUES(1,2) ON CONFLICT(a) DO UPDATE SET b=3")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM w"), ["1|2"]);
    // A2|1|3: the key the upsert names conflicts, so its DO UPDATE runs, not the key's REPLACE.
    conn.execute("INSERT INTO w VALUES(1,9) ON CONFLICT(a) DO UPDATE SET b=3")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM w"), ["1|3"]);
    // UNIQUE constraint failed: w.b, A3|1|3: b's own ABORT, which no upsert names.
    let refused = conn
        .execute("INSERT INTO w VALUES(5,3) ON CONFLICT(a) DO UPDATE SET b=4")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("UNIQUE constraint failed: w.b"),
        "{refused}"
    );
    assert_eq!(rows(&conn, "SELECT * FROM w ORDER BY a"), ["1|3"]);
    // A4|1|3: an upsert without a target covers every constraint.
    conn.execute("INSERT INTO w VALUES(1,7) ON CONFLICT DO NOTHING")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM w ORDER BY a"), ["1|3"]);
    // A5|1|7: without an upsert, the key's REPLACE replaces the row.
    conn.execute("INSERT INTO w VALUES(1,7)").unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM w ORDER BY a"), ["1|7"]);

    conn.execute("CREATE TABLE v(a INTEGER PRIMARY KEY, b UNIQUE ON CONFLICT REPLACE)")
        .unwrap();
    conn.execute("INSERT INTO v VALUES(1,2)").unwrap();
    // B1|5|2: the upsert names b, whose conflict its DO UPDATE settles.
    conn.execute("INSERT INTO v VALUES(5,2) ON CONFLICT(b) DO UPDATE SET a=excluded.a")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM v ORDER BY a"), ["5|2"]);
    // UNIQUE constraint failed: v.a, B2|5|2: the key's own ABORT, which no upsert names.
    let refused = conn
        .execute("INSERT INTO v VALUES(5,8) ON CONFLICT(b) DO UPDATE SET a=99")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("UNIQUE constraint failed: v.a"),
        "{refused}"
    );
    assert_eq!(rows(&conn, "SELECT * FROM v ORDER BY a"), ["5|2"]);
}
