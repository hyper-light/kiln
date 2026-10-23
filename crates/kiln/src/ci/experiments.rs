//! Experiments (docs/ci.md W20-R11, after docs/experiments.md): options measured against each other
//! and together, on a base commit.
//!
//! - Each option is a change: a commit, a branch, or a diff (its latest version), applied as the
//!   commits it adds.
//! - The strategy makes the variants (`kiln_ci::experiments`). Each is its options' changes
//!   cherry-picked onto the base in the options' order, kept as
//!   `refs/kiln/experiments/<id>/<variant>`; one whose options conflict is said to, and isn't
//!   measured.
//! - Measuring is ordinary CI: each variant's runs of the criteria's workflows (a command's is a
//!   one-job workflow on a commit made for it), on routed runners. Repetitions are interleaved:
//!   each round runs every variant once, in a shuffled order, `max_parallel` at once, within the
//!   time budget.
//! - The results (`kiln_ci::experiments::analyze`): each variant's required checks, its metrics
//!   against the baseline's with intervals, each option's main effect and each pair's
//!   interaction, and a recommendation. Then decided: brought back (the default), or the clear
//!   winner taken (`decide: auto`), and the agent that asked told either way.

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
    /// Its conclusion once it ended; `untestable` when it didn't start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub metrics: BTreeMap<String, f64>,
}

/// Who chose a variant, and when.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    pub strategy: Option<Strategy>,
    pub baseline: bool,
    pub criteria: Vec<Criterion>,
    pub repetitions: usize,
    pub max_parallel: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_minutes: Option<u64>,
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
/// How long one measuring run may take.
const ONE_RUN: Duration = Duration::from_secs(3 * 3600);

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
    hub.emit(&json!({ "type": "experiment", "repo": e.repo, "experiment": { "id": e.id, "repo": e.repo, "state": e.state, "measured": e.runs.iter().filter(|r| r.conclusion.is_some()).count(), "runs": planned(e) } }));
}

/// The runs an experiment plans: each buildable variant, each pass/fail criterion, each round.
fn planned(e: &Experiment) -> usize {
    let measuring = e.criteria.iter().filter(|c| !matches!(c.kind, Kind::Metric { .. })).count();
    e.variants.iter().filter(|v| v.state != "conflict").count() * measuring * e.repetitions
}

/// `e` with its cancellation and decision laid over it.
async fn overlay(hub: &Hub, mut e: Experiment) -> Experiment {
    if let Some(by) = hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten()
        && !matches!(e.state.as_str(), "done" | "failed")
    {
        e.state = "cancelled".into();
        e.why = Some(format!("cancelled by {by}"));
    }
    if let Some(d) = hub.store.kv_get(decision_key(&e.id)).await.ok().flatten().and_then(|t| serde_json::from_str::<Decision>(&t).ok()) {
        e.decision = Some(d);
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
    hub.emit(&json!({ "type": "experiment", "repo": e.repo, "experiment": { "id": e.id, "repo": e.repo, "state": e.state, "decision": e.decision } }));
}

/// The next experiment's id: `E1`, `E2`, ….
async fn next_id(hub: &Hub) -> anyhow::Result<String> {
    hub.store
        .call(|c| {
            let tx = c.transaction()?;
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
    if !hub.ci.enabled() {
        return Err((StatusCode::NOT_FOUND, "the CI engine is off ([ci] enabled)".into()));
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
        return Err(bad("say how a variant is measured: a `pipeline` or `command` criterion (a metric is read from their runs)"));
    }
    // A command runs where the asking agent does, unless it says.
    let machine = ask.agent.clone().or_else(|| caller.agent.clone()).and_then(|a| hub.harness.info(&a)).map(|i| i.target.strip_prefix("agent:").unwrap_or(&i.target).to_owned());
    for c in &mut ask.criteria {
        if let Kind::Command { runs_on, .. } = &mut c.kind
            && runs_on.is_none()
        {
            *runs_on = machine.clone();
        }
    }
    for c in &ask.criteria {
        if let Kind::Command { runs_on: None, .. } = &c.kind {
            return Err(bad(format!("`{}`: a command criterion says where it runs (`runs-on`: a label or a runner)", c.name)));
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
    let asker = ask.agent.clone().or_else(|| caller.agent.clone()).and_then(|a| hub.harness.info(&a));
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
        repetitions: ask.repetitions.unwrap_or(REPETITIONS).clamp(1, 20),
        max_parallel: ask.max_parallel.unwrap_or(PARALLEL).clamp(1, 16),
        budget_minutes: ask.budget_minutes,
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

/// Builds `e`'s variants, measures them, says what the measurements show and decides.
async fn drive(hub: Arc<Hub>, mut e: Experiment, repo: Repo) {
    let deadline = e.budget_minutes.map(|m| std::time::Instant::now() + Duration::from_secs(m * 60));
    if e.options.iter().any(|o| o.implement.is_some() && o.to.is_empty()) {
        e.state = "implementing".into();
        save(&hub, &mut e).await;
        implement(&hub, &mut e, &repo, deadline).await;
        if e.state == "cancelled" {
            save(&hub, &mut e).await;
            return;
        }
    }
    let names: Vec<String> = e.options.iter().map(|o| o.name.clone()).collect();
    let greedy = matches!(e.strategy, Some(Strategy::Greedy));
    let mut round_variants: Vec<String> = e.variants.iter().map(|v| v.name.clone()).collect();
    loop {
        if let Err(why) = build(&hub, &mut e, &repo, &round_variants).await {
            e.state = "failed".into();
            e.why = Some(why);
            save(&hub, &mut e).await;
            return;
        }
        e.state = "measuring".into();
        save(&hub, &mut e).await;
        let mut ended = measure(&hub, &mut e, &repo, &round_variants, deadline).await;
        // The variants agents glued, measured once they're made.
        if ended.is_none() {
            let glued = glue_finish(&hub, &mut e, &repo, &round_variants, deadline).await;
            if !glued.is_empty() {
                ended = measure(&hub, &mut e, &repo, &glued, deadline).await;
            }
        }
        analyze(&mut e);
        save(&hub, &mut e).await;
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
        save(&hub, &mut e).await;
    }
    if e.state != "cancelled" {
        e.state = "done".into();
        let recommended = e.results.as_ref().and_then(|r| r.get("recommendation")).and_then(Value::as_str).map(str::to_owned);
        save(&hub, &mut e).await;
        if e.decide == "auto"
            && let Some(v) = recommended.as_ref().and_then(|n| e.variants.iter().find(|v| &v.name == n)).cloned()
        {
            record_decision(&hub, &mut e, Decision { variant: v.name.clone(), commit: v.commit.clone().unwrap_or_default(), by: "kiln".into(), at: crate::util::now_unix() }).await;
        }
        let e = overlay(&hub, e).await;
        tell(&hub, &e).await;
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
        let starter = crate::harness::lineage::Starter { parent: parent.clone(), opener: Some(e.opener.clone()), actor: format!("experiment:{}", e.id), plan_run: None };
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
    let starter = crate::harness::lineage::Starter { parent, opener: Some(e.opener.clone()), actor: format!("experiment:{}", e.id), plan_run: None };
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
    let said = hub.harness.conclusion(id).and_then(|(_, reply, _)| reply).and_then(|r| r.get("text").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
    let Some(d) = named_diff(&said) else { return Err(format!("its agent ended without a diff: {}", crate::util::truncate(said.trim(), 200))) };
    let diff = super::diffs::find(hub, d).await.ok().flatten().filter(|x| x.repo == repo.name).ok_or_else(|| format!("its agent named {d}, which isn't a diff of {}", repo.name))?;
    let v = super::diffs::versions(hub, &diff).await.ok().and_then(|v| v.into_iter().max_by_key(|x| x.n)).ok_or_else(|| format!("{d} has no version"))?;
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    git(hub, &["-C", &mirror, "cat-file", "-e", &format!("{}^{{commit}}", v.commit)]).await.map_err(|_| format!("{d}'s commit isn't on the server"))?;
    Ok((diff.name, v.parent, v.commit))
}

/// Waits for agents `ids` to settle: done, waiting on a person, stopped; the experiment cancelled,
/// or out of time.
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
                let r = start_one(hub, e, repo, variant, commit, c, round).await;
                started.push(r);
            }
            for mut r in started {
                if let Some(run) = r.run.clone() {
                    let (conclusion, metrics, minutes) = finish(hub, &e.id, &run, deadline).await;
                    r.conclusion = Some(conclusion);
                    r.metrics = metrics;
                    if let Some(v) = e.variants.iter_mut().find(|v| v.name == r.variant) {
                        v.minutes += minutes;
                    }
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

/// Whether to stop: cancelled, or past the budget (its runs cancelled either way).
async fn stopped(hub: &Hub, e: &mut Experiment, deadline: Option<std::time::Instant>) -> Option<String> {
    if let Some(by) = hub.store.kv_get(cancel_key(&e.id)).await.ok().flatten() {
        e.state = "cancelled".into();
        return Some(format!("cancelled by {by}"));
    }
    deadline.filter(|d| std::time::Instant::now() >= *d).map(|_| format!("its budget of {} minutes ran out: what was measured is reported", e.budget_minutes.unwrap_or(0)))
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
    match super::uploads::start_runs(hub, repo, &ask, e.trusted, &actor, false).await {
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
    let why = e.results.as_ref().and_then(|r| r.get("why")).and_then(Value::as_str).unwrap_or_default().to_owned();
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
        .filter(|e| repo.is_none_or(|r| *r == e.repo) && caller.can("runs.read", &Scope::Repo { repo: e.repo.clone() }))
        .map(|e| {
            json!({ "id": e.id, "repo": e.repo, "question": e.question, "state": e.state, "variants": e.variants.len(), "measured": e.runs.iter().filter(|r| r.conclusion.is_some()).count(), "runs": planned(&e),
                    "recommendation": e.results.as_ref().and_then(|r| r.get("recommendation")), "decision": e.decision, "by": e.by, "created_at": e.created_at })
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "experiments": shown })))
}

/// The experiment `id`, for `caller` with `cap` on its repository.
async fn mine(hub: &Hub, caller: &Caller, id: &str, cap: &str) -> Result<Experiment, Fail> {
    let e = get(hub, id).await.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no experiment `{id}`")))?;
    if !caller.can(cap, &Scope::Repo { repo: e.repo.clone() }) {
        return Err((StatusCode::FORBIDDEN, format!("{} may not do that to {id}: it needs {cap} on {}", caller.name, e.repo)));
    }
    Ok(e)
}

/// `GET /api/experiments/:id`.
pub async fn show(hub: &Hub, caller: &Caller, id: &str) -> Result<Response, Fail> {
    let e = mine(hub, caller, id, "runs.read").await?;
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// `POST /api/experiments/:id/decide { variant }`: continue with it; the agent that asked is told.
pub async fn decide(hub: &Arc<Hub>, caller: &Caller, id: &str, body: &Value) -> Result<Response, Fail> {
    let mut e = mine(hub, caller, id, "experiments.run").await?;
    let name = body.get("variant").and_then(Value::as_str).ok_or_else(|| bad("say which variant (`variant`)"))?;
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
    if matches!(e.state.as_str(), "done" | "cancelled" | "failed") {
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
    hub.emit(&json!({ "type": "experiment", "repo": e.repo, "experiment": { "id": e.id, "repo": e.repo, "state": e.state } }));
    hub.store.audit(caller.name.clone(), "experiments.cancel".into(), id.to_owned()).await;
    Ok(srv::json(StatusCode::OK, &json!({ "experiment": e })))
}

/// When the server starts: experiments it was running stopped with it.
pub async fn interrupted(hub: &Hub) {
    for mut e in all(hub).await {
        if matches!(e.state.as_str(), "implementing" | "building" | "measuring") {
            e.state = "failed".into();
            e.why = Some("the server restarted while it ran: start it again".into());
            save(hub, &mut e).await;
        }
    }
}

/// `GET /api/experiments/where?agent=`: where an experiment from that agent's question runs: the
/// repository its folder is a checkout of, the workflows on its default branch that can measure
/// one, and whether the caller may start it.
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
    let workflows: Vec<String> = match super::events::named_ref(hub, &repo, &facts.default_branch).await {
        Ok((_, sha)) => super::load::load(hub, &repo, &sha).await.map(|c| c.workflows.into_iter().filter(|l| l.runs).map(|l| l.path).collect()).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(srv::json(StatusCode::OK, &json!({ "repo": repo.name, "branch": facts.default_branch, "workflows": workflows, "may": caller.can("experiments.run", &scope) })))
}

/// `/api/experiments[/:id[/decide|/cancel]]`, and `/where`.
pub async fn route(hub: &Arc<Hub>, caller: &Caller, method: &http::Method, rest: &[&str], q: &std::collections::HashMap<String, String>, body: Value) -> Result<Response, Fail> {
    use http::Method;
    match (method, rest) {
        (&Method::GET, []) => list(hub, caller, q).await,
        (&Method::GET, ["where"]) => where_for(hub, caller, q).await,
        (&Method::POST, []) => start(hub, caller, body).await,
        (&Method::GET, [id]) => show(hub, caller, id).await,
        (&Method::POST, [id, "decide"]) => decide(hub, caller, id, &body).await,
        (&Method::POST, [id, "cancel"]) => cancel(hub, caller, id).await,
        _ => Err((StatusCode::NOT_FOUND, "no such route".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
