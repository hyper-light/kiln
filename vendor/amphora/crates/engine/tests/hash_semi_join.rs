//! G6 step 5's planned test (plans that read less: hash semi joins for EXISTS): a differential of
//! the changed plans. Random tables with NULL and duplicate keys, filtered by EXISTS on one key and
//! two, with extra correlated terms, beside inner joins and NOT EXISTS, are planned with nested
//! loops only, hash joins only (`Connection::set_join_methods`) and by cost, with a page cache large
//! enough to hold the hash table and one small enough to spill it; the answers must be identical
//! and equal a semi join computed here from the rows themselves.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, JoinMethods, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::sync::Arc;

const ROWS: i64 = 1_500;

/// A key from 0 to 39, or NULL about one row in ten: every key repeats, and NULLs match nothing.
fn key(state: &mut u64) -> Option<i64> {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    let draw = (*state % 440) as i64;
    if draw < 400 {
        Some(draw % 40)
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

fn bytecode(conn: &Arc<Connection>, sql: &str) -> String {
    let mut stmt = conn.prepare(format!("EXPLAIN {sql}")).unwrap();
    stmt.run_collect_rows()
        .unwrap()
        .into_iter()
        .map(|row| row[1].to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

struct Row {
    id: i64,
    k: Option<i64>,
    m: Option<i64>,
    v: i64,
}

/// Do: run each EXISTS query under every join method, with the hash table in memory and spilled.
/// Expect: one answer, the model's; hash joins only plans the EXISTS as a hash semi join.
#[test]
fn g6_s5_hash_semi_join_matches_nested_loops_and_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("semi.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    // `s` has no index on its keys, so a nested loop scans it for each row of `a`.
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, k, m, v)")
        .unwrap();
    conn.execute("CREATE TABLE s (id INTEGER PRIMARY KEY, k, m, v)")
        .unwrap();
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, k, w)")
        .unwrap();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut a = Vec::new();
    let mut s = Vec::new();
    conn.execute("BEGIN").unwrap();
    for id in 1..=ROWS {
        let row_a = Row {
            id,
            k: key(&mut state),
            m: key(&mut state).map(|m| m % 3),
            v: (id * 37) % 100,
        };
        let row_s = Row {
            id,
            k: key(&mut state),
            m: key(&mut state).map(|m| m % 3),
            v: (id * 53) % 100,
        };
        conn.execute(format!(
            "INSERT INTO a VALUES ({id}, {}, {}, {})",
            sql_value(row_a.k),
            sql_value(row_a.m),
            row_a.v
        ))
        .unwrap();
        conn.execute(format!(
            "INSERT INTO s VALUES ({id}, {}, {}, {})",
            sql_value(row_s.k),
            sql_value(row_s.m),
            row_s.v
        ))
        .unwrap();
        if id % 5 == 0 {
            conn.execute(format!(
                "INSERT INTO b VALUES ({id}, {}, {})",
                sql_value(key(&mut state)),
                id % 11
            ))
            .unwrap();
        }
        a.push(row_a);
        s.push(row_s);
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();

    let one_key = "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k)";
    let two_keys =
        "SELECT a.id, a.v FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k AND s.m = a.m)";
    let correlated_range = "SELECT a.id FROM a WHERE a.v < 70 AND EXISTS (SELECT 1 FROM s WHERE s.k = a.k AND s.v > a.v)";
    let queries = [
        one_key,
        two_keys,
        correlated_range,
        "SELECT count(*), sum(a.v) FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k AND s.v < 10)",
        "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k AND s.m = 1)",
        "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k) AND NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        "SELECT a.k, count(*) FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k AND s.m = a.m) GROUP BY a.k",
        "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM s WHERE s.k = a.k) ORDER BY a.v DESC, a.id LIMIT 25",
    ];

    // The model: each row of `a` once when some row of `s` matches it, NULL matching nothing.
    let int = |v: i64| Value::Numeric(Numeric::Integer(v));
    let model = |keep: &dyn Fn(&Row, &Row) -> bool, out: &dyn Fn(&Row) -> Vec<Value>| {
        let mut want: Vec<String> = a
            .iter()
            .filter(|ra| s.iter().any(|rs| keep(ra, rs)))
            .map(|ra| format!("{:?}", out(ra)))
            .collect();
        want.sort();
        want
    };
    let same = |x: Option<i64>, y: Option<i64>| x.is_some() && x == y;
    let want_one_key = model(&|ra, rs| same(ra.k, rs.k), &|ra| vec![int(ra.id)]);
    let want_two_keys = model(&|ra, rs| same(ra.k, rs.k) && same(ra.m, rs.m), &|ra| {
        vec![int(ra.id), int(ra.v)]
    });
    let want_correlated_range = model(
        &|ra, rs| ra.v < 70 && same(ra.k, rs.k) && rs.v > ra.v,
        &|ra| vec![int(ra.id)],
    );
    assert!(
        !want_one_key.is_empty() && want_one_key.len() < a.len(),
        "the model keeps some rows and drops some"
    );

    let mut wrong = Vec::new();
    // The default page cache holds the hash table; eight pages do not, so it spills to grace
    // partitions and the matched bits live in them.
    for cache in [None, Some(8)] {
        if let Some(pages) = cache {
            conn.execute(format!("PRAGMA cache_size = {pages}"))
                .unwrap();
        }
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
                        "cache {cache:?}: {sql}\n  {methods:?} ({plan}) gives {} rows, nested loops {}",
                        got.len(),
                        first.len()
                    ));
                }
            }
            let model = if sql == one_key {
                Some(&want_one_key)
            } else if sql == two_keys {
                Some(&want_two_keys)
            } else if sql == correlated_range {
                Some(&want_correlated_range)
            } else {
                None
            };
            if let Some(model) = model {
                if first != model {
                    wrong.push(format!(
                        "cache {cache:?}: {sql}\n  gives {} rows, the model {}",
                        first.len(),
                        model.len()
                    ));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));

    // Non-vacuity: hash joins only plans the EXISTS as a hash semi join, which tests each build
    // row's first match; nested loops only does not.
    conn.set_join_methods(JoinMethods::HashJoinsOnly);
    assert!(
        plan(&conn, one_key).contains("HASH JOIN"),
        "hash joins only: {}",
        plan(&conn, one_key)
    );
    assert!(
        bytecode(&conn, one_key).contains("HashMarkFirstMatch"),
        "hash joins only: {}",
        bytecode(&conn, one_key)
    );
    conn.set_join_methods(JoinMethods::NestedLoopsOnly);
    assert!(
        !bytecode(&conn, one_key).contains("HashMarkFirstMatch"),
        "nested loops only: {}",
        bytecode(&conn, one_key)
    );
}
