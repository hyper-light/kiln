//! Custom plans, then a generic one, for prepared statements (docs/design/planner.md, decision
//! "custom plans, then a generic one"; PL.12). A statement was planned once, at prepare: a
//! parameter's value never reached its estimates (`bench/planner/audit/08-fixed-at-prepare.sql`),
//! so `k = ?` had one plan and one estimate whether the value bound kept nine rows in ten or none.

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, OpenOptions, PlanCacheMode, PlatformIO,
    SqliteDialect, Statement, Value,
};
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;

fn open(dir: &Path) -> Arc<Connection> {
    let io = Arc::new(PlatformIO::new().unwrap());
    let options = OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new());
    let db = Database::open(io, dir.join("plans.db").to_str().unwrap(), options).unwrap();
    db.connect(ConnectOpts::new()).unwrap()
}

/// `t` of 5,000 rows: `k` is 1 in 4,500 of them and distinct in the other 500 (2 to 501), `d` is
/// the row's number; an index on `k`, none on `d`. Analyzed when `analyze`.
fn skewed(conn: &Arc<Connection>, analyze: bool) {
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INT, d INT, v INT)")
        .unwrap();
    conn.execute("CREATE INDEX t_k ON t(k)").unwrap();
    conn.execute("BEGIN").unwrap();
    let rows: Vec<String> = (1..=5_000)
        .map(|i| {
            let k = if i <= 4_500 { 1 } else { i - 4_499 };
            format!("({i}, {k}, {i}, {})", i % 97)
        })
        .collect();
    for chunk in rows.chunks(500) {
        conn.execute(format!("INSERT INTO t VALUES {}", chunk.join(", ")))
            .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    if analyze {
        conn.execute("ANALYZE").unwrap();
    }
}

fn bind(stmt: &mut Statement, values: &[i64]) {
    for (i, value) in values.iter().enumerate() {
        stmt.bind_at(NonZero::new(i + 1).unwrap(), Value::from_i64(*value))
            .unwrap();
    }
}

/// Runs `stmt` with `values` bound, to its end, and resets it for the next run.
fn run(stmt: &mut Statement, values: &[i64]) -> Vec<Vec<Value>> {
    bind(stmt, values);
    let rows = stmt.run_collect_rows().unwrap();
    stmt.reset().unwrap();
    rows
}

/// The EXPLAIN QUERY PLAN FORMAT=JSON of `sql` prepared once and run with `values` bound (none:
/// unbound): its estimated rows and its plan's lines.
fn explain(conn: &Arc<Connection>, sql: &str, values: &[i64]) -> (f64, String) {
    let mut stmt = conn
        .prepare(format!("EXPLAIN QUERY PLAN FORMAT=JSON {sql}"))
        .unwrap();
    let rows = run(&mut stmt, values);
    let text = rows[0][0].to_string();
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let estimate = json["output_rows"]
        .as_f64()
        .unwrap_or_else(|| panic!("{sql}: no output_rows in {text}"));
    let mut stmt = conn.prepare(format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let plan = run(&mut stmt, values)
        .iter()
        .map(|row| row[3].to_string())
        .collect::<Vec<_>>()
        .join("; ");
    (estimate, plan)
}

const BY_K: &str = "SELECT sum(v) FROM t WHERE k = ?";

/// T-PL.12.3. Do: explain `k = ?` prepared once, unbound, then bound to the common value 1 and to
/// the rare 300. Expect: unbound, the generic estimate, a share of the distinct values (a few
/// rows); bound to 1, about the 4,500 rows it keeps; bound to 300, a few rows: the plan made with
/// a run's value reads the value, where every value had the generic estimate. The statement
/// returns its rows, not their sum: an aggregate's estimate is its one output row, whatever its
/// filter keeps, so the rows the filter keeps are read where they are the statement's output.
/// Which plan the estimates choose is not asserted: it weighs them by the seek, hash and sort costs
/// the database times where it runs (QT.5's learned weights), so on one machine an index search
/// for nine rows in ten of a table its cache holds whole costs less than a scan, and on another
/// more; the estimates are what a value bound changes.
#[test]
fn t_pl_12_3_a_plan_made_with_a_value_estimates_that_value() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let rows_by_k = "SELECT v FROM t WHERE k = ?";
    let (generic, generic_plan) = explain(&conn, rows_by_k, &[]);
    let (common, common_plan) = explain(&conn, rows_by_k, &[1]);
    let (rare, rare_plan) = explain(&conn, rows_by_k, &[300]);
    assert!(generic < 50.0, "generic estimate {generic}: {generic_plan}");
    assert!(common > 2_500.0, "k = 1 estimated {common}: {common_plan}");
    assert!(rare < 50.0, "k = 300 estimated {rare}: {rare_plan}");
}

/// T-PL.12.3. Do: explain `d < ?`, a range on a column no index holds, filtered on the table's
/// sample, bound to 10 and to 4,000. Expect: estimates near the 9 and 3,999 rows each keeps.
#[test]
fn t_pl_12_3_a_range_on_the_sample_takes_its_bound_value() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let sql = "SELECT v FROM t WHERE d < ?";
    let (few, _) = explain(&conn, sql, &[10]);
    let (most, _) = explain(&conn, sql, &[4_000]);
    assert!(few < 100.0, "d < 10 estimated {few}");
    assert!(most > 3_000.0, "d < 4000 estimated {most}");
}

/// T-PL.12.4. Do: run `k = ?` with custom plans forced and with the generic plan forced, for the
/// common value, rare ones and one no row holds, ten times each, against the same query with the
/// value written in. Expect: every answer the same, and a custom plan made for each forced run.
#[test]
fn t_pl_12_4_custom_and_generic_plans_answer_alike() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let values = [1, 2, 300, 501, 9_999];
    let mut custom = conn.prepare(BY_K).unwrap();
    let mut generic = conn.prepare(BY_K).unwrap();
    for _ in 0..10 {
        for value in values {
            let literal = conn
                .prepare(format!("SELECT sum(v) FROM t WHERE k = {value}"))
                .unwrap()
                .run_collect_rows()
                .unwrap();
            conn.set_plan_cache_mode(PlanCacheMode::ForceCustom);
            assert_eq!(run(&mut custom, &[value]), literal, "custom, k = {value}");
            conn.set_plan_cache_mode(PlanCacheMode::ForceGeneric);
            assert_eq!(run(&mut generic, &[value]), literal, "generic, k = {value}");
        }
    }
    assert_eq!(custom.metrics().custom_plans, 50);
    assert_eq!(generic.metrics().custom_plans, 0);
}

/// T-PL.12.5. Do: run, ten times each under the default mode, a statement whose estimates read no
/// value (a key lookup), the skewed `k = ?` on a table without statistics, and `k = ?` with no
/// value bound. Expect: no custom plan for any: a plan made with their values would be the plan
/// they have, so they pay no planning (AC-PL.12.2's suite statements).
#[test]
fn t_pl_12_5_a_statement_whose_plan_no_value_changes_is_never_planned_again() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let mut by_key = conn.prepare("SELECT v FROM t WHERE id = ?").unwrap();
    for i in 1..=10 {
        run(&mut by_key, &[i]);
    }
    assert_eq!(by_key.metrics().custom_plans, 0);

    let mut unbound = conn.prepare(BY_K).unwrap();
    for _ in 0..10 {
        unbound.run_collect_rows().unwrap();
        unbound.reset().unwrap();
    }
    assert_eq!(unbound.metrics().custom_plans, 0);

    let dir = tempfile::tempdir().unwrap();
    let plain = open(dir.path());
    skewed(&plain, false);
    let mut no_stats = plain.prepare(BY_K).unwrap();
    for value in [1, 300, 1, 300, 1, 300, 1, 300, 1, 300] {
        run(&mut no_stats, &[value]);
    }
    assert_eq!(no_stats.metrics().custom_plans, 0);
}

/// T-PL.12.6. Do: under the default mode, run `k = ?` once, then many times; ANALYZE; run it again.
/// Expect: its first run a custom plan, since nothing yet says what its values change; never more
/// custom plans than runs; and after ANALYZE, which plans it again on the new statistics, a custom
/// plan first again, the earlier runs' measurements being of the old ones.
#[test]
fn t_pl_12_6_the_first_run_is_custom_and_new_statistics_start_the_choice_again() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let mut stmt = conn.prepare(BY_K).unwrap();
    run(&mut stmt, &[300]);
    assert_eq!(stmt.metrics().custom_plans, 1);
    for value in [300, 1, 301, 1, 302, 1, 303, 1, 304] {
        run(&mut stmt, &[value]);
    }
    let after_ten = stmt.metrics().custom_plans;
    assert!((1..=10).contains(&after_ten), "{after_ten} custom plans");
    conn.execute("ANALYZE").unwrap();
    run(&mut stmt, &[300]);
    assert_eq!(stmt.metrics().custom_plans, after_ten + 1);
    assert!(stmt.metrics().reprepares >= 1);
}

/// T-PL.12.6. Do: under the default mode, run `k = ?` 200 times, its values alternating between
/// the common one and rare ones. Expect: its answers those of the generic plan forced, run by run,
/// whichever plan each run takes.
#[test]
fn t_pl_12_6_the_default_mode_answers_as_the_generic_plan_does() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    let mut auto = conn.prepare(BY_K).unwrap();
    let mut generic = conn.prepare(BY_K).unwrap();
    for i in 0..200 {
        let value = if i % 2 == 0 { 1 } else { 2 + i % 500 };
        conn.set_plan_cache_mode(PlanCacheMode::Auto);
        let got = run(&mut auto, &[value]);
        conn.set_plan_cache_mode(PlanCacheMode::ForceGeneric);
        assert_eq!(got, run(&mut generic, &[value]), "run {i}, k = {value}");
    }
}

/// T-PL.12.6. Do: start a custom run of `k = ?`, read one row, and reset it part way; run it again
/// to its end; set a per-run timeout before a run. Expect: each run answers, and the statement's
/// state is sized for the plan it runs (no error from a custom plan's registers or cursors).
#[test]
fn t_pl_12_6_a_custom_run_reset_part_way_runs_again() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    skewed(&conn, true);
    conn.set_plan_cache_mode(PlanCacheMode::ForceCustom);
    let mut stmt = conn.prepare("SELECT v FROM t WHERE k = ?").unwrap();
    bind(&mut stmt, &[1]);
    loop {
        match stmt.step().unwrap() {
            amphora_engine::StepResult::Row => break,
            amphora_engine::StepResult::IO | amphora_engine::StepResult::Yield => {
                stmt._io().step().unwrap()
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    stmt.reset().unwrap();
    assert_eq!(run(&mut stmt, &[1]).len(), 4_500);
    stmt.set_query_timeout_override(Some(Some(std::time::Duration::from_secs(3_600))));
    assert_eq!(run(&mut stmt, &[300]).len(), 1);
    conn.set_plan_cache_mode(PlanCacheMode::ForceGeneric);
    assert_eq!(run(&mut stmt, &[1]).len(), 4_500);
    assert_eq!(stmt.metrics().custom_plans, 3);
}
