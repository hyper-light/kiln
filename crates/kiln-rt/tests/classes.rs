//! Classes of tasks on a shard (kiln's goal 6, plan 01a1a59c2abe step 3; `hyper_rt::coop::Class`): each class
//! with tasks waiting gets an equal share of the shard's time, and the most urgent goes first. A Critical task
//! woken while Normal and Background tasks keep the shard busy runs next, not behind them; neither of those is
//! starved; and a shard whose tasks never take a class runs as it did. The shard counts each class's waits.
// Test harness code: a panic here is a failed test (CLAUDE.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::cognitive_complexity
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hyper_rt::coop::{Class, set_class};
use hyper_rt::futures::{now_ns, sleep, spawn, yield_now};
use hyper_rt::machine::clock::monotonic_ns;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};

/// Shape: a fixed quantum, long against a loaded host's noise and short enough for many in a test.
const QUANTUM_NS: u64 = 1_000_000;

/// Shape: busy tasks of each of the two lower classes: a round of them is eight quanta, far past one.
const BUSY: usize = 4;

/// Shape: how long the shard is kept busy, in quanta.
const QUANTA: u64 = 60;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 32,
        timers_per_shard: 32,
        interests_per_shard: 64,
        ring_entries: 32,
        step_budget_ns: QUANTUM_NS,
        timer_tick_ns: QUANTUM_NS / 20,
        batch: 32,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// A task of `class` that works a quantum at a time until `done`, yielding between, counting its quanta.
async fn busy(class: Class, done: Arc<AtomicBool>, worked: Arc<AtomicU64>) {
    set_class(class).unwrap();
    while !done.load(Ordering::Acquire) {
        let until = monotonic_ns() + QUANTUM_NS;
        while monotonic_ns() < until {
            std::hint::spin_loop();
        }
        worked.fetch_add(1, Ordering::Relaxed);
        yield_now().await;
    }
}

/// Runs a sleeper of `class` beside busy Normal and Background tasks: how late it ran after each timer, sorted,
/// and the quanta the Normal and Background tasks worked.
fn sleeper_beside_busy_work(class: Class) -> (Vec<u64>, u64, u64, hyper_rt::shard_loop::Counters) {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let (normal, background) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (done_in, normal_in, background_in) = (Arc::clone(&done), Arc::clone(&normal), Arc::clone(&background));
    let mut late = rt
        .block_on(async move {
            for _ in 0..BUSY {
                spawn(busy(Class::Normal, Arc::clone(&done_in), Arc::clone(&normal_in))).unwrap();
                spawn(busy(Class::Background, Arc::clone(&done_in), Arc::clone(&background_in))).unwrap();
            }
            set_class(class).unwrap();
            let mut late = Vec::new();
            let end = monotonic_ns() + QUANTA * QUANTUM_NS;
            while monotonic_ns() < end {
                let due = now_ns() + QUANTUM_NS / 4;
                sleep(QUANTUM_NS / 4).await.unwrap();
                late.push(now_ns().saturating_sub(due));
            }
            done_in.store(true, Ordering::Release);
            late
        })
        .unwrap();
    late.sort_unstable();
    let counters = rt.counters();
    (late, normal.load(Ordering::Relaxed), background.load(Ordering::Relaxed), counters)
}

/// Step 3: a Critical task goes ahead of the Normal and Background work that keeps its shard busy. Do: four Normal
/// and four Background tasks each work a quantum at a time; a sleeper wakes every quarter quantum. Expect: as
/// Critical, it runs within about the quantum then running at the median (a loaded host's preemptions move the
/// most, not the median); as Normal, it waits its turn behind the round of busy tasks of its share, several
/// quanta. Both lower classes keep working (neither starved), and the shard counts the Critical waits.
#[test]
fn g6_s3_a_critical_task_goes_ahead_of_busy_normal_and_background_work() {
    let (late, normal, background, counters) = sleeper_beside_busy_work(Class::Critical);
    let median = late[late.len() / 2];
    eprintln!(
        "critical sleeper: {} wakes, late {median} ns at the median, {} at most; normal worked {normal} quanta, background {background}; {counters:?}",
        late.len(),
        late[late.len() - 1]
    );
    assert!(
        median <= 2 * QUANTUM_NS,
        "a Critical task runs within the poll running as it woke: {median} ns at the median"
    );
    assert!(normal > 0 && background > 0, "neither lower class starved: {normal}, {background}");
    assert!(counters.queue_polls_critical > 0, "{counters:?}");

    let (late, _, _, _) = sleeper_beside_busy_work(Class::Normal);
    let normal_median = late[late.len() / 2];
    eprintln!("normal sleeper: late {normal_median} ns at the median");
    assert!(
        normal_median > median,
        "as Normal it waits behind its class's busy tasks: {normal_median} ns against {median} ns as Critical"
    );
}

/// Each class with work gets an equal share: Normal and Background tasks busy side by side work about alike, where
/// a Background task alone would have worked the whole time. Equal within a factor of two either way, against a
/// loaded host's noise.
#[test]
fn g6_s3_busy_classes_share_the_shard_alike() {
    let (_, normal, background, _) = sleeper_beside_busy_work(Class::Normal);
    eprintln!("normal worked {normal} quanta, background {background}");
    assert!(
        background * 2 >= normal && normal * 2 >= background,
        "equal shares: normal {normal}, background {background}"
    );
}

/// A shard whose tasks never take a class keeps one list, in arrival order: a round of yields goes in the order the
/// tasks were spawned, as before classes.
#[test]
fn g6_s3_without_classes_the_order_is_arrival() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let seen: Arc<Vec<AtomicU64>> = Arc::new((0..12).map(|_| AtomicU64::new(u64::MAX)).collect());
    let next = Arc::new(AtomicU64::new(0));
    let (seen_in, next_in) = (Arc::clone(&seen), Arc::clone(&next));
    rt.block_on(async move {
        for task in 0..4u64 {
            let (seen, next) = (Arc::clone(&seen_in), Arc::clone(&next_in));
            spawn(async move {
                for _ in 0..3 {
                    let at = next.fetch_add(1, Ordering::Relaxed) as usize;
                    seen[at].store(task, Ordering::Relaxed);
                    yield_now().await;
                }
            })
            .unwrap();
        }
        for _ in 0..8 {
            yield_now().await;
        }
    })
    .unwrap();
    let order: Vec<u64> = seen.iter().map(|task| task.load(Ordering::Relaxed)).collect();
    assert_eq!(order, vec![0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3]);
    assert_eq!(rt.counters().queue_polls_critical, 0);
    assert_eq!(rt.counters().queue_polls_background, 0);
}

/// A poll is charged to its class at most the watchdog's limit (`batch` quanta, `hyper_rt::watch`): past it the poll
/// is a stall, reported as one, and charged in full it would leave its class owing all of it, its other tasks
/// waiting as long again while the other classes caught up. Do: Background tasks keep the shard busy; a Normal task
/// holds it in one poll for ten limits, then sleeps a quarter quantum. Expect: it runs again within two limits of
/// its timer (a limit of catch-up, where the whole wedge would be ten).
#[test]
fn g6_s3_a_stall_is_charged_to_its_class_at_most_the_watchdogs_limit() {
    let limit_ns = QUANTUM_NS * config().batch as u64;
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let background = Arc::new(AtomicU64::new(0));
    let (done_in, background_in) = (Arc::clone(&done), Arc::clone(&background));
    let late = rt
        .block_on(async move {
            for _ in 0..BUSY {
                spawn(busy(Class::Background, Arc::clone(&done_in), Arc::clone(&background_in))).unwrap();
            }
            // Their first polls, which take their class.
            yield_now().await;
            let until = monotonic_ns() + 10 * limit_ns;
            while monotonic_ns() < until {
                std::hint::spin_loop();
            }
            let due = now_ns() + QUANTUM_NS / 4;
            sleep(QUANTUM_NS / 4).await.unwrap();
            let late = now_ns().saturating_sub(due);
            done_in.store(true, Ordering::Release);
            late
        })
        .unwrap();
    eprintln!("after a wedge of ten limits: late {late} ns; background worked {} quanta", background.load(Ordering::Relaxed));
    assert!(late <= 2 * limit_ns, "late {late} ns against a limit of {limit_ns} ns");
}
