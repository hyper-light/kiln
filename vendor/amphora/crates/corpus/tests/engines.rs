//! G2 step 4 (T-G2.4.2): the engine seam. Every engine the step names opens by name; one this
//! machine lacks is refused with a typed error, never passed; the shells' answers read as SQLite's
//! runner reads them; and where SQLite's own shell is here, a file runs on it through the gate's
//! code path, a record that can't finish stopped at its bound.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use amphora_corpus::amphora::AmphoraRender;
use amphora_corpus::answer::Value;
use amphora_corpus::engines::{
    ClickHouse, ENGINES, Shell, Unavailable, duckdb, mysql, open, postgres, quoted_rows,
    sqlite_shell, turso,
};
use amphora_corpus::run::{Limits, Outcome, run_file};

/// Do: open a name no engine has. Expect: Unknown, naming it.
#[test]
fn t_g2_4_2_an_unknown_engine_is_refused() {
    assert_eq!(
        open("oracle", None).err(),
        Some(Unavailable::Unknown("oracle".into()))
    );
    assert_eq!(ENGINES.len(), 9);
}

/// Do: open each shell engine with a program that isn't there. Expect: each refused as Missing,
/// naming the engine and the program.
#[test]
fn t_g2_4_2_an_engine_whose_program_is_missing_is_refused() {
    let gone = || "/no/such/dir/corpus-engine".to_owned();
    let specs = [
        sqlite_shell("sqlite", gone()),
        duckdb(gone()),
        turso(gone()),
        postgres("postgres", gone(), "postgres://p@127.0.0.1:1/p".into()),
        postgres("orioledb", gone(), "postgres://p@127.0.0.1:1/p".into()),
        postgres("cedardb", gone(), "postgres://p@127.0.0.1:1/p".into()),
        mysql(gone(), "mysql://root@127.0.0.1:1/db").unwrap(),
    ];
    let mut refused = Vec::new();
    for spec in specs {
        match Shell::open(spec, None) {
            Err(Unavailable::Missing {
                engine, program, ..
            }) => {
                assert_eq!(program, gone());
                refused.push(engine);
            }
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a missing program opened"),
        }
    }
    assert_eq!(
        refused,
        [
            "sqlite", "duckdb", "turso", "postgres", "orioledb", "cedardb", "mysql"
        ]
    );
}

/// Do: open ClickHouse at a port nothing listens on. Expect: Unreachable.
#[test]
fn t_g2_4_2_clickhouse_without_its_server_is_refused() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    match ClickHouse::open(&format!("http://127.0.0.1:{port}"), None) {
        Err(Unavailable::Unreachable { engine, .. }) => assert_eq!(engine, "clickhouse"),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("opened with no server"),
    }
}

/// Do: read SQLite's shell's quote mode. Expect: its types back, a quote doubled inside text, a
/// line end inside text kept, a bare word as text.
#[test]
fn t_g2_4_2_quote_mode_reads_typed() {
    let rows = quoted_rows("1,'a''b',NULL,1.5,X'00ff'\n-2,'two\nlines',true,1e300,''").unwrap();
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Text(b"a'b".to_vec()),
                Value::Null,
                Value::Real(1.5),
                Value::Blob(vec![0, 255]),
            ],
            vec![
                Value::Integer(-2),
                Value::Text(b"two\nlines".to_vec()),
                Value::Text(b"true".to_vec()),
                Value::Real(1e300),
                Value::Text(Vec::new()),
            ],
        ]
    );
    assert!(quoted_rows("'open").is_err());
}

const FILE: &str = "statement ok
CREATE TABLE t(a INTEGER, b TEXT, c REAL)

statement ok
INSERT INTO t VALUES (1, 'one', 1.5), (2, 'two', NULL), (3, NULL, 2.25)

query ITR rowsort
SELECT a, b, c FROM t
----
1
one
1.500
2
two
NULL
3
NULL
2.250

statement error
SELECT * FROM nope

query I nosort
WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c
----
0

query T nosort
SELECT 'after'
----
after
";

/// Do: run a file on SQLite's own shell, its endless query bounded. Expect: where sqlite3 is
/// here, every record agrees but the endless one, past the budget, and then the shell is gone, so
/// the next record isn't reached; where it isn't, the engine is refused as Missing (not passed).
#[test]
fn t_g2_4_2_a_file_runs_on_sqlites_shell_or_is_refused() {
    let mut engine = match open("sqlite", None) {
        Ok(e) => e,
        Err(Unavailable::Missing { engine, .. }) => {
            assert_eq!(engine, "sqlite");
            return;
        }
        Err(other) => panic!("{other}"),
    };
    let mut render = AmphoraRender::new().unwrap();
    let mut seen = Vec::new();
    run_file(
        engine.as_mut(),
        &mut render,
        FILE,
        None,
        None,
        Limits {
            deadline: None,
            record: Some(std::time::Duration::from_millis(300)),
        },
        &mut |r| seen.push(r.outcome),
    )
    .unwrap();
    assert_eq!(seen.len(), 6, "{seen:?}");
    assert!(seen[..4].iter().all(|o| *o == Outcome::Agree), "{seen:?}");
    assert_eq!(seen[4], Outcome::PastBudget);
    assert_eq!(seen[5], Outcome::Unreached);
}

/// Do: make the PostgreSQL-protocol shells. Expect: PostgreSQL and OrioleDB settle by CHECKPOINT;
/// CedarDB, which refuses it as not implemented, runs no settle.
#[test]
fn t_g2_4_2_cedardb_runs_no_checkpoint() {
    let dsn = || "postgres://p@127.0.0.1:1/p".to_owned();
    assert_eq!(
        postgres("postgres", "psql".into(), dsn()).settle,
        Some("CHECKPOINT")
    );
    assert_eq!(
        postgres("orioledb", "psql".into(), dsn()).settle,
        Some("CHECKPOINT")
    );
    assert_eq!(postgres("cedardb", "psql".into(), dsn()).settle, None);
}
