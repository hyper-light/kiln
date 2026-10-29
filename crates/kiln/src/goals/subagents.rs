//! A plan's steps side by side on a goal stack (docs/goals.md, "A plan's steps side by side";
//! docs/agentic.md P2). On a stack a plan's steps are goals, one after another; but a plan whose
//! steps say what they wait for (`needs`) says which of them can be under way at once. Its steps go
//! as its graph says: a step's goal waits until the steps it waits for have ended, and where more
//! than one of its steps can be under way at once, kiln hands each step that's ready to a subagent
//! of the stack's agent: an agent of its own (its child, in its harness, on its machine, in its
//! folder, its mode at most the agent's), started for the step and held to it on a goal stack of
//! its own. As many at once as there's room for in the agent's pool of subagents at work (its
//! Subagents setting, whatever each does; none set, no limit: `lineage::room`); the steps there's
//! no room for the agent takes itself, in turn, as it would have. Handing out and following take
//! any goal ([`hand`], [`follow_one`]): one made from no plan step goes as its words say.
//!
//! - **Met there, met here.** The step's goal on the stack (and its parts) ends met, saying which
//!   subagent met it and how; the subagent ends once it has settled, so it holds no room.
//! - **Not met there** (given up, skipped, its goal cleared, the subagent ended): the step comes back
//!   to the stack, and the agent takes it itself.
//! - **The stack paused** (by a person, by the agent, for usage): its steps' subagents pause, and
//!   resume with it. A step taken out of the stack, said met there, or the stack cleared: its
//!   subagent ends.
//!
//! A plan whose steps say nothing of waiting runs one after another, as before. Which subagent has
//! which step is kept with the step's goal (`Item::subagent`) and by the subagent (`MARK`), so the
//! driver follows each across restarts. What's decided here is the driver's, each tick ([`tick`]),
//! outside the stacks' lock, which each change takes as it goes; [`to_hand`] is pure, and what the
//! agent takes up next is its stack's graph's (`goals::graph`), with what's about to go to subagents
//! left for them.

use std::collections::HashMap;
use std::sync::LazyLock;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use super::{Action, At, By, CHANGING, Item, NewGoal, Pausing, Stack, descendants, renumber, touch};
use crate::harness::lineage::{Ask, Starter};
use crate::harness::model::Status;
use crate::harness::session::Info;
use crate::hub::Hub;

/// Where kiln keeps the step each subagent it started is for, by the subagent (`kv`).
const MARK: &str = "goals/subagent/";
/// After a subagent couldn't be started for an agent's steps, how long before kiln tries again: the
/// agent takes its steps itself meanwhile.
const RETRY_MS: i64 = 5 * 60_000;
/// A subagent's name, in characters at the most (as a plan run's step agents are named).
const NAME: usize = 80;

/// Who takes the first ready step of a plan when the agent has none of its steps under way: a
/// subagent, as each other ready step (`false`: a subagent per step, the agent taking only those
/// there's no room for), or the agent itself (`true`: one subagent fewer a round). One switch for
/// every stack.
const AGENT_FIRST: bool = false;

/// The goal a subagent was started for: the stack's agent and the goal there, the goal the subagent
/// holds it as on its own stack (`theirs`), and whether kiln paused the subagent's stack with the
/// stack it works for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Mark {
    agent: String,
    goal: String,
    theirs: String,
    paused: bool,
}

/// When starting a subagent last failed, by the stack's agent (unix ms).
static FAILED: LazyLock<parking_lot::Mutex<HashMap<String, i64>>> = LazyLock::new(Default::default);

/// What a stack's plan steps wait for, for the plans that say: each step's goal (its own, not one of
/// its parts) by id, with the goals of the steps it waits for in its run (a step of the plan not in
/// the run is taken as done); what a subagent of its is named and held to; and the steps about to go
/// to subagents, which the agent leaves for them ([`leave`]).
#[derive(Debug, Clone, Default)]
pub(super) struct Graph {
    waits: HashMap<String, Vec<String>>,
    briefs: HashMap<String, Brief>,
    handing: Vec<String>,
}

/// A step's subagent: its name, and the goal it's held to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Brief {
    name: String,
    goal: String,
}

impl Graph {
    fn has(&self, id: &str) -> bool {
        self.waits.contains_key(id)
    }

    fn is_empty(&self) -> bool {
        self.waits.is_empty()
    }

    /// What step goal `id` waits for, as its plan says (none: its plan says nothing of waiting, or
    /// it's no step).
    pub(super) fn waits_of(&self, id: &str) -> Option<&[String]> {
        self.waits.get(id).map(Vec::as_slice)
    }

    /// Step goal `id` is about to go to a subagent: the agent leaves it for one.
    pub(super) fn handing(&self, id: &str) -> bool {
        self.handing.iter().any(|h| h == id)
    }
}

/// A goal made from a plan's step in a run: the step's own goal, not one of its parts (its
/// acceptance criteria and tests, or parts its agent split it into).
fn is_step(i: &Item) -> bool {
    i.step.as_ref().is_some_and(|s| s.run.is_some()) && !matches!(i.part_kind.as_deref(), Some("checks" | "own"))
}

/// The step's goal `i` is, or is a part of (or a part of one of its parts).
fn unit_of<'a>(i: &'a Item, items: &'a [Item]) -> Option<&'a Item> {
    let mut at = i;
    for _ in 0..16 {
        if is_step(at) {
            return Some(at);
        }
        at = items.iter().find(|p| Some(p.id.as_str()) == at.part_of.as_deref())?;
    }
    None
}

fn open(i: &Item) -> bool {
    matches!(i.status.as_str(), "pending" | "active")
}

/// The run of the step `i` is (or is a part of), when its plan says what its steps wait for.
fn group_of<'a>(i: &'a Item, items: &'a [Item], g: &Graph) -> Option<&'a str> {
    let u = unit_of(i, items).filter(|u| g.has(&u.id))?;
    u.step.as_ref().and_then(|s| s.run.as_deref())
}

/// Every step step `u` waits for has ended here (met, or left behind: a stack goes on past a goal
/// skipped).
fn ready(u: &Item, items: &[Item], g: &Graph) -> bool {
    g.waits.get(&u.id).is_none_or(|w| w.iter().all(|d| items.iter().find(|i| i.id == *d).is_none_or(|i| !open(i))))
}

/// Whether kiln hands `info`'s steps to subagents now: not while it's paused at a usage limit, or
/// its machine is away, nor moments after a subagent couldn't be started for it (at `now`).
fn may_hand(info: &Info, now: i64) -> bool {
    info.pause.is_none() && info.status != Status::Disconnected && !FAILED.lock().get(&info.id).is_some_and(|at| now - at < RETRY_MS)
}

/// The steps of `info`'s stack about to go to subagents, noted in its graph: the agent leaves them
/// for them, and takes the steps there's no room for (`hub`: the agents it runs; at `now`).
pub(super) async fn leave(g: &mut Graph, hub: &Hub, stack: &Stack, info: &Info, now: i64) {
    if g.is_empty() || !may_hand(info, now) {
        return;
    }
    let room = crate::harness::lineage::room_of(hub, info).await.more();
    g.handing = to_hand(stack, g, room, AGENT_FIRST).into_iter().map(|i| i.id.clone()).collect();
}

/// The subagents kiln started and follows itself (by the marks it keeps): driven as the agents
/// whose stacks run are, so neither kept going nor news for the agent they work for.
pub(super) async fn held(hub: &Hub) -> Vec<String> {
    hub.store.kv_prefix(MARK.to_owned()).await.unwrap_or_default().into_iter().filter_map(|(k, _)| k.strip_prefix(MARK).map(str::to_owned)).collect()
}

/// A step none of which has been started here, nor handed to a subagent: still the stack's to hand.
fn untouched(u: &Item, items: &[Item]) -> bool {
    u.status == "pending" && u.subagent.is_none() && u.on.is_none() && !u.back && {
        let parts = descendants(items, &u.id);
        items.iter().filter(|p| parts.contains(&p.id)).all(|p| p.status == "pending" && p.subagent.is_none())
    }
}

/// The steps to hand to subagents now, at most `room`: where more than one of a plan's steps can be
/// under way at once (those ready to start, with those under way here or on subagents), each one
/// ready that the agent hasn't started, in the stack's order; `agent_first`, the first of them left
/// to the agent when it has none of the plan's steps under way. Only the steps of the plan the
/// stack is at: its goal under way's, else its next goal's.
pub(super) fn to_hand<'a>(stack: &'a Stack, g: &Graph, room: usize, agent_first: bool) -> Vec<&'a Item> {
    if room == 0 || g.is_empty() {
        return Vec::new();
    }
    let items = stack.items.as_slice();
    let first = items.iter().find(|i| i.status == "active").or_else(|| items.iter().find(|i| i.status == "pending" && i.on.is_none()));
    let Some(run) = first.and_then(|i| group_of(i, items, g)) else { return Vec::new() };
    let steps: Vec<&Item> = items.iter().filter(|i| is_step(i) && open(i) && group_of(i, items, g) == Some(run)).collect();
    let can: Vec<&Item> = steps.iter().copied().filter(|u| untouched(u, items) && ready(u, items, g)).collect();
    // Under way: on a subagent, or the agent's (started here, or back from a subagent and ready: it
    // takes those itself).
    let started = |u: &Item| u.status == "active" || descendants(items, &u.id).iter().any(|d| items.iter().any(|p| p.id == *d && p.status != "pending"));
    let agents = |u: &&Item| u.subagent.is_none() && (started(u) || (u.back && ready(u, items, g)));
    let under_way = steps.iter().filter(|u| u.subagent.is_some() || agents(u)).count();
    if can.len() + under_way < 2 {
        return Vec::new();
    }
    let left = usize::from(agent_first && !steps.iter().any(agents));
    can.into_iter().skip(left).take(room).collect()
}

/// The graph of `stack`'s plan steps, from the versions of their plans run (empty for a stack with
/// no step still to go of a plan that says what its steps wait for).
pub(super) async fn graph_of(store: &crate::store::Store, stack: &Stack) -> anyhow::Result<Graph> {
    if !stack.items.iter().any(|i| open(i) && is_step(i)) {
        return Ok(Graph::default());
    }
    let steps: Vec<Item> = stack.items.iter().filter(|i| is_step(i)).cloned().collect();
    store.call(move |c| graph_in(c, &steps)).await
}

/// [`graph_of`], in the store: each run's plan version read once. A plan gone, or one whose steps
/// say nothing of waiting, leaves its steps out (they go in the stack's order).
fn graph_in(c: &rusqlite::Connection, steps: &[Item]) -> anyhow::Result<Graph> {
    let mut g = Graph::default();
    let mut runs: Vec<(String, i64, String)> = Vec::new();
    for s in steps.iter().filter_map(|i| i.step.as_ref()) {
        let k = (s.plan.clone(), s.version, s.run.clone().unwrap_or_default());
        if !runs.contains(&k) {
            runs.push(k);
        }
    }
    for (plan, version, run) in runs {
        let row: Option<(String, String)> =
            c.query_row("SELECT items, title FROM plan_versions WHERE plan_id = ?1 AND version = ?2", params![plan, version], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((items, title)) = row else { continue };
        let items: Vec<crate::plans::Item> = serde_json::from_str(&items).unwrap_or_default();
        if !items.iter().any(|it| !it.needs.is_empty()) {
            continue;
        }
        let n = items.len();
        let waits = crate::plans::waits(&items, n, false);
        let of_run: Vec<&Item> = steps.iter().filter(|i| i.step.as_ref().is_some_and(|s| s.plan == plan && s.version == version && s.run.as_deref() == Some(run.as_str()))).collect();
        let goal_of = |k: usize| of_run.iter().rev().find(|i| i.step.as_ref().is_some_and(|s| s.n == k)).map(|i| i.id.clone());
        for i in &of_run {
            let Some(k) = i.step.as_ref().map(|s| s.n).filter(|k| (1..=n).contains(k)) else { continue };
            let w: Vec<String> = waits.get(k - 1).into_iter().flatten().filter_map(|d| goal_of(d + 1)).filter(|d| *d != i.id).collect();
            g.waits.insert(i.id.clone(), w);
            let it = items.get(k - 1);
            let name: String = format!("Step {k}: {}", it.map_or("", |it| it.title.trim())).chars().take(NAME).collect();
            // Its words here (a person's rewording too), saying where it stands in its plan.
            let goal = if i.text.starts_with("Step ") { i.text.clone() } else { crate::plans::agents::step_goal(&title, k - 1, n, &i.text) };
            g.briefs.insert(i.id.clone(), Brief { name, goal });
        }
    }
    Ok(g)
}

/// The driver's look at a plan's steps side by side, each tick: the steps handed to subagents
/// followed ([`follow`]), then those ready handed out ([`hand_out`]).
pub(super) async fn tick(hub: &Hub, metrics: &crate::metrics::AgentMetrics) {
    follow(hub, metrics).await;
    hand_out(hub).await;
}

/// Each subagent kiln started for a step, followed ([`follow_one`]).
async fn follow(hub: &Hub, metrics: &crate::metrics::AgentMetrics) {
    let marks = match hub.store.kv_prefix(MARK.to_owned()).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "plan steps' subagents: not read");
            return;
        }
    };
    for (key, value) in marks {
        let Some(child) = key.strip_prefix(MARK) else { continue };
        let Ok(mut mark) = serde_json::from_str::<Mark>(&value) else {
            let _ = hub.store.kv_delete(key.clone()).await;
            continue;
        };
        if let Err(e) = follow_one(hub, metrics, child, &mut mark).await {
            tracing::warn!(agent = %mark.agent, subagent = %child, error = %format!("{e:#}"), "a goal's subagent");
        }
    }
}

/// Whether agent `id` still has a goal stack: an agent ended for good (`ops::end`, the console's
/// close) has none, as its stack goes with it.
async fn kept(hub: &Hub, id: &str) -> anyhow::Result<bool> {
    let id = id.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM goal_stacks WHERE agent = ?1)", params![id], |r| r.get::<_, bool>(0))?)).await
}

/// The goal `theirs` (a subagent's stack) holds goal `g` as: the one its mark names (`id`), else one
/// made from the same plan step.
fn theirs_for<'a>(theirs: &'a Stack, id: &str, g: &Item) -> Option<&'a Item> {
    if !id.is_empty() {
        return theirs.items.iter().find(|i| i.id == id);
    }
    theirs.items.iter().rev().find(|i| i.part_of.is_none() && i.step.is_some() && i.step == g.step)
}

/// Keeps `mark`, subagent `child`'s.
async fn keep_mark(hub: &Hub, child: &str, mark: &Mark) -> anyhow::Result<()> {
    hub.store.kv_set(format!("{MARK}{child}"), serde_json::to_string(mark)?).await
}

/// Subagent `child`, started for the goal `mark` names (a plan's step, or any goal): met there, the
/// goal is met here; ended otherwise there, or the subagent gone, it comes back here; still at it,
/// it's paused and resumed with the stack it works for. A subagent whose goal is over ends once it
/// has settled, and one whose goal is no longer its (taken out, the stack cleared, said met here by
/// someone else) ends now.
async fn follow_one(hub: &Hub, metrics: &crate::metrics::AgentMetrics, child: &str, mark: &mut Mark) -> anyhow::Result<()> {
    let stack = super::get(hub, &mark.agent).await?;
    let mine = stack.items.iter().find(|i| i.id == mark.goal && i.subagent.as_deref() == Some(child));
    let Some(info) = hub.harness.info(child) else {
        // Not known right now (its machine reconnecting, the server just started) is waited for;
        // ended for good, its goal stack went with it, and its step comes back.
        if kept(hub, child).await? {
            return Ok(());
        }
        if let Some(g) = mine.filter(|g| open(g)) {
            back(hub, &mark.agent, &g.id, child, "its subagent ended before it was met").await?;
        }
        let _ = hub.store.kv_delete(format!("{MARK}{child}")).await;
        return Ok(());
    };
    let theirs = super::get(hub, child).await?;
    let there = mine.and_then(|g| theirs_for(&theirs, &mark.theirs, g)).map(|i| (i.status.clone(), i.reason.clone()));
    match (mine, there) {
        (Some(g), Some((s, why))) if open(g) && s == "met" => met_here(hub, metrics, &mark.agent, &g.id, &info, why).await?,
        (Some(g), Some((s, _))) if open(g) && matches!(s.as_str(), "pending" | "active") => paced(hub, &stack, &theirs, child, mark).await,
        (Some(g), there) if open(g) => {
            let why = match there.map(|t| t.0).as_deref() {
                Some("unmet") => "its subagent gave up on it",
                Some(_) => "its subagent's goal for it was skipped",
                None => "its subagent's goal for it was cleared",
            };
            back(hub, &mark.agent, &g.id, child, why).await?;
            end(hub, child).await;
        }
        // Met here as it met it there: it ends once it has settled (its last words said).
        (Some(_), Some((s, _))) if s == "met" => {
            if matches!(crate::harness::wait::stand(&info), "idle" | "stopped" | "ended") {
                end(hub, child).await;
            }
        }
        // No longer its step: it ends now.
        _ => end(hub, child).await,
    }
    Ok(())
}

/// A goal's subagent paused while the stack it works for is (by a person, by its agent, for
/// usage), and resumed with it; a pause of the subagent's own is left as it is.
async fn paced(hub: &Hub, stack: &Stack, theirs: &Stack, child: &str, mark: &mut Mark) {
    let by = By::Agent(hub.harness.info(&mark.agent).map_or_else(|| "its agent".to_owned(), |i| i.name));
    let changed = if stack.state == "paused" && theirs.state == "running" && !mark.paused {
        let why = Some("the goal stack it works for is paused: it goes on when that resumes".to_owned());
        super::control_as(hub, child, Action::Pause, &by, Pausing { why, until: None, review: None }).await.is_ok()
    } else if stack.state == "running" && mark.paused {
        if theirs.state == "paused" {
            let _ = super::control_as(hub, child, Action::Start, &by, Pausing::default()).await;
        }
        true
    } else {
        false
    };
    if changed {
        mark.paused = !mark.paused;
        let _ = keep_mark(hub, child, mark).await;
    }
}

/// Goal `goal` of `agent`'s stack met by its subagent `sub` (`why`: how it was met there): met here,
/// with its parts still to go, and what's next goes to the agent.
async fn met_here(hub: &Hub, metrics: &crate::metrics::AgentMetrics, agent: &str, goal: &str, sub: &Info, why: Option<String>) -> anyhow::Result<()> {
    let _one = CHANGING.lock().await;
    let stack = super::get(hub, agent).await?;
    if !stack.items.iter().any(|i| i.id == goal && open(i) && i.subagent.as_deref() == Some(sub.id.as_str())) {
        return Ok(());
    }
    let parts = descendants(&stack.items, goal);
    let ending: Vec<String> = stack.items.iter().filter(|i| open(i) && parts.contains(&i.id)).map(|i| i.id.clone()).collect();
    let reason = format!("Met by its subagent \u{201c}{}\u{201d} ({}){}", sub.name, crate::harness::lineage::short(&sub.id), why.map(|w| format!(": {w}")).unwrap_or_default());
    let (a, g, now) = (agent.to_owned(), goal.to_owned(), super::now_ms(hub));
    let (running, met) = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let mut met = tx.execute("UPDATE goal_items SET status = 'met', reason = ?2, ended_at = ?3 WHERE id = ?1", params![g, reason, now])?;
            for p in &ending {
                met += tx.execute("UPDATE goal_items SET status = 'met', reason = 'Met with the goal it is a part of.', ended_at = ?2 WHERE id = ?1", params![p, now])?;
            }
            renumber(&tx, &a)?;
            touch(&tx, &a, None, None, now)?;
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![a], |r| r.get(0))?;
            tx.commit()?;
            Ok((state == "running", met))
        })
        .await?;
    for _ in 0..met {
        metrics.goal_ended(true);
    }
    tracing::info!(%agent, %goal, subagent = %sub.id, "a goal met by its subagent");
    if running {
        super::push(hub, agent, metrics).await;
    }
    Ok(())
}

/// Goal `goal` of `agent`'s stack back from its subagent `child` unmet (`why`), with its parts: the
/// agent takes it itself (at once, when it's free).
async fn back(hub: &Hub, agent: &str, goal: &str, child: &str, why: &str) -> anyhow::Result<()> {
    let _one = CHANGING.lock().await;
    let (a, g, c, now) = (agent.to_owned(), goal.to_owned(), child.to_owned(), super::now_ms(hub));
    let reason = format!("Back from its subagent: {why}.");
    let running = hub
        .store
        .call(move |db| {
            let tx = crate::store::tx(db)?;
            tx.execute("UPDATE goal_items SET subagent = NULL, subagent_back = 1, reason = ?3 WHERE id = ?1 AND subagent = ?2 AND status IN ('pending', 'active')", params![g, c, reason])?;
            tx.execute("UPDATE goal_items SET subagent = NULL WHERE agent = ?1 AND subagent = ?2 AND status IN ('pending', 'active')", params![a, c])?;
            touch(&tx, &a, None, None, now)?;
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![a], |r| r.get(0))?;
            tx.commit()?;
            Ok(state == "running")
        })
        .await?;
    tracing::info!(%agent, %goal, subagent = %child, why, "a goal back from its subagent");
    if running {
        super::push(hub, agent, crate::metrics::agents()).await;
    }
    Ok(())
}

/// Ends subagent `child` (its goal over, or no longer its), and forgets it.
async fn end(hub: &Hub, child: &str) {
    crate::harness::ops::end(hub, child, "kiln").await;
    let _ = hub.store.kv_delete(format!("{MARK}{child}")).await;
}

/// The steps ready to hand to subagents on every running stack, as many as each agent has room for
/// ([`to_hand`]). Not for an agent at a usage limit, away, or whose program holds new goals for
/// usage; nor moments after one couldn't be started for it.
async fn hand_out(hub: &Hub) {
    let running: Vec<String> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'running'")?;
            let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap_or_default();
    if running.is_empty() {
        return;
    }
    let all = hub.harness.list();
    let now = super::now_ms(hub);
    for agent in running {
        let Some(info) = all.iter().find(|i| i.id == agent) else { continue };
        if !may_hand(info, now) {
            continue;
        }
        let Ok(stack) = super::get(hub, &agent).await else { continue };
        let graph = match graph_of(&hub.store, &stack).await {
            Ok(g) if !g.is_empty() => g,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(%agent, error = %format!("{e:#}"), "a plan's steps: their graph not read");
                continue;
            }
        };
        // None ever (as deep as agents may go, a limit of 0): no start is tried, and its steps are its own.
        let room = crate::harness::lineage::room_of(hub, info).await.more();
        let steps: Vec<Item> = to_hand(&stack, &graph, room, AGENT_FIRST).into_iter().cloned().collect();
        if steps.is_empty() || crate::programs::rule(&hub.store, &all, &agent).await.holds_new_goals() {
            continue;
        }
        for u in steps {
            if let Err(why) = hand(hub, info, &stack, &graph, &u).await {
                tracing::info!(%agent, goal = %u.id, %why, "a plan's step stays with its agent: no subagent started for it");
                FAILED.lock().insert(agent.clone(), now);
                break;
            }
        }
    }
}

/// Hands goal `u` of `info`'s stack (a plan's step, or any goal) to a subagent of its own: started
/// for it as its subagent (`lineage::start_subagent`: its child, shown in its chat; on its machine,
/// in its folder, its harness and its settings, its mode as far as that may be handed on), held to
/// the goal on its own goal stack (split there as it was here), and the goal marked here as under
/// way on it. A goal made from no plan step names its subagent by its words and is held as they
/// say. Refused (the agent may start no more, its machine is away…): why, and the goal stays the
/// agent's.
async fn hand(hub: &Hub, info: &Info, stack: &Stack, g: &Graph, u: &Item) -> Result<(), String> {
    let brief = g.briefs.get(&u.id).cloned().unwrap_or_else(|| Brief { name: name_of(&u.text), goal: u.text.clone() });
    let starter = Starter { parent: Some(info.clone()), opener: crate::mcp::acts_for(hub, &info.id).await, actor: "kiln".to_owned(), plan_run: None, asker: None };
    // Where and how it runs is its agent's own: nothing asked here but its name.
    let ask = Ask { name: Some(brief.name.clone()), ..Ask::default() };
    let mut mark = Mark { agent: info.id.clone(), goal: u.id.clone(), ..Mark::default() };
    let child = crate::harness::lineage::start_subagent(hub, &starter, ask).await.map_err(|(_, m)| m)?.id;
    // Kept first, so no subagent goes unfollowed: one whose step went meanwhile ends at the next look,
    // and one that can't be kept ends now.
    if let Err(e) = keep_mark(hub, &child, &mark).await {
        crate::harness::ops::end(hub, &child, "kiln").await;
        return Err(format!("{e:#}"));
    }
    let goal = NewGoal { text: brief.goal.clone(), step: u.step.clone(), ..NewGoal::default() };
    let theirs = super::add_at(hub, &child, vec![goal], true, &At::End).await.map_err(|e| e.to_string())?;
    // The goal it holds this one as: followed by its id (a goal made from no plan step too).
    if let Some(id) = theirs.items.iter().find(|i| i.part_of.is_none()).map(|i| i.id.clone()) {
        mark.theirs = id;
        keep_mark(hub, &child, &mark).await.map_err(|e| format!("{e:#}"))?;
    }
    let parts: Vec<&Item> = stack.items.iter().filter(|i| i.part_of.as_deref() == Some(u.id.as_str())).collect();
    if !parts.is_empty() {
        // Its acceptance criteria and tests are made there as here; parts its agent gave go as given.
        let given: Vec<String> = if parts.iter().all(|p| p.part_kind.as_deref() == Some("checks")) { Vec::new() } else { parts.iter().map(|p| p.text.clone()).collect() };
        let id = theirs_for(&theirs, &mark.theirs, u).map(|i| i.id.clone());
        if let Err(e) = super::split(hub, &child, id.as_deref(), given).await {
            tracing::info!(subagent = %child, error = %e, "a step's subagent: its goal not split as it was");
        }
    }
    if mark_out(hub, &info.id, &u.id, &child).await.map_err(|e| format!("{e:#}"))? {
        let side = to_words(stack, g, u);
        hub.store.audit("kiln".to_owned(), "goals.subagent".to_owned(), format!("{}: {} to {} ({side})", info.id, brief.name, child)).await;
        tracing::info!(agent = %info.id, goal = %u.id, subagent = %child, "a goal handed to a subagent, side by side with others");
    }
    Ok(())
}

/// A subagent's name for a goal made from no plan step: its words before any detail (" — "), at
/// most [`NAME`] characters.
fn name_of(text: &str) -> String {
    let head = text.split(" \u{2014} ").next().unwrap_or(text);
    head.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(NAME).collect()
}

/// Why a goal went to a subagent, in words: for a plan's step, the steps of its plan under way or
/// ready beside it.
fn to_words(stack: &Stack, g: &Graph, u: &Item) -> String {
    if !is_step(u) {
        return "side by side with the stack's other goals".to_owned();
    }
    let items = stack.items.as_slice();
    let run = group_of(u, items, g);
    let n: Vec<String> = items.iter().filter(|i| is_step(i) && open(i) && i.id != u.id && group_of(i, items, g) == run).filter_map(|i| i.step.as_ref().map(|s| s.n.to_string())).collect();
    if n.is_empty() { "its plan's steps run side by side".to_owned() } else { format!("side by side with step{} {}", if n.len() == 1 { "" } else { "s" }, n.join(", ")) }
}

/// Marks goal `goal` of `agent`'s stack, and its parts, under way on subagent `child`, if it's still
/// the agent's to hand ([`untouched`]): whether it was.
async fn mark_out(hub: &Hub, agent: &str, goal: &str, child: &str) -> anyhow::Result<bool> {
    let _one = CHANGING.lock().await;
    let stack = super::get(hub, agent).await?;
    if !stack.items.iter().find(|i| i.id == goal).is_some_and(|u| untouched(u, &stack.items)) {
        return Ok(false);
    }
    let ids: Vec<String> = std::iter::once(goal.to_owned()).chain(descendants(&stack.items, goal)).collect();
    let (a, c, now) = (agent.to_owned(), child.to_owned(), super::now_ms(hub));
    hub.store
        .call(move |db| {
            let tx = crate::store::tx(db)?;
            for id in &ids {
                tx.execute("UPDATE goal_items SET subagent = ?2 WHERE id = ?1", params![id, c])?;
            }
            touch(&tx, &a, None, None, now)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goals::StepRef;

    fn step(id: &str, n: usize, status: &str) -> Item {
        Item { id: id.into(), text: format!("step {n}"), status: status.into(), step: Some(StepRef { plan: "p".into(), version: 1, n, run: Some("r".into()) }), ..Item::default() }
    }

    fn stack(items: Vec<Item>) -> Stack {
        Stack { agent: "a".into(), state: "running".into(), items, ..Stack::default() }
    }

    /// Steps 1 → (2, 3, 4) → 5, by goal id.
    fn diamond() -> Graph {
        let mut g = Graph::default();
        for (id, w) in [("s1", vec![]), ("s2", vec!["s1"]), ("s3", vec!["s1"]), ("s4", vec!["s1"]), ("s5", vec!["s2", "s3", "s4"])] {
            g.waits.insert(id.into(), w.into_iter().map(str::to_owned).collect());
        }
        g
    }

    fn ids(v: &[&Item]) -> Vec<String> {
        v.iter().map(|i| i.id.clone()).collect()
    }

    /// What the agent takes up next, as the driver decides it (`goals::decide_on`): its stack's
    /// graph, `g`'s steps as their plan says, those about to go to subagents left for them.
    fn next_up<'a>(s: &'a Stack, g: &Graph) -> Option<&'a Item> {
        use crate::goals::graph::{Room, assign, build};
        let nodes = crate::goals::nodes_of(&s.items, g);
        let id = assign(&build(&nodes, false), &nodes, Room::Never).agent?;
        s.items.iter().find(|i| i.id == id)
    }

    /// docs/agentic.md P2: the first step alone goes to the agent; once it's met, the three that
    /// wait only for it can be under way at once, so each goes to a subagent while there's room, and
    /// the agent takes the ones there's no room for; the last waits for all three.
    #[test]
    fn p2_steps_that_can_run_at_once_go_to_subagents_as_room_allows() {
        let g = diamond();
        let s = stack(vec![step("s1", 1, "pending"), step("s2", 2, "pending"), step("s3", 3, "pending"), step("s4", 4, "pending"), step("s5", 5, "pending")]);
        assert!(to_hand(&s, &g, 8, false).is_empty(), "one step ready, none under way: the agent's own");
        assert_eq!(next_up(&s, &g).map(|i| i.id.as_str()), Some("s1"));
        let s = stack(vec![step("s1", 1, "met"), step("s2", 2, "pending"), step("s3", 3, "pending"), step("s4", 4, "pending"), step("s5", 5, "pending")]);
        assert_eq!(ids(&to_hand(&s, &g, 8, false)), ["s2", "s3", "s4"]);
        assert_eq!(ids(&to_hand(&s, &g, 2, false)), ["s2", "s3"], "as many as there's room for");
        assert!(to_hand(&s, &g, 0, false).is_empty());
        // The other way round (one switch): the agent takes the first, subagents the rest.
        assert_eq!(ids(&to_hand(&s, &g, 8, true)), ["s3", "s4"]);
        // The agent leaves the steps about to go to subagents for them, and takes the one there's no
        // room for; with room for all three, it takes none (the last waits for them).
        let leaving = |room: usize| Graph { handing: to_hand(&s, &g, room, false).iter().map(|i| i.id.clone()).collect(), ..g.clone() };
        assert_eq!(next_up(&s, &leaving(2)).map(|i| i.id.as_str()), Some("s4"));
        assert!(next_up(&s, &leaving(8)).is_none());
        assert_eq!(next_up(&s, &leaving(0)).map(|i| i.id.as_str()), Some("s2"), "no room: the agent takes them in turn, as before");
        // Two out on subagents: the agent takes the third itself; the last waits for all three.
        let mut items = vec![step("s1", 1, "met"), step("s2", 2, "pending"), step("s3", 3, "pending"), step("s4", 4, "pending"), step("s5", 5, "pending")];
        for i in items.iter_mut().filter(|i| i.id == "s2" || i.id == "s3") {
            i.subagent = Some(format!("sub-{}", i.id));
        }
        let s = stack(items.clone());
        assert_eq!(next_up(&s, &g).map(|i| i.id.as_str()), Some("s4"));
        assert_eq!(ids(&to_hand(&s, &g, 1, false)), ["s4"], "with room again, the third goes to a subagent too");
        assert!(to_hand(&s, &g, 1, true).is_empty(), "the agent first: the third is the agent's");
        for i in items.iter_mut().filter(|i| i.id == "s4") {
            i.status = "met".into();
        }
        let s = stack(items.clone());
        assert!(next_up(&s, &g).is_none(), "step 5 waits for 2 and 3, out on subagents");
        assert!(to_hand(&s, &g, 8, false).is_empty());
        for i in items.iter_mut().filter(|i| i.id == "s2" || i.id == "s3") {
            i.status = "met".into();
        }
        assert_eq!(next_up(&stack(items), &g).map(|i| i.id.as_str()), Some("s5"), "alone again: the agent's");
    }

    /// The stack's order holds: what's past a plan's steps waits for them, a planned goal waits for
    /// its parts out on subagents, a step under way here counts as one side by side, a step that came
    /// back from its subagent is the agent's, and a step's parts go with it (the agent pursues them
    /// first; one it has started stays its own).
    #[test]
    fn p2_the_stack_s_order_holds_around_steps_side_by_side() {
        let g = diamond();
        let planned = |id: &str, n: usize, status: &str| Item { part_of: Some("goal".into()), part_kind: Some("plan".into()), ..step(id, n, status) };
        let goal = Item { id: "goal".into(), text: "the goal".into(), status: "pending".into(), ..Item::default() };
        let later = Item { id: "later".into(), text: "a goal after it".into(), status: "pending".into(), ..Item::default() };
        let mut items = vec![planned("s1", 1, "met"), planned("s2", 2, "pending"), planned("s3", 3, "pending"), planned("s4", 4, "met"), planned("s5", 5, "pending"), goal, later];
        for i in items.iter_mut().filter(|i| i.id == "s2" || i.id == "s3") {
            i.subagent = Some("sub".into());
        }
        assert!(next_up(&stack(items.clone()), &g).is_none(), "neither the goal nor what's after it starts while its steps are out");
        // A step under way here, one ready: they're side by side.
        let s = stack(vec![step("s1", 1, "met"), step("s2", 2, "active"), step("s3", 3, "pending"), step("s4", 4, "met"), step("s5", 5, "pending")]);
        assert_eq!(ids(&to_hand(&s, &g, 8, false)), ["s3"]);
        // Back from its subagent: the agent's, not another subagent's.
        let mut came_back = step("s3", 3, "pending");
        came_back.back = true;
        let s = stack(vec![step("s1", 1, "met"), step("s2", 2, "pending"), came_back, step("s4", 4, "met"), step("s5", 5, "pending")]);
        assert_eq!(ids(&to_hand(&s, &g, 8, false)), ["s2"]);
        // Its parts first, as the agent pursues them; started, the step stays the agent's.
        let check = |id: &str, status: &str| Item { id: id.into(), status: status.into(), part_of: Some("s2".into()), part_kind: Some("checks".into()), ..step(id, 2, status) };
        let s = stack(vec![step("s1", 1, "met"), check("c1", "pending"), check("c2", "pending"), step("s2", 2, "pending"), step("s3", 3, "pending"), step("s4", 4, "met"), step("s5", 5, "pending")]);
        assert_eq!(next_up(&s, &g).map(|i| i.id.as_str()), Some("c1"));
        assert_eq!(ids(&to_hand(&s, &g, 8, false)), ["s2", "s3"]);
        let s = stack(vec![step("s1", 1, "met"), check("c1", "met"), check("c2", "pending"), step("s2", 2, "pending"), step("s3", 3, "pending"), step("s4", 4, "met"), step("s5", 5, "pending")]);
        assert_eq!(ids(&to_hand(&s, &g, 8, false)), ["s3"], "step 2 is under way here: its criteria part met");
        // A part of a step out on its subagent is out too, whoever split it.
        let mut out_step = step("s2", 2, "pending");
        out_step.subagent = Some("sub".into());
        let s = stack(vec![step("s1", 1, "met"), check("c1", "pending"), out_step, step("s3", 3, "pending"), step("s4", 4, "met"), step("s5", 5, "pending")]);
        assert_eq!(next_up(&s, &g).map(|i| i.id.as_str()), Some("s3"));
    }

    /// A plan whose steps say nothing of waiting isn't in the graph: its steps go as before, one after
    /// another in the stack's order, and none goes to a subagent; a goal under way on another agent (a
    /// program's worker) is passed over, as before.
    #[test]
    fn steps_that_say_nothing_of_waiting_go_as_before() {
        let g = Graph::default();
        let mut handed = step("s1", 1, "pending");
        handed.on = Some("worker".into());
        let s = stack(vec![handed, step("s2", 2, "pending"), step("s3", 3, "pending")]);
        assert_eq!(next_up(&s, &g).map(|i| i.id.as_str()), Some("s2"));
        assert!(to_hand(&s, &g, 8, false).is_empty());
    }

    /// A goal made from no plan step names its subagent by its words, its detail left out.
    #[test]
    fn a_goal_from_no_plan_step_names_its_subagent_by_its_words() {
        assert_eq!(name_of("Ship the  replication \u{2014} Done when: it's tested"), "Ship the replication");
        assert_eq!(name_of(&"x".repeat(200)).chars().count(), NAME);
    }
}

/// A plan's steps side by side on a hub of its own, under a timeout: no harness runs (each agent
/// starts and stands stopped, which a message starts again), and each goal is said met, or given
/// up on, in the store as its agent would.
#[cfg(all(test, unix))]
mod hub_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::goals::Change;

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("it hung"),
        }
    }

    /// Until agent `id` no longer stands starting (its start fails: no harness here).
    async fn settled(hub: &Hub, id: &str) {
        for _ in 0..200 {
            if hub.harness.info(id).is_some_and(|i| i.status != Status::Starting) {
                return;
            }
            crate::rt::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The driver's look at steps side by side, again and again, until `done` says so.
    async fn tick_until(hub: &Hub, metrics: &crate::metrics::AgentMetrics, what: &str, done: impl Fn(&Hub) -> bool) -> anyhow::Result<()> {
        for _ in 0..200 {
            tick(hub, metrics).await;
            if done(hub) {
                return Ok(());
            }
            crate::rt::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("never: {what}")
    }

    /// Each step's goal on `agent`'s stack, in step order: (number, status, subagent).
    async fn steps(hub: &Hub, agent: &str) -> anyhow::Result<Vec<(usize, String, Option<String>)>> {
        let s = super::super::get(hub, agent).await?;
        let mut v: Vec<(usize, String, Option<String>)> = s.items.iter().filter(|i| is_step(i)).filter_map(|i| Some((i.step.as_ref()?.n, i.status.clone(), i.subagent.clone()))).collect();
        v.sort_by_key(|r| r.0);
        Ok(v)
    }

    fn row(n: usize, status: &str, sub: Option<&str>) -> (usize, String, Option<String>) {
        (n, status.to_owned(), sub.map(str::to_owned))
    }

    /// A hub, its main agent (`max`: its Subagents setting), and a plan run on it as goals, its steps
    /// as `needs` say (by number from 1), the first goal under way.
    async fn run_on_main(max: Option<u32>, needs: &[&[usize]]) -> anyhow::Result<(Arc<Hub>, String, tempfile::TempDir)> {
        let hub = crate::hub::tests::scratch_hub_with(|c| {
            c.terminal.enabled = true;
            c.conversations.off = true;
        })
        .await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        let folder = tempfile::tempdir()?;
        let spec = crate::harness::session::Spec {
            kind: "claude".into(),
            cwd: Some(folder.path().to_string_lossy().into_owned()),
            mode: Some("default".into()),
            name: Some("Main".into()),
            max_subagents: max,
            ..crate::harness::session::Spec::default()
        };
        let main = hub.harness.open(&hub, "local", spec, 1).await?.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("no agent"))?;
        for _ in 0..200 {
            if hub.harness.info(&main).is_some_and(|i| i.max_subagents == max && i.status != Status::Starting) {
                break;
            }
            crate::rt::sleep(Duration::from_millis(25)).await;
        }
        let titles = ["Schema", "API", "UI", "Docs", "Ship"];
        let items = needs.iter().enumerate().map(|(k, n)| crate::plans::Item { title: titles.get(k).copied().unwrap_or("Step").to_owned(), needs: n.to_vec(), ..crate::plans::Item::default() }).collect();
        let p = crate::plans::create(&hub, crate::plans::New { title: "Fan out".into(), items, ..crate::plans::New::default() }, "admin").await?;
        let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None, asker: None };
        crate::plans::run(&hub, &p.id, crate::plans::Target::Session(main.clone()), crate::plans::Mode::Goals, &crate::plans::Pick::default(), &person, None).await?;
        Ok((hub, main, folder))
    }

    /// The goal for step `n` on `agent`'s stack.
    async fn goal_of(hub: &Hub, agent: &str, n: usize) -> anyhow::Result<String> {
        let s = super::super::get(hub, agent).await?;
        s.items.iter().find(|i| i.part_of.is_none() && i.step.as_ref().is_some_and(|st| st.n == n)).map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no goal for step {n} on {agent}: {s:?}"))
    }

    /// docs/agentic.md P2: run as goals on an agent that runs at most two subagents at once, a plan
    /// 1 → (2, 3, 4) → 5 goes as its graph says. Step 1 alone is the agent's. Once it's met, steps 2
    /// and 3 go to subagents of the agent's own (its children, named for their steps, each held to its
    /// step on its own stack) and the agent takes step 4, there being no room for a third. The stack
    /// paused, the subagents pause, and resume with it. Met on its subagent, step 2 is met here, saying
    /// so, and its subagent ends once it has settled; given up on there, step 3 comes back here (its
    /// subagent ends), and the agent takes it itself; step 5 waits for all three, then it's the
    /// agent's, and the stack is done.
    #[test]
    fn p2_steps_side_by_side_go_to_subagents_within_the_agent_s_setting_and_come_back() -> anyhow::Result<()> {
        within(async {
            let (hub, main, _folder) = run_on_main(Some(2), &[&[], &[1], &[1], &[1], &[2, 3, 4]]).await?;
            let metrics = crate::metrics::AgentMetrics::default();
            let main = main.as_str();
            assert_eq!(steps(&hub, main).await?, [row(1, "active", None), row(2, "pending", None), row(3, "pending", None), row(4, "pending", None), row(5, "pending", None)], "step 1 alone: the agent's");
            tick(&hub, &metrics).await;
            assert!(steps(&hub, main).await?.iter().all(|(_, _, s)| s.is_none()), "nothing to hand while step 1 goes");
            // Step 1 met: 2 and 3 are left for subagents, the agent takes 4.
            settled(&hub, main).await;
            super::super::done(&hub, main, Some(&goal_of(&hub, main, 1).await?), Some("the schema migrates"), &By::Itself).await?;
            assert_eq!(steps(&hub, main).await?, [row(1, "met", None), row(2, "pending", None), row(3, "pending", None), row(4, "active", None), row(5, "pending", None)]);
            tick(&hub, &metrics).await;
            let now = steps(&hub, main).await?;
            let sub = |n: usize| now.iter().find(|r| r.0 == n).and_then(|r| r.2.clone()).unwrap_or_default();
            let (two, three) = (sub(2), sub(3));
            assert!(!two.is_empty() && !three.is_empty() && two != three, "{now:?}");
            assert_eq!(now.iter().filter(|r| r.2.is_some()).count(), 2, "at most its setting at once: {now:?}");
            let agent = hub.harness.info(main).ok_or_else(|| anyhow::anyhow!("the agent isn't there"))?;
            for (id, n, name) in [(&two, 2, "Step 2: API"), (&three, 3, "Step 3: UI")] {
                let info = hub.harness.info(id).ok_or_else(|| anyhow::anyhow!("subagent {id} isn't there"))?;
                assert!(info.parent.as_deref() == Some(main) && info.name == name, "{info:?}");
                // Its subagent, not an agent of its own: on its machine, in its folder and harness,
                // in its mode and with its Subagents setting.
                assert!(info.subagent && info.target == agent.target && info.cwd == agent.cwd && info.kind == agent.kind && info.mode == agent.mode && info.max_subagents == Some(2), "{info:?} as {agent:?}");
                let theirs = super::super::get(&hub, id).await?;
                let g = theirs.items.first().cloned().unwrap_or_default();
                assert!(g.step.as_ref().is_some_and(|s| s.n == n) && g.text.starts_with(&format!("Step {n} of 5 of the plan \"Fan out\": ")) && theirs.state == "running", "{theirs:?}");
            }
            // It goes where its agent goes: not moved by itself.
            let alone = crate::harness::relocate::start(&hub, &two, "elsewhere", false, None, None).await;
            assert!(alone.as_ref().is_err_and(|e| e.contains("goes where its agent goes")), "{alone:?}");
            // Paused here, they pause; resumed, they go on.
            super::super::control(&hub, main, Action::Pause).await?;
            tick(&hub, &metrics).await;
            for id in [&two, &three] {
                let s = super::super::get(&hub, id).await?;
                assert!(s.state == "paused" && s.reason.as_deref().is_some_and(|r| r.contains("the goal stack it works for is paused")), "{s:?}");
            }
            settled(&hub, main).await;
            super::super::control(&hub, main, Action::Start).await?;
            tick(&hub, &metrics).await;
            for id in [&two, &three] {
                assert_eq!(super::super::get(&hub, id).await?.state, "running");
            }
            // Met on its subagent: met here, saying who met it and how; its subagent ends, settled.
            let theirs = goal_of(&hub, &two, 2).await?;
            super::super::done(&hub, &two, Some(&theirs), Some("the API answers"), &By::Itself).await?;
            tick(&hub, &metrics).await;
            let s = super::super::get(&hub, main).await?;
            let met = s.items.iter().find(|i| i.step.as_ref().is_some_and(|st| st.n == 2)).cloned().unwrap_or_default();
            assert!(met.status == "met" && met.reason.as_deref().is_some_and(|r| r.starts_with("Met by its subagent \u{201c}Step 2: API\u{201d}") && r.ends_with("the agent said so: the API answers")), "{met:?}");
            let gone = two.clone();
            tick_until(&hub, &metrics, "step 2's subagent ends", move |h| h.harness.info(&gone).is_none()).await?;
            // Given up on there: back here, its subagent ended, and the agent takes it itself.
            let t = three.clone();
            hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'unmet' WHERE agent = ?1", params![t]).map(|_| ())?)).await?;
            tick(&hub, &metrics).await;
            assert!(hub.harness.info(&three).is_none(), "its subagent ended");
            let s = super::super::get(&hub, main).await?;
            let back = s.items.iter().find(|i| i.step.as_ref().is_some_and(|st| st.n == 3)).cloned().unwrap_or_default();
            assert!(back.status == "pending" && back.subagent.is_none() && back.back && back.reason.as_deref() == Some("Back from its subagent: its subagent gave up on it."), "{back:?}");
            settled(&hub, main).await;
            super::super::done(&hub, main, Some(&goal_of(&hub, main, 4).await?), None, &By::Itself).await?;
            tick(&hub, &metrics).await;
            assert_eq!(steps(&hub, main).await?, [row(1, "met", None), row(2, "met", Some(two.as_str())), row(3, "active", None), row(4, "met", None), row(5, "pending", None)], "the agent's, not another subagent's");
            // The last waits for all three, then it's the agent's; then the stack is done.
            settled(&hub, main).await;
            super::super::done(&hub, main, Some(&goal_of(&hub, main, 3).await?), None, &By::Itself).await?;
            assert_eq!(steps(&hub, main).await?.last(), Some(&row(5, "active", None)));
            settled(&hub, main).await;
            let s = super::super::done(&hub, main, Some(&goal_of(&hub, main, 5).await?), None, &By::Itself).await?;
            assert_eq!(s.state, "done", "{s:?}");
            Ok(())
        })
    }

    /// A step out on a subagent that's taken out of the stack lets its subagent go (it ends), and so
    /// does one said met here by the agent itself, and clearing the stack; brought back, the steps
    /// are the stack's to hand again, to new subagents. With no setting of its own, an agent hands
    /// every step that's ready at once.
    #[test]
    fn p2_a_step_taken_out_said_met_here_or_the_stack_cleared_lets_its_subagent_go() -> anyhow::Result<()> {
        within(async {
            let (hub, main, _folder) = run_on_main(None, &[&[], &[1], &[1], &[1], &[1]]).await?;
            let metrics = crate::metrics::AgentMetrics::default();
            let main = main.as_str();
            settled(&hub, main).await;
            super::super::done(&hub, main, Some(&goal_of(&hub, main, 1).await?), None, &By::Itself).await?;
            tick(&hub, &metrics).await;
            let now = steps(&hub, main).await?;
            let sub = |n: usize| now.iter().find(|r| r.0 == n).and_then(|r| r.2.clone()).unwrap_or_default();
            let (two, three, four, five) = (sub(2), sub(3), sub(4), sub(5));
            assert!([&two, &three, &four, &five].iter().all(|s| !s.is_empty()), "all four at once: {now:?}");
            super::super::update(&hub, main, &goal_of(&hub, main, 3).await?, Change { remove: true, ..Change::default() }, &By::You).await?;
            tick(&hub, &metrics).await;
            assert!(hub.harness.info(&three).is_none() && [&two, &four, &five].iter().all(|s| hub.harness.info(s).is_some()), "only the step taken out lets its subagent go");
            super::super::done(&hub, main, Some(&goal_of(&hub, main, 4).await?), Some("done here"), &By::Itself).await?;
            tick(&hub, &metrics).await;
            assert!(hub.harness.info(&four).is_none() && hub.harness.info(&two).is_some(), "said met here, its subagent goes; the others stay");
            super::super::control(&hub, main, Action::Clear).await?;
            tick(&hub, &metrics).await;
            assert!(hub.harness.info(&two).is_none() && hub.harness.info(&five).is_none(), "cleared: their subagents go too");
            assert!(hub.store.kv_prefix(MARK.to_owned()).await?.is_empty(), "none left to follow");
            settled(&hub, main).await;
            super::super::control(&hub, main, Action::Restore).await?;
            tick(&hub, &metrics).await;
            let again = steps(&hub, main).await?;
            let fresh = |n: usize, before: &str| again.iter().find(|r| r.0 == n).is_some_and(|r| r.2.as_deref().is_some_and(|s| s != before));
            assert!(fresh(2, &two) && fresh(5, &five), "brought back, they go to new subagents: {again:?}");
            Ok(())
        })
    }
}
