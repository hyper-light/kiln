//! The planner's choices on the shapes Amphora's bench suite (`bench --suite`) found it getting
//! wrong, each a statement of the suite on a smaller copy of its tables (docs/design/planner.md):
//! a join from a key range looks each row's owner up by key instead of probing the range's table
//! once for every owner, and a sort by the key reads in key order instead of sorting a covering
//! index (AC-T1.1.2, AC-T1.13.4: reads at their latency).

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlatformIO, SqliteDialect,
};
use std::path::Path;
use std::sync::Arc;

const OWNERS: i64 = 400;
const ITEMS: i64 = 20_000;

/// A scatter of `id` over 32 bits, so an owner's items lie all over the table as the suite's do.
fn mix(id: i64) -> i64 {
    id.wrapping_mul(2_654_435_761) & 0xffff_ffff
}

/// The suite's tables, indexes and statistics, with a fiftieth of its rows.
fn tables(dir: &Path) -> Arc<Connection> {
    suite_tables(dir, OWNERS)
}

/// The suite's tables, indexes and statistics, with `owners` owners and a fiftieth of its items.
fn suite_tables(dir: &Path, owners: i64) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("suite.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    conn.execute("CREATE TABLE regions (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    conn.execute("CREATE TABLE owners (id INTEGER PRIMARY KEY, name TEXT NOT NULL, region INTEGER NOT NULL, score REAL NOT NULL)").unwrap();
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, owner INTEGER NOT NULL, price REAL NOT NULL, created INTEGER NOT NULL, name TEXT NOT NULL)").unwrap();
    conn.execute("BEGIN").unwrap();
    let regions: Vec<String> = (1..=20)
        .map(|id| format!("({id}, 'region-{id}')"))
        .collect();
    conn.execute(format!("INSERT INTO regions VALUES {}", regions.join(", ")))
        .unwrap();
    let rows: Vec<String> = (1..=owners)
        .map(|id| format!("({id}, 'owner-{id}', {}, {}.5)", id % 20 + 1, id % 97))
        .collect();
    conn.execute(format!("INSERT INTO owners VALUES {}", rows.join(", ")))
        .unwrap();
    for first in (1..=ITEMS).step_by(500) {
        let rows: Vec<String> = (first..first + 500)
            .map(|id| {
                format!(
                    "({id}, {}, {}, {}.25, {}, 'name-{:08x}')",
                    mix(id) % 100,
                    mix(id ^ 0x5555) % owners + 1,
                    mix(id) % 10_000,
                    1_700_000_000 + id * 60,
                    mix(id)
                )
            })
            .collect();
        conn.execute(format!("INSERT INTO items VALUES {}", rows.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    for index in [
        "CREATE INDEX items_name ON items (name)",
        "CREATE INDEX items_grp_created ON items (grp, created)",
        "CREATE INDEX items_owner ON items (owner)",
        "CREATE INDEX owners_region ON owners (region)",
        "ANALYZE",
    ] {
        conn.execute(index).unwrap();
    }
    conn
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

/// Do: join items to their owners for 20 consecutive ids (the suite's join_pk). Expect: the id
/// range is read and each row's owner looked up by key, not every owner's items probed and
/// filtered to the range (400 index searches and 20,000 index rows for 20 results).
#[test]
fn a_join_from_a_key_range_looks_each_owner_up_by_key() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path());
    let steps = plan(&conn, "SELECT i.id, i.price, o.name FROM items i JOIN owners o ON o.id = i.owner WHERE i.id BETWEEN ? AND ?");
    assert_eq!(steps.len(), 2, "{steps:?}");
    assert!(
        steps[0].starts_with("SEARCH i USING INTEGER PRIMARY KEY (rowid>"),
        "{steps:?}"
    );
    assert_eq!(
        steps[1], "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)",
        "{steps:?}"
    );
}

/// Do: join three tables by key from an id range and group (the suite's join_three). Expect: the
/// range first, then a key lookup into each of the other two.
#[test]
fn three_tables_joined_by_key_start_from_the_key_range() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path());
    let steps = plan(&conn, "SELECT r.name, COUNT(*), AVG(i.price) FROM items i JOIN owners o ON o.id = i.owner JOIN regions r ON r.id = o.region WHERE i.id BETWEEN ? AND ? GROUP BY r.name");
    assert!(
        steps[0].starts_with("SEARCH i USING INTEGER PRIMARY KEY (rowid>"),
        "{steps:?}"
    );
    assert_eq!(
        steps[1], "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)",
        "{steps:?}"
    );
    assert_eq!(
        steps[2], "SEARCH r USING INTEGER PRIMARY KEY (rowid=?)",
        "{steps:?}"
    );
}

/// Do: page through the table by key, 20 rows past an offset, either way (the suite's
/// offset_deep). Expect: the table read in key order, which stops after offset + 20 rows, never a
/// covering index read whole and sorted.
#[test]
fn a_page_by_key_reads_in_key_order_without_sorting() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path());
    for order in ["ASC", "DESC"] {
        let steps = plan(
            &conn,
            &format!("SELECT id, name FROM items ORDER BY id {order} LIMIT 20 OFFSET ?"),
        );
        assert_eq!(steps, vec!["SCAN items".to_owned()], "ORDER BY id {order}");
    }
}

/// Do: GROUP BY the first column of an index that lacks the aggregated columns, over the whole
/// table, with the connection's cache a fraction of the table and then holding all of it: with
/// aggregates a hash table keeps, and with a COUNT(DISTINCT), which a sort groups. Expect: the
/// hashed GROUP BY scans the table and hashes its 100 groups whatever the cache holds (a probe a
/// row costs less than the index order's seek a row, cached or not; docs/design/planner.md,
/// decision 7); the sorted one scans and sorts while each lookup would read a page of its own
/// (decision 5), and takes the index's order once the cache holds the table.
#[test]
fn a_whole_table_group_by_scans_and_sorts_where_lookups_would_read_pages() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path());
    let hashed = "SELECT grp, COUNT(*), AVG(price) FROM items GROUP BY grp";
    let sorted = "SELECT grp, COUNT(DISTINCT owner) FROM items GROUP BY grp";
    let scan_and_hash = vec![
        "SCAN items".to_owned(),
        "USE HASH TABLE FOR GROUP BY".to_owned(),
    ];
    // The COUNT(DISTINCT) keeps its own hash table of values either way; what changes is how its
    // groups come: scanned and sorted, or in the index's order.
    let sorted_plan = |conn: &Arc<Connection>| {
        let steps = plan(conn, sorted);
        let sorter = steps.iter().any(|s| s == "USE SORTER FOR GROUP BY");
        (steps.first().cloned().unwrap_or_default(), sorter)
    };
    conn.execute("PRAGMA cache_size = 200").unwrap();
    assert_eq!(plan(&conn, hashed), scan_and_hash);
    assert_eq!(sorted_plan(&conn), ("SCAN items".to_owned(), true));
    conn.execute("PRAGMA cache_size = 100000").unwrap();
    assert_eq!(plan(&conn, hashed), scan_and_hash);
    assert_eq!(
        sorted_plan(&conn),
        ("SCAN items USING INDEX items_grp_created".to_owned(), false)
    );
}

/// Do: join a grouped CTE (the sums of a few hundred owners' items) to the owners it names,
/// written as a CTE and as a subquery in FROM. Expect: its rows drive, each owner looked up by
/// key, rather than every owner scanned to probe an index built on it (docs/design/planner.md,
/// decision 6: the owners' pages fit in the cache, so after one search a search of them reads
/// nothing); and the rows the same join without a subquery returns.
#[test]
fn a_grouped_cte_drives_lookups_of_the_owners_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let conn = tables(dir.path());
    let grouped = "SELECT owner, SUM(price) AS s FROM items WHERE grp = 7 GROUP BY owner";
    let forms = [
        format!("WITH t AS ({grouped}) SELECT o.name, t.s FROM t JOIN owners o ON o.id = t.owner ORDER BY t.s DESC, o.name LIMIT 5"),
        format!("SELECT o.name, t.s FROM ({grouped}) t JOIN owners o ON o.id = t.owner ORDER BY t.s DESC, o.name LIMIT 5"),
    ];
    let joined = "SELECT o.name, SUM(i.price) FROM items i JOIN owners o ON o.id = i.owner WHERE i.grp = 7 GROUP BY o.id ORDER BY 2 DESC, o.name LIMIT 5";
    let mut want = conn.prepare(joined).unwrap();
    let want: Vec<String> = want
        .run_collect_rows()
        .unwrap()
        .iter()
        .map(|row| format!("{} {}", row[0], row[1]))
        .collect();
    assert_eq!(want.len(), 5);
    for sql in &forms {
        assert_eq!(
            plan(&conn, sql),
            vec![
                "SCAN t".to_owned(),
                "SEARCH items USING INDEX items_grp_created (grp=?)".to_owned(),
                "USE HASH TABLE FOR GROUP BY".to_owned(),
                "SEARCH o USING INTEGER PRIMARY KEY (rowid=?)".to_owned(),
                "USE SORTER FOR ORDER BY".to_owned(),
            ],
            "{sql}"
        );
        let mut stmt = conn.prepare(sql).unwrap();
        let got: Vec<String> = stmt
            .run_collect_rows()
            .unwrap()
            .iter()
            .map(|row| format!("{} {}", row[0], row[1]))
            .collect();
        assert_eq!(got, want, "{sql}");
    }
}

/// Do: count a region's owners beside a correlated count of its group's items, grouped by the
/// region (the suite's group_correlated) and ungrouped, with the suite's 4,000 owners, 200 a
/// region. Expect: the correlated form, its subquery a search of the group's items run once for
/// the output (docs/design/planner.md), not the unnested form's scan of every item grouped by grp
/// (the call priced once an input row, 200 times, cost more than the scan); and the counts read
/// directly (AC-T1.8.6, AC-T1.13.1).
#[test]
fn a_correlated_subquery_a_grouped_output_reads_is_planned_once_a_group() {
    let dir = tempfile::tempdir().unwrap();
    let conn = suite_tables(dir.path(), 4_000);
    let rows = |sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.run_collect_rows()
            .unwrap()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    };
    let owners = rows("SELECT count(*) FROM owners WHERE region = 5");
    let items = rows("SELECT count(*) FROM items WHERE grp = 5");
    assert_eq!(owners, ["200"]);
    let subquery = "(SELECT count(*) FROM items i WHERE i.grp = o.region)";
    for (sql, want) in [
        (
            format!(
                "SELECT o.region, count(*), {subquery} FROM owners o WHERE o.region = 5 GROUP BY o.region"
            ),
            format!("5 {} {}", owners[0], items[0]),
        ),
        (
            format!("SELECT count(*), {subquery} FROM owners o WHERE o.region = 5"),
            format!("{} {}", owners[0], items[0]),
        ),
    ] {
        let steps = plan(&conn, &sql);
        assert!(
            steps
                .iter()
                .any(|s| s.starts_with("CORRELATED SCALAR SUBQUERY")),
            "{sql}: {steps:?}"
        );
        assert!(
            steps.iter().all(|s| !s.contains("scalar_subquery")),
            "{sql}: {steps:?}"
        );
        assert_eq!(rows(&sql), [want], "{sql}");
    }
}

/// The rows the plan of `sql` estimates it returns: the plan's own `output_rows` (the join's), as
/// the oracles' CERT and order checks read them.
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

/// Whether two estimates agree to within 1e-9 of the larger (the oracles' order check).
fn same_estimate(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs())
}

/// A small generator of its own (xorshift64*), so a failing case is the same on every run.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % n
    }
}

/// Six tables of three integer columns, some indexed, a few hundred rows each.
fn order_tables(dir: &Path, analyze: bool) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("order.db").to_str().unwrap(), options).unwrap();
    let conn = db.connect(ConnectOpts::new()).unwrap();
    for t in 0..6 {
        let key = if t % 2 == 0 {
            "INTEGER PRIMARY KEY"
        } else {
            "INTEGER"
        };
        conn.execute(format!("CREATE TABLE t{t} (a {key}, b INTEGER, c INTEGER)"))
            .unwrap();
        let rows: Vec<String> = (1..=100 * (t + 1))
            .map(|i| format!("({i}, {}, {})", i % (7 + t), i % 13))
            .collect();
        conn.execute(format!("INSERT INTO t{t} VALUES {}", rows.join(", ")))
            .unwrap();
        if t % 3 != 2 {
            conn.execute(format!("CREATE INDEX t{t}_b ON t{t} (b)"))
                .unwrap();
        }
        if t == 1 {
            conn.execute("CREATE INDEX t1_bc ON t1 (b, c)").unwrap();
        }
    }
    if analyze {
        conn.execute("ANALYZE").unwrap();
    }
    conn
}

/// Every order of `from`'s tables, the identity first.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    let mut perm: Vec<usize> = (0..n).collect();
    let mut c = vec![0usize; n];
    let mut all = vec![perm.clone()];
    let mut i = 1;
    while i < n {
        if c[i] < i {
            if i % 2 == 0 {
                perm.swap(0, i);
            } else {
                perm.swap(c[i], i);
            }
            all.push(perm.clone());
            c[i] += 1;
            i = 1;
        } else {
            c[i] = 0;
            i += 1;
        }
    }
    all
}

/// Do: random inner joins of two to six tables (a table may come twice) with random join terms and
/// filters (equalities and ranges between tables, constants compared by `=`, `<`, `>`, BETWEEN, IS
/// NULL and IN), with statistics and without, each planned with its FROM list in every order (up to
/// 24 orders of the six-table ones) and each order forced by CROSS JOIN. Expect: one estimate for
/// all of them, within 1e-9 of the larger (decision 13; AC-T1.1.2: the planner's estimates, which
/// the reads' plans rest on).
#[test]
fn every_order_of_an_inner_join_gets_one_estimate() {
    for analyze in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = order_tables(dir.path(), analyze);
        let mut r = Rng(0x9E37_79B9_7F4A_7C15 ^ u64::from(analyze));
        let mut checked = 0;
        for _ in 0..60 {
            let n = 2 + r.below(5);
            let from: Vec<String> = (0..n).map(|i| format!("t{} AS x{i}", r.below(6))).collect();
            let col = |r: &mut Rng, i: usize| format!("x{i}.{}", ["a", "b", "c"][r.below(3)]);
            let mut terms = Vec::new();
            for i in 1..n {
                let j = r.below(i);
                let op = ["=", "=", "=", "<", ">="][r.below(5)];
                terms.push(format!("{} {op} {}", col(&mut r, i), col(&mut r, j)));
            }
            for _ in 0..r.below(4) {
                let i = r.below(n);
                let c = col(&mut r, i);
                terms.push(match r.below(6) {
                    0 => format!("{c} = {}", r.below(20)),
                    1 => format!("{c} < {}", r.below(20)),
                    2 => format!("{c} > {} AND {c} < {}", r.below(10), 10 + r.below(10)),
                    3 => format!("{c} BETWEEN {} AND {}", r.below(10), 10 + r.below(10)),
                    4 => format!("{c} IS NULL"),
                    _ => format!("{c} IN ({}, {})", r.below(20), r.below(20)),
                });
            }
            let wh = terms.join(" AND ");
            let base = format!("SELECT x0.a FROM {} WHERE {wh}", from.join(", "));
            let want = estimate(&conn, &base);
            let mut orders = permutations(n);
            // 720 orders of six tables: every fourth of five's 120 and every 30th of six's
            let step = match n {
                5 => 4,
                6 => 30,
                _ => 1,
            };
            orders = orders.into_iter().step_by(step).collect();
            for order in orders {
                let listed: Vec<&str> = order.iter().map(|&k| from[k].as_str()).collect();
                for join in [", ", " CROSS JOIN "] {
                    let sql = format!("SELECT x0.a FROM {} WHERE {wh}", listed.join(join));
                    let got = estimate(&conn, &sql);
                    assert!(
                        same_estimate(got, want),
                        "analyze {analyze}: {sql}\n  estimated {got}, as written {want}: {base}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 600, "{checked} orders checked");
    }
}

/// Do: the same two tables and ON terms joined INNER, LEFT, RIGHT and FULL (a BETWEEN of the right
/// table's column among them, its bounds a constant and the left table's column), with statistics
/// and without, and EXISTS and NOT EXISTS over the right table. Expect: LEFT and RIGHT never below
/// INNER, FULL never below LEFT or RIGHT (it keeps the unmatched rows of both sides), and a semi or
/// anti join never above the rows of its left side (decision 13; AC-T1.1.2).
#[test]
fn an_outer_join_is_estimated_at_least_its_inner_join() {
    for analyze in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = order_tables(dir.path(), analyze);
        for on in [
            "x.b = y.b",
            "x.a = y.a",
            "x.b = y.b AND y.c < 5",
            "x.c = y.a AND x.b > 3",
            "x.b < y.b",
            "y.c = 3",
            "y.b BETWEEN 2 AND x.c",
            "y.b BETWEEN NULL AND x.b",
            "y.a BETWEEN x.c AND 9",
            "y.b BETWEEN NULL AND x.c",
            "(x.c <= x.c) AND (y.b BETWEEN x.c AND 3)",
            "(x.b <= y.b) AND (y.b ISNULL)",
            "1",
        ] {
            for (left, right) in [("t0", "t1"), ("t1", "t2"), ("t3", "t0"), ("t2", "t5")] {
                let join = |kind: &str| {
                    estimate(
                        &conn,
                        &format!(
                            "SELECT x.a, y.a FROM {left} AS x {kind} JOIN {right} AS y ON {on}"
                        ),
                    )
                };
                let (inner, l, r, full) = (join(""), join("LEFT"), join("RIGHT"), join("FULL"));
                let what = format!("analyze {analyze}, {left} and {right} ON {on}: inner {inner}, left {l}, right {r}, full {full}");
                assert!(l >= inner * (1.0 - 1e-9), "LEFT below INNER: {what}");
                assert!(r >= inner * (1.0 - 1e-9), "RIGHT below INNER: {what}");
                assert!(full >= l * (1.0 - 1e-9), "FULL below LEFT: {what}");
                assert!(full >= r * (1.0 - 1e-9), "FULL below RIGHT: {what}");
                let all = estimate(&conn, &format!("SELECT x.a FROM {left} AS x"));
                for not in ["", "NOT "] {
                    let sql = format!("SELECT x.a FROM {left} AS x WHERE {not}EXISTS (SELECT 1 FROM {right} AS y WHERE {on})");
                    let semi = estimate(&conn, &sql);
                    assert!(
                        semi <= all * (1.0 + 1e-9),
                        "analyze {analyze}: {sql} estimated {semi}, its table {all}"
                    );
                }
            }
        }
    }
}

/// Do: the same constraints used by an index seek, and with NOT INDEXED as filters: equalities,
/// a closed range, a range one bound of which can't key the seek, and a join's equality on the
/// searched table's indexed column. Expect: the same output_rows each way (decision 13: an access
/// method changes only the cost; AC-T1.1.2).
#[test]
fn a_constraint_seeks_or_filters_to_one_estimate() {
    for analyze in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = order_tables(dir.path(), analyze);
        for (from, wh) in [
            ("t0 AS y{h}", "y.b = 3"),
            ("t0 AS y{h}", "y.b > 2 AND y.b < 5"),
            ("t1 AS y{h}", "y.b = 3 AND y.c > 4"),
            ("t1 AS y{h}", "y.b BETWEEN 2 AND 5"),
            (
                "t3 AS x, t0 AS y{h}",
                "y.b >= x.c AND y.b <= length(y.c + x.a)",
            ),
            ("t3 AS x, t0 AS y{h}", "y.b = x.c"),
            ("t3 AS x, t1 AS y{h}", "y.b = x.c AND y.c = x.b"),
            ("t3 AS x, t0 AS y{h}", "y.a = x.b AND y.c < 9"),
        ] {
            let sql =
                |hint: &str| format!("SELECT y.a FROM {} WHERE {wh}", from.replace("{h}", hint));
            let (seek, filter) = (sql(""), sql(" NOT INDEXED"));
            let (s, f) = (estimate(&conn, &seek), estimate(&conn, &filter));
            assert!(
                same_estimate(s, f),
                "analyze {analyze}: {seek} estimated {s}, {filter} {f}"
            );
        }
    }
}

/// Do: an inner join with its condition in ON, the same condition in WHERE, and the tables listed
/// the other way round: a BETWEEN, an EXISTS and a NOT EXISTS correlated with the right table, two
/// scalar subqueries (planned correlated in some orders and unnested to a grouped table in others), a
/// NULL, a NULL test of a literal that SQLite's parser folds to 0, alone, ANDed with NULL and
/// ORed with 0, a bound by a NULL literal, and NULL ANDed with an EXISTS.
/// Expect: one estimate for each condition's forms, and 0 rows where no row can pass (decision 13:
/// the oracles' order check found each of these estimated apart; AC-T1.1.2).
#[test]
fn an_on_term_and_a_where_term_give_one_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let conn = order_tables(dir.path(), false);
    for (cond, rows) in [
        ("x.b BETWEEN 2 AND y.c", None),
        ("0.1 BETWEEN x.c AND NULL", None),
        ("EXISTS (SELECT 1 FROM t2 AS s WHERE s.c != y.b)", None),
        (
            "NOT EXISTS (SELECT 1 FROM t2 AS s WHERE s.c IS NOT DISTINCT FROM y.a)",
            None,
        ),
        (
            "(SELECT count(s.c) FROM t2 AS s WHERE s.b = y.c) NOT BETWEEN x.b AND 3",
            None,
        ),
        ("NOT (SELECT min(s.c) FROM t2 AS s WHERE s.b = x.a)", None),
        (
            "(SELECT max(s.c) FROM t2 AS s WHERE s.b = x.a) IS NOT ('-1' <= x.c)",
            None,
        ),
        ("(SELECT count(s.c) FROM t2 AS s WHERE s.b = y.a) = 2", None),
        ("0 OR (255 ISNULL)", Some(0.0)),
        ("x.b BETWEEN NULL AND y.c", Some(0.0)),
        (
            "NULL AND EXISTS (SELECT 1 FROM t2 AS s WHERE s.c = y.b)",
            Some(0.0),
        ),
        (
            "NULL AND NOT EXISTS (SELECT 1 FROM t2 AS s WHERE EXISTS (SELECT 1 FROM t4 AS u WHERE u.b IS s.c))",
            Some(0.0),
        ),
        ("NULL", Some(0.0)),
        ("x'' ISNULL", Some(0.0)),
        ("(x'' ISNULL) AND NULL", Some(0.0)),
    ] {
        let forms = [
            format!("SELECT x.a FROM t1 AS x JOIN t3 AS y ON {cond}"),
            format!("SELECT x.a FROM t1 AS x, t3 AS y WHERE {cond}"),
            format!("SELECT x.a FROM t3 AS y, t1 AS x WHERE {cond}"),
            format!("SELECT x.a FROM t3 AS y CROSS JOIN t1 AS x WHERE {cond}"),
        ];
        let want = rows.unwrap_or_else(|| estimate(&conn, &forms[0]));
        for sql in &forms {
            let got = estimate(&conn, sql);
            assert!(
                same_estimate(got, want),
                "{sql}: estimated {got}, {want} wanted"
            );
        }
    }
}
