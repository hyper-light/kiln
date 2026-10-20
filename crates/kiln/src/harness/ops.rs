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

/// The agent `id` names: its id, or the start of it (8 characters, as lists shorten it).
fn known(cx: &Cx, id: &str) -> Result<Info, Fail> {
    if let Some(i) = cx.hub.harness.info(id) {
        return Ok(i);
    }
    if id.len() >= 4 {
        let mut found: Vec<Info> = cx.hub.harness.list().into_iter().filter(|i| i.id.starts_with(id)).collect();
        if found.len() > 1 {
            return Err((StatusCode::BAD_REQUEST, format!("{id} is the start of {} agents' ids: give more of it", found.len())));
        }
        if let Some(i) = found.pop() {
            return Ok(i);
        }
    }
    Err((StatusCode::NOT_FOUND, format!("no agent {id}")))
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
    lineage::may_steer(&cx.hub, &cx.caller, id)
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
    let info = known(&cx, &agent_of(&v)?)?;
    let id = info.id.clone();
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
    let id = info.id.clone();
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
    // Each by its id or the start of it; one kiln doesn't run (any more) has ended.
    let ids: Vec<String> = ids_of(&v)?.into_iter().map(|id| known(&cx, &id).map_or(id, |i| i.id)).collect();
    let until = match v.get("until").and_then(Value::as_str).unwrap_or("settled") {
        "settled" => Until::Settled,
        "change" => Until::Change,
        other => return Err((StatusCode::BAD_REQUEST, format!("`until` is settled or change, not {other:?}"))),
    };
    let secs = number(&v, "timeout_s").unwrap_or(wait::DEFAULT.as_secs_f64()).clamp(0.0, wait::MAX.as_secs_f64());
    let out = wait::wait(&cx.hub, &ids, until, flag(&v, "any"), std::time::Duration::from_secs_f64(secs)).await;
    // What a parent saw settle here, it isn't told of again (`told`).
    for id in &ids {
        if let Some(info) = cx.hub.harness.info(id)
            && info.parent.is_some()
            && info.parent == cx.caller.agent
            && wait::settled(wait::stand(&info))
        {
            super::told::saw(&info);
        }
    }
    Ok(out)
}

/// The text of a reply event, and when it was said.
fn reply_of(e: &Value) -> Option<Value> {
    let text = e.get("text").and_then(Value::as_str)?;
    Some(json!({"text": text, "at": e.get("at"), "streaming": e.get("streaming").and_then(Value::as_bool).unwrap_or(false)}))
}

async fn result(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    let id = info.id.clone();
    // Its parent reading it now isn't told of it again (`told`).
    if info.parent.is_some() && info.parent == cx.caller.agent {
        super::told::saw(&info);
    }
    let (_, reply, changes) = cx.hub.harness.conclusion(&id).unwrap_or_default();
    // Its last words, from its timeline; else as lists keep them (shortened).
    let reply = reply.as_ref().and_then(reply_of).unwrap_or_else(|| json!({"text": info.summary, "shortened": true}));
    let record = crate::workspace::change::of(&cx.hub, &id).await;
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
        // Its workspace of its own (docs/agentic.md, W): the folder it's laid over, and its volume.
        "workspace": info.workspace.as_ref().map(|w| json!({"kind": w.kind, "over": w.over, "sub": w.sub, "volume": w.made.as_ref().map(|m| &m.volume), "mount": w.made.as_ref().map(|m| &m.mount)})),
        "branch": record.as_ref().map(|r| json!(r.branch)),
        "change_request": record.as_ref().map(crate::workspace::change::shown),
    }))
}

async fn change(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    may_steer(&cx, &info.id)?;
    let want: crate::workspace::change::Want = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let r = crate::workspace::change::open(&cx.hub, &cx.caller.name, &info, want).await?;
    Ok(json!({"change_request": crate::workspace::change::shown(&r)}))
}

async fn workspace(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    let Some(w) = info.workspace.clone() else {
        return Ok(json!({"workspace": Value::Null, "drift": []}));
    };
    let look = crate::workspace::change::ask_machine(&cx.hub, &info, &crate::workspace::Ask::Look).await?;
    Ok(json!({"workspace": {"kind": w.kind, "over": w.over, "sub": w.sub, "volume": w.made.as_ref().map(|m| &m.volume), "mount": w.made.as_ref().map(|m| &m.mount)}, "drift": look.get("drift").cloned().unwrap_or(json!([]))}))
}

async fn land(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    may_steer(&cx, &info.id)?;
    if info.workspace.as_ref().is_none_or(|w| w.made.is_none()) {
        return Err((StatusCode::CONFLICT, format!("{} works in its folder itself: what it changed is there already", info.name)));
    }
    let Some(manifest) = v.get("manifest").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()).map(str::to_owned) else {
        let plan = crate::workspace::change::ask_machine(&cx.hub, &info, &crate::workspace::Ask::Plan).await?;
        return Ok(plan);
    };
    // Writing to a person's folder waits on a person (docs/agentic.md: no agent answers what waits on one).
    if cx.caller.agent.is_some() || cx.caller.kind == crate::authz::gate::Kind::Agent {
        return Err((StatusCode::FORBIDDEN, "writing an agent's work to disk is a person's to say: they do it from the agent's Changes, or with this operation themselves".to_owned()));
    }
    let landed = crate::workspace::change::ask_machine(&cx.hub, &info, &crate::workspace::Ask::Land { manifest }).await?;
    if landed.get("landed").is_some() {
        cx.hub.store.audit(cx.caller.name.clone(), "agent.land".into(), format!("{} ({}): its workspace written to {}", info.name, lineage::short(&info.id), info.workspace.as_ref().and_then(|w| w.over.clone()).unwrap_or_default())).await;
    }
    Ok(landed)
}

async fn changes(cx: Cx, v: Value) -> Out {
    let one = v.get("agent").and_then(Value::as_str).map(str::trim).filter(|a| !a.is_empty());
    let list: Vec<Value> = match one {
        Some(a) => {
            let id = known(&cx, a).map_or_else(|_| a.to_owned(), |i| i.id);
            crate::workspace::change::of(&cx.hub, &id).await.iter().map(crate::workspace::change::shown).collect()
        }
        None => crate::workspace::change::all(&cx.hub).await.iter().map(crate::workspace::change::shown).collect(),
    };
    Ok(json!({"changes": list}))
}

async fn interrupt(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    let id = info.id.clone();
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
    let id = known(&cx, &agent_of(&v)?)?.id;
    may_steer(&cx, &id)?;
    let mut ended: Vec<String> = Vec::new();
    // Its children first (those that ended too: they leave with it), the deepest first, so none
    // outlives the one it reports to.
    if flag(&v, "children") {
        let mut kids = lineage::descendants(&cx.hub.harness.list(), &id);
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
    let id = known(&cx, &agent_of(&v)?)?.id;
    Ok(json!({"children": lineage::children_brief(&cx.hub, &id)}))
}

/// How the server's own socket asks it an operation (`super::socket::Forward`): for the agent that
/// asks, or the person in the kiln shell.
pub fn forward(hub: std::sync::Arc<crate::hub::Hub>) -> super::socket::Forward {
    std::sync::Arc::new(move |asker, name, input| {
        let hub = std::sync::Arc::clone(&hub);
        Box::pin(async move {
            let caller = match &asker {
                super::socket::Asker::Agent(id) => crate::mcp::session_caller(&hub, id, None).await,
                super::socket::Asker::Terminal(t) => crate::mcp::terminal_caller(&hub, t, None).await,
            };
            match caller {
                Ok(caller) => match crate::ops::call(Cx { hub, caller }, &name, input).await {
                    Ok(v) => json!({"ok": v}),
                    Err((status, msg)) => json!({"error": msg, "status": status.as_u16()}),
                },
                Err(e) => json!({"error": e}),
            }
        })
    })
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
            summary: "Start agents to do part of the work: `kind` (claude, codex, gemini, opencode, goose, amp, antigravity), `on` a machine's name or what it needs (linux/amd64, macos, +gpu, cpus>=16; the idlest that suits and has the harness; by default your own machine), `cwd` (by default your folder, on your machine), `prompt` (the brief: what to do, what done looks like, what to report), `model`, `effort`, `mode` (at most your own: plan < ask < accept edits < auto < bypass), `name`, `count` (up to 16). An agent you start is your child: it gets no more than you have, and its approvals and questions go to a person, never to you. Carry on or end your turn: kiln tells you when it settles (its turn over, a person needed, stopped), then agents.result reads what it concluded; agents.wait when you can't go on without it.",
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
                        "workspace": {"type": "string", "enum": ["folder", "volume"], "description": "Where it works: `folder`, its folder itself (the default), or `volume`, a Slates volume of its own laid over the folder's repository, where what it changes stays its own until it goes up for review (agents.change) or a person writes it to disk. Where the machine has Slates."}
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
            summary: "Wait until agents settle (`until: settled`, the default: each is idle, needs a person, is paused for a usage limit, has stopped or has ended) or until anything about them changes (`until: change`), all of them or with `any` the first, up to `timeout_s` (default 300, at most 600; call again to wait longer). Returns where each stands. A message sent just before counts: an agent isn't settled until it has had its turn. You needn't wait: kiln tells you when each agent you started settles; wait when you can't go on without it.",
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
            summary: "What an agent concluded: its last reply, how its last turn ended, what it changed (files, lines added and removed), its folder and machine, its cost, and its change request and branch (agents.change) once it has one.",
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
            name: "agents.change",
            summary: "Open a change request from an agent's work (yours, or an agent's you started), or push a new version of the one it has: the files it changed, committed on its folder's last commit as `agents/<what it's about>-<id>` (the folder's branch, index and working tree untouched) and pushed for review, its author the agent, for its person. Into kiln's own review for a repository kiln hosts; as a pull request for one on GitHub, GitLab or Bitbucket (the branch pushed, and the request opened when kiln has an integration there, else a link to open it). Comments, requested changes, failing checks (with `fix_checks`) and the land queue then come back to the agent as messages. `title` (else what the agent is about), `body`, `draft`, `trunk` (else the repository's default).",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": "The agent whose work it is (as agents.list gives it); leave it out for your own."},
                        "title": {"type": "string"},
                        "body": {"type": "string", "description": "What it changes and why, for reviewers."},
                        "draft": {"type": "boolean", "description": "Not ready for review yet (kiln's own review)."},
                        "trunk": {"type": "string", "description": "The branch it goes into."},
                        "fix_checks": {"type": "boolean", "description": "Failing checks go to the agent to fix (the default)."}
                    }),
                    &[],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "change"]))),
            run: |cx, v| Box::pin(change(cx, v)),
        },
        Op {
            name: "agents.workspace",
            summary: "An agent's workspace of its own (a Slates volume over its folder): the folder it's laid over, its volume and mount, and the files under it that changed on disk after the agent changed them (drift).",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &["agent"]),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "workspace"]))),
            run: |cx, v| Box::pin(workspace(cx, v)),
        },
        Op {
            name: "agents.land",
            summary: "Write an agent's workspace of its own back to the folder it's laid over. Without `manifest`: the plan (what it would create, replace and delete, and any file changed on disk since, which it won't overwrite), for a person to read. With the plan's `manifest`: written exactly as that plan says, or, if anything changed since, nothing written and the new plan back (`changed`) — only a person can write, never an agent. Build output and what git ignores stay out, and so does `.git`.",
            read_only: false,
            destructive: true,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "manifest": {"type": "string", "description": "The plan's manifest hash: what is written is exactly what that plan lists."}
                    }),
                    &["agent"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "land"]))),
            run: |cx, v| Box::pin(land(cx, v)),
        },
        Op {
            name: "agents.changes",
            summary: "Agents' change requests: whose, where (repository, branch, the change and its page), its versions, review and checks, and whether it landed. `agent` for one.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"agent": {"type": "string", "description": AGENT}}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "harness", "changes"]))),
            run: |cx, v| Box::pin(changes(cx, v)),
        },
        Op {
            name: "agents.stop",
            summary: "End an agent you started for good (its changes stay on disk); with `children`, the agents it started (and theirs) too.",
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

/// The operations end to end on a hub of their own, under a timeout: a parent that asks starts a
/// child with no more than it has, through `crate::ops::call` (the route table, the audit trail,
/// the registry). No harness ever runs: the children's folder doesn't exist, so they can't start,
/// and the parent's program is missing.
#[cfg(test)]
mod end_to_end {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::harness::session::Spec;
    use crate::hub::Hub;
    use crate::ops::{Cx, call};

    /// A parent agent kiln runs on the server, in mode `mode`.
    async fn parent(hub: &Arc<Hub>, mode: &str, allow_bypass: bool) -> anyhow::Result<String> {
        let dir = tempfile::tempdir()?.keep();
        let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), mode: Some(mode.into()), program: Some(dir.join("no-claude").to_string_lossy().into_owned()), allow_bypass, ..Spec::default() };
        let infos = hub.harness.open(hub, "local", spec, 1).await?;
        let id = infos.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no parent"))?;
        // Its machine reports its mode (the registry follows the server's own sessions).
        for _ in 0..200 {
            if hub.harness.info(&id).is_some_and(|i| i.mode == mode) {
                return Ok(id);
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("the parent's mode never showed")
    }

    async fn as_agent(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Cx> {
        let caller = crate::mcp::session_caller(hub, id, None).await.map_err(anyhow::Error::msg)?;
        Ok(Cx { hub: Arc::clone(hub), caller })
    }

    const NOWHERE: &str = "/nonexistent/kiln-agents-test";

    fn run(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the operations hung (a lock held across a wait?)"),
        }
    }

    async fn hub_with(f: impl FnOnce(&mut crate::config::Config)) -> anyhow::Result<Arc<Hub>> {
        let hub = crate::hub::tests::scratch_hub_with(|c| {
            c.terminal.enabled = true;
            f(c);
        })
        .await?;
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::registry::Registry::run_local(h).await;
        });
        Ok(hub)
    }

    #[test]
    fn a_child_asked_for_more_than_its_parent_has_is_refused_and_one_within_it_starts() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let p = parent(&hub, "default", true).await?;
            let cx = as_agent(&hub, &p).await?;
            // Bypass under a parent that asks: refused, and nothing starts.
            let before = hub.harness.list().len();
            let refused = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "bypassPermissions", "cwd": NOWHERE})).await;
            let Err((status, why)) = refused else { anyhow::bail!("bypass under ask started") };
            anyhow::ensure!(status == http::StatusCode::FORBIDDEN && why.contains("above its own ask"), "{status} {why}");
            anyhow::ensure!(hub.harness.list().len() == before, "nothing started");
            // Codex's full access, and its default (auto), are above ask too: it starts in untrusted.
            anyhow::ensure!(call(cx.clone(), "agents.start", json!({"kind": "codex", "mode": "full-access", "cwd": NOWHERE})).await.is_err());
            let codex = call(cx.clone(), "agents.start", json!({"kind": "codex", "cwd": NOWHERE, "prompt": "list the files"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let cid = codex.pointer("/agents/0/id").and_then(Value::as_str).unwrap_or("").to_owned();
            let child = hub.harness.info(&cid).ok_or_else(|| anyhow::anyhow!("no child"))?;
            anyhow::ensure!(child.parent.as_deref() == Some(p.as_str()), "the parent is the agent that asked");
            anyhow::ensure!(child.mode == "untrusted" || hub.harness.state(&cid).is_some_and(|s| s.mode == "untrusted") || child.mode.is_empty(), "codex starts at most at ask: {}", child.mode);
            anyhow::ensure!(codex.get("parent") == Some(&json!(p)));
            // Within its parent's: as asked.
            let plan = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE, "name": "planner"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let kid = plan.pointer("/agents/0/id").and_then(Value::as_str).unwrap_or("").to_owned();
            // It can't start (its folder isn't there): it has stopped, and a wait says so at once.
            let waited = call(cx.clone(), "agents.wait", json!({"agent": kid, "timeout_s": 20})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(waited.get("done") == Some(&json!(true)), "{waited}");
            anyhow::ensure!(matches!(waited.pointer("/agents/0/stand").and_then(Value::as_str), Some("stopped" | "ended")), "{waited}");
            let result = call(cx.clone(), "agents.result", json!({"agent": kid})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(result.get("changes").is_some() && result.get("workspace") == Some(&Value::Null) && result.get("reply").is_some(), "{result}");
            // The parent lists its children; a person's list filters by parent too.
            let mine = call(cx.clone(), "agents.list", json!({"parent": p})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(mine.get("agents").and_then(Value::as_array).map(Vec::len) == Some(2), "{mine}");
            // Another agent can't steer or end them; their parent can, and ends them with it.
            let other = parent(&hub, "default", true).await?;
            let stranger = as_agent(&hub, &other).await?;
            let Err((s, _)) = call(stranger, "agents.send", json!({"agent": kid, "text": "stop that"})).await else { anyhow::bail!("a stranger steered a child") };
            anyhow::ensure!(s == http::StatusCode::FORBIDDEN);
            let ended = call(cx.clone(), "agents.stop", json!({"agent": p, "children": true})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(ended.get("ended").and_then(Value::as_array).map(Vec::len) == Some(3), "the parent and its two children: {ended}");
            anyhow::ensure!(hub.harness.info(&kid).is_none() && hub.harness.info(&cid).is_none() && hub.harness.info(&p).is_none());
            // Every change was audited as the agent, for its person.
            let rows = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM audit2 WHERE principal LIKE 'agent:%' AND on_behalf_of = 'admin'", [], |r| r.get::<_, i64>(0))?)).await.unwrap_or(0);
            anyhow::ensure!(rows > 0, "audited as agent:<id> for its person");
            Ok(())
        })
    }

    #[test]
    fn bypass_needs_the_parent_s_own_and_chains_and_children_are_bounded() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|c| {
                c.agents.max_depth = 1;
                c.agents.max_children = 1;
            })
            .await?;
            // A parent in bypass that was started without it may not hand it on.
            let p = parent(&hub, "bypassPermissions", false).await?;
            let cx = as_agent(&hub, &p).await?;
            let Err((_, why)) = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "bypassPermissions", "cwd": NOWHERE})).await else { anyhow::bail!("bypass without allow_bypass") };
            anyhow::ensure!(why.contains("bypass"), "{why}");
            // One child at once.
            let one = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let kid = one.pointer("/agents/0/id").and_then(Value::as_str).unwrap_or("").to_owned();
            // (It stopped: it isn't running, so another may start.)
            let two = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE, "count": 2})).await;
            anyhow::ensure!(two.as_ref().is_err_and(|(_, m)| m.contains("max_children")), "{two:?}");
            // A child is 1 deep: with max_depth 1 it may not start one of its own.
            let deep = as_agent(&hub, &kid).await?;
            let Err((_, why)) = call(deep, "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE})).await else { anyhow::bail!("too deep") };
            anyhow::ensure!(why.contains("max_depth"), "{why}");
            Ok(())
        })
    }
}
