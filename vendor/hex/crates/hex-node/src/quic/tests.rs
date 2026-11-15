//! The node's QUIC listener in this process (CP-T7.3; docs/design/hex-quic.md Q4).

use super::*;

use std::net::{Ipv4Addr, SocketAddr};

use hex_api::sim::SimRuntime;
use hex_api::{BoxId, BoxInfo, BoxSpec, Class, Proc, Program, Runtime, Signal, Usage};
use hex_core::netns;
use hex_core::patience::Patience;
use hex_quic::enroll::{self, sim::SimCa};
use hex_quic::{CipherSuite, Config, NamedGroup, Shape};
use hex_testkit::waits::follow::{self, Party};

use crate::serve::{Host, Native, Server};

/// The log's reader registered by the node: no recorded operation makes it, so measuring, for the fact alone.
static READER: Patience = Patience::measuring("test.hex_node.quic.log_reader");
/// A line the stopped reader missed: made by the reader's lane filling, which no record times.
static MISSED: Patience = Patience::measuring("test.hex_node.quic.log_missed");
/// The reader's lane taking the node's lines again once its reader has read what waited: no record times it.
static LOG_ROOM: Patience = Patience::measuring("test.hex_node.quic.log_room");
// Each wait over loopback below is for its own fact, named for it. No census over QUIC has recorded one yet, so each
// measures, for the fact alone, its answers recorded for the census that will seed it (hex-testkit's rule).
/// A dial's answer: its handshake, taken or refused, and the node's Welcome.
static DIAL: Patience = Patience::measuring("test.hex_node.quic.dial");
/// A connection a peer opened, taken at the node.
static ACCEPT: Patience = Patience::measuring("test.hex_node.quic.accept");
/// Bytes the other end wrote, its end, or the node's refusal of a port, come to the reader.
static READ: Patience = Patience::measuring("test.hex_node.quic.read");
/// Room for bytes or an end: the other end's grants.
static ROOM: Patience = Patience::measuring("test.hex_node.quic.room");
/// A program's stdin connection ended by the node: once its program let its stdin go, or after its peer's end.
static STDIN_END: Patience = Patience::measuring("test.hex_node.quic.stdin_end");

// Every wait follows the test's own subject (the review's C-5): a call on a connection, by the connection's life, its
// end ending the call too; a call on an endpoint, by its driver's.

/// A connection as the party of a wait on it: ended once it's gone.
fn conn_party(conn: &hex_quic::Conn) -> Party {
    if conn.gone().is_some() {
        Party::Ended
    } else {
        Party::Live
    }
}

/// An endpoint as the party of a wait on it: ended once its driver has.
fn driver_party(end: &Endpoint) -> Party {
    match end.links() {
        Err(hex_quic::Refused::Ended) => Party::Ended,
        _ => Party::Live,
    }
}

/// A connection's call's answer, as a slice of [`follow::within`] takes it: its own deadline passing is no answer yet.
fn io_slice<T>(got: std::io::Result<T>) -> Option<std::io::Result<T>> {
    match got {
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
        got => Some(got),
    }
}

/// What `conn` reads into `buf`, as `wait` waits, by the connection's life.
fn read(wait: &Patience, conn: &hex_quic::Conn, buf: &mut [u8]) -> std::io::Result<usize> {
    follow::within(
        wait,
        "bytes or an end come to the reader",
        || conn_party(conn),
        |by| io_slice(conn.read(buf, by)),
    )
    .unwrap()
}

/// Writes `bytes` on `conn` a frame at a time, as `wait` waits for room, by the connection's life: a frame's wait cut at
/// a slice is made again for that frame alone, as its timeout comes only before it is placed, so none goes twice.
fn write_all(wait: &Patience, conn: &hex_quic::Conn, bytes: &[u8]) -> std::io::Result<()> {
    for frame in bytes.chunks(hex_core::lanes::DATA_BOUND) {
        follow::within(
            wait,
            "room for a frame",
            || conn_party(conn),
            |by| io_slice(conn.write_all(frame, by)),
        )
        .unwrap()?;
    }
    Ok(())
}

/// Ends `conn`'s half, waiting for room as `wait` waits, by the connection's life.
fn end_half(wait: &Patience, conn: &hex_quic::Conn) -> std::io::Result<()> {
    follow::within(
        wait,
        "room for the end",
        || conn_party(conn),
        |by| io_slice(conn.end(by)),
    )
    .unwrap()
}

/// The next connection to `port` a peer opened at `node`, as `wait` waits, by the node's driver's life.
fn accept(
    wait: &Patience,
    node: &Endpoint,
    port: u32,
) -> Result<(hex_quic::PeerId, hex_quic::Conn), hex_quic::Refused> {
    follow::within(
        wait,
        "a connection a peer opened",
        || driver_party(node),
        |by| match node.accept(port, by) {
            Err(hex_quic::Refused::Deadline { .. }) => None,
            got => Some(got),
        },
    )
    .unwrap()
}

/// `from`'s dial of `name` at `addr`, as `wait` waits: one that can't be cut, whose cut would abandon its handshake and
/// dial again, so given no deadline, ended by its answer or its endpoint's end.
fn dial(
    wait: &Patience,
    from: &Endpoint,
    name: &str,
    addr: SocketAddr,
) -> Result<hex_quic::PeerId, hex_quic::Refused> {
    follow::whole(wait, "a dial's answer", || from.dial(name, addr, None))
}

fn limits() -> Limits {
    Limits {
        connections: 4,
        message_bytes: wire::MAX_MESSAGE,
        in_flight: 4,
        programs: 4,
    }
}

/// A folder of the test's own: under ~/.cache on Unix (the hazards' rule: never /tmp), the system's temp folder on
/// Windows, as Hex's Windows tests take.
fn scratch(prefix: &str) -> tempfile::TempDir {
    #[cfg(unix)]
    {
        let cache =
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/quic");
        std::fs::create_dir_all(&cache).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&cache)
            .unwrap()
    }
    #[cfg(windows)]
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

fn enrolled(ca: &SimCa, name: &str) -> hex_quic::tls::Credentials {
    ca.admit(name, name.as_bytes());
    enroll::enroll(ca, name, name.as_bytes().to_vec()).unwrap()
}

fn read_line(conn: &hex_quic::Conn, pending: &mut Vec<u8>) -> String {
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        if let Some(at) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            return String::from_utf8(line).unwrap();
        }
        let n = read(&READ, conn, &mut buf).unwrap();
        assert_ne!(n, 0, "the log ended");
        pending.extend_from_slice(&buf[..n]);
    }
}

/// HEX-QUIC-16 (Q4): a connection to the node's log port is sent each line the node says, in order; one whose reader
/// stops misses the lines its lane can't take, counted, never holding the node, and once it reads again it is told
/// how many it missed before the next line.
#[test]
fn hex_quic_16_the_log_lane_sends_lines_and_tells_a_slow_reader_what_it_missed() {
    let ca = SimCa::new().unwrap();
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "node.log.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: ports(false),
            shape: Shape {
                connections: limits.connections,
                lanes: 8,
            },
        })
        .unwrap(),
    );
    let dir = scratch("l-");
    let quic = Quic::new(
        Native::bind(&dir.path().join("node.sock")).unwrap(),
        Arc::clone(&node),
        Bounds::of(&limits),
        None,
    )
    .unwrap();
    let log = quic.log();
    let client = Endpoint::start(Config {
        credentials: enrolled(&ca, "client.log.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 8,
        },
    })
    .unwrap();
    let peer = dial(&DIAL, &client, "node.log.test", node.local_addr()).unwrap();
    let reader = client.open(peer, hex_core::lanes::LOG_PORT).unwrap();
    // A look: the node's log tells no event as it takes a reader, its count read.
    follow::until(
        &READER,
        "the log's reader",
        || driver_party(&node),
        || log.readers() == 1,
    )
    .unwrap();
    let mut pending = Vec::new();
    log.say("hex.node event=first\n");
    assert_eq!(read_line(&reader, &mut pending), "hex.node event=first\n");
    // The reader stops: lines of a frame each fill its lane and QUIC's window behind it, and the node goes on.
    let big = format!("{}\n", "x".repeat(hex_core::lanes::DATA_BOUND - 1));
    let mut said = 0u64;
    // A look that drives its fact: each says a line, until one is missed; a say tells no event, its count read.
    follow::until(
        &MISSED,
        "a missed line",
        || conn_party(&reader),
        || {
            log.say(&big);
            said += 1;
            log.missed() > 0
        },
    )
    .unwrap();
    let missed = log.missed();
    // Reading again: every line that went, then the count of those missed, then the next line.
    let mut lines = 0u64;
    loop {
        let line = read_line(&reader, &mut pending);
        if line == big {
            lines += 1;
            if lines + missed == said {
                break;
            }
            continue;
        }
        panic!("a line out of order: {} bytes", line.len());
    }
    // A look that drives its fact, as the missed line's: each says a line, until the lane takes one.
    follow::until(
        &LOG_ROOM,
        "the reader's room",
        || conn_party(&reader),
        || {
            log.say("hex.node event=after\n");
            log.missed() == 0
        },
    )
    .unwrap();
    // The count comes before the next line that went (more than once where a later line missed too), then that line.
    let mut counted = 0u64;
    loop {
        let line = read_line(&reader, &mut pending);
        if line == "hex.node event=after\n" {
            break;
        }
        let n = line
            .strip_prefix("hex.log event=missed lines=")
            .and_then(|n| n.trim_end().parse::<u64>().ok());
        let Some(n) = n else {
            panic!("a line out of order: {line:?}");
        };
        counted += n;
    }
    assert!(counted >= missed, "told {counted} of {missed} missed");
}

/// HEX-QUIC-17 (Q4, Linux): a file tree staged over QUIC lands under the node's staged folder as the guest's relay
/// writes it, through the relay's own staging server; unstaged, it is gone.
#[cfg(target_os = "linux")]
#[test]
fn hex_quic_17_a_tree_staged_over_quic_lands_and_unstages() {
    use hex_client::guest::stage::{self, Item, Source};
    let ca = SimCa::new().unwrap();
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "node.stage.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: ports(true),
            shape: Shape {
                connections: limits.connections,
                lanes: 8,
            },
        })
        .unwrap(),
    );
    let dir = scratch("s-");
    let staged = dir.path().join("staged");
    std::fs::create_dir_all(&staged).unwrap();
    let _quic = Quic::new(
        Native::bind(&dir.path().join("node.sock")).unwrap(),
        Arc::clone(&node),
        Bounds::of(&limits),
        Some(staged.clone()),
    )
    .unwrap();
    let client = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "client.stage.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: 8,
            },
        })
        .unwrap(),
    );
    let peer = dial(&DIAL, &client, "node.stage.test", node.local_addr()).unwrap();
    let link = hex_quic::link::Link::new(Arc::clone(&client), peer);
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("etc")).unwrap();
    std::fs::write(tree.join("etc/hostname"), b"quic\n").unwrap();
    let id = hex_api::BoxId::new("staged-over-quic").unwrap();
    stage::stage(
        &link,
        &id,
        &[
            Item {
                name: "config.json".to_owned(),
                source: Source::Bytes(b"{}".to_vec()),
            },
            Item {
                name: "rootfs".to_owned(),
                source: Source::Path(tree),
            },
        ],
        &hex_core::stream::files::LIMITS,
    )
    .unwrap();
    let copy = staged.join(id.as_str());
    assert_eq!(std::fs::read(copy.join("config.json")).unwrap(), b"{}");
    assert_eq!(
        std::fs::read(copy.join("rootfs/etc/hostname")).unwrap(),
        b"quic\n"
    );
    stage::unstage(&link, &id).unwrap();
    assert!(!copy.exists(), "the staged copy is left");
}

/// HEX-QUIC-21 (Q4; HEX-GUEST-41's rule, as the guest's relay keeps it): a program's stdin connection ends once its
/// program lets its stdin go, on Unix with no byte more from the peer (Windows' pipes can't be waited on: there, at
/// the peer's next write), so the peer's client lets its caller's pipe go; and the peer's end of its stdin ends the
/// program's pipe after every byte, then the connection, both halves.
#[test]
fn hex_quic_21_a_programs_stdin_connection_ends_with_its_program_or_its_peer() {
    let ca = SimCa::new().unwrap();
    let node = Endpoint::start(Config {
        credentials: enrolled(&ca, "node.stdin.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: true,
        serves: vec![stream::STDIO_PORT],
        shape: Shape {
            connections: 1,
            lanes: 4,
        },
    })
    .unwrap();
    let client = Endpoint::start(Config {
        credentials: enrolled(&ca, "client.stdin.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 4,
        },
    })
    .unwrap();
    let peer = dial(&DIAL, &client, "node.stdin.test", node.local_addr()).unwrap();
    let mut buf = [0u8; 64];

    // The program lets its stdin go.
    let theirs = client.open(peer, stream::STDIO_PORT).unwrap();
    let (_, ours) = accept(&ACCEPT, &node, stream::STDIO_PORT).unwrap();
    let (r, w) = std::io::pipe().unwrap();
    let copier = std::thread::spawn(move || feed(&ours, w));
    drop(r);
    #[cfg(windows)]
    write_all(&ROOM, &theirs, b"after its end\n").unwrap();
    let ended = read(&STDIN_END, &theirs, &mut buf);
    assert!(
        matches!(&ended, Ok(0))
            || ended
                .as_ref()
                .is_err_and(|e| e.kind() != std::io::ErrorKind::TimedOut),
        "the stdin connection outlived its program's stdin: {ended:?}"
    );
    copier.join().unwrap();

    // The peer ends its stdin.
    let theirs = client.open(peer, stream::STDIO_PORT).unwrap();
    let (_, ours) = accept(&ACCEPT, &node, stream::STDIO_PORT).unwrap();
    let (mut r, w) = std::io::pipe().unwrap();
    let copier = std::thread::spawn(move || feed(&ours, w));
    write_all(&ROOM, &theirs, b"round trip\n").unwrap();
    end_half(&ROOM, &theirs).unwrap();
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    assert_eq!(got, "round trip\n");
    assert_eq!(
        read(&STDIN_END, &theirs, &mut buf).unwrap(),
        0,
        "the node's half of a stdin connection didn't end after its peer's"
    );
    copier.join().unwrap();
}

/// HEX-QUIC-22 (Q4): a node that stages nothing serves no files port, so a peer's staging connection is refused by
/// type: every macOS and Windows node, which stage no file tree on any transport (their runtime stages only into its
/// own guest), and a Linux node given no folder. A macOS or Windows node given one is refused as its platform's,
/// typed. And the node's shape at its bounds starts its endpoint at once (hyper-transport lays none of it out ahead).
#[test]
fn hex_quic_22_a_node_that_stages_nothing_refuses_staging_by_type() {
    let ca = SimCa::new().unwrap();
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "node.nostage.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: ports(false),
            shape: shape(&limits, false),
        })
        .unwrap(),
    );
    let dir = scratch("n-");
    let _quic = Quic::new(
        Native::bind(&dir.path().join("node.sock")).unwrap(),
        Arc::clone(&node),
        Bounds::of(&limits),
        None,
    )
    .unwrap();
    let client = Endpoint::start(Config {
        credentials: enrolled(&ca, "client.nostage.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 4,
        },
    })
    .unwrap();
    let peer = dial(&DIAL, &client, "node.nostage.test", node.local_addr()).unwrap();
    let staging = client.open(peer, stream::files::FILES_PORT).unwrap();
    let mut buf = [0u8; 1];
    let refused = read(&READ, &staging, &mut buf).unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{refused}"
    );
    assert_eq!(staging.gone(), Some(Why::NoPort));
    #[cfg(not(target_os = "linux"))]
    {
        let other = scratch("n-");
        let given = Quic::new(
            Native::bind(&other.path().join("node.sock")).unwrap(),
            Arc::clone(&node),
            Bounds::of(&limits),
            Some(other.path().join("staged")),
        );
        assert!(
            matches!(&given, Err(Refusal::Platform { .. })),
            "a node off Linux took a folder to stage into: {:?}",
            given.err()
        );
    }
    // A host's bounds: hyper-transport grows to them, laying none out ahead, so the endpoint starts at once.
    let host = Limits {
        connections: 1 << 20,
        message_bytes: wire::MAX_MESSAGE,
        in_flight: 1 << 12,
        programs: 1 << 16,
    };
    let big = Endpoint::start(Config {
        credentials: enrolled(&ca, "node.big.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: true,
        serves: ports(false),
        shape: shape(&host, false),
    });
    assert!(big.is_ok(), "{:?}", big.err());
}

/// HEX-QUIC-23's cause on Windows (docs/design/hex-quic.md Q8; AC-T7.4.1): do dial a node whose endpoint is shaped by
/// a Windows host's platform, jupiter's as its node reads it (2^24 handles, 41,150 threads by its commit room over a
/// worker's stack, 2^23 programs by half its handle table); expect the peer connected, the node laying out at the
/// connection's start (quinn's `StreamsState::new`) a lane for each connection that can do work at once: a control
/// connection a call, a stdio connection a thread, and the log's. Counted by its programs' streams it laid out
/// 25,330,425, which held its driver past QUIC's idle timeout, and the dial was refused Unreachable.
#[test]
fn a_peer_connects_to_a_node_shaped_by_a_windows_hosts_platform() {
    let host = crate::serve::limits(&crate::serve::Platform {
        descriptors: 1 << 24,
        threads: 41_150,
        programs: 1 << 23,
    });
    let ca = SimCa::new().unwrap();
    let node = Endpoint::start(Config {
        credentials: enrolled(&ca, "node.windows-host.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: true,
        serves: ports(false),
        shape: shape(&host, false),
    })
    .unwrap();
    let client = Endpoint::start(Config {
        credentials: enrolled(&ca, "client.windows-host.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 4,
        },
    })
    .unwrap();
    let dialed = dial(&DIAL, &client, "node.windows-host.test", node.local_addr());
    assert!(dialed.is_ok(), "{:?}", dialed.err());
    let lanes = usize::try_from(shape(&host, false).lanes).unwrap();
    assert_eq!(lanes, 2 * host.in_flight + 1);
}

/// The simulator's boxes, as a node's host: the links test asks the node for none of them.
struct Simulated(SimRuntime);

impl Runtime for Simulated {
    fn classes(&self) -> Vec<Class> {
        self.0.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.0.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.0.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.0.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.0.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.0.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.0.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.delete(id)
    }
}

impl Host for Simulated {
    fn abandoned(&self, _id: &BoxId) -> Result<(), Refusal> {
        Ok(())
    }
    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(netns::Counts::default())
    }
}

/// HEX-QUIC-29 (Q12; Shards plan step 34's `kiln-hex` link): a client that reached the node over QUIC asks the node,
/// over that link, for its links, and a local client asks on the node's socket: each report names the client as its
/// certificate does, with the suite and group the node's own TLS negotiated, `TLS13_AES_256_GCM_SHA384` under a hybrid
/// post-quantum group. A node serving its local socket alone refuses the call by type.
#[test]
fn hex_quic_29_the_nodes_report_names_each_peer_with_its_suite_and_group() {
    let ca = SimCa::new().unwrap();
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "node.links.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: ports(false),
            shape: shape(&limits, false),
        })
        .unwrap(),
    );
    let dir = scratch("k-");
    let socket = dir.path().join("node.sock");
    let quic = Quic::new(
        Native::bind(&socket).unwrap(),
        Arc::clone(&node),
        Bounds::of(&limits),
        None,
    )
    .unwrap();
    let host = Arc::new(Simulated(SimRuntime::new()));
    let _server = Server::with_os(quic, host, limits, limits.in_flight, &socket).unwrap();
    let client_end = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "client.links.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: false,
            serves: Vec::new(),
            shape: Shape {
                connections: 1,
                lanes: hex_quic::limits::client_lanes(limits.in_flight, false),
            },
        })
        .unwrap(),
    );
    // No deadline of the test's own: the dial until the driver tells it, the hello and the links on their kinds'
    // learned waits (`client.quic.answer.<kind>`).
    let (client, _) = hex_quic::link::connect(
        client_end,
        "node.links.test",
        node.local_addr(),
        false,
        None,
    )
    .unwrap();
    let local = hex_client::Client::connect(&socket).unwrap();
    for links in [client.links().unwrap(), local.links().unwrap()] {
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].peer, "client.links.test");
        assert_eq!(
            CipherSuite::from(links[0].suite),
            CipherSuite::TLS13_AES_256_GCM_SHA384
        );
        let group = links[0].group.map(NamedGroup::from);
        assert!(
            matches!(
                group,
                Some(
                    NamedGroup::secp384r1MLKEM1024
                        | NamedGroup::X25519MLKEM768
                        | NamedGroup::secp256r1MLKEM768
                )
            ),
            "{group:?}"
        );
    }
    let other = scratch("k-");
    let alone = other.path().join("node.sock");
    let _plain = Server::bind(
        &alone,
        Arc::new(Simulated(SimRuntime::new())),
        limits,
        limits.in_flight,
    )
    .unwrap();
    let refused = hex_client::Client::connect(&alone)
        .unwrap()
        .links()
        .unwrap_err();
    assert!(
        matches!(refused, Refusal::Invalid { what: "links", .. }),
        "{refused:?}"
    );
}

/// The log's missed line (CLAUDE.md §10; ROOT, 2026-11-14): its count a whole number. Do: form it for three lines.
/// Expect: `hex.log event=missed lines=3`.
#[test]
fn a_readers_missed_line_counts_its_lines() {
    assert_eq!(super::missed_line(3), "hex.log event=missed lines=3");
}

/// What the node sends on control connection `conn` once a hello goes with id `id`: each answer in order, through the
/// hello's answer, or until the connection ends.
fn through_hello(conn: &hex_quic::Conn, id: u64) -> Vec<wire::Answer> {
    let hello = wire::request(
        id,
        &Request::Hello {
            build: wire::build(),
        },
        wire::MAX_MESSAGE,
    )
    .unwrap();
    let mut answers = Vec::new();
    if write_all(
        &ROOM,
        conn,
        &stream::frame(&hello, wire::MAX_MESSAGE).unwrap(),
    )
    .is_err()
    {
        return answers;
    }
    let mut deframer = Deframer::new(wire::MAX_MESSAGE);
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        let n = match read(&READ, conn, &mut buf) {
            Ok(0) | Err(_) => return answers,
            Ok(n) => n,
        };
        let mut input = &buf[..n];
        while !input.is_empty() {
            let (took, whole) = deframer.take(input).unwrap();
            input = &input[took..];
            if let Some(message) = whole {
                let received = wire::Received {
                    bytes: &message,
                    size: message.len(),
                    descriptors: 0,
                    descriptors_cut: false,
                };
                let (_, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
                let hello = matches!(answer, wire::Answer::Hello(_));
                answers.push(answer);
                if hello {
                    return answers;
                }
            }
        }
    }
}

/// HEX-QUIC-33 (Q13; the review's E-6; ROOT's note (3), 2026-11-14; AC-T7.4.1): the node takes a message that carries a
/// secret over QUIC sealed alone. Do open three control connections to a node over QUIC, read each one's token and have
/// its hello answered, then send a lease's answer on each: sealed under the connection's seal on the first, unsealed on
/// the second, and on the third an inspect sealed as a secret is, its kind marked; then a hello again on each. Expect the
/// first's lease read, named as the node gives it back (no ask waited for it), and its hello answered, its connection
/// kept; the second and third connections ended by the node with nothing more said, as bytes that aren't the node's wire
/// end one.
#[test]
fn hex_quic_33_the_node_takes_a_secret_over_quic_sealed_alone() {
    let ca = SimCa::new().unwrap();
    let limits = limits();
    let node = Arc::new(
        Endpoint::start(Config {
            credentials: enrolled(&ca, "node.seal.test"),
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            listen: true,
            serves: ports(false),
            shape: shape(&limits, false),
        })
        .unwrap(),
    );
    let dir = scratch("q-");
    let socket = dir.path().join("node.sock");
    let quic = Quic::new(
        Native::bind(&socket).unwrap(),
        Arc::clone(&node),
        Bounds::of(&limits),
        None,
    )
    .unwrap();
    let host = Arc::new(Simulated(SimRuntime::new()));
    let _server = Server::with_os(quic, host, limits, limits.in_flight, &socket).unwrap();
    let client = Endpoint::start(Config {
        credentials: enrolled(&ca, "client.seal.test"),
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        listen: false,
        serves: Vec::new(),
        shape: Shape {
            connections: 1,
            lanes: 8,
        },
    })
    .unwrap();
    let peer = dial(&DIAL, &client, "node.seal.test", node.local_addr()).unwrap();
    let keys = client.keys(peer).unwrap();
    let leased = wire::request(
        9,
        &Request::Leased {
            run: 1,
            answer: Ok(hex_api::lease::Lease {
                lease: "lease-33".to_owned(),
                value: hex_api::lease::Value::new(b"hex-33-value".to_vec()),
                life: None,
            }),
        },
        wire::MAX_MESSAGE,
    )
    .unwrap();
    let mut inspect = wire::request(
        10,
        &Request::Call(Call::Inspect(BoxId::new("q13-box").unwrap())),
        wire::MAX_MESSAGE,
    )
    .unwrap();
    // An inspect marked sealed, its body sealed under the connection's seal as a secret's would be.
    let kind = u16::from_le_bytes([inspect[12], inspect[13]]);
    inspect[12..14].copy_from_slice(&(kind | wire::SEALED).to_le_bytes());
    let mut sealed_inspect = inspect[..wire::HEADER_BYTES].to_vec();
    let mut body = vec![0u8; inspect.len() - wire::HEADER_BYTES];
    let (nonce, tag) = wire::Seal::seal(
        &*keys,
        &inspect[..wire::HEADER_BYTES],
        &inspect[wire::HEADER_BYTES..],
        &mut body,
    )
    .unwrap();
    sealed_inspect.extend_from_slice(&nonce);
    sealed_inspect.extend_from_slice(&body);
    sealed_inspect.extend_from_slice(&tag);
    let sends = [
        (
            "a lease's answer sealed",
            wire::seal(&leased, &*keys).unwrap(),
            true,
        ),
        ("a lease's answer unsealed", leased.clone(), false),
        ("an inspect marked sealed", sealed_inspect, false),
    ];
    for (n, (what, message, kept)) in sends.into_iter().enumerate() {
        let conn = client.open(peer, stream::CONTROL_PORT).unwrap();
        let mut token = [0u8; stream::TOKEN_BYTES];
        let mut got = 0;
        while got < token.len() {
            let read = read(&READ, &conn, &mut token[got..]).unwrap();
            assert_ne!(read, 0, "{what}: the token");
            got += read;
        }
        let first = 100 + 10 * n as u64;
        let hello = through_hello(&conn, first);
        assert!(
            matches!(hello.as_slice(), [wire::Answer::Hello(_)]),
            "{what}: the hello: {hello:?}"
        );
        write_all(
            &ROOM,
            &conn,
            &stream::frame(&message, wire::MAX_MESSAGE).unwrap(),
        )
        .unwrap();
        let after = through_hello(&conn, first + 1);
        if kept {
            assert!(
                matches!(
                    after.as_slice(),
                    [wire::Answer::LeaseGiven(lease), wire::Answer::Hello(_)] if lease == "lease-33"
                ),
                "{what}: its lease read and its connection kept: {after:?}"
            );
        } else {
            assert!(after.is_empty(), "{what}: its connection ended: {after:?}");
        }
    }
}
