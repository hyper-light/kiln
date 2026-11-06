//! The door's QUIC endpoint against hyper-quic clients over the in-memory network (goal 4 step 5,
//! plan 01a1a832eb597110a6dcfda227981604).

use hyper_quic::Dir;

use super::conn::Event;
use super::door::DoorConfig;
use super::testnet::{Echo, Net, open_session};
use super::tests::{CHROME, v};
use super::{frame, stream_type};

/// A browser dials, opens a session, and echoes one stream and one datagram: the events the
/// door told, in order, as a trace.
fn session_and_echo(seed: u64) -> Option<Vec<String>> {
    let mut net = Net::new(DoorConfig::door(), Echo, seed)?;
    let b = net.dial()?;
    net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.connected()));
    let connect = open_session(net.b(b)?, CHROME, b"/wt?ticket=t")?;
    let admitted = net.until(10_000, |n| n.events.iter().any(|(_, e)| matches!(e, Event::Accepted(_))));
    assert!(admitted, "no session: {:?}", net.events);
    // The response reached the browser: 200.
    net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.got.get(&connect).is_some_and(|g| !g.is_empty())));
    let got = net.b(b)?.got.get(&connect).cloned().unwrap_or_default();
    assert_eq!(super::tests::status(&got), Some(b"200".to_vec()));
    // The session's windows and stream counts were raised.
    let id = net.door.ids().into_iter().next()?;
    let cfg = DoorConfig::door();
    assert_eq!(net.door.quic(id)?.max_concurrent_streams(Dir::Bi), cfg.post.bidi_streams);
    assert_eq!(net.door.quic(id)?.max_concurrent_streams(Dir::Uni), cfg.post.uni_streams);
    // A WebTransport bidi stream, echoed.
    let s = net.b(b)?.open(true)?;
    let mut bytes = v(frame::WT_STREAM);
    bytes.extend(v(connect));
    bytes.extend_from_slice(b"ping");
    net.b(b)?.write(s, &bytes);
    net.b(b)?.finish(s);
    net.b(b)?.watch(s);
    let echoed = net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.got.get(&s).is_some_and(|g| g.as_slice() == b"ping")));
    assert!(echoed, "{:?}", net.b(b).map(|b| b.got.get(&s).cloned()));
    // A uni stream, echoed on one of the door's.
    let u = net.b(b)?.open(false)?;
    let mut bytes = v(stream_type::WEBTRANSPORT);
    bytes.extend(v(connect));
    bytes.extend_from_slice(b"uni");
    net.b(b)?.write(u, &bytes);
    net.b(b)?.finish(u);
    let mut want = v(stream_type::WEBTRANSPORT);
    want.extend(v(connect));
    want.extend_from_slice(b"uni");
    let uni = net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.got.values().any(|g| *g == want)));
    assert!(uni);
    // A datagram, echoed.
    let mut d = v(connect / 4);
    d.extend_from_slice(b"dgram");
    net.b(b)?.datagram(&d);
    let back = net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.datagrams.iter().any(|x| x.as_ref() == d.as_slice())));
    assert!(back);
    Some(net.events.iter().map(|(i, e)| format!("{i} {e:?}")).collect())
}

#[test]
fn step5_two_endpoints_handshake_admit_raise_and_echo_over_the_in_memory_network() {
    let trace = session_and_echo(7);
    assert!(trace.is_some());
}

#[test]
fn step5_the_same_seed_runs_the_same() {
    let (a, b) = (session_and_echo(42), session_and_echo(42));
    assert!(a.is_some());
    assert_eq!(a, b);
}

#[test]
fn step5_the_pre_session_bound_is_under_64_kib() {
    let cfg = DoorConfig::door();
    let b = cfg.bound(false);
    println!("door: B before a session {b} B, after {} B", cfg.bound(true));
    assert!(b <= 65_536, "{b}");
    assert!(cfg.bound(true) > b);
}

#[test]
fn step5_no_transport_parameter_is_left_at_hyper_quics_default() {
    // hyper-quic's defaults: a connection window of 2^62 - 1, 100 + 100 streams, windows for
    // 12.5 MB/s at 100 ms (1,250,000 B), a datagram buffer of the same, 1 MiB to send.
    let ours = format!("{:?}", DoorConfig::door().transport());
    let default = format!("{:?}", hyper_quic::TransportConfig::default());
    for (field, bad) in [
        ("receive_window", "4611686018427387903"),
        ("max_concurrent_bidi_streams", "100"),
        ("max_concurrent_uni_streams", "100"),
        ("stream_receive_window", "1250000"),
        ("send_window", "10000000"),
        ("datagram_receive_buffer_size", "Some(1250000)"),
        ("crypto_buffer_size", "16384"),
    ] {
        let leaked = format!("{field}: {bad},");
        assert!(default.contains(&leaked), "the default changed: {field} isn't {bad} ({default})");
        assert!(!ours.contains(&leaked), "{field} left at hyper-quic's default {bad}");
    }
}

#[test]
fn step5_a_full_pre_session_cap_closes_the_oldest_never_a_session() {
    let mut cfg = DoorConfig::door();
    cfg.max_pre_session = 3;
    let net = Net::new(cfg, Echo, 5);
    assert!(net.is_some());
    let Some(mut net) = net else { return };
    // Browser 0 gets a session.
    let Some(b0) = net.dial() else { return };
    net.until(10_000, |n| n.browsers.first().is_some_and(|b| b.connected()));
    if let Some(b) = net.b(b0) {
        open_session(b, CHROME, b"/wt");
    }
    assert!(net.until(10_000, |n| n.events.iter().any(|(_, e)| matches!(e, Event::Accepted(_)))));
    // Then N + 1 that never send a CONNECT, one after another.
    for k in 1..=4 {
        net.dial();
        net.until(10_000, |n| n.browsers.get(k).is_some_and(|b| b.connected() || b.closed()));
    }
    net.run_for(std::time::Duration::from_millis(200));
    assert_eq!(net.door.stats.evicted, 1);
    let (open, pre) = net.door.counts();
    assert_eq!((open, pre), (4, 3), "the session and three pre-session connections");
    assert!(net.browsers.get(1).is_some_and(|b| b.closed()), "the oldest pre-session one was closed");
    assert!(net.browsers.first().is_some_and(|b| !b.closed()), "the session stays");
}
