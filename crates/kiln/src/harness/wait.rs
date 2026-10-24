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
//! `agents.wait` hands it over then (`super::ops`), to the turn the wait began in only, each message
//! once, as many as one answer holds.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

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
/// How often a wait reads the agents again without an event of theirs.
const RECHECK: Duration = Duration::from_secs(5);

/// Messages sent to agents and not yet answered: how the agent's last turn had ended when it went
/// (its `last_turn.at`), and when it went.
static SENT: LazyLock<Mutex<HashMap<String, (Option<i64>, Instant)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The most messages one wait hands over, and the most of their words (bytes), all told: the rest
/// wait for the next wait, or the turn's end (one longer than that on its own, too).
const HAND_MAX: usize = 16;
const HAND_BYTES: usize = 48 * 1024;
/// How long a message handed over is remembered: the server's view of the queue, a moment behind,
/// doesn't hand it over again.
const HANDED_FOR: Duration = Duration::from_secs(120);
/// The most handed over remembered at once.
const HANDED_MAX: usize = 4096;
/// The messages handed over lately, by id, and when.
static HANDED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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

/// Where an agent stands, in a word: `needs_you` (an approval, a question or a plan waits on a
/// person), `working`, `idle` (its turn is over, nothing queued), `paused` (a usage limit),
/// `stopped` (it crashed or couldn't start), `offline` (its machine is away), `ended`.
pub fn stand(info: &Info) -> &'static str {
    if !info.approvals.is_empty() || info.status == Status::Waiting {
        return "needs_you";
    }
    if on_its_way(info) {
        return "working";
    }
    match info.status {
        Status::Exited if info.exit.as_ref().is_some_and(|e| e.error.is_some()) => "stopped",
        Status::Exited => "ended",
        Status::Disconnected => "offline",
        _ if info.pause.is_some() && info.status != Status::Running => "paused",
        Status::Running | Status::Starting | Status::Waiting => "working",
        Status::Idle if info.queued > 0 => "working",
        // Its turn is over but a subagent it sent off still works: it isn't done, and it goes on
        // when the subagent reports back.
        Status::Idle if info.subagents.iter().any(|s| matches!(s.status.as_str(), "running" | "pending" | "paused")) => "working",
        Status::Idle => "idle",
    }
}

/// Settled: where a wait for an agent to finish what it was given ends.
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
    /// Each agent is idle, needs a person, is paused for a limit, has stopped or has ended.
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
}

/// What a wait would hand `waiter` now, its queue as `info` has it: the messages other agents sent
/// it (not its own to itself) waiting behind the turn its wait began in, those not handed over
/// already, as many as one answer holds ([`HAND_MAX`], [`HAND_BYTES`]), by id. Nothing once that
/// turn is over: a wait left running past it (the turn interrupted, a wait in the background) hands
/// nothing over, as the agent takes its queue a turn at a time.
pub fn mail(waiter: &Waiter, info: &Info) -> Vec<String> {
    if info.status != Status::Running || info.turn_started != waiter.turn {
        return Vec::new();
    }
    let handed = HANDED.lock();
    let (mut out, mut bytes) = (Vec::new(), 0usize);
    for q in &info.queue {
        if out.len() >= HAND_MAX {
            break;
        }
        let ok = super::queued::sender_of(&q.id).is_some_and(|from| from != waiter.id) && !handed.contains_key(&q.id) && bytes.saturating_add(q.len) <= HAND_BYTES;
        if ok {
            bytes = bytes.saturating_add(q.len);
            out.push(q.id.clone());
        }
    }
    out
}

/// Takes message `id` for one wait to hand over: false when it's handed over already (by a wait
/// beside this one, or one a moment ago).
pub fn claim(id: &str) -> bool {
    let mut g = HANDED.lock();
    g.retain(|_, at| at.elapsed() < HANDED_FOR);
    if g.len() >= HANDED_MAX || g.contains_key(id) {
        return false;
    }
    g.insert(id.to_owned(), Instant::now());
    true
}

/// Wait up to `timeout` (at most [`MAX`]) for agents `ids` to stand where `until` says (all of
/// them, or with `any` the first), then say where each stands. An agent that's gone (closed) has
/// ended. `waiter`, the agent that waits, if one does: the wait ends early once a message from
/// another agent waits behind its turn ([`mail`]).
pub async fn wait(hub: &Hub, ids: &[String], until: Until, any: bool, timeout: Duration, waiter: Option<&Waiter>) -> Value {
    // Listening before the first look: a change between the two isn't missed.
    let mut events = hub.events.subscribe();
    let started = Instant::now();
    let timeout = timeout.min(MAX);
    let first: Vec<Option<Info>> = ids.iter().map(|id| hub.harness.info(id)).collect();
    loop {
        let now: Vec<Option<Info>> = ids.iter().map(|id| hub.harness.info(id)).collect();
        let done_each: Vec<bool> = now
            .iter()
            .zip(first.iter())
            .map(|(n, f)| match (until, n) {
                (_, None) => true,
                (Until::Settled, Some(i)) => settled(stand(i)),
                (Until::Change, Some(i)) => f.as_ref().is_none_or(|f| i.differs(f)),
            })
            .collect();
        let done = if any { done_each.iter().any(|d| *d) } else { done_each.iter().all(|d| *d) };
        let mailed = waiter.is_some_and(|w| hub.harness.info(&w.id).is_some_and(|i| !mail(w, &i).is_empty()));
        let elapsed = started.elapsed();
        if done || mailed || elapsed >= timeout {
            let agents: Vec<Value> = ids.iter().zip(now.iter()).map(|(id, n)| n.as_ref().map_or_else(|| json!({"id": id, "stand": "ended", "closed": true}), brief)).collect();
            return json!({
                "done": done,
                "timed_out": !done && elapsed >= timeout,
                "waited_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                "until": if until == Until::Settled { "settled" } else { "change" },
                "agents": agents,
            });
        }
        // An event of theirs (or the waiter's: a message in its queue), or the next look anyway.
        let next = Instant::now() + RECHECK.min(timeout.saturating_sub(elapsed));
        loop {
            let rest = next.saturating_duration_since(Instant::now());
            if rest.is_zero() {
                break;
            }
            match crate::rt::timeout(rest, events.recv()).await {
                Ok(Ok(ev)) if ids.iter().map(String::as_str).chain(waiter.map(|w| w.id.as_str())).any(|id| ev.contains(id)) => break,
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
    fn where_an_agent_stands_in_a_word() {
        assert_eq!(stand(&info("w1", Status::Running)), "working");
        assert_eq!(stand(&info("w2", Status::Starting)), "working");
        assert_eq!(stand(&info("w3", Status::Idle)), "idle");
        assert_eq!(stand(&Info { queued: 1, ..info("w4", Status::Idle) }), "working", "a message waits in its queue");
        let sub =
            |status: &str| crate::harness::session::SubagentBrief { id: "s".into(), tool_id: None, description: "audit".into(), status: status.into(), parent_task: None, started: 1, ended: None };
        assert_eq!(stand(&Info { subagents: vec![sub("running")], ..info("w4b", Status::Idle) }), "working", "a subagent it sent off still works");
        assert_eq!(stand(&Info { subagents: vec![sub("completed")], ..info("w4c", Status::Idle) }), "idle", "its subagents are done");
        assert_eq!(stand(&info("w5", Status::Waiting)), "needs_you");
        let asking = Info { approvals: vec![ApprovalBrief { id: "p".into(), tool: ToolKind::Shell, tool_name: "Bash".into(), title: "rm".into(), at: 1 }], ..info("w6", Status::Running) };
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
        assert!(claim(&peer) && !claim(&peer));
        assert!(mail(&waiter, &info).is_empty(), "handed over already");
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
                    hub.harness.ingest(&hub, &there, id, Update::Open { info: Box::new(Info { id: id.into(), kind: "claude".into(), status: Status::Running, ..Info::default() }), since: None });
                }
                let item = |id: String, text: &str| QueueItem { id, text: text.into(), at: 1, attachments: Vec::new(), overrides: None };
                let queue = |items: Vec<QueueItem>| Update::State { state: Box::new(State { status: Status::Running, queue: items, ..State::default() }) };
                let ids = ["at-work".to_owned()];
                hub.harness.ingest(&hub, &there, "waiter", queue(vec![item("m1".into(), "From a person: later.")]));
                let me = Waiter::of(&hub, "waiter");
                let t = Instant::now();
                let quiet = wait(&hub, &ids, Until::Settled, false, Duration::from_millis(600), Some(&me)).await;
                anyhow::ensure!(quiet.get("timed_out") == Some(&json!(true)) && t.elapsed() >= Duration::from_millis(600), "a person's message waits for the turn: {quiet}");
                let peer = item(crate::harness::queued::from_agent("01a15c10306176b4a096f6c8d3100c54"), "Which commit has the fix?");
                // In its queue when it starts waiting: it ends at once.
                hub.harness.ingest(&hub, &there, "waiter", queue(vec![item("m1".into(), "From a person: later."), peer.clone()]));
                let t = Instant::now();
                let early = wait(&hub, &ids, Until::Settled, false, Duration::from_secs(30), Some(&me)).await;
                anyhow::ensure!(early.get("done") == Some(&json!(false)) && early.get("timed_out") == Some(&json!(false)) && t.elapsed() < Duration::from_secs(5), "{early}");
                anyhow::ensure!(early.pointer("/agents/0/stand") == Some(&json!("working")), "it still says where each stands: {early}");
                // Not the waiter's own: it isn't woken by another's queue.
                let other = wait(&hub, &ids, Until::Settled, false, Duration::from_millis(300), None).await;
                anyhow::ensure!(other.get("timed_out") == Some(&json!(true)), "{other}");
                // Coming during the wait: woken by it, well before the next look.
                hub.harness.ingest(&hub, &there, "waiter", queue(vec![]));
                let t = Instant::now();
                let (woken, ()) = futures::join!(wait(&hub, &ids, Until::Settled, false, Duration::from_secs(30), Some(&me)), async {
                    crate::rt::sleep(Duration::from_millis(200)).await;
                    hub.harness.ingest(&hub, &there, "waiter", queue(vec![peer.clone()]));
                });
                anyhow::ensure!(woken.get("timed_out") == Some(&json!(false)) && t.elapsed() < RECHECK, "{woken} after {:?}", t.elapsed());
                Ok(())
            });
            let _ = tx.send(r);
        });
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the wait hung"),
        }
    }
}
