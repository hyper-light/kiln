//! The framed transport's tests over the one core: a stand-in agent's `SOCK_STREAM` socket in this process, so a
//! frame cut across reads, frames that share a read, two callers' answers in one write and descriptors at a message's
//! first byte meet a real stream.

use super::*;
use hex_api::{BoxId, Class, Exit, Program, Runtime, Stdio};
use hex_core::wire::{Answer, Call, Header, Limits, Request, Streams};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};

/// A stand-in agent's listening socket, in a directory of its own.
fn agent_socket() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
    let cache =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/client");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("agent-")
        .tempdir_in(&cache)
        .unwrap();
    let path = dir.path().join("node.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (dir, path, listener)
}

/// The stand-in agent's reading side: what of a frame came, and the whole requests a read brought beyond the one
/// asked for.
struct Requests {
    deframer: Deframer,
    pending: std::collections::VecDeque<Vec<u8>>,
}

impl Requests {
    fn new() -> Self {
        Self {
            deframer: Deframer::new(wire::MAX_MESSAGE),
            pending: std::collections::VecDeque::new(),
        }
    }
}

/// The next framed request on `conn`, read whole; requests that came with it in one read are kept for the next.
fn next_request(conn: &mut UnixStream, requests: &mut Requests) -> (Header, Request) {
    let mut buf = [0u8; 4096];
    while requests.pending.is_empty() {
        let n = conn.read(&mut buf).unwrap();
        assert!(n > 0, "the client closed the connection");
        let mut input = &buf[..n];
        while !input.is_empty() {
            let (took, whole) = requests.deframer.take(input).unwrap();
            input = &input[took..];
            if let Some(message) = whole {
                requests.pending.push_back(message);
            }
        }
    }
    let message = requests.pending.pop_front().unwrap();
    let msg = Received {
        bytes: &message,
        size: message.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_request(&msg, wire::MAX_MESSAGE).unwrap()
}

/// `answer` to request `h`, framed.
fn framed(h: Header, answer: &Answer) -> Vec<u8> {
    let bytes = wire::answer(h.id, answer, wire::MAX_MESSAGE).unwrap();
    stream::frame(&bytes, wire::MAX_MESSAGE).unwrap()
}

/// A stand-in agent's hello, of this wire and this build: a process class, room for `in_flight` calls at once.
fn agents_hello(in_flight: usize) -> wire::Hello {
    wire::Hello {
        build: wire::build(),
        classes: vec![Class::Process],
        limits: Limits {
            connections: 1,
            message_bytes: wire::MAX_MESSAGE,
            in_flight,
            programs: 1,
        },
        budgets: wire::Budgets {
            in_flight,
            programs: 1,
        },
    }
}

/// Answers a client's hello, which carries its build, with [`agents_hello`].
fn hello(conn: &mut UnixStream, requests: &mut Requests, in_flight: usize) {
    let (h, request) = next_request(conn, requests);
    assert_eq!(
        request,
        Request::Hello {
            build: wire::build()
        }
    );
    conn.write_all(&framed(h, &Answer::Hello(agents_hello(in_flight))))
        .unwrap();
}

/// Whether the client's reader holds part of a frame: its read state, between the turn's reads.
fn holds_part_of_a_frame(client: &Client) -> bool {
    client
        .shared()
        .with_reading(|reading| !reading.deframer.at_boundary())
}

/// A frame cut across reads is read whole: the turn's holder keeps what came of it for the next read. Do: inspect a box
/// whose refusal the stand-in agent writes in two parts, the second only once the client holds the first. Expect: the
/// inspect refused naming the box, the frame read whole.
#[test]
fn a_frame_cut_across_reads_is_answered_whole() {
    let (_dir, path, listener) = agent_socket();
    let (cut, first_part_sent) = std::sync::mpsc::channel();
    let (go, send_the_rest) = std::sync::mpsc::channel::<()>();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut requests = Requests::new();
        hello(&mut conn, &mut requests, 1);
        let (h, request) = next_request(&mut conn, &mut requests);
        let Request::Call(Call::Inspect(id)) = request else {
            panic!("an inspect, not {request:?}")
        };
        let bytes = framed(h, &Answer::Refused(Refusal::NoSuchBox(id)));
        let half = bytes.len() / 2;
        conn.write_all(&bytes[..half]).unwrap();
        cut.send(()).unwrap();
        send_the_rest.recv().unwrap();
        conn.write_all(&bytes[half..]).unwrap();
        conn
    });
    let client = Client::connect(&path).unwrap();
    let b = BoxId::new("b").unwrap();
    std::thread::scope(|s| {
        let inspect = s.spawn(|| client.inspect(&b));
        first_part_sent.recv().unwrap();
        while !holds_part_of_a_frame(&client) {
            std::thread::yield_now();
        }
        go.send(()).unwrap();
        assert_eq!(inspect.join().unwrap(), Err(Refusal::NoSuchBox(b.clone())));
    });
    assert!(!holds_part_of_a_frame(&client), "the frame was read whole");
    drop(agent.join().unwrap());
}

/// Two callers' answers in one write, the second's frame first and the first's cut across two writes: each reaches
/// the call it answers, whichever thread read it. Do: two threads inspect a box each; the stand-in agent reads both
/// requests, then writes the second's answer and the first's first half in one write, and the rest after. Expect:
/// each refused naming its own box.
#[test]
fn two_callers_answers_sharing_reads_each_reach_their_own() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut requests = Requests::new();
        hello(&mut conn, &mut requests, 2);
        let asked: Vec<(Header, Request)> = (0..2)
            .map(|_| next_request(&mut conn, &mut requests))
            .collect();
        let answers: Vec<Vec<u8>> = asked
            .iter()
            .map(|(h, request)| {
                let Request::Call(Call::Inspect(id)) = request else {
                    panic!("an inspect, not {request:?}")
                };
                framed(*h, &Answer::Refused(Refusal::NoSuchBox(id.clone())))
            })
            .collect();
        let (first, second) = (&answers[0], &answers[1]);
        let half = first.len() / 2;
        let mut one_write = second.clone();
        one_write.extend_from_slice(&first[..half]);
        conn.write_all(&one_write).unwrap();
        conn.write_all(&first[half..]).unwrap();
        conn
    });
    let client = Client::connect(&path).unwrap();
    std::thread::scope(|s| {
        for name in ["a", "b"] {
            let client = &client;
            s.spawn(move || {
                let id = BoxId::new(name).unwrap();
                assert_eq!(client.inspect(&id), Err(Refusal::NoSuchBox(id.clone())));
            });
        }
    });
    drop(agent.join().unwrap());
}

/// An answer's descriptors ride at its frame's first byte: a given answer's end reaches its exec, which hands it to the
/// caller that asked for the stream. Do: exec asking for stdout; the stand-in agent answers it given, with one end of a
/// socket pair at the frame's first byte and the frame cut across two writes, keeps its copy until the client says it
/// took it (`Request::Taken`), as the node agent does (an end in flight alone may arrive dead on macOS, HEX-GUEST-41),
/// then writes into the other end and closes it. Expect: the word, by the exec's id; the exec's stdout end close-on-exec
/// as it came, reading what the agent wrote, to its end, and the program's end as sent.
#[test]
fn a_given_answers_descriptor_at_its_first_byte_reaches_its_exec() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut requests = Requests::new();
        hello(&mut conn, &mut requests, 1);
        let (h, request) = next_request(&mut conn, &mut requests);
        let Request::Call(Call::ExecStreams { streams, ask, .. }) = request else {
            panic!("an exec asking for streams, not {request:?}")
        };
        assert_eq!(streams, Streams::default());
        assert!(ask.stdout && !ask.stdin && !ask.stderr);
        let (mut here, end) = UnixStream::pair().unwrap();
        let given = framed(
            h,
            &Answer::Given {
                pid: 42,
                given: Streams {
                    stdout: true,
                    ..Streams::default()
                },
            },
        );
        let mut space =
            [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        let fds = [end.as_fd()];
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        let half = given.len() / 2;
        rustix::net::sendmsg(
            &conn,
            &[IoSlice::new(&given[..half])],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        conn.write_all(&given[half..]).unwrap();
        let (t, taken) = next_request(&mut conn, &mut requests);
        assert_eq!((t.id, taken), (h.id, Request::Taken));
        drop(end);
        here.write_all(b"from the agent\n").unwrap();
        drop(here);
        conn.write_all(&framed(h, &Answer::Exited(Exit::Code(0))))
            .unwrap();
        conn
    });
    let client = Client::connect(&path).unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("g").unwrap(),
            Program::of(&["true"]),
            Stdio {
                ask: hex_api::Ask {
                    stdout: true,
                    ..hex_api::Ask::default()
                },
                ..Stdio::null()
            },
        )
        .unwrap();
    let given = proc.streams().unwrap();
    let stdout = given.stdout.expect("the stdout end");
    // macOS's recvmsg has no MSG_CMSG_CLOEXEC: the end that came is close-on-exec all the same, so no spawn of the
    // caller's inherits it (CVE-2024-21626's class).
    assert!(
        rustix::io::fcntl_getfd(&stdout)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC),
        "a given end crosses an exec"
    );
    let mut out = String::new();
    std::fs::File::from(stdout)
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(out, "from the agent\n");
    assert_eq!(
        proc.wait_for(std::time::Duration::from_secs(30)).unwrap(),
        Some(Exit::Code(0))
    );
    drop(proc);
    drop(client);
    drop(agent.join().unwrap());
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule), over the framed stream: a client
/// refuses an agent of another wire at the hello, typed, naming both wires' identities and both builds, and sends it
/// nothing more; an agent of its own wire it serves. Do: connect to a stand-in agent that answers the hello, framed,
/// as an agent of another wire would (its own identity and build, in the hello's head every build lays out alike);
/// then connect again, answered in this wire, and inspect a box. Expect: the first connect refused as
/// `Invalid { what: "wire" }` naming both, its connection ended with nothing more sent; the second greeted and its
/// inspect answered.
#[test]
fn an_agent_of_another_wire_is_refused_at_the_hello() {
    let (_dir, path, listener) = agent_socket();
    let theirs = !wire::IDENTITY;
    let their_build = "hex 9.9.9 (another wire)";
    let agent = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let mut requests = Requests::new();
        let (h, request) = next_request(&mut conn, &mut requests);
        assert!(matches!(request, Request::Hello { .. }), "{request:?}");
        let mut bytes = wire::answer(
            h.id,
            &Answer::Hello(wire::Hello {
                build: their_build.to_owned(),
                ..agents_hello(1)
            }),
            wire::MAX_MESSAGE,
        )
        .unwrap();
        bytes[4..12].copy_from_slice(&theirs.to_le_bytes());
        conn.write_all(&stream::frame(&bytes, wire::MAX_MESSAGE).unwrap())
            .unwrap();
        // Nothing more comes: the client's end of the connection, read as its close.
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).unwrap();
        let (mut conn, _) = listener.accept().unwrap();
        let mut requests = Requests::new();
        hello(&mut conn, &mut requests, 1);
        let (h, request) = next_request(&mut conn, &mut requests);
        let Request::Call(Call::Inspect(id)) = request else {
            panic!("an inspect, not {request:?}")
        };
        conn.write_all(&framed(h, &Answer::Refused(Refusal::NoSuchBox(id))))
            .unwrap();
        (rest, conn)
    });
    let refused = Client::connect(&path).err();
    let Some(Refusal::Invalid { what: "wire", why }) = &refused else {
        panic!("refused for its wire, not {refused:?}")
    };
    for named in [
        format!("{theirs:016x}"),
        format!("{:016x}", wire::IDENTITY),
        their_build.to_owned(),
        wire::build(),
    ] {
        assert!(why.contains(&named), "{why:?} names {named:?}");
    }
    let client = Client::connect(&path).unwrap();
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b.clone())));
    let (rest, conn) = agent.join().unwrap();
    assert!(
        rest.is_empty(),
        "the refused client sent {} bytes after its hello",
        rest.len()
    );
    drop(conn);
}
