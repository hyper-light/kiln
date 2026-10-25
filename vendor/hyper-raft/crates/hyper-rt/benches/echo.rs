//! A plain thread's round trip to a task on a shard and back: `cargo bench -p hyper-rt --bench echo`
//! (docs/benchmarks.md, "hyper-rt: a thread's round trip to a task").
//!
//! One shard, calibrated (`RuntimeConfig::from_calibration`), runs a task that answers each value it
//! receives on a `hyper_rt::sync` channel through the reply sender that came with it. A plain thread sends
//! a value and waits for the answer: the request and response path of a consumer whose clients are threads
//! and whose servers are tasks, mantle's ranges among them. Two ways of waiting:
//!
//! - **spinning**: the task marks client activity per request (`note_activity`), so the idle shard spins
//!   out the calibrated window, and the client spins on its receiver for the same window before it blocks.
//!   Both sides are active; this is the row a busy server sees.
//! - **parked**: no activity is marked and the client blocks at once, so each round trip is two kernel
//!   wakes.
//!
//! The floor is two plain threads spinning on std channels, the same exchange with no runtime.
//!
//! Each row takes Wilks' least sample for a one-sided 95% bound on the 99.9th percentile, `n =
//! ⌈ln 0.05 / ln 0.999⌉ = 2,995` round trips [WILKS], after as many unmeasured; its maximum is that bound.
//! Allocator calls are counted across the process (`hyper_measure::alloc::begin_process`) over the
//! measured round trips of each row and reported per round trip.
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

use std::time::{Duration, Instant};

use hyper_measure::alloc::{self, Counting};
use hyper_rt::machine::calibration::{Calibration, Policy};
use hyper_rt::registry;
use hyper_rt::runtime::{Runtime, RuntimeConfig};
use hyper_rt::sync::{Sender, channel};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Wilks' least sample for a one-sided bound on the `quantile` at `confidence`: the sample's maximum
/// exceeds the quantile with that confidence once `1 − quantile^n ≥ confidence`.
fn wilks(quantile: f64, confidence: f64) -> usize {
    ((1.0 - confidence).ln() / quantile.ln()).ceil() as usize
}

struct Row {
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    allocs_per_trip: f64,
}

fn summarize(mut lat: Vec<u64>, allocs: u64) -> Row {
    let n = lat.len();
    lat.sort_unstable();
    let at = |q: f64| lat[((n as f64 * q) as usize).min(n - 1)];
    Row {
        p50_ns: at(0.5),
        p99_ns: at(0.99),
        max_ns: lat[n - 1],
        allocs_per_trip: allocs as f64 / n as f64,
    }
}

fn print(name: &str, row: &Row) {
    println!(
        "{name:<10} p50 {:>8.2} µs  p99 {:>8.2} µs  max (p99.9 bound) {:>8.2} µs  allocs/trip {:.2}",
        row.p50_ns as f64 / 1e3,
        row.p99_ns as f64 / 1e3,
        row.max_ns as f64 / 1e3,
        row.allocs_per_trip
    );
}

/// The floor: two threads spinning on std channels.
fn std_floor(n: usize) -> Row {
    let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
    let (rtx, rrx) = std::sync::mpsc::sync_channel::<u64>(1);
    let server = std::thread::spawn(move || {
        loop {
            match rx.try_recv() {
                Ok(v) => rtx.send(v).unwrap(),
                Err(std::sync::mpsc::TryRecvError::Empty) => std::hint::spin_loop(),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
    });
    let trip = |i: u64| {
        let started = Instant::now();
        tx.send(i).unwrap();
        loop {
            if rrx.try_recv().is_ok() {
                break;
            }
            std::hint::spin_loop();
        }
        started.elapsed().as_nanos() as u64
    };
    for i in 0..n as u64 {
        trip(i);
    }
    let mut lat = Vec::with_capacity(n);
    alloc::begin_process();
    for i in 0..n as u64 {
        lat.push(trip(i));
    }
    let allocs = alloc::end_process().calls();
    drop(tx);
    server.join().unwrap();
    summarize(lat, allocs)
}

/// The thread-to-task exchange; `spinning` marks activity and spins the client for `window`.
fn echo(config: &RuntimeConfig, spinning: bool, n: usize) -> Row {
    let runtime = Runtime::start(config).unwrap();
    let shard = runtime.shard_ids()[0];
    let (tx, mut rx) = channel::<(u64, Sender<u64>)>(1).unwrap();
    runtime
        .spawn_on(shard, async move {
            while let Ok((v, reply)) = rx.recv().await {
                if spinning {
                    registry::with_current(|ctx| ctx.note_activity());
                }
                let _ = reply.try_send(v);
            }
        })
        .unwrap();
    let (rtx, mut rrx) = channel::<u64>(1).unwrap();
    let window = if spinning { config.spin_ns } else { 0 };
    let mut trip = |i: u64| {
        let started = Instant::now();
        tx.try_send((i, rtx.clone())).unwrap();
        loop {
            if rrx.try_recv().unwrap().is_some() {
                break;
            }
            if started.elapsed().as_nanos() as u64 >= window {
                rrx.blocking_recv().unwrap();
                break;
            }
            std::hint::spin_loop();
        }
        started.elapsed().as_nanos() as u64
    };
    for i in 0..n as u64 {
        trip(i);
    }
    let mut lat = Vec::with_capacity(n);
    alloc::begin_process();
    for i in 0..n as u64 {
        lat.push(trip(i));
    }
    let allocs = alloc::end_process().calls();
    drop(tx);
    let counters = runtime.shutdown().unwrap();
    println!("           {:?}", counters[0]);
    summarize(lat, allocs)
}

fn main() {
    let n = wilks(0.999, 0.95);
    let calibration = Calibration::measure(Duration::from_millis(40), 1).unwrap();
    let constants = calibration
        .constants(&Policy {
            reserved_cores: 1,
            lateness_tolerance_ns: None,
            latency_objective_ns: None,
        })
        .unwrap();
    let mut config = RuntimeConfig::from_calibration(&calibration, &constants, 4, 4);
    config.shards = 1;
    config.pin = false;
    config.cores.clear();
    println!(
        "{n} round trips a row; calibrated spin window {} ns, step quantum {} ns",
        config.spin_ns, config.step_budget_ns
    );
    print("std floor", &std_floor(n));
    let spinning = echo(&config, true, n);
    print("spinning", &spinning);
    let parked = echo(&config, false, n);
    print("parked", &parked);
}
