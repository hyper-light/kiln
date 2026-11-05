//! bp-g2 step 10: the codec and core measured offline, with no carrier, on sequences built from
//! kiln's real types (goal 1 recorded operations' sizes, not event traces): a leg's life, a busy
//! runners page, an agent session streaming, a log flood, and a reconnect. Each goes through the
//! protocol (compressed and not) and as today's endpoints send it (raw JSON, each message gzip'd
//! alone, and one kept deflate context, as permessage-deflate with context takeover would).
//! Reports bytes per operation at p50, p99 and p99.9; writes `ui/tests/bp-measure.json` (KILN_WRITE_BP=1)
//! for the browsers' decode timings (ui/e2e/bp-decode.spec.ts) and the server's CPU per operation
//! (crates/kiln-bp/tests/bench.rs, in release). Bytes are above the carrier
//! (no WebSocket, TLS or TCP bytes on either side) and deterministic: the same on every run.
//!
//! `cargo test -p kiln --release --lib bp::measure -- --nocapture`

use std::io::Write as _;

use flate2::{Compress, Compression, FlushCompress};
use kiln_bp::answers::Answers;
use kiln_bp::client::{Client, ClientConfig};
use kiln_bp::frames::Topic;
use kiln_bp::server::{Event, Server, ServerConfig};
use kiln_bp::tickets::{Principal, Tickets};
use kiln_bp::topic::TopicLog;
use kiln_bp::wire::Bits;
use serde_json::json;

use super::session::SessionFeed;
use super::tests_support::*;
use super::*;
use crate::wire::events::ServerEvent;
use crate::wire::ui::{SessionDown, SessionFrame};

const BORN: u64 = 1_793_825_000_000;

/// One operation: what today's endpoint sends for it, and the protocol's changes.
struct Op {
    today: Vec<u8>,
    outs: Vec<Out>,
}

/// A kept raw-deflate context, sync-flushed per message.
struct Kept(Compress);

impl Kept {
    fn new() -> Self {
        Self(Compress::new(Compression::new(6), false))
    }
    fn size(&mut self, b: &[u8]) -> usize {
        let mut out = Vec::with_capacity(b.len() + 64);
        let mut at = 0usize;
        loop {
            let before = self.0.total_in();
            if out.capacity() == out.len() {
                out.reserve(256);
            }
            if self.0.compress_vec(b.get(at..).unwrap_or(&[]), &mut out, FlushCompress::Sync).is_err() {
                return out.len();
            }
            at += (self.0.total_in() - before) as usize;
            if at >= b.len() && out.len() < out.capacity() {
                return out.len();
            }
        }
    }
}

fn gzip_alone(b: &[u8]) -> usize {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), Compression::new(6));
    let _ = e.write_all(b);
    e.finish().map(|v| v.len()).unwrap_or(0)
}

/// A connection over no carrier, for measuring: the bytes each operation's frames took.
struct Meter {
    srv: Server,
    cli: Client,
    tickets: Tickets,
    answers: Answers,
    topics: std::collections::HashMap<(u32, String), TopicLog>,
    subs: std::collections::HashMap<(u32, String), u32>,
    now: u64,
}

impl Meter {
    fn new(compress: bool, topics: &[(u32, &str)]) -> Self {
        let mut tickets = Tickets::new();
        let t = tickets.mint([1; 32], Principal { who: "ada".into(), ..Default::default() }, 0);
        let srv = Server::new(ServerConfig { compress, ..Default::default() }, 1, BORN, 0);
        let cli = Client::new(ClientConfig { caps: Bits(if compress { kiln_bp::conn::CAP_DEFLATE_RAW } else { 0 }), ..Default::default() }, &t, 0);
        let mut m = Self { srv, cli, tickets, answers: Answers::new(), topics: Default::default(), subs: Default::default(), now: 0 };
        for (i, (k, key)) in topics.iter().enumerate() {
            let sub = i as u32 + 1;
            m.subs.insert((*k, (*key).to_owned()), sub);
            m.cli.subscribe(sub, Topic { kind: *k, key: (*key).into() }, 0);
        }
        m.flush();
        m
    }

    fn publish(&mut self, outs: &[Out]) {
        for o in outs {
            let key = (o.topic.kind, o.topic.key.clone());
            let Some(spec) = spec(o.topic.kind) else { continue };
            let log = self.topics.entry(key.clone()).or_insert_with(|| TopicLog::new(spec, BORN));
            if let Ok(p) = log.publish(o.delta.clone()) {
                if let Some(sub) = self.subs.get(&key) {
                    self.srv.push(*sub, &p);
                }
            }
        }
    }

    /// Moves everything due; the bytes the server sent.
    fn flush(&mut self) -> usize {
        let mut sent = 0;
        for _ in 0..1000 {
            self.now += 1;
            let mut moved = false;
            if let Some(t) = self.cli.poll_transmit(64 * 1024, self.now) {
                self.srv.receive(&t.bytes, self.now, &mut self.tickets, &mut self.answers);
                moved = true;
            }
            while let Some(e) = self.srv.poll_event() {
                if let Event::Subscribe { sub, topic, cursor } | Event::Resume { sub, topic, cursor } = e {
                    let Some(spec) = spec(topic.kind) else { continue };
                    let log = self.topics.entry((topic.kind, topic.key.clone())).or_insert_with(|| TopicLog::new(spec, BORN));
                    self.srv.start(sub, spec, log.start(cursor.as_ref()));
                }
            }
            if let Some(t) = self.srv.poll_transmit(64 * 1024, self.now) {
                sent += t.bytes.len();
                self.cli.receive(&t.bytes, self.now);
                moved = true;
            }
            while self.cli.poll_event().is_some() {}
            if !moved {
                break;
            }
        }
        sent
    }
}

fn pct(v: &mut [usize], q: f64) -> usize {
    v.sort_unstable();
    v.get(((v.len().saturating_sub(1)) as f64 * q).round() as usize).copied().unwrap_or(0)
}

/// The sequences, each a list of operations and the topics a page holds for it.
fn sequences() -> Vec<(&'static str, Vec<(u32, String)>, Vec<Op>)> {
    let mut out = Vec::new();
    let lookup = |_: &str| None;
    // A leg's life: its run, six steps each starting and finishing, its run finishing.
    {
        let mut feed = Feed::new();
        let mut ops = Vec::new();
        let names = ["checkout", "build", "clippy", "test", "e2e", "pack"];
        let r0 = run("run-1", "kiln", crate::store::Status::Running);
        ops.push(Op { today: ServerEvent::Run { run: Box::new(r0.clone()) }.text().into_bytes(), outs: feed.event(&ServerEvent::Run { run: Box::new(r0) }, &lookup) });
        for s in 0..=names.len() * 2 {
            let steps: Vec<(&str, &str, Option<i64>)> = names.iter().enumerate().map(|(k, n)| (*n, if 2 * k + 1 < s { "succeeded" } else if 2 * k < s { "running" } else { "pending" }, (2 * k + 1 < s).then_some(1_793_825_061_000 + k as i64))).collect();
            let Some(j) = job("job-1", "run-1", &steps) else { continue };
            let ev = ServerEvent::Job { job: Box::new(j) };
            ops.push(Op { today: ev.text().into_bytes(), outs: feed.event(&ev, &lookup) });
        }
        let ev = ServerEvent::Run { run: Box::new(run("run-1", "kiln", crate::store::Status::Succeeded)) };
        ops.push(Op { today: ev.text().into_bytes(), outs: feed.event(&ev, &lookup) });
        out.push(("a leg's life", vec![(kind::RUNS, "kiln".into()), (kind::LEGS, "kiln".into()), (kind::STEPS, "kiln".into())], ops));
    }
    // A busy runners page: 20 runners, each saying how it is every second for a minute.
    {
        let mut feed = Feed::new();
        let mut ops = Vec::new();
        for t in 0..60i64 {
            for m in 0..20u32 {
                let a = agent(&format!("runner-{m:02}.attlocal.net"), (t as u32 + m) % 4, 10.0 + ((t * 7 + i64::from(m) * 13) % 80) as f32, 1_793_825_000_000 + t * 1000);
                let ev = ServerEvent::Agent { agent: Box::new(a) };
                ops.push(Op { today: ev.text().into_bytes(), outs: feed.event(&ev, &lookup) });
            }
        }
        out.push(("a busy runners page", vec![(kind::RUNNERS, String::new()), (kind::SAMPLES, String::new())], ops));
    }
    // An agent session streaming a turn: its state, an event, 300 tokens, the event whole.
    {
        let mut feed = SessionFeed::new("s1");
        let mut ops = Vec::new();
        let mut rev = 100u64;
        let mut frame = |f: SessionFrame, ops: &mut Vec<Op>, feed: &mut SessionFeed| {
            rev += 1;
            let today = SessionDown { frame: f.clone(), rev: Some(rev) }.text().into_bytes();
            ops.push(Op { today, outs: feed.frame(&f).0 });
        };
        frame(SessionFrame::StatePatch { set: serde_json::Map::from_iter([("busy".to_owned(), json!(true))]), gone: vec![] }, &mut ops, &mut feed);
        frame(SessionFrame::Events { events: vec![json!({"id": "msg_01HXYZ", "seq": 1207, "at": 1_793_825_000_000u64, "kind": "assistant", "text": ""})] }, &mut ops, &mut feed);
        let words = ["The", " protocol", " sends", " only", " what", " changed", ",", " and", " joins", " tokens"];
        for i in 0..300 {
            frame(SessionFrame::Stream { id: "msg_01HXYZ".into(), text: words.get(i % words.len()).copied().unwrap_or(" x").into() }, &mut ops, &mut feed);
        }
        frame(SessionFrame::StatePatch { set: serde_json::Map::from_iter([("busy".to_owned(), json!(false)), ("tokens".to_owned(), json!(1834))]), gone: vec![] }, &mut ops, &mut feed);
        out.push(("an agent session streaming", vec![(kind::SESSION, "s1".into())], ops));
    }
    // A log flood: 2,000 lines of a build's log, as the log socket's binary frames carry them.
    {
        let mut s = Streams::new();
        let mut ops = Vec::new();
        for i in 0..2000 {
            let line = format!("   Compiling crate-{i:04} v0.{}.{} (registry+https://github.com/rust-lang/crates.io-index)\n", i % 7, i % 13);
            ops.push(Op { today: line.clone().into_bytes(), outs: s.append(kind::STEP_LOG, "job-1/3", line.as_bytes(), 1024) });
        }
        out.push(("a log flood", vec![(kind::STEP_LOG, "job-1/3".into())], ops));
    }
    out
}

/// What a page fetches again today after a reconnect (its events socket says nothing of what it
/// missed: every key loads again), from goal 1's recorded operations: the bodies of
/// GET /api/agents (2,016 B), /api/harness/sessions (2,911 B) and /api/harness/kinds (4,279 B).
const REFETCH_TODAY: usize = 2016 + 2911 + 4279;

#[test]
fn bp_g2_s10_measure() {
    let mut report = String::new();
    let mut decode: Vec<serde_json::Value> = Vec::new();
    let mut totals: Vec<(String, usize)> = Vec::new();
    for (name, topics, ops) in sequences() {
        let refs: Vec<(u32, &str)> = topics.iter().map(|(k, s)| (*k, s.as_str())).collect();
        // Each operation sent as it happens (a quiet link): its own bytes.
        let mut per_op: Vec<Vec<usize>> = vec![Vec::new(); 5];
        for compress in [false, true] {
            let mut m = Meter::new(compress, &refs);
            for op in &ops {
                m.publish(&op.outs);
                let b = m.flush();
                if let Some(v) = per_op.get_mut(usize::from(compress)) {
                    v.push(b);
                }
            }
        }
        let mut kept = Kept::new();
        for op in &ops {
            if let Some(v) = per_op.get_mut(2) {
                v.push(op.today.len());
            }
            if let Some(v) = per_op.get_mut(3) {
                v.push(gzip_alone(&op.today));
            }
            let k = kept.size(&op.today);
            if let Some(v) = per_op.get_mut(4) {
                v.push(k);
            }
        }
        // All of them while the link is busy: what waits is merged and packed.
        let mut busy = Meter::new(true, &refs);
        for op in &ops {
            busy.publish(&op.outs);
        }
        let busy_total = busy.flush();
        let labels = ["protocol", "protocol, compressed", "today, JSON", "today, gzip each", "today, kept deflate"];
        report.push_str(&format!("\n### {name} ({} operations)\n\n| Form | p50 | p99 | p99.9 | total |\n|---|---|---|---|---|\n", ops.len()));
        for (l, v) in labels.iter().zip(per_op.iter_mut()) {
            let total: usize = v.iter().sum();
            report.push_str(&format!("| {l} | {} | {} | {} | {total} |\n", pct(v, 0.5), pct(v, 0.99), pct(v, 0.999)));
            totals.push((format!("{name}/{l}"), total));
        }
        report.push_str(&format!("| protocol, compressed, on a busy link (merged and packed) | | | | {busy_total} |\n"));
        totals.push((format!("{name}/busy"), busy_total));
        // A sample of frames for the browsers' decode timing.
        let mut m = Meter::new(false, &refs);
        for op in ops.iter().take(200) {
            m.publish(&op.outs);
            let mut bytes = Vec::new();
            m.now += 1;
            while let Some(t) = m.srv.poll_transmit(64 * 1024, m.now) {
                bytes.extend(t.bytes);
            }
            let mut h = String::new();
            kiln_bp::wire::hex(&bytes, &mut h);
            let outs: Vec<serde_json::Value> = op
                .outs
                .iter()
                .map(|o| {
                    let mut d = String::new();
                    kiln_bp::wire::hex(&kiln_bp::wire::to_bytes(&o.delta), &mut d);
                    json!({"kind": o.topic.kind, "key": o.topic.key, "delta": d})
                })
                .collect();
            decode.push(json!({"seq": name, "bp": h, "json": String::from_utf8_lossy(&op.today), "outs": outs}));
        }
    }
    // A reconnect after 30 s away with a busy runners page's minute of changes missed.
    {
        let seqs = sequences();
        let (_, topics, ops) = seqs.into_iter().nth(1).unwrap_or(("", Vec::new(), Vec::new()));
        let refs: Vec<(u32, &str)> = topics.iter().map(|(k, s)| (*k, s.as_str())).collect();
        let mut m = Meter::new(true, &refs);
        let half = ops.len() / 2;
        for op in ops.iter().take(half) {
            m.publish(&op.outs);
        }
        m.flush();
        // Away: the server's topics move on; the page comes back with its cursors.
        let mut tickets = Tickets::new();
        let t = tickets.mint([2; 32], Principal { who: "ada".into(), ..Default::default() }, 0);
        m.tickets = tickets;
        m.srv = Server::new(ServerConfig::default(), 2, BORN, m.now);
        m.cli = m.cli.reconnect(ClientConfig::default(), &t, m.now);
        for op in ops.iter().skip(half).take(40) {
            for o in &op.outs {
                let Some(spec) = spec(o.topic.kind) else { continue };
                let log = m.topics.entry((o.topic.kind, o.topic.key.clone())).or_insert_with(|| TopicLog::new(spec, BORN));
                let _ = log.publish(o.delta.clone());
            }
        }
        let resumed = m.flush();
        report.push_str(&format!("\n### a reconnect (40 changes missed)\n\n| Form | bytes |\n|---|---|\n| protocol (resume from the cursors) | {resumed} |\n| today (every key fetched again) | {REFETCH_TODAY} |\n"));
        totals.push(("reconnect".into(), resumed));
    }
    println!("{report}");
    // Bytes are deterministic: a second run gives the same counts.
    let again: Vec<usize> = sequences().iter().map(|(_, topics, ops)| {
        let refs: Vec<(u32, &str)> = topics.iter().map(|(k, s)| (*k, s.as_str())).collect();
        let mut m = Meter::new(false, &refs);
        ops.iter().map(|op| { m.publish(&op.outs); m.flush() }).sum()
    }).collect();
    let first: Vec<usize> = totals.iter().filter(|(k, _)| k.ends_with("/protocol")).map(|x| x.1).collect();
    assert_eq!(first, again, "byte counts differ between runs");
    if std::env::var("KILN_WRITE_BP").is_ok() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/bp-measure.json");
        let _ = std::fs::write(&path, serde_json::to_string(&json!({"about": "frames for the browsers' decode timing (crates/kiln/src/bp/measure.rs)", "frames": decode})).unwrap_or_default());
    }
}
