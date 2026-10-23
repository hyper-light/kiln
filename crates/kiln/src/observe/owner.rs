//! The observe group on a node (`docs/observe.md` §5): one thread owns the group's log, its replica
//! and this node's writes ([`Batcher`]); the hub (on the server) or the agent (on a runner) hands it
//! what the other side says and sends what it says back ([`Out`]); a maintenance thread rewrites
//! the store's blocks apart from it; queries read the store on any thread ([`Observe::store`]).
//!
//! The server's member ([`SERVER`]) is the group's one voter, and so its leader: it commits alone,
//! so no runner holds a write up, and a runner that sleeps (a laptop) costs nothing. A runner that
//! replicates is a learner, taking the log, or an image and its files, to answer queries on its own
//! machine. Runners reach each other only through the server, so a quorum of runners could never
//! stand without it: a voter on a runner would add a wait to every write and no availability.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use hyper_block::file::DeviceFile;
use hyper_durable::{GroupStore, LogStore as _, Output, Replica, Unbounded};
use hyper_log::Log;
use hyper_raft::proto::{ConfChangeType, Message, MessageType};
use hyper_raft::wire::Record as _;
use kiln_obs::batch::SeriesKey;
use kiln_obs::group::{self, TICK};
use kiln_obs::hash::Keyed;
use kiln_obs::image::{FileKind, Image};
use kiln_obs::logs::record::Record;
use kiln_obs::logs::metrics::Touched;
use kiln_obs::logs::tail::Feed;
use kiln_obs::machine::{Machine, Shared};
use kiln_obs::manifest::{Point, plain_name};
use kiln_obs::store::{Answer, snapshot_name};
use serde::{Deserialize, Serialize};

use super::ingest::{Batcher, WAITING_LOG_BYTES, WAITING_POINTS};
use super::wire::{self, Joiner, Msg, Outcome, Status};
use crate::http::client::Client;

/// The server's member of the group: its one voter.
pub const SERVER: u64 = 1;
/// A change of configuration not seen applied by then is proposed again.
const CHANGE_AGAIN: Duration = Duration::from_secs(10);
/// How often the leader moves the cutoff: each entry rebuilds the index only when series go, and
/// costs a scan of the series' newest times otherwise (some microseconds a thousand series).
const RETAIN_EVERY: Duration = Duration::from_secs(60);
/// How often a runner says how its replica is doing, and the maintenance thread looks unasked.
const STATUS_EVERY: Duration = Duration::from_secs(10);
/// How often the room left on observe's disk is looked at, and what is kept free besides what a
/// cut needs: a cut writes the head as blocks before the log can be trimmed, and blocks hold no
/// more than the log entries they come of, so room for the log's compactable bytes, and this,
/// lets the cut that frees the log always be made.
const DISK_EVERY: Duration = Duration::from_secs(1);
pub(crate) const DISK_MARGIN: u64 = 16 << 20;
/// A replica that stopped is opened again after a second, then twice as long each time it stops
/// again, up to this; once it has run this long, from a second again.
const REOPEN_MAX: Duration = Duration::from_secs(60);
const MAINTAIN_EVERY: Duration = Duration::from_secs(60);
/// How often the view the API shows is made again, at the most.
const VIEW_EVERY: Duration = Duration::from_millis(500);
/// Bytes as people say them: `512 MiB`, `3.2 GiB`.
fn size(b: u64) -> String {
    match b {
        0..1_048_576 => format!("{} KiB", b / 1024),
        1_048_576..1_073_741_824 => format!("{} MiB", b / 1_048_576),
        _ => format!("{:.1} GiB", b as f64 / 1_073_741_824.0),
    }
}

/// A span of ms as people say it, to the nearest second under two minutes, else minute: `90 s`,
/// `45 min`, `1 h 0 min`.
fn span(ms: u64) -> String {
    if ms < 119_500 {
        return format!("{} s", (ms + 500) / 1000);
    }
    let min = (ms + 30_000) / 60_000;
    if min < 60 { format!("{min} min") } else { format!("{} h {} min", min / 60, min % 60) }
}

/// A machine's clock this far from the server's is said (an event): the Datadog Agent's NTP check
/// warns past a minute.
const SKEW_SAID: Duration = Duration::from_secs(60);
/// How often the bytes observe holds on disk are counted (a walk of its folder: a stat a file).
const SIZED_EVERY: Duration = Duration::from_secs(30);
/// Writes of other nodes' senders whose answers the server routes, at the most: one out a sender.
const WRITERS: usize = kiln_obs::store::MAX_SESSIONS;
/// A snapshot's file fetched over HTTP, at the most (a store keeps O(log n) blocks of n samples).
const SNAPSHOT_FILE_MAX: u64 = 1 << 40;
/// Profilers' uploads a node's Agent took and has not sent on, at the most (their bytes); and the
/// frames waiting on the lane past which none is taken: profiles yield to the group's own messages.
const PROFILE_BYTES: u64 = 64 << 20;
const PROFILE_ROOM: usize = 64;

/// What this node is in the group.
pub enum Role {
    /// The server: `key` signs the tokens runners fetch snapshot files with.
    Server { key: String },
    /// A runner of machine `me`, whose server is at `server`.
    Runner { me: String, http: Arc<Client>, server: String },
}

pub struct Settings {
    /// The node's observe folder: the group's log and the store.
    pub dir: PathBuf,
    pub role: Role,
    /// How long samples, log records and spans are kept (the server moves the cutoffs).
    pub metrics_retention: Duration,
    pub logs_retention: Duration,
    pub spans_retention: Duration,
    /// The bound on series the server states (0: none).
    pub max_series: u64,
}

/// Something to send: to a machine (from the server), or to the server (from a runner: `machine`
/// empty).
#[derive(Debug)]
pub struct Out {
    pub machine: String,
    pub msg: Msg,
    pub bytes: Vec<u8>,
}

/// What the API shows of the group.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub role: &'static str,
    pub member: u64,
    pub leader: u64,
    pub term: u64,
    pub applied: u64,
    pub durable: u64,
    pub series: u64,
    pub blocks: u64,
    /// Bytes observe's folder holds on this machine (`dir_bytes`), and of them its logs files and
    /// its spans files.
    pub disk_bytes: u64,
    pub log_bytes: u64,
    pub span_bytes: u64,
    /// Bytes the store holds in memory, by part (`kiln_obs::store::Heap`): its symbols, its
    /// index (series, blocks' and logs files' summaries), its head's samples.
    pub heap_symbols: u64,
    pub heap_index: u64,
    pub heap_samples: u64,
    pub cutoff_ms: i64,
    /// This node's writes: points waiting, points dropped, writes applied; samples and records
    /// the group dropped as too far ahead of their time.
    pub waiting: u64,
    pub dropped: u64,
    pub written: u64,
    pub ahead: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    /// The server's view of each machine's replica.
    pub machines: Vec<MachineView>,
    /// This replica's digest of its series as of `applied`, and the full digest last asked for
    /// (`verify`) with the index it is of.
    pub digest: u64,
    pub digest_at: u64,
    pub verified: Option<(u64, u64)>,
    pub error: Option<String>,
    /// Bytes free on observe's disk, and whether that is too little for the next cut (exporters
    /// are refused until a cut frees room).
    pub disk_free: Option<u64>,
    pub disk_low: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MachineView {
    pub machine: String,
    pub member: u64,
    pub up: bool,
    pub status: Option<Status>,
    /// How far its clock is ahead of the server's (negative: behind), ms, from its last write.
    pub clock_skew_ms: Option<i64>,
}

enum Input {
    Wake,
    Heard { machine: String, json: String, bytes: Vec<u8> },
    Up { machine: String, up: bool },
    Series(Vec<(SeriesKey, Vec<(i64, f64)>)>),
    /// Samples as a request decoded them: keys once, samples flat.
    Samples(kiln_obs::otlp::metrics::Decoded),
    Logs(super::ingest::Logs),
    /// Log records whose sender waits to know they are written (a file's reader).
    LogsAcked(super::ingest::Logs, super::ingest::Ack),
    /// Spans, as rows a request's decoding wrote: segments, each with its rows.
    Spans(Vec<(Vec<u8>, u32)>),
    /// A sender's wait for everything taken before it to be written (a node stopping).
    Settle(super::ingest::Ack),
    Staged { generation: u64, msg: Box<Message>, ok: bool },
    Fetched { generation: u64, done: u64, of: u64 },
    /// Cut the store and compact the log now, keeping nothing (an operator's, or a test's).
    Compact,
    /// Take the full digest, and on the server ask every replica for theirs.
    Verify,
    Verified { at: u64, digest: u64 },
    /// The full digest could not be taken.
    DigestFailed,
    /// The server's scrape targets, all of them: each runner is told those it reads.
    Scrapes(Vec<(String, super::scrape::Target)>),
    /// The server's log sources, all of them: as scrape targets.
    LogSources(Vec<(String, super::collect::Source)>),
    /// The server's log pipelines, in order: every runner is told them all.
    Pipelines(Vec<super::pipeline::PipelineDef>),
    /// The server's scanning groups, in order, and their key: every runner is told them.
    Scanner(Vec<super::scanner::GroupDef>, String),
    /// The server's log indexes, in order, and the parts of those removed: every runner is told
    /// them, and those over their quota.
    Indexes(Vec<super::indexes::Kept>, Vec<u16>),
    /// The server's synthetic tests, all of them: each runner is told those it runs; and one of
    /// them to run now everywhere.
    Synthetics(Vec<(String, super::synthetics::Test)>),
    RunSynthetic(String),
    /// The server's DogStatsD listeners, all of them: each runner is told those it opens.
    Statsd(Vec<(String, super::statsd::Listener)>),
    /// A profiler's upload this node's Agent took, to send on to the server
    /// (`super::profiles::pack`'d).
    Profile(Vec<u8>),
}

/// What a node has taken to write and not yet cut into a write, against the batcher's bound: taken
/// when a request is answered (by whoever answers it), released by the owner as its batcher's
/// writes are cut. Both counts only grow; what is inside is their difference. A request is answered
/// 200 only once its room is taken, so a flood is refused (503) and never taken and then dropped.
#[derive(Default)]
struct Admission {
    points: AtomicU64,
    log_bytes: AtomicU64,
    left_points: AtomicU64,
    left_log_bytes: AtomicU64,
    /// Points and records of kiln's own refused at the bound itself.
    refused: AtomicU64,
    /// The queue has kept too long a wait (`Batcher::congested`): exporters are refused until it
    /// drains.
    congested: AtomicBool,
    /// The disk has too little room for the next cut: exporters are refused until a cut frees it.
    disk_low: AtomicBool,
    /// Profilers' uploads taken and not yet sent on (their bytes).
    profile_bytes: AtomicU64,
    /// Requests' buffers handed back for the next request.
    spares: Spares,
}

/// Buffers a request is decoded into, handed back once the owner has taken what they held: a few
/// kept, so steady ingest decodes into buffers already grown and never frees on one thread what it
/// grows again on another (the C library keeps what each thread's arena grew, §12.11).
#[derive(Default)]
struct Spares {
    decoded: Mutex<Vec<kiln_obs::otlp::metrics::Decoded>>,
    bytes: Mutex<Vec<Vec<u8>>>,
}

/// Buffers of each kind kept at the most: twice the requests decoded at once (the blocking pool's
/// `fs` share, 8 on a server; `crate::rt::Profile`).
const SPARES: usize = 16;
/// The room a kept buffer may hold: a large request's go back to the allocator.
const SPARE_BYTES: usize = 1 << 20;

impl Spares {
    fn decoded(&self) -> kiln_obs::otlp::metrics::Decoded {
        lock(&self.decoded).pop().unwrap_or_default()
    }

    fn give_decoded(&self, mut d: kiln_obs::otlp::metrics::Decoded) {
        if d.capacity_bytes() <= SPARE_BYTES {
            // Handed out empty: what took it last may have left samples (the owner takes their
            // keys alone).
            d.clear();
            let mut kept = lock(&self.decoded);
            if kept.len() < SPARES {
                kept.push(d);
            }
        }
    }

    fn bytes(&self) -> Vec<u8> {
        lock(&self.bytes).pop().unwrap_or_default()
    }

    fn give_bytes(&self, mut b: Vec<u8>) {
        if b.capacity() <= SPARE_BYTES {
            b.clear();
            let mut kept = lock(&self.bytes);
            if kept.len() < SPARES {
                kept.push(b);
            }
        }
    }
}

/// Takes room for `n` against `max`, or says there is none. One write bigger than the bound is taken
/// when nothing waits: it could never be otherwise.
fn reserve(taken: &AtomicU64, left: &AtomicU64, n: u64, max: u64) -> bool {
    let mut cur = taken.load(Ordering::Acquire);
    loop {
        // `left` only grows, so a value read before `taken` changes overstates what is inside: a
        // reservation is never wrongly granted.
        let inside = cur.saturating_sub(left.load(Ordering::Acquire));
        if inside > 0 && inside.saturating_add(n) > max {
            return false;
        }
        match taken.compare_exchange_weak(cur, cur.saturating_add(n), Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => cur = now,
        }
    }
}

/// The node's handle on the group.
#[derive(Clone)]
pub struct Observe {
    tx: Sender<Input>,
    admission: Arc<Admission>,
    store: Arc<Mutex<Option<Shared>>>,
    view: Arc<Mutex<View>>,
    out: async_channel::Receiver<Out>,
    snaps: PathBuf,
    key: Option<String>,
    feed: Arc<Feed>,
    touched: Arc<Touched>,
    spans_touched: Arc<Touched>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Observe {
    /// Starts the node's group (its thread, and its maintenance thread).
    pub fn start(settings: Settings) -> Self {
        // The node's receivers begin now (`super::otlp::keys`).
        let _ = super::otlp::keys();
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = async_channel::bounded(16_384);
        let store = Arc::new(Mutex::new(None));
        // Its role said from the start: a status asked before the group opens says what it is.
        let role = if matches!(settings.role, Role::Server { .. }) { "server" } else { "runner" };
        let view = Arc::new(Mutex::new(View { role, ..View::default() }));
        let snaps = settings.dir.join("store").join(kiln_obs::machine::SNAPSHOTS);
        let key = match &settings.role {
            Role::Server { key } => Some(key.clone()),
            Role::Runner { .. } => None,
        };
        let admission = Arc::new(Admission::default());
        let feed = Feed::new();
        // The server counts metrics from logs and from spans: its replica tells which windows
        // records and spans came in.
        let (touched, spans_touched) = (Arc::new(Touched::new()), Arc::new(Touched::new()));
        let server = matches!(settings.role, Role::Server { .. });
        let told = server.then(|| (touched.clone(), spans_touched.clone()));
        let handle = Self { tx: tx.clone(), admission: admission.clone(), store: store.clone(), view: view.clone(), out: out_rx, snaps, key, feed: feed.clone(), touched, spans_touched };
        let spawned = std::thread::Builder::new().name("kiln-observe".into()).spawn(move || {
            let mut owner = Owner::new(settings, store, view, out_tx, tx, admission, feed, told);
            owner.run(&rx);
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "observe's thread did not start");
        }
        handle
    }

    /// What to send, as it comes.
    pub fn outgoing(&self) -> async_channel::Receiver<Out> {
        self.out.clone()
    }

    /// What `machine` (on the server), or the server (on a runner: `machine` empty), said.
    pub fn heard(&self, machine: &str, json: String, bytes: Vec<u8>) {
        let _ = self.tx.send(Input::Heard { machine: machine.to_owned(), json, bytes });
    }

    /// A machine's replicating runner process came or went (on the server), or the server did (on
    /// a runner).
    pub fn up(&self, machine: &str, up: bool) {
        let _ = self.tx.send(Input::Up { machine: machine.to_owned(), up });
    }

    /// Points of kiln's own to write (its hosts', CI's, CD's): taken up to the bound itself, else
    /// refused and counted as dropped.
    pub fn write(&self, series: Vec<(SeriesKey, Vec<(i64, f64)>)>) {
        let _taken = self.write_within(series, WAITING_POINTS as u64, true);
    }

    /// Samples of kiln's own in a decoding (from [`Observe::decoding`]: a check's look, its keys
    /// made once and kept): as [`Observe::write`], taken up to the bound itself, else refused and
    /// counted as dropped; the decoding comes back, its room kept, to be filled again.
    pub fn write_samples(&self, d: kiln_obs::otlp::metrics::Decoded) {
        let n = d.samples.len() as u64;
        let a = &self.admission;
        if n == 0 || !reserve(&a.points, &a.left_points, n, WAITING_POINTS as u64) {
            a.refused.fetch_add(n, Ordering::Relaxed);
            a.spares.give_decoded(d);
            return;
        }
        if let Err(std::sync::mpsc::SendError(Input::Samples(d))) = self.tx.send(Input::Samples(d)) {
            a.left_points.fetch_add(n, Ordering::AcqRel);
            a.spares.give_decoded(d);
        }
    }

    /// Points an exporter or a client sent: taken up to three quarters of the bound (the rest is
    /// kiln's own), else not taken: answer 503, and it sends them again.
    #[must_use]
    pub fn offer(&self, series: Vec<(SeriesKey, Vec<(i64, f64)>)>) -> bool {
        self.write_within(series, WAITING_POINTS as u64 / 4 * 3, false)
    }

    /// Takes `series` if their points fit under `max`; what is refused is counted as dropped when
    /// `count` (kiln's own: no one sends it again).
    fn write_within(&self, series: Vec<(SeriesKey, Vec<(i64, f64)>)>, max: u64, count: bool) -> bool {
        let n: u64 = series.iter().map(|(_, p)| p.len() as u64).sum();
        if n == 0 {
            return true;
        }
        let a = &self.admission;
        if (!count && (a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire))) || !reserve(&a.points, &a.left_points, n, max) {
            if count {
                a.refused.fetch_add(n, Ordering::Relaxed);
            }
            return false;
        }
        if self.tx.send(Input::Series(series)).is_err() {
            // No owner to take them: the room goes back.
            a.left_points.fetch_add(n, Ordering::AcqRel);
        }
        true
    }

    /// Samples an exporter sent, as a request decoded them: as [`Observe::offer`]; the decoding
    /// comes back to [`Observe::decoding`] once taken or refused.
    #[must_use]
    pub fn offer_samples(&self, d: kiln_obs::otlp::metrics::Decoded) -> bool {
        match self.offer_samples_or_back(d) {
            Ok(()) => true,
            Err(d) => {
                self.admission.spares.give_decoded(*d);
                false
            }
        }
    }

    /// As [`Observe::offer_samples`], but what is refused comes back (boxed: the refusal is the
    /// rare way), for a sender that cannot be asked to send it again (a datagram's) to hold and
    /// offer again.
    pub fn offer_samples_or_back(&self, d: kiln_obs::otlp::metrics::Decoded) -> Result<(), Box<kiln_obs::otlp::metrics::Decoded>> {
        let n = d.samples.len() as u64;
        let a = &self.admission;
        if n == 0 {
            a.spares.give_decoded(d);
            return Ok(());
        }
        if a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire) || !reserve(&a.points, &a.left_points, n, WAITING_POINTS as u64 / 4 * 3) {
            return Err(Box::new(d));
        }
        if let Err(std::sync::mpsc::SendError(Input::Samples(d))) = self.tx.send(Input::Samples(d)) {
            a.left_points.fetch_add(n, Ordering::AcqRel);
            a.spares.give_decoded(d);
        }
        Ok(())
    }

    /// A decoding to decode a request into: one handed back, its room kept, or a new one.
    pub fn decoding(&self) -> kiln_obs::otlp::metrics::Decoded {
        self.admission.spares.decoded()
    }

    /// Bytes to read a request into (its body unzipped), and to hand back after.
    pub fn bytes(&self) -> Vec<u8> {
        self.admission.spares.bytes()
    }

    pub fn give_bytes(&self, b: Vec<u8>) {
        self.admission.spares.give_bytes(b);
    }

    /// A decoding not offered (it did not decode), handed back.
    pub fn give_decoding(&self, d: kiln_obs::otlp::metrics::Decoded) {
        self.admission.spares.give_decoded(d);
    }

    /// The server's scrape targets now: each runner is told those it reads, now and when it
    /// connects.
    pub fn set_scrapes(&self, all: Vec<(String, super::scrape::Target)>) {
        let _ = self.tx.send(Input::Scrapes(all));
    }

    /// The server's log sources now: each runner is told those it reads, now and when it connects.
    pub fn set_log_sources(&self, all: Vec<(String, super::collect::Source)>) {
        let _ = self.tx.send(Input::LogSources(all));
    }

    /// The server's log pipelines now: every runner is told them, now and when it connects.
    pub fn set_pipelines(&self, all: Vec<super::pipeline::PipelineDef>) {
        let _ = self.tx.send(Input::Pipelines(all));
    }

    /// The server's log indexes now: routed by here from the next record, every runner told, now
    /// and when it connects.
    pub fn set_indexes(&self, all: Vec<super::indexes::Kept>, removed: Vec<u16>) {
        let _ = self.tx.send(Input::Indexes(all, removed));
    }

    /// The server's scanning groups now, and their key: every runner is told them, now and when it
    /// connects.
    pub fn set_scanner(&self, groups: Vec<super::scanner::GroupDef>, key: String) {
        let _ = self.tx.send(Input::Scanner(groups, key));
    }

    /// The server's synthetic tests now: each runner is told those it runs, now and when it
    /// connects.
    pub fn set_synthetics(&self, all: Vec<(String, super::synthetics::Test)>) {
        let _ = self.tx.send(Input::Synthetics(all));
    }

    /// The server's DogStatsD listeners now: each runner is told those it opens, now and when it
    /// connects.
    pub fn set_statsd(&self, all: Vec<(String, super::statsd::Listener)>) {
        let _ = self.tx.send(Input::Statsd(all));
    }

    /// Asks every runner that runs test `name` to run it now.
    pub fn run_synthetic(&self, name: &str) {
        let _ = self.tx.send(Input::RunSynthetic(name.to_owned()));
    }

    /// Log records of kiln's own (its events): taken up to the bound itself, else refused and
    /// counted as dropped.
    pub fn log(&self, records: Vec<Record>) {
        let _taken = self.log_within(records, WAITING_LOG_BYTES as u64, true);
    }

    /// Spans of kiln's own (its CI runs', `super::kiln`): their rows in segments as a decoder cuts
    /// them, taken up to the bound itself, else refused and counted as dropped.
    pub fn spans(&self, records: &[Record]) {
        let mut segments: Vec<(Vec<u8>, u32)> = Vec::new();
        let (mut rows, mut n, mut prev) = (Vec::new(), 0u32, 0i64);
        for r in records {
            if rows.len() >= kiln_obs::otlp::traces::SEGMENT_BYTES {
                segments.push((std::mem::take(&mut rows), n));
                (n, prev) = (0, 0);
            }
            kiln_obs::logs::record::put_row(&mut rows, r, prev);
            prev = r.ts;
            n += 1;
        }
        if n > 0 {
            segments.push((rows, n));
        }
        let bytes: u64 = segments.iter().map(|(r, _)| r.len() as u64).sum();
        if bytes == 0 {
            return;
        }
        let a = &self.admission;
        if !reserve(&a.log_bytes, &a.left_log_bytes, bytes, WAITING_LOG_BYTES as u64) {
            a.refused.fetch_add(records.len() as u64, Ordering::Relaxed);
            return;
        }
        if self.tx.send(Input::Spans(segments)).is_err() {
            a.left_log_bytes.fetch_add(bytes, Ordering::AcqRel);
        }
    }

    /// Log records an exporter or a client sent: as [`Observe::offer`].
    #[must_use]
    pub fn offer_logs(&self, records: Vec<Record>) -> bool {
        self.log_within(records, WAITING_LOG_BYTES as u64 / 4 * 3, false)
    }

    /// Log records an exporter sent, decoded into rows where no pipeline reads them
    /// (`kiln_obs::otlp::logs::Decoder`), records where one does: as [`Observe::offer_logs`], the
    /// pipelines run on its records. Refused, they come back.
    pub fn offer_log_parts(&self, logs: super::ingest::Logs) -> Result<(), super::ingest::Logs> {
        self.log_within_acked(logs, WAITING_LOG_BYTES as u64 / 4 * 3, false, None)
    }

    /// Log records read from a source that can be read again (a file, the journal): as
    /// [`Observe::offer_logs`], and `ack` given once they are written, so the reader moves its
    /// place past them only then. Refused, they come back and the ack is never given: the reader
    /// offers them again.
    pub fn offer_logs_acked(&self, records: Vec<Record>, ack: super::ingest::Ack) -> Result<(), Vec<Record>> {
        self.offer_log_batch(records.into(), ack).map_err(super::ingest::Logs::into_records)
    }

    /// Log records as [`Observe::offer_logs_acked`] takes them, and records a collector made rows
    /// where it read them (none a pipeline reads), all under one ack.
    pub fn offer_log_batch(&self, logs: super::ingest::Logs, ack: super::ingest::Ack) -> Result<(), super::ingest::Logs> {
        self.log_within_acked(logs, WAITING_LOG_BYTES as u64 / 4 * 3, false, Some(ack))
    }

    fn log_within(&self, records: Vec<Record>, max: u64, count: bool) -> bool {
        self.log_within_acked(records.into(), max, count, None).is_ok()
    }

    fn log_within_acked(&self, mut logs: super::ingest::Logs, max: u64, count: bool, ack: Option<super::ingest::Ack>) -> Result<(), super::ingest::Logs> {
        if logs.is_empty() && ack.is_none() {
            return Ok(());
        }
        // Each service check's status as a sample of its series, from the run as it came
        // (`kiln_obs::monitor::checks`): written once its record is taken.
        let statuses: Vec<(SeriesKey, Vec<(i64, f64)>)> = logs.records().filter_map(kiln_obs::monitor::checks::status_sample).map(|(key, ms, v)| (key, vec![(ms, v)])).collect();
        // The pipelines, where the records are taken. A reader's (acked) records were processed as
        // it gathered them: one refused comes back to be offered again, and is not processed twice.
        // (A row was read by the pipelines that read it where it was written.)
        if ack.is_none() {
            for run in logs.runs_mut() {
                super::pipeline::kept::process(run);
                super::scanner::kept::process(run);
            }
        }
        // Each error's issue (`kiln_obs::errors`), after the pipelines that name its parts.
        for r in logs.records_mut() {
            kiln_obs::errors::put_log_issue(r);
        }
        // Each record's part of the store: its index's, kiln's events', or not kept for search
        // (`super::indexes`); what that came to counted once they are taken.
        let usage = super::indexes::route_logs(&mut logs);
        let n = logs.bytes();
        let a = &self.admission;
        if !logs.is_empty() && ((!count && (a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire))) || !reserve(&a.log_bytes, &a.left_log_bytes, n, max)) {
            if count {
                a.refused.fetch_add(logs.len(), Ordering::Relaxed);
            }
            return Err(logs);
        }
        let input = match ack {
            Some(ack) => Input::LogsAcked(logs, ack),
            None => Input::Logs(logs),
        };
        if let Err(e) = self.tx.send(input) {
            a.left_log_bytes.fetch_add(n, Ordering::AcqRel);
            return Err(match e.0 {
                Input::Logs(l) | Input::LogsAcked(l, _) => l,
                _ => super::ingest::Logs::default(),
            });
        }
        self.write(statuses);
        super::indexes::count(&usage);
        Ok(())
    }

    /// Spans an exporter sent, as rows its request's decoding wrote (`kiln_obs::otlp::traces`): as
    /// [`Observe::offer_logs`], their bytes against the same bound. Refused, their buffers are kept
    /// for the next request's rows.
    #[must_use]
    pub fn offer_spans(&self, segments: Vec<(Vec<u8>, u32)>) -> bool {
        let n: u64 = segments.iter().map(|(r, _)| r.len() as u64).sum();
        let a = &self.admission;
        if n == 0 {
            return true;
        }
        if a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire) || !reserve(&a.log_bytes, &a.left_log_bytes, n, WAITING_LOG_BYTES as u64 / 4 * 3) {
            for (rows, _) in segments {
                a.spares.give_bytes(rows);
            }
            return false;
        }
        if let Err(std::sync::mpsc::SendError(Input::Spans(segments))) = self.tx.send(Input::Spans(segments)) {
            a.left_log_bytes.fetch_add(n, Ordering::AcqRel);
            for (rows, _) in segments {
                a.spares.give_bytes(rows);
            }
        }
        true
    }

    /// A profiler's upload this node's Agent took (`super::profiles::pack`'d), to send on to the
    /// server: refused (false) past [`PROFILE_BYTES`] waiting, or while the lane holds
    /// [`PROFILE_ROOM`] frames.
    #[must_use]
    pub fn offer_profile(&self, payload: Vec<u8>) -> bool {
        let (a, n) = (&self.admission, payload.len() as u64);
        if self.out.len() >= PROFILE_ROOM {
            return false;
        }
        if a.profile_bytes.fetch_add(n, Ordering::AcqRel).saturating_add(n) > PROFILE_BYTES {
            a.profile_bytes.fetch_sub(n, Ordering::AcqRel);
            return false;
        }
        if self.tx.send(Input::Profile(payload)).is_err() {
            a.profile_bytes.fetch_sub(n, Ordering::AcqRel);
            return false;
        }
        true
    }

    /// Sets `ack` once everything taken before it is written (nothing waits, no write is out), as a
    /// node stopping waits for its last; at once with no owner to write it.
    pub fn settle(&self, ack: super::ingest::Ack) {
        if let Err(std::sync::mpsc::SendError(Input::Settle(a))) = self.tx.send(Input::Settle(ack)) {
            a.set();
        }
    }

    /// Cuts the store and compacts the group's log now, keeping nothing: a member behind takes an
    /// image after this.
    pub fn compact(&self) {
        let _ = self.tx.send(Input::Compact);
    }

    /// Takes the full digest of this replica, and on the server asks every replica for theirs;
    /// the view shows them as they come.
    pub fn verify(&self) {
        let _ = self.tx.send(Input::Verify);
    }

    /// Whether an exporter's write would be refused now (three quarters of the bound taken): asked
    /// before a request is decoded, so a flood costs the node no decoding.
    pub fn busy(&self) -> bool {
        let a = &self.admission;
        let inside = |taken: &AtomicU64, left: &AtomicU64| taken.load(Ordering::Acquire).saturating_sub(left.load(Ordering::Acquire));
        a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire) || inside(&a.points, &a.left_points) >= WAITING_POINTS as u64 / 4 * 3 || inside(&a.log_bytes, &a.left_log_bytes) >= WAITING_LOG_BYTES as u64 / 4 * 3
    }

    /// The store, for queries, once it is open.
    pub fn store(&self) -> Option<Shared> {
        lock(&self.store).clone()
    }

    /// The log records this node's replica applies, as it applies them (live tail): the same feed
    /// whichever replica is open.
    pub fn feed(&self) -> &Arc<Feed> {
        &self.feed
    }

    /// The windows of records' time this node's replica applied records in (the server's: metrics
    /// from logs count them).
    pub fn touched(&self) -> &Arc<Touched> {
        &self.touched
    }

    /// As [`Observe::touched`], of spans (metrics from spans: trace metrics).
    pub fn spans_touched(&self) -> &Arc<Touched> {
        &self.spans_touched
    }

    pub fn view(&self) -> View {
        lock(&self.view).clone()
    }

    /// The token machine `machine` fetches snapshot files with (on the server).
    pub fn token(&self, machine: &str) -> Option<String> {
        self.key.as_ref().map(|k| token(k, machine))
    }

    /// A snapshot's file, for a machine that fetches it: none for a name that is not one.
    pub fn snapshot_file(&self, snapshot: &str, file: &str) -> Option<PathBuf> {
        let valid = snapshot.split_once('-').is_some_and(|(a, b)| a.len() == 20 && b.len() == 20 && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit()));
        (valid && plain_name(file) && (file.ends_with(".block") || file.ends_with(".logs"))).then(|| self.snaps.join(snapshot).join(file)).filter(|p| p.is_file())
    }
}

/// The token a machine's snapshot fetches carry: signed by the server's key, for that machine.
pub fn token(key: &str, machine: &str) -> String {
    crate::util::hmac_hex(key.as_bytes(), format!("observe:{machine}").as_bytes())
}

/// Wakes the owner when the log answers: a message on its channel, once until it drives again.
struct Wakeup {
    tx: Sender<Input>,
    pending: AtomicBool,
}

impl Wake for Wakeup {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.pending.swap(true, Ordering::SeqCst) {
            let _ = self.tx.send(Input::Wake);
        }
    }
}

/// The replica, then its log: dropped in that order, as a replica must be.
struct Rep {
    r: Replica<GroupStore<DeviceFile>, Machine>,
    _log: Log<DeviceFile>,
    out: Output<(u64, Answer)>,
    compacted: u64,
    /// Where the store was last cut, and since when (ns since the owner started) an entry past it
    /// has waited: entries that come slowly are cut within [`cut_after`] of the first of them
    /// (`kiln_obs::group::due_by_age`).
    cut: u64,
    waiting: Option<u64>,
    /// The store's durable point when maintenance was last told: a cut since is new blocks to
    /// merge, however it came (a compaction, a snapshot's persisting).
    told: u64,
}

/// How long applied entries wait for a cut at the most (`kiln_obs::group::CUT_AFTER`);
/// `KILN_OBSERVE_CUT_AFTER_MS` makes it shorter, for a test of what waits on cuts (log archives).
fn cut_after() -> Duration {
    static AFTER: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *AFTER.get_or_init(|| std::env::var("KILN_OBSERVE_CUT_AFTER_MS").ok().and_then(|v| v.parse().ok()).map_or(group::CUT_AFTER, Duration::from_millis))
}

/// Which member each machine is, as the server keeps it (`<observe>/members.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Members {
    /// The group's identity: a runner that held another group's replica starts again.
    group: String,
    next: u64,
    machines: std::collections::BTreeMap<String, u64>,
}

/// This runner's member, as the server named it (`<observe>/member.json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Me {
    group: String,
    member: u64,
}

/// A snapshot whose files are being fetched.
struct Fetching {
    generation: u64,
    at: (u64, u64),
    progress: (u64, u64),
}

struct Owner {
    dir: PathBuf,
    role: Role,
    retention: (Duration, Duration, Duration),
    max_series: u64,
    rep: Option<Rep>,
    store: Arc<Mutex<Option<Shared>>>,
    view: Arc<Mutex<View>>,
    viewed: Instant,
    /// The bytes observe's folder holds (the store's files, the group's log, snapshots), and when
    /// they were last counted.
    dir_bytes: u64,
    sized: Option<Instant>,
    out: async_channel::Sender<Out>,
    wakeup: Arc<Wakeup>,
    waker: Waker,
    input: Sender<Input>,
    admission: Arc<Admission>,
    feed: Arc<Feed>,
    /// Told the times of records and of spans the replica applies (the server's).
    touched: Option<(Arc<Touched>, Arc<Touched>)>,
    start: Instant,
    batcher: Batcher,
    /// Waits for what is taken to be written: each told once nothing waits and no write is out.
    settles: Vec<super::ingest::Ack>,
    /// The server's scrape targets (`super::scrape`): each runner is told those it reads.
    scrapes: Vec<(String, super::scrape::Target)>,
    synthetics: Vec<(String, super::synthetics::Test)>,
    statsd: Vec<(String, super::statsd::Listener)>,
    log_sources: Vec<(String, super::collect::Source)>,
    pipelines: Vec<super::pipeline::PipelineDef>,
    scanner: (Vec<super::scanner::GroupDef>, String),
    /// The server's log indexes: every runner is told them.
    indexes: Vec<super::indexes::Kept>,
    /// The parts of indexes removed: let go by the leader (none until the server says).
    removed_parts: Vec<u16>,
    joiner: Joiner,
    part: u64,
    error: Option<String>,
    maintain: Option<Maintenance>,
    /// When the replica was last tried, while it is not open.
    tried: Option<Instant>,
    /// Stops of the replica since it last ran well, and since when it runs.
    stops: u32,
    opened_at: Option<Instant>,
    /// When the disk was last looked at, what was free, and whether too little.
    disk_seen: Option<Instant>,
    disk_free: Option<u64>,
    disk_low: bool,
    compact_asked: bool,
    verified: Option<(u64, u64)>,
    /// A full digest being taken (one at a time: it reads the whole replica), and whether another
    /// was asked for meanwhile (taken once it is done, so the asker gets one of now).
    digesting: bool,
    digest_again: bool,
    /// When the replica asks to be driven again though nothing arrives (ns since `start`).
    wake_at: Option<u64>,
    // The server's.
    members: Members,
    up: HashSet<String>,
    statuses: HashMap<String, Status>,
    writers: HashMap<u64, String>,
    /// Each machine's clock against the server's, from its writes' times; and whether it was past
    /// [`SKEW_SAID`] when last said.
    skews: HashMap<String, (i64, bool)>,
    changing: Option<(u64, Instant)>,
    retained: Option<Instant>,
    limited: bool,
    // A runner's.
    me: Option<Me>,
    token: String,
    lock: Option<std::fs::File>,
    fetching: Option<Fetching>,
    generation: u64,
    server_up: bool,
    said: Instant,
}

fn now_ns(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Owner {
    #[allow(clippy::too_many_arguments)]
    fn new(settings: Settings, store: Arc<Mutex<Option<Shared>>>, view: Arc<Mutex<View>>, out: async_channel::Sender<Out>, input: Sender<Input>, admission: Arc<Admission>, feed: Arc<Feed>, touched: Option<(Arc<Touched>, Arc<Touched>)>) -> Self {
        let wakeup = Arc::new(Wakeup { tx: input.clone(), pending: AtomicBool::new(false) });
        let waker = Waker::from(wakeup.clone());
        let mut o = Self {
            dir: settings.dir,
            role: settings.role,
            retention: (settings.metrics_retention, settings.logs_retention, settings.spans_retention),
            max_series: settings.max_series,
            rep: None,
            store,
            view,
            viewed: Instant::now(),
            dir_bytes: 0,
            sized: None,
            out,
            wakeup,
            waker,
            input,
            admission,
            feed,
            touched,
            start: Instant::now(),
            batcher: Batcher::new(rand::random()),
            settles: Vec::new(),
            scrapes: Vec::new(),
            synthetics: Vec::new(),
            statsd: Vec::new(),
            log_sources: Vec::new(),
            pipelines: Vec::new(),
            scanner: (Vec::new(), String::new()),
            indexes: Vec::new(),
            removed_parts: Vec::new(),
            joiner: Joiner::default(),
            part: 0,
            error: None,
            maintain: None,
            tried: None,
            stops: 0,
            opened_at: None,
            disk_seen: None,
            disk_free: None,
            disk_low: false,
            compact_asked: false,
            verified: None,
            digesting: false,
            digest_again: false,
            wake_at: None,
            members: Members::default(),
            up: HashSet::new(),
            statuses: HashMap::new(),
            writers: HashMap::new(),
            skews: HashMap::new(),
            changing: None,
            retained: None,
            limited: false,
            me: None,
            token: String::new(),
            lock: None,
            fetching: None,
            generation: 0,
            server_up: false,
            said: Instant::now(),
        };
        if let Err(e) = std::fs::create_dir_all(&o.dir) {
            o.error = Some(format!("{}: {e}", o.dir.display()));
            return o;
        }
        if matches!(o.role, Role::Server { .. }) {
            o.members = load_json(&o.dir.join("members.json")).unwrap_or_default();
            if o.members.group.is_empty() {
                o.members.group = crate::util::new_id();
                o.members.next = SERVER + 1;
                o.save_members();
            }
            o.open(SERVER);
        } else if let Some(me) = load_json::<Me>(&o.dir.join("member.json")) {
            // A runner opens what it held at once, to answer queries before the server says more.
            o.me = Some(me.clone());
            o.open(me.member);
        }
        o
    }

    fn is_server(&self) -> bool {
        matches!(self.role, Role::Server { .. })
    }

    fn save_members(&mut self) {
        if let Err(e) = save_json(&self.dir.join("members.json"), &self.members) {
            self.error = Some(format!("the group's members could not be saved: {e:#}"));
        }
    }

    /// Opens the replica of member `id` on the node's log and store. Whatever held them before is
    /// let go first, its maintenance thread ended: two stores never write one folder.
    fn open(&mut self, id: u64) {
        self.rep = None;
        self.maintain = None;
        self.tried = Some(Instant::now());
        if !self.is_server() && self.lock.is_none() {
            let path = self.dir.join("lock");
            match std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path) {
                Ok(f) if f.try_lock().is_ok() => self.lock = Some(f),
                Ok(_) => {
                    self.error = Some("another runner process of this machine replicates; waiting for it to let go".into());
                    return;
                }
                Err(e) => {
                    self.error = Some(format!("{}: {e}", path.display()));
                    return;
                }
            }
        }
        let opened = (|| -> Result<Rep, String> {
            let (log, store) = group::open_log(&self.dir.join("obs.log"))?;
            let machine = Machine::open(&self.dir.join("store"), Keyed::new(), &group::founding(&[SERVER])).map_err(|e| format!("the store: {e}"))?;
            let depth = store.depth();
            let settings = group::settings(id, depth).map_err(|e| format!("the group's settings: {e:?}"))?;
            let r = Replica::open(&settings, store, machine, Unbounded).map_err(|e| format!("the replica: {e:?}"))?;
            let applied = r.applied().index;
            Ok(Rep { compacted: applied, cut: applied, waiting: None, told: 0, r, _log: log, out: Output::default() })
        })();
        match opened {
            Ok(mut rep) => {
                if id == SERVER && rep.r.configuration().voters == [SERVER] {
                    let _ = rep.r.campaign();
                }
                // What it applies from here on is published, but nothing its log held as it
                // opened: entries applied before a restart are applied again.
                if let Ok(last) = hyper_raft::Storage::last_index(rep.r.core().store()) {
                    self.feed.seen(last);
                    if let Some((t, s)) = &self.touched {
                        t.seen(last);
                        s.seen(last);
                    }
                }
                {
                    let shared = rep.r.machine().shared();
                    let mut s = shared.write().unwrap_or_else(PoisonError::into_inner);
                    s.set_feed(Some(self.feed.clone()));
                    s.set_touched(self.touched.as_ref().map(|t| t.0.clone()));
                    s.set_spans_touched(self.touched.as_ref().map(|t| t.1.clone()));
                }
                *lock(&self.store) = Some(rep.r.machine().shared());
                self.opened_at = Some(Instant::now());
                self.maintain = Some(Maintenance::start(rep.r.machine().shared(), self.dir.clone()));
                self.rep = Some(rep);
                self.error = None;
            }
            Err(e) => {
                tracing::warn!(error = %e, "observe's group did not open");
                self.error = Some(e);
            }
        }
    }

    /// Lets go of the replica, its log and its folder (another process of this machine
    /// replicates now), removing nothing.
    fn close(&mut self) {
        self.rep = None;
        self.maintain = None;
        *lock(&self.store) = None;
        if let Some(f) = self.lock.take() {
            let _ = f.unlock();
        }
    }

    fn run(&mut self, rx: &Receiver<Input>) {
        let mut tick = Instant::now() + TICK;
        loop {
            // Until the next tick, the next write this node owes (a write freed by an answer goes
            // at once), or the replica's own next wake, whichever comes first.
            let now = Instant::now();
            let mut until = tick;
            if let Some(due) = self.batcher.next_due() {
                until = until.min(due);
            }
            if let Some(at) = self.wake_at {
                until = until.min(self.start + Duration::from_nanos(at));
            }
            match rx.recv_timeout(until.saturating_duration_since(now)) {
                Ok(input) => self.input(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(input) = rx.try_recv() {
                self.input(input);
            }
            if Instant::now() >= tick {
                tick = Instant::now() + TICK;
                if let Some(rep) = &mut self.rep {
                    let _ = rep.r.tick();
                }
            }
            self.writes();
            self.drive();
            self.periodic();
            // What left the batcher's waiting frees its room for the next requests; a queue that
            // keeps too long a wait refuses exporters until it drains.
            let (points, log_bytes) = self.batcher.left();
            self.admission.left_points.store(points, Ordering::Release);
            self.admission.left_log_bytes.store(log_bytes, Ordering::Release);
            // Spans' rows written into a write: their buffers for the next requests' rows.
            for rows in self.batcher.spent() {
                self.admission.spares.give_bytes(rows);
            }
            if !self.settles.is_empty() && self.batcher.backlog() == (0, false) {
                for a in self.settles.drain(..) {
                    a.set();
                }
            }
            self.admission.congested.store(self.batcher.congested(Instant::now()), Ordering::Release);
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::Wake => {}
            Input::Heard { machine, json, bytes } => {
                if let Some(msg) = wire::decode(&json) {
                    if self.is_server() {
                        self.heard_from_runner(&machine, msg, bytes);
                    } else {
                        self.heard_from_server(msg, bytes);
                    }
                }
            }
            Input::Up { machine, up } => {
                if self.is_server() {
                    self.runner_up(&machine, up);
                } else {
                    self.server_up = up;
                }
            }
            Input::Series(series) => self.batcher.series(series, Instant::now()),
            Input::Samples(mut d) => {
                self.batcher.samples(&mut d.keys, &d.samples, Instant::now());
                self.admission.spares.give_decoded(d);
            }
            Input::Scrapes(all) => {
                self.scrapes = all;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send_scrapes(&machine);
                }
            }
            Input::Synthetics(all) => {
                self.synthetics = all;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send_synthetics(&machine);
                }
            }
            Input::Statsd(all) => {
                self.statsd = all;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send_statsd(&machine);
                }
            }
            Input::RunSynthetic(name) => {
                let up: Vec<String> = self.up.iter().filter(|m| self.synthetics.iter().any(|(n, t)| *n == name && t.run_by(m))).cloned().collect();
                for machine in up {
                    self.send(&machine, Msg::RunSynthetic { name: name.clone() }, Vec::new());
                }
            }
            Input::LogSources(all) => {
                self.log_sources = all;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send_log_sources(&machine);
                }
            }
            Input::Pipelines(all) => {
                self.pipelines = all;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send(&machine, Msg::Pipelines { pipelines: self.pipelines.clone() }, Vec::new());
                }
            }
            Input::Indexes(all, removed) => {
                super::indexes::activate(&all);
                if self.is_server() {
                    let now = super::clock_ms();
                    let store = self.rep.as_ref().map(|rep| rep.r.machine());
                    let counted = |part: u16, from_ns: i64| store.as_ref().map_or(0, |s| s.read().log_rows_in(part, from_ns));
                    let over = {
                        let mut q = super::indexes::quota::global().lock().unwrap_or_else(PoisonError::into_inner);
                        q.set(&all, now, store.is_some().then_some(&counted as &dyn Fn(u16, i64) -> u64));
                        q.over()
                    };
                    super::indexes::set_limited(over);
                }
                self.indexes = all;
                self.removed_parts = removed;
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send_indexes(&machine);
                }
            }
            Input::Scanner(groups, key) => {
                self.scanner = (groups, key);
                let up: Vec<String> = self.up.iter().cloned().collect();
                for machine in up {
                    self.send(&machine, Msg::Scanner { groups: self.scanner.0.clone(), key: self.scanner.1.clone() }, Vec::new());
                }
            }
            Input::Profile(payload) => {
                self.admission.profile_bytes.fetch_sub(payload.len() as u64, Ordering::AcqRel);
                self.send_profile(payload);
            }
            Input::Logs(records) => self.batcher.logs(records, Instant::now()),
            Input::LogsAcked(records, ack) => self.batcher.logs_acked(records, ack, Instant::now()),
            Input::Spans(segments) => self.batcher.spans(segments, Instant::now()),
            Input::Settle(ack) => self.settles.push(ack),
            Input::Staged { generation, msg, ok } => {
                if self.fetching.as_ref().is_some_and(|f| f.generation == generation) {
                    self.fetching = None;
                    if ok && let Some(rep) = &mut self.rep {
                        let _ = rep.r.step(*msg);
                    }
                }
            }
            Input::Fetched { generation, done, of } => {
                if let Some(f) = self.fetching.as_mut().filter(|f| f.generation == generation) {
                    f.progress = (done, of);
                }
            }
            Input::Compact => self.compact_asked = true,
            Input::Verify => {
                self.digest_full();
                if self.is_server() {
                    let up: Vec<String> = self.up.iter().cloned().collect();
                    for m in up {
                        self.send(&m, Msg::Verify, Vec::new());
                    }
                }
            }
            Input::DigestFailed => {
                self.digesting = false;
                if std::mem::take(&mut self.digest_again) {
                    self.digest_full();
                }
            }
            Input::Verified { at, digest } => {
                self.verified = Some((at, digest));
                self.digesting = false;
                if std::mem::take(&mut self.digest_again) {
                    self.digest_full();
                }
                // A runner says so at once.
                if !self.is_server() && self.server_up {
                    let status = self.status();
                    self.send("", Msg::Status(status), Vec::new());
                }
            }
        }
    }

    // ------------------------------------------------------------------------------ the server

    fn runner_up(&mut self, machine: &str, up: bool) {
        if !up {
            self.up.remove(machine);
            return;
        }
        self.up.insert(machine.to_owned());
        let member = match self.members.machines.get(machine) {
            Some(&m) => m,
            None => {
                let m = self.members.next.max(SERVER + 1);
                self.members.next = m + 1;
                self.members.machines.insert(machine.to_owned(), m);
                self.save_members();
                m
            }
        };
        let token = match &self.role {
            Role::Server { key } => token(key, machine),
            Role::Runner { .. } => String::new(),
        };
        self.send(machine, Msg::Member { member, token, group: self.members.group.clone() }, Vec::new());
        self.send_scrapes(machine);
        self.send_synthetics(machine);
        self.send_statsd(machine);
        self.send_log_sources(machine);
        self.send(machine, Msg::Pipelines { pipelines: self.pipelines.clone() }, Vec::new());
        self.send(machine, Msg::Scanner { groups: self.scanner.0.clone(), key: self.scanner.1.clone() }, Vec::new());
        self.send_indexes(machine);
    }

    /// Tells `machine` the log indexes, and those over their quota now.
    fn send_indexes(&mut self, machine: &str) {
        let limited = super::indexes::snapshot().limited().to_vec();
        self.send(machine, Msg::Indexes { indexes: self.indexes.clone(), limited }, Vec::new());
    }

    /// A write the server proposes, counted against its indexes' daily quotas: what that says,
    /// said as events; the indexes over their quota, routed by from now on here and on every
    /// runner.
    fn count_quotas(&mut self, entry: &[u8]) {
        let now = super::clock_ms();
        let (said, over) = {
            let mut q = super::indexes::quota::global().lock().unwrap_or_else(PoisonError::into_inner);
            let said = q.count(entry, now);
            (said, q.over())
        };
        for s in said {
            self.batcher.logs(super::ingest::Logs::events(vec![super::indexes::quota_event(&s, now)]), Instant::now());
        }
        self.set_over(over);
    }

    /// The parts over their quota now: routed by here, every runner told, when they change.
    fn set_over(&mut self, over: Vec<u16>) {
        if super::indexes::snapshot().limited() == over.as_slice() {
            return;
        }
        super::indexes::set_limited(over);
        let up: Vec<String> = self.up.iter().cloned().collect();
        for machine in up {
            self.send_indexes(&machine);
        }
    }

    /// Tells `machine` the scrape targets it reads.
    fn send_scrapes(&mut self, machine: &str) {
        let targets: Vec<(String, super::scrape::Target)> = self.scrapes.iter().filter(|(_, t)| t.read_by(machine)).cloned().collect();
        self.send(machine, Msg::Scrapes { targets }, Vec::new());
    }

    /// Tells `machine` the synthetic tests it runs.
    fn send_synthetics(&mut self, machine: &str) {
        let tests: Vec<(String, super::synthetics::Test)> = self.synthetics.iter().filter(|(_, t)| t.run_by(machine)).cloned().collect();
        self.send(machine, Msg::Synthetics { tests }, Vec::new());
    }

    /// Tells `machine` the DogStatsD listeners it opens.
    fn send_statsd(&mut self, machine: &str) {
        let listeners: Vec<(String, super::statsd::Listener)> = self.statsd.iter().filter(|(_, l)| l.read_by(machine)).cloned().collect();
        self.send(machine, Msg::Statsd { listeners }, Vec::new());
    }

    /// Tells `machine` the log sources it reads.
    fn send_log_sources(&mut self, machine: &str) {
        let sources: Vec<(String, super::collect::Source)> = self.log_sources.iter().filter(|(_, s)| s.read_by(machine)).cloned().collect();
        self.send(machine, Msg::LogSources { sources }, Vec::new());
    }

    fn member_of(&self, machine: &str) -> Option<u64> {
        self.members.machines.get(machine).copied()
    }

    fn machine_of(&self, member: u64) -> Option<&str> {
        self.members.machines.iter().find(|(_, m)| **m == member).map(|(k, _)| k.as_str())
    }

    fn heard_from_runner(&mut self, machine: &str, msg: Msg, bytes: Vec<u8>) {
        match msg {
            Msg::Raft { id, part, parts } => {
                let Some(bytes) = self.joiner.add(machine, id, part, parts, bytes) else { return };
                let Ok(m) = Message::decode(&bytes) else { return };
                // A message is its sender's only from the machine of that member.
                if Some(m.from) != self.member_of(machine) {
                    return;
                }
                if m.to == SERVER {
                    if let Some(rep) = &mut self.rep {
                        let _ = rep.r.step(m);
                    }
                } else if let Some(to) = self.machine_of(m.to).map(str::to_owned) {
                    // Between runners (a voter on one): relayed, as reach's frames are.
                    self.send_raft(&to, bytes);
                }
            }
            Msg::Write { sender, seq, id, part, parts } => {
                let Some(entry) = self.joiner.add(machine, id ^ (1 << 63), part, parts, bytes) else { return };
                if entry.len() > group::ENTRY_BYTES + kiln_obs::command::WRITE_HEADER {
                    return;
                }
                if self.writers.len() >= WRITERS && !self.writers.contains_key(&sender) {
                    self.writers.clear();
                }
                self.writers.insert(sender, machine.to_owned());
                let _ = seq;
                let mut entry = entry;
                self.stamp(machine, &mut entry);
                self.count_quotas(&entry);
                if let Some(rep) = &mut self.rep {
                    let _ = rep.r.propose(Vec::new(), entry);
                }
            }
            Msg::Status(s) => {
                self.statuses.insert(machine.to_owned(), s);
            }
            Msg::Profile { id, part, parts } => {
                if let Some(payload) = self.joiner.add(machine, id ^ (1 << 62), part, parts, bytes) {
                    super::profiles::from_runner(machine, payload);
                }
            }
            Msg::Written { .. }
            | Msg::Member { .. }
            | Msg::Release
            | Msg::Verify
            | Msg::Scrapes { .. }
            | Msg::LogSources { .. }
            | Msg::Pipelines { .. }
            | Msg::Indexes { .. }
            | Msg::Scanner { .. }
            | Msg::Synthetics { .. }
            | Msg::RunSynthetic { .. }
            | Msg::Statsd { .. } => {}
        }
    }

    /// A runner's write stamped with the server's clock (its leader's: what every replica judges
    /// its samples' times against, `kiln_obs::store::AHEAD_MS`), the runner's own time read first:
    /// how far its clock is from the server's, said as an event once it passes [`SKEW_SAID`], and
    /// again once it is back.
    fn stamp(&mut self, machine: &str, entry: &mut [u8]) {
        let now = super::clock_ms();
        let Some(theirs) = kiln_obs::command::write_at(entry, Some(now)) else { return };
        let skew = theirs.saturating_sub(now);
        let past = skew.unsigned_abs() > SKEW_SAID.as_millis() as u64;
        let was = self.skews.insert(machine.to_owned(), (skew, past)).is_some_and(|(_, p)| p);
        if past != was {
            let (message, severity) = if past {
                (format!("{machine}'s clock is {} {} the server's: its own figures' times are off by as much, and samples more than ten minutes ahead are dropped", span(skew.unsigned_abs()), if skew > 0 { "ahead of" } else { "behind" }), 13)
            } else {
                (format!("{machine}'s clock agrees with the server's again"), 9)
            };
            let attrs = vec![("event.type".to_owned(), "observe.clock_skew".to_owned()), ("machine".to_owned(), machine.to_owned()), ("skew_ms".to_owned(), skew.to_string())];
            self.batcher.logs(
                super::ingest::Logs::events(vec![Record {
                    ts: now.saturating_mul(1_000_000),
                    severity,
                    service: "kiln.observe".into(),
                    host: machine.to_owned(),
                    source: "kiln.event".into(),
                    message,
                    attrs,
                    trace_id: None,
                    span_id: None,
                }]),
                Instant::now(),
            );
        }
    }

    /// Brings the configuration to every machine known as a learner, one change at a time.
    fn reconcile(&mut self) {
        let Some(rep) = &mut self.rep else { return };
        if !rep.r.is_leader() {
            return;
        }
        let conf = rep.r.configuration();
        let have: HashSet<u64> = conf.voters.iter().chain(&conf.learners).copied().collect();
        if let Some((member, at)) = self.changing {
            if have.contains(&member) {
                self.changing = None;
            } else if at.elapsed() < CHANGE_AGAIN {
                return;
            }
        }
        let Some(&member) = self.members.machines.values().find(|m| !have.contains(m)) else { return };
        if rep.r.change(Vec::new(), &group::change(member, ConfChangeType::AddLearnerNode)).is_ok() {
            self.changing = Some((member, Instant::now()));
        }
    }

    /// The leader's own entries: the cutoff moved on, and the bound on series stated once.
    fn lead(&mut self) {
        let Some(rep) = &mut self.rep else { return };
        if !rep.r.is_leader() {
            return;
        }
        if !self.limited {
            let have = rep.r.machine().read().max_series();
            if have != self.max_series {
                let _ = rep.r.propose(Vec::new(), kiln_obs::command::limits(self.max_series));
            }
            self.limited = true;
        }
        if self.retained.is_none_or(|t| t.elapsed() >= RETAIN_EVERY) {
            let now = super::clock_ms();
            let back = |d: Duration| now.saturating_sub(i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
            if rep.r.propose(Vec::new(), kiln_obs::command::retain(back(self.retention.0), back(self.retention.1), now)).is_ok() {
                self.retained = Some(Instant::now());
            }
            // Spans' own, once the group holds any: a group that never took a span never sends an
            // entry a build that knows none would stop at.
            if rep.r.machine().read().holds_spans() {
                let _ = rep.r.propose(Vec::new(), kiln_obs::command::retain_spans(back(self.retention.2), now));
            }
            // Each part of the logs kept for its own time (an index's; those of indexes removed;
            // those not kept for search): stated only for such a part, so a group that never had
            // one never sends an entry a build that knows no parts would stop at. Only a part said
            // removed is let go: before the indexes are heard, none is.
            let (held, parts) = {
                let s = rep.r.machine().read();
                let parts: Vec<u16> = s.part_cutoffs().keys().copied().collect();
                (s.log_parts(), parts)
            };
            for (part, cutoff) in super::indexes::retentions(&self.indexes, &self.removed_parts, &held, &|p| parts.contains(&p), now, back(self.retention.1)) {
                let _ = rep.r.propose(Vec::new(), kiln_obs::command::retain_part(part, cutoff, now));
            }
        }
        // Indexes' days turned: their quotas over again; a day set before the store opened, read.
        let turned = {
            let store = rep.r.machine();
            let mut q = super::indexes::quota::global().lock().unwrap_or_else(PoisonError::into_inner);
            let read = q.read_unread(&|part, from_ns| store.read().log_rows_in(part, from_ns));
            q.tick(super::clock_ms()) || read
        };
        if turned {
            let over = super::indexes::quota::global().lock().unwrap_or_else(PoisonError::into_inner).over();
            self.set_over(over);
        }
    }

    // ------------------------------------------------------------------------------ a runner

    fn heard_from_server(&mut self, msg: Msg, bytes: Vec<u8>) {
        match msg {
            Msg::Member { member, token, group } => {
                self.token = token;
                let me = Me { group, member };
                if self.me.as_ref() != Some(&me) {
                    if self.me.is_some() {
                        // Another group's replica, or another member's: this one starts again.
                        self.close();
                        let _ = std::fs::remove_dir_all(self.dir.join("store"));
                        let _ = std::fs::remove_file(self.dir.join("obs.log"));
                        self.feed.reset();
                        if let Some((t, s)) = &self.touched {
                            t.reset();
                            s.reset();
                        }
                    }
                    if let Err(e) = save_json(&self.dir.join("member.json"), &me) {
                        self.error = Some(format!("{e:#}"));
                    }
                    self.me = Some(me);
                }
                if self.rep.is_none() {
                    self.open(member);
                }
            }
            Msg::Raft { id, part, parts } => {
                let Some(bytes) = self.joiner.add("server", id, part, parts, bytes) else { return };
                let Ok(m) = Message::decode(&bytes) else { return };
                if m.msg_type == MessageType::MsgSnapshot {
                    self.stage(m);
                } else if let Some(rep) = &mut self.rep {
                    let _ = rep.r.step(m);
                }
            }
            Msg::Written { sender, seq, outcome } => {
                if sender == self.batcher.sender() {
                    self.batcher.answered(seq, &outcome, Instant::now());
                }
            }
            Msg::Release => self.close(),
            Msg::Verify => self.digest_full(),
            Msg::Scrapes { targets } => {
                if let Some(s) = super::scrape::get() {
                    s.set(&targets);
                }
            }
            Msg::LogSources { sources } => {
                if let Some(c) = super::collect::get() {
                    c.set(&sources);
                }
            }
            Msg::Pipelines { pipelines } => super::pipeline::kept::activate(&pipelines),
            Msg::Indexes { indexes, limited } => {
                super::indexes::activate(&indexes);
                super::indexes::set_limited(limited);
            }
            Msg::Scanner { groups, key } => super::scanner::kept::activate(&groups, &key),
            Msg::Synthetics { tests } => {
                if let Some(r) = super::synthetics::runner() {
                    r.set(&tests);
                }
            }
            Msg::RunSynthetic { name } => {
                if let Some(r) = super::synthetics::runner() {
                    r.run_now(&name);
                }
            }
            Msg::Statsd { listeners } => {
                if let Some(s) = super::statsd::get() {
                    s.set(&listeners);
                }
            }
            Msg::Write { .. } | Msg::Status(_) | Msg::Profile { .. } => {}
        }
    }

    /// A snapshot from the leader: its image's files fetched into the staging folder, checked whole,
    /// then the message stepped. One the same as the snapshot being fetched goes on fetching.
    fn stage(&mut self, m: Message) {
        let Some(snap) = &m.snapshot else { return };
        let Some(meta) = &snap.metadata else { return };
        let at = (meta.index, meta.term);
        if self.fetching.as_ref().is_some_and(|f| f.at == at) {
            return;
        }
        let Ok(image) = Image::decode(&snap.data) else { return };
        let (Some(rep), Role::Runner { me, http, server }) = (&self.rep, &self.role) else { return };
        self.generation += 1;
        let generation = self.generation;
        let total = image.file_bytes();
        self.fetching = Some(Fetching { generation, at, progress: (0, total) });
        let staging = rep.r.machine().staging().to_owned();
        let job = Fetch { http: http.clone(), url: format!("{}/api/observe/v1/snapshot/{}", server.trim_end_matches('/'), snapshot_name(Point { index: at.0, term: at.1 })), me: me.clone(), token: self.token.clone(), staging, image, generation, input: self.input.clone() };
        let msg = Box::new(m);
        crate::rt::spawn(async move {
            let ok = job.run().await;
            let _ = job.input.send(Input::Staged { generation, msg, ok });
        });
    }

    // ------------------------------------------------------------------------------ both

    fn send(&self, machine: &str, msg: Msg, bytes: Vec<u8>) {
        let _ = self.out.try_send(Out { machine: machine.to_owned(), msg, bytes });
    }

    /// A profiler's upload sent on to the server (kept here, on the server): one the server is not
    /// connected for is dropped, as Datadog's Agent drops what it cannot forward.
    fn send_profile(&mut self, payload: Vec<u8>) {
        if self.is_server() {
            super::profiles::from_runner(&crate::os::hostname(), payload);
            return;
        }
        if !self.server_up {
            return;
        }
        self.part = self.part.wrapping_add(1);
        for (msg, b) in wire::frames(self.part, payload, |id, part, parts| Msg::Profile { id, part, parts }) {
            self.send("", msg, b);
        }
    }

    fn send_raft(&mut self, machine: &str, bytes: Vec<u8>) {
        self.part = self.part.wrapping_add(1);
        for (msg, b) in wire::frames(self.part, bytes, |id, part, parts| Msg::Raft { id, part, parts }) {
            self.send(machine, msg, b);
        }
    }

    /// This node's writes: proposed here (the server) or sent to the server (a runner).
    fn writes(&mut self) {
        let Some((seq, mut entry)) = self.batcher.poll(Instant::now(), super::clock_ms()) else { return };
        // Its time is when it is sent, each time it is (a write may wait for a connection): what the
        // server measures this machine's clock by.
        kiln_obs::command::write_at(&mut entry, Some(super::clock_ms()));
        if self.is_server() {
            self.count_quotas(&entry);
            if let Some(rep) = &mut self.rep {
                let _ = rep.r.propose(Vec::new(), entry);
            }
        } else if self.server_up {
            let sender = self.batcher.sender();
            self.part = self.part.wrapping_add(1);
            for (msg, b) in wire::frames(self.part, entry, |id, part, parts| Msg::Write { sender, seq, id, part, parts }) {
                self.send("", msg, b);
            }
        }
    }

    fn drive(&mut self) {
        self.wakeup.pending.store(false, Ordering::SeqCst);
        let now = now_ns(self.start);
        let mut messages = Vec::new();
        let mut answers = Vec::new();
        let Some(rep) = &mut self.rep else { return };
        let pumped = group::pump(&mut rep.r, now, &self.waker, &mut rep.out, &mut messages, &mut answers);
        self.wake_at = pumped.as_ref().ok().copied().flatten().into_iter().chain(rep.r.deadline()).min();
        if let Err(e) = &pumped {
            let why = format!("{e:?}: {}", rep.r.machine().fault.clone().unwrap_or_default());
            // Opened again later, not at once: a fault that comes back (a full disk) would
            // otherwise have the log replayed and fail again, over and over.
            self.stops = self.stops.saturating_add(1);
            tracing::warn!(error = %why, again_in = ?self.reopen_after(), "observe's replica stopped; it opens again");
            self.error = Some(why);
            self.rep = None;
            self.maintain = None;
            self.tried = Some(Instant::now());
            return;
        }
        let asked = self.compact_asked && rep.r.in_flight() == 0;
        let keep = if asked { 0 } else { group::keep(&rep.r, rep.compacted) };
        if rep.waiting.is_none() && rep.r.applied().index > rep.cut {
            rep.waiting = Some(now);
        }
        let due = asked || group::due(&rep.r);
        if due && rep.r.compact(keep, now, &self.waker).unwrap_or(false) {
            self.compact_asked = false;
            rep.compacted = rep.r.applied().index;
            (rep.cut, rep.waiting) = (rep.compacted, None);
        } else if !due && rep.waiting.is_some_and(|since| group::due_by_age(&rep.r, rep.cut, Duration::from_nanos(now.saturating_sub(since)), cut_after())) {
            // Entries that came slowly have waited long enough: a compaction cuts the store first,
            // whether or not the log's start then moves (a few entries, all of them kept).
            if rep.r.compact(keep, now, &self.waker).unwrap_or(false) {
                rep.compacted = rep.r.applied().index;
            }
            (rep.cut, rep.waiting) = (rep.r.applied().index, None);
        }
        // A cut made blocks: maintenance merges them (the logarithmic method keeps a few).
        let durable = rep.r.machine().read().durable().index;
        if durable != rep.told {
            rep.told = durable;
            if let Some(m) = &self.maintain {
                m.poke();
            }
        }
        for (_, answer) in answers {
            let Answer::Write { sender, seq, outcome } = answer else { continue };
            let outcome = Outcome::of(&outcome);
            if sender == self.batcher.sender() {
                self.batcher.answered(seq, &outcome, Instant::now());
            } else if self.is_server()
                && let Some(machine) = self.writers.get(&sender).cloned()
            {
                self.send(&machine, Msg::Written { sender, seq, outcome }, Vec::new());
            }
        }
        for m in messages {
            let to = m.to;
            let bytes = m.encode_to_vec();
            if self.is_server() {
                let Some(machine) = self.machine_of(to).map(str::to_owned) else { continue };
                if !self.up.contains(&machine) {
                    if let Some(rep) = &mut self.rep {
                        let _ = rep.r.report_unreachable(to);
                    }
                    continue;
                }
                let snapshot = m.msg_type == MessageType::MsgSnapshot;
                self.send_raft(&machine, bytes);
                // A snapshot's files are fetched before it is stepped; the leader goes on as if it
                // arrived, and sends it again if the member is still behind.
                if snapshot && let Some(rep) = &mut self.rep {
                    let _ = rep.r.report_snapshot(to, true);
                }
            } else if self.server_up {
                self.send_raft("", bytes);
            }
        }
        if self.is_server() {
            self.reconcile();
            self.lead();
        }
    }

    /// How long after its last try a replica that is not open is tried again: a second after it
    /// stopped once, doubling each stop since it last ran well, up to [`REOPEN_MAX`]; a replica
    /// that never opened (another process holds its folder), every [`STATUS_EVERY`].
    fn reopen_after(&self) -> Duration {
        if self.stops == 0 {
            return STATUS_EVERY;
        }
        Duration::from_secs(1u64 << self.stops.saturating_sub(1).min(6)).min(REOPEN_MAX)
    }

    /// Looks at the room left on observe's disk: too little for the next cut (the log's
    /// compactable bytes, and [`DISK_MARGIN`]), exporters are refused and a compaction is asked
    /// for, which cuts and frees the log; said once each way.
    fn look_at_disk(&mut self) {
        if self.disk_seen.is_some_and(|t| t.elapsed() < DISK_EVERY) {
            return;
        }
        self.disk_seen = Some(Instant::now());
        self.disk_free = super::disk_free(&self.dir);
        let need = self.rep.as_ref().map_or(0, |r| r.r.compactable_bytes()).saturating_add(DISK_MARGIN);
        // Two marks, as a watermark has: refused under what the next cut needs, taken again only
        // once a margin more is free, so a cut that frees a little does not open and close the
        // door every few seconds.
        let low = self.disk_free.is_some_and(|f| if self.disk_low { f < need.saturating_add(DISK_MARGIN) } else { f < need });
        if low && self.rep.as_ref().is_some_and(|r| r.r.compactable_bytes() > 0) {
            self.compact_asked = true;
        }
        self.admission.disk_low.store(low, Ordering::Release);
        if low != self.disk_low {
            self.disk_low = low;
            let free = self.disk_free.unwrap_or(0);
            let (message, severity) = if low {
                (format!("observe's disk has {} free, too little for its next cut ({} wanted): exporters are refused until a cut frees room", size(free), size(need)), 13)
            } else {
                (format!("observe's disk has room again ({} free)", size(free)), 9)
            };
            tracing::warn!("{message}");
            let attrs = vec![("event.type".to_owned(), "observe.disk".to_owned()), ("disk_free".to_owned(), free.to_string())];
            self.batcher.logs(
                super::ingest::Logs::events(vec![Record {
                    ts: super::clock_ms().saturating_mul(1_000_000),
                    severity,
                    service: "kiln.observe".into(),
                    host: crate::os::hostname(),
                    source: "kiln.event".into(),
                    message,
                    attrs,
                    trace_id: None,
                    span_id: None,
                }]),
                Instant::now(),
            );
        }
    }

    fn periodic(&mut self) {
        self.look_at_disk();
        // A replica that has run well a while: its next stop is retried quickly again.
        if self.rep.is_some() && self.opened_at.is_some_and(|t| t.elapsed() >= REOPEN_MAX) {
            self.stops = 0;
        }
        // A replica that did not open (its folder held by another process of this machine, or a
        // fault) is tried again.
        if self.rep.is_none() && self.tried.is_none_or(|t| t.elapsed() >= self.reopen_after()) {
            match (&self.role, &self.me) {
                (Role::Server { .. }, _) => self.open(SERVER),
                (Role::Runner { .. }, Some(me)) => {
                    let member = me.member;
                    self.open(member);
                }
                (Role::Runner { .. }, None) => {}
            }
        }
        if !self.is_server() && self.said.elapsed() >= STATUS_EVERY && self.server_up {
            self.said = Instant::now();
            let status = self.status();
            self.send("", Msg::Status(status), Vec::new());
        }
        if self.sized.is_none_or(|t| t.elapsed() >= SIZED_EVERY) {
            self.sized = Some(Instant::now());
            self.dir_bytes = dir_bytes(&self.dir);
        }
        if self.viewed.elapsed() >= VIEW_EVERY {
            self.viewed = Instant::now();
            let view = self.make_view();
            *lock(&self.view) = view;
        }
    }

    /// The full digest of the replica, taken on a thread of its own under the store's read lock
    /// (a read of everything), said when it is done.
    fn digest_full(&mut self) {
        if self.digesting {
            self.digest_again = true;
            return;
        }
        let Some(rep) = &self.rep else { return };
        let (store, input) = (rep.r.machine().shared(), self.input.clone());
        let spawned = std::thread::Builder::new().name("kiln-observe-digest".into()).spawn(move || {
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            let _ = input.send(match s.digest(true) {
                Ok(digest) => Input::Verified { at: s.applied(), digest },
                Err(_) => Input::DigestFailed,
            });
        });
        match spawned {
            Ok(_) => self.digesting = true,
            Err(e) => tracing::warn!(error = %e, "observe's digest did not start"),
        }
    }

    fn status(&self) -> Status {
        let mut s = Status { error: self.error.clone(), at: crate::util::now_unix(), ..Status::default() };
        let (waiting, _) = self.batcher.backlog();
        s.queued = waiting;
        s.dropped = self.batcher.dropped + self.admission.refused.load(Ordering::Relaxed);
        s.ahead = self.batcher.ahead;
        s.fetching = self.fetching.as_ref().map(|f| f.progress);
        s.verified = self.verified;
        if let Some(rep) = &self.rep {
            let store = rep.r.machine().read();
            s.member = rep.r.id();
            s.digest = store.digest(false).unwrap_or(0);
            s.applied = store.applied();
            s.durable = store.durable().index;
            s.series = store.series_count() as u64;
            s.blocks = store.block_count() as u64;
        }
        s.disk_bytes = self.dir_bytes;
        s.scrapes = super::scrape::get().map(|s| s.states()).unwrap_or_default();
        s.collect = super::collect::get().map(|c| c.states()).unwrap_or_default();
        s.synthetics = super::synthetics::runner().map(|r| r.states()).unwrap_or_default();
        s.statsd = super::statsd::get().map(|r| r.states()).unwrap_or_default();
        s
    }

    fn make_view(&self) -> View {
        let (waiting, _) = self.batcher.backlog();
        let mut v = View { role: if self.is_server() { "server" } else { "runner" }, waiting, dropped: self.batcher.dropped + self.admission.refused.load(Ordering::Relaxed), written: self.batcher.written, ahead: self.batcher.ahead, error: self.error.clone(), ..View::default() };
        if let Some(rep) = &self.rep {
            let store = rep.r.machine().read();
            v.member = rep.r.id();
            v.leader = rep.r.leader();
            v.term = rep.r.term();
            v.applied = rep.r.applied().index;
            v.durable = store.durable().index;
            v.series = store.series_count() as u64;
            v.blocks = store.block_count() as u64;
            v.log_bytes = store.log_bytes();
            v.span_bytes = store.span_bytes();
            let heap = store.heap();
            (v.heap_symbols, v.heap_index, v.heap_samples) = (heap.symbols as u64, heap.index as u64, heap.samples as u64);
            v.cutoff_ms = store.cutoff();
            v.voters = rep.r.configuration().voters.clone();
            v.learners = rep.r.configuration().learners.clone();
            v.digest = store.digest(false).unwrap_or(0);
            v.digest_at = store.applied();
        }
        v.disk_bytes = self.dir_bytes;
        (v.disk_free, v.disk_low) = (self.disk_free, self.disk_low);
        v.verified = self.verified;
        if self.is_server() {
            v.machines = self.members.machines.iter().map(|(machine, &member)| MachineView { machine: machine.clone(), member, up: self.up.contains(machine), status: self.statuses.get(machine).cloned(), clock_skew_ms: self.skews.get(machine).map(|s| s.0) }).collect();
        }
        v
    }
}

/// A snapshot's files, fetched from the server into the staging folder.
struct Fetch {
    http: Arc<Client>,
    url: String,
    me: String,
    token: String,
    staging: PathBuf,
    image: Image,
    generation: u64,
    input: Sender<Input>,
}

impl Fetch {
    async fn run(&self) -> bool {
        let staging = self.staging.clone();
        let fresh = crate::rt::blocking(move || {
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)
        })
        .await;
        if !matches!(fresh, Ok(Ok(()))) {
            return false;
        }
        let total = self.image.file_bytes();
        let mut done = 0u64;
        for f in &self.image.files {
            if !self.one(f.name.as_str(), f.kind, f.bytes, f.crc).await {
                tracing::debug!(file = %f.name, "a snapshot's file did not come");
                return false;
            }
            done += f.bytes;
            let _ = self.input.send(Input::Fetched { generation: self.generation, done, of: total });
        }
        true
    }

    /// One file: fetched to `<name>.part`, checked whole against the image, then named.
    async fn one(&self, name: &str, kind: FileKind, bytes: u64, crc: u32) -> bool {
        let machine: String = self.me.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect();
        let url = format!("{}/{name}?machine={machine}", self.url);
        let Ok(mut resp) = self.http.get(&url).bearer(&self.token).timeout(Duration::from_secs(3600)).send().await else { return false };
        if !resp.is_success() {
            return false;
        }
        let part = self.staging.join(format!("{name}.part"));
        let Ok((sink, written)) = crate::rt::file_sink(part.clone(), bytes.min(SNAPSHOT_FILE_MAX)) else { return false };
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if sink.send(chunk).await.is_err() {
                        return false;
                    }
                }
                Ok(None) => break,
                Err(_) => return false,
            }
        }
        drop(sink);
        if !written.await.is_ok_and(|n| n == bytes) {
            return false;
        }
        let to = self.staging.join(name);
        let checked = crate::rt::blocking(move || -> bool {
            let ok = match kind {
                FileKind::Block => kiln_obs::block::Block::open(&part).is_ok_and(|b| b.bytes == bytes && b.crc == crc && b.verify().is_ok()),
                FileKind::Logs | FileKind::Spans => kiln_obs::logs::file::LogFile::open(&part).is_ok_and(|l| l.bytes == bytes && l.crc == crc && l.verify().is_ok()),
            };
            ok && std::fs::rename(&part, &to).is_ok()
        })
        .await;
        checked.unwrap_or(false)
    }
}

/// The maintenance thread of a store: blocks rewritten apart from the group's thread, when a cut
/// asks and every [`MAINTAIN_EVERY`] besides. Dropped, it is told to end and waited for: a rewrite
/// it is building is taken in (or not) first, so the store it writes is never written by another
/// opened on the same folder after.
struct Maintenance {
    tx: Option<Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Maintenance {
    fn start(store: Shared, dir: PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::Builder::new().name("kiln-observe-maintain".into()).spawn(move || maintain(&store, &rx, &dir));
        if let Err(e) = &thread {
            tracing::warn!(error = %e, "observe's maintenance thread did not start");
        }
        Self { tx: Some(tx), thread: thread.ok() }
    }

    fn poke(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(());
        }
    }
}

impl Drop for Maintenance {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn maintain(store: &Shared, rx: &Receiver<()>, dir: &Path) {
    {
        loop {
            match rx.recv_timeout(MAINTAIN_EVERY) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while rx.try_recv().is_ok() {}
            loop {
                // Each change of the files beside queries (`kiln_obs::store::cut_shared`): written
                // with no exclusive hold of the store, taken in under a brief one.
                tracing::debug!(blocks = ?store.read().unwrap_or_else(PoisonError::into_inner).block_list().iter().rev().take(4).collect::<Vec<_>>(), "observe's maintenance looks");
                if let Err(e) = kiln_obs::store::drop_expired_shared(store) {
                    tracing::warn!(error = %e, "observe's expired files were not deleted");
                }
                let Some(plan) = kiln_obs::store::plan_shared(store) else { break };
                // A rewrite writes its block before its inputs go: only with room for it.
                let inputs = plan.input_bytes();
                if super::disk_free(dir).is_some_and(|f| f < inputs.saturating_add(DISK_MARGIN)) {
                    tracing::debug!(inputs, "observe's rewrite waits for room on the disk");
                    break;
                }
                tracing::debug!(inputs, "observe's rewrite planned");
                match plan.build() {
                    Ok(built) => match kiln_obs::store::commit_shared(store, built) {
                        Ok(done) => tracing::debug!(?done, "observe's rewrite taken in"),
                        Err(e) => {
                            tracing::warn!(error = %e, "observe's rewritten block was not taken in");
                            break;
                        }
                    },
                    // Its inputs went (an install), or the disk refused: looked at again next time.
                    Err(e) => {
                        tracing::debug!(error = %e, "observe's rewrite stopped");
                        break;
                    }
                }
            }
        }
    }
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    crate::sync::group::save_atomic(path, &serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

/// The bytes the files under `dir` hold, each file once however many names it has: a snapshot's
/// files are hard links to the store's (where links can be told apart: elsewhere the snapshots,
/// copies or links, are left out). At most 100,000 entries are looked at.
fn dir_bytes(dir: &Path) -> u64 {
    #[cfg(unix)]
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let (mut total, mut entries) = (0u64, 0usize);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            entries += 1;
            if entries > 100_000 {
                return total;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if cfg!(unix) || e.file_name() != kiln_obs::machine::SNAPSHOTS {
                    stack.push(e.path());
                }
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            if !m.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if !seen.insert((m.dev(), m.ino())) {
                    continue;
                }
            }
            total = total.saturating_add(m.len());
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_as_people_say_them() {
        assert_eq!(span(0), "0 s");
        assert_eq!(span(89_600), "90 s");
        assert_eq!(span(3_599_500), "1 h 0 min");
        assert_eq!(span(2_700_000), "45 min");
        assert_eq!(span(7_329_000), "2 h 2 min");
    }

    /// A decoding handed back still holding samples (the owner takes their keys alone) is handed
    /// out empty, its room kept: what is written into it next finds nothing of the last one's.
    #[test]
    fn a_decoding_handed_back_is_handed_out_empty() {
        let spares = Spares::default();
        let mut d = kiln_obs::otlp::metrics::Decoded::default();
        d.samples.extend((0..100).map(|i| (0u32, i64::from(i), 1.0)));
        d.points = 100;
        spares.give_decoded(d);
        let d = spares.decoded();
        assert!(d.keys.is_empty() && d.samples.is_empty() && d.points == 0);
        assert!(d.samples.capacity() >= 100, "its room kept");
    }

    /// Eight threads take room as fast as they can while one frees it at its own pace: what is
    /// inside never passes the bound, read where it can be read exactly (the freeing thread is the
    /// only writer of what left: what it reads of the taken is what is inside at that moment).
    #[test]
    fn admission_never_takes_more_than_its_bound_whatever_the_race() {
        const MAX: u64 = 10_000;
        let (taken, left) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let stop = Arc::new(AtomicBool::new(false));
        // The race is had, not hoped for: each taker makes 200,000 tries, and goes on past them until
        // ten bounds' worth was granted and some were refused (the tries alone had none on a
        // machine busy enough that the freer never ran meanwhile), backing off when refused as a
        // writer would; a minute at the most.
        let (granted, refusals) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let takers: Vec<_> = (0..8u64)
            .map(|t| {
                let (taken, left, granted, refusals) = (taken.clone(), left.clone(), granted.clone(), refusals.clone());
                std::thread::spawn(move || {
                    let (mut x, mut got, mut refused) = (t.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1, 0u64, 0u64);
                    let mut tries = 0u32;
                    while (tries < 200_000 || !(granted.load(Ordering::Acquire) > MAX * 10 && refusals.load(Ordering::Acquire) > 0)) && std::time::Instant::now() < deadline {
                        tries += 1;
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let n = 1 + x % 500;
                        if reserve(&taken, &left, n, MAX) {
                            got += n;
                            granted.fetch_add(n, Ordering::AcqRel);
                        } else {
                            refused += 1;
                            refusals.fetch_add(1, Ordering::AcqRel);
                            std::thread::yield_now();
                        }
                    }
                    (got, refused)
                })
            })
            .collect();
        let freer = {
            let (taken, left, stop) = (taken.clone(), left.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut most = 0;
                while !stop.load(Ordering::Acquire) {
                    let l = left.load(Ordering::Acquire);
                    let inside = taken.load(Ordering::Acquire) - l;
                    most = most.max(inside);
                    assert!(inside <= MAX, "{inside} inside a bound of {MAX}");
                    // Frees part of what is inside, as the owner cuts writes.
                    left.fetch_add(inside / 3, Ordering::AcqRel);
                    std::thread::yield_now();
                }
                most
            })
        };
        let (mut got, mut refused) = (0, 0);
        for t in takers {
            let (g, r) = t.join().unwrap_or_default();
            got += g;
            refused += r;
        }
        stop.store(true, Ordering::Release);
        let most = freer.join().unwrap_or(u64::MAX);
        assert_eq!(taken.load(Ordering::Acquire), got, "every grant counted once");
        assert!(most <= MAX && got > MAX * 10 && refused > 0, "a real race: {got} taken, {refused} refused, at most {most} inside");
        // One write bigger than the bound is taken when nothing is inside, and only then.
        let (t, l) = (AtomicU64::new(0), AtomicU64::new(0));
        assert!(reserve(&t, &l, MAX * 3, MAX));
        assert!(!reserve(&t, &l, 1, MAX));
        l.store(MAX * 3, Ordering::Release);
        assert!(reserve(&t, &l, 1, MAX));
    }
}
