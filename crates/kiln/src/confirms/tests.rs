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
    hub_with(false).await
}

/// [`hub`], with deployments on (`cd`) or not.
async fn hub_with(cd: bool) -> anyhow::Result<Arc<Hub>> {
    let hub = crate::hub::tests::scratch_hub_with(|c| c.cd.enabled = cd).await?;
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

/// P5.6's slice S1: the Agents page's Remove is an operation (`agents.remove`, its route `DELETE
/// /api/harness/agents/{agent}`; the bare `DELETE /api/harness/sessions/{id}` is gone), held to the same
/// gate as every other: in the owner's chat turn its own child goes at once, another of theirs waits on
/// their click; and the click confirms it, run as theirs and audited as them, the turn's person on the
/// row of what the agent did at once. With deployments on, their destructive operations wait as kiln's
/// own do, and an agent on its own is refused them: the gate is before every dispatch.
#[test]
fn p5_6_s1_remove_is_an_operation_held_to_the_gate_as_deployments_are() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub_with(true).await?;
        let alone = agent(&hub, "chat").await?;
        let cd = call(alone, "cd.apps.delete", json!({"app": "web"})).await;
        anyhow::ensure!(code(&cd) == 403 && cd.as_ref().err().is_some_and(|(_, m)| m.contains("isn't serving a person's chat turn")), "deployments' ops on its own: {cd:?}");
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let cd = call(chat.clone(), "cd.apps.delete", json!({"app": "web"})).await;
        anyhow::ensure!(code(&cd) == 428, "deployments' ops in a person's turn wait: {cd:?}");

        let kid = call(chat.clone(), "agents.remove", json!({"agent": "kid"})).await;
        anyhow::ensure!(code(&kid) == 200 && kid.as_ref().ok().and_then(|v| v.get("removed")) == Some(&json!("kid")), "its own child, at once: {kid:?}");
        let other = call(chat.clone(), "agents.remove", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&other) == 428, "{other:?}");
        let id = asked(&hub).await?.iter().find(|a| a.get("op") == Some(&json!("agents.remove"))).and_then(|a| a.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        call(person(&hub), "confirms.answer", json!({"id": id, "yes": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
        let removed = call(chat, "agents.remove", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&removed) == 200, "confirmed: {removed:?}");
        let rows: Vec<(String, String, String)> = hub
            .store
            .call(|c| {
                let mut st = c.prepare("SELECT principal, target, detail FROM audit2 WHERE action LIKE 'DELETE %' AND target LIKE '/api/harness/agents/%' ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        // The one that waited ran nothing, and wrote no row: the two that ran did.
        anyhow::ensure!(rows.len() == 2, "{rows:?}");
        anyhow::ensure!(rows.iter().any(|(p, _, d)| p.starts_with("agent:chat") && d.contains("in owner's chat turn")), "what it did at once says whose turn: {rows:?}");
        anyhow::ensure!(rows.iter().any(|(p, _, d)| p == "admin" && d.contains("confirmed by owner's click")), "the confirmed one is the owner's: {rows:?}");
        Ok(())
    })
}

/// P5.6's slice S2: the Agents page's controls (rename, keep going), restart and interrupt are
/// operations, held to the gate: in the owner's chat turn its own child's run at once, another of
/// theirs waits on their click.
#[test]
fn p5_6_s2_controls_and_restart_are_operations_held_to_the_gate() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let renamed = call(chat.clone(), "agents.control", json!({"agent": "kid", "control": "rename", "value": "Kid"})).await;
        anyhow::ensure!(code(&renamed) == 200, "agents.control on its own child: {renamed:?}");
        // Past the gate at once; its fixture's machine isn't connected, which the operation itself then says (502).
        for (op, input) in [("agents.restart", json!({"agent": "kid"})), ("agents.interrupt", json!({"agent": "kid"}))] {
            let at_once = call(chat.clone(), op, input).await;
            anyhow::ensure!(!matches!(code(&at_once), 403 | 428), "{op} on its own child goes past the gate: {at_once:?}");
        }
        for (op, input) in [("agents.control", json!({"agent": "other", "control": "keep_going", "value": true})), ("agents.restart", json!({"agent": "other"}))] {
            let waits = call(chat.clone(), op, input).await;
            anyhow::ensure!(code(&waits) == 428, "{op} on another of the owner's: {waits:?}");
        }
        let by_person = call(person(&hub), "agents.control", json!({"agent": "other", "control": "rename", "value": "Other"})).await;
        anyhow::ensure!(code(&by_person) == 200, "the owner's own: {by_person:?}");
        Ok(())
    })
}

/// P5.6's slice S3: the Agents page's Move to… is `agents.move`, held to the gate: in the owner's chat turn a move of
/// its own child goes past it at once (to a machine this hub hasn't, so the move itself refuses it, saying why),
/// another of theirs waits on their click.
#[test]
fn p5_6_s3_move_is_an_operation_held_to_the_gate() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let own = call(chat.clone(), "agents.move", json!({"agent": "kid", "to": "nowhere"})).await;
        anyhow::ensure!(!matches!(code(&own), 403 | 428), "a move of its own child goes past the gate: {own:?}");
        let waits = call(chat.clone(), "agents.move", json!({"agent": "other", "to": "nowhere"})).await;
        anyhow::ensure!(code(&waits) == 428, "a move of another of the owner's waits for the click: {waits:?}");
        Ok(())
    })
}

/// P5.6's slice S4: the Agents page's Recover (a gone machine's agents going on elsewhere) is `agents.recover`, which
/// moves agents its chat didn't start: an agent's waits on its person's click; the person's own goes at once.
#[test]
fn p5_6_s4_recover_is_an_operation_held_to_the_gate() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let waits = call(chat.clone(), "agents.recover", json!({"machine": "gone"})).await;
        anyhow::ensure!(code(&waits) == 428, "an agent's recover waits for the click: {waits:?}");
        let by_person = call(person(&hub), "agents.recover", json!({"machine": "gone"})).await;
        anyhow::ensure!(by_person.as_ref().is_ok_and(|v| v.get("moved") == Some(&json!([]))), "the owner's own, of a machine none of whose agents are kiln's: {by_person:?}");
        Ok(())
    })
}

/// P5.6's slice S5: a broadcast is `agents.broadcast` (the bare `/api/harness/command` gone, its one command to the
/// agents listed a broadcast's `to.ids`), which reaches agents its chat didn't start: an agent's waits on its person's
/// click; the person's own goes past the gate at once.
#[test]
fn p5_6_s5_broadcast_is_an_operation_held_to_the_gate() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let to = json!({"to": {"ids": ["other"]}, "command": {"type": "interrupt"}});
        let waits = call(chat.clone(), "agents.broadcast", to.clone()).await;
        anyhow::ensure!(code(&waits) == 428, "an agent's broadcast waits for the click: {waits:?}");
        let by_person = call(person(&hub), "agents.broadcast", to).await;
        anyhow::ensure!(!matches!(code(&by_person), 403 | 428), "the owner's own goes past the gate: {by_person:?}");
        Ok(())
    })
}

/// P5.6's slice S6: an agent's place in a person's attention is changed by operations (`agents.pin` here): an agent's
/// pin of another of the owner's agents waits on their click; the owner's own goes past the gate at once.
#[test]
fn p5_6_s6_attention_places_are_operations_held_to_the_gate() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        says(&hub, "chat").await;
        let chat = agent(&hub, "chat").await?;
        let waits = call(chat.clone(), "agents.pin", json!({"agent": "other"})).await;
        anyhow::ensure!(code(&waits) == 428, "an agent's pin of another of the owner's waits for the click: {waits:?}");
        let by_person = call(person(&hub), "agents.pin", json!({"agent": "other"})).await;
        anyhow::ensure!(!matches!(code(&by_person), 403 | 428), "the owner's own goes past the gate: {by_person:?}");
        Ok(())
    })
}

/// A session on runner `box` in `mode`, launched in it, acting for the owner, the agent `parent` started (none: a person
/// did), working in a volume laid over `/src/project`: its own, or `sharing`'s (none: its own, `v-<id>`), whose owner it
/// names.
async fn in_volume(hub: &Hub, id: &str, parent: Option<&str>, mode: &str, sharing: Option<&str>) -> anyhow::Result<()> {
    launched_in(hub, id, parent, mode, mode, sharing).await
}

/// [`in_volume`], launched in mode `launched` and working in `mode` now (it, or its harness, moved its mode since).
async fn launched_in(hub: &Hub, id: &str, parent: Option<&str>, launched: &str, mode: &str, sharing: Option<&str>) -> anyhow::Result<()> {
    let volume = sharing.unwrap_or(id);
    let made = crate::workspace::slates::Made { volume: format!("v-{volume}"), name: format!("kiln-{volume}"), mount: format!("/w/kiln-{volume}") };
    let workspace = crate::workspace::Workspace { kind: "volume".into(), over: Some("/src/project".into()), made: Some(made), owner: sharing.map(str::to_owned), ..Default::default() };
    let info = crate::harness::session::Info {
        id: id.into(),
        kind: "claude".into(),
        name: id.into(),
        target: "agent:box".into(),
        parent: parent.map(str::to_owned),
        mode: mode.into(),
        launched: crate::harness::lineage::level("claude", launched),
        workspace: Some(workspace),
        ..Default::default()
    };
    hub.harness.ingest(&Location::Machine("box".into()), id, crate::harness::core::Update::Open { info: Box::new(info), since: None }).await;
    hub.store.kv_set(crate::mcp::opener_key(id), "owner".to_owned()).await?;
    Ok(())
}

/// Whether a landing went past every policy and reached its agent's machine: the fixture's `box` isn't connected, which
/// the landing itself then says (502).
fn reached_the_machine(r: &Result<Value, Fail>) -> bool {
    code(r) == 502 && r.as_ref().err().is_some_and(|(_, m)| m.contains("not connected"))
}

/// docs/agentic.md W6, the person's rule of 2026-11-18 (kiln goal 01a1ec34599974448aed2672bafaf9a2): an agent writes
/// its work to disk as its mode lets it. In a mode that edits unasked (accept edits, bypass) its landing, of its plan as
/// it stands or of a manifest, goes past every policy at once and reaches its machine, nothing asked of anyone; in one
/// that only plans it is refused, and so is a landing of another agent's work by one that only plans; in one that asks
/// it reads its plan at once, but writing waits on its person's click: outside a person's turn there's no one to ask
/// (403), in the owner's turn it waits (428, one ask, which says where it writes), and the owner's click lets the same
/// call through once, as the owner. A parent in bypass landing its child's work in a mode that asks waits on the click
/// too: the lower of the two modes decides.
#[test]
fn w6_an_agents_landing_follows_its_mode() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        in_volume(&hub, "planner", None, "plan", None).await?;
        in_volume(&hub, "asker", None, "default", None).await?;
        in_volume(&hub, "editor", None, "acceptEdits", None).await?;
        in_volume(&hub, "bold", None, "bypassPermissions", None).await?;
        in_volume(&hub, "kid", Some("bold"), "default", None).await?;
        let as_agent = |who: &'static str| {
            let hub = Arc::clone(&hub);
            async move { agent(&hub, who).await }
        };

        // Modes that edit unasked: at once, its plan as it stands or a manifest.
        for who in ["editor", "bold"] {
            for input in [json!({"agent": who}), json!({"agent": who, "manifest": "ab12"})] {
                let r = call(as_agent(who).await?, "agents.land", input.clone()).await;
                anyhow::ensure!(reached_the_machine(&r), "{who} {input}: {r:?}");
            }
        }
        anyhow::ensure!(asked(&hub).await?.is_empty(), "nothing asked of anyone");

        // A mode that only plans: refused, its own landing and another's work it lands.
        for input in [json!({"agent": "planner"}), json!({"agent": "planner", "manifest": "ab12"})] {
            let r = call(as_agent("planner").await?, "agents.land", input).await;
            anyhow::ensure!(code(&r) == 403 && r.as_ref().err().is_some_and(|(_, m)| m.contains("planner's mode plans and changes nothing")), "{r:?}");
        }

        // A mode that asks: its plan at once (to read), writing waits on its person's click.
        let read = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker"})).await;
        anyhow::ensure!(reached_the_machine(&read), "its plan, read at once: {read:?}");
        let alone = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&alone) == 403 && alone.as_ref().err().is_some_and(|(_, m)| m.contains("isn't serving a person's chat turn")), "no one to ask: {alone:?}");
        says(&hub, "asker").await;
        let waits = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&waits) == 428 && waits.as_ref().err().is_some_and(|(_, m)| m.contains("waiting on owner's click")), "{waits:?}");
        let list = asked(&hub).await?;
        let ask = list.iter().find(|a| a.get("op") == Some(&json!("agents.land"))).cloned().unwrap_or_default();
        anyhow::ensure!(list.len() == 1 && ask.get("says").and_then(Value::as_str).is_some_and(|s| s.contains("written to /src/project") && s.contains("ab12")), "one ask, saying where it writes: {list:?}");
        let id = ask.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        call(person(&hub), "confirms.answer", json!({"id": id, "yes": true})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
        let other = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker", "manifest": "cd34"})).await;
        anyhow::ensure!(code(&other) == 428, "another manifest asks anew: {other:?}");
        let clicked = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker", "manifest": "ab12"})).await;
        anyhow::ensure!(reached_the_machine(&clicked), "the click lets it through, as the owner's: {clicked:?}");
        let again = call(as_agent("asker").await?, "agents.land", json!({"agent": "asker", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&again) == 428, "once: {again:?}");

        // A parent in bypass, its child in a mode that asks: the lower decides.
        let parent = call(as_agent("bold").await?, "agents.land", json!({"agent": "kid", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&parent) == 403 && parent.as_ref().err().is_some_and(|(_, m)| m.contains("isn't serving a person's chat turn")), "{parent:?}");
        // A person: the plan to read, and a manifest written, whatever the agent's mode.
        for of in ["planner", "asker"] {
            for input in [json!({"agent": of}), json!({"agent": of, "manifest": "ab12"})] {
                let r = call(person(&hub), "agents.land", input.clone()).await;
                anyhow::ensure!(reached_the_machine(&r), "the owner's {input}: {r:?}");
            }
        }
        Ok(())
    })
}

/// docs/agentic.md W2 and W6: a volume lands whole, so an agent working in another's lands nothing of its own while
/// that one works, whoever asks (409, naming whose it is); once the owner has gone, the agent left working in it lands
/// it.
#[test]
fn w6_an_agent_in_anothers_volume_lands_nothing_of_its_own_while_that_one_works() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        in_volume(&hub, "maker", None, "bypassPermissions", None).await?;
        in_volume(&hub, "helper", Some("maker"), "bypassPermissions", Some("maker")).await?;
        in_volume(&hub, "orphan", None, "bypassPermissions", Some("gone")).await?;
        for caller in [person(&hub), agent(&hub, "helper").await?] {
            let r = call(caller, "agents.land", json!({"agent": "helper", "manifest": "ab12"})).await;
            anyhow::ensure!(code(&r) == 409 && r.as_ref().err().is_some_and(|(_, m)| m.contains("works in maker's volume")), "{r:?}");
        }
        let left = call(agent(&hub, "orphan").await?, "agents.land", json!({"agent": "orphan"})).await;
        anyhow::ensure!(reached_the_machine(&left), "its owner gone, the last in it lands it: {left:?}");
        Ok(())
    })
}

/// The person's rule of 2026-11-18 (an agent never raises its own permission; docs/agentic.md W6), by use: an agent
/// launched in a mode that asks, whose mode now is bypass (its harness reporting a mode it moved to itself, or a command
/// it sent itself), lands as its launcher let it: its writing waits on its person's click (refused outside their turn,
/// 428 in it), never at once; one launched in bypass and lowered since to plan is refused. Judging the live mode alone
/// writes the raised agent's landing at once (it reaches its machine).
#[test]
fn w6_an_agent_that_raised_its_own_mode_still_lands_as_its_launcher_let_it() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        launched_in(&hub, "climber", None, "default", "bypassPermissions", None).await?;
        launched_in(&hub, "lowered", None, "bypassPermissions", "plan", None).await?;
        let alone = call(agent(&hub, "climber").await?, "agents.land", json!({"agent": "climber", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&alone) == 403 && alone.as_ref().err().is_some_and(|(_, m)| m.contains("isn't serving a person's chat turn")), "a raised mode writes nothing at once: {alone:?}");
        says(&hub, "climber").await;
        let waits = call(agent(&hub, "climber").await?, "agents.land", json!({"agent": "climber", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&waits) == 428, "in its person's turn it waits on their click: {waits:?}");
        let lowered = call(agent(&hub, "lowered").await?, "agents.land", json!({"agent": "lowered", "manifest": "ab12"})).await;
        anyhow::ensure!(code(&lowered) == 403 && lowered.as_ref().err().is_some_and(|(_, m)| m.contains("plans and changes nothing")), "{lowered:?}");
        Ok(())
    })
}

/// The person's rule of 2026-11-18 (an agent never raises its own permission; docs/agentic.md W6), by use: the gate on
/// every way a command comes in (`lineage::may_command`). An agent launched in a mode that asks sets its own mode no
/// higher, not even queued behind a state (`at_state`): bypass is refused, saying so, and plan isn't; the same command
/// from a person isn't refused. An agent launched in bypass sets the mode of an agent it started up to its own grant,
/// and an agent it didn't start is no business of this gate (the send's own rules answer). Without the gate each of the
/// refused commands would reach its machine.
#[test]
fn w6_an_agent_never_sets_a_mode_above_its_own_grant() -> anyhow::Result<()> {
    use crate::harness::model::Command;
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        launched_in(&hub, "climber", None, "default", "default", None).await?;
        launched_in(&hub, "bold", None, "bypassPermissions", "bypassPermissions", None).await?;
        launched_in(&hub, "kid", Some("bold"), "default", "default", None).await?;
        let mode = |m: &str| Command::SetMode { mode: m.into() };
        let queued = |m: &str| Command::AtState { client: "c1".into(), revision: "r1".into(), command: Box::new(mode(m)) };
        let gate = |caller: &Caller, id: &str, c: &Command| crate::harness::lineage::may_command(&hub, caller, id, c);
        let climber = agent(&hub, "climber").await?.caller;
        let raised = gate(&climber, "climber", &mode("bypassPermissions"));
        anyhow::ensure!(raised.as_ref().err().is_some_and(|(c, m)| c.as_u16() == 403 && m.contains("an agent never raises a mode past its own")), "{raised:?}");
        anyhow::ensure!(gate(&climber, "climber", &queued("bypassPermissions")).is_err(), "nor queued behind a state");
        anyhow::ensure!(gate(&climber, "climber", &mode("acceptEdits")).is_err(), "accept edits is above ask too");
        anyhow::ensure!(gate(&climber, "climber", &mode("plan")).is_ok() && gate(&climber, "climber", &mode("default")).is_ok(), "lower, or as it is: its own to set");
        anyhow::ensure!(gate(&Caller::owner(), "climber", &mode("bypassPermissions")).is_ok(), "a person sets any mode");
        let bold = agent(&hub, "bold").await?.caller;
        anyhow::ensure!(gate(&bold, "kid", &mode("bypassPermissions")).is_ok(), "an agent sets its child's mode up to its own grant");
        anyhow::ensure!(gate(&climber, "climber", &Command::Interrupt).is_ok(), "a command that sets no permission isn't held here");
        Ok(())
    })
}

/// The person's rule of 2026-11-18 (an agent never raises its own permission; docs/agentic.md W6), by use through
/// `agents.control`, which the gate lets an agent call on itself at once: a control that sets its permissions (an ACP
/// agent's mode option, which kiln doesn't list as a control: `lineage::permission_control`) is refused, saying a person
/// sets it; one that sets no permission (its name) goes on; the person's own goes on. Without the refusal the agent's
/// own mode option would reach its machine.
#[test]
fn w6_an_agent_sets_none_of_its_own_permission_controls() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = hub().await?;
        let chat = agent(&hub, "chat").await?;
        let raised = call(chat.clone(), "agents.control", json!({"agent": "chat", "control": "acp:mode", "value": "yolo"})).await;
        anyhow::ensure!(code(&raised) == 403 && raised.as_ref().err().is_some_and(|(_, m)| m.contains("an agent never raises its own")), "{raised:?}");
        let named = call(chat.clone(), "agents.control", json!({"agent": "chat", "control": "rename", "value": "Chat"})).await;
        anyhow::ensure!(code(&named) != 403, "a control that sets no permission goes on: {named:?}");
        let by_person = call(person(&hub), "agents.control", json!({"agent": "chat", "control": "acp:mode", "value": "yolo"})).await;
        anyhow::ensure!(code(&by_person) != 403, "a person's goes on: {by_person:?}");
        Ok(())
    })
}

/// The person's rule of 2026-11-18 (docs/agentic.md W6), by use of the broadcast itself (`harness::broadcast::run`, as
/// `agents.broadcast` runs it once its caller is past the gate): an agent's broadcast of `set_mode` to bypass, to itself,
/// is refused for it and sends it nothing (`failed`, saying why); a person's isn't refused.
#[test]
fn w6_a_broadcast_raises_no_agent_past_its_senders_grant() -> anyhow::Result<()> {
    use crate::harness::broadcast::{Broadcast, To, run};
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        launched_in(&hub, "climber", None, "default", "default", None).await?;
        let among: Vec<crate::harness::session::Info> = hub.harness.info("climber").into_iter().collect();
        let b = Broadcast { to: To { ids: vec!["climber".into()], ..To::default() }, text: None, attachments: Vec::new(), mode: None, command: Some(json!({"type": "set_mode", "mode": "bypassPermissions"})), approvals: None, key: None };
        let climber = agent(&hub, "climber").await?.caller;
        let own = run(&hub, &b, &climber, "agent:climber", &among).await?;
        let why = own.results.first().and_then(|o| o.reason.clone()).unwrap_or_default();
        anyhow::ensure!(own.failed == 1 && own.sent == 0 && why.starts_with("refused: an agent never raises a mode past its own"), "{own:?}");
        let by_person = run(&hub, &b, &Caller::owner(), "admin", &among).await?;
        anyhow::ensure!(by_person.results.iter().all(|o| !o.reason.as_deref().unwrap_or_default().starts_with("refused:")), "{by_person:?}");
        Ok(())
    })
}

/// The person's rule of 2026-11-18 (docs/agentic.md W6), by use: a person's raise of an agent's mode is its grant. An
/// agent launched in a mode that asks, whose runner reports the grant a person's `set_mode` gave it (its state's record,
/// `State::launched`, which a session moves only on a mode set by command), lands at once: its landing reaches its
/// machine. One whose harness alone reports the raise still waits on its person
/// (`w6_an_agent_that_raised_its_own_mode_still_lands_as_its_launcher_let_it`).
#[test]
fn w6_a_persons_raise_of_an_agents_mode_is_its_grant() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let hub = crate::hub::tests::scratch_hub_with(|_| {}).await?;
        hub.put_agent(crate::hub::tests::live_runner("box", "linux", "x86_64"));
        launched_in(&hub, "raised", None, "default", "default", None).await?;
        let state = crate::harness::model::State { mode: "bypassPermissions".into(), launched: Some(crate::harness::lineage::Level::Bypass), ..Default::default() };
        hub.harness.ingest(&Location::Machine("box".into()), "raised", crate::harness::core::Update::State { state: Box::new(state) }).await;
        let info = hub.harness.info("raised").ok_or_else(|| anyhow::anyhow!("no session"))?;
        anyhow::ensure!(info.launched == Some(crate::harness::lineage::Level::Bypass), "the server's copy follows its state: {:?}", info.launched);
        let landed = call(agent(&hub, "raised").await?, "agents.land", json!({"agent": "raised", "manifest": "ab12"})).await;
        anyhow::ensure!(reached_the_machine(&landed), "a person's grant writes at once: {landed:?}");
        Ok(())
    })
}
