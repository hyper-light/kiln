//! What a shard's parks and wakes are counted as (kiln's goal 9, plan 01a1a87b2cec step 5: what wakes and parks
//! cost a shard, which the lab reports per request): each wait by what ended it, a park withdrawn as one, a kick
//! on the shard whose thread sent it, and a kick its sender was saved on the shard it reached.
//!
//! The shards here park in a driver whose waits end as the test scripts them, so each count is exact; the last
//! test parks on the OS's own driver.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use crate::RtError;
use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::interests::Readiness;
use crate::runtime::{LocalRuntime, RuntimeConfig};
use crate::shard_loop::Counters;

/// How a scripted wait ends.
enum End {
    /// A kick: the driver's count of them moves, and nothing is delivered.
    Kick,
    /// I/O: a completion naming the task word it carries.
    Io(u64),
    /// Its deadline: nothing.
    Deadline,
}

/// A driver whose blocking waits each end as the test's next line of script says, saying first that one began
/// where the test asks; a zero-timeout harvest returns at once with nothing.
struct Scripted {
    epoch: Instant,
    began: Option<SyncSender<()>>,
    ends: Receiver<End>,
    kicks: u64,
}

impl Driver for Scripted {
    fn kind(&self) -> DriverKind {
        DriverKind::Kqueue
    }

    fn kick_handle(&self) -> Kick {
        Kick::None
    }

    fn now_ns(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }

    fn wait(&mut self, timeout: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
        if timeout == Some(0) {
            return Ok(());
        }
        if let Some(began) = &self.began {
            began.send(()).map_err(|_| RtError::DriverLost)?;
        }
        match self.ends.recv().map_err(|_| RtError::DriverLost)? {
            End::Kick => self.kicks += 1,
            End::Io(word) => out.push(Completion {
                user_data: word,
                result: 0,
            }),
            End::Deadline => {}
        }
        Ok(())
    }

    fn submit_nop(&mut self, _user_data: u64) -> Result<(), RtError> {
        Ok(())
    }

    fn arm(&mut self, _raw: i32, _want: Readiness, _tag: u64) -> Result<(), RtError> {
        Ok(())
    }

    fn has_pending(&self) -> bool {
        false
    }

    fn kicks_taken(&self) -> u64 {
        self.kicks
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

/// A shard on a scripted driver, its waits ending as `ends` says, saying each began on `began` where given.
fn scripted(ends: Receiver<End>, began: Option<SyncSender<()>>) -> LocalRuntime {
    let seed: DriverSeed = Box::new(move |_kick| {
        Ok(Box::new(Scripted {
            epoch: Instant::now(),
            began,
            ends,
            kicks: 0,
        }) as Box<dyn Driver>)
    });
    LocalRuntime::with_driver(&config(), seed, Kick::None).unwrap()
}

/// A future pending until it is polled again, handing its waker over on its first poll.
struct PendOnce {
    wakers: Sender<Waker>,
    polled: bool,
}

impl Future for PendOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            return Poll::Ready(());
        }
        self.polled = true;
        self.wakers.send(cx.waker().clone()).unwrap();
        Poll::Pending
    }
}

/// Spawns a task on `rt` that waits for one wake and then says so on `ran`, runs it to its wait, and hands back
/// its waker.
fn waiting_task(rt: &mut LocalRuntime, ran: &Sender<()>) -> Waker {
    let (wakers, waker) = channel::<Waker>();
    let ran = ran.clone();
    rt.spawn(async move {
        PendOnce {
            wakers,
            polled: false,
        }
        .await;
        ran.send(()).unwrap();
    })
    .unwrap();
    while rt.step().did_work {}
    waker.recv().unwrap()
}

/// The counts that moved from `before` to `after`: waits, parks withdrawn, and waits ended by a kick, by I/O and
/// by their deadline.
fn moved(before: &Counters, after: &Counters) -> (u64, u64, u64, u64, u64) {
    (
        after.waits - before.waits,
        after.parks_withdrawn - before.parks_withdrawn,
        after.woken_by_kick - before.woken_by_kick,
        after.woken_by_io - before.woken_by_io,
        after.woken_by_timer - before.woken_by_timer,
    )
}

/// Do: park a shard three times, its driver's waits ended by a kick, by a task's I/O and by the deadline.
/// Expect: three waits, one counted by each of the three, none withdrawn, and the task the I/O named ran.
#[test]
fn g9_s5_each_wait_is_counted_by_what_ended_it() {
    let (script, ends) = channel::<End>();
    let mut rt = scripted(ends, None);
    let (ran_tx, ran) = channel::<()>();
    let waker = waiting_task(&mut rt, &ran_tx);
    let word = crate::waker::word_of(&waker).unwrap().word();
    let before = rt.counters();
    script.send(End::Kick).unwrap();
    rt.park(None);
    script.send(End::Io(word)).unwrap();
    rt.park(None);
    while rt.step().did_work {}
    script.send(End::Deadline).unwrap();
    rt.park(Some(u64::MAX));
    let after = rt.counters();
    assert_eq!(
        moved(&before, &after),
        (3, 0, 1, 1, 1),
        "(waits, withdrawn, by kick, by I/O, by deadline)"
    );
    ran.try_recv().expect("the task the completion named ran");
}

/// Do: wake a shard's task from another thread while the shard runs, then park the shard. Expect: the park is
/// withdrawn and counted so, with no wait made; the wake found the shard running, so its sender was saved the
/// kick, counted on the shard it reached; the task runs.
#[test]
fn g9_s5_a_park_with_work_waiting_is_withdrawn_and_its_senders_kick_saved() {
    // No script: a wait would end the driver, so a park that waited would show as one.
    let (_script, ends) = channel::<End>();
    let mut rt = scripted(ends, None);
    let (ran_tx, ran) = channel::<()>();
    let waker = waiting_task(&mut rt, &ran_tx);
    let before = rt.counters();
    std::thread::spawn(move || waker.wake()).join().unwrap();
    rt.park(None);
    let after = rt.counters();
    assert_eq!(
        moved(&before, &after),
        (0, 1, 0, 0, 0),
        "(waits, withdrawn, by kick, by I/O, by deadline)"
    );
    assert_eq!(after.kicks_saved - before.kicks_saved, 1, "the wake found the shard running");
    while rt.step().did_work {}
    ran.try_recv().expect("the woken task ran");
}

/// Do: a task on shard A wakes a task of A's own and a task on shard B while B is parked, then B's wait is ended
/// by that kick. Expect: A counts one wake of its own, one of another shard's and the one kick its thread sent;
/// B counts none of those, its wait ended by a kick and no kick saved; both woken tasks run.
#[test]
fn g9_s5_wakes_and_kicks_are_counted_on_the_shard_whose_thread_sent_them() {
    let (to_b, b_ends) = channel::<End>();
    let (began_tx, began) = sync_channel::<()>(1);
    let (b_waker_tx, b_waker) = channel::<Waker>();
    let (b_counts_tx, b_counts) = channel::<(Counters, Counters)>();
    let (b_ran_tx, b_ran) = channel::<()>();
    let (a_ran_tx, a_ran) = channel::<()>();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut b = scripted(b_ends, Some(began_tx));
            b_waker_tx.send(waiting_task(&mut b, &b_ran_tx)).unwrap();
            let before = b.counters();
            b.park(None);
            while b.step().did_work {}
            b_counts_tx.send((before, b.counters())).unwrap();
        });
        let b_waker = b_waker.recv().unwrap();
        // B is inside its driver's wait, announced parked, until its script ends the wait.
        began.recv().unwrap();
        let (_script, a_ends) = channel::<End>();
        let mut a = scripted(a_ends, None);
        let a_waker = waiting_task(&mut a, &a_ran_tx);
        let before = a.counters();
        a.spawn(async move {
            a_waker.wake_by_ref();
            b_waker.wake_by_ref();
        })
        .unwrap();
        while a.step().did_work {}
        let after = a.counters();
        assert_eq!(after.wakes_sent_local - before.wakes_sent_local, 1, "A woke its own task");
        assert_eq!(after.wakes_sent_foreign - before.wakes_sent_foreign, 1, "A woke B's task");
        assert_eq!(after.kicks_sent - before.kicks_sent, 1, "A's task kicked parked B");
        a_ran.try_recv().expect("A's woken task ran");
        to_b.send(End::Kick).unwrap();
    });
    let (before, after) = b_counts.recv().unwrap();
    assert_eq!(
        moved(&before, &after),
        (1, 0, 1, 0, 0),
        "(waits, withdrawn, by kick, by I/O, by deadline)"
    );
    assert_eq!(
        (
            after.wakes_sent_local - before.wakes_sent_local,
            after.wakes_sent_foreign - before.wakes_sent_foreign,
            after.kicks_sent - before.kicks_sent
        ),
        (0, 0, 0),
        "B woke and kicked no one"
    );
    assert_eq!(after.kicks_saved - before.kicks_saved, 0, "A's wake took B's kick");
    b_ran.try_recv().expect("B's woken task ran");
}

/// Shape: parks the OS-driver test makes, each woken from another thread: more than one, so a kick a park left
/// behind for the next is counted too.
const OS_ROUNDS: usize = 3;

/// Do: park a shard on the OS's own driver while another thread wakes its task as soon as it sees the park
/// announced (or once the park has returned), a few rounds. Expect: each round's park either waited or was
/// withdrawn, every wait was ended by a kick (the driver's count of them: an eventfd's, kqueue's, a port's) and
/// none by I/O or a deadline, and every woken task ran.
#[test]
#[cfg_attr(miri, ignore)]
fn g9_s5_the_os_drivers_kick_ends_a_wait_counted_as_a_kick() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let shard = rt.shard_id().0;
    let (ran_tx, ran) = channel::<()>();
    for round in 0..OS_ROUNDS {
        let waker = waiting_task(&mut rt, &ran_tx);
        let before = rt.counters();
        let returned = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let returned = &returned;
            scope.spawn(move || {
                // A park withdrawn or ended by a kick left from an earlier round may come and go unseen: the wake
                // goes then too, to a running shard.
                while !returned.load(Ordering::Acquire)
                    && !crate::registry::with_entry(shard, |entry| entry.parking.parked())
                        .unwrap_or(true)
                {
                    std::hint::spin_loop();
                }
                waker.wake();
            });
            rt.park(None);
            returned.store(true, Ordering::Release);
        });
        let after = rt.counters();
        let (waits, withdrawn, kick, io, timer) = moved(&before, &after);
        assert_eq!(waits + withdrawn, 1, "round {round}: the park waited or was withdrawn");
        assert_eq!(kick, waits, "round {round}: a wait here is ended only by a kick");
        assert_eq!((io, timer), (0, 0), "round {round}: no I/O and no deadline");
        while rt.step().did_work {}
        ran.try_recv().expect("the woken task ran");
    }
}
