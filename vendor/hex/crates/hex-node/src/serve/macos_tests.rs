//! The socket on macOS (native plan step 8b; HEX-SOCK-3 to 8 as the Linux tests hold them): a live server
//! in this process over [`MacOs`], whose boxes are the simulator's and whose programs are real processes of
//! this test, reached through `hex-client`'s macOS transport and, for hostile bytes, a raw stream.

use super::*;

use std::io::{Read, Write as _};
use std::os::fd::{AsFd, OwnedFd};

use hex_api::sim::SimRuntime;
use hex_api::{Bounds, BoxInfo, BoxSpec, ClassSpec, Signal as HexSignal, Usage};
use hex_client::Client;
use rustix::net::{AddressFamily, SendFlags, SocketAddrUnix, SocketType};

/// Longest a test waits for a fact it needs: a bound, so a server that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(30);

/// A real child of this test, as a program the host started.
struct ChildProc(Mutex<std::process::Child>, u32);

impl Proc for ChildProc {
    fn pid(&self) -> u32 {
        self.1
    }
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        let status = self
            .0
            .lock()
            .map_err(poisoned)?
            .try_wait()
            .map_err(|e| Refusal::os("waiting", e))?;
        Ok(status.map(|s| match s.code() {
            Some(c) => Exit::Code(c),
            None => Exit::Unknown,
        }))
    }
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(Some(exit));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }
    fn kill(&mut self) -> Result<(), Refusal> {
        self.0
            .lock()
            .map_err(poisoned)?
            .kill()
            .map_err(|e| Refusal::os("kill", e))
    }
    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(hex_api::Given::default())
    }
}

/// Boxes in the simulator; programs real processes of this test, started on the request's stdio.
#[derive(Default)]
struct TestHost {
    sim: SimRuntime,
    abandoned: Mutex<Vec<BoxId>>,
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
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.inspect(id)?;
        let (first, rest) = program.args.split_first().unwrap();
        let mut cmd = std::process::Command::new(first);
        cmd.args(rest);
        let null = std::process::Stdio::null;
        cmd.stdin(stdio.stdin.map_or_else(null, std::process::Stdio::from));
        cmd.stdout(stdio.stdout.map_or_else(null, std::process::Stdio::from));
        cmd.stderr(stdio.stderr.map_or_else(null, std::process::Stdio::from));
        let child = cmd.spawn().map_err(|e| Refusal::os("spawning", e))?;
        let pid = child.id();
        Ok(Box::new(ChildProc(Mutex::new(child), pid)))
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
        Ok(netns::Counts::default())
    }
}

struct Lab {
    server: Server,
    host: Arc<TestHost>,
    _dir: tempfile::TempDir,
}

impl Lab {
    fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("sock-").tempdir().unwrap();
        let host = Arc::new(TestHost::default());
        host.sim
            .create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: b(),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
            })
            .unwrap();
        let server = Server::bind(
            &dir.path().join("node.sock"),
            Arc::clone(&host) as Arc<dyn Host>,
            limits(),
            8,
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

    /// A connection that sends what it is given, framed or not; close-on-exec, so a program this test
    /// starts can't hold it open past the test's drop.
    fn raw(&self) -> OwnedFd {
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).unwrap();
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

fn limits() -> Limits {
    Limits {
        connections: 4,
        message_bytes: 4096,
        in_flight: 4,
        programs: 4,
    }
}

fn b() -> BoxId {
    BoxId::new("b").unwrap()
}

fn request(id: u64, call: Call) -> Vec<u8> {
    wire::request(id, &Request::Call(call), wire::MAX_MESSAGE).unwrap()
}

fn send_framed(fd: &OwnedFd, message: &[u8]) {
    let framed = hex_core::stream::frame(message, wire::MAX_MESSAGE).unwrap();
    rustix::net::send(fd, &framed, SendFlags::empty()).unwrap();
}

/// The next framed message the server sends, waiting at most [`PATIENCE`].
fn answer_of(fd: &OwnedFd) -> (Header, Answer) {
    rustix::net::sockopt::set_socket_timeout(
        fd,
        rustix::net::sockopt::Timeout::Recv,
        Some(PATIENCE),
    )
    .unwrap();
    let mut d = hex_core::stream::Deframer::new(wire::MAX_MESSAGE);
    let mut byte = [0u8; 1];
    loop {
        let n = rustix::net::recv(fd, &mut byte, rustix::net::RecvFlags::empty())
            .unwrap()
            .0;
        assert_eq!(n, 1, "the connection ended before an answer");
        if let (_, Some(m)) = d.take(&byte).unwrap() {
            let got = Received {
                bytes: &m,
                size: m.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            return wire::read_answer(&got, wire::MAX_MESSAGE).unwrap();
        }
    }
}

fn wait_until(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !fact() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(5));
    }
}

fn alive(pid: u32) -> bool {
    hex_runtime::macos::sys::rusage(i32::try_from(pid).unwrap())
        .unwrap()
        .is_some_and(|r| !r.ended)
}

/// HEX-SOCK-3 on macOS: a client's calls cross the framed stream and are answered as in process.
#[test]
fn calls_cross_the_framed_stream() {
    let lab = Lab::new();
    let client = lab.client();
    assert_eq!(client.classes(), lab.host.sim.classes());
    assert_eq!(client.inspect(&b()).unwrap().id, b());
    assert_eq!(client.list().unwrap().len(), 1);
    assert_eq!(
        client.inspect(&BoxId::new("none").unwrap()),
        Err(Refusal::NoSuchBox(BoxId::new("none").unwrap()))
    );
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule), over macOS's framed stream: a client of
/// another wire is refused at its hello, and nothing it sends is read as this wire's. Do: on a raw stream, a framed
/// hello as a client of another wire sends it (its identity and its build, in the hello's head every build lays out
/// alike) and a framed delete of box b of that wire; then inspect b through `hex-client`, of this wire. Expect: the
/// first answered with the node's own hello, of this wire and this build, which a client of another wire refuses
/// naming both (hex-client's own tests), then the stream's end, nothing more sent, and b kept; the inspect answered.
#[test]
fn a_client_of_another_wire_is_refused_at_its_hello() {
    let lab = Lab::new();
    let theirs = !wire::IDENTITY;
    let of_wire = |mut bytes: Vec<u8>| {
        bytes[4..12].copy_from_slice(&theirs.to_le_bytes());
        bytes
    };
    let fd = lab.raw();
    let their_hello = Request::Hello {
        build: "hex 9.9.9 (another wire)".to_owned(),
    };
    send_framed(
        &fd,
        &of_wire(wire::request(1, &their_hello, wire::MAX_MESSAGE).unwrap()),
    );
    send_framed(&fd, &of_wire(request(2, Call::Delete(b()))));
    match answer_of(&fd) {
        (Header { id: 1, .. }, Answer::Hello(h)) => assert_eq!(h.build, wire::build()),
        other => panic!("the node's own hello, not {other:?}"),
    }
    let mut rest = [0u8; 64];
    assert_eq!(
        rustix::net::recv(&fd, &mut rest, rustix::net::RecvFlags::empty())
            .unwrap()
            .0,
        0,
        "the node sent more than its hello"
    );
    wait_until("the refused connection to close", || lab.connections() == 0);
    assert_eq!(lab.host.sim.inspect(&b()).unwrap().id, b());
    assert_eq!(lab.client().inspect(&b()).unwrap().id, b());
}

/// HEX-SOCK-7 on macOS: an exec's stdio crosses as descriptors at its message's first byte, and the
/// program's end follows its start.
#[test]
fn stdio_crosses_as_descriptors_and_the_end_follows() {
    let lab = Lab::new();
    let client = lab.client();
    let (mut r, w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &b(),
            Program::of(&["/bin/echo", "framed"]),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut out = String::new();
    r.read_to_string(&mut out).unwrap();
    assert_eq!(out, "framed\n");
    assert_eq!(proc.wait_for(PATIENCE).unwrap(), Some(Exit::Code(0)));
    wait_until("the watch to be let go", || lab.watched() == 0);
}

/// HEX-SOCK-6 on macOS, the disconnect rule: a program whose client goes is killed through its watch,
/// reaped and counted in its box.
#[test]
fn a_program_whose_client_goes_is_killed_reaped_and_counted() {
    let lab = Lab::new();
    let fd = lab.raw();
    send_framed(
        &fd,
        &request(
            1,
            Call::Exec {
                id: b(),
                program: Program::of(&["/bin/sleep", "1000"]),
                streams: Streams::default(),
            },
        ),
    );
    let pid = match answer_of(&fd) {
        (Header { id: 1, .. }, Answer::Started { pid }) => pid,
        other => panic!("{other:?}"),
    };
    assert!(alive(pid));
    drop(fd);
    wait_until("the connection to close", || lab.connections() == 0);
    wait_until("the program to be ended", || !alive(pid));
    wait_until("the abandoned program to be counted in its box", || {
        lab.host.abandoned.lock().unwrap().as_slice() == [b()]
    });
    wait_until("the watch to go", || lab.watched() == 0);
}

/// HEX-SOCK-3 on macOS, hostile frames: one past the message bound is refused naming its size and its
/// connection closed (a stream can't be read past it); another connection is served after.
#[test]
fn an_oversized_frame_is_refused_by_name_and_closes_its_connection() {
    let lab = Lab::new();
    let fd = lab.raw();
    let size = u32::try_from(wire::MAX_MESSAGE + 1).unwrap();
    let mut oversized = size.to_be_bytes().to_vec();
    oversized.extend_from_slice(&request(9, Call::Inspect(b())));
    rustix::net::send(&fd, &oversized, SendFlags::empty()).unwrap();
    match answer_of(&fd).1 {
        Answer::Refused(r) => assert!(r.to_string().contains(&size.to_string()), "{r}"),
        other => panic!("{other:?}"),
    }
    wait_until("the connection to close", || lab.connections() == 0);
    assert_eq!(lab.client().inspect(&b()).unwrap().id, b());
}

/// HEX-SOCK-3 on macOS: descriptors sent in the middle of a frame are misplaced, and that message is
/// refused by type; the connection is served after.
#[test]
fn descriptors_in_the_middle_of_a_frame_are_refused() {
    let lab = Lab::new();
    let fd = lab.raw();
    let framed =
        hex_core::stream::frame(&request(5, Call::Inspect(b())), wire::MAX_MESSAGE).unwrap();
    let (head, tail) = framed.split_at(3);
    rustix::net::send(&fd, head, SendFlags::empty()).unwrap();
    let (pr, _pw) = std::io::pipe().unwrap();
    let borrowed = [pr.as_fd()];
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = rustix::net::SendAncillaryBuffer::new(&mut space);
    assert!(control.push(rustix::net::SendAncillaryMessage::ScmRights(&borrowed)));
    rustix::net::sendmsg(
        &fd,
        &[std::io::IoSlice::new(tail)],
        &mut control,
        SendFlags::empty(),
    )
    .unwrap();
    match answer_of(&fd) {
        (Header { id: 5, .. }, Answer::Refused(Refusal::Invalid { .. })) => {}
        other => panic!("{other:?}"),
    }
    send_framed(&fd, &request(6, Call::Inspect(b())));
    assert!(matches!(
        answer_of(&fd),
        (Header { id: 6, .. }, Answer::Info(_))
    ));
}

/// The socket at a path on macOS: one an ended agent left is replaced, one a live agent serves is refused,
/// and a file that isn't a socket is refused, never removed.
#[test]
fn a_socket_path_is_taken_only_from_an_ended_agent() {
    let lab = Lab::new();
    let path = lab.server.path().to_owned();
    let host = Arc::clone(&lab.host) as Arc<dyn Host>;
    assert!(matches!(
        Server::bind(&path, Arc::clone(&host), limits(), 1),
        Err(Refusal::Invalid { .. })
    ));
    drop(lab.server);
    // An ended agent's socket: left behind, as a crash leaves it.
    let left = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    rustix::net::bind(&left, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    drop(left);
    let again = Server::bind(&path, Arc::clone(&host), limits(), 1).unwrap();
    drop(again);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(b"not a socket")
        .unwrap();
    assert!(matches!(
        Server::bind(&path, host, limits(), 1),
        Err(Refusal::Invalid { .. })
    ));
    assert!(path.exists());
}
