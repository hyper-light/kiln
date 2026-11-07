//! A machine's conversation store (`docs/conversations.md`, "The store"): one writer thread owns the
//! database; sessions' updates reach it by channel, never by lock, and are committed in batches, each as
//! soon as the last is durable, with what came meanwhile as one (a self-clocked group commit): an update
//! waits for no others, at most for the commit under way. Reads go through the same thread, in turn with
//! the writes.
//!
//! Every write gets the next number of the store (its revision), so another store can take what
//! came after a number it has ([`Store::changes`], [`Store::take`]): that is how a runner's store
//! reaches the server's (`super::feed`).
//!
//! The engine is behind [`Engine`]: Turso ([`super::turso`]).
//!
//! What waits for the writer is bounded in bytes ([`Queue`]; goal 11, plan 01a1a87b2e56 step 3) by its
//! senders: one with a queue of its own lets as much wait here as that would hold, and waits past it
//! ([`Store::room_within`]); one on a thread of its own waits for the writer to take in what it sent, so its
//! next carries what it gathered meanwhile ([`Store::put_wait`]). Nothing is dropped (conversations are
//! durable state: nothing about a session is said before it's kept, plan 01a17197).

use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::rows::{self, EventRow, Row};
use super::telemetry::{CompactView, Compacted, Meter, Pruned, StoreReport, Upkeeping};
use crate::harness::core::Update;
use crate::harness::session::{Sink, Taken};

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

/// The bytes of `batch`'s rows, as the store counts a row ([`Row::bytes`]).
pub fn batch_bytes(batch: &Changes) -> u64 {
    batch.rows.iter().map(|c| u64::try_from(c.row.bytes()).unwrap_or(u64::MAX)).fold(0, u64::saturating_add)
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

/// Who wants to know that something was committed (a feed at its store's tip waits on it): a notifier each
/// ([`Store::watch`]). One its feed let go of goes at the next commit.
type Watchers = Arc<Mutex<Vec<Arc<crate::rt::Notify>>>>;

fn notify(watchers: &Watchers) {
    watchers.lock().unwrap_or_else(PoisonError::into_inner).retain(|w| {
        // Its feed holds it as long as it's there.
        let kept = Arc::strong_count(w) > 1;
        if kept {
            w.notify();
        }
        kept
    });
}

/// What waits for the writer, in bytes, and the most it may hold (goal 11, plan 01a1a87b2e56 step 3).
/// Lockless: atomics, and an event the writer tells as it commits.
#[derive(Default)]
pub struct Queue {
    /// Bytes of rows sent and not yet committed (`Row::bytes`).
    bytes: AtomicUsize,
    /// Bytes committed.
    written: AtomicU64,
    /// Bytes sent, and bytes let go of (committed, or the writer gone), each counted since the store began: the writer
    /// takes what's sent in the order it's sent, so what a sender sent is in once `gone` reaches where `sent` stood
    /// with it ([`Store::put_wait`]).
    sent: AtomicU64,
    gone: AtomicU64,
    /// Bytes of feeds' batches taken in ([`Store::take`], [`Store::take_copy`]): with `written`, what this store's own
    /// feeds have to ship ([`Store::grown`]).
    taken: AtomicU64,
    /// Told each time the writer commits (`crate::rt::Changed`, no lock; goal 5, plan 01a1a59c2aae, its top goal: it
    /// was an event-listener `Event`, its list behind a `Mutex`).
    room: crate::rt::Changed,
    /// The most any sender may let wait before it waits itself ([`Store::room_within`]): what `/metrics` says the
    /// queue may hold.
    within: AtomicUsize,
    /// The writer's wakes from a wait, and the wake it last took rows on (a test's).
    #[cfg(test)]
    woke: AtomicU64,
    #[cfg(test)]
    took_on: AtomicU64,
    /// The writer's wake as it last committed rows, and its count of commits as it told their senders (a test's).
    #[cfg(test)]
    committed_on: AtomicU64,
    #[cfg(test)]
    counted_when_told: AtomicU64,
}

impl Queue {
    /// Bytes waiting now.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Whether more may be sent by one that may let `most` wait: the queue is empty, or holds no more than `most`.
    fn has_room_within(&self, most: usize) -> bool {
        let b = self.bytes();
        b == 0 || b <= most
    }

    /// `bytes` sent: where the bytes sent stand with them ([`Queue::taken_in`]).
    fn took(&self, bytes: usize) -> u64 {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        crate::memory::CONVO_QUEUE.add(bytes);
        let n = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.sent.fetch_add(n, Ordering::Relaxed).saturating_add(n)
    }

    /// Whether the writer has let go of what was sent up to `at` (committed it, or gone with it).
    fn taken_in(&self, at: u64) -> bool {
        self.gone.load(Ordering::Acquire) >= at
    }

    /// `bytes` counted as waiting for the writer, for a test of what waits on the queue.
    #[cfg(test)]
    pub(crate) fn pretend_waiting(&self, bytes: usize) {
        self.took(bytes);
    }

    /// `bytes` the writer let go of, for a test of what waits on the queue.
    #[cfg(test)]
    pub(crate) fn pretend_written(&self, bytes: usize) {
        self.let_go(bytes, false);
    }

    /// `bytes` committed (`committed`), or let go of, the writer gone: their room is the senders' again.
    fn let_go(&self, bytes: usize, committed: bool) {
        let n = u64::try_from(bytes).unwrap_or(u64::MAX);
        if committed {
            self.written.fetch_add(n, Ordering::Relaxed);
        }
        let mut now = self.bytes.load(Ordering::Relaxed);
        while let Err(seen) = self.bytes.compare_exchange_weak(now, now.saturating_sub(bytes), Ordering::Relaxed, Ordering::Relaxed) {
            now = seen;
        }
        self.gone.fetch_add(n, Ordering::Release);
        crate::memory::CONVO_QUEUE.sub(bytes);
        crate::memory::CONVO_QUEUE.bound_at(self.within.load(Ordering::Relaxed));
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

    /// Rows to be committed, with whatever else waits, as soon as the writer is free. Never blocks and never drops
    /// them: whoever sends waits for room after ([`Store::room_within`]: a store holding more than its senders let
    /// wait holds them back, not their rows). Lost only when the writer has stopped (counted, said).
    pub fn put(&self, rows: Vec<Row>) {
        if rows.is_empty() {
            return;
        }
        let bytes = rows.iter().map(Row::bytes).sum();
        self.queue.took(bytes);
        if self.tx.send(Msg::Rows(rows, bytes)).is_err() {
            self.queue.let_go(bytes, false);
            let n = self.meter.dropped();
            if (n - 1).is_power_of_two() {
                tracing::warn!(dropped = n, "conversation store: its writer has stopped; updates were dropped");
            }
        }
    }

    /// Rows to be committed, then waiting, up to `READ_WAIT` (a writer stuck, its disk say, holds a sender no
    /// longer), until the writer has taken them in: one put on its way at a time, so what the sender gathers
    /// meanwhile goes in its next as one, and a burst waits for the writer rather than piling up (consts-audit B3:
    /// it waited while more waited than the writer committed in 50 ms). False when they couldn't be handed over
    /// (its writer has stopped: whoever sends them sends them again). Blocking: for a thread of its own.
    pub fn put_wait(&self, rows: Vec<Row>) -> bool {
        if rows.is_empty() {
            return true;
        }
        let bytes = rows.iter().map(Row::bytes).sum();
        let at = self.queue.took(bytes);
        if self.tx.send(Msg::Rows(rows, bytes)).is_err() {
            self.queue.let_go(bytes, false);
            return false;
        }
        let until = Instant::now() + READ_WAIT;
        while !self.queue.taken_in(at) && Instant::now() < until {
            let listener = self.queue.room.listen();
            if self.queue.taken_in(at) {
                break;
            }
            let _ = listener.wait_deadline(until);
        }
        true
    }

    /// Waits while anything waits for the writer ([`Queue`]): whoever sent rows waits here after, so a burst waits
    /// for the writer rather than growing without bound or being dropped. A task's wait (the thread goes on with
    /// others).
    pub async fn room(&self) {
        self.room_within(0).await;
    }

    /// [`Store::room`] for a sender whose updates' rows may wait as long as `most` bytes of them: what would wait
    /// for room in its own queues otherwise. Goal 10, plan 01a1a87b2dae step 5: a sessions' owner waited for room
    /// (the bound then, what the writer committed in 50 ms) between its turns, so a disk that stalled the writer
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

    /// Told of each commit (and of rows let go of, the writer gone): a thread of its own waits on what's returned for
    /// the store to take more in (`crate::convo::record`'s recorder, consts-audit B3).
    pub fn commits(&self) -> crate::rt::Listen<'_> {
        self.queue.room.listen()
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
        let (w, q) = (self.watchers.clone(), Arc::clone(&self.queue));
        self.run(move |e| {
            let took = e.take(&p, m.as_deref(), from, &batch)?;
            if took.applied > 0 {
                q.taken.fetch_add(batch_bytes(&batch), Ordering::Relaxed);
                notify(&w);
            }
            Ok(took)
        })
    }

    /// Take a batch of a mirror's copy of this store ([`Engine::take_copy`]). Blocking.
    pub fn take_copy(&self, peer: &str, from: u64, batch: Changes) -> anyhow::Result<Took> {
        let p = peer.to_owned();
        let (w, q) = (self.watchers.clone(), Arc::clone(&self.queue));
        self.run(move |e| {
            let took = e.take_copy(&p, from, &batch)?;
            if took.applied > 0 {
                q.taken.fetch_add(batch_bytes(&batch), Ordering::Relaxed);
                notify(&w);
            }
            Ok(took)
        })
    }

    /// Bytes of rows this store has committed since it opened (`Row::bytes`): those put ([`Store::put`]) and those of
    /// feeds' batches taken in. Read without asking its writer: past where it stood at a feed's last read that found
    /// nothing more, that feed has something to ship (goal 9, plan 01a1a87b2cec step 5; consts-audit R4).
    pub fn grown(&self) -> u64 {
        self.queue.written.load(Ordering::Relaxed).saturating_add(self.queue.taken.load(Ordering::Relaxed))
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

    /// Told of each commit: what a feed at its store's tip waits on (`super::feed`). A commit while no one waits
    /// leaves one notification however many came ([`crate::rt::Notify`]), waking nothing, so a feed holding what
    /// came, which waits on its link and its hold alone, is woken by none of the commits meanwhile (goal 9, plan
    /// 01a1a87b2cec step 5: a task of the feed's took each commit's word through a channel of its own and passed it
    /// on, two tasks woken for every commit, 20 a second while its store takes writes).
    pub fn watch(&self) -> Arc<crate::rt::Notify> {
        let commits = Arc::new(crate::rt::Notify::new());
        self.watchers.lock().unwrap_or_else(PoisonError::into_inner).push(Arc::clone(&commits));
        commits
    }
}

/// The writer: a self-clocked group commit (consts-audit B3; Ada, 2026-11-06: no arbitrary constants). It commits what
/// was sent as soon as it has it; what's sent while a commit is under way waits in the channel and goes in the next, once
/// the last is durable, as one. An idle store commits each row at once; a busy one as much at a time as came during the
/// last commit, so its batches grow with its load and its disk's pace. A job commits what waits first, so it sees
/// everything sent before it. Rows waited up to `COMMIT_MS` (50 ms) for others to share their commit, or for
/// `COMMIT_ROWS` (2048) of them.
fn writer(mut engine: Box<dyn Engine>, rx: &mpsc::Receiver<Msg>, watchers: &Watchers, upkeep: Upkeep, meter: &Meter, hold: &AtomicI64, queue: &Queue) {
    let keep_closed = upkeep.keep_closed;
    // The hold on letting records go as it last looked: one moved later lets more go, so it looks
    // again.
    let mut held = hold.load(Ordering::Acquire);
    let mut pending: Vec<Row> = Vec::new();
    // The bytes `pending` holds, as the queue counted them.
    let mut pending_bytes = 0usize;
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
        let applied = engine.apply(&rows);
        #[cfg(test)]
        queue.committed_on.store(queue.woke.load(Ordering::Relaxed), Ordering::Relaxed);
        // Counted before anyone is told (as `crate::store`'s writer counts a group: a caller that reads the count once
        // told finds its own commit in it): then the senders' room theirs again, then its watchers.
        match &applied {
            Ok(()) => meter.committed(rows.len()),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "conversation store: a batch couldn't be written"),
        }
        #[cfg(test)]
        queue.counted_when_told.store(meter.report("").commits, Ordering::Relaxed);
        queue.let_go(std::mem::take(pending_bytes), applied.is_ok());
        if applied.is_ok() {
            notify(watchers);
        }
    };
    loop {
        let idle = if squeezing || looking || compacting.is_some() { SQUEEZE_EVERY } else { look_at.saturating_duration_since(Instant::now()).max(SQUEEZE_EVERY).min(LOOK_FILE) };
        // Nothing waits here between commits (each takes all there is), so the writer waits on its channel: the first of
        // what's sent wakes it, and what's sent while it commits waits there, waking nothing (goal 9, plan 01a1a87b2cec
        // step 5: it was woken by every row sent while a batch gathered, a futex's wake on the sender's shard and a switch
        // to this thread and back, only to add the row to the batch).
        let got = rx.recv_timeout(idle);
        #[cfg(test)]
        queue.woke.fetch_add(1, Ordering::Relaxed);
        let began = Instant::now();
        match got {
            Ok(first) => {
                // What came since the last commit, as one: the first, and all the channel holds behind it, committed at
                // once (consts-audit B3); a job's rows committed before it runs.
                let mut next = Some(first);
                while let Some(m) = next.take() {
                    match m {
                        Msg::Rows(rows, bytes) => {
                            pending.extend(rows);
                            pending_bytes += bytes;
                            #[cfg(test)]
                            queue.took_on.store(queue.woke.load(Ordering::Relaxed), Ordering::Relaxed);
                        }
                        Msg::Run(f) => {
                            commit(&mut engine, &mut pending, &mut pending_bytes);
                            f(engine.as_mut());
                            meter.ran();
                        }
                        Msg::Close(reply) => {
                            commit(&mut engine, &mut pending, &mut pending_bytes);
                            // Its files closed before it says so: whoever moves them next finds them let go of.
                            let released = engine.release();
                            drop(engine);
                            let _ = reply.send(released);
                            return;
                        }
                    }
                    next = rx.try_recv().ok();
                }
                commit(&mut engine, &mut pending, &mut pending_bytes);
            }
            // Nothing waits (each commit took all there was): the file's upkeep.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if squeezing {
                    match engine.squeeze_some(SQUEEZE_BATCH) {
                        Ok(true) => {}
                        Ok(false) => squeezing = false,
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "conversation store: older record lines couldn't be compressed");
                            squeezing = false;
                        }
                    }
                } else if let Some(compact_began) = compacting {
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
                } else if looking || Instant::now() >= look_at || hold.load(Ordering::Acquire) > held {
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
/// event-listener's `Mutex`, each session's shard sending and the tee's waiting on its own). Each goes on
/// with when it was sent in, its wait here the runner's (goal 10, plan 01a1a87b2dae step 5: `Taken`).
pub fn tee(sink: impl Into<Sink>, store: Store, room: usize) -> Sink {
    let sink: Sink = sink.into();
    let (tx, rx) = crate::memory::queue::places::<Taken>(&crate::memory::owners::CHANNELS, room);
    crate::rt::spawn(async move {
        while let Ok(t) = rx.recv().await {
            store.send(&t.session, &t.update);
            // A store over its bound holds what passes back, rather than its rows piling up (goal 11), once they're as
            // many as this runner's streams keep unanswered for the server (goal 10, plan 01a1a87b2dae step 5: the
            // disk's stall held the updates, and the frames they make, before the server ever had them).
            store.room_within(crate::wire::uplink::TOTAL_ROOM).await;
            if sink.send_taken(t).await.is_err() {
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
    input: crate::memory::queue::WeakSender<Taken>,
}

impl Held {
    /// Keep what passes in `store` from now on, what waited for it first, in order.
    pub fn give(&self, store: Store) {
        let mut k = self.keeping.lock().unwrap_or_else(PoisonError::into_inner);
        if let Keeping::Waiting(rows) = std::mem::replace(&mut *k, Keeping::Gone) {
            store.put(rows);
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
    // `room` updates, whatever each carries, on a lock-free queue, as [`tee`]'s, each with when it was sent in.
    let (tx, rx) = crate::memory::queue::places::<Taken>(&crate::memory::owners::CHANNELS, room);
    let keeping = Arc::new(Mutex::new(store.map_or_else(|| Keeping::Waiting(Vec::new()), Keeping::Store)));
    let held = Held { keeping: keeping.clone(), input: tx.downgrade() };
    crate::rt::spawn(async move {
        while let Ok(t) = rx.recv().await {
            // Kept before it goes on, under the lock a taker waits on; then, the lock let go, a store over
            // its bound holds what passes back (goal 11, plan 01a1a87b2e56 step 3).
            let kept_in = match &mut *keeping.lock().unwrap_or_else(PoisonError::into_inner) {
                Keeping::Waiting(rows) => {
                    rows.extend(rows::of(&t.session, &t.update, crate::util::now_ms()));
                    None
                }
                Keeping::Store(st) => {
                    st.send(&t.session, &t.update);
                    Some(st.clone())
                }
                Keeping::Gone => None,
            };
            // As [`tee`]'s: held back once its rows are as many as this runner's streams keep unanswered.
            if let Some(st) = kept_in {
                st.room_within(crate::wire::uplink::TOTAL_ROOM).await;
            }
            // With when it was sent in, not now: its wait here was the runner's (goal 10, plan 01a1a87b2dae step 5).
            if sink.send_taken(t).await.is_err() {
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

    /// consts-audit B3 (goal 9, plan 01a1a87b2cec step 5): the docs say the store's commit as its writer makes it.
    /// docs/kilndb.md's account of the store as it is, and docs/runtime-lab.md's row for its queue, name no wait to
    /// share a commit (`COMMIT_MS`, gone from the code), and kilndb.md says the commit is self-clocked.
    #[test]
    fn g9_s5_b3_the_docs_say_the_group_commit_is_self_clocked() -> anyhow::Result<()> {
        let doc = |name: &str| std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs").join(name));
        let kilndb = doc("kilndb.md")?;
        let today = kilndb.split("## The design").next().unwrap_or_default();
        anyhow::ensure!(!today.contains("COMMIT_MS") && today.contains("self-clocked"), "docs/kilndb.md's \"Where it starts\" says the commit as the writer makes it");
        let lab = doc("runtime-lab.md")?;
        let row = lab.lines().find(|l| l.starts_with("| `convo_queue` |")).unwrap_or_default();
        anyhow::ensure!(!row.is_empty() && !row.contains("COMMIT_MS"), "docs/runtime-lab.md's convo_queue row: {row}");
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a feed's watch takes no lock (a [`crate::rt::Notify`]; goal 9, plan
    /// 01a1a87b2cec step 5). No word waits before a commit; commits made while its watcher looks elsewhere leave one word,
    /// which it takes, and none more until the next commit; a watcher gone is let go at the next commit, and the one left
    /// still hears it.
    #[test]
    fn g5_a_watchers_word_waits_once_however_many_commits_came() -> anyhow::Result<()> {
        use futures::FutureExt as _;
        let (_dir, store) = scratch()?;
        let watch = store.watch();
        // A word waiting, taken: its wait ends at once.
        let told = |w: &crate::rt::Notify| w.notified().now_or_never().is_some();
        anyhow::ensure!(!told(&watch), "a word before any commit");
        for i in 1..=3u64 {
            store.send("w1", &said(&format!("m{i}"), i, "x"));
            store.sync()?;
        }
        anyhow::ensure!(told(&watch), "no word after three commits");
        anyhow::ensure!(!told(&watch), "more than one word for three commits");
        drop(store.watch());
        anyhow::ensure!(store.watchers.lock().unwrap_or_else(PoisonError::into_inner).len() == 2, "the two watchers listed");
        store.send("w1", &said("m4", 4, "x"));
        store.sync()?;
        anyhow::ensure!(store.watchers.lock().unwrap_or_else(PoisonError::into_inner).len() == 1, "the watcher gone wasn't let go");
        anyhow::ensure!(told(&watch), "the watcher left didn't hear the next commit");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what a store's own feeds have to ship is read without asking its writer
    /// ([`Store::grown`]): it grows by the rows put once they're committed, and by a feed's batch taken in, each by its rows'
    /// bytes; a batch that brings nothing new grows it by nothing.
    #[test]
    fn g9_s5_a_store_grows_by_what_it_commits_and_takes() -> anyhow::Result<()> {
        let (_a, runner) = scratch()?;
        let (_b, server) = scratch()?;
        anyhow::ensure!(runner.grown() == 0 && server.grown() == 0, "nothing yet");
        let update = said("m1", 1, "the deadline");
        let bytes: u64 = rows::of("a1", &update, 0).iter().map(|r| u64::try_from(r.bytes()).unwrap_or(u64::MAX)).sum();
        runner.send("a1", &update);
        runner.sync()?;
        anyhow::ensure!(runner.grown() == bytes, "a commit's rows: {} against {bytes}", runner.grown());
        let batch = runner.changes_as(0, 1 << 20, false)?;
        let brought = batch_bytes(&batch);
        anyhow::ensure!(brought > 0, "a batch of what it committed");
        let took = server.take("runner", Some("box"), 0, batch.clone())?;
        anyhow::ensure!(took.applied > 0 && server.grown() == brought, "a batch taken in: {} against {brought}", server.grown());
        let again = server.take("runner", Some("box"), took.rev, Changes { rows: Vec::new(), upto: took.rev })?;
        anyhow::ensure!(again.applied == 0 && server.grown() == brought, "a batch that brings nothing: {}", server.grown());
        Ok(())
    }

    /// Consts-audit B3 (goal 9, plan 01a1a87b2cec step 5): the writer's group commit is self-clocked, the next commit once
    /// the last is durable with what came meanwhile as one, where a row waited up to `COMMIT_MS` (50 ms) for others, or for
    /// `COMMIT_ROWS` (2048). Do: a row to an idle writer, five times, each waited for until it's committed; then twenty rows
    /// sent one at a time while a job holds the writer (a slow commit's stand-in), the job let go, the rows waited for.
    /// Expect: each lone row committed on the wake that took it, waiting for nothing more (fast: no added wait), and counted
    /// before its sender is told (as `crate::store`'s groups: read on the writer's thread as it tells, and by a sender once
    /// told); the twenty in one commit, the writer woken for none of them (slow: a larger batch, of itself); every row read
    /// back.
    #[test]
    fn b3_a_lone_row_commits_at_once_and_rows_sent_during_a_commit_go_in_the_next_as_one() -> anyhow::Result<()> {
        let (_d, st) = scratch()?;
        st.sync()?;
        let committed = |st: &Store, commits: u64| {
            let until = Instant::now() + READ_WAIT;
            while st.report().commits < commits && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
            }
            st.report().commits >= commits
        };
        for i in 1..=5u64 {
            let commits = st.report().commits;
            st.send("a1", &said(&format!("m{i}"), i, "alone"));
            anyhow::ensure!(committed(&st, commits + 1), "row {i} never committed");
            st.sync()?;
            let (took, done) = (st.queue.took_on.load(Ordering::Relaxed), st.queue.committed_on.load(Ordering::Relaxed));
            anyhow::ensure!(done == took, "row {i}: taken on the writer's wake {took}, committed on {done}: it waited for more");
            let told = st.queue.counted_when_told.load(Ordering::Relaxed);
            anyhow::ensure!(told == commits + 1, "row {i}: its sender told with {told} commits counted, not its own ({})", commits + 1);
        }
        // A sender on a thread of its own, told its rows are in, finds them counted.
        let commits = st.report().commits;
        anyhow::ensure!(st.put_wait(rows::of("a1", &said("m0", 0, "waited for"), crate::util::now_ms())), "handed over");
        anyhow::ensure!(st.report().commits == commits + 1, "told, its commit not counted: {} of {}", st.report().commits, commits + 1);
        // A job holds the writer while twenty rows come, as a commit that took that long would.
        let (started_tx, started) = mpsc::channel();
        let (release, held) = mpsc::channel::<()>();
        let job = {
            let st = st.clone();
            std::thread::spawn(move || {
                st.run(move |_| {
                    let _ = started_tx.send(());
                    let _ = held.recv_timeout(READ_WAIT);
                    Ok(())
                })
            })
        };
        started.recv_timeout(READ_WAIT)?;
        let (commits, woke) = (st.report().commits, st.queue.woke.load(Ordering::Relaxed));
        for i in 6..=25u64 {
            st.send("a2", &said(&format!("m{i}"), i, "during a commit"));
        }
        let woke_during = st.queue.woke.load(Ordering::Relaxed).saturating_sub(woke);
        release.send(())?;
        job.join().map_err(|_| anyhow::anyhow!("the job's thread"))??;
        anyhow::ensure!(committed(&st, commits + 1), "the twenty never committed");
        st.sync()?;
        anyhow::ensure!(st.report().commits == commits + 1, "the twenty in {} commits", st.report().commits - commits);
        anyhow::ensure!(woke_during == 0, "the writer woke {woke_during} times while the rows came");
        let read = |s: &str| st.kept(s, 100).map(|k| k.map_or(0, |k| k.events.len()));
        anyhow::ensure!((read("a1")?, read("a2")?) == (6, 20), "every row read back");
        Ok(())
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
    /// as their inboxes hold) goes on while the queue holds no more than that, and past it waits until the writer lets
    /// them go; `/metrics` is told the queue may hold that much.
    #[test]
    fn g10_s5_a_sender_waits_only_past_what_it_may_let_wait() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        let q = store.queue();
        // One that may let nothing wait waits for whatever waits.
        q.pretend_waiting(1_000);
        anyhow::ensure!(!q.has_room_within(0) && q.has_room_within(1_000) && !q.has_room_within(999));
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
            let (out, passed) = crate::memory::queue::places::<(String, Update)>(&crate::memory::owners::CHANNELS, 4);
            let room = 16;
            let (sink, held) = tee_held(out, None, room);
            // Looked at weakly: a sender held here would keep the tee from ever ending.
            let way_in = match &sink {
                Sink::Stamped(q) => q.downgrade(),
                Sink::Bounded(_) | Sink::Channel(_) => anyhow::bail!("a tee's way in isn't a queue of updates taken in"),
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

    /// Goal 10, plan 01a1a87b2dae step 5: a runner's tee passes each update on with when its session sent it in, not
    /// when the tee got to pass it on: an update that waited in the tee, behind a sink with no room, still says when
    /// it was taken in, so its frame counts that wait as the runner's. Into a sink that keeps no such time (the
    /// server's), it passes on as it always did.
    #[test]
    fn g10_s5_a_tee_passes_each_update_on_with_when_it_was_taken_in() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (out, passed) = crate::memory::queue::places::<Taken>(&crate::memory::owners::CHANNELS, 1);
            let (sink, _held) = tee_held(out, None, 16);
            anyhow::ensure!(matches!(sink, Sink::Stamped(_)), "its way in notes when");
            sink.send(("a1".to_owned(), said("m1", 1, "fills what's after the tee"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            let before = Instant::now();
            sink.send(("a1".to_owned(), said("m2", 2, "waits in the tee"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            let sent = Instant::now();
            // The tee holds the second while what's after it has no room.
            let waited = Duration::from_millis(30);
            crate::rt::sleep(waited).await;
            let first = crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            let second = crate::rt::timeout(Duration::from_secs(5), passed.recv()).await??;
            anyhow::ensure!(first.session == "a1" && matches!(&second.update, Update::Events { events } if events.first().is_some_and(|e| e.id == "m2")), "in order: {first:?} {second:?}");
            anyhow::ensure!(before <= second.at && second.at <= sent, "taken in as it was sent, not as the tee passed it on");
            anyhow::ensure!(second.at.elapsed() >= waited, "its wait in the tee is counted from then");
            // Into a sink of plain updates, as before.
            let (plain, got) = async_channel::unbounded();
            let (sink, _held) = tee_held(plain, None, 16);
            sink.send(("a2".to_owned(), said("m3", 3, "plain"))).await.map_err(|_| anyhow::anyhow!("closed"))?;
            let (id, u) = crate::rt::timeout(Duration::from_secs(5), got.recv()).await??;
            anyhow::ensure!(id == "a2" && u == said("m3", 3, "plain"), "passed on as it came");
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

    /// Goal 11, plan 01a1a87b2e56 step 3; consts-audit B3: a burst from a thread of its own waits for the writer rather
    /// than piling up in memory: each put waits for the writer to take in what it sent, so what waits never goes past the
    /// one put under way; nothing is lost, and every event is kept, in order.
    #[test]
    fn g11_s3_b3_a_burst_waits_for_the_writer_and_loses_nothing() -> anyhow::Result<()> {
        let (_dir, store) = scratch()?;
        let text = "a reply of a few hundred words, ".repeat(40);
        let (mut most, mut one, n) = (0usize, 0usize, 2_000u64);
        for i in 0..=n {
            let rows = rows::of("burst", &said(&format!("m{i}"), i, &text), crate::util::now_ms());
            let bytes: usize = rows.iter().map(Row::bytes).sum();
            one = one.max(bytes);
            // What waits as this put goes in: what put_wait left, and this put's rows.
            most = most.max(store.queue().bytes() + bytes);
            assert!(store.put_wait(rows));
        }
        assert!(most <= one, "{most} bytes waited, past one put ({one})");
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
