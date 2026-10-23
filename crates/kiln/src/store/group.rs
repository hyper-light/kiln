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

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::task::{Wake, Waker};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{Fatal, GroupStore, Output, Point, Replica, Settings, StateMachine, Unbounded};
use hyper_log::{Config as LogConfig, Facts, Log};
use hyper_raft::proto::{ConfChangeV2, ConfState};
use hyper_raft::{Config, Elections, Limits, Stated};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::capture::{self, Change, Row, TableInfo};

/// The table that says where kiln.db stands: the last write it holds (`seq`) and the last entry
/// made durable with it (`applied`, `term`). One row.
pub const STATE: &str = "cluster_state";
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
/// Entries a compaction leaves behind what's applied.
const KEEP: u64 = 256;
/// Changes waiting for the log, at most: past them a write waits its turn.
const QUEUED: usize = 1024;
/// Entries, and log bytes, between compactions.
const COMPACT_ENTRIES: u64 = 8192;
const COMPACT_BYTES: u64 = 128 << 20;

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
}

impl Group {
    /// Opens the group for the database at `db`, its log in `dir`, and replays the log's writes
    /// kiln.db doesn't hold.
    pub fn open(db: &Path, dir: &Path) -> anyhow::Result<Group> {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        let path = dir.join("cluster.log");
        let fresh = std::fs::metadata(&path).map_or(true, |m| m.len() == 0);
        let align = Alignment::new(BLOCK).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let file = DeviceFile::open(&path, true, CachingRequest::PreferDirect, align).map_err(|e| anyhow::anyhow!("{}: {e:?}", path.display()))?;
        let config = log_config(file.layout_block())?;
        let log = if fresh {
            Log::create(file, config, LOG_ID).map_err(|e| anyhow::anyhow!("making the cluster log at {}: {e:?}", path.display()))?
        } else {
            Log::open(file, config, LOG_ID).map(|(log, _)| log).map_err(|e| anyhow::anyhow!("opening the cluster log at {}: {e:?}", path.display()))?
        };
        let store = GroupStore::claim(&log, GROUP).map_err(|e| anyhow::anyhow!("claiming the cluster log's group: {e:?}"))?;
        let machine = Machine::open(db)?;
        let seq = machine.seq.clone();
        let limits = Limits::derive(Stated { message: LARGEST + (64 << 10) + hyper_raft::wire::MESSAGE_RECORD_FIXED_BYTES, members: 64, memory: 64 << 20, depth: 1 })
            .map_err(|e| anyhow::anyhow!("the cluster group's limits: {e:?}"))?;
        let settings = Settings {
            core: Config { max_size_per_msg: (LARGEST + (64 << 10)) as u64, check_quorum: true, pre_vote: true, seed: SERVER, ..Config::new(SERVER, limits) },
            // Driven by events: what's proposed is written at the first lull, together.
            quiet: Duration::ZERO,
            elections: Elections::Suspicion,
        };
        let (tx, rx) = mpsc::sync_channel(QUEUED);
        let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<()>>();
        let applied = Arc::new(AtomicU64::new(0));
        let applied2 = applied.clone();
        let join = thread::Builder::new()
            .name("kiln-cluster".into())
            .spawn(move || {
                let waker = Waker::from(Arc::new(Unpark(thread::current())));
                let replica = match Replica::open(&settings, store, machine, Unbounded) {
                    Ok(r) => r,
                    Err(e) => {
                        ready_tx.send(Err(anyhow::anyhow!("opening the cluster group: {e:?}"))).ok();
                        return;
                    }
                };
                let mut d = Driver { replica, _log: log, waker, rx, pending: std::collections::HashMap::new(), next: 1, start: Instant::now(), since: 0, broken: None, applied: applied2 };
                d.run(ready_tx);
            })
            .context("starting the cluster group's thread")?;
        let thread = join.thread().clone();
        match ready_rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => bail!("the cluster group didn't start within 60 s"),
        }
        Ok(Group { tx, thread, join: Some(join), seq, applied })
    }

    /// Where it stands: kiln.db's last write, and the last entry applied (`/healthz/server`).
    pub fn standing(&self) -> (u64, u64) {
        (self.seq.load(Ordering::SeqCst), self.applied.load(Ordering::SeqCst))
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
}

impl Driver {
    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn run(&mut self, ready: mpsc::Sender<anyhow::Result<()>>) {
        // The sole voter elects itself.
        if let Err(e) = self.replica.campaign() {
            ready.send(Err(anyhow::anyhow!("campaigning: {e:?}"))).ok();
            return;
        }
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            if let Err(e) = self.drive() {
                ready.send(Err(e)).ok();
                return;
            }
            if self.replica.is_leader() || Instant::now() >= until {
                break;
            }
            thread::park_timeout(Duration::from_millis(1));
        }
        if !self.replica.is_leader() {
            ready.send(Err(anyhow::anyhow!("the cluster group's sole voter didn't elect itself"))).ok();
            return;
        }
        // A barrier: once an entry of its own applies, so has everything before it (the log's
        // writes kiln.db didn't hold), and the store serves what's committed.
        let (reply, answer) = mpsc::sync_channel(1);
        self.barrier(reply);
        if let Err(e) = self.settle() {
            ready.send(Err(e)).ok();
            return;
        }
        if !matches!(answer.try_recv(), Ok(Ok(()))) {
            ready.send(Err(anyhow::anyhow!("the cluster group's first entry didn't apply"))).ok();
            return;
        }
        self.replica.machine_mut().opened();
        self.since = self.replica.applied().index;
        ready.send(Ok(())).ok();
        let mut stopping = false;
        loop {
            while let Ok(req) = self.rx.try_recv() {
                match req {
                    Request::Append(seq, change) => self.append(seq, change),
                    Request::Flush(reply) => self.barrier(reply),
                    Request::Stop if !stopping => {
                        // What was sent before goes into the log first.
                        let (reply, _answer) = mpsc::sync_channel(1);
                        self.barrier(reply);
                        stopping = true;
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
                // What the store sends from now on can't be logged: take it and say so.
                while let Ok(req) = self.rx.recv() {
                    match req {
                        Request::Flush(reply) => {
                            reply.send(Err(format!("the cluster group failed: {e:#}"))).ok();
                        }
                        Request::Append(..) => {}
                        Request::Stop => return,
                    }
                }
                return;
            }
            // Compacting makes kiln.db durable (a commit that waits for the disk, holding its write
            // lock), so it's every so many writes, not by the image's size (on one node a marker).
            let applied = self.replica.applied().index;
            if self.replica.is_stalled() || applied.saturating_sub(self.since) >= COMPACT_ENTRIES || self.replica.compactable_bytes() >= COMPACT_BYTES {
                let now = self.now();
                if self.replica.compact(KEEP.min(applied), now, &self.waker).unwrap_or(false) {
                    self.since = applied;
                }
            }
            if stopping && self.pending.is_empty() {
                self.replica.machine_mut().persist().ok();
                return;
            }
            let wait = self.replica.deadline().map(|d| Duration::from_nanos(d.saturating_sub(self.now()))).unwrap_or(Duration::from_millis(100)).min(Duration::from_millis(100));
            thread::park_timeout(wait.max(Duration::from_micros(50)));
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
        for data in parts {
            if let Err(e) = self.replica.propose(Vec::new(), data) {
                tracing::error!(seq, "a write kiln.db holds isn't in the cluster log: {e:?}");
            }
        }
    }

    /// Proposes an entry of no change, answered once applied: everything before it is too.
    fn barrier(&mut self, reply: mpsc::SyncSender<Result<(), String>>) {
        let id = self.next;
        self.next += 1;
        match self.replica.propose(id.to_le_bytes().to_vec(), Vec::new()) {
            Ok(()) => {
                self.pending.insert(id, reply);
            }
            Err(e) => {
                reply.send(Err(format!("the cluster group refused an entry: {e:?}"))).ok();
            }
        }
    }

    /// Drives until the barriers proposed are applied.
    fn settle(&mut self) -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(120);
        while !self.pending.is_empty() {
            self.drive()?;
            if Instant::now() >= until {
                bail!("the cluster group didn't apply its log within 120 s");
            }
            if !self.pending.is_empty() {
                thread::park_timeout(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// Drives the replica until it rests, answering what applied.
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
            if !driven.more {
                break;
            }
        }
        self.applied.store(self.replica.applied().index, Ordering::SeqCst);
        Ok(())
    }
}

/// What applying a barrier answers: its request id and how it went.
type Answer = (u64, Result<(), String>);

/// kiln.db as the group's state machine: entries applied on a connection of its own.
struct Machine {
    conn: Connection,
    known: std::collections::HashMap<String, TableInfo>,
    applied: Point,
    durable: Point,
    configuration: ConfState,
    /// The last write kiln.db holds (shared with the store, which commits writes).
    seq: Arc<AtomicU64>,
    /// The last write the log held when the group opened (to say what kiln.db holds past it).
    logged: u64,
}

impl Machine {
    /// Opens kiln.db at `db` to apply entries: foreign keys off (an entry's rows hold its cascades
    /// already), and where it stands read from [`STATE`] (made, at 0, on the first start).
    fn open(db: &Path) -> anyhow::Result<Machine> {
        let conn = Connection::open(db).with_context(|| format!("opening {} for the cluster group", db.display()))?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = OFF; PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS {STATE} (one INTEGER PRIMARY KEY CHECK (one = 1), applied INTEGER NOT NULL, term INTEGER NOT NULL, seq INTEGER NOT NULL);
             INSERT OR IGNORE INTO {STATE} VALUES (1, 0, 0, 0);"
        ))?;
        let (index, term, seq): (i64, i64, i64) = conn.query_row(&format!("SELECT applied, term, seq FROM {STATE}"), [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let at = Point { index: u64::try_from(index).unwrap_or(0), term: u64::try_from(term).unwrap_or(0) };
        Ok(Machine {
            conn,
            known: std::collections::HashMap::new(),
            applied: at,
            durable: at,
            configuration: ConfState { voters: vec![SERVER], ..ConfState::default() },
            seq: Arc::new(AtomicU64::new(u64::try_from(seq).unwrap_or(0))),
            logged: 0,
        })
    }

    /// The log has been replayed: says what kiln.db holds that it doesn't.
    fn opened(&self) {
        let seq = self.seq.load(Ordering::SeqCst);
        if seq > self.logged && self.applied.index > 1 {
            tracing::warn!(seq, logged = self.logged, "kiln.db holds writes the cluster log doesn't (a stop before they reached it); a learner (P2) takes a copy of the database");
        }
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

    fn install(&mut self, _image: &[u8], _at: Point, _configuration: &ConfState) -> Result<(), Fatal> {
        Err(Fatal("a copy of the database comes to a learner in P2"))
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        if self.durable == self.applied {
            return Ok(());
        }
        // kiln.db commits without waiting for the disk (synchronous NORMAL); one that does makes
        // everything before it durable too, so the log may start past it.
        let at = self.applied;
        let done = self.conn.execute_batch("PRAGMA synchronous = FULL").map_err(anyhow::Error::from).and_then(|()| {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            self.conn.execute(&format!("UPDATE {STATE} SET applied = ?1, term = ?2 WHERE one = 1"), rusqlite::params![i64::try_from(at.index)?, i64::try_from(at.term)?])?;
            self.conn.execute_batch("COMMIT")?;
            Ok(())
        });
        self.conn.execute_batch("PRAGMA synchronous = NORMAL").ok();
        done.map_err(|_| Fatal("kiln.db couldn't be made durable"))?;
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
