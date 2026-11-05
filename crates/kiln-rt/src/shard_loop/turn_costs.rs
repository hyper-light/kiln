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
