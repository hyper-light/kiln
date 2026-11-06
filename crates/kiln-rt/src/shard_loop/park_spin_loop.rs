//! The spin before a park, in the loop (kiln's goal 9, plan 01a1a87b2cec step 4; `crate::park_spin`): a task on the
//! shard asks for it; the shard then spins before each park and counts what it did, finds work already waiting without
//! parking, and with a spin that sizes itself counts each park's gap into its window; asked for none, it parks at once,
//! as before. The shard waits in a driver whose blocking waits end as the test scripts them, on a real clock.

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use crate::RtError;
use crate::driver::{Completion, Driver, DriverKind, DriverSeed, Kick};
use crate::interests::Readiness;
use crate::park_spin::{Ask, ask_current_thread};
use crate::registry;
use crate::runtime::{RuntimeConfig, SlotGuard};
use crate::shard_loop::{Shard, ShardSeed};

/// A driver on the host's clock whose blocking waits each end at the script's next line, nothing delivered (as a
/// deadline would end them); a zero-timeout harvest returns at once with nothing.
struct Scripted {
    epoch: Instant,
    ends: Receiver<()>,
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

    fn wait(&mut self, timeout: Option<u64>, _out: &mut Vec<Completion>) -> Result<(), RtError> {
        if timeout == Some(0) {
            return Ok(());
        }
        self.ends.recv().map_err(|_| RtError::DriverLost)
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
        0
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

/// A registered shard, and the guard that gives its slot back after it: fields drop in their order, the shard first.
struct Held {
    shard: Shard,
    _slot: SlotGuard,
}

/// A registered shard on a scripted driver, its waits ending as `ends` says.
fn scripted(ends: Receiver<()>) -> Held {
    let seed: DriverSeed = Box::new(move |_kick| {
        Ok(Box::new(Scripted {
            epoch: Instant::now(),
            ends,
        }) as Box<dyn Driver>)
    });
    let seed = ShardSeed::register(&config(), seed, registry::RegisterKick::Kick(Kick::None)).unwrap();
    let slot = SlotGuard::new(seed.holder);
    Held {
        shard: Shard::build(seed).unwrap(),
        _slot: slot,
    }
}

/// A task on `shard` asks for `ask`, run to its end.
fn ask(shard: &mut Shard, ask: Ask) {
    shard
        .spawn_local(crate::shard::boxed(async move {
            ask_current_thread(ask).unwrap();
        }))
        .unwrap();
    while shard.step().did_work {}
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

/// Goal 9, plan 01a1a87b2cec step 4: asked for none, the shard parks at once: no spin, nothing counted.
#[test]
fn g9_s4_a_shard_asked_for_no_spin_parks_at_once() {
    let (_ends_tx, ends) = channel();
    let mut held = scripted(ends);
    let shard = &mut held.shard;
    let before = Instant::now();
    assert!(!shard.spin_before_park(None));
    let counters = shard.counters();
    assert_eq!((counters.park_spins, counters.park_spin_hits, counters.park_spin_ns), (0, 0, 0));
    assert_eq!(counters.park_spin_now_ns, 0);
    assert!(before.elapsed().as_micros() < 1_000_000, "it didn't wait");
}

/// Goal 9, plan 01a1a87b2cec step 4: a fixed spin runs its time out when nothing comes, counted as a spin and its time,
/// and the park follows; then it finds a wake already waiting at once, a hit, with no park.
#[test]
fn g9_s4_a_fixed_spin_runs_out_or_finds_work_without_parking() {
    let (ends_tx, ends) = channel();
    let mut held = scripted(ends);
    let shard = &mut held.shard;
    ask(shard, Ask::Fixed { ns: 200_000 });
    let start = Instant::now();
    assert!(!shard.spin_before_park(None), "nothing came");
    assert!(start.elapsed().as_nanos() >= 200_000, "it spun its time");
    let counters = shard.counters();
    assert_eq!((counters.park_spins, counters.park_spin_hits), (1, 0));
    assert!(counters.park_spin_ns >= 200_000, "{} ns spun", counters.park_spin_ns);
    assert_eq!(counters.park_spin_now_ns, 200_000);
    // The park the miss leaves to: one wait.
    let waits = counters.waits;
    ends_tx.send(()).unwrap();
    shard.park_owned(None);
    assert_eq!(shard.counters().waits, waits + 1);

    // A task pending on a waker, woken from off its poll: the spin finds the wake at once and the shard never parks.
    let (wakers_tx, wakers) = channel();
    shard
        .spawn_local(crate::shard::boxed(PendOnce {
            wakers: wakers_tx,
            polled: false,
        }))
        .unwrap();
    while shard.step().did_work {}
    let waker = wakers.recv().unwrap();
    waker.wake();
    let (spins, waits) = (shard.counters().park_spins, shard.counters().waits);
    let start = Instant::now();
    assert!(shard.spin_before_park(None), "the wake was found");
    assert!(start.elapsed().as_nanos() < 200_000_000, "found at once, not after a long spin");
    let counters = shard.counters();
    assert_eq!(counters.park_spins, spins + 1);
    assert_eq!(counters.park_spin_hits, 1);
    assert_eq!(counters.waits, waits, "no park");
    while shard.step().did_work {}
}

/// Goal 9, plan 01a1a87b2cec step 4: a spin that sizes itself starts at none and parks at once, each park's gap counted
/// into its window, and what each park cost the thread learned where the shard has a task clock to read it on.
#[test]
fn g9_s4_a_sized_spin_counts_each_parks_gap() {
    let (ends_tx, ends) = channel();
    let mut held = scripted(ends);
    let shard = &mut held.shard;
    ask(shard, Ask::Sized { cap_ns: crate::park_spin::CAP_NS });
    // The loop takes a new ask at its next idle moment.
    assert!(!shard.core.park_spin.sizes());
    for parked in 1..=3u64 {
        assert!(!shard.spin_before_park(None), "unsized: no spin");
        assert!(shard.core.park_spin.sizes());
        ends_tx.send(()).unwrap();
        shard.park_owned(None);
        assert_eq!(shard.core.park_spin.window_gaps(), parked);
    }
    let counters = shard.counters();
    assert_eq!((counters.park_spins, counters.park_spin_now_ns), (0, 0));
    assert_eq!(shard.core.park_cost.samples() > 0, shard.desk.task_clock.is_some());
    // Asked for none again: what the window held is let go of, and the shard parks at once.
    ask(shard, Ask::Off);
    assert!(!shard.spin_before_park(None));
    assert_eq!(shard.core.park_spin.window_gaps(), 0);
    assert!(!shard.core.park_spin.sizes());
}
