//! A runner's side of repo sync (`docs/sync.md`): the copies the server says this machine holds,
//! each a learner of its repo's group and a folder kept in step with the log. One thread owns them
//! and the machine's sync log (`<state>/sync`), holding that folder's lock so that one runner process
//! of the machine syncs at a time; the agent hands it what the server says ([`Runner::heard`]) and
//! sends what it says back. Contents move over HTTP beside it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use hyper_durable::{LogStore as _, Output, Replica, Unbounded};
use hyper_raft::proto::Message;
use hyper_raft::wire::Record as _;
use serde::{Deserialize, Serialize};

use super::blobs::Blobs;
use super::disk::{self, Store, SyncLog};
use super::group::{self, Feed, SERVER, TICK};
use super::scan::Limits;
use super::wire::{self, CopySpec, CopyStatus, Joiner, Msg, Traffic};
use super::{Step, Version};
use crate::http::client::Client;
use crate::metrics;
use crate::proto::AgentMsg;

/// How often each folder is looked at for what changed.
const LOOK: Duration = Duration::from_secs(2);
/// How often each copy says how it is doing when nothing changes, and how soon after a change.
const STATUS: Duration = Duration::from_secs(10);
const STATUS_SOON: Duration = Duration::from_secs(1);
/// Steps the log hasn't brought back are sent again this often.
const RESEND: Duration = Duration::from_secs(15);
/// Contents moving at once, and how long a failed move waits before it is tried again.
const TRANSFERS: usize = 6;
const RETRY: Duration = Duration::from_secs(10);
/// How often contents nothing names are dropped, and how old they must be.
const SWEEP: Duration = Duration::from_secs(600);
const SWEEP_AGE: Duration = Duration::from_secs(3600);
/// Skipped files a status lists, at the most.
const SKIPPED: usize = 20;
/// Which of a copy's errors: its group's, its folder taking the log, looking at its folder.
const E_GROUP: usize = 0;
const E_FOLDER: usize = 1;
const E_LOOK: usize = 2;

/// The agent's handle on this machine's sync.
#[derive(Clone)]
pub struct Runner {
    tx: Sender<Input>,
}

enum Input {
    Wake,
    Heard(Box<Msg>, Vec<u8>),
    Connected(bool),
    Fetched(String, Got, Moved),
    Uploaded(String, Put, Moved),
    /// Something to tell the server (an agent session's move, `crate::harness::session`).
    Tell(Box<Msg>),
    /// Look at once at the copy a folder is in (a session leaving it), and say what that found.
    LookNow(PathBuf, futures::channel::oneshot::Sender<Option<Looked>>),
    /// How the copy a folder is in stands.
    Standing(PathBuf, futures::channel::oneshot::Sender<Option<Standing>>),
}

/// Where a copy stood in its repo's log when an agent session left it (`crate::harness::moving`):
/// the machine the session goes to takes the log that far before the session goes on there, so
/// it never starts on files older than the ones it left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub repo: String,
    pub index: u64,
}

/// What a look asked for now found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Looked {
    pub repo: String,
    /// It could look: its folder had taken everything committed, and the server was there.
    pub looked: bool,
    /// Steps it proposed.
    pub proposed: usize,
}

/// How a copy stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Standing {
    pub repo: String,
    /// The repo's name.
    pub name: String,
    /// The copy's folder.
    pub folder: String,
    /// Files its last look left out, too big for sync (relative to its folder).
    pub skipped: Vec<String>,
    /// Through where its folder has taken the log, and what it hasn't taken yet.
    pub reached: u64,
    pub behind: usize,
    /// Its own changes the log hasn't brought back yet, and contents it waits for.
    pub unsent: usize,
    pub waiting: usize,
    pub connected: bool,
    pub error: Option<String>,
}

static GLOBAL: std::sync::OnceLock<Runner> = std::sync::OnceLock::new();

/// This process's sync, once the agent started it: what an agent session moving between machines
/// asks (`crate::harness::session`).
pub fn set_global(r: Runner) {
    let _ = GLOBAL.set(r);
}

pub fn global() -> Option<&'static Runner> {
    GLOBAL.get()
}

/// What a fetch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Got {
    Here,
    /// The server no longer has it: a version later steps replaced.
    Gone,
    Failed,
}

/// What an upload did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Put {
    Sent,
    /// The server had it already (another copy sent it).
    There,
    Failed,
}

/// What a transfer moved: its bytes, how long it took, and why it failed.
#[derive(Debug, Default)]
struct Moved {
    bytes: u64,
    took: Duration,
    why: Option<String>,
}

enum Transfer {
    Fetch(String),
    Upload(String),
}

impl Runner {
    /// Starts this machine's sync (`me`, its state under `state_dir`): contents fetched from and
    /// sent to `server` with `http`. What to tell the server comes out of the receiver.
    pub fn start(state_dir: &Path, me: &str, http: Arc<Client>, server: String) -> (Self, async_channel::Receiver<AgentMsg>) {
        let dir = state_dir.join("sync");
        let (tx, rx) = std::sync::mpsc::channel();
        let (say_tx, say_rx) = async_channel::unbounded();
        let (job_tx, job_rx) = async_channel::unbounded::<(Transfer, String)>();
        // The machine's git repositories, and its part of tasks that move them (`super::repos`).
        let repos = super::repos::start(dir.clone(), me.to_owned(), http.clone(), server.clone(), say_tx.clone());
        let (owner_dir, owner_me, wake_tx) = (dir.clone(), me.to_owned(), tx.clone());
        let spawned = std::thread::Builder::new().name("kiln-sync".into()).spawn(move || match Owner::new(owner_dir, owner_me, say_tx, job_tx, wake_tx, repos) {
            Ok(owner) => owner.run(&rx),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "repo sync is off on this machine"),
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "repo sync is off on this machine");
        }
        let (back, me) = (tx.clone(), me.to_owned());
        crate::rt::spawn(async move {
            let Ok(blobs) = Blobs::new(&dir.join("blobs")).map(Arc::new) else { return };
            while let Ok((job, token)) = job_rx.recv().await {
                let (http, server, back, blobs, me) = (http.clone(), server.clone(), back.clone(), blobs.clone(), me.clone());
                crate::rt::spawn(async move {
                    let started = Instant::now();
                    match job {
                        Transfer::Fetch(hash) => {
                            let (got, mut moved) = fetch(&http, &server, &me, &token, &blobs, &hash).await;
                            moved.took = started.elapsed();
                            let _ = back.send(Input::Fetched(hash, got, moved));
                        }
                        Transfer::Upload(hash) => {
                            let (put, mut moved) = upload(&http, &server, &me, &token, &blobs, &hash).await;
                            moved.took = started.elapsed();
                            let _ = back.send(Input::Uploaded(hash, put, moved));
                        }
                    }
                });
            }
        });
        (Self { tx }, say_rx)
    }

    /// What the server said (`ServerMsg::Sync`).
    pub fn heard(&self, json: &str, raft: Vec<u8>) {
        if let Some(msg) = wire::decode(json) {
            let _ = self.tx.send(Input::Heard(Box::new(msg), raft));
        }
    }

    /// The link to the server came up, or went down.
    pub fn connected(&self, up: bool) {
        let _ = self.tx.send(Input::Connected(up));
    }

    /// Looks at once at the copy `folder` is in, proposing what changed there: none when no copy
    /// here holds it.
    pub async fn look_now(&self, folder: &Path) -> Option<Looked> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.tx.send(Input::LookNow(folder.to_path_buf(), tx)).ok()?;
        rx.await.ok().flatten()
    }

    /// How the copy `folder` is in stands: none when no copy here holds it.
    pub async fn standing(&self, folder: &Path) -> Option<Standing> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.tx.send(Input::Standing(folder.to_path_buf(), tx)).ok()?;
        rx.await.ok().flatten()
    }

    /// Tells the server `msg` on the sync lane, if it is connected.
    pub fn tell(&self, msg: Msg) {
        let _ = self.tx.send(Input::Tell(Box::new(msg)));
    }
}

/// Where a blob is asked for or sent.
fn blob_url(server: &str, me: &str, hash: &str) -> String {
    let machine: String = me
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
        .collect();
    format!("{}/api/sync/blobs/{hash}?machine={machine}", server.trim_end_matches('/'))
}

async fn fetch(http: &Client, server: &str, me: &str, token: &str, blobs: &Blobs, hash: &str) -> (Got, Moved) {
    let failed = |why: String| (Got::Failed, Moved { why: Some(why), ..Moved::default() });
    let resp = http.get(&blob_url(server, me, hash)).bearer(token).timeout(Duration::from_secs(900)).send().await;
    let mut resp = match resp {
        Ok(r) if r.status() == 404 => return (Got::Gone, Moved::default()),
        Ok(r) if r.is_success() => r,
        Ok(r) => return failed(format!("the server answered {}", r.status())),
        Err(e) => return failed(format!("{e:#}")),
    };
    let tmp = blobs.incoming();
    let sink = crate::rt::file_sink(tmp.clone(), 64 << 30);
    let Ok((sink, done)) = sink else { return failed("no room to write it".to_owned()) };
    let mut bytes = 0u64;
    let mut broke: Option<String> = None;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                bytes = bytes.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
                if sink.send(chunk).await.is_err() {
                    broke = Some("writing it failed".to_owned());
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                broke = Some(format!("it broke off after {bytes} bytes: {e:#}"));
                break;
            }
        }
    }
    drop(sink);
    let written = done.await;
    if let Some(why) = broke {
        let _ = std::fs::remove_file(&tmp);
        return failed(why);
    }
    if let Err(e) = written {
        return failed(format!("writing it: {e:#}"));
    }
    let (store, h) = (blobs.clone(), hash.to_owned());
    match crate::rt::blocking(move || store.adopt(&tmp, &h)).await {
        Ok(Ok(true)) => (Got::Here, Moved { bytes, ..Moved::default() }),
        Ok(Ok(false)) => failed("it didn't hash to its name".to_owned()),
        Ok(Err(e)) => failed(format!("keeping it: {e}")),
        Err(e) => failed(format!("keeping it: {e:#}")),
    }
}

async fn upload(http: &Client, server: &str, me: &str, token: &str, blobs: &Blobs, hash: &str) -> (Put, Moved) {
    let failed = |why: String| (Put::Failed, Moved { why: Some(why), ..Moved::default() });
    let url = blob_url(server, me, hash);
    // There already (another copy sent it): nothing to send.
    if let Ok(r) = http.request(crate::http::client::Method::Head, &url).bearer(token).timeout(Duration::from_secs(30)).send().await
        && r.is_success()
    {
        return (Put::There, Moved::default());
    }
    let Some(path) = blobs.path(hash) else { return failed("it isn't here any more".to_owned()) };
    let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
    let body = crate::rt::file_source(path);
    let Ok(body) = body else { return failed("it couldn't be read".to_owned()) };
    match http.put(&url).bearer(token).header("content-type", "application/octet-stream").body_stream(body).timeout(Duration::from_secs(1800)).send().await {
        Ok(r) if r.is_success() => (Put::Sent, Moved { bytes, ..Moved::default() }),
        Ok(r) => failed(format!("the server answered {}", r.status())),
        Err(e) => failed(format!("{e:#}")),
    }
}

/// A look at a copy's folder (of repo `name`) took since `started` and proposed `steps`.
fn looked(name: &str, started: Instant, steps: usize) {
    let m = metrics::sync();
    m.look_seconds.observe(&[name], started.elapsed().as_secs_f64());
    if steps > 0 {
        m.proposed.add(&[name], u64::try_from(steps).unwrap_or(u64::MAX));
    }
}

/// What a copy's folder took from the log since last asked: other machines' steps counted, what
/// they did to the files here, and how long they took to get here.
fn took_in(h: &mut Held) {
    let took = std::mem::take(&mut h.r.machine_mut().took);
    let m = metrics::sync();
    let name = h.spec.name.as_str();
    let now = now_ms();
    for t in took.iter().filter(|t| !t.ours) {
        m.applied.inc(&[name]);
        for (outcome, n) in [("applied", t.applied), ("conflict", t.conflicts), ("kept", t.kept), ("refused", t.refused)] {
            if n > 0 {
                m.folder_writes.add(&[name, outcome], u64::try_from(n).unwrap_or(u64::MAX));
            }
        }
        // An image's files (no step, no time) say nothing of how long a change takes.
        if t.at > 0 {
            let ms = u64::try_from(now.saturating_sub(t.at)).unwrap_or(0);
            #[allow(clippy::cast_precision_loss)]
            m.apply_lag.observe(&[name], ms as f64 / 1000.0);
            h.lag_ms = Some(ms);
        }
    }
}

/// A copy's errors that came up since last looked: counted and logged once each, until it goes
/// right again.
fn erred(h: &mut Held) {
    for ((kind, error), counted) in ["group", "folder", "look"].into_iter().zip(&h.errors).zip(&mut h.erred) {
        if let Some(e) = error
            && !*counted
        {
            metrics::sync().errors.inc(&[&h.spec.name, kind]);
            tracing::warn!(target: "kiln.sync", event = "sync.error", repo = %h.spec.name, kind, path = %h.spec.path, error = %e, "a synced copy has an error");
        }
        *counted = error.is_some();
    }
}

/// Wakes the owner when the log answers.
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

/// Which group of this machine's log each copy is (`<state>/sync/copies.json`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    /// `<repo>/<member>` → group.
    groups: BTreeMap<String, u64>,
}

/// One copy this process syncs.
struct Held {
    spec: CopySpec,
    group: u64,
    r: Replica<Store, Feed>,
    out: Output<()>,
    compacted: u64,
    /// What went wrong last in its group, its folder taking the log, and looking at the folder:
    /// each until that goes right again.
    errors: [Option<String>; 3],
    /// The newest step sent so far.
    sent: u64,
    waiting: usize,
    /// What it last said of itself (its time left out), and when.
    said: Option<CopyStatus>,
    said_at: Option<Instant>,
    /// It has looked at its folder since it opened.
    looked: bool,
    /// Which of its errors were counted: each once until it goes right again.
    erred: [bool; 3],
    /// How long the last change from another machine took to get to its folder (ms).
    lag_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upload {
    Moving,
    Done,
    Failed(Instant),
}

struct Owner {
    me: String,
    dir: PathBuf,
    blobs: Blobs,
    say: async_channel::Sender<AgentMsg>,
    jobs: async_channel::Sender<(Transfer, String)>,
    /// The machine's git repositories and its part of tasks (`super::repos`).
    repos: async_channel::Sender<super::repos::Job>,
    wakeup: Arc<Wakeup>,
    waker: Waker,
    start: Instant,
    lock: Option<std::fs::File>,
    log: Option<SyncLog>,
    held: BTreeMap<(String, u64), Held>,
    registry: Registry,
    wanted: Vec<CopySpec>,
    token: String,
    told: bool,
    connected: bool,
    /// Why this machine can't sync, if it can't.
    error: Option<String>,
    joiner: Joiner,
    part: u64,
    fetching: HashSet<String>,
    retry: HashMap<String, Instant>,
    gone: HashSet<String>,
    uploads: HashMap<String, Upload>,
    /// The repo each transfer under way or failed is for (its id), and how often it failed.
    owner_of: HashMap<String, String>,
    tries: HashMap<String, u32>,
    /// What each repo's copies moved with the server.
    traffic: HashMap<String, Traffic>,
    looked: Instant,
    statused: Instant,
    resent: Instant,
    swept: Instant,
}

/// A folder as the file system resolves it (links followed), so one reached two ways is one.
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The contents a step carries.
fn hashes(step: &Step) -> impl Iterator<Item = &String> {
    step.changes.iter().flat_map(|c| {
        let file = match &c.to {
            Some(Version::File { hash, .. }) => Some(hash),
            _ => None,
        };
        file.into_iter().chain(c.bundle.as_ref())
    })
}

impl Owner {
    fn new(
        dir: PathBuf,
        me: String,
        say: async_channel::Sender<AgentMsg>,
        jobs: async_channel::Sender<(Transfer, String)>,
        tx: Sender<Input>,
        repos: async_channel::Sender<super::repos::Job>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir.join("copies"))?;
        let blobs = Blobs::new(&dir.join("blobs"))?;
        let registry = match std::fs::read(dir.join("copies.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        let wakeup = Arc::new(Wakeup { tx, pending: AtomicBool::new(false) });
        let waker = Waker::from(wakeup.clone());
        let long_ago = Instant::now().checked_sub(Duration::from_secs(3600)).unwrap_or_else(Instant::now);
        Ok(Self {
            me,
            dir,
            blobs,
            say,
            jobs,
            repos,
            wakeup,
            waker,
            start: Instant::now(),
            lock: None,
            log: None,
            held: BTreeMap::new(),
            registry,
            wanted: Vec::new(),
            token: String::new(),
            told: false,
            connected: false,
            error: None,
            joiner: Joiner::default(),
            part: 0,
            fetching: HashSet::new(),
            retry: HashMap::new(),
            gone: HashSet::new(),
            uploads: HashMap::new(),
            owner_of: HashMap::new(),
            tries: HashMap::new(),
            traffic: HashMap::new(),
            looked: long_ago,
            statused: long_ago,
            resent: long_ago,
            swept: Instant::now(),
        })
    }

    fn run(mut self, rx: &Receiver<Input>) {
        let mut tick = Instant::now() + TICK;
        loop {
            match rx.recv_timeout(tick.saturating_duration_since(Instant::now())) {
                Ok(input) => self.input(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(input) = rx.try_recv() {
                self.input(input);
            }
            if Instant::now() >= tick {
                tick = Instant::now() + TICK;
                for h in self.held.values_mut() {
                    let _ = h.r.tick();
                }
            }
            self.drive();
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::Wake => {}
            Input::Heard(msg, raft) => self.heard(*msg, raft),
            Input::Connected(up) => {
                self.connected = up;
                let _ = self.repos.try_send(super::repos::Job::Connected(up));
                if up {
                    // Steps and states again, at once.
                    self.resent = Instant::now().checked_sub(RESEND).unwrap_or_else(Instant::now);
                    self.statused = Instant::now().checked_sub(STATUS).unwrap_or_else(Instant::now);
                }
            }
            Input::Fetched(hash, got, moved) => {
                self.fetching.remove(&hash);
                let result = match got {
                    Got::Here => "ok",
                    Got::Gone => "gone",
                    Got::Failed => "failed",
                };
                self.moved(&hash, "down", result, &moved);
                match got {
                    Got::Here => {}
                    Got::Gone => {
                        for h in self.held.values_mut() {
                            h.r.machine_mut().copy.gone.insert(hash.clone());
                        }
                        self.gone.insert(hash);
                    }
                    Got::Failed => {
                        self.retry.insert(hash, Instant::now() + RETRY);
                    }
                }
            }
            Input::Uploaded(hash, put, moved) => {
                let result = match put {
                    Put::Sent => "ok",
                    Put::There => "present",
                    Put::Failed => "failed",
                };
                self.moved(&hash, "up", result, &moved);
                self.uploads.insert(hash, if put == Put::Failed { Upload::Failed(Instant::now() + RETRY) } else { Upload::Done });
            }
            Input::Tell(msg) => self.say(*msg, Vec::new()),
            Input::LookNow(folder, reply) => {
                let _ = reply.send(self.look_now(&folder));
            }
            Input::Standing(folder, reply) => {
                let _ = reply.send(self.standing(&folder));
            }
        }
    }

    /// The copy `folder` is in: the deepest, when copies nest.
    fn copy_of(&self, folder: &Path) -> Option<(String, u64)> {
        let want = canonical(folder);
        self.held.iter().filter(|(_, h)| want.starts_with(canonical(Path::new(&h.spec.path)))).max_by_key(|(_, h)| h.spec.path.len()).map(|(k, _)| k.clone())
    }

    /// Looks at the copy `folder` is in now, as [`Owner::look`] does each folder in turn.
    fn look_now(&mut self, folder: &Path) -> Option<Looked> {
        let key = self.copy_of(folder)?;
        let ready = self.connected && self.told;
        let h = self.held.get_mut(&key)?;
        let mut out = Looked { repo: h.spec.repo.clone(), ..Looked::default() };
        if !ready || !h.r.machine().idle() || h.r.machine().copy.adopting() {
            return Some(out);
        }
        let feed = h.r.machine_mut();
        let started = Instant::now();
        let proposed = feed.copy.propose(&self.blobs, now_ms());
        looked(&h.spec.name, started, proposed.as_ref().map_or(0, Vec::len));
        match proposed {
            Ok(steps) => {
                h.errors[E_LOOK] = if steps.is_empty() { None } else { feed.save().err().map(|e| format!("the copy's state: {e}")) };
                h.looked = true;
                out.looked = true;
                out.proposed = steps.len();
            }
            Err(e) => h.errors[E_LOOK] = Some(format!("looking at the folder: {e}")),
        }
        Some(out)
    }

    fn standing(&self, folder: &Path) -> Option<Standing> {
        let h = self.held.get(&self.copy_of(folder)?)?;
        let feed = h.r.machine();
        let (reached, behind) = feed.progress();
        Some(Standing {
            repo: h.spec.repo.clone(),
            name: h.spec.name.clone(),
            folder: h.spec.path.clone(),
            skipped: feed.copy.skipped.iter().map(|s| s.path.clone()).collect(),
            reached,
            behind,
            unsent: feed.copy.state.sent.len(),
            waiting: h.waiting,
            connected: self.connected,
            error: h.errors.iter().flatten().next().cloned().or_else(|| self.error.clone()),
        })
    }

    fn heard(&mut self, msg: Msg, raft: Vec<u8>) {
        match msg {
            Msg::Copies { copies, token } => {
                let _ = self.repos.try_send(super::repos::Job::Token(token.clone()));
                let _ = self.repos.try_send(super::repos::Job::Paths(copies.iter().map(|c| c.path.clone()).collect()));
                self.wanted = copies;
                self.token = token;
                self.told = true;
                self.hold();
            }
            Msg::Release => self.release(),
            Msg::Raft { repo, id, part, parts } => {
                let Some(bytes) = self.joiner.add("server", id, part, parts, raft) else { return };
                let Ok(m) = Message::decode(&bytes) else { return };
                if m.from != SERVER {
                    return;
                }
                if let Some(h) = self.held.get_mut(&(repo, m.to)) {
                    let _ = h.r.step(m);
                }
            }
            Msg::Want { repo, hashes } => {
                // Sent again, then the steps that name them.
                self.retried(&repo, "upload", u64::try_from(hashes.len()).unwrap_or(u64::MAX));
                for hash in hashes {
                    self.uploads.remove(&hash);
                }
                self.resent = Instant::now().checked_sub(RESEND).unwrap_or_else(Instant::now);
            }
            // The machine's repositories' (`super::repos`), in the order the server sent them.
            Msg::Look | Msg::Bundle { .. } | Msg::Clone(_) | Msg::Delete { .. } | Msg::Forget { .. } | Msg::Shares { .. } | Msg::Take(_) => {
                let _ = self.repos.try_send(super::repos::Job::Order(msg));
            }
            // Only a runner says these.
            Msg::Steps { .. } | Msg::Status(_) | Msg::Repos(_) | Msg::Task(_) | Msg::Move(_) | Msg::Shared(_) => {}
        }
    }

    /// Takes the machine's sync folder (another process of this machine may hold it until it lets
    /// go) and holds the copies the server named, and no others.
    fn hold(&mut self) {
        if self.lock.is_none() {
            let path = self.dir.join("lock");
            let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path);
            match file {
                Ok(f) if f.try_lock().is_ok() => self.lock = Some(f),
                Ok(_) => {
                    self.error = Some("another runner process of this machine is syncing; waiting for it to let go".to_owned());
                    return;
                }
                Err(e) => {
                    self.error = Some(format!("{}: {e}", path.display()));
                    return;
                }
            }
        }
        if self.log.is_none() {
            match disk::open(&self.dir.join("sync.log")) {
                Ok(log) => self.log = Some(log),
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    return;
                }
            }
        }
        self.error = None;
        let wanted: Vec<CopySpec> = self.wanted.clone();
        let keys: HashSet<(String, u64)> = wanted.iter().map(|s| (s.repo.clone(), s.member)).collect();
        let gone: Vec<(String, u64)> = self.held.keys().filter(|k| !keys.contains(*k)).cloned().collect();
        for key in gone {
            self.forget(&key);
        }
        for spec in wanted {
            let key = (spec.repo.clone(), spec.member);
            if self.held.contains_key(&key) {
                continue;
            }
            match self.open(&spec) {
                Ok(h) => {
                    self.held.insert(key, h);
                }
                Err(e) => {
                    tracing::warn!(repo = %spec.name, path = %spec.path, error = %format!("{e:#}"), "a synced copy did not open");
                    self.say(Msg::Status(Box::new(CopyStatus { repo: spec.repo.clone(), member: spec.member, error: Some(format!("{e:#}")), at: now_ms() / 1000, ..CopyStatus::default() })), Vec::new());
                }
            }
        }
    }

    fn open(&mut self, spec: &CopySpec) -> anyhow::Result<Held> {
        let log = self.log.as_ref().ok_or_else(|| anyhow::anyhow!("the sync log isn't open"))?;
        let root = PathBuf::from(&spec.path);
        std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("{}: {e}", root.display()))?;
        let name = format!("{}/{}", spec.repo, spec.member);
        let group = match self.registry.groups.get(&name) {
            Some(g) => *g,
            None => {
                let g = self.registry.groups.values().max().copied().unwrap_or(0).saturating_add(1);
                self.registry.groups.insert(name, g);
                self.save_registry()?;
                g
            }
        };
        let store = disk::claim(log, u128::from(group))?;
        let depth = store.depth();
        let limits = Limits { exclude: spec.exclude.clone(), mode: spec.mode, ..spec.max_size.map_or_else(Limits::default, |max_size| Limits { max_size, ..Limits::default() }) };
        let file = self.state_file(spec);
        let fresh = !file.exists();
        let mut feed = Feed::open(&root, &self.me, spec.member, limits, file)?;
        feed.copy.gone.clone_from(&self.gone);
        // A folder kiln just cloned to join: it takes the group's files, once, before it says anything.
        if fresh && let Some(at) = spec.adopt.filter(|at| *at > 0) {
            feed.copy.adopt(at).map_err(|e| anyhow::anyhow!("{}: {e}", root.display()))?;
            feed.save()?;
        }
        let r = Replica::open(&group::settings(spec.member, depth)?, store, feed, Unbounded).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let compacted = r.applied().index;
        Ok(Held { spec: spec.clone(), group, r, out: Output::default(), compacted, errors: [None, None, None], sent: 0, waiting: 0, said: None, said_at: None, looked: false, erred: [false; 3], lag_ms: None })
    }

    fn state_file(&self, spec: &CopySpec) -> PathBuf {
        let safe: String = spec.repo.chars().filter(char::is_ascii_alphanumeric).collect();
        self.dir.join("copies").join(format!("{safe}-{}.json", spec.member))
    }

    fn save_registry(&self) -> anyhow::Result<()> {
        group::save_atomic(&self.dir.join("copies.json"), &serde_json::to_vec_pretty(&self.registry)?)?;
        Ok(())
    }

    /// A copy the server no longer names: its group goes; its folder's files stay.
    fn forget(&mut self, key: &(String, u64)) {
        let Some(h) = self.held.remove(key) else { return };
        let file = self.state_file(&h.spec);
        let group = h.group;
        drop(h);
        if let Some(log) = &self.log
            && let Err(e) = disk::remove(log, u128::from(group))
        {
            tracing::warn!(error = %format!("{e:#}"), "a removed copy's group stayed in the sync log");
        }
        self.registry.groups.remove(&format!("{}/{}", key.0, key.1));
        let _ = self.save_registry();
        let _ = std::fs::remove_file(file);
    }

    /// Another process of this machine syncs now: everything is let go, nothing removed.
    fn release(&mut self) {
        self.held.clear();
        self.log = None;
        if let Some(f) = self.lock.take() {
            let _ = f.unlock();
        }
        self.told = false;
    }

    fn say(&self, msg: Msg, raft: Vec<u8>) {
        if self.connected {
            let _ = self.say.try_send(AgentMsg::Sync { json: wire::encode(&msg), raft });
        }
    }

    fn drive(&mut self) {
        self.wakeup.pending.store(false, Ordering::SeqCst);
        if self.told && self.lock.is_none() {
            self.hold();
        }
        let now = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        // Contents the folders wait for, each with the repo it's for.
        let mut wanted: Vec<(String, String)> = Vec::new();
        let mut frames: Vec<(Msg, Vec<u8>)> = Vec::new();
        let mut freed = false;
        for h in self.held.values_mut() {
            let mut messages = Vec::new();
            if let Err(e) = group::pump(&mut h.r, now, &self.waker, &mut h.out, &mut messages) {
                h.errors[E_GROUP] = Some(format!("{e:?}"));
                continue;
            }
            h.errors[E_GROUP] = None;
            for m in messages {
                if m.to == SERVER {
                    self.part = self.part.wrapping_add(1);
                    frames.extend(wire::frames(&h.spec.repo, self.part, m.encode_to_vec()));
                }
            }
            match h.r.machine_mut().work(&self.blobs) {
                Ok(missing) => {
                    h.waiting = missing.len();
                    wanted.extend(missing.into_iter().map(|m| (m, h.spec.repo.clone())));
                    h.errors[E_FOLDER] = None;
                }
                Err(e) => h.errors[E_FOLDER] = Some(format!("the folder: {e}")),
            }
            took_in(h);
            // An adopting folder the log has brought through its index: it is the group's now.
            let feed = h.r.machine_mut();
            if let Some(at) = feed.copy.state.adopt
                && feed.idle()
                && feed.progress().0 >= at
            {
                match feed.copy.adopted(&self.blobs).and_then(|_| feed.save()) {
                    Ok(()) => tracing::info!(repo = %h.spec.name, path = %h.spec.path, "a new copy took the group's files"),
                    Err(e) => h.errors[E_FOLDER] = Some(format!("the folder: {e}")),
                }
            }
            // A copy serves no one: it keeps nothing it took.
            if disk::due(&h.r, h.compacted) && h.r.compact(0, now, &self.waker).unwrap_or(false) {
                h.compacted = h.r.applied().index;
                freed = true;
            }
        }
        if freed {
            for h in self.held.values_mut() {
                h.r.resume();
            }
        }
        for (msg, raft) in frames {
            self.say(msg, raft);
        }
        self.fetch(wanted);
        if self.looked.elapsed() >= LOOK {
            self.looked = Instant::now();
            self.look();
        }
        self.upload();
        let again = self.resent.elapsed() >= RESEND;
        if again {
            self.resent = Instant::now();
        }
        self.send_steps(again);
        let every = self.statused.elapsed() >= STATUS;
        if every {
            self.statused = Instant::now();
        }
        self.status(every);
        for h in self.held.values_mut() {
            erred(h);
        }
        if self.swept.elapsed() >= SWEEP {
            self.swept = Instant::now();
            self.sweep();
        }
    }

    /// A synced repo's name, as metrics and logs label it, from its id.
    fn name_of(&self, repo: &str) -> String {
        self.held.values().find(|h| h.spec.repo == repo).map_or_else(|| repo.to_owned(), |h| h.spec.name.clone())
    }

    /// `n` of something (`what`) done again for `repo`.
    fn retried(&mut self, repo: &str, what: &'static str, n: u64) {
        if n == 0 {
            return;
        }
        let name = self.name_of(repo);
        metrics::sync().retries.add(&[&name, what], n);
        tracing::debug!(target: "kiln.sync", event = "sync.retry", repo = %name, what, n, "tried again");
        let t = self.traffic.entry(repo.to_owned()).or_default();
        if what == "resend" {
            t.resent = t.resent.saturating_add(n);
        } else {
            t.retried = t.retried.saturating_add(n);
        }
    }

    /// A transfer is over (`direction` `up` or `down`): counted for the repo it was for, and a
    /// failure logged the first time and every tenth after (a server away fails them all).
    fn moved(&mut self, hash: &str, direction: &'static str, result: &'static str, moved: &Moved) {
        let repo = if result == "failed" { self.owner_of.get(hash).cloned() } else { self.owner_of.remove(hash) }.unwrap_or_default();
        let name = self.name_of(&repo);
        let m = metrics::sync();
        m.transfers.inc(&[&name, direction, result]);
        m.transfer_seconds.observe(&[direction], moved.took.as_secs_f64());
        if moved.bytes > 0 {
            m.transfer_bytes.add(&[&name, direction], moved.bytes);
        }
        let t = self.traffic.entry(repo).or_default();
        match (direction, result) {
            ("up", "ok" | "present") => {
                t.sent += 1;
                t.sent_bytes = t.sent_bytes.saturating_add(moved.bytes);
            }
            ("down", "ok") => {
                t.fetched += 1;
                t.fetched_bytes = t.fetched_bytes.saturating_add(moved.bytes);
            }
            ("down", "gone") => t.gone += 1,
            _ => {}
        }
        if result != "failed" {
            self.tries.remove(hash);
            return;
        }
        let why = moved.why.clone().unwrap_or_else(|| "it failed".to_owned());
        t.failed += 1;
        t.last_failure = Some(format!("{} {}: {why}", if direction == "up" { "sending" } else { "fetching" }, hash.get(..12).unwrap_or(hash)));
        t.failed_at = now_ms() / 1000;
        let n = self.tries.entry(hash.to_owned()).or_insert(0);
        *n = n.saturating_add(1);
        if *n == 1 || n.is_multiple_of(10) {
            tracing::warn!(target: "kiln.sync", event = "sync.transfer_failed", repo = %name, direction, hash = %hash, tries = *n, error = %why, "contents didn't move: tried again in {}s", RETRY.as_secs());
        }
    }

    /// Asks the server for contents the folders wait for (each with the repo it's for), a few at
    /// a time.
    fn fetch(&mut self, mut wanted: Vec<(String, String)>) {
        if !self.connected || self.token.is_empty() {
            return;
        }
        wanted.sort();
        wanted.dedup_by(|a, b| a.0 == b.0);
        let now = Instant::now();
        self.retry.retain(|_, at| *at > now);
        for (hash, repo) in wanted {
            if self.fetching.len() >= TRANSFERS {
                break;
            }
            if self.fetching.contains(&hash) || self.retry.contains_key(&hash) || self.gone.contains(&hash) || self.blobs.has(&hash) {
                continue;
            }
            if self.jobs.try_send((Transfer::Fetch(hash.clone()), self.token.clone())).is_ok() {
                if self.tries.get(&hash).is_some_and(|n| *n > 0) {
                    self.retried(&repo, "fetch", 1);
                }
                self.owner_of.insert(hash.clone(), repo);
                self.fetching.insert(hash);
            }
        }
    }

    /// Looks at each folder that has taken everything committed, and proposes what changed.
    fn look(&mut self) {
        if !self.connected || !self.told {
            return;
        }
        for h in self.held.values_mut() {
            if !h.r.machine().idle() {
                continue;
            }
            let feed = h.r.machine_mut();
            if feed.copy.adopting() {
                continue;
            }
            let started = Instant::now();
            let proposed = feed.copy.propose(&self.blobs, now_ms());
            looked(&h.spec.name, started, proposed.as_ref().map_or(0, Vec::len));
            match proposed {
                Ok(steps) if !steps.is_empty() => {
                    h.errors[E_LOOK] = feed.save().err().map(|e| format!("the copy's state: {e}"));
                    h.looked = true;
                }
                Ok(_) => {
                    h.errors[E_LOOK] = None;
                    h.looked = true;
                }
                Err(e) => h.errors[E_LOOK] = Some(format!("looking at the folder: {e}")),
            }
        }
    }

    /// Sends the server the contents unsent steps carry, a few at a time.
    fn upload(&mut self) {
        if !self.connected || self.token.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut moving = self.uploads.values().filter(|u| **u == Upload::Moving).count();
        let need: Vec<(String, String)> = self.held.values().flat_map(|h| h.r.machine().copy.state.sent.iter().flat_map(hashes).map(|x| (x.clone(), h.spec.repo.clone())).collect::<Vec<_>>()).collect();
        for (hash, repo) in need {
            if moving >= TRANSFERS {
                break;
            }
            let (ready, again) = match self.uploads.get(&hash) {
                None => (true, false),
                Some(Upload::Failed(at)) => (*at <= now, true),
                Some(_) => (false, false),
            };
            if ready && self.jobs.try_send((Transfer::Upload(hash.clone()), self.token.clone())).is_ok() {
                if again {
                    self.retried(&repo, "upload", 1);
                }
                self.owner_of.insert(hash.clone(), repo);
                self.uploads.insert(hash, Upload::Moving);
                moving += 1;
            }
        }
    }

    /// Sends each copy's unsent steps whose contents are on the server, in order: new ones at once,
    /// all of them `again`.
    fn send_steps(&mut self, again: bool) {
        if !self.connected {
            return;
        }
        let mut out = Vec::new();
        let mut resent: Vec<(String, u64)> = Vec::new();
        for h in self.held.values_mut() {
            let sent = &h.r.machine().copy.state.sent;
            let ready: Vec<&Step> = sent.iter().take_while(|s| hashes(s).all(|x| self.uploads.get(x) == Some(&Upload::Done))).collect();
            let newest = ready.last().map_or(0, |s| s.seq);
            if ready.is_empty() || (!again && newest <= h.sent) {
                continue;
            }
            let mut n = 0u64;
            for s in ready {
                if again || s.seq > h.sent {
                    if s.seq <= h.sent {
                        n += 1;
                    }
                    out.push(Msg::Steps { repo: h.spec.repo.clone(), steps: vec![s.clone()] });
                }
            }
            h.sent = newest;
            resent.push((h.spec.repo.clone(), n));
        }
        for msg in out {
            self.say(msg, Vec::new());
        }
        for (repo, n) in resent {
            self.retried(&repo, "resend", n);
        }
    }

    /// Says how each copy is doing: soon after it changes, and `every` so often regardless.
    fn status(&mut self, every: bool) {
        if !self.connected {
            return;
        }
        let at = now_ms() / 1000;
        let mut out = Vec::new();
        for h in self.held.values_mut() {
            let feed = h.r.machine();
            let (applied, behind) = feed.progress();
            let status = CopyStatus {
                repo: h.spec.repo.clone(),
                member: h.spec.member,
                applied,
                behind,
                unsent: feed.copy.state.sent.len(),
                waiting: h.waiting,
                conflicts: feed.conflicts().len(),
                skipped: feed.copy.skipped.iter().take(SKIPPED).cloned().collect(),
                error: h.errors.iter().flatten().next().cloned().or_else(|| self.error.clone()),
                at: 0,
                looked: h.looked,
                adopting: feed.copy.adopting(),
                traffic: self.traffic.get(&h.spec.repo).cloned().unwrap_or_default(),
                lag_ms: h.lag_ms,
            };
            let changed = h.said.as_ref() != Some(&status);
            let soon = h.said_at.is_none_or(|t| t.elapsed() >= STATUS_SOON);
            if every || (changed && soon) {
                h.said = Some(status.clone());
                h.said_at = Some(Instant::now());
                out.push(CopyStatus { at, ..status });
            }
        }
        for status in out {
            self.say(Msg::Status(Box::new(status)), Vec::new());
        }
        if every {
            let skipped = self.held.values().map(|h| (vec![h.spec.name.clone()], i64::try_from(h.r.machine().copy.skipped.len()).unwrap_or(i64::MAX)));
            metrics::sync().skipped_files.replace(skipped.collect::<Vec<_>>());
        }
        if every && self.held.is_empty()
            && let Some(error) = &self.error
        {
            for spec in &self.wanted {
                self.say(Msg::Status(Box::new(CopyStatus { repo: spec.repo.clone(), member: spec.member, error: Some(error.clone()), at, ..CopyStatus::default() })), Vec::new());
            }
        }
    }

    /// Drops contents no copy needs any more: the folders hold what they took. What transfers
    /// over and done with were for goes too.
    fn sweep(&mut self) {
        let (fetching, retry, uploads) = (&self.fetching, &self.retry, &self.uploads);
        let live = |h: &String| fetching.contains(h) || retry.contains_key(h) || uploads.get(h).is_some_and(|u| *u != Upload::Done);
        self.owner_of.retain(|h, _| live(h));
        self.tries.retain(|h, _| live(h));
        let named: HashSet<String> = self.held.values().flat_map(|h| h.r.machine().named()).collect();
        let Ok(all) = self.blobs.all() else { return };
        for (hash, modified) in all {
            if !named.contains(&hash) && modified.elapsed().unwrap_or_default() >= SWEEP_AGE {
                let _ = self.blobs.remove(&hash);
            }
        }
    }
}
