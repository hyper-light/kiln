//! The agents operations (`agents.*`, docs/agents.md "Agents that run agents"), in kiln's one
//! operation table (`crate::ops::own`): each is an HTTP route under `/api/harness/agents`, an MCP
//! tool (`agents_start`, …), a code-mode function (`kiln.agents.start`) and, through a kiln shell's
//! socket, a CLI command (`kiln spawn --on`, `kiln agents wait`, `kiln agents result`).
//!
//! An agent that calls them is the parent of what it starts (`Caller::agent`, from how it reached
//! kiln), which gets no more than it has (`super::lineage`). An agent interrupts and ends
//! only the agents it started (and theirs); it may send or steer a message to any agent that works
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
use crate::ops::{Cx, Fail, Input, Op, Out, path, text};

fn agent_of(v: &Value) -> Result<String, Fail> {
    text(v, "agent")
}

/// The agent `id` names: its id, or the start of it (8 characters, as lists shorten it).
pub(crate) fn known(cx: &Cx, id: &str) -> Result<Info, Fail> {
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
    let keep = |i: &Info| {
        // A card's helper (`super::about`) is the card's: no agent of these.
        i.helper.is_none()
            && machine.as_deref().is_none_or(|m| wait::machine(&i.target) == m || (m == "server" && i.target == "local"))
            && harness.as_deref().is_none_or(|h| i.kind == h)
            && parent.as_deref().is_none_or(|p| i.parent.as_deref() == Some(p))
            && run.as_deref().is_none_or(|r| i.plan_run.as_deref() == Some(r))
    };
    // Whoever sees every session reads each info as its owner shares it, none copied (goal 9, plan 01a1a87b2cec step 5);
    // anyone else, those `lineage::seen` lets them.
    let infos: Vec<std::sync::Arc<Info>> = if lineage::sees_all(&cx.caller) {
        cx.hub.harness.list_shared().into_iter().filter(|i| keep(i)).collect()
    } else {
        lineage::seen(&cx.hub, &cx.caller, cx.hub.harness.list()).await.into_iter().filter(|i| keep(i)).map(std::sync::Arc::new).collect()
    };
    let wanted = |b: &Value| state.as_deref().is_none_or(|st| b.get("stand").and_then(Value::as_str) == Some(st) || b.get("status").and_then(Value::as_str) == Some(st));
    let agents = briefs(infos, wait::brief, wanted).await;
    // `{"agents": [...]}` with the briefs moved in: `json!` would serialize them into a new tree, a copy of every
    // brief on the shard (goal 10, plan 01a1a87b2dae step 5).
    let mut out = serde_json::Map::new();
    out.insert("agents".into(), Value::Array(agents));
    Ok(Value::Object(out))
}

/// The briefs `make` makes (`wait::brief`) of `infos` whose brief `wanted` wants, in their order, made on the compute
/// pool (`crate::rt::compute_map`, an agent a unit, its thread given up between any two where another job waits) and
/// handed back to the shard in one hop (goal 10, plan 01a1a87b2dae step 5). Made on the request's shard, `agents_list`
/// held it 1.2 ms a call made at once (g11alloc2's calm rounds), and 0.31 ms a poll at the mean made a brief at a
/// time with its turn taken after each (rt9's): a turn taken after a brief ends past the quantum where the brief
/// does. Where the pool can't take them, they're made here.
async fn briefs(infos: Vec<std::sync::Arc<Info>>, make: fn(&Info) -> Value, wanted: impl Fn(&Value) -> bool) -> Vec<Value> {
    let made = match crate::rt::compute_map(infos.clone(), move |i: std::sync::Arc<Info>| make(&i)).await {
        Ok(made) => made,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "the agents' briefs weren't made off the shard: made here");
            infos.iter().map(|i| make(i)).collect()
        }
    };
    made.into_iter().filter(|b| wanted(b)).collect()
}

/// What a state says that `agents.get` leaves out: catalogs and lists only a console needs.
const HEAVY: &[&str] = &["skills", "plugins", "mcp", "harness_status", "delivery", "control_values", "inventory", "limits", "stop_waived"];

async fn get(cx: Cx, v: Value) -> Out {
    let info = seen(&cx, &agent_of(&v)?).await?;
    let id = info.id.clone();
    // Its state alone: nothing of its events is read for it (goal 11 audit A2(c)).
    let mut state = cx.hub.harness.state_value(&id).unwrap_or_default();
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
    // A start that waits for room on its machine answers when it's queued, with its place and why,
    // and goes on starting by itself: waiting for room, it outlasted its caller's own wait, which
    // gave up with no answer and its agents unknown (2026-11-11: "the kiln server didn't answer in
    // 11 minutes").
    let (queued, heard) = lineage::Queued::new();
    let mut ask = ask;
    ask.queued = queued;
    let hub = std::sync::Arc::clone(&cx.hub);
    let started = crate::rt::spawn(async move {
        let r = lineage::start(&hub, &by, ask).await;
        if let Err((_, why)) = &r {
            tracing::warn!(actor = %by.actor, %why, "an agent start ended without starting it");
        }
        r
    });
    let heard = async { heard.await.ok() };
    futures::pin_mut!(heard);
    match futures::future::select(started, heard).await {
        futures::future::Either::Left((Ok(started), _)) => {
            let infos = started?;
            Ok(json!({"agents": infos.iter().map(wait::brief).collect::<Vec<_>>(), "parent": parent}))
        }
        futures::future::Either::Left((Err(_), _)) => Err((StatusCode::INTERNAL_SERVER_ERROR, "the start ended before it answered".into())),
        futures::future::Either::Right((Some(words), _)) => Ok(json!({"agents": [], "parent": parent, "queued": format!("{words}; it starts there as soon as there's room, and agents_list shows it then")})),
        // The notice went without a word (none was queued): the start's own answer.
        futures::future::Either::Right((None, started)) => {
            let infos = started.await.map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "the start ended before it answered".to_owned()))??;
            Ok(json!({"agents": infos.iter().map(wait::brief).collect::<Vec<_>>(), "parent": parent}))
        }
    }
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
    let words = text(&v, "text")?;
    let status = status_of(&v)?;
    let body = words.clone();
    // CP-T3.2: agent-to-agent messages enter the turn under way instead of waiting behind it.
    let default_mode = if cx.caller.agent.is_some() { "steer" } else { "queue" };
    let mode: SendMode = match v.get("mode").and_then(Value::as_str).unwrap_or(default_mode) {
        "queue" => SendMode::Queue,
        "steer" => SendMode::Steer,
        "now" => SendMode::Now,
        other => return Err((StatusCode::BAD_REQUEST, format!("`mode` is queue, steer or now, not {other:?}"))),
    };
    let info = known(&cx, &id)?;
    let id = info.id.clone();
    // A message may enter a fellow agent's turn, with its sender named, but cannot stop the turn.
    let body = match lineage::may_message(&cx.hub, &cx.caller, &id).await? {
        lineage::Reach::Own => body,
        lineage::Reach::Peer if mode != SendMode::Now => from_peer(&cx, &body),
        lineage::Reach::Peer => {
            return Err((StatusCode::FORBIDDEN, format!("an agent sends now only to the agents it started (and theirs), and {} isn't one of them: send it a steered or queued message instead", lineage::short(&id))));
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
    // An agent's words, its call relayed signed by its runner and taken: kiln passes them on too,
    // its record and the ones before it going with them, for the recipient's runner to check end to
    // end (docs/ci.md W27-R08's relays).
    let relayed = match cx.caller.agent.as_deref().and_then(|me| cx.hub.relays.carried(me, &words)) {
        Some(c) => crate::relays::words(&cx.hub, &id, c).await,
        None => None,
    };
    let command = Command::Send { id: message.clone(), text: body, mode, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
    let owner = mail_owner(&cx).await;
    let sender = key_scope(&cx.caller);
    let provenance = relayed.map(|(relayed, seq, digest)| json!({"relayed": relayed, "seq": seq, "digest": digest}));
    // Fault injection belongs after durable custody, at wire delivery. The ledger keeps these
    // exact original words and their original proof, even when a test corrupts the transmission.
    let receipt = cx.hub.harness.command_published(&cx.hub, &id, command, provenance, owner, sender, status).await.map_err(mail_failed)?;
    wait::sent(&id, info.last_turn.as_ref().map(|t| t.at));
    publication_answer(&receipt)
}

fn publication_answer(receipt: &super::mail::Receipt) -> Out {
    // `sent` already covered offline forwarding. Keep those public aliases while the receipt
    // states custody precisely; a receiver's `accepted` answer cannot be inferred from it.
    #[derive(serde::Serialize)]
    struct Published<'a> {
        sent: bool,
        retained: bool,
        message: &'a str,
        #[serde(flatten)]
        publication: &'a super::mail::Receipt,
    }
    serde_json::to_value(Published { sent: true, retained: true, message: &receipt.id, publication: receipt }).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

fn status_of(v: &Value) -> Result<Option<super::mail::StatusKey>, Fail> {
    let Some(status) = v.get("status").filter(|v| !v.is_null()) else { return Ok(None) };
    let status: super::mail::StatusKey = serde_json::from_value(status.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid explicit status scope: {e}")))?;
    if status.key.is_empty() || status.key.chars().any(char::is_control) || status.task.as_deref().is_some_and(|s| s.is_empty() || s.chars().any(char::is_control)) {
        return Err((StatusCode::BAD_REQUEST, "status key/task are nonempty labels without control characters".into()));
    }
    Ok(Some(status))
}

async fn mail_owner(cx: &Cx) -> String {
    if let Some(agent) = &cx.caller.agent && let Some(owner) = crate::mcp::acts_for(&cx.hub, agent).await { return owner; }
    cx.caller.user.as_ref().map(|u| format!("user:{u}")).or_else(|| cx.caller.opener()).unwrap_or_else(|| key_scope(&cx.caller))
}

fn mail_failed(error: super::mail::Error) -> Fail {
    (error.status_code(), serde_json::to_string(&error).unwrap_or_else(|_| "communication custody failed".into()))
}

fn mail_number(v: &Value, key: &str) -> Result<Option<u64>, Fail> {
    let Some(value) = v.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    value.as_u64().or_else(|| value.as_str().and_then(|s| s.parse().ok())).map(Some).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("{key} must be a nonnegative integer")))
}

fn mail_budget(v: &Value) -> Result<usize, Fail> {
    let bytes = mail_number(v, "bytes")?.unwrap_or(super::state_view::FRAME as u64);
    usize::try_from(bytes).ok().filter(|n| *n > 0 && *n <= super::state_view::FRAME).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("bytes must fit the {} byte control frame", super::state_view::FRAME)))
}

async fn mail_scope(cx: &Cx, v: &Value) -> Result<(String, String, String), Fail> {
    let recipient = known(cx, &agent_of(v)?)?.id;
    lineage::may_message(&cx.hub, &cx.caller, &recipient).await?;
    let own_sender = key_scope(&cx.caller);
    let sender = v.get("from").and_then(Value::as_str).unwrap_or(&own_sender).to_owned();
    // Outgoing history is the sender's. Reading another sender's incoming history requires
    // permission to see the recipient's conversation, not merely permission to send it a note.
    if sender != own_sender { lineage::may_see(&cx.hub, &cx.caller, &recipient).await?; }
    if sender.is_empty() || sender.chars().any(char::is_control) { return Err((StatusCode::BAD_REQUEST, "from must be an authenticated sender identity".into())); }
    Ok((mail_owner(cx).await, sender, recipient))
}

async fn messages(cx: Cx, v: Value) -> Out {
    let (owner, sender, recipient) = mail_scope(&cx, &v).await?;
    let status = match v.get("status_key").and_then(Value::as_str) {
        Some(key) => Some(super::mail::StatusKey { task: v.get("task").and_then(Value::as_str).map(str::to_owned), key: key.to_owned() }),
        None if v.get("task").is_some() => return Err((StatusCode::BAD_REQUEST, "task requires status_key".into())),
        None => None,
    };
    let page = super::mail::history(&cx.hub.store, owner, sender, recipient, status, mail_number(&v, "after")?.unwrap_or(0), mail_budget(&v)?).await.map_err(mail_failed)?;
    serde_json::to_value(page).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn message_body(cx: Cx, v: Value) -> Out {
    let (owner, sender, recipient) = mail_scope(&cx, &v).await?;
    let revision = mail_number(&v, "revision")?.ok_or_else(|| (StatusCode::BAD_REQUEST, "revision is required".into()))?;
    let offset = usize::try_from(mail_number(&v, "offset")?.unwrap_or(0)).map_err(|_| (StatusCode::BAD_REQUEST, "offset exceeds this platform's range".into()))?;
    let read = super::mail::BodyRead { revision, path: v.get("path").and_then(Value::as_str).unwrap_or("").to_owned(), offset };
    let reply = super::mail::body(&cx.hub.store, owner, sender, recipient, read, mail_budget(&v)?).await.map_err(mail_failed)?;
    serde_json::to_value(reply).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
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
    let sender = cx.hub.harness.info(me);
    let on = sender.as_ref().map(|s| cx.hub.shown(wait::machine(&s.target)));
    peer_text(sender.as_ref().zip(on.as_deref()), me, text)
}

/// How what kiln says of a fellow agent's message ends (`peer_text`), before the message itself.
const ANSWER_END: &str = "\"text\": …}.";

/// `text` from agent `me` (`sender`, as kiln lists it, with the name people see of its machine): who
/// sent it, that it isn't from the receiver's person, and how to answer.
fn peer_text(sender: Option<(&Info, &str)>, me: &str, text: &str) -> String {
    let who = match sender {
        Some((s, on)) => format!("Agent “{}” ({}, {} on {on})", s.name, lineage::short(me), s.kind),
        None => format!("Agent {}", lineage::short(me)),
    };
    format!("{who}, which works for the same person as you, sent this message. It's from a fellow agent, not from your person. To answer, send it a message: agents_send {{\"agent\": \"{me}\", {ANSWER_END}\n\n{text}")
}

/// The words of a message from another agent: what follows what kiln says of a fellow agent
/// (`peer_text`), or all of it.
pub(crate) fn body_of(text: &str) -> &str {
    text.split_once("\n\n").filter(|(said, _)| said.starts_with("Agent ") && said.ends_with(ANSWER_END)).map_or(text, |(_, body)| body)
}

/// The words of a message from another agent ([`body_of`]), for its inbox: what kiln says first
/// changes as its sender does (renamed, or moved to another machine), and a retry under its key is
/// the same message. Any other message as it is.
pub fn peer_words(text: &str) -> &str {
    body_of(text)
}

fn clear_handoff(out: &mut Value) {
    if let Some(object) = out.as_object_mut() { object.remove("messages"); object.remove("mail_handoff"); }
}

fn stage_handoff(out: &mut Value, handoff: &str, turn: Option<i64>, messages: Vec<Value>) -> Result<String, super::state_reads::Failure> {
    use super::state_reads::Failure;
    let digest = wait::handoff_digest(handoff, turn, &messages)?;
    let object = out.as_object_mut().ok_or(Failure::Malformed)?;
    if object.contains_key("messages") || object.contains_key("mail_handoff") { return Err(Failure::Malformed); }
    object.insert("messages".into(), Value::Array(messages));
    object.insert("mail_handoff".into(), json!({"id": handoff, "turn": turn}));
    if super::state_view::encode(out, super::state_view::FRAME).is_err() {
        clear_handoff(out);
        return Err(Failure::Source(super::state_view::Error::Budget));
    }
    Ok(digest)
}

/// Register one exact response while the source retains its queue bodies and input leases.
/// Neither this HTTP return nor a source registration ACK proves native tool-result delivery.
async fn hand_over(hub: &crate::hub::Hub, me: &wait::Waiter, out: &mut Value) -> Result<(), super::state_reads::Failure> {
    use super::state_reads::Failure;
    let Some(info) = hub.harness.info(&me.id) else { return Ok(()) };
    let mail = super::state_reads::mail(hub, me, &info).await?;
    if mail.is_empty() { return Ok(()); }
    let revision = info.queue_revision.as_ref().ok_or(Failure::NotReady)?;
    let (mut messages, mut ids, mut claims) = (Vec::new(), Vec::new(), Vec::new());
    let mut files_wait = false;
    for id in mail {
        let Some(observed) = super::state_reads::item(hub, &me.id, &info, &id).await? else { continue };
        if observed.revision.as_ref() != Some(revision) { return Err(Failure::NotReady); }
        let q = observed.value;
        // A text-only native result cannot prove delivery of attached files.
        if !q.attachments.is_empty() { files_wait = true; continue; }
        let Some(from) = super::queued::sender_of(&q.id).filter(|from| *from != me.id).map(str::to_owned) else { continue };
        let Some(claim) = wait::claim_for(me, &q.id) else { continue };
        let name = hub.harness.info(&from).map(|i| i.name);
        messages.push(json!({"id": q.id, "from": from, "name": name, "text": body_of(&q.text), "at": q.at}));
        ids.push(q.id);
        claims.push(claim);
    }
    if messages.is_empty() { return if files_wait { Err(Failure::Source(super::state_view::Error::Unreadable)) } else { Ok(()) }; }
    let handoff = crate::util::new_id();
    let digest = stage_handoff(out, &handoff, me.turn(), messages)?;
    let result = super::state_reads::register_handoff(hub, &me.id, revision.clone(), handoff, me.turn(), ids, digest).await;
    if result.is_err() { clear_handoff(out); }
    drop(claims);
    result
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
    // Only registered mail is returned. The source still owns each original until it proves
    // native result delivery; failed or lost registration never becomes queue removal.
    if let Some(me) = &me && let Err(error) = hand_over(&cx.hub, me, &mut out).await && let Some(object) = out.as_object_mut() { object.insert("mail_handoff_error".into(), json!(error)); }
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
    // Its last reply found by its place, read off the shard where its chunk is compact or spilled (goal 11 audit A2(c)).
    let (reply, changes) = (cx.hub.harness.reply(&id).await, cx.hub.harness.changes_value(&id));
    // Its last words, from its timeline; else as lists keep them (shortened).
    let reply = reply.as_ref().and_then(reply_of).unwrap_or_else(|| json!({"text": info.summary, "shortened": true}));
    let record = match crate::workspace::change::of(&cx.hub, &id).await {
        Some(r) => Some(crate::workspace::change::fresh(&cx.hub, r).await),
        None => None,
    };
    let files: Vec<Value> = changes
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            // Who changed it and whose it is: its counts are this project's files only.
            json!({"path": f.get("path"), "tree": f.get("tree"), "added": f.get("added"), "removed": f.get("removed"), "created": f.get("created").and_then(Value::as_bool).unwrap_or(false), "deleted": f.get("deleted").and_then(Value::as_bool).unwrap_or(false), "by": f.get("by"), "place": f.get("place").and_then(Value::as_str).unwrap_or("project")})
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

/// Its repository's branches, tags and latest commits (docs/agents.md, "Compare").
async fn refs(cx: Cx, v: Value) -> Out {
    let info = seen(&cx, &agent_of(&v)?).await?;
    crate::workspace::change::ask_machine(&cx.hub, &info, &crate::workspace::Ask::Refs).await
}

/// A point as `agents.compare` takes it: `now`, `began`, `agent:<who>`, else a branch, tag or
/// commit (`ref:<name>` for one named like those).
fn point(p: &str) -> crate::workspace::git::Point {
    use crate::workspace::git::Point;
    match p {
        "now" => Point::Now,
        "began" => Point::Began,
        _ => match p.strip_prefix("agent:") {
            Some(by) => Point::Agent { by: by.to_owned() },
            None => Point::Ref { name: p.strip_prefix("ref:").unwrap_or(p).to_owned() },
        },
    }
}

/// What changes from one point to another in its repository (docs/agents.md, "Compare").
async fn compare_points(cx: Cx, v: Value) -> Out {
    let info = seen(&cx, &agent_of(&v)?).await?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let (Some(from), Some(to)) = (s("from"), s("to")) else {
        return Err((StatusCode::BAD_REQUEST, "`from` and `to`: each a point (`now`, `began`, `agent:<who>`, or a branch, tag or commit)".to_owned()));
    };
    let split = s("split").is_none_or(|x| x != "false" && x != "0");
    let since_head = cx.hub.harness.state(&info.id).and_then(|st| st.started_head);
    let ask = crate::workspace::Ask::Compare { from: point(&from), to: point(&to), split, path: s("path"), since_head };
    crate::workspace::change::ask_machine(&cx.hub, &info, &ask).await
}

/// Its branch's commits, its session's own after the commit it began at (else made after it
/// began), and what `picks` change together (docs/agents.md, "Commits").
async fn commits(cx: Cx, v: Value) -> Out {
    let info = seen(&cx, &agent_of(&v)?).await?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
    let picks: Vec<String> = s("picks").map(|p| p.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned).collect()).unwrap_or_default();
    let since_head = cx.hub.harness.state(&info.id).and_then(|st| st.started_head);
    let ask = crate::workspace::Ask::Commits { branch: s("branch"), trunk: s("trunk"), since_head, since_at: Some(info.created_at), picks };
    crate::workspace::change::ask_machine(&cx.hub, &info, &ask).await
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
            match crate::workspace::change::of(&cx.hub, &id).await {
                Some(r) => vec![crate::workspace::change::shown(&crate::workspace::change::fresh(&cx.hub, r).await)],
                None => Vec::new(),
            }
        }
        None => {
            let mut out = Vec::new();
            for r in crate::workspace::change::all(&cx.hub).await {
                if cx.hub.harness.info(&r.agent).is_none() || lineage::may_see(&cx.hub, &cx.caller, &r.agent).await.is_ok() {
                    out.push(crate::workspace::change::shown(&crate::workspace::change::fresh(&cx.hub, r).await));
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
        Sent::Published(receipt) => Ok(json!({"publication": receipt})),
        Sent::PublicationRefused(error) => Err(mail_failed(error)),
        Sent::Ok | Sent::Accepted(_) | Sent::Taken | Sent::Parked => Ok(json!({"interrupted": true})),
        Sent::Refused(r) => Err(refused(&r)),
        s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => Err(s.failure().unwrap_or((StatusCode::CONFLICT, "its runner refused it".to_owned()))),
        Sent::NoSession => Err((StatusCode::NOT_FOUND, format!("no agent {id}"))),
        Sent::Unreachable => Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now", info.name))),
    }
}

async fn resume(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    may_steer(&cx, &info.id).await?;
    super::relocate::resume(&cx.hub, &info.id).await.map_err(|e| (StatusCode::CONFLICT, e))
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
            input: Input::of::<input::AgentsList>(),
            route: |_| Ok((Method::GET, path(&["api", "harness", "agents"]))),
            run: |cx, v| Box::pin(list(cx, v)),
        },
        Op {
            name: "agents.get",
            summary: "One agent: where it stands, its info and its state (status, approvals waiting on a person, queue, tasks, to-dos, usage, how its last turn ended).",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsGet>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?]))),
            run: |cx, v| Box::pin(get(cx, v)),
        },
        Op {
            name: "agents.start",
            summary: "Start agents to do part of the work: `kind` (claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor, pi), `on` what it needs (linux/amd64, macos, +gpu, cpus>=16: kiln's load balancer places it on a machine that suits and has the harness, by room, measured speed and where such work last went well; don't pick one yourself to spread agents out) or a machine's name only when it must run there (its files, its hardware); by default your own machine, `cwd` (by default your folder, on your machine), `prompt` (the brief: what to do, what done looks like, what to report), `model`, `effort`, `mode` (at most your own: plan < ask < accept edits < auto < bypass), `name`, `count` (up to 16), `isolation` (`microvm`: a microVM of its own, on a machine that starts them) and `image` (the Agentfile it's made from: kiln's, `harness-<harness>`, or one of yours, `agentfile:<id>`), `resume` (a conversation's id, to take it up again). An agent you start is your child: it gets no more than you have, and its approvals and questions go to a person, never to you. Carry on or end your turn: kiln tells you when it settles (its turn over, a person needed, stopped), then agents.result reads what it concluded; agents.wait when you can't go on without it.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsStart>(),
            route: |_| Ok((Method::POST, path(&["api", "harness", "agents"]))),
            run: |cx, v| Box::pin(start(cx, v)),
        },
        Op {
            name: "agents.about",
            summary: "Ask about a piece of code in an agent's conversation: a name, a line, or a change it made (`path`, `line`, `selection`, the `code` around it, `change`, and a `question` if not \"what is this?\"). A child of the agent answers in four parts (what it is, what it does here, where else it's used, what's worth knowing), reading the repository and changing nothing. When the agent runs no subagents (`max_subagents` 0) it answers itself: after its turn, or with `now` between its steps.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsAbout>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "about"]))),
            run: |cx, v| Box::pin(about(cx, v)),
        },
        Op {
            name: "agents.send",
            summary: "Send an agent a distinct instruction, decision request or result: steer into its turn (agents' default), queue after it (people's default), or now to stop a turn of an agent you started. Same-person peers may receive steered/queued messages and are told who sent them. Give a new immutable key per publication; retry with that key and exact content for the same receipt. Only explicitly replaceable progress may set status:{key,task?}: latest never-dispatched status replaces prior pending presentation in that stream, while every accepted original stays in agents.messages. A dispatched revision is frozen until actual consumption. Publish progress when it changes what the recipient can do; do not send repeated unchanged checkpoints. Instructions and final results use ordinary sends. A durable waiting receipt means retained custody, not model delivery; typed backpressure/storage failure means no acceptance and you retain the original.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsSend>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "send"]))),
            run: |cx, v| Box::pin(send(cx, v)),
        },
        Op {
            name: "agents.messages",
            summary: "Read byte-bounded pages of your accepted original communications to an agent, including superseded explicit statuses. after is the prior page cursor; status_key/task optionally select one status stream. from defaults to your authenticated sender identity; reading another sender requires permission to see the recipient's conversation. A too-large revision can be inspected with agents.message_body.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsMessages>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "messages"]))),
            run: |cx, v| Box::pin(messages(cx, v)),
        },
        Op {
            name: "agents.message_body",
            summary: "Inspect an immutable communication revision by JSON pointer (empty path starts at its envelope; /command/text reads its words). Objects page keys, arrays expose length, strings return UTF-8 ranges with next offsets. bytes bounds the complete reply; original content is never clipped or replaced. Uses the same recipient/sender authorization as agents.messages.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsMessageBody>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "message-body"]))),
            run: |cx, v| Box::pin(message_body(cx, v)),
        },
        Op {
            name: "agents.wait",
            summary: "Wait until agents settle (`until: settled`, the default: idle, needing a person, paused, stopped or ended) or anything about them changes (`until: change`), all of them or with `any` the first. `timeout_s` defaults to 300, at most 600. A message sent just before counts until its agent has had its turn. Your peer mail may return early in `messages`, with exact `id`, sender `from`/`name`, `text` and `at`, plus `mail_handoff`. The source keeps each original queued until it proves the exact native tool result reached your turn; an HTTP return does not consume mail. Missing proof and attachment-bearing messages remain for normal delivery. Code-mode waits hand over only with `hand_over: true`; return the complete messages and marker unchanged. You needn't wait: kiln tells you when an agent you started settles. Wait when you can't continue without it.",
            // Not read-only: it takes delivery of the waiter's own messages, which then live only in
            // its answer. A client may run a read-only tool beside others, again or ahead of time and
            // drop the answer; this one's would be lost with it.
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsWait>(),
            route: |_| Ok((Method::POST, path(&["api", "harness", "agents", "wait"]))),
            run: |cx, v| Box::pin(wait_for(cx, v)),
        },
        Op {
            name: "agents.result",
            summary: "What an agent concluded: its last reply, how its last turn ended, what it changed (files, lines added and removed), its folder and machine, its cost, and its change request and branch (agents.change) once it has one.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsResult>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "result"]))),
            run: |cx, v| Box::pin(result(cx, v)),
        },
        Op {
            name: "agents.children",
            summary: "The agents still running that an agent started, and theirs.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsChildren>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "children"]))),
            run: |cx, v| Box::pin(children(cx, v)),
        },
        Op {
            name: "agents.interrupt",
            summary: "Stop the turn an agent you started is in (it stays, ready for the next message).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsInterrupt>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "interrupt"]))),
            run: |cx, v| Box::pin(interrupt(cx, v)),
        },
        Op {
            name: "agents.resume",
            summary: "Resume a crashed agent in place (its stand `crashed`: its harness gone while its machine is up): a fresh harness process on its machine, under its id, continuing its conversation, its plans, goal stack and subagents still its own (the subagents that crashed with it resumed too).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsInterrupt>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "sessions", &agent_of(v)?, "resume"]))),
            run: |cx, v| Box::pin(resume(cx, v)),
        },
        Op {
            name: "agents.change",
            summary: "Open a change request from an agent's work (yours, or an agent's you started), or push a new version of the one it has: the files it changed, committed on its folder's last commit as `agents/<what it's about>-<id>` (the folder's branch, index and working tree untouched) and pushed for review, its author the agent, for its person. Into kiln's own review for a repository kiln hosts; as a pull request for one on GitHub, GitLab or Bitbucket (the branch pushed, and the request opened when kiln has an integration there, else a link to open it). Comments, requested changes, failing checks (with `fix_checks`) and the land queue then come back to the agent as messages. `title` (else what the agent is about), `body`, `draft` (a draft on every host, new versions staying one), `ready` (its draft marked ready for review, with a new version or nothing new), `trunk` (else the repository's default), `answers` (the threads a new version answers, in kiln's own review). Its answer says whether it's a `draft`.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsChange>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "change"]))),
            run: |cx, v| Box::pin(change(cx, v)),
        },
        Op {
            name: "agents.workspace",
            summary: "An agent's workspace of its own (a Slates volume over its folder): the folder it's laid over, its volume and mount, and the files under it that changed on disk after the agent changed them (drift).",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsWorkspace>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "workspace"]))),
            run: |cx, v| Box::pin(workspace(cx, v)),
        },
        Op {
            name: "agents.commits",
            summary: "An agent's branch's commits (or `branch`'s) since its trunk (`trunk`, else its remote's default branch), newest first, the ones its session made marked, and what isn't committed yet; with `picks` (commits, comma-separated, `now` for what isn't committed yet), what they change together: a run of them from the first one's parent to the last, or, with gaps, each applied in order onto the first one's parent beside the folder (one that doesn't go without a commit left out says which). Asked of its machine, in the working tree its work is in.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsCommits>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "commits"]))),
            run: |cx, v| Box::pin(commits(cx, v)),
        },
        Op {
            name: "agents.refs",
            summary: "An agent's repository's branches (its own, then its remote's), tags and latest commits, and its trunk: what Compare's From and To offer besides the session's own points. Asked of its machine.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsRefs>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "refs"]))),
            run: |cx, v| Box::pin(refs(cx, v)),
        },
        Op {
            name: "agents.compare",
            summary: "What changes in an agent's repository from one point to another. `from` and `to` are each `now` (its folder as it is, untracked files too), `began` (as its session began), `agent:<who>` (`main`, the agent itself, or a subagent run: its files as it left them) or a branch, tag or commit (`ref:<name>` for one named like those). `split` (on unless `false`): from where they split, as git's three-dot diff does; never between two agents' versions, which say of each file which of them changed it (`changed_by`). `path`: one file's lines. A large compare sends counts first (`counts_only`). Asked of its machine.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsCompare>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "agents", &agent_of(v)?, "compare"]))),
            run: |cx, v| Box::pin(compare_points(cx, v)),
        },
        Op {
            name: "agents.land",
            summary: "Write an agent's workspace of its own back to the folder it's laid over. Without `manifest`: the plan (what it would create, replace and delete, and any file changed on disk since, which it won't overwrite), for a person to read. With the plan's `manifest`: written exactly as that plan says, or, if anything changed since, nothing written and the new plan back (`changed`) — only a person can write, never an agent. Build output and what git ignores stay out, and so does `.git`.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::AgentsLand>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "land"]))),
            run: |cx, v| Box::pin(land(cx, v)),
        },
        Op {
            name: "agents.changes",
            summary: "Agents' change requests: whose, where (repository, branch, the change and its page), its versions, review and checks, and whether it landed. `agent` for one.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::AgentsChanges>(),
            route: |_| Ok((Method::GET, path(&["api", "harness", "changes"]))),
            run: |cx, v| Box::pin(changes(cx, v)),
        },
        Op {
            name: "agents.stop",
            summary: "End an agent you started for good (its changes stay on disk); with `children`, the agents it started (and theirs) too.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::AgentsStop>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "stop"]))),
            run: |cx, v| Box::pin(stop(cx, v)),
        },
        Op {
            name: "agents.credential",
            summary: "An agent's own credential for git, for a day: its pushes to kiln's repositories go as the agent, for whom it acts. Only the agent itself is given one, which its runner asks for as its git needs it.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::AgentsCredential>(),
            route: |v| Ok((Method::POST, path(&["api", "harness", "agents", &agent_of(v)?, "credential"]))),
            run: |cx, v| Box::pin(credential(cx, v)),
        },
    ]
}

/// An agent's own token for git (docs/ci.md W27-R03): its pushes to kiln's repositories go as the
/// agent, for whom it acts, never as them. Only the agent itself is given one (its runner asks as
/// it, telling it by its processes): for a day, to fetch, push and submit as git does, and no more
/// than its person may (nor, started for an attachment, than the attachment's capabilities give).
async fn credential(cx: Cx, v: Value) -> Out {
    let info = known(&cx, &agent_of(&v)?)?;
    if cx.caller.agent.as_deref() != Some(info.id.as_str()) {
        return Err((StatusCode::FORBIDDEN, format!("{}'s credential for git is its own: only it is given one, through its runner", info.name)));
    }
    let Some(opener) = crate::mcp::acts_for(&cx.hub, &info.id).await else {
        return Err((StatusCode::FORBIDDEN, format!("kiln doesn't know whom {} acts for", info.name)));
    };
    let grants: Vec<String> = match crate::ci::attach::limit_for(&cx.hub, &info.id).await {
        Some(limit) => limit.grants.iter().map(crate::authz::Grant::text).collect(),
        None => ["git.read", "diffs.submit", "diffs.land"].map(str::to_owned).to_vec(),
    };
    let grants: Vec<&str> = grants.iter().map(String::as_str).collect();
    let (_, token) = crate::authz::tokens::mint_agent(&cx.hub, &info.id, &opener, &grants, Some(1), &format!("{}'s git", info.name))
        .await
        .map_err(|e| (StatusCode::FORBIDDEN, format!("a credential for {}: {e:#}", info.name)))?;
    Ok(json!({"token": token, "expires_at": crate::util::now_unix() + 86_400}))
}

/// What each of these operations takes (`crate::ops::Input`): its JSON Schema is generated from
/// its type, each field's description from its doc comment.
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsList {
        pub machine: Option<String>,
        pub harness: Option<String>,
        pub state: Option<String>,
        /// Only the agents this agent started.
        pub parent: Option<String>,
        pub plan_run: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsGet {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentsStartWorkspace {
        Folder,
        Volume,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentsStartIsolation {
        None,
        Microvm,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsStart {
        /// The harness: claude, codex, devin, gemini, opencode, goose, amp, antigravity, cursor or pi.
        pub kind: String,
        /// What it needs: linux/amd64, macos, arm64, +gpu, cpus>=16, mem>=32g (kiln's load
        /// balancer places it); or a machine's name (local for the server), only when it must run
        /// there.
        pub on: Option<String>,
        /// Its folder there (a path, or ~/…).
        pub cwd: Option<String>,
        /// Its first message: the task.
        pub prompt: Option<String>,
        pub model: Option<String>,
        pub effort: Option<String>,
        /// Its permission mode, at most yours (claude: plan, default, acceptEdits, auto,
        /// bypassPermissions; codex: read-only, untrusted, auto, full-access). Left out: its
        /// harness's default when that's within yours, else the most that is.
        pub mode: Option<String>,
        pub name: Option<String>,
        #[schemars(range(min = 1, max = 16))]
        pub count: Option<i64>,
        /// Where it works: `folder`, its folder itself (the default), or `volume`, a Slates volume
        /// of its own laid over the folder's repository, where what it changes stays its own until
        /// it goes up for review (agents.change) or a person writes it to disk. Where the machine
        /// has Slates.
        pub workspace: Option<AgentsStartWorkspace>,
        /// How it's held: `none`, a process on its machine (the default), or `microvm`, a microVM of
        /// its own, on a machine that starts them. An isolated agent's agents are isolated too. An
        /// isolated agent's `on` sizes its microVM (`cpus=2,mem=4g`: its vCPUs and memory, else its
        /// parent's), which kiln's balancer asks and holds for it from its start to its end; its
        /// commands are refused, never run on a bare host.
        pub isolation: Option<AgentsStartIsolation>,
        /// The Agentfile its microVM is made from: kiln's own for its harness (`harness-<harness>`,
        /// as agentfiles_list's `builtin` names them) or one of yours (`agentfile:<id>`). Naming one
        /// holds it in a microVM of its own; an isolated agent's agents are made from its Agentfile
        /// unless they name another.
        pub image: Option<String>,
        /// The conversation to take up again: its harness's own session id (an agent's
        /// `harness_session`, as agents_get gives it), in its folder on its machine; one agent.
        pub resume: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsAbout {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        /// The file the code is in.
        pub path: String,
        /// Its line there.
        pub line: Option<i64>,
        /// The last of the lines, when they're several.
        pub end_line: Option<i64>,
        /// The name tapped; none: the whole line.
        pub selection: Option<String>,
        /// The code around it.
        pub code: String,
        /// It's about the change on that line.
        pub change: Option<bool>,
        pub question: Option<String>,
        /// An agent answering itself does between its steps.
        pub now: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentsSendMode {
        Queue,
        Steer,
        Now,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsSend {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        pub text: String,
        /// Steer into the turn under way by default for agents; queue after it by default for people.
        pub mode: Option<AgentsSendMode>,
        /// An idempotency key of yours, made once for this message (1 to 256 bytes): sent again
        /// with it (its answer lost), the message is the same one, delivered once. Without it, each
        /// call is another message: pass it whenever you may send it again.
        pub key: Option<String>,
        /// Explicitly replaceable progress only. This is separate from the immutable retry key;
        /// task is a workflow label, not source task routing or additional permission.
        pub status: Option<super::super::mail::StatusKey>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsMessages {
        pub agent: String,
        pub from: Option<String>,
        pub status_key: Option<String>,
        pub task: Option<String>,
        pub after: Option<u64>,
        pub bytes: Option<usize>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsMessageBody {
        pub agent: String,
        pub from: Option<String>,
        pub revision: u64,
        pub path: Option<String>,
        pub offset: Option<usize>,
        pub bytes: Option<usize>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(rename_all = "snake_case")]
    pub enum AgentsWaitUntil {
        Settled,
        Change,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsWait {
        /// The agent's id (as agents.list gives it).
        pub agent: Option<String>,
        /// Several agents.
        pub agents: Option<Vec<String>>,
        pub until: Option<AgentsWaitUntil>,
        /// Return when the first of them is done, rather than all.
        pub any: Option<bool>,
        #[schemars(range(min = 0, max = 600))]
        pub timeout_s: Option<f64>,
        /// Hand over the messages other agents sent you, taking them out of your queue (default
        /// true; false for a program's waits in code mode unless it says true).
        pub hand_over: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsResult {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsChildren {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsInterrupt {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsChange {
        /// The agent whose work it is (as agents.list gives it); leave it out for your own.
        pub agent: Option<String>,
        pub title: Option<String>,
        /// What it changes and why, for reviewers.
        pub body: Option<String>,
        /// Opened as a draft, not ready for review yet, on kiln, GitHub, GitLab and Bitbucket alike;
        /// its new versions stay a draft until `ready`.
        pub draft: Option<bool>,
        /// The branch it goes into.
        pub trunk: Option<String>,
        /// Failing checks go to the agent to fix (the default).
        pub fix_checks: Option<bool>,
        /// The threads of its change this new version fixes (kiln's own review, by their ids): the
        /// version is an agent's fix of theirs, and each says which version fixed it.
        pub threads: Option<Vec<String>>,
        /// Marks its draft ready for review, with a new version or with nothing new to push.
        pub ready: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsWorkspace {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsCommits {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        /// Commits to see together, comma-separated (`now`: what isn't committed yet).
        #[serde(default)]
        pub picks: Option<String>,
        /// The branch its commits are counted from (else its remote's default branch).
        #[serde(default)]
        pub trunk: Option<String>,
        /// Another of its repository's branches to list (else the one checked out).
        #[serde(default)]
        pub branch: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsRefs {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsCompare {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        /// Where from: `now`, `began`, `agent:<who>`, or a branch, tag or commit.
        pub from: String,
        /// Where to, as `from`.
        pub to: String,
        /// From where they split (git's three-dot): on unless `false`.
        #[serde(default)]
        pub split: Option<String>,
        /// One file's lines alone.
        #[serde(default)]
        pub path: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsLand {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        /// The plan's manifest hash: what is written is exactly what that plan lists.
        pub manifest: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsChanges {
        /// The agent's id (as agents.list gives it).
        pub agent: Option<String>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsStop {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
        /// End the agents it started (and theirs) too.
        pub children: Option<bool>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct AgentsCredential {
        /// The agent's id (as agents.list gives it).
        pub agent: String,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 10, plan 01a1a87b2dae step 5: `agents.list` makes its agents' briefs on the compute pool, none on the shard
    /// that asks for them, and has them back in one hop: each made once, the agents it gives, their order and its
    /// filter on what each brief says as they were made at once.
    #[test]
    fn g10_s5_agents_list_makes_its_briefs_off_the_shard() -> anyhow::Result<()> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static MADE: AtomicUsize = AtomicUsize::new(0);
        static ON_SHARD: AtomicUsize = AtomicUsize::new(0);
        fn counted(i: &Info) -> Value {
            MADE.fetch_add(1, Ordering::Relaxed);
            if crate::rt::on_shard() {
                ON_SHARD.fetch_add(1, Ordering::Relaxed);
            }
            wait::brief(i)
        }
        let info = |n: usize| Info { id: format!("a{n}"), kind: if n.is_multiple_of(2) { "claude" } else { "codex" }.into(), ..Info::default() };
        let infos: Vec<Arc<Info>> = (0..5).map(|n| Arc::new(info(n))).collect();
        let at_once: Vec<Value> = infos.iter().map(|i| wait::brief(i)).collect();
        let claude: Vec<Value> = at_once.iter().filter(|b| b.get("kind") == Some(&json!("claude"))).cloned().collect();
        let (all, some, none) = crate::rt::block_on(async move {
            let all = briefs(infos.clone(), counted, |_: &Value| true).await;
            let some = briefs(infos.clone(), counted, |b: &Value| b.get("kind") == Some(&json!("claude"))).await;
            let none = briefs(infos, counted, |_: &Value| false).await;
            Ok((all, some, none))
        })?;
        assert_eq!(all, at_once, "the same briefs, in order");
        assert_eq!((some.len(), some), (3, claude), "those its filter wants, in order");
        assert!(none.is_empty(), "none wanted");
        assert_eq!(MADE.load(Ordering::Relaxed), 15, "each agent's brief made once a call");
        assert_eq!(ON_SHARD.load(Ordering::Relaxed), 0, "none made on a shard");
        Ok(())
    }

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
            let schema = op.input.schema().to_string();
            for word in ["decision", "approve", "answers", "choices"] {
                assert!(!schema.contains(word), "{} takes {word}", op.name);
            }
            assert!(!op.name.contains("approve") && !op.name.contains("answer"), "{}", op.name);
        }
        let sends = list_ops().into_iter().find(|o| o.name == "agents.send").map(|o| o.input.schema()).unwrap_or_default();
        assert_eq!(sends.pointer("/properties/mode/enum"), Some(&json!(["queue", "steer", "now"])), "a message is all an agent sends another");
    }

    /// A wait takes delivery of the waiter's own messages: a change, so no client takes it for a
    /// read it may repeat or drop. Reading a result marks only what the caller saw: a read.
    #[test]
    fn a_wait_is_a_change_and_a_result_a_read() {
        let op = |name: &str| list_ops().into_iter().find(|o| o.name == name);
        assert!(op("agents.wait").is_some_and(|o| !o.read_only && !o.destructive && (o.route)(&json!({})).is_ok_and(|(m, _)| m == Method::POST)));
        assert!(op("agents.result").is_some_and(|o| o.read_only && (o.route)(&json!({"agent": "a"})).is_ok_and(|(m, _)| m == Method::GET)));
    }

    #[test]
    fn status_is_only_explicit_and_does_not_reuse_the_publication_key() -> anyhow::Result<()> {
        assert_eq!(status_of(&json!({"text":"status: tests pass", "key":"publication"})).ok(), Some(None));
        let status = status_of(&json!({"status":{"key":"build", "task":"task-a"}, "key":"publication-a"})).map_err(|(_, e)| anyhow::anyhow!(e))?.ok_or_else(|| anyhow::anyhow!("missing explicit status"))?;
        assert_eq!(status.key, "build");
        assert_eq!(status.task.as_deref(), Some("task-a"));
        assert!(status_of(&json!({"status":{"key":""}})).is_err());
        assert!(status_of(&json!({"status":{"key":"build", "unknown":true}})).is_err());
        assert!(mail_number(&json!({"after":"18446744073709551615"}), "after").is_ok_and(|v| v == Some(u64::MAX)));
        Ok(())
    }

    #[test]
    fn a_publication_keeps_send_aliases_without_inventing_receiver_acceptance() -> anyhow::Result<()> {
        let receipt = super::super::mail::Receipt { id: "original".into(), revision: 7, generation: 2, disposition: super::super::mail::Disposition::Waiting, repeat: true };
        let answer = publication_answer(&receipt).map_err(|(_, e)| anyhow::anyhow!(e))?;
        assert_eq!(answer.get("sent"), Some(&json!(true)));
        assert_eq!(answer.get("retained"), Some(&json!(true)));
        assert_eq!(answer.get("message"), answer.get("id"));
        assert_eq!(answer.get("revision"), Some(&json!(7)));
        assert_eq!(answer.get("repeat"), Some(&json!(true)));
        assert_eq!(answer.get("disposition"), Some(&json!("waiting")));
        assert!(answer.get("accepted").is_none(), "server custody cannot establish receiver admission");
        Ok(())
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
pub(crate) mod end_to_end {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::{Value, json};

    use crate::harness::session::Spec;
    use crate::harness::wait;
    use crate::hub::Hub;
    use crate::ops::{Cx, call};
    use super::{clear_handoff, stage_handoff};

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
        // A hang's bound, never a pace: a loaded machine takes what time it takes over them (60 s ran out under load).
        match rx.recv_timeout(Duration::from_secs(600)) {
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
        let h = Arc::clone(&hub);
        crate::rt::spawn(async move {
            let _ = crate::harness::mail_delivery::run(h).await;
        });
        hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
        Ok(hub)
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a person who sees every session gets the agents listed from their infos as each
    /// owner shares them (`Registry::list_shared`, none copied), and what the copied list gave: the same agents, in order.
    #[test]
    fn g9_s5_a_list_for_whoever_sees_every_session_is_the_copied_lists() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let (a, b) = (parent(&hub, "default", true).await?, parent(&hub, "default", true).await?);
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let listed = call(person, "agents.list", json!({})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let ids: Vec<String> = listed.get("agents").and_then(Value::as_array).map(|all| all.iter().filter_map(|x| x.get("id").and_then(Value::as_str).map(str::to_owned)).collect()).unwrap_or_default();
            let copied: Vec<String> = hub.harness.list().into_iter().filter(|i| i.helper.is_none()).map(|i| i.id).collect();
            anyhow::ensure!(ids == copied && ids.contains(&a) && ids.contains(&b), "{listed}");
            let shared: Vec<String> = hub.harness.list_shared().iter().map(|i| i.id.clone()).collect();
            anyhow::ensure!(shared == hub.harness.list().into_iter().map(|i| i.id).collect::<Vec<_>>(), "the same sessions, in the same order");
            Ok(())
        })
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
            // Another agent can't stop their turn or end them; their parent can, and ends them with it.
            let other = parent(&hub, "default", true).await?;
            let stranger = as_agent(&hub, &other).await?;
            let Err((s, _)) = call(stranger.clone(), "agents.send", json!({"agent": kid, "text": "stop that", "mode": "now"})).await else { anyhow::bail!("a stranger stopped a child's turn") };
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
    fn cp_t3_2_an_agent_steers_peers_but_stops_only_its_own() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            // Two agents a person started apart: neither started the other.
            let a = parent(&hub, "default", true).await?;
            let b = parent(&hub, "default", true).await?;
            let cx = as_agent(&hub, &a).await?;
            // Default, steered and queued messages go through (b's harness can't run here: what
            // matters is that kiln didn't refuse them), told as from a.
            for mode in [None, Some("steer"), Some("queue")] {
                let mut input = json!({"agent": b, "text": "which commit has the fix?"});
                if let (Some(mode), Some(o)) = (mode, input.as_object_mut()) {
                    o.insert("mode".into(), json!(mode));
                }
                let sent = call(cx.clone(), "agents.send", input).await;
                anyhow::ensure!(!matches!(sent, Err((http::StatusCode::FORBIDDEN, _))), "a message to a fellow agent: {sent:?}");
            }
            // Sending now, interrupting and ending it are its own's to do.
            for (op, input) in [
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
            for mode in ["queue", "steer", "now"] {
                let Err((s, why)) = call(cx.clone(), "agents.send", json!({"agent": c, "text": "hello", "mode": mode})).await else { anyhow::bail!("sent to another person's agent") };
                anyhow::ensure!(s == http::StatusCode::FORBIDDEN && why.contains("works for someone else"), "{s} {why}");
            }
            // A person still sends to any agent, in any mode.
            let person = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let sent = call(person, "agents.send", json!({"agent": b, "text": "go on", "mode": "steer"})).await;
            anyhow::ensure!(!matches!(sent, Err((http::StatusCode::FORBIDDEN, _))), "a person steering: {sent:?}");
            Ok(())
        })
    }

    /// Protocol fixture only: Busy never reports completed work; Complete records each exact
    /// input before emitting the correlated successful result that the production adapter needs.
    #[cfg(unix)]
    pub(crate) fn recording_program(dir: &std::path::Path, complete: bool) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir)?;
        let heard = dir.join("heard.jsonl");
        let script = dir.join("recording.py");
        std::fs::write(&script, r##"import hashlib, json, math, os, select, sys, time, uuid
result_gate = os.environ.get("KILN_TEST_RESULT_GATE")
heard, complete = sys.argv[1], sys.argv[2] == "complete"
if "--version" in sys.argv[3:]:
    print("2.1.296 (Claude Code)")
    sys.exit(0)
session = str(uuid.uuid4())
def out(value):
    print(json.dumps(value), flush=True)
for line in sys.stdin:
    value = json.loads(line)
    if value.get("type") == "control_request":
        response = {}
        if (value.get("request") or {}).get("subtype") == "initialize":
            response = {"commands": [], "models": [], "agents": [], "current_permission_mode": "default", "claude_code_version": "2.1.296"}
        out({"type": "control_response", "response": {"subtype": "success", "request_id": value.get("request_id"), "response": response}})
    elif value.get("type") == "user":
        started = time.monotonic_ns()
        with open(heard, "a") as file:
            file.write(json.dumps(value) + "\n")
            file.flush()
            os.fsync(file.fileno())
        result = hashlib.sha256(json.dumps(value.get("message"), sort_keys=True).encode()).hexdigest()
        elapsed_ms = math.ceil((time.monotonic_ns() - started) / 1000000)
        uid = value.get("uuid")
        gate = os.open(result_gate, os.O_RDONLY | os.O_NONBLOCK) if complete and result_gate else None
        gate_writer = os.open(result_gate, os.O_WRONLY | os.O_NONBLOCK) if gate is not None else None
        result_gate = None
        out({"type": "command_lifecycle", "command_uuid": uid, "state": "started", "session_id": session})
        out({"type": "system", "subtype": "init", "cwd": os.getcwd(), "session_id": session, "tools": [], "mcp_servers": [], "model": "fixture", "permissionMode": "default", "claude_code_version": "2.1.296"})
        if complete:
            if gate is not None:
                select.select([gate], [], [])
                if os.read(gate, 1) != b"1":
                    raise RuntimeError("private result gate closed without release")
                os.close(gate)
                os.close(gate_writer)
            out({"type": "assistant", "message": {"id": str(uuid.uuid4()), "type": "message", "role": "assistant", "model": "fixture", "content": [{"type": "text", "text": result}], "stop_reason": None, "usage": {"input_tokens": 1, "output_tokens": 1}}, "parent_tool_use_id": None, "session_id": session})
            out({"type": "result", "subtype": "success", "terminal_reason": "completed", "is_error": False, "duration_ms": elapsed_ms, "duration_api_ms": elapsed_ms, "num_turns": 1, "user_message_uuid": uid, "result": result, "session_id": session, "total_cost_usd": 0, "usage": {"input_tokens": 1, "output_tokens": 1}})
            out({"type": "command_lifecycle", "command_uuid": uid, "state": "completed", "session_id": session})
"##)?;
        let program = dir.join("recording-claude");
        std::fs::write(&program, format!("#!/bin/sh\nexec python3 '{}' '{}' '{}' \"$@\"\n", script.display(), heard.display(), if complete { "complete" } else { "busy" }))?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        Ok((program, heard))
    }

    /// The slowest gap between a source's moves seen in this run, ms, plus one (none: no gap yet): the
    /// bound a source wait holds a still source to, the party's slowest answer seen (no ×2: R03, the
    /// person's word, 2026-11-13).
    static SLOWEST_GAP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Listen before checking both source state and durable receipts. No timer retries a predicate or
    /// grants source capacity, and none cuts a wait short: it follows its source's liveness and
    /// progress (`kiln_ci::place::keeper::Waits`, as the capacity keeper's waits do). Each move of the
    /// source's feed or the mail tables is a fresh look, and times the gap since the move before it.
    /// While the source owes kiln no answer of its own the wait follows its liveness alone, with no
    /// clock bound: before its harness has answered kiln (said its version), as a harness starting on
    /// a loaded machine can use no CPU time for longer than any gap seen (all 17 waits such a bound
    /// refused in kiln run r512 had harnesses that hadn't answered, 22 to 64 µs of CPU time used); and
    /// while it has no turn under way, as what it waits for then is kiln's next move (a step's goal
    /// comes at the goal driver's next look, `crate::goals::TICK`: kiln run r622 refused one, its
    /// source idle). While a turn is under way, a source that hasn't moved for a whole window of the
    /// slowest gap between moves seen in this run is asked whether its process's CPU time moved in
    /// that window: the window starts again if it did, and the wait is refused if it didn't, saying
    /// what it saw. Its whole wait had a deadline of an inbox's answer (`registry::ANSWER_WAIT`,
    /// 10 s), which a loaded machine's healthy source outran: 3 of 230 tests in one parallel run
    /// beside goals:: on adas-mac-studio.local (kiln run r222, 2026-11-13).
    pub(crate) async fn source_until<F, Fut>(hub: &Hub, id: &str, what: &str, mut check: F) -> anyhow::Result<()>
    where F: FnMut() -> Fut, Fut: std::future::Future<Output = anyhow::Result<bool>> {
        use futures::FutureExt as _;
        use kiln_seams::Bus as _;
        use std::sync::atomic::Ordering;
        let attached = hub.harness.attach(id).ok_or_else(|| anyhow::anyhow!("{what}: no source"))?;
        let mut at = attached.at.ok_or_else(|| anyhow::anyhow!("{what}: no source feed"))?;
        let committed = hub.store.changes().watch(crate::harness::mail::TABLES);
        let clock = crate::rt::clock();
        let began = clock.now();
        let ms = || i64::try_from(clock.now().saturating_duration_since(began).as_millis()).unwrap_or(i64::MAX);
        // Whether it owes kiln an answer of its own (its harness answered kiln, a turn under way), and
        // its process's CPU time so far.
        let owes = || hub.harness.local.get(id).is_some_and(|s| {
            let core = s.core.lock();
            !core.meta.version.is_empty() && core.state.status == crate::harness::model::Status::Running
        });
        let cpu = || hub.harness.local.get(id).and_then(|s| *s.pid.lock()).and_then(|pid| crate::os::metrics::cpu_us_of(&[pid]));
        let mut waits = kiln_ci::place::keeper::Waits::default();
        waits.said(0);
        let (mut moved, mut spent) = (None, cpu());
        loop {
            let revision = hub.store.changes().of(crate::harness::mail::TABLES);
            if check().await? { return Ok(()); }
            anyhow::ensure!(attached.viewer.refusals() == attached.refusals, "{what}: source feed lost");
            let batch = hub.bus.read(&attached.topic, at, 1).await.map_err(|e| anyhow::anyhow!("{what}: {e:?}"))?;
            at = batch.next;
            if !batch.messages.is_empty() || revision != hub.store.changes().of(crate::harness::mail::TABLES) {
                let now = ms();
                (moved, spent) = (Some(now), cpu());
                if let Some(gap) = waits.answered(now) {
                    SLOWEST_GAP.fetch_max(gap.saturating_add(1), Ordering::Relaxed);
                }
                waits.said(now);
                continue;
            }
            loop {
                if let Some(slowest) = SLOWEST_GAP.load(Ordering::Relaxed).checked_sub(1) {
                    waits.slowest_at_least(slowest);
                }
                let still = waits.due().filter(|_| owes()).map(|due| clock.sleep(Duration::from_millis(u64::try_from(due.saturating_sub(ms())).unwrap_or(0))));
                futures::select_biased! {
                    result = hub.bus.wait(&attached.topic, at).fuse() => {
                        result.map_err(|e| anyhow::anyhow!("{what}: {e:?}"))?;
                        break;
                    }
                    () = committed.notified().fuse() => break,
                    () = async move { match still { Some(s) => s.await, None => std::future::pending().await } }.fuse() => {
                        let now = cpu();
                        if now.is_some() && now != spent {
                            spent = now;
                            waits.heard(ms());
                            continue;
                        }
                        let state = hub.harness.info(id).map(|i| format!("{:?}", i.status)).unwrap_or_else(|| "gone".into());
                        let last = moved.map_or_else(|| "hasn't moved since the wait began".to_owned(), |at| format!("last moved {at} ms into the wait and hasn't since"));
                        anyhow::bail!("{what}: its source {last}, a whole window of the slowest gap between moves seen in this run ({} ms), its CPU time {now:?} µs as it was at the window's start; its state {state}", waits.slowest().unwrap_or(0));
                    }
                }
            }
        }
    }

    pub(crate) async fn source_ready(hub: &Hub, id: &str) -> anyhow::Result<()> {
        source_until(hub, id, "authoritative source capacity", || async {
            Ok(crate::harness::mail_delivery::capacity(hub, id).is_some_and(|capacity| capacity.target > 0))
        }).await
    }

    pub(crate) async fn source_disposition(hub: &Hub, id: &str, message: &str, consumed: bool) -> anyhow::Result<()> {
        source_until(hub, id, "correlated durable source receipt", || {
            let recipient = id.to_owned();
            let message = message.to_owned();
            async move {
                hub.store.read(move |connection| {
                    Ok(connection.query_row("SELECT COUNT(*) FROM agent_mail WHERE recipient=?1 AND id=?2 AND (disposition='consumed' OR (NOT ?3 AND disposition='admitted'))", rusqlite::params![recipient, message, consumed], |row| row.get::<_, i64>(0))? == 1)
                }).await
            }
        }).await
    }

    #[cfg(unix)]
    async fn recording(hub: &Arc<Hub>, dir: &std::path::Path, complete: bool) -> anyhow::Result<(String, std::path::PathBuf)> {
        let (program, heard) = recording_program(dir, complete)?;
        let spec = Spec { kind: "claude".into(), cwd: Some(dir.to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
        let id = hub.harness.open(hub, "local", spec, 1).await?.first().map(|i| i.id.clone()).ok_or_else(|| anyhow::anyhow!("no agent"))?;
        source_ready(hub, &id).await?;
        Ok((id, heard))
    }

    /// The prompts a recording harness was sent, by their message ids.
    #[cfg(unix)]
    pub(crate) fn prompts(heard: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(heard)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("type") == Some(&json!("user")))
            .filter_map(|v| v.get("uuid").and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// CP-T3.2: fellow agents' messages reach a busy harness during its turn, with their sender
    /// preserved, even sent to queue (Ada, 2026-11-12: agent-to-agent messages steer, not queue),
    /// while a person's default still waits for the turn to end.
    #[cfg(unix)]
    #[test]
    fn cp_t3_2_agent_messages_steer_a_busy_peer_without_building_a_backlog() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let sender = parent(&hub, "default", true).await?;
            let peer = as_agent(&hub, &sender).await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            // Each case has one in-service input and one refill. Busy emits no consumption;
            // independent cases cannot borrow credit from a turn that never completes.
            for (key, mode, own, queued) in [
                ("peer-default", None, false, false),
                ("peer-steer", Some("steer"), false, false),
                ("own-default", None, true, false),
                ("peer-queue", Some("queue"), false, false),
                ("person-default", None, false, true),
            ] {
                let (id, heard) = recording(&hub, &dir.path().join(key), false).await?;
                let first = call(person.clone(), "agents.send", json!({"agent": id, "text": "keep working"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                let first_id = first.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {first}"))?.to_owned();
                source_until(&hub, &id, "the first busy prompt", || async { Ok(prompts(&heard) == [first_id.clone()]) }).await?;
                source_disposition(&hub, &id, &first_id, false).await?;
                let caller = if own { as_agent(&hub, &id).await? } else if key == "person-default" { person.clone() } else { peer.clone() };
                let mut input = json!({"agent": id, "text": key, "key": key});
                if let (Some(mode), Some(object)) = (mode, input.as_object_mut()) { object.insert("mode".into(), json!(mode)); }
                let sent = call(caller.clone(), "agents.send", input.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                let message = sent.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {sent}"))?.to_owned();
                source_disposition(&hub, &id, &message, false).await?;
                source_until(&hub, &id, "busy steer or explicit queue", || async {
                    Ok(if queued {
                        hub.harness.local.get(&id).is_some_and(|session| session.core.lock().state.queue.iter().any(|item| item.id == message))
                    } else { prompts(&heard) == [first_id.clone(), message.clone()] })
                }).await?;
                let again = call(caller, "agents.send", input).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                anyhow::ensure!(sent.get("retained") == Some(&json!(true)) && sent.get("id") == again.get("id") && sent.get("revision") == again.get("revision") && again.get("repeat") == Some(&json!(true)), "{sent} / {again}");
                let session = hub.harness.local.get(&id).ok_or_else(|| anyhow::anyhow!("no recording session"))?;
                let queue = session.core.lock().state.queue.clone();
                if queued {
                    anyhow::ensure!(queue.len() == 1 && queue.first().is_some_and(|item| item.id == message && item.text.ends_with(key)), "{key}: exact pending original: {queue:?}");
                    anyhow::ensure!(prompts(&heard) == [first_id], "{key}: queued input reached the busy provider");
                } else {
                    anyhow::ensure!(queue.is_empty(), "{key} queued behind a turn that never ends: {queue:?}");
                    anyhow::ensure!(prompts(&heard) == [first_id, message.clone()], "{key}: repeated native input");
                    let lines = std::fs::read_to_string(&heard)?;
                    let steered = lines.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).find(|value| value.get("uuid").and_then(Value::as_str) == Some(message.as_str())).ok_or_else(|| anyhow::anyhow!("no steered prompt"))?;
                    anyhow::ensure!(steered.get("priority").is_none(), "a steer must not stop the turn: {steered}");
                    if !own {
                        let content = steered.pointer("/message/content").and_then(Value::as_str).unwrap_or_default();
                        anyhow::ensure!(content.contains(&sender) && content.contains("It's from a fellow agent, not from your person.") && content.ends_with(key), "{content}");
                    }
                }
                // This fixture never produced a successful result for either input.
                let recipient = id.clone();
                let consumed = hub.store.read(move |connection| Ok(connection.query_row("SELECT COUNT(*) FROM agent_mail WHERE recipient=?1 AND disposition='consumed'", [recipient], |row| row.get::<_, i64>(0))?)).await?;
                anyhow::ensure!(consumed == 0, "busy fixture fabricated consumption");
                drop(session);
                anyhow::ensure!(hub.harness.close(&hub, &id).await, "recording session did not close");
            }
            Ok(())
        })
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
            let (id, heard) = recording(&hub, dir.path(), true).await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let send = |key: Option<&str>| {
                let mut input = json!({"agent": id, "text": "deploy the fix"});
                if let (Some(key), Some(object)) = (key, input.as_object_mut()) { object.insert("key".into(), json!(key)); }
                call(person.clone(), "agents.send", input)
            };
            let first = send(Some("deploy-1")).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let again = send(Some("deploy-1")).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let message = first.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {first}"))?.to_owned();
            anyhow::ensure!(first.get("retained") == Some(&json!(true)) && first.get("message") == first.get("id") && first.get("id") == again.get("id") && first.get("revision").is_some() && first.get("revision") == again.get("revision"), "immutable publication: {first} / {again}");
            anyhow::ensure!(first.get("repeat") == Some(&json!(false)) && again.get("repeat") == Some(&json!(true)), "{first} / {again}");
            anyhow::ensure!(first.get("accepted").is_none() && again.get("accepted").is_none(), "server custody is not a receiver inbox acknowledgement");
            source_disposition(&hub, &id, &message, true).await?;
            anyhow::ensure!(prompts(&heard) == [message.clone()], "one native prompt: {:?}", prompts(&heard));
            let queued: Vec<String> = hub.harness.local.get(&id).map(|session| session.core.lock().state.queue.iter().map(|item| item.id.clone()).collect()).unwrap_or_default();
            anyhow::ensure!(!queued.contains(&message), "a copy waits in its queue: {queued:?}");
            let a = send(None).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let a_id = a.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {a}"))?.to_owned();
            source_disposition(&hub, &id, &a_id, true).await?;
            let b = send(None).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let b_id = b.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {b}"))?.to_owned();
            source_disposition(&hub, &id, &b_id, true).await?;
            anyhow::ensure!(a_id != b_id && a.get("repeat") == Some(&json!(false)) && b.get("repeat") == Some(&json!(false)), "{a} / {b}");
            anyhow::ensure!(prompts(&heard) == [message.clone(), a_id, b_id], "distinct publications reach the native provider in order once");
            let refused = call(person, "agents.send", json!({"agent": id, "text": "something else", "key": "deploy-1"})).await;
            anyhow::ensure!(matches!(&refused, Err((http::StatusCode::CONFLICT, why)) if serde_json::from_str::<crate::harness::mail::Error>(why).is_ok_and(|error| matches!(error, crate::harness::mail::Error::Reused { id } if id == message))), "{refused:?}");
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.1: an agent's agents_send under a key, retried under it after the agent
    /// moved to another machine (its calls come through that machine's runner now, under another
    /// name), is the same message: answered as a repeat, never a second prompt.
    #[test]
    #[cfg(unix)]
    fn a_keyed_send_retried_after_its_sender_moved_is_the_same_message() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (to, heard) = recording(&hub, dir.path(), false).await?;
            let from = parent(&hub, "default", true).await?;
            let before = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &from, Some("box-a")).await.map_err(anyhow::Error::msg)? };
            let after = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &from, Some("box-b")).await.map_err(anyhow::Error::msg)? };
            let input = json!({"agent": to, "text": "deploy the fix", "key": "deploy-1"});
            let first = call(before, "agents.send", input.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let message = first.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no publication: {first}"))?.to_owned();
            source_disposition(&hub, &to, &message, false).await?;
            source_until(&hub, &to, "one moved-sender native prompt", || async { Ok(prompts(&heard) == [message.clone()]) }).await?;
            let again = call(after, "agents.send", input).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let taken = inbox.taken(&to, 100).await?;
            anyhow::ensure!(again.get("id") == first.get("id") && again.get("revision") == first.get("revision") && again.get("repeat") == Some(&json!(true)) && taken == [message.clone()], "same immutable publication after sender move: {first} / {again}; inbox: {taken:?}");
            anyhow::ensure!(prompts(&heard) == [message], "sender move repeated the native prompt");
            Ok(())
        })
    }

    /// CP-T3.2, AC-T3.2.1: an agent's plans.run under a key (its steps, on itself), asked for again
    /// under the key after the agent moved to another machine, is the same run: its messages reach
    /// it once.
    #[test]
    #[cfg(unix)]
    fn a_keyed_plan_run_retried_after_its_agent_moved_is_the_same_run() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (me, heard) = recording(&hub, dir.path(), true).await?;
            struct Owned(Arc<Hub>);
            impl Drop for Owned { fn drop(&mut self) { self.0.harness.local.stop_all(); self.0.shutdown.cancel(); } }
            let _owned = Owned(Arc::clone(&hub));
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let before = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &me, Some("box-a")).await.map_err(anyhow::Error::msg)? };
            let after = Cx { hub: Arc::clone(&hub), caller: crate::mcp::session_caller(&hub, &me, Some("box-b")).await.map_err(anyhow::Error::msg)? };
            let input = json!({"id": p.id, "mode": "steps", "key": "tidy-1"});
            let first = call(before, "plans.run", input.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let run_id = first.get("run").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no run: {first}"))?;
            let messages: Vec<String> = ["0", "1"].iter().map(|step| crate::util::derive_id(&["plan-run", run_id, step])).collect();
            for message in &messages { source_disposition(&hub, &me, message, true).await?; }
            let again = call(after, "plans.run", input).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            let taken = inbox.taken(&me, 100).await?;
            anyhow::ensure!(again.get("run") == first.get("run") && again.get("session") == first.get("session") && taken == messages, "asked again under its key after its agent moved: {first} then {again}; inbox: {taken:?}");
            anyhow::ensure!(first.get("sent").and_then(Value::as_u64).unwrap_or(0) + first.get("retained").and_then(Value::as_u64).unwrap_or(0) == 2 && again.get("sent") == Some(&json!(2)) && again.get("retained") == Some(&json!(0)), "{first} then {again}");
            anyhow::ensure!(prompts(&heard) == messages, "moving the caller repeated native plan inputs");
            Ok(())
        })
    }

    /// CP-T3.2 (the bar: typed refusals, never silent drops): a key is one request. plans.run asked
    /// again under its key for other steps, or in another mode, is refused 409, saying so, and
    /// sends nothing; so is a run of one agent per step asked again under its key for other steps.
    #[test]
    #[cfg(unix)]
    fn a_key_given_to_another_plan_run_is_refused() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (me, heard) = recording(&hub, dir.path(), true).await?;
            let children = tempfile::tempdir()?;
            let (program, child_heard) = recording_program(children.path(), true)?;
            hub.harness.set_test_program(&program.to_string_lossy());
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move { let _ = crate::goals::run(h).await; });
            struct Owned(Arc<Hub>);
            impl Drop for Owned { fn drop(&mut self) { self.0.harness.local.stop_all(); self.0.shutdown.cancel(); } }
            let _owned = Owned(Arc::clone(&hub));
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let first = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "only": [1], "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let run_id = first.get("run").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no run: {first}"))?;
            let message = crate::util::derive_id(&["plan-run", run_id, "0"]);
            source_disposition(&hub, &me, &message, true).await?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            for other in [json!({"id": p.id, "session": me, "mode": "steps", "only": [2], "key": "k"}), json!({"id": p.id, "session": me, "mode": "message", "only": [1], "key": "k"})] {
                let refused = call(person.clone(), "plans.run", other.clone()).await;
                anyhow::ensure!(matches!(&refused, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "{other} after {first}: {refused:?}");
            }
            let taken = inbox.taken(&me, 100).await?;
            anyhow::ensure!(taken == [message.clone()] && prompts(&heard) == [message.clone()], "only step 1's native message went: {taken:?}");
            // The same request again is the same run, answered as it was.
            let again = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "only": [1], "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(again.get("run") == first.get("run") && again.get("session") == first.get("session") && again.get("split") == first.get("split"), "{first} then {again}");
            anyhow::ensure!(first.get("sent").and_then(Value::as_u64).unwrap_or(0) + first.get("retained").and_then(Value::as_u64).unwrap_or(0) == 1 && again.get("sent") == Some(&json!(1)) && again.get("retained") == Some(&json!(0)), "{first} then {again}");
            anyhow::ensure!(inbox.taken(&me, 100).await? == [message.clone()] && prompts(&heard) == [message], "same-key retry repeated native work");
            // One agent per step: the same key, other steps.
            let cwd = tempfile::tempdir()?;
            let new = json!({"on": "local", "kind": "claude", "cwd": cwd.path().to_string_lossy()});
            let agents = call(person.clone(), "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "only": [1], "key": "a"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let run_id = agents.get("run").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no agents run: {agents}"))?;
            let child = crate::plans::agents::view(&hub, run_id).await?.steps.first().and_then(|step| step.agent_id.clone()).ok_or_else(|| anyhow::anyhow!("no step agent: {agents}"))?;
            source_ready(&hub, &child).await?;
            source_until(&hub, &child, "the step's one native input", || async { Ok(prompts(&child_heard).len() == 1) }).await?;
            let child_message = prompts(&child_heard).into_iter().next().ok_or_else(|| anyhow::anyhow!("no native step input"))?;
            source_disposition(&hub, &child, &child_message, true).await?;
            let refused = call(person, "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "only": [2], "key": "a"})).await;
            anyhow::ensure!(matches!(&refused, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "{refused:?}");
            anyhow::ensure!(inbox.taken(&child, 100).await? == [child_message.clone()] && prompts(&child_heard) == [child_message], "another request repeated the step agent's native input");
            Ok(())
        })
    }

    /// CP-T3.2 (the fourth adversarial check's): a key is one request whatever its mode: a key
    /// given to a run of one agent per step is refused (409) for the same plan run as steps on an
    /// agent, and one given to a run as steps is refused for one agent per step.
    #[test]
    #[cfg(unix)]
    fn a_key_given_to_a_plan_run_in_one_mode_is_refused_in_another() -> anyhow::Result<()> {
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (me, heard) = recording(&hub, dir.path(), true).await?;
            let children = tempfile::tempdir()?;
            let (program, child_heard) = recording_program(children.path(), true)?;
            hub.harness.set_test_program(&program.to_string_lossy());
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move { let _ = crate::goals::run(h).await; });
            struct Owned(Arc<Hub>);
            impl Drop for Owned { fn drop(&mut self) { self.0.harness.local.stop_all(); self.0.shutdown.cancel(); } }
            let _owned = Owned(Arc::clone(&hub));
            let p = crate::plans::create(&hub, crate::plans::New { title: "Tidy".into(), body: "- one\n- two\n".into(), ..Default::default() }, "admin").await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let cwd = tempfile::tempdir()?;
            let new = json!({"on": "local", "kind": "claude", "cwd": cwd.path().to_string_lossy()});
            let agents = call(person.clone(), "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "key": "k"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let run_id = agents.get("run").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no agents run: {agents}"))?;
            let child = crate::plans::agents::view(&hub, run_id).await?.steps.first().and_then(|step| step.agent_id.clone()).ok_or_else(|| anyhow::anyhow!("no step agent: {agents}"))?;
            source_ready(&hub, &child).await?;
            source_until(&hub, &child, "the step's one native input", || async { Ok(prompts(&child_heard).len() == 1) }).await?;
            let child_message = prompts(&child_heard).into_iter().next().ok_or_else(|| anyhow::anyhow!("no native step input"))?;
            source_disposition(&hub, &child, &child_message, true).await?;
            let other = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "key": "k"})).await;
            anyhow::ensure!(matches!(&other, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "the same key in another mode ran: {other:?}");
            let first = call(person.clone(), "plans.run", json!({"id": p.id, "session": me, "mode": "steps", "key": "j"})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let run_id = first.get("run").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no run: {first}"))?;
            let messages: Vec<String> = ["0", "1"].iter().map(|step| crate::util::derive_id(&["plan-run", run_id, step])).collect();
            for message in &messages { source_disposition(&hub, &me, message, true).await?; }
            let other = call(person, "plans.run", json!({"id": p.id, "new": new, "mode": "agents", "key": "j"})).await;
            anyhow::ensure!(matches!(&other, Err((c, m)) if c.as_u16() == 409 && m.contains("another request")), "the same key in another mode ran: {other:?}");
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            anyhow::ensure!(inbox.taken(&me, 100).await? == messages && prompts(&heard) == messages, "cross-mode conflict added native work");
            anyhow::ensure!(inbox.taken(&child, 100).await? == [child_message.clone()] && prompts(&child_heard) == [child_message], "cross-mode conflict repeated the step agent's native input");
            Ok(())
        })
    }

    /// Inbox state alone cannot promote an owner. A mismatched persisted epoch refuses native
    /// delivery while the exact committed publication remains retryable under its original ID.
    #[test]
    #[cfg(unix)]
    fn a_stale_inbox_epoch_preserves_custody_without_changing_the_current_owner() -> anyhow::Result<()> {
        use crate::harness::{mail, model::Command, registry::Sent};
        run(async {
            let hub = hub_with(|_| {}).await?;
            let dir = tempfile::tempdir()?;
            let (id, heard) = recording(&hub, dir.path(), true).await?;
            let inbox = hub.harness.local.inbox().cloned().ok_or_else(|| anyhow::anyhow!("no inbox"))?;
            inbox.open(&id, 3).await?;
            let owner_epoch = hub.harness.epoch_of(&id);
            let command = || Command::Send { id: "m1".into(), text: "hello".into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let sent = hub.harness.command(&hub, &id, command()).await;
            let Sent::Published(first) = sent else { anyhow::bail!("no committed original: {sent:?}") };
            source_until(&hub, &id, "mismatched inbox refuses source admission", || {
                let recipient = id.clone();
                let hub = Arc::clone(&hub);
                let revision = first.revision;
                async move {
                    let receipt = mail::receipt(&hub.store, "server".into(), "server".into(), recipient, "m1".into()).await?;
                    Ok(receipt.is_some_and(|receipt| receipt.revision == revision && receipt.generation > 0 && receipt.disposition == mail::Disposition::Waiting))
                }
            }).await?;
            let again = hub.harness.command(&hub, &id, command()).await;
            anyhow::ensure!(matches!(again, Sent::Published(ref receipt) if receipt.repeat && receipt.revision == first.revision && receipt.id == first.id), "exact custody retry: {again:?}");
            anyhow::ensure!(hub.harness.epoch_of(&id) == owner_epoch, "an inbox cannot impersonate a new authenticated owner");
            anyhow::ensure!(inbox.taken(&id, 100).await?.is_empty() && prompts(&heard).is_empty(), "no delivery under a mismatched epoch");
            let recipient = id.clone();
            let body: String = hub.store.read(move |connection| Ok(connection.query_row("SELECT body FROM agent_mail WHERE recipient=?1 AND id='m1'", [recipient], |row| row.get(0))?)).await?;
            let original: mail::Publish = serde_json::from_str(&body)?;
            anyhow::ensure!(original.command == serde_json::to_value(command())?, "the complete original survives the refusal");
            Ok(())
        })
    }

    #[test]
    fn a_message_from_a_fellow_agent_says_who_sent_it_and_how_to_answer() {
        let sender = crate::harness::session::Info { name: "Mantle".into(), kind: "claude".into(), target: "agent:adas-macbook-pro-2.local".into(), ..Default::default() };
        let me = "01a1488745a977f58e4224bc796f3308";
        let text = super::peer_text(Some((&sender, "adas-macbook-pro-2.local")), me, "Which commit adds the embedding API?");
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

    /// Hold one actual result, exhaust cold-start authority, retain a refused communication
    /// at its caller, then prove exact retry and immutable state history after real completion.
    #[test]
    #[cfg(unix)]
    fn a_busy_coordinator_takes_custody_of_and_then_delivers_a_waiting_communication() -> anyhow::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        use std::io::Write as _;
        use crate::harness::{admission::Learning, model::Status};
        run(async {
            let hub = hub_with(|config| config.conversations.off = true).await?;
            let dir = tempfile::tempdir()?;
            let (program, heard) = recording_program(dir.path(), true)?;
            let gate = dir.path().join("first-result.fifo");
            nix::unistd::mkfifo(gate.as_path(), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR)?;
            let wrapper = dir.path().join("gated-claude");
            std::fs::write(&wrapper, format!("#!/bin/sh\nKILN_TEST_RESULT_GATE='{}' exec '{}' \"$@\"\n", gate.display(), program.display()))?;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))?;
            let spec = Spec { kind: "claude".into(), name: Some("Coordinator".into()), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(wrapper.to_string_lossy().into_owned()), ..Spec::default() };
            let coordinator = hub.harness.open(&hub, "local", spec, 1).await?.first().map(|info| info.id.clone()).ok_or_else(|| anyhow::anyhow!("no coordinator"))?;
            source_ready(&hub, &coordinator).await?;
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let initial = call(person.clone(), "agents.send", json!({"agent": coordinator, "text": "Work the program's goals with your helpers.", "key": "initial-work"})).await.map_err(|(status, error)| anyhow::anyhow!("{status}: {error}"))?;
            let initial_id = initial.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no initial receipt: {initial}"))?.to_owned();
            source_until(&hub, &coordinator, "initial actual native input at the result gate", || async {
                Ok(prompts(&heard) == [initial_id.clone()] && hub.harness.info(&coordinator).is_some_and(|info| info.status == Status::Running)
                    && hub.harness.local.get(&coordinator).is_some_and(|session| session.core.lock().state.delivery.get(&initial_id) == Some(&crate::harness::model::Delivery::Started)))
            }).await?;
            source_disposition(&hub, &coordinator, &initial_id, false).await?;
            crate::goals::add(&hub, &coordinator, vec!["the lab program passes".into(), "the view loads in 300 ms".into()], true).await?;
            source_until(&hub, &coordinator, "one real refill goal", || async {
                Ok(hub.harness.local.get(&coordinator).is_some_and(|session| session.core.lock().state.queue.len() == 1)
                    && crate::harness::mail_delivery::capacity(&hub, &coordinator).is_some_and(|capacity| capacity.admitted == capacity.target))
            }).await?;
            let capacity = crate::harness::mail_delivery::capacity(&hub, &coordinator).ok_or_else(|| anyhow::anyhow!("no capacity"))?;
            anyhow::ensure!(capacity.learning == Learning::Bootstrap && capacity.target == 2 && capacity.admitted == 2, "only the two actual cold-start roles: {capacity:?}");
            let goal_id = hub.harness.local.get(&coordinator).and_then(|session| session.core.lock().state.queue.first().map(|item| item.id.clone())).ok_or_else(|| anyhow::anyhow!("no queued goal"))?;
            let original_recipient = coordinator.clone();
            let originals: Vec<(String, String)> = hub.store.read(move |connection| {
                let mut statement = connection.prepare("SELECT id,body FROM agent_mail WHERE recipient=?1 ORDER BY revision")?;
                Ok(statement.query_map([original_recipient], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
            }).await?;
            anyhow::ensure!(originals.len() == 2 && originals.iter().any(|(id, _)| id == &initial_id) && originals.iter().any(|(id, _)| id == &goal_id), "every accepted original is in SQLite");
            let sender = parent(&hub, "default", true).await?;
            let peer = as_agent(&hub, &sender).await?;
            let key = "registry-tests-pass";
            let blocked = json!({"agent": coordinator, "text": "The registry's tests pass on my side.", "mode": "queue", "key": key});
            let blocked_id = crate::harness::queued::from_agent_as(&sender, &super::keyed_id(&peer, "agents.send", &coordinator, key));
            // Ada, 2026-11-12: no message is refused for a busy recipient's depth; it's taken and waits.
            let accepted = call(peer.clone(), "agents.send", blocked.clone()).await.map_err(|(status, error)| anyhow::anyhow!("{status}: {error}"))?;
            anyhow::ensure!(accepted.get("id") == Some(&json!(blocked_id)) && accepted.get("repeat") == Some(&json!(false)) && accepted.get("disposition") == Some(&json!("waiting")), "a busy coordinator's peer is taken into custody, not refused: {accepted}");
            let current = crate::harness::mail_delivery::capacity(&hub, &coordinator).ok_or_else(|| anyhow::anyhow!("no capacity"))?;
            anyhow::ensure!(current.admitted <= current.target && current.admitted == capacity.admitted && prompts(&heard) == [initial_id.clone()], "custody cannot grow source admission or fabricate consumption");
            // The gate reader was opened before native Started, so this nonblocking writer
            // cannot wait on a missing provider or strand a blocking pool job on cancellation.
            let mut release = std::fs::OpenOptions::new().write(true).custom_flags(nix::libc::O_NONBLOCK).open(&gate)?;
            release.write_all(b"1")?;
            drop(release);
            source_disposition(&hub, &coordinator, &initial_id, true).await?;
            source_disposition(&hub, &coordinator, &goal_id, true).await?;
            source_disposition(&hub, &coordinator, &blocked_id, true).await?;
            let repeated = call(peer, "agents.send", blocked).await.map_err(|(status, error)| anyhow::anyhow!("{status}: {error}"))?;
            anyhow::ensure!(repeated.get("id") == accepted.get("id") && repeated.get("revision") == accepted.get("revision") && repeated.get("repeat") == Some(&json!(true)), "{accepted} / {repeated}");
            anyhow::ensure!(prompts(&heard) == [initial_id.clone(), goal_id.clone(), blocked_id.clone()], "one actual native delivery per accepted original");
            let recipient = coordinator.clone();
            let after: Vec<(String, String)> = hub.store.read(move |connection| {
                let mut statement = connection.prepare("SELECT id,body FROM agent_mail WHERE recipient=?1 ORDER BY revision")?;
                Ok(statement.query_map([recipient], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
            }).await?;
            anyhow::ensure!(after.starts_with(&originals), "completion/retry changed or deleted accepted original history");
            // Explicit news states retain both originals. Depending on actual dispatch, the
            // first is either consumed frozen work or an undispatched superseded successor.
            let news1 = call(person.clone(), "agents.send", json!({"agent": coordinator, "text": "Helper finished round 1.", "key": "news-round-1", "status": {"key": "incident-helper-news"}})).await.map_err(|(status, error)| anyhow::anyhow!("{status}: {error}"))?;
            let news2 = call(person, "agents.send", json!({"agent": coordinator, "text": "Helper finished round 2.", "key": "news-round-2", "status": {"key": "incident-helper-news"}})).await.map_err(|(status, error)| anyhow::anyhow!("{status}: {error}"))?;
            let latest = news2.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no latest news"))?.to_owned();
            source_disposition(&hub, &coordinator, &latest, true).await?;
            let recipient = coordinator.clone();
            let news: Vec<(String, String, String)> = hub.store.read(move |connection| {
                let mut statement = connection.prepare("SELECT id,disposition,body FROM agent_mail WHERE recipient=?1 AND status_key='incident-helper-news' ORDER BY revision")?;
                Ok(statement.query_map([recipient], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
            }).await?;
            anyhow::ensure!(news.len() == 2 && news.first().is_some_and(|(id, disposition, body)| Some(&json!(id)) == news1.get("id") && matches!(disposition.as_str(), "consumed" | "superseded") && body.contains("Helper finished round 1."))
                && news.last().is_some_and(|(id, disposition, body)| id == &latest && disposition == "consumed" && body.contains("Helper finished round 2.")), "original and latest news history: {news:?}");
            let native = prompts(&heard);
            for (id, disposition, _) in &news {
                anyhow::ensure!(native.iter().filter(|native| *native == id).count() == usize::from(disposition == "consumed"), "a superseded original is not model consumption; a consumed original goes once");
            }
            let stack = crate::goals::done(&hub, &coordinator, None, Some("the lab program passed"), &crate::goals::By::You).await?;
            anyhow::ensure!(stack.items.iter().any(|item| item.text == "the lab program passes" && item.status == "met") && stack.items.iter().any(|item| item.text == "the view loads in 300 ms" && item.status == "active"), "latest goal state preserves its completed predecessor");
            let recipient = coordinator.clone();
            let next: Option<String> = hub.store.read(move |connection| Ok(connection.query_row("SELECT sent_id FROM goal_items WHERE agent=?1 AND text='the view loads in 300 ms'", [recipient], |row| row.get(0))?)).await?;
            let next = next.ok_or_else(|| anyhow::anyhow!("no next-goal publication"))?;
            anyhow::ensure!(next != goal_id, "changed goal reused an immutable ID");
            source_disposition(&hub, &coordinator, &next, true).await?;
            anyhow::ensure!(prompts(&heard).iter().filter(|id| *id == &next).count() == 1, "latest goal reaches native input once");
            Ok(())
        })
    }

    /// MailPeers changes the same owner's capacity that later ordinary State frames carry.
    /// An unchanged demand must not need another publication or probe to recover eligibility.
    #[test]
    #[cfg(unix)]
    fn mail_peers_capacity_survives_an_ordinary_owner_state() -> anyhow::Result<()> {
        use crate::harness::{admission::Demand, mail, model::{Command, Status}, registry::Sent};
        run(async {
            let hub = crate::hub::tests::scratch_hub_with(|config| config.conversations.off = true).await?;
            struct Owned(Arc<Hub>);
            impl Drop for Owned { fn drop(&mut self) { self.0.harness.local.stop_all(); self.0.shutdown.cancel(); } }
            let _owned = Owned(Arc::clone(&hub));
            let owner = Arc::clone(&hub);
            crate::rt::spawn(async move { let _ = crate::harness::registry::Registry::run_local(owner).await; });
            let dir = tempfile::tempdir()?;
            let (recipient, heard) = recording(&hub, dir.path(), true).await?;
            source_until(&hub, &recipient, "native owner initialized", || async {
                Ok(hub.harness.info(&recipient).is_some_and(|info| info.status == Status::Idle)
                    && hub.harness.local.get(&recipient).is_some_and(|session| session.core.lock().state.status == Status::Idle))
            }).await?;
            let message = "mail-peers-state";
            let command = Command::Send { id: message.into(), text: "retain the exact original until real completion".into(), mode: Default::default(), attachments: Vec::new(), overrides: None, reply_to: None, task: None };
            let Sent::Published(original) = hub.harness.command(&hub, &recipient, command).await else { anyhow::bail!("the original was not committed") };
            anyhow::ensure!(original.generation == 0 && original.disposition == mail::Disposition::Waiting && prompts(&heard).is_empty(), "custody alone is not native consumption: {original:?}");
            let epoch = hub.harness.epoch_of(&recipient);
            anyhow::ensure!(hub.harness.local.command(&recipient, Command::MailPeers { epoch, peers: vec!["server".into()], demand: vec![Demand { peer: "server".into(), revision: original.revision }] }), "MailPeers was not queued");
            source_until(&hub, &recipient, "source grant for the retained publication", || async {
                Ok(crate::harness::mail_delivery::capacity(&hub, &recipient).is_some_and(|capacity| capacity.eligible_peer.as_deref() == Some("server")))
            }).await?;
            let grant = crate::harness::mail_delivery::capacity(&hub, &recipient).ok_or_else(|| anyhow::anyhow!("source grant disappeared"))?;
            // Another session's use of the machine's budget moves this one's capacity on, as a parallel
            // run's sessions do (`Controller::budget_changed`, flowed as the native budget's change is): a
            // new generation, the grant held.
            let session = hub.harness.local.get(&recipient).ok_or_else(|| anyhow::anyhow!("source disappeared"))?;
            let input = session.core.lock().input.clone().ok_or_else(|| anyhow::anyhow!("no admission input"))?;
            let moved = {
                let mut controller = input.controller.lock();
                controller.budget_changed(false).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                controller.capacity()
            };
            session.mail_flow(crate::harness::admission::FlowEvent::Capacity { capacity: moved });
            let name = "ordinary-state-after-mail-peers";
            anyhow::ensure!(hub.harness.local.command(&recipient, Command::Control { id: "rename".into(), value: Some(json!(name)) }), "ordinary owner control was not queued");
            source_until(&hub, &recipient, "ordinary owner State after the grant", || async { Ok(hub.harness.info(&recipient).is_some_and(|info| info.name == name)) }).await?;
            // The grant held through the ordinary State: the session's State, the server's projection of
            // its capacity and the raw State agree, the server still eligible at the grant's epoch and no
            // generation behind it. Its generation moves on with every budget, readiness or demand change
            // (`Controller::changed`), sessions beside it in a parallel run sharing the machine's budget:
            // the grant itself said again failed under that load (kiln runs r222 and r153, 2026-11-13).
            let views = || {
                let local = hub.harness.local.get(&recipient).and_then(|session| session.core.lock().state.admission.clone());
                (local, crate::harness::mail_delivery::capacity(&hub, &recipient), hub.harness.state(&recipient).and_then(|state| state.admission))
            };
            source_until(&hub, &recipient, "the State and its projections agree", || async {
                let (local, projected, raw) = views();
                Ok(local.is_some() && local == projected && projected == raw)
            }).await?;
            let held = views().0.ok_or_else(|| anyhow::anyhow!("source disappeared"))?;
            anyhow::ensure!(held.epoch == grant.epoch && held.eligible_peer == grant.eligible_peer && held.generation >= grant.generation, "an ordinary State revoked the live grant: {grant:?}, now {held:?}");
            let worker = Arc::clone(&hub);
            crate::rt::spawn(async move { let _ = crate::harness::mail_delivery::run(worker).await; });
            source_disposition(&hub, &recipient, message, true).await?;
            let receipt = mail::receipt(&hub.store, "server".into(), "server".into(), recipient.clone(), message.into()).await?.ok_or_else(|| anyhow::anyhow!("the committed original disappeared"))?;
            anyhow::ensure!(receipt.revision == original.revision && receipt.disposition == mail::Disposition::Consumed && prompts(&heard) == [message], "one exact native delivery and actual consumption: {receipt:?}");
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
        let first = super::peer_text(Some((&on("Mantle", "adacachy"), "adacachy")), me, "Which commit?\n\nThe one with the API.");
        let again = super::peer_text(Some((&on("Mantle T10", "fedora"), "fedora")), me, "Which commit?\n\nThe one with the API.");
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
    fn wait_response_stages_exact_ids_and_can_withdraw_unregistered_mail() -> anyhow::Result<()> {
        let base=json!({"done":false,"timed_out":false,"agents":[]});
        let mut response=base.clone();
        let message=json!({"id":"exact-original","from":"peer","name":null,"text":"unchanged 日本語 \"\\\nbody","at":7});
        let expected=wait::handoff_digest("handoff",Some(1),std::slice::from_ref(&message)).map_err(|e|anyhow::anyhow!("{e:?}"))?;
        assert_eq!(stage_handoff(&mut response,"handoff",Some(1),vec![message.clone()]),Ok(expected));
        assert_eq!(response.get("messages"),Some(&json!([message])));
        assert_eq!(response.get("mail_handoff"),Some(&json!({"id":"handoff","turn":1})));
        assert!(!response.to_string().contains("response_digest"),"the returned subtree contains no circular digest");
        clear_handoff(&mut response);
        assert_eq!(response,base,"failed or lost registration returns no mail and preserves the rest of the response");
        Ok(())
    }

    #[test]
    fn wait_handoff_budget_includes_the_complete_wait_response() -> anyhow::Result<()> {
        use super::super::state_reads::Failure;
        let mut response=json!({"agents":[],"padding":""});
        let overhead=serde_json::to_vec(&response)?.len();
        let padding=super::super::state_view::FRAME.checked_sub(overhead).ok_or_else(||anyhow::anyhow!("frame is smaller than fixture"))?;
        response.as_object_mut().ok_or_else(||anyhow::anyhow!("fixture is not an object"))?.insert("padding".into(),json!("x".repeat(padding)));
        assert_eq!(serde_json::to_vec(&response)?.len(),super::super::state_view::FRAME);
        let original=response.clone();
        let message=json!({"id":"original","from":"peer","name":null,"text":"body","at":1});
        assert_eq!(stage_handoff(&mut response,"handoff",Some(1),vec![message]),Err(Failure::Source(super::super::state_view::Error::Budget)));
        assert_eq!(response,original,"no registration can follow a response that does not fit");
        let mut reserved=json!({"messages":"existing data"});
        let before=reserved.clone();
        assert_eq!(stage_handoff(&mut reserved,"handoff",Some(1),vec![json!({"id":"original","from":"peer","name":null,"text":"body","at":1})]),Err(Failure::Malformed));
        assert_eq!(reserved,before,"preexisting response data is not overwritten");
        Ok(())
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

    /// Goal 9, plan 01a1a87b2cec step 3: `agents.list`, its briefs made in turns of the shard's and a person's infos read
    /// as their owners share them (goals 10 and 9's), lists what it listed when it did it all in one poll:
    /// for an agent (its own children and itself, its person's other agents), another agent of the same person,
    /// and the owner; with no filter, by parent, by state, and by a harness none runs; in the registry's order.
    #[test]
    fn g9_s3_agents_list_lists_what_it_listed_for_agents_and_people() -> anyhow::Result<()> {
        use crate::harness::{lineage, wait};
        run(async {
            let hub = hub_with(|_| {}).await?;
            let p = parent(&hub, "default", true).await?;
            let cx = as_agent(&hub, &p).await?;
            let mut kids = Vec::new();
            for name in ["a", "b"] {
                let started = call(cx.clone(), "agents.start", json!({"kind": "claude", "mode": "plan", "cwd": NOWHERE, "name": name})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                kids.push(started.pointer("/agents/0/id").and_then(Value::as_str).unwrap_or("").to_owned());
            }
            // They can't start (their folder isn't there): settled, so their briefs hold still.
            let waited = call(cx.clone(), "agents.wait", json!({"agents": kids, "timeout_s": 20})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(waited.get("done") == Some(&json!(true)), "{waited}");
            let other = parent(&hub, "default", true).await?;
            let stranger = as_agent(&hub, &other).await?;
            let owner = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            // Who each listed agent is, which holds still while the parents start and stop.
            let who_is = |v: &Value| -> Vec<(Value, Value, Value)> { v.as_array().map(|a| a.iter().map(|b| (b.get("id").cloned().unwrap_or_default(), b.get("kind").cloned().unwrap_or_default(), b.get("parent").cloned().unwrap_or_default())).collect()).unwrap_or_default() };
            for (who, caller) in [("the parent", &cx), ("another agent", &stranger), ("the owner", &owner)] {
                for args in [json!({}), json!({"harness": "codex"}), json!({"parent": p}), json!({"parent": p, "state": "stopped"}), json!({"parent": p, "state": "ended"})] {
                    let got = call(caller.clone(), "agents.list", args.clone()).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
                    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_owned);
                    let (parent, state, harness) = (s("parent"), s("state"), s("harness"));
                    // As it listed them: what the caller sees, then the filters, then each one's brief.
                    let want: Vec<Value> = lineage::seen(&hub, &caller.caller, hub.harness.list())
                        .await
                        .iter()
                        .filter(|i| i.helper.is_none())
                        .filter(|i| harness.as_deref().is_none_or(|h| i.kind == h))
                        .filter(|i| parent.as_deref().is_none_or(|q| i.parent.as_deref() == Some(q)))
                        .map(wait::brief)
                        .filter(|b| state.as_deref().is_none_or(|st| b.get("stand").and_then(Value::as_str) == Some(st) || b.get("status").and_then(Value::as_str) == Some(st)))
                        .collect();
                    let want = Value::Array(want);
                    // The children have settled: their briefs whole. The parents' may change as they're read.
                    let same = if parent.is_some() { got.get("agents") == Some(&want) } else { got.get("agents").map(who_is) == Some(who_is(&want)) };
                    anyhow::ensure!(same, "{who}, {args}: {got} against {want}");
                }
            }
            // And the parent's own children are what it lists by parent.
            let mine = call(cx, "agents.list", json!({"parent": p})).await.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            let ids: Vec<&str> = mine.get("agents").and_then(Value::as_array).map(|a| a.iter().filter_map(|b| b.get("id").and_then(Value::as_str)).collect()).unwrap_or_default();
            anyhow::ensure!(ids.len() == 2 && kids.iter().all(|k| ids.contains(&k.as_str())), "{mine}");
            Ok(())
        })
    }

    /// `fut`, and what its polls inflated and read back from spill files on the thread that polled them (a request's
    /// shard), counted a poll at a time so nothing else that thread runs between them counts.
    async fn read_back_in<F: std::future::Future>(fut: F) -> (F::Output, u64, u64) {
        let mut fut = std::pin::pin!(fut);
        let (mut inflated, mut read) = (0u64, 0u64);
        let out = std::future::poll_fn(|cx| {
            let was = (crate::harness::packed::INFLATED.with(std::cell::Cell::get), crate::harness::spill::READ_BACK.with(std::cell::Cell::get));
            let polled = fut.as_mut().poll(cx);
            inflated = inflated.wrapping_add(crate::harness::packed::INFLATED.with(std::cell::Cell::get).wrapping_sub(was.0));
            read = read.wrapping_add(crate::harness::spill::READ_BACK.with(std::cell::Cell::get).wrapping_sub(was.1));
            polled
        })
        .await;
        (out, inflated, read)
    }

    /// Goal 11 audit A2(c), A3 (goal 10, plan 01a1a87b2dae step 5: the API's reads under a millisecond): `agents.get` reads
    /// none of an agent's events, and `agents.result` reads its last reply by the place its copy keeps, its chunk read back
    /// off the request's shard. Do: an agent's reply, then six chunks' events of its subagent's, so the reply's chunk is
    /// compact and spilled; `agents.get` and `agents.result` called. Expect: nothing inflated or read from a spill file on
    /// the request's shard by either, the agent's state from `agents.get`, its reply's words from `agents.result`; and a
    /// reply after them read from memory. `Registry::conclusion` scanned back through every chunk on the shard for both,
    /// each compact one inflated, each spilled one read from its file: this fails on that code.
    #[test]
    fn g10_a2c_agents_get_and_result_read_nothing_back_on_the_requests_shard() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{Body, Event};
        use crate::harness::registry::Location;
        run(async {
            let hub = hub_with(|_| {}).await?;
            let there = Location::Machine("box".into());
            let id = crate::util::new_id();
            let info = crate::harness::session::Info { id: id.clone(), kind: "claude".into(), name: "Helper".into(), status: crate::harness::model::Status::Running, ..crate::harness::session::Info::default() };
            hub.harness.ingest(&there, &id, Update::Open { info: Box::new(info), since: None }).await;
            let said = |q: u64, parent: Option<&str>, text: String| Event { id: format!("e{q}"), seq: q, at: 1, parent: parent.map(str::to_owned), by: None, body: Body::Assistant { text, streaming: false } };
            let n = u64::try_from(crate::harness::copies::CHUNK * 6 + 9)?;
            let events: Vec<Event> = std::iter::once(said(1, None, "Done: D42 holds the fix.".into())).chain((2..=n).map(|q| said(q, Some("t1"), format!("{q}: a subagent at work")))).collect();
            hub.harness.ingest(&there, &id, Update::Events { events }).await;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while !hub.harness.view_held(&id).is_some_and(|v| v.events.chunks().first().is_some_and(|c| c.is_spilled())) {
                anyhow::ensure!(std::time::Instant::now() < deadline, "the reply's chunk never spilled");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            let person = Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            let (got, inflated, read) = read_back_in(call(person.clone(), "agents.get", json!({"agent": id}))).await;
            let got = got.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(inflated == 0 && read == 0, "agents.get: {inflated} inflated, {read} read from a spill file on its shard");
            anyhow::ensure!(got.pointer("/info/id") == Some(&json!(id)) && got.get("state").is_some(), "{got}");
            let (got, inflated, read) = read_back_in(call(person.clone(), "agents.result", json!({"agent": id}))).await;
            let got = got.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(inflated == 0 && read == 0, "agents.result: {inflated} inflated, {read} read from a spill file on its shard");
            anyhow::ensure!(got.pointer("/reply/text") == Some(&json!("Done: D42 holds the fix.")), "its last reply: {got}");
            // A reply after them, in the open chunk: read where it is.
            hub.harness.ingest(&there, &id, Update::Events { events: vec![said(n + 1, None, "And D43.".into())] }).await;
            hub.harness.applied(&id).await;
            let (got, inflated, read) = read_back_in(call(person, "agents.result", json!({"agent": id}))).await;
            let got = got.map_err(|(s, m)| anyhow::anyhow!("{s} {m}"))?;
            anyhow::ensure!(inflated == 0 && read == 0 && got.pointer("/reply/text") == Some(&json!("And D43.")), "the newer reply: {got}");
            Ok(())
        })
    }
}
