//! The durable store: [`crate::state::Machine`] replicated by hyper-raft, through hyper-durable's
//! shell, on hyper-log.
//!
//! One thread owns the log file and the replica. Callers propose [`Command`]s from any thread
//! (the server's async tasks among them) and get a future that resolves once the command is
//! committed and applied, with whether it applied. Readers read [`Data`] under a lock; followers
//! of the store take [`Change`]s from a channel.
//!
//! A store starts as a group of one voter, which commits alone: that is kiln on one machine.
//! Replicas on other kiln servers join the same group by a configuration change, and their
//! messages travel over the servers' QUIC connections (the next step; [`Store::step`] and
//! [`Store::take_messages`] are where they come in and go out).

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::task::Wake;
use std::task::Waker;
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use futures::channel::oneshot;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_durable::{Compaction, GroupStore, Output, Replica, Settings, Unbounded};
use hyper_log::{Config as LogConfig, Log, Waits};
use hyper_raft::proto::{ConfState, Message};
use hyper_raft::{Config, Elections, Limits, Stated};
use parking_lot::RwLock;

use crate::state::{Change, Command, Data, Machine};

/// The group deployments keep their state in.
pub const GROUP: u128 = 1;
/// The log's id: "kiln-cd" in ASCII.
const LOG_ID: u128 = 0x006b_696c_6e2d_6364;
/// The largest proposal or message (an app with large inline values fits).
const MESSAGE_BYTES: usize = 4 << 20;

fn log_config() -> LogConfig {
    LogConfig {
        segment_bytes: 1 << 20,
        max_segments: 256,
        max_groups: 8,
        group_entries: 1 << 18,
        group_bytes: 1 << 27,
        group_cache: 1 << 23,
        queue_submissions: 32,
        waits: Waits::Measured,
    }
}

type Member = Replica<GroupStore<DeviceFile>, Machine, Unbounded>;

enum Request {
    Propose(Command, oneshot::Sender<Result<(), String>>),
    Step(Box<Message>),
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

/// The store.
pub struct Store {
    data: Arc<RwLock<Data>>,
    tx: mpsc::Sender<Request>,
    thread: Thread,
    join: Option<JoinHandle<()>>,
    changes: parking_lot::Mutex<Option<mpsc::Receiver<Change>>>,
    outbox: Arc<parking_lot::Mutex<Vec<Message>>>,
    leader: Arc<std::sync::atomic::AtomicBool>,
}

impl Store {
    /// Opens the store in `dir`, creating it on first use, as member `id` of a group whose
    /// voters are `voters` (just `[id]` for a single server).
    pub fn open(dir: &Path, id: u64, voters: &[u64]) -> Result<Store> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("cd.log");
        let fresh = !path.exists();
        let align = Alignment::new(4096).map_err(|e| anyhow::anyhow!("block alignment: {e:?}"))?;
        let file = DeviceFile::open(&path, fresh, CachingRequest::PreferDirect, align)
            .map_err(|e| anyhow::anyhow!("opening {}: {e:?}", path.display()))?;
        let log = if fresh {
            Log::create(file, log_config(), LOG_ID).map_err(|e| anyhow::anyhow!("creating the log: {e:?}"))?
        } else {
            Log::open(file, log_config(), LOG_ID).map(|(log, _)| log).map_err(|e| anyhow::anyhow!("opening the log: {e:?}"))?
        };
        let store = GroupStore::claim(&log, GROUP).map_err(|e| anyhow::anyhow!("claiming the log's group: {e:?}"))?;
        let configuration = ConfState { voters: voters.to_vec(), ..ConfState::default() };
        let machine = Machine::open(&dir.join("cd.state"), configuration)?;
        let data = machine.data();
        let limits = Limits::derive(Stated {
            message: MESSAGE_BYTES + hyper_raft::wire::MESSAGE_RECORD_FIXED_BYTES,
            members: voters.len().max(1),
            memory: usize::try_from(log_config().group_bytes).unwrap_or(usize::MAX),
            depth: 1,
        })
        .map_err(|e| anyhow::anyhow!("raft limits: {e:?}"))?;
        let settings = Settings {
            core: Config {
                max_size_per_msg: MESSAGE_BYTES as u64,
                check_quorum: true,
                pre_vote: true,
                seed: id,
                ..Config::new(id, limits)
            },
            // Driven by events: a commit nobody wrote yet is written at the first lull.
            quiet: Duration::ZERO,
            elections: Elections::Suspicion,
        };
        let (tx, rx) = mpsc::channel();
        let (ctx, crx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let outbox = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let leader = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (out2, lead2) = (outbox.clone(), leader.clone());
        let sole = voters == [id];
        let join = thread::Builder::new()
            .name("kiln-cd-store".into())
            .spawn(move || {
                let waker = Waker::from(Arc::new(Unpark(thread::current())));
                let replica = match Replica::open(&settings, store, machine, Unbounded) {
                    Ok(r) => r,
                    Err(e) => {
                        ready_tx.send(Err(anyhow::anyhow!("opening the replica: {e:?}"))).ok();
                        return;
                    }
                };
                let mut d = Driver { replica, log, waker, rx, changes: ctx, pending: HashMap::new(), next: 1, start: Instant::now(), outbox: out2, leader: lead2 };
                d.run(sole, ready_tx);
            })
            .context("starting the store's thread")?;
        let thread = join.thread().clone();
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => bail!("the store did not start within 30 s"),
        }
        Ok(Store { data, tx, thread, join: Some(join), changes: parking_lot::Mutex::new(Some(crx)), outbox, leader })
    }

    /// The changes the store applies, for one follower (the first caller).
    pub fn changes(&self) -> Option<mpsc::Receiver<Change>> {
        self.changes.lock().take()
    }

    /// Proposes a command; the answer comes once it is committed and applied.
    pub fn submit(&self, cmd: Command) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Request::Propose(cmd, tx)).is_ok() {
            self.thread.unpark();
        }
        rx
    }

    /// Proposes a command and waits for its answer (for callers that may block).
    pub fn propose(&self, cmd: Command) -> Result<(), String> {
        match futures::executor::block_on(self.submit(cmd)) {
            Ok(r) => r,
            Err(_) => Err("the store stopped".into()),
        }
    }

    /// Reads the applied state.
    pub fn read<T>(&self, f: impl FnOnce(&Data) -> T) -> T {
        f(&self.data.read())
    }

    /// Whether this member leads its group (and so may propose).
    pub fn is_leader(&self) -> bool {
        self.leader.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A message from another member, for this one.
    pub fn step(&self, message: Message) {
        if self.tx.send(Request::Step(Box::new(message))).is_ok() {
            self.thread.unpark();
        }
    }

    /// Messages for other members, to send.
    pub fn take_messages(&self) -> Vec<Message> {
        std::mem::take(&mut *self.outbox.lock())
    }
}

impl Drop for Store {
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
    #[allow(dead_code)]
    log: Log<DeviceFile>,
    waker: Waker,
    rx: mpsc::Receiver<Request>,
    changes: mpsc::Sender<Change>,
    pending: HashMap<u64, oneshot::Sender<Result<(), String>>>,
    next: u64,
    start: Instant,
    outbox: Arc<parking_lot::Mutex<Vec<Message>>>,
    leader: Arc<std::sync::atomic::AtomicBool>,
}

impl Driver {
    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn run(&mut self, sole: bool, ready: mpsc::Sender<Result<()>>) {
        // A sole voter elects itself; a member of a larger group waits to hear its leader.
        if sole {
            if let Err(e) = self.replica.campaign() {
                ready.send(Err(anyhow::anyhow!("campaigning: {e:?}"))).ok();
                return;
            }
            // One voter may lead the moment it campaigns; its term and vote still need writing.
            let until = Instant::now() + Duration::from_secs(10);
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
                ready.send(Err(anyhow::anyhow!("the store's sole voter did not elect itself"))).ok();
                return;
            }
        }
        self.leader.store(self.replica.is_leader(), std::sync::atomic::Ordering::Relaxed);
        ready.send(Ok(())).ok();
        let mut stopping = false;
        loop {
            while let Ok(req) = self.rx.try_recv() {
                match req {
                    Request::Propose(cmd, reply) => self.propose(cmd, reply),
                    Request::Step(m) => {
                        self.replica.step(*m).ok();
                    }
                    Request::Stop => stopping = true,
                }
            }
            if let Err(e) = self.drive() {
                tracing::error!("deployments store stopped: {e:#}");
                for (_, reply) in self.pending.drain() {
                    reply.send(Err(format!("the store failed: {e:#}"))).ok();
                }
                return;
            }
            if self.replica.compaction_due(Compaction { expansion: 4 }) {
                let keep = self.replica.applied().index;
                let now = self.now();
                self.replica.compact(keep, now, &self.waker).ok();
            }
            if stopping {
                // Finish what was proposed (a sole voter commits within its next writes).
                if self.pending.is_empty() {
                    self.replica.machine_mut().persist_now();
                    return;
                }
            }
            let wait = self
                .replica
                .deadline()
                .map(|d| Duration::from_nanos(d.saturating_sub(self.now())))
                .unwrap_or(Duration::from_millis(100))
                .min(Duration::from_millis(100));
            thread::park_timeout(wait.max(Duration::from_micros(50)));
        }
    }

    fn propose(&mut self, cmd: Command, reply: oneshot::Sender<Result<(), String>>) {
        if !self.replica.is_leader() {
            reply.send(Err("this server doesn't lead the deployments group".into())).ok();
            return;
        }
        let data = match serde_json::to_vec(&cmd) {
            Ok(d) => d,
            Err(e) => {
                reply.send(Err(format!("unencodable command: {e}"))).ok();
                return;
            }
        };
        if data.len() > MESSAGE_BYTES - 1024 {
            reply.send(Err(format!("the command is {} bytes; the most is {}", data.len(), MESSAGE_BYTES - 1024))).ok();
            return;
        }
        let id = self.next;
        self.next += 1;
        match self.replica.propose(id.to_le_bytes().to_vec(), data) {
            Ok(()) => {
                self.pending.insert(id, reply);
            }
            Err(e) => {
                reply.send(Err(format!("refused: {e:?}"))).ok();
            }
        }
    }

    /// Drives the replica until it rests, answering what applied.
    fn drive(&mut self) -> Result<()> {
        let mut out: Output<crate::state::Answer> = Output::default();
        for _ in 0..1024 {
            out.clear();
            let driven = self.replica.drive(self.now(), &self.waker, &mut out).map_err(|e| anyhow::anyhow!("driving the replica: {e:?}"))?;
            for (id, result) in out.answers.drain(..) {
                if let Some(reply) = self.pending.remove(&id) {
                    reply.send(result).ok();
                }
            }
            if !out.messages.is_empty() {
                self.outbox.lock().append(&mut out.messages);
            }
            for c in self.replica.machine_mut().take_changes() {
                self.changes.send(c).ok();
            }
            if !driven.more {
                break;
            }
        }
        self.leader.store(self.replica.is_leader(), std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ClusterDef, Command};

    #[test]
    fn a_sole_member_commits_and_reopens_with_what_it_committed() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        {
            let s = Store::open(tmp.path(), 1, &[1])?;
            assert!(s.is_leader());
            let changes = s.changes().context("changes")?;
            for i in 0..50 {
                let c = ClusterDef { name: format!("c{i}"), server: format!("https://k{i}:6443"), ..ClusterDef::default() };
                s.propose(Command::PutCluster(Box::new(c))).map_err(|e| anyhow::anyhow!(e))?;
            }
            assert_eq!(s.read(|d| d.clusters.len()), 50);
            assert!(s.propose(Command::DeleteCluster("nope".into())).is_err(), "a command that doesn't apply says why");
            assert_eq!(changes.try_iter().count(), 50);
        }
        // Reopened: the image and the log give back everything committed.
        let s = Store::open(tmp.path(), 1, &[1])?;
        assert_eq!(s.read(|d| d.clusters.get("c49").map(|c| c.server.clone())), Some("https://k49:6443".into()));
        s.propose(Command::DeleteCluster("c0".into())).map_err(|e| anyhow::anyhow!(e))?;
        assert_eq!(s.read(|d| d.clusters.len()), 49);
        Ok(())
    }
}
