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
use kiln_obs::machine::{Machine, Shared};
use kiln_obs::manifest::{Point, plain_name};
use kiln_obs::store::{Answer, snapshot_name};
use serde::{Deserialize, Serialize};

use super::ingest::{Batcher, WAITING_LOG_BYTES, WAITING_POINTS, record_bytes};
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
    /// How long samples and log records are kept (the server moves the cutoffs).
    pub metrics_retention: Duration,
    pub logs_retention: Duration,
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
    /// Bytes observe's folder holds on this machine (`dir_bytes`), and of them its logs files.
    pub disk_bytes: u64,
    pub log_bytes: u64,
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
    Samples(Vec<SeriesKey>, Vec<(u32, i64, f64)>),
    Logs(Vec<Record>),
    Staged { generation: u64, msg: Box<Message>, ok: bool },
    Fetched { generation: u64, done: u64, of: u64 },
    /// Cut the store and compact the log now, keeping nothing (an operator's, or a test's).
    Compact,
    /// Take the full digest, and on the server ask every replica for theirs.
    Verify,
    Verified { at: u64, digest: u64 },
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
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Observe {
    /// Starts the node's group (its thread, and its maintenance thread).
    pub fn start(settings: Settings) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out_rx) = async_channel::bounded(16_384);
        let store = Arc::new(Mutex::new(None));
        let view = Arc::new(Mutex::new(View::default()));
        let snaps = settings.dir.join("store").join(kiln_obs::machine::SNAPSHOTS);
        let key = match &settings.role {
            Role::Server { key } => Some(key.clone()),
            Role::Runner { .. } => None,
        };
        let admission = Arc::new(Admission::default());
        let handle = Self { tx: tx.clone(), admission: admission.clone(), store: store.clone(), view: view.clone(), out: out_rx, snaps, key };
        let spawned = std::thread::Builder::new().name("kiln-observe".into()).spawn(move || {
            let mut owner = Owner::new(settings, store, view, out_tx, tx, admission);
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

    /// Samples an exporter sent, as a request decoded them (`kiln_obs::otlp::metrics::Decoded`):
    /// as [`Observe::offer`].
    #[must_use]
    pub fn offer_samples(&self, keys: Vec<SeriesKey>, samples: Vec<(u32, i64, f64)>) -> bool {
        let n = samples.len() as u64;
        if n == 0 {
            return true;
        }
        let a = &self.admission;
        if a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire) || !reserve(&a.points, &a.left_points, n, WAITING_POINTS as u64 / 4 * 3) {
            return false;
        }
        if self.tx.send(Input::Samples(keys, samples)).is_err() {
            a.left_points.fetch_add(n, Ordering::AcqRel);
        }
        true
    }

    /// Log records of kiln's own (its events): taken up to the bound itself, else refused and
    /// counted as dropped.
    pub fn log(&self, records: Vec<Record>) {
        let _taken = self.log_within(records, WAITING_LOG_BYTES as u64, true);
    }

    /// Log records an exporter or a client sent: as [`Observe::offer`].
    #[must_use]
    pub fn offer_logs(&self, records: Vec<Record>) -> bool {
        self.log_within(records, WAITING_LOG_BYTES as u64 / 4 * 3, false)
    }

    fn log_within(&self, records: Vec<Record>, max: u64, count: bool) -> bool {
        if records.is_empty() {
            return true;
        }
        let n: u64 = records.iter().map(|r| record_bytes(r) as u64).sum();
        let a = &self.admission;
        if (!count && (a.congested.load(Ordering::Acquire) || a.disk_low.load(Ordering::Acquire))) || !reserve(&a.log_bytes, &a.left_log_bytes, n, max) {
            if count {
                a.refused.fetch_add(records.len() as u64, Ordering::Relaxed);
            }
            return false;
        }
        if self.tx.send(Input::Logs(records)).is_err() {
            a.left_log_bytes.fetch_add(n, Ordering::AcqRel);
        }
        true
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
    /// The store's durable point when maintenance was last told: a cut since is new blocks to
    /// merge, however it came (a compaction, a snapshot's persisting).
    told: u64,
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
    retention: (Duration, Duration),
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
    start: Instant,
    batcher: Batcher,
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
    fn new(settings: Settings, store: Arc<Mutex<Option<Shared>>>, view: Arc<Mutex<View>>, out: async_channel::Sender<Out>, input: Sender<Input>, admission: Arc<Admission>) -> Self {
        let wakeup = Arc::new(Wakeup { tx: input.clone(), pending: AtomicBool::new(false) });
        let waker = Waker::from(wakeup.clone());
        let mut o = Self {
            dir: settings.dir,
            role: settings.role,
            retention: (settings.metrics_retention, settings.logs_retention),
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
            start: Instant::now(),
            batcher: Batcher::new(rand::random()),
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
            Ok(Rep { compacted: r.applied().index, told: 0, r, _log: log, out: Output::default() })
        })();
        match opened {
            Ok(mut rep) => {
                if id == SERVER && rep.r.configuration().voters == [SERVER] {
                    let _ = rep.r.campaign();
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
            Input::Samples(keys, samples) => self.batcher.samples(keys, &samples, Instant::now()),
            Input::Logs(records) => self.batcher.logs(records, Instant::now()),
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
            Input::Verified { at, digest } => {
                self.verified = Some((at, digest));
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
                if let Some(rep) = &mut self.rep {
                    let _ = rep.r.propose(Vec::new(), entry);
                }
            }
            Msg::Status(s) => {
                self.statuses.insert(machine.to_owned(), s);
            }
            Msg::Written { .. } | Msg::Member { .. } | Msg::Release | Msg::Verify => {}
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
            self.batcher.logs(vec![Record { ts: now.saturating_mul(1_000_000), severity, service: "kiln.observe".into(), host: machine.to_owned(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }], Instant::now());
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
            Msg::Write { .. } | Msg::Status(_) => {}
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
        if (asked || group::due(&rep.r)) && rep.r.compact(keep, now, &self.waker).unwrap_or(false) {
            self.compact_asked = false;
            rep.compacted = rep.r.applied().index;
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
            self.batcher.logs(vec![Record { ts: super::clock_ms().saturating_mul(1_000_000), severity, service: "kiln.observe".into(), host: crate::os::hostname(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }], Instant::now());
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
        let Some(rep) = &self.rep else { return };
        let (store, input) = (rep.r.machine().shared(), self.input.clone());
        let spawned = std::thread::Builder::new().name("kiln-observe-digest".into()).spawn(move || {
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            if let Ok(digest) = s.digest(true) {
                let _ = input.send(Input::Verified { at: s.applied(), digest });
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "observe's digest did not start");
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
                FileKind::Logs => kiln_obs::logs::file::LogFile::open(&part).is_ok_and(|l| l.bytes == bytes && l.crc == crc && l.verify().is_ok()),
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
                let plan = {
                    let mut s = store.write().unwrap_or_else(PoisonError::into_inner);
                    tracing::debug!(blocks = ?s.block_list().iter().rev().take(4).collect::<Vec<_>>(), "observe's maintenance looks");
                    if let Err(e) = s.drop_expired() {
                        tracing::warn!(error = %e, "observe's expired files were not deleted");
                    }
                    s.plan_merge().or_else(|| s.plan_retention())
                };
                let Some(plan) = plan else { break };
                // A rewrite writes its block before its inputs go: only with room for it.
                let inputs = plan.input_bytes();
                if super::disk_free(dir).is_some_and(|f| f < inputs.saturating_add(DISK_MARGIN)) {
                    tracing::debug!(inputs, "observe's rewrite waits for room on the disk");
                    break;
                }
                tracing::debug!(inputs, "observe's rewrite planned");
                match plan.build() {
                    Ok(built) => match store.write().unwrap_or_else(PoisonError::into_inner).commit(built) {
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

    /// Eight threads take room as fast as they can while one frees it at its own pace: what is
    /// inside never passes the bound, read where it can be read exactly (the freeing thread is the
    /// only writer of what left: what it reads of the taken is what is inside at that moment).
    #[test]
    fn admission_never_takes_more_than_its_bound_whatever_the_race() {
        const MAX: u64 = 10_000;
        let (taken, left) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let stop = Arc::new(AtomicBool::new(false));
        let takers: Vec<_> = (0..8u64)
            .map(|t| {
                let (taken, left) = (taken.clone(), left.clone());
                std::thread::spawn(move || {
                    let (mut x, mut got, mut refused) = (t.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1, 0u64, 0u64);
                    for _ in 0..200_000 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let n = 1 + x % 500;
                        if reserve(&taken, &left, n, MAX) {
                            got += n;
                        } else {
                            refused += 1;
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
