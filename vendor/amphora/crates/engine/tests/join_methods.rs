//! Query-speed step 5's planned test (QT.5, T-QT.5.1): every join run both ways. Random tables with
//! NULL and duplicate join keys, joined inner, left, semi and anti, on one key and two, over an
//! indexed column and an unindexed one, are planned with index nested loops only, with hash joins
//! only (`Connection::set_join_methods`) and by cost; the answers must be identical, and the inner
//! and left joins must equal a nested loop computed here from the rows themselves.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, JoinMethods, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::sync::Arc;

const ROWS: usize = 300;

/// A key from 0 to 19, or NULL about one row in ten: every key repeats, and NULLs join nothing.
fn key(state: &mut u64) -> Option<i64> {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    let draw = (*state % 220) as i64;
    if draw < 200 {
        Some(draw % 20)
    } else {
        None
    }
}

fn sql_value(v: Option<i64>) -> String {
    v.map_or_else(|| "NULL".to_string(), |v| v.to_string())
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out: Vec<String> = stmt
        .run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| format!("{row:?}"))
        .collect();
    out.sort();
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

/// Do: run each join under every method. Expect: one answer, the model's where it has one; hash
/// joins only plans a hash join for the plain inner join and nested loops only plans none.
#[test]
fn t_qt_5_1_every_join_run_both_ways_gives_one_answer() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("joins.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, k, x)")
        .unwrap();
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, k, y)")
        .unwrap();
    conn.execute("CREATE INDEX b_k ON b (k)").unwrap();
    conn.execute("CREATE TABLE c (k, z)").unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut a = Vec::new();
    let mut b = Vec::new();
    for id in 1..=ROWS as i64 {
        let (ak, bk, ck) = (key(&mut state), key(&mut state), key(&mut state));
        let x = (id * 37) % 100;
        a.push((id, ak));
        b.push((id, bk));
        conn.execute(format!(
            "INSERT INTO a VALUES ({id}, {}, {x})",
            sql_value(ak)
        ))
        .unwrap();
        conn.execute(format!(
            "INSERT INTO b VALUES ({id}, {}, {})",
            sql_value(bk),
            id % 7
        ))
        .unwrap();
        conn.execute(format!(
            "INSERT INTO c VALUES ({}, {})",
            sql_value(ck),
            (id * 13) % 100
        ))
        .unwrap();
    }
    conn.execute("ANALYZE").unwrap();

    let inner = "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k";
    let left = "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k";
    let queries = [
        inner,
        left,
        "SELECT a.id, c.z FROM a JOIN c ON c.k = a.k WHERE a.x < 50",
        "SELECT a.id, c.z FROM a LEFT JOIN c ON c.k = a.k AND c.z > 40",
        "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        "SELECT a.id FROM a WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.k = a.k)",
        "SELECT a.id FROM a WHERE a.k IN (SELECT k FROM c)",
        "SELECT a.id FROM a WHERE a.k NOT IN (SELECT k FROM c WHERE k IS NOT NULL)",
        "SELECT a.id, b.id, c.z FROM a JOIN b ON b.k = a.k JOIN c ON c.k = b.k AND c.z > a.x",
        "SELECT count(*), sum(b.y) FROM a JOIN b ON b.k = a.k AND b.y = a.x % 7",
        "SELECT a.k, count(*) FROM a JOIN c ON c.k = a.k GROUP BY a.k",
    ];

    // The model: the inner and left joins computed from the rows, NULL joining nothing.
    let mut want_inner = Vec::new();
    let mut want_left = Vec::new();
    for &(aid, ak) in &a {
        let matches: Vec<i64> = b
            .iter()
            .filter(|(_, bk)| ak.is_some() && *bk == ak)
            .map(|(bid, _)| *bid)
            .collect();
        let int = |v: i64| Value::Numeric(Numeric::Integer(v));
        for &bid in &matches {
            want_inner.push(format!("{:?}", vec![int(aid), int(bid)]));
        }
        want_left.extend(
            matches
                .iter()
                .map(|&bid| format!("{:?}", vec![int(aid), int(bid)])),
        );
        if matches.is_empty() {
            want_left.push(format!("{:?}", vec![int(aid), Value::Null]));
        }
    }
    want_inner.sort();
    want_left.sort();
    assert!(!want_inner.is_empty(), "the model joins some rows");

    let mut wrong = Vec::new();
    for sql in queries {
        let mut answers = Vec::new();
        for methods in [
            JoinMethods::NestedLoopsOnly,
            JoinMethods::HashJoinsOnly,
            JoinMethods::ByCost,
        ] {
            conn.set_join_methods(methods);
            answers.push((methods, plan(&conn, sql), rows(&conn, sql)));
        }
        let (_, _, first) = &answers[0];
        for (methods, plan, got) in &answers[1..] {
            if got != first {
                wrong.push(format!(
                    "{sql}\n  {methods:?} ({plan}) gives {} rows, nested loops {}",
                    got.len(),
                    first.len()
                ));
            }
        }
        let model = if sql == inner {
            Some(&want_inner)
        } else if sql == left {
            Some(&want_left)
        } else {
            None
        };
        if let Some(model) = model {
            if first != model {
                wrong.push(format!(
                    "{sql}\n  gives {} rows, the model {}",
                    first.len(),
                    model.len()
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    // Non-vacuity: the switch does choose the method.
    conn.set_join_methods(JoinMethods::HashJoinsOnly);
    assert!(
        plan(&conn, inner).contains("HASH JOIN"),
        "hash joins only: {}",
        plan(&conn, inner)
    );
    conn.set_join_methods(JoinMethods::NestedLoopsOnly);
    assert!(
        !plan(&conn, inner).contains("HASH JOIN"),
        "nested loops only: {}",
        plan(&conn, inner)
    );
}

/// PL.5 (T-PL.5.1): a star join planned with hash joins only and with nested loops only, its
/// dimension filters keeping some fact rows and keeping none, and the fact joined to one filtered
/// dimension. Expect: one answer, the model's; where the fact table is probed, against a filtered
/// dimension's keys, the hash-join plan probes a Bloom filter (most probe rows miss, so the costs
/// choose one); the nested-loop plans probe none.
#[test]
fn pl_5_star_join_filters_fire_and_answers_hold() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("star.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE f (id INTEGER PRIMARY KEY, a, b, v)")
        .unwrap();
    conn.execute("CREATE TABLE d1 (id INTEGER PRIMARY KEY, cat)")
        .unwrap();
    conn.execute("CREATE TABLE d2 (id INTEGER PRIMARY KEY, cat)")
        .unwrap();
    for id in 0..100_i64 {
        conn.execute(format!("INSERT INTO d1 VALUES ({id}, {})", id % 10))
            .unwrap();
        conn.execute(format!("INSERT INTO d2 VALUES ({id}, {})", (id * 7) % 20))
            .unwrap();
    }
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut facts = Vec::new();
    conn.execute("BEGIN").unwrap();
    for id in 0..4_000_i64 {
        let a = key(&mut state).map_or(0, |k| k * 5 + id % 5);
        let b = key(&mut state).map_or(1, |k| k * 5 + id % 3);
        facts.push((a, b, id % 13));
        conn.execute(format!(
            "INSERT INTO f VALUES ({id}, {a}, {b}, {})",
            id % 13
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();

    // (d1's category, d2's categories below, whether d2 is joined)
    for (d1_cat, d2_below, star) in [
        (3_i64, 5_i64, true),
        (999, 5, true),
        (3, 20, false),
        (999, 20, false),
    ] {
        let sql = if star {
            format!(
                "SELECT count(*), sum(f.v) FROM f JOIN d1 ON f.a = d1.id JOIN d2 ON f.b = d2.id \
                 WHERE d1.cat = {d1_cat} AND d2.cat < {d2_below}"
            )
        } else {
            format!(
                "SELECT count(*), sum(f.v) FROM d1 JOIN f ON f.a = d1.id WHERE d1.cat = {d1_cat}"
            )
        };
        let kept: Vec<i64> = facts
            .iter()
            .filter(|(a, b, _)| a % 10 == d1_cat && (b * 7) % 20 < d2_below)
            .map(|(_, _, v)| *v)
            .collect();
        let int = |v: i64| Value::Numeric(Numeric::Integer(v));
        let want = vec![format!(
            "{:?}",
            vec![
                int(kept.len() as i64),
                if kept.is_empty() {
                    Value::Null
                } else {
                    int(kept.iter().sum())
                },
            ]
        )];
        for methods in [
            JoinMethods::HashJoinsOnly,
            JoinMethods::NestedLoopsOnly,
            JoinMethods::ByCost,
        ] {
            conn.set_join_methods(methods);
            assert_eq!(rows(&conn, &sql), want, "{methods:?}: {sql}");
            let mut stmt = conn.prepare(format!("EXPLAIN {sql}")).unwrap();
            let filters = stmt
                .run_collect_rows()
                .unwrap()
                .into_iter()
                .filter(|row| row[1].to_string() == "Filter")
                .count();
            match methods {
                JoinMethods::HashJoinsOnly if !star && d1_cat == 3 => {
                    assert!(filters > 0, "no filter: {}", plan(&conn, &sql))
                }
                JoinMethods::HashJoinsOnly => {}
                JoinMethods::NestedLoopsOnly => assert_eq!(filters, 0, "{}", plan(&conn, &sql)),
                JoinMethods::ByCost => {}
            }
        }
    }
}
