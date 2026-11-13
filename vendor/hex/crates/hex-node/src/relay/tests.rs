//! The guest relay's tests (docs/design/shards-guest.md §3, HEX-GUEST-6 and HEX-GUEST-7; native plan
//! step 5): a host's guest client, through the relay, reaches a stand-in node agent's own socket; its
//! calls and answers cross whole; an exec's streams reach the node as pipe ends with the exec; and the
//! relay's bounds refuse by type: an exec whose stream never comes, and a connection past the bound.
//! And its staging port (§4a, HEX-GUEST-14): a host tree staged lands under the box's staged root as it
//! was, and goes when removed; a stage that names its way out, or comes twice, is refused and leaves
//! nothing behind.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use super::*;
use hex_api::{BoxId, Class, Exit, Program, Runtime, Stdio};
use hex_client::guest::{Client, Link};
use hex_core::stream::files as hf;
use hex_core::wire::{Header, Hello, Received};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::FileType;

/// The guest's ports as Unix sockets: the relay's control and stdio listeners, or a stdio socket nobody
/// serves from (`stdio` set apart).
struct Ports {
    control: PathBuf,
    stdio: PathBuf,
    files: PathBuf,
}

impl Link for Ports {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        let path = match port {
            stream::CONTROL_PORT => &self.control,
            stream::STDIO_PORT => &self.stdio,
            stream::files::FILES_PORT => &self.files,
            other => panic!("dialed port {other:#x}"),
        };
        hex_client::guest::own_duplex(UnixStream::connect(path).unwrap())
    }
}

struct Lab {
    dir: tempfile::TempDir,
    node: OwnedFd,
}

impl Lab {
    /// A stand-in node's listening socket and a relay serving it with `limits`.
    fn new(limits: Limits) -> Self {
        let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/relay");
        std::fs::create_dir_all(&cache).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("relay-")
            .tempdir_in(&cache)
            .unwrap();
        let node_path = dir.path().join("node.sock");
        let node = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        rustix::net::bind(&node, &SocketAddrUnix::new(&node_path).unwrap()).unwrap();
        rustix::net::listen(&node, 4).unwrap();
        let control = UnixListener::bind(dir.path().join("control.sock")).unwrap();
        let stdio = UnixListener::bind(dir.path().join("stdio.sock")).unwrap();
        let files = UnixListener::bind(dir.path().join("files.sock")).unwrap();
        let staged = dir.path().join("staged");
        std::fs::create_dir(&staged).unwrap();
        let relay = Relay::new(&node_path, &staged, limits);
        std::thread::spawn(move || {
            relay.serve(Box::new(control), Box::new(stdio), Box::new(files))
        });
        Self { dir, node }
    }

    fn link(&self) -> hex_client::guest::LinkOwner {
        hex_client::guest::own_link(Ports {
            control: self.dir.path().join("control.sock"),
            stdio: self.dir.path().join("stdio.sock"),
            files: self.dir.path().join("files.sock"),
        })
        .unwrap()
    }
}

/// The next request on the node's side of a connection, with the descriptors that came with it.
fn next_request(conn: &OwnedFd) -> (Header, Request, Vec<OwnedFd>) {
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        conn,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )
    .unwrap();
    let fds: Vec<OwnedFd> = control
        .drain()
        .flat_map(|m| match m {
            RecvAncillaryMessage::ScmRights(fds) => fds.collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    let received = Received {
        bytes: &buf[..msg.bytes],
        size: msg.bytes,
        descriptors: fds.len(),
        descriptors_cut: false,
    };
    let (h, r) = wire::read_request(&received, wire::MAX_MESSAGE).unwrap();
    (h, r, fds)
}

fn answer(conn: &OwnedFd, h: Header, answer: &Answer) {
    let bytes = wire::answer(h.id, answer, wire::MAX_MESSAGE).unwrap();
    rustix::net::send(conn, &bytes, SendFlags::NOSIGNAL).unwrap();
}

/// A node's bounds as this host's platform gives them ([`crate::serve::limits`]): what the stand-in node's hello
/// tells.
fn node_limits() -> wire::Limits {
    crate::serve::limits(&crate::sizes::Sizes::now().unwrap().platform())
}

/// The stand-in node's budgets over all its connections: its bounds' own.
fn budgets() -> wire::Budgets {
    let node = node_limits();
    wire::Budgets {
        in_flight: node.in_flight,
        programs: node.programs,
    }
}

/// The relay's bounds over the stand-in node, as its hello tells them ([`Limits::of`]).
fn limits() -> Limits {
    Limits::of(&node_limits(), &budgets())
}

/// Accepts a connection and answers its hello.
fn hello(node: &OwnedFd) -> OwnedFd {
    let conn = rustix::net::accept_with(node, SocketFlags::CLOEXEC).unwrap();
    let (h, r, _) = next_request(&conn);
    assert!(matches!(r, Request::Hello { .. }), "{r:?}");
    answer(
        &conn,
        h,
        &Answer::Hello(Hello {
            build: wire::build(),
            classes: vec![Class::Oci],
            limits: node_limits(),
            budgets: budgets(),
        }),
    );
    conn
}

/// Whether a message, or its end, is waiting on `conn` now.
fn readable(conn: impl AsFd) -> bool {
    let mut ready = [PollFd::new(&conn, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut ready, Some(&now)).unwrap() == 1
}

/// HEX-GUEST-6: connect through the relay and inspect a box the node doesn't hold: expect the node's
/// classes and its refusal, typed, as the node's own socket gives them.
#[test]
fn a_hosts_calls_reach_the_node_and_its_answers_come_back() {
    let lab = Lab::new(limits());
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, r, fds) = next_request(&conn);
        assert!(fds.is_empty());
        let Request::Call(Call::Inspect(id)) = r else {
            panic!("expected an inspect, got {r:?}")
        };
        answer(&conn, h, &Answer::Refused(Refusal::NoSuchBox(id)));
    });
    let client = Client::connect(lab.link()).unwrap();
    assert_eq!(client.classes(), vec![Class::Oci]);
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b)));
    agent.join().unwrap();
}

/// HEX-GUEST-6: exec a program with stdin and stdout through the relay: expect the node to get the exec
/// with two descriptors, each a pipe's end the program can use, the bytes the host writes to stdin read
/// from the one, the bytes written to the other read back by the host to its end, and the end as the
/// node sent it.
#[test]
fn an_execs_streams_reach_the_node_as_pipe_ends() {
    let lab = Lab::new(limits());
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, r, fds) = next_request(&conn);
        assert!(matches!(r, Request::Call(Call::Exec { .. })), "{r:?}");
        assert_eq!(fds.len(), 2);
        for fd in &fds {
            let st = rustix::fs::fstat(fd).unwrap();
            assert_eq!(FileType::from_raw_mode(st.st_mode), FileType::Fifo);
        }
        answer(&conn, h, &Answer::Started { pid: 7 });
        let mut fds = fds.into_iter();
        let mut stdin = File::from(fds.next().unwrap());
        let mut stdout = File::from(fds.next().unwrap());
        let mut got = String::new();
        stdin.read_to_string(&mut got).unwrap();
        stdout
            .write_all(format!("pong to {got}").as_bytes())
            .unwrap();
        drop(stdout);
        answer(&conn, h, &Answer::Exited(Exit::Code(3)));
        conn
    });
    let client = Client::connect(lab.link()).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    assert_eq!(proc.pid(), 7);
    in_w.write_all(b"ping").unwrap();
    drop(in_w);
    let mut out = String::new();
    out_r.read_to_string(&mut out).unwrap();
    assert_eq!(out, "pong to ping");
    assert_eq!(
        proc.wait_for(Duration::from_secs(30)).unwrap(),
        Some(Exit::Code(3))
    );
    drop(agent.join().unwrap());
}

use std::time::Duration;

/// A host connection that says each time its client writes to it.
struct Told {
    conn: UnixStream,
    wrote: std::sync::mpsc::Sender<()>,
}

impl Read for Told {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.conn.read(buf)
    }
}

impl Write for Told {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.conn.write(buf)?;
        let _ = self.wrote.send(());
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

impl Duplex for Told {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        hex_client::guest::own_duplex(Told {
            conn: self.conn.try_clone()?,
            wrote: self.wrote.clone(),
        })
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.conn.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_read_timeout(timeout)
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        std::os::fd::AsFd::as_fd(&self.conn)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.conn.set_nonblocking(on)
    }
}

/// The relay's control port, its client's writes told and the connection held for the test to end;
/// stdio to `stdio`.
struct Held {
    control: PathBuf,
    stdio: PathBuf,
    wrote: Mutex<std::sync::mpsc::Sender<()>>,
    held: Arc<Mutex<Option<UnixStream>>>,
}

impl Link for Held {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        if port != stream::CONTROL_PORT {
            return hex_client::guest::own_duplex(UnixStream::connect(&self.stdio).unwrap());
        }
        let conn = UnixStream::connect(&self.control).unwrap();
        *self.held.lock().unwrap() = Some(conn.try_clone().unwrap());
        hex_client::guest::own_duplex(Told {
            conn,
            wrote: self.wrote.lock().unwrap().clone(),
        })
    }
}

/// An exec of `cat` with stdin alone, through `client`.
fn exec_stdin(client: &Client) -> Result<Box<dyn hex_api::Proc>, Refusal> {
    let (in_r, _in_w) = std::io::pipe().unwrap();
    client.exec(
        &BoxId::new("b").unwrap(),
        Program::of(&["cat"]),
        Stdio {
            stdin: Some(in_r),
            stdout: None,
            stderr: None,
            ..Stdio::null()
        },
    )
}

/// HEX-GUEST-7 (CLAUDE.md §4: facts, not clocks): on a relay of one set, exec with a stdin whose
/// connection never reaches the relay, then end the host's connection: expect the exec never sent to
/// the node, its set gone with its host (no clock), so a second host's exec takes the one set and runs.
#[test]
fn an_exec_whose_stream_never_comes_waits_for_its_host_and_goes_with_it() {
    let lab = Lab::new(Limits {
        sets: 1,
        ..limits()
    });
    // Stdio goes to a socket nobody serves from: it connects, and nothing reads it.
    let nowhere = lab.dir.path().join("nowhere.sock");
    let nowhere_listener = UnixListener::bind(&nowhere).unwrap();
    let (wrote, writes) = std::sync::mpsc::channel();
    let held = Arc::new(Mutex::new(None));
    let link = hex_client::guest::own_link(Held {
        control: lab.dir.path().join("control.sock"),
        stdio: nowhere,
        wrote: Mutex::new(wrote),
        held: Arc::clone(&held),
    })
    .unwrap();
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let first = hello(&node);
        // The host's end reaches the node as its connection's end, with no exec before it.
        let mut buf = [0u8; 64];
        assert_eq!(rustix::io::read(&first, &mut buf).unwrap(), 0);
        let second = hello(&node);
        let (h, r, fds) = next_request(&second);
        assert!(matches!(r, Request::Call(Call::Exec { .. })), "{r:?}");
        assert_eq!(fds.len(), 1);
        answer(&second, h, &Answer::Started { pid: 8 });
        answer(&second, h, &Answer::Exited(Exit::Code(0)));
        second
    });
    let first = Client::connect(link.clone()).unwrap();
    while writes.try_recv().is_ok() {}
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| exec_stdin(&first).map(|_| ()));
        // The client dials its stream, then writes its exec: once written, the relay reads it before
        // the end that follows on the same connection.
        let _stream = nowhere_listener.accept().unwrap();
        writes.recv().unwrap();
        let host = held.lock().unwrap().take().unwrap();
        host.shutdown(Shutdown::Write).unwrap();
        assert!(matches!(waiting.join().unwrap(), Err(Refusal::Broken(_))));
    });
    let second = Client::connect(lab.link()).unwrap();
    let mut proc = exec_stdin(&second).unwrap();
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    drop(agent.join().unwrap());
}

/// HEX-GUEST-7: connect a stdio connection that never names itself, then exec with stdout through the
/// relay: expect the exec's stream named and its output read while the silent one waits, and the silent
/// one still open after (it waits on its host, not a clock).
#[test]
fn a_silent_stdio_connection_holds_no_other_exec_up() {
    let lab = Lab::new(limits());
    let silent = UnixStream::connect(lab.dir.path().join("stdio.sock")).unwrap();
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, _, fds) = next_request(&conn);
        answer(&conn, h, &Answer::Started { pid: 9 });
        let mut stdout = File::from(fds.into_iter().next().unwrap());
        stdout.write_all(b"named").unwrap();
        drop(stdout);
        answer(&conn, h, &Answer::Exited(Exit::Code(0)));
        conn
    });
    let client = Client::connect(lab.link()).unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["echo"]),
            Stdio {
                stdin: None,
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut out = String::new();
    out_r.read_to_string(&mut out).unwrap();
    assert_eq!(out, "named");
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    assert!(!readable(&silent), "the silent connection was closed");
    drop(agent.join().unwrap());
}

/// A lab as [`Lab::new`] makes it, with the relay kept for the test to read.
fn kept_lab(limits: Limits) -> (Lab, Arc<Relay>) {
    kept_lab_with(limits, |stdio| Box::new(stdio))
}

/// A kept lab whose stdio listener is `stdio` of the plain one.
fn kept_lab_with(
    limits: Limits,
    stdio: impl FnOnce(UnixListener) -> Box<dyn Listen>,
) -> (Lab, Arc<Relay>) {
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/relay");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("relay-")
        .tempdir_in(&cache)
        .unwrap();
    let node_path = dir.path().join("node.sock");
    let node = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&node, &SocketAddrUnix::new(&node_path).unwrap()).unwrap();
    rustix::net::listen(&node, 4).unwrap();
    let control = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let stdio = stdio(UnixListener::bind(dir.path().join("stdio.sock")).unwrap());
    let files = UnixListener::bind(dir.path().join("files.sock")).unwrap();
    let staged = dir.path().join("staged");
    std::fs::create_dir(&staged).unwrap();
    let relay = Relay::new(&node_path, &staged, limits);
    let served = Arc::clone(&relay);
    std::thread::spawn(move || served.serve(Box::new(control), stdio, Box::new(files)));
    (Lab { dir, node }, relay)
}

/// Reads `look` until it holds, re-reading after as long as the last read took (step 7's learned re-read).
fn until(mut look: impl FnMut() -> bool) {
    loop {
        let read = std::time::Instant::now();
        if look() {
            return;
        }
        let took = read.elapsed();
        let end = std::time::Instant::now() + took;
        while std::time::Instant::now() < end {
            std::thread::park_timeout(end - std::time::Instant::now());
        }
    }
}

/// HEX-GUEST-7, HEX-RT-17: end a control before its stdio names arrive; expect no recreated set, waiter or
/// socket. A second active control still accepts an early stream and passes its bytes to the node's pipe.
#[test]
fn late_streams_cannot_recreate_a_closed_controls_sets() {
    let (lab, relay) = kept_lab(Limits {
        sets: 1,
        ..limits()
    });
    let mut first = UnixStream::connect(lab.dir.path().join("control.sock")).unwrap();
    let mut token = [0u8; stream::TOKEN_BYTES];
    first.read_exact(&mut token).unwrap();
    let first_token = u64::from_le_bytes(token);
    let first_node = rustix::net::accept_with(&lab.node, SocketFlags::CLOEXEC).unwrap();
    drop(first);
    // Control cleanup precedes its shutdown of the node connection: EOF proves forget already ran.
    assert_eq!(rustix::io::read(&first_node, &mut [0u8; 1]).unwrap(), 0);
    assert!(relay.sets().unwrap().gather.is_empty());
    assert!(!relay.sets().unwrap().active.contains(&first_token));

    let (mut late, incoming) = UnixStream::pair().unwrap();
    relay.unnamed.fetch_add(1, Ordering::AcqRel);
    let receiving = Arc::clone(&relay);
    let late_thread = std::thread::spawn(move || receiving.stream(Box::new(incoming)));
    late.write_all(&stream::name(stream::Named {
        exec: Exec {
            conn: first_token,
            id: 7,
        },
        stream: stream::Stdio::In,
    }))
    .unwrap();
    until(|| late_thread.is_finished() || !relay.sets().unwrap().gather.is_empty());
    let retained = !relay.sets().unwrap().gather.is_empty();
    if retained {
        // The red repro releases and joins its own retained waiter before asserting the root cause.
        drop(relay.sets().unwrap().gather.forget(first_token));
    }
    late_thread.join().unwrap();
    assert_eq!(late.read(&mut [0u8; 1]).unwrap(), 0);
    assert!(relay.sets().unwrap().parked.is_empty());
    assert_eq!(relay.unnamed.load(Ordering::Acquire), 0);
    assert!(
        !retained,
        "late stdio recreated a set for the ended control"
    );

    let mut active = UnixStream::connect(lab.dir.path().join("control.sock")).unwrap();
    active.read_exact(&mut token).unwrap();
    let active_token = u64::from_le_bytes(token);
    assert_ne!(first_token, active_token);
    assert!(relay.sets().unwrap().active.contains(&active_token));
    let active_node = rustix::net::accept_with(&lab.node, SocketFlags::CLOEXEC).unwrap();
    let (mut early, incoming) = UnixStream::pair().unwrap();
    relay.unnamed.fetch_add(1, Ordering::AcqRel);
    let receiving = Arc::clone(&relay);
    let early_thread = std::thread::spawn(move || receiving.stream(Box::new(incoming)));
    let exec = Exec {
        conn: active_token,
        id: 9,
    };
    early
        .write_all(&stream::name(stream::Named {
            exec,
            stream: stream::Stdio::In,
        }))
        .unwrap();
    until(|| early_thread.is_finished() || !relay.sets().unwrap().gather.is_empty());
    assert!(
        !relay.sets().unwrap().gather.is_empty(),
        "an active control's early stream was refused"
    );
    let request = wire::request(
        exec.id,
        &Request::Call(Call::Exec {
            id: BoxId::new("active").unwrap(),
            program: Program::of(&["cat"]),
            streams: wire::Streams {
                stdin: true,
                stdout: false,
                stderr: false,
            },
        }),
        wire::MAX_MESSAGE,
    )
    .unwrap();
    active
        .write_all(&stream::frame(&request, wire::MAX_MESSAGE).unwrap())
        .unwrap();
    let (header, request, mut fds) = next_request(&active_node);
    assert_eq!(header.id, exec.id);
    assert!(matches!(request, Request::Call(Call::Exec { .. })));
    assert_eq!(fds.len(), 1);
    early.write_all(b"still active").unwrap();
    early.shutdown(Shutdown::Write).unwrap();
    let mut bytes = String::new();
    File::from(fds.pop().unwrap())
        .read_to_string(&mut bytes)
        .unwrap();
    assert_eq!(bytes, "still active");
    early_thread.join().unwrap();
    assert!(relay.sets().unwrap().gather.is_empty());
    assert!(relay.sets().unwrap().parked.is_empty());
    drop(active);
    assert_eq!(rustix::io::read(&active_node, &mut [0u8; 1]).unwrap(), 0);
    until(|| relay.connections.load(Ordering::Acquire) == 0);
    assert_eq!(relay.connections.load(Ordering::Acquire), 0);
    assert!(relay.sets().unwrap().active.is_empty());
}

/// A stdio connection whose name reaches the relay only once its exec is parked there.
struct Late {
    conn: UnixStream,
    relay: Arc<Relay>,
}

impl Read for Late {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.conn.read(buf)
    }
}

impl Write for Late {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (mut conn, relay, name) = (
            self.conn.try_clone()?,
            Arc::clone(&self.relay),
            buf.to_vec(),
        );
        // The name is the connection's first write: held until the exec it names is parked.
        std::thread::spawn(move || {
            until(|| relay.sets().map(|s| !s.parked.is_empty()).unwrap_or(false));
            conn.write_all(&name).unwrap();
        });
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Duplex for Late {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        hex_client::guest::own_duplex(self.conn.try_clone()?)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.conn.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_read_timeout(timeout)
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        std::os::fd::AsFd::as_fd(&self.conn)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.conn.set_nonblocking(on)
    }
}

/// The relay's ports, each stdio connection [`Late`].
struct LatePorts {
    dir: PathBuf,
    relay: Arc<Relay>,
}

impl Link for LatePorts {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        if port == stream::CONTROL_PORT {
            return hex_client::guest::own_duplex(
                UnixStream::connect(self.dir.join("control.sock")).unwrap(),
            );
        }
        hex_client::guest::own_duplex(Late {
            conn: UnixStream::connect(self.dir.join("stdio.sock")).unwrap(),
            relay: Arc::clone(&self.relay),
        })
    }
}

/// HEX-GUEST-7: exec with stdin whose connection names itself only once the exec is parked at the relay:
/// expect the parked exec to reach the node with its pipe as soon as its set is whole, and to run.
#[test]
fn a_parked_exec_goes_to_the_node_when_its_set_is_whole() {
    let (lab, relay) = kept_lab(limits());
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, r, fds) = next_request(&conn);
        assert!(matches!(r, Request::Call(Call::Exec { .. })), "{r:?}");
        assert_eq!(fds.len(), 1);
        answer(&conn, h, &Answer::Started { pid: 10 });
        answer(&conn, h, &Answer::Exited(Exit::Code(0)));
        conn
    });
    let client = Client::connect(
        hex_client::guest::own_link(LatePorts {
            dir: lab.dir.path().to_owned(),
            relay: Arc::clone(&relay),
        })
        .unwrap(),
    )
    .unwrap();
    let mut proc = exec_stdin(&client).unwrap();
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    assert!(relay.sets().unwrap().parked.is_empty());
    drop(agent.join().unwrap());
}

/// What the relay moved through user space on its stdio connections: bytes read and writes made.
#[derive(Default)]
struct Moved {
    read: AtomicUsize,
    writes: AtomicUsize,
}

/// The relay's stdio listener, counting what the relay moves through user space on its connections.
struct Counting {
    listener: UnixListener,
    moved: Arc<Moved>,
}

impl Listen for Counting {
    fn accept(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(CountingConn {
            conn: self.listener.accept()?.0,
            moved: Arc::clone(&self.moved),
        }))
    }
}

struct CountingConn {
    conn: UnixStream,
    moved: Arc<Moved>,
}

impl Read for CountingConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.conn.read(buf)?;
        self.moved.read.fetch_add(n, Ordering::SeqCst);
        Ok(n)
    }
}

impl Write for CountingConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.moved.writes.fetch_add(1, Ordering::SeqCst);
        self.conn.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

impl AsFd for CountingConn {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }
}

impl Duplex for CountingConn {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        hex_client::guest::own_duplex(CountingConn {
            conn: self.conn.try_clone()?,
            moved: Arc::clone(&self.moved),
        })
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.conn.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_read_timeout(timeout)
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        std::os::fd::AsFd::as_fd(&self.conn)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.conn.set_nonblocking(on)
    }
}

/// HEX-GUEST-36 (no copy through user space, splice(2)): exec `cat` with 1 MiB on its stdin read back from
/// its stdout: expect every byte to cross the relay by splice, the relay reading nothing from its stdio
/// connections but each one's name and writing nothing to them, where a copy through a buffer reads and
/// writes every byte.
#[test]
fn stdio_crosses_the_relay_without_a_copy_through_user_space() {
    const MIB: usize = 1 << 20;
    let moved = Arc::new(Moved::default());
    let counted = Arc::clone(&moved);
    let (lab, _relay) = kept_lab_with(limits(), move |listener| {
        Box::new(Counting {
            listener,
            moved: counted,
        })
    });
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, _, fds) = next_request(&conn);
        answer(&conn, h, &Answer::Started { pid: 11 });
        let mut fds = fds.into_iter();
        let mut stdin = File::from(fds.next().unwrap());
        let mut stdout = File::from(fds.next().unwrap());
        std::io::copy(&mut stdin, &mut stdout).unwrap();
        drop(stdout);
        answer(&conn, h, &Answer::Exited(Exit::Code(0)));
        conn
    });
    let client = Client::connect(lab.link()).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    let writer = std::thread::spawn(move || {
        in_w.write_all(&vec![7u8; MIB]).unwrap();
    });
    let mut got = Vec::new();
    out_r.read_to_end(&mut got).unwrap();
    writer.join().unwrap();
    assert_eq!(got.len(), MIB);
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    assert_eq!(moved.read.load(Ordering::SeqCst), 2 * stream::NAME_BYTES);
    assert_eq!(moved.writes.load(Ordering::SeqCst), 0);
    drop(agent.join().unwrap());
}

/// HEX-GUEST-7 (CLAUDE.md §2): on a relay bounded at two unnamed stdio connections, connect three that
/// never name themselves: expect the third refused by type and counted, then closed (its end is what it
/// reads), the two within the bound still open.
#[test]
fn an_unnamed_connection_past_the_bound_is_refused() {
    let (lab, relay) = kept_lab(Limits {
        unnamed: 2,
        ..limits()
    });
    let path = lab.dir.path().join("stdio.sock");
    let held = [
        UnixStream::connect(&path).unwrap(),
        UnixStream::connect(&path).unwrap(),
    ];
    let mut past = UnixStream::connect(&path).unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(past.read(&mut byte).unwrap(), 0);
    // Refused by type, counted and logged on `hex.guest` (event `stdio_unnamed`) before it closed.
    assert_eq!(relay.refused_unnamed(), 1);
    for conn in &held {
        assert!(!readable(conn), "a connection within the bound was closed");
    }
}

/// HEX-GUEST-7: connect twice to a relay bounded at one connection: expect the second refused by type,
/// naming the bound, as the node's own socket refuses one past its own.
#[test]
fn a_connection_past_the_bound_is_refused() {
    let lab = Lab::new(Limits {
        connections: 1,
        ..limits()
    });
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || hello(&node));
    let _first = Client::connect(lab.link()).unwrap();
    let held = agent.join().unwrap();
    assert_eq!(
        Client::connect(lab.link()).err(),
        Some(Refusal::Exhausted {
            what: crate::serve::CONNECTIONS,
            bound: 1,
        })
    );
    drop(held);
}

/// HEX-GUEST-7 (Ada's rule, 2026-11-06: no picked bound): a relay's bounds are its node's, asked in a hello of their
/// own. Do ask the stand-in node, whose hello tells this host's platform's bounds and budgets; expect its
/// connections, as many sets as its calls in flight together, and [`STREAMS`] unnamed connections a set.
#[test]
fn a_relays_bounds_are_its_nodes_as_its_hello_tells_them() {
    let lab = Lab::new(limits());
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || hello(&node));
    let asked = Limits::asked(&lab.dir.path().join("node.sock"), None).unwrap();
    drop(agent.join().unwrap());
    let (node, budgets) = (node_limits(), budgets());
    assert_eq!(asked.connections, node.connections);
    assert_eq!(asked.sets, budgets.in_flight);
    assert_eq!(asked.unnamed, budgets.in_flight.saturating_mul(STREAMS));
}

fn staged_lab() -> (Lab, PathBuf) {
    let lab = Lab::new(limits());
    let staged = lab.dir.path().join("staged");
    (lab, staged)
}

/// Sends `bytes` raw on the staging port and returns the relay's answer.
fn stage_raw(lab: &Lab, bytes: &[u8]) -> Answer {
    let mut conn = UnixStream::connect(lab.dir.path().join("files.sock")).unwrap();
    let _ = conn.write_all(bytes);
    conn.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    conn.read_to_end(&mut back).unwrap();
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    let mut rest = &back[..];
    // A version-2 stage's budget comes first; the answer is the message after it.
    let message = loop {
        let (took, whole) = d.take(rest).unwrap();
        rest = &rest[took..];
        if let Some(m) = whole
            && hf::read_budget(&m).is_none()
        {
            break m;
        }
    };
    let received = Received {
        bytes: &message,
        size: message.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_answer(&received, wire::MAX_MESSAGE).unwrap().1
}

/// HEX-GUEST-14: stage a host tree (a folder, a file with mode 0750, an empty file, a symlink) and a
/// bytes item through the host's packer: expect each under `<staged>/box-1/` as it was, modes kept,
/// the root 0755; then remove it: expect it gone, and a second removal refused as no such box.
#[test]
fn a_staged_tree_lands_as_it_was_and_goes_when_removed() {
    use hex_client::guest::stage::{Item, Source, stage, unstage};
    use std::os::unix::fs::PermissionsExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let root = host.path().join("rootfs");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/sh"), b"#!sh\n").unwrap();
    std::fs::set_permissions(root.join("bin/sh"), std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::set_permissions(root.join("bin"), std::fs::Permissions::from_mode(0o555)).unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    std::os::unix::fs::symlink("/usr/lib", root.join("lib")).unwrap();
    let id = BoxId::new("box-1").unwrap();
    let items = [
        Item {
            name: "root".to_owned(),
            source: Source::Path(root.clone()),
        },
        Item {
            name: "config.json".to_owned(),
            source: Source::Bytes(b"{}".to_vec()),
        },
    ];
    let link = lab.link();
    stage(link.as_ref(), &id, &items, &hf::LIMITS).unwrap();
    let copy = staged.join("box-1");
    let mode = |p: &str| {
        std::fs::symlink_metadata(copy.join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(""), 0o755);
    assert_eq!(std::fs::read(copy.join("root/bin/sh")).unwrap(), b"#!sh\n");
    assert_eq!(mode("root/bin/sh"), 0o750);
    assert_eq!(mode("root/bin"), 0o555);
    assert_eq!(std::fs::read(copy.join("root/empty")).unwrap(), b"");
    assert_eq!(
        std::fs::read_link(copy.join("root/lib")).unwrap(),
        Path::new("/usr/lib")
    );
    assert_eq!(std::fs::read(copy.join("config.json")).unwrap(), b"{}");
    // A box id staged already is refused, its copy untouched.
    assert_eq!(
        stage(link.as_ref(), &id, &items, &hf::LIMITS),
        Err(Refusal::Exists(id.clone()))
    );
    assert!(copy.join("root/bin/sh").exists());
    unstage(link.as_ref(), &id).unwrap();
    assert!(!copy.exists());
    assert_eq!(unstage(link.as_ref(), &id), Err(Refusal::NoSuchBox(id)));
    std::fs::set_permissions(root.join("bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The raw stage of `entries` (path, kind, target) for `b`.
fn raw_stage(entries: &[(&str, hf::What)]) -> Vec<u8> {
    let mut out = hf::open(hf::Op::Stage, "b").unwrap();
    for (path, what) in entries {
        let e = hf::Entry {
            path: (*path).to_owned(),
            mode: 0o755,
            what: what.clone(),
            meta: None,
        };
        out.extend(hf::head(&e, &hf::LIMITS).unwrap());
    }
    out.extend(hf::END);
    out
}

/// HEX-GUEST-14: stage a symlink `a` to `/` and then a directory `a/escaped` under it, and a symlink
/// to the staged folder's parent then a file under it, outside any layer: expect each refused as leading
/// through a symbolic link, nothing made outside the box's root, and the partial copy removed; and the same
/// within a layer: expect each inside the layer's root, as containerd resolves it (§4a).
#[test]
fn a_stage_that_names_its_way_out_through_a_link_is_refused_and_leaves_nothing() {
    let (lab, staged) = staged_lab();
    for (link_to, under) in [("/", hf::What::Dir), ("../..", hf::What::File { size: 0 })] {
        let answer = stage_raw(
            &lab,
            &raw_stage(&[
                (
                    "a",
                    hf::What::Symlink {
                        target: link_to.to_owned(),
                    },
                ),
                ("a/escaped", under),
            ]),
        );
        let Answer::Refused(Refusal::Invalid { why, .. }) = answer else {
            panic!("expected a refusal, got {answer:?}");
        };
        assert!(why.contains("symbolic link"), "{why}");
        assert!(!Path::new("/escaped").exists());
        assert!(!lab.dir.path().join("escaped").exists());
        assert!(!staged.join("b").exists(), "the partial copy was left");
    }
    // In a layer, the same names resolve within the layer's root (§4a, the lead's ruling): inside it.
    for (link_to, under) in [("/", hf::What::Dir), ("../..", hf::What::File { size: 0 })] {
        let answer = stage_raw(
            &lab,
            &raw_stage(&[
                ("r", hf::What::Dir),
                ("r", hf::What::Layer),
                (
                    "r/a",
                    hf::What::Symlink {
                        target: link_to.to_owned(),
                    },
                ),
                ("r/a/escaped", under),
            ]),
        );
        assert_eq!(answer, Answer::Done);
        assert!(!Path::new("/escaped").exists());
        assert!(!lab.dir.path().join("escaped").exists());
        assert!(!staged.join("escaped").exists());
        assert!(
            std::fs::symlink_metadata(staged.join("b/r/escaped")).is_ok(),
            "it lands at the layer's root"
        );
        hex_client::guest::stage::unstage(lab.link().as_ref(), &BoxId::new("b").unwrap()).unwrap();
    }
}

/// HEX-GUEST-14: stage an entry of a device's kind, an entry twice, and an entry under a file: expect
/// each refused by type (the file by its name) and the partial copy removed; and a stream cut before its end head refused as truncated.
#[test]
fn a_hostile_or_cut_stage_is_refused_and_leaves_nothing() {
    let (lab, staged) = staged_lab();
    let mut device = hf::open(hf::Op::Stage, "b").unwrap();
    // A head of kind 8 (none on the stream: a device has none), mode 0644, a one-byte path and no size.
    device.extend([8u8, 0]);
    device.extend(0o644u16.to_le_bytes());
    device.extend(1u16.to_le_bytes());
    device.extend([0u8; 10]);
    device.push(b'd');
    let Answer::Refused(Refusal::Invalid { why, .. }) = stage_raw(&lab, &device) else {
        panic!("expected a refusal");
    };
    assert!(
        why.contains("devices, FIFOs and sockets are never staged"),
        "{why}"
    );
    assert!(!staged.join("b").exists());
    let twice = raw_stage(&[("d", hf::What::Dir), ("d", hf::What::Dir)]);
    let Answer::Refused(Refusal::Invalid { why, .. }) = stage_raw(&lab, &twice) else {
        panic!("expected a refusal");
    };
    assert!(why.contains("came twice"), "{why}");
    assert!(!staged.join("b").exists());
    // A file in the way of a later entry's parent is refused by its name, and the copy removed.
    let through_a_file = raw_stage(&[("f", hf::What::File { size: 0 }), ("f/x", hf::What::Dir)]);
    let Answer::Refused(Refusal::Invalid { why, .. }) = stage_raw(&lab, &through_a_file) else {
        panic!("expected a refusal");
    };
    assert!(why.contains("\"f\", which isn't a directory"), "{why}");
    assert!(!staged.join("b").exists());
    let whole = raw_stage(&[("d", hf::What::Dir)]);
    let Answer::Refused(Refusal::Invalid { why, .. }) = stage_raw(&lab, &whole[..whole.len() - 1])
    else {
        panic!("expected a refusal");
    };
    assert!(why.contains("ended before its end head"), "{why}");
    assert!(!staged.join("b").exists());
}

/// One entry of a test layer: a directory, a file and its bytes, a symbolic link and its text, a hard
/// link and its target.
enum T<'a> {
    Dir(&'a str),
    DirMode(&'a str, u32),
    File(&'a str, &'a [u8]),
    Link(&'a str, &'a str),
    Hard(&'a str, &'a str),
}

/// Writes a layer of `entries` at `path`, each owned by this test's own uid and gid, mtime `mtime`.
fn layer_tar(path: &Path, mtime: u64, entries: &[T]) {
    let mut b = tar::Builder::new(File::create(path).unwrap());
    for e in entries {
        let mut h = tar::Header::new_ustar();
        h.set_uid(u64::from(rustix::process::getuid().as_raw()));
        h.set_gid(u64::from(rustix::process::getgid().as_raw()));
        h.set_mtime(mtime);
        h.set_size(0);
        match e {
            T::Dir(name) | T::DirMode(name, _) => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(match e {
                    T::DirMode(_, mode) => *mode,
                    _ => 0o755,
                });
                b.append_data(&mut h, format!("{name}/"), std::io::empty())
                    .unwrap();
            }
            T::File(name, data) => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(0o644);
                h.set_size(data.len() as u64);
                b.append_data(&mut h, name, *data).unwrap();
            }
            T::Link(name, to) => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_mode(0o777);
                b.append_link(&mut h, name, to).unwrap();
            }
            T::Hard(name, to) => {
                h.set_entry_type(tar::EntryType::Link);
                h.set_mode(0o644);
                b.append_link(&mut h, name, to).unwrap();
            }
        }
    }
    b.into_inner().unwrap();
}

/// Stages `layers` (each its mtime and entries) as box `box-1`'s root through the host's packer and the
/// relay: each layer to the guest's store, by its digest. Its layers' folders in the store, lowest first.
fn stage_layers(lab: &Lab, host: &Path, layers: &[(u64, Vec<T>)]) -> Result<Vec<PathBuf>, Refusal> {
    stage_as(lab, host, "box-1", layers)
}

/// The sha256 of the file at `path`: a layer's digest, as its image names it.
fn sha256(path: &Path) -> hf::Digest {
    <sha2::Sha256 as sha2::Digest>::digest(std::fs::read(path).unwrap()).into()
}

/// [`stage_layers`] as box `id`, its layers' tarballs named by their place in `layers`.
fn stage_as(
    lab: &Lab,
    host: &Path,
    id: &str,
    layers: &[(u64, Vec<T>)],
) -> Result<Vec<PathBuf>, Refusal> {
    use hex_client::guest::stage::{Item, Layer, Source, stage};
    let tarballs: Vec<Layer> = layers
        .iter()
        .enumerate()
        .map(|(i, (mtime, entries))| {
            let path = host.join(format!("{id}-layer{i}.tar"));
            layer_tar(&path, *mtime, entries);
            Layer {
                digest: Some(sha256(&path)),
                path,
            }
        })
        .collect();
    let folders = tarballs
        .iter()
        .map(|l| {
            lab.dir
                .path()
                .join("staged")
                .join(hf::STORE)
                .join(hf::digest_hex(&l.digest.unwrap()))
        })
        .collect();
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Layers(tarballs),
    }];
    stage(
        lab.link().as_ref(),
        &BoxId::new(id).unwrap(),
        &items,
        &hf::LIMITS,
    )
    .map(|_| folders)
}

/// HEX-GUEST-14 with the host's packer (the lead's ruling, §4a): stage a tarball root whose entries are
/// written through links it made, end to end through the relay: `a` to `/etc` then `a/x`, and `c` to
/// `../../..` (the lab's folder, from the staged root) then `c/x`, with a sentinel `x` in that folder:
/// expect each where containerd's `fs.RootPath` puts it, inside the root (`etc/x`, `x`), and the sentinel
/// untouched.
#[test]
fn a_tarball_writing_through_its_own_link_lands_inside_its_root() {
    let (lab, _staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let sentinel = lab.dir.path().join("x");
    std::fs::write(&sentinel, b"sentinel").unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[(
            1_600_000_000,
            vec![
                T::Dir("etc"),
                T::Link("a", "/etc"),
                T::File("a/x", b"in etc"),
                T::Link("c", "../../.."),
                T::File("c/x", b"at the top"),
            ],
        )],
    )
    .unwrap();
    let root = &layers[0];
    assert_eq!(std::fs::read(root.join("etc/x")).unwrap(), b"in etc");
    assert_eq!(std::fs::read(root.join("x")).unwrap(), b"at the top");
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"sentinel");
    assert!(!lab.dir.path().join("staged/x").exists());
    assert_eq!(
        std::fs::read_link(root.join("a")).unwrap(),
        Path::new("/etc"),
        "the link itself is kept as written"
    );
}

/// HEX-GUEST-14 (the lead's ruling after ctr's snapshot, §4a; containerd's overlay applier,
/// `core/diff/apply/apply_linux.go:36-55`, and `mkparent`, `pkg/archive/tar.go:448-503`): stage `bin` as a
/// link to `usr/bin` (mode 0750) in one layer and `bin/tool` in the next: expect `bin` a directory holding
/// `tool`, with `usr/bin`'s owner and mode copied as mkparent copies them, and `usr/bin` without it, as
/// ctr's snapshot of the same layers holds (bench/native/guest-tree, lab:tree).
#[test]
fn a_later_layers_entry_under_an_earlier_layers_link_makes_a_directory_as_overlay_does() {
    use std::os::unix::fs::PermissionsExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[
            (
                1_600_000_000,
                vec![
                    T::Dir("usr"),
                    T::DirMode("usr/bin", 0o750),
                    T::Link("bin", "usr/bin"),
                ],
            ),
            (1_600_000_100, vec![T::File("bin/tool", b"tool")]),
        ],
    )
    .unwrap();
    // The second layer's folder: `bin` a directory, which the overlay shows over the first's link.
    let bin = std::fs::symlink_metadata(layers[1].join("bin")).unwrap();
    assert!(bin.is_dir(), "bin became a directory");
    assert_eq!(bin.permissions().mode() & 0o7777, 0o750, "usr/bin's mode");
    assert_eq!(std::fs::read(layers[1].join("bin/tool")).unwrap(), b"tool");
    assert!(
        !layers[1].join("usr").exists(),
        "the second layer made nothing under usr"
    );
    assert!(
        std::fs::symlink_metadata(layers[0].join("bin"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the first layer's link is its own still"
    );
    assert!(
        !staged.join("box-1/root").exists(),
        "a layered root has no copy of its own"
    );
}

/// HEX-GUEST-14 (§4a, as ctr's snapshot of lab:file holds): stage a file `p` in one layer and `p/inner`
/// in the next: expect `p` a directory 0755 holding `inner`, as overlay's mkparent makes it.
#[test]
fn a_later_layers_entry_under_an_earlier_layers_file_makes_a_directory_as_overlay_does() {
    use std::os::unix::fs::PermissionsExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[
            (1_600_000_000, vec![T::File("p", b"p")]),
            (1_600_000_100, vec![T::File("p/inner", b"inner")]),
        ],
    )
    .unwrap();
    let p = std::fs::symlink_metadata(layers[1].join("p")).unwrap();
    assert!(p.is_dir());
    assert_eq!(p.permissions().mode() & 0o7777, 0o755);
    assert_eq!(std::fs::read(layers[1].join("p/inner")).unwrap(), b"inner");
    assert!(staged.join("box-1").exists());
}

/// HEX-GUEST-14 (§4a, as ctr fails lab:hardlink's unpack, `link .../q .../h: no such file or
/// directory`): stage a hard link in one layer to a file an earlier layer made: expect the stage
/// refused naming the link and the file, and the partial copy gone. (The relay refuses it likewise
/// where the packer can't tell, a target named through a link: `Stage::entry`.)
#[test]
fn a_hard_link_to_an_earlier_layers_file_is_refused_as_overlay_fails_it() {
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let got = stage_layers(
        &lab,
        host.path(),
        &[
            (1_600_000_000, vec![T::File("q", b"q")]),
            (1_600_000_100, vec![T::Hard("h", "q")]),
        ],
    );
    let why = got.unwrap_err().to_string();
    // The packer refuses it first, naming the archive's entry `h` and its target `q`.
    assert!(
        why.contains(": h: ") && why.contains("\"q\"") && why.contains("an earlier layer made"),
        "{why}"
    );
    assert!(!staged.join("box-1").exists(), "the partial copy was left");
}

/// HEX-GUEST-14 (the lead's ruling, §4a): stage a hard link whose target is named through the root's own
/// link (`bin` to `/usr/bin`, `usr/bin/perl` to `bin/perl5`): expect it the target's inode; and one named
/// through a link to `../../..`, where a file of that name sits outside the root: expect it refused as
/// containerd's unpack fails it, for the same reason: the target resolves inside the root
/// (`hardlinkRootPath`, `pkg/archive/tar.go:795-807`), where nothing of that name is (`ENOENT`), and the
/// outside file is never linked.
#[test]
fn a_hard_link_through_the_roots_own_link_resolves_inside_it() {
    use std::os::unix::fs::MetadataExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[(
            1_600_000_000,
            vec![
                T::Dir("usr"),
                T::Dir("usr/bin"),
                T::File("usr/bin/perl5", b"perl"),
                T::Link("bin", "/usr/bin"),
                T::Hard("usr/bin/perl", "bin/perl5"),
            ],
        )],
    )
    .unwrap();
    let root = &layers[0];
    let perl5 = std::fs::metadata(root.join("usr/bin/perl5")).unwrap();
    let perl = std::fs::symlink_metadata(root.join("usr/bin/perl")).unwrap();
    assert_eq!(perl.ino(), perl5.ino());
    hex_client::guest::stage::unstage(lab.link().as_ref(), &BoxId::new("box-1").unwrap()).unwrap();

    let secret = lab.dir.path().join("secret");
    std::fs::write(&secret, b"outside").unwrap();
    let got = stage_layers(
        &lab,
        host.path(),
        &[(
            1_600_000_000,
            vec![T::Link("d", "../../.."), T::Hard("h", "d/secret")],
        )],
    );
    let why = got.unwrap_err().to_string();
    assert!(
        why.contains("which isn't staged") && why.contains("No such file or directory"),
        "{why}"
    );
    assert_eq!(std::fs::metadata(&secret).unwrap().nlink(), 1);
    assert!(!staged.join("box-1").exists(), "the partial copy was left");
}

/// HEX-GUEST-14 (the lead's ruling, §4a; containerd's `mkparent`, `pkg/archive/tar.go:448-503`): stage a
/// layer holding `usr/local/bin/x` with no entry for `usr/local` or `usr/local/bin`, as hand-made and
/// Bazel layers are: expect the file staged and each missing parent made 0755, as containerd makes it.
#[test]
fn an_entry_whose_parents_no_layer_made_gets_them_made_as_containerd_does() {
    use std::os::unix::fs::PermissionsExt;
    let (lab, _staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[(
            1_600_000_000,
            vec![T::Dir("usr"), T::File("usr/local/bin/x", b"x")],
        )],
    )
    .unwrap();
    let root = &layers[0];
    assert_eq!(std::fs::read(root.join("usr/local/bin/x")).unwrap(), b"x");
    for made in ["usr/local", "usr/local/bin"] {
        let meta = std::fs::symlink_metadata(root.join(made)).unwrap();
        assert!(meta.is_dir(), "{made}");
        assert_eq!(meta.permissions().mode() & 0o7777, 0o755, "{made}");
    }
}

/// HEX-GUEST-14 (§4a; containerd's `link`, os.Link, `pkg/archive/tar.go:386-394`): stage a hard link to a
/// symbolic link: expect another name for the link itself, as linkat(2) without `AT_SYMLINK_FOLLOW` makes.
#[test]
fn a_hard_link_to_a_symbolic_link_names_the_link_itself() {
    use std::os::unix::fs::MetadataExt;
    let (lab, _staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[(
            1_600_000_000,
            vec![T::Link("sh", "bash"), T::Hard("sh2", "sh")],
        )],
    )
    .unwrap();
    let root = &layers[0];
    let sh = std::fs::symlink_metadata(root.join("sh")).unwrap();
    let sh2 = std::fs::symlink_metadata(root.join("sh2")).unwrap();
    assert!(sh2.file_type().is_symlink());
    assert_eq!(sh.ino(), sh2.ino());
}

/// HEX-GUEST-14 (CLAUDE.md §3; HEX-GUEST-19's HEX-RT-1 in a real guest): send a stage refused at an entry
/// mid-stream (a device's kind), followed by 4 MiB more the relay won't stage: expect every byte of it
/// written (the relay reads its peer to the end after a refusal; over vsock a close with bytes unread
/// resets the connection and loses the answer), then its typed refusal read back, and nothing staged.
#[test]
fn a_relay_refusing_mid_stream_reads_its_peer_to_the_end_and_answers_typed() {
    let (lab, staged) = staged_lab();
    let mut bytes = raw_stage(&[("ok", hf::What::Dir)]);
    bytes.truncate(bytes.len() - hf::HEAD_BYTES);
    // A head of kind 8 (none on the stream), then bytes past it.
    bytes.extend([8u8, 0]);
    bytes.extend(0o644u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend([0u8; 10]);
    bytes.push(b'd');
    bytes.extend(vec![0x5a_u8; 4 << 20]);
    let mut conn = UnixStream::connect(lab.dir.path().join("files.sock")).unwrap();
    conn.write_all(&bytes)
        .expect("the relay closed on bytes it hadn't read");
    conn.shutdown(Shutdown::Write).unwrap();
    let mut back = Vec::new();
    conn.read_to_end(&mut back).unwrap();
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    let mut rest = &back[..];
    let answer = loop {
        let (took, whole) = d.take(rest).unwrap();
        rest = &rest[took..];
        if let Some(m) = whole
            && hf::read_budget(&m).is_none()
        {
            let received = Received {
                bytes: &m,
                size: m.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            break wire::read_answer(&received, wire::MAX_MESSAGE).unwrap().1;
        }
    };
    let Answer::Refused(Refusal::Invalid { why, .. }) = answer else {
        panic!("expected a typed refusal, got {answer:?}");
    };
    assert!(why.contains("never staged"), "{why}");
    assert!(!staged.join("b").exists());
}

/// HEX-GUEST-14 (Ada's rule on constants: a learned bound): open a stage: expect the relay's first answer
/// its budget, the free inodes and bytes statvfs(3) reports for the staged folder, which a stage spends
/// against (within what the folder's file system holds, read again beside it; where it keeps no inode count,
/// btrfs, its bytes bound the entries).
#[test]
fn a_stages_first_answer_is_the_staged_folders_free_space() {
    let (lab, staged) = staged_lab();
    let mut conn = UnixStream::connect(lab.dir.path().join("files.sock")).unwrap();
    conn.write_all(&hf::open(hf::Op::Stage, "b").unwrap())
        .unwrap();
    let mut d = Deframer::new(wire::MAX_MESSAGE);
    let mut buf = [0u8; 256];
    let message = 'read: loop {
        let n = conn.read(&mut buf).unwrap();
        assert!(n > 0, "the relay closed before its budget");
        // The deframer takes what it can of the bytes at a time: every byte read is offered to it.
        let mut rest = &buf[..n];
        while !rest.is_empty() {
            let (took, whole) = d.take(rest).unwrap();
            rest = &rest[took..];
            if let Some(m) = whole {
                break 'read m;
            }
        }
    };
    let hf::Budget { entries, bytes, .. } = hf::read_budget(&message).expect("a budget");
    let st = rustix::fs::statvfs(&staged).unwrap();
    // Within the inodes the file system holds, or where it keeps no count (btrfs: f_files 0), its bytes.
    let held = if st.f_files == 0 {
        st.f_blocks * st.f_frsize
    } else {
        st.f_files
    };
    assert!(entries > 0 && entries <= held, "{entries} of {held}");
    assert!(bytes > 0 && bytes <= st.f_blocks * st.f_frsize, "{bytes}");
    drop(conn);
}

/// What the store at `store` holds but its lock and the room it leaves a stage (`.lock`, `.need`): layers,
/// partials, sizes.
fn layers_left(store: &Path) -> Vec<String> {
    std::fs::read_dir(store)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".lock" && n != ".need")
        .collect()
}

/// The raw stage of `entries` for `b`, its root's layers named `layers` in its opening.
fn raw_layered(layers: &[hf::Digest], entries: &[(&str, hf::What)]) -> Vec<u8> {
    let named: Vec<Option<hf::Digest>> = layers.iter().copied().map(Some).collect();
    let mut out = hf::open_stage("b", &named).unwrap();
    for (path, what) in entries {
        let e = hf::Entry {
            path: (*path).to_owned(),
            mode: 0o755,
            what: what.clone(),
            meta: None,
        };
        out.extend(hf::head(&e, &hf::LIMITS).unwrap());
    }
    out.extend(hf::END);
    out
}

/// HEX-GUEST-14 (§4a, as ctr fails lab:hardlink's unpack): send the relay, raw, a stage of two layers, the
/// first making `r/q` and the second linking `r/h` to it: expect the stage refused naming both and the
/// earlier layer, and the box's copy gone, the relay holding the rule where no packer stands before it.
#[test]
fn the_relay_refuses_a_hard_link_to_an_earlier_layers_file() {
    let (lab, staged) = staged_lab();
    let answer = stage_raw(
        &lab,
        &raw_layered(
            &[[1; 32], [2; 32]],
            &[
                ("r", hf::What::Layer),
                ("r/q", hf::What::File { size: 0 }),
                ("r", hf::What::Layer),
                (
                    "r/h",
                    hf::What::Hardlink {
                        target: "r/q".to_owned(),
                    },
                ),
            ],
        ),
    );
    let Answer::Refused(Refusal::Invalid { why, .. }) = answer else {
        panic!("expected a refusal, got {answer:?}");
    };
    assert!(
        why.contains("\"r/h\"") && why.contains("\"r/q\"") && why.contains("an earlier layer made"),
        "{why}"
    );
    assert!(!staged.join("b").exists(), "the box's copy was left");
}

/// HEX-GUEST-14 (§4a; containerd's overlay applier, `OverlayConvertWhiteout`): stage two layers, the second
/// whiting out `etc/a`, making `d` opaque and adding `d/new`, and putting a directory at `f` (a file below),
/// a file at `g` (a directory below) and a directory at `s` (a link below): expect the second layer's folder
/// to hold overlayfs's own forms, `etc/a` a character device 0/0 and `d` with the opaque xattr (`user.` as
/// a rootless relay's, `trusted.` as root's), with `d/new`, `d`'s mtime the second layer's, `f` a directory,
/// `g` a file and `s` a directory; and the first layer's folder as it was.
#[test]
fn a_later_layer_whites_out_makes_opaque_and_replaces_as_overlayfs_reads_them() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let (lab, _staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let layers = stage_layers(
        &lab,
        host.path(),
        &[
            (
                1_600_000_000,
                vec![
                    T::Dir("etc"),
                    T::File("etc/a", b"a"),
                    T::File("etc/b", b"b"),
                    T::Dir("d"),
                    T::File("d/old", b"old"),
                    T::File("f", b"file"),
                    T::Dir("g"),
                    T::File("g/inner", b"inner"),
                    T::Link("s", "etc"),
                ],
            ),
            (
                1_600_000_100,
                vec![
                    T::Dir("etc"),
                    T::File("etc/.wh.a", b""),
                    T::Dir("d"),
                    T::File("d/.wh..wh..opq", b""),
                    T::File("d/new", b"new"),
                    T::Dir("f"),
                    T::File("g", b"now a file"),
                    T::Dir("s"),
                ],
            ),
        ],
    )
    .unwrap();
    let (first, second) = (&layers[0], &layers[1]);
    let a = std::fs::symlink_metadata(second.join("etc/a")).unwrap();
    assert!(a.file_type().is_char_device(), "a whiteout is overlayfs's");
    assert_eq!(a.rdev(), 0);
    assert!(
        !second.join("etc/.wh.a").exists(),
        "a whiteout is never a file"
    );
    let opaque = if rustix::process::geteuid().is_root() {
        "trusted.overlay.opaque"
    } else {
        "user.overlay.opaque"
    };
    let mut value = [0u8; 8];
    let n = rustix::fs::getxattr(second.join("d"), opaque, &mut value).unwrap();
    assert_eq!(&value[..n], b"y");
    let mut d: Vec<String> = std::fs::read_dir(second.join("d"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    d.sort();
    assert_eq!(d, ["new"]);
    assert_eq!(
        std::fs::metadata(second.join("d")).unwrap().mtime(),
        1_600_000_100
    );
    assert!(
        std::fs::symlink_metadata(second.join("f"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(std::fs::read(second.join("g")).unwrap(), b"now a file");
    assert!(
        std::fs::symlink_metadata(second.join("s"))
            .unwrap()
            .is_dir()
    );
    // The first layer's folder is its own, untouched by the second.
    assert_eq!(std::fs::read(first.join("etc/a")).unwrap(), b"a");
    assert_eq!(std::fs::read(first.join("d/old")).unwrap(), b"old");
    assert!(
        std::fs::symlink_metadata(first.join("s"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// HEX-GUEST-14 (§4a, the layer store): stage box `one` of layers A and B, then box `two` of A and C, A's
/// tarball gone from the host by then: expect `two` staged (the guest holds A, so the packer never reads it
/// again), the store holding A, B and C once each, A's folder the same one, and each box's record naming
/// its own layers.
#[test]
fn a_layer_the_guest_holds_is_staged_once_and_shared() {
    use hex_client::guest::stage::{Item, Layer, Source, stage};
    use std::os::unix::fs::MetadataExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let one = stage_as(
        &lab,
        host.path(),
        "one",
        &[
            (1_600_000_000, vec![T::Dir("base"), T::File("base/a", b"a")]),
            (1_600_000_100, vec![T::File("b", b"b")]),
        ],
    )
    .unwrap();
    let a_inode = std::fs::metadata(&one[0]).unwrap().ino();
    // A's tarball, as `two` names it: its digest kept, the file itself gone.
    let a = host.path().join("one-layer0.tar");
    let a_layer = Layer {
        digest: Some(sha256(&a)),
        path: a.clone(),
    };
    std::fs::remove_file(&a).unwrap();
    let c = host.path().join("c.tar");
    layer_tar(&c, 1_600_000_200, &[T::File("c", b"c")]);
    let c_layer = Layer {
        digest: Some(sha256(&c)),
        path: c,
    };
    stage(
        lab.link().as_ref(),
        &BoxId::new("two").unwrap(),
        &[Item {
            name: "root".to_owned(),
            source: Source::Layers(vec![a_layer.clone(), c_layer.clone()]),
        }],
        &hf::LIMITS,
    )
    .unwrap();
    let store = staged.join(hf::STORE);
    let mut layers: Vec<String> = std::fs::read_dir(&store)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.len() == 64)
        .collect();
    layers.sort();
    assert_eq!(layers.len(), 3, "{layers:?}");
    assert_eq!(std::fs::metadata(&one[0]).unwrap().ino(), a_inode);
    let record = std::fs::read_to_string(staged.join("two/.layers")).unwrap();
    assert_eq!(
        record,
        format!(
            "{}\n{}\n",
            hf::digest_hex(&a_layer.digest.unwrap()),
            hf::digest_hex(&c_layer.digest.unwrap())
        )
    );
    assert_eq!(
        std::fs::read(
            store
                .join(hf::digest_hex(&c_layer.digest.unwrap()))
                .join("c")
        )
        .unwrap(),
        b"c"
    );
}

/// HEX-GUEST-14 (§4a, the store's crash safety): stage a layer whose file is cut short as the connection
/// ends mid-layer: expect it refused, no folder named its digest (nothing half-staged counts as held), and
/// no partial left; and a partial another relay process left (its pid not this one's), with junk in it:
/// expect the next stage to say the layer isn't held, remove that partial, and stage the layer whole.
#[test]
fn a_layer_cut_or_crashed_mid_stage_is_never_held() {
    let (lab, staged) = staged_lab();
    let store = staged.join(hf::STORE);
    let digest = [7u8; 32];
    let mut cut = raw_layered(
        &[digest],
        &[("r", hf::What::Layer), ("r/f", hf::What::File { size: 10 })],
    );
    cut.truncate(cut.len() - hf::HEAD_BYTES);
    cut.extend(b"half");
    let answer = stage_raw(&lab, &cut);
    assert!(matches!(answer, Answer::Refused(_)), "{answer:?}");
    let left = layers_left(&store);
    assert!(left.is_empty(), "left in the store: {left:?}");
    // A crashed relay's partial: another pid's.
    let partial = store.join(format!(".partial-1-1-{}", hf::digest_hex(&digest)));
    std::fs::create_dir(&partial).unwrap();
    std::fs::write(partial.join("junk"), b"junk").unwrap();
    let whole = raw_layered(
        &[digest],
        &[("r", hf::What::Layer), ("r/f", hf::What::File { size: 0 })],
    );
    assert_eq!(stage_raw(&lab, &whole), Answer::Done);
    assert!(!partial.exists(), "the crashed partial was left");
    let held = store.join(hf::digest_hex(&digest));
    assert!(held.join("f").exists() && !held.join("junk").exists());
}

/// HEX-GUEST-14 (§4a; the lead's rule, 2026-11-06: a create never hashes a layer the guest holds, and one it
/// lacks is hashed as it crosses, to verify it): stage a layer whose tarball changed after its digest was
/// taken: expect the stage refused naming the bytes' sha256 and the digest it was given, and nothing in the
/// store, under either digest or as a partial: what the store holds under a digest is only ever its bytes.
#[test]
fn a_layer_whose_bytes_arent_its_digest_is_refused_and_never_held() {
    use hex_client::guest::stage::{Item, Layer, Source, stage};
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let path = host.path().join("layer.tar");
    layer_tar(&path, 1_600_000_000, &[T::File("f", b"named")]);
    let named = sha256(&path);
    layer_tar(&path, 1_600_000_000, &[T::File("f", b"other")]);
    let got = sha256(&path);
    let why = stage(
        lab.link().as_ref(),
        &BoxId::new("box-1").unwrap(),
        &[Item {
            name: "root".to_owned(),
            source: Source::Layers(vec![Layer {
                path,
                digest: Some(named),
            }]),
        }],
        &hf::LIMITS,
    )
    .unwrap_err()
    .to_string();
    assert!(
        why.contains(&hf::digest_hex(&got)) && why.contains(&hf::digest_hex(&named)),
        "{why}"
    );
    let left = layers_left(&staged.join(hf::STORE));
    assert!(left.is_empty(), "left in the store: {left:?}");
}

/// HEX-GUEST-14 (§4a; the lead's check, 2026-11-06: what whole-root staging took, the store takes): stage a
/// layer whose digest the host doesn't know (docker save's `<id>/layer.tar`): expect it staged into the
/// box's own `layers/0`, a folder, nothing of it in the store, and its digest learned as it crossed.
#[test]
fn a_layer_of_no_known_digest_is_staged_into_its_box_and_its_digest_learned() {
    use hex_client::guest::stage::{Item, Layer, Source, stage};
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let path = host.path().join("layer.tar");
    layer_tar(&path, 1_600_000_000, &[T::File("f", b"saved")]);
    let got = stage(
        lab.link().as_ref(),
        &BoxId::new("b").unwrap(),
        &[Item {
            name: "root".to_owned(),
            source: Source::Layers(vec![Layer {
                path: path.clone(),
                digest: None,
            }]),
        }],
        &hf::LIMITS,
    )
    .unwrap();
    assert_eq!(got.learned.len(), 1);
    assert_eq!(got.learned[0].digest, sha256(&path));
    let own = staged.join("b/layers/0");
    assert!(std::fs::symlink_metadata(&own).unwrap().is_dir());
    assert_eq!(std::fs::read(own.join("f")).unwrap(), b"saved");
    assert!(layers_left(&staged.join(hf::STORE)).is_empty());
}

/// HEX-GUEST-14 (§4a; the lead's call after HEX-GUEST-22's 2x diagnosis, 2026-11-07: no reclaimer; with one,
/// grafana's create under pressure took 4,483 ms, without it 2,629, as a bypass's reclaim took cached layers
/// that would have been hits for layers never admitted): stage box `one` of layer A and remove it, so A is
/// cached unreferenced; make the room a stage needs more than is free; stage box `b` of layer B, raw, reading
/// the relay to its end: expect `b` answered `Done` with B in its own folder, never the store, and once the
/// relay closes, A still cached: a bypassing stage never evicts.
#[test]
fn a_bypassing_stage_never_evicts_a_cached_layer() {
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let one = stage_as(
        &lab,
        host.path(),
        "one",
        &[(1_600_000_000, vec![T::File("a", b"a")])],
    )
    .unwrap();
    assert!(one[0].exists());
    crate::relay::files::remove(&staged, &BoxId::new("one").unwrap()).unwrap();
    assert!(one[0].exists(), "a layer no box names stays cached");
    std::fs::write(
        staged.join(hf::STORE).join(".need"),
        format!("0 {}\n", u64::MAX),
    )
    .unwrap();
    let b = [0xbb; 32];
    let raw = raw_layered(
        &[b],
        &[("r", hf::What::Layer), ("r/f", hf::What::File { size: 0 })],
    );
    assert_eq!(stage_raw(&lab, &raw), Answer::Done);
    assert!(
        std::fs::symlink_metadata(staged.join("b/layers/0"))
            .unwrap()
            .is_dir()
    );
    assert!(!staged.join(hf::STORE).join(hf::digest_hex(&b)).exists());
    assert!(one[0].exists(), "a bypassing stage evicted a cached layer");
}

/// HEX-GUEST-14 (§4a as amended): stage a tarball root, end to end through the host's packer and the relay,
/// whose file is owned by this test's own uid and gid (all a rootless relay may give), has a fractional mtime
/// and a later atime, a `user.` xattr and a setuid mode, with a hard link to it and a directory with its own mtime: expect each
/// on disk as the archive says, the link the same inode, the mode kept though the owner was set after it was
/// written, and the directory's mtime unmoved by its children. Other owners and `security.capability` take
/// the guest's root: HEX-GUEST-21 holds those.
#[test]
fn a_tarball_roots_owners_times_xattrs_and_hard_links_land_on_disk() {
    use hex_client::guest::stage::{Item, Source, stage};
    use std::os::unix::fs::MetadataExt;
    let (lab, staged) = staged_lab();
    let host = tempfile::tempdir().unwrap();
    let tarball = host.path().join("root.tar");
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    {
        let mut b = tar::Builder::new(File::create(&tarball).unwrap());
        let mut dir = tar::Header::new_ustar();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_mode(0o750);
        dir.set_uid(u64::from(uid));
        dir.set_gid(u64::from(gid));
        dir.set_mtime(1_600_000_000);
        dir.set_size(0);
        b.append_data(&mut dir, "bin/", std::io::empty()).unwrap();
        // A PAX record (`%d key=value\n`, its length counting itself) before the file.
        let mut record = Vec::new();
        for (k, v) in [
            ("mtime", "1700000000.25"),
            ("atime", "1800000000.5"),
            ("SCHILY.xattr.user.hex", "staged"),
        ] {
            let body = format!(" {k}={v}\n");
            let mut n = body.len() + 1;
            while format!("{n}").len() + body.len() != n {
                n += 1;
            }
            record.extend(format!("{n}{body}").into_bytes());
        }
        let mut pax = tar::Header::new_ustar();
        pax.set_entry_type(tar::EntryType::XHeader);
        pax.set_mode(0o644);
        pax.set_size(record.len() as u64);
        b.append_data(&mut pax, "PaxHeaders/perl5", record.as_slice())
            .unwrap();
        let mut file = tar::Header::new_ustar();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o4755);
        file.set_uid(u64::from(uid));
        file.set_gid(u64::from(gid));
        file.set_mtime(1_700_000_000);
        file.set_size(4);
        b.append_data(&mut file, "bin/perl5", &b"perl"[..]).unwrap();
        let mut link = tar::Header::new_ustar();
        link.set_entry_type(tar::EntryType::Link);
        link.set_mode(0o4755);
        link.set_size(0);
        b.append_link(&mut link, "bin/perl", "bin/perl5").unwrap();
        b.into_inner().unwrap();
    }
    let id = BoxId::new("box-1").unwrap();
    let items = [Item {
        name: "root".to_owned(),
        source: Source::Tar(tarball),
    }];
    stage(lab.link().as_ref(), &id, &items, &hf::LIMITS).unwrap();
    let root = staged.join("box-1/root");
    let perl5 = std::fs::symlink_metadata(root.join("bin/perl5")).unwrap();
    let perl = std::fs::symlink_metadata(root.join("bin/perl")).unwrap();
    assert_eq!((perl5.uid(), perl5.gid()), (uid, gid));
    assert_eq!(
        perl5.mode() & 0o7777,
        0o4755,
        "setuid kept: the mode follows the owner"
    );
    assert_eq!(
        (perl5.mtime(), perl5.mtime_nsec()),
        (1_700_000_000, 250_000_000)
    );
    assert_eq!(
        (perl5.atime(), perl5.atime_nsec()),
        (1_800_000_000, 500_000_000),
        "its own atime, the later"
    );
    assert_eq!(perl.ino(), perl5.ino(), "the hard link is the same inode");
    assert_eq!(perl5.nlink(), 2);
    let mut value = [0u8; 16];
    let n = rustix::fs::getxattr(root.join("bin/perl5"), "user.hex", &mut value).unwrap();
    assert_eq!(&value[..n], b"staged");
    let bin = std::fs::symlink_metadata(root.join("bin")).unwrap();
    assert_eq!(bin.mode() & 0o7777, 0o750);
    assert_eq!(
        bin.mtime(),
        1_600_000_000,
        "the directory's mtime is set after its children"
    );
}

/// Whether the write end `w` of a pipe has lost every reader, waiting at most `within`: Linux's pipe poll reports
/// `POLLERR` on a write end whose readers are gone (`pipe_poll`, fs/pipe.c), asked or not.
fn readerless(w: &std::io::PipeWriter, within: Duration) -> bool {
    let mut ready = [PollFd::new(w, PollFlags::empty())];
    let ts = Timespec {
        tv_sec: i64::try_from(within.as_secs()).unwrap(),
        tv_nsec: 0,
    };
    rustix::event::poll(&mut ready, Some(&ts)).unwrap() == 1
        && ready[0].revents().contains(PollFlags::ERR)
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): exec a program with the caller's own stdin pipe through the
/// relay; the program ends without reading its stdin, its stdin pipe's read end going with it: expect the relay
/// to end that stream's connection, and the host's client to let the caller's pipe go with it, so the caller's
/// stdin pipe has no reader once the program has ended, as a local program's has none.
#[test]
fn a_programs_stdin_lets_the_callers_pipe_go_when_the_program_ends() {
    let lab = Lab::new(limits());
    let node = lab.node.try_clone().unwrap();
    let agent = std::thread::spawn(move || {
        let conn = hello(&node);
        let (h, r, fds) = next_request(&conn);
        assert!(matches!(r, Request::Call(Call::Exec { .. })), "{r:?}");
        assert_eq!(fds.len(), 1);
        answer(&conn, h, &Answer::Started { pid: 7 });
        // The program ends without reading its stdin: its pipe's read end goes with it.
        drop(fds);
        answer(&conn, h, &Answer::Exited(Exit::Code(0)));
        conn
    });
    let client = Client::connect(lab.link()).unwrap();
    let (in_r, in_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["true"]),
            Stdio {
                stdin: Some(in_r),
                ..Stdio::null()
            },
        )
        .unwrap();
    assert_eq!(
        proc.wait_for(Duration::from_secs(30)).unwrap(),
        Some(Exit::Code(0))
    );
    assert!(
        readerless(&in_w, Duration::from_secs(30)),
        "the caller's stdin pipe is still read after its program ended"
    );
    drop(proc);
    drop(agent.join().unwrap());
}

/// The system calls a stdin copier waits in, by this architecture's numbers: splice(2), and poll(2) or ppoll(2)
/// (arch/x86/entry/syscalls/syscall_64.tbl; include/uapi/asm-generic/unistd.h, which has no poll).
#[cfg(target_arch = "x86_64")]
const WAITS: [i64; 3] = [275, 7, 271];
#[cfg(target_arch = "aarch64")]
const WAITS: [i64; 2] = [76, 73];

/// Whether thread `tid` sleeps inside one of the copier's waits: the call `/proc/self/task/<tid>/syscall` names
/// is one of [`WAITS`], and its state in `/proc/self/task/<tid>/stat` is 'S' (proc(5)), past the call's entry.
fn waiting(tid: rustix::thread::Pid) -> bool {
    let task = format!("/proc/self/task/{}", tid.as_raw_nonzero());
    let call = std::fs::read_to_string(format!("{task}/syscall")).unwrap_or_default();
    let inside = call
        .split_whitespace()
        .next()
        .and_then(|nr| nr.parse::<i64>().ok())
        .is_some_and(|nr| WAITS.contains(&nr));
    let stat = std::fs::read_to_string(format!("{task}/stat")).unwrap_or_default();
    let asleep = stat
        .rsplit_once(')')
        .is_some_and(|(_, after)| after.trim_start().starts_with('S'));
    inside && asleep
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): a program ends without reading its stdin while the host's stdin
/// connection carries nothing and the relay's stdin copier waits on it: expect the program's close of its stdin
/// pipe, as its exit makes, to complete, never held until the host writes, and the copier then to end the stream's
/// connection, so the host sees it end.
#[test]
fn a_programs_stdin_close_is_held_by_no_copier() {
    let (host, relay) = UnixStream::pair().unwrap();
    let (r, w) = std::io::pipe().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(rustix::thread::gettid()).unwrap();
        Copier::In(w).run(Box::new(relay));
    });
    let tid = rx.recv().unwrap();
    // The fact the test needs: the copier asleep inside its wait for the stream, not on its way to it.
    until(|| waiting(tid));
    let (closed, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(r);
        let _ = closed.send(());
    });
    assert!(
        done.recv_timeout(Duration::from_secs(30)).is_ok(),
        "the program's close of its stdin pipe waited on the relay's copier"
    );
    host.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        (&host).read(&mut byte).unwrap(),
        0,
        "the relay held the stream's connection after its program let its stdin go"
    );
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): a program exits with its stdout pipe full while the host isn't
/// reading, its stream's connection full: expect the program's close of its stdout pipe, as its exit makes, to
/// complete while the relay's copier waits for the host's room, never held until the host reads; then, as the host
/// reads, every byte the program wrote to come, and the stream to end.
#[test]
fn a_programs_stdout_close_is_held_by_no_copier() {
    // A connection that fills, as a guest's vsock connection does by its credit: TCP over loopback with the
    // kernel's least buffers (SOCK_MIN_SNDBUF and SOCK_MIN_RCVBUF, include/net/sock.h). A Unix socket charges
    // spliced pages to no buffer, so it never fills (on Linux 7.3.3 the pipe's whole 64 KiB went to a reader that
    // read nothing).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (host, _) = listener.accept().unwrap();
    rustix::net::sockopt::set_socket_send_buffer_size(&relay, 1).unwrap();
    rustix::net::sockopt::set_socket_recv_buffer_size(&host, 1).unwrap();
    let (r, mut w) = std::io::pipe().unwrap();
    // The program's output, its pipe's whole capacity, written before the copier starts.
    let wrote = vec![b'x'; rustix::pipe::fcntl_getpipe_size(&w).unwrap()];
    w.write_all(&wrote).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        tx.send(rustix::thread::gettid()).unwrap();
        Copier::Out(r).run(Box::new(relay));
    });
    let tid = rx.recv().unwrap();
    // The fact the test needs: the copier asleep inside its wait for the host's room, not on its way to it.
    until(|| waiting(tid));
    let (closed, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(w);
        let _ = closed.send(());
    });
    assert!(
        done.recv_timeout(Duration::from_secs(30)).is_ok(),
        "the program's close of its stdout pipe waited on the relay's copier"
    );
    host.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut came = Vec::new();
    (&host).read_to_end(&mut came).unwrap();
    assert_eq!(came.len(), wrote.len(), "the stream lost bytes");
}

/// AC-T7.4.1: a staging connection as a host that never half-closes it is: its bytes, then nothing. A transport need
/// not carry a half-close (Firecracker's vsock muxer makes a host's a full shutdown, after which nothing the guest
/// sends reaches the host: `vsock/csm/connection.rs:228-239` and `:307` at 500efc6), so a read past what the host sent
/// is the relay waiting for an end no host owes it: it fails by name instead of waiting. What the relay writes is kept.
struct Unclosed {
    sent: io::Cursor<Vec<u8>>,
    answers: Arc<Mutex<Vec<u8>>>,
    handle: OwnedFd,
}

impl Read for Unclosed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.sent.position() == self.sent.get_ref().len() as u64 {
            return Err(io::Error::other("the relay read past what its host sent"));
        }
        self.sent.read(buf)
    }
}

impl Write for Unclosed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.answers.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Duplex for Unclosed {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn shutdown(&self, _: Shutdown) -> io::Result<()> {
        Ok(())
    }

    fn set_write_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn set_read_timeout(&self, _: Option<Duration>) -> io::Result<()> {
        Ok(())
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        self.handle.as_fd()
    }

    fn set_nonblocking(&self, _: bool) -> io::Result<()> {
        Ok(())
    }
}

/// Serves `sent` on one staging connection its host never half-closes: the relay's messages, in order.
fn serve_unclosed(staged: &Path, sent: Vec<u8>) -> Vec<Vec<u8>> {
    let answers = Arc::new(Mutex::new(Vec::new()));
    files::serve(
        Box::new(Unclosed {
            sent: io::Cursor::new(sent),
            answers: Arc::clone(&answers),
            handle: File::open("/dev/null").unwrap().into(),
        }),
        staged,
        &hf::LIMITS,
    );
    let bytes = answers.lock().unwrap().clone();
    let (mut deframer, mut rest, mut messages) =
        (Deframer::new(wire::MAX_MESSAGE), &bytes[..], Vec::new());
    while !rest.is_empty() {
        let (took, whole) = deframer.take(rest).unwrap();
        rest = &rest[took..];
        messages.extend(whole);
    }
    messages
}

/// The answer a relay's message frames.
fn answer_in(message: &[u8]) -> Answer {
    let received = Received {
        bytes: message,
        size: message.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_answer(&received, wire::MAX_MESSAGE).unwrap().1
}

/// AC-T7.4.1 (original): a stage's stream ends at its end head and a removal's at its opening
/// (`hex_core::stream::files`), and the relay answers there. Serve each on a connection its host never half-closes:
/// expect the stage's budget and `Done`, its copy whole, then the removal's `Done`, its copy gone. Over Firecracker the
/// relay answered only at a half-close, which the muxer had made a full shutdown, so no answer came
/// (native_firecracker_smoke: "the guest's relay closed a staging connection without an answer").
#[test]
fn original_the_relay_answers_a_stage_and_a_removal_at_their_in_band_end() {
    let (_lab, staged) = staged_lab();
    let stage = serve_unclosed(&staged, raw_stage(&[("d", hf::What::Dir)]));
    assert_eq!(stage.len(), 2, "the stage's budget and its answer");
    assert!(hf::read_budget(&stage[0]).is_some());
    assert_eq!(answer_in(&stage[1]), Answer::Done);
    assert!(staged.join("b/d").is_dir());
    let removal = serve_unclosed(&staged, hf::open(hf::Op::Remove, "b").unwrap());
    assert_eq!(
        removal.iter().map(|m| answer_in(m)).collect::<Vec<_>>(),
        [Answer::Done]
    );
    assert!(!staged.join("b").exists());
}

/// AC-T7.4.1: the in-band end is the stream's last byte. Serve a stage, then a removal, each with three bytes after its
/// end on a connection never half-closed: expect each refused by type (`3 bytes follow the stream's end`) and acted on
/// by neither: no copy staged, and the copy the removal named kept.
#[test]
fn the_relay_refuses_bytes_after_a_streams_in_band_end_before_acting_on_it() {
    let (lab, staged) = staged_lab();
    let trailing = |mut sent: Vec<u8>| {
        sent.extend([7u8; 3]);
        sent
    };
    let after_end = |answer: Answer| matches!(answer, Answer::Refused(Refusal::Invalid { why, .. }) if why == "3 bytes follow the stream's end");
    let stage = serve_unclosed(&staged, trailing(raw_stage(&[("d", hf::What::Dir)])));
    assert!(after_end(answer_in(stage.last().unwrap())));
    assert!(!staged.join("b").exists());
    assert_eq!(
        stage_raw(&lab, &raw_stage(&[("d", hf::What::Dir)])),
        Answer::Done
    );
    let removal = serve_unclosed(&staged, trailing(hf::open(hf::Op::Remove, "b").unwrap()));
    assert!(after_end(answer_in(removal.last().unwrap())));
    assert!(staged.join("b/d").is_dir());
}

/// A staging connection as Firecracker's vsock muxer carries it: a host's half-close is the connection's full
/// shutdown, after which nothing the guest sends reaches the host (`vsock/csm/connection.rs:228-239` and `:307` at
/// 500efc6). Its `shutdown(Write)` shuts both ways, so what the relay writes after it fails, as the muxer drops it.
struct Muxed(UnixStream);

impl Read for Muxed {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Muxed {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Duplex for Muxed {
    fn try_clone(&self) -> io::Result<hex_client::guest::DuplexOwner> {
        hex_client::guest::own_duplex(Muxed(self.0.try_clone()?))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(match how {
            Shutdown::Write => Shutdown::Both,
            how => how,
        })
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn handle(&self) -> hex_client::guest::Handle<'_> {
        self.0.as_fd()
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// The relay's staging port, each connection [`Muxed`].
struct MuxedPorts(PathBuf);

impl Link for MuxedPorts {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        assert_eq!(port, hf::FILES_PORT);
        hex_client::guest::own_duplex(Muxed(UnixStream::connect(&self.0).unwrap()))
    }
}

/// AC-T7.4.1 (original): stage a box and remove its copy through a port whose half-close is a full shutdown, as
/// Firecracker's is: expect both answered, the copy staged and then gone. The host half-closed before reading each
/// answer, so over Firecracker it read the connection's end instead (native_firecracker_smoke: "the guest's relay
/// closed a staging connection without an answer").
#[test]
fn original_staging_through_a_port_whose_half_close_is_a_full_shutdown_is_answered() {
    use hex_client::guest::stage::{Item, Source, stage, unstage};
    let (lab, staged) = staged_lab();
    let link = hex_client::guest::own_link(MuxedPorts(lab.dir.path().join("files.sock"))).unwrap();
    let id = BoxId::new("box-1").unwrap();
    let items = [Item {
        name: "config.json".to_owned(),
        source: Source::Bytes(b"{}".to_vec()),
    }];
    let staged_it = stage(link.as_ref(), &id, &items, &hf::LIMITS).map(|s| s.learned.len());
    let copied = std::fs::read(staged.join("box-1/config.json")).ok();
    let removed = unstage(link.as_ref(), &id);
    assert_eq!(
        (staged_it, copied, removed),
        (Ok(0), Some(b"{}".to_vec()), Ok(()))
    );
    assert!(!staged.join("box-1").exists());
}
