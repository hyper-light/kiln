//! The controller, as a state machine with no clock, no disk and no network.
//!
//! The owner tells the engine what happened ([`Input`]), with the time it happened, and takes
//! what the engine wants done ([`Action`]): render these inputs, watch that kind, apply this
//! object, delete that one, record this deployment, publish this status. The engine never
//! waits; everything it waits for is a later input.
//!
//! **Incremental by construction.** An app's status is kept as counts over its resources
//! (how many differ, how many of each health), updated as single resources change, so a watch
//! event costs a hash comparison and, only when the hash moved, one object comparison. A render
//! is requested only when an app's render key moves: the owner derives keys from content (git
//! tree hashes and parameters), so unrelated commits cost nothing, and apps with the same key
//! share one render. Rendered objects are kept once, compressed, without the namespace and
//! tracking metadata each app adds, so a chart deployed to a thousand clusters is held once.
//!
//! **Syncs** follow Argo CD's rules ([`crate::sync`]): phases, then waves; a multi-step sync waits
//! for each wave's objects to be healthy before the next; the final wave succeeds once applied;
//! hooks run to completion and are deleted as their policy says; a failure runs `SyncFail` hooks
//! and retries with backoff. Requests to a cluster go through its budget (a token bucket and a
//! limit in flight), highest priority first, so a fleet-wide rollout cannot starve a person's sync.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Map, Value};

use crate::diff::{self, Ignores, LiveView, Verdict};
use crate::health::{self, Rules};
use crate::model::{AppConfig, Health, Propagation, Retry, Sync, SyncOptions, Tracking};
use crate::obj::{self, Blob, GroupKind, Interner, ResKey, Scopes, Str};
use crate::sync::{self, Phase, TaskState};

/// The label kiln's watches select on: a hash of the app's id.
pub const APP_LABEL: &str = "kiln.dev/app";
/// The annotation naming the app and key an object was applied for.
pub const TRACKING: &str = "kiln.dev/tracking";
/// Argo CD's tracking annotation.
pub const ARGO_TRACKING: &str = "argocd.argoproj.io/tracking-id";
/// Argo CD's tracking label.
pub const ARGO_INSTANCE: &str = "app.kubernetes.io/instance";

/// The value of [`APP_LABEL`] for an app: 16 hex digits of a hash of its id.
pub fn app_label(id: &str) -> String {
    format!("{:016x}", obj::hash_bytes(id.as_bytes()))
}

/// Engine settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Wait between the waves of a multi-step sync (Argo CD's `ARGOCD_SYNC_WAVE_DELAY`, 2 s).
    pub wave_delay_ms: u64,
    /// Dry-run every apply of a sync before the first real one.
    pub dry_run_first: bool,
    /// First self-heal backoff (Argo CD: 2 s), multiplied by `heal_factor` per heal, capped.
    pub heal_backoff_ms: u64,
    /// Self-heal backoff factor (Argo CD: 3).
    pub heal_factor: u64,
    /// Self-heal backoff cap (Argo CD: 5 min).
    pub heal_cap_ms: u64,
    /// Per-cluster request rate (requests a second) and burst, unless the cluster says.
    pub cluster_qps: f64,
    /// Burst size of the per-cluster token bucket.
    pub cluster_burst: f64,
    /// Requests in flight per cluster.
    pub cluster_inflight: u32,
    /// Use kiln's generic `Ready`-condition rule for kinds with no other rule.
    pub generic_health: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wave_delay_ms: 2_000,
            dry_run_first: true,
            heal_backoff_ms: 2_000,
            heal_factor: 3,
            heal_cap_ms: 300_000,
            cluster_qps: 50.0,
            cluster_burst: 100.0,
            cluster_inflight: 64,
            generic_health: false,
        }
    }
}

/// Why a sync started; also its priority (lower first).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Trigger {
    /// A person (or an agent acting for one).
    Manual = 0,
    /// A rollback.
    Rollback = 1,
    /// A promotion between environments.
    Promotion = 2,
    /// Git moved and the app syncs automatically.
    Auto = 3,
    /// The cluster drifted and the app heals itself.
    SelfHeal = 4,
}

const PRIORITIES: usize = 5;

impl Trigger {
    /// A short name.
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Manual => "manual",
            Trigger::Rollback => "rollback",
            Trigger::Promotion => "promotion",
            Trigger::Auto => "auto",
            Trigger::SelfHeal => "self-heal",
        }
    }
}

/// A request for a sync.
#[derive(Clone, Debug, Default)]
pub struct SyncRequest {
    /// Prune (defaults to the app's policy for automatic syncs, `false` for manual ones).
    pub prune: Option<bool>,
    /// Only dry-run.
    pub dry_run: bool,
    /// Only these objects.
    pub resources: Option<Vec<ResKey>>,
    /// Options for this sync, over the app's.
    pub options: SyncOptions,
    /// Who asked, for the record.
    pub by: Option<String>,
    /// Why, for priority.
    pub trigger: Option<Trigger>,
}

/// Something that happened in a cluster.
#[derive(Clone, Debug)]
pub enum LiveChange {
    /// An object was added or changed (a watch event, or an apply's answer).
    Upsert(Value),
    /// An object was deleted.
    Delete(ResKey),
    /// A full list of one kind: every object of the kind not in it is gone.
    Listed {
        /// The kind.
        kind: GroupKind,
        /// Every object of it.
        objects: Vec<Value>,
    },
}

/// What happened, as the owner tells it.
#[derive(Clone, Debug)]
pub enum Input {
    /// Time passed; the engine checks its timers.
    Tick,
    /// An app was created or changed.
    UpsertApp(AppConfig),
    /// An app was deleted; with `cascade`, its objects are deleted too.
    RemoveApp {
        /// The app.
        id: Str,
        /// Delete its objects, propagating like this.
        cascade: Option<Propagation>,
    },
    /// A cluster is known, with its request budget.
    UpsertCluster {
        /// Its name.
        name: Str,
        /// Requests a second, if not the default.
        qps: Option<f64>,
        /// Requests in flight, if not the default.
        inflight: Option<u32>,
    },
    /// A cluster became reachable or unreachable.
    ClusterReachable {
        /// The cluster.
        name: Str,
        /// Whether it is reachable.
        reachable: bool,
    },
    /// Discovery said whether a kind is namespaced.
    Scope {
        /// The cluster.
        cluster: Str,
        /// The kind.
        kind: GroupKind,
        /// Whether it is namespaced.
        namespaced: bool,
    },
    /// An app's render inputs are now `key`, at `revision`.
    Inputs {
        /// The app.
        app: Str,
        /// The revision (commit) the inputs are from.
        revision: Str,
        /// The content address of the inputs.
        key: u64,
    },
    /// A render finished.
    Rendered {
        /// The render key.
        key: u64,
        /// The objects, or why it failed.
        result: Result<Vec<Value>, String>,
    },
    /// Something changed in a cluster.
    Live {
        /// The cluster.
        cluster: Str,
        /// What changed.
        change: LiveChange,
    },
    /// A request answered: the object as applied (or nothing, for deletes), or an error.
    Answer {
        /// The request.
        id: u64,
        /// What the API server said.
        result: Result<Option<Value>, String>,
    },
    /// Compare again; `hard` renders again too.
    Refresh {
        /// The app.
        app: Str,
        /// Render again even if the inputs didn't move.
        hard: bool,
    },
    /// Start a sync.
    Sync {
        /// The app.
        app: Str,
        /// How.
        request: SyncRequest,
    },
    /// Stop the running sync.
    Terminate {
        /// The app.
        app: Str,
    },
}

/// What the engine wants done.
#[derive(Clone, Debug)]
pub enum Action {
    /// Render an app's inputs; answer with [`Input::Rendered`] for `key`.
    Render {
        /// The render key.
        key: u64,
        /// An app with these inputs (for its sources and parameters).
        app: Str,
        /// The revision.
        revision: Str,
    },
    /// Watch a kind (and list it first); feed [`Input::Live`].
    Watch {
        /// The cluster.
        cluster: Str,
        /// The kind.
        kind: GroupKind,
    },
    /// Apply an object; answer with [`Input::Answer`].
    Apply {
        /// The request.
        id: u64,
        /// The cluster.
        cluster: Str,
        /// The object to apply.
        object: Value,
        /// Server-side apply (else client-side, with `last-applied-configuration`).
        server_side: bool,
        /// Force conflicts (SSA) or replace (CSA).
        force: bool,
        /// Only validate.
        dry_run: bool,
        /// Replace instead of apply.
        replace: bool,
        /// The field manager.
        manager: Str,
    },
    /// Delete an object; answer with [`Input::Answer`].
    Delete {
        /// The request.
        id: u64,
        /// The cluster.
        cluster: Str,
        /// The object.
        key: ResKey,
        /// How deletes cascade.
        propagation: Propagation,
    },
    /// An app's status changed.
    Status(Summary),
    /// Something to keep: a sync started or finished.
    Record(Record),
}

/// An app's status, as published.
#[derive(Clone, PartialEq, Debug)]
pub struct Summary {
    /// The app.
    pub app: Str,
    /// Sync state.
    pub sync: Sync,
    /// Health.
    pub health: Health,
    /// The revision its inputs are at.
    pub revision: Option<Str>,
    /// Resources in git.
    pub resources: u32,
    /// Resources that differ (including missing and to-prune).
    pub out_of_sync: u32,
    /// The running (or last) sync's state.
    pub operation: Option<OpStatus>,
    /// Why the status is what it is, when that isn't obvious (a render error, an unreachable
    /// cluster).
    pub message: Option<String>,
}

/// Where a sync stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpPhase {
    /// Validating every apply first.
    DryRun,
    /// Applying.
    Running,
    /// Running `SyncFail` hooks after a failure.
    Failing,
    /// Stopping on request.
    Terminating,
    /// Finished: everything applied.
    Succeeded,
    /// Finished: something failed.
    Failed,
}

/// A sync's status.
#[derive(Clone, PartialEq, Debug)]
pub struct OpStatus {
    /// The sync.
    pub id: u64,
    /// Its phase.
    pub phase: OpPhase,
    /// Why it started.
    pub trigger: Trigger,
    /// Its attempt (1-based).
    pub attempt: u32,
    /// What it says now.
    pub message: String,
}

/// A durable event.
#[derive(Clone, Debug)]
pub enum Record {
    /// A sync started.
    Started {
        /// The app.
        app: Str,
        /// The sync.
        op: u64,
        /// The revision synced.
        revision: Option<Str>,
        /// Why.
        trigger: Trigger,
        /// Who asked.
        by: Option<String>,
        /// When.
        at_ms: u64,
    },
    /// A sync finished.
    Finished {
        /// The app.
        app: Str,
        /// The sync.
        op: u64,
        /// The revision synced.
        revision: Option<Str>,
        /// The render key synced (enough to roll back to it).
        key: Option<u64>,
        /// Whether it succeeded.
        succeeded: bool,
        /// Its last message.
        message: String,
        /// Objects applied, pruned and run as hooks.
        applied: u32,
        /// Objects pruned.
        pruned: u32,
        /// When it finished.
        at_ms: u64,
    },
}

type AppIx = u32;
type ResIx = u32;
type ClusterIx = u32;

/// One rendered object, normalized and kept compressed. The namespace and tracking metadata of
/// each app are added when it is compared or applied, so apps with the same render share it.
struct Rendered {
    blob: Blob,
    hash: u64,
    group: Str,
    kind: Str,
    name: Str,
    namespace: Str,
    hook: bool,
}

/// A render: what one set of inputs produced.
struct Render {
    key: u64,
    objects: Vec<Rendered>,
}

enum RenderSlot {
    Pending(Vec<AppIx>),
    Done(Arc<Render>),
}

/// One object in one cluster, as far as some app is concerned.
struct Res {
    key: ResKey,
    /// The app it belongs to (desired by it, or tracked as applied by it).
    app: Option<AppIx>,
    /// Its desired form: a render and the object's index in it.
    desired: Option<(Arc<Render>, u32)>,
    live: Option<Live>,
    /// The last comparison.
    state: ResState,
    /// What it last added to its app's counts: (differs, health, desired).
    counted: Option<(bool, Option<Health>, bool)>,
}

struct Live {
    blob: Blob,
    hash: u64,
    health: Option<Health>,
    message: Option<Box<str>>,
    hook: bool,
    terminating: bool,
}

/// What a resource counts as in its app's status.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ResState {
    /// Not counted (a hook, or nothing known).
    #[default]
    None,
    /// Matches.
    Synced,
    /// Differs.
    OutOfSync,
    /// Desired, not live.
    Missing,
    /// Live, tracked, not desired: should be pruned.
    Prune,
}

/// A resource's view for queries.
#[derive(Clone, Debug)]
pub struct ResourceView {
    /// Its key.
    pub key: ResKey,
    /// Sync state.
    pub sync: Sync,
    /// Whether it is missing from the cluster.
    pub missing: bool,
    /// Whether it should be pruned.
    pub requires_pruning: bool,
    /// Health, if it has one.
    pub health: Option<Health>,
    /// Health message.
    pub message: Option<String>,
    /// Whether it is a hook.
    pub hook: bool,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    resources: u32,
    out_of_sync: u32,
    health: [u32; 6],
}

struct Op {
    id: u64,
    trigger: Trigger,
    by: Option<String>,
    revision: Option<Str>,
    render: Option<Arc<Render>>,
    tasks: Vec<sync::Task>,
    res: Vec<Option<ResIx>>,
    states: Vec<TaskState>,
    on_fail: Vec<sync::Task>,
    on_fail_states: Vec<TaskState>,
    dry_run_only: bool,
    options: SyncOptions,
    phase: OpPhase,
    attempt: u32,
    message: String,
    wave_ready_ms: u64,
    multi_step: bool,
    pending_dry_runs: u32,
    applied: u32,
    pruned: u32,
    request: SyncRequest,
    /// Requests this sync made (sent or not).
    requests: Vec<u64>,
}

#[derive(Default)]
struct Auto {
    /// The render key of the last automatic sync, and whether it succeeded.
    attempted: Option<(u64, bool)>,
    heal_backoff_ms: u64,
    heal_after_ms: u64,
    /// The heal timer already set, so re-evaluations don't stack timers.
    heal_timer_ms: u64,
}

struct App {
    cfg: Arc<AppConfig>,
    label: Str,
    cluster: ClusterIx,
    /// The revision of the render it is compared against.
    revision: Option<Str>,
    /// The revision of the inputs it was last given (rendering, if `key` moved).
    wanted_revision: Option<Str>,
    key: Option<u64>,
    render: Option<Arc<Render>>,
    render_error: Option<String>,
    /// Resources whose desired form or tracking is this app's.
    resources: FxHashSet<ResIx>,
    hooks: Vec<u32>,
    counts: Counts,
    published: Option<Summary>,
    op: Option<Op>,
    last_op: Option<OpStatus>,
    auto: Auto,
    /// A failed sync waiting to run again.
    retry: Option<PendingRetry>,
    dirty: bool,
}

struct PendingRetry {
    request: SyncRequest,
    trigger: Trigger,
    attempt: u32,
    at_ms: u64,
}

struct Bucket {
    tokens: f64,
    rate_per_ms: f64,
    burst: f64,
    at_ms: u64,
}

impl Bucket {
    fn new(qps: f64, burst: f64, now: u64) -> Self {
        Self { tokens: burst, rate_per_ms: qps / 1000.0, burst, at_ms: now }
    }

    fn refill(&mut self, now: u64) {
        let dt = now.saturating_sub(self.at_ms) as f64;
        self.tokens = (self.tokens + dt * self.rate_per_ms).min(self.burst);
        self.at_ms = now;
    }

    /// When the next token is due, if none is available.
    fn next_ms(&self) -> u64 {
        if self.tokens >= 1.0 || self.rate_per_ms <= 0.0 {
            return self.at_ms;
        }
        self.at_ms + ((1.0 - self.tokens) / self.rate_per_ms).ceil() as u64
    }
}

struct Cluster {
    name: Str,
    scopes: Scopes,
    index: FxHashMap<ResKey, ResIx>,
    res: Vec<Option<Res>>,
    free: Vec<ResIx>,
    watched: FxHashSet<GroupKind>,
    reachable: bool,
    bucket: Bucket,
    inflight: u32,
    max_inflight: u32,
    queue: [VecDeque<u64>; PRIORITIES],
    /// apps by their tracking label, for live objects to find their app.
    by_label: FxHashMap<Str, AppIx>,
    /// When the pending budget timer fires, if one is set: one at a time per cluster.
    budget_timer: Option<u64>,
}

enum ReqKind {
    Apply { task: u32, fail_phase: bool, dry_run: bool },
    Delete { task: u32, fail_phase: bool },
    /// A cascade delete of a removed app's object, or a hook's cleanup.
    Cleanup,
}

struct Request {
    app: Option<AppIx>,
    op: u64,
    cluster: ClusterIx,
    kind: ReqKind,
    action: Option<Action>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Timer {
    Wave(AppIx, u64),
    Retry(AppIx),
    Heal(AppIx),
    Budget(ClusterIx),
}

/// What the engine's memory holds (estimates, in bytes).
#[derive(Clone, Copy, Debug, Default)]
pub struct Footprint {
    /// Apps.
    pub apps: usize,
    /// Resources tracked.
    pub resources: usize,
    /// Renders cached.
    pub renders: usize,
    /// App records and their resource sets.
    pub app_bytes: usize,
    /// Resource records.
    pub resource_bytes: usize,
    /// The key index.
    pub index_bytes: usize,
    /// Object names.
    pub name_bytes: usize,
    /// Live objects, compressed.
    pub live_bytes: usize,
    /// Rendered objects, compressed.
    pub desired_bytes: usize,
}

impl Footprint {
    /// Everything.
    pub fn total(&self) -> usize {
        self.app_bytes + self.resource_bytes + self.index_bytes + self.name_bytes + self.live_bytes + self.desired_bytes
    }
}

/// Counters for benchmarks and `/metrics`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Live events taken.
    pub live_events: u64,
    /// Live events whose hash had not moved (no comparison needed).
    pub live_unchanged: u64,
    /// Object comparisons run.
    pub comparisons: u64,
    /// Renders requested.
    pub renders: u64,
    /// Render requests served from the cache or joined to one in flight.
    pub render_hits: u64,
    /// Applies sent.
    pub applies: u64,
    /// Deletes sent.
    pub deletes: u64,
    /// Syncs started.
    pub syncs: u64,
    /// Syncs succeeded.
    pub syncs_ok: u64,
    /// Syncs failed.
    pub syncs_failed: u64,
    /// Status updates published.
    pub statuses: u64,
}

/// The controller.
pub struct Engine {
    cfg: Config,
    now: u64,
    interner: Interner,
    apps: Vec<Option<App>>,
    free_apps: Vec<AppIx>,
    app_by_id: FxHashMap<Str, AppIx>,
    /// Apps by the name their objects' tracking metadata carries.
    app_by_tracking: FxHashMap<Str, AppIx>,
    clusters: Vec<Cluster>,
    cluster_by_name: FxHashMap<Str, ClusterIx>,
    renders: FxHashMap<u64, RenderSlot>,
    requests: FxHashMap<u64, Request>,
    next_id: u64,
    timers: BinaryHeap<Reverse<(u64, Timer)>>,
    out: VecDeque<Action>,
    dirty: Vec<AppIx>,
    rules: Box<dyn Rules + Send>,
    stats: Stats,
}

impl Engine {
    /// An engine with no apps and no clusters.
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            now: 0,
            interner: Interner::default(),
            apps: Vec::new(),
            free_apps: Vec::new(),
            app_by_id: FxHashMap::default(),
            app_by_tracking: FxHashMap::default(),
            clusters: Vec::new(),
            cluster_by_name: FxHashMap::default(),
            renders: FxHashMap::default(),
            requests: FxHashMap::default(),
            next_id: 1,
            timers: BinaryHeap::new(),
            out: VecDeque::new(),
            dirty: Vec::new(),
            rules: Box::new(health::BuiltIn),
            stats: Stats::default(),
        }
    }

    /// Replaces the health rules (settings, Lua scripts).
    pub fn set_rules(&mut self, rules: Box<dyn Rules + Send>) {
        self.rules = rules;
    }

    /// The counters.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// An estimate of the memory the engine holds, by what holds it.
    pub fn footprint(&self) -> Footprint {
        let mut f = Footprint { apps: self.app_by_id.len(), ..Footprint::default() };
        for cl in &self.clusters {
            f.resources += cl.index.len();
            f.index_bytes += cl.index.capacity() * (std::mem::size_of::<ResKey>() + std::mem::size_of::<ResIx>() + 8);
            f.resource_bytes += cl.res.capacity() * std::mem::size_of::<Option<Res>>();
            for res in cl.res.iter().flatten() {
                f.name_bytes += res.key.name.len();
                if let Some(l) = &res.live {
                    f.live_bytes += l.blob.len();
                }
            }
        }
        for slot in self.renders.values() {
            if let RenderSlot::Done(r) = slot {
                f.renders += 1;
                f.desired_bytes += r.objects.iter().map(|o| o.blob.len() + o.name.len() + std::mem::size_of::<Rendered>()).sum::<usize>();
            }
        }
        f.app_bytes = self.apps.capacity() * std::mem::size_of::<Option<App>>()
            + self.apps.iter().flatten().map(|a| a.resources.capacity() * 8 + a.cfg.id.len() * 2).sum::<usize>();
        f
    }

    /// The time of the last input.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// The next time the engine wants a [`Input::Tick`], if any.
    pub fn deadline(&self) -> Option<u64> {
        self.timers.peek().map(|Reverse((t, _))| *t)
    }

    /// The next action, if any.
    pub fn poll(&mut self) -> Option<Action> {
        self.out.pop_front()
    }

    /// Moves every pending action into `out`.
    pub fn drain(&mut self, out: &mut Vec<Action>) {
        out.extend(self.out.drain(..));
    }

    /// How many apps there are.
    pub fn app_count(&self) -> usize {
        self.app_by_id.len()
    }

    /// Takes one input at `now_ms` (milliseconds on the owner's clock; never decreasing).
    pub fn handle(&mut self, now_ms: u64, input: Input) {
        self.now = self.now.max(now_ms);
        match input {
            Input::Tick => {}
            Input::UpsertApp(cfg) => self.upsert_app(cfg),
            Input::RemoveApp { id, cascade } => self.remove_app(&id, cascade),
            Input::UpsertCluster { name, qps, inflight } => {
                let c = self.cluster_ix(&name);
                if let Some(cl) = self.clusters.get_mut(c as usize) {
                    if let Some(qps) = qps {
                        cl.bucket = Bucket::new(qps, qps.max(1.0) * 2.0, self.now);
                    }
                    if let Some(n) = inflight {
                        cl.max_inflight = n.max(1);
                    }
                }
            }
            Input::ClusterReachable { name, reachable } => {
                let c = self.cluster_ix(&name);
                if let Some(cl) = self.clusters.get_mut(c as usize) {
                    cl.reachable = reachable;
                }
                let apps: Vec<AppIx> = self.apps_in(c);
                for a in apps {
                    self.mark(a);
                }
                self.pump(c);
            }
            Input::Scope { cluster, kind, namespaced } => {
                let c = self.cluster_ix(&cluster);
                if let Some(cl) = self.clusters.get_mut(c as usize) {
                    cl.scopes.set(kind, namespaced);
                }
            }
            Input::Inputs { app, revision, key } => self.set_inputs(&app, revision, key, false),
            Input::Rendered { key, result } => self.rendered(key, result),
            Input::Live { cluster, change } => {
                let c = self.cluster_ix(&cluster);
                self.live(c, change);
            }
            Input::Answer { id, result } => self.answer(id, result),
            Input::Refresh { app, hard } => {
                if let Some(&a) = self.app_by_id.get(&*app) {
                    if hard {
                        let (rev, key) = match self.app(a) {
                            Some(x) => (x.wanted_revision.clone(), x.key),
                            None => (None, None),
                        };
                        if let (Some(rev), Some(key)) = (rev, key) {
                            self.renders.remove(&key);
                            self.set_inputs(&app, rev, key, true);
                        }
                    } else {
                        self.recompare_app(a);
                    }
                }
            }
            Input::Sync { app, request } => {
                if let Some(&a) = self.app_by_id.get(&*app) {
                    let trigger = request.trigger.unwrap_or(Trigger::Manual);
                    self.start_op(a, trigger, request);
                }
            }
            Input::Terminate { app } => {
                if let Some(&a) = self.app_by_id.get(&*app) {
                    self.terminate(a);
                }
            }
        }
        self.fire_timers();
        self.settle();
    }

    // ---------------------------------------------------------------- apps

    fn app(&self, a: AppIx) -> Option<&App> {
        self.apps.get(a as usize)?.as_ref()
    }

    fn app_mut(&mut self, a: AppIx) -> Option<&mut App> {
        self.apps.get_mut(a as usize)?.as_mut()
    }

    fn cluster_ix(&mut self, name: &str) -> ClusterIx {
        if let Some(&c) = self.cluster_by_name.get(name) {
            return c;
        }
        let c = self.clusters.len() as ClusterIx;
        let name: Str = name.into();
        self.clusters.push(Cluster {
            name: name.clone(),
            scopes: Scopes::default(),
            index: FxHashMap::default(),
            res: Vec::new(),
            free: Vec::new(),
            watched: FxHashSet::default(),
            reachable: true,
            bucket: Bucket::new(self.cfg.cluster_qps, self.cfg.cluster_burst, self.now),
            inflight: 0,
            max_inflight: self.cfg.cluster_inflight,
            queue: Default::default(),
            by_label: FxHashMap::default(),
            budget_timer: None,
        });
        self.cluster_by_name.insert(name, c);
        c
    }

    fn apps_in(&self, c: ClusterIx) -> Vec<AppIx> {
        self.apps
            .iter()
            .enumerate()
            .filter_map(|(i, a)| a.as_ref().filter(|a| a.cluster == c).map(|_| i as AppIx))
            .collect()
    }

    fn mark(&mut self, a: AppIx) {
        if let Some(app) = self.app_mut(a)
            && !app.dirty {
                app.dirty = true;
                self.dirty.push(a);
            }
    }

    fn upsert_app(&mut self, cfg: AppConfig) {
        let c = self.cluster_ix(&cfg.cluster);
        if let Some(&a) = self.app_by_id.get(&*cfg.id) {
            let moved = self.app(a).is_some_and(|app| app.cluster != c);
            if moved {
                // A new destination is a new app: its objects in the old cluster are orphaned
                // (Argo CD does the same), and it starts from nothing in the new one.
                let id = cfg.id.clone();
                self.remove_app(&id, None);
            } else {
                let changed = self.app(a).is_some_and(|app| {
                    app.cfg.ignore != cfg.ignore || app.cfg.namespace != cfg.namespace || app.cfg.create_namespace != cfg.create_namespace
                });
                let old_name: Option<Str> = self.app(a).map(|app| app.cfg.tracked_as().into());
                if let Some(old) = old_name {
                    self.app_by_tracking.remove(&old);
                }
                self.app_by_tracking.insert(cfg.tracked_as().into(), a);
                if let Some(app) = self.app_mut(a) {
                    app.cfg = Arc::new(cfg);
                }
                if changed
                    && let Some(render) = self.app(a).and_then(|app| app.render.clone()) {
                        self.apply_render(a, render);
                    }
                self.mark(a);
                return;
            }
        }
        let label: Str = app_label(&cfg.id).into();
        let app = App {
            label: label.clone(),
            cluster: c,
            cfg: Arc::new(cfg),
            revision: None,
            wanted_revision: None,
            key: None,
            render: None,
            render_error: None,
            resources: FxHashSet::default(),
            hooks: Vec::new(),
            counts: Counts::default(),
            published: None,
            op: None,
            last_op: None,
            auto: Auto::default(),
            retry: None,
            dirty: false,
        };
        let id = app.cfg.id.clone();
        let a = match self.free_apps.pop() {
            Some(a) => {
                if let Some(slot) = self.apps.get_mut(a as usize) {
                    *slot = Some(app);
                }
                a
            }
            None => {
                self.apps.push(Some(app));
                (self.apps.len() - 1) as AppIx
            }
        };
        self.app_by_id.insert(id, a);
        if let Some(name) = self.app(a).map(|app| -> Str { app.cfg.tracked_as().into() }) {
            self.app_by_tracking.insert(name, a);
        }
        if let Some(cl) = self.clusters.get_mut(c as usize) {
            cl.by_label.insert(label, a);
        }
        self.mark(a);
    }

    fn remove_app(&mut self, id: &str, cascade: Option<Propagation>) {
        let Some(a) = self.app_by_id.remove(id) else { return };
        let Some(app) = self.apps.get_mut(a as usize).and_then(Option::take) else { return };
        self.app_by_tracking.remove(app.cfg.tracked_as());
        let c = app.cluster;
        if let Some(cl) = self.clusters.get_mut(c as usize) {
            cl.by_label.remove(&app.label);
        }
        for r in app.resources {
            let live_key = {
                let Some(res) = self.res_mut(c, r) else { continue };
                res.app = None;
                res.desired = None;
                res.state = ResState::None;
                res.counted = None;
                // `Delete=false` (or `confirm`, which waits for a person) keeps an object when its
                // app goes, as Argo CD keeps it.
                let kept = res.live.as_ref().and_then(|l| l.blob.decode().ok()).is_some_and(|v| {
                    let o = sync::options_of(&v);
                    o.delete == Some(false) || o.delete_confirm == Some(true)
                });
                res.live.as_ref().filter(|_| !kept).map(|_| res.key.clone())
            };
            if let (Some(p), Some(key)) = (cascade, live_key) {
                let id = self.next_request_id();
                let cluster = self.cluster_name(c);
                self.requests.insert(id, Request {
                    app: None,
                    op: 0,
                    cluster: c,
                    kind: ReqKind::Cleanup,
                    action: Some(Action::Delete { id, cluster, key, propagation: p }),
                });
                self.enqueue(c, id, Trigger::Manual);
            }
            self.gc_res(c, r);
        }
        self.free_apps.push(a);
        self.pump(c);
    }

    fn set_inputs(&mut self, id: &str, revision: Str, key: u64, force: bool) {
        let Some(&a) = self.app_by_id.get(id) else { return };
        let same = self.app(a).is_some_and(|app| app.key == Some(key));
        let rendered = self.app(a).is_some_and(|app| app.render.as_ref().is_some_and(|r| r.key == key));
        if let Some(app) = self.app_mut(a) {
            app.wanted_revision = Some(revision.clone());
            // Same content at a new revision: what was compared is what this revision holds.
            if same && rendered {
                app.revision = Some(revision.clone());
            }
        }
        self.mark(a);
        if same && !force {
            return;
        }
        if let Some(app) = self.app_mut(a) {
            app.key = Some(key);
        }
        self.stats.renders += 1;
        match self.renders.get_mut(&key) {
            Some(RenderSlot::Done(r)) => {
                let r = r.clone();
                self.stats.render_hits += 1;
                self.apply_render(a, r);
            }
            Some(RenderSlot::Pending(waiters)) => {
                self.stats.render_hits += 1;
                if !waiters.contains(&a) {
                    waiters.push(a);
                }
            }
            None => {
                self.renders.insert(key, RenderSlot::Pending(vec![a]));
                self.out.push_back(Action::Render { key, app: id.into(), revision });
            }
        }
    }

    fn rendered(&mut self, key: u64, result: Result<Vec<Value>, String>) {
        let waiters = match self.renders.remove(&key) {
            Some(RenderSlot::Pending(w)) => w,
            Some(done) => {
                self.renders.insert(key, done);
                return;
            }
            None => Vec::new(),
        };
        match result {
            Ok(objects) => {
                let render = Arc::new(self.build_render(key, objects));
                self.renders.insert(key, RenderSlot::Done(render.clone()));
                for a in waiters {
                    if self.app(a).is_some_and(|app| app.key == Some(key)) {
                        self.apply_render(a, render.clone());
                    }
                }
            }
            Err(e) => {
                for a in waiters {
                    if let Some(app) = self.app_mut(a).filter(|app| app.key == Some(key)) {
                        app.render_error = Some(e.clone());
                    }
                    self.mark(a);
                }
            }
        }
    }

    fn build_render(&mut self, key: u64, objects: Vec<Value>) -> Render {
        let mut out = Vec::with_capacity(objects.len());
        for mut o in objects {
            if !o.is_object() || sync::is_skipped(&o) {
                continue;
            }
            diff::normalize_desired(&mut o);
            let kind = obj::kind(&o);
            let name = obj::name(&o);
            if kind.is_empty() || name.is_empty() {
                continue;
            }
            out.push(Rendered {
                hash: obj::hash_value(&o),
                group: self.interner.intern(obj::group(&o)),
                kind: self.interner.intern(kind),
                name: name.into(),
                namespace: self.interner.intern(obj::namespace(&o)),
                hook: sync::hook_of(&o).is_some(),
                blob: Blob::encode(&o),
            });
        }
        Render { key, objects: out }
    }

    /// The Namespace an app creates (`CreateNamespace=true`), as a render of its own: never
    /// pruned nor deleted with the app, as Argo CD leaves namespaces it created.
    fn namespace_render(&self, cfg: &AppConfig, render: &Render) -> Option<Arc<Render>> {
        let (labels, annotations) = cfg.create_namespace.as_ref()?;
        if cfg.namespace.is_empty() || render.objects.iter().any(|o| o.kind.as_ref() == "Namespace" && o.group.is_empty() && *o.name == *cfg.namespace) {
            return None;
        }
        let mut ann: Map<String, Value> = annotations.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        ann.insert(sync::SYNC_OPTIONS.into(), Value::String("Prune=false,Delete=false".into()));
        ann.insert(sync::SYNC_WAVE.into(), Value::String(i32::MIN.to_string()));
        let lab: Map<String, Value> = labels.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
        let mut v = serde_json::json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": cfg.namespace.to_string(), "annotations": ann}});
        if !lab.is_empty()
            && let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut)
        {
            meta.insert("labels".into(), Value::Object(lab));
        }
        Some(Arc::new(Render {
            key: 0,
            objects: vec![Rendered {
                hash: obj::hash_value(&v),
                group: "".into(),
                kind: "Namespace".into(),
                name: cfg.namespace.clone(),
                namespace: "".into(),
                hook: false,
                blob: Blob::encode(&v),
            }],
        }))
    }

    fn apply_render(&mut self, a: AppIx, render: Arc<Render>) {
        let Some(app) = self.app(a) else { return };
        let ns_render = self.namespace_render(&app.cfg.clone(), &render);
        let Some(app) = self.app(a) else { return };
        let c = app.cluster;
        let default_ns = app.cfg.namespace.clone();
        let old: Vec<ResIx> = app.resources.iter().copied().collect();
        let mut keep: FxHashSet<ResIx> = FxHashSet::default();
        let mut hooks = Vec::new();
        let mut kinds: Vec<GroupKind> = Vec::new();
        for (i, r) in render.objects.iter().enumerate() {
            if r.hook {
                // Hooks are created by syncs, but their results arrive by watch like any object's.
                let gk = GroupKind { group: r.group.clone(), kind: r.kind.clone() };
                if !kinds.contains(&gk) {
                    kinds.push(gk);
                }
                hooks.push(i as u32);
                continue;
            }
            let Some(cl) = self.clusters.get(c as usize) else { return };
            let ns = if cl.scopes.namespaced(&r.group, &r.kind) {
                if r.namespace.is_empty() { default_ns.clone() } else { r.namespace.clone() }
            } else {
                self.interner.intern("")
            };
            let key = ResKey { group: r.group.clone(), kind: r.kind.clone(), namespace: ns, name: r.name.clone() };
            let gk = key.group_kind();
            if !kinds.contains(&gk) {
                kinds.push(gk);
            }
            let ix = self.res_for(c, key);
            let taken = self.res(c, ix).is_some_and(|res| res.app.is_some_and(|other| other != a) && res.desired.is_some());
            if taken {
                // Another app already deploys this object; the first one keeps it.
                continue;
            }
            keep.insert(ix);
            let changed = {
                let Some(res) = self.res_mut(c, ix) else { continue };
                let before = res.desired.as_ref().map(|(r, i)| r.objects.get(*i as usize).map(|o| o.hash));
                res.app = Some(a);
                res.desired = Some((render.clone(), i as u32));
                before != Some(Some(r.hash))
            };
            if changed {
                self.recompare(c, ix);
            } else {
                self.count(c, ix);
            }
        }
        if let Some(nr) = ns_render {
            let key = ResKey { group: self.interner.intern(""), kind: self.interner.intern("Namespace"), namespace: self.interner.intern(""), name: default_ns.clone() };
            let ix = self.res_for(c, key.clone());
            let free = self.res(c, ix).is_none_or(|res| res.app.is_none_or(|other| other == a));
            if free {
                keep.insert(ix);
                if let Some(res) = self.res_mut(c, ix) {
                    res.app = Some(a);
                    res.desired = Some((nr, 0));
                }
                self.recompare(c, ix);
                let gk = key.group_kind();
                if !kinds.contains(&gk) {
                    kinds.push(gk);
                }
            }
        }
        for ix in old {
            if !keep.contains(&ix) {
                if let Some(res) = self.res_mut(c, ix) {
                    res.desired = None;
                }
                self.recompare(c, ix);
                self.gc_res(c, ix);
            }
        }
        if let Some(app) = self.app_mut(a) {
            if app.key == Some(render.key) {
                app.revision = app.wanted_revision.clone();
            }
            app.render = Some(render);
            app.render_error = None;
            app.hooks = hooks;
            app.resources.extend(keep.iter().copied());
        }
        for gk in kinds {
            self.watch(c, gk);
        }
        self.mark(a);
    }

    fn watch(&mut self, c: ClusterIx, gk: GroupKind) {
        let Some(cl) = self.clusters.get_mut(c as usize) else { return };
        if cl.watched.insert(gk.clone()) {
            self.out.push_back(Action::Watch { cluster: cl.name.clone(), kind: gk });
        }
    }

    // ---------------------------------------------------------------- resources

    fn res(&self, c: ClusterIx, r: ResIx) -> Option<&Res> {
        self.clusters.get(c as usize)?.res.get(r as usize)?.as_ref()
    }

    fn res_mut(&mut self, c: ClusterIx, r: ResIx) -> Option<&mut Res> {
        self.clusters.get_mut(c as usize)?.res.get_mut(r as usize)?.as_mut()
    }

    fn res_for(&mut self, c: ClusterIx, key: ResKey) -> ResIx {
        let Some(cl) = self.clusters.get_mut(c as usize) else { return 0 };
        if let Some(&ix) = cl.index.get(&key) {
            return ix;
        }
        let res = Res { key: key.clone(), app: None, desired: None, live: None, state: ResState::None, counted: None };
        let ix = match cl.free.pop() {
            Some(ix) => {
                if let Some(slot) = cl.res.get_mut(ix as usize) {
                    *slot = Some(res);
                }
                ix
            }
            None => {
                cl.res.push(Some(res));
                (cl.res.len() - 1) as ResIx
            }
        };
        cl.index.insert(key, ix);
        ix
    }

    /// Frees a resource nobody desires or tracks any more.
    fn gc_res(&mut self, c: ClusterIx, r: ResIx) {
        let Some(cl) = self.clusters.get_mut(c as usize) else { return };
        let gone = cl.res.get(r as usize).and_then(Option::as_ref).is_some_and(|res| res.desired.is_none() && res.live.is_none());
        if gone
            && let Some(res) = cl.res.get_mut(r as usize).and_then(Option::take) {
                cl.index.remove(&res.key);
                cl.free.push(r);
                if let Some(a) = res.app
                    && let Some(app) = self.apps.get_mut(a as usize).and_then(Option::as_mut) {
                        app.resources.remove(&r);
                    }
            }
    }

    /// The object as an app applies it: the rendered form plus namespace and tracking.
    fn desired_object(&self, app: &App, res: &Res) -> Option<Value> {
        let (render, i) = res.desired.as_ref()?;
        let r = render.objects.get(*i as usize)?;
        let mut v = r.blob.decode().ok()?;
        self.decorate(app, &res.key, &mut v);
        Some(v)
    }

    fn decorate(&self, app: &App, key: &ResKey, v: &mut Value) {
        if let Some(meta) = obj::ensure_map(v, &["metadata"])
            && !key.namespace.is_empty() {
                meta.insert("namespace".into(), Value::String(key.namespace.to_string()));
            }
        match app.cfg.tracking {
            Tracking::Kiln => {
                if let Some(labels) = obj::ensure_map(v, &["metadata", "labels"]) {
                    labels.insert(APP_LABEL.into(), Value::String(app.label.to_string()));
                }
                if let Some(ann) = obj::ensure_map(v, &["metadata", "annotations"]) {
                    ann.insert(TRACKING.into(), Value::String(format!("{}:{}", app.cfg.tracked_as(), key.tracking())));
                }
            }
            Tracking::ArgoLabel => {
                if let Some(labels) = obj::ensure_map(v, &["metadata", "labels"]) {
                    labels.insert(ARGO_INSTANCE.into(), Value::String(app.cfg.tracked_as().to_owned()));
                }
            }
            Tracking::ArgoAnnotation | Tracking::ArgoAnnotationAndLabel => {
                if let Some(ann) = obj::ensure_map(v, &["metadata", "annotations"]) {
                    ann.insert(ARGO_TRACKING.into(), Value::String(format!("{}:{}", app.cfg.tracked_as(), key.tracking())));
                }
                if app.cfg.tracking == Tracking::ArgoAnnotationAndLabel
                    && let Some(labels) = obj::ensure_map(v, &["metadata", "labels"]) {
                        labels.insert(ARGO_INSTANCE.into(), Value::String(app.cfg.tracked_as().to_owned()));
                    }
            }
        }
    }

    /// The app a live object belongs to, by its tracking metadata. The annotation must name this
    /// very object (`<app>:<group>/<kind>:<namespace>/<name>`), so a copied object claims nothing.
    fn owner_of(&self, key: &ResKey, v: &Value) -> Option<AppIx> {
        for ann in [TRACKING, ARGO_TRACKING] {
            if let Some((app_id, rest)) = obj::annotation(v, ann).and_then(|t| t.split_once(':'))
                && rest == key.tracking()
            {
                return self.app_by_tracking.get(app_id).copied();
            }
        }
        let instance = obj::label(v, ARGO_INSTANCE)?;
        let a = *self.app_by_tracking.get(instance)?;
        self.app(a).filter(|app| app.cfg.tracking == Tracking::ArgoLabel).map(|_| a)
    }

    fn live(&mut self, c: ClusterIx, change: LiveChange) {
        match change {
            LiveChange::Upsert(v) => self.upsert_live(c, v),
            LiveChange::Delete(key) => self.delete_live(c, &key),
            LiveChange::Listed { kind, objects } => {
                let mut seen: FxHashSet<ResKey> = FxHashSet::default();
                for v in objects {
                    if let Some(k) = self.live_key(c, &v) {
                        seen.insert(k);
                    }
                    self.upsert_live(c, v);
                }
                let gone: Vec<ResKey> = match self.clusters.get(c as usize) {
                    Some(cl) => cl
                        .index
                        .keys()
                        .filter(|k| k.group == kind.group && k.kind == kind.kind && !seen.contains(*k))
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                };
                for k in gone {
                    self.delete_live(c, &k);
                }
            }
        }
    }

    fn live_key(&mut self, c: ClusterIx, v: &Value) -> Option<ResKey> {
        let cl = self.clusters.get(c as usize)?;
        obj::key_of(v, &mut self.interner, "", &cl.scopes)
    }

    fn upsert_live(&mut self, c: ClusterIx, v: Value) {
        self.stats.live_events += 1;
        let Some(key) = self.live_key(c, &v) else { return };
        let owner = self.owner_of(&key, &v);
        let existing = self.clusters.get(c as usize).and_then(|cl| cl.index.get(&key).copied());
        let ix = match (existing, owner) {
            (Some(ix), _) => ix,
            (None, Some(_)) => self.res_for(c, key.clone()),
            (None, None) => return,
        };
        let cfg = owner.or_else(|| self.res(c, ix).and_then(|r| r.app)).and_then(|a| self.app(a)).map(|app| app.cfg.clone());
        let manager: Str = cfg.as_ref().map(|c| c.manager.clone()).unwrap_or_else(|| "kiln".into());
        let status = health::assess(&v, self.rules.as_ref())
            .or_else(|| if self.cfg.generic_health { health::generic_conditions(&v) } else { None });
        let terminating = obj::get(&v, &["metadata", "deletionTimestamp"]).is_some_and(|t| !t.is_null());
        let hash = diff::live_hash(&v, &manager);
        let (health, message) = (status.as_ref().map(|s| s.health), status.map(|s| s.message));
        let message = message.filter(|m| !m.is_empty()).map(String::into_boxed_str);
        let mut moved = false;
        let mut health_moved = false;
        if let Some(res) = self.res_mut(c, ix) {
            if res.app.is_none() {
                res.app = owner;
            }
            match &mut res.live {
                // Only status (or the server's bookkeeping) moved: no copy, no comparison.
                Some(l) if l.hash == hash => {
                    health_moved = l.health != health || l.terminating != terminating;
                    l.health = health;
                    l.message = message;
                    l.terminating = terminating;
                }
                _ => {
                    moved = true;
                    let hook = sync::hook_of(&v).is_some();
                    // Only kiln's own apply and the managers its ignore rules name are read again.
                    let view = LiveView::keeping(v, &manager, |m| {
                        cfg.as_ref().is_some_and(|c| c.ignore.iter().any(|r| r.managers.iter().any(|x| x == m)))
                    });
                    res.live = Some(Live { blob: Blob::encode(&recombine(view)), hash, health, message, hook, terminating });
                }
            }
        }
        if let Some(a) = self.res(c, ix).and_then(|r| r.app)
            && let Some(app) = self.app_mut(a) {
                app.resources.insert(ix);
            }
        if moved {
            self.recompare(c, ix);
        } else {
            self.stats.live_unchanged += 1;
            if health_moved {
                self.count(c, ix);
            }
        }
        if let Some(a) = self.res(c, ix).and_then(|r| r.app)
            && self.app(a).is_some_and(|app| app.op.is_some()) {
                self.mark(a);
            }
    }

    fn delete_live(&mut self, c: ClusterIx, key: &ResKey) {
        self.stats.live_events += 1;
        let Some(ix) = self.clusters.get(c as usize).and_then(|cl| cl.index.get(key).copied()) else { return };
        if let Some(res) = self.res_mut(c, ix) {
            res.live = None;
        }
        self.recompare(c, ix);
        if let Some(a) = self.res(c, ix).and_then(|r| r.app) {
            self.mark(a);
        }
        let keep = self.res(c, ix).is_some_and(|r| r.desired.is_some());
        if !keep {
            self.uncount(c, ix);
            self.gc_res(c, ix);
        }
    }

    /// Compares a resource again and updates its app's counts.
    fn recompare(&mut self, c: ClusterIx, ix: ResIx) {
        let state = self.classify(c, ix);
        self.uncount(c, ix);
        if let Some(res) = self.res_mut(c, ix) {
            res.state = state;
        }
        self.count(c, ix);
    }

    fn classify(&mut self, c: ClusterIx, ix: ResIx) -> ResState {
        let Some(res) = self.res(c, ix) else { return ResState::None };
        let Some(a) = res.app else { return ResState::None };
        let Some(app) = self.app(a) else { return ResState::None };
        match (&res.desired, &res.live) {
            (None, Some(l)) if l.hook => ResState::None,
            (None, Some(_)) => ResState::Prune,
            (None, None) => ResState::None,
            (Some(_), None) => ResState::Missing,
            (Some(_), Some(l)) => {
                let Some(d) = self.desired_object(app, res) else { return ResState::OutOfSync };
                let Ok(live) = l.blob.decode() else { return ResState::OutOfSync };
                let cfg = app.cfg.clone();
                self.stats.comparisons += 1;
                let view = LiveView::new(live, &cfg.manager);
                let owned = view.owned(&cfg.manager);
                let (ignores, _) = Ignores::for_object(cfg.ignore.iter(), &d);
                let verdict = if ignores.is_empty() {
                    diff::compare(&d, &view.object, owned.as_ref())
                } else {
                    let (mut d2, mut l2) = (d, view.object.clone());
                    ignores.strip(&mut d2, Some(&view));
                    ignores.strip(&mut l2, Some(&view));
                    diff::compare(&d2, &l2, owned.as_ref())
                };
                match verdict {
                    Verdict::Synced => ResState::Synced,
                    Verdict::OutOfSync { .. } => ResState::OutOfSync,
                }
            }
        }
    }

    fn contribution(&self, c: ClusterIx, ix: ResIx) -> Option<(AppIx, bool, Option<Health>)> {
        let res = self.res(c, ix)?;
        let a = res.app?;
        let health = match res.state {
            ResState::None => return None,
            ResState::Missing => Some(Health::Missing),
            // Objects to prune don't count toward their app's health (Argo CD's rule).
            ResState::Prune => None,
            ResState::Synced | ResState::OutOfSync => res.live.as_ref().and_then(|l| l.health),
        };
        let differs = matches!(res.state, ResState::OutOfSync | ResState::Missing | ResState::Prune);
        Some((a, differs, health))
    }

    fn count(&mut self, c: ClusterIx, ix: ResIx) {
        self.uncount(c, ix);
        let Some((a, differs, health)) = self.contribution(c, ix) else { return };
        let desired = self.res(c, ix).is_some_and(|r| r.desired.is_some());
        if let Some(app) = self.app_mut(a) {
            if desired {
                app.counts.resources += 1;
            }
            if differs {
                app.counts.out_of_sync += 1;
            }
            if let Some(slot) = health.and_then(|h| app.counts.health.get_mut(h.index())) {
                *slot += 1;
            }
        }
        if let Some(res) = self.res_mut(c, ix) {
            res_counted_set(res, Some((differs, health, desired)));
        }
        self.mark(a);
    }

    fn uncount(&mut self, c: ClusterIx, ix: ResIx) {
        let Some(res) = self.res_mut(c, ix) else { return };
        let Some((differs, health, desired)) = res_counted_take(res) else { return };
        let Some(a) = res.app else { return };
        if let Some(app) = self.app_mut(a) {
            if desired {
                app.counts.resources = app.counts.resources.saturating_sub(1);
            }
            if differs {
                app.counts.out_of_sync = app.counts.out_of_sync.saturating_sub(1);
            }
            if let Some(slot) = health.and_then(|h| app.counts.health.get_mut(h.index())) {
                *slot = slot.saturating_sub(1);
            }
        }
        self.mark(a);
    }

    fn recompare_app(&mut self, a: AppIx) {
        let Some(app) = self.app(a) else { return };
        let c = app.cluster;
        let all: Vec<ResIx> = app.resources.iter().copied().collect();
        for ix in all {
            self.recompare(c, ix);
        }
        self.mark(a);
    }

    // ---------------------------------------------------------------- status

    fn summary(&self, a: AppIx) -> Option<Summary> {
        let app = self.app(a)?;
        let cl = self.clusters.get(app.cluster as usize)?;
        let mut message = None;
        let sync = if let Some(e) = &app.render_error {
            message = Some(format!("render failed: {e}"));
            Sync::Unknown
        } else if app.render.is_none() {
            Sync::Unknown
        } else if !cl.reachable {
            message = Some(format!("cluster {} is unreachable", cl.name));
            Sync::Unknown
        } else if app.counts.out_of_sync > 0 {
            Sync::OutOfSync
        } else {
            Sync::Synced
        };
        let health = Health::ALL
            .iter()
            .rev()
            .find(|h| app.counts.health.get(h.index()).is_some_and(|n| *n > 0))
            .copied()
            .unwrap_or(Health::Healthy);
        let operation = app.op.as_ref().map(op_status).or_else(|| app.last_op.clone());
        Some(Summary {
            app: app.cfg.id.clone(),
            sync,
            health,
            revision: app.revision.clone(),
            resources: app.counts.resources,
            out_of_sync: app.counts.out_of_sync,
            operation,
            message,
        })
    }

    /// The current status of an app.
    pub fn status(&self, id: &str) -> Option<Summary> {
        self.summary(*self.app_by_id.get(id)?)
    }

    /// Every app's id.
    pub fn app_ids(&self) -> impl Iterator<Item = &Str> {
        self.app_by_id.keys()
    }

    /// An app's resources.
    pub fn resources(&self, id: &str) -> Vec<ResourceView> {
        let Some(&a) = self.app_by_id.get(id) else { return Vec::new() };
        let Some(app) = self.app(a) else { return Vec::new() };
        let mut out: Vec<ResourceView> = app
            .resources
            .iter()
            .filter_map(|ix| self.res(app.cluster, *ix))
            .filter(|r| r.state != ResState::None || r.live.as_ref().is_some_and(|l| l.hook))
            .map(|r| ResourceView {
                key: r.key.clone(),
                sync: match r.state {
                    ResState::Synced => Sync::Synced,
                    ResState::None => Sync::Unknown,
                    _ => Sync::OutOfSync,
                },
                missing: r.state == ResState::Missing,
                requires_pruning: r.state == ResState::Prune,
                health: if r.state == ResState::Missing { Some(Health::Missing) } else { r.live.as_ref().and_then(|l| l.health) },
                message: r.live.as_ref().and_then(|l| l.message.as_deref().map(str::to_owned)),
                hook: r.live.as_ref().is_some_and(|l| l.hook),
            })
            .collect();
        out.sort_by(|x, y| x.key.cmp(&y.key));
        out
    }

    /// The desired and live forms of one of an app's objects, for showing a diff.
    pub fn object(&self, id: &str, key: &ResKey) -> Option<(Option<Value>, Option<Value>)> {
        let a = *self.app_by_id.get(id)?;
        let app = self.app(a)?;
        let cl = self.clusters.get(app.cluster as usize)?;
        let res = self.res(app.cluster, *cl.index.get(key)?)?;
        Some((self.desired_object(app, res), res.live.as_ref().and_then(|l| l.blob.decode().ok())))
    }

    fn settle(&mut self) {
        let mut guard = 0;
        while !self.dirty.is_empty() && guard < 8 {
            guard += 1;
            let dirty = std::mem::take(&mut self.dirty);
            for &a in &dirty {
                if let Some(app) = self.app_mut(a) {
                    app.dirty = false;
                }
            }
            for a in dirty {
                self.advance_op(a);
                self.maybe_auto(a);
                self.publish(a);
            }
        }
    }

    fn publish(&mut self, a: AppIx) {
        let Some(s) = self.summary(a) else { return };
        let Some(app) = self.app_mut(a) else { return };
        if app.published.as_ref() != Some(&s) {
            app.published = Some(s.clone());
            self.stats.statuses += 1;
            self.out.push_back(Action::Status(s));
        }
    }

    // ---------------------------------------------------------------- automation

    fn maybe_auto(&mut self, a: AppIx) {
        let now = self.now;
        let Some(app) = self.app(a) else { return };
        if !app.cfg.sync.auto || app.cfg.read_only || app.op.is_some() || app.retry.is_some() || app.render_error.is_some() {
            return;
        }
        let Some(render) = app.render.as_ref() else { return };
        let Some(key) = app.key.filter(|k| *k == render.key) else { return };
        if !self.clusters.get(app.cluster as usize).is_some_and(|cl| cl.reachable) {
            return;
        }
        if app.counts.out_of_sync == 0 {
            return;
        }
        if render.objects.is_empty() && !app.cfg.sync.allow_empty {
            return;
        }
        // Only objects to prune differ, and automatic syncs don't prune: nothing to do.
        let only_prune = self.only_prune_differs(a);
        if only_prune && !app.cfg.sync.prune {
            return;
        }
        let trigger = match app.auto.attempted {
            Some((k, _)) if k != key => Trigger::Auto,
            None => Trigger::Auto,
            Some((_, true)) => {
                if !app.cfg.sync.self_heal {
                    return;
                }
                if now < app.auto.heal_after_ms {
                    let at = app.auto.heal_after_ms;
                    if app.auto.heal_timer_ms != at {
                        if let Some(app) = self.app_mut(a) {
                            app.auto.heal_timer_ms = at;
                        }
                        self.timer(at, Timer::Heal(a));
                    }
                    return;
                }
                Trigger::SelfHeal
            }
            // This revision already failed to sync automatically; wait for a new one.
            Some((_, false)) => return,
        };
        let request = SyncRequest { prune: Some(app.cfg.sync.prune), trigger: Some(trigger), ..SyncRequest::default() };
        self.start_op(a, trigger, request);
    }

    fn only_prune_differs(&self, a: AppIx) -> bool {
        let Some(app) = self.app(a) else { return false };
        app.resources
            .iter()
            .filter_map(|ix| self.res(app.cluster, *ix))
            .filter(|r| matches!(r.state, ResState::OutOfSync | ResState::Missing | ResState::Prune))
            .all(|r| r.state == ResState::Prune)
    }

    // ---------------------------------------------------------------- syncs

    fn start_op(&mut self, a: AppIx, trigger: Trigger, request: SyncRequest) {
        let now = self.now;
        let id = self.next_request_id();
        let Some(app) = self.app(a) else { return };
        if app.op.is_some() {
            return;
        }
        // Shadow mode compares and reports; it may dry-run, never write.
        let request = if app.cfg.read_only { SyncRequest { dry_run: true, ..request } } else { request };
        let Some(render) = app.render.clone() else { return };
        let options = request.options.or(app.cfg.sync.options);
        let prune = request.prune.unwrap_or(false);
        let selected: Option<FxHashSet<ResKey>> = request.resources.as_ref().map(|r| r.iter().cloned().collect());
        let c = app.cluster;
        // Candidates: every resource of the app, plus its hooks.
        let mut keys: Vec<ResKey> = Vec::new();
        let mut desired: Vec<Option<Value>> = Vec::new();
        let mut live: Vec<Option<Value>> = Vec::new();
        let mut differs: Vec<bool> = Vec::new();
        let mut ixs: Vec<Option<ResIx>> = Vec::new();
        for ix in &app.resources {
            let Some(res) = self.res(c, *ix) else { continue };
            if res.live.as_ref().is_some_and(|l| l.hook) && res.desired.is_none() {
                continue;
            }
            keys.push(res.key.clone());
            desired.push(self.desired_object(app, res));
            live.push(res.live.as_ref().and_then(|l| l.blob.decode().ok()));
            differs.push(matches!(res.state, ResState::OutOfSync | ResState::Missing | ResState::Prune));
            ixs.push(Some(*ix));
        }
        for &h in &app.hooks {
            let Some(r) = render.objects.get(h as usize) else { continue };
            let Ok(mut v) = r.blob.decode() else { continue };
            let ns = if self.clusters.get(c as usize).is_some_and(|cl| cl.scopes.namespaced(&r.group, &r.kind)) {
                if r.namespace.is_empty() { app.cfg.namespace.clone() } else { r.namespace.clone() }
            } else {
                "".into()
            };
            let key = ResKey { group: r.group.clone(), kind: r.kind.clone(), namespace: ns, name: r.name.clone() };
            self.decorate(app, &key, &mut v);
            keys.push(key);
            desired.push(Some(v));
            live.push(None);
            differs.push(true);
            ixs.push(None);
        }
        let cands: Vec<sync::Candidate> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| sync::Candidate {
                key: k,
                desired: desired.get(i).and_then(Option::as_ref),
                live: live.get(i).and_then(Option::as_ref),
                out_of_sync: differs.get(i).copied().unwrap_or(true),
            })
            .collect();
        let plan = sync::plan(&cands, sync::Options { prune, sync: options }, selected.as_ref());
        let by_key: FxHashMap<&ResKey, Option<ResIx>> = keys.iter().zip(ixs.iter().copied()).collect();
        let res: Vec<Option<ResIx>> = plan.tasks.iter().map(|t| by_key.get(&t.key).copied().flatten()).collect();
        let groups: FxHashSet<(Phase, i64)> = plan.tasks.iter().map(|t| (t.phase, t.wave)).collect();
        let multi_step = groups.len() > 1 || plan.tasks.iter().any(|t| t.action == sync::Action::Hook);
        let n = plan.tasks.len();
        let fail_n = plan.on_fail.len();
        let revision = app.revision.clone();
        let app_id = app.cfg.id.clone();
        let mut op = Op {
            id,
            trigger,
            by: request.by.clone(),
            revision: revision.clone(),
            render: Some(render),
            tasks: plan.tasks,
            res,
            states: vec![TaskState::Pending; n],
            on_fail: plan.on_fail,
            on_fail_states: vec![TaskState::Pending; fail_n],
            dry_run_only: request.dry_run,
            options,
            phase: OpPhase::Running,
            attempt: 1,
            message: "syncing".into(),
            wave_ready_ms: now,
            multi_step,
            pending_dry_runs: 0,
            applied: 0,
            pruned: 0,
            request,
            requests: Vec::new(),
        };
        if !plan.held.is_empty() {
            op.message = format!("{} objects need pruning but are kept by their Prune option", plan.held.len());
        }
        self.stats.syncs += 1;
        self.out.push_back(Action::Record(Record::Started {
            app: app_id,
            op: id,
            revision,
            trigger,
            by: op.by.clone(),
            at_ms: now,
        }));
        // The dry run keeps a sync from applying half of what it holds; one object can't be half
        // applied, so a sync of one object skips it and costs one request, not two.
        let applies = op.tasks.iter().filter(|t| t.action != sync::Action::Prune).count();
        let dry_first = (self.cfg.dry_run_first && applies > 1) || op.dry_run_only;
        if let Some(app) = self.app_mut(a) {
            app.op = Some(op);
            app.retry = None;
        }
        if dry_first {
            self.start_dry_run(a);
        }
        self.mark(a);
    }

    fn start_dry_run(&mut self, a: AppIx) {
        let Some(app) = self.app(a) else { return };
        let Some(op) = app.op.as_ref() else { return };
        let c = app.cluster;
        let mut sends = Vec::new();
        for (i, t) in op.tasks.iter().enumerate() {
            if t.action == sync::Action::Prune {
                continue;
            }
            let skip_missing = op.options.skip_dry_run_on_missing_resource.unwrap_or(false);
            let crd_in_sync = op.tasks.iter().any(|o| o.key.kind.as_ref() == "CustomResourceDefinition");
            if skip_missing && crd_in_sync && !t.key.group.is_empty() && t.rank == sync::KIND_ORDER.len() {
                continue;
            }
            if let Some(obj) = self.task_object(app, op, i, false) {
                sends.push((i as u32, obj));
            }
        }
        let trigger = op.trigger;
        let n = sends.len() as u32;
        let manager = app.cfg.manager.clone();
        let ssa = op.options.ssa();
        for (task, object) in sends {
            let id = self.next_request_id();
            let cluster = self.cluster_name(c);
            let op_id = self.app(a).and_then(|x| x.op.as_ref()).map(|o| o.id).unwrap_or(0);
            self.requests.insert(id, Request {
                app: Some(a),
                op: op_id,
                cluster: c,
                kind: ReqKind::Apply { task, fail_phase: false, dry_run: true },
                action: Some(Action::Apply {
                    id,
                    cluster,
                    object,
                    server_side: ssa,
                    force: false,
                    dry_run: true,
                    replace: false,
                    manager: manager.clone(),
                }),
            });
            self.own_request(a, id);
            self.enqueue(c, id, trigger);
        }
        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
            op.pending_dry_runs = n;
            if n > 0 {
                op.phase = OpPhase::DryRun;
                op.message = "validating".into();
            }
        }
        self.pump(c);
    }

    /// The object a task applies.
    fn task_object(&self, app: &App, op: &Op, i: usize, fail_phase: bool) -> Option<Value> {
        let t = if fail_phase { op.on_fail.get(i)? } else { op.tasks.get(i)? };
        if let Some(ix) = op.res.get(i).copied().flatten().filter(|_| !fail_phase) {
            let res = self.res(app.cluster, ix)?;
            return self.desired_object(app, res);
        }
        // A hook: find it in the render by key.
        let render = op.render.as_ref()?;
        let r = render.objects.iter().find(|r| r.hook && r.kind == t.key.kind && r.name == t.key.name && r.group == t.key.group)?;
        let mut v = r.blob.decode().ok()?;
        self.decorate(app, &t.key, &mut v);
        Some(v)
    }

    fn advance_op(&mut self, a: AppIx) {
        let now = self.now;
        let Some(app) = self.app(a) else { return };
        let Some(op) = app.op.as_ref() else { return };
        let c = app.cluster;
        match op.phase {
            OpPhase::DryRun => {
                if op.pending_dry_runs > 0 {
                    return;
                }
                if op.dry_run_only {
                    self.finish_op(a, true, "dry run succeeded".into());
                    return;
                }
                if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                    op.phase = OpPhase::Running;
                    op.message = "syncing".into();
                }
                self.advance_op(a);
            }
            OpPhase::Running => {
                // Settle what was applied: health for multi-step syncs, hook results.
                let updates = self.task_updates(a);
                let mut failed: Option<String> = None;
                if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                    for (i, s, msg) in updates {
                        if let Some(st) = op.states.get_mut(i) {
                            *st = s;
                        }
                        if s == TaskState::Failed && failed.is_none() {
                            failed = Some(msg);
                        }
                    }
                }
                if let Some(msg) = failed {
                    self.fail_op(a, msg);
                    return;
                }
                let Some(op) = self.app(a).and_then(|x| x.op.as_ref()) else { return };
                if op.states.contains(&TaskState::Running) {
                    return;
                }
                let Some((phase, wave)) = sync::next_group(&op.tasks, &op.states) else {
                    self.finish_op(a, true, "successfully synced (all tasks run)".into());
                    return;
                };
                let group: Vec<usize> = op
                    .tasks
                    .iter()
                    .enumerate()
                    .filter(|(i, t)| t.phase == phase && t.wave == wave && op.states.get(*i) == Some(&TaskState::Pending))
                    .map(|(i, _)| i)
                    .collect();
                if group.is_empty() {
                    // The group's tasks are sent or done and not settled yet: wait for them.
                    return;
                }
                if now < op.wave_ready_ms {
                    let at = op.wave_ready_ms;
                    self.timer(at, Timer::Wave(a, op.id));
                    return;
                }
                self.send_group(a, c, &group, false);
            }
            OpPhase::Failing => {
                let updates = self.fail_task_updates(a);
                if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                    for (i, s) in updates {
                        if let Some(st) = op.on_fail_states.get_mut(i) {
                            *st = s;
                        }
                    }
                }
                let Some(op) = self.app(a).and_then(|x| x.op.as_ref()) else { return };
                let pending: Vec<usize> =
                    op.on_fail_states.iter().enumerate().filter(|(_, s)| **s == TaskState::Pending).map(|(i, _)| i).collect();
                if !pending.is_empty() {
                    self.send_group(a, c, &pending, true);
                    return;
                }
                if op.on_fail_states.iter().all(|s| matches!(s, TaskState::Settled | TaskState::Failed)) {
                    let msg = op.message.clone();
                    self.retry_or_fail(a, msg);
                }
            }
            OpPhase::Terminating => {
                let inflight = op.requests.iter().any(|id| self.requests.contains_key(id));
                if !inflight {
                    self.finish_op(a, false, "terminated".into());
                }
            }
            OpPhase::Succeeded | OpPhase::Failed => {}
        }
    }

    /// New states for tasks that were sent: an applied object is settled once healthy (or
    /// at once when the sync has one step), a hook once it completed, a prune once it is gone.
    fn task_updates(&self, a: AppIx) -> Vec<(usize, TaskState, String)> {
        let mut out = Vec::new();
        let Some(app) = self.app(a) else { return out };
        let Some(op) = app.op.as_ref() else { return out };
        let c = app.cluster;
        let index = self.clusters.get(c as usize).map(|cl| &cl.index);
        for (i, (t, s)) in op.tasks.iter().zip(&op.states).enumerate() {
            if *s != TaskState::Running && *s != TaskState::Done {
                continue;
            }
            let res = op.res.get(i).copied().flatten().or_else(|| index.and_then(|ix| ix.get(&t.key).copied()));
            let live = res.and_then(|ix| self.res(c, ix)).and_then(|r| r.live.as_ref());
            match t.action {
                sync::Action::Prune => match live {
                    None => out.push((i, TaskState::Settled, String::new())),
                    Some(l) if !l.terminating && *s == TaskState::Done => out.push((i, TaskState::Settled, String::new())),
                    _ => {}
                },
                sync::Action::Hook => match live.map(|l| l.health) {
                    Some(None | Some(Health::Healthy)) => out.push((i, TaskState::Settled, String::new())),
                    Some(Some(Health::Degraded)) => out.push((
                        i,
                        TaskState::Failed,
                        format!("hook {} failed: {}", t.key, live.and_then(|l| l.message.as_deref()).unwrap_or("")),
                    )),
                    _ => {}
                },
                sync::Action::Apply => {
                    if *s == TaskState::Done && !op.multi_step {
                        out.push((i, TaskState::Settled, String::new()));
                        continue;
                    }
                    let final_group = op.tasks.iter().skip(i).all(|o| o.phase == t.phase && o.wave == t.wave);
                    let group_has_hooks = op.tasks.iter().any(|o| o.phase == t.phase && o.wave == t.wave && o.action == sync::Action::Hook);
                    if final_group && !group_has_hooks && *s == TaskState::Done {
                        out.push((i, TaskState::Settled, String::new()));
                        continue;
                    }
                    match live.map(|l| l.health) {
                        Some(None) | Some(Some(Health::Healthy)) => out.push((i, TaskState::Settled, String::new())),
                        Some(Some(Health::Degraded)) => out.push((
                            i,
                            TaskState::Failed,
                            format!("{} is degraded: {}", t.key, live.and_then(|l| l.message.as_deref()).unwrap_or("")),
                        )),
                        _ => {}
                    }
                }
            }
        }
        out
    }

    fn fail_task_updates(&self, a: AppIx) -> Vec<(usize, TaskState)> {
        let mut out = Vec::new();
        let Some(app) = self.app(a) else { return out };
        let Some(op) = app.op.as_ref() else { return out };
        let c = app.cluster;
        let Some(cl) = self.clusters.get(c as usize) else { return out };
        for (i, (t, s)) in op.on_fail.iter().zip(&op.on_fail_states).enumerate() {
            if *s != TaskState::Done && *s != TaskState::Running {
                continue;
            }
            let health = cl.index.get(&t.key).and_then(|ix| self.res(c, *ix)).and_then(|r| r.live.as_ref()).map(|l| l.health);
            match health {
                Some(None | Some(Health::Healthy)) => out.push((i, TaskState::Settled)),
                Some(Some(Health::Degraded)) => out.push((i, TaskState::Failed)),
                _ => {}
            }
        }
        out
    }

    fn send_group(&mut self, a: AppIx, c: ClusterIx, group: &[usize], fail_phase: bool) {
        let Some(app) = self.app(a) else { return };
        let Some(op) = app.op.as_ref() else { return };
        let manager = app.cfg.manager.clone();
        let trigger = op.trigger;
        let op_id = op.id;
        let ssa = op.options.ssa();
        let replace = op.options.replace.unwrap_or(false);
        let force = op.options.force.unwrap_or(false);
        let propagation = op.options.propagation.unwrap_or_default();
        let mut sends: Vec<(u32, Action)> = Vec::new();
        let mut hook_cleanups: Vec<ResKey> = Vec::new();
        let cluster = self.cluster_name(c);
        for &i in group {
            let task = if fail_phase { op.on_fail.get(i) } else { op.tasks.get(i) };
            let Some(t) = task else { continue };
            match t.action {
                sync::Action::Prune => {
                    sends.push((i as u32, Action::Delete { id: 0, cluster: cluster.clone(), key: t.key.clone(), propagation }));
                }
                sync::Action::Apply | sync::Action::Hook => {
                    let Some(object) = self.task_object(app, op, i, fail_phase) else { continue };
                    if t.action == sync::Action::Hook {
                        let before = sync::hook_of(&object).is_some_and(|h| h.delete.before_creation);
                        let exists = self.clusters.get(c as usize).is_some_and(|cl| cl.index.contains_key(&t.key));
                        if before && exists {
                            hook_cleanups.push(t.key.clone());
                        }
                    }
                    sends.push((i as u32, Action::Apply {
                        id: 0,
                        cluster: cluster.clone(),
                        object,
                        server_side: ssa,
                        force,
                        dry_run: false,
                        replace,
                        manager: manager.clone(),
                    }));
                }
            }
        }
        for key in hook_cleanups {
            // BeforeHookCreation: the previous run's object goes first.
            let id = self.next_request_id();
            self.requests.insert(id, Request {
                app: None,
                op: 0,
                cluster: c,
                kind: ReqKind::Cleanup,
                action: Some(Action::Delete { id, cluster: cluster.clone(), key, propagation: Propagation::Background }),
            });
            self.enqueue(c, id, trigger);
        }
        for (task, mut action) in sends {
            let id = self.next_request_id();
            let kind = match &mut action {
                Action::Apply { id: slot, .. } => {
                    *slot = id;
                    ReqKind::Apply { task, fail_phase, dry_run: false }
                }
                Action::Delete { id: slot, .. } => {
                    *slot = id;
                    ReqKind::Delete { task, fail_phase }
                }
                _ => continue,
            };
            self.requests.insert(id, Request { app: Some(a), op: op_id, cluster: c, kind, action: Some(action) });
            self.own_request(a, id);
            self.enqueue(c, id, trigger);
        }
        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
            let states = if fail_phase { &mut op.on_fail_states } else { &mut op.states };
            for &i in group {
                if let Some(s) = states.get_mut(i) {
                    *s = TaskState::Running;
                }
            }
        }
        self.pump(c);
    }

    fn answer(&mut self, id: u64, result: Result<Option<Value>, String>) {
        let Some(req) = self.requests.remove(&id) else { return };
        let c = req.cluster;
        if let Some(cl) = self.clusters.get_mut(c as usize) {
            cl.inflight = cl.inflight.saturating_sub(1);
        }
        let applied = match &result {
            Ok(Some(v)) => Some(v.clone()),
            _ => None,
        };
        if let Some(v) = applied.filter(|_| !matches!(req.kind, ReqKind::Apply { dry_run: true, .. })) {
            self.upsert_live(c, v);
        }
        if let Some(a) = req.app {
            let current = self.app(a).and_then(|x| x.op.as_ref()).is_some_and(|op| op.id == req.op);
            if current {
                match req.kind {
                    ReqKind::Apply { dry_run: true, .. } => {
                        let mut fail = None;
                        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                            op.pending_dry_runs = op.pending_dry_runs.saturating_sub(1);
                            if let Err(e) = &result {
                                fail = Some(format!("one or more objects failed to apply (dry run): {e}"));
                            }
                        }
                        if let Some(msg) = fail {
                            self.fail_op(a, msg);
                        }
                    }
                    ReqKind::Apply { task, fail_phase, .. } | ReqKind::Delete { task, fail_phase } => {
                        let is_delete = matches!(req.kind, ReqKind::Delete { .. });
                        let mut fail = None;
                        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                            let states = if fail_phase { &mut op.on_fail_states } else { &mut op.states };
                            if let Some(s) = states.get_mut(task as usize) {
                                match &result {
                                    Ok(_) => {
                                        *s = TaskState::Done;
                                        if !fail_phase {
                                            if is_delete {
                                                op.pruned += 1;
                                            } else {
                                                op.applied += 1;
                                            }
                                        }
                                    }
                                    // An object already gone is pruned.
                                    Err(e) if is_delete && e.contains("not found") => *s = TaskState::Done,
                                    Err(e) => {
                                        *s = TaskState::Failed;
                                        if !fail_phase {
                                            fail = Some(e.clone());
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(msg) = fail {
                            self.fail_op(a, format!("one or more objects failed to apply: {msg}"));
                        }
                    }
                    ReqKind::Cleanup => {}
                }
            }
            self.mark(a);
        }
        self.pump(c);
    }

    fn fail_op(&mut self, a: AppIx, message: String) {
        let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) else { return };
        op.message = message;
        if !op.on_fail.is_empty() && op.phase != OpPhase::Failing {
            op.phase = OpPhase::Failing;
            self.mark(a);
            return;
        }
        let msg = op.message.clone();
        self.retry_or_fail(a, msg);
    }

    fn retry_or_fail(&mut self, a: AppIx, message: String) {
        let now = self.now;
        let Some(app) = self.app(a) else { return };
        let retry: Option<Retry> = app.cfg.sync.retry;
        let attempt = app.op.as_ref().map(|o| o.attempt).unwrap_or(1);
        if let Some(r) = retry.filter(|r| r.allows(attempt)) {
            let wait = r.delay_ms(attempt);
            let request = app.op.as_ref().map(|o| o.request.clone()).unwrap_or_default();
            let trigger = app.op.as_ref().map(|o| o.trigger).unwrap_or(Trigger::Manual);
            let id = app.op.as_ref().map(|o| o.id).unwrap_or(0);
            self.drop_requests(a);
            if let Some(app) = self.app_mut(a) {
                app.op = None;
                app.last_op = Some(OpStatus {
                    id,
                    phase: OpPhase::Failed,
                    trigger,
                    attempt,
                    message: format!("{message} (retrying in {} s, attempt {} of {})", wait / 1000, attempt + 1, r.limit + 1),
                });
                app.retry = Some(PendingRetry { request, trigger, attempt: attempt + 1, at_ms: now + wait });
            }
            self.timer(now + wait, Timer::Retry(a));
            self.mark(a);
            return;
        }
        self.finish_op(a, false, message);
    }

    fn finish_op(&mut self, a: AppIx, succeeded: bool, message: String) {
        let now = self.now;
        let (heal_first, heal_factor, heal_cap) = (self.cfg.heal_backoff_ms, self.cfg.heal_factor, self.cfg.heal_cap_ms);
        self.drop_requests(a);
        let Some(app) = self.app_mut(a) else { return };
        let Some(op) = app.op.take() else { return };
        let key = op.render.as_ref().map(|r| r.key);
        // Like Argo CD, the last sync of a revision decides what automation does with it, whoever
        // started it: after a success, only self-heal syncs it again; after a failure, nothing does
        // until the revision moves.
        if let Some(k) = key {
            app.auto.attempted = Some((k, succeeded));
        }
        app.retry = None;
        if op.trigger == Trigger::SelfHeal {
            let next = if app.auto.heal_backoff_ms == 0 {
                heal_first
            } else {
                app.auto.heal_backoff_ms.saturating_mul(heal_factor).min(heal_cap)
            };
            app.auto.heal_backoff_ms = next;
            app.auto.heal_after_ms = now + next;
        } else if succeeded {
            app.auto.heal_backoff_ms = 0;
            app.auto.heal_after_ms = now + heal_first;
        }
        app.last_op = Some(OpStatus {
            id: op.id,
            phase: if succeeded { OpPhase::Succeeded } else { OpPhase::Failed },
            trigger: op.trigger,
            attempt: op.attempt,
            message: message.clone(),
        });
        let record = Record::Finished {
            app: app.cfg.id.clone(),
            op: op.id,
            revision: op.revision.clone(),
            key,
            succeeded,
            message,
            applied: op.applied,
            pruned: op.pruned,
            at_ms: now,
        };
        // Hooks whose policy deletes them on this outcome.
        let c = app.cluster;
        let mut cleanups = Vec::new();
        if let Some(render) = op.render.as_ref() {
            for t in op.tasks.iter().chain(op.on_fail.iter()).filter(|t| t.action == sync::Action::Hook) {
                let Some(r) = render.objects.iter().find(|r| r.hook && r.kind == t.key.kind && r.name == t.key.name) else { continue };
                let Some(h) = r.blob.decode().ok().as_ref().and_then(sync::hook_of) else { continue };
                if (succeeded && h.delete.on_success) || (!succeeded && h.delete.on_failure) {
                    cleanups.push(t.key.clone());
                }
            }
        }
        if succeeded {
            self.stats.syncs_ok += 1;
        } else {
            self.stats.syncs_failed += 1;
        }
        self.out.push_back(Action::Record(record));
        let cluster = self.cluster_name(c);
        for key in cleanups {
            let id = self.next_request_id();
            self.requests.insert(id, Request {
                app: None,
                op: 0,
                cluster: c,
                kind: ReqKind::Cleanup,
                action: Some(Action::Delete { id, cluster: cluster.clone(), key, propagation: Propagation::Background }),
            });
            self.enqueue(c, id, Trigger::Auto);
        }
        self.pump(c);
        self.mark(a);
    }

    fn terminate(&mut self, a: AppIx) {
        let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) else { return };
        op.phase = OpPhase::Terminating;
        op.message = "terminating".into();
        // Requests not yet sent are dropped; those in flight are waited for.
        self.drop_requests(a);
        self.mark(a);
    }

    /// Forgets the requests of the app's sync that were not sent yet.
    fn drop_requests(&mut self, a: AppIx) {
        let Some(ids) = self.app_mut(a).and_then(|x| x.op.as_mut()).map(|o| std::mem::take(&mut o.requests)) else { return };
        let mut kept = Vec::new();
        for id in ids {
            match self.requests.get(&id) {
                Some(r) if r.action.is_some() => {
                    self.requests.remove(&id);
                }
                Some(_) => kept.push(id),
                None => {}
            }
        }
        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
            op.requests = kept;
        }
    }

    /// Records a request of the app's sync.
    fn own_request(&mut self, a: AppIx, id: u64) {
        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
            op.requests.push(id);
        }
    }

    // ---------------------------------------------------------------- requests

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn cluster_name(&self, c: ClusterIx) -> Str {
        self.clusters.get(c as usize).map(|cl| cl.name.clone()).unwrap_or_else(|| "".into())
    }

    fn enqueue(&mut self, c: ClusterIx, id: u64, trigger: Trigger) {
        if let Some(q) = self.clusters.get_mut(c as usize).and_then(|cl| cl.queue.get_mut(trigger as usize)) {
            q.push_back(id);
        }
    }

    /// Sends what the cluster's budget allows, highest priority first.
    fn pump(&mut self, c: ClusterIx) {
        let now = self.now;
        let Some(cl) = self.clusters.get_mut(c as usize) else { return };
        if !cl.reachable {
            return;
        }
        cl.bucket.refill(now);
        loop {
            if cl.inflight >= cl.max_inflight {
                break;
            }
            if cl.bucket.tokens < 1.0 {
                let at = cl.bucket.next_ms();
                if cl.budget_timer.is_none_or(|t| t > at) {
                    cl.budget_timer = Some(at);
                    self.timers.push(Reverse((at, Timer::Budget(c))));
                }
                break;
            }
            let Some(id) = cl.queue.iter_mut().find_map(VecDeque::pop_front) else { break };
            let Some(req) = self.requests.get_mut(&id) else { continue };
            let Some(action) = req.action.take() else { continue };
            cl.bucket.tokens -= 1.0;
            cl.inflight += 1;
            match &action {
                Action::Apply { .. } => self.stats.applies += 1,
                Action::Delete { .. } => self.stats.deletes += 1,
                _ => {}
            }
            self.out.push_back(action);
        }
    }

    // ---------------------------------------------------------------- timers

    fn timer(&mut self, at: u64, t: Timer) {
        self.timers.push(Reverse((at, t)));
    }

    fn fire_timers(&mut self) {
        while let Some(Reverse((at, t))) = self.timers.peek().copied() {
            if at > self.now {
                break;
            }
            self.timers.pop();
            match t {
                Timer::Wave(a, op) => {
                    if self.app(a).and_then(|x| x.op.as_ref()).is_some_and(|o| o.id == op) {
                        self.mark(a);
                    }
                }
                Timer::Retry(a) => {
                    let now = self.now;
                    let due = self.app_mut(a).and_then(|x| {
                        if x.op.is_none() && x.retry.as_ref().is_some_and(|r| r.at_ms <= now) { x.retry.take() } else { None }
                    });
                    if let Some(r) = due {
                        self.start_op(a, r.trigger, r.request);
                        if let Some(op) = self.app_mut(a).and_then(|x| x.op.as_mut()) {
                            op.attempt = r.attempt;
                        }
                    }
                }
                Timer::Heal(a) => self.mark(a),
                Timer::Budget(c) => {
                    let current = self.clusters.get(c as usize).and_then(|cl| cl.budget_timer) == Some(at);
                    if current {
                        if let Some(cl) = self.clusters.get_mut(c as usize) {
                            cl.budget_timer = None;
                        }
                        self.pump(c);
                    }
                }
            }
        }
    }
}

fn op_status(op: &Op) -> OpStatus {
    OpStatus { id: op.id, phase: op.phase, trigger: op.trigger, attempt: op.attempt, message: op.message.clone() }
}

/// What a resource last added to its app's counts.
fn res_counted_set(res: &mut Res, v: Option<(bool, Option<Health>, bool)>) {
    res.counted = v;
}

fn res_counted_take(res: &mut Res) -> Option<(bool, Option<Health>, bool)> {
    res.counted.take()
}

/// The live object, as kept: what [`LiveView`] split off goes back in, so a later comparison
/// can split it again.
fn recombine(view: LiveView) -> Value {
    let mut v = view.object;
    if !view.managed.is_empty() {
        let entries: Vec<Value> = view
            .managed
            .into_iter()
            .map(|m| {
                let mut e = Map::new();
                e.insert("manager".into(), Value::String(m.manager));
                e.insert("operation".into(), Value::String(m.operation));
                if !m.subresource.is_empty() {
                    e.insert("subresource".into(), Value::String(m.subresource));
                }
                e.insert("fieldsV1".into(), fieldset_json(&m.fields));
                Value::Object(e)
            })
            .collect();
        if let Some(meta) = obj::ensure_map(&mut v, &["metadata"]) {
            meta.insert("managedFields".into(), Value::Array(entries));
        }
    }
    v
}

fn fieldset_json(f: &diff::FieldSet) -> Value {
    let mut m = Map::new();
    if f.own && !f.members.is_empty() {
        m.insert(".".into(), Value::Object(Map::new()));
    }
    for (e, child) in &f.members {
        let k = match e {
            diff::PathElem::Field(name) => format!("f:{name}"),
            diff::PathElem::Key(keys) => format!("k:{}", Value::Object(keys.clone())),
            diff::PathElem::Value(v) => format!("v:{v}"),
            diff::PathElem::Index(i) => format!("i:{i}"),
        };
        m.insert(k, fieldset_json(child));
    }
    Value::Object(m)
}
