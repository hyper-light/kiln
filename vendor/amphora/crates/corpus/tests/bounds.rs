//! G2 steps 4 and 7 (T-G2.4.2, T-G2.7.4): what opens, settles and closes an engine is bounded by
//! the run's budget as a record is, so a server that never answers is unreachable, never a hung
//! run; and an answer an engine's output can't carry whole (Turso's list mode: a text holding `|`
//! or a line end) is unreadable, never a disagreement.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use std::time::{Duration, Instant};

use amphora_corpus::amphora::AmphoraRender;
use amphora_corpus::answer::Value;
use amphora_corpus::engines::{ClickHouse, Shell, Unavailable, sqlite_shell};
use amphora_corpus::run::{Engine, Limits, Outcome, Stop, run_file};

/// An engine answering every query with `rows`, counting them as `count` when it has a count (an
/// output that can't carry text whole), and settling as `settle` says.
struct Canned {
    rows: Vec<Vec<Value>>,
    count: Option<u64>,
    settle: Result<(), Stop>,
}

impl Engine for Canned {
    fn run(&mut self, sql: &str, keep: usize, _: Option<Instant>) -> Result<Vec<Vec<Value>>, Stop> {
        if sql.starts_with("CREATE") {
            return Ok(Vec::new());
        }
        Ok(self.rows.iter().take(keep).cloned().collect())
    }

    fn settle(&mut self, _: Option<Instant>) -> Result<(), Stop> {
        self.settle.clone()
    }

    fn recount(&mut self, _: &str, _: Option<Instant>) -> Option<Result<u64, Stop>> {
        self.count.map(Ok)
    }
}

fn text(s: &str) -> Value {
    Value::Text(s.as_bytes().to_vec())
}

const TWO_COLUMNS: &str = "statement ok
CREATE TABLE t(a, b)

query TT nosort
SELECT a, b FROM t
----
x|y
z
";

const ONE_COLUMN: &str = "statement ok
CREATE TABLE t(a)

query T nosort
SELECT a FROM t
----
x@y
";

fn outcomes(engine: &mut Canned, file: &str) -> Vec<Outcome> {
    let mut render = AmphoraRender::new().unwrap();
    let mut seen = Vec::new();
    run_file(
        engine,
        &mut render,
        file,
        None,
        None,
        Limits::default(),
        &mut |r| seen.push(r.outcome),
    )
    .unwrap();
    seen
}

/// Do: answer a two-column query whose first text held `|`, read as three values. Expect:
/// unreadable, not a disagreement.
#[test]
fn t_g2_4_2_a_separator_in_text_is_unreadable() {
    let mut e = Canned {
        rows: vec![vec![text("x"), text("y"), text("z")]],
        count: Some(1),
        settle: Ok(()),
    };
    let seen = outcomes(&mut e, TWO_COLUMNS);
    assert!(
        matches!(&seen[1], Outcome::Unreadable(why) if why.contains("3 values, not 2")),
        "{seen:?}"
    );
}

/// Do: answer a one-column query whose one text held a line end, read as two rows while the engine
/// counts one. Expect: unreadable.
#[test]
fn t_g2_4_2_a_line_end_in_text_is_unreadable() {
    let mut e = Canned {
        rows: vec![vec![text("x")], vec![text("y")]],
        count: Some(1),
        settle: Ok(()),
    };
    let seen = outcomes(&mut e, ONE_COLUMN);
    assert!(
        matches!(&seen[1], Outcome::Unreadable(why) if why.contains("2 rows read, 1 counted")),
        "{seen:?}"
    );
}

/// Do: answer wrongly with rows read whole (as many as counted, each as wide as the query), and
/// again from an engine whose answers always read whole. Expect: both disagree.
#[test]
fn t_g2_4_2_a_wrong_answer_read_whole_still_disagrees() {
    for count in [Some(1), None] {
        let mut e = Canned {
            rows: vec![vec![text("other")]],
            count,
            settle: Ok(()),
        };
        let seen = outcomes(&mut e, ONE_COLUMN);
        assert!(matches!(seen[1], Outcome::Answer { .. }), "{seen:?}");
    }
}

/// Do: settle past the record's bound, then settle refused. Expect: the query is past the budget;
/// a refused settle is counted unsettled and the query still runs.
#[test]
fn t_g2_7_4_a_settle_is_bounded_as_its_record() {
    let mut e = Canned {
        rows: vec![vec![text("x@y")]],
        count: None,
        settle: Err(Stop::PastBudget),
    };
    assert_eq!(outcomes(&mut e, ONE_COLUMN)[1], Outcome::PastBudget);
    e.settle = Err(Stop::Refused("busy".into()));
    let mut render = AmphoraRender::new().unwrap();
    let counts = run_file(
        &mut e,
        &mut render,
        ONE_COLUMN,
        None,
        None,
        Limits::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((counts.agree, counts.unsettled), (2, 1));
}

/// Do: open ClickHouse on a socket that takes the connection and never answers, bounded. Expect:
/// Unreachable, saying it didn't answer within the bound.
#[test]
fn t_g2_7_4_a_server_that_never_answers_is_unreachable() {
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    match ClickHouse::open(
        &format!("http://127.0.0.1:{port}"),
        Some(Duration::from_millis(200)),
    ) {
        Err(Unavailable::Unreachable { engine, why }) => {
            assert_eq!(engine, "clickhouse");
            assert!(why.contains("no answer within"), "{why}");
        }
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("opened a server that never answered"),
    }
}

/// Do: open SQLite's shell with an opening statement that never ends, bounded. Expect: where
/// sqlite3 is here, Unreachable, saying it didn't answer within the bound; where it isn't, Missing.
#[test]
fn t_g2_7_4_an_opening_statement_past_its_bound_is_unreachable() {
    let mut spec = sqlite_shell("sqlite", "sqlite3".into());
    spec.setup = vec![
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c"
            .into(),
    ];
    match Shell::open(spec, Some(Duration::from_millis(300))) {
        Err(Unavailable::Unreachable { engine, why }) => {
            assert_eq!(engine, "sqlite");
            assert!(why.contains("no answer within"), "{why}");
        }
        Err(Unavailable::Missing { engine, .. }) => assert_eq!(engine, "sqlite"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("an endless opening statement opened"),
    }
}
