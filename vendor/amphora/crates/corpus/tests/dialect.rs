//! G2 step 4 (T-G2.4.2): each record runs on an engine in that engine's dialect, as the corpus's
//! build translated it once (bench/corpus/dialects.py), and a record with no translation, or one
//! resting on a statement with none, is classified untranslatable, never counted as a disagreement.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]

use std::path::Path;
use std::time::Instant;

use amphora_corpus::amphora::AmphoraRender;
use amphora_corpus::answer::Value;
use amphora_corpus::dialect::{Translated, Translation, of, path};
use amphora_corpus::run::{Engine, Limits, Outcome, Stop, run_file};

/// An engine that keeps what it was given: "ROW ..." answers one row of 1, "REFUSE ..." is
/// refused, anything else runs and answers nothing.
#[derive(Default)]
struct Kept {
    seen: Vec<String>,
}

impl Engine for Kept {
    fn run(
        &mut self,
        sql: &str,
        _rows: usize,
        _until: Option<Instant>,
    ) -> Result<Vec<Vec<Value>>, Stop> {
        self.seen.push(sql.trim().to_owned());
        if sql.starts_with("REFUSE") {
            Err(Stop::Refused("no".into()))
        } else if sql.starts_with("ROW") {
            Ok(vec![vec![Value::Integer(1)]])
        } else {
            Ok(Vec::new())
        }
    }

    fn settle(&mut self, _until: Option<Instant>) -> Result<(), Stop> {
        Ok(())
    }
}

const FILE: &str = "statement ok
CREATE TABLE t(a)

query I nosort
SELECT a FROM t
----
1

statement ok
CREATE TABLE u(b)

query I nosort
SELECT b FROM u
----
1

statement error
SELECT nope
";

/// Do: run a file with a translation that has text for some records, says one has none, and lacks
/// one. Expect: the translated text is what runs; the record with none, the one missing and the
/// refusal after the untranslated statement are untranslatable; nothing disagrees.
#[test]
fn t_g2_4_2_records_run_translated_and_untranslatable_ones_are_classified() {
    let translation = Translation::parse(
        r#"{"line": 1, "sql": "CREATE TABLE t (a INT)"}
{"line": 4, "sql": "ROW t"}
{"line": 9, "untranslatable": "sqlglot can't"}
{"line": 12, "sql": "REFUSE u"}
"#,
    )
    .unwrap();
    let mut engine = Kept::default();
    let mut render = AmphoraRender::new().unwrap();
    let mut seen = Vec::new();
    let counts = run_file(
        &mut engine,
        &mut render,
        FILE,
        None,
        Some(Translated {
            file: &translation,
            fixture: None,
        }),
        Limits::default(),
        &mut |r| seen.push(r.outcome),
    )
    .unwrap();
    assert_eq!(engine.seen, ["CREATE TABLE t (a INT)", "ROW t", "REFUSE u"]);
    assert_eq!(seen[..2], [Outcome::Agree, Outcome::Agree]);
    assert_eq!(seen[2], Outcome::Untranslatable("sqlglot can't".into()));
    assert!(
        matches!(&seen[3], Outcome::Untranslatable(why) if why.contains("line 9")),
        "{seen:?}"
    );
    assert_eq!(
        seen[4],
        Outcome::Untranslatable("the translation has no such record".into())
    );
    assert_eq!(
        (counts.agree, counts.disagree, counts.untranslatable),
        (2, 0, 3)
    );
}

/// Do: run the same file with no translation (an engine of SQLite's dialect). Expect: the records
/// as written, and the refusal SQLite wouldn't make is a disagreement.
#[test]
fn t_g2_4_2_sqlites_dialect_takes_records_as_written() {
    let mut engine = Kept::default();
    let mut render = AmphoraRender::new().unwrap();
    let counts = run_file(
        &mut engine,
        &mut render,
        FILE,
        None,
        None,
        Limits::default(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(engine.seen[0], "CREATE TABLE t(a)");
    assert_eq!(counts.untranslatable, 0);
    // "SELECT a FROM t" answers nothing here where the file wants 1; "SELECT nope" runs.
    assert_eq!(counts.disagree, 3);
}

/// Do: ask each engine's dialect, and where a file's translation lives. Expect: SQLite's dialect
/// engines get none; the PostgreSQL protocol's three share one; the path is beside the pool.
#[test]
fn t_g2_4_2_each_engine_has_its_dialect() {
    let dialects: Vec<_> = amphora_corpus::engines::ENGINES
        .iter()
        .map(|e| of(e))
        .collect();
    assert_eq!(
        dialects,
        [
            None,
            Some("duckdb"),
            None,
            None,
            Some("postgres"),
            Some("postgres"),
            Some("postgres"),
            Some("mysql"),
            Some("clickhouse"),
        ]
    );
    assert_eq!(
        path(Path::new("pool"), "mysql", Path::new("slt/a/b.test")),
        Path::new("pool/translations/mysql/slt/a/b.test.jsonl")
    );
    assert!(Translation::parse(r#"{"line": 1, "sql": "x", "untranslatable": "y"}"#).is_err());
    assert!(Translation::parse(r#"{"line": 1}"#).is_err());
}

/// Do: a statement the engine refuses in its dialect (a table it won't make), then a query on it.
/// Expect: the statement disagrees; the query after it, resting on it, is untranslatable.
#[test]
fn t_g2_4_2_records_resting_on_a_statement_refused_in_the_dialect_are_classified() {
    let translation = Translation::parse(
        r#"{"line": 1, "sql": "REFUSE CREATE"}
{"line": 4, "sql": "SELECT a FROM t"}
"#,
    )
    .unwrap();
    let mut render = AmphoraRender::new().unwrap();
    let mut seen = Vec::new();
    let file = "statement ok\nCREATE TABLE t(a)\n\nquery I nosort\nSELECT a FROM t\n----\n1\n";
    let counts = run_file(
        &mut Kept::default(),
        &mut render,
        file,
        None,
        Some(Translated {
            file: &translation,
            fixture: None,
        }),
        Limits::default(),
        &mut |r| seen.push(r.outcome),
    )
    .unwrap();
    assert_eq!(seen[0], Outcome::Refused("no".into()));
    assert!(
        matches!(&seen[1], Outcome::Untranslatable(why) if why.contains("line 1, refused")),
        "{seen:?}"
    );
    assert_eq!((counts.disagree, counts.untranslatable), (1, 1));
}
