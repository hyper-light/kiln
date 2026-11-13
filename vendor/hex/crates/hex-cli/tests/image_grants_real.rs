//! Joint plan step 15, AC-T7.7.2 and T-T7.7.1 (docs/design/network-grants.md N4 and N6): a Shards image VM's network
//! is its grants alone, on VMs a real `hex node` boots: on Linux KVM, macOS HVF and Windows WHP alike. These are the
//! booted-VM checks Hex's lead runs beside goal 12's driver of the class's cases (`image_real.rs`), whose node and
//! helpers they share (`image_node`). Every VM boots the probe image, whose one harness is the grants probe
//! (`image_grants_probe/probe.rs`; its context made by `scripts/grants-probe/build.sh`, whose header says what its
//! Agentfile grants).
//!
//! - **A destination its image grants not refuses the boot.** The probe granted a name its image doesn't grant is made,
//!   and its start refused `Invalid`, past the image's grants: Shards' run host refused it before anything started.
//! - **No grant, no network.** A VM granted nothing runs a command, in its image's own shell, that ends 0 only where
//!   `lo` is its one interface: with no NIC, no name is answered and no packet leaves. A VM of its members alone
//!   granted nothing runs with no network process (Shards' `shards-net`, one per networked microVM, D36), and no
//!   process of its run holds a listening TCP or UDP socket.
//! - **One publish, published alone, to its member.** A VM of its members alone is granted one publish: the probe's
//!   port at the host's loopback, on a host port the OS picks. Inspect shows that one grant, and its one published
//!   port at 127.0.0.1 reaching the probe at that port, as Shards names it. Its run listens there alone: at no
//!   wildcard address, and not on the image's other port coming in. A connection there reaches the probe, which says
//!   it came in on that port and that none of its own egress connected: its provider and an ungranted name, the
//!   host's service, and the image's port egress, every one its image grants dropped by Hex's narrowing. No process of
//!   the run holds a flow past the host's loopback, and the host's service, a listener of this test's, took nothing.
//! - **Its granted destinations alone, reached.** A VM of its members alone is granted that publish and, by TCP, the
//!   probe's provider by name and the host's service (kiln's endpoint, `host.docker.internal`): the probe says it
//!   reached both, its ungranted name and the image's port egress nowhere, and the host's service, this test's
//!   listener, took its connection (N4: the run host holds every member to its reach of the destinations kept).
//! - **A publish reaching the run's own command stops it.** A VM with a command of its own is granted a publish of a
//!   port its image gives no member, which Shards gives that command (D122): its start is refused `Invalid`, naming
//!   the run's own command, the VM is stopped, and no run host is left.
//!
//! What the host shows of a run (its processes, under its run host `shards api-run`, which the node starts, D231; and
//! the TCP and UDP sockets they hold) is read from /proc on Linux (`host`). There a descriptor list this test can't
//! read fails it: what can't be seen isn't passed. macOS and Windows keep no /proc: there those readings are owed, and
//! the run says so (`GRANTS-OWED`), while every check of the VM itself, its inspect and the host's service runs alike.
//!
//! Explicit inputs only, each by its own variable; a missing one fails by name. Run alone, `--ignored`: on Linux
//! inside a cgroup delegated to this binary (`HEX_DELEGATED`); with image_real's node inputs (`SHARDS_BIN`, which
//! must be Hex's `shards`, `scripts/shards-gpu/build.sh`'s, whose run host holds each member to its reach;
//! `SHARDS_KERNEL` and `SHARDS_INIT`, the kernel the run hosts boot and the init beside it, which Shards takes
//! together, the init the `shards` build carries (build.sh's `shards-init`); the node's `HEX_GUEST_*`,
//! `HEX_IMAGE_CPU_MILLI`, `HEX_IMAGE_MEMORY_BYTES`, `HEX_IMAGE_WORK`, a short path where the node's socket lies,
//! `HEX_IMAGE_WAIT_MS` and `HEX_WAITS_CENSUS_MAX`), and the probe's:
//! - the probe image, in the node's Shards home before the node starts: `HEX_GRANTS_CONTEXT`, the context
//!   `scripts/grants-probe/build.sh` made, built there; or else `HEX_GRANTS_IMAGE`, `NAME@sha256:...`, pulled there;
//! - the recipe's own inputs, as it was given them: `HEX_GRANTS_MEMBER`, `HEX_GRANTS_PORT`,
//!   `HEX_GRANTS_UNGRANTED_PORT`, `HEX_GRANTS_PROVIDER` (a provider this host reaches at TCP 443),
//!   `HEX_GRANTS_UNGRANTED`, `HEX_GRANTS_HOST_PORT` (where this test listens on the host's loopback for the probe's
//!   try) and `HEX_GRANTS_OPEN`;
//! - `HEX_GRANTS_COMMAND_PORT`: a TCP port the image gives no member, nor declares as egress.
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_grants_real -- --ignored --nocapture` with those set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::fs;
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, BoxState, Class, ClassSpec, Destination, Exit, Grant,
    Member, MemberKind, Ports, Proto, Receiver, Refusal, Runtime, ShardsImage, Signal,
};
use hex_client::Client;
use hex_core::patience::Patience;

#[path = "image_node/mod.rs"]
mod image_node;
use image_node::{Node, allocation, built, input, number, pull, records, step};

#[path = "image_grants_probe/probe.rs"]
mod probe;
use probe::{Outcome, Report};

/// What this test waits for, each fact its own: no recorded operation makes either, so each measures, for its fact
/// alone (hex_testkit::waits; the lead's rule of 2026-11-08), and the run's deadline bounds the whole.
static PROBE_ENDED: Patience = Patience::measuring("test.hex_cli.image_grants_probe_ended");
static MEMBER_ANSWERED: Patience = Patience::measuring("test.hex_cli.image_grants_member_answered");

/// The name the probe image is tagged with where this test builds it into the node's Shards home.
const BUILT: &str = "hex-image-grants/probe";

/// The probe image, its harness and the recipe's inputs, as the run was given them.
struct Probe {
    image: String,
    member: Member,
    port: u16,
    ungranted_port: u16,
    provider: String,
    ungranted: String,
    host_port: u16,
    open: String,
    command_port: u16,
}

fn port(name: &str) -> u16 {
    u16::try_from(number(name)).unwrap_or_else(|_| panic!("{name} is no TCP port"))
}

/// A Shards image VM of `image`, admitted for the run's workload, with its run's own command where it has one, and
/// its grants.
fn image_box(
    name: &str,
    image: &str,
    allocation: Allocation,
    command: Option<Vec<String>>,
    grants: Vec<Grant>,
) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(ShardsImage {
            command,
            grants,
            ..ShardsImage::new(image)
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation),
        route: None,
        band: 0,
    }
}

/// The probe's TCP port `port`, published at the host's loopback on a port the OS picks.
fn publish(probe: &Probe, port: u16) -> Grant {
    Grant::Publish {
        member: probe.member.clone(),
        proto: Proto::Tcp,
        port,
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        host_port: None,
    }
}

/// What the host shows of a run, from /proc: its processes, under the one run host the node `node` started
/// (`shards api-run`, D231), and the TCP and UDP sockets they hold, from their network namespace's tables.
#[cfg(target_os = "linux")]
mod host {
    use std::fs;
    use std::io::ErrorKind;
    use std::net::{IpAddr, SocketAddr};

    /// TCP_LISTEN, a listening TCP socket's state in /proc/net's tables (include/net/tcp_states.h).
    const TCP_LISTEN: u8 = 10;

    /// A process as /proc shows it now.
    #[derive(Debug)]
    struct Process {
        pid: u32,
        parent: u32,
        /// Its name (`comm`): its program's file name, as the kernel keeps it.
        name: String,
        /// Its arguments, its program's first.
        args: Vec<String>,
    }

    /// Every process /proc lists now. One that ends while this reads it is left out: it is no running run's.
    fn processes() -> Vec<Process> {
        let mut out = Vec::new();
        for entry in fs::read_dir("/proc").unwrap() {
            let entry = entry.unwrap();
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let (Ok(stat), Ok(name), Ok(args)) = (
                fs::read_to_string(format!("/proc/{pid}/stat")),
                fs::read_to_string(format!("/proc/{pid}/comm")),
                fs::read(format!("/proc/{pid}/cmdline")),
            ) else {
                continue;
            };
            // The name sits in parentheses and may hold any byte, so the state, then the parent, follow the last
            // ')'.
            let parent = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("/proc/{pid}/stat: {stat}"));
            out.push(Process {
                pid,
                parent,
                name: name.trim_end().to_owned(),
                args: args
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect(),
            });
        }
        out
    }

    /// The roots and every process under them, each found by its parent, breadth first, with no recursion
    /// (CLAUDE.md §11).
    fn under(all: &[Process], roots: Vec<u32>) -> Vec<u32> {
        let mut found = roots;
        let mut at = 0;
        while let Some(&parent) = found.get(at) {
            found.extend(all.iter().filter(|p| p.parent == parent).map(|p| p.pid));
            at += 1;
        }
        found
    }

    /// The run hosts under the node `node`.
    fn run_hosts(all: &[Process], node: u32) -> Vec<u32> {
        let of_node = under(all, vec![node]);
        all.iter()
            .filter(|p| of_node.contains(&p.pid) && p.args.get(1).is_some_and(|a| a == "api-run"))
            .map(|p| p.pid)
            .collect()
    }

    /// The run's processes: the one run host under the node `node` and every process under it.
    fn run_of(node: u32) -> Vec<Process> {
        let all = processes();
        let hosts = run_hosts(&all, node);
        assert_eq!(hosts.len(), 1, "the node's run hosts: {hosts:?}");
        let pids = under(&all, hosts);
        let mut run: Vec<Process> = all.into_iter().filter(|p| pids.contains(&p.pid)).collect();
        run.sort_by_key(|p| p.pid);
        for p in &run {
            eprintln!("GRANTS-PROCESS {} {} {:?}", p.pid, p.name, p.args);
        }
        run
    }

    /// A TCP or UDP socket as its network namespace's table shows it.
    #[derive(Debug)]
    struct Socket {
        tcp: bool,
        local: SocketAddr,
        remote: SocketAddr,
        /// Its state number in the table (include/net/tcp_states.h).
        state: u8,
        inode: u64,
    }

    impl Socket {
        /// It takes what comes in: a listening TCP socket, or a UDP socket with no peer.
        fn listens(&self) -> bool {
            if self.tcp {
                self.state == TCP_LISTEN
            } else {
                self.remote.ip().is_unspecified()
            }
        }

        /// It carries a flow to a peer past the host's own loopback.
        fn leaves(&self) -> bool {
            let peer = self.remote.ip().to_canonical();
            !self.listens() && !peer.is_loopback() && !peer.is_unspecified()
        }
    }

    /// An address as /proc/net's tables write it: the address's bytes as the host's 32-bit words in hex, then the
    /// port in hex (net/ipv4/tcp_ipv4.c `get_tcp4_sock`, net/ipv6/tcp_ipv6.c `get_tcp6_sock`).
    fn address(text: &str) -> SocketAddr {
        let (ip, port) = text.split_once(':').unwrap();
        let bytes: Vec<u8> = ip
            .as_bytes()
            .chunks(8)
            .flat_map(|word| {
                u32::from_str_radix(std::str::from_utf8(word).unwrap(), 16)
                    .unwrap()
                    .to_ne_bytes()
            })
            .collect();
        let ip = match bytes.len() {
            4 => IpAddr::from(<[u8; 4]>::try_from(bytes).unwrap()),
            16 => IpAddr::from(<[u8; 16]>::try_from(bytes).unwrap()),
            n => panic!("an address of {n} bytes: {text}"),
        };
        SocketAddr::new(ip, u16::from_str_radix(port, 16).unwrap())
    }

    /// The TCP and UDP sockets of the network namespace the process `pid` is in, from its tables. A kernel with no
    /// IPv6 has no IPv6 tables, and so no IPv6 socket.
    fn sockets_seen_by(pid: u32) -> Vec<Socket> {
        let mut out = Vec::new();
        for (table, tcp) in [
            ("tcp", true),
            ("tcp6", true),
            ("udp", false),
            ("udp6", false),
        ] {
            let path = format!("/proc/{pid}/net/{table}");
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == ErrorKind::NotFound && table.ends_with('6') => continue,
                Err(e) => panic!("{path}: {e}"),
            };
            for row in text.lines().skip(1) {
                let fields: Vec<&str> = row.split_whitespace().collect();
                // sl, local, remote, state, tx_queue:rx_queue, tr:tm->when, retrnsmt, uid, timeout, inode, ...
                let [_, local, remote, state, _, _, _, _, _, inode, ..] = fields.as_slice() else {
                    panic!("{path}: a row of {} fields: {row}", fields.len());
                };
                out.push(Socket {
                    tcp,
                    local: address(local),
                    remote: address(remote),
                    state: u8::from_str_radix(state, 16).unwrap(),
                    inode: inode.parse().unwrap(),
                });
            }
        }
        out
    }

    /// The inodes of the sockets the process `pid`'s descriptors hold.
    fn socket_inodes(pid: u32) -> Vec<u64> {
        let dir = format!("/proc/{pid}/fd");
        let mut out = Vec::new();
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{dir}: {e}: a run's descriptors this test can't read"));
        for entry in entries {
            // A descriptor closed while this reads the list is no longer held.
            let Ok(target) = fs::read_link(entry.unwrap().path()) else {
                continue;
            };
            if let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
            {
                out.push(inode.parse().unwrap());
            }
        }
        out
    }

    /// The TCP and UDP sockets the run's processes hold, each once.
    fn sockets_of(run: &[Process]) -> Vec<Socket> {
        let mut out: Vec<Socket> = Vec::new();
        for p in run {
            let held = socket_inodes(p.pid);
            if held.is_empty() {
                continue;
            }
            for socket in sockets_seen_by(p.pid) {
                if held.contains(&socket.inode) && out.iter().all(|s| s.inode != socket.inode) {
                    out.push(socket);
                }
            }
        }
        for s in &out {
            eprintln!(
                "GRANTS-SOCKET {} {} -> {} state {} inode {}",
                if s.tcp { "tcp" } else { "udp" },
                s.local,
                s.remote,
                s.state,
                s.inode
            );
        }
        out
    }

    /// How many of the run's processes are named `name`.
    fn named(run: &[Process], name: &str) -> usize {
        run.iter().filter(|p| p.name == name).count()
    }

    /// A VM granted nothing: its run's VM process beside its run host, no network process, and no socket that
    /// listens or leaves the host.
    pub fn granted_nothing(node: u32) {
        let run = run_of(node);
        assert_eq!(
            named(&run, "shards-vm"),
            1,
            "the run's VM process: {run:#?}"
        );
        assert_eq!(
            named(&run, "shards-net"),
            0,
            "a network process in the run of a VM granted nothing: {run:#?}"
        );
        let sockets = sockets_of(&run);
        let listening: Vec<&Socket> = sockets.iter().filter(|s| s.listens()).collect();
        assert!(
            listening.is_empty(),
            "the run of a VM granted nothing listens: {listening:#?}"
        );
        let leaving: Vec<&Socket> = sockets.iter().filter(|s| s.leaves()).collect();
        assert!(
            leaving.is_empty(),
            "the run of a VM granted nothing holds a flow past the host's loopback: {leaving:#?}"
        );
    }

    /// A VM granted one publish, bound at `at`: its network process there (D36: so a VM granted nothing, which had
    /// none, had a real one to lack), and its run listening at `at` alone, at no wildcard address and not on the
    /// image's port `ungranted`.
    pub fn listens_alone(node: u32, at: SocketAddr, ungranted: u16) {
        let run = run_of(node);
        assert_eq!(
            named(&run, "shards-net"),
            1,
            "the run's network process: {run:#?}"
        );
        let sockets = sockets_of(&run);
        let listening: Vec<&Socket> = sockets.iter().filter(|s| s.listens()).collect();
        assert!(
            listening.iter().all(|s| !s.local.ip().is_unspecified()),
            "the run listens at a wildcard address: {listening:#?}"
        );
        assert!(
            listening.iter().all(|s| s.local.port() != ungranted),
            "the run listens on the ungranted port {ungranted}: {listening:#?}"
        );
        assert!(
            matches!(listening.as_slice(), [only] if only.tcp && only.local == at),
            "the run listens other than at {at} alone: {listening:#?}"
        );
    }

    /// No process of the run holds a flow past the host's loopback.
    pub fn nothing_leaves(node: u32) {
        let leaving: Vec<Socket> = sockets_of(&run_of(node))
            .into_iter()
            .filter(Socket::leaves)
            .collect();
        assert!(
            leaving.is_empty(),
            "the run holds a flow past the host's loopback: {leaving:#?}"
        );
    }

    /// No run host is left under the node `node`.
    pub fn no_run_left(node: u32) {
        let left = run_hosts(&processes(), node);
        assert!(left.is_empty(), "run hosts left under the node: {left:?}");
    }

    /// The /proc reading's address form (no VM): do read addresses as a little-endian kernel writes them, an IPv4
    /// loopback port, IPv6's loopback, and an IPv4-mapped one; expect each as it is.
    #[cfg(target_endian = "little")]
    #[test]
    fn proc_net_addresses_read_as_the_kernel_writes_them() {
        assert_eq!(address("0100007F:1F90"), "127.0.0.1:8080".parse().unwrap());
        assert_eq!(
            address("00000000000000000000000001000000:01BB"),
            "[::1]:443".parse().unwrap()
        );
        assert_eq!(
            address("0000000000000000FFFF00000100007F:0035"),
            "[::ffff:127.0.0.1]:53".parse().unwrap()
        );
    }

    /// Non-vacuity of the /proc reading (no VM): do hold, in this process, a TCP listener and a UDP socket on the
    /// loopback and a TCP connection to the listener; expect the reading to find each at its address, the listener
    /// and the UDP socket listening, and the connection neither listening nor leaving the host. Do judge sockets
    /// whose peers are past the loopback, at it and unset; expect only those past it to leave.
    #[test]
    fn the_proc_reading_finds_a_processs_own_sockets() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let connection = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let this = Process {
            pid: std::process::id(),
            parent: 0,
            name: String::new(),
            args: Vec::new(),
        };
        let sockets = sockets_of(&[this]);
        let found = |at: SocketAddr, tcp: bool| {
            sockets
                .iter()
                .find(|s| s.local == at && s.tcp == tcp)
                .unwrap_or_else(|| panic!("no socket at {at} (tcp {tcp}) in {sockets:#?}"))
        };
        assert!(found(listener.local_addr().unwrap(), true).listens());
        assert!(found(udp.local_addr().unwrap(), false).listens());
        let near = found(connection.local_addr().unwrap(), true);
        assert_eq!(near.remote, listener.local_addr().unwrap());
        assert!(!near.listens() && !near.leaves());
        // States from include/net/tcp_states.h: 1 TCP_ESTABLISHED, 2 TCP_SYN_SENT.
        let tcp = |remote: &str, state| Socket {
            tcp: true,
            local: "10.0.0.2:40000".parse().unwrap(),
            remote: remote.parse().unwrap(),
            state,
            inode: 0,
        };
        assert!(tcp("192.0.2.1:443", 1).leaves());
        assert!(tcp("192.0.2.1:443", 2).leaves());
        assert!(!tcp("127.0.0.1:443", 1).leaves());
        assert!(!tcp("[::ffff:127.0.0.1]:443", 1).leaves());
        assert!(!tcp("0.0.0.0:0", TCP_LISTEN).leaves());
    }
}

/// What the host shows of a run is /proc's on Linux alone: here each of those readings is owed, and the run says so.
#[cfg(not(target_os = "linux"))]
mod host {
    use std::net::SocketAddr;

    fn owed(what: &str) {
        eprintln!(
            "GRANTS-OWED {what}: this host keeps no /proc, and this test reads a run's processes and sockets there alone"
        );
    }

    pub fn granted_nothing(_: u32) {
        owed("a VM granted nothing: no network process, no listening socket");
    }

    pub fn listens_alone(_: u32, at: SocketAddr, ungranted: u16) {
        owed(&format!(
            "a VM granted one publish: its run listening at {at} alone, not on {ungranted}"
        ));
    }

    pub fn nothing_leaves(_: u32) {
        owed("no flow of the run's past the host's loopback");
    }

    pub fn no_run_left(_: u32) {
        owed("no run host left under the node");
    }
}

/// Stops box `id` with KILL, Hex's stop of an image VM, and deletes it; expects it ended by that signal.
fn stop_and_delete(client: &Client, id: &BoxId) {
    client.signal(id, Signal::KILL).unwrap();
    let ended = client.inspect(id).unwrap();
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    client.delete(id).unwrap();
}

/// Joint plan step 15 (N4): do create, then start, the probe granted by TCP a name its image grants not (its ungranted
/// name). Expect it made, and its start refused Invalid, past the image's grants: Shards' run host refused it before
/// anything started, so the box ended and holds nothing.
fn a_destination_its_image_grants_not_refuses_the_boot(
    client: &Client,
    probe: &Probe,
    allocation: Allocation,
) {
    let spec = image_box(
        "grants-wider",
        &probe.image,
        allocation,
        None,
        vec![Grant::Egress {
            member: probe.member.clone(),
            to: Destination::Name(probe.ungranted.clone()),
            ports: Ports {
                proto: Proto::Tcp,
                first: 443,
                last: 443,
            },
        }],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    let refused = client.start(&id);
    eprintln!("GRANTS-CHECK-1 {id} start: {refused:?}");
    assert!(
        matches!(&refused, Err(Refusal::Invalid { what: "shards image", why }) if why.contains("past the image's grants")),
        "{refused:?}"
    );
    let ended = client.inspect(&id).unwrap();
    assert_eq!(ended.state, BoxState::Empty);
    assert!(ended.published.is_empty());
    client.delete(&id).unwrap();
}

/// Joint plan step 15, T-T7.7.1: do run a VM granted nothing whose command, in its image's own shell, ends 0 only where
/// `lo` is its one interface; expect it to end 0. Do run one of its members alone granted nothing; expect its run to
/// hold no network process and no listening socket, the VM process found beside its run host.
fn no_grant_no_network(client: &Client, node: u32, probe: &Probe, allocation: Allocation) {
    let only_lo = r#"set -- /sys/class/net/*; [ "$#" -eq 1 ] && [ "$1" = /sys/class/net/lo ]"#;
    let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), only_lo.to_owned()];
    let none = image_box(
        "grants-none-probe",
        &probe.image,
        allocation,
        Some(command),
        Vec::new(),
    );
    let id = none.id.clone();
    client.create(none).unwrap();
    client.start(&id).unwrap();
    let mut ended = None;
    let looks = hex_testkit::waits::until(&PROBE_ENDED, &format!("{id} ended"), || {
        let info = client.inspect(&id).unwrap();
        let done = info.state == BoxState::Empty;
        ended = Some(info);
        done
    })
    .unwrap();
    let ended = ended.unwrap();
    eprintln!(
        "GRANTS-CHECK-2 {id} ended {:?} after {looks} looks, granted {:?}",
        ended.exit, ended.grants
    );
    assert!(ended.grants.is_empty() && ended.published.is_empty());
    assert_eq!(
        ended.exit,
        Some(Exit::Code(0)),
        "a VM granted nothing has an interface other than lo, or no /sys/class/net"
    );
    client.delete(&id).unwrap();

    let held = image_box(
        "grants-none-held",
        &probe.image,
        allocation,
        None,
        Vec::new(),
    );
    let id = held.id.clone();
    client.create(held).unwrap();
    client.start(&id).unwrap();
    host::granted_nothing(node);
    eprintln!("GRANTS-CHECK-2 {id}: no network process, no listening socket");
    stop_and_delete(client, &id);
}

/// Joint plan step 15, AC-T7.7.2: do run a VM of its members alone granted one publish, the probe's port at the host's
/// loopback, the OS picking the host's port, while this test listens on the host's loopback at the service port the
/// image grants as `host.docker.internal`. Expect: inspect shows that one grant and its one published port at
/// 127.0.0.1, reaching the probe at that port; the run listens there alone, at no wildcard address and not on the
/// image's other port coming in; a connection there reaches the probe, which says it came in on that port and that
/// none of its egress connected (its provider, an ungranted name, the host's service, the image's port egress), each
/// outcome typed; the run holds no flow past the host's loopback; and the host's service took nothing.
fn one_publish_published_alone(client: &Client, node: u32, probe: &Probe, allocation: Allocation) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, probe.host_port)).unwrap();
    service.set_nonblocking(true).unwrap();
    let grant = publish(probe, probe.port);
    let spec = image_box(
        "grants-publish",
        &probe.image,
        allocation,
        None,
        vec![grant.clone()],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    let shown = client.inspect(&id).unwrap();
    eprintln!(
        "GRANTS-CHECK-3 {id} granted {:?}, published {:?}",
        shown.grants, shown.published
    );
    assert_eq!(shown.grants, vec![grant]);
    let [published] = shown.published.as_slice() else {
        panic!("published ports: {:?}", shown.published);
    };
    // Its receiver as Shards names it (D231's `Bound::to`): the probe, at its own port.
    assert_eq!(
        (published.grant, published.at.ip(), published.receiver),
        (
            0,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            Receiver::Named { port: probe.port }
        )
    );
    let at = published.at;
    host::listens_alone(node, at, probe.ungranted_port);
    // The probe answers once its program listens: each look is one connection read to its end, which Shards closes
    // where the probe doesn't take it yet.
    let mut answer = String::new();
    let looks = hex_testkit::waits::until(
        &MEMBER_ANSWERED,
        &format!("the probe's answer at {at}"),
        || {
            answer.clear();
            TcpStream::connect(at)
                .and_then(|mut c| c.read_to_string(&mut answer))
                .is_ok()
                && !answer.is_empty()
        },
    )
    .unwrap();
    eprintln!("GRANTS-CHECK-3 the probe said {answer:?} after {looks} looks");
    let report = Report::parse(&answer).unwrap();
    assert_eq!(report.ingress, probe.port, "the port it came in on");
    let (provider, ungranted, service_at, open) = (
        format!("{}:443", probe.provider),
        format!("{}:443", probe.ungranted),
        format!("host.docker.internal:{}", probe.host_port),
        format!("{}:80", probe.open),
    );
    let tried: Vec<(&str, &str)> = report
        .egress
        .iter()
        .map(|(target, _)| (target.label.as_str(), target.at.as_str()))
        .collect();
    assert_eq!(
        tried,
        [
            ("provider", provider.as_str()),
            ("ungranted", ungranted.as_str()),
            ("host", service_at.as_str()),
            ("open", open.as_str())
        ],
        "the probe's targets: its context isn't the recipe's of these inputs"
    );
    for (target, outcome) in &report.egress {
        assert_ne!(
            *outcome,
            Outcome::Connected,
            "the probe reached {} ({}), which Hex's narrowing drops",
            target.at,
            target.label
        );
    }
    host::nothing_leaves(node);
    match service.accept() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
        taken => panic!("the host's service at {}: {taken:?}", probe.host_port),
    }
    eprintln!(
        "GRANTS-CHECK-3 {id}: published at {at} alone, to the probe; its egress reached nothing"
    );
    stop_and_delete(client, &id);
}

/// Joint plan step 15, AC-T7.7.2 (N4): do run a VM of its members alone granted the probe's publish and, by TCP, its
/// provider by name and the host's service at the port its image grants as `host.docker.internal`, while this test
/// listens there on the host's loopback. Expect: inspect shows the three grants and the publish reaching the probe;
/// the probe says it came in on its port, reached its provider and the host's service, and its ungranted name and the
/// image's port egress nowhere; and the host's service took the probe's connection. So a harness reaches exactly its
/// granted provider and kiln's endpoint.
fn granted_destinations_are_reached_alone(client: &Client, probe: &Probe, allocation: Allocation) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, probe.host_port)).unwrap();
    service.set_nonblocking(true).unwrap();
    let tcp = |port| Ports {
        proto: Proto::Tcp,
        first: port,
        last: port,
    };
    let grants = vec![
        publish(probe, probe.port),
        Grant::Egress {
            member: probe.member.clone(),
            to: Destination::Name(probe.provider.clone()),
            ports: tcp(443),
        },
        Grant::Host {
            member: probe.member.clone(),
            ports: tcp(probe.host_port),
        },
    ];
    let spec = image_box(
        "grants-reach",
        &probe.image,
        allocation,
        None,
        grants.clone(),
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    let shown = client.inspect(&id).unwrap();
    eprintln!(
        "GRANTS-CHECK-4 {id} granted {:?}, published {:?}",
        shown.grants, shown.published
    );
    assert_eq!(shown.grants, grants);
    let [published] = shown.published.as_slice() else {
        panic!("published ports: {:?}", shown.published);
    };
    assert_eq!(published.receiver, Receiver::Named { port: probe.port });
    let at = published.at;
    let mut answer = String::new();
    let looks = hex_testkit::waits::until(
        &MEMBER_ANSWERED,
        &format!("the probe's answer at {at}"),
        || {
            answer.clear();
            TcpStream::connect(at)
                .and_then(|mut c| c.read_to_string(&mut answer))
                .is_ok()
                && !answer.is_empty()
        },
    )
    .unwrap();
    eprintln!("GRANTS-CHECK-4 the probe said {answer:?} after {looks} looks");
    let report = Report::parse(&answer).unwrap();
    assert_eq!(report.ingress, probe.port);
    let outcome = |label: &str| {
        report
            .egress
            .iter()
            .find(|(target, _)| target.label == label)
            .map(|(_, outcome)| outcome.clone())
            .unwrap_or_else(|| panic!("no {label} target in {answer:?}"))
    };
    assert_eq!(
        outcome("provider"),
        Outcome::Connected,
        "its granted provider"
    );
    assert_eq!(
        outcome("host"),
        Outcome::Connected,
        "the host's service, kiln's endpoint"
    );
    assert_ne!(
        outcome("ungranted"),
        Outcome::Connected,
        "a name no grant names"
    );
    assert_ne!(
        outcome("open"),
        Outcome::Connected,
        "the image's port egress, dropped"
    );
    assert!(
        service.accept().is_ok(),
        "the host's service at {} took no connection",
        probe.host_port
    );
    eprintln!("GRANTS-CHECK-4 {id}: its provider and the host's service reached, nothing else");
    stop_and_delete(client, &id);
}

/// Joint plan step 15 (N6): do create, then start, a VM with a command of its own granted a publish of a port its
/// image gives no member, which Shards gives that command (D122). Expect: the start refused Invalid naming the run's
/// own command, the box ended by KILL with nothing published, and no run host left under the node.
fn a_publish_reaching_the_runs_own_command_stops_it(
    client: &Client,
    node: u32,
    probe: &Probe,
    allocation: Allocation,
) {
    let command = ["/bin/sh", "-c", "exec tail -f /dev/null"]
        .map(str::to_owned)
        .to_vec();
    let spec = image_box(
        "grants-command",
        &probe.image,
        allocation,
        Some(command),
        vec![publish(probe, probe.command_port)],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    let refused = client.start(&id);
    eprintln!("GRANTS-CHECK-5 {id} start: {refused:?}");
    assert!(
        matches!(&refused, Err(Refusal::Invalid { what: "grants", why }) if why.contains("the run's own command")),
        "{refused:?}"
    );
    let ended = client.inspect(&id).unwrap();
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(ended.published.is_empty());
    host::no_run_left(node);
    client.delete(&id).unwrap();
}

/// Joint plan step 15 (the probe, no VM): do start the probe at the host's loopback with three targets, a listener of
/// this test's, a port nothing holds and a name RFC 6761 §6.4 reserves as never resolving, then connect to it. Expect
/// its answer to read back typed: the port it came in on, then each target in turn, connected, refused and
/// unresolved. Each target is decided by this host at once (a listener, a closed port's reset, the resolver's
/// answer), so the probe's wait is its largest: the test waits on those facts, never on a guess.
#[test]
fn the_probe_answers_each_connection_with_its_targets_outcomes() {
    let open = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let args: Vec<std::ffi::OsString> = [
        "--wait-ms".to_owned(),
        u64::MAX.to_string(),
        "--address".to_owned(),
        "127.0.0.1".to_owned(),
        "--listen".to_owned(),
        "0".to_owned(),
        "--egress".to_owned(),
        format!("open={}", open.local_addr().unwrap()),
        "--egress".to_owned(),
        format!("closed={closed}"),
        "--egress".to_owned(),
        "nowhere=nowhere.invalid:80".to_owned(),
    ]
    .map(Into::into)
    .to_vec();
    let started = probe::start(&args).unwrap();
    let [at] = started.at.as_slice() else {
        panic!("{:?}", started.at);
    };
    let mut answer = String::new();
    TcpStream::connect(at)
        .unwrap()
        .read_to_string(&mut answer)
        .unwrap();
    let report = Report::parse(&answer).unwrap();
    assert_eq!(report.ingress, at.port());
    let outcomes: Vec<(&str, &Outcome)> = report
        .egress
        .iter()
        .map(|(target, outcome)| (target.label.as_str(), outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("open", &Outcome::Connected),
            ("closed", &Outcome::Refused),
            ("nowhere", &Outcome::Unresolved)
        ],
        "{answer}"
    );
    // Its servers run on for the VM's life; this test's end ends them.
    drop(started.servers);
}

/// Joint plan step 15 (the probe, no VM): do write a report of every outcome and read it back; expect it whole. Do
/// read reports with no ingress line first, an egress line short of a word, and an outcome no outcome is; expect
/// each refused, never read as another. Do read the probe's arguments short of each one it needs, with a wait of 0,
/// and with a flag it has no use for; expect each refused by name.
#[test]
fn the_probes_report_and_arguments_read_back_typed() {
    let target = |label: &str| probe::Target {
        label: label.to_owned(),
        at: "api.example:443".to_owned(),
    };
    let every = Report {
        ingress: 7101,
        egress: [
            Outcome::Connected,
            Outcome::Refused,
            Outcome::Unresolved,
            Outcome::Unreachable,
            Outcome::TimedOut,
            Outcome::Failed("PermissionDenied".to_owned()),
        ]
        .into_iter()
        .enumerate()
        .map(|(n, outcome)| (target(&format!("t{n}")), outcome))
        .collect(),
    };
    let text = every
        .egress
        .iter()
        .fold(format!("ingress {}\n", every.ingress), |text, (t, o)| {
            text + &format!("egress {} {} {o}\n", t.label, t.at)
        });
    assert_eq!(Report::parse(&text).unwrap(), every);
    for bad in [
        "",
        "egress a api.example:443 connected\n",
        "ingress 7101\negress a api.example:443\n",
        "ingress 7101\negress a api.example:443 connected-ish\n",
        "ingress 7101\negress a api.example:443 failed:\n",
    ] {
        assert!(Report::parse(bad).is_err(), "{bad:?}");
    }
    let read = |args: &[&str]| {
        let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        probe::Told::read(&args)
    };
    let whole = [
        "--wait-ms",
        "1000",
        "--address",
        "0.0.0.0",
        "--listen",
        "7101",
        "--egress",
        "provider=api.example:443",
    ];
    let told = read(&whole).unwrap();
    assert_eq!(
        (told.wait, told.address, told.listen, told.targets.len()),
        (
            Duration::from_secs(1),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            vec![7101],
            1
        )
    );
    let short_of_address = [&whole[..2], &whole[4..]].concat();
    let no_wait = ["--wait-ms", "0", "--address", "0.0.0.0", "--listen", "1"];
    let no_label = [&whole[..], &["--egress", "nolabel"]].concat();
    let unknown = [&whole[..], &["--shout", "x"]].concat();
    for (args, says) in [
        (&whole[2..], "--wait-ms"),
        (&short_of_address[..], "--address"),
        (&whole[..4], "--listen"),
        (&no_wait[..], "--wait-ms"),
        (&no_label[..], "--egress"),
        (&unknown[..], "--shout"),
    ] {
        let refused = read(args).unwrap_err();
        assert!(refused.contains(says), "{args:?}: {refused}");
    }
}

#[test]
#[ignore = "native: KVM, HVF or WHP, a Shards guest configuration and the probe image (HEX_GRANTS_CONTEXT or HEX_GRANTS_IMAGE); on Linux a delegated cgroup"]
fn a_vms_network_is_its_grants_alone_on_a_real_node() {
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let kernel = PathBuf::from(input("SHARDS_KERNEL"));
    assert!(kernel.is_file(), "SHARDS_KERNEL {kernel:?} is no file");
    // A kernel goes with its init in Shards (`--kernel` and `--init` together); the node's run hosts inherit both.
    let init = PathBuf::from(input("SHARDS_INIT"));
    assert!(init.is_file(), "SHARDS_INIT {init:?} is no file");
    let wait = Duration::from_millis(number("HEX_IMAGE_WAIT_MS"));
    assert!(!wait.is_zero(), "HEX_IMAGE_WAIT_MS is 0");
    let work = PathBuf::from(input("HEX_IMAGE_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    let image = match std::env::var_os("HEX_GRANTS_CONTEXT") {
        Some(context) => built(&shards, &PathBuf::from(context), &work, BUILT),
        None => {
            let image = input("HEX_GRANTS_IMAGE");
            ShardsImage::new(&image).check().unwrap();
            pull(&shards, &image, &work.join("state"));
            image
        }
    };
    ShardsImage::new(&image).check().unwrap();
    let probe = Probe {
        image,
        member: Member {
            kind: MemberKind::Harness,
            name: input("HEX_GRANTS_MEMBER"),
        },
        port: port("HEX_GRANTS_PORT"),
        ungranted_port: port("HEX_GRANTS_UNGRANTED_PORT"),
        provider: input("HEX_GRANTS_PROVIDER"),
        ungranted: input("HEX_GRANTS_UNGRANTED"),
        host_port: port("HEX_GRANTS_HOST_PORT"),
        open: input("HEX_GRANTS_OPEN"),
        command_port: port("HEX_GRANTS_COMMAND_PORT"),
    };
    // As image_real: a VM's start boots it, which the client's learned bounds (an OCI box's records) would refuse
    // first, so every wait measures, and the run's deadline bounds the whole.
    hex_core::patience::measure_all();
    assert!(
        hex_core::patience::census(step),
        "a census sink was installed before this one"
    );
    let allocation = allocation();
    eprintln!("GRANTS each VM {:?}", allocation.vm_size().unwrap());
    // On Linux the node's boxes go in a cgroup delegated to this binary, as image_real's do.
    #[cfg(target_os = "linux")]
    let boxes = {
        assert!(
            std::env::var_os("HEX_DELEGATED").is_some(),
            "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
        );
        Some(image_node::own_cgroup("hex-image-grants-real").join("hex-image-grants-node"))
    };
    #[cfg(not(target_os = "linux"))]
    let boxes: Option<PathBuf> = None;
    let node = Node::start(&work, boxes.as_deref(), wait);
    let (socket, pid) = (node.socket.clone(), node.child.id());
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let client = Client::connect(&socket).unwrap();
        assert!(
            client.classes().contains(&Class::Shards),
            "{:?}",
            client.classes()
        );
        a_destination_its_image_grants_not_refuses_the_boot(&client, &probe, allocation);
        no_grant_no_network(&client, pid, &probe, allocation);
        one_publish_published_alone(&client, pid, &probe, allocation);
        granted_destinations_are_reached_alone(&client, &probe, allocation);
        a_publish_reaching_the_runs_own_command_stops_it(&client, pid, &probe, allocation);
        let _ = done.send(client.list().unwrap());
    });
    let left = match finished.recv_timeout(wait) {
        Ok(left) => left,
        Err(RecvTimeoutError::Timeout) => panic!("the checks did not end within {wait:?}"),
        Err(RecvTimeoutError::Disconnected) => panic!("a check failed: its panic is above"),
    };
    drop(node);
    records(&work.join("census.txt"));
    assert!(left.is_empty(), "a box is left: {left:?}");
}
