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

use super::{By, Came, Quiet, Wake, came, deadline, kick_wait, sleep_quiet, to_poll};
use crate::RtError;
use crate::parking::Woken;
use crate::runtime::{Runtime, RuntimeConfig};
use crate::shard::ShardId;
use crate::shard_loop::Counters;

/// Shape: a quiet sleep's span and quiet in the runtime's tests: long against a park's start and the host's wake
/// (microseconds), and against the gaps between the work that wakes the busy shard ([`GAP`]), so only a host that
/// held the test's thread longer than this could let the busy shard's park run out.
const SPAN: Duration = Duration::from_millis(200);

/// Shape: the gap between the busy shard's wakes: long against a step, short against [`SPAN`].
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

/// A quiet sleep of [`SPAN`] spawned on `shard`, sending how its turn came, when it began and how long it took on `tx`.
fn sleeper(rt: &Runtime, shard: ShardId, tx: Sender<(Result<Came, RtError>, Instant, Duration)>) {
    rt.spawn_on(shard, async move {
        let started = Instant::now();
        let came = sleep_quiet(ns(SPAN), ns(SPAN)).await;
        let _ = tx.send((came, started, started.elapsed()));
    })
    .unwrap();
}

/// A future pending until `open` is set (and then setting it back), handing its waker over each time it waits.
struct Gate {
    open: Arc<AtomicBool>,
    wakers: Sender<Waker>,
}

impl Future for Gate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.open.swap(false, Ordering::SeqCst) {
            return Poll::Ready(());
        }
        self.wakers.send(cx.waker().clone()).unwrap();
        Poll::Pending
    }
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
/// timer, woken, that wake's time and the rest to the poll; by a socket, woken, unknown; by its own deadline, quiet, its
/// lateness and the rest.
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
    assert_eq!(taken(wake(151, By::Quiet, Some(1)), 160), Came::Quiet(10));
    assert_eq!(taken(wake(100, By::Quiet, Some(0)), 100), Came::Quiet(0), "a wake as it falls due, taken at once");
    assert_eq!(to_poll(Came::Running(5), 200, 100), Came::Running(5), "a poll that reads an earlier clock adds nothing");
}

/// Do: a sleep falling due at 1_000 on the clock of a driver whose epoch is 5_000 on the shard's; the park it fell due
/// in announced at 5_700 and waiting from 5_800 on the shard's clock, kicked by a sender that stamped 5_900 (900 on the
/// driver's); the loop reading the wait's return at 1_200 (6_200), the park itself later, at 6_500 or at 9_000; the turn
/// taken by a poll at 1_700. Expect: `Came::Woken`, the kick's wait to the loop's reading (300) and the rest from it to
/// the poll (500) summing to the whole, from the kick's stamp to the poll (800), each stretch once, however late the
/// park's own reading; and no wait for a kick stamped as the park was set up, which it didn't measure.
#[test]
fn g9_s5_a_woken_sleeps_parts_sum_to_its_whole() {
    let (epoch, stamped) = (5_000, 5_900);
    let quiet = Quiet { due_ns: 1_000, quiet_ns: 500, word: 0, came: None, site: None };
    let woken = Woken { announced_ns: 5_700, waiting_ns: 5_800, returned_ns: 6_500, kicked_ns: Some(stamped) };
    let (woke, poll) = (1_200, 1_700);
    let whole = poll + epoch - stamped;
    for returned_ns in [6_500, 9_000] {
        let waited = kick_wait(&Woken { returned_ns, ..woken }, woke, epoch);
        assert_eq!(waited, Some(300), "the kick's wait, to the loop's reading of the wait's return");
        let (turn, from) = came(&quiet, Some(Wake { at_ns: woke, by: By::Kick, waited_ns: waited }));
        assert_eq!((turn, from), (Came::Woken(Some(300)), woke), "woken, the rest counted from the loop's reading");
        assert_eq!(to_poll(turn, from, poll), Came::Woken(Some(whole)), "its parts sum to its whole");
    }
    let early = Woken { kicked_ns: Some(5_750), ..woken };
    assert_eq!(kick_wait(&early, woke, epoch), None, "a kick stamped as the park was set up");
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
    let (came, _, took) = rx.recv_timeout(ANSWER).unwrap();
    let after = counters_on(&rt, shard);
    assert!(matches!(came, Ok(Came::Quiet(_))), "{came:?}");
    assert!(took >= SPAN, "it came after {took:?}, its span {SPAN:?}");
    assert!(after.woken_by_timer > before.woken_by_timer, "no timer woke the shard: {before:?} to {after:?}");
    rt.shutdown().unwrap();
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5: a shard that work wakes within a quiet sleep's quiet is never woken for it.
/// Its deadline ends a park only after its quiet with no wake at all, so it does once a host holds the thread waking the
/// shard that long, and the sleep then comes by it, as it should. Do: on a running runtime's shard, a quiet sleep of
/// [`SPAN`], and a task the test's thread wakes from off the shard every [`GAP`] until the sleep has come, noting when
/// each wake began and ended. Count the shard's wakes by timers from the sleep's start to its end. Expect: one exactly
/// when the sleep came by its own deadline (`Came::Quiet`), and that only if the test's thread went the quiet without a
/// wake while it waited; so while the thread kept up, none, and the sleep's turn came with the shard's work
/// (`Came::Woken`, by a kick, or `Came::Running` where the shard was running as it fell due); either way after its span.
#[test]
fn g9_s5_a_shard_work_wakes_within_its_quiet_takes_no_timer_wake() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let (wakers_tx, wakers) = channel::<Waker>();
    let open = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    rt.spawn_on(shard, {
        let (open, stop) = (Arc::clone(&open), Arc::clone(&stop));
        async move {
            while !stop.load(Ordering::SeqCst) {
                Gate { open: Arc::clone(&open), wakers: wakers_tx.clone() }.await;
            }
        }
    })
    .unwrap();
    let mut waker = wakers.recv_timeout(ANSWER).unwrap();
    let before = counters_on(&rt, shard);
    let (tx, rx) = channel();
    sleeper(&rt, shard, tx);
    let started = Instant::now();
    // Each wake of the task, from just before it to just after: it reached the shard between the two.
    let mut woke = Vec::new();
    let (came, began, took) = loop {
        pause(GAP);
        let from = Instant::now();
        open.store(true, Ordering::SeqCst);
        waker.wake();
        woke.push((from, Instant::now()));
        waker = wakers.recv_timeout(ANSWER).unwrap();
        if let Ok(answer) = rx.try_recv() {
            break answer;
        }
        assert!(started.elapsed() < ANSWER, "the sleep never came");
    };
    let after = counters_on(&rt, shard);
    stop.store(true, Ordering::SeqCst);
    open.store(true, Ordering::SeqCst);
    waker.wake();
    let came = match came {
        Ok(came) => came,
        Err(refused) => panic!("the sleep was refused: {refused:?}"),
    };
    // The longest the test's thread went without a wake while the sleep waited: from just before one of its wakes (or the
    // sleep's start) to just after the next (or its end), the furthest apart the two can have reached the shard.
    let ended = began + took;
    let froms = std::iter::once(began).chain(woke.iter().map(|(from, _)| *from));
    let tos = woke.iter().map(|(_, to)| *to).chain(std::iter::once(ended));
    let held = froms.zip(tos).map(|(from, to)| to.min(ended).saturating_duration_since(from.max(began))).max();
    let held = held.unwrap_or_default();
    let (timer, kicks) = (after.woken_by_timer - before.woken_by_timer, after.woken_by_kick - before.woken_by_kick);
    eprintln!("came {came:?} after {took:?}; {held:?} at the most without a wake; {timer} timer wakes, {kicks} kicks");
    assert!(took >= SPAN, "it came after {took:?}, its span {SPAN:?}");
    // The shard's only timer is the sleep's own deadline, which ends it: a timer wake exactly when it came by that.
    let quiet = matches!(came, Came::Quiet(_));
    assert_eq!(timer, u64::from(quiet), "it came {came:?}: {before:?} to {after:?}");
    // And that only after its quiet with no wake at all: while the test's thread kept up, the turn came with work.
    assert!(!quiet || held >= SPAN, "it came {came:?}, the test's thread {held:?} at the most without a wake");
    if let Came::Woken(_) = came {
        assert!(kicks > 0, "woken with no kick: {before:?} to {after:?}");
    }
    rt.shutdown().unwrap();
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5: a quiet sleep falling due while its shard runs ends at the shard's next step,
/// as a timer would, saying how late: from its due time to that step's poll of it. Do: on a running runtime's shard, a
/// quiet sleep of [`SPAN`], then a task that holds the shard twice that in one poll; each notes the shard's clock and
/// counts as it begins and ends. Expect, where the hold began after the sleep and before it could fall due:
/// `Came::Running`, in the step after the hold's with no park between, as late as that step came, after the hold's end
/// and by the sleep's own, less its due time (its span after it began, by the hold's start); in every run, no earlier
/// than its span.
#[test]
fn g9_s5_a_quiet_sleep_due_on_a_running_shard_ends_at_its_next_step() {
    let rt = Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let counts = || crate::registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
    let (tx, rx) = channel();
    rt.spawn_on(shard, async move {
        let armed = crate::futures::now_ns();
        let came = sleep_quiet(ns(SPAN), ns(SPAN)).await;
        let _ = tx.send((came, armed, crate::futures::now_ns(), counts()));
    })
    .unwrap();
    let (held_tx, held) = channel();
    rt.spawn_on(shard, async move {
        let from = crate::futures::now_ns();
        hold(SPAN * 2);
        let _ = held_tx.send((from, crate::futures::now_ns(), counts()));
    })
    .unwrap();
    let (from, to, at_end) = held.recv_timeout(ANSWER).unwrap();
    let (came, armed, taken, at_turn) = rx.recv_timeout(ANSWER).unwrap();
    let came = match came {
        Ok(came) => came,
        Err(refused) => panic!("the sleep was refused: {refused:?}"),
    };
    let (span, since) = (ns(SPAN), |at: u64| i128::from(at) - i128::from(armed));
    let (steps, waits) = ((at_end.steps, at_turn.steps), (at_end.waits, at_turn.waits));
    let (came_at, held_over) = (since(taken), (since(from), since(to)));
    eprintln!("came {came:?} at {came_at}, the hold {held_over:?} (ns after it began); steps {steps:?}, waits {waits:?}");
    // Its due time is its span after the shard's clock as it was first polled, at or after `armed`.
    assert!(taken - armed >= span, "it came {} ns after it began, within its span", taken - armed);
    if (armed..armed + span).contains(&from) {
        // It fell due while the shard ran: its turn came at the next step, the one after the hold's.
        let Came::Running(late) = came else {
            panic!("it fell due while the shard ran, and came {came:?}");
        };
        let next = steps.1 == steps.0 + 1 && waits.1 == waits.0;
        assert!(next, "its turn not the step after the hold's: steps {steps:?}, waits {waits:?}");
        // Its due time is no earlier than its span after `armed` and no later than its span after `from` (it was polled
        // first); the step's poll of it came after the hold's end, `to`, and by its own, `taken`.
        let (least, most) = (to.saturating_sub(from).saturating_sub(span), (taken - armed).saturating_sub(span));
        let after_due = least..=most;
        assert!(after_due.contains(&late), "{late} ns late, the step after the hold's {after_due:?} ns after it fell due");
    }
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
