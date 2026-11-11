//! Where agents stand, and waiting for them to settle (`agents.wait`, docs/agents.md "Agents that
//! run agents").
//!
//! A wait holds no thread and no lock: it listens to the server's change events (every change to
//! an agent as lists show it is one, `{"type": "harness", "info"}`), reads the agents as the
//! registry has them when one of theirs comes (or every few seconds, should events be dropped),
//! and returns as soon as they stand where it waits for, or its time is up (at most [`MAX`]).
//!
//! A message an agent was just sent counts until it has had its turn: an agent idle with a message
//! on its way isn't done yet, so a parent that sends and then waits isn't answered at once.
//!
//! An agent that waits is in a turn, and what's queued for it waits behind that turn. A message
//! another agent sent it doesn't: its wait returns as soon as one is in its queue ([`mail`]), and
//! `agents.wait` registers an exact bounded response (`super::ops`) for that original turn. Source
//! queue bodies and input leases remain until native tool-result delivery is proven.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Digest as _;

use super::model::Status;
use super::session::Info;
use crate::hub::Hub;

/// The longest one call waits; call again to wait longer.
pub const MAX: Duration = Duration::from_secs(600);
/// How long a call waits when it doesn't say.
pub const DEFAULT: Duration = Duration::from_secs(300);
/// How long a message an agent was sent counts as on its way when its harness never says its turn
/// ended (one idle with nothing queued that long has taken it).
const ON_ITS_WAY: Duration = Duration::from_secs(30);
/// Messages sent to agents and not yet answered: how the agent's last turn had ended when it went
/// (its `last_turn.at`), and when it went.
static SENT: LazyLock<Mutex<HashMap<String, (Option<i64>, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The most messages one wait hands over, and the most of their words (bytes), all told: the rest
/// wait for the next wait, or the turn's end (one longer than that on its own, too).
const HAND_MAX: usize = 16;
const HAND_BYTES: usize = 48 * 1024;
/// Only concurrent response assembly is claimed here. Durable source handoff metadata, not a
/// timed server cache, owns duplicate delivery and native result proof.
static CLAIMED: LazyLock<Mutex<HashMap<String, HashSet<String>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Agent `id` was just sent a message (or started with a prompt): `last_turn` is how its last turn
/// had ended then.
pub fn sent(id: &str, last_turn: Option<i64>) {
    let mut g = SENT.lock();
    // Forget what's long answered.
    if g.len() > 4096 {
        g.retain(|_, (_, at)| at.elapsed() < ON_ITS_WAY);
    }
    g.insert(id.to_owned(), (last_turn, Instant::now()));
}

/// A message sent to it hasn't had its turn yet.
fn on_its_way(info: &Info) -> bool {
    let mut g = SENT.lock();
    let Some((before, at)) = g.get(&info.id).copied() else { return false };
    let ended = info.last_turn.as_ref().map(|t| t.at);
    let taken = ended.is_some() && ended != before;
    // Idle (or ended) with nothing queued for a while: it took it without a turn of its own.
    let quiet = at.elapsed() >= ON_ITS_WAY && info.queued == 0 && matches!(info.status, Status::Idle | Status::Exited);
    if taken || quiet {
        g.remove(&info.id);
        return false;
    }
    true
}

/// Where an agent stands, in a word: `needs_you` (an approval, a question or its goals wait on a
/// person), `working`, `idle` (its turn is over, nothing queued or waiting for room), `paused` (a
/// person's explicit pause, or a usage limit after its turn),
/// `stopped` (it crashed or couldn't start), `offline` (its machine is away), `ended`. One whose
/// process ended while nobody ended it is working: kiln starts it again by itself.
pub fn stand(info: &Info) -> &'static str {
    let deferred_approvals = info.deferred.get("approvals").is_some_and(|d| d.kind == super::state_view::Kind::Array && d.len > 0);
    if !info.approvals.is_empty() || deferred_approvals || info.status == Status::Waiting || info.held.is_some() {
        return "needs_you";
    }
    // Attention plan 01a199e1b177: a person's pause stops live processes where they are, including
    // a running turn or a message on its way. Retained flags don't turn an ended or offline agent into paused.
    if info.frozen.is_some() && !matches!(info.status, Status::Exited | Status::Disconnected) {
        return "paused";
    }
    if on_its_way(info) {
        return "working";
    }
    match info.status {
        Status::Exited if info.exit.as_ref().is_some_and(|e| e.again_at.is_some()) => "working",
        Status::Exited if info.exit.as_ref().is_some_and(|e| e.error.is_some()) => "stopped",
        Status::Exited => "ended",
        Status::Disconnected => "offline",
        _ if info.pause.is_some() && info.status != Status::Running => "paused",
        Status::Running | Status::Starting | Status::Waiting => "working",
        Status::Idle if info.queued > 0 || info.room_wait.is_some() => "working",
        // Its turn is over but a subagent it sent off still works: it isn't done, and it goes on
        // when the subagent reports back.
        Status::Idle if info.subagents.iter().any(|s| matches!(s.status.as_str(), "running" | "pending" | "paused")) => "working",
        Status::Idle => "idle",
    }
}

/// Settled: where a wait can return because the agent yielded, needs a person, paused, stopped or
/// ended. This doesn't assert that its objective was met.
pub fn settled(stand: &str) -> bool {
    matches!(stand, "idle" | "needs_you" | "paused" | "stopped" | "ended")
}

/// The machine a target names (`local` is the server).
pub fn machine(target: &str) -> &str {
    target.strip_prefix("agent:").unwrap_or(if target.is_empty() { "local" } else { target })
}

/// An agent as `agents.*` answer with it: who it is, where it stands, what waits on a person.
pub fn brief(info: &Info) -> Value {
    let stand = stand(info);
    let needs: Vec<Value> = info.approvals.iter().map(|a| json!({"id": a.id, "tool": a.tool, "tool_name": a.tool_name, "title": a.title})).collect();
    let mut v = json!({
        "id": info.id,
        "name": info.name,
        "kind": info.kind,
        "machine": machine(&info.target),
        "cwd": info.cwd,
        "status": info.status,
        "stand": stand,
        "mode": info.mode,
        "model": info.model,
        "queued": info.queued,
        "summary": info.summary,
        "files_changed": info.files_changed,
        "lines_added": info.lines_added,
        "lines_removed": info.lines_removed,
        "cost_usd": info.cost_usd,
    });
    if let Some(o) = v.as_object_mut() {
        if !needs.is_empty() {
            o.insert("needs".into(), json!(needs));
            // It's a person's to answer, wherever they are: never another agent's.
            o.insert("needs_who".into(), json!("a person (in kiln, on the agent's page or Mission Control)"));
        }
        for (k, val) in [("doing", info.doing.as_ref().map(|d| json!(d))), ("parent", info.parent.as_ref().map(|p| json!(p))), ("plan_run", info.plan_run.as_ref().map(|p| json!(p))), ("last_turn", info.last_turn.as_ref().map(|t| json!(t))), ("pause", info.pause.as_ref().map(|p| json!(p))), ("exit", info.exit.as_ref().map(|e| json!(e)))] {
            if let Some(val) = val {
                o.insert(k.into(), val);
            }
        }
    }
    v
}

/// What a wait waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    /// Each agent is idle, needs a person, is paused, has stopped or has ended.
    Settled,
    /// Anything about any of them changed.
    Change,
}

/// The agent that waits, and the turn it was in when its wait began (`Info::turn_started`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiter {
    pub id: String,
    turn: Option<i64>,
}

impl Waiter {
    /// Agent `id`, in the turn it's in now.
    pub fn of(hub: &Hub, id: &str) -> Self {
        Self { id: id.to_owned(), turn: hub.harness.info(id).and_then(|i| i.turn_started) }
    }

    pub(super) fn turn(&self) -> Option<i64> { self.turn }
}

/// The source verifier uses this same schema and byte order, independent of JSON object key
/// insertion order. Only this subtree is proven; outer wait status and timing are not mail.
pub(super) fn handoff_digest(handoff: &str, turn: Option<i64>, messages: &[Value]) -> Result<String, super::state_reads::Failure> {
    use super::state_reads::Failure;
    if handoff.is_empty() || messages.is_empty() || messages.len() > HAND_MAX { return Err(Failure::Malformed); }
    #[derive(Serialize)]
    struct Message<'a> { id: &'a str, from: &'a str, name: Option<&'a str>, text: &'a str, at: i64 }
    #[derive(Serialize)]
    struct Response<'a> { handoff: &'a str, turn: Option<i64>, messages: Vec<Message<'a>> }
    let mut canonical = Vec::new();
    canonical.try_reserve_exact(messages.len()).map_err(|_| Failure::Source(super::state_view::Error::Budget))?;
    for message in messages {
        let object = message.as_object().ok_or(Failure::Malformed)?;
        if object.len() != 5 { return Err(Failure::Malformed); }
        let text = |key: &str| object.get(key).and_then(Value::as_str).ok_or(Failure::Malformed);
        let name = match object.get("name") { Some(Value::Null) => None, Some(Value::String(name)) => Some(name.as_str()), _ => return Err(Failure::Malformed) };
        canonical.push(Message { id: text("id")?, from: text("from")?, name, text: text("text")?, at: object.get("at").and_then(Value::as_i64).ok_or(Failure::Malformed)? });
    }
    struct Hasher { digest: sha2::Sha256, bytes: usize }
    impl Write for Hasher {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.checked_add(bytes.len()).filter(|size| *size <= super::state_view::FRAME).ok_or_else(|| std::io::Error::other("handoff response budget"))?;
            self.digest.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut hash = Hasher { digest: sha2::Sha256::new(), bytes: 0 };
    serde_json::to_writer(&mut hash, &Response { handoff, turn, messages: canonical }).map_err(|_| Failure::Source(super::state_view::Error::Budget))?;
    Ok(hex::encode(hash.digest.finalize()))
}

/// One wait checks each source queue revision once. A failed check remains uncertain and does
/// not turn unrelated events or the existing fallback look into source retries.
#[derive(Default)]
enum MailProbe {
    #[default]
    Unchecked,
    Checked { revision: String },
    Failed { revision: String, error: super::state_reads::Failure },
}

impl MailProbe {
    fn needed<'a>(&self, waiter: &Waiter, info: &'a Info) -> Option<&'a str> {
        if !info.queue_partial || info.status != Status::Running || info.turn_started != waiter.turn { return None; }
        let revision = info.queue_revision.as_deref()?;
        let tried = match self {
            Self::Unchecked => None,
            Self::Checked { revision } | Self::Failed { revision, .. } => Some(revision.as_str()),
        };
        (tried != Some(revision)).then_some(revision)
    }

    fn record(&mut self, revision: String, result: Result<bool, super::state_reads::Failure>) -> bool {
        match result {
            Ok(found) => { *self = Self::Checked { revision }; found }
            Err(error) => { *self = Self::Failed { revision, error }; false }
        }
    }

    fn unresolved(&self, waiter: &Waiter, info: &Info) -> Option<super::state_reads::Failure> {
        if !info.queue_partial || info.turn_started != waiter.turn || matches!(info.status, Status::Idle | Status::Exited) { return None; }
        let Some(current) = info.queue_revision.as_deref() else { return Some(super::state_reads::Failure::NotReady) };
        match self {
            Self::Failed { revision, error } if revision == current => Some(*error),
            _ => None,
        }
    }
}

/// What a wait would hand `waiter` now, its queue as `info` has it: the messages other agents sent
/// it (not its own to itself) waiting behind the turn its wait began in, those not handed over
/// already, as many as one answer holds ([`HAND_MAX`], [`HAND_BYTES`]), by id. Nothing once that
/// turn is over: a wait left running past it (the turn interrupted, a wait in the background) hands
/// nothing over, as the agent takes its queue a turn at a time.
pub fn mail(waiter: &Waiter, info: &Info) -> Vec<String> {
    let (mut out, mut bytes) = (Vec::new(), 0usize);
    mail_page(waiter, info, &info.queue, &mut out, &mut bytes);
    out
}

/// A single index page contributes to the same existing handoff count and text-byte budget.
pub(super) fn mail_page(waiter: &Waiter, info: &Info, queue: &[super::queued::Queued], out: &mut Vec<String>, bytes: &mut usize) -> bool {
    if info.status != Status::Running || info.turn_started != waiter.turn { return true; }
    let claimed = CLAIMED.lock();
    for q in queue {
        if out.len() >= HAND_MAX {
            return true;
        }
        let ok = super::queued::sender_of(&q.id).is_some_and(|from| from != waiter.id) && !claimed.get(&waiter.id).is_some_and(|ids| ids.contains(&q.id)) && !out.contains(&q.id) && bytes.saturating_add(q.len) <= HAND_BYTES;
        if ok {
            *bytes = bytes.saturating_add(q.len);
            out.push(q.id.clone());
        }
    }
    out.len() >= HAND_MAX
}

pub(super) struct Claim { owner: String, id: String }

impl Drop for Claim {
    fn drop(&mut self) {
        let mut claimed = CLAIMED.lock();
        if claimed.get_mut(&self.owner).is_some_and(|ids| { ids.remove(&self.id); ids.is_empty() }) { claimed.remove(&self.owner); }
    }
}

pub(super) fn claim_for(waiter: &Waiter, id: &str) -> Option<Claim> {
    if !CLAIMED.lock().entry(waiter.id.clone()).or_default().insert(id.to_owned()) { return None; }
    Some(Claim { owner: waiter.id.clone(), id: id.to_owned() })
}

/// Wait up to `timeout` (at most [`MAX`]) for agents `ids` to stand where `until` says (all of
/// them, or with `any` the first), then say where each stands. An agent that's gone (closed) has
/// ended. `waiter`, the agent that waits, if one does: the wait ends early once a message from
/// another agent waits behind its turn ([`mail`]).
#[allow(clippy::disallowed_methods)] // The protocol deadline still bounds a wait whose event channel is unavailable.
pub async fn wait(hub: &Hub, ids: &[String], until: Until, any: bool, timeout: Duration, waiter: Option<&Waiter>) -> Value {
    // Listening before the first look: a change between the two isn't missed. An agent's info changing,
    // or its end, is on its owner's lane of the sessions' events (`Hub::harness_events`; goal 4, plan
    // 01a1a59c2a9c step 5), each lane holding its share of what the hub's ring held. None to be had (the
    // runtime's channels all in use): only the final protocol deadline triggers another snapshot.
    let mut events = hub.harness_events.subscribe(crate::hub::EVENTS.div_ceil(hub.harness_events.lanes())).ok();
    let started = Instant::now();
    let timeout = timeout.min(MAX);
    let first: Vec<Option<Info>> = ids.iter().map(|id| hub.harness.info(id)).collect();
    let snapshot = || {
        let now: Vec<Option<Info>> = ids.iter().map(|id| hub.harness.info(id)).collect();
        let mut done_each = now.iter().zip(first.iter()).map(|(n, f)| match (until, n) {
            (_, None) => true,
            (Until::Settled, Some(i)) => settled(stand(i)),
            (Until::Change, Some(i)) => f.as_ref().is_none_or(|f| i.differs(f)),
        });
        let done = if any { done_each.any(|d| d) } else { done_each.all(|d| d) };
        (now, done)
    };
    let mut mail_probe = MailProbe::Unchecked;
    // How it went, said to a test build's tests: its looks, and the events that woke it for one. On a loaded machine
    // neither shows in its time (a wait that ended at its first look took 5.7 s under load).
    #[cfg(test)]
    let (mut looks, mut heard) = (0u32, 0u32);
    loop {
        #[cfg(test)]
        {
            looks += 1;
        }
        let (mut now, mut done) = snapshot();
        let mut waiter_info = waiter.and_then(|w| hub.harness.info(&w.id));
        let mut mailed = waiter.zip(waiter_info.as_ref()).is_some_and(|(w, i)| !mail(w, i).is_empty());
        if !done && !mailed && let Some((w, info, revision)) = waiter.zip(waiter_info.as_ref()).and_then(|(w, i)| mail_probe.needed(w, i).map(|r| (w, i, r.to_owned()))) {
                let remaining = timeout.saturating_sub(started.elapsed());
                if !remaining.is_zero() {
                    let result = crate::rt::timeout(remaining, super::state_reads::has_mail(hub, w, info)).await.unwrap_or(Err(super::state_reads::Failure::TimedOut));
                    let found = mail_probe.record(revision.clone(), result);
                    // The targets and waiter may have changed while their source answered. A
                    // positive result belongs only to the same queue revision and original turn.
                    (now, done) = snapshot();
                    waiter_info = hub.harness.info(&w.id);
                    mailed = waiter_info.as_ref().is_some_and(|i| {
                        !mail(w, i).is_empty() || (found && i.status == Status::Running && i.turn_started == w.turn && i.queue_revision.as_deref() == Some(revision.as_str()))
                    });
                }
        }
        let elapsed = started.elapsed();
        if done || mailed || elapsed >= timeout {
            let agents: Vec<Value> = ids.iter().zip(now.iter()).map(|(id, n)| n.as_ref().map_or_else(|| json!({"id": id, "stand": "ended", "closed": true}), brief)).collect();
            #[cfg_attr(not(test), allow(unused_mut))]
            let mut said = json!({
                "done": done,
                "timed_out": !done && elapsed >= timeout,
                "waited_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "until": if until == Until::Settled { "settled" } else { "change" },
                "agents": agents,
            });
            if !mailed && let Some(error) = waiter.zip(waiter_info.as_ref()).and_then(|(w, i)| mail_probe.unresolved(w, i)) && let Some(o) = said.as_object_mut() { o.insert("mail_read_error".into(), json!(error)); }
            #[cfg(test)]
            if let Some(o) = said.as_object_mut() {
                o.insert("looks".into(), json!(looks));
                o.insert("heard".into(), json!(heard));
            }
            return said;
        }
        // Owner events wake the predicate; the only timer is the caller's final deadline.
        let next = started + timeout;
        loop {
            let rest = next.saturating_duration_since(Instant::now());
            if rest.is_zero() {
                break;
            }
            let Some(events) = events.as_mut() else {
                crate::rt::sleep(rest).await;
                break;
            };
            match crate::rt::timeout(rest, events.recv()).await {
                Ok(Ok(ev)) if ids.iter().map(String::as_str).chain(waiter.map(|w| w.id.as_str())).any(|id| ev.contains(id)) => {
                    #[cfg(test)]
                    {
                        heard += 1;
                    }
                    break;
                }
                Ok(Ok(_)) => {}
                // Dropped some (a busy server): look again.
                Ok(Err(async_broadcast::RecvError::Overflowed(_))) => break,
                Ok(Err(async_broadcast::RecvError::Closed)) => {
                    crate::rt::sleep(rest).await;
                    break;
                }
                Err(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{ExitInfo, TurnEnd, ToolKind};
    use crate::harness::session::ApprovalBrief;

    fn info(id: &str, status: Status) -> Info {
        Info { id: id.into(), status, ..Info::default() }
    }

    #[test]
    fn a_quiet_wait_has_only_its_initial_and_final_snapshot() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let there = crate::harness::registry::Location::Machine("fixture".into());
            hub.harness.ingest(&there, "quiet", crate::harness::core::Update::Open { info: Box::new(info("quiet", Status::Running)), since: None }).await;
            // Cross the former five-second sampling interval without any owner event.
            let result = wait(&hub, &["quiet".into()], Until::Settled, false, Duration::from_secs(6), None).await;
            assert_eq!(result.get("timed_out"), Some(&json!(true)));
            assert_eq!(result.get("heard"), Some(&json!(0)));
            assert_eq!(result.get("looks"), Some(&json!(2)));
            Ok(())
        })
    }

    #[test]
    fn where_an_agent_stands_in_a_word() {
        assert_eq!(stand(&info("w1", Status::Running)), "working");
        assert_eq!(stand(&info("w2", Status::Starting)), "working");
        assert_eq!(stand(&info("w3", Status::Idle)), "idle");
        assert_eq!(stand(&Info { queued: 1, ..info("w4", Status::Idle) }), "working", "a message waits in its queue");
        let sub =
            |status: &str| crate::harness::session::SubagentBrief { id: "s".into(), tool_id: None, description: "audit".into(), status: status.into(), parent_task: None, started: 1, ended: None, last_tool: None, pausable: false, paused_at: None, perms: None };
        assert_eq!(stand(&Info { subagents: vec![sub("running")], ..info("w4b", Status::Idle) }), "working", "a subagent it sent off still works");
        assert_eq!(stand(&Info { subagents: vec![sub("completed")], ..info("w4c", Status::Idle) }), "idle", "its subagents are done");
        assert_eq!(stand(&info("w5", Status::Waiting)), "needs_you");
        let asking = Info { approvals: vec![ApprovalBrief { id: "p".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "rm".into(), at: 1, agent: None }], ..info("w6", Status::Running) };
        assert_eq!(stand(&asking), "needs_you");
        assert_eq!(stand(&Info { exit: Some(ExitInfo { code: Some(1), error: Some("boom".into()), ..Default::default() }), ..info("w7", Status::Exited) }), "stopped");
        assert_eq!(stand(&info("w8", Status::Exited)), "ended");
        assert_eq!(stand(&info("w9", Status::Disconnected)), "offline");
        let paused = Info { pause: Some(crate::harness::model::Pause { reason: "limit".into(), since: 1, auto: true, by: "harness".into(), ..Default::default() }), ..info("w10", Status::Idle) };
        assert_eq!(stand(&paused), "paused");
        assert!(settled("idle") && settled("needs_you") && settled("ended") && settled("stopped") && settled("paused"));
        assert!(!settled("working") && !settled("offline"));
    }

    #[test]
    fn deferred_approvals_still_need_a_person() {
        use crate::harness::state_view::{Descriptor, Kind};
        let mut agent = info("deferred-approvals", Status::Running);
        assert_eq!(stand(&agent), "working");
        agent.deferred.insert("approvals".into(), Descriptor { kind: Kind::Array, len: 1 });
        assert_eq!(stand(&agent), "needs_you", "the source retained a pending approval");
        agent.deferred.insert("approvals".into(), Descriptor { kind: Kind::Array, len: 0 });
        assert_eq!(stand(&agent), "working", "an empty approval array needs no answer");
        agent.deferred.insert("approvals".into(), Descriptor { kind: Kind::Text, len: 1 });
        assert_eq!(stand(&agent), "working", "only an array descriptor counts approvals");
    }

    #[test]
    fn handoff_digest_binds_exact_response_and_normalizes_object_order() -> anyhow::Result<()> {
        let message = json!({"id":"original-1","from":"peer-1","name":null,"text":"exact 日本語 \"\\\nbody","at":7});
        let reordered: Value = serde_json::from_str(r#"{"at":7,"text":"exact 日本語 \"\\\nbody","name":null,"from":"peer-1","id":"original-1"}"#)?;
        let digest = handoff_digest("handoff-1", Some(100), std::slice::from_ref(&message)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        assert_eq!(digest,"4f321f0d9414582e25d29dc364893ae2ea6caaf760b4335853894ba7706329d8");
        assert_eq!(handoff_digest("handoff-1",Some(100),&[reordered]),Ok(digest.clone()));
        for (key,value) in [("id",json!("other-id")),("from",json!("other-peer")),("name",json!("new name")),("text",json!("different original")),("at",json!(8))] {
            let mut changed=message.clone();
            changed.as_object_mut().ok_or_else(||anyhow::anyhow!("message is not an object"))?.insert(key.into(),value);
            assert_ne!(handoff_digest("handoff-1",Some(100),&[changed]),Ok(digest.clone()),"{key} is part of the proof");
        }
        assert_ne!(handoff_digest("other-handoff",Some(100),std::slice::from_ref(&message)),Ok(digest.clone()));
        assert_ne!(handoff_digest("handoff-1",Some(101),std::slice::from_ref(&message)),Ok(digest));
        Ok(())
    }

    #[test]
    fn malformed_or_oversized_handoff_payload_is_not_a_digest() {
        use super::super::state_reads::Failure;
        let message=json!({"id":"original","from":"peer","name":null,"text":"body","at":1});
        assert_eq!(handoff_digest("",Some(1),std::slice::from_ref(&message)),Err(Failure::Malformed));
        assert_eq!(handoff_digest("handoff",Some(1),&[]),Err(Failure::Malformed));
        assert_eq!(handoff_digest("handoff",Some(1),&vec![message;HAND_MAX+1]),Err(Failure::Malformed));
        assert_eq!(handoff_digest("handoff",Some(1),&[json!({"id":"original","from":"peer","name":null,"text":"body","at":1,"extra":"unproven"})]),Err(Failure::Malformed));
        assert_eq!(handoff_digest("handoff",Some(1),&[json!({"id":"original","from":"peer","name":false,"text":"body","at":1})]),Err(Failure::Malformed));
        let huge=json!({"id":"original","from":"peer","name":null,"text":"\"".repeat(super::super::state_view::FRAME),"at":1});
        assert_eq!(handoff_digest("handoff",Some(1),&[huge]),Err(Failure::Source(super::super::state_view::Error::Budget)));
    }

    #[test]
    fn concurrent_handoff_claims_are_recipient_scoped_and_end_with_request() -> anyhow::Result<()> {
        let a=Waiter { id:"claim-owner-a".into(),turn:Some(1) };
        let b=Waiter { id:"claim-owner-b".into(),turn:Some(1) };
        let first=claim_for(&a,"same-original").ok_or_else(||anyhow::anyhow!("no first claim"))?;
        assert!(claim_for(&a,"same-original").is_none());
        let second=claim_for(&b,"same-original").ok_or_else(||anyhow::anyhow!("another recipient was blocked"))?;
        drop(first);
        assert!(!CLAIMED.lock().contains_key(&a.id));
        assert!(claim_for(&a,"same-original").is_some(),"request cancellation releases only the transient claim");
        assert!(claim_for(&b,"same-original").is_none());
        drop(second);
        assert!(!CLAIMED.lock().contains_key(&b.id));
        Ok(())
    }

    #[test]
    fn a_partial_mail_probe_is_consumed_only_by_its_queue_revision() {
        use crate::harness::state_reads::Failure;
        let waiter = Waiter { id: "probe-waiter".into(), turn: Some(100) };
        let mut agent = Info { queue_partial: true, queue_revision: Some("queue-1".into()), turn_started: waiter.turn, ..info(&waiter.id, Status::Running) };
        let mut probe = MailProbe::Unchecked;
        assert_eq!(probe.needed(&waiter, &agent), Some("queue-1"));
        assert!(!probe.record("queue-1".into(), Ok(false)));
        assert_eq!(probe.needed(&waiter, &agent), None, "an unchanged fallback look does not read again");
        agent.summary = "unrelated status detail".into();
        assert_eq!(probe.needed(&waiter, &agent), None);
        agent.queue_revision = Some("queue-2".into());
        assert_eq!(probe.needed(&waiter, &agent), Some("queue-2"));
        assert!(!probe.record("queue-2".into(), Err(Failure::Unreachable)));
        assert!(matches!(probe, MailProbe::Failed { .. }), "failure is not a successful negative");
        assert_eq!(probe.unresolved(&waiter, &agent), Some(Failure::Unreachable));
        assert_eq!(probe.needed(&waiter, &agent), None, "unrelated events do not retry a failed revision");
        agent.status = Status::Disconnected;
        assert_eq!(probe.unresolved(&waiter, &agent), Some(Failure::Unreachable));
        agent.status = Status::Running;
        assert_eq!(probe.needed(&waiter, &agent), None, "reconnection alone does not reset the revision key");
        agent.queue_revision = Some("queue-3".into());
        assert_eq!(probe.needed(&waiter, &agent), Some("queue-3"));
        agent.queue_partial = false;
        assert_eq!(probe.needed(&waiter, &agent), None);
        assert_eq!(probe.unresolved(&waiter, &agent), None, "a complete index resolves the former uncertainty");
        agent.queue_partial = true;
        agent.turn_started = Some(200);
        assert_eq!(probe.needed(&waiter, &agent), None, "never read mail for a later turn");
        assert_eq!(probe.unresolved(&waiter, &agent), None);
        agent.turn_started = waiter.turn;
        agent.status = Status::Idle;
        assert_eq!(probe.needed(&waiter, &agent), None);
        agent.status = Status::Running;
        agent.queue_revision = None;
        assert_eq!(probe.needed(&waiter, &agent), None, "the owner must supply a queue fence first");
        assert_eq!(probe.unresolved(&waiter, &agent), Some(Failure::NotReady));
    }

    #[test]
    fn a_later_index_page_can_wake_a_wait_without_claiming_mail() {
        use crate::harness::queued::{Queued, from_agent};
        let waiter = Waiter { id: "suffix-waiter".into(), turn: Some(100) };
        let agent = Info { queue_partial: true, queue_revision: Some("suffix-revision".into()), turn_started: waiter.turn, queue: vec![Queued::of("from-person", "later")], ..info(&waiter.id, Status::Running) };
        assert!(mail(&waiter, &agent).is_empty(), "the bounded prefix has no sender mail");
        let peer = from_agent("01a1suffixpeer000000000000000000");
        let suffix = [Queued::of("more-from-person", "later"), Queued::of(&peer, "the source has replied")];
        let (mut out, mut bytes) = (Vec::new(), 0);
        mail_page(&waiter, &agent, &agent.queue, &mut out, &mut bytes);
        mail_page(&waiter, &agent, &suffix, &mut out, &mut bytes);
        let mut probe = MailProbe::Unchecked;
        assert!(probe.record("suffix-revision".into(), Ok(!out.is_empty())), "positive suffix metadata ends the wait");
        assert_eq!(out, std::slice::from_ref(&peer));
        assert!(!CLAIMED.lock().get(&waiter.id).is_some_and(|ids| ids.contains(&peer)), "only the existing handoff path claims mail");
    }

    /// Attention plan 01a199e1b177: queued input and admission wait are work still to take up.
    #[test]
    fn mc_attention_wait_queued_input_and_room_wait_stay_working_until_cleared() {
        let mut agent = info("attention-wait-room", Status::Idle);
        agent.room_wait = Some(1);
        assert_eq!(stand(&agent), "working", "waiting for room isn't idle");
        assert!(!settled(stand(&agent)));
        agent.queued = 1;
        agent.room_wait = None;
        assert_eq!(stand(&agent), "working", "room granted, its queued message still waits");
        agent.queued = 0;
        assert_eq!(stand(&agent), "idle", "the queue and admission wait are clear");
        agent.room_wait = Some(2);
        assert_eq!(stand(&agent), "working");
        agent.room_wait = None;
        assert_eq!(stand(&agent), "idle", "clearing admission wait removes its working override");
    }

    #[test]
    fn mc_attention_wait_a_persons_pause_wins_over_live_work_until_resumed() {
        let mut agent = Info { frozen: Some(1), queued: 1, room_wait: Some(1), ..info("attention-wait-frozen", Status::Idle) };
        for status in [Status::Idle, Status::Starting, Status::Running] {
            agent.status = status;
            assert_eq!(stand(&agent), "paused", "the person's pause freezes a live agent");
            assert!(settled(stand(&agent)), "a wait can return while the person has paused it");
        }
        agent.frozen = None;
        assert_eq!(stand(&agent), "working", "resuming the turn clears the pause override");
        agent.status = Status::Idle;
        assert_eq!(stand(&agent), "working", "queued input still has to go");
        agent.queued = 0;
        agent.room_wait = None;
        assert_eq!(stand(&agent), "idle");

        let mut in_flight = Info { frozen: Some(1), last_turn: Some(TurnEnd { at: 100, how: "done".into(), ..TurnEnd::default() }), ..info("attention-wait-frozen-send", Status::Idle) };
        sent(&in_flight.id, Some(100));
        assert_eq!(stand(&in_flight), "paused", "a message in flight doesn't undo a person's pause");
        in_flight.frozen = None;
        assert_eq!(stand(&in_flight), "working", "resume retains the message still to take up");
        in_flight.last_turn = Some(TurnEnd { at: 200, how: "done".into(), ..TurnEnd::default() });
        assert_eq!(stand(&in_flight), "idle", "its completed turn clears the in-flight marker");
    }

    #[test]
    fn mc_attention_wait_requests_and_held_goals_win_over_pause_and_pending_work() {
        let mut agent = Info {
            frozen: Some(1), queued: 1, room_wait: Some(1),
            held: Some(crate::harness::session::Held { why: "Please review the plan".into(), plan: Some("plan".into()), since: 1 }),
            ..info("attention-wait-held", Status::Idle)
        };
        assert_eq!(stand(&agent), "needs_you", "goals held for a person remain actionable");
        agent.held = None;
        assert_eq!(stand(&agent), "paused", "clearing the hold reveals the person's pause");
        agent.status = Status::Waiting;
        assert_eq!(stand(&agent), "needs_you", "a harness request wins over frozen");
        agent.status = Status::Running;
        agent.approvals.push(ApprovalBrief { id: "permission".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "approve".into(), at: 1, agent: None });
        assert_eq!(stand(&agent), "needs_you", "an approval wins over frozen and running");
        agent.approvals.clear();
        assert_eq!(stand(&agent), "paused", "answering the request doesn't resume a person's pause");
        agent.frozen = None;
        assert_eq!(stand(&agent), "working");
        agent.status = Status::Idle;
        agent.queued = 0;
        agent.room_wait = None;
        assert_eq!(stand(&agent), "idle", "no stale held/request override remains");
    }

    #[test]
    fn mc_attention_wait_pause_flags_preserve_exit_offline_and_usage_semantics() {
        let mut agent = Info {
            frozen: Some(1), queued: 1, room_wait: Some(1),
            pause: Some(crate::harness::model::Pause { reason: "limit".into(), ..Default::default() }),
            ..info("attention-wait-lifecycle", Status::Disconnected)
        };
        assert_eq!(stand(&agent), "offline", "retained pause and queue flags aren't a connection");
        agent.status = Status::Exited;
        assert_eq!(stand(&agent), "ended");
        agent.exit = Some(ExitInfo { error: Some("failed".into()), ..Default::default() });
        assert_eq!(stand(&agent), "stopped");
        agent.exit = Some(ExitInfo { error: Some("failed".into()), again_at: Some(2), ..Default::default() });
        assert_eq!(stand(&agent), "working", "an automatic restart remains work to resume");
        agent.status = Status::Running;
        assert_eq!(stand(&agent), "paused", "explicit freezing stops the running process");
        agent.frozen = None;
        assert_eq!(stand(&agent), "working", "a usage pause lets its running turn finish");
        agent.status = Status::Idle;
        assert_eq!(stand(&agent), "paused", "the usage pause wins over queued input once idle");
        agent.pause = None;
        assert_eq!(stand(&agent), "working");
    }

    #[test]
    fn a_message_on_its_way_keeps_an_idle_agent_working_until_its_turn_ends() {
        let before = Some(TurnEnd { at: 100, how: "done".into(), ..TurnEnd::default() });
        let idle = Info { last_turn: before.clone(), ..info("m1", Status::Idle) };
        sent("m1", Some(100));
        assert_eq!(stand(&idle), "working", "sent, not yet taken up");
        // Its turn ended since: done.
        let after = Info { last_turn: Some(TurnEnd { at: 200, how: "done".into(), ..TurnEnd::default() }), ..idle.clone() };
        assert_eq!(stand(&after), "idle");
        assert_eq!(stand(&idle), "idle", "answered once, forgotten");
        // A stopped agent a message starts again isn't ended while it's on its way.
        sent("m2", None);
        assert_eq!(stand(&info("m2", Status::Exited)), "working");
    }

    /// The real wait, on a registry with no agents and with one that changes, under a timeout:
    /// nothing it does holds a lock across its waiting.
    #[test]
    fn a_wait_returns_when_its_agents_settle_or_its_time_is_up() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub().await?;
                // A closed (unknown) agent has ended: at once.
                let gone = wait(&hub, &["nobody".to_owned()], Until::Settled, false, Duration::from_secs(30), None).await;
                anyhow::ensure!(gone.get("done") == Some(&json!(true)) && gone.pointer("/agents/0/stand") == Some(&json!("ended")), "{gone}");
                // Nothing changes within the timeout: timed out, quickly.
                let t = Instant::now();
                let quiet = wait(&hub, &["nobody".to_owned()], Until::Change, false, Duration::from_millis(300), None).await;
                anyhow::ensure!(quiet.get("done") == Some(&json!(true)), "a closed agent counts as changed: {quiet}");
                anyhow::ensure!(t.elapsed() < Duration::from_secs(5));
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the wait hung"),
        }
    }

    #[test]
    fn a_source_read_cannot_outlive_the_wait_deadline() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::State;
        use crate::harness::state_view::{Counts, Descriptor, Kind, Metadata, QueueIndex};
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                // This source never answers, and its protocol clock never advances. Only the
                // caller's remaining wait duration can cancel its in-flight source read.
                let hub = crate::hub::tests::hub_with_runner(std::sync::Arc::new(kiln_seams::clock::SimClock::new()), "wait-deadline-source", crate::proto::CAP_WIRE).await?;
                let there = crate::harness::registry::Location::Machine("wait-deadline-source".into());
                for id in ["deadline-target", "deadline-waiter"] {
                    hub.harness.ingest(&there, id, Update::Open { info: Box::new(info(id, Status::Running)), since: None }).await;
                }
                let revision = "deadline-queue".to_owned();
                let metadata = Metadata {
                    revision: "deadline-state".into(),
                    revisions: [("queue".into(), revision)].into(),
                    deferred: [("queue".into(), Descriptor { kind: Kind::Array, len: 1 })].into(),
                    counts: Counts { queue: 1, ..Counts::default() },
                    queue_index: Some(QueueIndex { entries: Vec::new(), complete: false, next: Some(0) }),
                };
                hub.harness.ingest(&there, "deadline-waiter", Update::State { state: Box::new(State { status: Status::Running, view: Some(metadata), ..State::default() }) }).await;
                let waiter = Waiter::of(&hub, "deadline-waiter");
                let answer = wait(&hub, &["deadline-target".into()], Until::Settled, false, Duration::from_millis(50), Some(&waiter)).await;
                anyhow::ensure!(answer.get("timed_out") == Some(&json!(true)), "{answer}");
                anyhow::ensure!(answer.pointer("/mail_read_error/code") == Some(&json!("timed_out")), "a cancelled source read stays unknown: {answer}");
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the wait outlived its deadline while reading source mail"),
        }
    }

    /// What a wait hands over: other agents' messages waiting behind the turn the wait began in,
    /// not the waiter's own to itself, not once that turn is over (interrupted, or the wait left
    /// running in the background), at most what one answer holds, and each once.
    #[test]
    fn a_wait_hands_over_others_messages_to_its_own_turn_each_once_as_many_as_fit() {
        use crate::harness::queued::{Queued, from_agent};
        let me = "01a1waiter0000000000000000000000";
        let waiter = Waiter { id: me.into(), turn: Some(100) };
        let at_work = |queue: Vec<Queued>| Info { id: me.into(), status: Status::Running, turn_started: Some(100), queued: queue.len(), queue, ..Info::default() };
        let (peer, own) = (from_agent("01a1peer000000000000000000000000"), from_agent(me));
        let info = at_work(vec![Queued::of("m-person", "later"), Queued::of(&own, "a note to myself"), Queued::of(&peer, "the registry passes")]);
        assert_eq!(mail(&waiter, &info), std::slice::from_ref(&peer), "another agent's only");
        // The turn it began in is over: nothing, whether a new turn runs or none does.
        assert!(mail(&waiter, &Info { turn_started: Some(200), ..info.clone() }).is_empty(), "a turn since");
        assert!(mail(&waiter, &Info { status: Status::Idle, turn_started: None, ..info.clone() }).is_empty(), "between turns");
        // As many as one answer holds: one too long on its own waits for the turn's end.
        let many: Vec<Queued> = (0..HAND_MAX + 4).map(|_| Queued::of(&from_agent("01a1peer000000000000000000000000"), "short")).collect();
        assert_eq!(mail(&waiter, &at_work(many)).len(), HAND_MAX);
        let long = "x".repeat(HAND_BYTES + 1);
        let big = from_agent("01a1peer000000000000000000000000");
        assert_eq!(mail(&waiter, &at_work(vec![Queued::of(&big, &long), Queued::of(&peer, "the registry passes")])), std::slice::from_ref(&peer));
        // Each once: taken by one wait, it isn't another's, nor again while kiln's view lags.
        let claimed = claim_for(&waiter, &peer);
        assert!(claimed.is_some() && claim_for(&waiter, &peer).is_none());
        assert!(mail(&waiter, &info).is_empty(), "handed over already");
        drop(claimed);
        assert_eq!(mail(&waiter, &info), [peer], "the source, not a timed server cache, owns delivery proof after response assembly");
    }

    /// An agent in a long wait for a child at work: a message from a person in its queue leaves the
    /// wait to its time, one from another agent ends it at once, and one that comes during the wait
    /// wakes it (its queue's change, not the next look).
    #[test]
    fn a_wait_ends_when_a_message_from_another_agent_waits_in_the_waiter_s_queue() -> anyhow::Result<()> {
        use crate::harness::core::Update;
        use crate::harness::model::{QueueItem, State};
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub().await?;
                let there = crate::harness::registry::Location::Machine("elsewhere".into());
                for id in ["at-work", "waiter"] {
                    hub.harness.ingest(&there, id, Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), status: Status::Running, ..Info::default() }), since: None }).await;
                }
                let item = |id: String, text: &str| QueueItem { id, text: text.into(), at: 1, attachments: Vec::new(), overrides: None, reply_to: None };
                let queue = |items: Vec<QueueItem>| Update::State { state: Box::new(State { status: Status::Running, queue: items, ..State::default() }) };
                let ids = ["at-work".to_owned()];
                hub.harness.ingest(&there, "waiter", queue(vec![item("m1".into(), "From a person: later.")])).await;
                let me = Waiter::of(&hub, "waiter");
                let t = Instant::now();
                let quiet = wait(&hub, &ids, Until::Settled, false, Duration::from_millis(600), Some(&me)).await;
                anyhow::ensure!(quiet.get("timed_out") == Some(&json!(true)) && t.elapsed() >= Duration::from_millis(600), "a person's message waits for the turn: {quiet}");
                let peer = item(crate::harness::queued::from_agent("01a15c10306176b4a096f6c8d3100c54"), "Which commit has the fix?");
                // In its queue when it starts waiting: it ends at once.
                hub.harness.ingest(&there, "waiter", queue(vec![item("m1".into(), "From a person: later."), peer.clone()])).await;
                // At once: on its first look, however long a loaded machine takes over it (5.7 s under load).
                let early = wait(&hub, &ids, Until::Settled, false, Duration::from_secs(30), Some(&me)).await;
                anyhow::ensure!(early.get("done") == Some(&json!(false)) && early.get("timed_out") == Some(&json!(false)) && early.get("looks") == Some(&json!(1)), "{early}");
                anyhow::ensure!(early.pointer("/agents/0/stand") == Some(&json!("working")), "it still says where each stands: {early}");
                // Not the waiter's own: it isn't woken by another's queue.
                let other = wait(&hub, &ids, Until::Settled, false, Duration::from_millis(300), None).await;
                anyhow::ensure!(other.get("timed_out") == Some(&json!(true)), "{other}");
                // Coming during the wait: woken by it (its queue's change, an event), before the final deadline: told
                // by what woke it, not by its time, which a loaded machine stretches past the next look's.
                hub.harness.ingest(&there, "waiter", queue(vec![])).await;
                let (woken, ()) = futures::join!(wait(&hub, &ids, Until::Settled, false, Duration::from_secs(30), Some(&me)), async {
                    crate::rt::sleep(Duration::from_millis(200)).await;
                    hub.harness.ingest(&there, "waiter", queue(vec![peer.clone()])).await;
                });
                // Every look after its first was an event's (none the next look's), and one was.
                let (looks, heard) = (woken.get("looks").and_then(serde_json::Value::as_u64), woken.get("heard").and_then(serde_json::Value::as_u64));
                anyhow::ensure!(woken.get("timed_out") == Some(&json!(false)) && heard.is_some_and(|h| h >= 1) && looks == heard.map(|h| h + 1), "{woken}");
                Ok(())
            });
            let _ = tx.send(r);
        });
        // A hang's bound, never a pace.
        match rx.recv_timeout(Duration::from_secs(300)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the wait hung"),
        }
    }
}
