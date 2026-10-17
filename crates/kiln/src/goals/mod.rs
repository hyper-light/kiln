//! Goal stacks: an agent's goals, one after another, each pursued until it's met before the next
//! starts, as if you gave it /goal again and again yourself (docs/goals.md). On a harness with
//! goals of its own the goal goes to them (Claude Code's `/goal <condition>`, Codex's thread goal)
//! and the harness says how it came out; on the rest it goes as a message and is done when that
//! turn ends. A goal that isn't met pauses the stack, with why. The server keeps the stacks and
//! drives them (`run`), so they go on with no console open and across restarts.

pub mod ops;

use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness::model::{Command, SendMode, Status};
use crate::harness::registry::Sent;
use crate::harness::session::Info;
use crate::hub::Hub;

/// Most goals a stack holds.
const ITEMS: usize = 100;
/// Longest goal.
const TEXT: usize = 2000;
/// How long an agent may sit idle on a goal its harness hasn't taken up before the stack pauses.
const UNTAKEN_MS: i64 = 90_000;

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
    /// The agent was seen at work on it (a goal sent as a message is done when that work ends).
    #[serde(skip)]
    saw_running: bool,
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

fn now_ms() -> i64 {
    crate::util::now_unix().saturating_mul(1000)
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
    let agent = agent.to_owned();
    Ok(hub.store.call(move |c| load(c, &agent)).await?)
}

fn load(c: &rusqlite::Connection, agent: &str) -> anyhow::Result<Stack> {
    let head: Option<(String, Option<String>, i64)> =
        c.query_row("SELECT state, reason, updated_at FROM goal_stacks WHERE agent = ?1", params![agent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
    let mut st = c.prepare("SELECT id, text, status, reason, how, started_at, ended_at, prior_at, saw_running FROM goal_items WHERE agent = ?1 ORDER BY pos")?;
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
                saw_running: r.get::<_, i64>(8)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let (state, reason, updated_at) = head.unwrap_or_else(|| ("paused".into(), None, 0));
    Ok(Stack { agent: agent.to_owned(), state, reason, items, updated_at })
}

fn touch(c: &rusqlite::Connection, agent: &str, state: Option<&str>, reason: Option<&str>) -> anyhow::Result<()> {
    let now = now_ms();
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
    hub.store
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            let kept: i64 = tx.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status != 'pending'", params![agent], |r| r.get(0))?;
            if usize::try_from(kept).unwrap_or(0) + items.len() > ITEMS {
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
            touch(&tx, &agent, None, None)?;
            tx.commit()?;
            load(c, &agent)
        })
        .await
        .map_err(|e| if e.to_string().contains("at most") { Error::Bad(e.to_string()) } else { Error::Other(e) })
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
    // Leaving a goal under way (or one still set that wasn't met): its harness's own goal goes too.
    let active = stack.items.iter().any(|i| i.status == "active");
    if action == Action::Clear || (action == Action::Skip && active) {
        clear_in_agent(hub, &info, &stack.items).await;
    }
    let agent_s = agent.to_owned();
    Ok(hub
        .store
        .call(move |c| {
            let now = now_ms();
            match action {
                Action::Start => {
                    let left = c.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent_s], |r| r.get::<_, i64>(0))?;
                    touch(c, &agent_s, Some(if left > 0 { "running" } else { "done" }), None)?;
                }
                Action::Pause => touch(c, &agent_s, Some("paused"), Some("Paused by you."))?,
                Action::Skip => {
                    c.execute("UPDATE goal_items SET status = 'skipped', ended_at = ?2, reason = 'Skipped by you.' WHERE agent = ?1 AND status = 'active'", params![agent_s, now])?;
                    touch(c, &agent_s, None, None)?;
                }
                Action::Clear => {
                    c.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent_s])?;
                    touch(c, &agent_s, Some("paused"), None)?;
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
fn how(hub: &Hub, agent: &str) -> &'static str {
    let meta = hub.harness.meta(agent);
    let goal = meta.as_ref().and_then(|m| m.get("commands")).and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c.get("name").and_then(Value::as_str) == Some("goal")));
    match goal.and_then(|c| c.pointer("/route/kind")).and_then(Value::as_str) {
        Some("control") => "goal_control",
        // Sent as its words: the harness runs /goal itself (Claude Code).
        Some("prompt") | None if goal.is_some() => "goal",
        _ => "message",
    }
}

fn goal_command(how: &str, text: &str) -> Command {
    let send = |text: String| Command::Send { id: crate::util::new_id(), text, mode: SendMode::Queue, attachments: Vec::new(), overrides: None };
    match how {
        "goal_control" => Command::Control { id: "goal".into(), value: Some(json!({"text": text})) },
        "goal" => send(format!("/goal {text}")),
        _ => send(format!("Your goal: {text}\n\nWork on it until it's done, then say that it is.")),
    }
}

/// Clears the stack's goal in the agent's harness, where it has goals of its own: the one under
/// way, or one it stopped pursuing while still set (Claude Code's after its checks gave up), so
/// the agent doesn't go on chasing it.
async fn clear_in_agent(hub: &Hub, info: &Info, items: &[Item]) {
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return };
    let Some(item) = items.iter().rev().find(|i| i.text == o.text && i.how.is_some()) else { return };
    let c = match item.how.as_deref() {
        Some("goal_control") => Command::Control { id: "goal".into(), value: Some(json!({"text": "clear"})) },
        Some("goal") => goal_command("goal", "clear"),
        _ => return,
    };
    let _ = hub.harness.command(hub, &info.id, c).await;
}

/// Drives every running stack: sends the next goal to an agent that's free, and follows the one
/// under way to how it came out.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(Duration::from_millis(1500)).await {
            return Ok(());
        }
        let running: Vec<String> = hub
            .store
            .call(|c| {
                let mut st = c.prepare("SELECT agent FROM goal_stacks WHERE state = 'running'")?;
                let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                Ok(v)
            })
            .await
            .unwrap_or_default();
        for agent in running {
            if let Err(e) = step(&hub, &agent).await {
                tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
            }
        }
    }
}

/// What a running stack does next.
enum Next {
    Wait,
    /// The goal under way ended: met (go on), or not (pause, with why).
    End { id: String, status: &'static str, reason: Option<String> },
    Saw { id: String },
    Start { id: String, text: String },
    Pause(String),
    Done,
}

fn decide(stack: &Stack, info: Option<&Info>, now: i64) -> Next {
    let Some(info) = info else { return Next::Pause("The agent isn't there any more.".into()) };
    if info.status == Status::Exited {
        return Next::Pause("The agent stopped. Restart it, then start the stack again.".into());
    }
    let free = info.status == Status::Idle && info.approvals.is_empty() && info.queued == 0;
    if let Some(item) = stack.items.iter().find(|i| i.status == "active") {
        let id = item.id.clone();
        return match item.how.as_deref() {
            Some("goal" | "goal_control") => match info.objective.as_ref().filter(|o| o.text == item.text && o.at > item.prior_at) {
                Some(o) => match o.status.as_str() {
                    "met" => Next::End { id, status: "met", reason: o.reason.clone() },
                    "unmet" => Next::End { id, status: "unmet", reason: Some(o.reason.clone().unwrap_or_else(|| "It wasn't met.".into())) },
                    "cleared" => Next::End { id, status: "skipped", reason: Some("The goal was cleared in the agent.".into()) },
                    "paused" => Next::Pause("The goal was paused in the agent: resume it there, or start the stack again.".into()),
                    _ => Next::Wait,
                },
                // Not taken up yet (the message waits in its queue); or never, after a while idle.
                None if free && now - item.started_at.unwrap_or(now) > UNTAKEN_MS => Next::Pause("The agent didn't take the goal up.".into()),
                None => Next::Wait,
            },
            // As a message: done when the work it started ends.
            _ if matches!(info.status, Status::Running | Status::Waiting | Status::Starting) && !item.saw_running => Next::Saw { id },
            _ if item.saw_running && free => Next::End { id, status: "met", reason: None },
            _ if free && now - item.started_at.unwrap_or(now) > UNTAKEN_MS => Next::End { id, status: "met", reason: Some("Its turn ended.".into()) },
            _ => Next::Wait,
        };
    }
    match stack.items.iter().find(|i| i.status == "pending") {
        None => Next::Done,
        Some(i) if free => Next::Start { id: i.id.clone(), text: i.text.clone() },
        Some(_) => Next::Wait,
    }
}

async fn step(hub: &Arc<Hub>, agent: &str) -> anyhow::Result<()> {
    let stack = get(hub, agent).await?;
    let info = hub.harness.list().into_iter().find(|i| i.id == agent);
    let now = now_ms();
    let next = decide(&stack, info.as_ref(), now);
    let a = agent.to_owned();
    match next {
        Next::Wait => {}
        Next::Saw { id } => {
            hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET saw_running = 1 WHERE id = ?1", params![id]).map(|_| ())?)).await?;
        }
        Next::End { id, status, reason } => {
            let pause = (status == "unmet").then(|| format!("A goal wasn't met: {}", reason.clone().unwrap_or_default()));
            hub.store
                .call(move |c| {
                    c.execute("UPDATE goal_items SET status = ?2, reason = ?3, ended_at = ?4 WHERE id = ?1", params![id, status, reason, now])?;
                    match pause {
                        Some(why) => touch(c, &a, Some("paused"), Some(&why)),
                        None => touch(c, &a, None, None),
                    }
                })
                .await?;
        }
        Next::Pause(why) => {
            hub.store.call(move |c| touch(c, &a, Some("paused"), Some(&why))).await?;
        }
        Next::Done => {
            hub.store.call(move |c| touch(c, &a, Some("done"), None)).await?;
        }
        Next::Start { id, text } => {
            let how = how(hub, agent);
            let prior_at = info.as_ref().and_then(|i| i.objective.as_ref()).map_or(0, |o| o.at);
            let sent = hub.harness.command(hub, agent, goal_command(how, &text)).await;
            let a2 = a.clone();
            match sent {
                Sent::Ok => {
                    hub.store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET status = 'active', how = ?2, prior_at = ?3, started_at = ?4, saw_running = 0 WHERE id = ?1",
                                params![id, how, prior_at, now],
                            )?;
                            touch(c, &a2, None, None)
                        })
                        .await?;
                }
                Sent::NoSession => hub.store.call(move |c| touch(c, &a2, Some("paused"), Some("The agent isn't there any more."))).await?,
                Sent::Unreachable => hub.store.call(move |c| touch(c, &a2, Some("paused"), Some("The agent's machine isn't connected."))).await?,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::Objective;

    fn info(status: Status, objective: Option<Objective>) -> Info {
        Info { id: "a1".into(), status, objective, ..Info::default() }
    }

    fn item(status: &str, how: Option<&str>) -> Item {
        Item { id: "g1".into(), text: "tests pass".into(), status: status.into(), how: how.map(str::to_owned), started_at: Some(1_000), prior_at: 5, ..Item::default() }
    }

    fn stack(items: Vec<Item>) -> Stack {
        Stack { agent: "a1".into(), state: "running".into(), items, ..Stack::default() }
    }

    fn obj(status: &str, at: i64) -> Objective {
        Objective { text: "tests pass".into(), status: status.into(), reason: Some("why".into()), at }
    }

    #[test]
    fn goals_start_when_the_agent_is_free_and_end_on_its_harness_verdict() {
        let pending = stack(vec![item("pending", None)]);
        assert!(matches!(decide(&pending, Some(&info(Status::Idle, None)), 2_000), Next::Start { .. }));
        assert!(matches!(decide(&pending, Some(&info(Status::Running, None)), 2_000), Next::Wait));
        let native = stack(vec![item("active", Some("goal"))]);
        // The verdict of the goal before it doesn't count.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 5)))), 2_000), Next::Wait));
        assert!(matches!(decide(&native, Some(&info(Status::Running, Some(obj("active", 9)))), 2_000), Next::Wait));
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("met", 9)))), 2_000), Next::End { status: "met", .. }));
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("unmet", 9)))), 2_000), Next::End { status: "unmet", .. }));
        assert!(matches!(decide(&native, Some(&info(Status::Idle, Some(obj("paused", 9)))), 2_000), Next::Pause(_)));
        // Never taken up.
        assert!(matches!(decide(&native, Some(&info(Status::Idle, None)), 1_000 + UNTAKEN_MS + 1), Next::Pause(_)));
        assert!(matches!(decide(&stack(vec![item("met", Some("goal"))]), Some(&info(Status::Idle, None)), 2_000), Next::Done));
        assert!(matches!(decide(&native, None, 2_000), Next::Pause(_)));
        assert!(matches!(decide(&native, Some(&info(Status::Exited, None)), 2_000), Next::Pause(_)));
    }

    #[test]
    fn a_goal_sent_as_a_message_is_done_when_that_work_ends() {
        let msg = stack(vec![item("active", Some("message"))]);
        assert!(matches!(decide(&msg, Some(&info(Status::Idle, None)), 2_000), Next::Wait));
        assert!(matches!(decide(&msg, Some(&info(Status::Running, None)), 2_000), Next::Saw { .. }));
        let mut seen = item("active", Some("message"));
        seen.saw_running = true;
        assert!(matches!(decide(&stack(vec![seen.clone()]), Some(&info(Status::Running, None)), 2_000), Next::Wait));
        assert!(matches!(decide(&stack(vec![seen]), Some(&info(Status::Idle, None)), 2_000), Next::End { status: "met", .. }));
    }

    #[test]
    fn goals_are_one_line() {
        assert_eq!(clean("  ship\n the   lexer ").ok().as_deref(), Some("ship the lexer"));
        assert!(clean("   ").is_err());
        assert!(clean(&"x".repeat(TEXT + 1)).is_err());
        assert!(matches!(goal_command("goal", "tests pass"), Command::Send { text, .. } if text == "/goal tests pass"));
        assert!(matches!(goal_command("goal_control", "ship"), Command::Control { id, value: Some(v) } if id == "goal" && v.get("text") == Some(&json!("ship"))));
        assert!(matches!(goal_command("message", "ship"), Command::Send { text, .. } if text.starts_with("Your goal: ship")));
    }
}
