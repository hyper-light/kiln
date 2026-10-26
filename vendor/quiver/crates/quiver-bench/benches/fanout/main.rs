//! Quiver's bench of the ephemeral class (kiln's CP-T2.1: AC-T2.1.2, AC-T2.1.3, AC-T2.1.4) and its
//! comparison, class for class, with NATS core, Redis Pub/Sub and Kafka on the same machine
//! (CLAUDE.md §7). bench/README.md says how to run it and records each run.
//!
//! One topic, one publisher, an open loop: messages arrive at the offered rate on a Poisson
//! schedule (hyper-measure's `open_loop`), each carrying its scheduled time and its number, and
//! latency is counted from the scheduled time, so a stall can't slow the offered load and hide
//! from the tail (Schroeder, Wierman and Harchol-Balter, NSDI 2006). Every subscriber takes every
//! message: `--subscribers` of them, shared out over `--workers` threads. A delivery's latency is
//! the time from its message's schedule to its take, recorded for every delivery of a message
//! scheduled in the measured window into a log-linear histogram (within 1.6%), and every subscriber
//! checks the numbers it takes: one it never took is undelivered, counted past every delivery in
//! the percentiles, and one missed with no lag notice before it is a frame lost.
//!
//! Every system runs the same schedule, payloads and fan-out, measured the same way: Quiver in this
//! process, as kiln runs it (`embedded.rs`), the brokers over loopback through their own clients
//! (`brokers.rs`), and beside them the kernel's own wake-up with no system around it (`wake.rs`),
//! the floor under a subscriber that waits.
//!
//! The mode is the first argument:
//! - `fanout` runs `--system` `--runs` times and reports each run and the best (the least p99,
//!   CLAUDE.md §7), adding each to `--out`;
//! - `soak` runs Quiver once for `--seconds` (an hour, for AC-T2.1.4), reporting every minute;
//! - `report <runs files>` sets each system's best beside Quiver's, load by load.
//!
//! Every run carries its profile (`profile/`), and with `--budgets` is held to the budgets for its
//! load (`budget.rs`).
//!
//! Benchmark code (CLAUDE.md: production is what's compiled outside tests, benches and examples):
//! it may unwrap and index.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// The brokers run on Linux alone (their clients are built there); elsewhere what only they read
// goes unread.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
mod brokers;
mod budget;
mod embedded;
mod follow;
mod histogram;
mod profile;
mod report;
mod wake;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use profile::Profiler;
use report::Run;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum System {
    Quiver,
    Nats,
    Redis,
    Kafka,
    /// The kernel's own wake-up, no system around it: the floor under a subscriber that waits.
    Wake,
}

impl System {
    pub fn name(self) -> &'static str {
        match self {
            Self::Quiver => "quiver",
            Self::Nats => "nats",
            Self::Redis => "redis",
            Self::Kafka => "kafka",
            Self::Wake => "wake",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "quiver" => Self::Quiver,
            "nats" => Self::Nats,
            "redis" => Self::Redis,
            "kafka" => Self::Kafka,
            "wake" => Self::Wake,
            other => panic!("unknown system {other:?}: quiver, nats, redis, kafka or wake"),
        }
    }

    /// Where `bench/compare.sh` starts its broker.
    fn endpoint(self) -> &'static str {
        match self {
            Self::Quiver => "in this process",
            Self::Nats => "127.0.0.1:14222",
            Self::Redis => "127.0.0.1:16380",
            Self::Kafka => "127.0.0.1:19092",
            Self::Wake => "the kernel's wake-up alone, in this process",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Fanout,
    Soak,
    Report,
}

#[derive(Clone, Debug)]
pub struct Args {
    mode: Mode,
    pub system: System,
    pub endpoint: String,
    pub rate: f64,
    pub subscribers: usize,
    pub workers: usize,
    pub seconds: u64,
    pub warmup: u64,
    pub payload: usize,
    pub retain: u64,
    runs: usize,
    seed: u64,
    /// Refuse to start above this load average (CLAUDE.md §7: under 2).
    max_load: f64,
    /// Subscribers' threads poll without waiting, as the lowest-latency consumers do: Quiver's
    /// workers look again, the brokers' clients' runtimes poll their sockets.
    pub busy: bool,
    /// How long after the schedule closes a broker's subscribers may take what's still coming.
    pub drain: u64,
    out: Option<PathBuf>,
    /// The broker's container's cgroup, whose CPU the run reports.
    pub broker_cgroup: Option<PathBuf>,
    /// The broker's Docker container, whose account and runtime the profile reads.
    pub broker_container: Option<String>,
    /// Where the broker's saved profiles go.
    pub profiles: Option<PathBuf>,
    /// The budgets each run is held to (`budget.rs`).
    budgets: Option<PathBuf>,
    files: Vec<PathBuf>,
}

fn args() -> Args {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let mut a = Args {
        mode: Mode::Fanout,
        system: System::Quiver,
        endpoint: String::new(),
        rate: 100_000.0,
        subscribers: 1000,
        workers: cores.saturating_sub(2).max(1),
        seconds: 30,
        warmup: 5,
        payload: 256,
        retain: 65_536,
        runs: 3,
        seed: 0x5eed,
        max_load: 2.0,
        busy: false,
        drain: 30,
        out: None,
        broker_cgroup: None,
        broker_container: None,
        profiles: None,
        budgets: None,
        files: Vec::new(),
    };
    let given: Vec<String> = std::env::args_os()
        .skip(1)
        .map(OsString::into_string)
        .collect::<Result<_, _>>()
        .expect("arguments are Unicode");
    let mut it = given.iter().filter(|s| *s != "--bench");
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .unwrap_or_else(|| panic!("{flag} takes a value"))
                .clone()
        };
        match flag.as_str() {
            "fanout" => a.mode = Mode::Fanout,
            "soak" => a.mode = Mode::Soak,
            "report" => a.mode = Mode::Report,
            "--system" => a.system = System::parse(&value()),
            "--endpoint" => a.endpoint = value(),
            "--rate" => a.rate = value().parse().unwrap(),
            "--subscribers" => a.subscribers = value().parse().unwrap(),
            "--workers" => a.workers = value().parse().unwrap(),
            "--seconds" => a.seconds = value().parse().unwrap(),
            "--warmup" => a.warmup = value().parse().unwrap(),
            "--payload" => a.payload = value().parse().unwrap(),
            "--retain" => a.retain = value().parse().unwrap(),
            "--runs" => a.runs = value().parse().unwrap(),
            "--seed" => a.seed = value().parse().unwrap(),
            "--max-load" => a.max_load = value().parse().unwrap(),
            "--busy" => a.busy = true,
            "--drain" => a.drain = value().parse().unwrap(),
            "--out" => a.out = Some(PathBuf::from(value())),
            "--broker-cgroup" => a.broker_cgroup = Some(PathBuf::from(value())),
            "--broker-container" => a.broker_container = Some(value()),
            "--profiles" => a.profiles = Some(PathBuf::from(value())),
            "--budgets" => a.budgets = Some(PathBuf::from(value())),
            file if a.mode == Mode::Report && !file.starts_with("--") => {
                a.files.push(PathBuf::from(file));
            }
            other => panic!(
                "unknown argument {other:?}: [fanout|soak|report FILE..] --system --endpoint --rate --subscribers --workers --seconds --warmup --payload --retain --runs --seed --max-load --busy --drain --out --broker-cgroup --broker-container --profiles --budgets"
            ),
        }
    }
    if a.endpoint.is_empty() {
        a.system.endpoint().clone_into(&mut a.endpoint);
    }
    assert!(
        a.payload >= 16,
        "a payload carries its schedule and its number: 16 bytes at least"
    );
    a
}

/// This thread's CPU so far, seconds: the scheduler's own count of its time on a CPU
/// (`/proc/thread-self/schedstat`, Linux), none where it isn't kept.
pub fn thread_cpu_s() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    let ns: u64 = text.split_whitespace().next()?.parse().ok()?;
    Some(ns as f64 / 1e9)
}

/// The machine, as the record states it.
fn machine() -> String {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let model = cpuinfo
        .lines()
        .find_map(|l| {
            l.strip_prefix("model name")
                .and_then(|r| r.split_once(':'))
                .map(|(_, m)| m.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown CPU".to_owned());
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let memory = std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    format!(
        "{model}, {cpus} CPUs this process may use, Linux {}, {}",
        kernel.trim(),
        memory
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

#[cfg(target_os = "linux")]
fn broker(a: &Args, system: System, seed: u64, index: usize, profiler: &Profiler) -> Run {
    brokers::run(a, system, seed, index, profiler)
}

#[cfg(not(target_os = "linux"))]
fn broker(_: &Args, system: System, _: u64, _: usize, _: &Profiler) -> Run {
    panic!(
        "{}: the comparison runs on Linux, its brokers in Docker (bench/compare.sh)",
        system.name()
    )
}

fn main() -> ExitCode {
    let a = args();
    if a.mode == Mode::Report {
        print!("{}", report::compare(&a.files));
        return ExitCode::SUCCESS;
    }
    let load = hyper_measure::usage::load().unwrap_or(0.0);
    assert!(
        load <= a.max_load,
        "the load average is {load:.2}, over {}: a benchmark needs a quiet machine (CLAUDE.md §7)",
        a.max_load
    );
    assert!(
        a.mode == Mode::Fanout || a.system == System::Quiver,
        "the soak is Quiver's (AC-T2.1.4)"
    );
    let budgets = a.budgets.as_deref().map(budget::read).unwrap_or_default();
    // Before any of a run's threads, so their counters are theirs alone; this thread publishes.
    let profiler = Profiler::start();
    profile::take_role(profile::Role::Publisher);
    println!(
        "# quiver-bench {} {}",
        if a.mode == Mode::Soak {
            "soak"
        } else {
            "fanout"
        },
        a.system.name()
    );
    println!();
    println!("- machine: {}", machine());
    println!(
        "- commit: {}",
        option_env!("QUIVER_COMMIT").unwrap_or("unknown (set QUIVER_COMMIT)")
    );
    println!("- system: {} ({})", a.system.name(), a.endpoint);
    println!(
        "- offered: {} messages/s, Poisson, {} B payloads, one topic{}",
        a.rate,
        a.payload,
        if a.system == System::Quiver {
            format!(" keeping {} messages", a.retain)
        } else {
            String::new()
        }
    );
    println!(
        "- subscribers: {} over {} threads, which {}; window {} s after {} s of warm-up; seed {:#x}",
        a.subscribers,
        a.workers.min(a.subscribers).max(1),
        if a.busy {
            "poll without waiting"
        } else {
            "wait when there's nothing to take"
        },
        a.seconds,
        a.warmup,
        a.seed
    );
    println!();
    if a.mode == Mode::Soak {
        println!("| minute | published | deliveries are counted at the end |\n|---|---|---|");
        let r = embedded::run(&a, a.seed, &profiler, |m, published| {
            println!("| {m} | {published} | |")
        });
        println!();
        println!("{}", report::header());
        println!("{}", report::row("soak", &r));
        println!();
        println!(
            "frames lost: {}; doubled: {}; subscribers that kept up (no lag notice): {} of {}",
            r.silent, r.doubled, r.kept_up, r.subscribers
        );
        if let Some(p) = &r.profile {
            println!();
            print!("{}", p.table());
        }
        if let Some(out) = &a.out {
            report::record(out, &r);
        }
        return held(&budgets, &[r]);
    }
    println!("{}", report::header());
    let mut runs = Vec::new();
    for i in 0..a.runs {
        let seed = a.seed.wrapping_add(i as u64);
        let r = match a.system {
            System::Quiver => embedded::run(&a, seed, &profiler, |_, _| {}),
            System::Wake => wake::run(&a, seed, &profiler),
            other => broker(&a, other, seed, i, &profiler),
        };
        println!("{}", report::row(&(i + 1).to_string(), &r));
        if let Some(out) = &a.out {
            report::record(out, &r);
        }
        runs.push(r);
    }
    let best = runs
        .iter()
        .min_by_key(|r| r.seen.latency.quantile(0.99).unwrap_or(u64::MAX))
        .unwrap();
    println!("{}", report::row("best", best));
    for (i, r) in runs.iter().enumerate() {
        if let Some(p) = &r.profile {
            println!();
            println!("Run {}'s profile:", i + 1);
            println!();
            print!("{}", p.table());
        }
    }
    held(&budgets, &runs)
}

/// Each run against the budgets for its load: their tables, and the process's exit, a failure if a
/// run went over one.
fn held(budgets: &[budget::Budget], runs: &[Run]) -> ExitCode {
    let machine = machine();
    let mut met = true;
    for (i, r) in runs.iter().enumerate() {
        let (table, ok) = budget::check(budgets, r, &machine);
        if !table.is_empty() {
            println!();
            println!("Run {}'s budgets:", i + 1);
            println!();
            print!("{table}");
        }
        met &= ok;
    }
    if met {
        ExitCode::SUCCESS
    } else {
        println!();
        println!("FAILED: a run went over its budget");
        ExitCode::FAILURE
    }
}
