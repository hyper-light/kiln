//! The scheduler (docs/ci.md W05-R01 to W05-R13, R19 to R24): an event-driven state machine.
//! [`Scheduler::handle`] takes one [`Input`] and the time and returns the [`Command`]s that follow;
//! it never polls and never reads storage. The dispatcher (`crates/kiln/src/ci/dispatch.rs`)
//! turns hub and store events into inputs and carries out the commands.
//!
//! Legs with the same requirement share a **signature**, with its eligible runners and one queue
//! ordered by effective priority (aged: `priority − aging × minutes queued`, which keeps its order
//! as time passes), then fair-share virtual start time across repositories, then arrival.
//! Capacity that frees looks only at the signatures that runner can serve.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use super::groups::{GroupAction, GroupConfig, Groups};
use super::pools::{self, ExecState, Executor, Pool, PoolView};
use super::reserve::Reservation;
use super::retry::{self, Failed, FailureClass, RetryPolicy, RunnerChoice};
use super::routing::{self, Check, LegRoute, Preference, Requirement, Rule, RunnerView};

/// What a leg asks for and a runner offers, in resource mode (W05-R03).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Resources {
    pub cpu: f64,
    pub mem: u64,
    pub disk: u64,
    pub gpu: u32,
}

/// The scheduler's settings (`[ci.scheduler]`, `[ci.fairness]`, routing rules).
#[derive(Debug, Clone)]
pub struct Config {
    /// Effective priority gained per minute of waiting (W05-R09).
    pub aging_per_minute: f64,
    /// Fair-share weights by repository (default 1, W05-R10).
    pub weights: HashMap<String, f64>,
    /// Seconds a leg is expected to take when its history doesn't say.
    pub default_cost: f64,
    /// Resource mode instead of slots (W05-R03).
    pub resources: bool,
    pub overcommit_cpu: f64,
    pub overcommit_mem: f64,
    pub overcommit_disk: f64,
    /// Routing rules, in order (W05-R21).
    pub rules: Vec<Rule>,
    /// Legs in progress at most, per repository (W05-R11).
    pub quotas: HashMap<String, usize>,
    /// Pools, in order: a leg goes to the first that serves its labels (W13-R09).
    pub pools: Vec<Pool>,
    /// Runners and pools kept for matching legs in windows (W05-R12).
    pub reservations: Vec<Reservation>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            aging_per_minute: 1.0,
            weights: HashMap::new(),
            default_cost: 60.0,
            resources: false,
            overcommit_cpu: 1.5,
            overcommit_mem: 1.0,
            overcommit_disk: 1.0,
            rules: vec![routing::native_rule()],
            quotas: HashMap::new(),
            pools: Vec::new(),
            reservations: Vec::new(),
        }
    }
}

/// A leg to schedule.
#[derive(Debug, Clone, Default)]
pub struct LegIn {
    pub id: String,
    pub run: String,
    pub repo: String,
    pub route: LegRoute,
    /// 0–100 (W05-R09).
    pub priority: u8,
    pub exclusive: bool,
    pub request: Resources,
    /// The job-level concurrency group: key (scope and name), name, behaviour.
    pub group: Option<(String, String, GroupConfig)>,
    pub retry: Option<RetryPolicy>,
    /// Expected runner-seconds (history, W19).
    pub cost: Option<f64>,
    pub prefer: Vec<Preference>,
    /// It has a `container:` or `services:`: on a hosted pool (`k8s`), it runs in an executor
    /// made for it (a pod of that container, those services beside it, W13-R07).
    pub own_executor: bool,
}

/// A runner that can take legs.
#[derive(Debug, Clone, Default)]
pub struct RunnerIn {
    pub id: String,
    pub view: RunnerView,
    pub slots: u32,
    /// What it offers in resource mode; slots are used when `None`.
    pub capacity: Option<Resources>,
    /// The hosted executor it is (a `k8s` pool's pod, W13-R07): it takes its pool's legs and no
    /// other's; `Some("")` when the server doesn't know whose it is (one left from before a
    /// restart), which takes none.
    pub executor: Option<String>,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed(Failed),
    Cancelled,
}

/// What `supersede:` does to older runs (W05-R08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supersede {
    Cancel,
    Deprioritize,
}

/// Something that happened.
#[derive(Debug, Clone)]
pub enum Input {
    /// A run is about to start: its workflow-level group may hold it, and `supersede` may stop
    /// older runs with the same key (workflow and branch, stack or pull request).
    RunQueued { run: String, group: Option<(String, String, GroupConfig)>, supersede: Option<(String, Supersede)> },
    RunFinished { run: String },
    LegQueued(Box<LegIn>),
    /// After a restart: a leg the runner still runs (W05-R25).
    LegRunning { leg: Box<LegIn>, runner: String, attempt: u32 },
    /// The plan cancelled a leg (fail-fast, the run cancelled).
    LegCancelled { leg: String, reason: String },
    AttemptFinished { leg: String, attempt: u32, outcome: Outcome },
    /// The runner refused the assignment (no free slot, draining): the leg goes back to its queue,
    /// the same attempt, and not to that runner.
    Rejected { leg: String },
    RunnerUp(Box<RunnerIn>),
    RunnerChanged(Box<RunnerIn>),
    RunnerLost { runner: String },
    RunnerDraining { runner: String },
    RulesChanged(Vec<Rule>),
    /// A pool's executor its host started is ready for a leg (W13-R09).
    ExecutorReady { executor: String },
    /// Its host couldn't start it, or it's gone.
    ExecutorFailed { executor: String, error: String },
    /// Pools and reservations, read again (the Pools page, W13-R09, W05-R12).
    PoolsChanged { pools: Vec<Pool>, reservations: Vec<Reservation> },
    /// The time a [`Command::Timer`] asked for.
    Tick,
}

/// Why a leg waits (W05-R13): exactly one, the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum WaitReason {
    Capacity { eligible: usize, busy: usize, draining: usize },
    NoRunner { offline: Vec<String> },
    Unroutable { failing: Vec<String> },
    Capability { runners: Vec<String> },
    Concurrency { group: String, holders: Vec<String> },
    Quota { limit: usize },
    Exclusive,
    Retry { at: i64 },
    /// Its pool is starting executors for it (W13-R09): how many, since when (unix ms).
    Scaling { pool: String, starting: usize, since: i64 },
    /// The runners or pool that could take it are kept for other legs until `until` (W05-R12).
    Reservation { reservation: String, until: i64 },
}

/// What to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    StartRun { run: String },
    CancelRun { run: String, reason: String },
    /// `image` is the routing rule's, for container and microVM executors (W13); `docker`, the
    /// Docker access the rules give the leg (W13-R04); `slot`, the pool's executor it runs in
    /// (W13-R09).
    Assign { leg: String, runner: String, attempt: u32, executor: Option<String>, image: Option<String>, docker: Option<String>, slot: Option<String> },
    /// Start one of a pool's executors on `runner`, ready for a leg (W13-R09).
    /// `leg`: the one it's made for (a `k8s` pod of that leg's containers, W13-R07).
    StartExecutor {
        pool: String,
        executor: String,
        runner: String,
        kind: String,
        image: Option<String>,
        leg: Option<String>,
    },
    /// Stop an idle executor (past its pool's `idle_timeout`, or its pool gone).
    StopExecutor { executor: String, runner: String },
    /// Stop a leg's attempt on its runner.
    Cancel { leg: String, attempt: u32, reason: String },
    /// A failed attempt will be retried at `at` as `attempt`.
    Retry { leg: String, attempt: u32, at: i64 },
    Wait { leg: String, reason: WaitReason },
    /// The leg is done for good: the plan hears of it now.
    Settled { leg: String, outcome: Outcome, reason: Option<String> },
    Timer { at: i64 },
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Waiting in its concurrency group.
    Pending,
    Queued,
    Retrying { at: i64 },
    Assigned { runner: String },
    Done,
}

#[derive(Debug, Clone)]
struct Leg {
    input: LegIn,
    req: Requirement,
    sig: String,
    state: State,
    attempt: u32,
    /// When it was (last) queued, for aging.
    queued_at: i64,
    key: Option<(i64, u64, u64)>,
    reason: Option<WaitReason>,
    avoid: Option<String>,
    pin: Option<String>,
    last_runner: Option<String>,
    /// Cancelled while it ran, and why: however its attempt ends, it ends cancelled, never retried.
    cancelling: Option<String>,
}

#[derive(Debug, Clone)]
struct Runner {
    input: RunnerIn,
    running: Vec<String>,
    used: Resources,
    connected: bool,
    draining: bool,
    exclusive: bool,
}

#[derive(Debug, Clone, Default)]
struct Sig {
    req: Requirement,
    eligible: BTreeSet<String>,
    queue: BTreeSet<(i64, u64, u64, String)>,
}

/// The scheduler.
#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    cfg: Config,
    runners: BTreeMap<String, Runner>,
    legs: HashMap<String, Leg>,
    sigs: HashMap<String, Sig>,
    groups: Groups,
    /// Runs by supersede key, oldest first.
    superseding: HashMap<String, Vec<String>>,
    run_groups: HashMap<String, String>,
    finish: HashMap<String, f64>,
    vtime: f64,
    seq: u64,
    timers: BTreeMap<i64, Vec<String>>,
    in_progress: HashMap<String, usize>,
    /// Pools' executors, by id.
    executors: BTreeMap<String, Executor>,
    /// (pool, runner) left alone until, and the wait after the next failure.
    backoff: HashMap<(String, String), (i64, i64)>,
    /// Each pool's last failure to start an executor: (runner, error).
    pool_errors: HashMap<String, (String, String)>,
    /// The reservations' windows open now (index, end), and when they're next looked at (W05-R12).
    windows: Vec<(usize, i64)>,
    windows_at: Option<i64>,
}

impl Scheduler {
    pub fn new(cfg: Config) -> Scheduler {
        Scheduler { cfg, ..Scheduler::default() }
    }

    fn config(&self) -> &Config {
        &self.cfg
    }

    /// Handles one input at `now` (unix milliseconds).
    pub fn handle(&mut self, input: Input, now: i64) -> Vec<Command> {
        let mut out = Vec::new();
        // Reservations' windows that opened or closed since: what may run where changes.
        if !self.cfg.reservations.is_empty() && self.windows_at.is_none_or(|at| now >= at) {
            self.refresh_windows(now, &mut out);
        }
        match input {
            Input::RunQueued { run, group, supersede } => self.run_queued(run, group, supersede, now, &mut out),
            Input::RunFinished { run } => {
                for v in self.superseding.values_mut() {
                    v.retain(|r| *r != run);
                }
                if let Some(key) = self.run_groups.remove(&run) {
                    for a in self.groups.leave(&key, &format!("run:{run}")) {
                        self.group_action(a, now, &mut out);
                    }
                }
            }
            Input::LegQueued(leg) => self.leg_queued(*leg, now, &mut out),
            Input::LegRunning { leg, runner, attempt } => self.adopt(*leg, runner, attempt),
            Input::LegCancelled { leg, reason } => self.cancel(&leg, reason, now, &mut out),
            Input::AttemptFinished { leg, attempt, outcome } => self.finished(&leg, attempt, outcome, now, &mut out),
            Input::Rejected { leg } => {
                if let Some(runner) = self.release(&leg) {
                    self.discard_used(&leg, &mut out);
                    if let Some(l) = self.legs.get_mut(&leg) {
                        l.avoid = Some(runner.clone());
                    }
                    self.enqueue(&leg, now, &mut out);
                    self.try_runner(&runner, now, &mut out);
                }
            }
            Input::RunnerUp(r) | Input::RunnerChanged(r) => self.runner_up(*r, now, &mut out),
            Input::RunnerLost { runner } => self.runner_lost(&runner, now, &mut out),
            Input::RunnerDraining { runner } => {
                if let Some(r) = self.runners.get_mut(&runner) {
                    r.draining = true;
                }
            }
            Input::RulesChanged(rules) => self.rules_changed(rules, now, &mut out),
            Input::ExecutorReady { executor } => self.executor_ready(&executor, now, &mut out),
            Input::ExecutorFailed { executor, error } => self.executor_failed(&executor, error, now, &mut out),
            Input::PoolsChanged { pools, reservations } => {
                self.cfg.pools = pools;
                self.cfg.reservations = reservations;
                self.windows_at = None;
                self.refresh_windows(now, &mut out);
                // A pool gone: its idle and starting executors stop; busy ones end with their legs.
                let gone: Vec<Executor> = self.executors.values().filter(|e| !matches!(e.state, ExecState::Busy { .. }) && !self.cfg.pools.iter().any(|p| p.name == e.pool)).cloned().collect();
                for e in gone {
                    self.drop_executor(&e.id);
                    out.push(Command::StopExecutor { executor: e.id, runner: e.runner });
                }
                let rules = self.cfg.rules.clone();
                self.rules_changed(rules, now, &mut out);
                self.scale_all(now, &mut out);
            }
            Input::Tick => self.tick(now, &mut out),
        }
        out
    }

    fn run_queued(&mut self, run: String, group: Option<(String, String, GroupConfig)>, supersede: Option<(String, Supersede)>, now: i64, out: &mut Vec<Command>) {
        if let Some((key, action)) = supersede {
            let older = self.superseding.entry(key).or_default();
            let olds: Vec<String> = std::mem::take(older);
            older.push(run.clone());
            for o in olds {
                match action {
                    Supersede::Cancel => out.push(Command::CancelRun { run: o, reason: "superseded by a newer run".into() }),
                    Supersede::Deprioritize => self.deprioritize(&o),
                }
            }
        }
        match group {
            Some((key, name, cfg)) => {
                self.run_groups.insert(run.clone(), key.clone());
                for a in self.groups.arrive(&key, &name, &format!("run:{run}"), &cfg) {
                    self.group_action(a, now, out);
                }
            }
            None => out.push(Command::StartRun { run }),
        }
    }

    /// The older run's queued legs drop to the lowest priority.
    fn deprioritize(&mut self, run: &str) {
        let ids: Vec<String> = self.legs.iter().filter(|(_, l)| l.input.run == run && l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in ids {
            self.unqueue(&id);
            let fair = self.legs.get_mut(&id).map_or(0, |l| {
                l.input.priority = 0;
                l.key.map_or(0, |k| k.1)
            });
            self.set_key(&id, fair);
            self.insert_queue(&id);
        }
    }

    fn group_action(&mut self, a: GroupAction, now: i64, out: &mut Vec<Command>) {
        match a {
            GroupAction::Start(m) => {
                if let Some(run) = m.strip_prefix("run:") {
                    out.push(Command::StartRun { run: run.to_owned() });
                } else if let Some(leg) = m.strip_prefix("leg:") {
                    self.enqueue(leg, now, out);
                }
            }
            GroupAction::Cancel { member, running, reason } => {
                if let Some(run) = member.strip_prefix("run:") {
                    out.push(Command::CancelRun { run: run.to_owned(), reason });
                } else if let Some(leg) = member.strip_prefix("leg:") {
                    let leg = leg.to_owned();
                    if running {
                        if let Some(l) = self.legs.get(&leg)
                            && matches!(l.state, State::Assigned { .. })
                        {
                            out.push(Command::Cancel { leg: leg.clone(), attempt: l.attempt, reason });
                        }
                    } else {
                        self.unqueue(&leg);
                        let _ = self.legs.remove(&leg);
                        out.push(Command::Settled { leg, outcome: Outcome::Cancelled, reason: Some(reason) });
                    }
                }
            }
        }
    }

    /// The pool that runs a leg of `route` (W13-R09): the first that serves its labels, and for an
    /// untrusted leg one whose executors may run untrusted code (W18-R07).
    pub fn pool_for(&self, route: &LegRoute) -> Option<&Pool> {
        let untrusted_ok = |p: &Pool| route.untrusted.as_ref().is_none_or(|u| u.executors.contains(&p.executor));
        self.config().pools.iter().find(|p| p.serves(&route.labels) && untrusted_ok(p))
    }

    fn requirement(&self, input: &LegIn) -> Requirement {
        // A pool that serves the leg's labels runs it, on the hosts the pool names.
        if let Some(p) = self.pool_for(&input.route) {
            let mut constraints = p.hosts.clone();
            for c in &input.route.requires {
                match routing::Constraint::parse(c) {
                    Ok(x) => constraints.push((c.clone(), x)),
                    Err(e) => constraints.push((e, routing::Constraint::Or(Vec::new()))),
                }
            }
            return Requirement {
                constraints,
                caps: input.route.caps | p.caps,
                executors: vec![p.executor.clone()],
                image: p.image.clone(),
                pool: Some(p.name.clone()),
                rootless_docker: input.route.untrusted.as_ref().is_some_and(|u| !u.rootful_docker),
                ..Requirement::default()
            };
        }
        routing::resolve(&input.route, &self.config().rules).unwrap_or_else(|e| {
            // A constraint that doesn't parse: no runner can satisfy it, and the leg says why.
            Requirement { labels: input.route.labels.clone(), constraints: vec![(e, routing::Constraint::Or(Vec::new()))], caps: input.route.caps, ..Requirement::default() }
        })
    }

    fn leg_queued(&mut self, mut input: LegIn, now: i64, out: &mut Vec<Command>) {
        let id = input.id.clone();
        if self.legs.get(&id).is_some_and(|l| l.state != State::Done) {
            return;
        }
        let mut req = self.requirement(&input);
        // A pool's leg takes what its executor holds already.
        if req.pool.is_some() {
            input.request = Resources::default();
        }
        req.prefer.extend(input.prefer.iter().cloned());
        let sig = req.signature();
        self.ensure_sig(&sig, &req);
        let group = input.group.clone();
        self.legs.insert(id.clone(), Leg { input, req, sig, state: State::Pending, attempt: 1, queued_at: now, key: None, reason: None, avoid: None, pin: None, last_runner: None, cancelling: None });
        match group {
            Some((key, name, cfg)) => {
                let actions = self.groups.arrive(&key, &name, &format!("leg:{id}"), &cfg);
                let started = actions.iter().any(|a| *a == GroupAction::Start(format!("leg:{id}")));
                for a in actions {
                    self.group_action(a, now, out);
                }
                if !started && self.legs.get(&id).is_some_and(|l| l.state == State::Pending) {
                    let holders = self.groups.holders(&key).into_iter().map(|h| h.trim_start_matches("leg:").to_owned()).collect();
                    self.set_reason(&id, WaitReason::Concurrency { group: name, holders }, out);
                }
            }
            None => self.enqueue(&id, now, out),
        }
    }

    fn adopt(&mut self, input: LegIn, runner: String, attempt: u32) {
        let id = input.id.clone();
        let req = self.requirement(&input);
        let sig = req.signature();
        self.ensure_sig(&sig, &req);
        if let Some((key, name, cfg)) = &input.group {
            let _ = self.groups.arrive(key, name, &format!("leg:{id}"), cfg);
        }
        *self.in_progress.entry(input.repo.clone()).or_default() += 1;
        let request = input.request;
        self.legs.insert(id.clone(), Leg { input, req, sig, state: State::Assigned { runner: runner.clone() }, attempt, queued_at: 0, key: None, reason: None, avoid: None, pin: None, last_runner: Some(runner.clone()), cancelling: None });
        // The runner may not have reconnected yet: keep what it runs until it does.
        let r = self.runners.entry(runner.clone()).or_insert_with(|| Runner { input: RunnerIn { id: runner.clone(), ..RunnerIn::default() }, running: Vec::new(), used: Resources::default(), connected: false, draining: false, exclusive: false });
        r.running.push(id);
        add(&mut r.used, &request);
    }

    fn ensure_sig(&mut self, sig: &str, req: &Requirement) {
        if self.sigs.contains_key(sig) {
            return;
        }
        let eligible: BTreeSet<String> = self.runners.iter().filter(|(_, r)| self.eligible_for(req, r)).map(|(k, _)| k.clone()).collect();
        self.sigs.insert(sig.to_owned(), Sig { req: req.clone(), eligible, queue: BTreeSet::new() });
    }

    fn weight(&self, repo: &str) -> f64 {
        self.config().weights.get(repo).copied().filter(|w| *w > 0.0).unwrap_or(1.0)
    }

    /// Puts a leg in its signature's queue and tries the runners that could take it.
    fn enqueue(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(leg) = self.legs.get(id) else { return };
        if leg.state == State::Done {
            return;
        }
        let (repo, cost) = (leg.input.repo.clone(), leg.input.cost.unwrap_or(self.config().default_cost));
        // Start-time fair queuing across repositories (W05-R10).
        let start = self.vtime.max(self.finish.get(&repo).copied().unwrap_or(0.0));
        self.finish.insert(repo.clone(), start + cost / self.weight(&repo));
        if let Some(l) = self.legs.get_mut(id) {
            l.queued_at = now;
        }
        self.set_key(id, (start * 1000.0) as u64);
        self.insert_queue(id);
        // The runners with room, best first.
        let candidates = self.candidates(id);
        for r in candidates {
            if !self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
                break;
            }
            self.try_runner(&r, now, out);
        }
        if let Some(pool) = self.legs.get(id).filter(|l| l.state == State::Queued).and_then(|l| l.req.pool.clone()) {
            self.scale(&pool, now, out);
        }
        if self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
            let reason = self.wait_reason(id);
            self.set_reason(id, reason, out);
        }
    }

    /// The queue key: aged priority (higher first; `priority − aging × minutes since queued`, an
    /// order that holds as time passes), fair-share start, arrival.
    fn set_key(&mut self, id: &str, fair: u64) {
        let aging = self.config().aging_per_minute;
        self.seq += 1;
        let seq = self.seq;
        if let Some(l) = self.legs.get_mut(id) {
            let minutes = l.queued_at as f64 / 60_000.0;
            let bucket = (f64::from(l.input.priority) - aging * minutes).floor() as i64;
            l.key = Some((bucket.saturating_neg(), fair, seq));
            l.state = State::Queued;
        }
    }

    fn insert_queue(&mut self, id: &str) {
        let Some(l) = self.legs.get(id) else { return };
        let Some((a, b, c)) = l.key else { return };
        let sig = l.sig.clone();
        if let Some(s) = self.sigs.get_mut(&sig) {
            s.queue.insert((a, b, c, id.to_owned()));
        }
    }

    fn unqueue(&mut self, id: &str) {
        let Some(l) = self.legs.get(id) else { return };
        let Some((a, b, c)) = l.key else { return };
        let sig = l.sig.clone();
        if let Some(s) = self.sigs.get_mut(&sig) {
            s.queue.remove(&(a, b, c, id.to_owned()));
        }
    }

    /// Eligible runners with room for the leg, in the order to try them: by declared preference,
    /// else least loaded, then most free slots, then name (W05-R23).
    fn candidates(&self, id: &str) -> Vec<String> {
        let Some(l) = self.legs.get(id) else { return Vec::new() };
        let Some(s) = self.sigs.get(&l.sig) else { return Vec::new() };
        let rooms: Vec<&Runner> = s.eligible.iter().filter_map(|r| self.runners.get(r)).filter(|r| self.fits(r, l)).collect();
        let views: Vec<&RunnerView> = rooms.iter().map(|r| &r.input.view).collect();
        let scores = routing::scores(&l.req.prefer, &views);
        let mut order: Vec<(usize, &Runner)> = rooms.into_iter().enumerate().collect();
        order.sort_by(|(i, a), (j, b)| {
            let (sa, sb) = (scores.get(*i).copied().unwrap_or(0.0), scores.get(*j).copied().unwrap_or(0.0));
            sb.total_cmp(&sa).then_with(|| load(a).total_cmp(&load(b))).then_with(|| free(b).cmp(&free(a))).then_with(|| a.input.id.cmp(&b.input.id))
        });
        order.into_iter().map(|(_, r)| r.input.id.clone()).collect()
    }

    fn fits(&self, r: &Runner, l: &Leg) -> bool {
        if !r.connected || r.draining || r.exclusive {
            return false;
        }
        if self.reserved_against(r, l).is_some() {
            return false;
        }
        if l.avoid.as_deref() == Some(r.input.id.as_str()) || l.pin.as_deref().is_some_and(|p| p != r.input.id) {
            return false;
        }
        if l.input.exclusive && !r.running.is_empty() {
            return false;
        }
        if let Some(q) = self.config().quotas.get(&l.input.repo)
            && self.in_progress.get(&l.input.repo).copied().unwrap_or(0) >= *q
        {
            return false;
        }
        // A pool's leg runs in one of its idle executors on that host: one made for it when it
        // needs that (W13-R07).
        if let Some(pool) = &l.req.pool {
            let own = self.own_leg(l);
            return self.executors.values().any(|e| e.pool == *pool && e.runner == r.input.id && e.state == ExecState::Idle && e.leg.as_deref() == own);
        }
        let c = self.config();
        match (c.resources, r.input.capacity) {
            (true, Some(cap)) => {
                let q = &l.input.request;
                r.used.cpu + q.cpu <= cap.cpu * c.overcommit_cpu
                    && (r.used.mem + q.mem) as f64 <= cap.mem as f64 * c.overcommit_mem
                    && (r.used.disk + q.disk) as f64 <= cap.disk as f64 * c.overcommit_disk
                    && r.used.gpu + q.gpu <= cap.gpu
            }
            _ => self.own_legs(r) < r.input.slots.max(1) as usize,
        }
    }

    /// The legs a runner runs in its own slots (a pool's run in their executors).
    fn own_legs(&self, r: &Runner) -> usize {
        r.running.iter().filter(|x| !self.legs.get(*x).is_some_and(|l| l.req.pool.is_some())).count()
    }

    /// Gives a runner the best queued legs that fit it, until none does.
    fn try_runner(&mut self, runner: &str, now: i64, out: &mut Vec<Command>) {
        loop {
            let Some(r) = self.runners.get(runner) else { return };
            let mut best: Option<((i64, u64, u64), String)> = None;
            for s in self.sigs.values() {
                if !s.eligible.contains(runner) {
                    continue;
                }
                for (a, b, c, id) in &s.queue {
                    if best.as_ref().is_some_and(|(k, _)| *k <= (*a, *b, *c)) {
                        break;
                    }
                    if self.legs.get(id).is_some_and(|l| self.fits(r, l)) {
                        best = Some(((*a, *b, *c), id.clone()));
                        break;
                    }
                }
            }
            let Some((_, id)) = best else { return };
            self.assign(&id, runner, now, out);
        }
    }

    fn assign(&mut self, id: &str, runner: &str, now: i64, out: &mut Vec<Command>) {
        self.unqueue(id);
        let Some(l) = self.legs.get_mut(id) else { return };
        l.state = State::Assigned { runner: runner.to_owned() };
        l.reason = None;
        l.last_runner = Some(runner.to_owned());
        let (attempt, request, exclusive, repo, fair) = (l.attempt, l.input.request, l.input.exclusive, l.input.repo.clone(), l.key.map_or(0, |k| k.1));
        let req = l.req.clone();
        let (image, docker) = (req.image.clone(), req.docker.clone());
        self.vtime = self.vtime.max(fair as f64 / 1000.0);
        *self.in_progress.entry(repo).or_default() += 1;
        let mut executor = None;
        if let Some(r) = self.runners.get_mut(runner) {
            r.running.push(id.to_owned());
            add(&mut r.used, &request);
            r.exclusive = exclusive;
            executor = req.executors.iter().find(|e| super::routing::usable(&req, &r.input.view.facts, e)).cloned();
        }
        // Its pool's idle executor there, which it alone uses.
        let own = self.legs.get(id).and_then(|l| self.own_leg(l)).map(str::to_owned);
        let slot = req.pool.as_ref().and_then(|p| self.executors.values_mut().find(|e| e.pool == *p && e.runner == runner && e.state == ExecState::Idle && e.leg == own)).map(|e| {
            e.state = ExecState::Busy { leg: id.to_owned() };
            e.since = now;
            e.id.clone()
        });
        out.push(Command::Assign { leg: id.to_owned(), runner: runner.to_owned(), attempt, executor, image, docker, slot });
    }

    fn release(&mut self, id: &str) -> Option<String> {
        let l = self.legs.get(id)?;
        let State::Assigned { runner } = &l.state else { return None };
        let (runner, request, repo) = (runner.clone(), l.input.request, l.input.repo.clone());
        if let Some(n) = self.in_progress.get_mut(&repo) {
            *n = n.saturating_sub(1);
        }
        if let Some(r) = self.runners.get_mut(&runner) {
            r.running.retain(|x| x != id);
            sub(&mut r.used, &request);
            if r.running.is_empty() {
                r.exclusive = false;
            }
        }
        Some(runner)
    }

    /// The executor a leg ran in took its one leg: it's discarded. Its host removed it as the leg
    /// ended; one the leg never reached (refused, failed before it started) goes now.
    fn discard_used(&mut self, id: &str, out: &mut Vec<Command>) {
        // And one made for it that it never got to (cancelled while its pod started).
        let used: Vec<(String, String)> =
            self.executors.values().filter(|e| e.state == ExecState::Busy { leg: id.to_owned() } || e.leg.as_deref() == Some(id)).map(|e| (e.id.clone(), e.runner.clone())).collect();
        for (e, runner) in used {
            self.drop_executor(&e);
            out.push(Command::StopExecutor { executor: e, runner });
        }
    }

    fn finished(&mut self, id: &str, attempt: u32, outcome: Outcome, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        if l.attempt != attempt || l.state == State::Done {
            return;
        }
        let runner = self.release(id);
        self.discard_used(id, out);
        // Cancelled while it ran: a failure now (killed, its runner gone) is the cancel's doing.
        if let Some(why) = self.legs.get(id).and_then(|l| l.cancelling.clone()) {
            let (outcome, reason) = match outcome {
                Outcome::Failed(_) => (Outcome::Cancelled, Some(why)),
                o => (o, None),
            };
            self.settle(id, outcome, reason, now, out);
            if let Some(r) = runner {
                self.try_runner(&r, now, out);
            }
            self.scale_all(now, out);
            return;
        }
        let decision = match &outcome {
            Outcome::Failed(f) => retry::decide(self.legs.get(id).and_then(|l| l.input.retry.as_ref()), attempt, f),
            _ => None,
        };
        match decision {
            Some(d) => {
                let at = now + i64::try_from(d.delay_ms).unwrap_or(i64::MAX / 2);
                if let Some(l) = self.legs.get_mut(id) {
                    l.attempt += 1;
                    l.state = State::Retrying { at };
                    l.avoid = None;
                    l.pin = None;
                    match d.runner {
                        RunnerChoice::Different => l.avoid = runner.clone(),
                        RunnerChoice::Same => l.pin = runner.clone(),
                        RunnerChoice::Any | RunnerChoice::Default => {}
                    }
                    out.push(Command::Retry { leg: id.to_owned(), attempt: l.attempt, at });
                }
                if d.delay_ms == 0 {
                    self.enqueue(id, now, out);
                } else {
                    self.timers.entry(at).or_default().push(id.to_owned());
                    out.push(Command::Timer { at });
                    self.set_reason(id, WaitReason::Retry { at }, out);
                }
            }
            None => self.settle(id, outcome, None, now, out),
        }
        if let Some(r) = runner {
            self.try_runner(&r, now, out);
        }
        self.scale_all(now, out);
    }

    fn settle(&mut self, id: &str, outcome: Outcome, reason: Option<String>, now: i64, out: &mut Vec<Command>) {
        // An executor made for it goes with it (a pod still starting for a cancelled leg).
        self.discard_used(id, out);
        let group = self.legs.remove(id).and_then(|l| l.input.group);
        out.push(Command::Settled { leg: id.to_owned(), outcome, reason });
        if let Some((key, _, _)) = group {
            for a in self.groups.leave(&key, &format!("leg:{id}")) {
                self.group_action(a, now, out);
            }
        }
    }

    fn cancel(&mut self, id: &str, reason: String, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        match l.state.clone() {
            State::Assigned { .. } => {
                out.push(Command::Cancel { leg: id.to_owned(), attempt: l.attempt, reason: reason.clone() });
                if let Some(l) = self.legs.get_mut(id) {
                    l.cancelling = Some(reason);
                }
            }
            State::Queued => {
                self.unqueue(id);
                self.settle(id, Outcome::Cancelled, Some(reason), now, out);
            }
            State::Pending | State::Retrying { .. } => self.settle(id, Outcome::Cancelled, Some(reason), now, out),
            State::Done => {}
        }
    }

    fn runner_up(&mut self, r: RunnerIn, now: i64, out: &mut Vec<Command>) {
        let id = r.id.clone();
        // A hosted executor's own runner: the executor is ready, there (W13-R07). One the
        // scheduler doesn't know is told to go.
        if let Some(e) = r.executor.clone() {
            match self.executors.get_mut(&e) {
                Some(x) if x.state == ExecState::Starting => {
                    x.runner = id.clone();
                    x.state = ExecState::Idle;
                    x.since = now;
                    let pool = x.pool.clone();
                    self.backoff.remove(&(pool.clone(), String::new()));
                    self.idle_timer(&pool, now, out);
                }
                Some(_) => {}
                // One from before a restart, or stopped since: it goes. An ephemeral runner that
                // is no executor's (`""`, a v1 job's pod) is left alone, and takes nothing.
                None if !e.is_empty() => out.push(Command::StopExecutor { executor: e, runner: id.clone() }),
                None => {}
            }
        }
        let (running, used, exclusive) = self.runners.get(&id).map(|x| (x.running.clone(), x.used, x.exclusive)).unwrap_or_default();
        self.runners.insert(id.clone(), Runner { input: r, running, used, connected: true, draining: false, exclusive });
        self.refresh_eligibility(&id);
        self.try_runner(&id, now, out);
        self.scale_all(now, out);
        self.refresh_reasons(out);
    }

    fn refresh_eligibility(&mut self, runner: &str) {
        let Some(r) = self.runners.get(runner) else { return };
        let ok: Vec<(String, bool)> = self.sigs.iter().map(|(k, s)| (k.clone(), self.eligible_for(&s.req, r))).collect();
        for (k, yes) in ok {
            if let Some(s) = self.sigs.get_mut(&k) {
                if yes {
                    s.eligible.insert(runner.to_owned());
                } else {
                    s.eligible.remove(runner);
                }
            }
        }
    }

    /// Whether a runner may take legs of `req`: a hosted executor's own runner only its pool's
    /// (W13-R07), and no other runner those; the rest as routing decides.
    fn eligible_for(&self, req: &Requirement, r: &Runner) -> bool {
        if !r.connected {
            return false;
        }
        let hosted = req.pool.as_deref().and_then(|p| self.pool(p)).is_some_and(|p| p.hosted);
        match r.input.executor.as_deref() {
            Some(e) => hosted && self.executors.get(e).is_some_and(|x| Some(x.pool.as_str()) == req.pool.as_deref()),
            None => !hosted && routing::eligible(req, &r.input.view),
        }
    }

    fn runner_lost(&mut self, runner: &str, now: i64, out: &mut Vec<Command>) {
        let legs = match self.runners.get_mut(runner) {
            Some(r) => {
                r.connected = false;
                r.running.clone()
            }
            None => return,
        };
        self.refresh_eligibility(runner);
        // Its pools' executors went with it.
        let gone: Vec<String> = self.executors.values().filter(|e| e.runner == runner && !matches!(e.state, ExecState::Busy { .. })).map(|e| e.id.clone()).collect();
        for e in gone {
            self.drop_executor(&e);
        }
        for leg in legs {
            let attempt = self.legs.get(&leg).map_or(0, |l| l.attempt);
            self.finished(&leg, attempt, Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }), now, out);
        }
        self.scale_all(now, out);
        self.refresh_reasons(out);
    }

    fn rules_changed(&mut self, rules: Vec<Rule>, now: i64, out: &mut Vec<Command>) {
        self.cfg.rules = rules;
        let queued: Vec<String> = self.legs.iter().filter(|(_, l)| l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in &queued {
            self.unqueue(id);
        }
        let ids: Vec<String> = self.legs.keys().cloned().collect();
        for id in ids {
            let Some(input) = self.legs.get(&id).map(|l| l.input.clone()) else { continue };
            let mut req = self.requirement(&input);
            req.prefer.extend(input.prefer.iter().cloned());
            let sig = req.signature();
            self.ensure_sig(&sig, &req);
            if let Some(l) = self.legs.get_mut(&id) {
                l.req = req;
                l.sig = sig;
            }
        }
        for id in &queued {
            self.insert_queue(id);
        }
        let runners: Vec<String> = self.runners.keys().cloned().collect();
        for r in &runners {
            self.refresh_eligibility(r);
        }
        for r in runners {
            self.try_runner(&r, now, out);
        }
        self.refresh_reasons(out);
    }

    fn tick(&mut self, now: i64, out: &mut Vec<Command>) {
        let due: Vec<i64> = self.timers.range(..=now).map(|(k, _)| *k).collect();
        for at in due {
            for id in self.timers.remove(&at).unwrap_or_default() {
                if matches!(self.legs.get(&id).map(|l| &l.state), Some(State::Retrying { .. })) {
                    self.enqueue(&id, now, out);
                }
            }
        }
        self.reap_idle(now, out);
        self.scale_all(now, out);
    }

    // ------------------------------------------------------------------ reservations (W05-R12)

    /// Which reservation keeps `r` (or the leg's pool) from `l` now, and until when.
    fn reserved_against(&self, r: &Runner, l: &Leg) -> Option<(String, i64)> {
        self.windows.iter().find_map(|(i, end)| {
            let res = self.cfg.reservations.get(*i)?;
            let keeps = match &res.pool {
                Some(p) => l.req.pool.as_deref() == Some(p.as_str()),
                None => res.holds(&r.input.view),
            };
            (keeps && !res.admits(&l.input.route)).then(|| (res.name.clone(), *end))
        })
    }

    /// The windows open at `now`, and a timer for the next that opens or closes; then every
    /// runner looks again at what waits, as what may run where changed.
    fn refresh_windows(&mut self, now: i64, out: &mut Vec<Command>) {
        let before = self.windows.clone();
        self.windows = self.cfg.reservations.iter().enumerate().filter_map(|(i, r)| r.window_end(now).map(|end| (i, end))).collect();
        let next = self.windows.iter().map(|(_, end)| *end).chain(self.cfg.reservations.iter().filter_map(|r| r.next_start(now))).filter(|t| *t > now).min();
        // Looked at again within a week at the latest.
        let at = next.unwrap_or(now + 7 * 86_400_000);
        self.windows_at = Some(at);
        self.timers.entry(at).or_default();
        out.push(Command::Timer { at });
        if self.windows != before {
            let runners: Vec<String> = self.runners.keys().cloned().collect();
            for r in runners {
                self.try_runner(&r, now, out);
            }
            self.refresh_reasons(out);
        }
    }

    // ------------------------------------------------------------------ pools (W13-R09)

    fn pool(&self, name: &str) -> Option<&Pool> {
        self.cfg.pools.iter().find(|p| p.name == name)
    }

    fn drop_executor(&mut self, id: &str) {
        if let Some(e) = self.executors.remove(id)
            && let Some(r) = self.runners.get_mut(&e.runner)
        {
            sub(&mut r.used, &e.reserved);
        }
    }

    /// Legs queued for `pool`.
    fn waiting_for(&self, pool: &str) -> usize {
        self.sigs.values().filter(|s| s.req.pool.as_deref() == Some(pool)).map(|s| s.queue.len()).sum()
    }

    /// The hosts that may run `pool`'s executors now, with room for another: connected, not
    /// draining, eligible for its legs, under `per_host_max`, with the resources in resource
    /// mode, and not backing off; the least loaded with the pool first.
    fn hosts_for(&self, p: &Pool, now: i64) -> Vec<String> {
        let req = Requirement { constraints: p.hosts.clone(), caps: p.caps, executors: vec![p.executor.clone()], pool: Some(p.name.clone()), ..Requirement::default() };
        let mut hosts: Vec<(usize, String)> = self
            .runners
            .values()
            .filter(|r| r.connected && !r.draining && routing::eligible(&req, &r.input.view))
            .filter(|r| self.backoff.get(&(p.name.clone(), r.input.id.clone())).is_none_or(|(until, _)| *until <= now))
            .filter_map(|r| {
                let here = self.executors.values().filter(|e| e.pool == p.name && e.runner == r.input.id).count();
                if here >= p.per_host_max {
                    return None;
                }
                let c = self.config();
                if let (true, Some(cap)) = (c.resources, r.input.capacity) {
                    let q = &p.request;
                    let fits = r.used.cpu + q.cpu <= cap.cpu * c.overcommit_cpu
                        && (r.used.mem + q.mem) as f64 <= cap.mem as f64 * c.overcommit_mem
                        && (r.used.disk + q.disk) as f64 <= cap.disk as f64 * c.overcommit_disk
                        && r.used.gpu + q.gpu <= cap.gpu;
                    if !fits {
                        return None;
                    }
                }
                Some((here, r.input.id.clone()))
            })
            .collect();
        hosts.sort();
        hosts.into_iter().map(|(_, h)| h).collect()
    }

    /// Starts as many of `pool`'s executors as its waiting legs and `min_idle` want.
    fn scale(&mut self, pool: &str, now: i64, out: &mut Vec<Command>) {
        let Some(p) = self.pool(pool).cloned() else { return };
        let count = |s: fn(&ExecState) -> bool| self.executors.values().filter(|e| e.pool == pool && s(&e.state)).count();
        let (starting, idle, busy) = (count(|s| *s == ExecState::Starting), count(|s| *s == ExecState::Idle), count(|s| matches!(s, ExecState::Busy { .. })));
        let mut want = pools::wanted(&p, starting, idle, busy, self.waiting_for(pool));
        // The server's executors (a `k8s` pool's pods): started by it, each its own runner once
        // it connects; not while a failed start's backoff lasts.
        if p.hosted {
            if self.backoff.get(&(p.name.clone(), String::new())).is_some_and(|(until, _)| *until > now) {
                return;
            }
            // Each waiting leg that needs a pod of its own (W13-R07) gets one, within `max`; the
            // shared ones are for the others.
            let waiting: Vec<(String, bool)> = self
                .sigs
                .values()
                .filter(|s| s.req.pool.as_deref() == Some(pool))
                .flat_map(|s| s.queue.iter().map(|(_, _, _, id)| id.clone()))
                .filter_map(|id| self.legs.get(&id).map(|l| (id, l.input.own_executor)))
                .collect();
            let mut room = p.max.saturating_sub(self.executors.values().filter(|e| e.pool == pool).count());
            for (leg, _) in waiting.iter().filter(|(_, own)| *own) {
                if room == 0 {
                    break;
                }
                if self.executors.values().any(|e| e.leg.as_deref() == Some(leg.as_str())) {
                    continue;
                }
                room -= 1;
                self.start_hosted(&p, Some(leg.clone()), now, out);
            }
            let shared = |s: fn(&ExecState) -> bool| self.executors.values().filter(|e| e.pool == pool && e.leg.is_none() && s(&e.state)).count();
            let (starting, idle, busy) = (shared(|s| *s == ExecState::Starting), shared(|s| *s == ExecState::Idle), shared(|s| matches!(s, ExecState::Busy { .. })));
            let others = waiting.iter().filter(|(_, own)| !*own).count();
            for _ in 0..pools::wanted(&p, starting, idle, busy, others).min(room) {
                self.start_hosted(&p, None, now, out);
            }
            return;
        }
        while want > 0 {
            let Some(host) = self.hosts_for(&p, now).into_iter().next() else { break };
            self.seq += 1;
            let id = format!("pool-{pool}-{now:x}-{:x}", self.seq);
            self.executors.insert(id.clone(), Executor { id: id.clone(), pool: pool.to_owned(), runner: host.clone(), state: ExecState::Starting, since: now, reserved: p.request, leg: None });
            if let Some(r) = self.runners.get_mut(&host) {
                add(&mut r.used, &p.request);
            }
            out.push(Command::StartExecutor { pool: pool.to_owned(), executor: id, runner: host, kind: p.executor.clone(), image: p.image.clone(), leg: None });
            want -= 1;
        }
    }

    /// One of a hosted pool's executors started by the server: made for `leg`, or for any.
    fn start_hosted(&mut self, p: &Pool, leg: Option<String>, now: i64, out: &mut Vec<Command>) {
        self.seq += 1;
        let id = format!("pool-{}-{now:x}-{:x}", p.name, self.seq);
        self.executors
            .insert(id.clone(), Executor { id: id.clone(), pool: p.name.clone(), runner: String::new(), state: ExecState::Starting, since: now, reserved: Resources::default(), leg: leg.clone() });
        out.push(Command::StartExecutor { pool: p.name.clone(), executor: id, runner: String::new(), kind: p.executor.clone(), image: p.image.clone(), leg });
    }

    /// The leg's own id when it needs an executor made for it: it has containers, and its pool's
    /// executors are the server's (W13-R07).
    fn own_leg<'l>(&self, l: &'l Leg) -> Option<&'l str> {
        let hosted = l.req.pool.as_deref().and_then(|p| self.pool(p)).is_some_and(|p| p.hosted);
        (hosted && l.input.own_executor).then_some(l.input.id.as_str())
    }

    fn scale_all(&mut self, now: i64, out: &mut Vec<Command>) {
        let names: Vec<String> = self.cfg.pools.iter().map(|p| p.name.clone()).collect();
        for n in names {
            self.scale(&n, now, out);
        }
    }

    fn executor_ready(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(e) = self.executors.get_mut(id) else { return };
        if e.state != ExecState::Starting {
            return;
        }
        e.state = ExecState::Idle;
        e.since = now;
        let (pool, runner) = (e.pool.clone(), e.runner.clone());
        self.backoff.remove(&(pool.clone(), runner.clone()));
        self.try_runner(&runner, now, out);
        self.idle_timer(&pool, now, out);
        self.refresh_reasons(out);
    }

    fn executor_failed(&mut self, id: &str, error: String, now: i64, out: &mut Vec<Command>) {
        let Some(e) = self.executors.get(id).cloned() else { return };
        self.drop_executor(id);
        // Gone under its leg: the leg's runner reports how the attempt ended.
        if matches!(e.state, ExecState::Busy { .. }) {
            return;
        }
        // The host is left alone a while, longer each time it fails.
        let key = (e.pool.clone(), e.runner.clone());
        let delay = self.backoff.get(&key).map_or(pools::FIRST_BACKOFF_MS, |(_, d)| *d);
        self.backoff.insert(key, (now + delay, (delay * 2).min(pools::MAX_BACKOFF_MS)));
        self.pool_errors.insert(e.pool.clone(), (e.runner.clone(), error));
        self.timers.entry(now + delay).or_default();
        out.push(Command::Timer { at: now + delay });
        self.scale(&e.pool, now, out);
        self.refresh_reasons(out);
    }

    /// A timer for when `pool`'s oldest idle executor passes its `idle_timeout`.
    fn idle_timer(&mut self, pool: &str, now: i64, out: &mut Vec<Command>) {
        let Some(p) = self.pool(pool) else { return };
        let timeout = p.idle_timeout_ms;
        if let Some(at) = self.executors.values().filter(|e| e.pool == pool && e.state == ExecState::Idle).map(|e| e.since + timeout).min()
            && at > now
        {
            self.timers.entry(at).or_default();
            out.push(Command::Timer { at });
        }
    }

    /// Stops executors idle past their pool's `idle_timeout`, oldest first, while the pool keeps
    /// `min_idle` and nothing waits for it.
    fn reap_idle(&mut self, now: i64, out: &mut Vec<Command>) {
        let pools: Vec<Pool> = self.cfg.pools.clone();
        for p in pools {
            if self.waiting_for(&p.name) > 0 {
                continue;
            }
            let mut idle: Vec<(i64, String, String)> = self.executors.values().filter(|e| e.pool == p.name && e.state == ExecState::Idle).map(|e| (e.since, e.id.clone(), e.runner.clone())).collect();
            idle.sort();
            let spare = idle.len() + self.executors.values().filter(|e| e.pool == p.name && e.state == ExecState::Starting).count();
            let mut over = spare.saturating_sub(p.min_idle);
            for (since, id, runner) in idle {
                if over == 0 || now - since < p.idle_timeout_ms {
                    break;
                }
                self.drop_executor(&id);
                out.push(Command::StopExecutor { executor: id, runner });
                over -= 1;
            }
            self.idle_timer(&p.name, now, out);
        }
    }

    /// The pools and their executors now (the Pools page, `GET /api/ci/pools`).
    pub fn pools(&self) -> Vec<PoolView> {
        self.cfg
            .pools
            .iter()
            .map(|p| {
                let mine: Vec<Executor> = self.executors.values().filter(|e| e.pool == p.name).cloned().collect();
                let count = |f: fn(&ExecState) -> bool| mine.iter().filter(|e| f(&e.state)).count();
                PoolView {
                    name: p.name.clone(),
                    executor: p.executor.clone(),
                    image: p.image.clone(),
                    labels: p.labels.clone(),
                    hosts: p.hosts.first().map(|(s, _)| s.clone()),
                    min_idle: p.min_idle,
                    max: p.max,
                    per_host_max: p.per_host_max,
                    idle_timeout_ms: p.idle_timeout_ms,
                    starting: count(|s| *s == ExecState::Starting),
                    idle: count(|s| *s == ExecState::Idle),
                    busy: count(|s| matches!(s, ExecState::Busy { .. })),
                    waiting: self.waiting_for(&p.name),
                    last_error: self.pool_errors.get(&p.name).cloned(),
                    executors: mine,
                }
            })
            .collect()
    }

    fn set_reason(&mut self, id: &str, reason: WaitReason, out: &mut Vec<Command>) {
        if let Some(l) = self.legs.get_mut(id)
            && l.reason.as_ref() != Some(&reason)
        {
            l.reason = Some(reason.clone());
            out.push(Command::Wait { leg: id.to_owned(), reason });
        }
    }

    /// Re-decides every queued leg's reason after the fleet changed.
    fn refresh_reasons(&mut self, out: &mut Vec<Command>) {
        let ids: Vec<String> = self.legs.iter().filter(|(_, l)| l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in ids {
            let r = self.wait_reason(&id);
            self.set_reason(&id, r, out);
        }
    }

    /// Why a queued leg waits (W05-R13).
    fn wait_reason(&self, id: &str) -> WaitReason {
        let Some(l) = self.legs.get(id) else { return WaitReason::Unroutable { failing: Vec::new() } };
        if let Some(q) = self.config().quotas.get(&l.input.repo)
            && self.in_progress.get(&l.input.repo).copied().unwrap_or(0) >= *q
        {
            return WaitReason::Quota { limit: *q };
        }
        let eligible: Vec<&Runner> = self.sigs.get(&l.sig).map(|s| s.eligible.iter().filter_map(|r| self.runners.get(r)).collect()).unwrap_or_default();
        // Every runner that could take it is kept for other legs: until the first window ends.
        if !eligible.is_empty()
            && let Some((name, until)) = eligible.iter().map(|r| self.reserved_against(r, l)).collect::<Option<Vec<_>>>().and_then(|all| all.into_iter().min_by_key(|(_, u)| *u))
        {
            return WaitReason::Reservation { reservation: name, until };
        }
        if let (Some(pool), false) = (&l.req.pool, eligible.is_empty()) {
            let mine: Vec<&Executor> = self.executors.values().filter(|e| e.pool == *pool).collect();
            let starting: Vec<&&Executor> = mine.iter().filter(|e| e.state == ExecState::Starting).collect();
            if let Some(since) = starting.iter().map(|e| e.since).min() {
                return WaitReason::Scaling { pool: pool.clone(), starting: starting.len(), since };
            }
            // At its `max` (or no host has room): its executors are the capacity.
            let busy = mine.iter().filter(|e| matches!(e.state, ExecState::Busy { .. })).count();
            return WaitReason::Capacity { eligible: mine.len(), busy, draining: 0 };
        }
        if !eligible.is_empty() {
            if l.input.exclusive && eligible.iter().all(|r| !r.running.is_empty() || r.draining) {
                return WaitReason::Exclusive;
            }
            let draining = eligible.iter().filter(|r| r.draining).count();
            let busy = eligible.iter().filter(|r| !r.draining && !self.fits(r, l)).count();
            return WaitReason::Capacity { eligible: eligible.len(), busy, draining };
        }
        let connected: Vec<&Runner> = self.runners.values().filter(|r| r.connected).collect();
        let lacking: Vec<String> = connected.iter().filter(|r| routing::lacks_only_capability(&l.req, &r.input.view)).map(|r| r.input.view.name.clone()).collect();
        if !lacking.is_empty() {
            return WaitReason::Capability { runners: lacking };
        }
        let offline: Vec<String> = self.runners.values().filter(|r| !r.connected && routing::eligible(&l.req, &r.input.view)).map(|r| r.input.view.name.clone()).collect();
        if !offline.is_empty() {
            return WaitReason::NoRunner { offline };
        }
        let mut failing: Vec<String> = Vec::new();
        for r in self.runners.values() {
            for c in routing::checks(&l.req, &r.input.view) {
                if !c.pass && !failing.contains(&c.requirement) {
                    failing.push(c.requirement);
                }
            }
        }
        if failing.is_empty() {
            failing = l.req.constraints.iter().map(|(s, _)| s.clone()).chain(std::iter::once(format!("runs-on [{}]", l.req.labels.join(", ")))).collect();
        }
        WaitReason::Unroutable { failing }
    }

    /// Why each runner can or can't take a leg (W05-R20).
    pub fn explain(&self, id: &str) -> Vec<Explained> {
        let Some(l) = self.legs.get(id) else { return Vec::new() };
        let views: Vec<&RunnerView> = self.runners.values().map(|r| &r.input.view).collect();
        let scores = routing::scores(&l.req.prefer, &views);
        self.runners
            .values()
            .enumerate()
            .map(|(i, r)| {
                let checks = routing::checks(&l.req, &r.input.view);
                Explained {
                    runner: r.input.id.clone(),
                    name: r.input.view.name.clone(),
                    connected: r.connected,
                    eligible: checks.iter().all(|c| c.pass),
                    room: self.fits(r, l),
                    score: scores.get(i).copied(),
                    checks,
                }
            })
            .collect()
    }

    /// How each runner measures up against a leg that isn't queued (W04-R29's preview): the
    /// requirement routing makes of `route`, and each runner's checks.
    pub fn explain_route(&self, route: &LegRoute) -> (Requirement, Vec<Explained>) {
        let req = self.requirement(&LegIn { route: route.clone(), ..LegIn::default() });
        let views: Vec<&RunnerView> = self.runners.values().map(|r| &r.input.view).collect();
        let scores = routing::scores(&req.prefer, &views);
        let list = self
            .runners
            .values()
            .enumerate()
            .map(|(i, r)| {
                let checks = routing::checks(&req, &r.input.view);
                Explained {
                    runner: r.input.id.clone(),
                    name: r.input.view.name.clone(),
                    connected: r.connected,
                    eligible: checks.iter().all(|c| c.pass),
                    room: r.connected && !r.draining && r.running.len() < r.input.slots as usize,
                    score: scores.get(i).copied(),
                    checks,
                }
            })
            .collect();
        (req, list)
    }

    /// The current wait reason of a leg, if it waits.
    pub fn reason(&self, id: &str) -> Option<&WaitReason> {
        self.legs.get(id).and_then(|l| l.reason.as_ref())
    }

    /// Legs queued now (for metrics).
    pub fn queued(&self) -> usize {
        self.sigs.values().map(|s| s.queue.len()).sum()
    }
}

/// One runner in a routing explanation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explained {
    pub runner: String,
    pub name: String,
    pub connected: bool,
    pub eligible: bool,
    pub room: bool,
    pub score: Option<f64>,
    pub checks: Vec<Check>,
}

fn add(a: &mut Resources, b: &Resources) {
    a.cpu += b.cpu;
    a.mem = a.mem.saturating_add(b.mem);
    a.disk = a.disk.saturating_add(b.disk);
    a.gpu = a.gpu.saturating_add(b.gpu);
}

fn sub(a: &mut Resources, b: &Resources) {
    a.cpu = (a.cpu - b.cpu).max(0.0);
    a.mem = a.mem.saturating_sub(b.mem);
    a.disk = a.disk.saturating_sub(b.disk);
    a.gpu = a.gpu.saturating_sub(b.gpu);
}

fn load(r: &Runner) -> f64 {
    r.running.len() as f64 / f64::from(r.input.slots.max(1))
}

fn free(r: &Runner) -> usize {
    (r.input.slots.max(1) as usize).saturating_sub(r.running.len())
}
