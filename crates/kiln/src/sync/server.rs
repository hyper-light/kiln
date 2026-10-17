//! The server's side of repo sync (`docs/sync.md`): every synced repo, the leader of its group, the
//! contents its steps name, and what each copy says of itself. One thread owns the groups and their
//! log ([`Owner`]); the hub hands it what runners say, the API what admins ask, and it hands back
//! what to send ([`Out`]), which the hub sends on the sync lane.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant, SystemTime};

use hyper_durable::{LogStore as _, Output, Replica, Unbounded};
use hyper_raft::proto::{Message, MessageType};
use hyper_raft::wire::Record as _;
use serde::{Deserialize, Serialize};

use super::blobs::Blobs;
use super::disk::{self, Store, SyncLog};
use super::group::{self, Index, SERVER, TICK};
use super::wire::{self, CopySpec, CopyStatus, Joiner, Msg};
use super::{Change, STEP_CHANGES, Step, Version};

/// A log entry's bytes at the most (the log holds one up to 1 MiB).
const ENTRY: usize = 900 << 10;
/// A change of configuration not seen applied by then is proposed again.
const CHANGE_AGAIN: Duration = Duration::from_secs(10);
/// How often contents no tree names any more are looked for, and how old they must be to go.
const SWEEP: Duration = Duration::from_secs(600);
const SWEEP_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// How often the view the API shows is made again, at the most.
const VIEW_EVERY: Duration = Duration::from_millis(500);

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
}

/// A copy of a repo: a machine and a folder on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyRef {
    pub member: u64,
    pub machine: String,
    pub path: String,
    pub added: i64,
}

/// What the API shows of a repo.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoView {
    pub id: String,
    pub name: String,
    pub created: i64,
    pub exclude: Vec<String>,
    /// Files in the repo as the log has it.
    pub files: usize,
    /// Through where the server's member applied the log.
    pub index: u64,
    pub conflicts: Vec<ConflictView>,
    pub copies: Vec<CopyView>,
    pub error: Option<String>,
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
    Create { name: String, copies: Vec<(String, String)>, exclude: Vec<String> },
    AddCopy { repo: String, machine: String, path: String },
    RemoveCopy { repo: String, member: u64 },
    Delete { repo: String },
    Resolve { repo: String, path: String, keep: Keep },
}

enum Input {
    Wake,
    From { machine: String, msg: Msg, raft: Vec<u8> },
    Machine { machine: String, up: bool },
    Ask { ask: Ask, reply: futures::channel::oneshot::Sender<Result<String, String>> },
}

/// The hub's handle on the server's side of sync.
pub struct Sync {
    tx: Sender<Input>,
    view: Arc<Mutex<View>>,
    out: async_channel::Receiver<Out>,
    blobs: Option<Arc<Blobs>>,
    key: String,
}

impl Sync {
    /// Starts the owner of the groups kept in `dir`. Blob tokens are signed with `key`.
    pub fn start(dir: PathBuf, key: String) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let (out_tx, out) = async_channel::unbounded();
        let view = Arc::new(Mutex::new(View::default()));
        let blobs = Blobs::new(&dir.join("blobs")).map(Arc::new);
        let blobs = match blobs {
            Ok(b) => Some(b),
            Err(e) => {
                lock(&view).error = Some(format!("sync is off: its contents folder: {e}"));
                None
            }
        };
        if let Some(b) = blobs.clone() {
            let (v, wake_tx) = (view.clone(), tx.clone());
            let k = key.clone();
            let spawned = std::thread::Builder::new().name("kiln-sync".into()).spawn(move || match Owner::open(&dir, b, out_tx, v.clone(), wake_tx, k) {
                Ok(owner) => owner.run(&rx),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "repo sync is off");
                    lock(&v).error = Some(format!("sync is off: {e:#}"));
                }
            });
            if let Err(e) = spawned {
                lock(&view).error = Some(format!("sync is off: {e}"));
            }
        }
        Self { tx, view, out, blobs, key }
    }

    pub fn view(&self) -> View {
        lock(&self.view).clone()
    }

    /// The server's contents.
    pub fn blobs(&self) -> Option<&Blobs> {
        self.blobs.as_deref()
    }

    /// What a machine's runner said (`AgentMsg::Sync`).
    pub fn from_runner(&self, machine: &str, json: &str, raft: Vec<u8>) {
        if let Some(msg) = wire::decode(json) {
            let _ = self.tx.send(Input::From { machine: machine.to_owned(), msg, raft });
        }
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
    blobs: Arc<Blobs>,
    key: String,
    start: Instant,
    swept: Instant,
}

impl Owner {
    fn open(dir: &Path, blobs: Arc<Blobs>, out: async_channel::Sender<Out>, view: Arc<Mutex<View>>, tx: Sender<Input>, key: String) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let repos: Vec<Repo> = match std::fs::read(dir.join("repos.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let log = disk::open(&dir.join("sync.log"))?;
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
            blobs,
            key,
            start: Instant::now(),
            swept: Instant::now(),
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
        self.groups.insert(repo.id.clone(), Group { r, out: Output::default(), compacted, next: HashMap::new(), changing: None, error: None });
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
                let _ = reply.send(answer);
                self.changed = true;
            }
        }
    }

    fn heard(&mut self, machine: &str, msg: Msg, raft: Vec<u8>) {
        match msg {
            Msg::Raft { repo, id, part, parts } => {
                let Some(bytes) = self.joiner.add(machine, id, part, parts, raft) else { return };
                let Ok(m) = Message::decode(&bytes) else { return };
                let mine = self.members_on(&repo, machine);
                if m.to != SERVER || !mine.contains(&m.from) {
                    return;
                }
                if let Some(g) = self.groups.get_mut(&repo) {
                    let _ = g.r.step(m);
                }
            }
            Msg::Steps { repo, steps } => self.steps(machine, &repo, steps),
            Msg::Status(status) => {
                if self.members_on(&status.repo, machine).contains(&status.member) {
                    self.statuses.insert((status.repo.clone(), status.member), status);
                    self.changed = true;
                }
            }
            // Only the server says these.
            Msg::Copies { .. } | Msg::Release | Msg::Want { .. } => {}
        }
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
        for mut step in steps {
            if !mine.contains(&step.member) || step.changes.len() > STEP_CHANGES {
                continue;
            }
            step.by = machine.to_owned();
            let taken = g.r.machine().tree.seqs.get(&step.member).copied().unwrap_or(0);
            let next = g.next.get(&step.member).copied().unwrap_or(0).max(taken.saturating_add(1));
            if step.seq < next {
                continue;
            }
            if step.seq > next {
                // One before it went missing: they all come again.
                break;
            }
            let lacking: Vec<String> = step
                .changes
                .iter()
                .filter_map(|c| match &c.to {
                    Some(Version::File { hash, .. }) if !self.blobs.has(hash) => Some(hash.clone()),
                    _ => None,
                })
                .collect();
            if !lacking.is_empty() {
                want.extend(lacking);
                break;
            }
            let entry = group::entry(&step);
            if entry.len() > ENTRY {
                tracing::warn!(%repo, %machine, bytes = entry.len(), "a step too big for the log was left out");
                g.next.insert(step.member, step.seq.saturating_add(1));
                continue;
            }
            if g.r.propose(Vec::new(), entry).is_err() {
                break;
            }
            g.next.insert(step.member, step.seq.saturating_add(1));
        }
        if !want.is_empty() {
            want.sort();
            want.dedup();
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
                    tracing::warn!(repo = %repo.name, error = %why, "a synced repo's group stopped");
                    g.error = Some(why);
                    self.changed = true;
                }
                continue;
            }
            if !g.r.machine().taken.is_empty() {
                g.r.machine_mut().taken.clear();
                self.changed = true;
            }
            for m in messages {
                let to = m.to;
                let Some(copy) = repo.copies.iter().find(|c| c.member == to) else { continue };
                if !self.up.contains(&copy.machine) {
                    let _ = g.r.report_unreachable(to);
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
        if self.changed && self.viewed.elapsed() >= VIEW_EVERY {
            self.changed = false;
            self.viewed = Instant::now();
            let view = self.make_view();
            *lock(&self.view) = view;
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
            Ask::Create { name, copies, exclude } => {
                let name = name.trim().to_owned();
                anyhow::ensure!(!name.is_empty(), "a synced repo needs a name");
                anyhow::ensure!(!self.repos.iter().any(|r| r.name == name), "a synced repo named {name} exists already");
                anyhow::ensure!(self.repos.len() < disk::GROUPS, "this server syncs {} repos at the most", disk::GROUPS);
                let group = self.repos.iter().map(|r| r.group).max().unwrap_or(0).saturating_add(1);
                let mut repo = Repo { id: crate::util::new_id(), name, group, created: now_s(), next_member: SERVER.saturating_add(1), copies: Vec::new(), exclude };
                for (machine, path) in copies {
                    add_copy(&mut repo, &machine, &path)?;
                }
                self.open_group(&repo)?;
                let machines: Vec<String> = repo.copies.iter().map(|c| c.machine.clone()).collect();
                let id = repo.id.clone();
                self.repos.push(repo);
                self.save()?;
                for m in machines {
                    self.tell_copies(&m);
                }
                Ok(id)
            }
            Ask::AddCopy { repo, machine, path } => {
                let r = self.repos.iter_mut().find(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let member = add_copy(r, &machine, &path)?;
                self.save()?;
                self.tell_copies(&machine);
                Ok(member.to_string())
            }
            Ask::RemoveCopy { repo, member } => {
                let r = self.repos.iter_mut().find(|r| r.id == repo).ok_or_else(|| anyhow::anyhow!("no such synced repo"))?;
                let at = r.copies.iter().position(|c| c.member == member).ok_or_else(|| anyhow::anyhow!("no such copy"))?;
                let copy = r.copies.remove(at);
                self.statuses.remove(&(repo, member));
                self.save()?;
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
                let gone = Change { path: path.clone(), to: None, base: theirs.clone() };
                let changes = match keep {
                    Keep::Name => vec![gone],
                    Keep::Copy => vec![Change { path: beside, to: theirs, base: ours }, gone],
                    Keep::Both => {
                        let kept = path.replacen(".conflict-", ".kept-", 1);
                        vec![Change { path: kept.clone(), to: theirs, base: tree.files.get(&kept).cloned() }, gone]
                    }
                };
                let taken = tree.seqs.get(&SERVER).copied().unwrap_or(0);
                let seq = g.next.get(&SERVER).copied().unwrap_or(0).max(taken.saturating_add(1));
                let step = Step { by: "kiln".into(), member: SERVER, seq, at: now_ms(), changes };
                g.r.propose(Vec::new(), group::entry(&step)).map_err(|e| anyhow::anyhow!("the repo's log would not take it now: {e:?}"))?;
                g.next.insert(SERVER, seq.saturating_add(1));
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
            .flat_map(|g| g.r.machine().tree.files.values())
            .filter_map(|v| match v {
                Version::File { hash, .. } => Some(hash.as_str()),
                Version::Link { .. } => None,
            })
            .collect();
        let Ok(all) = self.blobs.all() else { return };
        for (hash, modified) in all {
            if !named.contains(hash.as_str()) && modified.elapsed().unwrap_or_default() >= SWEEP_AGE {
                let _ = self.blobs.remove(&hash);
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
                        CopyView { member: c.member, machine: c.machine.clone(), path: c.path.clone(), added: c.added, state, status }
                    })
                    .collect();
                RepoView {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    created: r.created,
                    exclude: r.exclude.clone(),
                    files: g.map_or(0, |g| g.r.machine().tree.files.len()),
                    index,
                    conflicts,
                    copies,
                    error: match g {
                        None => Some("its group did not open".to_owned()),
                        Some(g) => g.error.clone(),
                    },
                }
            })
            .collect();
        View { repos, error: None }
    }
}

/// Adds a copy at `path` on `machine` to `repo`: its member.
fn add_copy(repo: &mut Repo, machine: &str, path: &str) -> anyhow::Result<u64> {
    let (machine, path) = (machine.trim(), path.trim());
    anyhow::ensure!(!machine.is_empty(), "a copy needs a machine");
    anyhow::ensure!(path.starts_with('/') || path.get(1..3) == Some(":\\") || path.get(1..3) == Some(":/"), "a copy's folder is a full path, not {path:?}");
    anyhow::ensure!(!repo.copies.iter().any(|c| c.machine == machine && c.path == path), "{machine}:{path} is a copy already");
    let member = repo.next_member;
    repo.next_member = member.saturating_add(1);
    repo.copies.push(CopyRef { member, machine: machine.to_owned(), path: path.to_owned(), added: now_s() });
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

    #[test]
    fn a_copy_is_a_full_path_on_a_named_machine() {
        let mut r = Repo { id: "r".into(), name: "n".into(), group: 1, created: 0, next_member: 2, copies: Vec::new(), exclude: Vec::new() };
        assert_eq!(add_copy(&mut r, "mac", "/Users/a/src/kiln").ok(), Some(2));
        assert_eq!(add_copy(&mut r, "box", "C:\\src\\kiln").ok(), Some(3));
        assert!(add_copy(&mut r, "mac", "/Users/a/src/kiln").is_err(), "twice");
        assert!(add_copy(&mut r, "mac", "src/kiln").is_err(), "relative");
        assert!(add_copy(&mut r, "", "/x").is_err());
        assert_eq!(r.next_member, 4);
    }
}
