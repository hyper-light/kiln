//! A task's turn by time (kiln's goal 6, plan 01a1a59c2abe step 1; `hyper_rt::coop`): a task looping on
//! always-ready work no longer holds its shard past its share. Its leaves charge it, and past its share,
//! with something else wanting the shard, it yields at its next await, with no yield of its own written in.
//! Alone on its shard it goes on with no round trip, and a long computation yields through
//! `consume_budget`. A simulated shard keeps its schedules.
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
    clippy::indexing_slicing
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use hyper_rt::coop::consume_budget;
use hyper_rt::futures::{now_ns, sleep, spawn};
use hyper_rt::machine::clock::monotonic_ns;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};

/// Shape: the share under test, a fixed step budget (no wake tracking), long enough that a loaded host's
/// scheduling noise is a small part of it and short enough that a test runs many shares in well under a
/// second.
const QUANTUM_NS: u64 = 2_000_000;

/// Shape: one item of the hog's work, a twentieth of the share: many items fit a share, so a hog that did
/// not yield would run many shares in one poll.
const ITEM_NS: u64 = QUANTUM_NS / 20;

/// Shape: how long the hog loops, in shares: enough for its competitor's timer to fall due many times.
const SHARES: u64 = 40;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 16,
        timers_per_shard: 16,
        interests_per_shard: 64,
        ring_entries: 16,
        step_budget_ns: QUANTUM_NS,
        // Shape: the wheel's tick, a twentieth of the share, so a competitor's timer falls due within a share.
        timer_tick_ns: QUANTUM_NS / 20,
        batch: 16,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// Spins for `ns` on the CPU: an item of work with no await in it.
fn work(ns: u64) {
    let until = monotonic_ns() + ns;
    while monotonic_ns() < until {
        std::hint::spin_loop();
    }
}

/// A leaf of the caller's own that is always ready and charges nothing: what a loop over work no runtime
/// leaf sees looks like, for the control.
struct Uncharged;

impl Future for Uncharged {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
}

/// Shape: room for every run a competitor can make in a hog's loop, four a share at the most.
const RUNS_KEPT: usize = 4 * SHARES as usize;

/// What a competitor saw while the hog looped: how late it ran after its timer fell due, nanoseconds, each run
/// in a place of its own.
#[derive(Debug)]
struct Seen {
    runs: AtomicU64,
    late_ns: Vec<AtomicU64>,
}

impl Default for Seen {
    fn default() -> Self {
        Seen {
            runs: AtomicU64::new(0),
            late_ns: (0..RUNS_KEPT).map(|_| AtomicU64::new(0)).collect(),
        }
    }
}

impl Seen {
    /// How late the competitor ran: the median of its runs and the most, nanoseconds.
    fn lateness(&self) -> (u64, u64) {
        let runs = usize::try_from(self.runs.load(Ordering::Relaxed)).unwrap().min(RUNS_KEPT);
        let mut late: Vec<u64> = self.late_ns[..runs].iter().map(|l| l.load(Ordering::Relaxed)).collect();
        late.sort_unstable();
        (late.get(runs / 2).copied().unwrap_or(0), late.last().copied().unwrap_or(0))
    }
}

/// A competitor: wakes on a timer every quarter share while `hog` runs, noting how late it ran.
async fn compete(seen: Arc<Seen>, hog_done: Arc<AtomicBool>) {
    while !hog_done.load(Ordering::Acquire) {
        let due = now_ns() + QUANTUM_NS / 4;
        sleep(QUANTUM_NS / 4).await.unwrap();
        if hog_done.load(Ordering::Acquire) {
            break;
        }
        let run = usize::try_from(seen.runs.fetch_add(1, Ordering::Relaxed)).unwrap();
        if let Some(place) = seen.late_ns.get(run) {
            place.store(now_ns().saturating_sub(due), Ordering::Relaxed);
        }
    }
}

/// Runs a hog for `SHARES` shares beside a competitor; `charged` picks the hog's always-ready leaf (a timer
/// already due, which charges, or the caller's own, which does not). What the competitor saw, and the hog's
/// polls.
fn hog_beside_a_competitor(charged: bool) -> (Arc<Seen>, u64, hyper_rt::shard_loop::Counters) {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let seen = Arc::new(Seen::default());
    let hog_polls = Arc::new(AtomicU64::new(0));
    let (seen_in, polls_in) = (Arc::clone(&seen), Arc::clone(&hog_polls));
    rt.block_on(async move {
        let hog_done = Arc::new(AtomicBool::new(false));
        spawn(compete(Arc::clone(&seen_in), Arc::clone(&hog_done))).unwrap();
        // The competitor's first poll, which arms its timer.
        hyper_rt::futures::yield_now().await;
        let mut last_task_poll = None;
        let end = monotonic_ns() + SHARES * QUANTUM_NS;
        while monotonic_ns() < end {
            if charged {
                // Always ready: a timer already due.
                sleep(0).await.unwrap();
            } else {
                Uncharged.await;
            }
            // A new poll of this task shows as a new count of the shard's polls.
            let polls = hyper_rt::futures::shard_counters().unwrap().polls;
            if last_task_poll != Some(polls) {
                last_task_poll = Some(polls);
                polls_in.fetch_add(1, Ordering::Relaxed);
            }
            work(ITEM_NS);
        }
        hog_done.store(true, Ordering::Release);
    })
    .unwrap();
    let counters = rt.counters();
    (seen, hog_polls.load(Ordering::Relaxed), counters)
}

/// Step 1's acceptance: a task looping on always-ready work no longer holds its shard past its share. Do: a
/// hog loops for forty shares on a timer already due (a runtime leaf, so charged), an item of a twentieth of
/// a share each, beside a competitor whose timer falls due every quarter share. Expect: the hog yields at its
/// share, so it is polled about once a share and the competitor runs as often, at the median no later than a
/// share and an item past its timer (a loaded host's preemptions move the most, not the median); the shard
/// counts the yields, and no long poll of the hog's (one past twice its share on the CPU, which preemptions
/// are not). The control, the same hog on a leaf of its own that charges nothing: one poll, and the
/// competitor never runs while it loops.
#[test]
fn g6_s1_a_task_looping_on_always_ready_work_no_longer_holds_its_shard_past_its_share() {
    let (seen, hog_polls, counters) = hog_beside_a_competitor(true);
    let runs = seen.runs.load(Ordering::Relaxed);
    let (median_late, most_late) = seen.lateness();
    eprintln!("charged hog: {hog_polls} polls; competitor {runs} runs, late {median_late} ns at the median, {most_late} ns at most; {counters:?}");
    assert!(
        hog_polls >= SHARES / 2,
        "the hog yielded at its share: {hog_polls} polls in {SHARES} shares"
    );
    // The hog yields at each share's end, and the competitor, due every quarter share, runs then: about
    // once a share.
    assert!(
        runs >= SHARES / 2,
        "the competitor ran while the hog looped: {runs} runs in {SHARES} shares"
    );
    // The median, which a loaded host's preemptions of the test's thread do not move as they move the most.
    assert!(
        median_late <= QUANTUM_NS + ITEM_NS,
        "the competitor waited a share and an item past its timer at the most, at the median: {median_late} ns"
    );
    assert!(
        counters.coop_yields >= SHARES / 2,
        "the yields were the budget's: {counters:?}"
    );
    assert_eq!(
        counters.long_steps, 0,
        "no poll of the hog held the shard past twice its share on the CPU: {counters:?}"
    );

    let (seen, hog_polls, counters) = hog_beside_a_competitor(false);
    eprintln!("uncharged hog: {hog_polls} polls; competitor {} runs", seen.runs.load(Ordering::Relaxed));
    assert_eq!(
        hog_polls, 1,
        "uncharged, the hog held its shard for the whole loop"
    );
    assert_eq!(
        seen.runs.load(Ordering::Relaxed),
        0,
        "the competitor never ran while the uncharged hog looped"
    );
    assert_eq!(counters.coop_yields, 0);
}

/// Alone on its shard, a charge needs no time and no round trip: a thousand `consume_budget`s and a thousand
/// due timers are one poll, as `maybe_yield`'s are (docs/runtime.md §3.10).
#[test]
fn g6_s1_alone_on_its_shard_a_charged_task_is_polled_once() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let before = rt.counters().polls;
    rt.block_on(async {
        for _ in 0..1000 {
            consume_budget().await;
            sleep(0).await.unwrap();
        }
    })
    .unwrap();
    assert_eq!(rt.counters().polls - before, 1, "no want: no round trip");
    assert_eq!(rt.counters().coop_yields, 0);
}

/// A long computation yields through the one call: pieces of a twentieth of a share with `consume_budget`
/// between them let a competitor's due timer in within a share and a piece at the median, and the computation
/// goes on.
#[test]
fn g6_s1_a_long_computation_yields_through_consume_budget() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let seen = Arc::new(Seen::default());
    let seen_in = Arc::clone(&seen);
    let pieces = rt
        .block_on(async move {
            let done = Arc::new(AtomicBool::new(false));
            spawn(compete(Arc::clone(&seen_in), Arc::clone(&done))).unwrap();
            hyper_rt::futures::yield_now().await;
            let mut pieces = 0u64;
            let end = monotonic_ns() + SHARES * QUANTUM_NS;
            while monotonic_ns() < end {
                work(ITEM_NS);
                pieces += 1;
                consume_budget().await;
            }
            done.store(true, Ordering::Release);
            pieces
        })
        .unwrap();
    let runs = seen.runs.load(Ordering::Relaxed);
    let (median_late, most_late) = seen.lateness();
    eprintln!("computation: {pieces} pieces; competitor {runs} runs, late {median_late} ns at the median, {most_late} ns at most");
    assert!(pieces > SHARES, "the computation went on: {pieces} pieces");
    assert!(runs >= SHARES / 2, "the competitor ran: {runs} runs");
    assert!(
        median_late <= QUANTUM_NS + ITEM_NS,
        "within a share and a piece, at the median: {median_late} ns"
    );
}

/// A simulated shard keeps a step a yield (docs/runtime.md §11): its clock does not move within a poll, so a
/// charge never ends one, and a loop of due timers is one poll.
#[test]
fn g6_s1_a_simulated_shard_never_yields_by_budget() {
    let mut sim = hyper_rt::sim::SimRuntime::new(&config(), 7).unwrap();
    let shard = sim.shard_ids()[0];
    let looped = Arc::new(AtomicU64::new(0));
    let looped_in = Arc::clone(&looped);
    sim.spawn_on(shard, async move {
        for _ in 0..1000 {
            sleep(0).await.unwrap();
            consume_budget().await;
            looped_in.fetch_add(1, Ordering::Relaxed);
        }
    })
    .unwrap();
    sim.run_until_idle();
    assert_eq!(looped.load(Ordering::Relaxed), 1000);
    let counters = sim.counters(shard).unwrap();
    assert_eq!(counters.coop_yields, 0, "{counters:?}");
}
