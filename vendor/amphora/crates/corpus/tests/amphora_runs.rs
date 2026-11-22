//! T-G2.7.2: a corpus file run on Amphora through the gate's own code path (amphora-corpus's
//! runner), SQLite's answers in the file.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use amphora_corpus::amphora::{Amphora, AmphoraRender};
use amphora_corpus::run::{Limits, Outcome, run_file};

const FILE: &str = "hash-threshold 4

statement ok
CREATE TABLE t(a INTEGER, b TEXT, c REAL)

statement ok
INSERT INTO t VALUES (1, 'one', 1.5), (2, 'two', NULL), (3, NULL, 2.25)

query ITR rowsort
SELECT a, b, c FROM t
----
6 values hashing to 0123456789abcdef0123456789abcdef

query R nosort
SELECT sum(c) FROM t
----
3.750

query T nosort
SELECT 1e15
----
1000000000000000.0

statement error
SELECT * FROM nope

onlyif mysql
query I nosort
SELECT 9
----
9
";

/// Do: run a file whose hashed answer is wrong on purpose, then the same file with it right.
/// Expect: that query, and only it, disagrees; then every record agrees, the MySQL-only one skipped.
#[test]
fn t_g2_7_2_a_file_runs_on_amphora_as_sqlite_runs_it() {
    let right = amphora_corpus::answer::md5_of(
        &[
            "1", "one", "1.500", "2", "two", "NULL", "3", "NULL", "2.250",
        ]
        .map(String::from),
    );
    let wrong_hash = FILE.replace("6 values", "9 values");
    let mut render = AmphoraRender::new().unwrap();
    let mut off = Vec::new();
    let counts = run_file(
        &mut Amphora::in_memory().unwrap(),
        &mut render,
        &wrong_hash,
        None,
        None,
        Limits::default(),
        &mut |r| {
            if r.outcome != Outcome::Agree {
                off.push((r.record.line, r.outcome));
            }
        },
    )
    .unwrap();
    assert_eq!(off.len(), 1, "{off:?}");
    assert_eq!(off[0].0, 9);
    assert_eq!(counts.disagree, 1);
    let fixed = wrong_hash.replace("0123456789abcdef0123456789abcdef", &right);
    let mut off = Vec::new();
    let counts = run_file(
        &mut Amphora::in_memory().unwrap(),
        &mut render,
        &fixed,
        None,
        None,
        Limits::default(),
        &mut |r| {
            if r.outcome != Outcome::Agree {
                off.push((r.record.line, r.outcome));
            }
        },
    )
    .unwrap();
    assert!(off.is_empty(), "{off:?}");
    assert_eq!((counts.agree, counts.skipped, counts.unsettled), (6, 1, 0));
}

/// Do: run a file with a deadline already past. Expect: no record run, each counted unreached.
#[test]
fn records_past_the_deadline_are_unreached_not_failed() {
    let mut render = AmphoraRender::new().unwrap();
    let past = std::time::Instant::now();
    let counts = run_file(
        &mut Amphora::in_memory().unwrap(),
        &mut render,
        FILE,
        None,
        None,
        Limits {
            deadline: Some(past),
            record: None,
        },
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((counts.agree, counts.disagree, counts.unreached), (0, 0, 6));
}

/// A table of 100 rows, then a 10-way join of it: 10^20 rows to count, which no machine finishes.
pub fn endless() -> String {
    let values: Vec<String> = (1..=100).map(|i| format!("({i})")).collect();
    let from: Vec<String> = (0..10).map(|i| format!("n t{i}")).collect();
    format!(
        "statement ok\nCREATE TABLE n(x INTEGER)\n\nstatement ok\nINSERT INTO n VALUES {}\n\nquery I nosort\nSELECT count(*) FROM {}\n----\n0\n\nquery I nosort\nSELECT 1\n----\n1\n",
        values.join(", "),
        from.join(", ")
    )
}

/// T-G2.7.4: Do: run a file whose query can't finish, each record bounded (a bound the test picks
/// as its input; the runner's own comes from the gate's budget). Expect: the engine stops it and
/// it's classified past the budget, not a disagreement; the statements before it agree, and the
/// next record still runs (no deadline: a required file) and agrees.
#[test]
fn t_g2_7_4_a_record_that_cant_finish_is_stopped_and_classified() {
    let mut render = AmphoraRender::new().unwrap();
    let mut seen = Vec::new();
    let counts = run_file(
        &mut Amphora::in_memory().unwrap(),
        &mut render,
        &endless(),
        None,
        None,
        Limits {
            deadline: None,
            record: Some(std::time::Duration::from_millis(200)),
        },
        &mut |r| seen.push(r.outcome),
    )
    .unwrap();
    assert_eq!(
        seen,
        [
            Outcome::Agree,
            Outcome::Agree,
            Outcome::PastBudget,
            Outcome::Agree
        ]
    );
    assert_eq!(
        (counts.agree, counts.disagree, counts.past_budget),
        (3, 0, 1)
    );
}
