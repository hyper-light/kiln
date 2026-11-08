//! bp-g3 step 4 (plan 01a1a832eb2f711f9c6f51d88effe89d): the acknowledgements a carrier asks for, which
//! its window and rate are made from. A probe of the carrier's goes ahead of everything, the page's
//! answer says the bytes it had received, and liveness's own probes stay apart.

mod common;

use common::*;
use kiln_bp::client::{ClientConfig, ClientEvent};
use kiln_bp::frames::*;
use kiln_bp::server::{ACK_NONCE, ACKS_OUT, Event, ServerConfig};

fn acked(host: &Host) -> Vec<(u64, u64, u64)> {
    host.events
        .iter()
        .filter_map(|e| match e {
            Event::Acked { got, rtt, at } => Some((*got, *rtt, *at)),
            _ => None,
        })
        .collect()
}

/// Do: open over a link of 20 ms each way, send a topic's changes, then ask for an acknowledgement.
/// Expect: the probe goes first in the next transmit, marked as the carrier's; its answer says every byte
/// the server had sent before it (and the probe), with a round trip of the link's 40 ms.
#[test]
fn bp_g3_s4_an_acknowledgement_says_what_the_page_has_and_when() -> Result<(), String> {
    let mut host = Host::new();
    let t = host.ticket("ada", 1, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &t, 0);
    p.up = Link::new(0.0, 20);
    p.down = Link::new(0.0, 20);
    p.client.subscribe(1, Topic { kind: 1, key: "k".into() }, 0);
    p.run(&mut host, 0, 200);
    host.publish(1, "k", Delta::Set { mask: kiln_bp::Bits(1), fields: vec![kiln_bp::Bytes(vec![7; 40])] }, &mut [&mut p.server]);
    p.run(&mut host, 200, 300);
    let sent_before = p.down.sent;
    p.server.ask_ack();
    assert_eq!(p.server.acks_out(), 1);
    let tx = p.server.poll_transmit(1 << 16, 300).ok_or("the probe goes")?;
    let first = tx.frames.first().ok_or("a frame")?;
    assert_eq!(first.kind, Kind::Probe, "ahead of everything");
    let probe = Frames::new(&tx.bytes).next_raw().and_then(|r| r.ok()).and_then(|(k, b)| decode_body(k, b).ok()).map(|r| r.frame);
    let Some(Frame::Probe(pr)) = probe else { return Err(format!("a probe: {probe:?}")) };
    assert_ne!(pr.nonce & ACK_NONCE, 0, "the carrier's, not liveness's");
    let len = tx.bytes.len() as u64;
    p.down.send(300, tx.bytes);
    p.run(&mut host, 300, 400);
    let a = acked(&host);
    assert_eq!(a.len(), 1, "one answer: {a:?}");
    let (got, rtt, at) = *a.first().ok_or("an answer")?;
    assert_eq!(at, 300);
    assert_eq!(rtt, 40, "the link's round trip");
    assert_eq!(got, sent_before + len, "every byte sent before the probe, and the probe");
    assert_eq!(p.server.acks_out(), 0);
    Ok(())
}

/// Do: ask for more acknowledgements than may be out, with the page not answering. Expect: at most
/// `ACKS_OUT` go; liveness's verdict isn't touched by them (the link stays alive while they're out).
#[test]
fn bp_g3_s4_acknowledgements_out_are_bounded_and_apart_from_liveness() {
    let mut host = Host::new();
    let t = host.ticket("ada", 2, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &t, 0);
    p.run(&mut host, 0, 50);
    p.up.cut = true;
    for now in 50..80 {
        p.server.ask_ack();
        if let Some(tx) = p.server.poll_transmit(1 << 16, now) {
            p.down.send(now, tx.bytes);
        }
    }
    assert_eq!(p.server.acks_out(), ACKS_OUT, "bounded");
    assert_eq!(p.server.verdict(100), kiln_bp::liveness::Verdict::Live, "liveness judges bytes, not these");
}

/// Do (seeded): a stream topic's appends of random sizes (up to 16 KiB), read through transmits of a
/// random chunk (256 B to 16 KiB) set as the fragment bound too, as goal 3's carrier does. Expect: no
/// transmit is past its chunk, and what the page reassembles is every byte appended, in order
/// (bp-g3 step 3, the chunker).
#[test]
fn bp_g3_s3_every_message_is_at_most_a_chunk_and_reassembles() -> Result<(), String> {
    let mut g = kiln_bp::rng::Rng::new(0xC4A7);
    for seed in 0..24u64 {
        let chunk = 256 + g.index(16 * 1024 - 256);
        let mut host = Host::new();
        let t = host.ticket("ada", 3, 0);
        // The fragment bound is the chunk less the slack a transmit keeps for compression's frame
        // (kiln-bp's `poll_transmit`, 16 B), as goal 3's carrier sets it: a fragment cut to the whole
        // chunk would never fit a compressing transmit's room.
        let s = ServerConfig { fragment: chunk - 16, compress: seed % 2 == 0, ..Default::default() };
        let mut p = Pair::open(&mut host, 1, s, ClientConfig::default(), &t, 0);
        p.room = chunk;
        p.client.subscribe(1, Topic { kind: 3, key: "log".into() }, 0);
        p.run(&mut host, 0, 5);
        let mut want: Vec<u8> = Vec::new();
        for i in 0..40u64 {
            let n = 1 + g.index(16 * 1024);
            let data: Vec<u8> = (0..n).map(|j| b'a' + u8::try_from((j + usize::try_from(i).unwrap_or(0)) % 26).unwrap_or(0)).collect();
            host.publish(3, "log", Delta::Append { at: want.len() as u64, data: kiln_bp::Bytes(data.clone()) }, &mut [&mut p.server]);
            want.extend_from_slice(&data);
        }
        for now in 6..8_000u64 {
            for m in p.up.due(now) {
                p.server.receive(&m, now, &mut host.tickets, &mut host.answers);
            }
            host.serve(&mut p.server, now);
            if let Some(tx) = p.server.poll_transmit(chunk, now) {
                if tx.bytes.len() > chunk {
                    return Err(format!("seed {seed}: a message of {} past its chunk {chunk}", tx.bytes.len()));
                }
                p.client.receive(&tx.bytes, now);
            }
            if let Some(tx) = p.client.poll_transmit(1 << 16, now) {
                p.up.send(now, tx.bytes);
            }
        }
        let got = p.client.state(1).map(|s| s.text.clone()).unwrap_or_default();
        if got != want {
            return Err(format!("seed {seed}, chunk {chunk}: reassembled {} B of {}", got.len(), want.len()));
        }
    }
    Ok(())
}

/// Do: what goal 3's carrier does each turn (its window's room, a chunk at most, an acknowledgement
/// asked a quarter-window apart or when the window is full), with a 40 KB answer to send over a link of
/// 1 ms each way. Expect: the answer arrives whole, the window never passed by more than a chunk.
#[test]
fn bp_g3_s4_a_large_answer_goes_through_the_window() -> Result<(), String> {
    use kiln_bp::window::{Config, Window};
    const CHUNK: usize = 1_200;
    let mut host = Host::new();
    host.auto_answer = false;
    let t = host.ticket("ada", 4, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig { fragment: CHUNK - 26, ..Default::default() }, ClientConfig::default(), &t, 0);
    p.up = Link::new(0.0, 1);
    p.down = Link::new(0.0, 1);
    let mut w = Window::new(Config::default());
    let mut since_ask = 0u64;
    p.client.request(9, Call::Http { method: Method::Get, path: "/x".into(), body: kiln_bp::Bytes::default() });
    let mut answered = false;
    let mut events = Vec::new();
    for now in 0..20_000u64 {
        for m in p.up.due(now) {
            p.server.receive(&m, now, &mut host.tickets, &mut host.answers);
        }
        while let Some(e) = p.server.poll_event() {
            match e {
                Event::Request { id, .. } if !answered => {
                    answered = true;
                    p.server.answer(id, Outcome::Reply { status: 200, body: kiln_bp::Bytes(vec![b'z'; 40_000]) }, None, kiln_bp::conn::Trust::Secret, &mut host.answers, now);
                }
                Event::Acked { got, rtt, at } => w.on_ack(now, got, rtt, at, false),
                _ => {}
            }
        }
        if w.stalled(CHUNK as u64, p.server.acks_out()) {
            p.server.ask_ack();
        }
        let room_w = usize::try_from(w.room()).unwrap_or(usize::MAX);
        let room = (CHUNK - 10).min(room_w).max(24);
        if let Some(tx) = p.server.poll_transmit(room, now) {
            let n = tx.bytes.len() as u64;
            w.on_sent(n);
            since_ask += n;
            if since_ask >= w.ack_every(CHUNK as u64) {
                p.server.ask_ack();
                since_ask = 0;
            }
            if w.in_flight() > w.window() + CHUNK as u64 {
                return Err(format!("in flight {} past window {}", w.in_flight(), w.window()));
            }
            p.down.send(now, tx.bytes);
        }
        for m in p.down.due(now) {
            p.client.receive(&m, now);
        }
        while let Some(e) = p.client.poll_event() {
            events.push(e);
        }
        if let Some(tx) = p.client.poll_transmit(1 << 16, now) {
            p.up.send(now, tx.bytes);
        }
        if events.iter().any(|e| matches!(e, ClientEvent::Answer { id: 9, .. })) {
            return Ok(());
        }
    }
    Err(format!("no answer; window {} in flight {} acks out {}", w.window(), w.in_flight(), p.server.acks_out()))
}

/// Do: a stream topic's appends (30 KB each) and an answer (40 KB) cut to a 16 KiB chunk, one message of
/// them sent, then the chunk shrinking to 1,200 B (goal 3's carrier follows the path's rate: on a link
/// that slows, or a reader that stops) with the rest still waiting. Expect: every message after it at
/// most the new chunk, what waited cut again (never a fragment of a fragment, which the page refuses),
/// and the page has every byte and the whole answer (bp-g3 step 3: a fragment past the room never sent
/// stalled the subscription for good).
#[test]
fn bp_g3_s3_a_chunk_that_shrinks_cuts_again_what_waits() -> Result<(), String> {
    const BIG: usize = 16 << 10;
    const SMALL: usize = 1_200;
    let mut host = Host::new();
    host.auto_answer = false;
    let t = host.ticket("ada", 5, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig { fragment: BIG - 16, ..Default::default() }, ClientConfig { window: 1 << 30, ..Default::default() }, &t, 0);
    p.room = BIG;
    p.client.subscribe(1, Topic { kind: 3, key: "log".into() }, 0);
    p.run(&mut host, 0, 5);
    p.client.request(9, Call::Http { method: Method::Get, path: "/big".into(), body: kiln_bp::Bytes::default() });
    p.run(&mut host, 6, 8);
    let mut want: Vec<u8> = Vec::new();
    for i in 0..6u8 {
        let data: Vec<u8> = (0..30_000u32).map(|j| b'a' + u8::try_from((j + u32::from(i)) % 26).unwrap_or(0)).collect();
        host.publish(3, "log", Delta::Append { at: want.len() as u64, data: kiln_bp::Bytes(data.clone()) }, &mut [&mut p.server]);
        want.extend_from_slice(&data);
    }
    let body: Vec<u8> = (0..40_000u32).map(|j| u8::try_from(j % 251).unwrap_or(0)).collect();
    p.server.answer(9, Outcome::Reply { status: 200, body: kiln_bp::Bytes(body.clone()) }, None, kiln_bp::conn::Trust::Secret, &mut host.answers, 9);
    // One message at the large chunk, then the chunk shrinks.
    let first = p.server.poll_transmit(BIG, 10).ok_or("nothing to send")?;
    p.client.receive(&first.bytes, 10);
    p.server.set_fragment(SMALL - 26);
    p.room = SMALL;
    for now in 11..20_000u64 {
        for m in p.up.due(now) {
            p.server.receive(&m, now, &mut host.tickets, &mut host.answers);
        }
        host.serve(&mut p.server, now);
        if let Some(tx) = p.server.poll_transmit(SMALL, now) {
            if tx.bytes.len() > SMALL {
                return Err(format!("a message of {} past the chunk {SMALL}", tx.bytes.len()));
            }
            p.client.receive(&tx.bytes, now);
        }
        while let Some(e) = p.client.poll_event() {
            p.client_events.push(e);
        }
        if let Some(tx) = p.client.poll_transmit(1 << 16, now) {
            p.up.send(now, tx.bytes);
        }
    }
    let refused: Vec<_> = p.client_events.iter().filter(|e| matches!(e, ClientEvent::Unreadable { .. } | ClientEvent::Unread { .. })).collect();
    if !refused.is_empty() {
        return Err(format!("the page refused frames: {refused:?}"));
    }
    let answered = p.client_events.iter().any(|e| matches!(e, ClientEvent::Answer { id: 9, outcome: Outcome::Reply { body: b, .. }, .. } if b.0 == body));
    if !answered {
        return Err("the answer didn't come whole".into());
    }
    let got = p.client.state(1).map(|s| s.text.clone()).unwrap_or_default();
    if got != want {
        return Err(format!("the page has {} B of {}", got.len(), want.len()));
    }
    Ok(())
}
