//! Two endpoints on loopback, each enrolled with the simulator CA (CP-T7.3; docs/design/hex-quic.md Q3, Q5).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::LazyLock;

use hex_core::lanes;
use hex_core::patience::Patience;
use hex_core::stream::{CONTROL_PORT, STDIO_PORT};
use hex_testkit::waits::within;
use hyper_quic::rustls;

use super::*;
use crate::enroll::sim::SimCa;

// Each wait over loopback is for its own fact, named for it. No census over QUIC has recorded one yet, so each
// measures, for the fact alone, its answers recorded for the census that will seed it (hex-testkit's rule; Q7: QUIC's
// waits are its own, never another transport's record).
/// A dial's answer: its handshake, taken or refused, and the node's Welcome.
static DIAL: Patience = Patience::measuring("test.hex_quic.dial");
/// A connection a peer opened, taken at the node.
static ACCEPT: Patience = Patience::measuring("test.hex_quic.accept");
/// Bytes the peer wrote, its end, or its refusal of the port, come to the reader.
static READ: Patience = Patience::measuring("test.hex_quic.read");
/// Room for bytes or an end: the peer's grants.
static ROOM: Patience = Patience::measuring("test.hex_quic.room");
/// Room for a stopped reader's bytes: the grants its reader gives only once it reads again.
static HELD_ROOM: Patience = Patience::measuring("test.hex_quic.held_room");

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

fn enrolled(ca: &SimCa, name: &str) -> tls::Credentials {
    ca.admit(name, name.as_bytes());
    enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap()
}

fn node(ca: &SimCa, name: &str) -> Endpoint {
    Endpoint::start(Config {
        credentials: enrolled(ca, name),
        bind: loopback(),
        listen: true,
        serves: vec![CONTROL_PORT, STDIO_PORT],
        shape: Shape {
            connections: 4,
            lanes: 8,
        },
    })
    .unwrap()
}

fn client(credentials: tls::Credentials) -> Endpoint {
    Endpoint::start(Config {
        credentials,
        bind: loopback(),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 8,
        },
    })
    .unwrap()
}

fn read_exactly(conn: &Conn, n: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = vec![0u8; n.max(1)];
    while got.len() < n {
        let read = within(&READ, |by| conn.read(&mut buf, by)).unwrap();
        assert_ne!(read, 0, "ended after {} of {n} bytes", got.len());
        got.extend_from_slice(&buf[..read]);
    }
    got
}

/// HEX-QUIC-10 (CP-T7.3; Ada 2026-11-11, "AESGCM256 bidirectional"): a client enrolled with kiln's CA dials a node
/// enrolled with it, and both ends of the connection report `TLS13_AES_256_GCM_SHA384`, under a hybrid
/// post-quantum group.
#[test]
fn hex_quic_10_both_ends_report_tls13_aes_256_gcm_sha384() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-10.test");
    let client = client(enrolled(&ca, "client-10.test"));
    let peer = within(&DIAL, |by| {
        client.dial("node-10.test", node.local_addr(), by)
    })
    .unwrap();
    let conn = client.open(peer, CONTROL_PORT).unwrap();
    within(&ROOM, |by| conn.write_all(b"ping", by)).unwrap();
    let (from, accepted) = within(&ACCEPT, |by| node.accept(CONTROL_PORT, by)).unwrap();
    assert_eq!(read_exactly(&accepted, 4), b"ping");
    assert_eq!(from, peer_id("client-10.test"));
    for (end, peer) in [(&client, peer), (&node, from)] {
        let (suite, group) = end.negotiated(peer).unwrap().unwrap();
        assert_eq!(suite, rustls::CipherSuite::TLS13_AES_256_GCM_SHA384);
        assert!(
            matches!(
                group,
                Some(
                    rustls::NamedGroup::secp384r1MLKEM1024
                        | rustls::NamedGroup::X25519MLKEM768
                        | rustls::NamedGroup::secp256r1MLKEM768
                )
            ),
            "{group:?}"
        );
    }
}

/// HEX-QUIC-11 (Q3): connections to ports carry bytes both ways in order and end on each half's End; one whose owner
/// doesn't read holds its own lane alone: another connection's bytes keep coming meanwhile, and once the first is
/// read, all of its bytes come, in order, none let go.
#[test]
fn hex_quic_11_a_reader_that_stops_holds_its_own_connection_alone() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-11.test");
    let client = client(enrolled(&ca, "client-11.test"));
    let peer = within(&DIAL, |by| {
        client.dial("node-11.test", node.local_addr(), by)
    })
    .unwrap();
    let slow = client.open(peer, STDIO_PORT).unwrap();
    within(&ROOM, |by| slow.write_all(b"s", by)).unwrap();
    let (_, slow_there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
    assert_eq!(read_exactly(&slow_there, 1), b"s");
    let fast = client.open(peer, STDIO_PORT).unwrap();
    within(&ROOM, |by| fast.write_all(b"f", by)).unwrap();
    let (_, fast_there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
    assert_eq!(read_exactly(&fast_there, 1), b"f");
    // The node writes far past the connection's credit (two frames) to the connection the client doesn't read, on a
    // thread of its own, which waits for the client's grants.
    let total = 64 * lanes::DATA_BOUND;
    let pattern: Vec<u8> = (0..total).map(|i| u8::try_from(i % 251).unwrap()).collect();
    let writer = {
        let expected = pattern.clone();
        std::thread::spawn(move || {
            within(&HELD_ROOM, |by| slow_there.write_all(&expected, by)).unwrap();
            within(&ROOM, |by| slow_there.end(by)).unwrap();
            slow_there
        })
    };
    // Meanwhile the other connection carries a round trip each way.
    for round in 0..8u8 {
        within(&ROOM, |by| fast_there.write_all(&[round], by)).unwrap();
        assert_eq!(read_exactly(&fast, 1), [round]);
        within(&ROOM, |by| fast.write_all(&[round], by)).unwrap();
        assert_eq!(read_exactly(&fast_there, 1), [round]);
    }
    // Now the stopped connection is read: every byte, in order, then its end.
    assert_eq!(read_exactly(&slow, total), pattern);
    let mut end = [0u8; 1];
    assert_eq!(within(&READ, |by| slow.read(&mut end, by)).unwrap(), 0);
    drop(writer.join().unwrap());
    within(&ROOM, |by| slow.end(by)).unwrap();
    within(&ROOM, |by| fast.end(by)).unwrap();
    within(&ROOM, |by| fast_there.end(by)).unwrap();
    assert_eq!(within(&READ, |by| fast.read(&mut end, by)).unwrap(), 0);
}

/// HEX-QUIC-15 (Q3): a port the node doesn't serve is refused to its opener, typed, NoPort.
#[test]
fn hex_quic_15_an_unserved_port_is_refused_by_type() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-12.test");
    let client = client(enrolled(&ca, "client-12.test"));
    let peer = within(&DIAL, |by| {
        client.dial("node-12.test", node.local_addr(), by)
    })
    .unwrap();
    let conn = client.open(peer, lanes::LOG_PORT).unwrap();
    let mut buf = [0u8; 1];
    let err = within(&READ, |by| conn.read(&mut buf, by)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused, "{err}");
    assert_eq!(conn.gone(), Some(Why::NoPort));
}

/// A provider that shares everything with Hex's links but its suites.
fn offering(suites: &[rustls::SupportedCipherSuite]) -> rustls::crypto::CryptoProvider {
    rustls::crypto::CryptoProvider {
        cipher_suites: suites.to_vec(),
        ..hyper_quic::crypto::rustls::node_provider().clone()
    }
}

static CHACHA20: LazyLock<rustls::crypto::CryptoProvider> = LazyLock::new(|| {
    offering(&[rustls::crypto::aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256])
});
static AES_128: LazyLock<rustls::crypto::CryptoProvider> = LazyLock::new(|| {
    offering(&[rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256])
});

/// An endpoint of `credentials` whose TLS offers what `provider` does.
fn under(
    provider: &'static rustls::crypto::CryptoProvider,
    credentials: &tls::Credentials,
    listen: bool,
    serves: Vec<u32>,
) -> Endpoint {
    let shape = Shape {
        connections: 4,
        lanes: 8,
    };
    let limits = limits::limits(shape);
    let derived = limits::derived(shape, &limits);
    let budget = limits::budget(&limits);
    let ep = hyper_transport::Config {
        tls: tls::tls_under(provider, credentials, listen).unwrap(),
        role: classes::Role::Member,
        limits,
        congestion: Default::default(),
    };
    let started = Driver::start(ep, budget, loopback(), derived, serves).unwrap();
    Endpoint {
        commands: started.commands,
        accepts: started.accepts,
        local: started.local,
        thread: Some(started.thread),
    }
}

/// HEX-QUIC-13 (Q5): a peer limited to ChaCha20-Poly1305 or to AES-128-GCM is refused at the handshake either way:
/// a client of Hex's dialing a node limited so, and a client limited so dialing Hex's node.
#[test]
fn hex_quic_13_a_peer_limited_to_another_suite_is_refused_either_way() {
    let ca = SimCa::new().unwrap();
    for (i, other) in [&*CHACHA20, &*AES_128].into_iter().enumerate() {
        let theirs = format!("their-node-{i}.test");
        let limited = under(other, &enrolled(&ca, &theirs), true, vec![CONTROL_PORT]);
        let ours = client(enrolled(&ca, &format!("our-client-{i}.test")));
        let dialed = within(&DIAL, |by| ours.dial(&theirs, limited.local_addr(), by));
        assert!(
            dialed.is_err(),
            "a node limited to {:?} took us",
            other.cipher_suites
        );
        let ours = format!("our-node-{i}.test");
        let node = node(&ca, &ours);
        let theirs = under(
            other,
            &enrolled(&ca, &format!("their-client-{i}.test")),
            false,
            Vec::new(),
        );
        let dialed = within(&DIAL, |by| theirs.dial(&ours, node.local_addr(), by));
        assert!(
            dialed.is_err(),
            "our node took a client limited to {:?}",
            other.cipher_suites
        );
    }
}

/// HEX-QUIC-14 (Q5, Q6): a peer kiln's CA never enrolled is refused at the handshake, typed: a client whose
/// certificate another CA issued is refused Identity; and a node whose certificate names another peer than the one
/// dialed is refused Identity too.
#[test]
fn hex_quic_14_an_unenrolled_peer_is_refused_identity() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-14.test");
    let stranger_ca = SimCa::new().unwrap();
    let mut stranger = enrolled(&stranger_ca, "stranger-14.test");
    // It trusts kiln's CA, so it verifies the node: what fails is the node's verification of it.
    stranger.roots = vec![ca.root()];
    let stranger = client(stranger);
    let refused = within(&DIAL, |by| {
        stranger.dial("node-14.test", node.local_addr(), by)
    })
    .unwrap_err();
    assert_eq!(refused, Refused::Identity, "{refused}");
    let ours = client(enrolled(&ca, "client-14.test"));
    let refused = within(&DIAL, |by| {
        ours.dial("someone-else-14.test", node.local_addr(), by)
    })
    .unwrap_err();
    assert_eq!(refused, Refused::Identity, "{refused}");
}
