//! The persisted class (Focal's doc 33, §4.2): a topic's messages synced to disk by a quorum of its
//! nodes before a publish is acknowledged, on Focal's replicated part (`focal_bus::member`, doc 34;
//! kiln's CP-T2.2).
//!
//! **Each topic a part.** A persisted topic is a part of its own, replicated by a Raft group of
//! every node of the cluster over hyper-durable's `GroupStore` on the node's disk: three nodes, a
//! group of three, each topic's part on all three; one node, a group of one, durable on its disk.
//! The group commits a publish once a majority's logs made it durable (no fates stated: Focal's
//! persisted acknowledgement, the full flush), and every node's member applies what its group
//! committed to its part.
//!
//! **The catalog** (`catalog`) is the cluster's record of its topics, a part of its own replicated
//! by every node the same way: a create, a delete and each node's port for each topic's part are
//! its records, applied by every node in the order its group committed them, so every node holds
//! the same topics and opens the same parts.
//!
//! **Reads are of what's committed.** Each node keeps a view of each topic (`crate::ephemeral`'s
//! log, fed only what the node's own member applied), which reads, waits, bounds and subscriptions
//! are served from, as for the ephemeral class: a topic's retention, lag notices and stamps alike.
//! A publish is answered once the group committed it and the node's member applied it (`group`).
//!
//! **Refusals.** A publish no leader could be found for within the quorum wait is refused
//! [`Refusal::NoQuorum`], never sent; one sent is sent again to whichever node leads until the
//! group commits it, and never appended twice (the part's sessions, doc 34 P6). A node that doesn't
//! lead redirects a publisher to the leader, typed (Focal's `Redirect`), and the publisher follows.
//!
//! **The host** (`host`) is a thread of the node's own, a hyper-rt shard driving every member and
//! publisher; the node's API reaches it through a bounded queue of commands.

mod catalog;
mod group;
mod host;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::task::Waker;
use std::thread::JoinHandle;
use std::time::Duration;

use focal_memory::MemoryBudget;
use futures::channel::oneshot;
use quiver_api::{Offset, Refusal, Topic};

use crate::ephemeral::Log;
use crate::node::Clock;

/// What a node of a persisted cluster is started with. Every field but `id` and `data` is the same
/// at every node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedConfig {
    /// This node, among `nodes`.
    pub id: u64,
    /// Every node of the cluster, numbered 1, 2, 3… in order, and the address its catalog's
    /// member listens on; each topic's member listens on the same host at a port of its own.
    pub nodes: Vec<(u64, SocketAddr)>,
    /// The directory the node keeps its logs, its parts' images and its run in.
    pub data: PathBuf,
    /// The cluster's identities (`focal_bus::pki`): a node's and a publisher's for each node, made
    /// there the first time, the same directory at every node.
    pub identities: PathBuf,
    /// Each part's memory, the catalog's and each topic's: what its window keeps of its batches
    /// and what its log holds (about twice it, `hyper_log::Config::derive`).
    pub part_memory: u64,
    /// Persisted topics the cluster holds at most.
    pub topics: usize,
    /// Publishes the node holds at once, waiting to be sent or committed.
    pub publishes: usize,
    /// How long a publish waits for a leader to send it to before it's refused for no quorum.
    pub quorum_wait: Duration,
}

impl PersistedConfig {
    /// The least memory a part takes: Focal's group test's (focal-bus `tests/group.rs`, its log
    /// let go behind its window at 24 MiB).
    pub const PART_MEMORY_MIN: u64 = 24 << 20;

    fn address(&self, id: u64) -> Option<SocketAddr> {
        self.nodes.iter().find(|(n, _)| *n == id).map(|(_, a)| *a)
    }

    fn peers(&self) -> Vec<(u64, SocketAddr)> {
        self.nodes
            .iter()
            .filter(|(n, _)| *n != self.id)
            .copied()
            .collect()
    }

    fn ids(&self) -> Vec<u64> {
        self.nodes.iter().map(|(n, _)| *n).collect()
    }

    fn check(&self) -> Result<(), Refusal> {
        let invalid = |why: String| Refusal::Invalid {
            what: "a persisted node's configuration",
            why,
        };
        let numbered = self
            .nodes
            .iter()
            .zip(1_u64..)
            .all(|((id, _), expected)| *id == expected);
        if self.nodes.is_empty() || !numbered {
            return Err(invalid(
                "its nodes are numbered 1, 2, 3… in order".to_owned(),
            ));
        }
        if self.address(self.id).is_none() {
            return Err(invalid(format!("node {} isn't among its nodes", self.id)));
        }
        if self.part_memory < Self::PART_MEMORY_MIN {
            return Err(invalid(format!(
                "a part's memory of {} bytes is under the least ({})",
                self.part_memory,
                Self::PART_MEMORY_MIN
            )));
        }
        if self.topics == 0 || self.publishes == 0 || self.quorum_wait.is_zero() {
            return Err(invalid(
                "it holds a topic and a publish at least, and waits for a quorum".to_owned(),
            ));
        }
        Ok(())
    }
}

/// A command for the host.
pub(crate) enum Command {
    Create {
        name: String,
        topic: Topic,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    Delete {
        name: String,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    Publish {
        name: String,
        /// The message framed as Quiver's record (`ephemeral::record_into`).
        record: Vec<u8>,
        at_us: u64,
        reply: oneshot::Sender<Result<Offset, Refusal>>,
    },
    /// A mark through the catalog: answered once this node applied it.
    Mark {
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
}

/// What one group of a node did, as its host last looked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupFigures {
    /// The topic's incarnation: the offset of the catalog's record that created it (0 for the
    /// catalog).
    pub incarnation: u64,
    /// Whether this node's member is open (every node's port known).
    pub open: bool,
    /// Whether this node's member leads the group, and the term it knows it in.
    pub leads: bool,
    pub term: u64,
    /// The leader the member knows.
    pub leader: Option<u64>,
    /// Frames this node's log for the group wrote and flushed: each a persistence point.
    pub frames: u64,
    /// The offset after the last its part applied.
    pub applied: u64,
    /// What this node's publisher to the group did.
    pub sent: u64,
    pub resent: u64,
    pub redirects: u64,
    /// The node the publisher sends to, the batches it owes (sent, not yet seen committed) and
    /// those it holds unsent.
    pub target: Option<u64>,
    pub owed: usize,
    pub queued: usize,
    /// Its connections that came up and went.
    pub connects: u64,
    pub losses: u64,
    pub dials_refused: u64,
}

impl GroupFigures {
    fn of(seat: &group::Seat, publisher: &group::Publisher, incarnation: u64) -> Self {
        let leader = seat.member.leader();
        let stats = seat.member.stats();
        Self {
            incarnation,
            open: true,
            leads: seat.member.is_leader(),
            term: leader.term,
            leader: leader.node,
            frames: stats.log.frames,
            applied: seat.part().bounds(group::STREAM).map_or(0, |(_, end)| end),
            sent: publisher.stats.sent,
            resent: publisher.stats.resent,
            redirects: publisher.stats.redirects,
            target: publisher.state().0,
            owed: publisher.state().1,
            queued: publisher.state().2,
            connects: publisher.stats.connects,
            losses: publisher.stats.losses,
            dials_refused: publisher.stats.dials_refused,
        }
    }
}

/// A persisted node's figures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedStats {
    /// The node's run: raised at every start.
    pub run: u64,
    pub catalog: GroupFigures,
    /// Each topic the node holds, by name.
    pub topics: Vec<(String, GroupFigures)>,
    /// Whether the node has applied every catalog record committed before it started: until it
    /// has, a topic it doesn't hold is refused for no quorum, never as no topic.
    pub settled: bool,
    /// Why the host ended, if it did.
    pub ended: Option<String>,
}

impl PersistedStats {
    /// Frames the node's logs wrote, every group's.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.topics.iter().fold(self.catalog.frames, |sum, (_, g)| {
            sum.saturating_add(g.frames)
        })
    }

    /// The topic `name`'s figures.
    #[must_use]
    pub fn topic(&self, name: &str) -> Option<&GroupFigures> {
        self.topics.iter().find(|(n, _)| n == name).map(|(_, g)| g)
    }
}

/// The host's queue of commands, its waker and the word to stop.
struct Inbox {
    commands: VecDeque<Command>,
    waker: Option<Waker>,
    stop: bool,
}

/// What the node's API and its host share.
pub(crate) struct Shared {
    inbox: Mutex<Inbox>,
    /// The commands the queue holds at most.
    bound: usize,
    views: RwLock<HashMap<String, Arc<Log>>>,
    stats: Mutex<PersistedStats>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn stopping(&self) -> bool {
        lock(&self.inbox).stop
    }

    fn take_commands(&self) -> Vec<Command> {
        lock(&self.inbox).commands.drain(..).collect()
    }

    fn set_waker(&self, waker: Waker) {
        lock(&self.inbox).waker = Some(waker);
    }

    fn put_view(&self, name: &str, view: Arc<Log>) -> Option<Arc<Log>> {
        self.views
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.to_owned(), view)
    }

    /// Takes `view` out of the views, if it's the one `name` names.
    fn drop_view(&self, name: &str, view: &Arc<Log>) {
        let mut views = self.views.write().unwrap_or_else(PoisonError::into_inner);
        if views.get(name).is_some_and(|held| Arc::ptr_eq(held, view)) {
            views.remove(name);
        }
    }

    fn settle(&self) {
        lock(&self.stats).settled = true;
    }

    fn set_figures(&self, run: u64, catalog: GroupFigures, topics: Vec<(String, GroupFigures)>) {
        let mut stats = lock(&self.stats);
        stats.run = run;
        stats.catalog = catalog;
        stats.topics = topics;
    }

    /// Queues `command` and wakes the host; refused once the queue is full or the host ended.
    fn send(&self, command: Command) -> Result<(), Refusal> {
        let waker = {
            let mut inbox = lock(&self.inbox);
            if inbox.stop {
                let why = lock(&self.stats).ended.clone();
                return Err(host::ended(why.as_deref().unwrap_or("it was stopped")));
            }
            if inbox.commands.len() >= self.bound {
                return Err(Refusal::Full {
                    what: "the node's persisted commands waiting",
                    limit: u64::try_from(self.bound).unwrap_or(u64::MAX),
                });
            }
            inbox.commands.push_back(command);
            inbox.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Tells the host to stop: it ends at its next wake, holding nothing it answers.
    fn stop(&self) {
        let waker = {
            let mut inbox = lock(&self.inbox);
            inbox.stop = true;
            inbox.commands.clear();
            inbox.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The hyper-rt shard the host runs on: one task, its endpoints' and its logs' wakes. Focal's own
/// group test's shard (focal-bus `tests/group.rs`), which drives three nodes and a client.
fn runtime() -> hyper_rt::runtime::RuntimeConfig {
    hyper_rt::runtime::RuntimeConfig {
        shards: 1,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 1_000_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// A node's persisted class: its host, and what its API reads. Dropping it stops the host.
pub(crate) struct Persisted {
    shared: Arc<Shared>,
    host: Mutex<Option<JoinHandle<()>>>,
}

impl Persisted {
    /// Starts the host for `config`, its views charged to `budget` and stamped by `clock`;
    /// answers once the node's catalog is open, or why it couldn't be.
    pub(crate) fn start(
        config: PersistedConfig,
        budget: MemoryBudget,
        clock: Clock,
    ) -> Result<Self, Refusal> {
        config.check()?;
        let shared = Arc::new(Shared {
            inbox: Mutex::new(Inbox {
                commands: VecDeque::new(),
                waker: None,
                stop: false,
            }),
            bound: config.publishes.saturating_add(config.topics),
            views: RwLock::new(HashMap::new()),
            stats: Mutex::new(PersistedStats::default()),
        });
        let (opened, open) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let for_host = Arc::clone(&shared);
        let host = std::thread::Builder::new()
            .name(format!("quiver-persisted-{}", config.id))
            .spawn(move || {
                let ended = run_host(config, &for_host, &opened, clock, budget);
                lock(&for_host.stats).ended = Some(ended.unwrap_or_else(|why| why));
                for_host.stop();
            })
            .map_err(|e| Refusal::Unavailable(format!("the persisted host's thread: {e}")))?;
        match open.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                host: Mutex::new(Some(host)),
            }),
            Ok(Err(why)) => {
                let _ = host.join();
                Err(Refusal::Unavailable(format!(
                    "the persisted node didn't open: {why}"
                )))
            }
            Err(_) => {
                let _ = host.join();
                Err(Refusal::Unavailable(
                    "the persisted node's host ended as it opened".to_owned(),
                ))
            }
        }
    }

    /// Stops the host at once, as a crash would: nothing is flushed or handed over that wasn't
    /// already, and every waiting command is refused. The node's data is as a kill left it.
    pub(crate) fn halt(&self) {
        self.shared.stop();
        if let Some(host) = lock(&self.host).take() {
            let _ = host.join();
        }
    }

    pub(crate) fn stats(&self) -> PersistedStats {
        lock(&self.shared.stats).clone()
    }

    /// The view of the persisted topic `name`; refused [`Refusal::NoTopic`] where the node holds
    /// none, or for no quorum while the node hasn't caught up with its catalog.
    pub(crate) fn view(&self, name: &str) -> Option<Arc<Log>> {
        self.shared
            .views
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// The view of the persisted topic `name`. A node may not yet have applied a create another
    /// node answered, so one it doesn't hold it looks for again once it has applied a mark of its
    /// own through the catalog, everything committed before it: refused [`Refusal::NoTopic`]
    /// only then, and for no quorum where the catalog has none.
    pub(crate) async fn lookup(&self, name: &str) -> Result<Arc<Log>, Refusal> {
        if let Some(view) = self.view(name) {
            return Ok(view);
        }
        self.ask(|reply| Command::Mark { reply }).await?;
        self.view(name)
            .ok_or_else(|| Refusal::NoTopic(name.to_owned()))
    }

    async fn ask<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, Refusal>>) -> Command,
    ) -> Result<T, Refusal> {
        let (reply, answer) = oneshot::channel();
        self.shared.send(command(reply))?;
        answer.await.unwrap_or_else(|_| Err(self.end_refusal()))
    }

    fn end_refusal(&self) -> Refusal {
        let why = lock(&self.shared.stats).ended.clone();
        host::ended(why.as_deref().unwrap_or("it was stopped"))
    }

    pub(crate) async fn create(&self, name: &str, topic: Topic) -> Result<(), Refusal> {
        let name = name.to_owned();
        self.ask(|reply| Command::Create { name, topic, reply })
            .await
    }

    pub(crate) async fn delete(&self, name: &str) -> Result<(), Refusal> {
        let name = name.to_owned();
        self.ask(|reply| Command::Delete { name, reply }).await
    }

    pub(crate) async fn publish(
        &self,
        name: &str,
        record: Vec<u8>,
        at_us: u64,
    ) -> Result<Offset, Refusal> {
        let name = name.to_owned();
        self.ask(|reply| Command::Publish {
            name,
            record,
            at_us,
            reply,
        })
        .await
    }
}

impl Drop for Persisted {
    fn drop(&mut self) {
        self.halt();
    }
}

/// The host's thread: its shard, the host opened on it (said on `opened`), run until it's stopped
/// or faults; why it ended.
fn run_host(
    config: PersistedConfig,
    shared: &Arc<Shared>,
    opened: &std::sync::mpsc::SyncSender<Result<(), String>>,
    clock: Clock,
    budget: MemoryBudget,
) -> Result<String, String> {
    let mut shard = match hyper_rt::runtime::LocalRuntime::new(&runtime()) {
        Ok(shard) => shard,
        Err(e) => {
            let why = format!("its shard: {e:?}");
            let _ = opened.send(Err(why.clone()));
            return Err(why);
        }
    };
    let shared = Arc::clone(shared);
    let opened = opened.clone();
    shard
        .block_on(async move {
            let waker =
                std::future::poll_fn(|context| std::task::Poll::Ready(context.waker().clone()))
                    .await;
            shared.set_waker(waker.clone());
            let mut host = match host::Host::open(config, Arc::clone(&shared), waker, clock, budget)
            {
                Ok(host) => host,
                Err(why) => {
                    let _ = opened.send(Err(why.clone()));
                    return Err(why);
                }
            };
            let _ = opened.send(Ok(()));
            host.run().await.map(|()| "it was stopped".to_owned())
        })
        .map_err(|e| format!("its shard: {e:?}"))?
}
