//! What a committed entry costs at each depth of a group's writes on hyper-log (`docs/durable.md`
//! §14, item 1): the flushes and the latency at depths one to three, closed and open loop, for one
//! group, and for many groups sharing their members' logs: whether a group's third write out earns
//! its place as the log's third pipeline frame does for the log.
//!
//! ```text
//! cargo bench -p hyper-durable --bench depth -- DIR [CLIENTS] [LOADS] [ROUNDS] [DEVICES] [ENTRIES] [GROUPS]
//! ```
//!
//! Three voters on hyper-log, every member's log on one kind of device (`DEVICES`, `file,sim` by
//! default), driven as `support` states. `GROUPS` (comma-separated, `1` by default) is how many
//! groups the three logs hold, member `id` of every group on log `id`, as a node's log holds every
//! group it hosts; each group's member 1 leads.
//!
//! - Closed loop (`CLIENTS`, comma-separated, `1,4,16,64` by default, the pipeline bench's): that
//!   many proposals out at each group's leader at once, each proposed again when the leader
//!   applies it; each latency is its proposal to its answer, and the numbers are labelled closed
//!   loop (`docs/tails.md` §3.1).
//! - `ENTRIES` are measured a run, a group where there are many, after each group's first
//!   [`support::WARM`].
//! - Open loop (`LOADS`, percentages of the device's capacity, `25,50,75,90` by default): proposals
//!   arrive as a Poisson process at that share of the most entries a second any depth's closed
//!   loop reached on the device in this run, the `k`th to group `k mod GROUPS`; each latency
//!   counts from its arrival, whenever the driver got to propose it.
//!
//! Each point runs every depth `ROUNDS` times (3 by default), the depths' order rotated each
//! round, each run on fresh logs, measuring `ENTRIES` (by default the run length a p99.9 needs for
//! an upper end at 95% coverage, shared among the rounds). Latencies are pooled over the rounds
//! and stated with their intervals (`docs/tails.md` §3.3); the rest are medians over the rounds.
//! The load average is read before and after each point.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod support;

use std::path::{Path, PathBuf};

use support::{Device, HEAD, Mode, Run, Shape, Wakes};

/// The depths a group's writes may take: one to the log's pipeline frames.
const DEPTHS: [usize; 3] = [1, 2, 3];

/// Every depth of one point, `rounds` times rotated; returns the most entries a second a depth
/// reached.
fn point(
    wakes: &Wakes,
    dir: &Path,
    device: Device,
    groups: usize,
    mode: Mode,
    rounds: usize,
    entries: u64,
) -> f64 {
    let before = support::load();
    let mut runs: Vec<Vec<Run>> = DEPTHS.iter().map(|_| Vec::new()).collect();
    for round in 0..rounds {
        for k in 0..DEPTHS.len() {
            let at = (k + round) % DEPTHS.len();
            let shape = Shape::on(device, DEPTHS[at]);
            // One schedule a round for every depth: the depths meet the same arrivals.
            let seed = 0x6465_7074 ^ ((round as u64) << 8);
            runs[at].push(if groups == 1 {
                support::run(wakes, dir, shape, mode, entries, seed)
            } else {
                // `entries` a group, so every group commits as many as one group's run measures.
                let total = entries * groups as u64;
                support::run_groups(wakes, dir, groups, shape, mode, total, seed)
            });
        }
    }
    let label = match mode {
        Mode::Closed(clients) if groups > 1 => format!("closed loop, {clients} client(s) a group"),
        _ => mode.label(),
    };
    let each = if groups > 1 { " a group" } else { "" };
    println!(
        "\n**{device:?}, {groups} group(s), {label}; {entries} entries{each} a run, {rounds} rounds; load {before} → {}**\n",
        support::load()
    );
    println!("| depth {HEAD}\n|---|---|---|---|---|---|---|---|---|");
    let mut most = 0.0f64;
    for (k, depth_runs) in runs.iter().enumerate() {
        let (row, rate) = support::row(depth_runs);
        most = most.max(rate);
        println!("| {} {row}", DEPTHS[k]);
    }
    let rates = |runs: &[Run]| -> Vec<f64> { runs.iter().map(|r| r.entries_per_second).collect() };
    let p50s = |runs: &[Run]| -> Vec<f64> { runs.iter().map(support::run_p50).collect() };
    println!();
    for k in 1..DEPTHS.len() {
        println!(
            "- depth {} against depth {}: entries a second {}; each run's p50 {}",
            DEPTHS[k],
            DEPTHS[0],
            support::verdict(&rates(&runs[0]), &rates(&runs[k])),
            support::verdict(&p50s(&runs[0]), &p50s(&runs[k]))
        );
    }
    most
}

fn main() {
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--bench")
        .collect();
    let dir = PathBuf::from(args.first().expect("a directory for the logs' files"));
    let clients: Vec<usize> = support::list(args.get(1), &[1, 4, 16, 64]);
    let loads: Vec<u64> = support::list(args.get(2), &[25, 50, 75, 90]);
    let rounds: usize = args.get(3).map_or(3, |r| r.parse().unwrap());
    let devices: Vec<Device> = args.get(4).map_or(vec![Device::File, Device::Sim], |d| {
        d.split(',')
            .map(|d| match d.trim() {
                "file" => Device::File,
                "sim" => Device::Sim,
                other => panic!("device {other}: file or sim"),
            })
            .collect()
    });
    let entries: u64 = args
        .get(5)
        .map_or_else(|| support::default_entries(rounds), |n| n.parse().unwrap());
    let group_counts: Vec<usize> = support::list(args.get(6), &[1]);
    let wakes = Wakes::for_groups(group_counts.iter().copied().max().unwrap_or(1));
    for &groups in &group_counts {
        for &device in &devices {
            let mut capacity = 0.0f64;
            for &c in &clients {
                capacity = capacity.max(point(
                    &wakes,
                    &dir,
                    device,
                    groups,
                    Mode::Closed(c),
                    rounds,
                    entries,
                ));
            }
            for &l in &loads {
                let rate = capacity * l as f64 / 100.0;
                point(
                    &wakes,
                    &dir,
                    device,
                    groups,
                    Mode::Open(rate),
                    rounds,
                    entries,
                );
            }
        }
    }
}
