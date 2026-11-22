//! No table read whole beneath loops that can give more than one row (PL.6, docs/research/planner.md
//! rank 6; docs/design/planner.md, "Decision (lane pl6)"). Such a table is read again for each row
//! of the loops before it, whatever their estimate said: the audit's `10-no-adaptive` scanned q
//! 3,000 times on statistics taken at 10 rows. A seek, an automatic index or a hash join reads it
//! instead; a whole read stays only beneath one-row loops (a lookup by a unique key) or where
//! nothing else can read the table (no equality joins it). Each plan here is counted as
//! `bench/planner/loops.py` counts the bench's (T-PL.6.1, AC-PL.6.1).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("loops.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// The table accesses of `sql`'s plan that read a table whole beneath loops that can give more
/// than one row, as `bench/planner/loops.py` counts them: in each block of the plan, a scan of a
/// table or index, or a hash join (whose probe scans its table), after a table access that isn't a
/// lookup by rowid. A hash join's build table (its `build`) is listed in the block after the probe
/// but read once, before the first probe, so its read is neither counted nor a loop.
fn whole_reads_in_loops(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))
        .unwrap();
    let text = stmt.run_collect_rows().unwrap()[0][0].to_string();
    let plan: serde_json::Value = serde_json::from_str(&text).unwrap();
    let nodes = plan["nodes"].as_array().unwrap();
    let mut found = Vec::new();
    let mut parents: Vec<&serde_json::Value> = Vec::new();
    for node in nodes {
        if !parents.contains(&&node["parent"]) {
            parents.push(&node["parent"]);
        }
    }
    for parent in parents {
        let block: Vec<&serde_json::Value> =
            nodes.iter().filter(|n| &n["parent"] == parent).collect();
        let builds: Vec<&serde_json::Value> = block
            .iter()
            .filter(|n| n["op"]["type"] == "hash_join" && n["op"]["build"].is_string())
            .map(|n| &n["op"]["build"])
            .collect();
        let mut before: Vec<&serde_json::Value> = Vec::new();
        for node in block {
            let op = &node["op"];
            let kind = op["type"].as_str().unwrap_or("");
            if !["scan", "search", "multi_index", "hash_join", "index_method"].contains(&kind) {
                continue;
            }
            let identifier = if op["alias"].is_string() {
                &op["alias"]
            } else {
                &op["table"]
            };
            if kind == "scan" && builds.contains(&identifier) {
                continue;
            }
            let whole = (kind == "scan" && op["source"] == "table") || kind == "hash_join";
            if whole
                && !before.is_empty()
                && !before.iter().all(|b| b["op"]["search_kind"] == "rowid_eq")
            {
                found.push(node["detail"].as_str().unwrap().to_string());
            }
            before.push(node);
        }
    }
    found.sort();
    found.dedup();
    found
}

fn plan_text(conn: &Arc<Connection>, sql: &str) -> String {
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[3].to_string())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn count(conn: &Arc<Connection>, sql: &str) -> i64 {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows().unwrap()[0][0].as_int().unwrap()
}

/// The audit's `10-no-adaptive` (bench/planner/audit/10-no-adaptive.sql): p and q analyzed at 10
/// rows each, then grown to 3,000.
fn grown_past_their_statistics(dir: &Path) -> Arc<Connection> {
    let conn = open(dir);
    for sql in [
        "CREATE TABLE p(id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE q(id INTEGER PRIMARY KEY, k INT)",
        "INSERT INTO p(k) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10) SELECT i FROM n",
        "INSERT INTO q(k) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10) SELECT i FROM n",
        "ANALYZE",
        "INSERT INTO p(k) WITH RECURSIVE n(i) AS (SELECT 11 UNION ALL SELECT i + 1 FROM n WHERE i < 3000) SELECT i FROM n",
        "INSERT INTO q(k) WITH RECURSIVE n(i) AS (SELECT 11 UNION ALL SELECT i + 1 FROM n WHERE i < 3000) SELECT i FROM n",
    ] {
        conn.execute(sql).unwrap();
    }
    conn
}

/// Do: the audit's `10-no-adaptive` at 3,000 rows a table, its join planned on statistics taken at
/// 10 (T-PL.6.1). Expect: neither table read whole beneath the other's loop, so q is read once (by
/// a hash join or an automatic index, built once) where the parent scanned it for each row of p;
/// and its count, 3,000, unchanged.
#[test]
fn t_pl_6_1_ten_no_adaptive_reads_q_once() {
    let dir = tempfile::tempdir().unwrap();
    let conn = grown_past_their_statistics(dir.path());
    let sql = "SELECT count(*) FROM p JOIN q ON q.k = p.k";
    assert_eq!(
        whole_reads_in_loops(&conn, sql),
        Vec::<String>::new(),
        "{}",
        plan_text(&conn, sql)
    );
    assert_eq!(count(&conn, sql), 3_000);
}

/// Do: the same tables joined LEFT and with the join's terms the other way round. Expect: no whole
/// read beneath the first loop either way, and every row of p counted once.
#[test]
fn an_outer_join_reads_its_inner_table_once() {
    let dir = tempfile::tempdir().unwrap();
    let conn = grown_past_their_statistics(dir.path());
    for sql in [
        "SELECT count(*) FROM p LEFT JOIN q ON q.k = p.k",
        "SELECT count(*) FROM q JOIN p ON p.k = q.k",
    ] {
        assert_eq!(
            whole_reads_in_loops(&conn, sql),
            Vec::<String>::new(),
            "{sql}: {}",
            plan_text(&conn, sql)
        );
        assert_eq!(count(&conn, sql), 3_000, "{sql}");
    }
}

/// JOB 1a's first two tables (step 3's 30 plans): a small dimension table filtered to one kind by a
/// value its statistics take for one row, and a fact table with an index on the column that joins
/// them, most of its rows of that kind. At step 3 the fact table was scanned beneath the
/// dimension's scan; the dimension's one row made that cost nothing, but nothing guaranteed it.
fn dimension_and_facts(dir: &Path) -> Arc<Connection> {
    let conn = open(dir);
    conn.execute("CREATE TABLE company_type (id INTEGER PRIMARY KEY, kind TEXT NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE movie_companies (id INTEGER PRIMARY KEY, movie_id INTEGER NOT NULL, company_type_id INTEGER NOT NULL, note TEXT)").unwrap();
    conn.execute(
        "CREATE INDEX company_type_id_movie_companies ON movie_companies (company_type_id)",
    )
    .unwrap();
    conn.execute("INSERT INTO company_type VALUES (1, 'distributors'), (2, 'production companies'), (3, 'special effects companies'), (4, 'miscellaneous companies')").unwrap();
    conn.execute("INSERT INTO movie_companies(movie_id, company_type_id, note) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000) SELECT i, CASE WHEN i % 10 < 6 THEN 2 WHEN i % 10 < 9 THEN 1 ELSE 3 + i % 2 END, CASE WHEN i % 7 = 0 THEN '(presents)' END FROM n").unwrap();
    conn.execute("ANALYZE").unwrap();
    conn
}

/// Do: JOB 1a's first join, the dimension filtered to one kind and joined to the facts. Expect: the
/// facts not read whole beneath the dimension's loop (a seek or a hash join reads them), and the
/// count of the kind's facts, 12,000, unchanged.
#[test]
fn a_dimension_filtered_to_one_row_by_estimate_does_not_loop_a_whole_read() {
    let dir = tempfile::tempdir().unwrap();
    let conn = dimension_and_facts(dir.path());
    for sql in [
        "SELECT count(*) FROM company_type AS ct, movie_companies AS mc WHERE ct.kind = 'production companies' AND mc.company_type_id = ct.id",
        "SELECT count(*) FROM company_type AS ct, movie_companies AS mc WHERE ct.kind = 'production companies' AND mc.company_type_id = ct.id AND mc.note IS NULL",
    ] {
        assert_eq!(
            whole_reads_in_loops(&conn, sql),
            Vec::<String>::new(),
            "{sql}: {}",
            plan_text(&conn, sql)
        );
    }
    assert_eq!(
        count(&conn, "SELECT count(*) FROM company_type AS ct, movie_companies AS mc WHERE ct.kind = 'production companies' AND mc.company_type_id = ct.id"),
        12_000
    );
}

/// Do: a join beneath a lookup of one row by rowid. Expect: the inner table may still be scanned,
/// once (the one-row loop is the rule's exception), and the count is the row's matches.
#[test]
fn a_whole_read_beneath_a_rowid_lookup_stays() {
    let dir = tempfile::tempdir().unwrap();
    let conn = grown_past_their_statistics(dir.path());
    let sql = "SELECT count(*) FROM p JOIN q ON q.k = p.k WHERE p.id = 5";
    let plan = plan_text(&conn, sql);
    assert!(plan.contains("SCAN q"), "{plan}");
    assert_eq!(
        whole_reads_in_loops(&conn, sql),
        Vec::<String>::new(),
        "{plan}"
    );
    assert_eq!(count(&conn, sql), 1);
}

/// Do: a cross join and a join by a range, which no hash join or automatic index on an equality
/// can read. Expect: the whole read stays, the only plan there is, and the counter finds it (so it
/// is not vacuous); the counts are right.
#[test]
fn a_join_with_no_equality_keeps_its_loop() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE a(id INTEGER PRIMARY KEY, x INT)")
        .unwrap();
    conn.execute("CREATE TABLE b(id INTEGER PRIMARY KEY, y INT)")
        .unwrap();
    conn.execute("INSERT INTO a(x) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40) SELECT i FROM n").unwrap();
    conn.execute("INSERT INTO b(y) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 30) SELECT i FROM n").unwrap();
    conn.execute("ANALYZE").unwrap();
    let cross = "SELECT count(*) FROM a, b";
    assert_eq!(
        whole_reads_in_loops(&conn, cross).len(),
        1,
        "{}",
        plan_text(&conn, cross)
    );
    assert_eq!(count(&conn, cross), 1_200);
    let range = "SELECT count(*) FROM a JOIN b ON b.y < a.x";
    assert_eq!(
        count(&conn, range),
        (1..=40).map(|x: i64| (x - 1).min(30)).sum::<i64>()
    );
}

/// Do: three tables joined in a chain with no index on any join column, so the third is joined
/// beneath two loops. Expect: no whole read beneath them (a hash join's probe beneath the first
/// table's loop would be one: its build input is taken from both instead), and the count right.
#[test]
fn a_chain_of_hash_joins_reads_each_table_once() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    for sql in [
        "CREATE TABLE r(id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE s(id INTEGER PRIMARY KEY, k INT, j INT)",
        "CREATE TABLE t(id INTEGER PRIMARY KEY, j INT)",
        "INSERT INTO r(k) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 300) SELECT i FROM n",
        "INSERT INTO s(k, j) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000) SELECT i % 300, i % 500 FROM n",
        "INSERT INTO t(j) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) SELECT i FROM n",
        "ANALYZE",
    ] {
        conn.execute(sql).unwrap();
    }
    let sql = "SELECT count(*) FROM r JOIN s ON s.k = r.k JOIN t ON t.j = s.j";
    assert_eq!(
        whole_reads_in_loops(&conn, sql),
        Vec::<String>::new(),
        "{}",
        plan_text(&conn, sql)
    );
    // s's rows whose k is in 1..=300 (k = 0 has no r row) and whose j is in 1..=499 (j = 0 has no t
    // row): i from 1 to 2,000 with i % 300 != 0 and i % 500 != 0.
    let want = (1..=2000)
        .filter(|i: &i64| i % 300 != 0 && i % 500 != 0)
        .count() as i64;
    assert_eq!(count(&conn, sql), want);
}
