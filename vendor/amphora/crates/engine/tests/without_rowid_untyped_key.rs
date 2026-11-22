//! An untyped column in a WITHOUT ROWID table's primary key (the unify audit's E2, open at
//! e2e67247): it has no affinity, so it stores each value as given, as SQLite 3.54.0 does. 5 stays
//! an integer that `b = 5` finds and `b = '5'` doesn't, and 5 and '5' are distinct keys
//! (sqlite.org/datatype3.html, "Determination Of Column Affinity"; T-T1.7.1: every record read
//! back as written).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, MemoryIO, OpenOptions, SqliteDialect,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn open() -> Arc<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let io = Arc::new(MemoryIO::new());
    let options = OpenOptions::new(Arc::new(SqliteDialect))
        .db_opts(DatabaseOpts::new().with_without_rowid(true));
    let name = format!(
        "without_rowid_untyped_key_{}.db",
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

/// Do: the audit's script and sqlite3 3.54.0's own cases beside it, on an untyped key column after
/// a TEXT column, in a composite key, and with the key's columns reordered. Expect: each answer as
/// sqlite3 3.54.0 gave it (its output beside each): values stored as given, lookups by 5 and by
/// '5' told apart, 5 and '5' distinct under the key, 5.0 equal to 5 (E2; T-T1.7.1).
#[test]
fn an_untyped_key_column_stores_values_as_given() {
    let conn = open();
    conn.execute("CREATE TABLE w(a TEXT, b, PRIMARY KEY(b)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO w VALUES('x',5)").unwrap();
    // E1|integer|5
    assert_eq!(rows(&conn, "SELECT typeof(b), b FROM w"), ["integer|5"]);
    // E2|x
    assert_eq!(rows(&conn, "SELECT a FROM w WHERE b = 5"), ["x"]);
    // E3|0
    assert_eq!(rows(&conn, "SELECT count(*) FROM w WHERE b = '5'"), ["0"]);
    // E4|x|integer|5, E4|y|text|5
    conn.execute("INSERT INTO w VALUES('y','5')").unwrap();
    assert_eq!(
        rows(&conn, "SELECT a, typeof(b), b FROM w ORDER BY a"),
        ["x|integer|5", "y|text|5"]
    );
    // "UNIQUE constraint failed: w.b": 5.0 equals 5.
    let error = conn.execute("INSERT INTO w VALUES('z',5.0)").unwrap_err();
    assert!(
        error.to_string().contains("UNIQUE constraint failed: w.b"),
        "{error}"
    );
    // E5|x|integer, E5|y|text
    assert_eq!(
        rows(&conn, "SELECT a, typeof(b) FROM w ORDER BY a"),
        ["x|integer", "y|text"]
    );

    // E6|integer|7, E6|text|'7', E6|blob|X'07'
    conn.execute("CREATE TABLE u(b, c TEXT, PRIMARY KEY(b, c)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO u VALUES(7,'k'),('7','k'),(x'07','k')")
        .unwrap();
    assert_eq!(
        rows(&conn, "SELECT typeof(b), quote(b) FROM u ORDER BY b"),
        ["integer|7", "text|'7'", "blob|X'07'"]
    );

    // E7|integer|text|real
    conn.execute("CREATE TABLE v(a INTEGER, b, c REAL, PRIMARY KEY(c, b)) WITHOUT ROWID")
        .unwrap();
    conn.execute("INSERT INTO v VALUES('3', '3', '3')").unwrap();
    assert_eq!(
        rows(&conn, "SELECT typeof(a), typeof(b), typeof(c) FROM v"),
        ["integer|text|real"]
    );
}
