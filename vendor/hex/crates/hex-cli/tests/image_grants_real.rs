//! Joint plan step 15, AC-T7.7.2 and T-T7.7.1 (docs/design/network-grants.md N4 and N6): a Shards image VM's network
//! is its grants alone, on VMs a real `hex node` boots on Linux KVM. These are the booted-VM checks Hex's lead runs
//! beside goal 12's driver of the class's cases (`image_real.rs`), whose inputs, node and helpers they share
//! (`image_node`).
//!
//! - **No grant, no network.** A VM granted nothing runs a command, in its image's own shell, that ends 0 only where
//!   `lo` is its one interface: with no NIC, no name is answered and no packet leaves. A VM of its members alone
//!   granted nothing runs with no network process (Shards' `shards-net`, one per networked microVM, D36), and no
//!   process of its run holds a listening TCP or UDP socket.
//! - **One publish, published alone.** A VM of its members alone is granted one publish: its probe member's port at
//!   the host's loopback, on a host port the OS picks. Inspect shows that one grant and its one published port at
//!   127.0.0.1. Its run listens there alone: at no wildcard address, and not on the image's other port coming in. A
//!   connection there reaches the member, which says its own egress connected nowhere, and no process of the run
//!   holds a flow past the host's loopback.
//!
//! A run's processes are its run host (`shards api-run`, which the node starts, D231) and every process under it,
//! read from /proc: their names, and the TCP and UDP sockets their descriptors hold, from their network namespace's
//! tables. A descriptor list this test can't read fails it: what can't be seen isn't passed.
//!
//! Explicit inputs only, each by its own variable; a missing one fails by name. Run alone, `--ignored`, inside a
//! cgroup delegated to this binary (`HEX_DELEGATED`), with image_real's inputs (`HEX_IMAGE` with a `/bin/sh`,
//! `SHARDS_BIN`, `SHARDS_KERNEL`, the node's `HEX_GUEST_*`, `HEX_IMAGE_CPU_MILLI`, `HEX_IMAGE_MEMORY_BYTES`,
//! `HEX_IMAGE_WORK`, `HEX_IMAGE_WAIT_MS` and `HEX_WAITS_CENSUS_MAX`) and the probe's:
//! - `HEX_GRANTS_IMAGE`: the probe image, `NAME@sha256:...`, pulled by its digest into the node's Shards home before
//!   the node starts. It is an Agentfile image whose one harness, named `HEX_GRANTS_MEMBER`, it gives TCP ports
//!   `HEX_GRANTS_PORT` and `HEX_GRANTS_UNGRANTED_PORT` coming in (`NETWORK --ingress=<port> <network>`, and
//!   `EXPOSE --harnesses=<member> <port> AS ingress FOR <network>`, as Shards' `crates/shards/tests/api_image.rs`
//!   builds its ingress image), and whose own grants let it out (`NETWORK --egress=<port>/tcp`), which Hex narrows
//!   to nothing.
//! - The probe harness's program: as it starts, it tries one TCP connection past the VM on that egress port. Then it
//!   listens on `HEX_GRANTS_PORT` and answers each connection with one line, `egress refused` where its connection
//!   failed or `egress connected` where it was made, and closes it.
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_grants_real -- --ignored --nocapture` with those set.
#![cfg(target_os = "linux")]
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
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, BoxState, Class, ClassSpec, Exit, Grant, Member,
    MemberKind, Proto, Receiver, Runtime, ShardsImage, Signal,
};
use hex_client::Client;
use hex_core::patience::Patience;

#[path = "image_node/mod.rs"]
mod image_node;
use image_node::{Node, allocation, input, number, own_cgroup, pull, records, step};

/// What this test waits for, each fact its own: no recorded operation makes either, so each measures, for its fact
/// alone (hex_testkit::waits; the lead's rule of 2026-11-08), and the run's deadline bounds the whole.
static PROBE_ENDED: Patience = Patience::measuring("test.hex_cli.image_grants_probe_ended");
static MEMBER_ANSWERED: Patience = Patience::measuring("test.hex_cli.image_grants_member_answered");

/// TCP_LISTEN, a listening TCP socket's state in /proc/net's tables (include/net/tcp_states.h).
const TCP_LISTEN: u8 = 10;

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
    }
}

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
        // The name sits in parentheses and may hold any byte, so the state, then the parent, follow the last ')'.
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

/// The run's processes: the one run host under the node `node` (`shards api-run`, D231) and every process under it,
/// each found by its parent, breadth first, with no recursion (CLAUDE.md §11).
fn run_of(node: u32) -> Vec<Process> {
    let all = processes();
    let under = |roots: Vec<u32>| {
        let mut found = roots;
        let mut at = 0;
        while let Some(&parent) = found.get(at) {
            found.extend(all.iter().filter(|p| p.parent == parent).map(|p| p.pid));
            at += 1;
        }
        found
    };
    let of_node = under(vec![node]);
    let hosts: Vec<u32> = all
        .iter()
        .filter(|p| of_node.contains(&p.pid) && p.args.get(1).is_some_and(|a| a == "api-run"))
        .map(|p| p.pid)
        .collect();
    assert_eq!(hosts.len(), 1, "the node's run hosts: {hosts:?}");
    let pids = under(hosts);
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

/// An address as /proc/net's tables write it: the address's bytes as the host's 32-bit words in hex, then the port
/// in hex (net/ipv4/tcp_ipv4.c `get_tcp4_sock`, net/ipv6/tcp_ipv6.c `get_tcp6_sock`).
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

/// The TCP and UDP sockets of the network namespace the process `pid` is in, from its tables. A kernel with no IPv6
/// has no IPv6 tables, and so no IPv6 socket.
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

/// Joint plan step 15, T-T7.7.1: do run a VM granted nothing whose command, in its image's own shell, ends 0 only where
/// `lo` is its one interface; expect it to end 0. Do run one of its members alone granted nothing; expect its run to
/// hold no network process and no listening socket, the VM process found beside its run host.
fn no_grant_no_network(client: &Client, node: u32, image: &str, allocation: Allocation) {
    let only_lo = r#"set -- /sys/class/net/*; [ "$#" -eq 1 ] && [ "$1" = /sys/class/net/lo ]"#;
    let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), only_lo.to_owned()];
    let probe = image_box(
        "grants-none-probe",
        image,
        allocation,
        Some(command),
        Vec::new(),
    );
    let id = probe.id.clone();
    client.create(probe).unwrap();
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

    let held = image_box("grants-none-held", image, allocation, None, Vec::new());
    let id = held.id.clone();
    client.create(held).unwrap();
    client.start(&id).unwrap();
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
    eprintln!("GRANTS-CHECK-2 {id}: no network process, no listening socket");
    stop_and_delete(client, &id);
}

/// Joint plan step 15, AC-T7.7.2: do run a VM of its members alone granted one publish, its probe member's port at
/// the host's loopback, the OS picking the host's port. Expect: inspect shows that one grant and its one published
/// port at 127.0.0.1; the run listens there alone, at no wildcard address and not on the image's other port coming
/// in; a connection there reaches the member, whose own egress connected nowhere; and the run holds no flow past the
/// host's loopback.
fn one_publish_published_alone(client: &Client, node: u32, allocation: Allocation) {
    let image = input("HEX_GRANTS_IMAGE");
    let port = u16::try_from(number("HEX_GRANTS_PORT")).unwrap();
    let ungranted = u16::try_from(number("HEX_GRANTS_UNGRANTED_PORT")).unwrap();
    let publish = Grant::Publish {
        member: Member {
            kind: MemberKind::Harness,
            name: input("HEX_GRANTS_MEMBER"),
        },
        proto: Proto::Tcp,
        port,
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        host_port: None,
    };
    let spec = image_box(
        "grants-publish",
        &image,
        allocation,
        None,
        vec![publish.clone()],
    );
    let id = spec.id.clone();
    client.create(spec).unwrap();
    client.start(&id).unwrap();
    let shown = client.inspect(&id).unwrap();
    eprintln!(
        "GRANTS-CHECK-3 {id} granted {:?}, published {:?}",
        shown.grants, shown.published
    );
    assert_eq!(shown.grants, vec![publish]);
    let [published] = shown.published.as_slice() else {
        panic!("published ports: {:?}", shown.published);
    };
    assert_eq!(
        (published.grant, published.at.ip(), published.receiver),
        (0, IpAddr::V4(Ipv4Addr::LOCALHOST), Receiver::Declared)
    );
    let at = published.at;
    let run = run_of(node);
    // A publish gives the run its network process (D36): so the run granted nothing, which had none, had a real one
    // to lack.
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
    // The member answers once its program listens, after its egress try: each look is one connection read to its
    // end, which Shards closes where the member doesn't take it yet.
    let mut answer = String::new();
    let looks = hex_testkit::waits::until(
        &MEMBER_ANSWERED,
        &format!("the member's answer at {at}"),
        || {
            answer.clear();
            TcpStream::connect(at)
                .and_then(|mut c| c.read_to_string(&mut answer))
                .is_ok()
                && !answer.is_empty()
        },
    )
    .unwrap();
    eprintln!("GRANTS-CHECK-3 the member said {answer:?} after {looks} looks");
    assert_eq!(
        answer.trim_end(),
        "egress refused",
        "the member's own egress, which Hex narrows to nothing"
    );
    let leaving: Vec<Socket> = sockets_of(&run_of(node))
        .into_iter()
        .filter(Socket::leaves)
        .collect();
    assert!(
        leaving.is_empty(),
        "the run holds a flow past the host's loopback: {leaving:#?}"
    );
    eprintln!("GRANTS-CHECK-3 {id}: published at {at} alone, egress refused");
    stop_and_delete(client, &id);
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
/// loopback and a TCP connection to the listener; expect the reading to find each at its address, the listener and
/// the UDP socket listening, and the connection neither listening nor leaving the host. Do judge sockets whose peers
/// are past the loopback, at it and unset; expect only those past it to leave.
#[test]
fn the_proc_reading_finds_a_processs_own_sockets() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let connection = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
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

#[test]
#[ignore = "native: KVM, a delegated cgroup, a Shards guest configuration and pinned images (HEX_IMAGE, HEX_GRANTS_IMAGE)"]
fn a_vms_network_is_its_grants_alone_on_a_real_node() {
    assert!(
        std::env::var_os("HEX_DELEGATED").is_some(),
        "run inside a cgroup delegated to this binary (HEX_DELEGATED)"
    );
    let image = input("HEX_IMAGE");
    ShardsImage::new(&image).check().unwrap();
    let probe = input("HEX_GRANTS_IMAGE");
    ShardsImage::new(&probe).check().unwrap();
    let shards = PathBuf::from(input("SHARDS_BIN"));
    let kernel = PathBuf::from(input("SHARDS_KERNEL"));
    assert!(kernel.is_file(), "SHARDS_KERNEL {kernel:?} is no file");
    let wait = Duration::from_millis(number("HEX_IMAGE_WAIT_MS"));
    assert!(!wait.is_zero(), "HEX_IMAGE_WAIT_MS is 0");
    let work = PathBuf::from(input("HEX_IMAGE_WORK"));
    assert!(!work.exists(), "a run's own directory: {work:?} exists");
    fs::create_dir_all(work.join("state")).unwrap();
    // As image_real: a VM's start boots it, which the client's learned bounds (an OCI box's records) would refuse
    // first, so every wait measures, and the run's deadline bounds the whole.
    hex_core::patience::measure_all();
    assert!(
        hex_core::patience::census(step),
        "a census sink was installed before this one"
    );
    let allocation = allocation();
    eprintln!("GRANTS each VM {:?}", allocation.vm_size().unwrap());
    let own = own_cgroup("hex-image-grants-real");
    pull(&shards, &image, &work.join("state"));
    pull(&shards, &probe, &work.join("state"));
    let node = Node::start(&work, &own, wait, "hex-image-grants-node");
    let (socket, pid) = (node.socket.clone(), node.child.id());
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let client = Client::connect(&socket).unwrap();
        assert!(
            client.classes().contains(&Class::Shards),
            "{:?}",
            client.classes()
        );
        no_grant_no_network(&client, pid, &image, allocation);
        one_publish_published_alone(&client, pid, allocation);
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
