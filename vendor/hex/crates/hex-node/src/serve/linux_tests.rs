//! The socket's own tests (decision 3, HEX-SOCK-3 to 8 and 10): a live server in this process, over a host
//! whose boxes are the simulator's and whose programs are real processes of this test, so the
//! server's pidfd watch, its bounds and the disconnect rule meet real programs without a cgroup.
//! Hostile bytes go in through a raw socket; the rest through `hex-client`, as a caller sends them.

use super::*;
use std::io::{Read, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::net::{AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType};
use rustix::process::{Pid, Signal};

use hex_api::conformance::end_of;
use hex_api::sim::SimRuntime;
use hex_api::{Bounds, BoxInfo, BoxSpec, ClassSpec, Signal as HexSignal, Usage};
use hex_client::Client;
use hex_core::patience::Patience;
use hex_testkit::waits::{EMPTYING, EXEC, until};

/// Longest a test waits for the server's answer to a call over its socket: twice the slowest recorded answer of the
/// kinds these tests ask (hello, inspect, netns counts, exec), an exec's (`client.answer.exec`, the census's debug
/// arm, as the tests run debug builds: hex_testkit::waits::EXEC), so a server that never answers fails the test
/// instead of hanging it. A program's end is followed to it as the suites follow one ([`end_of`]); each other fact
/// takes its own record.
const ANSWERED: Duration = EXEC.saturating_mul(2);

/// The server's ending of a program whose client went away: its kill, its reaping and its count in its box, a box's
/// emptying's work, which the server's own wait for the programs it ends takes the record of (decision 9's table:
/// `node.emptying`).
static ABANDONED: Patience = Patience::seeded("test.serve.abandoned", EMPTYING);
/// The I/O thread's letting go of a connection that has closed, and its admitting of one that has connected: no
/// recorded operation makes either, and no census row times them yet, so these wait measuring, for the fact alone, each
/// answer recorded for the census to come, from the suites run under it.
static LET_GO: Patience = Patience::measuring("test.serve.let_go");
static ADMITTED: Patience = Patience::measuring("test.serve.admitted");

/// Shuts a host's inspect until opened: holds calls in flight on purpose. A held call waits for the test to open the
/// gate, the fact it needs, never for a time (CLAUDE.md §4): a test holds it shut across looks of its own, `quiet`'s
/// among them, so a gate that opened itself after a patience would answer a call the test is checking is held. Its
/// bound is the test's end: a test's [`Lab`] opens it as it drops, before the server stops, a failed assertion's
/// unwinding too, so no stop waits on a held call (CLAUDE.md §2).
#[derive(Default)]
struct Gate {
    shut: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn set(&self, shut: bool) {
        *self.shut.lock().unwrap_or_else(PoisonError::into_inner) = shut;
        self.opened.notify_all();
    }

    fn pass(&self) {
        let mut shut = self.shut.lock().unwrap();
        while *shut {
            shut = self.opened.wait(shut).unwrap();
        }
    }
}

/// Whether a guest's stand-in program has ended, and the condition its waiters wait on: it ends once the test says.
type GuestEnd = Arc<(Mutex<bool>, Condvar)>;

/// A stand-in for a program in a VM's guest as its backend's client hands it over: its pid the guest's, which names no
/// process of this host or an unrelated one, and its end the guest's message, which comes once the test lets it.
struct GuestProgram {
    pid: u32,
    /// Whether it says it is a process of this host: a backend's that is, as a misbehaving one would.
    on_this_host: bool,
    ended: GuestEnd,
}

impl Proc for GuestProgram {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn on_this_host(&self) -> bool {
        self.on_this_host
    }
    fn try_wait(&mut self) -> Result<Option<hex_api::Exit>, Refusal> {
        let ended = *self.ended.0.lock().unwrap();
        Ok(if ended {
            Some(hex_api::Exit::Code(7))
        } else {
            None
        })
    }
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<hex_api::Exit>, Refusal> {
        let (ended, came) = &*self.ended;
        let (ended, _) = came
            .wait_timeout_while(ended.lock().unwrap(), timeout, |ended| !*ended)
            .unwrap();
        Ok(if *ended {
            Some(hex_api::Exit::Code(7))
        } else {
            None
        })
    }
    /// The guest's kill: its end, as the guest tells it.
    fn kill(&mut self) -> Result<(), Refusal> {
        *self.ended.0.lock().unwrap() = true;
        self.ended.1.notify_all();
        Ok(())
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// Boxes in the simulator; programs real processes of this test, started on the request's stdio, or, where a test
/// sets [`TestHost::guest`], a VM guest's stand-in.
#[derive(Default)]
struct TestHost {
    sim: SimRuntime,
    gate: Gate,
    /// The pid an exec's program has, whether it says it is this host's, and its end: an exec then hands over a guest's
    /// program ([`GuestProgram`]) rather than start a process.
    guest: Mutex<Option<(u32, bool, GuestEnd)>>,
    abandoned: Mutex<Vec<BoxId>>,
    /// Each descriptor an exec's streams came to the host as, or that it made for those asked for, by what it is, and
    /// whether it was close-on-exec as the host started the program.
    cloexec: Mutex<Vec<(&'static str, bool)>>,
}

impl Runtime for TestHost {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: Program,
        mut stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.inspect(id)?;
        if let Some((pid, on_this_host, ended)) = self.guest.lock().unwrap().clone() {
            return Ok(Box::new(GuestProgram {
                pid,
                on_this_host,
                ended,
            }));
        }
        let given = stdio.make_asked()?;
        let ends = [
            ("the program's stdin", stdio.stdin.as_ref().map(AsFd::as_fd)),
            (
                "the program's stdout",
                stdio.stdout.as_ref().map(AsFd::as_fd),
            ),
            (
                "the program's stderr",
                stdio.stderr.as_ref().map(AsFd::as_fd),
            ),
            ("the caller's stdin", given.stdin.as_ref().map(AsFd::as_fd)),
            (
                "the caller's stdout",
                given.stdout.as_ref().map(AsFd::as_fd),
            ),
            (
                "the caller's stderr",
                given.stderr.as_ref().map(AsFd::as_fd),
            ),
        ];
        self.cloexec.lock().unwrap().extend(
            ends.into_iter()
                .filter_map(|(what, end)| end.map(|end| (what, cloexec(end)))),
        );
        let (first, rest) = program.args.split_first().unwrap();
        let mut cmd = std::process::Command::new(first);
        cmd.args(rest).env_clear().envs(program.env.iter().cloned());
        let null = std::process::Stdio::null;
        cmd.stdin(stdio.stdin.map_or_else(null, std::process::Stdio::from));
        cmd.stdout(stdio.stdout.map_or_else(null, std::process::Stdio::from));
        cmd.stderr(stdio.stderr.map_or_else(null, std::process::Stdio::from));
        let child = cmd.spawn().map_err(|e| Refusal::os("spawning", e))?;
        let proc = hex_runtime::linux::proc::PidfdProc::of_child(child.id())?;
        Ok(Box::new(hex_api::Asked::new(proc, given)))
    }
    /// The run's box in the simulator, its program a real process of this test as an exec's is.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        let id = spec.id.clone();
        self.sim.create(spec)?;
        self.exec(&id, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: HexSignal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.gate.pass();
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

impl Host for TestHost {
    fn abandoned(&self, id: &BoxId) -> Result<(), Refusal> {
        self.abandoned.lock().unwrap().push(id.clone());
        Ok(())
    }

    fn netns(&self) -> Result<netns::Counts, Refusal> {
        Ok(counts())
    }
}

/// The test host's network namespace counts, each field a different value.
fn counts() -> netns::Counts {
    netns::Counts {
        made_ahead: 5,
        taken: 4,
        made_on_path: 3,
        discarded: 2,
        failed: 15,
        failed_by: netns::FailedBy {
            home: 1,
            unshare: 2,
            open: 3,
            loopback: 4,
            back: 5,
        },
        refused: None,
        refused_on_path: 6,
        last_failed: netns::LastFailed {
            unshare: Some(netns::Reason::new(Some(1), "unshare failed: EPERM")),
            ..netns::LastFailed::default()
        },
    }
}

struct Lab {
    server: Server,
    host: Arc<TestHost>,
    _dir: tempfile::TempDir,
}

impl Lab {
    fn new(limits: Limits) -> Self {
        Self::with(limits, 8)
    }

    /// A lab whose server runs calls on at most `workers` threads.
    fn with(limits: Limits, workers: usize) -> Self {
        let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/serve");
        std::fs::create_dir_all(&cache).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("sock-")
            .tempdir_in(&cache)
            .unwrap();
        let host = Arc::new(TestHost::default());
        host.sim
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new("b").unwrap(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            })
            .unwrap();
        let server = Server::bind(
            &dir.path().join("node.sock"),
            Arc::clone(&host) as Arc<dyn Host>,
            limits,
            workers,
        )
        .unwrap();
        Self {
            server,
            host,
            _dir: dir,
        }
    }

    fn client(&self) -> Client {
        Client::connect(self.server.path()).unwrap()
    }

    /// A connection that sends what it is given, hostile or not.
    fn raw(&self) -> OwnedFd {
        let fd = rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        rustix::net::connect(&fd, &SocketAddrUnix::new(self.server.path()).unwrap()).unwrap();
        fd
    }

    fn connections(&self) -> usize {
        self.server.shared.table.lock().unwrap().conns.len()
    }

    fn watched(&self) -> usize {
        self.server.shared.table.lock().unwrap().watched.len()
    }
}

impl Drop for Lab {
    /// Opens the host's gate before the server stops (this runs before the fields drop), so a call the test left held
    /// is let through and the server's stop never waits on it.
    fn drop(&mut self) {
        self.host.gate.set(false);
    }
}

fn limits() -> Limits {
    Limits {
        connections: 4,
        message_bytes: 4096,
        in_flight: 4,
        programs: 4,
    }
}

fn send_raw(fd: &OwnedFd, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = rustix::net::SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() {
        assert!(control.push(rustix::net::SendAncillaryMessage::ScmRights(fds)));
    }
    rustix::net::sendmsg(
        fd,
        &[std::io::IoSlice::new(bytes)],
        &mut control,
        SendFlags::NOSIGNAL,
    )
    .unwrap();
}

/// [`ANSWERED`] as poll(2) takes it: its whole seconds and the nanoseconds past them. Its seconds alone would make a
/// bound under a second zero, a poll that looks once and returns.
fn answer_wait() -> Timespec {
    Timespec {
        tv_sec: i64::try_from(ANSWERED.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(ANSWERED.subsec_nanos()),
    }
}

/// The next message the server sends, waiting at most [`ANSWERED`] for it.
fn answer_of(fd: &OwnedFd) -> (Header, Answer) {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    assert_eq!(
        rustix::event::poll(&mut fds, Some(&answer_wait())).unwrap(),
        1,
        "no answer came"
    );
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let n = rustix::net::recv(fd, &mut buf, rustix::net::RecvFlags::empty())
        .unwrap()
        .0;
    wire::read_answer(&received(&buf[..n]), wire::MAX_MESSAGE).unwrap()
}

/// The next message the server sends and the descriptors that ride with it, waiting at most [`ANSWERED`].
fn answer_with_ends(fd: &OwnedFd) -> (Header, Answer, Vec<OwnedFd>) {
    let mut ready = [PollFd::new(fd, PollFlags::IN)];
    assert_eq!(
        rustix::event::poll(&mut ready, Some(&answer_wait())).unwrap(),
        1,
        "no answer came"
    );
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let mut control = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        fd,
        &mut [std::io::IoSliceMut::new(&mut buf)],
        &mut control,
        rustix::net::RecvFlags::CMSG_CLOEXEC,
    )
    .unwrap();
    let mut ends = Vec::new();
    for m in control.drain() {
        if let rustix::net::RecvAncillaryMessage::ScmRights(fds) = m {
            ends.extend(fds);
        }
    }
    let message = Received {
        bytes: &buf[..msg.bytes],
        size: msg.bytes,
        descriptors: ends.len(),
        descriptors_cut: false,
    };
    let (h, answer) = wire::read_answer(&message, wire::MAX_MESSAGE).unwrap();
    (h, answer, ends)
}

/// Whether the server sends anything more within a short look: for "nothing more came".
fn quiet(fd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: 0,
        tv_nsec: 200_000_000,
    };
    rustix::event::poll(&mut fds, Some(&ts)).unwrap() == 0
}

fn received(bytes: &[u8]) -> Received<'_> {
    Received {
        bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    }
}

fn request(id: u64, call: Call) -> Vec<u8> {
    wire::request(id, &Request::Call(call), wire::MAX_MESSAGE).unwrap()
}

/// A hello of this wire and this build, as request `id`.
fn hello_of_this_wire(id: u64) -> Vec<u8> {
    let hello = Request::Hello {
        build: wire::build(),
    };
    wire::request(id, &hello, wire::MAX_MESSAGE).unwrap()
}

/// `bytes`, a message of this wire, as the wire whose identity is `theirs` would send it: the identity is the
/// header's bytes 4 to 12 in every build.
fn of_wire(mut bytes: Vec<u8>, theirs: u64) -> Vec<u8> {
    bytes[4..12].copy_from_slice(&theirs.to_le_bytes());
    bytes
}

fn b() -> BoxId {
    BoxId::new("b").unwrap()
}

/// Waits for `fact`, the server's, on `wait`, its own record's patience or measuring (hex_testkit::waits).
fn wait_until(wait: &Patience, what: &str, fact: impl FnMut() -> bool) {
    until(wait, what, fact).unwrap();
}

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
        s.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

struct Hostile<'a>(&'a str, Vec<u8>, Vec<BorrowedFd<'a>>, u64, &'a str);

/// HEX-SOCK-3: hostile messages to a live server (truncated, empty, not Hex's, an unknown kind, an answer sent
/// as a request, trailing bytes, oversized, descriptors where none belong and more than a message takes) are each
/// refused by type, to the request their header names, and the connection is served after each; another
/// connection is served too. A message of another wire is refused by its own test
/// (`a_client_of_another_wire_is_refused_at_its_hello`).
#[test]
fn hostile_messages_are_refused_and_the_connection_is_served_after() {
    let lab = Lab::new(limits());
    let fd = lab.raw();
    let hello = hello_of_this_wire(9);
    let with = |at: usize, two: [u8; 2]| {
        let mut m = hello.clone();
        m[at..at + 2].copy_from_slice(&two);
        m
    };
    let mut trailing = request(5, Call::Inspect(b()));
    trailing.push(0);
    let mut oversized = request(6, Call::Inspect(b()));
    oversized.resize(4097, 0);
    let (pipe_r, _pipe_w) = std::io::pipe().unwrap();
    let exec_stdout = request(
        8,
        Call::Exec {
            id: b(),
            program: Program::of(&["true"]),
            streams: wire::Streams {
                stdout: true,
                ..wire::Streams::default()
            },
        },
    );
    let p = pipe_r.as_fd();
    // Each: what it is, its bytes, the descriptors sent with it, the id its refusal goes to, and
    // what the refusal says.
    let cases: Vec<Hostile<'_>> = vec![
        Hostile("empty", Vec::new(), vec![], 0, "short of"),
        Hostile(
            "truncated header",
            b"HEXW\x01\x00".to_vec(),
            vec![],
            0,
            "short of",
        ),
        Hostile(
            "not Hex's",
            b"GET / HTTP/1.1\r\n\r\n".to_vec(),
            vec![],
            0,
            "HEXW",
        ),
        // Request kinds are numbered up from 0x0001 as the wire grows (0x0013 became
        // PHYSICAL_REPORT), so the probe is the request range's last kind.
        Hostile(
            "unknown kind",
            with(12, [0xff, 0x7f]),
            vec![],
            9,
            "kind 0x7fff isn't a request this wire defines",
        ),
        Hostile(
            "an answer kind",
            with(12, [0x03, 0x80]),
            vec![],
            9,
            "kind 0x8003",
        ),
        Hostile(
            "trailing bytes",
            trailing,
            vec![],
            5,
            "follow what this wire defines",
        ),
        Hostile(
            "oversized",
            oversized,
            vec![],
            6,
            "4097 bytes is over the 4096-byte bound",
        ),
        Hostile(
            "a descriptor on an inspect",
            request(7, Call::Inspect(b())),
            vec![p],
            7,
            "an inspect takes 0",
        ),
        Hostile(
            "no descriptor on an exec naming stdout",
            exec_stdout.clone(),
            vec![],
            8,
            "an exec takes 1",
        ),
        Hostile(
            "more descriptors than a message takes",
            exec_stdout,
            vec![p, p, p, p],
            8,
            "more descriptors than the 3",
        ),
    ];
    for Hostile(name, bytes, fds, id, says) in cases {
        send_raw(&fd, &bytes, &fds);
        match answer_of(&fd) {
            (
                h,
                Answer::Refused(Refusal::Invalid {
                    what: "message",
                    why,
                }),
            ) => {
                assert_eq!(h.id, id, "{name}: answered to {}", h.id);
                assert!(why.contains(says), "{name}: {why}");
            }
            other => panic!("{name}: {other:?}"),
        }
        send_raw(&fd, &hello, &[]);
        assert!(
            matches!(answer_of(&fd), (Header { id: 9, .. }, Answer::Hello(_))),
            "{name}: the connection wasn't served after"
        );
    }
    assert!(quiet(&fd), "an answer came that no request asked for");
    let other = lab.raw();
    send_raw(&other, &hello, &[]);
    assert!(matches!(answer_of(&other), (_, Answer::Hello(_))));
}

/// HEX-SOCK-10: the node's network namespace counts cross the socket as its host holds them: asked
/// through `hex-client`, and on a raw connection, answered with its request's id. The test times nothing, so the
/// client's hello and call each take [`ANSWERED`], the test's own bound on an answer, as their caller's deadline: an
/// answer slower than its learned wait, as under parallel suites, is a run-out recorded, not a refusal (AC-T7.4.1).
#[test]
fn the_network_namespace_counts_cross_the_socket() {
    let lab = Lab::new(limits());
    let answered = || Instant::now() + ANSWERED;
    let client = Client::connect_before(lab.server.path(), answered()).unwrap();
    assert_eq!(client.netns_before(answered()).unwrap(), counts());
    let fd = lab.raw();
    send_raw(&fd, &request(4, Call::Netns), &[]);
    let (h, answer) = answer_of(&fd);
    assert_eq!((h.id, answer), (4, Answer::Netns(counts())));
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule): a client of another wire is refused at
/// its hello, and nothing it sends is read as this wire's. Do: on a raw connection, a hello as a client of another wire
/// sends it (its identity and its build, in the hello's head every build lays out alike); on another, greeted in this
/// wire, a delete of box b as the other wire would send it; then inspect b through `hex-client`, of this wire. Expect:
/// each answered with the node's own hello, of this wire and this build, to its id, which a client of another wire
/// refuses naming both (hex-client's own tests), then its connection's end, nothing more sent; b kept; the inspect
/// answered.
#[test]
fn a_client_of_another_wire_is_refused_at_its_hello() {
    let lab = Lab::new(limits());
    let theirs = !wire::IDENTITY;
    let hello = Request::Hello {
        build: "hex 9.9.9 (another wire)".to_owned(),
    };
    let their_hello = of_wire(wire::request(9, &hello, wire::MAX_MESSAGE).unwrap(), theirs);
    let their_delete = of_wire(request(2, Call::Delete(b())), theirs);
    let refused = lab.raw();
    send_raw(&refused, &their_hello, &[]);
    let greeted = lab.raw();
    send_raw(&greeted, &hello_of_this_wire(1), &[]);
    assert!(matches!(answer_of(&greeted).1, Answer::Hello(_)));
    send_raw(&greeted, &their_delete, &[]);
    for (fd, id) in [(&refused, 9), (&greeted, 2)] {
        match answer_of(fd) {
            (h, Answer::Hello(told)) if h.id == id => assert_eq!(told.build, wire::build()),
            other => panic!("the node's own hello to {id}, not {other:?}"),
        }
        let mut after = [0u8; 64];
        let mut readable = [PollFd::new(fd, PollFlags::IN)];
        assert_eq!(
            rustix::event::poll(&mut readable, Some(&answer_wait())).unwrap(),
            1,
            "the node kept the connection open"
        );
        assert_eq!(
            rustix::net::recv(fd, &mut after, rustix::net::RecvFlags::empty())
                .unwrap()
                .0,
            0,
            "the node sent more than its hello"
        );
    }
    wait_until(&LET_GO, "the refused connections to close", || {
        lab.connections() == 0
    });
    assert_eq!(lab.host.sim.inspect(&b()).unwrap().id, b());
    assert_eq!(lab.client().inspect(&b()).unwrap().id, b());
}

/// HEX-SOCK-4: connections past the bound are refused by type, naming it, without disturbing the
/// connections the server holds; once one closes, the next is served.
#[test]
fn connections_past_the_bound_are_refused_naming_it() {
    let lab = Lab::new(Limits {
        connections: 2,
        ..limits()
    });
    let (a, c) = (lab.client(), lab.client());
    assert_eq!(
        Client::connect(lab.server.path()).err(),
        Some(Refusal::Exhausted {
            what: CONNECTIONS,
            bound: 2
        })
    );
    assert_eq!(a.inspect(&b()).unwrap().id, b());
    assert_eq!(c.list().unwrap().len(), 1);
    drop(a);
    wait_until(&LET_GO, "the closed connection to be let go", || {
        lab.connections() == 1
    });
    let d = lab.client();
    assert_eq!(d.inspect(&b()).unwrap().id, b());
    assert_eq!(c.inspect(&b()).unwrap().id, b());
}

/// HEX-SOCK-5: requests past a connection's in-flight bound are refused by type, naming it, while the
/// ones within it are carried out: every request gets exactly one answer, and none is dropped.
#[test]
fn requests_past_the_in_flight_bound_are_refused_and_none_is_lost() {
    let lab = Lab::new(Limits {
        in_flight: 2,
        ..limits()
    });
    let fd = lab.raw();
    lab.host.gate.set(true);
    for id in 1..=3 {
        send_raw(&fd, &request(id, Call::Inspect(b())), &[]);
    }
    assert_eq!(
        answer_of(&fd),
        (
            Header {
                kind: 0x8007,
                id: 3
            },
            Answer::Refused(Refusal::Exhausted {
                what: IN_FLIGHT,
                bound: 2
            })
        )
    );
    assert!(
        quiet(&fd),
        "a held request was answered while the gate was shut"
    );
    lab.host.gate.set(false);
    let mut ids: Vec<u64> = (0..2)
        .map(|_| match answer_of(&fd) {
            (h, Answer::Info(info)) if info.id == b() => h.id,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2]);
    send_raw(&fd, &request(4, Call::Inspect(b())), &[]);
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 4, .. }, Answer::Info(_))
    ));
    assert!(quiet(&fd), "more answers than requests");
}

/// HEX-SOCK-7: execs past the programs a connection watches are refused by type, naming the bound;
/// a program's end gives its place back before the end is reported.
#[test]
fn execs_past_the_programs_bound_are_refused_naming_it() {
    let lab = Lab::new(Limits {
        programs: 1,
        ..limits()
    });
    let client = lab.client();
    let mut first = client
        .exec(&b(), Program::of(&["sleep", "1000"]), Stdio::null())
        .unwrap();
    assert_eq!(
        client
            .exec(&b(), Program::of(&["true"]), Stdio::null())
            .err(),
        Some(Refusal::Exhausted {
            what: PROGRAMS,
            bound: 1
        })
    );
    rustix::process::kill_process(
        Pid::from_raw(i32::try_from(first.pid()).unwrap()).unwrap(),
        Signal::KILL,
    )
    .unwrap();
    assert_eq!(end_of(&mut first).unwrap(), Exit::Signal(9));
    let mut next = client
        .exec(&b(), Program::of(&["true"]), Stdio::null())
        .unwrap();
    assert_eq!(end_of(&mut next).unwrap(), Exit::Code(0));
}

/// HEX-SOCK-8: an exec's stdio crosses as descriptors: what the program writes reaches the pipes the
/// client holds, what the client writes reaches it, and its status comes back; a descriptor that
/// isn't the right end of a pipe is refused by name.
#[test]
fn stdio_crosses_as_descriptors_and_only_pipes_are_taken() {
    let lab = Lab::new(limits());
    let client = lab.client();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let (mut err_r, err_w) = std::io::pipe().unwrap();
    let mut p = client
        .exec(
            &b(),
            Program::of(&["sh", "-c", "echo hi; echo oops >&2; exit 3"]),
            Stdio {
                stdin: None,
                stdout: Some(out_w),
                stderr: Some(err_w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let (mut out, mut err) = (String::new(), String::new());
    out_r.read_to_string(&mut out).unwrap();
    err_r.read_to_string(&mut err).unwrap();
    assert_eq!((out.as_str(), err.as_str()), ("hi\n", "oops\n"));
    assert_eq!(end_of(&mut p).unwrap(), Exit::Code(3));
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut cat = client
        .exec(
            &b(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                stderr: None,
                ..Stdio::null()
            },
        )
        .unwrap();
    in_w.write_all(b"round trip\n").unwrap();
    drop(in_w);
    let mut back = String::new();
    out_r.read_to_string(&mut back).unwrap();
    assert_eq!(back, "round trip\n");
    assert_eq!(end_of(&mut cat).unwrap(), Exit::Code(0));

    let fd = lab.raw();
    let devnull = std::fs::File::open("/dev/null").unwrap();
    let (read_end, _write_end) = std::io::pipe().unwrap();
    let exec = request(
        1,
        Call::Exec {
            id: b(),
            program: Program::of(&["true"]),
            streams: wire::Streams {
                stdout: true,
                ..wire::Streams::default()
            },
        },
    );
    for (fd_given, says) in [
        (devnull.as_fd(), "stdout isn't a pipe"),
        (read_end.as_fd(), "stdout is a pipe's read end"),
    ] {
        send_raw(&fd, &exec, &[fd_given]);
        match answer_of(&fd) {
            (Header { id: 1, .. }, Answer::Refused(Refusal::Invalid { what: "stdio", why })) => {
                assert!(why.contains(says), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(lab.watched(), 0, "a refused exec started a program");
}

/// HEX-SOCK-6, the disconnect rule: a client that goes away while its program runs (its connection
/// closes, no release sent, as when its process dies) has the program sent KILL, reaped, and
/// counted in its box; none is left running unwatched.
#[test]
fn a_program_whose_client_goes_away_is_killed_reaped_and_counted() {
    let lab = Lab::new(limits());
    let fd = lab.raw();
    send_raw(
        &fd,
        &request(
            1,
            Call::Exec {
                id: b(),
                program: Program::of(&["sleep", "1000"]),
                streams: wire::Streams::default(),
            },
        ),
        &[],
    );
    let pid = match answer_of(&fd) {
        (Header { id: 1, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    assert!(alive(pid));
    drop(fd);
    wait_until(
        &ABANDONED,
        "the abandoned program to be counted in its box",
        || lab.host.abandoned.lock().unwrap().as_slice() == [b()],
    );
    // The count is the last of the server's ending of the program, after its kill and reaping, so counted, it is gone:
    // the fact is made within that ending, whose record bounds the wait.
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the program wasn't reaped"
    );
    assert_eq!((lab.connections(), lab.watched()), (0, 0));
}

/// HEX-SOCK-6: a program whose `Proc` the client dropped was released: it runs on after the client
/// goes, uncounted, as dropping an in-process `Proc` lets it.
#[test]
fn a_released_program_runs_on_after_its_client_goes() {
    let lab = Lab::new(limits());
    let client = lab.client();
    let proc = client
        .exec(&b(), Program::of(&["sleep", "1000"]), Stdio::null())
        .unwrap();
    let pid = proc.pid();
    drop(proc);
    drop(client);
    wait_until(&LET_GO, "the connection to close", || {
        lab.connections() == 0
    });
    assert_eq!(lab.watched(), 0);
    assert!(alive(pid), "a released program was ended");
    assert!(lab.host.abandoned.lock().unwrap().is_empty());
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, Signal::KILL).unwrap();
    rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()).unwrap();
}

/// The socket at a path: one an ended agent left is replaced, one a live agent serves is refused, and
/// a file that isn't a socket is refused, never removed.
#[test]
fn a_socket_path_is_taken_only_from_an_ended_agent() {
    let lab = Lab::new(limits());
    let path = lab.server.path().to_owned();
    let host = Arc::clone(&lab.host) as Arc<dyn Host>;
    assert!(matches!(
        Server::bind(&path, Arc::clone(&host), limits(), 1),
        Err(Refusal::Invalid { what: "socket path", why }) if why.contains("served by a node agent already")
    ));
    let file = path.with_file_name("not-a-socket");
    std::fs::write(&file, b"keep").unwrap();
    assert!(matches!(
        Server::bind(&file, Arc::clone(&host), limits(), 1),
        Err(Refusal::Invalid { what: "socket path", why }) if why.contains("isn't a socket")
    ));
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    // A socket no agent listens on: bound but never listened, as one an agent killed mid-start leaves.
    let stale = path.with_file_name("stale.sock");
    let left = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&left, &SocketAddrUnix::new(&stale).unwrap()).unwrap();
    drop(left);
    let again = Server::bind(&stale, host, limits(), 1).unwrap();
    assert!(Client::connect(again.path()).is_ok());
}

/// A list longer than one message comes whole, in order, over the socket.
#[test]
fn a_list_longer_than_one_message_comes_whole() {
    let lab = Lab::new(limits());
    for n in 0..200 {
        lab.host
            .sim
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: BoxId::new(&format!("many-{n:04}")).unwrap(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
                band: 0,
            })
            .unwrap();
    }
    let listed = lab.client().list().unwrap();
    assert_eq!(listed.len(), 201);
    assert_eq!(listed, lab.host.sim.list().unwrap());
}

/// HEX-SOCK-6: a client that closes with an answer unread leaves the server a reset, reported once
/// and before what it sent; the server still reads what it sent, so a release sent just before it
/// went lets its program run on rather than be ended as abandoned.
#[test]
fn a_release_sent_before_a_reset_is_honoured() {
    let lab = Lab::new(limits());
    let fd = lab.raw();
    send_raw(
        &fd,
        &request(
            1,
            Call::Exec {
                id: b(),
                program: Program::of(&["sleep", "1000"]),
                streams: wire::Streams::default(),
            },
        ),
        &[],
    );
    let pid = match answer_of(&fd) {
        (Header { id: 1, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    // An answer left unread at close is what makes the kernel report a reset to the server.
    send_raw(&fd, &request(2, Call::Inspect(b())), &[]);
    let mut readable = [PollFd::new(&fd, PollFlags::IN)];
    assert_eq!(
        rustix::event::poll(&mut readable, Some(&answer_wait())).unwrap(),
        1
    );
    {
        // The I/O thread takes the table on every event, so while it is held here the release and
        // the close both land before the server reads either: the reset comes first.
        let _held = lab.server.shared.table.lock().unwrap();
        send_raw(
            &fd,
            &wire::request(1, &Request::Release, wire::MAX_MESSAGE).unwrap(),
            &[],
        );
        drop(fd);
    }
    wait_until(&LET_GO, "the connection to close", || {
        lab.connections() == 0
    });
    assert_eq!(lab.watched(), 0);
    assert!(alive(pid), "the released program was ended");
    assert!(lab.host.abandoned.lock().unwrap().is_empty());
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, Signal::KILL).unwrap();
    rustix::process::waitpid(Some(pid), rustix::process::WaitOptions::empty()).unwrap();
}

/// HEX-SOCK-5, the lead's call of 2026-11-06 (AC-T7.1.3): calls in flight are one budget for the node, not a share
/// for each connection; the node's clients are its own control plane, so none is fair to another. Do: two
/// connections under a node-wide bound of 2, the host's gate shut, one call on the first and two on the second.
/// Expect: the third call, whichever connection it is on, refused naming the bound; the two within it answered once
/// the gate opens.
#[test]
fn calls_in_flight_are_bounded_for_the_node_not_each_connection() {
    let lab = Lab::new(Limits {
        in_flight: 2,
        ..limits()
    });
    let (one, two) = (lab.raw(), lab.raw());
    lab.host.gate.set(true);
    send_raw(&one, &request(1, Call::Inspect(b())), &[]);
    send_raw(&two, &request(2, Call::Inspect(b())), &[]);
    send_raw(&two, &request(3, Call::Inspect(b())), &[]);
    assert_eq!(
        answer_of(&two).1,
        Answer::Refused(Refusal::Exhausted {
            what: IN_FLIGHT,
            bound: 2
        })
    );
    lab.host.gate.set(false);
    assert!(matches!(
        answer_of(&one),
        (Header { id: 1, .. }, Answer::Info(_))
    ));
    assert!(matches!(
        answer_of(&two),
        (Header { id: 2, .. }, Answer::Info(_))
    ));
}

/// HEX-SOCK-7 and the lead's ruling of 2026-11-06 (AC-T7.1.3): the programs watched are one budget for the node, and
/// each admitted connection keeps the one its hello promised. Do: two connections under a node-wide budget of 3, an
/// exec on the first until refused, then one on the second. Expect: the first starts two, its third refused naming
/// the node's budget, since the second connection's one is kept for it; the second's starts.
#[test]
fn programs_watched_are_bounded_for_the_node_and_each_connection_keeps_its_one() {
    let lab = Lab::new(Limits {
        programs: 3,
        ..limits()
    });
    let (a, c) = (lab.client(), lab.client());
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            a.exec(&b(), Program::of(&["sleep", "1000"]), Stdio::null())
                .unwrap(),
        );
    }
    assert_eq!(
        a.exec(&b(), Program::of(&["true"]), Stdio::null()).err(),
        Some(Refusal::Exhausted {
            what: PROGRAMS,
            bound: 3
        })
    );
    let mut theirs = c.exec(&b(), Program::of(&["true"]), Stdio::null()).unwrap();
    assert_eq!(end_of(&mut theirs).unwrap(), Exit::Code(0));
    for mut p in held {
        rustix::process::kill_process(
            Pid::from_raw(i32::try_from(p.pid()).unwrap()).unwrap(),
            Signal::KILL,
        )
        .unwrap();
        assert_eq!(end_of(&mut p).unwrap(), Exit::Signal(9));
    }
}

/// The lead's ruling of 2026-11-11 (AC-T7.1.1; original): the streams an exec carries are held only within a program
/// place, so what the node holds of them is bounded by its programs' budget, which its start fits to its descriptor
/// limit, never by its calls in flight. Do: a server of one worker under a programs budget of one; an exec of `sleep`
/// taking the connection's kept place; the host's gate shut and an inspect sent, which holds the worker; then an exec
/// of `true` carrying three pipes' write ends, this test closing its own copies as it sends them, while another
/// process holds a copy of the first's, as a child another test forks holds every descriptor of this one from its fork
/// to its exec (the lead's flake of 2026-11-11, the full hex-node run on 00aad8da: "stream 0"). Expect: the exec
/// refused naming the programs' budget while the inspect is still held, and then no copy of any stream's write end
/// left in the node, this process (its descriptor table, by the pipe's inode), whatever another process holds: an end
/// of file on the read end needs every process's copy closed, a fact no node decides; the inspect answered once the
/// gate opens.
#[test]
fn an_exec_past_the_programs_budget_holds_none_of_its_streams() {
    let lab = Lab::with(
        Limits {
            programs: 1,
            ..limits()
        },
        1,
    );
    let fd = lab.raw();
    send_raw(
        &fd,
        &request(
            1,
            Call::Exec {
                id: b(),
                program: Program::of(&["sleep", "1000"]),
                streams: wire::Streams::default(),
            },
        ),
        &[],
    );
    let pid = match answer_of(&fd) {
        (Header { id: 1, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    lab.host.gate.set(true);
    send_raw(&fd, &request(2, Call::Inspect(b())), &[]);
    let pipes: Vec<(std::io::PipeReader, std::io::PipeWriter)> =
        (0..3).map(|_| std::io::pipe().unwrap()).collect();
    let ends: Vec<BorrowedFd<'_>> = pipes.iter().map(|(_, w)| w.as_fd()).collect();
    // Another process's copy of the first stream's write end, held across the check, made deterministic; ended as
    // the test ends, however it ends.
    struct Outside(std::process::Child);
    impl Drop for Outside {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let outside = Outside(
        std::process::Command::new("sleep")
            .arg("1000")
            .stdout(std::process::Stdio::from(pipes[0].1.try_clone().unwrap()))
            .spawn()
            .unwrap(),
    );
    send_raw(
        &fd,
        &request(
            3,
            Call::Exec {
                id: b(),
                program: Program::of(&["true"]),
                streams: wire::Streams {
                    stdin: true,
                    stdout: true,
                    stderr: true,
                },
            },
        ),
        &ends,
    );
    drop(ends);
    let readers: Vec<std::io::PipeReader> = pipes.into_iter().map(|(r, _)| r).collect();
    match answer_of(&fd) {
        (
            Header { id: 3, .. },
            Answer::Refused(Refusal::Exhausted {
                what: PROGRAMS,
                bound: 1,
            }),
        ) => {}
        other => panic!("refused while the inspect still holds the worker: {other:?}"),
    }
    // The node dropped them before it answered, so its table holds none once the answer is read.
    for (n, r) in readers.iter().enumerate() {
        use std::os::fd::AsRawFd;
        let link = format!("pipe:[{}]", rustix::fs::fstat(r).unwrap().st_ino);
        let reader = r.as_raw_fd().to_string();
        let copies: Vec<String> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let fd = entry.file_name().to_string_lossy().into_owned();
                if fd != reader && std::fs::read_link(entry.path()).ok()? == Path::new(&link) {
                    Some(fd)
                } else {
                    None
                }
            })
            .collect();
        assert!(
            copies.is_empty(),
            "stream {n}: the node holds its write end as {copies:?}"
        );
    }
    lab.host.gate.set(false);
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 2, .. }, Answer::Info(_))
    ));
    drop(outside);
    // Its connection still watches the `sleep`, so the node reaps it as it ends and says how: this test's own wait
    // for it would race the node's reap.
    let pid = Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, Signal::KILL).unwrap();
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 1, .. }, Answer::Exited(Exit::Signal(9)))
    ));
}

/// HEX-SOCK-5, the lead's ruling of 2026-11-06: a client is told what the node keeps for each connection, beside the
/// node's budgets, and is never refused within what it keeps, however much of the budgets other connections hold. Do:
/// a hello; then, under a node-wide budget of 3 calls held at the gate, one connection taking all it can before
/// another's call. Expect: the hello told one call and one program a connection and the node's budgets beside; the
/// first connection refused past two calls, naming the node's budget; the other's call admitted and answered.
#[test]
fn a_client_within_what_it_was_told_is_never_refused() {
    let lab = Lab::new(Limits {
        in_flight: 3,
        ..limits()
    });
    let told = {
        let fd = lab.raw();
        send_raw(&fd, &hello_of_this_wire(9), &[]);
        match answer_of(&fd).1 {
            Answer::Hello(h) => h,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!((told.limits.in_flight, told.limits.programs), (1, 1));
    assert_eq!(
        told.budgets,
        wire::Budgets {
            in_flight: 3,
            programs: limits().programs
        }
    );
    // Each connection holds its one call from its admission, so the hello's is let go first, and both of these are
    // admitted before either asks.
    wait_until(&LET_GO, "the hello's connection to close", || {
        lab.connections() == 0
    });
    let (greedy, other) = (lab.raw(), lab.raw());
    wait_until(&ADMITTED, "both connections to be admitted", || {
        lab.connections() == 2
    });
    lab.host.gate.set(true);
    for id in 1..=3 {
        send_raw(&greedy, &request(id, Call::Inspect(b())), &[]);
    }
    assert_eq!(
        answer_of(&greedy).1,
        Answer::Refused(Refusal::Exhausted {
            what: IN_FLIGHT,
            bound: 3
        })
    );
    send_raw(&other, &request(7, Call::Inspect(b())), &[]);
    lab.host.gate.set(false);
    assert!(matches!(
        answer_of(&other),
        (Header { id: 7, .. }, Answer::Info(_))
    ));
    for _ in 0..2 {
        assert!(matches!(answer_of(&greedy).1, Answer::Info(_)));
    }
}

/// The init budget and the socket's programs-watched bound are a pidfd per box the node may hold, within its hard
/// `RLIMIT_NOFILE` read at run time (getrlimit(2)), never a figure copied from systemd's default (Ada's 2026-11-06
/// constants rule; CLAUDE.md §2). Do: read the node's sizes and its socket's limits on this host. Expect: the
/// programs bound is the least of the boxes' bound and the hard limit (`/proc/sys/fs/nr_open` where unlimited).
#[test]
fn the_programs_bound_is_the_boxes_within_the_hard_descriptor_limit() {
    let hard = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .maximum
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/fs/nr_open")
                .ok()
                .and_then(|t| t.trim().parse().ok())
        })
        .unwrap_or(u64::MAX);
    let sizes = crate::sizes::Sizes::now().unwrap();
    assert_eq!(
        u64::try_from(super::limits(&sizes.platform()).programs).unwrap(),
        hard.min(u64::try_from(hex_runtime::MAX_BOXES).unwrap())
    );
}

/// The waits census's suites (run 5, waits-census-20261109T184054Z): `node_socket`'s contract test failed in 15 of 53
/// loops on calls answered past their patience, a start's among them while a hook held another. Two calls the I/O
/// thread reads in one wake were one worker's: it started a worker only when none was idle, and the one it notified
/// for the first is idle still until it wakes, so the second waited for the first to be done. Do: under calls in flight
/// bounded at 2 on 2 workers, an exec answered, its worker then idle; then, the host's gate shut, an inspect and a
/// netns call sent one after the other on the same connection. Expect: the netns call answered while the inspect is
/// held at the gate; the inspect once it opens.
#[test]
fn two_calls_read_in_one_wake_each_get_a_worker() {
    let lab = Lab::with(
        Limits {
            in_flight: 2,
            ..limits()
        },
        2,
    );
    let fd = lab.raw();
    let (stdin_r, stdin_w) = std::io::pipe().unwrap();
    send_raw(
        &fd,
        &request(
            1,
            Call::Exec {
                id: b(),
                program: Program::of(&["cat"]),
                streams: wire::Streams {
                    stdin: true,
                    ..wire::Streams::default()
                },
            },
        ),
        &[stdin_r.as_fd()],
    );
    drop(stdin_r);
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 1, .. }, Answer::Started { .. })
    ));
    lab.host.gate.set(true);
    send_raw(&fd, &request(2, Call::Inspect(b())), &[]);
    send_raw(&fd, &request(3, Call::Netns), &[]);
    match answer_of(&fd) {
        (Header { id: 3, .. }, Answer::Netns(c)) => assert_eq!(c, counts()),
        other => panic!("{other:?}"),
    }
    lab.host.gate.set(false);
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 2, .. }, Answer::Info(_))
    ));
    // The `cat` ends with its stdin, and its end comes.
    drop(stdin_w);
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 1, .. }, Answer::Exited(Exit::Code(0)))
    ));
}

/// Says hello on a raw connection, as a client's first message.
fn hello_raw(raw: &OwnedFd) {
    send_raw(raw, &hello_of_this_wire(1), &[]);
    assert!(matches!(answer_of(raw).1, Answer::Hello(_)));
}

/// Asks the node to run a shell that reads a line from its stdin, then writes one to its stdout, both streams
/// asked for (exec id 2); its given answer's two ends, stdin's then stdout's.
fn line_after_line(raw: &OwnedFd) -> (OwnedFd, OwnedFd) {
    let call = Call::ExecStreams {
        id: b(),
        program: Program::of(&["/bin/sh", "-c", "read l; echo x"]),
        streams: wire::Streams::default(),
        ask: wire::Streams {
            stdin: true,
            stdout: true,
            stderr: false,
        },
    };
    send_raw(raw, &request(2, call), &[]);
    let (h, given, ends) = answer_with_ends(raw);
    assert!(
        matches!((h.id, &given), (2, Answer::Given { .. })),
        "{given:?}"
    );
    let mut ends = ends.into_iter();
    (ends.next().unwrap(), ends.next().unwrap())
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): the node keeps a given answer's ends until its client says it
/// took them, so none is ever in flight alone: XNU's collector of descriptors in flight flushes a socket whose only
/// reference is a message in flight (719 of 20,000 arrived dead on macOS 26.4). A client takes them and lets its
/// stdout end go at once, saying nothing: the program's stdout pipe still has a reader, the node's, so the program
/// writes its line and ends 0, never by SIGPIPE.
#[test]
fn the_node_keeps_a_given_answers_ends_until_its_client_took_them() {
    let lab = Lab::new(limits());
    let raw = lab.raw();
    hello_raw(&raw);
    let (stdin, stdout) = line_after_line(&raw);
    drop(stdout);
    let mut stdin = std::fs::File::from(stdin);
    stdin.write_all(b"go\n").unwrap();
    drop(stdin);
    let (h, end) = answer_of(&raw);
    assert_eq!(
        (h.id, end),
        (2, Answer::Exited(Exit::Code(0))),
        "the program's stdout had no reader: the node let its ends go before its client took them"
    );
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): once its client says it took a given answer's ends, the node lets
/// its own copies go. Asserted from the node's own state, which no other test's spawn can touch: a spawn of this
/// process copies every descriptor of it, close-on-exec ones too, and keeps them until its exec, so whether the
/// program's write finds a reader is no fact of the node's (the lead's word of 2026-11-09, after this test, then
/// reading the program's SIGPIPE, failed once in a parallel run). Do: an exec asking for stdin and stdout; the word
/// that its ends were taken; a call. Expect: before the word, the node keeps both ends; once the call is answered after
/// it (the node reads a connection's requests in order), it keeps none for the exec, and each descriptor it held is
/// closed, or open on something else.
#[test]
fn the_node_lets_a_given_answers_ends_go_once_its_client_says_it_took_them() {
    let lab = Lab::new(limits());
    let raw = lab.raw();
    hello_raw(&raw);
    let (stdin, stdout) = line_after_line(&raw);
    let held = kept_ends(&lab, 2);
    assert_eq!(
        held.len(),
        2,
        "the node keeps both given ends until the word"
    );
    let taken = wire::request(2, &Request::Taken, wire::MAX_MESSAGE).unwrap();
    send_raw(&raw, &taken, &[]);
    send_raw(&raw, &request(3, Call::Inspect(b())), &[]);
    let (h, info) = answer_of(&raw);
    assert!(matches!((h.id, &info), (3, Answer::Info(_))), "{info:?}");
    assert!(
        kept_ends(&lab, 2).is_empty(),
        "the node still keeps exec 2's ends after the word"
    );
    for (fd, open_on) in &held {
        let now = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok();
        assert_ne!(
            now.as_ref(),
            Some(open_on),
            "the node's descriptor {fd} is still open on its end after the word"
        );
    }
    // The program ends with its stdin.
    drop(stdout);
    let mut stdin = std::fs::File::from(stdin);
    stdin.write_all(b"go\n").unwrap();
    drop(stdin);
    let (h, end) = answer_of(&raw);
    assert!(matches!((h.id, end), (2, Answer::Exited(_))));
}

/// The answer to request `id` on a raw connection, with the descriptors that rode with it, past the programs' ends
/// that come between.
fn answer_to(raw: &OwnedFd, id: u64) -> (Answer, Vec<OwnedFd>) {
    loop {
        let (h, answer, ends) = answer_with_ends(raw);
        if h.id == id && !matches!(answer, Answer::Exited(_)) {
            return (answer, ends);
        }
    }
}

/// An exec of `true` asking for its stdout, as request `id`.
fn true_asking_stdout(id: u64) -> Vec<u8> {
    let call = Call::ExecStreams {
        id: b(),
        program: Program::of(&["/bin/true"]),
        streams: wire::Streams::default(),
        ask: wire::Streams {
            stdout: true,
            ..wire::Streams::default()
        },
    };
    request(id, call)
}

/// The lead's word of 2026-11-09 on `Taken` (HEX-GUEST-41): `Limits::programs` counts given ends not yet taken, so a
/// client that never says it took them holds the node to no more descriptors than its places. Do: under a budget of
/// two programs, two execs asking for stdout, each answered with its end and ended, neither's ends said taken; a third
/// exec; the word that the first's were taken; a fourth. Expect: the third refused, naming the programs' budget; the
/// fourth given its end.
#[test]
fn a_given_answers_ends_not_yet_taken_hold_their_programs_place() {
    let lab = Lab::new(Limits {
        programs: 2,
        ..limits()
    });
    let raw = lab.raw();
    hello_raw(&raw);
    for id in [2, 3] {
        send_raw(&raw, &true_asking_stdout(id), &[]);
        let (given, ends) = answer_to(&raw, id);
        assert!(matches!(given, Answer::Given { .. }), "{given:?}");
        drop(ends);
        // Ended, so only its given ends can hold its place.
        let (h, end) = answer_of(&raw);
        assert_eq!((h.id, end), (id, Answer::Exited(Exit::Code(0))));
    }
    send_raw(&raw, &true_asking_stdout(4), &[]);
    assert_eq!(
        answer_to(&raw, 4).0,
        Answer::Refused(Refusal::Exhausted {
            what: PROGRAMS,
            bound: 2
        }),
        "a program's place was let go with its given ends not yet taken"
    );
    send_raw(
        &raw,
        &wire::request(2, &Request::Taken, wire::MAX_MESSAGE).unwrap(),
        &[],
    );
    send_raw(&raw, &true_asking_stdout(5), &[]);
    let (given, ends) = answer_to(&raw, 5);
    assert!(matches!(given, Answer::Given { .. }), "{given:?}");
    drop(ends);
}

/// Whether `fd` is close-on-exec: whether a spawn of this process, the node's or another's, lets it go at its exec.
fn cloexec(fd: impl AsFd) -> bool {
    rustix::io::fcntl_getfd(fd)
        .unwrap()
        .contains(rustix::io::FdFlags::CLOEXEC)
}

/// The lead's word of 2026-11-09: what the node makes is close-on-exec at its creation, never by a later `fcntl`, so
/// a spawn made while it is open never keeps it past its exec. Do: make the socket a node binding a path probes it
/// with ([`super::unix::clear`]). Expect: close-on-exec on it.
#[test]
fn the_socket_path_probe_is_close_on_exec() {
    let probe = super::unix::probe(SocketType::SEQPACKET).unwrap();
    assert!(
        cloexec(&probe),
        "the probe crosses an exec: a spawn while it is open keeps it"
    );
}

/// The node's own copies of exec `exec`'s given ends on `lab`'s connections, kept until its client says it took them:
/// each descriptor's number, and what it is open on (`/proc/self/fd`'s link: a pipe's or a socket's inode).
fn kept_ends(lab: &Lab, exec: u64) -> Vec<(i32, std::path::PathBuf)> {
    use std::os::fd::AsRawFd;
    let table = lab.server.shared.table.lock().unwrap();
    table
        .conns
        .values()
        .flat_map(|conn| {
            let out = conn.out.lock().unwrap();
            out.given
                .get(&exec)
                .map(|ends| {
                    ends.iter()
                        .map(|end| {
                            let fd = end.as_raw_fd();
                            let open_on =
                                std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
                            (fd, open_on)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

/// The lead's word of 2026-11-09: every descriptor the node makes for an exec's streams and its given ends is
/// close-on-exec from its creation (pipe2's `O_CLOEXEC`, recvmsg's `MSG_CMSG_CLOEXEC`), so no spawn of this process,
/// the node's or another's, keeps one past its exec. Do: an exec carrying the caller's own stdout pipe and asking for
/// stdin and stderr. Expect: close-on-exec on what the host was handed for the exec's streams (the carried pipe, as the
/// node received it) and on what it made for those asked for (each pipe's two ends), on the node's own copies of the
/// given ends, kept until its client took them, and on the given ends as its client received them.
#[test]
fn every_descriptor_the_node_makes_for_an_execs_streams_is_close_on_exec() {
    let lab = Lab::new(limits());
    let raw = lab.raw();
    hello_raw(&raw);
    let (_out, out_w) = std::io::pipe().unwrap();
    let call = Call::ExecStreams {
        id: b(),
        program: Program::of(&["/bin/sh", "-c", "read l"]),
        streams: wire::Streams {
            stdout: true,
            ..wire::Streams::default()
        },
        ask: wire::Streams {
            stdin: true,
            stdout: false,
            stderr: true,
        },
    };
    send_raw(&raw, &request(2, call), &[out_w.as_fd()]);
    drop(out_w);
    let (h, given, ends) = answer_with_ends(&raw);
    assert!(
        matches!((h.id, &given), (2, Answer::Given { .. })),
        "{given:?}"
    );
    let host = lab.host.cloexec.lock().unwrap().clone();
    assert_eq!(
        host.iter().map(|(what, _)| *what).collect::<Vec<_>>(),
        [
            "the program's stdin",
            "the program's stdout",
            "the program's stderr",
            "the caller's stdin",
            "the caller's stderr"
        ]
    );
    for (what, made) in &host {
        assert!(made, "{what} crosses an exec");
    }
    let kept = kept_ends(&lab, 2);
    assert_eq!(
        kept.len(),
        2,
        "the node keeps both given ends until the word"
    );
    let table = lab.server.shared.table.lock().unwrap();
    for conn in table.conns.values() {
        for end in conn.out.lock().unwrap().given.values().flatten() {
            assert!(
                cloexec(end),
                "the node's copy of a given end crosses an exec"
            );
        }
    }
    drop(table);
    for end in &ends {
        assert!(
            cloexec(end),
            "a given end as its client received it crosses an exec"
        );
    }
    // The program ends with its stdin, once its client has taken it and let it go.
    send_raw(
        &raw,
        &wire::request(2, &Request::Taken, wire::MAX_MESSAGE).unwrap(),
        &[],
    );
    drop(ends);
    let (h, end) = answer_of(&raw);
    assert!(matches!((h.id, end), (2, Answer::Exited(_))));
}

/// A live process of this host that isn't this test's child (nor the server's, which runs in this process): `sleep`,
/// let go by a shell that ends at once, so it is orphaned to the session's reaper. Its pid, which only this test ends.
fn unrelated_sleep() -> u32 {
    let out = std::process::Command::new("setsid")
        .args(["sh", "-c", "sleep 600 </dev/null >/dev/null 2>&1 & echo $!"])
        .output()
        .unwrap();
    let pid: u32 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let parent: u32 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_ne!(
        parent,
        std::process::id(),
        "the stand-in is no child of the server's process"
    );
    pid
}

/// GPU run r1119 (AC-T7.4.1; original): a Firecracker box's program came back from its backend as its guest's client
/// hands it, its pid the guest's, and the server watched that pid as one of this host's: it named no process here, so
/// it was waited for as a gone program, for the node's wait for a box to empty, and its end reported `Unknown` while it
/// still ran in its guest. Do: exec a guest's stand-in program that says it is another host's, its pid past the host's
/// `pid_max` (so it names no process here), its end held by the test. Expect the exec refused by type, never watched by
/// that pid, so no end is reported for it, `Unknown` least of all: its host forwards such an exec ([`Host::forward`]).
#[test]
fn original_a_guest_program_whose_pid_names_no_host_process_is_never_waited_for_here() {
    let lab = Lab::new(limits());
    let pid_max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = pid_max + 1;
    let ended = Arc::new((Mutex::new(false), Condvar::new()));
    *lab.host.guest.lock().unwrap() = Some((pid, false, Arc::clone(&ended)));
    let client = lab.client();
    match client.exec(&b(), Program::of(&["the-guests-program"]), Stdio::null()) {
        Err(Refusal::Invalid { what, why }) => {
            assert_eq!(what, "a started program");
            assert!(why.contains(&format!("pid {pid} ")), "{why}");
        }
        Err(other) => panic!("refused by another type: {other:?}"),
        Ok(mut proc) => panic!(
            "watched by its guest's pid {pid} as this host's: told {:?} while it runs in its guest",
            proc.wait_for(EMPTYING.saturating_mul(2))
        ),
    }
    assert_eq!(lab.watched(), 0, "nothing is watched for it");
}

/// The lead's ruling on GPU run r1119 (AC-T7.4.1; original): a guest's pid can name an unrelated process of this host,
/// which the server then watched as the program (and, were its watch refused, sent KILL). Do: exec a stand-in program
/// whose pid is a live `sleep` of this host that isn't the server's child, its backend saying, as a misbehaving one
/// would, that it is this host's; then let the program end. Expect the exec refused by type, as its pid is no child of
/// this node, the `sleep` alive and never watched.
#[test]
fn original_a_program_whose_pid_names_another_process_is_never_watched_or_signalled() {
    let lab = Lab::new(limits());
    let sleep = unrelated_sleep();
    let ended = Arc::new((Mutex::new(false), Condvar::new()));
    *lab.host.guest.lock().unwrap() = Some((sleep, true, Arc::clone(&ended)));
    let client = lab.client();
    let exec = client.exec(&b(), Program::of(&["the-guests-program"]), Stdio::null());
    // The program ends in its guest.
    *ended.0.lock().unwrap() = true;
    ended.1.notify_all();
    let (refused, told) = match exec {
        Ok(mut proc) => (None, Some(proc.wait_for(ANSWERED))),
        Err(why) => (Some(why), None),
    };
    let alive_after = alive(sleep);
    // Only the sleep this test started is ended, by its own pid.
    let _ = rustix::process::kill_process(
        Pid::from_raw(i32::try_from(sleep).unwrap()).unwrap(),
        Signal::KILL,
    );
    match (refused, alive_after) {
        (Some(Refusal::Os { op, why }), true) => {
            assert_eq!(op, "watching a started program");
            assert!(
                why.contains(&format!("pid {sleep} is no child of this node")),
                "{why}"
            );
        }
        (refused, alive_after) => panic!(
            "watched the unrelated process {sleep} as the program: refused {refused:?}, told {told:?}, alive after: \
             {alive_after}"
        ),
    }
    assert_eq!(lab.watched(), 0, "nothing is watched for it");
}
