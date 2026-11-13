//! Joint plan step 14 (AC-T7.4.1; `docs/design/learned-charge.md` C1): the probe whose result a Windows node's charge
//! window derives from. A job's CPU hard cap holds per scheduling interval, which Windows doesn't document
//! (`JOBOBJECT_CPU_RATE_CONTROL_INFORMATION`), so `WinRuntime::charge_window` is refused, typed, until this measures it
//! on the host.
//!
//! Each run starts a busy loop (one spinning thread per logical processor but one, at `BELOW_NORMAL_PRIORITY_CLASS`) in a
//! job of its own, its demand past every cap measured, and samples the job's CPU time
//! (`JobObjectBasicAndIoAccountingInformation`'s `BasicInfo`, which is `JobObjectBasicAccountingInformation`) every
//! 1 ms, each sample at its own deadline (a high-resolution waitable timer's, on a sampler at
//! `THREAD_PRIORITY_TIME_CRITICAL`, so the spinners never hold it back) and stamped when it was read, a deadline already
//! past read at once and the next taken from the clock. A run is bounded by its own samples: it ends once they span
//! 10 s, however long each read took. Each run says its cadence (n, p50, p99 and max of the gaps between its reads) and
//! is refused where its longest gap isn't shorter than the step it tells the CPU time's steps by: there a gap may be
//! the sampler's own (ROOT's r563, whose sampler at normal priority read every 41 ms behind 31 spinners). Each says too
//! what the machine ran beside the job (`GetSystemTimes`: busy, and busy less the job's), so a low rate tells other
//! work from the job held back. The first run is uncapped: the longest
//! gap between two of its CPU time's steps is the accounting's own step (Windows charges CPU time as its clock ticks),
//! so a gap longer than that in a capped run is the cap stopping the job. Then one run per cap
//! (`JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`, per 10,000 of the machine): first the least cap the charge learner sets on
//! this machine (the window has to hold at every rate it sets; 3 of 10,000 on jupiter's 32 processors), then ROOT's
//! 10%, 20% and 50% (1,000, 2,000 and 5,000). For each it prints one `HEX-CHARGE-WINDOW` line: the rate the job got,
//! and n, p50, p99 and max (nearest rank) of its runs (from the first step after a stop to the last before the next),
//! its stops (the gaps past the accounting's step) and its periods (from one run's start to the next's): the period is
//! the interval the cap holds over. A run whose CPU time stepped too rarely to show a whole period says so, with its
//! steps, and no number.
//!
//! Run on a Windows host with nothing else busy (ROOT's, on jupiter), from the repository's root:
//!   cargo test --locked -p hex-runtime --test windows_charge_window -- --ignored --nocapture --test-threads=1
//! With `HEX_CHARGE_WINDOW_OUT` naming a directory, each run's raw samples go there too, `<cap>.csv` (`uncapped.csv`
//! for the first): the sample's time since the run's start and the job's CPU time so far, both in 100 ns units.
//!
//! Its reckoning (the CPU time's steps, the cap's cycles, the sampler's cadence) is pure and checked on every OS.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

#[cfg(windows)]
use hex_runtime::windows::sys::HighResTimer;
#[cfg(windows)]
use std::os::windows::io::OwnedHandle;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

#[cfg(windows)]
use hex_runtime::windows::sys::{self, Job, Limits, Spawn};

/// How often the job's CPU time is read, and for how long each run: ROOT's probe, as step 14's brief asks it.
#[cfg(windows)]
const EVERY: Duration = Duration::from_millis(1);
#[cfg(windows)]
const FOR: Duration = Duration::from_secs(10);
/// The caps a run is capped at on a machine of `processors`, per `hex_core::job::RATE_SCALE` (10,000) of it: the least
/// cap the charge learner sets there, then ROOT's 10%, 20% and 50%.
fn caps(processors: u32) -> Vec<u32> {
    vec![least_cap(processors), 1_000, 2_000, 5_000]
}

/// The least hard cap the Windows charge learner sets a VM's job at on a machine of `processors`: its least rate, Hex's
/// least CPU bound (`hex_api::MIN_CPU_MILLI`, `hex-node`'s `recorded::charges` caps no VM below it), raised to the
/// machine's least cap (one of `RATE_SCALE` of its processors, rounded up, as the runtime raises a rate), as
/// `hex_core::job::cap` maps it.
fn least_cap(processors: u32) -> u32 {
    let processors = std::num::NonZeroU32::new(processors).unwrap();
    let machine =
        (u64::from(processors.get()) * 1000).div_ceil(u64::from(hex_core::job::RATE_SCALE));
    let milli = u64::from(hex_api::MIN_CPU_MILLI).max(machine);
    hex_core::job::cap(u32::try_from(milli).unwrap(), processors).unwrap()
}
/// What the child is told: how many threads to spin.
const SPIN: &str = "HEX_TEST_SPIN_THREADS";

/// Not a check on its own: run in a job by the probe with `HEX_TEST_SPIN_THREADS` set, it spins that many threads
/// until its job ends; otherwise it does nothing.
#[test]
fn spins_when_asked() {
    let Some(threads) = std::env::var_os(SPIN) else {
        return;
    };
    let threads: usize = threads.to_str().unwrap().parse().unwrap();
    // Below the sampler, whatever its own priority: the spinners never hold its reads back.
    #[cfg(windows)]
    sys::below_normal().unwrap();
    // This thread is one of them.
    for _ in 1..threads {
        std::thread::spawn(spin);
    }
    spin();
}

/// Spins until its process ends.
fn spin() {
    let mut n = 0u64;
    loop {
        n = std::hint::black_box(n.wrapping_add(1));
    }
}

/// Waits on `timer` until `due`, at once where it has passed.
#[cfg(windows)]
fn until(timer: &HighResTimer, due: Instant) {
    let now = Instant::now();
    if due > now {
        timer.wait(due - now).unwrap();
    }
}

#[cfg(windows)]
fn nul() -> OwnedHandle {
    OwnedHandle::from(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("NUL")
            .unwrap(),
    )
}

/// One run: a job capped at `cap` (none uncapped) running `threads` spinners, its CPU time in 100 ns read every
/// [`EVERY`] on `timer` until its reads span [`FOR`], each with its time since the run's start; and the machine's
/// busy CPU over them, in 100 ns.
#[cfg(windows)]
fn run(cap: Option<u32>, threads: u32, timer: &HighResTimer) -> (Vec<(Duration, u64)>, u128) {
    let job = Job::create().unwrap();
    job.configure(&Limits {
        memory: None,
        processes: None,
        cpu: cap.map(hex_core::job::Cpu::Cap),
    })
    .unwrap();
    let exe = std::env::current_exe().unwrap();
    let system = std::env::var("SystemRoot").unwrap();
    let mut spawn = Spawn {
        command_line: format!(
            "\"{}\" spins_when_asked --exact --test-threads=1",
            exe.display()
        )
        .encode_utf16()
        .chain([0])
        .collect(),
        environment: hex_core::job::environment(&[
            ("SystemRoot".to_owned(), system),
            (SPIN.to_owned(), threads.to_string()),
        ]),
        cwd: None,
        stdio: [nul(), nul(), nul()],
    };
    let started = sys::create(&mut spawn, &[job.raw()]).unwrap();
    sys::resume(&started.thread).unwrap();
    let cpu = || job.accounting().unwrap().cpu_100ns;
    // The run starts once the spinners are charged, the job's first step: waited for while they run, their end before
    // it the refusal, no timer guarding it (R03; the person's rule, 2026-11-13).
    while cpu() == 0 {
        assert!(
            !sys::wait(&started.process, 0).unwrap(),
            "the spinners ended before the job was charged"
        );
        until(timer, Instant::now() + EVERY);
    }
    let count = usize::try_from(FOR.as_micros() / EVERY.as_micros()).unwrap();
    let mut samples = Vec::with_capacity(count + 1);
    let machine = sys::system_times().unwrap();
    let start = Instant::now();
    samples.push((Duration::ZERO, cpu()));
    // Bounded by its own reads: it ends once they span FOR. A read past its deadline takes the next from the clock,
    // so a late one leaves no run of reads behind it.
    let mut due = start + EVERY;
    while samples.last().is_some_and(|(at, _)| *at < FOR) && samples.len() <= count {
        until(timer, due);
        let read = cpu();
        let at = start.elapsed();
        samples.push((at, read));
        let past = u32::try_from(at.as_micros() / EVERY.as_micros()).unwrap();
        due = start + EVERY * (past + 1);
    }
    let machine = busy(machine, sys::system_times().unwrap());
    job.terminate(hex_core::job::TERMINATED).unwrap();
    assert!(sys::wait(&started.process, u32::MAX).unwrap());
    (samples, machine)
}

/// The nearest-rank `q`-quantile (per mille) of `values`, and their count and max: ceil(q n / 1000)-th smallest.
fn quantiles(values: &mut [Duration]) -> String {
    values.sort();
    let n = values.len();
    let rank = |q: usize| -> Duration {
        if n == 0 {
            return Duration::ZERO;
        }
        let at = (q * n).div_ceil(1000).max(1) - 1;
        values[at]
    };
    format!(
        "n={n} p50={:?} p99={:?} max={:?}",
        rank(500),
        rank(990),
        values.last().copied().unwrap_or_default()
    )
}

/// The times of the CPU time's steps, and the gaps between them.
fn steps(samples: &[(Duration, u64)]) -> Vec<Duration> {
    samples
        .windows(2)
        .filter(|pair| pair[1].1 > pair[0].1)
        .map(|pair| pair[1].0)
        .collect()
}

/// A capped run's runs, stops and periods: a stop is a gap between two steps longer than `step`, the accounting's own.
#[cfg(windows)]
fn cycles(
    samples: &[(Duration, u64)],
    step: Duration,
) -> (Vec<Duration>, Vec<Duration>, Vec<Duration>) {
    let at = steps(samples);
    let (mut runs, mut stops, mut periods) = (Vec::new(), Vec::new(), Vec::new());
    let Some(&first) = at.first() else {
        return (runs, stops, periods);
    };
    let (mut began, mut last) = (first, first);
    for &t in &at[1..] {
        let gap = t - last;
        if gap > step {
            runs.push(last - began);
            stops.push(gap);
            periods.push(t - began);
            began = t;
        }
        last = t;
    }
    (runs, stops, periods)
}

/// `cpu` (in 100 ns) over `span` on `processors`, per `hex_core::job::RATE_SCALE` (10,000) of the machine.
fn per_machine(cpu: u128, span: Duration, processors: u32) -> u128 {
    let span = (span.as_nanos() / 100).max(1);
    cpu * 10_000 / (span * u128::from(processors).max(1))
}

/// The machine's busy CPU (in 100 ns) between two of its `GetSystemTimes` readings, each its idle, kernel and user
/// time: Windows counts idle time within kernel time, so busy is kernel and user less idle.
fn busy(before: [u64; 3], after: [u64; 3]) -> u128 {
    let [idle, kernel, user]: [u128; 3] =
        std::array::from_fn(|i| u128::from(after[i].saturating_sub(before[i])));
    (kernel + user).saturating_sub(idle)
}

/// The gaps between the reads of consecutive samples: the sampler's own cadence.
fn gaps(samples: &[(Duration, u64)]) -> Vec<Duration> {
    samples
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .collect()
}

/// The sampler's cadence over `samples` (n, p50, p99 and max of its gaps), refused where its longest gap isn't shorter
/// than `step`, the gap the run tells the CPU time's steps by: there a gap between steps may be the sampler's own,
/// not the accounting's or the cap's.
fn cadence(samples: &[(Duration, u64)], step: Duration) -> Result<String, String> {
    let mut gaps = gaps(samples);
    let longest = gaps.iter().max().copied().unwrap_or_default();
    let said = quantiles(&mut gaps);
    if longest >= step {
        return Err(format!(
            "the sampler's longest gap, {longest:?}, isn't shorter than the step it tells the CPU time's steps by, \
             {step:?}: its gaps {said}"
        ));
    }
    Ok(said)
}

#[cfg(windows)]
fn keep(name: &str, samples: &[(Duration, u64)]) {
    use std::fmt::Write as _;
    let Some(dir) = std::env::var_os("HEX_CHARGE_WINDOW_OUT") else {
        return;
    };
    let mut csv = String::from("t_100ns,cpu_100ns\n");
    for (t, cpu) in samples {
        writeln!(csv, "{},{cpu}", t.as_nanos() / 100).unwrap();
    }
    std::fs::write(std::path::Path::new(&dir).join(format!("{name}.csv")), csv).unwrap();
}

/// Joint plan step 14 (AC-T7.4.1), C1 on Windows: the interval a job's CPU hard cap holds over, measured. Do run a busy
/// loop uncapped, then capped at 10%, 20% and 50% of the machine, each sampled every 1 ms until its reads span 10 s.
/// Expect each run's cadence kept (its longest gap shorter than the accounting's step, else the run refused, naming
/// both), each capped run held near its cap and stopped past the accounting's own step, its runs, stops and periods
/// printed; the window `WinRuntime::charge_window` derives from is the period's.
#[cfg(windows)]
#[test]
#[ignore = "ROOT's probe on a quiet Windows host (jupiter): about 40 s of every processor but one"]
fn the_interval_a_jobs_cpu_hard_cap_holds_over() {
    sys::time_critical().unwrap();
    let timer = HighResTimer::new().unwrap();
    let processors = sys::active_processors();
    let threads = processors.saturating_sub(1).max(1);
    // The CPU the job got over a run, per RATE_SCALE (10,000) of the machine: its CPU time over the run's span on
    // every processor, both in 100 ns.
    let rate = |samples: &[(Duration, u64)]| -> u128 {
        let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
            return 0;
        };
        per_machine(u128::from(last.1 - first.1), last.0 - first.0, processors)
    };
    // What the machine ran over a run besides the job (ROOT, at e35e4811's rerun): its busy CPU, and that less the
    // job's, each per RATE_SCALE of the machine, so a low rate tells other work from the job held back.
    let machine = |samples: &[(Duration, u64)], busy: u128| -> String {
        let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
            return String::new();
        };
        let span = last.0 - first.0;
        let others = busy.saturating_sub(u128::from(last.1 - first.1));
        format!(
            "machine_busy={}/10000 others={}/10000",
            per_machine(busy, span, processors),
            per_machine(others, span, processors)
        )
    };
    let (uncapped, busy) = run(None, threads, &timer);
    keep("uncapped", &uncapped);
    let at = steps(&uncapped);
    let step = at.windows(2).map(|p| p[1] - p[0]).max().unwrap_or_default();
    let kept = cadence(&uncapped, step);
    println!(
        "HEX-CHARGE-WINDOW processors={processors} spinners={threads} uncapped rate={}/10000 {} \
         accounting_step={step:?} steps={} cadence[{}]",
        rate(&uncapped),
        machine(&uncapped, busy),
        at.len(),
        kept.as_deref().unwrap_or_else(String::as_str)
    );
    if let Err(why) = kept {
        panic!("REFUSED, the uncapped run: {why}");
    }
    let demand = u64::from(threads) * 10_000 / u64::from(processors);
    let least = least_cap(processors);
    for cap in caps(processors) {
        assert!(
            demand > u64::from(cap),
            "{threads} spinners of {processors} processors ask {demand}/10000, not past the cap {cap}/10000"
        );
        let (samples, busy) = run(Some(cap), threads, &timer);
        keep(&cap.to_string(), &samples);
        let kept = cadence(&samples, step);
        let (mut runs, mut stops, mut periods) = cycles(&samples, step);
        let cadence_said = kept.as_deref().unwrap_or_else(String::as_str);
        if periods.is_empty() {
            // Too few steps to time a period at this cap: said, with no number (ROOT, 2026-11-13).
            println!(
                "HEX-CHARGE-WINDOW cap={cap}/10000 rate={}/10000 {} steps={}: its CPU time stepped too rarely to time \
                 a period cadence[{cadence_said}]",
                rate(&samples),
                machine(&samples, busy),
                steps(&samples).len()
            );
        } else {
            println!(
                "HEX-CHARGE-WINDOW cap={cap}/10000 rate={}/10000 {} runs[{}] stops[{}] periods[{}] cadence[{cadence_said}]",
                rate(&samples),
                machine(&samples, busy),
                quantiles(&mut runs),
                quantiles(&mut stops),
                quantiles(&mut periods)
            );
        }
        if let Err(why) = kept {
            panic!("REFUSED, the run capped at {cap}/10000: {why}");
        }
        // Above the learner's least, a cap stops the job within a run: one that never did is the probe's own fault.
        assert!(
            cap == least || !periods.is_empty(),
            "a job capped at {cap}/10000 never stopped past the accounting's step of {step:?}"
        );
    }
}

/// `count` reads, the k-th at `at(k)`, the job's CPU time `cpu(t)` at each.
fn sampled(
    count: u64,
    at: impl Fn(u64) -> Duration,
    cpu: impl Fn(Duration) -> u64,
) -> Vec<(Duration, u64)> {
    (0..count).map(|k| (at(k), cpu(at(k)))).collect()
}

/// Joint plan step 14 (AC-T7.4.1), the probe's own check (ROOT, 2026-11-12, r563): a sampler that lost its cadence
/// can't measure the accounting's step. Do reckon r563's uncapped run as its samples had it: a read every 41 ms (a
/// Windows quantum behind 31 spinners of its priority), one 584 ms late, the CPU time stepping at each. Expect the
/// accounting's step taken as 584 ms, the sampler's own gap, and the run refused, naming both.
#[test]
fn w14_a_sampler_slower_than_the_step_it_measures_is_refused() {
    let at = |k: u64| Duration::from_millis(41 * k + if k >= 50 { 543 } else { 0 });
    let samples = sampled(100, at, |t| u64::try_from(t.as_millis()).unwrap() * 310);
    let times = steps(&samples);
    let step = times.windows(2).map(|p| p[1] - p[0]).max().unwrap();
    assert_eq!(step, Duration::from_millis(584));
    let refused = cadence(&samples, step).unwrap_err();
    assert!(refused.contains("584ms"), "{refused}");
}

/// Joint plan step 14 (AC-T7.4.1): a sampler that keeps its deadlines measures the accounting's step. Do reckon a run
/// read every 1 ms for 200 ms whose CPU time steps every 15 ms, the clock's tick. Expect the step 15 ms and the cadence
/// kept, its gaps 1 ms.
#[test]
fn w14_a_sampler_that_keeps_its_deadlines_measures_the_step() {
    let samples = sampled(201, Duration::from_millis, |t| {
        u64::try_from(t.as_millis() / 15).unwrap() * 150_000
    });
    let times = steps(&samples);
    let step = times.windows(2).map(|p| p[1] - p[0]).max().unwrap();
    assert_eq!(step, Duration::from_millis(15));
    let kept = cadence(&samples, step).unwrap();
    assert!(kept.contains("n=200 p50=1ms p99=1ms max=1ms"), "{kept}");
}

/// Joint plan step 14 (AC-T7.4.1), ROOT's question on the probe's rerun at e35e4811 (its uncapped run got 518/10000):
/// the machine's other CPU over a run, beside the job's. Do reckon two `GetSystemTimes` readings 1 s apart on a
/// 32-processor machine, 16 s more idle, 20 s more kernel and 12 s more user at the second, and a job that used 12 s of
/// CPU between them. Expect the machine busy 16 s (kernel and user less idle, kernel time holding idle time), 5,000 of
/// 10,000 of the machine, and 4 s of it past the job, 1,250 of 10,000.
#[test]
fn w14_the_machines_busy_cpu_is_kernel_and_user_less_idle() {
    let s = 10_000_000u64;
    let busy = busy([0, 0, 0], [16 * s, 20 * s, 12 * s]);
    assert_eq!(busy, 16 * u128::from(s));
    let second = Duration::from_secs(1);
    assert_eq!(per_machine(busy, second, 32), 5_000);
    assert_eq!(per_machine(busy - 12 * u128::from(s), second, 32), 1_250);
}

/// Joint plan step 14 (AC-T7.4.1), C1 on Windows: the window has to hold at every rate the charge learner sets, so the
/// probe runs the least cap the learner sets on its machine besides ROOT's (ROOT, 2026-11-13). Do reckon the probe's
/// caps for jupiter's 32 processors, a machine of one and one of 128; expect each first at the learner's least: Hex's
/// least CPU bound of 32 processors, 3 per 10,000; of one, 100; of 128, the machine's own least, 1.
#[test]
fn w14_the_probe_runs_the_least_cap_the_learner_sets() {
    assert_eq!(caps(32), [3, 1_000, 2_000, 5_000]);
    assert_eq!(caps(1).first(), Some(&100));
    assert_eq!(caps(128).first(), Some(&1));
}
