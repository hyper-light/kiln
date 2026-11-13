//! The bridge's tests (AC-T7.4.1; docs/design/hex-quic.md Q3): connections to a node on loopback, bridged as the
//! client's link bridges them.

use std::io::{ErrorKind, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr};
use std::time::{Duration, Instant};

use hex_core::lanes::DATA_BOUND;
use hex_core::patience::Patience;
use hex_core::stream::STDIO_PORT;
use hex_testkit::waits::{answered, until, within};
use rustix::event::{PollFd, PollFlags, Timespec};

use super::{Local, socket};
use crate::enroll::{self, sim::SimCa};
use crate::{Config, Conn, Endpoint, Shape};

// Each wait is for its own fact, named for it: no census records any of them yet, so each measures, for the fact
// alone (hex-testkit's rule).
/// A dial's answer: its handshake and the node's Welcome.
static DIAL: Patience = Patience::measuring("test.hex_quic.bridge.dial");
/// A connection the client opened, taken at the node.
static ACCEPT: Patience = Patience::measuring("test.hex_quic.bridge.accept");
/// Room on a connection of the test's own for its next byte: its last frame gone to the driver.
static ROOM: Patience = Patience::measuring("test.hex_quic.bridge.room");
/// The owner's bytes taken from the pair by the bridge's up thread: the owner's end writable again.
static TAKEN: Patience = Patience::measuring("test.hex_quic.bridge.taken");
/// What the owner wrote, then the End its half-close sent, come to the node.
static READ: Patience = Patience::measuring("test.hex_quic.bridge.read");

/// The rounds, and the offsets the half-close is spread over, of Shards' race harness for its M236 (Shards
/// `docs/research/platform-measurements.md` M236 and `docs/research/measurements/half-close/`, at 04abb68d): 50,000
/// rounds over 400 offsets, which left 360 macOS readers asleep through their end; the stdio copy's race test takes
/// them too (hex-client `copy::tests`).
const ROUNDS: u32 = 50_000;
const SPREAD: u32 = 400;

fn loopback() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
}

/// An endpoint the simulator CA enrolls as `name`: a node serving the stdio port, or a client of one node.
fn endpoint(ca: &SimCa, name: &str, listen: bool) -> Endpoint {
    ca.admit(name, name.as_bytes());
    Endpoint::start(Config {
        credentials: enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap(),
        bind: loopback(),
        listen,
        serves: if listen { vec![STDIO_PORT] } else { Vec::new() },
        shape: Shape {
            connections: if listen { 4 } else { 1 },
            lanes: 8,
        },
    })
    .unwrap()
}

/// Whether the owner's end has room to write now (poll(2)'s POLLOUT, not waiting): the up thread has taken what
/// filled the pair.
fn writable(owner: &Local) -> bool {
    let mut ready = [PollFd::new(owner, PollFlags::OUT)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut ready, Some(&now)).is_ok()
        && ready[0].revents().contains(PollFlags::OUT)
}

/// Fills the pair from the owner's end, a byte a write until it takes no more, its send buffer the least its kernel
/// keeps (a byte on macOS, SOCK_MIN_SNDBUF on Linux): the bytes it took.
fn fill(owner: &mut Local) -> usize {
    rustix::net::sockopt::set_socket_send_buffer_size(&*owner, 1).unwrap();
    owner.set_nonblocking(true).unwrap();
    let mut sent = 0;
    loop {
        match owner.write(&[0]) {
            Ok(n) => sent += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return sent,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => panic!("filling the bridged pair: {e}"),
        }
    }
}

/// Reads `conn` to its end: the bytes that came before it.
fn read_to_end(conn: &Conn) -> usize {
    let mut buf = vec![0u8; DATA_BOUND];
    let mut got = 0;
    loop {
        match within(&READ, |by| conn.read(&mut buf, by)).unwrap() {
            0 => return got,
            n => got += n,
        }
    }
}

/// The up thread's step between taking the owner's bytes and reading again, as this process takes it: one byte placed
/// on a connection (`Conn::write_all`: its frame, and the driver's wake), the slowest of [`SPREAD`] on a connection of
/// the test's own to `peer`, each once the last has gone.
fn step(client: &Endpoint, node: &Endpoint, peer: crate::PeerId) -> Duration {
    let ours = client.open(peer, STDIO_PORT).unwrap();
    let mut slowest = Duration::ZERO;
    for _ in 0..SPREAD {
        until(&ROOM, "room for the next byte", || ours.writable()).unwrap();
        let began = Instant::now();
        ours.write_all(&[0], None).unwrap();
        slowest = slowest.max(began.elapsed());
    }
    within(&ROOM, |by| ours.end(by)).unwrap();
    let (_, there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
    assert_eq!(read_to_end(&there), usize::try_from(SPREAD).unwrap());
    slowest
}

/// AC-T7.4.1 (HEX-QUIC-25 B3's hardening, 2026-11-12; Shards M236 and D175): an owner's half-close of its bridged
/// socket reaches the peer as the connection's End, however it lands against the up thread's next read. Do, each
/// round: open a connection to the node and bridge it; fill the pair from the owner's end; as soon as the up thread has
/// taken those bytes (the owner's end writable again), so it is placing them and about to read again, wait the
/// round's share of twice that step's slowest time here (`step`; twice the slowest, as decision 9 bounds a wait), and
/// half-close the owner's end (SHUT_WR), keeping it open. Expect the node to read the bytes and then the End, every
/// round. On macOS a read that waits in recv(2) on a Unix stream socket sleeps through a half-close landing as XNU's
/// `uipc_rcvd` lets go of the reader's lock (M236: 360 of 50,000 readers asleep), so the up thread waits in poll(2),
/// which sees it.
#[test]
fn ac_t7_4_1_an_owners_half_close_reaches_the_peer_through_its_bridge() {
    let ca = SimCa::new().unwrap();
    let node = endpoint(&ca, "node-bridge.test", true);
    let client = endpoint(&ca, "client-bridge.test", false);
    let peer = within(&DIAL, |by| {
        client.dial("node-bridge.test", node.local_addr(), by)
    })
    .unwrap();
    let spread = step(&client, &node, peer) * 2;
    for round in 0..ROUNDS {
        let mut owner = socket(client.open(peer, STDIO_PORT).unwrap()).unwrap();
        let sent = fill(&mut owner);
        // The take is looked for without giving the CPU up, so the half-close follows the up thread's step closely.
        let began = Instant::now();
        while !writable(&owner) {
            std::hint::spin_loop();
        }
        answered(&TAKEN, began.elapsed());
        let at = Instant::now() + spread * (round % SPREAD) / SPREAD;
        while Instant::now() < at {
            std::hint::spin_loop();
        }
        owner.shutdown(Shutdown::Write).unwrap();
        let (_, there) = within(&ACCEPT, |by| node.accept(STDIO_PORT, by)).unwrap();
        assert_eq!(read_to_end(&there), sent, "round {round}");
    }
}
