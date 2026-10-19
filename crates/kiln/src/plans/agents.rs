//! A plan run as one agent per step (docs/plans.md, "One agent per step"; docs/agentic.md P).
//!
//! Each step gets an agent of its own, held to the step as a goal (`crate::goals`): its title,
//! what to do, examples, "Done when:" its acceptance criteria and "Tests:" its tests
//! (`Item::goal`), on the machine and harness chosen for the run or for the step. Steps run one
//! after another: a step's agent starts once the step before is met. Asked to run them side by
//! side (`parallel`), kiln still runs them one at a time, and says why: in one plain folder they'd
//! work over each other, and steps side by side need workspaces of their own (docs/agentic.md, W).
//!
//! An agent that runs a plan this way is the parent of its steps' agents, which get no more than
//! it has (`crate::harness::lineage`). The server drives the runs ([`drive`]): it starts each
//! step's agent, follows its goal to met or unmet and goes on, with no console open and across
//! restarts. A person stops a run, retries a step (a new agent for it), or takes a step over (kiln
//! stops holding its agent to the goal, and they say when it's met).

use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Error, Item, Plan, R};
use crate::harness::lineage::{Ask, Starter};
use crate::harness::model::Command;
use crate::hub::Hub;

/// Steps one run has at the most.
const STEPS: usize = 100;
/// A goal's longest (as `crate::goals` keeps them).
const GOAL: usize = 1990;
/// How often the server looks at the runs under way.
const EVERY: Duration = Duration::from_secs(2);
/// Why steps asked to run side by side run one at a time, for now.
pub const ONE_AT_A_TIME: &str = "These steps run one at a time: in one plain folder, side by side they'd work over each other. Steps run together once agents have workspaces of their own (docs/agentic.md, W).";

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
}

/// How a run's steps run, and who ran it (kept with the run).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where its steps run, unless a step says otherwise.
    pub place: Place,
    /// Each step's own, by its number from 0.
    pub steps: Vec<Place>,
    /// Asked to run side by side (it runs one at a time for now: [`ONE_AT_A_TIME`]).
    pub parallel: bool,
    /// The agent that ran it: its steps' agents are its children.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Whom the steps' agents act for (`owner`, `user:<id>`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opener: Option<String>,
    /// Who ran it, as the audit trail names them.
    pub actor: String,
}

/// One step of a run, as it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Step {
    /// Its number, from 1.
    pub n: usize,
    pub title: String,
    /// What its agent is held to.
    pub goal: String,
    /// `waiting` (its turn hasn't come), `working`, `needs_you` (its agent asks a person),
    /// `yours` (taken over), `met`, `unmet`, `stopped`.
    pub stand: String,
    /// Its agent (the latest, after a retry), as `agents.*` show agents.
    pub agent: Option<Value>,
    pub agent_id: Option<String>,
    pub started_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub reason: Option<String>,
    /// Agents it took (1 more each retry).
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
    /// Said of how it runs (steps asked side by side run one at a time).
    pub note: Option<String>,
    pub at: i64,
    pub ended_at: Option<i64>,
    pub by: String,
    pub config: Config,
    pub steps: Vec<Step>,
    pub met: usize,
    pub total: usize,
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
    let line = format!("Step {} of {} of the plan \"{}\": {}", i + 1, n, p.title.trim(), step).split_whitespace().collect::<Vec<_>>().join(" ");
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

/// Runs plan `p` as one agent per step, as `cfg` says. The first step's agent starts at once.
pub async fn start(hub: &Arc<Hub>, p: &Plan, cfg: Config) -> R<Started> {
    let n = p.steps.len().min(STEPS);
    if n == 0 {
        return Err(Error::Bad("this plan has no steps (no list in it) to give agents".into()));
    }
    if cfg.place.kind.as_deref().is_none_or(|k| k.trim().is_empty()) && cfg.steps.iter().take(n).any(|s| s.kind.is_none()) {
        return Err(Error::Bad("say which harness runs its steps: `new.kind` (or each step's `kind`)".into()));
    }
    let note = cfg.parallel.then(|| ONE_AT_A_TIME.to_owned());
    let steps: Vec<(String, String)> = (0..n).map(|i| (title_of(p, i), goal_of(p, i, n))).collect();
    let (run, now, version, plan_id, by) = (crate::util::new_id(), crate::util::now_unix(), p.version, p.id.clone(), cfg.actor.clone());
    let config = serde_json::to_string(&cfg).map_err(anyhow::Error::from)?;
    let (run_c, note_c) = (run.clone(), note.clone());
    hub.store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by, state, config, note) VALUES (?1, ?2, ?3, '', 'agents', ?4, ?5, 'running', ?6, ?7)",
                params![run_c, plan_id, version, now, by, config, note_c],
            )?;
            for (i, (title, goal)) in steps.iter().enumerate() {
                tx.execute("INSERT INTO plan_run_steps (run_id, idx, title, goal) VALUES (?1, ?2, ?3, ?4)", params![run_c, i64::try_from(i).unwrap_or(0), title, goal])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    // The first step's agent, now rather than at the next look.
    if let Err(e) = advance(hub, &run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    Ok(Started { run, steps: n, note })
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
    let mut st = c.prepare("SELECT idx, title, goal, status, agent, started_at, ended_at, reason, tries FROM plan_run_steps WHERE run_id = ?1 ORDER BY idx")?;
    let rows = st
        .query_map(params![run], |r| Ok(Row { idx: r.get(0)?, title: r.get(1)?, goal: r.get(2)?, status: r.get(3)?, agent: r.get(4)?, started_at: r.get(5)?, ended_at: r.get(6)?, reason: r.get(7)?, tries: r.get(8)? }))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some((head, rows)))
}

async fn load_run(hub: &Hub, run: &str) -> R<(Head, Vec<Row>)> {
    let r = run.to_owned();
    hub.store.call(move |c| load(c, &r)).await?.ok_or_else(|| Error::Bad(format!("no plan run {run}")))
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
    Ok(Run { id: run.to_owned(), plan: head.plan, title, version: head.version, state: head.state, reason: head.reason, note: head.note, at: head.at, ended_at: head.ended_at, by: head.by, config: head.config, total: steps.len(), met, steps })
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
    let (run, state, now) = (run.to_owned(), state.to_owned(), crate::util::now_unix());
    let ends = state != "running";
    hub.store
        .call(move |c| {
            c.execute("UPDATE plan_runs SET state = ?2, reason = ?3, ended_at = CASE WHEN ?4 THEN ?5 ELSE NULL END WHERE id = ?1", params![run, state, reason, ends, now])?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Starts step `row`'s agent, holds it to the step's goal, and notes it.
async fn begin(hub: &Hub, run: &str, head: &Head, row: &Row) -> R<()> {
    let at = usize::try_from(row.idx).unwrap_or(0);
    let own = head.config.steps.get(at).cloned().unwrap_or_default();
    let base = &head.config.place;
    let pick = |a: &Option<String>, b: &Option<String>| a.clone().filter(|s| !s.trim().is_empty()).or_else(|| b.clone().filter(|s| !s.trim().is_empty()));
    let parent = match head.config.parent.as_deref() {
        Some(p) => Some(hub.harness.info(p).ok_or_else(|| Error::Bad("the agent that ran this plan has ended, so its steps can't be held to it: run the plan again".into()))?),
        None => None,
    };
    let starter = Starter { parent, opener: head.config.opener.clone(), actor: head.config.actor.clone(), plan_run: Some(run.to_owned()) };
    let name: String = format!("Step {}: {}", at + 1, row.title).chars().take(80).collect();
    let ask = Ask {
        kind: pick(&own.kind, &base.kind).unwrap_or_default(),
        on: pick(&own.on, &base.on),
        cwd: pick(&own.cwd, &base.cwd),
        model: pick(&own.model, &base.model),
        effort: pick(&own.effort, &base.effort),
        mode: pick(&own.mode, &base.mode),
        name: Some(name),
        count: Some(1),
        ..Ask::default()
    };
    let infos = crate::harness::lineage::start(hub, &starter, ask).await.map_err(|(_, m)| Error::Bad(m))?;
    let agent = infos.first().map(|i| i.id.clone()).ok_or_else(|| Error::Bad("the step's agent didn't start".into()))?;
    let (run_c, idx, agent_c, now) = (run.to_owned(), row.idx, agent.clone(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE plan_run_steps SET status = 'started', agent = ?3, started_at = ?4, ended_at = NULL, reason = NULL, tries = tries + 1 WHERE run_id = ?1 AND idx = ?2",
                params![run_c, idx, agent_c, now],
            )?;
            Ok(())
        })
        .await?;
    crate::goals::add(hub, &agent, vec![row.goal.clone()], true).await.map_err(|e| Error::Bad(format!("its goal: {e}")))?;
    tracing::info!(%run, step = at + 1, %agent, "a plan step's agent started");
    Ok(())
}

/// How a started step's goal came out: `Some((met, why))` once it ended.
async fn outcome(hub: &Hub, row: &Row) -> R<Option<(bool, Option<String>)>> {
    let Some(agent) = row.agent.as_deref() else { return Ok(Some((false, Some("it has no agent".into())))) };
    if hub.harness.info(agent).is_none() {
        return Ok(Some((false, Some("its agent ended before the step was met".into()))));
    }
    let stack = crate::goals::get(hub, agent).await.map_err(|e| Error::Bad(e.to_string()))?;
    let item = stack.items.iter().find(|i| i.text.split_whitespace().eq(row.goal.split_whitespace())).or_else(|| stack.items.first());
    Ok(match item.map(|i| (i.status.as_str(), i.reason.clone())) {
        Some(("met", why)) => Some((true, why)),
        Some(("unmet", why)) => Some((false, why.or_else(|| Some("its agent gave up on it".into())))),
        Some(("skipped", _)) => Some((false, Some("its goal was skipped".into()))),
        None => Some((false, Some("its goal was cleared".into()))),
        Some(_) => None,
    })
}

/// Moves run `run` on: a step met lets the next one's agent start; one unmet stops the run (a
/// person retries it, or takes it over); all met, it's done.
pub async fn advance(hub: &Hub, run: &str) -> R<()> {
    let (head, rows) = load_run(hub, run).await?;
    if head.state != "running" {
        return Ok(());
    }
    for row in &rows {
        match row.status.as_str() {
            "met" => continue,
            "waiting" => {
                if let Err(e) = begin(hub, run, &head, row).await {
                    let why = format!("step {} couldn't start: {e}", row.idx + 1);
                    set_step(hub, run, row.idx, "unmet", Some(e.to_string())).await?;
                    set_run(hub, run, "stopped", Some(why)).await?;
                }
                return Ok(());
            }
            "started" => match outcome(hub, row).await? {
                Some((true, why)) => {
                    set_step(hub, run, row.idx, "met", why).await?;
                    tracing::info!(%run, step = row.idx + 1, "a plan step was met");
                    continue;
                }
                Some((false, why)) => {
                    set_step(hub, run, row.idx, "unmet", why.clone()).await?;
                    set_run(hub, run, "stopped", Some(format!("step {} wasn't met{}: retry it, or take it over", row.idx + 1, why.map(|w| format!(" ({w})")).unwrap_or_default()))).await?;
                    return Ok(());
                }
                None => return Ok(()),
            },
            // Taken over: the person says when it's met.
            "taken" => return Ok(()),
            _ => {
                set_run(hub, run, "stopped", Some(format!("step {} {}", row.idx + 1, row.status))).await?;
                return Ok(());
            }
        }
    }
    set_run(hub, run, "done", None).await?;
    tracing::info!(%run, "a plan run is done: every step met");
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

/// Stops a run: no next step starts, and the agents of the steps under way are interrupted and
/// no longer held to their goals (they stay, to look at or carry on with).
pub async fn stop(hub: &Hub, run: &str, by: &str) -> R<Run> {
    let (head, rows) = load_run(hub, run).await?;
    if head.state == "running" {
        for row in rows.iter().filter(|r| matches!(r.status.as_str(), "started" | "taken")) {
            if let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
                let _ = crate::goals::control(hub, a, crate::goals::Action::Pause).await;
                let _ = hub.harness.command(hub, a, Command::Interrupt).await;
            }
            set_step(hub, run, row.idx, "stopped", Some(format!("stopped by {by}"))).await?;
        }
        set_run(hub, run, "stopped", Some(format!("stopped by {by}"))).await?;
    }
    hub.store.audit(by.to_owned(), "plan.run.stop".into(), run.to_owned()).await;
    view(hub, run).await
}

fn step_row(rows: &[Row], n: usize) -> R<&Row> {
    let idx = n.checked_sub(1).and_then(|i| i64::try_from(i).ok()).ok_or_else(|| Error::Bad("steps count from 1".into()))?;
    rows.iter().find(|r| r.idx == idx).ok_or_else(|| Error::Bad(format!("the run has no step {n}")))
}

/// Tries step `n` again with a new agent (the one before, if it still runs, ends), and the run
/// goes on from there.
pub async fn retry(hub: &Hub, run: &str, n: usize, by: &str) -> R<Run> {
    let (_, rows) = load_run(hub, run).await?;
    let row = step_row(&rows, n)?;
    if row.status == "met" {
        return Err(Error::Bad(format!("step {n} is met already")));
    }
    if row.status == "waiting" {
        return Err(Error::Bad(format!("step {n} hasn't started yet")));
    }
    if let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
        crate::harness::ops::end(hub, a, by).await;
    }
    // The steps after it start over too: they come after it again.
    let (run_c, idx) = (run.to_owned(), row.idx);
    hub.store
        .call(move |c| {
            c.execute("UPDATE plan_run_steps SET status = 'waiting', reason = NULL, ended_at = NULL WHERE run_id = ?1 AND idx >= ?2 AND status != 'met'", params![run_c, idx])?;
            Ok(())
        })
        .await?;
    set_run(hub, run, "running", None).await?;
    hub.store.audit(by.to_owned(), "plan.run.retry".into(), format!("{run} step {n}")).await;
    if let Err(e) = advance(hub, run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    view(hub, run).await
}

/// A person takes step `n` over: kiln no longer holds its agent to the goal (they work with it
/// themselves), and the run waits until they say it's met.
pub async fn take(hub: &Hub, run: &str, n: usize, by: &str) -> R<Run> {
    let (_, rows) = load_run(hub, run).await?;
    let row = step_row(&rows, n)?;
    if !matches!(row.status.as_str(), "started" | "unmet" | "stopped") {
        return Err(Error::Bad(format!("step {n} isn't under way: there's nothing to take over")));
    }
    if let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
        let _ = crate::goals::control(hub, a, crate::goals::Action::Pause).await;
    }
    set_step(hub, run, row.idx, "taken", Some(format!("taken over by {by}"))).await?;
    set_run(hub, run, "running", None).await?;
    hub.store.audit(by.to_owned(), "plan.run.take".into(), format!("{run} step {n}")).await;
    view(hub, run).await
}

/// A person says step `n` is met (one they took over, or one they judge done): the next starts.
pub async fn met(hub: &Hub, run: &str, n: usize, by: &str) -> R<Run> {
    let (_, rows) = load_run(hub, run).await?;
    let row = step_row(&rows, n)?;
    if row.status == "waiting" {
        return Err(Error::Bad(format!("step {n} hasn't started yet")));
    }
    if let Some(a) = row.agent.as_deref().filter(|a| hub.harness.info(a).is_some()) {
        let _ = crate::goals::control(hub, a, crate::goals::Action::Clear).await;
    }
    set_step(hub, run, row.idx, "met", Some(format!("met, said {by}"))).await?;
    set_run(hub, run, "running", None).await?;
    hub.store.audit(by.to_owned(), "plan.run.met".into(), format!("{run} step {n}")).await;
    if let Err(e) = advance(hub, run).await {
        tracing::warn!(%run, error = %format!("{e:#}"), "plan run");
    }
    view(hub, run).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run's sequencing on a hub of its own, under a timeout: each step's agent starts once
    /// the step before is met, one unmet stops the run, a retry gives it a new agent, and all met
    /// it's done. No harness runs: the steps' folder doesn't exist, and each step's goal is
    /// marked met in the store as its goal stack would.
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
                let items = ["Write the engine", "Wire it", "Screenshot it"].iter().map(|t| Item { title: (*t).to_owned(), ..Item::default() }).collect();
                let p = super::super::create(&hub, super::super::New { title: "Ship sync".into(), items, ..Default::default() }, "admin").await?;
                let cfg = Config { place: Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some("/nonexistent/kiln-plan-test".into()), ..Place::default() }, parallel: true, opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
                let started = start(&hub, &p, cfg).await?;
                anyhow::ensure!(started.steps == 3 && started.note.as_deref() == Some(ONE_AT_A_TIME), "side by side asked for: one at a time, saying why");
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
                let v = retry(&hub, &run, 3, "admin").await?;
                let a3b = v.steps.get(2).and_then(|s| s.agent_id.clone()).unwrap_or_default();
                anyhow::ensure!(v.state == "running" && a3b != a3 && v.steps.get(2).is_some_and(|s| s.tries == 2), "a new agent for the step");
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

    fn plan(items: Vec<Item>, steps: Vec<String>) -> Plan {
        Plan { id: "p".into(), title: "Retry rework".into(), body: String::new(), tags: vec![], source: None, share: None, version: 1, created_at: 0, updated_at: 0, steps, items, runs: 0 }
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
