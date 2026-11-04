//! The compute pool beside a shard (plan 01a1a59c2a8e step 1's test): CPU-heavy jobs on the pool leave a
//! shard's request loop as prompt as it is with the pool idle. The loop is a task that sleeps a millisecond
//! on the shard's timers and notes how late it woke, as a server's shard-health probe does; rounds with the
//! pool idle and rounds with every pool thread busy alternate, so drift on the host falls on both. The
//! loaded rounds' median p99 must stay within the calm rounds' noise: their worst p99, plus the timer
//! wheel's tick (a sleep ends up to a tick late by design, so the tick is the reading's resolution).
//!
//! On Linux the test's thread is pinned to the core it is on, so the process is granted that one core and the
//! pool is sized as a server sizes it, one thread less the shard and so one, which the pool's thread, started
//! from it, shares with the shard: the pool can only stay out of the shard's way by its rank, `Rank::Below`, and the shard's slice of its step (`compute::prefer_current_thread`, as
//! kiln's shards ask for); the host's other work falls on calm and loaded rounds alike. The rank below keeps
//! the pool's share of the CPU against other work in the cgroup: `compute_wait.rs` holds a job a request waits
//! on beside such work to what that share derives.
//!
//! On macOS (which keeps no affinity and has no slice a shard can ask for) the pool is sized as a server sizes
//! it and its lateness is reported, not held to: there no QoS class both keeps a shard at its calm lateness
//! beside a busy pool and keeps the pool's jobs from waiting on other work (`prioritysys.rs`'s measurements).
//!
//! The same load as plain threads at the process's own priority is measured beside it and reported: it is what
//! the pool's lower priority buys.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cognitive_complexity,
    clippy::cast_possible_truncation
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hyper_rt::compute::{self, Config, Grant, Rank, Step};
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};

/// The timer wheel's tick here, and so the lateness reading's resolution.
const TICK_NS: u64 = 100_000;
/// The request loop's period: a millisecond, a busy server's requests apart.
const PERIOD: Duration = Duration::from_millis(1);
/// Samples a round: a p99 of 250 is its third-largest.
const SAMPLES: usize = 250;
/// Rounds of each arm.
const ROUNDS: usize = 5;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 250_000,
        timer_tick_ns: TICK_NS,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// CPU work no compiler removes, a few microseconds of it.
fn churn(seed: u64) -> u64 {
    let mut x = seed | 1;
    for _ in 0..1_000 {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
    }
    x
}

/// One round of the request loop on the shard: the p99 of how late each sleep woke, in nanoseconds.
fn round(rt: &mut LocalRuntime) -> u64 {
    let mut late: Vec<u64> = rt
        .block_on(async {
            let mut late = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                let due = Instant::now() + PERIOD;
                hyper_rt::futures::sleep(PERIOD.as_nanos() as u64).await.unwrap();
                late.push(Instant::now().saturating_duration_since(due).as_nanos() as u64);
            }
            late
        })
        .unwrap();
    late.sort_unstable();
    late[late.len() * 99 / 100]
}

/// Load beside the shard while `body` runs: `jobs` CPU-bound jobs on the pool, or `threads` plain threads.
enum Load {
    Calm,
    Pool(usize),
    Threads(usize),
}

fn loaded(rt: &mut LocalRuntime, load: &Load) -> u64 {
    let stop = Arc::new(AtomicBool::new(false));
    let mut pool_jobs = Vec::new();
    let mut threads = Vec::new();
    match *load {
        Load::Calm => {}
        Load::Pool(jobs) => {
            for i in 0..jobs {
                let stop = Arc::clone(&stop);
                pool_jobs.push(
                    compute::run_pieces((stop, i as u64), |(stop, x), turn| {
                        while !stop.load(Ordering::Relaxed) {
                            *x = churn(*x);
                            if turn.over() {
                                return Step::More;
                            }
                        }
                        Step::Done(())
                    })
                    .unwrap(),
                );
            }
        }
        Load::Threads(count) => {
            for i in 0..count {
                let stop = Arc::clone(&stop);
                threads.push(std::thread::spawn(move || {
                    let mut x = i as u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = churn(x);
                    }
                }));
            }
        }
    }
    // Give the load a moment to take the cores before the round is measured.
    std::thread::sleep(Duration::from_millis(20));
    let p99 = round(rt);
    stop.store(true, Ordering::Relaxed);
    for mut job in pool_jobs {
        job.blocking_recv().unwrap();
    }
    for thread in threads {
        thread.join().unwrap();
    }
    p99
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// Where the load runs: on Linux one core shared with the shard, the pool sized for that grant; elsewhere the
/// process's grant, a pool thread for each CPU but the shard's. Said for the report.
fn placement() -> (usize, String) {
    #[cfg(target_os = "linux")]
    {
        let here = rustix::thread::sched_getcpu();
        let mut one = rustix::thread::CpuSet::new();
        one.set(here);
        rustix::thread::sched_setaffinity(None, &one).unwrap();
        let grant = Grant::of_process();
        (compute::workers_for(&grant, 1), format!("grant {grant} (CPU {here}), shared by the shard and the load"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let grant = Grant::of_process();
        (compute::workers_for(&grant, 1), format!("grant {grant}"))
    }
}

#[test]
fn cpu_heavy_jobs_on_the_pool_leave_a_shards_request_loop_as_prompt_as_calm() {
    let (workers, placed) = placement();
    let started = compute::start(Config {
        workers,
        queued: workers * 2,
        piece_ns: 250_000,
        rank: Rank::Below,
    })
    .unwrap();
    let mut rt = LocalRuntime::new(&config()).unwrap();
    // The shard (this thread) asks for a slice of its step, as kiln's shards do.
    #[cfg(target_os = "linux")]
    compute::prefer_current_thread(config().step_budget_ns).unwrap();
    let arms = [Load::Calm, Load::Pool(started.workers), Load::Threads(started.workers)];
    let mut p99s: Vec<Vec<u64>> = vec![Vec::new(); arms.len()];
    for round in 0..ROUNDS {
        let order: Vec<usize> = if round % 2 == 0 { vec![0, 1, 2] } else { vec![2, 1, 0] };
        for i in order {
            p99s[i].push(loaded(&mut rt, &arms[i]));
        }
    }
    let (calm, pool, threads) = (&p99s[0], &p99s[1], &p99s[2]);
    let noise = calm.iter().copied().max().unwrap() + TICK_NS;
    eprintln!(
        "{placed}; {} pool threads at {:?}, the shard at {}; p99 lateness ns, calm {calm:?}, pool {pool:?}, plain threads {threads:?}",
        started.workers,
        started.priority,
        compute::thread_priority()
    );
    #[cfg(target_os = "linux")]
    assert!(
        median(pool.clone()) <= noise,
        "the shard's p99 lateness with the pool busy, {pool:?} ns, past the calm rounds' noise, {calm:?} ns plus a tick"
    );
    #[cfg(not(target_os = "linux"))]
    eprintln!("the pool's median p99 {} ns, against the calm rounds' noise of {noise} ns: reported, not held to here", median(pool.clone()));
    compute::stop().unwrap();
}
