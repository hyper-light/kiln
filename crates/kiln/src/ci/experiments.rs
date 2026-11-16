//! Experiments (docs/ci.md W20-R11, after docs/experiments.md): options measured against each other
//! and together, on a base commit.
//!
//! - Each option is a change: a commit, a branch, or a diff (its latest version), applied as the
//!   commits it adds.
//! - The strategy makes the variants (`kiln_ci::experiments`). Each is its options' changes
//!   cherry-picked onto the base in the options' order, kept as
//!   `refs/kiln/experiments/<id>/<variant>`; one whose options conflict is said to, and isn't
//!   measured.
//! - Measuring needs no CI: a command runs in each variant's own worktree on the machine of the
//!   agent that asks (`crate::workspace::measure`), its exit code and the metrics it writes to
//!   `$KILN_METRICS` brought back. A pipeline, or a command given `runs-on`, is a CI run on routed
//!   runners instead (a command's a one-job workflow on a commit made for it), and only these need
//!   the CI engine. Repetitions are interleaved: each round runs every variant once, in a shuffled
//!   order, `max_parallel` at once, within the time budget.
//! - The results (`kiln_ci::experiments::analyze`): each variant's required checks, its metrics
//!   against the baseline's with intervals, each option's main effect and each pair's
//!   interaction, and a recommendation. Then decided: brought back (the default), or the clear
//!   winner taken (`decide: auto`), and the agent that asked told either way.
//! - **A split** (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, the person's decision 7): an experiment may deploy
//!   2 or more variants of a target instead of options. The target is one of the opener's agent definitions, or a
//!   harness and its Agentfile. Each variant (`arms`) is a deployment (an Agentfile, an agent or harness, an engine,
//!   a VM size) with a weight in percent. While it runs, each new session of the target that its opener starts is one
//!   of them, drawn by weight as it starts ([`assign`], from `harness::lineage::start`), recorded on the session
//!   (`Spec::experiment`) and kept for its life. Its measures come from those sessions as they run and end, each with
//!   its anytime-valid confidence sequence (`kiln_ci::split`), so reading them live inflates no false winner.
//!   Pausing stops the draws; deciding promotes one variant (a person's press), which every later session of the
//!   target gets, and ends the split; cancelling ends both.
//! - **A replay** (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1b, an addition to the split): one of its opener's
//!   recorded sessions, its tool calls run offline in a microVM of each of the split's microVM variants, `rounds` times,
//!   one run at a time on one runner (runs side by side would measure each other), interleaved: every round runs each
//!   variant once, in a shuffled order. What runs is `kiln_ci::replay`'s plan of the session's calls, on the runner
//!   (`crate::sandbox::replay`, asked by `ServerMsg::Replay`): no model, no person, no grant past its microVM. Each run
//!   measures what a session does, and its runs are read as a split's sessions are (`kiln_ci::split::analyze`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::experiments::{self as ex, Criterion, Kind, Measurement, Results, Strategy, Variant};
use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;
use kiln_ci::split;

/// An option: its name, what was given for it, and the commits it adds (`from..to`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OptionDef {
    pub name: String,
    /// `commit <sha>`, `branch <name>`, `diff D12`; or `built by its agent` for one an agent
    /// implements from `implement`.
    pub given: String,
    pub from: String,
    pub to: String,
    /// What it is, for the agent that implements it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implement: Option<String>,
    /// That agent, and the diff it submitted; or why there's no change to measure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// A variant as it's built and measured.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Built {
    pub name: String,
    pub options: Vec<String>,
    /// Its commit, once built: the base with its options' changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// `building`, `gluing` (an agent combines its options' conflicting changes), `ready`,
    /// `conflict` (`why`), `measured`.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// The agent gluing it, and the diff of the glue it made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glue: Option<String>,
    /// The runner minutes its runs took.
    pub minutes: f64,
}

/// One run of a variant for a criterion, in a round.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Run {
    pub variant: String,
    pub round: usize,
    pub criterion: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// The machine a command ran on beside the agent that asked (none for a CI run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// Its conclusion once it ended; `untestable` when it didn't start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub metrics: BTreeMap<String, f64>,
}

/// Who chose a variant, and when.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "ExperimentDecision"))]
#[serde(default)]
pub struct Decision {
    pub variant: String,
    pub commit: String,
    pub by: String,
    pub at: i64,
}

/// An experiment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Experiment {
    /// `E12`.
    pub id: String,
    pub repo: String,
    /// The commit every variant starts from, and the branch its runs stand for.
    pub base: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The agent that asked: told the results, and the variant chosen; its options' agents are
    /// its children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The agents that implement options: which harness, on which machine, in which folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessAsk>,
    /// Whom its agents act for (`owner` or `user:<id>`).
    pub opener: String,
    pub options: Vec<OptionDef>,
    /// What a split splits (P7.1): none for an experiment of options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// A split's variants, each a deployment with its weight.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arms: Vec<Arm>,
    /// A split's replays of recorded sessions (P7.1b), newest last: kept apart from it, and laid over it as it's shown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replays: Vec<Replay>,
    pub strategy: Option<Strategy>,
    pub baseline: bool,
    pub criteria: Vec<Criterion>,
    pub repetitions: usize,
    pub max_parallel: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_minutes: Option<u64>,
    /// Its time limit in seconds (a person sets it in any unit, a second to years); `budget_minutes`
    /// from before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_seconds: Option<u64>,
    /// `ask` (bring the results back) or `auto` (take the clear winner).
    pub decide: String,
    /// `implementing` (its options' agents at work), `building`, `measuring`, `done`, `cancelled`,
    /// `failed` (`why`).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub variants: Vec<Built>,
    pub runs: Vec<Run>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    /// Its runs are trusted as the owner's are; anyone else's are a fork's.
    pub trusted: bool,
    pub by: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// How many variants run at once, and repetitions, by default.
const PARALLEL: usize = 4;
const REPETITIONS: usize = 3;
/// The most repetitions an experiment measures each variant, and rounds a replay runs.
const MOST_REPETITIONS: usize = 20;
/// How long one measuring run may take.
const ONE_RUN: Duration = Duration::from_secs(3 * 3600);
/// How long a machine measuring beside the agent may go unheard before its measurement is given up.
const UNHEARD: Duration = Duration::from_secs(5 * 60);
/// The largest variant change sent to a machine to measure beside the agent.
const PATCH_MOST: usize = 8 << 20;

fn key(id: &str) -> String {
    format!("ci/experiment/{id}")
}

/// Who cancelled an experiment, and the variant chosen: kept apart from the experiment, which its
/// driver alone writes while it runs, and laid over it when it's read.
fn cancel_key(id: &str) -> String {
    format!("ci/experiment-cancel/{id}")
}

fn decision_key(id: &str) -> String {
    format!("ci/experiment-decision/{id}")
}

/// Who paused a split: its draws stop while this is kept.
fn paused_key(id: &str) -> String {
    format!("ci/experiment-paused/{id}")
}

/// A split's draws, each its own: the `n`-th session's variant, and what came of it.
fn drawn_key(id: &str, n: u64) -> String {
    format!("ci/experiment-drawn/{id}/{n:012}")
}

/// A split's replay `n` (P7.1b).
fn replay_key(id: &str, n: u64) -> String {
    format!("ci/experiment-replay/{id}/{n:06}")
}

/// Run `k` of a split's replay `n`, each kept on its own as it ends.
fn replayed_key(id: &str, n: u64, k: usize) -> String {
    format!("ci/experiment-replayed/{id}/{n:06}/{k:06}")
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

async fn save(hub: &Hub, e: &mut Experiment) {
    e.updated_at = crate::util::now_unix();
    if let Ok(text) = serde_json::to_string(&*e)
        && let Err(err) = hub.store.kv_set(key(&e.id), text).await
    {
        tracing::warn!(experiment = %e.id, error = %format!("{err:#}"), "an experiment couldn't be kept");
    }
    hub.emit(&crate::wire::events::ServerEvent::Experiment { repo: e.repo.clone(), experiment: Box::new(crate::wire::events::ExperimentBrief { id: e.id.clone(), repo: e.repo.clone(), state: e.state.clone(), measured: Some(e.runs.iter().filter(|r| r.conclusion.is_some()).count()), runs: Some(planned(e)), decision: None }) });
}

/// The runs an experiment plans: each buildable variant, each pass/fail criterion, each round.
fn planned(e: &Experiment) -> usize {
    let measuring = e.criteria.iter().filter(|c| !matches!(c.kind, Kind::Metric { .. })).count();
    e.variants.iter().filter(|v| v.state != "conflict").count() * measuring * e.repetitions
}

/// `e` with its cancellation and decision laid over it.
async fn overlay(hub: &Hub, mut e: Experiment) -> Experiment {
    if let Some(d) = hub.store.kv_get(decision_key(&e.id)).await.ok().flatten().and_then(|t| serde_json::from_str::<Decision>(&t).ok()) {
        e.decision = Some(d);
    }
    // A split runs until it's decided (its variant promoted), paused or cancelled; a cancel ends a promotion too.
    if e.target.is_some() {
        if e.decision.is_some() {
            e.state = "done".into();
        } else if let Some(by) = hub.store.kv_get(paused_key(&e.id)).await.ok().flatten() {
            e.state = "paused".into();
            e.why = Some(format!("paused by {by}"));
        }
    }
    if let Some(by) = hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten()
        && (e.target.is_some() || !matches!(e.state.as_str(), "done" | "failed"))
    {
        e.state = "cancelled".into();
        e.why = Some(format!("cancelled by {by}"));
    }
    e
}

pub async fn get(hub: &Hub, id: &str) -> Option<Experiment> {
    let e: Experiment = hub.store.kv_get(key(id)).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok())?;
    Some(overlay(hub, e).await)
}

async fn all(hub: &Hub) -> Vec<Experiment> {
    let mut v: Vec<Experiment> = Vec::new();
    for (_, t) in hub.store.kv_prefix("ci/experiment/".to_owned()).await.unwrap_or_default() {
        if let Ok(e) = serde_json::from_str::<Experiment>(&t) {
            v.push(overlay(hub, e).await);
        }
    }
    v.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    v
}

/// Records `d` as `e`'s decision.
async fn record_decision(hub: &Hub, e: &mut Experiment, d: Decision) {
    if let Ok(text) = serde_json::to_string(&d) {
        let _ = hub.store.kv_set(decision_key(&e.id), text).await;
    }
    e.decision = Some(d);
    hub.emit(&crate::wire::events::ServerEvent::Experiment { repo: e.repo.clone(), experiment: Box::new(crate::wire::events::ExperimentBrief { id: e.id.clone(), repo: e.repo.clone(), state: e.state.clone(), measured: None, runs: None, decision: e.decision.clone() }) });
}

/// The next experiment's id: `E1`, `E2`, ….
async fn next_id(hub: &Hub) -> anyhow::Result<String> {
    hub.store
        .call(|c| {
            let tx = crate::store::tx(c)?;
            let n: i64 = tx.query_row("SELECT v FROM kv WHERE k = 'ci/experiment-next'", [], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()).unwrap_or(1);
            tx.execute("INSERT INTO kv (k, v) VALUES ('ci/experiment-next', ?1) ON CONFLICT(k) DO UPDATE SET v = excluded.v", [(n + 1).to_string()])?;
            tx.commit()?;
            Ok(format!("E{n}"))
        })
        .await
}

async fn git(hub: &Hub, args: &[&str]) -> anyhow::Result<String> {
    Ok(String::from_utf8_lossy(&crate::gitops::run_git(hub, args, crate::gitops::LS_REMOTE_TIMEOUT).await?).trim().to_owned())
}

/// Where the agents that implement options run: the asking agent's harness, machine and folder
/// when left out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HarnessAsk {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What a start asks for.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Ask {
    repo: String,
    /// The commit the variants start from (the default branch's head when left out).
    base: Option<String>,
    question: Option<String>,
    agent: Option<String>,
    options: Vec<AskOption>,
    strategy: Option<Value>,
    /// `custom`'s variants: each a list of options.
    variants: Option<Vec<Vec<String>>>,
    baseline: Option<bool>,
    criteria: Vec<Criterion>,
    repetitions: Option<usize>,
    max_parallel: Option<usize>,
    budget_minutes: Option<u64>,
    budget_seconds: Option<u64>,
    decide: Option<String>,
    harness: Option<HarnessAsk>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AskOption {
    name: String,
    commit: Option<String>,
    branch: Option<String>,
    diff: Option<String>,
    /// What it is: an agent implements it.
    implement: Option<String>,
}

/// `POST /api/experiments`: an experiment started by `caller`, its variants built and measured from
/// now on.
pub async fn start(hub: &Arc<Hub>, caller: &Caller, body: Value) -> Result<Response, Fail> {
    if body.get("target").is_some_and(|t| !t.is_null()) {
        return start_split(hub, caller, body).await;
    }
    if body.get("arms").is_some_and(|a| !a.is_null()) {
        return Err(bad("`arms` are a split's: say what it splits (`target`)"));
    }
    let mut ask: Ask = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    // Left out: the repository the asking agent's folder is a checkout of.
    if ask.repo.trim().is_empty()
        && let Some(info) = ask.agent.clone().or_else(|| caller.agent.clone()).and_then(|a| hub.harness.info(&a))
    {
        ask.repo = repo_of_agent(hub, &info).await.ok_or_else(|| bad(format!("which repository? {}'s folder isn't a checkout of one kiln has: say `repo`", info.name)))?;
    }
    let repo =
        hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == ask.repo || r.id == ask.repo).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{}`", ask.repo)))?;
    if !caller.can("experiments.run", &Scope::Repo { repo: repo.name.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("{} may not run experiments on {}: that needs experiments.run", caller.name, repo.name)));
    }
    let strategy = match ask.strategy.as_ref().and_then(Value::as_str).unwrap_or("all") {
        "each" => Strategy::Each,
        "all" => Strategy::All,
        "pairs" => Strategy::Pairs,
        "greedy" => Strategy::Greedy,
        "custom" => Strategy::Custom(ask.variants.clone().ok_or_else(|| bad("`custom` takes `variants`: each a list of options"))?),
        other => return Err(bad(format!("`{other}`: a strategy is each, all, pairs, greedy or custom"))),
    };
    let names: Vec<String> = ask.options.iter().map(|o| o.name.trim().to_owned()).collect();
    let baseline = ask.baseline.unwrap_or(true);
    let variants = ex::variants(&names, &strategy, baseline).map_err(bad)?;
    if !ask.criteria.iter().any(|c| !matches!(c.kind, Kind::Metric { .. })) {
        return Err(bad("say how a variant is measured: a `command` (or `pipeline`) criterion (a metric is read from what they run)"));
    }
    // A command runs beside the agent that asks, in each variant's own worktree of its repository;
    // a pipeline, or a command given `runs-on`, is a CI run, and only these need CI's engine.
    let asker = ask.agent.clone().or_else(|| caller.agent.clone()).and_then(|a| hub.harness.info(&a));
    for c in &ask.criteria {
        let ci_run = match &c.kind {
            Kind::Pipeline { .. } => Some("a pipeline's run"),
            Kind::Command { runs_on: Some(_), .. } => Some("a command run on `runs-on`"),
            Kind::Command { runs_on: None, .. } | Kind::Metric { .. } => None,
        };
        if let Some(what) = ci_run
            && !hub.ci.enabled()
        {
            return Err((
                StatusCode::CONFLICT,
                format!("`{}` is {what}, which is CI's, and the CI engine is off ([ci] enabled): measure with a command, which runs in each variant's own worktree beside the agent that asks", c.name),
            ));
        }
        if matches!(c.kind, Kind::Command { runs_on: None, .. }) && asker.is_none() {
            return Err(bad(format!("`{}` runs in each variant's own worktree beside the agent that asks: start the experiment from an agent (`agent`), or say where it runs (`runs-on`, a CI run)", c.name)));
        }
    }
    let mut seen = Vec::new();
    for c in &ask.criteria {
        if c.name.trim().is_empty() || seen.contains(&c.name) {
            return Err(bad("each criterion has a name of its own"));
        }
        seen.push(c.name.clone());
    }
    let decide = ask.decide.clone().unwrap_or_else(|| "ask".into());
    if decide != "ask" && decide != "auto" {
        return Err(bad("`decide` is `ask` (bring the results back) or `auto` (take the clear winner)"));
    }
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    if !std::path::Path::new(&mirror).join("HEAD").exists() {
        return Err((StatusCode::CONFLICT, format!("the server hasn't mirrored {} yet; try again in a minute", repo.name)));
    }
    let facts = super::events::facts(hub, &repo).await;
    let base = match ask.base.as_deref().filter(|b| !b.is_empty()) {
        Some(b) => git(hub, &["-C", &mirror, "rev-parse", "--verify", &format!("{b}^{{commit}}")]).await.map_err(|_| bad(format!("no commit `{b}` in {}", repo.name)))?,
        None => git(hub, &["-C", &mirror, "rev-parse", "--verify", &format!("refs/heads/{}^{{commit}}", facts.default_branch)]).await.map_err(internal)?,
    };
    // The agents that implement options: as asked, or as the asking agent is.
    let harness = ask
        .harness
        .clone()
        .filter(|h| !h.kind.is_empty())
        .or_else(|| asker.as_ref().map(|i| HarnessAsk { kind: i.kind.clone(), on: Some(i.target.strip_prefix("agent:").unwrap_or(&i.target).to_owned()), cwd: Some(i.cwd.clone()), model: None }));
    let mut options = Vec::new();
    for o in &ask.options {
        if let Some(what) = o.implement.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
            if o.commit.is_some() || o.branch.is_some() || o.diff.is_some() {
                return Err(bad(format!("`{}`: an option an agent implements has no commit, branch or diff yet", o.name)));
            }
            if harness.is_none() {
                return Err(bad(format!("`{}` is for an agent to implement: say which (`harness`: kind, on, cwd), or start the experiment from an agent", o.name)));
            }
            options.push(OptionDef { name: o.name.trim().to_owned(), given: "built by its agent".into(), implement: Some(what.to_owned()), ..OptionDef::default() });
            continue;
        }
        let (given, to, from) = match (&o.commit, &o.branch, &o.diff) {
            (Some(c), None, None) => {
                (format!("commit {c}"), git(hub, &["-C", &mirror, "rev-parse", "--verify", &format!("{c}^{{commit}}")]).await.map_err(|_| bad(format!("`{}`: no commit {c}", o.name)))?, None)
            }
            (None, Some(b), None) => (
                format!("branch {b}"),
                git(hub, &["-C", &mirror, "rev-parse", "--verify", &format!("refs/heads/{b}^{{commit}}")]).await.map_err(|_| bad(format!("`{}`: no branch {b}", o.name)))?,
                None,
            ),
            (None, None, Some(d)) => {
                let diff = super::diffs::find(hub, d).await.map_err(internal)?.filter(|x| x.repo == repo.name).ok_or_else(|| bad(format!("`{}`: no diff {d} in {}", o.name, repo.name)))?;
                let last = super::diffs::versions(hub, &diff).await.map_err(internal)?.into_iter().max_by_key(|v| v.n).ok_or_else(|| bad(format!("`{}`: {d} has no version", o.name)))?;
                (format!("diff {}", diff.name), last.commit, Some(last.parent))
            }
            _ => return Err(bad(format!("`{}`: an option is a `commit`, a `branch` or a `diff`", o.name))),
        };
        let from = match from {
            Some(p) => p,
            None => git(hub, &["-C", &mirror, "merge-base", &base, &to]).await.map_err(|_| bad(format!("`{}` shares no history with the base", o.name)))?,
        };
        options.push(OptionDef { name: o.name.trim().to_owned(), given, from, to, ..OptionDef::default() });
    }
    let opener = if caller.is_owner() { "owner".to_owned() } else { caller.user.clone().map_or_else(|| "owner".to_owned(), |u| format!("user:{u}")) };
    let id = next_id(hub).await.map_err(internal)?;
    let now = crate::util::now_unix();
    let mut e = Experiment {
        id: id.clone(),
        repo: repo.name.clone(),
        base,
        branch: facts.default_branch.clone(),
        question: ask.question.clone(),
        agent: ask.agent.clone().or_else(|| caller.agent.clone()),
        harness,
        opener,
        options,
        strategy: Some(strategy),
        baseline,
        criteria: ask.criteria.clone(),
        repetitions: ask.repetitions.unwrap_or(REPETITIONS).clamp(1, MOST_REPETITIONS),
        max_parallel: ask.max_parallel.unwrap_or(PARALLEL).clamp(1, 16),
        budget_minutes: ask.budget_minutes,
        budget_seconds: ask.budget_seconds.or(ask.budget_minutes.map(|m| m.saturating_mul(60))).filter(|s| *s > 0),
        decide,
        state: "building".into(),
        variants: variants.iter().map(|v| Built { name: v.name.clone(), options: v.options.clone(), state: "building".into(), ..Built::default() }).collect(),
        trusted: caller.is_owner(),
        by: caller.name.clone(),
        created_at: now,
        ..Experiment::default()
    };
    save(hub, &mut e).await;
    hub.store.audit(caller.name.clone(), "experiments.start".into(), format!("{id} on {}: {} variants of {}", repo.name, e.variants.len(), names.join(", "))).await;
    let answer = e.clone();
    let h = Arc::clone(hub);
    crate::rt::spawn(async move { drive(h, e, repo).await });
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "experiment": answer })))
}

/// The kiln repository agent `info`'s folder is a checkout of, by its remotes.
async fn repo_of_agent(hub: &Hub, info: &crate::harness::session::Info) -> Option<String> {
    let answer = crate::workspace::change::ask_machine(hub, info, &crate::workspace::Ask::Git).await.ok()?;
    let facts: crate::workspace::git::Facts = serde_json::from_value(answer.get("facts").cloned()?).ok()?;
    let repos = hub.store.repos().await.ok()?;
    crate::workspace::change::matched(&repos, &facts.remotes, &hub.cfg.server.public_url).map(|(r, _)| r.name.clone())
}

/// Builds `e`'s variants, measures them, says what the measurements show and decides; then the
/// worktrees it measured in beside the agent that asked go.
async fn drive(hub: Arc<Hub>, mut e: Experiment, repo: Repo) {
    run_it(&hub, &mut e, &repo).await;
    unmeasure(&hub, &e).await;
}

async fn run_it(hub: &Arc<Hub>, e: &mut Experiment, repo: &Repo) {
    // A limit too far off to fall within this server's clock is none.
    let deadline = limit(e).and_then(|s| std::time::Instant::now().checked_add(Duration::from_secs(s)));
    if e.options.iter().any(|o| o.implement.is_some() && o.to.is_empty()) {
        e.state = "implementing".into();
        save(hub, e).await;
        implement(hub, e, repo, deadline).await;
        if e.state == "cancelled" {
            save(hub, e).await;
            return;
        }
    }
    let names: Vec<String> = e.options.iter().map(|o| o.name.clone()).collect();
    let greedy = matches!(e.strategy, Some(Strategy::Greedy));
    let mut round_variants: Vec<String> = e.variants.iter().map(|v| v.name.clone()).collect();
    loop {
        if let Err(why) = build(hub, e, repo, &round_variants).await {
            e.state = "failed".into();
            e.why = Some(why);
            save(hub, e).await;
            return;
        }
        e.state = "measuring".into();
        save(hub, e).await;
        let mut ended = measure(hub, e, repo, &round_variants, deadline).await;
        // The variants agents glued, measured once they're made.
        if ended.is_none() {
            let glued = glue_finish(hub, e, repo, &round_variants, deadline).await;
            if !glued.is_empty() {
                ended = measure(hub, e, repo, &glued, deadline).await;
            }
        }
        analyze(e);
        save(hub, e).await;
        if ended.is_some() || !greedy {
            if let Some(why) = ended {
                e.why = Some(why);
            }
            break;
        }
        // Greedy stacking: the best so far with each of the rest, while that's better still.
        let best = e.results.as_ref().and_then(|r| r.get("recommendation")).and_then(Value::as_str).and_then(|n| e.variants.iter().find(|v| v.name == n)).map(|v| v.options.clone());
        let Some(best) = best else { break };
        let next: Vec<Variant> = ex::greedy_next(&names, &best).into_iter().filter(|v| !e.variants.iter().any(|x| x.name == v.name)).collect();
        if next.is_empty() || e.variants.len() + next.len() > ex::MOST_VARIANTS {
            break;
        }
        // A round whose best isn't better than the last's adds nothing: its next round would be
        // variants measured already, and it stops.
        round_variants = next.iter().map(|v| v.name.clone()).collect();
        e.variants.extend(next.into_iter().map(|v| Built { name: v.name, options: v.options, state: "building".into(), ..Built::default() }));
        save(hub, e).await;
    }
    if e.state != "cancelled" {
        e.state = "done".into();
        let recommended = e.results.as_ref().and_then(|r| r.get("recommendation")).and_then(Value::as_str).map(str::to_owned);
        save(hub, e).await;
        if e.decide == "auto"
            && let Some(v) = recommended.as_ref().and_then(|n| e.variants.iter().find(|v| &v.name == n)).cloned()
        {
            record_decision(hub, e, Decision { variant: v.name.clone(), commit: v.commit.clone().unwrap_or_default(), by: "kiln".into(), at: crate::util::now_unix() }).await;
        }
        let told = overlay(hub, e.clone()).await;
        tell(hub, &told).await;
    }
}

/// How long the agents implementing options have, at most.
const IMPLEMENTING: Duration = Duration::from_secs(4 * 3600);

/// The brief of the agent implementing option `o` of `e`.
fn brief(e: &Experiment, o: &OptionDef) -> String {
    let short = e.repo.rsplit('/').next().unwrap_or(&e.repo);
    format!(
        "You're building one option of experiment {id} on {repo}, which measures options against each other{q}.\n\n\
         Your option is `{name}`: {what}\n\n\
         Do only this option, from commit {base} ({branch}), in a git worktree of your own, not the folder you start in (the experiment's other agents share it): `git worktree add ../{short}-{id}-{name} {base}`. \
         Commit it there, then submit it for review with kiln's `ci_diffs_submit_patch` tool: {{\"repo\": \"{repo}\", \"mbox\": <what `git format-patch {base}..HEAD --stdout` prints>}}. \
         Don't land it. Say the diff it made (`D12`) on your last line: kiln measures it with the other options.",
        id = e.id,
        repo = e.repo,
        q = e.question.as_deref().map(|q| format!(": {q}")).unwrap_or_default(),
        name = o.name,
        what = o.implement.clone().unwrap_or_default(),
        base = e.base,
        branch = e.branch,
    )
}

/// The options agents implement: an agent each, beside the asking one (its child), told to work in
/// a worktree of its own and submit the change as a diff; then each option is that diff's change.
async fn implement(hub: &Arc<Hub>, e: &mut Experiment, repo: &Repo, deadline: Option<std::time::Instant>) {
    let Some(h) = e.harness.clone() else { return };
    let parent = e.agent.as_ref().and_then(|a| hub.harness.info(a));
    let mut started: Vec<(usize, String)> = Vec::new();
    for (i, o) in e.options.clone().iter().enumerate().filter(|(_, o)| o.implement.is_some() && o.to.is_empty()) {
        let starter = crate::harness::lineage::Starter { parent: parent.clone(), opener: Some(e.opener.clone()), actor: format!("experiment:{}", e.id), plan_run: None, asker: None };
        let ask = crate::harness::lineage::Ask {
            kind: h.kind.clone(),
            on: h.on.clone(),
            cwd: h.cwd.clone(),
            prompt: Some(brief(e, o)),
            model: h.model.clone(),
            name: Some(format!("{} option {}", e.id, o.name)),
            count: Some(1),
            ..Default::default()
        };
        match crate::harness::lineage::start(hub, &starter, ask).await {
            Ok(infos) => match infos.first() {
                Some(info) => {
                    if let Some(d) = e.options.get_mut(i) {
                        d.agent = Some(info.id.clone());
                    }
                    started.push((i, info.id.clone()));
                }
                None => {
                    if let Some(d) = e.options.get_mut(i) {
                        d.why = Some("no agent started".into());
                    }
                }
            },
            Err((_, why)) => {
                if let Some(d) = e.options.get_mut(i) {
                    d.why = Some(format!("its agent couldn't start: {why}"));
                }
            }
        }
    }
    save(hub, e).await;
    let ids: Vec<String> = started.iter().map(|(_, id)| id.clone()).collect();
    settle(hub, e, &ids, deadline).await;
    for (i, id) in started {
        let found = diff_of(hub, repo, &id).await;
        let Some(o) = e.options.get_mut(i) else { continue };
        match found {
            Ok((diff, from, to)) => {
                o.given = format!("diff {diff}, by its agent");
                o.diff = Some(diff);
                o.from = from;
                o.to = to;
            }
            Err(why) => o.why = Some(why),
        }
    }
    save(hub, e).await;
}

/// A change to apply: its name, and the commits it adds (`from..to`).
struct Change {
    name: String,
    from: String,
    to: String,
}

/// `changes` cherry-picked onto `base` in the workspace `w`, in their order: the commit made, or
/// why they don't apply (which conflicts with which, in which files).
async fn compose(hub: &Hub, w: &str, base: &str, changes: &[Change]) -> Result<String, String> {
    let me = ["-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "-C", w];
    git(hub, &[&me[..], &["checkout", "--quiet", "--detach", base]].concat()).await.map_err(|err| format!("{err:#}"))?;
    for (k, c) in changes.iter().enumerate() {
        if c.from == c.to {
            continue;
        }
        if let Err(err) = git(hub, &[&me[..], &["cherry-pick", "--allow-empty", "--keep-redundant-commits", &format!("{}..{}", c.from, c.to)]].concat()).await {
            let files = git(hub, &["-C", w, "diff", "--name-only", "--diff-filter=U"]).await.unwrap_or_default();
            let _ = git(hub, &["-C", w, "cherry-pick", "--abort"]).await;
            let others: Vec<&str> = changes.iter().take(k).map(|x| x.name.as_str()).collect();
            return Err(if files.is_empty() {
                format!("{} doesn't apply{}: {}", c.name, if others.is_empty() { String::new() } else { format!(" on {}", others.join("+")) }, crate::util::truncate(&format!("{err:#}"), 300))
            } else {
                format!("{} conflicts with {} in {}", c.name, if others.is_empty() { "the base".to_owned() } else { others.join("+") }, files.lines().collect::<Vec<_>>().join(", "))
            });
        }
    }
    git(hub, &["-C", w, "rev-parse", "HEAD"]).await.map_err(|err| format!("{err:#}"))
}

/// Keeps `sha` as variant `variant`'s ref of experiment `id`, in the mirror and in a repository kiln
/// hosts.
async fn keep(hub: &Hub, repo: &Repo, w: &str, id: &str, variant: &str, sha: &str) -> Result<(), String> {
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let name = format!("refs/kiln/experiments/{id}/{variant}");
    git(hub, &["-C", w, "push", "--quiet", "--force", &mirror, &format!("{sha}:{name}")]).await.map_err(|err| format!("keeping {variant}: {err:#}"))?;
    if let Some(hosted) = super::hosting::native_path(hub, repo) {
        let h = hosted.to_string_lossy().into_owned();
        git(hub, &["-C", &h, "fetch", "--quiet", "--no-tags", &mirror, &format!("+{name}:{name}")]).await.map_err(|err| format!("{err:#}"))?;
    }
    Ok(())
}

/// A workspace for building `e`'s variants: a shared clone of the repository's mirror.
async fn workspace(hub: &Hub, e: &Experiment, repo: &Repo, what: &str) -> Result<(std::path::PathBuf, String), String> {
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let work = hub.cfg.data("tmp").join("experiments").join(&e.id).join(what);
    let _ = std::fs::remove_dir_all(&work);
    if let Some(p) = work.parent() {
        std::fs::create_dir_all(p).map_err(|err| format!("{err:#}"))?;
    }
    let w = work.to_string_lossy().into_owned();
    git(hub, &["clone", "--quiet", "--no-checkout", "--shared", &mirror, &w]).await.map_err(|err| format!("a workspace for its variants: {err:#}"))?;
    Ok((work, w))
}

/// Builds `which` of `e`'s variants: each its options' changes on the base, kept as a ref of the
/// experiment's. One whose options conflict is said to; with agents to ask, one is asked to
/// combine them (a glue commit), and the variant waits for it; else it isn't measured.
async fn build(hub: &Arc<Hub>, e: &mut Experiment, repo: &Repo, which: &[String]) -> Result<(), String> {
    let (work, w) = workspace(hub, e, repo, "work").await?;
    for name in which {
        let Some(v) = e.variants.iter().position(|v| &v.name == name) else { continue };
        let options = e.variants.get(v).map(|x| x.options.clone()).unwrap_or_default();
        let mut changes = Vec::new();
        let mut missing = None;
        for o in &options {
            let Some(def) = e.options.iter().find(|d| &d.name == o) else { continue };
            if def.to.is_empty() {
                missing = Some(format!("{o} has no change to apply: {}", def.why.clone().unwrap_or_else(|| "its agent didn't make one".into())));
                break;
            }
            changes.push(Change { name: o.clone(), from: def.from.clone(), to: def.to.clone() });
        }
        let built = match missing {
            Some(why) => Err(why),
            None => compose(hub, &w, &e.base, &changes).await,
        };
        let id = e.id.clone();
        match built {
            Ok(sha) => {
                keep(hub, repo, &w, &id, name, &sha).await?;
                if let Some(b) = e.variants.get_mut(v) {
                    b.commit = Some(sha);
                    b.state = "ready".into();
                }
            }
            Err(why) => {
                let conflicts = why.contains(" conflicts with ") || why.contains(" doesn't apply");
                let glue = if conflicts && e.harness.is_some() { glue_start(hub, e, name, &why).await } else { None };
                if let Some(b) = e.variants.get_mut(v) {
                    match glue {
                        Some(agent) => {
                            b.state = "gluing".into();
                            b.agent = Some(agent);
                            b.why = Some(format!("{why}: an agent is combining them"));
                        }
                        None => {
                            b.state = "conflict".into();
                            b.why = Some(why);
                        }
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

/// Starts the agent that combines variant `name`'s options, whose changes conflict (`why`): its id.
async fn glue_start(hub: &Arc<Hub>, e: &Experiment, name: &str, why: &str) -> Option<String> {
    let h = e.harness.clone()?;
    let v = e.variants.iter().find(|v| v.name == name)?;
    let short = e.repo.rsplit('/').next().unwrap_or(&e.repo);
    let steps: Vec<String> = v
        .options
        .iter()
        .filter_map(|o| e.options.iter().find(|d| &d.name == o))
        .map(|d| format!("- `{}`: the commits {}..{} (fetch {} from kiln's git by its SHA if your clone hasn't it)", d.name, d.from, d.to, d.to))
        .collect();
    let brief = format!(
        "Experiment {id} on {repo} measures {name}, the options {list} together, but their changes conflict: {why}.\n\n\
         In a git worktree of your own from commit {base} (`git worktree add ../{short}-{id}-{slug} {base}`), apply each option's change in this order, resolving the conflicts so every option still does what it does:\n{steps}\n\n\
         Then submit the combination as one diff with kiln's `ci_diffs_submit_patch` tool: {{\"repo\": \"{repo}\", \"mbox\": <what `git format-patch {base}..HEAD --stdout` prints>}}. Don't land it. Say the diff it made (`D12`) on your last line.",
        id = e.id,
        repo = e.repo,
        list = v.options.join(", "),
        base = e.base,
        slug = name.replace('+', "-"),
        steps = steps.join("\n"),
    );
    let parent = e.agent.as_ref().and_then(|a| hub.harness.info(a));
    let starter = crate::harness::lineage::Starter { parent, opener: Some(e.opener.clone()), actor: format!("experiment:{}", e.id), plan_run: None, asker: None };
    let ask =
        crate::harness::lineage::Ask { kind: h.kind, on: h.on, cwd: h.cwd, prompt: Some(brief), model: h.model, name: Some(format!("{} glue {name}", e.id)), count: Some(1), ..Default::default() };
    crate::harness::lineage::start(hub, &starter, ask).await.ok()?.first().map(|i| i.id.clone())
}

/// The diff an agent's last words name: the last `D<number>` in them.
fn named_diff(said: &str) -> Option<&str> {
    said.split(|c: char| !c.is_ascii_alphanumeric()).rfind(|w| w.len() > 1 && w.starts_with('D') && w.get(1..).is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
}

/// The diff agent `id` says it made (`D12` on its last line), and that diff's latest change
/// (`parent..commit`), if it's in `repo`.
async fn diff_of(hub: &Hub, repo: &Repo, id: &str) -> Result<(String, String, String), String> {
    let said = hub.harness.reply(id).await.and_then(|r| r.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
    let Some(d) = named_diff(&said) else { return Err(format!("its agent ended without a diff: {}", crate::util::truncate(said.trim(), 200))) };
    let diff = super::diffs::find(hub, d).await.ok().flatten().filter(|x| x.repo == repo.name).ok_or_else(|| format!("its agent named {d}, which isn't a diff of {}", repo.name))?;
    let v = super::diffs::versions(hub, &diff).await.ok().and_then(|v| v.into_iter().max_by_key(|x| x.n)).ok_or_else(|| format!("{d} has no version"))?;
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    git(hub, &["-C", &mirror, "cat-file", "-e", &format!("{}^{{commit}}", v.commit)]).await.map_err(|_| format!("{d}'s commit isn't on the server"))?;
    Ok((diff.name, v.parent, v.commit))
}

/// Waits for agents `ids` to settle: done, waiting on a person, stopped; the experiment cancelled,
/// or out of time.
#[allow(clippy::disallowed_methods)] // (e) a picked settle before it looks again
async fn settle(hub: &Hub, e: &mut Experiment, ids: &[String], deadline: Option<std::time::Instant>) {
    let until = std::time::Instant::now() + IMPLEMENTING;
    loop {
        if stopped(hub, e, deadline).await.is_some() || std::time::Instant::now() >= until {
            return;
        }
        let pending = ids.iter().any(|id| hub.harness.info(id).is_some_and(|info| !crate::harness::wait::settled(crate::harness::wait::stand(&info))));
        if !pending {
            return;
        }
        crate::rt::sleep(Duration::from_secs(10)).await;
    }
}

/// The glue `which` of `e`'s variants wait for: each agent's diff, its change applied on the base,
/// the variant ready to measure; or why not. The names of those now ready.
async fn glue_finish(hub: &Arc<Hub>, e: &mut Experiment, repo: &Repo, which: &[String], deadline: Option<std::time::Instant>) -> Vec<String> {
    let waiting: Vec<(String, String)> = e.variants.iter().filter(|v| which.contains(&v.name) && v.state == "gluing").filter_map(|v| Some((v.name.clone(), v.agent.clone()?))).collect();
    if waiting.is_empty() {
        return Vec::new();
    }
    let ids: Vec<String> = waiting.iter().map(|(_, a)| a.clone()).collect();
    settle(hub, e, &ids, deadline).await;
    let Ok((work, w)) = workspace(hub, e, repo, "glue").await else { return Vec::new() };
    let mut ready = Vec::new();
    for (name, agent) in waiting {
        let made = match diff_of(hub, repo, &agent).await {
            Ok((diff, from, to)) => match compose(hub, &w, &e.base, &[Change { name: diff.clone(), from, to }]).await {
                Ok(sha) => keep(hub, repo, &w, &e.id, &name, &sha).await.map(|()| (diff, sha)),
                Err(why) => Err(why),
            },
            Err(why) => Err(why),
        };
        if let Some(b) = e.variants.iter_mut().find(|v| v.name == name) {
            match made {
                Ok((diff, sha)) => {
                    b.commit = Some(sha);
                    b.glue = Some(diff);
                    b.state = "ready".into();
                    ready.push(name.clone());
                }
                Err(why) => {
                    b.state = "conflict".into();
                    b.why = Some(format!("{}; its glue: {why}", b.why.clone().unwrap_or_default()));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&work);
    save(hub, e).await;
    ready
}

/// A seeded shuffle (Fisher–Yates with splitmix64), so an experiment's order can be told again.
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut s = seed;
    let mut next = || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..v.len()).rev() {
        let j = usize::try_from(next() % (i as u64 + 1)).unwrap_or(0);
        v.swap(i, j);
    }
}

fn seed_of(id: &str) -> u64 {
    id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Measures `which` of `e`'s ready variants: every round, each variant once for each pass/fail
/// criterion, in a shuffled order, `max_parallel` at once. Why it stopped early, when it did.
async fn measure(hub: &Arc<Hub>, e: &mut Experiment, repo: &Repo, which: &[String], deadline: Option<std::time::Instant>) -> Option<String> {
    let measuring: Vec<Criterion> = e.criteria.iter().filter(|c| !matches!(c.kind, Kind::Metric { .. })).cloned().collect();
    // Each variant's change, as the machine measuring beside the agent applies it: made once.
    let mut patches: BTreeMap<String, Result<String, String>> = BTreeMap::new();
    for round in 0..e.repetitions {
        let mut order: Vec<(String, String, Criterion)> = e
            .variants
            .iter()
            .filter(|v| which.contains(&v.name) && v.state == "ready")
            .filter_map(|v| Some((v.name.clone(), v.commit.clone()?)))
            .flat_map(|(n, c)| measuring.iter().map(move |k| (n.clone(), c.clone(), k.clone())))
            .collect();
        shuffle(&mut order, seed_of(&e.id) ^ (round as u64).wrapping_mul(0x9E37_79B9));
        for chunk in order.chunks(e.max_parallel) {
            if let Some(why) = stopped(hub, e, deadline).await {
                return Some(why);
            }
            let mut started = Vec::new();
            for (variant, commit, c) in chunk {
                let r = match &c.kind {
                    Kind::Command { command, runs_on: None } => {
                        if !patches.contains_key(variant) {
                            patches.insert(variant.clone(), patch_of(hub, repo, &e.base, commit).await);
                        }
                        let patch = patches.get(variant).cloned().unwrap_or_else(|| Err("no change".into()));
                        match start_beside(hub, e, variant, &c.name, command, round, patch).await {
                            Ok(r) => r,
                            // Nothing of it can be measured there: it stops, saying why.
                            Err(why) => return Some(why),
                        }
                    }
                    _ => start_one(hub, e, repo, variant, commit, c, round).await,
                };
                started.push(r);
            }
            for mut r in started {
                let minutes = if let Some(run) = r.run.clone() {
                    let (conclusion, metrics, minutes) = finish(hub, &e.id, &run, deadline).await;
                    r.conclusion = Some(conclusion);
                    r.metrics = metrics;
                    minutes
                } else if r.on.is_some() && r.conclusion.is_none() {
                    finish_beside(hub, e, &mut r, deadline).await
                } else {
                    0.0
                };
                if let Some(v) = e.variants.iter_mut().find(|v| v.name == r.variant) {
                    v.minutes += minutes;
                }
                e.runs.push(r);
            }
            save(hub, e).await;
        }
    }
    for v in e.variants.iter_mut().filter(|v| which.contains(&v.name) && v.state == "ready") {
        v.state = "measured".into();
    }
    None
}

/// `e`'s time limit, in seconds.
fn limit(e: &Experiment) -> Option<u64> {
    e.budget_seconds.or(e.budget_minutes.map(|m| m.saturating_mul(60)))
}

/// `seconds` in the largest unit it's a whole number of: `90 seconds`, `2 hours`, `3 days`.
fn span(seconds: u64) -> String {
    let (n, unit) = [(86_400, "day"), (3_600, "hour"), (60, "minute")].into_iter().find(|(u, _)| seconds >= *u && seconds.is_multiple_of(*u)).map_or((seconds, "second"), |(u, name)| (seconds / u, name));
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Whether to stop: cancelled, or past the budget (its runs cancelled either way).
async fn stopped(hub: &Hub, e: &mut Experiment, deadline: Option<std::time::Instant>) -> Option<String> {
    if let Some(by) = hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten() {
        e.state = "cancelled".into();
        return Some(format!("cancelled by {by}"));
    }
    deadline.filter(|d| std::time::Instant::now() >= *d).map(|_| format!("its time limit of {} ran out: what was measured is reported", span(limit(e).unwrap_or(0))))
}

/// Measurement `round` of `variant` by `criterion`, as the machine beside the agent knows it: the
/// experiment's id and when it was made (ids of two servers' experiments can meet on a machine).
fn measure_key(e: &Experiment, variant: &str, criterion: &str, round: usize) -> String {
    format!("{}/{variant}/{criterion}/{round}", beside_id(e))
}

fn beside_id(e: &Experiment) -> String {
    format!("{}-{}", e.id, e.created_at)
}

/// What variant commit `commit` changes over `base`, as `git apply` takes it on the machine that
/// measures it (base64: a change's files needn't be UTF-8); or why it can't be sent.
async fn patch_of(hub: &Hub, repo: &Repo, base: &str, commit: &str) -> Result<String, String> {
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let out = crate::gitops::run_git(hub, &["-C", &mirror, "diff", "--binary", "--full-index", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv", base, commit], crate::gitops::LS_REMOTE_TIMEOUT)
        .await
        .map_err(|err| format!("its change: {err:#}"))?;
    if out.len() > PATCH_MOST {
        return Err(format!("its change is {} MiB, more than kiln sends a machine to measure ({} MiB)", out.len() >> 20, PATCH_MOST >> 20));
    }
    Ok(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, out))
}

/// Starts measuring `variant` by command criterion `criterion` in round `round` beside the agent
/// that asked: in the variant's own worktree of its repository, on its machine, its change
/// `patch` on the base (docs/ci.md W20-R11, measured without CI). `Err` when that machine can't
/// measure at all (its kiln is older than the server's): why.
async fn start_beside(hub: &Hub, e: &Experiment, variant: &str, criterion: &str, command: &str, round: usize, patch: Result<String, String>) -> Result<Run, String> {
    let mut r = Run { variant: variant.to_owned(), round, criterion: criterion.to_owned(), ..Run::default() };
    let Some(info) = e.agent.as_ref().and_then(|a| hub.harness.info(a)) else {
        r.conclusion = Some("untestable".into());
        r.why = Some("the agent that asked is gone, and a command runs beside it".into());
        return Ok(r);
    };
    let machine = crate::harness::wait::machine(&info.target).to_owned();
    r.on = Some(machine.clone());
    let patch = match patch {
        Ok(p) => p,
        Err(why) => {
            r.conclusion = Some("untestable".into());
            r.why = Some(why);
            return Ok(r);
        }
    };
    let m = crate::workspace::measure::Measure {
        key: measure_key(e, variant, criterion, round),
        experiment: beside_id(e),
        variant: variant.to_owned(),
        base: e.base.clone(),
        patch,
        command: command.to_owned(),
        limit_s: ONE_RUN.as_secs(),
    };
    match crate::workspace::change::ask_machine(hub, &info, &crate::workspace::Ask::Measure(m)).await {
        Ok(_) => {}
        // Not answered in time (a large repository's worktree, made first): it may be under way,
        // and asked about it says.
        Err((StatusCode::BAD_GATEWAY, _)) => {}
        // A runner from before measuring: it can't read the ask (`crate::wire::payload`; older,
        // `workspace::serve`), or takes no asks about agents' folders at all.
        Err((_, why)) if ["couldn't be read", "doesn't understand", "too old"].iter().any(|w| why.contains(w)) => {
            return Err(format!("{}'s kiln is older than this server's and can't measure beside its agent: upgrade its runner (System → Updates), then start the experiment again ({why})", hub.shown(&machine)));
        }
        Err((_, why)) => {
            r.conclusion = Some("untestable".into());
            r.why = Some(why);
        }
    }
    Ok(r)
}

/// A measurement beside the agent that asked, to its end: its conclusion and metrics in `r`, and
/// the minutes it took; what it printed last, when it failed. Stopped with the experiment, past
/// its budget, or past [`ONE_RUN`].
#[allow(clippy::disallowed_methods)] // (e) a picked settle before it looks again
async fn finish_beside(hub: &Hub, e: &Experiment, r: &mut Run, deadline: Option<std::time::Instant>) -> f64 {
    let key = measure_key(e, &r.variant, &r.criterion, r.round);
    let machine = r.on.clone().unwrap_or_default();
    let (until, mut heard) = (std::time::Instant::now() + ONE_RUN, std::time::Instant::now());
    loop {
        let Some(info) = e.agent.as_ref().and_then(|a| hub.harness.info(a)) else {
            r.conclusion = Some("untestable".into());
            r.why = Some("the agent that asked is gone, and its measurements with it".into());
            return 0.0;
        };
        let now = std::time::Instant::now();
        let cancelled = hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten().is_some();
        let stop = cancelled || now >= until || deadline.is_some_and(|d| now >= d);
        let asked = crate::workspace::change::ask_machine(hub, &info, &crate::workspace::Ask::Measured { key: key.clone(), stop }).await;
        if stop {
            r.conclusion = Some("cancelled".into());
            return 0.0;
        }
        match asked {
            Ok(v) => {
                heard = now;
                if let Some(done) = v.get("done") {
                    let code = done.get("exit_code").and_then(Value::as_i64);
                    r.metrics = serde_json::from_value(done.get("metrics").cloned().unwrap_or_default()).unwrap_or_default();
                    r.conclusion = Some(if code == Some(0) { "success" } else { "failure" }.into());
                    if code != Some(0) {
                        let error = done.get("error").and_then(Value::as_str).filter(|x| !x.is_empty());
                        let output = done.get("output").and_then(Value::as_str).unwrap_or_default();
                        let last: Vec<&str> = output.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
                        let said = match (error, code) {
                            (Some(err), _) => err.to_owned(),
                            (None, Some(c)) => format!("exit code {c}"),
                            (None, None) => "ended by a signal".to_owned(),
                        };
                        r.why = Some(crate::util::truncate(&format!("{said}: {}", last.join("\n")), 2000).trim_end_matches(": ").to_owned());
                    }
                    return done.get("seconds").and_then(Value::as_f64).unwrap_or_default() / 60.0;
                }
            }
            // The machine said it has no such measurement (its runner started again), or can't.
            Err((StatusCode::CONFLICT, why)) => {
                r.conclusion = Some("untestable".into());
                r.why = Some(why);
                return 0.0;
            }
            Err((_, why)) => {
                if now.duration_since(heard) > UNHEARD {
                    r.conclusion = Some("untestable".into());
                    r.why = Some(format!("{} went unheard for {} minutes: {why}", hub.shown(&machine), UNHEARD.as_secs() / 60));
                    return 0.0;
                }
            }
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

/// The worktrees `e` measured in beside the agent that asked go (its runs still going stop).
async fn unmeasure(hub: &Hub, e: &Experiment) {
    if !e.runs.iter().any(|r| r.on.is_some()) {
        return;
    }
    let Some(info) = e.agent.as_ref().and_then(|a| hub.harness.info(a)) else { return };
    if let Err((_, why)) = crate::workspace::change::ask_machine(hub, &info, &crate::workspace::Ask::Unmeasure { experiment: beside_id(e) }).await {
        tracing::warn!(experiment = %e.id, error = %why, "an experiment's worktrees stayed beside its agent");
    }
}

/// Starts the run measuring `variant` (at `commit`) for criterion `c`.
async fn start_one(hub: &Arc<Hub>, e: &Experiment, repo: &Repo, variant: &str, commit: &str, c: &Criterion, round: usize) -> Run {
    let mut r = Run { variant: variant.to_owned(), round, criterion: c.name.clone(), ..Run::default() };
    let (sha, workflow) = match &c.kind {
        Kind::Pipeline { workflow } => (commit.to_owned(), workflow.clone()),
        Kind::Command { command, runs_on } => match command_commit(hub, e, repo, commit, &c.name, command, runs_on.as_deref().unwrap_or_default()).await {
            Ok(sha) => (sha, format!(".kiln/experiments/{}.yaml", e.id)),
            Err(why) => {
                r.conclusion = Some("untestable".into());
                r.why = Some(why);
                return r;
            }
        },
        Kind::Metric { .. } => return r,
    };
    let ask = super::uploads::Ask { sha, git_ref: e.branch.clone(), event: "push".into(), workflows: vec![workflow], ..super::uploads::Ask::default() };
    let actor = format!("{} (experiment {})", e.by, e.id);
    match super::uploads::start_runs(hub, repo, &ask, e.trusted, (&actor, None), false).await {
        Ok((runs, skipped)) => match runs.first().and_then(|x| x.get("id")).and_then(Value::as_str) {
            Some(id) => r.run = Some(id.to_owned()),
            None => {
                r.conclusion = Some("untestable".into());
                r.why = Some(skipped.first().and_then(|s| s.get("why")).and_then(Value::as_str).unwrap_or("its run didn't start").to_owned());
            }
        },
        Err((_, why)) => {
            r.conclusion = Some("untestable".into());
            r.why = Some(why);
        }
    }
    r
}

/// A commit on top of `commit` that runs `command` as a one-job workflow on `runs_on`, kept beside
/// the variant's ref (as an action's "Try it" commit is).
async fn command_commit(hub: &Hub, e: &Experiment, repo: &Repo, commit: &str, criterion: &str, command: &str, runs_on: &str) -> Result<String, String> {
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let dir = hub.cfg.data("tmp").join("experiments").join(&e.id).join(format!("cmd-{}", commit.get(..12).unwrap_or(commit)));
    let _ = std::fs::remove_dir_all(&dir);
    let w = dir.to_string_lossy().into_owned();
    let run = |args: Vec<String>| async move {
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        git(hub, &a).await.map_err(|err| format!("{err:#}"))
    };
    run(vec!["clone".into(), "--quiet".into(), "--no-checkout".into(), "--shared".into(), mirror.clone(), w.clone()]).await?;
    run(vec!["-C".into(), w.clone(), "checkout".into(), "--quiet".into(), "--detach".into(), commit.to_owned()]).await?;
    let file = dir.join(".kiln/experiments").join(format!("{}.yaml", e.id));
    if let Some(p) = file.parent() {
        std::fs::create_dir_all(p).map_err(|err| format!("{err:#}"))?;
    }
    let quoted = serde_json::to_string(command).unwrap_or_default();
    let label = serde_json::to_string(runs_on).unwrap_or_default();
    std::fs::write(&file, format!("on: push\njobs:\n  measure:\n    runs-on: {label}\n    steps:\n      - name: {}\n        run: {quoted}\n", serde_json::to_string(criterion).unwrap_or_default()))
        .map_err(|err| format!("{err:#}"))?;
    let me = ["-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "-C", w.as_str()].map(str::to_owned);
    run([&me[..], &["add".into(), ".kiln/experiments".into()]].concat()).await?;
    run([&me[..], &["commit".into(), "--quiet".into(), "-m".into(), format!("{} measures {criterion}", e.id)]].concat()).await?;
    let sha = run(vec!["-C".into(), w.clone(), "rev-parse".into(), "HEAD".into()]).await?;
    let name = format!("refs/kiln/experiments/{}/measure/{}", e.id, sha.get(..12).unwrap_or(&sha));
    run(vec!["-C".into(), w.clone(), "push".into(), "--quiet".into(), "--force".into(), mirror.clone(), format!("{sha}:{name}")]).await?;
    if let Some(hosted) = super::hosting::native_path(hub, repo) {
        run(vec!["-C".into(), hosted.to_string_lossy().into_owned(), "fetch".into(), "--quiet".into(), "--no-tags".into(), mirror, format!("+{name}:{name}")]).await?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(sha)
}

/// A measuring run of experiment `id`, to its end: its conclusion, the metrics it emitted (each
/// one's mean, when several legs gave it), and the runner minutes it took. Cancelled with the
/// experiment, or past its budget.
#[allow(clippy::disallowed_methods)] // (e) a picked settle before it looks again
async fn finish(hub: &Hub, id: &str, run: &str, deadline: Option<std::time::Instant>) -> (String, BTreeMap<String, f64>, f64) {
    let until = std::time::Instant::now() + ONE_RUN;
    loop {
        let rid = run.to_owned();
        let conclusion: Option<String> =
            hub.store.call(move |c| Ok(c.query_row("SELECT conclusion FROM runs WHERE id = ?1", [&rid], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await.unwrap_or(None);
        if let Some(c) = conclusion {
            let rid = run.to_owned();
            let (metrics, seconds): (BTreeMap<String, f64>, i64) = hub
                .store
                .call(move |c| {
                    let mut st = c.prepare("SELECT name, AVG(value) FROM metrics WHERE run_id = ?1 GROUP BY name")?;
                    let m = st.query_map([&rid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?.collect::<Result<BTreeMap<_, _>, _>>()?;
                    let s: i64 =
                        c.query_row("SELECT COALESCE(SUM(finished_at - started_at), 0) FROM jobs WHERE run_id = ?1 AND finished_at IS NOT NULL AND started_at IS NOT NULL", [&rid], |r| r.get(0))?;
                    Ok((m, s))
                })
                .await
                .unwrap_or_default();
            return (c, metrics, seconds as f64 / 60.0);
        }
        let now = std::time::Instant::now();
        let cancelled = hub.store.kv_get(cancel_key(id)).await.ok().flatten().is_some();
        if cancelled || now >= until || deadline.is_some_and(|d| now >= d) {
            let _ = super::runs_api::cancel(hub, run).await;
            return ("cancelled".into(), BTreeMap::new(), 0.0);
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

/// What `e`'s measurements say.
fn analyze(e: &mut Experiment) {
    let variants: Vec<Variant> = e.variants.iter().filter(|v| v.state != "conflict").map(|v| Variant { name: v.name.clone(), options: v.options.clone() }).collect();
    let mut by: BTreeMap<(String, usize), Measurement> = BTreeMap::new();
    for r in &e.runs {
        let m = by.entry((r.variant.clone(), r.round)).or_insert_with(|| Measurement { variant: r.variant.clone(), round: r.round, ..Measurement::default() });
        if let Some(c) = &r.conclusion
            && c != "untestable"
            && c != "cancelled"
        {
            m.passed.insert(r.criterion.clone(), c == "success");
        }
        for (k, v) in &r.metrics {
            m.metrics.insert(k.clone(), *v);
        }
    }
    let measurements: Vec<Measurement> = by.into_values().collect();
    let results: Results = ex::analyze(&variants, &e.criteria, &measurements, seed_of(&e.id));
    e.results = serde_json::to_value(results).ok();
}

/// Tells the agent that asked what came of it: the results, and the variant taken or asked for.
async fn tell(hub: &Arc<Hub>, e: &Experiment) {
    let Some(agent) = e.agent.clone() else { return };
    let said = e.results.as_ref().and_then(|r| r.get("why")).and_then(Value::as_str).unwrap_or_default();
    // Ended early (its budget, a machine that couldn't measure): that first.
    let why = match e.why.as_deref() {
        Some(early) => format!("{early}; {said}"),
        None => said.to_owned(),
    };
    let text = match &e.decision {
        Some(d) => format!(
            "Experiment {} is done: {why}. Continue with {}: its commit is {} (refs/kiln/experiments/{}/{}), the base with its options' changes. Its results: `experiments.get {{\"experiment\": \"{}\"}}`.",
            e.id, d.variant, d.commit, e.id, d.variant, e.id
        ),
        None => format!("Experiment {} is done: {why}. Its results are `experiments.get {{\"experiment\": \"{}\"}}`; a person (or you, if you may) chooses with `experiments.decide`.", e.id, e.id),
    };
    if hub.harness.info(&agent).is_some() {
        crate::workspace::follow::deliver(hub, &agent, text).await;
    }
}

/// `GET /api/experiments?repo=`: the experiments the caller may read, newest first.
pub async fn list(hub: &Hub, caller: &Caller, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let repo = q.get("repo").filter(|r| !r.is_empty());
    let shown: Vec<Value> = all(hub)
        .await
        .into_iter()
        .filter(|e| repo.is_none_or(|r| *r == e.repo) && may_read(caller, e))
        .map(|e| {
            json!({ "id": e.id, "repo": e.repo, "question": e.question, "state": e.state, "variants": e.variants.len(), "target": e.target, "arms": e.arms.len(), "measured": e.runs.iter().filter(|r| r.conclusion.is_some()).count(), "runs": planned(&e),
                    "recommendation": e.results.as_ref().and_then(|r| r.get("recommendation")), "decision": e.decision, "by": e.by, "created_at": e.created_at })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "experiments": shown })))
}

/// The experiment `id`, for `caller` with `cap` on its repository.
async fn mine(hub: &Hub, caller: &Caller, id: &str, cap: &str) -> Result<Experiment, Fail> {
    let e = get(hub, id).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no experiment `{id}`")))?;
    if e.target.is_some() {
        // A split is its opener's: it splits their sessions alone.
        if !caller.is_owner() && crate::harness::lineage::person_of(caller).as_deref() != Some(e.opener.as_str()) {
            return Err((StatusCode::NOT_FOUND, format!("no experiment `{id}`")));
        }
        return Ok(e);
    }
    if !caller.can(cap, &Scope::Repo { repo: e.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("{} may not do that to {id}: it needs {cap} on {}", caller.name, e.repo)));
    }
    Ok(e)
}

/// `GET /api/experiments/:id`.
pub async fn show(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    let mut e = mine(hub, caller, id, "runs.read").await?;
    if e.target.is_some() {
        read_split(hub, &mut e).await;
        e.replays = replays_of(hub, &e).await;
    }
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// `POST /api/experiments/:id/decide { variant }`: continue with it; the agent that asked is told.
pub async fn decide(hub: &Arc<Hub>, caller: &Caller, id: &str, body: &Value) -> Result<Response, Fail> {
    let mut e = mine(hub, caller, id, "experiments.run").await?;
    let name = body.get("variant").and_then(Value::as_str).ok_or_else(|| bad("say which variant (`variant`)"))?;
    if e.target.is_some() {
        if e.state == "cancelled" {
            return Err((StatusCode::CONFLICT, format!("{id} was cancelled: it promotes nothing")));
        }
        if !e.arms.iter().any(|a| a.name == name) {
            return Err(bad(format!("{id} has no variant `{name}`")));
        }
        // Promoted: every later session of its target is this variant, and the split is over.
        record_decision(hub, &mut e, Decision { variant: name.to_owned(), commit: String::new(), by: caller.name.clone(), at: crate::util::now_unix() }).await;
        hub.store.audit(caller.name.clone(), "experiments.decide".into(), format!("{id}: {name} promoted")).await;
        let e = overlay(hub, e).await;
        return Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })));
    }
    let v = e.variants.iter().find(|v| v.name == name).cloned().ok_or_else(|| bad(format!("{id} has no variant `{name}`")))?;
    let commit = v.commit.clone().ok_or_else(|| (StatusCode::CONFLICT, format!("{name} wasn't built: {}", v.why.clone().unwrap_or_default())))?;
    record_decision(hub, &mut e, Decision { variant: name.to_owned(), commit, by: caller.name.clone(), at: crate::util::now_unix() }).await;
    hub.store.audit(caller.name.clone(), "experiments.decide".into(), format!("{id}: {name}")).await;
    if e.state == "done" {
        tell(hub, &e).await;
    }
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// `POST /api/experiments/:id/cancel`: no more runs; those running are cancelled.
pub async fn cancel(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    let mut e = mine(hub, caller, id, "experiments.run").await?;
    if e.state == "cancelled" || (e.target.is_none() && matches!(e.state.as_str(), "done" | "failed")) {
        return Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })));
    }
    hub.store.kv_set(cancel_key(id), caller.name.clone()).await.map_err(internal)?;
    e.state = "cancelled".into();
    e.why = Some(format!("cancelled by {}", caller.name));
    for r in e.runs.iter().filter(|r| r.conclusion.is_none()) {
        if let Some(run) = &r.run {
            let _ = super::runs_api::cancel(hub, run).await;
        }
    }
    hub.emit(&crate::wire::events::ServerEvent::Experiment { repo: e.repo.clone(), experiment: Box::new(crate::wire::events::ExperimentBrief { id: e.id.clone(), repo: e.repo.clone(), state: e.state.clone(), measured: None, runs: None, decision: None }) });
    hub.store.audit(caller.name.clone(), "experiments.cancel".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// When the server starts: experiments it was running stopped with it, and the worktrees they
/// measured in beside their agents go once those agents' runners are back.
#[allow(clippy::disallowed_methods)] // (e) an interrupted experiment waits 120 s, picked
pub async fn interrupted(hub: &Arc<Hub>) {
    for (key, text) in hub.store.kv_prefix("ci/experiment-replay/".to_owned()).await.unwrap_or_default() {
        let Ok(mut r) = serde_json::from_str::<Replay>(&text) else { continue };
        if r.state == "running" {
            r.state = "failed".into();
            r.why = Some("the server restarted while it ran: replay it again".into());
            if let Ok(text) = serde_json::to_string(&r) {
                let _ = hub.store.kv_set(key, text).await;
            }
        }
    }
    for mut e in all(hub).await {
        if matches!(e.state.as_str(), "implementing" | "building" | "measuring") {
            e.state = "failed".into();
            e.why = Some("the server restarted while it ran: start it again".into());
            save(hub, &mut e).await;
            let h = Arc::clone(hub);
            crate::rt::spawn(async move {
                crate::rt::sleep(Duration::from_secs(120)).await;
                unmeasure(&h, &e).await;
            });
        }
    }
}

/// What an agent suggests an experiment from its question be measured by (`experiments.suggest`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Suggested {
    /// The question, as asked (none: the agent's next).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    pub criteria: Vec<Criterion>,
    pub at: i64,
}

/// An agent's suggestions kept, its newest.
const SUGGESTIONS: usize = 8;

fn suggested_key(agent: &str) -> String {
    format!("ci/experiment-suggested/{agent}")
}

async fn suggestions(hub: &Hub, agent: &str) -> Vec<Suggested> {
    hub.store.kv_get(suggested_key(agent)).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// `POST /api/experiments/suggest {question?, criteria, agent?}`: how the calling agent suggests its
/// question's options be measured, should the person run an experiment instead of answering it:
/// the commands each variant runs, the metrics they write and their thresholds. The setup of an
/// experiment opened from that question starts with them (`where_for`'s `suggested`).
pub async fn suggest(hub: &Hub, caller: &Caller, body: Value) -> Result<Response, Fail> {
    let named = body.get("agent").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty()).map(str::to_owned);
    let agent = match (&caller.agent, named) {
        (Some(me), Some(other)) if *me != other => return Err((StatusCode::FORBIDDEN, format!("{} suggests for its own questions, not {other}'s", caller.name))),
        (Some(me), _) => me.clone(),
        (None, Some(other)) => other,
        (None, None) => return Err(bad("an agent suggests for its own questions: call this as one, or say which (`agent`)")),
    };
    if hub.harness.info(&agent).is_none() {
        return Err((StatusCode::NOT_FOUND, format!("no agent `{agent}`")));
    }
    let question = body.get("question").and_then(Value::as_str).map(str::trim).filter(|q| !q.is_empty()).map(str::to_owned);
    let criteria: Vec<Criterion> = serde_json::from_value(body.get("criteria").cloned().unwrap_or_default()).map_err(|e| bad(format!("`criteria`: {e}")))?;
    if criteria.is_empty() || criteria.len() > 16 {
        return Err(bad("suggest from 1 to 16 criteria"));
    }
    if !criteria.iter().any(|c| !matches!(c.kind, Kind::Metric { .. })) {
        return Err(bad("suggest a command among them: it's what a variant runs, and what writes its metrics"));
    }
    let mut seen: Vec<&str> = Vec::new();
    for c in &criteria {
        if c.name.trim().is_empty() || seen.contains(&c.name.as_str()) {
            return Err(bad("each criterion has a name of its own"));
        }
        seen.push(&c.name);
        let empty = match &c.kind {
            Kind::Command { command, .. } => command.trim().is_empty(),
            Kind::Pipeline { workflow } => workflow.trim().is_empty(),
            Kind::Metric { metric, .. } => metric.trim().is_empty(),
        };
        let unbounded = matches!(c.threshold, Some(ex::Threshold::AtMost(x) | ex::Threshold::AtLeast(x) | ex::Threshold::Better(x)) if !x.is_finite());
        if empty || unbounded {
            return Err(bad(format!("`{}`: say what it runs or measures, and a threshold that's a number", c.name)));
        }
    }
    let made = Suggested { question: question.clone(), criteria, at: crate::util::now_unix() };
    let mut kept = suggestions(hub, &agent).await;
    kept.retain(|k| k.question != question);
    kept.insert(0, made.clone());
    kept.truncate(SUGGESTIONS);
    let text = serde_json::to_string(&kept).map_err(|e| internal(e.into()))?;
    hub.store.kv_set(suggested_key(&agent), text).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "suggested": made })))
}

/// `GET /api/experiments/where?agent=`: where an experiment from that agent's question runs: the
/// repository its folder is a checkout of, the machine its commands run on (`machine`, each
/// variant in a worktree of its own there), whether CI's engine is on (`ci`) and the workflows on
/// its default branch that can measure one then, what the agent suggested measuring by
/// (`suggested`, newest first), and whether the caller may start it.
pub async fn where_for(hub: &Hub, caller: &Caller, q: &std::collections::HashMap<String, String>) -> Result<Response, Fail> {
    let agent = q.get("agent").filter(|a| !a.is_empty()).ok_or_else(|| bad("say which agent (`agent`)"))?;
    let info = hub.harness.info(agent).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no agent `{agent}`")))?;
    let name = repo_of_agent(hub, &info).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{}'s folder isn't a checkout of a repository kiln has", info.name)))?;
    let scope = Scope::Repo { repo: name.clone() };
    if !caller.can("runs.read", &scope) {
        return Err((StatusCode::FORBIDDEN, format!("{} may not read {name}", caller.name)));
    }
    let repo = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == name).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{name}`")))?;
    let facts = super::events::facts(hub, &repo).await;
    // A pipeline is CI's to run: offered while its engine is on.
    let ci = hub.ci.enabled();
    let workflows: Vec<String> = match super::events::named_ref(hub, &repo, &facts.default_branch).await {
        Ok((_, sha)) if ci => super::load::load(hub, &repo, &sha).await.map(|c| c.workflows.into_iter().filter(|l| l.runs).map(|l| l.path).collect()).unwrap_or_default(),
        _ => Vec::new(),
    };
    let machine = crate::harness::wait::machine(&info.target).to_owned();
    let suggested = suggestions(hub, agent).await;
    Ok(srv::json(
        StatusCode::OK,
        &json!({ "repo": repo.name, "branch": facts.default_branch, "machine": machine, "ci": ci, "workflows": workflows, "suggested": suggested, "may": caller.can("experiments.run", &scope) }),
    ))
}

/// What a split splits (P7.1): one of its opener's agent definitions (`agent`), or a harness (`harness`) and, where
/// given, the Agentfile its sessions are made from (`agentfile`; left out, every session of the harness).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Target {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentfile: Option<String>,
}

/// A start as a split's target matches it: the agent definition it runs, its harness and the Agentfile it's made
/// from (kiln's own for its harness where it's isolated and names none), and whom it's for.
pub struct Starting<'a> {
    pub agentdef: Option<&'a str>,
    pub harness: &'a str,
    pub agentfile: Option<&'a str>,
    pub person: &'a str,
}

impl Target {
    fn holds(&self, s: &Starting<'_>) -> bool {
        match (&self.agent, &self.harness) {
            (Some(agent), _) => s.agentdef == Some(agent.as_str()),
            (None, Some(harness)) => s.harness == harness && self.agentfile.as_deref().is_none_or(|f| s.agentfile == Some(f)),
            (None, None) => false,
        }
    }
}

/// A microVM's size, as a variant asks it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Size {
    pub cpus: u32,
    pub memory_mib: u64,
}

/// A split's variant (P7.1): a deployment, and its weight in percent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Arm {
    pub name: String,
    pub weight: f64,
    /// The harness its sessions run: its own, or its agent's.
    pub harness: String,
    /// The agent definition it runs, and its version as the split started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The Agentfile its microVM is made from; none: kiln's own for its harness, or none on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agentfile: Option<String>,
    /// `microvm` or `host`.
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    /// Its agent's settings as the split started: what its sessions are told.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::agentdefs::Settings>,
}

/// A split's draw: the variant its session got, and what came of it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct Drawn {
    arm: String,
    at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    /// Its start was refused, why: its variant's failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
    /// Its session as last seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    seen: Option<Seen>,
}

/// A split's session as last seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct Seen {
    ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed: Option<bool>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_ms: Option<f64>,
    measures: BTreeMap<String, f64>,
}

/// What a split's start asks.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SplitAsk {
    target: Target,
    arms: Vec<ArmAsk>,
    question: Option<String>,
    agent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ArmAsk {
    name: String,
    weight: f64,
    agentfile: Option<String>,
    harness: Option<String>,
    agent: Option<String>,
    engine: Option<String>,
    size: Option<Size>,
}

fn some(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|x| !x.is_empty())
}

/// `POST /api/experiments` with `target` (P7.1): a split of the caller's sessions of `target` across `arms`, by their
/// weights, from now on.
async fn start_split(hub: &Arc<Hub>, caller: &Caller, body: Value) -> Result<Response, Fail> {
    let ask: SplitAsk = serde_json::from_value(body).map_err(|e| bad(format!("{e}")))?;
    let person = crate::harness::lineage::person_of(caller).ok_or_else(|| (StatusCode::FORBIDDEN, format!("{} acts for no person, and a split splits a person's sessions", caller.name)))?;
    let files = crate::agentfiles::Who { person: person.clone(), all: false };
    let defs = crate::agentdefs::Who { person: person.clone(), all: false };
    let target = match (some(&ask.target.agent), some(&ask.target.harness)) {
        (Some(agent), None) => {
            if ask.target.agentfile.is_some() {
                return Err(bad("an agent's target is its definition alone: its Agentfile is its definition's"));
            }
            crate::agentdefs::get(hub, &defs, agent).await.map_err(|e| bad(format!("`target`: {e}")))?;
            Target { agent: Some(agent.to_owned()), ..Target::default() }
        }
        (None, Some(harness)) => {
            let kind = crate::harness::kinds::Kind::get(harness).ok_or_else(|| bad(format!("`target`: `{harness}` is none of kiln's harnesses")))?;
            let agentfile = match some(&ask.target.agentfile) {
                Some(f) => Some(crate::harness::lineage::image_for(hub, &files, kind, f).await.map_err(|(st, why)| (st, format!("`target`: {why}")))?),
                None => None,
            };
            Target { harness: Some(harness.to_owned()), agentfile, ..Target::default() }
        }
        _ => return Err(bad("a split's `target` is one of your agents (`agent`), or a harness (`harness`) and, where you like, its Agentfile (`agentfile`)")),
    };
    if ask.arms.len() < 2 || ask.arms.len() > ex::MOST_VARIANTS {
        return Err(bad(format!("a split deploys from 2 to {} variants (`arms`)", ex::MOST_VARIANTS)));
    }
    let mut arms: Vec<Arm> = Vec::new();
    for a in &ask.arms {
        let name = a.name.trim().to_owned();
        if name.is_empty() || arms.iter().any(|x| x.name == name) {
            return Err(bad("each variant has a name of its own"));
        }
        let named = |what: String| bad(format!("variant `{name}`: {what}"));
        if !(a.weight.is_finite() && a.weight > 0.0) {
            return Err(named("its `weight` is a percentage above 0".into()));
        }
        let engine = match a.engine.as_deref().map(str::trim) {
            None | Some("" | "microvm") => "microvm",
            Some("host") => "host",
            Some(other) => return Err(named(format!("`{other}` is no engine: microvm or host"))),
        };
        let mut arm = Arm { name: name.clone(), weight: a.weight, engine: engine.into(), size: a.size.filter(|s| s.cpus > 0 || s.memory_mib > 0), ..Arm::default() };
        let mut agentfile = some(&a.agentfile).map(str::to_owned);
        match (some(&a.harness), some(&a.agent)) {
            (Some(h), None) => arm.harness = h.to_owned(),
            (None, Some(agent)) => {
                let d = crate::agentdefs::get(hub, &defs, agent).await.map_err(|e| named(format!("{e}")))?;
                arm.harness.clone_from(&d.settings.harness);
                arm.model.clone_from(&d.settings.model);
                arm.agent = Some(d.id.clone());
                arm.agent_version = Some(d.version);
                agentfile = agentfile.or_else(|| d.settings.agentfile.clone());
                arm.settings = Some(d.settings);
            }
            _ => return Err(named("its member is a `harness` or one of your `agent`s, one of them".into())),
        }
        let kind = crate::harness::kinds::Kind::get(&arm.harness).ok_or_else(|| named(format!("`{}` is none of kiln's harnesses", arm.harness)))?;
        match engine {
            "host" => {
                if agentfile.is_some() || arm.size.is_some() {
                    return Err(named("an Agentfile and a size are a microVM's: the host engine runs its harness as a process of its machine".into()));
                }
            }
            _ => {
                arm.agentfile = match agentfile {
                    Some(f) => Some(crate::harness::lineage::image_for(hub, &files, kind, &f).await.map_err(|(st, why)| (st, format!("variant `{name}`: {why}")))?),
                    None if crate::agentfiles::builtin::ALL.iter().any(|b| b.harness == kind.id) => None,
                    None => return Err(named(format!("kiln ships no Agentfile for {}: name yours (`agentfile`)", kind.name))),
                };
            }
        }
        arms.push(arm);
    }
    let total: f64 = arms.iter().map(|a| a.weight).sum();
    if (total - 100.0).abs() > 1e-9 * 100.0 {
        return Err(bad(format!("a split's weights are percentages that add up to 100, not {total}")));
    }
    if let Some(other) = all(hub).await.into_iter().find(|o| o.opener == person && o.target.as_ref() == Some(&target) && o.state != "cancelled") {
        return Err((StatusCode::CONFLICT, format!("{} splits that target already ({}): cancel it first", other.id, other.state)));
    }
    let id = next_id(hub).await.map_err(internal)?;
    let mut e = Experiment {
        id: id.clone(),
        question: ask.question.clone(),
        agent: ask.agent.clone().or_else(|| caller.agent.clone()),
        opener: person,
        target: Some(target),
        arms,
        decide: "ask".into(),
        state: "running".into(),
        trusted: caller.is_owner(),
        by: caller.name.clone(),
        created_at: crate::util::now_unix(),
        ..Experiment::default()
    };
    save(hub, &mut e).await;
    hub.store.audit(caller.name.clone(), "experiments.start".into(), format!("{id}: a split of {} variants, {}", e.arms.len(), e.arms.iter().map(|a| format!("{} {}%", a.name, a.weight)).collect::<Vec<_>>().join(", "))).await;
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "experiment": e })))
}

/// A session's variant, as [`assign`] gave it: the split's `n`-th draw (none for a promoted variant).
pub struct Draw {
    pub assigned: crate::harness::session::Assigned,
    pub arm: Arm,
    n: Option<u64>,
}

/// The next of split `id`'s draws: `0`, `1`, ….
async fn next_draw(hub: &Hub, id: &str) -> anyhow::Result<u64> {
    let key = format!("ci/experiment-draws/{id}");
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n: u64 = tx.query_row("SELECT v FROM kv WHERE k = ?1", [&key], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()).unwrap_or(0);
            tx.execute("INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v", [key.clone(), (n + 1).to_string()])?;
            tx.commit()?;
            Ok(n)
        })
        .await
}

/// The variant a new session starting as `s` is (P7.1): while a split of its opener's holds its target, its draw by
/// the variants' weights, kept as the split's next; once the split promoted one, that one, with no draw; none while
/// none holds it, it's paused, or it was cancelled.
pub async fn assign(hub: &Hub, s: &Starting<'_>) -> Option<Draw> {
    let e = all(hub).await.into_iter().find(|e| e.opener == s.person && e.target.as_ref().is_some_and(|t| t.holds(s)) && matches!(e.state.as_str(), "running" | "done"))?;
    if let Some(d) = &e.decision {
        let arm = e.arms.iter().find(|a| a.name == d.variant)?.clone();
        return Some(Draw { assigned: crate::harness::session::Assigned { experiment: e.id.clone(), arm: arm.name.clone(), promoted: true }, arm, n: None });
    }
    let weights: Vec<f64> = e.arms.iter().map(|a| a.weight).collect();
    let n = match next_draw(hub, &e.id).await {
        Ok(n) => n,
        Err(err) => {
            tracing::warn!(experiment = %e.id, error = %format!("{err:#}"), "a split's draw wasn't kept: the session starts as asked");
            return None;
        }
    };
    let arm = e.arms.get(split::draw(&weights, seed_of(&e.id), n)?)?.clone();
    let drawn = Drawn { arm: arm.name.clone(), at: crate::util::now_ms(), ..Drawn::default() };
    let text = serde_json::to_string(&drawn).ok()?;
    hub.store.kv_set(drawn_key(&e.id, n), text).await.ok()?;
    Some(Draw { assigned: crate::harness::session::Assigned { experiment: e.id.clone(), arm: arm.name.clone(), promoted: false }, arm, n: Some(n) })
}

/// What draw `d`'s start came to: its session, or why it was refused (its variant's failure).
pub async fn started(hub: &Hub, d: &Draw, came: Result<String, String>) {
    let Some(n) = d.n else { return };
    let key = drawn_key(&d.assigned.experiment, n);
    let mut drawn: Drawn = hub.store.kv_get(key.clone()).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| Drawn { arm: d.arm.name.clone(), ..Drawn::default() });
    match came {
        Ok(session) => drawn.session = Some(session),
        Err(why) => drawn.refused = Some(why),
    }
    if let Ok(text) = serde_json::to_string(&drawn) {
        let _ = hub.store.kv_set(key, text).await;
    }
}

/// A split's session as its info says it (`Info::starts`' measures, P7.1).
fn seen_of(i: &crate::harness::session::Info) -> Seen {
    let ended = i.status == crate::harness::model::Status::Exited;
    let errored = i.exit.as_ref().is_some_and(|x| x.error.is_some());
    let ran = !i.starts.is_empty();
    let outcome = match (ended, errored || !ran) {
        (false, _) => "running",
        (true, true) => "failed",
        (true, false) => "ended",
    };
    let failed = if ended { Some(errored || !ran) } else if ran { Some(false) } else { None };
    let first = i.starts.iter().find_map(|s| s.measured.as_ref());
    let mut measures = BTreeMap::new();
    if let Some(peak) = i.starts.iter().filter_map(|s| s.measured.as_ref()?.footprint_peak_bytes).max() {
        measures.insert("footprint_peak_bytes".to_owned(), peak as f64);
    }
    if let Some(steady) = i.starts.iter().rev().find_map(|s| s.measured.as_ref()?.footprint_steady_bytes) {
        measures.insert("footprint_steady_bytes".to_owned(), steady as f64);
    }
    let cpu: Vec<u64> = i.starts.iter().filter_map(|s| s.measured.as_ref()?.cpu_ns).collect();
    if !cpu.is_empty() {
        measures.insert("cpu_ms".to_owned(), cpu.iter().map(|n| *n as f64).sum::<f64>() / 1e6);
    }
    Seen {
        ended,
        failed,
        outcome: outcome.into(),
        start_kind: first.and_then(|m| m.started_from.as_deref()).and_then(split::kind_of).map(str::to_owned),
        start_ms: first.and_then(|m| m.start_us).map(|us| us as f64 / 1000.0),
        measures,
    }
}

/// `e`'s results, a split's: each draw's session read as it stands now (one that ended kept as it ended), then what
/// they say (`kiln_ci::split::analyze`).
async fn read_split(hub: &Hub, e: &mut Experiment) {
    let mut sessions = Vec::new();
    for (key, text) in hub.store.kv_prefix(format!("ci/experiment-drawn/{}/", e.id)).await.unwrap_or_default() {
        let Ok(mut drawn) = serde_json::from_str::<Drawn>(&text) else { continue };
        let seen = match (&drawn.refused, &drawn.session) {
            (Some(_), _) => Some(Seen { ended: true, failed: Some(true), outcome: "refused".into(), ..Seen::default() }),
            (None, Some(session)) => match (&drawn.seen, hub.harness.info(session)) {
                (Some(last), _) if last.ended => Some(last.clone()),
                (_, Some(info)) => {
                    let now = seen_of(&info);
                    if drawn.seen.as_ref() != Some(&now) {
                        drawn.seen = Some(now.clone());
                        if let Ok(t) = serde_json::to_string(&drawn) {
                            let _ = hub.store.kv_set(key.clone(), t).await;
                        }
                    }
                    Some(now)
                }
                (last, None) => last.clone().or(Some(Seen { outcome: "gone".into(), ..Seen::default() })),
            },
            (None, None) => Some(Seen { outcome: "starting".into(), ..Seen::default() }),
        };
        let seen = seen.unwrap_or_default();
        sessions.push(split::Session {
            arm: drawn.arm.clone(),
            failed: seen.failed,
            outcome: seen.outcome.clone(),
            start: match (split::KINDS.iter().copied().find(|k| seen.start_kind.as_deref() == Some(*k)), seen.start_ms) {
                (Some(k), Some(ms)) => Some((k, ms)),
                _ => None,
            },
            measures: seen.measures.clone(),
        });
    }
    let names: Vec<String> = e.arms.iter().map(|a| a.name.clone()).collect();
    e.results = serde_json::to_value(split::analyze(&names, &sessions)).ok();
}

/// A replay of one of a split's opener's recorded sessions against its microVM variants (P7.1b; module docs).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Replay {
    /// Its number within its split: `1`, `2`, ….
    pub n: u64,
    pub session: String,
    /// The runner its runs run on, one at a time.
    pub on: String,
    pub rounds: usize,
    /// The variants it replays in, in the split's order: each that runs in a microVM.
    pub arms: Vec<String>,
    /// The variants it doesn't, and why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub not_replayed: BTreeMap<String, String>,
    /// Of its session's recorded calls (`calls`), how many each run replays (`steps`), and why the rest aren't.
    pub calls: usize,
    pub steps: usize,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub skipped: BTreeMap<kiln_ci::replay::Skip, usize>,
    /// `running`, `done`, `cancelled`, or `failed` (`why`).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub by: String,
    pub at: i64,
    /// Its runs as they ended, and what they say of each variant as a split's sessions say it; laid over it as it's
    /// shown.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub runs: Vec<Replayed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Value>,
}

/// A run of a replay: its variant and round, and what it measured.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Replayed {
    pub arm: String,
    pub round: usize,
    #[serde(flatten)]
    pub answer: crate::sandbox::replay::Answer,
}

/// A tool call as a replay reads its record.
fn recorded_of(t: &crate::harness::model::ToolCall) -> kiln_ci::replay::Recorded {
    use crate::harness::model::{ToolKind as T, ToolStatus as S};
    use kiln_ci::replay::{Kind, Ran};
    let kind = match t.tool {
        T::Shell => Kind::Shell,
        T::Edit => Kind::Edit,
        T::Read => Kind::Read,
        T::Search => Kind::Search,
        T::Web => Kind::Web,
        T::Agent => Kind::Agent,
        T::Skill => Kind::Skill,
        T::Todo => Kind::Todo,
        T::Question => Kind::Question,
        T::Plan => Kind::Plan,
        T::Mcp => Kind::Mcp,
        T::Other => Kind::Other,
    };
    let ran = match t.status {
        S::Pending => Ran::Pending,
        S::Running => Ran::Running,
        S::Done => Ran::Done,
        S::Error => Ran::Error,
        S::Denied => Ran::Denied,
        S::Cancelled => Ran::Cancelled,
    };
    let ms = t.ended.and_then(|end| u64::try_from(end.saturating_sub(t.started)).ok());
    kiln_ci::replay::Recorded { kind, name: t.name.clone(), input: t.input.clone(), ran, exit_code: t.exit_code, ms, shortened: crate::harness::model::json_clipped(&t.input) }
}

/// Session `session`'s recorded tool calls in their order, as kiln's conversation store keeps its events, a page at a
/// time (each event as its newest revision says it).
async fn recorded(hub: &Hub, session: &str) -> Result<Vec<kiln_ci::replay::Recorded>, Fail> {
    let store = hub.harness.convo.clone().ok_or_else(|| (StatusCode::NOT_FOUND, "this server keeps no conversations (KILN_CONVERSATIONS is off), so no session's calls to replay".to_owned()))?;
    let s = session.to_owned();
    let rows = crate::rt::blocking(move || -> anyhow::Result<Vec<(String, String)>> {
        let page = crate::harness::copies::EVENTS_PAGE;
        let (mut pages, mut before) = (Vec::new(), None);
        loop {
            let rows = store.events_committed(&s, before, page)?;
            let (full, first) = (rows.len() >= page, rows.first().map(|r| r.seq));
            pages.push(rows.into_iter().filter(|r| r.kind == "tool").map(|r| (r.id, r.body)).collect::<Vec<_>>());
            match first {
                Some(seq) if full => before = Some(seq),
                _ => break,
            }
        }
        Ok(pages.into_iter().rev().flatten().collect())
    })
    .await
    .and_then(|r| r)
    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("{session}'s calls weren't read: {e:#}")))?;
    let mut calls: Vec<kiln_ci::replay::Recorded> = Vec::new();
    let mut at: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (id, body) in rows {
        let Ok(event) = serde_json::from_str::<crate::harness::model::Event>(&body) else { continue };
        let crate::harness::model::Body::Tool(t) = event.body else { continue };
        let call = recorded_of(&t);
        match at.get(&id).copied() {
            Some(k) => {
                if let Some(kept) = calls.get_mut(k) {
                    *kept = call;
                }
            }
            None => {
                at.insert(id, calls.len());
                calls.push(call);
            }
        }
    }
    Ok(calls)
}

/// The most a replay's ask may be: one frame of the interactive lane it's sent on, which refuses a larger one.
fn ask_most() -> usize {
    usize::try_from(<crate::net::Kiln as hyper_transport::Classes>::frame_bound(crate::net::Class::Interactive)).unwrap_or(usize::MAX)
}

/// What run `round` of replay `r` of split `e` asks of its runner: `plan`'s steps in variant `a`'s microVM, from
/// `folder`, its image's text and build secrets `kept` where it's a person's own.
fn replay_ask(e: &Experiment, r: &Replay, a: &Arm, kept: &crate::harness::lineage::Kept, round: usize, folder: &str, plan: &kiln_ci::replay::Plan) -> crate::sandbox::replay::Ask {
    crate::sandbox::replay::Ask {
        experiment: e.id.clone(),
        replay: r.n,
        arm: a.name.clone(),
        round,
        harness: a.harness.clone(),
        agentfile: a.agentfile.clone(),
        text: kept.text.clone(),
        secrets: kept.secrets.clone(),
        size: a.size.map(|s| (s.cpus, s.memory_mib)),
        folder: folder.to_owned(),
        plan: plan.clone(),
        limit_s: ONE_RUN.as_secs(),
    }
}

/// Keeps replay `r` of split `id`.
async fn keep_replay(hub: &Hub, id: &str, r: &Replay) {
    let kept = Replay { runs: Vec::new(), results: None, ..r.clone() };
    if let Ok(text) = serde_json::to_string(&kept)
        && let Err(err) = hub.store.kv_set(replay_key(id, r.n), text).await
    {
        tracing::warn!(experiment = %id, replay = r.n, error = %format!("{err:#}"), "a replay couldn't be kept");
    }
}

/// `POST /api/experiments/:id/replay {session, rounds?, on?}` (P7.1b): split `id`'s microVM variants each replay the
/// caller's recorded session `session` `rounds` times (3), interleaved, one run at a time on runner `on` (the
/// session's own machine where it's left out), from now on.
pub async fn replay(hub: &Arc<Hub>, caller: &Caller, id: &str, body: &Value) -> Result<Response, Fail> {
    let e = mine(hub, caller, id, "experiments.run").await?;
    if e.target.is_none() {
        return Err(bad(format!("{id} measures options: only a split's variants replay a session")));
    }
    if e.state == "cancelled" {
        return Err((StatusCode::CONFLICT, format!("{id} was cancelled: it replays nothing")));
    }
    let session = body.get("session").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| bad("say which of your sessions it replays (`session`)"))?;
    let rounds = match body.get("rounds") {
        None | Some(Value::Null) => REPETITIONS,
        Some(v) => v.as_u64().and_then(|n| usize::try_from(n).ok()).filter(|n| (1..=MOST_REPETITIONS).contains(n)).ok_or_else(|| bad(format!("`rounds` is from 1 to {MOST_REPETITIONS}")))?,
    };
    crate::harness::lineage::may_see(hub, caller, session).await?;
    let info = hub.harness.info(session).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no session `{session}`")))?;
    let plan = kiln_ci::replay::plan(&recorded(hub, session).await?);
    if plan.steps.is_empty() {
        let why: Vec<String> = plan.skipped.iter().map(|(k, n)| format!("{n} {}", serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default())).collect();
        return Err((StatusCode::CONFLICT, format!("{session} has no call a replay runs: of its {} calls, none is a command or a whole file written ({})", plan.calls, why.join(", "))));
    }
    let (mut runs, mut not_replayed) = (Vec::new(), BTreeMap::new());
    for a in &e.arms {
        if a.engine != "microvm" {
            not_replayed.insert(a.name.clone(), "it runs on its machine, in no microVM: a replay runs in its variant's microVM, as a session's commands run on the machine would change it".to_owned());
            continue;
        }
        let isolation = crate::harness::session::Isolation { image: a.agentfile.clone(), ..crate::harness::session::Isolation::microvm() };
        let kept = crate::harness::lineage::kept_agentfile(hub, Some(&isolation), Some(e.opener.as_str())).await.map_err(|why| (StatusCode::CONFLICT, format!("variant `{}`: {why}", a.name)))?;
        runs.push((a.clone(), kept));
    }
    if runs.is_empty() {
        return Err((StatusCode::CONFLICT, format!("none of {id}'s variants runs in a microVM, where a replay runs")));
    }
    let on = match body.get("on").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => m.to_owned(),
        None => crate::harness::wait::machine(&info.target).to_owned(),
    };
    let speaks = {
        let inst = hub.instances.load();
        let Some(newest) = inst.newest(&on) else { return Err((StatusCode::CONFLICT, format!("{} is not connected", hub.shown(&on)))) };
        inst.get(newest).is_some_and(|i| crate::proto::feature::speaks(&i.features, crate::proto::feature::REPLAYS))
    };
    if !speaks {
        return Err((StatusCode::CONFLICT, format!("{}'s kiln replays no session: upgrade it (System → Updates)", hub.shown(&on))));
    }
    let replays = replays_of(hub, &e).await;
    if let Some(running) = replays.iter().find(|r| r.state == "running") {
        return Err((StatusCode::CONFLICT, format!("{id}'s replay {} runs: one at a time, as replays side by side would measure each other", running.n)));
    }
    let n = replays.iter().map(|r| r.n).max().unwrap_or(0).saturating_add(1);
    let r = Replay {
        n,
        session: session.to_owned(),
        on,
        rounds,
        arms: runs.iter().map(|(a, _)| a.name.clone()).collect(),
        not_replayed,
        calls: plan.calls,
        steps: plan.steps.len(),
        skipped: plan.skipped.clone(),
        state: "running".into(),
        by: caller.name.clone(),
        at: crate::util::now_unix(),
        ..Replay::default()
    };
    let folder = info.cwd.clone();
    for (a, kept) in &runs {
        let json = serde_json::to_string(&replay_ask(&e, &r, a, kept, 0, &folder, &plan)).map_err(|err| internal(err.into()))?;
        let frame = crate::proto::encode(&crate::proto::ServerMsg::Replay { req: u64::MAX, json }).map_err(internal)?.len();
        if frame > ask_most() {
            return Err((StatusCode::CONFLICT, format!("replaying {session} in `{}` takes {frame} bytes to ask, past the {} one ask carries (its lane's frame)", a.name, ask_most())));
        }
    }
    keep_replay(hub, id, &r).await;
    hub.store.audit(caller.name.clone(), "experiments.replay".into(), format!("{id}: {session} replayed in {}, {rounds} rounds", r.arms.join(", "))).await;
    let h = Arc::clone(hub);
    let answer = r.clone();
    drop(crate::rt::spawn(async move { drive_replay(h, e, r, runs, folder, plan).await }));
    Ok(srv::json(StatusCode::ACCEPTED, &json!({ "replay": answer })))
}

/// One run asked of runner `on`, waited for as long as the runner gives it and then as long as a machine measuring may
/// go unheard: what it measured, or why it couldn't (a failed run, counted).
async fn ask_replay(hub: &Hub, on: &str, ask: &crate::sandbox::replay::Ask) -> crate::sandbox::replay::Answer {
    use crate::sandbox::replay::Answer;
    let failed = |why: String| Answer { conclusion: "failure".into(), why: Some(why), ..Answer::default() };
    let json = match serde_json::to_string(ask) {
        Ok(json) => json,
        Err(e) => return failed(format!("its ask wasn't written: {e}")),
    };
    match hub.ask_speaking::<Answer>(on, crate::proto::feature::REPLAYS, ONE_RUN.saturating_add(UNHEARD), move |req| crate::proto::ServerMsg::Replay { req, json }).await {
        Ok(Some(Some(a))) => a,
        Ok(Some(None)) => failed(format!("{} answered nothing", hub.shown(on))),
        Ok(None) => failed(format!("{}'s kiln replays no session: upgrade it (System → Updates)", hub.shown(on))),
        Err(e) => failed(format!("{e:#}")),
    }
}

/// Runs replay `r` of split `e` (module docs): each round, each of its variants once, in a shuffled order, one run at a
/// time on its runner, each run kept as it ends; then done, or cancelled where its split was meanwhile.
async fn drive_replay(hub: Arc<Hub>, e: Experiment, mut r: Replay, runs: Vec<(Arm, crate::harness::lineage::Kept)>, folder: String, plan: kiln_ci::replay::Plan) {
    let mut k = 0usize;
    let seed = seed_of(&format!("{}/{}", e.id, r.n));
    'rounds: for round in 0..r.rounds {
        let mut order: Vec<usize> = (0..runs.len()).collect();
        shuffle(&mut order, seed ^ (round as u64).wrapping_mul(0x9E37_79B9));
        for i in order {
            if hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten().is_some() {
                r.state = "cancelled".into();
                r.why = Some("its split was cancelled".into());
                break 'rounds;
            }
            let Some((arm, kept)) = runs.get(i) else { continue };
            let answer = ask_replay(&hub, &r.on, &replay_ask(&e, &r, arm, kept, round, &folder, &plan)).await;
            if let Ok(text) = serde_json::to_string(&Replayed { arm: arm.name.clone(), round, answer }) {
                let _ = hub.store.kv_set(replayed_key(&e.id, r.n, k), text).await;
            }
            k = k.saturating_add(1);
        }
    }
    if r.state == "running" {
        r.state = "done".into();
    }
    keep_replay(&hub, &e.id, &r).await;
}

/// A replay's run as a split's session (P7.1b): failed where it couldn't be carried out (its microVM refused, its run
/// ended before its steps did, its limit passed); `diverged` where it ran to its end and a step ended otherwise than
/// its record, how many such steps a measure of its own; its start within its kind; its footprint and host CPU as a
/// session's (`seen_of`).
fn session_of_run(r: &Replayed) -> split::Session {
    let a = &r.answer;
    let finished = a.conclusion == "success";
    let diverged = a.outcome.map_or(0, |o| o.diverged);
    let outcome = match (finished, diverged) {
        (false, _) => "failed",
        (true, 0) => "ended",
        (true, _) => "diverged",
    };
    let m = a.measured.clone().unwrap_or_default();
    let mut measures = BTreeMap::new();
    if let Some(peak) = m.footprint_peak_bytes {
        measures.insert("footprint_peak_bytes".to_owned(), peak as f64);
    }
    if let Some(steady) = m.footprint_steady_bytes {
        measures.insert("footprint_steady_bytes".to_owned(), steady as f64);
    }
    if let Some(cpu) = m.cpu_ns {
        measures.insert("cpu_ms".to_owned(), cpu as f64 / 1e6);
    }
    if finished {
        measures.insert("diverged_steps".to_owned(), diverged as f64);
    }
    let start = m.started_from.as_deref().and_then(split::kind_of).zip(m.start_us.map(|us| us as f64 / 1000.0));
    split::Session { arm: r.arm.clone(), failed: Some(!finished), outcome: outcome.into(), start, measures }
}

/// Split `e`'s replays, oldest first, each with its runs as they ended and what they say of each of its variants.
async fn replays_of(hub: &Hub, e: &Experiment) -> Vec<Replay> {
    let mut out = Vec::new();
    for (_, text) in hub.store.kv_prefix(format!("ci/experiment-replay/{}/", e.id)).await.unwrap_or_default() {
        let Ok(mut r) = serde_json::from_str::<Replay>(&text) else { continue };
        let prefix = format!("ci/experiment-replayed/{}/{:06}/", e.id, r.n);
        r.runs = hub.store.kv_prefix(prefix).await.unwrap_or_default().into_iter().filter_map(|(_, t)| serde_json::from_str::<Replayed>(&t).ok()).collect();
        let sessions: Vec<split::Session> = r.runs.iter().map(session_of_run).collect();
        r.results = serde_json::to_value(split::analyze(&r.arms, &sessions)).ok();
        out.push(r);
    }
    out
}

/// `POST /api/experiments/:id/pause` (`on`) and `/resume`: a split's draws stop, and start again.
pub async fn pause(hub: &Hub, caller: &Caller, id: &str, on: bool) -> Result<Response, Fail> {
    let e = mine(hub, caller, id, "experiments.run").await?;
    if e.target.is_none() {
        return Err(bad(format!("{id} measures options: only a split pauses")));
    }
    if matches!(e.state.as_str(), "done" | "cancelled") {
        return Err((StatusCode::CONFLICT, format!("{id} is {}: it draws no more", e.state)));
    }
    if on {
        hub.store.kv_set(paused_key(id), caller.name.clone()).await.map_err(internal)?;
    } else {
        hub.store.kv_delete(paused_key(id)).await.map_err(internal)?;
    }
    hub.store.audit(caller.name.clone(), if on { "experiments.pause" } else { "experiments.resume" }.into(), id.to_owned()).await;
    let e = overlay(hub, e).await;
    hub.emit(&crate::wire::events::ServerEvent::Experiment { repo: e.repo.clone(), experiment: Box::new(crate::wire::events::ExperimentBrief { id: e.id.clone(), repo: e.repo.clone(), state: e.state.clone(), measured: None, runs: None, decision: None }) });
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// Whether `caller` may read `e`: a split its opener (and the server's owner); one of options whoever reads its
/// repository's runs.
fn may_read(caller: &Caller, e: &Experiment) -> bool {
    if e.target.is_some() {
        return caller.is_owner() || crate::harness::lineage::person_of(caller).as_deref() == Some(e.opener.as_str());
    }
    caller.can("runs.read", &Scope::Repo { repo: e.repo.clone() })
}

/// `/api/experiments[/:id[/decide|/cancel|/pause|/resume|/replay]]`, `/where` and `/suggest`.
pub async fn route(hub: &Arc<Hub>, caller: &Caller, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: Value) -> Result<Response, Fail> {
    use http::Method;
    match (method, rest) {
        (&Method::GET, []) => list(hub, caller, q).await,
        (&Method::GET, ["where"]) => where_for(hub, caller, q).await,
        (&Method::POST, []) => start(hub, caller, body).await,
        (&Method::POST, ["suggest"]) => suggest(hub, caller, body).await,
        (&Method::GET, [id]) => show(hub, caller, id).await,
        (&Method::POST, [id, "decide"]) => decide(hub, caller, id, &body).await,
        (&Method::POST, [id, "cancel"]) => cancel(hub, caller, id).await,
        (&Method::POST, [id, "pause"]) => pause(hub, caller, id, true).await,
        (&Method::POST, [id, "resume"]) => pause(hub, caller, id, false).await,
        (&Method::POST, [id, "replay"]) => replay(hub, caller, id, &body).await,
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w20_r11_a_time_limit_is_said_in_its_largest_whole_unit() {
        assert_eq!(span(90), "90 seconds");
        assert_eq!(span(60), "1 minute");
        assert_eq!(span(7_200), "2 hours");
        assert_eq!(span(3 * 86_400), "3 days");
        assert_eq!(span(31_536_000), "365 days");
        let e = Experiment { budget_minutes: Some(5), ..Experiment::default() };
        assert_eq!(limit(&e), Some(300), "a limit from before seconds");
        let e = Experiment { budget_minutes: Some(5), budget_seconds: Some(45), ..Experiment::default() };
        assert_eq!(limit(&e), Some(45), "seconds when it has them");
    }

    #[test]
    fn w20_r11_an_agents_diff_is_the_last_it_names() {
        assert_eq!(named_diff("Built it as D12, then fixed a test.\nD13"), Some("D13"));
        assert_eq!(named_diff("Done: `D7`."), Some("D7"));
        assert_eq!(named_diff("No diff: Docker isn't here, and DX isn't one"), None);
    }

    #[test]
    fn w20_r11_an_options_agent_is_told_how_to_build_it() {
        let e = Experiment { id: "E3".into(), repo: "acme/app".into(), base: "b".repeat(40), branch: "main".into(), question: Some("Which cache?".into()), ..Experiment::default() };
        let o = OptionDef { name: "redis".into(), implement: Some("Cache the parsed config in Redis".into()), ..OptionDef::default() };
        let told = brief(&e, &o);
        for part in ["`redis`: Cache the parsed config in Redis", "git worktree add ../app-E3-redis", &"b".repeat(40), "ci_diffs_submit_patch", "Don't land it", ": Which cache?"] {
            assert!(told.contains(part), "{part}: {told}");
        }
    }

    /// P7.1b ("with the same measures"): a replay's run reads as a split's session does, under a session's measure names
    /// (`seen_of`'s), its start within its kind; one that couldn't be carried out failed; one whose step ended otherwise
    /// than recorded diverged, how many such steps a measure of its own; and each variant's runs are read by the split's
    /// analysis.
    #[test]
    fn p7_1b_a_replays_runs_read_as_a_splits_sessions() {
        use crate::harness::session::Measured;
        use crate::sandbox::replay::Answer;
        let run = |arm: &str, diverged: usize, from: &str| Replayed {
            arm: arm.into(),
            round: 0,
            answer: Answer {
                conclusion: "success".into(),
                measured: Some(Measured { start_us: Some(250_000), started_from: Some(from.into()), cpu_ns: Some(3_000_000), window_ns: Some(1), footprint_steady_bytes: Some(64 << 20), footprint_peak_bytes: Some(96 << 20), built_us: None }),
                outcome: Some(kiln_ci::replay::Outcome { steps: 4, ended: 4, diverged }),
                ..Answer::default()
            },
        };
        let ended = session_of_run(&run("a", 0, "saved"));
        assert_eq!((ended.failed, ended.outcome.as_str(), ended.start), (Some(false), "ended", Some(("cold", 250.0))));
        let names: Vec<&str> = ended.measures.keys().map(String::as_str).collect();
        assert_eq!(names, ["cpu_ms", "diverged_steps", "footprint_peak_bytes", "footprint_steady_bytes"]);
        assert_eq!(ended.measures.get("cpu_ms"), Some(&3.0));
        let diverged = session_of_run(&run("a", 2, "restored"));
        assert_eq!((diverged.outcome.as_str(), diverged.measures.get("diverged_steps"), diverged.start.map(|s| s.0)), ("diverged", Some(&2.0), Some("template")));
        let failed = session_of_run(&Replayed { answer: Answer { conclusion: "failure".into(), why: Some("its microVM didn't start".into()), ..Answer::default() }, ..run("b", 0, "warm") });
        assert_eq!((failed.failed, failed.outcome.as_str(), failed.start, failed.measures.len()), (Some(true), "failed", None, 0));
        let res = split::analyze(&["a".to_owned(), "b".to_owned()], &[ended, diverged, failed]);
        assert!(res.arms.iter().any(|a| a.name == "a" && a.assigned == 2 && a.failures.failed == 0 && a.measures.contains_key("footprint_peak_bytes") && a.start_ms.contains_key("cold")), "{res:?}");
        assert!(res.arms.iter().any(|a| a.name == "b" && a.failures.failed == 1 && a.outcomes.get("failed") == Some(&1)), "{res:?}");
    }

    #[test]
    fn w20_r11_rounds_are_shuffled_the_same_way_again() {
        let mut a: Vec<u32> = (0..12).collect();
        let mut b = a.clone();
        shuffle(&mut a, seed_of("E1"));
        shuffle(&mut b, seed_of("E1"));
        assert_eq!(a, b, "the same experiment, the same order");
        let mut c: Vec<u32> = (0..12).collect();
        shuffle(&mut c, seed_of("E1") ^ 1);
        assert_ne!(a, c, "another round, another order");
        a.sort_unstable();
        assert_eq!(a, (0..12).collect::<Vec<u32>>(), "each once");
    }
}
