//! The node's QUIC listener in this process (CP-T7.3; docs/design/hex-quic.md Q4).

use super::*;

use std::net::{Ipv4Addr, SocketAddr};

use hex_core::patience::Patience;
use hex_quic::enroll::{self, sim::SimCa};
use hex_quic::{Config, Shape};
use hex_testkit::waits::within;

use crate::serve::Native;

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
        let n = within(&READ, |by| conn.read(&mut buf, by)).unwrap();
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
    let peer = within(&DIAL, |by| {
        client.dial("node.log.test", node.local_addr(), by)
    })
    .unwrap();
    let reader = client.open(peer, hex_core::lanes::LOG_PORT).unwrap();
    hex_testkit::waits::until(&READER, "the log's reader", || log.readers() == 1).unwrap();
    let mut pending = Vec::new();
    log.say("hex.node event=first\n");
    assert_eq!(read_line(&reader, &mut pending), "hex.node event=first\n");
    // The reader stops: lines of a frame each fill its lane and QUIC's window behind it, and the node goes on.
    let big = format!("{}\n", "x".repeat(hex_core::lanes::DATA_BOUND - 1));
    let mut said = 0u64;
    hex_testkit::waits::until(&MISSED, "a missed line", || {
        log.say(&big);
        said += 1;
        log.missed() > 0
    })
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
    hex_testkit::waits::until(&LOG_ROOM, "the reader's room", || {
        log.say("hex.node event=after\n");
        log.missed() == 0
    })
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
    let peer = within(&DIAL, |by| {
        client.dial("node.stage.test", node.local_addr(), by)
    })
    .unwrap();
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
    let peer = within(&DIAL, |by| {
        client.dial("node.stdin.test", node.local_addr(), by)
    })
    .unwrap();
    let mut buf = [0u8; 64];

    // The program lets its stdin go.
    let theirs = client.open(peer, stream::STDIO_PORT).unwrap();
    let (_, ours) = within(&ACCEPT, |by| node.accept(stream::STDIO_PORT, by)).unwrap();
    let (r, w) = std::io::pipe().unwrap();
    let copier = std::thread::spawn(move || feed(&ours, w));
    drop(r);
    #[cfg(windows)]
    within(&ROOM, |by| theirs.write_all(b"after its end\n", by)).unwrap();
    let ended = within(&STDIN_END, |by| theirs.read(&mut buf, by));
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
    let (_, ours) = within(&ACCEPT, |by| node.accept(stream::STDIO_PORT, by)).unwrap();
    let (mut r, w) = std::io::pipe().unwrap();
    let copier = std::thread::spawn(move || feed(&ours, w));
    within(&ROOM, |by| theirs.write_all(b"round trip\n", by)).unwrap();
    within(&ROOM, |by| theirs.end(by)).unwrap();
    let mut got = String::new();
    r.read_to_string(&mut got).unwrap();
    assert_eq!(got, "round trip\n");
    assert_eq!(
        within(&STDIN_END, |by| theirs.read(&mut buf, by)).unwrap(),
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
    let peer = within(&DIAL, |by| {
        client.dial("node.nostage.test", node.local_addr(), by)
    })
    .unwrap();
    let staging = client.open(peer, stream::files::FILES_PORT).unwrap();
    let mut buf = [0u8; 1];
    let refused = within(&READ, |by| staging.read(&mut buf, by)).unwrap_err();
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
