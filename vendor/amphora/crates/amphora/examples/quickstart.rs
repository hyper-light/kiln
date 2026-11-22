//! T-T1.13.2: a working Amphora from a fresh clone (README.md, "A working Amphora"). Opens a
//! database file (made if it isn't there; its folder must be), gives it a table once, writes a row
//! and reads every row back, in one atomic batch.
//!
//! `cargo run -p amphora --example quickstart -- <a database file's path>`

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora_api::{Migration, Statement, Value};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            // A closed stderr leaves only the status to say it failed.
            let _ = writeln!(std::io::stderr(), "quickstart: {why}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: quickstart <a database file's path>")?;
    // Each call's deadline: how long a person at a terminal waits, not a tuning value. A call
    // past it is refused, typed (`Refusal`), never left hanging.
    let until = || {
        Instant::now()
            .checked_add(Duration::from_secs(30))
            .ok_or_else(|| "the clock can't reach the deadline".to_owned())
    };
    let db = Database::open(&path, until()?).map_err(|e| e.to_string())?;
    // Applied once by name: a second run finds it applied and goes on.
    db.migrate(
        Migration {
            name: "001-notes".into(),
            sql: "CREATE TABLE notes(id INTEGER PRIMARY KEY, text TEXT NOT NULL)".into(),
        },
        until()?,
    )
    .map_err(|e| e.to_string())?;
    let results = db
        .execute(
            vec![
                Statement {
                    sql: "INSERT INTO notes(text) VALUES (?1)".into(),
                    params: vec![Value::Text("hello from Amphora".into())],
                },
                Statement {
                    sql: "SELECT id, text FROM notes ORDER BY id".into(),
                    params: Vec::new(),
                },
            ],
            until()?,
        )
        .map_err(|e| e.to_string())?;
    let mut out = std::io::stdout().lock();
    for row in results.iter().flat_map(|r| r.rows.iter()) {
        writeln!(out, "{row:?}").map_err(|e| e.to_string())?;
    }
    Ok(())
}
