//! The store's engine: Turso, embedded (`docs/conversations.md`, "The store"), vendored in
//! `vendor/turso`. One connection writes, owned by the store's writer thread; another reads beside it
//! on the store's reader thread ([`TursoRead`]; plan 01a1cc09, step 2). Every statement is driven to
//! its end on its connection's thread (`PlatformIO` does its I/O inline, so `step` waits on nothing).

use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use turso_core::{CheckpointMode, Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, Row as TRow, SqliteDialect, Statement, StepResult, Value};

use std::collections::HashMap;

use super::rows::{EventRow, InboxRow, RecordRow, Row};
use super::sql;
use super::store::{Change, Changes, Compacting, Engine, InboxPage, Kept, Look, Mark, ProjectionWrite, Query, Read, SearchFrom, Took, Unended};
use super::telemetry::CompactView;

#[derive(serde::Deserialize)]
struct HeadEpoch { epoch: u64 }

#[derive(Clone, Copy)]
enum LocalInbox {
    Active,
    Fenced,
    NeedsHead(u64),
}

/// Rows of each kind a feed reads at once.
const FEED_PAGE: usize = 512;
/// The big tables, copied a batch at a time while compacting: every row of them has a number
/// (`rev`), so what is written meanwhile is copied again by its number, and what a drop took away
/// is taken away from the copy too. The other tables are small, and go whole at the swap.
const COMPACT_SCANNED: [&str; 5] = ["sessions", "events", "records", "drops", "inbox"];
/// These tables mutate and delete rows without assigning a conversation revision. A compact
/// copy keeps their rowids and follows a connection-local dirty set until the file swap.
const RAFT_COPY: [(&str, &str); 4] = [
    ("inbox_outbox", "session,rev,ord,family,key,value,epoch,row_body"),
    ("convo_raft_state", "session,applied,term,complete,inbox_epoch,verify_events,owner,genesis"),
    ("convo_local_owner_fence", "session,epoch"),
    ("convo_raft_cells", "session,kind,source,id,n,body,ordinal"),
];

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
    inbox_replication: bool,
    local_owner: Option<String>,
    has_raft_schema: bool,
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
    // Drop after closing the engine, so a handoff cannot publish live SQLite files.
    _handoff: std::fs::File,
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
    /// (source rowid, presence, columns). An absent source row is deleted from the copy.
    Patch(&'static str, &'static str, Vec<Vec<Value>>, bool),
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
                    CopyJob::Patch(table, columns, rows, flush) => {
                        copy.transaction(|t| t.patch_copy(table, columns, &rows)).and_then(|()| if flush { copy.flush() } else { Ok(()) })
                    }
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

/// Bytes, as a blob is bound.
fn bytes(b: &[u8]) -> anyhow::Result<Value> {
    Value::from_slice(b).map_err(|e| anyhow::anyhow!("a value couldn't be held: {e}"))
}

fn encoded_row(row: &Row) -> anyhow::Result<Value> {
    let mut body = turso_core::ValueBlob::default();
    serde_json::to_writer(&mut body,row)?;
    Ok(Value::from_blob(body))
}

/// The first key past every key that starts with `prefix` (None: there's none).
fn past(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if let Some(next) = last.checked_add(1) {
            end.push(next);
            return Some(end);
        }
    }
    None
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
        let handoff = super::file_move::open_guard(path)?;
        finish_compact(path)?;
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("the store's path isn't UTF-8: {}", path.display()))?;
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().map_err(fail)?);
        // Opened once, never waited for: Turso's lock on the file is its process's (an fcntl lock),
        // so none of this process's own holds it against it, and another process's store is let go
        // of before it's handed on (`super::store::Store::close` answers once its files are closed).
        // It waited up to 15 s for a runner that hadn't let go yet, upgraded.
        let db = Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new())).map_err(fail)?;
        let conn = db.connect(ConnectOpts::new()).map_err(fail)?;
        let mut t = Self { io, _db: db, conn, path: path.to_path_buf(), id: String::new(), rev: 0, squeeze_below: 0, inbox_replication: false, local_owner: None, has_raft_schema: schema >= 7, copying: false, filling: false, compaction: None, touched: std::cell::RefCell::default(), _handoff: handoff };
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
        if schema >= 5 {
            rev = rev.max(t.one_int("SELECT MAX(rev) FROM inbox", &[])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0));
        }
        t.rev = rev;
        if schema >= 7 {
            t.inbox_replication = t.meta("inbox_raft_outbox")?.is_some();
            t.local_owner = t.meta("inbox_raft_owner")?;
        }
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
            let mut epochs: HashMap<&str, (u64, bool)> = HashMap::new();
            let mut copied = HashMap::new();
            let mut raft_groups = HashMap::new();
            let mut took = Took { rev: batch.upto, ..Took::default() };
            let first = t.rev + 1;
            for c in &batch.rows {
                let session = c.row.session();
                if t.has_raft_schema {
                    let grouped = match raft_groups.get(session) {
                        Some(grouped) => *grouped,
                        None => {
                            let grouped = t.one_int("SELECT 1 FROM convo_raft_state WHERE session=?1", &[txt(session)])?.is_some();
                            raft_groups.insert(session, grouped);
                            grouped
                        }
                    };
                    if grouped {
                        // A legacy feed can arrive after a committed tombstone or ownership change.
                        // Only the session's committed log may advance the working replica.
                        took.fenced += 1;
                        continue;
                    }
                }
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
                            copied.insert(session, o);
                            o
                        }
                    };
                    // What the copy dropped is gone from it already: nothing to drop here.
                    if !ours || matches!(c.row, Row::Reset { .. } | Row::RecordReset { .. }) {
                        took.left += 1;
                        continue;
                    }
                }
                let (current, accepted) = match epochs.entry(session) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => e.insert((t.epoch_of(session)?, false)),
                };
                // A former writer's rows (it moved on elsewhere) never count again.
                if c.epoch < *current {
                    took.fenced += 1;
                    continue;
                }
                *accepted = true;
                if c.epoch > *current {
                    // Its new writer, from here on: said before its rows (it may be new here).
                    t.rev += 1;
                    let r = t.rev;
                    let stmt = Writes::get(&mut w.session, t, sql::SESSION_UPSERT)?;
                    t.run(stmt, &[txt(session), Value::Null, Value::Null, Value::Null, Value::Null, int(now), seq(c.epoch), seq(r)], |_| Ok(()))?;
                    *current = c.epoch;
                }
                t.rev += 1;
                let r = t.rev;
                if t.write(&mut w, &c.row, r)? {
                    took.applied += 1;
                } else {
                    // As it is kept here already (sent again, from another of its machine's stores).
                    t.rev -= 1;
                    took.same += 1;
                }
            }
            // Where each session it brought runs: the machine of this feed (a former writer's
            // rows, left out, don't say so). A mirror's feed says it in its rows instead.
            if let Some(machine) = &machine {
                let mut at = t.prepare(sql::SESSION_MACHINE)?;
                for (session, (_, accepted)) in epochs {
                    if accepted {
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
                if let Err(e) = self.exec("COMMIT") {
                    let _ = self.exec("ROLLBACK");
                    self.rev = before;
                    return Err(e);
                }
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

    /// Its writer's connection ([`Link`]).
    fn link(&self) -> Link<'_> {
        Link { io: &self.io, conn: &self.conn, busy: None }
    }

    fn prepare(&self, sql: &str) -> anyhow::Result<Statement> {
        self.link().prepare(sql)
    }

    /// Run `stmt` with `params`, handing each row to `each`.
    fn run(&self, stmt: &mut Statement, params: &[Value], each: impl FnMut(&TRow) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.link().run(stmt, params, each)
    }

    /// A writer never retries a busy statement. Transfer large values into its bindings once.
    fn write_bound<const N: usize>(&self, stmt: &mut Statement, values: [Value; N]) -> anyhow::Result<()> {
        let mut values = Some(values);
        self.link().bound_rows(stmt, |stmt| {
            stmt.reset().map_err(fail)?;
            let values = values.take().ok_or_else(|| anyhow::anyhow!("writer attempted a second binding"))?;
            for (i,value) in values.into_iter().enumerate() { stmt.bind_at(at(i+1)?,value).map_err(fail)?; }
            Ok(())
        }, |_| Ok(true))
    }

    /// [`Turso::run`], stopping once `each` says no more (false).
    fn run_while(&self, stmt: &mut Statement, params: &[Value], each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        self.link().run_while(stmt, params, each)
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
}

/// How long a reader beside the writer waits out a lock the writer holds for a moment (a checkpoint's) before its read
/// fails ([`Link::run_while`]); the store's wait for its answer bounds it too (`super::store::READ_WAIT`).
const READ_BUSY: Duration = Duration::from_secs(5);
/// How long a reader waits before trying a statement Turso said busy for again.
const BUSY_AGAIN: Duration = Duration::from_millis(2);

/// A connection to the store's file and the I/O it steps on: the writer's ([`Turso`]), or a reader's beside it
/// ([`TursoRead`]; plan 01a1cc09, step 2), which waits out a lock the writer holds (`busy`: until when).
struct Link<'a> {
    io: &'a Arc<dyn IO>,
    conn: &'a Arc<Connection>,
    busy: Option<Instant>,
}

impl Link<'_> {
    fn has_legacy_name_catalog(&self)->anyhow::Result<bool> {
        let mut q=self.prepare("SELECT 1 FROM convo_name_origin o JOIN meta m ON m.key='id' AND m.value=o.store_id")?;
        let mut present=false;
        self.run(&mut q,&[],|_| {present=true; Ok(())})?;
        Ok(present)
    }

    /// The namespace transition needs every durable name, including partly imported rows
    /// with no session header. Seek past each name rather than scanning its transcript.
    fn historical_names(&self) -> anyhow::Result<std::collections::BTreeSet<String>> {
        let control = |sql: &str| self.run(&mut self.prepare(sql)?, &[], |_| Ok(()));
        control("BEGIN")?;
        let result = (|| {
            let mut names=std::collections::BTreeSet::new();
            for (table, column) in [("sessions","id"),("events","session"),("records","session"),("inbox","session"),
                ("drops","session"),("convo_raft_state","session"),("inbox_outbox","session")] {
                let mut q=self.prepare(&format!("SELECT {column} FROM {table} WHERE {column}>?1 ORDER BY {column} LIMIT 1"))?;
                let mut cursor=String::new();
                loop {
                    let mut next=None;
                    self.run(&mut q,&[txt(&cursor)], |r| {next=Some(r.get::<String>(0).map_err(fail)?);Ok(())})?;
                    let Some(next)=next else {break};
                    names.insert(next.clone());
                    cursor=next;
                }
            }
            Ok(names)
        })();
        let close=control("ROLLBACK");
        close?;
        result
    }

    /// Pin one WAL read transaction while importing the prior local format. Another session
    /// can keep writing and this image still represents one instant.
    fn founding_rows(&self, session: &str, epoch: u64, owner: &str) -> anyhow::Result<Vec<Row>> {
        let control = |sql: &str| self.run(&mut self.prepare(sql)?, &[], |_| Ok(()));
        control("BEGIN")?;
        let result = (|| {
            let mut output = Vec::new();
            let mut owned = 0usize;
            let mut seen = false;
            let mut append = |row: Row| -> anyhow::Result<()> {
                owned = owned
                    .checked_add(crate::memory::queue::json_bytes(&row))
                    .ok_or_else(|| anyhow::anyhow!("legacy conversation size overflow"))?;
                anyhow::ensure!(
                    owned <= crate::memory::room::share(),
                    "legacy conversation exceeds the current Raft proposal memory budget"
                );
                output.try_reserve(1)?;
                output.push(row);
                Ok(())
            };
            let mut meta=self.prepare("SELECT info,meta,state,changes,updated_at,closed_at,epoch,machine FROM sessions WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM convo_raft_state WHERE session=?1 AND (applied<>0 OR complete=0))")?;
            self.run(&mut meta, &[txt(session)], |r| {
                let text = |i| r.get_value(i).to_text().map(str::to_owned);
                let machine = text(7);
                anyhow::ensure!(machine.as_deref().is_none_or(|n| n == owner), "legacy session belongs to another node");
                let held = u64::try_from(r.get::<i64>(6).map_err(fail)?)?;
                anyhow::ensure!(held <= epoch, "legacy session has a newer owner");
                let closed = r.get::<i64>(5).ok();
                seen = true;
                append(Row::Session {
                    session: session.to_owned(),
                    info: text(0),
                    meta: text(1),
                    state: text(2),
                    changes: text(3),
                    epoch: Some(epoch),
                    open: closed.is_none(),
                    at: r.get::<i64>(4).map_err(fail)?,
                })?;
                if let Some(at) = closed {
                    append(Row::Closed {
                        session: session.to_owned(),
                        at,
                    })?;
                }
                Ok(())
            })?;
            if !seen {
                return Ok(output);
            }
            let mut events =
                self.prepare("SELECT session,id,seq,at,parent,by,kind,level,tool,status,text,body FROM events WHERE session=?1 ORDER BY seq,rev,id")?;
            self.run(&mut events, &[txt(session)], |r| append(Row::Event(Box::new(event_of(r)?))))?;
            let mut records = self.prepare("SELECT source,n,line_end,line FROM records WHERE session=?1 ORDER BY source,n")?;
            self.run(&mut records, &[txt(session)], |r| {
                let stored = r.get_value(3);
                let (line, kept) = match stored.to_blob() {
                    Some(b) if b.first() == Some(&CODEC_BROTLI) => (String::new(), Some(b.to_vec())),
                    _ => (unsqueeze(stored)?, None),
                };
                append(Row::Record(Box::new(RecordRow {
                    session: session.to_owned(),
                    source: r.get::<String>(0).map_err(fail)?,
                    n: u(r, 1),
                    end: u(r, 2),
                    line,
                    kept,
                })))?;
                Ok(())
            })?;
            let mut inbox = self.prepare("SELECT family,key,value,rev FROM inbox WHERE session=?1 ORDER BY rev,rowid")?;
            self.run(&mut inbox, &[txt(session)], |r| {
                let family = r.get::<String>(0).map_err(fail)?;
                let value = r
                    .get_value(2)
                    .to_blob()
                    .map(|v| crate::harness::inbox::with_receipt_sequence(&family, v, u(r, 3)))
                    .transpose()?;
                append(Row::Inbox(Box::new(InboxRow {
                    session: session.to_owned(),
                    family,
                    key: r.get_value(1).to_blob().ok_or_else(|| anyhow::anyhow!("invalid legacy inbox key"))?.to_vec(),
                    value,
                })))?;
                Ok(())
            })?;
            Ok(output)
        })();
        match result {
            Ok(rows) => {
                if let Err(e) = control("COMMIT") {
                    let _ = control("ROLLBACK");
                    return Err(e);
                }
                Ok(rows)
            }
            Err(e) => {
                control("ROLLBACK")?;
                Err(e)
            }
        }
    }

    fn prepare(&self, sql: &str) -> anyhow::Result<Statement> {
        self.conn.prepare(sql).map_err(fail)
    }

    /// The frontier and projected cells share a single SQLite read view, even when an event's
    /// ID and sequence are reused by a later edit while a snapshot is being encoded.
    fn raft_snapshot_rows(&self, session: &str, at: u64, pinned: &mut dyn FnMut()) -> anyhow::Result<Vec<Row>> {
        let mut stmt = self.prepare("SELECT s.applied,s.complete,c.body FROM convo_raft_state s LEFT JOIN convo_raft_cells c ON c.session=s.session WHERE s.session=?1 ORDER BY c.kind,c.source,c.n,c.ordinal,c.id")?;
        let mut found = false;
        let mut rows = Vec::new();
        self.run(&mut stmt, &[txt(session)], |r| {
            anyhow::ensure!(r.get::<i64>(1).map_err(fail)? != 0, "legacy Raft group requires a complete committed snapshot");
            anyhow::ensure!(u(r,0) <= at, "Raft projection advanced beyond snapshot index {at}");
            if !found {
                found = true;
                pinned();
            }
            if !matches!(r.get_value(2), Value::Null) {
                let data = r.get_value(2).to_blob().ok_or_else(|| anyhow::anyhow!("invalid projected row"))?;
                rows.push(serde_json::from_slice(data)?);
            }
            Ok(())
        })?;
        anyhow::ensure!(found, "Raft projection is missing for {session}");
        Ok(rows)
    }

    /// As [`super::store::Engine::inbox_get`], on this link's connection.
    fn inbox_get(&self, family: &str, key: &[u8]) -> anyhow::Result<Option<(Vec<u8>, u64)>> {
        let mut stmt = self.prepare(sql::INBOX_GET)?;
        let mut out = None;
        self.run(&mut stmt, &[txt(family), bytes(key)?], |r| {
            if let Some(v) = r.get_value(0).to_blob() {
                out = Some((v.to_vec(), u(r, 1)));
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// As [`super::store::Engine::inbox_scan`], on this link's connection.
    fn inbox_scan(&self, family: &str, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> anyhow::Result<InboxPage> {
        let mut stmt = self.prepare(sql::INBOX_SCAN)?;
        let end = if prefix.is_empty() { None } else { past(prefix) };
        let after = match after {
            Some(a) => bytes(a)?,
            None => Value::Null,
        };
        let end = match end {
            Some(e) => bytes(&e)?,
            None => Value::Null,
        };
        let want = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut rows = Vec::new();
        self.run(&mut stmt, &[txt(family), bytes(prefix)?, after, end, int(want)], |r| {
            let key = r.get_value(0).to_blob().map(<[u8]>::to_vec).unwrap_or_default();
            let value = r.get_value(1).to_blob().map(<[u8]>::to_vec).unwrap_or_default();
            rows.push((key, value, u(r, 2)));
            Ok(())
        })?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, more))
    }

    /// As [`super::store::Engine::unended`], on this link's connection (a reader's beside the writer).
    fn unended(&self, since: i64) -> anyhow::Result<Vec<Unended>> {
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

    /// As [`super::store::Read::restorable`], on this link's connection: its row by its key, its newest events by
    /// `events_by_seq` alone.
    fn restorable(&self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let mut get = self.prepare(sql::SESSION_GET)?;
        let mut kept: Option<Kept> = None;
        self.run(&mut get, &[Value::from_text(session.to_owned())], |r| {
            let s = |i: usize| r.get::<String>(i).ok();
            kept = Some(Kept { info: s(0), meta: s(1), state: s(2), changes: s(3), updated_at: r.get::<i64>(4).unwrap_or(0), closed_at: r.get::<i64>(5).ok(), events: Vec::new() });
            Ok(())
        })?;
        let Some(mut k) = kept else { return Ok(None) };
        let mut events = self.event_rows(sql::EVENTS_NEWEST, &[Value::from_text(session.to_owned()), int(i64::try_from(last).unwrap_or(i64::MAX))])?;
        events.reverse();
        k.events = events;
        Ok(Some(k))
    }

    /// As [`super::store::Engine::ended`], on this link's connection.
    fn ended(&self, after: (i64, &str), limit: usize) -> anyhow::Result<Vec<(String, i64)>> {
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

    /// As [`super::store::Engine::kept`], on this link's connection.
    fn kept(&self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let mut get = self.prepare(sql::SESSION_GET)?;
        let mut kept: Option<Kept> = None;
        self.run(&mut get, &[Value::from_text(session.to_owned())], |r| {
            let s = |i: usize| r.get::<String>(i).ok();
            kept = Some(Kept { info: s(0), meta: s(1), state: s(2), changes: s(3), updated_at: r.get::<i64>(4).unwrap_or(0), closed_at: r.get::<i64>(5).ok(), events: Vec::new() });
            Ok(())
        })?;
        let mut events = self.newest(session, None, last)?;
        if kept.is_none() && events.is_empty() {
            return Ok(None);
        }
        events.reverse();
        let mut k = kept.unwrap_or_default();
        k.events = events;
        Ok(Some(k))
    }

    /// Run `stmt` with `params`, handing each row to `each`.
    fn run(&self, stmt: &mut Statement, params: &[Value], mut each: impl FnMut(&TRow) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.run_while(stmt, params, |r| each(r).map(|()| true))
    }

    /// [`Link::run`], stopping once `each` says no more (false).
    fn run_while(&self, stmt: &mut Statement, params: &[Value], each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let bind = |stmt: &mut Statement| -> anyhow::Result<()> {
            stmt.reset().map_err(fail)?;
            for (i, p) in params.iter().enumerate() {
                stmt.bind_at(at(i + 1)?, p.clone()).map_err(fail)?;
            }
            Ok(())
        };
        self.bound_rows(stmt,bind,each)
    }

    fn bound_rows(&self, stmt: &mut Statement, mut bind: impl FnMut(&mut Statement) -> anyhow::Result<()>, mut each: impl FnMut(&TRow) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        bind(stmt)?;
        let mut rows = false;
        loop {
            match stmt.step().map_err(fail)? {
                StepResult::Row => {
                    rows = true;
                    if let Some(row) = stmt.row()
                        && !each(row)?
                    {
                        return stmt.reset().map_err(fail);
                    }
                }
                StepResult::Done => return Ok(()),
                StepResult::IO | StepResult::Yield => self.io.step().map_err(fail)?,
                // A reader's, before any row: the writer holds a lock it needs for a moment (a checkpoint's). From the
                // start again a little later, until its wait is out.
                StepResult::Busy | StepResult::Sleep { .. } if !rows && self.busy.is_some_and(|until| Instant::now() < until) => {
                    std::thread::sleep(BUSY_AGAIN);
                    bind(stmt)?;
                }
                // Never the writer's: Turso says busy only where a lock this statement needs is held by another
                // connection to the database (its WAL's read and write locks, VACUUM's), or where another
                // statement of the connection holds its statement journal, and asks for a wait (`Sleep`)
                // only where a busy handler says to try again. A store writes on one connection, its writer's (its
                // reader only reads, never taking the write lock): its file is opened once in a process (a second
                // open would share its database, Turso keeping one a file) and by no other process while it's open
                // (`Turso::open` is refused the file's lock); it runs a statement at a time, each to its end or
                // reset; it never vacuums, and sets no busy handler. So the writer's is a fault, said with the
                // statement it stopped, never waited out (it was tried 50 times, 10 ms apart); a reader's is past
                // its wait.
                StepResult::Busy | StepResult::Sleep { .. } => anyhow::bail!(
                    "the conversation store's database said busy for `{}`: a lock it needs is held by another connection to it, or its statement journal by another of its statements",
                    stmt.get_sql().lines().next().unwrap_or_default()
                ),
                StepResult::Interrupt => anyhow::bail!("a statement on the conversation store was interrupted"),
            }
        }
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

    /// [`Link::event_rows`] of a statement whose rows end with their durable revision (`sql::EVENTS_BEFORE_BY_SEQ` and
    /// its like): each event and that.
    fn event_rows_rev(&self, sql: &str, params: &[Value]) -> anyhow::Result<Vec<(EventRow, i64)>> {
        let mut stmt = self.prepare(sql)?;
        let mut out = Vec::new();
        self.run(&mut stmt, params, |r| {
            out.push((event_of(r)?, r.get::<i64>(sql::EVENT_COLUMNS).unwrap_or(0)));
            Ok(())
        })?;
        Ok(out)
    }

    /// A session's newest `limit` events (those before number `before`, where one is given), newest first, exactly as
    /// `sql::EVENTS_BEFORE` and `sql::EVENTS_LAST` order and bound them (by number, then the newest durable revision first:
    /// W21-R02, a replacement id at a number), read by `events_by_seq` alone: backwards from the page's end and stopped at
    /// the limit, so a page costs its own events wherever in its session it is (those two sorted every event before it:
    /// `sql::EVENTS_BEFORE_BY_SEQ`). The index gives the numbers in order and the events of one number in none, and the
    /// limit may cut the last number's short: that number's events are read whole, what was read is put in the order the
    /// sorted statements give (a stable sort, the index's order kept where number and revision are both the same), and the
    /// limit taken again.
    fn newest(&self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bound = int(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut rows = match before {
            Some(b) => self.event_rows_rev(sql::EVENTS_BEFORE_BY_SEQ, &[txt(session), seq(b), bound])?,
            None => self.event_rows_rev(sql::EVENTS_LAST_BY_SEQ, &[txt(session), bound])?,
        };
        if rows.len() >= limit
            && let Some(last) = rows.last().map(|(r, _)| r.seq)
        {
            rows.retain(|(r, _)| r.seq != last);
            rows.extend(self.event_rows_rev(sql::EVENTS_AT, &[txt(session), seq(last)])?);
        }
        rows.sort_by(|(a, a_rev), (b, b_rev)| b.seq.cmp(&a.seq).then(b_rev.cmp(a_rev)));
        rows.truncate(limit);
        Ok(rows.into_iter().map(|(r, _)| r).collect())
    }

    /// [`Engine::events`], on this connection.
    fn events(&self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let mut rows = self.newest(session, before, limit)?;
        rows.reverse();
        Ok(rows)
    }

    /// [`Engine::thread`], on this connection.
    fn thread(&self, session: &str, parent: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let limit = int(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut rows = self.event_rows(sql::EVENTS_OF_PARENT, &[Value::from_text(session.to_owned()), Value::from_text(parent.to_owned()), seq(before.unwrap_or(u64::MAX)), limit])?;
        rows.reverse();
        Ok(rows)
    }

    /// [`Engine::search_some`], on this connection.
    fn search_some(&self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)> {
        // Newest first by an index, so nothing is sorted and it stops once it has `want`; the words
        // matched here, as LIKE would (any case of an ASCII letter; `%` and `_` as they are).
        let words: Vec<String> = q.q.split_whitespace().map(str::to_ascii_lowercase).collect();
        let (sql, params) = match &q.session {
            Some(s) => {
                let before = q.before_seq.and_then(|n| i64::try_from(n).ok()).map_or(i64::MAX, |n| n.saturating_sub(1));
                let search = if q.before_seq.is_some() { sql::SEARCH_CANONICAL_SESSION } else { sql::SEARCH_SESSION };
                (search, vec![txt(s), int(from.map_or(before, |f| f.key))])
            }
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

    /// [`Engine::marks`], on this connection.
    fn marks(&self, session: &str) -> anyhow::Result<Vec<Mark>> {
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

    /// [`Engine::record`], on this connection.
    fn record(&self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        let mut stmt = self.prepare(sql::RECORD_LINES)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[txt(session), txt(source), seq(from), int(i64::try_from(limit).unwrap_or(i64::MAX))], |r| {
            out.push((u(r, 0), unsqueeze(r.get_value(1))?));
            Ok(())
        })?;
        Ok(out)
    }
}

/// A reader of a Turso store's file beside its writer ([`Read`]; plan 01a1cc09, step 2): a connection of its own to the
/// same database, reading what's committed, on the store's reader thread.
pub struct TursoRead {
    io: Arc<dyn IO>,
    conn: Arc<Connection>,
}

impl TursoRead {
    fn link(&self) -> Link<'_> {
        Link { io: &self.io, conn: &self.conn, busy: Some(Instant::now() + READ_BUSY) }
    }
}

impl Read for TursoRead {
    fn has_legacy_name_catalog(&mut self)->anyhow::Result<bool> {self.link().has_legacy_name_catalog()}
    fn historical_names(&mut self)->anyhow::Result<std::collections::BTreeSet<String>> { self.link().historical_names() }
    fn founding_rows(&mut self,session:&str,epoch:u64,owner:&str)->anyhow::Result<Vec<Row>> {self.link().founding_rows(session,epoch,owner)}
    fn raft_snapshot_rows(&mut self, session: &str, at: u64) -> anyhow::Result<Vec<Row>> {
        self.link().raft_snapshot_rows(session, at, &mut || {})
    }
    fn pinned_raft_rows(&mut self, session: &str, at: u64, pinned: &mut dyn FnMut()) -> anyhow::Result<Vec<Row>> {
        self.link().raft_snapshot_rows(session, at, pinned)
    }
    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        self.link().events(session, before, limit)
    }

    fn thread(&mut self, session: &str, parent: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        self.link().thread(session, parent, before, limit)
    }

    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>> {
        self.link().unended(since)
    }

    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        self.link().kept(session, last)
    }

    fn ended(&mut self, after: (i64, &str), limit: usize) -> anyhow::Result<Vec<(String, i64)>> {
        self.link().ended(after, limit)
    }

    fn restorable(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        self.link().restorable(session, last)
    }

    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)> {
        self.link().search_some(q, from, rows, want)
    }

    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>> {
        self.link().marks(session)
    }

    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        self.link().record(session, source, from, limit)
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

/// The statements a transaction writes with, lazily prepared on first use so a batch touching one
/// table prepares 1 statement instead of all 10.
#[derive(Default)]
struct Writes {
    inbox: Option<Statement>,
    session: Option<Statement>,
    machine: Option<Statement>,
    open: Option<Statement>,
    close: Option<Statement>,
    event: Option<Statement>,
    reset: Option<Statement>,
    dropped: Option<Statement>,
    record: Option<Statement>,
    records_reset: Option<Statement>,
    copied_drop: Option<Statement>,
}

impl Writes {
    fn get<'a>(slot: &'a mut Option<Statement>, t: &Turso, sql: &str) -> anyhow::Result<&'a mut Statement> {
        if slot.is_none() {
            *slot = Some(t.prepare(sql)?);
        }
        slot.as_mut().ok_or_else(|| anyhow::anyhow!("prepared statement missing"))
    }
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
    fn project_row(&self, session: &str, row: &Row, put: &mut Statement) -> anyhow::Result<()> {
        let key = match row {
            Row::Event(e) => Some(("event", bytes(&[])?, bytes(e.id.as_bytes())?, e.seq)),
            Row::Record(r) => Some(("record", bytes(r.source.as_bytes())?, bytes(&r.n.to_be_bytes())?, r.n)),
            Row::Inbox(i) if i.value.is_some() => Some(("inbox", bytes(i.family.as_bytes())?, bytes(&i.key)?, 0)),
            Row::Inbox(i) => {
                let mut del = self.prepare("DELETE FROM convo_raft_cells WHERE session=?1 AND kind='inbox' AND source=?2 AND id=?3")?;
                self.run(&mut del, &[txt(session), bytes(i.family.as_bytes())?, bytes(&i.key)?], |_| Ok(()))?;
                None
            }
            Row::RecordReset { source, .. } => {
                let mut del = self.prepare("DELETE FROM convo_raft_cells WHERE session=?1 AND kind='record' AND source=?2")?;
                self.run(&mut del, &[txt(session), bytes(source.as_bytes())?], |_| Ok(()))?;
                None
            }
            Row::Reset { from, .. } => {
                let mut del = self.prepare("DELETE FROM convo_raft_cells WHERE session=?1 AND kind='event' AND n>=?2")?;
                self.run(&mut del, &[txt(session), seq(from.unwrap_or(0))], |_| Ok(()))?;
                None
            }
            Row::Session { .. } | Row::Closed { .. } | Row::Machine { .. } => None,
        };
        if let Some((kind, source, id, n)) = key {
            self.write_bound(put, [txt(session), txt(kind), source, id, seq(n), encoded_row(row)?,seq(if kind=="event" {self.rev.checked_add(1).ok_or_else(||anyhow::anyhow!("event ordinal exhausted"))?}else{0})])?;
        }
        Ok(())
    }

    fn has_working_overlay(&self, session: &str, epoch: Option<u64>, row: &Row) -> anyhow::Result<bool> {
        let (family, key) = match row {
            Row::Inbox(inbox) => (inbox.family.as_str(), inbox.key.as_slice()),
            Row::Record(r) => ("", r.source.as_bytes()),
            Row::RecordReset { source, .. } => ("", source.as_bytes()),
            _ => return Ok(false),
        };
        Ok(self
            .one_int(
                "SELECT 1 FROM inbox_outbox WHERE session=?1 AND family=?2 AND key=?3 AND COALESCE(epoch,0)=COALESCE(?4,(SELECT inbox_epoch FROM convo_raft_state WHERE session=?1),0) LIMIT 1",
                &[txt(session), txt(family), bytes(key)?, epoch.map_or(Value::Null,seq)],
            )?
            .is_some())
    }

    fn outbox_row(session: &str, r: &TRow) -> anyhow::Result<Row> {
        Ok(if let Some(body) = r.get_value(5).to_blob() {
            serde_json::from_slice(body)?
        } else {
            let family = r.get::<String>(1).map_err(fail)?;
            let value = r
                .get_value(3)
                .to_blob()
                .map(|v| crate::harness::inbox::with_receipt_sequence(&family, v, u(r, 0)))
                .transpose()?;
            Row::Inbox(Box::new(InboxRow {
                session: session.to_owned(),
                family,
                key: r.get_value(2).to_blob().ok_or_else(|| anyhow::anyhow!("invalid inbox key"))?.to_vec(),
                value,
            }))
        })
    }

    /// Snapshot replacement restores committed rows first, then reinstates locally authored
    /// working custody in original transaction order. Confirmation and CAS remain independent.
    fn restore_outbox_overlay(&mut self, session: &str, epoch: u64, foreign: bool, writes: &mut Writes) -> anyhow::Result<()> {
        let mut remove = self.prepare("DELETE FROM inbox_outbox WHERE session=?1 AND COALESCE(epoch,0)<=?2")?;
        let last_abandoned = i64::try_from(epoch)? - i64::from(!foreign);
        self.run(&mut remove, &[txt(session), int(last_abandoned)], |_| Ok(()))?;
        if foreign { return Ok(()); }
        let mut query =
            self.prepare("SELECT rev,family,key,value,epoch,row_body FROM inbox_outbox WHERE session=?1 AND COALESCE(epoch,0)=?2 AND (row_body IS NULL OR length(key)>0) ORDER BY rev,ord")?;
        let mut latest = self.rev;
        self.run(&mut query, &[txt(session), seq(epoch)], |r| {
            let row = Self::outbox_row(session, r)?;
            let rev = latest.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            if self.write(writes, &row, rev)? {
                latest = rev;
            }
            Ok(())
        })?;
        self.rev = latest;
        Ok(())
    }

    /// The new committed owner supersedes local writes which never reached quorum. Restore only
    /// their affected keys from the committed projection, leaving the live CAS view intact for
    /// any later write at the new epoch. Read one original per key independent of backlog size.
    fn revert_abandoned_inbox(&mut self, session: &str, epoch: u64, foreign: bool) -> anyhow::Result<()> {
        let last_abandoned = i64::try_from(epoch)? - i64::from(!foreign);
        let mut next = self.prepare("SELECT family,key FROM inbox_outbox WHERE session=?1 AND COALESCE(epoch,0)<=?2 ORDER BY rev,ord LIMIT 1")?;
        let mut cell = self.prepare("SELECT body FROM convo_raft_cells WHERE session=?1 AND kind='inbox' AND source=?2 AND id=?3")?;
        let mut remove = self.prepare("DELETE FROM inbox_outbox WHERE session=?1 AND family=?2 AND key=?3 AND COALESCE(epoch,0)<=?4")?;
        let mut writes = self.writes()?;
        loop {
            let mut key = None;
            self.run(&mut next, &[txt(session), int(last_abandoned)], |r| {
                key = Some((
                    r.get::<String>(0).map_err(fail)?,
                    r.get_value(1).to_blob().ok_or_else(|| anyhow::anyhow!("invalid outbox key"))?.to_vec(),
                ));
                Ok(())
            })?;
            let Some((family, key)) = key else { return Ok(()) };
            anyhow::ensure!(
                self.one_int("SELECT complete FROM convo_raft_state WHERE session=?1", &[txt(session)])? == Some(1),
                "a legacy group needs a complete snapshot before discarding uncommitted journal writes"
            );
            self.run(&mut remove, &[txt(session), txt(&family), bytes(&key)?, int(last_abandoned)], |_| Ok(()))?;
            if self
                .one_int(
                    "SELECT 1 FROM inbox_outbox WHERE session=?1 AND family=?2 AND key=?3 AND COALESCE(epoch,0)=?4 LIMIT 1",
                    &[txt(session), txt(&family), bytes(&key)?, seq(epoch)],
                )?
                .is_some()
            {
                continue;
            }
            if family.is_empty() {
                if !key.is_empty() {
                    self.restore_committed_source(session, std::str::from_utf8(&key)?, &mut writes)?;
                }
                continue;
            }
            let mut committed = None;
            self.run(&mut cell, &[txt(session), bytes(family.as_bytes())?, bytes(&key)?], |r| {
                let body = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("invalid projected inbox row"))?;
                committed = Some(serde_json::from_slice(body)?);
                Ok(())
            })?;
            let row = committed.unwrap_or_else(|| {
                Row::Inbox(Box::new(InboxRow {
                    session: session.to_owned(),
                    family,
                    key,
                    value: None,
                }))
            });
            let rev = self.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            if self.write(&mut writes, &row, rev)? {
                self.rev = rev;
            }
        }
    }

    /// Reconcile pre-Raft working rows against the complete projection once, before resuming.
    /// Only the session being reclaimed is touched. Later local custody is overlaid separately.
    fn repair_preexisting_history(&mut self, session: &str, replace_header: bool) -> anyhow::Result<()> {
        if self.one_int("SELECT verify_events FROM convo_raft_state WHERE session=?1", &[txt(session)])? != Some(1) {
            return Ok(());
        }
        anyhow::ensure!(self.raft_projection_complete(session)?, "a legacy group needs a complete snapshot before repairing local history");
        self.clear_working_history(session, replace_header)?;
        let mut writes = self.writes()?;
        let mut read = self.prepare("SELECT body FROM convo_raft_cells WHERE session=?1 ORDER BY kind,source,n,ordinal,id")?;
        let mut rev = self.rev;
        self.run(&mut read, &[txt(session)], |r| {
            let body = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("invalid committed row"))?;
            let row = serde_json::from_slice(body)?;
            let next = rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            if self.write(&mut writes, &row, next)? { rev = next; }
            Ok(())
        })?;
        self.rev = rev;
        let mut done = self.prepare("UPDATE convo_raft_state SET verify_events=0 WHERE session=?1")?;
        self.run(&mut done, &[txt(session)], |_| Ok(()))
    }

    fn clear_working_history(&mut self, session: &str, replace_header: bool) -> anyhow::Result<()> {
        let at = crate::util::now_ms();
        let mut writes = self.writes()?;
        if self.one_int("SELECT 1 FROM events WHERE session=?1 LIMIT 1", &[txt(session)])?.is_some() {
            let reset = Row::Reset { session: session.to_owned(), from: None, at };
            let next = self.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            self.write(&mut writes, &reset, next)?;
            self.rev = next;
        }
        // RecordReset also tells existing feed readers to discard stale trailing native lines.
        let mut source = self.prepare("SELECT source FROM records WHERE session=?1 ORDER BY source LIMIT 1")?;
        loop {
            let mut name = None;
            self.run(&mut source, &[txt(session)], |r| { name = Some(r.get::<String>(0).map_err(fail)?); Ok(()) })?;
            let Some(source) = name else { break };
            let next = self.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            self.write(&mut writes, &Row::RecordReset { session: session.to_owned(), source, at }, next)?;
            self.rev = next;
        }
        // Each removed key has its own feed revision. After a large import, a feed page
        // must not need to materialize an entire session as a single synthetic transaction.
        let old_tip = self.rev;
        let mut cursor = (-1_i64, 0_i64);
        let mut keys = self.prepare("SELECT rev,rowid FROM inbox WHERE session=?1 AND (rev,rowid)>(?2,?3) AND rev<=?4 AND value IS NOT NULL ORDER BY rev,rowid LIMIT 1")?;
        let mut clear = self.prepare("UPDATE inbox SET value=NULL,rev=?2 WHERE rowid=?1")?;
        loop {
            let mut old = None;
            self.run(&mut keys, &[txt(session), int(cursor.0), int(cursor.1), seq(old_tip)], |r| {
                old = Some((r.get::<i64>(0).map_err(fail)?,r.get::<i64>(1).map_err(fail)?));
                Ok(())
            })?;
            let Some(found) = old else { break };
            let next = self.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            self.run(&mut clear, &[int(found.1),seq(next)], |_| Ok(()))?;
            self.rev = next;
            cursor = found;
        }
        if replace_header {
            let mut clear = self.prepare("DELETE FROM sessions WHERE id=?1")?;
            self.run(&mut clear, &[txt(session)], |_| Ok(()))?;
        }
        Ok(())
    }

    fn restore_committed_source(&mut self, session: &str, source: &str, writes: &mut Writes) -> anyhow::Result<()> {
        let reset = Row::RecordReset {
            session: session.to_owned(),
            source: source.to_owned(),
            at: crate::util::now_ms(),
        };
        let rev = self.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
        self.write(writes, &reset, rev)?;
        self.rev = rev;
        let mut read = self.prepare("SELECT body FROM convo_raft_cells WHERE session=?1 AND kind='record' AND source=?2 ORDER BY n,id")?;
        let mut next = self.rev;
        self.run(&mut read, &[txt(session), bytes(source.as_bytes())?], |r| {
            let body = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("invalid projected record"))?;
            let row = serde_json::from_slice(body)?;
            let rev = next.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            if self.write(writes, &row, rev)? {
                next = rev;
            }
            Ok(())
        })?;
        self.rev = next;
        Ok(())
    }

    fn local_inbox(&self, session: &str, owner: &str) -> anyhow::Result<LocalInbox> {
        let mut q = self.prepare(
            "SELECT s.owner<>?2, s.inbox_epoch, f.epoch FROM (SELECT ?1 AS id) k
                LEFT JOIN convo_raft_state s ON s.session=k.id LEFT JOIN convo_local_owner_fence f ON f.session=k.id",
        )?;
        let mut gate = LocalInbox::Fenced;
        self.run(&mut q, &[txt(session), txt(owner)], |r| {
            gate = if r.get::<i64>(0).ok() == Some(1) {
                LocalInbox::Fenced
            } else if let Ok(floor) = r.get::<i64>(2) {
                match r.get::<i64>(1).ok().filter(|epoch| *epoch > floor) {
                    Some(epoch) => LocalInbox::NeedsHead(u64::try_from(epoch)?),
                    None => LocalInbox::Fenced,
                }
            } else {
                LocalInbox::Active
            };
            Ok(())
        })?;
        if let LocalInbox::NeedsHead(epoch) = gate
            && let Some((bytes, _)) = self.link().inbox_get("inbox-heads", session.as_bytes())?
            && serde_json::from_slice::<HeadEpoch>(&bytes)?.epoch == epoch
        {
            return Ok(LocalInbox::Active);
        }
        Ok(gate)
    }

    fn check_outbox_epoch(&self, session: &str, epoch: u64) -> anyhow::Result<()> {
        let eligible = self.one_int(
            "SELECT NOT EXISTS(SELECT 1 FROM convo_raft_state WHERE session=?1 AND (inbox_epoch<>?2 OR owner<>?3))
                AND NOT EXISTS(SELECT 1 FROM convo_local_owner_fence WHERE session=?1 AND epoch>=?2)",
            &[txt(session), int(i64::try_from(epoch)?), text(self.local_owner.as_deref())],
        )?;
        anyhow::ensure!(eligible==Some(1), "inbox owner or ownership epoch changed");
        Ok(())
    }

    /// Receipt placement survives elections even when the former writer's store was much
    /// busier. Its successor must issue every later receipt above any position it took over.
    fn observe_receipt(&mut self, family: &str, value: &[u8]) -> anyhow::Result<()> {
        if let Some(position) = crate::harness::inbox::receipt_sequence(family, value) {
            anyhow::ensure!(position < i64::MAX as u64, "receipt position exhausts the store revision clock");
            self.rev = self.rev.max(position);
        }
        Ok(())
    }

    fn writes(&self) -> anyhow::Result<Writes> {
        Ok(Writes::default())
    }

    /// One row, as write number `rev`: whether it wrote anything. An event or a record line that
    /// comes again as it is kept writes nothing, and the number goes unused.
    fn write(&self, w: &mut Writes, row: &Row, rev: u64) -> anyhow::Result<bool> {
        let r = seq(rev);
        let none = |_: &TRow| Ok(());
        // While a copy restores this store, what else writes a session it brought (its own
        // machine's feed, come back) takes it over: the copy's later rows of it are left out.
        if self.copying && !self.filling {
            let gone = Writes::get(&mut w.copied_drop, self, sql::COPIED_DROP)?;
            self.run(gone, &[txt(row.session())], none)?;
        }
        match row {
            Row::Session { session, info, meta, state, changes, epoch, open, at } => {
                let stmt = Writes::get(&mut w.session, self, sql::SESSION_UPSERT)?;
                self.run(
                    stmt,
                    &[txt(session), text(info.as_deref()), text(meta.as_deref()), text(state.as_deref()), text(changes.as_deref()), int(*at), seq(epoch.unwrap_or(0)), r],
                    none,
                )?;
                if *open {
                    let open_st = Writes::get(&mut w.open, self, sql::SESSION_OPEN)?;
                    self.run(open_st, &[txt(session)], none)?;
                    self.touch(session);
                }
            }
            Row::Event(e) => {
                let stmt = Writes::get(&mut w.event, self, sql::EVENT_UPSERT)?;
                self.run(
                    stmt,
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
                )?;
                return Ok(stmt.n_change() > 0);
            }
            Row::Reset { session, from, at } => {
                let reset_st = Writes::get(&mut w.reset, self, sql::EVENTS_RESET)?;
                self.run(reset_st, &[txt(session), seq(from.unwrap_or(0))], none)?;
                let drop_st = Writes::get(&mut w.dropped, self, sql::DROP_ADD)?;
                self.run(drop_st, &[r, txt(session), txt("events"), Value::Null, from.map_or(Value::Null, seq), int(*at)], none)?;
            }
            Row::Closed { session, at } => {
                let close_st = Writes::get(&mut w.close, self, sql::SESSION_CLOSE)?;
                self.run(close_st, &[txt(session), int(*at), r], none)?;
            }
            Row::Record(rec) => {
                let rec_st = Writes::get(&mut w.record, self, sql::RECORD_UPSERT)?;
                self.run(rec_st, &[txt(&rec.session), txt(&rec.source), seq(rec.n), seq(rec.end), record_value(rec)?, r], none)?;
                return Ok(rec_st.n_change() > 0);
            }
            Row::RecordReset { session, source, at } => {
                let rr_st = Writes::get(&mut w.records_reset, self, sql::RECORDS_RESET)?;
                self.run(rr_st, &[txt(session), txt(source)], none)?;
                let drop_st = Writes::get(&mut w.dropped, self, sql::DROP_ADD)?;
                self.run(drop_st, &[r, txt(session), txt("record"), txt(source), Value::Null, int(*at)], none)?;
            }
            // Where it runs isn't a write of its own: it goes with the session's row.
            Row::Machine { session, machine } => {
                let mach_st = Writes::get(&mut w.machine, self, sql::SESSION_MACHINE)?;
                self.run(mach_st, &[txt(session), txt(machine)], none)?;
                self.touch(session);
            }
            // A row of its inbox and journal as its store has it now; a deleted one kept deleted, fed on as one.
            Row::Inbox(i) => {
                let value = match &i.value {
                    Some(v) => bytes(v)?,
                    None => Value::Null,
                };
                let put = Writes::get(&mut w.inbox, self, sql::INBOX_PUT)?;
                self.run(put, &[txt(&i.family), bytes(&i.key)?, txt(&i.session), value, r], none)?;
            }
        }
        Ok(true)
    }

    fn changes_for(&mut self, viewer: Option<&str>, from: u64, max_bytes: usize, kept: bool, inbox: bool) -> anyhow::Result<Changes> {
        if from >= self.rev {
            return Ok(Changes { rows: Vec::new(), upto: from });
        }
        let s = |r: &TRow, i: usize| r.get::<String>(i).ok();
        // Each kind's rows after `from`: (rev, session, the rows it makes, its epoch if it says).
        let pages = [
            self.feed_page(if viewer.is_some() { sql::VIEW_SESSIONS } else { sql::FEED_SESSIONS }, viewer, from, max_bytes, |r| {
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
            self.feed_page(if viewer.is_some() { sql::VIEW_EVENTS } else { sql::FEED_EVENTS }, viewer, from, max_bytes, |r| {
                let e = event_of(r)?;
                Ok((u(r, 12), e.session.clone(), vec![Row::Event(Box::new(e))], None))
            })?,
            if viewer.is_some() { (Vec::new(), false) } else { self.feed_page(sql::FEED_RECORDS, None, from, max_bytes, |r| {
                // To a taker that takes them so, a compressed line goes as it is kept.
                let v = r.get_value(4);
                let (line, as_kept) = match v.to_blob() {
                    Some(b) if kept && b.first() == Some(&CODEC_BROTLI) => (String::new(), Some(b.to_vec())),
                    _ => (unsqueeze(v)?, None),
                };
                let rec = RecordRow { session: s(r, 0).unwrap_or_default(), source: s(r, 1).unwrap_or_default(), n: u(r, 2), end: u(r, 3), line, kept: as_kept };
                Ok((u(r, 5), rec.session.clone(), vec![Row::Record(Box::new(rec))], None))
            })? },
            // The sessions' inboxes, to a taker that takes them: others read on past their numbers.
            if inbox { self.feed_inbox(from, max_bytes)? } else { (Vec::new(), false) },
            self.feed_page(if viewer.is_some() { sql::VIEW_DROPS } else { sql::FEED_DROPS }, viewer, from, max_bytes, |r| {
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
            // Never between rows of one number (an inbox commit's): its rows go together.
            if bytes >= max_bytes && out.rows.last().is_some_and(|c| c.rev != rev) {
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

    /// The inbox's rows after `from`, as [`Turso::feed_page`] reads a kind's: each commit's rows whole, though they share
    /// its number (a page cut short reads the rest of the last commit it read).
    fn feed_inbox(&self, from: u64, max_bytes: usize) -> anyhow::Result<(Vec<(u64, String, Vec<Row>, Option<u64>)>, bool)> {
        let row_of = |r: &TRow| -> anyhow::Result<(u64, String, Vec<Row>, Option<u64>)> {
            let session = r.get::<String>(0).unwrap_or_default();
            let row = InboxRow { session: session.clone(), family: r.get::<String>(1).unwrap_or_default(), key: r.get_value(2).to_blob().map(<[u8]>::to_vec).unwrap_or_default(), value: r.get_value(3).to_blob().map(<[u8]>::to_vec) };
            Ok((u(r, 4), session, vec![Row::Inbox(Box::new(row))], None))
        };
        let (mut out, more) = self.feed_page(sql::FEED_INBOX, None, from, max_bytes, row_of)?;
        if more && let Some(last) = out.last().map(|(rev, ..)| *rev) {
            out.retain(|(rev, ..)| *rev != last);
            let mut rest = self.prepare(sql::FEED_INBOX_AT)?;
            self.run(&mut rest, &[seq(last)], |r| {
                out.push(row_of(r)?);
                Ok(())
            })?;
        }
        Ok((out, more))
    }

    /// The epoch this store has for `session` (0: none yet).
    fn epoch_of(&self, session: &str) -> anyhow::Result<u64> {
        Ok(self.one_int(sql::SESSION_EPOCH, &[txt(session)])?.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    /// Rows of one kind after `from`, each as `(rev, session, rows, epoch)`: a page of them, or
    /// fewer once they come to `max_bytes` (nothing more is read, or decompressed, than a batch
    /// takes); and whether more may follow (the page is full, or stopped at the bytes).
    #[allow(clippy::type_complexity)]
    fn feed_page(&self, sql: &str, viewer: Option<&str>, from: u64, max_bytes: usize, each: impl Fn(&TRow) -> anyhow::Result<(u64, String, Vec<Row>, Option<u64>)>) -> anyhow::Result<(Vec<(u64, String, Vec<Row>, Option<u64>)>, bool)> {
        let mut stmt = self.prepare(sql)?;
        let (mut out, mut bytes, mut full) = (Vec::new(), 0usize, false);
        let common = [seq(from), int(i64::try_from(FEED_PAGE).unwrap_or(i64::MAX)), viewer.map_or(Value::Null, txt)];
        let params = if viewer.is_some() { &common[..] } else { common.get(..2).unwrap_or_default() };
        self.run_while(&mut stmt, params, |r| {
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

    fn patch_copy(&self, table: &str, columns: &str, rows: &[Vec<Value>]) -> anyhow::Result<()> {
        let fields = columns.split(',').count() + 1;
        let mut put = self.prepare(&format!("INSERT OR REPLACE INTO {table}(rowid,{columns}) VALUES({})", vec!["?";fields].join(",")))?;
        let mut del = self.prepare(&format!("DELETE FROM {table} WHERE rowid=?1"))?;
        for row in rows {
            let [id, present, values @ ..] = row.as_slice() else { anyhow::bail!("invalid compact row") };
            if matches!(present, Value::Null) {
                self.run(&mut del, std::slice::from_ref(id), |_| Ok(()))?;
            } else {
                // The presence column is the source rowid too. It is already contiguous with
                // the SQLite values, including large native or event blobs.
                let data = row.get(1..).ok_or_else(||anyhow::anyhow!("invalid compact parameters"))?;
                anyhow::ensure!(values.len() + 1 == fields,"invalid compact row width");
                self.run(&mut put,data, |_| Ok(()))?;
            }
        }
        Ok(())
    }

    fn watch_compact_rows(&mut self) -> anyhow::Result<()> {
        self.transaction(|t| {
            t.exec("CREATE TEMP TABLE convo_copy_dirty (tbl TEXT NOT NULL, r INTEGER NOT NULL, PRIMARY KEY(tbl,r))")?;
            for (table, _) in RAFT_COPY {
                for (op,side) in [("insert","NEW"),("update","NEW"),("delete","OLD")] {
                    t.conn.execute(format!("CREATE TEMP TRIGGER convo_copy_{table}_{op} AFTER {op} ON main.{table} BEGIN
                        INSERT INTO convo_copy_dirty(tbl,r) VALUES('{table}',{side}.rowid) ON CONFLICT(tbl,r) DO NOTHING; END")).map_err(fail)?;
                }
            }
            Ok(())
        })
    }

    fn unwatch_compact_rows(&self) -> anyhow::Result<()> {
        for (table, _) in RAFT_COPY {
            for op in ["insert","update","delete"] {
                self.exec(&format!("DROP TRIGGER IF EXISTS temp.convo_copy_{table}_{op}"))?;
            }
        }
        self.exec("DROP TABLE IF EXISTS temp.convo_copy_dirty")
    }

    /// A dirty entry records the identity alone. The current committed value (including a
    /// deletion) is read once irrespective of how often the key changed during the copy.
    fn compact_dirty(&self, c: &Compaction, budget: usize, flush: bool) -> anyhow::Result<bool> {
        for (table, columns) in RAFT_COPY {
            let sql = format!("SELECT d.r,d.r,t.rowid,t.* FROM convo_copy_dirty d LEFT JOIN {table} t ON t.rowid=d.r
                               WHERE d.tbl=?1 ORDER BY d.r LIMIT ?2");
            let (rows,last) = self.rows_from(&sql,&[txt(table),int(COMPACT_ROWS)],budget)?;
            if !rows.is_empty() {
                c.copier.give(CopyJob::Patch(table,columns,rows,flush))?;
                let mut done = self.prepare("DELETE FROM convo_copy_dirty WHERE tbl=?1 AND r<=?2")?;
                self.run(&mut done, &[txt(table),int(last)], |_| Ok(()))?;
                return Ok(true);
            }
        }
        Ok(false)
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
        self.unwatch_compact_rows()?;
        self.watch_compact_rows()?;
        self.touched.borrow_mut().clear();
        self.compaction = Some(Box::new(Compaction {
            copier,
            scans: COMPACT_SCANNED.iter().copied().chain(RAFT_COPY.iter().map(|(t,_)| *t)).map(|t| (t,Some(0))).collect(),
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
            let tracked = RAFT_COPY.iter().find(|(name,_)| *name == *table);
            let fields = if tracked.is_some() { "rowid,rowid,rowid,*" } else { "rowid,*" };
            let (rows, last) = self.rows_from(&format!("SELECT {fields} FROM {table} WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"), &[int(at.unwrap_or(0)), int(COMPACT_ROWS)], budget)?;
            if rows.is_empty() {
                *at = None;
            } else {
                tracing::debug!(table, rows = rows.len(), read_ms = ms(began.elapsed()), "conversation store: a step of compacting");
                let job = match tracked {
                    Some((name,cols)) => CopyJob::Patch(name,cols,rows,true),
                    None => CopyJob::Put(table,rows,true),
                };
                c.copier.give(job)?;
                *at = Some(last);
            }
            return Ok(false);
        }
        if self.compact_dirty(c, budget, true)? { return Ok(false); }
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
                // An inbox commit's rows share its number: the rest of the last one read, so none of it is left behind.
                if table == "inbox" {
                    all.retain(|(rev, t, _)| !(*t == "inbox" && *rev == last));
                    let mut rest = self.prepare(sql::INBOX_AT)?;
                    self.run(&mut rest, &[seq(last)], |r| {
                        all.push((last, table, r.get_values().skip(1).cloned().collect()));
                        Ok(())
                    })?;
                }
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
        while self.compact_dirty(c, super::store::COMPACT_STEP, false)? {}
        for table in tables.iter().filter(|t| !COMPACT_SCANNED.contains(&t.as_str()) && !RAFT_COPY.iter().any(|(name,_)| *name == t.as_str())) {
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
            if let Some(dir) = self.path.parent()
                && let Ok(d) = std::fs::File::open(dir)
            {
                let _ = d.sync_all();
            }
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
        if let Err(err) = self.unwatch_compact_rows() {
            tracing::warn!(error=%err, "could not drop temporary compact change tracking");
        }
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
    fn has_legacy_name_catalog(&mut self)->anyhow::Result<bool> {self.link().has_legacy_name_catalog()}
    fn historical_names(&mut self)->anyhow::Result<std::collections::BTreeSet<String>> { self.link().historical_names() }
    fn founding_rows(&mut self,session:&str,epoch:u64,owner:&str)->anyhow::Result<Vec<Row>> {self.link().founding_rows(session,epoch,owner)}
    fn id(&self) -> String {
        self.id.clone()
    }

    fn journal_raft_rows(&mut self, session: &str, epoch: Option<u64>, rows: &[Row], record: bool) -> anyhow::Result<()> {
        anyhow::ensure!(rows.iter().all(|r| r.session() == session), "outbox contains a foreign session");
        if record {
            anyhow::ensure!(rows.iter().all(|r| matches!(r, Row::Record(_) | Row::RecordReset { .. })), "expected recorder rows");
        }
        if !self.inbox_replication {
            anyhow::ensure!(record, "Raft outbox is not initialized");
            return self.apply(rows);
        }
        self.transaction(|t| {
            let epoch = match epoch {
                Some(epoch) => epoch,
                None => t.epoch_of(session)?,
            };
            t.check_outbox_epoch(session, epoch)?;
            let tip = t.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
            let mut put = t.prepare("INSERT INTO inbox_outbox(session,rev,ord,family,key,epoch,row_body) VALUES(?1,?2,?3,'',?4,?5,?6)")?;
            let mut writes = t.writes()?;
            for (ord, row) in rows.iter().enumerate() {
                let source = match row {
                    Row::Record(r) => r.source.as_bytes(),
                    Row::RecordReset { source, .. } => source.as_bytes(),
                    _ => &[],
                };
                if record {
                    let rev = t.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
                    if t.write(&mut writes, row, rev)? {
                        t.rev = rev;
                    }
                }
                t.write_bound(
                    &mut put,
                    [txt(session), seq(tip), int(i64::try_from(ord)?), bytes(source)?, seq(epoch), encoded_row(row)?],
                )?;
            }
            t.rev = t.rev.max(tip);
            Ok(())
        })
    }

    fn enable_inbox_for(&mut self, owner: &str) -> anyhow::Result<()> {
        anyhow::ensure!(owner.is_empty() || self.local_owner.as_deref().is_none_or(|previous| previous == owner), "conversation store is already bound to another local node");
        if self.inbox_replication && (owner.is_empty() || self.local_owner.is_some()) { return Ok(()); }
        self.transaction(|t| {
            if !owner.is_empty() { t.set_meta("inbox_raft_owner", owner)?; }
            if t.inbox_replication { return Ok(()); }
            // Seed all existing live rows AND tombstones exactly once, before a new local
            // transaction can be accepted. Preserve the original multi-key commit boundaries.
            let mut seed=t.prepare("INSERT INTO inbox_outbox(session,rev,ord,family,key,value,epoch) SELECT i.session,i.rev,i.rowid,i.family,i.key,i.value,COALESCE(json_extract(CAST(h.value AS TEXT),'$.epoch'),s.epoch,0)
                 FROM inbox i LEFT JOIN inbox h ON h.family='inbox-heads' AND h.key=CAST(i.session AS BLOB)
                 LEFT JOIN sessions s ON s.id=i.session WHERE s.machine IS NULL OR s.machine=?1")?;
            t.run(&mut seed,&[txt(owner)], |_|Ok(()))?;
            t.set_meta("inbox_raft_outbox", "1")
        })?;
        self.inbox_replication = true;
        if !owner.is_empty() { self.local_owner = Some(owner.to_owned()); }
        Ok(())
    }

    fn pending_inbox_sessions(&mut self) -> anyhow::Result<Vec<String>> {
        // Seek directly past the previous session. SELECT DISTINCT walks each pending row,
        // including years of offline backlog when another session only needs its next receipt.
        let mut out: Vec<String> = Vec::new();
        let mut stmt = self.prepare("SELECT session FROM inbox_outbox WHERE session>?1 ORDER BY session LIMIT 1")?;
        loop {
            let mut next = None;
            self.run(&mut stmt, &[txt(out.last().map_or("", String::as_str))], |r| {
                next = Some(r.get::<String>(0).map_err(fail)?);
                Ok(())
            })?;
            let Some(session) = next else { break };
            out.push(session);
        }
        Ok(out)
    }

    fn inbox_outbox_tip(&mut self, session: &str, epoch: u64) -> anyhow::Result<Option<u64>> {
        anyhow::ensure!(self.inbox_replication, "inbox quorum replication is not initialized");
        self.check_outbox_epoch(session, epoch)?;
        self.one_int("SELECT MAX(rev) FROM inbox_outbox WHERE session=?1", &[txt(session)])?
            .map(u64::try_from)
            .transpose()
            .map_err(Into::into)
    }

    fn next_inbox_tx(&mut self, session: &str, epoch: u64, through: u64) -> anyhow::Result<Option<(u64, Vec<Row>)>> {
        self.check_outbox_epoch(session, epoch)?;
        let mut stmt = self.prepare("SELECT rev,family,key,value,epoch,row_body FROM inbox_outbox WHERE session=?1 AND rev=(SELECT MIN(rev) FROM inbox_outbox WHERE session=?1 AND rev<=?2) ORDER BY ord")?;
        let mut result = None;
        self.run(&mut stmt, &[txt(session), seq(through)], |r| {
            if !matches!(r.get_value(4), Value::Null) {
                anyhow::ensure!(u(r, 4) == epoch, "inbox transaction belongs to ownership epoch {}, current is {epoch}", u(r, 4));
            }
            let row = Self::outbox_row(session, r)?;
            result.get_or_insert_with(|| (u(r, 0), Vec::new())).1.push(row);
            Ok(())
        })?;
        Ok(result)
    }

    fn confirm_inbox_tx(&mut self, session: &str, rev: u64) -> anyhow::Result<()> {
        self.transaction(|t| {
            let mut stmt = t.prepare("DELETE FROM inbox_outbox WHERE session=?1 AND rev=?2")?;
            t.run(&mut stmt, &[txt(session), seq(rev)], |_| Ok(()))
        })
    }

    fn confirm_local_owner_fence(&mut self, session: &str, epoch: u64) -> anyhow::Result<bool> {
        self.conn.set_sync_type(turso_core::io::FileSyncType::FullFsync);
        let result=self.transaction(|t|{
            let mut q=t.prepare("INSERT INTO convo_local_owner_fence(session,epoch) VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET epoch=excluded.epoch WHERE excluded.epoch>convo_local_owner_fence.epoch")?;
            t.run(&mut q,&[txt(session),int(i64::try_from(epoch)?)],|_|Ok(()))?;
            Ok(q.n_change()>0)
        });
        self.conn.set_sync_type(turso_core::io::FileSyncType::Fsync);
        result
    }

    fn raft_projection_origin(&mut self,session:&str) -> anyhow::Result<Option<[u8;32]>> {
        let mut q = self.prepare("SELECT genesis FROM convo_raft_state WHERE session=?1 AND complete=1 AND genesis IS NOT NULL")?;
        let mut origin = None;
        self.run(&mut q, &[txt(session)], |r| {
            let data = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("invalid stored founding identity"))?;
            origin = Some(data.try_into().map_err(|_| anyhow::anyhow!("invalid stored founding identity length"))?);
            Ok(())
        })?;
        Ok(origin)
    }

    fn has_raft_projection(&mut self, session: &str) -> anyhow::Result<bool> {
        Ok(self.one_int("SELECT 1 FROM convo_raft_state WHERE session=?1", &[txt(session)])?.is_some())
    }

    fn raft_projection_frontier(&mut self, session: &str) -> anyhow::Result<Option<(u64, u64)>> {
        let mut q = self.prepare("SELECT applied,term FROM convo_raft_state WHERE session=?1 AND complete=1")?;
        let mut frontier = None;
        self.run(&mut q, &[txt(session)], |r| {
            frontier = Some((u64::try_from(r.get::<i64>(0).map_err(fail)?)?, u64::try_from(r.get::<i64>(1).map_err(fail)?)?));
            Ok(())
        })?;
        Ok(frontier)
    }

    fn begin_raft_projection(&mut self, session: &str, previous_index: u64) -> anyhow::Result<()> {
        self.transaction(|t| {
            let mut stmt = t.prepare("INSERT INTO convo_raft_state(session,applied,term,complete,verify_events) VALUES(?1,0,0,?2,EXISTS(SELECT 1 FROM events WHERE session=?1) OR EXISTS(SELECT 1 FROM records WHERE session=?1) OR EXISTS(SELECT 1 FROM inbox WHERE session=?1) OR EXISTS(SELECT 1 FROM sessions WHERE id=?1)) ON CONFLICT(session) DO NOTHING")?;
            t.run(&mut stmt, &[txt(session), int(i64::from(previous_index == 0))], |_| Ok(()))
        })
    }

    fn raft_snapshot_rows(&mut self, session: &str, at: u64) -> anyhow::Result<Vec<Row>> {
        self.link().raft_snapshot_rows(session, at, &mut || {})
    }

    fn write_raft_rows(&mut self, session: &str, index: u64, term: u64, rows: &[Row], mode: ProjectionWrite, new_epoch: Option<u64>) -> anyhow::Result<()> {
        let install = mode != ProjectionWrite::Apply;
        let origin = match mode { ProjectionWrite::Certified { origin, .. } => Some(origin), _ => None };
        let recover = matches!(mode, ProjectionWrite::Recover | ProjectionWrite::Certified { recover:true, .. });
        anyhow::ensure!(rows.iter().all(|r| r.session() == session), "Raft entry contains a foreign session");
        self.conn.set_sync_type(turso_core::io::FileSyncType::FullFsync);
        let result = self.transaction(|t| {
            let state=t.one_int("SELECT applied FROM convo_raft_state WHERE session=?1", &[txt(session)])?;
            let matched_origin = match origin {
                Some(genesis) if state.is_some() => t.one_int("SELECT genesis=?2 FROM convo_raft_state WHERE session=?1 AND complete=1 AND genesis IS NOT NULL", &[txt(session),bytes(&genesis)?])?,
                _ => None,
            };
            let different_origin = origin.is_some() && matched_origin != Some(1);
            if !recover && !different_origin && let Some(prior) = state && (!install || t.raft_projection_complete(session)?) {
                if prior > i64::try_from(index)? { return Ok(()) }
                if prior == i64::try_from(index)? {
                    anyhow::ensure!(t.one_int("SELECT term FROM convo_raft_state WHERE session=?1",&[txt(session)])? == Some(i64::try_from(term)?), "Raft projection term mismatch at {index}");
                    return Ok(())
                }
            }
            anyhow::ensure!(install || state.is_some(), "Raft projection is missing");
            let owner = rows.iter().rev().find_map(|row| match row { Row::Machine { machine,.. } => Some(machine.as_str()), _ => None });
            let unmarked_history = recover && origin.is_some() && matched_origin.is_none() && state.is_some_and(|i| i > 0);
            let prior_owner_confirmed = if unmarked_history {
                match (t.local_owner.as_deref(), owner) {
                    (Some(local),Some(committed)) if local == committed =>
                        t.one_int("SELECT 1 FROM convo_raft_state WHERE session=?1 AND owner=?2", &[txt(session),txt(local)])?.is_some(),
                    _ => false,
                }
            } else { true };
            let replaced_custody = matched_origin == Some(0) || !prior_owner_confirmed;
            if replaced_custody {
                // A different session may have reached a greater epoch under the same name.
                // Retaining its future rows would make them reappear when this one caught up.
                let mut clear = t.prepare("DELETE FROM inbox_outbox WHERE session=?1")?;
                t.run(&mut clear, &[txt(session)], |_| Ok(()))?;
            }
            let foreign = replaced_custody || match (t.local_owner.as_deref(), owner) {
                (Some(local), Some(committed)) => local != committed,
                (Some(local), None) => t.one_int("SELECT owner<>?2 FROM convo_raft_state WHERE session=?1", &[txt(session), txt(local)])? == Some(1),
                _ => false,
            };
            if install {
                t.clear_working_history(session, true)?;
                let mut clear=t.prepare("DELETE FROM convo_raft_cells WHERE session=?1")?;
                t.run(&mut clear,&[txt(session)], |_| Ok(()))?;
            } else if let Some(epoch) = new_epoch {
                t.revert_abandoned_inbox(session, epoch, foreign)?;
                t.repair_preexisting_history(session, rows.iter().any(|row| matches!(row, Row::Session { .. })))?;
            }
            let mut writes=t.writes()?;
            let mut put=t.prepare("INSERT INTO convo_raft_cells(session,kind,source,id,n,body,ordinal) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(session,kind,source,id) DO UPDATE SET n=excluded.n, body=excluded.body,ordinal=excluded.ordinal WHERE convo_raft_cells.body<>excluded.body")?;
            let overlay=!foreign && !install && t.one_int("SELECT 1 FROM inbox_outbox WHERE session=?1 LIMIT 1",&[txt(session)])?.is_some();
            for row in rows {
                if let Row::Inbox(i)=row && let Some(value)=&i.value { t.observe_receipt(&i.family,value)?; }
                t.project_row(session,row,&mut put)?;
                let changed_event=matches!(row,Row::Event(_)) && put.n_change()>0;
                // Working CAS revisions and recorder offsets can already be ahead of the
                // acknowledged row. Only the authoritative projection advances in that case.
                if overlay && t.has_working_overlay(session,new_epoch,row)? { continue; }
                let rev=t.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
                if t.write(&mut writes,row,rev)? || changed_event { t.rev=rev; }
            }
            if install || new_epoch.is_some() { t.restore_outbox_overlay(session,new_epoch.unwrap_or(0),foreign,&mut writes)?; }
            let mut mark=t.prepare("INSERT INTO convo_raft_state(session,applied,term,complete,inbox_epoch,verify_events,owner,genesis) VALUES(?1,?2,?3,?4,?5,0,?6,?7) ON CONFLICT(session) DO UPDATE SET genesis=CASE WHEN excluded.complete=1 THEN excluded.genesis ELSE convo_raft_state.genesis END,owner=COALESCE(excluded.owner,convo_raft_state.owner),verify_events=CASE WHEN excluded.complete=1 THEN 0 ELSE convo_raft_state.verify_events END,applied=excluded.applied,term=excluded.term,complete=MAX(convo_raft_state.complete,excluded.complete),inbox_epoch=COALESCE(excluded.inbox_epoch,convo_raft_state.inbox_epoch)")?;
            t.run(&mut mark,&[txt(session),int(i64::try_from(index)?),int(i64::try_from(term)?),int(i64::from(install)),new_epoch.or_else(|| rows.iter().find_map(|r| match r { Row::Session { epoch, .. } => *epoch, _ => None })).map_or(Value::Null,seq), text(owner),origin.as_ref().map(|g|bytes(g)).transpose()?.unwrap_or(Value::Null)], |_| Ok(()))
        });
        self.conn.set_sync_type(turso_core::io::FileSyncType::Fsync);
        result
    }

    fn inbox_get(&mut self, family: &str, key: &[u8]) -> anyhow::Result<Option<(Vec<u8>, u64)>> {
        self.link().inbox_get(family, key)
    }

    fn inbox_scan(&mut self, family: &str, prefix: &[u8], after: Option<&[u8]>, limit: usize) -> anyhow::Result<InboxPage> {
        self.link().inbox_scan(family, prefix, after, limit)
    }

    fn inbox_commit_batch(&mut self, batch: &[(&kiln_seams::records::Tx, &[String])]) -> anyhow::Result<Vec<Result<u64, kiln_seams::records::Refusal>>> {
        use kiln_seams::records::{Cond, Refusal, Write};
        // What's answered is on disk past the drive's cache on Apple's systems (F_FULLFSYNC), as the inbox's own store
        // had it (`crate::records::turso`); only for these commits: the conversation's own writes keep their sync.
        self.conn.set_sync_type(turso_core::io::FileSyncType::FullFsync);
        let done = self.transaction(|t| {
            let mut out = Vec::with_capacity(batch.len());
            let mut put = t.prepare(sql::INBOX_PUT)?;
            let mut delete = t.prepare(sql::INBOX_DELETE)?;
            let mut post = t.inbox_replication.then(|| t.prepare("INSERT INTO inbox_outbox(session,rev,ord,family,key,value,epoch) VALUES(?1,?2,?3,?4,?5,?6,?7)")).transpose()?;
            #[derive(Default)]
            struct Custody { writable: Option<LocalInbox>, epoch: Option<u64> }
            let mut custody: HashMap<&str, Custody> = HashMap::new();
            'commit: for (tx, sessions) in batch {
                // Each read as the commits before it in this batch left it.
                for c in &tx.conds {
                    let (family, key, want) = match c {
                        Cond::Absent { family, key } => (family, key, None),
                        Cond::At { family, key, rev } => (family, key, Some(*rev)),
                    };
                    let at = t.link().inbox_get(family, key)?.map(|(_, rev)| rev);
                    if at != want {
                        out.push(Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at }));
                        continue 'commit;
                    }
                }
                if tx.writes.is_empty() {
                    out.push(Ok(t.rev));
                    continue;
                }
                anyhow::ensure!(sessions.len() == tx.writes.len(), "inbox transaction missing a session");
                if let Some(owner) = &t.local_owner {
                    for session in sessions.iter() {
                        let cached = custody.entry(session).or_default();
                        let gate = match cached.writable {
                            Some(gate) => gate,
                            None => *cached.writable.insert(t.local_inbox(session,owner)?),
                        };
                        let allowed = match gate {
                            LocalInbox::Active => true,
                            LocalInbox::Fenced => false,
                            // A delayed old-head CAS can still match after Raft commits the new
                            // owner. Only its matching new head opens the next inbox epoch.
                            LocalInbox::NeedsHead(epoch) => tx.writes.iter().any(|w| matches!(w,
                                Write::Put {family,key,value} if family=="inbox-heads" && key.as_slice()==session.as_bytes()
                                    && serde_json::from_slice::<HeadEpoch>(value).is_ok_and(|h|h.epoch==epoch))),
                        };
                        if !allowed {
                            out.push(Err(Refusal::Unavailable("the local inbox is fenced or awaiting its committed ownership epoch".into())));
                            continue 'commit;
                        }
                    }
                }
                for w in &tx.writes {
                    if let Write::Put { family,value,.. } = w { t.observe_receipt(family,value)?; }
                }
                t.rev = t.rev.checked_add(1).ok_or_else(|| anyhow::anyhow!("conversation revision exhausted"))?;
                let rev = t.rev;
                for (w, session) in tx.writes.iter().zip(sessions.iter()) {
                    if post.is_some() && matches!(w, Write::Put { family, .. } | Write::Delete { family, .. } if family == "inbox-heads")
                        && let Some(cached) = custody.get_mut(session.as_str()) { *cached = Custody::default(); }
                    match w {
                        Write::Put { family, key, value } => t.write_bound(&mut put, [txt(family), bytes(key)?, txt(session), bytes(value)?, seq(rev)])?,
                        Write::Delete { family, key } => t.run(&mut delete, &[txt(family), bytes(key)?, seq(rev)], |_| Ok(()))?,
                    }
                }
                if let Some(post) = post.as_mut() {
                    for (ord, (write, session)) in tx.writes.iter().zip(sessions.iter()).enumerate() {
                        let cached = custody.entry(session).or_default();
                        let epoch = match cached.epoch {
                            Some(epoch) => epoch,
                            None => {
                                let epoch = match t.link().inbox_get("inbox-heads",session.as_bytes())? {
                                    Some((head,_)) => serde_json::from_slice::<HeadEpoch>(&head)?.epoch,
                                    None => t.epoch_of(session)?,
                                };
                                cached.epoch = Some(epoch);
                                epoch
                            }
                        };
                        let (family,key,value) = match write {
                            Write::Put{family,key,value} => (family,key,bytes(value)?),
                            Write::Delete{family,key} => (family,key,Value::Null),
                        };
                        t.write_bound(post,[txt(session),seq(rev),int(i64::try_from(ord)?),txt(family),bytes(key)?,value,seq(epoch)])?;
                    }
                }
                out.push(Ok(rev));
            }
            Ok(out)
        });
        self.conn.set_sync_type(turso_core::io::FileSyncType::Fsync);
        done
    }

    fn inbox_floor(&mut self, rev: u64) -> anyhow::Result<()> {
        if rev <= self.rev {
            return Ok(());
        }
        self.transaction(|t| {
            t.rev = rev;
            Ok(())
        })
    }

    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.transaction(|t| {
            let mut w = t.writes()?;
            let mut last = None;
            for row in rows {
                // A provider update is journaled separately by the owning session. Only its
                // committed Raft entry may mutate the timeline used for restart and recovery.
                if last.is_none_or(|(id,_)| id != row.session()) {
                    let managed = t.has_raft_schema && t.one_int("SELECT 1 FROM convo_raft_state WHERE session=?1", &[txt(row.session())])?.is_some();
                    last = Some((row.session(),managed));
                }
                if last.is_some_and(|(_,managed)| managed) { continue; }
                t.rev += 1;
                if !t.write(&mut w, row, t.rev)? {
                    t.rev -= 1;
                }
            }
            Ok(())
        })
    }

    fn changes(&mut self, from: u64, max_bytes: usize, kept: bool, inbox: bool) -> anyhow::Result<Changes> {
        self.changes_for(None, from, max_bytes, kept, inbox)
    }

    fn viewer_generation(&mut self, session: &str) -> anyhow::Result<(u64,u64)> {
        let reset = self.one_int("SELECT MAX(rev) FROM drops INDEXED BY drops_by_session_rev WHERE session = ?1 AND what != 'record'", &[txt(session)])?;
        Ok((self.epoch_of(session)?, reset.and_then(|r| u64::try_from(r).ok()).unwrap_or_default()))
    }

    fn viewer_changes(&mut self, session: &str, from: u64, max_bytes: usize) -> anyhow::Result<Changes> {
        self.changes_for(Some(session), from, max_bytes, false, false)
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
        self.link().marks(session)
    }

    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        self.link().record(session, source, from, limit)
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

    fn compact_ready(&self) -> bool {
        self.compaction.as_ref().is_some_and(|c| matches!(c.counted, Counted::Yes) && c.copier.busy() == 0)
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

    fn release(&mut self) -> anyhow::Result<()> {
        self.compact_drop();
        self.flush()
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

    fn restorable(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        self.link().restorable(session, last)
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
        let mut events = self.link().newest(session, None, last)?;
        if kept.is_none() && events.is_empty() {
            return Ok(None);
        }
        events.reverse();
        let mut k = kept.unwrap_or_default();
        k.events = events;
        Ok(Some(k))
    }

    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        self.link().events(session, before, limit)
    }

    fn events_snapshot_page(&mut self, session: &str, max_seq: u64, after: Option<(u64, i64)>, limit: usize) -> anyhow::Result<Vec<(EventRow, i64)>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let bound = int(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut out = Vec::new();
        match after {
            None => {
                let mut stmt = self.prepare(sql::EVENTS_SNAPSHOT_FIRST)?;
                self.run(&mut stmt, &[txt(session), seq(max_seq), bound], |r| {
                    let ev = event_of(r)?;
                    let rowid = r.get::<i64>(sql::EVENT_COLUMNS).unwrap_or(0);
                    out.push((ev, rowid));
                    Ok(())
                })?;
            }
            Some((after_seq, after_rowid)) => {
                let mut stmt = self.prepare(sql::EVENTS_SNAPSHOT_AFTER)?;
                self.run(&mut stmt, &[txt(session), seq(max_seq), seq(after_seq), int(after_rowid), bound], |r| {
                    let ev = event_of(r)?;
                    let rowid = r.get::<i64>(sql::EVENT_COLUMNS).unwrap_or(0);
                    out.push((ev, rowid));
                    Ok(())
                })?;
            }
        }
        Ok(out)
    }

    fn replace_session_snapshot(&mut self, session: &str, rows: &[Row]) -> anyhow::Result<()> {
        let session = session.to_owned();
        self.transaction(|t| {
            anyhow::ensure!(rows.iter().all(|r| r.session() == session), "foreign row in conversation snapshot");
            anyhow::ensure!(!t.has_raft_schema || t.one_int("SELECT 1 FROM convo_raft_state WHERE session=?1", &[txt(&session)])?.is_none(), "managed conversation requires an indexed Raft snapshot");
            let mut clear_ev = t.prepare(sql::SNAPSHOT_CLEAR_EVENTS)?;
            t.run(&mut clear_ev, &[txt(&session)], |_| Ok(()))?;
            let mut clear_rec = t.prepare(sql::SNAPSHOT_CLEAR_RECORDS)?;
            t.run(&mut clear_rec, &[txt(&session)], |_| Ok(()))?;
            let mut clear_inb = t.prepare(sql::SNAPSHOT_CLEAR_INBOX)?;
            t.run(&mut clear_inb, &[txt(&session)], |_| Ok(()))?;
            let mut w = t.writes()?;
            for row in rows {
                t.rev += 1;
                let r = t.rev;
                if !t.write(&mut w, row, r)? {
                    t.rev -= 1;
                }
            }
            Ok(())
        })
    }

    fn inbox_session_all(&mut self, session: &str) -> anyhow::Result<Vec<(String, Vec<u8>, Option<Vec<u8>>)>> {
        let mut stmt = self.prepare(sql::INBOX_SESSION_ALL)?;
        let mut out = Vec::new();
        self.run(&mut stmt, &[txt(session)], |r| {
            let family = r.get::<String>(0).map_err(fail)?;
            let key = r.get_value(1).to_blob().map(<[u8]>::to_vec).ok_or_else(|| anyhow::anyhow!("inbox key is not a blob"))?;
            let value = r.get_value(2).to_blob().map(<[u8]>::to_vec);
            out.push((family, key, value));
            Ok(())
        })?;
        Ok(out)
    }

    fn thread(&mut self, session: &str, parent: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        self.link().thread(session, parent, before, limit)
    }

    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)> {
        self.link().search_some(q, from, rows, want)
    }

    fn reader(&self) -> Option<Box<dyn Read + Send>> {
        // Windows renames no file over one that's open: a compaction's swap would fail while a reader held it.
        if cfg!(windows) {
            return None;
        }
        match self._db.connect(ConnectOpts::new()) {
            Ok(conn) => Some(Box::new(TursoRead { io: Arc::clone(&self.io), conn })),
            Err(e) => {
                tracing::warn!(error = %e, "conversation store: no reader beside its writer, so its reads wait for it");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event, State};

    #[test]
    fn cp_t3_2_durable_former_owner_fence_leaves_the_successors_queue_alone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversation.db");
        let mut t = Turso::open(&path)?;
        t.enable_inbox_for("west")?;
        assert!(!t.has_raft_projection("legacy")?);
        t.begin_raft_projection("incomplete", 7)?;
        assert!(t.has_raft_projection("incomplete")?);
        assert!(t.raft_projection_frontier("incomplete")?.is_none());
        assert!(t.confirm_local_owner_fence("session", 4)?);
        assert!(!t.confirm_local_owner_fence("session", 2)?);
        drop(t);
        let mut t = Turso::open(&path)?;
        assert_eq!(
            t.one_int("SELECT epoch FROM convo_local_owner_fence WHERE session=?1", &[txt("session")])?,
            Some(4)
        );
        assert!(t.pending_inbox_sessions()?.is_empty());
        assert!(t.inbox_get("inbox-heads", b"session")?.is_none());
        assert!(t.inbox_outbox_tip("session", 4).is_err());
        let sessions = ["session".to_string()];
        let send = kiln_seams::records::Tx {
            conds: vec![],
            writes: vec![kiln_seams::records::Write::Put {
                family: "inbox-pending".into(),
                key: b"session/message".to_vec(),
                value: b"body".to_vec(),
            }],
        };
        assert!(matches!(
            t.inbox_commit_batch(&[(&send, &sessions)])?.as_slice(),
            [Err(kiln_seams::records::Refusal::Unavailable(_))]
        ));
        let claim = Row::Machine {
            session: "session".into(),
            machine: "west".into(),
        };
        t.write_raft_rows("session", 2, 3, &[claim], ProjectionWrite::Install, Some(5))?;
        assert!(
            matches!(
                t.inbox_commit_batch(&[(&send, &sessions)])?.as_slice(),
                [Err(kiln_seams::records::Refusal::Unavailable(_))]
            ),
            "a delayed send cannot cross the takeover boundary"
        );
        let mut open = send.clone();
        open.writes = vec![kiln_seams::records::Write::Put {
            family: "inbox-heads".into(),
            key: b"session".to_vec(),
            value: br#"{"epoch":5}"#.to_vec(),
        }];
        assert!(matches!(
            t.inbox_commit_batch(&[(&open, &sessions), (&send, &sessions)])?.as_slice(),
            [Ok(_), Ok(_)]
        ));
        assert!(t.next_inbox_tx("session", 5, u64::MAX)?.is_some());
        Ok(())
    }

    #[test]
    fn cp_t3_2_ordered_outbox_survives_restart_and_preserves_tombstones() -> anyhow::Result<()> {
        use kiln_seams::records::{Tx, Write};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        let mut t = Turso::open(&path)?;
        let sessions = vec!["s".to_owned()];
        let put = |body: &[u8]| Tx { conds: vec![], writes: vec![Write::Put { family: "test".into(), key: b"s/k".to_vec(), value: body.to_vec() }] };
        let delete = Tx { conds: vec![], writes: vec![Write::Delete { family: "test".into(), key: b"s/k".to_vec() }] };
        t.inbox_commit_batch(&[(&put(b"old"), &sessions)])?;
        t.enable_inbox_replication()?;
        t.inbox_commit_batch(&[(&put(b"new"), &sessions), (&delete, &sessions)])?;
        t.journal_raft_rows("s", Some(0), &super::super::rows::of("s", &said("m", 1, "event"), 0), false)?;
        drop(t);
        let mut t = Turso::open(&path)?;
        t.enable_inbox_replication()?;
        let tip = t.inbox_outbox_tip("s", 0)?.ok_or_else(|| anyhow::anyhow!("missing outbox"))?;
        let mut seen = Vec::new();
        while let Some((rev, rows)) = t.next_inbox_tx("s", 0, tip)? {
            assert_eq!(t.next_inbox_tx("s", 0, tip)?, Some((rev, rows.clone())), "same bytes until confirmed");
            seen.extend(rows);
            t.confirm_inbox_tx("s", rev)?;
        }
        let values: Vec<_> = seen
            .iter()
            .filter_map(|row| match row {
                Row::Inbox(i) => Some(i.value.as_deref()),
                _ => None,
            })
            .collect();
        assert_eq!(values, [Some(b"old".as_slice()), Some(b"new".as_slice()), None]);
        assert!(matches!(seen.last(), Some(Row::Event(_))));
        assert!(t.pending_inbox_sessions()?.is_empty());
        Ok(())
    }

    #[test]
    fn cp_t3_2_projection_is_committed_and_claim_reverts_abandoned_work() -> anyhow::Result<()> {
        use kiln_seams::records::{Tx, Write};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        let mut t = Turso::open(&path)?;
        t.enable_inbox_replication()?;
        t.apply(&super::super::rows::of("s1", &said("orphan", 88, "old speculation"), 0))?;
        t.begin_raft_projection("s1", 0)?;
        let inbox = |value: &[u8]| {
            Row::Inbox(Box::new(InboxRow { session: "s1".into(), family: "test".into(), key: b"s1/item".to_vec(), value: Some(value.to_vec()) }))
        };
        let timeline = super::super::rows::of("s1", &said("same", 1, "committed"), 0);
        let mut committed = timeline.clone();
        committed.extend([record(0, "committed"), inbox(b"committed")]);
        t.write_raft_rows("s1", 1, 1, &committed, ProjectionWrite::Apply, None)?;
        t.apply(&super::super::rows::of("s1", &said("same", 1, "uncommitted"), 0))?;
        t.journal_raft_rows("s1", Some(0), &[record(0, "uncommitted")], true)?;
        let tx = Tx { conds: vec![], writes: vec![Write::Put { family: "test".into(), key: b"s1/item".to_vec(), value: b"uncommitted".to_vec() }] };
        t.inbox_commit_batch(&[(&tx, &["s1".to_owned()])])?;
        let snapshot = t.raft_snapshot_rows("s1", 2)?;
        assert_eq!(snapshot.len(), committed.len());
        assert!(committed.iter().all(|row| snapshot.contains(row)));
        t.write_raft_rows("s1", 3, 2, &[], ProjectionWrite::Apply, Some(1))?;
        assert!(t.raft_snapshot_rows("s1", 1).is_err());
        assert_eq!(t.one_int("SELECT count(*) FROM events WHERE session='s1' AND id='orphan'", &[])?, Some(0));
        assert!(t.pending_inbox_sessions()?.is_empty());
        assert_eq!(t.inbox_get("test", b"s1/item")?.map(|v| v.0), Some(b"committed".to_vec()));
        assert_eq!(t.record("s1", "cc", 0, 10)?, [(0, "committed".to_owned())]);
        assert!(t.inbox_outbox_tip("s1", 0).is_err(), "clearing old queue must not settle a stale receipt");
        drop(t);
        let mut t = Turso::open(&path)?;
        assert_eq!(t.raft_snapshot_rows("s1", 3)?, snapshot);
        t.begin_raft_projection("legacy", 17)?;
        assert!(t.raft_snapshot_rows("legacy", 18).is_err());
        t.write_raft_rows("legacy", 20, 3, &[], ProjectionWrite::Install, None)?;
        assert_eq!(t.raft_snapshot_rows("legacy", 20)?, Vec::new());
        Ok(())
    }

    #[test]
    fn cp_t3_2_recreated_catalog_cannot_vouch_for_a_legacy_session_inventory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let old = dir.path().join("old.db");
        let mut t = Turso::open_at(&old,11)?;
        t.apply(&[record(0,"historic native line")])?;
        drop(t);
        let mut original = Turso::open(&old)?;
        assert!(Engine::has_legacy_name_catalog(&mut original)?);
        assert!(Engine::historical_names(&mut original)?.contains("s1"));
        let mut fresh = Turso::open(&dir.path().join("new.db"))?;
        assert!(!Engine::has_legacy_name_catalog(&mut fresh)?);
        Ok(())
    }

    #[test]
    fn cp_t3_2_equal_raft_coordinates_from_a_different_founder_replace_contents_and_custody() -> anyhow::Result<()> {
        let dir=tempfile::tempdir()?;
        let path=dir.path().join("conversation.db");
        let mut t=Turso::open(&path)?;
        t.enable_inbox_for("local")?;
        let first=ProjectionWrite::Certified {origin:[1;32],recover:false};
        let second=ProjectionWrite::Certified {origin:[2;32],recover:false};
        t.write_raft_rows("s1",1,1,&[record(0,"old origin")], first, Some(0))?;
        t.write_raft_rows("s1",2,1,&[record(1,"old source")],ProjectionWrite::Apply,None)?;
        t.journal_raft_rows("s1",Some(0),&[record(2,"uncertain message")],true)?;
        assert_eq!(t.raft_projection_origin("s1")?, Some([1;32]));
        // A DB restored from the other group may be several ownership epochs ahead.
        t.transaction(|db| {
            let mut q=db.prepare("UPDATE inbox_outbox SET epoch=7 WHERE session='s1'")?;
            db.run(&mut q,&[],|_|Ok(()))
        })?;
        let mut t={drop(t);Turso::open(&path)?};
        t.write_raft_rows("s1",2,1,&[record(0,"other origin")],second,Some(0))?;
        assert_eq!(t.raft_projection_origin("s1")?,Some([2;32]));
        assert_eq!(t.record("s1","cc",0,6)?,[(0,"other origin".to_string())]);
        assert!(t.pending_inbox_sessions()?.is_empty());
        t.write_raft_rows("s1",2,1,&[record(0,"duplicate")],second,Some(0))?;
        assert_eq!(t.record("s1","cc",0,6)?,[(0,"other origin".to_string())]);
        Ok(())
    }

    #[test]
    fn cp_t3_2_foreign_founding_discards_mirrored_custody_and_fences_local_writes_after_restart() -> anyhow::Result<()> {
        use kiln_seams::records::{Refusal, Tx, Write};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversation.db");
        let mut t = Turso::open(&path)?;
        let tx = Tx { conds: vec![], writes: vec![Write::Put { family: "item".into(), key: b"s1/old".to_vec(), value: b"orphan".to_vec() }] };
        let sessions = ["s1".to_owned()];
        t.inbox_commit_batch(&[(&tx, &sessions)])?;
        t.apply(&[record(1, "stale tail")])?;
        t.enable_inbox_for("west")?;
        assert_eq!(t.pending_inbox_sessions()?, ["s1"]);
        let header = Row::Session { session:"s1".into(), info:Some("{}".into()), meta:None,state:None,changes:None,epoch:Some(0),open:true,at:1 };
        let committed = [header, Row::Machine {session:"s1".into(), machine:"east".into()}, record(0, "authoritative")];
        t.write_raft_rows("s1", 2, 1, &committed, ProjectionWrite::Install, Some(0))?;
        assert_eq!(t.record("s1","cc",0,4)?, [(0,"authoritative".to_owned())]);
        assert!(t.inbox_get("item", b"s1/old")?.is_none());
        assert!(t.pending_inbox_sessions()?.is_empty());
        let mut t = { drop(t); Turso::open(&path)? };
        assert!(t.journal_raft_rows("s1",Some(0), &[record(1,"wrong owner")],true).is_err());
        assert!(matches!(t.inbox_commit_batch(&[(&tx,&sessions)])?.as_slice(), [Err(Refusal::Unavailable(_))]));
        assert!(t.inbox_outbox_tip("s1",0).is_err());
        Ok(())
    }

    #[test]
    fn cp_t3_2_takeover_repairs_legacy_native_rows_and_custody_without_a_local_outbox() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversation.db"))?;
        let stale = Row::Inbox(Box::new(InboxRow {session:"s1".into(), family:"item".into(), key:b"s1/orphan".to_vec(),value:Some(b"pending".to_vec())}));
        t.apply(&[record(5,"speculative tail"), stale])?;
        t.begin_raft_projection("s1",0)?;
        t.write_raft_rows("s1",1,1,&[record(0,"valid")], ProjectionWrite::Apply,None)?;
        t.write_raft_rows("s1",2,2,&[Row::Machine { session:"s1".into(), machine:"west".into() }],ProjectionWrite::Apply,Some(1))?;
        assert_eq!(t.record("s1","cc",0,8)?,[(0,"valid".to_owned())]);
        assert!(t.inbox_get("item", b"s1/orphan")?.is_none());
        let changes=t.changes(0,usize::MAX,true,true)?;
        assert!(changes.rows.iter().any(|r|matches!(&r.row,Row::RecordReset{source,..} if source=="cc")));
        assert!(changes.rows.iter().any(|r|matches!(&r.row,Row::Inbox(i) if i.key==b"s1/orphan" && i.value.is_none())));
        Ok(())
    }

    #[test]
    fn cp_t3_2_snapshot_preserves_same_epoch_custody_and_fences_older_work() -> anyhow::Result<()> {
        use kiln_seams::records::{Tx, Write};
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversations.db"))?;
        t.enable_inbox_replication()?;
        t.begin_raft_projection("s1", 0)?;
        let tx = Tx { conds: vec![], writes: vec![Write::Put { family: "item".into(), key: b"s1/item".to_vec(), value: b"local".to_vec() }] };
        t.inbox_commit_batch(&[(&tx,&["s1".to_string()])])?;
        t.journal_raft_rows("s1", Some(0), &[record(0, "pending line")], true)?;
        let base = vec![record(0, "committed line"), Row::Inbox(Box::new(InboxRow { session: "s1".into(),family: "item".into(),key: b"s1/item".to_vec(),value: Some(b"committed".to_vec()) }))];
        t.write_raft_rows("s1", 5, 1, &base, ProjectionWrite::Install, Some(0))?;
        assert_eq!(t.inbox_get("item",b"s1/item")?.map(|v|v.0),Some(b"local".to_vec()));
        assert_eq!(t.record("s1","cc",0,10)?,[(0,"pending line".to_string())]);
        assert!(t.inbox_outbox_tip("s1",0)?.is_some());
        assert!(base.iter().all(|r| t.raft_snapshot_rows("s1",5).is_ok_and(|rows| rows.contains(r))));
        t.write_raft_rows("s1", 6, 2, &base, ProjectionWrite::Install, Some(1))?;
        assert!(t.pending_inbox_sessions()?.is_empty());
        assert_eq!(t.inbox_get("item",b"s1/item")?.map(|v|v.0),Some(b"committed".to_vec()));
        assert_eq!(t.record("s1","cc",0,10)?,[(0,"committed line".to_string())]);
        t.write_raft_rows("s1", 5, 1, &[], ProjectionWrite::Install, Some(0))?;
        assert!(t.raft_snapshot_rows("s1",5).is_err());
        Ok(())
    }

    #[test]
    fn cp_t3_2_recovering_an_older_image_defers_future_native_work_until_its_epoch() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversations.db"))?;
        t.enable_inbox_replication()?;
        let baseline = [record(0,"committed")];
        t.write_raft_rows("s1", 6, 2, &baseline, ProjectionWrite::Install, Some(2))?;
        t.journal_raft_rows("s1", Some(2), &[record(0,"unconfirmed")],true)?;
        t.write_raft_rows("s1", 3, 1, &baseline, ProjectionWrite::Recover, Some(1))?;
        assert_eq!(t.record("s1","cc",0,10)?,[(0,"committed".to_owned())]);
        t.write_raft_rows("s1", 6, 2, &[], ProjectionWrite::Apply, Some(2))?;
        assert_eq!(t.record("s1","cc",0,10)?,[(0,"unconfirmed".to_owned())]);
        assert_eq!(t.raft_snapshot_rows("s1",6)?,baseline);
        Ok(())
    }

    #[test]
    fn cp_t3_2_new_owner_issues_receipts_after_the_former_owners_original_sequence() -> anyhow::Result<()> {
        use kiln_seams::records::{Tx,Write};
        let dir=tempfile::tempdir()?;
        let mut t=Turso::open(&dir.path().join("conversations.db"))?;
        t.enable_inbox_replication()?;
        t.begin_raft_projection("work",0)?;
        let former=1_000_000;
        let value=serde_json::to_vec(&serde_json::json!({"epoch":1,"at":10,"digest":"a","accepted_seq":former}))?;
        let prior=Row::Inbox(Box::new(InboxRow { session:"work".into(),family:"inbox".into(),key:b"work/a".to_vec(),value:Some(value) }));
        t.write_raft_rows("work",2,1,&[prior],ProjectionWrite::Install,Some(2))?;
        let tx=Tx{conds:vec![],writes:vec![Write::Put{family:"inbox".into(),key:b"work/b".to_vec(),value:br#"{"epoch":2,"at":11,"digest":"b"}"#.to_vec()}]};
        let result=t.inbox_commit_batch(&[(&tx,&["work".to_owned()])])?;
        assert!(matches!(result.as_slice(),[Ok(n)] if *n>former));
        let image=t.raft_snapshot_rows("work",2)?;
        t.journal_raft_rows("work",Some(2),&[Row::RecordReset{session:"work".into(),source:"cc".into(),at:0}],true)?;
        assert!(t.journal_raft_rows("work",Some(1),&[Row::RecordReset{session:"work".into(),source:"cc".into(),at:0}],true).is_err());
        assert_eq!(t.raft_snapshot_rows("work",2)?,image);
        Ok(())
    }

    #[test]
    fn cp_t3_2_legacy_founding_preserves_same_sequence_winner_and_refuses_foreign_custody() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversation.db"))?;
        let session = Row::Session {
            session: "s1".into(),
            info: Some("{}".into()),
            meta: None,
            state: None,
            changes: None,
            epoch: Some(0),
            open: true,
            at: 1,
        };
        let mut input = vec![
            session,
            Row::Machine {
                session: "s1".into(),
                machine: "west".into(),
            },
        ];
        for id in ["z", "a"] {
            input.extend(super::super::rows::of("s1", &said(id, 1, id), 1));
        }
        input.push(record(0, "native"));
        t.apply(&input)?;
        assert!(Engine::founding_rows(&mut t, "s1", 0, "east").is_err());
        let base = Engine::founding_rows(&mut t, "s1", 0, "west")?;
        assert!(base.contains(&record(0, "native")));
        t.begin_raft_projection("s1", 0)?;
        assert_eq!(Engine::founding_rows(&mut t, "s1", 0, "west")?, base);
        t.write_raft_rows("s1", 1, 1, &base, ProjectionWrite::Apply, None)?;
        let image = t.raft_snapshot_rows("s1", 1)?;
        let ids: Vec<_> = image
            .iter()
            .filter_map(|r| match r {
                Row::Event(e) => Some(e.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["z", "a"]);
        assert!(Engine::founding_rows(&mut t, "s1", 0, "west")?.is_empty());
        Ok(())
    }

    fn seq_v(v: u64) -> Value {
        super::seq(v)
    }

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    /// 2026-11-13 08:41Z: listing a session again reads its row by its key and its newest events by `events_by_seq`
    /// alone, no sort of its timeline: on a session of 100,000 events in a large store, Turso's plan for it uses the
    /// index with no temporary sort, and the read answers in a small part of the store's wait (READ_WAIT), where the
    /// read of its newest 4,000 (`kept`) is measured beside it.
    #[test]
    fn restore_reads_a_huge_sessions_row_and_newest_events_by_index_alone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversations.db"))?;
        let body = "x".repeat(400);
        let open = Update::Open { info: Box::new(crate::harness::session::Info { id: "huge".into(), kind: "claude".into(), ..Default::default() }), since: None };
        t.apply(&super::super::rows::of("huge", &open, 0))?;
        const EVENTS: u64 = 100_000;
        for chunk in (1..=EVENTS).collect::<Vec<_>>().chunks(5_000) {
            let rows: Vec<_> = chunk.iter().flat_map(|i| super::super::rows::of("huge", &said(&format!("m{i}"), *i, &body), 0)).collect();
            t.apply(&rows)?;
        }
        // Its plan: the index, read backwards, no sort.
        let link = t.link();
        let mut plan = link.prepare(&format!("EXPLAIN QUERY PLAN {}", sql::EVENTS_NEWEST))?;
        let mut said_plan = Vec::new();
        link.run(&mut plan, &[Value::from_text("huge".to_owned()), int(200)], |r| {
            said_plan.push((0..4).filter_map(|i| r.get::<String>(i).ok()).collect::<Vec<_>>().join(" "));
            Ok(())
        })?;
        let plan_text = said_plan.join(" | ");
        assert!(plan_text.contains("events_by_seq") && !plan_text.to_uppercase().contains("TEMP B-TREE"), "{plan_text}");
        let mut reader = t.reader().ok_or_else(|| anyhow::anyhow!("no reader"))?;
        let began = Instant::now();
        let k = reader.restorable("huge", 200)?.ok_or_else(|| anyhow::anyhow!("nothing of it"))?;
        let cheap = began.elapsed();
        assert_eq!(k.events.len(), 200);
        assert_eq!(k.events.last().map(|e| e.seq), Some(EVENTS), "its newest, oldest first");
        let began = Instant::now();
        let _ = reader.kept("huge", 4_000)?;
        let whole = began.elapsed();
        eprintln!("restore read of a 100k-event session: {cheap:?} (kept, its newest 4,000: {whole:?}); plan: {plan_text}");
        assert!(cheap < super::super::store::READ_WAIT / 20, "{cheap:?}");
        Ok(())
    }

    /// A subagent's thread, read by its own index: its events under the call that started it, oldest first, a page at a
    /// time back from where the viewer's begin, whatever else the session holds around them; on a store kept by an
    /// older kiln too (schema 3), once it opens.
    #[test]
    fn a_subagent_s_thread_is_read_by_its_call_alone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("conversations.db");
        let under = |id: &str, seq: u64, parent: Option<&str>| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: parent.map(str::to_owned), by: None, body: Body::Assistant { text: "words".into(), streaming: false } }] };
        let rows: Vec<_> = (1..=3_000u64).flat_map(|i| super::super::rows::of("a1", &under(&format!("m{i}"), i, (i % 10 == 0).then_some("toolu_5")), 0)).collect();
        {
            let mut old = Turso::open_at(&path, 3)?;
            old.apply(&rows)?;
        }
        let mut t = Turso::open(&path)?;
        let link = t.link();
        let mut plan = link.prepare(&format!("EXPLAIN QUERY PLAN {}", sql::EVENTS_OF_PARENT))?;
        let mut said_plan = Vec::new();
        link.run(&mut plan, &[Value::from_text("a1".to_owned()), Value::from_text("toolu_5".to_owned()), int(i64::MAX), int(100)], |r| {
            said_plan.push((0..4).filter_map(|i| r.get::<String>(i).ok()).collect::<Vec<_>>().join(" "));
            Ok(())
        })?;
        let plan_text = said_plan.join(" | ");
        assert!(plan_text.contains("events_by_parent") && !plan_text.to_uppercase().contains("SORTER") && !plan_text.to_uppercase().contains("TEMP B-TREE"), "{plan_text}");
        let mut reader = t.reader().ok_or_else(|| anyhow::anyhow!("no reader"))?;
        let seqs = |rows: Vec<EventRow>| rows.iter().map(|e| e.seq).collect::<Vec<_>>();
        let newest = seqs(reader.thread("a1", "toolu_5", None, 3)?);
        assert_eq!(newest, [2_980, 2_990, 3_000], "its newest, oldest first");
        assert_eq!(seqs(reader.thread("a1", "toolu_5", Some(2_980), 2)?), [2_960, 2_970], "the page before");
        assert_eq!(reader.thread("a1", "toolu_5", None, 1_000)?.len(), 300, "all of it, and nothing else");
        assert!(reader.thread("a1", "toolu_6", None, 100)?.is_empty());
        assert!(reader.thread("a2", "toolu_5", None, 100)?.is_empty(), "another session's call of that id isn't this one's");
        assert_eq!(seqs(t.thread("a1", "toolu_5", Some(31), 10)?), [10, 20, 30], "the writer reads it as the reader does");
        Ok(())
    }

    /// W21-R02's "Earlier messages": a page of a session's events is read by `events_by_seq` alone, exactly the events the
    /// sorted statements gave (`sql::EVENTS_BEFORE`, `sql::EVENTS_LAST`: by number, a number's newest revision first), for
    /// every place a page can end and every size, a number whose events the limit cuts among them.
    #[test]
    fn w21_r02_a_page_read_by_the_index_alone_is_the_page_the_sorted_read_gave() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversations.db"))?;
        for i in 1..=300u64 {
            t.apply(&super::super::rows::of("s", &said(&format!("m{i}"), i, "words"), 0))?;
        }
        // Other ids that took a number later (each a newer revision), two of them one number, at a page's edges and inside.
        for (i, id) in [(7u64, "b7"), (100, "b100"), (100, "c100"), (101, "b101"), (250, "b250"), (299, "b299"), (300, "b300"), (300, "c300")] {
            t.apply(&super::super::rows::of("s", &said(id, i, "other words"), 0))?;
        }
        // Another session's events are none of this one's.
        t.apply(&super::super::rows::of("other", &said("x1", 50, "words"), 0))?;
        let link = t.link();
        let ids = |rows: &[EventRow]| rows.iter().map(|e| format!("{}:{}", e.seq, e.id)).collect::<Vec<_>>();
        for before in [None, Some(1), Some(2), Some(8), Some(100), Some(101), Some(102), Some(251), Some(300), Some(301), Some(9_999)] {
            for limit in [1usize, 2, 3, 7, 50, 99, 100, 101, 199, 200, 201, 202, 299, 300, 308, 309, 1_000] {
                let bound = int(i64::try_from(limit)?);
                let mut sorted = match before {
                    Some(b) => link.event_rows(sql::EVENTS_BEFORE, &[txt("s"), seq_v(b), bound])?,
                    None => link.event_rows(sql::EVENTS_LAST, &[txt("s"), bound])?,
                };
                sorted.reverse();
                assert_eq!(ids(&link.events("s", before, limit)?), ids(&sorted), "before {before:?}, {limit} at the most");
            }
        }
        assert!(link.events("s", Some(50), 0)?.is_empty());
        assert!(link.events("none", None, 10)?.is_empty());
        Ok(())
    }

    /// A statement's plan, and what running it took by its own count (Turso's metrics, as SQLite counts a statement's VM
    /// steps): the rows it gave before `enough` said so, the rows it read, its sorts and its VM steps.
    fn counted(link: &Link<'_>, statement: &str, params: &[Value], enough: usize) -> anyhow::Result<(String, usize, u64, u64, u64)> {
        let mut plan = link.prepare(&format!("EXPLAIN QUERY PLAN {statement}"))?;
        let mut said_plan = Vec::new();
        link.run(&mut plan, params, |r| {
            said_plan.push((0..4).filter_map(|i| r.get::<String>(i).ok()).collect::<Vec<_>>().join(" "));
            Ok(())
        })?;
        let mut stmt = link.prepare(statement)?;
        stmt.reset_metrics();
        let mut rows = 0usize;
        link.run_while(&mut stmt, params, |_| {
            rows += 1;
            Ok(rows < enough)
        })?;
        let m = stmt.metrics();
        Ok((said_plan.join(" | "), rows, m.rows_read, m.sort_operations, m.insn_executed))
    }

    /// A store with a long session (`long` events) and a short one (300), each event 400 bytes.
    fn long_and_short(dir: &Path, long: u64) -> anyhow::Result<Turso> {
        let mut t = Turso::open(&dir.join("conversations.db"))?;
        let body = "x".repeat(400);
        for (session, n) in [("long", long), ("short", 300)] {
            for chunk in (1..=n).collect::<Vec<_>>().chunks(5_000) {
                let rows: Vec<_> = chunk.iter().flat_map(|i| super::super::rows::of(session, &said(&format!("m{i}"), *i, &body), 0)).collect();
                t.apply(&rows)?;
            }
        }
        Ok(t)
    }

    /// The same page of a long session costs its own events, not the session's (W21-R02's "Earlier messages"), by the
    /// statements' own count of their work: each page statement reads `events_by_seq` with no sort, reads no more rows
    /// than its page holds, and takes the same VM steps in the middle of 20,000 events, at their start, at their end and
    /// in a session of 300. (By the primary key's index, which Turso's planner takes in a process that made the store's
    /// file, as this one did, a page read the session's every event: `sql::EVENTS_BEFORE_BY_SEQ`.)
    #[test]
    fn w21_r02_a_page_of_a_long_session_reads_its_own_events_by_the_index() -> anyhow::Result<()> {
        const EVENTS: u64 = 20_000;
        const PAGE: i64 = 101;
        let dir = tempfile::tempdir()?;
        let t = long_and_short(dir.path(), EVENTS)?;
        let link = t.link();
        let page = usize::try_from(PAGE)?;
        let mut steps = Vec::new();
        for (what, statement, params) in [
            ("the middle of the long one", sql::EVENTS_BEFORE_BY_SEQ, vec![txt("long"), seq_v(EVENTS / 2), int(PAGE)]),
            ("its start", sql::EVENTS_BEFORE_BY_SEQ, vec![txt("long"), seq_v(150), int(PAGE)]),
            ("its end", sql::EVENTS_BEFORE_BY_SEQ, vec![txt("long"), seq_v(EVENTS + 1), int(PAGE)]),
            ("the short one", sql::EVENTS_BEFORE_BY_SEQ, vec![txt("short"), seq_v(250), int(PAGE)]),
            ("the long one's newest", sql::EVENTS_LAST_BY_SEQ, vec![txt("long"), int(PAGE)]),
            ("the short one's newest", sql::EVENTS_LAST_BY_SEQ, vec![txt("short"), int(PAGE)]),
        ] {
            let (plan, rows, read, sorts, took) = counted(&link, statement, &params, usize::MAX)?;
            assert!(plan.contains("events_by_seq") && !plan.to_uppercase().contains("SORTER") && !plan.to_uppercase().contains("TEMP B-TREE"), "{what}: {plan}");
            assert_eq!(rows, page, "{what}: a whole page");
            assert!(read <= u64::try_from(page)? && sorts == 0, "{what}: {read} rows read and {sorts} sorts for a page of {page}");
            steps.push((what, statement, took));
        }
        // No more steps where there are more events: each statement's are the same wherever its page is.
        for (what, statement, took) in &steps {
            let least = steps.iter().filter(|(_, s, _)| s == statement).map(|(_, _, n)| *n).min().unwrap_or(0);
            assert_eq!(*took, least, "{what}: {took} VM steps, {least} elsewhere");
        }
        // A number's own events: those rows, not the session's.
        let (plan, rows, read, _, _) = counted(&link, sql::EVENTS_AT, &[txt("long"), seq_v(EVENTS / 2)], usize::MAX)?;
        assert!(plan.contains("events_by_seq") && rows == 1 && read <= 2, "a number's events: {plan}, {rows} rows of {read} read");
        // And the reads that run them give the pages.
        let mut reader = t.reader().ok_or_else(|| anyhow::anyhow!("no reader"))?;
        for before in [Some(EVENTS + 1), Some(EVENTS * 3 / 4), Some(EVENTS / 2), Some(EVENTS / 4), Some(150), None] {
            let got = reader.events("long", before, page)?;
            let end = before.map_or(EVENTS, |b| b - 1);
            assert_eq!(got.len(), page.min(usize::try_from(end)?), "before {before:?}");
            assert_eq!(got.last().map(|e| e.seq), Some(end), "before {before:?}: its newest last");
        }
        Ok(())
    }

    /// Find in a long conversation (W21-R02) looks at a slice of it by `events_by_seq`, whatever its length, by the
    /// statements' own count of their work: looking at 200 events reads a few rows for each (the event, and whether its
    /// number has a newer one), sorts nothing, and takes the same VM steps in a session of 20,000 as in one of 300. (By
    /// the primary key's index, which Turso's planner took for "has its number a newer one", each event looked at read
    /// the session's every event: `sql::SEARCH_CANONICAL_SESSION`.)
    #[test]
    fn w21_r02_find_in_a_long_conversation_looks_at_its_slice_by_the_index() -> anyhow::Result<()> {
        const EVENTS: u64 = 20_000;
        const SLICE: usize = 200;
        let dir = tempfile::tempdir()?;
        let t = long_and_short(dir.path(), EVENTS)?;
        let link = t.link();
        for statement in [sql::SEARCH_CANONICAL_SESSION, sql::SEARCH_SESSION] {
            let mut steps = Vec::new();
            for (what, params) in [("the long one's end", vec![txt("long"), int(i64::MAX)]), ("its middle", vec![txt("long"), int(i64::try_from(EVENTS / 2)?)]), ("the short one", vec![txt("short"), int(i64::MAX)])] {
                let (plan, rows, read, sorts, took) = counted(&link, statement, &params, SLICE)?;
                assert!(!plan.contains("sqlite_autoindex") && plan.contains("events_by_seq") && !plan.to_uppercase().contains("SORTER"), "{what}: {plan}");
                assert_eq!(rows, SLICE, "{what}");
                // Each event looked at, and for the canonical search the probe of its number.
                assert!(read <= u64::try_from(SLICE * 2 + 2)? && sorts == 0, "{what}: {read} rows read and {sorts} sorts to look at {SLICE} events");
                steps.push((what, took));
            }
            // The same steps whatever the session's length: within one looked-at event's (a slice that ends where the index
            // does takes a step fewer there; measured 7,413 against 7,414).
            let least = steps.iter().map(|(_, n)| *n).min().unwrap_or(0);
            for (what, took) in &steps {
                assert!(took.abs_diff(least) < least / u64::try_from(SLICE)?, "{what}: {took} VM steps, {least} at the least");
            }
        }
        // And the search that runs them looks at the session newest first.
        let found = link.search_some(&Query { session: Some("long".into()), before_seq: Some(EVENTS + 1), ..Query::default() }, None, 1_000, 3)?.0;
        assert_eq!(found.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![EVENTS, EVENTS - 1, EVENTS - 2]);
        Ok(())
    }

    /// Plan 01a1cc09, step 2: a reader beside the writer reads what's committed while the writer holds a write it hasn't
    /// committed, on a thread of its own, without waiting for it; once it's committed, that too.
    #[test]
    fn a_reader_reads_the_last_commit_beside_a_write_held_open() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversations.db"))?;
        let rows = |from: u64, to: u64| (from..=to).flat_map(|i| super::super::rows::of("a1", &said(&format!("m{i}"), i, "words"), 0)).collect::<Vec<_>>();
        t.apply(&rows(1, 50))?;
        let mut reader = t.reader().ok_or_else(|| anyhow::anyhow!("no reader"))?;
        // A write held open on the writer's connection.
        t.exec("BEGIN")?;
        let mut w = t.writes()?;
        for row in rows(51, 60) {
            t.rev += 1;
            t.write(&mut w, &row, t.rev)?;
        }
        let (tx, rx) = mpsc::channel();
        let read = std::thread::spawn(move || {
            let page = reader.events("a1", None, 100);
            let _ = tx.send(());
            (reader, page)
        });
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| anyhow::anyhow!("the reader waited for the write held open"))?;
        let (mut reader, page) = read.join().map_err(|_| anyhow::anyhow!("the reader's thread panicked"))?;
        assert_eq!(page?.iter().map(|e| e.seq).max(), Some(50), "the reader reads the last commit, not the write held open");
        drop(w);
        t.exec("COMMIT")?;
        assert_eq!(reader.events("a1", None, 100)?.iter().map(|e| e.seq).max(), Some(60), "then what was committed");
        Ok(())
    }

    #[test]
    fn w21_r02_history_duplicate_sequence_latest_revision_wins_at_page_boundary() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversation.db"))?;
        for (id, n, text) in [("earlier",0,"first"),("old-id",1,"old branch"),("replacement",1,"current branch"),("next",2,"newest")] {
            t.apply(&super::super::rows::of("s1",&said(id,n,text),1))?;
        }
        let one = t.events("s1",Some(2),1)?;
        assert_eq!(one.first().map(|e| e.id.as_str()),Some("replacement"),"a LIMIT through duplicate seqs must include the current revision first");
        let all = t.events("s1",Some(2),10)?;
        assert_eq!(all.last().map(|e| e.id.as_str()),Some("replacement"),"the oldest-first API leaves the newest revision last for canonical upsert");
        // The canonical bounded page overlays by sequence; duplicate durable ids never create two
        // rows, and its exclusive next cursor crosses the whole duplicate group safely.
        let mut by_seq = std::collections::BTreeMap::new();
        for row in all { by_seq.insert(row.seq,serde_json::from_str::<serde_json::Value>(&row.body)?); }
        let (page,window) = crate::harness::history::page(by_seq.into_values().collect(),2,1,16384,"cursor".into(),false);
        assert_eq!(page.len(),1);
        assert_eq!(page.first().and_then(|e| e.get("id")).and_then(serde_json::Value::as_str),Some("replacement"));
        assert_eq!(window.before,Some(1)); assert!(window.has_more);
        assert_eq!(t.events("s1",window.before,10)?.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),vec!["earlier"]);
        Ok(())
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
        let all = t.changes(0, 1 << 20, false, true)?;
        let mut kinds: Vec<String> = all.rows.iter().map(|c| format!("{}:{}", c.row.session(), match &c.row { Row::Session { .. } => "session", Row::Closed { .. } => "closed", Row::Event(_) => "event", _ => "other" })).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["a1:event", "a1:event", "a1:session", "a2:closed", "a2:session"]);
        // Numbers of their own, none twice, and what's written next comes after them.
        let mut revs: Vec<u64> = all.rows.iter().filter(|c| !matches!(c.row, Row::Closed { .. })).map(|c| c.rev).collect();
        revs.dedup();
        assert_eq!(revs.len(), 4);
        let top = all.upto;
        t.apply(&[Row::Event(Box::new(super::super::rows::event("a1", &Event { id: "m3".into(), seq: 3, at: 9, parent: None, by: None, body: Body::Assistant { text: "new".into(), streaming: false } }).ok_or_else(|| anyhow::anyhow!("no row"))?))])?;
        let next = t.changes(top, 1 << 20, false, true)?;
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
    fn what_comes_again_as_it_is_kept_writes_nothing_and_no_feed_sends_it_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut runner = Turso::open(&dir.path().join("r.db"))?;
        let long = format!("{{\"type\":\"assistant\",\"text\":\"{}\"}}", "the agent read the file and wrote a patch ".repeat(40));
        let mut rows = super::super::rows::of("s1", &said("m1", 1, "first"), 5);
        rows.extend(super::super::rows::of("s1", &said("m2", 2, "second"), 5));
        rows.extend([record(0, "{\"type\":\"summary\"}"), record(1, &long)]);
        runner.apply(&rows)?;
        let top = runner.top();
        // Read again (a restart following the transcript from its top, a timeline written again):
        // nothing changes, and nothing gets a number for a feed to send.
        runner.apply(&rows)?;
        assert_eq!(runner.top(), top);
        assert!(runner.changes(top, 1 << 20, false, true)?.rows.is_empty());
        // What changed is written, and only that.
        runner.apply(&super::super::rows::of("s1", &said("m2", 2, "second, revised"), 5))?;
        assert_eq!(runner.top(), top + 1);
        // The server, sent what it has by a new store of the same machine (a new runner process
        // that took the sessions over): nothing written, nothing numbered, nothing for a mirror.
        let mut server = Turso::open(&dir.path().join("s.db"))?;
        let all = runner.changes(0, 1 << 20, true, true)?;
        let first = server.take("box/old", Some("box"), 0, &all)?;
        assert_eq!((first.applied, first.same), (all.rows.len(), 0));
        let at = server.top();
        let again = server.take("box/new", Some("box"), 0, &all)?;
        assert_eq!((again.applied, again.same, again.gap, again.wrote), (0, all.rows.len(), false, None));
        assert_eq!(server.top(), at);
        assert_eq!(server.peer("box/new")?, all.upto);
        assert_eq!(server.record("s1", "cc", 0, 10)?.into_iter().map(|(_, l)| l).collect::<Vec<_>>(), vec!["{\"type\":\"summary\"}".to_owned(), long]);
        assert_eq!(server.kept("s1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect::<Vec<_>>()), Some(vec!["first".to_owned(), "second, revised".to_owned()]));
        Ok(())
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
            if steps == 1 {
                // CP-T3.2. A fence and a certified origin written after the copy began
                // survive the swap even though neither has its own feed revision.
                for db in [engine.as_mut(), &mut same as &mut dyn Engine] {
                    db.confirm_local_owner_fence("former", 3)?;
                    db.write_raft_rows("group", 1, 1, &[], ProjectionWrite::Certified { origin:[7;32],recover:false }, Some(1))?;
                }
            }
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

    /// A store let go of for a new build to take over (`Engine::release`) while it's being made
    /// compact: the compaction stops, and its copy goes rather than stay behind as the file moves.
    #[test]
    fn a_store_let_go_of_while_it_is_made_compact_leaves_no_copy_beside_it() -> anyhow::Result<()> {
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
        engine.release()?;
        for left in [".compact", ".compact.tmp", ".compact.tmp-wal", ".compact.tmp-shm"] {
            assert!(!beside(&path, left).exists(), "{left} left beside the store");
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

    // W21-R02: older-history find cannot use time to divide events at the same instant.
    #[test]
    fn w21_r02_sequence_search_keeps_same_timestamp_history_reachable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("c.db"))?;
        let rows: Vec<Row> = (0..12u64).map(|n| Row::Event(Box::new(EventRow {
            session: if n % 2 == 0 { "wanted" } else { "other" }.into(), id: format!("e{n}"), seq: n / 2,
            at: 1_000, parent: None, by: None, kind: "assistant".into(), level: None, tool: None,
            status: None, text: "needle".into(), body: "{}".into(),
        }))).collect();
        t.apply(&rows)?;
        let q = Query { q: "needle".into(), session: Some("wanted".into()), before_seq: Some(3), ..Query::default() };
        let (all, _) = t.search_some(&q, None, usize::MAX, usize::MAX)?;
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 1, 0]);
        assert!(all.iter().all(|e| e.session == "wanted"));
        let (mut paged, mut from) = (Vec::new(), None);
        loop {
            let (some, next) = t.search_some(&q, from, 1, 1)?;
            paged.extend(some);
            match next { Some(next) => from = Some(next), None => break }
        }
        assert_eq!(paged, all);
        assert!(t.search_some(&Query { before_seq: Some(0), ..q }, None, usize::MAX, usize::MAX)?.0.is_empty());
        Ok(())
    }

    #[test]
    fn w21_r02_bounded_find_excludes_superseded_sequence_text_without_changing_legacy_search() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut t = Turso::open(&dir.path().join("conversation.db"))?;
        for (id, n, text) in [("first", 0, "needle first"), ("old-id", 1, "superseded needle"), ("replacement", 1, "canonical needle"), ("next", 2, "canonical needle")] {
            t.apply(&super::super::rows::of("s1", &said(id, n, text), 1))?;
        }
        // Another conversation's newer row at the same sequence cannot hide this one's row.
        t.apply(&super::super::rows::of("other", &said("other-id", 1, "other needle"), 1))?;
        let query = |text: &str| Query { q: text.into(), session: Some("s1".into()), before_seq: Some(3), ..Query::default() };
        let q = query("needle");
        let (all, _) = t.search_some(&q, None, usize::MAX, usize::MAX)?;
        assert_eq!(all.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["next", "replacement", "first"]);
        assert!(t.search_some(&query("superseded"), None, usize::MAX, usize::MAX)?.0.is_empty(), "obsolete text cannot become a bounded Find result");
        assert_eq!(t.search_some(&query("canonical"), None, usize::MAX, usize::MAX)?.0.len(), 2);
        let legacy = Query { before_seq: None, ..query("superseded") };
        assert_eq!(t.search_some(&legacy, None, usize::MAX, usize::MAX)?.0.first().map(|e| e.id.as_str()), Some("old-id"));
        let (mut paged, mut from) = (Vec::new(), None);
        loop {
            let (some, next) = t.search_some(&q, from, 1, 1)?;
            paged.extend(some);
            match next { Some(next) => from = Some(next), None => break }
        }
        assert_eq!(paged, all, "one-row search slices cross duplicate sequence groups without gaps or repeats");
        // A correction to an older id keeps its old rowid: durable revision, not rowid, wins.
        t.apply(&super::super::rows::of("s1", &said("old-id", 1, "corrected needle"), 1))?;
        let (corrected, _) = t.search_some(&query("needle"), None, usize::MAX, usize::MAX)?;
        assert_eq!(corrected.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["next", "old-id", "first"]);
        assert_eq!(t.search_some(&query("canonical"), None, usize::MAX, usize::MAX)?.0.len(), 1);
        assert_eq!(t.search_some(&query("corrected"), None, usize::MAX, usize::MAX)?.0.first().map(|e| e.id.as_str()), Some("old-id"));
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

    /// A statement that finds the store's database busy says so at once, naming the statement, and is
    /// never waited out: here the one way to make it busy, a second open of the same file in this
    /// process (Turso keeps one database a file, so the two share it), a write under way in the first.
    /// A store never opens a second (one connection, its writer's); the 50 tries 10 ms apart a busy
    /// statement was given, half a second of its writer held, hid such a fault behind a wait.
    #[test]
    fn a_busy_database_is_an_error_naming_the_statement_not_waited_out() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let (first, second) = (Turso::open(&path)?, Turso::open(&path)?);
        // The first holds the database's write lock: a write under way, not yet committed.
        first.exec("BEGIN")?;
        first.set_meta("held", "by the first")?;
        let busy = second.set_meta("wants", "the lock").err().map(|e| format!("{e:#}"));
        first.exec("ROLLBACK")?;
        anyhow::ensure!(busy.as_deref().is_some_and(|e| e.contains("said busy for `INSERT INTO meta")), "not said at once with its statement: {busy:?}");
        second.set_meta("wants", "the lock")?;
        Ok(())
    }

    /// Nothing waits for another process to let go of a store any more (a store is let go of before
    /// it's handed on, `super::super::store::Store::close`): one whose file another live process
    /// holds is refused while it does, saying so, where it was waited for (15 s at most, a runner
    /// upgraded not quite let go of it), and opens once that process has let go.
    #[cfg(unix)]
    #[test]
    fn a_store_another_process_holds_is_refused_while_it_does_not_waited_for() -> anyhow::Result<()> {
        use std::io::Write as _;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("c.db");
        let id = Turso::open(&path)?.id();
        // Held as Turso holds it (an fcntl lock), by another process, until it's told to let go, or
        // for 5 s (well within the 15 s an open waited): an open that waits opens it then.
        let holder = std::process::Command::new("python3")
            .args(["-c", "import fcntl, select, sys; f = open(sys.argv[1], 'r+'); fcntl.lockf(f, fcntl.LOCK_EX); print('held', flush=True); select.select([sys.stdin], [], [], 5)", &path.to_string_lossy()])
            .stdin(std::process::Stdio::piped())
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
        let refused = Turso::open(&path).err().map(|e| format!("{e:#}"));
        // Let go of, and gone, before anything is said of what the open did.
        if let Some(mut tell) = holder.stdin.take() {
            let _ = tell.write_all(b"go");
        }
        let _ = holder.wait();
        anyhow::ensure!(refused.as_deref().is_some_and(|e| e.contains("locked by another process")), "opened while another process held the file, or refused otherwise: {refused:?}");
        anyhow::ensure!(Turso::open(&path)?.id() == id, "not the same store once let go of");
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
        let fed: Vec<String> = t.changes(before, 1 << 20, false, true)?.rows.into_iter().filter_map(|c| if let Row::Record(r) = c.row { Some(r.line) } else { None }).collect();
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
            let batch = a.changes(from, 2_000, true, true)?;
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
