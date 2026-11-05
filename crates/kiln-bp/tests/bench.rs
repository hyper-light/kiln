//! bp-g2 step 10: the server's CPU per operation, in release (`cargo test -p kiln-bp --release
//! --test bench -- --nocapture`), over the operations kiln's measurement recorded
//! (ui/tests/bp-measure.json, written by `KILN_WRITE_BP=1 cargo test -p kiln --lib bp::measure`):
//! each operation's changes published and fanned out to N subscribers, each subscriber's frames
//! made; against today's work for the same event, its JSON made once and read once per socket
//! (the events socket's `Narrow` parses each event per socket). Microseconds per operation at p50,
//! p99 and p99.9.

use std::collections::HashMap;
use std::time::Instant;

use kiln_bp::answers::Answers;
use kiln_bp::client::{Client, ClientConfig};
use kiln_bp::conn::{Class, Prio, TopicSpec, Trust};
use kiln_bp::frames::{Delta, Topic};
use kiln_bp::server::{Event, Server, ServerConfig};
use kiln_bp::tickets::{Principal, Tickets};
use kiln_bp::topic::TopicLog;
use kiln_bp::wire::{Bits, from_bytes, unhex};

/// kiln::bp::spec's classes for the kinds the recording has.
fn spec(kind: u32) -> TopicSpec {
    let s = |class, prio, trust| TopicSpec { kind, class, prio, trust };
    match kind {
        1..=4 => s(Class::State, Prio::State, Trust::Text),
        5 | 6 => s(Class::Telemetry, Prio::State, Trust::Records),
        7 => s(Class::State, Prio::State, Trust::Records),
        9 | 11 => s(Class::Stream, Prio::Interactive, Trust::Text),
        _ => s(Class::Stream, Prio::Stream, Trust::Text),
    }
}

struct Op {
    json: String,
    outs: Vec<(u32, String, Delta)>,
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(((v.len().saturating_sub(1)) as f64 * q).round() as usize).copied().unwrap_or(0.0)
}

/// A server connection, opened and subscribed to `topics`, ready to be pushed to.
fn connection(id: u64, topics: &[(u32, String)]) -> (Server, HashMap<(u32, String), u32>) {
    let mut tickets = Tickets::new();
    let t = tickets.mint([3; 32], Principal { who: "ada".into(), ..Default::default() }, 0);
    let mut answers = Answers::new();
    let mut srv = Server::new(ServerConfig { compress: false, ..Default::default() }, id, 1, 0);
    let mut cli = Client::new(ClientConfig { caps: Bits(0), window: 1 << 40, ..Default::default() }, &t, 0);
    let mut subs = HashMap::new();
    for (i, (k, key)) in topics.iter().enumerate() {
        let sub = i as u32 + 1;
        cli.subscribe(sub, Topic { kind: *k, key: key.clone() }, 0);
        subs.insert((*k, key.clone()), sub);
    }
    for now in 0..4 {
        if let Some(tx) = cli.poll_transmit(1 << 20, now) {
            srv.receive(&tx.bytes, now, &mut tickets, &mut answers);
        }
        while let Some(e) = srv.poll_event() {
            if let Event::Subscribe { sub, topic, .. } = e {
                srv.start(sub, spec(topic.kind), kiln_bp::topic::Start::Snapshot { cursor: kiln_bp::frames::Cursor { born: 1, rev: 0 }, items: Vec::new() });
            }
        }
        while let Some(tx) = srv.poll_transmit(1 << 20, now) {
            cli.receive(&tx.bytes, now);
        }
    }
    (srv, subs)
}

#[test]
fn bp_g2_s10_server_cpu_per_operation() -> Result<(), String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/tests/bp-measure.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("{} isn't there: run KILN_WRITE_BP=1 cargo test -p kiln --lib bp::measure first", path.display());
        return Ok(());
    };
    let doc: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut seqs: Vec<(String, Vec<Op>)> = Vec::new();
    for f in doc.get("frames").and_then(|f| f.as_array()).into_iter().flatten() {
        let seq = f.get("seq").and_then(|s| s.as_str()).unwrap_or("").to_owned();
        let json = f.get("json").and_then(|s| s.as_str()).unwrap_or("").to_owned();
        let mut outs = Vec::new();
        for o in f.get("outs").and_then(|o| o.as_array()).into_iter().flatten() {
            let kind = o.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u32;
            let key = o.get("key").and_then(|k| k.as_str()).unwrap_or("").to_owned();
            let d = unhex(o.get("delta").and_then(|d| d.as_str()).unwrap_or("")).unwrap_or_default();
            outs.push((kind, key, from_bytes::<Delta>(&d).map_err(|e| e.to_string())?));
        }
        match seqs.last_mut() {
            Some((s, ops)) if *s == seq => ops.push(Op { json, outs }),
            _ => seqs.push((seq, vec![Op { json, outs }])),
        }
    }
    println!("| Sequence | Subscribers | protocol µs/op p50 / p99 / p99.9 | today µs/op p50 / p99 / p99.9 |\n|---|---|---|---|");
    for (name, ops) in &seqs {
        let mut topics: Vec<(u32, String)> = ops.iter().flat_map(|o| o.outs.iter().map(|(k, key, _)| (*k, key.clone()))).collect();
        topics.sort();
        topics.dedup();
        for n in [1usize, 10, 100] {
            let mut conns: Vec<(Server, HashMap<(u32, String), u32>)> = (0..n).map(|i| connection(i as u64, &topics)).collect();
            let mut logs: HashMap<(u32, String), TopicLog> = HashMap::new();
            let mut bp = Vec::new();
            let mut today = Vec::new();
            for (i, op) in ops.iter().enumerate() {
                let t0 = Instant::now();
                for (k, key, d) in &op.outs {
                    let log = logs.entry((*k, key.clone())).or_insert_with(|| TopicLog::new(spec(*k), 1));
                    let Ok(p) = log.publish(d.clone()) else { continue };
                    for (srv, subs) in &mut conns {
                        if let Some(sub) = subs.get(&(*k, key.clone())) {
                            srv.push(*sub, &p);
                        }
                    }
                }
                for (srv, _) in &mut conns {
                    while let Some(tx) = srv.poll_transmit(1 << 20, 10 + i as u64) {
                        std::hint::black_box(&tx);
                    }
                }
                bp.push(t0.elapsed().as_secs_f64() * 1e6);
                let v: Result<serde_json::Value, _> = serde_json::from_str(&op.json);
                let t1 = Instant::now();
                let made = v.as_ref().map(|v| v.to_string()).unwrap_or_else(|_| op.json.clone());
                for _ in 0..n {
                    let read: Result<serde_json::Value, _> = serde_json::from_str(&made);
                    std::hint::black_box(&read);
                }
                today.push(t1.elapsed().as_secs_f64() * 1e6);
            }
            println!(
                "| {name} | {n} | {:.2} / {:.2} / {:.2} | {:.2} / {:.2} / {:.2} |",
                pct(&mut bp, 0.5),
                pct(&mut bp, 0.99),
                pct(&mut bp, 0.999),
                pct(&mut today, 0.5),
                pct(&mut today, 0.99),
                pct(&mut today, 0.999)
            );
        }
    }
    Ok(())
}
