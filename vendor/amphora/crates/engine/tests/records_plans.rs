//! Kiln's records reads (its versions table, `crates/kiln/src/records`), planned without statistics
//! as SQLite 3.54.0 plans them (reported by Sends T3 for CP-T1.5): a row's newest version, or its
//! version at a revision, sought by its key on the primary key; a scan of keys sought on the primary
//! key from its first key, in its order; a family's versions from a revision walked by revision; none
//! sorted. Without `sqlite_stat1` SQLite estimates by its defaults (`sqlite3DefaultRowEst`), so the
//! index that matches more equality columns, and the one that saves the sort, win (AC-T1.5.4: the
//! change feed at its latency; AC-T1.13.4: history reads at theirs).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::sync::Arc;

/// Kiln's versions table and index holding 4,000 versions of one family (kiln's own test's), with
/// no statistics.
fn versions(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("records.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE versions (family TEXT NOT NULL, key BLOB NOT NULL, rev INTEGER NOT NULL, n INTEGER NOT NULL, value BLOB, PRIMARY KEY (family, key, rev))").unwrap();
    conn.execute("CREATE INDEX versions_by_rev ON versions (family, rev, n)")
        .unwrap();
    conn.execute("WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < 3999) INSERT INTO versions SELECT 'inbox', CAST('s' || (x / 500) || '/m' || x AS BLOB), x + 1, 0, zeroblob(60) FROM s").unwrap();
    conn
}

/// The plan's steps, as EXPLAIN QUERY PLAN words them.
fn plan(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[3].to_string())
        .collect()
}

/// Do: plan each of kiln's reads without statistics, as kiln words them and in their plain forms
/// (no ORDER BY on the equality columns, no INDEXED BY). Expect: SQLite 3.54.0's plan for each, one
/// search on the index it names, by the leading constraints it names, with no sort.
#[test]
fn kilns_reads_are_planned_without_statistics_as_sqlite_plans_them() {
    let dir = tempfile::tempdir().unwrap();
    let conn = versions(dir.path());
    let by_key = "INDEX sqlite_autoindex_versions_1 (family=? AND key";
    let by_rev = "INDEX versions_by_rev (family=? AND rev>?";
    for (sql, seek) in [
        // LATEST and GET_LATEST, as kiln words them and plainly.
        ("SELECT rev, value IS NULL FROM versions WHERE family = ?1 AND key = ?2 ORDER BY family DESC, key DESC, rev DESC LIMIT 1", format!("{by_key}=?)")),
        ("SELECT rev, value FROM versions WHERE family = ?1 AND key = ?2 ORDER BY rev DESC LIMIT 1", format!("{by_key}=?)")),
        // GET_AT and AT_DELETED.
        ("SELECT rev, value FROM versions WHERE family = ?1 AND key = ?2 AND rev <= ?3 ORDER BY family DESC, key DESC, rev DESC LIMIT 1", format!("{by_key}=? AND rev<")),
        ("SELECT rev, value IS NULL FROM versions WHERE family = ?1 AND key = ?2 AND rev <= ?3 ORDER BY rev DESC LIMIT 1", format!("{by_key}=? AND rev<")),
        // SCAN_TO and SCAN_ON.
        ("SELECT key, rev, value FROM versions WHERE family = ?1 AND key >= ?2 AND key < ?3 ORDER BY key, rev", format!("{by_key}>")),
        ("SELECT key, rev, value FROM versions WHERE family = ?1 AND key >= ?2 ORDER BY key, rev", format!("{by_key}>")),
        // FEED and WINDOW, as kiln words them and plainly.
        ("SELECT rev, key, value FROM versions INDEXED BY versions_by_rev WHERE family = ?1 AND rev > ?2 ORDER BY rev, n LIMIT ?3", format!("{by_rev})")),
        ("SELECT rev, key, value FROM versions WHERE family = ?1 AND rev > ?2 ORDER BY rev, n LIMIT ?3", format!("{by_rev})")),
        ("SELECT key, rev FROM versions INDEXED BY versions_by_rev WHERE family = ?1 AND rev > ?2 AND rev <= ?3 ORDER BY rev, n LIMIT ?4", format!("{by_rev} AND rev<")),
        ("SELECT key, rev FROM versions WHERE family = ?1 AND rev > ?2 AND rev <= ?3 ORDER BY rev, n LIMIT ?4", format!("{by_rev} AND rev<")),
        // PRUNE_BELOW and PRUNE_AT.
        ("DELETE FROM versions WHERE family = ?1 AND key = ?2 AND rev < ?3", format!("{by_key}=? AND rev<")),
        ("DELETE FROM versions WHERE family = ?1 AND key = ?2 AND rev = ?3", format!("{by_key}=? AND rev=?)")),
    ] {
        let steps = plan(&conn, sql);
        assert!(
            steps.len() == 1
                && steps[0].starts_with("SEARCH versions USING ")
                && steps[0].contains(&seek),
            "{sql}: {steps:?}"
        );
    }
}

/// Do: name an index with INDEXED BY where it can seek, where nothing seeks it and where it doesn't
/// exist, and forbid indexes with NOT INDEXED. Expect: SQLite 3.54.0's answer for each: the named
/// index sought or scanned, never ignored for a better one; a missing index refused when the
/// statement is prepared; a table scan under NOT INDEXED, where the primary key could seek.
#[test]
fn indexed_by_uses_the_named_index_or_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let conn = versions(dir.path());
    for (sql, want) in [
        (
            "SELECT rev FROM versions INDEXED BY versions_by_rev WHERE family = ?1 AND key = ?2 ORDER BY rev DESC LIMIT 1",
            "SEARCH versions USING INDEX versions_by_rev (family=?)",
        ),
        (
            "SELECT rev FROM versions INDEXED BY versions_by_rev WHERE key = ?1",
            "SCAN versions USING INDEX versions_by_rev",
        ),
        (
            "SELECT rev FROM versions INDEXED BY versions_by_rev WHERE family = ?1 OR key = ?2",
            "SCAN versions USING INDEX versions_by_rev",
        ),
        (
            "SELECT rev FROM versions NOT INDEXED WHERE family = ?1 AND key = ?2",
            "SCAN versions",
        ),
    ] {
        assert_eq!(plan(&conn, sql), [want], "{sql}");
    }
    let refused = conn
        .prepare("SELECT rev FROM versions INDEXED BY nosuch WHERE family = ?1")
        .err()
        .map(|e| e.to_string());
    assert!(
        refused
            .as_deref()
            .is_some_and(|e| e.contains("no such index: nosuch")),
        "{refused:?}"
    );
}

/// Do: plan, without statistics, a composite primary key's two-column prefix against a secondary
/// index on its first column, three equality columns against two, and an ORDER BY with a LIMIT that
/// an index's order takes against an equality prefix and a sort. Expect: SQLite 3.54.0's plan for
/// each, one search with no sort: the index matching more equality columns wins, and the one giving
/// the order a LIMIT reads in wins over one that would sort.
#[test]
fn more_equality_columns_and_the_order_a_limit_reads_in_win_without_statistics() {
    let dir = tempfile::tempdir().unwrap();
    let conn = versions(dir.path());
    for sql in [
        "CREATE TABLE p (a INT, b INT, c INT, d INT, PRIMARY KEY (a, b, c))",
        "CREATE INDEX p_ad ON p (a, d)",
        "CREATE TABLE q (a INT, b INT, c INT, v TEXT, PRIMARY KEY (a, b))",
        "CREATE INDEX q_ac ON q (a, c)",
        "CREATE TABLE r (a INT, b INT, c INT, d INT)",
        "CREATE INDEX r2 ON r (a, b)",
        "CREATE INDEX r3 ON r (a, b, c)",
    ] {
        conn.execute(sql).unwrap();
    }
    for (sql, want) in [
        (
            "SELECT d FROM p WHERE a = ?1 AND b = ?2",
            "SEARCH p USING INDEX sqlite_autoindex_p_1 (a=? AND b=?)",
        ),
        (
            "SELECT v FROM q WHERE a = ?1 ORDER BY c LIMIT 10",
            "SEARCH q USING INDEX q_ac (a=?)",
        ),
        (
            "SELECT v FROM q WHERE a = ?1 AND c > ?2 ORDER BY c LIMIT ?3",
            "SEARCH q USING INDEX q_ac (a=? AND c>?)",
        ),
        (
            "SELECT d FROM r WHERE a = ?1 AND b = ?2 AND c = ?3",
            "SEARCH r USING INDEX r3 (a=? AND b=? AND c=?)",
        ),
    ] {
        assert_eq!(plan(&conn, sql), [want], "{sql}");
    }
}

/// Do: order by columns of a unique key's lookup: a table's whole primary key matched by `=` (and
/// by `IS`, which can match many NULL keys), a rowid matched by `=`, and a join of that lookup
/// after its outer table's rowid lookup, without statistics; then, the tables filled and analyzed
/// (so the join seeks the key as SQLite's does), after its outer table read in rowid order and
/// ordered by that rowid, or by the lookup's column alone; and the lookup first, before a scan in
/// rowid order, with its column named before or after the rowid. Expect: SQLite 3.54.0's plans: no
/// sort where a loop that gives at most one row for each row before it orders its table's columns
/// and keeps the order of the loops before it, as `wherePathSatisfiesOrderBy` marks off the ORDER
/// BY terms of a one-row or order-distinct loop; a sort for `IS`, and for the lookup's column after
/// a scan (AC-T1.13.4: reads at their latency).
#[test]
fn a_one_row_lookup_needs_no_sort_for_its_columns() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("onerow.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE q (a, b, c, v, PRIMARY KEY (a, b))")
        .unwrap();
    conn.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, x, y, z)")
        .unwrap();
    let search_q = "SEARCH q USING INDEX sqlite_autoindex_q_1 (a=? AND b=?)";
    let search_p = "SEARCH p USING INTEGER PRIMARY KEY (rowid=?)";
    let sort = "USE SORTER FOR ORDER BY";
    let join = "SELECT p.id, q.v FROM p JOIN q ON q.a = p.x AND q.b = p.y";
    let mut wrong = Vec::new();
    let mut check = |conn: &Arc<Connection>, sql: &str, want: &[&str]| {
        let got = plan(conn, sql);
        if got != want {
            wrong.push(format!("{sql}\n  got  {got:?}\n  want {want:?}"));
        }
    };
    check(
        &conn,
        "SELECT v FROM q WHERE a = ? AND b = ? ORDER BY c LIMIT 10",
        &[search_q],
    );
    check(
        &conn,
        "SELECT v FROM q WHERE a IS ? AND b IS ? ORDER BY c LIMIT 10",
        &[search_q, sort],
    );
    check(
        &conn,
        "SELECT x FROM p WHERE id = ? ORDER BY z",
        &[search_p],
    );
    check(
        &conn,
        &format!("{join} WHERE p.id = ? ORDER BY q.c"),
        &[search_p, search_q],
    );
    for sql in [
        "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 2000) INSERT INTO q SELECT n % 50, n / 50, n, n FROM s",
        "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 2000) INSERT INTO p SELECT n, n % 50, n / 50, n FROM s",
        "ANALYZE",
    ] {
        conn.execute(sql).unwrap();
    }
    check(
        &conn,
        &format!("{join} ORDER BY p.id, q.c"),
        &["SCAN p", search_q],
    );
    check(
        &conn,
        &format!("{join} ORDER BY q.c"),
        &["SCAN p", search_q, sort],
    );
    // The lookup first: its columns hold for the whole result, wherever the ORDER BY names them.
    let after_lookup = "SELECT q.v, p.id FROM q, p WHERE q.a = ? AND q.b = ?";
    check(
        &conn,
        &format!("{after_lookup} ORDER BY p.id, q.v"),
        &[search_q, "SCAN p"],
    );
    check(
        &conn,
        &format!("{after_lookup} ORDER BY q.v, p.id"),
        &[search_q, "SCAN p"],
    );
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Do: join a table to another's whole primary key without statistics (no ANALYZE), with the tables
/// empty and with 2,000 rows each; and, as a control, join on columns no index covers. Expect:
/// SQLite 3.54.0's plan for the key join, the outer table scanned and the key searched once an
/// outer row, not a hash join whose cost rests on the fallback's million rows; and the control
/// still a hash join, as no unique key could be searched (AC-T1.5.4, AC-T1.13.4).
#[test]
fn a_join_on_a_whole_key_searches_it_without_statistics() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("keyjoin.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE q (a, b, c, v, PRIMARY KEY (a, b))")
        .unwrap();
    conn.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, x, y, z)")
        .unwrap();
    conn.execute("CREATE TABLE r (k, w)").unwrap();
    let join = "SELECT p.id, q.v FROM p JOIN q ON q.a = p.x AND q.b = p.y ORDER BY p.id, q.c";
    let control = "SELECT p.id, r.w FROM p JOIN r ON r.k = p.z";
    let want = [
        "SCAN p",
        "SEARCH q USING INDEX sqlite_autoindex_q_1 (a=? AND b=?)",
    ];
    let mut wrong = Vec::new();
    for filled in [false, true] {
        if filled {
            for sql in [
                "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 2000) INSERT INTO q SELECT n % 50, n / 50, n, n FROM s",
                "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 2000) INSERT INTO p SELECT n, n % 50, n / 50, n FROM s",
                "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 2000) INSERT INTO r SELECT n, n FROM s",
            ] {
                conn.execute(sql).unwrap();
            }
        }
        let got = plan(&conn, join);
        if got != want {
            wrong.push(format!(
                "filled {filled}: {join}\n  got  {got:?}\n  want {want:?}"
            ));
        }
        let got = plan(&conn, control);
        if !got.iter().any(|step| step.starts_with("HASH JOIN")) {
            wrong.push(format!(
                "filled {filled}: {control}\n  got  {got:?}\n  want a hash join"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
