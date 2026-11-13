//! `hex node` on Windows (native plan step 8; decision 3 of docs/design/runtime-architecture.md, AC-T7.1.1):
//! the node runs as a process of its own, and this test reaches it only through its AF_UNIX socket, as kiln
//! will. Each test starts a node on a folder of its own and kills it when done.
//! - HEX-SOCK-9: create a `process` box, exec a program in it that exits 7, see its end come over the
//!   socket, delete the box;
//! - HEX-SOCK-6, the disconnect rule: a caller that goes away while its exec'd program runs has it killed,
//!   reaped and counted in its box (`abandoned`), and one it released runs on;
//! - HEX-SOCK-3: hostile bytes to the live node (a frame past the bound, a frame of garbage) end that
//!   connection alone, and the node serves the next;
//! - an exec that gives a stream (the caller's own pipe) is refused by name: the Windows client lends no
//!   handle through the socket;
//! - HEX-RT-17 through the socket: an exec's asked streams come to the caller as pipe ends of its own,
//!   duplicated into its process by the node (the stdio decision's pick, dup), no byte copied between;
//! - HEX-SOCK-3 every time: each of many hostile connections brings its refusal and then its end.
#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hex_api::{Ask, Bounds, BoxId, BoxSpec, ClassSpec, Exit, Program, Refusal, Runtime, Stdio};
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Answer, Call, Received, Request, Streams};

/// Longest a test waits for a fact it needs: a bound, so a node that never makes it true fails the test
/// instead of hanging it.
const PATIENCE: Duration = Duration::from_secs(60);

/// A `hex node` of this build on a folder of its own, ended when dropped, and its keeper with it: the keeper
/// outlives a killed node by design (it holds the node's boxes for the next one), so a node this test started
/// leaves no keeper, and no box the keeper held, behind it.
struct Node {
    child: std::process::Child,
    socket: PathBuf,
    /// The keeper's state folder (`state\keeper`), whose `keeper.owner` names it.
    keeping: PathBuf,
}

impl Node {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("node.sock");
        let mut child = Command::new(env!("CARGO_BIN_EXE_hex"))
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(dir.join("state"))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start hex node");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), format!("ready {}", socket.display()));
        Self {
            child,
            socket,
            keeping: dir.join("state").join("keeper"),
        }
    }

    fn client(&self) -> hex_client::Client {
        hex_client::Client::connect(&self.socket).expect("connect to hex node")
    }
}

impl Drop for Node {
    /// Kills the node, then asks the keeper its `keeper.owner` names to stop, as a node going down cleanly
    /// does; the keeper's going ends what it held (kill-on-close).
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let Ok(text) = std::fs::read_to_string(self.keeping.join("keeper.owner")) else {
            return;
        };
        let mut words = text.split_whitespace();
        if let (Some(Ok(pid)), Some(Ok(created))) = (
            words.next().map(str::parse::<u32>),
            words.next().map(str::parse::<u64>),
        ) && let Ok(keeper) =
            hex_runtime::windows::keeper::NodeKeeper::open(pid, created, &self.keeping)
        {
            keeper.stop();
        }
    }
}

fn spec(id: &BoxId) -> BoxSpec {
    BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    }
}

fn act(args: &[&str]) -> Program {
    let mut all = vec![env!("CARGO_BIN_EXE_hex"), "selftest-box"];
    all.extend_from_slice(args);
    Program::of(&all)
}

/// Whether process `pid` is still running, by asking `tasklist` (pure std): its output names the pid only
/// while it runs.
fn alive(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

fn wait_for(what: &str, mut fact: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !fact() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

fn dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("hexnode-")
        .tempdir()
        .unwrap()
}

/// HEX-SOCK-9 (AC-T7.1.1): do create a box, exec a program that exits 7 and delete the box over a live
/// node's socket; expect the program's end, code 7, to come over the socket, and the box gone after.
#[test]
fn a_box_is_made_run_and_deleted_through_the_nodes_socket() {
    let dir = dir();
    let node = Node::start(dir.path());
    let client = node.client();
    let id = BoxId::new("sock-9").unwrap();
    client.create(spec(&id)).unwrap();
    let mut p = client
        .exec(&id, act(&["fail", "7"]), Stdio::null())
        .unwrap();
    let exit = p.wait_for(PATIENCE).unwrap();
    assert_eq!(exit, Some(Exit::Code(7)));
    client.delete(&id).unwrap();
    assert_eq!(client.inspect(&id), Err(Refusal::NoSuchBox(id)));
}

/// A caller of the node's socket in frames of its own (`hex_core::stream`), so a test can drop the
/// connection while programs it started run.
struct Raw {
    socket: hex_afunix::Socket,
    deframer: Deframer,
    next: u64,
}

impl Raw {
    fn connect(node: &Node) -> Self {
        Self {
            socket: hex_afunix::Socket::connect(&node.socket).unwrap(),
            deframer: Deframer::new(wire::MAX_MESSAGE + hex_afunix::ENVELOPE_MAX),
            next: 0,
        }
    }

    /// Sends `call` and waits for its answer.
    fn call(&mut self, call: Call) -> Answer {
        self.next += 1;
        let id = self.next;
        let bytes = wire::request(id, &Request::Call(call), wire::MAX_MESSAGE).unwrap();
        let framed = stream::frame(&bytes, wire::MAX_MESSAGE).unwrap();
        let mut sent = 0;
        while sent < framed.len() {
            sent += self.socket.send(&framed[sent..]).unwrap();
        }
        let mut buf = vec![0u8; stream::MAX_FRAME];
        loop {
            let n = self.socket.recv(&mut buf).unwrap();
            assert!(
                n > 0,
                "the node closed the connection before answering {id}"
            );
            let mut input = &buf[..n];
            while !input.is_empty() {
                let (took, message) = self.deframer.take(input).unwrap();
                input = &input[took..];
                let Some(message) = message else { continue };
                // The node's frames carry the envelope of the ends that ride with a message (none here).
                let (ends, body) = hex_afunix::unenvelope(&message).unwrap();
                assert!(ends.is_empty(), "ends rode with an answer to {id}");
                let received = Received {
                    bytes: body,
                    size: body.len(),
                    descriptors: 0,
                    descriptors_cut: false,
                };
                let (h, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
                if h.id == id {
                    return answer;
                }
            }
        }
    }

    fn exec(&mut self, id: &BoxId, args: &[&str]) -> u32 {
        match self.call(Call::Exec {
            id: id.clone(),
            program: act(args),
            streams: Streams::default(),
        }) {
            Answer::Started { pid } => pid,
            other => panic!("exec in {id}: {other:?}"),
        }
    }
}

/// HEX-SOCK-6 (AC-T7.1.1), the disconnect rule: do exec a waiting program over one connection and a second
/// through a client that releases it, then drop the first connection; expect its program killed and counted
/// in its box as abandoned, and the released one running on.
#[test]
fn a_gone_callers_programs_are_killed_and_counted_and_a_released_one_runs_on() {
    let dir = dir();
    let node = Node::start(dir.path());
    let client = node.client();
    let id = BoxId::new("sock-6").unwrap();
    client.create(spec(&id)).unwrap();
    let released = client.exec(&id, act(&["wait"]), Stdio::null()).unwrap();
    let released_pid = released.pid();
    // Dropped: its client sends a release, and the program runs on unwatched.
    drop(released);
    let mut caller = Raw::connect(&node);
    let watched = caller.exec(&id, &["wait"]);
    assert!(alive(watched), "the watched program isn't running");
    drop(caller);
    wait_for("the gone caller's program to be killed", || !alive(watched));
    wait_for("its box to count it abandoned", || {
        client.inspect(&id).is_ok_and(|b| b.abandoned == 1)
    });
    assert!(alive(released_pid), "the released program was ended");
    client.delete(&id).unwrap();
    wait_for("the delete to end the released program", || {
        !alive(released_pid)
    });
}

/// HEX-SOCK-3 (AC-T7.1.1): do send the live node a frame past its bound on one connection and a frame of
/// garbage on another; expect each connection ended by the node and the node still serving a third.
#[test]
fn hostile_frames_end_their_connection_and_the_node_serves_the_next() {
    let dir = dir();
    let node = Node::start(dir.path());
    for hostile in [
        // A prefix declaring a body past the format's largest message.
        u32::MAX.to_le_bytes().to_vec(),
        // A whole frame of bytes no request is.
        [16u32.to_le_bytes().to_vec(), vec![0xee; 16]].concat(),
    ] {
        let raw = hex_afunix::Socket::connect(&node.socket).unwrap();
        assert_eq!(raw.send(&hostile).unwrap(), hostile.len());
        // The node answers what it can (a refusal) and closes: the read reaches the end, within PATIENCE.
        // Read on a thread of its own, so a node that keeps the connection fails the test by name instead
        // of holding it (gates at 8907b97a, 2026-11-09, held here).
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            while let Ok(n) = raw.recv(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            let _ = ended.send(());
        });
        assert!(
            end.recv_timeout(PATIENCE).is_ok(),
            "the node kept the connection {PATIENCE:?} after a hostile frame"
        );
    }
    let client = node.client();
    let id = BoxId::new("sock-3").unwrap();
    client.create(spec(&id)).unwrap();
    client.delete(&id).unwrap();
}

/// HEX-RT-16 through the socket (native plan step 8): do exec two waiting programs in one box and kill one
/// through its `Proc`; expect its end, read over the socket, to be KILL's (Signal(9)), the other still
/// running, and a second kill accepted.
#[test]
fn a_program_killed_through_the_socket_ends_as_kill_and_its_box_runs_on() {
    let dir = dir();
    let node = Node::start(dir.path());
    let client = node.client();
    let id = BoxId::new("rt-16").unwrap();
    client.create(spec(&id)).unwrap();
    let mut killed = client.exec(&id, act(&["wait"]), Stdio::null()).unwrap();
    let other = client.exec(&id, act(&["wait"]), Stdio::null()).unwrap();
    killed.kill().unwrap();
    let exit = killed.wait_for(PATIENCE).unwrap();
    assert_eq!(exit, Some(Exit::Signal(9)), "a killed program's end");
    assert!(
        alive(other.pid()),
        "the box's other program ended with the kill"
    );
    killed.kill().unwrap();
    client.delete(&id).unwrap();
}

/// Native plan step 8: do exec a program with its stdout a pipe of the caller's own through the node; expect
/// the exec refused by name (the platform, and why: Windows' AF_UNIX passes no handles, so a Windows caller
/// asks for its streams instead), the box untouched and the connection still served.
#[test]
fn an_exec_that_gives_a_stream_is_refused_by_name() {
    let dir = dir();
    let node = Node::start(dir.path());
    let client = node.client();
    let id = BoxId::new("stdio").unwrap();
    client.create(spec(&id)).unwrap();
    let (_read, write) = std::io::pipe().unwrap();
    let stdio = Stdio {
        stdin: None,
        stdout: Some(write),
        stderr: None,
        ..Stdio::null()
    };
    match client.exec(&id, act(&["say", "hi"]), stdio) {
        Err(Refusal::Platform { what, .. }) => {
            assert_eq!(what, "an exec's stdio through the node agent's socket");
        }
        Err(other) => panic!("an exec with a stream was refused, but not by name: {other:?}"),
        Ok(_) => panic!("an exec with a stream wasn't refused"),
    }
    client.delete(&id).unwrap();
}

/// The rounds [`hostile_connections_each_get_their_refusal_then_their_end`] makes: at the ~2% of graceful
/// closes afunix.sys lost to a peer in another process (jupiter, 2026-11-09: 35 of 1,500 rounds, and 2-30
/// of 300 per run across the variants measured), 300 rounds keep at least one with probability above 99.7%
/// (1 - 0.98^300), so the node that loses one fails this test, not one run in twenty.
const HOSTILE_ROUNDS: usize = 300;

/// HEX-SOCK-3, every time (native plan step 8; Ada's ask, 2026-11-09): do send one live node a hostile
/// frame on each of [`HOSTILE_ROUNDS`] connections in turn, half a prefix past the format's largest message
/// and half a frame of garbage, reading each to its end. Expect each connection to bring its refusal (one
/// whole `Answer::Refused`) and then its end, within PATIENCE: the node ends every connection it refuses,
/// its refusal read first, though the platform loses an end-of-stream now and then.
#[test]
fn hostile_connections_each_get_their_refusal_then_their_end() {
    let dir = dir();
    let node = Node::start(dir.path());
    for round in 0..HOSTILE_ROUNDS {
        let hostile = if round % 2 == 0 {
            u32::MAX.to_le_bytes().to_vec()
        } else {
            [16u32.to_le_bytes().to_vec(), vec![0xee; 16]].concat()
        };
        let raw = hex_afunix::Socket::connect(&node.socket).unwrap();
        assert_eq!(raw.send(&hostile).unwrap(), hostile.len());
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            let mut got = Vec::new();
            while let Ok(n) = raw.recv(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(buf.get(..n).unwrap_or_default());
            }
            let _ = ended.send(got);
        });
        let got = end.recv_timeout(PATIENCE).unwrap_or_else(|_| {
            panic!("round {round}: the node's connection didn't end within {PATIENCE:?}")
        });
        // The answer's frame, taken as a client takes it (its prefix, then its body).
        let mut deframer = Deframer::new(wire::MAX_MESSAGE + hex_afunix::ENVELOPE_MAX);
        let mut input = got.as_slice();
        let mut message = None;
        while message.is_none() && !input.is_empty() {
            let (took, done) = deframer.take(input).unwrap();
            input = input.get(took..).unwrap_or_default();
            message = done;
        }
        let message = message.unwrap_or_else(|| {
            panic!(
                "round {round}: the connection ended before a whole answer ({} bytes)",
                got.len()
            )
        });
        assert!(input.is_empty(), "round {round}: bytes past the refusal");
        // The node's frames carry the envelope of the ends that ride with a message (none with a refusal).
        let (ends, body) = hex_afunix::unenvelope(&message).unwrap();
        assert!(ends.is_empty(), "round {round}: ends rode with the refusal");
        let received = Received {
            bytes: body,
            size: body.len(),
            descriptors: 0,
            descriptors_cut: false,
        };
        let (_, answer) = wire::read_answer(&received, wire::MAX_MESSAGE).unwrap();
        assert!(
            matches!(answer, Answer::Refused(_)),
            "round {round}: the node answered {answer:?}"
        );
    }
    let client = node.client();
    let id = BoxId::new("sock-3-every").unwrap();
    client.create(spec(&id)).unwrap();
    client.delete(&id).unwrap();
}

/// HEX-RT-17 through the socket (native plan step 8; the stdio decision's pick, dup): do exec a program that
/// echoes its stdin to its stdout, both asked of the node through its socket; write a line to the stdin end
/// the exec gave, close it, and read the stdout end to its end. Expect the line back whole and the program's
/// exit 0: the ends are pipe ends of this process's own, which the node duplicated into it.
#[test]
fn an_execs_asked_streams_come_through_the_socket_as_the_callers_own_pipe_ends() {
    let dir = dir();
    let node = Node::start(dir.path());
    let client = node.client();
    let id = BoxId::new("sock-17").unwrap();
    client.create(spec(&id)).unwrap();
    let mut p = client
        .exec(
            &id,
            act(&["echo"]),
            Stdio {
                ask: Ask {
                    stdin: true,
                    stdout: true,
                    stderr: false,
                },
                ..Stdio::null()
            },
        )
        .expect("an exec whose stdin and stdout are asked of the node");
    let hex_api::Given {
        stdin,
        stdout,
        stderr,
    } = p.streams().expect("the asked streams");
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        panic!("the asked streams weren't given");
    };
    assert!(stderr.is_none(), "stderr was given unasked");
    let mut stdin = hex_api::OwnedStream::from(stdin);
    stdin.write_all(b"round trip\n").unwrap();
    drop(stdin);
    let mut out = String::new();
    hex_api::OwnedStream::from(stdout)
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(out, "round trip\n");
    assert_eq!(p.wait_for(PATIENCE).unwrap(), Some(Exit::Code(0)));
    client.delete(&id).unwrap();
}
