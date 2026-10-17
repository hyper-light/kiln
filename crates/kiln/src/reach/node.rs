//! A runner's reach node (`docs/reach.md` §4): its encrypted links to other runners, the runs it
//! follows on them, and the runs it does for them. One task owns all of it; the agent feeds it the
//! server's frames, local tools (`kiln run`, the agents' MCP tools) talk to it through a
//! [`Handle`].
//!
//! Asking side: one session per runner it talks to, carried by one link at a time. A link that
//! drops is replaced (backoff from 200 ms to 5 s) and the session resumed: each run it follows
//! continues from the last byte that arrived.
//!
//! Doing side: a run outlives its link. Its output waits (up to [`BUFFER`], then the command waits
//! too) until the asker's session comes back on a new link, for as long as the run's linger allows.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::channel::oneshot;
use serde::Serialize;
use serde_json::{Value, json};

use super::directory::RunnerInfo;
use super::e2e::{self, Identity, Pins, Tls, Trust};
use super::relay::Announce;
use super::serve;
use super::sync::Mirror;
use super::wire::{self, Ask, Deframer, Host, Resume, RunSpec, Say, Signal, Stream, SyncAsk, SyncSay};
use crate::proto::AgentMsg;

/// Output a run holds for its asker before the command itself is made to wait.
pub const BUFFER: u64 = 16 << 20;
/// Ciphertext per relayed frame.
const FRAME: usize = 256 * 1024;
/// How long a run waits for its asker to come back, unless it says otherwise.
const LINGER: Duration = Duration::from_secs(15 * 60);
/// A finished run nobody collected is dropped after this long without a link.
const UNCOLLECTED: Duration = Duration::from_secs(10 * 60);
/// A link that has not finished its handshake in this long is given up.
const HANDSHAKE: Duration = Duration::from_secs(15);
/// Quiet this long, the asker pings; this long more, the link is dead.
const PING_AFTER: Duration = Duration::from_secs(15);
const DEAD_AFTER: Duration = Duration::from_secs(45);
/// The directory is asked for again when older than this.
const DIRECTORY_TTL: Duration = Duration::from_secs(30);
/// A peer with nothing to follow keeps its link this long.
const IDLE_LINK: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Config {
    /// This runner's name (its certificate's).
    pub name: String,
    /// Where its identity and pins live (`<state>/reach`).
    pub dir: PathBuf,
    /// It takes links from other runners.
    pub accept: bool,
    /// Where copies of other runners' trees live (`~/.kiln/reach`).
    pub mirrors: PathBuf,
}

impl Config {
    /// A runner's usual settings: identity under `state`, copies under `~/.kiln/reach`.
    pub fn for_runner(name: &str, state: &std::path::Path, accept: bool) -> Self {
        Self { name: name.to_owned(), dir: state.join("reach"), accept, mirrors: crate::os::home_dir().join(".kiln").join("reach") }
    }
}

/// What follows a run on another runner receives.
pub type Says = async_channel::Sender<Say>;

/// Messages into the node.
pub enum In {
    /// A frame of a link, relayed by the server ([`crate::proto::ServerMsg::Reach`]).
    Frame { from: String, link: String, open: bool, data: Vec<u8>, close: Option<String> },
    /// The server's answer to a question ([`crate::proto::ServerMsg::ReachAnswer`]).
    Answer { req: u64, json: String },
    /// The connection to the server came up (true) or went (false).
    Server(bool),
    Local(Local),
    /// An event of a run this node does for another runner.
    Ran { key: (String, String), id: u64, ev: serve::Event },
    /// What a sync this node does for another runner says.
    Synced { key: (String, String), say: SyncSay },
    Tick,
}

/// What local tools ask of the node.
pub enum Local {
    Directory { reply: oneshot::Sender<anyhow::Result<Vec<RunnerInfo>>> },
    /// Link up to `peer` (if it is not), answering with the machine it is.
    Connect { peer: String, reply: oneshot::Sender<anyhow::Result<Host>> },
    /// Run on `peer`; what the run says goes to `says`. Answers with its id.
    Run { peer: String, spec: Box<RunSpec>, says: Says, reply: oneshot::Sender<u64> },
    /// Something more for a run (input, a signal, Forget).
    Ask { peer: String, ask: Ask },
    /// Follow a sync of `key` on `peer` (what it says goes to `says`), then send it `asks`.
    Sync { peer: String, key: String, says: async_channel::Sender<SyncSay> },
    SyncAsk { peer: String, ask: SyncAsk },
    /// Stop following a run or a sync.
    Leave { peer: String, run: Option<u64>, sync: Option<String> },
    Status { reply: oneshot::Sender<Value> },
    /// Forget a runner's pinned key (it was reinstalled).
    Forget { runner: String, reply: oneshot::Sender<bool> },
}

/// How local tools reach the node.
#[derive(Clone)]
pub struct Handle {
    tx: async_channel::Sender<In>,
    announce: Announce,
}

impl Handle {
    /// The frame announcing this runner's reach identity, for every (re)connect.
    pub fn hello(&self) -> AgentMsg {
        AgentMsg::ReachHello { json: serde_json::to_string(&self.announce).unwrap_or_default() }
    }

    pub fn fingerprint(&self) -> &str {
        &self.announce.fingerprint
    }

    pub async fn feed(&self, m: In) -> bool {
        self.tx.send(m).await.is_ok()
    }

    async fn ask<T>(&self, f: impl FnOnce(oneshot::Sender<T>) -> Local) -> anyhow::Result<T> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(In::Local(f(tx))).await.map_err(|_| anyhow::anyhow!("reach is not running"))?;
        rx.await.map_err(|_| anyhow::anyhow!("reach stopped"))
    }

    pub async fn directory(&self) -> anyhow::Result<Vec<RunnerInfo>> {
        self.ask(|reply| Local::Directory { reply }).await?
    }

    pub async fn connect(&self, peer: &str) -> anyhow::Result<Host> {
        self.ask(|reply| Local::Connect { peer: peer.to_owned(), reply }).await?
    }

    pub async fn run(&self, peer: &str, spec: RunSpec, says: Says) -> anyhow::Result<u64> {
        self.ask(|reply| Local::Run { peer: peer.to_owned(), spec: Box::new(spec), says, reply }).await
    }

    pub async fn send(&self, peer: &str, ask: Ask) -> bool {
        self.tx.send(In::Local(Local::Ask { peer: peer.to_owned(), ask })).await.is_ok()
    }

    pub async fn sync(&self, peer: &str, key: &str, says: async_channel::Sender<SyncSay>) -> bool {
        self.tx.send(In::Local(Local::Sync { peer: peer.to_owned(), key: key.to_owned(), says })).await.is_ok()
    }

    pub async fn sync_ask(&self, peer: &str, ask: SyncAsk) -> bool {
        self.tx.send(In::Local(Local::SyncAsk { peer: peer.to_owned(), ask })).await.is_ok()
    }

    pub async fn leave(&self, peer: &str, run: Option<u64>, sync: Option<String>) {
        let _ = self.tx.send(In::Local(Local::Leave { peer: peer.to_owned(), run, sync })).await;
    }

    pub async fn status(&self) -> anyhow::Result<Value> {
        self.ask(|reply| Local::Status { reply }).await
    }

    pub async fn forget(&self, runner: &str) -> anyhow::Result<bool> {
        self.ask(|reply| Local::Forget { runner: runner.to_owned(), reply }).await
    }
}

/// Start the node. Returns its handle and what it sends to the server (the agent forwards it on
/// [`crate::proto::LANE_REACH`]).
pub fn start(cfg: Config) -> anyhow::Result<(Handle, async_channel::Receiver<AgentMsg>)> {
    let identity = Identity::load_or_create(&cfg.dir, &cfg.name)?;
    let pins = Pins::open(&cfg.dir);
    let announce = Announce { fingerprint: identity.fingerprint.clone(), accept: cfg.accept, version: wire::VERSION };
    let (tx, rx) = async_channel::bounded::<In>(4096);
    let (out_tx, out_rx) = async_channel::bounded::<AgentMsg>(1024);
    let ticker = tx.clone();
    crate::rt::spawn(async move {
        loop {
            crate::rt::sleep(Duration::from_secs(1)).await;
            if ticker.send(In::Tick).await.is_err() {
                return;
            }
        }
    });
    let host = Host {
        name: cfg.name.clone(),
        hostname: crate::config::Config::hostname(),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        home: crate::os::home_dir().to_string_lossy().into_owned(),
        cpus: u32::try_from(std::thread::available_parallelism().map(std::num::NonZero::get).unwrap_or(1)).unwrap_or(1),
    };
    tracing::info!(fingerprint = %e2e::short(&identity.fingerprint), accept = cfg.accept, "reach ready");
    let node = Node {
        cfg,
        identity,
        pins,
        host,
        out: out_tx,
        tx: tx.clone(),
        server: false,
        directory: None,
        asked: HashMap::new(),
        next_req: 1,
        waiting: Vec::new(),
        links: HashMap::new(),
        peers: HashMap::new(),
        served: HashMap::new(),
    };
    crate::rt::spawn(async move {
        let mut node = node;
        while let Ok(m) = rx.recv().await {
            let r = std::panic::AssertUnwindSafe(node.on(m)).catch_unwind().await;
            if r.is_err() {
                crate::metrics::global().panics_caught.inc();
                tracing::error!("reach node handler panicked; carrying on");
            }
        }
    });
    Ok((Handle { tx, announce }, out_rx))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// This side opened it: it asks.
    Asker,
    /// The other side opened it: this side does the work.
    Doer,
}

struct Link {
    peer: String,
    role: Role,
    tls: Tls,
    de: Deframer,
    fingerprint: String,
    opened: Instant,
    last_rx: Instant,
    /// Its first frame went (the asker's `open`).
    sent_any: bool,
    /// The handshake finished and the key was pinned.
    pinned: bool,
    /// Doer: the session the asker named in Hello.
    session: Option<String>,
    pinged: bool,
}

/// A run this side follows on another runner.
struct Follow {
    says: Says,
    /// Output offset received up to.
    got: u64,
    exited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerState {
    Idle,
    /// Waiting for the directory (no pinned key yet).
    Resolving,
    Connecting,
    Ready,
    Retry { at: Instant, backoff: Duration },
}

/// A runner this side asks things of.
struct Peer {
    session: String,
    link: Option<String>,
    state: PeerState,
    follows: HashMap<u64, Follow>,
    syncs: HashMap<String, async_channel::Sender<SyncSay>>,
    /// Asks waiting for the link.
    queue: VecDeque<Ask>,
    next_id: u64,
    host: Option<Host>,
    waiters: Vec<oneshot::Sender<anyhow::Result<Host>>>,
    last_used: Instant,
    error: Option<String>,
}

impl Peer {
    fn new() -> Self {
        Self {
            session: crate::util::new_id(),
            link: None,
            state: PeerState::Idle,
            follows: HashMap::new(),
            syncs: HashMap::new(),
            queue: VecDeque::new(),
            next_id: 1,
            host: None,
            waiters: Vec::new(),
            last_used: Instant::now(),
            error: None,
        }
    }

    fn busy(&self) -> bool {
        !self.follows.is_empty() || !self.syncs.is_empty() || !self.queue.is_empty() || !self.waiters.is_empty()
    }
}

/// A run this side does for another runner.
struct Served {
    pid: Option<u32>,
    stdin: Option<async_channel::Sender<Vec<u8>>>,
    /// Output not yet acknowledged: (offset, stream, bytes).
    buf: VecDeque<(u64, Stream, Vec<u8>)>,
    /// Bytes of output so far (the next offset).
    next: u64,
    /// Sent on the current link up to.
    sent: u64,
    acked: u64,
    exit: Option<(Option<i32>, Option<String>)>,
    exit_sent: bool,
    linger: Duration,
    deadline: Option<Instant>,
    /// When it was told to stop for its timeout (then killed if it won't).
    stopping: Option<Instant>,
    timed_out: bool,
    /// Shared with the run's forwarder: bytes held, and its wake-up when that drops.
    held: Arc<AtomicU64>,
    room: Arc<event_listener::Event>,
}

/// A session another runner holds with this one.
struct Session {
    link: Option<String>,
    runs: HashMap<u64, Served>,
    /// Syncs, each its own worker (file system work stays off this task).
    syncs: HashMap<String, async_channel::Sender<SyncAsk>>,
    detached: Option<Instant>,
}

enum Waiting {
    Directory(oneshot::Sender<anyhow::Result<Vec<RunnerInfo>>>),
    Peer(String),
    Accept { from: String, link: String, frames: Vec<Vec<u8>> },
}

struct Node {
    cfg: Config,
    identity: Identity,
    pins: Pins,
    host: Host,
    out: async_channel::Sender<AgentMsg>,
    tx: async_channel::Sender<In>,
    server: bool,
    directory: Option<(Instant, Vec<RunnerInfo>)>,
    asked: HashMap<u64, Instant>,
    next_req: u64,
    waiting: Vec<(Instant, Waiting)>,
    links: HashMap<String, Link>,
    peers: HashMap<String, Peer>,
    served: HashMap<(String, String), Session>,
}

impl Node {
    async fn on(&mut self, m: In) {
        match m {
            In::Frame { from, link, open, data, close } => self.on_frame(from, link, open, data, close).await,
            In::Answer { req, json } => self.on_answer(req, &json).await,
            In::Server(up) => {
                self.server = up;
                // Whatever the directory said may have changed while away: ask again.
                self.directory = None;
                if !up {
                    let ids: Vec<String> = self.links.keys().cloned().collect();
                    for id in ids {
                        self.drop_link(&id, "the connection to the kiln server dropped", false).await;
                    }
                    for (_, w) in std::mem::take(&mut self.waiting) {
                        if let Waiting::Directory(reply) = w {
                            let _ = reply.send(Err(anyhow::anyhow!("not connected to the kiln server")));
                        }
                    }
                } else {
                    // Back: everyone with something to do reconnects now.
                    let names: Vec<String> = self.peers.iter().filter(|(_, p)| p.busy()).map(|(n, _)| n.clone()).collect();
                    for n in names {
                        if let Some(p) = self.peers.get_mut(&n) {
                            p.state = PeerState::Idle;
                        }
                        self.ensure_link(&n).await;
                    }
                }
            }
            In::Local(l) => self.on_local(l).await,
            In::Ran { key, id, ev } => self.on_ran(key, id, ev).await,
            In::Synced { key, say } => self.on_synced(key, say).await,
            In::Tick => self.tick().await,
        }
    }

    // ------------------------------------------------------------------ the server

    async fn send_out(&self, m: AgentMsg) -> bool {
        self.out.send(m).await.is_ok()
    }

    /// Ask the server for the directory, unless a question is out already.
    async fn fetch_directory(&mut self) {
        if self.asked.values().any(|at| at.elapsed() < Duration::from_secs(5)) {
            return;
        }
        let req = self.next_req;
        self.next_req += 1;
        self.asked.insert(req, Instant::now());
        self.send_out(AgentMsg::ReachAsk { req, json: json!({"op": "directory"}).to_string() }).await;
    }

    fn fresh_directory(&self) -> Option<&[RunnerInfo]> {
        self.directory.as_ref().filter(|(at, _)| at.elapsed() < DIRECTORY_TTL).map(|(_, d)| d.as_slice())
    }

    async fn on_answer(&mut self, req: u64, json: &str) {
        if self.asked.remove(&req).is_none() {
            return;
        }
        let runners: Vec<RunnerInfo> = serde_json::from_str::<Value>(json).ok().and_then(|v| v.get("runners").cloned()).and_then(|r| serde_json::from_value(r).ok()).unwrap_or_default();
        self.directory = Some((Instant::now(), runners.clone()));
        for (_, w) in std::mem::take(&mut self.waiting) {
            match w {
                Waiting::Directory(reply) => {
                    let _ = reply.send(Ok(runners.clone()));
                }
                Waiting::Peer(name) => self.connect_resolved(&name).await,
                Waiting::Accept { from, link, frames } => self.accept(from, link, frames).await,
            }
        }
    }

    /// The key a runner must present: its pin, else the directory's. An error when the two
    /// disagree (the server says another key than the one met before) or neither knows it.
    fn expected(&self, name: &str) -> Result<String, String> {
        let listed = self.directory.as_ref().and_then(|(_, d)| d.iter().find(|r| r.name == name)).and_then(|r| r.fingerprint.clone());
        match (self.pins.get(name), listed) {
            (Some(pin), Some(l)) if pin.fingerprint != l => Err(format!(
                "{name}'s reach key changed (pinned {}, the server now says {}). If {name} was reinstalled, run `kiln reach forget {name}` on {} to trust its new key.",
                e2e::short(&pin.fingerprint),
                e2e::short(&l),
                self.cfg.name
            )),
            (Some(pin), _) => Ok(pin.fingerprint),
            (None, Some(l)) => Ok(l),
            (None, None) => Err(format!("{name} has no reach identity the server knows of (offline, or an older runner)")),
        }
    }

    // ------------------------------------------------------------------ links

    async fn send_frames(&mut self, id: &str, close: Option<String>) {
        let Some(l) = self.links.get_mut(id) else { return };
        let data = l.tls.outgoing();
        if data.is_empty() && close.is_none() {
            return;
        }
        let open = l.role == Role::Asker && !l.sent_any;
        l.sent_any = true;
        let to = l.peer.clone();
        let mut chunks: Vec<Vec<u8>> = data.chunks(FRAME).map(<[u8]>::to_vec).collect();
        if chunks.is_empty() {
            chunks.push(Vec::new());
        }
        let n = chunks.len();
        for (i, c) in chunks.into_iter().enumerate() {
            let last = i + 1 == n;
            let msg = AgentMsg::Reach { to: to.clone(), link: id.to_owned(), open: open && i == 0, data: c, close: if last { close.clone() } else { None } };
            if !self.send_out(msg).await {
                return;
            }
        }
    }

    /// Send a message on a link.
    async fn say_on<T: Serialize>(&mut self, id: &str, msg: &T) -> bool {
        let Ok(bytes) = wire::frame(msg) else { return false };
        let Some(l) = self.links.get_mut(id) else { return false };
        if l.tls.send(&bytes).is_err() {
            return false;
        }
        self.send_frames(id, None).await;
        true
    }

    /// The link is over: tell the server (unless the close came from there) and both roles' state.
    async fn drop_link(&mut self, id: &str, why: &str, tell: bool) {
        if tell && self.links.contains_key(id) {
            if let Some(l) = self.links.get_mut(id) {
                l.tls.close();
            }
            self.send_frames(id, Some(why.to_owned())).await;
        }
        let Some(l) = self.links.remove(id) else { return };
        tracing::debug!(peer = %l.peer, link = id, role = ?l.role, why, "reach link closed");
        match l.role {
            Role::Asker => {
                if let Some(p) = self.peers.get_mut(&l.peer)
                    && p.link.as_deref() == Some(id)
                {
                    p.link = None;
                    let backoff = match p.state {
                        PeerState::Retry { backoff, .. } => (backoff * 2).min(Duration::from_secs(5)),
                        _ => Duration::from_millis(200),
                    };
                    p.state = PeerState::Retry { at: Instant::now() + crate::sup::jitter(backoff), backoff };
                    p.error = Some(why.to_owned());
                    // Asks for a run never sent wait for the next link; acks and pings do not matter.
                    p.queue.retain(|a| matches!(a, Ask::Run(_) | Ask::Stdin { .. } | Ask::StdinEnd { .. } | Ask::Signal { .. }));
                    for w in p.waiters.drain(..) {
                        let _ = w.send(Err(anyhow::anyhow!("{why}")));
                    }
                    // A sync can't know which of its pieces arrived: its driver starts it over (it
                    // sends only what still differs).
                    for (key, s) in p.syncs.drain() {
                        let _ = s.try_send(SyncSay::Failed { key, error: format!("interrupted: {why}") });
                    }
                }
            }
            Role::Doer => {
                if let Some(s) = l.session.as_ref().and_then(|s| self.served.get_mut(&(l.peer.clone(), s.clone())))
                    && s.link.as_deref() == Some(id)
                {
                    s.link = None;
                    s.detached = Some(Instant::now());
                }
            }
        }
    }

    async fn on_frame(&mut self, from: String, link: String, open: bool, data: Vec<u8>, close: Option<String>) {
        if open && !self.links.contains_key(&link) {
            if !self.cfg.accept {
                let msg = AgentMsg::Reach { to: from.clone(), link, open: false, data: Vec::new(), close: Some(format!("{} does not accept commands from other runners", self.cfg.name)) };
                self.send_out(msg).await;
                return;
            }
            if self.fresh_directory().is_none() && self.pins.get(&from).is_none() {
                self.waiting.push((Instant::now(), Waiting::Accept { from, link, frames: vec![data] }));
                self.fetch_directory().await;
                return;
            }
            self.accept(from, link, vec![data]).await;
            return;
        }
        // More frames of a link still waiting for the directory.
        if let Some((_, Waiting::Accept { frames, .. })) = self.waiting.iter_mut().find(|(_, w)| matches!(w, Waiting::Accept { link: l, .. } if *l == link)) {
            frames.push(data);
            return;
        }
        let Some(l) = self.links.get_mut(&link) else {
            if close.is_none() {
                let msg = AgentMsg::Reach { to: from, link, open: false, data: Vec::new(), close: Some("no such link".into()) };
                self.send_out(msg).await;
            }
            return;
        };
        if l.peer != from {
            return;
        }
        l.last_rx = Instant::now();
        l.pinged = false;
        let mut plain = Vec::new();
        let was_ready = l.tls.ready();
        let role = l.role;
        if let Err(e) = l.tls.receive(&data, &mut plain) {
            let why = format!("{e:#}");
            tracing::warn!(peer = %from, link = %link, error = %why, "reach link failed");
            self.send_frames(&link, None).await;
            self.drop_link(&link, &why, true).await;
            // A handshake that failed (a key that isn't the expected one) won't do better on retry.
            if !was_ready && role == Role::Asker {
                self.fail_peer(&from, &format!("{from}'s reach link was refused: {why}")).await;
            }
            return;
        }
        l.de.push(&plain);
        let ready = l.tls.ready();
        let closed = l.tls.closed();
        // The handshake proved the key: pin it the first time.
        if ready && !l.pinned {
            l.pinned = true;
            let (peer, fp) = (l.peer.clone(), l.fingerprint.clone());
            if let Trust::First = self.pins.check(&peer, &fp) {
                tracing::info!(runner = %peer, fingerprint = %e2e::short(&fp), "pinned a runner's reach key");
            }
        }
        // Whatever the handshake left to say (its Finished, the held first message).
        self.send_frames(&link, None).await;
        loop {
            let Some(l) = self.links.get_mut(&link) else { return };
            let role = l.role;
            let next = match role {
                Role::Asker => l.de.take::<Say>().map(|m| m.map(Msg::Say)),
                Role::Doer => l.de.take::<Ask>().map(|m| m.map(Msg::Ask)),
            };
            match next {
                Ok(Some(Msg::Say(s))) => self.on_say(&link, s).await,
                Ok(Some(Msg::Ask(a))) => self.on_ask(&link, a).await,
                Ok(None) => break,
                Err(e) => {
                    self.drop_link(&link, &format!("{e:#}"), true).await;
                    return;
                }
            }
        }
        if closed || close.is_some() {
            self.drop_link(&link, close.as_deref().unwrap_or("closed by the other runner"), false).await;
        }
    }

    // ------------------------------------------------------------------ asking

    async fn ensure_link(&mut self, name: &str) {
        let Some(p) = self.peers.get_mut(name) else { return };
        p.last_used = Instant::now();
        match p.state {
            PeerState::Ready | PeerState::Connecting | PeerState::Resolving => return,
            PeerState::Retry { at, .. } if Instant::now() < at => return,
            _ => {}
        }
        if !self.server {
            self.fail_peer(name, "not connected to the kiln server; waiting for it").await;
            return;
        }
        if self.pins.get(name).is_none() && self.fresh_directory().is_none_or(|d| !d.iter().any(|r| r.name == name)) {
            if let Some(p) = self.peers.get_mut(name) {
                p.state = PeerState::Resolving;
            }
            self.waiting.push((Instant::now(), Waiting::Peer(name.to_owned())));
            self.fetch_directory().await;
            return;
        }
        self.connect_resolved(name).await;
    }

    async fn connect_resolved(&mut self, name: &str) {
        let fp = match self.expected(name) {
            Ok(f) => f,
            Err(why) => {
                self.fail_peer(name, &why).await;
                return;
            }
        };
        let tls = match Tls::client(&self.identity, &fp) {
            Ok(t) => t,
            Err(e) => {
                self.fail_peer(name, &format!("{e:#}")).await;
                return;
            }
        };
        let Some(p) = self.peers.get_mut(name) else { return };
        let id = crate::util::new_id();
        let resume: Vec<Resume> = p.follows.iter().map(|(id, f)| Resume { id: *id, from: f.got }).collect();
        let hello = Ask::Hello { version: wire::VERSION, session: p.session.clone(), resume };
        p.link = Some(id.clone());
        p.state = PeerState::Connecting;
        let now = Instant::now();
        self.links.insert(id.clone(), Link { peer: name.to_owned(), role: Role::Asker, tls, de: Deframer::default(), fingerprint: fp, opened: now, last_rx: now, sent_any: false, pinned: false, session: None, pinged: false });
        // Hello rides along: held until the handshake is done, then sent encrypted.
        self.say_on(&id, &hello).await;
    }

    /// Nothing can be done with `name` for now: everyone waiting on it hears why. Runs keep their
    /// place (they resume when a link comes back); a peer that never got a link fails its runs.
    async fn fail_peer(&mut self, name: &str, why: &str) {
        let Some(p) = self.peers.get_mut(name) else { return };
        p.error = Some(why.to_owned());
        let backoff = match p.state {
            PeerState::Retry { backoff, .. } => (backoff * 2).min(Duration::from_secs(5)),
            _ => Duration::from_secs(1),
        };
        p.state = PeerState::Retry { at: Instant::now() + backoff, backoff };
        for w in p.waiters.drain(..) {
            let _ = w.send(Err(anyhow::anyhow!("{why}")));
        }
        // Runs that never started there can't resume: they end here, with why.
        let never: Vec<u64> = p.queue.iter().filter_map(|a| if let Ask::Run(s) = a { Some(s.id) } else { None }).collect();
        if p.host.is_none() || !never.is_empty() {
            p.queue.retain(|a| !matches!(a, Ask::Run(_)));
            for id in never {
                if let Some(f) = p.follows.remove(&id) {
                    let _ = f.says.send(Say::Exit { id, code: None, signal: None, error: Some(why.to_owned()), end: f.got, lost: 0 }).await;
                }
            }
            for (key, s) in p.syncs.drain() {
                let _ = s.send(SyncSay::Failed { key, error: why.to_owned() }).await;
            }
        }
    }

    async fn flush_queue(&mut self, name: &str) {
        loop {
            let Some(p) = self.peers.get_mut(name) else { return };
            if p.state != PeerState::Ready {
                return;
            }
            let (Some(link), Some(a)) = (p.link.clone(), p.queue.pop_front()) else { return };
            if !self.say_on(&link, &a).await {
                if let Some(p) = self.peers.get_mut(name) {
                    p.queue.push_front(a);
                }
                return;
            }
        }
    }

    async fn send_or_queue(&mut self, name: &str, a: Ask) {
        let Some(p) = self.peers.get_mut(name) else { return };
        p.last_used = Instant::now();
        p.queue.push_back(a);
        if p.state == PeerState::Ready {
            self.flush_queue(name).await;
        } else {
            self.ensure_link(name).await;
        }
    }

    async fn on_say(&mut self, link: &str, s: Say) {
        let Some(name) = self.links.get(link).map(|l| l.peer.clone()) else { return };
        match s {
            Say::Welcome { version, host, resumed, lost } => {
                if version != wire::VERSION {
                    self.drop_link(link, &format!("{name} speaks reach version {version}, this runner {}", wire::VERSION), true).await;
                    return;
                }
                let Some(p) = self.peers.get_mut(&name) else { return };
                p.state = PeerState::Ready;
                p.host = Some(host.clone());
                p.error = None;
                for w in p.waiters.drain(..) {
                    let _ = w.send(Ok(host.clone()));
                }
                tracing::debug!(peer = %name, resumed = resumed.len(), lost = lost.len(), "reach link ready");
                for id in lost {
                    if let Some(f) = p.follows.remove(&id) {
                        let _ = f.says.send(Say::Exit { id, code: None, signal: None, error: Some(format!("{name} no longer has this run (its runner restarted)")), end: f.got, lost: 0 }).await;
                    }
                }
                self.flush_queue(&name).await;
            }
            Say::Refused { reason } => {
                self.fail_peer(&name, &reason).await;
                self.drop_link(link, &reason, true).await;
            }
            Say::Output { id, offset, stream, data } => {
                let Some(p) = self.peers.get_mut(&name) else { return };
                let Some(f) = p.follows.get_mut(&id) else { return };
                let end = offset + data.len() as u64;
                if end <= f.got {
                    return;
                }
                let lost = offset.saturating_sub(f.got);
                // A replay that overlaps what arrived: only the new part.
                let start = offset.max(f.got);
                let data = data.get(usize::try_from(start - offset).unwrap_or(0)..).map(<[u8]>::to_vec).unwrap_or_default();
                f.got = end;
                let says = f.says.clone();
                if lost > 0 {
                    let note = format!("\n[kiln reach: {lost} bytes of output were dropped]\n").into_bytes();
                    let _ = says.send(Say::Output { id, offset: start, stream: Stream::Err, data: note }).await;
                }
                let _ = says.send(Say::Output { id, offset: start, stream, data }).await;
                self.say_on(link, &Ask::Ack { id, upto: end }).await;
            }
            Say::Exit { id, code, signal, error, end, lost } => {
                let Some(p) = self.peers.get_mut(&name) else { return };
                let Some(f) = p.follows.get_mut(&id) else { return };
                // On one link output always comes before the exit; a shortfall is output lost.
                let lost = lost + end.saturating_sub(f.got);
                f.exited = true;
                let _ = f.says.send(Say::Exit { id, code, signal, error, end, lost }).await;
                p.follows.remove(&id);
                self.say_on(link, &Ask::Forget { id }).await;
            }
            Say::Started { id, pid, cwd } => {
                if let Some(f) = self.peers.get(&name).and_then(|p| p.follows.get(&id)) {
                    let _ = f.says.send(Say::Started { id, pid, cwd }).await;
                }
            }
            Say::Sync(s) => {
                let key = match &s {
                    SyncSay::Need { key, .. } | SyncSay::Got { key, .. } | SyncSay::Done { key, .. } | SyncSay::Failed { key, .. } => key.clone(),
                };
                let done = matches!(s, SyncSay::Done { .. } | SyncSay::Failed { .. });
                if let Some(p) = self.peers.get_mut(&name) {
                    if let Some(tx) = p.syncs.get(&key) {
                        let _ = tx.send(s).await;
                    }
                    if done {
                        p.syncs.remove(&key);
                    }
                }
            }
            Say::Pong { .. } => {}
        }
    }

    // ------------------------------------------------------------------ doing

    async fn accept(&mut self, from: String, link: String, frames: Vec<Vec<u8>>) {
        let refuse = |why: String| AgentMsg::Reach { to: from.clone(), link: link.clone(), open: false, data: Vec::new(), close: Some(why) };
        let fp = match self.expected(&from) {
            Ok(f) => f,
            Err(why) => {
                tracing::warn!(runner = %from, why = %why, "refused a reach link");
                self.send_out(refuse(why)).await;
                return;
            }
        };
        let tls = match Tls::server(&self.identity, &fp) {
            Ok(t) => t,
            Err(e) => {
                self.send_out(refuse(format!("{e:#}"))).await;
                return;
            }
        };
        let now = Instant::now();
        self.links.insert(link.clone(), Link { peer: from.clone(), role: Role::Doer, tls, de: Deframer::default(), fingerprint: fp, opened: now, last_rx: now, sent_any: true, pinned: false, session: None, pinged: false });
        for data in frames {
            Box::pin(self.on_frame(from.clone(), link.clone(), false, data, None)).await;
        }
    }

    async fn on_ask(&mut self, link: &str, a: Ask) {
        let Some((peer, session)) = self.links.get(link).map(|l| (l.peer.clone(), l.session.clone())) else { return };
        if let Ask::Hello { version, session, resume } = a {
            if version != wire::VERSION {
                self.say_on(link, &Say::Refused { reason: format!("{} speaks reach version {}, not {version}", self.cfg.name, wire::VERSION) }).await;
                return;
            }
            let key = (peer.clone(), session.clone());
            if let Some(l) = self.links.get_mut(link) {
                l.session = Some(session.clone());
            }
            let s = self.served.entry(key.clone()).or_insert_with(|| Session { link: None, runs: HashMap::new(), syncs: HashMap::new(), detached: None });
            let old = s.link.replace(link.to_owned());
            s.detached = None;
            let mut resumed = Vec::new();
            let mut lost = Vec::new();
            for r in &resume {
                match s.runs.get_mut(&r.id) {
                    Some(run) => {
                        // Everything up to `from` arrived; the rest is sent again.
                        while run.buf.front().is_some_and(|(o, _, d)| o + d.len() as u64 <= r.from) {
                            if let Some((_, _, d)) = run.buf.pop_front() {
                                run.held.fetch_sub(d.len() as u64, Ordering::Relaxed);
                            }
                        }
                        run.room.notify(usize::MAX);
                        run.acked = r.from.max(run.acked);
                        run.sent = run.acked;
                        run.exit_sent = false;
                        resumed.push(r.id);
                    }
                    None => lost.push(r.id),
                }
            }
            if let Some(old) = old.filter(|o| o != link) {
                self.drop_link(&old, "replaced by a new link", true).await;
            }
            self.say_on(link, &Say::Welcome { version: wire::VERSION, host: self.host.clone(), resumed: resumed.clone(), lost }).await;
            for id in resumed {
                self.pump(&key, id).await;
            }
            return;
        }
        let Some(session) = session else {
            self.drop_link(link, "Hello first", true).await;
            return;
        };
        let key = (peer, session);
        match a {
            Ask::Hello { .. } => {}
            Ask::Run(spec) => self.start_run(&key, *spec).await,
            Ask::Stdin { id, data } => {
                if let Some(tx) = self.served.get(&key).and_then(|s| s.runs.get(&id)).and_then(|r| r.stdin.clone())
                    && !data.is_empty()
                {
                    let _ = tx.send(data).await;
                }
            }
            Ask::StdinEnd { id } => {
                if let Some(r) = self.served.get_mut(&key).and_then(|s| s.runs.get_mut(&id))
                    && let Some(tx) = r.stdin.take()
                {
                    let _ = tx.send(Vec::new()).await;
                }
            }
            Ask::Ack { id, upto } => {
                if let Some(r) = self.served.get_mut(&key).and_then(|s| s.runs.get_mut(&id)) {
                    r.acked = r.acked.max(upto.min(r.next));
                    while r.buf.front().is_some_and(|(o, _, d)| o + d.len() as u64 <= r.acked) {
                        if let Some((_, _, d)) = r.buf.pop_front() {
                            r.held.fetch_sub(d.len() as u64, Ordering::Relaxed);
                        }
                    }
                    r.room.notify(usize::MAX);
                }
                self.pump(&key, id).await;
            }
            Ask::Signal { id, signal } => {
                if let Some(pid) = self.served.get(&key).and_then(|s| s.runs.get(&id)).and_then(|r| r.pid) {
                    serve::signal_group(pid, signal);
                }
            }
            Ask::Forget { id } => {
                if let Some(s) = self.served.get_mut(&key)
                    && s.runs.get(&id).is_some_and(|r| r.exit.is_some())
                {
                    s.runs.remove(&id);
                }
            }
            Ask::Sync(sa) => self.on_sync_ask(&key, sa).await,
            Ask::Ping { at } => {
                self.say_on(link, &Say::Pong { at }).await;
            }
        }
    }

    async fn start_run(&mut self, key: &(String, String), spec: RunSpec) {
        let id = spec.id;
        let Some(s) = self.served.get_mut(key) else { return };
        if s.runs.contains_key(&id) {
            return;
        }
        let mirror = match spec.mirror.as_deref() {
            Some(m) => match Mirror::root_for(&self.cfg.mirrors, &key.0, m) {
                Ok(r) => Some(r),
                Err(e) => {
                    self.refuse_run(key, id, format!("{e:#}")).await;
                    return;
                }
            },
            None => None,
        };
        let cwd = match serve::resolve_cwd(&spec, mirror.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                self.refuse_run(key, id, format!("{e:#}")).await;
                return;
            }
        };
        let shell = spec.shell.as_deref().map(|name| {
            let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' }).take(64).collect();
            self.cfg.dir.join("shells").join(&key.0).join(&key.1).join(safe)
        });
        let held = Arc::new(AtomicU64::new(0));
        let room = Arc::new(event_listener::Event::new());
        let (ev_tx, ev_rx) = async_channel::bounded::<serve::Event>(16);
        let extra = vec![("KILN_REACH_FROM".to_owned(), key.0.clone())];
        let stdin = serve::start(spec.clone(), cwd, shell, extra, ev_tx);
        // Forward the run's events into the node, waiting while its asker has too much unread.
        let (tx, k, h, r) = (self.tx.clone(), key.clone(), Arc::clone(&held), Arc::clone(&room));
        crate::rt::spawn(async move {
            while let Ok(ev) = ev_rx.recv().await {
                if let serve::Event::Output { data, .. } = &ev {
                    while h.load(Ordering::Relaxed) >= BUFFER {
                        let wait = r.listen();
                        if h.load(Ordering::Relaxed) < BUFFER {
                            break;
                        }
                        wait.await;
                    }
                    h.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                if tx.send(In::Ran { key: k.clone(), id, ev }).await.is_err() {
                    return;
                }
            }
        });
        let now = Instant::now();
        s.runs.insert(
            id,
            Served {
                pid: None,
                stdin: spec.stdin.then_some(stdin),
                buf: VecDeque::new(),
                next: 0,
                sent: 0,
                acked: 0,
                exit: None,
                exit_sent: false,
                linger: spec.linger_ms.map_or(LINGER, Duration::from_millis),
                deadline: spec.timeout_ms.map(|t| now + Duration::from_millis(t)),
                stopping: None,
                timed_out: false,
                held,
                room,
            },
        );
    }

    async fn refuse_run(&mut self, key: &(String, String), id: u64, why: String) {
        let link = self.served.get(key).and_then(|s| s.link.clone());
        if let Some(link) = link {
            self.say_on(&link, &Say::Exit { id, code: None, signal: None, error: Some(why), end: 0, lost: 0 }).await;
        }
    }

    async fn on_ran(&mut self, key: (String, String), id: u64, ev: serve::Event) {
        let Some(s) = self.served.get_mut(&key) else { return };
        let Some(r) = s.runs.get_mut(&id) else { return };
        let link = s.link.clone();
        match ev {
            serve::Event::Started { pid, cwd } => {
                r.pid = Some(pid);
                if let Some(link) = link {
                    self.say_on(&link, &Say::Started { id, pid, cwd }).await;
                }
            }
            serve::Event::Output { stream, data } => {
                let off = r.next;
                r.next += data.len() as u64;
                r.buf.push_back((off, stream, data));
                self.pump(&key, id).await;
            }
            serve::Event::Exit { code, error } => {
                let error = if r.timed_out { Some(error.unwrap_or_else(|| "timed out".into())) } else { error };
                r.exit = Some((code, error));
                r.stdin = None;
                self.pump(&key, id).await;
            }
        }
    }

    /// Send what the run has within the window, then its exit once all output went.
    async fn pump(&mut self, key: &(String, String), id: u64) {
        let Some(link) = self.served.get(key).and_then(|s| s.link.clone()) else { return };
        loop {
            let Some(r) = self.served.get_mut(key).and_then(|s| s.runs.get_mut(&id)) else { return };
            if r.sent < r.next && r.sent.saturating_sub(r.acked) < wire::WINDOW {
                let sent = r.sent;
                let Some((off, stream, data)) = r.buf.iter().find(|(o, _, d)| o + d.len() as u64 > sent).map(|(o, s, d)| (*o, *s, d.clone())) else { return };
                let skip = usize::try_from(sent.saturating_sub(off)).unwrap_or(0);
                let piece = data.get(skip..).map(<[u8]>::to_vec).unwrap_or_default();
                r.sent = off + data.len() as u64;
                let msg = Say::Output { id, offset: off + skip as u64, stream, data: piece };
                if !self.say_on(&link, &msg).await {
                    return;
                }
                continue;
            }
            if r.sent >= r.next
                && !r.exit_sent
                && let Some((code, error)) = r.exit.clone()
            {
                r.exit_sent = true;
                let end = r.next;
                self.say_on(&link, &Say::Exit { id, code, signal: None, error, end, lost: 0 }).await;
            }
            return;
        }
    }

    async fn on_sync_ask(&mut self, key: &(String, String), a: SyncAsk) {
        let sync_key = match &a {
            SyncAsk::Plan { key, .. } | SyncAsk::File { key, .. } | SyncAsk::Commit { key } => key.clone(),
        };
        let Some(s) = self.served.get_mut(key) else { return };
        if !s.syncs.contains_key(&sync_key) {
            let root = match Mirror::root_for(&self.cfg.mirrors, &key.0, &sync_key) {
                Ok(r) => r,
                Err(e) => {
                    let say = SyncSay::Failed { key: sync_key, error: format!("{e:#}") };
                    if let Some(link) = s.link.clone() {
                        self.say_on(&link, &Say::Sync(say)).await;
                    }
                    return;
                }
            };
            s.syncs.insert(sync_key.clone(), sync_worker(root, sync_key.clone(), key.clone(), self.tx.clone()));
        }
        if let Some(w) = s.syncs.get(&sync_key)
            && w.send(a).await.is_err()
        {
            s.syncs.remove(&sync_key);
        }
    }

    async fn on_synced(&mut self, key: (String, String), say: SyncSay) {
        let Some(s) = self.served.get_mut(&key) else { return };
        if let SyncSay::Done { key: k, .. } | SyncSay::Failed { key: k, .. } = &say {
            s.syncs.remove(k);
        }
        if let Some(link) = s.link.clone() {
            self.say_on(&link, &Say::Sync(say)).await;
        }
    }

    // ------------------------------------------------------------------ local tools

    async fn on_local(&mut self, l: Local) {
        match l {
            Local::Directory { reply } => {
                if let Some(d) = self.fresh_directory() {
                    let _ = reply.send(Ok(d.to_vec()));
                    return;
                }
                if !self.server {
                    let _ = reply.send(Err(anyhow::anyhow!("not connected to the kiln server")));
                    return;
                }
                self.waiting.push((Instant::now(), Waiting::Directory(reply)));
                self.fetch_directory().await;
            }
            Local::Connect { peer, reply } => {
                if peer == self.cfg.name {
                    let _ = reply.send(Err(anyhow::anyhow!("that is this runner")));
                    return;
                }
                let p = self.peers.entry(peer.clone()).or_insert_with(Peer::new);
                if p.state == PeerState::Ready
                    && let Some(h) = p.host.clone()
                {
                    let _ = reply.send(Ok(h));
                    return;
                }
                p.waiters.push(reply);
                if matches!(p.state, PeerState::Retry { .. }) {
                    p.state = PeerState::Idle;
                }
                self.ensure_link(&peer).await;
            }
            Local::Run { peer, mut spec, says, reply } => {
                let p = self.peers.entry(peer.clone()).or_insert_with(Peer::new);
                let id = p.next_id;
                p.next_id += 1;
                spec.id = id;
                p.follows.insert(id, Follow { says, got: 0, exited: false });
                let _ = reply.send(id);
                self.send_or_queue(&peer, Ask::Run(spec)).await;
            }
            Local::Ask { peer, ask } => {
                if self.peers.contains_key(&peer) {
                    self.send_or_queue(&peer, ask).await;
                }
            }
            Local::Sync { peer, key, says } => {
                let p = self.peers.entry(peer.clone()).or_insert_with(Peer::new);
                p.syncs.insert(key, says);
                self.ensure_link(&peer).await;
            }
            Local::SyncAsk { peer, ask } => {
                if self.peers.contains_key(&peer) {
                    self.send_or_queue(&peer, Ask::Sync(ask)).await;
                }
            }
            Local::Leave { peer, run, sync } => {
                if let Some(p) = self.peers.get_mut(&peer) {
                    if let Some(id) = run {
                        p.follows.remove(&id);
                    }
                    if let Some(k) = sync {
                        p.syncs.remove(&k);
                    }
                }
            }
            Local::Status { reply } => {
                let _ = reply.send(self.status());
            }
            Local::Forget { runner, reply } => {
                let _ = reply.send(self.pins.forget(&runner));
            }
        }
    }

    fn status(&self) -> Value {
        let peers: Vec<Value> = self
            .peers
            .iter()
            .map(|(n, p)| {
                json!({
                    "runner": n,
                    "state": format!("{:?}", p.state).split_whitespace().next().unwrap_or("").trim_end_matches('{').to_owned(),
                    "runs": p.follows.len(),
                    "syncs": p.syncs.len(),
                    "host": p.host,
                    "error": p.error,
                    "secured": p.link.as_ref().and_then(|l| self.links.get(l)).and_then(|l| l.tls.negotiated()),
                })
            })
            .collect();
        let serving: Vec<Value> = self.served.iter().map(|((r, _), s)| json!({"runner": r, "runs": s.runs.len(), "linked": s.link.is_some()})).collect();
        let pins: Vec<Value> = self.pins.all().into_iter().map(|(n, p)| json!({"runner": n, "fingerprint": p.fingerprint, "since": p.since})).collect();
        json!({
            "runner": self.cfg.name,
            "fingerprint": self.identity.fingerprint,
            "accept": self.cfg.accept,
            "server": self.server,
            "peers": peers,
            "serving": serving,
            "pins": pins,
        })
    }

    // ------------------------------------------------------------------ time

    async fn tick(&mut self) {
        let now = Instant::now();
        // Questions and waits that went unanswered.
        self.asked.retain(|_, at| at.elapsed() < Duration::from_secs(10));
        let mut stale = Vec::new();
        self.waiting.retain_mut(|(at, w)| {
            if at.elapsed() < Duration::from_secs(10) {
                return true;
            }
            stale.push(std::mem::replace(w, Waiting::Peer(String::new())));
            false
        });
        for w in stale {
            match w {
                Waiting::Directory(reply) => {
                    let _ = reply.send(Err(anyhow::anyhow!("the kiln server did not answer in time")));
                }
                Waiting::Peer(n) if !n.is_empty() => self.fail_peer(&n, "the kiln server did not say who that runner is in time").await,
                _ => {}
            }
        }
        // Links: handshakes that never finished, quiet links (pinged, then given up).
        let mut dead = Vec::new();
        let mut ping = Vec::new();
        for (id, l) in &self.links {
            if !l.tls.ready() && l.opened.elapsed() > HANDSHAKE {
                dead.push((id.clone(), format!("{} did not answer in {}s", l.peer, HANDSHAKE.as_secs())));
            } else if l.last_rx.elapsed() > DEAD_AFTER {
                dead.push((id.clone(), format!("{} went quiet", l.peer)));
            } else if l.role == Role::Asker && l.tls.ready() && !l.pinged && l.last_rx.elapsed() > PING_AFTER {
                ping.push(id.clone());
            }
        }
        for (id, why) in dead {
            self.drop_link(&id, &why, true).await;
        }
        for id in ping {
            if let Some(l) = self.links.get_mut(&id) {
                l.pinged = true;
            }
            self.say_on(&id, &Ask::Ping { at: crate::util::now_ms() }).await;
        }
        // Peers: retry those with work; let idle ones go.
        let names: Vec<String> = self.peers.keys().cloned().collect();
        for n in names {
            let Some(p) = self.peers.get(&n) else { continue };
            if !p.busy() && p.last_used.elapsed() > IDLE_LINK {
                if let Some(l) = p.link.clone() {
                    self.drop_link(&l, "idle", true).await;
                }
                self.peers.remove(&n);
                continue;
            }
            if p.busy() && matches!(p.state, PeerState::Retry { at, .. } if now >= at) {
                self.ensure_link(&n).await;
            }
        }
        // Runs done for others: timeouts, askers that never came back.
        let mut gone = Vec::new();
        for (key, s) in &mut self.served {
            let detached = s.detached.map(|d| d.elapsed());
            s.runs.retain(|_, r| {
                if let Some(d) = r.deadline
                    && now >= d
                    && r.exit.is_none()
                    && let Some(pid) = r.pid
                {
                    match r.stopping {
                        None => {
                            r.timed_out = true;
                            r.stopping = Some(now);
                            serve::signal_group(pid, Signal::Terminate);
                        }
                        Some(at) if now.duration_since(at) > Duration::from_secs(5) => serve::signal_group(pid, Signal::Kill),
                        Some(_) => {}
                    }
                }
                match detached {
                    Some(d) if r.exit.is_none() && d > r.linger => {
                        if let Some(pid) = r.pid {
                            serve::signal_group(pid, Signal::Kill);
                        }
                        false
                    }
                    Some(d) if r.exit.is_some() && d > UNCOLLECTED => false,
                    _ => true,
                }
            });
            if s.runs.is_empty() && s.syncs.is_empty() && detached.is_some_and(|d| d > UNCOLLECTED) {
                gone.push(key.clone());
            }
        }
        for k in gone {
            self.served.remove(&k);
        }
    }
}

enum Msg {
    Say(Say),
    Ask(Ask),
}

/// A sync done for another runner: its own task, the file system work on blocking threads. What it
/// says goes back through the node.
fn sync_worker(root: PathBuf, key: String, session: (String, String), node: async_channel::Sender<In>) -> async_channel::Sender<SyncAsk> {
    let (tx, rx) = async_channel::bounded::<SyncAsk>(64);
    crate::rt::spawn(async move {
        let say = |s: SyncSay| {
            let node = node.clone();
            let session = session.clone();
            async move { node.send(In::Synced { key: session, say: s }).await.is_ok() }
        };
        let mirror = match crate::rt::blocking({
            let root = root.clone();
            move || Mirror::new(root)
        })
        .await
        {
            Ok(Ok(m)) => m,
            Ok(Err(e)) | Err(e) => {
                say(SyncSay::Failed { key, error: format!("{e:#}") }).await;
                return;
            }
        };
        let mirror = Arc::new(parking_lot::Mutex::new(mirror));
        let mut reported = 0u64;
        while let Ok(a) = rx.recv().await {
            let m = Arc::clone(&mirror);
            let root = root.clone();
            let step = crate::rt::blocking(move || -> anyhow::Result<Option<SyncSay>> {
                let mut m = m.lock();
                match a {
                    SyncAsk::Plan { key, entries, start, more } => {
                        // The asker started over (a link dropped mid-sync): so does the copy's list.
                        if start {
                            *m = Mirror::new(root)?;
                        }
                        m.plan(entries)?;
                        if more {
                            return Ok(None);
                        }
                        let paths = super::sync::need(&m.root, m.entries());
                        Ok(Some(SyncSay::Need { key, root: m.root.to_string_lossy().into_owned(), paths }))
                    }
                    SyncAsk::File { key: _, path, offset, data, last } => {
                        m.write(&path, offset, &data, last)?;
                        Ok(None)
                    }
                    SyncAsk::Commit { key } => {
                        let removed = m.commit()?;
                        Ok(Some(SyncSay::Done { key, root: m.root.to_string_lossy().into_owned(), written: m.written, removed, bytes: m.bytes }))
                    }
                }
            })
            .await;
            match step {
                Ok(Ok(Some(s))) => {
                    let done = matches!(s, SyncSay::Done { .. });
                    if !say(s).await || done {
                        return;
                    }
                }
                Ok(Ok(None)) => {
                    let bytes = mirror.lock().bytes;
                    if bytes >= reported + (1 << 20) {
                        reported = bytes;
                        say(SyncSay::Got { key: key.clone(), bytes }).await;
                    }
                }
                Ok(Err(e)) | Err(e) => {
                    say(SyncSay::Failed { key, error: format!("{e:#}") }).await;
                    return;
                }
            }
        }
    });
    tx
}
