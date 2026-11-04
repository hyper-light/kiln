//! The compute pool (src/compute.rs, kiln's departure, ORIGIN.md), in one process because the pool is one per
//! process: a job's result returns through a task's await; its threads run below the thread that started
//! them; a job that unwinds ends without a result and the pool lives on; at its bound a job is refused and
//! handed back; a job in pieces gives its thread to a job that waits, and goes on where none does; a second
//! start is refused; the pool stops, and refuses jobs after.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cognitive_complexity
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use hyper_rt::RtError;
use hyper_rt::compute::{self, Config, Step};
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::sync::OneshotReceiver;

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

/// A piece's time here: long enough that a piece is many units of the spinning job's work.
const PIECE: Duration = Duration::from_millis(2);
/// How long a check waits for what must come before it calls the pool stuck.
const PATIENCE: Duration = Duration::from_secs(20);

/// The receiver's value, polled from this plain thread until it comes or [`PATIENCE`] passes.
fn within<T>(receiver: &mut OneshotReceiver<T>) -> Option<T> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match receiver.try_recv() {
            Ok(Some(value)) => return Some(value),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            _ => return None,
        }
    }
}

/// Some CPU work no compiler removes.
fn churn(seed: u64) -> u64 {
    let mut x = seed | 1;
    for _ in 0..1_000 {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
    }
    x
}

#[test]
fn the_pool_runs_lowers_refuses_yields_and_stops() {
    let started = compute::start(Config {
        workers: 1,
        queued: 4,
        piece_ns: u64::try_from(PIECE.as_nanos()).unwrap(),
    })
    .unwrap();
    assert_eq!(started.workers, 1);
    assert!(
        matches!(
            compute::start(Config { workers: 1, queued: 1, piece_ns: 1 }),
            Err(RtError::BadConfig { .. })
        ),
        "one pool per process"
    );

    // A job's result arrives through a task's await.
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let (answer, here, there) = rt
        .block_on(async {
            let answer = compute::run(|| 6 * 7).unwrap().await.unwrap();
            // The pool's thread runs below the thread that asked (a shard's, here, at the process's priority).
            let there = compute::run(compute::thread_priority).unwrap().await.unwrap();
            (answer, compute::thread_priority(), there)
        })
        .unwrap();
    assert_eq!(answer, 42);
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        assert!(there.is_below(here), "the pool's thread at {there}, the shard's at {here}");
        assert_eq!(started.lowered, 1, "{:?}", started.priority);
        assert_eq!(started.priority, Ok(there));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let _ = (here, there);

    // A job that unwinds ends without a result; the pool lives on.
    let mut unwound = compute::run(|| -> u8 { panic!("the job's own failure") }).unwrap();
    let deadline = Instant::now() + PATIENCE;
    while matches!(unwound.try_recv(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(unwound.try_recv().is_err(), "the receiver sees the sender gone");
    assert_eq!(within(&mut compute::run(|| 1u8).unwrap()), Some(1));

    // At the bound (four admitted, one running and three waiting) a fifth is refused, and handed back.
    let (release, hold) = sync_channel::<()>(1);
    let mut held = compute::run(move || hold.recv().is_ok()).unwrap();
    let mut waiting: Vec<_> = (0..3u64).map(|i| compute::run(move || churn(i)).unwrap()).collect();
    let refused = compute::run(|| 5u8).unwrap_err();
    assert!(matches!(refused.error, RtError::Capacity { bound: 4, .. }), "{:?}", refused.error);
    release.send(()).unwrap();
    assert_eq!(within(&mut held), Some(true));
    for (i, receiver) in waiting.iter_mut().enumerate() {
        assert_eq!(within(receiver), Some(churn(u64::try_from(i).unwrap())));
    }
    assert_eq!(within(&mut compute::run(refused.job).unwrap()), Some(5), "the job handed back runs");

    // A job in pieces gives the one thread to a job that waits: a job that spins until told to stop, asking
    // its turn between units of work, and a short job submitted after it, whose result comes while the
    // long one still runs (one piece of it at most ahead). Without pieces the short job would wait forever.
    let stop = Arc::new(AtomicBool::new(false));
    let units = Arc::new(AtomicU64::new(0));
    let (spinning, spins) = sync_channel::<()>(1);
    let mut long = compute::run_pieces(
        (Arc::clone(&stop), Arc::clone(&units), Some(spinning)),
        |(stop, units, spinning), turn| {
            if let Some(spinning) = spinning.take() {
                spinning.send(()).unwrap();
            }
            loop {
                if stop.load(Ordering::Acquire) {
                    return Step::Done(units.load(Ordering::Relaxed));
                }
                churn(units.fetch_add(1, Ordering::Relaxed));
                if turn.over() {
                    return Step::More;
                }
            }
        },
    )
    .unwrap();
    spins.recv_timeout(PATIENCE).unwrap();
    let yielded = compute::stats().yielded;
    let asked = Instant::now();
    assert_eq!(within(&mut compute::run(|| 9u8).unwrap()), Some(9), "the waiting job ran beside the long one");
    let waited = asked.elapsed();
    assert!(compute::stats().yielded > yielded, "the long job gave its thread up");
    assert!(!stop.load(Ordering::Acquire));
    eprintln!("a job waiting behind one in pieces of {PIECE:?} waited {waited:?}");

    // Where nothing waits, the long job goes on without giving its thread up.
    let yielded = compute::stats().yielded;
    let before = units.load(Ordering::Relaxed);
    while units.load(Ordering::Relaxed) < before + 20_000 {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(compute::stats().yielded, yielded, "no piece ended with no job waiting");
    stop.store(true, Ordering::Release);
    assert!(within(&mut long).unwrap() > 0);

    let stats = compute::stats();
    assert_eq!(stats.panicked, 1);
    assert_eq!(stats.refused, 1);
    assert_eq!(stats.finished, stats.accepted, "{stats:?}");
    assert!(stats.longest_piece_ns > 0);

    // The pool stops: what was admitted ran; a job after is refused, handed back.
    compute::stop().unwrap();
    assert!(matches!(compute::stop(), Err(RtError::BadConfig { .. })));
    let after = compute::run(|| ()).unwrap_err();
    assert!(matches!(after.error, RtError::BadConfig { .. }));
}
