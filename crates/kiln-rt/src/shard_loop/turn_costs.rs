//! What the loop does per turn that it needn't (kiln's goal 9, plan 01a1a87b2cec step 5: a tenth of the CPU; the loop
//! itself was 12% of the shards' user time in rt7's profile): the clock it reads around a park and between steps, and
//! the inputs an idle turn reads. The shards here run on a driver that counts what is asked of it, its blocking waits
//! ended as the test scripts them, so each count is exact.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::task::{Context, Poll, Waker};

use crate::RtError;
use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::interests::Readiness;
use crate::runtime::{LocalRuntime, RuntimeConfig};

/// What the test driver was asked: readings of its clock, looks at whether it holds anything, and waits that may not
/// block (harvests).
#[derive(Default)]
struct Asked {
    reads: AtomicU64,
    looks: AtomicU64,
    harvests: AtomicU64,
}

impl Asked {
    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }

    fn looks(&self) -> u64 {
        self.looks.load(Ordering::SeqCst)
    }

    fn harvests(&self) -> u64 {
        self.harvests.load(Ordering::SeqCst)
    }
}

/// A driver that counts what it is asked. Its clock moves a nanosecond a reading (so it moves, with nothing of the
/// host's in it); each blocking wait says it began where the test asks, then ends at the script's next line, or at
/// once with no script.
struct Counting {
    asked: Arc<Asked>,
    began: Option<SyncSender<()>>,
    ends: Option<Receiver<()>>,
}

impl Driver for Counting {
    fn kind(&self) -> DriverKind {
        DriverKind::Kqueue
    }

    fn kick_handle(&self) -> Kick {
        Kick::None
    }

    fn now_ns(&self) -> u64 {
        self.asked.reads.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn wait(&mut self, timeout: Option<u64>, _out: &mut Vec<Completion>) -> Result<(), RtError> {
        if timeout == Some(0) {
            self.asked.harvests.fetch_add(1, Ordering::SeqCst);
            return Ok(());
        }
        if let Some(began) = &self.began {
            began.send(()).map_err(|_| RtError::DriverLost)?;
        }
        match &self.ends {
            Some(ends) => ends.recv().map_err(|_| RtError::DriverLost),
            None => Ok(()),
        }
    }

    fn submit_nop(&mut self, _user_data: u64) -> Result<(), RtError> {
        Ok(())
    }

    fn arm(&mut self, _raw: i32, _want: Readiness, _tag: u64) -> Result<(), RtError> {
        Ok(())
    }

    fn has_pending(&self) -> bool {
        self.asked.looks.fetch_add(1, Ordering::SeqCst);
        false
    }
}

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

/// A shard on a counting driver, its waits saying they began on `began` and ending as `ends` says, where given.
fn counting(asked: &Arc<Asked>, began: Option<SyncSender<()>>, ends: Option<Receiver<()>>) -> LocalRuntime {
    let asked = Arc::clone(asked);
    let seed: DriverSeed = Box::new(move |_kick| Ok(Box::new(Counting { asked, began, ends }) as Box<dyn Driver>));
    LocalRuntime::with_driver(&config(), seed, Kick::None).unwrap()
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

/// Shape: park cycles timed, the first after the shard's start-up, whose reads are its own.
const ROUNDS: usize = 6;

/// Do: run a task under `block_on` that waits on a gate `ROUNDS` times, another thread opening the gate and waking it
/// from off the shard each time the shard has entered its driver's wait, then ending that wait. Count the clock's
/// readings from each wait's start to the next. Expect four a cycle: the wait's start and its return (the park's
/// length, on `/metrics`), the poll's start and its end; the return serving the driver's retrieval age and the next
/// step's clock, and the poll's end the clock of the idle step before the next park. Before, seven: the return read
/// again for the retrieval age and again for the step, and the idle step's own.
#[test]
fn g9_s5_a_park_cycle_reads_the_clock_four_times() {
    let asked = Arc::new(Asked::default());
    let (began_tx, began) = sync_channel::<()>(1);
    let (end, ends) = channel::<()>();
    let (wakers_tx, wakers) = channel::<Waker>();
    let open = Arc::new(AtomicBool::new(false));
    let mut rt = counting(&asked, Some(began_tx), Some(ends));
    let reads = std::thread::scope(|scope| {
        let waker_side = scope.spawn({
            let (asked, open) = (Arc::clone(&asked), Arc::clone(&open));
            move || {
                let mut at_each_wait = Vec::new();
                for _ in 0..ROUNDS {
                    began.recv().unwrap();
                    at_each_wait.push(asked.reads());
                    let waker = wakers.recv().unwrap();
                    open.store(true, Ordering::SeqCst);
                    waker.wake();
                    end.send(()).unwrap();
                }
                at_each_wait
            }
        });
        rt.block_on({
            let open = Arc::clone(&open);
            async move {
                for _ in 0..ROUNDS {
                    Gate {
                        open: Arc::clone(&open),
                        wakers: wakers_tx.clone(),
                    }
                    .await;
                }
            }
        })
        .unwrap();
        waker_side.join().unwrap()
    });
    let per_cycle: Vec<u64> = reads.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert_eq!(per_cycle, vec![4; ROUNDS - 1], "the clock's readings from each wait's start to the next");
    let counters = rt.counters();
    assert_eq!(counters.waits, u64::try_from(ROUNDS).unwrap(), "{counters:?}");
}

/// Do: take steps and a park through the public calls, one at a time. Expect: each step reads the clock afresh, and
/// the park its wait's start and return: a caller that steps alone may have done anything between, so only the loops
/// that take their turns back to back hand a reading on.
#[test]
fn g9_s5_a_step_taken_alone_reads_the_clock_afresh() {
    let asked = Arc::new(Asked::default());
    let mut rt = counting(&asked, None, None);
    let before = asked.reads();
    assert!(!rt.step().did_work);
    assert!(!rt.step().did_work);
    assert_eq!(asked.reads() - before, 2, "a reading a step");
    rt.park(None);
    assert_eq!(asked.reads() - before, 4, "the wait's start and its return");
    assert!(!rt.step().did_work);
    assert_eq!(asked.reads() - before, 5, "the step after reads its own");
}

/// Do: on a shard, three tasks that end at once; then tasks that wait for good until the arena refuses one, and one
/// more sent from off the shard (a control message, refused for want of a slot), a step after each. Expect: the pulse's
/// steps, spawns, completions, refusals and longest poll are what the loop has counted (its live counters, which
/// the step used to copy to the pulse whole): each figure recorded where it is counted, the steps at each step.
#[test]
fn g9_s5_the_pulse_says_what_the_counters_say() {
    let asked = Arc::new(Asked::default());
    let mut rt = counting(&asked, None, None);
    let shard = rt.shard_id().0;
    let pulse = || {
        crate::registry::with_entry(shard, |entry| {
            let pulse = &entry.pulse;
            (pulse.steps(), pulse.spawns(), pulse.completed(), pulse.admission_refused(), pulse.longest_step_ns())
        })
        .unwrap()
    };
    let counted = |rt: &LocalRuntime| {
        let live = &rt.context().live;
        (
            live.steps.get(),
            live.spawns.get(),
            live.completed.get(),
            live.admission_refused.get(),
            live.longest_step_ns.get(),
        )
    };
    for _ in 0..3 {
        rt.spawn(async {}).unwrap();
    }
    while rt.step().did_work {}
    assert_eq!(pulse(), counted(&rt));
    let (_, spawns, completed, _, _) = counted(&rt);
    assert_eq!((spawns, completed), (3, 3));
    while rt.spawn(std::future::pending::<()>()).is_ok() {}
    while rt.step().did_work {}
    let holder = crate::registry::holder_of(shard).unwrap();
    crate::runtime::submit_to_holder(holder, async {}).unwrap();
    while rt.step().did_work {}
    assert_eq!(pulse(), counted(&rt));
    assert_eq!(counted(&rt).3, 1, "the loop refused the spawn sent to it");
}

/// Do: hold a readiness wait on a shard (registered, unfired), then take an idle step. Expect: the step asked nothing of
/// the driver (it looked at nothing: no input was pending, and its harvest wasn't due), where it read every input
/// before, asking the driver twice; and the driver is still owed its look before the shard ends a nonblocking run, so
/// `run_until_idle` makes exactly one harvest at its last turn.
#[test]
fn g9_s5_an_idle_turn_with_readiness_waits_reads_no_input() {
    let asked = Arc::new(Asked::default());
    let mut rt = counting(&asked, None, None);
    rt.spawn(async {
        let _ = crate::readiness::ready(crate::readiness::Target::Os(7), false).await;
    })
    .unwrap();
    while rt.step().did_work {}
    let (looks, harvests) = (asked.looks(), asked.harvests());
    assert!(!rt.step().did_work);
    assert_eq!(asked.looks() - looks, 0, "the idle step asked its driver whether it held anything");
    assert_eq!(asked.harvests() - harvests, 0, "the idle step harvested");
    rt.run_until_idle();
    assert_eq!(asked.harvests() - harvests, 1, "the nonblocking run's last turn asks the driver once");
}

/// The shard's counters, asked of it by a task on it.
fn counters_on(rt: &crate::runtime::Runtime, shard: crate::shard::ShardId) -> crate::shard_loop::Counters {
    let (tx, rx) = channel();
    rt.spawn_on(shard, async move {
        let _ = tx.send(crate::registry::with_current(|ctx| ctx.counters()).unwrap_or_default());
    })
    .unwrap();
    rx.recv_timeout(ANSWER).unwrap()
}

/// Waits `span` on the test's thread: a channel nobody sends on is the wait (the runtime's `sleep` is disallowed).
fn pause(span: std::time::Duration) {
    let (_hold, rx) = channel::<()>();
    let started = std::time::Instant::now();
    while started.elapsed() < span {
        let _ = rx.recv_timeout(span.saturating_sub(started.elapsed()));
    }
}

/// Shape: how long the test waits on a shard's answer (a step is microseconds).
const ANSWER: std::time::Duration = std::time::Duration::from_secs(5);

/// Shape: the pause before each wake, long against a step and a park's start (microseconds), so the shard has parked.
const GAP: std::time::Duration = std::time::Duration::from_millis(2);

/// Kiln's goal 9, plan 01a1a87b2cec step 5: a wake from a park takes one step, the wake's own, before the next park. Do:
/// on a running runtime's shard (its loop [`crate::shard_loop::Shard::run`], on the OS's driver), a task that waits on a
/// gate `WAKES` times, the test's thread opening the gate and waking the task from off the shard each time, a pause after
/// the last so the shard has parked. Count the shard's steps and waits over them. Expect: about a step a wait, fewer than
/// three for two, where each wake took two (its own, then an idle one that found nothing before the park).
#[test]
fn g9_s5_a_wake_from_a_park_takes_one_step() {
    const WAKES: u64 = 32;
    let rt = crate::runtime::Runtime::start(&config()).unwrap();
    let shard = rt.shard_ids()[0];
    let (wakers_tx, wakers) = channel::<Waker>();
    let (done_tx, done) = channel::<()>();
    let open = Arc::new(AtomicBool::new(false));
    rt.spawn_on(shard, {
        let open = Arc::clone(&open);
        async move {
            for _ in 0..WAKES {
                Gate {
                    open: Arc::clone(&open),
                    wakers: wakers_tx.clone(),
                }
                .await;
            }
            let _ = done_tx.send(());
        }
    })
    .unwrap();
    let mut waker = wakers.recv_timeout(ANSWER).unwrap();
    let before = counters_on(&rt, shard);
    for n in 0..WAKES {
        pause(GAP);
        open.store(true, Ordering::SeqCst);
        waker.wake_by_ref();
        if n + 1 < WAKES {
            waker = wakers.recv_timeout(ANSWER).unwrap();
        }
    }
    done.recv_timeout(ANSWER).unwrap();
    pause(GAP);
    let after = counters_on(&rt, shard);
    let (steps, waits) = (after.steps - before.steps, after.waits - before.waits);
    assert!(waits >= WAKES, "{waits} waits for {WAKES} wakes from off the shard: {after:?}");
    assert!(steps * 2 < waits * 3, "{steps} steps for {waits} waits: an idle step after each wake");
    rt.shutdown().unwrap();
}

/// Kiln's goal 9, plan 01a1a87b2cec step 5 (legacy audit, 2026-11-06): a wake from a park takes one step on a local
/// runtime's loop too ([`LocalRuntime::block_on`], what each of kiln's net threads runs on), as on a runtime's shard
/// ([`g9_s5_a_wake_from_a_park_takes_one_step`]). Do: a local runtime on the OS's driver, its root waiting on a gate
/// `WAKES` times, another thread opening the gate and waking it each time after a pause, so the loop has parked. Count
/// the shard's steps and waits over them, from the root. Expect: about a step a wait, fewer than three for two, where
/// block_on took two a wake (its own, then an idle one that found nothing before the park).
#[test]
fn g9_s5_a_wake_from_a_park_takes_one_step_in_block_on() {
    const WAKES: u64 = 32;
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let (wakers_tx, wakers) = channel::<Waker>();
    let open = Arc::new(AtomicBool::new(false));
    let opener = {
        let open = Arc::clone(&open);
        std::thread::spawn(move || {
            for _ in 0..WAKES {
                let waker = wakers.recv_timeout(ANSWER).unwrap();
                pause(GAP);
                open.store(true, Ordering::SeqCst);
                waker.wake_by_ref();
            }
        })
    };
    let (steps, waits) = rt
        .block_on(async move {
            let counters = || crate::registry::with_current(|ctx| ctx.counters()).unwrap_or_default();
            let before = counters();
            for _ in 0..WAKES {
                Gate {
                    open: Arc::clone(&open),
                    wakers: wakers_tx.clone(),
                }
                .await;
            }
            let after = counters();
            (after.steps - before.steps, after.waits - before.waits)
        })
        .unwrap();
    opener.join().unwrap();
    assert!(waits >= WAKES, "{waits} waits for {WAKES} wakes from another thread");
    assert!(steps * 2 < waits * 3, "{steps} steps for {waits} waits: an idle step after each wake");
}
