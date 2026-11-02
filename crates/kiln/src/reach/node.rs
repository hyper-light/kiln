//! A runner's reach node (`docs/reach.md` §4): its encrypted links to other runners, the runs it
//! follows on them, and the runs it does for them. One task owns all of it; the agent feeds it the
//! server's frames, local tools (`kiln run`, the agents' MCP tools) talk to it through a
//! [`Handle`].
//!
//! Asking side: one session per runner it talks to, carried by one link at a time. A link that
//! drops is replaced (backoff from 200 ms to 5 s) and the session resumed: each run it follows
//! continues from the last byte that arrived. A detached run whose client went is followed on
//! alone: its output taken and let go, its link made again after a drop, until it ends.
//!
//! Doing side: a run outlives its link. Its output waits (up to [`BUFFER`], then the command waits
//! too) until the asker's session comes back on a new link, for as long as the run's linger allows.
//! A detached run ([`wire::DETACHED`]) waits for no one: with [`BUFFER`] held and no link, or nothing
//! taken for [`GIVE_WAY`], its oldest output gives way, counted as lost.

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
/// A detached run's output held at [`BUFFER`] with a link up but none of it taken this long (an
/// asker from before detached runs were followed, or a stalled one) gives way, as with no link.
const GIVE_WAY: Duration = Duration::from_secs(10);
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
/// A question for the server an agent's tools asked ([`Handle::query`]) is given up after this
/// (`agents.wait` waits up to 10 minutes).
const QUERY_TIMEOUT: Duration = Duration::from_secs(660);

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
    /// Who takes over what another runner hands this one (an agent session moving here); None:
    /// hand-overs are refused.
    pub adopt: Option<async_channel::Sender<Adoption>>,
    /// The copies' uses, for their keeper (`super::keeper`, docs/reach.md §5.1): each sync and run
    /// holds its copy in use while it's at it.
    pub using: Arc<super::keeper::Using>,
    /// The cache whose objects this runner's peers may ask for (docs/ci.md W24-R27); `None`, the
    /// running cache service's.
    pub objects: Option<Arc<crate::runner::work::store::Store>>,
    /// The highest reach version it speaks ([`wire::VERSION`]; a test's 1 stands in for a build from
    /// before, which carries no custody records).
    pub version: u32,
    /// kiln's root (its CA, DER), which the custody records another runner hands this one chain to
    /// (docs/ci.md W27-R08, `super::custody`).
    pub root: Vec<u8>,
}

/// Something another runner hands this one to take over, whole: `reply` says whether it was.
pub struct Adoption {
    /// The runner it comes from.
    pub from: String,
    pub data: Vec<u8>,
    pub reply: oneshot::Sender<Result<(), String>>,
}

impl std::fmt::Debug for Adoption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Adoption").field("from", &self.from).field("bytes", &self.data.len()).finish()
    }
}

/// The most a hand-over may carry.
pub const ADOPT_MAX: usize = 512 << 20;

/// What begins a runner's refusal of a hand-over it looked at and didn't take (`Host::adopt`, and
/// this node's own refusals of one it never had all of): the session isn't there. Any other refusal
/// (from a runner from before it) can't say so after a try whose answer was lost.
pub const NOT_TAKEN: &str = "not taken: ";
/// A hand-over a runner had all of, dropped before it said how taking it over went (it stopped).
pub const DROPPED: &str = "it was dropped before it was taken over";

/// How handing a session to another runner went, short of its taking it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotAdopted {
    /// It isn't there: refused, or not all of it went.
    Refused(String),
    /// All of it went and no answer came (the link closed, or the runner took too long): it may run
    /// there now.
    Unknown(String),
}

impl std::fmt::Display for NotAdopted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) => f.write_str(why),
            Self::Unknown(why) => write!(f, "no answer came, and it may run there now: {why}"),
        }
    }
}

/// How one try at a hand-over fell short.
enum Missed {
    /// The link closed before all of it went (tried again).
    Interrupted(String),
    /// It couldn't be sent: no link, or the runner stopped taking it on the way.
    Failed(String),
    /// All of it went, and no answer came.
    Unanswered(String),
    /// The runner said why it can't take it.
    Refused(String),
}
/// Bytes of a hand-over per message.
const ADOPT_PIECE: usize = 1 << 20;

impl Config {
    /// A runner's usual settings: identity under `state`, copies under `~/.kiln/reach`.
    pub fn for_runner(name: &str, state: &std::path::Path, accept: bool) -> Self {
        Self { name: name.to_owned(), dir: state.join("reach"), accept, mirrors: crate::os::home_dir().join(".kiln").join("reach"), adopt: None, using: Arc::default(), objects: None, version: wire::VERSION, root: Vec::new() }
    }
}

/// What follows a run on another runner receives.
pub type Says = async_channel::Sender<Say>;

/// Messages into the node.
pub enum In {
    /// A frame of a link, relayed by the server ([`crate::proto::ServerMsg::Reach`]).
    Frame { from: String, link: String, open: bool, data: Vec<u8>, close: Option<String> },
    /// The server's answer to a question ([`crate::proto::ServerMsg::ReachAnswer`]).
    Answer { req: u64, answer: Value },
    /// The connection to the server came up (true) or went (false).
    Server(bool),
    /// Whether the server takes word of a message a link couldn't read (it says
    /// [`crate::proto::CAP_UNREAD`]: [`crate::proto::AgentMsg::LinkUnread`]).
    Unread(bool),
    Local(Local),
    /// An event of a run this node does for another runner.
    Ran { key: (String, String), id: u64, ev: serve::Event },
    /// What a sync this node does for another runner says: with its commit, the custody frame that
    /// goes ahead of it and the copy's tree digest (docs/ci.md W27-R08).
    Synced { key: (String, String), say: SyncSay, custody: Option<String>, tree: Option<String> },
    /// Whether this runner took over what another handed it.
    Adopted { key: (String, String), id: u64, error: Option<String> },
    Tick,
}

/// What local tools ask of the node.
pub enum Local {
    Directory { reply: oneshot::Sender<anyhow::Result<Vec<RunnerInfo>>> },
    /// Link up to `peer` (if it is not), answering with the machine it is.
    Connect { peer: String, reply: oneshot::Sender<anyhow::Result<Host>> },
    /// Run on `peer`; what the run says goes to `says`. Answers with its id: `spec`'s when it has
    /// one ([`Local::Reserve`]), its custody frames (`custody`, JSON) going ahead of it.
    Run { peer: String, spec: Box<RunSpec>, says: Says, reply: oneshot::Sender<u64>, custody: Vec<String> },
    /// The id the next run on `peer` gets, and the session it's numbered in (what its custody
    /// records name it by, docs/ci.md W27-R08).
    Reserve { peer: String, reply: oneshot::Sender<(u64, String)> },
    /// The version of the link up to `peer` (None: none is up), and the receipt of sync `key`'s
    /// workspace it said, taken.
    Custody { peer: String, key: String, reply: oneshot::Sender<(Option<u32>, Option<String>)> },
    /// Something more for a run (input, a signal, Forget).
    Ask { peer: String, ask: Ask },
    /// Follow a sync of `key` on `peer` (what it says goes to `says`), then send it `asks`.
    Sync { peer: String, key: String, says: async_channel::Sender<SyncSay> },
    SyncAsk { peer: String, ask: SyncAsk },
    /// Stop following a run or a sync.
    Leave { peer: String, run: Option<u64>, sync: Option<String> },
    /// A detached run's client went: the run is followed on alone ([`Follow::detached`]).
    Detach { peer: String, run: u64 },
    /// A test's: `bytes` as they are, on this node's link to `peer`, as if a message.
    #[cfg(test)]
    Garble { peer: String, bytes: Vec<u8> },
    Status { reply: oneshot::Sender<Value> },
    /// Forget a runner's pinned key (it was reinstalled).
    Forget { runner: String, reply: oneshot::Sender<bool> },
    /// Follow a hand-over to `peer` (what it says goes to `says`); answers with its id.
    AdoptFollow { peer: String, says: Says, reply: oneshot::Sender<u64> },
    /// A question for the server (JSON), answered with its answer (JSON).
    Query { json: String, reply: oneshot::Sender<anyhow::Result<String>> },
}

/// How the agent and local tools reach the node. What the network and the node's own runs say
/// goes in unbounded (the agent never waits on the node: windows bound what is in flight); local
/// tools wait their turn.
#[derive(Clone)]
pub struct Handle {
    net: async_channel::Sender<In>,
    tx: async_channel::Sender<In>,
    announce: Announce,
    busy: Arc<AtomicU64>,
    name: String,
    /// Where its identity and pins live, and the hashes of the trees it syncs.
    dir: PathBuf,
    /// kiln's root, for what other runners sign (docs/ci.md W27-R08).
    root: Arc<Vec<u8>>,
}

static GLOBAL: std::sync::OnceLock<Handle> = std::sync::OnceLock::new();

/// This process's node, for what runs here without a handle passed to it (agent sessions moving).
pub fn set_global(h: Handle) {
    let _ = GLOBAL.set(h);
}

pub fn global() -> Option<&'static Handle> {
    GLOBAL.get()
}

impl Handle {
    /// The frame announcing this runner's reach identity, for every (re)connect.
    pub fn hello(&self) -> AgentMsg {
        AgentMsg::ReachHello { json: serde_json::to_string(&self.announce).unwrap_or_default() }
    }

    pub fn fingerprint(&self) -> &str {
        &self.announce.fingerprint
    }

    /// This runner's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs under way: those this runner does for others and those it follows elsewhere. A
    /// restart or an in-place upgrade loses them, so it waits while there are any.
    pub fn busy(&self) -> usize {
        usize::try_from(self.busy.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    /// From the agent: a frame, an answer, the server coming or going. Never waits.
    pub async fn feed(&self, m: In) -> bool {
        self.net.try_send(m).is_ok()
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
        self.run_with(peer, spec, says, Vec::new()).await
    }

    /// [`Handle::run`], its custody frames (JSON) going ahead of it on the link (docs/ci.md
    /// W27-R08), its id `spec`'s when it has one ([`Handle::reserve`]).
    pub async fn run_with(&self, peer: &str, spec: RunSpec, says: Says, custody: Vec<String>) -> anyhow::Result<u64> {
        self.ask(|reply| Local::Run { peer: peer.to_owned(), spec: Box::new(spec), says, reply, custody }).await
    }

    /// The id the next run on `peer` gets, and the reach session it's numbered in.
    pub async fn reserve(&self, peer: &str) -> anyhow::Result<(u64, String)> {
        self.ask(|reply| Local::Reserve { peer: peer.to_owned(), reply }).await
    }

    /// The version the link up to `peer` runs at (None: none is up), and the receipt of sync `key`'s
    /// workspace `peer` said on it, taken.
    pub async fn custody(&self, peer: &str, key: &str) -> anyhow::Result<(Option<u32>, Option<String>)> {
        self.ask(|reply| Local::Custody { peer: peer.to_owned(), key: key.to_owned(), reply }).await
    }

    /// Where this runner keeps the hashes of a tree it syncs as copy `key` (`super::sync::Hashes`).
    pub fn hashes_of(&self, key: &str) -> PathBuf {
        self.dir.join("trees").join(format!("{key}.json"))
    }

    /// kiln's root, which what other runners sign chains to (empty: none is checked).
    pub fn root(&self) -> &[u8] {
        &self.root
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

    /// The client of `run`, a detached run on `peer`, went (docs/reach.md §4.2): it's followed on
    /// without one until it ends.
    pub async fn detach(&self, peer: &str, run: u64) {
        let _ = self.tx.send(In::Local(Local::Detach { peer: peer.to_owned(), run })).await;
    }

    /// A test's: `bytes` sent as they are on this node's link to `peer`, where a message would go.
    #[cfg(test)]
    pub async fn garble(&self, peer: &str, bytes: Vec<u8>) {
        let _ = self.tx.send(In::Local(Local::Garble { peer: peer.to_owned(), bytes })).await;
    }

    pub async fn status(&self) -> anyhow::Result<Value> {
        self.ask(|reply| Local::Status { reply }).await
    }

    pub async fn forget(&self, runner: &str) -> anyhow::Result<bool> {
        self.ask(|reply| Local::Forget { runner: runner.to_owned(), reply }).await
    }

    /// Ask the kiln server `json` ([`crate::proto::AgentMsg::ReachAsk`]: an agent's MCP server
    /// asking for kiln's operations, `crate::mcp::runner_ask`), and wait for its answer. An agent
    /// session's act goes signed by the session this process holds (docs/ci.md W27-R08).
    pub async fn query(&self, json: String) -> anyhow::Result<String> {
        let json = crate::runner::custody::signed_ask(json);
        let sent = json.clone();
        let answer = match self.ask(|reply| Local::Query { json, reply }).await {
            Ok(a) => a,
            Err(e) => Err(e),
        };
        // Unanswered, a session's act wasn't done: kiln hears of it once it's back.
        if let Err(e) = &answer {
            crate::runner::custody::unanswered(&sent, &format!("{e:#}"));
        }
        answer
    }

    /// Hand `data` to `peer` to take over (an agent session moving there), and wait until it has,
    /// or says why it can't. A link that drops on the way starts it over (a few times), and so does
    /// one that went all the way without an answer: told then that the session runs there already,
    /// the try before took it over. `again`: a hand-over of it went all the way without an answer
    /// before this one (so told it runs there already, it was taken over). Never an answer, it's
    /// [`NotAdopted::Unknown`]: it may run there.
    pub async fn adopt(&self, peer: &str, data: &[u8], again: bool) -> Result<(), NotAdopted> {
        if data.len() > ADOPT_MAX {
            return Err(NotAdopted::Refused(format!("{} MB is more than a hand-over carries", data.len() >> 20)));
        }
        let (mut last, mut maybe) = (String::new(), again);
        // Short of an answer, what's known: it's there, or it may be.
        let short = |maybe: bool, why: String| if maybe { NotAdopted::Unknown(why) } else { NotAdopted::Refused(why) };
        for attempt in 0..3u64 {
            if attempt > 0 {
                crate::rt::sleep(Duration::from_millis(500 * attempt)).await;
            }
            match self.adopt_once(peer, data).await {
                Ok(()) => return Ok(()),
                // It looked, and it isn't there (it answers a hand-over it took before as taken,
                // whatever became of the session since).
                Err(Missed::Refused(why)) if why.starts_with(NOT_TAKEN) => return Err(NotAdopted::Refused(why.strip_prefix(NOT_TAKEN).unwrap_or(&why).to_owned())),
                // A runner from before NOT_TAKEN, after a try whose answer was lost: it runs the
                // session, so that try took it over; refused for anything else, it may have too
                // (since gone on, or ended there).
                Err(Missed::Refused(why)) if maybe && why == crate::harness::host::RUNS_HERE => return Ok(()),
                Err(Missed::Refused(why)) if maybe => return Err(NotAdopted::Unknown(why)),
                Err(Missed::Refused(why)) => return Err(NotAdopted::Refused(why)),
                Err(Missed::Unanswered(why)) => {
                    maybe = true;
                    last = why;
                }
                Err(Missed::Interrupted(why)) => last = why,
                Err(Missed::Failed(why)) => return Err(short(maybe, why)),
            }
        }
        Err(short(maybe, last))
    }

    /// `digest` for `repo` from `peer`'s cache (docs/ci.md W24-R27), written to `into`, a window at
    /// a time: whether `peer` had it. What it sent is checked by its digest by the caller.
    pub async fn object(&self, peer: &str, repo: &str, digest: &str, into: &std::path::Path) -> anyhow::Result<bool> {
        use std::io::Write as _;
        self.connect(peer).await?;
        let (tx, rx) = async_channel::unbounded::<Say>();
        let id = self.ask(|reply| Local::AdoptFollow { peer: peer.to_owned(), says: tx, reply }).await?;
        let mut file = std::fs::File::create(into)?;
        let mut offset = 0u64;
        loop {
            self.send(peer, Ask::Object { id, repo: repo.to_owned(), digest: digest.to_owned(), offset }).await;
            let end = offset.saturating_add(wire::WINDOW);
            while offset < end {
                match crate::rt::timeout(Duration::from_secs(60), rx.recv()).await {
                    Ok(Ok(Say::ObjectData { offset: at, data, last, .. })) => {
                        anyhow::ensure!(at == offset, "{peer} sent {digest} out of order");
                        file.write_all(&data)?;
                        offset = offset.saturating_add(data.len() as u64);
                        if last {
                            file.flush()?;
                            return Ok(true);
                        }
                    }
                    Ok(Ok(Say::ObjectMissing { .. })) => return Ok(false),
                    Ok(Ok(_)) => {}
                    Ok(Err(_)) => anyhow::bail!("interrupted: the link to {peer} closed"),
                    Err(_) => anyhow::bail!("{peer} did not answer in 60s"),
                }
            }
        }
    }

    async fn adopt_once(&self, peer: &str, data: &[u8]) -> Result<(), Missed> {
        self.connect(peer).await.map_err(|e| Missed::Failed(format!("{e:#}")))?;
        let (tx, rx) = async_channel::unbounded::<Say>();
        let id = self.ask(|reply| Local::AdoptFollow { peer: peer.to_owned(), says: tx, reply }).await.map_err(|e| Missed::Failed(format!("{e:#}")))?;
        let wait = |d: Duration| {
            let rx = rx.clone();
            async move {
                match crate::rt::timeout(d, rx.recv()).await {
                    Ok(Ok(s)) => Ok(s),
                    Ok(Err(_)) => Err("interrupted: the link closed".to_owned()),
                    Err(_) => Err(format!("{peer} did not answer in {}s", d.as_secs())),
                }
            }
        };
        let pieces: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(ADOPT_PIECE).collect() };
        let n = pieces.len();
        let (mut sent, mut got) = (0u64, 0u64);
        for (i, piece) in pieces.into_iter().enumerate() {
            let last = i + 1 == n;
            self.send(peer, Ask::Adopt { id, offset: sent, data: piece.to_vec(), last }).await;
            sent += piece.len() as u64;
            while !last && sent.saturating_sub(got) > wire::WINDOW {
                let said = wait(Duration::from_secs(60)).await.map_err(|e| if e.starts_with("interrupted") { Missed::Interrupted(e) } else { Missed::Failed(e) })?;
                match said {
                    Say::AdoptGot { bytes, .. } => got = bytes,
                    // The link lost on the way (said as it's lost, `interrupted: ...`).
                    Say::Adopted { error: Some(e), .. } if e.starts_with("interrupted") => return Err(Missed::Interrupted(e)),
                    Say::Adopted { error, .. } => return Err(Missed::Refused(error.unwrap_or_else(|| "taken over before it all arrived".into()))),
                    _ => {}
                }
            }
        }
        // All of it went. Taking it over starts the session over there: give it time.
        loop {
            match wait(Duration::from_secs(180)).await.map_err(Missed::Unanswered)? {
                Say::Adopted { error: None, .. } => return Ok(()),
                // The link lost before the answer came, or the runner stopped before it said.
                Say::Adopted { error: Some(e), .. } if e.starts_with("interrupted") || e == DROPPED => return Err(Missed::Unanswered(e)),
                Say::Adopted { error: Some(e), .. } => return Err(Missed::Refused(e)),
                _ => {}
            }
        }
    }
}

/// Start the node. Returns its handle and what it sends to the server (the agent forwards it on
/// [`crate::proto::LANE_REACH`]).
pub fn start(cfg: Config) -> anyhow::Result<(Handle, async_channel::Receiver<AgentMsg>)> {
    let identity = Identity::load_or_create(&cfg.dir, &cfg.name)?;
    let pins = Pins::open(&cfg.dir);
    let announce = Announce { fingerprint: identity.fingerprint.clone(), accept: cfg.accept, version: cfg.version };
    let (dir, root) = (cfg.dir.clone(), Arc::new(cfg.root.clone()));
    let (net_tx, net_rx) = async_channel::unbounded::<In>();
    let (tx, rx) = async_channel::bounded::<In>(1024);
    // Unbounded too: the node must never wait on the agent while the agent feeds it.
    let (out_tx, out_rx) = async_channel::unbounded::<AgentMsg>();
    let ticker = net_tx.clone();
    crate::rt::spawn(async move {
        loop {
            crate::rt::sleep(Duration::from_secs(1)).await;
            if ticker.send(In::Tick).await.is_err() {
                return;
            }
        }
    });
    let node_name = cfg.name.clone();
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
        tx: net_tx.clone(),
        dirty: Vec::new(),
        server: false,
        server_unread: false,
        directory: None,
        asked: HashMap::new(),
        queries: HashMap::new(),
        next_req: 1,
        waiting: Vec::new(),
        links: HashMap::new(),
        peers: HashMap::new(),
        served: HashMap::new(),
        root: Arc::clone(&root),
    };
    let busy = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&busy);
    // Kept open by the node itself, so local tools going away never ends it.
    let keep = tx.clone();
    crate::rt::spawn(async move {
        let _keep = keep;
        let mut node = node;
        loop {
            let first = futures::select_biased! {
                m = net_rx.recv().fuse() => m,
                m = rx.recv().fuse() => m,
            };
            let Ok(first) = first else { return };
            // Whatever else is waiting, then everything said goes out together: many messages per
            // frame, not a frame per message.
            let mut batch = vec![first];
            while batch.len() < 512 {
                match net_rx.try_recv().or_else(|_| rx.try_recv()) {
                    Ok(m) => batch.push(m),
                    Err(_) => break,
                }
            }
            for m in batch {
                let r = std::panic::AssertUnwindSafe(node.on(m)).catch_unwind().await;
                if r.is_err() {
                    crate::metrics::global().panics_caught.inc();
                    tracing::error!("reach node handler panicked; carrying on");
                }
            }
            node.flush().await;
            counted.store(node.busy(), Ordering::Relaxed);
        }
    });
    Ok((Handle { net: net_tx, tx, announce, busy, name: node_name, dir, root }, out_rx))
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
    /// The reach version it runs at: the asker's Hello's, once the doer speaks it.
    version: u32,
    /// The asker heard the doer's Welcome.
    welcomed: bool,
    /// Custody frames that came on it (docs/ci.md W27-R08): none ever on a link below
    /// [`wire::CUSTODY`].
    custody: u64,
}

/// A run this side follows on another runner.
struct Follow {
    says: Says,
    /// Output offset received up to.
    got: u64,
    exited: bool,
    /// Its output's SHA-256 so far: each byte once, as it first came (docs/ci.md W27-R08).
    out: sha2::Sha256,
    /// Its custody frames, said just ahead of it on whichever link takes it (one at version 2 or
    /// later).
    frames: Vec<String>,
    /// Its client went and it said `--detach` (docs/reach.md §4.2): its output is taken and let go,
    /// so it never waits on this side, and its peer has work, so a link that drops is made again;
    /// it isn't a run an upgrade waits for.
    detached: bool,
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
    /// Hand-overs to this runner being followed, by id.
    adopts: HashMap<u64, Says>,
    /// Asks waiting for the link.
    queue: VecDeque<Ask>,
    next_id: u64,
    host: Option<Host>,
    waiters: Vec<oneshot::Sender<anyhow::Result<Host>>>,
    last_used: Instant,
    error: Option<String>,
    /// The version it speaks, as its refusal of a later one said (a runner from before).
    speaks: Option<u32>,
    /// The receipts of syncs' workspaces it said, by sync key, for their syncs to take.
    receipts: HashMap<String, String>,
    /// Custody frames that came from it, on any link.
    custody: u64,
}

impl Peer {
    fn new() -> Self {
        Self {
            session: crate::util::new_id(),
            link: None,
            state: PeerState::Idle,
            follows: HashMap::new(),
            syncs: HashMap::new(),
            adopts: HashMap::new(),
            queue: VecDeque::new(),
            next_id: 1,
            host: None,
            waiters: Vec::new(),
            last_used: Instant::now(),
            error: None,
            speaks: None,
            receipts: HashMap::new(),
            custody: 0,
        }
    }

    fn busy(&self) -> bool {
        !self.follows.is_empty() || !self.syncs.is_empty() || !self.adopts.is_empty() || !self.queue.is_empty() || !self.waiters.is_empty()
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
    /// Its output's SHA-256 so far, each byte as the run said it (docs/ci.md W27-R08).
    out: sha2::Sha256,
    /// What it was taken as (its receipt, whom its output goes to), on a link that carries
    /// custody; and the frame handing its output on, once it's made (sent again with its exit).
    took: Option<super::custody::Took>,
    custody: bool,
    out_frame: Option<String>,
    /// Its asker may never come back for it ([`wire::DETACHED`]): held output gives way rather
    /// than make it wait.
    detached: bool,
    /// When its asker last took output (or its session came back on a link).
    taken: Instant,
    /// Output that gave way unread: what its exit says was lost.
    dropped: u64,
}

impl Served {
    /// A detached run's held output gives way, oldest first, down to half of [`BUFFER`], and its
    /// command goes on: what wasn't taken counts as lost, and an asker that comes back hears of it
    /// by the jump in offsets.
    fn give_way(&mut self) {
        while self.held.load(Ordering::Relaxed) > BUFFER / 2 {
            let Some((off, _, d)) = self.buf.pop_front() else { break };
            let end = off + d.len() as u64;
            self.held.fetch_sub(d.len() as u64, Ordering::Relaxed);
            self.dropped += end.saturating_sub(self.acked.max(off));
            self.acked = self.acked.max(end);
            self.sent = self.sent.max(end);
        }
        self.room.notify(usize::MAX);
    }
}

/// A session another runner holds with this one.
struct Session {
    link: Option<String>,
    runs: HashMap<u64, Served>,
    /// Syncs, each its own worker (file system work stays off this task).
    syncs: HashMap<String, async_channel::Sender<(SyncAsk, Option<Commit>)>>,
    /// Hand-overs arriving, by id: the bytes so far.
    adopting: HashMap<u64, Vec<u8>>,
    detached: Option<Instant>,
    /// Custody frames the asker sent ahead of what they hand on (docs/ci.md W27-R08), by what for:
    /// `workspace <key>`, `run <id>`.
    custody: HashMap<String, Value>,
    /// Each copy's tree digest as its last sync committed it, by its key: what its runs are of.
    trees: HashMap<String, String>,
    /// Custody frames that came from it.
    frames: u64,
}

/// What a sync's commit takes custody under (docs/ci.md W27-R08): the version of the link it came
/// on, the frame the asker sent ahead of it, and kiln's root.
struct Commit {
    version: u32,
    frame: Option<Value>,
    root: Arc<Vec<u8>>,
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
    /// Links with ciphertext waiting, flushed after each batch.
    dirty: Vec<String>,
    server: bool,
    /// The server takes word of a message a link couldn't read ([`In::Unread`]).
    server_unread: bool,
    directory: Option<(Instant, Vec<RunnerInfo>)>,
    asked: HashMap<u64, Instant>,
    /// Questions local tools asked the server, waiting for its answers.
    queries: HashMap<u64, (Instant, oneshot::Sender<anyhow::Result<String>>)>,
    next_req: u64,
    waiting: Vec<(Instant, Waiting)>,
    links: HashMap<String, Link>,
    peers: HashMap<String, Peer>,
    served: HashMap<(String, String), Session>,
    /// kiln's root, as `cfg.root` (what other runners' custody records chain to).
    root: Arc<Vec<u8>>,
}

impl Node {
    async fn on(&mut self, m: In) {
        match m {
            In::Frame { from, link, open, data, close } => {
                // Each frame kiln's relay passed on, once, as it came: its run's record is answered
                // with it (docs/ci.md W27-R08's relays). Not in on_frame: a link's first frames go
                // through it again as it's accepted.
                crate::runner::custody::reach_frame(&link, &data);
                self.on_frame(from, link, open, data, close).await;
            }
            In::Answer { req, answer } => self.on_answer(req, answer).await,
            In::Unread(takes) => self.server_unread = takes,
            In::Server(up) => {
                self.server = up;
                // The calls this process passes on to kiln are numbered afresh for each connection
                // (docs/ci.md W27-R08's relays).
                if up {
                    crate::runner::custody::calls_connected();
                }
                // It says again what it takes once it's back.
                self.server_unread &= up;
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
                    for (_, (_, reply)) in std::mem::take(&mut self.queries) {
                        let _ = reply.send(Err(anyhow::anyhow!("the connection to the kiln server dropped before it answered")));
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
            In::Synced { key, say, custody, tree } => self.on_synced(key, say, custody, tree).await,
            In::Adopted { key, id, error } => {
                if let Some(link) = self.served.get(&key).and_then(|s| s.link.clone()) {
                    self.say_on(&link, &Say::Adopted { id, error }).await;
                }
            }
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

    async fn on_answer(&mut self, req: u64, answer: Value) {
        if let Some((_, reply)) = self.queries.remove(&req) {
            let _ = reply.send(Ok(answer.to_string()));
            return;
        }
        if self.asked.remove(&req).is_none() {
            return;
        }
        let runners: Vec<RunnerInfo> = answer.get("runners").cloned().and_then(|r| serde_json::from_value(r).ok()).unwrap_or_default();
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
        let bytes = match wire::frame(msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(link = id, error = %format!("{e:#}"), "a reach message could not be encoded");
                return false;
            }
        };
        let Some(l) = self.links.get_mut(id) else { return false };
        if let Err(e) = l.tls.send(&bytes) {
            // Never lose a message quietly: the link goes, and its session resumes on a new one.
            let why = format!("{e:#}");
            tracing::warn!(link = id, error = %why, "reach link could not take a message");
            self.drop_link(id, &why, true).await;
            return false;
        }
        self.mark(id);
        true
    }

    /// The link has ciphertext to send (at the end of the batch).
    fn mark(&mut self, id: &str) {
        if !self.dirty.iter().any(|d| d == id) {
            self.dirty.push(id.to_owned());
        }
    }

    /// Send what every link has waiting.
    async fn flush(&mut self) {
        for id in std::mem::take(&mut self.dirty) {
            self.send_frames(&id, None).await;
        }
    }

    /// A message on link `id` this runner couldn't read: said to the other end (`Unread`: a runner
    /// from before, which can't read that either, ends the link as on any garbage), and the link
    /// ends; the server hears of it, naming the runner that sent it, when it takes word of it.
    async fn unread_on(&mut self, id: &str, role: Role, e: &crate::proto::Unreadable) {
        let of = match role {
            Role::Asker => "Say",
            Role::Doer => "Ask",
        };
        let (what, code, why) = (e.what(), e.code(), e.why(of));
        let from = self.links.get(id).map(|l| l.peer.clone()).unwrap_or_default();
        tracing::warn!(peer = %from, link = id, %what, code = code.name(), %why, "a reach message couldn't be read: the link ends");
        let said = match role {
            Role::Asker => self.say_on(id, &Ask::Unread { what: what.clone(), code, why: why.clone() }).await,
            Role::Doer => self.say_on(id, &Say::Unread { what: what.clone(), code, why: why.clone() }).await,
        };
        if self.server_unread {
            self.send_out(AgentMsg::LinkUnread { from, link: id.to_owned(), what: what.clone(), code, why: why.clone() }).await;
        }
        if said {
            self.drop_link(id, &format!("{of} {what} couldn't be read ({}): {why}", code.name()), true).await;
        }
    }

    async fn drop_link(&mut self, id: &str, why: &str, tell: bool) {
        if tell && self.links.contains_key(id) {
            if let Some(l) = self.links.get_mut(id) {
                l.tls.close();
            }
            self.send_frames(id, Some(why.to_owned())).await;
        }
        let Some(l) = self.links.remove(id) else { return };
        crate::runner::custody::reach_closed(id);
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
                    for (id, s) in p.adopts.drain() {
                        let _ = s.try_send(Say::Adopted { id, error: Some(format!("interrupted: {why}")) });
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
        // Whatever the handshake left to say (its Finished, the held first message) goes with the
        // batch.
        self.mark(&link);
        loop {
            let Some(l) = self.links.get_mut(&link) else { return };
            let role = l.role;
            let next = match role {
                Role::Asker => l.de.read::<Say>(wire::SAY_VARIANTS).map(|m| m.map(Msg::Say)),
                Role::Doer => l.de.read::<Ask>(wire::ASK_VARIANTS).map(|m| m.map(Msg::Ask)),
            };
            match next {
                Ok(Some(Msg::Say(s))) => self.on_say(&link, s).await,
                Ok(Some(Msg::Ask(a))) => self.on_ask(&link, a).await,
                Ok(None) => break,
                Err(e) => {
                    self.unread_on(&link, role, &e).await;
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
        // At the version both speak: the lower of this runner's and what the directory, or the
        // other's refusal of a later one, says it speaks.
        let listed = self.directory.as_ref().and_then(|(_, d)| d.iter().find(|r| r.name == name)).and_then(|r| r.reach_version);
        let Some(p) = self.peers.get_mut(name) else { return };
        let version = [Some(self.cfg.version), listed, p.speaks].into_iter().flatten().min().unwrap_or(wire::MIN_VERSION).max(wire::MIN_VERSION);
        let id = crate::util::new_id();
        let resume: Vec<Resume> = p.follows.iter().map(|(id, f)| Resume { id: *id, from: f.got }).collect();
        let hello = Ask::Hello { version, session: p.session.clone(), resume };
        p.link = Some(id.clone());
        p.state = PeerState::Connecting;
        let now = Instant::now();
        let link = Link { peer: name.to_owned(), role: Role::Asker, tls, de: Deframer::default(), fingerprint: fp, opened: now, last_rx: now, sent_any: false, pinned: false, session: None, pinged: false, version, welcomed: false, custody: 0 };
        self.links.insert(id.clone(), link);
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
            // Said as a link lost: nothing there said no.
            for (id, s) in p.adopts.drain() {
                let _ = s.send(Say::Adopted { id, error: Some(format!("interrupted: {why}")) }).await;
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
            // A link below CUSTODY carries no custody frames: a runner from before would end it. A
            // run's go just ahead of it, so a run asked again on a new link has them too.
            let carries = self.links.get(&link).is_some_and(|l| l.version >= wire::CUSTODY);
            if matches!(a, Ask::Custody { .. }) && !carries {
                continue;
            }
            let frames = match &a {
                Ask::Run(spec) if carries => p.follows.get(&spec.id).map(|f| f.frames.clone()).unwrap_or_default(),
                _ => Vec::new(),
            };
            for json in frames {
                self.say_on(&link, &Ask::Custody { json }).await;
            }
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
                let asked = self.links.get(link).map_or(wire::VERSION, |l| l.version);
                if version != asked {
                    self.drop_link(link, &format!("{name} answered at reach version {version}, not the {asked} this runner asked at"), true).await;
                    return;
                }
                if let Some(l) = self.links.get_mut(link) {
                    l.welcomed = true;
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
                // A runner from before speaks only an older version, and refuses a Hello at a later
                // one: asked again at its own, on a new link at once (only Hello went on this one).
                let (asked, welcomed) = self.links.get(link).map_or((wire::MIN_VERSION, true), |l| (l.version, l.welcomed));
                if !welcomed && asked > wire::MIN_VERSION {
                    let speaks = spoken(&reason).filter(|v| (wire::MIN_VERSION..asked).contains(v)).unwrap_or(wire::MIN_VERSION);
                    tracing::info!(peer = %name, asked, speaks, "a runner speaks an older reach version: linking again at its own");
                    if let Some(l) = self.links.get_mut(link) {
                        l.tls.close();
                    }
                    self.send_frames(link, Some(reason.clone())).await;
                    self.links.remove(link);
                    crate::runner::custody::reach_closed(link);
                    if let Some(p) = self.peers.get_mut(&name) {
                        p.speaks = Some(speaks);
                        p.link = None;
                        p.state = PeerState::Idle;
                    }
                    self.connect_resolved(&name).await;
                    return;
                }
                self.fail_peer(&name, &reason).await;
                self.drop_link(link, &reason, true).await;
            }
            Say::Custody { json } => {
                if !self.custody_on(link).await {
                    return;
                }
                let Some(p) = self.peers.get_mut(&name) else { return };
                p.custody += 1;
                match super::custody::frame_of(&json) {
                    Some((kind, _, key)) if kind == "workspace" => {
                        p.receipts.insert(key, json);
                    }
                    Some((_, _, id)) => {
                        if let Some(f) = id.parse::<u64>().ok().and_then(|id| p.follows.get(&id)) {
                            let _ = f.says.send(Say::Custody { json }).await;
                        }
                    }
                    None => tracing::debug!(peer = %name, "an unreadable custody frame"),
                }
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
                {
                    use sha2::Digest as _;
                    f.out.update(&data);
                }
                // A detached run followed on alone: taken and let go, nobody here reads it.
                if !f.detached {
                    let says = f.says.clone();
                    if lost > 0 {
                        let note = format!("\n[kiln reach: {lost} bytes of output were dropped]\n").into_bytes();
                        let _ = says.send(Say::Output { id, offset: start, stream: Stream::Err, data: note }).await;
                    }
                    let _ = says.send(Say::Output { id, offset: start, stream, data }).await;
                }
                self.say_on(link, &Ask::Ack { id, upto: end }).await;
            }
            Say::Exit { id, code, signal, error, end, lost } => {
                let Some(p) = self.peers.get_mut(&name) else { return };
                let Some(f) = p.follows.get_mut(&id) else { return };
                // On one link output always comes before the exit; a shortfall is output lost.
                let lost = lost + end.saturating_sub(f.got);
                f.exited = true;
                // What arrived of its output, by its SHA-256, for its follower's receipt of it
                // (docs/ci.md W27-R08): said just before its exit, on this side only.
                let got = {
                    use sha2::Digest as _;
                    hex::encode(std::mem::take(&mut f.out).finalize())
                };
                let _ = f.says.send(Say::Custody { json: json!({ "output_got": id, "sha256": got }).to_string() }).await;
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
            Say::AdoptGot { id, .. } | Say::Adopted { id, .. } | Say::ObjectData { id, .. } | Say::ObjectMissing { id } => {
                let done = matches!(s, Say::Adopted { .. } | Say::ObjectData { last: true, .. } | Say::ObjectMissing { .. });
                if let Some(p) = self.peers.get_mut(&name) {
                    if let Some(tx) = p.adopts.get(&id) {
                        let _ = tx.send(s).await;
                    }
                    if done {
                        p.adopts.remove(&id);
                    }
                }
            }
            Say::Pong { .. } => {}
            // The other end couldn't read a message of this one's: it ends the link.
            Say::Unread { what, code, why } => {
                tracing::warn!(peer = %name, link, %what, code = code.name(), %why, "the other runner couldn't read a message of this one's on a reach link");
                self.drop_link(link, &format!("{name} couldn't read {what} ({}): {why}", code.name()), false).await;
            }
        }
    }

    /// A custody frame came on `link` (docs/ci.md W27-R08): counted, if the link runs at a version
    /// that carries them; else the link ends, as a runner from before ends one that says what it
    /// can't read. Whether it's taken.
    async fn custody_on(&mut self, link: &str) -> bool {
        let Some(version) = self.links.get(link).map(|l| l.version) else { return false };
        if version < wire::CUSTODY {
            self.drop_link(link, &format!("a custody frame on a link at reach version {version}"), true).await;
            return false;
        }
        if let Some(l) = self.links.get_mut(link) {
            l.custody += 1;
        }
        true
    }

    /// The version the link up to session `key`'s asker runs at, while one is up.
    fn served_version(&self, key: &(String, String)) -> Option<u32> {
        let link = self.served.get(key)?.link.as_ref()?;
        self.links.get(link).map(|l| l.version)
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
        let l = Link { peer: from.clone(), role: Role::Doer, tls, de: Deframer::default(), fingerprint: fp, opened: now, last_rx: now, sent_any: true, pinned: false, session: None, pinged: false, version: wire::MIN_VERSION, welcomed: false, custody: 0 };
        self.links.insert(link.clone(), l);
        for data in frames {
            Box::pin(self.on_frame(from.clone(), link.clone(), false, data, None)).await;
        }
    }

    async fn on_ask(&mut self, link: &str, a: Ask) {
        let Some((peer, session)) = self.links.get(link).map(|l| (l.peer.clone(), l.session.clone())) else { return };
        if let Ask::Hello { version, session, resume } = a {
            // Any version from the oldest up to this runner's: the link runs at the asker's.
            if !(wire::MIN_VERSION..=self.cfg.version).contains(&version) {
                self.say_on(link, &Say::Refused { reason: format!("{} speaks reach version {}, not {version}", self.cfg.name, self.cfg.version) }).await;
                return;
            }
            let key = (peer.clone(), session.clone());
            if let Some(l) = self.links.get_mut(link) {
                l.session = Some(session.clone());
                l.version = version;
            }
            let s = self.served.entry(key.clone()).or_insert_with(|| Session { link: None, runs: HashMap::new(), syncs: HashMap::new(), adopting: HashMap::new(), detached: None, custody: HashMap::new(), trees: HashMap::new(), frames: 0 });
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
                        run.taken = Instant::now();
                        resumed.push(r.id);
                    }
                    None => lost.push(r.id),
                }
            }
            if let Some(old) = old.filter(|o| o != link) {
                self.drop_link(&old, "replaced by a new link", true).await;
            }
            self.say_on(link, &Say::Welcome { version, host: self.host.clone(), resumed: resumed.clone(), lost }).await;
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
                    if upto.min(r.next) > r.acked {
                        r.taken = Instant::now();
                    }
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
            Ask::Adopt { id, offset, data, last } => self.on_adopt(&key, id, offset, data, last).await,
            Ask::Ping { at } => {
                self.say_on(link, &Say::Pong { at }).await;
            }
            Ask::Object { id, repo, digest, offset } => {
                let store = self.cfg.objects.clone().or_else(|| crate::runner::work::service::running().map(|s| Arc::clone(s.store())));
                let (r, d) = (repo.clone(), digest.clone());
                let window = crate::rt::blocking(move || object_window(store.as_deref(), &r, &d, offset)).await;
                match window {
                    Ok(Ok(Some(mut pieces))) => {
                        // This runner's hand-on of the object to the peer that asked, signed as its
                        // first window goes (docs/ci.md W27-R08): who the peer is, kiln says, asked
                        // apart from this loop.
                        if offset == 0 {
                            let to = key.0.clone();
                            crate::rt::spawn(async move {
                                if let Some(uri) = crate::runner::work::peers::custodian_of(&to).await {
                                    crate::runner::custody::peer_object_handed(&uri, &repo, &digest);
                                }
                            });
                            // A test build's `peer_object_changed`: a byte changed after the hand-on.
                            if crate::runner::chaos::get().is_some_and(|c| c.peer_object_changed)
                                && let Some(b) = pieces.first_mut().and_then(|(_, data, _)| data.first_mut())
                            {
                                *b ^= 0x01;
                            }
                        }
                        for (offset, data, last) in pieces {
                            self.say_on(link, &Say::ObjectData { id, offset, data, last }).await;
                        }
                    }
                    _ => {
                        self.say_on(link, &Say::ObjectMissing { id }).await;
                    }
                }
            }
            // What the asker hands on ahead of a sync's commit or a run (docs/ci.md W27-R08), kept
            // for it.
            Ask::Custody { json } => {
                if !self.custody_on(link).await {
                    return;
                }
                let Some(s) = self.served.get_mut(&key) else { return };
                s.frames += 1;
                match super::custody::frame_of(&json) {
                    Some((kind, v, of)) if kind == "workspace" || kind == "run" => {
                        s.custody.insert(format!("{kind} {of}"), v);
                    }
                    _ => tracing::debug!(peer = %key.0, "an unreadable custody frame"),
                }
            }
            // The asker couldn't read a message of this one's: it ends the link.
            Ask::Unread { what, code, why } => {
                tracing::warn!(peer = %key.0, link, %what, code = code.name(), %why, "the other runner couldn't read a message of this one's on a reach link");
                self.drop_link(link, &format!("{} couldn't read {what} ({}): {why}", key.0, code.name()), false).await;
            }
        }
    }

    async fn start_run(&mut self, key: &(String, String), spec: RunSpec) {
        let id = spec.id;
        let version = self.served_version(key).unwrap_or(wire::MIN_VERSION);
        let Some(s) = self.served.get_mut(key) else { return };
        if s.runs.contains_key(&id) {
            return;
        }
        // Taken as its asker handed it on (docs/ci.md W27-R08): refused, and recorded, if it isn't
        // what came, or comes without its hand-on on a link that carries them.
        let known = spec.mirror.as_deref().map(|m| s.trees.get(m).cloned());
        let tree = known.clone().flatten().unwrap_or_default();
        let frame = s.custody.remove(&format!("run {id}"));
        // A copy no sync of this session committed (this process took over since): nothing here
        // can say what tree the run is of, so it's refused, to sync again.
        if version >= wire::CUSTODY && known.is_some_and(|t| t.is_none()) {
            let why = "its copy wasn't synced to this process: sync it again";
            super::custody::refused(&format!("run {id}"), &key.0, why);
            self.refuse_run(key, id, format!("refused: {why}")).await;
            return;
        }
        let took = if version >= wire::CUSTODY {
            match super::custody::take_run(&self.root, &key.0, &key.1, &spec, &tree, frame.as_ref()) {
                Ok((said, took)) => {
                    if let Some(link) = s.link.clone() {
                        self.say_on(&link, &Say::Custody { json: said }).await;
                    }
                    took
                }
                Err(why) => {
                    super::custody::refused(&format!("run {id}"), &key.0, &why);
                    self.refuse_run(key, id, format!("refused: {why}")).await;
                    return;
                }
            }
        } else {
            super::custody::run_from_before(&key.0, &key.1, &spec, &tree)
        };
        let Some(s) = self.served.get_mut(key) else { return };
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
        // A run in a copy holds it in use until the run's last event, noted as the agent's that
        // asked for it (docs/reach.md §5.1).
        let busy = spec.mirror.as_deref().map(|m| self.cfg.using.busy(&key.0, m, super::keeper::agent_of(&spec.env)));
        let stdin = serve::start(spec.clone(), cwd, shell, extra, ev_tx);
        // Forward the run's events into the node, waiting while its asker has too much unread.
        let (tx, k, h, r) = (self.tx.clone(), key.clone(), Arc::clone(&held), Arc::clone(&room));
        crate::rt::spawn(async move {
            let _busy = busy;
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
                out: sha2::Sha256::default(),
                took,
                custody: version >= wire::CUSTODY,
                out_frame: None,
                detached: spec.linger_ms == Some(wire::DETACHED),
                taken: now,
                dropped: 0,
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
                {
                    use sha2::Digest as _;
                    r.out.update(&data);
                }
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
                let (end, lost) = (r.next, r.dropped);
                // Its output handed back ahead of its exit (docs/ci.md W27-R08), made once and said
                // again with an exit said again; to a runner from before, signed but never said.
                if r.custody || r.took.is_some() {
                    let frame = match &r.out_frame {
                        Some(f) => f.clone(),
                        None => {
                            use sha2::Digest as _;
                            let sha = hex::encode(std::mem::take(&mut r.out).finalize());
                            let f = super::custody::output_hand_on(r.took.as_ref(), id, &sha);
                            r.out_frame = Some(f.clone());
                            f
                        }
                    };
                    if r.custody {
                        self.say_on(&link, &Say::Custody { json: frame }).await;
                    }
                }
                self.say_on(&link, &Say::Exit { id, code, signal: None, error, end, lost }).await;
            }
            return;
        }
    }

    /// A piece of a hand-over: kept until the last, then given whole to whoever takes it over here.
    async fn on_adopt(&mut self, key: &(String, String), id: u64, offset: u64, data: Vec<u8>, last: bool) {
        let Some(s) = self.served.get_mut(key) else { return };
        let Some(link) = s.link.clone() else { return };
        if offset == 0 {
            s.adopting.insert(id, Vec::new());
        }
        let refusal = match s.adopting.get_mut(&id) {
            None => Some(format!("{NOT_TAKEN}a hand-over that never began")),
            Some(buf) if buf.len() as u64 != offset => Some(format!("{NOT_TAKEN}a piece at {offset} where {} was due", buf.len())),
            Some(buf) if buf.len() + data.len() > ADOPT_MAX => Some(format!("{NOT_TAKEN}more than a hand-over carries")),
            Some(buf) => {
                let before = buf.len() >> 20;
                buf.extend_from_slice(&data);
                let now = buf.len();
                if !last && now >> 20 > before {
                    self.say_on(&link, &Say::AdoptGot { id, bytes: now as u64 }).await;
                }
                None
            }
        };
        if let Some(why) = refusal {
            if let Some(s) = self.served.get_mut(key) {
                s.adopting.remove(&id);
            }
            self.say_on(&link, &Say::Adopted { id, error: Some(why) }).await;
            return;
        }
        if !last {
            return;
        }
        let Some(data) = self.served.get_mut(key).and_then(|s| s.adopting.remove(&id)) else { return };
        let Some(adopt) = self.cfg.adopt.clone() else {
            self.say_on(&link, &Say::Adopted { id, error: Some(format!("{NOT_TAKEN}{} doesn't take over agent sessions", self.cfg.name)) }).await;
            return;
        };
        // Taking it over may take a while (a harness starting): off this task.
        let (reply, answer) = oneshot::channel();
        let (tx, key) = (self.tx.clone(), key.clone());
        let from = key.0.clone();
        crate::rt::spawn(async move {
            let error = if adopt.send(Adoption { from, data, reply }).await.is_err() {
                Some(format!("{NOT_TAKEN}nothing here takes it over"))
            } else {
                match answer.await {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e),
                    Err(_) => Some(DROPPED.to_owned()),
                }
            };
            let _ = tx.send(In::Adopted { key, id, error }).await;
        });
    }

    async fn on_sync_ask(&mut self, key: &(String, String), a: SyncAsk) {
        let sync_key = match &a {
            SyncAsk::Plan { key, .. } | SyncAsk::File { key, .. } | SyncAsk::Commit { key } => key.clone(),
        };
        let version = self.served_version(key).unwrap_or(wire::MIN_VERSION);
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
            // The copy is in use while its sync's worker is at it (docs/reach.md §5.1).
            let busy = self.cfg.using.busy(&key.0, &sync_key, None);
            s.syncs.insert(sync_key.clone(), sync_worker(root, sync_key.clone(), key.clone(), self.tx.clone(), busy));
        }
        // A commit takes the copy as the frame its asker sent ahead of it hands it on.
        let commit = matches!(a, SyncAsk::Commit { .. }).then(|| Commit { version, frame: s.custody.remove(&format!("workspace {sync_key}")), root: Arc::clone(&self.root) });
        if let Some(w) = s.syncs.get(&sync_key)
            && w.send((a, commit)).await.is_err()
        {
            s.syncs.remove(&sync_key);
        }
    }

    async fn on_synced(&mut self, key: (String, String), say: SyncSay, custody: Option<String>, tree: Option<String>) {
        let version = self.served_version(&key).unwrap_or(wire::MIN_VERSION);
        let Some(s) = self.served.get_mut(&key) else { return };
        if let SyncSay::Done { key: k, .. } | SyncSay::Failed { key: k, .. } = &say {
            s.syncs.remove(k);
        }
        if let (SyncSay::Done { key: k, .. }, Some(t)) = (&say, tree) {
            s.trees.insert(k.clone(), t);
        }
        if let Some(link) = s.link.clone() {
            if let Some(json) = custody.filter(|_| version >= wire::CUSTODY) {
                self.say_on(&link, &Say::Custody { json }).await;
            }
            self.say_on(&link, &Say::Sync(say)).await;
        }
    }

    // ------------------------------------------------------------------ local tools

    async fn on_local(&mut self, l: Local) {
        match l {
            Local::Query { json, reply } => {
                if !self.server {
                    let _ = reply.send(Err(anyhow::anyhow!("not connected to the kiln server")));
                    return;
                }
                // Its agent's act passed on with this process's record of it, numbered as asks go,
                // in order (docs/ci.md W27-R08's relays).
                let Some(json) = crate::runner::custody::passed_on_ask(json) else {
                    let _ = reply.send(Err(anyhow::anyhow!("dropped on its way to kiln by this runner, a faulty relay (a test build's fault)")));
                    return;
                };
                let req = self.next_req;
                self.next_req += 1;
                self.queries.insert(req, (Instant::now(), reply));
                self.send_out(AgentMsg::ReachAsk { req, json }).await;
            }
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
            Local::Run { peer, mut spec, says, reply, custody } => {
                let p = self.peers.entry(peer.clone()).or_insert_with(Peer::new);
                let id = if spec.id > 0 {
                    spec.id
                } else {
                    p.next_id += 1;
                    p.next_id - 1
                };
                spec.id = id;
                // Its custody frames go just ahead of it, on whichever link takes it (docs/ci.md
                // W27-R08).
                p.follows.insert(id, Follow { says, got: 0, exited: false, out: sha2::Sha256::default(), frames: custody, detached: false });
                let _ = reply.send(id);
                self.send_or_queue(&peer, Ask::Run(spec)).await;
            }
            Local::Reserve { peer, reply } => {
                let p = self.peers.entry(peer).or_insert_with(Peer::new);
                let id = p.next_id;
                p.next_id += 1;
                let _ = reply.send((id, p.session.clone()));
            }
            Local::Custody { peer, key, reply } => {
                let version = self.peers.get(&peer).filter(|p| p.state == PeerState::Ready).and_then(|p| p.link.as_ref()).and_then(|l| self.links.get(l)).map(|l| l.version);
                let receipt = self.peers.get_mut(&peer).and_then(|p| p.receipts.remove(&key));
                let _ = reply.send((version, receipt));
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
            #[cfg(test)]
            Local::Garble { peer, bytes } => {
                let link = self.peers.get(&peer).and_then(|p| p.link.clone());
                if let Some(id) = link
                    && let Some(l) = self.links.get_mut(&id)
                    && l.tls.send(&bytes).is_ok()
                {
                    self.mark(&id);
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
            Local::Detach { peer, run } => {
                if let Some(f) = self.peers.get_mut(&peer).and_then(|p| p.follows.get_mut(&run)) {
                    f.detached = true;
                }
            }
            Local::Status { reply } => {
                let _ = reply.send(self.status());
            }
            Local::Forget { runner, reply } => {
                let _ = reply.send(self.pins.forget(&runner));
            }
            Local::AdoptFollow { peer, says, reply } => {
                let p = self.peers.entry(peer).or_insert_with(Peer::new);
                let id = p.next_id;
                p.next_id += 1;
                p.adopts.insert(id, says);
                let _ = reply.send(id);
            }
        }
    }

    fn busy(&self) -> u64 {
        let doing: usize = self.served.values().map(|s| s.runs.values().filter(|r| r.exit.is_none()).count() + s.syncs.len()).sum();
        // A detached run followed on alone isn't waited for: nobody here reads it, and its runner
        // goes on without this one.
        let following: usize = self.peers.values().map(|p| p.follows.values().filter(|f| !f.detached).count() + p.syncs.len()).sum();
        u64::try_from(doing + following).unwrap_or(u64::MAX)
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
                    "detached": p.follows.values().filter(|f| f.detached).count(),
                    "link": p.link,
                    "syncs": p.syncs.len(),
                    "host": p.host,
                    "error": p.error,
                    "secured": p.link.as_ref().and_then(|l| self.links.get(l)).and_then(|l| l.tls.negotiated()),
                    "version": p.link.as_ref().and_then(|l| self.links.get(l)).map(|l| l.version),
                    "custody": p.custody,
                })
            })
            .collect();
        let serving: Vec<Value> = self
            .served
            .iter()
            .map(|((r, _), s)| json!({"runner": r, "runs": s.runs.len(), "linked": s.link.is_some(), "version": s.link.as_ref().and_then(|l| self.links.get(l)).map(|l| l.version), "custody": s.frames}))
            .collect();
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
        let late: Vec<u64> = self.queries.iter().filter(|(_, (at, _))| at.elapsed() >= QUERY_TIMEOUT).map(|(r, _)| *r).collect();
        for r in late {
            if let Some((_, reply)) = self.queries.remove(&r) {
                let _ = reply.send(Err(anyhow::anyhow!("the kiln server didn't answer in {} minutes", QUERY_TIMEOUT.as_secs() / 60)));
            }
        }
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
                // A detached run waits for no one (docs/reach.md §4.1): holding BUFFER with no link
                // up, or with none of it taken for GIVE_WAY, its oldest output gives way.
                if r.detached && r.exit.is_none() && r.held.load(Ordering::Relaxed) >= BUFFER && (detached.is_some() || r.taken.elapsed() > GIVE_WAY) {
                    r.give_way();
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

/// The version a runner's refusal of a Hello says it speaks ("<runner> speaks reach version 1, not
/// 2", every build's words).
fn spoken(reason: &str) -> Option<u32> {
    let (_, rest) = reason.split_once("speaks reach version ")?;
    rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// A sync done for another runner: its own task, the file system work on blocking threads. What it
/// says goes back through the node.
fn sync_worker(root: PathBuf, key: String, session: (String, String), node: async_channel::Sender<In>, busy: super::keeper::Busy) -> async_channel::Sender<(SyncAsk, Option<Commit>)> {
    // Unbounded: the node never waits on a sync; the asker's window bounds what is in flight.
    let (tx, rx) = async_channel::unbounded::<(SyncAsk, Option<Commit>)>();
    crate::rt::spawn(async move {
        let _busy = busy;
        let said = |s: SyncSay, custody: Option<String>, tree: Option<String>| {
            let node = node.clone();
            let session = session.clone();
            async move { node.send(In::Synced { key: session, say: s, custody, tree }).await.is_ok() }
        };
        let say = |s: SyncSay| said(s, None, None);
        let peer = session.0.clone();
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
        while let Ok((a, commit)) = rx.recv().await {
            let m = Arc::clone(&mirror);
            let root = root.clone();
            let peer = peer.clone();
            let step = crate::rt::blocking(move || -> anyhow::Result<Option<(SyncSay, Option<String>, Option<String>)>> {
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
                        let paths = m.needed();
                        Ok(Some((SyncSay::Need { key, root: m.root.to_string_lossy().into_owned(), paths }, None, None)))
                    }
                    SyncAsk::File { key: _, path, offset, data, last } => {
                        m.write(&path, offset, &data, last)?;
                        Ok(None)
                    }
                    SyncAsk::Commit { key } => {
                        let removed = m.commit()?;
                        let done = SyncSay::Done { key: key.clone(), root: m.root.to_string_lossy().into_owned(), written: m.written, removed, bytes: m.bytes };
                        // Taken as its asker handed it on (docs/ci.md W27-R08): what the copy holds
                        // now, by its digest, against the hand-on that came ahead of the commit.
                        let Some(c) = commit.filter(|c| c.version >= wire::CUSTODY || crate::runner::custody::sends()) else { return Ok(Some((done, None, None))) };
                        let tree = m.tree_digest()?;
                        if c.version < wire::CUSTODY {
                            super::custody::workspace_from_before(&peer, &key, &tree);
                            return Ok(Some((done, None, Some(tree))));
                        }
                        match super::custody::take_workspace(&c.root, &peer, &key, c.frame.as_ref(), &tree) {
                            Ok(frame) => Ok(Some((done, Some(frame), Some(tree)))),
                            Err(why) => {
                                // Refused: what it wrote is sent again next time.
                                m.spoil()?;
                                super::custody::refused(&format!("workspace {key}"), &peer, &why);
                                Ok(Some((SyncSay::Failed { key, error: format!("refused: {why}") }, None, None)))
                            }
                        }
                    }
                }
            })
            .await;
            match step {
                Ok(Ok(Some((s, custody, tree)))) => {
                    let done = matches!(s, SyncSay::Done { .. } | SyncSay::Failed { .. });
                    if !said(s, custody, tree).await || done {
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

/// The most of an object one piece carries (docs/ci.md W24-R27).
const OBJECT_PIECE: usize = 1 << 20;

/// What of `digest` this runner's cache holds for `repo`, from `offset`: at most a window, in
/// pieces, the last saying so once the object's end is in it. `None` when it isn't held for that
/// repository (a repository never learns through a peer what another stored). The object is put
/// together once as a local file beside the cache, which later windows are read from.
fn object_window(store: Option<&crate::runner::work::store::Store>, repo: &str, digest: &str, offset: u64) -> anyhow::Result<Option<Vec<(u64, Vec<u8>, bool)>>> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use crate::runner::work::store::{Scope, is_digest};
    let Some(store) = store else { return Ok(None) };
    let scopes: Vec<Scope> = ["trusted", "protected", "untrusted"].iter().map(|d| Scope::new(repo, d)).collect();
    if !is_digest(digest) || store.holder(&scopes, digest).is_none() {
        return Ok(None);
    }
    let dir = store.root().join("peers");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(digest.replace(':', "-"));
    if !path.exists() {
        let tmp = store.temp();
        if !store.copy_to(digest, &tmp)? {
            return Ok(None);
        }
        std::fs::rename(&tmp, &path)?;
    }
    let mut f = std::fs::File::open(&path)?;
    let size = f.metadata()?.len();
    f.seek(SeekFrom::Start(offset.min(size)))?;
    let mut out = Vec::new();
    let (mut at, end) = (offset.min(size), offset.saturating_add(wire::WINDOW).min(size));
    loop {
        let n = usize::try_from(end.saturating_sub(at)).unwrap_or(OBJECT_PIECE).min(OBJECT_PIECE);
        let mut data = vec![0u8; n];
        f.read_exact(&mut data)?;
        let next = at.saturating_add(n as u64);
        out.push((at, data, next >= size));
        at = next;
        if at >= end {
            break;
        }
    }
    Ok(Some(out))
}
