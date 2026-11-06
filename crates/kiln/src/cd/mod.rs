//! Deployments, inside the server: kiln-cd's engine with the I/O it asks for.
//!
//! The engine is a state machine with no clock, disk or network (`kiln_cd::engine`). It runs
//! on a thread of its own, since its work is CPU work and hyper-rt shards are never preempted;
//! what it asks for is done here, on the runtime: renders on the blocking pool, watches and API
//! requests as tasks, records into the durable store (hyper-raft, `kiln_cd::store`). Sources are
//! polled from bare mirrors, and an app's render key is the content address of what its render
//! reads, so a commit that doesn't touch an app doesn't render it again.
//!
//! The service registers itself in [`get`] rather than on the hub, so the server's busiest files
//! carry one line each for it.

pub mod api;
pub mod artifacts;
pub mod checks;
pub mod cli;
pub mod custody;
pub mod guard;
pub mod images;
pub mod kube;
pub mod live;
pub mod logs;
pub mod ops;
pub mod plans;
pub mod rollback;
pub mod rollouts;
pub mod runners;
pub mod targets;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use kiln_cd::apps::{self, AppDef, ArtifactRef, Origin};
use kiln_cd::engine::{Action, Config as EngineConfig, Engine, Input, LiveChange, Record, Summary};
use kiln_cd::git::{Auth as GitAuth, Mirror};
use kiln_cd::model::Tracking;
use kiln_cd::obj::{self, GroupKind, ResKey};
use kiln_cd::render::{self, Dep, Kind, Tools};
use kiln_cd::state::{ClusterAuth, ClusterDef, Command, Deployment};
use kiln_cd::store::Store;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};

use crate::ci::integrations::{self, oci};
use crate::hub::Hub;
use kube::Cluster;

static CD: RwLock<Option<Arc<Cd>>> = RwLock::new(None);

/// The running service, once it has started.
pub fn get() -> Option<Arc<Cd>> {
    CD.read().clone()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// What the engine thread is sent.
pub enum ToEngine {
    /// An input.
    Input(Box<Input>),
    /// A question, answered on the engine's thread.
    Query(Box<dyn FnOnce(&Engine) + Send>),
    /// Health rules: Argo CD's Lua library, and these settings' scripts
    /// (`resource.customizations.health.<group>_<kind>`, the script).
    Health(Vec<(String, String)>),
}

/// The bytes of what the engine is told and hasn't taken yet, at the most (goal 11, plan
/// 01a1a87b2e56 step 3: `crate::memory::owners::CD_INPUTS`): as much as kiln reads of one answer of
/// a cluster's API server ([`kube::BODY_LIMIT`]), which most of what it's told is (a kind's list, an
/// object a watch saw or an apply answered), each input counted by what it takes in memory. Past it
/// a sender waits for room as a task: a watch stops reading its stream, the API server holding what
/// follows; a render or an answer waits to be handed in; a person's request waits its turn. Nothing
/// is dropped, and one input larger than it (a whole kind's list) goes alone into an empty inbox.
pub const INPUTS_BYTES: usize = kube::BODY_LIMIT;

/// The bytes of what the engine asked for and the dispatcher hasn't taken yet, at the most
/// (`crate::memory::owners::CD_ACTIONS`): as much as its inputs' ([`INPUTS_BYTES`]), which what it
/// asks for is made of (an apply's object is a render's, a status an app's). Past it the engine's
/// thread waits (its own, never the runtime's), and its inputs wait behind it.
pub const ACTIONS_BYTES: usize = INPUTS_BYTES;

/// What a shared text holds past its place: its two counts and its bytes, as one holder's (texts the
/// engine interns are counted for each holder).
fn str_heap(s: &obj::Str) -> usize {
    s.len().saturating_add(2 * size_of::<usize>())
}

fn kind_heap(k: &GroupKind) -> usize {
    str_heap(&k.group).saturating_add(str_heap(&k.kind))
}

fn key_heap(k: &ResKey) -> usize {
    [&k.group, &k.kind, &k.namespace, &k.name].into_iter().map(str_heap).fold(0, usize::saturating_add)
}

fn request_heap(r: &kiln_cd::engine::SyncRequest) -> usize {
    let resources = r.resources.as_ref().map_or(0, |l| l.capacity().saturating_mul(size_of::<ResKey>()).saturating_add(l.iter().map(key_heap).fold(0, usize::saturating_add)));
    resources.saturating_add(r.by.as_ref().map_or(0, String::capacity))
}

/// An app's settings past their place: their texts, the rules' places (each rule's own texts are a few
/// paths), the namespace's labels and annotations.
fn app_heap(c: &kiln_cd::model::AppConfig) -> usize {
    let pairs = |l: &Vec<(String, String)>| l.capacity().saturating_mul(size_of::<(String, String)>()).saturating_add(l.iter().map(|(k, v)| k.capacity().saturating_add(v.capacity())).fold(0, usize::saturating_add));
    let texts = [&c.id, &c.project, &c.cluster, &c.namespace, &c.manager].into_iter().map(str_heap).fold(0, usize::saturating_add);
    texts
        .saturating_add(c.tracking_name.as_ref().map_or(0, str_heap))
        .saturating_add(c.ignore.capacity().saturating_mul(size_of::<kiln_cd::model::IgnoreRule>()))
        .saturating_add(c.create_namespace.as_ref().map_or(0, |(labels, notes)| pairs(labels).saturating_add(pairs(notes))))
}

/// What an input holds past its place: its texts, and its objects as serde_json keeps them
/// (`crate::memory::queue::json_heap`).
fn input_heap(i: &Input) -> usize {
    use crate::memory::queue::{json_heap, json_values_heap};
    match i {
        Input::Tick => 0,
        Input::UpsertApp(cfg) => app_heap(cfg),
        Input::RemoveApp { id, .. } => str_heap(id),
        Input::UpsertCluster { name, .. } | Input::ClusterReachable { name, .. } => str_heap(name),
        Input::Scope { cluster, kind, .. } => str_heap(cluster).saturating_add(kind_heap(kind)),
        Input::Inputs { app, revision, .. } => str_heap(app).saturating_add(str_heap(revision)),
        Input::Rendered { result, .. } => match result {
            Ok(objects) => json_values_heap(objects),
            Err(e) => e.capacity(),
        },
        Input::Live { cluster, change } => str_heap(cluster).saturating_add(match change {
            LiveChange::Upsert(v) => json_heap(v),
            LiveChange::Delete(k) => key_heap(k),
            LiveChange::Listed { kind, objects } => kind_heap(kind).saturating_add(json_values_heap(objects)),
        }),
        Input::Answer { result, .. } => match result {
            Ok(Some(v)) => json_heap(v),
            Ok(None) => 0,
            Err(e) => e.capacity(),
        },
        Input::Refresh { app, .. } | Input::Terminate { app } | Input::Remember { app, .. } => str_heap(app),
        Input::Sync { app, request } => str_heap(app).saturating_add(request_heap(request)),
        Input::SyncTo { app, revision, request, .. } => str_heap(app).saturating_add(str_heap(revision)).saturating_add(request_heap(request)),
    }
}

/// A message to the engine weighs its place and what it holds: an input its box and what that holds
/// ([`input_heap`]); a question its closure's captures as they sit (what they point to is its caller's,
/// who waits for the answer: one question each); health rules their texts.
impl crate::memory::queue::Weigh for ToEngine {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(match self {
            ToEngine::Input(i) => size_of::<Input>().saturating_add(input_heap(i)),
            ToEngine::Query(f) => std::mem::size_of_val(&**f),
            ToEngine::Health(rules) => rules.capacity().saturating_mul(size_of::<(String, String)>()).saturating_add(rules.iter().map(|(k, v)| k.capacity().saturating_add(v.capacity())).fold(0, usize::saturating_add)),
        })
    }
}

/// What the engine asks for weighs its place, its texts, and an apply's object as serde_json keeps it.
impl crate::memory::queue::Weigh for Action {
    fn weight(&self) -> usize {
        let opt = |s: &Option<String>| s.as_ref().map_or(0, String::capacity);
        size_of::<Self>().saturating_add(match self {
            Action::Render { app, revision, .. } => str_heap(app).saturating_add(str_heap(revision)),
            Action::Watch { cluster, kind } => str_heap(cluster).saturating_add(kind_heap(kind)),
            Action::Apply { cluster, object, manager, .. } => str_heap(cluster).saturating_add(str_heap(manager)).saturating_add(crate::memory::queue::json_heap(object)),
            Action::Delete { cluster, key, .. } => str_heap(cluster).saturating_add(key_heap(key)),
            Action::Status(s) => {
                let op = s.operation.as_ref().map_or(0, |o| o.message.capacity().saturating_add(opt(&o.by)));
                str_heap(&s.app).saturating_add(s.revision.as_ref().map_or(0, str_heap)).saturating_add(op).saturating_add(opt(&s.message))
            }
            Action::Record(Record::Started { app, revision, by, .. }) => str_heap(app).saturating_add(revision.as_ref().map_or(0, str_heap)).saturating_add(opt(by)),
            Action::Record(Record::Finished { app, revision, by, message, .. }) => {
                str_heap(app).saturating_add(revision.as_ref().map_or(0, str_heap)).saturating_add(opt(by)).saturating_add(message.capacity())
            }
        })
    }
}

/// The engine's inbox, bounded in bytes (goal 11, plan 01a1a87b2e56 step 3): each message counted at
/// what it takes and its place ([`counted`]) against its bound, into
/// `crate::memory::owners::CD_INPUTS`. The channel is std's, its places made as it fills, since the
/// engine's thread waits on it with a timeout (its timers come due); the bytes are a
/// [`crate::memory::queue::Room`]'s, which a sender waits for as a task. Every sender waits for room:
/// none sends past the bound.
struct Inbox {
    tx: std::sync::mpsc::Sender<(usize, ToEngine)>,
    room: Arc<crate::memory::queue::Room>,
}

/// The engine's end of its inbox: what it takes lets its bytes go. Dropped with the engine's thread,
/// it closes the inbox, so a sender waiting for room it would never get stops waiting.
struct Inflow {
    rx: std::sync::mpsc::Receiver<(usize, ToEngine)>,
    room: Arc<crate::memory::queue::Room>,
}

/// Room taken in the engine's inbox for one message, waited for before a lock no one holds across a
/// wait (the inputs gate): sent under it without waiting, or its room given back if it isn't sent.
pub(crate) struct Reserved<'a> {
    inbox: &'a Inbox,
    bytes: usize,
    message: Option<ToEngine>,
}

/// An inbox of `bound` bytes and the engine's end of it.
fn inbox(bound: usize) -> (Inbox, Inflow) {
    let (tx, rx) = std::sync::mpsc::channel();
    let room = Arc::new(crate::memory::queue::Room::new(&crate::memory::owners::CD_INPUTS, bound));
    (Inbox { tx, room: Arc::clone(&room) }, Inflow { rx, room })
}

/// What a message is counted at in the inbox: what it takes, and its place in the channel.
fn counted(m: &ToEngine) -> usize {
    crate::memory::queue::Weigh::weight(m).saturating_add(crate::memory::queue::PLACE)
}

impl Inbox {
    /// Sends `m` once there's room for it, waiting as a task: false when the engine is gone.
    async fn send(&self, m: ToEngine) -> bool {
        let bytes = counted(&m);
        self.room.take(bytes).await && self.put(bytes, m)
    }

    /// Room for `m`, waited for as [`Inbox::send`] waits: what's returned sends it later without
    /// waiting. `None` when the engine is gone.
    async fn reserve(&self, m: ToEngine) -> Option<Reserved<'_>> {
        let bytes = counted(&m);
        // Made only once its room is taken: a reservation gives back what it took, and none else.
        self.room.take(bytes).await.then(|| Reserved { inbox: self, bytes, message: Some(m) })
    }

    /// Into the channel, its bytes already taken: given back if the engine is gone.
    fn put(&self, bytes: usize, m: ToEngine) -> bool {
        if self.tx.send((bytes, m)).is_ok() {
            return true;
        }
        self.room.give(bytes);
        false
    }
}

impl Reserved<'_> {
    /// Sends its message, its room taken already: never waits. False when the engine is gone.
    fn send(mut self) -> bool {
        let bytes = std::mem::take(&mut self.bytes);
        match self.message.take() {
            Some(m) => self.inbox.put(bytes, m),
            None => false,
        }
    }
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        // Not sent: its room goes back, and whoever waits for room looks again.
        if self.bytes > 0 {
            self.inbox.room.give(self.bytes);
        }
    }
}

impl Inflow {
    /// The next message, waiting at most `wait`: its bytes let go as it's taken.
    fn recv_timeout(&self, wait: Duration) -> Result<ToEngine, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(wait).map(|(bytes, m)| {
            self.room.give(bytes);
            m
        })
    }

    /// The next message if one waits.
    fn try_recv(&self) -> Result<ToEngine, std::sync::mpsc::TryRecvError> {
        self.rx.try_recv().map(|(bytes, m)| {
            self.room.give(bytes);
            m
        })
    }

    /// The next message, waiting for one (a test's engine).
    #[cfg(test)]
    fn recv(&self) -> Result<ToEngine, std::sync::mpsc::RecvError> {
        self.rx.recv().map(|(bytes, m)| {
            self.room.give(bytes);
            m
        })
    }
}

impl Drop for Inflow {
    fn drop(&mut self) {
        // No one takes from it any more: whoever waits for room stops waiting, and what waited in it
        // goes now, its bytes with it.
        self.room.close();
        while let Ok((bytes, _)) = self.rx.try_recv() {
            self.room.give(bytes);
        }
    }
}

/// The settings that carry health scripts (Argo CD's `resource.customizations.health.*`).
const HEALTH_SETTINGS: &str = "resource.customizations.health.";

/// Health scripts from settings: each key and its script.
fn health_settings(store: &Store) -> Vec<(String, String)> {
    store.read(|d| d.settings.iter().filter(|(k, _)| k.starts_with(HEALTH_SETTINGS)).filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
}

/// An app as the server knows it: its definition and where its sources stand.
#[derive(Clone, Debug)]
pub struct AppEntry {
    /// The definition.
    pub def: AppDef,
    /// Each source's commit, as last resolved.
    pub commits: Vec<Option<String>>,
    /// What each source's render reads.
    pub deps: Vec<Vec<Dep>>,
    /// The render key last given to the engine.
    pub key: Option<u64>,
    /// Why its sources couldn't be read, if they couldn't.
    pub error: Option<String>,
    /// Apps it renders (app of apps), by id.
    pub children: BTreeSet<String>,
    /// Its latest resolutions, so a render (or a history entry) reads the commits its key
    /// stands for even after git moved.
    pub resolved: VecDeque<Resolved>,
}

/// One resolution of an app's sources: what a render key stands for.
#[derive(Clone, Debug, Default)]
pub struct Resolved {
    /// The first source's commit: the app's revision.
    pub revision: String,
    /// The render key: the content address of everything a render reads, the cluster's version
    /// and API versions included.
    pub key: u64,
    /// The same without the cluster's part: what git holds for the app.
    pub content: u64,
    /// Each source's commit.
    pub commits: Vec<Option<String>>,
    /// What each source's render reads.
    pub deps: Vec<Vec<Dep>>,
    /// Its artifacts, pinned for this revision (docs/cd-targets.md §1.3).
    pub artifacts: Vec<ArtifactRef>,
    /// Pull secrets its artifacts ask for.
    pub pull_secrets: Vec<PullSecret>,
    /// Its secrets, at the versions its definition pins (docs/deploy-api.md §3.5).
    pub secrets: Option<AppSecrets>,
}

/// Keeps `r` as its key's resolution, the newest of `list` (eight at the most): what a deploy of
/// that key records its commits from. A revert back to content deployed before is the revert's
/// commit, not the one whose content it restored; a rollback is the commits it went back to.
fn keep_resolved(list: &mut VecDeque<Resolved>, r: &Resolved) {
    list.retain(|x| x.key != r.key);
    list.push_back(r.clone());
    while list.len() > 8 {
        list.pop_front();
    }
}

impl Resolved {
    /// Makes `artifacts` part of what this resolution stands for: they move its key and content
    /// as a source's files do, so a new digest renders (and syncs) again, and an unchanged one
    /// doesn't.
    pub fn add_artifacts(&mut self, artifacts: Vec<ArtifactRef>, pull_secrets: Vec<PullSecret>) {
        let mut h = 0u64;
        for a in &artifacts {
            h = h.wrapping_add(blake(&[b"artifact", a.name.as_bytes(), a.reference.as_bytes()]));
        }
        for p in &pull_secrets {
            h = h.wrapping_add(blake(&[b"pull-secret", p.name.as_bytes(), crate::util::sha256_hex(p.config.as_bytes()).as_bytes()]));
        }
        self.key ^= h.rotate_left(13);
        self.content ^= h.rotate_left(13);
        self.artifacts = artifacts;
        self.pull_secrets = pull_secrets;
    }

    /// Makes an app's secrets part of what this resolution stands for, by their versions (never
    /// their values): a new version renders (and syncs) again, the same ones don't.
    pub fn add_secrets(&mut self, secrets: Option<AppSecrets>) {
        if let Some(s) = &secrets {
            let h = blake(&[b"app-secrets", s.name.as_bytes(), s.bundle.versions().as_bytes()]);
            self.key ^= h.rotate_left(29);
            self.content ^= h.rotate_left(29);
        }
        self.secrets = secrets;
    }
}

/// An app's secrets as a render delivers them: the Secret's name, and the values of the versions
/// its definition pins, which are never printed.
#[derive(Clone, Debug)]
pub struct AppSecrets {
    pub name: String,
    pub bundle: Arc<crate::secrets::Bundle>,
}

/// A Secret of registry credentials an artifact asks for (`pull-secret`): its name and the
/// Docker config it holds, which is never printed.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PullSecret {
    pub name: String,
    pub config: String,
}

impl std::fmt::Debug for PullSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullSecret").field("name", &self.name).field("config", &"<redacted>").finish()
    }
}

/// The deployments service.
pub struct Cd {
    hub: Arc<Hub>,
    /// What is kept durably.
    pub store: Store,
    /// The engine's inbox, bounded in bytes ([`INPUTS_BYTES`]).
    engine: Inbox,
    /// Every app, by id.
    pub apps: RwLock<BTreeMap<String, AppEntry>>,
    /// Connected clusters, by name.
    pub clusters: RwLock<BTreeMap<String, Arc<Cluster>>>,
    /// The latest status of every app.
    pub statuses: RwLock<BTreeMap<String, Summary>>,
    dirty: Mutex<BTreeSet<String>>,
    /// Apps this server is deleting through the API, each with whether its objects go too: the
    /// store's change of a deletion carries no cascade (another member's deletion was that member's
    /// to cascade), so whichever of the two unregisters the app first does it as this said.
    deleting: Mutex<BTreeMap<String, bool>>,
    watched: Mutex<BTreeSet<(String, GroupKind)>>,
    connecting: Mutex<BTreeSet<String>>,
    /// Held while an app's inputs are checked against its pin and sent, so the source poll and a
    /// rollback never interleave.
    inputs_gate: Mutex<()>,
    dir: PathBuf,
    tools: Tools,
    poll: Duration,
    in_cluster_context: String,
    kubeconfig: String,
    kick: crate::rt::Notify,
    /// What viewers read from clusters (trees, logs), kept a few seconds.
    pub live: live::Live,
    /// Renders plans made, kept by key.
    plans: plans::Renders,
    /// Images resolved lately, by registry and reference, so apps sharing one ask once a minute.
    artifacts: Mutex<HashMap<String, (crate::ci::integrations::oci::Resolved, Instant)>>,
    /// Each secrets scope's values, at the versions read last: read again (and the read
    /// recorded) only when a definition pins others, or a lease runs out.
    secret_values: Mutex<HashMap<String, (String, Arc<crate::secrets::Bundle>)>>,
    /// Why a repository's deploy file doesn't read, by repository.
    pub deploy_errors: Mutex<BTreeMap<String, String>>,
    /// Each repository's environments (`.kiln/environments.yaml` at trunk), by its URL.
    pub environments: RwLock<BTreeMap<String, BTreeMap<String, apps::EnvironmentSpec>>>,
    /// The latest sync each app started, for a self-deploy's record.
    pub started: Mutex<HashMap<String, guard::Started>>,
    /// Connected targets beyond Kubernetes, by name (docs/cd-targets.md §2).
    pub targets: RwLock<BTreeMap<String, Arc<targets::Conn>>>,
    /// Why a target can't be reached yet, by name.
    pub target_errors: Mutex<BTreeMap<String, String>>,
    /// Repositories' `owner/name`, by clone URL: whose workload a target's calls are.
    pub repo_names: RwLock<HashMap<String, String>>,
    /// Rollouts on targets being driven (docs/cd-targets.md §5).
    pub rollouts: rollouts::Drivers,
    /// What a sync applied, and what custody to production has looked at (docs/ci.md W27-R08).
    pub custody: custody::Custody,
}

impl Cd {
    /// Gives the engine an input, waiting first while its inbox holds [`INPUTS_BYTES`] (goal 11, plan
    /// 01a1a87b2e56 step 3): a task's wait, its shard going on with others. Let go only when the
    /// engine is gone.
    pub async fn input(&self, input: Input) {
        self.engine.send(ToEngine::Input(Box::new(input))).await;
    }

    /// Room in the engine's inbox for `input`, waited for as [`Cd::input`] waits: what's returned
    /// gives it to the engine without waiting, under the inputs gate (which no one holds across a
    /// wait), or gives its room back if dropped unsent. `None` when the engine is gone.
    pub(crate) async fn reserve_input(&self, input: Input) -> Option<Reserved<'_>> {
        self.engine.reserve(ToEngine::Input(Box::new(input))).await
    }

    /// Asks the engine something, waiting for room in its inbox as [`Cd::input`] does.
    pub async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = futures::channel::oneshot::channel();
        let job: Box<dyn FnOnce(&Engine) + Send> = Box::new(move |e| {
            let _ = tx.send(f(e));
        });
        if !self.engine.send(ToEngine::Query(job)).await {
            return None;
        }
        rx.await.ok()
    }

    /// Polls sources now.
    pub fn kick(&self) {
        self.kick.notify();
    }

    pub(crate) fn mirror_dir(&self, url: &str) -> PathBuf {
        self.dir.join("git").join(format!("{:016x}.git", obj::hash_bytes(url.as_bytes())))
    }

    pub(crate) fn git_auth(&self, url: &str) -> GitAuth {
        // The longest matching repository entry's credentials (Argo CD's credential templates).
        self.store.read(|d| {
            d.repos
                .values()
                .filter(|r| url.starts_with(r.url.trim_end_matches('/')))
                .max_by_key(|r| r.url.len())
                .map(|r| GitAuth {
                    header: match (&r.username, &r.password) {
                        (Some(u), Some(p)) => {
                            use base64::Engine as _;
                            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))))
                        }
                        (None, Some(p)) => Some(format!("Bearer {p}")),
                        _ => None,
                    },
                    ssh_key: None,
                    insecure: r.insecure,
                })
                .unwrap_or_default()
        })
    }

    /// A person's auto-sync for app `id`, if one set it (docs/ci.md W26-R27).
    pub(crate) fn auto_sync_setting(&self, id: &str) -> Option<kiln_cd::autosync::Setting> {
        self.store.read(|d| d.settings.get(&kiln_cd::autosync::app_setting(id)).cloned()).filter(|v| !v.is_null()).and_then(|v| serde_json::from_value(v).ok())
    }

    /// App `id`'s auto-sync: whether kiln syncs it on its own, and whose word that is.
    pub(crate) fn auto_sync_of(&self, id: &str, def: &AppDef) -> kiln_cd::autosync::Decision {
        kiln_cd::autosync::decide(def.sync.auto, self.auto_sync_setting(id).as_ref())
    }

    /// App `id`'s auto-sync as the API shows it: on or off, whose word (`from`: `app` or
    /// `deploy-file`), what the deploy file says, and the person's setting.
    pub(crate) fn auto_sync_json(&self, id: &str, def: &AppDef) -> serde_json::Value {
        let setting = self.auto_sync_setting(id);
        let d = kiln_cd::autosync::decide(def.sync.auto, setting.as_ref());
        serde_json::json!({
            "on": d.on,
            "from": d.from.as_str(),
            "deploy_file": def.sync.auto,
            "app": setting.map(|s| serde_json::json!({"on": s.on, "by": s.by, "at_ms": s.at_ms})),
        })
    }

    /// Registers (or replaces) an app and tells the engine, its auto-sync as a person set it, waiting
    /// for room in the engine's inbox ([`Cd::input`]).
    pub async fn register(&self, def: AppDef) {
        let id = def.id();
        let mut cfg = kiln_cd::autosync::config(&def, self.auto_sync_setting(&id).as_ref());
        cfg.read_only = is_shadow(&def);
        {
            let mut apps = self.apps.write();
            let entry = apps.entry(id.clone()).or_insert_with(|| AppEntry {
                def: def.clone(),
                commits: Vec::new(),
                deps: Vec::new(),
                key: None,
                error: None,
                children: BTreeSet::new(),
                resolved: VecDeque::new(),
            });
            if entry.def != def {
                entry.def = def;
                entry.key = None;
                entry.commits.clear();
                entry.resolved.clear();
            }
        }
        self.input(Input::UpsertApp(cfg)).await;
        // The last sync from before a restart: automation goes on from it, not from scratch.
        let last = self.store.read(|d| d.history.get(&id).and_then(|h| h.back()).and_then(|h| Some((h.content?, h.succeeded))));
        if let Some((content, succeeded)) = last {
            self.input(Input::Remember { app: id.as_str().into(), content, succeeded }).await;
        }
        self.kick();
    }

    /// Forgets an app (and its children); with `cascade`, deletes its objects. Waits for room in the
    /// engine's inbox ([`Cd::input`]); boxed, since it forgets its children the same way.
    pub fn unregister<'a>(&'a self, id: &'a str, cascade: bool) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async move {
            let children = self.apps.write().remove(id).map(|e| e.children).unwrap_or_default();
            self.statuses.write().remove(id);
            self.input(Input::RemoveApp { id: id.into(), cascade: cascade.then_some(kiln_cd::model::Propagation::Foreground) }).await;
            for child in children {
                self.unregister(&child, cascade).await;
            }
            self.hub.emit(&crate::wire::events::ServerEvent::CdAppDeleted { app: id.to_owned() });
        })
    }

    // ---------------------------------------------------------------- sources

    async fn poll_loop(self: Arc<Self>) {
        loop {
            self.refresh_sources().await;
            let wait = crate::rt::timeout(self.poll, self.kick.notified());
            let _ = wait.await;
            if self.hub.shutdown.is_cancelled() {
                return;
            }
        }
    }

    /// Apps from `.kiln/deploy.yaml` in kiln's repositories (cd.md, Defining applications): each
    /// round reads the file, and `.kiln/environments.yaml`, at the commit trunk is at. An app is
    /// registered when its definition changed, and removed (with its objects, as deleting an app
    /// does) when the file no longer defines it. A file that doesn't read, or a repository that
    /// can't be fetched, changes nothing: its apps stay as they were.
    async fn refresh_deploy_files(self: &Arc<Self>) {
        let repos = match self.hub.store.repos().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "deployments: can't list repositories");
                return;
            }
        };
        self.repo_names.write().extend(repos.iter().map(|r| (r.clone_url.clone(), r.name.clone())));
        for repo in repos.into_iter().filter(|r| r.enabled) {
            let trunk = repo.branches.first().cloned().unwrap_or_else(|| "main".into());
            let (dir, auth, url, t) = (self.mirror_dir(&repo.clone_url), self.git_auth(&repo.clone_url), repo.clone_url.clone(), trunk.clone());
            let read = crate::rt::blocking(move || -> Result<(String, Option<String>, Option<String>)> {
                let m = Mirror::open(&dir, &url, auth)?;
                m.fetch()?;
                let commit = m.resolve(&t)?;
                let text = |p: &str| -> Result<Option<String>> { Ok(m.read(&commit, p)?.map(|b| String::from_utf8_lossy(&b).into_owned())) };
                let deploy = text(".kiln/deploy.yaml")?;
                let envs = if deploy.is_some() { text(".kiln/environments.yaml")? } else { None };
                Ok((commit, deploy, envs))
            })
            .await
            .and_then(|r| r);
            let (commit, deploy, envs) = match read {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "deployments: can't read the repository's deploy file");
                    continue;
                }
            };
            let defs = match deploy {
                // No file: none of its apps are left.
                None => Vec::new(),
                Some(text) => {
                    let environments: BTreeMap<String, apps::EnvironmentSpec> = match envs.as_deref().map(serde_yaml::from_str).transpose() {
                        Ok(e) => e.unwrap_or_default(),
                        Err(e) => {
                            self.deploy_file_error(&repo.name, Some(format!(".kiln/environments.yaml at {}: {e}", short(&commit))));
                            continue;
                        }
                    };
                    let mut scope = apps::Scope::default();
                    scope.set("git.sha", json!(commit));
                    scope.set("git.short_sha", json!(short(&commit)));
                    scope.set("git.ref", json!(format!("refs/heads/{trunk}")));
                    scope.set("repo.name", json!(repo.name));
                    let parsed = apps::from_deploy_file(&text, &repo.clone_url, &trunk, &environments, &scope);
                    if parsed.is_ok() {
                        self.environments.write().insert(repo.clone_url.clone(), environments);
                    }
                    match parsed {
                        Ok(d) => d,
                        Err(e) => {
                            self.deploy_file_error(&repo.name, Some(format!(".kiln/deploy.yaml at {}: {}", short(&commit), e.0)));
                            continue;
                        }
                    }
                }
            };
            self.deploy_file_error(&repo.name, None);
            let ids: BTreeSet<String> = defs.iter().map(AppDef::id).collect();
            for def in defs {
                let changed = self.apps.read().get(&def.id()).is_none_or(|e| e.def != def);
                // A copy of the app kept in kiln (`cd.apps.keep`, taken over) loads at each start,
                // before this file is read, and stays while it doesn't read: kiln-production once
                // came back so as it was a week before, and a sync deployed that (2026-10-26). The
                // file's is the app now, so it's the copy kept.
                let kept = self.store.read(|d| d.apps.get(&def.id()).is_some_and(|k| *k != def));
                if changed {
                    tracing::info!(app = %def.id(), repo = %repo.name, commit = %short(&commit), "deployments: an app from a deploy file");
                    self.register(def.clone()).await;
                }
                if kept {
                    tracing::info!(app = %def.id(), repo = %repo.name, commit = %short(&commit), "deployments: the copy kept in kiln follows the deploy file");
                    match self.store.submit(Command::PutApp(Box::new(def))).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(repo = %repo.name, error = %e, "deployments: can't keep a deploy file's app"),
                        Err(_) => tracing::warn!(repo = %repo.name, "deployments: the store stopped"),
                    }
                }
            }
            let gone: Vec<String> = self
                .apps
                .read()
                .iter()
                .filter(|(id, e)| matches!(&e.def.origin, Origin::Deploy { repo: r } if *r == repo.clone_url) && !ids.contains(*id))
                .map(|(id, _)| id.clone())
                .collect();
            for id in gone {
                tracing::info!(app = %id, repo = %repo.name, "deployments: the deploy file no longer defines an app; removing it");
                let cascade = self.apps.read().get(&id).is_some_and(|e| e.def.cascade);
                self.unregister(&id, cascade).await;
            }
        }
    }

    /// Records (or clears) why a repository's deploy file doesn't read, once per change.
    fn deploy_file_error(&self, repo: &str, error: Option<String>) {
        let mut errors = self.deploy_errors.lock();
        if errors.get(repo) == error.as_ref() {
            return;
        }
        match error {
            Some(e) => {
                tracing::warn!(repo = %repo, error = %e, "deployments: a deploy file doesn't read; its apps stay as they were");
                errors.insert(repo.to_owned(), e);
            }
            None => {
                errors.remove(repo);
            }
        }
    }

    async fn refresh_sources(self: &Arc<Self>) {
        self.refresh_deploy_files().await;
        self.connect_targets().await;
        let apps: Vec<AppEntry> = self.apps.read().values().cloned().collect();
        // Each repository is fetched once a round.
        let mut urls: BTreeSet<String> = BTreeSet::new();
        for a in &apps {
            for s in a.def.sources.iter().filter(|s| s.chart.is_none()) {
                urls.insert(s.repo.clone());
            }
        }
        let mut fetched: HashMap<String, Result<(), String>> = HashMap::new();
        for url in urls {
            let (dir, auth) = (self.mirror_dir(&url), self.git_auth(&url));
            let u = url.clone();
            let r = crate::rt::blocking(move || -> Result<()> {
                let m = Mirror::open(&dir, &u, auth)?;
                m.fetch()
            })
            .await
            .and_then(|r| r)
            .map_err(|e| format!("{e:#}"));
            if let Err(e) = &r {
                tracing::warn!(repo = %url, error = %e, "deployments: fetching a source failed");
            }
            fetched.insert(url, r);
        }
        for a in apps {
            let id = a.def.id();
            let failed = a.def.sources.iter().find_map(|s| fetched.get(&s.repo).and_then(|r| r.as_ref().err()).cloned());
            if let Some(e) = failed {
                self.set_source_error(&id, Some(e));
                continue;
            }
            let this = self.clone();
            let def = a.def.clone();
            let def_for_artifacts = a.def.clone();
            // Renders read the cluster's version and API versions (Helm's .Capabilities), so an
            // app's inputs wait for its cluster: a render without them would differ from the one
            // after, and could be applied first.
            let Some(cluster) = self.clusters.read().get(&def.destination.cluster).cloned() else { continue };
            let (kube_version, api_versions) = (cluster.version.read().clone(), cluster.discovery.read().api_versions.clone());
            let pin = self.store.read(|d| d.pins.get(&id).cloned());
            let mut seen = pin.clone();
            let facts = (kube_version.clone(), api_versions.clone());
            let result = crate::rt::blocking(move || -> Result<(Resolved, Option<Resolved>)> {
                let head = this.inputs_of(&def, kube_version.as_deref(), &api_versions, None)?;
                // A rolled-back app holds its deployment's commits until git changes what it
                // renders; then it follows git again.
                let held = match &pin {
                    Some(p) if p.head == head.content => Some(this.inputs_of(&def, kube_version.as_deref(), &api_versions, Some(&p.commits))?),
                    _ => None,
                };
                Ok((head, held))
            })
            .await
            .and_then(|r| r);
            // Artifacts resolve for each revision: the head's, and a rolled-back app's own.
            let result = match result {
                Ok((mut head, mut held)) => async {
                    let (a, p) = match self.resolve_artifacts(&def_for_artifacts, &head.revision).await {
                        Ok(r) => r,
                        // The head's artifacts aren't built yet (pushes come faster than its image
                        // builds): the newest commit since what runs whose are takes its place.
                        Err(e) => match self.newest_built(&def_for_artifacts, &id, &head.revision, facts).await {
                            Some((built, a, p)) => {
                                head = built;
                                (a, p)
                            }
                            None => return Err(e),
                        },
                    };
                    head.add_artifacts(a, p);
                    // Its secrets are the ones its definition pins, whichever commits it holds.
                    let secrets = self.app_secrets(&def_for_artifacts).await?;
                    head.add_secrets(secrets.clone());
                    if let Some(h) = held.as_mut() {
                        let (a, p) = self.held_artifacts(&def_for_artifacts, &id, h).await?;
                        h.add_artifacts(a, p);
                        h.add_secrets(secrets);
                    }
                    Ok::<_, anyhow::Error>((head, held))
                }
                .await,
                Err(e) => Err(e),
            };
            match result {
                Ok((head, held)) => {
                    if seen.is_some() && held.is_none() {
                        tracing::info!(app = %id, "deployments: git changed a rolled-back app; it follows git again");
                        match self.store.submit(Command::Pin { app: id.clone(), pin: None }).await {
                            Ok(Ok(())) => seen = None,
                            Ok(Err(e)) => tracing::warn!(app = %id, error = %e, "deployments: letting go of a rollback failed"),
                            Err(_) => tracing::warn!(app = %id, "deployments: the store stopped"),
                        }
                    }
                    // A rollback may have pinned the app (or let go of it) meanwhile: then its
                    // inputs stand, and the next poll starts from them. Room in the engine's inbox
                    // is waited for before the gate (goal 11, plan 01a1a87b2e56 step 3), which is
                    // held only while the pin is checked and the inputs kept and sent.
                    let r = held.unwrap_or(head);
                    let told = self.reserve_input(Input::Inputs { app: id.as_str().into(), revision: r.revision.as_str().into(), key: r.key, content: r.content }).await;
                    let _gate = self.inputs_gate.lock();
                    if self.store.read(|d| d.pins.get(&id).cloned()) == seen {
                        self.use_inputs(&id, &r, told);
                    }
                }
                Err(e) => self.set_source_error(&id, Some(format!("{e:#}"))),
            }
        }
    }

    /// The newest commit from `head` back to the one the app runs whose artifacts are built (the
    /// head's aren't): its inputs and artifacts, in the head's place until the head's are. `None`:
    /// none of them is, or the head doesn't lead from what runs (then it waits for the head's).
    async fn newest_built(self: &Arc<Self>, def: &AppDef, id: &str, head: &str, (kube_version, api_versions): (Option<String>, Vec<String>)) -> Option<(Resolved, Vec<ArtifactRef>, Vec<PullSecret>)> {
        // What runs: its last deployment that succeeded (kept, so known after a restart too, before
        // the app is compared), else what its status says.
        let runs = self
            .store
            .read(|d| d.history.get(id).and_then(|h| h.iter().rev().find(|x| x.succeeded).and_then(|x| x.revision.clone())))
            .or_else(|| self.statuses.read().get(id).and_then(|s| s.revision.as_deref().map(str::to_owned)))?;
        // The source whose commit is the app's revision.
        let i = def.sources.iter().position(|s| s.chart.is_none() && s.as_ref.is_none())?;
        let source = def.sources.get(i)?;
        let (dir, url, auth, from, to) = (self.mirror_dir(&source.repo), source.repo.clone(), self.git_auth(&source.repo), runs.clone(), head.to_owned());
        let stack = crate::rt::blocking(move || Mirror::open(&dir, &url, auth)?.stack(Some(&from), &to, NEWEST_BUILT)).await.ok()?.ok()?;
        for commit in in_place_of_head(stack, &runs) {
            if self.resolve_artifacts(def, &commit).await.is_err() {
                continue;
            }
            let at: Vec<Option<String>> = (0..def.sources.len()).map(|j| (j == i).then(|| commit.clone())).collect();
            let (this, d, (kv, av)) = (Arc::clone(self), def.clone(), (kube_version.clone(), api_versions.clone()));
            let built = crate::rt::blocking(move || this.inputs_of(&d, kv.as_deref(), &av, Some(&at))).await.ok()?.ok()?;
            let (a, p) = self.resolve_artifacts(def, &built.revision).await.ok()?;
            if commit != runs {
                tracing::info!(app = %id, head = %head.get(..7).unwrap_or(head), commit = %commit.get(..7).unwrap_or(&commit), "deployments: the head's artifacts aren't built yet; the newest commit whose are takes its place");
            }
            return Some((built, a, p));
        }
        None
    }

    fn set_source_error(&self, id: &str, e: Option<String>) {
        if let Some(entry) = self.apps.write().get_mut(id) {
            if entry.error != e {
                tracing::warn!(app = %id, error = ?e, "deployments: an app's sources can't be read");
            }
            entry.error = e;
        }
    }

    /// Makes `r` an app's inputs, telling the engine when they moved (`told`: their input, its room
    /// in the engine's inbox taken), under the inputs gate.
    fn use_inputs(&self, id: &str, r: &Resolved, told: Option<Reserved<'_>>) {
        if self.keep_inputs(id, r)
            && let Some(told) = told
        {
            told.send();
        }
    }

    /// Records `r` as an app's inputs (renders read their commits from here); whether they moved.
    fn keep_inputs(&self, id: &str, r: &Resolved) -> bool {
        let mut apps = self.apps.write();
        let Some(e) = apps.get_mut(id) else { return false };
        let moved = e.key != Some(r.key) || e.commits != r.commits;
        e.key = Some(r.key);
        e.commits = r.commits.clone();
        e.deps = r.deps.clone();
        e.error = None;
        keep_resolved(&mut e.resolved, r);
        moved
    }

    /// Resolves an app's sources: its revision, render key, each source's commit and what each
    /// source reads. `at` holds sources at given commits (a rollback); the others resolve their
    /// revisions.
    fn inputs_of(&self, def: &AppDef, kube_version: Option<&str>, api_versions: &[String], at: Option<&[Option<String>]>) -> Result<Resolved> {
        let mut key = blake(&[serde_json::to_vec(&def.sources)?.as_slice(), def.destination.namespace.as_bytes(), def.name.as_bytes()]);
        // Only Helm and plugins read the cluster's version and API versions. Other renders come out
        // alike anywhere, so a CRD installed on the cluster moves none of their keys.
        let cluster = blake(&[kube_version.unwrap_or("").as_bytes(), api_versions.join(",").as_bytes()]).rotate_left(7);
        let mut reads_cluster = false;
        let held = |i: usize| at.and_then(|c| c.get(i).cloned().flatten());
        let mut commits = Vec::new();
        let mut all_deps = Vec::new();
        let mut revision = String::new();
        // Commits of ref sources first: other sources' value files read them.
        let mut ref_commits: HashMap<String, (Mirror, String)> = HashMap::new();
        for (i, s) in def.sources.iter().enumerate() {
            if let (Some(name), None) = (&s.as_ref, &s.chart) {
                let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
                let commit = match held(i) {
                    Some(c) => c,
                    None => m.resolve(&s.revision)?,
                };
                ref_commits.insert(name.clone(), (m, commit));
            }
        }
        for (i, s) in def.sources.iter().enumerate() {
            if s.chart.is_some() {
                reads_cluster = true;
                commits.push(None);
                // A chart from a Helm repository: its version is its content (ref value files below).
                let deps = render::deps(s, Kind::Helm, &|_| None);
                key ^= self.ref_dep_hashes(&deps, &ref_commits)?.rotate_left((i % 61) as u32);
                all_deps.push(deps);
                continue;
            }
            let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
            let commit = match held(i) {
                Some(c) => c,
                None => m.resolve(&s.revision)?,
            };
            if revision.is_empty() && s.as_ref.is_none() {
                revision = commit.clone();
            }
            if s.as_ref.is_some() {
                commits.push(Some(commit));
                all_deps.push(Vec::new());
                continue;
            }
            let read = |p: &str| m.read(&commit, p).ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
            let kind = render::kind_of(s, &|p| m.hashes(&commit, &[p]).ok().and_then(|h| h.into_iter().next().flatten()).is_some());
            reads_cluster |= matches!(kind, Kind::Helm | Kind::Plugin);
            // A plugin, or parameters naming the revision, can write the commit itself into the
            // objects: then it is part of what the render reads.
            if kind == Kind::Plugin || serde_json::to_string(&s.engine).is_ok_and(|e| e.contains("ARGOCD_APP_REVISION") || e.contains("KILN_SHA")) {
                key ^= blake(&[b"commit", commit.as_bytes()]).rotate_left(((i + 17) % 61) as u32);
            }
            let deps = render::deps(s, kind, &read);
            let own: Vec<String> = deps.iter().filter(|d| d.source.is_none()).map(|d| d.path.clone()).collect();
            let own = m.with_links(&commit, &own)?;
            let refs: Vec<Dep> = deps.into_iter().filter(|d| d.source.is_some()).collect();
            let paths: Vec<&str> = own.iter().map(String::as_str).collect();
            let hashes = m.hashes(&commit, &paths)?;
            let mut h = 0u64;
            for (p, hash) in paths.iter().zip(&hashes) {
                h = h.wrapping_add(blake(&[p.as_bytes(), hash.as_deref().unwrap_or("-").as_bytes()]));
            }
            key ^= h.rotate_left((i % 61) as u32);
            key ^= self.ref_dep_hashes(&refs, &ref_commits)?.rotate_left(((i + 31) % 61) as u32);
            let mut deps: Vec<Dep> = own.into_iter().map(|path| Dep { source: None, path }).collect();
            deps.extend(refs);
            commits.push(Some(commit));
            all_deps.push(deps);
        }
        if revision.is_empty() {
            revision = def.sources.first().map(|s| s.revision.clone()).unwrap_or_default();
        }
        let content = key;
        if reads_cluster {
            key ^= cluster;
        }
        Ok(Resolved { revision, key, content, commits, deps: all_deps, ..Resolved::default() })
    }

    fn ref_dep_hashes(&self, deps: &[Dep], refs: &HashMap<String, (Mirror, String)>) -> Result<u64> {
        let mut h = 0u64;
        for d in deps {
            let Some(name) = &d.source else { continue };
            let (m, commit) = refs.get(name).with_context(|| format!("no source is named {name}"))?;
            let hash = m.hashes(commit, &[d.path.as_str()])?.into_iter().next().flatten();
            h = h.wrapping_add(blake(&[name.as_bytes(), d.path.as_bytes(), hash.as_deref().unwrap_or("-").as_bytes()]));
        }
        Ok(h)
    }

    // ---------------------------------------------------------------- artifacts

    /// The registry an artifact is pulled from: its `registry` integration, else the one set up
    /// for its registry, else none (anonymously).
    async fn artifact_registry(&self, spec: &apps::ArtifactSpec, reference: &oci::Reference) -> Result<oci::Registry> {
        match &spec.registry {
            Some(name) => integrations::registry(&self.hub, name).await,
            None => Ok(integrations::registry_for(&self.hub, &reference.registry).await?.unwrap_or_else(|| oci::Registry::for_name(&reference.registry, None, false))),
        }
    }

    /// An app's artifacts for `revision`: each image's digest now, and the pull secrets they ask
    /// for. An image not in its registry yet (CI hasn't pushed it) is an error that says so: the
    /// app's inputs wait, and the next poll looks again.
    pub(crate) async fn resolve_artifacts(&self, def: &AppDef, revision: &str) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        let mut refs = Vec::new();
        for (name, given) in &def.artifacts {
            // A workflow's artifact: its content, by hash, as the commit's run uploaded it.
            if let (Some(workflow), Some(artifact)) = (&given.workflow, &given.artifact) {
                let hex = self.workflow_artifact(def, workflow, artifact, revision).await.with_context(|| format!("artifact {name}"))?;
                let digest = format!("sha256:{hex}");
                refs.push(ArtifactRef { name: name.clone(), reference: digest.clone(), repository: format!("{workflow}/{artifact}"), tag: None, digest });
                continue;
            }
            // A published image: in a registry, an image like any other; local, ready when its
            // pipeline's run at this commit succeeded, and run by its tag.
            let spec = match (&given.published, self.published_spec(given)) {
                (Some(_), Some(s)) => s,
                (Some(p), None) => {
                    let pipeline = given.pipeline.as_deref().unwrap_or("image");
                    self.published_locally(def, p, pipeline, revision).await.with_context(|| format!("artifact {name}"))?;
                    refs.push(ArtifactRef { name: name.clone(), reference: format!("{p}:{revision}"), repository: p.clone(), tag: Some(revision.to_owned()), digest: String::new() });
                    continue;
                }
                (None, _) => given.clone(),
            };
            let spec = &spec;
            let text = spec.reference(revision);
            let reference = oci::parse_reference(&text).with_context(|| format!("artifact {name}"))?;
            let registry = self.artifact_registry(spec, &reference).await.with_context(|| format!("artifact {name}"))?;
            let who = registry.credentials.as_ref().map(|(u, _)| u.as_str()).unwrap_or("");
            let key = format!("{}|{who}|{reference}", registry.name);
            let cached = self.artifacts.lock().get(&key).filter(|(_, at)| at.elapsed() < Duration::from_secs(60)).map(|(r, _)| r.clone());
            let resolved = match cached {
                Some(r) => r,
                None => {
                    let r = oci::resolve(&registry, &reference)
                        .await
                        .with_context(|| format!("artifact {name}"))?
                        .with_context(|| format!("waiting for {reference}: it isn't in {} yet", registry.name))?;
                    self.artifacts.lock().insert(key, (r.clone(), Instant::now()));
                    r
                }
            };
            for want in &spec.platforms {
                let has = resolved.platforms.is_empty() || resolved.platforms.iter().any(|p| p == want || p.starts_with(&format!("{want}/")));
                anyhow::ensure!(has, "artifact {name}: {reference} isn't built for {want} (it has {})", resolved.platforms.join(", "));
            }
            refs.push(ArtifactRef { name: name.clone(), reference: reference.pinned(&resolved.digest), repository: reference.name(), tag: reference.tag.clone(), digest: resolved.digest.clone() });
        }
        Ok((refs, self.pull_secrets_of(def, revision).await?))
    }

    /// The secrets `def` pins (docs/deploy-api.md §3.5), with their values from kiln's secrets
    /// service: read once for each set of versions (the read recorded there), and kept in memory.
    pub(crate) async fn app_secrets(&self, def: &AppDef) -> Result<Option<AppSecrets>> {
        let Some(want) = &def.secrets else { return Ok(None) };
        let versions = want.versions_text();
        let fresh = |b: &crate::secrets::Bundle| b.expires_at.is_none_or(|t| t - crate::util::now_unix() > 600);
        let kept = self.secret_values.lock().get(&want.scope).filter(|(v, b)| *v == versions && fresh(b)).map(|(_, b)| Arc::clone(b));
        let bundle = match kept {
            Some(b) => b,
            None => {
                let scope = crate::secrets::Scope::from_key(&want.scope).with_context(|| format!("`{}` isn't a secrets scope", want.scope))?;
                let pinned: Vec<(String, u32)> = want.versions.iter().map(|(n, v)| (n.clone(), *v)).collect();
                let why = crate::secrets::Use { by: "kilncd".into(), purpose: format!("render {}", def.id()) };
                let b = Arc::new(crate::secrets::of(&self.hub).resolve(&scope, &pinned, &why).await.context("the app's secrets")?);
                self.secret_values.lock().insert(want.scope.clone(), (versions, Arc::clone(&b)));
                b
            }
        };
        Ok(Some(AppSecrets { name: want.name.clone(), bundle }))
    }

    /// Whether kiln knows a cluster by this name.
    pub(crate) fn has_cluster(&self, name: &str) -> bool {
        self.clusters.read().contains_key(name)
    }

    /// Applies `object` to the cluster kiln knows as `cluster` (server-side, as `manager`): what
    /// kiln writes there for something it doesn't render itself (an Argo CD app's secrets).
    pub(crate) async fn apply_to(&self, cluster: &str, object: &Value, manager: &str) -> Result<Value> {
        let c = self.clusters.read().get(cluster).cloned().with_context(|| format!("kiln isn't connected to the cluster {cluster}"))?;
        c.apply(object, manager, false).await
    }

    /// What kiln wrote with [`Cd::apply_to`] in the cluster kiln knows as `cluster`, if it's there.
    pub(crate) async fn get_from(&self, cluster: &str, key: &ResKey) -> Result<Option<Value>> {
        let c = self.clusters.read().get(cluster).cloned().with_context(|| format!("kiln isn't connected to the cluster {cluster}"))?;
        c.get(key).await
    }

    /// Deletes what kiln wrote with [`Cd::apply_to`] in the cluster kiln knows as `cluster`, if
    /// it's there and `ours` says it is kiln's.
    pub(crate) async fn delete_from(&self, cluster: &str, key: &ResKey, ours: impl Fn(&Value) -> bool) -> Result<()> {
        let c = self.clusters.read().get(cluster).cloned().with_context(|| format!("kiln isn't connected to the cluster {cluster}"))?;
        match c.get(key).await? {
            Some(o) if ours(&o) => c.delete(key, "Background").await,
            _ => Ok(()),
        }
    }

    /// An app's secrets for a plan, from memory alone: those it was last rendered with.
    pub(crate) fn planned_secrets(&self, def: &AppDef) -> Option<AppSecrets> {
        let want = def.secrets.as_ref()?;
        if let Some((_, b)) = self.secret_values.lock().get(&want.scope).filter(|(v, _)| *v == want.versions_text()) {
            return Some(AppSecrets { name: want.name.clone(), bundle: Arc::clone(b) });
        }
        self.apps.read().get(&def.id()).and_then(|e| e.resolved.back().and_then(|r| r.secrets.clone()))
    }

    /// The pull secrets an app's artifacts ask for, with their registries' credentials now.
    async fn pull_secrets_of(&self, def: &AppDef, revision: &str) -> Result<Vec<PullSecret>> {
        let mut out = Vec::new();
        for (name, given) in &def.artifacts {
            // A local published image is on the nodes: nothing to pull with.
            let spec = match (&given.published, self.published_spec(given)) {
                (Some(_), Some(s)) => s,
                (Some(_), None) => continue,
                (None, _) => given.clone(),
            };
            let spec = &spec;
            let Some(ps) = &spec.pull_secret else { continue };
            let reference = oci::parse_reference(&spec.reference(revision)).with_context(|| format!("artifact {name}"))?;
            let registry = self.artifact_registry(spec, &reference).await.with_context(|| format!("artifact {name}"))?;
            let config = oci::docker_config(&registry)
                .with_context(|| format!("artifact {name} asks for pull secret {ps}, but no registry integration holds credentials for {}", reference.registry))?;
            out.push(PullSecret { name: ps.clone(), config });
        }
        Ok(out)
    }

    /// A rolled-back app's artifacts: exactly what its deployment recorded; else (a deployment
    /// kept before artifacts were) its revision's now.
    async fn held_artifacts(&self, def: &AppDef, id: &str, held: &Resolved) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        let recorded = self
            .store
            .read(|d| d.pins.get(id).and_then(|p| d.history.get(id).and_then(|h| h.iter().find(|x| x.id == p.to)).map(|x| x.artifacts.clone())))
            .unwrap_or_default();
        self.recorded_artifacts(def, recorded, &held.revision).await
    }

    /// `recorded` when it covers every artifact the app has, else the revision's resolved now.
    pub(crate) async fn recorded_artifacts(&self, def: &AppDef, recorded: Vec<ArtifactRef>, revision: &str) -> Result<(Vec<ArtifactRef>, Vec<PullSecret>)> {
        if recorded.is_empty() || !def.artifacts.keys().all(|n| recorded.iter().any(|a| &a.name == n)) {
            return self.resolve_artifacts(def, revision).await;
        }
        Ok((recorded, self.pull_secrets_of(def, revision).await?))
    }

    /// Artifacts for a plan, without asking any registry (a planned commit's image may not be
    /// built yet): the app's own when the revision is one it resolved, else each image by its tag.
    pub(crate) fn planned_artifacts(&self, def: &AppDef, revision: &str) -> (Vec<ArtifactRef>, Vec<PullSecret>) {
        let apps = self.apps.read();
        let entry = apps.get(&def.id());
        if let Some(r) = entry.and_then(|e| e.resolved.iter().rev().find(|r| r.revision == revision && r.artifacts.len() == def.artifacts.len())) {
            return (r.artifacts.clone(), r.pull_secrets.clone());
        }
        let secrets = entry.and_then(|e| e.resolved.back().map(|r| r.pull_secrets.clone())).unwrap_or_default();
        let refs = def
            .artifacts
            .iter()
            .map(|(name, given)| {
                let spec = match (&given.published, self.published_spec(given)) {
                    (Some(_), Some(s)) => s,
                    (Some(p), None) => return ArtifactRef { name: name.clone(), reference: format!("{p}:{revision}"), repository: p.clone(), tag: Some(revision.to_owned()), digest: String::new() },
                    (None, _) => given.clone(),
                };
                let text = spec.reference(revision);
                let (repository, tag) = oci::parse_reference(&text).map(|r| (r.name(), r.tag.clone())).unwrap_or_else(|_| (text.clone(), None));
                ArtifactRef { name: name.clone(), reference: text, repository, tag, digest: String::new() }
            })
            .collect();
        (refs, secrets)
    }

    // ---------------------------------------------------------------- renders

    async fn render(self: Arc<Self>, key: u64, app: String) {
        let entry = self.apps.read().get(&app).cloned();
        let Some(entry) = entry else {
            self.input(Input::Rendered { key, result: Err(format!("no app {app}")) }).await;
            return;
        };
        let cluster = self.clusters.read().get(&entry.def.destination.cluster).cloned();
        let (kube_version, api_versions) = match &cluster {
            Some(c) => (c.version.read().clone(), c.discovery.read().api_versions.clone()),
            None => (None, Vec::new()),
        };
        // The commits this key stands for, even if git moved since it was resolved.
        let inputs = entry.resolved.iter().find(|r| r.key == key).cloned().unwrap_or_else(|| Resolved {
            key,
            commits: entry.commits.clone(),
            deps: entry.deps.clone(),
            ..Resolved::default()
        });
        let this = self.clone();
        let def = entry.def.clone();
        let result = crate::rt::blocking(move || this.render_app(&def, &inputs, key, kube_version.as_deref(), &api_versions)).await.and_then(|r| r);
        match result {
            Ok((objects, children)) => {
                self.adopt_children(&app, children).await;
                self.input(Input::Rendered { key, result: Ok(objects) }).await;
            }
            Err(e) => {
                tracing::warn!(app = %app, error = %format!("{e:#}"), "deployments: render failed");
                self.input(Input::Rendered { key, result: Err(format!("{e:#}")) }).await;
            }
        }
    }

    /// An app's definition, as kiln holds it now.
    pub(crate) fn definition(&self, id: &str) -> Option<AppDef> {
        self.apps.read().get(id).map(|e| e.def.clone())
    }

    /// What `def` renders: its sources read at their revisions as an app's are, its artifacts as a
    /// plan takes them (no registry is asked). A release previewed before it is released
    /// (docs/deploy-api.md §8.1).
    pub(crate) async fn render_definition(self: &Arc<Self>, def: AppDef) -> Result<Vec<Value>> {
        let cluster = self.clusters.read().get(&def.destination.cluster).cloned();
        let (kube_version, api_versions) = match &cluster {
            Some(c) => (c.version.read().clone(), c.discovery.read().api_versions.clone()),
            None => (None, Vec::new()),
        };
        let secrets = self.app_secrets(&def).await?;
        let cd = Arc::clone(self);
        crate::rt::blocking(move || {
            let mut r = cd.inputs_of(&def, kube_version.as_deref(), &api_versions, None)?;
            let (artifacts, pull_secrets) = cd.planned_artifacts(&def, &r.revision);
            r.add_artifacts(artifacts, pull_secrets);
            r.add_secrets(secrets);
            cd.render_app(&def, &r, r.key, kube_version.as_deref(), &api_versions).map(|(objects, _children)| objects)
        })
        .await
        .and_then(|r| r)
    }

    /// Renders every source of an app into one list; Argo CD Applications among the objects are
    /// its children (app of apps), returned apart.
    fn render_app(&self, def: &AppDef, inputs: &Resolved, key: u64, kube_version: Option<&str>, api_versions: &[String]) -> Result<(Vec<Value>, Vec<Value>)> {
        // A directory of its own: a plan may render a key while the app renders it too.
        static VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let views = self.dir.join("views").join(format!("{key:016x}-{n}"));
        let _ = std::fs::remove_dir_all(&views);
        // Ref sources' files, by name.
        let mut refs: BTreeMap<String, PathBuf> = BTreeMap::new();
        for (i, s) in def.sources.iter().enumerate() {
            let Some(name) = &s.as_ref else { continue };
            let commit = inputs.commits.get(i).cloned().flatten().context("a ref source's commit is unknown")?;
            let wanted: Vec<String> = inputs.deps.iter().flatten().filter(|d| d.source.as_deref() == Some(name)).map(|d| d.path.clone()).collect();
            let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
            let root = views.join(format!("ref-{name}"));
            let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
            m.extract(&commit, &wanted, &root)?;
            refs.insert(name.clone(), root);
        }
        // The revision's artifacts, filled in (`${{ artifacts.<name> }}`).
        let sources = apps::with_artifacts(&def.sources, &inputs.artifacts).map_err(|e| anyhow::anyhow!("{}", e.0))?;
        let mut objects = Vec::new();
        for (i, s) in sources.iter().enumerate() {
            if s.as_ref.is_some() && s.chart.is_none() {
                continue;
            }
            let root = views.join(format!("src-{i}"));
            let commit = inputs.commits.get(i).cloned().flatten().unwrap_or_else(|| s.revision.clone());
            if s.chart.is_none() {
                let m = Mirror::open(&self.mirror_dir(&s.repo), &s.repo, self.git_auth(&s.repo))?;
                let own: Vec<&str> = inputs.deps.get(i).into_iter().flatten().filter(|d| d.source.is_none()).map(|d| d.path.as_str()).collect();
                m.extract(&commit, &own, &root)?;
            }
            std::fs::create_dir_all(&root)?;
            let req = render::Request {
                source: s,
                root: &root,
                refs: &refs,
                app: &def.name,
                namespace: &def.destination.namespace,
                revision: &commit,
                kube_version,
                api_versions,
                scratch: &views.join("scratch"),
            };
            objects.extend(render::render(&req, &self.tools)?);
        }
        // Plain manifests read them too: `image: ${{ artifacts.web }}` in a Deployment.
        if !inputs.artifacts.is_empty() {
            for o in &mut objects {
                apps::substitute_value(o, &inputs.artifacts).map_err(|e| anyhow::anyhow!("{} {}: {}", obj::kind(o), obj::name(o), e.0))?;
            }
        }
        for p in &inputs.pull_secrets {
            objects.push(apps::pull_secret(&p.name, &def.destination.namespace, &p.config));
        }
        // Its secrets: on a cluster one Secret, and the pods that read it marked with their
        // versions; on a platform, references its provider reads the values of as it applies.
        let platform = kiln_cd::providers::is_target(&def.destination.cluster);
        if platform && let Some(o) = objects.iter().find(|o| apps::holds_secret_ref(o)) {
            anyhow::bail!("{} {} holds `{}`, which is kiln's: an app's secrets reach it from its secrets (docs/deploy-api.md §3.5)", obj::kind(o), obj::name(o), apps::SECRET_REF);
        }
        if let Some(s) = &inputs.secrets {
            if platform {
                let scope = def.secrets.as_ref().map(|r| r.scope.as_str()).unwrap_or_default();
                let versions: Vec<(String, u32)> = s.bundle.items.iter().map(|(n, v, _)| (n.clone(), *v)).collect();
                apps::platform_secrets(&mut objects, scope, &versions).map_err(|e| anyhow::anyhow!("its secrets: {e}"))?;
            } else {
                let versions = s.bundle.versions();
                objects.push(apps::app_secret(&s.name, &def.destination.namespace, &versions, s.bundle.items.iter().map(|(n, _, v)| (n.as_str(), v.bytes()))));
                apps::stamp_secrets(&mut objects, &s.name, &def.destination.namespace, &versions);
            }
        }
        apps::stamp_rollout(def, &mut objects);
        let _ = std::fs::remove_dir_all(&views);
        let (children, objects): (Vec<Value>, Vec<Value>) =
            objects.into_iter().partition(|o| obj::group(o) == "argoproj.io" && obj::kind(o) == "Application");
        Ok((objects, children))
    }

    /// Registers the Applications an app rendered as its children, and forgets the ones it no
    /// longer renders. One kept in kiln (taken over, or made through the API) is kiln's own: the
    /// parent rendering it changes nothing, and it is never the parent's to forget.
    async fn adopt_children(&self, parent: &str, children: Vec<Value>) {
        let (clusters, kept) = self.store.read(|d| (d.clusters.clone(), d.apps.keys().cloned().collect::<BTreeSet<String>>()));
        let by_url = |url: &str| clusters.values().find(|c| c.server.trim_end_matches('/') == url.trim_end_matches('/')).map(|c| c.name.clone());
        let defs: Vec<AppDef> = children
            .iter()
            .filter_map(|c| {
                apps::from_argo(c, Some(parent), &by_url)
                    .map_err(|e| tracing::warn!(parent = %parent, error = %e.0, "deployments: a rendered Application can't be read"))
                    .ok()
            })
            .collect();
        let defs = rendered_children(defs, &kept);
        let ids: BTreeSet<String> = defs.iter().map(AppDef::id).collect();
        for def in defs {
            self.register(def).await;
        }
        let (gone, prune) = {
            let mut apps = self.apps.write();
            let Some(p) = apps.get_mut(parent) else { return };
            let gone = forgotten(&p.children, &ids, &kept);
            p.children = ids;
            (gone, p.def.sync.prune)
        };
        for g in gone {
            let cascade = prune && self.apps.read().get(&g).is_some_and(|e| e.def.cascade);
            self.unregister(&g, cascade).await;
        }
    }

    // ---------------------------------------------------------------- clusters

    /// The endpoint of a registered cluster.
    fn endpoint(&self, c: &ClusterDef) -> Result<kiln_cd::kube::Endpoint> {
        use kiln_cd::kube::{Auth, Endpoint, Exec};
        let base = || Endpoint {
            server: c.server.trim_end_matches('/').to_owned(),
            ca: c.ca.as_ref().map(|s| s.clone().into_bytes()),
            insecure: c.insecure,
            ..Endpoint::default()
        };
        Ok(match &c.auth {
            ClusterAuth::InCluster => kiln_cd::kube::in_cluster().context("kiln doesn't run in a Kubernetes pod")?,
            ClusterAuth::Token { token } => Endpoint { auth: Auth::Token(token.clone()), ..base() },
            ClusterAuth::Certificate { cert, key } => Endpoint { auth: Auth::Certificate { cert: cert.clone().into_bytes(), key: key.clone().into_bytes() }, ..base() },
            ClusterAuth::Exec { command, args, env } => Endpoint {
                auth: Auth::Exec(Exec {
                    command: command.clone(),
                    args: args.clone(),
                    env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    api_version: "client.authentication.k8s.io/v1".into(),
                    provide_cluster_info: false,
                }),
                ..base()
            },
            ClusterAuth::Kubeconfig { path, context } => kubeconfig_endpoint(path, context)?,
        })
    }

    /// Every cluster to connect: the registered ones, and the one kiln runs in (or stands in for
    /// it, outside Kubernetes) as `in-cluster`.
    fn cluster_defs(&self) -> Vec<ClusterDef> {
        let mut defs: Vec<ClusterDef> = self.store.read(|d| d.clusters.values().cloned().collect());
        if !defs.iter().any(|c| c.name == apps::IN_CLUSTER) {
            if kiln_cd::kube::in_cluster().is_some() {
                defs.push(ClusterDef { name: apps::IN_CLUSTER.into(), server: apps::IN_CLUSTER_URL.into(), ..ClusterDef::default() });
            } else if !self.in_cluster_context.is_empty() {
                defs.push(ClusterDef {
                    name: apps::IN_CLUSTER.into(),
                    server: apps::IN_CLUSTER_URL.into(),
                    auth: ClusterAuth::Kubeconfig { path: self.kubeconfig.clone(), context: self.in_cluster_context.clone() },
                    ..ClusterDef::default()
                });
            }
        }
        defs
    }

    async fn connect_clusters(self: &Arc<Self>) {
        for def in self.cluster_defs() {
            if self.clusters.read().contains_key(&def.name) || !self.connecting.lock().insert(def.name.clone()) {
                continue;
            }
            let this = self.clone();
            crate::rt::spawn(async move { this.connect(def).await });
        }
    }

    async fn connect(self: Arc<Self>, def: ClusterDef) {
        let mut wait = Duration::from_secs(2);
        loop {
            let attempt = async {
                let ep = self.endpoint(&def)?;
                let cluster = Arc::new(Cluster::new(&def.name, ep)?);
                let d = cluster.discover().await?;
                Ok::<_, anyhow::Error>((cluster, d))
            };
            match attempt.await {
                Ok((cluster, d)) => {
                    tracing::info!(cluster = %def.name, kinds = d.kinds.len(), version = ?cluster.version.read(), "deployments: cluster connected");
                    self.input(Input::UpsertCluster { name: def.name.as_str().into(), qps: def.qps, inflight: def.inflight }).await;
                    for (gk, r) in &d.kinds {
                        self.input(Input::Scope { cluster: def.name.as_str().into(), kind: gk.clone(), namespaced: r.namespaced }).await;
                    }
                    self.clusters.write().insert(def.name.clone(), cluster.clone());
                    self.input(Input::ClusterReachable { name: def.name.as_str().into(), reachable: true }).await;
                    // kiln's own cluster: a self-deploy in flight before this start finishes here.
                    if def.name == apps::IN_CLUSTER {
                        let this = self.clone();
                        crate::rt::spawn(async move { this.finish_self_deploy(&cluster).await });
                    }
                    self.connecting.lock().remove(&def.name);
                    self.kick();
                    return;
                }
                Err(e) => {
                    tracing::warn!(cluster = %def.name, error = %format!("{e:#}"), "deployments: can't reach a cluster yet");
                    self.input(Input::ClusterReachable { name: def.name.as_str().into(), reachable: false }).await;
                }
            }
            if !self.hub.shutdown.sleep(wait).await {
                return;
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    }

    // ---------------------------------------------------------------- watches

    /// Objects of kiln's apps carry its label, so the API server filters for kiln; apps
    /// adopted from Argo CD don't, so a cluster with any of them is watched unfiltered.
    fn selector(&self, cluster: &str) -> Option<&'static str> {
        let any_argo = self.apps.read().values().any(|e| e.def.destination.cluster == cluster && e.def.tracking != Tracking::Kiln);
        (!any_argo).then(kiln_cd::kube::kiln_selector)
    }

    fn watch(self: &Arc<Self>, cluster: String, kind: GroupKind) {
        if !self.watched.lock().insert((cluster.clone(), kind.clone())) {
            return;
        }
        let this = self.clone();
        crate::rt::spawn(async move { this.watch_loop(cluster, kind).await });
    }

    async fn watch_loop(self: Arc<Self>, cluster: String, kind: GroupKind) {
        let name: obj::Str = cluster.as_str().into();
        let mut wait = Duration::from_secs(1);
        loop {
            if self.hub.shutdown.is_cancelled() {
                return;
            }
            let Some(c) = self.clusters.read().get(&cluster).cloned() else {
                if !self.hub.shutdown.sleep(Duration::from_secs(2)).await {
                    return;
                }
                continue;
            };
            if !self.serves(&c, &kind, Duration::ZERO).await {
                // The cluster serves no such kind (yet), so none of it exists: say so, or what
                // waits for the kind to be read waits forever. Look again later: its CRD may be
                // what a sync is about to apply.
                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Listed { kind: kind.clone(), objects: Vec::new() } }).await;
                if !self.hub.shutdown.sleep(wait).await {
                    return;
                }
                wait = (wait * 2).min(Duration::from_secs(10));
                continue;
            }
            let selector = self.selector(&cluster);
            let listed = c.list(&kind, selector).await;
            let (objects, mut rv) = match listed {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(cluster = %cluster, kind = %kind, error = %format!("{e:#}"), "deployments: list failed");
                    if !self.hub.shutdown.sleep(wait).await {
                        return;
                    }
                    wait = (wait * 2).min(Duration::from_secs(60));
                    continue;
                }
            };
            wait = Duration::from_secs(1);
            self.input(Input::Live { cluster: name.clone(), change: LiveChange::Listed { kind: kind.clone(), objects } }).await;
            // Watch from the list's version; a dropped stream resumes from the last version seen,
            // and only a 410 lists again. While the engine's inbox is full the stream isn't read: the
            // API server holds what follows (goal 11, plan 01a1a87b2e56 step 3).
            'watch: loop {
                let Ok(path) = c.watch_path(&kind, &rv, selector) else { return };
                let mut resp = match c.stream(&path).await {
                    Ok(r) if r.status().is_success() => r,
                    _ => break 'watch,
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut idle = Duration::ZERO;
                loop {
                    // An app that tracks as Argo CD does needs objects without kiln's label: what
                    // the cluster is watched for changed, and it's listed again at once.
                    if self.selector(&cluster) != selector {
                        break 'watch;
                    }
                    let chunk = crate::rt::timeout(Duration::from_secs(5), kube::next_chunk(resp.body_mut())).await;
                    let bytes = match chunk {
                        Ok(Ok(Some(bytes))) => bytes,
                        // Quiet: the stream is watched again after a while, as before.
                        Err(_) if idle < Duration::from_secs(330) => {
                            idle += Duration::from_secs(5);
                            continue;
                        }
                        _ => continue 'watch,
                    };
                    idle = Duration::ZERO;
                    buf.extend_from_slice(&bytes);
                    for line in kiln_cd::kube::take_lines(&mut buf) {
                        match kiln_cd::kube::watch_event(&line) {
                            Some(kiln_cd::kube::WatchEvent::Upsert(v)) => {
                                if let Some(r) = obj::str_at(&v, &["metadata", "resourceVersion"]) {
                                    rv = r.to_owned();
                                }
                                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Upsert(v) }).await;
                            }
                            Some(kiln_cd::kube::WatchEvent::Delete(v)) => {
                                if let Some(r) = obj::str_at(&v, &["metadata", "resourceVersion"]) {
                                    rv = r.to_owned();
                                }
                                let key = ResKey::new(obj::group(&v), obj::kind(&v), obj::namespace(&v), obj::name(&v));
                                self.input(Input::Live { cluster: name.clone(), change: LiveChange::Delete(key) }).await;
                            }
                            Some(kiln_cd::kube::WatchEvent::Bookmark(r)) => rv = r,
                            Some(kiln_cd::kube::WatchEvent::Expired) => break 'watch,
                            Some(kiln_cd::kube::WatchEvent::Error(e)) => {
                                tracing::debug!(cluster = %cluster, kind = %kind, error = %e, "deployments: watch error");
                                break 'watch;
                            }
                            None => {}
                        }
                    }
                    if self.hub.shutdown.is_cancelled() {
                        return;
                    }
                }
            }
        }
    }

    /// Whether `c` serves `kind`, discovering again when it doesn't seem to: a CRD applied since
    /// the last discovery serves new kinds. With `wait`, keeps looking that long (a CRD just
    /// applied takes a moment to be served). Discovery runs at most once a second per cluster,
    /// and what it finds goes to the engine.
    async fn serves(&self, c: &Arc<Cluster>, kind: &GroupKind, wait: Duration) -> bool {
        let until = Instant::now() + wait;
        loop {
            if c.knows(kind) {
                return true;
            }
            if c.claim_discovery(Duration::from_secs(1)) {
                match c.discover().await {
                    Ok(d) => {
                        for (gk, r) in &d.kinds {
                            self.input(Input::Scope { cluster: c.name.as_str().into(), kind: gk.clone(), namespaced: r.namespaced }).await;
                        }
                        if c.knows(kind) {
                            return true;
                        }
                    }
                    Err(e) => tracing::debug!(cluster = %c.name, error = %format!("{e:#}"), "deployments: discovery failed"),
                }
            }
            if Instant::now() >= until || !self.hub.shutdown.sleep(Duration::from_millis(500)).await {
                return false;
            }
        }
    }

    // ---------------------------------------------------------------- actions

    /// Does what the engine asks, taking each from its bounded queue ([`ACTIONS_BYTES`]). It never
    /// waits on the engine's inbox itself (what answers it is a task of its own), so the engine's
    /// thread, waiting on this queue, always goes on.
    async fn dispatch(self: Arc<Self>, rx: crate::memory::queue::Receiver<Action>) {
        while let Ok(a) = rx.recv().await {
            match a {
                Action::Render { key, app, .. } => {
                    let this = self.clone();
                    crate::rt::spawn(this.render(key, app.to_string()));
                }
                Action::Watch { cluster, kind } if kiln_cd::providers::is_target(&cluster) => self.target_watch(cluster.to_string(), kind),
                Action::Watch { cluster, kind } => self.watch(cluster.to_string(), kind),
                Action::Apply { id, cluster, object, dry_run, .. } if kiln_cd::providers::is_target(&cluster) => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let result = this.target_apply(&cluster, &object, dry_run).await;
                        this.input(Input::Answer { id, result }).await;
                    });
                }
                Action::Delete { id, cluster, key, .. } if kiln_cd::providers::is_target(&cluster) => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let result = this.target_delete(&cluster, &key).await.map(|()| None);
                        this.input(Input::Answer { id, result }).await;
                    });
                }
                Action::Apply { id, cluster, object, dry_run, manager, .. } => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let c = this.clusters.read().get(cluster.as_ref()).cloned();
                        let result = match c {
                            Some(c) => {
                                // A kind discovery doesn't know may be a CRD's this very sync just
                                // applied: look again, and give a real apply a moment for the CRD
                                // to be served. A dry run doesn't wait; the engine knows a miss
                                // the sync answers itself.
                                let gk = GroupKind::new(obj::group(&object), obj::kind(&object));
                                let wait = if dry_run { Duration::ZERO } else { Duration::from_secs(15) };
                                this.serves(&c, &gk, wait).await;
                                // kiln's own Deployment: a guard watches the restart first.
                                let guarded = if dry_run || cluster.as_ref() != apps::IN_CLUSTER { Ok(()) } else { this.before_self_apply(&object, &c).await };
                                match guarded {
                                    Ok(()) => {
                                        let answered = c.apply(&object, &manager, dry_run).await;
                                        if let (false, Ok(answer)) = (dry_run, &answered) {
                                            this.custody.applied(&object, answer, &manager);
                                        }
                                        answered.map(Some).map_err(|e| format!("{e:#}"))
                                    }
                                    Err(e) => Err(format!("not deploying kiln itself without its guard: {e:#}")),
                                }
                            }
                            None => Err(format!("cluster {cluster} is not connected")),
                        };
                        this.input(Input::Answer { id, result }).await;
                    });
                }
                Action::Delete { id, cluster, key, propagation } => {
                    let this = self.clone();
                    crate::rt::spawn(async move {
                        let c = this.clusters.read().get(cluster.as_ref()).cloned();
                        let result = match c {
                            Some(c) => c.delete(&key, propagation.as_str()).await.map(|_| None).map_err(|e| format!("{e:#}")),
                            None => Err(format!("cluster {cluster} is not connected")),
                        };
                        this.input(Input::Answer { id, result }).await;
                    });
                }
                Action::Status(s) => {
                    let app = s.app.to_string();
                    let before = self.statuses.write().insert(app.clone(), s.clone());
                    crate::observe::kiln::cd_status(before.as_ref(), &s);
                    self.dirty.lock().insert(app);
                }
                Action::Record(r) => {
                    custody::on_record(&self, &r);
                    if let kiln_cd::engine::Record::Started { app, op, revision, trigger, by, at_ms } = &r {
                        let s = guard::Started { op: *op, revision: revision.as_ref().map(|r| r.to_string()), trigger: trigger.as_str().to_owned(), by: by.clone(), at_ms: *at_ms };
                        self.started.lock().insert(app.to_string(), s);
                    }
                    self.record(r)
                }
            }
        }
    }

    fn record(&self, r: Record) {
        let Record::Finished { app, op, revision, key, content, trigger, by, succeeded, message, applied, pruned, at_ms } = r else { return };
        if !succeeded {
            tracing::info!(app = %app, op, message = %message, "deployments: a sync failed");
        }
        let (limit, commits, artifacts, dora) = {
            let apps = self.apps.read();
            let e = apps.get(app.as_ref());
            let r = key.and_then(|k| e.and_then(|e| e.resolved.iter().find(|r| r.key == k)));
            let commits = r.map(|r| r.commits.clone()).unwrap_or_default();
            let artifacts = r.map(|r| r.artifacts.clone()).unwrap_or_default();
            // What DORA's deployment says (docs/observe.md §10.7): its environment, its first git
            // source and that source's commit, its team.
            let dora = e.map(|e| {
                let source = e.def.sources.iter().zip(commits.iter()).find_map(|(s, c)| c.clone().map(|c| (s.repo.clone(), c)));
                (e.def.environment.clone(), source, e.def.labels.get("team").cloned())
            });
            (e.map(|e| e.def.history_limit).unwrap_or(10), commits, artifacts, dora)
        };
        let started_ms = self.started.lock().get(app.as_ref()).filter(|s| s.op == op).map(|s| s.at_ms);
        let d = Deployment {
            app: app.to_string(),
            op,
            revision: revision.map(|r| r.to_string()),
            key,
            trigger: trigger.as_str().to_owned(),
            by,
            succeeded,
            message,
            applied,
            pruned,
            at_ms,
            id: 0,
            commits,
            content,
            artifacts,
        };
        crate::observe::kiln::deployed(&d);
        if let Some((env, source, team)) = &dora {
            crate::observe::dora::cd_deployed(&d, started_ms, env.as_deref(), source.as_ref().map(|(r, c)| (r.as_str(), c.as_str())), team.as_deref());
        }
        let rx = self.store.submit(Command::Deployed { deployment: Box::new(d), limit });
        crate::rt::spawn(async move {
            if let Ok(Err(e)) = rx.await {
                tracing::warn!(error = %e, "deployments: recording a deployment failed");
            }
        });
    }

    /// Publishes changed statuses at most every 300 ms, in one event.
    async fn flush_loop(self: Arc<Self>) {
        while self.hub.shutdown.sleep(Duration::from_millis(300)).await {
            let dirty: Vec<String> = std::mem::take(&mut *self.dirty.lock()).into_iter().collect();
            if dirty.is_empty() {
                continue;
            }
            let statuses = self.statuses.read();
            let apps: Vec<crate::wire::events::CdStatus> = dirty.iter().filter_map(|a| statuses.get(a)).map(crate::wire::events::CdStatus::of).collect();
            drop(statuses);
            for chunk in apps.chunks(500) {
                self.hub.emit(&crate::wire::events::ServerEvent::CdApps { apps: chunk.to_vec() });
            }
        }
    }
}

/// The children an app of apps rendered that it registers: none kept in kiln (taken over, or made
/// through the API), which is kiln's own whatever its parent renders.
fn rendered_children(defs: Vec<AppDef>, kept: &BTreeSet<String>) -> Vec<AppDef> {
    defs.into_iter().filter(|d| !kept.contains(&d.id())).collect()
}

/// The children an app of apps no longer renders, to forget (`before`: its children until now):
/// never one kept in kiln.
fn forgotten(before: &BTreeSet<String>, now: &BTreeSet<String>, kept: &BTreeSet<String>) -> Vec<String> {
    before.difference(now).filter(|g| !kept.contains(*g)).cloned().collect()
}

/// Apps adopted from Argo CD are shadowed (compared, never written) until they are taken over.
pub fn is_shadow(def: &AppDef) -> bool {
    matches!(def.origin, Origin::Argo { .. }) && def.labels.get("kiln.dev/take-over").map(String::as_str) != Some("true")
}

/// How many commits back from the head an app looks for the newest whose artifacts are built.
const NEWEST_BUILT: usize = 64;

/// The commits to try in place of a head whose artifacts aren't built, newest first: those after
/// `runs` (the commit the app runs) and before the head, then `runs` itself, never older. `stack`:
/// the commits from `runs` (left out) up to the head, oldest first.
fn in_place_of_head(mut stack: Vec<String>, runs: &str) -> Vec<String> {
    stack.pop();
    stack.reverse();
    stack.push(runs.to_owned());
    stack
}

/// An app's status as the API and events show it.
pub fn summary_json(s: &Summary) -> Value {
    serde_json::to_value(crate::wire::events::CdStatus::of(s)).unwrap_or(Value::Null)
}

/// [`summary_json`] as it was built before [`crate::wire::events::CdStatus`], for its test.
#[cfg(test)]
fn summary_json_was(s: &Summary) -> Value {
    json!({
        "app": s.app.as_ref(),
        "sync": s.sync.as_str(),
        "health": s.health.as_str(),
        "revision": s.revision.as_deref(),
        "resources": s.resources,
        "out_of_sync": s.out_of_sync,
        "shared": s.shared,
        "missing": s.missing,
        "prune": s.prune,
        "unmarked": s.unmarked,
        "message": s.message,
        "operation": s.operation.as_ref().map(|o| json!({
            "id": o.id,
            "phase": format!("{:?}", o.phase),
            "trigger": o.trigger.as_str(),
            "attempt": o.attempt,
            "message": o.message,
            "by": o.by,
            "started_ms": o.started_ms,
            "finished_ms": o.finished_ms,
        })),
    })
}

/// A commit's first seven characters.
fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn blake(parts: &[&[u8]]) -> u64 {
    let mut joined = Vec::new();
    for p in parts {
        joined.extend_from_slice(&(p.len() as u64).to_le_bytes());
        joined.extend_from_slice(p);
    }
    obj::hash_bytes(&joined)
}

/// The endpoint of a kubeconfig context (the default kubeconfig when `path` is empty).
pub fn kubeconfig_endpoint(path: &str, context: &str) -> Result<kiln_cd::kube::Endpoint> {
    let path = if path.is_empty() {
        std::env::var("KUBECONFIG")
            .ok()
            .and_then(|p| p.split(':').next().map(PathBuf::from))
            .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".kube/config")))
            .context("no kubeconfig: set KUBECONFIG or give a path")?
    } else {
        PathBuf::from(path)
    };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    kiln_cd::kube::kubeconfig(&text, context, path.parent().unwrap_or(std::path::Path::new(".")))
}

/// The engine's thread: its inbox's messages in (their bytes let go as each is taken), what it asks
/// for out, into a queue bounded at [`ACTIONS_BYTES`] that it waits on when full (its own thread's
/// wait, never the runtime's), so its inputs wait behind it.
fn engine_thread(rx: Inflow, out: crate::memory::queue::Sender<Action>, cfg: EngineConfig) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut engine = Engine::new(cfg);
    let mut actions = Vec::new();
    loop {
        let wait = engine.deadline().map(|d| d.saturating_sub(now_ms())).unwrap_or(1_000).clamp(1, 1_000);
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(ToEngine::Input(i)) => engine.handle(now_ms(), *i),
            Ok(ToEngine::Query(f)) => f(&engine),
            Ok(ToEngine::Health(settings)) => set_health(&mut engine, &settings),
            Err(RecvTimeoutError::Timeout) => engine.handle(now_ms(), Input::Tick),
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Take whatever else is waiting before answering, so bursts are handled together.
        for _ in 0..4096 {
            match rx.try_recv() {
                Ok(ToEngine::Input(i)) => engine.handle(now_ms(), *i),
                Ok(ToEngine::Query(f)) => f(&engine),
                Ok(ToEngine::Health(settings)) => set_health(&mut engine, &settings),
                Err(_) => break,
            }
        }
        engine.drain(&mut actions);
        for a in actions.drain(..) {
            if out.send_blocking(a).is_err() {
                return;
            }
        }
    }
}

/// Gives the engine Argo CD's health checks for custom resources, settings' scripts first. An
/// object's health follows the new rules the next time it changes.
fn set_health(engine: &mut Engine, settings: &[(String, String)]) {
    match kiln_cd::lua::LuaRules::from_settings(settings.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
        Ok(rules) => engine.set_rules(Box::new(rules)),
        Err(e) => tracing::warn!(error = %e, "deployments: no Lua for health checks; custom resources use built-in rules only"),
    }
}

/// Starts the service and runs until the server stops.
pub async fn run(hub: Arc<Hub>) -> Result<()> {
    let cfg = hub.cfg.cd.clone();
    let dir = hub.cfg.server.data_dir.join("cd");
    let store_dir = dir.join("store");
    let store = crate::rt::blocking(move || Store::open(&store_dir, 1, &[1])).await??;
    let changes = store.changes();
    // Both bounded in bytes (goal 11, plan 01a1a87b2e56 step 3): what the engine is told, and what it
    // asks for.
    let (etx, erx) = inbox(INPUTS_BYTES);
    let (atx, arx) = crate::memory::queue::bounded::<Action>(&crate::memory::owners::CD_ACTIONS, ACTIONS_BYTES);
    let ecfg = EngineConfig { wave_delay_ms: cfg.wave_delay_ms, cluster_qps: cfg.qps, cluster_burst: cfg.qps * 2.0, ..EngineConfig::default() };
    etx.send(ToEngine::Health(health_settings(&store))).await;
    std::thread::Builder::new().name("kiln-cd-engine".into()).spawn(move || engine_thread(erx, atx, ecfg))?;
    let tools = Tools {
        helm: cfg.helm.clone().into(),
        kubectl: cfg.kubectl.clone().into(),
        kustomize: (!cfg.kustomize.is_empty()).then(|| cfg.kustomize.clone().into()),
        kustomize_options: cfg.kustomize_options.clone(),
        timeout: Duration::from_secs(cfg.render_timeout_seconds.max(5)),
        helm_home: Some(dir.join("helm")),
        // kiln's own charts (kiln-base), under every name its registry goes by: laid in, not pulled.
        local_charts: std::iter::once(crate::githost::host_of(&hub.cfg.server.public_url))
            .chain(hub.cfg.ci.registry.hosts.iter().cloned())
            .map(|host| (crate::deploy::init::charts_repository(&host), crate::deploy::init::local_charts(&hub.cfg)))
            .collect(),
        ..Tools::default()
    };
    let cd = Arc::new(Cd {
        hub: hub.clone(),
        store,
        engine: etx,
        apps: RwLock::new(BTreeMap::new()),
        clusters: RwLock::new(BTreeMap::new()),
        statuses: RwLock::new(BTreeMap::new()),
        dirty: Mutex::new(BTreeSet::new()),
        deleting: Mutex::new(BTreeMap::new()),
        watched: Mutex::new(BTreeSet::new()),
        connecting: Mutex::new(BTreeSet::new()),
        inputs_gate: Mutex::new(()),
        dir,
        tools,
        poll: Duration::from_secs(cfg.poll_seconds.max(5)),
        in_cluster_context: cfg.in_cluster_context.clone(),
        kubeconfig: cfg.kubeconfig.clone(),
        kick: crate::rt::Notify::new(),
        live: live::Live::default(),
        plans: plans::Renders::default(),
        artifacts: Mutex::new(HashMap::new()),
        secret_values: Mutex::new(HashMap::new()),
        deploy_errors: Mutex::new(BTreeMap::new()),
        environments: RwLock::new(BTreeMap::new()),
        started: Mutex::new(HashMap::new()),
        targets: RwLock::new(BTreeMap::new()),
        target_errors: Mutex::new(BTreeMap::new()),
        repo_names: RwLock::new(HashMap::new()),
        rollouts: rollouts::Drivers::default(),
        custody: custody::Custody::default(),
    });
    *CD.write() = Some(cd.clone());
    // What the engine asks for is taken from the start: registering waits for room in its inbox, and
    // the engine for room in its queue of what it asks (goal 11, plan 01a1a87b2e56 step 3), so the
    // queue is drained before the first app is registered.
    crate::rt::spawn(cd.clone().dispatch(arx));
    // Apps kept in kiln, then the clusters they deploy to.
    let kept: Vec<AppDef> = cd.store.read(|d| d.apps.values().cloned().collect());
    for def in kept {
        cd.register(def).await;
    }
    cd.connect_clusters().await;
    crate::rt::spawn(cd.clone().poll_loop());
    crate::rt::spawn(cd.clone().flush_loop());
    crate::rt::spawn(custody::run(cd.clone()));
    cd.resume_rollouts();
    // The store's changes (from this server or, later, other members) reach the registry.
    if let Some(changes) = changes {
        let (ctx, crx) = async_channel::unbounded();
        std::thread::Builder::new().name("kiln-cd-changes".into()).spawn(move || {
            while let Ok(c) = changes.recv() {
                if ctx.send_blocking(c).is_err() {
                    return;
                }
            }
        })?;
        let c2 = cd.clone();
        crate::rt::spawn(async move {
            while let Ok(change) = crx.recv().await {
                // Ship's Deploys reads what the store keeps (history, holds, settings).
                crate::ship::deploys::heard();
                match change {
                    kiln_cd::state::Change::App(id) => {
                        if let Some(def) = c2.store.read(|d| d.apps.get(&id).cloned()) {
                            c2.register(def).await;
                        }
                    }
                    kiln_cd::state::Change::AppDeleted(id) => {
                        let cascade = c2.deleting.lock().remove(&id).unwrap_or(false);
                        c2.unregister(&id, cascade).await;
                    }
                    kiln_cd::state::Change::Cluster(_) => c2.connect_clusters().await,
                    kiln_cd::state::Change::ClusterDeleted(name) => {
                        c2.clusters.write().remove(&name);
                    }
                    kiln_cd::state::Change::Pinned(_) => c2.kick(),
                    kiln_cd::state::Change::Setting(k) if k.starts_with(HEALTH_SETTINGS) => {
                        c2.engine.send(ToEngine::Health(health_settings(&c2.store))).await;
                    }
                    _ => {}
                }
            }
        });
    }
    tracing::info!(apps = cd.apps.read().len(), "deployments ready");
    let _ = hub.shutdown.cancelled().await;
    *CD.write() = None;
    Ok(())
}

#[cfg(test)]
impl Cd {
    /// A KilnCD of its own over `hub`, its store under `dir`, for tests: never the server's (`get`
    /// doesn't answer it), no cluster connected, nothing polled. Its engine is the one `drive`
    /// leaves, on a thread of its own (the simulated cluster's, `kiln_cd::harness`): it answers
    /// queries, and inputs change nothing. What `drive` gives besides comes back with it.
    pub(crate) fn scratch<T: Send + 'static>(hub: Arc<Hub>, dir: &std::path::Path, drive: impl FnOnce() -> (kiln_cd::harness::Harness, T) + Send + 'static) -> Result<(Arc<Cd>, T)> {
        let store = Store::open(&dir.join("store"), 1, &[1])?;
        let (etx, erx) = inbox(INPUTS_BYTES);
        let (ttx, trx) = std::sync::mpsc::channel::<T>();
        std::thread::Builder::new().name("kiln-cd-scratch".into()).spawn(move || {
            let (h, t) = drive();
            if ttx.send(t).is_err() {
                return;
            }
            while let Ok(m) = erx.recv() {
                if let ToEngine::Query(f) = m {
                    f(&h.engine);
                }
            }
        })?;
        let t = trx.recv().context("the scratch engine's drive ended")?;
        let cd = Arc::new(Cd {
            hub,
            store,
            engine: etx,
            apps: RwLock::new(BTreeMap::new()),
            clusters: RwLock::new(BTreeMap::new()),
            statuses: RwLock::new(BTreeMap::new()),
            dirty: Mutex::new(BTreeSet::new()),
            deleting: Mutex::new(BTreeMap::new()),
            watched: Mutex::new(BTreeSet::new()),
            connecting: Mutex::new(BTreeSet::new()),
            inputs_gate: Mutex::new(()),
            dir: dir.to_path_buf(),
            tools: Tools::default(),
            poll: Duration::from_secs(60),
            in_cluster_context: String::new(),
            kubeconfig: String::new(),
            kick: crate::rt::Notify::new(),
            live: live::Live::default(),
            plans: plans::Renders::default(),
            artifacts: Mutex::new(HashMap::new()),
            secret_values: Mutex::new(HashMap::new()),
            deploy_errors: Mutex::new(BTreeMap::new()),
            environments: RwLock::new(BTreeMap::new()),
            started: Mutex::new(HashMap::new()),
            targets: RwLock::new(BTreeMap::new()),
            target_errors: Mutex::new(BTreeMap::new()),
            repo_names: RwLock::new(HashMap::new()),
            rollouts: rollouts::Drivers::default(),
            custody: custody::Custody::default(),
        });
        Ok((cd, t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An app's status is the JSON it was before it was a type (`wire::events::CdStatus`).
    #[test]
    fn summary_json_is_what_it_was() -> anyhow::Result<()> {
        use kiln_cd::engine::{OpPhase, OpStatus, Trigger};
        let mut s = Summary {
            app: "default/web".into(),
            sync: kiln_cd::model::Sync::OutOfSync,
            health: kiln_cd::model::Health::Progressing,
            revision: None,
            resources: 4,
            out_of_sync: 2,
            shared: 0,
            missing: 1,
            prune: 1,
            unmarked: 0,
            operation: None,
            message: None,
        };
        anyhow::ensure!(summary_json(&s) == summary_json_was(&s), "{}", summary_json(&s));
        s.revision = Some("abc".into());
        s.message = Some("rendering".into());
        s.operation = Some(OpStatus { id: 3, phase: OpPhase::Running, trigger: Trigger::SelfHeal, attempt: 1, message: "applying".into(), by: Some("user:ada".into()), started_ms: 10, finished_ms: None });
        anyhow::ensure!(summary_json(&s) == summary_json_was(&s), "{}", summary_json(&s));
        Ok(())
    }

    /// An Application `kiln-root` renders (app of apps).
    fn child(name: &str) -> Result<AppDef, String> {
        let app = json!({"apiVersion": "argoproj.io/v1alpha1", "kind": "Application", "metadata": {"name": name, "namespace": "argocd"},
            "spec": {"project": "default", "source": {"repoURL": "http://git.example/kiln.git", "path": name, "targetRevision": "main"},
                "destination": {"server": "https://kubernetes.default.svc", "namespace": name}, "syncPolicy": {"automated": {"prune": true, "selfHeal": true}}}});
        apps::from_argo(&app, Some("default/kiln-root"), &|_| None).map_err(|e| e.0)
    }

    fn ids(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_child_taken_over_stays_kilns_whatever_its_parent_renders() -> Result<(), String> {
        let rendered = vec![child("kiln")?, child("kiln-namespaces")?];
        assert!(rendered.iter().all(is_shadow), "rendered by a parent from Argo CD: shadowed");
        // default/kiln was taken over: kept in kiln, not the parent's to register again.
        let kept = ids(&["default/kiln"]);
        let registered: Vec<String> = rendered_children(rendered, &kept).iter().map(AppDef::id).collect();
        assert_eq!(registered, ["default/kiln-namespaces"]);
        // Nor to forget (and with it, maybe delete) once the parent stops counting it.
        let before = ids(&["default/kiln", "default/kiln-namespaces"]);
        assert!(forgotten(&before, &ids(&["default/kiln-namespaces"]), &kept).is_empty());
        assert_eq!(forgotten(&before, &BTreeSet::new(), &kept), ["default/kiln-namespaces"], "one it no longer renders is forgotten");
        Ok(())
    }

    /// A revert to content deployed before: its key's resolution is the revert's commit, which a deploy
    /// of the key records (and DORA reads), not the commit whose content it restored.
    #[test]
    fn a_key_resolved_again_is_its_newest_commit() {
        let at = |key: u64, sha: &str| Resolved { revision: sha.into(), key, commits: vec![Some(sha.into())], ..Resolved::default() };
        let mut list = VecDeque::new();
        for r in [at(1, "c1"), at(2, "c2"), at(1, "c3")] {
            keep_resolved(&mut list, &r);
        }
        let kept: Vec<(u64, &str)> = list.iter().map(|r| (r.key, r.revision.as_str())).collect();
        assert_eq!(kept, [(2, "c2"), (1, "c3")]);
        assert_eq!(list.iter().find(|r| r.key == 1).map(|r| r.commits.clone()), Some(vec![Some("c3".to_owned())]));
        for n in 3..20 {
            keep_resolved(&mut list, &at(n, "cn"));
        }
        assert_eq!(list.len(), 8, "eight at the most, the oldest let go");
        assert_eq!(list.front().map(|r| r.key), Some(12));
    }

    #[test]
    fn a_head_not_built_yet_gives_way_to_the_newest_commit_since_what_runs() {
        let stack: Vec<String> = ["c3", "c4", "c5", "head"].iter().map(|c| (*c).to_owned()).collect();
        assert_eq!(in_place_of_head(stack, "c2"), ["c5", "c4", "c3", "c2"], "newest first, down to what runs and never past it");
        assert_eq!(in_place_of_head(vec!["head".to_owned()], "c2"), ["c2"], "nothing between: what runs stays");
    }

    #[test]
    fn taking_over_ends_the_shadow() -> Result<(), String> {
        let mut d = child("kiln")?;
        assert!(is_shadow(&d));
        d.labels.insert("kiln.dev/take-over".into(), "true".into());
        assert!(!is_shadow(&d));
        Ok(())
    }

    /// An input of a small app's word for the inbox's tests: `n` as its content.
    fn remember(n: u64) -> ToEngine {
        ToEngine::Input(Box::new(Input::Remember { app: format!("default/app-{n:03}").as_str().into(), content: n, succeeded: true }))
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: what the engine is told and what it asks for weigh the
    /// objects they carry, as serde_json keeps them: a kind's list of a hundred 10 KB objects weighs
    /// at least a megabyte more than an empty one, an apply its object's text and more.
    #[test]
    fn g11_s3_the_engines_inputs_and_actions_weigh_their_objects() {
        use crate::memory::queue::Weigh as _;
        let listed = |n: usize| {
            let objects = (0..n).map(|i| json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": format!("cm-{i}")}, "data": {"k": "v".repeat(10_000)}})).collect();
            ToEngine::Input(Box::new(Input::Live { cluster: "in-cluster".into(), change: LiveChange::Listed { kind: GroupKind::new("", "ConfigMap"), objects } }))
        };
        let (empty, full) = (counted(&listed(0)), counted(&listed(100)));
        assert!(full >= empty + 100 * 10_000, "a list of 100 objects of 10 KB: {full} against {empty}");
        let apply = |object: Value| Action::Apply { id: 1, cluster: "in-cluster".into(), object, server_side: true, force: false, dry_run: false, replace: false, manager: "kiln".into() };
        let (bare, big) = (apply(json!({})).weight(), apply(json!({"data": "x".repeat(100_000)})).weight());
        assert!(big >= bare + 100_000, "an apply of 100 KB: {big} against {bare}");
        assert!(counted(&remember(1)) >= size_of::<ToEngine>() + size_of::<Input>() + crate::memory::queue::PLACE, "an input counts its place and its box");
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a sender to the engine waits for room past the inbox's bound
    /// (a task's wait) and goes on as the engine takes; every input arrives once, in order, and the
    /// inbox never holds more than its bound.
    #[test]
    fn g11_s3_a_sender_to_the_engine_waits_at_its_bound_and_nothing_is_lost() -> anyhow::Result<()> {
        let bound = 4 * counted(&remember(100));
        let (tx, rx) = inbox(bound);
        let room = Arc::clone(&tx.room);
        anyhow::ensure!(crate::memory::owners::CD_INPUTS.bound().is_some_and(|b| b >= bound), "the inbox's bound on /metrics: {:?}", crate::memory::owners::CD_INPUTS.bound());
        // The engine's thread, slower than its sender.
        let engine = std::thread::spawn(move || {
            let (mut got, mut most) = (Vec::new(), 0);
            while got.len() < 200 {
                most = most.max(rx.room.held());
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(ToEngine::Input(i)) => {
                        if let Input::Remember { content, .. } = *i {
                            got.push(content);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            (got, most)
        });
        let sent = futures::executor::block_on(async {
            for n in 1..=200 {
                if !tx.send(remember(n)).await {
                    return false;
                }
            }
            true
        });
        let (got, most) = engine.join().map_err(|_| anyhow::anyhow!("the engine's thread panicked"))?;
        anyhow::ensure!(sent, "the inbox refused a send");
        anyhow::ensure!(got == (1..=200).collect::<Vec<u64>>(), "inputs lost or out of order: {} arrived", got.len());
        anyhow::ensure!(most <= bound, "the inbox held {most} bytes, past its bound of {bound}");
        anyhow::ensure!(room.held() == 0, "{} bytes still counted", room.held());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: room is waited for before the inputs gate and taken once:
    /// sent under it without waiting, or given back when it isn't (a pin moved meanwhile), never sent.
    #[test]
    fn g11_s3_room_reserved_before_the_gate_is_sent_or_given_back() -> anyhow::Result<()> {
        let one = counted(&remember(1));
        let (tx, rx) = inbox(2 * one);
        let tx = crate::rt::block_on(async move {
            let (a, b) = (tx.reserve(remember(1)).await, tx.reserve(remember(2)).await);
            anyhow::ensure!(a.is_some() && b.is_some() && tx.room.held() == 2 * one, "both reserved: {} held", tx.room.held());
            // Full now: a third waits for room.
            let waited = crate::rt::timeout(Duration::from_millis(50), tx.reserve(remember(3))).await.is_err();
            anyhow::ensure!(waited, "a reservation past the bound waits");
            drop(b);
            anyhow::ensure!(tx.room.held() == one, "one given back unsent: {} held", tx.room.held());
            anyhow::ensure!(a.is_some_and(Reserved::send), "the other sent, without waiting");
            Ok(tx)
        })?;
        let ToEngine::Input(i) = rx.try_recv().map_err(|e| anyhow::anyhow!("nothing arrived: {e}"))? else { anyhow::bail!("not an input") };
        anyhow::ensure!(matches!(*i, Input::Remember { content: 1, .. }), "the one sent arrives");
        anyhow::ensure!(rx.try_recv().is_err(), "the one given back never does");
        anyhow::ensure!(tx.room.held() == 0, "{} bytes still counted", tx.room.held());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a sender waiting for room stops once the engine is gone, its
    /// input refused, and what waited in the inbox goes with the engine, its bytes with it.
    #[test]
    fn g11_s3_a_sender_waiting_for_the_engine_stops_when_it_goes() -> anyhow::Result<()> {
        let (tx, rx) = inbox(counted(&remember(1)));
        anyhow::ensure!(futures::executor::block_on(tx.send(remember(1))), "the first fits");
        let room = Arc::clone(&tx.room);
        let waiting = std::thread::spawn(move || futures::executor::block_on(tx.send(remember(2))));
        std::thread::sleep(Duration::from_millis(50));
        drop(rx);
        let sent = waiting.join().map_err(|_| anyhow::anyhow!("the sender panicked"))?;
        anyhow::ensure!(!sent, "the waiting send ended, refused");
        anyhow::ensure!(room.held() == 0, "{} bytes still counted after the engine went", room.held());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: what the engine asks for waits in a queue bounded in bytes:
    /// the engine's thread waits at the bound (its own thread's wait) and every action reaches the
    /// dispatcher once, in order.
    #[test]
    fn g11_s3_the_engine_waits_at_its_actions_bound_and_nothing_is_lost() -> anyhow::Result<()> {
        use crate::memory::queue::Weigh as _;
        let apply = |id: u64| Action::Apply { id, cluster: "in-cluster".into(), object: json!({"kind": "ConfigMap", "data": {"k": "v".repeat(1000)}}), server_side: true, force: false, dry_run: false, replace: false, manager: "kiln".into() };
        let bound = 3 * (apply(0).weight() + crate::memory::queue::PLACE);
        let (tx, rx) = crate::memory::queue::bounded::<Action>(&crate::memory::owners::CD_ACTIONS, bound);
        let engine = std::thread::spawn(move || (1..=100).all(|id| tx.send_blocking(apply(id)).is_ok()));
        let (mut got, mut most) = (Vec::new(), 0);
        while got.len() < 100 {
            std::thread::sleep(Duration::from_micros(200));
            most = most.max(rx.held());
            match futures::executor::block_on(rx.recv()) {
                Ok(Action::Apply { id, .. }) => got.push(id),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        anyhow::ensure!(engine.join().unwrap_or(false), "the engine's sends were refused");
        anyhow::ensure!(got == (1..=100).collect::<Vec<u64>>(), "actions lost or out of order: {} arrived", got.len());
        anyhow::ensure!(most <= bound, "the queue held {most} bytes, past its bound of {bound}");
        Ok(())
    }
}
