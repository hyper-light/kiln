//! bp-g2 step 5 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): the connection core's open, auth,
//! version skew, requests and liveness, client and server halves over an in-memory loopback.

mod common;

use common::*;
use kiln_bp::client::{ClientConfig, ClientEvent};
use kiln_bp::conn::Trust;
use kiln_bp::frames::*;
use kiln_bp::liveness::Verdict;
use kiln_bp::server::{Event, ServerConfig};
use kiln_bp::tickets::{Principal, Redeem, Tickets};
use kiln_bp::wire::Why;

fn cfgs(sv: (u32, u32), cv: (u32, u32)) -> (ServerConfig, ClientConfig) {
    (ServerConfig { version: sv.0, oldest: sv.1, build: "srv-build".into(), ..Default::default() }, ClientConfig { version: cv.0, oldest: cv.1, ..Default::default() })
}

fn opened(p: &mut Pair) -> Option<u32> {
    p.take_events().into_iter().find_map(|e| match e {
        ClientEvent::Opened { version, .. } => Some(version),
        _ => None,
    })
}

fn refused(p: &mut Pair) -> Option<(OpenRefusal, String)> {
    p.take_events().into_iter().find_map(|e| match e {
        ClientEvent::Refused { code, build, .. } => Some((code, build)),
        _ => None,
    })
}

/// Version pairs N to N, N-1 to N and N to N-1 (as builds with versions 3 and 2 speaking down to
/// 2 and 1), and one too old to be served, which is told the build to switch to.
#[test]
fn bp_g2_s5_opens_across_version_pairs() {
    let cases = [((3, 2), (3, 2), Some(3)), ((3, 2), (2, 1), Some(2)), ((2, 1), (3, 2), Some(2)), ((3, 2), (1, 1), None), ((1, 1), (3, 2), None)];
    for (i, (sv, cv, want)) in cases.into_iter().enumerate() {
        let mut host = Host::new();
        let t = host.ticket("ada", i as u8, 0);
        let (s, c) = cfgs(sv, cv);
        let mut p = Pair::open(&mut host, 1, s, c, &t, 0);
        p.run(&mut host, 0, 5);
        match want {
            Some(v) => {
                assert_eq!(opened(&mut p), Some(v), "server {sv:?} client {cv:?}");
                assert_eq!(p.server.version(), v);
            }
            None => {
                let r = refused(&mut p);
                let code = if cv.0 < sv.1 { OpenRefusal::TooOld } else { OpenRefusal::TooNew };
                assert_eq!(r, Some((code.clone(), "srv-build".into())), "server {sv:?} client {cv:?}");
                assert!(host.events.iter().any(|e| matches!(e, Event::OpenRefused { code: c, .. } if *c == code)));
            }
        }
    }
}

/// Tickets valid, expired, reused and forged: only the first opens, each other is refused typed
/// and noted for the refusal counters and the audit log (`Event::OpenRefused`).
#[test]
fn bp_g2_s5_tickets_open_once_within_their_life() {
    let mut host = Host::new();
    let good = host.ticket("ada", 1, 0);
    let late = host.ticket("ada", 2, 0);
    let mut p = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &good, 10);
    p.run(&mut host, 10, 12);
    assert_eq!(opened(&mut p), Some(VERSION));
    assert_eq!(p.server.principal().who, "ada");
    for (ticket, at, code) in [(good.clone(), 20, OpenRefusal::Reused), (late, 31_000, OpenRefusal::Expired), (vec![9; 32], 40, OpenRefusal::BadTicket), (Vec::new(), 50, OpenRefusal::BadTicket)] {
        host.events.clear();
        let mut q = Pair::open(&mut host, 2, ServerConfig::default(), ClientConfig::default(), &ticket, at);
        q.run(&mut host, at, at + 2);
        assert_eq!(refused(&mut q).map(|r| r.0), Some(code.clone()));
        assert!(q.server.is_closed());
        assert!(host.events.iter().any(|e| matches!(e, Event::OpenRefused { code: c, .. } if *c == code)));
    }
    // The store itself, directly.
    let mut t = Tickets::with_life(100);
    let k = t.mint([3; 32], Principal::default(), 0);
    assert_eq!(t.redeem(&k, 101).err(), Some(OpenRefusal::Expired));
    assert_eq!(t.redeem(&k, 1000).err(), Some(OpenRefusal::BadTicket), "let go of after twice its life");
}

/// A first frame that isn't an open is refused; frames the server doesn't take up are refused
/// `out_of_order`, told to the browser, and the next frame is read.
#[test]
fn bp_g2_s5_out_of_order_frames_are_refused_and_told() {
    let mut host = Host::new();
    let t = host.ticket("ada", 1, 0);
    let mut s = kiln_bp::server::Server::new(ServerConfig::default(), 1, BORN, 0);
    s.receive(&Frame::Idle(Idle { got: 0 }).encode(VERSION), 0, &mut host.tickets, &mut host.answers);
    assert!(s.is_closed());
    let mut p = Pair::open(&mut host, 2, ServerConfig::default(), ClientConfig::default(), &t, 0);
    p.run(&mut host, 0, 3);
    let mut msg = Frame::Answer(Answer { id: 1, outcome: Outcome::Taken, at: None }).encode(VERSION);
    msg.extend(Frame::Request(Request { id: 7, call: Call::Http { method: Method::Get, path: "/x".into(), body: Default::default() } }).encode(VERSION));
    host.events.clear();
    p.server.receive(&msg, 4, &mut host.tickets, &mut host.answers);
    host.serve(&mut p.server, 4);
    assert!(host.events.iter().any(|e| matches!(e, Event::Unreadable { kind: Some(4), why: Why::OutOfOrder })));
    assert!(host.events.iter().any(|e| matches!(e, Event::Request { id: 7, .. })));
    p.run(&mut host, 4, 6);
    assert!(p.take_events().iter().any(|e| matches!(e, ClientEvent::Unread { kind: 4, why: UnreadWhy::OutOfOrder, .. })));
}

/// A request resent under the same id after a reconnect is answered once, with the same answer;
/// one resent while it still runs is not run again.
#[test]
fn bp_g2_s5_a_request_resent_after_a_reconnect_is_answered_once() {
    let mut host = Host::new();
    host.auto_answer = false;
    let t1 = host.ticket("ada", 1, 0);
    let mut a = Pair::open(&mut host, 1, ServerConfig::default(), ClientConfig::default(), &t1, 0);
    a.run(&mut host, 0, 2);
    a.client.request(42, Call::Command { session: "s1".into(), command: "{\"type\":\"interrupt\"}".into() });
    a.run(&mut host, 3, 5);
    assert_eq!(host.requests.len(), 1);
    // The link drops before the answer; the browser connects again and sends it again.
    let t2 = host.ticket("ada", 2, 6);
    let mut b = Pair::open(&mut host, 2, ServerConfig::default(), ClientConfig::default(), &t2, 6);
    b.client = a.client.reconnect(ClientConfig::default(), &t2, 6);
    host.answers.gone(1);
    b.run(&mut host, 6, 9);
    assert_eq!(host.requests.len(), 1, "not run a second time while the first runs");
    // The first finishes: its answer goes to the connection that waits now.
    let to = host.answers.finish("ada", 42, &Outcome::Accepted { epoch: 1, seq: 5, at: 9, repeat: false }, None, 10);
    assert_eq!(to, vec![2]);
    b.server.deliver(42, Outcome::Accepted { epoch: 1, seq: 5, at: 9, repeat: false }, None, Trust::Secret);
    b.run(&mut host, 10, 12);
    let answers: Vec<_> = b.take_events().into_iter().filter_map(|e| if let ClientEvent::Answer { id, outcome, .. } = e { Some((id, outcome)) } else { None }).collect();
    assert_eq!(answers, vec![(42, Outcome::Accepted { epoch: 1, seq: 5, at: 9, repeat: false })]);
    assert!(b.client.pending().is_empty());
    // Sent again on a third connection: the kept answer, not a second run.
    let t3 = host.ticket("ada", 3, 13);
    let mut c = Pair::open(&mut host, 3, ServerConfig::default(), ClientConfig::default(), &t3, 13);
    c.run(&mut host, 13, 15);
    c.client.request(42, Call::Command { session: "s1".into(), command: "{\"type\":\"interrupt\"}".into() });
    c.run(&mut host, 16, 18);
    assert_eq!(host.requests.len(), 1);
    assert!(c.take_events().iter().any(|e| matches!(e, ClientEvent::Answer { id: 42, outcome: Outcome::Accepted { seq: 5, .. }, .. })));
    // Ids are their caller's own: another person's 42 runs.
    let t4 = host.ticket("bo", 4, 19);
    let mut d = Pair::open(&mut host, 4, ServerConfig::default(), ClientConfig::default(), &t4, 19);
    d.run(&mut host, 19, 21);
    d.client.request(42, Call::Http { method: Method::Get, path: "/api/runs".into(), body: Default::default() });
    d.run(&mut host, 22, 24);
    assert_eq!(host.requests.len(), 2);
}

/// `Parked` is for now: resent, it's answered `Parked` until the real answer takes its place; a
/// not-this-time answer isn't kept, so a resend runs again.
#[test]
fn bp_g2_s5_answers_last_as_their_finality_says() {
    let mut ans = kiln_bp::answers::Answers::new();
    use kiln_bp::answers::Taking;
    assert_eq!(ans.begin("ada", 1, 1, 0), Taking::Run);
    ans.finish("ada", 1, &Outcome::Parked, None, 1);
    assert_eq!(ans.begin("ada", 1, 2, 2), Taking::Kept { outcome: Outcome::Parked, at: None });
    let to = ans.finish("ada", 1, &Outcome::Taken, None, 3);
    assert!(to.contains(&2));
    assert_eq!(ans.begin("ada", 1, 3, 4), Taking::Kept { outcome: Outcome::Taken, at: None });
    assert_eq!(ans.begin("ada", 2, 1, 5), Taking::Run);
    ans.finish("ada", 2, &Outcome::Nack { code: "unreachable".into(), why: String::new(), again: true }, None, 6);
    assert_eq!(ans.begin("ada", 2, 1, 7), Taking::Run, "not this time: it runs again");
    // Kept ten minutes.
    assert_eq!(ans.begin("ada", 1, 1, 3 + 10 * 60 * 1000 + 1), Taking::Run);
}

/// The link-liveness rule in the core: idle frames keep a quiet link live and never count as
/// activity; a link gone dark is suspect after its expected gap, probed, and dead a round trip
/// later; a session that expires cuts the connection.
#[test]
fn bp_g2_s5_liveness_by_bytes_arriving_and_session_expiry() {
    let mut host = Host::new();
    let t = host.tickets.mint([5; 32], Principal { who: "ada".into(), session: "s".into(), expires_at: Some(200_000) }, 0);
    let s = ServerConfig { idle_ms: 1000, ..Default::default() };
    let c = ClientConfig { idle_ms: 1000, ..Default::default() };
    let mut p = Pair::open(&mut host, 1, s, c, &t, 0);
    p.up = Link::new(10.0, 50);
    p.down = Link::new(10.0, 50);
    p.run(&mut host, 0, 20_000);
    assert_eq!(p.client.verdict(20_000), Verdict::Live);
    assert_eq!(p.server.verdict(20_000), Verdict::Live);
    assert!(p.server.activity() <= 200, "idle frames aren't activity: {}", p.server.activity());
    let idles = p.client.counts.get(&Kind::Idle.index()).copied().unwrap_or(0);
    assert!(idles >= 15, "{idles} idle frames in 20 s at 1 s");
    // Dark: nothing arrives either way.
    p.up.cut = true;
    p.down.cut = true;
    let dark = 20_001;
    let mut dead_at = None;
    for t in dark..dark + 120_000 {
        p.step(&mut host, t);
        if p.client.verdict(t) == Verdict::Dead {
            dead_at = Some(t);
            break;
        }
    }
    let dead_at = dead_at.unwrap_or(u64::MAX);
    let bound = p.client.liveness().last_arrival() + 1000 + p.client.liveness().spread() + p.client.liveness().rtt_bound() * ((1 << kiln_bp::liveness::BACKOFF) - 1) + 2;
    assert!(dead_at <= bound, "dead at {dead_at}, bound {bound}");
    assert!(dead_at >= p.client.liveness().last_arrival() + 1000, "not before its idle period");
    // The session's end cuts the connection.
    let t2 = host.tickets.mint([6; 32], Principal { who: "ada".into(), session: "s".into(), expires_at: Some(500) }, 0);
    let mut q = Pair::open(&mut host, 2, ServerConfig::default(), ClientConfig::default(), &t2, 0);
    q.run(&mut host, 0, 600);
    assert!(q.server.is_closed());
    assert!(q.take_events().iter().any(|e| matches!(e, ClientEvent::Closed { code: CloseCode::Expired, .. })));
}

/// An older client gets only kinds and fields its version knows: every frame the server sends a
/// version-1 client is a kind of version 1, read with no newer bytes left over.
#[test]
fn bp_g2_s5_an_older_client_gets_only_what_it_knows() {
    let mut host = Host::new();
    let t = host.ticket("ada", 1, 0);
    let (s, c) = cfgs((3, 1), (1, 1));
    let mut p = Pair::open(&mut host, 1, s, c, &t, 0);
    p.room = 512;
    p.run(&mut host, 0, 3);
    p.client.subscribe(1, Topic { kind: 1, key: "runs".into() }, 0);
    p.client.request(1, Call::Http { method: Method::Get, path: "/api/runs".into(), body: Default::default() });
    p.run(&mut host, 4, 8);
    host.publish(1, "runs", Delta::Put { key: Key::Str { s: "r1".into() }, fields: vec![vec![1].into()] }, &mut [&mut p.server]);
    let mut sent = Vec::new();
    for now in 9..20 {
        if let Some(t) = p.server.poll_transmit(4096, now) {
            sent.push(t.bytes);
        }
    }
    assert_eq!(p.server.version(), 1);
    assert!(!sent.is_empty());
    for m in sent {
        for r in decode_message(&m) {
            let r = r.unwrap_or_else(|e| Read { frame: Frame::Idle(Idle { got: 0 }), extra: e.why as usize + 1000 });
            assert!(r.frame.kind().since() <= 1);
            assert_eq!(r.extra, 0);
        }
    }
}
