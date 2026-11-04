//! A request waiting on a compute job beside busy work in the server's own cgroup (goal 3, plan 01a1a59c2a8e
//! step 1): the pool at the rank kiln gives it (`Rank::Below`) keeps a share of the CPU against threads at the
//! process's own priority, so a job a request waits on ends within what that share derives, where the idle
//! rank's share all but stops it (`benches/compute_wait.rs`: a 5 ms job beside one spinning thread took 6.1 s
//! at the idle rank).
//!
//! On Linux, the shard, the pool's one thread and a thread spinning at the process's priority share the core the
//! test starts on (pinned; the spinner is in the same process, so the same cgroup). A job of `COST` of CPU,
//! asked for from the shard: first with nothing else on the core (the calm wait), then beside the spinner. The
//! pool's thread runs the batch policy at a nice value 10 above the spinner's, so beside it the job's thread
//! gets `w_pool / (w_pool + w_spinner)` of the core, each weight its nice value's in the kernel's table
//! (kernel/sched/core.c, `sched_prio_to_weight`); and EEVDF keeps each task's lag, what it was owed and not
//! given, within one request, a slice [Stoica et al., "Earliest Eligible Virtual Deadline First", 1995; the
//! kernel's fair class], for each of the two threads. So the job ends within `(calm + 2 × slice) × (1 +
//! w_spinner / w_pool)`, the slice the kernel's base slice, 0.75 ms × (1 + ⌊log₂ min(CPUs, 8)⌋)
//! (kernel/sched/fair.c, `sysctl_sched_base_slice` and its logarithmic scaling). The median of ten waits must.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cognitive_complexity,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use hyper_rt::compute::{self, Config, Rank};
    use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};

    /// The job's CPU: about a session snapshot's (perf/api-pass's session_feed bench: 6 ms).
    const COST: Duration = Duration::from_millis(5);
    /// Jobs a phase: the median of ten.
    const JOBS: usize = 10;

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

    /// `COST` of CPU on the calling thread's own clock.
    fn work() -> u64 {
        let start = cpu();
        let mut x = 1;
        while cpu().saturating_sub(start) < COST {
            x = churn(x);
        }
        x
    }

    /// The kernel's weight for a nice value (kernel/sched/core.c, `sched_prio_to_weight`, nice -20 to 19).
    fn weight(nice: i32) -> f64 {
        const WEIGHTS: [u32; 40] = [
            88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904, 3906, 3121, 2501,
            1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87, 70, 56, 45, 36, 29, 23, 18, 15,
        ];
        f64::from(WEIGHTS[usize::try_from(nice + 20).unwrap()])
    }

    /// A Linux priority's nice value.
    fn nice_of(priority: compute::Priority) -> i32 {
        match priority {
            compute::Priority::Idle { nice } | compute::Priority::Batch { nice } | compute::Priority::Other { nice } => nice,
            other => panic!("not a Linux priority: {other:?}"),
        }
    }

    /// The median wait of `JOBS` jobs asked for from the shard, one at a time.
    fn median_wait(rt: &mut LocalRuntime) -> Duration {
        let mut waits = rt
            .block_on(async {
                let mut waits = Vec::with_capacity(JOBS);
                for _ in 0..JOBS {
                    let asked = Instant::now();
                    compute::run(work).unwrap().await.unwrap();
                    waits.push(asked.elapsed());
                    hyper_rt::futures::sleep(2_000_000).await.unwrap();
                }
                waits
            })
            .unwrap();
        waits.sort_unstable();
        waits[waits.len() / 2]
    }

    #[test]
    fn a_job_a_request_waits_on_beside_busy_work_in_its_cgroup_ends_within_its_share() {
        let here = rustix::thread::sched_getcpu();
        let mut one = rustix::thread::CpuSet::new();
        one.set(here);
        rustix::thread::sched_setaffinity(None, &one).unwrap();
        let started = compute::start(Config {
            workers: 1,
            queued: 4,
            piece_ns: 250_000,
            rank: Rank::Below,
        })
        .unwrap();
        let mut rt = LocalRuntime::new(&config()).unwrap();
        let calm = median_wait(&mut rt);
        let stop = Arc::new(AtomicBool::new(false));
        let spinner = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x = 1;
                while !stop.load(Ordering::Relaxed) {
                    x = churn(x);
                }
            })
        };
        let busy = median_wait(&mut rt);
        stop.store(true, Ordering::Relaxed);
        spinner.join().unwrap();
        compute::stop().unwrap();
        let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get).min(8);
        let slice = Duration::from_micros(750) * (1 + cpus.ilog2());
        let (spinner_nice, pool_nice) = (nice_of(compute::thread_priority()), nice_of(started.priority.unwrap()));
        let ratio = weight(spinner_nice) / weight(pool_nice);
        let bound = (calm + slice * 2).mul_f64(1.0 + ratio);
        eprintln!(
            "CPU {here}, the pool at nice {pool_nice} beside a spinner at nice {spinner_nice}: a {COST:?} job waited {calm:?} calm and {busy:?} beside the spinner in its cgroup; derived bound {bound:?}"
        );
        assert!(busy <= bound, "beside busy work in its cgroup a {COST:?} job waited {busy:?}, past its share's {bound:?}");
    }
}
