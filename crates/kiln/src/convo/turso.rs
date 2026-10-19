//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store"), vendored in
//! `vendor/turso`. One connection, owned by the store's writer thread; every statement is driven
//! to its end on that thread (`PlatformIO` does its I/O inline, so `step` waits on nothing).

use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use turso_core::{Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, Row as TRow, SqliteDialect, Statement, StepResult, Value};

use std::collections::HashMap;

use super::rows::{EventRow, RecordRow, Row};
use super::sql;
use super::store::{Change, Changes, Engine, Kept, Mark, Query, SEARCH_MAX, Took, Unended};

/// How often a statement that finds the database busy tries again, and how long it waits between.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// Rows of each kind a feed reads at once.
const FEED_PAGE: usize = 512;
/// How long opening waits for whoever else holds the file to let go of it: this process's last
/// writer still closing, or the runner process before this one on its way out after an upgrade.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// A Turso database file.
pub struct Turso {
    io: Arc<dyn IO>,
    _db: Arc<Database>,
    conn: Arc<Connection>,
    path: PathBuf,
    /// Who the store is to its peers.
    id: String,
    /// The last number given to a write.
    rev: u64,
    /// Record lines written before this number may still be kept uncompressed (0: none are).
    squeeze_below: u64,
    /// A mirror's copy is restoring this store (`copied` has sessions): what else writes a
    /// session it brought takes it over from the copy.
    copying: bool,
    /// The rows being written are a copy's ([`Engine::take_copy`]).
    filling: bool,
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

/// A file beside the store's: its name with `suffix` added.
fn beside(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// What a compaction left behind it ([`Engine::compact`]): a compact copy written whole takes the
/// store's place (the compaction stopped between the copy and the swap); one written in part goes.
fn finish_compact(path: &Path) -> anyhow::Result<()> {
    for part in [".compact.tmp", ".compact.tmp-wal", ".compact.tmp-shm"] {
        let _ = std::fs::remove_file(beside(path, part));
    }
    let whole = beside(path, ".compact");
    if !whole.exists() {
        return Ok(());
    }
    // The old file's log would be read into the copy: it goes first.
    for side in ["-wal", "-shm"] {
        if let Err(e) = std::fs::remove_file(beside(path, side))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(anyhow::Error::from(e).context(format!("remove the old {side} of {}", path.display())));
        }
    }
    std::fs::rename(&whole, path).map_err(|e| anyhow::Error::from(e).context(format!("put the compact copy in place of {}", path.display())))?;
    // The rename itself on disk (not on Windows, which opens no folder).
    if let Some(dir) = path.parent()
        && let Ok(d) = std::fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    tracing::info!(file = %path.display(), "conversation store: the compact copy took the store's place");
    Ok(())
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
        finish_compact(path)?;
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("the store's path isn't UTF-8: {}", path.display()))?;
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().map_err(fail)?);
        let until = std::time::Instant::now() + LOCK_WAIT;
        let db = loop {
            match Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new())) {
                Err(turso_core::LimboError::LockingError(_)) if std::time::Instant::now() < until => std::thread::sleep(std::time::Duration::from_millis(100)),
                opened => break opened.map_err(fail)?,
            }
        };
        let conn = db.connect(ConnectOpts::new()).map_err(fail)?;
        let mut t = Self { io, _db: db, conn, path: path.to_path_buf(), id: String::new(), rev: 0, squeeze_below: 0, copying: false, filling: false };
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
        // Where compressing the lines kept before lines were compressed is (a store that never
        // started: from its newest line down).
        t.squeeze_below = match t.meta("squeeze_below")?.and_then(|v| v.parse::<u64>().ok()) {
            Some(b) => b,
            None => rev + 1,
        };
        if schema >= 3 {
            t.copying = t.one_int(sql::COPIED_ANY, &[])?.is_some();
        }
        Ok(t)
    }

    /// A batch of `peer`'s feed that follows its number `from` ([`Engine::take`]), or of a copy
    /// restoring this store (`copy`, [`Engine::take_copy`]): a copy brings back the sessions this
    /// store has nothing of, and leaves alone those it has anything of from elsewhere.
    fn take_rows(&mut self, peer: &str, machine: Option<&str>, from: u64, batch: &Changes, copy: bool) -> anyhow::Result<Took> {
        let have = self.peer(peer)?;
        if from != have || batch.upto < from {
            return Ok(Took { rev: have, gap: true, ..Took::default() });
        }
        let now = crate::util::now_ms();
        let (peer, machine) = (peer.to_owned(), machine.map(str::to_owned));
        self.transaction(|t| {
            let mut w = t.writes()?;
            let mut epochs: HashMap<String, u64> = HashMap::new();
            let mut copied: HashMap<String, bool> = HashMap::new();
            let mut took = Took { rev: batch.upto, ..Took::default() };
            for c in &batch.rows {
                let session = c.row.session();
                if copy {
                    let ours = match copied.get(session) {
                        Some(o) => *o,
                        None => {
                            let o = if t.one_int(sql::COPIED_HAS, &[txt(session)])?.is_some() {
                                true
                            } else if t.known(session)? {
                                false
                            } else {
                                let mut add = t.prepare(sql::COPIED_ADD)?;
                                t.run(&mut add, &[txt(session)], |_| Ok(()))?;
                                t.copying = true;
                                true
                            };
                            copied.insert(session.to_owned(), o);
                            o
                        }
                    };
                    // What the copy dropped is gone from it already: nothing to drop here.
                    if !ours || matches!(c.row, Row::Reset { .. } | Row::RecordReset { .. }) {
                        took.left += 1;
                        continue;
                    }
                }
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
            // Where each session it brought runs: the machine of this feed (a former writer's
            // rows, left out, don't say so). A mirror's feed says it in its rows instead.
            if let Some(machine) = &machine {
                let mut at = t.prepare(sql::SESSION_MACHINE)?;
                for (session, epoch) in &epochs {
                    let theirs = batch.rows.iter().any(|c| c.row.session() == session.as_str() && c.epoch >= *epoch);
                    if theirs {
                        t.run(&mut at, &[txt(session), txt(machine)], |_| Ok(()))?;
                    }
                }
            }
            let mut set = t.prepare(sql::PEER_SET)?;
            t.run(&mut set, &[txt(&peer), seq(batch.upto), int(now)], |_| Ok(()))?;
            Ok(took)
        })
    }

    /// Whether the store has anything of `session`: its row, an event, a line of its record.
    fn known(&self, session: &str) -> anyhow::Result<bool> {
        for q in [sql::KNOWN_SESSION, sql::KNOWN_EVENTS, sql::KNOWN_RECORDS] {
            if self.one_int(q, &[txt(session)])?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
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
    machine: Statement,
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

/// A line of a harness's record shorter than this is kept as it is.
const SQUEEZE_MIN: usize = 256;
/// A kept line's first byte, when it is a blob: how the rest is compressed.
const CODEC_BROTLI: u8 = 1;
/// Compression quality: fast, and most of brotli's gain on JSON.
const SQUEEZE_QUALITY: u32 = 5;

/// A record line as the store keeps it: brotli-compressed behind a codec byte when that saves a
/// tenth or more (a blob), else as it is (text). Transcripts are most of what a store holds.
fn squeeze(line: &str) -> Value {
    use std::io::Write as _;
    if line.len() >= SQUEEZE_MIN {
        let mut out = Vec::with_capacity(line.len() / 3 + 1);
        out.push(CODEC_BROTLI);
        let written = {
            let mut w = brotli::CompressorWriter::new(&mut out, 4096, SQUEEZE_QUALITY, 22);
            w.write_all(line.as_bytes()).and_then(|()| w.flush())
        };
        if written.is_ok()
            && out.len() * 10 <= line.len() * 9
            && let Ok(v) = Value::from_slice(&out)
        {
            return v;
        }
    }
    txt(line)
}

/// A record line as [`squeeze`] kept it.
fn unsqueeze(v: &Value) -> anyhow::Result<String> {
    use std::io::Read as _;
    if let Some(t) = v.to_text() {
        return Ok(t.to_owned());
    }
    let b = v.to_blob().ok_or_else(|| anyhow::anyhow!("a record line that is neither text nor a blob"))?;
    match b.split_first() {
        Some((&CODEC_BROTLI, rest)) => {
            let mut out = String::new();
            brotli::Decompressor::new(rest, 4096).read_to_string(&mut out)?;
            Ok(out)
        }
        _ => anyhow::bail!("a record line kept in an unknown encoding"),
    }
}

impl Turso {
    fn writes(&self) -> anyhow::Result<Writes> {
        Ok(Writes {
            session: self.prepare(sql::SESSION_UPSERT)?,
            machine: self.prepare(sql::SESSION_MACHINE)?,
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
        // While a copy restores this store, what else writes a session it brought (its own
        // machine's feed, come back) takes it over: the copy's later rows of it are left out.
        if self.copying && !self.filling {
            let mut gone = self.prepare(sql::COPIED_DROP)?;
            self.run(&mut gone, &[txt(row.session())], none)?;
        }
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
            Row::Record(rec) => self.run(&mut w.record, &[txt(&rec.session), txt(&rec.source), seq(rec.n), seq(rec.end), squeeze(&rec.line), r], none)?,
            Row::RecordReset { session, source, at } => {
                self.run(&mut w.records_reset, &[txt(session), txt(source)], none)?;
                self.run(&mut w.dropped, &[r, txt(session), txt("record"), txt(source), Value::Null, int(*at)], none)?;
            }
            // Where it runs isn't a write of its own: it goes with the session's row.
            Row::Machine { session, machine } => self.run(&mut w.machine, &[txt(session), txt(machine)], none)?,
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
                if let Some(machine) = s(r, 9) {
                    rows.push(Row::Machine { session: id.clone(), machine });
                }
                Ok((u(r, 8), id, rows, Some(epoch)))
            })?,
            self.feed_page(sql::FEED_EVENTS, from, |r| {
                let e = event_of(r)?;
                Ok((u(r, 12), e.session.clone(), vec![Row::Event(Box::new(e))], None))
            })?,
            self.feed_page(sql::FEED_RECORDS, from, |r| {
                let rec = RecordRow { session: s(r, 0).unwrap_or_default(), source: s(r, 1).unwrap_or_default(), n: u(r, 2), end: u(r, 3), line: unsqueeze(r.get_value(4))? };
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

    fn take(&mut self, peer: &str, machine: Option<&str>, from: u64, batch: &Changes) -> anyhow::Result<Took> {
        self.take_rows(peer, machine, from, batch, false)
    }

    fn take_copy(&mut self, peer: &str, from: u64, batch: &Changes) -> anyhow::Result<Took> {
        self.filling = true;
        let took = self.take_rows(peer, None, from, batch, true);
        self.filling = false;
        took
    }

    fn end_copy(&mut self) -> anyhow::Result<usize> {
        let n = self.one_int(sql::COPIED_COUNT, &[])?.and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
        self.exec(sql::COPIED_CLEAR)?;
        self.copying = false;
        Ok(n)
    }

    fn is_empty(&mut self) -> anyhow::Result<bool> {
        Ok(self.one_int(sql::ANY_SESSION, &[])?.is_none() && self.one_int(sql::ANY_EVENT, &[])?.is_none())
    }

    fn top(&mut self) -> u64 {
        self.rev
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
                found = Some((u(r, 0), unsqueeze(r.get_value(1))?));
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
            out.push((u(r, 0), unsqueeze(r.get_value(1))?));
            Ok(())
        })?;
        Ok(out)
    }

    fn squeeze_some(&mut self, limit: usize) -> anyhow::Result<bool> {
        if self.squeeze_below == 0 {
            return Ok(false);
        }
        let mut below = self.prepare(sql::RECORDS_BELOW)?;
        let mut rows: Vec<(String, String, u64, String, u64)> = Vec::new();
        let mut lowest = self.squeeze_below;
        self.run(&mut below, &[seq(self.squeeze_below), int(i64::try_from(limit).unwrap_or(500))], |r| {
            let rev = u(r, 4);
            lowest = lowest.min(rev);
            // Only lines kept as text, long enough to be worth it.
            if let (Ok(session), Ok(source), Ok(line)) = (r.get::<String>(0), r.get::<String>(1), r.get::<String>(3))
                && line.len() >= SQUEEZE_MIN
            {
                rows.push((session, source, u(r, 2), line, rev));
            }
            Ok(())
        })?;
        // Nothing below: every line kept before lines were compressed has been looked at.
        let next = if lowest == self.squeeze_below { 0 } else { lowest };
        self.transaction(|t| {
            let mut set = t.prepare(sql::RECORD_SQUEEZE)?;
            for (session, source, n, line, _) in &rows {
                let v = squeeze(line);
                if v.to_blob().is_some() {
                    t.run(&mut set, &[txt(session), txt(source), seq(*n), v], |_| Ok(()))?;
                }
            }
            t.set_meta("squeeze_below", &next.to_string())
        })?;
        self.squeeze_below = next;
        Ok(next > 0)
    }

    fn prune_some(&mut self, before: i64, limit: usize) -> anyhow::Result<usize> {
        let mut ended = self.prepare(sql::SESSIONS_ENDED_BEFORE)?;
        let mut ids: Vec<String> = Vec::new();
        self.run(&mut ended, &[int(before)], |r| {
            if let Ok(id) = r.get::<String>(0) {
                ids.push(id);
            }
            Ok(())
        })?;
        // The first lines of the first file of the first of them with a record still kept.
        let (mut first, mut lines, mut gone) = (self.prepare(sql::RECORD_FIRST_SOURCE)?, self.prepare(sql::RECORD_FIRST_LINES)?, self.prepare(sql::RECORDS_PRUNE)?);
        for id in ids {
            let mut source = None;
            self.run(&mut first, &[txt(&id)], |r| {
                source = r.get::<String>(0).ok();
                Ok(())
            })?;
            let Some(source) = source else { continue };
            let mut ns: Vec<u64> = Vec::new();
            self.run(&mut lines, &[txt(&id), txt(&source), int(i64::try_from(limit).unwrap_or(500))], |r| {
                ns.push(u(r, 0));
                Ok(())
            })?;
            let Some(last) = ns.last().copied() else { continue };
            self.run(&mut gone, &[txt(&id), txt(&source), seq(last)], |_| Ok(()))?;
            return Ok(ns.len());
        }
        Ok(0)
    }

    fn compact(self: Box<Self>, min_free: u64) -> anyhow::Result<(Box<dyn Engine>, Option<(u64, u64)>)> {
        let pages = |q: &str| self.one_int(q, &[]).ok().flatten().and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
        let page = pages("PRAGMA page_size");
        let (before, free) = (pages("PRAGMA page_count") * page, pages("PRAGMA freelist_count") * page);
        if free < min_free || free * 3 < before {
            return Ok((self, None));
        }
        // The copy, whole once renamed: from then on it takes the store's place, even after a crash.
        let path = self.path.clone();
        let (part, whole) = (beside(&path, ".compact.tmp"), beside(&path, ".compact"));
        let copied = (|| -> anyhow::Result<()> {
            let _ = std::fs::remove_file(&part);
            let name = part.to_str().ok_or_else(|| anyhow::anyhow!("the store's path isn't UTF-8: {}", part.display()))?;
            self.conn.execute(format!("VACUUM INTO '{}'", name.replace('\'', "''"))).map_err(fail)?;
            // Everything is in the copy itself: the log it was written with holds nothing.
            anyhow::ensure!(std::fs::metadata(beside(&part, "-wal")).map_or(0, |m| m.len()) == 0, "the compact copy's log isn't empty");
            for side in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(beside(&part, side));
            }
            std::fs::File::open(&part)?.sync_all()?;
            std::fs::rename(&part, &whole)?;
            Ok(())
        })();
        if let Err(e) = copied {
            let _ = std::fs::remove_file(&part);
            tracing::warn!(file = %path.display(), error = %format!("{e:#}"), "conversation store: no compact copy could be made; it goes on as it is");
            return Ok((self, None));
        }
        drop(self);
        let opened = Self::open(&path)?;
        let after = std::fs::metadata(&path).map_or(0, |m| m.len());
        Ok((Box::new(opened), Some((before, after))))
    }

    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>> {
        let mut stmt = self.prepare(sql::SESSIONS_UNENDED)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[int(since)], |r| {
            if let (Ok(id), Ok(info)) = (r.get::<String>(0), r.get::<String>(1)) {
                out.push(Unended { id, info, meta: r.get::<String>(2).ok(), state: r.get::<String>(3).ok(), machine: r.get::<String>(4).ok() });
            }
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

    fn record(n: u64, line: &str) -> Row {
        Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc".into(), n, end: (n + 1) * 1000, line: line.into() }))
    }

    #[test]
    fn the_records_of_sessions_that_ended_long_ago_go_and_their_events_stay() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("c.db"))?;
        let mut rows = Vec::new();
        for s in ["old", "recent", "open"] {
            rows.push(Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 });
            rows.extend(super::super::rows::of(s, &said("m1", 1, "the clock was read twice"), 5));
            for source in ["cc-a", "cc-b"] {
                for n in 0..5u64 {
                    rows.push(Row::Record(Box::new(RecordRow { session: s.into(), source: source.into(), n, end: (n + 1) * 100, line: format!("{{\"n\":{n}}}") })));
                }
            }
        }
        rows.push(Row::Closed { session: "old".into(), at: 1_000 });
        rows.push(Row::Closed { session: "recent".into(), at: 9_000 });
        t.apply(&rows)?;
        // Ended before 5,000: "old" alone, its two files, four lines at a time.
        let mut gone = Vec::new();
        loop {
            match t.prune_some(5_000, 4)? {
                0 => break,
                n => gone.push(n),
            }
        }
        assert_eq!(gone, vec![4, 1, 4, 1]);
        assert!(t.marks("old")?.is_empty());
        assert_eq!(t.kept("old", 10)?.map(|k| k.events.len()), Some(1), "its events stay");
        for s in ["recent", "open"] {
            assert_eq!(t.marks(s)?.len(), 2, "{s}");
            assert_eq!(t.record(s, "cc-b", 0, 10)?.len(), 5, "{s}");
        }
        Ok(())
    }

    #[test]
    fn a_copy_brings_back_what_the_store_lost_and_leaves_alone_what_it_has() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("c.db"))?;
        let session = |s: &str, state: &str| Row::Session { session: s.into(), info: Some(format!("{{\"id\":\"{s}\"}}")), meta: None, state: Some(format!("{{\"title\":\"{state}\"}}")), changes: None, epoch: None, open: true, at: 5 };
        let event = |s: &str, id: &str, text: &str| super::super::rows::of(s, &said(id, 1, text), 5).into_iter().next().ok_or_else(|| anyhow::anyhow!("no event row"));
        let line = |s: &str, n: u64| Row::Record(Box::new(RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: format!("{{\"n\":{n}}}") }));
        let changes = |rows: Vec<Row>, from: u64| Changes { upto: from + rows.len() as u64, rows: rows.into_iter().enumerate().map(|(i, row)| Change { rev: from + 1 + i as u64, epoch: 0, row }).collect() };
        // Its machine's own feed brought `live` back already.
        t.take("box/s1", Some("box"), 0, &changes(vec![session("live", "now"), event("live", "e1", "said now")?], 0))?;
        // The copy has `live` as it was, and two sessions the store lost.
        let copy = vec![
            session("live", "before"),
            event("live", "e1", "said before")?,
            event("live", "e0", "only in the copy")?,
            session("gone", "gone's"),
            event("gone", "g1", "from a machine that is gone")?,
            line("gone", 0),
            Row::Closed { session: "gone".into(), at: 9 },
            Row::Machine { session: "gone".into(), machine: "old-box".into() },
            Row::Reset { session: "gone".into(), from: Some(0), at: 9 },
            session("back", "back's"),
            event("back", "b1", "from the copy")?,
        ];
        let took = t.take_copy("mirror/m1", 0, &changes(copy, 0))?;
        assert_eq!((took.applied, took.left), (7, 4), "{took:?}");
        // What the store had from elsewhere stays as it was.
        let live = t.kept("live", 10)?.ok_or_else(|| anyhow::anyhow!("live is gone"))?;
        assert!(live.state.as_deref().is_some_and(|s| s.contains("now")));
        assert_eq!(live.events.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), vec!["said now"]);
        // What it lost is back, as it was: its events, record, end and machine.
        let gone = t.kept("gone", 10)?.ok_or_else(|| anyhow::anyhow!("gone didn't come back"))?;
        assert_eq!(gone.events.len(), 1);
        assert_eq!(gone.closed_at, Some(9));
        assert_eq!(t.record("gone", "cc", 0, 10)?.len(), 1);
        assert_eq!(t.one_int("SELECT count(*) FROM sessions WHERE id = 'gone' AND machine = 'old-box'", &[])?, Some(1));
        // `back`'s machine comes back mid-copy: from then on its own feed is what counts.
        t.take("box2/s2", Some("box2"), 0, &changes(vec![event("back", "b1", "said on its machine")?], 0))?;
        let more = t.take_copy("mirror/m1", 11, &changes(vec![event("back", "b2", "late copy")?, event("gone", "g2", "more of gone")?], 11))?;
        assert_eq!((more.applied, more.left), (1, 1), "{more:?}");
        let back = t.kept("back", 10)?.ok_or_else(|| anyhow::anyhow!("back is gone"))?;
        assert_eq!(back.events.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), vec!["said on its machine"]);
        // Whole: the copy brought back one session it still has.
        assert_eq!(t.end_copy()?, 1);
        assert!(!t.copying);
        Ok(())
    }

    /// A line that compresses little: random ids, about `bytes` of them.
    fn noise(bytes: usize) -> String {
        let mut s = String::with_capacity(bytes + 32);
        while s.len() < bytes {
            s.push_str(&crate::util::new_id());
        }
        format!("{{\"text\":\"{s}\"}}")
    }

    #[test]
    fn a_store_made_compact_keeps_everything_and_goes_on() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let mut t = Turso::open(&path)?;
        let line = |s: &str, n: u64, text: String| Row::Record(Box::new(RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: text }));
        let mut rows = vec![Row::Session { session: "gone".into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 }];
        rows.extend((0..1_500u64).map(|n| line("gone", n, noise(2_000))));
        rows.push(Row::Closed { session: "gone".into(), at: 10 });
        let kept: Vec<(u64, String)> = (0..40u64).map(|n| (n, noise(500))).collect();
        rows.extend(kept.iter().map(|(n, l)| line("kept", *n, l.clone())));
        rows.extend(super::super::rows::of("kept", &said("m1", 1, "the clock was read twice"), 5));
        t.apply(&rows)?;
        while t.prune_some(1_000, 500)? > 0 {}
        let (id, rev) = (t.id(), t.rev);
        // Not worth it below the bar; then made compact, the same store going on.
        let (t, none) = Box::new(t).compact(u64::MAX)?;
        assert_eq!(none, None);
        let (mut t, sizes) = t.compact(0)?;
        let Some((before, after)) = sizes else { anyhow::bail!("nothing was made compact") };
        assert!(after * 2 < before, "{before} -> {after}");
        assert_eq!(t.id(), id);
        assert_eq!(t.record("kept", "cc", 0, 100)?, kept);
        assert_eq!(t.kept("kept", 10)?.map(|k| k.events.len()), Some(1));
        assert_eq!(t.changes(rev, 1 << 20)?.rows.len(), 0, "the numbers went on where they were");
        t.apply(&[line("kept", 40, "{\"after\":true}".into())])?;
        assert_eq!(t.record("kept", "cc", 40, 10)?, vec![(40, "{\"after\":true}".to_owned())]);
        assert!(t.changes(rev, 1 << 20)?.rows.iter().all(|c| c.rev > rev));
        for left in [".compact", ".compact.tmp", ".compact.tmp-wal", ".compact.tmp-shm"] {
            assert!(!beside(&path, left).exists(), "{left} left beside the store");
        }
        Ok(())
    }

    /// A store whose file another process still holds (the runner before this one, on its way
    /// out after an upgrade) is opened once that process lets go, not given up on.
    #[cfg(unix)]
    #[test]
    fn opening_waits_for_another_process_to_let_go_of_the_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let id = Turso::open(&path)?.id();
        // Held as Turso holds it (an fcntl lock), by another process, for a second.
        let holder = std::process::Command::new("python3")
            .args(["-c", "import fcntl, sys, time; f = open(sys.argv[1], 'r+'); fcntl.lockf(f, fcntl.LOCK_EX); print('held', flush=True); time.sleep(1)", &path.to_string_lossy()])
            .stdout(std::process::Stdio::piped())
            .spawn();
        let Ok(mut holder) = holder else {
            eprintln!("skipped: no python3");
            return Ok(());
        };
        let mut line = String::new();
        let out = holder.stdout.take().ok_or_else(|| anyhow::anyhow!("the holder has no stdout"))?;
        std::io::BufRead::read_line(&mut std::io::BufReader::new(out), &mut line)?;
        anyhow::ensure!(line.trim() == "held", "the holder said {line:?}");
        let started = std::time::Instant::now();
        let t = Turso::open(&path)?;
        assert!(started.elapsed() >= std::time::Duration::from_millis(300), "opened while another process held the file");
        assert_eq!(t.id(), id);
        let _ = holder.wait();
        Ok(())
    }

    #[test]
    fn a_compaction_stopped_before_the_swap_is_finished_when_the_store_opens() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let id = {
            let mut t = Turso::open(&path)?;
            t.apply(&[Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc".into(), n: 0, end: 1, line: "{\"in\":\"the copy\"}".into() }))])?;
            t.conn.execute(format!("VACUUM INTO '{}'", beside(&path, ".compact").display())).map_err(fail)?;
            t.id()
        };
        // The old file's log, which mustn't be read into the copy, and a copy written in part.
        std::fs::write(beside(&path, "-wal"), b"not a log of the copy")?;
        std::fs::write(beside(&path, ".compact.tmp"), b"half a copy")?;
        let mut t = Turso::open(&path)?;
        assert_eq!(t.id(), id);
        assert_eq!(t.record("s1", "cc", 0, 10)?, vec![(0, "{\"in\":\"the copy\"}".to_owned())]);
        assert!(!beside(&path, ".compact").exists() && !beside(&path, ".compact.tmp").exists());
        Ok(())
    }

    #[test]
    fn transcript_lines_are_kept_compressed_and_read_back_whole() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("c.db"))?;
        let long = format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{}\"}}}}", "the retry loop read the clock twice; ".repeat(40));
        let short = "{\"type\":\"summary\"}";
        let before = t.rev;
        t.apply(&[record(0, &long), record(1, short)])?;
        // The long one is a blob, the short one text; both read back as they were, everywhere.
        assert_eq!(t.one_int("SELECT count(*) FROM records WHERE typeof(line) = 'blob'", &[])?, Some(1));
        assert_eq!(t.one_int("SELECT count(*) FROM records WHERE typeof(line) = 'text'", &[])?, Some(1));
        assert!(t.one_int("SELECT length(line) FROM records WHERE n = 0", &[])?.is_some_and(|b| (b as usize) < long.len() / 3));
        assert_eq!(t.record("s1", "cc", 0, 10)?, vec![(0, long.clone()), (1, short.to_owned())]);
        assert_eq!(t.marks("s1")?.first().map(|m| m.line.clone()), Some(short.to_owned()));
        let fed: Vec<String> = t.changes(before, 1 << 20)?.rows.into_iter().filter_map(|c| if let Row::Record(r) = c.row { Some(r.line) } else { None }).collect();
        assert_eq!(fed, vec![long, short.to_owned()]);
        Ok(())
    }

    #[test]
    fn lines_kept_before_compression_are_compressed_in_the_background_and_keep_their_numbers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let long = |i: u64| format!("{{\"n\":{i},\"text\":\"{}\"}}", "tool output, the same words again and again. ".repeat(30));
        {
            // As a store from before compression kept them: text, and nothing yet compressed.
            let mut t = Turso::open(&path)?;
            let mut put = t.prepare(sql::RECORD_UPSERT)?;
            t.exec("BEGIN")?;
            for i in 0..5_000u64 {
                t.rev += 1;
                let r = t.rev;
                t.run(&mut put, &[txt("s1"), txt("cc"), seq(i), seq(i), txt(&long(i)), seq(r)], |_| Ok(()))?;
            }
            t.exec("COMMIT")?;
            let top = t.rev;
            t.set_meta("rev", &top.to_string())?;
            t.set_meta("squeeze_below", &(top + 1).to_string())?;
        }
        let mut t = Turso::open(&path)?;
        let revs = t.one_int("SELECT sum(rev) FROM records", &[])?;
        let mut passes = 0;
        while t.squeeze_some(2_000)? {
            passes += 1;
            assert!(passes < 10, "the pass never ends");
        }
        assert_eq!(t.one_int("SELECT count(*) FROM records WHERE typeof(line) = 'text'", &[])?, Some(0));
        // The same lines, under the same numbers: nothing for a feed to send again.
        assert_eq!(t.one_int("SELECT sum(rev) FROM records", &[])?, revs);
        assert_eq!(t.record("s1", "cc", 4_321, 1)?, vec![(4_321, long(4_321))]);
        // Done, and it stays done after reopening.
        drop(t);
        let mut again = Turso::open(&path)?;
        assert!(!again.squeeze_some(2_000)?);
        Ok(())
    }

    #[test]
    fn a_turso_store_keeps_reads_back_and_reopens() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        {
            let store = super::super::store::Store::start(Box::new(Turso::open(&path)?), None)?;
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
        let store = super::super::store::Store::start(Box::new(Turso::open(&path)?), None)?;
        let k = store.kept("a1", 5_000)?.ok_or_else(|| anyhow::anyhow!("nothing kept after reopening"))?;
        assert_eq!(k.events.len(), 2_000);
        assert!(k.events.iter().any(|e| e.id == "m7" && e.text == "reply seven, revised"));
        Ok(())
    }
}
