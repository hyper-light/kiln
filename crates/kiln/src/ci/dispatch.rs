//! The CI engine's dispatcher (docs/ci.md W05 design): one task that owns the scheduler and every
//! live run's plan. It turns what happens (a run to start, a runner connecting, a leg's events and
//! end, a cancel) into the plan's and the scheduler's inputs, and carries out what they decide:
//! legs written to the store, `AssignV2` sent to runners, steps and annotations recorded, runs
//! finished. Plans are pure, so after a restart each unfinished run's plan is rebuilt by replaying
//! its log.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kiln_ci::plan::{self, Conclusion as PlanC, LegReport, LegSpec, PlanAction, PlanSource, RunInput, RunPlan, Settings};
use kiln_ci::sched::groups::GroupConfig;
use kiln_ci::sched::retry::{Failed, FailureClass, RetryOn, RetryPolicy};
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler};
use kiln_ci::trigger::Event;
use kiln_ci::wf::Workflow;
use kiln_ci::wire::{self, AssignmentV2, JobEventV2};
use parking_lot::Mutex;
use rusqlite::OptionalExtension as _;

use super::runs;
use crate::hub::Hub;
use crate::proto::{CAP_ACTIONS, CAP_CI2, CAP_DOCKER, JobResult, LANE_CONTROL, ServerMsg};
use crate::store::{Repo, Status};

/// A run to start: a workflow that matched an event.
#[derive(Debug, Clone)]
pub struct Start {
    pub repo: Repo,
    pub event: Event,
    pub path: String,
    pub workflow: Arc<Workflow>,
    /// The commit's message and author, for the run list.
    pub message: String,
    pub author: String,
    /// The run's remote actions, resolved when it started (W07-R02).
    pub actions: std::collections::BTreeMap<String, wire::ActionRef>,
    /// The `inputs` context, typed (`workflow_dispatch`, W03-R11); null takes the payload's.
    pub inputs: serde_json::Value,
    /// What `kiln workflow run` asked for besides (W17-R06).
    pub given: Given,
}

/// What a run of a working tree (`kiln workflow run`, W17-R06) brings besides its event.
#[derive(Debug, Clone, Default)]
pub struct Given {
    /// Only these matrix combinations (`--matrix k=v`).
    pub only_matrix: Vec<(String, String)>,
    /// Secrets for this run only (`--secret-file`). They stay in the server's memory, never on
    /// disk, while the run runs: legs after a restart, and reruns, go without them.
    pub secrets: Arc<std::collections::BTreeMap<String, String>>,
    /// Variables over the server's (`--var`), in `vars`.
    pub vars: Arc<std::collections::BTreeMap<String, String>>,
    /// Told the run's id once it's created.
    pub reply: Option<async_channel::Sender<String>>,
}

/// What the dispatcher hears.
#[derive(Debug)]
pub enum Msg {
    Start(Box<Start>),
    /// A runner connected (or said what it can do); `running` are the legs it still runs.
    RunnerUp { agent: String, running: Vec<String> },
    RunnerLost { agent: String },
    /// A runner's probes reported (W13-R01): it is routed by what they found.
    RunnerFacts { agent: String },
    Event { agent: String, leg: String, json: String },
    Finished { agent: String, result: JobResult },
    Rejected { agent: String, leg: String },
    Cancel { run: String, reason: String },
    /// Why a leg waits, and how each runner measures up against it (W05-R11).
    Explain { leg: String, reply: async_channel::Sender<serde_json::Value> },
    /// The plan `Start` would make, without a run (W04-R29).
    Preview { start: Box<Start>, reply: async_channel::Sender<serde_json::Value> },
    /// The stored routing rules changed (W05-R21): read them again.
    RoutingChanged,
    /// A run's deployments were reviewed (W15-R03): its legs held for approval are let go, or
    /// fail.
    Reviewed { run: String },
    /// Where a leg routed so would go now, and why each runner can or can't take it.
    ExplainRoute { route: Box<LegRoute>, reply: async_channel::Sender<serde_json::Value> },
    /// A maintainer decided an outside contributor's run (W18-R06): approved, it starts; rejected,
    /// it is cancelled. `false` when the run waits for no approval.
    ApproveRun { run: String, by: String, approve: bool, reply: async_channel::Sender<bool> },
    /// A new attempt of a finished run (W05-R17): its number, or why not.
    Rerun { run: String, mode: RerunMode, debug: bool, keep_failed: Option<u32>, actions: Option<std::collections::BTreeMap<String, wire::ActionRef>>, no_reuse: bool, by: String, reply: async_channel::Sender<Result<u32, String>> },
    /// A runner's word on a pool's executor it was asked to start (W13-R09).
    Executor { agent: String, json: String },
    /// The stored pools changed: read them again.
    PoolsChanged,
    /// The pools and their executors now.
    Pools { reply: async_channel::Sender<serde_json::Value> },
}

/// Where and how a leg runs, as the scheduler placed it.
struct Placement {
    executor: Option<String>,
    image: Option<String>,
    docker: Option<String>,
    /// The pool's executor it runs in (W13-R09).
    slot: Option<String>,
}

pub use kiln_ci::plan::rerun::Mode as RerunMode;

/// What a leg asks of routing (W05-R20).
/// Whether a `runs-on` label says what machine a leg needs: GitHub's hosted labels, and the OS and
/// architecture labels every self-hosted runner has.
fn machine_label(l: &str) -> bool {
    matches!(l.to_ascii_lowercase().as_str(), "self-hosted" | "linux" | "windows" | "macos" | "x64" | "arm64" | "arm") || kiln_ci::sched::routing::github_label_rules().iter().any(|r| r.matches.labels.iter().any(|x| x.eq_ignore_ascii_case(l)))
}

/// A leg's route. On `kiln workflow run`'s own server (`local`, W17-R06) a leg keeps only the labels
/// that say what machine it needs: the rest name the user's runners, and it runs on this one.
fn route_of(spec: &LegSpec, workflow: &str, repo: &str, event: &str, local: bool, untrusted: Option<kiln_ci::sched::routing::Untrusted>) -> LegRoute {
    let job = spec.def();
    let uses = job.is_some_and(|j| j.steps.iter().any(|s| s.uses.as_deref().is_some_and(|u| !u.starts_with("docker://"))));
    let docker = spec.container.is_some() || !spec.services.is_empty() || job.is_some_and(|j| j.steps.iter().any(|s| s.uses.as_deref().is_some_and(|u| u.starts_with("docker://"))));
    let caps = u64::from(CAP_CI2) | if uses { u64::from(CAP_ACTIONS) } else { 0 } | if docker { u64::from(CAP_DOCKER) } else { 0 };
    LegRoute {
        job: spec.job.clone(),
        job_name: spec.job_name.clone(),
        workflow: workflow.to_owned(),
        repo: repo.to_owned(),
        event: event.to_owned(),
        labels: if local { spec.runs_on.labels.iter().filter(|l| machine_label(l)).cloned().collect() } else { spec.runs_on.labels.clone() },
        group: if local { None } else { spec.runs_on.group.clone() },
        requires: spec.requires.clone(),
        caps,
        untrusted,
    }
}

/// The secrets kiln actions in `steps` declare (W09-R07): (the action, the declared name, the
/// job's secret it reads, whether it's required). A step's `with-secrets` maps a declared name
/// to another of the job's; else it reads its own name.
fn declared_sources(steps: &[kiln_ci::wf::ir::Step], actions: &std::collections::BTreeMap<String, wire::ActionRef>) -> Vec<(String, String, String, bool)> {
    let mut out = Vec::new();
    for step in steps {
        let Some((uses, a)) = step.uses.as_deref().and_then(|u| actions.get(u).map(|a| (u, a))) else { continue };
        for (name, required) in &a.secrets {
            let from = step.kiln.with_secrets.iter().find(|(d, _)| d.eq_ignore_ascii_case(name)).map_or_else(|| name.clone(), |(_, f)| f.to_ascii_uppercase());
            out.push((uses.to_owned(), name.clone(), from, *required));
        }
    }
    out
}

/// The actions in `steps` that couldn't be resolved when the run started (W07-R03), as GitHub
/// says so at "Set up job".
fn unresolved(steps: &[kiln_ci::wf::ir::Step], actions: &std::collections::BTreeMap<String, wire::ActionRef>) -> Option<String> {
    let lines: Vec<String> = steps
        .iter()
        .filter_map(|s| s.uses.as_deref())
        .filter_map(|u| actions.get(u).and_then(|a| a.error.as_deref()).map(|why| format!("Unable to resolve action `{u}`: {why}")))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The `log:` patterns of a leg's retry policy (W05-R14) that its attempt's log has, its steps'
/// logs read in turn.
async fn log_hits(hub: &Hub, leg: &str, patterns: &[String]) -> Vec<String> {
    let id = leg.to_owned();
    let steps: Vec<u32> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT idx FROM steps WHERE job_id = ?1 ORDER BY idx")?;
            let rows = st.query_map([&id], |r| r.get::<_, u32>(0))?;
            Ok(rows.filter_map(Result::ok).collect())
        })
        .await
        .unwrap_or_default();
    let mut hits: Vec<String> = Vec::new();
    for step in steps {
        let text = super::logsearch::plain(&hub.logs.read(leg, step).await);
        for p in patterns {
            if !hits.contains(p) && text.contains(p.as_str()) {
                hits.push(p.clone());
            }
        }
        if hits.len() == patterns.len() {
            break;
        }
    }
    hits
}

/// `KCI4031` (W07-R11): the scopes a kiln action in `steps` needs that `granted` gives less of.
fn short_permissions(steps: &[kiln_ci::wf::ir::Step], actions: &std::collections::BTreeMap<String, wire::ActionRef>, granted: &kiln_ci::plan::perms::Resolved) -> Option<String> {
    let mut lines = Vec::new();
    for step in steps {
        let Some((uses, need)) = step.uses.as_deref().and_then(|u| actions.get(u).and_then(wire::ActionRef::needs).map(|n| (u, n))) else { continue };
        for (scope, level) in &need.0 {
            if *level > granted.level(scope) {
                lines.push(format!("KCI4031: `{uses}` needs `{scope}: {}`, and the job grants `{}`. Give the job `permissions: {{ {scope}: {} }}`.", word(*level), word(granted.level(scope)), word(*level)));
            }
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn word(l: kiln_ci::plan::perms::Level) -> &'static str {
    match l {
        kiln_ci::plan::perms::Level::None => "none",
        kiln_ci::plan::perms::Level::Read => "read",
        kiln_ci::plan::perms::Level::Write => "write",
    }
}

/// `debug: { keep-on-failure: 30m }` (W17-R02): minutes (a bare number is minutes), at most a day.
fn keep_minutes(debug: Option<&kiln_ci::wf::ir::YVal>) -> Option<u32> {
    use kiln_ci::wf::ir::YVal;
    let minutes = match debug?.get("keep-on-failure")? {
        YVal::Num(n) if *n > 0.0 => n.ceil() as u64,
        YVal::Str(t) => kiln_ci::trigger::cron::seconds(&t.src)?.div_ceil(60),
        YVal::Bool(true) => 30,
        _ => return None,
    };
    Some(u32::try_from(minutes.min(u64::from(crate::runner::kept::MAX_KEEP_MINUTES))).unwrap_or(crate::runner::kept::MAX_KEEP_MINUTES))
}

/// What the scheduler gets of a leg: its route, priority, concurrency group and retries.
fn leg_in(r: &RunSt, run: &str, id: &str, spec: &LegSpec, cfg: &crate::config::ci::Ci) -> LegIn {
    let job = spec.def();
    // Untrusted code runs only where `ci.untrusted` lets it (W18-R07).
    let mut route = route_of(spec, &r.wf.name.clone().unwrap_or_default(), &r.repo.name, &r.event.name, cfg.local, (!r.trusted).then(|| cfg.untrusted.route()));
    // What kiln actions' steps need of the runner (W07-R11).
    for step in job.map(|j| j.steps.as_slice()).unwrap_or_default() {
        if let Some(a) = step.uses.as_deref().and_then(|u| r.actions.get(u)) {
            route.requires.extend(a.requires.iter().filter(|q| !route.requires.contains(q)).cloned().collect::<Vec<_>>());
        }
    }
    let group = spec.concurrency.as_ref().map(|g| (format!("{}\0{}", r.repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
    let retry = job.and_then(|j| j.kiln.retry.as_ref()).and_then(RetryPolicy::from_yval);
    LegIn {
        id: id.to_owned(),
        run: run.to_owned(),
        repo: r.repo.name.clone(),
        route,
        priority: priority(&r.event.name),
        exclusive: job.is_some_and(|j| j.kiln.exclusive),
        group,
        retry,
        cost: r.costs.get(&spec.job).copied(),
        ..LegIn::default()
    }
}

/// The scheduler's settings: `[ci.scheduler]`, `[ci.fairness]` and the routing rules.
fn sched_config(hub: &Hub, rules: Vec<kiln_ci::sched::routing::Rule>) -> Config {
    let (s, f) = (&hub.cfg.ci.scheduler, &hub.cfg.ci.fairness);
    Config { rules, aging_per_minute: s.aging_per_minute, default_cost: s.default_cost_seconds, weights: f.weights.clone(), quotas: f.quotas.clone(), ..Config::default() }
}

/// The hub's handle on the dispatcher.
pub struct Handle {
    tx: async_channel::Sender<Msg>,
    rx: Mutex<Option<async_channel::Receiver<Msg>>>,
    on: AtomicBool,
    /// Legs the dispatcher owns, so the hub hands their messages here and not to v1.
    legs: Arc<Mutex<HashSet<String>>>,
    runs: Arc<Mutex<HashSet<String>>>,
    /// Failed legs kept to look into: the runner keeping each, and until when (W17-R02).
    kept: Mutex<HashMap<String, (String, i64)>>,
}

impl Default for Handle {
    fn default() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Handle { tx, rx: Mutex::new(Some(rx)), on: AtomicBool::new(false), legs: Arc::default(), runs: Arc::default(), kept: Mutex::default() }
    }
}

impl Handle {
    /// Sends a message, unless the engine is off (then nothing listens).
    pub fn send(&self, m: Msg) {
        if self.on.load(Ordering::Relaxed) {
            let _ = self.tx.try_send(m);
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Whether `job` is one of the engine's legs.
    pub fn is_leg(&self, job: &str) -> bool {
        self.legs.lock().contains(job)
    }

    /// A failed leg kept on `agent` until `until` (W17-R02): its events and logs (a step rerun in
    /// place) are taken from that runner; `None` lets it go.
    pub fn keep(&self, job: &str, kept: Option<(String, i64)>) {
        let mut g = self.kept.lock();
        match kept {
            Some(k) => {
                g.insert(job.to_owned(), k);
            }
            None => {
                g.remove(job);
            }
        }
    }

    /// Whether `agent` keeps `job` now.
    pub fn kept_by(&self, job: &str, agent: &str) -> bool {
        self.kept.lock().get(job).is_some_and(|(a, until)| a == agent && *until > crate::util::now_unix())
    }

    /// Whether `run` is one of the engine's live runs.
    pub fn is_run(&self, run: &str) -> bool {
        self.runs.lock().contains(run)
    }
}

/// A run in progress.
struct RunSt {
    plan: RunPlan,
    repo: Repo,
    event: Event,
    /// The workflow's file, and when the run was created (for `workflow_run`).
    path: String,
    created: i64,
    /// Whether `workflow_run` `in_progress` went out.
    in_progress: bool,
    /// The reusable workflows it may call (W04-R18).
    files: Arc<HashMap<String, Arc<Workflow>>>,
    wf: Arc<Workflow>,
    github: serde_json::Value,
    vars: serde_json::Value,
    trusted: bool,
    actions: std::collections::BTreeMap<String, wire::ActionRef>,
    /// The plan's leg keys, to leg ids.
    keys: HashMap<String, String>,
    /// Actions waiting for the run's concurrency group to let it start.
    held: Option<Vec<PlanAction>>,
    seq: i64,
    /// A rerun's legs that keep the last attempt's result, by key: the report and that leg.
    keep: HashMap<String, (LegReport, String)>,
    /// A rerun with debug logging (`ACTIONS_STEP_DEBUG`, `ACTIONS_RUNNER_DEBUG`).
    debug: bool,
    /// "Re-run and keep if it fails" (W17-R02): minutes a failed leg is kept.
    keep_failed: Option<u32>,
    /// What `kiln workflow run` gave the run besides (W17-R06): secrets, in memory only, and
    /// variables.
    given: Given,
    /// The timing table its shards were balanced by (W16-R06), saved with its input.
    timing: Arc<HashMap<String, f64>>,
    /// The commit's files its shards and chunks split, by pattern, saved with its input too.
    tree: Arc<HashMap<String, Vec<String>>>,
    /// How many combinations its plan had removed when they were last saved (W04-R29).
    removed: usize,
    /// Each job's median seconds lately: what the scheduler expects its legs to cost (W05-R10).
    costs: Arc<HashMap<String, f64>>,
    /// An outside contributor's run waiting for a maintainer (W18-R06): the concurrency group
    /// it joins once approved.
    awaiting: Option<Awaiting>,
    /// "Run anyway" (W08-R10): its legs take no earlier result over.
    no_reuse: bool,
}

/// What an unapproved run is handed to the scheduler with, once it's approved.
struct Awaiting {
    group: Option<(String, String, GroupConfig)>,
}

/// A leg's egress policy (W18-R08), from its job's `network:` and its trust. One that doesn't
/// read lets the leg reach only kiln's server: closed, never open.
fn egress_of(network: Option<&kiln_ci::wf::ir::YVal>, trusted: bool, untrusted_allow: &[String], leg: &str) -> Option<wire::Network> {
    match kiln_ci::trust::egress(network, trusted, untrusted_allow) {
        Ok(allow) => allow.map(|allow| wire::Network { allow, untrusted: !trusted }),
        Err(e) => {
            tracing::warn!(leg, error = %e, "the job's network policy doesn't read; it reaches only kiln");
            Some(wire::Network { allow: Vec::new(), untrusted: !trusted })
        }
    }
}

/// Whether a run of `e` is trusted: the event's own say (a fork's pull request isn't, W03-R08),
/// unless an administrator marked the repository or the actor (W18-R05).
fn trusted_of(cfg: &crate::config::ci::Ci, repo: &Repo, e: &Event) -> bool {
    e.trusted && !cfg.untrusted.marks(&repo.name, &e.actor)
}

/// The author's standing for approvals (GitHub's `author_association`): the pull request's, or
/// kiln's for what it synthesized; a run through the API is a known account's.
fn association(e: &Event) -> Option<String> {
    let p = &e.payload;
    p.pointer("/pull_request/author_association")
        .or_else(|| p.pointer("/kiln/author_association"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| (e.source == kiln_ci::trigger::Source::Api).then(|| "CONTRIBUTOR".to_owned()))
}

/// A leg the scheduler has.
struct LegSt {
    run: String,
    key: String,
    spec: LegSpec,
    attempt: u32,
    agent: Option<String>,
    report: LegReport,
    class: Option<FailureClass>,
    /// Its deployment (W15-R04), once it started, and the `environment.url` it evaluated.
    deployment: Option<i64>,
    env_url: Option<String>,
    /// The last of the attempt's events taken (their `seq`): a runner sends them again after a
    /// reconnect, and those taken already are skipped.
    seq: u64,
}

/// An event's place in its attempt (`seq`, from runners that send events again after a reconnect).
#[derive(serde::Deserialize)]
struct Seq {
    seq: Option<u64>,
}

/// A leg its environment's protection holds back (W15-R03, R04): not yet the scheduler's.
struct Held {
    leg: LegIn,
    env: String,
    /// When it could first have started: the wait timer counts from here.
    ready_at: i64,
    /// Whether `approval.waiting` went out for it (W19-R06).
    announced: bool,
}

/// Reusable workflows: read at the run's commit from the repository's mirror (W04-R24; remote
/// ones arrive with W07's mirror).
struct Source<'a> {
    files: &'a HashMap<String, Arc<Workflow>>,
    /// Recorded durations by test file and test (W16-R06), for shards balanced by timing.
    timing: &'a HashMap<String, f64>,
    /// The commit's files that shards and chunks split (`files:`), by pattern.
    tree: &'a HashMap<String, Vec<String>>,
}

impl PlanSource for Source<'_> {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        let path = uses.trim_start_matches("./").trim_start_matches("$/");
        self.files.get(uses).or_else(|| self.files.get(path)).cloned().ok_or_else(|| format!("`{uses}`: no such workflow (or it couldn't be fetched when the run started)"))
    }

    fn durations(&self, _job: &str, items: &[String]) -> Vec<Option<f64>> {
        items.iter().map(|i| self.timing.get(i).or_else(|| self.timing.get(i.trim_start_matches("./"))).copied()).collect()
    }

    fn files(&self, pattern: &str) -> Vec<String> {
        self.tree.get(pattern).cloned().unwrap_or_default()
    }
}

/// How soon a leg held by its environment's promotion or custom gates is looked at again.
const GATE_AGAIN_SECS: i64 = 15;

/// Whether a check's conclusion lets a gate pass (a check skipped by its own rules does).
fn passes(conclusion: &str) -> bool {
    matches!(conclusion, "success" | "neutral" | "skipped")
}

/// What an environment's promotion and custom gates find for commit `sha` (W15-R03): when its
/// deployment to `after` succeeded, and each gate's result. A workflow gate is that workflow's
/// run for the commit; a check gate is the check run of that name, else the commit status, else
/// a run or job kiln names so (`smoke`, `smoke/test`).
async fn gate_facts(hub: &Hub, repo: &Repo, sha: &str, p: &kiln_ci::env::Protection) -> kiln_ci::env::Found {
    let mut found = kiln_ci::env::Found::default();
    if let Some(after) = p.after.clone() {
        let (repo_id, sha) = (repo.id.clone(), sha.to_owned());
        found.promoted_at = hub
            .store
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT MAX(s.created_at) FROM ci_deployments d JOIN ci_deployment_statuses s ON s.deployment_id = d.id WHERE d.repo_id = ?1 AND d.environment = ?2 AND d.sha = ?3 AND s.state = 'success'",
                    rusqlite::params![repo_id, after, sha],
                    |r| r.get::<_, Option<i64>>(0),
                )?)
            })
            .await
            .unwrap_or(None);
    }
    if p.gates.is_empty() {
        return found;
    }
    let ran = super::review::checks_on(hub, &repo.name, sha).await.unwrap_or_default();
    for g in &p.gates {
        let result = match (&g.workflow, &g.check) {
            (Some(w), _) => ran.get(w).and_then(|c| c.as_deref().map(passes)),
            (None, Some(name)) => {
                let (repo_id, sha, n) = (repo.id.clone(), sha.to_owned(), name.clone());
                // (whether it's done, and how), from a check run or a commit status.
                let recorded: Option<(bool, String)> = hub
                    .store
                    .call(move |c| {
                        let check: Option<(String, Option<String>)> = c
                            .query_row("SELECT status, conclusion FROM ci_check_runs WHERE repo_id = ?1 AND head_sha = ?2 AND name = ?3 ORDER BY id DESC LIMIT 1", rusqlite::params![repo_id, sha, n], |r| Ok((r.get(0)?, r.get(1)?)))
                            .optional()?;
                        if let Some((status, conclusion)) = check {
                            return Ok(Some((status == "completed", conclusion.unwrap_or_default())));
                        }
                        let state: Option<String> = c.query_row("SELECT state FROM ci_statuses WHERE repo_id = ?1 AND sha = ?2 AND context = ?3 ORDER BY id DESC LIMIT 1", rusqlite::params![repo_id, sha, n], |r| r.get(0)).optional()?;
                        Ok(state.map(|s| (s != "pending", s)))
                    })
                    .await
                    .unwrap_or(None);
                match recorded {
                    Some((false, _)) => None,
                    Some((true, how)) => Some(passes(&how)),
                    None => ran.get(name).and_then(|c| c.as_deref().map(passes)),
                }
            }
            (None, None) => None,
        };
        found.gates.push((g.name(), result));
    }
    found
}

/// The timing table a run's saved input kept, so a replay shards as the run did.
fn saved_timing(v: &serde_json::Value) -> Arc<HashMap<String, f64>> {
    Arc::new(v.get("timing").and_then(serde_json::Value::as_object).map(|o| o.iter().filter_map(|(k, x)| Some((k.clone(), x.as_f64()?))).collect()).unwrap_or_default())
}

/// The commit's files a run's saved input kept for its shards (`files:`), so a replay splits them
/// as the run did.
fn saved_tree(v: &serde_json::Value) -> Arc<HashMap<String, Vec<String>>> {
    Arc::new(v.get("tree").and_then(|t| serde_json::from_value(t.clone()).ok()).unwrap_or_default())
}

/// The commit's files that the shards and chunks of a workflow, and of those it calls, split
/// (`files:`, W04-R14), by pattern: listed once when a run starts, and saved with its input.
async fn shard_files(hub: &Hub, repo: &Repo, sha: &str, wf: &Workflow, called: &HashMap<String, Arc<Workflow>>) -> HashMap<String, Vec<String>> {
    let patterns: std::collections::BTreeSet<String> = std::iter::once(wf)
        .chain(called.values().map(|w| &**w))
        .flat_map(|w| &w.jobs)
        .filter_map(|j| j.strategy.as_ref())
        .flat_map(|s| [s.shard.as_ref(), s.chunk.as_ref()])
        .flatten()
        .filter_map(|v| v.get("files").and_then(kiln_ci::wf::ir::YVal::as_str))
        .map(str::to_owned)
        .collect();
    if patterns.is_empty() {
        return HashMap::new();
    }
    let mirror = super::events::facts(hub, repo).await.mirror;
    let listing = match crate::gitops::run_git(hub, &["-C", &mirror, "ls-tree", "-r", "-z", "--name-only", "--full-tree", sha], crate::gitops::LS_REMOTE_TIMEOUT).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(repo = %repo.name, sha, error = %format!("{e:#}"), "the commit's files for its shards");
            return HashMap::new();
        }
    };
    let all: Vec<String> = listing.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    patterns
        .into_iter()
        .map(|p| {
            let matched = kiln_ci::trigger::pattern::Patterns::new(&[p.as_str()]).map(|m| all.iter().filter(|f| m.matches(f)).cloned().collect()).unwrap_or_default();
            (p, matched)
        })
        .collect()
}

struct Dispatch {
    hub: Arc<Hub>,
    sched: Scheduler,
    /// The routing rules by source (W05-R21); the scheduler holds them in order.
    routing: super::routing::Sources,
    /// Legs waiting on their environment's protection.
    held: HashMap<String, Held>,
    runs: HashMap<String, RunSt>,
    legs: HashMap<String, LegSt>,
    timers: BTreeSet<i64>,
    /// What a command left for the command loop to do next ([`Dispatch::fail_unstarted`], and
    /// any commands a command's own work made): run by the loop, not a level down from it, which
    /// in a debug build would take more stack than a shard has.
    next: Vec<Command>,
    /// The command loop is running: commands made inside it wait in `next` for their turn.
    in_commands: bool,
}

fn now_ms() -> i64 {
    crate::util::now_unix() * 1000
}

/// Default priority by event (W05-R09).
fn priority(event: &str) -> u8 {
    match event {
        "workflow_dispatch" | "manual" => 100,
        "merge_group" | "land" => 90,
        "pull_request" | "pull_request_target" | "diff" => 80,
        "push" | "agent" => 60,
        "workflow_run" | "pipeline" => 50,
        "schedule" => 20,
        _ => 50,
    }
}

fn status_of(c: PlanC) -> Status {
    match c {
        PlanC::Success => Status::Succeeded,
        PlanC::Failure => Status::Failed,
        PlanC::Cancelled => Status::Cancelled,
        PlanC::Skipped => Status::Skipped,
        PlanC::TimedOut => Status::TimedOut,
    }
}

/// Starts the dispatcher when the engine is on.
pub fn spawn(hub: Arc<Hub>) {
    if !hub.cfg.ci.enabled {
        return;
    }
    let Some(rx) = hub.ci.rx.lock().take() else { return };
    hub.ci.on.store(true, Ordering::Relaxed);
    super::services::spawn_maintenance(Arc::clone(&hub));
    super::webhooks::spawn(Arc::clone(&hub));
    super::insights::spawn(Arc::clone(&hub));
    super::logsearch::spawn(Arc::clone(&hub));
    crate::rt::spawn(async move {
        // Legs kept to look into before a restart (W17-R02).
        match runs::kept_legs(&hub).await {
            Ok(kept) => {
                for (leg, agent, until) in kept {
                    hub.ci.keep(&leg, Some((agent, until)));
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "kept legs could not be read"),
        }
        let sources = super::routing::sources(&hub).await;
        let rules = sources.effective();
        let mut cfg = sched_config(&hub, rules);
        (cfg.pools, cfg.reservations) = super::pools::sources(&hub).await;
        let mut d = Dispatch { hub: Arc::clone(&hub), sched: Scheduler::new(cfg), routing: sources, held: HashMap::new(), runs: HashMap::new(), legs: HashMap::new(), timers: BTreeSet::new(), next: Vec::new(), in_commands: false };
        if let Err(e) = d.restore().await {
            tracing::warn!(error = %format!("{e:#}"), "the CI engine could not rebuild its runs");
        }
        let agents: Vec<String> = hub.agents.lock().values().filter(|a| a.connected).map(|a| a.name.clone()).collect();
        for a in agents {
            d.runner_up(&a, &[]).await;
        }
        loop {
            let wait = d.timers.first().map(|at| Duration::from_millis(u64::try_from((at - now_ms()).max(0)).unwrap_or(0)));
            let msg = match wait {
                Some(w) => crate::rt::timeout(w, rx.recv()).await.ok(),
                None => Some(rx.recv().await),
            };
            match msg {
                Some(Ok(m)) => d.handle(m).await,
                Some(Err(_)) => break,
                None => d.tick().await,
            }
        }
    });
}

impl Dispatch {
    async fn handle(&mut self, m: Msg) {
        let r = match m {
            Msg::Start(s) => self.start(*s).await,
            Msg::RunnerUp { agent, running } => {
                self.runner_up(&agent, &running).await;
                Ok(())
            }
            Msg::RunnerFacts { agent } => {
                self.runner_facts(&agent).await;
                Ok(())
            }
            Msg::RunnerLost { agent } => {
                let cmds = self.sched.handle(Input::RunnerLost { runner: agent }, now_ms());
                self.commands(cmds).await;
                Ok(())
            }
            Msg::Event { agent, leg, json } => self.event(&agent, &leg, &json).await,
            Msg::Finished { agent, result } => self.finished(&agent, result).await,
            Msg::Rejected { agent, leg } => {
                self.hub.release_slot(&agent, &leg);
                if let Some(l) = self.legs.get_mut(&leg) {
                    l.agent = None;
                }
                let cmds = self.sched.handle(Input::Rejected { leg }, now_ms());
                self.commands(cmds).await;
                Ok(())
            }
            Msg::Cancel { run, reason } => self.cancel_run(&run, &reason).await,
            Msg::ApproveRun { run, by, approve, reply } => {
                let r = self.approve_run(&run, &by, approve).await;
                let _ = reply.try_send(*r.as_ref().unwrap_or(&false));
                r.map(|_| ())
            }
            Msg::Explain { leg, reply } => {
                let v = serde_json::json!({ "leg": leg, "known": self.legs.contains_key(&leg), "wait_reason": self.sched.reason(&leg), "runners": self.sched.explain(&leg) });
                let _ = reply.try_send(v);
                Ok(())
            }
            Msg::Preview { start, reply } => {
                let v = self.preview(&start).await;
                let _ = reply.try_send(v);
                Ok(())
            }
            Msg::Reviewed { run } => {
                let ids: Vec<String> = self.held.keys().filter(|id| self.legs.get(*id).is_some_and(|l| l.run == run)).cloned().collect();
                for id in ids {
                    if let Err(e) = self.gate(&id).await {
                        tracing::warn!(leg = %id, error = %format!("{e:#}"), "an environment's protection could not be checked");
                    }
                }
                Ok(())
            }
            Msg::Executor { agent, json } => {
                let said: crate::runner::pool::Said = serde_json::from_str(&json).unwrap_or_default();
                let input = match said.state.as_str() {
                    "ready" => Input::ExecutorReady { executor: said.executor },
                    _ => {
                        let error = said.error.unwrap_or_else(|| "it failed".into());
                        tracing::warn!(runner = %agent, executor = %said.executor, error = %error, "a pool's executor didn't start");
                        Input::ExecutorFailed { executor: said.executor, error }
                    }
                };
                let cmds = self.sched.handle(input, now_ms());
                self.commands(cmds).await;
                Ok(())
            }
            Msg::PoolsChanged => {
                let (pools, reservations) = super::pools::sources(&self.hub).await;
                let cmds = self.sched.handle(Input::PoolsChanged { pools, reservations }, now_ms());
                self.commands(cmds).await;
                Ok(())
            }
            Msg::Pools { reply } => {
                let _ = reply.try_send(serde_json::to_value(self.sched.pools()).unwrap_or_default());
                Ok(())
            }
            Msg::RoutingChanged => {
                let fresh = super::routing::sources(&self.hub).await;
                if fresh != self.routing {
                    self.routing = fresh;
                    self.rules_changed().await;
                }
                Ok(())
            }
            Msg::ExplainRoute { route, reply } => {
                let (req, runners) = self.sched.explain_route(&route);
                let constraints: Vec<&String> = req.constraints.iter().map(|(text, _)| text).collect();
                let labelled = self.routing.labelled();
                let rules: Vec<serde_json::Value> = req.rules.iter().filter_map(|i| labelled.get(*i)).map(|(source, r)| serde_json::json!({ "source": source, "rule": r })).collect();
                let v = serde_json::json!({ "labels": req.labels, "group": req.group, "constraints": constraints, "executors": req.executors, "image": req.image, "rules": rules, "runners": runners });
                let _ = reply.try_send(v);
                Ok(())
            }
            Msg::Rerun { run, mode, debug, keep_failed, actions, no_reuse, by, reply } => {
                let r = self.rerun(&run, &mode, debug, keep_failed, actions, no_reuse, &by).await.map_err(|e| format!("{e:#}"));
                let _ = reply.try_send(r);
                Ok(())
            }
        };
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "CI dispatcher");
        }
    }

    /// Hands the scheduler the rules as they now stand: waiting legs are routed again.
    async fn rules_changed(&mut self) {
        let cmds = self.sched.handle(Input::RulesChanged(self.routing.effective()), now_ms());
        self.commands(cmds).await;
    }

    async fn tick(&mut self) {
        let now = now_ms();
        let mut woke = false;
        while self.timers.first().is_some_and(|t| *t <= now) {
            self.timers.pop_first();
            woke = true;
        }
        // Wait timers that ran out (W15-R03).
        if woke && !self.held.is_empty() {
            let ids: Vec<String> = self.held.keys().cloned().collect();
            for id in ids {
                if let Err(e) = self.gate(&id).await {
                    tracing::warn!(leg = %id, error = %format!("{e:#}"), "an environment's protection could not be checked");
                }
            }
        }
        let cmds = self.sched.handle(Input::Tick, now);
        self.commands(cmds).await;
    }

    /// A runner as the scheduler sees it: what its probes found (W13-R01), else what its build
    /// announced; never an executor its build didn't.
    fn runner_view(&self, a: &crate::hub::AgentLive) -> RunnerView {
        let caps = self.hub.runner_caps(&a.name);
        let mut facts = match &a.facts {
            Some(f) => (**f).clone(),
            None => {
                let mut executors = std::collections::BTreeMap::new();
                if caps & CAP_DOCKER != 0 {
                    let rootless = caps & crate::proto::CAP_DOCKER_ROOTLESS != 0;
                    executors.insert("docker".to_owned(), kiln_ci::facts::Executor { details: [("rootless".to_owned(), rootless.to_string())].into(), ..kiln_ci::facts::Executor::default() });
                }
                kiln_ci::facts::RunnerFacts { os: a.os.clone(), arch: a.arch.clone(), executors, ..kiln_ci::facts::RunnerFacts::default() }
            }
        };
        if caps & CAP_DOCKER == 0 {
            facts.executors.remove("docker");
        }
        facts.executors.entry("host".to_owned()).or_default();
        RunnerView { name: a.name.clone(), labels: a.labels.clone(), pool: None, facts, caps: u64::from(caps) }
    }

    async fn runner_facts(&mut self, agent: &str) {
        let Some(a) = self.hub.agents.lock().get(agent).cloned() else { return };
        let r = RunnerIn { id: agent.to_owned(), view: self.runner_view(&a), slots: a.slots, capacity: None };
        let cmds = self.sched.handle(Input::RunnerChanged(Box::new(r)), now_ms());
        self.commands(cmds).await;
    }

    async fn runner_up(&mut self, agent: &str, running: &[String]) {
        let Some(a) = self.hub.agents.lock().get(agent).cloned() else { return };
        let view = self.runner_view(&a);
        // Legs this runner was running that it no longer has were lost.
        let gone: Vec<(String, u32)> = self.legs.iter().filter(|(id, l)| l.agent.as_deref() == Some(agent) && !running.contains(id) && !a.running.contains(id)).map(|(id, l)| (id.clone(), l.attempt)).collect();
        // Legs it says it runs that aren't its own now (given up while it was away, past the
        // reconnect grace): it stops them.
        let stale: Vec<String> = running.iter().filter(|id| self.hub.ci.is_leg(id) && !self.legs.get(*id).is_some_and(|l| l.agent.as_deref() == Some(agent))).cloned().collect();
        for leg in stale {
            self.hub.release_slot(agent, &leg);
            self.hub.send_to_agent(agent, LANE_CONTROL, &ServerMsg::Cancel { job: leg, reason: "the leg was given up while this runner was away".into() }).await;
        }
        // The hub keeps at least 1 for runners that take jobs; 0 (an outpost) takes no legs.
        let r = RunnerIn { id: agent.to_owned(), view, slots: a.slots, capacity: None };
        let mut cmds = self.sched.handle(Input::RunnerUp(Box::new(r)), now_ms());
        for (leg, attempt) in gone {
            let end = runs::AttemptEnd { conclusion: "failure", class: Some(FailureClass::RunnerLost.as_str()), exit_code: None, error: Some("the runner lost it".into()), peak_rss: None, cpu_seconds: None };
            if let Err(e) = runs::attempt_finished(&self.hub, &leg, attempt, end).await {
                tracing::warn!(leg = %leg, error = %format!("{e:#}"), "the attempt could not be recorded");
            }
            cmds.extend(self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }) }, now_ms()));
        }
        self.commands(cmds).await;
    }

    /// A workflow that matched an event becomes a run (W04-R01).
    async fn start(&mut self, mut s: Start) -> anyhow::Result<()> {
        // The repository's routing rules as its default branch has them now (W05-R21).
        let fresh = super::routing::repo_rules(&self.hub, &s.repo).await;
        if self.routing.set_repo(&s.repo.name, fresh) {
            self.rules_changed().await;
        }
        let id = crate::util::new_id();
        s.event.trusted = trusted_of(&self.hub.cfg.ci, &s.repo, &s.event);
        let trusted = s.event.trusted;
        // An outside contributor's run waits for a maintainer before any job starts (W18-R06).
        let approval = kiln_ci::trust::needs_approval(self.hub.cfg.ci.untrusted.require_approval, trusted, association(&s.event).as_deref());
        let model = serde_json::to_string(&*s.workflow).unwrap_or_default();
        let name = s.workflow.name.clone().unwrap_or_else(|| s.path.clone());
        let new = runs::NewRun {
            id: id.clone(),
            workflow: s.path.clone(),
            name: name.clone(),
            event: s.event.name.clone(),
            branch: s.event.ref_name().to_owned(),
            sha: s.event.sha.clone(),
            message: s.message.clone(),
            author: s.author.clone(),
            actor: s.event.actor.clone(),
            trigger: serde_json::json!({ "event": s.event.name, "action": s.event.action, "ref": s.event.git_ref, "before": s.event.before }),
            trusted,
            priority: priority(&s.event.name),
            payload: s.event.payload.clone(),
            model,
            input: String::new(),
        };
        let (number, rowid) = runs::create(&self.hub, &s.repo, new).await?;
        if let Some(r) = &s.given.reply {
            let _ = r.try_send(id.clone());
        }
        // A kiln workflow that declares its secrets needs every required one (W09-R06).
        if let Some(why) = self.missing_secrets(&s).await {
            runs::finish_run(&self.hub, &id, Status::Failed, "startup_failure", Some(why)).await?;
            self.hub.emit_run(&id).await;
            return Ok(());
        }
        let urls = github_api(&self.hub, &s.repo).await.map(|(app, _)| (app.web.clone(), app.api.clone()));
        let github = self.github_context(&s, number, rowid, &name, urls.as_ref());
        let inputs = if s.inputs.is_null() { s.event.payload.get("inputs").cloned().unwrap_or(serde_json::Value::Null) } else { s.inputs.clone() };
        // The `vars` context (W09-R09): every scope but environments', which legs add, and what
        // the run was given (W17-R06) over them.
        let mut vars = match super::secrets::vars_for(&self.hub, &s.repo, &s.path, None).await {
            Ok(v) => serde_json::Value::Object(v),
            Err(e) => {
                tracing::warn!(run = %id, error = %format!("{e:#}"), "variables could not be read");
                serde_json::json!({})
            }
        };
        if let Some(o) = vars.as_object_mut() {
            o.extend(s.given.vars.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))));
        }
        let input = RunInput { github: github.clone(), inputs, vars: vars.clone(), changed: s.event.changed.clone().map(Arc::new), trusted };
        let perms = if super::repo_config::load(&self.hub, &s.repo).await.default_permissions() == plan::perms::Default::Write { "write" } else { "read" };
        let only: serde_json::Map<String, serde_json::Value> = s.given.only_matrix.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect();
        let timing = super::results::timing(&self.hub, &s.repo.id).await;
        let files = Arc::new(super::reusable::files(&self.hub, &s.repo, &s.event.sha, &s.workflow).await);
        let tree = Arc::new(shard_files(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await);
        let settings = serde_json::json!({ "default_permissions": perms, "only_matrix": only, "timing": *timing, "tree": *tree });
        self.save_input(&id, &input, &s.actions, &s.event, (false, None), &HashMap::new(), &settings).await?;
        let started = RunPlan::start(Arc::clone(&s.workflow), input, self.settings_with(&settings), &Source { files: &files, timing: &timing, tree: &tree });
        let (plan, actions) = match started {
            Ok(x) => x,
            Err(e) => {
                runs::finish_run(&self.hub, &id, Status::Failed, "failure", Some(format!("The run could not start: {e}"))).await?;
                self.hub.emit_run(&id).await;
                return Ok(());
            }
        };
        runs::set_run_name(&self.hub, &id, plan.name.clone()).await?;
        let group = plan.concurrency.as_ref().map(|g| (format!("{}\0{}", s.repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
        let costs = super::insights::costs(&self.hub, &s.repo.id, &s.path).await;
        let mut st = RunSt { plan, repo: s.repo, event: s.event, path: s.path, created: crate::util::now_unix(), in_progress: false, files, wf: s.workflow, github, vars, trusted, actions: s.actions, keys: HashMap::new(), held: Some(actions), seq: 0, keep: HashMap::new(), debug: false, keep_failed: None, given: s.given, timing, tree, removed: 0, costs, awaiting: None, no_reuse: false };
        self.workflow_run(&st, "requested", None);
        if approval {
            runs::set_approval(&self.hub, &id, "required", None).await?;
            st.awaiting = Some(Awaiting { group });
            self.runs.insert(id.clone(), st);
            self.hub.ci.runs.lock().insert(id.clone());
            self.hub.emit_run(&id).await;
            crate::rt::spawn(super::notify::approval_waiting(Arc::clone(&self.hub), id, Vec::new()));
            return Ok(());
        }
        self.runs.insert(id.clone(), st);
        self.hub.ci.runs.lock().insert(id.clone());
        self.hub.emit_run(&id).await;
        let cmds = self.sched.handle(Input::RunQueued { run: id, group, supersede: None }, now_ms());
        self.commands(cmds).await;
        Ok(())
    }

    /// A maintainer's decision on an outside contributor's run (W18-R06).
    async fn approve_run(&mut self, run: &str, by: &str, approve: bool) -> anyhow::Result<bool> {
        let Some(waiting) = self.runs.get_mut(run).and_then(|r| r.awaiting.take()) else { return Ok(false) };
        runs::set_approval(&self.hub, run, if approve { "approved" } else { "rejected" }, Some(by)).await?;
        if approve {
            self.hub.emit_run(run).await;
            let cmds = self.sched.handle(Input::RunQueued { run: run.to_owned(), group: waiting.group, supersede: None }, now_ms());
            self.commands(cmds).await;
        } else {
            self.cancel_run(run, &format!("{by} didn't approve it")).await?;
        }
        Ok(true)
    }

    /// `KCI4040`: the required secrets a kiln workflow declares that don't resolve for each job's
    /// environment, with the scopes searched and how to add them; `None` when all do.
    async fn missing_secrets(&self, s: &Start) -> Option<String> {
        let wf = &s.workflow;
        let mut lines = self.missing_action_secrets(s).await;
        if wf.secrets.is_empty() || !wf.secrets.iter().any(|d| d.required) {
            return (!lines.is_empty()).then(|| lines.join("\n"));
        }
        let mut envs: Vec<Option<String>> = vec![None];
        for j in &wf.jobs {
            if let Some(e) = j.environment.as_ref().map(|e| e.name.src.clone()).filter(|n| !n.contains("${{"))
                && !envs.contains(&Some(e.clone()))
            {
                envs.push(Some(e));
            }
        }
        for env in envs {
            let have = super::secrets::names_for(&self.hub, &s.repo, &s.path, env.as_deref()).await.unwrap_or_default();
            for name in kiln_ci::secrets::missing_required(&wf.secrets, &have) {
                lines.push(format!(
                    "KCI4040: the workflow requires the secret {name}, which isn't set{}: searched {}. Add it: kiln secrets set {name} --scope repo:{}",
                    env.as_deref().map(|e| format!(" for environment `{e}`")).unwrap_or_default(),
                    super::secrets::searched(&s.repo, &s.path, env.as_deref()),
                    s.repo.name
                ));
            }
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// `KCI4040` for kiln actions (W09-R07): each required secret an action a job uses declares,
    /// that doesn't resolve for that job (as the job's own name, or the one `with-secrets` maps).
    async fn missing_action_secrets(&self, s: &Start) -> Vec<String> {
        let mut lines = Vec::new();
        let mut have: HashMap<Option<String>, std::collections::BTreeSet<String>> = HashMap::new();
        for j in &s.workflow.jobs {
            let wanted: Vec<_> = declared_sources(&j.steps, &s.actions).into_iter().filter(|d| d.3).collect();
            if wanted.is_empty() {
                continue;
            }
            let env = j.environment.as_ref().map(|e| e.name.src.clone()).filter(|n| !n.contains("${{"));
            if !have.contains_key(&env) {
                let names = super::secrets::names_for(&self.hub, &s.repo, &s.path, env.as_deref()).await.unwrap_or_default();
                have.insert(env.clone(), names);
            }
            let names = have.get(&env).cloned().unwrap_or_default();
            for (uses, name, from, _) in wanted {
                if !names.contains(&from) {
                    let mapped = if from == name { String::new() } else { format!(" (as {from}, through `with-secrets`)") };
                    lines.push(format!(
                        "KCI4040: `{uses}` in job `{}` needs the secret {name}{mapped}, which isn't set{}: searched {}. Add it: kiln secrets set {from} --scope repo:{}",
                        j.id,
                        env.as_deref().map(|e| format!(" for environment `{e}`")).unwrap_or_default(),
                        super::secrets::searched(&s.repo, &s.path, env.as_deref()),
                        s.repo.name
                    ));
                }
            }
        }
        lines
    }

    /// `workflow_run` for this run (W03-R02): `requested`, `in_progress` or `completed`, to the
    /// default branch's workflows that listen for it, off the dispatcher's loop.
    fn workflow_run(&self, r: &RunSt, action: &str, conclusion: Option<&str>) {
        let g = &r.github;
        let num = |k: &str| g.get(k).and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64())).unwrap_or(0);
        let rfc = |t: i64| time::OffsetDateTime::from_unix_timestamp(t).ok().and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok()).unwrap_or_default();
        let summary = kiln_ci::trigger::context::RunSummary {
            id: num("run_id"),
            name: g.get("workflow").and_then(serde_json::Value::as_str).unwrap_or(&r.path).to_owned(),
            path: r.path.clone(),
            display_title: r.plan.name.clone().unwrap_or_else(|| r.event.message.clone().unwrap_or_default().lines().next().unwrap_or_default().to_owned()),
            run_number: num("run_number"),
            run_attempt: u32::try_from(num("run_attempt")).unwrap_or(1).max(1),
            event: r.event.name.clone(),
            status: if action == "completed" { "completed".into() } else if action == "in_progress" { "in_progress".into() } else { "queued".into() },
            conclusion: conclusion.map(str::to_owned),
            head_branch: r.event.ref_name().to_owned(),
            head_sha: r.event.sha.clone(),
            head_message: r.event.message.clone().unwrap_or_default(),
            actor: r.event.actor.clone(),
            created_at: rfc(r.created),
            updated_at: rfc(crate::util::now_unix()),
        };
        let depth = r.event.payload.pointer("/kiln/depth").and_then(serde_json::Value::as_u64).unwrap_or(0);
        let (hub, repo, action) = (Arc::clone(&self.hub), r.repo.clone(), action.to_owned());
        crate::rt::spawn(async move { super::events::workflow_run(&hub, &repo, &action, &summary, depth).await });
    }

    /// What [`Self::start`] would plan for `s`, without a run: the `github` context of run 0,
    /// the variables and reusable workflows a run would get, and each leg's routing against the
    /// runners connected now.
    async fn preview(&mut self, s: &Start) -> serde_json::Value {
        let name = s.workflow.name.clone().unwrap_or_else(|| s.path.clone());
        let urls = github_api(&self.hub, &s.repo).await.map(|(app, _)| (app.web.clone(), app.api.clone()));
        let github = self.github_context(s, 0, 0, &name, urls.as_ref());
        let inputs = if s.inputs.is_null() { s.event.payload.get("inputs").cloned().unwrap_or(serde_json::Value::Null) } else { s.inputs.clone() };
        let vars = super::secrets::vars_for(&self.hub, &s.repo, &s.path, None).await.map(serde_json::Value::Object).unwrap_or_else(|_| serde_json::json!({}));
        let input = RunInput { github, inputs, vars, changed: s.event.changed.clone().map(Arc::new), trusted: s.event.trusted };
        let files = Arc::new(super::reusable::files(&self.hub, &s.repo, &s.event.sha, &s.workflow).await);
        let perms = if super::repo_config::load(&self.hub, &s.repo).await.default_permissions() == plan::perms::Default::Write { "write" } else { "read" };
        let timing = super::results::timing(&self.hub, &s.repo.id).await;
        let tree = shard_files(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await;
        let p = match RunPlan::preview(Arc::clone(&s.workflow), input, self.settings_with(&serde_json::json!({ "default_permissions": perms })), &Source { files: &files, timing: &timing, tree: &tree }) {
            Ok(p) => p,
            Err(e) => return serde_json::json!({ "error": e }),
        };
        let workflow = s.workflow.name.clone().unwrap_or_default();
        let envs = super::environments::defs(&self.hub, &s.repo).await.unwrap_or_default();
        let legs: Vec<serde_json::Value> = p
            .legs
            .iter()
            .map(|spec| {
                let untrusted = (!trusted_of(&self.hub.cfg.ci, &s.repo, &s.event)).then(|| self.hub.cfg.ci.untrusted.route());
                let mut route = route_of(spec, &workflow, &s.repo.name, &s.event.name, self.hub.cfg.ci.local, untrusted);
                // Where its environment deploys, as its gate adds it (W15-R02).
                if let Some(d) = spec.environment.as_ref().and_then(|e| envs.get(&e.name.to_ascii_lowercase())) {
                    for q in super::environments::routing(d) {
                        if !route.requires.contains(&q) {
                            route.requires.push(q);
                        }
                    }
                }
                let (req, runners) = self.sched.explain_route(&route);
                let mut v = serde_json::to_value(spec).unwrap_or_default();
                if let Some(o) = v.as_object_mut() {
                    let constraints: Vec<&String> = req.constraints.iter().map(|(text, _)| text).collect();
                    o.insert("routing".into(), serde_json::json!({ "labels": req.labels, "group": req.group, "constraints": constraints, "executors": req.executors, "image": req.image, "rules": req.rules, "runners": runners }));
                }
                v
            })
            .collect();
        serde_json::json!({ "name": p.name, "legs": legs, "decided": p.decided, "removed": p.removed, "later": p.later })
    }

    fn settings(&self) -> Settings {
        Settings { default_runs_on: vec!["k8s".into()], ..Settings::default() }
    }

    /// A run's settings: the repository's default token permissions (W10-R04) and the matrix
    /// combinations asked for (W17-R06), as it started or as its saved input says, so a replay or
    /// rerun plans exactly as the run did.
    fn settings_with(&self, saved: &serde_json::Value) -> Settings {
        let default_permissions = if saved.get("default_permissions").and_then(serde_json::Value::as_str) == Some("write") { plan::perms::Default::Write } else { plan::perms::Default::Read };
        let only_matrix = saved.get("only_matrix").and_then(serde_json::Value::as_object).map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect()).unwrap_or_default();
        Settings { default_permissions, only_matrix, ..self.settings() }
    }

    #[allow(clippy::too_many_arguments)]
    async fn save_input(&self, run: &str, input: &RunInput, actions: &std::collections::BTreeMap<String, wire::ActionRef>, event: &Event, debug: (bool, Option<u32>), keep: &HashMap<String, (LegReport, String)>, settings: &serde_json::Value) -> anyhow::Result<()> {
        let keep: serde_json::Map<String, serde_json::Value> = keep.iter().map(|(k, (rep, leg))| (k.clone(), serde_json::json!({ "report": report_json(rep), "leg": leg }))).collect();
        let (debug, keep_failed) = debug;
        let json = serde_json::json!({ "github": input.github, "inputs": input.inputs, "vars": input.vars, "changed": input.changed.as_deref(), "trusted": input.trusted, "actions": actions, "event": event, "debug": debug, "keep_failed": keep_failed, "keep": keep, "default_permissions": settings.get("default_permissions"), "only_matrix": settings.get("only_matrix"), "timing": settings.get("timing"), "tree": settings.get("tree"), "no_reuse": settings.get("no_reuse") }).to_string();
        let run = run.to_owned();
        self.hub.store.call(move |c| Ok(c.execute("UPDATE ci_plans SET input = ?2 WHERE run_id = ?1", rusqlite::params![run, json]).map(|_| ())?)).await
    }

    /// The `github` context (W03-R07, W06-R03), with GitHub's URLs for repositories hosted there
    /// whose jobs act on GitHub (`github_api: github`, W14-R12; `urls`: its web and API origins).
    fn github_context(&self, s: &Start, number: i64, rowid: i64, name: &str, urls: Option<&(String, String)>) -> serde_json::Value {
        // The code host is kiln's facade; each runner sees it at its own address of the server.
        let base = self.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
        let server_url = urls.map_or_else(|| base.clone(), |(web, _)| web.clone());
        let default_branch = s.event.payload.pointer("/repository/default_branch").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let repo = kiln_ci::trigger::context::Repo { full_name: s.repo.name.clone(), id: 0, owner_id: 0, default_branch, private: false, server_url, created_at: s.repo.created_at };
        let run = kiln_ci::trigger::context::RunFacts {
            run_id: u64::try_from(rowid).unwrap_or(0),
            run_number: u64::try_from(number).unwrap_or(0),
            run_attempt: 1,
            workflow: name.to_owned(),
            workflow_ref: format!("{}/{}@{}", s.repo.name, s.path, s.event.git_ref),
            workflow_sha: s.event.sha.clone(),
            api_url: urls.map_or_else(|| format!("{base}/api/v3"), |(_, api)| api.clone()),
            graphql_url: urls.map_or_else(|| format!("{base}/api/graphql"), |(_, api)| graphql_of(api)),
            retention_days: 90,
            actor_id: 0,
            triggering_actor: String::new(),
        };
        kiln_ci::trigger::context::github(&s.event, &repo, &run)
    }

    /// Carries out what a plan decided (W04-R02).
    async fn actions(&mut self, run: &str, actions: Vec<PlanAction>) -> anyhow::Result<()> {
        for a in actions {
            match a {
                PlanAction::Queue(spec) => {
                    // A rerun's leg that isn't run again keeps the last attempt's result.
                    match self.runs.get_mut(run).and_then(|r| r.keep.remove(&spec.key)) {
                        Some((report, old)) => Box::pin(self.keep(run, *spec, report, &old)).await?,
                        None => self.queue(run, *spec).await?,
                    }
                }
                PlanAction::Settle { leg, conclusion, reason } => {
                    let existing = self.runs.get(run).and_then(|r| r.keys.get(&leg.key).cloned());
                    match existing {
                        Some(id) => {
                            runs::finish_leg(&self.hub, &id, status_of(conclusion), conclusion.result(), None, Some(reason)).await?;
                            self.hub.emit_job(&id).await;
                        }
                        None => {
                            let id = crate::util::new_id();
                            runs::insert_leg(&self.hub, run, &id, &leg, status_of(conclusion), Some(reason)).await?;
                            if let Some(r) = self.runs.get_mut(run) {
                                r.keys.insert(leg.key.clone(), id.clone());
                            }
                            self.hub.emit_job(&id).await;
                        }
                    }
                }
                PlanAction::Job { job, conclusion, reason } => {
                    // A job decided before it had legs still shows on the run, as GitHub shows it.
                    if let Some(r) = self.runs.get(run) {
                        let spec = placeholder_spec(&r.wf, &job);
                        let id = crate::util::new_id();
                        runs::insert_leg(&self.hub, run, &id, &spec, status_of(conclusion), Some(reason)).await?;
                        self.hub.emit_job(&id).await;
                    }
                }
                PlanAction::Cancel { leg, reason, .. } => {
                    let id = self.runs.get(run).and_then(|r| r.keys.get(&leg).cloned());
                    if let Some(id) = id {
                        // Held for its environment: it never reached the scheduler, and ends here.
                        if self.held.remove(&id).is_some() {
                            Box::pin(self.settled(&id, Outcome::Cancelled, Some(reason))).await?;
                            continue;
                        }
                        let cmds = self.sched.handle(Input::LegCancelled { leg: id, reason }, now_ms());
                        Box::pin(self.commands(cmds)).await;
                    }
                }
                PlanAction::Warn { leg, message } => {
                    let id = self.runs.get(run).and_then(|r| r.keys.get(&leg).cloned());
                    if let Some(id) = id {
                        runs::annotation(&self.hub, &id, None, "warning", None, &message, None, None, None, None, None).await?;
                    }
                }
                PlanAction::Finish(c) => {
                    self.save_removed(run).await?;
                    runs::finish_run(&self.hub, run, status_of(c), c.result(), None).await?;
                    // W19-R06: who wants to hear of it; R02: its trace.
                    crate::rt::spawn(super::notify::run_finished(Arc::clone(&self.hub), run.to_owned()));
                    crate::rt::spawn(super::otel::run_finished(Arc::clone(&self.hub), run.to_owned()));
                    let cmds = self.sched.handle(Input::RunFinished { run: run.to_owned() }, now_ms());
                    if let Some(r) = self.runs.remove(run) {
                        self.workflow_run(&r, "completed", Some(c.result()));
                        let mut g = self.hub.ci.legs.lock();
                        for id in r.keys.values() {
                            g.remove(id);
                        }
                    }
                    self.hub.ci.runs.lock().remove(run);
                    self.hub.emit_run(run).await;
                    Box::pin(self.commands(cmds)).await;
                }
            }
        }
        self.save_removed(run).await
    }

    /// The combinations the run's plan removed (`exclude`, `where`), by job, with why, saved when
    /// there are more than were (W04-R29): the run shows them, as the preview does.
    async fn save_removed(&mut self, run: &str) -> anyhow::Result<()> {
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        let n: usize = r.plan.removed().values().map(Vec::len).sum();
        if n == r.removed {
            return Ok(());
        }
        r.removed = n;
        let (run, json) = (run.to_owned(), serde_json::to_string(r.plan.removed()).unwrap_or_default());
        self.hub.store.call(move |c| Ok(c.execute("UPDATE ci_plans SET removed = ?2 WHERE run_id = ?1", rusqlite::params![run, json]).map(|_| ())?)).await
    }

    /// A leg of a rerun that keeps the last attempt's result: its copy, and its report to the plan.
    async fn keep(&mut self, run: &str, spec: LegSpec, report: LegReport, old: &str) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        runs::keep_leg(&self.hub, run, old, &id).await?;
        self.hub.emit_job(&id).await;
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id);
        r.seq += 1;
        let seq = r.seq;
        let files = Arc::clone(&r.files);
        let timing = Arc::clone(&r.timing);
        let tree = Arc::clone(&r.tree);
        let acts = r.plan.on_leg_finished(&spec.key, report.clone(), &Source { files: &files, timing: &timing, tree: &tree });
        runs::log(&self.hub, run, seq, "leg", Some(spec.key.clone()), Some(report_json(&report))).await?;
        self.actions(run, acts).await
    }

    /// A new attempt of a finished run (W05-R17): the same run, `github.run_attempt` one more,
    /// planned again from its saved workflow and input. The jobs `mode` selects run again; the
    /// others keep the last attempt's legs and results, which the plan takes as they were.
    #[allow(clippy::too_many_arguments)]
    async fn rerun(&mut self, run: &str, mode: &RerunMode, debug: bool, keep_failed: Option<u32>, fresh: Option<std::collections::BTreeMap<String, wire::ActionRef>>, no_reuse: bool, by: &str) -> anyhow::Result<u32> {
        if self.runs.contains_key(run) {
            anyhow::bail!("the run is still in progress; cancel it or wait for it to finish");
        }
        let r = run.to_owned();
        let saved: Option<(String, String, String)> = self.hub.store.call(move |c| Ok(c.query_row("SELECT p.workflow, p.input, r.repo_id FROM ci_plans p JOIN runs r ON r.id = p.run_id WHERE p.run_id = ?1", [&r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?)).await?;
        let Some((model, input, repo_id)) = saved else { anyhow::bail!("no such run of the CI engine") };
        let wf: Arc<Workflow> = Arc::new(serde_json::from_str(&model)?);
        let v: serde_json::Value = serde_json::from_str(&input)?;
        let event: Event = v.get("event").and_then(|e| serde_json::from_value(e.clone()).ok()).ok_or_else(|| anyhow::anyhow!("the run predates saved events; it can't be run again"))?;
        let repo = self.hub.store.repos().await?.into_iter().find(|x| x.id == repo_id).ok_or_else(|| anyhow::anyhow!("the run's repository is gone"))?;
        // The last attempt: each leg's last report, and its conclusion.
        let r = run.to_owned();
        let (reports, legs, seq): (Vec<(String, String)>, Vec<(String, String, Option<String>, Option<String>)>, i64) = self.hub.store.call(move |c| {
            let mut q = c.prepare("SELECT leg, report FROM ci_plan_log WHERE run_id = ?1 AND kind = 'leg' AND attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY seq")?;
            let reports = q.query_map([&r], |x| Ok((x.get::<_, Option<String>>(0)?.unwrap_or_default(), x.get::<_, Option<String>>(1)?.unwrap_or_default())))?.collect::<Result<Vec<_>, _>>()?;
            let mut q = c.prepare("SELECT id, key, job_id, conclusion FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1)")?;
            let legs = q.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            // The log goes on across attempts.
            let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) FROM ci_plan_log WHERE run_id = ?1", [&r], |x| x.get(0))?;
            Ok((reports, legs, seq))
        }).await?;
        let failed: BTreeSet<String> = legs.iter().filter(|(_, _, _, c)| matches!(c.as_deref(), Some("failure" | "cancelled" | "timed_out" | "startup_failure"))).filter_map(|(_, _, j, _)| j.clone()).collect();
        let again = kiln_ci::plan::rerun::jobs(&wf, mode, &failed).map_err(|e| anyhow::anyhow!(e))?;
        let top = |j: &str| kiln_ci::plan::rerun::top(j).to_owned();
        let last: HashMap<String, String> = reports.into_iter().collect();
        let keep: HashMap<String, (LegReport, String)> = legs
            .iter()
            .filter(|(_, _, j, _)| j.as_deref().is_some_and(|j| !again.contains(&top(j))))
            .filter_map(|(id, key, _, _)| last.get(key).map(|rep| (key.clone(), (report_from_json(rep), id.clone()))))
            .collect();
        let attempt = runs::start_attempt(&self.hub, run, by).await?;
        let mut github = v.get("github").cloned().unwrap_or_default();
        if let Some(o) = github.as_object_mut() {
            o.insert("run_attempt".into(), serde_json::Value::String(attempt.to_string()));
            // Whoever reran it, GitHub-exact: the person (for an agent, whom it acts for).
            o.insert("triggering_actor".into(), serde_json::Value::String(login_of(by)));
        }
        let changed: Option<Vec<String>> = v.get("changed").and_then(|c| serde_json::from_value(c.clone()).ok());
        let input = RunInput { github: github.clone(), inputs: v.get("inputs").cloned().unwrap_or_default(), vars: v.get("vars").cloned().unwrap_or_default(), changed: changed.map(Arc::new), trusted: v.get("trusted").and_then(serde_json::Value::as_bool).unwrap_or(false) };
        let actions = fresh.unwrap_or_else(|| v.get("actions").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default());
        let timing = super::results::timing(&self.hub, &repo.id).await;
        // "Run anyway" (W08-R10): no leg of this attempt takes an earlier result over.
        let files = Arc::new(super::reusable::files(&self.hub, &repo, &event.sha, &wf).await);
        let tree = Arc::new(shard_files(&self.hub, &repo, &event.sha, &wf, &files).await);
        let settings = serde_json::json!({ "default_permissions": v.get("default_permissions").and_then(serde_json::Value::as_str).unwrap_or("read"), "only_matrix": v.get("only_matrix"), "timing": *timing, "tree": *tree, "no_reuse": no_reuse });
        self.save_input(run, &input, &actions, &event, (debug, keep_failed), &keep, &settings).await?;
        let (plan, acts) = match RunPlan::start(Arc::clone(&wf), input.clone(), self.settings_with(&v), &Source { files: &files, timing: &timing, tree: &tree }) {
            Ok(x) => x,
            Err(e) => {
                runs::finish_run(&self.hub, run, Status::Failed, "failure", Some(format!("The run could not start: {e}"))).await?;
                self.hub.emit_run(run).await;
                return Ok(attempt);
            }
        };
        let group = plan.concurrency.as_ref().map(|g| (format!("{}\0{}", repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
        let path = run_path(&github);
        let costs = super::insights::costs(&self.hub, &repo.id, &path).await;
        // A maintainer reruns it: that is approval enough, as on GitHub.
        let st = RunSt { plan, repo, event, path, created: crate::util::now_unix(), in_progress: false, files, wf, github, vars: input.vars, trusted: input.trusted, actions, keys: HashMap::new(), held: Some(acts), seq, keep, debug, keep_failed, given: Given::default(), timing: Arc::clone(&timing), tree: Arc::clone(&tree), removed: 0, costs, awaiting: None, no_reuse };
        self.workflow_run(&st, "requested", None);
        self.runs.insert(run.to_owned(), st);
        self.hub.ci.runs.lock().insert(run.to_owned());
        self.hub.emit_run(run).await;
        let cmds = self.sched.handle(Input::RunQueued { run: run.to_owned(), group, supersede: None }, now_ms());
        self.commands(cmds).await;
        Ok(attempt)
    }

    /// A held leg against its environment's protection (W15-R03, R04): let go to the scheduler,
    /// failed (a ref that may not deploy there, a rejection), or kept waiting, its wait reason
    /// saying for what.
    async fn gate(&mut self, id: &str) -> anyhow::Result<()> {
        let Some((env, ready_at)) = self.held.get(id).map(|h| (h.env.clone(), h.ready_at)) else { return Ok(()) };
        let Some((run, git_ref, repo, sha, path)) = self.legs.get(id).and_then(|l| self.runs.get(&l.run).map(|r| (l.run.clone(), r.github.get("ref").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(), r.repo.clone(), r.event.sha.clone(), r.path.clone()))) else {
            self.held.remove(id);
            return Ok(());
        };
        let records = self.legs.get(id).and_then(|l| l.spec.environment.as_ref()).is_none_or(|e| e.deployment);
        let def = super::environments::ensure(&self.hub, &repo, &env).await?;
        let reviews = super::environments::reviews(&self.hub, &run, &def.name).await?;
        let now = crate::util::now_unix();
        let p = &def.protection;
        let verdict = if !records && !p.gates.is_empty() {
            // As GitHub's custom protection rules can't be.
            kiln_ci::env::Gate::Refused(format!("{} has custom gates, which a job with `deployment: false` can't use", def.name))
        } else if p.gates.iter().any(|g| g.workflow.as_deref() == Some(path.as_str())) {
            kiln_ci::env::Gate::Refused(format!("{}'s gate {path} is the workflow deploying to it: it can't pass before this job runs", def.name))
        } else {
            let found = if p.after.is_some() || !p.gates.is_empty() { gate_facts(&self.hub, &repo, &sha, p).await } else { kiln_ci::env::Found::default() };
            kiln_ci::env::gate_with(&def.name, p, &git_ref, ready_at, now, &reviews, &found)
        };
        match verdict {
            kiln_ci::env::Gate::Open => {
                // Its environment's concurrency limit (W15-R01): at most that many deployments
                // past the gate at once; the rest wait their turn, in the order they got ready.
                if let Some(limit) = def.concurrency
                    && self.deploying(&repo.id, &def.name, id) >= usize::try_from(limit).unwrap_or(usize::MAX)
                {
                    runs::wait_reason(&self.hub, id, &serde_json::json!({ "reason": "concurrency", "environment": def.name, "limit": limit })).await?;
                    self.hub.emit_job(id).await;
                    return Ok(());
                }
                let Some(mut h) = self.held.remove(id) else { return Ok(()) };
                runs::wait_reason(&self.hub, id, &serde_json::Value::Null).await?;
                self.hub.emit_job(id).await;
                // A host, container or microVM environment's jobs run on its runners (W15-R02).
                for q in super::environments::routing(&def) {
                    if !h.leg.route.requires.contains(&q) {
                        h.leg.route.requires.push(q);
                    }
                }
                let cmds = self.sched.handle(Input::LegQueued(Box::new(h.leg)), now_ms());
                Box::pin(self.commands(cmds)).await;
            }
            kiln_ci::env::Gate::Refused(m) => {
                self.held.remove(id);
                runs::annotation(&self.hub, id, None, "error", None, &m, None, None, None, None, None).await?;
                Box::pin(self.settled(id, Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() }), Some(m))).await?;
            }
            kiln_ci::env::Gate::Rejected { by, comment } => {
                self.held.remove(id);
                let m = format!("The deployment to {} was rejected by {by}{}", def.name, if comment.is_empty() { String::new() } else { format!(": {comment}") });
                runs::annotation(&self.hub, id, None, "error", None, &m, None, None, None, None, None).await?;
                Box::pin(self.settled(id, Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() }), Some(m))).await?;
            }
            kiln_ci::env::Gate::Waiting { reviewers, until, window, promotion, gate } => {
                let reason = match (reviewers.is_empty(), &window, &promotion, &gate) {
                    (false, ..) => "approval",
                    (true, Some(_), ..) => "window",
                    (true, None, Some(_), _) => "promotion",
                    (true, None, None, Some(_)) => "gate",
                    (true, None, None, None) => "wait-timer",
                };
                runs::wait_reason(&self.hub, id, &serde_json::json!({ "reason": reason, "environment": def.name, "reviewers": reviewers, "until": until, "window": window, "promotion": promotion, "gate": gate })).await?;
                // Promotion and custom gates move as other runs do: it's looked at again shortly.
                if promotion.is_some() || gate.is_some() {
                    self.timers.insert((now + GATE_AGAIN_SECS) * 1000);
                }
                self.hub.emit_job(id).await;
                // W19-R06: the first time it waits for a reviewer, who wants to hear of it.
                if !reviewers.is_empty()
                    && let Some(h) = self.held.get_mut(id)
                    && !h.announced
                {
                    h.announced = true;
                    crate::rt::spawn(super::notify::approval_waiting(Arc::clone(&self.hub), run.clone(), vec![def.name.clone()]));
                }
                if let Some(t) = until {
                    self.timers.insert(t * 1000);
                }
            }
        }
        Ok(())
    }

    /// How many of `repo`'s legs (other than `except`) are past `env`'s gate and not done: its
    /// deployments under way.
    fn deploying(&self, repo: &str, env: &str, except: &str) -> usize {
        self.legs
            .iter()
            .filter(|(id, l)| id.as_str() != except && !self.held.contains_key(id.as_str()) && l.spec.environment.as_ref().is_some_and(|e| e.name.eq_ignore_ascii_case(env)) && self.runs.get(&l.run).is_some_and(|r| r.repo.id == repo))
            .count()
    }

    /// A deployment to `repo`'s `env` ended: the legs held there are gated again, in the order
    /// they got ready, so the next under its concurrency limit goes (W15-R01).
    async fn next_deployments(&mut self, repo: &str, env: &str) -> anyhow::Result<()> {
        let mut waiting: Vec<(i64, String)> = self
            .held
            .iter()
            .filter(|(id, h)| h.env.eq_ignore_ascii_case(env) && self.legs.get(id.as_str()).and_then(|l| self.runs.get(&l.run)).is_some_and(|r| r.repo.id == repo))
            .map(|(id, h)| (h.ready_at, id.clone()))
            .collect();
        waiting.sort();
        for (_, id) in waiting {
            Box::pin(self.gate(&id)).await?;
        }
        Ok(())
    }

    /// A leg the plan queued: its row, then the scheduler.
    async fn queue(&mut self, run: &str, spec: LegSpec) -> anyhow::Result<()> {
        // W08-R10: a job that opted in takes an earlier leg's result for the same inputs.
        let reuse = self.reuse_hash(run, &spec).await;
        if let Some((hash, max_age)) = &reuse {
            match super::reuse::lookup(&self.hub, hash, *max_age).await {
                Ok(Some((source, report))) => return Box::pin(self.take_over(run, spec, &source, hash, report_from_json(&report))).await,
                Ok(None) => {}
                Err(e) => tracing::warn!(leg = %spec.key, error = %format!("{e:#}"), "reuse couldn't be looked up; the leg runs"),
            }
        }
        let id = crate::util::new_id();
        runs::insert_leg(&self.hub, run, &id, &spec, Status::Queued, None).await?;
        if let Some((hash, _)) = &reuse {
            super::reuse::set_hash(&self.hub, &id, hash).await?;
        }
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id.clone());
        self.hub.ci.legs.lock().insert(id.clone());
        let leg = leg_in(r, run, &id, &spec, &self.hub.cfg.ci);
        self.legs.insert(id.clone(), LegSt { run: run.to_owned(), key: spec.key.clone(), spec, attempt: 1, agent: None, report: LegReport::default(), class: None, deployment: None, env_url: None, seq: 0 });
        self.hub.emit_job(&id).await;
        // A leg bound to an environment starts when its protection lets it (W15-R04).
        if let Some(env) = self.legs.get(&id).and_then(|l| l.spec.environment.as_ref()).map(|e| e.name.clone()).filter(|n| !n.is_empty()) {
            self.held.insert(id.clone(), Held { leg, env, ready_at: crate::util::now_unix(), announced: false });
            return self.gate(&id).await;
        }
        let cmds = self.sched.handle(Input::LegQueued(Box::new(leg)), now_ms());
        Box::pin(self.commands(cmds)).await;
        Ok(())
    }

    /// The leg's input hash and how long a result may be reused, when its job opts in (W08-R10)
    /// and its run may reuse.
    async fn reuse_hash(&self, run: &str, spec: &LegSpec) -> Option<(String, u64)> {
        let reuse = super::reuse::spec_of(spec)?;
        let (repo, sha, actions, vars, trusted) = {
            let r = self.runs.get(run).filter(|r| !r.no_reuse)?;
            (r.repo.clone(), r.event.sha.clone(), r.actions.clone(), r.vars.clone(), r.trusted)
        };
        match super::reuse::inputs(&self.hub, &repo, &sha, spec, &reuse, &actions, &vars, trusted).await {
            Ok(inputs) => Some((kiln_ci::plan::reuse::hash(&inputs), reuse.max_age)),
            Err(why) => {
                tracing::warn!(leg = %spec.key, why = %why, "the leg's inputs can't be known; it runs");
                None
            }
        }
    }

    /// A leg that takes `source`'s result over (W08-R10): its copy in this run, and its report to
    /// the plan.
    async fn take_over(&mut self, run: &str, spec: LegSpec, source: &str, hash: &str, report: LegReport) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        super::reuse::take_over(&self.hub, run, source, &id, hash).await?;
        self.hub.emit_job(&id).await;
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id);
        r.seq += 1;
        let seq = r.seq;
        let files = Arc::clone(&r.files);
        let timing = Arc::clone(&r.timing);
        let tree = Arc::clone(&r.tree);
        let acts = r.plan.on_leg_finished(&spec.key, report.clone(), &Source { files: &files, timing: &timing, tree: &tree });
        runs::log(&self.hub, run, seq, "leg", Some(spec.key.clone()), Some(report_json(&report))).await?;
        self.actions(run, acts).await
    }

    /// Carries out what the scheduler decided.
    async fn commands(&mut self, cmds: Vec<Command>) {
        // One loop does them all. What a command makes as it runs (a cancel's settles, a settle's
        // next legs, their assignments) comes right after it, as if it had done it itself, rather
        // than a level down: a chain of them doesn't deepen the stack.
        if self.in_commands {
            self.next.extend(cmds);
            return;
        }
        self.in_commands = true;
        let mut todo: std::collections::VecDeque<Command> = cmds.into();
        while let Some(c) = todo.pop_front() {
            let r = match c {
                Command::StartRun { run } => {
                    let held = self.runs.get_mut(&run).and_then(|r| r.held.take()).unwrap_or_default();
                    self.actions(&run, held).await
                }
                Command::CancelRun { run, reason } => self.cancel_run(&run, &reason).await,
                Command::Assign { leg, runner, attempt, executor, image, docker, slot } => self.assign(&leg, &runner, attempt, Placement { executor, image, docker, slot }).await,
                Command::StartExecutor { pool, executor, runner, kind, image } => {
                    let ask = super::pools::start_ask(&self.hub, &pool, &executor, &kind, image.as_deref());
                    self.hub.send_to_agent(&runner, LANE_CONTROL, &ServerMsg::Executor { json: ask.to_string() }).await;
                    Ok(())
                }
                Command::StopExecutor { executor, runner } => {
                    let ask = serde_json::json!({ "op": "stop", "executor": executor });
                    self.hub.send_to_agent(&runner, LANE_CONTROL, &ServerMsg::Executor { json: ask.to_string() }).await;
                    Ok(())
                }
                Command::Cancel { leg, reason, .. } => {
                    if let Some(agent) = self.legs.get(&leg).and_then(|l| l.agent.clone()) {
                        self.hub.send_to_agent(&agent, LANE_CONTROL, &ServerMsg::Cancel { job: leg, reason }).await;
                    }
                    Ok(())
                }
                Command::Retry { leg, attempt, at } => {
                    if let Some(l) = self.legs.get_mut(&leg) {
                        l.attempt = attempt;
                        l.seq = 0;
                        l.agent = None;
                        l.report = LegReport::default();
                        l.class = None;
                    }
                    let note = format!("retrying: attempt {attempt} at {}", at / 1000);
                    let r = runs::requeue(&self.hub, &leg, attempt, Some(note)).await;
                    self.hub.emit_job(&leg).await;
                    r
                }
                Command::Wait { leg, reason } => {
                    let r = runs::wait_reason(&self.hub, &leg, &serde_json::to_value(&reason).unwrap_or_default()).await;
                    self.hub.emit_job(&leg).await;
                    r
                }
                Command::Settled { leg, outcome, reason } => self.settled(&leg, outcome, reason).await,
                Command::Timer { at } => {
                    self.timers.insert(at);
                    Ok(())
                }
            };
            if let Err(e) = r {
                tracing::warn!(error = %format!("{e:#}"), "CI dispatcher command");
            }
            // What it left to do comes next, before the rest: as if it had done it itself.
            for c in std::mem::take(&mut self.next).into_iter().rev() {
                todo.push_front(c);
            }
        }
        self.in_commands = false;
    }

    /// Sends a leg to its runner (W05, W06).
    /// Fails `leg`'s attempt before it reaches a runner, saying why on the leg; what follows is
    /// the command loop's to do (`next`), as `assign` is only ever one of its commands.
    async fn fail_unstarted(&mut self, leg: &str, attempt: u32, kind: &'static str, why: String) -> anyhow::Result<()> {
        runs::annotation(&self.hub, leg, None, "error", Some(kind.into()), &why, None, None, None, None, None).await?;
        let failed = Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() });
        let cmds = self.sched.handle(Input::AttemptFinished { leg: leg.to_owned(), attempt, outcome: failed }, now_ms());
        self.next.extend(cmds);
        Ok(())
    }

    async fn assign(&mut self, leg: &str, runner: &str, attempt: u32, placed: Placement) -> anyhow::Result<()> {
        let Placement { executor, image, docker, slot } = placed;
        let Some(l) = self.legs.get(leg) else { return Ok(()) };
        let Some(r) = self.runs.get(&l.run) else { return Ok(()) };
        let (cache_mode, cache) = cache_access(&l.spec, r);
        let ttl = (i64::from(l.spec.timeout_minutes) + 60) * 60;
        let token = super::token::mint(&self.hub, &super::token::Leg { leg, run: &l.run, repo: &r.repo.id, cache, cache_mode: cache_mode.as_str(), ttl });
        // GITHUB_TOKEN: a job token with the job's permissions, revoked when the attempt ends
        // (W10-R03, R04).
        let job_token = super::token::mint_job(&self.hub, leg, &l.run, &r.repo.id, &l.spec.permissions, ttl).await?;
        // `github_api: github` (W14-R12): the jobs of a repository on GitHub that act on GitHub
        // itself get an installation token of the App, limited to the job's permissions and the
        // repository, as GITHUB_TOKEN, and GitHub's URLs.
        let on_github = github_api(&self.hub, &r.repo).await;
        // On a task of its own: GitHub is another host, and this is deep in the dispatcher's
        // calls, where a debug build's shard stack runs out.
        let github_token = match &on_github {
            Some((app, name)) => {
                let (app, name, perms) = (Arc::clone(app), name.clone(), crate::githost::github::permissions_of(&l.spec.permissions.0));
                Some(crate::rt::spawn(async move { app.token(&name, &perms).await }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("the token request ended without an answer"))))
            }
            None => None,
        };
        let job_token = match github_token {
            Some(token) => match token {
                Ok(t) => t,
                Err(e) => return self.fail_unstarted(leg, attempt, "github_api", format!("`github_api: github`: no GitHub token for the job: {e:#}")).await,
            },
            None => job_token,
        };
        let (secrets, used_ids, vars, unmade) = self.leg_secrets(l, r, &job_token).await;
        // Brokered secrets that couldn't be made (W09-R08) and secrets withheld from whoever
        // started the run (W10-R02): the leg goes on without them, told why.
        for why in &unmade {
            runs::annotation(&self.hub, leg, None, "warning", Some("secrets".into()), why, None, None, None, None, None).await?;
        }
        let secret_env = secret_outputs_for(&self.hub, l, r).await;
        let mut a = self.assignment(leg, l, r, attempt, executor.clone(), image, token, job_token, cache_mode, secrets, vars);
        // An action it uses that couldn't be resolved (W07-R03) fails it as it starts, as GitHub
        // fails a job at "Set up job"; the run's other jobs go on.
        if let Some(m) = l.spec.def().and_then(|j| unresolved(&j.steps, &r.actions)) {
            return self.fail_unstarted(leg, attempt, "actions", m).await;
        }
        // KCI4031 (W07-R11): a kiln action that needs more permissions than the job grants.
        if let Some(m) = l.spec.def().and_then(|j| short_permissions(&j.steps, &r.actions, &l.spec.permissions)) {
            return self.fail_unstarted(leg, attempt, "permissions", m).await;
        }
        if let Some((app, _)) = &on_github {
            a.runtime.facade_url = app.web.clone();
            a.runtime.api_url = app.api.clone();
            a.runtime.graphql_url = graphql_of(&app.api);
        }
        // Its integrations (W11-R03); one it may not use fails it before it starts.
        match self.leg_integrations(l, r, a.runtime.id_token_url.is_some()).await {
            Ok(list) => a.integrations = list,
            Err(m) => return self.fail_unstarted(leg, attempt, "integrations", m).await,
        }
        // Its deployment (W15-R04), unless `deployment: false`.
        let deploys = l.deployment.is_none() && l.spec.environment.as_ref().is_some_and(|e| e.deployment && !e.name.is_empty());
        let deployment = if deploys {
            let env = l.spec.environment.as_ref().map(|e| e.name.clone()).unwrap_or_default();
            // As GitHub records a job's deployment: the ref's short name.
            let git_ref = r.github.get("ref_name").or_else(|| r.github.get("ref")).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
            let actor = r.github.get("actor").and_then(serde_json::Value::as_str).unwrap_or("github-actions").to_owned();
            super::environments::started(&self.hub, &r.repo, &env, &l.run, leg, &r.event.sha, &git_ref, &actor).await
        } else {
            None
        };
        // Docker inside the `docker` executor's container: the host's socket, for trusted runs
        // only (W13-R04).
        if let Some(d) = docker.filter(|_| r.trusted && a.executor.kind == "docker") {
            a.executor.options = serde_json::json!({ "docker": d });
        }
        a.executor.slot = slot;
        // Needed jobs' secret outputs, as masked environment only (W04-R22).
        for (k, v) in secret_env {
            a.masks.push(v.clone());
            a.env_from.push((k, v));
        }
        let json = serde_json::to_string(&a)?;
        if let Err(e) = super::secrets::record_uses(&self.hub, leg, used_ids).await {
            tracing::warn!(leg, error = %format!("{e:#}"), "secret uses could not be recorded");
        }
        runs::claim(&self.hub, leg, runner, executor.as_deref().unwrap_or("host"), attempt).await?;
        // The run's first leg to start: `workflow_run` `in_progress`.
        if let Some(run) = self.legs.get(leg).map(|l| l.run.clone()) {
            let first = self.runs.get_mut(&run).is_some_and(|r| !std::mem::replace(&mut r.in_progress, true));
            if first && let Some(r) = self.runs.get(&run) {
                self.workflow_run(r, "in_progress", None);
            }
        }
        {
            let mut g = self.hub.agents.lock();
            if let Some(ag) = g.get_mut(runner) {
                ag.running.push(leg.to_owned());
                ag.busy = u32::try_from(ag.running.len()).unwrap_or(u32::MAX);
            }
        }
        if let Some(l) = self.legs.get_mut(leg) {
            l.agent = Some(runner.to_owned());
            l.seq = 0;
            if deployment.is_some() {
                l.deployment = deployment;
            }
        }
        let _rx = self.hub.logs.subscribe(leg);
        tracing::info!(leg, runner, attempt, "leg assigned");
        if !self.hub.send_to_agent(runner, LANE_CONTROL, &ServerMsg::AssignV2 { json }).await {
            self.hub.release_slot(runner, leg);
            if let Some(l) = self.legs.get_mut(leg) {
                l.agent = None;
            }
            runs::requeue(&self.hub, leg, attempt, Some("the assignment could not be delivered".into())).await?;
            let cmds = self.sched.handle(Input::Rejected { leg: leg.to_owned() }, now_ms());
            Box::pin(self.commands(cmds)).await;
        }
        self.hub.emit_job(leg).await;
        Ok(())
    }

    /// What a leg is given (W09-R04, W09-R05): the secrets its expressions read, resolved by scope
    /// and followed through the calls that brought it, plus `GITHUB_TOKEN`. Untrusted runs get
    /// only the token. Also returns the secrets' rows (for their uses) and the leg's `vars`
    /// (its environment's included).
    async fn leg_secrets(&self, l: &LegSt, r: &RunSt, token: &str) -> (Vec<wire::Secret>, Vec<String>, serde_json::Value, Vec<String>) {
        let mut unmade = Vec::new();
        let spec = &l.spec;
        let env = spec.environment.as_ref().map(|e| e.name.clone());
        let vars = match &env {
            Some(e) => match super::secrets::vars_for(&self.hub, &r.repo, &r.path, Some(e)).await {
                Ok(mut v) => {
                    v.extend(r.given.vars.iter().map(|(k, x)| (k.clone(), serde_json::Value::String(x.clone()))));
                    serde_json::Value::Object(v)
                }
                Err(_) => r.vars.clone(),
            },
            None => r.vars.clone(),
        };
        let token_secret = wire::Secret { name: "GITHUB_TOKEN".into(), value: token.to_owned() };
        if !r.trusted && r.given.secrets.is_empty() {
            return (vec![token_secret], Vec::new(), vars, unmade);
        }
        let mut used = spec.def().map(kiln_ci::secrets::used).unwrap_or_default();
        // A kiln action's declared secrets: the job's of that name, or the one it maps (W09-R07).
        if let Some(j) = spec.def() {
            used.names.extend(declared_sources(&j.steps, &r.actions).into_iter().map(|(_, _, from, _)| from));
        }
        let needed = kiln_ci::secrets::needed(&spec.secrets, &used);
        // An untrusted run gets none of the server's secrets: only those given with it (W17-R06).
        let mut resolved = if !r.trusted {
            Default::default()
        } else {
            let text = |k: &str| r.github.get(k).and_then(serde_json::Value::as_str);
            let w = kiln_ci::oidc::Workload { repository: &r.repo.name, environment: env.as_deref(), git_ref: text("ref"), sha: Some(&r.event.sha), actor: text("actor").unwrap_or("kiln"), source: "job", app: None, deployment: None, trigger: Some(&l.run) };
            match super::secrets::for_leg(&self.hub, &r.repo, &r.path, env.as_deref(), &needed, &w).await {
                Ok((x, failed)) => {
                    for (name, why) in failed {
                        tracing::warn!(leg = %spec.key, secret = %name, error = %why, "a brokered secret couldn't be made; the leg goes without it");
                        unmade.push(format!("secret {name} couldn't be made {why}"));
                    }
                    x
                }
                Err(e) => {
                    tracing::warn!(leg = %spec.key, error = %format!("{e:#}"), "secrets could not be read; the leg gets none");
                    Default::default()
                }
            }
        };
        // W10-R02's `secrets.use`: of the rest, only what whoever started the run may use here (its
        // environment, else its workflow), the others withheld and said so.
        if let Some(by) = &r.event.started_by {
            let target = match &env {
                Some(e) => crate::authz::Scope::Environment { repo: r.repo.name.clone(), environment: e.clone() },
                None => crate::authz::Scope::Workflow { repo: r.repo.name.clone(), path: r.path.clone() },
            };
            let who = by.get("name").and_then(serde_json::Value::as_str).unwrap_or("whoever started the run").to_owned();
            for name in crate::authz::gate::withheld_secrets(&self.hub, by, resolved.keys().cloned().collect(), &target).await {
                resolved.remove(&name);
                unmade.push(format!("secret {name} withheld: {who} may not use it here (`secrets.use:{name}`)"));
            }
        }
        let mut run: std::collections::BTreeMap<String, String> = resolved.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect();
        run.extend(r.given.secrets.iter().map(|(k, v)| (k.clone(), v.clone())));
        run.insert("GITHUB_TOKEN".into(), token.to_owned());
        let given = kiln_ci::secrets::deliver(&spec.secrets, &used, &run);
        let ids: Vec<String> = resolved.values().filter_map(|v| v.id.clone()).collect();
        let mut out: Vec<wire::Secret> = given.into_iter().filter(|(k, _)| k != "GITHUB_TOKEN").map(|(name, value)| wire::Secret { name, value }).collect();
        out.push(token_secret);
        (out, ids, vars, unmade)
    }

    /// The integrations a leg authenticates to (W11-R03): its job's `integrations:`, its steps',
    /// and those its `kiln/auth@v1` steps name, each one its repository may use. Federating
    /// ones need ID tokens (`permissions: id-token: write`); untrusted runs get none.
    async fn leg_integrations(&self, l: &LegSt, r: &RunSt, id_tokens: bool) -> Result<Vec<wire::Integration>, String> {
        let Some(job) = l.spec.def() else { return Ok(Vec::new()) };
        let mut names: Vec<(String, bool)> = job.kiln.integrations.iter().map(|n| (n.clone(), true)).collect();
        for step in &job.steps {
            names.extend(step.kiln.integrations.iter().map(|n| (n.clone(), false)));
            if step.uses.as_deref().and_then(crate::runner::builtins::builtin).is_some_and(|b| b.name == "auth") {
                let name = step.with.iter().find(|(k, _)| k == "integration").and_then(|(_, v)| v.as_str()).unwrap_or_default();
                if name.is_empty() || name.contains("${{") {
                    return Err("kiln/auth@v1 names its integration (`with: integration: <name>`), not an expression".into());
                }
                names.push((name.to_owned(), false));
            }
        }
        // A platform environment's integration, for the jobs deploying there (W15-R02).
        if let Some(env) = l.spec.environment.as_ref().map(|e| e.name.clone()).filter(|n| !n.is_empty())
            && let Ok(def) = super::environments::ensure(&self.hub, &r.repo, &env).await
            && def.kind.as_deref() == Some("platform")
            && let Some(i) = def.targets.integration.filter(|i| !i.is_empty())
        {
            names.push((i, true));
        }
        if names.is_empty() {
            return Ok(Vec::new());
        }
        if !r.trusted {
            return Err("untrusted runs (from forks) get no integrations".into());
        }
        let mut out: Vec<wire::Integration> = Vec::new();
        for (name, whole) in names {
            if let Some(have) = out.iter_mut().find(|i| i.name == name) {
                have.job |= whole;
                continue;
            }
            let i = super::integrations::get(&self.hub, &name).await.map_err(|e| format!("{e:#}"))?.filter(|i| super::integrations::usable_by(i, &r.repo)).ok_or_else(|| format!("no integration `{name}` that {} may use", r.repo.name))?;
            let mode = super::integrations::kind(&i.kind).map(|k| k.mode);
            let oidc = mode == Some(super::integrations::Mode::Oidc);
            if oidc && !id_tokens {
                return Err(format!("`{name}` ({}) federates with kiln's ID token: the job needs `permissions: id-token: write`", i.kind));
            }
            let token = if oidc { None } else { super::integrations::token(&self.hub, &i).await.map_err(|e| format!("`{name}`'s token: {e:#}"))? };
            if mode == Some(super::integrations::Mode::Token) && token.is_none() {
                return Err(format!("`{name}` has no token stored"));
            }
            out.push(wire::Integration { name, kind: i.kind.clone(), mode: if oidc { "oidc".into() } else { "token".into() }, config: i.config.clone(), token, job: whole });
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn assignment(&self, leg: &str, l: &LegSt, r: &RunSt, attempt: u32, executor: Option<String>, image: Option<String>, token: String, job_token: String, cache_mode: kiln_ci::store::cache::Mode, secrets: Vec<wire::Secret>, vars: serde_json::Value) -> AssignmentV2 {
        let spec = &l.spec;
        let wf = &spec.workflow;
        let job = spec.def();
        let id_tokens = spec.permissions.level("id-token") == kiln_ci::plan::perms::Level::Write && (r.trusted || self.hub.cfg.ci.oidc.untrusted);
        let num = |k: &str| spec.strategy.get(k).and_then(serde_json::Value::as_f64).map_or(0, |v| v as u32);
        let mut env_from = spec.env_from.clone();
        env_from.extend(spec.run_env.iter().map(|(k, v)| (format!("KILN_RUN_{k}"), v.clone())));
        AssignmentV2 {
            job_id: leg.to_owned(),
            run_id: l.run.clone(),
            attempt,
            repo: r.repo.name.clone(),
            sha: r.event.sha.clone(),
            git_ref: r.event.git_ref.clone(),
            dialect: if wf.dialect == Some(kiln_ci::expr::Dialect::Kiln) { "kiln".into() } else { "gha".into() },
            leg: wire::LegInfo { job_id: spec.job.clone(), name: spec.name.clone(), matrix: spec.matrix.clone(), index: num("job-index"), total: num("job-total") },
            executor: wire::ExecutorSpec { image: if executor.as_deref() == Some("docker") { image } else { None }, kind: executor.unwrap_or_else(|| "host".into()), options: serde_json::Value::Null, slot: None },
            container: spec.container.clone(),
            services: spec.services.iter().cloned().collect(),
            job_container: job.and_then(|j| j.container.clone()),
            job_services: job.map(|j| j.services.clone()).unwrap_or_default(),
            // The kiln dialect's job-level caches come first (W08-R09); after the job's own steps,
            // its discovered test reports (W16-R01), v1's `artifacts:` (W08-R08), its `attest:`
            // (W18-R01), then its `deploy:` (W15-R05).
            steps: job
                .map(|j| {
                    kiln_ci::store::cache::job_steps(&j.kiln.cache)
                        .into_iter()
                        .chain(j.steps.iter().cloned())
                        .chain(j.kiln.test_results.as_ref().and_then(kiln_ci::results::discover::job_step))
                        .chain(kiln_ci::store::v1_artifacts_step(&j.kiln.artifacts))
                        .chain(j.kiln.attest.as_ref().and_then(kiln_ci::attest::job_step))
                        .chain(j.kiln.deploy.as_ref().and_then(kiln_ci::env::deploy_step))
                        .collect()
                })
                .unwrap_or_default(),
            workflow_env: wf.env.clone(),
            job_env: job.map(|j| j.env.clone()).unwrap_or_default(),
            workflow_defaults: wf.defaults.clone(),
            job_defaults: job.map(|j| j.defaults.clone()).unwrap_or_default(),
            outputs: job.map(|j| j.outputs.iter().map(|o| (o.name.clone(), o.value.clone())).collect()).unwrap_or_default(),
            environment_url: job.and_then(|j| j.environment.as_ref()).and_then(|e| e.url.clone()),
            contexts: wire::Contexts {
                github: r.github.clone(),
                vars,
                needs: spec.needs.clone(),
                strategy: spec.strategy.clone(),
                matrix: spec.matrix.clone(),
                inputs: spec.inputs.clone(),
                run: serde_json::json!({ "env": spec.run_env.iter().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect::<serde_json::Map<String, serde_json::Value>>() }),
            },
            secrets,
            masks: Vec::new(),
            env_from,
            items: spec.items.clone(),
            timeout_minutes: spec.timeout_minutes,
            continue_on_error: spec.continue_on_error,
            runtime: wire::Runtime {
                // The results service (Twirp at the origin's root) and the cache as GHES serves
                // it (`_apis/artifactcache/` under the root), W08.
                results_url: "/".into(),
                runtime_token: token.clone(),
                cache_url: "/".into(),
                // ID tokens only with `permissions: id-token: write` (W10-R07, GitHub-exact).
                id_token_url: id_tokens.then(|| "/_kiln/oidc/token?api-version=1".to_owned()),
                id_token_request_token: id_tokens.then(|| token.clone()),
                // The code host: kiln's mirror, which the leg's token reads (W14), at the
                // server's root (`actions/checkout` keeps only the origin).
                facade_url: "/".into(),
                // GitHub's API as GHES serves it (W14-R11).
                api_url: "/api/v3".into(),
                graphql_url: "/api/graphql".into(),
                github_token: job_token,
                cache_mode: Some(cache_mode.as_str().into()),
                registry_url: "/".into(),
                // The package proxies jobs go through by themselves (W12-R04).
                proxies: self.hub.cfg.ci.registry.proxies.iter().filter(|p| p.jobs).map(|p| (p.ecosystem.clone(), format!("/_kiln/proxy/{}", p.name.clone().unwrap_or_else(|| p.ecosystem.clone())))).collect(),
                // Images from the registries kiln mirrors come through it (W12-R02).
                mirrors: self.hub.cfg.ci.registry.mirrors.iter().map(|m| (m.upstream.clone(), m.prefix())).collect(),
                // Its certificate authority when it serves HTTPS (on its own port, or behind an
                // `https://` address): GitHub Enterprise Server's clients go `https://` to the
                // host they're given whatever its scheme (§2.6).
                server_ca: (self.hub.cfg.server.https || self.hub.cfg.server.public_url.starts_with("https://")).then(|| self.hub.pki.ca_pem.clone()),
            },
            checkout: if wf.checkout && job.is_none_or(|j| j.kiln.checkout != Some(false)) { "auto".into() } else { "none".into() },
            actions: r.actions.clone(),
            debug: wire::Debug { runner_debug: r.debug, step_debug: r.debug, keep_on_failure_minutes: keep_minutes(job.and_then(|j| j.kiln.debug.as_ref())).or(r.keep_failed) },
            exports_env: job.map(|j| j.kiln.exports_env.clone()).unwrap_or_default(),
            secret_outputs: job.map(|j| j.outputs.iter().filter(|o| o.secret).map(|o| o.name.clone()).collect()).unwrap_or_default(),
            run_env: wf.run_env,
            quarantine: job.is_some_and(|j| j.kiln.quarantine_enforce),
            integrations: Vec::new(),
            limits: wire::Limits::default(),
            network: egress_of(job.and_then(|j| j.kiln.network.as_ref()), r.trusted, &self.hub.cfg.ci.untrusted.allow_hosts, leg),
        }
    }

    /// What a runner says about a leg while it runs (W06, §2.5).
    async fn event(&mut self, agent: &str, leg: &str, json: &str) -> anyhow::Result<()> {
        if let (Ok(Seq { seq: Some(seq) }), Some(l)) = (serde_json::from_str::<Seq>(json), self.legs.get_mut(leg)) {
            if seq <= l.seq {
                return Ok(());
            }
            l.seq = seq;
        }
        let e: JobEventV2 = serde_json::from_str(json)?;
        match e {
            JobEventV2::StepStarted { idx, name, kind, action } => {
                runs::step_started(&self.hub, leg, idx, &name, &kind, action).await?;
                self.hub.logs.step(leg, idx, Status::Running.as_str());
                self.hub.emit_job(leg).await;
            }
            JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, outputs } => {
                let status = match conclusion {
                    wire::Conclusion::Success => {
                        if outcome == wire::Conclusion::Failure {
                            Status::Failed
                        } else {
                            Status::Succeeded
                        }
                    }
                    wire::Conclusion::Failure => Status::Failed,
                    wire::Conclusion::Cancelled => Status::Cancelled,
                    wire::Conclusion::Skipped => Status::Skipped,
                };
                let word = |c: wire::Conclusion| serde_json::to_value(c).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
                runs::step_finished(&self.hub, leg, idx, status, &word(outcome), &word(conclusion), exit_code, serde_json::to_string(&outputs).unwrap_or_default()).await?;
                self.hub.logs.step(leg, idx, status.as_str());
                self.hub.emit_job(leg).await;
            }
            JobEventV2::Annotation { idx, level, message, title, file, line, end_line, col, end_col, .. } => {
                runs::annotation(&self.hub, leg, Some(idx), &level, title, &message, file, line, end_line, col, end_col).await?;
            }
            JobEventV2::Summary { idx, markdown } => runs::summary(&self.hub, leg, idx, &markdown).await?,
            JobEventV2::JobOutputs { outputs, json_outputs, env, run_env, secret_outputs, secret_values, environment_url } => {
                // Secret outputs are sealed the moment they arrive (W04-R22, W09-R10).
                if !secret_values.is_empty()
                    && let Some(run) = self.legs.get(leg).map(|l| l.run.clone())
                {
                    seal_secret_outputs(&self.hub, &run, leg, &secret_values).await?;
                }
                if let Some(l) = self.legs.get_mut(leg) {
                    l.env_url = environment_url.filter(|u| !u.is_empty());
                    l.report.outputs = outputs.into_iter().collect();
                    l.report.json_outputs = json_outputs.into_iter().collect();
                    l.report.env = env.into_iter().collect();
                    l.report.run_env = run_env;
                    l.report.secret_outputs = secret_outputs;
                }
            }
            JobEventV2::Kept { until } => {
                runs::kept(&self.hub, leg, Some(until)).await?;
                self.hub.ci.keep(leg, Some((agent.to_owned(), until)));
            }
            JobEventV2::FailureClass { class } => {
                if let Some(l) = self.legs.get_mut(leg) {
                    l.class = FailureClass::parse(&class);
                }
            }
            // W16-R07, R08: benchmark metrics, each compared with its baseline.
            JobEventV2::Metrics { idx, points, fingerprint } => {
                if let Err(e) = super::results::metrics(&self.hub, leg, idx, &points, &fingerprint).await {
                    tracing::warn!(leg, error = %format!("{e:#}"), "metrics could not be kept");
                }
            }
            // W18-R01: the attest step's statements, made from the server's own records and signed.
            JobEventV2::Attest { idx, nonce, subjects, provenance, sbom } => {
                if let Err(e) = super::attest::reported(&self.hub, leg, idx, &nonce, subjects, provenance, sbom).await {
                    tracing::warn!(leg, error = %format!("{e:#}"), "attestations could not be made");
                }
            }
            // W13-R11: an image the leg built into kiln's registry, registered for pools and rules.
            JobEventV2::ImageBuilt { name, reference, digest, platforms, .. } => {
                if let Err(e) = super::images::built(&self.hub, leg, &name, &reference, &digest, &platforms).await {
                    tracing::warn!(leg, error = %format!("{e:#}"), "a built image could not be registered");
                }
            }
            JobEventV2::Group { .. } | JobEventV2::Mask { .. } | JobEventV2::TestReport { .. } => {}
        }
        Ok(())
    }

    /// A leg's attempt ended on its runner: the scheduler decides on a retry, else the plan hears.
    async fn finished(&mut self, agent: &str, r: JobResult) -> anyhow::Result<()> {
        self.hub.release_slot(agent, &r.job);
        // A result sent again (its receipt was lost with the connection), or of a leg given up
        // while its runner was away: the leg has gone on without it.
        if self.legs.get(&r.job).is_some_and(|l| l.agent.as_deref() != Some(agent)) {
            return Ok(());
        }
        self.hub.logs.finish(&r.job);
        // The attempt's GITHUB_TOKEN dies with it (W10-R03).
        if let Err(e) = super::token::revoke_leg(&self.hub, &r.job).await {
            tracing::warn!(leg = %r.job, error = %format!("{e:#}"), "the job token could not be revoked");
        }
        let Some(l) = self.legs.get(&r.job) else { return Ok(()) };
        let attempt = l.attempt;
        let gate = l.spec.def().and_then(|j| j.kiln.bench_gate.clone());
        let mut outcome = match r.status {
            Status::Succeeded | Status::Skipped => Outcome::Success,
            Status::Cancelled => Outcome::Cancelled,
            Status::TimedOut => Outcome::Failed(Failed { class: FailureClass::Timeout, exit_code: r.exit_code, log_hits: Vec::new() }),
            _ => {
                // The retry policy's `log:` patterns the attempt's log has (W05-R14).
                let patterns: Vec<String> = l.spec.def().and_then(|j| j.kiln.retry.as_ref()).and_then(RetryPolicy::from_yval).map(|p| p.on.into_iter().filter_map(|o| if let RetryOn::Log(s) = o { Some(s) } else { None }).collect()).unwrap_or_default();
                let class = l.class.unwrap_or(FailureClass::Failure);
                let log_hits = if patterns.is_empty() { Vec::new() } else { log_hits(&self.hub, &r.job, &patterns).await };
                Outcome::Failed(Failed { class, exit_code: r.exit_code, log_hits })
            }
        };
        // W16-R09: a job's bench-gate fails (or warns about) a leg whose metrics got worse.
        if let (Outcome::Success, Some(g)) = (&outcome, gate) {
            match kiln_ci::results::metrics::Gate::from_yval(&g) {
                Ok(g) => match super::results::gate(&self.hub, &r.job, &g).await {
                    Ok(Some((true, why))) => {
                        super::results::annotate(&self.hub, &r.job, "error", "bench-gate", &why).await;
                        outcome = Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() });
                    }
                    Ok(Some((false, why))) => super::results::annotate(&self.hub, &r.job, "warning", "bench-gate", &why).await,
                    Ok(None) => {}
                    Err(e) => tracing::warn!(leg = %r.job, error = %format!("{e:#}"), "the bench-gate could not be judged"),
                },
                Err(e) => super::results::annotate(&self.hub, &r.job, "warning", "bench-gate", &e).await,
            }
        }
        if let Some(l) = self.legs.get_mut(&r.job) {
            l.agent = None;
            if let Some(e) = &r.error {
                l.report.outputs.retain(|_| true);
                tracing::info!(leg = %r.job, error = %e, "leg ended with an error");
            }
        }
        let (conclusion, class) = match &outcome {
            Outcome::Success => ("success", None),
            Outcome::Cancelled => ("cancelled", None),
            Outcome::Failed(f) => (if f.class == FailureClass::Timeout { "timed_out" } else { "failure" }, Some(f.class.as_str())),
        };
        let end = runs::AttemptEnd { conclusion, class, exit_code: r.exit_code, error: r.error.clone(), peak_rss: r.peak_rss, cpu_seconds: r.cpu_seconds };
        if let Err(e) = runs::attempt_finished(&self.hub, &r.job, attempt, end).await {
            tracing::warn!(leg = %r.job, error = %format!("{e:#}"), "the attempt could not be recorded");
        }
        let cmds = self.sched.handle(Input::AttemptFinished { leg: r.job.clone(), attempt, outcome }, now_ms());
        self.commands(cmds).await;
        Ok(())
    }

    /// The leg is done for good: its row, then its plan (W04-R02).
    async fn settled(&mut self, leg: &str, outcome: Outcome, reason: Option<String>) -> anyhow::Result<()> {
        let Some(l) = self.legs.remove(leg) else { return Ok(()) };
        // A deployment ends here: the next its environment holds may go (W15-R01).
        if let Some((env, repo)) = l.spec.environment.as_ref().map(|e| e.name.clone()).zip(self.runs.get(&l.run).map(|r| r.repo.id.clone())) {
            Box::pin(self.next_deployments(&repo, &env)).await?;
        }
        let c = match &outcome {
            Outcome::Success => PlanC::Success,
            Outcome::Cancelled => PlanC::Cancelled,
            Outcome::Failed(f) if f.class == FailureClass::Timeout => PlanC::TimedOut,
            Outcome::Failed(_) => PlanC::Failure,
        };
        let outputs = serde_json::to_string(&l.report.outputs.iter().cloned().collect::<std::collections::BTreeMap<_, _>>()).ok();
        // Its deployment's last status (W15-R04).
        if let Some(d) = l.deployment {
            let state = match c {
                PlanC::Success => "success",
                PlanC::Cancelled => "inactive",
                PlanC::Failure | PlanC::TimedOut => "failure",
                _ => "error",
            };
            if let Err(e) = super::environments::status(&self.hub, d, state, l.env_url.clone(), None, "github-actions[bot]").await {
                tracing::warn!(leg = %leg, error = %format!("{e:#}"), "a deployment's status could not be recorded");
            }
            // W19-R06: `deploy.succeeded` or `deploy.failed`.
            if matches!(c, PlanC::Success | PlanC::Failure | PlanC::TimedOut)
                && let Some(env) = l.spec.environment.as_ref().map(|e| e.name.clone())
            {
                crate::rt::spawn(super::notify::deployed(Arc::clone(&self.hub), l.run.clone(), env, c == PlanC::Success));
            }
        }
        runs::finish_leg(&self.hub, leg, status_of(c), c.result(), outputs, reason).await?;
        self.hub.emit_job(leg).await;
        let report = LegReport { conclusion: Some(c), ..l.report };
        // W08-R10: a success of a job that opted in is kept for the legs with its inputs.
        if c == PlanC::Success
            && let Ok(Some(hash)) = super::reuse::hash_of(&self.hub, leg).await
            && let Err(e) = super::reuse::remember(&self.hub, leg, &l.run, &hash, &report_json(&report)).await
        {
            tracing::warn!(leg = %leg, error = %format!("{e:#}"), "a result couldn't be kept for reuse");
        }
        let Some(run) = self.runs.get_mut(&l.run) else { return Ok(()) };
        run.seq += 1;
        let seq = run.seq;
        let files = Arc::clone(&run.files);
        let timing = Arc::clone(&run.timing);
        let tree = Arc::clone(&run.tree);
        let acts = run.plan.on_leg_finished(&l.key, report.clone(), &Source { files: &files, timing: &timing, tree: &tree });
        runs::log(&self.hub, &l.run, seq, "leg", Some(l.key.clone()), Some(report_json(&report))).await?;
        self.actions(&l.run, acts).await
    }

    async fn cancel_run(&mut self, run: &str, reason: &str) -> anyhow::Result<()> {
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        if r.held.is_some() {
            // It never started: it ends here.
            r.held = None;
            let acts = vec![PlanAction::Finish(PlanC::Cancelled)];
            return Box::pin(self.actions(run, acts)).await;
        }
        r.seq += 1;
        let seq = r.seq;
        let files = Arc::clone(&r.files);
        let timing = Arc::clone(&r.timing);
        let tree = Arc::clone(&r.tree);
        let acts = r.plan.on_cancel(&Source { files: &files, timing: &timing, tree: &tree });
        runs::log(&self.hub, run, seq, "cancel", None, Some(serde_json::json!({ "reason": reason }).to_string())).await?;
        Box::pin(self.actions(run, acts)).await
    }

    /// Rebuilds unfinished runs by replaying their plans' logs (W05-R25); legs that were running
    /// wait for their runner to say it still has them.
    async fn restore(&mut self) -> anyhow::Result<()> {
        let rows: Vec<(String, String, String)> = self.hub.store.call(|c| {
            let mut st = c.prepare("SELECT p.run_id, p.workflow, p.input FROM ci_plans p WHERE p.done = 0")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
        for (run, model, input) in rows {
            let r = self.replay(&run, &model, &input).await;
            if let Err(e) = r {
                tracing::warn!(run, error = %format!("{e:#}"), "a CI run could not be rebuilt; it fails");
                runs::finish_run(&self.hub, &run, Status::Failed, "failure", Some(format!("The run could not be rebuilt after a restart: {e:#}"))).await?;
            }
        }
        Ok(())
    }

    async fn replay(&mut self, run: &str, model: &str, input: &str) -> anyhow::Result<()> {
        let wf: Arc<Workflow> = Arc::new(serde_json::from_str(model)?);
        let v: serde_json::Value = serde_json::from_str(input)?;
        let changed: Option<Vec<String>> = v.get("changed").and_then(|c| serde_json::from_value(c.clone()).ok());
        let input = RunInput {
            github: v.get("github").cloned().unwrap_or_default(),
            inputs: v.get("inputs").cloned().unwrap_or_default(),
            vars: v.get("vars").cloned().unwrap_or_default(),
            changed: changed.map(Arc::new),
            trusted: v.get("trusted").and_then(serde_json::Value::as_bool).unwrap_or(false),
        };
        let row = self.hub.store.run(run.to_owned()).await?.ok_or_else(|| anyhow::anyhow!("no run"))?;
        let repo = self.hub.store.repos().await?.into_iter().find(|r| r.id == row.repo_id).ok_or_else(|| anyhow::anyhow!("no repository"))?;
        let files = Arc::new(super::reusable::files(&self.hub, &repo, &row.sha, &wf).await);
        let timing = saved_timing(&v);
        let tree = saved_tree(&v);
        let (mut plan, initial) = RunPlan::start(Arc::clone(&wf), input.clone(), self.settings_with(&v), &Source { files: &files, timing: &timing, tree: &tree }).map_err(|e| anyhow::anyhow!(e))?;
        let run_owned = run.to_owned();
        let (log, last_seq): (Vec<(i64, String, Option<String>, Option<String>)>, i64) = self.hub.store.call(move |c| {
            let mut st = c.prepare("SELECT seq, kind, leg, report FROM ci_plan_log WHERE run_id = ?1 AND attempt = (SELECT attempt FROM runs WHERE id = ?1) ORDER BY seq")?;
            let log = st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            // The log goes on across attempts: an attempt that logged nothing yet continues from
            // the last attempt's end.
            let last: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) FROM ci_plan_log WHERE run_id = ?1", [&run_owned], |r| r.get(0))?;
            Ok((log, last))
        })
        .await?;
        let seq = last_seq;
        let logged = !log.is_empty();
        let mut reported: HashSet<String> = HashSet::new();
        for (_, kind, leg, report) in log {
            match kind.as_str() {
                "cancel" => {
                    let _ = plan.on_cancel(&Source { files: &files, timing: &timing, tree: &tree });
                }
                _ => {
                    let rep = report.as_deref().map(report_from_json).unwrap_or_default();
                    let key = leg.unwrap_or_default();
                    let _ = plan.on_leg_finished(&key, rep, &Source { files: &files, timing: &timing, tree: &tree });
                    reported.insert(key);
                }
            }
        }
        // The legs as the store has them.
        let run_owned = run.to_owned();
        type Row = (String, String, String, Option<String>, u32, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Row> = self.hub.store.call(move |c| {
            let mut st = c.prepare("SELECT id, key, status, agent, attempt, leg, conclusion, outputs FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1)")?;
            Ok(st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
        // Legs that ended before their report reached the log (the server stopped in between):
        // their result goes to the plan now, so the run goes on.
        let waiting: HashSet<String> = plan.jobs().iter().flat_map(|j| j.legs.iter()).filter(|l| l.state != "done").map(|l| l.key.clone()).collect();
        let late: Vec<(String, LegReport)> = rows
            .iter()
            .filter(|r| Status::parse(&r.2).is_terminal() && !reported.contains(&r.1) && waiting.contains(&r.1))
            .map(|r| {
                let outputs: Vec<(String, String)> = r.7.as_deref().and_then(|o| serde_json::from_str::<std::collections::BTreeMap<String, String>>(o).ok()).map(|m| m.into_iter().collect()).unwrap_or_default();
                (r.1.clone(), LegReport { conclusion: Some(conclusion_of(r.6.as_deref())), outputs, ..LegReport::default() })
            })
            .collect();
        let legs: Vec<(String, String, String, Option<String>, u32, Option<String>)> = rows.into_iter().map(|r| (r.0, r.1, r.2, r.3, r.4, r.5)).collect();
        let event = v.get("event").and_then(|e| serde_json::from_value::<Event>(e.clone()).ok()).unwrap_or_else(|| Event { name: row.event.clone(), sha: row.sha.clone(), git_ref: input.github.get("ref").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(), trusted: input.trusted, ..Event::default() });
        let actions = v.get("actions").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default();
        let path = run_path(&input.github);
        let debug = v.get("debug").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let keep_failed = v.get("keep_failed").and_then(serde_json::Value::as_u64).and_then(|m| u32::try_from(m).ok());
        // A rerun's legs still to keep: those not in this attempt yet.
        let have: HashSet<&str> = legs.iter().map(|l| l.1.as_str()).collect();
        let keep: HashMap<String, (LegReport, String)> = v
            .get("keep")
            .and_then(serde_json::Value::as_object)
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| !have.contains(k.as_str()))
                    .filter_map(|(k, x)| Some((k.clone(), (report_from_json(x.get("report")?.as_str()?), x.get("leg")?.as_str()?.to_owned()))))
                    .collect()
            })
            .unwrap_or_default();
        let costs = super::insights::costs(&self.hub, &repo.id, &path).await;
        // Still waiting for a maintainer's approval (W18-R06): it starts from the plan's first
        // actions once approved.
        let unapproved = !logged && legs.is_empty() && runs::approval(&self.hub, run).await?.as_deref() == Some("required");
        let (held, awaiting) = if unapproved {
            let group = plan.concurrency.as_ref().map(|g| (format!("{}\0{}", repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
            (Some(initial), Some(Awaiting { group }))
        } else {
            (None, None)
        };
        let mut st = RunSt { plan, repo: repo.clone(), event, path, created: row.created_at, in_progress: !unapproved, files: Arc::clone(&files), wf: Arc::clone(&wf), github: input.github.clone(), vars: input.vars.clone(), trusted: input.trusted, actions, keys: HashMap::new(), held, seq, keep, debug, keep_failed, given: Given::default(), timing: Arc::clone(&timing), tree: Arc::clone(&tree), removed: 0, costs, awaiting, no_reuse: v.get("no_reuse").and_then(serde_json::Value::as_bool).unwrap_or(false) };
        let mut queued = Vec::new();
        for (id, key, status, agent, attempt, leg) in legs {
            st.keys.insert(key.clone(), id.clone());
            let s = Status::parse(&status);
            if s.is_terminal() {
                continue;
            }
            // The leg as the replayed plan made it (environment, permissions, concurrency, a
            // called workflow's steps and all), else as its row kept it.
            let spec = match st.plan.spec_of(&key) {
                Some(s) => s.clone(),
                None => {
                    let Some(v) = leg.and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok()) else { continue };
                    rebuild_spec(&wf, &v)
                }
            };
            self.hub.ci.legs.lock().insert(id.clone());
            self.legs.insert(id.clone(), LegSt { run: run.to_owned(), key, spec: spec.clone(), attempt: attempt.max(1), agent: agent.clone(), report: LegReport::default(), class: None, deployment: None, env_url: None, seq: 0 });
            let li = leg_in(&st, run, &id, &spec, &self.hub.cfg.ci);
            match (s, agent) {
                (Status::Running, Some(a)) => {
                    let _ = self.sched.handle(Input::LegRunning { leg: Box::new(li), runner: a.clone(), attempt: attempt.max(1) }, now_ms());
                    let mut g = self.hub.agents.lock();
                    if let Some(ag) = g.get_mut(&a) {
                        ag.running.push(id.clone());
                        ag.busy = u32::try_from(ag.running.len()).unwrap_or(u32::MAX);
                    }
                }
                _ => queued.push(li),
            }
        }
        self.hub.ci.runs.lock().insert(run.to_owned());
        let waits = st.awaiting.is_some();
        self.runs.insert(run.to_owned(), st);
        if waits {
            // Still waiting for a maintainer: nothing goes to the scheduler until then.
            return Ok(());
        }
        let _ = self.sched.handle(Input::RunQueued { run: run.to_owned(), group: None, supersede: None }, now_ms());
        for li in queued {
            // Held for its environment before the restart: held again, its wait counted from then.
            if let Some(env) = self.legs.get(&li.id).and_then(|l| l.spec.environment.as_ref()).map(|e| e.name.clone()).filter(|n| !n.is_empty()) {
                let id = li.id.clone();
                let ready_at = runs::queued_at(&self.hub, &id).await.unwrap_or_else(|_| crate::util::now_unix());
                // Announced before the server stopped, or not: once more at worst.
                self.held.insert(id.clone(), Held { leg: li, env, ready_at, announced: true });
                if let Err(e) = self.gate(&id).await {
                    tracing::warn!(leg = %id, error = %format!("{e:#}"), "an environment's protection could not be checked");
                }
                continue;
            }
            let cmds = self.sched.handle(Input::LegQueued(Box::new(li)), now_ms());
            self.commands(cmds).await;
        }
        for (key, report) in late {
            let Some(r) = self.runs.get_mut(run) else { break };
            r.seq += 1;
            let seq = r.seq;
            let files = Arc::clone(&r.files);
            let timing = Arc::clone(&r.timing);
            let tree = Arc::clone(&r.tree);
            let acts = r.plan.on_leg_finished(&key, report.clone(), &Source { files: &files, timing: &timing, tree: &tree });
            runs::log(&self.hub, run, seq, "leg", Some(key), Some(report_json(&report))).await?;
            self.actions(run, acts).await?;
        }
        Ok(())
    }
}

/// A leg's spec as the store kept it, joined again to its workflow (the model isn't stored per
/// leg).
fn rebuild_spec(wf: &Arc<Workflow>, v: &serde_json::Value) -> LegSpec {
    let job = v.get("job").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let local = job.rsplit('/').next().unwrap_or(&job).to_owned();
    let mut spec = placeholder_spec(wf, &local);
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    spec.key = s("key");
    spec.job = job;
    spec.name = s("name");
    spec.job_name = s("job_name");
    spec.matrix = v.get("matrix").cloned().unwrap_or_default();
    spec.strategy = v.get("strategy").cloned().unwrap_or_default();
    spec.needs = v.get("needs").cloned().unwrap_or_default();
    spec.inputs = v.get("inputs").cloned().unwrap_or_default();
    spec.runs_on = v.get("runs_on").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default();
    spec.requires = v.get("requires").and_then(|r| serde_json::from_value(r.clone()).ok()).unwrap_or_default();
    spec.timeout_minutes = v.get("timeout_minutes").and_then(serde_json::Value::as_u64).map_or(360, |t| u32::try_from(t).unwrap_or(360));
    spec.continue_on_error = v.get("continue_on_error").and_then(serde_json::Value::as_bool).unwrap_or(false);
    spec.container = v.get("container").and_then(|c| c.as_str().map(str::to_owned));
    spec.cache_cap = v.get("cache_cap").and_then(|c| c.as_str().map(str::to_owned));
    spec
}

/// A spec for a job with no legs (skipped, or failed while expanding), so the run page shows it.
fn placeholder_spec(wf: &Arc<Workflow>, job: &str) -> LegSpec {
    let local = job.rsplit('/').next().unwrap_or(job);
    let index = wf.jobs.iter().position(|j| j.id == local).unwrap_or(0);
    LegSpec {
        key: job.to_owned(),
        job: job.to_owned(),
        job_name: local.to_owned(),
        name: wf.jobs.get(index).and_then(|j| j.name.as_ref().map(|n| n.src.clone())).unwrap_or_else(|| local.to_owned()),
        matrix: serde_json::Value::Null,
        strategy: serde_json::Value::Null,
        needs: serde_json::Value::Null,
        inputs: serde_json::Value::Null,
        runs_on: plan::RunsOnSpec::default(),
        requires: Vec::new(),
        timeout_minutes: 360,
        continue_on_error: false,
        environment: None,
        concurrency: None,
        permissions: plan::perms::Resolved::default(),
        container: None,
        services: Vec::new(),
        secrets: plan::SecretsSpec::Run,
        secret_outputs: Vec::new(),
        env_from: Vec::new(),
        run_env: Vec::new(),
        items: Vec::new(),
        emulated: false,
        reads: Vec::new(),
        cache_cap: None,
        workflow: Arc::clone(wf),
        job_index: index,
    }
}

/// The AAD a secret output is sealed under: the leg and the name, so a value opens only there.
fn secret_output_aad(leg: &str, name: &str) -> Vec<u8> {
    format!("kiln secret output\0{leg}\0{name}").into_bytes()
}

/// A leg's secret outputs (W04-R22), sealed by the master key (W09-R10).
async fn seal_secret_outputs(hub: &Hub, run: &str, leg: &str, values: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for (name, value) in values {
        let (sealed, nonce) = super::secrets::seal_master(hub, value.as_bytes(), &secret_output_aad(leg, name))?;
        rows.push((name.clone(), sealed, nonce));
    }
    let (run, leg) = (run.to_owned(), leg.to_owned());
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            for (name, sealed, nonce) in rows {
                tx.execute(
                    "INSERT INTO ci_secret_outputs (run_id, leg_id, name, sealed, nonce) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(leg_id, name) DO UPDATE SET sealed = excluded.sealed, nonce = excluded.nonce",
                    rusqlite::params![run, leg, name, sealed, nonce],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The secret outputs of the jobs a leg needs, opened, as the environment it gets them in:
/// `KILN_NEEDS_<JOB>_<NAME>` (W04-R22).
async fn secret_outputs_for(hub: &Hub, l: &LegSt, r: &RunSt) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for so in &l.spec.secret_outputs {
        let Some(producer) = r.keys.get(&so.leg).cloned() else { continue };
        let name = so.name.clone();
        let found: Option<(Vec<u8>, Vec<u8>)> = hub
            .store
            .call({
                let (producer, name) = (producer.clone(), name.clone());
                move |c| Ok(c.query_row("SELECT sealed, nonce FROM ci_secret_outputs WHERE leg_id = ?1 AND name = ?2", rusqlite::params![producer, name], |x| Ok((x.get(0)?, x.get(1)?))).optional()?)
            })
            .await
            .unwrap_or_default();
        let Some((sealed, nonce)) = found else { continue };
        match super::secrets::open_master(hub, &sealed, &nonce, &secret_output_aad(&producer, &name)) {
            Ok(v) => {
                let var = format!("KILN_NEEDS_{}_{}", so.job, so.name).chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
                out.push((var, String::from_utf8_lossy(&v).into_owned()));
            }
            Err(e) => tracing::warn!(leg = %producer, output = %name, error = %format!("{e:#}"), "a secret output could not be opened"),
        }
    }
    out
}

/// A leg's stored conclusion, as the plan has it.
fn conclusion_of(s: Option<&str>) -> PlanC {
    match s {
        Some("success") => PlanC::Success,
        Some("cancelled") => PlanC::Cancelled,
        Some("skipped") => PlanC::Skipped,
        Some("timed_out") => PlanC::TimedOut,
        _ => PlanC::Failure,
    }
}

/// A principal as GitHub's contexts name people: `user:lead`, or `agent:… for user:lead`, is `lead`.
fn login_of(by: &str) -> String {
    let who = by.rsplit(" for ").next().unwrap_or(by);
    let who = who.rsplit(" of ").next().unwrap_or(who);
    who.strip_prefix("user:").unwrap_or(who).to_owned()
}

fn report_json(r: &LegReport) -> String {
    serde_json::json!({
        "conclusion": r.conclusion,
        "outputs": r.outputs,
        "json_outputs": r.json_outputs,
        "env": r.env,
        "run_env": r.run_env,
        "secret_outputs": r.secret_outputs,
    })
    .to_string()
}

fn report_from_json(s: &str) -> LegReport {
    let v: serde_json::Value = serde_json::from_str(s).unwrap_or_default();
    let get = |k: &str| v.get(k).cloned().unwrap_or_default();
    LegReport {
        conclusion: serde_json::from_value(get("conclusion")).ok(),
        outputs: serde_json::from_value(get("outputs")).unwrap_or_default(),
        json_outputs: serde_json::from_value(get("json_outputs")).unwrap_or_default(),
        env: serde_json::from_value(get("env")).unwrap_or_default(),
        run_env: serde_json::from_value(get("run_env")).unwrap_or_default(),
        secret_outputs: serde_json::from_value(get("secret_outputs")).unwrap_or_default(),
    }
}

/// A leg's cache mode and the cache scopes its token allows, in lookup order (W08-R06,
/// W08-R14): restores from its own ref, a pull request's base and the default branch, and saves
/// to its own ref.
fn cache_access(spec: &kiln_ci::plan::LegSpec, r: &RunSt) -> (kiln_ci::store::cache::Mode, Vec<super::token::Access>) {
    use kiln_ci::store::cache::{self, Mode};
    let default_branch = r.github.pointer("/event/repository/default_branch").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).unwrap_or("main").to_owned();
    let git_ref = r.event.git_ref.clone();
    let on_default = git_ref == format!("refs/heads/{default_branch}");
    let job_mode = spec.def().and_then(|j| j.cache_mode.as_deref()).and_then(Mode::parse);
    let wf_mode = spec.workflow.cache_mode.as_deref().and_then(Mode::parse);
    let mode = cache::effective(job_mode, wf_mode, &r.event.name, on_default);
    // A called workflow's legs do no more than its caller allows (W08-R14).
    let mode = match spec.cache_cap.as_deref().and_then(Mode::parse) {
        Some(cap) => mode.min(cap),
        None => mode,
    };
    let mut access = Vec::new();
    let mut scopes = cache::scopes(&git_ref, r.event.base_ref.as_deref(), &default_branch);
    // An untrusted run saves only to a scope of its own, which no trusted run reads; it restores
    // from that and its base's, as on GitHub (W18-R07).
    if let (false, Some(own)) = (r.trusted, scopes.first_mut()) {
        *own = format!("untrusted/{git_ref}");
    }
    for (i, scope) in scopes.into_iter().enumerate() {
        let read = if mode.restores() { super::token::READ } else { 0 };
        let write = if i == 0 && mode.saves() { super::token::WRITE } else { 0 };
        if read | write != 0 {
            access.push(super::token::Access { scope, permission: read | write });
        }
    }
    (mode, access)
}

/// A run's workflow file, from `github.workflow_ref` (`owner/repo/<path>@<ref>`).
fn run_path(github: &serde_json::Value) -> String {
    let r = github.get("workflow_ref").and_then(serde_json::Value::as_str).unwrap_or_default();
    let repo = github.get("repository").and_then(serde_json::Value::as_str).unwrap_or_default();
    r.strip_prefix(repo).map(|x| x.trim_start_matches('/')).and_then(|x| x.rsplit_once('@')).map(|(p, _)| p.to_owned()).unwrap_or_default()
}

/// A repository on GitHub whose jobs act on GitHub itself (`github_api: github`, W14-R12): the App
/// they act through, and the repository's name there.
async fn github_api(hub: &Hub, repo: &crate::store::Repo) -> Option<(Arc<crate::githost::github::App>, String)> {
    let mode = super::repo_config::load(hub, repo).await.github_api;
    if mode.as_ref().and_then(serde_yaml::Value::as_str) != Some("github") {
        return None;
    }
    let app = crate::githost::github::app(hub).await?;
    let name = app.name_of(repo)?;
    Some((app, name))
}

/// GitHub's GraphQL endpoint beside its REST API (`/api/v3` → `/api/graphql` on Enterprise Server).
fn graphql_of(api: &str) -> String {
    match api.trim_end_matches('/').strip_suffix("/v3") {
        Some(base) => format!("{base}/graphql"),
        None => format!("{}/graphql", api.trim_end_matches('/')),
    }
}

#[cfg(test)]
mod tests {
    /// W05-R17: a rerun's `github.triggering_actor` is whoever reran it, as GitHub names people; an
    /// agent's is the person it acts for.
    #[test]
    fn w05_r17_triggering_actor_is_who_reran_it() {
        assert_eq!(super::login_of("user:lead"), "lead");
        assert_eq!(super::login_of("agent:abcdefgh for user:lead"), "lead");
        assert_eq!(super::login_of("agent:abcdefgh@mac for user:lead"), "lead");
        assert_eq!(super::login_of("octocat"), "octocat", "GitHub's Re-run, by its login");
    }
}
