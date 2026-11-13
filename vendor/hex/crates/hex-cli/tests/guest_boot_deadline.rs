//! AC-T7.4.1: guest init's calls to its node before it says ready carry the boot's remaining deadline. On 00aad a
//! Firecracker guest with the AMD GPU passed through booted, and its relay asked its node its bounds: the node, busy
//! with the GPU's initialization, answered after the hello's learned wait, "none came in 2.354074ms, twice the slowest
//! answer to a call of its kind", the relay ended, init with it, and the guest panicked, with most of the boot's 60 s
//! left. Init hands the relay the boot's deadline on the guest's own clock (`HEX_BOOT_READY_BY`, CLOCK_BOOTTIME
//! nanoseconds), which the relay's hello takes as its caller's.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::{BufRead, BufReader, Read};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use hex_api::Class;
use hex_core::wire::{self, Answer, Received, Request};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketAddrUnix, SocketFlags, SocketType};

/// The variable init hands the relay the boot's deadline in.
const READY_BY: &str = "HEX_BOOT_READY_BY";
/// The most a fixture step waits before the test fails rather than hangs, and the boot's deadline the second relay is
/// given: a bound nothing here comes near.
const FIXTURE: Duration = Duration::from_secs(30);

/// A stand-in node agent's listening socket, in a directory of its own.
fn agent_socket() -> (tempfile::TempDir, PathBuf, OwnedFd) {
    let cache = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/hex-t7/guest-boot");
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
    rustix::net::listen(&listener, 2).unwrap();
    (dir, path, listener)
}

/// Whether `fd` turns readable (a connection to take, a message, or its end) within `within`.
fn readable(fd: &OwnedFd, within: Duration) -> bool {
    let mut ready = [PollFd::new(fd, PollFlags::IN)];
    rustix::event::poll(&mut ready, Some(&Timespec::try_from(within).unwrap())).unwrap() == 1
}

/// The relay's connection and its hello's id, taken from `listener`, and when the hello was read.
fn hello(listener: &OwnedFd) -> (OwnedFd, u64, Instant) {
    assert!(readable(listener, FIXTURE), "the relay never connected");
    let conn = rustix::net::accept_with(listener, SocketFlags::CLOEXEC).unwrap();
    assert!(readable(&conn, FIXTURE), "the relay never said hello");
    let read = Instant::now();
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let (n, _) = rustix::net::recv(&conn, &mut buf, RecvFlags::empty()).unwrap();
    let message = Received {
        bytes: &buf[..n],
        size: n,
        descriptors: 0,
        descriptors_cut: false,
    };
    let (header, request) = wire::read_request(&message, wire::MAX_MESSAGE).unwrap();
    assert!(matches!(request, Request::Hello { .. }), "{request:?}");
    (conn, header.id, read)
}

/// Answers the hello `id` on `conn` as a node of this wire does.
fn answer_hello(conn: &OwnedFd, id: u64) {
    let hello = wire::Hello {
        build: wire::build(),
        classes: vec![Class::Oci],
        limits: wire::Limits {
            connections: 1,
            message_bytes: wire::MAX_MESSAGE,
            in_flight: 1,
            programs: 1,
        },
        budgets: wire::Budgets {
            in_flight: 1,
            programs: 1,
        },
    };
    let bytes = wire::answer(id, &Answer::Hello(hello), wire::MAX_MESSAGE).unwrap();
    let _ = rustix::net::send(conn, &bytes, SendFlags::NOSIGNAL);
}

/// A relay ended and reaped when dropped, so a failing assertion leaves none running.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `hex guest relay` to the node at `node`, listening in `dir`, handed the boot's deadline `ready_by` as init hands it
/// where given.
fn relay(node: &Path, dir: &Path, ready_by: Option<u128>) -> Reaped {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hex"));
    command
        .args(["guest", "relay", "--node"])
        .arg(node)
        .arg("--unix")
        .arg(dir)
        .arg("--staged")
        .arg(dir.join("staged"))
        .env_remove(READY_BY)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(at) = ready_by {
        command.env(READY_BY, at.to_string());
    }
    Reaped(command.spawn().unwrap())
}

/// The relay's first line on stdout, then, once it has ended (ended here if it still runs), its stderr.
fn said(mut relay: Reaped) -> (String, String) {
    let mut line = String::new();
    BufReader::new(relay.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let _ = relay.0.kill();
    let _ = relay.0.wait();
    let mut err = String::new();
    relay
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (line, err)
}

/// Now on the guest's clock, CLOCK_BOOTTIME, in nanoseconds.
fn boottime() -> u128 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Boottime);
    u128::try_from(t.tv_sec).unwrap() * 1_000_000_000 + u128::try_from(t.tv_nsec).unwrap()
}

/// AC-T7.4.1 (original): do start `hex guest relay` to a stand-in node that never answers its hello, with no boot
/// deadline, measuring `m` from reading the hello to the relay's end; then start it again with the boot's deadline as
/// init hands it, to a node that answers `2m` after reading its hello: past the hello's learned wait, well inside the
/// boot's deadline. Expect: the first refused at its learned bound, "twice the slowest", unchanged where no caller
/// gives a deadline; the second ready.
#[test]
fn original_the_relays_hello_waits_out_a_busy_node_within_the_boots_deadline() {
    let (dir, node, listener) = agent_socket();
    let first = relay(&node, &dir.path().join("first"), None);
    let (conn, _, read) = hello(&listener);
    assert!(readable(&conn, FIXTURE), "the relay never ended");
    let m = read.elapsed();
    let (line, err) = said(first);
    drop(conn);
    eprintln!("guest_boot_deadline ready_by=none m={m:?} said={line:?} stderr={err:?}");
    assert_eq!(line, "", "{err}");
    assert!(
        err.contains("asking the node its bounds") && err.contains("twice the slowest answer"),
        "{err}"
    );
    let ready_by = boottime() + FIXTURE.as_nanos();
    let second = relay(&node, &dir.path().join("second"), Some(ready_by));
    let (conn, id, read) = hello(&listener);
    // Held 2m, a span the relay sends nothing in: the poll ends at its timeout, or at the relay's end.
    let ended = readable(&conn, 2 * m);
    answer_hello(&conn, id);
    let (line, err) = said(second);
    drop(conn);
    eprintln!(
        "guest_boot_deadline ready_by={ready_by} held={:?} ended_first={ended} said={line:?} stderr={err:?}",
        read.elapsed()
    );
    assert_eq!(line, "ready\n", "{err}");
}
