//! The kernel's own wake-up, with no system around it: the floor under every subscriber that
//! waits. A publisher on the same open loop writes each message's scheduled time into a ring and
//! wakes the waiters (`Thread::unpark`, a futex on Linux, which Quiver's waits come down to as the
//! brokers' clients' epoll does); each waiter, woken, takes what was written since it last looked.
//! With `--busy` the waiters look without sleeping: the floor of a cache line's trip between two
//! cores. Its rows sit beside the systems' in the report, so what a system adds to a wake-up is its
//! own.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::follow::{Follow, Schedule, Seen, Window, stamp};
use crate::profile::{Profiler, Role, Watch};
use crate::report::{Run, tally};
use crate::{Args, System, thread_cpu_s};

/// Messages the ring holds: a waiter further behind than this has missed what it skipped.
const RING: u64 = 1 << 16;

struct Shared {
    base: Instant,
    window: Window,
    /// Each message's scheduled time, at its number's slot.
    ring: Vec<AtomicU64>,
    /// Messages published so far: the next one's number.
    published: AtomicU64,
    /// The number each waiter takes next.
    caught_up: Vec<AtomicU64>,
    stop: AtomicBool,
}

fn waiter(index: usize, shared: Arc<Shared>, busy: bool) -> (Seen, Follow) {
    crate::profile::take_role(Role::Subscriber);
    let mut seen = Seen::new();
    let mut follow = Follow::new();
    let mut payload = [0u8; 16];
    loop {
        let published = shared.published.load(Ordering::Acquire);
        let mut next = follow.next().max(published.saturating_sub(RING));
        while next < published {
            let at = shared.ring[(next % RING) as usize].load(Ordering::Acquire);
            stamp(&mut payload, at, next);
            let now = shared.base.elapsed().as_nanos() as u64;
            seen.take(&mut follow, &payload, now, &shared.window);
            next += 1;
        }
        shared.caught_up[index].store(follow.next(), Ordering::Release);
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        if shared.published.load(Ordering::Acquire) == published {
            // Nothing since the look: a publish after it leaves its unpark, so this park returns.
            if busy {
                std::hint::spin_loop();
            } else {
                std::thread::park();
            }
        }
    }
    (seen, follow)
}

/// One run, profiled by `profiler`: `--subscribers` waiters, each on a thread of its own, at most
/// `--workers` of them.
pub fn run(a: &Args, seed: u64, profiler: &Profiler) -> Run {
    let waiters = a.workers.min(a.subscribers).max(1);
    let shared = Arc::new(Shared {
        base: Instant::now(),
        window: Window::new(),
        ring: (0..RING).map(|_| AtomicU64::new(0)).collect(),
        published: AtomicU64::new(0),
        caught_up: (0..waiters).map(|_| AtomicU64::new(0)).collect(),
        stop: AtomicBool::new(false),
    });
    let load_before = hyper_measure::usage::load();
    let cpu_before = hyper_measure::usage::this().ok();
    let handles: Vec<_> = (0..waiters)
        .map(|i| {
            let shared = Arc::clone(&shared);
            let busy = a.busy;
            std::thread::Builder::new()
                .name(format!("wake-{i}"))
                .spawn(move || waiter(i, shared, busy))
                .unwrap()
        })
        .collect();
    let threads: Vec<_> = handles.iter().map(|h| h.thread().clone()).collect();

    // A millisecond in, so every waiter is waiting.
    let mut schedule = Schedule::new(a, seed, 1_000_000, &shared.window);
    let (from, until) = schedule.window();
    let pending = profiler.watch(Watch::new(shared.base, from, until));
    let began = Instant::now();
    let publisher_before = thread_cpu_s();
    loop {
        if schedule.over(shared.base.elapsed().as_nanos() as u64) {
            break;
        }
        let Some((at, number)) = schedule.next() else {
            break;
        };
        while (shared.base.elapsed().as_nanos() as u64) < at {
            std::hint::spin_loop();
        }
        shared.ring[(number % RING) as usize].store(at, Ordering::Release);
        shared.published.store(number + 1, Ordering::Release);
        crate::profile::published();
        if !a.busy {
            for t in &threads {
                t.unpark();
            }
        }
    }
    let seconds = began.elapsed().as_secs_f64();
    let publisher_cpu_s = match (publisher_before, thread_cpu_s()) {
        (Some(b), Some(a)) => a - b,
        _ => f64::NAN,
    };
    // Every waiter takes what was published (the fact waited on), then they stop.
    // As long as a broker's subscribers get (`--drain`): what isn't taken by then is undelivered.
    let deadline = Instant::now() + Duration::from_secs(a.drain);
    while shared
        .caught_up
        .iter()
        .any(|c| c.load(Ordering::Acquire) < schedule.offered())
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    shared.stop.store(true, Ordering::Release);
    for t in &threads {
        t.unpark();
    }
    let mut seen = Seen::new();
    let mut follows = Vec::new();
    for h in handles {
        let (s, f) = h.join().unwrap();
        seen.merge(&s);
        follows.push(f);
    }
    let cpu_s = match (cpu_before, hyper_measure::usage::this().ok()) {
        (Some(b), Some(a)) => a.since(&b).cpu_ns() as f64 / 1e9,
        _ => f64::NAN,
    };
    let t = tally(&mut seen, &follows, &schedule);
    Run {
        system: System::Wake,
        busy: a.busy,
        rate: a.rate,
        subscribers: a.subscribers,
        subscriptions: waiters,
        workers: waiters,
        published: schedule.offered(),
        unoffered: schedule.unoffered,
        publish_errors: 0,
        seconds,
        window_s: a.seconds,
        seen,
        undelivered: t.undelivered,
        silent: t.silent,
        lags: t.lags,
        doubled: t.doubled,
        kept_up: t.kept_up,
        cpu_s,
        publisher_cpu_s,
        broker_cpu_s: None,
        load_before,
        load_after: hyper_measure::usage::load(),
        profile: pending.profile(),
    }
}
