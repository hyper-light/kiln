//! A machine's conversation store (`docs/conversations.md`, "The store"): one writer thread owns the
//! database; safe admission owns one immutable single-session batch and a durable ticket.
//! Independent projection sessions can share the [`COMMIT_MS`] window without coalescing
//! across admissions; Records/transcripts are ordered flush boundaries. Historical void
//! admission remains a migration surface and does not provide the new no-drop contract.
//! Only a successful selected-owner apply confirms durability. A failed batch stays with
//! the writer; later reads and barriers fail until the owner is torn down, without retrying an
//! unknown outcome. Reads go through the same thread, in turn with the writes.
//!
//! Every write gets the next number of the store (its revision), so another store can take what
//! came after a number it has ([`Store::changes`], [`Store::take`]): that is how a runner's store
//! reaches the server's (`super::feed`).
//!
//! The engine is behind [`Engine`]: the host's shared Amphora database ([`super::amphora`]).

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::rows::{self, EventRow, Row};
use super::batch::{AdmissionRefusal, ApplyFailure, Batch, Committed, Completion, PendingSubmission, Rejected, Ticket, GROUP_MAX, WORK_MAX, group_command_cost};
use event_listener::Listener as _;
use super::telemetry::{CompactView, Compacted, Meter, Pruned, StoreReport, Upkeeping};
use crate::harness::core::Update;
use crate::harness::session::Sink;

/// How long a row waits for others to share its commit.
pub const COMMIT_MS: u64 = 50;
/// Rows that make a commit at once, however soon.
pub const COMMIT_ROWS: usize = 2048;
/// Historical/control channel slots and default maximum safe input reservations. Safe batches
/// also have a retained-byte quota; typed Full returns input. Historical void writes still drop
/// on refusal until the session owner migrates those production callers.
pub const QUEUE: usize = 65_536;
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
    /// U05: one exact envelope; production Amphora composes its SQL and bound Records Tx.
    fn apply_batch(&mut self, batch: &Batch) -> Result<Committed, ApplyFailure> {
        if batch.records().is_some() {
            return Err(ApplyFailure::Definite(amphora_api::invalid("conversation batch", "this fixture engine cannot compose Records")));
        }
        self.apply(batch.rows()).map_err(|e| ApplyFailure::Unknown(amphora_api::Refusal::Unavailable(format!("{e:#}"))))?;
        Ok(Committed { conversation_rev: self.top(), records_rev: None })
    }
    /// Projection flush only: no Records, duplicate sessions or cross-admission coalescing.
    /// Production overrides this to return each envelope's exact transaction revision.
    fn apply_group(&mut self, batches: &[&Batch]) -> Result<Vec<Committed>, ApplyFailure> {
        match batches {
            [] => Ok(Vec::new()),
            [batch] => self.apply_batch(batch).map(|answer| vec![answer]),
            _ => Err(ApplyFailure::Definite(amphora_api::invalid("projection flush", "this fixture engine does not provide exact grouped revisions"))),
        }
    }

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
    /// Up to `limit` lines of `session`'s record from `source`, from line `from` on: `(n, line)`.
    /// U06: pages also have a byte bound; advance after the last number until an empty page.
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
    /// A bounded maintenance page can advance without deleting rows. Continue
    /// only on the same actor's next idle turn, preserving input responsiveness.
    fn maintenance_pending(&self)->bool { false }
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

type Job = Box<dyn FnOnce(Result<&mut dyn Engine, Arc<ApplyFailure>>) + Send>;

/// U17/U19: admission and the first apply failure share one gate. Once a batch fails, no new
/// message can add to the bounded channel or the writer's retained rows.
type Failure = Arc<Mutex<Option<Arc<ApplyFailure>>>>;

/// U05: admitted input custody, separately bounded from backend preparation scratch and RSS.
#[derive(Debug, Clone, Copy)]
pub struct AdmissionLimits { pub batches: usize, pub bytes: usize }
impl Default for AdmissionLimits {
    fn default() -> Self { Self { batches: QUEUE, bytes: 512 << 20 } }
}

#[derive(Default)]
struct Used { batches: usize, bytes: usize, closed: bool }
struct Capacity {
    limits: AdmissionLimits,
    used: Mutex<Used>,
    changed: event_listener::Event,
    meter: Arc<Meter>,
}

impl Capacity {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Permit, AdmissionRefusal> {
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        if used.closed { return Err(AdmissionRefusal::Closed); }
        if bytes > self.limits.bytes { return Err(AdmissionRefusal::TooLarge { size: bytes, limit: self.limits.bytes }); }
        let next = used.bytes.checked_add(bytes).filter(|n| *n <= self.limits.bytes);
        if used.batches >= self.limits.batches || next.is_none() {
            return Err(AdmissionRefusal::Full { limit_batches: self.limits.batches, limit_bytes: self.limits.bytes });
        }
        used.bytes = next.ok_or(AdmissionRefusal::TooLarge { size: usize::MAX, limit: self.limits.bytes })?;
        used.batches += 1;
        self.meter.admitted(bytes);
        Ok(Permit { capacity: Arc::clone(self), bytes, wake: true })
    }
    fn close(&self) {
        self.used.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
        self.changed.notify(usize::MAX);
    }
}

/// Moves with accepted input through queue, pending, apply and terminal failure custody.
/// It does not contain the Store sender and cannot keep its own writer alive.
pub(crate) struct Permit { capacity: Arc<Capacity>, bytes: usize, wake: bool }
impl Permit { pub(crate) fn abandoned(&self) { self.capacity.meter.abandoned_failure(); } }
impl Drop for Permit {
    fn drop(&mut self) {
        let mut used = self.capacity.used.lock().unwrap_or_else(PoisonError::into_inner);
        used.batches = used.batches.saturating_sub(1);
        used.bytes = used.bytes.saturating_sub(self.bytes);
        self.capacity.meter.released(self.bytes);
        drop(used);
        if self.wake { self.capacity.changed.notify(usize::MAX); }
    }
}
struct WriterLife(Arc<Capacity>);
impl Drop for WriterLife { fn drop(&mut self) { self.0.close(); } }
struct Envelope { batch: Batch, completion: Completion, permit: Permit }


enum Msg {
    /// Historical migration surface only; production callers still need step6 migration.
    Rows(Vec<Row>),
    Batch(Envelope),
    Barrier(Completion, Permit),
    /// Run on the engine, between commits (whatever was sent before it is committed first).
    Run(Job),
    /// Commit what was sent before, let go of the file ([`Engine::release`]) and stop.
    Close(mpsc::Sender<anyhow::Result<()>>),
}

impl Msg {
    fn cancel_reservation(&mut self) {
        match self {
            Msg::Batch(envelope) => envelope.permit.wake = false,
            Msg::Barrier(_, permit) => permit.wake = false,
            Msg::Rows(_) | Msg::Run(_) | Msg::Close(_) => {}
        }
    }
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
    failure: Failure,
    capacity: Arc<Capacity>,
    identity: Arc<()>,
    id: Arc<str>,
    meter: Arc<Meter>,
    watchers: Watchers,
    /// Sessions that ended at or after this (unix ms) keep their records ([`Store::hold_prune`]).
    hold: Arc<AtomicI64>,
    maintenance: Option<super::retention::Authority>,
    maintenance_due: Arc<AtomicBool>,
}

impl Store {
    /// The store, its writer thread started on `engine`, looking after its file as `upkeep` says.
    pub fn start(engine: Box<dyn Engine>, upkeep: Upkeep) -> anyhow::Result<Self> {
        Self::start_with_limits(engine, upkeep, AdmissionLimits::default())
    }

    /// Limits bound safe admitted input, not historical migration calls or engine RSS.
    pub fn start_with_limits(engine: Box<dyn Engine>, upkeep: Upkeep, limits: AdmissionLimits) -> anyhow::Result<Self> {
        Self::start_initialized(engine, upkeep, limits, i64::MAX, None)
    }

    /// The existing archive owner installs its initial hold before the writer can run.
    /// Ordinary constructors retain their unbounded initial cutoff and original upkeep.
    #[cfg(test)]
    pub(crate) fn start_with_initial_hold(engine: Box<dyn Engine>, upkeep: Upkeep, initial_hold: i64) -> anyhow::Result<Self> {
        Self::start_initialized(engine, upkeep, AdmissionLimits::default(), initial_hold, None)
    }

    pub(crate) fn start_with_maintenance(engine: Box<dyn Engine>, upkeep: Upkeep, initial_hold: i64, authority: Option<super::retention::Authority>) -> anyhow::Result<Self> {
        Self::start_initialized(engine, upkeep, AdmissionLimits::default(), initial_hold, authority)
    }

    pub(crate) fn maintenance(&self) -> Option<super::retention::Authority> { self.maintenance.clone() }

    /// A coalesced wake hint only; the engine rechecks the durable authority and
    /// exact receipt. No user event or destructive permission lives in this bit.
    pub(crate) fn wake_maintenance(&self) { self.maintenance_due.store(true,Ordering::Release); }

    fn start_initialized(engine: Box<dyn Engine>, upkeep: Upkeep, limits: AdmissionLimits, initial_hold: i64, maintenance: Option<super::retention::Authority>) -> anyhow::Result<Self> {
        anyhow::ensure!(limits.batches > 0 && limits.bytes > 0, "conversation admission limits must be positive");
        let (tx, rx) = mpsc::sync_channel::<Msg>(QUEUE.min(limits.batches));
        let id: Arc<str> = engine.id().into();
        let watchers: Watchers = Arc::default();
        let meter: Arc<Meter> = Arc::default();
        let hold = Arc::new(AtomicI64::new(initial_hold));
        let maintenance_due = Arc::new(AtomicBool::new(false));
        let failure: Failure = Arc::default();
        let capacity = Arc::new(Capacity { limits, used: Mutex::default(), changed: event_listener::Event::new(), meter: meter.clone() });
        let identity = Arc::new(());
        let life = WriterLife(capacity.clone());
        let (w, m, h, f, due) = (watchers.clone(), meter.clone(), Arc::clone(&hold), failure.clone(), maintenance_due.clone());
        std::thread::Builder::new().name("kiln-convo".into()).spawn(move || { writer(engine, &rx, WriterContext { watchers: &w, upkeep, meter: &m, hold: &h, maintenance_due: &due, failure: &f, capacity: &life.0 }); drop(life); })?;
        Ok(Self { tx, failure, capacity, identity, id, meter, watchers, hold, maintenance, maintenance_due })
    }

    /// Binding token only: no sender/Database clone is captured by an admitted Records batch.
    pub(crate) fn identity(&self) -> Arc<()> { self.identity.clone() }

    /// U05: bounded admission into this actor. Success returns a pending durable ticket;
    /// refusal returns the exact unaccepted input. No application/flush runs under the gate.
    pub fn try_admit(&self, batch: Batch) -> Result<Ticket, Rejected<Batch>> {
        self.try_admit_inner(batch, true)
    }

    fn try_admit_inner(&self, batch: Batch, count: bool) -> Result<Ticket, Rejected<Batch>> {
        let refused = |input, reason| {
            if count { self.meter.refused(matches!(reason, AdmissionRefusal::Full { .. })); }
            Rejected { input, reason }
        };
        if batch.records().is_some_and(|records| !records.belongs_to(&self.identity)) {
            return Err(refused(batch, AdmissionRefusal::Invalid("Records and conversation batch belong to different host actors".into())));
        }
        let failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(failure) = failure.as_ref() {
            return Err(refused(batch, AdmissionRefusal::Fenced(failure.clone())));
        }
        let permit = match self.capacity.reserve(batch.retained_bytes()) {
            Ok(permit) => permit,
            Err(reason) => return Err(refused(batch, reason)),
        };
        let (ticket, completion) = Ticket::pending();
        let message = Msg::Batch(Envelope { batch: batch.shared(), completion, permit });
        match self.tx.try_send(message) {
            Ok(()) => Ok(ticket),
            Err(error) => {
                let (mut message, reason) = match error {
                    mpsc::TrySendError::Full(message) => (message, AdmissionRefusal::Full { limit_batches: self.capacity.limits.batches, limit_bytes: self.capacity.limits.bytes }),
                    mpsc::TrySendError::Disconnected(message) => (message, AdmissionRefusal::Closed),
                };
                message.cancel_reservation();
                // Suppress only this rolled-back reservation's wake. The admission gate is
                // still held: another producer cannot observe the transient reservation.
                drop(message);
                Err(refused(batch, reason))
            }
        }
    }

    /// FIFO settlement/durability frontier, not a substitute for an effect's own ticket.
    /// A frame that makes no rows can wait its preceding ticket or this explicit barrier;
    /// a settled definite refusal is still a failed attempt and must be read from its ticket.
    pub fn barrier(&self) -> Result<Ticket, AdmissionRefusal> {
        let result = (|| {
            let failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(cause) = failure.as_ref() { return Err(AdmissionRefusal::Fenced(cause.clone())); }
            let permit = self.capacity.reserve(4096)?;
            let (ticket, completion) = Ticket::pending();
            match self.tx.try_send(Msg::Barrier(completion, permit)) {
                Ok(()) => Ok(ticket),
                Err(error) => {
                    let (mut message, reason) = match error {
                        mpsc::TrySendError::Full(message) => (message, AdmissionRefusal::Full { limit_batches: self.capacity.limits.batches, limit_bytes: self.capacity.limits.bytes }),
                        mpsc::TrySendError::Disconnected(message) => (message, AdmissionRefusal::Closed),
                    };
                    message.cancel_reservation();
                    drop(message);
                    Err(reason)
                }
            }
        })();
        if let Err(reason) = &result { self.meter.refused(matches!(reason, AdmissionRefusal::Full { .. })); }
        result
    }

    /// Waits without consuming Prepared input on cancellation/timeout. A successful handoff
    /// installs its ticket in the caller-owned slot before returning, without another await.
    pub async fn admit_until(&self, pending: &mut PendingSubmission, until: Instant) -> Result<(), AdmissionRefusal> {
        let result = super::batch::wait_for_admission(pending, until, &self.capacity.changed, |batch| self.try_admit_inner(batch, false)).await;
        if let Err(reason) = &result { self.meter.refused(matches!(reason, AdmissionRefusal::Full { .. })); }
        result
    }

    /// Dedicated blocking callers only; never wait for room on the shared reactor.
    pub fn admit_blocking_until(&self, pending: &mut PendingSubmission, until: Instant) -> Result<(), AdmissionRefusal> {
        loop {
            if pending.ticket.is_some() { return Ok(()); }
            if Instant::now() >= until { self.meter.refused(false); return Err(AdmissionRefusal::Deadline); }
            let changed = self.capacity.changed.listen();
            match self.admit_pending(pending) {
                Ok(()) => return Ok(()),
                Err(AdmissionRefusal::Full { .. }) => {}
                Err(reason) => { self.meter.refused(false); return Err(reason); }
            }
            if changed.wait_deadline(until).is_none() {
                self.meter.refused(false);
                return Err(AdmissionRefusal::Deadline);
            }
        }
    }

    fn admit_pending(&self, pending: &mut PendingSubmission) -> Result<(), AdmissionRefusal> {
        let batch = pending.batch.take().ok_or_else(|| AdmissionRefusal::Invalid("the submission has no prepared input".into()))?;
        match self.try_admit_inner(batch, false) {
            Ok(ticket) => { pending.ticket = Some(ticket); Ok(()) }
            Err(Rejected { input, reason }) => { pending.batch = Some(input); Err(reason) }
        }
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

    /// HISTORICAL migration surface. Void admission can lose updates; production step6 must
    /// replace every caller with sealed batches/tickets before no-drop can be claimed.
    /// What `update` of `session` makes, to be committed with the next batch.
    pub fn send(&self, session: &str, update: &Update) {
        self.put(rows::of(session, update, crate::util::now_ms()));
    }

    /// HISTORICAL migration surface, without the new safe admission contract.
    /// Rows to be committed with the next batch. Doesn't wait for channel room: when full,
    /// stopped or fenced by a failed batch, they're rejected and counted. Admission shares a
    /// brief mutex with the first-failure latch; no engine work runs under that mutex.
    pub fn put(&self, rows: Vec<Row>) {
        if rows.is_empty() {
            return;
        }
        if let Err(e) = self.enqueue(Msg::Rows(rows), false) {
            let n = self.meter.dropped();
            if (n - 1).is_power_of_two() {
                tracing::warn!(dropped = n, error = %e, "conversation store: updates couldn't be admitted");
            }
        }
    }

    /// HISTORICAL migration surface; false consumes rejected rows and has no typed receipt.
    /// Rows to be committed with the next batch, waiting up to `READ_WAIT` for room: false when
    /// they couldn't be handed over. True is admission, never a commit receipt; false after a
    /// failed batch doesn't authorize retrying that batch's unknown outcome. Blocking.
    pub fn put_wait(&self, rows: Vec<Row>) -> bool {
        rows.is_empty() || self.enqueue(Msg::Rows(rows), true).is_ok()
    }

    /// Run `f` on the engine once what was sent before is committed. Blocking, for `READ_WAIT` at
    /// most.
    fn run<T: Send + 'static>(&self, f: impl FnOnce(&mut dyn Engine) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        let asked = Instant::now();
        self.enqueue(Msg::Run(Box::new(move |e| {
            let _ = rtx.send(e.map_err(|e| anyhow::anyhow!("{e}")).and_then(f));
        })), true)?;
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

    /// Lines of `session`'s record from `source`, from line `from` on. Blocking. Pages may be
    /// shorter than `limit` because of their byte bound; an empty page alone means EOF.
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

    /// FIFO settlement barrier. Prior successful safe batches are durable, but a typed definite
    /// refusal is a settled failed attempt: each caller must inspect its own ticket. Unknown or
    /// unobservable legacy failures remain errors/fences. This does not certify a refused batch
    /// or a frame with no rows. Blocking; rejected historical put input is outside the barrier.
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

impl Store {
    /// Admission alone isn't a commit receipt. The gate prevents accepted rows from growing
    /// after failure; the existing channel and pending batch retain everything already admitted.
    fn enqueue(&self, mut msg: Msg, wait: bool) -> anyhow::Result<()> {
        let until = Instant::now() + READ_WAIT;
        loop {
            {
                let failure = self.failure.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(e) = failure.as_ref() {
                    anyhow::bail!("{e}");
                }
                match self.tx.try_send(msg) {
                    Ok(()) => return Ok(()),
                    Err(mpsc::TrySendError::Disconnected(_)) => anyhow::bail!("the conversation store has stopped"),
                    Err(mpsc::TrySendError::Full(back)) => msg = back,
                }
            }
            if !wait || Instant::now() >= until {
                anyhow::bail!("the conversation store is full");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Settlement<'a> { failure: &'a Failure, capacity: &'a Capacity, meter: &'a Meter, watchers: &'a Watchers }
struct WriterContext<'a> { watchers: &'a Watchers, upkeep: Upkeep, meter: &'a Meter, hold: &'a AtomicI64, maintenance_due: &'a AtomicBool, failure: &'a Failure, capacity: &'a Capacity }

fn block(envelope: Envelope, cause: &Arc<ApplyFailure>, retained: &mut Vec<Envelope>) {
    envelope.completion.finish(Err(Arc::new(ApplyFailure::Blocked(cause.clone()))));
    retained.push(envelope);
}

/// A flush retains each sealed envelope's meaning. A Records/Record envelope is a boundary;
/// grouped projections contain each session once and fit the aggregate prepared-input bound.
fn settle(engine: &mut Box<dyn Engine>, pending: &mut Vec<Envelope>, retained: &mut Vec<Envelope>, failed: &mut Option<Arc<ApplyFailure>>, context: &Settlement<'_>) {
    let Settlement { failure, capacity, meter, watchers } = context;
    if pending.is_empty() { return; }
    if let Some(cause) = failed.as_ref() {
        for envelope in std::mem::take(pending) { block(envelope, cause, retained); }
        return;
    }
    let group = std::mem::take(pending);
    let result = if let Some(envelope) = group.first().filter(|_| group.len() == 1) {
        engine.apply_batch(&envelope.batch).map(|answer| vec![answer])
    } else {
        let batches = group.iter().map(|e| &e.batch).collect::<Vec<_>>();
        engine.apply_group(&batches)
    };
    let result = result.and_then(|answers| {
        if answers.len() == group.len() { Ok(answers) }
        else { Err(ApplyFailure::Unknown(amphora_api::Refusal::Unavailable("conversation backend returned the wrong number of durable answers".into()))) }
    });
    match result {
        Ok(answers) => {
            meter.committed(group.iter().map(|e| e.batch.rows().len()).sum());
            for (Envelope { batch, completion, permit }, answer) in group.into_iter().zip(answers) {
                // The selected owner already returned its durable answer. No client wait/drop
                // can release this permit before that point.
                drop(batch);
                drop(permit);
                completion.finish(Ok(answer));
            }
            notify(watchers);
        }
        Err(error @ ApplyFailure::Definite(_)) => {
            let cause = Arc::new(error);
            for Envelope { batch, completion, permit } in group {
                // A definitely failed attempt is settled, but its input stays in the ticket
                // until take_failed or explicit abandonment of this terminal result.
                completion.refused(batch, permit, cause.clone());
            }
        }
        Err(error) => {
            let cause = Arc::new(error);
            *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(cause.clone());
            capacity.changed.notify(usize::MAX);
            for envelope in group {
                envelope.completion.finish(Err(cause.clone()));
                retained.push(envelope);
            }
            *failed = Some(cause);
        }
    }
}

fn boundary(batch: &Batch) -> bool {
    batch.records().is_some() || batch.rows().iter().any(|row| matches!(row, Row::Record(_) | Row::RecordReset { .. }))
}

fn shares_flush(pending: &[Envelope], batch: &Batch) -> bool {
    !pending.iter().any(|e| e.batch.session() == batch.session())
        && pending.iter().try_fold(batch.prepared_bytes(), |n, e| n.checked_add(e.batch.prepared_bytes())).is_some_and(|n| n <= GROUP_MAX)
        && group_command_cost(pending.iter().map(|e| &e.batch).chain(std::iter::once(batch))).is_some_and(|n| n <= WORK_MAX)
}

/// The writer: rows wait at most `COMMIT_MS` (or until `COMMIT_ROWS`) to share a commit; a job
/// commits what waits first, so it sees everything sent before it.
fn writer(mut engine: Box<dyn Engine>, rx: &mpsc::Receiver<Msg>, context: WriterContext<'_>) {
    let WriterContext { watchers, upkeep, meter, hold, maintenance_due, failure, capacity } = context;
    let settlement = Settlement { watchers, meter, failure, capacity };
    let keep_closed = upkeep.keep_closed;
    // The hold on letting records go as it last looked: one moved later lets more go, so it looks
    // again.
    let mut held = hold.load(Ordering::Acquire);
    let mut pending: Vec<Row> = Vec::new();
    let mut failed: Option<Arc<ApplyFailure>> = None;
    let mut exact: Vec<Envelope> = Vec::new();
    let mut retained: Vec<Envelope> = Vec::new();
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
    let commit = |engine: &mut Box<dyn Engine>, pending: &mut Vec<Row>, failed: &mut Option<Arc<ApplyFailure>>| {
        if pending.is_empty() {
            return;
        }
        let rows = rows::coalesce(std::mem::take(pending));
        match engine.apply(&rows) {
            Ok(()) => {
                meter.committed(rows.len());
                notify(watchers);
            }
            Err(e) => {
                let reason = Arc::new(ApplyFailure::Unknown(amphora_api::Refusal::Unavailable(format!("the conversation store's historical pending batch failed; its outcome may be unknown and it wasn't retried: {e:#}"))));
                *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason.clone());
                capacity.changed.notify(usize::MAX);
                tracing::warn!(error = %reason, rows = rows.len(), "conversation store: a failed batch is retained; new work is refused");
                // Preserve exactly what apply saw. No re-coalescing, later write or maintenance
                // may supersede it while its outcome is unknown.
                *pending = rows;
                *failed = Some(reason);
            }
        }
    };
    loop {
        if let Some(e) = failed.as_ref() {
            // Admission is closed under the same gate that latched this failure, so only the
            // finite existing channel remains. Retain its rows after the failed batch; queued
            // jobs receive failure without touching the engine. No retry, poll or upkeep runs.
            let got = rx.recv();
            if got.is_ok() { capacity.changed.notify(usize::MAX); }
            match got {
                Ok(Msg::Rows(rows)) => pending.extend(rows),
                Ok(Msg::Batch(envelope)) => block(envelope, e, &mut retained),
                Ok(Msg::Barrier(completion, _permit)) => completion.finish(Err(Arc::new(ApplyFailure::Blocked(e.clone())))),
                Ok(Msg::Run(f)) => f(Err(e.clone())),
                // Its file let go of all the same, for whoever asked to move it.
                Ok(Msg::Close(reply)) => {
                    let _ = reply.send(engine.release());
                    return;
                }
                Err(_) => return,
            }
            continue;
        }
        let idle = if squeezing || looking || compacting.is_some() { SQUEEZE_EVERY } else { look_at.saturating_duration_since(Instant::now()).max(SQUEEZE_EVERY).min(LOOK_FILE) };
        let wait = since.map_or(idle, |t| Duration::from_millis(COMMIT_MS).saturating_sub(t.elapsed()));
        let got = rx.recv_timeout(wait);
        // Real channel room, including a historical/control dequeue, wakes safe waiters.
        if got.is_ok() { capacity.changed.notify(usize::MAX); }
        let began = Instant::now();
        match got {
            Ok(Msg::Rows(rows)) => {
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                pending.extend(rows);
                since.get_or_insert_with(Instant::now);
                if failed.is_none() && pending.len() >= COMMIT_ROWS {
                    commit(&mut engine, &mut pending, &mut failed);
                    since = None;
                }
            }
            Ok(Msg::Batch(envelope)) => {
                // Historical and safe admissions share FIFO ordering but never coalesce.
                commit(&mut engine, &mut pending, &mut failed);
                if let Some(cause) = failed.as_ref() {
                    block(envelope, cause, &mut retained);
                    continue;
                }
                let immediate = boundary(&envelope.batch);
                if immediate || !shares_flush(&exact, &envelope.batch) {
                    settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                    since = None;
                }
                if let Some(cause) = failed.as_ref() {
                    block(envelope, cause, &mut retained);
                    continue;
                }
                exact.push(envelope);
                since.get_or_insert_with(Instant::now);
                if immediate || exact.iter().map(|e| e.batch.rows().len()).sum::<usize>() >= COMMIT_ROWS {
                    settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                    since = None;
                }
            }
            Ok(Msg::Barrier(completion, _permit)) => {
                commit(&mut engine, &mut pending, &mut failed);
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                since = None;
                match failed.as_ref() {
                    Some(cause) => completion.finish(Err(Arc::new(ApplyFailure::Blocked(cause.clone())))),
                    None => completion.finish(Ok(Committed { conversation_rev: engine.top(), records_rev: None })),
                }
            }
            Ok(Msg::Run(f)) => {
                commit(&mut engine, &mut pending, &mut failed);
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                since = None;
                match failed.as_ref() {
                    Some(e) => f(Err(e.clone())),
                    None => {
                        f(Ok(engine.as_mut()));
                        meter.ran();
                    }
                }
            }
            Ok(Msg::Close(reply)) => {
                commit(&mut engine, &mut pending, &mut failed);
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                // Its files closed before it says so: whoever moves them next finds them let go of.
                let released = engine.release();
                drop(engine);
                let _ = reply.send(released);
                return;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let had = !pending.is_empty() || !exact.is_empty();
                commit(&mut engine, &mut pending, &mut failed);
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                since = None;
                if failed.is_some() {
                    continue;
                }
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
                } else if !had && (looking || Instant::now() >= look_at || hold.load(Ordering::Acquire) > held || maintenance_due.load(Ordering::Acquire)) {
                    held = hold.load(Ordering::Acquire);
                    maintenance_due.store(false,Ordering::Release);
                    let gone = match keep_closed {
                        Some(keep) => engine.prune_some(crate::util::now_ms().saturating_sub(i64::try_from(keep.as_millis()).unwrap_or(i64::MAX)).min(held), PRUNE_BATCH),
                        None => Ok(0),
                    };
                    match gone {
                        Ok(0) if engine.maintenance_pending() => looking=true,
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
                commit(&mut engine, &mut pending, &mut failed);
                settle(&mut engine, &mut exact, &mut retained, &mut failed, &settlement);
                return;
            }
        }
        if failed.is_some() {
            continue;
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
    input: async_channel::WeakSender<(String, Update)>,
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
pub fn tee_held(sink: Sink, store: Option<Store>, room: usize) -> (Sink, Held) {
    let (tx, rx) = async_channel::bounded::<(String, Update)>(room);
    let keeping = Arc::new(Mutex::new(store.map_or_else(|| Keeping::Waiting(Vec::new()), Keeping::Store)));
    let held = Held { keeping: keeping.clone(), input: tx.downgrade() };
    crate::rt::spawn(async move {
        while let Ok((id, u)) = rx.recv().await {
            // Kept before it goes on, under the lock a taker waits on.
            match &mut *keeping.lock().unwrap_or_else(PoisonError::into_inner) {
                Keeping::Waiting(rows) => rows.extend(rows::of(&id, &u, crate::util::now_ms())),
                Keeping::Store(st) => st.send(&id, &u),
                Keeping::Gone => {}
            }
            if sink.send((id, u)).await.is_err() {
                break;
            }
        }
    });
    (tx, held)
}

/// A store on a new database in a temporary folder, for tests.
#[cfg(test)]
pub fn scratch() -> anyhow::Result<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let host = crate::store::Store::open(&dir.path().join("kiln.db"))?;
    let store = super::from_database(host.database(), &dir.path().join(super::FILE), Upkeep::default())?;
    Ok((dir, store))
}

/// A store whose writer has stopped: everything asked of it fails at once, for tests.
#[cfg(test)]
pub fn stopped() -> Store {
    let (tx, _) = mpsc::sync_channel(1);
    let meter: Arc<Meter> = Arc::default();
    let capacity = Arc::new(Capacity { limits: AdmissionLimits::default(), used: Mutex::new(Used { closed: true, ..Used::default() }), changed: event_listener::Event::new(), meter: meter.clone() });
    Store { tx, failure: Arc::default(), capacity, identity: Arc::new(()), id: "stopped".into(), meter, watchers: Arc::default(), hold: Arc::new(AtomicI64::new(i64::MAX)), maintenance: None, maintenance_due: Arc::new(AtomicBool::new(false)) }
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
        let host = crate::store::Store::open(&dir.path().join("kiln.db"))?;
        let mut engine = super::super::amphora::Amphora::from_database(host.database())?;
        let now = crate::util::now_ms();
        let held = now - 3_600_000;
        let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: now };
        let line = |s: &str, n: u64| Row::Record(Box::new(rows::RecordRow { session: s.into(), source: "cc".into(), n, end: n + 1, line: "{}".into(), kept: None }));
        // Rows already exist at startup. No caller-side hold update can protect them
        // after this actor starts: its initial hold must precede the first prune turn.
        engine.apply(&[session("before"), line("before", 0), Row::Closed { session: "before".into(), at: held - 60_000 },
            session("held"), line("held", 0), Row::Closed { session: "held".into(), at: held }])?;
        let store = Store::start_with_initial_hold(Box::new(engine), Upkeep { keep_closed: Some(Duration::from_secs(60)), compact: None }, held)?;
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

    /// U17/U19: a held SQL owner cannot be mistaken for a successful flush. Once apply fails,
    /// even a later repair doesn't turn a new empty barrier into success or replay the batch.
    #[test]
    fn u17_u19_a_failed_held_flush_refuses_later_barriers_and_never_retries() -> anyhow::Result<()> {
        let host = crate::store::Store::in_memory()?;
        let database = host.database();
        let engine = super::super::amphora::Amphora::from_database(database.clone())?;
        let store = Store::start(Box::new(engine), Upkeep::default())?;
        let committed = store.watch();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held = std::thread::spawn(move || -> anyhow::Result<()> {
            database.call(Instant::now() + READ_WAIT, move |c| {
                // A real engine rejection, before any timeline row is written. The held call
                // publishes this value only when released; apply must wait behind it.
                c.execute("INSERT INTO convo_meta(key,value) VALUES ('rev',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", vec![amphora_api::Value::Text(u64::MAX.to_string())])?;
                entered_tx.send(())?;
                release_rx.recv_timeout(READ_WAIT)?;
                Ok(())
            })?;
            Ok(())
        });
        entered_rx.recv_timeout(READ_WAIT)?;
        store.send("held", &said("m1", 1, "must not disappear behind a successful flush"));
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let (answer_tx, answer_rx) = mpsc::channel();
        let flushing = store.clone();
        let waiter = std::thread::spawn(move || {
            let _ = waiting_tx.send(());
            let _ = answer_tx.send(flushing.sync());
        });
        waiting_rx.recv_timeout(READ_WAIT)?;
        assert!(matches!(answer_rx.try_recv(), Err(mpsc::TryRecvError::Empty)), "a held owner isn't a completed flush");
        release_tx.send(())?;
        held.join().map_err(|_| anyhow::anyhow!("the held SQL call's thread failed"))??;
        let failed = answer_rx.recv_timeout(READ_WAIT)?;
        waiter.join().map_err(|_| anyhow::anyhow!("the flush waiter's thread failed"))?;
        let error = failed.err().ok_or_else(|| anyhow::anyhow!("a failed batch was reported as committed"))?;
        assert!(format!("{error:#}").contains("conversation revision overflow"));
        assert!(matches!(committed.try_recv(), Err(async_channel::TryRecvError::Empty)), "failed apply doesn't publish a commit");
        assert_eq!(store.report().commits, 0);

        // Repair the modeled cause through the same canonical SQL owner. The conversation
        // wrapper still cannot decide whether an arbitrary apply error was a definite rollback.
        host.now(|c| {
            c.execute("UPDATE convo_meta SET value='0' WHERE key='rev'", [])?;
            Ok(())
        })?;
        assert!(store.sync().is_err(), "an empty barrier must preserve the earlier failure");
        assert!(store.kept("held", 10).is_err(), "a dependent read must not run past the failed write");
        assert!(!store.put_wait(rows::of("later", &said("m2", 2, "not admitted"), 2)), "failure closes admission without accepting more rows");
        let count = host.now(|c| {
            let mut query = c.prepare("SELECT count(*) FROM convo_events")?;
            let mut rows = query.query([])?;
            let row = rows.next()?.ok_or_else(|| anyhow::anyhow!("missing event count"))?;
            Ok(row.get::<_, i64>(0)?)
        })?;
        assert_eq!(count, 0, "repair cannot cause a blind retry or a later write");
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
    #[test]
    fn u05_room_cancellation_and_ticket_drop_keep_applying_input_in_custody() -> anyhow::Result<()> {
        let host = crate::store::Store::in_memory()?;
        let database = host.database();
        let engine = super::super::amphora::Amphora::from_database(database.clone())?;
        let store = Store::start_with_limits(Box::new(engine), Upkeep::default(), AdmissionLimits { batches: 1, bytes: 1 << 20 })?;
        // Scoped ownership drops the release sender before joining on every error/unwind.
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let held = scope.spawn(move || database.call(Instant::now() + READ_WAIT, move |_connection| {
                entered_tx.send(())?;
                // Only explicit release or sender drop ends this original callback.
                // The public Database::call deadline remains READ_WAIT.
                release_rx.recv()?;
                Ok(())
            }));
            entered_rx.recv_timeout(READ_WAIT)?;
            let batch = Batch::from_update("s", &said("m1", 1, "retained"), 1)?.ok_or_else(|| anyhow::anyhow!("missing projection"))?;
            let ticket = store.try_admit(batch).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
            let next = Batch::from_update("later", &said("m2", 2, "caller owns this"), 2)?.ok_or_else(|| anyhow::anyhow!("missing later projection"))?;
            let refused = store.try_admit(next).err().ok_or_else(|| anyhow::anyhow!("the applying batch released its count reservation"))?;
            anyhow::ensure!(matches!(refused.reason, AdmissionRefusal::Full { .. }) && refused.input.session() == "later", "typed Full did not return its input");
            anyhow::ensure!(ticket.try_result().is_none() && store.report().admission.batches == 1, "a held owner was reported durable or released custody");
            let waiting_store = store.clone();
            let waiting_ticket = ticket.clone();
            let pending = crate::rt::block_on(async move {
                match waiting_ticket.wait_until(Instant::now()).await {
                    Err(_elapsed) => {}
                    Ok(Ok(committed)) => anyhow::bail!("the held batch produced a durable receipt before release: {committed:?}"),
                    Ok(Err(cause)) => anyhow::bail!("the held batch answered before release: {cause}"),
                }
                let mut pending = PendingSubmission::new(refused.input);
                let mut waiting = Box::pin(waiting_store.admit_until(&mut pending, Instant::now() + READ_WAIT));
                anyhow::ensure!(futures::poll!(waiting.as_mut()).is_pending(), "full store did not hold admission");
                drop(waiting);
                anyhow::ensure!(pending.input().is_some() && pending.ticket().is_none(), "cancelling room wait consumed the input");
                Ok(pending)
            })?;
            anyhow::ensure!(pending.into_input().is_some(), "caller could not recover unadmitted input");
            drop(ticket);
            anyhow::ensure!(store.report().admission.batches == 1, "dropping the final caller ticket cancelled applying input");
            release_tx.send(())?;
            held.join().map_err(|_| anyhow::anyhow!("held owner thread failed"))??;
            store.sync()?;
            anyhow::ensure!(store.report().admission.batches == 0 && store.kept("s", 10)?.is_some_and(|kept| kept.events.len() == 1), "durable completion did not release input or retain the event");
            Ok(())
        })
    }

    #[test]
    fn u05_definite_refusal_returns_input_without_poisoning_unrelated_settlement() -> anyhow::Result<()> {
        use crate::records::SessionBatches as _;
        use kiln_seams::records::{Cond, Tx, Write};
        let host = crate::store::Store::in_memory()?;
        let storage = crate::records::SessionStorage::from_host_database(host.database(), "host", &[], Upkeep::default())?;
        let (_, committer) = storage.namespace("session-test", None)?;
        let store = storage.conversations();
        let transaction = || Tx { conds: vec![Cond::Absent { family: "inbox".into(), key: b"same".to_vec() }], writes: vec![Write::Put { family: "inbox".into(), key: b"same".to_vec(), value: b"once".to_vec() }] };
        let rows = rows::try_of("s", &said("m1", 1, "first"), 1)?;
        let first = committer.batch("s".into(), rows, transaction()).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let first = committer.try_admit(first).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        futures::executor::block_on(first.wait()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let rows = rows::try_of("s", &said("m2", 2, "refused"), 2)?;
        let stale = committer.batch("s".into(), rows, transaction()).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let stale = committer.try_admit(stale).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        let answer = futures::executor::block_on(stale.wait());
        anyhow::ensure!(matches!(answer, Err(cause) if matches!(cause.as_ref(), ApplyFailure::Definite(amphora_api::Refusal::Conflict { .. }))), "known CAS refusal was not typed");
        anyhow::ensure!(store.report().admission.batches == 1, "failed input lost its reservation before take-back");
        let other = Batch::from_update("other", &said("ok", 1, "unrelated"), 1)?.ok_or_else(|| anyhow::anyhow!("missing unrelated projection"))?;
        let other = store.try_admit(other).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        futures::executor::block_on(other.wait()).map_err(|e| anyhow::anyhow!("{e}"))?;
        store.sync()?; // Settlement is not success of the failed attempt above.
        let recovered = stale.take_failed().ok_or_else(|| anyhow::anyhow!("definite failed input was not returned"))?;
        anyhow::ensure!(recovered.session() == "s" && recovered.rows().len() == 1 && stale.take_failed().is_none(), "failed input ownership did not transfer exactly once");
        anyhow::ensure!(store.report().admission.batches == 0 && store.kept("s", 10)?.is_some_and(|kept| kept.events.len() == 1), "refused SQL was published or its quota never retired");
        Ok(())
    }

    #[test]
    fn u05_unknown_attempt_fences_barriers_and_keeps_input_after_ticket_drop() -> anyhow::Result<()> {
        let host = crate::store::Store::in_memory()?;
        let engine = super::super::amphora::Amphora::from_database(host.database())?;
        let store = Store::start(Box::new(engine), Upkeep::default())?;
        host.now(|c| {
            let changed = c.execute("INSERT INTO convo_meta(key,value) VALUES ('rev',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", vec![amphora_api::Value::Text(u64::MAX.to_string())])?;
            anyhow::ensure!(changed == 1, "the overflow fixture did not install its revision");
            Ok(())
        })?;
        let batch = Batch::from_update("s", &said("m1", 1, "must not be blindly retried"), 1)?.ok_or_else(|| anyhow::anyhow!("missing projection"))?;
        let ticket = store.try_admit(batch).map_err(|e| anyhow::anyhow!("{:?}", e.reason))?;
        anyhow::ensure!(matches!(futures::executor::block_on(ticket.wait()), Err(cause) if matches!(cause.as_ref(), ApplyFailure::Unknown(_))), "ambiguous apply was called definite or durable");
        anyhow::ensure!(ticket.take_failed().is_none(), "unknown input was returned for a blind retry");
        drop(ticket);
        anyhow::ensure!(store.report().admission.batches == 1 && matches!(store.barrier(), Err(AdmissionRefusal::Fenced(_))) && store.sync().is_err(), "unknown outcome lost custody or allowed a successful barrier");
        Ok(())
    }

    #[test]
    fn u05_channel_full_waits_for_a_real_dequeue_without_self_waking() -> anyhow::Result<()> {
        let host = crate::store::Store::in_memory()?;
        let engine = super::super::amphora::Amphora::from_database(host.database())?;
        let store = Store::start_with_limits(Box::new(engine), Upkeep::default(), AdmissionLimits { batches: 1, bytes: 1 << 20 })?;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        // Hold the existing actor after an actual control dequeue. Holding its SQL owner
        // instead lets idle telemetry block before that dequeue and never fills this slot.
        store.enqueue(Msg::Run(Box::new(move |_| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv_timeout(READ_WAIT);
        })), true)?;
        entered_rx.recv_timeout(READ_WAIT)?;
        anyhow::ensure!(store.put_wait(rows::try_of("old", &said("two", 2, "queued historical row"), 2)?), "could not fill the one-slot channel");
        anyhow::ensure!(store.report().admission.batches == 0, "this scenario must be channel Full, not input-quota Full");
        let waiting_store = store.clone();
        crate::rt::block_on(async move {
            let batch = Batch::from_update("safe", &said("three", 3, "safe after actual dequeue"), 3)?.ok_or_else(|| anyhow::anyhow!("missing safe projection"))?;
            let mut pending = PendingSubmission::new(batch);
            let mut waiting = Box::pin(waiting_store.admit_until(&mut pending, Instant::now() + READ_WAIT));
            anyhow::ensure!(futures::poll!(waiting.as_mut()).is_pending(), "channel-full rollback woke itself instead of waiting");
            release_tx.send(())?;
            waiting.await?;
            let ticket = pending.ticket().ok_or_else(|| anyhow::anyhow!("dequeue wake lost the ticket"))?;
            ticket.wait().await.map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(())
        })?;
        anyhow::ensure!(store.kept("safe", 10)?.is_some_and(|kept| kept.events.len() == 1), "the real dequeue never admitted/durably completed the safe batch");
        Ok(())
    }

}
