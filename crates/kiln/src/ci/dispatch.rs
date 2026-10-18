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
use kiln_ci::sched::retry::{Failed, FailureClass, RetryPolicy};
use kiln_ci::sched::routing::{self, LegRoute, RunnerView};
use kiln_ci::sched::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler};
use kiln_ci::trigger::Event;
use kiln_ci::wf::Workflow;
use kiln_ci::wire::{self, AssignmentV2, JobEventV2};
use parking_lot::Mutex;

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
}

/// What the dispatcher hears.
#[derive(Debug)]
pub enum Msg {
    Start(Box<Start>),
    /// A runner connected (or said what it can do); `running` are the legs it still runs.
    RunnerUp { agent: String, running: Vec<String> },
    RunnerLost { agent: String },
    Event { agent: String, leg: String, json: String },
    Finished { agent: String, result: JobResult },
    Rejected { agent: String, leg: String },
    Cancel { run: String, reason: String },
    /// Why a leg waits, and how each runner measures up against it (W05-R11).
    Explain { leg: String, reply: async_channel::Sender<serde_json::Value> },
}

/// The hub's handle on the dispatcher.
pub struct Handle {
    tx: async_channel::Sender<Msg>,
    rx: Mutex<Option<async_channel::Receiver<Msg>>>,
    on: AtomicBool,
    /// Legs the dispatcher owns, so the hub hands their messages here and not to v1.
    legs: Arc<Mutex<HashSet<String>>>,
    runs: Arc<Mutex<HashSet<String>>>,
}

impl Default for Handle {
    fn default() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Handle { tx, rx: Mutex::new(Some(rx)), on: AtomicBool::new(false), legs: Arc::default(), runs: Arc::default() }
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
}

/// Reusable workflows: read at the run's commit from the repository's mirror (W04-R24; remote
/// ones arrive with W07's mirror).
struct Source<'a> {
    files: &'a HashMap<String, Arc<Workflow>>,
}

impl PlanSource for Source<'_> {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        let path = uses.trim_start_matches("./").trim_start_matches("$/");
        self.files.get(uses).or_else(|| self.files.get(path)).cloned().ok_or_else(|| format!("`{uses}`: no such workflow (or it couldn't be fetched when the run started)"))
    }
}

struct Dispatch {
    hub: Arc<Hub>,
    sched: Scheduler,
    runs: HashMap<String, RunSt>,
    legs: HashMap<String, LegSt>,
    timers: BTreeSet<i64>,
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
    crate::rt::spawn(async move {
        let mut rules = vec![routing::native_rule()];
        if hub.cfg.ci.github_labels {
            rules.extend(routing::github_label_rules());
        }
        let mut d = Dispatch { hub: Arc::clone(&hub), sched: Scheduler::new(Config { rules, ..Config::default() }), runs: HashMap::new(), legs: HashMap::new(), timers: BTreeSet::new() };
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
            Msg::Explain { leg, reply } => {
                let v = serde_json::json!({ "leg": leg, "known": self.legs.contains_key(&leg), "wait_reason": self.sched.reason(&leg), "runners": self.sched.explain(&leg) });
                let _ = reply.try_send(v);
                Ok(())
            }
        };
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "CI dispatcher");
        }
    }

    async fn tick(&mut self) {
        let now = now_ms();
        while self.timers.first().is_some_and(|t| *t <= now) {
            self.timers.pop_first();
        }
        let cmds = self.sched.handle(Input::Tick, now);
        self.commands(cmds).await;
    }

    async fn runner_up(&mut self, agent: &str, running: &[String]) {
        let Some(a) = self.hub.agents.lock().get(agent).cloned() else { return };
        // Executors it has: the host always, Docker when it announced it (W13-R01's probes, for now).
        let mut executors = std::collections::BTreeMap::new();
        executors.insert("host".to_owned(), kiln_ci::facts::Executor::default());
        if self.hub.runner_caps(agent) & CAP_DOCKER != 0 {
            executors.insert("docker".to_owned(), kiln_ci::facts::Executor::default());
        }
        let facts = kiln_ci::facts::RunnerFacts { os: a.os.clone(), arch: a.arch.clone(), executors, ..kiln_ci::facts::RunnerFacts::default() };
        let view = RunnerView { name: a.name.clone(), labels: a.labels.clone(), pool: None, facts, caps: u64::from(self.hub.runner_caps(agent)) };
        // Legs this runner was running that it no longer has were lost.
        let gone: Vec<(String, u32)> = self.legs.iter().filter(|(id, l)| l.agent.as_deref() == Some(agent) && !running.contains(id) && !a.running.contains(id)).map(|(id, l)| (id.clone(), l.attempt)).collect();
        let r = RunnerIn { id: agent.to_owned(), view, slots: a.slots.max(1), capacity: None };
        let mut cmds = self.sched.handle(Input::RunnerUp(Box::new(r)), now_ms());
        for (leg, attempt) in gone {
            cmds.extend(self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }) }, now_ms()));
        }
        self.commands(cmds).await;
    }

    /// A workflow that matched an event becomes a run (W04-R01).
    async fn start(&mut self, s: Start) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        let trusted = s.event.trusted;
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
        // A kiln workflow that declares its secrets needs every required one (W09-R06).
        if let Some(why) = self.missing_secrets(&s).await {
            runs::finish_run(&self.hub, &id, Status::Failed, "startup_failure", Some(why)).await?;
            self.hub.emit_run(&id).await;
            return Ok(());
        }
        let github = self.github_context(&s, number, rowid, &name);
        let inputs = if s.inputs.is_null() { s.event.payload.get("inputs").cloned().unwrap_or(serde_json::Value::Null) } else { s.inputs.clone() };
        // The `vars` context (W09-R09): every scope but environments', which legs add.
        let vars = match super::secrets::vars_for(&self.hub, &s.repo, &s.path, None).await {
            Ok(v) => serde_json::Value::Object(v),
            Err(e) => {
                tracing::warn!(run = %id, error = %format!("{e:#}"), "variables could not be read");
                serde_json::json!({})
            }
        };
        let input = RunInput { github: github.clone(), inputs, vars: vars.clone(), changed: s.event.changed.clone().map(Arc::new), trusted };
        self.save_input(&id, &input, &s.actions, &s.event).await?;
        let files = Arc::new(super::reusable::files(&self.hub, &s.repo, &s.event.sha, &s.workflow).await);
        let started = RunPlan::start(Arc::clone(&s.workflow), input, self.settings(), &Source { files: &files });
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
        let st = RunSt { plan, repo: s.repo, event: s.event, path: s.path, created: crate::util::now_unix(), in_progress: false, files, wf: s.workflow, github, vars, trusted, actions: s.actions, keys: HashMap::new(), held: Some(actions), seq: 0 };
        self.workflow_run(&st, "requested", None);
        self.runs.insert(id.clone(), st);
        self.hub.ci.runs.lock().insert(id.clone());
        self.hub.emit_run(&id).await;
        let cmds = self.sched.handle(Input::RunQueued { run: id, group, supersede: None }, now_ms());
        self.commands(cmds).await;
        Ok(())
    }

    /// `KCI4040`: the required secrets a kiln workflow declares that don't resolve for each job's
    /// environment, with the scopes searched and how to add them; `None` when all do.
    async fn missing_secrets(&self, s: &Start) -> Option<String> {
        let wf = &s.workflow;
        if wf.secrets.is_empty() || !wf.secrets.iter().any(|d| d.required) {
            return None;
        }
        let mut envs: Vec<Option<String>> = vec![None];
        for j in &wf.jobs {
            if let Some(e) = j.environment.as_ref().map(|e| e.name.src.clone()).filter(|n| !n.contains("${{"))
                && !envs.contains(&Some(e.clone()))
            {
                envs.push(Some(e));
            }
        }
        let mut lines = Vec::new();
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

    fn settings(&self) -> Settings {
        Settings { default_runs_on: vec!["k8s".into()], ..Settings::default() }
    }

    async fn save_input(&self, run: &str, input: &RunInput, actions: &std::collections::BTreeMap<String, wire::ActionRef>, event: &Event) -> anyhow::Result<()> {
        let json = serde_json::json!({ "github": input.github, "inputs": input.inputs, "vars": input.vars, "changed": input.changed.as_deref(), "trusted": input.trusted, "actions": actions, "event": event }).to_string();
        let run = run.to_owned();
        self.hub.store.call(move |c| Ok(c.execute("UPDATE ci_plans SET input = ?2 WHERE run_id = ?1", rusqlite::params![run, json]).map(|_| ())?)).await
    }

    /// The `github` context (W03-R07, W06-R03), with GitHub's URLs for repositories hosted there.
    fn github_context(&self, s: &Start, number: i64, rowid: i64, name: &str) -> serde_json::Value {
        // The code host is kiln's facade; each runner sees it at its own address of the server.
        let base = self.hub.cfg.server.public_url.trim_end_matches('/').to_owned();
        let server_url = base.clone();
        let default_branch = s.event.payload.pointer("/repository/default_branch").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
        let repo = kiln_ci::trigger::context::Repo { full_name: s.repo.name.clone(), id: 0, owner_id: 0, default_branch, private: false, server_url };
        let run = kiln_ci::trigger::context::RunFacts {
            run_id: u64::try_from(rowid).unwrap_or(0),
            run_number: u64::try_from(number).unwrap_or(0),
            run_attempt: 1,
            workflow: name.to_owned(),
            workflow_ref: format!("{}/{}@{}", s.repo.name, s.path, s.event.git_ref),
            workflow_sha: s.event.sha.clone(),
            api_url: format!("{base}/api/v3"),
            graphql_url: format!("{base}/api/graphql"),
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
                PlanAction::Queue(spec) => self.queue(run, *spec).await?,
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
                    runs::finish_run(&self.hub, run, status_of(c), c.result(), None).await?;
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
        Ok(())
    }

    /// A leg the plan queued: its row, then the scheduler.
    async fn queue(&mut self, run: &str, spec: LegSpec) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        runs::insert_leg(&self.hub, run, &id, &spec, Status::Queued, None).await?;
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id.clone());
        self.hub.ci.legs.lock().insert(id.clone());
        let job = spec.def();
        let uses = job.is_some_and(|j| j.steps.iter().any(|s| s.uses.as_deref().is_some_and(|u| !u.starts_with("docker://"))));
        let docker = spec.container.is_some() || !spec.services.is_empty() || job.is_some_and(|j| j.steps.iter().any(|s| s.uses.as_deref().is_some_and(|u| u.starts_with("docker://"))));
        let caps = u64::from(CAP_CI2) | if uses { u64::from(CAP_ACTIONS) } else { 0 } | if docker { u64::from(CAP_DOCKER) } else { 0 };
        let route = LegRoute {
            job: spec.job.clone(),
            job_name: spec.job_name.clone(),
            workflow: r.wf.name.clone().unwrap_or_default(),
            repo: r.repo.name.clone(),
            event: r.event.name.clone(),
            labels: spec.runs_on.labels.clone(),
            group: spec.runs_on.group.clone(),
            requires: spec.requires.clone(),
            caps,
        };
        let group = spec.concurrency.as_ref().map(|g| (format!("{}\0{}", r.repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
        let retry = job.and_then(|j| j.kiln.retry.as_ref()).and_then(RetryPolicy::from_yval);
        let leg = LegIn {
            id: id.clone(),
            run: run.to_owned(),
            repo: r.repo.name.clone(),
            route,
            priority: priority(&r.event.name),
            exclusive: job.is_some_and(|j| j.kiln.exclusive),
            group,
            retry,
            ..LegIn::default()
        };
        self.legs.insert(id.clone(), LegSt { run: run.to_owned(), key: spec.key.clone(), spec, attempt: 1, agent: None, report: LegReport::default(), class: None });
        self.hub.emit_job(&id).await;
        let cmds = self.sched.handle(Input::LegQueued(Box::new(leg)), now_ms());
        Box::pin(self.commands(cmds)).await;
        Ok(())
    }

    /// Carries out what the scheduler decided.
    async fn commands(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            let r = match c {
                Command::StartRun { run } => {
                    let held = self.runs.get_mut(&run).and_then(|r| r.held.take()).unwrap_or_default();
                    self.actions(&run, held).await
                }
                Command::CancelRun { run, reason } => self.cancel_run(&run, &reason).await,
                Command::Assign { leg, runner, attempt, executor, image } => self.assign(&leg, &runner, attempt, executor, image).await,
                Command::Cancel { leg, reason, .. } => {
                    if let Some(agent) = self.legs.get(&leg).and_then(|l| l.agent.clone()) {
                        self.hub.send_to_agent(&agent, LANE_CONTROL, &ServerMsg::Cancel { job: leg, reason }).await;
                    }
                    Ok(())
                }
                Command::Retry { leg, attempt, at } => {
                    if let Some(l) = self.legs.get_mut(&leg) {
                        l.attempt = attempt;
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
        }
    }

    /// Sends a leg to its runner (W05, W06).
    async fn assign(&mut self, leg: &str, runner: &str, attempt: u32, executor: Option<String>, image: Option<String>) -> anyhow::Result<()> {
        let Some(l) = self.legs.get(leg) else { return Ok(()) };
        let Some(r) = self.runs.get(&l.run) else { return Ok(()) };
        let (cache_mode, cache) = cache_access(&l.spec, r);
        let ttl = (i64::from(l.spec.timeout_minutes) + 60) * 60;
        let token = super::token::mint(&self.hub, &super::token::Leg { leg, run: &l.run, repo: &r.repo.id, cache, cache_mode: cache_mode.as_str(), ttl });
        // GITHUB_TOKEN: a job token with the job's permissions, revoked when the attempt ends
        // (W10-R03, R04).
        let job_token = super::token::mint_job(&self.hub, leg, &l.run, &r.repo.id, &l.spec.permissions, ttl).await?;
        let (secrets, used_ids, vars) = self.leg_secrets(l, r, &job_token).await;
        let a = self.assignment(leg, l, r, attempt, executor.clone(), image, token, job_token, cache_mode, secrets, vars);
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
    async fn leg_secrets(&self, l: &LegSt, r: &RunSt, token: &str) -> (Vec<wire::Secret>, Vec<String>, serde_json::Value) {
        let spec = &l.spec;
        let env = spec.environment.as_ref().map(|e| e.name.clone());
        let vars = match &env {
            Some(e) => match super::secrets::vars_for(&self.hub, &r.repo, &r.path, Some(e)).await {
                Ok(v) => serde_json::Value::Object(v),
                Err(_) => r.vars.clone(),
            },
            None => r.vars.clone(),
        };
        let token_secret = wire::Secret { name: "GITHUB_TOKEN".into(), value: token.to_owned() };
        if !r.trusted {
            return (vec![token_secret], Vec::new(), vars);
        }
        let used = spec.def().map(kiln_ci::secrets::used).unwrap_or_default();
        let needed = kiln_ci::secrets::needed(&spec.secrets, &used);
        let resolved = match super::secrets::for_leg(&self.hub, &r.repo, &r.path, env.as_deref(), &needed).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(leg = %spec.key, error = %format!("{e:#}"), "secrets could not be read; the leg gets none");
                Default::default()
            }
        };
        let mut run: std::collections::BTreeMap<String, String> = resolved.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect();
        run.insert("GITHUB_TOKEN".into(), token.to_owned());
        let given = kiln_ci::secrets::deliver(&spec.secrets, &used, &run);
        let ids: Vec<String> = resolved.values().filter_map(|v| v.id.clone()).collect();
        let mut out: Vec<wire::Secret> = given.into_iter().filter(|(k, _)| k != "GITHUB_TOKEN").map(|(name, value)| wire::Secret { name, value }).collect();
        out.push(token_secret);
        (out, ids, vars)
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
            executor: wire::ExecutorSpec { image: if executor.as_deref() == Some("docker") { image } else { None }, kind: executor.unwrap_or_else(|| "host".into()), options: serde_json::Value::Null },
            container: spec.container.clone(),
            services: spec.services.iter().cloned().collect(),
            job_container: job.and_then(|j| j.container.clone()),
            job_services: job.map(|j| j.services.clone()).unwrap_or_default(),
            steps: job.map(|j| j.steps.clone()).unwrap_or_default(),
            workflow_env: wf.env.clone(),
            job_env: job.map(|j| j.env.clone()).unwrap_or_default(),
            workflow_defaults: wf.defaults.clone(),
            job_defaults: job.map(|j| j.defaults.clone()).unwrap_or_default(),
            outputs: job.map(|j| j.outputs.iter().map(|o| (o.name.clone(), o.value.clone())).collect()).unwrap_or_default(),
            contexts: wire::Contexts { github: r.github.clone(), vars, needs: spec.needs.clone(), strategy: spec.strategy.clone(), matrix: spec.matrix.clone(), inputs: spec.inputs.clone() },
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
            },
            checkout: if wf.checkout && job.is_none_or(|j| j.kiln.checkout != Some(false)) { "auto".into() } else { "none".into() },
            actions: r.actions.clone(),
            debug: wire::Debug::default(),
            limits: wire::Limits::default(),
        }
    }

    /// What a runner says about a leg while it runs (W06, §2.5).
    async fn event(&mut self, agent: &str, leg: &str, json: &str) -> anyhow::Result<()> {
        let _ = agent;
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
            JobEventV2::JobOutputs { outputs, json_outputs, env, run_env, secret_outputs } => {
                if let Some(l) = self.legs.get_mut(leg) {
                    l.report.outputs = outputs.into_iter().collect();
                    l.report.json_outputs = json_outputs.into_iter().collect();
                    l.report.env = env.into_iter().collect();
                    l.report.run_env = run_env;
                    l.report.secret_outputs = secret_outputs;
                }
            }
            JobEventV2::FailureClass { class } => {
                if let Some(l) = self.legs.get_mut(leg) {
                    l.class = FailureClass::parse(&class);
                }
            }
            JobEventV2::Group { .. } | JobEventV2::Mask { .. } | JobEventV2::TestReport { .. } | JobEventV2::Metrics { .. } => {}
        }
        Ok(())
    }

    /// A leg's attempt ended on its runner: the scheduler decides on a retry, else the plan hears.
    async fn finished(&mut self, agent: &str, r: JobResult) -> anyhow::Result<()> {
        self.hub.release_slot(agent, &r.job);
        self.hub.logs.finish(&r.job);
        // The attempt's GITHUB_TOKEN dies with it (W10-R03).
        if let Err(e) = super::token::revoke_leg(&self.hub, &r.job).await {
            tracing::warn!(leg = %r.job, error = %format!("{e:#}"), "the job token could not be revoked");
        }
        let Some(l) = self.legs.get(&r.job) else { return Ok(()) };
        let attempt = l.attempt;
        let outcome = match r.status {
            Status::Succeeded | Status::Skipped => Outcome::Success,
            Status::Cancelled => Outcome::Cancelled,
            Status::TimedOut => Outcome::Failed(Failed { class: FailureClass::Timeout, exit_code: r.exit_code, log_hits: Vec::new() }),
            _ => Outcome::Failed(Failed { class: l.class.unwrap_or(FailureClass::Failure), exit_code: r.exit_code, log_hits: Vec::new() }),
        };
        if let Some(l) = self.legs.get_mut(&r.job) {
            l.agent = None;
            if let Some(e) = &r.error {
                l.report.outputs.retain(|_| true);
                tracing::info!(leg = %r.job, error = %e, "leg ended with an error");
            }
        }
        let cmds = self.sched.handle(Input::AttemptFinished { leg: r.job.clone(), attempt, outcome }, now_ms());
        self.commands(cmds).await;
        Ok(())
    }

    /// The leg is done for good: its row, then its plan (W04-R02).
    async fn settled(&mut self, leg: &str, outcome: Outcome, reason: Option<String>) -> anyhow::Result<()> {
        let Some(l) = self.legs.remove(leg) else { return Ok(()) };
        let c = match &outcome {
            Outcome::Success => PlanC::Success,
            Outcome::Cancelled => PlanC::Cancelled,
            Outcome::Failed(f) if f.class == FailureClass::Timeout => PlanC::TimedOut,
            Outcome::Failed(_) => PlanC::Failure,
        };
        let outputs = serde_json::to_string(&l.report.outputs.iter().cloned().collect::<std::collections::BTreeMap<_, _>>()).ok();
        runs::finish_leg(&self.hub, leg, status_of(c), c.result(), outputs, reason).await?;
        self.hub.emit_job(leg).await;
        let report = LegReport { conclusion: Some(c), ..l.report };
        let Some(run) = self.runs.get_mut(&l.run) else { return Ok(()) };
        run.seq += 1;
        let seq = run.seq;
        let files = Arc::clone(&run.files);
        let acts = run.plan.on_leg_finished(&l.key, report.clone(), &Source { files: &files });
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
        let acts = r.plan.on_cancel(&Source { files: &files });
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
        let (mut plan, _) = RunPlan::start(Arc::clone(&wf), input.clone(), self.settings(), &Source { files: &files }).map_err(|e| anyhow::anyhow!(e))?;
        let run_owned = run.to_owned();
        let log: Vec<(i64, String, Option<String>, Option<String>)> = self.hub.store.call(move |c| {
            let mut st = c.prepare("SELECT seq, kind, leg, report FROM ci_plan_log WHERE run_id = ?1 ORDER BY seq")?;
            Ok(st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
        let mut seq = 0;
        for (s, kind, leg, report) in log {
            seq = s;
            match kind.as_str() {
                "cancel" => {
                    let _ = plan.on_cancel(&Source { files: &files });
                }
                _ => {
                    let rep = report.as_deref().map(report_from_json).unwrap_or_default();
                    let _ = plan.on_leg_finished(leg.as_deref().unwrap_or_default(), rep, &Source { files: &files });
                }
            }
        }
        // The legs as the store has them.
        let run_owned = run.to_owned();
        let legs: Vec<(String, String, String, Option<String>, u32, Option<String>)> = self.hub.store.call(move |c| {
            let mut st = c.prepare("SELECT id, key, status, agent, attempt, leg FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL")?;
            Ok(st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
        let event = v.get("event").and_then(|e| serde_json::from_value::<Event>(e.clone()).ok()).unwrap_or_else(|| Event { name: row.event.clone(), sha: row.sha.clone(), git_ref: input.github.get("ref").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned(), trusted: input.trusted, ..Event::default() });
        let actions = v.get("actions").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default();
        let path = run_path(&input.github);
        let mut st = RunSt { plan, repo: repo.clone(), event, path, created: row.created_at, in_progress: true, files: Arc::clone(&files), wf: Arc::clone(&wf), github: input.github.clone(), vars: input.vars.clone(), trusted: input.trusted, actions, keys: HashMap::new(), held: None, seq };
        let mut queued = Vec::new();
        for (id, key, status, agent, attempt, leg) in legs {
            st.keys.insert(key.clone(), id.clone());
            let s = Status::parse(&status);
            if s.is_terminal() {
                continue;
            }
            let Some(spec) = leg.and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok()) else { continue };
            let spec = rebuild_spec(&wf, &spec);
            self.hub.ci.legs.lock().insert(id.clone());
            self.legs.insert(id.clone(), LegSt { run: run.to_owned(), key, spec: spec.clone(), attempt: attempt.max(1), agent: agent.clone(), report: LegReport::default(), class: None });
            let li = LegIn { id: id.clone(), run: run.to_owned(), repo: repo.name.clone(), route: LegRoute { job: spec.job.clone(), labels: spec.runs_on.labels.clone(), requires: spec.requires.clone(), caps: u64::from(CAP_CI2), ..LegRoute::default() }, priority: priority(&row.event), ..LegIn::default() };
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
        self.runs.insert(run.to_owned(), st);
        let _ = self.sched.handle(Input::RunQueued { run: run.to_owned(), group: None, supersede: None }, now_ms());
        for li in queued {
            let cmds = self.sched.handle(Input::LegQueued(Box::new(li)), now_ms());
            self.commands(cmds).await;
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
        workflow: Arc::clone(wf),
        job_index: index,
    }
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
    let mut access = Vec::new();
    for (i, scope) in cache::scopes(&git_ref, r.event.base_ref.as_deref(), &default_branch).into_iter().enumerate() {
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
