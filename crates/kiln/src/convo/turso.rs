//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store"), vendored in
//! `vendor/turso`. One connection, owned by the store's writer thread; every statement is driven
//! to its end on that thread (`PlatformIO` does its I/O inline, so `step` waits on nothing).

use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use turso_core::{CheckpointMode, Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, Row as TRow, SqliteDialect, Statement, StepResult, Value};

use std::collections::HashMap;

use super::rows::{EventRow, RecordRow, Row};
use super::sql;
use super::store::{Change, Changes, Compacting, Engine, Kept, Look, Mark, Query, SearchFrom, Took, Unended};
use super::telemetry::CompactView;

/// How often a statement that finds the database busy tries again, and how long it waits between.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

/// Rows of each kind a feed reads at once.
const FEED_PAGE: usize = 512;
/// How long opening waits for whoever else holds the file to let go of it: this process's last
/// writer still closing, or the runner process before this one on its way out after an upgrade.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// The big tables, copied a batch at a time while compacting: every row of them has a number
/// (`rev`), so what is written meanwhile is copied again by its number, and what a drop took away
/// is taken away from the copy too. The other tables are small, and go whole at the swap.
const COMPACT_SCANNED: [&str; 4] = ["sessions", "events", "records", "drops"];
/// Rows in one step of compacting, at most (a step also stops at its bytes).
const COMPACT_ROWS: i64 = 4096;
/// The copy's page cache while compacting.
const COMPACT_CACHE: usize = 64 << 20;
/// Batches the copier may have to write, at most: the memory a compaction takes is a few.
const COMPACT_AHEAD: usize = 2;

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
    /// A compaction under way ([`Engine::compact_some`]).
    compaction: Option<Box<Compaction>>,
    /// Sessions changed while compacting without a number of their own (where they run): copied
    /// again at the swap.
    touched: std::cell::RefCell<std::collections::HashSet<String>>,
}

/// A compaction under way: the store copied into `<file>.compact.tmp` a batch at a time, under the
/// same numbers; then what was written meanwhile, by its numbers; then, with nothing else writing,
/// the swap. The writer reads each batch; the copier writes it, on a thread of its own, so a read
/// of the store never waits on the copy's disk. It takes the memory of a few batches.
struct Compaction {
    copier: Copier,
    /// Each big table copied through this rowid (None: all of it).
    scans: Vec<(&'static str, Option<i64>)>,
    /// What was written under a number after this was written while copying: copied again.
    through: u64,
    /// The file's size when compacting began, what it held then (its size less what was free),
    /// and when (unix ms).
    before: u64,
    held: u64,
    began: i64,
    /// Caught up once, the copy on disk and counted against the store: a copy that doesn't hold
    /// what the store holds stops before the swap, and the swap's count reads what this one read.
    counted: Counted,
}

enum Counted {
    No,
    /// The store's counts, and the copier's to come.
    Asked(Vec<Option<i64>>, mpsc::Receiver<anyhow::Result<Vec<Option<i64>>>>),
    Yes,
}

/// What the copier does to the copy, in the order it is given.
enum CopyJob {
    /// Whole rows into a table, as they are; the log written into the file after, when `true`
    /// (each batch: the swap leaves it to its count, which writes it once).
    Put(&'static str, Vec<Vec<Value>>, bool),
    /// What was written while copying, in the order it was written ([`Turso::replay`]); the log
    /// as for `Put`.
    Replay(Vec<(u64, &'static str, Vec<Value>)>, bool),
    /// A small table's rows, the table emptied first.
    Replace(String, Vec<Vec<Value>>),
    /// How many rows each table holds, the copy put on disk first when `true`.
    Count(Vec<String>, bool, mpsc::Sender<anyhow::Result<Vec<Option<i64>>>>),
}

/// The thread that writes the copy.
struct Copier {
    tx: Option<mpsc::SyncSender<CopyJob>>,
    /// Jobs given and not yet done.
    pending: Arc<AtomicUsize>,
    /// What went wrong in it, if anything: it takes nothing more then.
    failed: Arc<Mutex<Option<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Copier {
    fn start(mut copy: Turso) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<CopyJob>(COMPACT_AHEAD);
        let (pending, failed) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
        let (left, wrong) = (pending.clone(), failed.clone());
        let thread = std::thread::Builder::new().name("kiln-convo-copy".into()).spawn(move || {
            while let Ok(job) = rx.recv() {
                let began = std::time::Instant::now();
                let done = match job {
                    CopyJob::Put(table, rows, flush) => copy.transaction(|t| t.put_rows(table, &rows)).and_then(|()| if flush { copy.flush() } else { Ok(()) }),
                    CopyJob::Replay(rows, flush) => copy.replay(&rows).and_then(|()| if flush { copy.flush() } else { Ok(()) }),
                    CopyJob::Replace(table, rows) => copy.transaction(|t| {
                        t.exec(&format!("DELETE FROM {table}"))?;
                        t.put_rows(&table, &rows)
                    }),
                    CopyJob::Count(tables, sync, answer) => {
                        let counted = (|| {
                            if sync {
                                copy.flush()?;
                                sync_file(&copy.path)?;
                            }
                            copy.counts(&tables)
                        })();
                        let _ = answer.send(counted);
                        Ok(())
                    }
                };
                tracing::debug!(ms = ms(began.elapsed()), "conversation store: the copier wrote a batch");
                left.fetch_sub(1, Ordering::SeqCst);
                if let Err(e) = done {
                    *wrong.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("{e:#}"));
                    return;
                }
            }
        })?;
        Ok(Self { tx: Some(tx), pending, failed, thread: Some(thread) })
    }

    /// Jobs given and not yet done.
    fn busy(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }

    /// What went wrong in the copier, as an error.
    fn check(&self) -> anyhow::Result<()> {
        match self.failed.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            Some(e) => anyhow::bail!("the compact copy couldn't be written: {e}"),
            None => Ok(()),
        }
    }

    /// A job, after those given before it (waiting for room, which a step makes sure there is).
    fn give(&self, job: CopyJob) -> anyhow::Result<()> {
        self.check()?;
        let tx = self.tx.as_ref().ok_or_else(|| anyhow::anyhow!("the copier has ended"))?;
        self.pending.fetch_add(1, Ordering::SeqCst);
        if tx.send(job).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            self.check()?;
            anyhow::bail!("the copier has ended");
        }
        Ok(())
    }

    /// The copy's counts of `tables`, once it has done what it was given before.
    fn count(&self, tables: Vec<String>, sync: bool) -> anyhow::Result<mpsc::Receiver<anyhow::Result<Vec<Option<i64>>>>> {
        let (answer, counted) = mpsc::channel();
        self.give(CopyJob::Count(tables, sync, answer))?;
        Ok(counted)
    }

    /// Everything given done, and the copy closed.
    fn finish(&mut self) -> anyhow::Result<()> {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.check()
    }
}

impl Drop for Copier {
    /// Stopped: the copy closes before its files go.
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A file's bytes on disk. Opened for writing: Windows flushes no file opened only to read.
fn sync_file(path: &Path) -> anyhow::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

/// Milliseconds, for the log.
fn ms(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// About how many bytes a value is.
fn value_bytes(v: &Value) -> usize {
    v.to_text().map(str::len).or_else(|| v.to_blob().map(<[u8]>::len)).unwrap_or(8)
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

/// What a compaction left behind it ([`Engine::compact_some`]): a compact copy written whole takes
/// the store's place (the compaction stopped between the copy and the swap); one written in part
/// goes.
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
        let mut t = Self { io, _db: db, conn, path: path.to_path_buf(), id: String::new(), rev: 0, squeeze_below: 0, copying: false, filling: false, compaction: None, touched: std::cell::RefCell::default() };
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
            let first = t.rev + 1;
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
                        t.touch(session);
                    }
                }
            }
            let mut set = t.prepare(sql::PEER_SET)?;
            t.run(&mut set, &[txt(&peer), seq(batch.upto), int(now)], |_| Ok(()))?;
            took.wrote = (t.rev >= first).then_some((first, t.rev));
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
        self.run_while(stmt, params, |r| each(r).map(|()| true))
    }

    /// [`Turso::run`], stopping once `each` says no more (false).
    fn run_while(&self, stmt: &mut Statement, params: &[Value], mut each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        stmt.reset().map_err(fail)?;
        for (i, p) in params.iter().enumerate() {
            stmt.bind_at(at(i + 1)?, p.clone()).map_err(fail)?;
        }
        let mut busy = 0;
        loop {
            match stmt.step().map_err(fail)? {
                StepResult::Row => {
                    if let Some(row) = stmt.row()
                        && !each(row)?
                    {
                        return stmt.reset().map_err(fail);
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

/// What the store keeps of a record line that came to it: kept as the store it is from keeps
/// it, when it came so ([`RecordRow::kept`]); else [`squeeze`]d here.
fn record_value(rec: &RecordRow) -> anyhow::Result<Value> {
    match &rec.kept {
        Some(k) => {
            anyhow::ensure!(k.len() > 1 && k.first() == Some(&CODEC_BROTLI), "a record line kept in an unknown encoding");
            Value::from_slice(k).map_err(|e| anyhow::anyhow!("a record line too large to keep: {e}"))
        }
        None => Ok(squeeze(&rec.line)),
    }
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
                    self.touch(session);
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
            Row::Record(rec) => self.run(&mut w.record, &[txt(&rec.session), txt(&rec.source), seq(rec.n), seq(rec.end), record_value(rec)?, r], none)?,
            Row::RecordReset { session, source, at } => {
                self.run(&mut w.records_reset, &[txt(session), txt(source)], none)?;
                self.run(&mut w.dropped, &[r, txt(session), txt("record"), txt(source), Value::Null, int(*at)], none)?;
            }
            // Where it runs isn't a write of its own: it goes with the session's row.
            Row::Machine { session, machine } => {
                self.run(&mut w.machine, &[txt(session), txt(machine)], none)?;
                self.touch(session);
            }
        }
        Ok(())
    }

    /// The epoch this store has for `session` (0: none yet).
    fn epoch_of(&self, session: &str) -> anyhow::Result<u64> {
        Ok(self.one_int(sql::SESSION_EPOCH, &[txt(session)])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    /// Rows of one kind after `from`, each as `(rev, session, rows, epoch)`: a page of them, or
    /// fewer once they come to `max_bytes` (nothing more is read, or decompressed, than a batch
    /// takes); and whether more may follow (the page is full, or stopped at the bytes).
    #[allow(clippy::type_complexity)]
    fn feed_page(&self, sql: &str, from: u64, max_bytes: usize, each: impl Fn(&TRow) -> anyhow::Result<(u64, String, Vec<Row>, Option<u64>)>) -> anyhow::Result<(Vec<(u64, String, Vec<Row>, Option<u64>)>, bool)> {
        let mut stmt = self.prepare(sql)?;
        let (mut out, mut bytes, mut full) = (Vec::new(), 0usize, false);
        self.run_while(&mut stmt, &[seq(from), int(i64::try_from(FEED_PAGE).unwrap_or(512))], |r| {
            let page = each(r)?;
            bytes += page.2.iter().map(Row::bytes).sum::<usize>();
            out.push(page);
            full = bytes >= max_bytes;
            Ok(!full)
        })?;
        let more = full || out.len() >= FEED_PAGE;
        Ok((out, more))
    }

    /// A session changed without a number of its own: while compacting, copied again at the swap.
    fn touch(&self, session: &str) {
        if self.compaction.is_some() {
            self.touched.borrow_mut().insert(session.to_owned());
        }
    }

    /// The store's tables (the engine's own left out).
    fn tables(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[], |r| {
            if let Ok(name) = r.get::<String>(0) {
                out.push(name);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// The rows `sql` answers, each without its first column (what they go in the order of; the
    /// last one's comes back too), until they come to `budget` bytes.
    fn rows_from(&self, sql: &str, params: &[Value], budget: usize) -> anyhow::Result<(Vec<Vec<Value>>, i64)> {
        let mut stmt = self.prepare(sql)?;
        let (mut rows, mut bytes, mut last) = (Vec::new(), 0usize, 0i64);
        self.run_while(&mut stmt, params, |r| {
            last = r.get::<i64>(0).map_err(fail)?;
            let row: Vec<Value> = r.get_values().skip(1).cloned().collect();
            bytes += row.iter().map(value_bytes).sum::<usize>();
            rows.push(row);
            Ok(bytes < budget)
        })?;
        Ok((rows, last))
    }

    /// Whole rows into `table` as they are, their columns in order: what is there under the same
    /// key is replaced.
    fn put_rows(&self, table: &str, rows: &[Vec<Value>]) -> anyhow::Result<()> {
        let Some(first) = rows.first() else { return Ok(()) };
        let mut stmt = self.prepare(&format!("INSERT OR REPLACE INTO {table} VALUES ({})", vec!["?"; first.len()].join(", ")))?;
        for row in rows {
            self.run(&mut stmt, row, |_| Ok(()))?;
        }
        Ok(())
    }

    /// Whether compacting is worth it, with room beside the file for the copy: if so it begins.
    fn compact_begin(&mut self, min_free: u64) -> anyhow::Result<bool> {
        let pages = |p: &str| -> anyhow::Result<u64> { Ok(self.one_int(&format!("PRAGMA {p}"), &[])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0)) };
        let page = pages("page_size")?;
        let (size, free) = (pages("page_count")? * page, pages("freelist_count")? * page);
        if free < min_free || free * 3 < size {
            return Ok(false);
        }
        // Room for what the file holds, and some.
        let need = (size - free) + (size - free) / 10 + (64 << 20);
        if let Some(have) = self.path.parent().and_then(crate::os::disk_available)
            && have < need
        {
            tracing::info!(file = %self.path.display(), need, have, "conversation store: no room beside it to make its file compact");
            return Ok(false);
        }
        let part = beside(&self.path, ".compact.tmp");
        for side in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(beside(&part, side));
        }
        let copy = Turso::open(&part)?;
        // The copy counts for nothing until it is whole, on disk and in place: no waiting on the
        // disk for each batch of it. It goes to disk whole before the swap.
        copy.exec("PRAGMA synchronous = OFF")?;
        // Its indexes take rows in no order of theirs (an event's session and number, its time):
        // pages enough to keep them at hand, or each batch reads them again. Bounded, and gone
        // with the copy.
        copy.exec(&format!("PRAGMA cache_size = -{}", COMPACT_CACHE >> 10))?;
        let copier = Copier::start(copy)?;
        self.touched.borrow_mut().clear();
        self.compaction = Some(Box::new(Compaction {
            copier,
            scans: COMPACT_SCANNED.iter().map(|t| (*t, Some(0))).collect(),
            through: self.rev,
            before: size,
            held: size - free,
            began: crate::util::now_ms(),
            counted: Counted::No,
        }));
        tracing::info!(file = %self.path.display(), size, free, "conversation store: its file is being made compact, a step at a time");
        Ok(true)
    }

    /// A step of compacting: a batch of a big table, or of what was written meanwhile, read and
    /// given to the copier. True: nothing is left to copy but what the swap copies.
    fn compact_step(&mut self, budget: usize) -> anyhow::Result<bool> {
        let mut c = self.compaction.take().ok_or_else(|| anyhow::anyhow!("no compaction under way"))?;
        let done = self.compact_batch(&mut c, budget);
        self.compaction = Some(c);
        done
    }

    fn compact_batch(&self, c: &mut Compaction, budget: usize) -> anyhow::Result<bool> {
        c.copier.check()?;
        // The copier a few batches behind at most: a step reads only once it has room.
        if c.copier.busy() >= COMPACT_AHEAD {
            return Ok(false);
        }
        if let Some((table, at)) = c.scans.iter_mut().find(|(_, at)| at.is_some()) {
            let began = std::time::Instant::now();
            let (rows, last) = self.rows_from(&format!("SELECT rowid, * FROM {table} WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"), &[int(at.unwrap_or(0)), int(COMPACT_ROWS)], budget)?;
            if rows.is_empty() {
                *at = None;
            } else {
                tracing::debug!(table, rows = rows.len(), read_ms = ms(began.elapsed()), "conversation store: a step of compacting");
                c.copier.give(CopyJob::Put(table, rows, true))?;
                *at = Some(last);
            }
            return Ok(false);
        }
        self.compact_catch_up(c, budget, true)?;
        if c.through < self.rev {
            return Ok(false);
        }
        // Caught up once: the copy on disk and counted against the store, its count after all the
        // copier was given. The copier counts while the writer goes on.
        match std::mem::replace(&mut c.counted, Counted::Yes) {
            Counted::No => {
                if c.copier.busy() > 0 {
                    c.counted = Counted::No;
                    return Ok(false);
                }
                let tables: Vec<String> = COMPACT_SCANNED.iter().map(|t| (*t).to_owned()).collect();
                let ours = self.counts(&tables)?;
                c.counted = Counted::Asked(ours, c.copier.count(tables, true)?);
                return Ok(false);
            }
            Counted::Asked(ours, theirs) => match theirs.try_recv() {
                Ok(counted) => {
                    let counted = counted?;
                    anyhow::ensure!(counted == ours, "the copy holds {counted:?} rows of {COMPACT_SCANNED:?}, the store {ours:?}");
                    return Ok(false);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    c.counted = Counted::Asked(ours, theirs);
                    return Ok(false);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    c.copier.check()?;
                    anyhow::bail!("the copier has ended");
                }
            },
            Counted::Yes => {}
        }
        // Then the swap, once the copier has done all it was given.
        Ok(c.copier.busy() == 0)
    }

    /// A batch of what was written while copying, in the order it was written, for the copier:
    /// each big table's rows written after `through` (a page of each, or what comes to `budget`),
    /// through where every page is whole.
    fn compact_catch_up(&self, c: &mut Compaction, budget: usize, flush: bool) -> anyhow::Result<()> {
        if c.through >= self.rev {
            return Ok(());
        }
        let began = std::time::Instant::now();
        let mut bound = self.rev;
        let mut all: Vec<(u64, &'static str, Vec<Value>)> = Vec::new();
        for table in COMPACT_SCANNED {
            let mut stmt = self.prepare(&format!("SELECT rev, * FROM {table} WHERE rev > ?1 ORDER BY rev LIMIT ?2"))?;
            let (mut n, mut bytes, mut last) = (0i64, 0usize, 0u64);
            self.run_while(&mut stmt, &[seq(c.through), int(COMPACT_ROWS)], |r| {
                last = u(r, 0);
                let row: Vec<Value> = r.get_values().skip(1).cloned().collect();
                bytes += row.iter().map(value_bytes).sum::<usize>();
                all.push((last, table, row));
                n += 1;
                Ok(bytes < budget)
            })?;
            // A page cut short may have more after its last.
            if n >= COMPACT_ROWS || bytes >= budget {
                bound = bound.min(last);
            }
        }
        all.retain(|(rev, ..)| *rev <= bound);
        all.sort_by_key(|(rev, ..)| *rev);
        tracing::debug!(rows = all.len(), through = bound, read_ms = ms(began.elapsed()), "conversation store: a step of compacting, what was written meanwhile");
        if !all.is_empty() {
            c.copier.give(CopyJob::Replay(all, flush))?;
        }
        c.through = bound;
        Ok(())
    }

    /// What was written while copying, in the order it was written, into this (the copy): each
    /// row as it is now, and what a drop took away taken away here too, before the rows written
    /// after it.
    fn replay(&mut self, rows: &[(u64, &'static str, Vec<Value>)]) -> anyhow::Result<()> {
        self.transaction(|t| {
            let mut puts: HashMap<&str, Statement> = HashMap::new();
            for (_, table, row) in rows {
                if *table == "drops" {
                    // A file of a record, or events from a number on (all of them: none said).
                    let s = |i: usize| row.get(i).and_then(Value::to_text).map(str::to_owned);
                    match (s(1), s(2).as_deref()) {
                        (Some(session), Some("record")) => {
                            let mut reset = t.prepare(sql::RECORDS_RESET)?;
                            t.run(&mut reset, &[txt(&session), text(s(3).as_deref())], |_| Ok(()))?;
                        }
                        (Some(session), _) => {
                            let from = row.get(4).filter(|v| !matches!(v, Value::Null)).cloned().unwrap_or_else(|| int(0));
                            let mut reset = t.prepare(sql::EVENTS_RESET)?;
                            t.run(&mut reset, &[txt(&session), from], |_| Ok(()))?;
                        }
                        _ => {}
                    }
                }
                let stmt = match puts.get_mut(table) {
                    Some(stmt) => stmt,
                    None => {
                        let stmt = t.prepare(&format!("INSERT OR REPLACE INTO {table} VALUES ({})", vec!["?"; row.len()].join(", ")))?;
                        puts.entry(table).or_insert(stmt)
                    }
                };
                t.run(stmt, row, |_| Ok(()))?;
            }
            Ok(())
        })
    }

    /// Everything in the log written into the file itself, and the log emptied. The copy does it
    /// each batch: its log restarts no other way, and would grow to the size of everything copied
    /// (and keep the disk and the memory of a capped server busy writing it back).
    fn flush(&self) -> anyhow::Result<()> {
        self.conn.checkpoint(CheckpointMode::Truncate { upper_bound_inclusive: None }).map_err(fail)?;
        Ok(())
    }

    /// How many rows each of `tables` holds.
    fn counts(&self, tables: &[String]) -> anyhow::Result<Vec<Option<i64>>> {
        tables.iter().map(|t| self.one_int(&format!("SELECT count(*) FROM {t}"), &[])).collect()
    }

    /// With nothing else writing: the rest of what was written meanwhile, the sessions changed
    /// without a number of their own, and the small tables whole, for the copier; then the copy,
    /// on disk, counted against the store, and closed.
    fn compact_seal(&self, c: &mut Compaction) -> anyhow::Result<()> {
        let began = std::time::Instant::now();
        while c.through < self.rev {
            self.compact_catch_up(c, usize::MAX, false)?;
        }
        let touched: Vec<String> = self.touched.borrow_mut().drain().collect();
        let mut again = Vec::new();
        for session in touched {
            again.extend(self.rows_from("SELECT rowid, * FROM sessions WHERE id = ?1", &[txt(&session)], usize::MAX)?.0);
        }
        if !again.is_empty() {
            c.copier.give(CopyJob::Put("sessions", again, false))?;
        }
        let tables = self.tables()?;
        for table in tables.iter().filter(|t| !COMPACT_SCANNED.contains(&t.as_str())) {
            let (rows, _) = self.rows_from(&format!("SELECT rowid, * FROM {table}"), &[], usize::MAX)?;
            c.copier.give(CopyJob::Replace(table.clone(), rows))?;
        }
        let read = began.elapsed();
        let ours = self.counts(&tables)?;
        let theirs = match c.copier.count(tables.clone(), true)?.recv() {
            Ok(counted) => counted?,
            Err(_) => {
                c.copier.check()?;
                anyhow::bail!("the copier has ended");
            }
        };
        anyhow::ensure!(theirs == ours, "the copy holds {theirs:?} rows of {tables:?}, the store {ours:?}");
        let counted = began.elapsed();
        c.copier.finish()?;
        tracing::debug!(read_ms = ms(read), count_ms = ms(counted - read), close_ms = ms(began.elapsed() - counted), "conversation store: the compact copy sealed");
        Ok(())
    }

    /// The copy, sealed, takes the file's place: from its rename on, even after a crash
    /// (`finish_compact`), and the store goes on in it.
    fn compact_swap(mut self: Box<Self>) -> anyhow::Result<(Box<dyn Engine>, Compacting)> {
        let Some(mut c) = self.compaction.take() else { return Ok((self, Compacting::Not)) };
        let (before, held) = (c.before, std::time::Instant::now());
        let sealed = self.compact_seal(&mut c);
        // The copy closed before it moves (a copier stopped part-way finishes here).
        drop(c);
        let part = beside(&self.path, ".compact.tmp");
        let sealed_at = held.elapsed();
        let placed = sealed.and_then(|()| {
            anyhow::ensure!(std::fs::metadata(beside(&part, "-wal")).map_or(0, |m| m.len()) == 0, "the compact copy's log isn't empty");
            for side in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(beside(&part, side));
            }
            sync_file(&part)?;
            std::fs::rename(&part, beside(&self.path, ".compact"))?;
            Ok(())
        });
        let placed_at = held.elapsed();
        if let Err(e) = placed {
            tracing::warn!(file = %self.path.display(), error = %format!("{e:#}"), "conversation store: its compact copy couldn't take its place; it goes on as it is");
            self.compact_drop();
            return Ok((self, Compacting::Not));
        }
        let path = self.path.clone();
        drop(self);
        let opened = Self::open(&path)?;
        let after = std::fs::metadata(&path).map_or(0, |m| m.len());
        tracing::debug!(seal_ms = ms(sealed_at), sync_ms = ms(placed_at - sealed_at), open_ms = ms(held.elapsed() - placed_at), "conversation store: the swap");
        // How long reads and writes waited for the swap.
        tracing::info!(file = %path.display(), held_ms = ms(held.elapsed()), "conversation store: the compact copy took the file's place");
        Ok((Box::new(opened), Compacting::Done { before, after }))
    }

    /// A compaction stopped: the copier ends (the copy closes), and the copy goes.
    fn compact_drop(&mut self) {
        self.compaction = None;
        let part = beside(&self.path, ".compact.tmp");
        for side in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(beside(&part, side));
        }
        self.touched.borrow_mut().clear();
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

    fn changes(&mut self, from: u64, max_bytes: usize, kept: bool) -> anyhow::Result<Changes> {
        if from >= self.rev {
            return Ok(Changes { rows: Vec::new(), upto: from });
        }
        let s = |r: &TRow, i: usize| r.get::<String>(i).ok();
        // Each kind's rows after `from`: (rev, session, the rows it makes, its epoch if it says).
        let pages = [
            self.feed_page(sql::FEED_SESSIONS, from, max_bytes, |r| {
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
            self.feed_page(sql::FEED_EVENTS, from, max_bytes, |r| {
                let e = event_of(r)?;
                Ok((u(r, 12), e.session.clone(), vec![Row::Event(Box::new(e))], None))
            })?,
            self.feed_page(sql::FEED_RECORDS, from, max_bytes, |r| {
                // To a taker that takes them so, a compressed line goes as it is kept.
                let v = r.get_value(4);
                let (line, as_kept) = match v.to_blob() {
                    Some(b) if kept && b.first() == Some(&CODEC_BROTLI) => (String::new(), Some(b.to_vec())),
                    _ => (unsqueeze(v)?, None),
                };
                let rec = RecordRow { session: s(r, 0).unwrap_or_default(), source: s(r, 1).unwrap_or_default(), n: u(r, 2), end: u(r, 3), line, kept: as_kept };
                Ok((u(r, 5), rec.session.clone(), vec![Row::Record(Box::new(rec))], None))
            })?,
            self.feed_page(sql::FEED_DROPS, from, max_bytes, |r| {
                let session = s(r, 1).unwrap_or_default();
                let at = r.get::<i64>(5).unwrap_or(0);
                let row = match s(r, 2).as_deref() {
                    Some("record") => Row::RecordReset { session: session.clone(), source: s(r, 3).unwrap_or_default(), at },
                    _ => Row::Reset { session: session.clone(), from: r.get::<i64>(4).ok().and_then(|n| u64::try_from(n).ok()), at },
                };
                Ok((u(r, 0), session, vec![row], None))
            })?,
        ];
        // Complete through `bound`: a kind that filled its page, or its bytes, may have more after
        // its last.
        let mut bound = self.rev;
        for (p, more) in &pages {
            if *more && let Some((last, ..)) = p.last() {
                bound = bound.min(*last);
            }
        }
        let mut all: Vec<(u64, String, Vec<Row>, Option<u64>)> = pages.into_iter().flat_map(|(p, _)| p).filter(|(rev, ..)| *rev <= bound).collect();
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

    fn peers(&mut self) -> anyhow::Result<Vec<(String, u64, i64)>> {
        let mut stmt = self.prepare(sql::PEERS_ALL)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[], |r| {
            out.push((r.get::<String>(0).map_err(fail)?, u(r, 1), r.get::<i64>(2).unwrap_or(0)));
            Ok(())
        })?;
        Ok(out)
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
        // Not while compacting: a line compressed in place keeps its number, so the copy wouldn't
        // know of it.
        if self.squeeze_below == 0 || self.compaction.is_some() {
            return Ok(self.squeeze_below > 0);
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
        // Not while compacting: lines let go have no number, so the copy wouldn't know of it.
        if self.compaction.is_some() {
            return Ok(0);
        }
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

    fn compact_some(mut self: Box<Self>, min_free: u64, step_bytes: usize) -> anyhow::Result<(Box<dyn Engine>, Compacting)> {
        let ready = if self.compaction.is_some() {
            self.compact_step(step_bytes)
        } else {
            match self.compact_begin(min_free) {
                Ok(true) => return Ok((self, Compacting::Going)),
                Ok(false) => return Ok((self, Compacting::Not)),
                Err(e) => Err(e),
            }
        };
        match ready {
            Ok(false) => Ok((self, Compacting::Going)),
            Ok(true) => self.compact_swap(),
            Err(e) => {
                tracing::warn!(file = %self.path.display(), error = %format!("{e:#}"), "conversation store: making its file compact stopped; it goes on as it is");
                self.compact_drop();
                Ok((self, Compacting::Not))
            }
        }
    }

    fn look(&mut self, count: bool) -> anyhow::Result<Look> {
        let pages = |p: &str| -> anyhow::Result<u64> { Ok(self.one_int(&format!("PRAGMA {p}"), &[])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0)) };
        let page = pages("page_size")?;
        let size = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        let compacting = self.compaction.as_ref().map(|c| {
            let (phase, table) = match c.scans.iter().find(|(_, at)| at.is_some()) {
                Some((t, _)) => ("copy", Some((*t).to_owned())),
                None => match c.counted {
                    Counted::No => ("catch_up", None),
                    Counted::Asked(..) => ("count", None),
                    Counted::Yes => ("swap", None),
                },
            };
            let part = beside(&self.path, ".compact.tmp");
            CompactView { phase: phase.to_owned(), table, copied: size(&part) + size(&beside(&part, "-wal")), of: c.held, began: c.began }
        });
        let sessions = if count { self.one_int("SELECT count(*) FROM sessions", &[])?.and_then(|n| u64::try_from(n).ok()) } else { None };
        Ok(Look { bytes: pages("page_count")? * page, free: pages("freelist_count")? * page, wal: size(&beside(&self.path, "-wal")), top: self.rev, sessions, compacting })
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

    fn ended(&mut self, after: (i64, &str), limit: usize) -> anyhow::Result<Vec<(String, i64)>> {
        let mut stmt = self.prepare(sql::SESSIONS_ENDED_AFTER)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[int(after.0), txt(after.1), int(i64::try_from(limit).unwrap_or(i64::MAX))], |r| {
            if let (Ok(id), Ok(at)) = (r.get::<String>(0), r.get::<i64>(1)) {
                out.push((id, at));
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

    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)> {
        // Newest first by an index, so nothing is sorted and it stops once it has `want`; the words
        // matched here, as LIKE would (any case of an ASCII letter; `%` and `_` as they are).
        let words: Vec<String> = q.q.split_whitespace().map(str::to_ascii_lowercase).collect();
        let (sql, params) = match &q.session {
            Some(s) => (sql::SEARCH_SESSION, vec![txt(s), int(from.map_or(i64::MAX, |f| f.key))]),
            None => (sql::SEARCH_ALL, vec![int(from.map_or_else(|| q.until.map_or(i64::MAX, |u| u.saturating_sub(1)), |f| f.key))]),
        };
        let mut stmt = self.prepare(sql)?;
        let (mut hits, mut looked, mut last, mut more) = (Vec::new(), 0usize, None, false);
        self.run_while(&mut stmt, &params, |r| {
            let (rowid, key) = (r.get::<i64>(0).map_err(fail)?, r.get::<i64>(1).map_err(fail)?);
            // Looked at already (the same key as where it goes on from, a rowid not before it).
            if from.is_some_and(|f| key == f.key && rowid >= f.rowid) {
                return Ok(true);
            }
            let at = r.get::<i64>(2).unwrap_or(0);
            // Across agents it goes back in time: older than it looks back to, nothing more fits.
            if q.session.is_none() && q.since.is_some_and(|s| at < s) {
                return Ok(false);
            }
            looked += 1;
            last = Some(SearchFrom { key, rowid });
            let column = |i: usize| r.get::<String>(i).ok();
            let fits = q.since.is_none_or(|s| at >= s)
                && q.until.is_none_or(|u| at < u)
                && [(&q.kind, 3), (&q.tool, 4), (&q.status, 5)].into_iter().all(|(w, i)| w.as_deref().is_none_or(|w| column(i).as_deref() == Some(w)))
                && (words.is_empty() || {
                    let text = column(6).unwrap_or_default().to_ascii_lowercase();
                    words.iter().all(|w| text.contains(w.as_str()))
                });
            if fits {
                hits.push(rowid);
            }
            more = hits.len() >= want || looked >= rows;
            Ok(!more)
        })?;
        let mut whole = self.prepare(sql::EVENT_BY_ROWID)?;
        let mut found = Vec::with_capacity(hits.len());
        for rowid in hits {
            self.run(&mut whole, &[int(rowid)], |r| {
                found.push(event_of(r)?);
                Ok(())
            })?;
        }
        Ok((found, if more { last } else { None }))
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
        let all = t.changes(0, 1 << 20, false)?;
        let mut kinds: Vec<String> = all.rows.iter().map(|c| format!("{}:{}", c.row.session(), match &c.row { Row::Session { .. } => "session", Row::Closed { .. } => "closed", Row::Event(_) => "event", _ => "other" })).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["a1:event", "a1:event", "a1:session", "a2:closed", "a2:session"]);
        // Numbers of their own, none twice, and what's written next comes after them.
        let mut revs: Vec<u64> = all.rows.iter().filter(|c| !matches!(c.row, Row::Closed { .. })).map(|c| c.rev).collect();
        revs.dedup();
        assert_eq!(revs.len(), 4);
        let top = all.upto;
        t.apply(&[Row::Event(Box::new(super::super::rows::event("a1", &Event { id: "m3".into(), seq: 3, at: 9, parent: None, by: None, body: Body::Assistant { text: "new".into(), streaming: false } }).ok_or_else(|| anyhow::anyhow!("no row"))?))])?;
        let next = t.changes(top, 1 << 20, false)?;
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
        Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc".into(), n, end: (n + 1) * 1000, line: line.into(), kept: None }))
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
                    rows.push(Row::Record(Box::new(RecordRow { session: s.into(), source: source.into(), n, end: (n + 1) * 100, line: format!("{{\"n\":{n}}}"), kept: None })));
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
        let line = |s: &str, n: u64| Row::Record(Box::new(RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: format!("{{\"n\":{n}}}"), kept: None }));
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
    fn a_store_made_compact_a_step_at_a_time_keeps_everything_written_meanwhile_and_goes_on() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        // The store compacted, and one given the same writes and left alone, to hold it up to.
        let (mut t, mut same) = (Turso::open(&path)?, Turso::open(&dir.path().join("same.db"))?);
        let line = |s: &str, src: &str, n: u64, text: String| Row::Record(Box::new(RecordRow { session: s.into(), source: src.into(), n, end: n + 1, line: text, kept: None }));
        let mut rows = vec![Row::Session { session: "gone".into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 }];
        rows.extend((0..1_500u64).map(|n| line("gone", "cc", n, noise(2_000))));
        rows.push(Row::Closed { session: "gone".into(), at: 10 });
        rows.extend((0..40u64).map(|n| line("kept", "cc", n, noise(500))));
        rows.extend(super::super::rows::of("kept", &said("m1", 1, "the clock was read twice"), 5));
        for s in [&mut t, &mut same] {
            s.apply(&rows)?;
            while s.prune_some(1_000, 500)? > 0 {}
        }
        let id = t.id();
        // What is written while it compacts, one more each step: rows of every kind, a drop of
        // events and of a record's file, where a session runs (no number of its own), reopened.
        let meanwhile: Vec<Vec<Row>> = vec![
            super::super::rows::of("kept", &said("m2", 2, "written while copying"), 6),
            vec![line("kept", "cc", 40, noise(300)), line("kept", "cx", 0, noise(300))],
            vec![Row::Machine { session: "kept".into(), machine: "box-2".into() }],
            vec![Row::Reset { session: "kept".into(), from: Some(2), at: 7 }],
            super::super::rows::of("kept", &said("m3", 2, "after the reset"), 8),
            vec![Row::RecordReset { session: "kept".into(), source: "cx".into(), at: 9 }, line("kept", "cx", 0, "{\"fresh\":true}".into())],
            vec![Row::Session { session: "new".into(), info: Some("{\"n\":1}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 11 }],
            vec![Row::Closed { session: "new".into(), at: 12 }, Row::Session { session: "new".into(), info: None, meta: None, state: None, changes: None, epoch: None, open: true, at: 13 }],
        ];
        // Not worth it below the bar.
        let (engine, not) = Box::new(t).compact_some(u64::MAX, 64 << 10)?;
        assert_eq!(not, Compacting::Not);
        let mut engine = engine;
        let (mut steps, mut writes) = (0, meanwhile.iter());
        let (before, after) = loop {
            let (e, step) = engine.compact_some(0, 64 << 10)?;
            engine = e;
            match step {
                // The copier writes meanwhile; the writer comes back after a while.
                Compacting::Going => std::thread::sleep(std::time::Duration::from_millis(2)),
                Compacting::Done { before, after } => break (before, after),
                Compacting::Not => anyhow::bail!("compacting stopped after {steps} steps"),
            }
            steps += 1;
            anyhow::ensure!(steps < 50_000, "compacting never ended");
            if let Some(rows) = writes.next() {
                engine.apply(rows)?;
                same.apply(rows)?;
            }
        };
        assert!(writes.next().is_none(), "every write landed while it compacted ({steps} steps)");
        assert!(after * 2 < before, "{before} -> {after}");
        // The store goes on in the compact file, holding what the one left alone holds, under the
        // same numbers.
        engine.apply(&[line("kept", "cc", 41, "{\"after\":true}".into())])?;
        same.apply(&[line("kept", "cc", 41, "{\"after\":true}".into())])?;
        drop(engine);
        let t = Turso::open(&path)?;
        assert_eq!(t.id(), id);
        assert_eq!(t.rev, same.rev);
        assert_eq!(dump(&t)?, dump(&same)?);
        for left in [".compact", ".compact.tmp", ".compact.tmp-wal", ".compact.tmp-shm"] {
            assert!(!beside(&path, left).exists(), "{left} left beside the store");
        }
        Ok(())
    }

    #[test]
    fn a_compaction_stopped_part_way_leaves_the_store_as_it_was() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let mut t = Turso::open(&path)?;
        let line = |n: u64, text: String| Row::Record(Box::new(RecordRow { session: "gone".into(), source: "cc".into(), n, end: n + 1, line: text, kept: None }));
        t.apply(&[Row::Session { session: "gone".into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 }])?;
        t.apply(&(0..1_000u64).map(|n| line(n, noise(2_000))).collect::<Vec<_>>())?;
        t.apply(&[Row::RecordReset { session: "gone".into(), source: "cc".into(), at: 6 }, line(0, "{\"kept\":true}".into())])?;
        let was = dump(&t)?;
        let mut engine: Box<dyn Engine> = Box::new(t);
        for _ in 0..3 {
            let (e, step) = engine.compact_some(0, 16 << 10)?;
            engine = e;
            assert_eq!(step, Compacting::Going);
        }
        assert!(beside(&path, ".compact.tmp").exists());
        // Stopped (kiln stops): the copy goes when the store opens again, which holds what it did.
        drop(engine);
        let t = Turso::open(&path)?;
        assert!(!beside(&path, ".compact.tmp").exists());
        assert_eq!(dump(&t)?, was);
        // And compacting can begin again, and end.
        let mut engine: Box<dyn Engine> = Box::new(t);
        loop {
            let (e, step) = engine.compact_some(0, 1 << 20)?;
            engine = e;
            match step {
                Compacting::Going => std::thread::sleep(std::time::Duration::from_millis(2)),
                Compacting::Done { .. } => break,
                Compacting::Not => anyhow::bail!("compacting stopped"),
            }
        }
        drop(engine);
        assert_eq!(dump(&Turso::open(&path)?)?, was);
        Ok(())
    }

    #[test]
    fn a_search_a_slice_at_a_time_finds_what_one_look_does_once_each_newest_first() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("c.db"))?;
        // 300 events of three agents, seven at each instant (the key a slice goes on from), a third
        // of them saying the word.
        let rows: Vec<Row> = (0..300u64)
            .map(|n| {
                let text = if n % 3 == 0 { format!("the needle {n}") } else { format!("hay {n}") };
                let at = 1_000 + i64::try_from(n / 7).unwrap_or(0);
                Row::Event(Box::new(EventRow { session: format!("a{}", n % 3), id: format!("e{n}"), seq: n, at, parent: None, by: None, kind: "assistant".into(), level: None, tool: None, status: None, text, body: "{}".into() }))
            })
            .collect();
        t.apply(&rows)?;
        let needle = |session: Option<&str>| Query { q: "NEEDLE".into(), session: session.map(str::to_owned), ..Query::default() };
        for session in [None, Some("a0")] {
            let q = needle(session);
            let (all, rest) = t.search_some(&q, None, usize::MAX, usize::MAX)?;
            assert!(rest.is_none());
            assert_eq!(all.len(), 100, "{session:?}");
            // Newest first.
            assert!(all.windows(2).all(|w| matches!(w, [a, b] if (a.at, a.seq) >= (b.at, b.seq))), "{session:?}");
            // Seven at a time, each slice going on from where the last ended: the same, once each.
            let (mut sliced, mut from, mut slices) = (Vec::new(), None, 0);
            loop {
                let (some, next) = t.search_some(&q, from, 7, usize::MAX)?;
                sliced.extend(some);
                slices += 1;
                match next {
                    Some(n) => from = Some(n),
                    None => break,
                }
            }
            assert!(slices > 10);
            assert_eq!(sliced.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), all.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), "{session:?}");
        }
        // As many as wanted, then where to go on from; a window of time; nothing for a word none says.
        let (two, next) = t.search_some(&needle(None), None, usize::MAX, 2)?;
        assert_eq!((two.len(), next.is_some()), (2, true));
        let (all, _) = t.search_some(&needle(None), None, usize::MAX, usize::MAX)?;
        let window = Query { since: Some(1_010), until: Some(1_020), ..needle(None) };
        let (within, _) = t.search_some(&window, None, usize::MAX, usize::MAX)?;
        assert_eq!(within.len(), all.iter().filter(|e| (1_010..1_020).contains(&e.at)).count());
        assert!(!within.is_empty());
        let none = Query { q: "nowhere".into(), ..Query::default() };
        assert_eq!(t.search_some(&none, None, usize::MAX, usize::MAX)?, (Vec::new(), None));
        Ok(())
    }

    /// Every table's rows (rowid aside), sorted, but the store's own id: what two stores holding
    /// the same things have alike.
    fn dump(t: &Turso) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        let mut out = Vec::new();
        for table in t.tables()? {
            let (rows, _) = t.rows_from(&format!("SELECT rowid, * FROM {table}"), &[], usize::MAX)?;
            let mut rows: Vec<String> = rows.iter().filter(|r| !(table == "meta" && r.first().and_then(Value::to_text) == Some("id"))).map(|r| format!("{r:?}")).collect();
            rows.sort();
            out.push((table, rows));
        }
        Ok(out)
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
            t.apply(&[Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc".into(), n: 0, end: 1, line: "{\"in\":\"the copy\"}".into(), kept: None }))])?;
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
        let fed: Vec<String> = t.changes(before, 1 << 20, false)?.rows.into_iter().filter_map(|c| if let Row::Record(r) = c.row { Some(r.line) } else { None }).collect();
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
            let store = super::super::store::Store::start(Box::new(Turso::open(&path)?), super::super::store::Upkeep::default())?;
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
        let store = super::super::store::Store::start(Box::new(Turso::open(&path)?), super::super::store::Upkeep::default())?;
        let k = store.kept("a1", 5_000)?.ok_or_else(|| anyhow::anyhow!("nothing kept after reopening"))?;
        assert_eq!(k.events.len(), 2_000);
        assert!(k.events.iter().any(|e| e.id == "m7" && e.text == "reply seven, revised"));
        Ok(())
    }

    #[test]
    fn small_batches_bring_everything_once_in_order_and_a_line_sent_as_kept_is_kept_the_same() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut a = Turso::open(&dir.path().join("a.db"))?;
        let mut b = Turso::open(&dir.path().join("b.db"))?;
        // Sessions and long lines, one after another: two kinds, each read a page at a time.
        let mut rows = Vec::new();
        for n in 0..40u64 {
            rows.push(Row::Session { session: format!("x{n}"), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 });
            rows.push(Row::Record(Box::new(RecordRow { session: "s1".into(), source: "cc".into(), n, end: (n + 1) * 100, line: format!("{{\"n\":{n},\"text\":\"{}\"}}", "words that compress well ".repeat(400)), kept: None })));
        }
        a.apply(&rows)?;
        let (mut from, mut seen) = (0u64, Vec::new());
        loop {
            let batch = a.changes(from, 2_000, true)?;
            if batch.rows.is_empty() {
                break;
            }
            let sizes: Vec<usize> = batch.rows.iter().map(|c| c.row.bytes()).collect();
            assert!(sizes.iter().sum::<usize>() < 2_000 + sizes.iter().max().copied().unwrap_or(0), "a batch of {sizes:?}");
            seen.extend(batch.rows.iter().map(|c| c.rev));
            let took = b.take("a", None, from, &batch)?;
            assert!(!took.gap);
            from = batch.upto;
        }
        assert_eq!(seen, (1..=80).collect::<Vec<u64>>(), "everything, once, in order");
        // Each long line is kept byte for byte as it was: never decompressed on the way.
        let raw = |t: &Turso| -> anyhow::Result<Vec<Vec<u8>>> {
            let mut stmt = t.prepare("SELECT line FROM records ORDER BY n")?;
            let mut out = Vec::new();
            t.run(&mut stmt, &[], |r| {
                out.push(r.get_value(0).to_blob().map(<[u8]>::to_vec).unwrap_or_default());
                Ok(())
            })?;
            Ok(out)
        };
        let (kept_a, kept_b) = (raw(&a)?, raw(&b)?);
        assert_eq!(kept_a.len(), 40);
        assert!(kept_a.iter().all(|l| l.first() == Some(&CODEC_BROTLI)));
        assert_eq!(kept_a, kept_b);
        assert_eq!(b.record("s1", "cc", 39, 1)?.first().map(|(n, l)| (*n, l.starts_with("{\"n\":39,"))), Some((39, true)));
        Ok(())
    }
}
