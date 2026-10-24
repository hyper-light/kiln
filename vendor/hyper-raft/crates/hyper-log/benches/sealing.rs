//! docs/seal.md §11's group commit, sealed against unsealed, in an open loop. Appends arrive on a
//! Poisson schedule (`hyper_measure::open_loop`) whatever the logs do with earlier ones, and an
//! append's latency counts from its scheduled arrival to the log's answer that it is durable: a
//! stall cannot slow the offered load and hide from the tail (`docs/tails.md` §3.1).
//!
//! ```text
//! cargo bench -p hyper-log --bench sealing -- DIR [ROUNDS] [SECONDS] [SIZES] [LOADS] [GROUPS]
//! ```
//!
//! **At once.** Three logs run together, on fresh scratch files of the device under DIR: a plain
//! log, a second plain log, and a sealed one, each on its own Poisson schedule at the same rate.
//! What the host does to the device, a flush stalled behind another process's writes, it does to
//! all three in the same span, and the two plain logs show what it makes of two identical logs.
//! Run one after the other, a plain and a sealed log's tails were the host's: on a loaded machine
//! a 2 s run that met a stall had a p99 of 200 to 380 ms where its neighbour had 30 to 50 ms, the
//! sealed log's p99 from 0.09 to 7.3 times the plain one's while their p50s stayed within a third.
//! Offered the same arrivals at once, their batches were ready at the same moments and the sealed
//! log, its owner a few microseconds longer at each frame, could reach the device second: so each
//! log's schedule is its own (`docs/benchmarks.md`, "Sealing at rest").
//!
//! For each entry size (100, 300, 1,024 and 16,384 bytes by default: slates' op log's records of
//! 100 B to a KiB, focal's frames of a few hundred bytes, and the closed-loop bench's largest), the
//! bench measures the three logs' capacity, closed loop with GROUPS replicas (64) on each for
//! SECONDS (2), and offers each log each load (25, 50 and 75 per cent of the first plain log's rate
//! there) ROUNDS times (5), each on fresh logs. Each log's arrivals go to its groups in turn, at most
//! [`DEPTH`] out a group; an arrival with no group to take it waits, its latency counting.
//!
//! A row pools a log's rounds at one size and load: the acknowledgement's p50, p99 and p99.9 with
//! their 95% intervals (`docs/tails.md` §3.3); the appends a second; the log's flushes an append
//! and the flushes' own p50 and p99 (its `flush` histogram); and the bytes written an append. Under
//! the rows, docs/seal.md §11's rule for each quantile: the sealed log's interval overlaps the
//! first plain log's, or the sealed estimate exceeds that one's by no more than the larger of the
//! plain log's flush spread (the median over its runs of the flush's p99 less its p50) and the
//! second plain log's difference from the first.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    clippy::cognitive_complexity,
    clippy::too_many_arguments,
    clippy::unwrap_in_result
)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::task::Waker;
use std::time::{Duration, Instant};

use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::scratch::Scratch;
use hyper_log::{Class, Config, Entries, Entry, Log, Pending, Sealing, Start, Update, Waits};
use hyper_measure::open_loop::Arrivals;
use hyper_measure::quantile::{self, Level};
use hyper_seal::Secret32;
use hyper_seal::keys::{KeyId, WrappingKey};

/// Shape: the intervals' coverage (`docs/tails.md` §3.3).
const COVERAGE: Level = match Level::new(95, 100) {
    Ok(level) => level,
    Err(_) => Level::P50,
};

/// Entries a group keeps behind its last before it compacts, as the closed-loop bench does.
const KEEP: u64 = 64;

/// Appends a group keeps out at once: a replica's, hyper-log's room for a group (its ready in
/// flight and a compaction), past which the log would hold a submission waiting for room.
const DEPTH: usize = 2;

/// Shape: the channel's room for wake tags. A tag a full channel misses is caught by the sweep
/// that follows every timed-out wait.
const TAGS: usize = 1 << 16;

/// The logs a run drives at once: two plain ones, whose difference is what the host makes of two
/// identical logs in one span, and the sealed one.
const LOGS: [bool; 3] = [false, false, true];

/// The closed-loop bench's log.
fn config(groups: usize) -> Config {
    Config {
        segment_bytes: 16 << 20,
        max_segments: 64,
        max_groups: groups,
        group_entries: 1 << 20,
        group_bytes: 1 << 30,
        group_cache: 1 << 16,
        queue_submissions: groups * DEPTH,
        waits: Waits::Measured,
    }
}

fn open(path: &Path) -> DeviceFile {
    DeviceFile::open(
        path,
        false,
        CachingRequest::PreferDirect,
        Alignment::new(4096).unwrap(),
    )
    .unwrap()
}

/// A sealed log's keys: the same each run.
fn keys() -> Sealing {
    Sealing {
        parent: WrappingKey::new(KeyId([1; 16]), 0, Secret32::from_bytes(&[1; 32]).unwrap()),
        auth: Secret32::from_bytes(&[2; 32]).unwrap(),
    }
}

/// The next append of a group whose last submitted index is `last`: one entry of `size` bytes,
/// compacting behind itself every [`KEEP`] entries.
fn update(last: u64, size: usize) -> Update {
    let next = last + 1;
    let mut update = Update {
        entries: Some(Entries {
            first: next,
            entries: vec![Entry {
                term: 1,
                bytes: vec![0x5a; size],
            }],
        }),
        ..Update::default()
    };
    if next > KEEP && next.is_multiple_of(KEEP) {
        update.start = Some(Start {
            index: next - KEEP,
            term: 1,
        });
    }
    update
}

/// The groups' wakers, one a group of each log of a run, made once for the whole bench, and the
/// channel they tell: log `i`'s group `g` tells `i · groups + g`.
struct Wakes {
    wakers: Vec<Waker>,
    woken: Receiver<usize>,
}

impl Wakes {
    fn new(groups: usize) -> Self {
        let (tell, woken) = sync_channel(TAGS);
        let wakers = (0..LOGS.len() * groups)
            .map(|tag| hyper_measure::wake::waker(tag, tell.clone()).0)
            .collect();
        Self { wakers, woken }
    }

    /// Tags left from an earlier run.
    fn drain(&self) {
        while self.woken.try_recv().is_ok() {}
    }

    /// The tags to look at next: the one that came, or every one after a wait that ran out.
    fn next(&self, wait: Duration, groups: usize) -> Vec<usize> {
        match self.woken.recv_timeout(wait) {
            Ok(tag) => vec![tag],
            Err(RecvTimeoutError::Timeout) => (0..LOGS.len() * groups).collect(),
            Err(RecvTimeoutError::Disconnected) => panic!("the wakers are gone"),
        }
    }
}

/// One group's submissions out, each with when it was scheduled, answered in the order submitted.
#[derive(Default)]
struct Group {
    last: u64,
    out: VecDeque<(Pending, Instant)>,
}

/// One log of a run as the run drives it.
struct Side {
    /// Its place among the run's logs, which names its wakers.
    index: usize,
    log: Log<DeviceFile>,
    groups: Vec<Group>,
    /// Arrivals no group has taken yet, oldest first.
    backlog: VecDeque<Instant>,
    turn: usize,
    out: usize,
    latencies: Vec<u64>,
    _scratch: Scratch,
}

impl Side {
    fn new(dir: &Path, groups: usize, id: u128, index: usize) -> Self {
        let sealed = LOGS[index];
        let scratch = Scratch::create(dir, ".hyper-bench-sealing").unwrap();
        let file = open(scratch.path());
        let log = if sealed {
            Log::create_sealed(file, config(groups), id, keys()).unwrap()
        } else {
            Log::create(file, config(groups), id).unwrap()
        };
        Self {
            index,
            log,
            groups: (0..groups).map(|_| Group::default()).collect(),
            backlog: VecDeque::new(),
            turn: 0,
            out: 0,
            latencies: Vec::new(),
            _scratch: scratch,
        }
    }

    /// Submits the next append of group `g`, scheduled at `scheduled`.
    fn submit(&mut self, g: usize, size: usize, scheduled: Instant, wakes: &Wakes) {
        let count = self.groups.len();
        let tag = self.index * count + g;
        let group = &mut self.groups[g];
        let pending = self
            .log
            .submit_waking(
                g as u128,
                Class::Normal,
                update(group.last, size),
                wakes.wakers[tag].clone(),
            )
            .unwrap();
        group.last += 1;
        group.out.push_back((pending, scheduled));
        self.out += 1;
    }

    /// Hands the backlog to the groups with room, each in turn.
    fn dispatch(&mut self, size: usize, wakes: &Wakes) {
        let count = self.groups.len();
        while let Some(&scheduled) = self.backlog.front() {
            let Some(g) = (0..count)
                .map(|k| (self.turn + k) % count)
                .find(|&g| self.groups[g].out.len() < DEPTH)
            else {
                break;
            };
            self.turn = (g + 1) % count;
            self.backlog.pop_front();
            self.submit(g, size, scheduled, wakes);
        }
    }

    /// Takes every answer that has come for group `g`, recording each latency from its scheduled
    /// time; returns how many.
    fn take(&mut self, g: usize) -> usize {
        let mut taken = 0;
        let group = &mut self.groups[g];
        while let Some((pending, _)) = group.out.front() {
            let Some(answer) = pending.poll() else { break };
            answer.unwrap();
            let (_, scheduled) = group.out.pop_front().unwrap();
            self.latencies.push(scheduled.elapsed().as_nanos() as u64);
            taken += 1;
        }
        self.out -= taken;
        taken
    }

    /// The log's own counts after a run.
    fn finish(self, elapsed: Duration) -> Ran {
        let stats = self.log.stats(None).unwrap();
        let ran = Ran {
            latencies: self.latencies,
            elapsed,
            flushes: stats.flushes,
            flush_p50: stats.flush.quantile(500_000).unwrap_or(0),
            flush_p99: stats.flush.quantile(990_000).unwrap_or(0),
            bytes: stats.bytes,
        };
        drop(self.log.close().unwrap());
        ran
    }
}

/// What one log measured in one run.
struct Ran {
    latencies: Vec<u64>,
    elapsed: Duration,
    flushes: u64,
    flush_p50: u64,
    flush_p99: u64,
    bytes: u64,
}

/// The run's capacity, closed loop: each group of each log keeps one append out. Returns the first
/// plain log's appends a second.
fn capacity(
    dir: &Path,
    wakes: &Wakes,
    groups: usize,
    size: usize,
    step: Duration,
    id: u128,
) -> f64 {
    let mut sides = [0, 1, 2].map(|index| Side::new(dir, groups, id, index));
    wakes.drain();
    let started = Instant::now();
    let deadline = started + step;
    for side in &mut sides {
        for g in 0..groups {
            side.submit(g, size, Instant::now(), wakes);
        }
    }
    while sides.iter().any(|side| side.out > 0) {
        for tag in wakes.next(step, groups) {
            let (side, g) = (&mut sides[tag / groups], tag % groups);
            if side.take(g) > 0 && Instant::now() < deadline {
                side.submit(g, size, Instant::now(), wakes);
            }
        }
    }
    let elapsed = started.elapsed();
    let [plain, _, _] = sides.map(|side| side.finish(elapsed));
    plain.latencies.len() as f64 / elapsed.as_secs_f64()
}

/// One open-loop run of the three logs at once: each takes arrivals at `rate` a second for `step` on
/// a schedule of its own, drawn from `seed` and the two seeds after it.
fn concurrent(
    dir: &Path,
    wakes: &Wakes,
    groups: usize,
    size: usize,
    rate: f64,
    step: Duration,
    seed: u64,
    id: u128,
) -> [Ran; 3] {
    let mut sides = [0, 1, 2].map(|index| Side::new(dir, groups, id, index));
    wakes.drain();
    let span = step.as_nanos() as u64;
    let mut arrivals =
        [0u64, 1, 2].map(|k| Arrivals::poisson(rate, seed.wrapping_add(k), 0).unwrap());
    let mut next = [0, 1, 2].map(|i| arrivals[i].next_arrival().unwrap());
    let started = Instant::now();
    loop {
        let now = started.elapsed().as_nanos() as u64;
        for (i, side) in sides.iter_mut().enumerate() {
            while next[i] <= now && next[i] < span {
                side.backlog
                    .push_back(started + Duration::from_nanos(next[i]));
                next[i] = arrivals[i].next_arrival().unwrap();
            }
            side.dispatch(size, wakes);
        }
        let soonest = next.iter().copied().min().unwrap_or(u64::MAX);
        if soonest >= span && sides.iter().all(|s| s.out == 0 && s.backlog.is_empty()) {
            break;
        }
        // Until the next arrival, or a whole run's span once every arrival is in
        let wait = if soonest < span {
            Duration::from_nanos(soonest.saturating_sub(started.elapsed().as_nanos() as u64))
        } else {
            step
        };
        for tag in wakes.next(wait, groups) {
            let (side, g) = (&mut sides[tag / groups], tag % groups);
            if side.take(g) > 0 {
                side.dispatch(size, wakes);
            }
        }
    }
    let elapsed = started.elapsed();
    sides.map(|side| side.finish(elapsed))
}

fn micros(ns: u64) -> String {
    format!("{:.0}", ns as f64 / 1e3)
}

/// The levels a row states.
const LEVELS: [Level; 3] = [Level::P50, Level::P99, Level::P999];

/// One log's rounds pooled: the row, each level's quantile, and the flush's spread (the median
/// over the runs of its p99 less its p50).
fn row(
    size: usize,
    load: usize,
    label: &str,
    runs: &[Ran],
) -> (String, Vec<quantile::Quantile<u64>>, u64) {
    let mut all: Vec<u64> = runs
        .iter()
        .flat_map(|r| r.latencies.iter().copied())
        .collect();
    all.sort_unstable();
    let appends: usize = runs.iter().map(|r| r.latencies.len()).sum();
    let seconds: f64 = runs.iter().map(|r| r.elapsed.as_secs_f64()).sum();
    let flushes: u64 = runs.iter().map(|r| r.flushes).sum();
    let bytes: u64 = runs.iter().map(|r| r.bytes).sum();
    let mut p50s: Vec<u64> = runs.iter().map(|r| r.flush_p50).collect();
    let mut p99s: Vec<u64> = runs.iter().map(|r| r.flush_p99).collect();
    let mut spreads: Vec<u64> = runs
        .iter()
        .map(|r| r.flush_p99.saturating_sub(r.flush_p50))
        .collect();
    p50s.sort_unstable();
    p99s.sort_unstable();
    spreads.sort_unstable();
    let per = |total: f64| total / appends.max(1) as f64;
    let quantiles = LEVELS
        .iter()
        .map(|&q| quantile::quantile(&all, q, COVERAGE).unwrap())
        .collect();
    (
        format!(
            "| {size} | {load}% | {label} | {} | {} | {} | {:.0} | {:.3} | {} / {} | {:.0} |",
            quantile::cell(&all, Level::P50, COVERAGE, micros),
            quantile::cell(&all, Level::P99, COVERAGE, micros),
            quantile::cell(&all, Level::P999, COVERAGE, micros),
            appends as f64 / seconds.max(f64::MIN_POSITIVE),
            per(flushes as f64),
            micros(p50s[p50s.len() / 2]),
            micros(p99s[p99s.len() / 2]),
            per(bytes as f64),
        ),
        quantiles,
        spreads[spreads.len() / 2],
    )
}

/// docs/seal.md §11's rule for one level: the sealed interval overlaps the first plain log's, or
/// the sealed estimate is above that one's by no more than `margin`, the larger of the plain log's
/// flush spread and the second plain log's difference from the first.
fn holds(plain: &quantile::Quantile<u64>, sealed: &quantile::Quantile<u64>, margin: u64) -> bool {
    let overlap = match (plain.upper, sealed.lower) {
        (Some(upper), Some(lower)) => lower <= upper,
        // An end the samples do not reach cannot separate the two
        _ => true,
    };
    overlap || sealed.estimate.saturating_sub(plain.estimate) <= margin
}

fn list(arg: Option<&String>, default: &[usize]) -> Vec<usize> {
    arg.map_or_else(
        || default.to_vec(),
        |s| s.split(',').map(|n| n.parse().unwrap()).collect(),
    )
}

fn load_average() -> String {
    std::process::Command::new("uptime")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split("load average")
                .nth(1)
                .map(|l| l.trim_start_matches(['s', ':', ' ']).trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let dir = args
        .first()
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let rounds: usize = args.get(1).map_or(5, |s| s.parse().unwrap()).max(1);
    let seconds: f64 = args.get(2).map_or(2.0, |s| s.parse().unwrap());
    let sizes = list(args.get(3), &[100, 300, 1 << 10, 16 << 10]);
    let loads = list(args.get(4), &[25, 50, 75]);
    let groups = list(args.get(5), &[64])[0].max(1);
    hyper_seal::lock_keys(64).unwrap();
    let step = Duration::from_secs_f64(seconds);
    let wakes = Wakes::new(groups);
    println!(
        "{}: {rounds} rounds of {seconds} s, three logs at once, {groups} groups a log, load {}",
        dir.display(),
        load_average()
    );
    println!();
    println!(
        "| entry B | offered | log | p50 [95%] µs | p99 [95%] µs | p99.9 [95%] µs | appends/s | flushes/append | flush p50 / p99 µs | bytes/append |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    let mut id = 0u128;
    let mut verdicts = Vec::new();
    for &size in &sizes {
        id += 1;
        let capacity = capacity(&dir, &wakes, groups, size, step, id);
        for &load in &loads {
            let rate = capacity * load as f64 / 100.0;
            let mut runs: [Vec<Ran>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            for round in 0..rounds {
                id += 1;
                let seed = (size as u64) << 32 | (load as u64) << 16 | (round as u64) << 2;
                let ran = concurrent(&dir, &wakes, groups, size, rate, step, seed, id);
                for (runs, ran) in runs.iter_mut().zip(ran) {
                    runs.push(ran);
                }
            }
            let (plain_row, plain, spread) = row(size, load, "plain", &runs[0]);
            let (again_row, again, _) = row(size, load, "plain again", &runs[1]);
            let (sealed_row, sealed, _) = row(size, load, "sealed", &runs[2]);
            println!("{plain_row}");
            println!("{again_row}");
            println!("{sealed_row}");
            verdicts.push((size, load, rate, plain, again, sealed, spread));
        }
    }
    println!();
    println!(
        "| entry B | offered appends/s each | plain flush spread µs | plain again / plain: p50, p99, p99.9 | sealed / plain: p50, p99, p99.9 | docs/seal.md §11's rule |"
    );
    println!("|---|---|---|---|---|---|");
    for (size, load, rate, plain, again, sealed, spread) in verdicts {
        let ratio = |of: &[quantile::Quantile<u64>], i: usize| {
            of[i].estimate as f64 / plain[i].estimate.max(1) as f64
        };
        let rule: Vec<&str> = (0..LEVELS.len())
            .map(|i| {
                let margin = spread.max(again[i].estimate.abs_diff(plain[i].estimate));
                if holds(&plain[i], &sealed[i], margin) {
                    "holds"
                } else {
                    "fails"
                }
            })
            .collect();
        println!(
            "| {size} | {rate:.0} ({load}%) | {} | {:.3}, {:.3}, {:.3} | {:.3}, {:.3}, {:.3} | {} |",
            micros(spread),
            ratio(&again, 0),
            ratio(&again, 1),
            ratio(&again, 2),
            ratio(&sealed, 0),
            ratio(&sealed, 1),
            ratio(&sealed, 2),
            rule.join(", ")
        );
    }
    println!();
    println!("load at the end {}", load_average());
}
