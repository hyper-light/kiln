//! The join search past 12 tables (docs/design/planner.md decision 19; planner rank 3, PL.3): a join
//! whose connected subsets fit the search's budget is searched exactly, whatever its table count,
//! and one past it is ordered by the greedy search, which adds at each step the table whose joined
//! set has the fewest estimated rows (AC-PL.3.1, T-PL.3.1).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::path::Path;
use std::sync::Arc;

/// A fact table `f` and `dims` dimensions `d1..`, `f.kN` naming `dN`'s key, none analyzed (every
/// table the planner's fallback row count), as the audit's 01-left-deep-greedy.sql has them.
fn star(dir: &Path, dims: usize) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("star.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    let keys: Vec<String> = (1..=dims).map(|n| format!("k{n} INT")).collect();
    conn.execute(format!(
        "CREATE TABLE f(id INTEGER PRIMARY KEY, {}, v INT)",
        keys.join(", ")
    ))
    .unwrap();
    for n in 1..=dims {
        conn.execute(format!(
            "CREATE TABLE d{n}(id INTEGER PRIMARY KEY, v INT, w INT)"
        ))
        .unwrap();
    }
    conn
}

/// The star's join of every dimension, the last filtered.
fn star_query(dims: usize) -> String {
    let joins: Vec<String> = (1..=dims)
        .map(|n| format!("JOIN d{n} ON d{n}.id = f.k{n}"))
        .collect();
    format!(
        "SELECT count(*) FROM f {} WHERE d{dims}.v = 7",
        joins.join(" ")
    )
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

/// The step that reads `table`.
fn step_of(steps: &[String], table: &str) -> usize {
    steps
        .iter()
        .position(|step| step.split_whitespace().nth(1) == Some(table))
        .unwrap_or_else(|| panic!("no step reads {table}: {steps:?}"))
}

/// Do: plan the audit's row 2 query, a fact table and 12 dimensions (13 tables), the last filtered.
/// Expect: the filtered dimension read among the first two tables, as the search of 12 tables
/// reads it, where the greedy search past 12 read it last at 5.7 times the cost
/// (bench/planner/audit/01-left-deep-greedy.out; AC-PL.3.1).
#[test]
fn thirteen_tables_within_the_budget_are_searched_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let conn = star(dir.path(), 12);
    let steps = plan(&conn, &star_query(12));
    assert_eq!(steps.len(), 13, "{steps:?}");
    assert!(step_of(&steps, "d12") <= 1, "{steps:?}");
}

/// Do: plan a fact table and 39 dimensions (40 tables), the last filtered: 2^39 connected subsets,
/// past any budget. Expect: planning ends with a plan of all 40 tables, the filtered dimension
/// among the first two, since its joined set has the fewest rows (T-PL.3.1).
#[test]
fn a_join_past_the_budget_is_ordered_by_the_fewest_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = star(dir.path(), 39);
    let steps = plan(&conn, &star_query(39));
    assert_eq!(steps.len(), 40, "{steps:?}");
    assert!(step_of(&steps, "d39") <= 1, "{steps:?}");
}

/// The audit's row 3 tables, `big`, `mid` and `tiny` (bench/planner/audit/03-right-full-barrier.sql),
/// with a few rows each: mid 6's key matches no big row, and big 3's no mid row.
fn barrier(dir: &Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("barrier.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    for sql in [
        "CREATE TABLE big(id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE mid(id INTEGER PRIMARY KEY, k INT)",
        "CREATE TABLE tiny(id INTEGER PRIMARY KEY, k INT)",
        "INSERT INTO big VALUES (1, 10), (2, 20), (3, 99)",
        "INSERT INTO mid VALUES (5, 10), (6, 30), (7, 20)",
        "INSERT INTO tiny VALUES (5, 0), (6, 0), (8, 0)",
    ] {
        conn.execute(sql).unwrap();
    }
    conn
}

/// The rows of `sql`, columns joined by `|`, NULL as `NULL`.
fn answers(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    amphora_engine::Value::Null => "NULL".to_owned(),
                    value => value.to_string(),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Do: plan the audit's row 3 query, `(big RIGHT JOIN mid ON mid.k = big.k) JOIN tiny ON tiny.id =
/// mid.id WHERE tiny.id = 5`. Expect: tiny's one row read first, as the inner join of the same
/// tables reads it, where the RIGHT JOIN's barrier read big and mid ahead of tiny at
/// 328,300,039,000.3 against 23,007.0 (bench/planner/audit/03-right-full-barrier.out; T-PL.8.1).
#[test]
fn t_pl_8_1_the_right_join_reads_tiny_first() {
    let dir = tempfile::tempdir().unwrap();
    let conn = barrier(dir.path());
    let sql = "SELECT count(*) FROM big RIGHT JOIN mid ON mid.k = big.k \
               JOIN tiny ON tiny.id = mid.id WHERE tiny.id = 5";
    let steps = plan(&conn, sql);
    assert_eq!(steps.len(), 3, "{steps:?}");
    assert_eq!(step_of(&steps, "tiny"), 0, "{steps:?}");
    assert!(step_of(&steps, "mid") < step_of(&steps, "big"), "{steps:?}");
    assert_eq!(answers(&conn, sql), ["1"]);
}

/// Do: run RIGHT JOINs led by one table, which the planner turns into the mirrored LEFT JOIN, with
/// the columns of `*`, a WHERE term on the null-extended table, USING, a later inner join, a later
/// RIGHT JOIN and an aggregate. Expect: SQLite's answers, worked by hand from the rows: mid's rows
/// all kept, big's NULL where no key matches, `*`'s columns in the written order (T-PL.8.2).
#[test]
fn t_pl_8_2_a_right_join_turned_left_keeps_its_answers() {
    let dir = tempfile::tempdir().unwrap();
    let conn = barrier(dir.path());
    let cases: [(&str, &[&str]); 8] = [
        (
            "SELECT * FROM big RIGHT JOIN mid ON mid.k = big.k ORDER BY mid.id",
            &["1|10|5|10", "NULL|NULL|6|30", "2|20|7|20"],
        ),
        (
            "SELECT mid.id FROM big RIGHT JOIN mid ON mid.k = big.k WHERE big.id IS NULL",
            &["6"],
        ),
        (
            "SELECT big.id, mid.id FROM big RIGHT JOIN mid USING (k) ORDER BY mid.id",
            &["1|5", "NULL|6", "2|7"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big RIGHT JOIN mid ON mid.k = big.k \
             JOIN tiny ON tiny.id = mid.id ORDER BY tiny.id",
            &["1|5|5", "NULL|6|6"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big RIGHT JOIN mid ON mid.k = big.k \
             RIGHT JOIN tiny ON tiny.id = mid.id ORDER BY tiny.id",
            &["1|5|5", "NULL|6|6", "NULL|NULL|8"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big RIGHT JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = mid.id ORDER BY mid.id",
            &["1|5|5", "NULL|6|6", "2|7|NULL"],
        ),
        (
            "SELECT count(*), sum(big.id), count(big.id) FROM big RIGHT JOIN mid ON mid.k = big.k",
            &["3|3|2"],
        ),
        (
            "SELECT big.id, mid.id FROM big RIGHT JOIN mid ON mid.k = big.k AND big.id > 1 \
             ORDER BY mid.id",
            &["NULL|5", "NULL|6", "2|7"],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answers(&conn, sql), expected, "{sql}");
    }
}

/// Do: run LEFT JOINs whose right table CD-C's conflict rules let the search read before tables of
/// its left side that its ON terms don't read (PL.8, CD-C's left-asscom rule), beside an inner join
/// reading the null-extended table, an ON term reading no left table, a WHERE term on the NULL row,
/// an aggregate, and semi and anti joins (EXISTS, NOT EXISTS) over them. Expect: SQLite's answers,
/// worked by hand from the rows of `barrier` (T-PL.8.2).
#[test]
fn t_pl_8_2_outer_semi_and_anti_joins_keep_their_answers() {
    let dir = tempfile::tempdir().unwrap();
    let conn = barrier(dir.path());
    let cases: [(&str, &[&str]); 12] = [
        (
            "SELECT big.id, mid.id, tiny.id FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 5 ORDER BY big.id",
            &["1|5|6", "2|7|NULL"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 5 WHERE tiny.id IS NULL",
            &["2|7|NULL"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big LEFT JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 4 ORDER BY big.id",
            &["1|5|5", "2|7|6", "3|NULL|NULL"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big LEFT JOIN mid ON mid.k = big.k \
             JOIN tiny ON tiny.id = mid.id ORDER BY big.id",
            &["1|5|5"],
        ),
        (
            "SELECT big.id, mid.id FROM big LEFT JOIN mid ON mid.id = 6 ORDER BY big.id",
            &["1|6", "2|6", "3|6"],
        ),
        (
            "SELECT big.id, mid.id FROM big LEFT JOIN mid ON mid.id = 9 ORDER BY big.id",
            &["1|NULL", "2|NULL", "3|NULL"],
        ),
        (
            "SELECT big.id, mid.id, tiny.id FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = mid.id AND big.id = 1 ORDER BY big.id",
            &["1|5|5", "2|7|NULL"],
        ),
        (
            "SELECT count(*), count(tiny.id) FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 5",
            &["2|1"],
        ),
        (
            "SELECT big.id FROM big LEFT JOIN mid ON mid.k = big.k \
             WHERE EXISTS (SELECT 1 FROM tiny WHERE tiny.id = mid.id) ORDER BY big.id",
            &["1"],
        ),
        (
            "SELECT big.id FROM big LEFT JOIN mid ON mid.k = big.k \
             WHERE NOT EXISTS (SELECT 1 FROM tiny WHERE tiny.id = mid.id) ORDER BY big.id",
            &["2", "3"],
        ),
        (
            "SELECT big.id, tiny.id FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 5 \
             WHERE EXISTS (SELECT 1 FROM tiny t2 WHERE t2.id = mid.id) ORDER BY big.id",
            &["1|6"],
        ),
        (
            "SELECT big.id, tiny.id FROM big JOIN mid ON mid.k = big.k \
             LEFT JOIN tiny ON tiny.id = big.id + 5 \
             WHERE NOT EXISTS (SELECT 1 FROM tiny t2 WHERE t2.id = mid.id) ORDER BY big.id",
            &["2|NULL"],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answers(&conn, sql), expected, "{sql}");
    }
}
