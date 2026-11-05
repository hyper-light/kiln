//! bp-g2 step 6 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): subscriptions with resume cursors and
//! typed deltas. A resume after a reconnect costs only the missed changes and a few bytes, and no
//! snapshot while the log holds them; common changes go within the step 1 budgets; and over
//! seeds where records change while links drop, every client converges to the server's state,
//! those past the log's reach with exactly one snapshot.

mod common;

use common::*;
use kiln_bp::client::{ClientConfig, ClientEvent};
use kiln_bp::conn::Lane;
use kiln_bp::frames::*;
use kiln_bp::rng::Rng;
use kiln_bp::server::ServerConfig;
use kiln_bp::wire::{Bits, Bytes, to_bytes};

fn plain() -> (ServerConfig, ClientConfig) {
    (ServerConfig { compress: false, ..Default::default() }, ClientConfig { caps: Bits(0), ..Default::default() })
}

fn snapshots(evs: &[ClientEvent]) -> usize {
    evs.iter().filter(|e| matches!(e, ClientEvent::Snapshot { .. })).count()
}

/// AC: resuming after a reconnect with K changes missed (the log covers them) costs at most 64
/// bytes beyond the K deltas, and sends no snapshot.
#[test]
fn bp_g2_s6_a_resume_costs_the_missed_changes_and_no_snapshot() {
    let mut host = Host::new();
    let t1 = host.ticket("ada", 1, 0);
    let (s, c) = plain();
    let mut a = Pair::open(&mut host, 1, s.clone(), c.clone(), &t1, 0);
    a.run(&mut host, 0, 2);
    a.client.subscribe(7, Topic { kind: 1, key: "runners".into() }, 0);
    a.run(&mut host, 3, 5);
    for i in 0..20u8 {
        host.publish(1, "runners", Delta::Put { key: Key::Str { s: format!("m{i}") }, fields: vec![vec![i].into(), vec![1, 2, 3].into()] }, &mut [&mut a.server]);
    }
    a.run(&mut host, 6, 8);
    // The link drops; K changes happen meanwhile.
    for k in [1usize, 5, 40] {
        host.subs.clear();
        let mut want = Vec::new();
        for i in 0..k {
            let p = Delta::Patch { key: Key::Str { s: format!("m{}", i % 20) }, mask: Bits(1), fields: vec![vec![(i % 200) as u8].into()] };
            host.publish(1, "runners", p.clone(), &mut []);
            want.push(p);
        }
        let t = host.ticket("ada", 10 + k as u8, 9);
        let mut b = Pair::open(&mut host, 2 + k as u64, s.clone(), c.clone(), &t, 9);
        b.client = a.client.reconnect(c.clone(), &t, 9);
        let mut lane_bytes = 0usize;
        let mut delta_bytes = 0usize;
        for now in 9..30 {
            if let Some(t) = b.client.poll_transmit(64 * 1024, now) {
                b.server.receive(&t.bytes, now, &mut host.tickets, &mut host.answers);
            }
            host.serve(&mut b.server, now);
            if let Some(t) = b.server.poll_transmit(64 * 1024, now) {
                for f in &t.frames {
                    if f.lane == Lane::Sub(7) {
                        lane_bytes += f.len;
                        if f.kind == Kind::Change {
                            delta_bytes += f.len;
                        }
                    }
                }
                b.client.receive(&t.bytes, now);
            }
        }
        while let Some(e) = b.client.poll_event() {
            b.client_events.push(e);
        }
        let evs = b.take_events();
        assert_eq!(snapshots(&evs), 0, "K={k}: no snapshot while the log holds the changes");
        let changes = evs.iter().filter(|e| matches!(e, ClientEvent::Change { sub: 7, .. })).count();
        assert_eq!(changes, k.min(20).max(if k > 20 { 20 } else { k }), "K={k}: each key's newest, merged while they waited");
        assert!(lane_bytes - delta_bytes <= 64, "K={k}: {} bytes beyond the deltas", lane_bytes - delta_bytes);
        assert_eq!(b.client.state(7).map(|s| s.digest()), Some(host.topic(1, "runners").state().digest()));
        a.client = b.client.reconnect(c.clone(), &t, 30);
        a.server = b.server;
        let _ = want;
    }
}

/// AC: a changed step of a leg, a runner's sample and a streamed token each go as a delta or an
/// append within the step 1 budget (32 B, 32 B, 4 B and its text).
#[test]
fn bp_g2_s6_common_changes_go_within_their_budgets() {
    let mut host = Host::new();
    let t = host.ticket("ada", 1, 0);
    let (s, c) = plain();
    let mut p = Pair::open(&mut host, 1, s, c, &t, 0);
    p.run(&mut host, 0, 2);
    p.client.subscribe(1, Topic { kind: 1, key: "steps".into() }, 0);
    p.client.subscribe(2, Topic { kind: 2, key: "samples".into() }, 0);
    p.client.subscribe(3, Topic { kind: 10, key: "session/s1".into() }, 0);
    p.run(&mut host, 3, 5);
    // A step of a leg: its status, when it finished, its exit code (the record's fields 2, 4, 5).
    let step_key = "01a1a855ef4874fa81e38988c2f4a81e.3";
    let full = |status: u8| vec![to_bytes(&3u32).into(), to_bytes(&"cargo test".to_owned()).into(), vec![status].into(), to_bytes(&Some(1_793_825_000_123i64)).into(), to_bytes(&None::<i64>).into(), to_bytes(&None::<i64>).into()];
    host.publish(1, "steps", Delta::Put { key: Key::Str { s: step_key.into() }, fields: full(2) }, &mut [&mut p.server]);
    host.publish(2, "samples", Delta::Put { key: Key::Str { s: "adas-mac-studio.local-2".into() }, fields: vec![vec![0; 3].into(); 8] }, &mut [&mut p.server]);
    host.publish(10, "session/s1", Delta::Put { key: Key::Num { n: 1207 }, fields: vec![to_bytes(&"assistant".to_owned()).into(), Bytes::default()] }, &mut [&mut p.server]);
    host.publish(10, "session/s1", Delta::Extend { key: Key::Num { n: 1207 }, field: 1, at: 0, data: "The".into() }, &mut [&mut p.server]);
    p.run(&mut host, 6, 8);
    let mut sizes: Vec<(u32, Kind, usize)> = Vec::new();
    let mut measure = |p: &mut Pair, host: &mut Host, now: u64| {
        if let Some(t) = p.server.poll_transmit(64 * 1024, now) {
            for f in &t.frames {
                if let Lane::Sub(s) = f.lane {
                    sizes.push((s, f.kind, f.len));
                }
            }
            p.client.receive(&t.bytes, now);
        }
        host.serve(&mut p.server, now);
    };
    host.publish(1, "steps", Delta::Patch { key: Key::Str { s: step_key.into() }, mask: Bits(0b11100), fields: vec![vec![3].into(), to_bytes(&Some(1_793_825_061_456i64)).into(), to_bytes(&Some(0i64)).into()] }, &mut [&mut p.server]);
    measure(&mut p, &mut host, 9);
    // A sample: CPU per mille, load in hundredths, memory used, the time (fields 0, 1, 3, 6).
    host.publish(2, "samples", Delta::Patch { key: Key::Str { s: "adas-mac-studio.local-2".into() }, mask: Bits(0b1001011), fields: vec![to_bytes(&731u32).into(), to_bytes(&235u32).into(), to_bytes(&48_000_000_000u64).into(), to_bytes(&1_793_825_061_456u64).into()] }, &mut [&mut p.server]);
    measure(&mut p, &mut host, 10);
    host.publish(10, "session/s1", Delta::Extend { key: Key::Num { n: 1207 }, field: 1, at: 3, data: " quick".into() }, &mut [&mut p.server]);
    measure(&mut p, &mut host, 11);
    let of = |sub: u32| sizes.iter().filter(|(s, _, _)| *s == sub).map(|x| (x.1, x.2)).collect::<Vec<_>>();
    let step = of(1);
    let sample = of(2);
    let token = of(3);
    println!("leg step change {step:?}; host sample {sample:?}; streamed token {token:?}");
    assert!(matches!(step.as_slice(), [(Kind::Change, n)] if *n <= 32), "{step:?}");
    assert!(matches!(sample.as_slice(), [(Kind::Change, n)] if *n <= 32), "{sample:?}");
    assert!(matches!(token.as_slice(), [(Kind::Tail, n)] if *n <= 4 + " quick".len()), "{token:?}");
    assert_eq!(p.client.state(3).map(|s| s.digest()), Some(host.topic(10, "session/s1").state().digest()));
}

/// Seeds: records change while links drop and come back; clients resubscribe from their cursors,
/// some past the log's reach. Every client converges to the server's state; those past the
/// log's reach get exactly one snapshot.
#[test]
fn bp_g2_s6_clients_converge_across_drops_seed_by_seed() {
    let seeds: u64 = std::env::var("KILN_BP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    let mut past_reach = 0;
    for seed in 0..seeds {
        let mut g = Rng::new(0xC0DE ^ seed);
        let mut host = Host::new();
        let (s, c) = (ServerConfig::default(), ClientConfig::default());
        host.topics.insert((1, "items".into()), kiln_bp::topic::TopicLog::new(spec(1), BORN).with_log(16, 1 << 20));
        host.topics.insert((3, "log".into()), kiln_bp::topic::TopicLog::new(spec(3), BORN).with_log(16, 1 << 20));
        let mut pairs: Vec<Pair> = Vec::new();
        let mut next_id = 1u64;
        for n in 0..3u8 {
            let t = host.ticket("ada", n, 0);
            let mut p = Pair::open(&mut host, next_id, s.clone(), c.clone(), &t, 0);
            next_id += 1;
            p.client.subscribe(1, Topic { kind: 1, key: "items".into() }, 0);
            p.client.subscribe(2, Topic { kind: 3, key: "log".into() }, 0);
            pairs.push(p);
        }
        let mut log_at = 0u64;
        let mut down: Vec<(Option<u64>, usize)> = vec![(None, 1); pairs.len()];
        for now in 0..400u64 {
            // A change or two.
            for _ in 0..g.below(3) {
                let k = format!("k{}", g.below(12));
                let have = host.topic(1, "items").state().items.contains_key(&kiln_bp::state::ItemKey::Str(k.clone()));
                let d = match (g.below(4), have) {
                    (0, true) => Delta::Remove { key: Key::Str { s: k } },
                    (1, true) => Delta::Patch { key: Key::Str { s: k }, mask: Bits(1 << g.below(3)), fields: vec![vec![g.byte()].into()] },
                    _ => Delta::Put { key: Key::Str { s: k }, fields: vec![vec![g.byte()].into(), vec![g.byte(); 2].into(), Bytes::default()] },
                };
                let mut conns: Vec<&mut kiln_bp::server::Server> = pairs.iter_mut().map(|p| &mut p.server).collect();
                host.publish(1, "items", d, &mut conns);
                let line = format!("line {now}\n");
                let n = line.len() as u64;
                host.publish(3, "log", Delta::Append { at: log_at, data: line.as_str().into() }, &mut conns);
                log_at += n;
            }
            for (p, d) in pairs.iter_mut().zip(down.iter_mut()) {
                // Links drop now and then, for a while; a long drop goes past the log's reach.
                match d.0 {
                    None if g.below(60) == 0 => {
                        let long = g.coin();
                        d.0 = Some(now + g.below(if long { 5 } else { 60 }));
                        p.up.cut = true;
                        p.down.cut = true;
                    }
                    Some(until) if now >= until => {
                        d.0 = None;
                        d.1 += 1;
                        let t = host.ticket("ada", (now % 250) as u8, now);
                        for l in host.subs.values_mut() {
                            l.retain(|(conn, _)| *conn != p.server.id());
                        }
                        let fresh = kiln_bp::server::Server::new(s.clone(), next_id, BORN, now);
                        next_id += 1;
                        p.server = fresh;
                        p.client = p.client.reconnect(c.clone(), &t, now);
                        p.up = Link::new(0.0, 0);
                        p.down = Link::new(0.0, 0);
                    }
                    _ => {}
                }
                p.step(&mut host, now);
            }
        }
        for (i, (p, d)) in pairs.iter_mut().zip(down.iter_mut()).enumerate() {
            if d.0.is_some() {
                d.1 += 1;
                let t = host.ticket("ada", 251, 400);
                p.server = kiln_bp::server::Server::new(s.clone(), next_id, BORN, 400);
                next_id += 1;
                p.client = p.client.reconnect(c.clone(), &t, 400);
                p.up = Link::new(0.0, 0);
                p.down = Link::new(0.0, 0);
            }
            p.run(&mut host, 400, 420);
            let evs = p.take_events();
            assert_eq!(p.client.state(1).map(|s| s.digest()), Some(host.topic(1, "items").state().digest()), "seed {seed}, client {i}: items");
            assert_eq!(p.client.state(2).map(|s| s.digest()), Some(host.topic(3, "log").state().digest()), "seed {seed}, client {i}: log");
            // A snapshot comes only to start, or once when the log no longer holds what was missed.
            let snaps = evs.iter().filter(|e| matches!(e, ClientEvent::Snapshot { sub: 1, .. })).count();
            assert!(snaps <= d.1, "seed {seed}, client {i}: {snaps} snapshots over {} connects", d.1);
            if snaps > 1 {
                past_reach += 1;
            }
        }
    }
    assert!(past_reach > 0, "no seed went past the log's reach");
}

/// A client whose cursor the log no longer covers gets exactly one snapshot, then only changes.
#[test]
fn bp_g2_s6_past_the_logs_reach_exactly_one_snapshot() {
    let mut host = Host::new();
    host.topics.insert((1, "items".into()), kiln_bp::topic::TopicLog::new(spec(1), BORN).with_log(8, 1 << 20));
    let t = host.ticket("ada", 1, 0);
    let (s, c) = plain();
    let mut a = Pair::open(&mut host, 1, s.clone(), c.clone(), &t, 0);
    a.client.subscribe(1, Topic { kind: 1, key: "items".into() }, 0);
    a.run(&mut host, 0, 5);
    assert_eq!(snapshots(&a.take_events()), 1);
    host.subs.clear();
    for i in 0..30u8 {
        host.publish(1, "items", Delta::Put { key: Key::Num { n: u64::from(i % 5) }, fields: vec![vec![i].into()] }, &mut []);
    }
    let t2 = host.ticket("ada", 2, 6);
    let mut b = Pair::open(&mut host, 2, s, c.clone(), &t2, 6);
    b.client = a.client.reconnect(c, &t2, 6);
    b.run(&mut host, 6, 10);
    host.publish(1, "items", Delta::Put { key: Key::Num { n: 9 }, fields: vec![vec![1].into()] }, &mut [&mut b.server]);
    b.run(&mut host, 11, 14);
    let evs = b.take_events();
    assert_eq!(snapshots(&evs), 1);
    assert!(evs.iter().any(|e| matches!(e, ClientEvent::Change { rev: 31, .. })));
    assert_eq!(b.client.state(1).map(|s| s.digest()), Some(host.topic(1, "items").state().digest()));
}
