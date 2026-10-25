//! Records in a file of kiln's embedded Turso (vendor/turso): kiln-seams' `Records` until Amphora's
//! own (CP-T1.5). Every version of every row is one row of one table, so a read at a past revision
//! and a family's change feed come from the same rows.
//!
//! One thread owns the file. Transactions handed to it together are written together, durably
//! (Turso's WAL with `SyncMode::Full`), each still at a revision of its own and under its own
//! conditions, and each is answered only once the write that holds it is on disk: group commit.
//! Reads are answered on the same thread, after the commits that arrived with them, so a read never
//! sees what isn't durable. The head, the horizons and each family's newest change are also kept in
//! memory, so `head` and `wait` answer without the thread.
//!
//! Bounds: at most [`QUEUE`] requests wait for the thread (past it, a caller waits its turn); one
//! write gathers at most [`BATCH`] transactions, or about [`BATCH_BYTES`] of values; at most
//! [`ROWS_MAX`] rows live (past it, a commit is refused `Full`). The history of the last
//! [`KEEP_REVS`] revisions is kept: each [`COMPACT_EVERY`] revisions what's older is compacted, a
//! step of [`COMPACT_STEP`] versions at a time between writes, and reads from before what's kept
//! are refused `Compacted`.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

use std::collections::{HashMap, HashSet};
use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::future::BoxFuture;
use kiln_seams::records::{Change, Changes, Cond, Consistency, FAMILY_MAX, KEY_MAX, Page, Records, Refusal, Rev, Row, SCAN_MAX, Scan, TX_MAX, Tx, VALUE_MAX, Write};
use parking_lot::Mutex;
use turso_core::{Connection, ConnectOpts, Database, DatabaseOpts, IO, OpenOptions, PlatformIO, SqliteDialect, Statement, StepResult, SyncMode, Value};

/// Requests that wait for the store's thread, at most: past it, a caller waits its turn.
pub const QUEUE: usize = 4096;
/// Transactions written together, at most.
pub const BATCH: usize = 512;
/// Bytes of values written together, about: a write takes no more once past it.
pub const BATCH_BYTES: usize = 16 << 20;
/// Rows live at once, at most: a commit that would leave more is refused `Full`.
pub const ROWS_MAX: usize = 10_000_000;
/// The history kept: the last this many revisions, and each row as it was before them.
pub const KEEP_REVS: Rev = 100_000;
/// The history is compacted each time this many revisions are committed past what's kept.
pub const COMPACT_EVERY: Rev = 10_000;
/// Versions looked at in one step of compacting, between writes (above [`TX_MAX`], so a step always
/// moves past a revision).
pub const COMPACT_STEP: usize = 2_000;
/// Keys a compaction remembers having pruned, at most: past it, it forgets them (pruning again is
/// only work done twice).
const PRUNED_MAX: usize = 100_000;
/// How often a statement that finds the file busy tries again, and how long it waits between.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: Duration = Duration::from_millis(10);
/// How long opening waits for whoever else holds the file to let go of it: this process's last
/// image, or the runner process before this one on its way out.
const LOCK_WAIT: Duration = Duration::from_secs(15);

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS meta (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS versions (family TEXT NOT NULL, key BLOB NOT NULL, rev INTEGER NOT NULL, n INTEGER NOT NULL, value BLOB, PRIMARY KEY (family, key, rev));
    CREATE INDEX IF NOT EXISTS versions_by_rev ON versions (family, rev, n)";
const META_GET: &str = "SELECT name, value FROM meta";
const META_SET: &str = "INSERT INTO meta (name, value) VALUES (?1, ?2) ON CONFLICT (name) DO UPDATE SET value = excluded.value";
const FAMILIES: &str = "SELECT family, MAX(rev) FROM versions GROUP BY family";
// A row's newest version (at a revision) is found by its key: ordered by the whole primary key (the
// same order, its family and key given), Turso seeks it, where ordered by `rev` alone it walks the
// family's every version by `versions_by_rev` (a take's cost growing with what the family holds).
const LATEST: &str = "SELECT rev, value IS NULL FROM versions WHERE family = ?1 AND key = ?2 ORDER BY family DESC, key DESC, rev DESC LIMIT 1";
const GET_LATEST: &str = "SELECT rev, value FROM versions WHERE family = ?1 AND key = ?2 ORDER BY family DESC, key DESC, rev DESC LIMIT 1";
const GET_AT: &str = "SELECT rev, value FROM versions WHERE family = ?1 AND key = ?2 AND rev <= ?3 ORDER BY family DESC, key DESC, rev DESC LIMIT 1";
const AT_DELETED: &str = "SELECT rev, value IS NULL FROM versions WHERE family = ?1 AND key = ?2 AND rev <= ?3 ORDER BY family DESC, key DESC, rev DESC LIMIT 1";
const PUT: &str = "INSERT INTO versions (family, key, rev, n, value) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (family, key, rev) DO UPDATE SET n = excluded.n, value = excluded.value";
const SCAN_TO: &str = "SELECT key, rev, value FROM versions WHERE family = ?1 AND key >= ?2 AND key < ?3 ORDER BY key, rev";
const SCAN_ON: &str = "SELECT key, rev, value FROM versions WHERE family = ?1 AND key >= ?2 ORDER BY key, rev";
// A family's versions in the order they were written, from a revision: Turso plans these by the
// primary key and sorts the family's every version (a compaction's step costing what the family
// holds: 100 ms at 100,000 versions), so they name the index that walks them in that order.
const FEED: &str = "SELECT rev, key, value FROM versions INDEXED BY versions_by_rev WHERE family = ?1 AND rev > ?2 ORDER BY rev, n LIMIT ?3";
const WINDOW: &str = "SELECT key, rev FROM versions INDEXED BY versions_by_rev WHERE family = ?1 AND rev > ?2 AND rev <= ?3 ORDER BY rev, n LIMIT ?4";
const PRUNE_BELOW: &str = "DELETE FROM versions WHERE family = ?1 AND key = ?2 AND rev < ?3";
const PRUNE_AT: &str = "DELETE FROM versions WHERE family = ?1 AND key = ?2 AND rev = ?3";
#[cfg(test)]
const COUNT: &str = "SELECT count(*) FROM versions";

/// What the store holds at most, and how much history it keeps ([`ROWS_MAX`] and the rest).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub rows: usize,
    pub keep: Rev,
    pub every: Rev,
    pub step: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { rows: ROWS_MAX, keep: KEEP_REVS, every: COMPACT_EVERY, step: COMPACT_STEP }
    }
}

/// What the store's thread has done: transactions committed, the durable writes they took, and
/// compactions finished; and the time its writes took, applying their transactions and then making
/// them durable (nanoseconds, summed).
#[derive(Debug, Default)]
pub struct Stats {
    pub commits: AtomicU64,
    pub writes: AtomicU64,
    pub compactions: AtomicU64,
    pub apply_ns: AtomicU64,
    pub sync_ns: AtomicU64,
}

/// What the store's thread last made durable, readable without it.
#[derive(Default)]
struct Seen {
    head: Rev,
    /// Reads at a revision before it are refused `Compacted`.
    horizon: Rev,
    /// The change feed is whole after it: from before it, refused `Compacted`.
    feed: Rev,
    /// Each family's newest change.
    families: HashMap<String, Rev>,
    /// The thread stopped, and why.
    stopped: Option<String>,
}

enum Job {
    Commit(Tx, oneshot::Sender<Result<Rev, Refusal>>),
    Get(String, Vec<u8>, Consistency, oneshot::Sender<Result<Option<Row>, Refusal>>),
    Scan(String, Scan, oneshot::Sender<Result<Page, Refusal>>),
    Changes(String, Rev, usize, oneshot::Sender<Result<Changes, Refusal>>),
    Compact(Rev, oneshot::Sender<()>),
    /// The versions the file holds (tests).
    #[cfg(test)]
    Count(oneshot::Sender<Result<u64, Refusal>>),
}

/// Records in a Turso file, its thread answering for it. Dropped, the thread finishes what it was
/// given and lets go of the file.
pub struct TursoRecords {
    jobs: async_channel::Sender<Job>,
    seen: Arc<Mutex<Seen>>,
    changed: Arc<event_listener::Event>,
    stats: Arc<Stats>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TursoRecords {
    /// The records in the file at `path`, made as needed.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with(path, Limits::default(), None)
    }

    /// [`TursoRecords::open`], keeping what `limits` say; with `start`, its thread takes nothing
    /// until that's said (tests: what's handed to it before then is written together).
    pub(crate) fn open_with(path: &Path, limits: Limits, start: Option<std::sync::mpsc::Receiver<()>>) -> anyhow::Result<Self> {
        let db = Db::open(path, limits)?;
        let seen = Arc::new(Mutex::new(Seen::default()));
        db.publish(&seen);
        let (jobs, rx) = async_channel::bounded(QUEUE);
        let changed = Arc::new(event_listener::Event::new());
        let stats = Arc::new(Stats::default());
        let (s, c, st) = (Arc::clone(&seen), Arc::clone(&changed), Arc::clone(&stats));
        let thread = std::thread::Builder::new().name("kiln-records".into()).spawn(move || {
            if let Some(start) = start {
                let _ = start.recv();
            }
            serve(db, &rx, &s, &c, &st);
        })?;
        Ok(Self { jobs, seen, changed, stats, thread: Mutex::new(Some(thread)) })
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Forgets the history from before `before` now (at most the head), as the store does on its
    /// own each [`COMPACT_EVERY`] revisions, and completes once it has.
    pub fn compact(&self, before: Rev) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            if self.jobs.send(Job::Compact(before, tx)).await.is_ok() {
                let _ = rx.await;
            }
        })
    }

    /// Hands `job` (made with its answer's sender) to the thread, and waits for the answer.
    async fn ask<T>(&self, job: impl FnOnce(oneshot::Sender<Result<T, Refusal>>) -> Job) -> Result<T, Refusal> {
        let (tx, rx) = oneshot::channel();
        if self.jobs.send(job(tx)).await.is_err() {
            return Err(self.stopped());
        }
        rx.await.unwrap_or_else(|_| Err(self.stopped()))
    }

    fn stopped(&self) -> Refusal {
        Refusal::Unavailable(self.seen.lock().stopped.clone().unwrap_or_else(|| "the records store has stopped".into()))
    }
}

impl Drop for TursoRecords {
    fn drop(&mut self) {
        self.jobs.close();
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
    }
}

impl Records for TursoRecords {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        Box::pin(async move {
            check_family(family)?;
            check_key(key)?;
            self.ask(|r| Job::Get(family.to_owned(), key.to_vec(), at, r)).await
        })
    }

    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        Box::pin(async move {
            check_family(family)?;
            check_limit(scan.limit)?;
            self.ask(|r| Job::Scan(family.to_owned(), scan, r)).await
        })
    }

    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            check_tx(&tx)?;
            self.ask(|r| Job::Commit(tx, r)).await
        })
    }

    fn changes<'a>(&'a self, family: &'a str, after: Rev, limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        Box::pin(async move {
            check_family(family)?;
            check_limit(limit)?;
            self.ask(|r| Job::Changes(family.to_owned(), after, limit, r)).await
        })
    }

    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            check_family(family)?;
            loop {
                let listener = self.changed.listen();
                {
                    let s = self.seen.lock();
                    // The feed is whole only after its horizon: a change before it may have gone.
                    if after < s.feed {
                        return Err(Refusal::Compacted { asked: after, horizon: s.feed });
                    }
                    if let Some(r) = s.families.get(family).copied().filter(|r| *r > after) {
                        return Ok(r);
                    }
                    if let Some(why) = &s.stopped {
                        return Err(Refusal::Unavailable(why.clone()));
                    }
                }
                listener.await;
            }
        })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        let head = self.seen.lock().head;
        Box::pin(async move { Ok(head) })
    }
}

/// The thread's work: what's given, a batch at a time, until the store is dropped. Commits that
/// came together are written together; the reads that came with them are answered after.
fn serve(mut db: Db, jobs: &async_channel::Receiver<Job>, seen: &Mutex<Seen>, changed: &event_listener::Event, stats: &Stats) {
    let why = loop {
        // A compaction under way goes on a step at a time whenever nothing waits.
        let first = if db.compacting.is_some() {
            match jobs.try_recv() {
                Ok(j) => j,
                Err(async_channel::TryRecvError::Empty) => {
                    db.compact_step(stats);
                    db.publish(seen);
                    continue;
                }
                Err(async_channel::TryRecvError::Closed) => break "the records store was closed".to_owned(),
            }
        } else {
            match jobs.recv_blocking() {
                Ok(j) => j,
                Err(_) => break "the records store was closed".to_owned(),
            }
        };
        let mut bytes = job_bytes(&first);
        let mut batch = vec![first];
        while batch.len() < BATCH && bytes < BATCH_BYTES {
            match jobs.try_recv() {
                Ok(j) => {
                    bytes = bytes.saturating_add(job_bytes(&j));
                    batch.push(j);
                }
                Err(_) => break,
            }
        }
        let mut commits = Vec::new();
        let mut rest = Vec::new();
        for j in batch {
            match j {
                Job::Commit(tx, reply) => commits.push((tx, reply)),
                other => rest.push(other),
            }
        }
        if !commits.is_empty() {
            let n = u64::try_from(commits.len()).unwrap_or(u64::MAX);
            let (txs, replies): (Vec<Tx>, Vec<_>) = commits.into_iter().unzip();
            let answers = db.commit_batch(&txs, stats);
            stats.writes.fetch_add(1, Ordering::AcqRel);
            stats.commits.fetch_add(n, Ordering::AcqRel);
            // What's durable is said before anyone is answered: a caller told its commit is in
            // finds it in the head and the feed.
            db.publish(seen);
            for (reply, answer) in replies.into_iter().zip(answers) {
                let _ = reply.send(answer);
            }
            changed.notify(usize::MAX);
        }
        for j in rest {
            match j {
                Job::Get(family, key, at, reply) => {
                    let _ = reply.send(db.get(&family, &key, at));
                }
                Job::Scan(family, scan, reply) => {
                    let _ = reply.send(db.scan(&family, &scan));
                }
                Job::Changes(family, after, limit, reply) => {
                    let _ = reply.send(db.changes(&family, after, limit));
                }
                Job::Compact(before, reply) => {
                    // One under way finishes first; then this one, whole. Bounded: each step moves
                    // past at least one revision of a family's window, or on to the next family.
                    for target in [None, Some(before)] {
                        if let Some(to) = target {
                            db.start_compaction(to);
                        }
                        while db.compacting.is_some() {
                            db.compact_step(stats);
                        }
                    }
                    db.publish(seen);
                    changed.notify(usize::MAX);
                    let _ = reply.send(());
                }
                #[cfg(test)]
                Job::Count(reply) => {
                    let _ = reply.send(db.versions().map_err(unavailable));
                }
                Job::Commit(..) => {}
            }
        }
        if db.compacting.is_none() && db.head >= db.compacted.saturating_add(db.limits.keep).saturating_add(db.limits.every) {
            db.start_compaction(db.head.saturating_sub(db.limits.keep));
            db.publish(seen);
            // Waits from before the new horizon are refused now, not left waiting.
            changed.notify(usize::MAX);
        }
    };
    seen.lock().stopped = Some(why);
    changed.notify(usize::MAX);
}

/// About how many bytes `job` writes.
fn job_bytes(job: &Job) -> usize {
    match job {
        Job::Commit(tx, _) => tx
            .writes
            .iter()
            .map(|w| match w {
                Write::Put { key, value, .. } => key.len().saturating_add(value.len()),
                Write::Delete { key, .. } => key.len(),
            })
            .fold(0usize, usize::saturating_add),
        _ => 0,
    }
}

/// A compaction under way: from the last one's revision to `to`, a family at a time, each in the
/// order its versions were written.
struct Compaction {
    to: Rev,
    from: Rev,
    families: Vec<String>,
    at: usize,
    /// The window's versions are looked at after this revision (none: from `from` on).
    cursor: Option<Rev>,
    /// Keys of this family pruned in this compaction.
    pruned: HashSet<Vec<u8>>,
}

/// The file, as its thread holds it.
struct Db {
    io: Arc<dyn IO>,
    _db: Arc<Database>,
    conn: Arc<Connection>,
    statements: HashMap<&'static str, Statement>,
    limits: Limits,
    head: Rev,
    horizon: Rev,
    feed: Rev,
    /// The revision the last compaction finished at.
    compacted: Rev,
    live: u64,
    families: HashMap<String, Rev>,
    compacting: Option<Compaction>,
    /// The newest version of each row a write under way has read or written (its revision, and
    /// whether it's live): each is read once a write, however many of its transactions touch it.
    /// Emptied as each write ends.
    seen: HashMap<(String, Vec<u8>), Option<(Rev, bool)>>,
}

impl Db {
    fn open(path: &Path, limits: Limits) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path.to_str().ok_or_else(|| anyhow::anyhow!("the records file's path isn't UTF-8: {}", path.display()))?;
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().map_err(fail)?);
        let until = Instant::now().checked_add(LOCK_WAIT);
        let db = loop {
            match Database::open(io.clone(), name, OpenOptions::new(Arc::new(SqliteDialect)).db_opts(DatabaseOpts::new())) {
                Err(turso_core::LimboError::LockingError(_)) if until.is_some_and(|u| Instant::now() < u) => std::thread::sleep(Duration::from_millis(100)),
                opened => break opened.map_err(fail)?,
            }
        };
        let conn = db.connect(ConnectOpts::new()).map_err(fail)?;
        // Committed is on disk: what a commit is answered after. On Apple's systems a plain fsync
        // leaves it in the drive's cache: there the full one (F_FULLFSYNC), as Amphora's store does.
        conn.set_sync_mode(SyncMode::Full);
        conn.set_sync_type(turso_core::io::FileSyncType::FullFsync);
        let mut d = Self {
            io,
            _db: db,
            conn,
            statements: HashMap::new(),
            limits,
            head: 0,
            horizon: 0,
            feed: 0,
            compacted: 0,
            live: 0,
            families: HashMap::new(),
            compacting: None,
            seen: HashMap::new(),
        };
        d.exec(SCHEMA)?;
        let mut meta: HashMap<String, i64> = HashMap::new();
        d.query(META_GET, &[], |r| {
            meta.insert(r.get::<String>(0).map_err(fail)?, r.get::<i64>(1).map_err(fail)?);
            Ok(true)
        })?;
        let num = |k: &str| meta.get(k).and_then(|v| u64::try_from(*v).ok()).unwrap_or(0);
        (d.head, d.horizon, d.feed, d.compacted, d.live) = (num("head"), num("horizon"), num("feed"), num("compacted"), num("live"));
        let mut families = HashMap::new();
        d.query(FAMILIES, &[], |r| {
            families.insert(r.get::<String>(0).map_err(fail)?, u64::try_from(r.get::<i64>(1).map_err(fail)?).unwrap_or(0));
            Ok(true)
        })?;
        // The head is never behind what's written (they're written together; this only reads so).
        d.head = families.values().copied().fold(d.head, Rev::max);
        d.families = families;
        Ok(d)
    }

    /// What's durable, for readers outside the thread.
    fn publish(&self, seen: &Mutex<Seen>) {
        let mut s = seen.lock();
        s.head = self.head;
        s.horizon = self.horizon;
        s.feed = self.feed;
        for (f, r) in &self.families {
            if s.families.get(f) != Some(r) {
                s.families.insert(f.clone(), *r);
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

    /// Runs `sql` (prepared once, then kept) with `params`, handing each row to `each` until it
    /// says no more.
    fn query(&mut self, sql: &'static str, params: &[Value], each: impl FnMut(&turso_core::Row) -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let stmt = match self.statements.entry(sql) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(self.conn.prepare(sql).map_err(fail)?),
        };
        run_while(&self.io, stmt, params, each)
    }

    fn set_meta(&mut self, name: &str, value: Rev) -> anyhow::Result<()> {
        self.query(META_SET, &[Value::from_text(name.to_owned()), num(value)?], |_| Ok(true))
    }

    /// The newest version of a row: its revision, and whether it's live (not a delete). Read once
    /// a write ([`Db::seen`]).
    fn latest(&mut self, family: &str, key: &[u8]) -> anyhow::Result<Option<(Rev, bool)>> {
        if let Some(v) = self.seen.get(&(family.to_owned(), key.to_vec())) {
            return Ok(*v);
        }
        let mut out = None;
        self.query(LATEST, &[text(family), blob(key)?], |r| {
            out = Some((rev_of(r, 0)?, r.get::<i64>(1).map_err(fail)? == 0));
            Ok(false)
        })?;
        self.seen.insert((family.to_owned(), key.to_vec()), out);
        Ok(out)
    }

    /// Commits `txs` in one durable write, each at a revision of its own and under its own
    /// conditions, in order: what each was answered. A write that fails answers every one of
    /// them `Unavailable`, and none of it stays.
    fn commit_batch(&mut self, txs: &[Tx], stats: &Stats) -> Vec<Result<Rev, Refusal>> {
        let (head, live) = (self.head, self.live);
        let mut changed: Vec<(String, Rev)> = Vec::new();
        self.seen.clear();
        let applied: anyhow::Result<Vec<Result<Rev, Refusal>>> = (|| {
            let began = Instant::now();
            self.exec("BEGIN")?;
            let mut out = Vec::with_capacity(txs.len());
            for tx in txs {
                out.push(self.apply(tx, &mut changed)?);
            }
            if self.head != head {
                self.set_meta("head", self.head)?;
                self.set_meta("live", self.live)?;
            }
            let applied = Instant::now();
            self.exec("COMMIT")?;
            stats.apply_ns.fetch_add(nanos(applied.duration_since(began)), Ordering::AcqRel);
            stats.sync_ns.fetch_add(nanos(applied.elapsed()), Ordering::AcqRel);
            Ok(out)
        })();
        // What a write read is the write's alone: a failed one's never was.
        self.seen.clear();
        match applied {
            Ok(out) => {
                for (family, rev) in changed {
                    let r = self.families.entry(family).or_insert(0);
                    *r = (*r).max(rev);
                }
                out
            }
            Err(e) => {
                let _ = self.exec("ROLLBACK");
                (self.head, self.live) = (head, live);
                let why = format!("writing the records failed: {e:#}");
                tracing::warn!(target: "kiln.records", event = "write_failed", error = %why, "a write of the records failed; its transactions were refused");
                txs.iter().map(|_| Err(Refusal::Unavailable(why.clone()))).collect()
            }
        }
    }

    /// One transaction inside the write under way: its conditions checked against what the write
    /// holds so far, then all of its writes or none.
    fn apply(&mut self, tx: &Tx, changed: &mut Vec<(String, Rev)>) -> anyhow::Result<Result<Rev, Refusal>> {
        for c in &tx.conds {
            match c {
                Cond::Absent { family, key } => {
                    if let Some((at, true)) = self.latest(family, key)? {
                        return Ok(Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at: Some(at) }));
                    }
                }
                Cond::At { family, key, rev } => {
                    let now = self.latest(family, key)?.and_then(|(at, live)| live.then_some(at));
                    if now != Some(*rev) {
                        return Ok(Err(Refusal::Conflict { family: family.clone(), key: key.clone(), at: now }));
                    }
                }
            }
        }
        if tx.writes.is_empty() {
            return Ok(Ok(self.head));
        }
        // The rows this commit would leave live, checked before anything is written.
        let mut live = self.live;
        let mut was: HashMap<(&str, &[u8]), bool> = HashMap::new();
        for w in &tx.writes {
            let (family, key, puts) = match w {
                Write::Put { family, key, .. } => (family.as_str(), key.as_slice(), true),
                Write::Delete { family, key } => (family.as_str(), key.as_slice(), false),
            };
            let before = match was.get(&(family, key)) {
                Some(b) => *b,
                None => self.latest(family, key)?.is_some_and(|(_, l)| l),
            };
            live = match (before, puts) {
                (false, true) => live.saturating_add(1),
                (true, false) => live.saturating_sub(1),
                _ => live,
            };
            was.insert((family, key), puts);
        }
        if live > u64::try_from(self.limits.rows).unwrap_or(u64::MAX) {
            return Ok(Err(Refusal::Full { limit: self.limits.rows }));
        }
        let rev = self.head.checked_add(1).ok_or_else(|| anyhow::anyhow!("the records store's revisions ran out"))?;
        for (n, w) in tx.writes.iter().enumerate() {
            let (family, key, value, live) = match w {
                Write::Put { family, key, value } => (family, key, blob(value)?, true),
                Write::Delete { family, key } => (family, key, Value::Null, false),
            };
            self.query(PUT, &[text(family), blob(key)?, num(rev)?, num(u64::try_from(n)?)?, value], |_| Ok(true))?;
            self.seen.insert((family.clone(), key.clone()), Some((rev, live)));
            changed.push((family.clone(), rev));
        }
        self.head = rev;
        self.live = live;
        Ok(Ok(rev))
    }

    /// The revision a read is at: none for the newest; refused when it's ahead of the head or
    /// from before what's kept.
    fn snapshot(&self, at: Consistency) -> Result<Option<Rev>, Refusal> {
        match at {
            Consistency::Linearizable => Ok(None),
            Consistency::Snapshot(r) if r > self.head => Err(Refusal::Ahead { asked: r, head: self.head }),
            Consistency::Snapshot(r) if r < self.horizon => Err(Refusal::Compacted { asked: r, horizon: self.horizon }),
            Consistency::Snapshot(r) => Ok(Some(r)),
        }
    }

    fn get(&mut self, family: &str, key: &[u8], at: Consistency) -> Result<Option<Row>, Refusal> {
        let at = self.snapshot(at)?;
        let mut out = None;
        let read = |r: &turso_core::Row| -> anyhow::Result<bool> {
            let rev = rev_of(r, 0)?;
            out = value_of(r, 1)?.map(|value| Row { key: key.to_vec(), value, rev });
            Ok(false)
        };
        let done = match at {
            None => self.query(GET_LATEST, &[text(family), blob(key).map_err(unavailable)?], read),
            Some(s) => self.query(GET_AT, &[text(family), blob(key).map_err(unavailable)?, num(s).map_err(unavailable)?], read),
        };
        done.map_err(unavailable)?;
        Ok(out)
    }

    fn scan(&mut self, family: &str, scan: &Scan) -> Result<Page, Refusal> {
        let limit = check_limit(scan.limit)?;
        let at = self.snapshot(scan.at)?;
        let (start, skip) = match &scan.after {
            Some(a) if a.as_slice() >= scan.prefix.as_slice() => (a.clone(), Some(a.as_slice())),
            _ => (scan.prefix.clone(), None),
        };
        let mut rows: Vec<Row> = Vec::new();
        let mut more = false;
        // The key being read, and its version at the read's revision (its value, none for a delete).
        let mut cur: Option<(Vec<u8>, Option<(Rev, Option<Vec<u8>>)>)> = None;
        let prefix = scan.prefix.as_slice();
        let mut each = |r: &turso_core::Row| -> anyhow::Result<bool> {
            let key = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("a key that isn't bytes"))?;
            if !key.starts_with(prefix) {
                return Ok(false);
            }
            if skip == Some(key) {
                return Ok(true);
            }
            let rev = rev_of(r, 1)?;
            let seen = at.is_none_or(|s| rev <= s);
            match &mut cur {
                Some((k, version)) if k.as_slice() == key => {
                    if seen {
                        *version = Some((rev, value_of(r, 2)?));
                    }
                }
                _ => {
                    if let Some((k, Some((r0, Some(v))))) = cur.take() {
                        if rows.len() >= limit {
                            more = true;
                            return Ok(false);
                        }
                        rows.push(Row { key: k, value: v, rev: r0 });
                    }
                    let version = if seen { Some((rev, value_of(r, 2)?)) } else { None };
                    cur = Some((key.to_vec(), version));
                }
            }
            Ok(true)
        };
        let read = match successor(prefix) {
            Some(end) => self.query(SCAN_TO, &[text(family), blob(&start).map_err(unavailable)?, blob(&end).map_err(unavailable)?], &mut each),
            None => self.query(SCAN_ON, &[text(family), blob(&start).map_err(unavailable)?], &mut each),
        };
        read.map_err(unavailable)?;
        if !more && let Some((k, Some((r0, Some(v))))) = cur {
            if rows.len() >= limit {
                more = true;
            } else {
                rows.push(Row { key: k, value: v, rev: r0 });
            }
        }
        Ok(Page { rows, more })
    }

    fn changes(&mut self, family: &str, after: Rev, limit: usize) -> Result<Changes, Refusal> {
        let limit = check_limit(limit)?;
        if after < self.feed {
            return Err(Refusal::Compacted { asked: after, horizon: self.feed });
        }
        let mut changes = Vec::new();
        let params = [text(family), num(after).map_err(unavailable)?, num(u64::try_from(limit).unwrap_or(u64::MAX)).map_err(unavailable)?];
        self.query(FEED, &params, |r| {
            let key = r.get_value(1).to_blob().ok_or_else(|| anyhow::anyhow!("a key that isn't bytes"))?.to_vec();
            changes.push(Change { rev: rev_of(r, 0)?, key, value: value_of(r, 2)? });
            Ok(true)
        })
        .map_err(unavailable)?;
        Ok(Changes { changes, head: self.head })
    }

    /// Starts forgetting the history from before `to` (at most the head): reads before it are
    /// refused from now on, and its versions go a step at a time ([`Db::compact_step`]).
    fn start_compaction(&mut self, to: Rev) {
        let to = to.min(self.head);
        if to <= self.compacted || self.compacting.is_some() {
            return;
        }
        let (horizon, feed) = (self.horizon.max(to), self.feed.max(to.saturating_sub(1)));
        let saved = self.exec("BEGIN").and_then(|()| self.set_meta("horizon", horizon)).and_then(|()| self.set_meta("feed", feed)).and_then(|()| self.exec("COMMIT"));
        if let Err(e) = saved {
            let _ = self.exec("ROLLBACK");
            tracing::warn!(target: "kiln.records", event = "compact_failed", error = %format!("{e:#}"), "couldn't start compacting the records");
            return;
        }
        (self.horizon, self.feed) = (horizon, feed);
        let mut families: Vec<String> = self.families.keys().cloned().collect();
        families.sort_unstable();
        self.compacting = Some(Compaction { to, from: self.compacted, families, at: 0, cursor: None, pruned: HashSet::new() });
    }

    /// One step of the compaction under way: up to [`Limits::step`] versions of a family's window
    /// looked at, and each key they're of pruned to the version a read at the compaction's
    /// revision finds (and those after it). The last step says it's done.
    fn compact_step(&mut self, stats: &Stats) {
        let Some(mut c) = self.compacting.take() else { return };
        let Some(family) = c.families.get(c.at).cloned() else {
            if let Err(e) = self.set_meta("compacted", c.to) {
                tracing::warn!(target: "kiln.records", event = "compact_failed", error = %format!("{e:#}"), "couldn't record a compaction of the records");
            }
            self.compacted = c.to;
            stats.compactions.fetch_add(1, Ordering::AcqRel);
            return;
        };
        let lo = c.cursor.unwrap_or_else(|| c.from.saturating_sub(1));
        let mut window: Vec<(Vec<u8>, Rev)> = Vec::new();
        let step = self.limits.step.max(TX_MAX.saturating_add(1));
        let fetched = num(lo).and_then(|lo| Ok([text(&family), lo, num(c.to)?, num(u64::try_from(step)?)?])).and_then(|params| {
            self.query(WINDOW, &params, |r| {
                let key = r.get_value(0).to_blob().ok_or_else(|| anyhow::anyhow!("a key that isn't bytes"))?.to_vec();
                window.push((key, rev_of(r, 1)?));
                Ok(true)
            })
        });
        let pruned = fetched.and_then(|()| {
            self.exec("BEGIN")?;
            for (key, _) in &window {
                if !c.pruned.contains(key) {
                    self.prune(&family, key, c.to)?;
                    c.pruned.insert(key.clone());
                }
            }
            self.exec("COMMIT")
        });
        if let Err(e) = pruned {
            let _ = self.exec("ROLLBACK");
            // Left for the next compaction, which starts from the last one finished.
            tracing::warn!(target: "kiln.records", event = "compact_failed", error = %format!("{e:#}"), "a step of compacting the records failed; it's left for the next");
            return;
        }
        if c.pruned.len() > PRUNED_MAX {
            c.pruned.clear();
        }
        // A full window may end part-way through a revision: that one is looked at again.
        match window.last().map(|(_, r)| *r) {
            Some(last) if window.len() >= step => c.cursor = Some(last.saturating_sub(1)),
            _ => {
                c.at = c.at.saturating_add(1);
                c.cursor = None;
                c.pruned.clear();
            }
        }
        self.compacting = Some(c);
    }

    /// Prunes `key` to the version a read at `to` finds, and those after it: older versions go,
    /// and so does that one when it's a delete from before `to` (from `to` on, the feed keeps
    /// every change).
    fn prune(&mut self, family: &str, key: &[u8], to: Rev) -> anyhow::Result<()> {
        let mut found = None;
        self.query(AT_DELETED, &[text(family), blob(key)?, num(to)?], |r| {
            found = Some((rev_of(r, 0)?, r.get::<i64>(1).map_err(fail)? != 0));
            Ok(false)
        })?;
        let Some((m, deleted)) = found else { return Ok(()) };
        self.query(PRUNE_BELOW, &[text(family), blob(key)?, num(m)?], |_| Ok(true))?;
        if deleted && m < to {
            self.query(PRUNE_AT, &[text(family), blob(key)?, num(m)?], |_| Ok(true))?;
        }
        Ok(())
    }

    /// Versions the file holds (tests).
    #[cfg(test)]
    fn versions(&mut self) -> anyhow::Result<u64> {
        let mut n = 0;
        self.query(COUNT, &[], |r| {
            n = u64::try_from(r.get::<i64>(0).map_err(fail)?)?;
            Ok(false)
        })?;
        Ok(n)
    }
}

/// Runs `stmt` with `params`, handing each row to `each` until it says no more.
fn run_while(io: &Arc<dyn IO>, stmt: &mut Statement, params: &[Value], mut each: impl FnMut(&turso_core::Row) -> anyhow::Result<bool>) -> anyhow::Result<()> {
    stmt.reset().map_err(fail)?;
    for (i, p) in params.iter().enumerate() {
        let at = NonZero::new(i.saturating_add(1)).ok_or_else(|| anyhow::anyhow!("SQL parameters count from 1"))?;
        stmt.bind_at(at, p.clone()).map_err(fail)?;
    }
    let mut busy = 0u32;
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
            StepResult::IO | StepResult::Yield => io.step().map_err(fail)?,
            StepResult::Sleep { duration } => {
                std::thread::sleep(duration);
                io.step().map_err(fail)?;
            }
            StepResult::Busy => {
                busy = busy.saturating_add(1);
                if busy > BUSY_TRIES {
                    let _ = stmt.reset();
                    anyhow::bail!("the records file stayed busy");
                }
                std::thread::sleep(BUSY_WAIT);
            }
            StepResult::Interrupt => {
                let _ = stmt.reset();
                anyhow::bail!("a statement on the records file was interrupted");
            }
        }
    }
}

fn fail(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn unavailable(e: anyhow::Error) -> Refusal {
    Refusal::Unavailable(format!("{e:#}"))
}

fn text(s: &str) -> Value {
    Value::from_text(s.to_owned())
}

fn blob(b: &[u8]) -> anyhow::Result<Value> {
    Value::from_slice(b).map_err(|e| anyhow::anyhow!("no room for a value of {} bytes: {e}", b.len()))
}

fn num(n: u64) -> anyhow::Result<Value> {
    Ok(Value::from_i64(i64::try_from(n)?))
}

fn rev_of(r: &turso_core::Row, i: usize) -> anyhow::Result<Rev> {
    Ok(u64::try_from(r.get::<i64>(i).map_err(fail)?)?)
}

/// A version's value: none for a delete.
fn value_of(r: &turso_core::Row, i: usize) -> anyhow::Result<Option<Vec<u8>>> {
    let v = r.get_value(i);
    if matches!(v, Value::Null) {
        return Ok(None);
    }
    v.to_blob().map(|b| Some(b.to_vec())).ok_or_else(|| anyhow::anyhow!("a value that isn't bytes"))
}

/// The first key after every key that starts with `prefix`: none when there's no such key (an
/// empty prefix, or one of 0xff bytes only).
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < u8::MAX {
            end.push(last.saturating_add(1));
            return Some(end);
        }
    }
    None
}

// What kiln-seams' simulator checks, checked alike: every implementation refuses the same things.

fn check_family(family: &str) -> Result<(), Refusal> {
    let ok = !family.is_empty() && family.len() <= FAMILY_MAX && family.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.'));
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a family name", why: format!("{family:?}: 1 to {FAMILY_MAX} of a-z, 0-9, _ - .") }) }
}

fn check_key(key: &[u8]) -> Result<(), Refusal> {
    if key.is_empty() {
        return Err(Refusal::Invalid { what: "a key", why: "it's empty".into() });
    }
    if key.len() > KEY_MAX {
        return Err(Refusal::TooLarge { what: "a key", size: key.len(), limit: KEY_MAX });
    }
    Ok(())
}

fn check_limit(limit: usize) -> Result<usize, Refusal> {
    if limit == 0 { Err(Refusal::Invalid { what: "a limit", why: "it's 0".into() }) } else { Ok(limit.min(SCAN_MAX)) }
}

fn check_tx(tx: &Tx) -> Result<(), Refusal> {
    if tx.conds.len() > TX_MAX {
        return Err(Refusal::TooLarge { what: "a transaction's conditions", size: tx.conds.len(), limit: TX_MAX });
    }
    if tx.writes.len() > TX_MAX {
        return Err(Refusal::TooLarge { what: "a transaction's writes", size: tx.writes.len(), limit: TX_MAX });
    }
    for c in &tx.conds {
        let (Cond::Absent { family, key } | Cond::At { family, key, .. }) = c;
        check_family(family)?;
        check_key(key)?;
    }
    for w in &tx.writes {
        let (Write::Put { family, key, .. } | Write::Delete { family, key }) = w;
        check_family(family)?;
        check_key(key)?;
        if let Write::Put { value, .. } = w
            && value.len() > VALUE_MAX
        {
            return Err(Refusal::TooLarge { what: "a value", size: value.len(), limit: VALUE_MAX });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::conformance::run;
    use kiln_seams::records::{RecordsFixture, cases};

    /// A store in a folder of its own, the folder removed with it; or, for a case no store was
    /// made for, one that refuses everything (the case fails, named).
    struct Fixture {
        records: Result<TursoRecords, Broken>,
        _dir: Option<tempfile::TempDir>,
    }

    impl RecordsFixture for Fixture {
        fn records(&self) -> &dyn Records {
            match &self.records {
                Ok(r) => r,
                Err(b) => b,
            }
        }

        fn compact(&self, before: Rev) -> BoxFuture<'_, ()> {
            match &self.records {
                Ok(r) => r.compact(before),
                Err(_) => Box::pin(async {}),
            }
        }
    }

    fn fixture() -> anyhow::Result<Fixture> {
        let dir = tempfile::tempdir()?;
        let records = TursoRecords::open(&dir.path().join("records.db"))?;
        Ok(Fixture { records: Ok(records), _dir: Some(dir) })
    }

    /// Records that refuse everything, saying why.
    struct Broken(String);

    impl Records for Broken {
        fn get<'a>(&'a self, _: &'a str, _: &'a [u8], _: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
        fn scan<'a>(&'a self, _: &'a str, _: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
        fn commit<'a>(&'a self, _: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
        fn changes<'a>(&'a self, _: &'a str, _: Rev, _: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
        fn wait<'a>(&'a self, _: &'a str, _: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
        fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
            Box::pin(async { Err(Refusal::Unavailable(self.0.clone())) })
        }
    }

    fn put(family: &str, key: &str, value: &str) -> Write {
        Write::Put { family: family.into(), key: key.as_bytes().to_vec(), value: value.as_bytes().to_vec() }
    }

    fn tx(writes: Vec<Write>) -> Tx {
        Tx { conds: Vec::new(), writes }
    }

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        futures::executor::block_on(f)
    }

    /// Versions the store's file holds, asked of its thread.
    fn versions(r: &TursoRecords) -> Result<u64, Refusal> {
        block(r.ask(Job::Count))
    }

    /// CP-T3.2 (kiln's records until Amphora's, CP-T1.5): the Turso store holds every clause of the
    /// `Records` contract, as kiln-seams' simulator does (the master plan §3: "Every trait has a
    /// simulator implementation and a conformance suite that both implementations pass").
    #[test]
    fn the_turso_store_passes_the_records_suite() -> anyhow::Result<()> {
        let all = cases::<Fixture>();
        // A store for each case, made first: one that can't be made fails the test here.
        let mut stores: std::collections::VecDeque<Fixture> = all.iter().map(|_| fixture()).collect::<anyhow::Result<_>>()?;
        let report = block(run("Records (kiln's Turso store)", &all, || {
            let f = stores.pop_front().unwrap_or_else(|| Fixture { records: Err(Broken("no store was made for this case".into())), _dir: None });
            async move { f }
        }));
        report.into_result().map(|n| assert_eq!(n, all.len())).map_err(|e| anyhow::anyhow!(e))
    }

    /// CP-T3.2: what a commit was answered is on disk: the file opened again has every committed
    /// row, the head, and each family's newest change.
    #[test]
    fn commits_are_there_when_the_file_is_opened_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("records.db");
        let (r1, r2) = {
            let r = TursoRecords::open(&path)?;
            let r1 = block(r.commit(tx(vec![put("inbox", "s1/m1", "one"), put("inbox", "s1/m2", "two")])))?;
            let r2 = block(r.commit(tx(vec![Write::Delete { family: "inbox".into(), key: b"s1/m1".to_vec() }, put("epochs", "s1", "3")])))?;
            (r1, r2)
        };
        let r = TursoRecords::open(&path)?;
        assert_eq!(block(r.head())?, r2);
        assert_eq!(block(r.get("inbox", b"s1/m1", Consistency::Linearizable))?, None);
        assert_eq!(block(r.get("inbox", b"s1/m2", Consistency::Linearizable))?.map(|x| (x.value, x.rev)), Some((b"two".to_vec(), r1)));
        assert_eq!(block(r.get("inbox", b"s1/m1", Consistency::Snapshot(r1)))?.map(|x| x.value), Some(b"one".to_vec()));
        assert_eq!(futures::FutureExt::now_or_never(r.wait("epochs", r1)), Some(Ok(r2)), "the family's newest change, known at once");
        let page = block(r.scan("inbox", Scan { prefix: b"s1/".to_vec(), after: None, limit: 10, at: Consistency::Linearizable }))?;
        assert_eq!(page.rows.iter().map(|x| x.key.as_slice()).collect::<Vec<_>>(), [b"s1/m2".as_slice()]);
        // The next commit goes on from the head; an empty value is a value, not a delete.
        assert_eq!(block(r.commit(tx(vec![put("inbox", "s1/m3", "")])))?, r2 + 1);
        assert_eq!(block(r.get("inbox", b"s1/m3", Consistency::Linearizable))?.map(|x| x.value), Some(Vec::new()));
        Ok(())
    }

    /// CP-T3.2: commits handed to the store together are written in one durable write (group
    /// commit), each still at a revision of its own, under its own conditions, in the order given:
    /// of 64 creates of one row, the first is committed and the rest refused `Conflict`.
    #[test]
    fn commits_given_together_share_one_write_and_keep_their_own_conditions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (start, held) = std::sync::mpsc::channel();
        let r = TursoRecords::open_with(&dir.path().join("records.db"), Limits::default(), Some(held))?;
        let create = |v: String| Tx { conds: vec![Cond::Absent { family: "inbox".into(), key: b"s/m".to_vec() }], writes: vec![put("inbox", "s/m", &v)] };
        let mut commits: Vec<_> = (0..64).map(|i| r.commit(create(format!("v{i}")))).collect();
        commits.extend((0..64).map(|i| r.commit(tx(vec![put("other", &format!("k{i}"), "x")]))));
        // Each is handed over (its first poll), and only then does the thread take any.
        let polled: Vec<_> = commits.into_iter().map(|mut c| (futures::FutureExt::now_or_never(&mut c), c)).collect();
        assert!(polled.iter().all(|(done, _)| done.is_none()), "nothing answered before the thread starts");
        start.send(())?;
        let answers: Vec<Result<Rev, Refusal>> = polled.into_iter().map(|(_, c)| block(c)).collect();
        assert_eq!(r.stats().writes.load(Ordering::Acquire), 1, "one durable write for all 128");
        assert_eq!(r.stats().commits.load(Ordering::Acquire), 128);
        let created: Vec<Rev> = answers.iter().take(64).filter_map(|a| a.as_ref().ok().copied()).collect();
        assert_eq!(created, [1], "the first create, at the first revision");
        assert!(answers.iter().skip(1).take(63).all(|a| matches!(a, Err(Refusal::Conflict { at: Some(1), .. }))), "{answers:?}");
        let others: Vec<Rev> = answers.iter().skip(64).filter_map(|a| a.as_ref().ok().copied()).collect();
        assert_eq!(others, (2..=65).collect::<Vec<Rev>>(), "a revision each, in the order given");
        assert_eq!(block(r.get("inbox", b"s/m", Consistency::Linearizable))?.map(|x| x.value), Some(b"v0".to_vec()));
        Ok(())
    }

    /// CP-T3.2: the history is bounded. Past [`Limits::keep`] and [`Limits::every`] revisions the
    /// store compacts on its own; once compacted to a revision, reads from before it are refused
    /// `Compacted`, those from it on read as they did, and each key keeps only the versions they
    /// need.
    #[test]
    fn the_history_is_compacted_and_reads_from_before_it_are_refused() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let limits = Limits { keep: 10, every: 5, step: 1_100, ..Limits::default() };
        let r = TursoRecords::open_with(&dir.path().join("records.db"), limits, None)?;
        for i in 0..20u32 {
            // Revision i + 1: s1 rewritten, and a row of its own written once.
            block(r.commit(tx(vec![put("journal", "s1", &format!("v{i}")), put("journal", &format!("once{i}"), "x")])))?;
        }
        // 15 revisions in, it compacted on its own (a step at a time while idle, so waited for).
        let deadline = Instant::now() + Duration::from_secs(30);
        while r.stats().compactions.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(r.stats().compactions.load(Ordering::Acquire) >= 1, "it compacted on its own");
        // Then to revision 10, whatever it did on its own.
        block(r.compact(10));
        assert!(matches!(block(r.get("journal", b"s1", Consistency::Snapshot(9))), Err(Refusal::Compacted { asked: 9, horizon: 10 })));
        assert!(matches!(block(r.changes("journal", 0, 10)), Err(Refusal::Compacted { .. })));
        assert_eq!(futures::FutureExt::now_or_never(r.wait("journal", 0)), Some(Err(Refusal::Compacted { asked: 0, horizon: 9 })));
        assert_eq!(block(r.get("journal", b"s1", Consistency::Snapshot(10)))?.map(|x| x.value), Some(b"v9".to_vec()));
        assert_eq!(block(r.get("journal", b"once0", Consistency::Snapshot(10)))?.map(|x| x.rev), Some(1), "a row written once keeps its one version");
        assert_eq!(block(r.changes("journal", 9, 1000))?.changes.len(), 22, "the feed is whole after its horizon: revisions 10 to 20");
        // s1's version at 10 and the 10 after it, and each once-row's one.
        assert_eq!(versions(&r)?, 11 + 20);
        Ok(())
    }

    /// CP-T3.2: past [`Limits::rows`] live rows a commit is refused `Full` and writes nothing; a
    /// delete makes room again.
    #[test]
    fn past_its_rows_a_commit_is_refused_full() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = TursoRecords::open_with(&dir.path().join("records.db"), Limits { rows: 2, ..Limits::default() }, None)?;
        block(r.commit(tx(vec![put("f", "a", "1"), put("f", "b", "1")])))?;
        assert_eq!(block(r.commit(tx(vec![put("f", "c", "1")]))), Err(Refusal::Full { limit: 2 }));
        assert_eq!(block(r.get("f", b"c", Consistency::Linearizable))?, None);
        // Rewriting a live row takes no room; a delete and a put together stay within it.
        block(r.commit(tx(vec![put("f", "a", "2")])))?;
        block(r.commit(tx(vec![Write::Delete { family: "f".into(), key: b"b".to_vec() }, put("f", "c", "1")])))?;
        Ok(())
    }

    /// CP-T3.2: keys are bytes: a prefix of 0xff bytes, and keys holding them, scan in order.
    #[test]
    fn prefixes_of_any_bytes_scan_in_byte_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let r = TursoRecords::open(&dir.path().join("records.db"))?;
        let w = |k: &[u8]| Write::Put { family: "f".into(), key: k.to_vec(), value: b"v".to_vec() };
        block(r.commit(tx(vec![w(&[0xff, 0x01]), w(&[0xff, 0xff]), w(&[0xff]), w(&[0xfe, 0xff]), w(&[0x00])])))?;
        let keys = |prefix: &[u8]| -> Result<Vec<Vec<u8>>, Refusal> {
            Ok(block(r.scan("f", Scan { prefix: prefix.to_vec(), after: None, limit: 10, at: Consistency::Linearizable }))?.rows.into_iter().map(|x| x.key).collect())
        };
        assert_eq!(keys(&[0xff])?, [vec![0xff], vec![0xff, 0x01], vec![0xff, 0xff]]);
        assert_eq!(keys(&[0xff, 0xff])?, [vec![0xff, 0xff]]);
        assert_eq!(keys(&[])?.len(), 5);
        assert_eq!(keys(&[0xfe])?, [vec![0xfe, 0xff]]);
        Ok(())
    }

    /// CP-T3.2: a commit is answered only once it's on disk: the store asks Turso for a full sync
    /// of each commit, past the drive's cache where the system has a way (F_FULLFSYNC on Apple's).
    #[test]
    fn commits_are_synced_in_full() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(&dir.path().join("records.db"), Limits::default())?;
        assert_eq!(db.conn.get_sync_mode(), SyncMode::Full);
        // Past the drive's cache on Apple's systems (elsewhere Turso's sync is already the full
        // one there is, and it keeps no other setting).
        #[cfg(target_vendor = "apple")]
        assert_eq!(db.conn.get_sync_type(), turso_core::io::FileSyncType::FullFsync, "past the drive's cache on Apple's systems too");
        Ok(())
    }
}

#[cfg(test)]
mod plans {
    use super::*;

    /// CP-T3.2 (the eighth and ninth adversarial checks'): each read is planned on the index it
    /// needs, so none costs what its family holds. A row's newest version, and its version at a
    /// revision, are sought by its key on the primary key, as a prune and a scan are, never found by
    /// walking its family's every version by revision (a take's cost would grow with every
    /// session's ids on the host, and their history); and a family's versions from a revision on
    /// are walked by revision.
    #[test]
    fn its_reads_are_planned_on_the_index_they_need() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut db = Db::open(&dir.path().join("r.db"), Limits::default())?;
        let stats = Stats::default();
        let writes: Vec<Write> = (0..4000).map(|i| Write::Put { family: "inbox".into(), key: format!("s{}/m{i}", i / 500).into_bytes(), value: vec![b'v'; 60] }).collect();
        for chunk in writes.chunks(500) {
            let answers = db.commit_batch(&[Tx { conds: Vec::new(), writes: chunk.to_vec() }], &stats);
            anyhow::ensure!(answers.iter().all(Result::is_ok), "{answers:?}");
        }
        let two = [text("inbox"), blob(b"s1/m700")?];
        let three = [text("inbox"), blob(b"s1/m700")?, Value::from_i64(5)];
        let lookups: [(&str, &'static str, &[Value]); 6] = [
            ("LATEST", LATEST, &two),
            ("GET_LATEST", GET_LATEST, &two),
            ("GET_AT", GET_AT, &three),
            ("AT_DELETED", AT_DELETED, &three),
            ("PRUNE_BELOW", PRUNE_BELOW, &three),
            ("PRUNE_AT", PRUNE_AT, &three),
        ];
        let plan_of = |db: &mut Db, sql: &str, params: &[Value]| -> anyhow::Result<Vec<String>> {
            let q: &'static str = Box::leak(format!("EXPLAIN QUERY PLAN {sql}").into_boxed_str());
            let mut plan = Vec::new();
            db.query(q, params, |r| {
                plan.push(r.get::<String>(3).map_err(fail)?);
                Ok(true)
            })?;
            Ok(plan)
        };
        for (name, sql, params) in lookups {
            let plan = plan_of(&mut db, sql, params)?;
            anyhow::ensure!(plan.iter().any(|p| p.contains("family=? AND key=?")) && !plan.iter().any(|p| p.contains("versions_by_rev")), "{name} isn't sought by its key: {plan:?}");
        }
        // A scan, to a key or to its family's end, is sought on the primary key from its first key
        // and read in its order, never sorted (the twelfth adversarial check's).
        let scan_to = [text("inbox"), blob(b"s1/")?, blob(b"s10")?];
        let scan_on = [text("inbox"), blob(b"s1/")?];
        for (name, sql, params) in [("SCAN_TO", SCAN_TO, &scan_to[..]), ("SCAN_ON", SCAN_ON, &scan_on[..])] {
            let plan = plan_of(&mut db, sql, params)?;
            anyhow::ensure!(
                plan.iter().any(|p| p.contains("family=? AND key>=?")) && !plan.iter().any(|p| p.contains("versions_by_rev") || p.contains("SORTER")),
                "{name} isn't sought on its keys: {plan:?}"
            );
        }
        // CP-T3.2 (the ninth adversarial check's): a family's versions from a revision on, in their
        // order (the change feed, a compaction's step), are walked by `versions_by_rev` from that
        // revision, never sorted from the family's every version.
        let feed = [text("inbox"), Value::from_i64(5), Value::from_i64(100)];
        let window = [text("inbox"), Value::from_i64(5), Value::from_i64(8), Value::from_i64(100)];
        for (name, sql, params) in [("FEED", FEED, &feed[..]), ("WINDOW", WINDOW, &window[..])] {
            let plan = plan_of(&mut db, sql, params)?;
            anyhow::ensure!(plan.iter().any(|p| p.contains("versions_by_rev (family=? AND rev>?")) && !plan.iter().any(|p| p.contains("SORTER")), "{name} isn't walked by revision: {plan:?}");
        }
        Ok(())
    }
}

