//! What the host shows of a run (joint plan step 15, AC-T7.7.2; docs/design/network-grants.md N4): its processes,
//! under the one run host the node `node` started (`shards api-run`, D231), and the TCP and UDP sockets they hold.
//! One reader for each OS behind one interface (`os::processes`, `os::sockets`), each saying how it reads
//! ([`HOW`], told with every reading): /proc on Linux, `ps` and `lsof` on macOS, CIM's `Win32_Process` and the
//! NetTCPIP module's tables on Windows. What a reader can't read fails the test: what can't be seen isn't passed.

use std::net::SocketAddr;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod os;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod os;
#[cfg(windows)]
#[path = "windows.rs"]
mod os;

pub use os::HOW;

/// A process as the host shows it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    /// Its program's file name as the OS keeps it: Linux's `comm`, macOS's accounting name, Windows' image name
    /// without its `.exe`.
    pub name: String,
    /// Its command line's words, its program's first.
    pub args: Vec<String>,
}

/// A TCP or UDP socket a process holds, as the host shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub tcp: bool,
    pub local: SocketAddr,
    /// Its peer, where it has one.
    pub remote: Option<SocketAddr>,
    /// A TCP socket in its listening state.
    pub listening: bool,
    /// The processes holding it, each once, as the host shows them: on Linux and macOS each process whose descriptors
    /// hold it (a socket handed on is held by each), on Windows its owning process.
    pub pids: Vec<u32>,
}

/// `socket` added to `out`: its holders joined to the one already there with its protocol, addresses and state (the
/// same socket, shown for another process), else as one more. For the readers that name no socket's own identity
/// (Linux's names each by its inode).
#[cfg(not(target_os = "linux"))]
fn add(out: &mut Vec<Socket>, socket: Socket) {
    match out.iter_mut().find(|s| {
        (s.tcp, s.local, s.remote, s.listening)
            == (socket.tcp, socket.local, socket.remote, socket.listening)
    }) {
        Some(known) => {
            for pid in socket.pids {
                if !known.pids.contains(&pid) {
                    known.pids.push(pid);
                }
            }
        }
        None => out.push(socket),
    }
}

impl Socket {
    /// It takes what comes in: a listening TCP socket, or a UDP socket with no peer.
    pub fn listens(&self) -> bool {
        if self.tcp {
            self.listening
        } else {
            self.remote.is_none()
        }
    }

    /// It carries a flow to a peer past the host's own loopback.
    pub fn leaves(&self) -> bool {
        !self.listens()
            && self.remote.is_some_and(|peer| {
                let peer = peer.ip().to_canonical();
                !peer.is_loopback() && !peer.is_unspecified()
            })
    }
}

/// The roots and every process under them, each found by its parent, breadth first, with no recursion
/// (CLAUDE.md §11).
fn under(all: &[Process], roots: Vec<u32>) -> Vec<u32> {
    let mut found = roots;
    let mut at = 0;
    while let Some(&parent) = found.get(at) {
        found.extend(
            all.iter()
                .filter(|p| p.parent == parent && p.pid != parent)
                .map(|p| p.pid),
        );
        at += 1;
    }
    found
}

/// The run hosts under the node `node`: `shards api-run` (D231), its first argument after its program.
fn run_hosts(all: &[Process], node: u32) -> Vec<u32> {
    let of_node = under(all, vec![node]);
    all.iter()
        .filter(|p| of_node.contains(&p.pid) && p.args.get(1).is_some_and(|a| a == "api-run"))
        .map(|p| p.pid)
        .collect()
}

/// The run's processes: the one run host under the node `node` and every process under it.
fn run_of(node: u32) -> Vec<Process> {
    let all = os::processes();
    let hosts = run_hosts(&all, node);
    assert_eq!(hosts.len(), 1, "the node's run hosts ({HOW}): {hosts:?}");
    let pids = under(&all, hosts);
    let mut run: Vec<Process> = all.into_iter().filter(|p| pids.contains(&p.pid)).collect();
    run.sort_by_key(|p| p.pid);
    eprintln!("GRANTS-HOST read by {HOW}");
    for p in &run {
        eprintln!("GRANTS-PROCESS {} {} {:?}", p.pid, p.name, p.args);
    }
    run
}

/// The TCP and UDP sockets the run's processes hold, each once, each told with the processes holding it.
fn sockets_of(run: &[Process]) -> Vec<Socket> {
    let pids: Vec<u32> = run.iter().map(|p| p.pid).collect();
    let sockets = os::sockets(&pids);
    for s in &sockets {
        let holders: Vec<String> = s
            .pids
            .iter()
            .map(|pid| {
                let name = run
                    .iter()
                    .find(|p| p.pid == *pid)
                    .map_or("", |p| p.name.as_str());
                format!("{pid} {name}")
            })
            .collect();
        assert!(
            !holders.is_empty(),
            "a socket no process holds ({HOW}): {s:?}"
        );
        eprintln!(
            "GRANTS-SOCKET {} {} -> {} {} held by {}",
            if s.tcp { "tcp" } else { "udp" },
            s.local,
            s.remote
                .map_or_else(|| "none".to_owned(), |r| r.to_string()),
            if s.listens() { "listens" } else { "" },
            holders.join(", ")
        );
    }
    sockets
}

/// Whether `socket` is one of the run's agent-control endpoint's at `control` (`BoxInfo::control_sockets`: its served
/// address, its driver's waker and its wake): Hex's own control endpoint, which its run host binds on the host's
/// loopback (Shards' D238, D252), a QUIC endpoint on UDP sockets (D180), its waker and wake connected to each other, which
/// a host's table may show with no peer (Windows' shows none for any UDP socket).
fn is_control(socket: &Socket, control: &[SocketAddr; 3]) -> bool {
    !socket.tcp && control.contains(&socket.local)
}

/// The sockets of `sockets` that take what comes in past the run's agent-control endpoint's at `control`.
fn listening_past<'s>(sockets: &'s [Socket], control: &[SocketAddr; 3]) -> Vec<&'s Socket> {
    sockets
        .iter()
        .filter(|s| s.listens() && !is_control(s, control))
        .collect()
}

/// The run's agent-control endpoint's sockets of `control` that none of `sockets` is.
fn missing(sockets: &[Socket], control: &[SocketAddr; 3]) -> Vec<SocketAddr> {
    control
        .iter()
        .filter(|at| !sockets.iter().any(|s| !s.tcp && s.local == **at))
        .copied()
        .collect()
}

/// The run's sockets, read once, and those that take what comes in past its agent-control endpoint's at `control`,
/// each of which must be on the host's loopback. The run holds all three while it runs (Hex asks every run for an
/// endpoint, which its run host binds before the run starts, and Shards tells all three, image channel v30), so a
/// reading without one of them missed a socket it was told of, and fails: the endpoint's sockets are exactly that set.
fn read_past_control(run: &[Process], control: [SocketAddr; 3]) -> (Vec<Socket>, Vec<Socket>) {
    assert!(
        control
            .iter()
            .all(|at| at.ip().to_canonical().is_loopback()),
        "the run's agent-control endpoint at {control:?} is past the host's loopback"
    );
    let sockets = sockets_of(run);
    let missing = missing(&sockets, &control);
    assert!(
        missing.is_empty(),
        "the run's agent-control endpoint's sockets {missing:?} are none of its sockets ({HOW}): {sockets:#?}"
    );
    let past = listening_past(&sockets, &control)
        .into_iter()
        .cloned()
        .collect();
    (sockets, past)
}

/// How many of the run's processes are named `name`.
fn named(run: &[Process], name: &str) -> usize {
    run.iter().filter(|p| p.name == name).count()
}

/// A VM granted nothing whose image links its harness past the microVM: its run's VM process beside its run host,
/// and one network process (D250: the run keeps its network, the network process told nothing kept, so it lets
/// nothing past); no socket that takes what comes in but its run's agent-control endpoint's at `control`, all three of
/// which it holds; and no flow past the host's loopback.
pub fn granted_nothing(node: u32, control: [SocketAddr; 3]) {
    let run = run_of(node);
    assert_eq!(
        named(&run, "shards-vm"),
        1,
        "the run's VM process: {run:#?}"
    );
    assert_eq!(
        named(&run, "shards-net"),
        1,
        "the run's network process (D250): {run:#?}"
    );
    let (sockets, listening) = read_past_control(&run, control);
    assert!(
        listening.is_empty(),
        "the run of a VM granted nothing takes what comes in past its agent-control endpoint at {control:?}: \
         {listening:#?}"
    );
    let leaving: Vec<&Socket> = sockets.iter().filter(|s| s.leaves()).collect();
    assert!(
        leaving.is_empty(),
        "the run of a VM granted nothing holds a flow past the host's loopback: {leaving:#?}"
    );
}

/// A VM granted one publish, bound at `at`: its network process there (D36: so a VM granted nothing, which had
/// none, had a real one to lack), and its run listening at `at` alone past its agent-control endpoint's at `control`,
/// all three of which it holds: at no wildcard address and not on the image's port `ungranted`.
pub fn listens_alone(node: u32, at: SocketAddr, ungranted: u16, control: [SocketAddr; 3]) {
    let run = run_of(node);
    assert_eq!(
        named(&run, "shards-net"),
        1,
        "the run's network process: {run:#?}"
    );
    let (_, listening) = read_past_control(&run, control);
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
    let left = run_hosts(&os::processes(), node);
    assert!(
        left.is_empty(),
        "run hosts left under the node ({HOW}): {left:?}"
    );
}

/// The non-vacuity of this OS's reading (no VM), on every OS: do start a known child, `sort` waiting on its stdin
/// (a program each OS has), and hold, in this process, a TCP listener and a UDP socket on the loopback and a TCP
/// connection to the listener, and the connection the listener accepted. Expect the child found under this process
/// by its name; the listener (at its address, no peer) and the UDP socket listening; and the connection at its own
/// address, its peer the listener's, and the accepted one at the listener's, its peer the connection's, neither
/// listening nor leaving the host; each held by this process alone. Each socket is told by its address and its
/// peer's: the accepted connection shares the listener's address.
#[test]
fn the_hosts_reading_finds_a_known_child_and_a_processs_own_sockets() {
    let mut child = std::process::Command::new("sort")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let connection = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    let this = std::process::id();
    let all = os::processes();
    let sockets = os::sockets(&[this]);
    drop(child.stdin.take());
    let reaped = child.wait();
    eprintln!("GRANTS-HOST read by {HOW}: child {} {reaped:?}", child.id());
    let found = all
        .iter()
        .find(|p| p.pid == child.id())
        .unwrap_or_else(|| panic!("no process {} in this host's reading", child.id()));
    assert_eq!(
        (found.parent, found.name.as_str()),
        (this, "sort"),
        "{found:?}"
    );
    assert!(under(&all, vec![this]).contains(&child.id()));
    let at = |local: SocketAddr, remote: Option<SocketAddr>, tcp: bool| {
        let found = sockets
            .iter()
            .find(|s| s.local == local && s.remote == remote && s.tcp == tcp)
            .unwrap_or_else(|| {
                panic!("no socket at {local}, peer {remote:?} (tcp {tcp}) in {sockets:#?}")
            });
        assert_eq!(found.pids, [this], "{found:?}");
        found
    };
    let (listening, near) = (
        listener.local_addr().unwrap(),
        connection.local_addr().unwrap(),
    );
    assert!(at(listening, None, true).listens());
    assert!(at(udp.local_addr().unwrap(), None, false).listens());
    for (local, remote) in [(near, listening), (listening, near)] {
        let flow = at(local, Some(remote), true);
        assert!(!flow.listens() && !flow.leaves(), "{flow:?}");
    }
    drop(accepted);
}

/// What leaves the host, judged alike on every OS (no VM): do judge sockets whose peers are past the loopback, at it
/// (an IPv4-mapped one too) and none; expect only those past it to leave, and a listener never to.
#[test]
fn only_a_flow_past_the_loopback_leaves() {
    let tcp = |remote: Option<&str>, listening| Socket {
        tcp: true,
        local: "10.0.0.2:40000".parse().unwrap(),
        remote: remote.map(|r| r.parse().unwrap()),
        listening,
        pids: vec![1],
    };
    assert!(tcp(Some("192.0.2.1:443"), false).leaves());
    assert!(!tcp(Some("127.0.0.1:443"), false).leaves());
    assert!(!tcp(Some("[::ffff:127.0.0.1]:443"), false).leaves());
    assert!(!tcp(Some("[::1]:443"), false).leaves());
    assert!(!tcp(None, true).leaves());
    assert!(!tcp(None, false).leaves());
    let udp = |remote: Option<&str>| Socket {
        tcp: false,
        local: "127.0.0.1:5353".parse().unwrap(),
        remote: remote.map(|r| r.parse().unwrap()),
        listening: false,
        pids: vec![1],
    };
    assert!(udp(None).listens() && !udp(None).leaves());
    assert!(udp(Some("192.0.2.53:53")).leaves());
}

/// What takes what comes in past a run's agent-control endpoint, judged alike on every OS (no VM; joint plan step 15,
/// AC-T7.7.2; Shards' D252 amended, image channel v30): do judge, beside the endpoint at 127.0.0.1:43471 with its waker
/// at 46144 and its wake at 45708 (`BoxInfo::control_sockets`), its served UDP socket, its wake with no peer (as Windows'
/// table shows every UDP socket) and its waker with its peer (as Linux's and macOS's show it), another peerless UDP socket
/// on the loopback, a TCP listener at the endpoint's own address and port, and a TCP connection on the loopback. Expect
/// the other peerless UDP socket and the TCP listener alone to take what comes in past it, and the endpoint's three
/// sockets found; without its served socket, that one missing.
#[test]
fn a_runs_agent_control_endpoint_alone_takes_what_comes_in_past_its_grants() {
    let control: [SocketAddr; 3] = [
        "127.0.0.1:43471".parse().unwrap(),
        "127.0.0.1:46144".parse().unwrap(),
        "127.0.0.1:45708".parse().unwrap(),
    ];
    let socket = |tcp, local: &str, remote: Option<&str>, listening| Socket {
        tcp,
        local: local.parse().unwrap(),
        remote: remote.map(|r| r.parse().unwrap()),
        listening,
        pids: vec![1],
    };
    let stray = socket(false, "127.0.0.1:47000", None, false);
    let listener = socket(true, "127.0.0.1:43471", None, true);
    let sockets = [
        socket(false, "127.0.0.1:43471", None, false),
        socket(false, "127.0.0.1:45708", None, false),
        socket(false, "127.0.0.1:46144", Some("127.0.0.1:45708"), false),
        stray.clone(),
        listener.clone(),
        socket(true, "127.0.0.1:50000", Some("127.0.0.1:7101"), false),
    ];
    assert_eq!(listening_past(&sockets, &control), [&stray, &listener]);
    assert!(missing(&sockets, &control).is_empty());
    assert_eq!(missing(&sockets[1..], &control), [control[0]]);
}
