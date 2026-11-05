//! bp-g2 step 9: the adapters against the protocol's core, with no carrier. A replayed sequence of
//! the hub's events reaches a client whose state equals the server's topics, each frame within its
//! budget; a session viewed through the protocol converges to what its socket's viewer holds;
//! access is decided from the topic alone.

use std::collections::{BTreeMap, HashMap};

use kiln_bp::answers::Answers;
use kiln_bp::client::{Client, ClientConfig};
use kiln_bp::conn::Lane;
use kiln_bp::frames::{Kind, Topic};
use kiln_bp::records::SessionItem;
use kiln_bp::server::{Event, Server, ServerConfig};
use kiln_bp::state::ItemKey;
use kiln_bp::tickets::{Principal, Tickets};
use kiln_bp::topic::TopicLog;
use kiln_bp::wire::{Bits, Fields};
use serde_json::{Value, json};

use super::session::SessionFeed;
use super::tests_support::*;
use super::*;
use crate::wire::events::ServerEvent;
use crate::wire::ui::SessionFrame;

const BORN: u64 = 1_793_825_000_000;

/// A server connection and a client over no carrier, the topics the server keeps, and the
/// bytes each subscription's frames took.
struct Loop {
    srv: Server,
    cli: Client,
    tickets: Tickets,
    answers: Answers,
    topics: HashMap<(u32, String), TopicLog>,
    subs: HashMap<(u32, String), u32>,
    next: u32,
    /// Each frame sent on a subscription: (sub, kind, bytes).
    frames: Vec<(u32, Kind, usize)>,
}

impl Loop {
    fn new() -> Self {
        let mut tickets = Tickets::new();
        let t = tickets.mint([1; 32], Principal { who: "ada".into(), ..Default::default() }, 0);
        let srv = Server::new(ServerConfig { compress: false, ..Default::default() }, 1, BORN, 0);
        let cli = Client::new(ClientConfig { caps: Bits(0), ..Default::default() }, &t, 0);
        Self { srv, cli, tickets, answers: Answers::new(), topics: HashMap::new(), subs: HashMap::new(), next: 1, frames: Vec::new() }
    }

    fn subscribe(&mut self, kind: u32, key: &str) -> u32 {
        let sub = self.next;
        self.next += 1;
        self.subs.insert((kind, key.to_owned()), sub);
        self.cli.subscribe(sub, Topic { kind, key: key.into() }, 0);
        sub
    }

    fn publish(&mut self, outs: Vec<Out>) {
        for o in outs {
            let key = (o.topic.kind, o.topic.key.clone());
            let Some(spec) = spec(o.topic.kind) else { continue };
            let log = self.topics.entry(key.clone()).or_insert_with(|| TopicLog::new(spec, BORN));
            if let Ok(p) = log.publish(o.delta) {
                if let Some(sub) = self.subs.get(&key) {
                    self.srv.push(*sub, &p);
                }
            }
        }
    }

    fn pump(&mut self, now: u64) {
        for _ in 0..100 {
            let mut moved = false;
            if let Some(t) = self.cli.poll_transmit(64 * 1024, now) {
                self.srv.receive(&t.bytes, now, &mut self.tickets, &mut self.answers);
                moved = true;
            }
            while let Some(e) = self.srv.poll_event() {
                if let Event::Subscribe { sub, topic, cursor } | Event::Resume { sub, topic, cursor } = e {
                    let key = (topic.kind, topic.key.clone());
                    let Some(spec) = spec(topic.kind) else { continue };
                    let log = self.topics.entry(key).or_insert_with(|| TopicLog::new(spec, BORN));
                    self.srv.start(sub, spec, log.start(cursor.as_ref()));
                }
            }
            if let Some(t) = self.srv.poll_transmit(64 * 1024, now) {
                for f in &t.frames {
                    if let Lane::Sub(s) = f.lane {
                        self.frames.push((s, f.kind, f.len));
                    }
                }
                self.cli.receive(&t.bytes, now);
                moved = true;
            }
            while self.cli.poll_event().is_some() {}
            if !moved {
                return;
            }
        }
    }

    fn converged(&self) -> Result<(), String> {
        for ((kind, key), sub) in &self.subs {
            let server = self.topics.get(&(*kind, key.clone())).map(|t| t.state().digest());
            let client = self.cli.state(*sub).map(|s| s.digest());
            if server.is_some() && server != client {
                return Err(format!("topic {kind}/{key} differs"));
            }
        }
        Ok(())
    }
}

/// A replayed sequence of the hub's events: runs, legs whose steps move one at a time, runners
/// and their samples, the queue. The client ends with the server's topics, and each step's change
/// and each sample goes within its budget (docs/browser-protocol.md, "Byte budgets").
#[test]
fn bp_g2_s9_a_replay_of_hub_events_converges_within_budgets() -> Result<(), String> {
    let mut l = Loop::new();
    let mut feed = Feed::new();
    let repos = ["kiln", "hyper-raft", "vorpal"];
    let mut step_subs = Vec::new();
    for r in repos {
        l.subscribe(kind::RUNS, r);
        l.subscribe(kind::LEGS, r);
        step_subs.push(l.subscribe(kind::STEPS, r));
    }
    let runners_sub = l.subscribe(kind::RUNNERS, "");
    let samples_sub = l.subscribe(kind::SAMPLES, "");
    l.subscribe(kind::QUEUE, "");
    l.subscribe(kind::CHANGED, "");
    l.pump(0);
    let lookup = |_: &str| None;
    let names = ["build", "clippy", "test", "e2e", "docs", "pack"];
    for (i, r) in repos.iter().enumerate() {
        let run_id = format!("run-{i}");
        let out = feed.event(&ServerEvent::Run { run: Box::new(run(&run_id, r, crate::store::Status::Running)) }, &lookup);
        l.publish(out);
        for s in 0..names.len() {
            let steps: Vec<(&str, &str, Option<i64>)> = names.iter().enumerate().map(|(k, n)| (*n, if k < s { "succeeded" } else if k == s { "running" } else { "pending" }, (k < s).then_some(1_793_825_061_000 + k as i64))).collect();
            let Some(j) = job(&format!("job-{i}"), &run_id, &steps) else { return Err("a job".into()) };
            let out = feed.event(&ServerEvent::Job { job: Box::new(j) }, &lookup);
            l.publish(out);
            l.pump(1 + s as u64);
        }
        let out = feed.event(&ServerEvent::Run { run: Box::new(run(&run_id, r, crate::store::Status::Succeeded)) }, &lookup);
        l.publish(out);
    }
    for t in 0..20i64 {
        for m in ["fedora", "adas-mac-studio.local-2"] {
            let out = feed.event(&ServerEvent::Agent { agent: Box::new(agent(m, (t % 3) as u32, 20.0 + t as f32, 1_793_825_000_000 + t * 1000)) }, &lookup);
            l.publish(out);
        }
        let out = feed.event(&ServerEvent::Queue(crate::wire::events::Queue::Counts { queued: t % 4, running: 2 }), &lookup);
        l.publish(out);
        l.publish(feed.event(&ServerEvent::Terminals, &lookup));
        l.pump(100 + t as u64);
    }
    l.converged()?;
    // The budgets: a step's change after its first, and each sample after the first.
    let steps: Vec<usize> = l.frames.iter().filter(|(s, k, _)| step_subs.contains(s) && *k == Kind::Change).map(|x| x.2).collect();
    let runner: Vec<usize> = l.frames.iter().filter(|(s, k, _)| *s == runners_sub && *k == Kind::Change).map(|x| x.2).collect();
    let samples: Vec<usize> = l.frames.iter().filter(|(s, k, _)| *s == samples_sub && *k == Kind::Change).map(|x| x.2).collect();
    println!("step changes {steps:?}\nrunner changes {runner:?}\nsample changes {samples:?}");
    // Each step's first appearance is a whole Put; every change after is a Patch.
    let firsts = repos.len() * names.len();
    let over: Vec<usize> = steps.iter().copied().filter(|n| *n > 32).collect();
    if over.len() > firsts {
        return Err(format!("{} step changes past 32 B: {over:?}", over.len() - firsts));
    }
    if let Some(n) = samples.iter().skip(2).find(|n| **n > 48) {
        return Err(format!("a sample of {n} B, past 48 B"));
    }
    Ok(())
}

/// What a viewer of `/api/harness/sessions/:id/ws` holds after a sequence of frames, applied as
/// the page applies them.
#[derive(Debug, Default, PartialEq)]
struct WsView {
    info: Option<Value>,
    meta: Option<Value>,
    state: BTreeMap<String, Value>,
    changes: BTreeMap<String, Value>,
    files: Option<Value>,
    events: BTreeMap<u64, (Value, String)>,
}

impl WsView {
    fn apply(&mut self, f: &SessionFrame) {
        match f {
            SessionFrame::Open { info, .. } => {
                *self = WsView { info: Some(info.clone()), meta: self.meta.take(), files: self.files.take(), ..Default::default() };
            }
            SessionFrame::Resumed { info, .. } => self.info = Some(info.clone()),
            SessionFrame::Meta { meta } => self.meta = Some(meta.clone()),
            SessionFrame::State { state } => self.state = state.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default(),
            SessionFrame::StatePatch { set, gone } => {
                for (k, v) in set {
                    self.state.insert(k.clone(), v.clone());
                }
                for k in gone {
                    self.state.remove(k);
                }
            }
            SessionFrame::Changes { files } => self.changes = files.as_array().map(|a| a.iter().map(|v| (session::change_key(v), v.clone())).collect()).unwrap_or_default(),
            SessionFrame::ChangesPatch { set, gone } => {
                for v in set {
                    self.changes.insert(session::change_key(v), v.clone());
                }
                for v in gone {
                    self.changes.remove(&session::change_key(v));
                }
            }
            SessionFrame::Files { files } => self.files = serde_json::to_value(files).ok(),
            SessionFrame::Events { events } => {
                for e in events {
                    if let Some(seq) = e.get("seq").and_then(Value::as_u64) {
                        self.events.insert(seq, (e.clone(), String::new()));
                    }
                }
            }
            SessionFrame::Stream { id, text } => {
                if let Some((_, s)) = self.events.values_mut().find(|(e, _)| e.get("id").and_then(Value::as_str) == Some(id)) {
                    s.push_str(text);
                }
            }
            SessionFrame::Reset { from } => self.events.retain(|s, _| from.is_some_and(|f| *s < f)),
            _ => {}
        }
    }

    /// The same, read back from the protocol's topic state.
    fn of(state: &kiln_bp::state::TopicState) -> Self {
        let mut v = WsView::default();
        for (k, fields) in &state.items {
            let Ok(item) = SessionItem::from_fields(fields) else { continue };
            let j: Value = serde_json::from_slice(&item.json.0).unwrap_or(Value::Null);
            match k {
                ItemKey::Num(seq) => {
                    v.events.insert(*seq, (j, item.streamed));
                }
                ItemKey::Str(s) if s == "info" => v.info = Some(j),
                ItemKey::Str(s) if s == "meta" => v.meta = Some(j),
                ItemKey::Str(s) if s == "files" => v.files = Some(j),
                ItemKey::Str(s) => {
                    if let Some(f) = s.strip_prefix("state.") {
                        v.state.insert(f.to_owned(), j);
                    } else if s.starts_with("change.") {
                        v.changes.insert(s.clone(), j);
                    }
                }
            }
        }
        v
    }
}

/// A session viewed through the protocol converges to what its socket's viewer holds, for the
/// same sequence of updates.
#[test]
fn bp_g2_s9_a_session_converges_as_its_socket_would() -> Result<(), String> {
    let mut l = Loop::new();
    let sub = l.subscribe(kind::SESSION, "s1");
    l.pump(0);
    let mut feed = SessionFeed::new("s1");
    let mut ws = WsView::default();
    let ev = |seq: u64, id: &str, text: &str| json!({"id": id, "seq": seq, "at": 1_793_825_000_000u64 + seq, "kind": "assistant", "text": text});
    let mut frames = vec![
        SessionFrame::Open { info: json!({"id": "s1", "harness": "claude"}), born: None },
        SessionFrame::Meta { meta: json!({"model": "opus", "cwd": "/work"}) },
        SessionFrame::State { state: json!({"busy": false, "turn": 0, "mode": "default"}) },
        SessionFrame::Changes { files: json!([{"path": "a.rs", "tree": "/w"}, {"path": "b.rs"}]) },
        SessionFrame::Events { events: (1..=5).map(|i| ev(i, &format!("e{i}"), "hello")).collect() },
    ];
    for t in 0..30u64 {
        frames.push(SessionFrame::StatePatch { set: serde_json::Map::from_iter([("busy".to_owned(), json!(t % 2 == 0)), ("turn".to_owned(), json!(t))]), gone: if t == 7 { vec!["mode".into()] } else { vec![] } });
        let seq = 6 + t;
        frames.push(SessionFrame::Events { events: vec![ev(seq, &format!("e{seq}"), "")] });
        for w in ["The", " quick", " brown", " fox"] {
            frames.push(SessionFrame::Stream { id: format!("e{seq}"), text: w.into() });
        }
        if t % 5 == 4 {
            frames.push(SessionFrame::Events { events: vec![ev(seq, &format!("e{seq}"), "The quick brown fox")] });
            frames.push(SessionFrame::ChangesPatch { set: vec![json!({"path": format!("f{t}.rs")})], gone: vec![json!({"path": "b.rs"})] });
        }
        if t == 20 {
            frames.push(SessionFrame::Reset { from: Some(30) });
        }
    }
    frames.push(SessionFrame::Synced);
    for (i, f) in frames.iter().enumerate() {
        ws.apply(f);
        let (outs, ended) = feed.frame(f);
        l.publish(outs);
        if ended {
            break;
        }
        if i % 3 == 0 {
            l.pump(1 + i as u64);
        }
    }
    l.pump(10_000);
    l.converged()?;
    let got = l.cli.state(sub).map(WsView::of).unwrap_or_default();
    if got != ws {
        return Err(format!("the protocol's view differs from the socket's:\n{got:?}\n{ws:?}"));
    }
    // The tokens went as `Tail`s.
    let tails = l.frames.iter().filter(|(s, k, _)| *s == sub && *k == Kind::Tail).count();
    if tails == 0 {
        return Err("no token went as a Tail".into());
    }
    Ok(())
}

/// Access is decided at subscribe, from the topic alone: a narrowed person gets their
/// repositories' runs, legs, steps and events, their own inbox's changes and their sessions;
/// not the runners, another repository, or another's inbox.
#[test]
fn bp_g2_s9_access_is_decided_from_the_topic() {
    let c = Caller { owner: false, person: "bo".into(), repos: ["kiln".to_owned()].into(), sessions: ["s1".to_owned()].into() };
    let t = |kind, key: &str| Topic { kind, key: key.into() };
    assert!(allowed(&t(kind::RUNS, "kiln"), &c));
    assert!(allowed(&t(kind::STEPS, "kiln"), &c));
    assert!(allowed(&t(kind::EVENTS, "kiln"), &c));
    assert!(allowed(&t(kind::CHANGED, "person:bo"), &c));
    assert!(allowed(&t(kind::SESSION, "s1"), &c));
    assert!(!allowed(&t(kind::RUNS, "vorpal"), &c));
    assert!(!allowed(&t(kind::RUNNERS, ""), &c));
    assert!(!allowed(&t(kind::CHANGED, "person:ada"), &c));
    assert!(!allowed(&t(kind::SESSION, "s2"), &c));
    assert!(!allowed(&t(99, ""), &Caller { owner: true, ..Default::default() }));
    assert!(allowed(&t(kind::RUNNERS, ""), &Caller { owner: true, ..Default::default() }));
    for (k, _, _) in CATALOGUE {
        assert!(spec(*k).is_some());
    }
}

/// Logs and terminals go as appends by offset, cut so none is long.
#[test]
fn bp_g2_s9_logs_go_as_appends_by_offset() {
    let mut s = Streams::new();
    let a = s.append(kind::STEP_LOG, "job-1/2", &[b'x'; 2500], 1000);
    let b = s.append(kind::STEP_LOG, "job-1/2", b"done\n", 1000);
    let ats: Vec<u64> = a.iter().chain(b.iter()).filter_map(|o| if let kiln_bp::frames::Delta::Append { at, .. } = o.delta { Some(at) } else { None }).collect();
    assert_eq!(ats, vec![0, 1000, 2000, 2500]);
}
