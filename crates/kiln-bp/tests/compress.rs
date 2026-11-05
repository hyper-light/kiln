//! bp-g2 step 8 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): compression by trust class, never for
//! secrets. A secret's answer goes beside compressed text frames uncompressed; and a BREACH-style
//! probe (attacker-chosen text in a log subscription beside a secret-bearing answer, guess after
//! guess) sees the same compressed sizes whether or not the secret was sent, so they can't vary
//! with how well a guess matches it. The same probe against one shared context finds the secret,
//! which shows the probe has the power it's meant to.

mod common;

use common::*;
use kiln_bp::client::ClientConfig;
use kiln_bp::compress::{Compressible, Compressors};
use kiln_bp::conn::{Lane, Trust};
use kiln_bp::frames::*;
use kiln_bp::server::{Server, ServerConfig};
use kiln_bp::wire::Bytes;

const SECRET: &str = "kiln_tok_Q7m2Xa9";

/// The compressed bytes the log's frames took, guess after guess, on a connection that sent the
/// secret's answer first (or didn't).
fn probe(guesses: &[String], send_secret: bool) -> Vec<usize> {
    let mut host = Host::new();
    let t = host.ticket("victim", 1, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &t, 0);
    p.client.subscribe(1, Topic { kind: 3, key: "log".into() }, 0);
    p.run(&mut host, 0, 3);
    host.auto_answer = false;
    p.client.request(9, Call::Http { method: Method::Post, path: "/api/tokens".into(), body: Bytes::default() });
    p.run(&mut host, 4, 6);
    if send_secret {
        p.server.answer_secret(9, Outcome::Reply { status: 201, body: Bytes::from(format!("{{\"token\":\"{SECRET}\"}}").as_str()) }, &mut host.answers, 7);
    }
    let mut at = 0u64;
    let mut sizes = Vec::new();
    for (i, g) in guesses.iter().enumerate() {
        let line = format!("{{\"token\":\"{g}\"}}\n");
        let n = line.len() as u64;
        host.publish(3, "log", Delta::Append { at, data: line.as_str().into() }, &mut [&mut p.server]);
        at += n;
        let now = 10 + i as u64;
        let mut size = 0;
        while let Some(t) = p.server.poll_transmit(64 * 1024, now) {
            for f in &t.frames {
                if f.kind == Kind::Compressed && f.lane == Lane::Sub(1) {
                    size += f.len;
                }
            }
        }
        sizes.push(size);
    }
    sizes
}

fn guesses() -> Vec<String> {
    let mut g: Vec<String> = ["kiln_tok_A", "kiln_tok_Q", "kiln_tok_Q7", "kiln_tok_Q7m", "kiln_tok_Q7m2", "kiln_tok_Q7m2X", "kiln_tok_Q7m2Xa", SECRET, "kiln_tok_Z7m2Xa9", "kiln_tok_Q7m2Xa8"].iter().map(|s| (*s).to_owned()).collect();
    g.extend((0..20).map(|i| format!("kiln_tok_{i:07}")));
    g
}

/// The BREACH probe: the attacker's compressed sizes are the same with and without the secret,
/// for every guess, the right one among them.
#[test]
fn bp_g2_s8_a_breach_probe_learns_nothing_of_the_secret() {
    let g = guesses();
    let with = probe(&g, true);
    let without = probe(&g, false);
    println!("compressed sizes of the attacker's frames, guess by guess, with the secret sent: {with:?}");
    assert_eq!(with, without, "the attacker's sizes varied with the secret");
    assert!(with.iter().all(|s| *s > 0), "the log was compressed");
}

/// The probe has power: with the secret in the same context (what the protocol never does), the
/// right guess compresses smaller than wrong ones of its length.
#[test]
fn bp_g2_s8_the_probe_would_find_a_secret_sharing_a_context() {
    let mut sizes = Vec::new();
    for g in guesses() {
        let mut z = Compressors::new(false);
        let secret = Frame::Change(Change { sub: 2, rev: 1, step: 1, delta: Delta::Append { at: 0, data: Bytes::from(format!("{{\"token\":\"{SECRET}\"}}").as_str()) } }).encode(VERSION);
        let _ = z.deflate(Lane::Control, &Compressible::of(Kind::Change, Trust::Text, &secret).into_iter().collect::<Vec<_>>());
        let guess = Frame::Change(Change { sub: 1, rev: 1, step: 1, delta: Delta::Append { at: 0, data: Bytes::from(format!("{{\"token\":\"{g}\"}}\n").as_str()) } }).encode(VERSION);
        let (_, out) = z.deflate(Lane::Control, &Compressible::of(Kind::Change, Trust::Text, &guess).into_iter().collect::<Vec<_>>());
        sizes.push((g, out.len()));
    }
    let right = sizes.iter().find(|(g, _)| g == SECRET).map(|x| x.1).unwrap_or(0);
    let wrong_same_length: Vec<usize> = sizes.iter().filter(|(g, _)| g.len() == SECRET.len() && g != SECRET).map(|x| x.1).collect();
    println!("one shared context: the right guess {right} B, wrong ones of its length {wrong_same_length:?}");
    assert!(wrong_same_length.iter().all(|w| *w > right));
}

/// A secret's answer, and an answer whose route isn't classed, go raw beside compressed frames;
/// classed text and records go in their own contexts.
#[test]
fn bp_g2_s8_secret_frames_go_raw_beside_compressed_ones() {
    let mut host = Host::new();
    host.auto_answer = false;
    let t = host.ticket("ada", 1, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &t, 0);
    p.client.subscribe(1, Topic { kind: 3, key: "log".into() }, 0);
    p.client.subscribe(2, Topic { kind: 1, key: "runs".into() }, 0);
    p.client.request(1, Call::Http { method: Method::Get, path: "/a".into(), body: Bytes::default() });
    p.client.request(2, Call::Http { method: Method::Get, path: "/b".into(), body: Bytes::default() });
    p.run(&mut host, 0, 4);
    host.publish(3, "log", Delta::Append { at: 0, data: "hello hello hello\n".into() }, &mut [&mut p.server]);
    host.publish(1, "runs", Delta::Put { key: Key::Str { s: "r1".into() }, fields: vec![Bytes::from("main")] }, &mut [&mut p.server]);
    p.server.answer_secret(1, Outcome::Reply { status: 200, body: Bytes::from("{\"code\":\"123456\"}") }, &mut host.answers, 5);
    p.server.answer(2, Outcome::Reply { status: 200, body: Bytes::from("{}") }, None, Trust::Secret, &mut host.answers, 5);
    let t = p.server.poll_transmit(64 * 1024, 5);
    let kinds: Vec<(Kind, Lane)> = t.map(|t| t.frames.iter().map(|f| (f.kind, f.lane)).collect()).unwrap_or_default();
    println!("{kinds:?}");
    assert!(kinds.contains(&(Kind::SecretAnswer, Lane::Control)));
    assert!(kinds.contains(&(Kind::Answer, Lane::Control)));
    assert!(kinds.contains(&(Kind::Compressed, Lane::Sub(1))));
    assert!(kinds.contains(&(Kind::Compressed, Lane::Sub(2))));
    let _ = Server::new(ServerConfig::default(), 9, BORN, 0);
}
