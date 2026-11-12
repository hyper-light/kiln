//! HEX-QUIC-1..7 (CP-T7.3; docs/design/hex-quic.md Q3): two muxes, each frame one sends handed to the other
//! in order, as hyper-transport's lanes deliver them.

use super::*;

/// Hands every frame `outs` sends to `to`, in order, and returns what `to` made of them; what isn't a frame
/// stays in `rest` for the test to read.
fn deliver(outs: Vec<Out>, to: &mut Mux, rest: &mut Vec<Out>) -> Vec<Out> {
    let mut made = Vec::new();
    for out in outs {
        match out {
            Out::Send {
                lane,
                kind,
                payload,
            } => made.extend(to.frame(lane, kind, payload).unwrap()),
            other => rest.push(other),
        }
    }
    made
}

/// Runs frames between `a` and `b` until neither sends more: each side's events, in order.
fn settle(a: &mut Mux, b: &mut Mux, from_a: Vec<Out>) -> (Vec<Out>, Vec<Out>) {
    let (mut seen_a, mut seen_b) = (Vec::new(), Vec::new());
    let mut to_b = from_a;
    let mut to_a = Vec::new();
    for _ in 0..64 {
        if to_a.is_empty() && to_b.is_empty() {
            return (seen_a, seen_b);
        }
        let made_b = deliver(std::mem::take(&mut to_b), b, &mut seen_a);
        let made_a = deliver(std::mem::take(&mut to_a), a, &mut seen_b);
        to_a = made_b;
        to_b = made_a;
    }
    panic!("the muxes never settled");
}

fn split(outs: Vec<Out>) -> (Vec<Out>, Vec<Out>) {
    outs.into_iter()
        .partition(|out| matches!(out, Out::Send { .. }))
}

const LIMITS: Limits = Limits { lanes: 4 };

fn pair() -> (Mux, Mux) {
    (
        Mux::new(LIMITS, &[]),
        Mux::new(
            LIMITS,
            &[stream::CONTROL_PORT, stream::STDIO_PORT, LOG_PORT],
        ),
    )
}

/// HEX-QUIC-1: a connection opened to a served port carries bytes both ways in order, each half ends on its
/// End, and once both have both sides close it; its lanes then carry the next connection.
#[test]
fn hex_quic_1_a_connection_carries_bytes_both_ways_and_closes_on_both_ends() {
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(stream::CONTROL_PORT).unwrap();
    let mut first = vec![open];
    first.extend(a.send(conn, b"hello").unwrap());
    let (seen_a, seen_b) = settle(&mut a, &mut b, first);
    assert_eq!(seen_a, vec![Out::Opened { conn }]);
    let there = Conn {
        here: false,
        id: conn.id,
    };
    assert_eq!(
        seen_b,
        vec![
            Out::Accepted {
                conn: there,
                port: stream::CONTROL_PORT
            },
            Out::Data {
                conn: there,
                bytes: b"hello".to_vec()
            }
        ]
    );
    let back = b.send(there, b"world").unwrap();
    let (seen_b2, seen_a2) = settle(&mut b, &mut a, back);
    assert!(seen_b2.is_empty());
    assert_eq!(
        seen_a2,
        vec![Out::Data {
            conn,
            bytes: b"world".to_vec()
        }]
    );
    let (sends, events) = split(a.end(conn));
    assert!(events.is_empty(), "a's half ended alone: {events:?}");
    let (seen_a3, seen_b3) = settle(&mut a, &mut b, sends);
    assert!(seen_a3.is_empty());
    assert_eq!(seen_b3, vec![Out::Ended { conn: there }]);
    let (sends, events) = split(b.end(there));
    assert_eq!(events, vec![Out::Closed { conn: there }]);
    let (_, seen_a4) = settle(&mut b, &mut a, sends);
    assert_eq!(seen_a4, vec![Out::Ended { conn }, Out::Closed { conn }]);
    assert_eq!((a.connections(), b.connections()), (0, 0));
    // The lanes are free again: four connections more fit a mux of four lanes.
    for _ in 0..4 {
        let (next, open) = a.connect(stream::STDIO_PORT).unwrap();
        let (seen, _) = settle(&mut a, &mut b, vec![open]);
        assert_eq!(seen, vec![Out::Opened { conn: next }]);
    }
}

/// HEX-QUIC-2: an Open of a port the peer doesn't serve is refused NoPort on the refusals' lane, what was sent
/// after it let go; the opener's lane then carries its next connection.
#[test]
fn hex_quic_2_an_unserved_port_is_refused_by_type_and_its_lane_is_used_again() {
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(files::FILES_PORT).unwrap();
    let mut first = vec![open];
    first.extend(a.send(conn, b"a tree").unwrap());
    let (seen_a, seen_b) = settle(&mut a, &mut b, first);
    assert_eq!(
        seen_a,
        vec![Out::Refused {
            conn,
            why: Why::NoPort
        }]
    );
    assert!(seen_b.is_empty(), "b saw {seen_b:?}");
    assert_eq!((a.connections(), b.connections()), (0, 0));
    let (next, open) = a.connect(stream::CONTROL_PORT).unwrap();
    let (seen, _) = settle(&mut a, &mut b, vec![open]);
    assert_eq!(seen, vec![Out::Opened { conn: next }]);
}

/// HEX-QUIC-3: lanes are a bound on both sides: an Open past the acceptor's lanes is refused Lanes, and a
/// connect past the opener's own is refused before anything is sent.
#[test]
fn hex_quic_3_past_its_lanes_a_connection_is_refused_by_type() {
    let mut a = Mux::new(Limits { lanes: 3 }, &[]);
    let mut b = Mux::new(Limits { lanes: 1 }, &[stream::CONTROL_PORT]);
    let (first, open) = a.connect(stream::CONTROL_PORT).unwrap();
    let (seen, _) = settle(&mut a, &mut b, vec![open]);
    assert_eq!(seen, vec![Out::Opened { conn: first }]);
    let (second, open) = a.connect(stream::CONTROL_PORT).unwrap();
    let (seen, _) = settle(&mut a, &mut b, vec![open]);
    assert_eq!(
        seen,
        vec![Out::Refused {
            conn: second,
            why: Why::Lanes
        }]
    );
    let (_, open) = a.connect(stream::CONTROL_PORT).unwrap();
    drop(open);
    let (_, open) = a.connect(stream::CONTROL_PORT).unwrap();
    drop(open);
    assert_eq!(a.connect(stream::CONTROL_PORT).unwrap_err(), Why::Lanes);
    assert_eq!(a.connect(0x1234).unwrap_err(), Why::NoPort);
}

/// HEX-QUIC-4: a connection its owner resets is gone at both ends: the peer hears the Reset with its why,
/// ends its half, and what it sent meanwhile is let go; every lane is free again on both sides.
#[test]
fn hex_quic_4_a_reset_ends_both_halves_and_frees_every_lane() {
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(stream::STDIO_PORT).unwrap();
    let (_, seen_b) = settle(&mut a, &mut b, vec![open]);
    let there = Conn {
        here: false,
        id: conn.id,
    };
    assert!(matches!(seen_b.as_slice(), [Out::Accepted { .. }]));
    // b sends as a resets: b's bytes, which reach a after its reset, are let go.
    let late = b.send(there, b"late").unwrap();
    let reset = a.reset(conn, Why::Gone);
    let mut stray = Vec::new();
    assert!(deliver(late, &mut a, &mut stray).is_empty());
    assert!(stray.is_empty());
    let (_, seen_b) = settle(&mut a, &mut b, reset);
    assert_eq!(
        seen_b,
        vec![Out::Reset {
            conn: there,
            why: Why::Gone
        }]
    );
    assert_eq!((a.connections(), b.connections()), (0, 0));
    for _ in 0..4 {
        let (next, open) = a.connect(stream::STDIO_PORT).unwrap();
        let (seen, _) = settle(&mut a, &mut b, vec![open]);
        assert_eq!(seen, vec![Out::Opened { conn: next }]);
    }
}

/// HEX-QUIC-5: a frame that breaks the protocol is Broken, naming what came: bytes or an End on a lane that
/// carries nothing, a frame on the refusals' lane, an Open of the wrong size, an Accept of a connection never
/// opened, and bytes of another class than their port's.
#[test]
fn hex_quic_5_a_frame_that_breaks_the_protocol_is_broken() {
    let (mut a, mut b) = pair();
    assert!(b.frame(3, Kind::ControlData, b"x".to_vec()).is_err());
    assert!(b.frame(3, Kind::End, Vec::new()).is_err());
    assert!(b.frame(REFUSALS, Kind::Open, vec![0; 12]).is_err());
    assert!(b.frame(2, Kind::Open, vec![0; 11]).is_err());
    assert!(
        a.frame(2, Kind::Accept, 9u64.to_be_bytes().to_vec())
            .is_err()
    );
    assert!(a.frame(2, Kind::Refuse, vec![0; 9]).is_err());
    let (_, open) = a.connect(stream::STDIO_PORT).unwrap();
    let Out::Send { lane, .. } = &open else {
        panic!("{open:?}");
    };
    let lane = *lane;
    settle(&mut a, &mut b, vec![open]);
    let why = b.frame(lane, Kind::ControlData, b"jump the queue".to_vec());
    assert!(why.is_err(), "stdio bytes as control bytes: {why:?}");
}

/// HEX-QUIC-6: when the connection to the peer closes, every connection on it is reset Closed, and the lanes
/// start over for the next connection.
#[test]
fn hex_quic_6_a_closed_connection_resets_every_connection_on_it() {
    let (mut a, mut b) = pair();
    let (one, open1) = a.connect(stream::CONTROL_PORT).unwrap();
    let (two, open2) = a.connect(stream::STDIO_PORT).unwrap();
    settle(&mut a, &mut b, vec![open1, open2]);
    let mut gone = a.closed();
    gone.sort_by_key(|out| match out {
        Out::Reset { conn, .. } => conn.id,
        _ => u64::MAX,
    });
    assert_eq!(
        gone,
        vec![
            Out::Reset {
                conn: one,
                why: Why::Closed
            },
            Out::Reset {
                conn: two,
                why: Why::Closed
            }
        ]
    );
    assert_eq!(b.closed().len(), 2);
    let (next, open) = a.connect(stream::CONTROL_PORT).unwrap();
    let (seen, _) = settle(&mut a, &mut b, vec![open]);
    assert_eq!(seen, vec![Out::Opened { conn: next }]);
}

/// HEX-QUIC-7: every kind's code names it again, each class's rank is below the class count, and a data
/// frame is at most a whole framed wire message, so sends past it go in frames of the bound.
#[test]
fn hex_quic_7_kinds_classes_and_the_data_bound_hold() {
    for kind in Kind::ALL {
        assert_eq!(Kind::of(kind.code()), Some(kind));
        assert!(kind.class().rank() < Class::RANKS);
    }
    assert_eq!(Kind::of(0), None);
    assert_eq!(Kind::of(11), None);
    // A Welcome travels on the refusals' lane alone, empty.
    let (mut a, b) = pair();
    let Out::Send {
        lane,
        kind,
        payload,
    } = b.welcome()
    else {
        panic!("a welcome is a frame");
    };
    assert_eq!((lane, kind), (REFUSALS, Kind::Welcome));
    assert_eq!(a.frame(lane, kind, payload).unwrap(), vec![Out::Welcomed]);
    assert!(a.frame(1, Kind::Welcome, Vec::new()).is_err());
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(stream::STDIO_PORT).unwrap();
    settle(&mut a, &mut b, vec![open]);
    let big = vec![7u8; DATA_BOUND + 1];
    let frames = a.send(conn, &big).unwrap();
    let sizes: Vec<usize> = frames
        .iter()
        .map(|out| match out {
            Out::Send { payload, .. } => payload.len(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(sizes, vec![DATA_BOUND, 1]);
}

/// HEX-QUIC-18 (Q3): a connection's bytes go within the credit its peer grants: its first INITIAL_CREDIT, then what
/// the peer's owner has read; one byte more is refused Credit here, and a peer that sends past what it was granted
/// is Broken; a grant reaches the sender as Credit, and its room grows by it.
#[test]
fn hex_quic_18_bytes_go_within_the_credit_the_peer_grants() {
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(stream::STDIO_PORT).unwrap();
    settle(&mut a, &mut b, vec![open]);
    let there = Conn {
        here: false,
        id: conn.id,
    };
    assert_eq!(a.room(conn), INITIAL_CREDIT);
    let all = vec![1u8; usize::try_from(INITIAL_CREDIT).unwrap()];
    let frames = a.send(conn, &all).unwrap();
    assert_eq!(a.room(conn), 0);
    assert_eq!(a.send(conn, &[1]).unwrap_err(), Why::Credit);
    let mut stray = Vec::new();
    let delivered = deliver(frames, &mut b, &mut stray);
    assert!(stray.is_empty());
    assert_eq!(delivered.len(), 2, "two frames of data");
    // b's owner reads one frame: its grant goes back on b's lane and a's room grows by it.
    let grant = b.grant(there, u64::try_from(DATA_BOUND).unwrap()).unwrap();
    let mut events = Vec::new();
    let made = deliver(vec![grant], &mut a, &mut events);
    assert_eq!(made, vec![Out::Credit { conn }]);
    assert_eq!(a.room(conn), u64::try_from(DATA_BOUND).unwrap());
    // A peer that sends past its grant breaks the protocol: b has granted INITIAL_CREDIT + one frame.
    let Some(lane) = a.our_lane(conn) else {
        panic!("a's half is open");
    };
    let past = vec![2u8; DATA_BOUND];
    assert!(b.frame(lane, Kind::StreamData, past.clone()).is_ok());
    assert!(b.frame(lane, Kind::StreamData, vec![3u8]).is_err());
}

/// HEX-QUIC-19 (Q3): a Reset that crosses the other side's close is let go, as is a Credit sent after its sender's
/// End: the side whose two halves ended frees both lanes, and the frame that comes after names nothing it holds.
#[test]
fn hex_quic_19_a_reset_or_credit_that_crosses_a_close_is_let_go() {
    let (mut a, mut b) = pair();
    let (conn, open) = a.connect(stream::STDIO_PORT).unwrap();
    settle(&mut a, &mut b, vec![open]);
    let there = Conn {
        here: false,
        id: conn.id,
    };
    // b ends its half; a hears it, then ends its own and closes: b's later frames cross that close.
    let (b_end, _) = split(b.end(there));
    let mut seen = Vec::new();
    assert_eq!(deliver(b_end, &mut a, &mut seen), vec![Out::Ended { conn }]);
    let (a_end, closed) = split(a.end(conn));
    assert_eq!(closed, vec![Out::Closed { conn }]);
    // b, not yet hearing a's End, grants a frame after its own End, then resets.
    let credit = b.grant(there, 7).unwrap();
    let reset = b.reset(there, Why::Gone);
    let mut stray = Vec::new();
    assert!(deliver(vec![credit], &mut a, &mut stray).is_empty());
    assert!(deliver(reset, &mut a, &mut stray).is_empty());
    assert!(stray.is_empty());
    // a's End reaches b, whose reset let a's half go: it is let go too, and both sides hold nothing.
    let mut stray_b = Vec::new();
    assert!(deliver(a_end, &mut b, &mut stray_b).is_empty());
    assert_eq!((a.connections(), b.connections()), (0, 0));
}
