//! The Linux transport's tests over the one core: a stand-in agent's `SOCK_SEQPACKET` socket in this process, so
//! the reader's turn, its hand-off, room waited for and the given answer's descriptors meet a real socket.

use super::*;
use hex_api::{Ask, BoxId, Class, Exit, Program, Runtime, Stdio};
use hex_core::wire::{Answer, Call, Header, Limits, Request, Streams};
use rustix::event::{PollFd, PollFlags, Timespec};
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};

/// A stand-in agent's listening socket, in a directory of its own.
fn agent_socket() -> (tempfile::TempDir, std::path::PathBuf, OwnedFd) {
    let cache =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/client");
    std::fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("agent-")
        .tempdir_in(&cache)
        .unwrap();
    let path = dir.path().join("node.sock");
    let listener = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&listener, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    rustix::net::listen(&listener, 1).unwrap();
    (dir, path, listener)
}

/// The next request on `conn`, waiting at most 30 s for it.
fn next_request(conn: &OwnedFd) -> (Header, Request) {
    let mut ready = [PollFd::new(conn, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: 30,
        tv_nsec: 0,
    };
    assert_eq!(rustix::event::poll(&mut ready, Some(&ts)).unwrap(), 1);
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let (n, _) = rustix::net::recv(conn, &mut buf, RecvFlags::empty()).unwrap();
    let msg = Received {
        bytes: &buf[..n],
        size: n,
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_request(&msg, wire::MAX_MESSAGE).unwrap()
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
fn hello(conn: &OwnedFd, in_flight: usize) {
    let (h, request) = next_request(conn);
    assert_eq!(
        request,
        Request::Hello {
            build: wire::build()
        }
    );
    answer(conn, h.id, &Answer::Hello(agents_hello(in_flight)));
}

/// Sends a stand-in agent's `answer` to request `id`.
fn answer(conn: &OwnedFd, id: u64, answer: &Answer) {
    let bytes = wire::answer(id, answer, wire::MAX_MESSAGE).unwrap();
    rustix::net::send(conn, &bytes, SendFlags::NOSIGNAL).unwrap();
}

/// The request on `conn` that is next to be read, left there unread, waiting at most 30 s for it.
fn peek_request(conn: &OwnedFd) -> (Header, Request) {
    let mut ready = [PollFd::new(conn, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: 30,
        tv_nsec: 0,
    };
    assert_eq!(rustix::event::poll(&mut ready, Some(&ts)).unwrap(), 1);
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let (n, _) = rustix::net::recv(conn, &mut buf, RecvFlags::PEEK).unwrap();
    let msg = Received {
        bytes: &buf[..n],
        size: n,
        descriptors: 0,
        descriptors_cut: false,
    };
    wire::read_request(&msg, wire::MAX_MESSAGE).unwrap()
}

/// A program whose exec's request fills `client`'s socket's send buffer, which it shrinks to fit: once the agent
/// holds that request unread, every send finds no room until the agent reads it. A unix socket's send finds room
/// while what its peer holds unread of it, each message counted at its buffer's true size, is under its send
/// buffer (net/unix/af_unix.c, `unix_dgram_sendmsg` through net/core/sock.c, `sock_alloc_send_pskb`), and a message
/// may be as large as that buffer less 32 bytes; the kernel takes twice the size asked of it (socket(7), SO_SNDBUF).
fn filling(client: &Client) -> Program {
    let fd = &client.shared().transport.fd;
    let program = Program::of(&["/bin/true", &"x".repeat(wire::MAX_MESSAGE / 8)]);
    let call = Request::Call(Call::Exec {
        id: BoxId::new("b").unwrap(),
        program: program.clone(),
        streams: Streams::default(),
    });
    let len = wire::request(u64::MAX, &call, wire::MAX_MESSAGE)
        .unwrap()
        .len();
    rustix::net::sockopt::set_socket_send_buffer_size(fd, (len + 32).div_ceil(2)).unwrap();
    let room = rustix::net::sockopt::socket_send_buffer_size(fd).unwrap();
    assert!(
        (len + 32..len + 34).contains(&room),
        "a send buffer of {room} bytes for a request of {len}"
    );
    program
}

/// Answers a stand-in agent sends while it reads no request, as the node agent does while its answers wait for
/// room, each to a request the client never made: until what it sent is more than its send buffer holds, and once
/// more, which waits until the client reads, the send that crosses the buffer being let through (`filling`).
fn more_than_room(conn: &OwnedFd) {
    let room = rustix::net::sockopt::socket_send_buffer_size(conn).unwrap();
    let filler = Answer::Refused(Refusal::Broken("x".repeat(room / 8)));
    let bytes = wire::answer(u64::MAX, &filler, wire::MAX_MESSAGE).unwrap();
    let mut sent = 0;
    loop {
        rustix::net::send(conn, &bytes, SendFlags::NOSIGNAL).unwrap();
        if sent > room {
            break;
        }
        sent += bytes.len();
    }
}

/// The socket's per-call cost (goal 16; the lead, 2026-11-09): no thread reads the agent's socket but those waiting
/// on it, so an answer wakes the call it answers and no thread between hands it on. Do: connect to a stand-in agent
/// and delete a box. Expect: the call answered, and no thread of this process the client's reader.
#[test]
fn a_calls_answer_is_read_by_the_thread_waiting_for_it() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        let (h, request) = next_request(&conn);
        let Request::Call(Call::Delete(id)) = request else {
            panic!("a delete, not {request:?}")
        };
        answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
    });
    let client = Client::connect(&path).unwrap();
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.delete(&b), Err(Refusal::NoSuchBox(b.clone())));
    let readers: Vec<String> = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.trim() == "hex-client")
        .collect();
    assert_eq!(readers, Vec::<String>::new(), "the client's reading thread");
    agent.join().unwrap();
}

/// Many calls at once, answered last first: each gets its own answer, whichever thread read it, and the turn to read
/// passes from each call answered to one still waiting. Do: eight threads each delete a box of its own; the stand-in
/// agent reads all eight requests, then answers them in the reverse of their order. Expect: each refused naming its
/// own box.
#[test]
fn many_calls_answered_out_of_order_each_get_their_own() {
    let (_dir, path, listener) = agent_socket();
    let n = 8;
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, n);
        let asked: Vec<(Header, Request)> = (0..n).map(|_| next_request(&conn)).collect();
        for (h, request) in asked.into_iter().rev() {
            let Request::Call(Call::Delete(id)) = request else {
                panic!("a delete, not {request:?}")
            };
            answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        }
    });
    let client = Client::connect(&path).unwrap();
    std::thread::scope(|s| {
        for i in 0..n {
            let client = &client;
            s.spawn(move || {
                let b = BoxId::new(&format!("b{i}")).unwrap();
                assert_eq!(client.delete(&b), Err(Refusal::NoSuchBox(b.clone())));
            });
        }
    });
    agent.join().unwrap();
}

/// A call that runs out while it holds the reader's turn hands it to a call still waiting, whose answer then comes.
/// Do: an inspect sent first, taking the turn; then a delete, sent while the stand-in agent holds the inspect; the
/// agent answers the delete alone, once the inspect has run out. Expect: the inspect refused for no answer in its
/// patience, and the delete answered.
#[test]
fn a_call_that_runs_out_hands_the_turn_to_one_still_waiting() {
    let (_dir, path, listener) = agent_socket();
    let (held, inspect_held) = std::sync::mpsc::channel();
    let (go, answer_delete) = std::sync::mpsc::channel::<()>();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 2);
        let (_, inspect) = next_request(&conn);
        assert!(
            matches!(inspect, Request::Call(Call::Inspect(_))),
            "{inspect:?}"
        );
        held.send(()).unwrap();
        let (h, delete) = next_request(&conn);
        let Request::Call(Call::Delete(id)) = delete else {
            panic!("a delete, not {delete:?}")
        };
        answer_delete.recv().unwrap();
        answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        conn
    });
    let client = Client::connect(&path).unwrap();
    let a = BoxId::new("a").unwrap();
    let b = BoxId::new("b").unwrap();
    std::thread::scope(|s| {
        let inspect = s.spawn(|| client.inspect(&a));
        inspect_held.recv().unwrap();
        let delete = s.spawn(|| client.delete(&b));
        assert!(
            matches!(
                inspect.join().unwrap(),
                Err(Refusal::Os {
                    op: "waiting for the node agent's answer",
                    ..
                })
            ),
            "the inspect ran out"
        );
        go.send(()).unwrap();
        assert_eq!(delete.join().unwrap(), Err(Refusal::NoSuchBox(b.clone())));
    });
    drop(agent.join().unwrap());
}

/// A send that waits for room reads what the agent sends once the reader's turn is its: the node agent reads no
/// request while its answers wait for room in the client's socket, so a send waiting with no one reading would
/// wait for good. Do: a list, its request read, holds the turn; an exec's request as large as the client's send
/// buffer is left unread, and answered by its id; a delete is made, which finds no room; the agent answers the
/// list, then sends more than its own send buffer holds before it reads the exec's request and the delete's.
/// Expect: the list answered, and the delete answered, not refused for want of room.
#[test]
fn a_send_waiting_for_room_reads_once_the_turn_is_its() {
    let (_dir, path, listener) = agent_socket();
    let (list_read, list_was_read) = std::sync::mpsc::channel();
    let (go, answer_list) = std::sync::mpsc::channel::<()>();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 3);
        let (list, request) = next_request(&conn);
        assert_eq!(request, Request::Call(Call::List));
        list_read.send(()).unwrap();
        let (exec, request) = peek_request(&conn);
        let Request::Call(Call::Exec { id, .. }) = request else {
            panic!("an exec, not {request:?}")
        };
        answer(&conn, exec.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        answer_list.recv().unwrap();
        answer(
            &conn,
            list.id,
            &Answer::Boxes {
                infos: Vec::new(),
                more: false,
            },
        );
        more_than_room(&conn);
        let (_, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        let (delete, request) = next_request(&conn);
        let Request::Call(Call::Delete(id)) = request else {
            panic!("a delete, not {request:?}")
        };
        answer(&conn, delete.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        conn
    });
    let client = Client::connect(&path).unwrap();
    let program = filling(&client);
    let b = BoxId::new("b").unwrap();
    let d = BoxId::new("d").unwrap();
    std::thread::scope(|s| {
        let list = s.spawn(|| client.list());
        list_was_read.recv().unwrap();
        assert_eq!(
            client.exec(&b, program, Stdio::null()).err(),
            Some(Refusal::NoSuchBox(b.clone()))
        );
        let delete = s.spawn(|| client.delete(&d));
        // The delete has its place among the calls, beside the list's.
        while client.shared().calls_waiting() < 2 {
            std::thread::yield_now();
        }
        go.send(()).unwrap();
        assert_eq!(list.join().unwrap(), Ok(Vec::new()));
        assert_eq!(delete.join().unwrap(), Err(Refusal::NoSuchBox(d.clone())));
    });
    drop(agent.join().unwrap());
}

/// A call that runs out waiting for room hands on the reader's turn it holds, so the calls after it are still
/// read. Do: as for the send waiting for room, but an inspect finds no room, and the agent sends nothing more and
/// reads nothing until it has run out; then the agent reads the exec's request, and a delete is made. Expect: the
/// inspect refused for want of room, and the delete answered.
#[test]
fn a_call_that_runs_out_waiting_for_room_hands_on_its_turn() {
    let (_dir, path, listener) = agent_socket();
    let (list_read, list_was_read) = std::sync::mpsc::channel();
    let (go, answer_list) = std::sync::mpsc::channel::<()>();
    let (read_on, inspect_ran_out) = std::sync::mpsc::channel::<()>();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 3);
        let (list, request) = next_request(&conn);
        assert_eq!(request, Request::Call(Call::List));
        list_read.send(()).unwrap();
        let (exec, request) = peek_request(&conn);
        let Request::Call(Call::Exec { id, .. }) = request else {
            panic!("an exec, not {request:?}")
        };
        answer(&conn, exec.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        answer_list.recv().unwrap();
        answer(
            &conn,
            list.id,
            &Answer::Boxes {
                infos: Vec::new(),
                more: false,
            },
        );
        inspect_ran_out.recv().unwrap();
        let (_, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        let (delete, request) = next_request(&conn);
        let Request::Call(Call::Delete(id)) = request else {
            panic!("a delete, not {request:?}")
        };
        answer(&conn, delete.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        conn
    });
    let client = Client::connect(&path).unwrap();
    let program = filling(&client);
    let b = BoxId::new("b").unwrap();
    let i = BoxId::new("i").unwrap();
    let d = BoxId::new("d").unwrap();
    std::thread::scope(|s| {
        let list = s.spawn(|| client.list());
        list_was_read.recv().unwrap();
        assert_eq!(
            client.exec(&b, program, Stdio::null()).err(),
            Some(Refusal::NoSuchBox(b.clone()))
        );
        let inspect = s.spawn(|| client.inspect(&i));
        // The inspect has its place among the calls, beside the list's.
        while client.shared().calls_waiting() < 2 {
            std::thread::yield_now();
        }
        go.send(()).unwrap();
        assert_eq!(list.join().unwrap(), Ok(Vec::new()));
        assert!(
            matches!(
                inspect.join().unwrap(),
                Err(Refusal::Os {
                    op: "sending to the node agent",
                    ..
                })
            ),
            "the inspect ran out for want of room"
        );
    });
    read_on.send(()).unwrap();
    assert_eq!(client.delete(&d), Err(Refusal::NoSuchBox(d.clone())));
    drop(agent.join().unwrap());
}

/// A program's end comes to its `Proc` waiting for it with no call in flight: the wait reads it itself, there being
/// no other reader. Do: exec through a stand-in agent that answers it started and then sends its end. Expect: the
/// `Proc`'s wait returns that exit.
#[test]
fn a_programs_end_comes_to_its_wait_with_no_call_in_flight() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        let (h, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        answer(&conn, h.id, &Answer::Started { pid: 4321 });
        answer(&conn, h.id, &Answer::Exited(Exit::Code(3)));
        conn
    });
    let client = Client::connect(&path).unwrap();
    let b = BoxId::new("b").unwrap();
    let mut proc = client
        .exec(&b, Program::of(&["/bin/true"]), Stdio::null())
        .unwrap();
    assert_eq!(proc.pid(), 4321);
    assert_eq!(
        proc.wait_for(Duration::from_secs(60)),
        Ok(Some(Exit::Code(3)))
    );
    drop(proc);
    drop(agent.join().unwrap());
}

/// HEX-SOCK-4: an agent that refuses a connection and closes it with the client's hello unread
/// leaves the client a reset, reported once and before the refusal it sent; the client still reads
/// the refusal, so connect fails with it, typed, rather than with the reset.
#[test]
fn a_refusal_before_a_reset_is_the_one_connect_returns() {
    let (_dir, path, listener) = agent_socket();
    let refusal = Refusal::Exhausted {
        what: "connections to the node agent's socket",
        bound: 2,
    };
    let agent = {
        let refusal = refusal.clone();
        std::thread::spawn(move || {
            let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
            // The client's hello is in this side's queue, unread, before the refusal goes.
            let mut hello = [PollFd::new(&conn, PollFlags::IN)];
            let ts = Timespec {
                tv_sec: 30,
                tv_nsec: 0,
            };
            assert_eq!(rustix::event::poll(&mut hello, Some(&ts)).unwrap(), 1);
            // As the node agent refuses a connection: to no request (id 0).
            let bytes = wire::answer(0, &Answer::Refused(refusal), wire::MAX_MESSAGE).unwrap();
            rustix::net::send(&conn, &bytes, SendFlags::NOSIGNAL).unwrap();
        })
    };
    assert_eq!(Client::connect(&path).err(), Some(refusal));
    agent.join().unwrap();
}

/// One wire (the lead's word of 2026-11-09, which drops the append-only rule): a client refuses an agent of another
/// wire at the hello, typed, naming both wires' identities and both builds, and sends it nothing more; an agent of its
/// own wire it serves. Do: connect to a stand-in agent that answers the hello as an agent of another wire would (its
/// own identity and build, in the hello's head every build lays out alike); then connect again, answered in this wire,
/// and delete a box. Expect: the first connect refused as `Invalid { what: "wire" }` naming both, and its connection
/// ended with nothing more sent; the second greeted and its delete answered.
#[test]
fn an_agent_of_another_wire_is_refused_at_the_hello() {
    let (_dir, path, listener) = agent_socket();
    let theirs = !wire::IDENTITY;
    let their_build = "hex 9.9.9 (another wire)";
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        let (h, request) = next_request(&conn);
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
        rustix::net::send(&conn, &bytes, SendFlags::NOSIGNAL).unwrap();
        // Nothing more comes: the client's end of the connection, read as its close.
        let mut rest = [0u8; 64];
        let after = rustix::net::recv(&conn, &mut rest, RecvFlags::empty())
            .unwrap()
            .0;
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        let (h, request) = next_request(&conn);
        let Request::Call(Call::Delete(id)) = request else {
            panic!("a delete, not {request:?}")
        };
        answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        (after, conn)
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
    assert_eq!(client.delete(&b), Err(Refusal::NoSuchBox(b.clone())));
    let (after, conn) = agent.join().unwrap();
    assert_eq!(after, 0, "the refused client sent nothing after its hello");
    drop(conn);
}

/// The next request on `conn` and the descriptors that came with it, waiting at most 30 s for it.
fn next_request_with(conn: &OwnedFd) -> (Header, Request, Vec<OwnedFd>) {
    let mut ready = [PollFd::new(conn, PollFlags::IN)];
    let ts = Timespec {
        tv_sec: 30,
        tv_nsec: 0,
    };
    assert_eq!(rustix::event::poll(&mut ready, Some(&ts)).unwrap(), 1);
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
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(came) = m {
            fds.extend(came);
        }
    }
    let received = Received {
        bytes: &buf[..msg.bytes],
        size: msg.bytes,
        descriptors: fds.len(),
        descriptors_cut: false,
    };
    let (h, request) = wire::read_request(&received, wire::MAX_MESSAGE).unwrap();
    (h, request, fds)
}

/// Sends `answer` to request `h`, with `fds` riding as its descriptors.
fn answer_with(conn: &OwnedFd, h: Header, answer: &Answer, fds: &[BorrowedFd<'_>]) {
    let bytes = wire::answer(h.id, answer, wire::MAX_MESSAGE).unwrap();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() {
        assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
    }
    rustix::net::sendmsg(
        conn,
        &[IoSlice::new(&bytes)],
        &mut control,
        SendFlags::NOSIGNAL,
    )
    .unwrap();
}

/// A stand-in agent's hello that runs OCI boxes, as a guest's node does: room for one call and one program.
fn oci_hello() -> Answer {
    Answer::Hello(wire::Hello {
        classes: vec![Class::Oci],
        ..agents_hello(1)
    })
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): an agent whose program can't take the caller's own pipes
/// (a guest's, whose streams are connections of their own) lets them go and answers the exec with its streams'
/// ends; expect the client to copy between those ends and the caller's pipes, so what the caller writes to its
/// stdin pipe comes back on its stdout pipe, to its end, and the program's end comes as the agent sent it. The
/// one copy left runs in the caller's client, never in the node.
#[test]
fn the_client_copies_between_its_callers_pipes_and_the_ends_its_agent_gives() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        let (h, request, _) = next_request_with(&conn);
        assert!(matches!(request, Request::Hello { .. }), "{request:?}");
        answer_with(&conn, h, &oci_hello(), &[]);
        let (h, request, carried) = next_request_with(&conn);
        let Request::Call(Call::Exec { streams, .. }) = request else {
            panic!("expected an exec, got {request:?}");
        };
        assert!(streams.stdin && streams.stdout && !streams.stderr);
        assert_eq!(carried.len(), 2);
        // A guest's program can't take them: let go here, as a guest's node lets them go.
        drop(carried);
        let pair = || {
            rustix::net::socketpair(
                AddressFamily::UNIX,
                SocketType::STREAM,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap()
        };
        let ((stdin, stdin_end), (stdout, stdout_end)) = (pair(), pair());
        let given = Answer::Given {
            pid: 42,
            given: Streams {
                stdin: true,
                stdout: true,
                stderr: false,
            },
        };
        answer_with(&conn, h, &given, &[stdin_end.as_fd(), stdout_end.as_fd()]);
        drop((stdin_end, stdout_end));
        // The program: its stdin to its stdout, to its end, then its own end.
        std::io::copy(
            &mut std::fs::File::from(stdin),
            &mut std::fs::File::from(stdout),
        )
        .unwrap();
        answer_with(&conn, h, &Answer::Exited(Exit::Code(0)), &[]);
        // Held open until the client has gone, as an agent's connection is.
        let mut rest = [0u8; 64];
        let _ = rustix::net::recv(&conn, &mut rest, RecvFlags::empty());
    });
    let client = Client::connect(&path).unwrap();
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("g").unwrap(),
            Program::of(&["cat"]),
            Stdio {
                stdin: Some(in_r),
                stdout: Some(out_w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let wrote = in_w.write_all(b"round trip\n");
    drop(in_w);
    let mut out = String::new();
    let read = out_r.read_to_string(&mut out);
    assert!(
        wrote.is_ok() && read.is_ok() && out == "round trip\n",
        "the caller's pipes carried nothing: wrote {wrote:?}, read {read:?}, {out:?}"
    );
    assert_eq!(
        proc.wait_for(Duration::from_secs(30)).unwrap(),
        Some(Exit::Code(0))
    );
    drop(proc);
    drop(client);
    agent.join().unwrap();
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): an agent that answers an exec with the end of a stream the
/// exec neither carried nor asked for: expect the exec refused as broken, naming the stream, and its program
/// killed, never an end handed to a caller that didn't ask for it.
#[test]
fn an_end_of_a_stream_its_exec_neither_carried_nor_asked_for_is_refused() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        let (h, _, _) = next_request_with(&conn);
        answer_with(&conn, h, &oci_hello(), &[]);
        let (exec, request, _) = next_request_with(&conn);
        assert!(
            matches!(&request, Request::Call(Call::Exec { streams, .. }) if *streams == Streams::default()),
            "{request:?}"
        );
        let (_here, end) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let given = Answer::Given {
            pid: 42,
            given: Streams {
                stdout: true,
                ..Streams::default()
            },
        };
        answer_with(&conn, exec, &given, &[end.as_fd()]);
        let (t, taken, _) = next_request_with(&conn);
        assert_eq!((t.id, taken), (exec.id, Request::Taken));
        let (k, kill, _) = next_request_with(&conn);
        answer_with(&conn, k, &Answer::Done, &[]);
        (exec.id, kill)
    });
    let client = Client::connect(&path).unwrap();
    let got = client.exec(
        &BoxId::new("g").unwrap(),
        Program::of(&["true"]),
        Stdio::null(),
    );
    assert!(
        matches!(&got, Err(Refusal::Broken(why)) if why.contains("stdout") && why.contains("neither carried nor asked")),
        "{:?}",
        got.as_ref().err()
    );
    let (exec, kill) = agent.join().unwrap();
    assert_eq!(kill, Request::Call(Call::Kill { exec }));
}

/// A given answer to `exec` with one end of a fresh socket pair as its stdout's, sent on `conn`.
fn give_stdout(conn: &OwnedFd, exec: Header) {
    let (_here, end) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let given = Answer::Given {
        pid: 42,
        given: Streams {
            stdout: true,
            ..Streams::default()
        },
    };
    answer_with(conn, exec, &given, &[end.as_fd()]);
}

/// A caller's stdio asking for its program's stdout.
fn asking_stdout() -> Stdio {
    Stdio {
        ask: Ask {
            stdout: true,
            ..Ask::default()
        },
        ..Stdio::null()
    }
}

/// Native plan step 8, guest stdio (HEX-GUEST-41): the agent keeps its copies of a given answer's ends until its
/// client says it took them: expect the client to say so (`Request::Taken`, by the exec's id) as the answer comes,
/// before its caller acts on it, so the caller's next request, the release its dropped program sends, comes after.
#[test]
fn the_client_says_it_took_a_given_answers_ends_before_its_caller_acts_on_it() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        let (exec, request, _) = next_request_with(&conn);
        assert!(
            matches!(&request, Request::Call(Call::ExecStreams { ask, .. }) if ask.stdout),
            "{request:?}"
        );
        give_stdout(&conn, exec);
        // What the client says from here to its caller's last word, the release its dropped program sends.
        let mut said = Vec::new();
        loop {
            let (h, request, _) = next_request_with(&conn);
            let last = request == Request::Release;
            said.push((h.id, request));
            if last {
                return (exec.id, said);
            }
        }
    });
    let client = Client::connect(&path).unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("p").unwrap(),
            Program::of(&["true"]),
            asking_stdout(),
        )
        .unwrap();
    assert!(proc.streams().unwrap().stdout.is_some());
    drop(proc);
    let (exec, said) = agent.join().unwrap();
    assert_eq!(
        said,
        [(exec, Request::Taken), (exec, Request::Release)],
        "the client's first word after the given answer wasn't that it took the ends"
    );
}

/// The lead's word of 2026-11-09 on the client's `Taken` (HEX-GUEST-41): one that finds no room is queued, never lost,
/// and goes out before any later request. Do: an exec asking for stdout is read; an exec whose request is as large as
/// the client's send buffer (`filling`) is left unread, so no send finds room; the given answer comes, and once the
/// exec has returned its program, the caller kills it; then the agent reads on. Expect: after the large request, the
/// word that the given ends were taken, by the exec's id, then the kill, nothing between.
#[test]
fn a_taken_queued_for_room_goes_out_before_any_later_request() {
    let (_dir, path, listener) = agent_socket();
    let (exec_read, read) = std::sync::mpsc::channel();
    let (large_held, held) = std::sync::mpsc::channel();
    let (returned, exec_returned) = std::sync::mpsc::channel::<()>();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 3);
        let (exec, request, _) = next_request_with(&conn);
        assert!(
            matches!(&request, Request::Call(Call::ExecStreams { .. })),
            "{request:?}"
        );
        exec_read.send(()).unwrap();
        let (large, request) = peek_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        large_held.send(()).unwrap();
        give_stdout(&conn, exec);
        exec_returned.recv().unwrap();
        let (_, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        let (t, taken) = next_request(&conn);
        let (k, kill) = next_request(&conn);
        answer(&conn, k.id, &Answer::Done);
        answer(
            &conn,
            large.id,
            &Answer::Refused(Refusal::NoSuchBox(BoxId::new("l").unwrap())),
        );
        (exec.id, (t.id, taken), kill, conn)
    });
    let client = Client::connect(&path).unwrap();
    std::thread::scope(|s| {
        let exec = s.spawn(|| {
            client.exec(
                &BoxId::new("p").unwrap(),
                Program::of(&["true"]),
                asking_stdout(),
            )
        });
        read.recv().unwrap();
        let program = filling(&client);
        let large = s.spawn(|| client.exec(&BoxId::new("l").unwrap(), program, Stdio::null()));
        held.recv().unwrap();
        let mut proc = exec.join().unwrap().unwrap();
        returned.send(()).unwrap();
        proc.kill().unwrap();
        assert!(large.join().unwrap().is_err());
        drop(proc);
    });
    let (exec, taken, kill, conn) = agent.join().unwrap();
    assert_eq!(taken, (exec, Request::Taken), "the word didn't come first");
    assert_eq!(kill, Request::Call(Call::Kill { exec }));
    drop(conn);
}

/// The lead's word of 2026-11-09 on the client's `Taken` (HEX-GUEST-41): with both sockets full the client's reader
/// waits on neither itself nor the agent. The node agent reads no request while its answers wait for room, so a reader
/// that waited for room to send its word, reading nothing meanwhile, would wait for good. Do: an exec asking for stdout
/// is read; an exec as large as the client's send buffer is left unread (no send finds room); the agent sends the
/// given answer, then more answers than its own send buffer holds, which wait until the client reads, and only then
/// reads on. Expect: the large request, then the word that the given ends were taken, by the exec's id.
#[test]
fn a_taken_owed_with_both_sockets_full_goes_out_without_hanging() {
    let (_dir, path, listener) = agent_socket();
    let (exec_read, read) = std::sync::mpsc::channel();
    let (large_held, held) = std::sync::mpsc::channel();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 3);
        let (exec, request, _) = next_request_with(&conn);
        assert!(
            matches!(&request, Request::Call(Call::ExecStreams { .. })),
            "{request:?}"
        );
        exec_read.send(()).unwrap();
        let (large, request) = peek_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        large_held.send(()).unwrap();
        give_stdout(&conn, exec);
        more_than_room(&conn);
        let (_, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Exec { .. })),
            "{request:?}"
        );
        let (t, taken) = next_request(&conn);
        answer(
            &conn,
            large.id,
            &Answer::Refused(Refusal::NoSuchBox(BoxId::new("l").unwrap())),
        );
        (exec.id, (t.id, taken), conn)
    });
    let client = Client::connect(&path).unwrap();
    std::thread::scope(|s| {
        let exec = s.spawn(|| {
            client.exec(
                &BoxId::new("p").unwrap(),
                Program::of(&["true"]),
                asking_stdout(),
            )
        });
        read.recv().unwrap();
        let program = filling(&client);
        let large = s.spawn(|| client.exec(&BoxId::new("l").unwrap(), program, Stdio::null()));
        held.recv().unwrap();
        let proc = exec.join().unwrap().unwrap();
        assert!(large.join().unwrap().is_err());
        drop(proc);
    });
    let (exec, taken, conn) = agent.join().unwrap();
    assert_eq!(taken, (exec, Request::Taken));
    drop(conn);
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

/// Native plan step 8, guest stdio (HEX-GUEST-41): a guest program that ends without reading its stdin, its stdin's
/// connection ending with it, while the caller holds its own stdin pipe open: expect the client to let the caller's
/// pipe go with that connection's end, so the pipe has no reader once the program has ended, as a local program's
/// own stdin pipe has none; never a copier left waiting on the caller.
#[test]
fn the_stdin_copier_lets_the_callers_pipe_go_when_its_programs_connection_ends() {
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        let (h, _, _) = next_request_with(&conn);
        answer_with(&conn, h, &oci_hello(), &[]);
        let (h, _, carried) = next_request_with(&conn);
        assert_eq!(carried.len(), 1);
        drop(carried);
        let (stdin, stdin_end) = rustix::net::socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();
        let given = Answer::Given {
            pid: 42,
            given: Streams {
                stdin: true,
                ..Streams::default()
            },
        };
        answer_with(&conn, h, &given, &[stdin_end.as_fd()]);
        drop(stdin_end);
        // The program ends without reading its stdin: its connection ends with it.
        drop(stdin);
        answer_with(&conn, h, &Answer::Exited(Exit::Code(0)), &[]);
        // Held open until the client has gone, as an agent's connection is.
        let mut rest = [0u8; 64];
        let _ = rustix::net::recv(&conn, &mut rest, RecvFlags::empty());
    });
    let client = Client::connect(&path).unwrap();
    let (in_r, in_w) = std::io::pipe().unwrap();
    let mut proc = client
        .exec(
            &BoxId::new("g").unwrap(),
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
        "the client still reads the caller's stdin pipe after its program's connection ended"
    );
    drop(proc);
    drop(client);
    agent.join().unwrap();
}

/// This thread's voluntary context switches so far: each a wait it blocked in (proc(5), `/proc/thread-self/status`).
fn voluntary_switches() -> u64 {
    std::fs::read_to_string("/proc/thread-self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The node agent's socket, each wait the core makes on it and each read counted by the thread that made it. The
/// core waits on a connection only by polling the handle its transport lends it, and reads it only through `recv`
/// (`crate::calls`), so these count its waits and reads exactly, and nothing else's.
struct Watched {
    socket: Socket,
    caller: std::thread::ThreadId,
    waits: std::sync::atomic::AtomicU64,
    reads: std::sync::atomic::AtomicU64,
    elsewhere: std::sync::atomic::AtomicU64,
}

impl Watched {
    fn count(&self, mine: &std::sync::atomic::AtomicU64) {
        let counter = if std::thread::current().id() == self.caller {
            mine
        } else {
            &self.elsewhere
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn counts(&self) -> (u64, u64, u64) {
        let load = |n: &std::sync::atomic::AtomicU64| n.load(std::sync::atomic::Ordering::Relaxed);
        (load(&self.waits), load(&self.reads), load(&self.elsewhere))
    }
}

impl Transport for Watched {
    type Reading = Reading;

    fn answers() -> &'static [Patience; KINDS] {
        Socket::answers()
    }

    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        Socket::class_answers(class)
    }

    fn reading(&self) -> Reading {
        self.socket.reading()
    }

    fn handle(&self) -> Handle<'_> {
        self.count(&self.waits);
        self.socket.handle()
    }

    fn recv(&self, reading: &mut Reading, take: &mut Take<'_>) -> Result<bool, Refusal> {
        self.count(&self.reads);
        self.socket.recv(reading, take)
    }

    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        self.socket.try_send(bytes, at, lent)
    }

    fn shutdown(&self) {
        self.socket.shutdown();
    }
}

/// Goal 16's per-call cost on the one core (decision 3's client): an uncontended call waits at most once, on its
/// connection for its answer, which it reads itself, and no other thread waits on or reads the connection for it.
/// Do: one inspect to warm the connection, then 200 in a row on this thread through the node agent's socket, each
/// answered by a stand-in agent, every wait the core makes on the socket and every read counted by thread
/// ([`Watched`]). Expect: at most one wait a call and one read each, all this thread's, and no reading thread. The
/// thread's voluntary context switches are printed, not held to that bound: they count every sleep of the thread,
/// the client's one wait a call and sleeps it never made (an allocator lock another thread holds, a fault behind
/// another thread's mapping, reclaim in a send), so at one a call (200 of 200 alone, 00aad) any such sleep failed it.
#[test]
fn an_uncontended_call_blocks_at_most_once_and_wakes_no_other_thread() {
    let calls = 200u64;
    let (_dir, path, listener) = agent_socket();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        for _ in 0..=calls {
            let (h, request) = next_request(&conn);
            let Request::Call(Call::Inspect(id)) = request else {
                panic!("an inspect, not {request:?}")
            };
            answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        }
        conn
    });
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::connect(&fd, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    let client = Core::hello(
        Shared::try_new(Watched {
            socket: Socket { fd },
            caller: std::thread::current().id(),
            waits: std::sync::atomic::AtomicU64::new(0),
            reads: std::sync::atomic::AtomicU64::new(0),
            elsewhere: std::sync::atomic::AtomicU64::new(0),
        })
        .unwrap(),
    )
    .unwrap();
    let b = BoxId::new("b").unwrap();
    assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b.clone())));
    let watched = &client.shared().transport;
    let (waits, reads, elsewhere) = watched.counts();
    let before = voluntary_switches();
    for _ in 0..calls {
        assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b.clone())));
    }
    let switches = voluntary_switches() - before;
    let (waited, read, other) = watched.counts();
    let (waits, reads, elsewhere) = (waited - waits, read - reads, other - elsewhere);
    eprintln!(
        "{calls} calls: {waits} waits and {reads} reads on the connection, {elsewhere} by other threads; \
         {switches} voluntary context switches of the calling thread"
    );
    assert_eq!(
        elsewhere, 0,
        "another thread waited on or read the client's connection"
    );
    assert!(
        waits <= calls,
        "{waits} waits on the connection in {calls} calls"
    );
    assert_eq!(reads, calls, "each answer read once, by its own caller");
    // The reading thread the turn replaced was named `hex-client`; the stdio copies of tests beside this one,
    // `hex-client-stdio`, read no connection.
    let readers = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|t| std::fs::read_to_string(t.ok()?.path().join("comm")).ok())
        .filter(|comm| comm.trim() == "hex-client")
        .count();
    assert_eq!(readers, 0, "the client's reading thread");
    drop(agent.join().unwrap());
}

/// `calls` inspects in a row through `client`: this thread's voluntary context switches across them, and the calls'
/// times, sorted.
fn measured(client: &Client, calls: u64) -> (u64, Vec<std::time::Duration>) {
    let b = BoxId::new("b").unwrap();
    let before = voluntary_switches();
    let mut took = Vec::new();
    for _ in 0..calls {
        let asked = std::time::Instant::now();
        assert_eq!(client.inspect(&b), Err(Refusal::NoSuchBox(b.clone())));
        took.push(asked.elapsed());
    }
    let switches = voluntary_switches() - before;
    took.sort_unstable();
    (switches, took)
}

/// A stand-in agent answering `calls` inspects after the hello.
fn inspect_agent(listener: OwnedFd, calls: u64) -> std::thread::JoinHandle<OwnedFd> {
    std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        for _ in 0..calls {
            let (h, request) = next_request(&conn);
            let Request::Call(Call::Inspect(id)) = request else {
                panic!("an inspect, not {request:?}")
            };
            answer(&conn, h.id, &Answer::Refused(Refusal::NoSuchBox(id)));
        }
        conn
    })
}

/// Decision 3's client, the two cores compared on one machine (goal 16's per-call cost): the reader's turn against a
/// reading thread of the client's own, the design the turn replaced, set up here as the event form's loop on a thread
/// that does nothing else (it waits on the connection and hands each answer on, as that thread did). Do: 2,000
/// uncontended inspects through each, answered by a stand-in agent; count the voluntary context switches of the
/// client's threads, the caller and, for the second, its reading thread. Expect: the turn's threads wait fewer times a
/// call: its caller reads its own answer, where the reading thread wakes and then wakes the caller.
#[test]
fn the_readers_turn_wakes_fewer_threads_a_call_than_a_reading_thread() {
    let calls = 2_000u64;
    let p = |took: &[std::time::Duration], q: usize| took[(took.len() - 1) * q / 100];
    // The reader's turn.
    let (_dir, path, listener) = agent_socket();
    let agent = inspect_agent(listener, calls);
    let client = Client::connect(&path).unwrap();
    let (turn, turn_took) = measured(&client, calls);
    drop(client);
    drop(agent.join().unwrap());
    // A reading thread of the client's own: the connection is the loop's, which a thread reads, waiting on it.
    let (_dir, path, listener) = agent_socket();
    let agent = inspect_agent(listener, calls);
    let client = Client::connect(&path).unwrap();
    client.shared().read_by_loop().unwrap();
    let (caller, reader, thread_took) = std::thread::scope(|s| {
        let shared = client.shared();
        let reading = s.spawn(move || {
            let before = voluntary_switches();
            while shared.read_as_a_thread_would().is_ok() {}
            voluntary_switches() - before
        });
        let (caller, took) = measured(&client, calls);
        client.shared().transport.shutdown();
        (caller, reading.join().unwrap(), took)
    });
    drop(client);
    drop(agent.join().unwrap());
    let a_call = |n: u64| n as f64 / calls as f64;
    eprintln!(
        "reader's turn: {:.2} waits a call (the caller's), p50 {:?}, p99 {:?}; reading thread: {:.2} waits a call \
         (caller {:.2}, reader {:.2}), p50 {:?}, p99 {:?} ({calls} calls each)",
        a_call(turn),
        p(&turn_took, 50),
        p(&turn_took, 99),
        a_call(caller + reader),
        a_call(caller),
        a_call(reader),
        p(&thread_took, 50),
        p(&thread_took, 99)
    );
    assert!(
        turn < caller + reader,
        "the reader's turn waits {turn} times in {calls} calls, a reading thread's client {}",
        caller + reader
    );
}

/// A box of `class`, `oci` or `firecracker`, of a container spec that names nothing on this host.
fn container_box(id: &str, class: Class) -> hex_api::BoxSpec {
    let oci = Box::new(hex_api::OciSpec {
        rootfs: hex_api::Rootfs::Dir("/stand-in".into()),
        init: Program::of(&["/bin/true"]),
        mounts: Vec::new(),
        hostname: None,
        network: hex_api::Network::Private,
        hosts: Vec::new(),
    });
    hex_api::BoxSpec {
        id: BoxId::new(id).unwrap(),
        bounds: hex_api::Bounds::default(),
        class: match class {
            Class::Firecracker => hex_api::ClassSpec::Firecracker(oci),
            _ => hex_api::ClassSpec::Oci(oci),
        },
        owner: None,
        devices: Vec::new(),
        allocation: None,
        route: None,
    }
}

/// AC-T7.4.1: a box's class times its create, each (kind, class) a learned wait of its own; a class with no record
/// yet measures, never borrowing another's. Do: through the node agent's socket, create an `oci` box the stand-in
/// agent never answers, so the create runs out at the `oci` create's bound `b`; then create a `firecracker` box, which
/// the agent answers only 4`b` after it reads it, a microVM's boot being slower than any OCI create recorded. Expect:
/// the `oci` create refused as twice its slowest; the `firecracker` create answered.
#[test]
fn a_vm_create_slower_than_the_oci_record_is_not_refused_by_it() {
    let (_dir, path, listener) = agent_socket();
    let (measured, oci_bound) = std::sync::mpsc::channel::<Duration>();
    let agent = std::thread::spawn(move || {
        let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
        hello(&conn, 1);
        let (_, request) = next_request(&conn);
        assert!(
            matches!(request, Request::Call(Call::Create(_))),
            "{request:?}"
        );
        let b = oci_bound.recv().unwrap();
        let (h, request) = next_request(&conn);
        let Request::Call(Call::Create(spec)) = request else {
            panic!("a create, not {request:?}")
        };
        // Held 4b, a span the client sends nothing in: the poll ends at its timeout, or at the client's end.
        let mut quiet = [PollFd::new(&conn, PollFlags::IN)];
        let _ = rustix::event::poll(&mut quiet, Some(&Timespec::try_from(b * 4).unwrap()));
        let info = hex_api::BoxInfo {
            published: Vec::new(),
            grants: Vec::new(),
            id: spec.id,
            class: Class::Firecracker,
            state: hex_api::BoxState::Created,
            bounds: spec.bounds,
            allocation: None,
            adopted: false,
            membership: None,
            exit: None,
            abandoned: 0,
            route: None,
            last_resort: None,
        };
        let bytes = wire::answer(h.id, &Answer::Info(info), wire::MAX_MESSAGE).unwrap();
        let _ = rustix::net::send(&conn, &bytes, SendFlags::NOSIGNAL);
        conn
    });
    let client = Client::connect(&path).unwrap();
    let began = std::time::Instant::now();
    let oci = client.create(container_box("o", Class::Oci));
    measured.send(began.elapsed()).unwrap();
    let vm = client.create(container_box("v", Class::Firecracker));
    eprintln!("client_class_waits oci={oci:?} vm={vm:?}");
    assert!(
        matches!(&oci, Err(Refusal::Os { why, .. }) if why.contains("twice the slowest")),
        "{oci:?}"
    );
    assert!(
        matches!(&vm, Ok(info) if info.class == Class::Firecracker),
        "{vm:?}"
    );
    drop(client);
    drop(agent.join().unwrap());
}
