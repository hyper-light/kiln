//! The server's side of repo sync (`docs/sync.md`): every synced repo, the leader of its group, the
//! contents its steps name, and what each copy says of itself. One thread owns the groups and their
//! log ([`Owner`]); the hub hands it what runners say, the API what admins ask, and it hands back
//! what to send ([`Out`]), which the hub sends on the sync lane.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use hyper_durable::{LogStore as _, Output, Replica, ReplicaError, Unbounded};
use hyper_raft::proto::{Message, MessageType};
use hyper_raft::wire::Record as _;
use serde::{Deserialize, Serialize};

use super::activity::{self, Activity, Event, Kind, Tone};
use super::contents::Contents;
use super::disk::{self, Store, SyncLog};
use super::group::{self, Index, SERVER, TICK, Taken};
use super::wire::{self, CopySpec, CopyStatus, Joiner, MoveNews, Msg};
use super::{Change, Decision, STEP_CHANGES, Step, Version};
use crate::metrics;

/// A log entry's bytes at the most (the log holds one up to 1 MiB).
const ENTRY: usize = 900 << 10;
/// A change of configuration not seen applied by then is proposed again.
const CHANGE_AGAIN: Duration = Duration::from_secs(10);
/// How often contents no tree names any more are looked for, and how old they must be to go.
const SWEEP: Duration = Duration::from_secs(600);
const SWEEP_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// How often the view the API shows is made again, at the most.
const VIEW_EVERY: Duration = Duration::from_millis(500);
/// How often new activity is saved, at the most, and the copies' gauges measured at the least.
const SAVE_EVERY: Duration = Duration::from_secs(5);
const GAUGE_EVERY: Duration = Duration::from_secs(5);
/// The same trouble from the same machine shows in a repo's activity (and the log) once in this
/// long; the metrics count every time.
const QUIET: Duration = Duration::from_secs(60);
/// Steps' arrival times a group keeps for their commits' latency, at the most.
const ARRIVED: usize = 4096;
/// A copy's states, as the API and the metrics name them.
const STATES: [&str; 7] = ["in step", "catching up", "fetching", "sending", "starting", "offline", "error"];

/// A synced repo, as the server keeps it (`<data>/sync/repos.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub name: String,
    /// Its group in the server's log.
    pub group: u64,
    /// Unix seconds.
    pub created: i64,
    pub next_member: u64,
    pub copies: Vec<CopyRef>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Its files, its git refs, or both.
    #[serde(default, skip_serializing_if = "super::Mode::is_files")]
    pub mode: super::Mode,
    /// Synced only while a task moves the repository between machines (`crate::sync::tasks`): the
    /// task's batch, which stops syncing it once the move is done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transient: Option<String>,
}

/// A copy of a repo: a machine and a folder on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyRef {
    pub member: u64,
    pub machine: String,
    pub path: String,
    pub added: i64,
    /// A folder kiln cloned to join: it takes the group's files through this log index
    /// ([`CopySpec::adopt`]); cleared once it says it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopt: Option<u64>,
}

/// What the API shows of a repo.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoView {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub exclude: Vec<String>,
    pub mode: super::Mode,
    /// Files in the repo as the log has it.
    pub files: usize,
    /// Through where the server's member applied the log.
    pub index: u64,
    pub conflicts: Vec<ConflictView>,
    pub copies: Vec<CopyView>,
    pub error: Option<String>,
    /// Synced only for a move under way: that task's batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transient: Option<String>,
    /// What its sync did since the server started.
    pub stats: RepoStats,
}

/// What a repo's sync did since the server started (`since`, unix milliseconds).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoStats {
    pub since: i64,
    /// Steps committed, the files they changed and the bytes of the contents they brought.
    pub steps: u64,
    pub files: u64,
    pub bytes: u64,
    /// Changes that met another machine's.
    pub conflicts: u64,
    /// Steps and messages let go, things done again, and errors.
    pub dropped: u64,
    pub retries: u64,
    pub errors: u64,
    /// When the newest step was committed (unix milliseconds).
    pub last_step: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CopyView {
    pub member: u64,
    pub machine: String,
    pub path: String,
    pub added: i64,
    /// `in step`, `catching up`, `fetching`, `sending`, `starting`, `offline`, `error`.
    pub state: &'static str,
    pub status: Option<CopyStatus>,
    /// When the server last heard how it is (unix milliseconds).
    pub heard: Option<i64>,
    /// Log entries its folder hasn't taken yet.
    pub lag: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ConflictView {
    /// The conflict copy.
    pub path: String,
    /// The path it stands beside.
    pub beside: String,
}

/// Everything the API shows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct View {
    pub repos: Vec<RepoView>,
    /// Why sync isn't running on this server, if it isn't.
    pub error: Option<String>,
}

/// Something to send a machine's syncing runner process.
#[derive(Debug)]
pub struct Out {
    pub machine: String,
    pub msg: Msg,
    pub raft: Vec<u8>,
}

/// What a conflict's resolution keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Keep {
    /// The path's own version; the conflict copy goes.
    Name,
    /// The conflict copy's version, at the path.
    Copy,
    /// Both: the conflict copy stays under a name of its own.
    Both,
}

/// What an admin asks.
#[derive(Debug)]
pub enum Ask {
    Create {
        name: String,
        copies: Vec<(String, String)>,
        exclude: Vec<String>,
        mode: super::Mode,
        transient: Option<String>,
    },
    /// `adopt`: the folder is one kiln just cloned, to take the group's files ([`CopySpec::adopt`]).
    AddCopy { repo: String, machine: String, path: String, adopt: bool },
    RemoveCopy { repo: String, member: u64 },
    Delete { repo: String },
    Resolve { repo: String, path: String, keep: Keep },
}

enum Input {
    Wake,
    From { machine: String, msg: Msg, raft: Vec<u8> },
    Machine { machine: String, up: bool },
    Ask { ask: Ask, reply: futures::channel::oneshot::Sender<Result<String, String>> },
    /// Save what isn't saved yet: the server is stopping.
    Flush(futures::channel::oneshot::Sender<()>),
}

/// The hub's handle on the server's side of sync.
pub struct Sync {
    tx: Sender<Input>,
    view: Arc<Mutex<View>>,
    activity: Arc<Mutex<Activity>>,
    out: async_channel::Receiver<Out>,
    /// What others send a machine's syncing runner process on the sync lane (`crate::sync::tasks`).
    say: async_channel::Sender<Out>,
    contents: Arc<Contents>,
    key: String,
}

impl Sync {
    /// Starts the owner of the groups kept in `dir`, the contents their steps name in `contents`.
    /// Blob tokens are signed with `key`.
    pub fn start(dir: PathBuf, key: String, contents: Arc<Contents>) -> Self {
        // Contents half-sent when the server stopped (`<data>/sync/incoming`): none is under way
        // before it listens.
        let incoming = dir.join("incoming");
        if incoming.is_dir()
            && let Err(e) = std::fs::remove_dir_all(&incoming)
        {
            tracing::warn!(target: "kiln.sync", event = "sync.incoming", error = %e, "contents half-sent before the server stopped couldn't be removed");
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out) = async_channel::unbounded();
        let view = Arc::new(Mutex::new(View::default()));
        let activity = Arc::new(Mutex::new(Activity::default()));
        let say = out_tx.clone();
        let (v, a, wake_tx) = (view.clone(), activity.clone(), tx.clone());
        let (k, c) = (key.clone(), Arc::clone(&contents));
        let spawned = std::thread::Builder::new().name("kiln-sync".into()).spawn(move || {
            // What's held is judged from the store's listing: the thread waits for it.
            if !c.wait_loaded(super::contents::LOAD_WAIT) {
                tracing::warn!(target: "kiln.blobs", event = "list", "sync's contents weren't listed in time: what's held is known as it comes");
            }
            match Owner::open(&dir, c, out_tx, v.clone(), a, wake_tx, k) {
                Ok(owner) => owner.run(&rx),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "repo sync is off");
                    lock(&v).error = Some(format!("sync is off: {e:#}"));
                }
            }
        });
        if let Err(e) = spawned {
            lock(&view).error = Some(format!("sync is off: {e}"));
        }
        Self { tx, view, activity, out, say, contents, key }
    }

    pub fn view(&self) -> View {
        lock(&self.view).clone()
    }

    /// Saves the activity not saved yet, waiting a second at the most: the server is stopping.
    pub async fn flush(&self) {
        let (reply, done) = futures::channel::oneshot::channel();
        if self.tx.send(Input::Flush(reply)).is_ok() {
            let _ = crate::rt::timeout(Duration::from_secs(1), done).await;
        }
    }

    /// What happened in the synced repos: the events numbered `since` on, of `repo` alone if given,
    /// the newest `limit` first; and the number the next event gets.
    pub fn activity(&self, since: u64, repo: Option<&str>, limit: usize) -> (Vec<Event>, u64) {
        let a = lock(&self.activity);
        (a.since(since, repo, limit), a.next())
    }

    /// The server's contents.
    pub fn contents(&self) -> &Arc<Contents> {
        &self.contents
    }

    /// What a machine's runner said of its copies (`AgentMsg::Sync`, decoded).
    pub fn from_runner(&self, machine: &str, msg: Msg, raft: Vec<u8>) {
        let _ = self.tx.send(Input::From { machine: machine.to_owned(), msg, raft });
    }

    /// Sends a machine's syncing runner process `msg` on the sync lane, after what is already on
    /// its way there.
    pub fn send(&self, machine: &str, msg: Msg) {
        let _ = self.say.try_send(Out { machine: machine.to_owned(), msg, raft: Vec::new() });
    }

    /// A machine has a syncing runner process connected now (`up`), or none any more.
    pub fn machine(&self, machine: &str, up: bool) {
        let _ = self.tx.send(Input::Machine { machine: machine.to_owned(), up });
    }

    pub async fn ask(&self, ask: Ask) -> Result<String, String> {
        let (reply, answer) = futures::channel::oneshot::channel();
        self.tx.send(Input::Ask { ask, reply }).map_err(|_| "sync is not running".to_owned())?;
        answer.await.unwrap_or_else(|_| Err("sync is not running".to_owned()))
    }

    /// What to send, as the owner makes it.
    pub fn outgoing(&self) -> async_channel::Receiver<Out> {
        self.out.clone()
    }

    /// The token a machine's blob requests carry.
    pub fn token(&self, machine: &str) -> String {
        token(&self.key, machine)
    }
}

/// The token a machine's blob requests carry: signed by the server's key, for that machine.
pub fn token(key: &str, machine: &str) -> String {
    crate::util::hmac_hex(key.as_bytes(), format!("sync:{machine}").as_bytes())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
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

/// One repo's group on the server.
struct Group {
    r: Replica<Store, Index>,
    out: Output<()>,
    /// Where the log was last compacted to.
    compacted: u64,
    /// The next step taken from each member: a member's steps are taken in order.
    next: HashMap<u64, u64>,
    /// A change of configuration proposed and not yet seen applied: the member, added or not.
    changing: Option<(u64, bool, Instant)>,
    error: Option<String>,
    /// When each step proposed and not yet committed arrived: its member and number.
    arrived: HashMap<(u64, u64), Instant>,
}

pub struct Owner {
    dir: PathBuf,
    log: SyncLog,
    repos: Vec<Repo>,
    groups: HashMap<String, Group>,
    up: HashSet<String>,
    statuses: HashMap<(String, u64), CopyStatus>,
    joiner: Joiner,
    part: u64,
    out: async_channel::Sender<Out>,
    wakeup: Arc<Wakeup>,
    waker: Waker,
    view: Arc<Mutex<View>>,
    viewed: Instant,
    changed: bool,
    contents: Arc<Contents>,
    key: String,
    start: Instant,
    swept: Instant,
    /// What happened in each repo, shared with the API; saved now and then.
    activity: Arc<Mutex<Activity>>,
    saved: Instant,
    /// The copies' gauges, last measured, and the repos (by name) this server published them for.
    gauged: Instant,
    gauged_repos: BTreeSet<String>,
    /// Each copy's state as last seen, and when the server last heard how it is (and the unix
    /// milliseconds then).
    states: HashMap<(String, u64), &'static str>,
    heard_at: HashMap<(String, u64), (Instant, i64)>,
    /// When each kind of trouble from a machine last showed in a repo's activity.
    quiet: HashMap<(String, String, &'static str), Instant>,
    /// The copies, by repo and machine, noted going offline (now, or before the server started):
    /// their coming back is news; a copy merely connecting (as each does when the server starts)
    /// isn't.
    away: HashSet<(String, String)>,
    /// Each repo's steps through this log index are on record (in its activity, or committed
    /// before the server started): taken again, as the log is replayed when the server starts,
    /// they aren't news, and aren't counted again.
    recorded: HashMap<String, u64>,
    stats: HashMap<String, RepoStats>,
    started: i64,
}

impl Owner {
    fn open(dir: &Path, contents: Arc<Contents>, out: async_channel::Sender<Out>, view: Arc<Mutex<View>>, activity: Arc<Mutex<Activity>>, tx: Sender<Input>, key: String) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let repos: Vec<Repo> = match std::fs::read(dir.join("repos.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let log = disk::open(&dir.join("sync.log"))?;
        let loaded = Activity::load(dir, repos.iter().map(|r| r.id.as_str()));
        let away = loaded.away();
        let recorded = loaded.recorded();
        *lock(&activity) = loaded;
        let wakeup = Arc::new(Wakeup { tx, pending: AtomicBool::new(false) });
        let waker = Waker::from(wakeup.clone());
        let mut owner = Self {
            dir: dir.to_owned(),
            log,
            repos: Vec::new(),
            groups: HashMap::new(),
            up: HashSet::new(),
            statuses: HashMap::new(),
            joiner: Joiner::default(),
            part: 0,
            out,
            wakeup,
            waker,
            view,
            viewed: Instant::now(),
            changed: true,
            contents,
            key,
            start: Instant::now(),
            swept: Instant::now(),
            activity,
            saved: Instant::now(),
            gauged: Instant::now(),
            gauged_repos: BTreeSet::new(),
            states: HashMap::new(),
            heard_at: HashMap::new(),
            quiet: HashMap::new(),
            away,
            recorded,
            stats: HashMap::new(),
            started: now_ms(),
        };
        for repo in repos {
            if let Err(e) = owner.open_group(&repo) {
                tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "a synced repo's group did not open");
            }
            owner.repos.push(repo);
        }
        Ok(owner)
    }

    fn open_group(&mut self, repo: &Repo) -> anyhow::Result<()> {
        let store = disk::claim(&self.log, u128::from(repo.group))?;
        let depth = store.depth();
        let index = Index::open(Some(self.dir.join(format!("{}.tree.json", repo.id))))?;
        let mut r = Replica::open(&group::settings(SERVER, depth)?, store, index, Unbounded).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        r.campaign().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let compacted = r.applied().index;
        // What the log committed before now is replayed into the tree: no news.
        let before = self.recorded.entry(repo.id.clone()).or_default();
        *before = (*before).max(r.durable_commit());
        self.groups.insert(repo.id.clone(), Group { r, out: Output::default(), compacted, next: HashMap::new(), changing: None, error: None, arrived: HashMap::new() });
        Ok(())
    }

    fn save(&self) -> anyhow::Result<()> {
        group::save_atomic(&self.dir.join("repos.json"), &serde_json::to_vec_pretty(&self.repos)?)?;
        Ok(())
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
                for g in self.groups.values_mut() {
                    let _ = g.r.tick();
                }
            }
            self.drive();
        }
    }

    fn input(&mut self, input: Input) {
        match input {
            Input::Wake => {}
            Input::From { machine, msg, raft } => self.heard(&machine, msg, raft),
            Input::Machine { machine, up } => {
                if up {
                    self.up.insert(machine.clone());
                    self.tell_copies(&machine);
                } else {
                    self.up.remove(&machine);
                }
                self.changed = true;
            }
            Input::Ask { ask, reply } => {
                let answer = self.ask(ask).map_err(|e| format!("{e:#}"));
                // The view shows what was asked before the asker hears back (a task looks for the
                // copy it just added).
                self.publish();
                let _ = reply.send(answer);
            }
            Input::Flush(reply) => {
                self.save_activity();
                let _ = reply.send(());
            }
        }
    }

    fn heard(&mut self, machine: &str, msg: Msg, raft: Vec<u8>) {
        match msg {
            Msg::Raft { repo, id, part, parts } => {
                let Some(bytes) = self.joiner.add(machine, id, part, parts, raft) else { return };
                let Ok(m) = Message::decode(&bytes) else {
                    self.dropped(&repo, machine, "message", "undecodable", format!("a message from {machine} didn't decode: let go"));
                    return;
                };
                let mine = self.members_on(&repo, machine);
                if m.to != SERVER || !mine.contains(&m.from) {
                    self.dropped(&repo, machine, "message", "not_member", format!("a message from {machine} for a member that isn't its: let go"));
                    return;
                }
                let refused = self.groups.get_mut(&repo).and_then(|g| g.r.step(m).err());
                if let Some(e) = refused {
                    self.dropped(&repo, machine, "message", why(&e), format!("the log didn't take a message from {machine} ({e}): it comes again"));
                }
            }
            Msg::Steps { repo, steps } => self.steps(machine, &repo, steps),
            Msg::Status(status) => {
                if self.members_on(&status.repo, machine).contains(&status.member) {
                    if status.looked && !status.adopting {
                        self.settled(&status.repo, status.member);
                    }
                    self.heard_at.insert((status.repo.clone(), status.member), (Instant::now(), now_ms()));
                    self.statuses.insert((status.repo.clone(), status.member), *status);
                    self.changed = true;
                }
            }
            Msg::Move(news) => self.moved(machine, news),
            // Only the server says these, or they aren't for the groups (`crate::sync::places`).
            Msg::Copies { .. } | Msg::Release | Msg::Want { .. } | Msg::Repos(_) | Msg::Look | Msg::Bundle { .. } | Msg::Clone(_) | Msg::Delete { .. } | Msg::Forget { .. } | Msg::Task(_) => {}
        }
    }

    /// An agent session moving with a repo's files, as one of its machines saw its part: in the
    /// repo's activity, if that machine holds a copy of it.
    fn moved(&mut self, machine: &str, mut news: MoveNews) {
        if self.members_on(&news.repo, machine).is_empty() {
            return;
        }
        // Each machine speaks for its own part alone.
        if news.phase == "arrived" {
            machine.clone_into(&mut news.to);
        } else {
            machine.clone_into(&mut news.from);
        }
        let waited = secs_text(news.waited_ms);
        let session = if news.session.is_empty() { "an agent session".to_owned() } else { format!("agent session {}", short(&news.session)) };
        let (tone, text) = match news.phase.as_str() {
            "left" => (Tone::Info, format!("{session} is leaving {} for {}: its folder's last changes are in the log through #{} (that took {waited})", news.from, news.to, news.index)),
            "arrived" => (Tone::Ok, format!("{session} went on on {} once its folder there had #{} (it waited {waited})", news.to, news.index)),
            _ => (Tone::Warn, format!("{session} couldn't move from {} to {}, and stays: {}", news.from, news.to, news.error.as_deref().unwrap_or("it didn't say why"))),
        };
        let name = self.name_of(&news.repo);
        tracing::info!(target: "kiln.sync", event = "sync.move", repo = %name, machine = %machine, phase = %news.phase, from = %news.from, to = %news.to, index = news.index, waited_ms = news.waited_ms, "{text}");
        self.note(&news.repo, Kind::Move, machine, tone, text);
    }

    /// A copy that was to take the group's files has: it is a copy like any other from now on, and
    /// is never told to again (a machine whose state was lost must not overwrite its files).
    fn settled(&mut self, repo: &str, member: u64) {
        let Some(c) = self.repos.iter_mut().filter(|r| r.id == repo).flat_map(|r| r.copies.iter_mut()).find(|c| c.member == member && c.adopt.is_some()) else { return };
        c.adopt = None;
        let machine = c.machine.clone();
        if let Err(e) = self.save() {
            tracing::warn!(error = %format!("{e:#}"), "the synced repos could not be saved");
        }
        self.note(repo, Kind::Copy, &machine, Tone::Ok, format!("the new copy on {machine} has the repo's files: it syncs like the others now"));
    }

    /// The members of `repo` whose copies are on `machine`.
    fn members_on(&self, repo: &str, machine: &str) -> Vec<u64> {
        self.repos.iter().filter(|r| r.id == repo).flat_map(|r| r.copies.iter()).filter(|c| c.machine == machine).map(|c| c.member).collect()
    }

    /// A machine's steps: each taken in its member's order once its contents are here; what is
    /// missing is asked for, and the steps come again.
    fn steps(&mut self, machine: &str, repo: &str, steps: Vec<Step>) {
        let mine = self.members_on(repo, machine);
        let Some(g) = self.groups.get_mut(repo) else { return };
        let mut want: Vec<String> = Vec::new();
        // What was let go (and why), and how many came again, taken already.
        let mut dropped: Vec<(&'static str, String)> = Vec::new();
        let mut again = 0u64;
        for mut step in steps {
            if !mine.contains(&step.member) {
                dropped.push(("not_member", format!("a step from {machine} for a copy that isn't on it: let go")));
                continue;
            }
            if step.changes.len() > STEP_CHANGES {
                dropped.push(("too_many_changes", format!("a step from {machine} of {} changes, more than {STEP_CHANGES}: let go", step.changes.len())));
                continue;
            }
            step.by = machine.to_owned();
            let taken = g.r.machine().tree.seqs.get(&step.member).copied().unwrap_or(0);
            let next = g.next.get(&step.member).copied().unwrap_or(0).max(taken.saturating_add(1));
            if step.seq < next {
                again += 1;
                continue;
            }
            if step.seq > next {
                // One before it went missing: they all come again.
                dropped.push(("out_of_order", format!("steps from {machine} came out of order (#{} before #{next}): they come again", step.seq)));
                break;
            }
            let lacking: Vec<String> = step
                .changes
                .iter()
                .flat_map(|c| {
                    let file = match &c.to {
                        Some(Version::File { hash, .. }) => Some(hash),
                        _ => None,
                    };
                    file.into_iter().chain(c.bundle.as_ref())
                })
                .filter(|hash| !self.contents.has(hash))
                .cloned()
                .collect();
            if !lacking.is_empty() {
                want.extend(lacking);
                break;
            }
            let entry = group::entry(&step);
            if entry.len() > ENTRY {
                dropped.push(("too_big", format!("a step from {machine} too big for the log ({} KiB, {} changes): left out", entry.len() >> 10, step.changes.len())));
                g.next.insert(step.member, step.seq.saturating_add(1));
                continue;
            }
            if let Err(e) = g.r.propose(Vec::new(), entry) {
                dropped.push((why(&e), format!("the log didn't take a step from {machine} ({e}): it comes again")));
                break;
            }
            if g.arrived.len() >= ARRIVED {
                g.arrived.clear();
            }
            g.arrived.insert((step.member, step.seq), Instant::now());
            g.next.insert(step.member, step.seq.saturating_add(1));
        }
        let name = self.name_of(repo);
        if again > 0 {
            metrics::sync().retries.add(&[&name, "step_again"], again);
            self.stats_of(repo).retries += again;
        }
        for (reason, text) in dropped {
            self.dropped(repo, machine, "step", reason, text);
        }
        if !want.is_empty() {
            want.sort();
            want.dedup();
            let n = want.len();
            metrics::sync().retries.add(&[&name, "contents_asked"], u64::try_from(n).unwrap_or(u64::MAX));
            self.stats_of(repo).retries += 1;
            self.note_quietly(repo, Kind::Retry, machine, "contents_asked", Tone::Info, format!("asked {machine} again for the contents of {n} file{} its steps name", if n == 1 { "" } else { "s" }));
            self.send(machine, Msg::Want { repo: repo.to_owned(), hashes: want }, Vec::new());
        }
    }

    fn send(&self, machine: &str, msg: Msg, raft: Vec<u8>) {
        let _ = self.out.try_send(Out { machine: machine.to_owned(), msg, raft });
    }

    /// Tells a machine every copy it holds.
    fn tell_copies(&self, machine: &str) {
        if !self.up.contains(machine) {
            return;
        }
        let copies: Vec<CopySpec> = self
            .repos
            .iter()
            .flat_map(|r| {
                r.copies.iter().filter(|c| c.machine == machine).map(|c| CopySpec {
                    repo: r.id.clone(),
                    name: r.name.clone(),
                    member: c.member,
                    path: c.path.clone(),
                    exclude: r.exclude.clone(),
                    max_size: None,
                    mode: r.mode,
                    adopt: c.adopt,
                })
            })
            .collect();
        self.send(machine, Msg::Copies { copies, token: token(&self.key, machine) }, Vec::new());
    }

    fn drive(&mut self) {
        self.wakeup.pending.store(false, Ordering::SeqCst);
        let now = u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let ids: Vec<String> = self.groups.keys().cloned().collect();
        let mut freed = false;
        for id in ids {
            let Some(repo) = self.repos.iter().find(|r| r.id == id).cloned() else { continue };
            let mut messages = Vec::new();
            let Some(g) = self.groups.get_mut(&id) else { continue };
            if let Err(e) = group::pump(&mut g.r, now, &self.waker, &mut g.out, &mut messages) {
                let why = format!("{e:?}");
                if g.error.as_deref() != Some(why.as_str()) {
                    tracing::warn!(target: "kiln.sync", event = "sync.error", repo = %repo.name, kind = "group", error = %why, "a synced repo's group stopped");
                    g.error = Some(why.clone());
                    self.changed = true;
                    metrics::sync().errors.inc(&[&repo.name, "group"]);
                    self.stats_of(&id).errors += 1;
                    self.note(&id, Kind::Error, "kiln", Tone::Fail, format!("the repo's log stopped: {why}"));
                }
                continue;
            }
            let taken = std::mem::take(&mut g.r.machine_mut().taken);
            let arrived: Vec<Option<Instant>> = taken.iter().map(|t| g.arrived.remove(&(t.step.member, t.step.seq))).collect();
            let mut offline = 0u64;
            for m in messages {
                let to = m.to;
                let Some(copy) = repo.copies.iter().find(|c| c.member == to) else { continue };
                if !self.up.contains(&copy.machine) {
                    let _ = g.r.report_unreachable(to);
                    offline += 1;
                    continue;
                }
                let snapshot = m.msg_type == MessageType::MsgSnapshot;
                self.part = self.part.wrapping_add(1);
                for (msg, raft) in wire::frames(&repo.id, self.part, m.encode_to_vec()) {
                    let _ = self.out.try_send(Out { machine: copy.machine.clone(), msg, raft });
                }
                if snapshot {
                    let _ = g.r.report_snapshot(to, true);
                }
            }
            Self::reconcile(g, &repo);
            if disk::due(&g.r, g.compacted) && g.r.compact(disk::keep(&g.r, g.compacted), now, &self.waker).unwrap_or(false) {
                g.compacted = g.r.applied().index;
                freed = true;
            }
            if offline > 0 {
                // A machine away misses its messages: the log sends them again once it's back.
                metrics::sync().dropped.add(&[&repo.name, "message", "offline"], offline);
            }
            if !taken.is_empty() {
                self.changed = true;
                self.committed(&repo, taken, &arrived);
            }
        }
        // Room in the shared log for any group stalled for it.
        if freed {
            for g in self.groups.values_mut() {
                g.r.resume();
            }
        }
        if self.swept.elapsed() >= SWEEP {
            self.swept = Instant::now();
            self.sweep();
        }
        if (self.changed && self.viewed.elapsed() >= VIEW_EVERY) || self.gauged.elapsed() >= GAUGE_EVERY {
            self.publish();
        }
        if self.saved.elapsed() >= SAVE_EVERY {
            self.save_activity();
        }
    }

    /// Saves the repos' new activity.
    fn save_activity(&mut self) {
        self.saved = Instant::now();
        if let Err(e) = lock(&self.activity).save(&self.dir) {
            tracing::warn!(error = %e, "the synced repos' activity could not be saved");
        }
    }

    /// Steps `repo`'s log committed, each with when it arrived (if the server saw it arrive): its
    /// counts, latency, conflicts, and its event in the repo's activity.
    fn committed(&mut self, repo: &Repo, taken: Vec<Taken>, arrived: &[Option<Instant>]) {
        let m = metrics::sync();
        let name = repo.name.as_str();
        let on_record = self.recorded.get(&repo.id).copied().unwrap_or(0);
        if let Some(last) = taken.last() {
            self.recorded.insert(repo.id.clone(), on_record.max(last.index));
        }
        for (t, came) in taken.iter().zip(arrived) {
            if t.index <= on_record {
                continue;
            }
            let commit = came.map(|c| c.elapsed());
            if let Some(d) = commit {
                m.commit_seconds.observe(&[name], d.as_secs_f64());
            }
            let contents = Arc::clone(&self.contents);
            let step = activity::step_event(t.index, &t.step, &t.decisions, commit.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)), |hash| contents.size(hash));
            m.steps_committed.inc(&[name]);
            for (kind, n) in [("added", step.added), ("changed", step.changed), ("deleted", step.deleted)] {
                if n > 0 {
                    m.changes.add(&[name, kind], u64::try_from(n).unwrap_or(u64::MAX));
                }
            }
            if step.conflicts > 0 {
                m.conflicts.add(&[name], u64::try_from(step.conflicts).unwrap_or(u64::MAX));
            }
            if step.refused > 0 {
                m.dropped.add(&[name, "change", "unsafe_path"], u64::try_from(step.refused).unwrap_or(u64::MAX));
            }
            for (c, d) in t.step.changes.iter().zip(&t.decisions) {
                if let Decision::Beside(beside) = d {
                    tracing::warn!(target: "kiln.sync", event = "sync.conflict", repo = %name, machine = %t.step.by, path = %c.path, beside = %beside.as_deref().unwrap_or(""), index = t.index, "another machine's change got there first: this one went beside it");
                }
            }
            tracing::info!(target: "kiln.sync", event = "sync.step", repo = %name, machine = %t.step.by, index = t.index, added = step.added, changed = step.changed, deleted = step.deleted, conflicts = step.conflicts, bytes = step.bytes, commit_ms = step.commit_ms.unwrap_or(0), "a step committed");
            let now = now_ms();
            let s = self.stats_of(&repo.id);
            s.steps += 1;
            s.files += u64::try_from(step.added + step.changed + step.deleted).unwrap_or(u64::MAX);
            s.bytes = s.bytes.saturating_add(step.bytes);
            s.conflicts += u64::try_from(step.conflicts).unwrap_or(u64::MAX);
            s.last_step = Some(now);
            let tone = if step.conflicts > 0 { Tone::Warn } else { Tone::Info };
            lock(&self.activity).push(Event { seq: 0, repo: repo.id.clone(), at: now, kind: Kind::Step, machine: t.step.by.clone(), text: String::new(), tone, step: Some(step), state: None });
        }
    }

    /// The view the API shows, made again now; each copy's change of state noted; the gauges
    /// measured.
    fn publish(&mut self) {
        self.changed = false;
        self.viewed = Instant::now();
        self.gauged = Instant::now();
        let view = self.make_view();
        self.watch(&view);
        *lock(&self.view) = view;
    }

    /// Each copy's state, as the view has it: a change worth knowing of noted in its repo's
    /// activity and logged (going offline and coming back, an error and getting over it; not the
    /// coming and going of steps), and every copy's gauges set.
    fn watch(&mut self, view: &View) {
        let m = metrics::sync();
        let (mut lag, mut unsent, mut waiting, mut state, mut age, mut conflicts) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        for r in &view.repos {
            conflicts.push((vec![r.name.clone()], i64::try_from(r.conflicts.len()).unwrap_or(i64::MAX)));
            for c in &r.copies {
                let key = (r.id.clone(), c.member);
                seen.insert(key.clone());
                let labels = vec![r.name.clone(), c.machine.clone()];
                let s = c.status.as_ref();
                lag.push((labels.clone(), i64::try_from(c.lag).unwrap_or(i64::MAX)));
                unsent.push((labels.clone(), s.map_or(0, |s| i64::try_from(s.unsent).unwrap_or(i64::MAX))));
                waiting.push((labels.clone(), s.map_or(0, |s| i64::try_from(s.waiting).unwrap_or(i64::MAX))));
                for st in STATES {
                    state.push((vec![r.name.clone(), c.machine.clone(), st.replace(' ', "_")], i64::from(st == c.state)));
                }
                if let Some((at, _)) = self.heard_at.get(&key) {
                    age.push((labels, i64::try_from(at.elapsed().as_secs()).unwrap_or(i64::MAX)));
                }
                let before = self.states.insert(key, c.state);
                let gone = (r.id.clone(), c.machine.clone());
                // Out of offline: news only for a copy noted going away, not one merely connecting.
                let before = before.filter(|b| *b != "offline" || self.away.contains(&gone));
                if let Some(text) = state_change(before, c.state, &c.machine, s.and_then(|s| s.error.as_deref())) {
                    let tone = match c.state {
                        "error" => Tone::Fail,
                        "offline" => Tone::Warn,
                        _ => Tone::Ok,
                    };
                    let from = before.unwrap_or("");
                    if tone == Tone::Ok {
                        tracing::info!(target: "kiln.sync", event = "sync.copy_state", repo = %r.name, machine = %c.machine, from = %from, to = %c.state, "{text}");
                    } else {
                        tracing::warn!(target: "kiln.sync", event = "sync.copy_state", repo = %r.name, machine = %c.machine, from = %from, to = %c.state, "{text}");
                    }
                    if c.state == "error" {
                        self.stats_of(&r.id).errors += 1;
                    }
                    if c.state == "offline" {
                        self.away.insert(gone);
                    } else {
                        self.away.remove(&gone);
                    }
                    lock(&self.activity).push(Event { seq: 0, repo: r.id.clone(), at: now_ms(), kind: Kind::State, machine: c.machine.clone(), text, tone, step: None, state: Some(c.state.to_owned()) });
                }
            }
        }
        // Copies no longer synced: forgotten.
        self.states.retain(|k, _| seen.contains(k));
        self.heard_at.retain(|k, _| seen.contains(k));
        // Its own repos' series: what it published before (a repo gone takes its series with it)
        // and now; another server's in this process kept.
        let names: BTreeSet<String> = view.repos.iter().map(|r| r.name.clone()).collect();
        let owned: BTreeSet<String> = self.gauged_repos.union(&names).cloned().collect();
        m.copy_lag_steps.replace_owned(&owned, lag);
        m.copy_unsent_steps.replace_owned(&owned, unsent);
        m.copy_waiting_blobs.replace_owned(&owned, waiting);
        m.copy_state.replace_owned(&owned, state);
        m.copy_heard_age.replace_owned(&owned, age);
        m.conflict_files.replace_owned(&owned, conflicts);
        self.gauged_repos = names;
    }

    /// The repo's name, as metrics and logs label it.
    fn name_of(&self, repo: &str) -> String {
        self.repos.iter().find(|r| r.id == repo).map_or_else(|| repo.to_owned(), |r| r.name.clone())
    }

    fn stats_of(&mut self, repo: &str) -> &mut RepoStats {
        let since = self.started;
        self.stats.entry(repo.to_owned()).or_insert_with(|| RepoStats { since, ..RepoStats::default() })
    }

    /// Something happened in `repo`: in its activity.
    fn note(&self, repo: &str, kind: Kind, machine: &str, tone: Tone, text: String) {
        lock(&self.activity).push(Event { seq: 0, repo: repo.to_owned(), at: now_ms(), kind, machine: machine.to_owned(), text, tone, step: None, state: None });
    }

    /// As [`Owner::note`], once in [`QUIET`] for the same trouble (`what`) from the same machine:
    /// whether it was noted.
    fn note_quietly(&mut self, repo: &str, kind: Kind, machine: &str, what: &'static str, tone: Tone, text: String) -> bool {
        let key = (repo.to_owned(), machine.to_owned(), what);
        if self.quiet.get(&key).is_some_and(|t| t.elapsed() < QUIET) {
            return false;
        }
        if self.quiet.len() > 1024 {
            self.quiet.retain(|_, t| t.elapsed() < QUIET);
        }
        self.quiet.insert(key, Instant::now());
        self.note(repo, kind, machine, tone, text);
        true
    }

    /// The server let go of a step or a message from `machine` (`what`) for `reason`: counted
    /// always; noted and logged once in a while.
    fn dropped(&mut self, repo: &str, machine: &str, what: &'static str, reason: &'static str, text: String) {
        let name = self.name_of(repo);
        metrics::sync().dropped.inc(&[&name, what, reason]);
        if !self.repos.iter().any(|r| r.id == repo) {
            return;
        }
        self.stats_of(repo).dropped += 1;
        // Out of order or refused for now, it comes again: worth knowing, not alarming.
        let tone = if matches!(reason, "out_of_order" | "stalled" | "budget") { Tone::Info } else { Tone::Warn };
        if self.note_quietly(repo, Kind::Dropped, machine, reason, tone, text.clone()) {
            tracing::warn!(target: "kiln.sync", event = "sync.dropped", repo = %name, machine = %machine, what, reason, "{text}");
        }
    }

    /// Brings the group's learners to the repo's copies, one change at a time.
    fn reconcile(g: &mut Group, repo: &Repo) {
        if !g.r.is_leader() {
            return;
        }
        let have: Vec<u64> = g.r.configuration().learners.clone();
        if let Some((member, add, at)) = g.changing {
            if have.contains(&member) == add {
                g.changing = None;
            } else if at.elapsed() < CHANGE_AGAIN {
                return;
            }
        }
        let want: Vec<u64> = repo.copies.iter().map(|c| c.member).collect();
        let next = want.iter().find(|m| !have.contains(m)).map(|m| (*m, true)).or_else(|| have.iter().find(|m| !want.contains(m)).map(|m| (*m, false)));
        if let Some((member, add)) = next
            && g.r.change(Vec::new(), &group::learner(member, add)).is_ok()
        {
            g.changing = Some((member, add, Instant::now()));
        }
    }

    fn ask(&mut self, ask: Ask) -> anyhow::Result<String> {
        match ask {
            Ask::Create { name, copies, exclude, mode, transient } => {
                let name = name.trim().to_owned();
                anyhow::ensure!(!name.is_empty(), "a synced repo needs a name");
                anyhow::ensure!(!self.repos.iter().any(|r| r.name == name), "a synced repo named {name} exists already");
                anyhow::ensure!(self.repos.len() < disk::GROUPS, "this server syncs {} repos at the most", disk::GROUPS);
                let group = self.repos.iter().map(|r| r.group).max().unwrap_or(0).saturating_add(1);
                let mut repo = Repo { id: crate::util::new_id(), name, group, created: now_s(), next_member: SERVER.saturating_add(1), copies: Vec::new(), exclude, mode, transient };
                for (machine, path) in copies {
                    add_copy(&mut repo, &machine, &path)?;
                }
                self.open_group(&repo)?;
                let machines: Vec<String> = repo.copies.iter().map(|c| c.machine.clone()).collect();
                let id = repo.id.clone();
                self.repos.push(repo);
                self.save()?;
                let on = if machines.is_empty() { "no copies yet".to_owned() } else { format!("copies on {}", machines.join(", ")) };
                self.note(&id, Kind::Copy, "kiln", Tone::Info, format!("syncing started: {on}"));
                for m in machines {
                    self.tell_copies(&m);
                }
                Ok(id)
            }
            Ask::AddCopy { repo, machine, path, adopt } => {
                // What the folder takes the group's files through: everything committed now.
                let index = self.groups.get(&repo).map_or(0, |g| g.r.applied().index);
                let r = self.repos.iter_mut().find(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let member = add_copy(r, &machine, &path)?;
                let adopting = adopt && index > 0;
                if adopting && let Some(c) = r.copies.iter_mut().find(|c| c.member == member) {
                    c.adopt = Some(index);
                }
                self.save()?;
                let first = if adopting { ": it takes the repo's files first" } else { "" };
                self.note(&repo, Kind::Copy, &machine, Tone::Info, format!("a copy added on {machine}, in {path}{first}"));
                self.tell_copies(&machine);
                Ok(member.to_string())
            }
            Ask::RemoveCopy { repo, member } => {
                let r = self.repos.iter_mut().find(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let at = r.copies.iter().position(|c| c.member == member).ok_or_else(|| anyhow::anyhow!("no such copy"))?;
                let copy = r.copies.remove(at);
                self.statuses.remove(&(repo.clone(), member));
                self.save()?;
                self.note(&repo, Kind::Copy, &copy.machine, Tone::Info, format!("the copy on {} was removed: its files stay in {}", copy.machine, copy.path));
                self.tell_copies(&copy.machine);
                Ok(String::new())
            }
            Ask::Delete { repo } => {
                let at = self.repos.iter().position(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let r = self.repos.remove(at);
                self.save()?;
                self.groups.remove(&r.id);
                disk::remove(&self.log, u128::from(r.group))?;
                let _ = std::fs::remove_file(self.dir.join(format!("{}.tree.json", r.id)));
                self.statuses.retain(|(id, _), _| *id != r.id);
                lock(&self.activity).forget(&self.dir, &r.id);
                self.stats.remove(&r.id);
                self.away.retain(|(id, _)| *id != r.id);
                self.recorded.remove(&r.id);
                let machines: HashSet<String> = r.copies.iter().map(|c| c.machine.clone()).collect();
                for m in machines {
                    self.tell_copies(&m);
                }
                Ok(String::new())
            }
            Ask::Resolve { repo, path, keep } => {
                let g = self.groups.get_mut(&repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let tree = &g.r.machine().tree;
                let beside = group::conflicts(tree).remove(&path).ok_or_else(|| anyhow::anyhow!("{path} is no conflict copy"))?;
                let theirs = tree.files.get(&path).cloned();
                let ours = tree.files.get(&beside).cloned();
                let gone = Change { path: path.clone(), to: None, base: theirs.clone(), bundle: None };
                let (changes, said) = match keep {
                    Keep::Name => (vec![gone], format!("{beside} kept its version; the conflict copy {path} went")),
                    Keep::Copy => (
                        vec![Change { path: beside.clone(), to: theirs, base: ours, bundle: tree.bundles.get(&path).cloned() }, gone],
                        format!("{beside} took the conflict copy's version; {path} went"),
                    ),
                    Keep::Both => {
                        let kept = path.replacen(".conflict-", ".kept-", 1);
                        let said = format!("both kept: {beside} as it was, and the conflict copy as {kept}");
                        (vec![Change { path: kept.clone(), to: theirs, base: tree.files.get(&kept).cloned(), bundle: tree.bundles.get(&path).cloned() }, gone], said)
                    }
                };
                let taken = tree.seqs.get(&SERVER).copied().unwrap_or(0);
                let seq = g.next.get(&SERVER).copied().unwrap_or(0).max(taken.saturating_add(1));
                let step = Step { by: "kiln".into(), member: SERVER, seq, at: now_ms(), changes };
                g.r.propose(Vec::new(), group::entry(&step)).map_err(|e| anyhow::anyhow!("the repo's log would not take it now: {e:?}"))?;
                g.arrived.insert((SERVER, seq), Instant::now());
                g.next.insert(SERVER, seq.saturating_add(1));
                self.note(&repo, Kind::Resolved, "kiln", Tone::Ok, format!("a conflict resolved: {said}"));
                Ok(String::new())
            }
        }
    }

    /// Drops contents no repo's tree names, once they are old enough that no copy far behind
    /// still needs them (one that does is told they are gone, and takes the later version).
    fn sweep(&self) {
        let named: HashSet<&str> = self
            .groups
            .values()
            .flat_map(|g| {
                let tree = &g.r.machine().tree;
                let files = tree.files.values().filter_map(|v| match v {
                    Version::File { hash, .. } => Some(hash.as_str()),
                    Version::Link { .. } | Version::Ref { .. } => None,
                });
                files.chain(tree.bundles.values().map(String::as_str))
            })
            .collect();
        let old = crate::util::now_ms().saturating_sub(i64::try_from(SWEEP_AGE.as_millis()).unwrap_or(i64::MAX));
        for (hash, kept) in self.contents.all() {
            if !named.contains(hash.as_str()) && kept <= old {
                self.contents.remove(&hash);
            }
        }
    }

    fn make_view(&self) -> View {
        let repos = self
            .repos
            .iter()
            .map(|r| {
                let g = self.groups.get(&r.id);
                let index = g.map_or(0, |g| g.r.applied().index);
                let conflicts = g
                    .map(|g| group::conflicts(&g.r.machine().tree).into_iter().map(|(path, beside)| ConflictView { path, beside }).collect())
                    .unwrap_or_default();
                let copies = r
                    .copies
                    .iter()
                    .map(|c| {
                        let status = self.statuses.get(&(r.id.clone(), c.member)).cloned();
                        let state = match &status {
                            _ if !self.up.contains(&c.machine) => "offline",
                            None => "starting",
                            Some(s) if s.error.is_some() => "error",
                            Some(s) if s.waiting > 0 => "fetching",
                            Some(s) if s.behind > 0 || s.applied < index => "catching up",
                            Some(s) if s.unsent > 0 => "sending",
                            Some(_) => "in step",
                        };
                        let heard = self.heard_at.get(&(r.id.clone(), c.member)).map(|(_, ms)| *ms);
                        let lag = status.as_ref().map_or(0, |s| index.saturating_sub(s.applied));
                        CopyView { member: c.member, machine: c.machine.clone(), path: c.path.clone(), added: c.added, state, status, heard, lag }
                    })
                    .collect();
                RepoView {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    created: r.created,
                    exclude: r.exclude.clone(),
                    mode: r.mode,
                    files: g.map_or(0, |g| g.r.machine().tree.files.keys().filter(|p| !super::refs::is_ref(p)).count()),
                    index,
                    conflicts,
                    copies,
                    error: match g {
                        None => Some("its group did not open".to_owned()),
                        Some(g) => g.error.clone(),
                    },
                    transient: r.transient.clone(),
                    stats: self.stats.get(&r.id).cloned().unwrap_or_else(|| RepoStats { since: self.started, ..RepoStats::default() }),
                }
            })
            .collect();
        View { repos, error: None }
    }
}

/// A refusal of a repo's log, as the metrics' `reason` names it.
fn why(e: &ReplicaError) -> &'static str {
    match e {
        ReplicaError::Refused(_) => "refused",
        ReplicaError::Stalled => "stalled",
        ReplicaError::Marked => "marked",
        ReplicaError::Budget(_) => "budget",
        ReplicaError::Fenced(_) => "fenced",
    }
}

/// A copy's change of state worth noting, in words: going offline and coming back, an error and
/// getting over it. Not a copy first seen, nor the coming and going of steps.
fn state_change(before: Option<&str>, now: &str, machine: &str, error: Option<&str>) -> Option<String> {
    let before = before?;
    if before == now {
        return None;
    }
    match (before, now) {
        (_, "offline") => Some(format!("{machine} went offline: its copy takes what it missed once it's back")),
        (_, "error") => Some(format!("{machine}'s copy has an error: {}", error.unwrap_or("it didn't say what"))),
        ("offline", _) => Some(format!("{machine} is back ({now})")),
        ("error", _) => Some(format!("{machine}'s copy is over its error ({now})")),
        _ => None,
    }
}

/// Milliseconds, as a person reads them.
fn secs_text(ms: u64) -> String {
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..60_000 => format!("{:.1} s", Duration::from_millis(ms).as_secs_f64()),
        _ => format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1_000),
    }
}

/// An id's first characters, as the activity names a session.
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Adds a copy at `path` on `machine` to `repo`: its member.
fn add_copy(repo: &mut Repo, machine: &str, path: &str) -> anyhow::Result<u64> {
    let (machine, path) = (machine.trim(), path.trim());
    anyhow::ensure!(!machine.is_empty(), "a copy needs a machine");
    anyhow::ensure!(path.starts_with('/') || path.get(1..3) == Some(":\\") || path.get(1..3) == Some(":/"), "a copy's folder is a full path, not {path:?}");
    anyhow::ensure!(!repo.copies.iter().any(|c| c.machine == machine && c.path == path), "{machine}:{path} is a copy already");
    let member = repo.next_member;
    repo.next_member = member.saturating_add(1);
    repo.copies.push(CopyRef { member, machine: machine.to_owned(), path: path.to_owned(), added: now_s(), adopt: None });
    Ok(member)
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Owner").field("repos", &self.repos.len()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `f` comes to hold within ten seconds.
    fn eventually(mut f: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_committed_step_shows_with_its_files_and_a_stray_one_as_let_go() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let contents = Contents::empty(Arc::new(kiln_seams::blobs::SimBlobs::default()));
        let sync = Sync::start(dir.path().join("sync"), "key".into(), contents);
        let name = "activity-test";
        let create = Ask::Create {
            name: name.into(),
            copies: vec![("mac".into(), "/src/kiln".into()), ("box".into(), "/src/kiln".into())],
            exclude: Vec::new(),
            mode: super::super::Mode::Files,
            transient: None,
        };
        let id = futures::executor::block_on(sync.ask(create)).map_err(anyhow::Error::msg)?;
        let hash = futures::executor::block_on(sync.contents().put_bytes(b"hello".to_vec()))?;
        let step =
            Step { by: String::new(), member: 2, seq: 1, at: now_ms(), changes: vec![Change { path: "src/a.txt".into(), to: Some(Version::File { hash, exec: false }), base: None, bundle: None }] };
        // From a machine with no copy of it: let go.
        sync.from_runner("elsewhere", Msg::Steps { repo: id.clone(), steps: vec![step.clone()] }, Vec::new());
        // Sent again until taken, as a runner does.
        let shown = eventually(|| {
            sync.from_runner("mac", Msg::Steps { repo: id.clone(), steps: vec![step.clone()] }, Vec::new());
            sync.activity(0, Some(&id), 50).0.iter().any(|e| e.kind == Kind::Step)
        });
        anyhow::ensure!(shown, "the step shows in the activity");
        let (events, next) = sync.activity(0, Some(&id), 50);
        let e = events.iter().find(|e| e.kind == Kind::Step).ok_or_else(|| anyhow::anyhow!("no step"))?;
        let s = e.step.as_ref().ok_or_else(|| anyhow::anyhow!("a step event without its step"))?;
        assert_eq!(e.machine, "mac", "the machine that made it");
        assert_eq!((s.added, s.changed, s.deleted, s.bytes), (1, 0, 0, 5));
        assert_eq!(s.files.first().map(|f| (f.path.as_str(), f.size)), Some(("src/a.txt", Some(5))));
        assert!(s.commit_ms.is_some(), "its commit was timed");
        assert!(events.iter().any(|e| e.kind == Kind::Dropped && e.machine == "elsewhere"), "the stray step is noted: {events:?}");
        assert!(events.iter().any(|e| e.kind == Kind::Copy), "the start is noted");
        assert!(next > e.seq);
        assert!(sync.activity(next, None, 50).0.is_empty(), "nothing after the newest");
        let counted = eventually(|| sync.view().repos.iter().any(|r| r.id == id && r.stats.steps == 1 && r.stats.files == 1 && r.stats.dropped >= 1));
        anyhow::ensure!(counted, "the view counts them: {:?}", sync.view().repos.first().map(|r| r.stats.clone()));
        let text = crate::metrics::render();
        assert!(text.contains(&format!("kiln_sync_steps_committed_total{{repo=\"{name}\"}} 1")), "committed");
        assert!(text.contains(&format!("kiln_sync_changes_total{{repo=\"{name}\",kind=\"added\"}} 1")), "its change");
        assert!(text.contains(&format!("kiln_sync_dropped_total{{repo=\"{name}\",what=\"step\",reason=\"not_member\"}} 1")), "the stray step");
        assert!(text.contains(&format!("kiln_sync_commit_seconds_count{{repo=\"{name}\"}} 1")), "its commit timed");
        // No runner is connected: each copy is offline, and every other state reads 0.
        let states = eventually(|| {
            let t = crate::metrics::render();
            t.contains(&format!("kiln_sync_copy_state{{repo=\"{name}\",machine=\"mac\",state=\"offline\"}} 1")) && t.contains(&format!("kiln_sync_copy_state{{repo=\"{name}\",machine=\"mac\",state=\"in_step\"}} 0"))
        });
        anyhow::ensure!(states, "every state of each copy, 1 for the one it's in");
        Ok(())
    }

    #[test]
    fn only_a_copy_going_away_or_wrong_and_coming_back_is_news() {
        assert_eq!(state_change(None, "offline", "mac", None), None, "first seen");
        assert_eq!(state_change(Some("in step"), "sending", "mac", None), None, "steps coming and going");
        assert_eq!(state_change(Some("catching up"), "in step", "mac", None), None);
        assert!(state_change(Some("in step"), "offline", "mac", None).is_some_and(|t| t.contains("went offline")));
        assert!(state_change(Some("offline"), "catching up", "mac", None).is_some_and(|t| t.contains("back")));
        assert!(state_change(Some("in step"), "error", "mac", Some("disk full")).is_some_and(|t| t.contains("disk full")));
        assert!(state_change(Some("error"), "in step", "mac", None).is_some_and(|t| t.contains("over its error")));
    }

    #[test]
    fn waits_read_as_a_person_says_them() {
        assert_eq!(secs_text(340), "340 ms");
        assert_eq!(secs_text(2_500), "2.5 s");
        assert_eq!(secs_text(125_000), "2 min 5 s");
    }

    #[test]
    fn a_copy_is_a_full_path_on_a_named_machine() {
        let mut r = Repo { id: "r".into(), name: "n".into(), group: 1, created: 0, next_member: 2, copies: Vec::new(), exclude: Vec::new(), mode: super::super::Mode::Files, transient: None };
        assert_eq!(add_copy(&mut r, "mac", "/Users/a/src/kiln").ok(), Some(2));
        assert_eq!(add_copy(&mut r, "box", "C:\\src\\kiln").ok(), Some(3));
        assert!(add_copy(&mut r, "mac", "/Users/a/src/kiln").is_err(), "twice");
        assert!(add_copy(&mut r, "mac", "src/kiln").is_err(), "relative");
        assert!(add_copy(&mut r, "", "/x").is_err());
        assert_eq!(r.next_member, 4);
    }
}
