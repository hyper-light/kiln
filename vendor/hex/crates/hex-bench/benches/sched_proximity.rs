//! S4/A2's measure (G3): goodput and the completion-time tail of proximity-scored placement against
//! proximity-blind placement on hex-sim's modelled WAN (`hex_sim::proximity`, `hex-sim.md` H21).
//!
//!   cargo bench -p hex-bench --bench sched_proximity
//!
//! The record: G2's recorded day as it is (`hex-sim.md` H10), on each of its node models, every
//! launch at its arrival with its recorded lifetime as compute: `huawei-r5-day30.trace` (ten m5d.metal
//! hosts running all three kinds) and `huawei-r5-day30.fleet.trace` (Hex's own Linux, Windows and
//! macOS hosts). Homes uniform; the WAN Azure's table with every zone pair at Azure's stated bound and
//! no heights or jitter (`Access::Bound`); coordinates converged on hyper-swim's schedule until their
//! median error stops falling. Each study runs under every policy: blind (the fast path's best fit),
//! proximity (hyper-swim's predicted RTT) and the oracle (the true RTT). The seeds are as many as the claim needs (`SEEDS`). Then, labelled and
//! not of record, two sensitivity runs over the same seeds: skewed homes, and the modelled WAN
//! (`Access::Modelled`). Virtual time: the numbers depend on the seed and the build, not the host's
//! load.
// A bench (not production, CLAUDE.md): it takes the test-style allowances, as the other benches do.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use hex_sim::index::Pick;
use hex_sim::proximity::{CLASSES, Homes, Policy, PolicyReport, Study, StudyConfig};
use hex_sim::replay::{Limits, parse};
use hex_sim::wan::{Access, Accuracy};

/// The claim the seeds carry: if a fraction `CLAIM_FRACTION` of seeds would show no gain, `n` seeds
/// all showing one happens with chance `(1 − CLAIM_FRACTION)^n`; `SEEDS` is the least `n` putting
/// that below `1 − CLAIM_CONFIDENCE`. Gains on every seed then claim: at least 95% of seeds gain, at
/// 95% confidence.
const CLAIM_FRACTION: f64 = 0.05;
const CLAIM_CONFIDENCE: f64 = 0.95;

fn seeds() -> u64 {
    ((1.0 - CLAIM_CONFIDENCE).ln() / (1.0 - CLAIM_FRACTION).ln()).ceil() as u64
}

const POLICIES: [Policy; 3] = [Policy::Blind, Policy::Proximity, Policy::Oracle];

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn line(r: &PolicyReport) -> String {
    let q = r.completion.unwrap();
    format!(
        "{:<9} goodput {:>8.3}/s useful {:>8.1} cores | completion ms p50 {:>10.1} p99 {:>10.1} p99.9 {:>10.1} max {:>10.1} | rtt {:>6.1} ms moved {:>5} queue {:>5}",
        format!("{:?}", r.policy),
        r.goodput_per_s(),
        r.useful_cores(),
        ms(q.p50),
        ms(q.p99),
        ms(q.p999),
        ms(q.max),
        r.mean_rtt_ms(),
        r.moved,
        r.queue_peak
    )
}

fn class_lines(r: &PolicyReport) -> String {
    CLASSES
        .iter()
        .zip(&r.classes)
        .map(|(class, c)| {
            let q = c.completion.unwrap();
            format!(
                "{:<11} placed {:>5} done {:>5} goodput {:>8.3}/s p50 {:>10.1} p99 {:>10.1} ms",
                format!("{class:?}"),
                c.placements,
                c.done_by_horizon,
                r.class_goodput_per_s(*class),
                ms(q.p50),
                ms(q.p99)
            )
        })
        .collect::<Vec<_>>()
        .join("\n      ")
}

/// Proximity's gain over blind: (goodput %, p99 cut %), overall and for each kind.
fn gains(blind: &PolicyReport, near: &PolicyReport) -> [(f64, f64); 4] {
    let gain = |b: f64, n: f64| 100.0 * (n / b - 1.0);
    let cut = |b: u64, n: u64| 100.0 * (1.0 - n as f64 / b as f64);
    let mut out = [(
        gain(blind.goodput_per_s(), near.goodput_per_s()),
        cut(blind.completion.unwrap().p99, near.completion.unwrap().p99),
    ); 4];
    for (i, class) in CLASSES.iter().enumerate() {
        out[i + 1] = (
            gain(
                blind.class_goodput_per_s(*class),
                near.class_goodput_per_s(*class),
            ),
            cut(
                blind.classes[i].completion.unwrap().p99,
                near.classes[i].completion.unwrap().p99,
            ),
        );
    }
    out
}

fn main() {
    // G2's recorded day on each fleet: its launches (owners, kinds, needs with overheads, arrivals,
    // lifetimes) and its node rows.
    let fleets = [
        (
            "AllKinds",
            include_str!("../../../bench/sched/huawei-r5-day30.trace"),
        ),
        (
            "Mixed",
            include_str!("../../../bench/sched/huawei-r5-day30.fleet.trace"),
        ),
    ]
    .map(|(name, text)| (name, parse(text, &Limits::default()).unwrap()));
    for (fleet, recorded) in &fleets {
        println!(
            "recorded {fleet}: {} node rows, {} owners, {} launches; overheads by kind (bytes, or by guest size) {:?}",
            recorded.nodes.len(),
            recorded.owners.len(),
            recorded.workloads.len(),
            recorded.overheads
        );
    }
    let seeds = seeds();
    println!(
        "seeds: {seeds} (at least {:.0}% of seeds gain, at {:.0}% confidence)",
        100.0 * (1.0 - CLAIM_FRACTION),
        100.0 * CLAIM_CONFIDENCE
    );
    let runs = [
        ("RECORD", Homes::Uniform, Access::Bound),
        (
            "SENSITIVITY skewed homes (not of record)",
            Homes::Skewed,
            Access::Bound,
        ),
        (
            "SENSITIVITY modelled WAN (not of record)",
            Homes::Uniform,
            Access::Modelled,
        ),
    ];
    for (label, homes, access) in runs {
        // Least gains of proximity over blind (goodput %, p99 cut %): all workloads, then each kind.
        let mut least = [(f64::MAX, f64::MAX); 4];
        let (mut studies, mut max_rounds) = (0u64, 0usize);
        let mut oracle_gap: f64 = 0.0;
        for (fleet, recorded) in &fleets {
            for seed in 1..=seeds {
                let config = StudyConfig {
                    access,
                    homes,
                    pick: Pick::default(),
                    seed,
                };
                let study = Study::generate(&config, recorded).unwrap();
                let accuracy = Accuracy::measure(&study.wan, &study.coordinates).unwrap();
                max_rounds = max_rounds.max(study.coordinates.rounds());
                println!(
                    "== {label}: {fleet}, seed {seed}: horizon {:.1} s, coordinates {} rounds, median error {:.1}%, region ranked first {}/{}",
                    study.horizon as f64 / 1e9,
                    study.coordinates.rounds(),
                    accuracy.median_relative_error * 100.0,
                    accuracy.region_ranked_first,
                    accuracy.region_trials
                );
                let reports: Vec<PolicyReport> =
                    POLICIES.iter().map(|&p| study.run(p).unwrap()).collect();
                for r in &reports {
                    println!("   {}", line(r));
                    println!("      {}", class_lines(r));
                }
                let g = gains(&reports[0], &reports[1]);
                println!(
                    "   GAIN {fleet} seed {seed}: {}",
                    ["all", "Oci", "Firecracker", "Shards"]
                        .iter()
                        .zip(g)
                        .map(|(n, (gp, p))| format!("{n} ({gp:+.1}, {p:+.1})"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                for (k, (gp, p)) in g.iter().enumerate() {
                    least[k] = (least[k].0.min(*gp), least[k].1.min(*p));
                }
                studies += 1;
                oracle_gap = oracle_gap
                    .max(100.0 * (reports[2].goodput_per_s() / reports[1].goodput_per_s() - 1.0));
            }
        }
        println!(
            "\nSUMMARY {label}: {studies} studies; most convergence rounds {max_rounds}; oracle's goodput over proximity's at most {oracle_gap:.2}%"
        );
        for (k, name) in ["all", "Oci", "Firecracker", "Shards"].iter().enumerate() {
            println!(
                "LEAST {label} {name:<11}: goodput {:+.1}%, p99 cut {:+.1}%",
                least[k].0, least[k].1
            );
        }
        println!();
    }
}
