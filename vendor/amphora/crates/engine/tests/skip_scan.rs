//! A constraint on an index's second column seeks it once for each distinct value of its first,
//! where ANALYZE counted many rows to a value: a skip-scan, as SQLite's (`docs/design/planner.md`,
//! decision 4). It reads what a scan of the same rows reads, for ranges, equalities and `IS NULL`,
//! with NULLs in either column, a case-blind first column, a descending second, a third after
//! it, in order, under a LIMIT and inside a join (AC-T1.1.1: reads as committed), and its work
//! grows with the values and the rows it keeps, not the table (AC-T1.13.4: reads at their
//! latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("skip.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn plan(conn: &Arc<Connection>, sql: &str) -> String {
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[3].to_string())
        .collect::<Vec<_>>()
        .join(" / ")
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row.iter().map(|v| v.to_string()).collect())
        .collect()
}

/// `g` takes 21 values, one of them NULL (in a text column, letters in either case: one value to
/// a case-blind column); `c` a value in 0..97, even alone for ten of `g`'s values (NULL among
/// them), so a seek for an odd one finds none there, and NULL in every fifth row; `d` a value in
/// 0..13. `name` holds `rows` rows and gets the index `index`; `plain_name` holds the same rows,
/// no index.
fn load(conn: &Arc<Connection>, name: &str, g_type: &str, index: &str, rows: i64) {
    for table in [name.to_owned(), format!("plain_{name}")] {
        conn.execute(format!(
            "CREATE TABLE {table} (id INTEGER PRIMARY KEY, g {g_type}, c INTEGER, d INTEGER)"
        ))
        .unwrap();
    }
    let g = if g_type.starts_with("TEXT") {
        "CASE WHEN id % 21 = 0 THEN NULL WHEN id % 2 = 0 THEN upper(char(97 + id % 21)) \
         ELSE char(97 + id % 21) END"
    } else {
        "CASE WHEN id % 21 = 0 THEN NULL ELSE id % 21 END"
    };
    conn.execute(format!(
        "WITH RECURSIVE n(id) AS (SELECT 1 UNION ALL SELECT id + 1 FROM n WHERE id < {rows}) \
         INSERT INTO {name} SELECT id, {g}, CASE WHEN id % 5 = 0 THEN NULL WHEN id % 21 < 10 \
         THEN (id * 31) % 97 / 2 * 2 ELSE (id * 31) % 97 END, \
         (id * 17) % 13 FROM n"
    ))
    .unwrap();
    conn.execute(format!("INSERT INTO plain_{name} SELECT * FROM {name}"))
        .unwrap();
    conn.execute(format!("CREATE INDEX {name}_idx ON {name} {index}"))
        .unwrap();
}

/// Do: ANALYZE a table whose first index column holds 21 values, then a constraint on its second
/// column alone. Expect: a skip-scan of the index; none before ANALYZE, none when the first
/// column is all but unique, none with the first column constrained.
#[test]
fn a_constraint_on_the_second_column_skip_scans_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn, "t", "INTEGER", "(g, c)", 2000);
    let sql = "SELECT COUNT(*) FROM t WHERE c BETWEEN ? AND ?";
    assert!(
        !plan(&conn, sql).contains("ANY("),
        "no skip-scan before ANALYZE: {}",
        plan(&conn, sql)
    );
    conn.execute("ANALYZE").unwrap();
    assert_eq!(
        plan(&conn, sql),
        "SEARCH t USING COVERING INDEX t_idx (ANY(g) AND c>=? AND c<=?)"
    );
    let eq = plan(&conn, "SELECT id FROM t WHERE c = 5");
    assert_eq!(eq, "SEARCH t USING COVERING INDEX t_idx (ANY(g) AND c=?)");
    let both = plan(&conn, "SELECT id FROM t WHERE g = 3 AND c > 5");
    assert!(
        !both.contains("ANY("),
        "the first column constrained: {both}"
    );

    conn.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, g INTEGER, c INTEGER)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for id in 1..=2000 {
        conn.execute(format!("INSERT INTO u VALUES ({id}, {id}, {})", id % 97))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("CREATE INDEX u_idx ON u (g, c)").unwrap();
    conn.execute("ANALYZE").unwrap();
    let unique = plan(&conn, "SELECT COUNT(*) FROM u WHERE c BETWEEN 3 AND 9");
    assert!(!unique.contains("ANY("), "a value to a row: {unique}");
}

/// Do: for each table (an integer first column, a case-blind text one, a descending second
/// column, a third column after it), statements of every shape against the indexed table and
/// its copy with no index. Expect: the same rows, in the same order where the statement orders
/// them; and a skip-scan where the shape allows one.
#[test]
fn a_skip_scan_reads_what_a_scan_reads() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn, "t", "INTEGER", "(g, c)", 20_000);
    load(&conn, "nc", "TEXT COLLATE NOCASE", "(g, c)", 20_000);
    load(&conn, "dsc", "INTEGER", "(g, c DESC)", 20_000);
    load(&conn, "three", "INTEGER", "(g, c, d)", 20_000);
    conn.execute("CREATE TABLE probe (v INTEGER)").unwrap();
    conn.execute("INSERT INTO probe VALUES (3), (40), (NULL), (96), (500)")
        .unwrap();
    conn.execute("ANALYZE").unwrap();

    // (filter on the table, whether a skip-scan serves its count). `d` is in only one index:
    // elsewhere a filter on it reads the table's rows, and a third of them are cheaper scanned.
    let filters = [
        ("c BETWEEN 10 AND 20", true),
        ("c > 50", true),
        ("c >= 50", true),
        ("c < 5", true),
        ("c <= 5", true),
        ("c = 7", true),
        ("c > 10 AND c < 15", true),
        ("c BETWEEN 1000 AND 2000", true),
        ("c IS NULL", true),
        ("c BETWEEN 10 AND 20 AND g <> 4", true),
        ("g > 3 AND c > 10", false),
        ("c > 90 AND d = 3", false),
    ];
    let mut skip_scans = 0;
    for table in ["t", "nc", "dsc", "three"] {
        for (filter, expect_skip) in filters {
            let forms = [
                format!("SELECT id FROM {{t}} WHERE {filter} ORDER BY id"),
                format!("SELECT COUNT(*), SUM(c), MIN(id), MAX(id) FROM {{t}} WHERE {filter}"),
                format!("SELECT id, g, c FROM {{t}} WHERE {filter} ORDER BY g, c, id"),
                format!("SELECT id FROM {{t}} WHERE {filter} ORDER BY c, id"),
                format!("SELECT id FROM {{t}} WHERE {filter} ORDER BY g, c, id LIMIT 7"),
            ];
            for form in &forms {
                let on = form.replace("{t}", table);
                let off = form.replace("{t}", &format!("plain_{table}"));
                assert_eq!(rows(&conn, &on), rows(&conn, &off), "{on}");
            }
            let shown = plan(
                &conn,
                &format!("SELECT COUNT(*) FROM {table} WHERE {filter}"),
            );
            let expect_skip = expect_skip || (table == "three" && filter.contains("d ="));
            if expect_skip {
                assert!(shown.contains("ANY(g)"), "{table}, {filter}: {shown}");
                skip_scans += 1;
            }
        }
        let join = "SELECT p.v, x.id FROM probe p JOIN {t} x ON x.c = p.v ORDER BY 1, 2";
        assert_eq!(
            rows(&conn, &join.replace("{t}", table)),
            rows(&conn, &join.replace("{t}", &format!("plain_{table}"))),
            "{table}, a join"
        );
    }
    assert_eq!(skip_scans, 41, "every shape that allows one skip-scans");
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Numeric(Numeric::Integer(i)) => *i,
        other => panic!("{other:?}"),
    }
}

/// Do: count a narrow range of the second column of a 30,000-row table whose first column holds
/// 21 values. Expect: the right count, for instructions in the hundreds, where a scan of the
/// table ran some for every row.
#[test]
fn a_skip_scans_work_grows_with_its_values_not_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn, "t", "INTEGER", "(g, c)", 30_000);
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT COUNT(*) FROM t WHERE c BETWEEN 10 AND 11";
    let mut stmt = conn.prepare(sql).unwrap();
    let got = int(&stmt.run_collect_rows().unwrap()[0][0]);
    let want = int(&conn
        .prepare("SELECT COUNT(*) FROM plain_t WHERE c BETWEEN 10 AND 11")
        .unwrap()
        .run_collect_rows()
        .unwrap()[0][0]);
    assert_eq!(got, want);
    let insns = stmt.metrics().insn_executed;
    // 21 values, each a seek and a few instructions, and about 530 rows counted.
    let bound = 21 * 20 + 4 * want as u64 + 100;
    assert!(insns < bound, "{insns} instructions, bound {bound}");
}

/// Do: UPDATE and DELETE through a skip-scan (a column outside the index, the scanned column
/// itself moved past the range and within it, rows deleted), the same on the unindexed copy.
/// Expect: the same rows after each, the skip-scan planned for each (AC-T1.1.1).
#[test]
fn writes_through_a_skip_scan_change_what_a_scan_changes() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn, "t", "INTEGER", "(g, c)", 20_000);
    conn.execute("ANALYZE").unwrap();
    let writes = [
        "UPDATE {t} SET d = d + 100 WHERE c BETWEEN 10 AND 20",
        "UPDATE {t} SET c = c + 1000 WHERE c BETWEEN 30 AND 40",
        "UPDATE {t} SET c = c + 1 WHERE c BETWEEN 50 AND 60",
        "DELETE FROM {t} WHERE c BETWEEN 70 AND 75",
    ];
    for write in writes {
        let shown = plan(&conn, &write.replace("{t}", "t"));
        assert!(shown.contains("ANY(g)"), "{write}: {shown}");
        conn.execute(write.replace("{t}", "t")).unwrap();
        conn.execute(write.replace("{t}", "plain_t")).unwrap();
        let all = "SELECT id, g, c, d FROM {t} ORDER BY id";
        assert_eq!(
            rows(&conn, &all.replace("{t}", "t")),
            rows(&conn, &all.replace("{t}", "plain_t")),
            "after {write}"
        );
    }
    let check = "SELECT id FROM t WHERE c BETWEEN 0 AND 2000 ORDER BY id";
    assert_eq!(
        rows(&conn, check),
        rows(&conn, &check.replace(" t ", " plain_t ")),
        "the index agrees with the table"
    );
}
