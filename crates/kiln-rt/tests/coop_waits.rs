//! A leaf is charged to its task's turn where it goes on, never where it waits (kiln's goal 6, plan 01a1a59c2abe
//! step 1; `hyper_rt::coop`). Each looks first, taking nothing: one that waits registers with no charge, one that
//! goes on is charged and then takes, so a refusal leaves what it would have taken. Charged as it looked, a leaf
//! that waited took its poll's first charge, which always goes on, so the leaf after it was refused at every poll
//! once the poll's own work outlasted the share, and its task was polled again and again for nothing (kiln's
//! front door pipe test spun a shard for hours so, 11-07).
// Test harness code: a panic here is a failed test (CLAUDE.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc
)]

use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use hyper_rt::coop::consume_budget;
use hyper_rt::futures::{sleep, spawn, yield_now};
use hyper_rt::machine::clock::monotonic_ns;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::sync::{Notify, Semaphore, channel, oneshot, watch};

/// Shape: the share under test, a fixed step budget (no wake tracking), as `coop_budget.rs` has it.
const QUANTUM_NS: u64 = 2_000_000;
/// Shape: the steps a test takes beside a leaf that waits, each in a poll of its own.
const STEPS: u32 = 8;
/// Shape: the polls past which a task counts as spun, four times its steps.
const SPUN: u32 = 4 * STEPS;

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 16,
        timers_per_shard: 16,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: QUANTUM_NS,
        // Shape: a twentieth of the share, so a timer falls due within one.
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

/// Spins for `ns` on the CPU: a poll's own work, past its share.
fn work(ns: u64) {
    let until = monotonic_ns() + ns;
    while monotonic_ns() < until {
        std::hint::spin_loop();
    }
}

/// Keeps the shard wanted, until `done`: a timer of its own falls due within every share.
fn compete() -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let until = Arc::clone(&done);
    spawn(async move {
        while !until.load(Ordering::Acquire) && sleep(QUANTUM_NS / 4).await.is_ok() {}
    })
    .unwrap();
    done
}

/// Steps beside `wait`, a leaf that waits, on a wanted shard: each step works twice its share, polls `wait`, then
/// charges once (`consume_budget`), in a poll of its own. The polls the steps took, and whether `wait` waited all
/// along.
async fn steps_beside<W: Future + Unpin>(wait: &mut W) -> (u32, bool) {
    let done = compete();
    yield_now().await;
    let (mut polls, mut waiting) = (0u32, true);
    for _ in 0..STEPS {
        let mut step = consume_budget();
        poll_fn(|cx| {
            polls += 1;
            if polls > SPUN {
                return Poll::Ready(());
            }
            work(2 * QUANTUM_NS);
            waiting &= Pin::new(&mut *wait).poll(cx).is_pending();
            Pin::new(&mut step).poll(cx)
        })
        .await;
        yield_now().await;
    }
    done.store(true, Ordering::Release);
    (polls, waiting)
}

/// `ready`, a leaf whose operation is ready, polled on a wanted shard after its poll's own work outlasted the share
/// and its first charge went on elsewhere, then in the next poll: whether it was refused there, and went on here.
async fn twice<R: Future + Unpin>(ready: &mut R) -> (bool, bool) {
    let done = compete();
    yield_now().await;
    let mut first = consume_budget();
    let refused = poll_fn(|cx| {
        work(2 * QUANTUM_NS);
        assert!(Pin::new(&mut first).poll(cx).is_ready(), "a poll's first charge goes on");
        Poll::Ready(Pin::new(&mut *ready).poll(cx).is_pending())
    })
    .await;
    yield_now().await;
    let went_on = poll_fn(|cx| Poll::Ready(Pin::new(&mut *ready).poll(cx).is_ready())).await;
    done.store(true, Ordering::Release);
    (refused, went_on)
}

/// A leaf that waits, built and stepped beside by `steps`, is no charge: each step one poll, the leaf waiting.
fn waits_uncharged<F: Future<Output = (u32, bool)> + 'static>(what: &str, steps: impl FnOnce() -> F) {
    let (polls, waiting) = LocalRuntime::new(&config()).unwrap().block_on(steps()).unwrap();
    assert!(waiting, "{what}: nothing came, yet it went on");
    assert_eq!(polls, STEPS, "{what}: the waiting leaf took each poll's first charge, its steps redone poll after poll");
}

/// A leaf that goes on, built and polled by `polls`, is charged: refused past its share, its operation left for the
/// next poll.
fn goes_on_charged<F: Future<Output = (bool, bool)> + 'static>(what: &str, polls: impl FnOnce() -> F) {
    let (refused, went_on) = LocalRuntime::new(&config()).unwrap().block_on(polls()).unwrap();
    assert!(refused, "{what}: going on past its share beside other work, it wasn't charged");
    assert!(went_on, "{what}: refused, it lost what it would have taken");
}

/// Do: a notification waited for beside a step's charge, then one that came, polled past the share. Expect: the
/// wait no charge (each step one poll); the one that came charged, refused, then taken.
#[test]
fn g6_s1_a_notification_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a notification", || async {
        let notify = Notify::new().unwrap();
        let mut wait = notify.notified();
        steps_beside(&mut wait).await
    });
    goes_on_charged("a notification", || async {
        let notify = Notify::new().unwrap();
        notify.notify_one();
        let mut ready = notify.notified();
        twice(&mut ready).await
    });
}

/// Do: a one-shot waited for, then one sent. Expect: as for a notification.
#[test]
fn g6_s1_a_one_shot_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a one-shot", || async {
        let (_sender, mut wait) = oneshot::<u32>().unwrap();
        steps_beside(&mut wait).await
    });
    goes_on_charged("a one-shot", || async {
        let (sender, mut ready) = oneshot::<u32>().unwrap();
        sender.send(7).unwrap();
        twice(&mut ready).await
    });
}

/// Do: a channel's receive waited for, then one with a value to take. Expect: as for a notification.
#[test]
fn g6_s1_a_channel_receive_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a channel's receive", || async {
        let (_sender, mut receiver) = channel::<u32>(1).unwrap();
        let mut wait = receiver.recv();
        steps_beside(&mut wait).await
    });
    goes_on_charged("a channel's receive", || async {
        let (sender, mut receiver) = channel::<u32>(1).unwrap();
        sender.try_send(7).unwrap();
        let mut ready = receiver.recv();
        twice(&mut ready).await
    });
}

/// Do: a channel's send waiting for room (its receiver takes nothing), then one with room. Expect: as for a
/// notification.
#[test]
fn g6_s1_a_channel_send_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a channel's send", || async {
        let (sender, _receiver) = channel::<u32>(1).unwrap();
        sender.try_send(0).unwrap();
        let mut wait = sender.send(1);
        steps_beside(&mut wait).await
    });
    goes_on_charged("a channel's send", || async {
        let (sender, _receiver) = channel::<u32>(1).unwrap();
        let mut ready = sender.send(7);
        twice(&mut ready).await
    });
}

/// Do: a watched word's change waited for, then one changed. Expect: as for a notification.
#[test]
fn g6_s1_a_watched_change_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a watched word", || async {
        let (_sender, mut receiver) = watch(0, 1).unwrap();
        let mut wait = receiver.changed();
        steps_beside(&mut wait).await
    });
    goes_on_charged("a watched word", || async {
        let (mut sender, mut receiver) = watch(0, 1).unwrap();
        sender.send(7);
        let mut ready = receiver.changed();
        twice(&mut ready).await
    });
}

/// Do: a semaphore's permit waited for (none free), then one free. Expect: as for a notification.
#[test]
fn g6_s1_a_permit_is_charged_as_it_goes_on_never_as_it_waits() {
    waits_uncharged("a permit", || async {
        let semaphore = Semaphore::new(0, 1).unwrap();
        let mut wait = semaphore.acquire();
        steps_beside(&mut wait).await
    });
    goes_on_charged("a permit", || async {
        let semaphore = Semaphore::new(1, 1).unwrap();
        let mut ready = semaphore.acquire();
        twice(&mut ready).await
    });
}
