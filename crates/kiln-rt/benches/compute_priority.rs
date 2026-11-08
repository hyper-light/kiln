//! What a CPU-heavy pool's priority does to a shard beside it (kiln's goal 3, plan 01a1a59c2a8e step 1): a
//! shard's request loop (a task sleeping a millisecond on the shard's timers and noting how late it woke, as
//! a server's shard-health probe does) while as many threads as the compute pool would have
//! (`compute::workers_for` the process's grant) spin on CPU work at each priority policy in turn. Arms
//! alternate within each round, in reverse order every other round, so drift on the host falls on all.
//!
//! Arms: `calm` (no load), `plain` (the threads at the process's priority), and per OS the policies the pool
//! could take: Linux `nice+10`, `batch`, `Pool` (the pool's idle rank, `compute::lower_current_thread`) and
//! `idle` (`SCHED_IDLE`); macOS `Pool` (its idle rank, background) and `background`. Each arm's line: the medians over
//! rounds of each round's p50, p99 and p99.9 lateness, and the range of its p99s.
//!
//! `cargo bench -p hyper-rt --bench compute_priority -- [rounds] [samples]` (defaults 7 and 500). With
//! `ONE_CORE=n` (Linux) the shard and `n` load threads share the one core the bench starts on: what each
//! policy does where the pool and a shard want the same CPU, apart from the host's other work.
#![allow(
    unsafe_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    clippy::missing_panics_doc
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hyper_rt::compute::{self, Grant, Rank};
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};

const PERIOD: Duration = Duration::from_millis(1);

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 250_000,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    Calm,
    Plain,
    #[cfg(target_os = "linux")]
    Nice,
    #[cfg(target_os = "linux")]
    Batch,
    /// The pool's own lowering.
    Pool,
    #[cfg(target_os = "linux")]
    Idle,
    #[cfg(target_os = "macos")]
    Background,
}

fn arms() -> Vec<Arm> {
    let mut arms = vec![Arm::Calm, Arm::Plain];
    #[cfg(target_os = "linux")]
    arms.extend([Arm::Nice, Arm::Batch]);
    arms.push(Arm::Pool);
    #[cfg(target_os = "linux")]
    arms.push(Arm::Idle);
    #[cfg(target_os = "macos")]
    arms.push(Arm::Background);
    arms
}

#[cfg(target_os = "linux")]
fn set_policy(policy: libc::c_int) {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: a live sched_param read for the call's duration; pid 0 is the calling thread.
    let done = unsafe { libc::sched_setscheduler(0, policy, &raw const param) };
    assert_eq!(done, 0, "sched_setscheduler refused");
}

/// A load thread started by a shard given a short slice inherits it: its own goes back to the default.
#[cfg(target_os = "linux")]
fn default_slice() {
    if std::env::var("SHARD_SLICE_NS").is_err() {
        return;
    }
    let me = rustix::thread::gettid();
    let nice = rustix::process::getpriority_process(Some(me)).unwrap();
    let attr = libc::sched_attr {
        size: std::mem::size_of::<libc::sched_attr>() as u32,
        sched_policy: libc::SCHED_OTHER as u32,
        sched_flags: 0,
        sched_nice: nice,
        sched_priority: 0,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
    };
    // SAFETY: a live sched_attr of its stated size, read for the call; pid 0 is the calling thread.
    let _ = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) };
}

/// Puts the calling (load) thread at the arm's priority.
fn take_priority(arm: Arm) {
    #[cfg(target_os = "linux")]
    default_slice();
    match arm {
        Arm::Calm | Arm::Plain => {}
        Arm::Pool => {
            // `POOL_RANK=below`: the pool's rank below the shards that keeps a share of the CPU.
            let rank = if std::env::var("POOL_RANK").is_ok_and(|r| r == "below") { Rank::Below } else { Rank::Idle };
            compute::lower_current_thread(rank).unwrap();
            // `POOL_SLICE_NS=n` (Linux): the pool's threads ask for an `n` ns slice too.
            #[cfg(target_os = "linux")]
            if let Some(slice) = std::env::var("POOL_SLICE_NS").ok().and_then(|n| n.parse::<u64>().ok()) {
                let me = rustix::thread::gettid();
                let nice = rustix::process::getpriority_process(Some(me)).unwrap();
                // SAFETY: pid 0 is the calling thread; no pointer passes.
                let policy = unsafe { libc::sched_getscheduler(0) };
                let attr = libc::sched_attr {
                    size: std::mem::size_of::<libc::sched_attr>() as u32,
                    sched_policy: policy as u32,
                    sched_flags: 0,
                    sched_nice: nice,
                    sched_priority: 0,
                    sched_runtime: slice,
                    sched_deadline: 0,
                    sched_period: 0,
                };
                // SAFETY: a live sched_attr of its stated size, read for the call; pid 0 is the calling thread.
                assert_eq!(unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) }, 0);
            }
        }
        #[cfg(target_os = "linux")]
        Arm::Nice => {
            let me = rustix::thread::gettid();
            let now = rustix::process::getpriority_process(Some(me)).unwrap();
            rustix::process::setpriority_process(Some(me), (now + 10).min(19)).unwrap();
        }
        #[cfg(target_os = "linux")]
        Arm::Batch => set_policy(libc::SCHED_BATCH),
        #[cfg(target_os = "linux")]
        Arm::Idle => set_policy(libc::SCHED_IDLE),
        #[cfg(target_os = "macos")]
        Arm::Background => {
            // SAFETY: no pointer passes; only the calling thread's class changes.
            let done = unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0) };
            assert_eq!(done, 0);
        }
    }
}

fn churn(seed: u64) -> u64 {
    let mut x = seed | 1;
    for _ in 0..1_000 {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
    }
    x
}

/// One round of an arm: the shard's lateness percentiles (p50, p99, p99.9), nanoseconds.
fn measure(rt: &mut LocalRuntime, arm: Arm, threads: usize, samples: usize) -> [u64; 3] {
    let stop = Arc::new(AtomicBool::new(false));
    let load: Vec<_> = if arm == Arm::Calm {
        Vec::new()
    } else {
        (0..threads)
            .map(|i| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    take_priority(arm);
                    let mut x = i as u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = churn(x);
                    }
                })
            })
            .collect()
    };
    std::thread::sleep(Duration::from_millis(50));
    let mut late: Vec<u64> = rt
        .block_on(async move {
            let mut late = Vec::with_capacity(samples);
            for _ in 0..samples {
                let due = Instant::now() + PERIOD;
                hyper_rt::futures::sleep(PERIOD.as_nanos() as u64).await.unwrap();
                late.push(Instant::now().saturating_duration_since(due).as_nanos() as u64);
            }
            late
        })
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    for thread in load {
        thread.join().unwrap();
    }
    late.sort_unstable();
    let at = |q: usize| late[(late.len() * q / 1000).min(late.len() - 1)];
    [at(500), at(990), at(999)]
}

fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1).filter(|a| !a.starts_with('-'));
    let rounds: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(7);
    let samples: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(500);
    let grant = Grant::of_process();
    let threads = compute::workers_for(&grant, 1);
    #[cfg(target_os = "linux")]
    let threads = match std::env::var("ONE_CORE").ok().and_then(|n| n.parse::<usize>().ok()) {
        Some(n) => {
            let here = rustix::thread::sched_getcpu();
            let mut one = rustix::thread::CpuSet::new();
            one.set(here);
            rustix::thread::sched_setaffinity(None, &one).unwrap();
            println!("one core: CPU {here}, shared by the shard and the load");
            n
        }
        None => threads,
    };
    // macOS: `SHARD_QOS=default` puts the shard (this thread, and the load threads it starts) at the default
    // class, as a server runs, where a runner starts its commands at utility.
    #[cfg(target_os = "macos")]
    if let Ok(q) = std::env::var("SHARD_QOS") {
        let class = if q == "interactive" { libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE } else { libc::qos_class_t::QOS_CLASS_DEFAULT };
        // SAFETY: no pointer passes; only the calling thread's class changes.
        let done = unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
        println!("the shard asked for the {q} class: {done}");
    }
    // Linux: `SHARD_SLICE_NS=n` asks EEVDF for an `n` ns slice for the shard (sched_attr's sched_runtime on
    // SCHED_OTHER, Linux 6.12+): a waking task with a shorter slice than the running one's preempts it.
    #[cfg(target_os = "linux")]
    if let Some(slice) = std::env::var("SHARD_SLICE_NS").ok().and_then(|n| n.parse::<u64>().ok()) {
        let me = rustix::thread::gettid();
        let nice = rustix::process::getpriority_process(Some(me)).unwrap();
        let attr = libc::sched_attr {
            size: std::mem::size_of::<libc::sched_attr>() as u32,
            sched_policy: libc::SCHED_OTHER as u32,
            sched_flags: 0,
            sched_nice: nice,
            sched_priority: 0,
            sched_runtime: slice,
            sched_deadline: 0,
            sched_period: 0,
        };
        // SAFETY: a live sched_attr of its stated size, read for the call; pid 0 is the calling thread.
        let done = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) };
        println!("the shard asked for a {slice} ns slice: {done}");
    }
    println!("grant: {grant}; {threads} load threads; the shard at {}", compute::thread_priority());
    let mut rt = LocalRuntime::new(&config()).unwrap();
    let arms = arms();
    let mut results: Vec<Vec<[u64; 3]>> = vec![Vec::new(); arms.len()];
    for round in 0..rounds {
        let order: Vec<usize> = if round % 2 == 0 { (0..arms.len()).collect() } else { (0..arms.len()).rev().collect() };
        for i in order {
            results[i].push(measure(&mut rt, arms[i], threads, samples));
        }
    }
    println!("{:<14} {:>10} {:>10} {:>10} {:>24}", "arm", "p50 us", "p99 us", "p99.9 us", "p99 range us");
    for (arm, rows) in arms.iter().zip(&results) {
        let column = |k: usize| median(rows.iter().map(|r| r[k]).collect());
        let p99s: Vec<u64> = rows.iter().map(|r| r[1]).collect();
        println!(
            "{:<14} {:>10.1} {:>10.1} {:>10.1} {:>11.1} .. {:>9.1}",
            format!("{arm:?}"),
            column(0) as f64 / 1e3,
            column(1) as f64 / 1e3,
            column(2) as f64 / 1e3,
            *p99s.iter().min().unwrap() as f64 / 1e3,
            *p99s.iter().max().unwrap() as f64 / 1e3,
        );
    }
}
