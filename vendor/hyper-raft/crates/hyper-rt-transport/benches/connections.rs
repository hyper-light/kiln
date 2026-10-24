//! What an exchange costs as the server's connections grow: one asker times small exchanges, one at
//! a time, while `n - 1` other askers hold their connections to the same server open and idle, for
//! `n` doubling from 1 to `WIDE`, every endpoint on one hyper-rt shard over loopback. Each row is the
//! timed exchanges' wall time and allocations per exchange after a warm-up, the shard's one thread
//! doing everything every endpoint does. The idle connections have nothing to send and no timer due
//! but their keep-alive, so an endpoint that visits only what an event touched keeps the row flat; one
//! whose every turn visits every connection, for its transmits, its timers or its exchanges'
//! deadlines, makes the row grow with `n` (focal's comparison leader, 2026-10-11: the endpoint's
//! polls 8% of its one CPU at 1,000 requests a second over about twenty connections).
//! `cargo bench -p hyper-rt-transport --bench connections` (docs/benchmarks.md, "An exchange by the
//! server's connections").

#![allow(
    clippy::unwrap_in_result,
    clippy::type_complexity,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};

use common::*;
use hyper_measure::alloc;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt_transport::{Driver, Io};
use hyper_transport::{AdmissionLimits, Config, Endpoint, Event, Fixed, LEAST_PROGRESS, Limits};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// The most connections a row holds: a server of five hundred agents.
const WIDE: usize = 512;
/// Exchanges timed per row, after the warm-up.
const ROUNDS: u64 = 1_000;
const WARM: u64 = 100;
/// The timed exchange's body each way: half the least datagram (RFC 9000 §14), so request and
/// reply each go in one datagram and the row times the endpoint's turns, not segmentation.
const BODY: u64 = LEAST_PROGRESS / 2;
/// The period exchanges are judged at: far past anything loopback needs.
const PERIOD: Duration = Duration::from_secs(2);
/// The failure guard of a wait.
const GUARD: Duration = Duration::from_secs(120);
/// Datagrams a system call carries: one QUIC initial window (RFC 9002 §7.2).
const IO: Io = Io { batch: 10 };
/// The server's peer id; the askers are 1 to `n`.
const SERVER: u64 = 0;

type Drv = Driver<Mantle, Fixed, Book>;

/// Connections held per row: doubling from 1, then `WIDE`; or the one row `CONNECTIONS` names, for
/// a profile of it.
fn counts() -> Vec<usize> {
    if let Some(n) = std::env::var("CONNECTIONS")
        .ok()
        .and_then(|n| n.parse().ok())
    {
        return vec![n];
    }
    let mut counts: Vec<usize> = std::iter::successors(Some(1usize), |n| Some(n * 2))
        .take_while(|n| *n < WIDE)
        .collect();
    counts.push(WIDE);
    counts
}

/// A shard with room for `n` askers' tasks, sockets and timers beside the server's.
fn runtime(n: usize) -> RuntimeConfig {
    let room = 4 * n + 64;
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: room,
        timers_per_shard: room,
        interests_per_shard: room,
        ring_entries: 256,
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

/// The harness's limits, admitting `n` identities a connection each. No keep-alive, and an idle
/// bound past any row: a connection with nothing to do sends nothing, so a row waits for every
/// connection to settle (each path's warm-up spent) and then times its exchanges alone.
fn admitting(n: usize) -> Limits {
    let base = limits();
    Limits {
        admission: AdmissionLimits {
            pending: n + 8,
            identities: n + 8,
            connections: 2 * n + 8,
            per_identity: 2,
        },
        keep_alive: None,
        idle_timeout: Duration::from_secs(600),
        ..base
    }
}
/// The server's connections and the datagrams it received, as its task last read them: what the
/// timed asker waits on. Statics, not shared cells: one row runs at a time, each from zero, every
/// task of it on the shard's one thread.
static CONNECTED: AtomicUsize = AtomicUsize::new(0);
static RECEIVED: AtomicU64 = AtomicU64::new(0);
/// How long the server must hear nothing for the row's connections to count as settled.
const QUIET: Duration = Duration::from_millis(200);

fn endpoint(
    root: &[u8],
    own: &(Vec<u8>, Vec<u8>),
    peers: &[(&[u8], u64)],
    listen: bool,
    limits: Limits,
) -> Endpoint<Mantle, Fixed, Book> {
    let mut book = Book::default();
    for (certificate, peer) in peers {
        book.add(certificate, *peer, Role::Node);
    }
    let config = Config {
        tls: hyper_transport::tls::Tls::mutual(&credentials(root, &own.0, &own.1), listen).unwrap(),
        role: Role::Node,
        limits,
        congestion: Default::default(),
    };
    Endpoint::new(config, Fixed::new(512 << 20, 64), book, Instant::now()).unwrap()
}

/// The driver's next event, or a millisecond, whichever first.
async fn next(driver: &mut Drv) -> Option<Event<Mantle>> {
    let mut event = pin!(driver.event());
    let mut tick = pin!(hyper_rt::futures::sleep(1_000_000));
    poll_fn(|context| {
        if let Poll::Ready(event) = event.as_mut().poll(context) {
            return Poll::Ready(Some(event.unwrap()));
        }
        if tick.as_mut().poll(context).is_ready() {
            return Poll::Ready(None);
        }
        Poll::Pending
    })
    .await
}

/// One row: `n` connections held, the first asker timed. Wall time and allocations per exchange.
fn row(pki: &Pki, server: &(Vec<u8>, Vec<u8>), askers: &[(Vec<u8>, Vec<u8>)]) -> (f64, f64) {
    let n = askers.len();
    let mut rt = LocalRuntime::new(&runtime(n)).unwrap();
    let (root, server, askers) = (pki.root.clone(), server.clone(), askers.to_vec());
    rt.block_on(async move {
        let (root, server) = (root.as_slice(), &server);
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let peers: Vec<(&[u8], u64)> = askers
            .iter()
            .enumerate()
            .map(|(index, asker)| (asker.0.as_slice(), index as u64 + 1))
            .collect();
        let mut server_driver: Drv = Driver::bind(
            endpoint(root, server, &peers, true, admitting(n)),
            loopback,
            IO,
        )
        .unwrap();
        let server_addr = server_driver.local_addr().unwrap();
        CONNECTED.store(0, Ordering::Relaxed);
        RECEIVED.store(0, Ordering::Relaxed);
        hyper_rt::futures::spawn_detached(async move {
            let mut serving = Server::new();
            loop {
                let Ok(event) = server_driver.event().await else {
                    return;
                };
                serving.on_event(server_driver.endpoint(), event);
                while let Some(event) = server_driver.endpoint().poll_event() {
                    serving.on_event(server_driver.endpoint(), event);
                }
                serving.advance_all(server_driver.endpoint());
                CONNECTED.store(
                    server_driver.endpoint().stats().connections,
                    Ordering::Relaxed,
                );
                RECEIVED.store(server_driver.stats().received, Ordering::Relaxed);
            }
        })
        .unwrap();
        let to_server = [(server.0.as_slice(), SERVER)];
        // The idle askers: each connects and then only answers what its connection asks of it.
        for asker in &askers[1..] {
            let mut driver: Drv = Driver::bind(
                endpoint(root, asker, &to_server, false, admitting(1)),
                loopback,
                IO,
            )
            .unwrap();
            let now = driver.clock().unwrap();
            driver.endpoint().connect(now, SERVER, server_addr).unwrap();
            hyper_rt::futures::spawn_detached(async move {
                loop {
                    if driver.event().await.is_err() {
                        return;
                    }
                    while driver.endpoint().poll_event().is_some() {}
                }
            })
            .unwrap();
        }
        // The timed asker.
        let mut driver: Drv = Driver::bind(
            endpoint(root, &askers[0], &to_server, false, admitting(1)),
            loopback,
            IO,
        )
        .unwrap();
        let mut asker = Asker::new();
        let began = Instant::now();
        let now = driver.clock().unwrap();
        driver.endpoint().connect(now, SERVER, server_addr).unwrap();
        // Every connection held before anything is timed: the fact the row needs, waited on.
        while asker.connected.is_empty() || CONNECTED.load(Ordering::Relaxed) < n {
            if let Some(event) = next(&mut driver).await {
                asker.clock = driver.clock().unwrap();
                asker.on_event(driver.endpoint(), event);
                while let Some(event) = driver.endpoint().poll_event() {
                    asker.on_event(driver.endpoint(), event);
                }
            }
            asker.advance_all(driver.endpoint());
            assert!(
                began.elapsed() < GUARD,
                "{} of {n} connected",
                CONNECTED.load(Ordering::Relaxed)
            );
        }
        // Settled: the server heard nothing for a while, every path's warm-up spent.
        loop {
            let before = RECEIVED.load(Ordering::Relaxed);
            hyper_rt::futures::sleep(u64::try_from(QUIET.as_nanos()).unwrap())
                .await
                .unwrap();
            if RECEIVED.load(Ordering::Relaxed) == before {
                break;
            }
            assert!(began.elapsed() < GUARD, "{n} connections never settled");
        }
        let mut timed = (Duration::ZERO, 0u64);
        alloc::begin();
        let rounds = std::env::var("ROUNDS")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(ROUNDS);
        for round in 0..WARM + rounds {
            let started = Instant::now();
            let allocations = alloc::read().calls();
            let now = driver.clock().unwrap();
            let index = asker
                .ask(
                    driver.endpoint(),
                    now,
                    SERVER,
                    (Kind::Get, Class::Request),
                    (round % 251) as u8,
                    Some(BODY),
                    PERIOD,
                )
                .unwrap();
            asker.advance_all(driver.endpoint());
            while !asker.asked[index].done {
                if let Some(event) = next(&mut driver).await {
                    asker.clock = driver.clock().unwrap();
                    asker.on_event(driver.endpoint(), event);
                    while let Some(event) = driver.endpoint().poll_event() {
                        asker.on_event(driver.endpoint(), event);
                    }
                }
                asker.advance_all(driver.endpoint());
                assert!(began.elapsed() < GUARD, "round {round} never answered");
            }
            let asked = &asker.asked[index];
            assert_eq!(asked.refused, None, "{asked:?}");
            assert_eq!((asked.reply, asked.read), (Some(BODY), BODY), "{asked:?}");
            if round >= WARM {
                timed.0 += started.elapsed();
                timed.1 += alloc::read().calls() - allocations;
            }
        }
        (
            timed.0.as_nanos() as f64 / rounds as f64 / 1_000.0,
            timed.1 as f64 / rounds as f64,
        )
    })
    .unwrap()
}

fn main() {
    let pki = Pki::new();
    let server = pki.issue(&name(SERVER));
    let askers: Vec<(Vec<u8>, Vec<u8>)> = (1..=WIDE as u64)
        .map(|peer| pki.issue(&name(peer)))
        .collect();
    println!("| connections | µs an exchange | allocations an exchange |");
    println!("|---|---|---|");
    for n in counts() {
        let (micros, allocations) = row(&pki, &server, &askers[..n]);
        println!("| {n} | {micros:.1} | {allocations:.1} |");
    }
}
