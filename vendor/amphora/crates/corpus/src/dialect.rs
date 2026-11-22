//! Each record in the dialect of the engine it runs on (G2 step 4: "translate each query once per
//! dialect"). The translation is made once, when the corpus is built, by sqlglot
//! (bench/corpus/dialects.py), and written beside the pool: `<pool>/translations/<dialect>/<file>.jsonl`,
//! a JSON line per record of `<pool>/<file>`, `{"line": n, "sql": ...}` or
//! `{"line": n, "untranslatable": why}`. The harness reads it; it never translates as it runs, so a
//! record's time is the engine's alone. A record with no translation is classified as such, never
//! counted as a disagreement.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The dialect an engine is given its records in, or None when it takes SQLite's as written
/// (SQLite, Amphora, and Turso, which speaks SQLite's dialect). PostgreSQL's speaks for OrioleDB
/// and CedarDB, which take PostgreSQL's SQL and protocol (bench/corpus/probe.py's DIALECT).
pub fn of(engine: &str) -> Option<&'static str> {
    match engine {
        "duckdb" => Some("duckdb"),
        "postgres" | "orioledb" | "cedardb" => Some("postgres"),
        "mysql" => Some("mysql"),
        "clickhouse" => Some("clickhouse"),
        _ => None,
    }
}

/// Where `<pool>/<rel>`'s translation into `dialect` is.
pub fn path(pool: &Path, dialect: &str, rel: &Path) -> PathBuf {
    let mut p = pool.join("translations").join(dialect).join(rel);
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".jsonl");
    p.set_file_name(name);
    p
}

/// One file's records in one dialect, by the line each record starts on.
#[derive(Clone, Debug, Default)]
pub struct Translation {
    by_line: HashMap<usize, Result<String, String>>,
}

#[derive(serde::Deserialize)]
struct Line {
    line: usize,
    sql: Option<String>,
    untranslatable: Option<String>,
}

impl Translation {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut by_line = HashMap::new();
        for (n, l) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let l: Line = serde_json::from_str(l)
                .map_err(|e| format!("line {}: {e}", n.saturating_add(1)))?;
            let t = match (l.sql, l.untranslatable) {
                (Some(sql), None) => Ok(sql),
                (None, Some(why)) => Err(why),
                _ => {
                    return Err(format!(
                        "line {}: neither sql nor untranslatable, or both",
                        n.saturating_add(1)
                    ));
                }
            };
            by_line.insert(l.line, t);
        }
        Ok(Self { by_line })
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "{}: {e} (the pool's translation: bench/corpus/dialects.py <pool> makes it)",
                path.display()
            )
        })?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The record at `line` in the dialect, or why it has none.
    pub fn get(&self, line: usize) -> Result<&str, &str> {
        match self.by_line.get(&line) {
            Some(Ok(sql)) => Ok(sql),
            Some(Err(why)) => Err(why),
            None => Err("the translation has no such record"),
        }
    }
}

/// A file's translation and its fixture's, handed to crate::run::run_file.
#[derive(Clone, Copy, Debug)]
pub struct Translated<'a> {
    pub file: &'a Translation,
    pub fixture: Option<&'a Translation>,
}
