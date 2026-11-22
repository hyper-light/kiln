//! Amphora as a corpus engine: its engine crate on a database of its own per file, opened as SQLite
//! opens one (no store settings: the corpus's answers are a default SQLite connection's), and the
//! renderer of REALs, SQLite's text through Amphora's own port of it.

use std::num::NonZero;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use amphora_engine::{
    ConnectOpts, Connection, Database, DatabaseOpts, IO, LimboError, MemoryIO, Numeric,
    OpenOptions, SqliteDialect, Statement, StepResult,
};

use crate::answer::{Render, Value};
use crate::run::{Engine, Stop};

/// Names every in-memory database the process opens apart (the engine keeps databases by name).
static OPENED: AtomicU64 = AtomicU64::new(0);

fn fresh(io: &Arc<dyn IO>) -> Result<(Arc<Database>, Arc<Connection>), String> {
    let n = OPENED.fetch_add(1, Ordering::Relaxed);
    let name = format!("corpus-{}-{n}.db", std::process::id());
    let db = Database::open(
        io.clone(),
        &name,
        OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new()),
    )
    .map_err(|e| e.to_string())?;
    let conn = db.connect(ConnectOpts::new()).map_err(|e| e.to_string())?;
    Ok((db, conn))
}

fn value(v: &amphora_engine::Value) -> Value {
    use amphora_engine::Value as V;
    match v {
        V::Null => Value::Null,
        V::Numeric(Numeric::Integer(i)) => Value::Integer(*i),
        V::Numeric(Numeric::Float(f)) => Value::Real(f64::from(*f)),
        V::Text(_) => Value::Text(v.to_text().unwrap_or_default().as_bytes().to_vec()),
        V::Blob(_) => Value::Blob(v.to_blob().unwrap_or_default().to_vec()),
    }
}

fn stop(e: LimboError) -> Stop {
    match e {
        LimboError::Interrupt => Stop::PastBudget,
        e => Stop::Refused(e.to_string()),
    }
}

/// Steps `stmt` to its end, keeping at most `keep` rows.
fn drain(io: &dyn IO, stmt: &mut Statement, keep: usize) -> Result<Vec<Vec<Value>>, Stop> {
    let mut rows = Vec::new();
    loop {
        match stmt.step().map_err(stop)? {
            StepResult::Row => {
                if rows.len() < keep
                    && let Some(r) = stmt.row()
                {
                    rows.push(r.get_values().map(value).collect());
                }
            }
            StepResult::Done => return Ok(rows),
            StepResult::Interrupt => return Err(Stop::PastBudget),
            StepResult::IO | StepResult::Yield => io.step().map_err(stop)?,
            other => return Err(Stop::Refused(format!("step: {other:?}"))),
        }
    }
}

/// Amphora on an in-memory database: the gate's engine.
pub struct Amphora {
    io: Arc<dyn IO>,
    conn: Arc<Connection>,
    _db: Arc<Database>,
}

impl Amphora {
    pub fn in_memory() -> Result<Self, String> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let (db, conn) = fresh(&io)?;
        Ok(Self { io, conn, _db: db })
    }
}

impl Engine for Amphora {
    /// The bound is the engine's own query deadline: its VM checks it as it steps and interrupts
    /// the statement (vdbe `maybe_request_interrupt`), so a record that can't finish ends there.
    fn run(
        &mut self,
        sql: &str,
        keep: usize,
        until: Option<Instant>,
    ) -> Result<Vec<Vec<Value>>, Stop> {
        let mut stmt = self.conn.prepare(sql).map_err(stop)?;
        if let Some(u) = until {
            stmt.set_query_timeout_override(Some(Some(
                u.saturating_duration_since(Instant::now()),
            )));
        }
        drain(self.io.as_ref(), &mut stmt, keep)
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)`: every frame copied and the log cut to nothing, or the
    /// row it answers says busy (its first column non-zero).
    fn settle(&mut self, until: Option<Instant>) -> Result<(), Stop> {
        let rows = self.run("PRAGMA wal_checkpoint(TRUNCATE)", 1, until)?;
        match rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(0)) | None => Ok(()),
            Some(other) => Err(Stop::Refused(format!(
                "the checkpoint answered busy: {other:?}"
            ))),
        }
    }
}

/// REALs rendered by Amphora's port of SQLite's (printf's etFLOAT for R, `%!.17g` for T), each
/// through a statement prepared once.
pub struct AmphoraRender {
    io: Arc<dyn IO>,
    real3: Statement,
    text: Statement,
    _conn: Arc<Connection>,
    _db: Arc<Database>,
}

impl AmphoraRender {
    pub fn new() -> Result<Self, String> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let (db, conn) = fresh(&io)?;
        let real3 = conn
            .prepare("SELECT printf('%.3f', ?1)")
            .map_err(|e| e.to_string())?;
        let text = conn
            .prepare("SELECT CAST(?1 AS TEXT)")
            .map_err(|e| e.to_string())?;
        Ok(Self {
            io,
            real3,
            text,
            _conn: conn,
            _db: db,
        })
    }

    fn one(io: &dyn IO, stmt: &mut Statement, r: f64) -> Result<String, String> {
        stmt.reset().map_err(|e| e.to_string())?;
        let first = NonZero::new(1usize).ok_or("parameter 0")?;
        stmt.bind_at(first, amphora_engine::Value::from_f64(r))
            .map_err(|e| e.to_string())?;
        let rows = drain(io, stmt, 1).map_err(|e| format!("{e:?}"))?;
        match rows.first().and_then(|row| row.first()) {
            Some(Value::Text(t)) => String::from_utf8(t.clone()).map_err(|e| e.to_string()),
            other => Err(format!("rendering {r}: {other:?}")),
        }
    }
}

impl Render for AmphoraRender {
    fn real3(&mut self, r: f64) -> Result<String, String> {
        Self::one(self.io.as_ref(), &mut self.real3, r)
    }

    fn real_text(&mut self, r: f64) -> Result<String, String> {
        Self::one(self.io.as_ref(), &mut self.text, r)
    }
}
