//! `x IN (SELECT ...)` answers a row with one search of the subquery's list, as SQLite does
//! (`docs/design/executor.md`, decision 2): in a condition a NULL answer fails the row as FALSE does,
//! so a miss fails it at once; where the two differ (`NOT IN`, or the answer used as a value), a
//! one-column list holds its NULLs first, so its first entry says whether it holds one. Every form
//! still answers SQL's three values (AC-T1.1.1: reads as committed; AC-T1.13.4: reads at their
//! latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::num::NonZero;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("in.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

const PROBES: i64 = 1000;
const LISTED: i64 = 2000;

/// `probe` holds odd numbers but for every hundredth row, `list` the even numbers: ten probes are
/// in the list, the rest miss it.
fn probe_and_list(conn: &Arc<Connection>) {
    conn.execute("CREATE TABLE probe (id INTEGER PRIMARY KEY, x INTEGER)")
        .unwrap();
    conn.execute("CREATE TABLE list (id INTEGER PRIMARY KEY, y INTEGER)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    for id in 1..=PROBES {
        let x = if id % 100 == 0 { 2 * id } else { 2 * id + 1 };
        conn.execute(format!("INSERT INTO probe VALUES ({id}, {x})"))
            .unwrap();
    }
    for id in 1..=LISTED {
        conn.execute(format!("INSERT INTO list VALUES ({id}, {})", 2 * id))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// The rows `sql` returns and the instructions it ran.
fn run(conn: &Arc<Connection>, sql: &str) -> (Vec<Vec<Value>>, u64) {
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    (rows, stmt.metrics().insn_executed)
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Numeric(Numeric::Integer(i)) => *i,
        other => panic!("{other:?}"),
    }
}

/// Instructions a statement over every probe and every listed value may run: a few dozen a row.
/// A miss that searched the list for NULLs ran some thousands of instructions each.
const BOUND: u64 = 40 * (PROBES as u64 + LISTED as u64);

/// Do: keep the probes found in the list, most of them misses. Expect: the ten hits, each miss
/// answered without reading more of the list.
#[test]
fn a_miss_in_a_condition_reads_no_more_of_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    probe_and_list(&conn);
    let (rows, insns) = run(
        &conn,
        "SELECT id FROM probe WHERE x IN (SELECT y FROM list)",
    );
    let ids: Vec<i64> = rows.iter().map(|row| int(&row[0])).collect();
    assert_eq!(ids, (1..=10).map(|i| i * 100).collect::<Vec<_>>());
    assert!(insns < BOUND, "{insns} instructions, bound {BOUND}");
}

/// Do: the same probes as values of `IN` and `NOT IN`, and kept by `NOT IN`, where a NULL in the
/// list would make a miss NULL. Expect: the right answers, each miss reading the list's first
/// entry alone.
#[test]
fn a_miss_where_null_differs_reads_only_the_first_entry() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    probe_and_list(&conn);
    let (rows, insns) = run(
        &conn,
        "SELECT x IN (SELECT y FROM list), x NOT IN (SELECT y FROM list) FROM probe",
    );
    assert_eq!(rows.len(), PROBES as usize);
    for (id, row) in (1..=PROBES).zip(&rows) {
        let hit = i64::from(id % 100 == 0);
        assert_eq!((int(&row[0]), int(&row[1])), (hit, 1 - hit), "probe {id}");
    }
    assert!(
        insns < 2 * BOUND,
        "{insns} instructions, bound {}",
        2 * BOUND
    );
    let (rows, insns) = run(
        &conn,
        "SELECT id FROM probe WHERE x NOT IN (SELECT y FROM list)",
    );
    assert_eq!(rows.len(), (PROBES - 10) as usize);
    assert!(insns < BOUND, "{insns} instructions, bound {BOUND}");
}

/// SQL's answer to `lhs IN list`: FALSE for an empty list; NULL for a NULL; TRUE if found; NULL
/// if the list holds a NULL; FALSE otherwise.
fn model_in(lhs: Option<i64>, list: &[Option<i64>]) -> Option<bool> {
    if list.is_empty() {
        return Some(false);
    }
    let v = lhs?;
    if list.contains(&Some(v)) {
        Some(true)
    } else if list.contains(&None) {
        None
    } else {
        Some(false)
    }
}

/// SQL's answer to `(a, b) IN list`: TRUE if a row equals it; else NULL if a row is equal but for
/// NULLs; else FALSE.
fn model_row_in(lhs: [Option<i64>; 2], list: &[[Option<i64>; 2]]) -> Option<bool> {
    let mut unknown = false;
    for row in list {
        let pairs = lhs.iter().zip(row);
        let differs = pairs
            .clone()
            .any(|(a, b)| matches!((a, b), (Some(a), Some(b)) if a != b));
        let equal = pairs
            .clone()
            .all(|(a, b)| matches!((a, b), (Some(a), Some(b)) if a == b));
        if equal {
            return Some(true);
        }
        unknown |= !differs;
    }
    if unknown {
        None
    } else {
        Some(false)
    }
}

fn three(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        other => Some(int(other) != 0),
    }
}

fn bind(stmt: &mut amphora_engine::Statement, at: usize, v: Option<i64>) {
    let value = v.map_or(Value::Null, Value::from_i64);
    stmt.bind_at(NonZero::new(at).unwrap(), value).unwrap();
}

/// The answer of `sql`'s single value, one parameter bound per element of `params`.
fn answer(conn: &Arc<Connection>, sql: &str, params: &[Option<i64>]) -> Option<bool> {
    let mut stmt = conn.prepare(sql).unwrap();
    for (i, p) in params.iter().enumerate() {
        bind(&mut stmt, i + 1, *p);
    }
    three(&stmt.run_collect_rows().unwrap()[0][0])
}

/// Whether `sql`, a statement keeping one row or none, keeps its row.
fn kept(conn: &Arc<Connection>, sql: &str, params: &[Option<i64>]) -> bool {
    let mut stmt = conn.prepare(sql).unwrap();
    for (i, p) in params.iter().enumerate() {
        bind(&mut stmt, i + 1, *p);
    }
    !stmt.run_collect_rows().unwrap().is_empty()
}

/// Do: every form of `IN` and `NOT IN` (a value, a condition, under `OR` and `NOT`, a column or a
/// parameter on the left) against lists empty, without NULLs, and with NULLs first, between and
/// last. Expect: SQL's answer in every case.
#[test]
fn every_form_answers_sqls_three_values() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE one (id INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO one VALUES (1)").unwrap();
    conn.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    let probes = [Some(1), Some(2), Some(5), None];
    for (id, v) in probes.iter().enumerate() {
        let v = v.map_or("NULL".to_owned(), |v| v.to_string());
        conn.execute(format!("INSERT INTO p VALUES ({id}, {v})"))
            .unwrap();
    }
    let lists: [&[Option<i64>]; 7] = [
        &[],
        &[Some(1), Some(2), Some(3)],
        &[None, Some(1), Some(3)],
        &[Some(1), None, Some(3)],
        &[Some(1), Some(3), None],
        &[None],
        &[None, None, Some(2)],
    ];
    for list in lists {
        conn.execute("DROP TABLE IF EXISTS l").unwrap();
        conn.execute("CREATE TABLE l (id INTEGER PRIMARY KEY, y INTEGER)")
            .unwrap();
        for (id, y) in list.iter().enumerate() {
            let y = y.map_or("NULL".to_owned(), |y| y.to_string());
            conn.execute(format!("INSERT INTO l VALUES ({id}, {y})"))
                .unwrap();
        }
        for lhs in probes {
            let want = model_in(lhs, list);
            let not = want.map(|b| !b);
            let case = format!("list {list:?}, lhs {lhs:?}");
            let p = [lhs];
            assert_eq!(
                answer(&conn, "SELECT ? IN (SELECT y FROM l)", &p),
                want,
                "IN, {case}"
            );
            assert_eq!(
                answer(&conn, "SELECT ? NOT IN (SELECT y FROM l)", &p),
                not,
                "NOT IN, {case}"
            );
            let cond = |sql: &str| kept(&conn, sql, &p);
            assert_eq!(
                cond("SELECT 1 FROM one WHERE ? IN (SELECT y FROM l)"),
                want == Some(true),
                "WHERE IN, {case}"
            );
            assert_eq!(
                cond("SELECT 1 FROM one WHERE ? NOT IN (SELECT y FROM l)"),
                not == Some(true),
                "WHERE NOT IN, {case}"
            );
            assert_eq!(
                cond("SELECT 1 FROM one WHERE ? IN (SELECT y FROM l) OR one.id < 0"),
                want == Some(true),
                "IN OR, {case}"
            );
            assert_eq!(
                cond("SELECT 1 FROM one WHERE NOT (? IN (SELECT y FROM l))"),
                want == Some(false),
                "NOT (IN), {case}"
            );
            assert_eq!(
                cond("SELECT 1 FROM one WHERE CASE WHEN ? IN (SELECT y FROM l) THEN 1 ELSE 0 END"),
                want == Some(true),
                "CASE, {case}"
            );
        }
        let mut stmt = conn
            .prepare("SELECT v IN (SELECT y FROM l), v NOT IN (SELECT y FROM l) FROM p ORDER BY id")
            .unwrap();
        let rows = stmt.run_collect_rows().unwrap();
        for (lhs, row) in probes.iter().zip(&rows) {
            let want = model_in(*lhs, list);
            assert_eq!(
                (three(&row[0]), three(&row[1])),
                (want, want.map(|b| !b)),
                "column {lhs:?}, list {list:?}"
            );
        }
        let mut stmt = conn
            .prepare("SELECT id FROM p WHERE v IN (SELECT y FROM l) ORDER BY id")
            .unwrap();
        let ids: Vec<i64> = stmt
            .run_collect_rows()
            .unwrap()
            .iter()
            .map(|row| int(&row[0]))
            .collect();
        let want: Vec<i64> = (0i64..)
            .zip(probes)
            .filter(|(_, lhs)| model_in(*lhs, list) == Some(true))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, want, "WHERE v IN, list {list:?}");
    }
}

/// Do: `(a, b) IN` and `NOT IN` against lists of pairs with NULLs in either place, as values and
/// as conditions. Expect: SQL's answer in every case.
#[test]
fn a_row_value_answers_sqls_three_values() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE one (id INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO one VALUES (1)").unwrap();
    let lists: [&[[Option<i64>; 2]]; 5] = [
        &[],
        &[[Some(1), Some(2)], [Some(3), Some(4)]],
        &[[Some(1), None], [Some(3), Some(4)]],
        &[[None, Some(2)]],
        &[[Some(3), Some(4)], [None, None]],
    ];
    let probes = [
        [Some(1), Some(2)],
        [Some(1), Some(5)],
        [Some(3), Some(4)],
        [None, Some(2)],
        [Some(1), None],
        [None, None],
        [Some(7), Some(7)],
    ];
    for list in lists {
        conn.execute("DROP TABLE IF EXISTS l2").unwrap();
        conn.execute("CREATE TABLE l2 (id INTEGER PRIMARY KEY, a INTEGER, b INTEGER)")
            .unwrap();
        for (id, [a, b]) in list.iter().enumerate() {
            let s = |v: &Option<i64>| v.map_or("NULL".to_owned(), |v| v.to_string());
            conn.execute(format!("INSERT INTO l2 VALUES ({id}, {}, {})", s(a), s(b)))
                .unwrap();
        }
        for lhs in probes {
            let want = model_row_in(lhs, list);
            let case = format!("list {list:?}, lhs {lhs:?}");
            let p = lhs.to_vec();
            assert_eq!(
                answer(&conn, "SELECT (?, ?) IN (SELECT a, b FROM l2)", &p),
                want,
                "IN, {case}"
            );
            assert_eq!(
                answer(&conn, "SELECT (?, ?) NOT IN (SELECT a, b FROM l2)", &p),
                want.map(|b| !b),
                "NOT IN, {case}"
            );
            assert_eq!(
                kept(
                    &conn,
                    "SELECT 1 FROM one WHERE (?, ?) IN (SELECT a, b FROM l2)",
                    &p
                ),
                want == Some(true),
                "WHERE IN, {case}"
            );
            assert_eq!(
                kept(
                    &conn,
                    "SELECT 1 FROM one WHERE (?, ?) NOT IN (SELECT a, b FROM l2)",
                    &p
                ),
                want == Some(false),
                "WHERE NOT IN, {case}"
            );
        }
    }
}
