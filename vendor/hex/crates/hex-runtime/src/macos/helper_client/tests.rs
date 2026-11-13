//! The helper client over a real socket (a `SOCK_STREAM` socketpair), with a stand-in responder in place
//! of the daemon: every request reaches it whole, its descriptors ride across, and each answer and typed
//! refusal comes back mapped (HEX-MAC-12). The framing itself (`SOCK_STREAM`, `u32` length) is exercised,
//! not mocked.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::thread;

use hex_api::{BoxId, Program};
use hex_core::helper::{Answer, Refused, Request, State, encode_answer, read_request};
use rustix::net::{AddressFamily, SocketFlags, SocketType};

use super::*;

fn box_id(s: &str) -> BoxId {
    BoxId::new(s).expect("a valid box id")
}

fn a_program() -> Program {
    Program {
        args: vec!["/bin/echo".to_owned()],
        env: vec![],
        cwd: None,
        user: None,
    }
}

/// A stand-in helper on `fd`: reads framed requests and answers each as the daemon would, until the
/// client closes. Start's three descriptors are received and dropped (closed). A request for box "gone"
/// is refused `UnknownBox`, so the refusal mapping is exercised.
fn responder(fd: OwnedFd) {
    loop {
        let (bytes, fds) = match recv_framed(fd.as_fd()) {
            Ok(x) => x,
            Err(_) => return, // client closed
        };
        let got_fds = fds.len();
        drop(fds);
        let (version, id, request) = match read_request(&bytes, got_fds) {
            Ok(r) => r,
            Err(_) => return,
        };
        let answer = match &request {
            Request::Hello { .. } => Answer::Hello { version: VERSION },
            Request::Acquire { box_id, .. } if box_id.as_str() == "gone" => {
                Answer::Refused(Refused::UnknownBox(box_id.clone()))
            }
            Request::Acquire { .. } => Answer::Acquired { uid: 7000 },
            Request::Start { .. } => Answer::Started { pid: 1234 },
            Request::End { .. } => Answer::Ended,
            Request::Signal { .. } => Answer::Signalled,
            Request::Wait { .. } => Answer::Waited(Some(Exit::Code(3))),
            Request::Kill { .. } => Answer::Killed,
            Request::Inspect { .. } => Answer::State(State {
                members: u32::try_from(got_fds).unwrap_or(0),
                footprint: 200 << 20,
                peak: 240 << 20,
                processes: 3,
                crossings: 0,
                ended_over: false,
            }),
        };
        let out = encode_answer(version, id, &answer).expect("encode answer");
        if send_framed(fd.as_fd(), &out, &[]).is_err() {
            return;
        }
    }
}

fn pair() -> (Helper, thread::JoinHandle<()>) {
    let (client, server) = rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::empty(),
        None,
    )
    .expect("socketpair");
    // No MSG_NOSIGNAL on macOS: keep a dropped peer from raising SIGPIPE on the other's send.
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&client, true);
    let _ = rustix::net::sockopt::set_socket_nosigpipe(&server, true);
    let handle = thread::spawn(move || responder(server));
    let helper = Helper {
        fd: client,
        version: VERSION,
        next_id: AtomicU64::new(1),
        path: PathBuf::from("<test>"),
        turn: std::sync::Mutex::new(()),
    };
    (helper, handle)
}

/// HEX-MAC-12: acquire, start (its three stdio descriptors carried by SCM_RIGHTS), inspect and end each
/// round-trip through the real socket framing and come back as the answer the daemon gave.
#[test]
fn the_client_round_trips_every_call() {
    let (helper, handle) = pair();
    assert_eq!(
        helper
            .acquire(&box_id("b1"), 256 << 20, 16 << 20, None)
            .unwrap(),
        7000
    );

    let n0 = File::open("/dev/null").unwrap();
    let n1 = File::open("/dev/null").unwrap();
    let n2 = File::open("/dev/null").unwrap();
    let pid = helper
        .start(
            &box_id("b1"),
            &a_program(),
            [n0.as_fd(), n1.as_fd(), n2.as_fd()],
            true,
        )
        .unwrap();
    assert_eq!(pid, 1234);

    // Inspect's stand-in echoes the descriptor count it last saw: 0 for this (no fds on inspect).
    let state = helper.inspect(&box_id("b1")).unwrap();
    assert_eq!(state.members, 0);
    assert_eq!(state.footprint, 200 << 20);

    assert_eq!(
        helper.wait(&box_id("b1"), pid).unwrap(),
        Some(Exit::Code(3))
    );

    helper.end(&box_id("b1")).unwrap();

    drop(helper);
    handle.join().unwrap();
}

/// HEX-MAC-12: a typed refusal from the helper maps to the node's Refusal by name.
#[test]
fn a_refusal_maps_by_name() {
    let (helper, handle) = pair();
    match helper.acquire(&box_id("gone"), 1 << 20, 0, None) {
        Err(Refusal::NoSuchBox(b)) => assert_eq!(b.as_str(), "gone"),
        other => panic!("expected NoSuchBox, got {other:?}"),
    }
    drop(helper);
    handle.join().unwrap();
}
