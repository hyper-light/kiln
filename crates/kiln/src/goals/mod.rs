//! Goal stacks: an agent's goals, one after another, each pursued until it's met before the next
//! starts, as if you gave it /goal again and again yourself (docs/goals.md). On a harness with
//! goals of its own the goal goes to them (Claude Code's `/goal <condition>`, Codex's thread goal)
//! and the harness says when it's met; on the rest it goes as a message, and the agent says so
//! (its reply ends with `GOAL MET`).
//!
//! Until the goal under way is met, kiln keeps the agent on it: whenever the agent stops short of
//! it (it gave up, paused it, stopped with it still set, never took it up, or lost it when it
//! started again after a runner update or a restart), the goal is sent again, after a pause that
//! grows while the reminders bring no real work. A goal set in the agent directly (`/goal` in its
//! composer) joins its stack, so it's kept the same way. The server keeps the stacks and drives
//! them (`run`), so they go on with no console open and across restarts. What a stack says to an
//! agent behind a turn under way (a goal, the goal again, a clear) waits in its queue one message
//! at a time ([`say`]): the next edits the one waiting, so a long turn doesn't end with goals and
//! clears stacked up behind it.
//!
//! An agent works its stack itself too: it says a goal is met ([`done`], with what shows it), changes
//! one goal ([`update`]: other words, moved, taken out), splits one into parts pursued first
//! ([`split`]: a plan step's goal into its acceptance criteria and tests), and pauses the stack for
//! a while ([`control_as`]: it resumes by itself when the time is up).
//!
//! The driver ([`Driver`]) takes its time from a kiln-seams `Clock` and reaches agents through a
//! `HarnessPort` (docs/seams.md): in production, the hub's clock (kiln's runtime) and its harness
//! registry; in tests, a simulated clock and agents, so a 15-minute back-off is checked in
//! microseconds.

pub mod keep;
pub mod ops;

use std::collections::HashMap;
use std::pin::pin;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use kiln_seams::HarnessPort;
use kiln_seams::harness::Sent;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness::model::{Command, SendMode, Status};
use crate::harness::queued::{GOAL, Kept, Now};
use crate::harness::session::{Held, Info};
use crate::hub::Hub;

/// Most goals a stack holds, those met or under way included: room for a program of a hundred
/// goals and more, its met goals kept for the record.
const ITEMS: usize = 500;
/// Longest goal.
const TEXT: usize = 2000;
/// How long an agent that stopped short of its goal is left before the goal is sent again, at
/// first; twice that after each reminder that brought no real work, up to `REMIND_MAX_MS`.
const REMIND_MS: i64 = 30_000;
const REMIND_MAX_MS: i64 = 15 * 60_000;
/// Work on a goal for this long since it was last sent counts as real: reminders start over.
const WORKED_MS: i64 = 120_000;
/// How an agent without goals of its own says a goal is met: its reply's last line.
pub const MET_LINE: &str = "GOAL MET";
/// A clear in the agent this soon after kiln cleared its goal (pausing the stack) is kiln's own.
const OWN_CLEAR_MS: i64 = 60_000;
/// Why a stack you paused is paused.
const PAUSED_BY_YOU: &str = "Paused by you: the agent isn't held to its goal (its own goal is cleared) until you resume.";
/// How often the driver looks at every stack.
pub const TICK: Duration = Duration::from_millis(1500);

/// One change to the stacks at a time, yours (start, skip, an edit …) or the server's own steps: a
/// goal goes to its agent once, and what's decided is decided on the stack as it is.
static CHANGING: async_lock::Mutex<()> = async_lock::Mutex::new(());

/// An agent's goal stack.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stack {
    pub agent: String,
    /// "running" (its goals go to the agent one after another), "paused" (see `reason`), "done".
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub items: Vec<Item>,
    pub updated_at: i64,
    /// How many goals you cleared last, kept to bring back (`Action::Restore`).
    #[serde(skip_serializing_if = "is_zero")]
    pub cleared: i64,
    /// Paused for a while: when it resumes by itself (unix ms; 0: when someone resumes it).
    #[serde(skip_serializing_if = "is_zero")]
    pub resume_at: i64,
    /// Paused by its agent for a person, until someone resumes it (what waits on you wherever you
    /// are in kiln: `held`): "" for any such pause, a plan's id when it waits for you to look at
    /// that plan first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_for: Option<String>,
    /// When kiln last cleared the goal in the agent itself (pause, skip, clear).
    #[serde(skip)]
    dropped_at: i64,
}

/// One goal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub text: String,
    /// "pending", "active", "met", "unmet", "skipped".
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// How it went to the agent: "goal" (`/goal …`), "goal_control" (the harness's goal control),
    /// "message".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// The agent it's under way on, when it was handed to another (docs/programs.md,
    /// "Assignments"): a program's coordinator keeps it, its track's worker pursues it. The driver
    /// doesn't start it on this agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    /// When the goal it replaced last changed (a verdict must be newer).
    #[serde(skip)]
    prior_at: i64,
    /// Times it was sent again, in a row, without real work in between.
    #[serde(skip_serializing_if = "is_zero")]
    pub reminders: i64,
    /// When it was last sent (unix ms; 0: not yet).
    #[serde(skip_serializing_if = "is_zero")]
    pub reminded_at: i64,
    /// When the agent was first seen at work on it since it was last sent (0: not since).
    #[serde(skip)]
    busy_at: i64,
    /// The message that last took it to the agent (`withdraw`).
    #[serde(skip)]
    sent_id: Option<String>,
    /// The goal it's a part of ([`split`]): that one comes back after its parts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_of: Option<String>,
    /// The plan step it was made from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<StepRef>,
    /// Who made it a part: `own` (the agent split its goal itself: that goal comes back for its own
    /// check after its parts), or kiln: `checks` (a plan step's acceptance criteria and tests),
    /// `plan` (the steps of the plan its goal became), `planning` (the planning of that plan),
    /// `run` (waiting for that plan's run). A goal whose parts kiln made is met once they all are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_kind: Option<String>,
    /// The plan it became ([`plan`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned_as: Option<String>,
    /// The plan run it waits for (a `run` part): nothing goes to the agent for it, and it's met
    /// once the run is done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waits_on: Option<String>,
}

/// The plan step a goal was made from (`crate::plans`): its plan, the version run, its number (from
/// 1), and the run it's a step of.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StepRef {
    pub plan: String,
    pub version: i64,
    pub n: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// A goal to add: its words, and the plan step it's made from.
#[derive(Debug, Clone, Default)]
pub struct NewGoal {
    pub text: String,
    pub step: Option<StepRef>,
    /// The goal it's a part of, and who made it so (`Item::part_kind`).
    pub part_of: Option<String>,
    pub kind: Option<&'static str>,
    /// The plan run it waits for.
    pub waits_on: Option<String>,
}

impl From<String> for NewGoal {
    fn from(text: String) -> Self {
        Self { text, ..Self::default() }
    }
}

/// Where goals added go: after the others, next (right after the goal under way), or before or after
/// a goal still to go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum At {
    #[default]
    End,
    Next,
    Before(String),
    After(String),
}

/// One goal's change ([`update`]): other words, moved (before or after another, or next), or taken
/// out.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Change {
    pub text: Option<String>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub next: bool,
    pub remove: bool,
}

/// Who changes a stack, as what it keeps says so: a person (you), the agent itself, or another
/// agent (one that started it), by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum By {
    #[default]
    You,
    Itself,
    Agent(String),
}

impl By {
    /// Who `caller` is to `agent`'s stack.
    pub fn of(hub: &Hub, caller: &crate::authz::Caller, agent: &str) -> Self {
        match caller.agent.as_deref() {
            None => Self::You,
            Some(me) if me == agent => Self::Itself,
            Some(other) => Self::Agent(hub.harness.info(other).map_or_else(|| format!("agent {}", crate::harness::lineage::short(other)), |i| i.name)),
        }
    }

    /// As a sentence says it: "you", "the agent", its name.
    fn name(&self) -> String {
        match self {
            Self::You => "you".to_owned(),
            Self::Itself => "the agent".to_owned(),
            Self::Agent(n) => n.clone(),
        }
    }
}

/// A pause as it's asked for: why, and when it ends by itself (unix ms; none: when someone resumes
/// it).
#[derive(Debug, Clone, Default)]
pub struct Pausing {
    pub why: Option<String>,
    pub until: Option<i64>,
    /// The plan it waits for a person to look at first (`plans_ready` with `then: review`).
    pub review: Option<String>,
}

/// The longest a stack pauses for by itself.
const PAUSE_MAX_MS: i64 = 30 * 24 * 3_600_000;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// A goal as an edit gives it: an existing one by `id`, or a new one.
#[derive(Debug, Clone, Deserialize)]
pub struct Given {
    #[serde(default)]
    pub id: Option<String>,
    pub text: String,
}

/// What to do with a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Run it: each goal in turn (resuming one that paused). What's next goes to the agent at once.
    #[serde(alias = "resume")]
    Start,
    /// The goal under way is met ([`done`]): the next goes at once.
    #[serde(alias = "met", alias = "complete", alias = "completed")]
    Done,
    /// Stop sending goals, and clear the one under way in the agent (it keeps its place).
    Pause,
    /// Leave the goal under way (its harness's goal cleared) and go on with the next, at once.
    Skip,
    /// Empty the stack (the goal under way, or one still set, cleared in the agent). Its goals are
    /// kept, to bring back.
    Clear,
    /// Bring back the goals cleared last, as they were, and run the stack.
    Restore,
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("no such agent")]
    NoAgent,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type R<T> = Result<T, Error>;

/// The hub's wall clock, in milliseconds: what goals' times are kept in.
fn now_ms(hub: &Hub) -> i64 {
    hub.clock.unix_ms()
}

/// One line: a goal is a condition or an objective (its harness's /goal takes a line).
fn clean(text: &str) -> R<String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return Err(Error::Bad("a goal needs some words".into()));
    }
    if t.chars().count() > TEXT {
        return Err(Error::Bad(format!("a goal is at most {TEXT} characters")));
    }
    Ok(t)
}

/// The agent's stack (empty when it has none).
pub async fn get(hub: &Hub, agent: &str) -> R<Stack> {
    stack_of(&hub.store, agent).await
}

async fn stack_of(store: &crate::store::Store, agent: &str) -> R<Stack> {
    let agent = agent.to_owned();
    Ok(store.call(move |c| load(c, &agent)).await?)
}

fn load(c: &rusqlite::Connection, agent: &str) -> anyhow::Result<Stack> {
    let head: Option<(String, Option<String>, i64, i64, i64, i64, Option<String>)> = c
        .query_row("SELECT state, reason, updated_at, dropped_at, COALESCE(json_array_length(cleared), 0), resume_at, held_for FROM goal_stacks WHERE agent = ?1", params![agent], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })
        .optional()?;
    let mut st = c.prepare(
        "SELECT id, text, status, reason, how, started_at, ended_at, prior_at, reminders, reminded_at, busy_at, part_of, plan_id, plan_version, plan_step, plan_run, sent_id, planned_as, part_kind, waits_on, delegated_to FROM goal_items WHERE agent = ?1 ORDER BY pos",
    )?;
    let items = st
        .query_map(params![agent], |r| {
            let plan: Option<String> = r.get(12)?;
            let (version, at, run): (Option<i64>, Option<i64>, Option<String>) = (r.get(13)?, r.get(14)?, r.get(15)?);
            Ok(Item {
                id: r.get(0)?,
                text: r.get(1)?,
                status: r.get(2)?,
                reason: r.get(3)?,
                how: r.get(4)?,
                started_at: r.get(5)?,
                ended_at: r.get(6)?,
                prior_at: r.get(7)?,
                reminders: r.get(8)?,
                reminded_at: r.get(9)?,
                busy_at: r.get(10)?,
                part_of: r.get(11)?,
                sent_id: r.get(16)?,
                planned_as: r.get(17)?,
                part_kind: r.get(18)?,
                waits_on: r.get(19)?,
                step: plan.map(|plan| StepRef { plan, version: version.unwrap_or(0), n: at.and_then(|i| usize::try_from(i).ok()).unwrap_or(0) + 1, run }),
                on: r.get(20)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let (state, reason, updated_at, dropped_at, cleared, resume_at, held_for) = head.unwrap_or_else(|| ("paused".into(), None, 0, 0, 0, 0, None));
    Ok(Stack { agent: agent.to_owned(), state, reason, items, updated_at, cleared, resume_at, held_for, dropped_at })
}

/// What a goal stack last said to its agent through its queue (`say`): the message, while it may
/// still wait there, and the goal it takes (none: a clear).
#[derive(Debug, Clone)]
struct Word {
    msg: Kept,
    item: Option<String>,
}

/// Each agent's word from its stack, by agent.
static SAID: LazyLock<parking_lot::Mutex<HashMap<String, Word>>> = LazyLock::new(Default::default);
/// The most agents' words kept: past it, the oldest goes (an agent gone without being closed keeps
/// its word otherwise).
const SAID_MAX: usize = 4096;

/// Says `c` to `agent` for goal `item` (none: a clear), at `now`, one word at a time: a message,
/// while the stack's last one still waits in the agent's queue (behind a turn under way), goes as
/// an edit of that one, or not at all when it says the same, so goals, reminders and clears never
/// stack up there; anything else goes as it is. A word a server before this one left waiting
/// (`queued::from_before`) is the one waiting. How it went, and the message that takes it.
async fn say<P>(port: &P, agent: &str, item: Option<&str>, c: Command, now: i64) -> (Sent, Option<String>)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let Command::Send { id, text, .. } = &c else { return (port.send(agent, c).await, None) };
    let (id, text) = (id.clone(), text.clone());
    let info = port.agent(agent);
    let waiting = {
        let mut said = SAID.lock();
        let w = said.get_mut(agent).and_then(|w| (w.msg.look(info.as_ref(), now) == Now::Waiting).then(|| (w.msg.id.clone(), w.msg.text == text)));
        if w.is_none() {
            said.remove(agent);
        }
        // Its words not known, what it holds is known by its digest: the same words, or an edit.
        w.or_else(|| {
            let q = info.as_ref()?.queue.iter().find(|q| crate::harness::queued::from_before(&q.id, GOAL))?;
            let same = q.digest == crate::harness::queued::digest(&text);
            said.insert(agent.to_owned(), Word { msg: Kept::found(q.id.clone(), if same { text.clone() } else { String::new() }, now), item: None });
            Some((q.id.clone(), same))
        })
    };
    let item = item.map(str::to_owned);
    match waiting {
        Some((waits, true)) => {
            if let Some(w) = SAID.lock().get_mut(agent) {
                w.item = item;
            }
            (Sent::Delivered, Some(waits))
        }
        Some((waits, false)) => {
            let sent = port.send(agent, Command::QueueEdit { id: waits.clone(), text: text.clone() }).await;
            if sent == Sent::Delivered
                && let Some(w) = SAID.lock().get_mut(agent)
            {
                w.msg.text = text;
                w.item = item;
            }
            (sent, Some(waits))
        }
        None => {
            let sent = port.send(agent, c).await;
            if sent == Sent::Delivered
                && let Some(to) = info.as_ref()
            {
                let mut said = SAID.lock();
                if said.len() >= SAID_MAX
                    && !said.contains_key(agent)
                    && let Some(oldest) = said.iter().min_by_key(|(_, w)| w.msg.at()).map(|(a, _)| a.clone())
                {
                    said.remove(&oldest);
                }
                said.insert(agent.to_owned(), Word { msg: Kept::sent(id.clone(), text, to, now), item });
            }
            (sent, Some(id))
        }
    }
}

/// The stacks' words that went from their agents' queues before the edit that last changed them
/// was seen there (it may have come too late, and done nothing): each said again, at `now`, in a
/// message of its own. The rest gone are forgotten; an agent not known right now keeps its word.
async fn resay<P>(port: &P, now: i64)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let agents: Vec<String> = SAID.lock().keys().cloned().collect();
    let infos: Vec<(String, Info)> = agents.into_iter().filter_map(|a| port.agent(&a).map(|i| (a, i))).collect();
    let lost: Vec<(String, Word)> = {
        let mut said = SAID.lock();
        let mut lost = Vec::new();
        for (agent, info) in &infos {
            if said.get_mut(agent).is_some_and(|w| w.msg.look(Some(info), now) == Now::Gone)
                && let Some(w) = said.remove(agent)
                && w.msg.lost()
            {
                lost.push((agent.clone(), w));
            }
        }
        lost
    };
    for (agent, w) in lost {
        let c = send(w.msg.text.clone());
        let (sent, _) = say(port, &agent, w.item.as_deref(), c, now).await;
        // Not delivered: kept as it was, to say again at the next look (unless something was said since).
        if sent != Sent::Delivered {
            tracing::info!(%agent, "a goal stack's word, said again, didn't reach its agent");
            SAID.lock().entry(agent).or_insert(w);
        }
    }
}

/// Takes the messages that last took goals `which` picks to the agent out of its queue, where
/// they still wait (behind a turn under way): a goal that ended, or went back among the others,
/// before the agent took it doesn't reach it after (where, set as its harness's own goal, it would
/// join the stack again as one set in the agent). A message the agent took already is left as it
/// is, and so is the stack's word waiting when it says something else by now (`say`).
async fn withdraw<P>(port: &P, agent: &str, items: &[Item], which: impl Fn(&Item) -> bool)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let mut gone: Vec<String> = Vec::new();
    for i in items.iter().filter(|i| which(i)) {
        let mut said = SAID.lock();
        match said.get(agent) {
            // The word waiting takes this goal: it goes.
            Some(w) if w.item.as_deref() == Some(i.id.as_str()) => {
                gone.push(w.msg.id.clone());
                said.remove(agent);
            }
            // The message that took it says something else by now.
            Some(w) if i.sent_id.as_deref() == Some(w.msg.id.as_str()) => {}
            // No word of it kept (the server started since): by the message kept with the goal,
            // unless a goal still to go has that one too.
            _ => {
                let shared = |id: &String| items.iter().any(|o| !which(o) && matches!(o.status.as_str(), "pending" | "active") && o.sent_id.as_ref() == Some(id));
                if let Some(id) = i.sent_id.as_ref().filter(|id| !shared(id) && !gone.contains(id)) {
                    gone.push(id.clone());
                }
            }
        }
    }
    for id in gone {
        let _ = port.send(agent, Command::QueueRemove { id }).await;
    }
}

/// What a goal is beside its words: the goal it's a part of and who made it so, the plan step
/// it's made from, the plan run it waits for.
#[derive(Debug, Clone, Copy, Default)]
struct Noted<'a> {
    part_of: Option<&'a str>,
    kind: Option<&'a str>,
    step: Option<&'a StepRef>,
    waits_on: Option<&'a str>,
}

/// Notes goals `ids` as `n` says.
fn note(c: &rusqlite::Connection, ids: &[String], n: Noted<'_>) -> anyhow::Result<()> {
    let step = n.step;
    for id in ids {
        c.execute(
            "UPDATE goal_items SET part_of = ?2, plan_id = ?3, plan_version = ?4, plan_step = ?5, plan_run = ?6, part_kind = ?7, waits_on = ?8 WHERE id = ?1",
            params![
                id,
                n.part_of,
                step.map(|s| s.plan.clone()),
                step.map(|s| s.version),
                step.map(|s| i64::try_from(s.n.saturating_sub(1)).unwrap_or(0)),
                step.and_then(|s| s.run.clone()),
                n.kind,
                n.waits_on
            ],
        )?;
    }
    Ok(())
}

/// The goals that are parts of goal `id`, and parts of those, as far as they go.
fn descendants(items: &[Item], id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut frontier = vec![id.to_owned()];
    while let Some(p) = frontier.pop() {
        for i in items.iter().filter(|i| i.part_of.as_deref() == Some(p.as_str())) {
            if !out.contains(&i.id) && i.id != id {
                out.push(i.id.clone());
                frontier.push(i.id.clone());
            }
        }
    }
    out
}

/// The goals still to go, in order (the one under way first), each by its id: what `set_locked`
/// takes to keep them as they are.
fn to_go(items: &[Item]) -> Vec<(Option<String>, String)> {
    let under_way = items.iter().filter(|i| i.status == "active");
    under_way.chain(items.iter().filter(|i| i.status == "pending")).map(|i| (Some(i.id.clone()), i.text.clone())).collect()
}

/// Where in `given` (the goals still to go, as [`to_go`] lists them) goals go to be at `at`.
fn place_of(given: &[(Option<String>, String)], items: &[Item], at: &At) -> R<usize> {
    let find = |id: &str| given.iter().position(|(g, _)| g.as_deref() == Some(id)).ok_or_else(|| Error::Bad(format!("no goal {id} still to go here (goals_get lists them)")));
    Ok(match at {
        At::End => given.len(),
        // Right after the goal under way, else first.
        At::Next => usize::from(items.iter().any(|i| i.status == "active")),
        At::Before(id) => find(id)?,
        At::After(id) => find(id)? + 1,
    })
}

/// The goals of a stack that weren't in it before (`before`, its ids), in order.
fn added(s: &Stack, before: &[String]) -> Vec<String> {
    s.items.iter().filter(|i| !before.contains(&i.id)).map(|i| i.id.clone()).collect()
}

/// Puts the goals done first (as they were), then the one under way, then those to come (each as
/// they were): after a goal's status changed where it stood.
fn renumber(c: &rusqlite::Connection, agent: &str) -> anyhow::Result<()> {
    let mut st = c.prepare("SELECT id FROM goal_items WHERE agent = ?1 ORDER BY CASE status WHEN 'pending' THEN 2 WHEN 'active' THEN 1 ELSE 0 END, pos")?;
    let ids = st.query_map(params![agent], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    for (pos, id) in ids.iter().enumerate() {
        c.execute("UPDATE goal_items SET pos = ?2 WHERE id = ?1", params![id, i64::try_from(pos).unwrap_or(i64::MAX)])?;
    }
    Ok(())
}

fn touch(c: &rusqlite::Connection, agent: &str, state: Option<&str>, reason: Option<&str>, now: i64) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO goal_stacks (agent, state, reason, created_at, updated_at) VALUES (?1, COALESCE(?2, 'paused'), ?3, ?4, ?4)
         ON CONFLICT(agent) DO UPDATE SET state = COALESCE(?2, state), reason = CASE WHEN ?2 IS NULL THEN reason ELSE ?3 END,
           held_for = CASE WHEN ?2 IS NULL THEN held_for ELSE NULL END, updated_at = ?4",
        params![agent, state, reason, now],
    )?;
    Ok(())
}

fn known(hub: &Hub, agent: &str) -> R<Info> {
    hub.harness.list().into_iter().find(|i| i.id == agent).ok_or(Error::NoAgent)
}

/// Sets the goals still to go: `given`, in that order (existing ones by id, or by their words
/// alone, edited; new ones added). The goal under way may be among them: first, it stays under way
/// (with other words, it goes to the agent again as they say); after others, it goes back among
/// them (cleared in the agent) and the first goes next. Left out, it stays under way, before them.
/// Goals done stay as they are, and one given by id isn't added again.
pub async fn set(hub: &Hub, agent: &str, given: Vec<Given>) -> R<Stack> {
    let info = known(hub, agent)?;
    let given = given.into_iter().map(|g| Ok((g.id, clean(&g.text)?))).collect::<R<Vec<_>>>()?;
    let _one = CHANGING.lock().await;
    set_locked(hub, &info, agent, given).await
}

/// `set`, with the stacks held (`CHANGING`).
async fn set_locked(hub: &Hub, info: &Info, agent: &str, given: Vec<(Option<String>, String)>) -> R<Stack> {
    let stack = get(hub, agent).await?;
    let (rows, gone) = arrange(&stack.items, given);
    let kept = rows.iter().filter(|r| matches!(r, Row::Kept(_) | Row::Reworded { .. })).count();
    if !fits(kept, rows.len() - kept) {
        return Err(Error::Bad(format!("a stack holds at most {ITEMS} goals")));
    }
    let back = rows.iter().any(|r| matches!(r, Row::Back { .. }));
    let moved = back || rows.iter().any(|r| matches!(r, Row::Reworded { .. }));
    // The goal under way put back among the others: its harness's own goal goes, so nothing holds
    // the agent to it; and the goal under way, back or in other words, doesn't reach the agent as
    // it was if that still waits in its queue.
    if back {
        clear_in_agent(hub, info, &stack.items, now_ms(hub)).await;
    }
    if moved {
        withdraw(hub, agent, &stack.items, |i| i.status == "active").await;
    }
    let agent_s = agent.to_owned();
    let now = now_ms(hub);
    let running = hub
        .store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            for (pos, row) in rows.iter().enumerate() {
                let pos = i64::try_from(pos).unwrap_or(i64::MAX);
                match row {
                    Row::Kept(id) => tx.execute("UPDATE goal_items SET pos = ?2 WHERE id = ?1", params![id, pos])?,
                    // Sent again as it now says, at once (as when you resume it).
                    Row::Reworded { id, text } => tx.execute("UPDATE goal_items SET pos = ?2, text = ?3, reminded_at = 0, reminders = 0, busy_at = 0 WHERE id = ?1", params![id, pos, text])?,
                    Row::Next { id: Some(id), text } => tx.execute("UPDATE goal_items SET pos = ?2, text = ?3 WHERE id = ?1", params![id, pos, text])?,
                    Row::Next { id: None, text } => tx.execute("INSERT INTO goal_items (id, agent, pos, text) VALUES (?1, ?2, ?3, ?4)", params![crate::util::new_id(), agent_s, pos, text])?,
                    Row::Back { id, text } => tx.execute(
                        "UPDATE goal_items SET status = 'pending', pos = ?2, text = ?3, reason = NULL, how = NULL, started_at = NULL, reminded_at = 0, reminders = 0, busy_at = 0, sent_id = NULL WHERE id = ?1",
                        params![id, pos, text],
                    )?,
                };
            }
            for id in &gone {
                tx.execute("DELETE FROM goal_items WHERE id = ?1", params![id])?;
            }
            touch(&tx, &agent_s, None, None, now)?;
            // Its goal under way changed: what the agent still pursues of the one before doesn't join
            // the stack again (`adopt`).
            if moved {
                tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
            }
            // Done, then given goals to go: no longer done, and yours to start.
            tx.execute(
                "UPDATE goal_stacks SET state = 'paused', reason = NULL WHERE agent = ?1 AND state = 'done' AND EXISTS (SELECT 1 FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active'))",
                params![agent_s],
            )?;
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![agent_s], |r| r.get(0))?;
            tx.commit()?;
            Ok(state == "running")
        })
        .await?;
    // A running stack whose goal under way changed: what's first now goes to the agent at once.
    if moved && running {
        push(hub, agent, crate::metrics::agents()).await;
    }
    get(hub, agent).await
}

/// Whether `kept` goals done or under way, and `given` more, fit in one stack.
fn fits(kept: usize, given: usize) -> bool {
    kept.checked_add(given).is_some_and(|n| n <= ITEMS)
}

/// A stack as an edit leaves it, in order: the goals done as they are, then the goal under way
/// (if it stays so), then the goals to come.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// A goal as it is, in its place: done, or under way.
    Kept(String),
    /// The goal under way with other words: it goes to the agent again as they say.
    Reworded { id: String, text: String },
    /// A goal to come: one there (`id`) with these words, or a new one.
    Next { id: Option<String>, text: String },
    /// The goal under way, put back among the goals to come.
    Back { id: String, text: String },
}

/// What `set` makes of `items`, given the goals still to go (`set` says how), and the goals to
/// come that go (left out).
fn arrange(items: &[Item], given: Vec<(Option<String>, String)>) -> (Vec<Row>, Vec<String>) {
    let still = |i: &Item| matches!(i.status.as_str(), "pending" | "active");
    let status = |id: &str| items.iter().find(|i| i.id == id).map(|i| i.status.clone());
    // Each goal there at most once (said again, it's a new one); goals done are left where they are,
    // and one unknown (gone since) is a new one.
    let mut seen: Vec<String> = Vec::new();
    let mut to_go: Vec<(Option<String>, String)> = Vec::new();
    for (id, text) in given {
        match id.filter(|id| !seen.contains(id)).map(|id| (status(&id), id)) {
            Some((Some(s), id)) if matches!(s.as_str(), "pending" | "active") => {
                seen.push(id.clone());
                to_go.push((Some(id), text));
            }
            Some((Some(_), _)) => {}
            _ => to_go.push((None, text)),
        }
    }
    // A goal given by its words alone is the one still to go with those words, unless that one was
    // named: a list in another order, without ids, reorders the goals rather than copying them.
    for (id, text) in to_go.iter_mut().filter(|(id, _)| id.is_none()) {
        if let Some(i) = items.iter().find(|i| still(i) && !seen.contains(&i.id) && same(&i.text, text)) {
            seen.push(i.id.clone());
            *id = Some(i.id.clone());
        }
    }
    let mut rows: Vec<Row> = items.iter().filter(|i| !still(i)).map(|i| Row::Kept(i.id.clone())).collect();
    let active = items.iter().find(|i| i.status == "active");
    let at = active.and_then(|a| to_go.iter().position(|(id, _)| id.as_deref() == Some(a.id.as_str())));
    if let Some(a) = active {
        match (at, to_go.first()) {
            (Some(0), Some((_, text))) if !same(&a.text, text) => rows.push(Row::Reworded { id: a.id.clone(), text: text.clone() }),
            (None | Some(0), _) => rows.push(Row::Kept(a.id.clone())),
            _ => {}
        }
    }
    for (k, (id, text)) in to_go.into_iter().enumerate() {
        match id {
            Some(_) if at == Some(0) && k == 0 => {}
            Some(id) if at == Some(k) => rows.push(Row::Back { id, text }),
            id => rows.push(Row::Next { id, text }),
        }
    }
    let gone =
        items.iter().filter(|i| i.status == "pending" && !rows.iter().any(|r| matches!(r, Row::Next { id: Some(id), .. } | Row::Back { id, .. } if *id == i.id))).map(|i| i.id.clone()).collect();
    (rows, gone)
}

/// Adds goals after the ones there; `start` runs the stack.
pub async fn add(hub: &Hub, agent: &str, texts: Vec<String>, start: bool) -> R<Stack> {
    add_at(hub, agent, texts.into_iter().map(NewGoal::from).collect(), start, &At::End).await
}

/// Adds goals where `at` says (before the goal under way, it goes back among them and the first
/// goes next, as `set` does it), each saying the plan step it's made from; `start` runs the stack.
pub async fn add_at(hub: &Hub, agent: &str, goals: Vec<NewGoal>, start: bool, at: &At) -> R<Stack> {
    let info = known(hub, agent)?;
    let goals = goals.into_iter().map(|g| Ok(NewGoal { text: clean(&g.text)?, ..g })).collect::<R<Vec<_>>>()?;
    let stack = {
        let _one = CHANGING.lock().await;
        let now = get(hub, agent).await?;
        // Every goal still to go named, the one under way first (where it stays, unless they go
        // before it): what's added is new, whatever its words.
        let mut given = to_go(&now.items);
        let k = place_of(&given, &now.items, at)?;
        for (j, g) in goals.iter().enumerate() {
            given.insert(k + j, (None, g.text.clone()));
        }
        let before: Vec<String> = now.items.iter().map(|i| i.id.clone()).collect();
        let s = set_locked(hub, &info, agent, given).await?;
        // Each new one, in the order given, with the step it's made from.
        if goals.iter().any(|g| g.step.is_some() || g.part_of.is_some() || g.kind.is_some() || g.waits_on.is_some()) {
            let pairs: Vec<(String, NewGoal)> = added(&s, &before).into_iter().zip(goals).collect();
            hub.store
                .call(move |c| {
                    for (id, g) in &pairs {
                        note(c, std::slice::from_ref(id), Noted { part_of: g.part_of.as_deref(), kind: g.kind, step: g.step.as_ref(), waits_on: g.waits_on.as_deref() })?;
                    }
                    Ok(())
                })
                .await?;
            get(hub, agent).await?
        } else {
            s
        }
    };
    if start { control(hub, agent, Action::Start).await } else { Ok(stack) }
}

/// Goal `id` (without: the one under way) is met, as `by` says (`evidence`: what shows it). It ends
/// as met, and its parts still to go ([`split`]) with it; the goal under way among them is cleared
/// in the agent's harness (nothing holds the agent to it any more) and, the stack running, what's
/// next goes to the agent at once. A goal skipped or not met can be said met after all; one met
/// already stays as it is.
pub async fn done(hub: &Hub, agent: &str, id: Option<&str>, evidence: Option<&str>, by: &By) -> R<Stack> {
    done_counted(hub, agent, id, evidence, by, crate::metrics::agents()).await
}

/// [`done`], the goals it ends (the one said met and its parts still to go) counted into `metrics`.
async fn done_counted(hub: &Hub, agent: &str, id: Option<&str>, evidence: Option<&str>, by: &By, metrics: &crate::metrics::AgentMetrics) -> R<Stack> {
    let info = known(hub, agent)?;
    let evidence = evidence.map(|e| e.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|e| !e.is_empty());
    if evidence.as_ref().is_some_and(|e| e.chars().count() > TEXT) {
        return Err(Error::Bad(format!("what shows it's met is at most {TEXT} characters")));
    }
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    let target = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => stack.items.iter().find(|i| i.id == id).ok_or_else(|| Error::Bad(format!("no goal {id} in this stack (goals_get lists them)")))?,
        None => stack.items.iter().find(|i| i.status == "active").ok_or_else(|| Error::Bad("no goal is under way here: say which one is met by its `id` (goals_get lists them)".into()))?,
    };
    if target.status == "met" {
        return Ok(stack);
    }
    let target_id = target.id.clone();
    let parts = descendants(&stack.items, &target_id);
    let ending = |i: &Item| i.id == target_id || (parts.contains(&i.id) && matches!(i.status.as_str(), "pending" | "active"));
    let ended_parts: Vec<String> = stack.items.iter().filter(|i| i.id != target_id && ending(i)).map(|i| i.id.clone()).collect();
    let was_active = stack.items.iter().any(|i| i.status == "active" && ending(i));
    if was_active {
        clear_in_agent(hub, &info, &stack.items, now_ms(hub)).await;
    }
    withdraw(hub, agent, &stack.items, &ending).await;
    let reason = format!("{} said so{}", by.name(), evidence.map(|e| format!(": {e}")).unwrap_or_default());
    let (agent_s, now) = (agent.to_owned(), now_ms(hub));
    let running = hub
        .store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let mut met = tx.execute("UPDATE goal_items SET status = 'met', reason = ?2, ended_at = ?3 WHERE id = ?1", params![target_id, reason, now])?;
            for id in &ended_parts {
                met += tx.execute("UPDATE goal_items SET status = 'met', reason = 'Met with the goal it is a part of.', ended_at = ?2 WHERE id = ?1", params![id, now])?;
            }
            renumber(&tx, &agent_s)?;
            touch(&tx, &agent_s, None, None, now)?;
            // What the agent's harness still has of it doesn't join the stack again (`adopt`).
            if was_active {
                tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
            }
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![agent_s], |r| r.get(0))?;
            tx.commit()?;
            Ok((state == "running", met))
        })
        .await?;
    let (running, met) = running;
    for _ in 0..met {
        metrics.goal_ended(true);
    }
    if running && was_active {
        push(hub, agent, metrics).await;
    }
    get(hub, agent).await
}

/// Goal `id`, still to go, changed as `change` says: other words (the goal under way, sent again as
/// they say), moved (before or after another goal still to go, or next: right after the goal under
/// way; put before the goal under way, it goes next and that one goes back among the others), or
/// taken out (the goal under way ends as skipped, its harness's own goal cleared; one to come goes,
/// with its parts).
pub async fn update(hub: &Hub, agent: &str, id: &str, change: Change, by: &By) -> R<Stack> {
    update_counted(hub, agent, id, change, by, crate::metrics::agents()).await
}

/// [`update`], the goal under way taken out (it ends, skipped) counted into `metrics`.
async fn update_counted(hub: &Hub, agent: &str, id: &str, change: Change, by: &By, metrics: &crate::metrics::AgentMetrics) -> R<Stack> {
    let info = known(hub, agent)?;
    let text = change.text.as_deref().map(clean).transpose()?;
    let at = match (change.before.clone(), change.after.clone(), change.next) {
        (None, None, false) => None,
        (Some(b), None, false) => Some(At::Before(b)),
        (None, Some(a), false) => Some(At::After(a)),
        (None, None, true) => Some(At::Next),
        _ => return Err(Error::Bad("move it one way: `before`, `after` or `next`".into())),
    };
    if change.remove && (text.is_some() || at.is_some()) {
        return Err(Error::Bad("a goal taken out isn't changed or moved too".into()));
    }
    if !change.remove && text.is_none() && at.is_none() {
        return Err(Error::Bad("say what changes: `text`, `before`, `after`, `next` or `remove`".into()));
    }
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    let it = stack.items.iter().find(|i| i.id == id).ok_or_else(|| Error::Bad(format!("no goal {id} in this stack (goals_get lists them)")))?;
    if !matches!(it.status.as_str(), "pending" | "active") {
        return Err(Error::Bad(format!("that goal is {} already and stays as it was: add it again (goals_add) to pursue it once more", it.status)));
    }
    if change.remove {
        let (active, gone, who, now, agent_s) = (it.status == "active", id.to_owned(), by.name(), now_ms(hub), agent.to_owned());
        if active {
            clear_in_agent(hub, &info, &stack.items, now_ms(hub)).await;
        }
        let parts = descendants(&stack.items, id);
        let gone_parts: Vec<String> = stack.items.iter().filter(|i| parts.contains(&i.id) && i.status == "pending").map(|i| i.id.clone()).collect();
        withdraw(hub, agent, &stack.items, |i| i.id == id || (parts.contains(&i.id) && i.status == "pending")).await;
        let running = hub
            .store
            .call(move |c| {
                let tx = c.unchecked_transaction()?;
                if active {
                    tx.execute("UPDATE goal_items SET status = 'skipped', ended_at = ?2, reason = ?3 WHERE id = ?1", params![gone, now, format!("Taken out by {who}.")])?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                } else {
                    tx.execute("DELETE FROM goal_items WHERE id = ?1", params![gone])?;
                    for p in &gone_parts {
                        tx.execute("DELETE FROM goal_items WHERE id = ?1", params![p])?;
                    }
                }
                renumber(&tx, &agent_s)?;
                touch(&tx, &agent_s, None, None, now)?;
                let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![agent_s], |r| r.get(0))?;
                tx.commit()?;
                Ok(state == "running")
            })
            .await?;
        if active {
            metrics.goal_ended(false);
        }
        if running && active {
            push(hub, agent, metrics).await;
        }
        return get(hub, agent).await;
    }
    let mut given = to_go(&stack.items);
    if let (Some(t), Some(g)) = (text, given.iter_mut().find(|(g, _)| g.as_deref() == Some(id))) {
        g.1 = t;
    }
    if let Some(at) = at {
        if matches!(&at, At::Before(x) | At::After(x) if x == id) {
            return Err(Error::Bad("move it before or after another goal, not itself".into()));
        }
        let k = given.iter().position(|(g, _)| g.as_deref() == Some(id)).unwrap_or(0);
        let me = given.remove(k);
        // The goal under way, moved next, stays under way.
        let to = if at == At::Next && it.status == "active" { 0 } else { place_of(&given, &stack.items, &at)? };
        given.insert(to.min(given.len()), me);
    }
    set_locked(hub, &info, agent, given).await
}

/// Goal `id` (without: the one under way) split into parts, pursued first, each in turn, before
/// the goal itself comes back for its own check: `parts`, or, none given, the acceptance criteria
/// and tests of the plan step it's made from, a goal each. They go right before it (after any parts
/// it has already); the goal under way split goes back among the others and its first part goes
/// next.
pub async fn split(hub: &Hub, agent: &str, id: Option<&str>, parts: Vec<String>) -> R<Stack> {
    let info = known(hub, agent)?;
    let given_parts = parts.iter().map(|p| clean(p)).collect::<R<Vec<_>>>()?;
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    let target = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => stack.items.iter().find(|i| i.id == id).ok_or_else(|| Error::Bad(format!("no goal {id} in this stack (goals_get lists them)")))?,
        None => stack.items.iter().find(|i| i.status == "active").ok_or_else(|| Error::Bad("no goal is under way here: say which to split by its `id` (goals_get lists them)".into()))?,
    };
    if !matches!(target.status.as_str(), "pending" | "active") {
        return Err(Error::Bad(format!("that goal is {} already: there's nothing left of it to split", target.status)));
    }
    // A part splits only when it's a plan's step (into its criteria and tests, or parts of its own).
    if target.part_of.is_some() && target.part_kind.as_deref() != Some("plan") {
        return Err(Error::Bad("that goal is a part of another already: add goals before it instead (goals_add with `before`)".into()));
    }
    let kind = if given_parts.is_empty() { "checks" } else { "own" };
    let parts = if given_parts.is_empty() {
        let Some(s) = &target.step else {
            return Err(Error::Bad("say its parts in `goals`, a goal each (a goal made from a plan's step splits into its acceptance criteria and tests by itself)".into()));
        };
        let parts = step_parts(hub, s).await?;
        if parts.is_empty() {
            return Err(Error::Bad("its plan step has no acceptance criteria or tests to split it into: say its parts in `goals`".into()));
        }
        parts
    } else {
        given_parts
    };
    let mut given = to_go(&stack.items);
    let k = given.iter().position(|(g, _)| g.as_deref() == Some(target.id.as_str())).unwrap_or(given.len());
    for (j, p) in parts.into_iter().enumerate() {
        given.insert(k + j, (None, p));
    }
    let before: Vec<String> = stack.items.iter().map(|i| i.id.clone()).collect();
    let (target_id, step) = (target.id.clone(), target.step.clone());
    let s = set_locked(hub, &info, agent, given).await?;
    let new = added(&s, &before);
    hub.store.call(move |c| note(c, &new, Noted { part_of: Some(&target_id), kind: Some(kind), step: step.as_ref(), waits_on: None })).await?;
    get(hub, agent).await
}

/// The parts plan step `s` splits into: its acceptance criteria, then its tests, a goal each.
async fn step_parts(hub: &Hub, s: &StepRef) -> R<Vec<String>> {
    let gone = || Error::Bad("the plan this goal was made from is gone: say its parts in `goals`".into());
    let plan = crate::plans::get(hub, &s.plan).await.map_err(|_| gone())?;
    let at = s.n.saturating_sub(1);
    let item = if plan.version == s.version {
        plan.items.get(at).cloned()
    } else {
        crate::plans::versions(hub, &s.plan).await.map_err(|_| gone())?.into_iter().find(|v| v.version == s.version).and_then(|v| v.items.get(at).cloned())
    };
    Ok(item.map(|it| crate::plans::part_goals(&plan.title, s.n, &it)).unwrap_or_default())
}

/// Starts, pauses, skips, clears or brings back the agent's stack.
pub async fn control(hub: &Hub, agent: &str, action: Action) -> R<Stack> {
    control_as(hub, agent, action, &By::You, Pausing::default()).await
}

/// [`control`], as `by` does it: a pause says who paused it and why, and one `pausing.until` a time
/// resumes by itself then (the driver resumes it: [`Driver::drive`]); a goal skipped says who
/// skipped it.
pub async fn control_as(hub: &Hub, agent: &str, action: Action, by: &By, pausing: Pausing) -> R<Stack> {
    control_counted(hub, agent, action, by, pausing, crate::metrics::agents()).await
}

/// [`control_as`], the goals it ends (skipped, or said met) counted into `metrics`.
async fn control_counted(hub: &Hub, agent: &str, action: Action, by: &By, pausing: Pausing, metrics: &crate::metrics::AgentMetrics) -> R<Stack> {
    if action == Action::Done {
        return done_counted(hub, agent, None, None, by, metrics).await;
    }
    let info = known(hub, agent)?;
    let now = now_ms(hub);
    if let Some(until) = pausing.until.filter(|_| action == Action::Pause) {
        if until <= now {
            return Err(Error::Bad("a pause for a while ends after now".into()));
        }
        if until - now > PAUSE_MAX_MS {
            return Err(Error::Bad("a pause for a while is 30 days at the most: pause it without a time to pause it until someone resumes it".into()));
        }
    }
    let why = pausing.why.as_deref().map(|w| w.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|w| !w.is_empty());
    if why.as_ref().is_some_and(|w| w.chars().count() > TEXT) {
        return Err(Error::Bad(format!("why it's paused is at most {TEXT} characters")));
    }
    let paused_why = match (by, why) {
        (By::You, None) => PAUSED_BY_YOU.to_owned(),
        (by, Some(w)) => format!("Paused by {}: {w}", by.name()),
        (by, None) => format!("Paused by {}.", by.name()),
    };
    let skipped_why = format!("Skipped by {}.", by.name());
    let until = pausing.until.unwrap_or(0);
    // What waits on a person: a plan an agent (not you) wants you to look at first, or a pause its
    // agent made with no time to resume; yours, a parent's or one for a while don't.
    let held_for = match (by, &pausing.review) {
        (By::You, _) => None,
        (_, Some(plan)) => Some(plan.clone()),
        (By::Itself, None) if until == 0 => Some(String::new()),
        _ => None,
    };
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    if action == Action::Restore && stack.cleared == 0 {
        return Err(Error::Bad("no goals were cleared here to bring back".into()));
    }
    // Leaving a goal under way (or one still set that wasn't met), or pausing it: its harness's own
    // goal goes too, so nothing holds the agent to it.
    let active = stack.items.iter().any(|i| i.status == "active");
    if action == Action::Clear || (matches!(action, Action::Skip | Action::Pause) && active) {
        clear_in_agent(hub, &info, &stack.items, now).await;
    }
    // What still waits in its queue for the goal it leaves, pauses or clears doesn't reach it after.
    if matches!(action, Action::Clear | Action::Skip | Action::Pause) {
        withdraw(hub, agent, &stack.items, |i| matches!(i.status.as_str(), "active" | "pending")).await;
    }
    // What the agent last said of its goal: a verdict from before you resume doesn't count.
    let said_at = info.objective.as_ref().map_or(0, |o| o.at);
    let agent_s = agent.to_owned();
    let (running, skipped) = hub
        .store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let mut skipped = 0;
            match action {
                Action::Start => run_from_here(&tx, &agent_s, said_at, now)?,
                Action::Pause => {
                    touch(&tx, &agent_s, Some("paused"), Some(&paused_why), now)?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2, resume_at = ?3, held_for = ?4 WHERE agent = ?1", params![agent_s, now, until, held_for])?;
                }
                Action::Skip => {
                    skipped = tx.execute("UPDATE goal_items SET status = 'skipped', ended_at = ?2, reason = ?3 WHERE agent = ?1 AND status = 'active'", params![agent_s, now, skipped_why])?;
                    touch(&tx, &agent_s, None, None, now)?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Clear => {
                    // Kept as they were, to bring back (the last clear's; clearing none keeps those).
                    let items = load(&tx, &agent_s)?.items;
                    touch(&tx, &agent_s, Some("paused"), None, now)?;
                    if !items.is_empty() {
                        tx.execute("UPDATE goal_stacks SET cleared = ?2 WHERE agent = ?1", params![agent_s, serde_json::to_string(&items)?])?;
                    }
                    tx.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent_s])?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2, resume_at = 0 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Restore => restore(&tx, &agent_s, said_at, now)?,
                // Said met: `done`, before any of this.
                Action::Done => {}
            }
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![agent_s], |r| r.get(0))?;
            tx.commit()?;
            Ok((state == "running", skipped))
        })
        .await
        .map_err(|e| if e.to_string().contains("at most") { Error::Bad(e.to_string()) } else { Error::Other(e) })?;
    for _ in 0..skipped {
        metrics.goal_ended(false);
    }
    // You started or resumed it, skipped its goal or brought its goals back: what's next goes to
    // the agent now, whatever it's doing (behind a turn under way, by its harness's queue).
    if running && matches!(action, Action::Start | Action::Skip | Action::Restore) {
        push(hub, agent, metrics).await;
    }
    get(hub, agent).await
}

/// Runs the stack from where it stands, at `now`: the goal under way goes to the agent again at
/// once (its last send forgotten: the pause since it started is long over), reminders starting
/// over, a verdict from before (`said_at`: what its harness last said of its goal) not counting;
/// with nothing left, it's done. A pause for a while is over.
fn run_from_here(c: &rusqlite::Connection, agent: &str, said_at: i64, now: i64) -> anyhow::Result<()> {
    c.execute("UPDATE goal_items SET reminded_at = 0, reminders = 0, busy_at = 0, prior_at = MAX(prior_at, ?2) WHERE agent = ?1 AND status = 'active'", params![agent, said_at])?;
    let left = c.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent], |r| r.get::<_, i64>(0))?;
    touch(c, agent, Some(if left > 0 { "running" } else { "done" }), None, now)?;
    c.execute("UPDATE goal_stacks SET resume_at = 0 WHERE agent = ?1", params![agent])?;
    Ok(())
}

/// Brings back the goals cleared last (`merged` says where) and runs the stack, at `now`.
fn restore(c: &rusqlite::Connection, agent: &str, said_at: i64, now: i64) -> anyhow::Result<()> {
    let kept: Option<String> = c.query_row("SELECT cleared FROM goal_stacks WHERE agent = ?1", params![agent], |r| r.get(0)).optional()?.flatten();
    let back: Vec<Item> = serde_json::from_str(kept.as_deref().unwrap_or("[]"))?;
    let all = merged(load(c, agent)?.items, back, said_at);
    if all.len() > ITEMS {
        anyhow::bail!("a stack holds at most {ITEMS} goals");
    }
    c.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent])?;
    for (pos, mut i) in all.into_iter().enumerate() {
        // Its id taken since (by a goal of its own): a new one.
        if i.id.is_empty() || c.query_row("SELECT EXISTS(SELECT 1 FROM goal_items WHERE id = ?1)", params![i.id], |r| r.get::<_, bool>(0))? {
            i.id = crate::util::new_id();
        }
        c.execute(
            "INSERT INTO goal_items (id, agent, pos, text, status, reason, how, prior_at, started_at, ended_at, reminders, reminded_at, busy_at, delegated_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![i.id, agent, i64::try_from(pos).unwrap_or(i64::MAX), i.text, i.status, i.reason, i.how, i.prior_at, i.started_at, i.ended_at, i.reminders, i.reminded_at, i.busy_at, i.on],
        )?;
        note(c, std::slice::from_ref(&i.id), Noted { part_of: i.part_of.as_deref(), kind: i.part_kind.as_deref(), step: i.step.as_ref(), waits_on: i.waits_on.as_deref() })?;
    }
    let left = c.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent], |r| r.get::<_, i64>(0))?;
    touch(c, agent, Some(if left > 0 { "running" } else { "done" }), None, now)?;
    c.execute("UPDATE goal_stacks SET cleared = NULL, resume_at = 0 WHERE agent = ?1", params![agent])?;
    Ok(())
}

/// The goals cleared last (`back`), back in the stack as it is now (`now`): the goals done first
/// (those, then its own since), then the goal under way (its own; else the one that was, under way
/// again and sent again at once), then those to come, then its own.
fn merged(now: Vec<Item>, back: Vec<Item>, said_at: i64) -> Vec<Item> {
    let done = |i: &Item| !matches!(i.status.as_str(), "pending" | "active");
    let (mut out, back): (Vec<Item>, Vec<Item>) = back.into_iter().partition(done);
    let (now_done, now): (Vec<Item>, Vec<Item>) = now.into_iter().partition(done);
    out.extend(now_done);
    let (now_active, now_next): (Vec<Item>, Vec<Item>) = now.into_iter().partition(|i| i.status == "active");
    let busy = !now_active.is_empty();
    out.extend(now_active);
    for i in back {
        out.push(match i.status.as_str() {
            // A verdict from before doesn't count.
            "active" if !busy => Item { reminded_at: 0, reminders: 0, busy_at: 0, prior_at: said_at, ..i },
            // Another is under way now: it comes next.
            "active" => Item { status: "pending".into(), reason: None, how: None, started_at: None, reminded_at: 0, reminders: 0, busy_at: 0, ..i },
            _ => i,
        });
    }
    out.extend(now_next);
    out
}

/// What's next on a running stack goes to the agent now (`step` pushed): a failure is only logged,
/// since the change it follows stands. The driver's step, on the hub's parts: its clock, its store
/// and its agents (`hub: None`: a step tells no parents and moves no sessions), what it sends
/// counted into `metrics`.
async fn push(hub: &Hub, agent: &str, metrics: &crate::metrics::AgentMetrics) {
    let d = Driver { store: &hub.store, clock: hub.clock.as_ref(), shutdown: &hub.shutdown, port: hub, hub: None, metrics };
    if let Err(e) = step(&d, agent, true).await {
        tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
    }
}

/// Forgets an agent's stack (the agent was closed).
pub async fn forget(hub: &Hub, agent: &str) -> R<()> {
    SAID.lock().remove(agent);
    let agent = agent.to_owned();
    hub.store
        .call(move |c| {
            c.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent])?;
            c.execute("DELETE FROM goal_stacks WHERE agent = ?1", params![agent])?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// How a goal goes to this agent: its harness's /goal sent as text ("goal"), its goal control
/// ("goal_control"), or a message ("message"), from its command catalog.
fn how<P: HarnessPort>(port: &P, agent: &str) -> &'static str {
    let meta = port.catalog(agent);
    let goal = meta.as_ref().and_then(|m| m.get("commands")).and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some("goal")));
    match goal.and_then(|c| c.pointer("/route/kind")).and_then(Value::as_str) {
        Some("control") => "goal_control",
        // Sent as its words: the harness runs /goal itself (Claude Code).
        Some("prompt") | None if goal.is_some() => "goal",
        _ => "message",
    }
}

fn goal_command(how: &str, text: &str) -> Command {
    match how {
        "goal_control" => Command::Control { id: "goal".into(), value: Some(json!({"text": text})) },
        "goal" => send(format!("/goal {text}")),
        _ => send(format!("Your goal: {text}\n\nWork on it until it's met. When it is, end your reply with this line: {MET_LINE}")),
    }
}

/// A goal as it first goes to the agent (or goes again once you resumed it or gave it other words).
/// A stopped agent starts again for a message (a control waits for it to run), so to one that
/// stopped a goal its harness takes as a control goes as words.
fn first_command(how: &str, text: &str, stopped: bool) -> Command {
    if stopped && how == "goal_control" { send(format!("Your goal: {text}\n\nWork on it until it's met.")) } else { goal_command(how, text) }
}

/// The goal under way, sent again. A stopped agent starts again for a message (a control waits
/// for it to run), so to one that stopped the goal goes as words.
fn reminder_command(how: &str, text: &str, stopped: bool) -> Command {
    match how {
        "goal_control" if stopped => send(format!("Your goal isn't met yet: {text}\n\nKeep working on it until it is.")),
        "goal" | "goal_control" => goal_command(how, text),
        _ => send(format!("Your goal isn't met yet: {text}\n\nKeep working on it until it is. When it's met, end your reply with this line: {MET_LINE}")),
    }
}

/// A message of the stack's (its id says so: `queued::GOAL`).
fn send(text: String) -> Command {
    Command::Send { id: crate::harness::queued::made(GOAL), text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None }
}

/// The same goal: the same words, however they're spaced (a goal is kept as one line).
fn same(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// The agent's last words end with the line that says its goal is met.
fn said_met(summary: &str) -> bool {
    summary
        .lines()
        .map(|l| l.trim().trim_matches(|c: char| matches!(c, '*' | '_' | '`' | '#' | '>' | '.' | '!' | ' ')))
        .rfind(|l| !l.is_empty())
        .is_some_and(|l| l.eq_ignore_ascii_case(MET_LINE))
}

/// How long the agent is left before the goal is sent again, after `reminders` in a row.
fn gap(reminders: i64) -> i64 {
    let doublings = u32::try_from(reminders.clamp(0, 16)).unwrap_or(16);
    REMIND_MS.saturating_mul(1_i64 << doublings).min(REMIND_MAX_MS)
}

/// Clears the stack's goal in the agent's harness, where it has goals of its own: the one under
/// way, or one it stopped pursuing while still set (Claude Code's after its checks gave up), so
/// the agent doesn't go on chasing it. Said at `now`, as the stack's word (`say`).
async fn clear_in_agent<P>(port: &P, info: &Info, items: &[Item], now: i64)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return };
    let Some(item) = items.iter().rev().find(|i| same(&i.text, &o.text) && i.how.is_some()) else { return };
    let c = match item.how.as_deref() {
        Some("goal_control") => Command::Control { id: "goal".into(), value: Some(json!({"text": "clear"})) },
        Some("goal") => goal_command("goal", "clear"),
        _ => return,
    };
    let _ = say(port, &info.id, None, c, now).await;
}

/// Drives every running stack: sends the next goal to an agent that's free, follows the one under
/// way to how it came out, and sends it again when the agent stops short of it. Goals set in an
/// agent directly join its stack first.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    Driver { store: &hub.store, clock: hub.clock.as_ref(), shutdown: &hub.shutdown, port: hub.as_ref(), hub: Some(&hub), metrics: crate::metrics::agents() }.drive().await
}

/// What the goal driver works with: the stacks' store, its clock, what stops it, and its agents.
pub struct Driver<'a, P> {
    pub store: &'a crate::store::Store,
    pub clock: &'a dyn kiln_seams::Clock,
    pub shutdown: &'a crate::rt::Shutdown,
    pub port: &'a P,
    /// The hub, for parents told of their children (`harness::told`) and paused sessions that go
    /// on elsewhere (`harness::elsewhere`): none for the driver alone (its tests), and for a step
    /// of yours that goes at once (`push`).
    pub hub: Option<&'a Arc<Hub>>,
    /// What it counts its sends and the goals it saw end into (kiln's, `metrics::agents()`).
    pub metrics: &'a crate::metrics::AgentMetrics,
}

impl<P> Driver<'_, P>
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    /// Every [`TICK`] by its clock, until it's shut down: each running stack stepped, goals set
    /// in agents adopted, agents no stack drives kept going. Each agent's adoption and step is one
    /// change to the stacks (`CHANGING`), as each of yours is.
    pub async fn drive(&self) -> anyhow::Result<()> {
        let mut ticks: u64 = 0;
        loop {
            if !self.tick().await {
                return Ok(());
            }
            ticks = ticks.wrapping_add(1);
            for info in self.port.agents() {
                let _one = CHANGING.lock().await;
                if let Err(e) = adopt(self, &info).await {
                    tracing::warn!(agent = %info.id, error = %format!("{e:#}"), "goal stack");
                }
            }
            // Stacks paused for a while whose time is up: running again, what's next going at once.
            let now = self.clock.unix_ms();
            let due: Vec<String> = self
                .store
                .call(move |c| {
                    let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'paused' AND resume_at > 0 AND resume_at <= ?1")?;
                    let v = st.query_map(params![now], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                    Ok(v)
                })
                .await
                .unwrap_or_default();
            for agent in &due {
                let _one = CHANGING.lock().await;
                if let Err(e) = resume_due(self, agent).await {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
                }
            }
            let running: Vec<String> = self
                .store
                .call(|c| {
                    let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'running'")?;
                    let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                    Ok(v)
                })
                .await
                .unwrap_or_default();
            for agent in &running {
                let _one = CHANGING.lock().await;
                if let Err(e) = step(self, agent, false).await {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
                }
            }
            resume_lifted(self).await;
            // What the stacks said that an edit may not have got to their agents: said again.
            {
                let _one = CHANGING.lock().await;
                resay(self.port, self.clock.unix_ms()).await;
            }
            // The agents no goal stack drives: kept going when they stop part-way (`keep`), but
            // for those `keep_going` leaves alone.
            let now = self.clock.unix_ms();
            let all = self.port.agents();
            let waiting: Vec<String> = self
                .store
                .call(|c| {
                    let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'paused' AND resume_at > 0")?;
                    let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                    Ok(v)
                })
                .await
                .unwrap_or_default();
            let held = paused_for_usage(self.store).await;
            let quiet: Vec<String> = all.iter().filter(|i| i.pause.is_some() || held.contains(&i.id)).map(|i| i.id.clone()).collect();
            for info in keep_going(&all, &running, &waiting, &quiet) {
                keep::tick(self.port, info, now, crate::harness::told::children_working(&all, &info.id)).await;
            }
            keep::retain(&all.iter().map(|i| i.id.clone()).collect::<Vec<_>>());
            if let Some(hub) = self.hub {
                // Programs' workers held to their usage thresholds, and settings' notices delivered
                // (docs/programs.md), every 15 s.
                if ticks.is_multiple_of(10) {
                    crate::programs::enforce(hub).await;
                }
                // Parents told of their children that settled, so none has to wait for them (one
                // that's quiet once it goes on).
                crate::harness::told::tick(hub, &all, now, &running, &quiet).await;
                // The paused ones that go on on another machine at a limit (`harness::elsewhere`).
                crate::harness::elsewhere::tick(hub, &all, now);
                // What agents' goals wait on a person for, on their info: Mission Control's Needs
                // you, and a card wherever you are in kiln.
                hub.harness.set_held(hub, &held_stacks(self.store).await);
            }
        }
    }

    /// Waits one [`TICK`] by its clock: false when it's shut down first.
    async fn tick(&self) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }
        let cancelled = pin!(self.shutdown.cancelled());
        let slept = self.clock.sleep(TICK);
        matches!(futures::future::select(cancelled, slept).await, futures::future::Either::Right(_))
    }
}

/// The agents keep-going looks at: those no goal stack drives (`running`), but for one that paused
/// its stack for a while (`waiting`: it waits on purpose, until it resumes) and one that's paused
/// for usage (`quiet`: its session at a limit or its threshold, or its stack for its program's
/// usage), which nothing prods (docs/programs.md, "Usage").
fn keep_going<'a>(all: &'a [Info], running: &[String], waiting: &[String], quiet: &[String]) -> Vec<&'a Info> {
    all.iter().filter(|i| !running.contains(&i.id) && !waiting.contains(&i.id) && !quiet.contains(&i.id)).collect()
}

/// A stack paused for a while, its time up: it runs again from where it stands (`run_from_here`),
/// and what's next goes to the agent at once (as when you resume it). One resumed or paused again
/// since is left as it is.
async fn resume_due<P>(d: &Driver<'_, P>, agent: &str) -> anyhow::Result<()>
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let said_at = d.port.agents().into_iter().find(|i| i.id == agent).and_then(|i| i.objective).map_or(0, |o| o.at);
    let (a, now) = (agent.to_owned(), d.clock.unix_ms());
    let running = d
        .store
        .call(move |c| {
            let due: bool = c.query_row("SELECT state = 'paused' AND resume_at > 0 AND resume_at <= ?2 FROM goal_stacks WHERE agent = ?1", params![a, now], |r| r.get(0)).optional()?.unwrap_or(false);
            if !due {
                return Ok(false);
            }
            let tx = c.unchecked_transaction()?;
            run_from_here(&tx, &a, said_at, now)?;
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![a], |r| r.get(0))?;
            tx.commit()?;
            Ok(state == "running")
        })
        .await?;
    if running {
        tracing::info!(%agent, "a goal stack paused for a while resumes");
        step(d, agent, true).await?;
    }
    Ok(())
}

/// A goal set in the agent that its stack doesn't know (`/goal` typed in its composer, or its
/// harness's own goal set some other way), pursued or given up on for now but still set, joins the
/// stack as the goal under way,
/// so kiln keeps it like the others; one the stack had under way goes back to the front of what's
/// next. The stack runs.
async fn adopt<P: HarnessPort>(d: &Driver<'_, P>, info: &Info) -> anyhow::Result<()> {
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return Ok(()) };
    let Ok(text) = clean(&o.text) else { return Ok(()) };
    let (agent, how, at, now, gave_up) = (info.id.clone(), how(d.port, &info.id), o.at, d.clock.unix_ms(), o.status == "unmet");
    d.store
        .call(move |c| {
            // One you cleared or skipped is being cleared in the agent: it doesn't come back.
            let dropped: i64 = c.query_row("SELECT dropped_at FROM goal_stacks WHERE agent = ?1", params![agent], |r| r.get(0)).optional()?.unwrap_or(0);
            if at <= dropped {
                return Ok(());
            }
            // Its harness giving up on a goal the stack knows, one it still had set: one the stack has
            // ended already (said met, skipped, taken out) or put back among the others (split into
            // parts, moved after another). That's kiln's own goal, not one set in the agent: it
            // doesn't come back either.
            if gave_up
                && c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM goal_items WHERE agent = ?1 AND text = ?2) AND NOT EXISTS(SELECT 1 FROM goal_items WHERE agent = ?1 AND text = ?2 AND status = 'active')",
                    params![agent, text],
                    |r| r.get::<_, bool>(0),
                )?
            {
                return Ok(());
            }
            let tx = c.unchecked_transaction()?;
            let active: Option<(String, String)> =
                tx.query_row("SELECT id, text FROM goal_items WHERE agent = ?1 AND status = 'active'", params![agent], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if active.as_ref().is_some_and(|(_, t)| same(t, &text)) {
                return Ok(());
            }
            // Where it goes: after the goals done, before the ones to come.
            let pos: i64 = tx.query_row("SELECT COALESCE(MAX(pos), -1) + 1 FROM goal_items WHERE agent = ?1 AND status NOT IN ('pending', 'active')", params![agent], |r| r.get(0))?;
            tx.execute("UPDATE goal_items SET pos = pos + 2 WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent])?;
            if let Some((id, _)) = &active {
                tx.execute(
                    "UPDATE goal_items SET status = 'pending', pos = ?2, how = NULL, started_at = NULL, reminded_at = 0, reminders = 0, busy_at = 0 WHERE id = ?1",
                    params![id, pos + 1],
                )?;
            }
            // Already on the stack, to come: that one, now; else a new one.
            let pending: Option<String> = tx.query_row("SELECT id FROM goal_items WHERE agent = ?1 AND status = 'pending' AND text = ?2 ORDER BY pos LIMIT 1", params![agent, text], |r| r.get(0)).optional()?;
            let id = pending.unwrap_or_else(crate::util::new_id);
            tx.execute(
                "INSERT INTO goal_items (id, agent, pos, text, status, how, prior_at, started_at, reminded_at) VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?7)
                 ON CONFLICT(id) DO UPDATE SET status = 'active', pos = ?3, how = ?5, prior_at = ?6, started_at = ?7, reminded_at = ?7, reminders = 0, busy_at = 0",
                params![id, agent, pos, text, how, at.saturating_sub(1), now],
            )?;
            touch(&tx, &agent, Some("running"), None, now)?;
            tx.commit()?;
            tracing::info!(%agent, "a goal set in the agent joins its goal stack");
            Ok(())
        })
        .await
}

/// The goal next to start whose parts kiln made (a plan's steps, its planning and its run; a
/// step's criteria and tests) are all met, and why it's met with them; none while a goal is under
/// way. A goal whose planning is its only part met isn't: its steps come next. As in [`decide`], a
/// goal under way on another agent (a program's worker's) isn't this one's next.
fn met_by_parts(stack: &Stack) -> Option<(String, String)> {
    if stack.items.iter().any(|i| i.status == "active") {
        return None;
    }
    let next = stack.items.iter().find(|i| i.status == "pending" && i.on.is_none())?;
    let parts: Vec<&Item> = stack.items.iter().filter(|i| i.part_of.as_deref() == Some(next.id.as_str())).collect();
    let kiln_made = |i: &&Item| matches!(i.part_kind.as_deref(), Some("plan" | "planning" | "run" | "checks"));
    if parts.is_empty() || !parts.iter().all(kiln_made) || !parts.iter().all(|i| i.status == "met") || parts.iter().all(|i| i.part_kind.as_deref() == Some("planning")) {
        return None;
    }
    let count = |k: &str| parts.iter().filter(|i| i.part_kind.as_deref() == Some(k)).count();
    let why = if count("run") > 0 {
        "Met with its plan: its run is done, every step met.".to_owned()
    } else if count("plan") > 0 {
        let n = count("plan");
        format!("Met with its plan: its {n} step{} met.", if n == 1 { " is" } else { "s are" })
    } else {
        "Met: each of its acceptance criteria and tests is met.".to_owned()
    };
    Some((next.id.clone(), why))
}

/// A goal turned into a plan ([`plan`]): the plan, its research, the goal, and the part of the
/// goal its planning is.
#[derive(Debug, Clone, Serialize)]
pub struct Planned {
    pub plan: String,
    pub research: String,
    pub goal: String,
    pub part: String,
}

/// Goal `id` (without: the one under way) of agent `agent` turned into a plan, its planning begun:
/// a plan in its planning phase (the goal's words its brief, saying the goal it came from), its
/// research, and a first part of the goal: plan it (research it inside and out, write its steps,
/// say it's ready). The goal under way waits after that part, which goes to the agent now. A goal
/// being planned already answers with the plan it's becoming.
pub async fn plan(hub: &Hub, agent: &str, id: Option<&str>, title: Option<&str>, by: &str) -> R<Planned> {
    let info = known(hub, agent)?;
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    let target = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => stack.items.iter().find(|i| i.id == id).ok_or_else(|| Error::Bad(format!("no goal {id} in this stack (goals_get lists them)")))?,
        None => stack.items.iter().find(|i| i.status == "active").ok_or_else(|| Error::Bad("no goal is under way here: say which to plan by its `id` (goals_get lists them)".into()))?,
    };
    if !matches!(target.status.as_str(), "pending" | "active") {
        return Err(Error::Bad(format!("that goal is {} already: there's nothing left of it to plan", target.status)));
    }
    if matches!(target.part_kind.as_deref(), Some("checks" | "planning" | "run" | "own")) {
        return Err(Error::Bad("that goal is a part of another: plan the goal it's a part of".into()));
    }
    let as_plan_error = |e: crate::plans::Error| match e {
        crate::plans::Error::Bad(m) | crate::plans::Error::Refused(_, m) => Error::Bad(m),
        crate::plans::Error::Missing => Error::Bad("no such plan".into()),
        crate::plans::Error::Other(e) => Error::Other(e),
    };
    // Being planned already: that plan.
    if let Some(pid) = target.planned_as.clone()
        && let Ok(p) = crate::plans::get(hub, &pid).await
        && p.phase == "planning"
    {
        let part = stack.items.iter().find(|i| i.part_of.as_deref() == Some(target.id.as_str()) && i.part_kind.as_deref() == Some("planning")).map(|i| i.id.clone()).unwrap_or_default();
        let research = crate::research::of_plan(hub, &pid).await.unwrap_or_default();
        return Ok(Planned { plan: pid, research, goal: target.id.clone(), part });
    }
    let words = target.text.clone();
    let title = title.map(str::trim).filter(|t| !t.is_empty()).map_or_else(|| words.chars().take(80).collect::<String>(), str::to_owned);
    let source = crate::plans::Source {
        session: Some(agent.to_owned()),
        goal: Some(crate::plans::GoalOf { agent: agent.to_owned(), id: target.id.clone(), text: words.clone() }),
        ..crate::plans::Source::default()
    };
    let new = crate::plans::New { title, body: format!("Goal: {words}"), items: Vec::new(), tags: vec!["from a goal".to_owned()], source: Some(source) };
    let p = crate::plans::create_in(hub, new, by, "planning").await.map_err(as_plan_error)?;
    let research = crate::research::start(hub, &words, Some(agent), by, Some(&p.id)).await.map_err(|e| Error::Bad(e.to_string()))?;
    let part_text = clean(&format!(
        "Plan \"{}\" (plan {}, research {}) is ready: researched inside and outside with each claim's sources (research_add), its steps written with their acceptance criteria, tests and the findings each rests on (plans_update), and said ready (plans_ready)",
        p.title, p.id, research.id
    ))?;
    let (target_id, pid) = (target.id.clone(), p.id.clone());
    {
        let (t, pid) = (target_id.clone(), pid.clone());
        hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET planned_as = ?2 WHERE id = ?1", params![t, pid]).map(|_| ())?)).await?;
    }
    let mut given = to_go(&stack.items);
    let k = given.iter().position(|(g, _)| g.as_deref() == Some(target_id.as_str())).unwrap_or(given.len());
    given.insert(k, (None, part_text));
    let before: Vec<String> = stack.items.iter().map(|i| i.id.clone()).collect();
    let s = set_locked(hub, &info, agent, given).await?;
    let new_ids = added(&s, &before);
    let t = target_id.clone();
    let ids = new_ids.clone();
    hub.store.call(move |c| note(c, &ids, Noted { part_of: Some(&t), kind: Some("planning"), step: None, waits_on: None })).await?;
    Ok(Planned { plan: pid, research: research.id, goal: target_id, part: new_ids.first().cloned().unwrap_or_default() })
}

/// What a goal does once the plan it became is ready ([`planned`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Then {
    /// Its steps become its parts, here.
    Goals,
    /// Its steps become its parts, and the stack pauses for a person to look at the plan first.
    Review,
    /// The plan runs with an agent per step (this run): the goal waits for it.
    Run(String),
}

/// Plan `plan` (with run `run`, kiln's record of it running here) made from goal `goal` of agent
/// `agent` is ready: the goal goes on as `then` says, its planning part met. Its steps (or the
/// part waiting for its run) go in before the goal, then the planning part ends, so what comes next
/// is its first step; once every part is met, so is the goal ([`met_by_parts`]).
pub async fn planned(hub: &Hub, agent: &str, goal: &str, plan: &crate::plans::Plan, run: &str, then: &Then, by: &By) -> R<Stack> {
    let stack = get(hub, agent).await?;
    if !stack.items.iter().any(|i| i.id == goal && matches!(i.status.as_str(), "pending" | "active")) {
        return Err(Error::Bad("the goal this plan was made from has ended, or gone: run the plan another way (plans_run)".into()));
    }
    let n = plan.steps.len();
    let goals: Vec<NewGoal> = match then {
        Then::Run(r) => vec![NewGoal {
            text: format!("Every step of the plan \"{}\" is met: its run {r}, an agent per step (kiln marks this met when the run is done)", plan.title),
            part_of: Some(goal.to_owned()),
            kind: Some("run"),
            waits_on: Some(r.clone()),
            step: None,
        }],
        Then::Goals | Then::Review => (0..n)
            .map(|i| NewGoal {
                text: crate::plans::agents::goal_of(plan, i, n),
                step: Some(StepRef { plan: plan.id.clone(), version: plan.version, n: i + 1, run: Some(run.to_owned()) }),
                part_of: Some(goal.to_owned()),
                kind: Some("plan"),
                waits_on: None,
            })
            .collect(),
    };
    add_at(hub, agent, goals, false, &At::Before(goal.to_owned())).await?;
    if *then == Then::Review {
        let why = format!("plan \"{}\" is ready for you to look at: its steps go to the agent once you resume", plan.title);
        control_as(hub, agent, Action::Pause, by, Pausing { why: Some(why), until: None, review: Some(plan.id.clone()) }).await?;
    }
    let stack = get(hub, agent).await?;
    let planning: Vec<String> = stack
        .items
        .iter()
        .filter(|i| i.part_of.as_deref() == Some(goal) && i.part_kind.as_deref() == Some("planning") && matches!(i.status.as_str(), "pending" | "active"))
        .map(|i| i.id.clone())
        .collect();
    for id in planning {
        done(hub, agent, Some(&id), Some("the plan is ready"), by).await?;
    }
    get(hub, agent).await
}

/// What a running stack does next.
#[derive(Debug)]
enum Next {
    Wait,
    /// The goal under way ended: met, or skipped (cleared in the agent). The next one follows.
    End { id: String, status: &'static str, reason: Option<String> },
    /// The agent is at work on the goal under way (since it was last sent).
    Busy { id: String },
    Start { id: String, text: String },
    /// Send the goal under way again: the agent stopped short of it. `worked`: it did real work
    /// on it since it was last sent; `stopped`: its harness isn't running (a message starts it);
    /// `fresh`: it hasn't been sent since you resumed it or gave it other words (it goes as it first
    /// went, not as a reminder).
    Remind { id: String, text: String, how: String, worked: bool, stopped: bool, fresh: bool },
    Done,
}

/// What a running stack does next, at `now`. `children`: agents it started are at work. `push`:
/// you just started or resumed the stack, skipped its goal or changed what's under way, so what's
/// next goes now, whatever the agent is doing.
fn decide(stack: &Stack, info: Option<&Info>, now: i64, children: bool, push: bool) -> Next {
    // An agent kiln doesn't know right now is waited for: its runner is reconnecting, or the
    // server has just started. (A closed agent's stack goes when it's closed.)
    let Some(info) = info else { return Next::Wait };
    let busy = matches!(info.status, Status::Running | Status::Waiting | Status::Starting);
    let active = stack.items.iter().find(|i| i.status == "active");
    // How the goal under way came out is taken as soon as it's known, whatever else the agent is at
    // (agents it started at work, a message queued): only the next goal waits for it.
    if let Some(end) = active.and_then(|item| ended(stack, item, info, busy)) {
        return end;
    }
    // Paused for a usage limit: nothing is sent (no goal, no reminder) until the pause ends; the
    // goal under way goes on then.
    if info.pause.is_some() {
        return Next::Wait;
    }
    // Yours goes now: behind a turn under way (its harness queues it), but not to an agent still
    // starting, whose goal command isn't known yet (`how`), nor to one whose machine is away (it
    // goes when it's back).
    let pushed = push && !matches!(info.status, Status::Starting | Status::Disconnected);
    // Nothing waiting on you or in its queue; stopped counts (a message starts it again).
    let idle = matches!(info.status, Status::Idle | Status::Exited) && info.approvals.is_empty() && info.queued == 0;
    // Not while agents it started are at work: it ended its turn to let them, and is told when each
    // settles (`harness::told`).
    let free = pushed || (idle && !children);
    if let Some(item) = active {
        let id = item.id.clone();
        if busy && !pushed {
            return if item.busy_at == 0 { Next::Busy { id } } else { Next::Wait };
        }
        // Not sent since you resumed it (or gave it other words), or under way from before these
        // rules: it goes as soon as the agent is idle, agents it started at work or not.
        let fresh = item.reminded_at == 0;
        if !(free || (fresh && idle)) {
            return Next::Wait;
        }
        // Claude Code judges its /goal when a turn ends with nothing of its own still at work in the
        // background (agents, shells): it's woken when those end. Sent again before then, the goal
        // only starts over and the judgement it would have made is lost.
        if !fresh && item.how.as_deref() == Some("goal") && info.tasks_running > 0 {
            return Next::Wait;
        }
        // Since it was last sent (never, for a fresh one: at once).
        if now.saturating_sub(item.reminded_at) < gap(item.reminders) {
            return Next::Wait;
        }
        let how = item.how.clone().unwrap_or_else(|| "message".into());
        let worked = item.busy_at > 0 && info.last_activity.saturating_sub(item.busy_at) >= WORKED_MS;
        return Next::Remind { id, text: item.text.clone(), how, worked, stopped: info.status == Status::Exited, fresh };
    }
    // A goal under way on another agent (delegated: docs/programs.md) isn't this one's to start,
    // and while one is open the stack isn't done.
    match stack.items.iter().find(|i| i.status == "pending" && i.on.is_none()) {
        Some(i) if free => Next::Start { id: i.id.clone(), text: i.text.clone() },
        Some(_) => Next::Wait,
        None if stack.items.iter().any(|i| i.status == "pending") => Next::Wait,
        None => Next::Done,
    }
}

/// How the goal under way came out, since it was last sent, if it did: its harness says it's met,
/// or that you cleared it there; or, sent as a message, the agent said it's met. Still set, given
/// up, paused, or none at all: it hasn't (it's reminded).
fn ended(stack: &Stack, item: &Item, info: &Info, busy: bool) -> Option<Next> {
    let id = item.id.clone();
    if !matches!(item.how.as_deref(), Some("goal" | "goal_control")) {
        // Its last words, once its turn is over.
        return (!busy && item.busy_at > 0 && said_met(&info.summary)).then_some(Next::End { id, status: "met", reason: None });
    }
    let o = info.objective.as_ref().filter(|o| same(&o.text, &item.text) && o.at > item.prior_at)?;
    // Cleared by kiln itself when you paused the stack isn't cleared by you.
    let own = stack.dropped_at > 0 && o.at >= stack.dropped_at && o.at - stack.dropped_at <= OWN_CLEAR_MS;
    match o.status.as_str() {
        "met" => Some(Next::End { id, status: "met", reason: o.reason.clone() }),
        // Not while it's at work: a clear kiln queued behind its turn may have the goal sent again
        // queued after it.
        "cleared" if !own && !busy => Some(Next::End { id, status: "skipped", reason: Some("The goal was cleared in the agent.".into()) }),
        _ => None,
    }
}

/// The agents whose stacks their program's usage paused (docs/programs.md).
/// The goals agents paused for a person, until someone resumes them (`Stack::held_for`), by agent.
pub async fn held_stacks(store: &crate::store::Store) -> HashMap<String, Held> {
    store
        .call(|c| {
            let mut st = c.prepare("SELECT agent, COALESCE(reason, ''), held_for, dropped_at FROM goal_stacks WHERE state = 'paused' AND held_for IS NOT NULL")?;
            let v = st
                .query_map([], |r| {
                    let plan: String = r.get(2)?;
                    Ok((r.get::<_, String>(0)?, Held { why: r.get(1)?, plan: Some(plan).filter(|p| !p.is_empty()), since: r.get(3)? }))
                })?
                .collect::<Result<HashMap<_, _>, _>>()?;
            Ok(v)
        })
        .await
        .unwrap_or_default()
}

async fn paused_for_usage(store: &crate::store::Store) -> Vec<String> {
    let like = format!("{}%", crate::programs::PAUSED_FOR_USAGE);
    let paused = store
        .call(move |c| {
            let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'paused' AND reason LIKE ?1")?;
            let v = st.query_map(params![like], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(v)
        })
        .await;
    paused.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), "goal stacks paused for usage: not read");
        Vec::new()
    })
}

/// Stacks their program's usage paused, running again once it's at 80% or under (between 80% and
/// 95% they stay paused, so a window near 95% doesn't pause and resume them by turns), as when you
/// resume one (`run_from_here`): the goal under way goes to the agent again at once, and what its
/// harness said of it before (the clear the pause sent) doesn't count. A stack you paused stays
/// paused: each goes on only if it's still paused for usage when it does, under the stacks' lock
/// (`CHANGING`), so a pause of yours made meanwhile holds.
async fn resume_lifted<P>(d: &Driver<'_, P>)
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let paused = paused_for_usage(d.store).await;
    if paused.is_empty() {
        return;
    }
    let all = d.port.agents();
    for agent in paused {
        if crate::programs::rule(d.store, &all, &agent).await != crate::programs::Rule::Ok {
            continue;
        }
        let _one = CHANGING.lock().await;
        let said_at = all.iter().find(|i| i.id == agent).and_then(|i| i.objective.as_ref()).map_or(0, |o| o.at);
        let now = d.clock.unix_ms();
        let (a, like) = (agent.clone(), format!("{}%", crate::programs::PAUSED_FOR_USAGE));
        let resumed = d
            .store
            .call(move |c| {
                let still: bool = c.query_row("SELECT state = 'paused' AND reason LIKE ?2 FROM goal_stacks WHERE agent = ?1", params![a, like], |r| r.get(0)).optional()?.unwrap_or(false);
                if !still {
                    return Ok(false);
                }
                let tx = c.unchecked_transaction()?;
                run_from_here(&tx, &a, said_at, now)?;
                let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![a], |r| r.get(0))?;
                tx.commit()?;
                Ok(state == "running")
            })
            .await;
        match resumed {
            Ok(true) => {
                tracing::info!(%agent, "goal stack resumed: its program's usage is at 80% or under again");
                if let Err(e) = step(d, &agent, true).await {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack paused for usage: not resumed"),
        }
    }
}

/// Puts goal `item` of `agent`'s stack (a program's coordinator's) under way on agent `on`, its
/// track's worker, or marks it met (`met`), or, with neither, back in the stack as one to come
/// (docs/programs.md, "Workers"). Another goal of the same track (`track`, its key's prefix:
/// `CP-T7.`) under way elsewhere comes back first: a track has one goal under way. False, with
/// nothing changed, when `item` isn't a goal of the stack still to come.
pub async fn delegate(store: &crate::store::Store, agent: &str, track: &str, item: Option<&str>, on: Option<&str>, met: bool, now: i64) -> anyhow::Result<bool> {
    let _one = CHANGING.lock().await;
    // A goal said met through it (programs.assign) ends, and counts as one said met does.
    let saying_met = met && item.is_some();
    let (agent, item, on) = (agent.to_owned(), item.map(str::to_owned), on.map(str::to_owned));
    let like = format!("{}%", track.replace(['%', '_'], ""));
    let done = store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("UPDATE goal_items SET delegated_to = NULL WHERE agent = ?1 AND delegated_to IS NOT NULL AND text LIKE ?2", params![agent, like])?;
            if let Some(item) = &item {
                let n = if met {
                    tx.execute("UPDATE goal_items SET status = 'met', ended_at = ?3, delegated_to = NULL WHERE agent = ?1 AND id = ?2 AND status IN ('pending', 'active')", params![agent, item, now])?
                } else if let Some(on) = &on {
                    tx.execute("UPDATE goal_items SET delegated_to = ?3 WHERE agent = ?1 AND id = ?2 AND status = 'pending'", params![agent, item, on])?
                } else {
                    1
                };
                if n == 0 {
                    return Ok(false);
                }
            }
            touch(&tx, &agent, None, None, now)?;
            tx.commit()?;
            Ok(true)
        })
        .await?;
    if done && saying_met {
        crate::metrics::agents().goal_ended(true);
    }
    Ok(done)
}

/// One step of a running stack (`decide`), by the driver's clock and through its port; `push`:
/// yours, which goes now.
async fn step<P>(d: &Driver<'_, P>, agent: &str, push: bool) -> anyhow::Result<()>
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let (store, port) = (d.store, d.port);
    let mut stack = stack_of(store, agent).await?;
    // Paused or done since it was found running: left as it is.
    if stack.state != "running" {
        return Ok(());
    }
    // A part waiting for a plan run: met once the run is done; until then nothing goes to the
    // agent (the run's agents do the work, and it's told as each settles).
    if let Some(run) = stack.items.iter().find(|i| i.status == "active").and_then(|i| i.waits_on.clone().map(|r| (i.id.clone(), r))) {
        let (id, run) = run;
        let r = run.clone();
        let state: Option<String> = store.call(move |c| Ok(c.query_row("SELECT state FROM plan_runs WHERE id = ?1", params![r], |x| x.get(0)).optional()?)).await?;
        if state.as_deref() != Some("done") {
            return Ok(());
        }
        let (a, now) = (agent.to_owned(), d.clock.unix_ms());
        store
            .call(move |c| {
                c.execute("UPDATE goal_items SET status = 'met', reason = 'Its plan run is done: every step met.', ended_at = ?2 WHERE id = ?1", params![id, now])?;
                touch(c, &a, None, None, now)
            })
            .await?;
        d.metrics.goal_ended(true);
        stack = stack_of(store, agent).await?;
    }
    // A goal whose parts kiln made are all met: met itself, at once, with no check of its own.
    while let Some((id, why)) = met_by_parts(&stack) {
        let (a, now) = (agent.to_owned(), d.clock.unix_ms());
        store
            .call(move |c| {
                c.execute("UPDATE goal_items SET status = 'met', reason = ?2, ended_at = ?3 WHERE id = ?1", params![id, why, now])?;
                touch(c, &a, None, None, now)
            })
            .await?;
        d.metrics.goal_ended(true);
        stack = stack_of(store, agent).await?;
    }
    let all = port.agents();
    let children = crate::harness::told::children_working(&all, agent);
    // Its program's usage rule (docs/programs.md; the critical path's master plan, §5): above 95%
    // of a weekly window every stack of the program pauses, above 80% no new goal starts.
    let rule = crate::programs::rule(store, &all, agent).await;
    let info = all.into_iter().find(|i| i.id == agent);
    let now = d.clock.unix_ms();
    let a = agent.to_owned();
    if let crate::programs::Rule::Pause { why } = &rule {
        let why = why.clone();
        // As a pause of yours does: the clear kiln sends now is its own (`dropped_at`), not the
        // agent's, so it doesn't end the goal.
        store
            .call(move |c| {
                touch(c, &a, Some("paused"), Some(&why), now)?;
                c.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![a, now])?;
                Ok(())
            })
            .await?;
        tracing::info!(%agent, "goal stack paused for usage");
        // Its own goal (Claude Code's /goal) cleared too, so the agent doesn't go on pursuing it, and
        // what still waits in its queue for its goals withdrawn.
        if let Some(i) = info.as_ref() {
            if stack.items.iter().any(|g| g.status == "active") {
                clear_in_agent(port, i, &stack.items, now).await;
            }
            withdraw(port, agent, &stack.items, |g| matches!(g.status.as_str(), "active" | "pending")).await;
        }
        return Ok(());
    }
    let next = decide(&stack, info.as_ref(), now, children, push);
    // What its harness last said of the goal it pursues: a verdict on what's sent now must be newer.
    let prior_at = info.as_ref().and_then(|i| i.objective.as_ref()).map_or(0, |o| o.at);
    match next {
        Next::Wait => {}
        Next::Busy { id } => {
            store.call(move |c| Ok(c.execute("UPDATE goal_items SET busy_at = ?2 WHERE id = ?1", params![id, now]).map(|_| ())?)).await?;
        }
        Next::End { id, status, reason } => {
            store
                .call(move |c| {
                    c.execute("UPDATE goal_items SET status = ?2, reason = ?3, ended_at = ?4 WHERE id = ?1", params![id, status, reason, now])?;
                    touch(c, &a, None, None, now)
                })
                .await?;
            d.metrics.goal_ended(status == "met");
        }
        Next::Done => {
            store.call(move |c| touch(c, &a, Some("done"), None, now)).await?;
        }
        Next::Start { id, .. } if stack.items.iter().any(|i| i.id == id && i.waits_on.is_some()) => {
            store
                .call(move |c| {
                    c.execute("UPDATE goal_items SET status = 'active', how = 'run', started_at = ?2, reminded_at = ?2, reminders = 0, busy_at = 0 WHERE id = ?1", params![id, now])?;
                    touch(c, &a, None, None, now)
                })
                .await?;
        }
        // Held for usage: the next goal waits until its program is under 80% again.
        Next::Start { .. } if rule.holds_new_goals() => {}
        Next::Start { id, text } => {
            let how = how(port, agent);
            let stopped = info.as_ref().is_some_and(|i| i.status == Status::Exited);
            let c = first_command(how, &text, stopped);
            let (went, sent) = say(port, agent, Some(id.as_str()), c, now).await;
            match went {
                Sent::Delivered => {
                    d.metrics.goal_sent(false);
                    store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET status = 'active', how = ?2, prior_at = ?3, started_at = ?4, reminded_at = ?4, reminders = 0, busy_at = 0, sent_id = ?5 WHERE id = ?1",
                                params![id, how, prior_at, now, sent],
                            )?;
                            touch(c, &a, None, None, now)
                        })
                        .await?;
                }
                // Not known right now, or its machine is away (or its inbox full): the goal goes
                // when it's back.
                Sent::NoSession | Sent::Unreachable | Sent::Full => {}
            }
        }
        Next::Remind { id, text, how, worked, stopped, fresh } => {
            let c = if fresh { first_command(&how, &text, stopped) } else { reminder_command(&how, &text, stopped) };
            let (went, sent) = say(port, agent, Some(id.as_str()), c, now).await;
            match went {
                Sent::Delivered => {
                    // Afresh (resumed, or reworded) it goes as a goal goes first: a start.
                    d.metrics.goal_sent(!fresh);
                    tracing::info!(%agent, worked, stopped, fresh, "goal sent again");
                    store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET reminded_at = ?2, reminders = CASE WHEN ?5 THEN 0 WHEN ?3 THEN 1 ELSE reminders + 1 END, prior_at = ?4, busy_at = 0, sent_id = COALESCE(?6, sent_id) WHERE id = ?1",
                                params![id, now, worked, prior_at, fresh, sent],
                            )?;
                            touch(c, &a, None, None, now)
                        })
                        .await?;
                }
                // Not known right now, or its machine is away (or its inbox full): tried again after
                // a pause, as a reminder that brought no work.
                Sent::NoSession | Sent::Unreachable | Sent::Full => {
                    store.call(move |c| Ok(c.execute("UPDATE goal_items SET reminded_at = ?2, reminders = reminders + 1 WHERE id = ?1", params![id, now]).map(|_| ())?)).await?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Objective, ToolKind};
    use crate::harness::session::ApprovalBrief;

    fn info(status: Status, objective: Option<Objective>) -> Info {
        Info { id: "a1".into(), status, objective, ..Info::default() }
    }

    fn item(status: &str, how: Option<&str>) -> Item {
        Item { id: "g1".into(), text: "tests pass".into(), status: status.into(), how: how.map(str::to_owned), started_at: Some(1_000), reminded_at: 1_000, prior_at: 5, ..Item::default() }
    }

    fn stack(items: Vec<Item>) -> Stack {
        Stack { agent: "a1".into(), state: "running".into(), items, ..Stack::default() }
    }

    fn obj(status: &str, at: i64) -> Objective {
        Objective { text: "tests  pass".into(), status: status.into(), reason: Some("why".into()), at }
    }

    /// Past the first pause after the goal was sent.
    const LATER: i64 = 1_000 + REMIND_MS;

    /// The id of a message a command sends, if it sends one.
    fn sent_id(c: &Command) -> Option<String> {
        match c {
            Command::Send { id, .. } => Some(id.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_stack_holds_a_program_of_more_than_a_hundred_goals_and_stays_bounded() {
        assert!(fits(1, 120), "a goal under way and 120 more");
        assert!(fits(100, 300), "a program's met goals stay, with room for more");
        assert!(fits(0, ITEMS) && !fits(1, ITEMS) && !fits(ITEMS, 1), "at most {ITEMS}");
        assert!(!fits(usize::MAX, 1), "no overflow");
    }

    #[test]
    fn an_agent_paused_for_a_limit_gets_no_goal_and_no_reminder() {
        let pause = crate::harness::model::Pause { reason: "limit".into(), since: 1, auto: true, by: "harness".into(), ..Default::default() };
        let paused = Info { pause: Some(pause), ..info(Status::Idle, None) };
        assert!(matches!(decide(&stack(vec![item("pending", None)]), Some(&paused), 2_000, false, false), Next::Wait));
        assert!(matches!(decide(&stack(vec![item("active", None)]), Some(&paused), LATER * 10, false, false), Next::Wait));
        assert!(matches!(decide(&stack(vec![item("active", Some("goal"))]), Some(&paused), LATER * 10, false, false), Next::Wait));
        // Once it goes on, so does the stack.
        assert!(matches!(decide(&stack(vec![item("pending", None)]), Some(&info(Status::Idle, None)), 2_000, false, false), Next::Start { .. }));
    }

    #[test]
    fn a_claude_code_goal_is_not_sent_again_while_its_agent_has_background_work() {
        let native = stack(vec![item("active", Some("goal"))]);
        let busy_behind = Info { tasks_running: 2, ..info(Status::Idle, None) };
        // Its turn ended with agents or shells of its own still at work: Claude Code hasn't
        // judged the goal yet, and sending it again would start it over.
        assert!(matches!(decide(&native, Some(&busy_behind), LATER * 100, false, false), Next::Wait));
        // Their end wakes it; once a turn ends with none left, a goal still unmet goes again.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, None)), LATER, false, false), Next::Remind { .. }));
        // Its verdict is taken whatever is still at work.
        assert!(matches!(decide(&native, Some(&Info { tasks_running: 2, ..info(Status::Idle, Some(obj("met", 9))) }), LATER, false, false), Next::End { status: "met", .. }));
        // Other harnesses' goals aren't held: their goal doesn't start over when sent again.
        assert!(matches!(decide(&stack(vec![item("active", Some("goal_control"))]), Some(&busy_behind), LATER, false, false), Next::Remind { .. }));
        assert!(matches!(decide(&stack(vec![item("active", None)]), Some(&busy_behind), LATER, false, false), Next::Remind { .. }));
        // One you resumed (not sent since) still goes at once.
        let resumed = Item { reminded_at: 0, ..item("active", Some("goal")) };
        assert!(matches!(decide(&stack(vec![resumed]), Some(&busy_behind), LATER, false, false), Next::Remind { fresh: true, .. }));
    }

    #[test]
    fn goals_start_when_the_agent_is_free_and_end_when_its_harness_says_met() {
        let pending = stack(vec![item("pending", None)]);
        assert!(matches!(decide(&pending, Some(&info(Status::Idle, None)), 2_000, false, false), Next::Start { .. }));
        assert!(matches!(decide(&pending, Some(&info(Status::Running, None)), 2_000, false, false), Next::Wait));
        // A stopped agent starts again for its next goal.
        assert!(matches!(decide(&pending, Some(&info(Status::Exited, None)), 2_000, false, false), Next::Start { .. }));
        let native = stack(vec![item("active", Some("goal"))]);
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 9)))), 2_000, false, false), Next::End { status: "met", .. }));
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("cleared", 9)))), 2_000, false, false), Next::End { status: "skipped", .. }));
        // The verdict on the goal before it doesn't count.
        assert!(!matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 5)))), LATER, false, false), Next::End { .. }));
        assert!(matches!(decide(&stack(vec![item("met", Some("goal"))]), Some(&info(Status::Idle, None)), 2_000, false, false), Next::Done));
        // Not known right now (its runner reconnecting, the server just started): waited for.
        assert!(matches!(decide(&native, None, LATER, false, false), Next::Wait));
    }

    #[test]
    fn an_agent_that_stops_short_of_its_goal_is_reminded_never_left() {
        let native = stack(vec![item("active", Some("goal"))]);
        // Gave up, paused it, stopped with it still set, never took it up, or lost it when it
        // started again: once the pause since it was sent is over, it's sent again.
        for o in [Some(obj("unmet", 9)), Some(obj("paused", 9)), Some(obj("active", 9)), None] {
            assert!(matches!(decide(&native, Some(&info(Status::Idle, o.clone())), 2_000, false, false), Next::Wait), "{o:?}");
            assert!(matches!(decide(&native, Some(&info(Status::Idle, o.clone())), LATER, false, false), Next::Remind { stopped: false, .. }), "{o:?}");
        }
        // Stopped: sent again all the same (the message starts it).
        assert!(matches!(decide(&native, Some(&info(Status::Exited, None)), LATER, false, false), Next::Remind { stopped: true, .. }));
        // Agents it started at work: it ended its turn to let them, and isn't reminded until they settle.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, None)), LATER, true, false), Next::Wait));
        // At work on it: noted once, then left to it.
        assert!(matches!(decide(&native, Some(&info(Status::Running, None)), LATER, false, false), Next::Busy { .. }));
        let mut working = item("active", Some("goal"));
        working.busy_at = 2_000;
        assert!(matches!(decide(&stack(vec![working]), Some(&info(Status::Running, None)), LATER, false, false), Next::Wait));
        // Waiting on you, or with something in its queue: not reminded.
        let mut asking = info(Status::Idle, None);
        asking.approvals = vec![ApprovalBrief { id: "p1".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "cargo test".into(), at: 1 }];
        assert!(matches!(decide(&native, Some(&asking), LATER, false, false), Next::Wait));
        let mut queued = info(Status::Idle, None);
        queued.queued = 1;
        assert!(matches!(decide(&native, Some(&queued), LATER, false, false), Next::Wait));
    }

    /// CP-M3 (docs/programs.md, "Usage"): keep-going looks at an agent no stack drives, but not one
    /// paused for usage (its session at a limit or threshold, or its stack for its program's usage:
    /// the driver's `quiet`), nor one that paused its stack for a while.
    #[test]
    fn keep_going_leaves_an_agent_paused_for_usage_alone() {
        let at_limit = Info { id: "limit".into(), pause: Some(crate::harness::model::Pause { reason: "limit".into(), ..Default::default() }), ..Info::default() };
        let all: Vec<Info> = ["driven", "waits", "held", "free"].iter().map(|id| Info { id: (*id).into(), ..Info::default() }).chain([at_limit]).collect();
        let quiet: Vec<String> = all.iter().filter(|i| i.pause.is_some() || i.id == "held").map(|i| i.id.clone()).collect();
        assert_eq!(quiet, ["held", "limit"]);
        let looked_at: Vec<&str> = keep_going(&all, &["driven".to_owned()], &["waits".to_owned()], &quiet).into_iter().map(|i| i.id.as_str()).collect();
        assert_eq!(looked_at, ["free"]);
    }

    /// CP-M3: a goal under way on another agent (a program's coordinator's, handed to its track's
    /// worker) isn't started here; the next is; while it's open, the stack isn't done; nor is it the
    /// goal next to start that its parts met (`met_by_parts`).
    #[test]
    fn a_goal_under_way_on_another_agent_isn_t_started_and_keeps_the_stack_open() {
        let idle = info(Status::Idle, None);
        let handed = Item { id: "g1".into(), text: "CP-T7.1 Hex set up".into(), status: "pending".into(), on: Some("hex-t7".into()), ..Item::default() };
        let next = Item { id: "g2".into(), text: "CP-T3.1 Sends deduplicated".into(), status: "pending".into(), ..Item::default() };
        assert!(matches!(decide(&stack(vec![handed.clone(), next]), Some(&idle), 2_000, false, false), Next::Start { id, .. } if id == "g2"));
        assert!(matches!(decide(&stack(vec![handed.clone()]), Some(&idle), 2_000, false, false), Next::Wait), "open on Hex T7: not done");
        let met = Item { status: "met".into(), on: None, ..handed.clone() };
        assert!(matches!(decide(&stack(vec![met]), Some(&idle), 2_000, false, false), Next::Done));
        // Nor is it the goal next to start that its parts met: one after it, whose plan's steps are
        // all met, is (met by its parts, as goal-to-plan has it).
        let planned = Item { id: "g3".into(), text: "CP-M5 A planned goal".into(), status: "pending".into(), ..Item::default() };
        let step = Item { id: "g4".into(), text: "its step".into(), status: "met".into(), part_of: Some("g3".into()), part_kind: Some("plan".into()), ..Item::default() };
        assert_eq!(met_by_parts(&stack(vec![handed, planned, step])).map(|(id, _)| id), Some("g3".to_owned()));
    }

    #[test]
    fn the_clear_a_pause_sends_is_kiln_s_own_and_not_the_end_of_the_goal() {
        let mut paused_then_resumed = stack(vec![item("active", Some("goal"))]);
        paused_then_resumed.dropped_at = 7;
        // Its harness says the goal was cleared, just after kiln cleared it to pause: sent again.
        assert!(matches!(decide(&paused_then_resumed, Some(&info(Status::Idle, Some(obj("cleared", 9)))), LATER, false, false), Next::Remind { .. }));
        // Cleared in the agent long after: by you, so the goal ends.
        assert!(matches!(decide(&paused_then_resumed, Some(&info(Status::Idle, Some(obj("cleared", 7 + OWN_CLEAR_MS + 1)))), LATER, false, false), Next::End { status: "skipped", .. }));
    }

    #[test]
    fn a_goal_resumed_or_from_before_reminders_goes_to_a_free_agent_at_once() {
        let mut resumed = item("active", Some("goal"));
        resumed.reminded_at = 0;
        // Any time now (unix ms): it has never been sent by these rules.
        assert!(matches!(decide(&stack(vec![resumed]), Some(&info(Status::Idle, None)), 1_792_000_000_000, false, false), Next::Remind { .. }));
    }

    #[test]
    fn reminders_wait_longer_while_they_bring_no_work_and_start_over_after_real_work() {
        assert_eq!(gap(0), REMIND_MS);
        assert_eq!(gap(1), 2 * REMIND_MS);
        assert_eq!(gap(3), 8 * REMIND_MS);
        assert_eq!(gap(40), REMIND_MAX_MS);
        let mut thrice = item("active", Some("goal"));
        thrice.reminders = 3;
        thrice.reminded_at = 10_000;
        let s = stack(vec![thrice]);
        assert!(matches!(decide(&s, Some(&info(Status::Idle, None)), 10_000 + 8 * REMIND_MS - 1, false, false), Next::Wait));
        assert!(matches!(decide(&s, Some(&info(Status::Idle, None)), 10_000 + 8 * REMIND_MS, false, false), Next::Remind { worked: false, .. }));
        // It worked on it a good while since it was last sent: real work.
        let mut worked = item("active", Some("goal"));
        worked.reminded_at = 10_000;
        worked.busy_at = 11_000;
        let mut after = info(Status::Idle, None);
        after.last_activity = 11_000 + WORKED_MS;
        assert!(matches!(decide(&stack(vec![worked]), Some(&after), 10_000 + REMIND_MS, false, false), Next::Remind { worked: true, .. }));
    }

    /// CP-M2's counters move (a non-vacuity check; docs/agents.md, "Metrics"): a goal's first send
    /// counts as a start, a send again as a reminder, and a goal its agent's harness says is met, or
    /// that's cleared in the agent, as ended, met or skipped. Each exactly once, into the driver's
    /// own counts, so nothing else running moves them.
    #[test]
    fn each_goal_counter_moves_once_for_each_send_and_end_the_driver_makes() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "goals-metrics";
            assert!(port.open(id, Info { id: id.into(), status: Status::Idle, ..Info::default() }));
            assert!(port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]})));
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            store
                .call(move |c| {
                    for (pos, (g, text)) in [("g1", "the first goal"), ("g2", "the second goal")].into_iter().enumerate() {
                        c.execute("INSERT INTO goal_items (id, agent, pos, text) VALUES (?1, ?2, ?3, ?4)", params![g, id, i64::try_from(pos)?, text])?;
                    }
                    touch(c, id, Some("running"), None, 0)
                })
                .await?;
            let counts = || (metrics.goals_sent.get(&["start"]), metrics.goals_sent.get(&["reminder"]), metrics.goals_ended.get(&["met"]), metrics.goals_ended.get(&["skipped"]));
            let says = |status: &str, text: &str| {
                let objective = Objective { text: text.into(), status: status.into(), reason: None, at: clock.unix_ms() };
                port.update(id, |i| i.objective = Some(objective))
            };
            // The first goal goes to the agent, idle: a start.
            step(&driver, id, false).await?;
            assert_eq!(counts(), (1, 0, 0, 0));
            // It stops short of it: 30 s on, the goal goes again.
            clock.advance(std::time::Duration::from_secs(30));
            step(&driver, id, false).await?;
            assert_eq!(counts(), (1, 1, 0, 0));
            // Its harness says it's met: it ended, met; the second one starts.
            clock.advance(std::time::Duration::from_secs(1));
            assert!(says("met", "the first goal"));
            step(&driver, id, false).await?;
            assert_eq!(counts(), (1, 1, 1, 0));
            step(&driver, id, false).await?;
            assert_eq!(counts(), (2, 1, 1, 0));
            // Cleared in the agent: it ended, skipped.
            clock.advance(std::time::Duration::from_secs(1));
            assert!(says("cleared", "the second goal"));
            step(&driver, id, false).await?;
            assert_eq!(counts(), (2, 1, 1, 1));
            // What was counted is what the agent was sent: two goals and a reminder.
            assert_eq!(port.heard(id).len(), 3);
            Ok(())
        })
    }

    /// A goal skipped through goal control counts as ended, skipped, once; a skip with no goal
    /// under way counts nothing; a goal said met counts as ended, met; one taken out while under way
    /// as ended, skipped (CP-M2's counters, a non-vacuity check).
    #[test]
    fn a_goal_skipped_said_met_or_taken_out_counts_as_ended_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|c| c.conversations.off = true).await?;
            let dir = tempfile::tempdir()?;
            let spec = crate::harness::session::Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Default::default()
            };
            let opened = hub.harness.open(&hub, "local", spec, 1).await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
            let id = opened.into_iter().next().map(|i| i.id).ok_or_else(|| anyhow::anyhow!("nothing opened"))?;
            set(&hub, &id, vec![Given { id: None, text: "a goal to skip".into() }]).await?;
            let a = id.clone();
            hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'active' WHERE agent = ?1", params![a]).map(|_| ())?)).await?;
            let metrics = crate::metrics::AgentMetrics::default();
            control_counted(&hub, &id, Action::Skip, &By::You, Pausing::default(), &metrics).await?;
            assert_eq!(metrics.goals_ended.get(&["skipped"]), 1);
            control_counted(&hub, &id, Action::Skip, &By::You, Pausing::default(), &metrics).await?;
            assert_eq!((metrics.goals_ended.get(&["skipped"]), metrics.goals_ended.get(&["met"])), (1, 0), "nothing under way: nothing ended");
            // Said met by the agent: ended, met. Taken out while under way: ended, skipped.
            let under_way = |text: &'static str| {
                let (hub, a) = (Arc::clone(&hub), id.clone());
                async move { hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'active' WHERE agent = ?1 AND text = ?2", params![a, text]).map(|_| ())?)).await }
            };
            add(&hub, &id, vec!["a goal said met".into(), "a goal taken out".into()], false).await?;
            under_way("a goal said met").await?;
            done_counted(&hub, &id, None, Some("its tests pass"), &By::Itself, &metrics).await?;
            assert_eq!(metrics.goals_ended.get(&["met"]), 1);
            under_way("a goal taken out").await?;
            let out = get(&hub, &id).await?.items.into_iter().find(|i| i.text == "a goal taken out").map(|i| i.id).unwrap_or_default();
            update_counted(&hub, &id, &out, Change { remove: true, ..Change::default() }, &By::Itself, &metrics).await?;
            assert_eq!((metrics.goals_ended.get(&["met"]), metrics.goals_ended.get(&["skipped"])), (1, 2));
            Ok(())
        })
    }

    /// CP-M2's test, T-M2.1 (and AC-M2.1: the goal driver on the simulated clock in tests): the
    /// driver's reminder back-off, under the simulated clock. An
    /// agent stops short of its goal again and again: it's sent the goal 30 s after the goal joined
    /// its stack, then each time twice the gap before, up to 15 minutes, each at exactly its time and
    /// none in between (a tick short of each, nothing has gone).
    #[test]
    fn reminders_go_30_s_after_then_doubling_to_15_min_by_the_driver_s_clock() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            // Its migrations on the blocking pool, not on a runtime thread the tests beside it share.
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let (id, goal) = ("goals-backoff", "the parser takes every input");
            // Its harness has goals of its own (Claude Code's /goal); it's idle, its goal still set.
            let objective = Objective { text: goal.into(), status: "active".into(), reason: None, at: clock.unix_ms() };
            assert!(port.open(id, Info { id: id.into(), status: Status::Idle, objective: Some(objective), ..Info::default() }));
            port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}));
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            let sent = || port.heard(id).iter().map(|d| d.at_ms).collect::<Vec<i64>>();
            let test = async {
                clock.sleeping_since(0, 1).await;
                // A tick: the goal set in the agent joins its stack, as the goal under way.
                clock.advance_and_settle(TICK).await;
                let joined = stack_of(&store, id).await?;
                anyhow::ensure!(joined.items.first().is_some_and(|i| i.status == "active" && i.how.as_deref() == Some("goal")), "the goal didn't join the stack: {joined:?}");
                let mut last = clock.unix_ms();
                let mut gaps = Vec::new();
                for secs in [30, 60, 120, 240, 480, 900, 900] {
                    clock.advance_and_settle(Duration::from_secs(secs).saturating_sub(TICK)).await;
                    anyhow::ensure!(sent().len() == gaps.len(), "a reminder went a tick before {secs} s: {:?}", sent());
                    clock.advance_and_settle(TICK).await;
                    let at = sent().last().copied().unwrap_or_default();
                    anyhow::ensure!(sent().len() == gaps.len() + 1 && at == clock.unix_ms(), "no reminder at {secs} s: {:?}", sent());
                    gaps.push(at - last);
                    last = at;
                }
                // The driver stops when it's shut down.
                shutdown.cancel();
                Ok(gaps)
            };
            let (driven, gaps) = futures::join!(driver.drive(), test);
            driven?;
            assert_eq!(gaps?, [30_000, 60_000, 120_000, 240_000, 480_000, 900_000, 900_000]);
            // Each was the goal again, as its harness takes it.
            let again = format!("/goal {goal}");
            assert!(port.heard(id).iter().all(|d| matches!(&d.command, Command::Send { text, .. } if *text == again)), "{:?}", port.heard(id));
            Ok(())
        })
    }

    /// What you start, resume or skip goes at once (`push`), as the driver's step does it: through
    /// its port, by its clock, behind a turn under way (but not again while the message that took
    /// it still waits there); not to an agent whose machine is away (it goes once it's back); and a
    /// send refused (its inbox full, its machine unreachable) is tried again later, as a reminder
    /// that brought no work.
    #[test]
    fn what_you_push_goes_through_the_driver_s_port_at_once_and_a_refused_one_is_tried_again() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        /// The goal under way: when it was last sent, and the reminders in a row.
        async fn sent(store: &crate::store::Store, id: &str) -> anyhow::Result<Option<(i64, i64)>> {
            Ok(stack_of(store, id).await?.items.first().map(|i| (i.reminded_at, i.reminders)))
        }
        /// You resumed it: not sent since.
        async fn resumed(store: &crate::store::Store) -> anyhow::Result<()> {
            store.call(|c| Ok(c.execute("UPDATE goal_items SET reminded_at = 0", []).map(|_| ())?)).await
        }
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            // Room for one command: past it, a send is refused (`Full`).
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone())).with_inbox(1);
            let id = "goals-push";
            assert!(port.open(id, Info { id: id.into(), status: Status::Disconnected, ..Info::default() }));
            assert!(port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]})));
            let metrics = crate::metrics::AgentMetrics::default();
            let d = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            // A running stack, its goal under way just resumed.
            let (a, now) = (id.to_owned(), clock.unix_ms());
            store
                .call(move |c| {
                    touch(c, &a, Some("running"), None, now)?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status, how) VALUES ('g1', ?1, 0, 'tests pass', 'active', 'goal')", params![a])?;
                    Ok(())
                })
                .await?;
            // Its machine away: nothing goes, and the goal waits as it was.
            step(&d, id, true).await?;
            assert!(port.heard(id).is_empty());
            assert_eq!(sent(&store, id).await?, Some((0, 0)));
            // Back, and at work: it goes now, as it first went, by the driver's clock.
            assert!(port.update(id, |i| i.status = Status::Running));
            clock.advance(Duration::from_secs(5));
            step(&d, id, true).await?;
            let heard = port.heard(id);
            assert!(matches!(heard.as_slice(), [first] if first.at_ms == clock.unix_ms() && matches!(&first.command, Command::Send { text, .. } if text == "/goal tests pass")), "{heard:?}");
            assert_eq!(sent(&store, id).await?, Some((clock.unix_ms(), 0)));
            // Resumed at once, the goal still waiting in its queue: that one says it already.
            resumed(&store).await?;
            clock.advance(Duration::from_secs(5));
            step(&d, id, true).await?;
            assert_eq!((port.heard(id).len(), sent(&store, id).await?), (1, Some((clock.unix_ms(), 0))));
            // Resumed again once it took the goal, its inbox full: nothing delivered, and it's tried
            // again later.
            resumed(&store).await?;
            clock.advance(Duration::from_millis(u64::try_from(crate::harness::queued::SHOW_MS).unwrap_or(0)));
            step(&d, id, true).await?;
            assert_eq!((port.heard(id).len(), sent(&store, id).await?), (1, Some((clock.unix_ms(), 1))));
            // So too when its machine can't be reached.
            resumed(&store).await?;
            assert!(port.away(id, true));
            clock.advance(Duration::from_secs(5));
            step(&d, id, true).await?;
            assert_eq!((port.heard(id).len(), sent(&store, id).await?), (1, Some((clock.unix_ms(), 2))));
            Ok(())
        })
    }

    /// What waits in agent `id`'s queue: each command it was sent, applied in order behind its one
    /// long turn, but for the messages it took (`taken`); shown on the agent as its machine would
    /// report it.
    fn queue_of(port: &kiln_seams::harness::SimHarness<Info, Command>, id: &str, taken: &[String]) -> Vec<(String, String)> {
        let mut q: Vec<(String, String)> = Vec::new();
        for d in port.heard(id) {
            match d.command {
                Command::Send { id, text, .. } => q.push((id, text)),
                Command::QueueEdit { id, text } => {
                    if let Some(m) = q.iter_mut().find(|m| m.0 == id) {
                        m.1 = text;
                    }
                }
                Command::QueueRemove { id } => q.retain(|m| m.0 != id),
                _ => {}
            }
        }
        q.retain(|m| !taken.contains(&m.0));
        port.update(id, |i| {
            i.queue = q.iter().map(|(m, t)| crate::harness::queued::Queued::of(m, t)).collect();
            i.queued = q.len();
        });
        q
    }

    /// The incident's goals: an agent in one long turn, its stack resumed again and again, a goal
    /// said met and the next sent, then that one met before the agent took it, its own goal cleared
    /// each time. One message of the stack's waits in its queue at a time, edited, never a second
    /// behind it; and what an edit said that came too late (the agent took the message as it was)
    /// is said again.
    #[test]
    fn a_goal_stack_s_word_waits_in_its_agent_s_queue_one_message_at_a_time() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "goals-one-word";
            // At work on one long turn, its harness's own goal the stack's first, met there already.
            let lint = Objective { text: "lint passes".into(), status: "active".into(), reason: None, at: clock.unix_ms() };
            assert!(port.open(id, Info { id: id.into(), status: Status::Running, objective: Some(lint), ..Info::default() }));
            port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}));
            let metrics = crate::metrics::AgentMetrics::default();
            let d = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            let a = id.to_owned();
            store
                .call(move |c| {
                    touch(c, &a, Some("running"), None, 0)?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status, how, reminded_at) VALUES ('g0', ?1, 0, 'lint passes', 'active', 'goal', 1)", params![a])?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status) VALUES ('g1', ?1, 1, 'tests pass', 'pending'), ('g2', ?1, 2, 'docs build', 'pending')", params![a])?;
                    Ok(())
                })
                .await?;
            let st = &store;
            let items = || async move { anyhow::Ok(stack_of(st, id).await?.items) };
            let info = || port.agent(id).unwrap_or_default();
            let met = |goal: &'static str| async move {
                st.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'met' WHERE id = ?1", params![goal]).map(|_| ())?)).await?;
                renumber_in(st, id).await
            };
            // g0 is met (as `done` does it): its goal cleared in the agent, behind its turn; then the next goes.
            clear_in_agent(&port, &info(), &items().await?, clock.unix_ms()).await;
            withdraw(&port, id, &items().await?, |i| i.id == "g0").await;
            met("g0").await?;
            queue_of(&port, id, &[]);
            step(&d, id, true).await?;
            let q = queue_of(&port, id, &[]);
            anyhow::ensure!(matches!(q.as_slice(), [(_, t)] if t == "/goal tests pass"), "the clear waiting became the next goal: {q:?}");
            // Resumed again and again: the goal waiting already says it, so nothing more goes.
            for _ in 0..3 {
                store.call(|c| Ok(c.execute("UPDATE goal_items SET reminded_at = 0 WHERE status = 'active'", []).map(|_| ())?)).await?;
                clock.advance(Duration::from_secs(60));
                step(&d, id, true).await?;
            }
            let heard: Vec<Command> = port.heard(id).into_iter().map(|d| d.command).collect();
            anyhow::ensure!(matches!(heard.as_slice(), [Command::Send { .. }, Command::QueueEdit { .. }]), "one message, edited once: {heard:?}");
            // g1 met before the agent took it: the agent's own goal is cleared, the next goes, and
            // each is the one message, edited.
            clear_in_agent(&port, &info(), &items().await?, clock.unix_ms()).await;
            withdraw(&port, id, &items().await?, |i| i.id == "g1").await;
            met("g1").await?;
            step(&d, id, true).await?;
            let q = queue_of(&port, id, &[]);
            anyhow::ensure!(matches!(q.as_slice(), [(_, t)] if t == "/goal docs build"), "{q:?}");
            let sends = port.heard(id).iter().filter(|d| matches!(d.command, Command::Send { .. })).count();
            anyhow::ensure!(sends == 1, "a single message queued all along: {:?}", port.heard(id));
            let waiting = q.first().map(|(m, _)| m.clone()).unwrap_or_default();
            // Its goal reworded, and the edit comes too late: the turn ended, the agent took the
            // message as it was. What the edit said goes again, in a message of its own.
            let _ = say(&port, id, Some("g2"), send("/goal docs build on every platform".into()), clock.unix_ms()).await;
            queue_of(&port, id, std::slice::from_ref(&waiting));
            resay(&port, clock.unix_ms()).await;
            let q = queue_of(&port, id, std::slice::from_ref(&waiting));
            anyhow::ensure!(matches!(q.as_slice(), [(m, t)] if *m != waiting && t == "/goal docs build on every platform"), "{q:?}");
            // Once that's seen waiting and then taken, nothing more is said.
            resay(&port, clock.unix_ms()).await;
            let taken: Vec<String> = q.iter().map(|(m, _)| m.clone()).chain(std::iter::once(waiting)).collect();
            queue_of(&port, id, &taken);
            let before = port.heard(id).len();
            resay(&port, clock.unix_ms()).await;
            anyhow::ensure!(port.heard(id).len() == before, "{:?}", port.heard(id));
            Ok(())
        })
    }

    /// A word a server before this one left waiting in the agent's queue (its id says it's a goal
    /// stack's) is the one waiting: the same goal isn't sent again, another goes as an edit of it.
    /// One said again that doesn't reach the agent (its machine away) is kept, and said once it's
    /// back.
    #[test]
    fn a_word_a_server_before_left_waiting_is_the_one_waiting_and_one_said_again_is_kept_till_it_goes() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "goals-word-from-before";
            let old = "goal-01a16251c49a75fba41b1fae971ba8a9";
            let waiting = vec![crate::harness::queued::Queued::of(old, "/goal tests pass")];
            assert!(port.open(id, Info { id: id.into(), status: Status::Running, queue: waiting, queued: 1, ..Info::default() }));
            let (went, by) = say(&port, id, Some("g1"), send("/goal tests pass".into()), clock.unix_ms()).await;
            anyhow::ensure!(went == Sent::Delivered && by.as_deref() == Some(old) && port.heard(id).is_empty(), "the same word, left as it is: {:?}", port.heard(id));
            let (_, by) = say(&port, id, Some("g2"), send("/goal docs build".into()), clock.unix_ms()).await;
            let heard = port.heard(id);
            anyhow::ensure!(by.as_deref() == Some(old) && matches!(heard.as_slice(), [d] if matches!(&d.command, Command::QueueEdit { id: e, text } if e == old && text == "/goal docs build")), "{heard:?}");
            // The agent took it before the edit came, and its machine is away when it's said again.
            port.update(id, |i| {
                i.queue.clear();
                i.queued = 0;
            });
            assert!(port.away(id, true));
            resay(&port, clock.unix_ms()).await;
            anyhow::ensure!(port.heard(id).len() == 1, "nothing reached it: {:?}", port.heard(id));
            assert!(port.away(id, false));
            resay(&port, clock.unix_ms()).await;
            let heard = port.heard(id);
            anyhow::ensure!(heard.len() == 2 && matches!(heard.last().map(|d| &d.command), Some(Command::Send { id, text, .. }) if id.starts_with(GOAL) && text == "/goal docs build"), "said once it's back: {heard:?}");
            Ok(())
        })
    }

    /// Puts the goals done first, as `done` leaves them.
    async fn renumber_in(store: &crate::store::Store, agent: &str) -> anyhow::Result<()> {
        let a = agent.to_owned();
        store.call(move |c| renumber(c, &a)).await
    }

    /// A stack paused for a while resumes by itself when its time is up, by the driver's clock: not
    /// a tick before, and its goal under way goes to the agent at once. One resumed or paused again
    /// since (its time gone) is left as it is.
    #[test]
    fn a_stack_paused_for_a_while_resumes_by_itself_when_its_time_is_up() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "goals-paused-a-while";
            assert!(port.open(id, Info { id: id.into(), status: Status::Idle, ..Info::default() }));
            port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}));
            let (a, until) = (id.to_owned(), clock.unix_ms() + 60_000);
            store
                .call(move |c| {
                    touch(c, &a, Some("paused"), Some("Paused by the agent: waiting for CI"), 0)?;
                    c.execute("UPDATE goal_stacks SET resume_at = ?2 WHERE agent = ?1", params![a, until])?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status, how, reminded_at) VALUES ('g1', ?1, 0, 'deploy is green', 'active', 'goal', 5)", params![a])?;
                    Ok(())
                })
                .await?;
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            let test = async {
                clock.sleeping_since(0, 1).await;
                clock.advance_and_settle(Duration::from_secs(60).saturating_sub(TICK)).await;
                let s = stack_of(&store, id).await?;
                anyhow::ensure!(s.state == "paused" && port.heard(id).is_empty(), "resumed a tick early: {s:?}");
                clock.advance_and_settle(TICK).await;
                let s = stack_of(&store, id).await?;
                anyhow::ensure!(s.state == "running" && s.resume_at == 0 && s.reason.is_none(), "{s:?}");
                let heard = port.heard(id);
                anyhow::ensure!(matches!(heard.as_slice(), [d] if matches!(&d.command, Command::Send { text, .. } if text == "/goal deploy is green")), "{heard:?}");
                // The message that took it is noted, to withdraw if the goal ends before the agent takes it.
                let sent = heard.first().and_then(|d| sent_id(&d.command));
                anyhow::ensure!(sent.is_some() && s.items.first().is_some_and(|i| i.sent_id == sent), "{:?} {sent:?}", s.items.first().map(|i| &i.sent_id));
                shutdown.cancel();
                Ok(())
            };
            let (driven, tested) = futures::join!(driver.drive(), test);
            driven?;
            tested
        })
    }

    /// A goal the stack has ended (said met, skipped) that the agent's harness still had set, and
    /// gave up on after: it doesn't join the stack again, as a goal set in the agent would. Set in
    /// the agent anew, it does.
    #[test]
    fn a_goal_the_stack_ended_doesn_t_come_back_when_its_harness_gives_up_on_it() -> anyhow::Result<()> {
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "goals-ended";
            let later = clock.unix_ms() + 10_000;
            // Said met at 0; its harness gives up on it 10 s later (its checks never agreed).
            let gave_up = Objective { text: "tests pass".into(), status: "unmet".into(), reason: None, at: later };
            assert!(port.open(id, Info { id: id.into(), status: Status::Idle, objective: Some(gave_up), ..Info::default() }));
            port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}));
            let a = id.to_owned();
            store
                .call(move |c| {
                    touch(c, &a, Some("running"), None, 0)?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status, how) VALUES ('g1', ?1, 0, 'tests pass', 'met', 'goal')", params![a])?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status) VALUES ('g2', ?1, 1, 'docs build', 'pending')", params![a])?;
                    Ok(())
                })
                .await?;
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            let test = async {
                clock.sleeping_since(0, 1).await;
                clock.advance_and_settle(TICK).await;
                let s = stack_of(&store, id).await?;
                let w: Vec<String> = s.items.iter().map(|i| format!("{}:{}", i.status, i.text)).collect();
                anyhow::ensure!(w == ["met:tests pass", "active:docs build"], "{w:?}");
                // Set in the agent anew (`/goal tests pass` again): it joins, as the goal under way.
                port.update(id, |i| i.objective = Some(Objective { text: "tests pass".into(), status: "active".into(), reason: None, at: later + 20_000 }));
                clock.advance_and_settle(TICK).await;
                let s = stack_of(&store, id).await?;
                anyhow::ensure!(s.items.iter().any(|i| i.status == "active" && i.text == "tests pass"), "{s:?}");
                // Its harness giving up on the goal that went back among the others (split, moved): it stays where it is.
                port.update(id, |i| i.objective = Some(Objective { text: "docs build".into(), status: "unmet".into(), reason: None, at: later + 40_000 }));
                clock.advance_and_settle(TICK).await;
                let s = stack_of(&store, id).await?;
                let w: Vec<String> = s.items.iter().filter(|i| i.status != "met").map(|i| format!("{}:{}", i.status, i.text)).collect();
                anyhow::ensure!(w.first().map(String::as_str) == Some("active:tests pass") && w.contains(&"pending:docs build".to_owned()), "{w:?}");
                shutdown.cancel();
                Ok(())
            };
            let (driven, tested) = futures::join!(driver.drive(), test);
            driven?;
            tested
        })
    }

    #[test]
    fn a_goal_sent_as_a_message_is_met_when_the_agent_says_so() {
        let mut said = item("active", Some("message"));
        said.busy_at = 1_500;
        let mut done = info(Status::Idle, None);
        done.summary = "All 212 tests pass on every platform.\n\n**GOAL MET**".into();
        assert!(matches!(decide(&stack(vec![said.clone()]), Some(&done), 2_000, false, false), Next::End { status: "met", .. }));
        // Its turn ended without saying so: reminded.
        let mut not_yet = info(Status::Idle, None);
        not_yet.summary = "Fixed the parser; the lexer is next.".into();
        assert!(matches!(decide(&stack(vec![said]), Some(&not_yet), LATER, false, false), Next::Remind { .. }));
        // Words from before it was last sent don't count.
        assert!(!matches!(decide(&stack(vec![item("active", Some("message"))]), Some(&done), LATER, false, false), Next::End { .. }));
        assert!(said_met("done.\nGoal met."));
        assert!(!said_met("GOAL MET is what I'll say when it's done"));
        assert!(!said_met(""));
    }

    #[test]
    fn goals_are_one_line_and_go_as_their_harness_takes_them() {
        assert_eq!(clean("  ship\n the   lexer ").ok().as_deref(), Some("ship the lexer"));
        assert!(clean("   ").is_err());
        assert!(clean(&"x".repeat(TEXT + 1)).is_err());
        assert!(same("tests  pass", "tests pass") && !same("tests pass", "tests passed"));
        assert!(matches!(goal_command("goal", "tests pass"), Command::Send { text, .. } if text == "/goal tests pass"));
        assert!(matches!(goal_command("goal_control", "ship"), Command::Control { id, value: Some(v) } if id == "goal" && v.get("text") == Some(&json!("ship"))));
        assert!(matches!(goal_command("message", "ship"), Command::Send { text, .. } if text.starts_with("Your goal: ship") && text.ends_with(MET_LINE)));
        assert!(matches!(reminder_command("goal", "ship", true), Command::Send { text, .. } if text == "/goal ship"));
        assert!(matches!(reminder_command("goal_control", "ship", false), Command::Control { .. }));
        assert!(matches!(reminder_command("goal_control", "ship", true), Command::Send { text, .. } if text.contains("isn't met yet: ship")));
        assert!(matches!(reminder_command("message", "ship", false), Command::Send { text, .. } if text.contains("isn't met yet: ship") && text.ends_with(MET_LINE)));
    }
    fn goal(id: &str, text: &str, status: &str) -> Item {
        let sent = status != "pending";
        Item {
            id: id.into(),
            text: text.into(),
            status: status.into(),
            how: sent.then(|| "goal".to_owned()),
            started_at: sent.then_some(1_000),
            reminded_at: if status == "active" { 1_000 } else { 0 },
            ..Item::default()
        }
    }

    /// The screenshot: you paused the stack (its goal cleared in the agent), then resumed it while
    /// agents it started were at work. The goal under way goes again at once all the same, where it
    /// used to wait for them, shown under way the while.
    #[test]
    fn a_goal_you_resume_goes_at_once_though_agents_it_started_are_at_work() {
        let mut resumed = item("active", Some("goal"));
        resumed.reminded_at = 0;
        // Resuming took the clear for what was said before.
        resumed.prior_at = 9;
        let cleared = info(Status::Idle, Some(obj("cleared", 9)));
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&cleared), LATER, true, false), Next::Remind { fresh: true, .. }));
        // A reminder (it was sent since) still waits for them.
        assert!(matches!(decide(&stack(vec![item("active", Some("goal"))]), Some(&info(Status::Idle, None)), LATER, true, false), Next::Wait));
        // At work: noted, and sent once its turn is over.
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&info(Status::Running, None)), LATER, true, false), Next::Busy { .. }));
        // Yours goes now, behind the turn under way; not to an agent still starting, nor at a usage limit.
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&info(Status::Running, None)), LATER, true, true), Next::Remind { fresh: true, .. }));
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&info(Status::Starting, None)), LATER, true, true), Next::Busy { .. }));
        // Its machine away: it goes once it's back, as soon as it's idle.
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&info(Status::Disconnected, None)), LATER, true, true), Next::Wait));
        assert!(matches!(decide(&stack(vec![resumed.clone()]), Some(&info(Status::Idle, None)), LATER, true, false), Next::Remind { fresh: true, .. }));
        let pause = crate::harness::model::Pause { reason: "limit".into(), since: 1, auto: true, by: "harness".into(), ..Default::default() };
        assert!(matches!(decide(&stack(vec![resumed]), Some(&Info { pause: Some(pause), ..info(Status::Idle, None) }), LATER, false, true), Next::Wait));
        // Sent again as it first went (not as a reminder).
        assert!(matches!(first_command("message", "ship", false), Command::Send { text, .. } if text.starts_with("Your goal: ship")));
        assert!(matches!(first_command("goal_control", "ship", true), Command::Send { text, .. } if text.starts_with("Your goal: ship")));
        assert!(matches!(first_command("goal", "ship", true), Command::Send { text, .. } if text == "/goal ship"));
    }

    #[test]
    fn the_next_goal_waits_for_agents_it_started_but_yours_goes_now() {
        let pending = stack(vec![goal("g2", "docs build", "pending")]);
        assert!(matches!(decide(&pending, Some(&info(Status::Idle, None)), 2_000, true, false), Next::Wait));
        assert!(matches!(decide(&pending, Some(&info(Status::Idle, None)), 2_000, true, true), Next::Start { .. }));
        // Behind a turn under way: its harness queues it. An agent still starting waits.
        assert!(matches!(decide(&pending, Some(&info(Status::Running, None)), 2_000, false, true), Next::Start { .. }));
        assert!(matches!(decide(&pending, Some(&info(Status::Starting, None)), 2_000, false, true), Next::Wait));
    }

    #[test]
    fn how_a_goal_came_out_is_taken_whatever_the_agent_is_doing() {
        let native = stack(vec![item("active", Some("goal"))]);
        // Met while agents it started are at work, or while it's at something else.
        for (status, children) in [(Status::Idle, true), (Status::Running, false), (Status::Waiting, true)] {
            assert!(matches!(decide(&native, Some(&info(status, Some(obj("met", 9)))), 2_000, children, false), Next::End { status: "met", .. }), "{status:?}");
        }
        // Cleared there by you while agents it started are at work: skipped, not shown under way.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("cleared", 9)))), 2_000, true, false), Next::End { status: "skipped", .. }));
        // Not while it's at work: a clear kiln queued behind its turn may have the goal queued after it.
        assert!(matches!(decide(&native, Some(&info(Status::Running, Some(obj("cleared", 9)))), 2_000, false, false), Next::Busy { .. }));
        // Said met, as a message, its turn over, agents it started at work.
        let mut said = item("active", Some("message"));
        said.busy_at = 1_500;
        let mut done = info(Status::Idle, None);
        done.summary = "All green.\n\nGOAL MET".into();
        assert!(matches!(decide(&stack(vec![said]), Some(&done), 2_000, true, false), Next::End { status: "met", .. }));
    }

    /// An edit's rows, as words: "=id" as it is, "~id:text" the goal under way reworded, "id:text"
    /// or "+text" to come, "<id:text" the goal under way put back; and the goals to come that go.
    fn edit(items: &[Item], given: &[(Option<&str>, &str)]) -> (Vec<String>, Vec<String>) {
        let (rows, gone) = arrange(items, given.iter().map(|(id, t)| (id.map(str::to_owned), (*t).to_owned())).collect());
        let words = rows
            .iter()
            .map(|r| match r {
                Row::Kept(id) => format!("={id}"),
                Row::Reworded { id, text } => format!("~{id}:{text}"),
                Row::Next { id: Some(id), text } => format!("{id}:{text}"),
                Row::Next { id: None, text } => format!("+{text}"),
                Row::Back { id, text } => format!("<{id}:{text}"),
            })
            .collect();
        (words, gone)
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_edit_reorders_the_goals_to_go_the_one_under_way_too_and_never_repeats_one() {
        let items = [goal("m", "lint passes", "met"), goal("s", "fly", "skipped"), goal("a", "tests pass", "active"), goal("b", "docs build", "pending"), goal("c", "ship it", "pending")];
        // The goals to come, reordered, one with other words: the goal under way stays, before them.
        assert_eq!(edit(&items, &[(Some("c"), "ship it now"), (Some("b"), "docs build")]), (strs(&["=m", "=s", "=a", "c:ship it now", "b:docs build"]), vec![]));
        // The goal under way after another: put back among them, and that one goes next.
        assert_eq!(edit(&items, &[(Some("b"), "docs build"), (Some("a"), "tests pass"), (Some("c"), "ship it")]), (strs(&["=m", "=s", "b:docs build", "<a:tests pass", "c:ship it"]), vec![]));
        // First, as it was (however it's spaced): as it is. With other words: reworded.
        assert_eq!(edit(&items, &[(Some("a"), "tests  pass"), (Some("b"), "docs build"), (Some("c"), "ship it")]), (strs(&["=m", "=s", "=a", "b:docs build", "c:ship it"]), vec![]));
        assert_eq!(edit(&items, &[(Some("a"), "tests pass on CI"), (Some("b"), "docs build")]), (strs(&["=m", "=s", "~a:tests pass on CI", "b:docs build"]), strs(&["c"])));
        // Goals done, given by id, are left where they are: not added again. A goal said twice, or
        // one unknown (gone since), is a new one.
        assert_eq!(edit(&items, &[(Some("m"), "lint passes"), (Some("s"), "fly"), (Some("b"), "docs build")]), (strs(&["=m", "=s", "=a", "b:docs build"]), strs(&["c"])));
        assert_eq!(edit(&items, &[(Some("b"), "docs build"), (Some("b"), "docs build"), (Some("x"), "new one")]), (strs(&["=m", "=s", "=a", "b:docs build", "+docs build", "+new one"]), strs(&["c"])));
        // Given by their words alone (an agent's list without ids): the goals they are, reordered,
        // the goal under way too; words of a goal done make a new one; a goal named isn't taken
        // by words as well.
        assert_eq!(edit(&items, &[(None, "ship it"), (None, "tests  pass"), (None, "docs build")]), (strs(&["=m", "=s", "c:ship it", "<a:tests  pass", "b:docs build"]), vec![]));
        assert_eq!(edit(&items, &[(None, "lint passes")]), (strs(&["=m", "=s", "=a", "+lint passes"]), strs(&["b", "c"])));
        assert_eq!(edit(&items, &[(None, "docs build"), (Some("b"), "docs build")]), (strs(&["=m", "=s", "=a", "+docs build", "b:docs build"]), strs(&["c"])));
        // An edit begun before the goal under way was met and the next started: nothing repeats.
        let since = [goal("a", "tests pass", "met"), goal("b", "docs build", "active"), goal("c", "ship it", "pending")];
        assert_eq!(edit(&since, &[(Some("a"), "tests pass"), (Some("b"), "docs build"), (Some("c"), "ship it")]), (strs(&["=a", "=b", "c:ship it"]), vec![]));
    }

    #[test]
    fn cleared_goals_come_back_as_they_were_after_the_goals_done_since() {
        let back = vec![goal("m", "lint passes", "met"), goal("a", "tests pass", "active"), goal("b", "docs build", "pending")];
        let order = |v: &[Item]| v.iter().map(|i| format!("{}:{}", i.id, i.status)).collect::<Vec<_>>();
        // Into an empty stack: as they were; the goal under way sent again at once, and what was
        // said of it before doesn't count.
        let got = merged(vec![], back.clone(), 77);
        assert_eq!(order(&got), strs(&["m:met", "a:active", "b:pending"]));
        assert!(got.iter().any(|i| i.id == "a" && i.reminded_at == 0 && i.prior_at == 77 && i.how.as_deref() == Some("goal")));
        // Goals of its own since: its own done after theirs; its goal under way stays so, theirs comes next.
        let now = vec![goal("n", "fix the build", "met"), goal("o", "bump deps", "active"), goal("p", "tag it", "pending")];
        let got = merged(now, back.clone(), 77);
        assert_eq!(order(&got), strs(&["m:met", "n:met", "o:active", "a:pending", "b:pending", "p:pending"]));
        assert!(got.iter().any(|i| i.id == "a" && i.how.is_none() && i.started_at.is_none()));
        // Only goals to come of its own: theirs first, under way again.
        assert_eq!(order(&merged(vec![goal("p", "tag it", "pending")], back, 77)), strs(&["m:met", "a:active", "b:pending", "p:pending"]));
    }
}

/// The stack against a store, on a hub of its own with one agent (no harness runs: its start fails,
/// and it stands stopped, which a message starts again), under a timeout.
#[cfg(all(test, unix))]
mod stack_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::harness::lineage::{Ask, Starter, start};

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

    /// Each goal as "status:text", in order.
    fn words(s: &Stack) -> Vec<String> {
        s.items.iter().map(|i| format!("{}:{}", i.status, i.text)).collect()
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// The goal under way, and whether it went to the agent since it went under way or was resumed.
    fn under_way(s: &Stack) -> Option<(String, bool)> {
        s.items.iter().find(|i| i.status == "active").map(|i| (i.text.clone(), i.reminded_at > 0))
    }

    /// Until the agent no longer stands starting (its start fails: no harness here).
    async fn settled(hub: &Hub, id: &str) {
        for _ in 0..200 {
            if hub.harness.info(id).is_some_and(|i| i.status != Status::Starting) {
                return;
            }
            crate::rt::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn a_cleared_stack_comes_back_and_runs_and_edits_never_lose_or_repeat_a_goal() -> anyhow::Result<()> {
        within(async {
            // No conversation store: what's changed is goals, not conversations.
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None };
            let ask = Ask {
                kind: "claude".into(),
                on: Some("local".into()),
                cwd: Some(folder.path().to_string_lossy().into_owned()),
                mode: Some("default".into()),
                name: Some("A".into()),
                count: Some(1),
                ..Ask::default()
            };
            let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent started"))?;
            let id = a.id.as_str();
            settled(&hub, id).await;

            // Lined up, not started.
            let s = add(&hub, id, vec!["tests pass".into(), "docs build".into(), "ship it".into()], false).await?;
            assert_eq!((s.state.as_str(), words(&s)), ("paused", strs(&["pending:tests pass", "pending:docs build", "pending:ship it"])));
            // Started: the first goes to the agent at once.
            let s = control(&hub, id, Action::Start).await?;
            assert_eq!((s.state.as_str(), under_way(&s)), ("running", Some(("tests pass".into(), true))), "{s:?}");
            let ids: Vec<String> = s.items.iter().map(|i| i.id.clone()).collect();
            let given = |order: &[(usize, &str)]| order.iter().map(|(k, t)| Given { id: ids.get(*k).cloned(), text: (*t).to_owned() }).collect::<Vec<_>>();

            // Another put before the goal under way: that one goes now, and it waits next.
            settled(&hub, id).await;
            let s = set(&hub, id, given(&[(2, "ship it"), (0, "tests pass"), (1, "docs build")])).await?;
            assert_eq!(words(&s), strs(&["active:ship it", "pending:tests pass", "pending:docs build"]));
            assert_eq!(under_way(&s), Some(("ship it".into(), true)));
            // An edit of the goals to come only: the goal under way stays.
            let s = set(&hub, id, given(&[(1, "docs build"), (0, "tests pass")])).await?;
            assert_eq!(words(&s), strs(&["active:ship it", "pending:docs build", "pending:tests pass"]));
            // The goal under way with other words: sent again as they say.
            settled(&hub, id).await;
            let s = set(&hub, id, given(&[(2, "ship it today"), (1, "docs build"), (0, "tests pass")])).await?;
            assert_eq!(under_way(&s), Some(("ship it today".into(), true)));
            assert_eq!(s.items.len(), 3, "nothing repeated: {s:?}");

            // Paused, then resumed: the goal under way goes again at once.
            let s = control(&hub, id, Action::Pause).await?;
            assert_eq!(s.state, "paused");
            settled(&hub, id).await;
            let s = control(&hub, id, Action::Start).await?;
            assert_eq!((s.state.as_str(), under_way(&s)), ("running", Some(("ship it today".into(), true))));
            // Skipped: the next goes at once.
            settled(&hub, id).await;
            let s = control(&hub, id, Action::Skip).await?;
            assert_eq!(words(&s), strs(&["skipped:ship it today", "active:docs build", "pending:tests pass"]));
            assert_eq!(under_way(&s), Some(("docs build".into(), true)));

            // Cleared: kept, to bring back, through a goal added since.
            let s = control(&hub, id, Action::Clear).await?;
            assert_eq!((s.state.as_str(), s.items.len(), s.cleared), ("paused", 0, 3));
            let s = add(&hub, id, vec!["hotfix".into()], false).await?;
            assert_eq!((words(&s), s.cleared), (strs(&["pending:hotfix"]), 3));
            settled(&hub, id).await;
            let s = control(&hub, id, Action::Restore).await?;
            assert_eq!(words(&s), strs(&["skipped:ship it today", "active:docs build", "pending:tests pass", "pending:hotfix"]));
            assert_eq!((s.state.as_str(), s.cleared, under_way(&s)), ("running", 0, Some(("docs build".into(), true))));
            assert!(matches!(control(&hub, id, Action::Restore).await, Err(Error::Bad(_))), "nothing more to bring back");

            // A stack that's done, given another goal: no longer done.
            let ids: Vec<String> = s.items.iter().map(|i| i.id.clone()).collect();
            hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'met' WHERE id IN (?1, ?2, ?3)", params![ids.get(1), ids.get(2), ids.get(3)]).map(|_| ())?)).await?;
            let s = control(&hub, id, Action::Start).await?;
            assert_eq!(s.state, "done");
            let s = add(&hub, id, vec!["one more".into()], false).await?;
            assert_eq!((s.state.as_str(), words(&s).last().cloned()), ("paused", Some("pending:one more".into())));
            // A goal added with the words of the goal under way is a new one: that one stays under way.
            settled(&hub, id).await;
            let s = control(&hub, id, Action::Start).await?;
            assert_eq!(under_way(&s), Some(("one more".into(), true)));
            let s = add(&hub, id, vec!["one  more".into()], false).await?;
            assert_eq!(words(&s).get(s.items.len().saturating_sub(2)..).map(<[String]>::to_vec), Some(strs(&["active:one more", "pending:one more"])));
            Ok(())
        })
    }

    /// CP-M3's usage rules, held by the driver (docs/programs.md; the master plan's §5): an agent of
    /// a program (a track's worker, as assigned) whose weekly window is over 80% isn't started on a
    /// new goal; over 95% its stack pauses for usage, and stays paused down to 80%; at 80% or under
    /// it goes on, its goal under way sent again at once, the clear the pause sent not taken for the
    /// agent's (even one its harness reported long after). A stack you paused stays paused, whatever
    /// the usage. An agent that isn't the program's isn't held.
    #[test]
    fn a_program_s_usage_holds_new_goals_above_80_and_pauses_its_stacks_above_95() -> anyhow::Result<()> {
        use crate::harness::model::Limit;
        use kiln_seams::Clock as _;
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let weekly = |pct: f64| vec![Limit { id: "seven_day".into(), label: "Weekly".into(), used_percent: Some(pct), window: Some("7d".into()), ..Limit::default() }];
            assert!(port.open("coord", Info { id: "coord".into(), status: Status::Running, ..Info::default() }));
            let id = "hex-t7";
            assert!(port.open(id, Info { id: id.into(), name: "Hex T7".into(), status: Status::Idle, limits: weekly(85.0), ..Info::default() }));
            assert!(port.open("loose", Info { id: "loose".into(), status: Status::Idle, parent: Some("coord".into()), limits: weekly(85.0), ..Info::default() }));
            assert!(port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]})));
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            store
                .call(|c| {
                    c.execute("INSERT INTO plans (id, title, body, tags, items, created_at, updated_at) VALUES ('m', 'A program', '', '[\"lab\",\"master\"]', '[]', 1, 1)", [])?;
                    c.execute("INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES ('r', 'm', 1, 'coord', 'goals', 1, 'admin')", [])?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text) VALUES ('g1', 'hex-t7', 0, 'LAB-T7.1 Hex set up')", [])?;
                    touch(c, "hex-t7", Some("running"), None, 0)
                })
                .await?;
            // Hex T7 is T7's worker by its assignment (not the coordinator's child); the
            // coordinator's child that's no track's worker isn't the program's.
            store.kv_set("programs/m/assignments".into(), json!({"T7": {"agent": "hex-t7", "at": 1, "by": "admin"}}).to_string()).await?;
            let all = port.agents();
            assert!(matches!(crate::programs::rule(&store, &all, id).await, crate::programs::Rule::Hold { .. }));
            assert_eq!(crate::programs::rule(&store, &all, "loose").await, crate::programs::Rule::Ok);
            let state = || async { stack_of(&store, id).await.map(|s| (s.state, s.reason)) };
            let goal = || async { stack_of(&store, id).await.map(|s| s.items.first().map(|i| i.status.clone())) };
            // Over 80%: the goal doesn't start; under it, it does.
            step(&driver, id, false).await?;
            assert!(port.heard(id).is_empty(), "held: no new goal starts over 80%");
            assert_eq!(state().await?.0, "running");
            let sends = || port.heard(id).iter().filter(|h| matches!(&h.command, Command::Send { text, .. } if text.contains("LAB-T7.1"))).count();
            assert!(port.update(id, |i| i.limits = weekly(40.0)));
            step(&driver, id, false).await?;
            assert_eq!((sends(), goal().await?), (1, Some("active".to_owned())), "its goal started");
            // Over 95%: the stack pauses, for usage, the clear it sends kiln's own (as a pause of
            // yours), and stays paused between 80% and 95%.
            clock.advance(Duration::from_secs(5));
            assert!(port.update(id, |i| i.limits = weekly(96.0)));
            let paused_at = clock.unix_ms();
            step(&driver, id, false).await?;
            let (s, why) = state().await?;
            assert!(s == "paused" && why.as_deref().is_some_and(|r| r.starts_with(crate::programs::PAUSED_FOR_USAGE)), "{s} {why:?}");
            assert_eq!(stack_of(&store, id).await?.dropped_at, paused_at);
            assert!(port.heard(id).iter().any(|h| matches!(h.command, Command::QueueRemove { .. })), "what waited for its goal withdrawn: {:?}", port.heard(id));
            // Its harness reports the goal cleared (as Claude Code does /goal clear), here behind a
            // turn that ran on past the pause, so long after it.
            clock.advance(Duration::from_secs(90));
            let cleared = crate::harness::model::Objective { text: "LAB-T7.1 Hex set up".into(), status: "cleared".into(), reason: None, at: clock.unix_ms() };
            assert!(port.update(id, |i| i.objective = Some(cleared.clone())));
            assert!(port.update(id, |i| i.limits = weekly(90.0)));
            resume_lifted(&driver).await;
            assert_eq!(state().await?.0, "paused", "between 80% and 95% it stays paused");
            // At 80% or under: it goes on, its goal still under way and sent to the agent again at
            // once; the clear doesn't end it.
            clock.advance(Duration::from_secs(60));
            assert!(port.update(id, |i| i.limits = weekly(40.0)));
            resume_lifted(&driver).await;
            assert_eq!(state().await?, ("running".to_owned(), None));
            assert_eq!((goal().await?, sends()), (Some("active".to_owned()), 2), "its goal goes on, sent again");
            step(&driver, id, false).await?;
            assert_eq!(goal().await?, Some("active".to_owned()), "not ended as skipped by the pause's clear");
            // Paused by you: never resumed for usage.
            store.call(|c| touch(c, "hex-t7", Some("paused"), Some(PAUSED_BY_YOU), 1)).await?;
            resume_lifted(&driver).await;
            assert_eq!(state().await?.0, "paused");
            Ok(())
        })
    }

    /// CP-M2's counters with goal-to-plan's endings: a goal met with its plan (its kiln-made parts
    /// all met) counts as ended, met, once (a non-vacuity check).
    #[test]
    fn a_goal_met_with_its_plan_counts_as_ended_met() -> anyhow::Result<()> {
        use kiln_seams::clock::SimClock;
        use kiln_seams::harness::SimHarness;
        crate::rt::block_on(async {
            let clock = SimClock::new();
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let id = "planned-a";
            assert!(port.open(id, Info { id: id.into(), status: Status::Idle, ..Info::default() }));
            let metrics = crate::metrics::AgentMetrics::default();
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None, metrics: &metrics };
            store
                .call(|c| {
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status) VALUES ('g1', 'planned-a', 0, 'the planned goal', 'pending')", [])?;
                    c.execute("INSERT INTO goal_items (id, agent, pos, text, status, part_of, part_kind) VALUES ('p1', 'planned-a', 1, 'its step', 'met', 'g1', 'plan')", [])?;
                    touch(c, "planned-a", Some("running"), None, 0)
                })
                .await?;
            step(&driver, id, false).await?;
            let met = stack_of(&store, id).await?.items.into_iter().find(|i| i.id == "g1").map(|i| i.status);
            assert_eq!((met.as_deref(), metrics.goals_ended.get(&["met"])), (Some("met"), 1), "met with its plan, counted once");
            Ok(())
        })
    }

    /// CP-M3: a coordinator's goal handed to its track's worker (`delegate`): under way there, one
    /// a track (another of its goals comes back first; another track's, `CP-T70.`, stays), then
    /// met, or back in the stack; only a goal still to come is handed over.
    #[test]
    fn a_goal_is_handed_to_a_track_s_worker_one_a_track_and_comes_back() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let store = crate::rt::blocking(crate::store::Store::in_memory).await??;
            store
                .call(|c| {
                    for (pos, id, text) in [(0, "g1", "CP-T7.1 Hex set up"), (1, "g2", "CP-T7.2 Hex's scheduler"), (2, "g3", "CP-T70.1 Another track"), (3, "g4", "CP-T3.1 Sends")] {
                        c.execute("INSERT INTO goal_items (id, agent, pos, text) VALUES (?1, 'coord', ?2, ?3)", params![id, pos, text])?;
                    }
                    touch(c, "coord", Some("running"), None, 0)
                })
                .await?;
            let on = || async {
                let s = stack_of(&store, "coord").await?;
                anyhow::Ok(s.items.into_iter().map(|i| format!("{}:{}:{}", i.id, i.status, i.on.unwrap_or_default())).collect::<Vec<_>>())
            };
            assert!(delegate(&store, "coord", "CP-T70.", Some("g3"), Some("other"), false, 1).await?);
            assert!(delegate(&store, "coord", "CP-T7.", Some("g1"), Some("hex"), false, 2).await?);
            assert_eq!(on().await?, ["g1:pending:hex", "g2:pending:", "g3:pending:other", "g4:pending:"]);
            // Another of T7's: the first comes back; T70's stays where it is.
            assert!(delegate(&store, "coord", "CP-T7.", Some("g2"), Some("hex"), false, 3).await?);
            assert_eq!(on().await?, ["g1:pending:", "g2:pending:hex", "g3:pending:other", "g4:pending:"]);
            // Met; then none under way for T70 (it comes back).
            assert!(delegate(&store, "coord", "CP-T7.", Some("g2"), None, true, 4).await?);
            assert!(delegate(&store, "coord", "CP-T70.", None, None, false, 5).await?);
            assert_eq!(on().await?, ["g1:pending:", "g2:met:", "g3:pending:", "g4:pending:"]);
            // A goal no longer to come isn't handed over, and nothing changes.
            assert!(!delegate(&store, "coord", "CP-T7.", Some("g2"), Some("hex"), false, 6).await?);
            assert_eq!(on().await?, ["g1:pending:", "g2:met:", "g3:pending:", "g4:pending:"]);
            Ok(())
        })
    }

    /// An agent works its own stack: says its goal is met (with what shows it) and the next goes
    /// at once; says a goal to come is met already; changes one goal by its id (other words,
    /// moved next, before the goal under way, taken out); adds goals next; splits its goal into
    /// parts pursued first (said met, the goal takes its parts with it); and pauses the stack for a
    /// while, saying why.
    #[test]
    fn an_agent_says_its_goal_is_met_changes_one_goal_splits_one_and_pauses_for_a_while() -> anyhow::Result<()> {
        within(async {
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
            let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None };
            let ask = Ask {
                kind: "claude".into(),
                on: Some("local".into()),
                cwd: Some(folder.path().to_string_lossy().into_owned()),
                mode: Some("default".into()),
                name: Some("A".into()),
                count: Some(1),
                ..Ask::default()
            };
            let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent started"))?;
            let id = a.id.as_str();
            settled(&hub, id).await;
            let me = By::Itself;
            let id_of = |s: &Stack, text: &str| s.items.iter().find(|i| i.text == text).map(|i| i.id.clone()).unwrap_or_default();

            add(&hub, id, strs(&["parser", "lexer", "docs", "ship"]), false).await?;
            let s = control(&hub, id, Action::Start).await?;
            assert_eq!(under_way(&s), Some(("parser".into(), true)));
            // Met, said the agent: the next goes at once; said again, nothing changes.
            settled(&hub, id).await;
            let s = done(&hub, id, None, Some("  all 212 tests   pass "), &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "active:lexer", "pending:docs", "pending:ship"]));
            assert_eq!(s.items.first().and_then(|i| i.reason.as_deref()), Some("the agent said so: all 212 tests pass"));
            assert_eq!(under_way(&s), Some(("lexer".into(), true)));
            let parser = id_of(&s, "parser");
            assert_eq!(words(&done(&hub, id, Some(parser.as_str()), None, &me).await?), words(&s), "met already: as it was");
            // A goal to come, met already: it joins the goals done, and the one under way stays.
            let s = done(&hub, id, Some(id_of(&s, "docs").as_str()), None, &By::You).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "active:lexer", "pending:ship"]));
            assert_eq!(s.items.get(1).and_then(|i| i.reason.as_deref()), Some("you said so"));
            // One goal changed by its id: other words; added next; moved before the goal under way.
            let s = update(&hub, id, &id_of(&s, "ship"), Change { text: Some("ship it".into()), ..Change::default() }, &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "active:lexer", "pending:ship it"]));
            let s = add_at(&hub, id, vec![NewGoal::from("bench".to_owned())], false, &At::Next).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "active:lexer", "pending:bench", "pending:ship it"]));
            settled(&hub, id).await;
            let s = update(&hub, id, &id_of(&s, "ship it"), Change { before: Some(id_of(&s, "lexer")), ..Change::default() }, &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "active:ship it", "pending:lexer", "pending:bench"]));
            assert!(matches!(update(&hub, id, &id_of(&s, "bench"), Change { next: true, after: Some(id_of(&s, "lexer")), ..Change::default() }, &me).await, Err(Error::Bad(_))), "one way");
            assert!(matches!(update(&hub, id, &parser, Change { text: Some("again".into()), ..Change::default() }, &me).await, Err(Error::Bad(_))), "a goal done stays");
            // Taken out: one to come goes; the goal under way ends skipped, saying who.
            let s = update(&hub, id, &id_of(&s, "bench"), Change { remove: true, ..Change::default() }, &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "active:ship it", "pending:lexer"]));
            settled(&hub, id).await;
            let s = update(&hub, id, &id_of(&s, "ship it"), Change { remove: true, ..Change::default() }, &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "skipped:ship it", "active:lexer"]));
            assert_eq!(s.items.get(2).and_then(|i| i.reason.as_deref()), Some("Taken out by the agent."));
            // Split: its parts go first (the first now), it waits after them, and said met it takes
            // its parts still to go with it.
            settled(&hub, id).await;
            let lexer = id_of(&s, "lexer");
            let s = split(&hub, id, None, strs(&["numbers scan", "strings scan"])).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "skipped:ship it", "active:numbers scan", "pending:strings scan", "pending:lexer"]));
            assert!(s.items.iter().filter(|i| i.text.ends_with(" scan")).all(|i| i.part_of.as_deref() == Some(lexer.as_str())));
            assert!(matches!(split(&hub, id, None, Vec::new()).await, Err(Error::Bad(_))), "a part isn't split again, nor a goal without parts");
            settled(&hub, id).await;
            let s = done(&hub, id, None, None, &me).await?;
            assert_eq!(under_way(&s), Some(("strings scan".into(), true)));
            let s = done(&hub, id, Some(lexer.as_str()), Some("the lexer's tests pass"), &me).await?;
            assert_eq!(words(&s), strs(&["met:parser", "met:docs", "skipped:ship it", "met:numbers scan", "met:strings scan", "met:lexer"]));
            // Paused for a while, saying why: it says who paused it and when it resumes.
            add(&hub, id, strs(&["deploy"]), true).await?;
            let later = hub.clock.unix_ms() + 600_000;
            let s = control_as(&hub, id, Action::Pause, &me, Pausing { why: Some("waiting for CI".into()), until: Some(later), review: None }).await?;
            assert_eq!((s.state.as_str(), s.reason.as_deref(), s.resume_at), ("paused", Some("Paused by the agent: waiting for CI"), later));
            assert!(matches!(control_as(&hub, id, Action::Pause, &me, Pausing { why: None, until: Some(hub.clock.unix_ms() - 1), review: None }).await, Err(Error::Bad(_))), "a pause for a while ends after now");
            settled(&hub, id).await;
            let s: Stack = control(&hub, id, serde_json::from_value(serde_json::json!("resume"))?).await?;
            assert_eq!((s.state.as_str(), s.resume_at, s.reason.as_deref()), ("running", 0, None));
            // Done as an action: the goal under way is met.
            let s: Stack = control(&hub, id, serde_json::from_value(serde_json::json!("done"))?).await?;
            assert_eq!(words(&s).last().map(String::as_str), Some("met:deploy"));
            assert!(matches!(done(&hub, id, None, None, &me).await, Err(Error::Bad(_))), "nothing under way to say met");
            Ok(())
        })
    }

    /// What waits on a person: a pause its agent made with no time to resume, or a plan an agent
    /// wants looked at first; not yours, nor one for a while. On the agent's info while it waits
    /// (as each tick of the driver puts it), gone once it's resumed.
    #[test]
    fn goals_an_agent_pauses_for_you_are_on_its_info_until_resumed() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = agent_with_goals(&["tests pass", "ship it"]).await?;
            let id = id.as_str();
            settled(&hub, id).await;
            let tick = async || {
                hub.harness.set_held(&hub, &held_stacks(&hub.store).await);
                hub.harness.info(id).and_then(|i| i.held)
            };
            // For a while: it resumes by itself.
            let later = hub.clock.unix_ms() + 600_000;
            let s = control_as(&hub, id, Action::Pause, &By::Itself, Pausing { why: Some("waiting for CI".into()), until: Some(later), review: None }).await?;
            assert_eq!((s.state.as_str(), s.held_for), ("paused", None));
            assert_eq!(tick().await, None);
            // Until someone resumes it: it waits on you, saying why.
            let s = control_as(&hub, id, Action::Pause, &By::Itself, Pausing { why: Some("I need the staging password".into()), until: None, review: None }).await?;
            assert_eq!(s.held_for.as_deref(), Some(""));
            let held = tick().await;
            assert!(held.as_ref().is_some_and(|h| h.why == "Paused by the agent: I need the staging password" && h.plan.is_none() && h.since > 0), "{held:?}");
            // Resumed: nothing waits.
            let s: Stack = control(&hub, id, serde_json::from_value(serde_json::json!("resume"))?).await?;
            assert_eq!((s.state.as_str(), s.held_for), ("running", None));
            assert_eq!(tick().await, None);
            // Yours doesn't wait on you.
            let s = control_as(&hub, id, Action::Pause, &By::You, Pausing::default()).await?;
            assert_eq!((s.state.as_str(), s.held_for), ("paused", None));
            // A plan to look at first: its id, whichever agent asked.
            let s = control_as(&hub, id, Action::Pause, &By::Agent("Release prep".into()), Pausing { why: Some("plan ready".into()), until: None, review: Some("pl_1".into()) }).await?;
            assert_eq!(s.held_for.as_deref(), Some("pl_1"));
            assert_eq!(tick().await.and_then(|h| h.plan).as_deref(), Some("pl_1"));
            Ok(())
        })
    }

    /// A plan's step put on an agent's goal stack by itself (`only`), as a goal stack of its own
    /// (`split`): its acceptance criteria and tests, a goal each, before it; it keeps its number.
    #[test]
    fn a_plan_step_goes_on_a_stack_as_a_goal_stack_of_its_own() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = agent_with_goals(&["keep the docs current"]).await?;
            let id = id.as_str();
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let crit = |t: &str| crate::plans::Criterion::Qualitative { text: t.into() };
            let parser = crate::plans::Item {
                title: "Parser".into(),
                acceptance: vec![crit("expressions parse"), crit("errors say where")],
                testing: vec![crate::plans::Test { scenario: "nested".into(), criteria: vec![crit("((1)) is 1")] }],
                ..crate::plans::Item::default()
            };
            let items = vec![crate::plans::Item { title: "Lexer".into(), ..crate::plans::Item::default() }, parser];
            let p = crate::plans::create(&hub, crate::plans::New { title: "Compiler".into(), items, ..Default::default() }, "admin").await?;
            settled(&hub, id).await;
            let ran = call_ok(crate::ops::call(cx.clone(), "plans.run", serde_json::json!({"id": p.id, "session": id, "mode": "goals", "only": [2], "split": true})).await)?;
            assert_eq!(ran.get("split"), Some(&serde_json::json!(1)), "{ran}");
            let s = get(&hub, id).await?;
            let step = s.items.iter().find(|i| i.step.as_ref().is_some_and(|st| st.plan == p.id) && i.part_of.is_none()).cloned().unwrap_or_default();
            assert_eq!(step.step.as_ref().map(|st| st.n), Some(2), "it keeps its number: {:?}", words(&s));
            let parts: Vec<&str> = s.items.iter().filter(|i| i.part_of.as_deref() == Some(step.id.as_str())).map(|i| i.part_kind.as_deref().unwrap_or("")).collect();
            assert_eq!(parts, ["checks", "checks", "checks"], "{:?}", words(&s));
            assert!(!s.items.iter().any(|i| i.text.contains("Lexer")), "only the step asked for: {:?}", words(&s));
            // Steps numbered from 1: 0 isn't one.
            let code = |r: Result<serde_json::Value, crate::ops::Fail>| r.err().map(|(c, m)| (c.as_u16(), m));
            let refused = code(crate::ops::call(cx.clone(), "plans.run", serde_json::json!({"id": p.id, "session": id, "mode": "goals", "only": [0]})).await);
            assert!(refused.as_ref().is_some_and(|(c, m)| *c == 400 && m.contains("from 1")), "{refused:?}");
            Ok(())
        })
    }

    /// The text at `path` in `v`, if there's text there.
    fn at<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
        path.iter().try_fold(v, |v, k| v.get(*k)).and_then(serde_json::Value::as_str)
    }

    /// A hub and an agent on it (no harness runs) with a goal stack: `goals`, started.
    async fn agent_with_goals(goals: &[&str]) -> anyhow::Result<(Arc<Hub>, String, tempfile::TempDir)> {
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
        let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None };
        let ask = Ask {
            kind: "claude".into(),
            on: Some("local".into()),
            cwd: Some(folder.path().to_string_lossy().into_owned()),
            mode: Some("default".into()),
            name: Some("A".into()),
            count: Some(1),
            ..Ask::default()
        };
        let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent started"))?;
        settled(&hub, &a.id).await;
        add(&hub, &a.id, strs(goals), true).await?;
        Ok((hub, a.id, folder))
    }

    /// Its research: an internal finding and an external one (no page fetched: a book).
    async fn researched(hub: &Arc<Hub>, research: &str) -> anyhow::Result<()> {
        let add: crate::research::Add = serde_json::from_value(serde_json::json!({"findings": [
            {"claim": "the store already searches", "cites": [{"source": {"type": "code", "title": "store.rs", "locator": "store.rs:1"}, "quote": "fn search"}]},
            {"claim": "trigram indexes find substrings", "cites": [{"source": {"type": "book", "title": "SQLite internals", "locator": "ISBN 978-0-00-000000-0"}, "quote": "trigram"}]}
        ]}))?;
        crate::research::add(hub, research, add).await?;
        Ok(())
    }

    fn call_ok(r: Result<serde_json::Value, crate::ops::Fail>) -> anyhow::Result<serde_json::Value> {
        r.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))
    }

    /// A goal turned into a plan: its planning comes first (the goal waits after it), the plan isn't
    /// run while it's planned, and isn't ready without steps or without research inside and out;
    /// ready, its steps become the goal's parts, a step splits into its acceptance criteria and
    /// tests and is met once they are, and once every step is met so is the goal, at once.
    #[test]
    fn a_goal_becomes_a_plan_whose_steps_are_its_parts_and_is_met_once_they_all_are() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = agent_with_goals(&["agents can search conversations", "ship it"]).await?;
            let id = id.as_str();
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            settled(&hub, id).await;
            let planned = call_ok(crate::ops::call(cx.clone(), "goals.plan", serde_json::json!({"agent": id})).await)?;
            let (pid, rid) = (at(&planned, &["plan"]).unwrap_or_default().to_owned(), at(&planned, &["research"]).unwrap_or_default().to_owned());
            let s = get(&hub, id).await?;
            let goal = s.items.iter().find(|i| i.text == "agents can search conversations").cloned().unwrap_or_default();
            assert_eq!(goal.planned_as.as_deref(), Some(pid.as_str()));
            let first = s.items.iter().find(|i| i.status == "active").cloned().unwrap_or_default();
            assert!(
                first.part_of.as_deref() == Some(goal.id.as_str()) && first.part_kind.as_deref() == Some("planning") && first.text.starts_with("Plan \"agents can search conversations\""),
                "{first:?}"
            );
            assert_eq!(goal.status, "pending", "the goal waits after its planning");
            // Asked again: the same plan.
            let again = call_ok(crate::ops::call(cx.clone(), "goals.plan", serde_json::json!({"agent": id, "id": goal.id})).await)?;
            assert_eq!(at(&again, &["plan"]), Some(pid.as_str()));
            // Not run, and not ready, while it has no steps or research.
            let code = |r: Result<serde_json::Value, crate::ops::Fail>| r.err().map(|(c, m)| (c.as_u16(), m));
            let refused = code(crate::ops::call(cx.clone(), "plans.run", serde_json::json!({"id": pid, "session": id, "mode": "message"})).await);
            assert!(refused.as_ref().is_some_and(|(c, m)| *c == 400 && m.contains("still being planned")), "{refused:?}");
            let refused = code(crate::ops::call(cx.clone(), "plans.ready", serde_json::json!({"id": pid})).await);
            assert!(refused.as_ref().is_some_and(|(_, m)| m.contains("no steps yet")), "{refused:?}");
            let crit = |t: &str| crate::plans::Criterion::Qualitative { text: t.into() };
            let items = vec![
                crate::plans::Item { title: "Search the store".into(), acceptance: vec![crit("a query finds an event")], basis: vec!["f1".into()], ..crate::plans::Item::default() },
                crate::plans::Item { title: "An op for agents".into(), needs: vec![1], basis: vec!["f1".into(), "f2".into()], ..crate::plans::Item::default() },
            ];
            crate::plans::update(&hub, &pid, crate::plans::Patch { items: Some(items), ..crate::plans::Patch::default() }, "admin").await?;
            let refused = code(crate::ops::call(cx.clone(), "plans.ready", serde_json::json!({"id": pid})).await);
            assert!(refused.as_ref().is_some_and(|(_, m)| m.contains("no internal sources")), "{refused:?}");
            researched(&hub, &rid).await?;
            settled(&hub, id).await;
            let ready = call_ok(crate::ops::call(cx.clone(), "plans.ready", serde_json::json!({"id": pid})).await)?;
            assert_eq!((at(&ready, &["then"]), at(&ready, &["plan", "phase"])), (Some("goals"), Some("ready")), "{ready}");
            // It answers with its research for the agent to show the person: each claim graded, its sources numbered.
            let shown = at(&ready, &["research"]).unwrap_or_default();
            assert!(shown.contains("the store already searches") && shown.contains(" · ") && shown.contains("**Sources**") && shown.contains("store.rs (code, internal)"), "{shown}");
            assert!(at(&ready, &["say"]).is_some_and(|s| s.starts_with("Show the person this research")), "{ready}");
            // Its steps, the goal's parts, the first under way; the planning met.
            settled(&hub, id).await;
            let s = get(&hub, id).await?;
            let parts: Vec<&Item> = s.items.iter().filter(|i| i.part_of.as_deref() == Some(goal.id.as_str())).collect();
            let kinds: Vec<(&str, &str)> = parts.iter().map(|i| (i.part_kind.as_deref().unwrap_or(""), i.status.as_str())).collect();
            assert_eq!(kinds, [("planning", "met"), ("plan", "active"), ("plan", "pending")], "{:?}", words(&s));
            let step1 = parts.get(1).map(|i| i.id.clone()).unwrap_or_default();
            assert!(parts.get(1).is_some_and(|i| i.step.as_ref().is_some_and(|st| st.plan == pid && st.n == 1) && i.text.starts_with("Step 1 of 2 of the plan")));
            // The step splits into its acceptance criterion, and is met once it is.
            settled(&hub, id).await;
            let s = split(&hub, id, Some(&step1), Vec::new()).await?;
            let check = s.items.iter().find(|i| i.part_of.as_deref() == Some(step1.as_str())).cloned().unwrap_or_default();
            assert_eq!((check.part_kind.as_deref(), check.status.as_str()), (Some("checks"), "active"), "{:?}", words(&s));
            settled(&hub, id).await;
            let s = done(&hub, id, Some(&check.id), Some("a query found it"), &By::Itself).await?;
            let step = s.items.iter().find(|i| i.id == step1).cloned().unwrap_or_default();
            assert_eq!((step.status.as_str(), step.reason.as_deref()), ("met", Some("Met: each of its acceptance criteria and tests is met.")), "{:?}", words(&s));
            // The last step met: the goal too, and the next goal goes on.
            let step2 = s.items.iter().find(|i| i.status == "active").map(|i| i.id.clone()).unwrap_or_default();
            settled(&hub, id).await;
            let s = done(&hub, id, Some(&step2), None, &By::Itself).await?;
            let g = s.items.iter().find(|i| i.id == goal.id).cloned().unwrap_or_default();
            assert_eq!((g.status.as_str(), g.reason.as_deref()), ("met", Some("Met with its plan: its 2 steps are met.")), "{:?}", words(&s));
            assert_eq!(under_way(&s).map(|u| u.0), Some("ship it".to_owned()));
            Ok(())
        })
    }

    /// Ready for review, the plan's steps wait in a paused stack, saying so; run with an agent per
    /// step, the goal waits for the run (nothing sent to its agent for it) and is met once the run
    /// is done.
    #[test]
    fn a_planned_goal_waits_for_review_or_for_its_run() -> anyhow::Result<()> {
        within(async {
            let (hub, id, _folder) = agent_with_goals(&["one", "two"]).await?;
            let id = id.as_str();
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let s = get(&hub, id).await?;
            let (one, two) = (s.items.first().map(|i| i.id.clone()).unwrap_or_default(), s.items.get(1).map(|i| i.id.clone()).unwrap_or_default());
            let step = |t: &str| crate::plans::Item { title: t.into(), basis: vec!["f1".into()], ..crate::plans::Item::default() };
            let mut plans = Vec::new();
            for goal in [&one, &two] {
                settled(&hub, id).await;
                let p = call_ok(crate::ops::call(cx.clone(), "goals.plan", serde_json::json!({"agent": id, "id": goal})).await)?;
                let (pid, rid) = (at(&p, &["plan"]).unwrap_or_default().to_owned(), at(&p, &["research"]).unwrap_or_default().to_owned());
                crate::plans::update(&hub, &pid, crate::plans::Patch { items: Some(vec![step("a"), step("b")]), ..crate::plans::Patch::default() }, "admin").await?;
                researched(&hub, &rid).await?;
                plans.push(pid);
            }
            // For review, as the agent itself says it's ready: its steps go in, and the stack pauses,
            // saying why, waiting on you to look at the plan.
            let mut itself = crate::authz::Caller::owner();
            itself.agent = Some(id.to_owned());
            let r = call_ok(crate::ops::call(crate::ops::Cx { hub: Arc::clone(&hub), caller: itself }, "plans.ready", serde_json::json!({"id": plans.first(), "then": "review"})).await)?;
            let s = get(&hub, id).await?;
            assert!(s.state == "paused" && s.reason.as_deref().is_some_and(|w| w.contains("is ready for you to look at")), "{r} {:?}", s.reason);
            assert_eq!(s.held_for.as_deref(), plans.first().map(String::as_str), "it waits on you to look at the plan");
            assert_eq!(s.items.iter().filter(|i| i.part_of.as_deref() == Some(one.as_str()) && i.part_kind.as_deref() == Some("plan") && i.status == "pending").count(), 2);
            // Its steps met by hand: the goal is met; resumed, the next goal's planning goes on.
            for p in s.items.iter().filter(|i| i.part_of.as_deref() == Some(one.as_str()) && i.part_kind.as_deref() == Some("plan")) {
                done(&hub, id, Some(&p.id), None, &By::You).await?;
            }
            settled(&hub, id).await;
            control(&hub, id, Action::Start).await?;
            let s = get(&hub, id).await?;
            assert_eq!(s.items.iter().find(|i| i.id == one).map(|i| i.status.as_str()), Some("met"), "{:?}", words(&s));
            assert_eq!(s.held_for, None, "resumed, nothing waits on you");
            // With an agent per step: the goal waits on a part held for the run.
            settled(&hub, id).await;
            let r = call_ok(crate::ops::call(cx.clone(), "plans.ready", serde_json::json!({"id": plans.get(1), "then": "agents"})).await)?;
            let run = at(&r, &["run"]).unwrap_or_default().to_owned();
            let s = get(&hub, id).await?;
            let held = s.items.iter().find(|i| i.part_of.as_deref() == Some(two.as_str()) && i.part_kind.as_deref() == Some("run")).cloned().unwrap_or_default();
            assert_eq!((held.status.as_str(), held.how.as_deref(), held.waits_on.as_deref()), ("active", Some("run"), Some(run.as_str())), "{:?}", words(&s));
            // Not done: it waits. Done: the part is met, and with it the goal, each counted once as
            // ended, met (CP-M2's counters).
            let metrics = crate::metrics::AgentMetrics::default();
            push(&hub, id, &metrics).await;
            assert_eq!(get(&hub, id).await?.items.iter().find(|i| i.id == held.id).map(|i| i.status.clone()).as_deref(), Some("active"));
            let r2 = run.clone();
            assert_eq!(metrics.goals_ended.get(&["met"]), 0, "nothing ended while the run goes on");
            hub.store.call(move |c| Ok(c.execute("UPDATE plan_runs SET state = 'done' WHERE id = ?1", params![r2]).map(|_| ())?)).await?;
            push(&hub, id, &metrics).await;
            let s = get(&hub, id).await?;
            assert_eq!(s.items.iter().find(|i| i.id == held.id).map(|i| i.status.as_str()), Some("met"), "{:?}", words(&s));
            let g = s.items.iter().find(|i| i.id == two).cloned().unwrap_or_default();
            assert_eq!((g.status.as_str(), g.reason.as_deref()), ("met", Some("Met with its plan: its run is done, every step met.")), "{:?}", words(&s));
            assert_eq!(metrics.goals_ended.get(&["met"]), 2, "the run's part and its goal");
            Ok(())
        })
    }
}
