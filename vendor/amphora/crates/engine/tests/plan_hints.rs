//! G6 step 3's tests (T-G6.3.1; `optimizer::plan_hints`): every hint set the planner can obey
//! gives the answer the unhinted plan gives, and the answer a nested loop computed here from the
//! rows gives, on joins with NULL and duplicate keys, inner, left, semi and anti, over indexed
//! and unindexed columns, with ORDER BY and GROUP BY. The answers against SQLite's own run in the
//! harness (`bench/plan_hints`, its `--tiny` run and the coordinator's JOB and corpus runs).

use amphora_engine::{
    candidate_plans, ConnectOpts, Connection, Database, DatabaseOpts, HintSet, Numeric,
    OpenOptions, PlatformIO, SqliteDialect, Value,
};
use std::sync::Arc;

const ROWS: usize = 240;

/// A key from 0 to 14, or NULL about one row in eleven: keys repeat, and NULLs join nothing.
fn key(state: &mut u64) -> Option<i64> {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    let draw = (*state % 165) as i64;
    if draw < 150 {
        Some(draw % 15)
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

struct Fixture {
    conn: Arc<Connection>,
    a: Vec<(i64, Option<i64>)>,
    b: Vec<(i64, Option<i64>)>,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.path().join("hints.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, k, x)")
        .unwrap();
    conn.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, k, y)")
        .unwrap();
    conn.execute("CREATE INDEX b_k ON b (k)").unwrap();
    conn.execute("CREATE TABLE c (k, z)").unwrap();
    conn.execute("CREATE INDEX c_z ON c (z)").unwrap();
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for id in 1..=ROWS as i64 {
        let (ak, bk, ck) = (key(&mut state), key(&mut state), key(&mut state));
        a.push((id, ak));
        b.push((id, bk));
        conn.execute(format!(
            "INSERT INTO a VALUES ({id}, {}, {})",
            sql_value(ak),
            (id * 37) % 100
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
    Fixture {
        conn,
        a,
        b,
        _dir: dir,
    }
}

const INNER: &str = "SELECT a.id, b.id FROM a JOIN b ON b.k = a.k";
const LEFT: &str = "SELECT a.id, b.id FROM a LEFT JOIN b ON b.k = a.k";
const QUERIES: [&str; 12] = [
    INNER,
    LEFT,
    "SELECT a.id, c.z FROM a JOIN c ON c.k = a.k WHERE a.x < 50",
    "SELECT a.id, c.z FROM a LEFT JOIN c ON c.k = a.k AND c.z > 40",
    "SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
    "SELECT a.id FROM a WHERE NOT EXISTS (SELECT 1 FROM c WHERE c.k = a.k)",
    "SELECT a.id FROM a WHERE a.k IN (SELECT k FROM c)",
    "SELECT a.id, b.id, c.z FROM a JOIN b ON b.k = a.k JOIN c ON c.k = b.k AND c.z > a.x",
    "SELECT count(*), sum(b.y) FROM a JOIN b ON b.k = a.k AND b.y = a.x % 7",
    "SELECT a.k, count(*) FROM a JOIN c ON c.k = a.k GROUP BY a.k",
    "SELECT c.z, a.id FROM c JOIN a ON a.k = c.k WHERE c.z BETWEEN 10 AND 60 ORDER BY c.z, a.id",
    "SELECT b.y, count(DISTINCT a.k) FROM a JOIN b ON b.k = a.k GROUP BY b.y ORDER BY b.y",
];

/// T-G6.3.1: do run each join under every hint set the planner can obey. Expect one answer: the
/// unhinted plan's, and the nested loop's computed from the rows where the test has one.
#[test]
fn t_g6_3_1_every_hint_set_gives_the_unhinted_answer() {
    let Fixture { conn, a, b, _dir } = fixture();

    let int = |v: i64| Value::Numeric(Numeric::Integer(v));
    let (mut want_inner, mut want_left) = (Vec::new(), Vec::new());
    for &(aid, ak) in &a {
        let matches: Vec<i64> = b
            .iter()
            .filter(|(_, bk)| ak.is_some() && *bk == ak)
            .map(|(bid, _)| *bid)
            .collect();
        for &bid in &matches {
            want_inner.push(format!("{:?}", vec![int(aid), int(bid)]));
            want_left.push(format!("{:?}", vec![int(aid), int(bid)]));
        }
        if matches.is_empty() {
            want_left.push(format!("{:?}", vec![int(aid), Value::Null]));
        }
    }
    want_inner.sort();
    want_left.sort();
    assert!(!want_inner.is_empty(), "the model joins some rows");

    let mut wrong = Vec::new();
    let mut plans_seen = std::collections::BTreeSet::new();
    for sql in QUERIES {
        HintSet::UNHINTED.apply(&conn);
        let unhinted = rows(&conn, sql);
        let model = match sql {
            INNER => Some(&want_inner),
            LEFT => Some(&want_left),
            _ => None,
        };
        if let Some(model) = model {
            if &unhinted != model {
                wrong.push(format!(
                    "{sql}\n  unhinted gives {} rows, the model {}",
                    unhinted.len(),
                    model.len()
                ));
            }
        }
        for hint in HintSet::ARMS {
            hint.apply(&conn);
            let shown = plan(&conn, sql);
            let got = rows(&conn, sql);
            plans_seen.insert((sql, shown.clone()));
            if got != unhinted {
                wrong.push(format!(
                    "{sql}\n  {} ({shown}) gives {} rows, unhinted {}",
                    hint.name(),
                    got.len(),
                    unhinted.len()
                ));
            }
        }
    }
    HintSet::UNHINTED.apply(&conn);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Non-vacuity: the hint sets gave more plans than the queries, so answers were compared
    // across plans that differ.
    assert!(
        plans_seen.len() > QUERIES.len(),
        "{} plans for {} queries",
        plans_seen.len(),
        QUERIES.len()
    );
}

/// T-G6.3.1: do read each hint set's plan of a join. Expect the planner to obey the mask: a hash
/// join where only hash joins are allowed, none where only nested loops are, and the features to
/// say so; and the connection left unhinted.
#[test]
fn t_g6_3_1_the_planner_obeys_each_mask() {
    let Fixture { conn, .. } = fixture();
    let plans = candidate_plans(&conn, INNER).unwrap();
    assert_eq!(plans.len(), HintSet::ARMS.len());
    // Feature 7 counts hash joins (`PlanFeatures`).
    let hash_joins = |hint: HintSet| {
        plans
            .iter()
            .find(|(h, _)| *h == hint)
            .map(|(_, f)| f.0[7])
            .unwrap()
    };
    let hash_only = HintSet::ARMS[6];
    let loops_only = HintSet::ARMS[8];
    assert_eq!(hash_only.name(), "hash");
    assert_eq!(loops_only.name(), "loop");
    assert!(hash_joins(hash_only) >= 1.0, "{plans:?}");
    assert_eq!(hash_joins(loops_only), 0.0, "{plans:?}");
    // The connection is unhinted after: its plan is the unhinted mask's.
    let unhinted_plan = plan(&conn, INNER);
    HintSet::UNHINTED.apply(&conn);
    assert_eq!(plan(&conn, INNER), unhinted_plan);
    // A statement not prepared as EXPLAIN QUERY PLAN has no plan nodes: refused.
    let stmt = conn.prepare(INNER).unwrap();
    assert!(amphora_engine::plan_features(&stmt).is_err());
}

/// T-G6.3.1: do turn automatic indexes off. Expect no automatic index in a plan that builds one
/// with them on, and the same answer.
#[test]
fn t_g6_3_1_automatic_indexes_obey_their_switch() {
    let Fixture { conn, .. } = fixture();
    let sql = "SELECT a.id, c.z FROM a JOIN c ON c.k = a.k WHERE a.x < 50";
    // Hash joins off, so an unindexed join column takes an automatic index or a plain loop.
    let with = HintSet::ARMS[0];
    conn.set_join_methods(amphora_engine::JoinMethods::ByCost);
    with.apply(&conn);
    let on_rows = rows(&conn, sql);
    conn.set_automatic_indexes(false);
    let off_plan = plan(&conn, sql);
    assert!(!off_plan.contains("AUTOMATIC"), "{off_plan}");
    assert_eq!(rows(&conn, sql), on_rows);
    conn.set_automatic_indexes(true);
}
