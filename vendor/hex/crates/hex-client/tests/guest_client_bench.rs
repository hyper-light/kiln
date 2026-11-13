//! The host's guest client's exec cost, measured (native plan step 5, constants; the lead's rule that a
//! learned buffer or stack size is kept only if neither latency nor throughput worsens): the client
//! execs through a stand-in node agent on loopback, as it does through a guest's relay, and the run prints
//! - start: from `exec` to the first byte of its stdout, N execs one after another, p50/p99/max;
//! - throughput: bytes a single exec's stdin carries through the client's copier to the agent and back out
//!   through its stdout copier, per second.
//!
//! Ignored by default: `cargo test -p hex-client --release --test guest_client_bench -- --ignored
//! --nocapture`, with `CLIENT_BENCH_EXECS` (default 2000) and `CLIENT_BENCH_MIB` (default 1024) the run's
//! sizes, recorded with each run in bench/native/guest-client/.

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

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use hex_api::{BoxId, Class, Exit, Program, Refusal, Runtime, Stdio};
use hex_client::guest::{Client, Link};
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Answer, Call, Header, Limits, Request};

struct Loopback {
    control: SocketAddr,
    stdio: SocketAddr,
}

impl Link for Loopback {
    fn dial(&self, port: u32) -> Result<hex_client::guest::DuplexOwner, Refusal> {
        let addr = if port == stream::CONTROL_PORT {
            self.control
        } else {
            self.stdio
        };
        let conn = TcpStream::connect(addr).unwrap();
        conn.set_nodelay(true).unwrap();
        hex_client::guest::own_duplex(conn)
    }
}

fn next_request(
    conn: &mut TcpStream,
    d: &mut Deframer,
    buf: &mut Vec<u8>,
) -> Option<(Header, Request)> {
    loop {
        if !buf.is_empty() {
            let (took, whole) = d.take(buf).unwrap();
            buf.drain(..took);
            if let Some(m) = whole {
                return Some(wire::read_request_apart(&m, wire::MAX_MESSAGE).unwrap());
            }
            continue;
        }
        let mut chunk = [0u8; 4096];
        let n = conn.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn send_answer(conn: &mut TcpStream, h: Header, answer: &Answer) {
    let bytes = wire::answer(h.id, answer, wire::MAX_MESSAGE).unwrap();
    conn.write_all(&stream::frame(&bytes, wire::MAX_MESSAGE).unwrap())
        .unwrap();
}

/// A stand-in node agent behind a relay: every exec is `cat`, its stdin connection copied to its stdout
/// connection.
fn agent(control: TcpListener, stdio: TcpListener) {
    let (mut conn, _) = control.accept().unwrap();
    conn.set_nodelay(true).unwrap();
    conn.write_all(&1u64.to_le_bytes()).unwrap();
    let (mut d, mut buf) = (Deframer::new(wire::MAX_MESSAGE), Vec::new());
    while let Some((h, r)) = next_request(&mut conn, &mut d, &mut buf) {
        match r {
            Request::Hello { .. } => send_answer(
                &mut conn,
                h,
                &Answer::Hello(wire::Hello {
                    build: wire::build(),
                    classes: vec![Class::Oci],
                    limits: Limits {
                        connections: 1,
                        message_bytes: wire::MAX_MESSAGE,
                        in_flight: 4,
                        programs: 4,
                    },
                    budgets: wire::Budgets {
                        in_flight: 4,
                        programs: 4,
                    },
                }),
            ),
            Request::Call(Call::Exec { .. }) => {
                let mut named = BTreeMap::new();
                for _ in 0..2 {
                    let (mut s, _) = stdio.accept().unwrap();
                    s.set_nodelay(true).unwrap();
                    let mut name = [0u8; stream::NAME_BYTES];
                    s.read_exact(&mut name).unwrap();
                    named.insert(stream::read_name(&name).unwrap().stream, s);
                }
                send_answer(&mut conn, h, &Answer::Started { pid: 1 });
                let mut stdin = named.remove(&stream::Stdio::In).unwrap();
                let mut stdout = named.remove(&stream::Stdio::Out).unwrap();
                std::io::copy(&mut stdin, &mut stdout).unwrap();
                drop(stdout);
                send_answer(&mut conn, h, &Answer::Exited(Exit::Code(0)));
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
fn guest_client_exec_start_and_throughput() {
    let control = TcpListener::bind("127.0.0.1:0").unwrap();
    let stdio = TcpListener::bind("127.0.0.1:0").unwrap();
    let link = hex_client::guest::own_link(Loopback {
        control: control.local_addr().unwrap(),
        stdio: stdio.local_addr().unwrap(),
    })
    .unwrap();
    std::thread::spawn(move || agent(control, stdio));
    let client = Client::connect(link).unwrap();
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

    let n = env("CLIENT_BENCH_EXECS", 2000);
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
        "CLIENT start n={n} p50={:?} p99={:?} max={:?}",
        at(0.5),
        at(0.99),
        starts[starts.len() - 1]
    );

    let mib = env("CLIENT_BENCH_MIB", 1024);
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
        "CLIENT throughput mib={mib} took={took:?} mib_per_s={:.1}",
        mib as f64 / took.as_secs_f64()
    );
}
