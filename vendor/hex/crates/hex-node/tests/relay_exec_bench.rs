//! The guest relay's exec cost, measured (native plan step 5; docs/design/shards-guest.md §3; serves the
//! relay's fact-based waits, HEX-GUEST-7, kept only if neither figure worsens): a host's guest client
//! execs through the relay to a stand-in node over Unix sockets, and the run prints
//! - start: from `exec` to the first byte of its stdout, N execs one after another, as p50/p99/max;
//! - throughput: bytes a single exec's stdin carries through the relay to the node's pipe and back out
//!   its stdout, per second.
//!
//! Ignored by default: `cargo test -p hex-node --release --test relay_exec_bench -- --ignored --nocapture`,
//! with `RELAY_BENCH_EXECS` (default 2000) and `RELAY_BENCH_MIB` (default 1024) the run's sizes, recorded
//! with each run in bench/native/relay/.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::fs::File;
use std::io::{IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hex_api::{BoxId, Class, Exit, Program, Refusal, Runtime, Stdio};
use hex_client::guest::{Client, Link};
use hex_core::stream;
use hex_core::wire::{self, Answer, Call, Header, Hello, Received, Request};
use hex_node::relay::{Limits, Relay};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendFlags, SocketAddrUnix,
    SocketFlags, SocketType,
};

struct Ports(PathBuf);

impl Link for Ports {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        let name = if port == stream::CONTROL_PORT {
            "control.sock"
        } else {
            "stdio.sock"
        };
        hex_client::guest::own_duplex(
            std::os::unix::net::UnixStream::connect(self.0.join(name)).unwrap(),
        )
    }
}

fn next_request(conn: &OwnedFd) -> Option<(Header, Request, Vec<OwnedFd>)> {
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        conn,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )
    .ok()?;
    if msg.bytes == 0 {
        return None;
    }
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
    Some((h, r, fds))
}

fn answer(conn: &OwnedFd, h: Header, answer: &Answer) {
    let bytes = wire::answer(h.id, answer, wire::MAX_MESSAGE).unwrap();
    rustix::net::send(conn, &bytes, SendFlags::NOSIGNAL).unwrap();
}

/// A node's bounds as this host's platform gives them (`hex_node::serve::limits`): what the stand-in node's hello
/// tells, and the relay's own are taken from ([`Limits::of`]).
fn node_limits() -> wire::Limits {
    hex_node::serve::limits(&hex_node::sizes::Sizes::now().unwrap().platform())
}

/// The stand-in node's budgets over all its connections: its bounds' own.
fn budgets() -> wire::Budgets {
    let node = node_limits();
    wire::Budgets {
        in_flight: node.in_flight,
        programs: node.programs,
    }
}

/// A stand-in node: every exec is `cat`, its stdin copied to its stdout on a thread of its own.
fn node(listener: OwnedFd) {
    let conn = rustix::net::accept_with(&listener, SocketFlags::CLOEXEC).unwrap();
    while let Some((h, r, fds)) = next_request(&conn) {
        match r {
            Request::Hello { .. } => answer(
                &conn,
                h,
                &Answer::Hello(Hello {
                    build: wire::build(),
                    classes: vec![Class::Oci],
                    limits: node_limits(),
                    budgets: budgets(),
                }),
            ),
            Request::Call(Call::Exec { .. }) => {
                answer(&conn, h, &Answer::Started { pid: 1 });
                let mut fds = fds.into_iter();
                let mut stdin = File::from(fds.next().unwrap());
                let mut stdout = File::from(fds.next().unwrap());
                std::io::copy(&mut stdin, &mut stdout).unwrap();
                drop(stdout);
                answer(&conn, h, &Answer::Exited(Exit::Code(0)));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |v| v.parse().unwrap())
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture, release"]
fn relay_exec_start_and_throughput() {
    let dir = tempfile::tempdir().unwrap();
    let node_path = dir.path().join("node.sock");
    let listener = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&listener, &SocketAddrUnix::new(&node_path).unwrap()).unwrap();
    rustix::net::listen(&listener, 4).unwrap();
    let control = UnixListener::bind(dir.path().join("control.sock")).unwrap();
    let stdio = UnixListener::bind(dir.path().join("stdio.sock")).unwrap();
    let files = UnixListener::bind(dir.path().join("files.sock")).unwrap();
    let staged = dir.path().join("staged");
    std::fs::create_dir(&staged).unwrap();
    let relay = Relay::new(&node_path, &staged, Limits::of(&node_limits(), &budgets()));
    std::thread::spawn(move || relay.serve(Box::new(control), Box::new(stdio), Box::new(files)));
    std::thread::spawn(move || node(listener));
    let client =
        Client::connect(hex_client::guest::own_link(Ports(dir.path().to_owned())).unwrap())
            .unwrap();
    let b = BoxId::new("b").unwrap();
    let exec = |input: std::io::PipeReader, output: std::io::PipeWriter| {
        client
            .exec(
                &b,
                Program::of(&["cat"]),
                Stdio {
                    stdin: Some(input),
                    stdout: Some(output),
                    stderr: None,
                    ..Stdio::null()
                },
            )
            .unwrap()
    };

    let n = env("RELAY_BENCH_EXECS", 2000);
    let mut starts = Vec::with_capacity(n);
    for _ in 0..n {
        let (in_r, mut in_w) = std::io::pipe().unwrap();
        let (mut out_r, out_w) = std::io::pipe().unwrap();
        in_w.write_all(b"x").unwrap();
        drop(in_w);
        let began = Instant::now();
        let mut proc = exec(in_r, out_w);
        let mut byte = [0u8; 1];
        out_r.read_exact(&mut byte).unwrap();
        starts.push(began.elapsed());
        assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    }
    starts.sort();
    let at = |q: f64| starts[((starts.len() - 1) as f64 * q) as usize];
    println!(
        "RELAY start n={n} p50={:?} p99={:?} max={:?}",
        at(0.5),
        at(0.99),
        starts[starts.len() - 1]
    );

    let mib = env("RELAY_BENCH_MIB", 1024);
    let (in_r, mut in_w) = std::io::pipe().unwrap();
    let (mut out_r, out_w) = std::io::pipe().unwrap();
    let began = Instant::now();
    let mut proc = exec(in_r, out_w);
    let writer = std::thread::spawn(move || {
        let chunk = vec![7u8; 1 << 20];
        for _ in 0..mib {
            in_w.write_all(&chunk).unwrap();
        }
    });
    let mut sink = vec![0u8; 1 << 20];
    let mut got = 0usize;
    loop {
        let k = out_r.read(&mut sink).unwrap();
        if k == 0 {
            break;
        }
        got += k;
    }
    let took = began.elapsed();
    writer.join().unwrap();
    assert_eq!(got, mib << 20);
    assert_eq!(proc.wait_for(Duration::MAX).unwrap(), Some(Exit::Code(0)));
    println!(
        "RELAY throughput mib={mib} took={took:?} mib_per_s={:.1}",
        mib as f64 / took.as_secs_f64()
    );
}
