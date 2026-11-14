//! The CI engine's dispatcher (docs/ci.md W05 design): one task that owns the scheduler and every
//! live run's plan. It turns what happens (a run to start, a runner connecting, a leg's events and
//! end, a cancel) into the plan's and the scheduler's inputs, and carries out what they decide:
//! legs written to the store, `AssignV2` sent to runners, steps and annotations recorded, runs
//! finished. Plans are pure, so after a restart each unfinished run's plan is rebuilt by replaying
//! its log.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures::FutureExt as _;
use kiln_ci::plan::{self, Conclusion as PlanC, LegReport, LegSpec, PlanAction, PlanSource, RunInput, RunPlan, Settings};
use kiln_ci::sched::groups::GroupConfig;
use kiln_ci::sched::retry::{Failed, FailureClass, RetryOn, RetryPolicy};
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::demand::{Demand, Domain, Intent, Supersession, When, Work};
use kiln_ci::sched::state::Decision;
use kiln_ci::sched::{Command, Config, Fence, Input, LegIn, Outcome, RunnerIn, Scheduler, Supersede};
use kiln_ci::trigger::Event;
use kiln_ci::wf::Workflow;
use kiln_ci::wire::{self, AssignmentV2, JobEventV2};
use rusqlite::OptionalExtension as _;

use super::runs;
use crate::hub::Hub;
use crate::proto::{CAP_ACTIONS, CAP_CI2, CAP_DOCKER, CAP_LEASES, JobResult, LANE_CONTROL, ServerMsg};
use crate::rcu::Rcu;
use crate::store::{Repo, Status};

/// A dependency preflight's slot, if one is free: 32 at once, taken as a turn of a gate with no lock (`crate::turn::Gate`,
/// goal 5, plan 01a1a59c2aae), never waited for.
fn preflight_slot() -> Option<crate::turn::Turn<'static>> {
    static SLOTS: crate::turn::Gate = crate::turn::Gate::new(32);
    SLOTS.try_turn()
}

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
    /// Told the run's id once it's created: a queue of one answer (`crate::memory::queue::answer`), lock-free
    /// (goal 5, plan 01a1a59c2aae, its top goal), as every answer of the dispatcher's below is: an async-channel's
    /// wait took event-listener's `Mutex`, on the asker's shard and the dispatcher's.
    pub reply: Option<crate::memory::queue::Sender<String>>,
    /// The run is a culprit-finding try (W28-R57): only kiln's bisections set it.
    pub culprit: Option<Try>,
    /// The run is its author's of this Pipelines draft (docs/ci.md W29-R14): it reports no status
    /// to its change, and deploys nothing.
    pub draft: Option<String>,
    /// The agent session that asked for the run (its whole id), when one did: kept with the run.
    pub agent: Option<String>,
}

/// A culprit-finding try (docs/ci.md W28-R57), as kiln's bisection starts it. A gate's goes at
/// once: at gate priority, never held by admission. The rest go at low priority, below W28-R64's
/// work in progress, and where the workflow declares admission each is a speculative demand of its
/// lane, under the lane's window (W28-R68). A try is no newer push of its branch: it joins no
/// run-level concurrency group and supersedes nothing, so nothing on its branch waits for it,
/// cancels it or is withdrawn by it. Kept with the run's input, so a restart keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Try {
    /// Its branch is in `[ci] culprit_gates`.
    pub gate: bool,
}

impl Try {
    /// A gate's tries' priority: W28-R64's gate.
    pub const GATE: u8 = 90;
    /// The rest's: below W28-R64's work in progress (40).
    pub const LOW: u8 = 20;

    pub fn priority(self) -> u8 {
        if self.gate { Self::GATE } else { Self::LOW }
    }

    /// Its class and why, as a leg's standing names them (W28-R64).
    fn intent(self) -> (kiln_ci::sched::intent::Intent, String) {
        if self.gate {
            (kiln_ci::sched::intent::Intent::Gate, "a culprit-finding try on a gate's branch ([ci] culprit_gates): at once (W28-R57)".into())
        } else {
            (kiln_ci::sched::intent::Intent::Wip, format!("a culprit-finding try: at low priority, {} (W28-R57)", Self::LOW))
        }
    }

    /// A run's try, from its saved input.
    fn of_input(v: &serde_json::Value) -> Option<Try> {
        v.get("culprit").cloned().and_then(|c| serde_json::from_value(c).ok())
    }

    /// What admission takes of a leg (W28-R60, R68), its run a try (`t`) or not, its workflow's
    /// admission `a`: the workflow's `coalesce:` and `cadence:`, and not speculative. A try
    /// coalesces with no push and keeps no cadence: it is speculative under its lane's window,
    /// unless it is a gate's, which nothing holds.
    fn admission(t: Option<Try>, a: kiln_ci::sched::admit::Spec) -> (Option<kiln_ci::sched::admit::Coalesce>, Option<kiln_ci::sched::admit::Cadence>, bool) {
        match t {
            Some(t) => (None, None, !t.gate),
            None => (a.coalesce, a.cadence, false),
        }
    }
}

/// What the dispatcher hears.
#[derive(Debug)]
pub enum Msg {
    Start(Box<Start>),
    /// W29-R23: network preflight runs outside the dispatcher. Its bounded slot stays held
    /// until this message is consumed, bounding ready snapshots as well as active fetches.
    PreparedStart { start: Box<Start>, snapshot: Result<Box<super::locks::Frozen>, String>, permit: crate::turn::Turn<'static> },
    PreparedRerun {
        run: String, mode: RerunMode, debug: bool, keep_failed: Option<u32>, snapshot: Result<Box<super::locks::Frozen>, String>, fresh_actions: bool,
        no_reuse: bool, cold: bool, placed: HashMap<String, kiln_ci::sched::Placement>, by: String, agent: Option<String>,
        reply: crate::memory::queue::Sender<Result<u32, String>>, permit: crate::turn::Turn<'static>,
    },
    /// A runner connected (or said what it can do); `running` are the legs it still runs.
    RunnerUp { agent: String, running: Vec<String> },
    /// A runner gone past its grace, no process of it left, whatever it held (W05-R35): it leaves
    /// the scheduler's fleet until it's back, and its grants are lost.
    RunnerLost { agent: String },
    /// A runner draining (W05-R35): its operator drained it, or it drains on its own (it refused
    /// what it was sent, draining or upgrading). It keeps what it runs and takes nothing new until
    /// it's back or undrained (`RunnerFacts`, which says it as the hub has it).
    RunnerDraining { agent: String },
    /// A machine's runner process gone past its grace while another of its processes keeps the
    /// machine connected (W05-R35): the legs that process ran are lost; the machine, and what its
    /// other processes run, stay.
    LegsLost { agent: String, legs: Vec<String> },
    /// A runner's heartbeat named the legs it runs: it renews their grants (W05-R34).
    Renew { agent: String, running: Vec<String> },
    /// A lease runner's process `peer` renews the grants it names (W05-R34): it's answered which of
    /// them aren't its own any more (`Renewed`, under `seq`).
    Leased { agent: String, peer: hyper_transport::PeerId, seq: u64, held: Vec<crate::proto::Grant> },
    /// A runner's probes reported (W13-R01): it is routed by what they found.
    RunnerFacts { agent: String },
    /// What a runner says about a leg, and the grant it says it under (a lease runner, W05-R36).
    Event { agent: String, leg: String, event: Box<crate::proto::JobEvent>, grant: Option<crate::proto::Grant> },
    Finished { agent: String, result: JobResult, grant: Option<crate::proto::Grant> },
    /// A repository deleted (its runs' records with it): its runs end here, their legs stopped
    /// where they run.
    RepoDeleted {
        repo: String,
    },
    Rejected {
        agent: String,
        leg: String,
        /// The grant it refused, as a lease runner names it (W05-R34); else the leg's now.
        fence: Option<Fence>,
        /// It refused it draining or upgrading: it drains on its own (W05-R35).
        draining: bool,
    },
    Cancel {
        run: String,
        reason: String,
    },
    /// Some of a run's legs cancelled by their ids, the run going on (docs/ci.md W26-R21): each
    /// leg and whether it was, or why not. Answered once, on a one-shot (goal 4, plan 01a1a59c2a9c step
    /// 5: an async-channel's wait took event-listener's `Mutex`, the dispatcher's shard and its caller's).
    CancelLegs {
        run: String,
        legs: Vec<String>,
        reason: String,
        reply: hyper_rt::sync::OneshotSender<Vec<(String, Result<(), String>)>>,
    },
    /// Why a leg waits, and how each runner measures up against it (W05-R11).
    Explain { leg: String, reply: crate::memory::queue::Sender<serde_json::Value> },
    /// The plan `Start` would make, without a run (W04-R29).
    Preview { start: Box<Start>, reply: crate::memory::queue::Sender<serde_json::Value> },
    PreparedPreview { start: Box<Start>, snapshot: Result<Box<super::locks::Frozen>, String>, reply: crate::memory::queue::Sender<serde_json::Value>, permit: crate::turn::Turn<'static> },
    /// The stored routing rules changed (W05-R21): read them again.
    RoutingChanged,
    /// A run's deployments were reviewed (W15-R03): its legs held for approval are let go, or
    /// fail.
    Reviewed { run: String },
    /// Where a leg routed so would go now, and why each runner can or can't take it.
    ExplainRoute { route: Box<LegRoute>, reply: crate::memory::queue::Sender<serde_json::Value> },
    /// A maintainer decided an outside contributor's run (W18-R06): approved, it starts; rejected,
    /// it is cancelled. `false` when the run waits for no approval.
    ApproveRun { run: String, by: String, approve: bool, reply: crate::memory::queue::Sender<bool> },
    /// A new attempt of a finished run (W05-R17): its number, or why not. `placed`: where a
    /// person's retry asked legs to run, by key (W05-R28).
    Rerun {
        run: String,
        mode: RerunMode,
        debug: bool,
        keep_failed: Option<u32>,
        fresh_actions: bool,
        no_reuse: bool,
        cold: bool,
        placed: HashMap<String, kiln_ci::sched::Placement>,
        by: String,
        /// The agent session rerunning it (its whole id), when one is.
        agent: Option<String>,
        reply: crate::memory::queue::Sender<Result<u32, String>>,
    },
    /// A runner's word on a pool's executor it was asked to start (W13-R09).
    Executor { agent: String, said: crate::runner::pool::Said },
    /// The stored pools changed: read them again.
    PoolsChanged,
    /// The pools and their executors now.
    Pools { reply: crate::memory::queue::Sender<serde_json::Value> },
    /// What a runner holds warm (W24-R18): its lanes' workspaces and how long their legs took
    /// there, as it last said.
    WorkState { agent: String, state: Box<kiln_ci::work::slots::WorkState> },
    /// A runner's warm workspaces as placement takes them, their distances measured (W24-R18);
    /// `seq` names the report they're made of, so an older one never stands for a newer.
    Warm { agent: String, seq: u64, held: Box<kiln_ci::sched::warm::Held> },
    /// A runner sent a sample (its CPUs busy, its memory in use): the engine weighs its headroom
    /// by it (the scheduler's headroom, W05).
    RunnerSample { agent: String },
    /// A runner's measured pace changed, as its kernel's words say it (docs/lb.md "Rank"): the
    /// engine's load balancer ranks its free cores at that speed (W05-R26).
    RunnerPace { agent: String, pace: kiln_ci::sched::balance::Pace },
    /// A person paused a leg, or resumed it (docs/ci.md W26-R22): waiting, it's placed nowhere
    /// until it's resumed. The store has it paused already, and its runner froze it if it runs.
    LegPaused { leg: String, paused: bool },
}

/// The bytes of messages waiting for the dispatcher, at the most (goal 11, plan 01a1a87b2e56 step 3:
/// `crate::memory::owners::CI_INBOX`): as much as the hub's queue of runners' messages
/// ([`crate::net::EVENTS_BYTES`]) its runners' messages are made from, one a message. Past it the hub's
/// loop waits ([`Handle::send_held`]), and behind it the net thread and QUIC's flow control hold the
/// runners; what's sent where nothing can wait ([`Handle::send`]) is counted all the same.
const INBOX_BYTES: usize = crate::net::EVENTS_BYTES;

impl crate::memory::queue::Weigh for Msg {
    /// Its place, and what varies with what a runner said: its names, its payload as JSON's bytes
    /// (`crate::memory::queue::json_bytes`). A person's asks count by their place and their own size.
    fn weight(&self) -> usize {
        use crate::memory::queue::json_bytes;
        let names = |l: &[String]| l.iter().map(|n| size_of::<String>().saturating_add(n.len())).fold(0usize, usize::saturating_add);
        size_of::<Self>().saturating_add(match self {
            Msg::Start(_) | Msg::Preview { .. } => size_of::<Start>(),
            Msg::RunnerUp { agent, running } => agent.len().saturating_add(names(running)),
            Msg::Event { agent, leg, event, .. } => agent.len().saturating_add(leg.len()).saturating_add(size_of::<crate::proto::JobEvent>()).saturating_add(json_bytes(event)),
            Msg::Finished { agent, result, .. } => agent.len().saturating_add(json_bytes(result)),
            Msg::CancelLegs { run, legs, reason, .. } => run.len().saturating_add(names(legs)).saturating_add(reason.len()),
            Msg::Executor { agent, said } => agent.len().saturating_add(json_bytes(said)),
            Msg::WorkState { agent, state } => agent.len().saturating_add(size_of::<kiln_ci::work::slots::WorkState>()).saturating_add(json_bytes(state)),
            Msg::Warm { agent, held, .. } => agent.len().saturating_add(size_of::<kiln_ci::sched::warm::Held>()).saturating_add(json_bytes(held)),
            _ => 0,
        })
    }
}

/// Where and how a leg runs, as the scheduler placed it.
struct Placement {
    executor: Option<String>,
    image: Option<String>,
    docker: Option<String>,
    /// The pool's executor it runs in (W13-R09).
    slot: Option<String>,
    /// Its grant (W05-R34): its fence, and what its runner does once its lease is lost.
    grant: Option<(Fence, kiln_ci::sched::LeaseLoss)>,
    /// Its grant is in the store already, written with its gang's (W05-R40).
    claimed: bool,
}

/// A gang member's `Assign` (W05-R40), as the scheduler made it: its leg, runner, attempt and
/// where it runs.
struct Member {
    leg: String,
    runner: String,
    attempt: u32,
    placed: Placement,
}

pub use kiln_ci::plan::rerun::Mode as RerunMode;

/// What a leg asks of routing (W05-R20).
/// Whether a `runs-on` label says what machine a leg needs: GitHub's hosted labels, and the OS and
/// architecture labels every self-hosted runner has.
fn machine_label(l: &str) -> bool {
    matches!(l.to_ascii_lowercase().as_str(), "self-hosted" | "linux" | "windows" | "macos" | "x64" | "arm64" | "arm") || kiln_ci::sched::routing::github_label_rules().iter().any(|r| r.matches.labels.iter().any(|x| x.eq_ignore_ascii_case(l)))
}

/// Whether any of `steps` runs containers itself (`podman`, `buildah`), as routing asks.
fn nests_containers(steps: &[kiln_ci::wf::ir::Step]) -> bool {
    steps.iter().any(|s| s.run.as_ref().is_some_and(|r| kiln_ci::sched::routing::runs_containers(&r.src)) || nests_containers(&s.parallel))
}

/// A leg's route. On `kiln workflow run`'s own server (`local`, W17-R06) a leg keeps only the labels
/// that say what machine it needs: the rest name the user's runners, and it runs on this one.
pub(crate) fn route_of(spec: &LegSpec, workflow: &str, repo: &str, event: &str, local: bool, untrusted: Option<kiln_ci::sched::routing::Untrusted>) -> LegRoute {
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
        nests_containers: job.is_some_and(|j| nests_containers(&j.steps)),
        locked: false,
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

/// The steps that need Docker where they run (W07-R06): `docker://` images and Docker actions,
/// as they name them.
fn docker_steps(steps: &[kiln_ci::wf::ir::Step], actions: &std::collections::BTreeMap<String, wire::ActionRef>, out: &mut Vec<String>) {
    for s in steps {
        if let Some(u) = s.uses.as_deref().filter(|u| (u.starts_with("docker://") || actions.get(*u).is_some_and(|a| a.docker)) && !out.iter().any(|x| x == u)) {
            out.push(u.to_owned());
        }
        docker_steps(&s.parallel, actions, out);
    }
}

/// KCI4032 (W13-R07): a leg with Docker steps whose pool runs it in a `k8s` pod with no Docker
/// sidecar fails as it's queued, saying where they run, rather than when the step comes.
fn dockerless_pod(sched: &Scheduler, r: &RunSt, spec: &LegSpec, route: &LegRoute) -> Option<String> {
    let pool = sched.pool_for(route).filter(|p| p.executor == "k8s" && !p.docker)?;
    let mut steps = Vec::new();
    docker_steps(spec.def().map(|j| j.steps.as_slice()).unwrap_or_default(), r.actions_for(spec), &mut steps);
    let named: Vec<String> = steps.iter().map(|u| format!("`{u}`")).collect();
    (!named.is_empty()).then(|| {
        format!(
            "KCI4032: {} {} Docker, and pool `{}` runs this job in Kubernetes pods without it. Docker actions run on the `host` executor where there's Docker, on the `docker` executor, or in a `k8s` pool's pods when the pool has a Docker sidecar (`docker` in its `[[ci.pools]]`).",
            named.join(", "),
            if named.len() == 1 { "needs" } else { "need" },
            pool.name
        )
    })
}

/// KCI4033, KCI4034 (W29-R29, R30): a leg with a microVM action's step fails as it's queued when no
/// runner of this kiln has the executor that action needs, naming the action and the executor: a
/// Firecracker action's, or a Shards action's, which none has until Hex's shards executor is there.
fn microvm_unrunnable(sched: &Scheduler, r: &RunSt, spec: &LegSpec) -> Option<String> {
    fn each<'a>(steps: &'a [kiln_ci::wf::ir::Step], out: &mut Vec<&'a str>) {
        for s in steps {
            out.extend(s.uses.as_deref());
            each(&s.parallel, out);
        }
    }
    let actions = r.actions_for(spec);
    let mut uses = Vec::new();
    each(spec.def().map(|j| j.steps.as_slice()).unwrap_or_default(), &mut uses);
    for u in uses {
        let Some(a) = actions.get(u) else { continue };
        for needs in [wire::FIRECRACKER, wire::SHARDS] {
            if !a.requires.iter().any(|q| q == needs) {
                continue;
            }
            let (_, runners) = sched.explain_route(&LegRoute { requires: vec![needs.to_owned()], ..LegRoute::default() });
            if runners.iter().any(|x| x.eligible) {
                continue;
            }
            return Some(if needs == wire::SHARDS {
                format!("KCI4034: `{u}` is a Shards action, and no machine runs shards microVMs yet (docs/ci.md W29-R30).")
            } else {
                format!("KCI4033: `{u}` is a Firecracker action, and no runner of this kiln has the `firecracker` executor (KVM, Firecracker and its guest kernel: `kiln runner setup firecracker`). Each of its calls is a microVM of its own (docs/ci.md W29-R29).")
            });
        }
    }
    None
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

/// What a run is for and whose it is (W28-R64, R67): its intent class and its actor, each with why.
#[derive(Debug, Clone, Default)]
struct Who {
    intent: Option<(kiln_ci::sched::intent::Intent, String)>,
    actor: Option<kiln_ci::sched::actor::Actor>,
}

/// What a run is for and whose it is, from its event and kiln's records (W28-R64, R67): read only
/// when the scheduler orders by intent or shares by actor.
async fn who_of(hub: &Hub, st: &RunSt) -> Who {
    use kiln_ci::sched::{actor, intent};
    let s = &hub.cfg.ci.scheduler;
    if !s.by_intent() && !s.fair_levels().contains(&kiln_ci::sched::FairBy::Actor) {
        return Who::default();
    }
    // Agents' change requests (crate::workspace::change): who pushed which commit last, and who
    // waits on its checks.
    let changes: Vec<crate::workspace::change::Record> =
        hub.store.kv_prefix("agents/change/".into()).await.unwrap_or_default().iter().filter_map(|(_, v)| serde_json::from_str(v).ok()).collect();
    let sha = st.event.sha.as_str();
    let mine = |c: &&crate::workspace::change::Record| !sha.is_empty() && c.sha.eq_ignore_ascii_case(sha);
    let waiting = changes.iter().filter(mine).find(|c| c.fix_checks || c.stack.is_some()).map(|c| {
        let name = c.change.clone().unwrap_or_else(|| "its change request".into());
        if c.stack.is_some() { format!("a plan run's step waits on it ({name})") } else { format!("agent {} waits on its verdict to fix its checks ({name})", c.agent) }
    });
    let default_branch = st.repo.default_branch.clone().unwrap_or_else(|| "main".into());
    let server = crate::api::COMMIT;
    let facts = intent::Facts {
        event: &st.event.name,
        branch: st.event.ref_name(),
        default_branch: &default_branch,
        deploys: false,
        server_commit: !server.is_empty() && !sha.is_empty() && sha.eq_ignore_ascii_case(server),
        waiting: waiting.as_deref(),
    };
    let message = st.event.payload.pointer("/head_commit/message").and_then(serde_json::Value::as_str).unwrap_or_default();
    let refs: Vec<actor::Change> = changes.iter().map(|c| actor::Change { agent: &c.agent, sha: &c.sha, change: c.change.as_deref() }).collect();
    Who { intent: Some(intent::classify(&facts)), actor: Some(actor::attribute(&actor::Push { sha, pusher: &st.event.actor, message }, &refs)) }
}

/// A leg's route with what its run brings: untrusted code only where `ci.untrusted` lets it
/// (W18-R07), what kiln actions' steps need of the runner (W07-R11), and Docker for Docker
/// actions (W07-R06).
#[allow(clippy::too_many_arguments)]
fn route_with(spec: &LegSpec, workflow: &str, repo: &str, event: &str, trusted: bool, actions: &std::collections::BTreeMap<String, wire::ActionRef>, locks: Option<&kiln_ci::lock::execution::Locks>, cfg: &crate::config::ci::Ci) -> LegRoute {
    let mut route = route_of(spec, workflow, repo, event, cfg.local, (!trusted).then(|| cfg.untrusted.route()));
    if let Some(locks) = locks { route.locked = true; route.caps |= crate::proto::CAP_DEPENDENCY_LOCKS; route.requires.extend(locks.runtime_requirements()); }
    for step in spec.def().map(|j| j.steps.as_slice()).unwrap_or_default() {
        if let Some(a) = step.uses.as_deref().and_then(|u| actions.get(u)) {
            route.requires.extend(a.requires.iter().filter(|q| !route.requires.contains(q)).cloned().collect::<Vec<_>>());
            if a.docker {
                route.caps |= u64::from(CAP_DOCKER);
            }
        }
    }
    route
}

/// The route a leg the store kept asks for, as [`leg_in`] made it when it was queued: where it
/// could run again (W05-R28). `None` for a row that isn't a leg of the engine's.
pub(crate) async fn stored_route(hub: &Hub, leg: &str) -> anyhow::Result<Option<LegRoute>> {
    let id = leg.to_owned();
    let row: Option<(String, String, String, String, String, String)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT j.leg, p.workflow, p.input, r.event, o.name, r.sha FROM jobs j JOIN runs r ON r.id = j.run_id JOIN ci_plans p ON p.run_id = r.id JOIN repos o ON o.id = r.repo_id
                 WHERE j.id = ?1 AND j.job_id IS NOT NULL AND j.leg IS NOT NULL",
                [&id],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?)),
            )
            .optional()?)
        })
        .await?;
    let Some((leg, model, input, event, repo, sha)) = row else { return Ok(None) };
    let wf: Arc<Workflow> = Arc::new(serde_json::from_str(&model)?);
    let v: serde_json::Value = serde_json::from_str(&input)?;
    let snapshot = super::locks::Frozen::saved(&v, &repo, &wf.path, &sha).map_err(anyhow::Error::msg)?;
    let stored: serde_json::Value = serde_json::from_str(&leg)?;
    let defined: plan::DefinedIn = stored.get("defined_in").map(|d| serde_json::from_value(d.clone())).transpose()?.unwrap_or_default();
    let defining = snapshot.as_ref().and_then(|s| s.workflow_for(&defined)).unwrap_or_else(|| Arc::clone(&wf));
    let spec = rebuild_spec(&defining, &stored);
    let trusted = v.get("trusted").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let actions: std::collections::BTreeMap<String, wire::ActionRef> = match &snapshot {
        Some(s) => s.actions_for(&spec.defined_in).ok_or_else(|| anyhow::anyhow!("the saved dependency snapshot omits this leg's workflow"))?.clone(),
        None => v.get("actions").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default(),
    };
    Ok(Some(route_with(&spec, &wf.name.clone().unwrap_or_default(), &repo, &event, trusted, &actions, snapshot.as_ref().and_then(|s| s.execution_for(&spec.defined_in, &spec.job)), &hub.cfg.ci)))
}

/// A leg's durations as the engine counts them, in milliseconds (docs/needs.md, E1): its job's
/// median and p90 seconds from history (W05-R10, R38), and its timeout.
fn durations(costs: &HashMap<String, super::insights::JobCost>, job: &str, timeout_minutes: u32) -> (Option<u64>, Option<u64>, Option<u64>) {
    let (cost, ms) = (costs.get(job), kiln_ci::sched::units::ms_of_secs);
    (cost.and_then(|c| ms(c.p50)), cost.and_then(|c| ms(c.p90)), Some(u64::from(timeout_minutes).saturating_mul(60_000)))
}

/// What the scheduler gets of a leg: its route, priority, concurrency group and retries; and for
/// placement (W24-R18), its lane, how long it runs cold (`warm`: what runners last said they hold
/// warm), its job's container and workflow file, and its job's `prefer:`.
fn leg_in(r: &RunSt, run: &str, id: &str, spec: &LegSpec, cfg: &crate::config::ci::Ci, warm: &HashMap<String, (u64, kiln_ci::work::slots::WorkState)>) -> LegIn {
    let job = spec.def();
    let (cost, p90, timeout) = durations(&r.costs, &spec.job, spec.timeout_minutes);
    let route = route_with(spec, &r.wf.name.clone().unwrap_or_default(), &r.repo.name, &r.event.name, r.trusted, r.actions_for(spec), r.dependencies.as_ref().and_then(|s| s.execution_for(&spec.defined_in, &spec.job)), cfg);
    let group = spec.concurrency.as_ref().map(|g| (format!("{}\0{}", r.repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
    let retry = job.and_then(|j| j.kiln.retry.as_ref()).and_then(RetryPolicy::from_yval);
    // The workflow's `coalesce:` and `cadence:` (W28-R60): its lane is the repository's workflow on
    // this branch, the leg's unit its job and matrix, its place the run's number.
    let admission = r.plan.admission.any().then(|| {
        // A culprit-finding try's (W28-R57): speculative under its lane's window (W28-R68), or a
        // gate's, never held.
        let (coalesce, cadence, speculative) = Try::admission(r.culprit, r.plan.admission.millis());
        kiln_ci::sched::admit::Demand {
            lane: format!("{}\0{}\0{}", r.repo.id, r.path, r.event.ref_name()),
            unit: spec.key.clone(),
            order: r.github.get("run_number").and_then(serde_json::Value::as_u64).unwrap_or_else(|| u64::try_from(r.created).unwrap_or_default()),
            coalesce,
            cadence,
            speculative,
        }
    });
    // Priority by intent (W28-R64), opt-in: a job that deploys is a gate; the rest as its run is.
    let intent = cfg.scheduler.by_intent().then(|| match (spec.environment.as_ref().filter(|e| !e.name.is_empty()), r.culprit) {
        (Some(e), _) => (kiln_ci::sched::intent::Intent::Gate, format!("it deploys to {}", e.name)),
        (None, Some(t)) => t.intent(),
        (None, None) => r.who.intent.clone().unwrap_or((kiln_ci::sched::intent::Intent::Trunk, "its run wasn't classified".into())),
    });
    // A job that keeps a warm workspace has a lane (W24-R11), as placement matches lanes: its
    // runners' toolchains apart. A cold rerun's legs (W24-R23) start from nothing kept, so warm
    // runners are nothing to them.
    let keeps = job.is_some_and(|j| kiln_ci::work::slots::warm(j.kiln.work.as_ref()).is_some());
    let lane = (keeps && !r.cold).then(|| kiln_ci::work::slots::placement(&kiln_ci::work::slots::lane_for(&r.repo.name, &r.path, &spec.job, &spec.matrix, String::new())).hex());
    let cold = lane.as_deref().and_then(|l| kiln_ci::work::slots::cold_ms(warm.values().map(|(_, s)| s), l));
    let mut prefer = job.map(|j| kiln_ci::sched::routing::preferences(&j.kiln.prefer)).unwrap_or_default();
    if r.cold {
        prefer.retain(|p| !p.warm && p.affinity.is_none());
    }
    LegIn {
        id: id.to_owned(),
        run: run.to_owned(),
        repo: r.repo.name.clone(),
        admission,
        actor: r.who.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default(),
        lane: format!("{}\0{}\0{}", r.repo.id, r.path, r.event.ref_name()),
        // A culprit-finding try's own priority (W28-R57), else by intent or event.
        priority: r.culprit.map_or_else(|| intent.as_ref().map_or_else(|| priority(&r.event.name), |(i, _)| i.priority()), Try::priority),
        // Its class (W05-R30): a culprit-finding try's from its own intent (a gate's `land`, the
        // rest `batch`, W28-R57), as its priority is its own; else from its intent, or its event.
        class: r.culprit.map_or_else(|| intent.as_ref().map_or_else(|| class(&r.event.name), |(i, _)| i.class()), |t| t.intent().0.class()),
        intent,
        route,
        exclusive: job.is_some_and(|j| j.kiln.exclusive),
        group,
        retry,
        // History's seconds, in the engine's milliseconds (docs/needs.md, E1), and its timeout:
        // when a grant of it is expected to end, and when its runner stops it (W05-R38).
        cost,
        p90,
        timeout,
        // On a `k8s` pool, a pod of its own, its containers in it (W13-R07); a kiln-dialect leg
        // that sets its `toolchain:`, `image:` or `resources:`, v1's job pod (W02-R06).
        own_executor: r.dependencies.as_ref().and_then(|s| s.execution_for(&spec.defined_in, &spec.job)).is_some() || job.is_some_and(|j| j.container.is_some() || !j.services.is_empty() || (spec.workflow.dialect == Some(kiln_ci::expr::Dialect::Kiln) && super::pools::sets_pod(j))),
        request: request_of(job, cfg),
        // Its I/O, for admission by load (W24-R19).
        load: kiln_ci::sched::load::load_of(job.and_then(|j| j.kiln.resources.as_ref())),
        warm_lane: lane,
        cold,
        container: spec.container.clone(),
        workflow_file: r.path.clone(),
        prefer,
        placement: r.placed.get(&spec.key).cloned().unwrap_or_default(),
        gang: gang_of(run, spec),
        // Its kind of work, as its runner keys its runs of it (`AssignV2`'s repository, job
        // workflow, job and matrix): where its kind last went well, the load balancer sends it
        // back (docs/lb.md "Good places").
        kind: crate::sense::run::leg_key(&r.repo.name, &spec.defined_in.workflow_file_path, &spec.job, &spec.matrix.to_string()),
        // A CI leg runs to its end when its lease is lost (W05-R34); a test build's `stop=<job>`
        // makes that job's legs stop then, as stop items do (W05-R37).
        on_lease_loss: match crate::runner::chaos::get().and_then(|c| c.stop_job.as_deref()) {
            Some(job) if spec.job.rsplit('/').next() == Some(job) => kiln_ci::sched::LeaseLoss::Stop,
            _ => kiln_ci::sched::LeaseLoss::Finish,
        },
        ..LegIn::default()
    }
}

/// What leg kind `kind`'s runs taught of its work (docs/lb.md "The CI engine"): its CPU time on
/// the reference core from the fleet's speed fit (every run of it kept, each machine's speed taken
/// out), and from its newest runs how wide it ran and its bytes. `None`: nothing kept of it yet.
async fn profile_of(hub: &Hub, kind: &str) -> Option<kiln_ci::sched::balance::Profile> {
    let work = hub.pace.work(kind)?;
    let rows = hub.store.run_observations(kind.to_owned()).await.inspect_err(|e| tracing::debug!(kind, error = %format!("{e:#}"), "a leg's runs couldn't be read: it's placed by what other kinds taught")).ok()?;
    kiln_ci::sched::balance::Profile::learn(Some(work), &rows)
}

/// Each job's upward rank, ms (W05-R23; HEFT's `rank_u`: Topcuoglu, Hariri & Wu, "Performance-
/// effective and low-complexity task scheduling for heterogeneous computing", IEEE TPDS 13(3),
/// 2002): its median duration (its history's, `costs`) and the longest chain of medians among the
/// jobs that need it, transitively: how long the run goes on after the job starts, at the least.
/// A job with no history adds nothing of its own; its chain still counts.
fn upward_ranks(wf: &Workflow, costs: &HashMap<String, super::insights::JobCost>) -> HashMap<String, u64> {
    let mut after: HashMap<&str, Vec<&str>> = HashMap::new();
    for j in &wf.jobs {
        for n in &j.needs {
            after.entry(n.job()).or_default().push(j.id.as_str());
        }
    }
    let own = |job: &str| costs.get(job).map_or(0, |c| kiln_ci::sched::units::ms_of_secs(c.p50).unwrap_or(0));
    let mut ranks: HashMap<String, u64> = HashMap::new();
    // Depth first, each job once; a job met again on its own chain (a cycle validation refuses)
    // counts nothing more.
    fn rank<'a>(job: &'a str, after: &HashMap<&'a str, Vec<&'a str>>, own: &dyn Fn(&str) -> u64, ranks: &mut HashMap<String, u64>, on: &mut HashSet<&'a str>) -> u64 {
        if let Some(r) = ranks.get(job) {
            return *r;
        }
        if !on.insert(job) {
            return 0;
        }
        let tail = after.get(job).map_or(0, |next| next.iter().map(|n| rank(n, after, own, ranks, on)).max().unwrap_or(0));
        on.remove(job);
        let r = own(job).saturating_add(tail);
        ranks.insert(job.to_owned(), r);
        r
    }
    let mut on = HashSet::new();
    for j in &wf.jobs {
        rank(j.id.as_str(), &after, &own, &mut ranks, &mut on);
    }
    ranks
}

/// `actions` with each stretch of legs queued together put in order of their jobs' upward ranks,
/// the highest first, legs of one rank as the plan gave them; every other action where it was. A
/// called workflow's leg (`call/test`) ranks as its caller's job does.
fn critical_first(actions: Vec<PlanAction>, ranks: &HashMap<String, u64>) -> Vec<PlanAction> {
    let rank_of = |spec: &LegSpec| ranks.get(&spec.job).or_else(|| spec.job.split('/').next().and_then(|caller| ranks.get(caller))).copied().unwrap_or(0);
    let mut out: Vec<PlanAction> = Vec::with_capacity(actions.len());
    let mut stretch: Vec<(u64, PlanAction)> = Vec::new();
    let flush = |stretch: &mut Vec<(u64, PlanAction)>, out: &mut Vec<PlanAction>| {
        stretch.sort_by_key(|(r, _)| std::cmp::Reverse(*r));
        out.extend(stretch.drain(..).map(|(_, a)| a));
    };
    for a in actions {
        match &a {
            PlanAction::Queue(spec) => stretch.push((rank_of(spec), a)),
            _ => {
                flush(&mut stretch, &mut out);
                out.push(a);
            }
        }
    }
    flush(&mut stretch, &mut out);
    out
}

/// The gang a leg is a member of (W05-R40): a job's `together: true` makes its legs, its matrix,
/// one, named by the run and the job, as big as the job's legs are many (two at least).
fn gang_of(run: &str, spec: &LegSpec) -> Option<kiln_ci::sched::state::Gang> {
    let size = spec.strategy.get("job-total").and_then(serde_json::Value::as_f64).map_or(0, |n| n as u32);
    (spec.def().is_some_and(|j| j.kiln.together) && size >= 2).then(|| kiln_ci::sched::state::Gang { id: format!("{run}/{}", spec.job), size })
}

/// The scheduler's settings: `[ci.scheduler]`, `[ci.fairness]` and the routing rules.
fn sched_config(hub: &Hub, rules: Vec<kiln_ci::sched::routing::Rule>) -> Config {
    let (s, f) = (&hub.cfg.ci.scheduler, &hub.cfg.ci.fairness);
    let (lease, lock_delay) = hub.cfg.sched.leases().unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "grants' leases are kiln's own");
        (60_000, 15_000)
    });
    Config {
        rules,
        // Config's numbers, in the engine's integers (docs/needs.md, E1).
        aging_per_minute: kiln_ci::sched::units::thousandths_of(s.aging_per_minute),
        default_cost: kiln_ci::sched::units::ms_of_secs(s.default_cost_seconds).unwrap_or(kiln_ci::sched::Config::default().default_cost),
        weights: f.weights.iter().map(|(repo, w)| (repo.clone(), kiln_ci::sched::units::thousandths_of(*w))).collect(),
        quotas: f.quotas.clone(),
        resources: s.resources(),
        overcommit_cpu: kiln_ci::sched::state::per_mille(s.overcommit.cpu),
        overcommit_mem: kiln_ci::sched::state::per_mille(s.overcommit.memory),
        overcommit_disk: kiln_ci::sched::state::per_mille(s.overcommit.disk),
        fair_by: s.fair_levels(),
        target_use: target_use(s),
        // Admission by load (W24-R19): off until set, and while it doesn't read.
        admission: s.admission.as_ref().and_then(|a| a.admission().inspect_err(|e| tracing::warn!(error = %e, "admission by load is off")).ok()),
        // The classes' floors and bounds (W05-R31): kiln's while what's set doesn't read.
        classes: hub.cfg.sched.classes().unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "the classes' floors and bounds are kiln's own");
            kiln_ci::sched::state::default_classes()
        }),
        // Grants are leases, renewed by runners' heartbeats (W05-R34): kiln's while what's set
        // doesn't read.
        lease: Some(lease),
        lock_delay,
        // Reservations for big items (W05-R39): off until declared.
        backfill: hub.cfg.sched.backfill,
        // The load balancer's good places (docs/lb.md): on unless `[reach] good_places = false`.
        good_places: hub.cfg.reach.good_places,
        ..Config::default()
    }
}

/// What a leg asks of its runner in resource mode (W05-R03): its job's `resources:` (kiln's: `{
/// cpu, memory, disk, gpu }`), read as its needs are (`sched::needs`), else `[ci.scheduler]
/// default_request`. An exclusive leg's is its own alone: what it asked is what it holds of its
/// runner, and asking nothing, it holds the whole runner (the default request never makes it a
/// share).
fn request_of(job: Option<&kiln_ci::wf::ir::Job>, cfg: &crate::config::ci::Ci) -> kiln_ci::sched::state::Resources {
    let asked = asked_of(job);
    if job.is_some_and(|j| j.kiln.exclusive) {
        return asked.unwrap_or_default();
    }
    asked.or_else(|| cfg.scheduler.default_request.resources().ok()).unwrap_or_default()
}

/// A job's own `resources:`, read as its needs are (`sched::needs`), when it says one that's a
/// quantity.
fn asked_of(job: Option<&kiln_ci::wf::ir::Job>) -> Option<kiln_ci::sched::state::Resources> {
    job.and_then(|j| j.kiln.resources.as_ref()).map(kiln_ci::sched::needs::resource_spec).and_then(|a| a.resources().ok())
}

/// What a leg's executor holds it to (W05-R03): in resource mode, the memory it was given, which a
/// container executor makes its job container's limit (tools that size themselves by their
/// cgroup, CodeQL's analysis among them, then fit the leg, not the machine), and its CPUs, which
/// the runner gives it of its own ([`crate::runner::cpus`]); nothing in slot mode.
fn executor_options(mode: &str, request: &kiln_ci::sched::state::Resources) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if mode == "resources" {
        if request.mem > 0 {
            o.insert("memory".into(), serde_json::json!(request.mem));
        }
        // Whole CPUs, as a cpuset gives them: half of one is one.
        if !request.cpu.is_zero() {
            o.insert("cpus".into(), serde_json::json!(request.cpu.ceil()));
        }
    }
    if o.is_empty() { serde_json::Value::Null } else { serde_json::Value::Object(o) }
}

/// The executor's options with Docker inside its container (W13-R04): beside what resource mode
/// gave the leg (its memory and CPUs), not in their place, which left a leg with Docker free of
/// both (corpus pass 1025b).
fn with_docker(options: serde_json::Value, docker: &str) -> serde_json::Value {
    let mut o = match options {
        serde_json::Value::Object(o) => o,
        _ => serde_json::Map::new(),
    };
    o.insert("docker".into(), serde_json::json!(docker));
    serde_json::Value::Object(o)
}

/// A lease runner's write under a grant that isn't its leg's live one (docs/ci.md W05-R36): refused,
/// counted, said once a grant, and its runner told once to stop that grant. The frame was taken
/// (acknowledged as a frame is), so it isn't sent again.
pub(crate) async fn refuse_write(hub: &Hub, agent: &str, g: &crate::proto::Grant, what: &'static str) {
    crate::metrics::global().ci_refused_writes.inc(&[what]);
    if hub.ci.first_refusal(g) {
        tracing::warn!(leg = %g.leg, attempt = g.attempt, epoch = g.epoch, grant = g.n, runner = agent, what, "refused a write under a grant that isn't the leg's any more (lost, or the leg granted again); its runner is told to stop it");
        let stop = ServerMsg::Stop { grant: g.clone(), reason: "its grant isn't the leg's any more: lost, or the leg granted again".into() };
        hub.command_start(agent, &stop).await.log_when_answered(crate::hub::LOG_WITHIN);
    }
}

/// `agent` told to stop grant `g`, once a grant (W05-R25, R36): one it holds that isn't the leg's.
async fn stop_grant(hub: &Hub, agent: &str, g: &crate::proto::Grant, why: &str) {
    if hub.ci.first_refusal(g) {
        tracing::warn!(leg = %g.leg, attempt = g.attempt, epoch = g.epoch, grant = g.n, runner = agent, "a runner holds a grant that isn't the leg's: it's told to stop it");
        hub.command_start(agent, &ServerMsg::Stop { grant: g.clone(), reason: why.to_owned() }).await.log_when_answered(crate::hub::LOG_WITHIN);
    }
}

/// [`runs::claim`]; in a test build with `claim=<duration>` (`KILN_CHAOS`), the first held back
/// that long and then failed, as a store that can't write would (W05-R05).
async fn claim(hub: &Hub, leg: &str, runner: &str, executor: &str, attempt: u32, fence: Fence) -> anyhow::Result<()> {
    if let Some(hold) = crate::runner::chaos::claim_fails() {
        crate::rt::sleep_until(kiln_seams::clock::later(std::time::Instant::now(), hold)).await;
        anyhow::bail!("the store couldn't write it (KILN_CHAOS claim)");
    }
    runs::claim(hub, leg, runner, executor, attempt, fence).await
}

/// [`runs::claim_all`]: a gang's grants in one transaction (W05-R40); in a test build with
/// `claim=<duration>` (`KILN_CHAOS`), the first claim held back that long and then failed, as
/// [`claim`] fails it.
async fn claim_all(hub: &Hub, grants: &[(String, String, String, u32, Fence)]) -> anyhow::Result<()> {
    if let Some(hold) = crate::runner::chaos::claim_fails() {
        crate::rt::sleep_until(kiln_seams::clock::later(std::time::Instant::now(), hold)).await;
        anyhow::bail!("the store couldn't write it (KILN_CHAOS claim)");
    }
    runs::claim_all(hub, grants).await
}

/// The hub's handle on the dispatcher. What the hub reads of it on its runners' messages (whether
/// a job is one of the engine's legs, which runner keeps a failed one) and the API on its calls,
/// each reads without a lock (goal 5, plan 01a1a59c2aae step 2): an immutable copy swapped in whole
/// as legs, runs and kept legs come and go (`crate::rcu`), the dispatcher the one writer of the legs
/// and runs. A reader sees every change made before it looked, as it did under the lock, and never
/// waits on a writer.
pub struct Handle {
    /// Its inbox, bounded at [`INBOX_BYTES`].
    tx: crate::memory::queue::Sender<Msg>,
    /// The dispatcher's end of its messages, taken once as it starts.
    rx: ArcSwapOption<crate::memory::queue::Receiver<Msg>>,
    on: AtomicBool,
    /// Legs the dispatcher owns, so the hub hands their messages here and not to v1.
    legs: Rcu<HashSet<Arc<str>>>,
    runs: Rcu<HashSet<Arc<str>>>,
    /// Each leg's live grant (docs/ci.md W05-R34, R36): the runner it was given to, its attempt and
    /// its fence, from the claim until it ends or is lost. What a lease runner writes about the leg
    /// is taken only under it; the dispatcher writes it, the hub reads it for the output it takes,
    /// without a lock (as `legs`).
    grants: Rcu<HashMap<String, (String, u32, Fence)>>,
    /// Grants a write was refused under, each said once (W05-R36).
    refused: Rcu<HashSet<(String, Fence)>>,
    /// Failed legs kept to look into: the runner keeping each, until when (W17-R02), and the grant
    /// (its attempt and fence) it was kept under, whose tokens live on while it's kept (W10-R03).
    kept: Rcu<HashMap<String, (String, i64, Option<(u32, Fence)>)>>,
    /// The unfinished runs rebuilt as the dispatcher started (W05-R25, AC09): set once, then read.
    restored: OnceLock<Restored>,
    /// Told once `restored` is set, for what waits on it (W10-R03).
    restored_told: crate::rt::Changed,
    /// Whether the log indexer runs for this hub (`super::logsearch::spawn`), engine on or off.
    indexing: AtomicBool,
    /// The lanes' verdict books, kept between reads (`super::verdict::Books`).
    pub books: super::verdict::Books,
}

/// What the dispatcher rebuilt as it started: the unfinished runs, their legs, and how long it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    pub runs: usize,
    pub legs: usize,
    pub took: Duration,
}

impl Default for Handle {
    fn default() -> Self {
        let (tx, rx) = crate::memory::queue::bounded(&crate::memory::owners::CI_INBOX, INBOX_BYTES);
        Handle { tx, rx: ArcSwapOption::from_pointee(rx), on: AtomicBool::new(false), legs: Rcu::default(), runs: Rcu::default(), grants: Rcu::default(), refused: Rcu::default(), kept: Rcu::default(), restored: OnceLock::new(), restored_told: crate::rt::Changed::new(), indexing: AtomicBool::new(false), books: super::verdict::Books::default() }
    }
}

impl Handle {
    /// Sends a message, unless the engine is off (then nothing listens), never waiting: for callers that
    /// can't (a person's request, a change to the settings: a message each), counted in the inbox's bytes
    /// all the same, past [`INBOX_BYTES`] if it must be. Runners' messages wait for room instead
    /// ([`Handle::send_held`]).
    pub fn send(&self, m: Msg) {
        if self.on.load(Ordering::Relaxed) {
            let _ = self.tx.force_send(m);
        }
    }

    /// Sends a runner's message, unless the engine is off, waiting while the inbox holds
    /// [`INBOX_BYTES`]: the hub's loop waits, and behind it the net thread and QUIC's flow control hold
    /// the runners, as for sessions' inboxes (goal 11, plan 01a1a87b2e56 step 3).
    pub async fn send_held(&self, m: Msg) {
        if self.on.load(Ordering::Relaxed) {
            let _ = self.tx.send(m).await;
        }
    }

    pub fn enabled(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// What the dispatcher rebuilt as it started, once it has.
    pub fn restored(&self) -> Option<Restored> {
        self.restored.get().copied()
    }

    /// Returns once the dispatcher has rebuilt ([`Self::restored`]), woken by the rebuild's end.
    pub async fn until_restored(&self) {
        loop {
            let told = self.restored_told.listen();
            if self.restored.get().is_some() {
                return;
            }
            told.await;
        }
    }

    /// A test's: what's sent to the dispatcher from now on, to read.
    #[cfg(test)]
    pub(crate) fn listen(&self) -> Option<Arc<crate::memory::queue::Receiver<Msg>>> {
        self.on.store(true, Ordering::Relaxed);
        self.rx.swap(None)
    }

    /// Whether `job` is one of the engine's legs.
    pub fn is_leg(&self, job: &str) -> bool {
        self.legs.load().contains(job)
    }

    /// Whether `grant` is its leg's live grant, `agent`'s (docs/ci.md W05-R36): what a lease runner
    /// writes about the leg under it is taken only then.
    pub fn live(&self, agent: &str, grant: &crate::proto::Grant) -> bool {
        self.grants.load().get(&grant.leg).is_some_and(|(a, attempt, fence)| a == agent && *attempt == grant.attempt && *fence == grant.fence())
    }

    /// Whether `leg` runs under a grant with a fence (W05-R34): a lease runner names it in what it
    /// writes about the leg, so its word unnamed isn't taken.
    pub fn fenced(&self, leg: &str) -> bool {
        self.grants.load().get(leg).is_some_and(|(_, _, fence)| *fence != Fence::default())
    }

    /// Every leg's live grant: its runner, attempt and fence (a measure's, `kiln ci bench`).
    pub(crate) fn live_grants(&self) -> HashMap<String, (String, u32, Fence)> {
        (*self.grants.get()).clone()
    }

    /// Whether `leg`'s live grant is `attempt`'s, under `fence` (W10-R03): a token minted for that
    /// grant is taken only then. One lookup in memory.
    pub fn holds(&self, leg: &str, attempt: u32, fence: Fence) -> bool {
        self.grants.load().get(leg).is_some_and(|(_, a, f)| *a == attempt && *f == fence)
    }

    /// `leg`'s live grant from now: `agent`'s, at an attempt, under a fence; or none (it ended, was
    /// lost or refused, or waits for its next). The grant this ends, or replaces with another: its
    /// tokens die with it (W10-R03).
    pub(crate) fn grant(&self, leg: &str, now: Option<(String, u32, Fence)>) -> Option<(String, u32, Fence)> {
        self.grants.update(|g| {
            let was = match now.clone() {
                Some(x) => g.insert(leg.to_owned(), x),
                None => g.remove(leg),
            };
            was.filter(|w| Some(w) != now.as_ref())
        })
    }

    /// Whether a write under `grant` is the first refused (W05-R36): it's said, and its runner told,
    /// once a grant.
    fn first_refusal(&self, grant: &crate::proto::Grant) -> bool {
        self.refused.update(|r| {
            if r.len() > 10_000 {
                r.clear();
            }
            r.insert((grant.leg.clone(), grant.fence()))
        })
    }

    /// A failed leg kept on `agent` until `until` (W17-R02): its events and logs (a step rerun in
    /// place) are taken from that runner, and the tokens of the grant it's kept under (W10-R03):
    /// its live grant as it's kept, else the one it was kept under before. `None` lets it go.
    pub fn keep(&self, job: &str, kept: Option<(String, i64)>) {
        let grant = self.grants.load().get(job).map(|(_, a, f)| (*a, *f));
        self.keep_under(job, kept, grant);
    }

    /// [`Handle::keep`], under `grant` (its attempt and fence) when it's said: the store's, as the
    /// dispatcher starts.
    pub fn keep_under(&self, job: &str, kept: Option<(String, i64)>, grant: Option<(u32, Fence)>) {
        self.kept.update(|g| match &kept {
            Some((agent, until)) => {
                let grant = grant.or_else(|| g.get(job).and_then(|k| k.2));
                g.insert(job.to_owned(), (agent.clone(), *until, grant));
            }
            None => {
                g.remove(job);
            }
        });
    }

    /// Whether `agent` keeps `job` now.
    pub fn kept_by(&self, job: &str, agent: &str) -> bool {
        self.kept.load().get(job).is_some_and(|(a, until, _)| a == agent && *until > crate::util::now_unix())
    }

    /// Whether `leg` is kept now under `attempt`'s grant, `fence` (W17-R02, W10-R03): that grant's
    /// tokens live on while it is, for its steps rerun in place.
    pub fn kept_under(&self, leg: &str, attempt: u32, fence: Fence) -> bool {
        self.kept.load().get(leg).is_some_and(|(_, until, g)| *until > crate::util::now_unix() && *g == Some((attempt, fence)))
    }

    /// Whether `run` is one of the engine's live runs.
    pub fn is_run(&self, run: &str) -> bool {
        self.runs.load().contains(run)
    }

    /// The dispatcher's: `legs` are its now.
    fn own_legs<'a>(&self, legs: impl IntoIterator<Item = &'a str> + Clone) {
        self.legs.update(|s| s.extend(legs.clone().into_iter().map(Arc::from)));
    }

    /// The dispatcher's: `legs` aren't its any more.
    fn drop_legs<'a>(&self, legs: impl IntoIterator<Item = &'a str> + Clone) {
        self.legs.update(|s| {
            for l in legs.clone() {
                s.remove(l);
            }
        });
    }

    /// The dispatcher's: `run` is live.
    fn own_run(&self, run: &str) {
        self.runs.update(|s| {
            s.insert(Arc::from(run));
        });
    }

    /// The dispatcher's: `run` isn't live any more.
    fn drop_run(&self, run: &str) {
        self.runs.update(|s| {
            s.remove(run);
        });
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
    /// W29-R23: accepted Git sources, including each callee's own action-lock scope.
    dependencies: Option<Arc<super::locks::Frozen>>,
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
    /// Each job's median and p90 seconds lately: what the scheduler expects its legs to cost
    /// (W05-R10), and when a grant of one is expected to end (W05-R38).
    costs: Arc<HashMap<String, super::insights::JobCost>>,
    /// An outside contributor's run waiting for a maintainer (W18-R06): the concurrency group
    /// it joins once approved.
    awaiting: Option<Awaiting>,
    /// What it's for and whose it is (W28-R64, R67), when the scheduler orders by them.
    who: Who,
    /// "Run anyway" (W08-R10): its legs take no earlier result over.
    no_reuse: bool,
    /// Its workflow's `supersede:`, for its event (W05-R08, W28-R46).
    sup: Option<Sup>,
    /// A cold run (`kiln rerun --cold`, W24-R23): no reuse, no kept folders, no cache restored.
    cold: bool,
    /// A culprit-finding try (W28-R57).
    culprit: Option<Try>,
    /// Where a person's retry asked its legs to run, by key (W05-R28).
    placed: HashMap<String, kiln_ci::sched::Placement>,
}

impl RunSt {
    fn actions_for(&self, spec: &LegSpec) -> &std::collections::BTreeMap<String, wire::ActionRef> {
        static EMPTY: std::sync::LazyLock<std::collections::BTreeMap<String, wire::ActionRef>> = std::sync::LazyLock::new(Default::default);
        match &self.dependencies {
            Some(snapshot) => snapshot.actions_for(&spec.defined_in).unwrap_or(&EMPTY),
            None => &self.actions,
        }
    }
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

/// W29-R24: a pod starts from accepted images before its dedicated runner connects.
fn lock_pod(pod: &mut super::pools::LegPod, pool_image: Option<&str>, locks: Option<&kiln_ci::lock::execution::Locks>) -> Result<(), String> {
    let Some(locks) = locks else { return Ok(()) };
    locks.validate()?;
    let image = pod.image.as_deref().or(pool_image).unwrap_or_else(|| crate::pipeline::toolchain_image(None));
    pod.image = Some(locks.image(image)?);
    for service in &mut pod.services { service.image = locks.image(&service.image)?; }
    Ok(())
}

/// A runner as the scheduler sees it: what its probes found (W13-R01), else what its build
/// announced; never an executor its build didn't.
fn runner_view(hub: &Hub, a: &crate::hub::AgentLive) -> RunnerView {
    let caps = hub.runner_wide_caps(&a.name);
    let mut facts = match &a.facts {
        Some(f) => (**f).clone(),
        None => {
            let mut executors = std::collections::BTreeMap::new();
            // Never on Windows (`crate::runner::probe`'s executors).
            if caps & u64::from(CAP_DOCKER) != 0 && a.os != "windows" {
                let rootless = caps & u64::from(crate::proto::CAP_DOCKER_ROOTLESS) != 0;
                executors.insert("docker".to_owned(), kiln_ci::facts::Executor { details: [("rootless".to_owned(), rootless.to_string())].into(), ..kiln_ci::facts::Executor::default() });
            }
            kiln_ci::facts::RunnerFacts { os: a.os.clone(), arch: a.arch.clone(), executors, ..kiln_ci::facts::RunnerFacts::default() }
        }
    };
    if caps & u64::from(CAP_DOCKER) == 0 {
        facts.executors.remove("docker");
    }
    if caps & u64::from(crate::proto::CAP_FIRECRACKER) == 0 {
        facts.executors.remove("firecracker");
    }
    facts.executors.entry("host".to_owned()).or_default();
    RunnerView { name: a.name.clone(), label: a.label.clone(), labels: a.labels.clone(), pool: None, facts, caps }
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
    /// Gone with its repository while it ran: its end frees its slot, and nothing is written.
    gone: bool,
    /// Its grant's fence (W05-R34), from the scheduler's `Assign`: what a renewal or a refusal of
    /// it names.
    fence: Fence,
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

/// A run's [`Source`] as it starts, with its commit's package graph for `affected()` (W24-R16,
/// R17), which the plan asks for then and keeps.
struct Graphed<'a> {
    source: Source<'a>,
    graph: Option<Arc<kiln_ci::work::graph::Graph>>,
}

impl PlanSource for Graphed<'_> {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        self.source.workflow(uses)
    }

    fn durations(&self, job: &str, items: &[String]) -> Vec<Option<f64>> {
        self.source.durations(job, items)
    }

    fn files(&self, pattern: &str) -> Vec<String> {
        self.source.files(pattern)
    }

    fn graph(&self) -> Option<Arc<kiln_ci::work::graph::Graph>> {
        self.graph.clone()
    }
}

/// The package graph a run's saved input kept (W24-R17), so a replay plans `affected()` as the
/// run did.
fn saved_graph(v: &serde_json::Value) -> Option<Arc<kiln_ci::work::graph::Graph>> {
    v.get("graph").filter(|g| !g.is_null()).and_then(|g| serde_json::from_value(g.clone()).ok()).map(Arc::new)
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
    /// The `k8s` pools' executors this server started as pods (W13-R07).
    pods: std::collections::HashSet<String>,
    /// Each leg's last decision in demands, for its routing (W28-R47).
    decisions: HashMap<String, Decision>,
    /// What each runner last said it holds warm (W24-R18), numbered: legs' cold durations are read
    /// from it, and placement takes the measured report of the newest.
    warm: HashMap<String, (u64, kiln_ci::work::slots::WorkState)>,
    warm_seq: u64,
}

fn now_ms() -> i64 {
    crate::util::now_unix() * 1000
}

/// A run's `supersede:` (W05-R08, W28-R46): what it does to older runs, the lane it supersedes on,
/// whether it withdraws there (its branch not in `keep`), and its `finish-within`.
#[derive(Debug, Clone)]
struct Sup {
    lane: String,
    action: Supersede,
    withdraws: bool,
    finish_within: i64,
}

/// How a run reaches the scheduler: new (or approved), rerun (its demand live again, withdrawing
/// nothing), or rebuilt after a restart (withdrawals worked out again, in creation order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arrival {
    New,
    Rerun,
    Restore,
}

/// The run's supersession, when its workflow declares one and its event is one `when` names: on
/// the repository, the workflow, and the branch or pull request (or W14 stack).
fn sup_of(wf: &Workflow, repo: &Repo, path: &str, event: &str, github: &serde_json::Value) -> Option<Sup> {
    let s = match Supersession::from_yval(wf.supersede.as_ref()?) {
        Ok(s) => s?,
        Err(e) => {
            tracing::warn!(workflow = %path, error = %e, "the workflow's supersede doesn't read; nothing is superseded");
            return None;
        }
    };
    let text = |p: &str| github.pointer(p).map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    let scope = s.when.iter().find_map(|w| match w {
        When::Branch if event == "push" => text("/ref").filter(|r| r.starts_with("refs/heads/")).map(|r| format!("branch:{r}")),
        When::PullRequest if event.starts_with("pull_request") => text("/event/pull_request/number").or_else(|| text("/event/number")).map(|n| format!("pr:{n}")),
        When::Stack if event == "diff" => text("/event/stack").map(|n| format!("stack:{n}")),
        _ => None,
    })?;
    let branch = text("/ref_name").unwrap_or_default();
    Some(Sup { lane: format!("{}\0{path}\0{scope}", repo.id), action: s.action, withdraws: !s.keep.contains(&branch), finish_within: s.finish_within })
}

/// A run's demand (W28-R40, R46): its commit, its intent by event (a land or merge group is a
/// gate, a dispatch someone waits for, a push to the default branch the trunk's, the rest work in
/// progress), the actor that pushed it, and its trust.
fn demand_of(run: &str, r: &RunSt, sup: &Sup) -> Demand {
    let text = |p: &str| r.github.pointer(p).and_then(serde_json::Value::as_str);
    let default = text("/event/repository/default_branch").unwrap_or("main");
    let intent = match r.event.name.as_str() {
        "land" | "merge_group" => Intent::Gate,
        "workflow_dispatch" => Intent::Waiting,
        "push" if text("/ref_name") == Some(default) => Intent::Trunk,
        _ => Intent::Wip,
    };
    let domain = if r.trusted { Domain::Trusted } else { Domain::Untrusted };
    Demand { run: run.to_owned(), lane: sup.lane.clone(), commit: r.event.sha.clone(), intent, actor: text("/actor").map(str::to_owned), domain, at: r.created.saturating_mul(1000) }
}

/// A leg's work (W28-R42, R46), in a run whose workflow adopts: its W08-R10 input hash as its key
/// (a leg without `reuse:` has none, and is never adopted); its run's trust as its domain, or
/// protected when it's bound to an environment; its job as its warm lane.
fn work_of(r: &RunSt, spec: &LegSpec, key: Option<String>) -> Option<Work> {
    if !r.sup.as_ref().is_some_and(|s| s.action == Supersede::Adopt) {
        return None;
    }
    let domain = match (spec.environment.as_ref().is_some_and(|e| !e.name.is_empty()), r.trusted) {
        (true, _) => Domain::Protected,
        (false, true) => Domain::Trusted,
        (false, false) => Domain::Untrusted,
    };
    Some(Work { key, domain, lane: format!("{}/{}/{}", r.repo.name, r.path, spec.job), observed: false })
}

/// A leg's class by its run's event (W05-R30, docs/needs.md E2.3), as W05-R09's priorities go: a
/// manual run `interactive`, a land or merge group `land`, a schedule `background`, the rest
/// (pushes, pull requests, diffs, agents' runs, `workflow_run`) `batch`.
fn class(event: &str) -> kiln_ci::sched::Class {
    use kiln_ci::sched::Class;
    match event {
        "workflow_dispatch" | "manual" => Class::Interactive,
        "merge_group" | "land" => Class::Land,
        "schedule" => Class::Background,
        _ => Class::Batch,
    }
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

/// Starts the dispatcher when the engine is on, and the log indexer either way: with the engine off it
/// keeps the legs not indexed (what the search counts) and indexes none.
#[allow(clippy::disallowed_methods)] // (e) a picked grace before it acts
pub fn spawn(hub: Arc<Hub>) {
    if !hub.ci.indexing.swap(true, Ordering::Relaxed) {
        super::logsearch::spawn(Arc::clone(&hub), hub.cfg.ci.enabled);
    }
    if !hub.cfg.ci.enabled {
        return;
    }
    let Some(rx) = hub.ci.rx.swap(None) else { return };
    hub.ci.on.store(true, Ordering::Relaxed);
    super::services::spawn_maintenance(Arc::clone(&hub));
    super::webhooks::spawn(Arc::clone(&hub));
    super::insights::spawn(Arc::clone(&hub));
    crate::rt::spawn(async move {
        // Legs kept to look into before a restart (W17-R02).
        match runs::kept_legs(&hub).await {
            Ok(kept) => {
                for (leg, agent, until, grant) in kept {
                    hub.ci.keep_under(&leg, Some((agent, until)), grant);
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "kept legs could not be read"),
        }
        let sources = super::routing::sources(&hub).await;
        let rules = sources.effective();
        let mut cfg = sched_config(&hub, rules);
        (cfg.pools, cfg.reservations) = super::pools::sources(&hub).await;
        // Its grants' epoch (W05-R25): the cluster group's term, which this start's campaign
        // raised, or one past the last if that's greater; a grant made before keeps its own.
        let term = hub.store.group().map_or(0, crate::store::group::Group::term);
        let epoch = runs::next_epoch(&hub.store, term).await.unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), term, "the CI engine's epoch couldn't be kept: it takes the group's term");
            term
        });
        let mut d = Dispatch {
            hub: Arc::clone(&hub),
            sched: Scheduler::with_epoch(cfg, epoch),
            routing: sources,
            held: HashMap::new(),
            runs: HashMap::new(),
            legs: HashMap::new(),
            timers: BTreeSet::new(),
            next: Vec::new(),
            in_commands: false,
            pods: std::collections::HashSet::new(),
            decisions: HashMap::new(),
            warm: HashMap::new(),
            warm_seq: 0,
        };
        // A test build's fault: the rebuild late, so the runners say hello first (W22-AC04).
        if let Some(late) = crate::runner::chaos::get().and_then(|c| c.rebuild_after) {
            crate::rt::sleep(late).await;
        }
        let started = std::time::Instant::now();
        if let Err(e) = d.restore().await {
            tracing::warn!(error = %format!("{e:#}"), "the CI engine could not rebuild its runs");
        }
        let restored = Restored { runs: d.runs.len(), legs: d.legs.len(), took: started.elapsed() };
        if restored.runs > 0 {
            tracing::info!(runs = restored.runs, legs = restored.legs, ms = restored.took.as_millis(), epoch, "the CI engine rebuilt its runs");
        }
        let _ = hub.ci.restored.set(restored);
        hub.ci.restored_told.notify();
        // The runners that said hello while it rebuilt: what each said it runs is on its record
        // (the hub kept it); a leg it didn't say it has, it lost.
        let agents: Vec<(String, Vec<String>)> = hub.agents.load().values().filter(|a| a.connected).map(|a| (a.name.clone(), a.running.clone())).collect();
        for (a, running) in agents {
            d.runner_up(&a, &running).await;
        }
        // Those yet to say it renew their grants once they're back, or lose them at their lease's
        // end (W05-R25): the rebuild gave each a lease from now.
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
            Msg::Start(s) => self.prepare_start(*s).await,
            Msg::PreparedStart { start, snapshot, permit } => {
                let result = self.start(*start, snapshot.map(|s| *s)).await;
                drop(permit);
                result
            }
            Msg::RunnerUp { agent, running } => {
                self.runner_up(&agent, &running).await;
                Ok(())
            }
            Msg::RunnerFacts { agent } => {
                self.runner_facts(&agent).await;
                Ok(())
            }
            Msg::RunnerSample { agent } => {
                self.runner_sample(&agent).await;
                Ok(())
            }
            Msg::RunnerPace { agent, pace } => {
                let cmds = self.sched.handle(Input::RunnerPace { runner: agent, pace }, now_ms());
                self.commands(cmds).await;
                Ok(())
            }
            Msg::WorkState { agent, state } => {
                self.warm_seq += 1;
                let (seq, hub, report) = (self.warm_seq, Arc::clone(&self.hub), (*state).clone());
                self.warm.insert(agent.clone(), (seq, *state));
                // How far each workspace is behind is git's to say, in the mirrors: off the loop.
                crate::rt::spawn(async move {
                    let held = super::warm::held(&hub, &report).await;
                    hub.ci.send(Msg::Warm { agent, seq, held: Box::new(held) });
                });
                Ok(())
            }
            Msg::Warm { agent, seq, held } => {
                if self.warm.get(&agent).is_some_and(|(newest, _)| *newest == seq) {
                    let cmds = self.sched.handle(Input::Warm { runner: agent, held: *held }, now_ms());
                    Box::pin(self.commands(cmds)).await;
                }
                Ok(())
            }
            Msg::RunnerLost { agent } => {
                let cmds = self.sched.handle(Input::RunnerLost { runner: agent }, now_ms());
                Box::pin(self.commands(cmds)).await;
                Ok(())
            }
            Msg::RunnerDraining { agent } => {
                let cmds = self.sched.handle(Input::RunnerDraining { runner: agent }, now_ms());
                Box::pin(self.commands(cmds)).await;
                Ok(())
            }
            // Each of those legs that's the machine's here still: lost now, as its process was
            // (W05-R35). One the hub didn't know was that process's runs out its lease, as nobody
            // renews it.
            Msg::LegsLost { agent, legs } => {
                let gone: Vec<(String, u32)> = legs.into_iter().filter_map(|id| self.legs.get(&id).filter(|l| l.agent.as_deref() == Some(agent.as_str())).map(|l| (id, l.attempt))).collect();
                let cmds = self.lose(gone, "its runner process went away and didn't come back").await;
                Box::pin(self.commands(cmds)).await;
                Ok(())
            }
            // Each grant it names that's its own: renewed by the scheduler, then this runner process
            // told which aren't its own any more.
            Msg::Leased { agent, peer, seq, held } => {
                let named: Vec<(String, Fence)> = held.iter().filter_map(|g| self.held_by(&agent, g).map(|f| (g.leg.clone(), f))).collect();
                if !named.is_empty() {
                    let cmds = self.sched.handle(Input::Renewed { runner: agent.clone(), held: named }, now_ms());
                    Box::pin(self.commands(cmds)).await;
                }
                let lost: Vec<crate::proto::Grant> = held.into_iter().filter(|g| self.held_by(&agent, g).is_none()).collect();
                // What it holds that isn't its own (W05-R25): a stale attempt, a leg settled or
                // given to another runner, as after a restart. Stopped, once a grant; what it
                // sends under it is refused (W05-R36).
                for g in &lost {
                    stop_grant(&self.hub, &agent, g, "its grant isn't the leg's any more: settled, granted again, or lost").await;
                }
                let lease_ms = self.sched.lease().unwrap_or(0);
                self.hub.send_peer(peer, &ServerMsg::Renewed { seq, lease_ms, epoch: self.sched.epoch(), lost }).await;
                Ok(())
            }
            // The legs it names that are its own now, each by the fence of the grant it was sent.
            Msg::Renew { agent, running } => {
                let held: Vec<(String, Fence)> = running.into_iter().filter_map(|id| self.legs.get(&id).filter(|l| l.agent.as_deref() == Some(agent.as_str())).map(|l| (id, l.fence))).collect();
                if !held.is_empty() {
                    let cmds = self.sched.handle(Input::Renewed { runner: agent, held }, now_ms());
                    Box::pin(self.commands(cmds)).await;
                }
                Ok(())
            }
            Msg::Event { agent, leg, event, grant } => self.event(&agent, &leg, *event, grant.as_ref()).await,
            Msg::Finished { agent, result, grant } => self.finished(&agent, result, grant).await,
            Msg::RepoDeleted { repo } => {
                self.repo_deleted(&repo).await;
                Ok(())
            }
            Msg::LegPaused { leg, paused } => {
                let cmds = self.sched.handle(Input::LegPaused { leg, paused }, now_ms());
                Box::pin(self.commands(cmds)).await;
                Ok(())
            }
            Msg::Rejected { agent, leg, fence: named, draining } => {
                // Refused draining or upgrading: the runner drains on its own (W05-R35), so it gets
                // nothing new until it's back, and the leg isn't kept off it for this.
                if draining {
                    let cmds = self.sched.handle(Input::RunnerDraining { runner: agent.clone() }, now_ms());
                    Box::pin(self.commands(cmds)).await;
                }
                // A lease runner's refusal of a grant that isn't the leg's now (one sent late)
                // changes nothing (W05-R34): the scheduler sees it's not, and the leg's runner stays.
                let now = self.legs.get(&leg).map(|l| l.fence).unwrap_or_default();
                let fence = named.unwrap_or(now);
                if fence == now {
                    self.hub.release_slot(&agent, &leg);
                    self.regrant(&leg, None).await;
                    if let Some(l) = self.legs.get_mut(&leg) {
                        l.agent = None;
                    }
                }
                // A gang's member refused under its live grant takes its gang back (W05-R40): it's
                // queued again, its records saying why, and the others are stopped.
                let member = self.sched.gang(&leg).is_some() && self.sched.grant(&leg).and_then(|(_, g)| g).is_some_and(|(r, f)| r == agent && f == fence);
                let cmds = self.sched.handle(Input::Rejected { leg: leg.clone(), fence }, now_ms());
                let noted = if member { self.refused_member(&leg, &agent).await } else { Ok(()) };
                Box::pin(self.commands(cmds)).await;
                noted
            }
            Msg::Cancel { run, reason } => self.cancel_run(&run, &reason).await,
            Msg::CancelLegs { run, legs, reason, reply } => match self.cancel_legs(&run, &legs, &reason).await {
                Ok(each) => {
                    let _ = reply.send(each);
                    Ok(())
                }
                Err(e) => {
                    let _ = reply.send(legs.iter().map(|l| (l.clone(), Err(format!("{e:#}")))).collect());
                    Err(e)
                }
            },
            Msg::ApproveRun { run, by, approve, reply } => {
                let r = self.approve_run(&run, &by, approve).await;
                let _ = reply.try_send(*r.as_ref().unwrap_or(&false));
                r.map(|_| ())
            }
            Msg::Explain { leg, reply } => {
                let demand = self.decisions.get(&leg).map(|d| serde_json::json!({ "decision": d, "why": d.why() }));
                let v = serde_json::json!({ "leg": leg, "known": self.legs.contains_key(&leg), "wait_reason": self.sched.reason(&leg), "standing": self.sched.standing(&leg), "runners": self.sched.explain(&leg), "demand": demand, "overflow": self.sched.overflowed(&leg) });
                let _ = reply.try_send(v);
                Ok(())
            }
            Msg::Preview { start, reply } => {
                if let Some(permit) = preflight_slot() {
                    let fallback = reply.clone();
                    let hub = Arc::clone(&self.hub);
                    let task = crate::rt::spawn(async move {
                        let snapshot = super::locks::inspect(&hub, &start.repo, &start.event.sha, &start.path, Arc::clone(&start.workflow)).await.map(Box::new);
                        hub.ci.send(Msg::PreparedPreview { start, snapshot, reply, permit });
                    });
                    if let Some(Err(_)) = task.now_or_never() { let _ = fallback.try_send(serde_json::json!({ "error": "dependency preflight could not start: runtime is full" })); }
                } else { let _ = reply.try_send(serde_json::json!({ "error": "dependency preflight is full (32 pending); retry later" })); }
                Ok(())
            }
            Msg::PreparedPreview { start, snapshot, reply, permit } => {
                let v = self.preview(&start, snapshot.map(|s| *s)).await;
                drop(permit);
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
            Msg::Executor { agent, said } => {
                let input = match said.state.as_str() {
                    "ready" => Input::ExecutorReady { executor: said.executor },
                    _ => {
                        let error = said.error.unwrap_or_else(|| "it failed".into());
                        tracing::warn!(runner = %agent, executor = %said.executor, error = %error, "a pool's executor didn't start");
                        Input::ExecutorFailed { executor: said.executor, error }
                    }
                };
                let cmds = self.sched.handle(input, now_ms());
                Box::pin(self.commands(cmds)).await;
                Ok(())
            }
            Msg::PoolsChanged => {
                let (pools, reservations) = super::pools::sources(&self.hub).await;
                let cmds = self.sched.handle(Input::PoolsChanged { pools, reservations }, now_ms());
                Box::pin(self.commands(cmds)).await;
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
            Msg::Rerun { run, mode, debug, keep_failed, fresh_actions, no_reuse, cold, placed, by, agent, reply } => {
                let recheck = if fresh_actions { Ok(true) } else { super::locks::retry_needs_preflight(&self.hub, &run).await };
                if let Err(why) = recheck { let _ = reply.try_send(Err(why)); return; }
                if recheck.unwrap_or(false) {
                    let Some(permit) = preflight_slot() else { let _ = reply.try_send(Err("dependency preflight is full (32 pending); retry later".into())); return; };
                    let fallback = reply.clone();
                    let hub = Arc::clone(&self.hub);
                    let task = crate::rt::spawn(async move {
                        let snapshot = super::locks::prepare_retry(&hub, &run, fresh_actions).await.map(Box::new);
                        hub.ci.send(Msg::PreparedRerun { run, mode, debug, keep_failed, snapshot, fresh_actions, no_reuse, cold, placed, by, agent, reply, permit });
                    });
                    if let Some(Err(_)) = task.now_or_never() { let _ = fallback.try_send(Err("dependency preflight could not start: runtime is full".into())); }
                } else {
                    let r = self.rerun(&run, &mode, debug, keep_failed, None, false, no_reuse, cold, placed, (&by, agent.as_deref())).await.map_err(|e| format!("{e:#}"));
                    let _ = reply.try_send(r);
                }
                Ok(())
            }
            Msg::PreparedRerun { run, mode, debug, keep_failed, snapshot, fresh_actions, no_reuse, cold, placed, by, agent, reply, permit } => {
                let result = match snapshot {
                    Ok(snapshot) => self.rerun(&run, &mode, debug, keep_failed, Some(Arc::new(*snapshot)), fresh_actions, no_reuse, cold, placed, (&by, agent.as_deref())).await.map_err(|e| format!("{e:#}")),
                    Err(why) => Err(why),
                };
                drop(permit);
                let _ = reply.try_send(result);
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
        Box::pin(self.commands(cmds)).await;
    }

    async fn tick(&mut self) {
        let now = now_ms();
        let mut woke = false;
        while self.timers.first().is_some_and(|t| *t <= now) {
            self.timers.pop_first();
            woke = true;
        }
        // A new term while this node leads (elected anew): grants from now are made in the next
        // epoch (W05-R25).
        let term = self.hub.store.group().map_or(0, crate::store::group::Group::term);
        if term > self.sched.epoch() {
            match runs::next_epoch(&self.hub.store, term).await {
                Ok(epoch) => self.sched.new_epoch(epoch),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), term, "the CI engine's next epoch couldn't be kept"),
            }
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
        Box::pin(self.commands(cmds)).await;
    }

    /// The `k8s` pool's executor a runner is (W13-R07): its pod's, by the name it enrolled
    /// with; `Some("")` for another ephemeral runner (a v1 job's pod, or an executor's from
    /// before a restart), which takes no leg of the engine's.
    fn hosted_executor(&self, agent: &str, ephemeral: bool) -> Option<String> {
        match self.hub.pod_executors.load().get(agent) {
            Some(e) => Some(e.clone()),
            None => ephemeral.then(String::new),
        }
    }

    /// What a runner offers in resource mode (W05-R03): what its facts say it has (its CPUs'
    /// threads, its memory, its work volume's free disk, its GPUs); `None`, its slots, until they're
    /// in, and in slot mode.
    fn capacity_of(&self, a: &crate::hub::AgentLive) -> Option<kiln_ci::sched::state::Resources> {
        if !self.hub.cfg.ci.scheduler.resources() {
            return None;
        }
        let f = a.facts.as_ref()?;
        let cpus = if f.cpu.threads > 0 { f.cpu.threads } else { f.cpu.cores };
        (cpus > 0).then(|| kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(u64::from(cpus)), mem: f.mem.total, disk: f.disk.free, gpu: u32::try_from(f.gpus.len()).unwrap_or(u32::MAX) })
    }

    async fn runner_facts(&mut self, agent: &str) {
        let Some(a) = self.hub.agents.load().get(agent).cloned() else { return };
        // Drained by its operator, it stays so whatever else changed (W05-R35); undrained, this is
        // its undrain.
        let r = RunnerIn { id: agent.to_owned(), view: runner_view(&self.hub, &a), slots: work_slots(&a), capacity: self.capacity_of(&a), executor: self.hosted_executor(agent, a.ephemeral), draining: a.draining };
        let cmds = self.sched.handle(Input::RunnerChanged(Box::new(r)), now_ms());
        Box::pin(self.commands(cmds)).await;
    }

    /// A runner's latest sample, to the engine ([`sample_of`]).
    async fn runner_sample(&mut self, agent: &str) {
        // With headroom off, runners are placed as before their samples (W05-R03).
        if !self.hub.cfg.ci.scheduler.headroom {
            return;
        }
        let sample = {
            let g = self.hub.agents.load();
            g.get(agent).and_then(|a| a.sample.as_ref().map(|s| sample_of(s, a.facts.as_deref())))
        };
        let Some(sample) = sample else { return };
        let cmds = self.sched.handle(Input::RunnerSample { runner: agent.to_owned(), sample }, now_ms());
        self.commands(cmds).await;
    }

    async fn runner_up(&mut self, agent: &str, running: &[String]) {
        let Some(a) = self.hub.agents.load().get(agent).cloned() else { return };
        let view = runner_view(&self.hub, &a);
        // Legs this runner was running that it no longer has were lost.
        let gone: Vec<(String, u32)> = self.legs.iter().filter(|(id, l)| l.agent.as_deref() == Some(agent) && !running.contains(id) && !a.running.contains(id)).map(|(id, l)| (id.clone(), l.attempt)).collect();
        // Legs it says it runs that aren't its own now (given up while it was away, past the
        // reconnect grace): it stops them.
        let stale: Vec<String> = running.iter().filter(|id| self.hub.ci.is_leg(id) && !self.legs.get(*id).is_some_and(|l| l.agent.as_deref() == Some(agent))).cloned().collect();
        for leg in stale {
            self.hub.release_slot(agent, &leg);
            self.hub.command_start(agent, &ServerMsg::Cancel { job: leg, reason: "the leg was given up while this runner was away".into() }).await.log_when_answered(crate::hub::LOG_WITHIN);
        }
        // The hub keeps at least 1 for runners that take jobs; 0 (an outpost) takes no legs.
        // Back, it takes work again, unless its operator drained it (W05-R35).
        let r = RunnerIn { id: agent.to_owned(), view, slots: work_slots(&a), capacity: self.capacity_of(&a), executor: self.hosted_executor(agent, a.ephemeral), draining: a.draining };
        let mut cmds = self.sched.handle(Input::RunnerUp(Box::new(r)), now_ms());
        cmds.extend(self.lose(gone, "the runner lost it").await);
        Box::pin(self.commands(cmds)).await;
    }

    /// `leg`'s live grant from now, `now` or none (W05-R36). The one this ends or replaces dies
    /// with its tokens (W10-R03): they're taken only under the live grant, and the leg's job tokens
    /// (GITHUB_TOKEN) are revoked in the store. Every grant's end comes here: finished (cancelled
    /// too), refused, lost (its lease ran out, its runner lost, or reconciled), settled, gone, or
    /// superseded by the leg's next.
    async fn regrant(&self, leg: &str, now: Option<(String, u32, Fence)>) {
        let Some((_, attempt, _)) = self.hub.ci.grant(leg, now) else { return };
        if let Err(e) = super::token::revoke_leg(&self.hub, leg).await {
            tracing::warn!(leg, attempt, error = %format!("{e:#}"), "the job token could not be revoked");
        }
    }

    /// Whether `g` is the live grant of its leg, `agent`'s, as the dispatcher has it (W05-R36).
    fn live(&self, agent: &str, g: &crate::proto::Grant) -> bool {
        self.legs.get(&g.leg).is_some_and(|l| l.agent.as_deref() == Some(agent) && l.attempt == g.attempt && l.fence == g.fence())
    }

    /// The fence `agent`'s grant `g` is renewed by, if it's the agent's own now (W05-R34): by its
    /// fence; or, adopted after a restart with no fence kept (the store keeps one from E3.7), by its
    /// leg, as an older runner's are.
    fn held_by(&self, agent: &str, g: &crate::proto::Grant) -> Option<Fence> {
        let l = self.legs.get(&g.leg).filter(|l| l.agent.as_deref() == Some(agent))?;
        (l.fence == g.fence() || l.fence == Fence::default()).then_some(l.fence)
    }

    /// `gone`'s legs (each with its attempt) lost with their runner: each attempt fails so, for the
    /// scheduler to retry or conclude (W05-R05). The scheduler's commands.
    async fn lose(&mut self, gone: Vec<(String, u32)>, why: &str) -> Vec<Command> {
        let mut cmds = Vec::new();
        for (leg, attempt) in gone {
            if self.legs.get(&leg).is_some_and(|l| l.gone) {
                self.forget_gone(&leg, attempt).await;
                continue;
            }
            // Its grant is over now, its tokens with it (W10-R03), before its loss is recorded.
            self.regrant(&leg, None).await;
            let end = runs::AttemptEnd { conclusion: "failure", class: Some(FailureClass::RunnerLost.as_str()), exit_code: None, error: Some(why.to_owned()), peak_rss: None, cpu_seconds: None };
            if let Err(e) = runs::attempt_finished(&self.hub, &leg, attempt, None, end).await {
                tracing::warn!(leg = %leg, error = %format!("{e:#}"), "the attempt could not be recorded");
            }
            cmds.extend(self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }) }, now_ms()));
        }
        cmds
    }

    async fn prepare_start(&mut self, s: Start) -> anyhow::Result<()> {
        let Some(permit) = preflight_slot() else { return self.start(s, Err("KCI4051 dependency preflight is full (32 pending); start the run again".into())).await };
        let fallback = s.clone();
        let hub = Arc::clone(&self.hub);
        let task = crate::rt::spawn(async move {
            let snapshot = super::locks::prepare(&hub, &s.repo, &s.event.sha, &s.path, Arc::clone(&s.workflow)).await.map(Box::new);
            hub.ci.send(Msg::PreparedStart { start: Box::new(s), snapshot, permit });
        });
        if let Some(Err(_)) = task.now_or_never() { return self.start(fallback, Err("KCI4051 dependency preflight could not start: runtime is full".into())).await; }
        Ok(())
    }

    /// A workflow that matched an event becomes a run (W04-R01).
    async fn start(&mut self, mut s: Start, prepared: Result<super::locks::Frozen, String>) -> anyhow::Result<()> {
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
        let culprit = s.given.culprit;
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
            priority: culprit.map_or_else(|| priority(&s.event.name), Try::priority),
            payload: s.event.payload.clone(),
            model,
            input: String::new(),
        };
        let (number, rowid) = runs::create_for(&self.hub, &s.repo, new, s.given.agent.clone()).await?;
        if let Some(d) = &s.given.draft {
            runs::set_draft(&self.hub, &id, d).await?;
        }
        if let Some(r) = &s.given.reply {
            let _ = r.try_send(id.clone());
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
        let prepared = prepared.and_then(|snapshot| {
            let actions = snapshot.root_actions()?.clone();
            Ok((Arc::new(snapshot), actions))
        });
        let (dependencies, actions) = match prepared {
            Ok(frozen) => frozen,
            Err(why) => {
                self.save_input(&id, &input, &Default::default(), &s.event, (false, None), &HashMap::new(), &serde_json::json!({ "default_permissions": perms, "only_matrix": only, "culprit": culprit, "dependency_preflight_failed": true })).await?;
                runs::finish_run(&self.hub, &id, Status::Failed, "startup_failure", Some(why), false).await?;
                self.hub.emit_run(&id).await;
                return Ok(());
            }
        };
        s.actions = actions;
        // Persist acceptance before a later startup check can fail. A retry after missing
        // secrets must retain these pins too; a failed preflight has no accepted snapshot.
        self.save_input(&id, &input, &s.actions, &s.event, (false, None), &HashMap::new(), &serde_json::json!({ "default_permissions": perms, "only_matrix": only, "culprit": culprit, "dependency_snapshot": dependencies })).await?;
        let pins: Vec<String> = dependencies.scopes.values().flat_map(|scope| scope.actions.iter().map(move |(uses, a)| format!("{}:{} {uses} → {}", scope.repository, scope.path, a.error.as_deref().unwrap_or(&a.sha)))).collect();
        if !pins.is_empty() { self.hub.store.audit(s.event.actor.clone(), "ci.actions.resolve".into(), format!("{} {} at {}: {}", s.repo.name, s.path, s.event.sha, pins.join(", "))).await; }
        // A kiln workflow that declares its secrets needs every required one (W09-R06).
        if let Some(why) = self.missing_secrets(&s).await {
            runs::finish_run(&self.hub, &id, Status::Failed, "startup_failure", Some(why), false).await?;
            self.hub.emit_run(&id).await;
            return Ok(());
        }
        let timing = super::results::timing(&self.hub, &s.repo.id).await;
        let files = Arc::new(dependencies.files.clone());
        let tree = Arc::new(shard_files(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await);
        // `affected()`'s package graph (W24-R17), kept with the input too.
        let graph = super::graphs::of_run(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await;
        let settings = serde_json::json!({ "default_permissions": perms, "only_matrix": only, "timing": *timing, "tree": *tree, "graph": graph.as_deref(), "culprit": culprit, "dependency_snapshot": dependencies });
        self.save_input(&id, &input, &s.actions, &s.event, (false, None), &HashMap::new(), &settings).await?;
        for scope in dependencies.scopes.values() { Box::pin(super::catalog::record(&self.hub, &id, &s.repo, &scope.actions, &files)).await; }
        let started = RunPlan::start(Arc::clone(&s.workflow), input, self.settings_with(&settings), &Graphed { source: Source { files: &files, timing: &timing, tree: &tree }, graph });
        let (plan, actions) = match started {
            Ok(x) => x,
            Err(e) => {
                runs::finish_run(&self.hub, &id, Status::Failed, "failure", Some(format!("The run could not start: {e}")), false).await?;
                self.hub.emit_run(&id).await;
                return Ok(());
            }
        };
        runs::set_run_name(&self.hub, &id, plan.name.clone()).await?;
        // A culprit-finding try joins no run-level group and supersedes nothing (W28-R57).
        let group = plan.concurrency.as_ref().filter(|_| culprit.is_none()).map(|g| (format!("{}\0{}", s.repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
        let costs = super::insights::costs(&self.hub, &s.repo.id, &s.path).await;
        let sup = if culprit.is_some() { None } else { sup_of(&s.workflow, &s.repo, &s.path, &s.event.name, &github) };
        let mut st = RunSt { plan, repo: s.repo, event: s.event, path: s.path, created: crate::util::now_unix(), in_progress: false, files, wf: s.workflow, github, vars, trusted, actions: s.actions, dependencies: Some(dependencies), keys: HashMap::new(), held: Some(actions), seq: 0, keep: HashMap::new(), debug: false, keep_failed: None, given: s.given, timing, tree, removed: 0, costs, awaiting: None, no_reuse: false, who: Who::default(), sup, cold: false, culprit, placed: HashMap::new() };
        st.who = who_of(&self.hub, &st).await;
        self.workflow_run(&st, "requested", None);
        if approval {
            // W19-R06's `approval.waiting`, recorded with it (crate::events).
            runs::set_approval(&self.hub, &id, "required", None).await?;
            st.awaiting = Some(Awaiting { group });
            self.runs.insert(id.clone(), st);
            self.hub.ci.own_run(&id);
            self.hub.emit_run(&id).await;
            return Ok(());
        }
        self.runs.insert(id.clone(), st);
        self.hub.ci.own_run(&id);
        self.hub.emit_run(&id).await;
        let cmds = self.run_queued(&id, group, Arrival::New);
        // Boxed, as each step of the commands-actions-queue cycle is: its futures otherwise nest
        // in one another's frames, past a shard's stack in a debug build.
        Box::pin(self.commands(cmds)).await;
        Ok(())
    }

    /// Hands a run to the scheduler (W05-R06, W05-R08, W28-R40, R46): its demand first when its
    /// workflow adopts, then its `RunQueued`, superseding older runs on its lane as its arrival
    /// lets it.
    fn run_queued(&mut self, run: &str, group: Option<(String, String, GroupConfig)>, arrival: Arrival) -> Vec<Command> {
        let mut cmds = Vec::new();
        let mut supersede = None;
        if let Some((s, d)) = self.runs.get(run).and_then(|r| r.sup.as_ref().map(|s| (s.clone(), demand_of(run, r, s)))) {
            if s.action == Supersede::Adopt {
                cmds.extend(self.sched.handle(Input::Demand(Box::new(d)), now_ms()));
            }
            supersede = match arrival {
                Arrival::New if s.withdraws => Some((s.lane, s.action)),
                Arrival::Restore if s.withdraws && s.action == Supersede::Adopt => Some((s.lane, s.action)),
                _ => None,
            };
        }
        cmds.extend(self.sched.handle(Input::RunQueued { run: run.to_owned(), group, supersede }, now_ms()));
        cmds
    }

    /// An adopter's execution ended (W28-R44): it takes `from`'s result over in place, its row
    /// `reused_from` naming it, and concludes as `from` did.
    async fn adopted(&mut self, leg: &str, from: &str, outcome: Outcome) -> anyhow::Result<()> {
        if let Some(report) = super::reuse::adopt_into(&self.hub, leg, from).await?
            && let Some(l) = self.legs.get_mut(leg)
        {
            l.report = report_from_json(&report);
        }
        self.settled(leg, outcome, Some(format!("reused from {from}, the same work it adopted"))).await
    }

    /// A maintainer's decision on an outside contributor's run (W18-R06).
    async fn approve_run(&mut self, run: &str, by: &str, approve: bool) -> anyhow::Result<bool> {
        let Some(waiting) = self.runs.get_mut(run).and_then(|r| r.awaiting.take()) else { return Ok(false) };
        runs::set_approval(&self.hub, run, if approve { "approved" } else { "rejected" }, Some(by)).await?;
        if approve {
            self.hub.emit_run(run).await;
            let cmds = self.run_queued(run, waiting.group, Arrival::New);
            Box::pin(self.commands(cmds)).await;
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
    async fn preview(&mut self, s: &Start, prepared: Result<super::locks::Frozen, String>) -> serde_json::Value {
        let snapshot = match prepared { Ok(s) => s, Err(error) => return serde_json::json!({ "error": error }) };
        let name = s.workflow.name.clone().unwrap_or_else(|| s.path.clone());
        let urls = github_api(&self.hub, &s.repo).await.map(|(app, _)| (app.web.clone(), app.api.clone()));
        let github = self.github_context(s, 0, 0, &name, urls.as_ref());
        let inputs = if s.inputs.is_null() { s.event.payload.get("inputs").cloned().unwrap_or(serde_json::Value::Null) } else { s.inputs.clone() };
        let vars = super::secrets::vars_for(&self.hub, &s.repo, &s.path, None).await.map(serde_json::Value::Object).unwrap_or_else(|_| serde_json::json!({}));
        let input = RunInput { github, inputs, vars, changed: s.event.changed.clone().map(Arc::new), trusted: s.event.trusted };
        let files = Arc::new(snapshot.files.clone());
        let perms = if super::repo_config::load(&self.hub, &s.repo).await.default_permissions() == plan::perms::Default::Write { "write" } else { "read" };
        let timing = super::results::timing(&self.hub, &s.repo.id).await;
        let tree = shard_files(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await;
        let graph = super::graphs::of_run(&self.hub, &s.repo, &s.event.sha, &s.workflow, &files).await;
        let p = match RunPlan::preview(Arc::clone(&s.workflow), input, self.settings_with(&serde_json::json!({ "default_permissions": perms })), &Graphed { source: Source { files: &files, timing: &timing, tree: &tree }, graph }) {
            Ok(p) => p,
            Err(e) => return serde_json::json!({ "error": e }),
        };
        let workflow = s.workflow.name.clone().unwrap_or_default();
        let envs = super::environments::defs(&self.hub, &s.repo).await.unwrap_or_default();
        let legs: Vec<serde_json::Value> = p
            .legs
            .iter()
            .map(|spec| {
                let empty = std::collections::BTreeMap::new();
                let actions = snapshot.actions_for(&spec.defined_in).unwrap_or(&empty);
                let mut route = route_with(spec, &workflow, &s.repo.name, &s.event.name, trusted_of(&self.hub.cfg.ci, &s.repo, &s.event), actions, snapshot.execution_for(&spec.defined_in, &spec.job), &self.hub.cfg.ci);
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
        let json = serde_json::json!({ "github": input.github, "inputs": input.inputs, "vars": input.vars, "changed": input.changed.as_deref(), "trusted": input.trusted, "actions": actions, "event": event, "debug": debug, "keep_failed": keep_failed, "keep": keep, "default_permissions": settings.get("default_permissions"), "only_matrix": settings.get("only_matrix"), "timing": settings.get("timing"), "tree": settings.get("tree"), "no_reuse": settings.get("no_reuse"), "graph": settings.get("graph"), "cold": settings.get("cold"), "culprit": settings.get("culprit"), "placed": settings.get("placed"), "dependency_snapshot": settings.get("dependency_snapshot"), "dependency_preflight_failed": settings.get("dependency_preflight_failed") }).to_string();
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
        // Legs ready together go to the scheduler critical path first (W05-R23): the one the run
        // waits on longest takes the load balancer's best runner first.
        let actions = match self.runs.get(run) {
            Some(r) => critical_first(actions, &upward_ranks(&r.wf, &r.costs)),
            None => actions,
        };
        for a in actions {
            match a {
                PlanAction::Queue(spec) => {
                    // A rerun's leg that isn't run again keeps the last attempt's result.
                    match self.runs.get_mut(run).and_then(|r| r.keep.remove(&spec.key)) {
                        Some((report, old)) => Box::pin(self.keep(run, *spec, report, &old)).await?,
                        None => Box::pin(self.queue(run, *spec)).await?,
                    }
                }
                PlanAction::Settle { leg, conclusion, reason } => {
                    let existing = self.runs.get(run).and_then(|r| r.keys.get(&leg.key).cloned());
                    match existing {
                        Some(id) => {
                            runs::finish_leg(&self.hub, &id, status_of(conclusion), conclusion.as_str(), None, Some(reason), None).await?;
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
                        if let Some(cmds) = self.after(cmds) {
                            Box::pin(self.commands(cmds)).await;
                        }
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
                    // W19-R06: who wants to hear of it, recorded with the end (crate::events); R02:
                    // its trace.
                    runs::finish_run(&self.hub, run, status_of(c), c.result(), None, true).await?;
                    crate::rt::spawn(super::otel::run_finished(Arc::clone(&self.hub), run.to_owned()));
                    let cmds = self.sched.handle(Input::RunFinished { run: run.to_owned() }, now_ms());
                    if let Some(r) = self.runs.remove(run) {
                        self.workflow_run(&r, "completed", Some(c.result()));
                        self.hub.ci.drop_legs(r.keys.values().map(String::as_str));
                        for id in r.keys.values() {
                            crate::offers::leg_done(&self.hub, id);
                        }
                    }
                    self.hub.ci.drop_run(run);
                    self.hub.emit_run(run).await;
                    if let Some(cmds) = self.after(cmds) {
                        Box::pin(self.commands(cmds)).await;
                    }
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
    /// planned again from its saved workflow and input. The jobs (or the leg) `mode` selects run
    /// again, the legs `placed` names where a person's retry asked (W05-R28); the others keep the
    /// last attempt's legs and results, which the plan takes as they were.
    #[allow(clippy::too_many_arguments)]
    async fn rerun(
        &mut self,
        run: &str,
        mode: &RerunMode,
        debug: bool,
        keep_failed: Option<u32>,
        fresh: Option<Arc<super::locks::Frozen>>,
        fresh_actions: bool,
        no_reuse: bool,
        cold: bool,
        placed: HashMap<String, kiln_ci::sched::Placement>,
        (by, agent): (&str, Option<&str>),
    ) -> anyhow::Result<u32> {
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
        let mut dependencies = super::locks::Frozen::saved(&v, &repo.name, &wf.path, &event.sha).map_err(anyhow::Error::msg)?.map(Arc::new);
        if let Some(fresh) = fresh {
            if fresh_actions { anyhow::ensure!(!dependencies.as_ref().is_some_and(|s| s.root_locked) && !fresh.root_locked, "Fresh actions require a reviewed lock change (W29-R25)"); }
            anyhow::ensure!(fresh.repository.eq_ignore_ascii_case(&repo.name) && fresh.commit == event.sha && fresh.path == wf.path && fresh.version == 1, "fresh dependency snapshot belongs to another workflow or commit");
            if fresh_actions || dependencies.is_none() { dependencies = Some(fresh); }
        }
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
        let last: HashMap<String, String> = reports.into_iter().collect();
        let keep: HashMap<String, (LegReport, String)> = legs
            .iter()
            .filter(|(_, key, j, _)| j.as_deref().is_some_and(|j| !again.runs(j, key)))
            .filter_map(|(id, key, _, _)| last.get(key).map(|rep| (key.clone(), (report_from_json(rep), id.clone()))))
            .collect();
        let attempt = runs::start_attempt(&self.hub, run, by, agent).await?;
        let mut github = v.get("github").cloned().unwrap_or_default();
        if let Some(o) = github.as_object_mut() {
            o.insert("run_attempt".into(), serde_json::Value::String(attempt.to_string()));
            // Whoever reran it, GitHub-exact: the person (for an agent, whom it acts for).
            o.insert("triggering_actor".into(), serde_json::Value::String(login_of(by)));
        }
        let changed: Option<Vec<String>> = v.get("changed").and_then(|c| serde_json::from_value(c.clone()).ok());
        let input = RunInput { github: github.clone(), inputs: v.get("inputs").cloned().unwrap_or_default(), vars: v.get("vars").cloned().unwrap_or_default(), changed: changed.map(Arc::new), trusted: v.get("trusted").and_then(serde_json::Value::as_bool).unwrap_or(false) };
        let actions = match &dependencies {
            Some(snapshot) => snapshot.root_actions().map_err(anyhow::Error::msg)?.clone(),
            None => v.get("actions").and_then(|a| serde_json::from_value(a.clone()).ok()).unwrap_or_default(),
        };
        let timing = super::results::timing(&self.hub, &repo.id).await;
        // "Run anyway" (W08-R10): no leg of this attempt takes an earlier result over; nor, run
        // cold (W24-R23), does any start from kept folders or restore a cache.
        let no_reuse = no_reuse || cold;
        let files = Arc::new(match &dependencies { Some(s) => s.files.clone(), None => super::reusable::files(&self.hub, &repo, &event.sha, &wf).await });
        let tree = Arc::new(shard_files(&self.hub, &repo, &event.sha, &wf, &files).await);
        let graph = super::graphs::of_run(&self.hub, &repo, &event.sha, &wf, &files).await;
        let culprit = Try::of_input(&v);
        let settings = serde_json::json!({ "default_permissions": v.get("default_permissions").and_then(serde_json::Value::as_str).unwrap_or("read"), "only_matrix": v.get("only_matrix"), "timing": *timing, "tree": *tree, "no_reuse": no_reuse, "graph": graph.as_deref(), "cold": cold, "culprit": culprit, "placed": placed, "dependency_snapshot": dependencies });
        self.save_input(run, &input, &actions, &event, (debug, keep_failed), &keep, &settings).await?;
        let (plan, acts) = match RunPlan::start(Arc::clone(&wf), input.clone(), self.settings_with(&v), &Graphed { source: Source { files: &files, timing: &timing, tree: &tree }, graph }) {
            Ok(x) => x,
            Err(e) => {
                runs::finish_run(&self.hub, run, Status::Failed, "failure", Some(format!("The run could not start: {e}")), false).await?;
                self.hub.emit_run(run).await;
                return Ok(attempt);
            }
        };
        let group = plan.concurrency.as_ref().filter(|_| culprit.is_none()).map(|g| (format!("{}\0{}", repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
        let path = run_path(&github);
        let costs = super::insights::costs(&self.hub, &repo.id, &path).await;
        // A maintainer reruns it: that is approval enough, as on GitHub.
        let sup = if culprit.is_some() { None } else { sup_of(&wf, &repo, &path, &event.name, &github) };
        let mut st = RunSt { plan, repo, event, path, created: crate::util::now_unix(), in_progress: false, files, wf, github, vars: input.vars, trusted: input.trusted, actions, dependencies, keys: HashMap::new(), held: Some(acts), seq, keep, debug, keep_failed, given: Given::default(), timing: Arc::clone(&timing), tree: Arc::clone(&tree), removed: 0, costs, awaiting: None, no_reuse, who: Who::default(), sup, cold, culprit, placed };
        st.who = who_of(&self.hub, &st).await;
        self.workflow_run(&st, "requested", None);
        self.runs.insert(run.to_owned(), st);
        self.hub.ci.own_run(run);
        self.hub.emit_run(run).await;
        let cmds = self.run_queued(run, group, Arrival::Rerun);
        Box::pin(self.commands(cmds)).await;
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
        // A preview's leg deploys to it only while kiln keeps it (W15-R07): one that's gone isn't
        // made again as an environment with no rules.
        let previewed = self.legs.get(id).and_then(|l| l.spec.environment.as_ref()).is_some_and(|e| e.preview.is_some());
        let def = if previewed {
            match super::environments::defs(&self.hub, &repo).await?.remove(&env.to_ascii_lowercase()) {
                Some(d) => d,
                None => {
                    self.held.remove(id);
                    let m = format!("{env} is gone: its change's preview went, and it takes no more deployments");
                    runs::annotation(&self.hub, id, None, "error", None, &m, None, None, None, None, None).await?;
                    return Box::pin(self.settled(id, Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() }), Some(m))).await;
                }
            }
        } else {
            super::environments::ensure(&self.hub, &repo, &env).await?
        };
        let reviews = super::environments::reviews(&self.hub, &run, &def.name).await?;
        let now = crate::util::now_unix();
        let p = &def.protection;
        // A preview that went with its change takes only its change's closing runs (W15-R07).
        let closing = self.runs.get(&run).is_some_and(|r| r.event.action == "closed");
        let gone = def.preview.as_ref().and_then(|p| p.removed.clone()).filter(|_| !closing);
        let verdict = if runs::draft_of(&self.hub, &run).await?.is_some() {
            // A draft's run deploys nothing (docs/ci.md W29-R14).
            kiln_ci::env::Gate::Refused(format!("this run is a draft's, by its author's hand: it doesn't deploy to {} until the draft is published", def.name))
        } else if let Some(why) = gone {
            kiln_ci::env::Gate::Refused(format!("{} went ({why}): it takes no more deployments", def.name))
        } else if !records && !p.gates.is_empty() {
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
                    runs::wait_reason(&self.hub, id, &serde_json::json!({ "reason": "concurrency", "environment": def.name, "limit": limit }), None).await?;
                    self.hub.emit_job(id).await;
                    return Ok(());
                }
                let Some(mut h) = self.held.remove(id) else { return Ok(()) };
                runs::wait_reason(&self.hub, id, &serde_json::Value::Null, None).await?;
                self.hub.emit_job(id).await;
                // A host, container or microVM environment's jobs run on its runners (W15-R02).
                for q in super::environments::routing(&def) {
                    if !h.leg.route.requires.contains(&q) {
                        h.leg.route.requires.push(q);
                    }
                }
                let cmds = self.sched.handle(Input::LegQueued(Box::new(h.leg)), now_ms());
                if let Some(cmds) = self.after(cmds) {
                    Box::pin(self.commands(cmds)).await;
                }
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
                // W19-R06: the first time it waits for a reviewer, who wants to hear of it, recorded
                // with the wait (crate::events).
                let announce = !reviewers.is_empty() && self.held.get(id).is_some_and(|h| !h.announced);
                let waiting = announce.then_some((run.as_str(), def.name.as_str()));
                runs::wait_reason(&self.hub, id, &serde_json::json!({ "reason": reason, "environment": def.name, "reviewers": reviewers, "until": until, "window": window, "promotion": promotion, "gate": gate }), waiting).await?;
                if announce && let Some(h) = self.held.get_mut(id) {
                    h.announced = true;
                }
                // Promotion and custom gates move as other runs do: it's looked at again shortly.
                if promotion.is_some() || gate.is_some() {
                    self.timers.insert((now + GATE_AGAIN_SECS) * 1000);
                }
                self.hub.emit_job(id).await;
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
    async fn queue(&mut self, run: &str, mut spec: LegSpec) -> anyhow::Result<()> {
        // W08-R10: a job that opted in takes an earlier leg's result for the same inputs.
        let reuse = self.reuse_hash(run, &spec).await;
        if let Some((hash, max_age)) = &reuse {
            let (repo, trusted) = self.runs.get(run).map(|r| (r.repo.name.clone(), super::reuse::work_trusted(r.trusted, &r.event.name))).unwrap_or_default();
            match super::reuse::lookup(&self.hub, &repo, trusted, hash, *max_age).await {
                Ok(Some((source, report, class))) => return Box::pin(self.take_over(run, spec, &source, hash, report_from_json(&report), class)).await,
                Ok(None) => {}
                Err(e) => tracing::warn!(leg = %spec.key, error = %format!("{e:#}"), "reuse couldn't be looked up; the leg runs"),
            }
        }
        // W24-R06: a leg the change can't affect, with no result of its inputs to take: skipped, as
        // its `if` said.
        if let Some(why) = spec.unaffected.clone() {
            return Box::pin(self.unaffected(run, spec, why)).await;
        }
        // One of `previews:` named: the leg deploys to its change's preview, that environment from
        // here on (W15-R07); or to none, skipped or failed, saying why.
        let mut unbound: Option<(bool, String)> = None;
        if let Some(r) = self.runs.get(run)
            && let Some(e) = spec.environment.as_mut().filter(|e| !e.name.is_empty() && e.preview.is_none())
        {
            match super::previews::bind(&self.hub, &r.repo, &e.name, &r.event.payload).await {
                Ok(super::previews::Bind::Not) => {}
                Ok(super::previews::Bind::To(p)) => {
                    e.name = format!("{}-{}", p.template, p.id);
                    e.preview = Some(p);
                }
                Ok(super::previews::Bind::Skip(why)) => unbound = Some((true, why)),
                Ok(super::previews::Bind::Refuse(why)) => unbound = Some((false, why)),
                Err(err) => unbound = Some((false, format!("its preview couldn't be made: {err:#}"))),
            }
        }
        let id = crate::util::new_id();
        runs::insert_leg(&self.hub, run, &id, &spec, Status::Queued, None).await?;
        if let Some((hash, _)) = &reuse {
            super::reuse::set_hash(&self.hub, &id, hash).await?;
        }
        if let Some(p) = self.runs.get(run).and_then(|r| r.placed.get(&spec.key)).filter(|p| !p.is_empty()).cloned() {
            runs::set_placement(&self.hub, &id, &p).await?;
        }
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id.clone());
        self.hub.ci.own_legs([id.as_str()]);
        let mut leg = leg_in(r, run, &id, &spec, &self.hub.cfg.ci, &self.warm);
        crate::offers::ci_leg(&self.hub, &leg);
        // W28-R46: a run that adopts gives its legs work, keyed by W08-R10's input hash.
        leg.work = work_of(r, &spec, reuse.as_ref().map(|(h, _)| h.clone()));
        leg.finish_within = r.sup.as_ref().map(|s| s.finish_within);
        let dockerless = dockerless_pod(&self.sched, r, &spec, &leg.route).or_else(|| microvm_unrunnable(&self.sched, r, &spec));
        self.legs.insert(
            id.clone(),
            LegSt { run: run.to_owned(), key: spec.key.clone(), spec, attempt: 1, agent: None, report: LegReport::default(), class: None, deployment: None, env_url: None, seq: 0, gone: false, fence: Fence::default() },
        );
        self.hub.emit_job(&id).await;
        // What its kind's runs taught of its work, for the load balancer's expected finish.
        leg.profile = profile_of(&self.hub, &leg.kind).await;
        if let Some(m) = dockerless {
            runs::annotation(&self.hub, &id, None, "error", Some("executor".into()), &m, None, None, None, None, None).await?;
            return Box::pin(self.settled(&id, Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() }), Some(m))).await;
        }
        match unbound {
            Some((true, why)) => {
                runs::annotation(&self.hub, &id, None, "notice", Some("environment".into()), &why, None, None, None, None, None).await?;
                return Box::pin(self.concluded(&id, PlanC::Skipped, Some(why))).await;
            }
            Some((false, why)) => {
                runs::annotation(&self.hub, &id, None, "error", Some("environment".into()), &why, None, None, None, None, None).await?;
                return Box::pin(self.settled(&id, Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: None, log_hits: Vec::new() }), Some(why))).await;
            }
            None => {}
        }
        // A leg bound to an environment starts when its protection lets it (W15-R04).
        if let Some(env) = self.legs.get(&id).and_then(|l| l.spec.environment.as_ref()).map(|e| e.name.clone()).filter(|n| !n.is_empty()) {
            self.held.insert(id.clone(), Held { leg, env, ready_at: crate::util::now_unix(), announced: false });
            return self.gate(&id).await;
        }
        let cmds = self.sched.handle(Input::LegQueued(Box::new(leg)), now_ms());
        if let Some(cmds) = self.after(cmds) {
            Box::pin(self.commands(cmds)).await;
        }
        Ok(())
    }

    /// The leg's input hash and how long a result may be reused, when its job opts in (W08-R10)
    /// and its run may reuse.
    async fn reuse_hash(&self, run: &str, spec: &LegSpec) -> Option<(String, u64)> {
        let reuse = super::reuse::spec_of(spec)?;
        let (repo, sha, actions, vars, trusted) = {
            let r = self.runs.get(run).filter(|r| !r.no_reuse)?;
            (r.repo.clone(), r.event.sha.clone(), r.actions_for(spec).clone(), r.vars.clone(), r.trusted)
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
    async fn take_over(&mut self, run: &str, spec: LegSpec, source: &str, hash: &str, report: LegReport, class: Option<String>) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        super::reuse::take_over(&self.hub, run, source, &id, hash, class).await?;
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

    /// A leg queued only to be reused, with no result of its inputs to take (W24-R06): skipped for
    /// what its `if` (or `when`) said, and saying so; its report to the plan.
    async fn unaffected(&mut self, run: &str, spec: LegSpec, why: String) -> anyhow::Result<()> {
        let id = crate::util::new_id();
        runs::insert_leg(&self.hub, run, &id, &spec, Status::Skipped, Some(format!("{why}: the change doesn't affect it, and no earlier result of its inputs is there to reuse"))).await?;
        self.hub.emit_job(&id).await;
        let Some(r) = self.runs.get_mut(run) else { return Ok(()) };
        r.keys.insert(spec.key.clone(), id);
        r.seq += 1;
        let seq = r.seq;
        let files = Arc::clone(&r.files);
        let timing = Arc::clone(&r.timing);
        let tree = Arc::clone(&r.tree);
        let report = LegReport::of(PlanC::Skipped);
        let acts = r.plan.on_leg_finished(&spec.key, report.clone(), &Source { files: &files, timing: &timing, tree: &tree });
        runs::log(&self.hub, run, seq, "leg", Some(spec.key.clone()), Some(report_json(&report))).await?;
        self.actions(run, acts).await
    }

    /// Commands that come of carrying one out go after it, from the loop already under way: handed
    /// to `next` here, with nothing for the caller to run, so no second `commands` frame goes on the
    /// stack. A debug build gives that frame a slot for every future it awaits inline, and two of
    /// them overflowed a shard's 2 MiB stack (a queued leg's commands, from `commands`' own work).
    /// Otherwise the commands, for the caller to carry out.
    fn after(&mut self, cmds: Vec<Command>) -> Option<Vec<Command>> {
        if self.in_commands {
            self.next.extend(cmds);
            None
        } else {
            Some(cmds)
        }
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
            crate::offers::ci(&self.hub, &c);
            let r = match c {
                Command::StartRun { run } => {
                    let held = self.runs.get_mut(&run).and_then(|r| r.held.take()).unwrap_or_default();
                    Box::pin(self.actions(&run, held)).await
                }
                Command::CancelRun { run, reason } => self.cancel_run(&run, &reason).await,
                Command::Assign { leg, runner, attempt, executor, image, docker, slot, fence, lease_loss, .. } => {
                    let placed = Placement { executor, image, docker, slot, grant: Some((fence, lease_loss)), claimed: false };
                    match self.sched.gang(&leg).map(|g| g.id.clone()) {
                        // A gang's Assigns come together, out of one decision (W05-R40): its other
                        // members' are taken from what's to do, and all go as one.
                        Some(gang) => {
                            let mut members = vec![Member { leg, runner, attempt, placed }];
                            let mut rest = std::collections::VecDeque::with_capacity(todo.len());
                            for c in todo.drain(..) {
                                match c {
                                    Command::Assign { leg, runner, attempt, executor, image, docker, slot, fence, lease_loss, .. } if self.sched.gang(&leg).is_some_and(|g| g.id == gang) => {
                                        members.push(Member { leg, runner, attempt, placed: Placement { executor, image, docker, slot, grant: Some((fence, lease_loss)), claimed: false } });
                                    }
                                    other => rest.push_back(other),
                                }
                            }
                            todo = rest;
                            self.assign_gang(&gang, members).await
                        }
                        None => {
                            if let Some(l) = self.legs.get_mut(&leg) {
                                l.fence = fence;
                            }
                            self.regrant(&leg, Some((runner.clone(), attempt, fence))).await;
                            self.assign(&leg, &runner, attempt, placed).await
                        }
                    }
                }
                // A `k8s` pool's executor is this server's: its pod (W13-R07), made for a leg with
                // its containers in it when it has them.
                Command::StartExecutor { pool, executor, runner, kind, leg, image } if runner.is_empty() => {
                    match leg.as_deref().map(|l| (l.to_owned(), self.leg_pod(l, image.as_deref()))) {
                        // Its containers can't be said: the leg fails, saying why, and the executor
                        // made for it goes.
                        Some((l, Err(why))) => {
                            let attempt = self.legs.get(&l).map_or(1, |x| x.attempt);
                            self.next.extend(self.sched.handle(Input::ExecutorFailed { executor: executor.clone(), error: why.clone() }, now_ms()));
                            self.fail_unstarted(&l, attempt, "k8s", why).await
                        }
                        made => {
                            let pod = made.and_then(|(_, p)| p.ok());
                            self.pods.insert(executor.clone());
                            let hub = Arc::clone(&self.hub);
                            crate::rt::spawn(async move { super::pools::start_pod(&hub, &pool, &executor, &kind, pod).await });
                            Ok(())
                        }
                    }
                }
                Command::StopExecutor { executor, runner } if self.pods.remove(&executor) || runner.starts_with("kiln-x-") => {
                    let hub = Arc::clone(&self.hub);
                    crate::rt::spawn(async move { super::pools::stop_pod(&hub, &executor, &runner).await });
                    Ok(())
                }
                Command::StartExecutor { pool, executor, runner, kind, image, leg } => {
                    let pinned = leg.as_deref().map(|l| self.leg_pool_image(l, image.as_deref())).transpose();
                    match pinned {
                        Err(why) => {
                            let l = leg.as_deref().unwrap_or_default();
                            let attempt = self.legs.get(l).map_or(1, |x| x.attempt);
                            self.next.extend(self.sched.handle(Input::ExecutorFailed { executor: executor.clone(), error: why.clone() }, now_ms()));
                            self.fail_unstarted(l, attempt, "dependency-lock", why).await
                        }
                        Ok(pinned) => {
                            let mut ask = super::pools::start_ask(&self.hub, &pool, &executor, &kind, pinned.as_deref().or(image.as_deref()));
                            if leg.is_some() && let Some(ask) = ask.as_object_mut() { ask.insert("dependency_locked".into(), serde_json::json!(true)); }
                            self.hub.send_to_agent(&runner, LANE_CONTROL, &ServerMsg::Executor { json: ask.to_string() }).await;
                            Ok(())
                        }
                    }
                }
                Command::StopExecutor { executor, runner } => {
                    let ask = serde_json::json!({ "op": "stop", "executor": executor });
                    self.hub.send_to_agent(&runner, LANE_CONTROL, &ServerMsg::Executor { json: ask.to_string() }).await;
                    Ok(())
                }
                Command::Cancel { leg, reason, .. } => {
                    if let Some((agent, attempt, fence)) = self.legs.get(&leg).and_then(|l| l.agent.clone().map(|a| (a, l.attempt, l.fence))) {
                        // A lease runner is told which grant to stop (W05-R34): another of the leg's
                        // it runs since is let be.
                        let msg = if self.hub.runner_wide_caps(&agent) & CAP_LEASES != 0 && fence != Fence::default() {
                            ServerMsg::Stop { grant: crate::proto::Grant::of(&leg, attempt, fence), reason }
                        } else {
                            ServerMsg::Cancel { job: leg, reason }
                        };
                        self.hub.command_start(&agent, &msg).await.log_when_answered(crate::hub::LOG_WITHIN);
                    }
                    Ok(())
                }
                Command::Retry { leg, attempt, at } => {
                    self.regrant(&leg, None).await;
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
                    let mut value = serde_json::to_value(&reason).unwrap_or_default();
                    if matches!(reason, kiln_ci::sched::WaitReason::Capability { .. })
                        && self.legs.get(&leg).and_then(|l| self.runs.get(&l.run).map(|r| (l, r)))
                            .is_some_and(|(l, r)| r.dependencies.as_ref().and_then(|s| s.execution_for(&l.spec.defined_in, &l.spec.job)).is_some())
                        && let Some(object) = value.as_object_mut()
                    { object.insert("message".into(), "This locked job needs a runner that enforces dependency locks (W29-R24).".into()); }
                    let r = runs::wait_reason(&self.hub, &leg, &value, None).await;
                    self.hub.emit_job(&leg).await;
                    r
                }
                Command::Settled { leg, outcome, reason } => self.settled(&leg, outcome, reason).await,
                Command::Timer { at } => {
                    self.timers.insert(at);
                    Ok(())
                }
                // W28-R47: every decision in demands says why, on the leg's page (a notice) and in
                // its routing (`demand`).
                Command::Demand { leg, decision } => {
                    let r = runs::annotation(&self.hub, &leg, None, "notice", Some("demand".into()), &decision.why(), None, None, None, None, None).await;
                    self.decisions.insert(leg.clone(), decision);
                    self.hub.emit_job(&leg).await;
                    r
                }
                Command::Adopted { leg, from, outcome } => self.adopted(&leg, &from, outcome).await,
                // Its pool's executor was late, so it goes to the overflow's runners (W28-R80): the
                // leg says why, on its run's page and in its routing.
                Command::Overflowed { leg, why } => {
                    let r = runs::annotation(&self.hub, &leg, None, "notice", Some("overflow".into()), &why, None, None, None, None, None).await;
                    self.hub.emit_job(&leg).await;
                    r
                }
                // A grant the scheduler took back (W05-R34): its attempt ended so, and its runner
                // holds it no more; the `Retry` or `Settled` that follows does the rest.
                Command::Lost { leg, attempt, runner, fence, why } => {
                    self.hub.release_slot(&runner, &leg);
                    self.regrant(&leg, None).await;
                    let end = runs::AttemptEnd { conclusion: "failure", class: Some(FailureClass::RunnerLost.as_str()), exit_code: None, error: Some(why.why().to_owned()), peak_rss: None, cpu_seconds: None };
                    let r = runs::attempt_finished(&self.hub, &leg, attempt, Some(fence), end).await.map(|_| ());
                    if why == kiln_ci::sched::Loss::Gang {
                        let stopped = self.stop_member(&leg, attempt, &runner, fence).await;
                        r.and(stopped)
                    } else {
                        r
                    }
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

    /// What a `k8s` pod made for a leg holds (W13-R07): its `container:` as the job container, its
    /// `services:` beside it, their keys evaluated as the runner evaluates a job container's (with
    /// the run's `github`, `vars`, `needs`, `strategy`, `matrix` and `inputs`). Why not, when one of
    /// them can't be: an expression that fails, or `credentials` (a pod pulls with its namespace's
    /// image pull secrets).
    /// W29-R24: a pool boots only this leg's accepted image, before it receives an assignment.
    fn leg_pool_image(&self, leg: &str, image: Option<&str>) -> Result<String, String> {
        let l = self.legs.get(leg).ok_or("KCI4050 the locked leg is gone")?;
        let r = self.runs.get(&l.run).ok_or("KCI4050 its locked run is gone")?;
        let locks = r.dependencies.as_ref().and_then(|s| s.execution_for(&l.spec.defined_in, &l.spec.job)).ok_or("KCI4050 its execution lock is missing")?;
        locks.image(image.ok_or("KCI4050 the pool names no locked image")?)
    }

    fn leg_pod(&self, leg: &str, pool_image: Option<&str>) -> Result<super::pools::LegPod, String> {
        use kiln_ci::expr::{self, Budget, MapContexts, Template, Value};
        use kiln_ci::wf::validate::{Site, options};
        let l = self.legs.get(leg).ok_or("the leg is gone")?;
        let r = self.runs.get(&l.run).ok_or("its run is gone")?;
        let job = l.spec.def().ok_or("its job is gone")?;
        let dialect = l.spec.workflow.dialect.unwrap_or(expr::Dialect::Gha);
        let cx = MapContexts::default()
            .with("github", Value::from_json(&r.github))
            .with("vars", Value::from_json(&r.vars))
            .with("needs", Value::from_json(&l.spec.needs))
            .with("strategy", Value::from_json(&l.spec.strategy))
            .with("matrix", Value::from_json(&l.spec.matrix))
            .with("inputs", Value::from_json(&l.spec.inputs));
        let render = |t: &kiln_ci::wf::ir::Tpl, site: Site, what: &str| -> Result<String, String> {
            let tpl = Template::parse_with(&t.src, &options(site, dialect)).map_err(|e| format!("{what}: {}", e.message))?;
            expr::render(&tpl, &cx, &mut Budget::default()).map_err(|e| format!("{what}: {}", e.message))
        };
        // `6379`, `8080:80`, `80/tcp`: the port the container listens on (on the pod's
        // `localhost`, where the job container reaches it).
        let ports = |c: &kiln_ci::wf::ir::Container| -> Result<Vec<u16>, String> {
            let mut out = Vec::new();
            for p in &c.ports {
                let text = match p {
                    kiln_ci::wf::ir::YVal::Num(n) => format!("{n}"),
                    kiln_ci::wf::ir::YVal::Str(t) => render(t, Site::JobServices, "ports")?,
                    other => return Err(format!("ports: `{}` isn't a port", serde_json::to_string(other).unwrap_or_default())),
                };
                let port = text.rsplit(':').next().and_then(|x| x.split('/').next()).and_then(|x| x.trim().parse::<u16>().ok()).ok_or_else(|| format!("ports: `{text}` isn't a port"))?;
                out.push(port);
            }
            Ok(out)
        };
        let one = |c: &kiln_ci::wf::ir::Container, site: Site, env_site: Site, what: &str| -> Result<(String, Vec<(String, String)>), String> {
            if c.credentials.is_some() {
                return Err(format!("{what}: `credentials` aren't how a k8s pool's pod pulls: give its namespace's service account an image pull secret instead"));
            }
            let image = render(&c.image, site, &format!("{what}.image"))?;
            let env = c.env.iter().map(|(k, v)| render(v, env_site, &format!("{what}.env.{k}")).map(|v| (k.clone(), v))).collect::<Result<Vec<_>, _>>()?;
            Ok((image, env))
        };
        let (image, env) = match &job.container {
            Some(c) => {
                let (i, e) = one(c, Site::JobContainer, Site::JobContainerEnv, "container")?;
                (Some(i), e)
            }
            None => (None, Vec::new()),
        };
        let mut services = Vec::new();
        for (id, c) in &job.services {
            let (image, env) = one(c, Site::JobServices, Site::JobServiceEnv, &format!("services.{id}"))?;
            services.push(super::pools::PodService { id: id.clone(), image, env, ports: ports(c)? });
        }
        let containers = job.container.is_some() || !services.is_empty();
        // A kiln-dialect leg that sets its `toolchain:`, `image:` or `resources:` gets v1's job pod
        // (W02-R06): `image:` (else `toolchain:`'s) unless its `container:` is the job container.
        if dialect == expr::Dialect::Kiln && super::pools::sets_pod(job) {
            let image = match (image, &job.kiln.image) {
                (Some(i), _) => Some(i),
                (None, Some(t)) => Some(render(t, Site::KilnJob, "image")?).filter(|i| !i.trim().is_empty()),
                (None, None) => None,
            };
            let mut pod = super::pools::LegPod { env, services, containers, ..super::pools::kiln_pod(job, image, l.spec.timeout_minutes) };
            lock_pod(&mut pod, pool_image, r.dependencies.as_ref().and_then(|s| s.execution_for(&l.spec.defined_in, &l.spec.job)))?;
            return Ok(pod);
        }
        let mut pod = super::pools::LegPod { image, env, services, containers, ..super::pools::LegPod::default() };
        lock_pod(&mut pod, pool_image, r.dependencies.as_ref().and_then(|s| s.execution_for(&l.spec.defined_in, &l.spec.job)))?;
        Ok(pod)
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

    /// The executor a grant's records name: in a `k8s` pool's pod a leg runs in the pod's own
    /// container (W13-R07), so a terminal into it is an exec into the pod, not a shell on a machine
    /// (W17-R01).
    fn recorded(&self, runner: &str, executor: Option<&str>) -> String {
        if self.hub.pod_executors.load().contains_key(runner) { "k8s".into() } else { executor.unwrap_or("host").to_owned() }
    }

    /// A gang's Assigns (W05-R40): their grants written together, in one transaction, before any
    /// of them goes (W05-R05 grown: a grant is in the store before anything acts on it, G10, and a
    /// gang's all or none of them, G4); then each goes as [`Self::assign`] sends one. A write that
    /// fails sends none, and tells the scheduler each member is `Ungranted`, so the gang goes back
    /// to its queue whole.
    async fn assign_gang(&mut self, gang: &str, members: Vec<Member>) -> anyhow::Result<()> {
        let mut grants = Vec::with_capacity(members.len());
        for m in &members {
            let fence = m.placed.grant.map(|(f, _)| f).unwrap_or_default();
            if let Some(l) = self.legs.get_mut(&m.leg) {
                l.fence = fence;
                grants.push((m.leg.clone(), m.runner.clone(), self.recorded(&m.runner, m.placed.executor.as_deref()), m.attempt, fence));
            }
            self.regrant(&m.leg, Some((m.runner.clone(), m.attempt, fence))).await;
        }
        if let Err(e) = claim_all(&self.hub, &grants).await {
            tracing::warn!(gang, members = members.len(), error = %format!("{e:#}"), "a gang's grants couldn't be written: none of its Assigns is sent, and the gang goes back to its queue whole");
            for m in &members {
                self.regrant(&m.leg, None).await;
                let fence = m.placed.grant.map(|(f, _)| f).unwrap_or_default();
                let cmds = self.sched.handle(Input::Ungranted { leg: m.leg.clone(), fence }, now_ms());
                self.next.extend(cmds);
            }
            return Ok(());
        }
        for m in members {
            if let Err(e) = self.assign(&m.leg, &m.runner, m.attempt, Placement { claimed: true, ..m.placed }).await {
                tracing::warn!(leg = %m.leg, error = %format!("{e:#}"), "CI dispatcher command");
            }
        }
        Ok(())
    }

    /// A gang's member its runner refused (W05-R40), its gang taken back: queued again, its records
    /// saying who refused it.
    async fn refused_member(&mut self, leg: &str, runner: &str) -> anyhow::Result<()> {
        let Some(attempt) = self.legs.get(leg).map(|l| l.attempt) else { return Ok(()) };
        tracing::info!(leg, runner, attempt, "a member of its gang was refused: its gang goes again whole");
        runs::annotation(&self.hub, leg, None, "notice", Some("gang".into()), &format!("Refused by {runner}: its gang goes again whole, together."), None, None, None, None, None).await?;
        runs::requeue(&self.hub, leg, attempt, Some(format!("refused by {runner}: its gang goes again whole"))).await?;
        self.hub.emit_job(leg).await;
        Ok(())
    }

    /// A gang's member taken back with its gang (W05-R40), another member refused or lost: its
    /// runner, which was sent it, is told to stop it now, as a grant lost is once its runner says
    /// it holds it (W05-R25): `Stop` naming its grant for a lease runner, a cancel for an older
    /// one. It's queued again, its records saying why. One whose `Assign` never went (its gang's
    /// write failed) has nothing to stop or say.
    async fn stop_member(&mut self, leg: &str, attempt: u32, runner: &str, fence: Fence) -> anyhow::Result<()> {
        let Some(l) = self.legs.get_mut(leg).filter(|l| l.agent.as_deref() == Some(runner)) else { return Ok(()) };
        l.agent = None;
        l.report = LegReport::default();
        l.class = None;
        let why = kiln_ci::sched::Loss::Gang.why();
        let msg = if self.hub.runner_wide_caps(runner) & CAP_LEASES != 0 && fence != Fence::default() {
            ServerMsg::Stop { grant: crate::proto::Grant::of(leg, attempt, fence), reason: why.to_owned() }
        } else {
            ServerMsg::Cancel { job: leg.to_owned(), reason: why.to_owned() }
        };
        self.hub.command_start(runner, &msg).await.log_when_answered(crate::hub::LOG_WITHIN);
        tracing::info!(leg, runner, attempt, "a member of its gang was refused or lost: this one is stopped, and its gang goes again whole");
        runs::annotation(&self.hub, leg, None, "notice", Some("gang".into()), &format!("Stopped on {runner}: {why}."), None, None, None, None, None).await?;
        runs::requeue(&self.hub, leg, attempt, Some(format!("stopped on {runner}: its gang goes again whole"))).await?;
        self.hub.emit_job(leg).await;
        Ok(())
    }

    async fn assign(&mut self, leg: &str, runner: &str, attempt: u32, placed: Placement) -> anyhow::Result<()> {
        let Placement { executor, image, docker, slot, grant, claimed } = placed;
        let Some(l) = self.legs.get(leg) else { return Ok(()) };
        let Some(r) = self.runs.get(&l.run) else { return Ok(()) };
        let (cache_mode, cache) = cache_access(&l.spec, r);
        let ttl = (i64::from(l.spec.timeout_minutes) + 60) * 60;
        // Both tokens are this attempt's, under its grant: taken only while that's the leg's live
        // grant, they die with it (W10-R03).
        let held = grant.map(|(fence, _)| (attempt, fence));
        let token = super::token::mint(&self.hub, &super::token::Leg { leg, run: &l.run, repo: &r.repo.id, cache, cache_mode: cache_mode.as_str(), ttl, grant: held });
        // GITHUB_TOKEN: a job token with the job's permissions, revoked when its grant ends
        // (W10-R03, R04).
        let job_token = super::token::mint_job(&self.hub, leg, &l.run, &r.repo.id, &l.spec.permissions, ttl, held).await?;
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
        // Its grant (W05-R34): what its runner names what it sends about the leg by, and its lease.
        if let Some((fence, loss)) = grant {
            (a.fence, a.lease_ms, a.on_lease_loss) = (Some(fence), self.sched.lease(), loss);
        }
        // Its environment's kind: a `container` or `microvm` one's deployment is the runner's to make
        // (W15-R02).
        if let Some(b) = a.environment.as_mut() {
            b.kind = super::environments::ensure(&self.hub, &r.repo, &b.name).await.ok().and_then(|d| d.kind);
        }
        // A one-leg runner in a pod (`kiln agent --ephemeral`: a job's, a `k8s` pool's) has the
        // CPUs its cluster gives it, its node's, which the node's other pods share: CPUs of the
        // leg's own taken there would be the same first ones in each pod (W05-R03).
        let pod = self.hub.pod_executors.load().contains_key(runner) || self.hub.agents.load().get(runner).is_some_and(|g| g.ephemeral);
        if pod && let Some(o) = a.executor.options.as_object_mut() {
            o.remove("cpus");
        }
        // An action it uses that couldn't be resolved (W07-R03) fails it as it starts, as GitHub
        // fails a job at "Set up job"; the run's other jobs go on.
        if let Some(m) = l.spec.def().and_then(|j| unresolved(&j.steps, r.actions_for(&l.spec))) {
            return self.fail_unstarted(leg, attempt, "actions", m).await;
        }
        // KCI4031 (W07-R11): a kiln action that needs more permissions than the job grants.
        if let Some(m) = l.spec.def().and_then(|j| short_permissions(&j.steps, r.actions_for(&l.spec), &l.spec.permissions)) {
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
            a.executor.options = with_docker(std::mem::take(&mut a.executor.options), &d);
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
        let recorded = self.recorded(runner, executor.as_deref());
        // The grant is written before its Assign goes (W05-R05, G10), a gang's member's with its
        // gang's already. One that couldn't be sends none: it ends here, and the leg goes back to
        // its queue, the same attempt, a while later.
        let fence = grant.map(|(f, _)| f).unwrap_or_default();
        if !claimed
            && let Err(e) = claim(&self.hub, leg, runner, &recorded, attempt, fence).await
        {
            tracing::warn!(leg, runner, attempt, error = %format!("{e:#}"), "the grant couldn't be written: its Assign isn't sent, and the leg goes back to its queue");
            self.regrant(leg, None).await;
            let cmds = self.sched.handle(Input::Ungranted { leg: leg.to_owned(), fence }, now_ms());
            self.next.extend(cmds);
            return Ok(());
        }
        // What kiln hands the leg, as its receipt of it will name it (docs/ci.md W27-R05).
        if let Err(e) = super::signed::handed(&self.hub, leg, attempt, &json).await {
            tracing::warn!(leg, error = %format!("{e:#}"), "what the leg was handed could not be recorded");
        }
        // The run's first leg to start: `workflow_run` `in_progress`.
        if let Some(run) = self.legs.get(leg).map(|l| l.run.clone()) {
            let first = self.runs.get_mut(&run).is_some_and(|r| !std::mem::replace(&mut r.in_progress, true));
            if first && let Some(r) = self.runs.get(&run) {
                self.workflow_run(r, "in_progress", None);
            }
        }
        // What it holds there, so the v1 dispatcher packs its jobs beside it (W05-R03).
        let incoming = kiln_ci::sched::state::Incoming::of(a.exclusive, &a.holds.unwrap_or_default());
        self.hub.change_agent(runner, |ag| {
            ag.running.push(leg.to_owned());
            ag.busy = u32::try_from(ag.running.len()).unwrap_or(u32::MAX);
            crate::hub::hold_on(ag, leg, incoming);
        });
        if let Some(l) = self.legs.get_mut(leg) {
            l.agent = Some(runner.to_owned());
            l.seq = 0;
            if deployment.is_some() {
                l.deployment = deployment;
            }
        }
        let _rx = self.hub.logs.subscribe(leg);
        tracing::info!(leg, runner, attempt, "leg assigned");
        // A live command: refused at once while its runner is away. Once taken, the leg's own
        // answer (JobStarted or Reject) says what came of it.
        let pending = self.hub.command_start(runner, &ServerMsg::AssignV2 { json }).await;
        match pending.at_once().filter(|a| **a != crate::wire::commands::Answer::Sent).cloned() {
            Some(a) => {
                self.hub.release_slot(runner, leg);
                self.regrant(leg, None).await;
                if let Some(l) = self.legs.get_mut(leg) {
                    l.agent = None;
                }
                runs::requeue(&self.hub, leg, attempt, Some(format!("the assignment could not be delivered: {}", a.words()))).await?;
                let fence = self.legs.get(leg).map(|l| l.fence).unwrap_or_default();
                // Its runner is away: it takes nothing new until it's back (its hello), and the leg
                // isn't kept off it for this (W05-R35); the grant goes back, the same attempt.
                let mut cmds = self.sched.handle(Input::RunnerDraining { runner: runner.to_owned() }, now_ms());
                cmds.extend(self.sched.handle(Input::Rejected { leg: leg.to_owned(), fence }, now_ms()));
                if let Some(cmds) = self.after(cmds) {
                    Box::pin(self.commands(cmds)).await;
                }
            }
            None => pending.log_when_answered(crate::hub::LOG_WITHIN),
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
            used.names.extend(declared_sources(&j.steps, r.actions_for(spec)).into_iter().map(|(_, _, from, _)| from));
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
        let gpu_request = request_of(job, &self.hub.cfg.ci);
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
            leg: wire::LegInfo { job_id: spec.job.clone(), name: spec.name.clone(), matrix: spec.matrix.clone(), index: num("job-index"), total: num("job-total"), requires: if gpu_request.gpu > 0 { spec.requires.clone() } else { Vec::new() } },
            // The image for container and microVM executors (W13-R04, R05).
            executor: wire::ExecutorSpec {
                image: if matches!(executor.as_deref(), Some("docker" | "firecracker")) { image } else { None },
                kind: executor.unwrap_or_else(|| "host".into()),
                options: executor_options(&self.hub.cfg.ci.scheduler.mode, &request_of(job, &self.hub.cfg.ci)),
                slot: None,
            },
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
                        .chain(kiln_ci::store::v1_artifacts_step(&j.kiln.artifacts, &spec.key))
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
            // The workflow file that defines the job (W06-R03), for `job.workflow_*`.
            job_workflow: Some(spec.defined_in.clone()).filter(|d| !d.workflow_file_path.is_empty()),
            environment_url: job.and_then(|j| j.environment.as_ref()).and_then(|e| e.url.clone()),
            environment: l.spec.environment.as_ref().filter(|e| !e.name.is_empty()).map(|e| wire::Bound { name: e.name.clone(), kind: None, preview: e.preview.clone() }),
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
            actions: r.actions_for(spec).clone(),
            debug: wire::Debug { runner_debug: r.debug, step_debug: r.debug, keep_on_failure_minutes: keep_minutes(job.and_then(|j| j.kiln.debug.as_ref())).or(r.keep_failed) },
            exports_env: job.map(|j| j.kiln.exports_env.clone()).unwrap_or_default(),
            secret_outputs: job.map(|j| j.outputs.iter().filter(|o| o.secret).map(|o| o.name.clone()).collect()).unwrap_or_default(),
            run_env: wf.run_env,
            quarantine: job.is_some_and(|j| j.kiln.quarantine_enforce),
            integrations: Vec::new(),
            limits: wire::Limits::default(),
            network: egress_of(job.and_then(|j| j.kiln.network.as_ref()), r.trusted, &self.hub.cfg.ci.untrusted.allow_hosts, leg),
            trusted: r.trusted,
            // The job's warm workspace (`work: { warm }`, W24-R11), kept by its runner per lane.
            warm: job.and_then(|j| kiln_ci::work::slots::warm(j.kiln.work.as_ref())),
            cold: r.cold,
            // Exclusive, it has its runner to itself: the share it asked for, or the whole runner.
            exclusive: job.is_some_and(|j| j.kiln.exclusive),
            holds: job.filter(|j| j.kiln.exclusive).and_then(|j| asked_of(Some(j))).filter(|q| !q.cpu.is_zero()),
            request: (gpu_request.gpu > 0).then_some(gpu_request),
            // Its grant, as its assignment gives it (W05-R34).
            fence: None,
            lease_ms: None,
            on_lease_loss: kiln_ci::sched::LeaseLoss::default(),
            dependency_locks: r.dependencies.as_ref().and_then(|s| s.execution_for(&spec.defined_in, &spec.job)).cloned(),
        }
    }

    /// What a runner says about a leg while it runs (W06, §2.5).
    async fn event(&mut self, agent: &str, leg: &str, event: crate::proto::JobEvent, grant: Option<&crate::proto::Grant>) -> anyhow::Result<()> {
        // Under the grant it names, taken only while that's the leg's live grant, this runner's
        // (W05-R36): the hub let it by, and it may have been lost or granted again since.
        if let Some(g) = grant
            && (g.leg != leg || !self.live(agent, g))
        {
            refuse_write(&self.hub, agent, g, "event").await;
            return Ok(());
        }
        if let (Some(seq), Some(l)) = (event.seq, self.legs.get_mut(leg)) {
            if seq <= l.seq {
                return Ok(());
            }
            l.seq = seq;
        }
        match event.event {
            JobEventV2::StepStarted { idx, name, kind, action } => {
                runs::step_started(&self.hub, leg, idx, &name, &kind, action).await?;
                self.hub.logs.step(leg, idx, Status::Running.as_str());
                self.hub.emit_job(leg).await;
            }
            JobEventV2::StepFinished { idx, outcome, conclusion, exit_code, outputs } => {
                let status = runs::step_status(outcome, conclusion);
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
    async fn finished(&mut self, agent: &str, r: JobResult, grant: Option<crate::proto::Grant>) -> anyhow::Result<()> {
        // Under the grant it names, taken only while that's the leg's live grant, this runner's
        // (W05-R36): an attempt lost, or the leg granted again since, ends nothing.
        if let Some(g) = &grant
            && (g.leg != r.job || !self.live(agent, g))
        {
            refuse_write(&self.hub, agent, g, "result").await;
            return Ok(());
        }
        self.hub.release_slot(agent, &r.job);
        // Gone with its repository: its slot is free now, and there's nothing to write.
        if let Some(attempt) = self.legs.get(&r.job).filter(|l| l.gone).map(|l| l.attempt) {
            self.forget_gone(&r.job, attempt).await;
            return Ok(());
        }
        // A result sent again (its receipt was lost with the connection), or of a leg given up
        // while its runner was away: the leg has gone on without it.
        if self.legs.get(&r.job).is_some_and(|l| l.agent.as_deref() != Some(agent)) {
            return Ok(());
        }
        self.hub.logs.finish(&r.job);
        // Its grant ends with its result, and its tokens with it (W10-R03).
        self.regrant(&r.job, None).await;
        let Some(l) = self.legs.get(&r.job) else { return Ok(()) };
        let attempt = grant.as_ref().map_or(l.attempt, |g| g.attempt);
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
        // The store takes it only while its grant is the leg's, running (W05-R36): one that raced
        // a new grant ends nothing.
        match runs::attempt_finished(&self.hub, &r.job, attempt, grant.as_ref().map(crate::proto::Grant::fence), end).await {
            Ok(false) => {
                if let Some(g) = &grant {
                    refuse_write(&self.hub, agent, g, "result").await;
                }
                return Ok(());
            }
            Ok(true) => {}
            Err(e) => tracing::warn!(leg = %r.job, error = %format!("{e:#}"), "the attempt could not be recorded"),
        }
        let cmds = self.sched.handle(Input::AttemptFinished { leg: r.job.clone(), attempt, outcome }, now_ms());
        Box::pin(self.commands(cmds)).await;
        Ok(())
    }

    /// A leg gone with its repository that ended (or was lost with its runner): out of the
    /// dispatcher, and its slot the scheduler's to give.
    async fn forget_gone(&mut self, leg: &str, attempt: u32) {
        self.regrant(leg, None).await;
        self.legs.remove(leg);
        self.hub.ci.drop_legs([leg]);
        crate::offers::leg_done(&self.hub, leg);
        self.hub.logs.finish(leg);
        let cmds = self.sched.handle(Input::AttemptFinished { leg: leg.to_owned(), attempt, outcome: Outcome::Cancelled }, now_ms());
        if let Some(cmds) = self.after(cmds) {
            Box::pin(self.commands(cmds)).await;
        }
    }

    /// A deleted repository's runs, gone with their records: each leg out of the scheduler, a
    /// running one stopped on its runner and its slot free once it ended; nothing written,
    /// there's nowhere to.
    async fn repo_deleted(&mut self, repo: &str) {
        let runs: Vec<String> = self.runs.iter().filter(|(_, r)| r.repo.id == repo).map(|(id, _)| id.clone()).collect();
        for run in runs {
            let legs: Vec<String> = self.legs.iter().filter(|(_, l)| l.run == run).map(|(id, _)| id.clone()).collect();
            for leg in legs {
                // Its runner hears the cancel while the leg still says where it runs.
                let cmds = self.sched.handle(Input::LegGone { leg: leg.clone() }, now_ms());
                Box::pin(self.commands(cmds)).await;
                match self.legs.get_mut(&leg) {
                    // Running: kept until its runner says it ended (its slot is the runner's until then).
                    Some(l) if l.agent.is_some() => l.gone = true,
                    _ => {
                        self.legs.remove(&leg);
                        self.held.remove(&leg);
                        self.hub.ci.drop_legs([leg.as_str()]);
                        crate::offers::leg_done(&self.hub, &leg);
                    }
                }
            }
            self.runs.remove(&run);
            self.hub.ci.drop_run(&run);
            let cmds = self.sched.handle(Input::RunFinished { run }, now_ms());
            Box::pin(self.commands(cmds)).await;
        }
    }

    /// The leg is done for good: its row, then its plan (W04-R02).
    async fn settled(&mut self, leg: &str, outcome: Outcome, reason: Option<String>) -> anyhow::Result<()> {
        // Done for good: it holds no grant (W05-R36).
        self.regrant(leg, None).await;
        let c = match &outcome {
            Outcome::Success => PlanC::Success,
            Outcome::Cancelled => PlanC::Cancelled,
            Outcome::Failed(f) if f.class == FailureClass::Timeout => PlanC::TimedOut,
            Outcome::Failed(_) => PlanC::Failure,
        };
        self.concluded(leg, c, reason).await
    }

    /// A leg's end, concluded `c` (a skip too: a stack's lower diff deploys nothing to its
    /// preview, W15-R07): its deployment's last status, its row, and the plan told.
    async fn concluded(&mut self, leg: &str, c: PlanC, reason: Option<String>) -> anyhow::Result<()> {
        let Some(l) = self.legs.remove(leg) else { return Ok(()) };
        self.decisions.remove(leg);
        // A deployment ends here: the next its environment holds may go (W15-R01).
        if let Some((env, repo)) = l.spec.environment.as_ref().map(|e| e.name.clone()).zip(self.runs.get(&l.run).map(|r| r.repo.id.clone())) {
            Box::pin(self.next_deployments(&repo, &env)).await?;
        }
        // A preview that went while this leg deployed to it: what it made there goes too (W15-R07).
        if let (Some(e), Some(runner), Some(repo)) = (l.spec.environment.as_ref().filter(|e| e.preview.is_some()), l.agent.as_deref(), self.runs.get(&l.run).map(|r| r.repo.clone())) {
            super::previews::leg_ended(&self.hub, &repo, &e.name, runner).await;
        }
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
        }
        // W19-R06: `deploy.succeeded` or `deploy.failed`, recorded with the leg's end (crate::events).
        let deployed = l
            .deployment
            .filter(|_| matches!(c, PlanC::Success | PlanC::Failure | PlanC::TimedOut))
            .and(l.spec.environment.as_ref())
            .map(|e| runs::Deployed { run: l.run.clone(), environment: e.name.clone(), succeeded: c == PlanC::Success });
        // A timeout stays `timed_out` on the leg (W06-R12); its dependants see a failure.
        runs::finish_leg(&self.hub, leg, status_of(c), c.as_str(), outputs, reason, deployed).await?;
        self.hub.emit_job(leg).await;
        let report = LegReport { conclusion: Some(c), ..l.report };
        // W08-R10: a success of a job that opted in is kept for the legs with its inputs, in the work
        // ledger too (W24-R06).
        let (repo, trusted) = self.runs.get(&l.run).map(|r| (r.repo.name.clone(), super::reuse::work_trusted(r.trusted, &r.event.name))).unwrap_or_default();
        if c == PlanC::Success
            && let Ok(Some(hash)) = super::reuse::hash_of(&self.hub, leg).await
            && let Err(e) = super::reuse::remember(&self.hub, &repo, trusted, leg, &l.run, &hash, &report_json(&report)).await
        {
            tracing::warn!(leg = %leg, error = %format!("{e:#}"), "a result couldn't be kept for reuse");
        }
        // `[updates]`: a leg of kiln's own release pipeline left the binaries runners upgrade to
        // (its `artifacts:`), kept beside the dispatcher, which reading them would hold up.
        if c == PlanC::Success && self.runs.get(&l.run).is_some_and(|r| r.repo.name == self.hub.cfg.updates.release_repo) {
            let (hub, leg, run) = (Arc::clone(&self.hub), leg.to_owned(), l.run.clone());
            crate::rt::spawn(async move {
                if let Ok(Some(run)) = hub.store.run(run).await {
                    hub.adopt_release(&leg, &run).await;
                }
            });
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

    /// Some of a run's legs cancelled by their ids, the run going on (docs/ci.md W26-R21): each
    /// one of the run that hasn't finished goes to its plan by its key, logged so a restart cancels
    /// it again; the others say why not.
    async fn cancel_legs(&mut self, run: &str, ids: &[String], reason: &str) -> anyhow::Result<Vec<(String, Result<(), String>)>> {
        let (run_owned, wanted) = (run.to_owned(), ids.to_vec());
        let rows: Vec<(String, String, String)> = self
            .hub
            .store
            .call(move |c| {
                let mut st = c.prepare("SELECT id, key, status FROM jobs WHERE run_id = ?1 AND id IN (SELECT value FROM json_each(?2))")?;
                let rows = st.query_map(rusqlite::params![run_owned, serde_json::to_string(&wanted).unwrap_or_default()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        let mut each = Vec::new();
        let mut keys = Vec::new();
        for id in ids {
            match rows.iter().find(|(r, ..)| r == id) {
                None => each.push((id.clone(), Err("not a leg of this run".to_owned()))),
                Some((_, key, status)) if crate::store::Status::parse(status).is_terminal() => each.push((id.clone(), Err(format!("`{key}` already finished: it's {status}")))),
                Some((_, key, _)) => {
                    keys.push(key.clone());
                    each.push((id.clone(), Ok(())));
                }
            }
        }
        if keys.is_empty() {
            return Ok(each);
        }
        let Some(r) = self.runs.get_mut(run) else {
            return Ok(ids.iter().map(|i| (i.clone(), Err("the run isn't going: nothing of it can be cancelled".to_owned()))).collect());
        };
        r.seq += 1;
        let seq = r.seq;
        let files = Arc::clone(&r.files);
        let timing = Arc::clone(&r.timing);
        let tree = Arc::clone(&r.tree);
        let acts = r.plan.on_cancel_legs(&keys, reason, &Source { files: &files, timing: &timing, tree: &tree });
        for key in &keys {
            runs::log(&self.hub, run, seq, "cancel_leg", Some(key.clone()), Some(serde_json::json!({ "reason": reason }).to_string())).await?;
        }
        Box::pin(self.actions(run, acts)).await?;
        Ok(each)
    }

    /// Rebuilds unfinished runs by replaying their plans' logs (W05-R25); legs that were running
    /// wait for their runner to say it still has them.
    /// W05-AC09: publish recovered explanations together before any following action
    /// can assign, settle or otherwise change those legs.
    async fn restore_waits(&self, waits: &mut Vec<(String, serde_json::Value)>) {
        let batch = std::mem::take(waits);
        if batch.is_empty() {
            return;
        }
        let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
        if let Err(e) = runs::wait_reasons(&self.hub, batch).await {
            tracing::warn!(legs = ids.len(), error = %format!("{e:#}"), "recovered wait reasons could not be kept");
        }
        for id in ids {
            self.hub.emit_job(&id).await;
        }
    }

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
                runs::finish_run(&self.hub, &run, Status::Failed, "failure", Some(format!("The run could not be rebuilt after a restart: {e:#}")), false).await?;
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
        let dependencies = super::locks::Frozen::saved(&v, &repo.name, &wf.path, &row.sha).map_err(anyhow::Error::msg)?.map(Arc::new);
        let files = Arc::new(match &dependencies { Some(s) => s.files.clone(), None => super::reusable::files(&self.hub, &repo, &row.sha, &wf).await });
        let timing = saved_timing(&v);
        let tree = saved_tree(&v);
        let graph = match saved_graph(&v) {
            Some(g) => Some(g),
            None => super::graphs::of_run(&self.hub, &repo, &row.sha, &wf, &files).await,
        };
        let (mut plan, initial) = RunPlan::start(Arc::clone(&wf), input.clone(), self.settings_with(&v), &Graphed { source: Source { files: &files, timing: &timing, tree: &tree }, graph }).map_err(|e| anyhow::anyhow!(e))?;
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
        // The legs a cancel in the log cancelled. The scheduler, rebuilt below, hears it again
        // for those not ended yet: a leg that was running ends cancelled however it ends (lost
        // with its runner across the restart, say), not run again.
        let mut cancelled: Vec<(String, String)> = Vec::new();
        for (_, kind, leg, report) in log {
            match kind.as_str() {
                "cancel" => {
                    for act in plan.on_cancel(&Source { files: &files, timing: &timing, tree: &tree }) {
                        if let PlanAction::Cancel { leg, reason, .. } = act {
                            cancelled.push((leg, reason));
                        }
                    }
                }
                // Legs cancelled one by one (W26-R21), the run going on.
                "cancel_leg" => {
                    let why = report.as_deref().and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok()).and_then(|v| v.get("reason").and_then(serde_json::Value::as_str).map(str::to_owned)).unwrap_or_else(|| "cancelled".to_owned());
                    for act in plan.on_cancel_legs(&leg.iter().cloned().collect::<Vec<_>>(), &why, &Source { files: &files, timing: &timing, tree: &tree }) {
                        if let PlanAction::Cancel { leg, reason, .. } = act {
                            cancelled.push((leg, reason));
                        }
                    }
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
        type Row = (String, String, String, Option<String>, u32, Option<String>, Option<String>, Option<String>, Fence, Option<String>, Option<String>);
        let rows: Vec<Row> = self.hub.store.call(move |c| {
            let mut st = c.prepare("SELECT id, key, status, agent, attempt, leg, conclusion, outputs, grant_epoch, grant_n, input_hash, wait_reason FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND run_attempt = (SELECT attempt FROM runs WHERE id = ?1)")?;
            // Its grant's fence (W05-R34), none from an older build's claim.
            let fence = |e: Option<i64>, n: Option<i64>| match (e.and_then(|e| u64::try_from(e).ok()), n.and_then(|n| u64::try_from(n).ok())) {
                (Some(epoch), Some(n)) => Fence { epoch, n },
                _ => Fence::default(),
            };
            Ok(st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, fence(r.get(8)?, r.get(9)?), r.get(10)?, r.get(11)?)))?.collect::<Result<Vec<_>, _>>()?)
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
        let mut previous_waits: HashMap<String, serde_json::Value> = rows.iter().filter_map(|r| r.10.as_deref().and_then(|v| serde_json::from_str(v).ok()).map(|v| (r.0.clone(), v))).collect();
        let legs: Vec<(String, String, String, Option<String>, u32, Option<String>, Fence, Option<String>)> = rows.into_iter().map(|r| (r.0, r.1, r.2, r.3, r.4, r.5, r.8, r.9)).collect();
        // Those a person paused (W26-R22): they come back paused, and their tokens live the while
        // they were paused longer.
        let run_owned = run.to_owned();
        let pauses: Vec<(String, i64, Option<i64>)> = self
            .hub
            .store
            .call(move |c| {
                let mut st = c.prepare("SELECT id, paused_ms, paused_at FROM jobs WHERE run_id = ?1 AND job_id IS NOT NULL AND (paused_at IS NOT NULL OR paused_ms > 0)")?;
                Ok(st.query_map([&run_owned], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        for (id, ms, since) in &pauses {
            self.hub.restore_paused(id, *ms, *since);
        }
        let paused: HashSet<String> = pauses.into_iter().filter(|p| p.2.is_some()).map(|p| p.0).collect();
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
        // A culprit-finding try (W28-R57) joins no run-level group and supersedes nothing.
        let culprit = Try::of_input(&v);
        let (held, awaiting) = if unapproved {
            let group = plan.concurrency.as_ref().filter(|_| culprit.is_none()).map(|g| (format!("{}\0{}", repo.id, g.group), g.group.clone(), GroupConfig { cancel_in_progress: g.cancel_in_progress, queue_max: g.queue_max, limit: g.limit.map_or(1, |l| l.max(1) as usize), max_pending: 100 }));
            (Some(initial), Some(Awaiting { group }))
        } else {
            (None, None)
        };
        let sup = if culprit.is_some() { None } else { sup_of(&wf, &repo, &path, &event.name, &input.github) };
        // Where a person's retry asked its legs to run (W05-R28), those queued after a restart too.
        let placed = v.get("placed").and_then(|p| serde_json::from_value(p.clone()).ok()).unwrap_or_default();
        let mut st = RunSt { plan, repo: repo.clone(), event, path, created: row.created_at, in_progress: !unapproved, files: Arc::clone(&files), wf: Arc::clone(&wf), github: input.github.clone(), vars: input.vars.clone(), trusted: input.trusted, actions, dependencies, keys: HashMap::new(), held, seq, keep, debug, keep_failed, given: Given::default(), timing: Arc::clone(&timing), tree: Arc::clone(&tree), removed: 0, costs, awaiting, no_reuse: v.get("no_reuse").and_then(serde_json::Value::as_bool).unwrap_or(false), who: Who::default(), sup, cold: v.get("cold").and_then(serde_json::Value::as_bool).unwrap_or(false), culprit, placed };
        st.who = who_of(&self.hub, &st).await;
        let mut queued = Vec::new();
        let mut owned = Vec::with_capacity(legs.len());
        for (id, key, status, agent, attempt, leg, fence, input_hash) in legs {
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
            // W05-AC09: publishing one id copies the ownership set. Keep running grants
            // visible before taking them back, and publish the queued ids together below.
            if s == Status::Running && agent.is_some() {
                self.hub.ci.own_legs([id.as_str()]);
            }
            owned.push(id.clone());
            self.legs.insert(
                id.clone(),
                LegSt {
                    run: run.to_owned(),
                    key,
                    spec: spec.clone(),
                    attempt: attempt.max(1),
                    agent: agent.clone(),
                    report: LegReport::default(),
                    class: None,
                    deployment: None,
                    env_url: None,
                    seq: 0,
                    gone: false,
                    fence,
                },
            );
            let mut li = leg_in(&st, run, &id, &spec, &self.hub.cfg.ci, &self.warm);
            crate::offers::ci_leg(&self.hub, &li);
            li.paused = paused.contains(&id);
            // W05-AC09, W28-R46: restore its queued input hash with its row, avoiding one
            // database round trip per leg while keeping the same adoption key.
            li.work = work_of(&st, &spec, input_hash);
            li.finish_within = st.sup.as_ref().map(|s| s.finish_within);
            // Queued again at the attempt the store has (W05-R25): one that ended isn't granted again.
            li.attempt = attempt.max(1);
            match (s, agent) {
                // Running on `a` as the store has it, under the grant it was given (its attempt and
                // the epoch it was made in): `a` says whether it still is. A runner that said hello
                // while this rebuilt said what it runs (what it left out it lost: `spawn`'s
                // `runner_up`); one yet to renews it when it's back, and its other grants are
                // stopped; one that isn't back by the lease's end loses it then (W05-R25). Not put
                // on `a`'s record here: a leg its hello left out would hold one of its slots for
                // good.
                // Its grant gets a lease from now (W05-R34): the timer for its expiry is kept.
                (Status::Running, Some(a)) => {
                    self.hub.ci.grant(&id, Some((a.clone(), attempt.max(1), fence)));
                    for c in self.sched.handle(Input::LegRunning { leg: Box::new(li), runner: a, attempt: attempt.max(1), fence }, now_ms()) {
                        if let Command::Timer { at } = c {
                            self.timers.insert(at);
                        }
                    }
                }
                _ => queued.push(li),
            }
        }
        self.hub.ci.own_legs(owned.iter().map(String::as_str));
        self.hub.ci.own_run(run);
        let waits = st.awaiting.is_some();
        self.runs.insert(run.to_owned(), st);
        if waits {
            // Still waiting for a maintainer: nothing goes to the scheduler until then.
            return Ok(());
        }
        // Its demand again, so withdrawals are worked out afresh in creation order (W28-R46). It has
        // started already.
        let cmds: Vec<Command> = self.run_queued(run, None, Arrival::Restore).into_iter().filter(|c| !matches!(c, Command::StartRun { .. })).collect();
        Box::pin(self.commands(cmds)).await;
        let mut waits = Vec::new();
        for li in queued {
            // Held for its environment before the restart: held again, its wait counted from then.
            if let Some(env) = self.legs.get(&li.id).and_then(|l| l.spec.environment.as_ref()).map(|e| e.name.clone()).filter(|n| !n.is_empty()) {
                self.restore_waits(&mut waits).await;
                let id = li.id.clone();
                let ready_at = runs::queued_at(&self.hub, &id).await.unwrap_or_else(|_| crate::util::now_unix());
                // Announced before the server stopped, or not: once more at worst.
                self.held.insert(id.clone(), Held { leg: li, env, ready_at, announced: true });
                if let Err(e) = self.gate(&id).await {
                    tracing::warn!(leg = %id, error = %format!("{e:#}"), "an environment's protection could not be checked");
                }
                continue;
            }
            let id = li.id.clone();
            let previous = previous_waits.remove(&id);
            let cmds = self.sched.handle(Input::LegQueued(Box::new(li)), now_ms());
            // W05-AC09, W05-R13: only independent waits and timers commute here. Keep
            // identical waits as they are; batch changed ones. All other actions first flush
            // those waits, and capability waits retain the locked-job message path.
            if cmds.iter().filter(|c| matches!(c, Command::Wait { .. })).count() == 1
                && cmds.iter().all(|c| matches!(c, Command::Timer { .. }) || matches!(c, Command::Wait { leg, reason } if leg == &id && !matches!(reason, kiln_ci::sched::WaitReason::Capability { .. })))
            {
                for c in cmds {
                    crate::offers::ci(&self.hub, &c);
                    match c {
                        Command::Wait { leg, reason } => {
                            let value = serde_json::to_value(reason).unwrap_or_default();
                            if previous.as_ref() != Some(&value) {
                                waits.push((leg, value));
                            }
                        }
                        Command::Timer { at } => { self.timers.insert(at); }
                        other => {
                            self.restore_waits(&mut waits).await;
                            Box::pin(self.commands(vec![other])).await;
                        }
                    }
                }
            } else {
                self.restore_waits(&mut waits).await;
                Box::pin(self.commands(cmds)).await;
            }
        }
        self.restore_waits(&mut waits).await;
        for (key, reason) in cancelled {
            let Some(id) = self.runs.get(run).and_then(|r| r.keys.get(&key).cloned()).filter(|id| self.legs.contains_key(id)) else { continue };
            if self.held.remove(&id).is_some() {
                Box::pin(self.settled(&id, Outcome::Cancelled, Some(reason))).await?;
                continue;
            }
            let cmds = self.sched.handle(Input::LegCancelled { leg: id, reason }, now_ms());
            Box::pin(self.commands(cmds)).await;
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
    spec.defined_in = v.get("defined_in").and_then(|d| serde_json::from_value(d.clone()).ok()).unwrap_or_default();
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
        unaffected: None,
        defined_in: plan::DefinedIn::default(),
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
            let tx = crate::store::tx(c)?;
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
    // A cold run's legs restore nothing (W24-R23); what they save is theirs, and a cache entry,
    // as a ledger entry, is never overwritten.
    let mode = if r.cold { mode.min(Mode::WriteOnly) } else { mode };
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

/// The slots a runner offers legs: none while its kiln is to be upgraded (`crate::skew`), as an
/// outpost offers none.
fn work_slots(a: &crate::hub::AgentLive) -> u32 {
    if a.skew.takes_work() { a.slots } else { 0 }
}

/// `[ci.scheduler] target_use` as placement takes it (W05): a person's share within (0, 1]; else,
/// unset or past 1 (a runner would be filled past all it has), derived for each runner
/// (`kiln_ci::sched::state::TargetUse`).
pub(crate) fn target_use(s: &crate::config::ci::Scheduler) -> kiln_ci::sched::state::TargetUse {
    s.target_use.filter(|t| *t > 0.0 && *t <= 1.0).into()
}

/// What the engine weighs of a runner's sample: its CPUs, the share of them busy, its load and
/// memory, and the pressure its facts last said (docs/reach.md §2): Linux's memory and I/O stalls
/// (PSI's `full`) as they are, and memory macOS calls critical counted all in use (even
/// compressed, its working set no longer fits, and it swaps).
pub(crate) fn sample_of(s: &crate::metrics::HostSample, facts: Option<&kiln_ci::facts::RunnerFacts>) -> kiln_ci::sched::state::Sample {
    let work = facts.and_then(|f| f.work.as_ref());
    let depleted = work.and_then(|w| w.memory_level.as_deref()) == Some("critical");
    kiln_ci::sched::state::Sample {
        cpus: s.cpus,
        busy: f64::from(s.cpu_pct.clamp(0.0, 100.0)) / 100.0,
        load1: f64::from(s.load1),
        mem_total: s.mem_total,
        mem_used: if depleted { s.mem_total } else { s.mem_used },
        io_full: work.and_then(|w| w.io_full),
        mem_full: work.and_then(|w| w.mem_full),
    }
}

#[cfg(test)]
mod tests {
    /// W05-R23 (HEFT's upward rank): legs ready together go to the scheduler critical path first.
    /// b needs a and c needs b: a's rank is its 60 s and the chain's 150 s, 210 s; d, 300 s alone,
    /// goes first; e, with no history and no one needing it, last. An action that isn't a leg
    /// queued keeps its place, each stretch of legs ordered on its own, and legs of one rank keep
    /// the plan's order. A called workflow's leg ranks as its caller's job.
    #[test]
    fn w05_r23_legs_ready_together_go_critical_path_first() -> anyhow::Result<()> {
        use super::super::insights::JobCost;
        use kiln_ci::plan::PlanAction;
        let src = "on: push\njobs:\n  a:\n    runs-on: linux\n    steps:\n      - run: x\n  b:\n    needs: a\n    runs-on: linux\n    steps:\n      - run: x\n  c:\n    needs: [b]\n    runs-on: linux\n    steps:\n      - run: x\n  d:\n    runs-on: linux\n    steps:\n      - run: x\n  e:\n    runs-on: linux\n    steps:\n      - run: x\n";
        let (wf, diags) = kiln_ci::wf::parse_workflow(".kiln/ci.yaml", src, kiln_ci::expr::Dialect::Kiln);
        let wf = std::sync::Arc::new(wf.ok_or_else(|| anyhow::anyhow!("{diags:?}"))?);
        let costs: std::collections::HashMap<String, JobCost> = [("a", 60.0), ("b", 120.0), ("c", 30.0), ("d", 300.0)].into_iter().map(|(j, s)| (j.to_owned(), JobCost { p50: s, p90: s })).collect();
        let ranks = super::upward_ranks(&wf, &costs);
        let want: std::collections::HashMap<String, u64> = [("a", 210_000), ("b", 150_000), ("c", 30_000), ("d", 300_000), ("e", 0)].into_iter().map(|(j, r)| (j.to_owned(), r)).collect();
        anyhow::ensure!(ranks == want, "{ranks:?}");
        let q = |job: &str| PlanAction::Queue(Box::new(super::placeholder_spec(&wf, job)));
        let order = |acts: Vec<PlanAction>| -> Vec<String> { acts.iter().map(|a| if let PlanAction::Queue(s) = a { s.job.clone() } else { "·".to_owned() }).collect() };
        anyhow::ensure!(order(super::critical_first(vec![q("e"), q("a"), q("d")], &ranks)) == ["d", "a", "e"]);
        let warn = PlanAction::Warn { leg: "x".into(), message: "m".into() };
        anyhow::ensure!(order(super::critical_first(vec![q("c"), q("b"), warn, q("e"), q("a/inner"), q("d")], &ranks)) == ["b", "c", "·", "d", "a/inner", "e"]);
        anyhow::ensure!(order(super::critical_first(vec![q("e"), q("e/x")], &ranks)) == ["e", "e/x"], "one rank: the plan's order");
        Ok(())
    }

    #[test]
    fn w29_r24_wide_enforcement_capabilities_reach_the_schedulers_runner_view() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let hello = crate::proto::Hello { protocol: crate::proto::VERSION, name: "lock-runner".into(), labels: vec![], slots: 1, hostname: "lock-runner".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: true, dedicated_job: None, running: vec![], terminal: false };
            hub.instances.update(|i| i.join(1, "lock-runner", hello.clone(), 0));
            let live = crate::hub::tests::live_runner("lock-runner", "linux", "x86_64");
            assert_eq!(super::runner_view(&hub, &live).caps & crate::proto::CAP_DEPENDENCY_LOCKS, 0);
            hub.instances.update(|i| i.set_wide(1, crate::proto::CAP_DEPENDENCY_LOCKS | u64::from(crate::proto::CAP_CI2)));
            let updated = super::runner_view(&hub, &live);
            assert_ne!(updated.caps & crate::proto::CAP_DEPENDENCY_LOCKS, 0);
            assert_ne!(updated.caps & u64::from(crate::proto::CAP_CI2), 0);
            Ok(())
        })
    }

    #[test]
    fn w29_r24_old_runners_cannot_take_locked_legs_and_pods_use_the_same_pins() -> anyhow::Result<()> {
        use kiln_ci::sched::routing::{RunnerView, checks, resolve};
        let (wf, diagnostics) = kiln_ci::wf::parse_workflow(".kiln/ci.yml", "on: push\njobs:\n  check:\n    runs-on: baremetal\n    steps: [{run: echo accepted}]\n", kiln_ci::expr::Dialect::Kiln);
        assert!(!diagnostics.iter().any(|d| d.is_error()));
        let workflow = std::sync::Arc::new(wf.ok_or_else(|| anyhow::anyhow!("no workflow"))?);
        let (_, actions) = kiln_ci::plan::RunPlan::start(workflow, kiln_ci::plan::RunInput::default(), kiln_ci::plan::Settings::default(), &kiln_ci::plan::NoSource).map_err(anyhow::Error::msg)?;
        let spec = actions.iter().find_map(|a| if let kiln_ci::plan::PlanAction::Queue(spec) = a { Some(spec) } else { None }).ok_or_else(|| anyhow::anyhow!("no queued leg"))?;
        let config = crate::config::ci::Ci::default();
        let route = super::route_with(spec, "ci", "acme/app", "push", true, &Default::default(), Some(&Default::default()), &config);
        assert_ne!(route.caps & crate::proto::CAP_DEPENDENCY_LOCKS, 0);
        let requirement = resolve(&route, &[]).map_err(anyhow::Error::msg)?;
        let old = RunnerView { caps: route.caps & !crate::proto::CAP_DEPENDENCY_LOCKS, ..Default::default() };
        assert!(checks(&requirement, &old).iter().any(|c| !c.pass && c.requirement.starts_with("capabilities")));
        let new = RunnerView { caps: route.caps, ..old };
        assert!(checks(&requirement, &new).iter().filter(|c| c.requirement.starts_with("capabilities")).all(|c| c.pass));
        let pin = format!("alpine@sha256:{}", "a".repeat(64));
        let locks = kiln_ci::lock::execution::Locks { images: [("alpine:3".into(), pin.clone())].into(), ..Default::default() };
        let mut pod = crate::ci::pools::LegPod { services: vec![crate::ci::pools::PodService { id: "db".into(), image: "alpine:3".into(), ..Default::default() }], ..Default::default() };
        super::lock_pod(&mut pod, Some("alpine:3"), Some(&locks)).map_err(anyhow::Error::msg)?;
        assert_eq!(pod.image.as_ref(), Some(&pin));
        assert_eq!(pod.services.first().map(|s| &s.image), Some(&pin));
        assert!(super::lock_pod(&mut pod, Some("unknown:latest"), Some(&locks)).is_ok(), "the explicit accepted image precedes the pool default");
        pod.image = None;
        assert!(super::lock_pod(&mut pod, Some("unknown:latest"), Some(&locks)).is_err());
        Ok(())
    }
    /// W28-R57: a culprit-finding try is no push of its branch. A gate's goes at once, at gate
    /// priority, nothing holding it; the rest at low priority, below work in progress, speculative
    /// under its lane's window and coalescing with no push. Its run keeps that through a restart,
    /// and no request can make its run one.
    #[test]
    fn w28_r57_a_try_s_priority_and_admission() -> anyhow::Result<()> {
        use kiln_ci::sched::admit::{Cadence, Coalesce, Spec};
        use kiln_ci::sched::intent::Intent;

        use super::Try;
        let (low, gate) = (Try { gate: false }, Try { gate: true });
        assert!(low.priority() < Intent::Wip.priority() && low.priority() == Try::LOW, "{}", low.priority());
        assert_eq!(gate.priority(), Intent::Gate.priority());
        assert_eq!((low.intent().0, gate.intent().0), (Intent::Wip, Intent::Gate));
        let declared = Spec { coalesce: Some(Coalesce { quiet: 60_000, max_wait: 600_000, always: false }), cadence: Some(Cadence::Free) };
        assert_eq!(Try::admission(None, declared), (declared.coalesce, declared.cadence, false), "a push's legs as its workflow says");
        assert_eq!(Try::admission(Some(low), declared), (None, None, true), "speculative under the window, coalescing with nothing");
        assert_eq!(Try::admission(Some(gate), declared), (None, None, false), "a gate's: never held");
        // Kept with the run's input, and read back after a restart.
        assert_eq!(Try::of_input(&serde_json::json!({ "culprit": gate })), Some(gate));
        assert_eq!(Try::of_input(&serde_json::json!({ "culprit": null })), None);
        assert_eq!(Try::of_input(&serde_json::json!({})), None);
        // A request's `culprit` is no field of its: only kiln's bisection marks a try.
        let ask: crate::ci::uploads::Ask = serde_json::from_value(serde_json::json!({ "sha": "a", "culprit": { "gate": true }, "vars": { "KILN_TRY_PRIORITY": "90" } }))?;
        assert_eq!(ask.culprit, None);
        Ok(())
    }

    /// W05-R38, docs/needs.md E4.1: a leg gives the engine its job's median and p90 from
    /// history, and its timeout, in milliseconds; a job with no history, its timeout alone.
    #[test]
    fn w05_r38_a_leg_carries_its_p90_and_its_timeout() {
        use super::super::insights::JobCost;
        let costs: std::collections::HashMap<String, JobCost> = [("build".to_owned(), JobCost { p50: 360.0, p90: 540.0 })].into_iter().collect();
        assert_eq!(super::durations(&costs, "build", 45), (Some(360_000), Some(540_000), Some(2_700_000)));
        assert_eq!(super::durations(&costs, "lint", 360), (None, None, Some(21_600_000)));
    }

    /// W05-R30, docs/needs.md E2.3: in W05-R09's event mode a leg's class is its run's event's, in
    /// W28-R64's intent mode its intent's.
    #[test]
    fn w05_r30_each_event_and_intent_has_its_class() {
        use kiln_ci::sched::Class;
        use kiln_ci::sched::intent::Intent;
        let events = [
            ("workflow_dispatch", Class::Interactive),
            ("manual", Class::Interactive),
            ("land", Class::Land),
            ("merge_group", Class::Land),
            ("push", Class::Batch),
            ("pull_request", Class::Batch),
            ("pull_request_target", Class::Batch),
            ("diff", Class::Batch),
            ("agent", Class::Batch),
            ("workflow_run", Class::Batch),
            ("pipeline", Class::Batch),
            ("schedule", Class::Background),
        ];
        for (event, want) in events {
            assert_eq!(super::class(event), want, "{event}");
        }
        let intents = [(Intent::Gate, Class::Land), (Intent::Waiting, Class::Interactive), (Intent::Trunk, Class::Batch), (Intent::Wip, Class::Batch)];
        for (intent, want) in intents {
            assert_eq!(intent.class(), want, "{}", intent.name());
        }
    }

    /// W05-R31: `[sched.classes.<class>]` sets a class's floor (percent) and bound over kiln's
    /// defaults; a class it leaves out, and what it leaves out of one, stay as they are; a class
    /// kiln doesn't have, or a floor or bound that doesn't read, is an error.
    #[test]
    fn w05_r31_classes_floors_and_bounds_are_read() -> anyhow::Result<()> {
        use kiln_ci::sched::{Class, ClassConfig};
        let cfg: crate::config::Config = toml::from_str("[sched.classes.batch]\nfloor = 20\nbound = \"45m\"\n[sched.classes.background]\nbound = \"never\"\n[sched.classes.interactive]\nfloor = 2.5\n")?;
        let classes = cfg.sched.classes()?;
        assert_eq!(classes.get(&Class::Batch), Some(&ClassConfig { floor: 200, bound: Some(45 * 60_000) }));
        assert_eq!(classes.get(&Class::Background), Some(&ClassConfig { floor: 50, bound: None }));
        assert_eq!(classes.get(&Class::Interactive), Some(&ClassConfig { floor: 25, bound: None }));
        assert_eq!(classes.get(&Class::Land), None);
        assert_eq!(crate::config::Config::default().sched.classes()?, kiln_ci::sched::state::default_classes());
        assert!(toml::from_str::<crate::config::Config>("[sched.classes.urgent]\nfloor = 10\n").is_err(), "no class `urgent`");
        for bad in ["[sched.classes.batch]\nfloor = 150\n", "[sched.classes.batch]\nbound = \"soon\"\n"] {
            let cfg: crate::config::Config = toml::from_str(bad)?;
            assert!(cfg.sched.classes().is_err(), "{bad}");
        }
        Ok(())
    }

    /// W05-R36: a lease runner's write is taken under its leg's live grant alone: not under its
    /// leg's earlier attempt's (the retry on the same runner, W05-R16's fallback), not under
    /// another grant of the same attempt (granted again after a refusal), not from another runner,
    /// and not once the grant is over. A refusal under a grant is said once.
    #[test]
    fn w05_r36_a_write_is_taken_under_its_legs_live_grant_alone() {
        use crate::proto::Grant;
        use kiln_ci::sched::Fence;
        let h = super::Handle::default();
        let grant = |attempt, n| Grant { leg: "L".into(), attempt, epoch: 0, n };
        h.grant("L", Some(("a".into(), 1, Fence { epoch: 0, n: 4 })));
        assert!(h.live("a", &grant(1, 4)) && h.fenced("L"));
        h.grant("L", Some(("a".into(), 2, Fence { epoch: 0, n: 5 })));
        assert!(!h.live("a", &grant(1, 4)), "attempt 1's grant, attempt 2 now on the same runner");
        assert!(!h.live("a", &grant(2, 4)), "attempt 2 under attempt 1's fence");
        assert!(!h.live("b", &grant(2, 5)), "another runner");
        assert!(h.live("a", &grant(2, 5)));
        h.grant("L", Some(("a".into(), 2, Fence { epoch: 0, n: 6 })));
        assert!(!h.live("a", &grant(2, 5)), "the same attempt, granted again");
        h.grant("L", None);
        assert!(!h.live("a", &grant(2, 6)) && !h.fenced("L"), "its grant over");
        h.grant("M", Some(("a".into(), 1, Fence::default())));
        assert!(!h.fenced("M"), "a grant an older build made has no fence");
        assert!(h.first_refusal(&grant(1, 4)) && !h.first_refusal(&grant(1, 4)) && h.first_refusal(&grant(2, 5)), "said once a grant");
    }

    /// A dispatcher on `hub`, with no runs yet: what a test drives a grant's ends through.
    fn dispatcher(hub: &std::sync::Arc<crate::hub::Hub>) -> super::Dispatch {
        super::Dispatch {
            hub: std::sync::Arc::clone(hub),
            sched: kiln_ci::sched::Scheduler::new(kiln_ci::sched::Config::default()),
            routing: crate::ci::routing::Sources::default(),
            held: std::collections::HashMap::new(),
            runs: std::collections::HashMap::new(),
            legs: std::collections::HashMap::new(),
            timers: std::collections::BTreeSet::new(),
            next: Vec::new(),
            in_commands: false,
            pods: std::collections::HashSet::new(),
            decisions: std::collections::HashMap::new(),
            warm: std::collections::HashMap::new(),
            warm_seq: 0,
        }
    }

    /// `leg` running attempt 1 on runner `a` under grant `n`, granted as `Assign` grants it, and
    /// its job token (GITHUB_TOKEN) minted for that grant as `assign` mints it: the token.
    async fn running(d: &mut super::Dispatch, leg: &str, n: u64) -> anyhow::Result<String> {
        let fence = kiln_ci::sched::Fence { epoch: 0, n };
        let spec = super::placeholder_spec(&std::sync::Arc::new(kiln_ci::wf::Workflow::default()), "build");
        let l = super::LegSt { run: "R".into(), key: "build".into(), spec, attempt: 1, agent: Some("a".into()), report: kiln_ci::plan::LegReport::default(), class: None, deployment: None, env_url: None, seq: 0, gone: false, fence };
        d.legs.insert(leg.to_owned(), l);
        d.regrant(leg, Some(("a".into(), 1, fence))).await;
        crate::ci::token::mint_job(&d.hub, leg, "R", "repo", &kiln_ci::plan::perms::Resolved::default(), 3600, Some((1, fence))).await
    }

    /// Whether `token` is revoked in the store, and refused.
    async fn revoked(hub: &crate::hub::Hub, token: &str) -> anyhow::Result<bool> {
        let hash = crate::util::sha256_hex(token.as_bytes());
        let at: Option<i64> = hub.store.call(move |c| Ok(c.query_row("SELECT revoked_at FROM tokens WHERE hash = ?1", [hash], |r| r.get(0))?)).await?;
        Ok(at.is_some() && crate::ci::token::job_of(hub, token).await.is_none())
    }

    /// W10-R03: a job token (GITHUB_TOKEN) is its attempt's grant's, revoked in the store at every
    /// end of that grant: its result (finished, or cancelled), a refusal, a loss (the scheduler's
    /// `Lost`, for a lease run out or a runner lost; or reconciled, a runner that no longer has
    /// it), the leg settled or gone with its repository, and the leg's next grant superseding it.
    /// Taken until then, and the next grant's is taken.
    #[test]
    fn w10_r03_every_end_revokes_the_job_token() -> anyhow::Result<()> {
        use crate::proto::{Grant, JobResult};
        use crate::store::Status;
        use kiln_ci::sched::{Command, Fence, LeaseLoss, Loss, Outcome};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut d = dispatcher(&hub);
            let grant = |leg: &str, n| Grant { leg: leg.into(), attempt: 1, epoch: 0, n };
            let result = |leg: &str, status| JobResult { job: leg.into(), status, exit_code: None, error: None, peak_rss: None, cpu_seconds: None };
            let mut ends = Vec::new();

            let t = running(&mut d, "finished", 1).await?;
            assert!(crate::ci::token::job_of(&hub, &t).await.is_some(), "taken while its grant is live");
            d.finished("a", result("finished", Status::Succeeded), Some(grant("finished", 1))).await?;
            ends.push(("finished", t));

            let t = running(&mut d, "cancelled", 2).await?;
            d.finished("a", result("cancelled", Status::Cancelled), Some(grant("cancelled", 2))).await?;
            ends.push(("cancelled", t));

            let t = running(&mut d, "refused", 3).await?;
            d.handle(super::Msg::Rejected { agent: "a".into(), leg: "refused".into(), fence: Some(Fence { epoch: 0, n: 3 }), draining: false }).await;
            ends.push(("refused", t));

            let t = running(&mut d, "expired", 4).await?;
            d.commands(vec![Command::Lost { leg: "expired".into(), attempt: 1, runner: "a".into(), fence: Fence { epoch: 0, n: 4 }, why: Loss::Expired }]).await;
            ends.push(("lost, its lease run out", t));

            let t = running(&mut d, "runner-lost", 5).await?;
            d.commands(vec![Command::Lost { leg: "runner-lost".into(), attempt: 1, runner: "a".into(), fence: Fence { epoch: 0, n: 5 }, why: Loss::RunnerLost }]).await;
            ends.push(("lost with its runner", t));

            let t = running(&mut d, "reconciled", 6).await?;
            let cmds = d.lose(vec![("reconciled".into(), 1)], "the runner lost it").await;
            d.commands(cmds).await;
            ends.push(("reconciled", t));

            let t = running(&mut d, "settled", 7).await?;
            d.settled("settled", Outcome::Cancelled, None).await?;
            ends.push(("settled", t));

            let t = running(&mut d, "gone", 8).await?;
            d.forget_gone("gone", 1).await;
            ends.push(("gone with its repository", t));

            let t = running(&mut d, "superseded", 9).await?;
            let next = Fence { epoch: 0, n: 10 };
            d.commands(vec![Command::Assign { leg: "superseded".into(), runner: "b".into(), attempt: 2, executor: None, image: None, docker: None, slot: None, fence: next, expires: None, lease_loss: LeaseLoss::default() }]).await;
            ends.push(("superseded", t));
            let second = crate::ci::token::mint_job(&hub, "superseded", "R", "repo", &kiln_ci::plan::perms::Resolved::default(), 3600, Some((2, next))).await?;
            assert!(crate::ci::token::job_of(&hub, &second).await.is_some(), "the next grant's is taken");

            for (end, t) in &ends {
                assert!(revoked(&hub, t).await?, "{end}: its job token is revoked");
            }
            assert_eq!(ends.len(), 9);
            Ok(())
        })
    }

    /// W05-R25: the store's cluster group opened three times, as three starts of a server. Each
    /// open campaigns, so the group's term rises; each start's epoch is the next (the term, or one
    /// past the last taken), so the three rise; and the Assign an engine of each epoch makes
    /// carries it.
    #[test]
    fn w05_r25_a_restart_raises_the_epoch() -> anyhow::Result<()> {
        use kiln_ci::sched::{Command, Config, Input, LegIn, RunnerIn, Scheduler};
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let mut seen: Vec<(u64, u64)> = Vec::new();
            for _ in 0..3 {
                let store = crate::store::Store::open_grouped(&dir.path().join("kiln.db"), &dir.path().join("cluster"))?;
                let term = store.group().map_or(0, crate::store::group::Group::term);
                let epoch = super::runs::next_epoch(&store, term).await?;
                let mut s = Scheduler::with_epoch(Config::default(), epoch);
                let view = kiln_ci::sched::routing::RunnerView { name: "a".into(), labels: vec!["kiln".into()], ..kiln_ci::sched::routing::RunnerView::default() };
                s.handle(Input::RunnerUp(Box::new(RunnerIn { id: "a".into(), view, slots: 1, ..RunnerIn::default() })), 0);
                let route = kiln_ci::sched::routing::LegRoute { labels: vec!["kiln".into()], ..kiln_ci::sched::routing::LegRoute::default() };
                let c = s.handle(Input::LegQueued(Box::new(LegIn { id: "x".into(), run: "R".into(), repo: "acme/x".into(), route, ..LegIn::default() })), 0);
                let carried = c.iter().find_map(|x| match x {
                    Command::Assign { fence, .. } => Some(fence.epoch),
                    _ => None,
                });
                assert_eq!(carried, Some(epoch), "the Assign carries its epoch: {c:?}");
                seen.push((term, epoch));
                drop(store);
            }
            let rising = |v: Vec<u64>| v.windows(2).all(|w| matches!(w, [a, b] if a < b));
            assert!(rising(seen.iter().map(|(t, _)| *t).collect()) && seen.iter().all(|(t, _)| *t > 0), "each open campaigns, its term rising: {seen:?}");
            assert!(rising(seen.iter().map(|(_, e)| *e).collect()) && seen.iter().all(|(t, e)| e >= t), "each start's epoch rising, at least its term: {seen:?}");
            Ok(())
        })
    }

    /// W05-R25, the example of docs/needs.md E3.7: the server restarted, its epoch 7 to 8, while
    /// runner A ran L's attempt 2 (in the store, granted under epoch 7) and M's attempt 1 (which the
    /// store has settled). The rebuild brought L back as that grant, its lease begun then. A
    /// reconnects: its hello and its Renew name both. L is adopted at epoch 7 and renewed: it isn't
    /// lost at the lease it was rebuilt with, and what A writes under it is taken. M isn't A's: A is
    /// told to stop it (once a grant), and its result is refused, changing nothing.
    #[test]
    fn w05_r25_hello_running_is_reconciled_after_a_restart() -> anyhow::Result<()> {
        use crate::proto::Grant;
        use kiln_ci::sched::{Command, Config, Fence, Input, LegIn, Scheduler};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut d = dispatcher(&hub);
            d.sched = Scheduler::with_epoch(Config { lease: Some(60_000), ..Config::default() }, 8);
            hub.put_agent(runner("a", 2));
            // The rebuild, 30 s ago: L running on A under its grant of epoch 7, attempt 2.
            let (l, m) = (Fence { epoch: 7, n: 4 }, Grant { leg: "m".into(), attempt: 1, epoch: 7, n: 3 });
            track(&mut d, "l", "a");
            if let Some(x) = d.legs.get_mut("l") {
                (x.attempt, x.fence) = (2, l);
            }
            hub.ci.grant("l", Some(("a".into(), 2, l)));
            let route = kiln_ci::sched::routing::LegRoute { labels: vec!["kiln".into()], ..kiln_ci::sched::routing::LegRoute::default() };
            let li = LegIn { id: "l".into(), run: "R".into(), repo: "acme/x".into(), route, ..LegIn::default() };
            let rebuilt = super::now_ms() - 30_000;
            d.sched.handle(Input::LegRunning { leg: Box::new(li), runner: "a".into(), attempt: 2, fence: l }, rebuilt);
            // A back: its hello, then its Renew.
            d.handle(super::Msg::RunnerUp { agent: "a".into(), running: vec!["l".into(), "m".into()] }).await;
            d.handle(super::Msg::Leased { agent: "a".into(), peer: 7, seq: 1, held: vec![Grant::of("l", 2, l), m.clone()] }).await;
            let c = d.sched.handle(Input::Tick, rebuilt + 61_000);
            assert!(!c.iter().any(|x| matches!(x, Command::Lost { leg, .. } if leg == "l")), "L renewed, not lost at the lease it was rebuilt with: {c:?}");
            assert_eq!(granted(&hub, "l"), Some(("a".to_owned(), 2, l)), "adopted at epoch 7, attempt 2");
            assert!(d.live("a", &Grant::of("l", 2, l)), "what A writes under it is taken");
            assert!(!hub.ci.first_refusal(&m), "A was told to stop M already");
            d.finished("a", crate::proto::JobResult { job: "m".into(), status: crate::store::Status::Succeeded, exit_code: None, error: None, peak_rss: None, cpu_seconds: None }, Some(m.clone())).await?;
            assert!(!d.legs.contains_key("m") && granted(&hub, "m").is_none(), "M's result refused, changing nothing");
            assert_eq!(d.sched.epoch(), 8, "grants from now are made in 8");
            Ok(())
        })
    }

    /// A runner of the hub's, `slots` of them, labelled as the legs [`queue`] makes ask.
    fn runner(name: &str, slots: u32) -> crate::hub::AgentLive {
        crate::hub::AgentLive { labels: vec!["kiln".into()], slots, ..crate::hub::tests::live_runner(name, "linux", "x86_64") }
    }

    /// `leg`, one of the dispatcher's (the hub knows it as such), as `assign` leaves it once sent
    /// to `agent`.
    fn track(d: &mut super::Dispatch, leg: &str, agent: &str) {
        let spec = super::placeholder_spec(&std::sync::Arc::new(kiln_ci::wf::Workflow::default()), "build");
        let l = super::LegSt { run: "R".into(), key: leg.into(), spec, attempt: 1, agent: Some(agent.into()), report: kiln_ci::plan::LegReport::default(), class: None, deployment: None, env_url: None, seq: 0, gone: false, fence: kiln_ci::sched::Fence::default() };
        d.legs.insert(leg.to_owned(), l);
        d.hub.ci.own_legs([leg]);
    }

    /// `leg` queued in the scheduler, and what it decided done.
    async fn queue(d: &mut super::Dispatch, leg: &str) {
        let route = kiln_ci::sched::routing::LegRoute { labels: vec!["kiln".into()], ..kiln_ci::sched::routing::LegRoute::default() };
        let l = kiln_ci::sched::LegIn { id: leg.into(), run: "R".into(), repo: "acme/x".into(), route, ..kiln_ci::sched::LegIn::default() };
        let cmds = d.sched.handle(kiln_ci::sched::Input::LegQueued(Box::new(l)), super::now_ms());
        d.commands(cmds).await;
    }

    /// `leg`'s live grant: its runner, attempt and fence.
    fn granted(hub: &crate::hub::Hub, leg: &str) -> Option<(String, u32, kiln_ci::sched::Fence)> {
        hub.ci.grants.load().get(leg).cloned()
    }

    /// What the hub told the dispatcher since it was last read.
    fn said(rx: &crate::memory::queue::Receiver<super::Msg>) -> Vec<super::Msg> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// W05-R35: a runner gone past its grace leaves the scheduler's fleet whatever it held. The
    /// hub tells the dispatcher it's lost, once, though it ran nothing; it gets no Assign after;
    /// back (its hello), it gets work again.
    #[test]
    fn w05_r35_a_runner_without_legs_lost_leaves_the_fleet() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let rx = hub.ci.listen().ok_or_else(|| anyhow::anyhow!("listened to already"))?;
            let mut d = dispatcher(&hub);
            hub.put_agent(runner("idle", 2));
            d.runner_up("idle", &[]).await;
            hub.change_agent("idle", |a| {
                a.connected = false;
                a.lost_at = std::time::Instant::now().checked_sub(hub.comeback.grace() + std::time::Duration::from_secs(1));
            });
            hub.check_lost_agents().await;
            hub.check_lost_agents().await;
            let told = said(&rx);
            assert!(matches!(told.as_slice(), [super::Msg::RunnerLost { agent }] if agent == "idle"), "told once: {told:?}");
            for m in told {
                d.handle(m).await;
            }
            queue(&mut d, "x").await;
            assert_eq!(granted(&hub, "x"), None, "no Assign to a runner lost");
            hub.change_agent("idle", |a| {
                a.connected = true;
                a.lost_at = None;
            });
            d.handle(super::Msg::RunnerUp { agent: "idle".into(), running: Vec::new() }).await;
            assert_eq!(granted(&hub, "x").map(|g| g.0), Some("idle".to_owned()), "back, it gets the leg");
            Ok(())
        })
    }

    /// W05-R35, the example of docs/needs.md E3.6: machine M runs processes P1 (legs A and B) and
    /// P2 (leg C), a blue/green hand-over. P1 goes and doesn't come back within its grace: A and B
    /// are lost (their grants over, their attempts ended as their runner's loss and tried again),
    /// while C runs on under its grant, and M stays in the fleet (no RunnerLost).
    #[test]
    fn w05_r35_a_machines_lost_process_loses_only_its_grants() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let rx = hub.ci.listen().ok_or_else(|| anyhow::anyhow!("listened to already"))?;
            let mut d = dispatcher(&hub);
            hub.put_agent(runner("m", 3));
            d.runner_up("m", &[]).await;
            for leg in ["a", "b", "c"] {
                track(&mut d, leg, "m");
                queue(&mut d, leg).await;
            }
            let c = granted(&hub, "c").ok_or_else(|| anyhow::anyhow!("c granted"))?;
            assert!(["a", "b"].iter().all(|l| granted(&hub, l).is_some_and(|g| g.0 == "m")) && c.0 == "m");
            hub.change_agent("m", |a| {
                a.running = vec!["a".into(), "b".into(), "c".into()];
            });
            let past = std::time::Instant::now().checked_sub(hub.comeback.grace() + std::time::Duration::from_secs(1)).ok_or_else(|| anyhow::anyhow!("a time past its grace"))?;
            hub.lost_instance(7, "m", past, vec!["a".into(), "b".into()]);
            hub.check_lost_agents().await;
            let told = said(&rx);
            assert!(matches!(told.as_slice(), [super::Msg::LegsLost { agent, legs }] if agent == "m" && *legs == ["a", "b"]), "the process's legs, not its machine: {told:?}");
            assert_eq!(hub.agents.load().get("m").map(|a| a.running.clone()), Some(vec!["c".to_owned()]), "their slots free");
            for m in told {
                d.handle(m).await;
            }
            for leg in ["a", "b"] {
                assert!(granted(&hub, leg).is_none_or(|g| g.1 == 2), "{leg}'s first grant lost");
                assert_eq!(d.legs.get(leg).map(|l| l.attempt), Some(2), "{leg} tried again");
            }
            assert_eq!(granted(&hub, "c"), Some(c.clone()), "C runs on under its grant");
            assert!(d.live("m", &crate::proto::Grant::of("c", 1, c.2)), "and what it writes is taken");
            Ok(())
        })
    }

    /// W05-R35, the example of docs/needs.md E3.6: an operator drains runner R, which holds two
    /// legs. R gets no third, while both run on under their grants, and finish; a probe's report
    /// doesn't take the drain back; after the undrain R gets work again. A runner that refuses work
    /// draining or upgrading drains on its own: the leg it refused isn't kept off it, it gets
    /// nothing new, and back (its hello) it takes the leg.
    #[test]
    fn w05_r35_draining_reaches_the_engine() -> anyhow::Result<()> {
        use kiln_ci::sched::{Input, Outcome};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let rx = hub.ci.listen().ok_or_else(|| anyhow::anyhow!("listened to already"))?;
            let mut d = dispatcher(&hub);
            hub.put_agent(runner("r", 3));
            d.runner_up("r", &[]).await;
            for leg in ["a", "b"] {
                track(&mut d, leg, "r");
                queue(&mut d, leg).await;
            }
            assert!(hub.set_draining("r", true).await);
            let told = said(&rx);
            assert!(matches!(told.as_slice(), [super::Msg::RunnerDraining { agent }] if agent == "r"), "{told:?}");
            for m in told {
                d.handle(m).await;
            }
            track(&mut d, "c", "r");
            queue(&mut d, "c").await;
            assert_eq!(granted(&hub, "c"), None, "drained, it gets no third");
            d.handle(super::Msg::RunnerFacts { agent: "r".into() }).await;
            assert_eq!(granted(&hub, "c"), None, "a probe's report doesn't undrain it");
            for leg in ["a", "b"] {
                let g = granted(&hub, leg).ok_or_else(|| anyhow::anyhow!("{leg} runs on"))?;
                assert!(g.0 == "r" && d.live("r", &crate::proto::Grant::of(leg, 1, g.2)), "{leg} runs on under its grant");
                let cmds = d.sched.handle(Input::AttemptFinished { leg: leg.into(), attempt: 1, outcome: Outcome::Success }, super::now_ms());
                d.commands(cmds).await;
            }
            assert!(!d.legs.contains_key("a") && !d.legs.contains_key("b"), "both finished, drained");
            assert!(hub.set_draining("r", false).await);
            let told = said(&rx);
            assert!(matches!(told.as_slice(), [super::Msg::RunnerFacts { agent }] if agent == "r"), "its undrain is the runner as it is: {told:?}");
            for m in told {
                d.handle(m).await;
            }
            assert_eq!(granted(&hub, "c").map(|g| g.0), Some("r".to_owned()), "undrained, it gets work again");
            // On its own: it refuses a leg, upgrading.
            track(&mut d, "e", "r");
            queue(&mut d, "e").await;
            let e = granted(&hub, "e").ok_or_else(|| anyhow::anyhow!("e granted"))?;
            d.handle(super::Msg::Rejected { agent: "r".into(), leg: "e".into(), fence: Some(e.2), draining: true }).await;
            assert_eq!(granted(&hub, "e"), None, "draining on its own, it gets nothing new");
            d.handle(super::Msg::RunnerUp { agent: "r".into(), running: vec!["c".into()] }).await;
            assert_eq!(granted(&hub, "e").map(|g| g.0), Some("r".to_owned()), "back, it takes the leg it refused");
            Ok(())
        })
    }

    /// W05-R34: `[sched] lease` and `lock_delay` are read as durations, kiln's 60 s and 15 s when
    /// unset; a lease under 3 s (renewed three times a lease, twice a second at most), or a
    /// duration that doesn't read, is refused.
    #[test]
    fn w05_r34_lease_and_lock_delay_are_read() -> anyhow::Result<()> {
        assert_eq!(crate::config::Config::default().sched.leases()?, (60_000, 15_000));
        let cfg: crate::config::Config = toml::from_str("[sched]\nlease = \"2m\"\nlock_delay = \"40s\"\n")?;
        assert_eq!(cfg.sched.leases()?, (120_000, 40_000));
        let short: crate::config::Config = toml::from_str("[sched]\nlease = \"5s\"\n")?;
        assert_eq!(short.sched.leases()?, (5_000, 15_000), "as short as a test's");
        for bad in ["[sched]\nlease = \"2s\"\n", "[sched]\nlease = \"soon\"\n", "[sched]\nlock_delay = \"later\"\n"] {
            let cfg: crate::config::Config = toml::from_str(bad)?;
            assert!(cfg.sched.leases().is_err(), "{bad}");
        }
        Ok(())
    }

    /// W05-R39: reservations for big items are off until `[sched] backfill` declares them
    /// (docs/ci.md §0.3).
    #[test]
    fn w05_r39_backfill_is_off_until_declared() -> anyhow::Result<()> {
        assert!(!crate::config::Config::default().sched.backfill);
        let cfg: crate::config::Config = toml::from_str("[sched]\nbackfill = true\n")?;
        assert!(cfg.sched.backfill);
        Ok(())
    }

    /// W05-R40, docs/needs.md E4.4: a gang's grants are written together, in one transaction,
    /// before any of its Assigns goes. Three runners, a gang of three; the store fails the write at
    /// the third member's row, after the first two were written in it: none of the three is in the
    /// store, none was sent to a runner, and all three are taken back, queued again at their
    /// attempts. Once the store writes again and they may go again, they're granted whole, their
    /// grants written together.
    #[test]
    fn w05_r40_a_gang_s_grants_are_written_together() -> anyhow::Result<()> {
        use kiln_ci::sched::state::Gang;
        use kiln_ci::sched::{Input, LegIn, WaitReason};
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut d = dispatcher(&hub);
            for r in ["a", "b", "c"] {
                hub.put_agent(runner(r, 1));
                d.runner_up(r, &[]).await;
            }
            hub.store
                .call(|c| {
                    Ok(c.execute_batch(
                        "INSERT INTO repos (id, name, clone_url, created_at) VALUES ('r', 'acme/x', '', 0);
                         INSERT INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at) VALUES ('R', 'r', 1, 'ci', 'push', 'main', 'abc', 'queued', 0);
                         INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec) VALUES ('m0', 'R', 'sim (a)', 'sim (a)', 'queued', '[]', '[]', '{}'), ('m1', 'R', 'sim (b)', 'sim (b)', 'queued', '[]', '[]', '{}'), ('m2', 'R', 'sim (c)', 'sim (c)', 'queued', '[]', '[]', '{}');
                         CREATE TRIGGER gang_write_fails BEFORE INSERT ON ci_attempts WHEN NEW.leg_id = 'm2' BEGIN SELECT RAISE(ABORT, 'the store can''t write'); END;",
                    )?)
                })
                .await?;
            let members = ["m0", "m1", "m2"];
            for m in members {
                track(&mut d, m, "a");
                if let Some(l) = d.legs.get_mut(m) {
                    l.agent = None;
                }
                let route = kiln_ci::sched::routing::LegRoute { labels: vec!["kiln".into()], ..kiln_ci::sched::routing::LegRoute::default() };
                let l = LegIn { id: m.into(), run: "R".into(), repo: "acme/x".into(), route, gang: Some(Gang { id: "R/sim".into(), size: 3 }), ..LegIn::default() };
                let cmds = d.sched.handle(Input::LegQueued(Box::new(l)), super::now_ms());
                d.commands(cmds).await;
            }
            let written = || hub.store.call(|c| Ok(c.prepare("SELECT leg_id, number FROM ci_attempts WHERE leg_id IN ('m0', 'm1', 'm2') ORDER BY leg_id")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>, _>>()?));
            assert_eq!(written().await?, Vec::<(String, i64)>::new(), "none of the gang's grants is in the store");
            for m in members {
                assert_eq!(granted(&hub, m), None, "{m} is no runner's");
                assert_eq!(d.sched.grant(m), Some((1, None)), "{m} queued again at its attempt");
                assert!(d.legs.get(m).is_some_and(|l| l.agent.is_none()), "{m} was sent to no runner");
            }
            // Granted, then taken back: the two its first's `Ungranted` took back go again a while
            // later, with it.
            let again = members.iter().filter(|m| matches!(d.sched.reason(m), Some(WaitReason::Retry { .. }))).count();
            assert_eq!(again, 2, "{:?}", members.map(|m| d.sched.reason(m).cloned()));
            hub.store.call(|c| Ok(c.execute_batch("DROP TRIGGER gang_write_fails")?)).await?;
            let cmds = d.sched.handle(Input::Tick, super::now_ms() + 16_000);
            d.commands(cmds).await;
            assert_eq!(written().await?, members.map(|m| (m.to_owned(), 1)), "the three grants written together");
            for m in members {
                assert!(granted(&hub, m).is_some_and(|g| g.1 == 1), "{m} granted whole, at its attempt");
            }
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (W26-R21): a cancel of some of a run's legs is answered once, on a
    /// one-shot (hyper-rt's, lock-free), from the dispatcher's shard to its caller's: each leg asked, in the
    /// order asked, and why it wasn't cancelled (none of these is a leg of the run).
    #[test]
    fn g4_s5_a_cancel_of_legs_is_answered_once_on_a_one_shot() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.ci.enabled = true).await?;
            super::spawn(std::sync::Arc::clone(&hub));
            let legs: Vec<String> = vec!["l3".into(), "l1".into(), "l2".into()];
            let (reply, answer) = hyper_rt::sync::oneshot().map_err(|e| anyhow::anyhow!("{e}"))?;
            hub.ci.send(super::Msg::CancelLegs { run: "no-such-run".into(), legs: legs.clone(), reason: "cancelled by a test".into(), reply });
            let each = crate::rt::timeout(std::time::Duration::from_secs(30), answer).await.map_err(|_| anyhow::anyhow!("the dispatcher never answered"))?.map_err(|_| anyhow::anyhow!("the dispatcher let go of the answer"))?;
            anyhow::ensure!(each.iter().map(|(l, _)| l).eq(legs.iter()), "each leg once, in the order asked: {each:?}");
            anyhow::ensure!(each.iter().all(|(_, r)| r.as_ref().is_err_and(|why| why == "not a leg of this run")), "{each:?}");
            Ok(())
        })
    }

    /// W05-R17: a rerun's `github.triggering_actor` is whoever reran it, as GitHub names people; an
    /// agent's is the person it acts for.
    #[test]
    fn w05_r17_triggering_actor_is_who_reran_it() {
        assert_eq!(super::login_of("user:lead"), "lead");
        assert_eq!(super::login_of("agent:abcdefgh for user:lead"), "lead");
        assert_eq!(super::login_of("agent:abcdefgh@mac for user:lead"), "lead");
        assert_eq!(super::login_of("octocat"), "octocat", "GitHub's Re-run, by its login");
    }

    /// W05-R03: in resource mode a leg's executor is told the memory it was given, to hold it to;
    /// in slot mode, and without a memory request, nothing.
    #[test]
    fn w05_r03_a_legs_memory_is_its_executors_limit() {
        let asked = kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::whole(4), mem: 16 << 30, disk: 0, gpu: 0 };
        assert_eq!(super::executor_options("resources", &asked), serde_json::json!({ "memory": 17179869184u64, "cpus": 4 }));
        assert_eq!(super::executor_options("slots", &asked), serde_json::Value::Null);
        assert_eq!(super::executor_options("resources", &kiln_ci::sched::state::Resources { mem: 0, ..asked }), serde_json::json!({ "cpus": 4 }));
        assert_eq!(super::executor_options("resources", &kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::milli(500), mem: 0, ..asked }), serde_json::json!({ "cpus": 1 }), "whole CPUs");
        assert_eq!(super::executor_options("resources", &kiln_ci::sched::state::Resources { cpu: kiln_ci::sched::state::Cpus::ZERO, mem: 0, ..asked }), serde_json::Value::Null);
    }

    /// W05-R03, W13-R04: Docker inside a leg's container goes beside the memory and CPUs it was
    /// given, not in their place.
    #[test]
    fn w05_r03_docker_inside_keeps_the_legs_resources() {
        let given = serde_json::json!({ "memory": 17179869184u64, "cpus": 4 });
        assert_eq!(super::with_docker(given, "socket"), serde_json::json!({ "memory": 17179869184u64, "cpus": 4, "docker": "socket" }));
        assert_eq!(super::with_docker(serde_json::Value::Null, "socket"), serde_json::json!({ "docker": "socket" }), "slot mode: Docker alone");
    }

    /// W05-R03: a leg's request in resource mode is its job's `resources:`, read as its needs are
    /// (`sched::needs`), else `[ci.scheduler] default_request`.
    #[test]
    fn w05_r03_a_legs_request_is_its_jobs_resources() -> anyhow::Result<()> {
        use kiln_ci::sched::state::Resources;
        let job = |resources: &str| {
            let src = format!("on: push\njobs:\n  build:\n    runs-on: linux\n{resources}    steps:\n      - run: make\n");
            let (wf, diags) = kiln_ci::wf::parse_workflow(".kiln/ci.yaml", &src, kiln_ci::expr::Dialect::Kiln);
            wf.and_then(|w| w.jobs.into_iter().next()).ok_or_else(|| anyhow::anyhow!("{diags:?}"))
        };
        let mut cfg = crate::config::ci::Ci::default();
        cfg.scheduler.default_request = kiln_ci::sched::pools::ResourceSpec { cpu: Some("1".into()), memory: Some("2Gi".into()), ..Default::default() };
        let asked = job("    resources:\n      cpu: 8\n      memory: 32Gi\n")?;
        assert_eq!(super::request_of(Some(&asked), &cfg), Resources { cpu: kiln_ci::sched::state::Cpus::whole(8), mem: 32 << 30, ..Resources::default() });
        let fallback = Resources { cpu: kiln_ci::sched::state::Cpus::whole(1), mem: 2 << 30, ..Resources::default() };
        assert_eq!(super::request_of(Some(&job("")?), &cfg), fallback);
        assert_eq!(super::request_of(None, &cfg), fallback);
        Ok(())
    }

    /// The scheduler's headroom (W05): a runner's sample reaches the engine as it says, its I/O
    /// pressure from its facts, and the engine's headroom of it is the sample's.
    #[test]
    fn a_runners_sample_reaches_the_engine_as_it_says() {
        use kiln_ci::sched::state::{Config, Input, RunnerIn, Scheduler};
        const GIB: u64 = 1 << 30;
        let host = crate::metrics::HostSample { cpus: 32, cpu_pct: 40.0, load1: 12.5, mem_total: 124 * GIB, mem_used: 60 * GIB, ..crate::metrics::HostSample::default() };
        let facts = kiln_ci::facts::RunnerFacts { work: Some(kiln_ci::facts::WorkFacts { io_full: Some(2.1), ..kiln_ci::facts::WorkFacts::default() }), ..kiln_ci::facts::RunnerFacts::default() };
        let s = super::sample_of(&host, Some(&facts));
        assert_eq!((s.cpus, s.busy, s.load1, s.mem_total, s.mem_used, s.io_full), (32, 0.4, 12.5, 124 * GIB, 60 * GIB, Some(2.1)));
        assert_eq!(super::sample_of(&host, None).io_full, None, "no facts yet, no pressure said");
        assert_eq!(super::sample_of(&crate::metrics::HostSample { cpu_pct: 250.0, ..host.clone() }, None).busy, 1.0);
        let mut engine = Scheduler::new(Config::default());
        engine.handle(Input::RunnerUp(Box::new(RunnerIn { id: "adacachy".into(), slots: 4, ..RunnerIn::default() })), 0);
        engine.handle(Input::RunnerSample { runner: "adacachy".into(), sample: s }, 1_000);
        assert!(engine.headroom("adacachy").is_some_and(|h| (h.cpus - 19.2).abs() < 1e-6 && h.mem == 64 * GIB && h.io_full == Some(2.1)), "{:?}", engine.headroom("adacachy"));
    }

    /// Goal 5, plan 01a1a59c2aae step 2: what the hub asks of the dispatcher on its runners'
    /// messages answers as it did under the lock: a leg or run the dispatcher took on is the
    /// engine's from then, until it lets it go; a finished run's legs go together, so a reader
    /// sees all of them or none; a kept leg is its runner's until it's let go or its time is up;
    /// what it rebuilt is said once; and its messages' end is taken once.
    /// Goal 11, plan 01a1a87b2e56 step 3: the dispatcher's inbox is bounded in bytes (on `/metrics` as
    /// the CI inbox's): past it a runner's message waits for room, and a person's goes all the same; a
    /// message weighs what it carries.
    #[test]
    fn g11_s3_the_dispatchers_inbox_is_bounded_in_bytes() -> anyhow::Result<()> {
        use crate::memory::queue::Weigh as _;
        let h = super::Handle::default();
        anyhow::ensure!(crate::memory::owners::CI_INBOX.bound().is_some_and(|b| b >= super::INBOX_BYTES));
        h.on.store(true, std::sync::atomic::Ordering::Relaxed);
        let up = |n: usize| super::Msg::RunnerUp { agent: "a".into(), running: vec!["l".repeat(n)] };
        anyhow::ensure!(up(1000).weight() - up(0).weight() == 1000);
        crate::rt::block_on(async move {
            h.send_held(up(super::INBOX_BYTES / 2)).await;
            let waited = crate::rt::timeout(std::time::Duration::from_millis(50), h.send_held(up(super::INBOX_BYTES / 2))).await.is_err();
            anyhow::ensure!(waited, "a runner's message waits for room");
            h.send(super::Msg::RoutingChanged);
            anyhow::ensure!(h.tx.len() == 2, "a person's went: {}", h.tx.len());
            Ok(())
        })
    }

    #[test]
    fn g5_the_dispatchers_legs_runs_and_kept_legs_read_without_a_lock() -> anyhow::Result<()> {
        let h = super::Handle::default();
        anyhow::ensure!(!h.is_leg("l1") && !h.is_run("r1") && h.restored().is_none());
        h.own_run("r1");
        h.own_legs(["l1"]);
        h.own_legs(["l2", "l3"]);
        anyhow::ensure!(h.is_run("r1") && h.is_leg("l1") && h.is_leg("l2") && h.is_leg("l3") && !h.is_leg("l4"));
        // A reader's copy, taken before the run finished, keeps every leg; the next read, none.
        let before = h.legs.get();
        h.drop_legs(["l1", "l2"]);
        h.drop_run("r1");
        anyhow::ensure!(before.len() == 3 && !h.is_leg("l1") && !h.is_leg("l2") && h.is_leg("l3") && !h.is_run("r1"));
        h.drop_legs(["l3"]);
        anyhow::ensure!(h.legs.load().is_empty());
        // Kept on a runner until a time to come; another runner doesn't keep it; let go, nobody does.
        let later = crate::util::now_unix() + 600;
        h.keep("l9", Some(("metal-1".into(), later)));
        anyhow::ensure!(h.kept_by("l9", "metal-1") && !h.kept_by("l9", "metal-2"));
        h.keep("l9", Some(("metal-1".into(), crate::util::now_unix() - 1)));
        anyhow::ensure!(!h.kept_by("l9", "metal-1"), "its time is up");
        h.keep("l9", None);
        anyhow::ensure!(h.kept.load().is_empty());
        let r = super::Restored { runs: 2, legs: 5, took: std::time::Duration::from_millis(3) };
        anyhow::ensure!(h.restored.set(r).is_ok() && h.restored() == Some(r));
        anyhow::ensure!(h.rx.swap(None).is_some() && h.rx.swap(None).is_none(), "the dispatcher's end is taken once");
        Ok(())
    }

    /// W10-R03: what waits on the rebuild (a leg's token that came before it) is woken by its end, and a
    /// wait begun after that returns at once.
    #[test]
    fn w10_r03_a_wait_for_the_rebuild_is_woken_by_its_end() -> anyhow::Result<()> {
        use futures::FutureExt as _;
        crate::rt::block_on(async {
            let h = super::Handle::default();
            let mut wait = Box::pin(h.until_restored());
            anyhow::ensure!((&mut wait).now_or_never().is_none(), "the rebuild isn't over");
            let r = super::Restored { runs: 1, legs: 1, took: std::time::Duration::from_millis(1) };
            anyhow::ensure!(h.restored.set(r).is_ok());
            h.restored_told.notify();
            crate::rt::timeout(std::time::Duration::from_secs(30), wait).await.map_err(|_| anyhow::anyhow!("the wait wasn't woken"))?;
            anyhow::ensure!(h.until_restored().now_or_never().is_some(), "the rebuild is over");
            Ok(())
        })
    }

    /// Goal 5: writers on many threads at once lose none of their changes (kept legs are kept by the
    /// dispatcher and by the API both), and readers on other threads go on reading meanwhile.
    #[test]
    fn g5_kept_legs_written_at_once_lose_nothing() -> anyhow::Result<()> {
        let h = std::sync::Arc::new(super::Handle::default());
        let later = crate::util::now_unix() + 600;
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let h = std::sync::Arc::clone(&h);
                std::thread::spawn(move || {
                    for i in 0..200 {
                        let leg = format!("l{t}-{i}");
                        h.keep(&leg, Some((format!("m{t}"), later)));
                        let _ = h.kept_by(&leg, "m0");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().map_err(|_| anyhow::anyhow!("a writer panicked"))?;
        }
        anyhow::ensure!(h.kept.load().len() == 8 * 200);
        anyhow::ensure!((0..8).all(|t| h.kept_by(&format!("l{t}-199"), &format!("m{t}"))));
        Ok(())
    }
}
