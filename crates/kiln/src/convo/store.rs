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
//!
//! What waits for the writer is bounded in bytes ([`Queue`]; goal 11, plan 01a1a87b2e56 step 3): the bytes
//! the writer commits in [`COMMIT_MS`] at the rate it has committed, so a row sent is kept within about
//! that, as this store says of its updates. Past the bound whoever sends rows waits for the writer
//! ([`Store::room`], [`Store::put_wait`]): nothing is dropped (conversations are durable state: nothing
//! about a session is said before it's kept, plan 01a17197).

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::rows::{self, EventRow, Row};
use super::telemetry::{CompactView, Compacted, Meter, Pruned, StoreReport, Upkeeping};
use crate::harness::core::Update;
use crate::harness::session::Sink;

/// How long a row waits for others to share its commit.
pub const COMMIT_MS: u64 = 50;
/// Rows that make a commit at once, however soon.
pub const COMMIT_ROWS: usize = 2048;
/// Longest a read waits for the writer.
pub const READ_WAIT: Duration = Duration::from_secs(10);
/// The most sessions [`Store::ended`] answers a page.
pub const ENDED_MAX: usize = 1000;
/// While the writer has nothing else to do, record lines kept before lines were compressed are
/// compressed this many at a time, this often.
const SQUEEZE_BATCH: usize = 2000;
const SQUEEZE_EVERY: Duration = Duration::from_millis(200);
/// This often, while the writer has nothing else to do, it looks after the file: the records of
/// sessions that ended long enough ago go, this many lines at a time (as often as compressing
/// goes); then the file is made compact if enough of it is free ([`Engine::compact_some`]), a
/// step at a time.
const LOOK_EVERY: Duration = Duration::from_secs(3600);
const PRUNE_BATCH: usize = 2000;
/// Compacting is worth it once this much of the file is free, and a third of it.
pub const COMPACT_MIN_FREE: u64 = 256 << 20;
/// Each step of compacting copies about this much, as often as compressing goes: the memory it
/// takes, and about how long the writer is held (a copy of the whole file in one go took more
/// memory than the first server had, which killed it over and over, 2026-10-18).
const COMPACT_STEP: usize = 4 << 20;
/// This often the writer looks at its file for its telemetry ([`Engine::look`]), whatever else it
/// does; its sessions it counts every `COUNT_EVERY`.
const LOOK_FILE: Duration = Duration::from_secs(2);
const COUNT_EVERY: Duration = Duration::from_secs(30);

/// How a store looks after its file while it has nothing else to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Upkeep {
    /// How long the records of sessions that ended are kept (None: for good).
    pub keep_closed: Option<Duration>,
    /// Make the file compact once this many bytes of it are free, and a third of it (None: never).
    pub compact: Option<u64>,
}

/// What a step of compacting did ([`Engine::compact_some`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compacting {
    /// Nothing more to do: not worth it, no room for the copy, or it stopped (and said why).
    Not,
    /// A step was taken; more follow.
    Going,
    /// The compact copy took the file's place: its size before and after.
    Done { before: u64, after: u64 },
}

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
/// How long one go of a search may hold the store, about: the events it looks at in a go grow
/// or shrink to it (from `SEARCH_SLICE_FIRST`, between `SEARCH_SLICE_MIN` and `SEARCH_SLICE`), as
/// the disk is quick or not (one agent's events, spread through the file, are slower than the
/// newest of every agent's), and the store does what else waits between them.
pub const SEARCH_SLICE_TIME: Duration = Duration::from_millis(50);
pub const SEARCH_SLICE_FIRST: usize = 2_000;
pub const SEARCH_SLICE_MIN: usize = 250;
pub const SEARCH_SLICE: usize = 20_000;
/// How long one search goes on, at most: past that it answers with the newest matches it found
/// (it looks newest first).
pub const SEARCH_WAIT: Duration = Duration::from_secs(8);

/// Where a search goes on from: the order key of the last event it looked at (its time; its
/// number, in one agent's) and its rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchFrom {
    pub key: i64,
    pub rowid: i64,
}

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
    /// Rows of a copy ([`Engine::take_copy`]) of sessions this store has from elsewhere, or of
    /// what the copy dropped, left out.
    pub left: usize,
    /// Events and record lines it had already as they came (sent again, from another store of
    /// the same machine): nothing written, no number given.
    pub same: usize,
    /// The batch didn't follow what it had (`rev` says what it has): nothing was taken.
    pub gap: bool,
    /// This store's own numbers the batch was written under, first and last (None: it wrote
    /// nothing).
    pub wrote: Option<(u64, u64)>,
}

/// Where a store's file stands ([`Engine::look`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Look {
    /// The file's size, the part of it free for what comes next, and its log's size.
    pub bytes: u64,
    pub free: u64,
    pub wal: u64,
    /// The last number the store gave a write.
    pub top: u64,
    /// Its sessions, when asked to count them.
    pub sessions: Option<u64>,
    /// How far making it compact has got (None: it isn't being made compact).
    pub compacting: Option<CompactView>,
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
    /// Events matching `q`, newest first: up to `want` of them among the next `rows` events it
    /// looks at after `from` (None: from the newest), and where to go on from (None: it has looked
    /// at every one).
    fn search_some(&mut self, q: &Query, from: Option<SearchFrom>, rows: usize, want: usize) -> anyhow::Result<(Vec<EventRow>, Option<SearchFrom>)>;
    /// The rows written after number `from`, as they are now, oldest first, about `max_bytes`.
    /// `kept`: record lines as they are kept ([`RecordRow::kept`](super::rows::RecordRow::kept)),
    /// for a taker that takes them so; else as text.
    fn changes(&mut self, from: u64, max_bytes: usize, kept: bool) -> anyhow::Result<Changes>;
    /// Take a batch of `peer`'s feed, which follows number `from` of it, in one transaction:
    /// numbered here as this store's own writes, a session's rows from its latest writer alone.
    /// `machine`: the machine of the feed, where the sessions it brings run (None: its rows say,
    /// a mirror's feed of the server's store).
    fn take(&mut self, peer: &str, machine: Option<&str>, from: u64, batch: &Changes) -> anyhow::Result<Took>;
    /// Through where this store has taken `peer`'s feed (0: none of it).
    fn peer(&mut self, peer: &str) -> anyhow::Result<u64>;
    /// Every peer whose feed it took: its name, through where, and when it last took from it
    /// (unix ms).
    fn peers(&mut self) -> anyhow::Result<Vec<(String, u64, i64)>>;
    /// Each file of `session`'s record kept, with its last line.
    fn marks(&mut self, session: &str) -> anyhow::Result<Vec<Mark>>;
    /// `limit` lines of `session`'s record from `source`, from line `from` on: `(n, line)`.
    fn record(&mut self, session: &str, source: &str, from: u64, limit: usize) -> anyhow::Result<Vec<(u64, String)>>;
    /// The sessions not ended, heard of since `since` (unix ms), oldest first.
    fn unended(&mut self, since: i64) -> anyhow::Result<Vec<Unended>>;
    /// The sessions that ended after `after` (`(closed_at, id)` together), oldest first, `limit` at
    /// most: `(id, closed_at)`.
    fn ended(&mut self, after: (i64, &str), limit: usize) -> anyhow::Result<Vec<(String, i64)>>;
    /// Compress the next `limit` record lines kept before lines were compressed, if any are left:
    /// whether more are.
    fn squeeze_some(&mut self, limit: usize) -> anyhow::Result<bool>;
    /// Take a batch of a mirror's copy of this store, sent back to restore it (it follows number
    /// `from` of the copy): the sessions it brings that this store has nothing of come back as
    /// they were, where their rows say they run; a session this store has anything of from
    /// elsewhere (its machine's own feed) keeps what it has, and takes the session over if it
    /// comes while the copy is being taken.
    fn take_copy(&mut self, peer: &str, from: u64, batch: &Changes) -> anyhow::Result<Took>;
    /// A copy has been taken whole: how many sessions it brought back.
    fn end_copy(&mut self) -> anyhow::Result<usize>;
    /// Whether the store holds no session at all (made just now, or lost and made again).
    fn is_empty(&mut self) -> anyhow::Result<bool>;
    /// The last number the store gave a write.
    fn top(&mut self) -> u64;
    /// Let go of up to `limit` record lines of sessions that ended before `before` (unix ms):
    /// how many went (0: none are left). Their events stay.
    fn prune_some(&mut self, before: i64, limit: usize) -> anyhow::Result<usize>;
    /// A step of making the database's file compact, once compressing and letting go left
    /// `min_free` bytes of it free, and a third of it: a copy, `step_bytes` at a time, under the
    /// same numbers, then what was written meanwhile, then the copy takes the file's place. The
    /// engine to go on with (this one, or one on the compact file once it took its place). An
    /// error: the engine is gone (the compact file couldn't be opened; the next start finishes
    /// putting it in place).
    fn compact_some(self: Box<Self>, min_free: u64, step_bytes: usize) -> anyhow::Result<(Box<dyn Engine>, Compacting)>;
    /// Where its file stands, for its telemetry: cheap (the file's header, its log's size), as
    /// the writer looks every few seconds. `count`: its sessions counted too.
    fn look(&mut self, count: bool) -> anyhow::Result<Look>;
    /// Ready to be let go ([`Store::close`]): everything written goes into the file itself, its
    /// log emptied, for another process to open it next; a compaction under way stops, its copy
    /// gone (it would stay behind as the file moves).
    fn release(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

type Job = Box<dyn FnOnce(&mut dyn Engine) + Send>;

enum Msg {
    /// Rows, and the bytes [`Queue`] counts them at.
    Rows(Vec<Row>, usize),
    /// Run on the engine, between commits (whatever was sent before it is committed first).
    Run(Job),
    /// Commit what was sent before, let go of the file ([`Engine::release`]) and stop.
    Close(mpsc::Sender<anyhow::Result<()>>),
}

/// Who wants to know that something was committed (a feed with nothing left to send waits on it).
type Watchers = Arc<Mutex<Vec<async_channel::Sender<()>>>>;

fn notify(watchers: &Watchers) {
    watchers.lock().unwrap_or_else(PoisonError::into_inner).retain(|w| !matches!(w.try_send(()), Err(async_channel::TrySendError::Closed(_))));
}

/// What waits for the writer, in bytes, and the most it may hold (goal 11, plan 01a1a87b2e56 step 3).
/// Lockless: atomics, and an event the writer tells as it commits.
#[derive(Default)]
pub struct Queue {
    /// Bytes of rows sent and not yet committed (`Row::bytes`).
    bytes: AtomicUsize,
    /// Bytes committed, and the microseconds their commits took: the writer's measured rate.
    written: AtomicU64,
    took_us: AtomicU64,
    /// Told each time the writer commits (`crate::rt::Changed`, no lock; goal 5, plan 01a1a59c2aae, its top goal: it
    /// was an event-listener `Event`, its list behind a `Mutex`).
    room: crate::rt::Changed,
    /// The most any sender may let wait past [`Queue::bound`] before it waits itself ([`Store::room_within`]),
    /// for what `/metrics` says the queue may hold.
    within: AtomicUsize,
}

impl Queue {
    /// The most that may wait: what the writer commits in [`COMMIT_MS`] (the most a row waits to share a
    /// commit) at the rate it has committed so far, so what's sent is kept within about that. None measured
    /// yet: nothing past what's sent while the queue is empty.
    pub fn bound(&self) -> usize {
        let (written, took) = (self.written.load(Ordering::Relaxed), self.took_us.load(Ordering::Relaxed));
        if took == 0 {
            return 0;
        }
        let wait_us = u128::from(COMMIT_MS).saturating_mul(1000);
        usize::try_from(u128::from(written).saturating_mul(wait_us) / u128::from(took)).unwrap_or(usize::MAX)
    }

    /// Bytes waiting now.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Whether more may be sent: the queue is empty, or under its bound.
    fn has_room(&self) -> bool {
        self.has_room_within(0)
    }

    /// Whether more may be sent by one that may let `most` wait: the queue is empty, or under its bound or `most`.
    fn has_room_within(&self, most: usize) -> bool {
        let b = self.bytes();
        b == 0 || b <= self.bound().max(most)
    }

    fn took(&self, bytes: usize) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        crate::memory::CONVO_QUEUE.add(bytes);
    }

    /// `bytes` counted as waiting for the writer, for a test of what waits on the queue.
    #[cfg(test)]
    pub(crate) fn pretend_waiting(&self, bytes: usize) {
        self.took(bytes);
    }

    /// `bytes` the writer let go of, for a test of what waits on the queue.
    #[cfg(test)]
    pub(crate) fn pretend_written(&self, bytes: usize) {
        self.let_go(bytes, None);
    }

    /// `bytes` committed in `took` (or let go of, the writer gone): their room is the senders' again.
    fn let_go(&self, bytes: usize, took: Option<Duration>) {
        if let Some(d) = took {
            self.written.fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
            self.took_us.fetch_add(u64::try_from(d.as_micros()).unwrap_or(u64::MAX).max(1), Ordering::Relaxed);
        }
        let mut now = self.bytes.load(Ordering::Relaxed);
        while let Err(seen) = self.bytes.compare_exchange_weak(now, now.saturating_sub(bytes), Ordering::Relaxed, Ordering::Relaxed) {
            now = seen;
        }
        crate::memory::CONVO_QUEUE.sub(bytes);
        crate::memory::CONVO_QUEUE.bound_at(self.bound().max(self.within.load(Ordering::Relaxed)));
        self.room.notify();
    }
}

/// A handle to a machine's store: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Msg>,
    queue: Arc<Queue>,
    id: Arc<str>,
    meter: Arc<Meter>,
    watchers: Watchers,
    /// Sessions that ended at or after this (unix ms) keep their records ([`Store::hold_prune`]).
    hold: Arc<AtomicI64>,
}

impl Store {
    /// The store, its writer thread started on `engine`, looking after its file as `upkeep` says.
    pub fn start(engine: Box<dyn Engine>, upkeep: Upkeep) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let id: Arc<str> = engine.id().into();
        let watchers: Watchers = Arc::default();
        let meter: Arc<Meter> = Arc::default();
        let hold = Arc::new(AtomicI64::new(i64::MAX));
        let queue: Arc<Queue> = Arc::default();
        let (w, m, h, q) = (watchers.clone(), meter.clone(), Arc::clone(&hold), Arc::clone(&queue));
        std::thread::Builder::new().name("kiln-convo".into()).spawn(move || writer(engine, &rx, &w, upkeep, &m, &h, &q))?;
        Ok(Self { tx, queue, id, meter, watchers, hold })
    }

    /// Keeps the records of sessions that ended at or after `ended_ms` (unix ms), however long
    /// ago, whatever `keep_closed` says: those the server's archive of transcripts hasn't taken yet
    /// (`crate::harness::archive`). `i64::MAX` holds nothing back; moved later, the writer lets go
    /// at once of what it now may.
    pub fn hold_prune(&self, ended_ms: i64) {
        self.hold.store(ended_ms, Ordering::Release);
    }

    /// Where [`Store::hold_prune`] holds it: None, nothing held back.
    pub fn held(&self) -> Option<i64> {
        Some(self.hold.load(Ordering::Acquire)).filter(|h| *h != i64::MAX)
    }

    /// Let go of the store's file for another process to open it (the new build taking over from
    /// this runner, `crate::bluegreen`): what was sent before is committed, the log emptied into the
    /// file, and the file closed. Whatever is asked of it after fails, and rows put after go.
    /// Blocking, for `READ_WAIT` at most.
    pub fn close(&self) -> anyhow::Result<()> {
        let (rtx, rrx) = mpsc::channel();
        self.tx.send(Msg::Close(rtx)).map_err(|_| anyhow::anyhow!("the conversation store has stopped"))?;
        rrx.recv_timeout(READ_WAIT).unwrap_or_else(|_| Err(anyhow::anyhow!("the conversation store didn't let go of its file in time")))
    }

    /// Who this store is to its peers.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What the store has done since kiln started, and its file as the writer last saw it. Never
    /// waits for the writer.
    pub fn report(&self) -> StoreReport {
        self.meter.report(&self.id)
    }

    /// What `update` of `session` makes, to be committed with the next batch.
    pub fn send(&self, session: &str, update: &Update) {
        self.put(rows::of(session, update, crate::util::now_ms()));
    }

    /// Rows to be committed with the next batch. Never blocks and never drops them: whoever sends
    /// waits for [`Store::room`] after (a store over its bound holds its senders back, not their rows).
    /// Lost only when the writer has stopped (counted, said).
    pub fn put(&self, rows: Vec<Row>) {
        if rows.is_empty() {
            return;
        }
        let bytes = rows.iter().map(Row::bytes).sum();
        self.queue.took(bytes);
        if self.tx.send(Msg::Rows(rows, bytes)).is_err() {
            self.queue.let_go(bytes, None);
            let n = self.meter.dropped();
            if (n - 1).is_power_of_two() {
                tracing::warn!(dropped = n, "conversation store: its writer has stopped; updates were dropped");
            }
        }
    }

    /// Rows to be committed with the next batch, then waiting, up to `READ_WAIT` (a writer stuck, its
    /// disk say, holds a sender no longer), while the store is over its bound ([`Queue`]): false when
    /// they couldn't be handed over (its writer has stopped: whoever sends them sends them again).
    /// Blocking: for a thread of its own.
    pub fn put_wait(&self, rows: Vec<Row>) -> bool {
        if rows.is_empty() {
            return true;
        }
        let bytes = rows.iter().map(Row::bytes).sum();
        self.queue.took(bytes);
        if self.tx.send(Msg::Rows(rows, bytes)).is_err() {
            self.queue.let_go(bytes, None);
            return false;
        }
        let until = Instant::now() + READ_WAIT;
        while !self.queue.has_room() && Instant::now() < until {
            let listener = self.queue.room.listen();
            if self.queue.has_room() {
                break;
            }
            let _ = listener.wait_deadline(until);
        }
        true
    }

    /// Waits while more waits for the writer than its bound ([`Queue`]): whoever sent rows waits here
    /// after, so a burst waits for the writer rather than growing without bound or being dropped. A
    /// task's wait (the thread goes on with others).
    pub async fn room(&self) {
        self.room_within(0).await;
    }

    /// [`Store::room`] for a sender whose updates' rows may wait as long as `most` bytes of them: what would wait
    /// for room in its own queues otherwise. Goal 10, plan 01a1a87b2dae step 5: a sessions' owner waited for room
    /// (the bound, what the writer commits in [`COMMIT_MS`]) between its turns, so a disk that stalled the writer
    /// stalled every frame and attach of every session it owned (g9p7: an update's wait for its owner 839 ms at
    /// p99.9 in one round). Its rows now wait here instead, as long as the updates would have waited for it in its
    /// inbox, and what hands it updates waits past that: none of them dropped, each session's in order (they come
    /// from its one owner), and as much held as before.
    pub async fn room_within(&self, most: usize) {
        self.queue.within.fetch_max(most, Ordering::Relaxed);
        while !self.queue.has_room_within(most) {
            let listener = self.queue.room.listen();
            if self.queue.has_room_within(most) {
                return;
            }
            listener.await;
        }
    }

    /// What waits for the writer, in bytes, and its bound.
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Run `f` on the engine once what was sent before is committed. Blocking, for `READ_WAIT` at
    /// most.
    fn run<T: Send + 'static>(&self, f: impl FnOnce(&mut dyn Engine) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        let asked = Instant::now();
        self.tx
            .send(Msg::Run(Box::new(move |e| {
                let _ = rtx.send(f(e));
            })))
            .map_err(|_| anyhow::anyhow!("the conversation store has stopped"))?;
        match rrx.recv_timeout(READ_WAIT) {
            Ok(answer) => {
                self.meter.waited(asked.elapsed());
                answer
            }
            Err(_) => {
                self.meter.timed_out();
                Err(anyhow::anyhow!("the conversation store didn't answer in time"))
            }
        }
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

    /// Events matching `q`, newest first, looked for a slice at a time: each slice is a job of its
    /// own, so a search that looks at every event (for a word that is rare, or in none) holds the
    /// store a slice at a time, never for all of it. Past `SEARCH_WAIT`, the newest matches it
    /// found. Blocking.
    pub fn search(&self, q: Query) -> anyhow::Result<Vec<EventRow>> {
        let limit = q.limit.unwrap_or(100).min(SEARCH_MAX);
        let (q, until) = (Arc::new(q), Instant::now() + SEARCH_WAIT);
        let (mut found, mut from, mut rows) = (Vec::new(), None, SEARCH_SLICE_FIRST);
        while found.len() < limit {
            let (q, want) = (q.clone(), limit - found.len());
            let (more, next, took) = self.run(move |e| {
                let began = Instant::now();
                let (more, next) = e.search_some(&q, from, rows, want)?;
                Ok((more, next, began.elapsed()))
            })?;
            found.extend(more);
            rows = if took > SEARCH_SLICE_TIME {
                (rows / 2).max(SEARCH_SLICE_MIN)
            } else if took < SEARCH_SLICE_TIME / 2 {
                (rows * 2).min(SEARCH_SLICE)
            } else {
                rows
            };
            match next {
                Some(next) if Instant::now() < until => from = Some(next),
                _ => break,
            }
        }
        Ok(found)
    }

    /// The rows written after number `from`, about `max_bytes` of them, record lines as text.
    /// Blocking.
    pub fn changes(&self, from: u64, max_bytes: usize) -> anyhow::Result<Changes> {
        self.changes_as(from, max_bytes, false)
    }

    /// [`Store::changes`], record lines as they are kept when `kept` ([`Engine::changes`]).
    /// Blocking.
    pub fn changes_as(&self, from: u64, max_bytes: usize, kept: bool) -> anyhow::Result<Changes> {
        self.run(move |e| e.changes(from, max_bytes, kept))
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

    /// Take a batch of a mirror's copy of this store ([`Engine::take_copy`]). Blocking.
    pub fn take_copy(&self, peer: &str, from: u64, batch: Changes) -> anyhow::Result<Took> {
        let p = peer.to_owned();
        let w = self.watchers.clone();
        self.run(move |e| {
            let took = e.take_copy(&p, from, &batch)?;
            if took.applied > 0 {
                notify(&w);
            }
            Ok(took)
        })
    }

    /// A copy has been taken whole: how many sessions it brought back. Blocking.
    pub fn end_copy(&self) -> anyhow::Result<usize> {
        self.run(|e| e.end_copy())
    }

    /// Whether the store holds no session at all. Blocking.
    pub fn is_empty(&self) -> anyhow::Result<bool> {
        self.run(|e| e.is_empty())
    }

    /// The last number the store gave a write. Blocking.
    pub fn top(&self) -> anyhow::Result<u64> {
        self.run(|e| Ok(e.top()))
    }

    /// Through where this store has `peer`'s feed. Blocking.
    pub fn peer(&self, peer: &str) -> anyhow::Result<u64> {
        let p = peer.to_owned();
        self.run(move |e| e.peer(&p))
    }

    /// Every peer whose feed this store took ([`Engine::peers`]). Blocking.
    pub fn peers(&self) -> anyhow::Result<Vec<(String, u64, i64)>> {
        self.run(|e| e.peers())
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

    /// The sessions that ended after `after` (the page before's last, `(closed_at, id)`; `None`:
    /// from the first), oldest first, a page of `limit`, at most [`ENDED_MAX`]: `(id, closed_at)`.
    /// Paged by close and id together, so sessions that closed in one millisecond each come once.
    /// Blocking.
    pub fn ended(&self, after: Option<(i64, String)>, limit: usize) -> anyhow::Result<Vec<(String, i64)>> {
        let (at, id) = after.unwrap_or((i64::MIN, String::new()));
        let limit = limit.min(ENDED_MAX);
        self.run(move |e| e.ended((at, &id), limit))
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

/// The writer: rows wait at most `COMMIT_MS` (or until `COMMIT_ROWS`) to share a commit; a job
/// commits what waits first, so it sees everything sent before it.
fn writer(mut engine: Box<dyn Engine>, rx: &mpsc::Receiver<Msg>, watchers: &Watchers, upkeep: Upkeep, meter: &Meter, hold: &AtomicI64, queue: &Queue) {
    let keep_closed = upkeep.keep_closed;
    // The hold on letting records go as it last looked: one moved later lets more go, so it looks
    // again.
    let mut held = hold.load(Ordering::Acquire);
    let mut pending: Vec<Row> = Vec::new();
    // The bytes `pending` holds, as the queue counted them.
    let mut pending_bytes = 0usize;
    let mut since: Option<Instant> = None;
    // Lines kept before lines were compressed may be left: compressed while nothing else waits.
    let mut squeezing = true;
    // Looking after the file while nothing else waits, once compressing is done and then every
    // `LOOK_EVERY`: the records of sessions that ended longer than `keep_closed` ago go, then the
    // file is made compact if enough of it is free, a step at a time.
    let (mut looking, mut pruned, mut look_at) = (false, 0usize, Instant::now());
    let mut compacting: Option<Instant> = None;
    // When it last looked at its file for its telemetry, and counted its sessions.
    let (mut looked, mut counted): (Option<Instant>, Option<Instant>) = (None, None);
    let commit = |engine: &mut Box<dyn Engine>, pending: &mut Vec<Row>, pending_bytes: &mut usize| {
        if pending.is_empty() {
            return;
        }
        let rows = rows::coalesce(std::mem::take(pending));
        let began = Instant::now();
        let applied = engine.apply(&rows);
        // Their room is the senders' again; the time a commit took is the writer's rate (`Queue::bound`).
        queue.let_go(std::mem::take(pending_bytes), applied.is_ok().then(|| began.elapsed()));
        match applied {
            Ok(()) => {
                meter.committed(rows.len());
                notify(watchers);
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "conversation store: a batch couldn't be written"),
        }
    };
    loop {
        let idle = if squeezing || looking || compacting.is_some() { SQUEEZE_EVERY } else { look_at.saturating_duration_since(Instant::now()).max(SQUEEZE_EVERY).min(LOOK_FILE) };
        let wait = since.map_or(idle, |t| Duration::from_millis(COMMIT_MS).saturating_sub(t.elapsed()));
        let got = rx.recv_timeout(wait);
        let began = Instant::now();
        match got {
            Ok(Msg::Rows(rows, bytes)) => {
                pending.extend(rows);
                pending_bytes += bytes;
                since.get_or_insert_with(Instant::now);
                if pending.len() >= COMMIT_ROWS {
                    commit(&mut engine, &mut pending, &mut pending_bytes);
                    since = None;
                }
            }
            Ok(Msg::Run(f)) => {
                commit(&mut engine, &mut pending, &mut pending_bytes);
                since = None;
                f(engine.as_mut());
                meter.ran();
            }
            Ok(Msg::Close(reply)) => {
                commit(&mut engine, &mut pending, &mut pending_bytes);
                // Its files closed before it says so: whoever moves them next finds them let go of.
                let released = engine.release();
                drop(engine);
                let _ = reply.send(released);
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let had = !pending.is_empty();
                commit(&mut engine, &mut pending, &mut pending_bytes);
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
                } else if let Some(compact_began) = compacting.filter(|_| !had) {
                    engine = match engine.compact_some(upkeep.compact.unwrap_or(COMPACT_MIN_FREE), COMPACT_STEP) {
                        Ok((e, Compacting::Going)) => e,
                        Ok((e, done)) => {
                            if let Compacting::Done { before, after } = done {
                                tracing::info!(before, after, "conversation store: its file was made compact");
                                let ms = u64::try_from(compact_began.elapsed().as_millis()).unwrap_or(u64::MAX);
                                meter.saw(|f| f.compacted = Some(Compacted { at: crate::util::now_ms(), before, after, ms }));
                            }
                            (compacting, look_at, looked) = (None, Instant::now() + LOOK_EVERY, None);
                            e
                        }
                        Err(e) => {
                            tracing::error!(error = %format!("{e:#}"), "conversation store: the compact copy couldn't be opened; the store stops until kiln starts again, which puts it in place");
                            return;
                        }
                    };
                } else if !had && (looking || Instant::now() >= look_at || hold.load(Ordering::Acquire) > held) {
                    held = hold.load(Ordering::Acquire);
                    let gone = match keep_closed {
                        Some(keep) => engine.prune_some(crate::util::now_ms().saturating_sub(i64::try_from(keep.as_millis()).unwrap_or(i64::MAX)).min(held), PRUNE_BATCH),
                        None => Ok(0),
                    };
                    match gone {
                        Ok(0) => {
                            if pruned > 0 {
                                tracing::info!(lines = pruned, days = keep_closed.map_or(0, |k| k.as_secs() / 86_400), "conversation store: the records of sessions that ended long ago were let go");
                                meter.saw(|f| f.pruned = Some(Pruned { at: crate::util::now_ms(), lines: pruned as u64 }));
                            }
                            (looking, pruned) = (false, 0);
                            // Then the file is made compact, if that is worth it.
                            if upkeep.compact.is_some() {
                                compacting = Some(Instant::now());
                            } else {
                                look_at = Instant::now() + LOOK_EVERY;
                            }
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
                commit(&mut engine, &mut pending, &mut pending_bytes);
                return;
            }
        }
        // Its telemetry: every `LOOK_FILE`, whatever else it does, where its file stands; and how
        // long what it did held it.
        if looked.is_none_or(|t| t.elapsed() >= LOOK_FILE) {
            let count = counted.is_none_or(|t| t.elapsed() >= COUNT_EVERY);
            match engine.look(count) {
                Ok(l) => {
                    let upkeep = match (&l.compacting, squeezing, looking) {
                        (Some(c), ..) => Upkeeping::Compacting(c.clone()),
                        (None, true, _) => Upkeeping::Squeezing,
                        (None, false, true) => Upkeeping::Pruning { lines: pruned as u64 },
                        _ => Upkeeping::Idle,
                    };
                    meter.saw(|f| {
                        let now = crate::util::now_ms();
                        if f.at != 0 && f.top != l.top {
                            f.wrote_at = now;
                        }
                        (f.at, f.bytes, f.free, f.wal, f.top, f.upkeep) = (now, l.bytes, l.free, l.wal, l.top, upkeep);
                        if l.sessions.is_some() {
                            f.sessions = l.sessions;
                        }
                    });
                    if count {
                        counted = Some(Instant::now());
                    }
                }
                Err(e) => tracing::debug!(error = %format!("{e:#}"), "conversation store: its file couldn't be looked at"),
            }
            looked = Some(Instant::now());
        }
        meter.held(began.elapsed());
    }
}

/// A sink that keeps what passes through it in `store` on its way to `sink`, in order, with the
/// same room (a full `sink` holds the sessions back, as it always did): `room` updates, whatever each
/// carries, on a lock-free queue (goal 5, plan 01a1a59c2aae: an async-channel's wait took
/// event-listener's `Mutex`, each session's shard sending and the tee's waiting on its own).
pub fn tee(sink: impl Into<Sink>, store: Store, room: usize) -> Sink {
    let sink: Sink = sink.into();
    let (tx, rx) = crate::memory::queue::places::<(String, Update)>(&crate::memory::owners::CHANNELS, room);
    crate::rt::spawn(async move {
        while let Ok((id, u)) = rx.recv().await {
            store.send(&id, &u);
            // A store over its bound holds what passes back, rather than its rows piling up (goal 11), once they're as
            // many as this runner's streams keep unanswered for the server (goal 10, plan 01a1a87b2dae step 5: the
            // disk's stall held the updates, and the frames they make, before the server ever had them).
            store.room_within(crate::wire::uplink::TOTAL_ROOM).await;
            if sink.send((id, u)).await.is_err() {
                break;
            }
        }
    });
    tx.into()
}

/// Where the store a [`tee_held`] keeps what passes in stands.
enum Keeping {
    /// Not given yet: the rows wait for it, in order.
    Waiting(Vec<Row>),
    Store(Store),
    /// Taken away: what passes after goes nowhere.
    Gone,
}

/// The store a [`tee_held`] keeps what passes through it in, held so that it can be given later
/// (a new build taking over waits for the store the runner it takes over from hands it,
/// `crate::bluegreen`) or taken away (that runner, handing its store over).
#[derive(Clone)]
pub struct Held {
    keeping: Arc<Mutex<Keeping>>,
    /// The tee's way in, held weakly: the tee still ends once its senders have.
    input: crate::memory::queue::WeakSender<(String, Update)>,
}

impl Held {
    /// Keep what passes in `store` from now on, what waited for it first, in order.
    pub fn give(&self, store: Store) {
        let mut k = self.keeping.lock().unwrap_or_else(PoisonError::into_inner);
        if let Keeping::Waiting(rows) = std::mem::replace(&mut *k, Keeping::Gone) {
            for part in rows.chunks(COMMIT_ROWS) {
                store.put(part.to_vec());
            }
        }
        *k = Keeping::Store(store);
    }

    /// The store, taken away: what is being kept is in it first, and what passes after goes
    /// nowhere. None: it had none.
    pub fn take(&self) -> Option<Store> {
        match std::mem::replace(&mut *self.keeping.lock().unwrap_or_else(PoisonError::into_inner), Keeping::Gone) {
            Keeping::Store(st) => Some(st),
            Keeping::Waiting(_) | Keeping::Gone => None,
        }
    }

    /// Nothing waits to pass: what was sent through it before is kept (or waits for the store).
    pub fn drained(&self) -> bool {
        self.input.upgrade().is_none_or(|s| s.is_empty())
    }
}

/// [`tee`], with its store [`Held`]: given now, or later, or taken away. Rows that come before
/// the store wait for it in memory; never given, they go.
pub fn tee_held(sink: impl Into<Sink>, store: Option<Store>, room: usize) -> (Sink, Held) {
    let sink: Sink = sink.into();
    // `room` updates, whatever each carries, on a lock-free queue, as [`tee`]'s.
    let (tx, rx) = crate::memory::queue::places::<(String, Update)>(&crate::memory::owners::CHANNELS, room);
    let keeping = Arc::new(Mutex::new(store.map_or_else(|| Keeping::Waiting(Vec::new()), Keeping::Store)));
    let held = Held { keeping: keeping.clone(), input: tx.downgrade() };
    crate::rt::spawn(async move {
        while let Ok((id, u)) = rx.recv().await {
            // Kept before it goes on, under the lock a taker waits on; then, the lock let go, a store over
            // its bound holds what passes back (goal 11, plan 01a1a87b2e56 step 3).
            let kept_in = match &mut *keeping.lock().unwrap_or_else(PoisonError::into_inner) {
                Keeping::Waiting(rows) => {
                    rows.extend(rows::of(&id, &u, crate::util::now_ms()));
                    None
                }
                Keeping::Store(st) => {
                    st.send(&id, &u);
                    Some(st.clone())
                }
                Keeping::Gone => None,
            };
            // As [`tee`]'s: held back once its rows are as many as this runner's streams keep unanswered.
            if let Some(st) = kept_in {
                st.room_within(crate::wire::uplink::TOTAL_ROOM).await;
            }
            if sink.send((id, u)).await.is_err() {
                break;
            }
        }
    });
    (tx.into(), held)
}

/// A store on a new database in a temporary folder, for tests.
#[cfg(test)]
pub fn scratch() -> anyhow::Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::start(Box::new(super::turso::Turso::open(&dir.path().join(super::FILE))?), Upkeep::default())?;
    Ok((dir, store))
}

/// A store whose writer has stopped: everything asked of it fails at once, for tests.
#[cfg(test)]
pub fn stopped() -> Store {
    let (tx, _) = mpsc::channel();
    Store { tx, queue: Arc::default(), id: "stopped".into(), meter: Arc::default(), watchers: Arc::default(), hold: Arc::new(AtomicI64::new(i64::MAX)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Body, Event, State};

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    #[test]
    fn a_store_let_go_of_goes_on_in_another_process_folder_with_what_it_wrote_and_its_numbers() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (from, to) = (dir.path().join("old").join(super::super::FILE), dir.path().join("new").join(super::super::FILE));
        std::fs::create_dir_all(dir.path().join("old"))?;
        let st = Store::start(Box::new(super::super::turso::Turso::open(&from)?), Upkeep::default())?;
        st.send("a1", &said("m1", 1, "kept across the upgrade"));
        let (id, top) = (st.id().to_owned(), st.top()?);
        st.close()?;
        // Its files closed by the time it says so: nothing here holds them as they move.
        #[cfg(target_os = "linux")]
        {
            let file = std::fs::canonicalize(&from)?.to_string_lossy().into_owned();
            let open: Vec<std::path::PathBuf> = std::fs::read_dir("/proc/self/fd")?.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok()).filter(|p| p.to_string_lossy().starts_with(&file)).collect();
            assert!(open.is_empty(), "still open once let go of: {open:?}");
        }
        // Let go of: nothing more is taken, and asking fails at once.
        st.send("a1", &said("m2", 2, "too late"));
        assert!(st.top().is_err());
        // Moved to the new build's folder and opened there: the same store, numbers and all.
        super::super::move_file(&from, &to)?;
        assert!(!from.exists());
        assert!(super::super::move_file(&from, &to).is_err(), "never over a store that's there");
        let again = Store::start(Box::new(super::super::turso::Turso::open(&to)?), Upkeep::default())?;
        assert_eq!((again.id(), again.top()?), (id.as_str(), top));
        assert_eq!(again.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect::<Vec<_>>()), Some(vec!["kept across the upgrade".to_owned()]));
        Ok(())
    }

    /// A store that won't open (here, not a database at all) is set aside whole, its log beside it,
    /// never over another set aside before, and a new store takes its place: a new build handed it
    /// keeps conversations all the same.
    #[test]
    fn a_store_that_wont_open_is_set_aside_whole_and_a_new_one_takes_its_place() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (name, junk) = (super::super::FILE, vec![0xab_u8; 8192]);
        let at = |suffix: &str| dir.path().join(format!("{name}{suffix}"));
        std::fs::write(at(""), &junk)?;
        std::fs::write(at("-wal"), b"nor is this a log")?;
        let (st, aside) = super::super::open_or_set_aside(dir.path(), name, Upkeep::default(), 2, Duration::from_millis(10));
        let st = st.ok_or_else(|| anyhow::anyhow!("no new store"))?;
        assert_eq!(aside, Some(at(".unopened")));
        assert_eq!(std::fs::read(at(".unopened"))?, junk);
        assert_eq!(std::fs::read(at(".unopened-wal"))?, b"nor is this a log");
        st.send("a1", &said("m1", 1, "kept in the new one"));
        st.sync()?;
        assert_eq!(st.kept("a1", 10)?.map(|k| k.events.len()), Some(1));
        st.close()?;
        // Another that won't open, set aside beside the first.
        std::fs::write(at(""), &junk)?;
        for side in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(at(side));
        }
        let (st, aside) = super::super::open_or_set_aside(dir.path(), name, Upkeep::default(), 1, Duration::ZERO);
        assert!(st.is_some());
        assert_eq!(aside, Some(at(".unopened.2")));
        assert_eq!(std::fs::read(at(".unopened"))?, junk);
        // A store set aside still opens as it is, by its new name, its log by the same name.
        let real = dir.path().join("real.db");
        let st = Store::start(Box::new(super::super::turso::Turso::open(&real)?), Upkeep::default())?;
        st.send("a1", &said("m1", 1, "set aside whole"));
        let id = st.id().to_owned();
        st.close()?;
        let aside = super::super::set_aside(&real)?;
        assert_eq!(aside, dir.path().join("real.db.unopened"));
        let again = Store::start(Box::new(super::super::turso::Turso::open(&aside)?), Upkeep::default())?;
        assert_eq!(again.id(), id);
        assert_eq!(again.kept("a1", 10)?.map(|k| k.events.len()), Some(1));
        Ok(())
    }

    /// Goal 10, plan 01a1a87b2dae step 5: a sender that may let `most` bytes of rows wait (a sessions' owners', as many
    /// as their inboxes hold) goes on while the queue holds no more than that, past the writer's own bound, and past it
    /// waits until the writer lets them go; `/metrics` is told the queue may hold that much.
    #[test]
    fn g10_s5_a_sender_waits_only_past_what_it_may_let_wait() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        let q = store.queue();
        // Nothing committed yet, so the writer's own bound lets nothing wait.
        q.pretend_waiting(1_000);
        anyhow::ensure!(!q.has_room() && q.has_room_within(1_000) && !q.has_room_within(999));
        anyhow::ensure!(futures::FutureExt::now_or_never(store.room_within(1_000)).is_some(), "within what it may let wait: at once");
        let mut past = Box::pin(store.room_within(999));
        anyhow::ensure!(futures::FutureExt::now_or_never(past.as_mut()).is_none(), "past it: it waits");
        q.pretend_written(1_000);
        anyhow::ensure!(futures::FutureExt::now_or_never(past.as_mut()).is_some(), "the writer let them go: it goes on");
        anyhow::ensure!(q.within.load(Ordering::Relaxed) == 1_000, "the most any sender may let wait");
        Ok(())
    }

    #[test]
    fn a_tee_whose_store_comes_later_passes_everything_on_and_keeps_it_all_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (out, passed) = async_channel::unbounded();
            let (sink, held) = tee_held(out, None, 16);
            sink.send(("a1".to_owned(), said("m1", 1, "before the store"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            // It goes on at once, with no store yet.
            let (id, _) = crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            assert_eq!(id, "a1");
            let (_d, st) = scratch()?;
            held.give(st.clone());
            sink.send(("a1".to_owned(), said("m2", 2, "after it"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            st.sync()?;
            let texts = st.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect::<Vec<_>>()).unwrap_or_default();
            assert_eq!(texts, vec!["before the store".to_owned(), "after it".to_owned()]);
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side: a tee's way in is a lock-free queue of
    /// `room` updates. Sessions on every shard sending at once, past its room while what's after it takes
    /// slowly, each have every update passed on once, in the order sent; its way in never holds more than its
    /// room; and once its senders are gone and it's drained, what's after it sees the end.
    #[test]
    fn g5_a_tee_passes_every_shards_updates_on_once_in_order_within_its_room() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (out, passed) = crate::memory::queue::places(&crate::memory::owners::CHANNELS, 4);
            let room = 16;
            let (sink, held) = tee_held(out, None, room);
            // Looked at weakly: a sender held here would keep the tee from ever ending.
            let way_in = match &sink {
                Sink::Bounded(q) => q.downgrade(),
                Sink::Channel(_) => anyhow::bail!("a tee's way in isn't a queue"),
            };
            let (senders, per) = (8usize, 200u64);
            let mut sending = Vec::new();
            for t in 0..senders {
                let sink = sink.clone();
                sending.push(crate::rt::spawn_spread_joined(async move {
                    for n in 0..per {
                        if sink.send((format!("s{t}"), said(&format!("m{n}"), n, "x"))).await.is_err() {
                            return false;
                        }
                    }
                    true
                }));
            }
            drop(sink);
            let (mut got, mut most) = (vec![Vec::<u64>::new(); senders], 0usize);
            loop {
                most = most.max(way_in.upgrade().map_or(0, |q| q.len()));
                let next = crate::rt::timeout(Duration::from_secs(30), passed.recv()).await.map_err(|_| anyhow::anyhow!("the tee never ended"))?;
                let Ok((id, u)) = next else { break };
                let t: usize = id.trim_start_matches('s').parse()?;
                let Update::Events { events } = u else { anyhow::bail!("not what was sent") };
                got.get_mut(t).ok_or_else(|| anyhow::anyhow!("no session {t}"))?.extend(events.iter().map(|e| e.seq));
                // Slower than its senders now and then, so they wait for room.
                if got.iter().map(Vec::len).sum::<usize>() % 100 == 0 {
                    crate::rt::sleep(Duration::from_millis(2)).await;
                }
            }
            for s in sending {
                anyhow::ensure!(s.await.unwrap_or(false), "a session's update was refused");
            }
            for (t, theirs) in got.iter().enumerate() {
                anyhow::ensure!(theirs.iter().copied().eq(0..per), "session {t}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
            }
            anyhow::ensure!(most <= room, "its way in held {most}, past its room of {room}");
            anyhow::ensure!(held.drained(), "drained, its senders gone");
            Ok(())
        })
    }

    #[test]
    fn a_tee_whose_store_is_taken_away_keeps_nothing_more_in_it_and_says_when_it_is_drained() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (out, passed) = async_channel::unbounded();
            let (_d, st) = scratch()?;
            let (sink, held) = tee_held(out, Some(st.clone()), 16);
            sink.send(("a1".to_owned(), said("m1", 1, "kept"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            assert!(held.drained());
            let taken = held.take().ok_or_else(|| anyhow::anyhow!("no store"))?;
            assert_eq!(taken.id(), st.id());
            assert!(held.take().is_none(), "taken once");
            // After: it still goes on, and isn't kept.
            sink.send(("a1".to_owned(), said("m2", 2, "not kept"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            st.sync()?;
            assert_eq!(st.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.text).collect::<Vec<_>>()), Some(vec!["kept".to_owned()]));
            Ok(())
        })
    }

    #[test]
    fn the_writer_lets_go_of_the_records_of_sessions_that_ended_long_ago_while_idle() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::start(Box::new(super::super::turso::Turso::open(&dir.path().join(super::super::FILE))?), Upkeep { keep_closed: Some(Duration::from_secs(60)), compact: None })?;
        let now = crate::util::now_ms();
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: now };
        let line = |s: &str, n: u64| Row::Record(Box::new(rows::RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: "{}".into(), kept: None }));
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

    /// CP-T10.1: a hold keeps the records of sessions that ended at or after it, however long ago,
    /// while those that ended before it go; moved past them, they go at once (the server's
    /// transcripts are archived before their records go).
    #[test]
    fn a_hold_keeps_the_records_of_sessions_that_ended_at_or_after_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::start(Box::new(super::super::turso::Turso::open(&dir.path().join(super::super::FILE))?), Upkeep { keep_closed: Some(Duration::from_secs(60)), compact: None })?;
        let now = crate::util::now_ms();
        let held = now - 3_600_000;
        store.hold_prune(held);
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: now };
        let line = |s: &str, n: u64| Row::Record(Box::new(rows::RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: "{}".into(), kept: None }));
        store.put(vec![session("before"), line("before", 0), Row::Closed { session: "before".into(), at: held - 60_000 }]);
        store.put(vec![session("held"), line("held", 0), Row::Closed { session: "held".into(), at: held }]);
        let until = Instant::now() + Duration::from_secs(15);
        while !store.marks("before")?.is_empty() {
            anyhow::ensure!(Instant::now() < until, "the record of a session that ended before the hold was kept");
            std::thread::sleep(SQUEEZE_EVERY * 3);
        }
        assert_eq!(store.marks("held")?.len(), 1, "a session that ended at the hold keeps its record");
        store.hold_prune(held + 1);
        // Asked more seldom than the writer waits, idle, before it looks again.
        let until = Instant::now() + Duration::from_secs(30);
        while !store.marks("held")?.is_empty() {
            anyhow::ensure!(Instant::now() < until, "the hold moved past it, the record was still kept");
            std::thread::sleep(LOOK_FILE + SQUEEZE_EVERY);
        }
        Ok(())
    }

    /// CP-T10.1: sessions that ended come a page at a time by close and id together: sessions that
    /// closed in one millisecond each come once across pages, and one not ended never.
    #[test]
    fn ended_sessions_page_by_close_and_id_together() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::start(Box::new(super::super::turso::Turso::open(&dir.path().join(super::super::FILE))?), Upkeep::default())?;
        let t = crate::util::now_ms() - 60_000;
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: t };
        let closed = |s: &str, at: i64| Row::Closed { session: s.into(), at };
        assert!(store.put_wait(vec![session("s3"), session("s1"), session("s2"), session("s4"), session("open")]));
        assert!(store.put_wait(vec![closed("s3", t), closed("s1", t), closed("s2", t), closed("s4", t + 1)]));
        let first = store.ended(None, 2)?;
        assert_eq!(first, [("s1".to_owned(), t), ("s2".to_owned(), t)]);
        let second = store.ended(Some((t, "s2".to_owned())), 2)?;
        assert_eq!(second, [("s3".to_owned(), t), ("s4".to_owned(), t + 1)]);
        assert!(store.ended(Some((t + 1, "s4".to_owned())), 2)?.is_empty());
        assert_eq!(store.ended(None, 10_000)?.len(), 4, "capped at ENDED_MAX, and every ended one");
        Ok(())
    }

    #[test]
    fn the_writer_makes_the_file_compact_by_itself_once_enough_of_it_is_free_and_goes_on() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join(super::super::FILE);
        let store = Store::start(Box::new(super::super::turso::Turso::open(&path)?), Upkeep { keep_closed: Some(Duration::from_secs(60)), compact: Some(1) })?;
        let now = crate::util::now_ms();
        // Lines that hardly compress: a file of a few MB.
        let noise = |n: u64| -> String {
            let mut x = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (0..2_000)
                .map(|_| {
                    x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    char::from(b'a' + u8::try_from((x >> 33) % 26).unwrap_or(0))
                })
                .collect()
        };
        let line = |s: &str, n: u64, text: String| Row::Record(Box::new(rows::RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: text, kept: None }));
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: now };
        // An agent that ended an hour ago, with a long record: let go, which leaves most of the
        // file free; then the file is made compact.
        store.put(vec![session("old")]);
        for chunk in (0..3_000u64).collect::<Vec<_>>().chunks(500) {
            assert!(store.put_wait(chunk.iter().map(|n| line("old", *n, noise(*n))).collect()));
        }
        store.put(vec![Row::Closed { session: "old".into(), at: now - 3_600_000 }, session("live"), line("live", 0, "{\"kept\":true}".into())]);
        store.sync()?;
        let size = || std::fs::metadata(&path).map_or(0, |m| m.len());
        let (mut most, until) = (size(), Instant::now() + Duration::from_secs(60));
        loop {
            let now = size();
            most = most.max(now);
            if most > 2 << 20 && now * 4 < most {
                break;
            }
            anyhow::ensure!(Instant::now() < until, "the file wasn't made compact: {most} bytes at most, {now} now");
            std::thread::sleep(SQUEEZE_EVERY * 3);
        }
        assert!(!dir.path().join(format!("{}.compact.tmp", super::super::FILE)).exists());
        // The store goes on in the compact file, holding what it held.
        assert_eq!(store.record("live", "cc", 0, 10)?, vec![(0, "{\"kept\":true}".to_owned())]);
        store.put(vec![line("live", 1, "{\"after\":true}".into())]);
        store.sync()?;
        assert_eq!(store.record("live", "cc", 0, 10)?.len(), 2);
        assert!(store.marks("old")?.is_empty());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a burst several times the queue's bound waits for the writer
    /// rather than piling up in memory: what waits never goes past the bound by more than the one put that
    /// found it full, nothing is lost, and every event is kept, in order.
    #[test]
    fn g11_s3_a_burst_past_the_bound_waits_for_the_writer_and_loses_nothing() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        // The writer's first commit measures its rate: until then the bound is none past an empty queue.
        assert!(store.put_wait(rows::of("burst", &said("m0", 0, "the first"), crate::util::now_ms())));
        let _ = store.kept("burst", 1)?;
        let first = store.queue().bound();
        anyhow::ensure!(first > 0, "the writer's rate wasn't measured");
        let text = "a reply of a few hundred words, ".repeat(40);
        let one = rows::of("burst", &said("m1", 1, &text), 0).iter().map(Row::bytes).sum::<usize>();
        // Sent until it's four times the bound as the writer's rate settles (the bound grows with it), within
        // what a test takes.
        let (mut sent, mut most, mut bound, mut n) = (0usize, 0usize, first, 0u64);
        while (sent <= bound.saturating_mul(4) || n < 2_000) && n < 200_000 {
            n += 1;
            let i = n;
            let rows = rows::of("burst", &said(&format!("m{i}"), i, &text), crate::util::now_ms());
            let bytes: usize = rows.iter().map(Row::bytes).sum();
            sent += bytes;
            // What waits as this put goes in: what put_wait left, and this put's rows.
            most = most.max(store.queue().bytes() + bytes);
            bound = bound.max(store.queue().bound());
            assert!(store.put_wait(rows));
        }
        assert!(sent > 3 * bound, "the burst ({sent} bytes) wasn't past the bound ({bound} bytes) for long");
        assert!(most <= bound + one, "{most} bytes waited, past the bound ({bound}) and one put ({one})");
        let all = store.events("burst", None, usize::try_from(n + 1)?)?;
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), (0..=n).collect::<Vec<_>>(), "every event kept, in order");
        assert_eq!(store.queue().bytes(), 0, "nothing waits once it's all read back");
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
