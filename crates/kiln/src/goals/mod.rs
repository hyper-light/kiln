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
//! them (`run`), so they go on with no console open and across restarts.
//!
//! The driver ([`Driver`]) takes its time from a kiln-seams `Clock` and reaches agents through a
//! `HarnessPort` (docs/seams.md): in production, the hub's clock (kiln's runtime) and its harness
//! registry; in tests, a simulated clock and agents, so a 15-minute back-off is checked in
//! microseconds.

pub mod keep;
pub mod ops;

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use kiln_seams::HarnessPort;
use kiln_seams::harness::Sent;
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness::model::{Command, SendMode, Status};
use crate::harness::session::Info;
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
    /// When kiln last cleared the goal in the agent itself (pause, skip, clear).
    #[serde(skip)]
    dropped_at: i64,
}

/// One goal.
#[derive(Debug, Clone, Default, Serialize)]
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
}

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
    /// Run it: each goal in turn (resuming one that paused).
    Start,
    /// Stop sending goals; the one under way goes on in the agent.
    Pause,
    /// Leave the goal under way (its harness's goal cleared) and go on with the next.
    Skip,
    /// Empty the stack (the goal under way, or one still set, cleared in the agent).
    Clear,
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
    let head: Option<(String, Option<String>, i64, i64)> = c
        .query_row("SELECT state, reason, updated_at, dropped_at FROM goal_stacks WHERE agent = ?1", params![agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()?;
    let mut st = c.prepare("SELECT id, text, status, reason, how, started_at, ended_at, prior_at, reminders, reminded_at, busy_at FROM goal_items WHERE agent = ?1 ORDER BY pos")?;
    let items = st
        .query_map(params![agent], |r| {
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
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let (state, reason, updated_at, dropped_at) = head.unwrap_or_else(|| ("paused".into(), None, 0, 0));
    Ok(Stack { agent: agent.to_owned(), state, reason, items, updated_at, dropped_at })
}

fn touch(c: &rusqlite::Connection, agent: &str, state: Option<&str>, reason: Option<&str>, now: i64) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO goal_stacks (agent, state, reason, created_at, updated_at) VALUES (?1, COALESCE(?2, 'paused'), ?3, ?4, ?4)
         ON CONFLICT(agent) DO UPDATE SET state = COALESCE(?2, state), reason = CASE WHEN ?2 IS NULL THEN reason ELSE ?3 END, updated_at = ?4",
        params![agent, state, reason, now],
    )?;
    Ok(())
}

fn known(hub: &Hub, agent: &str) -> R<Info> {
    hub.harness.list().into_iter().find(|i| i.id == agent).ok_or(Error::NoAgent)
}

/// Replaces the goals not yet started with `given`, in that order (existing ones by id, edited;
/// new ones added); goals done or under way stay as they are, before them.
pub async fn set(hub: &Hub, agent: &str, given: Vec<Given>) -> R<Stack> {
    known(hub, agent)?;
    let mut items = Vec::new();
    for g in given {
        items.push((g.id, clean(&g.text)?));
    }
    let agent = agent.to_owned();
    let now = now_ms(hub);
    hub.store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let kept: i64 = tx.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status != 'pending'", params![agent], |r| r.get(0))?;
            if !fits(usize::try_from(kept).unwrap_or(usize::MAX), items.len()) {
                anyhow::bail!("a stack holds at most {ITEMS} goals");
            }
            let pending: Vec<String> = {
                let mut st = tx.prepare("SELECT id FROM goal_items WHERE agent = ?1 AND status = 'pending'")?;
                st.query_map(params![agent], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?
            };
            let keep: Vec<&String> = items.iter().filter_map(|(id, _)| id.as_ref()).filter(|id| pending.contains(id)).collect();
            for id in &pending {
                if !keep.contains(&id) {
                    tx.execute("DELETE FROM goal_items WHERE id = ?1", params![id])?;
                }
            }
            for (i, (id, text)) in items.iter().enumerate() {
                let pos = kept + i64::try_from(i).unwrap_or(0);
                match id.as_ref().filter(|id| pending.contains(id)) {
                    Some(id) => {
                        tx.execute("UPDATE goal_items SET text = ?2, pos = ?3 WHERE id = ?1", params![id, text, pos])?;
                    }
                    None => {
                        tx.execute("INSERT INTO goal_items (id, agent, pos, text) VALUES (?1, ?2, ?3, ?4)", params![crate::util::new_id(), agent, pos, text])?;
                    }
                }
            }
            touch(&tx, &agent, None, None, now)?;
            tx.commit()?;
            load(c, &agent)
        })
        .await
        .map_err(|e| if e.to_string().contains("at most") { Error::Bad(e.to_string()) } else { Error::Other(e) })
}

/// Whether `kept` goals done or under way, and `given` more, fit in one stack.
fn fits(kept: usize, given: usize) -> bool {
    kept.checked_add(given).is_some_and(|n| n <= ITEMS)
}

/// Adds goals after the ones there; `start` runs the stack.
pub async fn add(hub: &Hub, agent: &str, texts: Vec<String>, start: bool) -> R<Stack> {
    let now = get(hub, agent).await?;
    let mut given: Vec<Given> = now.items.iter().filter(|i| i.status == "pending").map(|i| Given { id: Some(i.id.clone()), text: i.text.clone() }).collect();
    given.extend(texts.into_iter().map(|text| Given { id: None, text }));
    let stack = set(hub, agent, given).await?;
    if start { control(hub, agent, Action::Start).await } else { Ok(stack) }
}

/// Starts, pauses, skips or clears the agent's stack.
pub async fn control(hub: &Hub, agent: &str, action: Action) -> R<Stack> {
    let info = known(hub, agent)?;
    let stack = get(hub, agent).await?;
    // Leaving a goal under way (or one still set that wasn't met), or pausing it: its harness's own
    // goal goes too, so nothing holds the agent to it.
    let active = stack.items.iter().any(|i| i.status == "active");
    if action == Action::Clear || (matches!(action, Action::Skip | Action::Pause) && active) {
        clear_in_agent(hub, &info, &stack.items).await;
    }
    // What the agent last said of its goal: a verdict from before you resume doesn't count.
    let said_at = info.objective.as_ref().map_or(0, |o| o.at);
    let agent_s = agent.to_owned();
    let now = now_ms(hub);
    Ok(hub
        .store
        .call(move |c| {
            match action {
                Action::Start => {
                    // The goal under way goes to the agent again at once (its last send forgotten:
                    // the pause since it started is long over), reminders starting over.
                    c.execute(
                        "UPDATE goal_items SET reminded_at = 0, reminders = 0, busy_at = 0, prior_at = MAX(prior_at, ?2) WHERE agent = ?1 AND status = 'active'",
                        params![agent_s, said_at],
                    )?;
                    let left = c.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent_s], |r| r.get::<_, i64>(0))?;
                    touch(c, &agent_s, Some(if left > 0 { "running" } else { "done" }), None, now)?;
                }
                Action::Pause => {
                    touch(c, &agent_s, Some("paused"), Some(PAUSED_BY_YOU), now)?;
                    c.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Skip => {
                    c.execute("UPDATE goal_items SET status = 'skipped', ended_at = ?2, reason = 'Skipped by you.' WHERE agent = ?1 AND status = 'active'", params![agent_s, now])?;
                    touch(c, &agent_s, None, None, now)?;
                    c.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Clear => {
                    c.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent_s])?;
                    touch(c, &agent_s, Some("paused"), None, now)?;
                    c.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
            }
            load(c, &agent_s)
        })
        .await?)
}

/// Forgets an agent's stack (the agent was closed).
pub async fn forget(hub: &Hub, agent: &str) -> R<()> {
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

/// The goal under way, sent again. A stopped agent starts again for a message (a control waits
/// for it to run), so to one that stopped the goal goes as words.
fn reminder_command(how: &str, text: &str, stopped: bool) -> Command {
    match how {
        "goal_control" if stopped => send(format!("Your goal isn't met yet: {text}\n\nKeep working on it until it is.")),
        "goal" | "goal_control" => goal_command(how, text),
        _ => send(format!("Your goal isn't met yet: {text}\n\nKeep working on it until it is. When it's met, end your reply with this line: {MET_LINE}")),
    }
}

fn send(text: String) -> Command {
    Command::Send { id: crate::util::new_id(), text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None }
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
/// the agent doesn't go on chasing it.
async fn clear_in_agent(hub: &Hub, info: &Info, items: &[Item]) {
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return };
    let Some(item) = items.iter().rev().find(|i| same(&i.text, &o.text) && i.how.is_some()) else { return };
    let c = match item.how.as_deref() {
        Some("goal_control") => Command::Control { id: "goal".into(), value: Some(json!({"text": "clear"})) },
        Some("goal") => goal_command("goal", "clear"),
        _ => return,
    };
    let _ = hub.harness.command(hub, &info.id, c).await;
}

/// Drives every running stack: sends the next goal to an agent that's free, follows the one under
/// way to how it came out, and sends it again when the agent stops short of it. Goals set in an
/// agent directly join its stack first.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    Driver { store: &hub.store, clock: hub.clock.as_ref(), shutdown: &hub.shutdown, port: hub.as_ref(), hub: Some(&hub) }.drive().await
}

/// What the goal driver works with: the stacks' store, its clock, what stops it, and its agents.
pub struct Driver<'a, P> {
    pub store: &'a crate::store::Store,
    pub clock: &'a dyn kiln_seams::Clock,
    pub shutdown: &'a crate::rt::Shutdown,
    pub port: &'a P,
    /// The hub, for parents told of their children (`harness::told`) and paused sessions that go
    /// on elsewhere (`harness::elsewhere`): none for the driver alone (its tests).
    pub hub: Option<&'a Arc<Hub>>,
}

impl<P> Driver<'_, P>
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    /// Every [`TICK`] by its clock, until it's shut down: each running stack stepped, goals set
    /// in agents adopted, agents no stack drives kept going.
    pub async fn drive(&self) -> anyhow::Result<()> {
        loop {
            if !self.tick().await {
                return Ok(());
            }
            for info in self.port.agents() {
                if let Err(e) = adopt(self, &info).await {
                    tracing::warn!(agent = %info.id, error = %format!("{e:#}"), "goal stack");
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
                if let Err(e) = step(self, agent).await {
                    tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
                }
            }
            // The agents no goal stack drives: kept going when they stop part-way (`keep`).
            let now = self.clock.unix_ms();
            let all = self.port.agents();
            for info in all.iter().filter(|i| !running.contains(&i.id)) {
                keep::tick(self.port, info, now, crate::harness::told::children_working(&all, &info.id)).await;
            }
            keep::retain(&all.iter().map(|i| i.id.clone()).collect::<Vec<_>>());
            if let Some(hub) = self.hub {
                // Parents told of their children that settled, so none has to wait for them.
                crate::harness::told::tick(hub, &all, now, &running).await;
                // The paused ones that go on on another machine at a limit (`harness::elsewhere`).
                crate::harness::elsewhere::tick(hub, &all, now);
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

/// A goal set in the agent that its stack doesn't know (`/goal` typed in its composer, or its
/// harness's own goal set some other way), pursued or given up on for now but still set, joins the
/// stack as the goal under way,
/// so kiln keeps it like the others; one the stack had under way goes back to the front of what's
/// next. The stack runs.
async fn adopt<P: HarnessPort>(d: &Driver<'_, P>, info: &Info) -> anyhow::Result<()> {
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return Ok(()) };
    let Ok(text) = clean(&o.text) else { return Ok(()) };
    let (agent, how, at, now) = (info.id.clone(), how(d.port, &info.id), o.at, d.clock.unix_ms());
    d.store
        .call(move |c| {
            // One you cleared or skipped is being cleared in the agent: it doesn't come back.
            let dropped: i64 = c.query_row("SELECT dropped_at FROM goal_stacks WHERE agent = ?1", params![agent], |r| r.get(0)).optional()?.unwrap_or(0);
            if at <= dropped {
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
    /// on it since it was last sent; `stopped`: its harness isn't running (a message starts it).
    Remind { id: String, text: String, how: String, worked: bool, stopped: bool },
    Done,
}

fn decide(stack: &Stack, info: Option<&Info>, now: i64, children: bool) -> Next {
    // An agent kiln doesn't know right now is waited for: its runner is reconnecting, or the
    // server has just started. (A closed agent's stack goes when it's closed.)
    let Some(info) = info else { return Next::Wait };
    // Paused for a usage limit: nothing is sent (no goal, no reminder) until the pause ends; the
    // goal under way goes on then.
    if info.pause.is_some() {
        return Next::Wait;
    }
    let busy = matches!(info.status, Status::Running | Status::Waiting | Status::Starting);
    // Nothing waiting on you or in its queue; stopped counts (a message starts it again). Not
    // while agents it started are at work: it ended its turn to let them, and is told when each
    // settles (`harness::told`).
    let free = matches!(info.status, Status::Idle | Status::Exited) && info.approvals.is_empty() && info.queued == 0 && !children;
    if let Some(item) = stack.items.iter().find(|i| i.status == "active") {
        let id = item.id.clone();
        if busy {
            return if item.busy_at == 0 { Next::Busy { id } } else { Next::Wait };
        }
        if !free {
            return Next::Wait;
        }
        let how = item.how.clone().unwrap_or_else(|| "message".into());
        if matches!(how.as_str(), "goal" | "goal_control") {
            // Its harness's word on this goal since it was last sent: met, or cleared (by you, in
            // the agent: it goes). Still set, given up, paused, or none at all: it's reminded.
            if let Some(o) = info.objective.as_ref().filter(|o| same(&o.text, &item.text) && o.at > item.prior_at) {
                // Cleared by kiln itself when you paused the stack isn't cleared by you.
                let own = stack.dropped_at > 0 && o.at >= stack.dropped_at && o.at - stack.dropped_at <= OWN_CLEAR_MS;
                match o.status.as_str() {
                    "met" => return Next::End { id, status: "met", reason: o.reason.clone() },
                    "cleared" if !own => return Next::End { id, status: "skipped", reason: Some("The goal was cleared in the agent.".into()) },
                    _ => {}
                }
            }
        } else if item.busy_at > 0 && said_met(&info.summary) {
            return Next::End { id, status: "met", reason: None };
        }
        // Since it was last sent (never, by these rules: a goal just resumed, or under way from
        // before them: at once).
        if now.saturating_sub(item.reminded_at) < gap(item.reminders) {
            return Next::Wait;
        }
        let worked = item.busy_at > 0 && info.last_activity.saturating_sub(item.busy_at) >= WORKED_MS;
        return Next::Remind { id, text: item.text.clone(), how, worked, stopped: info.status == Status::Exited };
    }
    match stack.items.iter().find(|i| i.status == "pending") {
        None => Next::Done,
        Some(i) if free => Next::Start { id: i.id.clone(), text: i.text.clone() },
        Some(_) => Next::Wait,
    }
}

async fn step<P>(d: &Driver<'_, P>, agent: &str) -> anyhow::Result<()>
where
    P: HarnessPort<Agent = Info, Command = Command>,
{
    let (store, port) = (d.store, d.port);
    let stack = stack_of(store, agent).await?;
    let all = port.agents();
    let children = crate::harness::told::children_working(&all, agent);
    let info = all.into_iter().find(|i| i.id == agent);
    let now = d.clock.unix_ms();
    let next = decide(&stack, info.as_ref(), now, children);
    let a = agent.to_owned();
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
        }
        Next::Done => {
            store.call(move |c| touch(c, &a, Some("done"), None, now)).await?;
        }
        Next::Start { id, text } => {
            let how = how(port, agent);
            let stopped = info.as_ref().is_some_and(|i| i.status == Status::Exited);
            let first = if stopped && how == "goal_control" { send(format!("Your goal: {text}\n\nWork on it until it's met.")) } else { goal_command(how, &text) };
            match port.send(agent, first).await {
                Sent::Delivered => {
                    store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET status = 'active', how = ?2, prior_at = ?3, started_at = ?4, reminded_at = ?4, reminders = 0, busy_at = 0 WHERE id = ?1",
                                params![id, how, prior_at, now],
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
        Next::Remind { id, text, how, worked, stopped } => match port.send(agent, reminder_command(&how, &text, stopped)).await {
            Sent::Delivered => {
                tracing::info!(%agent, worked, stopped, "goal sent again: the agent stopped short of it");
                store
                    .call(move |c| {
                        c.execute(
                            "UPDATE goal_items SET reminded_at = ?2, reminders = CASE WHEN ?3 THEN 1 ELSE reminders + 1 END, prior_at = ?4, busy_at = 0 WHERE id = ?1",
                            params![id, now, worked, prior_at],
                        )?;
                        touch(c, &a, None, None, now)
                    })
                    .await?;
            }
            // Not known right now, or its machine is away (or its inbox full): tried again after a
            // pause, as a reminder that brought no work.
            Sent::NoSession | Sent::Unreachable | Sent::Full => {
                store
                    .call(move |c| Ok(c.execute("UPDATE goal_items SET reminded_at = ?2, reminders = reminders + 1 WHERE id = ?1", params![id, now]).map(|_| ())?))
                    .await?;
            }
        },
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
        assert!(matches!(decide(&stack(vec![item("pending", None)]), Some(&paused), 2_000, false), Next::Wait));
        assert!(matches!(decide(&stack(vec![item("active", None)]), Some(&paused), LATER * 10, false), Next::Wait));
        assert!(matches!(decide(&stack(vec![item("active", Some("goal"))]), Some(&paused), LATER * 10, false), Next::Wait));
        // Once it goes on, so does the stack.
        assert!(matches!(decide(&stack(vec![item("pending", None)]), Some(&info(Status::Idle, None)), 2_000, false), Next::Start { .. }));
    }

    #[test]
    fn goals_start_when_the_agent_is_free_and_end_when_its_harness_says_met() {
        let pending = stack(vec![item("pending", None)]);
        assert!(matches!(decide(&pending, Some(&info(Status::Idle, None)), 2_000, false), Next::Start { .. }));
        assert!(matches!(decide(&pending, Some(&info(Status::Running, None)), 2_000, false), Next::Wait));
        // A stopped agent starts again for its next goal.
        assert!(matches!(decide(&pending, Some(&info(Status::Exited, None)), 2_000, false), Next::Start { .. }));
        let native = stack(vec![item("active", Some("goal"))]);
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 9)))), 2_000, false), Next::End { status: "met", .. }));
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("cleared", 9)))), 2_000, false), Next::End { status: "skipped", .. }));
        // The verdict on the goal before it doesn't count.
        assert!(!matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 5)))), LATER, false), Next::End { .. }));
        assert!(matches!(decide(&stack(vec![item("met", Some("goal"))]), Some(&info(Status::Idle, None)), 2_000, false), Next::Done));
        // Not known right now (its runner reconnecting, the server just started): waited for.
        assert!(matches!(decide(&native, None, LATER, false), Next::Wait));
    }

    #[test]
    fn an_agent_that_stops_short_of_its_goal_is_reminded_never_left() {
        let native = stack(vec![item("active", Some("goal"))]);
        // Gave up, paused it, stopped with it still set, never took it up, or lost it when it
        // started again: once the pause since it was sent is over, it's sent again.
        for o in [Some(obj("unmet", 9)), Some(obj("paused", 9)), Some(obj("active", 9)), None] {
            assert!(matches!(decide(&native, Some(&info(Status::Idle, o.clone())), 2_000, false), Next::Wait), "{o:?}");
            assert!(matches!(decide(&native, Some(&info(Status::Idle, o.clone())), LATER, false), Next::Remind { stopped: false, .. }), "{o:?}");
        }
        // Stopped: sent again all the same (the message starts it).
        assert!(matches!(decide(&native, Some(&info(Status::Exited, None)), LATER, false), Next::Remind { stopped: true, .. }));
        // Agents it started at work: it ended its turn to let them, and isn't reminded until they settle.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, None)), LATER, true), Next::Wait));
        // At work on it: noted once, then left to it.
        assert!(matches!(decide(&native, Some(&info(Status::Running, None)), LATER, false), Next::Busy { .. }));
        let mut working = item("active", Some("goal"));
        working.busy_at = 2_000;
        assert!(matches!(decide(&stack(vec![working]), Some(&info(Status::Running, None)), LATER, false), Next::Wait));
        // Waiting on you, or with something in its queue: not reminded.
        let mut asking = info(Status::Idle, None);
        asking.approvals = vec![ApprovalBrief { id: "p1".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "cargo test".into(), at: 1 }];
        assert!(matches!(decide(&native, Some(&asking), LATER, false), Next::Wait));
        let mut queued = info(Status::Idle, None);
        queued.queued = 1;
        assert!(matches!(decide(&native, Some(&queued), LATER, false), Next::Wait));
    }

    #[test]
    fn the_clear_a_pause_sends_is_kiln_s_own_and_not_the_end_of_the_goal() {
        let mut paused_then_resumed = stack(vec![item("active", Some("goal"))]);
        paused_then_resumed.dropped_at = 7;
        // Its harness says the goal was cleared, just after kiln cleared it to pause: sent again.
        assert!(matches!(decide(&paused_then_resumed, Some(&info(Status::Idle, Some(obj("cleared", 9)))), LATER, false), Next::Remind { .. }));
        // Cleared in the agent long after: by you, so the goal ends.
        assert!(matches!(decide(&paused_then_resumed, Some(&info(Status::Idle, Some(obj("cleared", 7 + OWN_CLEAR_MS + 1)))), LATER, false), Next::End { status: "skipped", .. }));
    }

    #[test]
    fn a_goal_resumed_or_from_before_reminders_goes_to_a_free_agent_at_once() {
        let mut resumed = item("active", Some("goal"));
        resumed.reminded_at = 0;
        // Any time now (unix ms): it has never been sent by these rules.
        assert!(matches!(decide(&stack(vec![resumed]), Some(&info(Status::Idle, None)), 1_792_000_000_000, false), Next::Remind { .. }));
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
        assert!(matches!(decide(&s, Some(&info(Status::Idle, None)), 10_000 + 8 * REMIND_MS - 1, false), Next::Wait));
        assert!(matches!(decide(&s, Some(&info(Status::Idle, None)), 10_000 + 8 * REMIND_MS, false), Next::Remind { worked: false, .. }));
        // It worked on it a good while since it was last sent: real work.
        let mut worked = item("active", Some("goal"));
        worked.reminded_at = 10_000;
        worked.busy_at = 11_000;
        let mut after = info(Status::Idle, None);
        after.last_activity = 11_000 + WORKED_MS;
        assert!(matches!(decide(&stack(vec![worked]), Some(&after), 10_000 + REMIND_MS, false), Next::Remind { worked: true, .. }));
    }

    /// CP-M2's test (docs/seams.md): the driver's reminder back-off, under the simulated clock. An
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
            let store = crate::store::Store::in_memory()?;
            let shutdown = crate::rt::Shutdown::new();
            let port: SimHarness<Info, Command> = SimHarness::new(Arc::new(clock.clone()));
            let (id, goal) = ("goals-backoff", "the parser takes every input");
            // Its harness has goals of its own (Claude Code's /goal); it's idle, its goal still set.
            let objective = Objective { text: goal.into(), status: "active".into(), reason: None, at: clock.unix_ms() };
            port.open(id, Info { id: id.into(), status: Status::Idle, objective: Some(objective), ..Info::default() });
            port.set_catalog(id, json!({"commands": [{"name": "goal", "route": {"kind": "prompt"}}]}));
            let driver = Driver { store: &store, clock: &clock, shutdown: &shutdown, port: &port, hub: None };
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

    #[test]
    fn a_goal_sent_as_a_message_is_met_when_the_agent_says_so() {
        let mut said = item("active", Some("message"));
        said.busy_at = 1_500;
        let mut done = info(Status::Idle, None);
        done.summary = "All 212 tests pass on every platform.\n\n**GOAL MET**".into();
        assert!(matches!(decide(&stack(vec![said.clone()]), Some(&done), 2_000, false), Next::End { status: "met", .. }));
        // Its turn ended without saying so: reminded.
        let mut not_yet = info(Status::Idle, None);
        not_yet.summary = "Fixed the parser; the lexer is next.".into();
        assert!(matches!(decide(&stack(vec![said]), Some(&not_yet), LATER, false), Next::Remind { .. }));
        // Words from before it was last sent don't count.
        assert!(!matches!(decide(&stack(vec![item("active", Some("message"))]), Some(&done), LATER, false), Next::End { .. }));
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
}
