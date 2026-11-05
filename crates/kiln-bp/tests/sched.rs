//! bp-g2 step 7 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): the send scheduler. On a virtual 50
//! kbit/s link with a 600 ms round trip, a terminal's echo waits behind at most one fragment while
//! a log floods and a snapshot goes; samples arrive as their latest value, never one staler than
//! one already had; a client that stops granting credit holds the server to its memory bound and
//! is resumed from its cursor when it grants again; and no command, ask, answer or ack is ever
//! merged or dropped.

mod common;

use common::*;
use kiln_bp::client::{ClientConfig, ClientEvent};
use kiln_bp::conn::{Lane, Prio, info, never_coalesced};
use kiln_bp::frames::*;
use kiln_bp::rng::Rng;
use kiln_bp::server::{Event, ServerConfig};
use kiln_bp::wire::{Bits, Bytes, to_bytes};

/// 50 kbit/s is 6.25 bytes a millisecond.
const RATE: f64 = 6.25;
const FRAGMENT: usize = 256;

/// AC: p99.9 of the time an interactive frame waits behind other frames, at 50 kbit/s during a
/// log flood and a snapshot, is at most one fragment's serialization (plus its own).
#[test]
fn bp_g2_s7_an_echo_waits_behind_at_most_one_fragment() {
    let seeds: u64 = std::env::var("KILN_BP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let one_fragment = FRAGMENT as f64 / RATE;
    let mut waits: Vec<f64> = Vec::new();
    for seed in 0..seeds {
        let mut g = Rng::new(0x5EED ^ seed);
        let mut host = Host::new();
        for i in 0..300u32 {
            host.publish(5, "history", Delta::Put { key: Key::Num { n: u64::from(i) }, fields: vec![Bytes(vec![b'h'; 80 + g.index(40)])] }, &mut []);
        }
        let t = host.ticket("ada", 1, 0);
        let s = ServerConfig { fragment: FRAGMENT, compress: false, ..Default::default() };
        let mut p = Pair::open(&mut host, 1, s, ClientConfig { caps: Bits(0), ..Default::default() }, &t, 0);
        p.up = Link::new(RATE, 300);
        p.down = Link::new(RATE, 300);
        p.room = FRAGMENT;
        p.client.subscribe(1, Topic { kind: 4, key: "terminal".into() }, 0);
        p.client.subscribe(2, Topic { kind: 3, key: "log".into() }, 0);
        p.client.subscribe(3, Topic { kind: 5, key: "history".into() }, 0);
        let mut log_at = 0u64;
        let mut term_at = 0u64;
        let mut pushed: Vec<(u64, u64)> = Vec::new(); // (rev, when)
        let mut seen = std::collections::BTreeSet::new();
        for now in 0..40_000u64 {
            if now > 1500 && now % 20 == 0 {
                let line = format!("{} compiling crate {} of 600 ...\n", now, g.below(600));
                let n = line.len() as u64;
                host.publish(3, "log", Delta::Append { at: log_at, data: line.as_str().into() }, &mut [&mut p.server]);
                log_at += n;
            }
            if now > 1500 && now % 300 == 7 {
                let rev = host.publish(4, "terminal", Delta::Append { at: term_at, data: "ls\r".into() }, &mut [&mut p.server]);
                term_at += 3;
                pushed.push((rev, now));
            }
            // The carrier: whenever the link is free, it takes one transmit of up to a fragment.
            for m in p.up.due(now) {
                p.server.receive(&m, now, &mut host.tickets, &mut host.answers);
            }
            host.serve(&mut p.server, now);
            for m in p.down.due(now) {
                p.client.receive(&m, now);
            }
            if p.up.idle(now)
                && let Some(t) = p.client.poll_transmit(FRAGMENT, now)
            {
                p.up.send(now, t.bytes);
            }
            if p.down.idle(now) {
                let start = p.down.free_at.max(now as f64);
                if let Some(t) = p.server.poll_transmit(FRAGMENT, now) {
                    for f in &t.frames {
                        if f.lane == Lane::Sub(1) && f.prio == Prio::Interactive {
                            // Which echo this is: the oldest pushed not yet seen.
                            if let Some((rev, when)) = pushed.iter().find(|(r, _)| !seen.contains(r)).copied() {
                                seen.insert(rev);
                                waits.push(start - when as f64);
                            }
                        }
                    }
                    p.down.send(now, t.bytes);
                }
            }
        }
        assert!(seen.len() >= pushed.len().saturating_sub(2), "seed {seed}: {} of {} echoes went", seen.len(), pushed.len());
        while let Some(e) = p.client.poll_event() {
            p.client_events.push(e);
        }
        assert!(p.client_events.iter().any(|e| matches!(e, ClientEvent::Ready { sub: 3, .. })), "seed {seed}: the snapshot finished");
    }
    waits.sort_by(f64::total_cmp);
    let at = |q: f64| waits.get(((waits.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(f64::MAX);
    println!("{} echoes: wait p50 {:.1} ms, p99 {:.1} ms, p99.9 {:.1} ms, max {:.1} ms; one fragment is {one_fragment:.1} ms", waits.len(), at(0.5), at(0.99), at(0.999), at(1.0));
    assert!(waits.len() > 1000);
    assert!(at(0.999) <= one_fragment + 1.0, "p99.9 {} ms past one fragment's {one_fragment} ms", at(0.999));
}

/// AC: no stale state value is delivered after a newer one for its key was queued: each sample
/// that arrives is newer than the last that did, and the last is the newest.
#[test]
fn bp_g2_s7_samples_arrive_as_their_latest_value() {
    let seeds: u64 = std::env::var("KILN_BP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let mut merged = 0usize;
    for seed in 0..seeds {
        let mut g = Rng::new(0xA11 ^ seed);
        let mut host = Host::new();
        let t = host.ticket("ada", 1, 0);
        let s = ServerConfig { fragment: FRAGMENT, ..Default::default() };
        let mut p = Pair::open(&mut host, 1, s, ClientConfig::default(), &t, 0);
        p.up = Link::new(RATE, 300);
        p.down = Link::new(RATE, 300);
        p.room = FRAGMENT;
        p.client.subscribe(1, Topic { kind: 2, key: "samples".into() }, 0);
        p.client.subscribe(2, Topic { kind: 3, key: "log".into() }, 0);
        let mut newest = [0u64; 8];
        let mut log_at = 0;
        let mut published = 0usize;
        for now in 0..30_000u64 {
            if now > 1000 && now % 2 == 0 {
                let k = g.index(8);
                let v = newest.get(k).copied().unwrap_or(0) + 1 + g.below(3);
                if let Some(slot) = newest.get_mut(k) {
                    *slot = v;
                }
                host.publish(2, "samples", Delta::Put { key: Key::Str { s: format!("machine-{k}") }, fields: vec![to_bytes(&v).into(), Bytes(vec![g.byte(); 4])] }, &mut [&mut p.server]);
                published += 1;
            }
            if now > 1000 && now % 15 == 0 {
                let line = format!("{now}\n");
                let n = line.len() as u64;
                host.publish(3, "log", Delta::Append { at: log_at, data: line.as_str().into() }, &mut [&mut p.server]);
                log_at += n;
            }
            p.step(&mut host, now);
        }
        p.run(&mut host, 30_000, 45_000);
        let mut last = std::collections::BTreeMap::new();
        let mut got = 0;
        for e in p.take_events() {
            if let ClientEvent::Change { sub: 1, delta: Delta::Put { key: Key::Str { s }, fields }, .. } = e {
                let v: u64 = fields.first().and_then(|f| kiln_bp::wire::from_bytes(&f.0).ok()).unwrap_or(0);
                let prev = last.insert(s.clone(), v).unwrap_or(0);
                assert!(v > prev, "seed {seed}: {s} went from {prev} to {v}");
                got += 1;
            }
        }
        for (k, v) in newest.iter().enumerate() {
            if *v > 0 {
                assert_eq!(last.get(&format!("machine-{k}")).copied(), Some(*v), "seed {seed}: machine-{k} ends at its newest");
            }
        }
        merged += published.saturating_sub(got);
    }
    println!("samples merged while they waited: {merged}");
    assert!(merged > 0, "the link was never busy enough to merge any");
}

/// AC: peak queued bytes per client stay within the configured bound while a client stops
/// granting credit; once it grants again it is resumed from its cursor and converges.
#[test]
fn bp_g2_s7_a_stalled_client_is_bounded_then_resumed() {
    let seeds: u64 = std::env::var("KILN_BP_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(12);
    let bound = 64 * 1024;
    for seed in 0..seeds {
        let mut g = Rng::new(0xB0 ^ seed);
        let mut host = Host::new();
        host.topics.insert((3, "log".into()), kiln_bp::topic::TopicLog::new(spec(3), BORN).with_text_keep(8 * 1024));
        let t = host.ticket("ada", 1, 0);
        let s = ServerConfig { fragment: FRAGMENT, max_queued: bound, compress: false, ..Default::default() };
        let mut p = Pair::open(&mut host, 1, s, ClientConfig { caps: Bits(0), ..Default::default() }, &t, 0);
        p.client.subscribe(1, Topic { kind: 3, key: "log".into() }, 4096);
        p.client.subscribe(2, Topic { kind: 2, key: "samples".into() }, 0);
        let mut log_at = 0;
        let mut peak = 0usize;
        let mut resumed = false;
        for now in 0..3000u64 {
            let line = format!("{now} {}\n", "x".repeat(g.index(80)));
            let n = line.len() as u64;
            host.publish(3, "log", Delta::Append { at: log_at, data: line.as_str().into() }, &mut [&mut p.server]);
            log_at += n;
            if now % 10 == 0 {
                host.publish(2, "samples", Delta::Put { key: Key::Num { n: g.below(4) }, fields: vec![to_bytes(&now).into()] }, &mut [&mut p.server]);
            }
            // The page reads the log again after two seconds: it grants as it consumes.
            if now >= 2000 {
                let had = p.client.received(1);
                p.client.grant(1, had + 64 * 1024);
            }
            p.step(&mut host, now);
            peak = peak.max(p.server.queued());
            resumed |= host.events.iter().any(|e| matches!(e, Event::Resume { sub: 1, .. }));
        }
        p.run(&mut host, 3000, 3100);
        assert!(peak <= bound, "seed {seed}: {peak} bytes queued past the bound of {bound}");
        assert!(resumed, "seed {seed}: the stalled subscription wasn't resumed");
        // The server keeps the log's last 8 KiB; the page has all it was sent since its snapshot.
        let server = host.topic(3, "log").state().clone();
        let page = p.client.state(1).cloned().unwrap_or_default();
        assert_eq!(page.text_end(), server.text_end(), "seed {seed}: the log converges to its end");
        assert!(page.text.ends_with(&server.text), "seed {seed}: the page has what the server holds");
        assert_eq!(p.client.state(2).map(|s| s.digest()), Some(host.topic(2, "samples").state().digest()), "seed {seed}: the samples converge");
    }
}

/// AC: no command, ask, answer or ack is ever merged or dropped: every kind of those classes is
/// marked never coalesced, and a hundred answers and keystrokes each arrive, in order.
#[test]
fn bp_g2_s7_commands_and_answers_are_never_merged_or_dropped() {
    for k in KINDS {
        let class = info(*k).class;
        if ["command", "ask", "answer", "ack", "handshake"].iter().any(|c| class.starts_with(c)) {
            assert!(never_coalesced(*k), "{k:?} ({class}) may be coalesced");
        }
    }
    let mut host = Host::new();
    host.auto_answer = false;
    let t = host.ticket("ada", 1, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig { fragment: FRAGMENT, ..Default::default() }, ClientConfig::default(), &t, 0);
    p.up = Link::new(RATE, 300);
    p.down = Link::new(RATE, 300);
    p.room = FRAGMENT;
    p.run(&mut host, 0, 700);
    for i in 0..100u64 {
        p.client.request(i, Call::Http { method: Method::Post, path: format!("/api/x/{i}"), body: Bytes::default() });
        p.client.input(9, &[b'a' + (i % 26) as u8]);
    }
    p.run(&mut host, 701, 6000);
    let mut answers = Vec::new();
    for (conn, id, _) in host.requests.clone() {
        assert_eq!(conn, 1);
        answers.push(id);
        p.server.answer(id, Outcome::Reply { status: 200, body: Bytes::default() }, None, kiln_bp::conn::Trust::Secret, &mut host.answers, 6000);
    }
    assert_eq!(answers, (0..100).collect::<Vec<_>>());
    let inputs: Vec<u8> = host.events.iter().filter_map(|e| if let Event::Input { data, .. } = e { data.0.first().copied() } else { None }).collect();
    assert_eq!(inputs, (0..100u64).map(|i| b'a' + (i % 26) as u8).collect::<Vec<_>>());
    p.run(&mut host, 6000, 12_000);
    let got: Vec<u64> = p.take_events().into_iter().filter_map(|e| if let ClientEvent::Answer { id, .. } = e { Some(id) } else { None }).collect();
    assert_eq!(got, (0..100).collect::<Vec<_>>());
}
