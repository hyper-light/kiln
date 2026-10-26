//! What a heartbeat costs the detector's estimator, in time, allocations and page faults:
//! `cargo bench -p hyper-timing --bench estimator` (`docs/benchmarks.md`, "The detector's
//! estimator").
//!
//! A link at a 50 ms interval is fed heartbeats with a millisecond of jitter and one in a thousand
//! stalled 40 ms, after a warm-up that fills its window and configures it. Each heartbeat is the
//! driver's work: poll the deadline if it passed, feed the heartbeat, check whether a
//! configuration is due. Two rings: the drift bound of a 1 ms granularity (window bound 1,332), and the
//! largest any link can have (`WINDOW_LIMIT`, 66,665), where the granularity reaches the interval.
//! Configurations are timed apart, as is a path estimator's sample in the same run for reference:
//! the per-sample work each project already does for a path.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    missing_docs
)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use hyper_measure::{alloc, faults};
use hyper_timing::{Costs, ExchangeRtt, LinkEstimator, PathRtt, Schedule};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// Heartbeats a run times.
const BEATS: u64 = 2_000_000;
/// Runs a row; the median and the range are reported.
const RUNS: usize = 7;
const MS: u64 = 1_000_000;
const INTERVAL: u64 = 50 * MS;

struct Delays(u64);

impl Delays {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        MS + self.0 % MS
            + if self.0.is_multiple_of(1_000) {
                40 * MS
            } else {
                0
            }
    }
}

fn costs() -> Costs {
    Costs {
        election: Duration::from_micros(400),
        mtbf: Duration::from_secs(3_600),
    }
}

/// A configured link and the next sequence number.
fn warm(granularity: Duration, delays: &mut Delays) -> (LinkEstimator, u64) {
    let mut link = LinkEstimator::new(
        Duration::from_nanos(INTERVAL),
        granularity,
        Some(Schedule { seq: 0, at_ns: 0 }),
    )
    .unwrap();
    let warm = 2 * link.estimates().window.drift + 2_000;
    for seq in 0..warm {
        link.on_heartbeat(seq, seq * INTERVAL + delays.next())
            .unwrap();
    }
    link.configure(&costs(), granularity, granularity).unwrap();
    (link, warm)
}

struct Cost {
    nanos: f64,
    allocations: f64,
    reallocations: f64,
    faults: f64,
}

/// `BEATS` heartbeats: the driver's poll, the heartbeat and the due check, configurations set
/// aside from the time but not from the counts.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn heartbeats(granularity: Duration, seed: u64) -> (Cost, u64) {
    let mut delays = Delays(seed);
    let (mut link, start) = warm(granularity, &mut delays);
    let arrivals: Vec<u64> = (start..start + BEATS)
        .map(|seq| seq * INTERVAL + delays.next())
        .collect();
    let costs = costs();
    let mut configure_time = Duration::ZERO;
    let mut configurations = 0u64;
    let before = faults::read().unwrap();
    alloc::begin();
    let began = Instant::now();
    for (i, &arrival) in arrivals.iter().enumerate() {
        if let Some(deadline) = link.deadline().filter(|d| *d <= arrival) {
            black_box(link.poll(deadline));
        }
        black_box(link.on_heartbeat(start + i as u64, arrival).unwrap());
        if link.reconfigure_due() {
            let at = Instant::now();
            black_box(link.configure(&costs, granularity, granularity).unwrap());
            configure_time += at.elapsed();
            configurations += 1;
        }
    }
    let total = began.elapsed();
    let counts = alloc::end();
    let after = faults::read().unwrap();
    let beats = BEATS as f64;
    (
        Cost {
            nanos: (total - configure_time).as_nanos() as f64 / beats,
            allocations: counts.allocations as f64 / beats,
            reallocations: counts.reallocations as f64 / beats,
            faults: after.since(&before).minor as f64 / beats,
        },
        configure_time.as_nanos() as u64 / configurations.max(1),
    )
}

/// A path estimator's sample, the same delays as round trips.
#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn path_sample<P>(mut path: P, sample: impl Fn(&mut P, u64), seed: u64) -> f64 {
    let mut delays = Delays(seed);
    let samples: Vec<u64> = (0..BEATS).map(|_| delays.next()).collect();
    let began = Instant::now();
    for &rtt in &samples {
        sample(&mut path, rtt);
        black_box(&path);
    }
    began.elapsed().as_nanos() as f64 / BEATS as f64
}

fn median(values: &mut [f64]) -> (f64, f64, f64) {
    values.sort_by(f64::total_cmp);
    (
        values[values.len() / 2],
        values[0],
        values[values.len() - 1],
    )
}

fn main() {
    assert!(alloc::installed(), "the counting allocator is installed");
    let rows = [
        (
            "1 ms granularity (window bound 1,332)",
            Duration::from_millis(1),
        ),
        (
            "granularity at the interval (window bound 66,665)",
            Duration::from_nanos(INTERVAL),
        ),
    ];
    println!(
        "hyper-timing estimator: per heartbeat at a 50 ms interval, {BEATS} heartbeats a run, \
         {RUNS} runs, median (least–most)"
    );
    // Per row: time, allocations, reallocations, faults, configuration time; one value a run.
    let mut table: Vec<[Vec<f64>; 5]> = vec![Default::default(); rows.len()];
    let (mut path, mut exchange) = (Vec::new(), Vec::new());
    for run in 0..RUNS {
        let seed = 0x9E37_79B9_7F4A_7C15 ^ run as u64;
        // The rows rotate each run, so neither always runs first.
        for k in 0..rows.len() {
            let index = (k + run) % rows.len();
            let (cost, configure) = heartbeats(rows[index].1, seed);
            let row = &mut table[index];
            row[0].push(cost.nanos);
            row[1].push(cost.allocations);
            row[2].push(cost.reallocations);
            row[3].push(cost.faults);
            row[4].push(configure as f64);
        }
        // A path probed at the interval on a link whose correlation time is the interval: the
        // window of three the derivation gives at the configurator's multi-heartbeat floor.
        let interval = Duration::from_nanos(INTERVAL);
        // Its answers stamped at one instant: none ages past the span, as on a path probed at its
        // interval.
        path.push(path_sample(
            PathRtt::new(interval, interval).unwrap(),
            |path: &mut PathRtt, rtt| path.on_sample(rtt, 0, 0),
            seed,
        ));
        exchange.push(path_sample(
            ExchangeRtt::new(),
            ExchangeRtt::on_sample,
            seed,
        ));
    }
    println!(
        "| ring | ns a heartbeat | allocations | reallocations | minor faults | ns a configuration |"
    );
    println!("|---|---|---|---|---|---|");
    for (index, (name, _)) in rows.iter().enumerate() {
        let row = &mut table[index];
        let (ns, low, high) = median(&mut row[0]);
        let (conf, conf_low, conf_high) = median(&mut row[4]);
        println!(
            "| {name} | {ns:.1} ({low:.1}–{high:.1}) | {:.4} | {:.4} | {:.5} | {conf:.0} ({conf_low:.0}–{conf_high:.0}) |",
            median(&mut row[1]).0,
            median(&mut row[2]).0,
            median(&mut row[3]).0,
        );
    }
    let (p, pl, ph) = median(&mut path);
    let (e, el, eh) = median(&mut exchange);
    println!("| PathRtt::on_sample, for reference | {p:.1} ({pl:.1}–{ph:.1}) | | | | |");
    println!("| ExchangeRtt::on_sample, for reference | {e:.1} ({el:.1}–{eh:.1}) | | | | |");
}
