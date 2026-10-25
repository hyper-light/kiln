//! The cluster group (docs/p2p.md, P1): kiln.db's writes go through one hyper-raft group, with the
//! store as its state machine. On the server alone it's a group of one voter, which commits alone;
//! runners join it as learners, then voters, in the phases after.
//!
//! A write runs as it always has, in a transaction of the store's connection, while the rows it
//! touches are noted ([`super::capture`]). When it's done, its net change is read and committed with
//! the write's number (`seq`, in [`STATE`]), and then proposed to the group as an entry: the log
//! takes the change right after, written alongside what other writes sent meanwhile, one flush for
//! all of them. So a write's caller gets its answer as fast as before the group, and is as durable
//! (kiln.db commits without waiting for the disk, as it always has).
//!
//! - **A restart replays** the log's entries kiln.db doesn't hold (a write whose commit the disk
//!   lost while the log's flush kept it): an entry applies when its `seq` is past kiln.db's, and is
//!   skipped otherwise (this node made it, so it's there). An entry's rows are written whole by key,
//!   so applying one twice leaves what applying it once does.
//! - **The other way** (kiln.db holds writes the log didn't take, a stop before they reached it),
//!   kiln.db is whole and the log goes on from it; a learner (P2) takes a copy of the database.
//! - **Seeded from today's store:** on first start the database as it is is the group's state at
//!   index 0, write 0.
//! - **Images (snapshots):** the log compacts behind what's applied and durable; on one node the
//!   image is kiln.db itself, so the machine's image is only where it stands.
//! - Writes forwarded by learners (P2), and a group with voters (P3), answer once committed.
//!
//! **Learners (P2).** A runner holds a replica: a copy of kiln.db, fetched from the server over
//! HTTP ([`Fetch`]) and made at a point the group had applied, then the log's entries past it. Any
//! copy made later stands in for that point: an entry its `seq` holds already is skipped, as a
//! restart skips them. So a learner the log has moved past (its entries compacted) installs a
//! fresh copy, and the image the group sends it is only where it stands. The server adds each
//! runner as a learner ([`Group::learners`]), one change at a time, and the group's messages to
//! and from the runners go through [`Group::outgoing`] and [`Group::step`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::task::{Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{Fatal, GroupStore, LogStore as _, OpenError, Output, Point, Replica, Settings, StateMachine, Unbounded};
use hyper_log::{Config as LogConfig, Facts, Log};
use hyper_raft::proto::{ConfChangeV2, ConfState, Message, MessageType};
use hyper_raft::wire::Record as _;
use hyper_raft::{Config, Elections, Limits, Stated, Timing};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use super::capture::{self, Change, Row, TableInfo};
use crate::sync::group::Conf;

/// The table that says where kiln.db stands: the last write it holds (`seq`) and the last entry
/// made durable with it (`applied`, `term`). One row, of exactly these four columns: every release
/// since P1 fills them by position as it starts, so one more there would stop each from starting on
/// this database, and a rollback to it with them.
pub const STATE: &str = "cluster_state";
/// The group's configuration where [`STATE`] stands (its voters and learners, as JSON), in a table of
/// its own for that reason. One row; none said means the server alone, as before the group had
/// learners.
pub const CONF: &str = "cluster_conf";
/// Where a build whose learners take a copy at a gap in the log's writes last said kiln.db stands
/// (its `applied`), in a table of its own too. A learner's replica whose [`STATE`] says otherwise
/// was kept by a build before those (or rolled back to one), which went on past such a gap without
/// the write, its replica looking whole: it's copied again as it starts.
pub const MARK: &str = "cluster_mark";
/// The log's id, the same in every kiln's file.
const LOG_ID: u128 = u128::from_be_bytes(*b"kiln-cluster-gp1");
/// The group, the log's only one.
const GROUP: u128 = 1;
/// This member: the server.
const SERVER: u64 = 1;
/// The block the log's file is written in.
const BLOCK: usize = 4096;
/// The most an entry carries of a change; a larger change goes in parts of this.
pub const LARGEST: usize = 8 << 20;
/// Bytes the log's file may take (it grows as it's used).
const DISK: u64 = 4 << 30;
/// Entries a compaction leaves behind what's applied (fewer in tests, which compact past a learner).
const KEEP: u64 = if cfg!(test) { 8 } else { 256 };
/// Changes waiting for the log, at most: past them a write waits its turn.
const QUEUED: usize = 1024;
/// Entries, and log bytes, between compactions.
const COMPACT_ENTRIES: u64 = if cfg!(test) { 64 } else { 8192 };
const COMPACT_BYTES: u64 = 128 << 20;
/// With learners, a compaction keeps up to this much of the log behind what's applied, at the
/// entries' mean size, for a learner a little behind (a runner reconnecting); one further behind
/// takes a copy of the database.
const KEEP_LEARNERS_BYTES: u64 = 64 << 20;
const KEEP_LEARNERS: u64 = if cfg!(test) { 16 } else { 65_536 };
/// How often the server's member beats while it has learners: a learner hears what's committed
/// when no write moves.
const BEAT: Duration = Duration::from_secs(1);
/// What the group's elections and the leader's own beat are timed by, until hyper-liveness measures
/// the paths between kiln's nodes (P3e, `Replica::measure`). Without one, the leader's beat never
/// runs: nothing counts how long a member hasn't answered, so a probe or an append lost on the way
/// with nobody saying so is never sent again, and that member waits until the leader restarts
/// (adacachy's learner, 2026-10-25, for hours). A follower draws no delay to campaign without one
/// either. A campaign is drawn over half a second, a vote round is half a second (a leader with
/// work in flight beats that often), and an election is expected to take two.
const TIMING: Timing = Timing { span: Duration::from_millis(500), round: Duration::from_millis(500), election: Duration::from_secs(2) };
/// A change of learners that hasn't applied in this long is proposed again.
const CHANGE_PATIENCE: Duration = Duration::from_secs(30);
/// The group's messages waiting for the links to the runners, at most (past them, they're
/// dropped: Raft sends what's needed again).
const OUTGOING: usize = 4096;
/// How many times a learner tries to fetch a copy of the database before its replica stops.
const FETCH_TRIES: u32 = 4;
/// Writes kept, at most, while the log has no room for them ([`Held`]): past it a write kiln.db
/// holds goes without its entry, and says so.
const HELD_BYTES: u64 = 256 << 20;
/// How long a stop waits for the log to have room for what's held.
const STOP_PATIENCE: Duration = Duration::from_secs(30);
/// How long a learner waits for another process to let its replica go (a machine's older runner
/// process, told to let its learner go as the newest takes it).
const ALONE_PATIENCE: Duration = Duration::from_secs(30);

/// Fetches a copy of the server's kiln.db into a file, made now (a learner's way to the database:
/// the runner's HTTP client to the server).
pub type Fetch = Arc<dyn Fn(&Path) -> anyhow::Result<()> + Send + Sync>;

/// Whether `db` is a copy of kiln.db a learner opens on: it says where it stands, and this build
/// said so last ([`MARK`]).
fn holds_a_copy(db: &Path) -> bool {
    db.exists()
        && Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .and_then(|c| c.query_row(&format!("SELECT s.applied = m.applied FROM {STATE} s, {MARK} m"), [], |r| r.get::<_, bool>(0)).ok())
            .unwrap_or(false)
}

/// Says a copy just taken stands where its [`STATE`] says ([`MARK`]).
fn mark(db: &Path) -> anyhow::Result<()> {
    let conn = Connection::open(db).with_context(|| format!("opening {} to mark it", db.display()))?;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {MARK} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL);
         INSERT INTO {MARK} (one, applied) SELECT 1, applied FROM {STATE} WHERE one = 1 ON CONFLICT (one) DO UPDATE SET applied = excluded.applied;"
    ))
    .with_context(|| format!("marking {}", db.display()))
}

/// The file beside `db` named with `suffix` after it (its `-wal`).
fn beside(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Removes `file`, if it's there.
fn remove(file: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(file) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(anyhow::Error::from(e).context(format!("removing {}", file.display()))),
        _ => Ok(()),
    }
}

/// Waits until no other process has the database at `db` open. SQLite leaves WAL mode only then,
/// its WAL checkpointed and gone: so nothing of theirs writes to the replica or its log after this,
/// nor takes the WAL of a copy put in its place as it closes (SQLite removes a WAL by name). A file
/// that isn't a database is nobody's.
fn alone(db: &Path) -> anyhow::Result<()> {
    let since = Instant::now();
    loop {
        let left = Connection::open(db).and_then(|c| c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get::<_, String>(0)));
        match left {
            Ok(mode) if mode.eq_ignore_ascii_case("delete") => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(e, _)) if !matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => return Ok(()),
            _ if since.elapsed() >= ALONE_PATIENCE => bail!("another process still has {} open", db.display()),
            _ => thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// A runner's member in the group: from its machine's name, stable, never the server's.
pub fn member_of(machine: &str) -> u64 {
    // FNV-1a: the same on every build and platform.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in machine.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h.max(SERVER + 1)
}

/// The log's layout. Every value is fixed: a log reopens with the facts it was made with.
fn log_config(align: Alignment) -> anyhow::Result<LogConfig> {
    LogConfig::derive(&Facts {
        align,
        sealed: false,
        largest_entry: LARGEST + (64 << 10),
        disk_bytes: DISK,
        max_groups: 2,
        cadence_entries: 4096,
        cadence_bytes: 256 << 20,
        uncommitted_entries: 2048,
        uncommitted_bytes: 64 << 20,
        // Little to gain on one node from keeping recent entries in memory (the server's pod is
        // tight on memory): what replays at a start reads the file.
        cache_bytes: 8 << 20,
    })
    .map_err(|e| anyhow::anyhow!("the cluster log's settings: {e:?}"))
}

/// An entry: one write's change, or a part of it (a large one).
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    part: u32,
    parts: u32,
    rows: Vec<Row>,
}

/// A change in entries of at most [`LARGEST`] bytes.
fn entries(seq: u64, change: Change) -> anyhow::Result<Vec<Vec<u8>>> {
    let mut groups: Vec<Vec<Row>> = vec![Vec::new()];
    let mut size = 0usize;
    for row in change.rows {
        let n = postcard::experimental::serialized_size(&row).unwrap_or(0) + 8;
        if size + n > LARGEST && groups.last().is_some_and(|g| !g.is_empty()) {
            groups.push(Vec::new());
            size = 0;
        }
        size += n;
        if let Some(g) = groups.last_mut() {
            g.push(row);
        }
    }
    let parts = u32::try_from(groups.len()).unwrap_or(u32::MAX);
    groups.into_iter().enumerate().map(|(i, rows)| postcard::to_allocvec(&Entry { seq, part: u32::try_from(i).unwrap_or(u32::MAX), parts, rows }).context("encoding an entry")).collect()
}

type Member = Replica<GroupStore<DeviceFile>, Machine, Unbounded>;

/// What the group's thread is asked.
enum Request {
    /// A write's change, committed to kiln.db, for the log.
    Append(u64, Change),
    /// An answer once everything asked before is applied.
    Flush(mpsc::SyncSender<Result<(), String>>),
    /// A message from another member (a learner's to the server, the server's to a learner).
    Step(Box<Message>),
    /// The learners the group should have (the server's: the runners that hold a replica).
    Learners(BTreeSet<u64>),
    /// A member no link reaches now: the group stops sending it entries until it answers again.
    Unreachable(u64),
    Stop,
}

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// The group: its thread, which owns the log and the replica.
pub struct Group {
    tx: mpsc::SyncSender<Request>,
    thread: Thread,
    join: Option<JoinHandle<()>>,
    /// The last write kiln.db holds, the store's and the machine's both.
    seq: Arc<AtomicU64>,
    /// The last entry the group applied: how far the log has taken the writes.
    applied: Arc<AtomicU64>,
    /// The last entry applied with its term, and the group's configuration there: where a copy of
    /// kiln.db made from now on stands.
    point: Arc<Mutex<(Point, ConfState)>>,
    /// Where the log starts (its compaction's index): a member behind it is sent an image.
    log_start: Arc<AtomicU64>,
    /// The group's messages to the other members: to whom, and the message's bytes.
    outgoing: async_channel::Receiver<(u64, Vec<u8>)>,
    /// Why its driver stopped, once it has (it applies nothing from then on).
    stopped: Arc<Mutex<Option<String>>>,
}

/// What opening a member of the group found wrong.
enum Opening {
    /// A learner's log starts past its copy of the database: the log goes, the copy stays.
    LogPastCopy,
    Failed(anyhow::Error),
}

impl Group {
    /// Opens the server's member of the group for the database at `db`, its log in `dir`, and
    /// replays the log's writes kiln.db doesn't hold.
    pub fn open(db: &Path, dir: &Path) -> anyhow::Result<Group> {
        match Self::start(db, dir, SERVER, None) {
            Ok(g) => Ok(g),
            Err(Opening::LogPastCopy) => bail!("the cluster log starts past kiln.db"),
            Err(Opening::Failed(e)) => Err(e),
        }
    }

    /// Opens a runner's learner, `member` of the group, on its replica at `db` (fetched with
    /// `fetch` when it has none) and its own log in `dir`.
    pub fn learner(db: &Path, dir: &Path, member: u64, fetch: Fetch) -> anyhow::Result<Group> {
        if member == SERVER {
            bail!("a learner is never the server's member");
        }
        if let Some(d) = db.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("making {}", d.display()))?;
        }
        // A machine's older runner process lets its replica go as the newest takes it (the server
        // tells both at once): nothing here touches the replica, its log or its WAL until it has.
        if db.exists() {
            alone(db)?;
        }
        if !holds_a_copy(db) {
            // None yet, or one an earlier build kept: a copy made now, its log starting again at it
            // (and the replica's WAL gone, which would be read into the copy).
            if db.exists() {
                tracing::info!("this machine's replica of kiln's records was kept by an earlier build, which could go past writes the log lacked: it's copied again");
            }
            let part = db.with_extension("copy");
            fetch(&part).context("fetching a copy of kiln.db")?;
            for gone in [dir.join("cluster.log"), beside(db, "-wal"), beside(db, "-shm")] {
                remove(&gone)?;
            }
            std::fs::rename(&part, db).with_context(|| format!("keeping the copy at {}", db.display()))?;
            mark(db)?;
        }
        match Self::start(db, dir, member, Some(fetch.clone())) {
            Ok(g) => Ok(g),
            Err(Opening::LogPastCopy) => {
                // A log that went on past the copy (a copy older than it): it starts again at the
                // copy's point, and the group sends what follows.
                std::fs::remove_file(dir.join("cluster.log")).ok();
                Self::start(db, dir, member, Some(fetch)).map_err(|e| match e {
                    Opening::LogPastCopy => anyhow::anyhow!("the learner's new log starts past its copy"),
                    Opening::Failed(e) => e,
                })
            }
            Err(Opening::Failed(e)) => Err(e),
        }
    }

    fn start(db: &Path, dir: &Path, member: u64, fetch: Option<Fetch>) -> Result<Group, Opening> {
        let failed = Opening::Failed;
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display())).map_err(failed)?;
        let path = dir.join("cluster.log");
        let fresh = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
        let align = Alignment::new(BLOCK).map_err(|e| failed(anyhow::anyhow!("{e:?}")))?;
        let file = DeviceFile::open(&path, true, CachingRequest::PreferDirect, align).map_err(|e| failed(anyhow::anyhow!("{}: {e:?}", path.display())))?;
        let config = log_config(file.layout_block()).map_err(failed)?;
        let log = if fresh {
            Log::create(file, config, LOG_ID).map_err(|e| failed(anyhow::anyhow!("making the cluster log at {}: {e:?}", path.display())))?
        } else {
            Log::open(file, config, LOG_ID).map(|(log, _)| log).map_err(|e| failed(anyhow::anyhow!("opening the cluster log at {}: {e:?}", path.display())))?
        };
        let store = GroupStore::claim(&log, GROUP).map_err(|e| failed(anyhow::anyhow!("claiming the cluster log's group: {e:?}")))?;
        let learner = fetch.is_some();
        let machine = Machine::open(db, fetch).map_err(failed)?;
        let seq = machine.seq.clone();
        let point = Arc::new(Mutex::new((machine.applied, machine.configuration.clone())));
        let limits = Limits::derive(Stated { message: LARGEST + (64 << 10) + hyper_raft::wire::MESSAGE_RECORD_FIXED_BYTES, members: 64, memory: 64 << 20, depth: 1 })
            .map_err(|e| failed(anyhow::anyhow!("the cluster group's limits: {e:?}")))?;
        let settings = Settings {
            core: Config { max_size_per_msg: (LARGEST + (64 << 10)) as u64, check_quorum: true, pre_vote: true, seed: member, ..Config::new(member, limits) },
            // Driven by events: what's proposed is written at the first lull, together.
            quiet: Duration::ZERO,
            elections: Elections::Suspicion,
        };
        let (tx, rx) = mpsc::sync_channel(QUEUED);
        let (out_tx, outgoing) = async_channel::bounded(OUTGOING);
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), Opening>>();
        let applied = Arc::new(AtomicU64::new(0));
        let log_start = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(Mutex::new(None));
        let (applied2, point2, log_start2, stopped2) = (applied.clone(), point.clone(), log_start.clone(), stopped.clone());
        let join = thread::Builder::new()
            .name("kiln-cluster".into())
            .spawn(move || {
                let waker = Waker::from(Arc::new(Unpark(thread::current())));
                let mut replica = match Replica::open(&settings, store, machine, Unbounded) {
                    Ok(r) => r,
                    Err(OpenError::StartPastMachine { .. }) if learner => {
                        ready_tx.send(Err(Opening::LogPastCopy)).ok();
                        return;
                    }
                    Err(e) => {
                        ready_tx.send(Err(Opening::Failed(anyhow::anyhow!("opening the cluster group: {e:?}")))).ok();
                        return;
                    }
                };
                if let Err(e) = replica.set_timing(TIMING) {
                    ready_tx.send(Err(Opening::Failed(anyhow::anyhow!("timing the cluster group's beat: {e:?}")))).ok();
                    return;
                }
                let mut d = Driver {
                    replica,
                    _log: log,
                    waker,
                    rx,
                    pending: std::collections::HashMap::new(),
                    next: 1,
                    start: Instant::now(),
                    since: 0,
                    broken: None,
                    applied: applied2,
                    point: point2,
                    log_start: log_start2,
                    stopped: stopped2,
                    outgoing: out_tx,
                    server: !learner,
                    wanted: None,
                    changing: None,
                    beat: Instant::now(),
                    held: Held::default(),
                };
                d.run(ready_tx);
            })
            .context("starting the cluster group's thread")
            .map_err(failed)?;
        let thread = join.thread().clone();
        match ready_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(failed(anyhow::anyhow!("the cluster group didn't start within 60 s"))),
        }
        Ok(Group { tx, thread, join: Some(join), seq, applied, point, log_start, outgoing, stopped })
    }

    /// Where it stands: kiln.db's last write, and the last entry applied (`/healthz/server`).
    pub fn standing(&self) -> (u64, u64) {
        (self.seq.load(Ordering::SeqCst), self.applied.load(Ordering::SeqCst))
    }

    /// Why the group's driver stopped, if it has: it applies nothing more (a runner starts its
    /// learner again).
    pub fn stopped(&self) -> Option<String> {
        self.stopped.lock().clone()
    }

    /// The last entry applied, with its term, and the group's configuration there: where a copy
    /// of kiln.db made from now on stands (every entry up to it is in kiln.db before it's logged).
    pub fn point(&self) -> (Point, ConfState) {
        self.point.lock().clone()
    }

    /// Where the log starts: a member that applied no further is sent an image, not entries.
    pub fn log_start(&self) -> u64 {
        self.log_start.load(Ordering::SeqCst)
    }

    /// The group's messages to the other members, as they come: the member, and the message.
    pub fn outgoing(&self) -> async_channel::Receiver<(u64, Vec<u8>)> {
        self.outgoing.clone()
    }

    /// A message to this member from another (its bytes as [`Group::outgoing`] gave them there).
    pub fn step(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let m = Message::decode(bytes).map_err(|e| anyhow::anyhow!("a cluster group message that doesn't read: {e:?}"))?;
        self.tx.try_send(Request::Step(Box::new(m))).map_err(|_| anyhow::anyhow!("the cluster group is busy or stopped"))?;
        self.thread.unpark();
        Ok(())
    }

    /// The learners the group should have: each runner holding a replica (the server's member).
    pub fn learners(&self, members: BTreeSet<u64>) {
        if self.tx.try_send(Request::Learners(members)).is_ok() {
            self.thread.unpark();
        }
    }

    /// A member no link reaches now (its runner is away).
    pub fn unreachable(&self, member: u64) {
        if self.tx.try_send(Request::Unreachable(member)).is_ok() {
            self.thread.unpark();
        }
    }

    /// The number the next write commits with (the store's writes come one at a time).
    pub fn next_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst) + 1
    }

    /// Write `seq` (committed to kiln.db) and its change, for the log: it goes in its turn, after
    /// the writes before it; past [`QUEUED`] waiting, this waits too.
    pub fn append(&self, seq: u64, change: Change) {
        self.seq.store(seq, Ordering::SeqCst);
        if self.tx.send(Request::Append(seq, change)).is_err() {
            tracing::error!(seq, "the cluster group has stopped: a write kiln.db holds isn't in its log");
        }
        self.thread.unpark();
    }

    /// Waits until every change sent is in the log and applied (tests; a clean stop).
    pub fn flush(&self) -> anyhow::Result<()> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.tx.send(Request::Flush(reply)).map_err(|_| anyhow::anyhow!("the cluster group stopped"))?;
        self.thread.unpark();
        match answer.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("the cluster group stopped before it answered"),
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.tx.send(Request::Stop).ok();
        self.thread.unpark();
        if let Some(j) = self.join.take() {
            j.join().ok();
        }
    }
}

/// What waits to go into the log, in order, while it has no room. A replica stalled for room
/// refuses every proposal and makes again only the writes it had taken, so a write the store sent
/// then would be in kiln.db and never in the log, and every learner would need a new copy of the
/// database for it. So each part and barrier is offered from here, oldest first, and nothing goes
/// before one the log had no room for.
#[derive(Default)]
struct Held {
    items: std::collections::VecDeque<Item>,
    /// The parts' bytes.
    bytes: u64,
}

/// A thing for the log: a write's part (its `seq` and the entry's bytes), or a barrier (its id).
enum Item {
    Part(u64, Vec<u8>),
    Barrier(u64),
}

/// What an [`Item`] was, to say what became of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum What {
    Part(u64),
    Barrier(u64),
}

/// Where held items go: the replica (a stand-in in tests).
trait Proposer {
    /// Whether it takes a proposal now (a replica stalled for room refuses each).
    fn room(&self) -> bool;
    /// Proposes `item`; an error is a refusal for good, not for room.
    fn offer(&mut self, item: Item) -> Result<(), String>;
}

impl Proposer for Member {
    fn room(&self) -> bool {
        !self.is_stalled()
    }

    fn offer(&mut self, item: Item) -> Result<(), String> {
        let proposed = match item {
            Item::Part(_, data) => self.propose(Vec::new(), data),
            Item::Barrier(id) => self.propose(id.to_le_bytes().to_vec(), Vec::new()),
        };
        proposed.map_err(|e| format!("{e:?}"))
    }
}

impl Item {
    fn what(&self) -> What {
        match self {
            Item::Part(seq, _) => What::Part(*seq),
            Item::Barrier(id) => What::Barrier(*id),
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            Item::Part(_, data) => data.len() as u64,
            Item::Barrier(_) => 0,
        }
    }
}

impl Held {
    fn push(&mut self, item: Item) {
        self.bytes = self.bytes.saturating_add(item.bytes());
        self.items.push_back(item);
    }

    /// Offers what's held to `to`, oldest first, while it has room: what it refused for good, and
    /// why.
    fn drain(&mut self, to: &mut impl Proposer) -> Vec<(What, String)> {
        let mut refused = Vec::new();
        while to.room() {
            let Some(item) = self.items.pop_front() else { break };
            self.bytes = self.bytes.saturating_sub(item.bytes());
            let what = item.what();
            if let Err(why) = to.offer(item) {
                refused.push((what, why.clone()));
                // A write goes into the log whole or not at all: its parts after a refused one go
                // too, so a learner meets its absence as a gap in the writes' numbers.
                if let What::Part(seq) = what {
                    while matches!(self.items.front(), Some(Item::Part(s, _)) if *s == seq) {
                        if let Some(rest) = self.items.pop_front() {
                            self.bytes = self.bytes.saturating_sub(rest.bytes());
                            refused.push((rest.what(), why.clone()));
                        }
                    }
                }
            }
        }
        refused
    }

    /// Everything held, given up (the group failed, or stopped with no room).
    fn give_up(&mut self) -> Vec<What> {
        self.bytes = 0;
        self.items.drain(..).map(|i| i.what()).collect()
    }
}

struct Driver {
    replica: Member,
    // Kept open for as long as the replica writes to it.
    _log: Log<DeviceFile>,
    waker: Waker,
    rx: mpsc::Receiver<Request>,
    /// Proposals answered once applied: the barrier and flushes.
    pending: std::collections::HashMap<u64, mpsc::SyncSender<Result<(), String>>>,
    next: u64,
    start: Instant,
    /// What was applied at the last compaction.
    since: u64,
    /// Why the group can't take writes, once it can't.
    broken: Option<String>,
    /// The last entry applied, for the store to say.
    applied: Arc<AtomicU64>,
    /// The last entry applied with its term, and the configuration there (for copies).
    point: Arc<Mutex<(Point, ConfState)>>,
    /// Where the log starts, for [`Group::log_start`].
    log_start: Arc<AtomicU64>,
    /// The group's messages to the other members.
    outgoing: async_channel::Sender<(u64, Vec<u8>)>,
    /// Why it stopped, for [`Group::stopped`].
    stopped: Arc<Mutex<Option<String>>>,
    /// The server's member (the group's leader), or a runner's learner.
    server: bool,
    /// The learners the group should have, once the server has said.
    wanted: Option<BTreeSet<u64>>,
    /// A change of learners under way: the learner, added or not, and when it was proposed.
    changing: Option<(u64, bool, Instant)>,
    /// When the server's member last beat.
    beat: Instant,
    /// Writes' parts and barriers waiting for room in the log, in order.
    held: Held,
}

impl Driver {
    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn run(&mut self, ready: mpsc::Sender<Result<(), Opening>>) {
        if self.server {
            if let Err(e) = self.lead() {
                ready.send(Err(Opening::Failed(e))).ok();
                return;
            }
            self.replica.machine_mut().opened();
        } else if let Err(e) = self.drive() {
            ready.send(Err(Opening::Failed(e))).ok();
            return;
        }
        self.since = self.replica.applied().index;
        ready.send(Ok(())).ok();
        let mut stopping: Option<Instant> = None;
        loop {
            while let Ok(req) = self.rx.try_recv() {
                match req {
                    Request::Append(seq, change) => self.append(seq, change),
                    Request::Flush(reply) => self.barrier(reply),
                    Request::Step(m) => {
                        if let Err(e) = self.replica.step(*m) {
                            tracing::debug!("a cluster group message wasn't taken: {e:?}");
                        }
                    }
                    Request::Learners(members) => self.wanted = Some(members),
                    Request::Unreachable(member) => {
                        self.replica.report_unreachable(member).ok();
                    }
                    Request::Stop if stopping.is_none() => {
                        // What was sent before goes into the log first (the server's; a learner
                        // proposes nothing).
                        if self.server {
                            let (reply, _answer) = mpsc::sync_channel(1);
                            self.barrier(reply);
                        }
                        stopping = Some(Instant::now());
                    }
                    Request::Stop => {}
                }
            }
            if let Err(e) = self.drive() {
                tracing::error!("the cluster group stopped: {e:#}");
                for (_, reply) in self.pending.drain() {
                    reply.send(Err(format!("the cluster group failed: {e:#}"))).ok();
                }
                self.broken = Some(format!("{e:#}"));
                *self.stopped.lock() = Some(format!("{e:#}"));
                for what in self.held.give_up() {
                    if let What::Part(seq) = what {
                        tracing::error!(seq, "a write kiln.db holds isn't in the cluster log: {e:#}");
                    }
                }
                // What the store sends from now on can't be logged: take it and say so.
                while let Ok(req) = self.rx.recv() {
                    match req {
                        Request::Flush(reply) => {
                            reply.send(Err(format!("the cluster group failed: {e:#}"))).ok();
                        }
                        Request::Stop => return,
                        _ => {}
                    }
                }
                return;
            }
            if self.server {
                self.reconcile();
                self.beat_learners();
            }
            // Compacting makes kiln.db durable (a commit that waits for the disk, holding its write
            // lock), so it's every so many writes, not by the image's size (on one node a marker).
            let applied = self.replica.applied().index;
            if self.replica.is_stalled() || applied.saturating_sub(self.since) >= COMPACT_ENTRIES || self.replica.compactable_bytes() >= COMPACT_BYTES {
                let now = self.now();
                if self.replica.compact(self.keep().min(applied), now, &self.waker).unwrap_or(false) {
                    self.since = applied;
                }
            }
            if let Ok((start, _)) = self.replica.log_mut().bounds() {
                self.log_start.store(start.index, Ordering::SeqCst);
            }
            // What waited for room goes in now there's some, in order.
            self.propose_held();
            if let Some(since) = stopping {
                if since.elapsed() >= STOP_PATIENCE {
                    self.give_up_held("the log had no room before the group stopped");
                }
                if self.pending.is_empty() {
                    self.replica.machine_mut().persist().ok();
                    return;
                }
            }
            let wait = self.replica.deadline().map(|d| Duration::from_nanos(d.saturating_sub(self.now()))).unwrap_or(Duration::from_millis(100)).min(Duration::from_millis(100));
            thread::park_timeout(wait.max(Duration::from_micros(50)));
        }
    }

    /// The server's member takes the lead: the sole voter elects itself, then a barrier (an entry
    /// of its own) applies, and with it everything before (the log's writes kiln.db didn't hold),
    /// so the store serves what's committed.
    fn lead(&mut self) -> anyhow::Result<()> {
        self.replica.campaign().map_err(|e| anyhow::anyhow!("campaigning: {e:?}"))?;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            self.drive()?;
            if self.replica.is_leader() || Instant::now() >= until {
                break;
            }
            thread::park_timeout(Duration::from_millis(1));
        }
        if !self.replica.is_leader() {
            bail!("the cluster group's sole voter didn't elect itself");
        }
        let (reply, answer) = mpsc::sync_channel(1);
        self.barrier(reply);
        self.settle()?;
        if !matches!(answer.try_recv(), Ok(Ok(()))) {
            bail!("the cluster group's first entry didn't apply");
        }
        Ok(())
    }

    /// Entries a compaction keeps behind what's applied: with learners, as many as fit
    /// [`KEEP_LEARNERS_BYTES`] at the entries' mean size, for one a little behind.
    fn keep(&self) -> u64 {
        if self.replica.is_stalled() || self.replica.configuration().learners.is_empty() {
            return KEEP;
        }
        let entries = self.replica.applied().index.saturating_sub(self.since).max(1);
        let mean = (self.replica.compactable_bytes() / entries).max(1);
        (KEEP_LEARNERS_BYTES / mean).clamp(KEEP, KEEP_LEARNERS)
    }

    /// Brings the group's learners to those wanted, one change at a time.
    fn reconcile(&mut self) {
        let Some(wanted) = &self.wanted else { return };
        let conf = self.replica.configuration();
        if let Some((member, add, at)) = self.changing {
            let done = conf.learners.contains(&member) == add;
            if !done && at.elapsed() < CHANGE_PATIENCE {
                return;
            }
            self.changing = None;
        }
        let have: BTreeSet<u64> = conf.learners.iter().copied().collect();
        let change = wanted.difference(&have).next().map(|m| (*m, true)).or_else(|| have.difference(wanted).next().map(|m| (*m, false)));
        if let Some((member, add)) = change
            && self.replica.change(Vec::new(), &crate::sync::group::learner(member, add)).is_ok()
        {
            tracing::info!(member, add, "the cluster group's learners change");
            self.changing = Some((member, add, Instant::now()));
        }
    }

    /// The server's member beats while it has learners: they hear what's committed.
    fn beat_learners(&mut self) {
        if self.beat.elapsed() >= BEAT && !self.replica.configuration().learners.is_empty() {
            self.beat = Instant::now();
            self.replica.beat().ok();
        }
    }

    /// Proposes a write's change, in parts if it's large.
    fn append(&mut self, seq: u64, change: Change) {
        if let Some(why) = &self.broken {
            tracing::error!(seq, "a write kiln.db holds isn't in the cluster log: {why}");
            return;
        }
        let parts = match entries(seq, change) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(seq, "a write kiln.db holds isn't in the cluster log: {e:#}");
                return;
            }
        };
        // Whole or not at all: a learner joins a write's parts before it applies them.
        let bytes: u64 = parts.iter().map(|p| p.len() as u64).sum();
        if self.held.bytes.saturating_add(bytes) > HELD_BYTES {
            tracing::error!(seq, held = self.held.bytes, "a write kiln.db holds isn't in the cluster log: no room for {} MiB of writes (a learner takes a copy at the gap)", HELD_BYTES >> 20);
            return;
        }
        for data in parts {
            self.held.push(Item::Part(seq, data));
        }
        self.propose_held();
    }

    /// Offers the log what's held, oldest first, while it has room ([`Held`]), and says what it
    /// refused for good.
    fn propose_held(&mut self) {
        for (what, why) in self.held.drain(&mut self.replica) {
            self.refused(what, &why);
        }
    }

    /// Gives up what's held, saying why: each write it held isn't in the log, each barrier fails.
    fn give_up_held(&mut self, why: &str) {
        for what in self.held.give_up() {
            self.refused(what, why);
        }
    }

    fn refused(&mut self, what: What, why: &str) {
        match what {
            What::Part(seq) => tracing::error!(seq, "a write kiln.db holds isn't in the cluster log: {why}"),
            What::Barrier(id) => {
                if let Some(reply) = self.pending.remove(&id) {
                    reply.send(Err(format!("the cluster group refused an entry: {why}"))).ok();
                }
            }
        }
    }

    /// Proposes an entry of no change, answered once applied: everything before it is too (it goes
    /// in behind what's held).
    fn barrier(&mut self, reply: mpsc::SyncSender<Result<(), String>>) {
        let id = self.next;
        self.next += 1;
        self.pending.insert(id, reply);
        self.held.push(Item::Barrier(id));
        self.propose_held();
    }

    /// Drives until the barriers proposed are applied.
    fn settle(&mut self) -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(120);
        while !self.pending.is_empty() {
            self.drive()?;
            self.propose_held();
            if Instant::now() >= until {
                bail!("the cluster group didn't apply its log within 120 s");
            }
            if !self.pending.is_empty() {
                thread::park_timeout(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// Drives the replica until it rests, answering what applied and sending its messages.
    fn drive(&mut self) -> anyhow::Result<()> {
        let mut out: Output<Answer> = Output::default();
        for _ in 0..1024 {
            out.clear();
            let driven = self.replica.drive(self.now(), &self.waker, &mut out).map_err(|e| anyhow::anyhow!("driving the cluster group: {e:?}"))?;
            for (id, result) in out.answers.drain(..) {
                if let Some(reply) = self.pending.remove(&id) {
                    reply.send(result).ok();
                }
            }
            for m in out.messages.drain(..) {
                let (to, snapshot) = (m.to, m.msg_type == MessageType::MsgSnapshot);
                // A message the links can't take now is dropped: Raft sends what's needed again.
                let sent = self.outgoing.try_send((to, m.encode_to_vec())).is_ok();
                if snapshot {
                    // The image is only where the group stands (the learner fetches the database).
                    self.replica.report_snapshot(to, sent).ok();
                }
            }
            if !driven.more {
                break;
            }
        }
        let at = self.replica.applied();
        self.applied.store(at.index, Ordering::SeqCst);
        *self.point.lock() = (at, self.replica.configuration().clone());
        Ok(())
    }
}

/// What applying a barrier answers: its request id and how it went.
type Answer = (u64, Result<(), String>);

/// kiln.db as the group's state machine: entries applied on a connection of its own.
struct Machine {
    conn: Connection,
    /// Where kiln.db is (a learner's copy is fetched beside it).
    path: PathBuf,
    known: std::collections::HashMap<String, TableInfo>,
    applied: Point,
    durable: Point,
    configuration: ConfState,
    /// The last write kiln.db holds (shared with the store, which commits writes).
    seq: Arc<AtomicU64>,
    /// The last write the log held when the group opened (to say what kiln.db holds past it).
    logged: u64,
    /// A learner's way to a copy of the database; none on the server, which never takes one.
    fetch: Option<Fetch>,
}

/// Where kiln.db stands, as [`STATE`] says: its last entry applied (with its term), its last
/// write, and the group's configuration there ([`CONF`]).
fn stands(conn: &Connection) -> anyhow::Result<(Point, u64, ConfState)> {
    let (index, term, seq): (i64, i64, i64) = conn.query_row(&format!("SELECT applied, term, seq FROM {STATE}"), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let conf: Option<String> = conn.query_row(&format!("SELECT conf FROM {CONF} WHERE one = 1"), [], |r| r.get(0)).optional()?.flatten();
    let at = Point { index: u64::try_from(index).unwrap_or(0), term: u64::try_from(term).unwrap_or(0) };
    // Before the group had learners, its configuration was the server alone, unsaid.
    let conf = conf.and_then(|c| serde_json::from_str::<Conf>(&c).ok()).map_or_else(|| ConfState { voters: vec![SERVER], ..ConfState::default() }, |c| ConfState::from(&c));
    Ok((at, u64::try_from(seq).unwrap_or(0), conf))
}

/// Says in [`STATE`], [`CONF`] and [`MARK`] that kiln.db stands at `at` under `conf`, in one commit
/// that waits for the disk (kiln.db's others don't): everything before it is durable too.
pub(super) fn stand(conn: &Connection, at: Point, conf: &ConfState) -> anyhow::Result<()> {
    let (index, term, conf) = (i64::try_from(at.index)?, i64::try_from(at.term)?, serde_json::to_string(&Conf::from(conf))?);
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {CONF} (one INTEGER PRIMARY KEY CHECK (one = 1), conf TEXT);
         CREATE TABLE IF NOT EXISTS {MARK} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL);"
    ))?;
    conn.execute_batch("PRAGMA synchronous = FULL")?;
    let done = (|| -> anyhow::Result<()> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let wrote = conn
            .execute(&format!("UPDATE {STATE} SET applied = ?1, term = ?2 WHERE one = 1"), rusqlite::params![index, term])
            .and_then(|_| conn.execute(&format!("INSERT INTO {CONF} (one, conf) VALUES (1, ?1) ON CONFLICT (one) DO UPDATE SET conf = excluded.conf"), [&conf]))
            .and_then(|_| conn.execute(&format!("INSERT INTO {MARK} (one, applied) VALUES (1, ?1) ON CONFLICT (one) DO UPDATE SET applied = excluded.applied"), [index]));
        match wrote {
            Ok(_) => Ok(conn.execute_batch("COMMIT")?),
            Err(e) => {
                conn.execute_batch("ROLLBACK").ok();
                Err(e.into())
            }
        }
    })();
    conn.execute_batch("PRAGMA synchronous = NORMAL").ok();
    done
}

/// A database that builds from before [`CONF`] kept the configuration on [`STATE`] in, as a fifth
/// column: it moves to [`CONF`] and the column goes, in one commit, so the releases before them
/// start on it again. The column's, when it says one, is the newest: only those builds write it.
fn unwiden(conn: &Connection) -> anyhow::Result<()> {
    let wide: bool = conn.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{STATE}') WHERE name = 'conf'"), [], |r| r.get::<_, i64>(0))? > 0;
    if !wide {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let done = conn.execute_batch(&format!(
        "INSERT INTO {CONF} (one, conf) SELECT 1, conf FROM {STATE} WHERE one = 1 AND conf IS NOT NULL ON CONFLICT (one) DO UPDATE SET conf = excluded.conf;
         ALTER TABLE {STATE} DROP COLUMN conf;"
    ));
    match done {
        Ok(()) => Ok(conn.execute_batch("COMMIT")?),
        Err(e) => {
            conn.execute_batch("ROLLBACK").ok();
            Err(anyhow::Error::from(e).context(format!("moving the cluster group's configuration off {STATE}")))
        }
    }
}

impl Machine {
    /// Opens kiln.db at `db` to apply entries: foreign keys off (an entry's rows hold its cascades
    /// already), and where it stands read from [`STATE`] (made, at 0, on the first start).
    fn open(db: &Path, fetch: Option<Fetch>) -> anyhow::Result<Machine> {
        let conn = Connection::open(db).with_context(|| format!("opening {} for the cluster group", db.display()))?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF; PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS {STATE} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL, term INTEGER NOT NULL, seq INTEGER NOT NULL);
             INSERT OR IGNORE INTO {STATE} (one, applied, term, seq) VALUES (1, 0, 0, 0);
             CREATE TABLE IF NOT EXISTS {CONF} (one INTEGER PRIMARY KEY CHECK (one = 1), conf TEXT);
             CREATE TABLE IF NOT EXISTS {MARK} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL);"
        ))?;
        unwiden(&conn)?;
        let (at, seq, configuration) = stands(&conn)?;
        Ok(Machine {
            conn,
            path: db.to_path_buf(),
            known: std::collections::HashMap::new(),
            applied: at,
            durable: at,
            configuration,
            seq: Arc::new(AtomicU64::new(seq)),
            logged: 0,
            fetch,
        })
    }

    /// The log has been replayed: says what kiln.db holds that it doesn't.
    fn opened(&self) {
        let seq = self.seq.load(Ordering::SeqCst);
        if seq > self.logged && self.applied.index > 1 {
            tracing::warn!(seq, logged = self.logged, "kiln.db holds writes the cluster log doesn't (a stop before they reached it); a learner (P2) takes a copy of the database");
        }
    }

    /// Fetches a copy of the database made now and takes it in whole, as its readers see it at
    /// once; says it stands at `at` under `configuration`. Its last write's number, now this one's.
    fn take_a_copy(&mut self, at: Point, configuration: &ConfState) -> Result<u64, Fatal> {
        let Some(fetch) = self.fetch.clone() else { return Err(Fatal("only a learner takes a copy of the database")) };
        let part = self.path.with_extension("copy");
        let took = (|| -> anyhow::Result<u64> {
            // The server may be busy or a moment away: a few tries before the learner gives up.
            let mut tried = 0;
            while let Err(e) = fetch(&part) {
                tried += 1;
                if tried >= FETCH_TRIES {
                    return Err(e.context("fetching a copy of kiln.db"));
                }
                tracing::warn!(tried, "fetching a copy of kiln.db: {e:#}");
                thread::sleep(if cfg!(test) { Duration::from_millis(10) } else { Duration::from_secs(2 << tried) });
            }
            {
                let copy = Connection::open_with_flags(&part, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                rusqlite::backup::Backup::new(&copy, &mut self.conn)?.run_to_completion(4096, Duration::ZERO, None)?;
            }
            self.conn.execute_batch("PRAGMA journal_mode = WAL")?;
            let (_, seq, _) = stands(&self.conn)?;
            stand(&self.conn, at, configuration)?;
            Ok(seq)
        })();
        std::fs::remove_file(&part).ok();
        match took {
            Ok(seq) => {
                self.seq.store(seq, Ordering::SeqCst);
                self.known.clear();
                Ok(seq)
            }
            Err(e) => {
                tracing::error!("a learner couldn't take a copy of kiln.db: {e:#}");
                Err(Fatal("a copy of kiln.db couldn't be taken"))
            }
        }
    }

    /// A learner meets writes its log lacks: a copy of the database now, said to stand where this
    /// one is durable (its log holds everything after that; what it applies of it the copy holds
    /// already is skipped by its `seq`). Its replica answers nothing meanwhile: its last write stays
    /// behind the server's.
    fn copy_again(&mut self) -> Result<(), Fatal> {
        let (at, configuration) = (self.durable, self.configuration.clone());
        let seq = self.take_a_copy(at, &configuration)?;
        tracing::info!(index = at.index, seq, "a learner took a copy of kiln.db over writes its log lacked");
        Ok(())
    }

    /// Applies an entry's rows (a write kiln.db doesn't hold) and, with its last part, its `seq`.
    fn apply_entry(&mut self, e: &Entry) -> anyhow::Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let done = capture::apply(&self.conn, &Change { rows: e.rows.clone() }, &mut self.known).and_then(|()| {
            if e.part + 1 >= e.parts {
                self.conn.execute(&format!("UPDATE {STATE} SET seq = ?1 WHERE one = 1"), [i64::try_from(e.seq)?])?;
            }
            Ok(())
        });
        match done {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                if e.part + 1 >= e.parts {
                    self.seq.store(e.seq, Ordering::SeqCst);
                }
                Ok(())
            }
            Err(err) => {
                self.conn.execute_batch("ROLLBACK").ok();
                Err(err)
            }
        }
    }
}

impl StateMachine for Machine {
    type Answer = Answer;

    fn apply(&mut self, entry: &hyper_durable::EntryRef<'_>, answers: &mut Vec<Answer>) -> Result<(), Fatal> {
        if entry.index != self.applied.index + 1 {
            return Err(Fatal("an entry applied out of order"));
        }
        self.applied = Point { index: entry.index, term: entry.term };
        let id = entry.context.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes).unwrap_or(0);
        if entry.data.is_empty() {
            // A barrier, or a new leader's empty entry.
            if id != 0 {
                answers.push((id, Ok(())));
            }
            return Ok(());
        }
        match postcard::from_bytes::<Entry>(entry.data) {
            Ok(e) => {
                self.logged = self.logged.max(e.seq);
                // A learner hears the writes one number after another. A number past the next is
                // writes the log lacks (the server stopped before they reached it, or its log had no
                // room for them), and its replica would answer without them: it takes a copy of the
                // database first, which has them (the server's kiln.db holds every write).
                let have = self.seq.load(Ordering::SeqCst);
                if self.fetch.is_some() && e.part == 0 && e.seq > have.saturating_add(1) {
                    tracing::warn!(index = entry.index, seq = e.seq, have, "the cluster log lacks writes this replica doesn't hold: it takes a copy of kiln.db");
                    self.copy_again()?;
                }
                // kiln.db holds what this node wrote (it committed it first); a write past that is
                // one the disk lost from kiln.db and the log kept.
                // (A large write's number is recorded with its last part, so its parts all apply.)
                if e.seq > self.seq.load(Ordering::SeqCst)
                    && let Err(err) = self.apply_entry(&e)
                {
                    // Deterministic: a replay meets the same. The log goes on.
                    tracing::error!(index = entry.index, seq = e.seq, "a cluster log entry didn't apply: {err:#}");
                }
            }
            Err(err) => tracing::error!(index = entry.index, "a cluster log entry didn't read: {err}"),
        }
        Ok(())
    }

    fn apply_change(&mut self, at: Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        if at.index != self.applied.index + 1 {
            return Err(Fatal("a configuration change applied out of order"));
        }
        self.configuration = configuration.clone();
        self.applied = at;
        Ok(())
    }

    fn durable(&self) -> Point {
        self.durable
    }

    fn configuration(&self) -> &ConfState {
        &self.configuration
    }

    fn acts_at_start(&self, _entry: &hyper_durable::EntryRef<'_>) -> bool {
        false
    }

    fn image(&mut self, into: &mut Vec<u8>) -> Result<(Point, ConfState), Fatal> {
        // On one node the image is kiln.db itself: only where it stands (P2 sends the database).
        into.extend_from_slice(&self.applied.index.to_le_bytes());
        into.extend_from_slice(&self.applied.term.to_le_bytes());
        Ok((self.applied, self.configuration.clone()))
    }

    fn image_bytes(&self) -> Option<u64> {
        Some(16)
    }

    fn install(&mut self, _image: &[u8], at: Point, configuration: &ConfState) -> Result<(), Fatal> {
        // The image is only where the group stands: a learner fetches a copy of the database made
        // now, which stands in for `at` (the entries after `at` it holds already are skipped by
        // their `seq`).
        let seq = self.take_a_copy(at, configuration)?;
        tracing::info!(index = at.index, seq, "a learner took a copy of kiln.db");
        self.applied = at;
        self.durable = at;
        self.configuration = configuration.clone();
        Ok(())
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        if self.durable == self.applied {
            return Ok(());
        }
        // kiln.db commits without waiting for the disk (synchronous NORMAL); one that does makes
        // everything before it durable too, so the log may start past it.
        let at = self.applied;
        stand(&self.conn, at, &self.configuration).map_err(|_| Fatal("kiln.db couldn't be made durable"))?;
        self.durable = at;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    /// Where kiln.db stands: its last write's number, and the last entry made durable with it.
    fn state(db: &Path) -> anyhow::Result<(i64, i64)> {
        let c = Connection::open(db)?;
        Ok(c.query_row(&format!("SELECT seq, applied FROM {STATE}"), [], |r| Ok((r.get(0)?, r.get(1)?)))?)
    }

    fn kv(store: &Store, k: &str) -> anyhow::Result<Option<String>> {
        crate::rt::block_on({
            let (store, k) = (store.clone(), k.to_owned());
            async move { store.kv_get(k).await }
        })
    }

    fn set(store: &Store, k: &str, v: &str) -> anyhow::Result<()> {
        crate::rt::block_on({
            let (store, k, v) = (store.clone(), k.to_owned(), v.to_owned());
            async move { store.kv_set(k, v).await }
        })
    }

    #[test]
    fn writes_through_the_group_are_numbered_and_reads_write_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let store = Store::open_grouped(&db, &dir.path().join("cluster"))?;
        assert!(store.grouped());
        set(&store, "a", "1")?;
        assert_eq!(kv(&store, "a")?.as_deref(), Some("1"), "kept at once, as without the group");
        store.flush_group()?;
        assert_eq!(state(&db)?.0, 1, "the first write");
        assert_eq!(kv(&store, "nothing")?, None);
        set(&store, "b", "2")?;
        assert_eq!(state(&db)?.0, 2, "a read takes no number");
        Ok(())
    }

    /// What every release since P1 does to [`STATE`] as it starts (8adeadba's own statements): its
    /// four columns filled by position, then where kiln.db stands read and said again.
    fn start_a_release_from_before(db: &Path) -> anyhow::Result<()> {
        let c = Connection::open(db)?;
        c.execute_batch(
            "CREATE TABLE IF NOT EXISTS cluster_state (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL, term INTEGER NOT NULL, seq INTEGER NOT NULL);
             INSERT OR IGNORE INTO cluster_state VALUES (1, 0, 0, 0);",
        )?;
        let (applied, term): (i64, i64) = c.query_row("SELECT applied, term, seq FROM cluster_state", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        c.execute("UPDATE cluster_state SET applied = ?1, term = ?2 WHERE one = 1", [applied, term])?;
        Ok(())
    }

    /// A rollback starts the release before on what this one left. A fifth column on [`STATE`]
    /// stopped 8adeadba there: "table cluster_state has 5 columns but 4 values were supplied".
    #[test]
    fn the_release_before_starts_on_a_database_this_one_ran() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        {
            let store = Store::open_grouped(&db, &dir.path().join("cluster"))?;
            set(&store, "a", "1")?;
            store.flush_group()?;
        }
        start_a_release_from_before(&db)
    }

    /// A database an earlier build of this release ran keeps the configuration on [`STATE`], in a
    /// fifth column. Opened, the column goes and the configuration moves to [`CONF`], learners and
    /// all, and the release before starts on it again.
    #[test]
    fn a_database_an_earlier_build_widened_is_narrowed_and_keeps_its_configuration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let conf = Conf { voters: vec![SERVER], learners: vec![7, 9], ..Conf::default() };
        {
            let c = Connection::open(&db)?;
            c.execute_batch(&format!("CREATE TABLE {STATE} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL, term INTEGER NOT NULL, seq INTEGER NOT NULL, conf TEXT);"))?;
            c.execute(&format!("INSERT INTO {STATE} (one, applied, term, seq, conf) VALUES (1, 4, 2, 6, ?1)"), [serde_json::to_string(&conf)?])?;
        }
        let m = Machine::open(&db, None)?;
        assert_eq!(m.configuration.learners, vec![7, 9], "its learners kept");
        assert_eq!((m.applied.index, m.applied.term, m.seq.load(Ordering::SeqCst)), (4, 2, 6), "where it stands kept");
        drop(m);
        let columns: i64 = Connection::open(&db)?.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{STATE}')"), [], |r| r.get(0))?;
        assert_eq!(columns, 4, "the four columns every release since P1 fills");
        start_a_release_from_before(&db)?;
        // And this release again: the configuration still there.
        assert_eq!(Machine::open(&db, None)?.configuration.learners, vec![7, 9]);
        Ok(())
    }

    #[test]
    fn a_restart_replays_the_writes_kiln_db_lost_from_the_log() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (db, log) = (dir.path().join("kiln.db"), dir.path().join("cluster"));
        {
            let store = Store::open_grouped(&db, &log)?;
            for i in 0..5 {
                set(&store, &format!("k{i}"), &format!("v{i}"))?;
            }
            store.flush_group()?;
        }
        // kiln.db loses its last two writes (its commits didn't reach the disk; the log's did), and
        // where it stood goes back with them.
        {
            let c = Connection::open(&db)?;
            c.execute_batch(&format!("DELETE FROM kv WHERE k IN ('k3', 'k4'); UPDATE {STATE} SET seq = seq - 2, applied = 0"))?;
        }
        let store = Store::open_grouped(&db, &log)?;
        for i in 0..5 {
            assert_eq!(kv(&store, &format!("k{i}"))?, Some(format!("v{i}")), "k{i} is back from the log");
        }
        assert_eq!(state(&db)?.0, 5, "and so is where kiln.db stands");
        // Writes go on from there.
        set(&store, "k5", "v5")?;
        assert_eq!(state(&db)?.0, 6);
        Ok(())
    }

    #[test]
    fn a_write_that_fails_after_writing_keeps_what_it_kept_and_writers_at_once_all_land() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let store = Store::open_grouped(&db, &dir.path().join("cluster"))?;
        let failed = crate::rt::block_on({
            let s = store.clone();
            async move {
                s.call(|c| -> anyhow::Result<()> {
                    c.execute("INSERT INTO kv (k, v) VALUES ('kept', 'yes')", [])?;
                    anyhow::bail!("and then it failed")
                })
                .await
            }
        });
        assert!(failed.is_err());
        assert_eq!(kv(&store, "kept")?.as_deref(), Some("yes"), "as without the group: what it wrote before failing stays");
        let n = 40;
        crate::rt::block_on({
            let s = store.clone();
            async move {
                let tasks: Vec<_> = (0..n)
                    .map(|i| {
                        let s = s.clone();
                        crate::rt::spawn(async move { s.kv_set(format!("w{i}"), format!("{i}")).await })
                    })
                    .collect();
                for t in tasks {
                    t.await??;
                }
                Ok::<_, anyhow::Error>(())
            }
        })?;
        for i in 0..n {
            assert_eq!(kv(&store, &format!("w{i}"))?, Some(format!("{i}")));
        }
        store.flush_group()?;
        assert_eq!(state(&db)?.0, 1 + n, "each write its number");
        Ok(())
    }

    #[test]
    fn this_node_s_own_tables_go_past_the_group_and_not_through_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let store = Store::open_grouped(&db, &dir.path().join("cluster"))?;
        crate::rt::block_on({
            let s = store.clone();
            async move { s.call_local(|c| Ok(c.execute("INSERT INTO ci_log_indexed (leg, at, bytes) VALUES ('leg', 1, 2)", [])?)).await }
        })?;
        assert_eq!(state(&db)?.0, 0, "past the group: no number");
        let through = crate::rt::block_on({
            let s = store.clone();
            async move { s.call(|c| Ok(c.execute("INSERT INTO ci_log_indexed (leg, at, bytes) VALUES ('other', 1, 2)", [])?)).await }
        });
        assert!(through.is_err_and(|e| format!("{e:#}").contains("this node's own tables")));
        let n: i64 = Connection::open(&db)?.query_row("SELECT COUNT(*) FROM ci_log_indexed", [], |r| r.get(0))?;
        assert_eq!(n, 1, "the refused write left nothing");
        Ok(())
    }

    #[test]
    fn a_large_change_goes_in_parts_each_within_an_entry() -> anyhow::Result<()> {
        let rows: Vec<Row> = (0..7).map(|i| Row::Kept { table: "blob".into(), rowid: Some(i), values: vec![capture::Val::Blob(vec![7u8; 3 << 20])] }).collect();
        let parts = entries(42, Change { rows })?;
        assert!(parts.len() >= 3, "{} parts", parts.len());
        let mut seen = 0;
        for (i, p) in parts.iter().enumerate() {
            assert!(p.len() <= LARGEST + (64 << 10), "part {i} is {} bytes", p.len());
            let e: Entry = postcard::from_bytes(p)?;
            assert_eq!((e.seq, e.part as usize, e.parts as usize), (42, i, parts.len()));
            seen += e.rows.len();
        }
        assert_eq!(seen, 7, "every row, once");
        Ok(())
    }

    /// Waits up to 30 s for `ok`.
    fn until(what: &str, mut ok: impl FnMut() -> anyhow::Result<bool>) -> anyhow::Result<()> {
        let end = Instant::now() + Duration::from_secs(30);
        while !ok()? {
            if Instant::now() >= end {
                bail!("timed out waiting for {what}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    /// The links between the server's member and a learner, in a thread: each one's messages to the
    /// other, while `up` (a runner away misses them).
    fn link(server: Store, learner: Arc<Group>, member: u64, up: Arc<std::sync::atomic::AtomicBool>, done: Arc<std::sync::atomic::AtomicBool>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let Some(g) = server.group() else { return };
            let (to_learner, to_server) = (g.outgoing(), learner.outgoing());
            while !done.load(Ordering::SeqCst) {
                let mut moved = false;
                while let Ok((to, m)) = to_learner.try_recv() {
                    moved = true;
                    if to == member && up.load(Ordering::SeqCst) {
                        learner.step(&m).ok();
                    }
                }
                while let Ok((_, m)) = to_server.try_recv() {
                    moved = true;
                    if up.load(Ordering::SeqCst) {
                        g.step(&m).ok();
                    }
                }
                if !moved {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        })
    }

    /// A log that has room for `left` more proposals, then none until given more; what it took,
    /// in order; and an item it refuses for good, once.
    struct Room {
        left: usize,
        took: Vec<What>,
        refuse: Option<What>,
    }

    impl Proposer for Room {
        fn room(&self) -> bool {
            self.left > 0
        }

        fn offer(&mut self, item: Item) -> Result<(), String> {
            self.left = self.left.saturating_sub(1);
            if self.refuse == Some(item.what()) {
                self.refuse = None;
                return Err("refused".into());
            }
            self.took.push(item.what());
            Ok(())
        }
    }

    /// The lab's stall (2026-10-24): a replica stalled for room refused the store's writes, and
    /// each was in kiln.db and never in the log. Held, a write the log had no room for goes in
    /// once there is, and before every write sent after it, its parts and the barriers too.
    #[test]
    fn a_write_the_log_had_no_room_for_goes_in_before_every_write_after_it() {
        let mut held = Held::default();
        let mut log = Room { left: 1, took: Vec::new(), refuse: None };
        // Room for one: write 1 goes in; write 2 (two parts) and a flush's barrier wait.
        for item in [Item::Part(1, vec![1; 10]), Item::Part(2, vec![2; 10]), Item::Part(2, vec![2; 5]), Item::Barrier(7)] {
            held.push(item);
        }
        assert!(held.drain(&mut log).is_empty());
        assert_eq!(log.took, [What::Part(1)]);
        assert_eq!(held.bytes, 15, "write 2's parts are held");
        // Write 3, sent while write 2 waits, waits behind it.
        held.push(Item::Part(3, vec![3; 4]));
        assert!(held.drain(&mut log).is_empty());
        assert_eq!(log.took, [What::Part(1)], "nothing goes in without room");
        // Room for two: write 2's parts, and the log is full again before the barrier.
        log.left = 2;
        assert!(held.drain(&mut log).is_empty());
        assert_eq!(log.took, [What::Part(1), What::Part(2), What::Part(2)]);
        // Room: the barrier, then write 3, in the order sent.
        log.left = usize::MAX;
        assert!(held.drain(&mut log).is_empty());
        assert_eq!(log.took, [What::Part(1), What::Part(2), What::Part(2), What::Barrier(7), What::Part(3)]);
        assert_eq!((held.items.len(), held.bytes), (0, 0));
        // A refusal for good, not for room, is said, and what's after it still goes in.
        log.refuse = Some(What::Part(4));
        held.push(Item::Part(4, Vec::new()));
        held.push(Item::Part(5, Vec::new()));
        assert_eq!(held.drain(&mut log), [(What::Part(4), "refused".to_string())]);
        assert_eq!(log.took.last(), Some(&What::Part(5)));
        // Its first part refused, a write's other parts go with it, never offered: the write is
        // in the log whole or not at all. The write after it goes in.
        log.refuse = Some(What::Part(9));
        for item in [Item::Part(9, vec![9; 3]), Item::Part(9, vec![9; 2]), Item::Part(10, Vec::new())] {
            held.push(item);
        }
        let took = log.took.len();
        assert_eq!(held.drain(&mut log), [(What::Part(9), "refused".to_string()), (What::Part(9), "refused".to_string())]);
        assert_eq!(log.took.get(took..), Some(&[What::Part(10)][..]), "only the write after it went in");
        assert_eq!(held.bytes, 0);
        // Given up, everything held is said, in order.
        log.left = 0;
        held.push(Item::Part(6, vec![6]));
        held.push(Item::Barrier(8));
        assert_eq!(held.give_up(), [What::Part(6), What::Barrier(8)]);
        assert_eq!((held.items.len(), held.bytes), (0, 0));
    }

    #[test]
    fn a_learner_starts_from_a_copy_follows_the_log_and_takes_a_new_copy_once_left_behind() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (server, copies) = (server.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                copies.fetch_add(1, Ordering::SeqCst);
                crate::rt::block_on({
                    let (s, d) = (server.clone(), dest.to_path_buf());
                    async move { s.copy_for_learner(d).await }
                })
            })
        };
        let replica = dir.path().join("runner").join("kiln.db");
        let learner = Arc::new(Group::learner(&replica, &dir.path().join("runner").join("cluster"), member, fetch)?);
        assert_eq!(copies.load(Ordering::SeqCst), 1, "it started from a copy");
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        assert_eq!(read("before")?.as_deref(), Some("1"), "the copy holds what was written before it");
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up.clone(), done.clone());
        set(&server, "after", "2")?;
        until("the write after the copy on the learner", || Ok(read("after")?.as_deref() == Some("2")))?;
        assert_eq!(learner.standing().0, server.group_standing().map_or(0, |s| s.0), "the learner holds the server's last write");
        // Away while the server writes past what the log keeps for it: back, it takes a new copy.
        // Back only once the log starts past it: compactions go out when the log has room for
        // them, so the writes alone don't say one has.
        let away_at = learner.standing().1;
        up.store(false, Ordering::SeqCst);
        for i in 0..200 {
            set(&server, &format!("away{i}"), "x")?;
        }
        server.flush_group()?;
        until("the server's log starting past the learner", || Ok(g.log_start() > away_at))?;
        up.store(true, Ordering::SeqCst);
        until("the writes made while it was away", || Ok(read("away199")?.as_deref() == Some("x")))?;
        assert_eq!(copies.load(Ordering::SeqCst), 2, "left behind by the log, it took a new copy");
        set(&server, "later", "3")?;
        until("a write after the new copy", || Ok(read("later")?.as_deref() == Some("3")))?;
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        Ok(())
    }

    /// A write kiln.db holds and the log never got (the server stopped before it reached the log,
    /// or the log had no room past what's held): a learner meets it as a write number past the
    /// next, and takes a copy of the database before it applies anything after it. Its last write
    /// stays behind the server's until then, so its front door answers nothing from it.
    #[test]
    fn a_learner_that_meets_a_write_the_log_lacks_takes_a_copy_before_it_goes_on() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let server = Store::open_grouped(&db, &dir.path().join("cluster"))?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (server, copies) = (server.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                copies.fetch_add(1, Ordering::SeqCst);
                crate::rt::block_on({
                    let (s, d) = (server.clone(), dest.to_path_buf());
                    async move { s.copy_for_learner(d).await }
                })
            })
        };
        let replica = dir.path().join("runner").join("kiln.db");
        let learner = Arc::new(Group::learner(&replica, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up.clone(), done.clone());
        set(&server, "after", "2")?;
        until("the write after the copy on the learner", || Ok(read("after")?.as_deref() == Some("2")))?;
        assert_eq!(copies.load(Ordering::SeqCst), 1, "it started from a copy");
        // A write kiln.db commits that never reaches the log, as a stop before it did leaves one.
        let lost = g.next_seq();
        {
            let c = Connection::open(&db)?;
            c.execute("INSERT INTO kv (k, v) VALUES ('lost', 'l')", [])?;
            c.execute(&format!("UPDATE {STATE} SET seq = ?1 WHERE one = 1"), [i64::try_from(lost)?])?;
        }
        g.seq.store(lost, Ordering::SeqCst);
        // The next write is numbered past it: the learner meets the gap.
        set(&server, "next", "3")?;
        until("the write after the gap on the learner", || Ok(read("next")?.as_deref() == Some("3")))?;
        assert_eq!(read("lost")?.as_deref(), Some("l"), "it holds the write the log lacked");
        assert_eq!(copies.load(Ordering::SeqCst), 2, "it took a copy for it");
        set(&server, "later", "4")?;
        until("a write after the copy", || Ok(read("later")?.as_deref() == Some("4")))?;
        assert_eq!(copies.load(Ordering::SeqCst), 2, "one copy for the gap, not one a write");
        assert_eq!(learner.standing().0, server.group_standing().map_or(0, |s| s.0), "the learner holds the server's last write");
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        Ok(())
    }

    /// A replica a build before the mark kept may have gone on past a write the log lacked (its
    /// learners took no copy at a gap), its last write past the hole and looking whole: it's copied
    /// again as it starts, once. So is one a build before stood since (a rollback, then forward).
    #[test]
    fn a_replica_an_earlier_build_kept_is_copied_again_as_it_starts_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (server, copies) = (server.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                copies.fetch_add(1, Ordering::SeqCst);
                crate::rt::block_on({
                    let (s, d) = (server.clone(), dest.to_path_buf());
                    async move { s.copy_for_learner(d).await }
                })
            })
        };
        let (replica, logs) = (dir.path().join("runner").join("kiln.db"), dir.path().join("runner").join("cluster"));
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        // The learner starts, follows the server until it holds `k`, and stops.
        let follow = |k: &str| -> anyhow::Result<()> {
            let learner = Arc::new(Group::learner(&replica, &logs, member, fetch.clone())?);
            let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let links = link(server.clone(), learner.clone(), member, up, done.clone());
            let held = until(&format!("{k} on the learner"), || Ok(read(k)?.is_some()));
            done.store(true, Ordering::SeqCst);
            links.join().ok();
            held
        };
        set(&server, "after", "2")?;
        follow("after")?;
        assert_eq!(copies.load(Ordering::SeqCst), 1, "it started from a copy");
        // As a build before the mark leaves it: the write the log lacked missing, its last write
        // past it, and no mark.
        {
            let c = Connection::open(&replica)?;
            c.execute_batch(&format!("DELETE FROM kv WHERE k = 'after'; DROP TABLE {MARK};"))?;
        }
        set(&server, "next", "3")?;
        follow("next")?;
        assert_eq!(copies.load(Ordering::SeqCst), 2, "it took a copy as it started");
        assert_eq!(read("after")?.as_deref(), Some("2"), "the copy holds the write it lacked");
        // This build's replica goes on from where it is.
        set(&server, "later", "4")?;
        follow("later")?;
        assert_eq!(copies.load(Ordering::SeqCst), 2, "a replica this build kept isn't copied again");
        // A build before stood it since: where it stands moved on past the mark.
        {
            let c = Connection::open(&replica)?;
            c.execute(&format!("UPDATE {MARK} SET applied = applied - 1"), [])?;
        }
        set(&server, "last", "5")?;
        follow("last")?;
        assert_eq!(copies.load(Ordering::SeqCst), 3, "one a build before stood since is copied again");
        Ok(())
    }

    /// The server restarts (a deploy) while a runner's learner goes on: the restarted server leads
    /// a new term from its own files, and the learner follows it as it followed the one before
    /// (production, 2026-10-25: adacachy's learner applied nothing after the server restarted).
    #[test]
    fn a_learner_follows_the_server_after_it_restarts() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (db, logs) = (dir.path().join("kiln.db"), dir.path().join("cluster"));
        let server = Store::open_grouped(&db, &logs)?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        // Copies come from whichever server runs now (none while it's stopped).
        let current = Arc::new(Mutex::new(Some(server.clone())));
        let fetch: Fetch = {
            let current = current.clone();
            Arc::new(move |dest: &Path| {
                let (s, d) = (current.lock().clone().context("the server is stopped")?, dest.to_path_buf());
                crate::rt::block_on(async move { s.copy_for_learner(d).await })
            })
        };
        let replica = dir.path().join("runner").join("kiln.db");
        let learner = Arc::new(Group::learner(&replica, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up.clone(), done.clone());
        for i in 0..50 {
            set(&server, &format!("w{i}"), "x")?;
        }
        until("the writes on the learner", || Ok(read("w49")?.as_deref() == Some("x")))?;
        // The server stops (its links with it) and starts again on its files; the learner goes on.
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        *current.lock() = None;
        drop(server);
        let server = Store::open_grouped(&db, &logs)?;
        *current.lock() = Some(server.clone());
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up, done.clone());
        set(&server, "after", "2")?;
        let followed = until("a write after the restart on the learner", || Ok(read("after")?.as_deref() == Some("2")));
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        followed
    }

    /// As in production on 2026-10-25: the server restarts, writes go on while the runner is away,
    /// and the runner's learner starts again on its replica (no copy: this build kept it) once the
    /// runner is back. It follows the restarted server from where its replica stands.
    #[test]
    fn a_learner_started_again_on_its_replica_follows_a_restarted_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (db, logs) = (dir.path().join("kiln.db"), dir.path().join("cluster"));
        let server = Store::open_grouped(&db, &logs)?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        {
            let g = server.group().context("grouped")?;
            g.learners([member].into());
            until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        }
        let current = Arc::new(Mutex::new(Some(server.clone())));
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (current, copies) = (current.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                copies.fetch_add(1, Ordering::SeqCst);
                let (s, d) = (current.lock().clone().context("the server is stopped")?, dest.to_path_buf());
                crate::rt::block_on(async move { s.copy_for_learner(d).await })
            })
        };
        let (replica, learner_logs) = (dir.path().join("runner").join("kiln.db"), dir.path().join("runner").join("cluster"));
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        let learner = Arc::new(Group::learner(&replica, &learner_logs, member, fetch.clone())?);
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up, done.clone());
        for i in 0..50 {
            set(&server, &format!("w{i}"), "x")?;
        }
        until("the writes on the learner", || Ok(read("w49")?.as_deref() == Some("x")))?;
        // The server restarts; writes go on while the runner is away; then its learner starts again.
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        *current.lock() = None;
        drop(server);
        let server = Store::open_grouped(&db, &logs)?;
        *current.lock() = Some(server.clone());
        for i in 0..50 {
            set(&server, &format!("gap{i}"), "y")?;
        }
        drop(learner);
        let learner = Arc::new(Group::learner(&replica, &learner_logs, member, fetch)?);
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up, done.clone());
        set(&server, "after", "2")?;
        let followed = until("a write after both restarts on the learner", || Ok(read("after")?.as_deref() == Some("2")));
        eprintln!("copies {}, learner standing {:?}, server {:?}", copies.load(Ordering::SeqCst), learner.standing(), server.group_standing());
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        followed?;
        assert_eq!(read("gap49")?.as_deref(), Some("y"), "with the writes made while it was away");
        Ok(())
    }

    /// The restarted server's first probe of a learner is lost on the way and nobody says so (it
    /// went into a connection that was going): the leader sends it again a beat later, and the
    /// learner follows (production, 2026-10-25: without a timing the leader never did, and
    /// adacachy's learner waited for hours, until the next restart).
    #[test]
    fn a_learner_follows_a_restarted_server_whose_first_probe_was_lost() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (db, logs) = (dir.path().join("kiln.db"), dir.path().join("cluster"));
        let server = Store::open_grouped(&db, &logs)?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        {
            let g = server.group().context("grouped")?;
            g.learners([member].into());
            until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        }
        let current = Arc::new(Mutex::new(Some(server.clone())));
        let fetch: Fetch = {
            let current = current.clone();
            Arc::new(move |dest: &Path| {
                let (s, d) = (current.lock().clone().context("the server is stopped")?, dest.to_path_buf());
                crate::rt::block_on(async move { s.copy_for_learner(d).await })
            })
        };
        let replica = dir.path().join("runner").join("kiln.db");
        let read = |k: &str| -> anyhow::Result<Option<String>> {
            let c = Connection::open_with_flags(&replica, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            Ok(c.query_row("SELECT v FROM kv WHERE k = ?1", [k], |r| r.get(0)).ok())
        };
        let learner = Arc::new(Group::learner(&replica, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up, done.clone());
        set(&server, "w", "x")?;
        until("a write on the learner", || Ok(read("w")?.as_deref() == Some("x")))?;
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        *current.lock() = None;
        drop(server);
        // The restarted server's messages are dropped, unsaid, until its first probe has gone.
        let server = Store::open_grouped(&db, &logs)?;
        *current.lock() = Some(server.clone());
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(false)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up.clone(), done.clone());
        set(&server, "lost", "y")?;
        thread::sleep(Duration::from_millis(500));
        up.store(true, Ordering::SeqCst);
        set(&server, "after", "2")?;
        let followed = until("a write after the lost probe on the learner", || Ok(read("after")?.as_deref() == Some("2")));
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        followed
    }

    /// A learner whose group stops (here a gap it can't fetch a copy for) says so and why, and
    /// applies nothing more: its runner starts it again rather than keep it (adacachy's, on
    /// 2026-10-25, sat for hours applying nothing).
    #[test]
    fn a_learner_whose_group_stopped_says_why() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("kiln.db");
        let server = Store::open_grouped(&db, &dir.path().join("cluster"))?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        // The first copy comes; every one after fails.
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (server, copies) = (server.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                if copies.fetch_add(1, Ordering::SeqCst) > 0 {
                    anyhow::bail!("the server is away");
                }
                crate::rt::block_on({
                    let (s, d) = (server.clone(), dest.to_path_buf());
                    async move { s.copy_for_learner(d).await }
                })
            })
        };
        let replica = dir.path().join("runner").join("kiln.db");
        let learner = Arc::new(Group::learner(&replica, &dir.path().join("runner").join("cluster"), member, fetch)?);
        let (up, done) = (Arc::new(std::sync::atomic::AtomicBool::new(true)), Arc::new(std::sync::atomic::AtomicBool::new(false)));
        let links = link(server.clone(), learner.clone(), member, up, done.clone());
        assert_eq!(learner.stopped(), None, "it follows");
        // A write kiln.db holds and the log lacks: the learner meets a gap it can't copy over.
        let lost = g.next_seq();
        {
            let c = Connection::open(&db)?;
            c.execute("INSERT INTO kv (k, v) VALUES ('lost', 'l')", [])?;
            c.execute(&format!("UPDATE {STATE} SET seq = ?1 WHERE one = 1"), [i64::try_from(lost)?])?;
        }
        g.seq.store(lost, Ordering::SeqCst);
        set(&server, "next", "3")?;
        let stopped = until("the learner's group stopped", || Ok(learner.stopped().is_some()));
        done.store(true, Ordering::SeqCst);
        links.join().ok();
        stopped?;
        assert!(learner.stopped().is_some_and(|why| !why.is_empty()), "it says why: {:?}", learner.stopped());
        assert!(copies.load(Ordering::SeqCst) > u64::from(FETCH_TRIES), "it tried for a copy first");
        Ok(())
    }

    /// A machine's older runner process lets its replica go as the newest takes it, told both at
    /// once: the newest's learner leaves the replica alone until the older has closed it, and then
    /// copies it again (it was kept by a build before the mark).
    #[test]
    fn a_learner_waits_for_another_process_to_let_its_replica_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let server = Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
        set(&server, "before", "1")?;
        let member = member_of("runner-1");
        let g = server.group().context("grouped")?;
        g.learners([member].into());
        until("the learner in the configuration", || Ok(g.point().1.learners.contains(&member)))?;
        let copies = Arc::new(AtomicU64::new(0));
        let fetch: Fetch = {
            let (server, copies) = (server.clone(), copies.clone());
            Arc::new(move |dest: &Path| {
                copies.fetch_add(1, Ordering::SeqCst);
                crate::rt::block_on({
                    let (s, d) = (server.clone(), dest.to_path_buf());
                    async move { s.copy_for_learner(d).await }
                })
            })
        };
        let (replica, logs) = (dir.path().join("runner").join("kiln.db"), dir.path().join("runner").join("cluster"));
        drop(Group::learner(&replica, &logs, member, fetch.clone())?);
        assert_eq!(copies.load(Ordering::SeqCst), 1, "it started from a copy");
        // The older process, from a build before the mark, with the replica open.
        let older = Connection::open(&replica)?;
        older.execute_batch(&format!("DROP TABLE {MARK}"))?;
        let starting = {
            let (replica, logs, fetch) = (replica.clone(), logs.clone(), fetch.clone());
            thread::spawn(move || Group::learner(&replica, &logs, member, fetch).map(drop))
        };
        thread::sleep(Duration::from_secs(2));
        assert!(!starting.is_finished(), "it waits while another process has the replica open");
        assert_eq!(copies.load(Ordering::SeqCst), 1, "and takes no copy meanwhile");
        drop(older);
        starting.join().map_err(|_| anyhow::anyhow!("the learner's start panicked"))??;
        assert_eq!(copies.load(Ordering::SeqCst), 2, "let go, the replica is copied again");
        Ok(())
    }

    /// How long a write takes through the group and without it, on a disk (`KILN_BENCH_DIR`, else
    /// the temporary folder): `cargo test -p kiln --lib how_long_a_write_takes -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measure, not a check: run by hand"]
    fn how_long_a_write_takes() -> anyhow::Result<()> {
        let base = std::env::var("KILN_BENCH_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
        for grouped in [false, true] {
            let dir = base.join(format!("kiln-write-bench-{}", crate::util::new_id()));
            let db = dir.join("kiln.db");
            let store = if grouped { Store::open_grouped(&db, &dir.join("cluster"))? } else { Store::open(&db)? };
            let mut took = Vec::new();
            for i in 0..400 {
                let s = store.clone();
                let t = Instant::now();
                crate::rt::block_on(async move { s.kv_set(format!("k{i}"), "v".repeat(200)).await })?;
                took.push(t.elapsed());
            }
            let flushed = Instant::now();
            store.flush_group()?;
            took.sort();
            let at = |q: usize| took.get(took.len() * q / 100).copied().unwrap_or_default();
            eprintln!(
                "{}: p50 {:?} p95 {:?} p99 {:?} max {:?}; the log caught up {:?} after the last",
                if grouped { "through the group" } else { "directly" },
                at(50),
                at(95),
                at(99),
                took.last().copied().unwrap_or_default(),
                flushed.elapsed()
            );
            drop(store);
            std::fs::remove_dir_all(&dir).ok();
        }
        Ok(())
    }
}
