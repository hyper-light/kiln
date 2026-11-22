//! Each table's filters estimated together on its sample (docs/design/planner.md, decision 21;
//! PL.4): ranges, LIKE, ORs and correlated columns, where each term took a fixed selectivity and
//! the terms multiplied as if independent; and where no sample serves, the terms combined by
//! exponential backoff, so an added conjunct never raises the estimate (CERT).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("filters.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// Inserts `rows` into `table`, 500 a statement, in one transaction.
fn insert(conn: &Arc<Connection>, table: &str, rows: &[String]) {
    conn.execute("BEGIN").unwrap();
    for chunk in rows.chunks(500) {
        conn.execute(format!("INSERT INTO {table} VALUES {}", chunk.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
}

/// The rows the plan of `sql` estimates it returns, as the oracles' CERT check reads them.
fn estimate(conn: &Arc<Connection>, sql: &str) -> f64 {
    let mut stmt = conn
        .prepare(format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))
        .unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    let text = rows[0][0].to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"))
}

/// The rows `sql` returns, counted.
fn count(conn: &Arc<Connection>, sql: &str) -> f64 {
    let mut stmt = conn
        .prepare(format!("SELECT count(*) FROM ({sql})"))
        .unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    rows[0][0].as_int().unwrap() as f64
}

/// max(est/act, act/est), each at least one row (Moerkotte et al.), as the planner bench reads it.
fn q_error(estimated: f64, counted: f64) -> f64 {
    let (e, c) = (estimated.max(1.0), counted.max(1.0));
    (e / c).max(c / e)
}

/// A scatter of `i` over 32 bits, so two columns drawn from it are about independent.
fn mix(i: u64) -> u64 {
    i.wrapping_mul(2_654_435_761) & 0xffff_ffff
}

/// TPC-H's lineitem at a small scale: 20,000 rows, quantity 1 to 50, discount 0 to 0.10, ship date
/// a day of seven years (as days), a comment of words, each drawn apart.
fn lineitem(conn: &Arc<Connection>) {
    conn.execute(
        "CREATE TABLE lineitem (id INTEGER PRIMARY KEY, quantity INTEGER, discount REAL, shipdate INTEGER, comment TEXT)",
    )
    .unwrap();
    let words = [
        "final", "regular", "express", "pending", "quick", "bold", "ironic", "furious",
    ];
    let rows: Vec<String> = (0..20_000u64)
        .map(|i| {
            format!(
                "({i}, {}, {}, {}, '{} {}')",
                mix(i) % 50 + 1,
                (mix(i ^ 0x1111) % 11) as f64 / 100.0,
                mix(i ^ 0x2222) % 2555,
                words[(mix(i ^ 0x3333) % 8) as usize],
                words[(mix(i ^ 0x4444) % 8) as usize]
            )
        })
        .collect();
    insert(conn, "lineitem", &rows);
}

/// Do: ANALYZE lineitem, then estimate q06's filter: a year of ship dates, a discount BETWEEN and a
/// quantity bound, each a range a fixed selectivity took. Expect: within 2 of its count, where the
/// parent was off by more (decision 21; AC-PL.4.2).
#[test]
fn q06s_ranges_are_estimated_together_on_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    lineitem(&conn);
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT id FROM lineitem WHERE shipdate >= 730 AND shipdate < 1095 \
               AND discount BETWEEN 0.05 AND 0.07 AND quantity < 24";
    let (e, c) = (estimate(&conn, sql), count(&conn, sql));
    assert!(q_error(e, c) <= 2.0, "{sql}: estimated {e}, counted {c}");
}

/// Do: ANALYZE lineitem, then estimate a LIKE on its comment and a range beside it. Expect: within
/// 1.5 of the count: the share of the sample that passes both, where a LIKE took a fixed share
/// (decision 21; AC-PL.4.2, JOB's LIKE filters).
#[test]
fn a_like_is_estimated_on_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    lineitem(&conn);
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT id FROM lineitem WHERE comment LIKE '%express%' AND quantity <= 25";
    let (e, c) = (estimate(&conn, sql), count(&conn, sql));
    assert!(q_error(e, c) <= 1.5, "{sql}: estimated {e}, counted {c}");
}

/// Do: ANALYZE lineitem, then estimate an OR of 25 one-value BETWEENs on quantity, the odd
/// quantities, half the table: a program of 124 operations, past the 64 a term was once held to.
/// Expect: within 1.25 of its count, run on the sample. A term's length is bounded by its
/// statement, and its run by the sample's rows, never more than a scan of the table runs it
/// (decision 21, 2026-11-11; AC-PL.4.2).
#[test]
fn a_long_term_is_estimated_on_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    lineitem(&conn);
    conn.execute("ANALYZE").unwrap();
    let odd: Vec<String> = (0..25)
        .map(|k| format!("quantity BETWEEN {q} AND {q}", q = 2 * k + 1))
        .collect();
    let sql = format!("SELECT id FROM lineitem WHERE {}", odd.join(" OR "));
    let (e, c) = (estimate(&conn, &sql), count(&conn, &sql));
    assert!(q_error(e, c) <= 1.25, "estimated {e}, counted {c}");
}

/// Do: ANALYZE a table whose y equals its x, then estimate a range on each. Expect: within 1.25
/// of the half of the table counted: the sample sees the columns move together, where their
/// product took a quarter or less (decision 21; audit row 5, the independence assumption).
#[test]
fn correlated_columns_are_estimated_together() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, x INTEGER, y INTEGER)")
        .unwrap();
    let rows: Vec<String> = (0..10_000)
        .map(|i| format!("({i}, {}, {})", i % 100, i % 100))
        .collect();
    insert(&conn, "t", &rows);
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT id FROM t WHERE x < 50 AND y < 50";
    let (e, c) = (estimate(&conn, sql), count(&conn, sql));
    assert_eq!(c, 5_000.0);
    assert!(q_error(e, c) <= 1.25, "{sql}: estimated {e}, counted {c}");
}

/// Do: ANALYZE TPC-H's part at a small scale (20,000 rows: 5 brands, 8 containers, 50 sizes,
/// drawn apart), then estimate q19's OR of three conjunctions. Expect: within 2 of its count
/// (about 600 rows), where the parent estimated a few rows (decision 21; AC-PL.4.1).
#[test]
fn q19s_or_of_conjunctions_is_estimated_on_the_sample() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute(
        "CREATE TABLE part (id INTEGER PRIMARY KEY, brand TEXT, container TEXT, size INTEGER)",
    )
    .unwrap();
    let containers = [
        "SM CASE",
        "SM BOX",
        "MED BAG",
        "MED BOX",
        "LG CASE",
        "LG BOX",
        "JUMBO PKG",
        "WRAP JAR",
    ];
    let rows: Vec<String> = (0..20_000u64)
        .map(|i| {
            format!(
                "({i}, 'Brand#{}', '{}', {})",
                mix(i) % 5 + 1,
                containers[(mix(i ^ 0x5555) % 8) as usize],
                mix(i ^ 0x6666) % 50 + 1
            )
        })
        .collect();
    insert(&conn, "part", &rows);
    conn.execute("ANALYZE").unwrap();
    let sql = "SELECT id FROM part WHERE \
               (brand = 'Brand#1' AND container IN ('SM CASE', 'SM BOX') AND size BETWEEN 1 AND 5) \
               OR (brand = 'Brand#2' AND container IN ('MED BAG', 'MED BOX') AND size BETWEEN 1 AND 10) \
               OR (brand = 'Brand#3' AND container IN ('LG CASE', 'LG BOX') AND size BETWEEN 1 AND 15)";
    let (e, c) = (estimate(&conn, sql), count(&conn, sql));
    assert!(c > 400.0 && c < 800.0, "{sql}: counted {c}");
    assert!(q_error(e, c) <= 2.0, "{sql}: estimated {e}, counted {c}");
}

/// A small generator of its own (xorshift64*), so a failing case is the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) % n
    }
}

/// Do: random conjunctions of lineitem's filters (ranges, equalities, BETWEEN, IN lists, LIKE,
/// ORs, a parameter), with ANALYZE's sample and without statistics, each planned and then
/// planned again with one more conjunct. Expect: the added conjunct never raises the estimate
/// (CERT, Ba and Rigger, ICSE 2024), and it lowers it in many cases, so the check is not vacuous
/// (decision 21; T-PL.4.1).
#[test]
fn a_filter_made_more_restrictive_never_raises_its_estimate() {
    let mut lowered = 0;
    let mut checked = 0;
    for analyze in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(dir.path());
        lineitem(&conn);
        if analyze {
            conn.execute("ANALYZE").unwrap();
        }
        let mut r = Rng(0x5EED_F17E_25AA_0001 ^ u64::from(analyze));
        let term = |r: &mut Rng| match r.below(9) {
            0 => format!("quantity < {}", r.below(52)),
            1 => format!("quantity >= {}", r.below(52)),
            2 => format!("shipdate BETWEEN {} AND {}", r.below(2555), r.below(2555)),
            3 => format!("discount = {}", r.below(11) as f64 / 100.0),
            4 => format!(
                "quantity IN ({}, {}, {})",
                r.below(50),
                r.below(50),
                r.below(50)
            ),
            5 => [
                "comment LIKE '%final%'",
                "comment LIKE 'quick%'",
                "comment NOT LIKE '%bold%'",
            ][r.below(3) as usize]
                .to_owned(),
            6 => format!("(quantity < {} OR discount > 0.08)", r.below(52)),
            7 => "quantity > ?".to_owned(),
            _ => format!("abs(quantity - {}) < 5", r.below(50)),
        };
        for _ in 0..200 {
            let mut terms: Vec<String> = (0..r.below(4)).map(|_| term(&mut r)).collect();
            let sql = |terms: &[String]| {
                let wh = if terms.is_empty() {
                    String::new()
                } else {
                    format!(" WHERE {}", terms.join(" AND "))
                };
                format!("SELECT id FROM lineitem{wh}")
            };
            let before = estimate(&conn, &sql(&terms));
            terms.push(term(&mut r));
            let restricted = sql(&terms);
            let after = estimate(&conn, &restricted);
            assert!(
                after <= before * (1.0 + 1e-9),
                "analyze {analyze}: {restricted} estimated {after}, {before} without its last term"
            );
            checked += 1;
            if after < before * (1.0 - 1e-9) {
                lowered += 1;
            }
        }
    }
    assert_eq!(checked, 400);
    assert!(lowered > 200, "only {lowered} of {checked} lowered");
}

/// Do: ANALYZE small tables, then estimate joins whose filter stands in an inner join's ON and,
/// again, in WHERE, as the oracles' order check states them (seed 11's findings at a6a5fb0: 0.25
/// rows against 0.005, 1,000 against 200). Expect: one estimate for both, the ON term run on the
/// sample as the WHERE term is (decisions 13 and 21).
#[test]
fn an_inner_joins_on_term_and_a_where_term_give_one_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE t1 (c0 NUMERIC)").unwrap();
    conn.execute("INSERT INTO t1 VALUES (7), (3), (5), (9)")
        .unwrap();
    lineitem(&conn);
    conn.execute("ANALYZE").unwrap();
    for (on, wh) in [
        (
            "SELECT 1 FROM t1 AS a0 JOIN t1 AS a1 ON (3 BETWEEN a0.c0 AND a0.c0)",
            "SELECT 1 FROM t1 AS a0, t1 AS a1 WHERE (3 BETWEEN a0.c0 AND a0.c0)",
        ),
        (
            "SELECT 1 FROM t1 AS a0, lineitem AS l JOIN t1 AS a2 ON (l.comment GLOB '*final*')",
            "SELECT 1 FROM t1 AS a0, lineitem AS l, t1 AS a2 WHERE (l.comment GLOB '*final*')",
        ),
    ] {
        let (a, b) = (estimate(&conn, on), estimate(&conn, wh));
        assert!(
            (a - b).abs() <= 1e-9 * a.abs().max(b.abs()),
            "{on}: {a}; {wh}: {b}"
        );
    }
}

/// Do: ANALYZE, then estimate outer joins whose right table has a sampled ON term and a WHERE term
/// of its own, and the inner joins they restrict to, as CERT states them (seed 11's finding at
/// a6a5fb0 with the ON terms sampled apart: the inner join at 1 row, the RIGHT JOIN at 0.967).
/// Expect: no inner join estimated above its outer join, the ON and WHERE terms read from the
/// sample together in both (decision 21; T-PL.4.1).
#[test]
fn an_outer_join_with_sampled_terms_is_estimated_at_least_its_inner_join() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute("CREATE TABLE t0 (c0 REAL, c1 INT NOT NULL)")
        .unwrap();
    conn.execute("INSERT INTO t0 VALUES (NULL, -2.5), (9007199254740993, 2)")
        .unwrap();
    lineitem(&conn);
    conn.execute("ANALYZE").unwrap();
    for (from, on, wh) in [
        (
            "t0 AS a0 {} t0 AS a1",
            "NOT (a1.c0 GLOB 'a%')",
            "a1.c1 >= 3",
        ),
        (
            "t0 AS a0 {} lineitem AS a1",
            "a1.comment LIKE '%final%'",
            "a1.quantity < 20",
        ),
        (
            "t0 AS a0 {} lineitem AS a1",
            "a1.quantity > 10 OR a1.discount < 0.02",
            "a1.quantity < 30",
        ),
    ] {
        let sql = |join: &str| {
            format!(
                "SELECT 1 FROM {} ON ({on}) WHERE ({wh})",
                from.replace("{}", join)
            )
        };
        let inner = estimate(&conn, &sql("JOIN"));
        for outer in ["LEFT JOIN", "RIGHT JOIN", "FULL JOIN"] {
            let e = estimate(&conn, &sql(outer));
            assert!(
                inner <= e * (1.0 + 1e-9),
                "{}: {e}, below its inner join's {inner}",
                sql(outer)
            );
        }
    }
}
