//! Quiver's bench of the ephemeral class on one node (kiln's CP-T2.1: AC-T2.1.2, AC-T2.1.3,
//! AC-T2.1.4; bench/README.md says how to run it and records each run).
//!
//! One topic, one publisher, an open loop: messages arrive at the offered rate on a Poisson
//! schedule (hyper-measure's `open_loop`), each carrying its scheduled time and its number, and
//! latency is counted from the scheduled time, so a stall can't slow the offered load and hide
//! from the tail (Schroeder, Wierman and Harchol-Balter, NSDI 2006). Every subscriber takes every
//! message: `--subscribers` subscriptions, shared out over `--workers` threads, each thread taking
//! from each of its subscriptions in turn and waiting on the topic when none has anything. A
//! delivery's latency is the time from its message's schedule to its take, recorded for every
//! delivery of a message scheduled in the measured window into a log-linear histogram (64
//! buckets a power of two: within 1.6% of the value). Every subscriber checks the numbers it
//! takes: a gap with no lag notice before it is a frame lost.
//!
//! `fanout` runs `--runs` times and reports each run and the best (the least p99, CLAUDE.md §7);
//! `soak` runs once for `--seconds` (an hour, for AC-T2.1.4) and reports every minute.
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

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use hyper_measure::open_loop::Arrivals;
use quiver::{Node, NodeConfig};
use quiver_api::{Class, Next, Start, Subscription, Topic, Topics};

#[derive(Clone, Debug)]
struct Args {
    soak: bool,
    rate: f64,
    subscribers: usize,
    workers: usize,
    seconds: u64,
    warmup: u64,
    payload: usize,
    retain: u64,
    runs: usize,
    seed: u64,
    /// Refuse to start above this load average (CLAUDE.md §7: under 2).
    max_load: f64,
    /// How long a worker with nothing to take keeps looking before it waits on the topic (0: it
    /// waits at once): a consumer that polls a while, as low-latency consumers do, wakes no one.
    spin_us: u64,
}

fn args() -> Args {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    let mut a = Args {
        soak: false,
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
        spin_us: 0,
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
            "soak" => a.soak = true,
            "fanout" => a.soak = false,
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
            "--spin-us" => a.spin_us = value().parse().unwrap(),
            other => panic!(
                "unknown argument {other:?}: [fanout|soak] --rate --subscribers --workers --seconds --warmup --payload --retain --runs --seed --max-load --spin-us"
            ),
        }
    }
    assert!(
        a.payload >= 16,
        "a payload carries its schedule and its number: 16 bytes at least"
    );
    a
}

// ------------------------------------------------------------------------------- histogram

/// Nanoseconds in log-linear buckets: exact below 128, then 64 buckets each power of two.
#[derive(Clone)]
struct Histogram {
    counts: Vec<u64>,
    total: u64,
    max: u64,
}

const SUB: u32 = 6;

impl Histogram {
    fn new() -> Self {
        Self {
            counts: vec![0; 64 * 64 + 128],
            total: 0,
            max: 0,
        }
    }

    fn index(v: u64) -> usize {
        if v < 128 {
            return v as usize;
        }
        let msb = 63 - v.leading_zeros();
        let shift = msb - SUB;
        ((shift as usize + 1) << SUB) + ((v >> shift) & 63) as usize
    }

    /// The largest value bucket `i` holds.
    fn upper(i: usize) -> u64 {
        if i < 128 {
            return i as u64;
        }
        let shift = (i >> SUB) as u32 - 1;
        let sub = (i & 63) as u64;
        ((64 + sub) << shift) | ((1 << shift) - 1)
    }

    fn record(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.total += 1;
        self.max = self.max.max(v);
    }

    fn merge(&mut self, other: &Self) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
        self.max = self.max.max(other.max);
    }

    /// The value at quantile `q`: the upper end of the bucket the `q`-th delivery is in.
    fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let rank = ((q * self.total as f64).ceil() as u64).clamp(1, self.total);
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::upper(i).min(self.max);
            }
        }
        self.max
    }
}

// ---------------------------------------------------------------------------------- a run

/// What one worker saw of its subscriptions.
struct Seen {
    latency: Histogram,
    deliveries: u64,
    measured: u64,
    lags: u64,
    /// Numbers missed with no lag notice before them: frames lost.
    lost: u64,
    /// Numbers taken again, or out of order.
    doubled: u64,
    /// Subscriptions that took no lag notice: those that kept up.
    kept_up: usize,
    subscriptions: usize,
}

/// One subscription's place in its numbers.
struct Follow {
    sub: Box<dyn Subscription>,
    next: Option<u64>,
    lagged: bool,
    noticed: bool,
}

struct Shared {
    base: Instant,
    /// Deliveries of a message scheduled in `[from, until)` ns are measured.
    from: AtomicU64,
    until: AtomicU64,
    /// The number of the last message each worker has taken from every subscription, at least.
    caught_up: Vec<AtomicU64>,
    stop: AtomicBool,
}

fn worker(index: usize, mut subs: Vec<Follow>, shared: Arc<Shared>, spin: Duration) -> Seen {
    let mut idle_since: Option<Instant> = None;
    let mut seen = Seen {
        latency: Histogram::new(),
        deliveries: 0,
        measured: 0,
        lags: 0,
        lost: 0,
        doubled: 0,
        kept_up: 0,
        subscriptions: subs.len(),
    };
    loop {
        let mut took = false;
        let mut lowest = u64::MAX;
        let mut ended = false;
        for f in &mut subs {
            while let Some(next) = f.sub.take() {
                took = true;
                match next {
                    Next::Message(m) => {
                        let at = shared.base.elapsed().as_nanos() as u64;
                        let scheduled = u64::from_le_bytes(m.payload[..8].try_into().unwrap());
                        let number = u64::from_le_bytes(m.payload[8..16].try_into().unwrap());
                        if let Some(want) = f.next {
                            if number < want {
                                seen.doubled += 1;
                            } else if number > want && !f.lagged {
                                seen.lost += number - want;
                            }
                        }
                        f.lagged = false;
                        f.next = Some(number + 1);
                        seen.deliveries += 1;
                        if scheduled >= shared.from.load(Ordering::Relaxed)
                            && scheduled < shared.until.load(Ordering::Relaxed)
                        {
                            seen.latency.record(at.saturating_sub(scheduled));
                            seen.measured += 1;
                        }
                    }
                    Next::Lag { .. } => {
                        seen.lags += 1;
                        f.lagged = true;
                        f.noticed = true;
                    }
                    Next::Ended => {
                        ended = true;
                        break;
                    }
                }
            }
            lowest = lowest.min(f.next.unwrap_or(0));
        }
        shared.caught_up[index].store(lowest, Ordering::Release);
        if ended || shared.stop.load(Ordering::Acquire) {
            break;
        }
        if took {
            idle_since = None;
            continue;
        }
        // Nothing to take: look again until the spin is over, then wait on the topic.
        let since = *idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() < spin {
            std::hint::spin_loop();
            continue;
        }
        idle_since = None;
        if let Some(first) = subs.first_mut() {
            futures::executor::block_on(first.sub.ready());
        }
    }
    seen.kept_up = subs.iter().filter(|f| !f.noticed).count();
    seen
}

/// A run's results.
#[derive(Clone)]
struct Run {
    published: u64,
    publish_refused: u64,
    seconds: f64,
    latency: Histogram,
    deliveries: u64,
    measured: u64,
    lags: u64,
    lost: u64,
    doubled: u64,
    kept_up: usize,
    subscriptions: usize,
    cpu_s: f64,
    load_before: Option<f64>,
    load_after: Option<f64>,
}

fn run(a: &Args, seed: u64, mut minute: impl FnMut(u64, &Shared, u64)) -> Run {
    let node = Node::new(NodeConfig {
        memory: 4 << 30,
        topics: 4,
        subscriptions: a.subscribers + 16,
    })
    .unwrap();
    node.create(
        "bench",
        Topic {
            class: Class::Ephemeral,
            retain_messages: a.retain,
            retain_bytes: 1 << 30,
        },
    )
    .now_or_never()
    .unwrap()
    .unwrap();
    let mut shares: Vec<Vec<Follow>> = (0..a.workers).map(|_| Vec::new()).collect();
    for i in 0..a.subscribers {
        let sub = node
            .subscribe("bench", Start::Latest)
            .now_or_never()
            .unwrap()
            .unwrap();
        shares[i % a.workers].push(Follow {
            sub,
            next: None,
            lagged: false,
            noticed: false,
        });
    }
    let shared = Arc::new(Shared {
        base: Instant::now(),
        from: AtomicU64::new(u64::MAX),
        until: AtomicU64::new(u64::MAX),
        caught_up: (0..a.workers).map(|_| AtomicU64::new(0)).collect(),
        stop: AtomicBool::new(false),
    });
    let load_before = hyper_measure::usage::load();
    let cpu_before = hyper_measure::usage::this().ok();
    let handles: Vec<_> = shares
        .into_iter()
        .enumerate()
        .map(|(i, subs)| {
            let shared = Arc::clone(&shared);
            let spin = Duration::from_micros(a.spin_us);
            std::thread::Builder::new()
                .name(format!("sub-{i}"))
                .spawn(move || worker(i, subs, shared, spin))
                .unwrap()
        })
        .collect();

    let ns = |s: u64| s * 1_000_000_000;
    let start = 1_000_000; // a millisecond in, so every worker is waiting
    let measure_from = start + ns(a.warmup);
    let measure_until = measure_from + ns(a.seconds);
    // Published past the window by half a second, so every measured message has one after it.
    let end = measure_until + 500_000_000;
    shared.from.store(measure_from, Ordering::Relaxed);
    shared.until.store(measure_until, Ordering::Relaxed);
    let mut arrivals = Arrivals::poisson(a.rate, seed, start).unwrap();
    let mut payload = vec![0u8; a.payload];
    let (mut published, mut refused) = (0u64, 0u64);
    let mut last_minute = 0u64;
    let began = Instant::now();
    loop {
        let at = arrivals.next_arrival().unwrap();
        if at >= end {
            break;
        }
        while (shared.base.elapsed().as_nanos() as u64) < at {
            std::hint::spin_loop();
        }
        payload[..8].copy_from_slice(&at.to_le_bytes());
        payload[8..16].copy_from_slice(&published.to_le_bytes());
        match node.publish("bench", None, &payload).now_or_never() {
            Some(Ok(_)) => published += 1,
            _ => refused += 1,
        }
        let minute_now = at / ns(60);
        if minute_now > last_minute {
            last_minute = minute_now;
            minute(minute_now, &shared, published);
        }
    }
    let seconds = began.elapsed().as_secs_f64();
    // Every worker takes what was published (the fact waited on), then the topic ends.
    let deadline = Instant::now() + Duration::from_secs(60);
    while shared
        .caught_up
        .iter()
        .any(|c| c.load(Ordering::Acquire) < published)
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    shared.stop.store(true, Ordering::Release);
    node.delete("bench").now_or_never().unwrap().unwrap();
    let mut total = Seen {
        latency: Histogram::new(),
        deliveries: 0,
        measured: 0,
        lags: 0,
        lost: 0,
        doubled: 0,
        kept_up: 0,
        subscriptions: 0,
    };
    for h in handles {
        let s = h.join().unwrap();
        total.latency.merge(&s.latency);
        total.deliveries += s.deliveries;
        total.measured += s.measured;
        total.lags += s.lags;
        total.lost += s.lost;
        total.doubled += s.doubled;
        total.kept_up += s.kept_up;
        total.subscriptions += s.subscriptions;
    }
    let cpu_s = match (cpu_before, hyper_measure::usage::this().ok()) {
        (Some(b), Some(a)) => a.since(&b).cpu_ns() as f64 / 1e9,
        _ => f64::NAN,
    };
    Run {
        published,
        publish_refused: refused,
        seconds,
        latency: total.latency,
        deliveries: total.deliveries,
        measured: total.measured,
        lags: total.lags,
        lost: total.lost,
        doubled: total.doubled,
        kept_up: total.kept_up,
        subscriptions: total.subscriptions,
        cpu_s,
        load_before,
        load_after: hyper_measure::usage::load(),
    }
}

fn us(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1000.0)
}

fn row(i: &str, r: &Run) -> String {
    let l = &r.latency;
    format!(
        "| {i} | {} | {:.0} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}/{} | {:.1} | {:.0} | {} / {} |",
        r.published,
        r.published as f64 / r.seconds,
        r.publish_refused,
        r.deliveries,
        r.measured,
        us(l.quantile(0.5)),
        us(l.quantile(0.9)),
        us(l.quantile(0.99)),
        us(l.quantile(0.999)),
        us(l.max),
        r.lags,
        r.lost,
        r.kept_up,
        r.subscriptions,
        r.cpu_s,
        if r.deliveries > 0 {
            r.cpu_s * 1e9 / r.deliveries as f64
        } else {
            f64::NAN
        },
        r.load_before
            .map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
        r.load_after
            .map_or_else(|| "-".to_owned(), |l| format!("{l:.2}")),
    )
}

fn header() -> &'static str {
    "| run | published | /s achieved | refused | deliveries | measured | p50 µs | p90 µs | p99 µs | p99.9 µs | max µs | lag notices | lost | kept up | CPU s | CPU ns a delivery | load before / after |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
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
        "{model}, {cpus} CPUs, Linux {}, {}",
        kernel.trim(),
        memory
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

fn main() {
    let a = args();
    let load = hyper_measure::usage::load().unwrap_or(0.0);
    assert!(
        load <= a.max_load,
        "the load average is {load:.2}, over {}: a benchmark needs a quiet machine (CLAUDE.md §7)",
        a.max_load
    );
    println!("# quiver-bench {}", if a.soak { "soak" } else { "fanout" });
    println!();
    println!("- machine: {}", machine());
    println!(
        "- commit: {}",
        option_env!("QUIVER_COMMIT").unwrap_or("unknown (set QUIVER_COMMIT)")
    );
    println!(
        "- offered: {} messages/s, Poisson, {} B payloads, one topic keeping {} messages",
        a.rate, a.payload, a.retain
    );
    println!(
        "- subscribers: {} over {} worker threads, each looking {} µs before it waits; window {} s after {} s of warm-up; seed {:#x}",
        a.subscribers, a.workers, a.spin_us, a.seconds, a.warmup, a.seed
    );
    println!();
    if a.soak {
        println!(
            "| minute | published | deliveries so far are counted at the end |\n|---|---|---|"
        );
        let r = run(&a, a.seed, |m, _, published| {
            println!("| {m} | {published} | |")
        });
        println!();
        println!("{}", header());
        println!("{}", row("soak", &r));
        println!();
        println!(
            "frames lost: {}; doubled: {}; subscribers that kept up (no lag notice): {} of {}",
            r.lost, r.doubled, r.kept_up, r.subscriptions
        );
        return;
    }
    println!("{}", header());
    let mut runs = Vec::new();
    for i in 0..a.runs {
        let r = run(&a, a.seed.wrapping_add(i as u64), |_, _, _| {});
        println!("{}", row(&(i + 1).to_string(), &r));
        runs.push(r);
    }
    let best = runs
        .iter()
        .min_by_key(|r| r.latency.quantile(0.99))
        .unwrap();
    println!("{}", row("best", best));
}
