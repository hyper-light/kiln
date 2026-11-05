//! bp-g3 step 4 (plan 01a1a832eb2f711f9c6f51d88effe89d): the acknowledgements a carrier asks for, which
//! its window and rate are made from. A probe of the carrier's goes ahead of everything, the page's
//! answer says the bytes it had received, and liveness's own probes stay apart.

mod common;

use common::*;
use kiln_bp::client::ClientConfig;
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
