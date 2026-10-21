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

pub mod keep;
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
    /// Run it: each goal in turn (resuming one that paused). What's next goes to the agent at once.
    Start,
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
    let head: Option<(String, Option<String>, i64, i64, i64)> = c
        .query_row("SELECT state, reason, updated_at, dropped_at, COALESCE(json_array_length(cleared), 0) FROM goal_stacks WHERE agent = ?1", params![agent], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
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
    let (state, reason, updated_at, dropped_at, cleared) = head.unwrap_or_else(|| ("paused".into(), None, 0, 0, 0));
    Ok(Stack { agent: agent.to_owned(), state, reason, items, updated_at, cleared, dropped_at })
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
    // the agent to it.
    if back {
        clear_in_agent(hub, info, &stack.items).await;
    }
    let agent_s = agent.to_owned();
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
                        "UPDATE goal_items SET status = 'pending', pos = ?2, text = ?3, reason = NULL, how = NULL, started_at = NULL, reminded_at = 0, reminders = 0, busy_at = 0 WHERE id = ?1",
                        params![id, pos, text],
                    )?,
                };
            }
            for id in &gone {
                tx.execute("DELETE FROM goal_items WHERE id = ?1", params![id])?;
            }
            touch(&tx, &agent_s, None, None)?;
            // Its goal under way changed: what the agent still pursues of the one before doesn't join
            // the stack again (`adopt`).
            if moved {
                tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now_ms()])?;
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
        push(hub, agent).await;
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
    let info = known(hub, agent)?;
    let texts = texts.iter().map(|t| clean(t)).collect::<R<Vec<_>>>()?;
    let stack = {
        let _one = CHANGING.lock().await;
        let now = get(hub, agent).await?;
        // Every goal still to go named, the one under way first (where it stays): what's added is new,
        // whatever its words.
        let under_way = now.items.iter().filter(|i| i.status == "active");
        let mut given: Vec<(Option<String>, String)> = under_way.chain(now.items.iter().filter(|i| i.status == "pending")).map(|i| (Some(i.id.clone()), i.text.clone())).collect();
        given.extend(texts.into_iter().map(|t| (None, t)));
        set_locked(hub, &info, agent, given).await?
    };
    if start { control(hub, agent, Action::Start).await } else { Ok(stack) }
}

/// Starts, pauses, skips, clears or brings back the agent's stack.
pub async fn control(hub: &Hub, agent: &str, action: Action) -> R<Stack> {
    let info = known(hub, agent)?;
    let _one = CHANGING.lock().await;
    let stack = get(hub, agent).await?;
    if action == Action::Restore && stack.cleared == 0 {
        return Err(Error::Bad("no goals were cleared here to bring back".into()));
    }
    // Leaving a goal under way (or one still set that wasn't met), or pausing it: its harness's own
    // goal goes too, so nothing holds the agent to it.
    let active = stack.items.iter().any(|i| i.status == "active");
    if action == Action::Clear || (matches!(action, Action::Skip | Action::Pause) && active) {
        clear_in_agent(hub, &info, &stack.items).await;
    }
    // What the agent last said of its goal: a verdict from before you resume doesn't count.
    let said_at = info.objective.as_ref().map_or(0, |o| o.at);
    let agent_s = agent.to_owned();
    let running = hub
        .store
        .call(move |c| {
            let now = now_ms();
            let tx = c.unchecked_transaction()?;
            match action {
                Action::Start => {
                    // The goal under way goes to the agent again at once (its last send forgotten:
                    // the pause since it started is long over), reminders starting over.
                    tx.execute(
                        "UPDATE goal_items SET reminded_at = 0, reminders = 0, busy_at = 0, prior_at = MAX(prior_at, ?2) WHERE agent = ?1 AND status = 'active'",
                        params![agent_s, said_at],
                    )?;
                    let left = tx.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent_s], |r| r.get::<_, i64>(0))?;
                    touch(&tx, &agent_s, Some(if left > 0 { "running" } else { "done" }), None)?;
                }
                Action::Pause => {
                    touch(&tx, &agent_s, Some("paused"), Some(PAUSED_BY_YOU))?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Skip => {
                    tx.execute("UPDATE goal_items SET status = 'skipped', ended_at = ?2, reason = 'Skipped by you.' WHERE agent = ?1 AND status = 'active'", params![agent_s, now])?;
                    touch(&tx, &agent_s, None, None)?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Clear => {
                    // Kept as they were, to bring back (the last clear's; clearing none keeps those).
                    let items = load(&tx, &agent_s)?.items;
                    touch(&tx, &agent_s, Some("paused"), None)?;
                    if !items.is_empty() {
                        tx.execute("UPDATE goal_stacks SET cleared = ?2 WHERE agent = ?1", params![agent_s, serde_json::to_string(&items)?])?;
                    }
                    tx.execute("DELETE FROM goal_items WHERE agent = ?1", params![agent_s])?;
                    tx.execute("UPDATE goal_stacks SET dropped_at = ?2 WHERE agent = ?1", params![agent_s, now])?;
                }
                Action::Restore => restore(&tx, &agent_s, said_at)?,
            }
            let state: String = tx.query_row("SELECT state FROM goal_stacks WHERE agent = ?1", params![agent_s], |r| r.get(0))?;
            tx.commit()?;
            Ok(state == "running")
        })
        .await
        .map_err(|e| if e.to_string().contains("at most") { Error::Bad(e.to_string()) } else { Error::Other(e) })?;
    // You started or resumed it, skipped its goal or brought its goals back: what's next goes to
    // the agent now, whatever it's doing (behind a turn under way, by its harness's queue).
    if running && matches!(action, Action::Start | Action::Skip | Action::Restore) {
        push(hub, agent).await;
    }
    get(hub, agent).await
}

/// Brings back the goals cleared last (`merged` says where) and runs the stack.
fn restore(c: &rusqlite::Connection, agent: &str, said_at: i64) -> anyhow::Result<()> {
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
            "INSERT INTO goal_items (id, agent, pos, text, status, reason, how, prior_at, started_at, ended_at, reminders, reminded_at, busy_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![i.id, agent, i64::try_from(pos).unwrap_or(i64::MAX), i.text, i.status, i.reason, i.how, i.prior_at, i.started_at, i.ended_at, i.reminders, i.reminded_at, i.busy_at],
        )?;
    }
    let left = c.query_row("SELECT COUNT(*) FROM goal_items WHERE agent = ?1 AND status IN ('pending', 'active')", params![agent], |r| r.get::<_, i64>(0))?;
    touch(c, agent, Some(if left > 0 { "running" } else { "done" }), None)?;
    c.execute("UPDATE goal_stacks SET cleared = NULL WHERE agent = ?1", params![agent])?;
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
/// since the change it follows stands.
async fn push(hub: &Hub, agent: &str) {
    if let Err(e) = step(hub, agent, true).await {
        tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
    }
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
    loop {
        if !hub.shutdown.sleep(Duration::from_millis(1500)).await {
            return Ok(());
        }
        for info in hub.harness.list() {
            let _one = CHANGING.lock().await;
            if let Err(e) = adopt(&hub, &info).await {
                tracing::warn!(agent = %info.id, error = %format!("{e:#}"), "goal stack");
            }
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
        for agent in &running {
            let _one = CHANGING.lock().await;
            if let Err(e) = step(&hub, agent, false).await {
                tracing::warn!(%agent, error = %format!("{e:#}"), "goal stack");
            }
        }
        // The agents no goal stack drives: kept going when they stop part-way (`keep`).
        let now = now_ms();
        let all = hub.harness.list();
        for info in all.iter().filter(|i| !running.contains(&i.id)) {
            keep::tick(&hub, info, now, crate::harness::told::children_working(&all, &info.id)).await;
        }
        keep::retain(&all.iter().map(|i| i.id.clone()).collect::<Vec<_>>());
        // Parents told of their children that settled, so none has to wait for them.
        crate::harness::told::tick(&hub, &all, now, &running).await;
        // The paused ones that go on on another machine at a limit (`harness::elsewhere`).
        crate::harness::elsewhere::tick(&hub, &all, now);
    }
}

/// A goal set in the agent that its stack doesn't know (`/goal` typed in its composer, or its
/// harness's own goal set some other way), pursued or given up on for now but still set, joins the
/// stack as the goal under way,
/// so kiln keeps it like the others; one the stack had under way goes back to the front of what's
/// next. The stack runs.
async fn adopt(hub: &Hub, info: &Info) -> anyhow::Result<()> {
    let Some(o) = info.objective.as_ref().filter(|o| matches!(o.status.as_str(), "active" | "unmet")) else { return Ok(()) };
    let Ok(text) = clean(&o.text) else { return Ok(()) };
    let (agent, how, at) = (info.id.clone(), how(hub, &info.id), o.at);
    hub.store
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
            let now = now_ms();
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
            touch(&tx, &agent, Some("running"), None)?;
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
        // Since it was last sent (never, for a fresh one: at once).
        if now.saturating_sub(item.reminded_at) < gap(item.reminders) {
            return Next::Wait;
        }
        let how = item.how.clone().unwrap_or_else(|| "message".into());
        let worked = item.busy_at > 0 && info.last_activity.saturating_sub(item.busy_at) >= WORKED_MS;
        return Next::Remind { id, text: item.text.clone(), how, worked, stopped: info.status == Status::Exited, fresh };
    }
    match stack.items.iter().find(|i| i.status == "pending") {
        None => Next::Done,
        Some(i) if free => Next::Start { id: i.id.clone(), text: i.text.clone() },
        Some(_) => Next::Wait,
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

/// One step of a running stack (`decide`); `push`: yours, which goes now.
async fn step(hub: &Hub, agent: &str, push: bool) -> anyhow::Result<()> {
    let stack = get(hub, agent).await?;
    // Paused or done since it was found running: left as it is.
    if stack.state != "running" {
        return Ok(());
    }
    let all = hub.harness.list();
    let children = crate::harness::told::children_working(&all, agent);
    let info = all.into_iter().find(|i| i.id == agent);
    let now = now_ms();
    let next = decide(&stack, info.as_ref(), now, children, push);
    let a = agent.to_owned();
    // What its harness last said of the goal it pursues: a verdict on what's sent now must be newer.
    let prior_at = info.as_ref().and_then(|i| i.objective.as_ref()).map_or(0, |o| o.at);
    match next {
        Next::Wait => {}
        Next::Busy { id } => {
            hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET busy_at = ?2 WHERE id = ?1", params![id, now]).map(|_| ())?)).await?;
        }
        Next::End { id, status, reason } => {
            hub.store
                .call(move |c| {
                    c.execute("UPDATE goal_items SET status = ?2, reason = ?3, ended_at = ?4 WHERE id = ?1", params![id, status, reason, now])?;
                    touch(c, &a, None, None)
                })
                .await?;
        }
        Next::Done => {
            hub.store.call(move |c| touch(c, &a, Some("done"), None)).await?;
        }
        Next::Start { id, text } => {
            let how = how(hub, agent);
            let stopped = info.as_ref().is_some_and(|i| i.status == Status::Exited);
            match hub.harness.command(hub, agent, first_command(how, &text, stopped)).await {
                Sent::Ok => {
                    hub.store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET status = 'active', how = ?2, prior_at = ?3, started_at = ?4, reminded_at = ?4, reminders = 0, busy_at = 0 WHERE id = ?1",
                                params![id, how, prior_at, now],
                            )?;
                            touch(c, &a, None, None)
                        })
                        .await?;
                }
                // Not known right now, or its machine is away: the goal goes when it's back.
                Sent::NoSession | Sent::Unreachable => {}
            }
        }
        Next::Remind { id, text, how, worked, stopped, fresh } => {
            let c = if fresh { first_command(&how, &text, stopped) } else { reminder_command(&how, &text, stopped) };
            match hub.harness.command(hub, agent, c).await {
                Sent::Ok => {
                    tracing::info!(%agent, worked, stopped, fresh, "goal sent again");
                    hub.store
                        .call(move |c| {
                            c.execute(
                                "UPDATE goal_items SET reminded_at = ?2, reminders = CASE WHEN ?5 THEN 0 WHEN ?3 THEN 1 ELSE reminders + 1 END, prior_at = ?4, busy_at = 0 WHERE id = ?1",
                                params![id, now, worked, prior_at, fresh],
                            )?;
                            touch(c, &a, None, None)
                        })
                        .await?;
                }
                // Not known right now, or its machine is away: tried again after a pause, as a
                // reminder that brought no work.
                Sent::NoSession | Sent::Unreachable => {
                    hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET reminded_at = ?2, reminders = reminders + 1 WHERE id = ?1", params![id, now]).map(|_| ())?)).await?;
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
            let hub = crate::hub::tests::scratch_hub_with(|c| c.terminal.enabled = true).await?;
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
}
