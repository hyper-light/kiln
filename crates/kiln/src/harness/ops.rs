//! The agents operations (`agents.*`, docs/agents.md "Agents that run agents"), in kiln's one
//! operation table (`crate::ops::own`): each is an HTTP route under `/api/harness/agents`, an MCP
//! tool (`agents_start`, …), a code-mode function (`kiln.agents.start`) and, through a kiln shell's
//! socket, a CLI command (`kiln spawn --on`, `kiln agents wait`, `kiln agents result`).
//!
//! An agent that calls them is the parent of what it starts (`Caller::agent`, from how it reached
//! kiln), which gets no more than it has (`super::lineage`). An agent steers, interrupts and ends
//! only the agents it started (and theirs). No operation answers what waits on a person: an
//! agent's approvals, questions and plans are for people, in kiln.

use http::{Method, StatusCode};
use serde_json::{Value, json};

use super::lineage::{self, Ask, Starter};
use super::model::{Command, SendMode};
use super::registry::Sent;
use super::session::Info;
use super::wait::{self, Until};
use crate::ops::{Cx, Fail, Op, Out, path, schema, text};

const AGENT: &str = "The agent's id (as agents.list gives it).";

fn agent_of(v: &Value) -> Result<String, Fail> {
    text(v, "agent")
}

fn known(cx: &Cx, id: &str) -> Result<Info, Fail> {
    cx.hub.harness.info(id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no agent {id}")))
}

/// Who starts agents for this call.
pub fn starter(cx: &Cx) -> Result<Starter, Fail> {
    let parent = match cx.caller.agent.as_deref() {
        // An agent kiln no longer runs can't start one: there'd be no parent to hold it to.
        Some(id) => Some(cx.hub.harness.info(id).ok_or_else(|| (StatusCode::FORBIDDEN, format!("kiln doesn't run agent {} any more, so it can't start agents for it", lineage::short(id))))?),
        None => None,
    };
    Ok(Starter { parent, opener: cx.caller.opener(), actor: cx.caller.name.clone(), plan_run: None })
}

/// An agent calling may steer, interrupt or end only itself and the agents it started (and theirs).
fn may_steer(cx: &Cx, id: &str) -> Result<(), Fail> {
    let Some(me) = cx.caller.agent.as_deref() else { return Ok(()) };
    if me == id || lineage::descends(&cx.hub.harness.list(), id, me) {
        return Ok(());
    }
    Err((StatusCode::FORBIDDEN, format!("an agent steers and ends only the agents it started (and theirs): {} isn't one of {}'s", lineage::short(id), lineage::short(me))))
}

async fn list(cx: Cx, v: Value) -> Out {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let (machine, harness, state, parent, run) = (s("machine"), s("harness"), s("state"), s("parent"), s("plan_run"));
    let agents: Vec<Value> = cx
        .hub
        .harness
        .list()
        .iter()
        .filter(|i| machine.as_deref().is_none_or(|m| wait::machine(&i.target) == m || (m == "server" && i.target == "local")))
        .filter(|i| harness.as_deref().is_none_or(|h| i.kind == h))
        .filter(|i| parent.as_deref().is_none_or(|p| i.parent.as_deref() == Some(p)))
        .filter(|i| run.as_deref().is_none_or(|r| i.plan_run.as_deref() == Some(r)))
        .map(wait::brief)
        .filter(|b| state.as_deref().is_none_or(|st| b.get("stand").and_then(Value::as_str) == Some(st) || b.get("status").and_then(Value::as_str) == Some(st)))
        .collect();
    Ok(json!({"agents": agents}))
}

/// What a state says that `agents.get` leaves out: catalogs and lists only a console needs.
const HEAVY: &[&str] = &["skills", "plugins", "mcp", "harness_status", "delivery", "control_values", "inventory", "limits", "stop_waived"];

async fn get(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    let info = known(&cx, &id)?;
    let (mut state, _, _) = cx.hub.harness.conclusion(&id).unwrap_or_default();
    if let Some(o) = state.as_object_mut() {
        for k in HEAVY {
            o.remove(*k);
        }
    }
    Ok(json!({"agent": wait::brief(&info), "info": info, "state": state}))
}

async fn start(cx: Cx, v: Value) -> Out {
    let ask: Ask = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    if ask.kind.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "`kind` is required: the harness (claude, codex, …)".into()));
    }
    let by = starter(&cx)?;
    let parent = by.parent.as_ref().map(|p| p.id.clone());
    let infos = lineage::start(&cx.hub, &by, ask).await?;
    Ok(json!({"agents": infos.iter().map(wait::brief).collect::<Vec<_>>(), "parent": parent}))
}

async fn send(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    let body = text(&v, "text")?;
    let mode: SendMode = match v.get("mode").and_then(Value::as_str).unwrap_or("queue") {
        "queue" => SendMode::Queue,
        "steer" => SendMode::Steer,
        "now" => SendMode::Now,
        other => return Err((StatusCode::BAD_REQUEST, format!("`mode` is queue, steer or now, not {other:?}"))),
    };
    let info = known(&cx, &id)?;
    may_steer(&cx, &id)?;
    let message = crate::util::new_id();
    wait::sent(&id, info.last_turn.as_ref().map(|t| t.at));
    match cx.hub.harness.command(&cx.hub, &id, Command::Send { id: message.clone(), text: body, mode, attachments: Vec::new(), overrides: None }).await {
        Sent::Ok => Ok(json!({"sent": true, "message": message})),
        Sent::NoSession => Err((StatusCode::NOT_FOUND, format!("no agent {id}"))),
        Sent::Unreachable => Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now: try again once it's back", info.name))),
    }
}

/// The agents a wait is for: `agent`, or `agents`.
fn ids_of(v: &Value) -> Result<Vec<String>, Fail> {
    let mut ids: Vec<String> = v.get("agents").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    // Over HTTP: `agents=a,b`.
    if let Some(list) = v.get("agents").and_then(Value::as_str) {
        ids.extend(list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned));
    }
    if let Some(one) = v.get("agent").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        ids.push(one.trim().to_owned());
    }
    ids.dedup();
    if ids.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "say which agent to wait for: `agent`, or `agents`".into()));
    }
    if ids.len() > 64 {
        return Err((StatusCode::BAD_REQUEST, "a wait is for 64 agents at the most".into()));
    }
    Ok(ids)
}

/// A number given as a number or, over HTTP, as text.
fn number(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())))
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).is_some_and(|x| x.as_bool().unwrap_or_else(|| matches!(x.as_str(), Some("true" | "1"))))
}

async fn wait_for(cx: Cx, v: Value) -> Out {
    let ids = ids_of(&v)?;
    let until = match v.get("until").and_then(Value::as_str).unwrap_or("settled") {
        "settled" => Until::Settled,
        "change" => Until::Change,
        other => return Err((StatusCode::BAD_REQUEST, format!("`until` is settled or change, not {other:?}"))),
    };
    let secs = number(&v, "timeout_s").unwrap_or(wait::DEFAULT.as_secs_f64()).clamp(0.0, wait::MAX.as_secs_f64());
    Ok(wait::wait(&cx.hub, &ids, until, flag(&v, "any"), std::time::Duration::from_secs_f64(secs)).await)
}

/// The text of a reply event, and when it was said.
fn reply_of(e: &Value) -> Option<Value> {
    let text = e.get("text").and_then(Value::as_str)?;
    Some(json!({"text": text, "at": e.get("at"), "streaming": e.get("streaming").and_then(Value::as_bool).unwrap_or(false)}))
}

async fn result(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    let info = known(&cx, &id)?;
    let (_, reply, changes) = cx.hub.harness.conclusion(&id).unwrap_or_default();
    // Its last words, from its timeline; else as lists keep them (shortened).
    let reply = reply.as_ref().and_then(reply_of).unwrap_or_else(|| json!({"text": info.summary, "shortened": true}));
    let files: Vec<Value> = changes
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| json!({"path": f.get("path"), "added": f.get("added"), "removed": f.get("removed"), "created": f.get("created").and_then(Value::as_bool).unwrap_or(false), "deleted": f.get("deleted").and_then(Value::as_bool).unwrap_or(false)}))
        .collect();
    Ok(json!({
        "agent": wait::brief(&info),
        "reply": reply,
        "last_turn": info.last_turn,
        "changes": {"files": files, "files_changed": info.files_changed, "lines_added": info.lines_added, "lines_removed": info.lines_removed},
        "cwd": info.cwd,
        "machine": wait::machine(&info.target),
        "cost_usd": info.cost_usd,
        // Filled in once agents work in workspaces of their own and open change requests
        // (docs/agentic.md, W and C).
        "workspace": Value::Null,
        "branch": Value::Null,
        "change_request": Value::Null,
    }))
}

async fn interrupt(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    let info = known(&cx, &id)?;
    may_steer(&cx, &id)?;
    match cx.hub.harness.command(&cx.hub, &id, Command::Interrupt).await {
        Sent::Ok => Ok(json!({"interrupted": true})),
        Sent::NoSession => Err((StatusCode::NOT_FOUND, format!("no agent {id}"))),
        Sent::Unreachable => Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now", info.name))),
    }
}

/// Ends agent `id` for good, its goal stack with it (and is audited by `actor`).
pub async fn end(hub: &crate::hub::Hub, id: &str, actor: &str) -> bool {
    if !hub.harness.close(hub, id).await {
        return false;
    }
    let _ = crate::goals::forget(hub, id).await;
    hub.store.audit(actor.to_owned(), "agent.close".into(), id.to_owned()).await;
    true
}

async fn stop(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    known(&cx, &id)?;
    may_steer(&cx, &id)?;
    let mut ended: Vec<String> = Vec::new();
    // Its children first, the deepest first, so none outlives the one it reports to.
    if flag(&v, "children") {
        let mut kids = lineage::live_descendants(&cx.hub.harness.list(), &id);
        kids.reverse();
        for k in kids {
            if end(&cx.hub, &k.id, &cx.caller.name).await {
                ended.push(k.id);
            }
        }
    }
    if end(&cx.hub, &id, &cx.caller.name).await {
        ended.push(id);
    }
    Ok(json!({"ended": ended}))
}

async fn children(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    known(&cx, &id)?;
    Ok(json!({"children": lineage::children_brief(&cx.hub, &id)}))
}

/// The `agents.*` operations.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "agents.list",
            summary: "Agents kiln runs, on every machine, each with where it stands (stand: working, idle, needs_you, paused, stopped, offline, ended), its machine, harness, folder, mode and what it changed. Filter by `machine` (a runner's name, or local for the server), `harness`, `state` (a stand or a status), `parent` (the agents an agent started) or `plan_run`.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"machine": {"type": "string"}, "harness": {"type": "string"}, "state": {"type": "string"}, "parent": {"type": "string", "description": "Only the agents this agent started."}, "plan_run": {"type": "string"}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "harness", "agents"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agents.get",
            summary: "One agent: where it stands, its info and its state (status, approvals waiting on a person, queue, tasks, to-dos, usage, how its last turn ended).",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "agents.start",
            summary: "Start agents to do part of the work: `kind` (claude, codex, gemini, opencode, goose, amp, antigravity), `on` a machine's name or what it needs (linux/amd64, macos, +gpu, cpus>=16; the idlest that suits and has the harness; by default your own machine), `cwd` (by default your folder, on your machine), `prompt` (the brief: what to do, what done looks like, what to report), `model`, `effort`, `mode` (at most your own: plan < ask < accept edits < auto < bypass), `name`, `count` (up to 16). An agent you start is your child: it gets no more than you have, and its approvals and questions go to a person, never to you. Then agents.wait and agents.result.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "kind": {"type": "string", "description": "The harness: claude, codex, gemini, opencode, goose, amp or antigravity."},
                        "on": {"type": "string", "description": "A machine's name (local for the server), or what it needs: linux/amd64, macos, arm64, +gpu, cpus>=16, mem>=32g."},
                        "cwd": {"type": "string", "description": "Its folder there (a path, or ~/…)."},
                        "prompt": {"type": "string", "description": "Its first message: the task."},
                        "model": {"type": "string"},
                        "effort": {"type": "string"},
                        "mode": {"type": "string", "description": "Its permission mode, at most yours (claude: plan, default, acceptEdits, auto, bypassPermissions; codex: read-only, untrusted, auto, full-access). Left out: its harness's default when that's within yours, else the most that is."},
                        "name": {"type": "string"},
                        "count": {"type": "integer", "minimum": 1, "maximum": 16},
                        "workspace": {"type": "string", "enum": ["folder"], "description": "Where it works: its folder itself (workspaces of their own are to come)."}
                    }),
                    &["kind"],
                )
            },
            route: |_| Ok((Method::POST, path(&["api", "harness", "agents"]))),
            run: |cx, v| Box::pin(start(cx, v)),
        },
        Op {
            name: "agents.send",
            summary: "Send an agent you started a message: `queue` (after its turn, the default), `steer` (into the turn under way) or `now` (stopping it).",
            read_only: false,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}, "text": {"type": "string"}, "mode": {"type": "string", "enum": ["queue", "steer", "now"]}}), &["agent", "text"]),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "send"]))),
            run: |cx, v| Box::pin(send(cx, v)),
        },
        Op {
            name: "agents.wait",
            summary: "Wait until agents settle (`until: settled`, the default: each is idle, needs a person, is paused for a usage limit, has stopped or has ended) or until anything about them changes (`until: change`), all of them or with `any` the first, up to `timeout_s` (default 300, at most 600; call again to wait longer). Returns where each stands. A message sent just before counts: an agent isn't settled until it has had its turn.",
            read_only: true,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "agents": {"type": "array", "items": {"type": "string"}, "description": "Several agents."},
                        "until": {"type": "string", "enum": ["settled", "change"]},
                        "any": {"type": "boolean", "description": "Return when the first of them is done, rather than all."},
                        "timeout_s": {"type": "number", "minimum": 0, "maximum": 600}
                    }),
                    &[],
                )
            },
            route: |_| Ok((Method::GET, path(&["api", "harness", "agents", "wait"]))),
            run: |cx, v| Box::pin(wait_for(cx, v)),
        },
        Op {
            name: "agents.result",
            summary: "What an agent concluded: its last reply, how its last turn ended, what it changed (files, lines added and removed), its folder and machine, and its cost. (Its workspace, branch and change request come once agents have them.)",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "result"]))),
            run: |cx, v| Box::pin(result(cx, v)),
        },
        Op {
            name: "agents.children",
            summary: "The agents still running that an agent started, and theirs.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "children"]))),
            run: |cx, v| Box::pin(children(cx, v)),
        },
        Op {
            name: "agents.interrupt",
            summary: "Stop the turn an agent you started is in (it stays, ready for the next message).",
            read_only: false,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "interrupt"]))),
            run: |cx, v| Box::pin(interrupt(cx, v)),
        },
        Op {
            name: "agents.stop",
            summary: "End an agent you started for good (its changes stay on disk); with `children`, the agents it started too.",
            read_only: false,
            destructive: true,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}, "children": {"type": "boolean", "description": "End the agents it started (and theirs) too."}}), &["agent"]),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "stop"]))),
            run: |cx, v| Box::pin(stop(cx, v)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_operation_answers_for_a_person() {
        // Approvals, questions and plans wait on people: nothing here decides them.
        for op in list_ops() {
            let schema = (op.input)().to_string();
            for word in ["decision", "approve", "answers", "choices"] {
                assert!(!schema.contains(word), "{} takes {word}", op.name);
            }
            assert!(!op.name.contains("approve") && !op.name.contains("answer"), "{}", op.name);
        }
        let sends = list_ops().into_iter().find(|o| o.name == "agents.send").map(|o| (o.input)()).unwrap_or_default();
        assert_eq!(sends.pointer("/properties/mode/enum"), Some(&json!(["queue", "steer", "now"])), "a message is all an agent sends another");
    }

    #[test]
    fn waits_say_whom_they_wait_for() {
        assert_eq!(ids_of(&json!({"agent": "a"})).ok(), Some(vec!["a".to_owned()]));
        assert_eq!(ids_of(&json!({"agents": ["a", "b"]})).ok(), Some(vec!["a".to_owned(), "b".to_owned()]));
        assert_eq!(ids_of(&json!({"agents": "a, b"})).ok(), Some(vec!["a".to_owned(), "b".to_owned()]), "over HTTP, a list in the query");
        assert!(ids_of(&json!({})).is_err());
        assert_eq!(number(&json!({"timeout_s": "12"}), "timeout_s"), Some(12.0));
        assert!(flag(&json!({"any": "true"}), "any") && flag(&json!({"any": true}), "any") && !flag(&json!({}), "any"));
    }
}
