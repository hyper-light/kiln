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
use super::wire::{self, CopySpec, CopyStatus, Joiner, Msg};
use super::{Step, Version};
use crate::http::client::Client;
use crate::proto::AgentMsg;

/// How often each folder is looked at for what changed.
const LOOK: Duration = Duration::from_secs(2);
/// How often each copy says how it is doing.
const STATUS: Duration = Duration::from_secs(10);
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

/// The agent's handle on this machine's sync.
pub struct Runner {
    tx: Sender<Input>,
}

enum Input {
    Wake,
    Heard(Msg, Vec<u8>),
    Connected(bool),
    Fetched(String, Got),
    Uploaded(String, bool),
}

/// What a fetch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Got {
    Here,
    /// The server no longer has it: a version later steps replaced.
    Gone,
    Failed,
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
        let (owner_dir, owner_me, wake_tx) = (dir.clone(), me.to_owned(), tx.clone());
        let spawned = std::thread::Builder::new().name("kiln-sync".into()).spawn(move || match Owner::new(owner_dir, owner_me, say_tx, job_tx, wake_tx) {
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
                    match job {
                        Transfer::Fetch(hash) => {
                            let got = fetch(&http, &server, &me, &token, &blobs, &hash).await;
                            let _ = back.send(Input::Fetched(hash, got));
                        }
                        Transfer::Upload(hash) => {
                            let ok = upload(&http, &server, &me, &token, &blobs, &hash).await;
                            let _ = back.send(Input::Uploaded(hash, ok));
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
            let _ = self.tx.send(Input::Heard(msg, raft));
        }
    }

    /// The link to the server came up, or went down.
    pub fn connected(&self, up: bool) {
        let _ = self.tx.send(Input::Connected(up));
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

async fn fetch(http: &Client, server: &str, me: &str, token: &str, blobs: &Blobs, hash: &str) -> Got {
    let resp = http.get(&blob_url(server, me, hash)).bearer(token).timeout(Duration::from_secs(900)).send().await;
    let mut resp = match resp {
        Ok(r) if r.status() == 404 => return Got::Gone,
        Ok(r) if r.is_success() => r,
        Ok(r) => {
            tracing::debug!(%hash, status = r.status(), "a sync blob did not come");
            return Got::Failed;
        }
        Err(e) => {
            tracing::debug!(%hash, error = %format!("{e:#}"), "a sync blob did not come");
            return Got::Failed;
        }
    };
    let tmp = blobs.incoming();
    let Ok((sink, done)) = crate::rt::file_sink(tmp.clone(), 64 << 30) else { return Got::Failed };
    let mut whole = true;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if sink.send(chunk).await.is_err() {
                    whole = false;
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                whole = false;
                break;
            }
        }
    }
    drop(sink);
    let written = done.await.is_ok();
    let (store, h) = (blobs.clone(), hash.to_owned());
    let adopted = whole && written && crate::rt::blocking(move || store.adopt(&tmp, &h)).await.is_ok_and(|r| r.unwrap_or(false));
    if adopted { Got::Here } else { Got::Failed }
}

async fn upload(http: &Client, server: &str, me: &str, token: &str, blobs: &Blobs, hash: &str) -> bool {
    let url = blob_url(server, me, hash);
    // There already (another copy sent it): nothing to send.
    if let Ok(r) = http.request(crate::http::client::Method::Head, &url).bearer(token).timeout(Duration::from_secs(30)).send().await
        && r.is_success()
    {
        return true;
    }
    let Some(path) = blobs.path(hash) else { return false };
    let Ok(body) = crate::rt::file_source(path) else { return false };
    match http.put(&url).bearer(token).header("content-type", "application/octet-stream").body_stream(body).timeout(Duration::from_secs(1800)).send().await {
        Ok(r) => r.is_success(),
        Err(_) => false,
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
    error: Option<String>,
    /// The newest step sent so far.
    sent: u64,
    waiting: usize,
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
    looked: Instant,
    statused: Instant,
    resent: Instant,
    swept: Instant,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The contents a step carries.
fn hashes(step: &Step) -> impl Iterator<Item = &String> {
    step.changes.iter().filter_map(|c| match &c.to {
        Some(Version::File { hash, .. }) => Some(hash),
        _ => None,
    })
}

impl Owner {
    fn new(dir: PathBuf, me: String, say: async_channel::Sender<AgentMsg>, jobs: async_channel::Sender<(Transfer, String)>, tx: Sender<Input>) -> anyhow::Result<Self> {
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
            Input::Heard(msg, raft) => self.heard(msg, raft),
            Input::Connected(up) => {
                self.connected = up;
                if up {
                    // Steps and states again, at once.
                    self.resent = Instant::now().checked_sub(RESEND).unwrap_or_else(Instant::now);
                    self.statused = Instant::now().checked_sub(STATUS).unwrap_or_else(Instant::now);
                }
            }
            Input::Fetched(hash, got) => {
                self.fetching.remove(&hash);
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
            Input::Uploaded(hash, ok) => {
                self.uploads.insert(hash, if ok { Upload::Done } else { Upload::Failed(Instant::now() + RETRY) });
            }
        }
    }

    fn heard(&mut self, msg: Msg, raft: Vec<u8>) {
        match msg {
            Msg::Copies { copies, token } => {
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
            Msg::Want { hashes, .. } => {
                // Sent again, then the steps that name them.
                for hash in hashes {
                    self.uploads.remove(&hash);
                }
                self.resent = Instant::now().checked_sub(RESEND).unwrap_or_else(Instant::now);
            }
            // Only a runner says these.
            Msg::Steps { .. } | Msg::Status(_) => {}
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
                    self.say(Msg::Status(CopyStatus { repo: spec.repo.clone(), member: spec.member, error: Some(format!("{e:#}")), at: now_ms() / 1000, ..CopyStatus::default() }), Vec::new());
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
        let limits = Limits { exclude: spec.exclude.clone(), ..spec.max_size.map_or_else(Limits::default, |max_size| Limits { max_size, ..Limits::default() }) };
        let mut feed = Feed::open(&root, &self.me, spec.member, limits, self.state_file(spec))?;
        feed.copy.gone.clone_from(&self.gone);
        let r = Replica::open(&group::settings(spec.member, depth)?, store, feed, Unbounded).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let compacted = r.applied().index;
        Ok(Held { spec: spec.clone(), group, r, out: Output::default(), compacted, error: None, sent: 0, waiting: 0 })
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
        let mut wanted: Vec<String> = Vec::new();
        let mut frames: Vec<(Msg, Vec<u8>)> = Vec::new();
        for h in self.held.values_mut() {
            let mut messages = Vec::new();
            if let Err(e) = group::pump(&mut h.r, now, &self.waker, &mut h.out, &mut messages) {
                h.error = Some(format!("{e:?}"));
                continue;
            }
            for m in messages {
                if m.to == SERVER {
                    self.part = self.part.wrapping_add(1);
                    frames.extend(wire::frames(&h.spec.repo, self.part, m.encode_to_vec()));
                }
            }
            match h.r.machine_mut().work(&self.blobs) {
                Ok(missing) => {
                    h.waiting = missing.len();
                    wanted.extend(missing);
                }
                Err(e) => h.error = Some(format!("the folder: {e}")),
            }
            if disk::due(&h.r, h.compacted) && h.r.compact(0, now, &self.waker).unwrap_or(false) {
                h.compacted = h.r.applied().index;
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
        if self.statused.elapsed() >= STATUS {
            self.statused = Instant::now();
            self.status();
        }
        if self.swept.elapsed() >= SWEEP {
            self.swept = Instant::now();
            self.sweep();
        }
    }

    /// Asks the server for contents the folders wait for, a few at a time.
    fn fetch(&mut self, mut wanted: Vec<String>) {
        if !self.connected || self.token.is_empty() {
            return;
        }
        wanted.sort();
        wanted.dedup();
        let now = Instant::now();
        self.retry.retain(|_, at| *at > now);
        for hash in wanted {
            if self.fetching.len() >= TRANSFERS {
                break;
            }
            if self.fetching.contains(&hash) || self.retry.contains_key(&hash) || self.gone.contains(&hash) || self.blobs.has(&hash) {
                continue;
            }
            if self.jobs.try_send((Transfer::Fetch(hash.clone()), self.token.clone())).is_ok() {
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
            match feed.copy.propose(&self.blobs, now_ms()) {
                Ok(steps) if !steps.is_empty() => {
                    if let Err(e) = feed.save() {
                        h.error = Some(format!("the copy's state: {e}"));
                    }
                }
                Ok(_) => {}
                Err(e) => h.error = Some(format!("looking at the folder: {e}")),
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
        let need: Vec<String> = self.held.values().flat_map(|h| h.r.machine().copy.state.sent.iter().flat_map(hashes).cloned().collect::<Vec<_>>()).collect();
        for hash in need {
            if moving >= TRANSFERS {
                break;
            }
            let ready = match self.uploads.get(&hash) {
                None => true,
                Some(Upload::Failed(at)) => *at <= now,
                Some(_) => false,
            };
            if ready && self.jobs.try_send((Transfer::Upload(hash.clone()), self.token.clone())).is_ok() {
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
        for h in self.held.values_mut() {
            let sent = &h.r.machine().copy.state.sent;
            let ready: Vec<&Step> = sent.iter().take_while(|s| hashes(s).all(|x| self.uploads.get(x) == Some(&Upload::Done))).collect();
            let newest = ready.last().map_or(0, |s| s.seq);
            if ready.is_empty() || (!again && newest <= h.sent) {
                continue;
            }
            for s in ready {
                if again || s.seq > h.sent {
                    out.push(Msg::Steps { repo: h.spec.repo.clone(), steps: vec![s.clone()] });
                }
            }
            h.sent = newest;
        }
        for msg in out {
            self.say(msg, Vec::new());
        }
    }

    fn status(&self) {
        let at = now_ms() / 1000;
        for h in self.held.values() {
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
                error: h.error.clone().or_else(|| self.error.clone()),
                at,
            };
            self.say(Msg::Status(status), Vec::new());
        }
        if self.held.is_empty()
            && let Some(error) = &self.error
        {
            for spec in &self.wanted {
                self.say(Msg::Status(CopyStatus { repo: spec.repo.clone(), member: spec.member, error: Some(error.clone()), at, ..CopyStatus::default() }), Vec::new());
            }
        }
    }

    /// Drops contents no copy needs any more: the folders hold what they took.
    fn sweep(&self) {
        let named: HashSet<String> = self.held.values().flat_map(|h| h.r.machine().named()).collect();
        let Ok(all) = self.blobs.all() else { return };
        for (hash, modified) in all {
            if !named.contains(&hash) && modified.elapsed().unwrap_or_default() >= SWEEP_AGE {
                let _ = self.blobs.remove(&hash);
            }
        }
    }
}
