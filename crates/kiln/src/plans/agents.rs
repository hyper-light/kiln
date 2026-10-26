//! A plan run as one agent per step (docs/plans.md, "One agent per step"; docs/agentic.md P).
//!
//! Each step is held to as a goal (`crate::goals`): its title, what to do, examples, "Done when:"
//! its acceptance criteria and "Tests:" its tests (`Item::goal`). An agent of its own pursues it, on
//! the machine and harness chosen for the run or for the step; or an agent that's there already
//! that the step was given to (`Place::agent`: the agent that ran the plan, say), on whose goal
//! stack the step's goal goes.
//!
//! A step starts once each step it waits for is met (`Item::needs`; in a plan whose steps say none,
//! each waits for the one before, or, run side by side, for none): steps that don't wait for each
//! other run side by side, as many at once as there's room for (`Config::max_parallel`, and the
//! agents a parent may run at once). Side by side in one folder they share it, and the run says so:
//! each step's agent keeps to its own files, or the steps get folders or machines of their own
//! (`Config::steps`), until agents have workspaces of their own (docs/agentic.md, W).
//!
//! An agent that runs a plan this way is the parent of its steps' agents, which get no more than
//! it has (`crate::harness::lineage`). The server drives the runs ([`drive`]): it starts each
//! step's agent, follows its goal to met or unmet and goes on, with no console open and across
//! restarts. A step that isn't met holds up the steps that wait for it, while the rest go on; once
//! nothing is at work and nothing more can start, the run stops, saying which steps hold it up. A
//! person stops a run, retries a step (a new agent for it), or takes a step over (kiln stops holding
//! its agent to the goal, and they say when it's met).
//!
//! As each step is met, what its agent changed goes up for review as that step's diff, on the diff
//! of the step met before it (`super::stack`): the run ends as a stack with one diff per step.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Error, Item, Plan, R};
use crate::goals::{At, By, Change, NewGoal, StepRef};
use crate::harness::lineage::{Ask, Starter};
use crate::harness::model::Command;
use crate::hub::Hub;

/// Steps one run has at the most.
const STEPS: usize = 100;
/// A goal's longest (as `crate::goals` keeps them).
const GOAL: usize = 1990;
/// How often the server looks at the runs under way.
const EVERY: Duration = Duration::from_secs(2);

/// Where and how one step's agent runs, over the run's own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Place {
    /// A machine's name, or what it needs (`linux/amd64`, `+gpu`), as `agents.start` takes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Give the step to an agent that's there already rather than start one: `main` (the run's
    /// main agent: [`Config::main`]) or an agent's id. Its goal goes on that agent's goal stack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// How a run's steps run, and who ran it (kept with the run).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where its steps run, unless a step says otherwise.
    pub place: Place,
    /// Each step's own, by its number from 0.
    pub steps: Vec<Place>,
    /// Steps of a plan that doesn't say what each waits for run side by side, not each once the
    /// one before is met.
    pub parallel: bool,
    /// Steps at work at once, at the most (none: every step that's ready).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<usize>,
    /// The agent steps given to `main` go to: the agent that ran the plan, or the one it was run
    /// for (`session`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// The agent that ran it: its steps' agents are its children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Whom the steps' agents act for (`owner`, `user:<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opener: Option<String>,
    /// Who ran it, as the audit trail names them.
    pub actor: String,
    /// Only these steps, by their number from 0 (none: every step): the steps they wait for
    /// outside them are taken as done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<usize>>,
}

/// One step of a run, as it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    /// Its number, from 1.
    pub n: usize,
    pub title: String,
    /// What its agent is held to.
    pub goal: String,
    /// `waiting` (for the steps it needs, or for room), `working`, `needs_you` (its agent asks a
    /// person), `yours` (taken over), `met`, `unmet`, `stopped`.
    pub stand: String,
    /// The steps it waits for, by number.
    pub needs: Vec<usize>,
    /// Given to an agent that was there already (its goal on that agent's stack), not one of its own.
    pub given: bool,
    /// Its agent (the latest, after a retry), as `agents.*` show agents.
    pub agent: Option<Value>,
    pub agent_id: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub reason: Option<String>,
    /// Times it was started (1 more each retry).
    pub tries: i64,
}

/// A run, as it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Run {
    pub id: String,
    pub plan: String,
    pub title: String,
    pub version: i64,
    /// `running`, `stopped` (see `reason`) or `done`.
    pub state: String,
    pub reason: Option<String>,
    /// Said of how it runs (which steps run side by side, and in one folder).
    pub note: Option<String>,
    pub at: i64,
    pub ended_at: Option<i64>,
    pub by: String,
    pub config: Config,
    pub steps: Vec<Step>,
    pub met: usize,
    pub total: usize,
    /// What its met steps are in review: one diff per step, each on the one met before it
    /// (`super::stack::shown`).
    pub stack: Option<Value>,
}

/// A run as lists show it (Mission control groups its agents under it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Brief {
    pub id: String,
    pub plan: String,
    pub title: String,
    pub state: String,
    pub met: usize,
    pub total: usize,
    pub at: i64,
}

/// The goal a step's agent is held to: where it stands in the plan, then the step as a goal.
pub fn goal_of(p: &Plan, i: usize, n: usize) -> String {
    let step = match p.items.get(i) {
        Some(it) => it.goal(),
        None => p.steps.get(i).cloned().unwrap_or_default(),
    };
    step_goal(&p.title, i, n, &step)
}

/// Step `i` (from 0) of `n` of the plan `title`, held to as a goal: where it stands in the plan, then
/// `step` (the step as a goal).
pub fn step_goal(title: &str, i: usize, n: usize, step: &str) -> String {
    let line = format!("Step {} of {} of the plan \"{}\": {}", i + 1, n, title.trim(), step).split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > GOAL { format!("{}…", line.chars().take(GOAL).collect::<String>()) } else { line }
}

fn title_of(p: &Plan, i: usize) -> String {
    let t = match p.items.get(i) {
        Some(Item { title, .. }) => title.trim().to_owned(),
        None => p.steps.get(i).map(|s| s.trim().to_owned()).unwrap_or_default(),
    };
    t.chars().take(120).collect()
}

/// What a run answers when it starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Started {
    pub run: String,
    pub steps: usize,
    pub note: Option<String>,
}

/// The agent step `i` is given to, when it's one that's there already (`Place::agent`): `main`,
/// the run's main agent, or an agent by id.
fn given_to(hub: &Hub, cfg: &Config, i: usize) -> R<Option<String>> {
    let named = cfg.steps.get(i).and_then(|s| s.agent.clone()).or_else(|| cfg.place.agent.clone()).map(|a| a.trim().to_owned()).filter(|a| !a.is_empty());
    match named.as_deref() {
        None => Ok(None),
        Some("main" | "self" | "me") => cfg
            .main
            .clone()
            .map(Some)
            .ok_or_else(|| Error::Bad(format!("step {} is given to the main agent, but this run has none: run the plan as an agent, or name the agent with `session`", i + 1))),
        Some(id) => hub.harness.info(id).map(|a| Some(a.id)).ok_or_else(|| Error::Bad(format!("step {} is given to agent {id}, which isn't there", i + 1))),
    }
}

fn either(own: &Option<String>, run: &Option<String>) -> Option<String> {
    own.clone().filter(|s| !s.trim().is_empty()).or_else(|| run.clone().filter(|s| !s.trim().is_empty()))
}

async fn ran(hub: &Hub, run: &str) -> R<bool> {
    let run = run.to_owned();
    Ok(hub.store.call(move |c| Ok(c.query_row("SELECT 1 FROM plan_runs WHERE id = ?1", params![run], |_| Ok(())).optional()?.is_some())).await?)
}

/// Steps as words: "step 2", "steps 2 and 3", "steps 2, 3 and 4" (each `i` from 0).
fn steps_words(i: &[usize]) -> String {
    let n: Vec<String> = i.iter().map(|i| (i + 1).to_string()).collect();
    match n.as_slice() {
        [] => String::new(),
        [one] => format!("step {one}"),
        [rest @ .., last] => format!("steps {} and {last}", rest.join(", ")),
    }
}

/// What a run says of how its steps run: which run side by side, and, side by side in one folder,
/// that each step's agent keeps to its own files. None when every step waits for the one before.
fn side_by_side(hub: &Hub, cfg: &Config, needs: &[Vec<usize>], given: &[Option<String>], picked: &[usize]) -> Option<String> {
    let rounds: Vec<Vec<usize>> = super::rounds(needs).into_iter().map(|r| r.into_iter().filter(|i| picked.contains(i)).collect()).collect();
    let together: Vec<&Vec<usize>> = rounds.iter().filter(|r| r.len() > 1).collect();
    if together.is_empty() {
        return None;
    }
    // Where each works: on the agent it's given to's machine and folder, else its own place.
    let place = |i: usize| -> (String, String) {
        if let Some(a) = given.get(i).cloned().flatten().and_then(|a| hub.harness.info(&a)) {
            return (a.target, a.cwd);
        }
        let own = cfg.steps.get(i).cloned().unwrap_or_default();
        let pick = |a: &Option<String>, b: &Option<String>| a.clone().filter(|s| !s.trim().is_empty()).or_else(|| b.clone()).unwrap_or_default();
        (pick(&own.on, &cfg.place.on), pick(&own.cwd, &cfg.place.cwd))
    };
    let shared = together.iter().any(|r| r.iter().enumerate().any(|(k, &i)| r.iter().skip(k + 1).any(|&j| place(i) == place(j))));
    let list = together.iter().map(|r| steps_words(r)).collect::<Vec<_>>().join("; ");
    Some(if shared {
        format!("Steps that don't wait for each other run side by side ({list}), in one folder: each step's agent keeps to its own files. Give steps folders or machines of their own with `steps`.")
    } else {
        format!("Steps that don't wait for each other run side by side ({list}).")
    })
}

/// Runs plan `p` as one agent per step, as `cfg` says. The steps that wait for none start at once.
/// Under `key` (CP-T3.2), asked for again, it's the run it started then, as it stands.
pub async fn start(hub: &Arc<Hub>, p: &Plan, cfg: Config, key: Option<&str>) -> R<Started> {
    super::ready_to_run(p)?;
    let n = p.steps.len().min(STEPS);
    if n == 0 {
        return Err(Error::Bad("this plan has no steps (no list in it) to give agents".into()));
    }
    let picked = super::Pick { only: cfg.only.clone(), split: false }.of(n);
    if picked.is_empty() {
        return Err(Error::Bad(format!("none of those steps is in this plan: it has {n}")));
    }
    let given: Vec<Option<String>> = (0..n).map(|i| if picked.contains(&i) { given_to(hub, &cfg, i) } else { Ok(None) }).collect::<R<_>>()?;
    let blank = |k: &Option<String>| k.as_deref().is_none_or(|k| k.trim().is_empty());
    if blank(&cfg.place.kind) && picked.iter().any(|&i| given.get(i).is_some_and(Option::is_none) && cfg.steps.get(i).is_none_or(|s| blank(&s.kind))) {
        return Err(Error::Bad("say which harness runs its steps: `new.kind` (or each step's `kind`)".into()));
    }
    // The same id as `plans::run` makes under the key: one key, one request, whatever its mode.
    let run = key.map_or_else(crate::util::new_id, |k| crate::util::derive_id(&["plans.run", &p.id, k]));
    if key.is_none() || !ran(hub, &run).await? {
        let parent = cfg.parent.as_deref().and_then(|a| hub.harness.info(a));
        for &i in picked.iter().filter(|&&i| given.get(i).is_some_and(Option::is_none)) {
            let own = cfg.steps.get(i).cloned().unwrap_or_default();
            let kind = either(&own.kind, &cfg.place.kind).unwrap_or_default();
            crate::harness::lineage::placed(hub, parent.as_ref(), either(&own.on, &cfg.place.on).as_deref(), &kind).await.map_err(|(c, m)| Error::Refused(c, format!("step {}: {m}", i + 1)))?;
        }
    }
    // The steps it takes wait only for each other: those outside the run are taken as done.
    let all = super::waits(&p.items, n, cfg.parallel);
    let needs: Vec<Vec<usize>> = all.iter().enumerate().map(|(i, d)| if picked.contains(&i) { d.iter().copied().filter(|j| picked.contains(j)).collect() } else { d.clone() }).collect();
    let outside: Vec<String> = picked
        .iter()
        .filter_map(|&i| {
            let o: Vec<usize> = all.get(i)?.iter().copied().filter(|j| !picked.contains(j)).collect();
            (!o.is_empty()).then(|| format!("step {} waits for {}", i + 1, steps_words(&o)))
        })
        .collect();
    let outside = (!outside.is_empty()).then(|| format!("Not in this run, taken as done: {}.", outside.join("; ")));
    let note = [side_by_side(hub, &cfg, &needs, &given, &picked), outside].into_iter().flatten().collect::<Vec<_>>();
    let note = (!note.is_empty()).then(|| note.join(" "));
    // Each step keeps its number in the plan.
    let steps: Vec<(usize, String, String, String, Option<String>)> = picked
        .iter()
        .map(|&i| (i, title_of(p, i), goal_of(p, i, n), serde_json::to_string(needs.get(i).map_or(&[][..], Vec::as_slice)).unwrap_or_else(|_| "[]".into()), given.get(i).cloned().flatten()))
        .collect();
    let taken = steps.len();
    let (now, version, plan_id, by) = (crate::util::now_unix(), p.version, p.id.clone(), cfg.actor.clone());
    let config = serde_json::to_string(&cfg).map_err(anyhow::Error::from)?;
    let (run_c, note_c) = (run.clone(), note.clone());
    // The run it started before under its key: its steps and note as they were, what it was asked
    // (its config), and whether it was one agent per step too.
    let had: Option<(usize, Option<String>, Option<String>, bool)> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let made = tx.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by, state, config, note) VALUES (?1, ?2, ?3, '', 'agents', ?4, ?5, 'running', ?6, ?7) ON CONFLICT (id) DO NOTHING",
                params![run_c, plan_id, version, now, by, config, note_c],
            )?;
            if made == 0 {
                let steps: i64 = tx.query_row("SELECT COUNT(*) FROM plan_run_steps WHERE run_id = ?1", params![run_c], |r| r.get(0))?;
                let (note, was, mode): (Option<String>, Option<String>, String) = tx.query_row("SELECT note, config, mode FROM plan_runs WHERE id = ?1", params![run_c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                return Ok(Some((usize::try_from(steps).unwrap_or(0), note, was, mode == "agents")));
            }
            for (i, title, goal, needs, given) in &steps {
                tx.execute(
                    "INSERT INTO plan_run_steps (run_id, idx, title, goal, needs, given_to) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![run_c, i64::try_from(*i).unwrap_or(0), title, goal, needs, given],
                )?;
            }
            tx.commit()?;
            Ok(None)
        })
        .await?;
    if let Some((steps, note, was, agents)) = had {
        // Asked again under its key for other steps, or after a run of it in another mode, it's
        // another request: refused, never answered as the run it isn't.
        let was_cfg = was.as_deref().and_then(|c| serde_json::from_str::<Config>(c).ok());
        if !agents || was_cfg.as_ref().is_none_or(|c| c.only != cfg.only) {
            return Err(Error::Refused(http::StatusCode::CONFLICT, "this key was given to another request (other steps, or another mode): give this one a key of its own".into()));
        }
        return Ok(Started { run, steps, note });
    }
    // The first steps' agents, now rather than at the next look.
    if let Err(e) = advance(hub, &run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    Ok(Started { run, steps: taken, note })
}

struct Row {
    idx: i64,
    title: String,
    goal: String,
    status: String,
    agent: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    reason: Option<String>,
    tries: i64,
    /// The steps it waits for (indexes from 0).
    needs: Vec<usize>,
    /// The agent that was there already it's given to.
    given_to: Option<String>,
}

struct Head {
    plan: String,
    version: i64,
    state: String,
    reason: Option<String>,
    note: Option<String>,
    at: i64,
    ended_at: Option<i64>,
    by: String,
    config: Config,
}

fn load(c: &rusqlite::Connection, run: &str) -> anyhow::Result<Option<(Head, Vec<Row>)>> {
    let head = c
        .query_row(
            "SELECT plan_id, version, COALESCE(state, ''), reason, note, at, ended_at, by, COALESCE(config, '{}') FROM plan_runs WHERE id = ?1 AND mode = 'agents'",
            params![run],
            |r| {
                Ok(Head {
                    plan: r.get(0)?,
                    version: r.get(1)?,
                    state: r.get(2)?,
                    reason: r.get(3)?,
                    note: r.get(4)?,
                    at: r.get(5)?,
                    ended_at: r.get(6)?,
                    by: r.get(7)?,
                    config: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
                })
            },
        )
        .optional()?;
    let Some(head) = head else { return Ok(None) };
    let mut st = c.prepare("SELECT idx, title, goal, status, agent, started_at, ended_at, reason, tries, needs, given_to FROM plan_run_steps WHERE run_id = ?1 ORDER BY idx")?;
    let rows = st
        .query_map(params![run], |r| {
            let idx: i64 = r.get(0)?;
            let needs: Option<String> = r.get(9)?;
            Ok(Row {
                idx,
                title: r.get(1)?,
                goal: r.get(2)?,
                status: r.get(3)?,
                agent: r.get(4)?,
                started_at: r.get(5)?,
                ended_at: r.get(6)?,
                reason: r.get(7)?,
                tries: r.get(8)?,
                // A run from before steps said what they wait for: each the one before.
                needs: match needs {
                    Some(n) => serde_json::from_str(&n).unwrap_or_default(),
                    None => usize::try_from(idx - 1).ok().into_iter().collect(),
                },
                given_to: r.get(10)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((head, rows)))
}

async fn load_run(hub: &Hub, run: &str) -> R<(Head, Vec<Row>)> {
    let r = run.to_owned();
    hub.store.call(move |c| load(c, &r)).await?.ok_or_else(|| Error::Bad(format!("no plan run {run}")))
}

/// One look at a run at a time (the server's, a start, a retry, a stop …), so a step's agent
/// starts once and what's changed is changed on the run as it is.
static LOOKING: LazyLock<parking_lot::Mutex<HashMap<String, Arc<async_lock::Mutex<()>>>>> = LazyLock::new(Default::default);

fn looking(run: &str) -> Arc<async_lock::Mutex<()>> {
    Arc::clone(LOOKING.lock().entry(run.to_owned()).or_default())
}

/// Where a step stands, its agent as it is now.
fn stand_of(hub: &Hub, row: &Row) -> (String, Option<Value>) {
    let info = row.agent.as_deref().and_then(|a| hub.harness.info(a));
    let brief = info.as_ref().map(crate::harness::wait::brief);
    let stand = match row.status.as_str() {
        "started" => match brief.as_ref().and_then(|b| b.get("stand")).and_then(Value::as_str) {
            Some("needs_you") => "needs_you",
            _ => "working",
        },
        "taken" => "yours",
        "waiting" => "waiting",
        other => other,
    };
    (stand.to_owned(), brief)
}

/// A run as it stands.
pub async fn view(hub: &Hub, run: &str) -> R<Run> {
    let (head, rows) = load_run(hub, run).await?;
    let title = super::get(hub, &head.plan).await.map(|p| p.title).unwrap_or_else(|_| "(a plan since deleted)".into());
    let steps: Vec<Step> = rows
        .iter()
        .map(|r| {
            let (stand, agent) = stand_of(hub, r);
            Step {
                n: usize::try_from(r.idx).unwrap_or(0) + 1,
                title: r.title.clone(),
                goal: r.goal.clone(),
                stand,
                needs: r.needs.iter().map(|d| d + 1).collect(),
                given: r.given_to.is_some(),
                agent,
                agent_id: r.agent.clone(),
                started_at: r.started_at,
                ended_at: r.ended_at,
                reason: r.reason.clone(),
                tries: r.tries,
            }
        })
        .collect();
    let met = steps.iter().filter(|s| s.stand == "met").count();
    let stack = super::stack::of(hub, run).await.map(|s| super::stack::shown(&s));
    Ok(Run { id: run.to_owned(), plan: head.plan, title, version: head.version, state: head.state, reason: head.reason, note: head.note, at: head.at, ended_at: head.ended_at, by: head.by, config: head.config, total: steps.len(), met, steps, stack })
}

/// Where run `run` stands (`running`, `stopped`, `done`), when there is such a run.
pub async fn state(hub: &Hub, run: &str) -> Option<String> {
    let run = run.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT COALESCE(state, '') FROM plan_runs WHERE id = ?1", params![run], |r| r.get::<_, String>(0)).optional()?)).await.ok().flatten()
}

/// Runs one agent per step, newest first: those under way (`active`), else the last 50.
pub async fn list(hub: &Hub, active: bool) -> R<Vec<Brief>> {
    let rows: Vec<(String, String, String, i64, i64, i64)> = hub
        .store
        .call(move |c| {
            let q = if active {
                "SELECT r.id, r.plan_id, COALESCE(r.state, ''), r.at, (SELECT COUNT(*) FROM plan_run_steps s WHERE s.run_id = r.id AND s.status = 'met'), (SELECT COUNT(*) FROM plan_run_steps s WHERE s.run_id = r.id) FROM plan_runs r WHERE r.mode = 'agents' AND r.state = 'running' ORDER BY r.at DESC LIMIT 50"
            } else {
                "SELECT r.id, r.plan_id, COALESCE(r.state, ''), r.at, (SELECT COUNT(*) FROM plan_run_steps s WHERE s.run_id = r.id AND s.status = 'met'), (SELECT COUNT(*) FROM plan_run_steps s WHERE s.run_id = r.id) FROM plan_runs r WHERE r.mode = 'agents' ORDER BY r.at DESC LIMIT 50"
            };
            let mut st = c.prepare(q)?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut out = Vec::new();
    for (id, plan, state, at, met, total) in rows {
        let title = super::get(hub, &plan).await.map(|p| p.title).unwrap_or_else(|_| "(a plan since deleted)".into());
        out.push(Brief { id, plan, title, state, met: usize::try_from(met).unwrap_or(0), total: usize::try_from(total).unwrap_or(0), at });
    }
    Ok(out)
}

async fn set_step(hub: &Hub, run: &str, idx: i64, status: &str, reason: Option<String>) -> R<()> {
    let (run, status, now) = (run.to_owned(), status.to_owned(), crate::util::now_unix());
    let ends = matches!(status.as_str(), "met" | "unmet" | "stopped");
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE plan_run_steps SET status = ?3, reason = ?4, ended_at = CASE WHEN ?5 THEN ?6 ELSE NULL END WHERE run_id = ?1 AND idx = ?2",
                params![run, idx, status, reason, ends, now],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

async fn set_run(hub: &Hub, run: &str, state: &str, reason: Option<String>) -> R<()> {
    let (run_c, state_c, now) = (run.to_owned(), state.to_owned(), crate::util::now_unix());
    let ends = state != "running";
    hub.store
        .call(move |c| {
            c.execute("UPDATE plan_runs SET state = ?2, reason = ?3, ended_at = CASE WHEN ?4 THEN ?5 ELSE NULL END WHERE id = ?1", params![run_c, state_c, reason, ends, now])?;
            Ok(())
        })
        .await?;
    // What review and CI said to its steps' agents while it went on goes to them now.
    if ends {
        super::stack::release(hub, run).await;
    }
    Ok(())
}

/// Step `row` is met: what its agent changed goes up for review, on the diff of the step met
/// before it.
async fn stacked(hub: &Arc<Hub>, run: &str, head: &Head, row: &Row, total: usize) {
    let plan = super::get(hub, &head.plan).await.map(|p| p.title).unwrap_or_default();
    let m = super::stack::Met { run, n: usize::try_from(row.idx).unwrap_or(0) + 1, total, title: &row.title, goal: &row.goal, plan: &plan, agent: row.agent.as_deref(), opener: head.config.opener.as_deref() };
    super::stack::met(hub, m).await;
}

/// The goal step `row` of run `run` is on its agent's stack: the one made from it (the latest),
/// else (a goal from before goals said their step) the one with its words, else, on an agent of its
/// own, its first.
fn goal_of_step<'a>(stack: &'a crate::goals::Stack, run: &str, row: &Row) -> Option<&'a crate::goals::Item> {
    let n = usize::try_from(row.idx).unwrap_or(0) + 1;
    stack
        .items
        .iter()
        .rev()
        .find(|i| i.part_of.is_none() && i.step.as_ref().is_some_and(|s| s.run.as_deref() == Some(run) && s.n == n))
        .or_else(|| stack.items.iter().rev().find(|i| i.part_of.is_none() && i.text.split_whitespace().eq(row.goal.split_whitespace())))
        .or_else(|| if row.given_to.is_none() { stack.items.first() } else { None })
}

/// Notes step `row` started, by `agent`.
async fn mark_started(hub: &Hub, run: &str, idx: i64, agent: &str) -> R<()> {
    let (run_c, agent_c, now) = (run.to_owned(), agent.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE plan_run_steps SET status = 'started', agent = ?3, started_at = ?4, ended_at = NULL, reason = NULL, tries = tries + 1 WHERE run_id = ?1 AND idx = ?2",
                params![run_c, idx, agent_c, now],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Starts step `row`: its agent started and held to the step's goal, or, given to an agent that's
/// there already, the step's goal on that agent's stack (run, unless someone paused it: then it
/// waits there until it's resumed).
async fn begin(hub: &Hub, run: &str, head: &Head, row: &Row) -> R<()> {
    let at = usize::try_from(row.idx).unwrap_or(0);
    let step = StepRef { plan: head.plan.clone(), version: head.version, n: at + 1, run: Some(run.to_owned()) };
    let goal = vec![NewGoal { text: row.goal.clone(), step: Some(step), ..NewGoal::default() }];
    if let Some(to) = row.given_to.as_deref() {
        if hub.harness.info(to).is_none() {
            return Err(Error::Bad("the agent it was given to has ended".into()));
        }
        let paused = crate::goals::get(hub, to).await.is_ok_and(|s| s.state == "paused" && s.reason.is_some());
        mark_started(hub, run, row.idx, to).await?;
        crate::goals::add_at(hub, to, goal, !paused, &At::End).await.map_err(|e| Error::Bad(format!("its goal: {e}")))?;
        tracing::info!(%run, step = at + 1, agent = %to, "a plan step's goal goes on an agent's goal stack");
        return Ok(());
    }
    let own = head.config.steps.get(at).cloned().unwrap_or_default();
    let base = &head.config.place;
    let parent = match head.config.parent.as_deref() {
        Some(p) => Some(hub.harness.info(p).ok_or_else(|| Error::Bad("the agent that ran this plan has ended, so its steps can't be held to it: run the plan again".into()))?),
        None => None,
    };
    // The person who ran the plan needs runners.terminal where each step's agent starts, as they
    // would to start it themselves (docs/agents.md, "Whose agents").
    let asker = match head.config.opener.as_deref() {
        Some(o) => crate::authz::gate::agent_for(hub, &head.config.actor, o).await,
        None => None,
    };
    let starter = Starter { parent, opener: head.config.opener.clone(), actor: head.config.actor.clone(), plan_run: Some(run.to_owned()), asker };
    let name: String = format!("Step {}: {}", at + 1, row.title).chars().take(80).collect();
    let ask = Ask {
        kind: either(&own.kind, &base.kind).unwrap_or_default(),
        on: either(&own.on, &base.on),
        cwd: either(&own.cwd, &base.cwd),
        model: either(&own.model, &base.model),
        effort: either(&own.effort, &base.effort),
        mode: either(&own.mode, &base.mode),
        name: Some(name),
        count: Some(1),
        ..Ask::default()
    };
    let infos = crate::harness::lineage::start(hub, &starter, ask).await.map_err(|(_, m)| Error::Bad(m))?;
    let agent = infos.first().map(|i| i.id.clone()).ok_or_else(|| Error::Bad("the step's agent didn't start".into()))?;
    mark_started(hub, run, row.idx, &agent).await?;
    crate::goals::add_at(hub, &agent, goal, true, &At::End).await.map_err(|e| Error::Bad(format!("its goal: {e}")))?;
    tracing::info!(%run, step = at + 1, %agent, "a plan step's agent started");
    Ok(())
}

/// How a started step's goal came out: `Some((met, why))` once it ended.
async fn outcome(hub: &Hub, run: &str, row: &Row) -> R<Option<(bool, Option<String>)>> {
    let Some(agent) = row.agent.as_deref() else { return Ok(Some((false, Some("it has no agent".into())))) };
    if hub.harness.info(agent).is_none() {
        return Ok(Some((false, Some("its agent ended before the step was met".into()))));
    }
    let stack = crate::goals::get(hub, agent).await.map_err(|e| Error::Bad(e.to_string()))?;
    Ok(match goal_of_step(&stack, run, row).map(|i| (i.status.as_str(), i.reason.clone())) {
        Some(("met", why)) => Some((true, why)),
        Some(("unmet", why)) => Some((false, why.or_else(|| Some("its agent gave up on it".into())))),
        Some(("skipped", _)) => Some((false, Some("its goal was skipped".into()))),
        None => Some((false, Some("its goal was cleared".into()))),
        Some(_) => None,
    })
}

/// Step `row`'s agent no longer held to the step: an agent of its own has its goal stack paused (and,
/// `interrupt`, its turn stopped); on an agent it was given to, the step's goal is taken out of that
/// agent's stack, and the agent goes on with its others.
async fn let_go(hub: &Hub, run: &str, row: &Row, interrupt: bool) {
    let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) else { return };
    if row.given_to.is_some() {
        let goal = crate::goals::get(hub, a).await.ok().and_then(|s| goal_of_step(&s, run, row).filter(|g| matches!(g.status.as_str(), "pending" | "active")).map(|g| g.id.clone()));
        if let Some(id) = goal {
            let _ = crate::goals::update(hub, a, &id, Change { remove: true, ..Change::default() }, &By::You).await;
        }
        return;
    }
    let _ = crate::goals::control(hub, a, crate::goals::Action::Pause).await;
    if interrupt {
        let _ = hub.harness.command(hub, a, Command::Interrupt).await;
    }
}

/// How many more steps may start now: (steps at all, steps that start an agent) within the run's
/// `max_parallel` (`at_work` are at work) and the room left in its parent's pool of subagents at
/// work (its Subagents setting, whatever each does; none set, no limit: `lineage::room`).
async fn room(hub: &Hub, head: &Head, at_work: usize) -> (usize, usize) {
    let all = head.config.max_parallel.filter(|m| *m > 0).map_or(usize::MAX, |m| m.saturating_sub(at_work));
    let agents = match head.config.parent.as_deref().and_then(|p| hub.harness.info(p)) {
        Some(p) => crate::harness::lineage::room(hub, &p).await,
        None => usize::MAX,
    };
    (all, agents)
}

/// Why a run with nothing at work and nothing more to start stops: the steps not met that hold up
/// the rest.
fn held_up(rows: &[Row]) -> String {
    let stuck: Vec<&Row> = rows.iter().filter(|r| matches!(r.status.as_str(), "unmet" | "stopped")).collect();
    match stuck.as_slice() {
        [] => "no step can start: each waits for another".to_owned(),
        [one] => format!("step {} wasn't met{}: retry it, or take it over", one.idx + 1, one.reason.as_ref().map(|w| format!(" ({w})")).unwrap_or_default()),
        many => {
            let i: Vec<usize> = many.iter().filter_map(|r| usize::try_from(r.idx).ok()).collect();
            format!("{} weren't met: retry them, or take them over", steps_words(&i))
        }
    }
}

/// Moves run `run` on: the steps at work that ended are met (each going up for review) or not; the
/// steps whose waits are all met start, as many as there's room for; every step met, it's done; and
/// with nothing at work and nothing more to start, it stops, saying which steps hold it up (a
/// person retries them, or takes them over).
pub async fn advance(hub: &Arc<Hub>, run: &str) -> R<()> {
    let lock = looking(run);
    let _one = lock.lock().await;
    advance_locked(hub, run).await
}

async fn advance_locked(hub: &Arc<Hub>, run: &str) -> R<()> {
    let (head, mut rows) = load_run(hub, run).await?;
    if head.state != "running" {
        return Ok(());
    }
    let total = rows.len();
    for row in rows.iter_mut().filter(|r| r.status == "started") {
        match outcome(hub, run, row).await? {
            Some((true, why)) => {
                set_step(hub, run, row.idx, "met", why).await?;
                tracing::info!(%run, step = row.idx + 1, "a plan step was met");
                stacked(hub, run, &head, row, total).await;
                row.status = "met".into();
            }
            Some((false, why)) => {
                set_step(hub, run, row.idx, "unmet", why.clone()).await?;
                tracing::info!(%run, step = row.idx + 1, why = why.as_deref().unwrap_or(""), "a plan step wasn't met");
                row.status = "unmet".into();
                row.reason = why;
            }
            None => {}
        }
    }
    if rows.iter().all(|r| r.status == "met") {
        set_run(hub, run, "done", None).await?;
        tracing::info!(%run, "a plan run is done: every step met");
        return Ok(());
    }
    let met: Vec<usize> = rows.iter().filter(|r| r.status == "met").filter_map(|r| usize::try_from(r.idx).ok()).collect();
    let mut at_work = rows.iter().filter(|r| matches!(r.status.as_str(), "started" | "taken")).count();
    let (mut all, mut agents) = room(hub, &head, at_work).await;
    let mut no_room = false;
    for row in rows.iter_mut().filter(|r| r.status == "waiting" && r.needs.iter().all(|d| met.contains(d))) {
        let own = row.given_to.is_none();
        if all == 0 || (own && agents == 0) {
            no_room = true;
            continue;
        }
        match begin(hub, run, &head, row).await {
            Ok(()) => {
                all = all.saturating_sub(1);
                agents = agents.saturating_sub(usize::from(own));
                at_work += 1;
                row.status = "started".into();
            }
            Err(e) => {
                let why = format!("it couldn't start: {e}");
                set_step(hub, run, row.idx, "unmet", Some(why.clone())).await?;
                row.status = "unmet".into();
                row.reason = Some(why);
            }
        }
    }
    if at_work == 0 {
        let parent = head.config.parent.as_deref().and_then(|p| hub.harness.info(p));
        // No room while its parent has other subagents at work (one pool, whatever each does:
        // docs/agentic.md P2): its steps wait for one to finish, rather than the run stopping.
        if no_room
            && let Some(p) = &parent
            && crate::harness::lineage::busy(hub, p).await
        {
            return Ok(());
        }
        let why = match parent {
            Some(p) if no_room => format!("{} may have no subagents at work ({}): raise that, then retry the run's next step", p.name, crate::harness::lineage::limit_words(hub, &p).await),
            _ => held_up(&rows),
        };
        set_run(hub, run, "stopped", Some(why)).await?;
    }
    Ok(())
}

/// Drives every run under way, with no console open and across restarts.
pub async fn drive(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(EVERY).await {
            return Ok(());
        }
        let running: Vec<String> = hub
            .store
            .call(|c| {
                let mut st = c.prepare("SELECT id FROM plan_runs WHERE mode = 'agents' AND state = 'running'")?;
                let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                Ok(v)
            })
            .await
            .unwrap_or_default();
        for run in running {
            if let Err(e) = advance(&hub, &run).await {
                tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
            }
        }
    }
}

/// Stops a run: no next step starts, and the steps under way are let go (their own agents
/// interrupted and no longer held to their goals: they stay, to look at or carry on with; a step's
/// goal on an agent it was given to taken out of its stack).
pub async fn stop(hub: &Hub, run: &str, by: &str) -> R<Run> {
    {
        let lock = looking(run);
        let _one = lock.lock().await;
        let (head, rows) = load_run(hub, run).await?;
        if head.state == "running" {
            for row in rows.iter().filter(|r| matches!(r.status.as_str(), "started" | "taken")) {
                let_go(hub, run, row, true).await;
                set_step(hub, run, row.idx, "stopped", Some(format!("stopped by {by}"))).await?;
            }
            set_run(hub, run, "stopped", Some(format!("stopped by {by}"))).await?;
        }
    }
    hub.store.audit(by.to_owned(), "plan.run.stop".into(), run.to_owned()).await;
    view(hub, run).await
}

fn step_row(rows: &[Row], n: usize) -> R<&Row> {
    let idx = n.checked_sub(1).and_then(|i| i64::try_from(i).ok()).ok_or_else(|| Error::Bad("steps count from 1".into()))?;
    rows.iter().find(|r| r.idx == idx).ok_or_else(|| Error::Bad(format!("the run has no step {n}")))
}

/// Tries step `n` again (a new agent for it; given to an agent that's there, its goal again on that
/// agent's stack): the one before, if it still runs, ends, and the run goes on from there. `seen`:
/// the try the asker means to replace (the step's `tries` as it saw them): asked again once the step
/// was tried again since, nothing more is started (CP-T3.2), and the run is answered as it is.
pub async fn retry(hub: &Arc<Hub>, run: &str, n: usize, seen: Option<i64>, by: &str) -> R<Run> {
    {
        let lock = looking(run);
        let _one = lock.lock().await;
        let (_, rows) = load_run(hub, run).await?;
        let row = step_row(&rows, n)?;
        if seen.is_some_and(|t| t != row.tries) {
            drop(_one);
            return view(hub, run).await;
        }
        if row.status == "met" {
            return Err(Error::Bad(format!("step {n} is met already")));
        }
        if row.status == "waiting" {
            return Err(Error::Bad(format!("step {n} hasn't started yet")));
        }
        match row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
            Some(_) if row.given_to.is_some() => let_go(hub, run, row, false).await,
            Some(a) => {
                crate::harness::ops::end(hub, a, by).await;
            }
            None => {}
        }
        let (run_c, idx) = (run.to_owned(), row.idx);
        hub.store
            .call(move |c| {
                c.execute("UPDATE plan_run_steps SET status = 'waiting', reason = NULL, ended_at = NULL WHERE run_id = ?1 AND idx = ?2", params![run_c, idx])?;
                Ok(())
            })
            .await?;
        set_run(hub, run, "running", None).await?;
    }
    hub.store.audit(by.to_owned(), "plan.run.retry".into(), format!("{run} step {n}")).await;
    if let Err(e) = advance(hub, run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    view(hub, run).await
}

/// A person takes step `n` over: kiln no longer holds its agent to the goal (they work with it
/// themselves), and the steps waiting for it wait until they say it's met.
pub async fn take(hub: &Hub, run: &str, n: usize, by: &str) -> R<Run> {
    {
        let lock = looking(run);
        let _one = lock.lock().await;
        let (_, rows) = load_run(hub, run).await?;
        let row = step_row(&rows, n)?;
        if !matches!(row.status.as_str(), "started" | "unmet" | "stopped") {
            return Err(Error::Bad(format!("step {n} isn't under way: there's nothing to take over")));
        }
        let_go(hub, run, row, false).await;
        set_step(hub, run, row.idx, "taken", Some(format!("taken over by {by}"))).await?;
        set_run(hub, run, "running", None).await?;
    }
    hub.store.audit(by.to_owned(), "plan.run.take".into(), format!("{run} step {n}")).await;
    view(hub, run).await
}

/// A person says step `n` is met (one they took over, or one they judge done): the steps waiting
/// for it start.
pub async fn met(hub: &Arc<Hub>, run: &str, n: usize, by: &str) -> R<Run> {
    {
        let lock = looking(run);
        let _one = lock.lock().await;
        let (head, rows) = load_run(hub, run).await?;
        let row = step_row(&rows, n)?;
        if row.status == "waiting" {
            return Err(Error::Bad(format!("step {n} hasn't started yet")));
        }
        if let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
            if row.given_to.is_some() {
                // Its goal on that agent's stack, met; the agent's others go on.
                let goal = crate::goals::get(hub, a).await.ok().and_then(|s| goal_of_step(&s, run, row).filter(|g| g.status != "met").map(|g| g.id.clone()));
                if let Some(id) = goal {
                    let _ = crate::goals::done(hub, a, Some(id.as_str()), None, &By::You).await;
                }
            } else {
                let _ = crate::goals::control(hub, a, crate::goals::Action::Clear).await;
            }
        }
        set_step(hub, run, row.idx, "met", Some(format!("met, said {by}"))).await?;
        stacked(hub, run, &head, row, rows.len()).await;
        set_run(hub, run, "running", None).await?;
    }
    hub.store.audit(by.to_owned(), "plan.run.met".into(), format!("{run} step {n}")).await;
    if let Err(e) = advance(hub, run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    view(hub, run).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run's sequencing on a hub of its own, under a timeout: in a plan whose steps don't say
    /// what they wait for, each step's agent starts once the step before is met, one unmet stops the
    /// run, a retry gives it a new agent, and all met it's done. No harness runs: the steps' folder
    /// doesn't exist, and each step's goal is marked met in the store as its goal stack would.
    #[test]
    fn steps_run_one_after_another_each_once_the_one_before_is_met() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let h = Arc::clone(&hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::registry::Registry::run_local(h).await;
                });
                hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
                let items = ["Write the engine", "Wire it", "Screenshot it"].iter().map(|t| Item { title: (*t).to_owned(), ..Item::default() }).collect();
                let p = super::super::create(&hub, super::super::New { title: "Ship sync".into(), items, ..Default::default() }, "admin").await?;
                let cfg = Config {
                    place: Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some("/nonexistent/kiln-plan-test".into()), ..Place::default() },
                    opener: Some("owner".into()),
                    actor: "admin".into(),
                    ..Config::default()
                };
                let started = start(&hub, &p, cfg, None).await?;
                anyhow::ensure!(started.steps == 3 && started.note.is_none(), "each step waits for the one before: nothing side by side to say");
                let run = started.run;
                let meet = |agent: String| {
                    let hub = Arc::clone(&hub);
                    async move { hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'met' WHERE agent = ?1", params![agent])?)).await }
                };
                let v = view(&hub, &run).await?;
                let stands: Vec<&str> = v.steps.iter().map(|s| s.stand.as_str()).collect();
                anyhow::ensure!(stands == ["working", "waiting", "waiting"], "only the first step's agent starts: {stands:?}");
                let a1 = v.steps.first().and_then(|s| s.agent_id.clone()).ok_or_else(|| anyhow::anyhow!("no agent for step 1"))?;
                let info = hub.harness.info(&a1).ok_or_else(|| anyhow::anyhow!("not listed"))?;
                anyhow::ensure!(info.plan_run.as_deref() == Some(run.as_str()) && info.name.starts_with("Step 1: Write the engine"), "{info:?}");
                let goals = crate::goals::get(&hub, &a1).await?;
                anyhow::ensure!(goals.items.first().is_some_and(|g| g.text.starts_with("Step 1 of 3 of the plan \"Ship sync\": Write the engine")), "held to its step as a goal");
                // Met: the next starts; a stalled one doesn't move.
                advance(&hub, &run).await?;
                anyhow::ensure!(view(&hub, &run).await?.steps.get(1).is_some_and(|s| s.stand == "waiting"), "step 2 waits for step 1");
                meet(a1).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.met == 1 && v.steps.get(1).is_some_and(|s| s.stand == "working"), "{:?}", v.steps.iter().map(|s| &s.stand).collect::<Vec<_>>());
                let a2 = v.steps.get(1).and_then(|s| s.agent_id.clone()).unwrap_or_default();
                meet(a2).await?;
                advance(&hub, &run).await?;
                // Step 3's agent ends before it's met: unmet, and the run stops, saying why.
                let a3 = view(&hub, &run).await?.steps.get(2).and_then(|s| s.agent_id.clone()).unwrap_or_default();
                hub.harness.close(&hub, &a3).await;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.state == "stopped" && v.steps.get(2).is_some_and(|s| s.stand == "unmet") && v.reason.as_deref().is_some_and(|r| r.contains("step 3 wasn't met")), "{:?} {:?}", v.state, v.reason);
                // Retried: a new agent, and the run goes on to done.
                let v = retry(&hub, &run, 3, Some(1), "admin").await?;
                let a3b = v.steps.get(2).and_then(|s| s.agent_id.clone()).unwrap_or_default();
                anyhow::ensure!(v.state == "running" && a3b != a3 && v.steps.get(2).is_some_and(|s| s.tries == 2), "a new agent for the step");
                // Asked again for the try it replaced (its answer lost, CP-T3.2): nothing more starts.
                let again = retry(&hub, &run, 3, Some(1), "admin").await?;
                anyhow::ensure!(again.steps.get(2).is_some_and(|s| s.tries == 2 && s.agent_id.as_deref() == Some(a3b.as_str())), "the retry asked again started another: {:?}", again.steps.get(2).map(|s| (s.tries, s.agent_id.clone())));
                meet(a3b).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.state == "done" && v.met == 3, "{:?}", v.state);
                // Listed, with how many of its steps are met.
                let all = list(&hub, false).await?;
                anyhow::ensure!(all.first().is_some_and(|b| b.id == run && b.met == 3 && b.total == 3 && b.title == "Ship sync"));
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the run hung"),
        }
    }

    #[test]
    fn a_run_whose_steps_cant_start_where_theyre_placed_is_refused_before_it_starts() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let offering = |ids: &[&str]| {
                    crate::harness::kinds::offered()
                        .into_iter()
                        .map(|mut k| {
                            k.available = ids.contains(&k.id.as_str());
                            k
                        })
                        .collect::<Vec<_>>()
                };
                for (name, os) in [("box", "linux"), ("studio", "macos")] {
                    hub.agents.lock().insert(name.into(), crate::hub::tests::live_runner(name, os, "aarch64"));
                }
                hub.harness.set_kinds("local", offering(&["claude"]));
                hub.harness.set_kinds("agent:box", offering(&["claude", "devin"]));
                hub.harness.set_kinds("agent:studio", offering(&["claude"]));
                let items = ["Schema", "API"].iter().map(|t| Item { title: (*t).to_owned(), ..Item::default() }).collect();
                let p = super::super::create(&hub, super::super::New { title: "Placed".into(), items, ..Default::default() }, "admin").await?;
                let place = |on: &str| Place { on: Some(on.into()), kind: Some("devin".into()), cwd: Some("/nonexistent/kiln-plan-placed".into()), ..Place::default() };
                let cfg = |place: Place, steps: Vec<Place>| Config { place, steps, opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let refused = |r: R<Started>| match r {
                    Err(Error::Refused(code, why)) => Some((code, why)),
                    _ => None,
                };
                let (code, why) = refused(start(&hub, &p, cfg(place("studio"), Vec::new()), None).await).ok_or_else(|| anyhow::anyhow!("a run whose machine hasn't its harness started"))?;
                anyhow::ensure!(code == http::StatusCode::CONFLICT && why == "step 1: Devin isn't installed on studio: pick a machine that has it with `on`: box", "{why}");
                let own = vec![Place::default(), Place { on: Some("studio".into()), ..Place::default() }];
                let why = refused(start(&hub, &p, cfg(place("box"), own), None).await).map(|(_, why)| why).unwrap_or_default();
                anyhow::ensure!(why == "step 2: Devin isn't installed on studio: pick a machine that has it with `on`: box", "{why}");
                anyhow::ensure!(list(&hub, false).await?.is_empty(), "no run was made");
                hub.harness.set_kinds("local", offering(&["devin"]));
                let first = start(&hub, &p, cfg(place("local"), Vec::new()), Some("k")).await?;
                hub.harness.set_kinds("local", offering(&["claude"]));
                let again = start(&hub, &p, cfg(place("local"), Vec::new()), Some("k")).await?;
                anyhow::ensure!(again.run == first.run, "asked again under its key, it's the run it started then");
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the run hung"),
        }
    }

    /// On a hub of its own, under a timeout (`f`, its hub; agents start with no harness to run).
    fn on_a_hub(f: impl FnOnce(Arc<Hub>) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
                let h = Arc::clone(&hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::registry::Registry::run_local(h).await;
                });
                hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
                f(hub).await
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the run hung"),
        }
    }

    /// Step `n`'s goal (of run `run`) said met on its agent's stack, as its goal stack would.
    async fn meet(hub: &Hub, run: &str, n: usize) -> anyhow::Result<()> {
        let v = view(hub, run).await?;
        let agent = v.steps.get(n - 1).and_then(|s| s.agent_id.clone()).ok_or_else(|| anyhow::anyhow!("step {n} has no agent"))?;
        let stack = crate::goals::get(hub, &agent).await?;
        let goal = stack
            .items
            .iter()
            .find(|i| i.step.as_ref().is_some_and(|s| s.run.as_deref() == Some(run) && s.n == n))
            .map(|i| i.id.clone())
            .ok_or_else(|| anyhow::anyhow!("step {n}'s goal isn't on its agent's stack: {stack:?}"))?;
        crate::goals::done(hub, &agent, Some(goal.as_str()), Some("its tests pass"), &By::Itself).await?;
        Ok(())
    }

    fn stands(v: &Run) -> Vec<String> {
        v.steps.iter().map(|s| s.stand.clone()).collect()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// A plan whose steps say what they wait for runs as their graph says: steps that don't wait
    /// for each other side by side (the run saying so, and that they share a folder), each once all
    /// it waits for are met; a step not met holds up only the steps that wait for it, the rest going
    /// on, and with nothing left at work the run stops, saying which; retried, it goes on to done.
    #[test]
    fn steps_that_don_t_wait_for_each_other_run_side_by_side_as_their_graph_says() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let it = |t: &str, needs: &[usize]| Item { title: t.into(), needs: needs.to_vec(), ..Item::default() };
                let items = vec![it("Schema", &[]), it("API", &[1]), it("UI", &[1]), it("Docs", &[]), it("Ship", &[2, 3])];
                let p = super::super::create(&hub, super::super::New { title: "Graph".into(), items, ..Default::default() }, "admin").await?;
                let place = Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some("/nonexistent/kiln-plan-dag".into()), ..Place::default() };
                let started = start(&hub, &p, Config { place, opener: Some("owner".into()), actor: "admin".into(), ..Config::default() }, None).await?;
                let note = started.note.clone().unwrap_or_default();
                anyhow::ensure!(note.contains("steps 1 and 4; steps 2 and 3") && note.contains("one folder"), "{note}");
                let run = started.run;
                // Those that wait for none start at once.
                let v = view(&hub, &run).await?;
                anyhow::ensure!(stands(&v) == strs(&["working", "waiting", "waiting", "working", "waiting"]), "{:?}", stands(&v));
                anyhow::ensure!(v.steps.get(4).is_some_and(|s| s.needs == [2, 3] && !s.given), "{:?}", v.steps.get(4).map(|s| &s.needs));
                // Step 1 met: steps 2 and 3, which wait only for it, start together.
                meet(&hub, &run, 1).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(stands(&v) == strs(&["met", "working", "working", "working", "waiting"]), "{:?}", stands(&v));
                let (a2, a3) = (v.steps.get(1).and_then(|s| s.agent_id.clone()), v.steps.get(2).and_then(|s| s.agent_id.clone()));
                anyhow::ensure!(a2.is_some() && a3.is_some() && a2 != a3, "an agent each");
                // Step 3 met, step 2's agent gone: 5 waits on 2, step 4 goes on, and the run with it.
                meet(&hub, &run, 3).await?;
                hub.harness.close(&hub, a2.as_deref().unwrap_or_default()).await;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.state == "running" && stands(&v) == strs(&["met", "unmet", "met", "working", "waiting"]), "{} {:?}", v.state, stands(&v));
                // Step 4 met: nothing at work, and step 2 holds up the rest: stopped, saying so.
                meet(&hub, &run, 4).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.state == "stopped" && v.reason.as_deref().is_some_and(|r| r.starts_with("step 2 wasn't met")), "{} {:?}", v.state, v.reason);
                // Retried: a new agent for step 2 only; met, step 5 starts; met, done.
                let v = retry(&hub, &run, 2, None, "admin").await?;
                anyhow::ensure!(v.state == "running" && stands(&v) == strs(&["met", "working", "met", "met", "waiting"]) && v.steps.get(1).is_some_and(|s| s.tries == 2), "{:?}", stands(&v));
                meet(&hub, &run, 2).await?;
                advance(&hub, &run).await?;
                anyhow::ensure!(stands(&view(&hub, &run).await?) == strs(&["met", "met", "met", "met", "working"]));
                meet(&hub, &run, 5).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(v.state == "done" && v.met == 5, "{}", v.state);
                // A step's goal said met with what shows it: kept as the step's reason.
                anyhow::ensure!(v.steps.first().and_then(|s| s.reason.as_deref()) == Some("the agent said so: its tests pass"), "{:?}", v.steps.first().map(|s| &s.reason));
                Ok(())
            })
        })
    }

    /// The plan an agent proposes (plan mode) is kept as one of kiln's plans: one an agent, a new
    /// version each time it proposes another, the same words again changing nothing.
    #[test]
    fn an_agents_proposed_plan_is_kept_one_an_agent() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let keep = |agent: &'static str, words: &'static str| super::super::keep_proposed(&hub, agent, words);
                let first = keep("a1", "# Fix the mount\n\n1. Read the log\n2. Fix the rule").await?;
                anyhow::ensure!(first.title == "Fix the mount" && first.version == 1 && first.steps.len() == 2, "{first:?}");
                anyhow::ensure!(first.source.as_ref().is_some_and(|s| s.proposed && s.session.as_deref() == Some("a1")), "{:?}", first.source);
                let same = keep("a1", "# Fix the mount\n\n1. Read the log\n2. Fix the rule\n").await?;
                anyhow::ensure!(same.id == first.id && same.version == 1, "the same words change nothing: {same:?}");
                let next = keep("a1", "# Fix the mount, again\n\n1. Read the log\n2. Fix the rule\n3. Test it").await?;
                anyhow::ensure!(next.id == first.id && next.version == 2 && next.title == "Fix the mount, again" && next.steps.len() == 3, "{next:?}");
                let other = keep("a2", "1. Something else").await?;
                anyhow::ensure!(other.id != first.id, "one an agent");
                Ok(())
            })
        })
    }

    /// A run of some of a plan's steps (`only`) keeps each step's number; the steps they wait for
    /// outside the run are taken as done (and the run says so), those inside it still wait.
    #[test]
    fn a_run_of_some_steps_keeps_their_numbers_and_takes_the_rest_as_done() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let item = |t: &str, needs: Vec<usize>| Item { title: t.into(), needs, ..Item::default() };
                let items = vec![item("Schema", vec![]), item("Server", vec![1]), item("Client", vec![2]), item("Docs", vec![])];
                let p = super::super::create(&hub, super::super::New { title: "Some".into(), items, ..Default::default() }, "admin").await?;
                let place = Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some("/nonexistent/kiln-plan-some".into()), ..Place::default() };
                let cfg = Config { place, only: Some(vec![1, 2]), opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let started = start(&hub, &p, cfg, None).await?;
                anyhow::ensure!(started.steps == 2, "{started:?}");
                anyhow::ensure!(started.note.as_deref().is_some_and(|n| n.contains("Not in this run, taken as done: step 2 waits for step 1.")), "{:?}", started.note);
                let v = view(&hub, &started.run).await?;
                let ns: Vec<usize> = v.steps.iter().map(|s| s.n).collect();
                anyhow::ensure!(ns == [2, 3], "{ns:?}");
                anyhow::ensure!(stands(&v) == strs(&["working", "waiting"]), "{:?}", stands(&v));
                Ok(())
            })
        })
    }

    /// Run side by side, steps of a plan that doesn't say what they wait for start together, as
    /// many at once as `max_parallel` lets: the next starts as one ends.
    #[test]
    fn side_by_side_runs_at_most_max_parallel_steps_at_once() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let items = ["One", "Two", "Three"].iter().map(|t| Item { title: (*t).to_owned(), ..Item::default() }).collect();
                let p = super::super::create(&hub, super::super::New { title: "Fan out".into(), items, ..Default::default() }, "admin").await?;
                let place = Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some("/nonexistent/kiln-plan-fan".into()), ..Place::default() };
                let cfg = Config { place, parallel: true, max_parallel: Some(2), opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let run = start(&hub, &p, cfg, None).await?.run;
                anyhow::ensure!(stands(&view(&hub, &run).await?) == strs(&["working", "working", "waiting"]));
                meet(&hub, &run, 2).await?;
                advance(&hub, &run).await?;
                anyhow::ensure!(stands(&view(&hub, &run).await?) == strs(&["working", "met", "working"]));
                Ok(())
            })
        })
    }

    /// docs/agentic.md P2: run by an agent held to fewer subagents than `[agents] max_children` (its
    /// Subagents setting), steps side by side wait for room within it, rather than fail to start.
    #[test]
    fn p2_side_by_side_waits_for_room_within_the_parent_s_subagents_setting() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let folder = tempfile::tempdir()?;
                let cwd = folder.path().to_string_lossy().into_owned();
                let spec = crate::harness::session::Spec { kind: "claude".into(), cwd: Some(cwd.clone()), mode: Some("default".into()), name: Some("Main".into()), max_subagents: Some(1), ..Default::default() };
                let main = hub.harness.open(&hub, "local", spec, 1).await?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("no agent"))?;
                for _ in 0..200 {
                    if hub.harness.info(&main).is_some_and(|i| i.max_subagents == Some(1)) {
                        break;
                    }
                    crate::rt::sleep(Duration::from_millis(20)).await;
                }
                let items = ["One", "Two", "Three"].iter().map(|t| Item { title: (*t).to_owned(), ..Item::default() }).collect();
                let p = super::super::create(&hub, super::super::New { title: "Fan out".into(), items, ..Default::default() }, "admin").await?;
                let place = Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some(cwd), ..Place::default() };
                let cfg = Config { place, parallel: true, main: Some(main.clone()), parent: Some(main), opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let v = view(&hub, &start(&hub, &p, cfg, None).await?.run).await?;
                anyhow::ensure!(stands(&v) == strs(&["working", "waiting", "waiting"]), "{:?} {:?}", stands(&v), v.steps.iter().map(|s| &s.reason).collect::<Vec<_>>());
                Ok(())
            })
        })
    }

    /// A step given to the run's main agent (the agent that ran the plan) goes on that agent's own
    /// goal stack, saying the step it's made from (so the agent splits it into its acceptance
    /// criteria and tests), and no agent is started for it; met there, the next step starts with an
    /// agent of its own, the main agent's child. Stopped, that step's agent is let go, while the main
    /// agent keeps its other goals.
    #[test]
    fn a_step_given_to_the_main_agent_goes_on_its_own_goal_stack() -> anyhow::Result<()> {
        on_a_hub(|hub| {
            Box::pin(async move {
                let folder = tempfile::tempdir()?;
                let cwd = folder.path().to_string_lossy().into_owned();
                let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
                let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(cwd.clone()), mode: Some("default".into()), name: Some("Main".into()), count: Some(1), ..Ask::default() };
                let main = crate::harness::lineage::start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?.id;
                crate::goals::add(&hub, &main, vec!["keep the docs current".into()], false).await?;
                let crit = |t: &str| super::super::Criterion::Qualitative { text: t.into() };
                let first = Item {
                    title: "Lexer".into(),
                    acceptance: vec![crit("numbers scan"), crit("strings scan")],
                    testing: vec![super::super::Test { scenario: "hex".into(), criteria: vec![crit("0x1F is 31")] }],
                    ..Item::default()
                };
                let items = vec![first, Item { title: "Parser".into(), ..Item::default() }];
                let p = super::super::create(&hub, super::super::New { title: "Compiler".into(), items, ..Default::default() }, "admin").await?;
                let place = Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some(cwd.clone()), ..Place::default() };
                let steps = vec![Place { agent: Some("main".into()), ..Place::default() }];
                let cfg = Config { place, steps, main: Some(main.clone()), parent: Some(main.clone()), opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let run = start(&hub, &p, cfg, None).await?.run;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(stands(&v) == strs(&["working", "waiting"]) && v.steps.first().is_some_and(|s| s.given && s.agent_id.as_deref() == Some(main.as_str())), "{:?}", stands(&v));
                anyhow::ensure!(hub.harness.list().iter().all(|i| i.parent.as_deref() != Some(main.as_str())), "no agent started for a step given to the main agent");
                // On its stack after its own goal, saying the step: it splits into its 2 criteria and 1 test.
                let s = crate::goals::get(&hub, &main).await?;
                let goal = s
                    .items
                    .iter()
                    .find(|i| i.step.as_ref().is_some_and(|st| st.run.as_deref() == Some(run.as_str()) && st.n == 1))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("not on its stack: {s:?}"))?;
                anyhow::ensure!(s.items.first().is_some_and(|i| i.text == "keep the docs current") && goal.text.starts_with("Step 1 of 2 of the plan \"Compiler\": Lexer"), "{s:?}");
                let s = crate::goals::split(&hub, &main, Some(goal.id.as_str()), Vec::new()).await?;
                let parts: Vec<&str> = s.items.iter().filter(|i| i.part_of.as_deref() == Some(goal.id.as_str())).map(|i| i.text.as_str()).collect();
                anyhow::ensure!(
                    parts
                        == [
                            "\"Lexer\" (step 1 of the plan \"Compiler\"), acceptance criterion 1 of 2: numbers scan",
                            "\"Lexer\" (step 1 of the plan \"Compiler\"), acceptance criterion 2 of 2: strings scan",
                            "\"Lexer\" (step 1 of the plan \"Compiler\"), test 1 of 1: hex — passes when 0x1F is 31"
                        ],
                    "{parts:?}"
                );
                // Its parts met don't meet the step; the step's own goal met does.
                for part in s.items.iter().filter(|i| i.part_of.as_deref() == Some(goal.id.as_str())) {
                    crate::goals::done(&hub, &main, Some(part.id.as_str()), None, &By::Itself).await?;
                }
                advance(&hub, &run).await?;
                anyhow::ensure!(stands(&view(&hub, &run).await?) == strs(&["working", "waiting"]));
                crate::goals::done(&hub, &main, Some(goal.id.as_str()), Some("lexer tests pass"), &By::Itself).await?;
                advance(&hub, &run).await?;
                let v = view(&hub, &run).await?;
                anyhow::ensure!(stands(&v) == strs(&["met", "working"]), "{:?}", stands(&v));
                let child = v.steps.get(1).and_then(|s| s.agent_id.clone()).unwrap_or_default();
                anyhow::ensure!(child != main && hub.harness.info(&child).is_some_and(|i| i.parent.as_deref() == Some(main.as_str())), "step 2's agent is the main agent's child");
                // Stopped: step 2's agent let go; the main agent's own goal stays.
                let v = stop(&hub, &run, "admin").await?;
                anyhow::ensure!(v.state == "stopped" && stands(&v) == strs(&["met", "stopped"]));
                anyhow::ensure!(crate::goals::get(&hub, &main).await?.items.iter().any(|i| i.text == "keep the docs current" && matches!(i.status.as_str(), "pending" | "active")));
                Ok(())
            })
        })
    }

    #[test]
    fn a_step_given_to_the_main_agent_needs_one() {
        let cfg = Config { steps: vec![Place { agent: Some("main".into()), ..Place::default() }], ..Config::default() };
        let need = |c: &Config| matches!(c.steps.first().and_then(|s| s.agent.as_deref()), Some("main"));
        assert!(need(&cfg) && cfg.main.is_none());
        assert_eq!(steps_words(&[1]), "step 2");
        assert_eq!(steps_words(&[1, 2]), "steps 2 and 3");
        assert_eq!(steps_words(&[0, 1, 2]), "steps 1, 2 and 3");
    }

    fn plan(items: Vec<Item>, steps: Vec<String>) -> Plan {
        Plan {
            id: "p".into(),
            title: "Retry rework".into(),
            body: String::new(),
            tags: vec![],
            source: None,
            share: None,
            version: 1,
            created_at: 0,
            updated_at: 0,
            steps,
            items,
            runs: 0,
            phase: "ready".into(),
            research: None,
        }
    }

    #[test]
    fn a_step_s_goal_says_where_it_stands_and_what_makes_it_done() {
        let it = Item { title: "Split the scanner".into(), acceptance: vec![super::super::Criterion::Qualitative { text: "scan_number exists".into() }], ..Item::default() };
        let p = plan(vec![it.clone(), Item { title: "Ship it".into(), ..Item::default() }], vec![it.text(), "Ship it".into()]);
        assert_eq!(goal_of(&p, 0, 2), "Step 1 of 2 of the plan \"Retry rework\": Split the scanner — Done when: scan_number exists");
        assert_eq!(goal_of(&p, 1, 2), "Step 2 of 2 of the plan \"Retry rework\": Ship it");
        assert_eq!(title_of(&p, 0), "Split the scanner");
        // A plan of text: its list's items.
        let t = plan(vec![], vec!["Write the engine with tests".into()]);
        assert_eq!(goal_of(&t, 0, 1), "Step 1 of 1 of the plan \"Retry rework\": Write the engine with tests");
        let long = plan(vec![], vec!["x ".repeat(3000)]);
        assert!(goal_of(&long, 0, 1).chars().count() <= GOAL + 1, "a goal is one line of at most 2000 characters");
    }
}
