//! Two endpoints on loopback, each enrolled with the simulator CA (CP-T7.3; docs/design/hex-quic.md Q3, Q5).

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::LazyLock;

use hex_core::lanes;
use hex_core::patience::Patience;
use hex_core::stream::{CONTROL_PORT, STDIO_PORT};
use hex_testkit::waits::{until, within};
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
/// A peer's Reset that follows its End, come to the reader.
static RESET_AFTER_END: Patience = Patience::measuring("test.hex_quic.reset_after_end");
/// A peer's reset, come to a connection whose owner hasn't read.
static RESET: Patience = Patience::measuring("test.hex_quic.reset");

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

/// HEX-QUIC-26 (Q3; CLAUDE.md §3): a connection whose peer wrote, ended its half and then let it go (a `Conn` dropped
/// while its peer's half is open is reset) is read to its end all the same, its reset come before its owner read any of
/// it: a program's stdout, which the node's copier ends and drops once its pipe ends. Do: the node writes to a stdio
/// connection, ends it and drops it; once its reset has come, the client reads. Expect: every byte the node wrote,
/// then the connection's end; only a write to the node, which let it go, refused as reset.
#[test]
fn hex_quic_26_a_connection_its_peer_ended_and_let_go_is_read_to_its_end() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-26.test");
    let client = client(enrolled(&ca, "client-26.test"));
    let peer = within(&DIAL, |by| {
        client.dial("node-26.test", node.local_addr(), by)
    })
    .unwrap();
    let conn = client.open(peer, STDIO_PORT).unwrap();
    within(&ROOM, |by| conn.write_all(b"n", by)).unwrap();
    let (_, there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
    assert_eq!(read_exactly(&there, 1), b"n");
    let out = b"an owned box\n";
    within(&ROOM, |by| there.write_all(out, by)).unwrap();
    within(&ROOM, |by| there.end(by)).unwrap();
    // The client's half is open: the drop resets the connection after its End.
    drop(there);
    until(&RESET, "the node's reset", || conn.gone().is_some()).unwrap();
    assert!(!conn.hung_up(), "the node's bytes are still to read");
    assert_eq!(read_exactly(&conn, out.len()), out);
    let mut end = [0u8; 1];
    assert_eq!(within(&READ, |by| conn.read(&mut end, by)).unwrap(), 0);
    assert!(conn.hung_up());
    let refused = conn.write_all(b"x", None).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionReset);
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

/// What `owner` holds now, read without waiting and kept in `got`: whether its end has come.
fn drained(owner: &mut bridge::Local, got: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 64];
    loop {
        match std::io::Read::read(owner, &mut buf) {
            Ok(0) => return true,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("reading a bridged socket: {e}"),
        }
    }
}

/// AC-T7.4.1 (joint plan step 4, Q3; HEX-QUIC-23's lost stdout, 2026-11-12): what a peer wrote before its End reaches
/// the reader, whatever ends the connection after it. Do: as the node relays a program's stdout (`hex-node`
/// `quic::stream_conn`), the node writes a line on a stdio connection, ends its half and lets the connection go with
/// the client's half still open, so its Reset follows its End; the client bridges the connection (`link::Link`'s
/// dial) only once that Reset has come, as a bridge whose reader the scheduler ran late reads it; then again with the
/// client's endpoint ended before its bridge reads. Expect: each bridged socket reads the line, then its end.
#[test]
fn ac_t7_4_1_what_a_peer_wrote_before_its_end_outlasts_its_reset() {
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-ended.test");
    for endpoint_ended in [false, true] {
        let client = client(enrolled(
            &ca,
            &format!("client-ended-{endpoint_ended}.test"),
        ));
        let peer = within(&DIAL, |by| {
            client.dial("node-ended.test", node.local_addr(), by)
        })
        .unwrap();
        let conn = client.open(peer, STDIO_PORT).unwrap();
        let (_, there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        within(&ROOM, |by| there.write_all(b"line\n", by)).unwrap();
        within(&ROOM, |by| there.end(by)).unwrap();
        drop(there);
        until(&RESET_AFTER_END, "the node's Reset after its End", || {
            conn.gone() == Some(Why::Gone)
        })
        .unwrap();
        if endpoint_ended {
            drop(client);
        }
        let mut owner = bridge::socket(conn).unwrap();
        owner.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        until(&READ, "the bridged line and its end", || {
            drained(&mut owner, &mut got)
        })
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "line\n",
            "the client's endpoint ended before its bridge read: {endpoint_ended}"
        );
    }
}

/// How a bridged stream's owner read its end: the peer's End, or the connection cut short, as the read's error says.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    Ended,
    Cut(std::io::ErrorKind),
}

/// What `owner` holds now, read without waiting and kept in `got`: its end, once it has come.
fn ending(owner: &mut hex_client::guest::DuplexOwner, got: &mut Vec<u8>) -> Option<Ending> {
    let mut buf = [0u8; 64];
    loop {
        match std::io::Read::read(&mut **owner, &mut buf) {
            Ok(0) => return Some(Ending::Ended),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Some(Ending::Cut(e.kind())),
        }
    }
}

/// AC-T7.4.1 (joint plan step 4, Q3; hex-quic.md Q11, route (b)): a stream cut short reads as cut, never as ended. Do:
/// the client's link bridges a stdio connection (`link::Link`'s dial); the node writes a line on it and, once the line
/// has gone, resets the connection with no End; then again with the node ending its half instead. Expect the cut
/// stream to read the line, then a reset (`ConnectionReset`), and the ended one the line, then its end.
#[test]
fn ac_t7_4_1_a_stream_cut_short_reads_as_cut() {
    use hex_client::guest::Link as _;
    let ca = SimCa::new().unwrap();
    let node = node(&ca, "node-cut.test");
    let client = std::sync::Arc::new(client(enrolled(&ca, "client-cut.test")));
    let peer = within(&DIAL, |by| {
        client.dial("node-cut.test", node.local_addr(), by)
    })
    .unwrap();
    let link = link::Link::new(std::sync::Arc::clone(&client), peer);
    for (cut, expected) in [
        (true, Ending::Cut(std::io::ErrorKind::ConnectionReset)),
        (false, Ending::Ended),
    ] {
        let mut owner = link.dial(STDIO_PORT).unwrap();
        owner.set_nonblocking(true).unwrap();
        let (_, there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        within(&ROOM, |by| there.write_all(b"line\n", by)).unwrap();
        if cut {
            // Its frame gone to the driver, ahead of the Reset on the connection's lane.
            until(&ROOM, "the line gone to the driver", || there.writable()).unwrap();
            there.reset(Why::Gone);
        } else {
            within(&ROOM, |by| there.end(by)).unwrap();
        }
        let (mut got, mut end) = (Vec::new(), None);
        until(&READ, "the bridged line and its end", || {
            end = ending(&mut owner, &mut got);
            end.is_some()
        })
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&got), "line\n", "cut: {cut}");
        assert_eq!(end, Some(expected), "cut: {cut}");
    }
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

/// The link falling silent (AC-T7.4.1; HEX-QUIC-25 B3, 2026-11-12): a relay in front of the node, and the bridges of a
/// connection whose link ends with nothing said.
#[cfg(unix)]
mod silent {
    use std::io::ErrorKind;
    use std::net::{SocketAddr, UdpSocket};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use hex_client::guest::{DuplexOwner, Link as _};
    use hex_core::patience::Patience;
    use hex_core::stream::STDIO_PORT;
    use hex_testkit::waits::{until, within};
    use rustix::event::{PollFd, PollFlags, Timespec};

    use super::{ACCEPT, DIAL, READ, ROOM, client, enrolled, loopback, node, read_exactly};
    use crate::enroll::sim::SimCa;
    use crate::link;

    /// The client's connection ending once its link has fallen silent: QUIC's idle timeout (Q8), nothing said.
    static IDLE_END: Patience = Patience::measuring("test.hex_quic.idle_end");
    /// A bridge letting its owner's socket go once its connection has ended.
    static LET_GO: Patience = Patience::measuring("test.hex_quic.bridge_let_go");

    /// A UDP relay in front of a node: it passes datagrams both ways until it falls silent, then takes every datagram
    /// and passes none, its sockets still bound, so the link ends with nothing said, no close and no ICMP, as with a
    /// node that stops answering or a successor bound to its port (HEX-QUIC-25 B3).
    struct Relay {
        addr: SocketAddr,
        silent: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Relay {
        fn start(node: SocketAddr) -> Self {
            let front = UdpSocket::bind(loopback()).unwrap();
            let back = UdpSocket::bind(loopback()).unwrap();
            back.connect(node).unwrap();
            front.set_nonblocking(true).unwrap();
            back.set_nonblocking(true).unwrap();
            let addr = front.local_addr().unwrap();
            let silent = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (quiet, done) = (Arc::clone(&silent), Arc::clone(&stop));
            let thread = std::thread::spawn(move || {
                // A UDP datagram's most: its length is 16 bits (RFC 768).
                let mut buf = vec![0u8; usize::from(u16::MAX)];
                let mut client = None;
                while !done.load(Ordering::Acquire) {
                    let mut ready = [
                        PollFd::new(&front, PollFlags::IN),
                        PollFd::new(&back, PollFlags::IN),
                    ];
                    let _ = rustix::event::poll(&mut ready, None);
                    let pass = !quiet.load(Ordering::Acquire);
                    while let Ok((n, from)) = front.recv_from(&mut buf) {
                        client = Some(from);
                        if pass {
                            let _ = back.send(&buf[..n]);
                        }
                    }
                    while let Ok(n) = back.recv(&mut buf) {
                        if pass && let Some(to) = client {
                            let _ = front.send_to(&buf[..n], to);
                        }
                    }
                }
            });
            Self {
                addr,
                silent,
                stop,
                thread: Some(thread),
            }
        }

        /// From now on the relay passes nothing.
        fn silence(&self) {
            self.silent.store(true, Ordering::Release);
        }
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Its wait ends with a datagram.
            if let Ok(waker) = UdpSocket::bind(loopback()) {
                let _ = waker.send_to(&[0], self.addr);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// What `owner` holds now, read without waiting (its socket non-blocking) and kept in `got`: whether its end has
    /// come (an end of input, a reset, or the cut of its connection's close, Q11).
    fn drain(owner: &mut DuplexOwner, got: &mut Vec<u8>) -> bool {
        let mut buf = [0u8; 64];
        loop {
            match owner.read(&mut buf) {
                Ok(0) => return true,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                    ) =>
                {
                    return true;
                }
                Err(e) => panic!("reading a bridged socket: {e}"),
            }
        }
    }

    /// Whether the bridge has let `owner`'s socket go: the pair's other end is closed, so it can't be written to
    /// (poll(2)'s POLLHUP for writing: on Linux a socket shut both ways, on macOS one that can send no more).
    fn let_go(owner: &DuplexOwner) -> bool {
        let mut ready = [PollFd::from_borrowed_fd(owner.handle(), PollFlags::OUT)];
        let now = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        rustix::event::poll(&mut ready, Some(&now)).is_ok()
            && ready[0]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR)
    }

    /// AC-T7.4.1 (HEX-QUIC-25 B3, 2026-11-12; Q3): a connection's end ends every bridge on it, however each stood. Do:
    /// over a relay, bridge three of a program's stdio connections as the client dials them (`link::Link`): one the node
    /// wrote a line on and then ended, as a program's stdout at its program's end, read to its end and written on after
    /// it, its owner's half still open; one open both ways, as a stdin its program reads; then the relay falls silent,
    /// taking every datagram and passing none, and a third is dialed as the link goes, its Open lost. Expect: the
    /// client's connection ends at QUIC's idle timeout, with no close and no ICMP, and every bridge then lets its
    /// owner's socket go: its read ends, and a write is refused, none taken.
    #[test]
    fn ac_t7_4_1_a_connections_end_lets_every_bridged_socket_go() {
        let ca = SimCa::new().unwrap();
        let node = node(&ca, "node-silent.test");
        let relay = Relay::start(node.local_addr());
        let client = Arc::new(client(enrolled(&ca, "client-silent.test")));
        let peer = within(&DIAL, |by| client.dial("node-silent.test", relay.addr, by)).unwrap();
        let link = link::Link::new(Arc::clone(&client), peer);
        // A program's stdout at its program's end: a line, then the node's End; its owner's half stays open.
        let mut ended = link.dial(STDIO_PORT).unwrap();
        ended.set_nonblocking(true).unwrap();
        let (_, ended_there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        within(&ROOM, |by| ended_there.write_all(b"line\n", by)).unwrap();
        within(&ROOM, |by| ended_there.end(by)).unwrap();
        let mut got = Vec::new();
        until(&READ, "the ended stream's line and end", || {
            drain(&mut ended, &mut got)
        })
        .unwrap();
        assert_eq!(got, b"line\n");
        ended.write_all(b"after\n").unwrap();
        assert_eq!(read_exactly(&ended_there, 6), b"after\n");
        // A program's stdin, open both ways.
        let mut open = link.dial(STDIO_PORT).unwrap();
        open.write_all(b"in\n").unwrap();
        open.set_nonblocking(true).unwrap();
        let (_, open_there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        assert_eq!(read_exactly(&open_there, 3), b"in\n");
        // The connection's end comes to the test on a connection of its own on the link.
        let sentinel = client.open(peer, STDIO_PORT).unwrap();
        within(&ROOM, |by| sentinel.write_all(b"s", by)).unwrap();
        let (_, sentinel_there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        assert_eq!(read_exactly(&sentinel_there, 1), b"s");
        relay.silence();
        // Dialed as the link goes: its Open never reaches the node.
        let mut opening = link.dial(STDIO_PORT).unwrap();
        opening.set_nonblocking(true).unwrap();
        let mut byte = [0u8; 1];
        let end = within(&IDLE_END, |by| sentinel.read(&mut byte, by)).unwrap_err();
        assert_eq!(end.kind(), ErrorKind::ConnectionAborted, "{end}");
        for (what, owner) in [
            ("open", &mut open),
            ("opening", &mut opening),
            ("ended", &mut ended),
        ] {
            eprintln!(
                "the connection ended: waiting for the {what} stream's bridge to let its socket go"
            );
            until(
                &LET_GO,
                &format!("the {what} stream's socket let go"),
                || let_go(owner),
            )
            .unwrap();
            let mut rest = Vec::new();
            assert!(
                drain(owner, &mut rest),
                "the {what} stream's read didn't end"
            );
            assert!(
                owner.write(b"x").is_err(),
                "the {what} stream took a write after its connection ended"
            );
        }
        drop((ended_there, open_there, sentinel_there));
    }
}
