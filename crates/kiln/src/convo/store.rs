//! A machine's conversation store (`docs/conversations.md`, "The store"): one writer thread owns the
//! database; sessions' updates reach it by channel, never by lock, and are committed in batches, so
//! an update is durable within [`COMMIT_MS`] of its flush. Reads go through the same thread, in
//! turn with the writes.
//!
//! Every write gets the next number of the store (its revision), so another store can take what
//! came after a number it has ([`Store::changes`], [`Store::take`]): that is how a runner's store
//! reaches the server's (`super::feed`).
//!
//! The engine is behind [`Engine`]: Turso ([`super::turso`]).

use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::rows::{self, EventRow, Row};
use crate::harness::core::Update;
use crate::harness::session::Sink;

/// How long a row waits for others to share its commit.
pub const COMMIT_MS: u64 = 50;
/// Rows that make a commit at once, however soon.
pub const COMMIT_ROWS: usize = 2048;
/// Updates waiting for the writer, at most: one that can't keep up (a stuck disk) drops what
/// comes, saying so, rather than holding sessions back or growing without bound.
pub const QUEUE: usize = 65_536;
/// Longest a read waits for the writer.
pub const READ_WAIT: Duration = Duration::from_secs(10);
/// While the writer has nothing else to do, record lines kept before lines were compressed are
/// compressed this many at a time, this often.
const SQUEEZE_BATCH: usize = 2000;
const SQUEEZE_EVERY: Duration = Duration::from_millis(200);
/// This often, while the writer has nothing else to do, it looks after the file: the records of
/// sessions that ended long enough ago go, this many lines at a time (as often as compressing
/// goes); then the file is made compact if that and compressing left enough of it free.
const LOOK_EVERY: Duration = Duration::from_secs(3600);
const PRUNE_BATCH: usize = 2000;
/// Free pages that make compacting worth it (and a third of the file).
const COMPACT_MIN_FREE: u64 = 256 << 20;

/// What the store keeps of one session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// Each part as JSON, as the model serializes it.
    pub info: Option<String>,
    pub meta: Option<String>,
    pub state: Option<String>,
    pub changes: Option<String>,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    /// Its events, oldest first.
    pub events: Vec<EventRow>,
}

/// What to look for (`GET /api/conversations/search`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// Words the event's text holds (each one, any case); empty: any.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Most results one search returns.
pub const SEARCH_MAX: usize = 500;

/// A row as a feed sends it: its number in the store it is from, and its session's epoch there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub rev: u64,
    #[serde(default)]
    pub epoch: u64,
    pub row: Row,
}

/// What a feed reads at once: rows after a number, oldest first, and the number read through.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Changes {
    pub rows: Vec<Change>,
    pub upto: u64,
}

/// Where following one file of a session's record goes on: its last line kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub source: String,
    pub n: u64,
    pub end: u64,
    pub line: String,
}

/// A session the store has that hasn't ended: what it is, as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unended {
    pub id: String,
    pub info: String,
    pub meta: Option<String>,
    pub state: Option<String>,
    /// The machine whose feed brought it (None: it came another way, `info` says where).
    pub machine: Option<String>,
}

/// What a store did with a batch of a peer's feed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Took {
    /// Through where it has the peer's feed now.
    pub rev: u64,
    pub applied: usize,
    /// Rows of a session's former writer (a lower epoch than the one it has), left out.
    pub fenced: usize,
    /// The batch didn't follow what it had (`rev` says what it has): nothing was taken.
    pub gap: bool,
}

/// A database the store keeps conversations in.
pub trait Engine: Send + 'static {
    /// Who the store is to its peers: made once, kept in the database.
    fn id(&self) -> String;
    /// The rows, in one transaction, each with the next number.
    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()>;
    /// What is kept of `session` (None: nothing). `last`: at most its last this many events.
    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>>;
    /// A session's events before `before` (its event numbers), the newest `limit`, oldest first.
    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>>;
    /// Events matching `q`, newest first.
    fn search(&mut self, q: &Query) -> anyhow::Result<Vec<EventRow>>;
    /// The rows written after number `from`, as they are now, oldest first, about `max_bytes`.
    fn changes(&mut self, from: u64, max_bytes: usize) -> anyhow::Result<Changes>;
    /// Take a batch of `peer`'s feed, which follows number `from` of it, in one transaction:
    /// numbered here as this store's own writes, a session's rows from its latest writer alone.
    /// `machine`: the machine of the feed, where the sessions it brings run (None: its rows say,
    /// a mirror's feed of the server's store).
    fn take(&mut self, peer: &str, machine: Option<&str>, from: u64, batch: &Changes) -> anyhow::Result<Took>;
    /// Through where this store has taken `peer`'s feed (0: none of it).
    fn peer(&mut self, peer: &str) -> anyhow::Result<u64>;
    /// Each file of `session`'s record kept, with its last line.
    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>>;
    /// `limit` lines of `session`'s record from `source`, from line `from` on: `(n, line)`.
    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>>;
    /// The sessions not ended, heard of since `since` (unix ms), oldest first.
    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>>;
    /// Compress the next `limit` record lines kept before lines were compressed, if any are left:
    /// whether more are.
    fn squeeze_some(&mut self, limit: usize) -> anyhow::Result<bool>;
    /// Let go of up to `limit` record lines of sessions that ended before `before` (unix ms):
    /// how many went (0: none are left). Their events stay.
    fn prune_some(&mut self, before: i64, limit: usize) -> anyhow::Result<usize>;
    /// Make the database's file compact when compressing and letting go left `min_free` bytes of
    /// it free, and a third of it: the engine to go on with (this one, or one on the compact
    /// file), and the file's size before and after when it was made compact. An error: the
    /// engine is gone (the compact file couldn't be opened; the next start finishes it).
    fn compact(self: Box<Self>, min_free: u64) -> anyhow::Result<(Box<dyn Engine>, Option<(u64, u64)>)>;
}

type Job = Box<dyn FnOnce(&mut dyn Engine) + Send>;

enum Msg {
    Rows(Vec<Row>),
    /// Run on the engine, between commits (whatever was sent before it is committed first).
    Run(Job),
}

/// Who wants to know that something was committed (a feed with nothing left to send waits on it).
type Watchers = Arc<Mutex<Vec<async_channel::Sender<()>>>>;

fn notify(watchers: &Watchers) {
    watchers.lock().unwrap_or_else(PoisonError::into_inner).retain(|w| !matches!(w.try_send(()), Err(async_channel::TrySendError::Closed(_))));
}

/// A handle to a machine's store: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Store {
    tx: mpsc::SyncSender<Msg>,
    id: Arc<str>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    watchers: Watchers,
}

impl Store {
    /// The store, its writer thread started on `engine`. `keep_closed`: how long the records of
    /// sessions that ended are kept (None: for good).
    pub fn start(engine: Box<dyn Engine>, keep_closed: Option<Duration>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE);
        let id: Arc<str> = engine.id().into();
        let watchers: Watchers = Arc::default();
        let w = watchers.clone();
        std::thread::Builder::new().name("kiln-convo".into()).spawn(move || writer(engine, &rx, &w, keep_closed))?;
        Ok(Self { tx, id, dropped: Arc::default(), watchers })
    }

    /// Who this store is to its peers.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What `update` of `session` makes, to be committed with the next batch.
    pub fn send(&self, session: &str, update: &Update) {
        self.put(rows::of(session, update, crate::util::now_ms()));
    }

    /// Rows to be committed with the next batch. Never blocks: when the writer can't keep up,
    /// they're dropped and counted.
    pub fn put(&self, rows: Vec<Row>) {
        if rows.is_empty() {
            return;
        }
        if let Err(mpsc::TrySendError::Full(_)) = self.tx.try_send(Msg::Rows(rows)) {
            let n = self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n.is_power_of_two() {
                tracing::warn!(dropped = n + 1, "conversation store: the writer can't keep up; updates were dropped");
            }
        }
    }

    /// Rows to be committed with the next batch, waiting up to `READ_WAIT` for room: false when
    /// they couldn't be handed over (whoever sends them sends them again). Blocking.
    pub fn put_wait(&self, rows: Vec<Row>) -> bool {
        rows.is_empty() || self.tx.send_wait(Msg::Rows(rows)).is_ok()
    }

    /// Run `f` on the engine once what was sent before is committed. Blocking, for `READ_WAIT` at
    /// most.
    fn run<T: Send + 'static>(&self, f: impl FnOnce(&mut dyn Engine) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send_wait(Msg::Run(Box::new(move |e| {
                let _ = rtx.send(f(e));
            })))
            .map_err(|()| anyhow::anyhow!("the conversation store has stopped, or is full"))?;
        rrx.recv_timeout(READ_WAIT).map_err(|_| anyhow::anyhow!("the conversation store didn't answer in time"))?
    }

    /// What is kept of `session`, with at most its last `last` events. Blocking.
    pub fn kept(&self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let s = session.to_owned();
        self.run(move |e| e.kept(&s, last))
    }

    /// A session's events before event number `before`. Blocking.
    pub fn events(&self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let s = session.to_owned();
        self.run(move |e| e.events(&s, before, limit))
    }

    /// Events matching `q`, newest first. Blocking.
    pub fn search(&self, q: Query) -> anyhow::Result<Vec<EventRow>> {
        self.run(move |e| e.search(&q))
    }

    /// The rows written after number `from`, about `max_bytes` of them. Blocking.
    pub fn changes(&self, from: u64, max_bytes: usize) -> anyhow::Result<Changes> {
        self.run(move |e| e.changes(from, max_bytes))
    }

    /// Take a batch of `peer`'s feed (it follows `from`; `machine` as for [`Engine::take`]).
    /// Blocking.
    pub fn take(&self, peer: &str, machine: Option<&str>, from: u64, batch: Changes) -> anyhow::Result<Took> {
        let (p, m) = (peer.to_owned(), machine.map(str::to_owned));
        let w = self.watchers.clone();
        self.run(move |e| {
            let took = e.take(&p, m.as_deref(), from, &batch)?;
            if took.applied > 0 {
                notify(&w);
            }
            Ok(took)
        })
    }

    /// Through where this store has `peer`'s feed. Blocking.
    pub fn peer(&self, peer: &str) -> anyhow::Result<u64> {
        let p = peer.to_owned();
        self.run(move |e| e.peer(&p))
    }

    /// Each file of `session`'s record kept, with its last line. Blocking.
    pub fn marks(&self, session: &str) -> anyhow::Result<Vec<Mark>> {
        let s = session.to_owned();
        self.run(move |e| e.marks(&s))
    }

    /// Lines of `session`'s record from `source`, from line `from` on. Blocking.
    pub fn record(&self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>> {
        let (s, src) = (session.to_owned(), source.to_owned());
        self.run(move |e| e.record(&s, &src, from, limit))
    }

    /// The sessions not ended, heard of since `since` (unix ms). Blocking.
    pub fn unended(&self, since: i64) -> anyhow::Result<Vec<Unended>> {
        self.run(move |e| e.unended(since))
    }

    /// Wait until everything sent so far is committed. Blocking.
    pub fn sync(&self) -> anyhow::Result<()> {
        self.run(|_| Ok(()))
    }

    /// Woken (once, however many commits came) after a commit: what a feed with nothing left to
    /// send waits on.
    pub fn watch(&self) -> async_channel::Receiver<()> {
        let (tx, rx) = async_channel::bounded(1);
        self.watchers.lock().unwrap_or_else(PoisonError::into_inner).push(tx);
        rx
    }
}

/// Sending that waits for room, up to `READ_WAIT` (a full queue means a writer behind).
trait SendWait {
    fn send_wait(&self, m: Msg) -> Result<(), ()>;
}

impl SendWait for mpsc::SyncSender<Msg> {
    fn send_wait(&self, mut m: Msg) -> Result<(), ()> {
        let until = Instant::now() + READ_WAIT;
        loop {
            match self.try_send(m) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(()),
                Err(mpsc::TrySendError::Full(back)) => {
                    if Instant::now() >= until {
                        return Err(());
                    }
                    m = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

/// The writer: rows wait at most `COMMIT_MS` (or until `COMMIT_ROWS`) to share a commit; a job
/// commits what waits first, so it sees everything sent before it.
fn writer(mut engine: Box<dyn Engine>, rx: &mpsc::Receiver<Msg>, watchers: &Watchers, keep_closed: Option<Duration>) {
    let mut pending: Vec<Row> = Vec::new();
    let mut since: Option<Instant> = None;
    // Lines kept before lines were compressed may be left: compressed while nothing else waits.
    let mut squeezing = true;
    // Looking after the file while nothing else waits, once compressing is done and then every
    // `LOOK_EVERY`: the records of sessions that ended longer than `keep_closed` ago go, then the
    // file is made compact if enough of it is free.
    let (mut looking, mut pruned, mut look_at) = (false, 0usize, Instant::now());
    let commit = |engine: &mut Box<dyn Engine>, pending: &mut Vec<Row>| {
        if pending.is_empty() {
            return;
        }
        let rows = rows::coalesce(std::mem::take(pending));
        match engine.apply(&rows) {
            Ok(()) => notify(watchers),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "conversation store: a batch couldn't be written"),
        }
    };
    loop {
        let idle = if squeezing || looking { SQUEEZE_EVERY } else { look_at.saturating_duration_since(Instant::now()).max(SQUEEZE_EVERY) };
        let wait = since.map_or(idle, |t| Duration::from_millis(COMMIT_MS).saturating_sub(t.elapsed()));
        match rx.recv_timeout(wait) {
            Ok(Msg::Rows(rows)) => {
                pending.extend(rows);
                since.get_or_insert_with(Instant::now);
                if pending.len() >= COMMIT_ROWS {
                    commit(&mut engine, &mut pending);
                    since = None;
                }
            }
            Ok(Msg::Run(f)) => {
                commit(&mut engine, &mut pending);
                since = None;
                f(engine.as_mut());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let had = !pending.is_empty();
                commit(&mut engine, &mut pending);
                since = None;
                if squeezing && !had {
                    match engine.squeeze_some(SQUEEZE_BATCH) {
                        Ok(true) => {}
                        Ok(false) => squeezing = false,
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "conversation store: older record lines couldn't be compressed");
                            squeezing = false;
                        }
                    }
                } else if !had && (looking || Instant::now() >= look_at) {
                    let gone = match keep_closed {
                        Some(keep) => engine.prune_some(crate::util::now_ms().saturating_sub(i64::try_from(keep.as_millis()).unwrap_or(i64::MAX)), PRUNE_BATCH),
                        None => Ok(0),
                    };
                    match gone {
                        Ok(0) => {
                            if pruned > 0 {
                                tracing::info!(lines = pruned, days = keep_closed.map_or(0, |k| k.as_secs() / 86_400), "conversation store: the records of sessions that ended long ago were let go");
                            }
                            (looking, pruned, look_at) = (false, 0, Instant::now() + LOOK_EVERY);
                            engine = match engine.compact(COMPACT_MIN_FREE) {
                                Ok((e, Some((before, after)))) => {
                                    tracing::info!(before, after, "conversation store: its file was made compact");
                                    e
                                }
                                Ok((e, None)) => e,
                                Err(e) => {
                                    tracing::error!(error = %format!("{e:#}"), "conversation store: the compact file couldn't be opened; the store stops until kiln starts again, which finishes it");
                                    return;
                                }
                            };
                        }
                        Ok(n) => (looking, pruned) = (true, pruned.saturating_add(n)),
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "conversation store: the records of sessions that ended long ago couldn't be let go");
                            (looking, pruned, look_at) = (false, 0, Instant::now() + LOOK_EVERY);
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                commit(&mut engine, &mut pending);
                return;
            }
        }
    }
}

/// A sink that keeps what passes through it in `store` on its way to `sink`, in order, with the
/// same room (a full `sink` holds the sessions back, as it always did).
pub fn tee(sink: Sink, store: Store, room: usize) -> Sink {
    let (tx, rx) = async_channel::bounded::<(String, Update)>(room);
    crate::rt::spawn(async move {
        while let Ok((id, u)) = rx.recv().await {
            store.send(&id, &u);
            if sink.send((id, u)).await.is_err() {
                break;
            }
        }
    });
    tx
}

/// A store on a new database in a temporary folder, for tests.
#[cfg(test)]
pub fn scratch() -> anyhow::Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::start(Box::new(super::turso::Turso::open(&dir.path().join(super::FILE))?), None)?;
    Ok((dir, store))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Body, Event, State};

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    #[test]
    fn the_writer_lets_go_of_the_records_of_sessions_that_ended_long_ago_while_idle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::start(Box::new(super::super::turso::Turso::open(&dir.path().join(super::super::FILE))?), Some(Duration::from_secs(60)))?;
        let now = crate::util::now_ms();
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: now };
        let line = |s: &str, n: u64| Row::Record(Box::new(rows::RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: "{}".into() }));
        store.put(vec![session("old"), line("old", 0), line("old", 1), Row::Closed { session: "old".into(), at: now - 3_600_000 }]);
        store.put(vec![session("new"), line("new", 0), Row::Closed { session: "new".into(), at: now }]);
        // Asked seldom: each question is the writer's work too, and it lets go only while idle.
        let until = Instant::now() + Duration::from_secs(15);
        while !store.marks("old")?.is_empty() {
            anyhow::ensure!(Instant::now() < until, "the record of a session that ended an hour ago was kept");
            std::thread::sleep(SQUEEZE_EVERY * 3);
        }
        assert_eq!(store.marks("new")?.len(), 1, "a session that ended just now keeps its record");
        Ok(())
    }

    #[test]
    fn what_is_sent_is_kept_and_read_back_in_order() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        store.send("a1", &Update::State { state: Box::new(State { title: "parser".into(), ..State::default() }) });
        for i in 1..=50u64 {
            store.send("a1", &said(&format!("m{i}"), i, &format!("reply {i}")));
        }
        // An event changed later is kept as it is now.
        store.send("a1", &said("m3", 3, "reply three, revised"));
        let k = store.kept("a1", 10)?.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
        assert!(k.state.as_deref().is_some_and(|s| s.contains("parser")));
        assert_eq!(k.events.len(), 10);
        assert_eq!(k.events.first().map(|e| e.id.as_str()), Some("m41"));
        let older = store.events("a1", Some(41), 5)?;
        assert_eq!(older.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![36, 37, 38, 39, 40]);
        let all = store.events("a1", None, 100)?;
        assert!(all.iter().any(|e| e.id == "m3" && e.text == "reply three, revised"));
        Ok(())
    }

    #[test]
    fn a_search_finds_words_newest_first() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        store.send("a1", &said("m1", 1, "the parser fails on tabs"));
        store.send("a2", &said("m5", 5, "Parser fixed"));
        store.send("a2", &said("m6", 6, "unrelated"));
        let hits = store.search(Query { q: "parser".into(), ..Query::default() })?;
        // Any case; the newer first.
        assert_eq!(hits.iter().map(|e| e.session.as_str()).collect::<Vec<_>>(), vec!["a2", "a1"]);
        let only = store.search(Query { q: "parser tabs".into(), ..Query::default() })?;
        assert_eq!(only.len(), 1);
        Ok(())
    }

    #[test]
    fn a_reset_drops_the_events_from_its_number_on_and_keeps_the_history_before() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        for i in 1..=6u64 {
            store.send("a1", &said(&format!("m{i}"), i, "x"));
        }
        // A restart: the timeline in memory held 4.. (the store holds the rest); it says so again.
        store.send("a1", &Update::Reset { from: Some(4) });
        for i in 4..=6u64 {
            store.send("a1", &said(&format!("m{i}"), i, "x"));
        }
        let ids = |k: Option<Kept>| k.map(|k| k.events.into_iter().map(|e| e.id).collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(ids(store.kept("a1", 100)?), vec!["m1", "m2", "m3", "m4", "m5", "m6"]);
        // A rewind to m5: from 4 again, but only m4 comes back.
        store.send("a1", &Update::Reset { from: Some(4) });
        store.send("a1", &said("m4", 4, "x"));
        assert_eq!(ids(store.kept("a1", 100)?), vec!["m1", "m2", "m3", "m4"]);
        // From the start: all of it goes.
        store.send("a1", &Update::Reset { from: None });
        store.send("a1", &said("n1", 1, "x"));
        assert_eq!(ids(store.kept("a1", 100)?), vec!["n1"]);
        Ok(())
    }

    #[test]
    fn the_tee_keeps_and_passes_on_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_dir, store) = scratch()?;
            let (sink, out) = async_channel::bounded(4);
            let tee = tee(sink, store.clone(), 4);
            for i in 1..=3u64 {
                tee.send(("a1".to_owned(), said(&format!("m{i}"), i, "x"))).await?;
            }
            for i in 1..=3u64 {
                let (id, u) = out.recv().await?;
                assert_eq!(id, "a1");
                assert!(matches!(u, Update::Events { events } if events.first().map(|e| e.seq) == Some(i)));
            }
            store.sync()?;
            assert_eq!(store.kept("a1", 10)?.map(|k| k.events.len()), Some(3));
            Ok(())
        })
    }
}
