//! Movable tasks (kiln's goal 7, plan 01a1a59c2aea step 2; `hyper_rt::movable`): a shard held by one long poll has
//! its queued movable tasks finished by its idle siblings; a task woken from off the runtime runs on whichever shard
//! looks first; a movable task that yields, or uses its share, goes behind the shard's other work (hyper-rt's fair
//! yield kept); and under many threads waking many tasks at once, every task runs to its end exactly once.
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
    clippy::indexing_slicing,
    clippy::cognitive_complexity
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use hyper_rt::coop::Class;
use hyper_rt::runtime::{Runtime, RuntimeConfig};

/// Shape: how long a test waits for what it expects, far past anything a loaded host takes.
const WAIT: Duration = Duration::from_secs(20);

/// A runtime of `shards` shards with a fixed quantum of `quantum_ns` and a batch of 64 (each shard's movable queue
/// bound): the watchdog's limit is 64 quanta.
fn config(shards: u16, quantum_ns: u64) -> RuntimeConfig {
    RuntimeConfig {
        shards,
        tasks_per_shard: 512,
        timers_per_shard: 512,
        interests_per_shard: 1024,
        ring_entries: 256,
        step_budget_ns: quantum_ns,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
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

/// Pending once, its waker handed to `out`; ready once `go` is set.
struct Signal {
    go: Arc<AtomicBool>,
    out: Option<Sender<Waker>>,
}

impl Future for Signal {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.go.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        if let Some(out) = self.out.take() {
            out.send(cx.waker().clone()).unwrap();
        }
        Poll::Pending
    }
}

/// Pending `n` times, waking itself each time: a task that yields `n` times.
struct Yields(u32);

impl Future for Yields {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            return Poll::Ready(());
        }
        self.0 -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Step 2's acceptance: with one shard busy, its queued requests finish on other shards. Do: on shard A of four, a
/// task queues 32 movable tasks on A and then holds A, spinning, until they have all run (or the test's wait ends).
/// The watchdog's limit is 64 quanta of 50 ms, past the wait, so only idle siblings taking from A's queue can run
/// them. Expect: all 32 run while A is held, none of them on A, the siblings having stolen them, the watchdog having
/// handed none off.
#[test]
fn g7_s2_a_busy_shards_queued_movable_tasks_finish_on_other_shards() {
    let rt = Runtime::start(&config(4, 50_000_000)).unwrap();
    let a = rt.shard_ids()[0];
    const TASKS: usize = 32;
    let done = Arc::new(AtomicUsize::new(0));
    let (ran, ran_on) = channel::<u16>();
    let (held, held_for) = channel::<Duration>();
    let done_in = Arc::clone(&done);
    rt.spawn_on(a, async move {
        for _ in 0..TASKS {
            let (ran, done) = (ran.clone(), Arc::clone(&done_in));
            hyper_rt::movable::spawn(Class::Normal, async move {
                ran.send(hyper_rt::futures::shard_id().unwrap().0).unwrap();
                done.fetch_add(1, Ordering::AcqRel);
            })
            .unwrap();
        }
        // Held: no await until the queued tasks have run elsewhere (or the wait ends).
        let began = Instant::now();
        while done_in.load(Ordering::Acquire) < TASKS && began.elapsed() < WAIT {
            std::hint::spin_loop();
        }
        held.send(began.elapsed()).unwrap();
    })
    .unwrap();
    let held_for = held_for.recv_timeout(WAIT * 2).unwrap();
    assert_eq!(done.load(Ordering::Acquire), TASKS, "every queued task ran while A was held ({held_for:?})");
    let shards: Vec<u16> = ran_on.try_iter().collect();
    assert_eq!(shards.len(), TASKS);
    assert!(shards.iter().all(|s| *s != a.0), "none ran on the held shard: {shards:?}");
    let stats = rt.movable_stats();
    assert_eq!(stats.spawned, TASKS as u64);
    assert!(stats.stolen >= 1, "{stats:?}");
    assert_eq!(stats.handed_off, 0, "the watchdog's limit was never reached: {stats:?}");
    rt.shutdown().unwrap();
}

/// The watchdog's part: a shard held past its limit has its queued movable tasks handed to the shared queues. Do:
/// on shard A of two, a task queues 16 movable tasks and holds A; the quantum is 1 ms, so the limit is 64 ms.
/// Expect: all 16 run off A while A is held, whether its sibling stole them or the watchdog handed them off.
#[test]
fn g7_s2_a_held_shards_queued_movable_tasks_run_elsewhere_whoever_moves_them() {
    let rt = Runtime::start(&config(2, 1_000_000)).unwrap();
    let a = rt.shard_ids()[0];
    const TASKS: usize = 16;
    let done = Arc::new(AtomicUsize::new(0));
    let (ran, ran_on) = channel::<u16>();
    let done_in = Arc::clone(&done);
    let (held, held_for) = channel::<Duration>();
    rt.spawn_on(a, async move {
        for _ in 0..TASKS {
            let (ran, done) = (ran.clone(), Arc::clone(&done_in));
            hyper_rt::movable::spawn(Class::Critical, async move {
                ran.send(hyper_rt::futures::shard_id().unwrap().0).unwrap();
                done.fetch_add(1, Ordering::AcqRel);
            })
            .unwrap();
        }
        let began = Instant::now();
        while done_in.load(Ordering::Acquire) < TASKS && began.elapsed() < WAIT {
            std::hint::spin_loop();
        }
        held.send(began.elapsed()).unwrap();
    })
    .unwrap();
    let held_for = held_for.recv_timeout(WAIT * 2).unwrap();
    assert_eq!(done.load(Ordering::Acquire), TASKS, "ran while A was held ({held_for:?})");
    assert!(ran_on.try_iter().all(|s| s != a.0));
    let stats = rt.movable_stats();
    assert!(stats.stolen + stats.handed_off >= 1, "{stats:?}");
    rt.shutdown().unwrap();
}

/// A movable task woken from a thread off the runtime goes on the shared queue and a parked shard is kicked to run
/// it. Do: a movable task spawned from off the runtime waits on a signal whose waker it hands to the test thread;
/// the test sets the signal and wakes it. Expect: the task ends, through the shared queue.
#[test]
fn g7_s2_a_wake_from_off_the_runtime_runs_the_task_on_a_shard() {
    let rt = Runtime::start(&config(2, 1_000_000)).unwrap();
    let go = Arc::new(AtomicBool::new(false));
    let (waker_out, waker_in) = channel::<Waker>();
    let (ended, ended_on) = channel::<u16>();
    let signal = Signal { go: Arc::clone(&go), out: Some(waker_out) };
    rt.spawn_movable(Class::Normal, async move {
        signal.await;
        ended.send(hyper_rt::futures::shard_id().unwrap().0).unwrap();
    });
    let waker = waker_in.recv_timeout(WAIT).unwrap();
    go.store(true, Ordering::Release);
    waker.wake();
    let on = ended_on.recv_timeout(WAIT).unwrap();
    assert!(rt.shard_ids().iter().any(|s| s.0 == on));
    let stats = rt.movable_stats();
    assert!(stats.from_shared >= 2, "spawned and woken off the runtime, taken from the shared queue twice: {stats:?}");
    rt.shutdown().unwrap();
}

/// hyper-rt's fair yield, kept for movable tasks (research f13): on one shard, a movable task that yields a million
/// times does not keep 32 others, each yielding a few times, or a pinned task's timer, from finishing first. Do:
/// spawn the yielder, then the others and a pinned task that sleeps 5 ms. Expect: the others and the timer finish
/// while the yielder still has most of its yields to go.
#[test]
fn g7_s2_a_movable_tasks_yield_is_one_turn_of_a_round() {
    let rt = Runtime::start(&config(1, 250_000)).unwrap();
    let shard = rt.shard_ids()[0];
    let others_done = Arc::new(AtomicUsize::new(0));
    let timer_fired = Arc::new(AtomicBool::new(false));
    let yielder_done = Arc::new(AtomicBool::new(false));
    let (o, t, y) = (Arc::clone(&others_done), Arc::clone(&timer_fired), Arc::clone(&yielder_done));
    rt.spawn_on(shard, async move {
        let y2 = Arc::clone(&y);
        hyper_rt::movable::spawn(Class::Normal, async move {
            Yields(1_000_000).await;
            y2.store(true, Ordering::Release);
        })
        .unwrap();
        for _ in 0..32 {
            let o = Arc::clone(&o);
            hyper_rt::movable::spawn(Class::Normal, async move {
                Yields(3).await;
                o.fetch_add(1, Ordering::AcqRel);
            })
            .unwrap();
        }
        let t2 = Arc::clone(&t);
        hyper_rt::futures::spawn_detached(async move {
            let _ = hyper_rt::futures::sleep(5_000_000).await;
            t2.store(true, Ordering::Release);
        })
        .unwrap();
    })
    .unwrap();
    wait_for("the others and the timer", || others_done.load(Ordering::Acquire) == 32 && timer_fired.load(Ordering::Acquire));
    assert!(!yielder_done.load(Ordering::Acquire), "the yielder ran its million yields first");
    rt.shutdown().unwrap();
}

/// A movable task's charges are its own (`hyper_rt::coop`): one that loops on an operation that is always ready
/// yields at its share, so a pinned task's timer due on its shard fires while it loops. Do: on one shard, a pinned
/// task sleeps 5 ms; a movable task charges its budget and spins 10 µs a round until the timer has fired. Expect:
/// the timer fires within the wait.
#[test]
fn g7_s2_a_movable_task_yields_at_its_share() {
    let rt = Runtime::start(&config(1, 250_000)).unwrap();
    let shard = rt.shard_ids()[0];
    let fired = Arc::new(AtomicBool::new(false));
    let rounds = Arc::new(AtomicU64::new(0));
    let (f, r) = (Arc::clone(&fired), Arc::clone(&rounds));
    rt.spawn_on(shard, async move {
        let f2 = Arc::clone(&f);
        hyper_rt::futures::spawn_detached(async move {
            let _ = hyper_rt::futures::sleep(5_000_000).await;
            f2.store(true, Ordering::Release);
        })
        .unwrap();
        hyper_rt::movable::spawn(Class::Normal, async move {
            let began = Instant::now();
            while !f.load(Ordering::Acquire) && began.elapsed() < WAIT {
                hyper_rt::coop::consume_budget().await;
                let until = Instant::now() + Duration::from_micros(10);
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
                r.fetch_add(1, Ordering::Relaxed);
            }
        })
        .unwrap();
    })
    .unwrap();
    wait_for("the timer to fire beside the movable loop", || fired.load(Ordering::Acquire));
    rt.shutdown().unwrap();
}

/// A movable task's class is its own, and it can change it (`hyper_rt::coop::class`, `set_class`).
#[test]
fn g7_s2_a_movable_tasks_class_is_its_own() {
    let rt = Runtime::start(&config(1, 1_000_000)).unwrap();
    let (said, heard) = channel::<(Option<Class>, Option<Class>, Option<Class>)>();
    rt.spawn_on(rt.shard_ids()[0], async move {
        hyper_rt::movable::spawn(Class::Critical, async move {
            let first = hyper_rt::coop::class();
            let had = hyper_rt::coop::set_class(Class::Background);
            Yields(1).await;
            said.send((first, had, hyper_rt::coop::class())).unwrap();
        })
        .unwrap();
    })
    .unwrap();
    let (first, had, after) = heard.recv_timeout(WAIT).unwrap();
    assert_eq!(first, Some(Class::Critical));
    assert_eq!(had, Some(Class::Critical));
    assert_eq!(after, Some(Class::Background));
    rt.shutdown().unwrap();
}

/// The queues under contention (a simulation of the races, with real threads): four shards and four threads of the
/// test's own wake 2,000 movable tasks spawned from shards and from off the runtime, each through 8 waits that
/// alternate between waking itself in its poll and handing its waker to one of the threads, which may wake it
/// before the poll has even returned. Expect: every task runs to its end exactly once, none lost.
#[test]
fn g7_s2_every_movable_task_ends_exactly_once_under_concurrent_wakes() {
    const TASKS: usize = 2_000;
    const WAITS: u32 = 8;
    const THREADS: usize = 4;
    let rt = Runtime::start(&config(4, 250_000)).unwrap();
    let ended: Arc<Vec<AtomicUsize>> = Arc::new((0..TASKS).map(|_| AtomicUsize::new(0)).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let mut outs = Vec::new();
    let mut threads = Vec::new();
    for _ in 0..THREADS {
        let (out, wakes) = channel::<Waker>();
        outs.push(out);
        let stop = Arc::clone(&stop);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if let Ok(waker) = wakes.recv_timeout(Duration::from_millis(5)) {
                    waker.wake();
                }
            }
        }));
    }
    /// A task that waits `left` times: waking itself, or handing its waker to `out`, in turn.
    struct Waits {
        left: u32,
        out: Sender<Waker>,
    }
    impl Future for Waits {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.left == 0 {
                return Poll::Ready(());
            }
            self.left -= 1;
            if self.left.is_multiple_of(2) {
                cx.waker().wake_by_ref();
            } else {
                self.out.send(cx.waker().clone()).unwrap();
            }
            Poll::Pending
        }
    }
    let task = |i: usize, ended: Arc<Vec<AtomicUsize>>, out: Sender<Waker>| async move {
        Waits { left: WAITS, out }.await;
        ended[i].fetch_add(1, Ordering::AcqRel);
    };
    // Half from off the runtime, half from a shard.
    for i in (0..TASKS).step_by(2) {
        rt.spawn_movable(Class::Normal, task(i, Arc::clone(&ended), outs[i % THREADS].clone()));
    }
    let (spawned, all_spawned) = channel::<()>();
    let (e, o) = (Arc::clone(&ended), outs.clone());
    rt.spawn_on(rt.shard_ids()[1], async move {
        for i in (1..TASKS).step_by(2) {
            let class = if i % 3 == 0 { Class::Critical } else { Class::Normal };
            hyper_rt::movable::spawn(class, task(i, Arc::clone(&e), o[i % THREADS].clone())).unwrap();
        }
        spawned.send(()).unwrap();
    })
    .unwrap();
    all_spawned.recv_timeout(WAIT).unwrap();
    wait_for("every task to end", || ended.iter().all(|n| n.load(Ordering::Acquire) >= 1));
    stop.store(true, Ordering::Release);
    for t in threads {
        t.join().unwrap();
    }
    assert!(ended.iter().all(|n| n.load(Ordering::Acquire) == 1), "a task ended twice");
    let stats = rt.movable_stats();
    assert_eq!(stats.spawned, TASKS as u64);
    assert!(stats.polls >= (TASKS as u64) * u64::from(WAITS + 1), "{stats:?}");
    rt.shutdown().unwrap();
}

/// On a runtime with no movable queues (a local one), a movable spawn is a pinned task of its class.
#[test]
fn g7_s2_a_movable_spawn_on_a_local_runtime_is_pinned() {
    let mut local = hyper_rt::runtime::LocalRuntime::new(&config(1, 1_000_000)).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&ran);
    let out = local
        .block_on(async move {
            assert!(!hyper_rt::movable::available());
            hyper_rt::movable::spawn(Class::Critical, async move {
                assert_eq!(hyper_rt::coop::class(), Some(Class::Critical));
                r.store(true, Ordering::Release);
            })
            .map_err(|e| format!("{e}"))?;
            Yields(2).await;
            Ok::<(), String>(())
        })
        .unwrap();
    out.unwrap();
    assert!(ran.load(Ordering::Acquire));
}
