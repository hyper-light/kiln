//! Every disagreement with SQLite's answer, classified (G2 step 4): an Amphora bug, a competitor's
//! bug, a dialect difference, or a translation gap. SQLite is the oracle for its own semantics;
//! PostgreSQL for standard SQL, so an engine that answers as PostgreSQL does where SQLite answers
//! otherwise differs in dialect, not by a bug.
//!
//! The rules, first that fits:
//! - an engine that speaks SQLite's dialect (Amphora, Turso, SQLite's other builds) takes the
//!   record as written: any difference is its bug;
//! - no translation into the engine's dialect: a translation gap;
//! - the engine refuses what SQLite runs: a dialect difference when the support matrix says the
//!   engine lacks a feature the record uses (bench/corpus/matrix.json), else a translation gap (the
//!   engine has every feature, so the translation said it wrong);
//! - the engine runs what SQLite refuses: a dialect difference (it accepts more);
//! - a different answer: a dialect difference when it is PostgreSQL's answer (standard SQL), when
//!   only NULLs' place in the order differs (SQL leaves it to the implementation: SQL:2016 Part 2,
//!   10.10, "<null ordering>"; SQLite puts them first ascending, PostgreSQL, DuckDB and others
//!   last), when integer division gave a fraction (MySQL's `/` always does; its `DIV` is SQLite's
//!   `/`), when the same values come in another order, or when the record leans on what SQLite does
//!   its own way (types it lacks, dates and times, its catalog, plans: bench/corpus/translated.py's
//!   triage); else the competitor's bug.

use serde::Serialize;

/// The four classes the step names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    AmphoraBug,
    CompetitorBug,
    Dialect(Why),
    TranslationGap,
}

/// Why a difference is the dialect's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    /// The matrix says the engine lacks a feature the record uses.
    Unsupported,
    /// It runs what SQLite refuses.
    Accepts,
    /// Its answer is PostgreSQL's.
    StandardSql,
    NullOrder,
    IntegerDivision,
    Order,
    Type,
    DateTime,
    Catalog,
    Plan,
}

/// What became of a record on an engine, against SQLite's expectation.
#[derive(Clone, Debug, PartialEq)]
pub enum Difference {
    /// No translation into the engine's dialect.
    Untranslatable,
    /// The engine refused what SQLite runs.
    Refused,
    /// The engine ran what SQLite refuses.
    Accepted,
    /// A different answer: SQLite's values and the engine's, rendered, in the record's sort.
    Answer { want: Vec<String>, got: Vec<String> },
}

/// What the classifier knows of the record and the engine.
pub struct Facts<'a> {
    /// The engine is Amphora.
    pub amphora: bool,
    /// The engine takes SQLite's dialect, the record as written.
    pub sqlite_dialect: bool,
    /// The record's SQL, as SQLite runs it.
    pub sql: &'a str,
    /// Inventory entries the record uses that the matrix says this engine lacks.
    pub lacks: &'a [String],
    /// The engine's answer is PostgreSQL's for the same record (None: not known, or the engine is
    /// PostgreSQL).
    pub as_postgres: Option<bool>,
}

pub fn classify(d: &Difference, f: &Facts<'_>) -> Class {
    if f.sqlite_dialect {
        return if f.amphora {
            Class::AmphoraBug
        } else {
            Class::CompetitorBug
        };
    }
    match d {
        Difference::Untranslatable => Class::TranslationGap,
        Difference::Refused if !f.lacks.is_empty() => Class::Dialect(Why::Unsupported),
        Difference::Refused => Class::TranslationGap,
        Difference::Accepted => Class::Dialect(Why::Accepts),
        Difference::Answer { want, got } => {
            if f.as_postgres == Some(true) {
                Class::Dialect(Why::StandardSql)
            } else if null_order(want, got) {
                Class::Dialect(Why::NullOrder)
            } else if f.sql.contains('/') && integer_division(want, got) {
                Class::Dialect(Why::IntegerDivision)
            } else if same_values(want, got) {
                Class::Dialect(Why::Order)
            } else if let Some(why) = leans_on_sqlite(f.sql) {
                Class::Dialect(why)
            } else {
                Class::CompetitorBug
            }
        }
    }
}

/// The values hold the same multiset.
fn same_values(want: &[String], got: &[String]) -> bool {
    let (mut a, mut b) = (want.to_vec(), got.to_vec());
    a.sort();
    b.sort();
    a == b
}

/// `got` is `want` with its NULLs' rows moved to the other end. Rows aren't marked in the flat
/// values, so a NULL counts at whatever position it holds: SQLite's NULLs first become the other
/// engine's NULLs last, and the other way round (a descending order).
fn null_order(want: &[String], got: &[String]) -> bool {
    if want == got || want.len() != got.len() || !want.iter().any(|v| v == "NULL") {
        return false;
    }
    let (nulls, rest): (Vec<&String>, Vec<&String>) = want.iter().partition(|v| *v == "NULL");
    let last: Vec<&String> = rest.iter().copied().chain(nulls.iter().copied()).collect();
    let first: Vec<&String> = nulls.iter().copied().chain(rest.iter().copied()).collect();
    let got: Vec<&String> = got.iter().collect();
    got == last || got == first
}

/// Every value that differs is SQLite's integer quotient of a fraction the engine kept.
fn integer_division(want: &[String], got: &[String]) -> bool {
    if want.len() != got.len() {
        return false;
    }
    let mut any = false;
    for (w, g) in want.iter().zip(got) {
        if w == g {
            continue;
        }
        let (Ok(w), Ok(g)) = (w.parse::<f64>(), g.parse::<f64>()) else {
            return false;
        };
        if w.fract() != 0.0 || g.fract() == 0.0 || g.trunc() != w {
            return false;
        }
        any = true;
    }
    any
}

/// What SQLite does its own way that the record leans on (bench/corpus/translated.py's WHY_*
/// patterns, the corpus's triage of the translated sources), by the words of its SQL.
fn leans_on_sqlite(sql: &str) -> Option<Why> {
    let upper = sql.to_ascii_uppercase();
    let words: Vec<&str> = upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();
    let has = |names: &[&str]| words.iter().any(|w| names.contains(w));
    if words.first() == Some(&"EXPLAIN") {
        return Some(Why::Plan);
    }
    if has(&[
        "PRAGMA",
        "SQLITE_SCHEMA",
        "SQLITE_MASTER",
        "INFORMATION_SCHEMA",
        "PG_CATALOG",
    ]) {
        return Some(Why::Catalog);
    }
    if has(&[
        "HUGEINT",
        "UBIGINT",
        "DECIMAL",
        "NUMERIC",
        "INTERVAL",
        "UUID",
        "BIT",
        "ENUM",
        "TIMESTAMPTZ",
        "BOOLEAN",
        "BOOL",
        "TINYINT",
        "SMALLINT",
    ]) {
        return Some(Why::Type);
    }
    if has(&[
        "DATE",
        "TIME",
        "DATETIME",
        "JULIANDAY",
        "UNIXEPOCH",
        "STRFTIME",
        "TIMEDIFF",
        "TIMESTAMP",
        "EXTRACT",
        "NOW",
    ]) {
        return Some(Why::DateTime);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(sql: &'a str, lacks: &'a [String]) -> Facts<'a> {
        Facts {
            amphora: false,
            sqlite_dialect: false,
            sql,
            lacks,
            as_postgres: None,
        }
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| (*x).to_owned()).collect()
    }

    /// T-G2.4.1: a known dialect difference. Do: classify PostgreSQL's NULLs last against SQLite's
    /// first, ascending and descending, and MySQL's 7/2 = 3.5 against SQLite's 3. Expect: dialect
    /// differences (null order, integer division), never a bug.
    #[test]
    fn t_g2_4_1_known_dialect_differences_are_dialect_not_bugs() {
        let asc = Difference::Answer {
            want: v(&["NULL", "1", "2"]),
            got: v(&["1", "2", "NULL"]),
        };
        assert_eq!(
            classify(&asc, &facts("SELECT a FROM t ORDER BY a", &[])),
            Class::Dialect(Why::NullOrder)
        );
        let desc = Difference::Answer {
            want: v(&["2", "1", "NULL"]),
            got: v(&["NULL", "2", "1"]),
        };
        assert_eq!(
            classify(&desc, &facts("SELECT a FROM t ORDER BY a DESC", &[])),
            Class::Dialect(Why::NullOrder)
        );
        let div = Difference::Answer {
            want: v(&["3.000", "2.000"]),
            got: v(&["3.500", "2.000"]),
        };
        assert_eq!(
            classify(&div, &facts("SELECT 7/2, 4/2", &[])),
            Class::Dialect(Why::IntegerDivision)
        );
    }

    /// Do: the same differences on Amphora and on Turso, which take SQLite's dialect. Expect:
    /// Amphora's bug and the competitor's bug.
    #[test]
    fn a_sqlite_dialect_engine_owns_every_difference() {
        let asc = Difference::Answer {
            want: v(&["NULL", "1"]),
            got: v(&["1", "NULL"]),
        };
        let mut f = facts("SELECT a FROM t ORDER BY a", &[]);
        f.sqlite_dialect = true;
        f.amphora = true;
        assert_eq!(classify(&asc, &f), Class::AmphoraBug);
        f.amphora = false;
        assert_eq!(classify(&Difference::Refused, &f), Class::CompetitorBug);
    }

    /// Do: refusals with and without a feature the matrix says the engine lacks, a missing
    /// translation, an accepted error, PostgreSQL's answer, and a wrong value nothing explains.
    /// Expect: unsupported, translation gap twice, accepts, standard SQL, the competitor's bug.
    #[test]
    fn the_rest_of_the_rules() {
        let lacks = vec!["function.json:json_patch".to_owned()];
        assert_eq!(
            classify(
                &Difference::Refused,
                &facts("SELECT json_patch(a, b)", &lacks)
            ),
            Class::Dialect(Why::Unsupported)
        );
        assert_eq!(
            classify(&Difference::Refused, &facts("SELECT abs(a)", &[])),
            Class::TranslationGap
        );
        assert_eq!(
            classify(&Difference::Untranslatable, &facts("SELECT 1", &[])),
            Class::TranslationGap
        );
        assert_eq!(
            classify(&Difference::Accepted, &facts("SELECT 1", &[])),
            Class::Dialect(Why::Accepts)
        );
        let wrong = Difference::Answer {
            want: v(&["1"]),
            got: v(&["0"]),
        };
        let mut f = facts("SELECT 'a' < 'B'", &[]);
        f.as_postgres = Some(true);
        assert_eq!(classify(&wrong, &f), Class::Dialect(Why::StandardSql));
        f.as_postgres = Some(false);
        assert_eq!(classify(&wrong, &f), Class::CompetitorBug);
        let order = Difference::Answer {
            want: v(&["1", "2"]),
            got: v(&["2", "1"]),
        };
        assert_eq!(
            classify(&order, &facts("SELECT a FROM t", &[])),
            Class::Dialect(Why::Order)
        );
        assert_eq!(
            classify(&wrong, &facts("SELECT date('now') > '2000'", &[])),
            Class::Dialect(Why::DateTime)
        );
    }
}
