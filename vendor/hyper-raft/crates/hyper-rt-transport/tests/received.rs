//! `Driver::poll_received` hands out a turn's events before the turn's sends, which its caller then
//! makes (`Driver::flush`); `Driver::poll_event` sends first, as before. Do: a client connects to a
//! server polled with the one, then with the other, the caller flushing after each event it takes,
//! and sends it frames. Expect: the server's turn that ends the handshake hands out its `Connected`
//! from `poll_received` with the server's next flight (the handshake's end, its acknowledgement of the
//! client's) still to send, which the flush sends; from `poll_event`, with that flight already sent,
//! the flush finding nothing. Either way every frame arrives.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    dead_code,
    missing_docs
)]

#[path = "../../hyper-transport/tests/common/mod.rs"]
mod common;

use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::pin::pin;
use std::task::Poll;
use std::time::{Duration, Instant};

use common::*;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt_transport::{Driver, Io};
use hyper_transport::{Config, Endpoint, Event, Fixed, Refusal};

type Drv = Driver<Mantle, Fixed, Book>;

/// Datagrams a system call carries: one QUIC initial window (RFC 9002 §7.2).
const IO: Io = Io { batch: 10 };
/// The failure guard of a run, far past anything loopback needs.
const GUARD: Duration = Duration::from_secs(60);
/// The frames the client sends once connected, a thousand bytes each.
const FRAMES: u32 = 256;
/// The server is node 2 in the client's directory, the client node 1 in the server's.
const SERVER: u64 = 2;
const CLIENT: u64 = 1;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 1_000_000_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

fn endpoint(
    pki: &Pki,
    own: &(Vec<u8>, Vec<u8>),
    other: &[u8],
    peer: u64,
    listen: bool,
) -> Endpoint<Mantle, Fixed, Book> {
    let mut book = Book::default();
    book.add(other, peer, Role::Node);
    let config = Config {
        tls: hyper_transport::tls::Tls::mutual(&credentials(&pki.root, &own.0, &own.1), listen)
            .unwrap(),
        role: Role::Node,
        limits: limits(),
        congestion: Default::default(),
    };
    Endpoint::new(config, Fixed::new(512 << 20, 64), book, Instant::now()).unwrap()
}

/// The datagrams `driver` has sent, by its listener and its peers' sockets.
fn sent(driver: &Drv) -> u64 {
    driver.stats().sent + driver.peer_stats().sent
}

/// What a run saw: the datagrams the flush after the server's `Connected` sent, which its turn had
/// left to send, and the frames that arrived.
struct Seen {
    owed: u64,
    frames: u32,
}

/// A client connects to a server polled with `poll_received` (`received`) or `poll_event`, the
/// server's caller flushing after each event it takes, and sends it [`FRAMES`] frames.
fn run(received: bool) -> Seen {
    let pki = Pki::new();
    let one = pki.issue(&name(1));
    let two = pki.issue(&name(2));
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async move {
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut server: Drv =
            Driver::bind(endpoint(&pki, &two, &one.0, CLIENT, true), loopback, IO).unwrap();
        let server_addr = server.local_addr().unwrap();
        let mut client: Drv =
            Driver::bind(endpoint(&pki, &one, &two.0, SERVER, false), loopback, IO).unwrap();
        let now = client.clock().unwrap();
        client.endpoint().connect(now, SERVER, server_addr).unwrap();
        let began = Instant::now();
        let frame = [5u8; 1_000];
        let (mut connected, mut sent_frames, mut steps) = (false, 0, 0u64);
        let mut seen = Seen {
            owed: u64::MAX,
            frames: 0,
        };
        while seen.owed == u64::MAX || seen.frames < FRAMES {
            assert!(
                began.elapsed() < GUARD,
                "{} of {FRAMES} frames arrived, connected: {connected}",
                seen.frames
            );
            while connected && sent_frames < FRAMES {
                match client
                    .endpoint()
                    .send_frame(SERVER, 0, Kind::Append, &frame)
                {
                    Ok(()) => sent_frames += 1,
                    Err(Refusal::LaneFull) => break,
                    Err(other) => panic!("{other:?}"),
                }
            }
            client.flush().unwrap();
            // The driver asked first alternates, so neither side's events starve the other's
            // (peer_sockets.rs, `step`).
            let mut tick = pin!(hyper_rt::futures::sleep(1_000_000));
            let server_first = steps.is_multiple_of(2);
            steps += 1;
            let next = poll_fn(|context| {
                for side in if server_first {
                    [SERVER, CLIENT]
                } else {
                    [CLIENT, SERVER]
                } {
                    let polled = match (side, received) {
                        (SERVER, true) => server.poll_received(context),
                        (SERVER, false) => server.poll_event(context),
                        _ => client.poll_event(context),
                    };
                    if let Poll::Ready(event) = polled {
                        return Poll::Ready(Some((side, event.unwrap())));
                    }
                }
                if tick.as_mut().poll(context).is_ready() {
                    return Poll::Ready(None);
                }
                Poll::Pending
            })
            .await;
            let Some((side, first)) = next else {
                continue;
            };
            if side == CLIENT {
                let mut next = Some(first);
                while let Some(event) = next {
                    match event {
                        Event::Connected { .. } => connected = true,
                        Event::Frame { frame, .. } => client.endpoint().release(frame),
                        _ => {}
                    }
                    next = client.endpoint().poll_event();
                }
                continue;
            }
            // What the server had sent as its driver handed the turn's first event out, and after the
            // flush that makes the turn's sends; its events are taken first, as the method asks.
            let before = sent(&server);
            let mut ends_handshake = false;
            let mut next = Some(first);
            while let Some(event) = next {
                match event {
                    Event::Connected { .. } => ends_handshake = true,
                    Event::Frame { frame, .. } => {
                        seen.frames += 1;
                        server.endpoint().release(frame);
                    }
                    _ => {}
                }
                next = server.endpoint().poll_event();
            }
            server.flush().unwrap();
            if ends_handshake {
                seen.owed = sent(&server) - before;
            }
        }
        seen
    })
    .unwrap()
}

#[test]
fn a_turns_event_comes_out_of_poll_received_before_the_turns_sends() {
    let received = run(true);
    assert!(
        received.owed > 0,
        "the turn that ended the handshake had sent its next flight when poll_received handed out its event"
    );
    assert_eq!(received.frames, FRAMES);
    let event = run(false);
    assert_eq!(
        event.owed, 0,
        "the turn that ended the handshake left {} datagrams to send when poll_event handed out its event",
        event.owed
    );
    assert_eq!(event.frames, FRAMES);
}
