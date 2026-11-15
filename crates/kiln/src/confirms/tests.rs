//! Managing agents from chat (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.6, ROOT's rulings of
//! 2026-11-14), through the operations as an agent's MCP tools, code mode and the console call them
//! (`crate::ops::call`, every surface's one way in): a person's turn, what an agent in it does at once,
//! what waits on the person's click, and what the click is held to.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::authz::Caller;
use crate::harness::registry::Location;
use crate::hub::Hub;
use crate::ops::{Cx, Fail, call};

/// A session on runner `box`, the agent `parent` started (none: a person did), acting for `person`.
async fn session(hub: &Hub, id: &str, parent: Option<&str>, person: &str, last_turn_at: Option<i64>) -> anyhow::Result<()> {
    let info = crate::harness::session::Info {
        id: id.into(),
        kind: "claude".into(),
        name: id.into(),
        target: "agent:box".into(),
        parent: parent.map(str::to_owned),
        last_turn: last_turn_at.map(|at| crate::harness::model::TurnEnd { at, how: "done".into(), ..Default::default() }),
        ..Default::default()
    };
    hub.harness.ingest(&Location::Machine("box".into()), id, crate::harness::core::Update::Open { info: Box::new(info), since: None }).await;
    hub.store.kv_set(crate::mcp::opener_key(id), person.to_owned()).await?;
    Ok(())
}

/// A hub with runner `box` and the owner's agents on it: `chat` (a person talks to it), `kid` (which
/// `chat` started), `other` (another of the owner's, which it didn't); and `bos`, user `bo`'s.
async fn hub() -> anyhow::Result<Arc<Hub>> {
    let hub = crate::hub::tests::scratch_hub().await?;
    hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
    session(&hub, "chat", None, "owner", None).await?;
    session(&hub, "kid", Some("chat"), "owner", None).await?;
    session(&hub, "other", None, "owner", None).await?;
    session(&hub, "bos", None, "user:bo", None).await?;
    Ok(hub)
}

/// Agent `id`'s caller as kiln makes it for its calls (its MCP server, code mode, its shell).
async fn agent(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Cx> {
    Ok(Cx { hub: Arc::clone(hub), caller: crate::mcp::session_caller(hub, id, Some("box")).await.map_err(anyhow::Error::msg)? })
}

fn person(hub: &Arc<Hub>) -> Cx {
    Cx { hub: Arc::clone(hub), caller: Caller::owner() }
}

/// The owner's message to `agent`, as kiln records a person's command by every path.
async fn says(hub: &Hub, agent: &str) {
    crate::harness::attention_hooks::record(hub, &Caller::owner(), agent, "message", &crate::util::new_id(), crate::util::now_ms()).await;
}

fn code(r: &Result<Value, Fail>) -> u16 {
    r.as_ref().err().map_or(200, |(c, _)| c.as_u16())
}

/// The owner's asks waiting on their click.
async fn asked(hub: &Arc<Hub>) -> anyhow::Result<Vec<Value>> {
    let l = call(person(hub), "confirms.list", json!({})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
    Ok(l.get("asked").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// ROOT's reach rule and correction 1. A person's message makes the turn theirs; an agent's doesn't.
/// In the owner's chat turn, the agent ends its own child at once; interrupting or ending another of the
/// owner's agents waits on their click (428, an ask the owner sees). The click confirms the very call:
/// the same call then runs once, as the owner (the audit line says whose click); one with other
/// arguments, or the same one again, asks anew; an agent can't click. Taking the gate out of
/// `ops::call` lets `chat` interrupt `other` at once; letting a confirmation run twice lets the reused
/// call through.
#[test]
fn p5_6_in_a_persons_chat_its_own_child_runs_at_once_and_another_of_theirs_waits_for_the_click() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        crate::harness::attention_hooks::record(&hub, &agent(&hub, "kid").await?.caller, "chat", "message", "k", crate::util::now_ms()).await;
        anyhow::ensure!(crate::confirms::turn_of(&hub, "chat").await.is_none(), "an agent's message is no person's turn");
        says(&hub, "chat").await;
        anyhow::ensure!(crate::confirms::turn_of(&hub, "chat").await.as_deref() == Some("owner"), "the owner's message makes it theirs");
        let chat = agent(&hub, "chat").await?;
        anyhow::ensure!(chat.caller.turn() == Some("owner"), "its caller carries the turn's person");

        let ran = call(chat.clone(), "agents.stop", json!({"agent": "kid"})).await;
        anyhow::ensure!(code(&ran) == 200, "its own child, at once: {ran:?}");
        let interrupt = call(chat.clone(), "agents.interrupt", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&interrupt) == 428 && interrupt.as_ref().err().is_some_and(|(_, m)| m.contains("waiting on owner's click")), "{interrupt:?}");
        let stop = call(chat.clone(), "agents.stop", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&stop) == 428, "{stop:?}");
        let waiting = asked(&hub).await?;
        let id = waiting.iter().find(|a| a.get("op") == Some(&json!("agents.stop"))).and_then(|a| a.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        anyhow::ensure!(waiting.len() == 2 && !id.is_empty(), "both wait on the owner: {waiting:?}");

        let by_agent = call(chat.clone(), "confirms.answer", json!({"id": id, "yes": true})).await;
        anyhow::ensure!(code(&by_agent) == 403, "an agent can't click: {by_agent:?}");
        let clicked = call(person(&hub), "confirms.answer", json!({"id": id, "yes": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
        anyhow::ensure!(clicked.get("state") == Some(&json!("confirmed")), "{clicked}");
        let other_args = call(chat.clone(), "agents.stop", json!({"agent": "other", "children": true})).await;
        anyhow::ensure!(code(&other_args) == 428, "other arguments ask anew: {other_args:?}");
        let confirmed = call(chat.clone(), "agents.stop", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&confirmed) == 200, "the confirmed call runs: {confirmed:?}");
        let again = call(chat.clone(), "agents.stop", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&again) == 428, "once: {again:?}");
        let audit: Vec<(String, String)> = hub
            .store
            .call(|c| {
                let mut st = c.prepare("SELECT principal, detail FROM audit2 WHERE detail LIKE '%confirmed by owner''s click%'")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        anyhow::ensure!(audit.len() == 1 && audit.iter().all(|(p, d)| p == "admin" && d.contains("asked by agent:chat")), "run as the owner, saying whose click: {audit:?}");
        Ok(())
    })
}

/// ROOT's reach rule: an agent serving no person's turn keeps today's rule (another of its person's
/// agents is refused, nothing is asked); and a chat of the owner's can't touch user bo's agent, at once
/// or by asking. Taking the turn check out of `gate` asks for the autonomous agent.
#[test]
fn p5_6_an_autonomous_agent_cant_and_a_persons_chat_cant_reach_anothers_agents() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        let alone = agent(&hub, "chat").await?;
        let refused = call(alone, "agents.stop", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&refused) == 403 && refused.as_ref().err().is_some_and(|(_, m)| m.contains("isn't serving a person's chat turn")), "{refused:?}");
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let theirs = call(chat, "agents.stop", json!({"agent": "bos"})).await;
        anyhow::ensure!(code(&theirs) == 403, "bo's agent: {theirs:?}");
        anyhow::ensure!(asked(&hub).await?.is_empty(), "nothing asked of anyone");
        Ok(())
    })
}

/// ROOT's confirm ruling and correction 2, on a destructive op (an agent definition's delete) and on
/// each way in: without the click it waits; the click holds to its target's version, so a definition
/// changed since refuses it (and the next call asks anew); a click after the agent's turn ended still
/// confirms it; the person's next message drops what was asked. CI's operations pass the same gate
/// (ci.runs.cancel waits too). Taking the version out of the confirmation lets the changed one through.
#[test]
fn p5_6_a_destructive_op_waits_for_the_click_held_to_its_target_until_the_persons_next_message() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        let owner = person(&hub);
        let fail = |(c, m): Fail| anyhow::anyhow!("{c}: {m}");
        let made = call(owner.clone(), "agentdefs.create", json!({"name": "reviewer", "harness": "claude"})).await.map_err(fail)?;
        let def = made.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;

        let cancel = call(chat.clone(), "ci.runs.cancel", json!({"run": "r1"})).await;
        anyhow::ensure!(code(&cancel) == 428, "CI's destructive ops wait too: {cancel:?}");
        let delete = call(chat.clone(), "agentdefs.delete", json!({"id": def})).await;
        anyhow::ensure!(code(&delete) == 428, "{delete:?}");
        let id_of = |op: &str, list: &[Value]| list.iter().find(|a| a.get("op") == Some(&json!(op))).and_then(|a| a.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let id = id_of("agentdefs.delete", &asked(&hub).await?);
        call(owner.clone(), "confirms.answer", json!({"id": id, "yes": true})).await.map_err(fail)?;
        call(owner.clone(), "agentdefs.update", json!({"id": def, "instructions": "Review it twice."})).await.map_err(fail)?;
        let stale = call(chat.clone(), "agentdefs.delete", json!({"id": def})).await;
        anyhow::ensure!(code(&stale) == 409, "changed since the click: {stale:?}");
        anyhow::ensure!(call(owner.clone(), "agentdefs.get", json!({"id": def})).await.is_ok(), "kept");

        // Asked anew, its turn ends, then the click: still confirmed, and it runs.
        let anew = call(chat.clone(), "agentdefs.delete", json!({"id": def})).await;
        anyhow::ensure!(code(&anew) == 428, "{anew:?}");
        let id = id_of("agentdefs.delete", &asked(&hub).await?);
        session(&hub, "chat", None, "owner", Some(crate::util::now_ms() + 1_000)).await?;
        let after = agent(&hub, "chat").await?;
        anyhow::ensure!(after.caller.turn().is_none(), "its turn is over");
        call(owner.clone(), "confirms.answer", json!({"id": id, "yes": true})).await.map_err(fail)?;
        let deleted = call(after, "agentdefs.delete", json!({"id": def})).await;
        anyhow::ensure!(code(&deleted) == 200 && code(&call(owner.clone(), "agentdefs.get", json!({"id": def})).await) == 404, "a click after the turn: {deleted:?}");

        // The person's next message drops what was asked before it.
        session(&hub, "chat", None, "owner", None).await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        anyhow::ensure!(code(&call(chat, "agentdefs.create", json!({"name": "fixer", "harness": "codex"})).await) == 428, "a definition's create from chat waits too");
        let id = id_of("agentdefs.create", &asked(&hub).await?);
        says(&hub, "chat").await;
        let late = call(owner, "confirms.answer", json!({"id": id, "yes": true})).await;
        anyhow::ensure!(code(&late) == 409 && late.as_ref().err().is_some_and(|(_, m)| m.contains("dropped")), "{late:?}");
        Ok(())
    })
}
