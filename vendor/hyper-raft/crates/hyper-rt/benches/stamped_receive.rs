//! A stamped UDP socket's receive, a datagram a call, the path a plane's heartbeats take on macOS:
//! `cargo bench -p hyper-rt --bench stamped_receive` (docs/benchmarks.md, "hyper-rt: a stamp in the
//! kernel's ticks, the receive's cost").
//!
//! One shard. Each round a plain socket on the shard's thread sends [`BURST`] datagrams of [`BYTES`] to a
//! `Batched` socket that asked for the kernel's receive stamps (`Batched::bind`, the plane socket's), and
//! the shard takes them; each receive call that took a datagram is timed, one that found the socket empty
//! awaits readiness untimed. Allocator calls (`hyper_measure::alloc`, the whole process) and the process's
//! page faults (`hyper_measure::faults`) are counted over the measured rounds and reported a datagram.
//! Rounds: Wilks' least sample for a one-sided 95% bound on the 99.9th percentile, `n = ⌈ln 0.05 / ln
//! 0.999⌉ = 2,995` [WILKS], after as many unmeasured.
//!
//! Then a stamp's age alone, worked out as the receive does now on this host's rates
//! (`machine::clock::MachTicks::age_ns`) and as it did before it read the kernel's (the process's timebase
//! on the difference), alternating, [`AGE_CALLS`] calls a turn on inputs the optimizer cannot see,
//! nanoseconds a call, the median of [`AGE_TURNS`] turns each.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::hint::black_box;
use std::net::UdpSocket;
use std::time::Instant;

use hyper_measure::alloc::{self, Counting};
use hyper_measure::faults;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::udp::{Batched, Io, Ipv4Addr, SocketAddr};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Shape: datagrams a round, sent before any is taken: a burst the socket's default receive buffer holds
/// whole (macOS's `net.inet.udp.recvspace`, 786,896 bytes here, past 64 datagrams' mbufs).
const BURST: usize = 64;
/// Shape: a datagram's payload, the order of a plane's heartbeat (48 bytes sealed in `e2e plane`).
const BYTES: usize = 64;
/// Shape: calls a turn of the age alone, enough that a turn's two clock reads are under a part in ten
/// thousand of it (two `Instant` reads of some 20 ns against a turn of milliseconds).
#[cfg(target_os = "macos")]
const AGE_CALLS: u64 = 1_000_000;
/// Shape: turns of the age alone, each way; odd, so the median is a turn's.
#[cfg(target_os = "macos")]
const AGE_TURNS: usize = 21;

/// Wilks' least sample for a one-sided bound on the `quantile` at `confidence`.
fn wilks(quantile: f64, confidence: f64) -> usize {
    ((1.0 - confidence).ln() / quantile.ln()).ceil() as usize
}

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 16,
        timers_per_shard: 16,
        interests_per_shard: 16,
        ring_entries: 16,
        step_budget_ns: 1_000_000_000,
        timer_tick_ns: 100_000,
        batch: 16,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// One round: [`BURST`] datagrams sent, then taken; the nanoseconds the receive calls that took them spent.
async fn round(socket: &mut Batched, out: &UdpSocket, to: SocketAddr, payload: &[u8]) -> u64 {
    for _ in 0..BURST {
        out.send_to(payload, to).unwrap();
    }
    let mut taken = 0usize;
    let mut spent = 0u64;
    while taken < BURST {
        let began = Instant::now();
        let got = socket
            .receive(|arrival, bytes| {
                black_box((arrival, bytes));
            })
            .unwrap();
        let took = began.elapsed().as_nanos() as u64;
        if got == 0 {
            socket.readable().await.unwrap();
            continue;
        }
        spent += took;
        taken += got;
    }
    spent
}

/// The receive's rounds: nanoseconds a datagram at the median, the 99th percentile and the maximum (the
/// 99.9th's bound), allocator calls and page faults a datagram.
fn receive(n: usize) -> String {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async move {
        let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        let mut socket = Batched::bind(loopback, Io { batch: 1 }, true).unwrap();
        assert_eq!(
            socket.stats().kernel_stamps,
            cfg!(any(target_os = "linux", target_os = "macos")),
            "the kernel stamps receives where it can"
        );
        let to = socket.local_addr().unwrap();
        let out = UdpSocket::bind(loopback).unwrap();
        let payload = [0x5au8; BYTES];
        for _ in 0..n {
            round(&mut socket, &out, to, &payload).await;
        }
        let mut per_datagram = Vec::with_capacity(n);
        let faults_before = faults::read().unwrap();
        alloc::begin_process();
        for _ in 0..n {
            per_datagram.push(round(&mut socket, &out, to, &payload).await / BURST as u64);
        }
        let calls = alloc::end_process().calls();
        let faulted = faults::read().unwrap().since(&faults_before).total();
        per_datagram.sort_unstable();
        let at = |q: f64| per_datagram[((n as f64 * q) as usize).min(n - 1)];
        let datagrams = (n * BURST) as f64;
        format!(
            "receive, ns a datagram: p50 {} p99 {} max {}; allocator calls a datagram {:.4}; page faults a \
             datagram {:.4} ({} datagrams)",
            at(0.5),
            at(0.99),
            per_datagram[n - 1],
            calls as f64 / datagrams,
            faulted as f64 / datagrams,
            n * BURST
        )
    })
    .unwrap()
}

/// Nanoseconds a call of `age` over [`AGE_CALLS`] calls, the inputs moving each call.
#[cfg(target_os = "macos")]
fn turn(age: impl Fn(u64, u64) -> u64) -> f64 {
    let base = black_box(28_155_671_796_901u64);
    let began = Instant::now();
    let mut sum = 0u64;
    for i in 0..AGE_CALLS {
        sum = sum.wrapping_add(age(black_box(base + i * 7), black_box(base + i * 5)));
    }
    black_box(sum);
    began.elapsed().as_nanos() as f64 / AGE_CALLS as f64
}

/// The age alone, now and before, alternating turns; the medians.
#[cfg(target_os = "macos")]
fn ages() -> String {
    let ticks = hyper_rt::machine::clock::mach_ticks().unwrap();
    let mut now = Vec::with_capacity(AGE_TURNS);
    let mut before = Vec::with_capacity(AGE_TURNS);
    for _ in 0..AGE_TURNS {
        now.push(turn(|at, stamp| black_box(ticks).age_ns(at, stamp)));
        // Before: the process's timebase on the difference, the stamp taken as the process's ticks.
        before.push(turn(|at, stamp| {
            black_box(ticks).ns(at.saturating_sub(stamp))
        }));
    }
    let median = |mut turns: Vec<f64>| {
        turns.sort_by(f64::total_cmp);
        turns[turns.len() / 2]
    };
    format!(
        "a stamp's age alone, ns a call: now {:.3}, before {:.3} ({AGE_TURNS} turns each)",
        median(now),
        median(before)
    )
}

fn main() {
    let n = wilks(0.999, 0.95);
    println!("{n} rounds of {BURST} datagrams of {BYTES} bytes");
    println!("{}", receive(n));
    #[cfg(target_os = "macos")]
    println!("{}", ages());
}
