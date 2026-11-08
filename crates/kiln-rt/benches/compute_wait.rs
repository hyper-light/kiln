//! How long a request waits on a compute job when busy work shares the server's cgroup (kiln's goal 3,
//! plan 01a1a59c2a8e step 1): a shard asks a worker thread, at each priority policy the pool could take,
//! for a job of a fixed CPU cost, and times it from the ask to the answer; the same, with `H` threads at the
//! process's own priority spinning beside them in the same process (so the same cgroup), for H of 0, 1 and 2.
//! On Linux everything shares the one core the bench starts on: the worker's share of it is its policy's
//! weight against the spinners' (sched(7); the kernel's weights: 1024 at nice 0, 110 at nice 10, 3 under
//! `SCHED_IDLE`), so a job of CPU cost `C` should take about `C × (1 + H × 1024 / w)`, which each line
//! prints beside what was measured. Arms alternate round by round.
//!
//! `cargo bench -p hyper-rt --bench compute_wait -- [rounds] [jobs] [job_ms]` (defaults 3, 20 and 5).
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
    clippy::cast_precision_loss,
    clippy::cognitive_complexity,
    clippy::missing_panics_doc
)]

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
    use hyper_rt::sync::oneshot;

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

    #[derive(Clone, Copy, Debug)]
    enum Policy {
        Plain,
        Nice,
        Batch,
        Idle,
    }

    impl Policy {
        /// The kernel's weight for a thread at it (kernel/sched/core.c, sched_prio_to_weight; WEIGHT_IDLEPRIO).
        fn weight(self) -> f64 {
            match self {
                Policy::Plain => 1024.0,
                Policy::Nice | Policy::Batch => 110.0,
                Policy::Idle => 3.0,
            }
        }

        fn take(self) {
            let me = rustix::thread::gettid();
            let now = rustix::process::getpriority_process(Some(me)).unwrap();
            let (policy, nice) = match self {
                Policy::Plain => return,
                Policy::Nice => (libc::SCHED_OTHER, now + 10),
                Policy::Batch => (libc::SCHED_BATCH, now + 10),
                Policy::Idle => (libc::SCHED_IDLE, now),
            };
            let param = libc::sched_param { sched_priority: 0 };
            // SAFETY: a live sched_param read for the call's duration; pid 0 is the calling thread.
            assert_eq!(unsafe { libc::sched_setscheduler(0, policy, &raw const param) }, 0);
            rustix::process::setpriority_process(Some(me), nice.min(19)).unwrap();
        }
    }

    fn churn(seed: u64) -> u64 {
        let mut x = seed | 1;
        for _ in 0..1_000 {
            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
        }
        x
    }

    /// This thread's CPU time.
    fn cpu() -> Duration {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
    }

    /// CPU work for `cost` on this thread's own clock (so time it spends off the CPU doesn't count).
    fn work(cost: Duration) -> u64 {
        let start = cpu();
        let mut x = 1;
        while cpu().saturating_sub(start) < cost {
            x = churn(x);
        }
        x
    }

    /// One arm: `jobs` asks of `cost` each to a worker at `policy`, beside `hogs` spinners; each ask's wait.
    fn arm(rt: &mut LocalRuntime, policy: Policy, hogs: usize, jobs: usize, cost: Duration) -> Vec<Duration> {
        let stop = Arc::new(AtomicBool::new(false));
        let spinners: Vec<_> = (0..hogs)
            .map(|i| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut x = i as u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = churn(x);
                    }
                })
            })
            .collect();
        let (ask, asked) = std::sync::mpsc::channel::<hyper_rt::sync::OneshotSender<u64>>();
        let worker = std::thread::spawn(move || {
            policy.take();
            while let Ok(answer) = asked.recv() {
                let _ = answer.send(work(cost));
            }
        });
        std::thread::sleep(Duration::from_millis(30));
        let waits = rt
            .block_on(async move {
                let mut waits = Vec::with_capacity(jobs);
                for _ in 0..jobs {
                    let (tx, rx) = oneshot().unwrap();
                    let asked_at = Instant::now();
                    ask.send(tx).unwrap();
                    rx.await.unwrap();
                    waits.push(asked_at.elapsed());
                    hyper_rt::futures::sleep(2_000_000).await.unwrap();
                }
                waits
            })
            .unwrap();
        stop.store(true, Ordering::Relaxed);
        for s in spinners {
            s.join().unwrap();
        }
        worker.join().unwrap();
        waits
    }

    pub(super) fn main() {
        let mut args = std::env::args().skip(1).filter(|a| !a.starts_with('-'));
        let rounds: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(3);
        let jobs: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(20);
        let cost = Duration::from_millis(args.next().and_then(|a| a.parse().ok()).unwrap_or(5));
        let here = rustix::thread::sched_getcpu();
        let mut one = rustix::thread::CpuSet::new();
        one.set(here);
        rustix::thread::sched_setaffinity(None, &one).unwrap();
        println!("one core: CPU {here}; a job of {cost:?} of CPU; the shard and the spinners at the process's priority");
        let mut rt = LocalRuntime::new(&config()).unwrap();
        let arms: Vec<(Policy, usize)> = [Policy::Plain, Policy::Nice, Policy::Batch, Policy::Idle]
            .into_iter()
            .flat_map(|p| [0usize, 1, 2].map(|h| (p, h)))
            .collect();
        let mut waits: Vec<Vec<Duration>> = vec![Vec::new(); arms.len()];
        for round in 0..rounds {
            let order: Vec<usize> = if round % 2 == 0 { (0..arms.len()).collect() } else { (0..arms.len()).rev().collect() };
            for i in order {
                let (p, h) = arms[i];
                waits[i].extend(arm(&mut rt, p, h, jobs, cost));
            }
        }
        println!("{:<8} {:>5} {:>10} {:>10} {:>10} {:>12}", "worker", "hogs", "p50 ms", "p99 ms", "most ms", "derived ms");
        for ((p, h), w) in arms.iter().zip(waits.iter_mut()) {
            w.sort_unstable();
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            let at = |q: f64| ms(w[((w.len() as f64 * q) as usize).min(w.len() - 1)]);
            let derived = ms(cost) * (1.0 + *h as f64 * 1024.0 / p.weight());
            println!("{:<8} {:>5} {:>10.1} {:>10.1} {:>10.1} {:>12.1}", format!("{p:?}"), h, at(0.5), at(0.99), ms(*w.last().unwrap()), derived);
        }
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    linux::main();
}
