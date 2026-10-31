//! A GROUP BY whose rows don't arrive in group order aggregates each row into its group in a hash
//! table, the rows of groups past the table's budget going to the sorter as before, and the two
//! merged in group order (`docs/design/executor.md`, decision 4). Every answer is the one sorting
//! every row gave (AC-T1.1.1: reads as committed; AC-T1.13.4: reads at their latency). The debug
//! build's budget is 32 KiB, so the larger tables here overflow into the sorter.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, Numeric, OpenOptions, PlatformIO,
    SqliteDialect, Value,
};
use std::collections::BTreeMap;
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("g.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.run_collect_rows().unwrap()
}

fn plan(conn: &Arc<Connection>, sql: &str) -> String {
    rows(conn, &format!("EXPLAIN QUERY PLAN {sql}"))
        .iter()
        .map(|r| format!("{:?}", r.last().unwrap()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Null => None,
        Value::Numeric(Numeric::Integer(i)) => Some(*i),
        other => panic!("not an integer: {other:?}"),
    }
}

fn real(v: &Value) -> f64 {
    match v {
        Value::Numeric(Numeric::Integer(i)) => *i as f64,
        Value::Numeric(Numeric::Float(f)) => f64::from(*f),
        other => panic!("not a number: {other:?}"),
    }
}

/// The rows of `t`: (id, g, h, v, w), g NULL one row in 50, v NULL one in 20.
struct Row {
    g: Option<i64>,
    h: String,
    v: Option<i64>,
    w: f64,
}

/// `n` rows with `groups` values of g, inserted into `t`, and the rows themselves.
fn table(conn: &Arc<Connection>, n: i64, groups: i64, seed: u64) -> Vec<Row> {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INTEGER, h TEXT, v INTEGER, w REAL)")
        .unwrap();
    let words = ["Ant", "ant", "ANT", "bee", "Bee", "cat", "dog", "Dog"];
    let mut x = seed;
    let mut next = || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 33
    };
    let mut out = Vec::new();
    conn.execute("BEGIN").unwrap();
    for id in 1..=n {
        let g = (next() % 50 != 0).then(|| (next() % groups as u64) as i64);
        let h = words[(next() % words.len() as u64) as usize].to_string();
        let v = (next() % 20 != 0).then(|| (next() % 100) as i64);
        let w = (next() % 1000) as f64 / 8.0;
        let sql_g = g.map_or_else(|| "NULL".to_string(), |g| g.to_string());
        let sql_v = v.map_or_else(|| "NULL".to_string(), |v| v.to_string());
        conn.execute(format!(
            "INSERT INTO t VALUES ({id}, {sql_g}, '{h}', {sql_v}, {w})"
        ))
        .unwrap();
        out.push(Row { g, h, v, w });
    }
    conn.execute("COMMIT").unwrap();
    out
}

#[derive(Default, Debug, PartialEq)]
struct Acc {
    count: i64,
    count_v: i64,
    sum_v: Option<i64>,
    min_v: Option<i64>,
    max_v: Option<i64>,
    total_w: f64,
    big_v: i64,
}

fn oracle(rows: &[Row]) -> BTreeMap<Option<i64>, Acc> {
    let mut groups: BTreeMap<Option<i64>, Acc> = BTreeMap::new();
    for r in rows {
        let a = groups.entry(r.g).or_default();
        a.count += 1;
        a.total_w += r.w;
        if let Some(v) = r.v {
            a.count_v += 1;
            a.sum_v = Some(a.sum_v.unwrap_or(0) + v);
            a.min_v = Some(a.min_v.map_or(v, |m| m.min(v)));
            a.max_v = Some(a.max_v.map_or(v, |m| m.max(v)));
            if v > 50 {
                a.big_v += 1;
            }
        }
    }
    groups
}

const AGGREGATES: &str = "SELECT g, COUNT(*), COUNT(v), SUM(v), MIN(v), MAX(v), TOTAL(w), COUNT(*) FILTER (WHERE v > 50) FROM t GROUP BY g";

fn check_aggregates(got: &[Vec<Value>], want: &BTreeMap<Option<i64>, Acc>) {
    assert_eq!(got.len(), want.len(), "one row a group");
    for (row, (key, acc)) in got.iter().zip(want) {
        assert_eq!(int(&row[0]), *key, "groups in key order, NULL first");
        let got_acc = Acc {
            count: int(&row[1]).unwrap(),
            count_v: int(&row[2]).unwrap(),
            sum_v: int(&row[3]),
            min_v: int(&row[4]),
            max_v: int(&row[5]),
            total_w: real(&row[6]),
            big_v: int(&row[7]).unwrap(),
        };
        assert_eq!(
            Acc {
                total_w: 0.0,
                ..got_acc
            },
            Acc {
                total_w: 0.0,
                count: acc.count,
                count_v: acc.count_v,
                sum_v: acc.sum_v,
                min_v: acc.min_v,
                max_v: acc.max_v,
                big_v: acc.big_v,
            },
            "group {key:?}"
        );
        assert!(
            (got_acc.total_w - acc.total_w).abs() < 1e-6,
            "group {key:?}"
        );
    }
}

/// Do: GROUP BY a column no index orders, over 2,000 rows in 40 groups (the table holds them
/// all), and over 6,000 rows in 5,000 groups (past the debug budget: most rows go to the sorter).
/// Expect: the hash table in the plan, and every group's COUNT, SUM, MIN, MAX, TOTAL and FILTERed
/// COUNT as an oracle counts them, the groups in key order, NULL first, whichever way each went.
#[test]
fn hashed_groups_match_an_oracle_within_and_past_the_budget() {
    for (n, groups, seed) in [(2_000, 40, 1), (6_000, 5_000, 2)] {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path());
        let data = table(&conn, n, groups, seed);
        assert!(plan(&conn, AGGREGATES).contains("USE HASH TABLE FOR GROUP BY"));
        check_aggregates(&rows(&conn, AGGREGATES), &oracle(&data));
    }
}

/// Do: GROUP BY g with ORDER BY g DESC (the sorter's order, so no second sort), and with ORDER
/// BY COUNT(*) DESC, g LIMIT 7 after a HAVING, past the budget. Expect: the groups in descending
/// key order; the seven largest groups of more than five rows, ties by key.
#[test]
fn hashed_groups_keep_the_order_and_limits_a_sorter_gives() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let data = table(&conn, 6_000, 3_000, 3);
    let want = oracle(&data);
    let desc = rows(
        &conn,
        "SELECT g, COUNT(*) FROM t GROUP BY g ORDER BY g DESC",
    );
    let keys: Vec<Option<i64>> = desc.iter().map(|r| int(&r[0])).collect();
    let mut want_keys: Vec<Option<i64>> = want.keys().copied().collect();
    want_keys.reverse();
    assert_eq!(keys, want_keys);
    let top = rows(
        &conn,
        "SELECT g, COUNT(*) c FROM t GROUP BY g HAVING c > 5 ORDER BY c DESC, g LIMIT 7",
    );
    let mut ranked: Vec<(i64, Option<i64>)> = want
        .iter()
        .filter(|(_, a)| a.count > 5)
        .map(|(k, a)| (-a.count, *k))
        .collect();
    ranked.sort();
    let want_top: Vec<(Option<i64>, i64)> = ranked.iter().take(7).map(|(c, k)| (*k, -c)).collect();
    let got_top: Vec<(Option<i64>, i64)> = top
        .iter()
        .map(|r| (int(&r[0]), int(&r[1]).unwrap()))
        .collect();
    assert_eq!(got_top, want_top);
    let first = rows(&conn, "SELECT g FROM t GROUP BY g LIMIT 10");
    let first: Vec<Option<i64>> = first.iter().map(|r| int(&r[0])).collect();
    assert_eq!(first, want.keys().copied().take(10).collect::<Vec<_>>());
}

/// Do: GROUP BY two columns; GROUP BY text under NOCASE; GROUP BY a column holding 1 and 1.0.
/// Expect: one group a distinct pair; one group a word whatever its case; 1 and 1.0 one group, as
/// a sorter compares them.
#[test]
fn hashed_groups_compare_keys_as_the_sorter_does() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let data = table(&conn, 3_000, 12, 4);
    let mut pairs: BTreeMap<(Option<i64>, String), i64> = BTreeMap::new();
    for r in &data {
        *pairs.entry((r.g, r.h.clone())).or_default() += 1;
    }
    let got = rows(&conn, "SELECT g, h, COUNT(*) FROM t GROUP BY g, h");
    let got: BTreeMap<(Option<i64>, String), i64> = got
        .iter()
        .map(|r| {
            let Value::Text(h) = &r[1] else {
                panic!("{:?}", r[1])
            };
            ((int(&r[0]), h.as_str().to_string()), int(&r[2]).unwrap())
        })
        .collect();
    assert_eq!(got, pairs);
    let mut words: BTreeMap<String, i64> = BTreeMap::new();
    for r in &data {
        *words.entry(r.h.to_lowercase()).or_default() += 1;
    }
    let got = rows(
        &conn,
        "SELECT lower(h), COUNT(*) FROM t GROUP BY h COLLATE NOCASE",
    );
    let got: BTreeMap<String, i64> = got
        .iter()
        .map(|r| {
            let Value::Text(h) = &r[0] else {
                panic!("{:?}", r[0])
            };
            (h.as_str().to_string(), int(&r[1]).unwrap())
        })
        .collect();
    assert_eq!(got, words);
    conn.execute("CREATE TABLE m (x)").unwrap();
    conn.execute("INSERT INTO m VALUES (1), (1.0), (2), (2.5), (1)")
        .unwrap();
    let got = rows(&conn, "SELECT x, COUNT(*) FROM m GROUP BY x");
    let counts: Vec<i64> = got.iter().map(|r| int(&r[1]).unwrap()).collect();
    assert_eq!(counts, vec![3, 1, 1]);
}

/// Do: a correlated subquery whose GROUP BY runs once for each outer row, its LIMIT 1 leaving
/// groups undrained. Expect: each run answers from its own rows alone: the smallest v of its g.
#[test]
fn each_run_of_a_hashed_group_by_starts_with_no_groups() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let data = table(&conn, 2_000, 30, 5);
    let got = rows(
        &conn,
        "SELECT o.g, (SELECT i.v FROM t i WHERE i.g = o.g AND i.v IS NOT NULL GROUP BY i.v ORDER BY i.v LIMIT 1) FROM (SELECT DISTINCT g FROM t WHERE g IS NOT NULL) o ORDER BY o.g",
    );
    let mut least: BTreeMap<i64, i64> = BTreeMap::new();
    for r in &data {
        if let (Some(g), Some(v)) = (r.g, r.v) {
            let e = least.entry(g).or_insert(v);
            *e = (*e).min(v);
        }
    }
    let got: BTreeMap<i64, i64> = got
        .iter()
        .map(|r| (int(&r[0]).unwrap(), int(&r[1]).unwrap()))
        .collect();
    assert_eq!(got, least);
}

/// Do: ask for the plans of GROUP BYs a hash table may and may not serve. Expect: the hash table
/// for COUNT, SUM, AVG, MIN and MAX; the sorter for group_concat (a state that grows with its
/// group), for COUNT(DISTINCT ...), and for a bare column beside MAX.
#[test]
fn only_groups_with_small_states_are_hashed() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    table(&conn, 100, 5, 6);
    for sql in [
        "SELECT g, COUNT(*), SUM(v), AVG(w), MIN(v), MAX(v) FROM t GROUP BY g",
        "SELECT g, h, COUNT(*) FROM t GROUP BY g, h",
    ] {
        assert!(
            plan(&conn, sql).contains("USE HASH TABLE FOR GROUP BY"),
            "{sql}"
        );
    }
    for sql in [
        "SELECT g, group_concat(h) FROM t GROUP BY g",
        "SELECT g, COUNT(DISTINCT v) FROM t GROUP BY g",
        "SELECT g, h, MAX(v) FROM t GROUP BY g",
    ] {
        assert!(
            plan(&conn, sql).contains("USE SORTER FOR GROUP BY"),
            "{sql}"
        );
    }
}
