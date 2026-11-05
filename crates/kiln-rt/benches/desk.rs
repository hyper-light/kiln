//! What the desk's hot paths cost (kiln's goal 11, plan 01a1a87b2e56: a desk made as it is used must not slow
//! them): `cargo bench -p hyper-rt --bench desk`.
//!
//! - **spawn**: a task on the shard spawns `TASKS` detached tasks that end at their first poll, then awaits their
//!   end, a round at a time; nanoseconds a spawn (spawned, listed, polled, ended, its slot given back), the
//!   median of `ROUNDS` rounds. The first round takes fresh slots (made, on a desk made as it's used); the rest
//!   take slots given back.
//! - **yield**: a task yields `YIELDS` times, a round at a time; nanoseconds a yield (its wake listed apart, the
//!   loop's inputs read, its release onto the ready list, its next poll), the median of `ROUNDS` rounds.
//!
//! Two builds are compared by running each, interleaved, and reading the medians.
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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hyper_rt::runtime::{Runtime, RuntimeConfig};

/// Tasks a spawn round spawns: half the shard's bound, so the first round makes fresh slots.
const TASKS: u64 = 4_096;
/// Yields a yield round takes: enough that a round's clock reads are a small part of it.
const YIELDS: u64 = 100_000;
/// Rounds, whose median is reported.
const ROUNDS: usize = 31;

static ENDED: AtomicU64 = AtomicU64::new(0);

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 8_192,
        timers_per_shard: 16_384,
        interests_per_shard: 16_384,
        ring_entries: 1024,
        step_budget_ns: 250_000,
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

/// Runs `round` on the runtime's shard and hands back what it took.
fn on_shard(rt: &Runtime, round: impl std::future::Future<Output = Duration> + Send + 'static) -> Duration {
    let (tx, rx) = std::sync::mpsc::channel::<Duration>();
    let shard = rt.shard_ids()[0];
    rt.spawn_on(shard, async move {
        let _ = tx.send(round.await);
    })
    .unwrap();
    rx.recv_timeout(Duration::from_secs(60)).expect("a round ends")
}

fn spawns(rt: &Runtime) -> f64 {
    let before = ENDED.load(Ordering::Relaxed);
    let took = on_shard(rt, async move {
        let began = Instant::now();
        for _ in 0..TASKS {
            hyper_rt::futures::spawn_detached(async {
                ENDED.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
        }
        while ENDED.load(Ordering::Relaxed) < before + TASKS {
            hyper_rt::futures::yield_now().await;
        }
        began.elapsed()
    });
    took.as_nanos() as f64 / TASKS as f64
}

fn yields(rt: &Runtime) -> f64 {
    let took = on_shard(rt, async move {
        let began = Instant::now();
        for _ in 0..YIELDS {
            hyper_rt::futures::yield_now().await;
        }
        began.elapsed()
    });
    took.as_nanos() as f64 / YIELDS as f64
}

fn median(mut rounds: Vec<f64>) -> f64 {
    rounds.sort_by(f64::total_cmp);
    rounds[rounds.len() / 2]
}

fn main() {
    let rt = Runtime::start(&config()).unwrap();
    let first = spawns(&rt);
    let spawn = median((0..ROUNDS).map(|_| spawns(&rt)).collect());
    let yielded = median((0..ROUNDS).map(|_| yields(&rt)).collect());
    println!("spawn: {first:.1} ns a spawn the first round (fresh slots), {spawn:.1} ns the median of {ROUNDS} after (slots given back)");
    println!("yield: {yielded:.1} ns a yield, the median of {ROUNDS} rounds of {YIELDS}");
    rt.shutdown().unwrap();
}
