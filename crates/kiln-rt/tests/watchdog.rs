//! The watchdog (kiln's goal 6, plan 01a1a59c2abe step 2; `hyper_rt::watch`): a shard held in one poll past its
//! limit (a step's worth of polls that use their shares, `batch` × the quantum) is reported with its task, where
//! that task was spawned and whose the time was; placement is told the shard is held; the movable spawns that
//! wait for it run on another shard; and once the poll ends the mark lifts. Reports are capped at Seastar's 5 a
//! minute a shard, the rest counted.
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
    clippy::cognitive_complexity
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use hyper_rt::machine::clock::monotonic_ns;
use hyper_rt::runtime::{Runtime, RuntimeConfig, submit_movable};
use hyper_rt::watch::{self, Whose};

/// Shape: a fixed quantum (no wake tracking) and a batch of four: a limit of four milliseconds, so a wedge of a
/// few hundred milliseconds is far past it on any host.
const QUANTUM_NS: u64 = 1_000_000;
const BATCH: usize = 4;
const LIMIT_NS: u64 = QUANTUM_NS * BATCH as u64;

/// Shape: how long a test waits for what the watchdog does, far past a look (half the limit) on a loaded host.
const WAIT: Duration = Duration::from_secs(10);

fn config(shards: u16) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 128,
        ring_entries: 64,
        step_budget_ns: QUANTUM_NS,
        timer_tick_ns: QUANTUM_NS / 10,
        batch: BATCH,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// Spins on the CPU until `release` is set: a poll with no await, holding its shard.
fn hold_until(release: &AtomicBool) {
    while !release.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
}

/// Waits until `test` holds, failing past [`WAIT`].
fn wait_for(what: &str, mut test: impl FnMut() -> bool) {
    let until = Instant::now() + WAIT;
    while !test() {
        assert!(Instant::now() < until, "waited {WAIT:?} for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Step 2's acceptance: a test that wedges a shard gets the stall reported with its task, and the shard's waiting
/// work runs elsewhere. Do: wedge shard A with a task that spins without awaiting; once the watchdog marks A held,
/// submit movable spawns to A. Expect: A is marked held and B is not; the report names the wedging task, its spawn
/// site in this file and line, a hold of at least the limit and, where the platform tells, that the task ran on
/// the CPU; the spawns run on B while A is still wedged; once A's poll ends, the mark lifts and the episode is
/// counted with its time and the spawns moved.
#[test]
fn g6_s2_a_wedged_shard_is_reported_with_its_task_and_its_waiting_work_runs_elsewhere() {
    let rt = Runtime::start(&config(2)).unwrap();
    let (a, b) = (rt.shard_ids()[0], rt.shard_ids()[1]);
    let (holder_a, holder_b) = (rt.holder_of(a).unwrap(), rt.holder_of(b).unwrap());
    let release = Arc::new(AtomicBool::new(false));
    let (started, wedge_task) = channel();
    let release_in = Arc::clone(&release);
    let wedging = async move {
        started
            .send(hyper_rt::futures::current_task().unwrap())
            .unwrap();
        hold_until(&release_in);
    };
    #[rustfmt::skip]
    let (line, spawned) = (line!(), rt.spawn_on(a, wedging));
    spawned.unwrap();
    let wedge = wedge_task.recv_timeout(WAIT).unwrap();
    let wedged_at = monotonic_ns();
    wait_for("the watchdog to mark A held", || watch::stalled(holder_a));
    assert!(
        monotonic_ns() - wedged_at >= LIMIT_NS / 2,
        "not marked before the limit could pass"
    );
    assert!(!watch::stalled(holder_b), "B is not held");

    let report = watch::report(holder_a).expect("a report of A's stall");
    assert_eq!(report.number, 1);
    assert_eq!(report.task, wedge, "the report names the wedging task");
    let at = report.spawned_at.expect("the task's spawn site");
    assert!(at.file().ends_with("watchdog.rs"), "{at}");
    assert_eq!(at.line(), line, "the spawn's own line: {at}");
    assert!(report.held_ns >= LIMIT_NS / 2, "{report:?}");
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        // A spin runs on the CPU: the task's, unless the host took the CPU away for most of the window.
        assert!(
            matches!(report.whose, Whose::Task | Whose::Host),
            "{report:?}"
        );
    }

    // Movable work submitted to the held shard runs on the other while A is still wedged.
    let (ran, ran_on) = channel();
    for _ in 0..8 {
        let ran = ran.clone();
        submit_movable(holder_a, async move {
            ran.send(hyper_rt::futures::shard_id().unwrap()).unwrap();
        })
        .unwrap();
    }
    for _ in 0..8 {
        assert_eq!(
            ran_on.recv_timeout(WAIT).unwrap(),
            b,
            "waiting work ran on the shard not held"
        );
    }
    assert!(watch::stalled(holder_a), "A is still wedged");
    let stalls = watch::stalls(holder_a).unwrap();
    assert!(stalls.moved >= 8, "{stalls:?}");

    release.store(true, Ordering::Release);
    wait_for("A's mark to lift", || !watch::stalled(holder_a));
    let stalls = watch::stalls(holder_a).unwrap();
    assert_eq!(stalls.episodes, 1, "{stalls:?}");
    assert!(stalls.held_ns >= LIMIT_NS / 2, "{stalls:?}");
    assert_eq!(watch::stalls(holder_b).unwrap().episodes, 0);

    // Once A lets go, movable work submitted to it runs there.
    submit_movable(holder_a, async move {
        ran.send(hyper_rt::futures::shard_id().unwrap()).unwrap();
    })
    .unwrap();
    assert_eq!(ran_on.recv_timeout(WAIT).unwrap(), a);
    rt.shutdown().unwrap();
}

/// A poll that waits inside a call (a blocking sleep, off the CPU and not runnable) is the task's, blocked, where
/// the platform tells a call's wait from a preemption (Linux's `/proc`, macOS's run state).
#[test]
fn g6_s2_a_poll_blocked_in_a_call_is_reported_as_the_tasks() {
    let rt = Runtime::start(&config(1)).unwrap();
    let a = rt.shard_ids()[0];
    let holder = rt.holder_of(a).unwrap();
    rt.spawn_on(a, async move {
        // Far past the limit, in the kernel.
        std::thread::sleep(Duration::from_millis(400));
    })
    .unwrap();
    wait_for("the watchdog to mark the shard held", || {
        watch::stalled(holder)
    });
    let report = watch::report(holder).unwrap();
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        assert_eq!(report.whose, Whose::Blocked, "{report:?}");
        assert!(report.whose.is_tasks());
    }
    wait_for("the mark to lift", || !watch::stalled(holder));
    rt.shutdown().unwrap();
}

/// Reports are capped as Seastar caps its stall detector's, at 5 a minute a shard; every episode past the cap is
/// counted, not reported. Do: wedge one shard seven times, each past the limit, with a poll between. Expect:
/// seven episodes, five reports, two suppressed.
#[test]
fn g6_s2_reports_are_capped_at_five_a_minute_and_the_rest_counted() {
    let rt = Runtime::start(&config(1)).unwrap();
    let a = rt.shard_ids()[0];
    let holder = rt.holder_of(a).unwrap();
    for episode in 1..=7u64 {
        let release = Arc::new(AtomicBool::new(false));
        let release_in = Arc::clone(&release);
        rt.spawn_on(a, async move { hold_until(&release_in) })
            .unwrap();
        wait_for("the wedge's mark", || watch::stalled(holder));
        release.store(true, Ordering::Release);
        wait_for("the mark to lift", || !watch::stalled(holder));
        let stalls = watch::stalls(holder).unwrap();
        assert_eq!(stalls.episodes, episode, "{stalls:?}");
    }
    let stalls = watch::stalls(holder).unwrap();
    assert_eq!(stalls.reports, watch::REPORTS_PER_MINUTE, "{stalls:?}");
    assert_eq!(stalls.suppressed, 7 - watch::REPORTS_PER_MINUTE, "{stalls:?}");
    assert_eq!(
        watch::report(holder).unwrap().number,
        watch::REPORTS_PER_MINUTE
    );
    rt.shutdown().unwrap();
}

/// A shard that never holds a poll past its limit is never marked: polls of a quantum each, yielding between,
/// for many limits.
#[test]
fn g6_s2_a_shard_whose_polls_keep_to_their_shares_is_never_marked() {
    let rt = Runtime::start(&config(1)).unwrap();
    let a = rt.shard_ids()[0];
    let holder = rt.holder_of(a).unwrap();
    let (done, finished) = channel();
    rt.spawn_on(a, async move {
        let end = monotonic_ns() + 50 * LIMIT_NS;
        while monotonic_ns() < end {
            let piece = monotonic_ns() + QUANTUM_NS;
            while monotonic_ns() < piece {
                std::hint::spin_loop();
            }
            hyper_rt::futures::yield_now().await;
        }
        done.send(()).unwrap();
    })
    .unwrap();
    finished.recv_timeout(WAIT).unwrap();
    let stalls = watch::stalls(holder).unwrap();
    // A loaded host may hold the shard's thread off the CPU past a limit; such an episode is the host's.
    assert!(
        stalls.episodes == 0
            || watch::report(holder).is_some_and(|report| report.whose == Whose::Host),
        "{stalls:?}"
    );
    rt.shutdown().unwrap();
}
