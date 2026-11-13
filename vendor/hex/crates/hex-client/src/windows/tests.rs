use super::*;
use std::io::{Read, Write};
use std::os::windows::io::AsHandle;
use std::time::Duration;

use hex_api::{Ask, BoxId, Class, Exit, Program, Runtime, Stdio};
use hex_core::wire::{Answer, Call, Header, Hello, Limits, Request, Streams};
use rustix::event::{PollFd, PollFlags, Timespec};

const TEST_WAIT: Duration = Duration::from_secs(30);

fn pair() -> (tempfile::TempDir, Raw, Raw) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s");
    let listener = Raw::listen(&path).unwrap();
    let port = socket::Port::new().unwrap();
    port.add(&listener).unwrap();
    socket::accept(&port, &listener, 1).unwrap();
    let client = Raw::connect(&path).unwrap();
    let mut done = Vec::new();
    port.wait(&mut done, Some(TEST_WAIT), 1).unwrap();
    let peer = match done.pop() {
        Some(socket::Done::Accepted { conn, .. }) => conn.unwrap(),
        _ => panic!("the pending accept did not complete"),
    };
    (dir, client, peer)
}

fn next_request(raw: &Raw) -> (Header, Request) {
    let mut frame = Deframer::new(wire::MAX_MESSAGE);
    loop {
        let mut ready = [PollFd::from_borrowed_fd(raw.as_socket(), PollFlags::IN)];
        let wait = Timespec::try_from(TEST_WAIT).unwrap();
        assert_eq!(rustix::event::poll(&mut ready, Some(&wait)).unwrap(), 1);
        let mut byte = [0];
        assert_eq!(raw.recv(&mut byte).unwrap(), 1);
        if let (_, Some(message)) = frame.take(&byte).unwrap() {
            return wire::read_request_apart(&message, wire::MAX_MESSAGE).unwrap();
        }
    }
}

fn answer(raw: &Raw, id: u64, answer: Answer, handles: &[u64]) {
    let message = wire::answer(id, &answer, wire::MAX_MESSAGE).unwrap();
    let enveloped = socket::envelope(handles, &message).unwrap();
    let frame = stream::frame(&enveloped, FRAME_BOUND).unwrap();
    let mut at = 0;
    // One byte per send exercises a prefix and envelope cut across receives, without guessing packet sizes.
    while at < frame.len() {
        at += raw.send(&frame[at..at + 1]).unwrap();
    }
}

fn hello(raw: &Raw) {
    let (header, request) = next_request(raw);
    assert!(matches!(request, Request::Hello { .. }));
    answer(
        raw,
        header.id,
        Answer::Hello(Hello {
            build: wire::build(),
            classes: vec![Class::Process],
            limits: Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 1,
            },
            budgets: wire::Budgets {
                in_flight: 2,
                programs: 2,
            },
        }),
        &[],
    );
}

/// Native step 8: a local Windows transport's handle can be polled by the common reader, and a quiet
/// non-blocking receive returns WouldBlock. Switching the same socket back makes a peer's next byte readable.
#[test]
fn a_windows_transport_is_non_blocking_and_waited_on_by_its_handle() {
    let (_dir, raw, peer) = pair();
    raw.set_nonblocking(true).unwrap();
    assert_eq!(
        raw.recv(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let mut ready = [PollFd::from_borrowed_fd(raw.as_socket(), PollFlags::IN)];
    assert_eq!(
        rustix::event::poll(&mut ready, Some(&Timespec::default())).unwrap(),
        0
    );
    raw.set_nonblocking(false).unwrap();
    peer.send(b"x").unwrap();
    let mut byte = [0];
    assert_eq!(raw.recv(&mut byte).unwrap(), 1);
    assert_eq!(byte, *b"x");
}

/// HEX-RT-17, native step 8: the Windows transport hands a pipe the server duplicated into this process
/// to the common client core. Its caller reads that pipe, and the core sends Taken before the server sends
/// the program's end; no Windows reader thread or second calls implementation participates.
#[test]
fn the_common_core_takes_windows_stream_handles_and_acknowledges_them() {
    let (_dir, raw, peer) = pair();
    let server = std::thread::spawn(move || {
        hello(&peer);
        let (header, request) = next_request(&peer);
        assert!(matches!(request, Request::Call(Call::ExecStreams { .. })));
        let (reader, mut writer) = std::io::pipe().unwrap();
        let target = socket::Peer::open(std::process::id()).unwrap();
        let theirs = target.give(reader.as_handle()).unwrap();
        answer(
            &peer,
            header.id,
            Answer::Given {
                pid: 7,
                given: Streams {
                    stdin: false,
                    stdout: true,
                    stderr: false,
                },
            },
            &[theirs],
        );
        let (taken, request) = next_request(&peer);
        assert_eq!(taken.id, header.id);
        assert!(matches!(request, Request::Taken));
        drop(reader);
        writer.write_all(b"native stream").unwrap();
        drop(writer);
        answer(&peer, header.id, Answer::Exited(Exit::Code(0)), &[]);
    });
    raw.set_nonblocking(true).unwrap();
    let client = Core::hello(Shared::try_new(Socket { raw }).unwrap()).unwrap();
    assert_eq!(client.classes(), vec![Class::Process]);
    let mut proc = client
        .exec(
            &BoxId::new("b").unwrap(),
            Program::of(&["stand-in"]),
            Stdio {
                ask: Ask {
                    stdout: true,
                    ..Ask::default()
                },
                ..Stdio::null()
            },
        )
        .unwrap();
    let streams = proc.streams().unwrap();
    // Driving the turn flushes Taken. The server then writes and ends the program.
    assert_eq!(proc.wait_for(TEST_WAIT).unwrap(), Some(Exit::Code(0)));
    let mut stdout = hex_api::OwnedStream::from(streams.stdout.unwrap());
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"native stream");
    server.join().unwrap();
}
