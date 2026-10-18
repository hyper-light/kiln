//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store"), vendored in
//! `vendor/turso`. One connection, owned by the store's writer thread; every statement is driven
//! to its end on that thread (`PlatformIO` does its I/O inline, so `step` waits on nothing).

use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;

use turso_core::{Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, Row as TRow, SqliteDialect, Statement, StepResult, Value};

use std::collections::HashMap;

use super::rows::{EventRow, RecordRow, Row};
use super::sql;
use super::store::{Change, Changes, Engine, Kept, Mark, Query, SEARCH_MAX, Took};

/// How often a statement that finds the database busy tries again, and how long it waits between.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// Rows of each kind a feed reads at once.
const FEED_PAGE: usize = 512;

/// A Turso database file.
pub struct Turso {
    io: Arc<dyn IO>,
    _db: Arc<Database>,
    conn: Arc<Connection>,
    /// Who the store is to its peers.
    id: String,
    /// The last number given to a write.
    rev: u64,
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
        Self::open_at(path, sql::MIGRATIONS.len())
    }

    /// The database at `path`, migrated as far as schema `schema` (an older kiln's, in tests).
    fn open_at(path: &Path, schema: usize) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("the store's path isn't UTF-8: {}", path.display()))?;
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().map_err(fail)?);
        let db = Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new())).map_err(fail)?;
        let conn = db.connect(ConnectOpts::new()).map_err(fail)?;
        let mut t = Self { io, _db: db, conn, id: String::new(), rev: 0 };
        t.migrate(schema)?;
        if schema < 2 {
            return Ok(t);
        }
        t.id = match t.meta("id")? {
            Some(id) => id,
            None => {
                let id = crate::util::new_id();
                t.set_meta("id", &id)?;
                id
            }
        };
        let mut rev = t.meta("rev")?.and_then(|r| r.parse::<u64>().ok()).unwrap_or(0);
        for q in sql::REV_MAX {
            rev = rev.max(t.one_int(q, &[])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0));
        }
        t.rev = rev;
        Ok(t)
    }

    fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self.prepare(sql::META_GET)?;
        let mut out = None;
        self.run(&mut stmt, &[Value::from_text(key.to_owned())], |r| {
            out = r.get::<String>(0).ok();
            Ok(())
        })?;
        Ok(out)
    }

    fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut stmt = self.prepare(sql::META_SET)?;
        self.run(&mut stmt, &[Value::from_text(key.to_owned()), Value::from_text(value.to_owned())], |_| Ok(()))
    }

    /// The first column of the first row a query answers, as a number (None: no row, or NULL).
    fn one_int(&self, sql: &str, params: &[Value]) -> anyhow::Result<Option<i64>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = None;
        self.run(&mut stmt, params, |r| {
            if out.is_none() {
                out = r.get::<i64>(0).ok();
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// `f` in one transaction: committed when it returns Ok, rolled back otherwise. The store's
    /// last number is kept with what it wrote.
    fn transaction<T>(&mut self, f: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let before = self.rev;
        self.exec("BEGIN")?;
        let done = f(self).and_then(|v| {
            if self.rev != before {
                self.set_meta("rev", &self.rev.to_string())?;
            }
            Ok(v)
        });
        match done {
            Ok(v) => {
                self.exec("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.exec("ROLLBACK");
                self.rev = before;
                Err(e)
            }
        }
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

    fn migrate(&mut self, schema: usize) -> anyhow::Result<()> {
        self.exec(sql::META)?;
        let mut get = self.prepare(sql::SCHEMA_GET)?;
        let mut have = 0usize;
        self.run(&mut get, &[], |r| {
            have = r.get::<String>(0).ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            Ok(())
        })?;
        for (i, m) in sql::MIGRATIONS.iter().enumerate().take(schema).skip(have) {
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

/// The statements a transaction writes with, prepared once for it.
struct Writes {
    session: Statement,
    open: Statement,
    close: Statement,
    event: Statement,
    reset: Statement,
    dropped: Statement,
    record: Statement,
    records_reset: Statement,
}

fn txt(s: &str) -> Value {
    Value::from_text(s.to_owned())
}

impl Turso {
    fn writes(&self) -> anyhow::Result<Writes> {
        Ok(Writes {
            session: self.prepare(sql::SESSION_UPSERT)?,
            open: self.prepare(sql::SESSION_OPEN)?,
            close: self.prepare(sql::SESSION_CLOSE)?,
            event: self.prepare(sql::EVENT_UPSERT)?,
            reset: self.prepare(sql::EVENTS_RESET)?,
            dropped: self.prepare(sql::DROP_ADD)?,
            record: self.prepare(sql::RECORD_UPSERT)?,
            records_reset: self.prepare(sql::RECORDS_RESET)?,
        })
    }

    /// One row, as write number `rev`.
    fn write(&self, w: &mut Writes, row: &Row, rev: u64) -> anyhow::Result<()> {
        let r = seq(rev);
        let none = |_: &TRow| Ok(());
        match row {
            Row::Session { session, info, meta, state, changes, epoch, open, at } => {
                self.run(
                    &mut w.session,
                    &[txt(session), text(info.as_deref()), text(meta.as_deref()), text(state.as_deref()), text(changes.as_deref()), int(*at), seq(epoch.unwrap_or(0)), r],
                    none,
                )?;
                if *open {
                    self.run(&mut w.open, &[txt(session)], none)?;
                }
            }
            Row::Event(e) => self.run(
                &mut w.event,
                &[
                    txt(&e.session),
                    txt(&e.id),
                    seq(e.seq),
                    int(e.at),
                    text(e.parent.as_deref()),
                    text(e.by.as_deref()),
                    txt(&e.kind),
                    text(e.level.as_deref()),
                    text(e.tool.as_deref()),
                    text(e.status.as_deref()),
                    txt(&e.text),
                    txt(&e.body),
                    r,
                ],
                none,
            )?,
            Row::Reset { session, from, at } => {
                self.run(&mut w.reset, &[txt(session), seq(from.unwrap_or(0))], none)?;
                self.run(&mut w.dropped, &[r, txt(session), txt("events"), Value::Null, from.map_or(Value::Null, seq), int(*at)], none)?;
            }
            Row::Closed { session, at } => self.run(&mut w.close, &[txt(session), int(*at), r], none)?,
            Row::Record(rec) => self.run(&mut w.record, &[txt(&rec.session), txt(&rec.source), seq(rec.n), seq(rec.end), txt(&rec.line), r], none)?,
            Row::RecordReset { session, source, at } => {
                self.run(&mut w.records_reset, &[txt(session), txt(source)], none)?;
                self.run(&mut w.dropped, &[r, txt(session), txt("record"), txt(source), Value::Null, int(*at)], none)?;
            }
        }
        Ok(())
    }

    /// The epoch this store has for `session` (0: none yet).
    fn epoch_of(&self, session: &str) -> anyhow::Result<u64> {
        Ok(self.one_int(sql::SESSION_EPOCH, &[txt(session)])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    /// Rows of one kind after `from` (a page of them), each as `(rev, session, rows)`.
    fn feed_page(&self, sql: &str, from: u64, each: impl Fn(&TRow) -> anyhow::Result<(u64, String, Vec<Row>, Option<u64>)>) -> anyhow::Result<Vec<(u64, String, Vec<Row>, Option<u64>)>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[seq(from), int(i64::try_from(FEED_PAGE).unwrap_or(512))], |r| {
            out.push(each(r)?);
            Ok(())
        })?;
        Ok(out)
    }
}

fn u(r: &TRow, i: usize) -> u64 {
    r.get::<i64>(i).ok().and_then(|n| u64::try_from(n).ok()).unwrap_or(0)
}

impl Engine for Turso {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.transaction(|t| {
            let mut w = t.writes()?;
            for row in rows {
                t.rev += 1;
                t.write(&mut w, row, t.rev)?;
            }
            Ok(())
        })
    }

    fn changes(&mut self, from: u64, max_bytes: usize) -> anyhow::Result<Changes> {
        if from >= self.rev {
            return Ok(Changes { rows: Vec::new(), upto: from });
        }
        let s = |r: &TRow, i: usize| r.get::<String>(i).ok();
        // Each kind's rows after `from`: (rev, session, the rows it makes, its epoch if it says).
        let pages = [
            self.feed_page(sql::FEED_SESSIONS, from, |r| {
                let id = s(r, 0).unwrap_or_default();
                let epoch = u(r, 7);
                let closed = r.get::<i64>(6).ok();
                let mut rows = vec![Row::Session { session: id.clone(), info: s(r, 1), meta: s(r, 2), state: s(r, 3), changes: s(r, 4), epoch: Some(epoch), open: closed.is_none(), at: r.get::<i64>(5).unwrap_or(0) }];
                if let Some(at) = closed {
                    rows.push(Row::Closed { session: id.clone(), at });
                }
                Ok((u(r, 8), id, rows, Some(epoch)))
            })?,
            self.feed_page(sql::FEED_EVENTS, from, |r| {
                let e = event_of(r)?;
                Ok((u(r, 12), e.session.clone(), vec![Row::Event(Box::new(e))], None))
            })?,
            self.feed_page(sql::FEED_RECORDS, from, |r| {
                let rec = RecordRow { session: s(r, 0).unwrap_or_default(), source: s(r, 1).unwrap_or_default(), n: u(r, 2), end: u(r, 3), line: s(r, 4).unwrap_or_default() };
                Ok((u(r, 5), rec.session.clone(), vec![Row::Record(Box::new(rec))], None))
            })?,
            self.feed_page(sql::FEED_DROPS, from, |r| {
                let session = s(r, 1).unwrap_or_default();
                let at = r.get::<i64>(5).unwrap_or(0);
                let row = match s(r, 2).as_deref() {
                    Some("record") => Row::RecordReset { session: session.clone(), source: s(r, 3).unwrap_or_default(), at },
                    _ => Row::Reset { session: session.clone(), from: r.get::<i64>(4).ok().and_then(|n| u64::try_from(n).ok()), at },
                };
                Ok((u(r, 0), session, vec![row], None))
            })?,
        ];
        // Complete through `bound`: a kind that filled its page may have more after its last.
        let mut bound = self.rev;
        for p in &pages {
            if p.len() >= FEED_PAGE
                && let Some((last, ..)) = p.last()
            {
                bound = bound.min(*last);
            }
        }
        let mut all: Vec<(u64, String, Vec<Row>, Option<u64>)> = pages.into_iter().flatten().filter(|(rev, ..)| *rev <= bound).collect();
        all.sort_by_key(|(rev, ..)| *rev);
        let mut epochs: HashMap<String, u64> = HashMap::new();
        let mut out = Changes { rows: Vec::new(), upto: bound };
        let mut bytes = 0usize;
        for (rev, session, rows, epoch) in all {
            if bytes >= max_bytes && !out.rows.is_empty() {
                // The rest next time: read through the last one taken.
                out.upto = out.rows.last().map_or(from, |c| c.rev);
                break;
            }
            let epoch = match epoch {
                Some(e) => e,
                None => match epochs.get(&session) {
                    Some(e) => *e,
                    None => {
                        let e = self.epoch_of(&session)?;
                        epochs.insert(session.clone(), e);
                        e
                    }
                },
            };
            for row in rows {
                bytes += row.bytes();
                out.rows.push(Change { rev, epoch, row });
            }
        }
        Ok(out)
    }

    fn take(&mut self, peer: &str, from: u64, batch: &Changes) -> anyhow::Result<Took> {
        let have = self.peer(peer)?;
        if from != have || batch.upto < from {
            return Ok(Took { rev: have, gap: true, ..Took::default() });
        }
        let now = crate::util::now_ms();
        let peer = peer.to_owned();
        self.transaction(|t| {
            let mut w = t.writes()?;
            let mut epochs: HashMap<String, u64> = HashMap::new();
            let mut took = Took { rev: batch.upto, ..Took::default() };
            for c in &batch.rows {
                let session = c.row.session();
                let current = match epochs.get(session) {
                    Some(e) => *e,
                    None => {
                        let e = t.epoch_of(session)?;
                        epochs.insert(session.to_owned(), e);
                        e
                    }
                };
                // A former writer's rows (it moved on elsewhere) never count again.
                if c.epoch < current {
                    took.fenced += 1;
                    continue;
                }
                if c.epoch > current {
                    // Its new writer, from here on: said before its rows (it may be new here).
                    t.rev += 1;
                    let r = t.rev;
                    t.run(&mut w.session, &[txt(session), Value::Null, Value::Null, Value::Null, Value::Null, int(now), seq(c.epoch), seq(r)], |_| Ok(()))?;
                    epochs.insert(session.to_owned(), c.epoch);
                }
                t.rev += 1;
                let r = t.rev;
                t.write(&mut w, &c.row, r)?;
                took.applied += 1;
            }
            let mut set = t.prepare(sql::PEER_SET)?;
            t.run(&mut set, &[txt(&peer), seq(batch.upto), int(now)], |_| Ok(()))?;
            Ok(took)
        })
    }

    fn peer(&mut self, peer: &str) -> anyhow::Result<u64> {
        Ok(self.one_int(sql::PEER_GET, &[txt(peer)])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>> {
        let mut sources = self.prepare(sql::RECORD_SOURCES)?;
        let mut last: Vec<(String, u64)> = Vec::new();
        self.run(&mut sources, &[txt(session)], |r| {
            if let Ok(src) = r.get::<String>(0) {
                last.push((src, u(r, 1)));
            }
            Ok(())
        })?;
        let mut line = self.prepare(sql::RECORD_LINE)?;
        let mut out = Vec::new();
        for (source, n) in last {
            let mut found = None;
            self.run(&mut line, &[txt(session), txt(&source), seq(n)], |r| {
                found = Some((u(r, 0), r.get::<String>(1).unwrap_or_default()));
                Ok(())
            })?;
            if let Some((end, l)) = found {
                out.push(Mark { source, n, end, line: l });
            }
        }
        Ok(out)
    }

    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        let mut stmt = self.prepare(sql::RECORD_LINES)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[txt(session), txt(source), seq(from), int(i64::try_from(limit).unwrap_or(i64::MAX))], |r| {
            out.push((u(r, 0), r.get::<String>(1).unwrap_or_default()));
            Ok(())
        })?;
        Ok(out)
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

    fn seq_v(v: u64) -> Value {
        super::seq(v)
    }

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    #[test]
    fn a_store_from_before_revisions_gets_them_and_its_feed_sends_everything_it_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        {
            // Schema 1, as C1 left it, with what C1 kept.
            let old = Turso::open_at(&path, 1)?;
            let a = |id: &str, seq: u64| super::super::rows::of("a1", &said(id, seq, id), 5).into_iter().next();
            old.exec("BEGIN")?;
            for sql in [
                "INSERT INTO sessions (id, info, state, updated_at) VALUES ('a1', '{}', '{\"title\":\"old\"}', 5)",
                "INSERT INTO sessions (id, info, updated_at, closed_at) VALUES ('a2', '{}', 6, 7)",
            ] {
                old.exec(sql)?;
            }
            old.exec("COMMIT")?;
            let mut up = old.prepare("INSERT INTO events (session, id, seq, at, kind, text, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
            for (id, seq) in [("m1", 1u64), ("m2", 2)] {
                let Some(Row::Event(e)) = a(id, seq) else { anyhow::bail!("no event row") };
                old.run(&mut up, &[txt(&e.session), txt(&e.id), seq_v(e.seq), int(e.at), txt(&e.kind), txt(&e.text), txt(&e.body)], |_| Ok(()))?;
            }
        }
        let mut t = Turso::open(&path)?;
        assert!(!t.id().is_empty());
        let all = t.changes(0, 1 << 20)?;
        let mut kinds: Vec<String> = all.rows.iter().map(|c| format!("{}:{}", c.row.session(), match &c.row { Row::Session { .. } => "session", Row::Closed { .. } => "closed", Row::Event(_) => "event", _ => "other" })).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["a1:event", "a1:event", "a1:session", "a2:closed", "a2:session"]);
        // Numbers of their own, none twice, and what's written next comes after them.
        let mut revs: Vec<u64> = all.rows.iter().filter(|c| !matches!(c.row, Row::Closed { .. })).map(|c| c.rev).collect();
        revs.dedup();
        assert_eq!(revs.len(), 4);
        let top = all.upto;
        t.apply(&[Row::Event(Box::new(super::super::rows::event("a1", &Event { id: "m3".into(), seq: 3, at: 9, parent: None, by: None, body: Body::Assistant { text: "new".into(), streaming: false } }).ok_or_else(|| anyhow::anyhow!("no row"))?))])?;
        let next = t.changes(top, 1 << 20)?;
        assert_eq!(next.rows.len(), 1);
        assert!(next.rows.iter().all(|c| c.rev > top));
        // Reopened: the same id, and numbering goes on.
        let id = t.id();
        drop(t);
        let t = Turso::open(&path)?;
        assert_eq!(t.id(), id);
        assert_eq!(t.rev, next.upto);
        Ok(())
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
            store.send("a3", &Update::Reset { from: None });
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
