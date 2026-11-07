//! The quiet sleep (kiln's goal 9, plan 01a1a87b2cec step 5: a shard's delay probe that wakes a parked shard only
//! after a whole period with no wake at all): its deadline and its account of how its turn came, then on a running
//! runtime's shard, on the OS's driver: an idle shard is woken for it once its quiet is out, by its timer; a shard that
//! work wakes within its quiet is never woken for it; a shard running when it falls due ends it at its next step.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::{By, Came, Quiet, Wake, came, deadline, sleep_quiet, to_poll};
use crate::RtError;
use crate::machine::clock::shard_clock_ns;
use crate::runtime::{Runtime, RuntimeConfig};
use crate::shard::ShardId;
use crate::shard_loop::Counters;

/// Shape: a quiet sleep's span and quiet in the runtime's tests: long against a park's start and the host's wake
/// (microseconds).
const SPAN: Duration = Duration::from_millis(200);

/// Shape: a span long against a step and short against [`SPAN`].
const GAP: Duration = Duration::from_millis(2);

/// Shape: how long the test waits on a shard's answer (a step is microseconds; a sleep is [`SPAN`]).
const ANSWER: Duration = Duration::from_secs(10);

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 16,
        timers_per_shard: 4,
        interests_per_shard: 4,
        ring_entries: 16,
        step_budget_ns: 1_000_000,
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

fn ns(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).unwrap()
}

/// The shard's counters, asked of it by a task on it.
fn counters_on(rt: &Runtime, shard: ShardId) -> Counters {
    let (tx, rx) = channel();
    rt.spawn_on(shard, async move {
        let _ = tx.send(crate::registry::with_current(|ctx| ctx.counters()).unwrap_or_default());
    })
    .unwrap();
    rx.recv_timeout(ANSWER).unwrap()
}

/// Waits `span` on the test's thread: a channel nobody sends on is the wait.
fn pause(span: Duration) {
    let (_hold, rx) = channel::<()>();
    let started = Instant::now();
    while started.elapsed() < span {
        let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
    }
}

/// A quiet sleep of [`SPAN`] spawned on `shard`, sending how its turn came and how long it took on `tx`.
fn sleeper(rt: &Runtime, shard: ShardId, tx: Sender<(Result<Came, RtError>, Duration)>) {
    rt.spawn_on(shard, async move {
        let started = Instant::now();
        let came = sleep_quiet(ns(SPAN), ns(SPAN)).await;
        let _ = tx.send((came, started.elapsed()));
    })
    .unwrap();
}

/// A future pending until `open` is set (and then setting it back), handing its waker over each time it waits, with
/// when the quiet sleep waiting on its shard falls due ([`waiting_due`]).
struct Gate {
    open: Arc<AtomicBool>,
    handoffs: Sender<(Waker, Option<u64>)>,
}

impl Future for Gate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.open.swap(false, Ordering::SeqCst) {
            return Poll::Ready(());
        }
        self.handoffs.send((cx.waker().clone(), waiting_due())).unwrap();
        Poll::Pending
    }
}

/// When the quiet sleep waiting on this thread's shard falls due, on the clock a kick is stamped on
/// ([`shard_clock_ns`]); none while none waits (not yet polled, or its turn come).
fn waiting_due() -> Option<u64> {
    crate::registry::with_current(|ctx| match (ctx.quiet.get(), &ctx.clock) {
        (Some(quiet), crate::driver::Clock::Since(epoch)) if quiet.came.is_none() => Some(quiet.due_ns.saturating_add(*epoch)),
        _ => None,
    })
    .flatten()
}

/// Polls its future once, then is done, whatever that poll said: the future is dropped with it.
struct PollOnce<F>(Pin<Box<F>>);

impl<F: Future> Future for PollOnce<F> {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        Poll::Ready(self.0.as_mut().poll(cx).is_ready())
    }
}

/// Holds the shard's thread in its poll for `span`, as a long poll would.
fn hold(span: Duration) {
    let started = Instant::now();
    while started.elapsed() < span {
        std::hint::spin_loop();
    }
}

/// Do: a sleep falling due at 100 with a quiet of 50, against parks beginning before it fell due and after. Expect: a
/// park beginning at 10 ends at 100, its due time (its quiet ran out before); one at 80 at 130, its quiet after it
/// began; one at 200, after it fell due, at 250.
#[test]
fn g9_s5_a_park_ends_for_a_quiet_sleep_no_earlier_than_its_due_time_nor_its_quiet() {
    let quiet = Quiet { due_ns: 100, quiet_ns: 50, word: 0, came: None, site: None };
    assert_eq!(deadline(&quiet, 10), 100);
    assert_eq!(deadline(&quiet, 80), 130);
    assert_eq!(deadline(&quiet, 200), 250);
    assert_eq!(deadline(&Quiet { due_ns: u64::MAX, ..quiet }, 10), u64::MAX, "saturates");
}

/// Do: the account of a sleep falling due at 100, from the shard's last wake, then taken by a poll at 130 (160 for its
/// own deadline's). Expect: no wake since it fell due, running, from its due time; a wake after by a kick or another
/// timer, woken, that wake's time and the rest to the poll; by a socket or a signal, woken, unknown; by its own deadline,
/// quiet, its lateness and the rest.
#[test]
fn g9_s5_a_quiet_sleep_says_how_its_turn_came() {
    let quiet = Quiet { due_ns: 100, quiet_ns: 50, word: 0, came: None, site: None };
    let wake = |at_ns, by, waited_ns| Some(Wake { at_ns, by, waited_ns });
    let taken = |last, now_ns| {
        let (came, from_ns) = came(&quiet, last);
        to_poll(came, from_ns, now_ns)
    };
    assert_eq!(taken(None, 130), Came::Running(30), "no wake yet");
    assert_eq!(taken(wake(90, By::Kick, Some(5)), 130), Came::Running(30), "its last wake before it fell due");
    assert_eq!(taken(wake(120, By::Kick, Some(7)), 130), Came::Woken(Some(17)));
    assert_eq!(taken(wake(120, By::Timer, Some(3)), 130), Came::Woken(Some(13)));
    assert_eq!(taken(wake(120, By::Io, None), 130), Came::Woken(None));
    assert_eq!(taken(wake(120, By::Signal, None), 130), Came::Woken(None), "a signal's return is no deadline's");
    assert_eq!(taken(wake(151, By::Quiet, Some(1)), 160), Came::Quiet(10));
    assert_eq!(taken(wake(100, By::Quiet, Some(0)), 100), Came::Quiet(0), "a wake as it falls due, taken at once");
    assert_eq!(to_poll(Came::Running(5), 200, 100), Came::Running(5), "a poll that reads an earlier clock adds nothing");
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5: an idle shard is woken for a quiet sleep once its quiet is out, by a timer,
/// and says so. Do: on a running runtime's idle shard, a quiet sleep of [`SPAN`], nothing else on the shard. Expect: it
/// comes after its span, `Came::Quiet`, and the shard took a timer's wake for it.
#[test]
fn g9_s5_an_idle_shard_is_woken_for_a_quiet_sleep_after_its_quiet() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let before = counters_on(&rt, shard);
    let (tx, rx) = channel();
    sleeper(&rt, shard, tx);
    let (came, took) = rx.recv_timeout(ANSWER).unwrap();
    let after = counters_on(&rt, shard);
    assert!(matches!(came, Ok(Came::Quiet(_))), "{came:?}");
    assert!(took >= SPAN, "it came after {took:?}, its span {SPAN:?}");
    assert!(after.woken_by_timer > before.woken_by_timer, "no timer woke the shard: {before:?} to {after:?}");
    rt.shutdown().unwrap();
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5: a shard that work wakes within a quiet sleep's quiet is never woken for it.
/// Do: on a running runtime's shard, a quiet sleep of [`SPAN`] whose quiet is the test's own patience ([`ANSWER`]), so
/// only a test held past it could let a park run out; and a task the test's thread wakes from off the shard at each of
/// its hand-offs: at once while no sleep waits, and once one does, as soon as it has fallen due, so the park that
/// spans its due time is one the shard's work ends. Expect: the sleep's turn came with the shard's work (`Came::Woken`,
/// its account no longer than from the kick that ended the park to its answer, on the clock the kick is stamped on;
/// or `Came::Running` where the host held the shard from its hand-off until the sleep fell due), never by its own
/// deadline: no timer wake counted against where it was asked for (a signal's return, as a pacer's stop and continue
/// makes, counts with no site).
#[test]
fn g9_s5_a_shard_work_wakes_within_its_quiet_takes_no_timer_wake() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let holder = rt.holder_of(shard).unwrap();
    let before = counters_on(&rt, shard);
    let (tx, rx) = channel();
    rt.spawn_on(shard, async move {
        let started = Instant::now();
        let site = line!() + 1;
        let came = sleep_quiet(ns(SPAN), ns(ANSWER)).await;
        let _ = tx.send((came, started.elapsed(), site));
    })
    .unwrap();
    let (handoffs_tx, handoffs) = channel();
    let open = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    rt.spawn_on(shard, {
        let (open, stop) = (Arc::clone(&open), Arc::clone(&stop));
        async move {
            while !stop.load(Ordering::SeqCst) {
                Gate { open: Arc::clone(&open), handoffs: handoffs_tx.clone() }.await;
            }
        }
    })
    .unwrap();
    // The stamp of the last kick sent while the sleep waited: the one that ended the park spanning its due time, since
    // the step after that wake gives the sleep its turn before the gate hands over again.
    let mut kicked = None;
    let ((came, took, site), answered, waker) = loop {
        let (waker, due) = handoffs.recv_timeout(ANSWER).unwrap();
        if let Ok(answer) = rx.try_recv() {
            break (answer, shard_clock_ns(), waker);
        }
        if let Some(due) = due {
            while shard_clock_ns() < due {
                pause(Duration::from_nanos(due.saturating_sub(shard_clock_ns())));
            }
            kicked = Some(shard_clock_ns());
        }
        open.store(true, Ordering::SeqCst);
        waker.wake();
    };
    let after = counters_on(&rt, shard);
    stop.store(true, Ordering::SeqCst);
    open.store(true, Ordering::SeqCst);
    waker.wake();
    assert!(matches!(came, Ok(Came::Woken(_) | Came::Running(_))), "{came:?}");
    if let (Ok(Came::Woken(Some(waited))), Some(kicked)) = (came, kicked) {
        let since = answered.saturating_sub(kicked);
        assert!(waited <= since, "its wake took {waited} ns by its account, past the {since} ns from the kick to its answer");
    }
    assert!(took >= SPAN, "it came after {took:?}, its span {SPAN:?}");
    let wakes = crate::timer_sites::timer_wakes(holder);
    let for_it: u64 = wakes.iter().filter(|w| w.site.is_some_and(|s| s.file() == file!() && s.line() == site)).map(|w| w.count).sum();
    assert_eq!(for_it, 0, "its own deadline woke the shard: {wakes:?}");
    assert!(after.woken_by_kick > before.woken_by_kick, "{before:?} to {after:?}");
    rt.shutdown().unwrap();
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5: a quiet sleep falling due while its shard runs ends at the shard's next step,
/// as a timer would, saying how late. Do: on a running runtime's shard, a quiet sleep of [`SPAN`], then a task that holds
/// the shard twice that in one poll. Expect: `Came::Running`, as late as the hold ran past its due time, give or take.
#[test]
fn g9_s5_a_quiet_sleep_due_on_a_running_shard_ends_at_its_next_step() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let (tx, rx) = channel();
    sleeper(&rt, shard, tx);
    let (held_tx, held) = channel();
    rt.spawn_on(shard, async move {
        hold(SPAN * 2);
        let _ = held_tx.send(());
    })
    .unwrap();
    held.recv_timeout(ANSWER).unwrap();
    let (came, took) = rx.recv_timeout(ANSWER).unwrap();
    let Ok(Came::Running(late)) = came else {
        panic!("{came:?}");
    };
    assert!(late > 0 && late < ns(SPAN * 2), "{late} ns late");
    assert!(took >= SPAN * 2 - GAP, "it came after {took:?}, before the hold ended");
    rt.shutdown().unwrap();
}

/// Do: two quiet sleeps on one shard at once; then a sleep polled once and dropped before it came, and another after it.
/// Expect: the second of the two refused; the one after the dropped one taken (the desk freed).
#[test]
fn g9_s5_one_quiet_sleep_waits_on_a_shard_at_a_time() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let (tx, rx) = channel();
    rt.spawn_on(shard, async move {
        let mut first = Box::pin(sleep_quiet(ns(SPAN), ns(SPAN)));
        let waiting = PollOnce(Box::pin(&mut first)).await;
        let second = sleep_quiet(ns(GAP), ns(GAP)).await;
        drop(first);
        let after = sleep_quiet(ns(GAP), ns(GAP)).await;
        let _ = tx.send((waiting, second, after));
    })
    .unwrap();
    let (waiting, second, after) = rx.recv_timeout(ANSWER).unwrap();
    assert!(!waiting, "the first came at once");
    assert!(matches!(second, Err(RtError::BadConfig { .. })), "{second:?}");
    assert!(after.is_ok(), "{after:?}");
    rt.shutdown().unwrap();
}

/// Do: a quiet sleep awaited off any shard. Expect: refused.
#[test]
fn g9_s5_a_quiet_sleep_off_a_shard_is_refused() {
    let mut sleep = std::pin::pin!(sleep_quiet(ns(GAP), ns(GAP)));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(sleep.as_mut().poll(&mut cx), Poll::Ready(Err(RtError::NotOnShardThread))));
}
