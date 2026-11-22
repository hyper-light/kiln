//! An upsert under a statement's OR clause (the unify audit's E4, open at e2e67247): the upsert's
//! DO UPDATE or DO NOTHING settles a conflict on the constraint it names whatever the statement's
//! OR clause says, and the OR clause settles conflicts on the constraints no upsert names, as
//! SQLite 3.54.0 does (sqlite.org/lang_upsert.html; T-T1.7.1: every record read back as written).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let name = format!("upsert_or_clause_{}.db", N.fetch_add(1, Ordering::Relaxed));
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

/// Do: run sqlite3 3.54.0's script of upserts under OR IGNORE, OR REPLACE and OR ABORT, on a table
/// with one UNIQUE column and on one with two beside its rowid. Expect: each statement's rows as
/// sqlite3 3.54.0 gave them (its output beside each): the upsert's DO clause for the constraint it
/// names, the OR clause for the others (E4; T-T1.7.1).
#[test]
fn an_upsert_settles_its_constraint_whatever_the_statements_or_clause() {
    let conn = open();
    conn.execute("CREATE TABLE t(u UNIQUE, v)").unwrap();
    conn.execute("INSERT INTO t VALUES(1,'a')").unwrap();
    // C1|1|c
    conn.execute("INSERT OR IGNORE INTO t VALUES(1,'b') ON CONFLICT(u) DO UPDATE SET v='c'")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM t"), ["1|c"]);
    // C2|1|e
    conn.execute("INSERT OR REPLACE INTO t VALUES(1,'d') ON CONFLICT(u) DO UPDATE SET v='e'")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM t"), ["1|e"]);
    // C3|1|e
    conn.execute("INSERT OR IGNORE INTO t VALUES(1,'f') ON CONFLICT(u) DO NOTHING")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM t"), ["1|e"]);

    conn.execute("CREATE TABLE r(id INTEGER PRIMARY KEY, u UNIQUE, w UNIQUE)")
        .unwrap();
    conn.execute("INSERT INTO r VALUES(1,1,1)").unwrap();
    // C4|1|1|9: the conflict on u goes to the DO UPDATE.
    conn.execute("INSERT OR IGNORE INTO r VALUES(2,1,1) ON CONFLICT(u) DO UPDATE SET w=9")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM r"), ["1|1|9"]);
    conn.execute("INSERT INTO r VALUES(3,3,3)").unwrap();
    // C5|1|1|9, C5|3|3|3: the conflict on w, which no upsert names, goes to OR IGNORE.
    conn.execute("INSERT OR IGNORE INTO r VALUES(4,4,3) ON CONFLICT(u) DO UPDATE SET w=9")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM r ORDER BY id"),
        ["1|1|9", "3|3|3"]
    );
    // C6|1|1|19, C6|3|3|3
    conn.execute("INSERT OR ABORT INTO r VALUES(5,1,5) ON CONFLICT(u) DO UPDATE SET w=w+10")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM r ORDER BY id"),
        ["1|1|19", "3|3|3"]
    );
}

/// Do: the rest of the matrix, each case on rows set afresh: a named conflict's DO NOTHING under
/// OR REPLACE and OR ABORT, and a conflict no upsert names under OR REPLACE, OR ABORT and OR
/// IGNORE, and a conflict on both. Expect: sqlite3 3.54.0's rows or error (its output beside each):
/// the DO NOTHING for the constraint it names; for the others, OR REPLACE replaces the conflicting
/// row, OR ABORT fails the statement and keeps the rows, and OR IGNORE skips the row; on both, the
/// named one's upsert, in either declaration order (E4; T-T1.7.1).
#[test]
fn the_or_clause_settles_the_conflicts_no_upsert_names() {
    let conn = open();
    conn.execute("CREATE TABLE t(u UNIQUE, v)").unwrap();
    conn.execute("INSERT INTO t VALUES(1,'a')").unwrap();
    // G1|1|a
    conn.execute("INSERT OR REPLACE INTO t VALUES(1,'x') ON CONFLICT(u) DO NOTHING")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM t"), ["1|a"]);
    // G2|1|a
    conn.execute("INSERT OR ABORT INTO t VALUES(1,'y') ON CONFLICT(u) DO NOTHING")
        .unwrap();
    assert_eq!(rows(&conn, "SELECT * FROM t"), ["1|a"]);

    conn.execute("CREATE TABLE r(id INTEGER PRIMARY KEY, u UNIQUE, w UNIQUE)")
        .unwrap();
    let reset = |conn: &Arc<Connection>| {
        conn.execute("DELETE FROM r").unwrap();
        conn.execute("INSERT INTO r VALUES(1,1,1),(3,3,3)").unwrap();
    };
    // G3|1|1|1, G3|4|4|3 and G4, the same: the conflict on w goes to OR REPLACE.
    for upsert in ["DO UPDATE SET w=9", "DO NOTHING"] {
        reset(&conn);
        conn.execute(format!(
            "INSERT OR REPLACE INTO r VALUES(4,4,3) ON CONFLICT(u) {upsert}"
        ))
        .unwrap();
        assert_eq!(
            rows(&conn, "SELECT * FROM r ORDER BY id"),
            ["1|1|1", "4|4|3"],
            "{upsert}"
        );
    }
    // G5 and G6: "UNIQUE constraint failed: r.w", and the rows stay (1|1|1, 3|3|3).
    for upsert in ["DO UPDATE SET w=9", "DO NOTHING"] {
        reset(&conn);
        let error = conn
            .execute(format!(
                "INSERT OR ABORT INTO r VALUES(4,4,3) ON CONFLICT(u) {upsert}"
            ))
            .unwrap_err();
        assert!(
            error.to_string().contains("UNIQUE constraint failed: r.w"),
            "{upsert}: {error}"
        );
        assert_eq!(
            rows(&conn, "SELECT * FROM r ORDER BY id"),
            ["1|1|1", "3|3|3"],
            "{upsert}"
        );
    }
    // G7|1|1|1, G7|3|3|3
    reset(&conn);
    conn.execute("INSERT OR IGNORE INTO r VALUES(4,4,3) ON CONFLICT(u) DO NOTHING")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM r ORDER BY id"),
        ["1|1|1", "3|3|3"]
    );
    // G8|1|1|9, G8|3|3|3: a conflict on both: the named one's DO UPDATE runs and the insert, with
    // its REPLACE of row 3, never happens.
    reset(&conn);
    conn.execute("INSERT OR REPLACE INTO r VALUES(4,1,3) ON CONFLICT(u) DO UPDATE SET w=9")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM r ORDER BY id"),
        ["1|1|9", "3|3|3"]
    );
    // G9|1|9|1, G9|3|3|3: the same with the unnamed column declared first.
    conn.execute("CREATE TABLE s(id INTEGER PRIMARY KEY, w UNIQUE, u UNIQUE)")
        .unwrap();
    conn.execute("INSERT INTO s VALUES(1,1,1),(3,3,3)").unwrap();
    conn.execute("INSERT OR REPLACE INTO s VALUES(4,3,1) ON CONFLICT(u) DO UPDATE SET w=9")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT * FROM s ORDER BY id"),
        ["1|9|1", "3|3|3"]
    );
}
