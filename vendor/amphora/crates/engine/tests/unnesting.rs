//! T-PL.10.1: every correlated subquery answers the same nested and unnested. Each query runs three
//! ways on one small TPC-H-shaped database with NULLs, duplicates and empty matches: kept nested
//! (`SubqueryUnnestingMode::Disabled`, SQLite's way of running it: once for each outer row), every
//! rewrite that applies taken (`Forced`), and as the planner chooses (`Auto`). The answers must be
//! the same rows. The classes are the corpus's (bench/planner/audit/07-unnesting.sql) and TPC-H's
//! correlated queries (Q2, Q4, Q17, Q20, Q21, Q22) at a small scale, plus the forms PL.10 adds:
//! NOT IN, subqueries of several tables, EXISTS under OR and in the SELECT list, and subqueries
//! nested in subqueries down to a depth the worklist takes level by level.
//!
//! Non-vacuity: the classes marked to unnest must change their plan under `Forced`, counted.
//!
//! The unnesting mode is the engine's simulator control, so this file needs that feature:
//! `cargo test -p amphora-engine --features simulator --test unnesting`.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
    SubqueryUnnestingMode, Value,
};
use std::sync::Arc;

fn open(dir: &std::path::Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("unnest.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// A fixed sequence of numbers, the same on every run (a linear congruential generator, Knuth's
/// MMIX constants), so a failure reproduces.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) % below
    }
}

/// `v` as SQL, or NULL one time in `null_one_in`.
fn maybe_null(numbers: &mut Numbers, v: u64, null_one_in: u64) -> String {
    if numbers.next(null_one_in) == 0 {
        "NULL".to_string()
    } else {
        v.to_string()
    }
}

fn load(conn: &Arc<Connection>) {
    for sql in [
        "CREATE TABLE nation (n_nationkey INTEGER PRIMARY KEY, n_name TEXT NOT NULL)",
        "CREATE TABLE supplier (s_suppkey INTEGER PRIMARY KEY, s_name TEXT NOT NULL, s_nationkey INTEGER NOT NULL)",
        "CREATE TABLE customer (c_custkey INTEGER PRIMARY KEY, c_nationkey INTEGER NOT NULL, c_phone TEXT NOT NULL, c_acctbal REAL NOT NULL, c_ref INTEGER)",
        "CREATE TABLE orders (o_orderkey INTEGER PRIMARY KEY, o_custkey INTEGER NOT NULL, o_orderstatus TEXT NOT NULL, o_totalprice REAL NOT NULL, o_orderdate TEXT NOT NULL, o_ref INTEGER)",
        "CREATE TABLE lineitem (l_orderkey INTEGER NOT NULL, l_linenumber INTEGER NOT NULL, l_partkey INTEGER NOT NULL, l_suppkey INTEGER NOT NULL, l_quantity REAL NOT NULL, l_commitdate TEXT NOT NULL, l_receiptdate TEXT NOT NULL, PRIMARY KEY (l_orderkey, l_linenumber))",
        "CREATE TABLE part (p_partkey INTEGER PRIMARY KEY, p_brand TEXT NOT NULL, p_size INTEGER NOT NULL, p_name TEXT NOT NULL)",
        "CREATE TABLE partsupp (ps_partkey INTEGER NOT NULL, ps_suppkey INTEGER NOT NULL, ps_availqty INTEGER NOT NULL, ps_supplycost REAL NOT NULL, PRIMARY KEY (ps_partkey, ps_suppkey))",
        // Text keys of two collations and loose types, for the forms that group or compare keys.
        "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT COLLATE NOCASE, num)",
        "CREATE TABLE labels (id INTEGER PRIMARY KEY, name TEXT, num INTEGER)",
        "CREATE INDEX orders_custkey ON orders (o_custkey)",
        "CREATE INDEX lineitem_suppkey ON lineitem (l_suppkey)",
        "CREATE INDEX lineitem_partkey ON lineitem (l_partkey)",
        "CREATE INDEX supplier_nationkey ON supplier (s_nationkey)",
    ] {
        conn.execute(sql).unwrap();
    }
    let mut numbers = Numbers(0x5eed);
    conn.execute("BEGIN").unwrap();
    for n in 0..5 {
        let name = ["ALGERIA", "BRAZIL", "CANADA", "EGYPT", "SAUDI ARABIA"][n];
        conn.execute(format!("INSERT INTO nation VALUES ({n}, '{name}')"))
            .unwrap();
    }
    for s in 1..=20 {
        let nation = numbers.next(5);
        conn.execute(format!(
            "INSERT INTO supplier VALUES ({s}, 'Supplier#{s:03}', {nation})"
        ))
        .unwrap();
    }
    for c in 1..=60 {
        let nation = numbers.next(5);
        let phone = 10 + numbers.next(25);
        let bal = numbers.next(2000) as f64 - 500.0;
        let reference = maybe_null(&mut numbers, numbers_value(c), 4);
        conn.execute(format!(
            "INSERT INTO customer VALUES ({c}, {nation}, '{phone}-555', {bal}, {reference})"
        ))
        .unwrap();
    }
    let mut line_rows = 0;
    for o in 1..=240 {
        // Customers past 50 have no orders: NOT EXISTS and NOT IN keep them.
        let customer = 1 + numbers.next(50);
        let status = ["F", "O", "P"][numbers.next(3) as usize];
        let price = numbers.next(10_000) as f64 / 10.0;
        let date = format!(
            "199{}-0{}-1{}",
            3 + numbers.next(4),
            1 + numbers.next(9),
            numbers.next(10)
        );
        let reference = numbers.next(70);
        let reference = maybe_null(&mut numbers, reference, 5);
        conn.execute(format!(
            "INSERT INTO orders VALUES ({o}, {customer}, '{status}', {price}, '{date}', {reference})"
        ))
        .unwrap();
        // Some orders have no lines; most have one to six.
        for line in 1..=numbers.next(7) {
            let part = 1 + numbers.next(40);
            let supplier = 1 + numbers.next(20);
            let quantity = 1 + numbers.next(50);
            let commit = 10 + numbers.next(20);
            let receipt = 10 + numbers.next(20);
            conn.execute(format!(
                "INSERT INTO lineitem VALUES ({o}, {line}, {part}, {supplier}, {quantity}, '1995-01-{commit}', '1995-01-{receipt}')"
            ))
            .unwrap();
            line_rows += 1;
        }
    }
    assert!(line_rows > 500, "{line_rows} lines");
    for p in 1..=40 {
        let brand = format!("Brand#{}", 1 + numbers.next(5));
        let size = 1 + numbers.next(50);
        let name =
            ["forest green", "lace blue", "forest red", "misty rose"][numbers.next(4) as usize];
        conn.execute(format!(
            "INSERT INTO part VALUES ({p}, '{brand}', {size}, '{name}')"
        ))
        .unwrap();
        for s in 1..=20 {
            if numbers.next(5) == 0 {
                let qty = numbers.next(100);
                let cost = numbers.next(1000) as f64 / 10.0;
                conn.execute(format!(
                    "INSERT INTO partsupp VALUES ({p}, {s}, {qty}, {cost})"
                ))
                .unwrap();
            }
        }
    }
    let words = [
        "alpha", "Alpha", "ALPHA", "beta", "Beta", "gamma", "1", "01",
    ];
    for id in 1..=30 {
        let tag = words[numbers.next(words.len() as u64) as usize];
        let label = words[numbers.next(words.len() as u64) as usize];
        let tag_num = ["1", "'1'", "1.0", "NULL", "2"][numbers.next(5) as usize];
        let label_num = numbers.next(3);
        let label_num = maybe_null(&mut numbers, label_num, 4);
        conn.execute(format!(
            "INSERT INTO tags VALUES ({id}, '{tag}', {tag_num})"
        ))
        .unwrap();
        conn.execute(format!(
            "INSERT INTO labels VALUES ({id}, '{label}', {label_num})"
        ))
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("ANALYZE").unwrap();
}

fn numbers_value(c: u64) -> u64 {
    (c * 7) % 70
}

/// One query of a class, and whether the class is one the rewrites take.
struct Case {
    class: &'static str,
    sql: &'static str,
    unnests: bool,
}

const CASES: &[Case] = &[
    // The fixed patterns.
    Case { class: "exists, one table", unnests: true,
        sql: "SELECT o_orderkey FROM orders WHERE EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate)" },
    Case { class: "not exists, one table", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)" },
    Case { class: "in, correlated", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_nationkey IN (SELECT s_nationkey FROM supplier WHERE s_suppkey = c_custkey)" },
    Case { class: "scalar count", unnests: true,
        sql: "SELECT c_custkey, (SELECT count(*) FROM orders WHERE o_custkey = c_custkey) FROM customer" },
    Case { class: "scalar avg in where", unnests: true,
        sql: "SELECT p_partkey FROM part WHERE p_size < (SELECT avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)" },
    Case { class: "scalar sum, join-first", unnests: true,
        sql: "SELECT o_orderkey FROM orders WHERE o_totalprice / 10 > (SELECT sum(l_quantity) FROM lineitem WHERE l_orderkey = o_orderkey)" },
    Case { class: "exists and not exists by <> (q21's)", unnests: true,
        sql: "SELECT l1.l_orderkey, l1.l_linenumber FROM lineitem l1 JOIN orders ON o_orderkey = l1.l_orderkey WHERE EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey) AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate)" },
    // NOT IN: NULL on either side, on both, on neither, and empty lists.
    Case { class: "not in, nullable both sides", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_ref NOT IN (SELECT o_ref FROM orders WHERE o_custkey = c_custkey)" },
    Case { class: "not in, nullable left only", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_ref NOT IN (SELECT o_orderkey FROM orders WHERE o_custkey = c_custkey)" },
    Case { class: "not in, not null", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_nationkey NOT IN (SELECT s_nationkey FROM supplier WHERE s_suppkey = c_custkey)" },
    Case { class: "not in, loose types", unnests: false,
        sql: "SELECT id FROM tags WHERE num NOT IN (SELECT num FROM labels WHERE labels.id = tags.id)" },
    Case { class: "not in, collated", unnests: false,
        sql: "SELECT id FROM tags WHERE name NOT IN (SELECT name FROM labels WHERE labels.id <= tags.id)" },
    // Several tables: semi- and anti-joins with a derived table.
    Case { class: "exists, two tables", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders JOIN lineitem ON l_orderkey = o_orderkey WHERE o_custkey = c_custkey AND l_quantity > 45)" },
    Case { class: "not exists, two tables", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE NOT EXISTS (SELECT * FROM orders, lineitem WHERE l_orderkey = o_orderkey AND o_custkey = c_custkey AND l_quantity > 45)" },
    Case { class: "in, two tables", unnests: true,
        sql: "SELECT s_suppkey FROM supplier WHERE s_nationkey IN (SELECT c_nationkey FROM customer JOIN orders ON o_custkey = c_custkey WHERE o_orderkey = s_suppkey * 3)" },
    Case { class: "not in, two tables, nullable", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_ref NOT IN (SELECT o_ref FROM orders JOIN lineitem ON l_orderkey = o_orderkey WHERE o_custkey = c_custkey AND l_quantity > 25)" },
    Case { class: "exists, distinct and order by", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT DISTINCT o_orderstatus FROM orders WHERE o_custkey = c_custkey AND o_totalprice > 500 ORDER BY 1)" },
    Case { class: "exists, two tables, range correlation", unnests: true,
        sql: "SELECT o_orderkey FROM orders WHERE EXISTS (SELECT * FROM lineitem JOIN part ON p_partkey = l_partkey WHERE l_orderkey = o_orderkey AND p_size > o_totalprice / 50)" },
    // Mark joins: EXISTS where no semi-join can take it.
    Case { class: "exists under or", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_acctbal > 1200 OR EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND o_orderstatus = 'F')" },
    Case { class: "not exists under or", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE c_acctbal > 1200 OR NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND o_orderstatus = 'P')" },
    Case { class: "two exists under or, several tables (TPC-DS Q10, Q35)", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders, lineitem WHERE l_orderkey = o_orderkey AND o_custkey = c_custkey AND l_quantity > 48) OR EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND o_totalprice > 990)" },
    Case { class: "exists in the select list", unnests: true,
        sql: "SELECT c_custkey, EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND o_orderstatus = 'O') FROM customer" },
    Case { class: "exists in an aggregate", unnests: true,
        sql: "SELECT c_nationkey, count(*), sum(EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)) FROM customer GROUP BY c_nationkey" },
    Case { class: "exists under or, collated key", unnests: false,
        sql: "SELECT id FROM tags WHERE id > 25 OR EXISTS (SELECT * FROM labels WHERE labels.name = tags.name)" },
    Case { class: "exists under or, loose types", unnests: false,
        sql: "SELECT id FROM labels WHERE id > 25 OR EXISTS (SELECT * FROM tags WHERE tags.num = labels.num)" },
    // Outer joins around and inside.
    Case { class: "exists reading a left join's null side", unnests: false,
        sql: "SELECT c_custkey, o_orderkey FROM customer LEFT JOIN orders ON o_custkey = c_custkey AND o_orderstatus = 'F' WHERE EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey)" },
    Case { class: "not exists beside a left join", unnests: true,
        sql: "SELECT c_custkey, o_orderkey FROM customer LEFT JOIN orders ON o_custkey = c_custkey AND o_orderstatus = 'F' WHERE NOT EXISTS (SELECT * FROM supplier WHERE s_suppkey = c_custkey)" },
    Case { class: "exists with a left join inside", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders LEFT JOIN lineitem ON l_orderkey = o_orderkey AND l_quantity > 40 WHERE o_custkey = c_custkey AND l_orderkey IS NULL)" },
    Case { class: "exists beside a right join", unnests: false,
        sql: "SELECT count(*) FROM orders RIGHT JOIN customer ON c_custkey = o_custkey WHERE EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate)" },
    // Nested: a subquery's own correlated subquery, top-down.
    Case { class: "exists in exists, each on its parent", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_quantity > 40))" },
    Case { class: "not exists in exists, reading both levels", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND NOT EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_suppkey = c_nationkey + 1))" },
    Case { class: "scalar in exists", unnests: true,
        sql: "SELECT c_custkey FROM customer WHERE EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey AND o_totalprice > (SELECT avg(l_quantity) * 20 FROM lineitem WHERE l_orderkey = o_orderkey))" },
    Case { class: "exists in a two-table exists", unnests: true,
        sql: "SELECT n_nationkey FROM nation WHERE EXISTS (SELECT * FROM supplier JOIN partsupp ON ps_suppkey = s_suppkey WHERE s_nationkey = n_nationkey AND EXISTS (SELECT * FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey))" },
    // TPC-H's correlated queries, on this schema.
    Case { class: "TPC-H Q2", unnests: true,
        sql: "SELECT s_name, p_partkey FROM part, supplier, partsupp, nation WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND p_size > 10 AND s_nationkey = n_nationkey AND ps_supplycost = (SELECT min(ps_supplycost) FROM partsupp, supplier, nation WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND s_nationkey = n_nationkey AND n_name <> 'BRAZIL') ORDER BY s_name, p_partkey" },
    Case { class: "TPC-H Q4", unnests: true,
        sql: "SELECT o_orderstatus, count(*) FROM orders WHERE o_orderdate >= '1994-01-01' AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate) GROUP BY o_orderstatus ORDER BY o_orderstatus" },
    Case { class: "TPC-H Q17", unnests: true,
        sql: "SELECT sum(l_quantity) / 7.0 FROM lineitem, part WHERE p_partkey = l_partkey AND p_brand = 'Brand#3' AND l_quantity < (SELECT 0.8 * avg(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)" },
    // Not unnested yet: its scalar `0.5 * total(..)` is 0.0 over no rows, an expression of an
    // aggregate that isn't NULL on empty input, which `result_on_empty_input` doesn't take (only a
    // bare aggregate, or one NULL on empty input). Its answers are still checked.
    Case { class: "TPC-H Q20", unnests: false,
        sql: "SELECT s_name FROM supplier, nation WHERE s_suppkey IN (SELECT ps_suppkey FROM partsupp WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name >= 'forest' AND p_name < 'foresu') AND ps_availqty > (SELECT 0.5 * total(l_quantity) FROM lineitem WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey)) AND s_nationkey = n_nationkey AND n_name <> 'EGYPT' ORDER BY s_name" },
    Case { class: "TPC-H Q21", unnests: true,
        sql: "SELECT s_name, count(*) AS numwait FROM supplier, lineitem l1, orders, nation WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey AND o_orderstatus = 'F' AND l1.l_receiptdate > l1.l_commitdate AND EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey) AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate) AND s_nationkey = n_nationkey AND n_name = 'SAUDI ARABIA' GROUP BY s_name ORDER BY numwait DESC, s_name" },
    Case { class: "TPC-H Q22", unnests: true,
        sql: "SELECT cntrycode, count(*), sum(c_acctbal) FROM (SELECT substr(c_phone, 1, 2) AS cntrycode, c_acctbal FROM customer WHERE c_acctbal > (SELECT avg(c_acctbal) FROM customer WHERE c_acctbal > 0.00) AND NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)) AS custsale GROUP BY cntrycode ORDER BY cntrycode" },
];

/// The rows `sql` returns under `mode`, in a canonical order: the tests compare multisets.
fn rows(conn: &Arc<Connection>, mode: SubqueryUnnestingMode, sql: &str) -> Vec<String> {
    conn.set_subquery_unnesting_mode(mode);
    let mut stmt = conn
        .prepare(sql)
        .unwrap_or_else(|e| panic!("{mode:?} {sql}: {e}"));
    let rows = stmt
        .run_collect_rows()
        .unwrap_or_else(|e| panic!("{mode:?} {sql}: {e}"));
    let mut rows: Vec<String> = rows.iter().map(|row| row_text(row)).collect();
    rows.sort();
    rows
}

fn row_text(row: &[Value]) -> String {
    row.iter()
        .map(|value| format!("{value:?}"))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn plan(conn: &Arc<Connection>, mode: SubqueryUnnestingMode, sql: &str) -> String {
    conn.set_subquery_unnesting_mode(mode);
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    rows.iter()
        .map(|row| row_text(row))
        .collect::<Vec<_>>()
        .join("\n")
}

/// T-PL.10.1. Do: each class's query nested, unnested and as planned. Expect: the same rows each
/// way, and every class marked to unnest planned otherwise when the rewrites are forced.
#[test]
fn t_pl10_1_unnested_answers_equal_nested_answers() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn);
    let mut unnested = 0;
    let mut failures = Vec::new();
    for case in CASES {
        let nested = rows(&conn, SubqueryUnnestingMode::Disabled, case.sql);
        let forced = rows(&conn, SubqueryUnnestingMode::Forced, case.sql);
        let auto = rows(&conn, SubqueryUnnestingMode::Auto, case.sql);
        if forced != nested || auto != nested {
            failures.push(format!(
                "{}: nested {} rows, forced {} rows, auto {} rows\n{}\nnested plan:\n{}\nforced plan:\n{}",
                case.class,
                nested.len(),
                forced.len(),
                auto.len(),
                case.sql,
                plan(&conn, SubqueryUnnestingMode::Disabled, case.sql),
                plan(&conn, SubqueryUnnestingMode::Forced, case.sql),
            ));
        }
        let changed = plan(&conn, SubqueryUnnestingMode::Disabled, case.sql)
            != plan(&conn, SubqueryUnnestingMode::Forced, case.sql);
        if changed {
            unnested += 1;
        }
        if case.unnests && !changed {
            failures.push(format!(
                "{}: not unnested\n{}\nplan:\n{}",
                case.class,
                case.sql,
                plan(&conn, SubqueryUnnestingMode::Forced, case.sql)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    let marked = CASES.iter().filter(|case| case.unnests).count();
    assert!(unnested >= marked, "{unnested} unnested, {marked} marked");
}

/// The query `EXISTS` nested `depth` deep, each level correlated with its parent and the first.
fn nested_exists(depth: usize) -> String {
    let mut sql = String::from("SELECT c_custkey FROM customer c0 WHERE ");
    for level in 1..=depth {
        let parent = level - 1;
        sql.push_str(&format!(
            "EXISTS (SELECT * FROM orders c{level} WHERE c{level}.o_custkey = c{parent}.{} AND c{level}.o_orderkey > c0.c_custkey AND ",
            if parent == 0 { "c_custkey" } else { "o_custkey" }
        ));
    }
    sql.push('1');
    for _ in 0..depth {
        sql.push(')');
    }
    sql
}

/// T-PL.10.1, depth. Do: EXISTS nested 1 to 24 deep, every level correlated with its parent and the
/// outermost query. Expect: the same rows nested, forced and as planned, and every level unnested:
/// the worklist takes each level's subquery in its turn as the splice above brings it up, with no
/// subquery left in the forced plan.
#[test]
fn t_pl10_1_nested_exists_unnest_level_by_level() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    load(&conn);
    for depth in 1..=24 {
        let sql = nested_exists(depth);
        let nested = rows(&conn, SubqueryUnnestingMode::Disabled, &sql);
        let forced = rows(&conn, SubqueryUnnestingMode::Forced, &sql);
        let auto = rows(&conn, SubqueryUnnestingMode::Auto, &sql);
        assert_eq!(forced, nested, "depth {depth}: {sql}");
        assert_eq!(auto, nested, "depth {depth}: {sql}");
        let forced_plan = plan(&conn, SubqueryUnnestingMode::Forced, &sql);
        assert!(
            !forced_plan.contains("SUBQUERY"),
            "depth {depth} keeps a subquery:\n{forced_plan}"
        );
    }
}
