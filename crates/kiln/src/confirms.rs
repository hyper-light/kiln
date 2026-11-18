//! A person's click before an agent's act on what they own (kiln plan 01a1dd2e8618733a87f2793aab1322a8
//! P5.6, ROOT's rulings of 2026-11-14): managing agents from chat without handing an agent, or the
//! untrusted text it reads, a person's authority.
//!
//! - **The turn's person.** A person's message to an agent (or goals they give it) makes the turn it
//!   serves theirs ([`spoke`], from `harness::attention_hooks`, where kiln records a person's command
//!   by every path); it ends with that turn ([`turn_of`]). An agent's [`crate::authz::Caller`] carries
//!   that person ([`crate::authz::Caller::turn`]) only for an agent that acts for them.
//! - **What runs at once.** Reads; an agent's acts on itself and on what it started (its children and
//!   theirs, the plan runs they ran); messages and questions to its person's other agents, as their own
//!   rules have them; and everything else that changes nothing an agent can't already change.
//! - **What waits for the click** ([`gate`], at the top of `crate::ops::call`, so every operation is
//!   held to it, CI's, the hosts', experiments', Ship's and deployments' too): an op the registry marks
//!   destructive; a change to another of its person's agents (interrupting, ending, resuming one it
//!   didn't start); and any change to their agent definitions. Asked in a person's turn, kiln keeps the
//!   ask (the op, its exact arguments, its target's version then) and answers 428: it waits on the
//!   person. Outside a person's turn, there's no one to click: refused, and today's rules stand.
//! - **An agent's landing** (`agents.land` with a plan's manifest, of its own workspace or one of an agent it
//!   started: docs/agentic.md W6, the person's rule of 2026-11-18): its mode decides (`landing_click`): it waits for
//!   the click where its mode asks before it edits, held to the manifest, and runs at once otherwise, the op writing
//!   where the mode edits unasked and refusing where it only plans.
//! - **The click** (`confirms.answer`, a person's alone: an agent's call is refused, and nothing an
//!   agent or a page it reads writes in chat can stand for it). Confirmed, the agent's same call (the
//!   same op and arguments, its target unchanged) runs once, as the person whose click it was: their
//!   grants, their reach over their own agents. Another call, a second one, one whose target changed,
//!   or one after the person's next message to that agent, doesn't. The confirmation is kept by kiln
//!   and never handed to the agent, so there's nothing to copy, guess or replay.

use http::{Method, StatusCode};
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::ops::{Cx, Fail, Input, Op, Out, path};

/// Where kiln keeps whose turn agent `agent` serves: `{person, at}`, the person's last message to it.
fn turn_key(agent: &str) -> String {
    format!("chat/turn/{agent}")
}

/// Person `person` sent agent `agent` a message (or goals) at `at` (Unix ms): the turn it serves is theirs
/// from now, and what it asked of them before is dropped, confirmed or not (a confirmation lasts until the
/// person's next message).
pub async fn spoke(hub: &Hub, agent: &str, person: &str, at: i64) {
    if let Err(e) = hub.store.kv_set(turn_key(agent), json!({"person": person, "at": at}).to_string()).await {
        tracing::warn!(%agent, error = %format!("{e:#}"), "whose turn an agent serves wasn't kept");
    }
    let (a, p, now) = (agent.to_owned(), person.to_owned(), crate::util::now_ms());
    let dropped = hub
        .store
        .call(move |c| {
            c.execute("UPDATE confirms SET state = 'dropped', answered_at = ?3 WHERE agent = ?1 AND person = ?2 AND state IN ('asked', 'confirmed')", params![a, p, now])?;
            Ok(())
        })
        .await;
    if let Err(e) = dropped {
        tracing::warn!(%agent, error = %format!("{e:#}"), "an agent's asks of its person weren't dropped at their next message");
    }
}

/// The person whose chat turn agent `agent` serves now: the last to message it, since it last ended a
/// turn, where it acts for them. None: it works on its own (or for someone kiln can't name).
pub async fn turn_of(hub: &Hub, agent: &str) -> Option<String> {
    let kept: Value = serde_json::from_str(&hub.store.kv_get(turn_key(agent)).await.ok()??).ok()?;
    let (person, at) = (kept.get("person")?.as_str()?.to_owned(), kept.get("at")?.as_i64()?);
    let info = hub.harness.info(agent)?;
    if info.last_turn.as_ref().is_some_and(|t| t.at > at) {
        return None;
    }
    (crate::mcp::acts_for(hub, agent).await.as_deref() == Some(person.as_str())).then_some(person)
}

/// What an agent's act is on, as the gate tells what it may do at once.
enum Target {
    /// One agent session (`agent`), or the agent that ran a plan run (`run`).
    Session(String),
    /// A person's plan run that no agent ran.
    PersonsRun,
    /// An agent definition: by id, or none for a new one.
    Def(Option<String>),
    /// Nothing the gate reads apart from the op and its arguments.
    Op,
}

async fn target_of(cx: &Cx, name: &str, input: &Value) -> Target {
    if name.starts_with("agentdefs.") {
        return Target::Def(input.get("id").and_then(Value::as_str).map(str::to_owned));
    }
    if name.starts_with("plans.runs.")
        && let Some(run) = input.get("run").and_then(Value::as_str)
    {
        return match crate::plans::agents::view(&cx.hub, run).await.ok().and_then(|r| r.config.parent) {
            Some(parent) => Target::Session(parent),
            None => Target::PersonsRun,
        };
    }
    match input.get("agent").and_then(Value::as_str).map(|a| crate::harness::ops::known(cx, a)) {
        Some(Ok(info)) => Target::Session(info.id),
        _ => Target::Op,
    }
}

/// Agent definitions' operations that change nothing kept: a check, and a run (an agent started as
/// agents.start starts one).
const DEFS_UNCHANGED: &[&str] = &["agentdefs.check", "agentdefs.run"];

/// Operations on another agent that its own rules already hold an agent to: a message (steered or
/// queued from it, `lineage::may_message`) and a question about its code.
const TO_PEERS: &[&str] = &["agents.send", "agents.about"];

/// What needs its person's click, of an agent's call of op `name` (`destructive` as the registry has
/// it) with `input`, and what its target is now (its version, for the confirmation to hold to); none
/// when it runs at once.
async fn needs_click(cx: &Cx, me: &str, name: &str, read_only: bool, destructive: bool, input: &Value) -> Option<(String, String)> {
    if read_only {
        return None;
    }
    if name == "agents.land"
        && let Some(decided) = landing_click(cx, me, input)
    {
        return decided;
    }
    match target_of(cx, name, input).await {
        Target::Def(_) if DEFS_UNCHANGED.contains(&name) => None,
        Target::Def(id) => {
            let version = match id {
                Some(id) => {
                    let person = crate::mcp::acts_for(&cx.hub, me).await.unwrap_or_default();
                    match crate::agentdefs::get(&cx.hub, &crate::agentdefs::Who { person, all: false }, &id).await {
                        Ok(d) => format!("v{}", d.version),
                        // Another's, or none: the op's own answer, after the click as before.
                        Err(_) => "none".to_owned(),
                    }
                }
                None => String::new(),
            };
            Some((what_def(name, input), version))
        }
        Target::Session(t) => {
            let list = cx.hub.harness.list();
            if t == me || crate::harness::lineage::descends(&list, &t, me) || TO_PEERS.contains(&name) {
                return None;
            }
            // Another person's agent isn't the asker's person's to confirm: the op's own rules answer.
            let theirs = crate::mcp::acts_for(&cx.hub, &t).await;
            if theirs.is_none() || theirs != crate::mcp::acts_for(&cx.hub, me).await {
                return None;
            }
            let shown = list.iter().find(|i| i.id == t).map_or_else(|| crate::harness::lineage::short(&t).to_owned(), |i| format!("{} ({})", i.name, crate::harness::lineage::short(&t)));
            Some((format!("{name} on {shown}"), session_version(&cx.hub, &t)))
        }
        Target::PersonsRun | Target::Op if destructive => Some((format!("{name} {}", canonical(input)), String::new())),
        Target::PersonsRun | Target::Op => None,
    }
}

/// An agent's landing of its own workspace, or of the workspace of an agent it started (docs/agentic.md W6; the
/// person's rule of 2026-11-18, kiln goal 01a1ec34599974448aed2672bafaf9a2): the modes decide, not the registry's mark
/// (`crate::workspace::landing_policy`). It waits on the click where the lower of the two modes asks before it edits,
/// held to the plan's manifest in its arguments, which binds what is written: no version beside it, since the asking
/// agent's own session moves on while it waits, and a volume that changed meanwhile lands nothing (the op answers
/// `changed`). A plan asked for (no manifest), a mode that edits unasked, one that only plans, and a workspace that
/// isn't a volume of its own run at once, the op answering for each. None: another agent's workspace, which the gate's
/// rules for acts on others answer.
fn landing_click(cx: &Cx, me: &str, input: &Value) -> Option<Option<(String, String)>> {
    let target = input.get("agent").and_then(Value::as_str).and_then(|a| crate::harness::ops::known(cx, a).ok())?;
    if target.id != me && !crate::harness::lineage::descends(&cx.hub.harness.list(), &target.id, me) {
        return None;
    }
    let manifest = input.get("manifest").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty());
    let workspace = target.workspace.as_ref().filter(|w| w.made.is_some() && w.owner.as_deref().is_none_or(|o| cx.hub.harness.info(o).is_none()));
    let asker = cx.hub.harness.info(me);
    let (Some(manifest), Some(workspace), Some(asker)) = (manifest, workspace, asker) else { return Some(None) };
    if crate::workspace::landing_policy(&asker, &target) != crate::workspace::WritePolicy::Approve {
        return Some(None);
    }
    let over = workspace.over.as_deref().unwrap_or_default();
    let plan = manifest.get(..12).unwrap_or(manifest);
    Some(Some((format!("agents.land on {} ({}): its work written to {over}, exactly as plan {plan} says", target.name, crate::harness::lineage::short(&target.id)), String::new())))
}

fn what_def(name: &str, input: &Value) -> String {
    let id = input.get("id").and_then(Value::as_str).unwrap_or_default();
    match name {
        "agentdefs.delete" => format!("delete agent definition {id}"),
        "agentdefs.create" => format!("create agent definition {}", input.get("name").and_then(Value::as_str).unwrap_or_default()),
        _ => format!("{name} {}", canonical(input)),
    }
}

/// A session's version, as a confirmation of an act on it holds to: its status, and its newest event.
fn session_version(hub: &Hub, id: &str) -> String {
    let Some(view) = hub.harness.view_held(id) else { return "gone".to_owned() };
    let status = serde_json::to_string(&view.info.status).unwrap_or_default();
    let seq = view.events.metas().next_back().and_then(|m| m.seq()).unwrap_or_default();
    format!("{status}:{seq}")
}

/// `v` as one text whatever order its keys came in: what a confirmation holds the arguments to.
fn canonical(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), m.get(k).map(sorted).unwrap_or(Value::Null))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(v).to_string()
}

/// What runs an op: who calls it, and what its audit row says of the turn and the click.
pub struct Pass {
    pub cx: Cx,
    pub note: Option<String>,
}

/// The gate an agent's call of op `name` with `input` passes before anything of it runs (see the
/// module's words): at once (as it is, its person's turn noted), once as the person whose click
/// confirmed it, or not yet: 428 while it waits on their click, refused where no person's turn is there
/// to ask in.
pub async fn gate(cx: Cx, name: &str, input: &Value) -> Result<Pass, Fail> {
    let Some(me) = cx.caller.agent.clone() else { return Ok(Pass { cx, note: None }) };
    let turn = cx.caller.turn().map(str::to_owned);
    // The turn's person goes on the audit row as its caller makes it (`crate::audit::request`).
    let Some(info) = crate::ops::find(&cx.hub, |n| n == name) else { return Ok(Pass { cx, note: None }) };
    let Some((says, version)) = needs_click(&cx, &me, name, info.read_only, info.destructive, input).await else { return Ok(Pass { cx, note: None }) };
    let input = canonical(input);
    // Confirmed already: run once, as the person whose click it was.
    if let Some((id, person)) = confirmed(&cx.hub, &me, name, &input).await? {
        return redeem(cx, &id, &person, &me, &version).await;
    }
    let Some(person) = turn else {
        return Err((
            StatusCode::FORBIDDEN,
            format!("{says} needs its person's click, and this agent isn't serving a person's chat turn: there's no one to ask. Ask them in their chat, or they do it themselves"),
        ));
    };
    let id = ask(&cx.hub, &person, &me, name, &input, &version, &says).await?;
    Err((
        StatusCode::PRECONDITION_REQUIRED,
        format!("waiting on {person}'s click: kiln asked them in this chat to confirm {says} (confirm {id}). Once they do, kiln tells you; then make this same call again, with the same arguments, and it runs once, as theirs"),
    ))
}

/// The confirmed ask agent `agent` made of op `name` with exactly `input`: its id and person.
async fn confirmed(hub: &Hub, agent: &str, name: &str, input: &str) -> Result<Option<(String, String)>, Fail> {
    let (a, n, i) = (agent.to_owned(), name.to_owned(), input.to_owned());
    hub.store
        .read(move |c| Ok(c.query_row("SELECT id, person FROM confirms WHERE agent = ?1 AND op = ?2 AND input = ?3 AND state = 'confirmed' ORDER BY asked_at DESC LIMIT 1", params![a, n, i], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
}

/// Ask `id` used: once, and only while its target is as it was when it was asked.
async fn redeem(cx: Cx, id: &str, person: &str, agent: &str, now_is: &str) -> Result<Pass, Fail> {
    let (i, v) = (id.to_owned(), now_is.to_owned());
    let at = crate::util::now_ms();
    let used: Result<bool, Fail> = cx
        .hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let then: Option<String> = tx.query_row("SELECT version FROM confirms WHERE id = ?1 AND state = 'confirmed'", params![i], |r| r.get(0)).optional()?;
            if then.as_deref() != Some(v.as_str()) {
                // Confirmed for its target as it was: spent, so the next such call asks anew.
                tx.execute("UPDATE confirms SET state = 'changed', used_at = ?2 WHERE id = ?1 AND state = 'confirmed'", params![i, at])?;
                tx.commit()?;
                return Ok(false);
            }
            let n = tx.execute("UPDATE confirms SET state = 'used', used_at = ?2 WHERE id = ?1 AND state = 'confirmed'", params![i, at])?;
            tx.commit()?;
            Ok(n == 1)
        })
        .await
        .map_err(|e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
    if !used? {
        return Err((StatusCode::CONFLICT, format!("what {person} confirmed (confirm {id}) has changed since, or was taken already: ask them again")));
    }
    let Some(caller) = crate::authz::gate::person_for(&cx.hub, person).await else {
        return Err((StatusCode::FORBIDDEN, format!("{person} can't sign in here any more, so their click can't be acted on")));
    };
    Ok(Pass { cx: Cx { hub: cx.hub, caller }, note: Some(format!("asked by agent:{agent}, confirmed by {person}'s click (confirm {id})")) })
}

/// A new ask of `person`'s click: its id.
async fn ask(hub: &Hub, person: &str, agent: &str, name: &str, input: &str, version: &str, says: &str) -> Result<String, Fail> {
    let id = crate::util::new_id();
    let (i, p, a, n, inp, v, s, at) = (id.clone(), person.to_owned(), agent.to_owned(), name.to_owned(), input.to_owned(), version.to_owned(), says.to_owned(), crate::util::now_ms());
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO confirms (id, person, agent, op, input, version, says, state, asked_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'asked', ?8)", params![i, p, a, n, inp, v, s, at])?;
            Ok(())
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(id)
}

/// One ask, as a person's chat shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Asked {
    pub id: String,
    pub agent: String,
    pub op: String,
    pub says: String,
    pub state: String,
    pub asked_at: i64,
}

fn person_of(cx: &Cx) -> Result<String, Fail> {
    if cx.caller.agent.is_some() {
        return Err((StatusCode::FORBIDDEN, "a confirmation is its person's click: an agent can't give or read one".to_owned()));
    }
    crate::harness::attention::person(&cx.caller).ok_or_else(|| (StatusCode::FORBIDDEN, "a confirmation is a person's, and this caller is no person here".to_owned()))
}

/// The caller's asks waiting on their click, newest first; with `agent`, that agent's alone.
async fn list(cx: Cx, v: Value) -> Out {
    let person = person_of(&cx)?;
    let agent = v.get("agent").and_then(Value::as_str).map(str::to_owned);
    let rows: Vec<Asked> = cx
        .hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT id, agent, op, says, state, asked_at FROM confirms WHERE person = ?1 AND state = 'asked' AND (?2 IS NULL OR agent = ?2) ORDER BY asked_at DESC")?;
            let rows = st.query_map(params![person, agent], |r| Ok(Asked { id: r.get(0)?, agent: r.get(1)?, op: r.get(2)?, says: r.get(3)?, state: r.get(4)?, asked_at: r.get(5)? }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(json!({"asked": rows}))
}

/// The person's click on ask `id`: `yes` confirms it (the agent's same call then runs once, as theirs),
/// else it's declined. Its agent is told either way.
async fn answer(cx: Cx, v: Value) -> Out {
    let person = person_of(&cx)?;
    let id = crate::ops::text(&v, "id")?;
    let yes = v.get("yes").and_then(Value::as_bool).ok_or_else(|| (StatusCode::BAD_REQUEST, "`yes`: true to confirm, false to decline".to_owned()))?;
    let (i, p, at) = (id.clone(), person.clone(), crate::util::now_ms());
    let answered: Option<(String, String, String, String)> = cx
        .hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let row: Option<(String, String, String, String)> = tx.query_row("SELECT agent, op, says, state FROM confirms WHERE id = ?1 AND person = ?2", params![i, p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
            if row.as_ref().is_some_and(|r| r.3 == "asked") {
                tx.execute("UPDATE confirms SET state = ?2, answered_at = ?3 WHERE id = ?1", params![i, if yes { "confirmed" } else { "declined" }, at])?;
                tx.commit()?;
            }
            Ok(row)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    // Another's is as none.
    let Some((agent, op, says, state)) = answered else { return Err((StatusCode::NOT_FOUND, "no such confirmation".to_owned())) };
    if state != "asked" {
        return Err((StatusCode::CONFLICT, format!("that confirmation is {state} already")));
    }
    let words = if yes { format!("{person} confirmed {says}: make your {op} call again, with the same arguments, and it runs once, as theirs.") } else { format!("{person} declined {says}: don't make that call again.") };
    let told = crate::workspace::follow::deliver(&cx.hub, &agent, words).await;
    cx.hub.store.audit(cx.caller.name.clone(), "confirm.answer".into(), format!("{id} {}: {says} (asked by agent:{agent})", if yes { "confirmed" } else { "declined" })).await;
    Ok(json!({"id": id, "state": if yes { "confirmed" } else { "declined" }, "told": told}))
}

/// The operations, in the order they are listed.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "confirms.list",
            summary: "What your agents asked to do that waits on your click (deleting a definition, ending or changing one of your agents they didn't start, anything kiln marks destructive), newest first; with `agent`, that agent's. A person's: an agent can't read them.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::ConfirmsList>(),
            route: |_| Ok((Method::GET, path(&["api", "confirms"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "confirms.answer",
            summary: "Your click on what an agent asked (`id`, as confirms.list gives it): `yes` confirms it, and the agent's same call (same arguments, its target as it was) then runs once, as yours; else it's declined. Its agent is told either way. A person's: an agent's call is refused.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::ConfirmsAnswer>(),
            route: |v| Ok((Method::POST, path(&["api", "confirms", &crate::ops::text(v, "id")?]))),
            run: |cx, v| Box::pin(answer(cx, v)),
        },
    ]
}

/// What each of these operations takes.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ConfirmsList {
        /// One agent's asks alone.
        pub agent: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct ConfirmsAnswer {
        /// The ask's id.
        pub id: String,
        /// True: confirm it; false: decline it.
        pub yes: bool,
    }
}

#[cfg(test)]
mod tests;
