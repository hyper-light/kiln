//! T-QT.6.7 and T-QT.6.8: morsels over a table's own leaf pages, each worker with its own cursor in
//! the statement's snapshot, answer exactly as the serial VDBE does (docs/design/executor.md §8).

use super::*;
use crate::io::MemoryIO;
use crate::vdbe::morsel::{Aggregate, CmpOp, Predicate};
use crate::{ConnectOpts, Connection, Database, DatabaseOpts, OpenFlags, SqliteDialect};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use std::num::NonZeroUsize;

const COLUMNS: [&str; 4] = ["a", "b", "c", "p"];

fn open(name: &str) -> Arc<Connection> {
    let io = Arc::new(MemoryIO::new());
    let db = Database::open_file_with_flags(
        io,
        name,
        OpenFlags::default(),
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

fn sql_rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    conn.prepare(sql).unwrap().run_collect_rows().unwrap()
}

fn literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Text(text) => format!("'{}'", text.as_str()),
        other => other.to_string(),
    }
}

/// A table `name(a INTEGER, b INTEGER, c TEXT, p TEXT)` of `rows` random rows, some deleted, some
/// at negative and far rowids, padded so it spans many leaf pages.
fn fill(conn: &Arc<Connection>, rng: &mut ChaCha8Rng, name: &str, rows: usize) {
    conn.execute(format!(
        "CREATE TABLE {name}(a INTEGER, b INTEGER, c TEXT, p TEXT)"
    ))
    .unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 0..rows {
        let a = if rng.random_range(0..10) == 0 {
            "NULL".to_string()
        } else {
            rng.random_range(-20i64..20).to_string()
        };
        let b = if rng.random_range(0..10) == 0 {
            "NULL".to_string()
        } else {
            rng.random_range(-1_000_000_000_000i64..1_000_000_000_000)
                .to_string()
        };
        let c = if rng.random_range(0..10) == 0 {
            "NULL".to_string()
        } else {
            format!("'t{}'", rng.random_range(0..12))
        };
        let rowid = match rng.random_range(0..20) {
            0 => format!("{}", -1 - i64::try_from(i).unwrap()),
            1 => format!("{}", 1_000_000_000_000i64 + i64::try_from(i).unwrap()),
            _ => "NULL".to_string(),
        };
        let pad = "x".repeat(rng.random_range(0..120));
        conn.execute(format!(
            "INSERT INTO {name}(rowid, a, b, c, p) VALUES ({rowid}, {a}, {b}, {c}, '{pad}')"
        ))
        .unwrap();
    }
    conn.execute(format!("DELETE FROM {name} WHERE (rowid % 7) = 3"))
        .unwrap();
    conn.execute("COMMIT").unwrap();
}

fn source(conn: &Arc<Connection>, name: &str) -> TableSource {
    let root = conn
        .schema
        .read()
        .get_table(name)
        .unwrap()
        .get_root_page()
        .unwrap();
    TableSource::open(conn.get_pager(), root, COLUMNS.len(), None).unwrap()
}

fn random_query(rng: &mut ChaCha8Rng) -> (Vec<Predicate>, Vec<usize>, Vec<Aggregate>) {
    let ops = [
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Gt,
        CmpOp::Ge,
    ];
    let mut filter = Vec::new();
    for _ in 0..rng.random_range(0..3) {
        filter.push(match rng.random_range(0..4) {
            0 => Predicate::IsNull {
                column: rng.random_range(0..3),
            },
            1 => Predicate::NotNull {
                column: rng.random_range(0..3),
            },
            2 => Predicate::Compare {
                column: 2,
                op: CmpOp::Eq,
                value: Value::build_text(format!("t{}", rng.random_range(0..12))),
            },
            _ => {
                let column = rng.random_range(0..2);
                let value = if column == 0 {
                    rng.random_range(-20i64..20)
                } else {
                    rng.random_range(-1_000_000_000_000i64..1_000_000_000_000)
                };
                Predicate::Compare {
                    column,
                    op: ops[rng.random_range(0..ops.len())],
                    value: Value::from_i64(value),
                }
            }
        });
    }
    let group_by = match rng.random_range(0..4) {
        0 => vec![],
        1 => vec![0],
        2 => vec![2],
        _ => vec![2, 0],
    };
    let mut aggregates = vec![Aggregate::CountStar];
    for _ in 0..rng.random_range(0..4) {
        aggregates.push(match rng.random_range(0..4) {
            0 => Aggregate::Count(rng.random_range(0..3)),
            1 => Aggregate::Sum(rng.random_range(0..2)),
            2 => Aggregate::Min(rng.random_range(0..3)),
            _ => Aggregate::Max(rng.random_range(0..3)),
        });
    }
    (filter, group_by, aggregates)
}

fn to_sql(
    name: &str,
    filter: &[Predicate],
    group_by: &[usize],
    aggregates: &[Aggregate],
) -> String {
    let mut select: Vec<String> = group_by.iter().map(|g| COLUMNS[*g].to_string()).collect();
    for aggregate in aggregates {
        select.push(match aggregate {
            Aggregate::CountStar => "count(*)".to_string(),
            Aggregate::Count(c) => format!("count({})", COLUMNS[*c]),
            Aggregate::Sum(c) => format!("sum({})", COLUMNS[*c]),
            Aggregate::Min(c) => format!("min({})", COLUMNS[*c]),
            Aggregate::Max(c) => format!("max({})", COLUMNS[*c]),
        });
    }
    let terms: Vec<String> = filter
        .iter()
        .map(|term| match term {
            Predicate::IsNull { column } => format!("{} IS NULL", COLUMNS[*column]),
            Predicate::NotNull { column } => format!("{} IS NOT NULL", COLUMNS[*column]),
            Predicate::Compare { column, op, value } => {
                let op = match op {
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Eq => "=",
                    CmpOp::Ne => "<>",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                };
                format!("{} {op} {}", COLUMNS[*column], literal(value))
            }
        })
        .collect();
    let mut sql = format!("SELECT {} FROM {name}", select.join(", "));
    if !terms.is_empty() {
        sql.push_str(&format!(" WHERE {}", terms.join(" AND ")));
    }
    if !group_by.is_empty() {
        let keys: Vec<&str> = group_by.iter().map(|g| COLUMNS[*g]).collect();
        sql.push_str(&format!(" GROUP BY {}", keys.join(", ")));
    }
    sql
}

/// Rows as text, sorted: SQL and the pipeline order groups by the same keys but may print a
/// grouping's text and integers in another order, so the answers are compared as sets of rows.
fn canonical(rows: &[Vec<Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
    out.sort();
    out
}

fn degrees(leaves: usize) -> Vec<Degree> {
    let mut out = vec![Degree::Serial];
    for workers in [2usize, 3, 4, 8] {
        for morsel in [1usize, 2, 5, leaves.max(1)] {
            out.push(Degree::Parallel {
                workers: NonZeroUsize::new(workers).unwrap(),
                morsel_rows: NonZeroUsize::new(morsel).unwrap(),
            });
        }
    }
    out
}

/// T-QT.6.7: random filters, groupings and aggregates over random tables of many leaf pages, run
/// with each worker's own cursor over its leaves at 1, 2, 3, 4 and 8 workers and several morsel
/// sizes, and through `ParallelMode::On` and `Off`, equal the VDBE's serial answer row for row.
#[test]
fn t_qt_6_7_leaf_morsels_answer_as_the_serial_vdbe() {
    let conn = open(":memory:qt-6-7");
    let mut rng = ChaCha8Rng::seed_from_u64(67);
    fill(&conn, &mut rng, "t", 3000);
    let mut costs = MachineCosts::measure().unwrap();
    // One read transaction: every worker's cursor reads the statement's one snapshot.
    conn.execute("BEGIN").unwrap();
    let count = sql_rows(&conn, "SELECT count(*) FROM t");
    let table = source(&conn, "t");
    assert!(
        table.leaves() > 8,
        "the table spans {} leaves",
        table.leaves()
    );
    let mut parallel_runs = 0usize;
    for _ in 0..60 {
        let (filter, group_by, aggregates) = random_query(&mut rng);
        let sql = to_sql("t", &filter, &group_by, &aggregates);
        let expected = canonical(&sql_rows(&conn, &sql));
        let shape = ScanAggregate {
            rows: &[],
            filter: &filter,
            group_by: &group_by,
            aggregates: &aggregates,
        };
        let pipeline = TableAggregate {
            table: &table,
            shape: &shape,
        };
        for degree in degrees(table.leaves()) {
            let budget = MemoryBudget::new(usize::MAX);
            let (answer, stats) = pipeline.run_at(degree, &budget).unwrap();
            assert_eq!(canonical(&answer), expected, "{sql} at {degree:?}");
            if stats.workers > 1 {
                parallel_runs += 1;
            }
        }
        for mode in [ParallelMode::Off, ParallelMode::On] {
            let budget = MemoryBudget::new(usize::MAX);
            let (answer, _) = pipeline.run_mode(mode, &mut costs, &budget).unwrap();
            assert_eq!(canonical(&answer), expected, "{sql} in {mode:?}");
        }
    }
    // Non-vacuity: the parallel degrees really ran on several workers.
    assert!(parallel_runs > 0);
    assert_eq!(count, sql_rows(&conn, "SELECT count(*) FROM t"));
    conn.execute("COMMIT").unwrap();
}

/// T-QT.6.8: a hash join's probe side read by workers over the probe table's leaves equals the
/// VDBE's join, match for match and in its order (probe rowid, then build rowid).
#[test]
fn t_qt_6_8_leaf_morsel_probe_matches_the_serial_vdbe_join() {
    let conn = open(":memory:qt-6-8");
    let mut rng = ChaCha8Rng::seed_from_u64(68);
    fill(&conn, &mut rng, "t", 2500);
    fill(&conn, &mut rng, "u", 300);
    let mut costs = MachineCosts::measure().unwrap();
    conn.execute("BEGIN").unwrap();
    let probe_table = source(&conn, "t");
    let build_table = source(&conn, "u");
    for (probe_key, build_key) in [(vec![0usize], vec![0usize]), (vec![2, 0], vec![2, 0])] {
        let on: Vec<String> = probe_key
            .iter()
            .zip(&build_key)
            .map(|(p, b)| format!("t.{} = u.{}", COLUMNS[*p], COLUMNS[*b]))
            .collect();
        let sql = format!(
            "SELECT t.rowid, u.rowid FROM t JOIN u ON {} ORDER BY t.rowid, u.rowid",
            on.join(" AND ")
        );
        let expected: Vec<(i64, i64)> = sql_rows(&conn, &sql)
            .into_iter()
            .map(|row| match (&row[0], &row[1]) {
                (
                    Value::Numeric(crate::numeric::Numeric::Integer(t)),
                    Value::Numeric(crate::numeric::Numeric::Integer(u)),
                ) => (*t, *u),
                other => panic!("not two rowids: {other:?}"),
            })
            .collect();
        assert!(!expected.is_empty());
        let build_rows = build_table.rows().unwrap();
        let build_values: Vec<Vec<Value>> = build_rows.iter().map(|(_, row)| row.clone()).collect();
        let budget = MemoryBudget::new(usize::MAX);
        let probe = HashProbe::build(&build_values, &build_key, &[], &probe_key, &budget).unwrap();
        let pipeline = TableProbe {
            table: &probe_table,
            probe: &probe,
        };
        let as_rowids = |matches: Vec<(i64, usize)>| -> Vec<(i64, i64)> {
            matches
                .into_iter()
                .map(|(t, build)| (t, build_rows[build].0))
                .collect()
        };
        for degree in degrees(probe_table.leaves()) {
            let budget = MemoryBudget::new(usize::MAX);
            let (matches, _) = pipeline.run_at(degree, &budget).unwrap();
            assert_eq!(as_rowids(matches), expected, "{sql} at {degree:?}");
        }
        for mode in [ParallelMode::Off, ParallelMode::On] {
            let budget = MemoryBudget::new(usize::MAX);
            let (matches, _) = pipeline.run_mode(mode, &mut costs, &budget).unwrap();
            assert_eq!(as_rowids(matches), expected, "{sql} in {mode:?}");
        }
    }
    conn.execute("COMMIT").unwrap();
}

/// T-QT.6.9: a table whose INTEGER PRIMARY KEY is its rowid reads the rowid into that column,
/// and an empty table is one leaf with no rows.
#[test]
fn t_qt_6_9_rowid_alias_and_empty_table() {
    let conn = open(":memory:qt-6-9");
    conn.execute("CREATE TABLE e(id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    conn.execute("BEGIN").unwrap();
    let root = conn
        .schema
        .read()
        .get_table("e")
        .unwrap()
        .get_root_page()
        .unwrap();
    let empty = TableSource::open(conn.get_pager(), root, 2, Some(0)).unwrap();
    assert_eq!(empty.leaves(), 1);
    assert!(empty.rows().unwrap().is_empty());
    conn.execute("COMMIT").unwrap();
    for i in 0..2000 {
        conn.execute(format!(
            "INSERT INTO e VALUES ({}, {})",
            i * 3 - 500,
            i % 17
        ))
        .unwrap();
    }
    conn.execute("BEGIN").unwrap();
    let expected = sql_rows(
        &conn,
        "SELECT sum(id), min(id), max(id), count(*) FROM e WHERE v > 4",
    );
    let table = TableSource::open(conn.get_pager(), root, 2, Some(0)).unwrap();
    assert!(table.leaves() > 2);
    let filter = [Predicate::Compare {
        column: 1,
        op: CmpOp::Gt,
        value: Value::from_i64(4),
    }];
    let aggregates = [
        Aggregate::Sum(0),
        Aggregate::Min(0),
        Aggregate::Max(0),
        Aggregate::CountStar,
    ];
    let shape = ScanAggregate {
        rows: &[],
        filter: &filter,
        group_by: &[],
        aggregates: &aggregates,
    };
    let pipeline = TableAggregate {
        table: &table,
        shape: &shape,
    };
    for degree in degrees(table.leaves()) {
        let budget = MemoryBudget::new(usize::MAX);
        let (answer, _) = pipeline.run_at(degree, &budget).unwrap();
        assert_eq!(answer, expected, "at {degree:?}");
    }
    conn.execute("COMMIT").unwrap();
}

/// T-QT.6.10: the VDBE's hook. Random filters, groupings and aggregates in SQL answer the same,
/// row for row and in order, with `ParallelMode` off (the serial loop) and on, the machine stated
/// as 2, 3, 4 and 8 cores with workers cheap to start, so the run takes that many workers; and
/// the hook really ran (the parallel run measured the connection's costs).
#[test]
fn t_qt_6_10_vdbe_hook_answers_as_the_serial_loop() {
    let conn = open(":memory:qt-6-10");
    let mut rng = ChaCha8Rng::seed_from_u64(610);
    fill(&conn, &mut rng, "t", 3000);
    let mut hooked = 0usize;
    for _ in 0..60 {
        let (filter, group_by, aggregates) = random_query(&mut rng);
        let sql = to_sql("t", &filter, &group_by, &aggregates);
        conn.set_parallel_mode(ParallelMode::Off);
        let serial = sql_rows(&conn, &sql);
        conn.set_parallel_mode(ParallelMode::On);
        for cores in [2usize, 3, 4, 8] {
            *conn.morsel_costs.lock() = Some(MachineCosts::stated(
                NonZeroUsize::new(cores).unwrap(),
                1.0,
                1.0,
            ));
            assert_eq!(sql_rows(&conn, &sql), serial, "{sql} on {cores} cores");
            // The stated costs are learned from by the run: a changed mean shows it ran.
            let ran = conn
                .morsel_costs
                .lock()
                .as_ref()
                .is_some_and(|costs| costs.worker_start().samples() > 1);
            if ran {
                hooked += 1;
            }
        }
    }
    conn.set_parallel_mode(ParallelMode::Off);
    // Non-vacuity: most queries qualify (every shape `random_query` makes does).
    assert!(hooked > 120, "the hook ran {hooked} times of 240");
}

/// A random inner equi-join of `t` and `u` on one or two columns, with filters on either table, a
/// grouping and aggregates over both tables' columns.
fn random_join(rng: &mut ChaCha8Rng) -> String {
    let tables = ["t", "u"];
    let ops = ["<", "<=", "=", "<>", ">", ">="];
    let keys: &[&str] = match rng.random_range(0..3) {
        0 => &["a"],
        1 => &["c"],
        _ => &["a", "c"],
    };
    let on: Vec<String> = keys.iter().map(|k| format!("t.{k} = u.{k}")).collect();
    let mut terms = Vec::new();
    for _ in 0..rng.random_range(0..3) {
        let table = tables[rng.random_range(0..2)];
        let op = ops[rng.random_range(0..ops.len())];
        terms.push(match rng.random_range(0..5) {
            0 => format!("{table}.a {op} {}", rng.random_range(-20i64..20)),
            1 => format!(
                "{table}.b {op} {}",
                rng.random_range(-1_000_000_000_000i64..1_000_000_000_000)
            ),
            2 => format!("{table}.c {op} 't{}'", rng.random_range(0..12)),
            3 => format!("{table}.c IS NULL"),
            _ => format!("{table}.a IS NOT NULL"),
        });
    }
    let column = |rng: &mut ChaCha8Rng| {
        format!(
            "{}.{}",
            tables[rng.random_range(0..2)],
            ["a", "b", "c"][rng.random_range(0..3)]
        )
    };
    let group_by: Vec<String> = (0..rng.random_range(0..3)).map(|_| column(rng)).collect();
    let mut select = group_by.clone();
    for _ in 0..rng.random_range(1..4) {
        select.push(match rng.random_range(0..5) {
            0 => "count(*)".to_string(),
            1 => format!("count({})", column(rng)),
            2 => format!(
                "sum({}.{})",
                tables[rng.random_range(0..2)],
                ["a", "b"][rng.random_range(0..2)]
            ),
            3 => format!("min({})", column(rng)),
            _ => format!("max({})", column(rng)),
        });
    }
    let mut sql = format!(
        "SELECT {} FROM t JOIN u ON {}",
        select.join(", "),
        on.join(" AND ")
    );
    if !terms.is_empty() {
        sql.push_str(&format!(" WHERE {}", terms.join(" AND ")));
    }
    if !group_by.is_empty() {
        sql.push_str(&format!(" GROUP BY {}", group_by.join(", ")));
    }
    sql
}

/// T-QT.6.11: the VDBE's hook for a hash join's probe. Random inner equi-joins of two tables of
/// many leaves, on one or two columns, with filters on either table and groupings and aggregates
/// over both tables' columns, answer the same, row for row and in order, with `ParallelMode` off
/// (the serial hash join) and on, the machine stated as 2, 3, 4 and 8 cores; and the hook ran for
/// most of them (the planner held to hash joins).
#[test]
fn t_qt_6_11_vdbe_hook_probes_a_hash_join_as_the_serial_loop() {
    let conn = open(":memory:qt-6-11");
    let mut rng = ChaCha8Rng::seed_from_u64(611);
    fill(&conn, &mut rng, "t", 1500);
    fill(&conn, &mut rng, "u", 400);
    conn.set_join_methods(crate::JoinMethods::HashJoinsOnly);
    let queries = 40usize;
    let mut hooked = 0usize;
    for _ in 0..queries {
        let sql = random_join(&mut rng);
        conn.set_parallel_mode(ParallelMode::Off);
        let serial = sql_rows(&conn, &sql);
        conn.set_parallel_mode(ParallelMode::On);
        for cores in [2usize, 3, 4, 8] {
            *conn.morsel_costs.lock() = Some(MachineCosts::stated(
                NonZeroUsize::new(cores).unwrap(),
                1.0,
                1.0,
            ));
            assert_eq!(sql_rows(&conn, &sql), serial, "{sql} on {cores} cores");
            let ran = conn
                .morsel_costs
                .lock()
                .as_ref()
                .is_some_and(|costs| costs.worker_start().samples() > 1);
            if ran {
                hooked += 1;
            }
        }
    }
    conn.set_parallel_mode(ParallelMode::Off);
    conn.set_join_methods(crate::JoinMethods::ByCost);
    // Non-vacuity: most joins take the planner's hash join and the hook.
    assert!(
        hooked > queries * 2,
        "the hook ran {hooked} times of {}",
        queries * 4
    );
}

/// T-QT.6.12: a hash join's table built by the workers together, partitioned. The build side is
/// scanned over its own leaves at 1, 2, 3, 4 and 8 workers and filled into 1, 2, 3, 4 and 8
/// partitions; the probe's matches equal the serial VDBE join's, in order (a key's build rows in
/// build order whatever worker read them), and the runs were parallel and partitioned. A budget
/// too small for the table is a typed refusal, never a partial table.
#[test]
fn t_qt_6_12_partitioned_parallel_build_matches_the_serial_join() {
    let conn = open(":memory:qt-6-12");
    let mut rng = ChaCha8Rng::seed_from_u64(612);
    fill(&conn, &mut rng, "t", 1500);
    fill(&conn, &mut rng, "u", 1200);
    conn.execute("BEGIN").unwrap();
    let probe_table = source(&conn, "t");
    let build_table = source(&conn, "u");
    let mut parallel_scans = 0usize;
    let mut partitioned = 0usize;
    for (probe_key, build_key, filter) in [
        (vec![0usize], vec![0usize], vec![]),
        (
            vec![2, 0],
            vec![2, 0],
            vec![Predicate::NotNull { column: 1 }],
        ),
        (
            vec![2],
            vec![2],
            vec![Predicate::Compare {
                column: 0,
                op: CmpOp::Ge,
                value: Value::from_i64(-5),
            }],
        ),
    ] {
        let on: Vec<String> = probe_key
            .iter()
            .zip(&build_key)
            .map(|(p, b)| format!("t.{} = u.{}", COLUMNS[*p], COLUMNS[*b]))
            .collect();
        let mut sql = format!(
            "SELECT t.rowid, u.rowid FROM t JOIN u ON {}",
            on.join(" AND ")
        );
        for term in &filter {
            sql.push_str(match term {
                Predicate::NotNull { .. } => " AND u.b IS NOT NULL",
                _ => " AND u.a >= -5",
            });
        }
        sql.push_str(" ORDER BY t.rowid, u.rowid");
        let expected: Vec<(i64, i64)> = sql_rows(&conn, &sql)
            .into_iter()
            .map(|row| match (&row[0], &row[1]) {
                (
                    Value::Numeric(crate::numeric::Numeric::Integer(t)),
                    Value::Numeric(crate::numeric::Numeric::Integer(u)),
                ) => (*t, *u),
                other => panic!("not two rowids: {other:?}"),
            })
            .collect();
        assert!(!expected.is_empty(), "{sql}");
        // The rowid of each kept build row, read back from the build's own rows by the rowid
        // stored in no column: the test keeps `u`'s rows with their rowids beside them.
        let all = build_table.rows().unwrap();
        for partitions in [1usize, 2, 3, 4, 8] {
            let partitioner = Partitioner::new(NonZeroUsize::new(partitions).unwrap());
            for degree in degrees(build_table.leaves()) {
                let budget = MemoryBudget::new(usize::MAX);
                let build = TableBuild {
                    table: &build_table,
                    filter: &filter,
                    key: &build_key,
                    partitioner: &partitioner,
                };
                let (rows, table, stats) = build.run_at(degree, &budget).unwrap();
                if stats.scan.workers > 1 {
                    parallel_scans += 1;
                }
                if table.filled_partitions() > 1 {
                    partitioned += 1;
                }
                // Kept rows map back to rowids: rows are kept in rowid order, so walk `all`.
                let mut rowids = Vec::new();
                let mut next = all.iter();
                for row in &rows {
                    let (rowid, _) = next.by_ref().find(|(_, r)| r == row).unwrap();
                    rowids.push(*rowid);
                }
                let probe = HashProbe::over(table, &[], &probe_key);
                let pipeline = TableProbe {
                    table: &probe_table,
                    probe: &probe,
                };
                let (matches, _) = pipeline.run_at(degree, &budget).unwrap();
                let got: Vec<(i64, i64)> = matches
                    .into_iter()
                    .map(|(t, build)| (t, rowids[build]))
                    .collect();
                assert_eq!(
                    got, expected,
                    "{sql}: {partitions} partitions at {degree:?}"
                );
            }
        }
        // A budget the table can't fit in is refused by type, at every degree.
        for degree in degrees(build_table.leaves()) {
            let partitioner = Partitioner::new(NonZeroUsize::new(4).unwrap());
            let budget = MemoryBudget::new(4096);
            let build = TableBuild {
                table: &build_table,
                filter: &filter,
                key: &build_key,
                partitioner: &partitioner,
            };
            assert!(
                matches!(
                    build.run_at(degree, &budget),
                    Err(MorselError::MemoryBudget { .. })
                ),
                "{sql} at {degree:?}"
            );
        }
    }
    conn.execute("COMMIT").unwrap();
    // Non-vacuity: most builds ran on several workers and filled several partitions.
    assert!(parallel_scans > 100, "{parallel_scans}");
    assert!(partitioned > 150, "{partitioned}");
}

/// A random inner equi-join of `t` and `u` returning rows: result columns of either table, filters
/// on either, and half the time an ORDER BY over the first one or two result columns, ascending or
/// descending.
fn random_join_rows(rng: &mut ChaCha8Rng) -> String {
    let tables = ["t", "u"];
    let ops = ["<", "<=", "=", "<>", ">", ">="];
    let keys: &[&str] = match rng.random_range(0..3) {
        0 => &["a"],
        1 => &["c"],
        _ => &["a", "c"],
    };
    let on: Vec<String> = keys.iter().map(|k| format!("t.{k} = u.{k}")).collect();
    let mut terms = Vec::new();
    for _ in 0..rng.random_range(0..3) {
        let table = tables[rng.random_range(0..2)];
        let op = ops[rng.random_range(0..ops.len())];
        terms.push(match rng.random_range(0..4) {
            0 => format!("{table}.a {op} {}", rng.random_range(-20i64..20)),
            1 => format!("{table}.c {op} 't{}'", rng.random_range(0..12)),
            2 => format!("{table}.c IS NULL"),
            _ => format!("{table}.a IS NOT NULL"),
        });
    }
    let column = |rng: &mut ChaCha8Rng| {
        format!(
            "{}.{}",
            tables[rng.random_range(0..2)],
            ["a", "b", "c", "p"][rng.random_range(0..4)]
        )
    };
    let select: Vec<String> = (0..rng.random_range(1..5)).map(|_| column(rng)).collect();
    let mut sql = format!(
        "SELECT {} FROM t JOIN u ON {}",
        select.join(", "),
        on.join(" AND ")
    );
    if !terms.is_empty() {
        sql.push_str(&format!(" WHERE {}", terms.join(" AND ")));
    }
    if rng.random_bool(0.5) {
        // The ORDER BY keys lead the select list, so a tie's rows can be told apart below.
        let keys = rng.random_range(1..=select.len().min(2));
        let order: Vec<String> = select[..keys]
            .iter()
            .map(|key| {
                let dir = if rng.random_bool(0.5) { " DESC" } else { "" };
                format!("{key}{dir}")
            })
            .collect();
        sql.push_str(&format!(" ORDER BY {} -- keys {keys}", order.join(", ")));
    }
    sql
}

/// How many ORDER BY keys lead `sql`'s select list, read from the comment `random_join_rows` ends it with.
fn order_keys(sql: &str) -> Option<usize> {
    let (_, keys) = sql.split_once("-- keys ")?;
    keys.trim().parse().ok()
}

/// T-QT.6.13: joins returning rows, no aggregates, run on morsels. Random inner equi-joins of two
/// tables of many leaves, with filters on either table and an ORDER BY or none, answer the same
/// with `ParallelMode` off (the serial hash join) and on, the machine stated as 2, 3, 4 and 8
/// cores: in order with an ORDER BY (the workers' sorted runs merged), as the same rows without
/// one; and the hook ran for most of them.
#[test]
fn t_qt_6_13_vdbe_hook_returns_a_hash_joins_rows_as_the_serial_loop() {
    let conn = open(":memory:qt-6-13");
    let mut rng = ChaCha8Rng::seed_from_u64(613);
    fill(&conn, &mut rng, "t", 1500);
    fill(&conn, &mut rng, "u", 400);
    conn.set_join_methods(crate::JoinMethods::HashJoinsOnly);
    let queries = 40usize;
    let mut hooked = 0usize;
    let mut ordered = 0usize;
    for _ in 0..queries {
        let sql = random_join_rows(&mut rng);
        let has_order = order_keys(&sql).is_some();
        conn.set_parallel_mode(ParallelMode::Off);
        let serial = sql_rows(&conn, &sql);
        conn.set_parallel_mode(ParallelMode::On);
        for cores in [2usize, 3, 4, 8] {
            *conn.morsel_costs.lock() = Some(MachineCosts::stated(
                NonZeroUsize::new(cores).unwrap(),
                1.0,
                1.0,
            ));
            let parallel = sql_rows(&conn, &sql);
            if let Some(keys) = order_keys(&sql) {
                // In ORDER BY order, row for row in the keys; rows tied on them may come in
                // any order, so each run of ties is compared as a set.
                let key = |row: &Vec<Value>| format!("{:?}", &row[..keys]);
                let serial_keys: Vec<String> = serial.iter().map(key).collect();
                let parallel_keys: Vec<String> = parallel.iter().map(key).collect();
                assert_eq!(parallel_keys, serial_keys, "{sql} on {cores} cores");
                assert_eq!(
                    canonical(&parallel),
                    canonical(&serial),
                    "{sql} on {cores} cores"
                );
            } else {
                assert_eq!(
                    canonical(&parallel),
                    canonical(&serial),
                    "{sql} on {cores} cores"
                );
            }
            let ran = conn
                .morsel_costs
                .lock()
                .as_ref()
                .is_some_and(|costs| costs.worker_start().samples() > 1);
            if ran {
                hooked += 1;
                if has_order {
                    ordered += 1;
                }
            }
        }
    }
    conn.set_parallel_mode(ParallelMode::Off);
    conn.set_join_methods(crate::JoinMethods::ByCost);
    // Non-vacuity: most joins took the hook, ordered ones among them.
    assert!(
        hooked > queries * 2,
        "the hook ran {hooked} times of {}",
        queries * 4
    );
    assert!(ordered > 0, "no ordered join took the hook");
}
