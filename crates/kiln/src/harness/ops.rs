//! The agents operations (`agents.*`, docs/agents.md "Agents that run agents"), in kiln's one
//! operation table (`crate::ops::own`): each is an HTTP route under `/api/harness/agents`, an MCP
//! tool (`agents_start`, …), a code-mode function (`kiln.agents.start`) and, through a kiln shell's
//! socket, a CLI command (`kiln spawn --on`, `kiln agents wait`, `kiln agents result`).
//!
//! An agent that calls them is the parent of what it starts (`Caller::agent`, from how it reached
//! kiln), which gets no more than it has (`super::lineage`). An agent steers, interrupts and ends
//! only the agents it started (and theirs); it may send a queued message to any agent that works
//! for the same person, which is told who sent it. An agent's `agents.wait` hands over the messages
//! other agents sent it rather than leave them behind its turn. No operation answers what waits on
//! a person: an agent's approvals, questions and plans are for people, in kiln.

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
    Ok(Starter { parent, opener: cx.caller.opener(), actor: cx.caller.name.clone(), plan_run: None, asker: Some(cx.caller.clone()) })
}

/// An agent calling may steer, interrupt or end only itself and the agents it started (and theirs).
async fn may_steer(cx: &Cx, id: &str) -> Result<(), Fail> {
    lineage::may_steer(&cx.hub, &cx.caller, id).await
}

/// The agent `id` names (or begins), when the caller sees it (docs/agents.md, "Whose agents"):
/// one it doesn't see answers as one kiln doesn't run.
async fn seen(cx: &Cx, id: &str) -> Result<Info, Fail> {
    let info = known(cx, id)?;
    lineage::may_see(&cx.hub, &cx.caller, &info.id).await?;
    Ok(info)
}

async fn list(cx: Cx, v: Value) -> Out {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let (machine, harness, state, parent, run) = (s("machine"), s("harness"), s("state"), s("parent"), s("plan_run"));
    let agents: Vec<Value> = lineage::seen(&cx.hub, &cx.caller, cx.hub.harness.list())
        .await
        .iter()
        // A card's helper (`super::about`) is the card's: no agent of these.
        .filter(|i| i.helper.is_none())
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
    let info = seen(&cx, &agent_of(&v)?).await?;
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

/// The four parts an answer about a piece of code comes in (the UI reads them by these headings).
pub const ABOUT_PARTS: [&str; 4] = ["What it is", "What it does here", "Used elsewhere", "Worth knowing"];

/// What a person tapped in a conversation: a name on a line, a line or lines, or the change on one.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tapped<'a> {
    pub path: &'a str,
    pub line: Option<u64>,
    /// The last of the lines, when they're several.
    pub end: Option<u64>,
    pub selection: Option<&'a str>,
    pub change: bool,
    pub question: Option<&'a str>,
}

/// What a child is asked about a piece of code a person tapped, with `code`, the code around it.
pub fn about_prompt(t: &Tapped, code: &str) -> String {
    let path = t.path;
    let at = match (t.line, t.end.filter(|e| t.line.is_some_and(|l| l != *e))) {
        (Some(l), Some(e)) => format!("{path}:{}-{}", l.min(e), l.max(e)),
        (Some(l), None) => format!("{path}:{l}"),
        (None, _) => path.to_owned(),
    };
    let lines = if t.end.is_some_and(|e| t.line.is_some_and(|l| l != e)) { "lines" } else { "line" };
    let subject = match (t.change, t.selection) {
        (true, _) => format!("the change at {at}"),
        (false, Some(s)) => format!("`{s}` at {at}"),
        (false, None) => format!("{lines} {at}"),
    };
    let asked = t.question.map_or_else(|| "what is this?".to_owned(), |q| format!("{q:?}"));
    let parts = ABOUT_PARTS.iter().map(|p| format!("## {p}")).collect::<Vec<_>>().join("\n");
    format!(
        "A person reading a conversation about this repository tapped {subject} and asked: {asked}\n\nThe code there:\n```\n{code}\n```\n\nAnswer them in these four parts, each under its heading, briefly:\n{parts}\n\nUnder \"Used elsewhere\", each other place it's used, found by searching the repository, as `path:line` and that line. Read and search only: change nothing, and don't ask anything back."
    )
}

/// What a person asks about a piece of code in an agent's conversation (a name, a line, a change):
/// a child of the agent answers, reading the repository and changing nothing; when the agent runs
/// no subagents, the agent itself, after its turn or (`now`) between its steps.
async fn about(cx: Cx, v: Value) -> Out {
    let id = agent_of(&v)?;
    let info = known(&cx, &id)?;
    lineage::may_message(&cx.hub, &cx.caller, &info.id).await?;
    let path = text(&v, "path")?;
    let code = text(&v, "code")?;
    let opt = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let selection = opt("selection");
    let tapped = Tapped {
        path: &path,
        line: v.get("line").and_then(Value::as_u64),
        end: v.get("end_line").and_then(Value::as_u64),
        selection,
        change: v.get("change").and_then(Value::as_bool).unwrap_or(false),
        question: opt("question"),
    };
    let prompt = about_prompt(&tapped, &code);
    if info.max_subagents == Some(0) {
        let mode = if v.get("now").and_then(Value::as_bool).unwrap_or(false) { "steer" } else { "queue" };
        let sent = send(cx.clone(), json!({"agent": info.id, "text": prompt, "mode": mode})).await?;
        return Ok(json!({"agents": [], "itself": true, "sent": sent}));
    }
    let by = Starter { parent: Some(info.clone()), opener: cx.caller.opener(), actor: cx.caller.name.clone(), plan_run: None, asker: Some(cx.caller.clone()) };
    // Read-only, in the harnesses that have such a mode; the others start within what the agent has.
    let mode = match info.kind.as_str() {
        "claude" => Some("plan".to_owned()),
        "codex" => Some("read-only".to_owned()),
        _ => None,
    };
    let name = format!("About {}", selection.unwrap_or(&path).chars().take(48).collect::<String>());
    let ask = Ask { kind: info.kind.clone(), prompt: Some(prompt), mode, name: Some(name), helper: Some(super::about::ABOUT.to_owned()), ..Ask::default() };
    let infos = lineage::start(&cx.hub, &by, ask).await?;
    // It's the card's: ended once no card reads it (`super::about`).
    for i in &infos {
        super::about::started(cx.hub.clone(), i.id.clone(), cx.caller.name.clone());
    }
    Ok(json!({"agents": infos.iter().map(wait::brief).collect::<Vec<_>>(), "itself": false}))
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
    // Steering into a turn and stopping one are an agent's own to do; a queued message any agent
    // that works for the same person may send, its receiver told who sent it.
    let body = match lineage::may_message(&cx.hub, &cx.caller, &id).await? {
        lineage::Reach::Own => body,
        lineage::Reach::Peer if mode == SendMode::Queue => from_peer(&cx, &body),
        lineage::Reach::Peer => {
            return Err((StatusCode::FORBIDDEN, format!("an agent steers into a turn and sends now only to the agents it started (and theirs), and {} isn't one of them: send it a queued message instead", lineage::short(&id))));
        }
    };
    // With a key the message's id is made from it (and from who sends it, and to whom): a retry
    // with the same key is the same message, which its agent's inbox takes once (CP-T3.2). From an
    // agent, its id says who sent it too: the receiver's agents_wait hands it over.
    let made = match key_of(&v)? {
        Some(key) => keyed_id(&cx, "agents.send", &id, &key),
        None => crate::util::new_id(),
    };
    let message = match cx.caller.agent.as_deref() {
        Some(sender) => super::queued::from_agent_as(sender, &made),
        None => made,
    };
    wait::sent(&id, info.last_turn.as_ref().map(|t| t.at));
    match cx.hub.harness.command(&cx.hub, &id, Command::Send { id: message.clone(), text: body, mode, attachments: Vec::new(), overrides: None, reply_to: None, task: None }).await {
        Sent::Ok | Sent::Taken => Ok(json!({"sent": true, "message": message})),
        // Its machine is away: it goes once that's back, once.
        Sent::Parked => Ok(json!({"sent": true, "parked": true, "message": message})),
        Sent::Accepted(a) => Ok(super::registry::accepted(&message, &a)),
        Sent::Refused(r) => Err(refused(&r)),
        s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => Err(s.failure().unwrap_or((StatusCode::CONFLICT, "its runner refused it".to_owned()))),
        Sent::NoSession => Err((StatusCode::NOT_FOUND, format!("no agent {id}"))),
        Sent::Unreachable => Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now: try again once it's back", info.name))),
    }
}

/// Longest idempotency key.
pub const KEY_MAX: usize = 256;

/// A send's idempotency key (`key`; over HTTP the `Idempotency-Key` header too), when it has one.
pub fn key_of(v: &Value) -> Result<Option<String>, Fail> {
    let Some(k) = v.get("key").and_then(Value::as_str).map(str::trim).filter(|k| !k.is_empty()) else { return Ok(None) };
    if k.len() > KEY_MAX || k.chars().any(char::is_control) {
        return Err((StatusCode::BAD_REQUEST, format!("`key` is 1 to {KEY_MAX} bytes with no control characters")));
    }
    Ok(Some(k.to_owned()))
}

/// The id of the message `what` sends `agent` under `key`, for `cx`'s caller: the same on every
/// retry, and its own for each caller (another's key can't name its message).
pub fn keyed_id(cx: &Cx, what: &str, agent: &str, key: &str) -> String {
    crate::util::derive_id(&[what, &key_scope(&cx.caller), agent, key])
}

/// Whose an idempotency key is (CP-T3.2): an agent's by its id, as its name says the machine it's
/// on, which changes as it moves (a retry after a move is the same request); a person's by theirs;
/// else the caller's name.
pub fn key_scope(caller: &crate::authz::Caller) -> String {
    match (&caller.agent, &caller.user) {
        (Some(agent), _) => format!("agent:{agent}"),
        (None, Some(user)) => format!("user:{user}"),
        (None, None) => caller.name.clone(),
    }
}

/// What a session's inbox refusing a message is, answered.
pub fn refused(r: &super::inbox::Refusal) -> Fail {
    use super::inbox::Refusal as R;
    let status = match r {
        R::StaleEpoch { .. } | R::Behind { .. } | R::Moved { .. } | R::Reused { .. } => StatusCode::CONFLICT,
        R::Invalid { .. } => StatusCode::BAD_REQUEST,
        R::Full { .. } => StatusCode::TOO_MANY_REQUESTS,
        R::NoSession { .. } => StatusCode::NOT_FOUND,
        R::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, r.to_string())
}

/// A message from another agent that works for the same person, as its receiver reads it.
fn from_peer(cx: &Cx, text: &str) -> String {
    let me = cx.caller.agent.as_deref().unwrap_or_default();
    peer_text(cx.hub.harness.info(me).as_ref(), me, text)
}

/// How what kiln says of a fellow agent's message ends (`peer_text`), before the message itself.
const ANSWER_END: &str = "\"text\": …}.";

/// `text` from agent `me` (`sender`, as kiln lists it): who sent it, that it isn't from the
/// receiver's person, and how to answer.
fn peer_text(sender: Option<&Info>, me: &str, text: &str) -> String {
    let who = match sender {
        Some(s) => format!("Agent “{}” ({}, {} on {})", s.name, lineage::short(me), s.kind, wait::machine(&s.target)),
        None => format!("Agent {}", lineage::short(me)),
    };
    format!("{who}, which works for the same person as you, sent this message. It's from a fellow agent, not from your person. To answer, send it a message: agents_send {{\"agent\": \"{me}\", {ANSWER_END}\n\n{text}")
}

/// The words of a message from another agent: what follows what kiln says of a fellow agent
/// (`peer_text`), or all of it.
fn body_of(text: &str) -> &str {
    text.split_once("\n\n").filter(|(said, _)| said.starts_with("Agent ") && said.ends_with(ANSWER_END)).map_or(text, |(_, body)| body)
}

/// The words of a message from another agent ([`body_of`]), for its inbox: what kiln says first
/// changes as its sender does (renamed, or moved to another machine), and a retry under its key is
/// the same message. Any other message as it is.
pub fn peer_words(text: &str) -> &str {
    body_of(text)
}

/// The messages other agents sent the agent that waits, waiting behind the turn its wait began in
/// (`wait::mail`), handed over: each taken for this wait alone and out of its queue (one that can't
/// be stays, for after its turn), with who sent it and its words.
async fn hand_over(hub: &crate::hub::Hub, me: &wait::Waiter) -> Vec<Value> {
    let mut out = Vec::new();
    let mail = hub.harness.info(&me.id).map(|i| wait::mail(me, &i)).unwrap_or_default();
    if mail.is_empty() {
        return out;
    }
    for q in hub.harness.queue(&me.id).into_iter().filter(|q| mail.contains(&q.id)) {
        let Some(from) = super::queued::sender_of(&q.id).map(str::to_owned) else { continue };
        if !wait::claim(&q.id) || !hub.harness.command(hub, &me.id, Command::QueueRemove { id: q.id.clone() }).await.went() {
            continue;
        }
        let name = hub.harness.info(&from).map(|i| i.name);
        out.push(json!({"from": from, "name": name, "text": body_of(&q.text), "at": q.at}));
    }
    out
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
    for id in &ids {
        if cx.hub.harness.info(id).is_some() {
            lineage::may_see(&cx.hub, &cx.caller, id).await?;
        }
    }
    let until = match v.get("until").and_then(Value::as_str).unwrap_or("settled") {
        "settled" => Until::Settled,
        "change" => Until::Change,
        other => return Err((StatusCode::BAD_REQUEST, format!("`until` is settled or change, not {other:?}"))),
    };
    let secs = number(&v, "timeout_s").unwrap_or(wait::DEFAULT.as_secs_f64()).clamp(0.0, wait::MAX.as_secs_f64());
    // An agent waits in a turn of its own: what other agents sent it is handed over to that turn,
    // unless it says not to (a program's waits in code mode don't, unless they ask).
    let hands = v.get("hand_over").and_then(Value::as_bool).unwrap_or(true);
    let me = cx.caller.agent.as_deref().filter(|_| hands).map(|me| wait::Waiter::of(&cx.hub, me));
    let mut out = wait::wait(&cx.hub, &ids, until, flag(&v, "any"), std::time::Duration::from_secs_f64(secs), me.as_ref()).await;
    // What a parent saw settle here, it isn't told of again (`told`).
    for id in &ids {
        if let Some(info) = cx.hub.harness.info(id)
            && info.parent.is_some()
            && info.parent == cx.caller.agent
            && wait::settled(wait::stand(&info))
        {
            super::told::saw(&cx.hub, &info).await;
        }
    }
    // The messages other agents sent it: delivered here, not after its turn.
    if let Some(me) = &me {
        let handed = hand_over(&cx.hub, me).await;
        if !handed.is_empty()
            && let Some(o) = out.as_object_mut()
        {
            o.insert("messages".into(), json!(handed));
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
    let info = seen(&cx, &agent_of(&v)?).await?;
    let id = info.id.clone();
    // A card reading its helper keeps it (`super::about`).
    super::about::seen(&id);
    // Its parent reading it now isn't told of it again (`told`).
    if info.parent.is_some() && info.parent == cx.caller.agent {
        super::told::saw(&cx.hub, &info).await;
    }
    let (_, reply, changes) = cx.hub.harness.conclusion(&id).unwrap_or_default();
    // Its last words, from its timeline; else as lists keep them (shortened).
    let reply = reply.as_ref().and_then(reply_of).unwrap_or_else(|| json!({"text": info.summary, "shortened": true}));
    let record = crate::workspace::change::of(&cx.hub, &id).await;
    let files: Vec<Value> = changes
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            json!({"path": f.get("path"), "tree": f.get("tree"), "added": f.get("added"), "removed": f.get("removed"), "created": f.get("created").and_then(Value::as_bool).unwrap_or(false), "deleted": f.get("deleted").and_then(Value::as_bool).unwrap_or(false)})
        })
        .collect();
    Ok(json!({
        "agent": wait::brief(&info),
        "reply": reply,
        "last_turn": info.last_turn,
        "changes": {"files": files, "files_changed": info.files_changed, "lines_added": info.lines_added, "lines_removed": info.lines_removed},
        "cwd": info.cwd,
        // The working tree its work is in, when it went into one (a worktree): its files' paths are its.
        "tree": info.tree,
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
    may_steer(&cx, &info.id).await?;
    let want: crate::workspace::change::Want = serde_json::from_value(v).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))?;
    let r = crate::workspace::change::open(&cx.hub, &cx.caller.name, &info, want).await?;
    Ok(json!({"change_request": crate::workspace::change::shown(&r)}))
}

async fn workspace(cx: Cx, v: Value) -> Out {
    let info = seen(&cx, &agent_of(&v)?).await?;
    let Some(w) = info.workspace.clone() else {
        return Ok(json!({"workspace": Value::Null, "drift": []}));
    };
    let look = crate::workspace::change::ask_machine(&cx.hub, &info, &crate::workspace::Ask::Look).await?;
    Ok(json!({"workspace": {"kind": w.kind, "over": w.over, "sub": w.sub, "volume": w.made.as_ref().map(|m| &m.volume), "mount": w.made.as_ref().map(|m| &m.mount)}, "drift": look.get("drift").cloned().unwrap_or(json!([]))}))
}

async fn land(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    may_steer(&cx, &info.id).await?;
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
            if cx.hub.harness.info(&id).is_some() {
                lineage::may_see(&cx.hub, &cx.caller, &id).await?;
            }
            crate::workspace::change::of(&cx.hub, &id).await.iter().map(crate::workspace::change::shown).collect()
        }
        None => {
            let mut out = Vec::new();
            for r in crate::workspace::change::all(&cx.hub).await {
                if cx.hub.harness.info(&r.agent).is_none() || lineage::may_see(&cx.hub, &cx.caller, &r.agent).await.is_ok() {
                    out.push(crate::workspace::change::shown(&r));
                }
            }
            out
        }
    };
    Ok(json!({"changes": list}))
}

async fn interrupt(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    let id = info.id.clone();
    may_steer(&cx, &id).await?;
    match cx.hub.harness.command(&cx.hub, &id, Command::Interrupt).await {
        Sent::Ok | Sent::Accepted(_) | Sent::Taken | Sent::Parked => Ok(json!({"interrupted": true})),
        Sent::Refused(r) => Err(refused(&r)),
        s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => Err(s.failure().unwrap_or((StatusCode::CONFLICT, "its runner refused it".to_owned()))),
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
    may_steer(&cx, &id).await?;
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
    super::about::ended(&id);
    if end(&cx.hub, &id, &cx.caller.name).await {
        ended.push(id);
    }
    Ok(json!({"ended": ended}))
}

async fn children(cx: Cx, v: Value) -> Out {
    let id = seen(&cx, &agent_of(&v)?).await?.id;
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
            summary: "Start agents to do part of the work: `kind` (claude, codex, devin, gemini, opencode, goose, amp, antigravity), `on` a machine's name or what it needs (linux/amd64, macos, +gpu, cpus>=16; the idlest that suits and has the harness; by default your own machine), `cwd` (by default your folder, on your machine), `prompt` (the brief: what to do, what done looks like, what to report), `model`, `effort`, `mode` (at most your own: plan < ask < accept edits < auto < bypass), `name`, `count` (up to 16). An agent you start is your child: it gets no more than you have, and its approvals and questions go to a person, never to you. Carry on or end your turn: kiln tells you when it settles (its turn over, a person needed, stopped), then agents.result reads what it concluded; agents.wait when you can't go on without it.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "kind": {"type": "string", "description": "The harness: claude, codex, devin, gemini, opencode, goose, amp or antigravity."},
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
            name: "agents.about",
            summary: "Ask about a piece of code in an agent's conversation: a name, a line, or a change it made (`path`, `line`, `selection`, the `code` around it, `change`, and a `question` if not \"what is this?\"). A child of the agent answers in four parts (what it is, what it does here, where else it's used, what's worth knowing), reading the repository and changing nothing. When the agent runs no subagents (`max_subagents` 0) it answers itself: after its turn, or with `now` between its steps.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "path": {"type": "string", "description": "The file the code is in."},
                        "line": {"type": "integer", "description": "Its line there."},
                        "end_line": {"type": "integer", "description": "The last of the lines, when they're several."},
                        "selection": {"type": "string", "description": "The name tapped; none: the whole line."},
                        "code": {"type": "string", "description": "The code around it."},
                        "change": {"type": "boolean", "description": "It's about the change on that line."},
                        "question": {"type": "string"},
                        "now": {"type": "boolean", "description": "An agent answering itself does between its steps."}
                    }),
                    &["agent", "path", "code"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "about"]))),
            run: |cx, v| Box::pin(about(cx, v)),
        },
        Op {
            name: "agents.send",
            summary: "Send an agent a message. To an agent you started (and theirs): `queue` (after its turn, the default), `steer` (into the turn under way) or `now` (stopping it). To any other agent that works for the same person: `queue` only, and it's told the message is from you and how to answer you. Give a `key` to send it safely again: a call with the same key is the same message, which the agent gets once, and every such call is answered as the first was.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "text": {"type": "string"},
                        "mode": {"type": "string", "enum": ["queue", "steer", "now"]},
                        "key": {"type": "string", "description": "An idempotency key of yours, made once for this message (1 to 256 bytes): sent again with it (its answer lost), the message is the same one, delivered once. Without it, each call is another message: pass it whenever you may send it again."}
                    }),
                    &["agent", "text"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "send"]))),
            run: |cx, v| Box::pin(send(cx, v)),
        },
        Op {
            name: "agents.wait",
            summary: "Wait until agents settle (`until: settled`, the default: each is idle, needs a person, is paused for a usage limit, has stopped or has ended) or until anything about them changes (`until: change`), all of them or with `any` the first, up to `timeout_s` (default 300, at most 600; call again to wait longer). Returns where each stands. A message sent just before counts: an agent isn't settled until it has had its turn. Messages other agents send you don't wait behind your turn while you wait: it returns early once one is in your queue, with them in `messages` (who sent each, `from` and `name`, and its `text`), taken out of your queue since they're delivered now (a program's waits in code mode don't hand them over unless they pass `hand_over: true`, and then must return them). You needn't wait: kiln tells you when each agent you started settles; wait when you can't go on without it.",
            read_only: true,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "agent": {"type": "string", "description": AGENT},
                        "agents": {"type": "array", "items": {"type": "string"}, "description": "Several agents."},
                        "until": {"type": "string", "enum": ["settled", "change"]},
                        "any": {"type": "boolean", "description": "Return when the first of them is done, rather than all."},
                        "timeout_s": {"type": "number", "minimum": 0, "maximum": 600},
                        "hand_over": {"type": "boolean", "description": "Hand over the messages other agents sent you, taking them out of your queue (default true; false for a program's waits in code mode unless it says true)."}
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
    fn about_asks_what_was_tapped_in_the_four_parts_the_card_reads() {
        let path = "crates/kiln/src/http/client.rs";
        let name = about_prompt(&Tapped { path, line: Some(48), selection: Some("next"), ..Tapped::default() }, "let wait = backoff.next(now);");
        assert!(name.contains("`next` at crates/kiln/src/http/client.rs:48") && name.contains("what is this?") && name.contains("backoff.next(now)"), "{name}");
        for part in ABOUT_PARTS {
            assert!(name.contains(&format!("## {part}")), "{part}: {name}");
        }
        assert!(name.contains("change nothing"), "{name}");
        let change = about_prompt(&Tapped { path: "a.rs", line: Some(3), selection: Some("x"), change: true, question: Some("why this?"), ..Tapped::default() }, "-a\n+b");
        assert!(change.contains("the change at a.rs:3") && change.contains("\"why this?\""), "{change}");
        assert!(about_prompt(&Tapped { path: "a.rs", ..Tapped::default() }, "x").contains("line a.rs"));
        // Several lines, either way round, read as one stretch.
        let lines = about_prompt(&Tapped { path: "a.rs", line: Some(48), end: Some(44), ..Tapped::default() }, "x");
        assert!(lines.contains("lines a.rs:44-48"), "{lines}");
        assert!(about_prompt(&Tapped { path: "a.rs", line: Some(4), end: Some(4), ..Tapped::default() }, "x").contains("line a.rs:4 "));
    }

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

    /// CP-T3.2: a key is its caller's: an agent's by its id, the same on whichever machine it runs
    /// (its name says the machine); a person's by theirs, whatever they're called.
    #[test]
    fn a_key_is_whose_it_is_wherever_they_call_from() {
        let caller = |name: &str, agent: Option<&str>, user: Option<&str>| {
            let mut c = crate::authz::Caller::owner();
            name.clone_into(&mut c.name);
            c.agent = agent.map(str::to_owned);
            c.user = user.map(str::to_owned);
            c
        };
        assert_eq!(key_scope(&caller("agent:01a1@box-a for admin", Some("01a1"), None)), key_scope(&caller("agent:01a1@box-b for admin", Some("01a1"), None)));
        assert_eq!(key_scope(&caller("ada", None, Some("u1"))), key_scope(&caller("Ada L", None, Some("u1"))));
        assert_ne!(key_scope(&caller("x", Some("01a1"), None)), key_scope(&caller("x", None, Some("01a1"))), "an agent's key isn't a person's");
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
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
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
            // Another agent can't steer or end them (it may queue them a message: they work for
            // the same person); their parent can, and ends them with it.
            let other = parent(&hub, "default", true).await?;
            let stranger = as_agent(&hub, &other).await?;
            let Err((s, _)) = call(stranger.clone(), "agents.send", json!({"agent": kid, "text": "stop that", "mode": "steer"})).await else { anyhow::bail!("a stranger steered a child") };
            anyhow::ensure!(s == http::StatusCode::FORBIDDEN);
            let Err((s, _)) = call(stranger, "agents.stop", json!({"agent": kid})).await else { anyhow::bail!("a stranger ended a child") };
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
    fn an_agent_messages_any_agent_of_its_person_and_steers_only_its_own() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            // Two agents a person started apart: neither started the other.
            let a = parent(&hub, "default", true).await?;
            let b = parent(&hub, "default", true).await?;
            let cx = as_agent(&hub, &a).await?;
            // A queued message goes through (b's harness can't run here: what matters is that kiln
            // didn't refuse it), told as from a.
            let sent = call(cx.clone(), "agents.send", json!({"agent": b, "text": "which commit has the fix?"})).await;
            anyhow::ensure!(!matches!(sent, Err((http::StatusCode::FORBIDDEN, _))), "a queued message to a fellow agent: {sent:?}");
            // Steering into its turn, sending now, interrupting and ending it are its own's to do.
            for (op, input) in [
                ("agents.send", json!({"agent": b, "text": "x", "mode": "steer"})),
                ("agents.send", json!({"agent": b, "text": "x", "mode": "now"})),
                ("agents.interrupt", json!({"agent": b})),
                ("agents.stop", json!({"agent": b})),
            ] {
                let Err((s, why)) = call(cx.clone(), op, input).await else { anyhow::bail!("{op} on a fellow agent went through") };
                anyhow::ensure!(s == http::StatusCode::FORBIDDEN, "{op}: {s} {why}");
            }
            anyhow::ensure!(hub.harness.info(&b).is_some(), "b still runs");
            // An agent that works for someone else isn't sent to.
            let c = parent(&hub, "default", true).await?;
            hub.store.kv_set(crate::mcp::opener_key(&c), "user:someone-else".to_owned()).await?;
            let Err((s, why)) = call(cx.clone(), "agents.send", json!({"agent": c, "text": "hello"})).await else { anyhow::bail!("sent to another person's agent") };
            anyhow::ensure!(s == http::StatusCode::FORBIDDEN && why.contains("works for someone else"), "{s} {why}");
            // A person still sends to any agent, in any mode.
            let person = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let sent = call(person, "agents.send", json!({"agent": b, "text": "go on", "mode": "steer"})).await;
            anyhow::ensure!(!matches!(sent, Err((http::StatusCode::FORBIDDEN, _))), "a person steering: {sent:?}");
            Ok(())
        })
    }

    /// An agent kiln runs on the server whose harness keeps every line it's sent, in `heard`.
    #[cfg(unix)]
    async fn recording(hub: &Arc<Hub>, dir: &std::path::Path) -> anyhow::Result<(String, std::path::PathBuf)> {
        use std::os::unix::fs::PermissionsExt;
        let heard = dir.join("heard.jsonl");
        let program = dir.join("recording-claude");
        std::fs::write(&program, format!("#!/bin/sh\nexec cat >> '{}'\n", heard.display()))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
        let id = hub.harness.open(hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no agent"))?;
        // Its harness running, its adapter started.
        for _ in 0..500 {
            if hub.harness.local.get(&id).is_some_and(|s| s.pid.lock().is_some() && !s.core.lock().meta.features.is_empty()) {
                return Ok((id, heard));
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("the recording harness never started")
    }

    /// The prompts a recording harness was sent, by their message ids.
    #[cfg(unix)]
    fn prompts(heard: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(heard)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type") == Some(&json!("user")))
            .filter_map(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// CP-T3.2, T-T3.2.1: `agents_send` called twice with one key: one prompt reaches the harness,
    /// and both calls get its answer (the second marked a repeat). Without a key, two calls are two
    /// messages.
    #[cfg(unix)]
    #[test]
    fn agents_send_twice_with_one_key_is_one_prompt_and_both_get_its_answer() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (id, heard) = recording(&hub, dir.path()).await?;
            let person = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let send = |key: Option<&str>| {
                let mut input = json!({"agent": id, "text": "deploy the fix"});
                if let (Some(k), Some(o)) = (key, input.as_object_mut()) {
                    o.insert("key".into(), json!(k));
                }
                call(person.clone(), "agents.send", input)
            };
            let first = send(Some("deploy-1")).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let again = send(Some("deploy-1")).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let message = first.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
            anyhow::ensure!(!message.is_empty() && again.get("message") == first.get("message"), "the same message: {first} {again}");
            // The same answer, the first time's, the second marked a repeat.
            let answer = |v: &Value, k: &str| v.pointer(&format!("/accepted/{k}")).cloned();
            for k in ["seq", "at", "epoch", "session", "message"] {
                anyhow::ensure!(answer(&first, k).is_some() && answer(&first, k) == answer(&again, k), "{k}: {first} {again}");
            }
            anyhow::ensure!(answer(&first, "repeat").is_none() && answer(&again, "repeat") == Some(json!(true)), "{first} {again}");
            // One prompt reached the harness, and no second copy waits behind it.
            for _ in 0..500 {
                if !prompts(&heard).is_empty() {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            anyhow::ensure!(prompts(&heard) == [message.clone()], "the harness was sent {:?}", prompts(&heard));
            let queued: Vec<String> = hub.harness.local.get(&id).map(|s| s.core.lock().state.queue.iter().map(|q| q.id.clone()).collect()).unwrap_or_default();
            anyhow::ensure!(!queued.contains(&message), "a copy waits in its queue: {queued:?}");
            // Without a key, each call is a message of its own.
            let a = send(None).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let b = send(None).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(a.get("message") != b.get("message") && b.pointer("/accepted/repeat").is_none(), "{a} {b}");
            // A key reused for other words is refused, saying so.
            let refused = call(person.clone(), "agents.send", json!({"agent": id, "text": "something else", "key": "deploy-1"})).await;
            anyhow::ensure!(matches!(&refused, Err((http::StatusCode::CONFLICT, why)) if why.contains("already used")), "{refused:?}");
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.1: an agent's agents_send under a key, retried under it after the agent
    /// moved to another machine (its calls come through that machine's runner now, under another
    /// name), is the same message: answered as a repeat, never a second prompt.
    #[test]
    fn a_keyed_send_retried_after_its_sender_moved_is_the_same_message() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let to = parent(&hub, "default", true).await?;
            let from = parent(&hub, "default", true).await?;
            let before = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &from, Some("box-a")).await.map_err(anyhow::Error::msg)? };
            let after = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &from, Some("box-b")).await.map_err(anyhow::Error::msg)? };
            let input = json!({"agent": to, "text": "deploy the fix", "key": "deploy-1"});
            let first = call(before, "agents.send", input.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let again = call(after, "agents.send", input).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let taken = inbox.taken(&to, 100).await?;
            anyhow::ensure!(
                again.get("message") == first.get("message") && again.pointer("/accepted/repeat") == Some(&json!(true)) && taken.len() == 1,
                "a retry under its key after its sender moved was another message: {first} then {again}; its agent's inbox took {taken:?}"
            );
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.1: an agent's plans.run under a key (its steps, on itself), asked for again
    /// under the key after the agent moved to another machine, is the same run: its messages reach
    /// it once.
    #[test]
    fn a_keyed_plan_run_retried_after_its_agent_moved_is_the_same_run() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let me = parent(&hub, "default", true).await?;
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let before = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &me, Some("box-a")).await.map_err(anyhow::Error::msg)? };
            let after = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &me, Some("box-b")).await.map_err(anyhow::Error::msg)? };
            let input = json!({"id": p.id, "mode": "steps", "key": "tidy-1"});
            let first = call(before, "plans.run", input.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let again = call(after, "plans.run", input).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let taken = inbox.taken(&me, 100).await?;
            anyhow::ensure!(again.get("run") == first.get("run") && taken.len() == 2, "asked again under its key after its agent moved: {first} then {again}; its inbox took {} messages: {taken:?}", taken.len());
            Ok(())
        })
    }

    /// CP-T3.2 (the bar: typed refusals, never silent drops): a key is one request. plans.run asked
    /// again under its key for other steps, or in another mode, is refused 409, saying so, and
    /// sends nothing; so is a run of one agent per step asked again under its key for other steps.
    #[test]
    fn a_key_given_to_another_plan_run_is_refused() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let me = parent(&hub, "default", true).await?;
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let first = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "only": [1], "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            for other in [json!({"id": p.id, "session": me, "mode": "steps", "only": [2], "key": "k"}), json!({"id": p.id, "session": me, "mode": "message", "only": [1], "key": "k"})] {
                let refused = call(person.clone(), "plans.run", other.clone()).await;
                anyhow::ensure!(matches!(&refused, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "{other} after {first}: {refused:?}");
            }
            let taken = inbox.taken(&me, 100).await?;
            anyhow::ensure!(taken.len() == 1, "only step 1's message went: {taken:?}");
            // The same request again is the same run, answered as it was.
            let again = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "only": [1], "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(again == first, "{first} then {again}");
            // One agent per step: the same key, other steps.
            let cwd = tempfile::tempdir()?;
            let new = json!({"on": "local", "kind": "claude", "cwd": cwd.path().to_string_lossy()});
            call(person.clone(), "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "only": [1], "key": "a"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let refused = call(person, "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "only": [2], "key": "a"})).await;
            anyhow::ensure!(matches!(&refused, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "{refused:?}");
            Ok(())
        })
    }

    /// CP-T3.2 (the fourth adversarial check's): a key is one request whatever its mode: a key
    /// given to a run of one agent per step is refused (409) for the same plan run as steps on an
    /// agent, and one given to a run as steps is refused for one agent per step.
    #[test]
    fn a_key_given_to_a_plan_run_in_one_mode_is_refused_in_another() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let me = parent(&hub, "default", true).await?;
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let cwd = tempfile::tempdir()?;
            let new = json!({"on": "local", "kind": "claude", "cwd": cwd.path().to_string_lossy()});
            call(person.clone(), "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let other = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "key": "k"})).await;
            anyhow::ensure!(matches!(&other, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "the same key in another mode ran: {other:?}");
            call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "key": "j"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let other = call(person, "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "key": "j"})).await;
            anyhow::ensure!(matches!(&other, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "the same key in another mode ran: {other:?}");
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.4: a message the server sends for an epoch its agent has left is refused
    /// by the agent's inbox, naming the epoch it's at; the server learns it, sends it again for that
    /// epoch, once, and it's taken. Sent again after, it's a repeat.
    #[test]
    fn a_message_for_an_epoch_its_agent_left_goes_again_for_the_one_it_is_at() -> anyhow::Result<()> {
        use crate::harness::registry::Sent;
        run(async {
            let hub = hub_with(|_| {}).await?;
            let id = parent(&hub, "default", true).await?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("the server's sessions have no inboxes"))?;
            // Its inbox has gone on to epoch 3 (as a session that moved back here would have it);
            // the server last heard 0.
            inbox.open(&id, 3).await?;
            anyhow::ensure!(hub.harness.epoch_of(&id) == 0);
            let send = || crate::harness::model::Command::Send { id: "m1".into(), text: "hello".into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let sent = hub.harness.command(&hub, &id, send()).await;
            anyhow::ensure!(matches!(&sent, Sent::Accepted(a) if a.epoch == 3 && !a.repeat), "{sent:?}");
            anyhow::ensure!(hub.harness.epoch_of(&id) == 3, "the server knows the epoch it's at now");
            let again = hub.harness.command(&hub, &id, send()).await;
            anyhow::ensure!(matches!(&again, Sent::Accepted(a) if a.epoch == 3 && a.repeat), "{again:?}");
            Ok(())
        })
    }

    #[test]
    fn a_message_from_a_fellow_agent_says_who_sent_it_and_how_to_answer() {
        let sender = crate::harness::session::Info { name: "Mantle".into(), kind: "claude".into(), target: "agent:adas-macbook-pro-2.local".into(), ..Default::default() };
        let me = "01a1488745a977f58e4224bc796f3308";
        let text = super::peer_text(Some(&sender), me, "Which commit adds the embedding API?");
        assert!(text.starts_with("Agent “Mantle” (01a14887, claude on adas-macbook-pro-2.local), which works for the same person as you"), "{text}");
        assert!(text.contains("not from your person"), "{text}");
        assert!(text.contains(&format!("agents_send {{\"agent\": \"{me}\"")), "{text}");
        assert!(text.ends_with("\n\nWhich commit adds the embedding API?"), "{text}");
        // One kiln no longer lists still says who, by id.
        assert!(super::peer_text(None, me, "hi").starts_with("Agent 01a14887,"));
        // A wait hands it over as its words, without what kiln says of its sender.
        assert_eq!(super::body_of(&text), "Which commit adds the embedding API?");
        assert_eq!(super::body_of(&super::peer_text(None, me, "First line.\n\nSecond.")), "First line.\n\nSecond.");
        assert_eq!(super::body_of("Rebase on main, then go on.\n\nThanks."), "Rebase on main, then go on.\n\nThanks.", "a parent's own words, as they are");
    }

    /// Until `f` holds (the registry hears from the sessions' machines a moment later).
    #[cfg(unix)]
    async fn until(f: impl Fn() -> bool, what: &str) -> anyhow::Result<()> {
        for _ in 0..500 {
            if f() {
                return Ok(());
            }
            crate::rt::sleep(Duration::from_millis(20)).await;
        }
        anyhow::bail!("{what}: never")
    }

    /// The incident, whole. A coordinator spends one long turn (its harness took its first message
    /// and never ends the turn) while its three helpers, on a machine of their own, settle again
    /// and again; two fellow agents send it messages; its goal stack is resumed now and then and
    /// its first goal said met. When it waits for a helper still at work, the fellow agents'
    /// messages come back with the wait and leave its queue. What's left there for when its turn
    /// ends is one message of news, each helper's latest line in it once, and its goal under way,
    /// once.
    #[test]
    #[cfg(unix)]
    fn a_coordinator_in_one_long_turn_ends_it_with_one_message_of_news_its_goal_once_and_its_mail_handed_over() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        use crate::harness::core::Update;
        use crate::harness::model::{State, Status, TurnEnd};
        use crate::harness::registry::Location;
        use crate::harness::session::Info;
        run(async {
            let hub = hub_with(|c| c.conversations.off = true).await?;
            // A stand-in for Claude Code that takes what it's sent and never answers.
            let dir = tempfile::tempdir()?.keep();
            let busy = dir.join("busy-claude");
            std::fs::write(&busy, "#!/bin/sh\nexec cat > /dev/null\n")?;
            std::fs::set_permissions(&busy, std::fs::Permissions::from_mode(0o755))?;
            let spec = Spec {
                kind: "claude".into(),
                name: Some("Coordinator".into()),
                cwd: Some(dir.to_string_lossy().into_owned()),
                program: Some(busy.to_string_lossy().into_owned()),
                prompt: Some("Work the program's goals with your helpers.".into()),
                ..Spec::default()
            };
            let p = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no coordinator"))?;
            until(|| hub.harness.info(&p).is_some_and(|i| i.status == Status::Running), "the coordinator's turn").await?;
            let me = as_agent(&hub, &p).await?;
            // Two agents of the same person, apart from it: they only send.
            let (a, b) = (parent(&hub, "default", true).await?, parent(&hub, "default", true).await?);
            // Its helpers, as their machine reports them.
            let there = Location::Machine("helpers".into());
            let kids: Vec<String> = (0..3).map(|_| crate::util::new_id()).collect();
            for (n, k) in kids.iter().enumerate() {
                let info = Info { id: k.clone(), kind: "claude".into(), name: format!("Helper {}", n + 1), parent: Some(p.clone()), status: Status::Running, ..Info::default() };
                hub.harness.ingest(&hub, &there, k, Update::Open { info: Box::new(info), since: None });
            }
            let report = |k: &str, status: Status, said: String, at: i64| {
                let state = State { status, summary: said, last_turn: (at > 0).then(|| TurnEnd { at, how: "done".into(), ..TurnEnd::default() }), ..State::default() };
                hub.harness.ingest(&hub, &there, k, Update::State { state: Box::new(state) });
            };
            let queue = || hub.harness.queue(&p);
            let news = || queue().into_iter().filter(|q| q.text.contains("which you started")).collect::<Vec<_>>();
            let goals = || queue().into_iter().filter(|q| q.text.starts_with("Your goal")).count();
            // Its goals: the first goes behind its turn at once.
            crate::goals::add(&hub, &p, vec!["the lab program passes".into(), "the view loads in 300 ms".into()], true).await?;
            until(|| goals() == 1, "its first goal, queued").await?;
            let mut now = 1_792_000_000_000_i64;
            crate::harness::told::tick(&hub, &hub.harness.list(), now, &[], &[]).await;
            for round in 0..6 {
                // Each helper goes to work and ends a turn, and stands so a while.
                for k in &kids {
                    report(k, Status::Running, String::new(), 0);
                }
                now += 1_000;
                crate::harness::told::tick(&hub, &hub.harness.list(), now, &[], &[]).await;
                for (n, k) in kids.iter().enumerate() {
                    report(k, Status::Idle, format!("Helper {} finished round {round}.", n + 1), now + i64::try_from(n).unwrap_or(0));
                }
                now += 1_000;
                crate::harness::told::tick(&hub, &hub.harness.list(), now, &[], &[]).await;
                now += 4_000;
                crate::harness::told::tick(&hub, &hub.harness.list(), now, &[], &[]).await;
                let latest = |q: &crate::harness::model::QueueItem| (1..=3).all(|n| q.text.contains(&format!("Helper {n} finished round {round}.")));
                until(|| news().iter().any(latest), "the round's news, waiting").await?;
                anyhow::ensure!(news().len() == 1, "round {round}: one message of news, not {:?}", news());
                match round {
                    // Fellow agents write.
                    1 => {
                        call(as_agent(&hub, &a).await?, "agents.send", json!({"agent": p, "text": "The registry's tests pass on my side."})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                    }
                    3 => {
                        call(as_agent(&hub, &b).await?, "agents.send", json!({"agent": p, "text": "I'm holding fedora until 14:00."})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                    }
                    _ => {}
                }
                // Its stack resumed (its goal sent again, behind the turn); its first goal met.
                if round % 2 == 0 {
                    crate::goals::control(&hub, &p, crate::goals::Action::Start).await?;
                }
                if round == 4 {
                    crate::goals::done(&hub, &p, None, Some("the lab program passed"), &crate::goals::By::Itself).await?;
                    until(|| queue().iter().any(|q| q.text.starts_with("Your goal: the view loads")), "its next goal").await?;
                }
                anyhow::ensure!(goals() == 1, "round {round}: its goal once, not {}: {:?}", goals(), queue());
                // It read the second helper's result: that line comes out of the news (and back once it settles again).
                if round == 2 {
                    let read = call(me.clone(), "agents.result", json!({"agent": kids.get(1)})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                    anyhow::ensure!(read.get("agent").is_some(), "{read}");
                    until(|| news().iter().all(|q| !q.text.contains("Helper 2 finished")), "the line it saw, out of the news").await?;
                }
            }
            until(|| queue().iter().filter(|q| crate::harness::queued::sender_of(&q.id).is_some()).count() == 2, "the fellow agents' messages, waiting").await?;
            // The third helper goes back to work; it waits for it, and the wait returns at once with its mail.
            if let Some(k) = kids.get(2) {
                report(k, Status::Running, String::new(), 0);
            }
            // (A wait that says not to, as a program's in code mode does, hands nothing over.)
            let program = call(me.clone(), "agents.wait", json!({"agent": kids.get(2), "timeout_s": 1, "hand_over": false})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(program.get("timed_out") == Some(&json!(true)) && program.get("messages").is_none(), "{program}");
            anyhow::ensure!(queue().iter().filter(|q| crate::harness::queued::sender_of(&q.id).is_some()).count() == 2, "{:?}", queue());
            let waited = call(me.clone(), "agents.wait", json!({"agent": kids.get(2), "timeout_s": 30})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(waited.get("timed_out") == Some(&json!(false)) && waited.pointer("/agents/0/stand") == Some(&json!("working")), "{waited}");
            let mail: Vec<(String, String)> = waited.get("messages").and_then(Value::as_array).into_iter().flatten().map(|m| (m.get("from").and_then(Value::as_str).unwrap_or("").to_owned(), m.get("text").and_then(Value::as_str).unwrap_or("").to_owned())).collect();
            anyhow::ensure!(mail == [(a.clone(), "The registry's tests pass on my side.".to_owned()), (b.clone(), "I'm holding fedora until 14:00.".to_owned())], "{waited}");
            until(|| queue().iter().all(|q| crate::harness::queued::sender_of(&q.id).is_none()), "the messages handed over, out of its queue").await?;
            // What waits for the end of its turn: one message of news, each helper's latest line once, and its goal once.
            let left = queue();
            let news = news();
            anyhow::ensure!(left.len() == 2 && news.len() == 1 && goals() == 1, "{left:?}");
            let text = news.first().map(|q| q.text.clone()).unwrap_or_default();
            anyhow::ensure!(text.matches("which you started").count() == 3, "each helper once: {text}");
            for n in 1..=3 {
                anyhow::ensure!(text.contains(&format!("Helper {n} finished round 5.")), "helper {n}'s latest: {text}");
                anyhow::ensure!(!text.contains(&format!("Helper {n} finished round 4.")), "helper {n}'s older line: {text}");
            }
            anyhow::ensure!(left.iter().any(|q| q.text.starts_with("Your goal: the view loads in 300 ms")), "{left:?}");
            Ok(())
        })
    }

    #[test]
    fn an_agent_held_to_fewer_subagents_starts_no_more_through_kiln() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?.keep();
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.to_string_lossy().into_owned()),
                mode: Some("plan".into()),
                program: Some(dir.join("no-claude").to_string_lossy().into_owned()),
                max_subagents: Some(1),
                ..Spec::default()
            };
            let infos = hub.harness.open(&hub, "local", spec, 1).await?;
            let id = infos.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no parent"))?;
            for _ in 0..200 {
                if hub.harness.info(&id).is_some_and(|i| i.max_subagents == Some(1)) {
                    break;
                }
                crate::rt::sleep(Duration::from_millis(20)).await;
            }
            let cx = as_agent(&hub, &id).await?;
            // Its settings say one at once.
            let two = call(cx, "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE, "count": 2})).await;
            anyhow::ensure!(two.as_ref().is_err_and(|(_, m)| m.contains("its settings")), "{two:?}");
            Ok(())
        })
    }

    /// CP-T3.2: a message from another agent is its words, for its inbox: renamed or moved between a
    /// send and its retry under the same key, its sender's message is the same one (a repeat, not
    /// another message under its id).
    #[test]
    fn a_fellow_agents_message_is_its_words_whatever_kiln_says_of_its_sender() {
        let me = "01a1488745a977f58e4224bc796f3308";
        let on = |name: &str, machine: &str| crate::harness::session::Info { name: name.into(), kind: "claude".into(), target: format!("agent:{machine}"), ..Default::default() };
        let first = super::peer_text(Some(&on("Mantle", "adacachy")), me, "Which commit?\n\nThe one with the API.");
        let again = super::peer_text(Some(&on("Mantle T10", "fedora")), me, "Which commit?\n\nThe one with the API.");
        assert_ne!(first, again);
        assert_eq!(super::peer_words(&first), "Which commit?\n\nThe one with the API.");
        assert_eq!(super::peer_words(&first), super::peer_words(&again));
        assert_eq!(super::peer_words(&super::peer_text(None, me, "hi")), "hi");
        // A person's message is as it is, whatever it says.
        for text in ["Agent Smith is here.\n\nHello", "hello", "Agent \n\n"] {
            assert_eq!(super::peer_words(text), text);
        }
    }

    #[test]
    fn bypass_needs_the_parent_s_own_and_chains_and_children_are_bounded() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|c| {
                c.agents.max_depth = 1;
                c.agents.max_children = Some(1);
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
