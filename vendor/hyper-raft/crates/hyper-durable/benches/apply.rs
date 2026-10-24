//! What a leader gains by applying its own term's committed entries before its own write of them is
//! durable (`docs/durable.md` §4.2, §14 item 2): commit-to-answer latency with it and without, on a
//! leader whose disk is the slowest of its quorum, and on a disk the members share.
//!
//! ```text
//! cargo bench -p hyper-durable --bench apply -- DIR [CLIENTS] [LOADS] [ROUNDS] [ENTRIES]
//! ```
//!
//! Three voters on hyper-log at the log's depth, driven as `support` states, in two shapes:
//!
//! - **Slowest leader**: the leader's log on a scratch file under `DIR`, the followers' on
//!   hyper-block's simulated device, whose flush costs nothing: a disk per member, the leader's
//!   the slowest by the whole of its flush, the bound of what applying ahead can gain.
//! - **Shared disk**: every member's log a scratch file under `DIR`, on the one disk.
//!
//! Each shape runs closed loop (`CLIENTS`, `1,16` by default) and open loop (`LOADS`, percentages of
//! the most entries a second the shape's closed loop reached, `50,90` by default), with the leader
//! applying ahead and without, `ROUNDS` times (3 by default) in turn, the order turned each round,
//! each run on fresh logs measuring `ENTRIES` (by default the run length a p99.9 needs for an upper
//! end at 95% coverage, shared among the rounds). Latencies are pooled over the rounds and stated
//! with their intervals (`docs/tails.md` §3.3); the rest are medians over the rounds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod support;

use std::path::{Path, PathBuf};

use support::{Device, HEAD, MEMBERS, Mode, Run, Shape, Wakes};

/// The log's depth: a group's writes out at once, as the shell runs them.
const DEPTH: usize = 3;

/// The two shapes, the leader's device first.
const SHAPES: [(&str, [Device; MEMBERS as usize]); 2] = [
    ("slowest leader", [Device::File, Device::Sim, Device::Sim]),
    ("shared disk", [Device::File, Device::File, Device::File]),
];

/// Both ways of one point, `rounds` times in turn; returns the most entries a second either
/// reached.
fn point(
    wakes: &Wakes,
    dir: &Path,
    (name, devices): (&str, [Device; MEMBERS as usize]),
    mode: Mode,
    rounds: usize,
    entries: u64,
) -> f64 {
    let before = support::load();
    let mut runs: [Vec<Run>; 2] = [Vec::new(), Vec::new()];
    for round in 0..rounds {
        let seed = 0x6170_706c ^ ((round as u64) << 8);
        for k in 0..2 {
            let at = (k + round) % 2;
            let shape = Shape {
                apply_ahead: at == 1,
                devices,
                ..Shape::on(Device::File, DEPTH)
            };
            runs[at].push(support::run(wakes, dir, shape, mode, entries, seed));
        }
    }
    println!(
        "\n**{name}, {}; {entries} entries a run, {rounds} rounds; load {before} → {}**\n",
        mode.label(),
        support::load()
    );
    println!("| leader applies | {HEAD}\n|---|---|---|---|---|---|---|---|---|");
    let mut most = 0.0f64;
    for (at, way) in ["after its own write", "ahead of its own write"]
        .iter()
        .enumerate()
    {
        let (row, rate) = support::row(&runs[at]);
        most = most.max(rate);
        println!("| {way} {row}");
    }
    let p50s = |runs: &[Run]| -> Vec<f64> { runs.iter().map(support::run_p50).collect() };
    let rates = |runs: &[Run]| -> Vec<f64> { runs.iter().map(|r| r.entries_per_second).collect() };
    println!(
        "\n- ahead against after: each run's p50 {}; entries a second {}",
        support::verdict(&p50s(&runs[0]), &p50s(&runs[1])),
        support::verdict(&rates(&runs[0]), &rates(&runs[1]))
    );
    for (at, way) in ["after", "ahead"].iter().enumerate() {
        println!("- {way}: {}", phases(&runs[at]));
        let mut held: Vec<u64> = runs[at]
            .iter()
            .flat_map(|r| r.held.iter().copied())
            .collect();
        held.sort_unstable();
        let total: u64 = held.iter().sum();
        let rate =
            |f: fn(&Run) -> f64| support::median(&mut runs[at].iter().map(f).collect::<Vec<_>>());
        println!(
            "  {way}: leader held a Ready at its depth {} times, p50 {:.2} ms, max {:.2} ms, {:.1} ms in all; readies {:.3} and empty {:.3} an entry",
            held.len(),
            held.get(held.len() / 2).copied().unwrap_or(0) as f64 / 1e6,
            held.last().copied().unwrap_or(0) as f64 / 1e6,
            total as f64 / 1e6,
            rate(|r| r.readies_per_entry),
            rate(|r| r.empty_per_entry)
        );
    }
    most
}

/// Where an entry's time went, for every entry and for those at or past the pooled p99: the median
/// time to its first leaving the leader, the median time from then to its answer, and how many
/// writes the leader had out as it was proposed.
fn phases(runs: &[Run]) -> String {
    let all: Vec<(u64, u64, usize)> = runs.iter().flat_map(|r| r.phases.iter().copied()).collect();
    let mut total: Vec<u64> = all.iter().map(|(a, b, _)| a + b).collect();
    total.sort_unstable();
    let p99 = hyper_measure::quantile::quantile(
        &total,
        hyper_measure::quantile::Level::P99,
        support::COVERAGE,
    )
    .map_or(u64::MAX, |q| q.estimate);
    let describe = |picked: Vec<(u64, u64, usize)>| -> String {
        let ms = |mut v: Vec<f64>| support::median(&mut v) / 1e6;
        let n = picked.len();
        let mut out = [0usize; 5];
        for (_, _, o) in &picked {
            out[(*o).min(4)] += 1;
        }
        format!(
            "{n} entries, to leave the leader {:.2} ms, then to answer {:.2} ms, writes out at proposal 0/1/2/3/4+: {:?}",
            ms(picked.iter().map(|p| p.0 as f64).collect()),
            ms(picked.iter().map(|p| p.1 as f64).collect()),
            out
        )
    };
    let tail: Vec<(u64, u64, usize)> = all
        .iter()
        .copied()
        .filter(|(a, b, _)| a + b >= p99)
        .collect();
    format!("all {}; at or past p99 {}", describe(all), describe(tail))
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let dir = PathBuf::from(args.first().expect("a directory for the logs' files"));
    let clients: Vec<usize> = support::list(args.get(1), &[1, 16]);
    let loads: Vec<u64> = support::list(args.get(2), &[50, 90]);
    let rounds: usize = args.get(3).map_or(3, |r| r.parse().unwrap());
    let entries: u64 = args
        .get(4)
        .map_or_else(|| support::default_entries(rounds), |n| n.parse().unwrap());
    let wakes = Wakes::new();
    for shape in SHAPES {
        let mut capacity = 0.0f64;
        for &c in &clients {
            capacity = capacity.max(point(&wakes, &dir, shape, Mode::Closed(c), rounds, entries));
        }
        for &l in &loads {
            let rate = capacity * l as f64 / 100.0;
            point(&wakes, &dir, shape, Mode::Open(rate), rounds, entries);
        }
    }
}
