//! The rows each table access and join of an executed statement gave, read back beside the
//! planner's estimates (docs/design/planner.md, decision 12): on tables whose join sizes are
//! known, each counter holds the true count, and where the estimate is exact its q-error is 1
//! (AC-T1.1.2: reads at their latency rest on estimates measured against what ran).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, RowCount,
    RowCountKind, SqliteDialect,
};
use std::sync::Arc;

const A: i64 = 100;
/// Rows of `b` for each row of `a`, and of `c` for each row of `b`.
const B_PER_A: i64 = 3;
const C_PER_B: i64 = 2;

/// a(100 rows), b(3 a row of a), c(2 a row of b), each joined by an indexed key, analyzed.
fn tables(dir: &std::path::Path, counted: bool) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("r.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, v INTEGER NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, a INTEGER NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, b INTEGER NOT NULL)")
        .unwrap();
    conn.execute("CREATE INDEX b_a ON b(a)").unwrap();
    conn.execute("CREATE INDEX c_b ON c(b)").unwrap();
    conn.execute("BEGIN").unwrap();
    for id in 1..=A {
        conn.execute(format!("INSERT INTO a VALUES ({id}, {})", id % 10))
            .unwrap();
    }
    for id in 1..=A * B_PER_A {
        conn.execute(format!(
            "INSERT INTO b VALUES ({id}, {})",
            (id - 1) / B_PER_A + 1
        ))
        .unwrap();
    }
    for id in 1..=A * B_PER_A * C_PER_B {
        conn.execute(format!(
            "INSERT INTO c VALUES ({id}, {})",
            (id - 1) / C_PER_B + 1
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();
    conn.set_row_counters(counted);
    conn
}

fn counts(conn: &Arc<Connection>, sql: &str) -> Vec<RowCount> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows().unwrap();
    stmt.row_counts()
}

fn prefixes(counts: &[RowCount]) -> Vec<(String, f64, u64)> {
    let mut prefixes: Vec<_> = counts
        .iter()
        .filter(|count| count.site.kind == RowCountKind::Prefix)
        .map(|count| {
            let estimate = count.site.estimate.unwrap();
            (
                estimate.position,
                count.site.table.clone().unwrap(),
                estimate.output_rows,
                count.rows,
            )
        })
        .collect();
    prefixes.sort_by_key(|prefix| prefix.0);
    prefixes
        .into_iter()
        .map(|(_, table, estimate, rows)| (table, estimate, rows))
        .collect()
}

fn q_error(estimate: f64, actual: u64) -> f64 {
    let estimate = estimate.max(1.0);
    let actual = (actual as f64).max(1.0);
    (estimate / actual).max(actual / estimate)
}

/// Do: join all three tables by their keys and count each node. Expect: one entry into the
/// loops, each prefix's actual rows the true join size (100, 300, 600 in the plan's order of
/// tables) and, the statistics giving the sizes exactly, a q-error of 1 at every prefix.
#[test]
fn each_join_prefix_counts_its_true_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    let counts = counts(
        &conn,
        "SELECT max(c.id) FROM a JOIN b ON b.a = a.id JOIN c ON c.b = b.id",
    );
    let loops: Vec<_> = counts
        .iter()
        .filter(|count| count.site.kind == RowCountKind::Loops)
        .collect();
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0].rows, 1);
    let prefixes = prefixes(&counts);
    let tables: Vec<_> = prefixes.iter().map(|p| p.0.as_str()).collect();
    assert_eq!(tables, ["a", "b", "c"], "{prefixes:?}");
    let actual: Vec<_> = prefixes.iter().map(|p| p.2).collect();
    assert_eq!(
        actual,
        [
            A as u64,
            (A * B_PER_A) as u64,
            (A * B_PER_A * C_PER_B) as u64
        ]
    );
    for (table, estimate, rows) in &prefixes {
        assert_eq!(
            q_error(*estimate, *rows),
            1.0,
            "{table}: {estimate} against {rows}"
        );
    }
}

/// Do: the same join with a filter on `a` that keeps a tenth of its rows. Expect: each prefix
/// counts the rows that passed it (10, 30, 60), whatever the planner estimated.
#[test]
fn a_filtered_prefix_counts_the_rows_that_passed_it() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    let counts = counts(
        &conn,
        "SELECT max(c.id) FROM a JOIN b ON b.a = a.id JOIN c ON c.b = b.id WHERE a.v = 3",
    );
    let prefixes = prefixes(&counts);
    let by_table: Vec<_> = prefixes.iter().map(|p| (p.0.as_str(), p.2)).collect();
    assert_eq!(by_table, [("a", 10), ("b", 30), ("c", 60)], "{prefixes:?}");
}

/// Do: a correlated subquery called for each row of `a`. Expect: its loops entered once a row
/// of `a` (100 times), its prefix the rows of all its calls together (300).
#[test]
fn a_correlated_subquery_counts_its_calls_and_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    let counts = counts(
        &conn,
        "SELECT a.id, (SELECT count(*) + max(b.id) FROM b WHERE b.a = a.id) FROM a",
    );
    let inner: Vec<_> = counts.iter().filter(|count| count.site.plan == 1).collect();
    let loops: Vec<_> = inner
        .iter()
        .filter(|count| count.site.kind == RowCountKind::Loops)
        .map(|count| count.rows)
        .collect();
    assert_eq!(loops, [A as u64], "{counts:?}");
    let rows: Vec<_> = inner
        .iter()
        .filter(|count| count.site.kind == RowCountKind::Prefix)
        .map(|count| (count.site.table.clone().unwrap(), count.rows))
        .collect();
    assert_eq!(rows, [("b".to_string(), (A * B_PER_A) as u64)]);
}

/// Do: run a counted statement twice. Expect: the second run's counts are its own, not the sum.
#[test]
fn a_second_run_counts_from_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    let mut stmt = conn
        .prepare("SELECT max(b.id) FROM a JOIN b ON b.a = a.id WHERE a.v = 3")
        .unwrap();
    stmt.run_collect_rows().unwrap();
    let first = stmt.row_counts();
    stmt.reset().unwrap();
    stmt.run_collect_rows().unwrap();
    assert_eq!(stmt.row_counts(), first);
}

/// Do: prepare the join on a connection that did not ask for counts. Expect: no counting
/// instruction in its program and no counters to read.
#[test]
fn counters_are_off_unless_asked() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), false);
    let sql = "SELECT max(c.id) FROM a JOIN b ON b.a = a.id JOIN c ON c.b = b.id";
    let program = conn
        .prepare(format!("EXPLAIN {sql}"))
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(
        program.iter().all(|row| row[1].to_string() != "CountRows"),
        "a counting instruction in an uncounted program"
    );
    assert!(counts(&conn, sql).is_empty());
}

/// Do: compare each counter's estimate with the node EXPLAIN QUERY PLAN FORMAT=JSON gives the
/// same table. Expect: the same output_rows, so the counts read back beside the plan's estimates.
#[test]
fn the_counted_estimates_are_explain_query_plans() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    let sql = "SELECT max(c.id) FROM a JOIN b ON b.a = a.id JOIN c ON c.b = b.id WHERE a.v = 3";
    let json = conn
        .prepare(format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))
        .unwrap()
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[0].to_string())
        .collect::<String>();
    let plan: serde_json::Value = serde_json::from_str(&json).unwrap();
    let nodes = plan["nodes"].as_array().unwrap();
    for (table, estimate, _) in prefixes(&counts(&conn, sql)) {
        let node = nodes
            .iter()
            .find(|node| node["op"]["table"].as_str() == Some(table.as_str()))
            .unwrap();
        assert_eq!(
            node["op"]["estimate"]["output_rows"].as_f64(),
            Some(estimate),
            "{table}"
        );
    }
}

/// Do: join `a` to a copy of `b` with no index on its key, so the planner hashes one side.
/// Expect: the build counts the rows it put in its hash table and the join its true 300 rows.
#[test]
fn a_hash_join_counts_its_build_and_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path(), true);
    conn.execute("CREATE TABLE d (id INTEGER PRIMARY KEY, a INTEGER NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO d SELECT id, a FROM b").unwrap();
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT max(d.id) FROM a JOIN d ON d.a = a.id";
    let plan = conn
        .prepare(format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert!(
        plan.iter().any(|row| row[3].to_string().contains("HASH")),
        "{plan:?}"
    );
    let counts = counts(&conn, sql);
    let builds: Vec<_> = counts
        .iter()
        .filter(|count| count.site.kind == RowCountKind::HashBuild)
        .map(|count| (count.site.table.clone().unwrap(), count.rows))
        .collect();
    let built = match builds.as_slice() {
        [(table, rows)] if table == "a" => *rows == A as u64,
        [(table, rows)] if table == "d" => *rows == (A * B_PER_A) as u64,
        _ => false,
    };
    assert!(built, "{counts:?}");
    let last = prefixes(&counts).last().cloned().unwrap();
    assert_eq!(last.2, (A * B_PER_A) as u64, "{counts:?}");
}
