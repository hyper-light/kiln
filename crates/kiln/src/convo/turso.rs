//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store"), vendored in
//! `vendor/turso`. One connection, owned by the store's writer thread; every statement is driven
//! to its end on that thread (`PlatformIO` does its I/O inline, so `step` waits on nothing).

use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;

use turso_core::{Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, Row as TRow, SqliteDialect, Statement, StepResult, Value};

use super::rows::{EventRow, Row};
use super::sql;
use super::store::{Engine, Kept, Query, SEARCH_MAX};

/// How often a statement that finds the database busy tries again, and how long it waits between.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// A Turso database file.
pub struct Turso {
    io: Arc<dyn IO>,
    _db: Arc<Database>,
    conn: Arc<Connection>,
}

fn fail(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// The `n`th parameter's place (1-based).
fn at(n: usize) -> anyhow::Result<NonZero<usize>> {
    NonZero::new(n).ok_or_else(|| anyhow::anyhow!("SQL parameters count from 1"))
}

fn text(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::from_text(s.to_owned()))
}

fn int(v: i64) -> Value {
    Value::from_i64(v)
}

fn seq(v: u64) -> Value {
    Value::from_i64(i64::try_from(v).unwrap_or(i64::MAX))
}

impl Turso {
    /// The database at `path`, made and migrated as needed.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("the store's path isn't UTF-8: {}", path.display()))?;
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().map_err(fail)?);
        let db = Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new())).map_err(fail)?;
        let conn = db.connect(ConnectOpts::new()).map_err(fail)?;
        let mut t = Self { io, _db: db, conn };
        t.migrate()?;
        Ok(t)
    }

    /// Statements that answer nothing, one after another.
    fn exec(&self, sql: &str) -> anyhow::Result<()> {
        for one in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            self.conn.execute(one).map_err(|e| anyhow::anyhow!("{e} (in: {})", one.lines().next().unwrap_or_default()))?;
        }
        Ok(())
    }

    fn prepare(&self, sql: &str) -> anyhow::Result<Statement> {
        self.conn.prepare(sql).map_err(fail)
    }

    /// Run `stmt` with `params`, handing each row to `each`.
    fn run(&self, stmt: &mut Statement, params: &[Value], mut each: impl FnMut(&TRow) -> anyhow::Result<()>) -> anyhow::Result<()> {
        stmt.reset().map_err(fail)?;
        for (i, p) in params.iter().enumerate() {
            stmt.bind_at(at(i + 1)?, p.clone()).map_err(fail)?;
        }
        let mut busy = 0;
        loop {
            match stmt.step().map_err(fail)? {
                StepResult::Row => {
                    if let Some(row) = stmt.row() {
                        each(row)?;
                    }
                }
                StepResult::Done => return Ok(()),
                StepResult::IO | StepResult::Yield => self.io.step().map_err(fail)?,
                StepResult::Sleep { duration } => {
                    std::thread::sleep(duration);
                    self.io.step().map_err(fail)?;
                }
                StepResult::Busy => {
                    busy += 1;
                    if busy > BUSY_TRIES {
                        anyhow::bail!("the conversation store stayed busy");
                    }
                    std::thread::sleep(BUSY_WAIT);
                }
                StepResult::Interrupt => anyhow::bail!("a statement on the conversation store was interrupted"),
            }
        }
    }

    fn migrate(&mut self) -> anyhow::Result<()> {
        self.exec(sql::META)?;
        let mut get = self.prepare(sql::SCHEMA_GET)?;
        let mut have = 0usize;
        self.run(&mut get, &[], |r| {
            have = r.get::<String>(0).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            Ok(())
        })?;
        for (i, m) in sql::MIGRATIONS.iter().enumerate().skip(have) {
            self.exec("BEGIN")?;
            let done = self.exec(m).and_then(|()| {
                let mut set = self.prepare(sql::SCHEMA_SET)?;
                self.run(&mut set, &[Value::from_text((i + 1).to_string())], |_| Ok(()))
            });
            match done {
                Ok(()) => self.exec("COMMIT")?,
                Err(e) => {
                    let _ = self.exec("ROLLBACK");
                    return Err(e.context(format!("migrating the conversation store to schema {}", i + 1)));
                }
            }
        }
        Ok(())
    }

    fn event_rows(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<EventRow>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = Vec::new();
        self.run(&mut stmt, params, |r| {
            out.push(event_of(r)?);
            Ok(())
        })?;
        Ok(out)
    }
}

/// An event row as `sql::EVENTS_*` select it.
fn event_of(r: &TRow) -> anyhow::Result<EventRow> {
    let s = |i: usize| r.get::<String>(i).ok();
    Ok(EventRow {
        session: s(0).unwrap_or_default(),
        id: s(1).unwrap_or_default(),
        seq: r.get::<i64>(2).ok().and_then(|n| u64::try_from(n).ok()).unwrap_or(0),
        at: r.get::<i64>(3).unwrap_or(0),
        parent: s(4),
        by: s(5),
        kind: s(6).unwrap_or_default(),
        level: s(7),
        tool: s(8),
        status: s(9),
        text: s(10).unwrap_or_default(),
        body: s(11).ok_or_else(|| anyhow::anyhow!("an event without its body"))?,
    })
}

/// `word` as a LIKE pattern that matches it anywhere, its wildcards taken literally.
fn like(word: &str) -> String {
    let mut p = String::with_capacity(word.len() + 2);
    p.push('%');
    for c in word.chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

impl Engine for Turso {
    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.exec("BEGIN")?;
        let written = (|| -> anyhow::Result<()> {
            let mut upsert_session = self.prepare(sql::SESSION_UPSERT)?;
            let mut upsert_event = self.prepare(sql::EVENT_UPSERT)?;
            let mut reset = self.prepare(sql::EVENTS_RESET)?;
            let mut close = self.prepare(sql::SESSION_CLOSE)?;
            for row in rows {
                match row {
                    Row::Session { session, info, meta, state, changes, at } => self.run(
                        &mut upsert_session,
                        &[Value::from_text(session.clone()), text(info.as_deref()), text(meta.as_deref()), text(state.as_deref()), text(changes.as_deref()), int(*at)],
                        |_| Ok(()),
                    )?,
                    Row::Event(e) => self.run(
                        &mut upsert_event,
                        &[
                            Value::from_text(e.session.clone()),
                            Value::from_text(e.id.clone()),
                            seq(e.seq),
                            int(e.at),
                            text(e.parent.as_deref()),
                            text(e.by.as_deref()),
                            Value::from_text(e.kind.clone()),
                            text(e.level.as_deref()),
                            text(e.tool.as_deref()),
                            text(e.status.as_deref()),
                            Value::from_text(e.text.clone()),
                            Value::from_text(e.body.clone()),
                        ],
                        |_| Ok(()),
                    )?,
                    Row::Reset { session, .. } => self.run(&mut reset, &[Value::from_text(session.clone())], |_| Ok(()))?,
                    Row::Closed { session, at } => self.run(&mut close, &[Value::from_text(session.clone()), int(*at)], |_| Ok(()))?,
                }
            }
            Ok(())
        })();
        match written {
            Ok(()) => self.exec("COMMIT"),
            Err(e) => {
                let _ = self.exec("ROLLBACK");
                Err(e)
            }
        }
    }

    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let mut get = self.prepare(sql::SESSION_GET)?;
        let mut kept: Option<Kept> = None;
        self.run(&mut get, &[Value::from_text(session.to_owned())], |r| {
            let s = |i: usize| r.get::<String>(i).ok();
            kept = Some(Kept { info: s(0), meta: s(1), state: s(2), changes: s(3), updated_at: r.get::<i64>(4).unwrap_or(0), closed_at: r.get::<i64>(5).ok(), events: Vec::new() });
            Ok(())
        })?;
        let mut events = self.event_rows(sql::EVENTS_LAST, &[Value::from_text(session.to_owned()), int(i64::try_from(last).unwrap_or(i64::MAX))])?;
        if kept.is_none() && events.is_empty() {
            return Ok(None);
        }
        events.reverse();
        let mut k = kept.unwrap_or_default();
        k.events = events;
        Ok(Some(k))
    }

    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let limit = int(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut rows = match before {
            Some(b) => self.event_rows(sql::EVENTS_BEFORE, &[Value::from_text(session.to_owned()), seq(b), limit])?,
            None => self.event_rows(sql::EVENTS_LAST, &[Value::from_text(session.to_owned()), limit])?,
        };
        rows.reverse();
        Ok(rows)
    }

    fn search(&mut self, q: &Query) -> anyhow::Result<Vec<EventRow>> {
        let mut sql = String::from("SELECT session, id, seq, at, parent, by, kind, level, tool, status, text, body FROM events WHERE 1 = 1");
        let mut params: Vec<Value> = Vec::new();
        let mut and = |clause: &str, v: Value, sql: &mut String| {
            params.push(v);
            sql.push_str(&format!(" AND {clause} ?{}", params.len()));
        };
        for (clause, v) in [("session =", &q.session), ("kind =", &q.kind), ("tool =", &q.tool), ("status =", &q.status)] {
            if let Some(v) = v {
                and(clause, Value::from_text(v.clone()), &mut sql);
            }
        }
        if let Some(s) = q.since {
            and("at >=", int(s), &mut sql);
        }
        if let Some(u) = q.until {
            and("at <", int(u), &mut sql);
        }
        for word in q.q.split_whitespace() {
            and("text LIKE", Value::from_text(like(word)), &mut sql);
            sql.push_str(" ESCAPE '\\'");
        }
        let limit = q.limit.unwrap_or(100).min(SEARCH_MAX);
        params.push(int(i64::try_from(limit).unwrap_or(100)));
        sql.push_str(&format!(" ORDER BY at DESC, seq DESC LIMIT ?{}", params.len()));
        self.event_rows(&sql, &params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event, State};

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    #[test]
    fn a_turso_store_keeps_reads_back_and_reopens() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        {
            let store = super::super::store::Store::start(Box::new(Turso::open(&path)?))?;
            store.send("a1", &Update::State { state: Box::new(State { title: "the parser".into(), ..State::default() }) });
            for i in 1..=2_000u64 {
                store.send("a1", &said(&format!("m{i}"), i, &format!("reply {i} about 100% of_the parser")));
            }
            store.send("a1", &said("m7", 7, "reply seven, revised"));
            store.send("a2", &said("x1", 1, "unrelated"));
            store.send("a3", &said("y1", 1, "one"));
            store.send("a3", &Update::Reset);
            store.send("a3", &said("y2", 2, "two"));
            store.sync()?;
            let k = store.kept("a1", 10)?.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
            assert_eq!(k.events.len(), 10);
            assert_eq!(k.events.first().map(|e| e.id.as_str()), Some("m1991"));
            assert!(k.state.as_deref().is_some_and(|s| s.contains("the parser")));
            let page = store.events("a1", Some(1991), 3)?;
            assert_eq!(page.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1988, 1989, 1990]);
            assert_eq!(store.kept("a3", 10)?.map(|k| k.events.into_iter().map(|e| e.id).collect::<Vec<_>>()), Some(vec!["y2".to_owned()]));
            // Words, any case; wildcards taken literally; newest first.
            let hits = store.search(Query { q: "REVISED seven".into(), ..Query::default() })?;
            assert_eq!(hits.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m7"]);
            let literal = store.search(Query { q: "100%".into(), limit: Some(5), ..Query::default() })?;
            assert_eq!(literal.len(), 5);
            assert!(literal.first().is_some_and(|e| e.id == "m2000"));
            let none = store.search(Query { q: "1000%".into(), ..Query::default() })?;
            assert!(none.is_empty());
            let scoped = store.search(Query { q: "unrelated".into(), session: Some("a1".into()), ..Query::default() })?;
            assert!(scoped.is_empty());
        }
        // Reopened: everything is still there, and the schema isn't made twice.
        let store = super::super::store::Store::start(Box::new(Turso::open(&path)?))?;
        let k = store.kept("a1", 5_000)?.ok_or_else(|| anyhow::anyhow!("nothing kept after reopening"))?;
        assert_eq!(k.events.len(), 2_000);
        assert!(k.events.iter().any(|e| e.id == "m7" && e.text == "reply seven, revised"));
        Ok(())
    }
}
