//! AC-T7.4.3's measure: stranded capacity at 90% load, Hex's scorer against best fit alone (G2 step 4).
//!
//!   cargo bench -p hex-bench --bench sched_packing
//!
//! It packs the recorded workload (bench/sched/huawei_workload.md), each launch an OCI container, a
//! Firecracker microVM or a Shards microVM, on each of its two node models: EC2 m5d.metal hosts that
//! run every class (huawei-r5-day30.trace, the main one) and Hex's fleet's host types five times
//! over, each running the classes its OS runs (huawei-r5-day30.fleet.trace); and, as context, on the
//! fleet as it is, one of each host. On each, it packs the mix and each class alone
//! (`hex_sim::packing::alone`), from `hex_sim::packing::SEEDS` starting arrivals drawn uniformly from
//! the day (`hex_sim::packing::start`), to 90% load through the fast path's pick
//! (`hex_sim::packing::pack`), three ways: with no completions, so lifetimes don't enter; and with
//! completions, once with lifetimes as recorded and once with Huawei's one-minute keep-alive added
//! (the `.keepalive.trace` beside each).
//! At the recorded speed the day never loads the nodes to 90%, so arrivals come faster: for each set
//! and trace, the slowest whole speed at which Hex and both best fits reach 90% on every seed, so
//! lifetimes enter as far as the nodes allow: doubling from the recorded speed finds one at which
//! they do, and every speed below it is packed. It prints each speed it tries: the seeds reaching
//! 90% by arm, the fewest completions on the way, the seeds on which Hex meets the bar, and the seeds
//! on which a best fit strands nothing (so none can be cut).
//!
//! Each clock runs under each arm: best fit by free CPU first (`BestFit`) and across resources
//! (`MostAllocated`), Tetris's alignment (`Align`), and least stranded then a tie: Tetris's
//! alignment (the plan's), least allocated, most allocated, or the fewest classes then most
//! allocated (Hex's, `Scorer::HEX`). For each arm and seed it prints the stranded capacity, the
//! arrivals passed over on the way and of what kind, the completions, and the load at the first
//! refusal and at the first refusal for room; then each arm's reduction against both best fits. It
//! is deterministic: the same trace gives the same tables.
// A bench (not production, CLAUDE.md): it takes the test-style allowances, as the other benches do.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss
)]

use hex_sim::index::{Pick, Scorer, Shapes, Tie};
use hex_sim::model::{Class, Cluster, Time};
use hex_sim::packing::{
    Clock, LOAD_PER_MILLE, PackReport, SEEDS, alone, minimal_shapes, pack, start,
};

const ARMS: [(&str, Scorer); 7] = [
    ("best_fit", Scorer::BestFit),
    ("most_allocated", Scorer::MostAllocated),
    ("align", Scorer::Align),
    ("stranded_then_align", Scorer::Fragment(Tie::Align)),
    ("stranded_then_least", Scorer::Fragment(Tie::LeastAllocated)),
    ("stranded_then_most", Scorer::Fragment(Tie::MostAllocated)),
    ("stranded_then_narrowest (Hex)", Scorer::HEX),
];

fn cut(arm: &PackReport, base: &PackReport) -> f64 {
    let base = base.stranded_per_mille();
    if base == 0 {
        0.0
    } else {
        100.0 * (1.0 - arm.stranded_per_mille() as f64 / base as f64)
    }
}

fn load_at(at: Option<u64>) -> String {
    at.map_or_else(|| "-".to_string(), |l| l.to_string())
}

fn trace(name: &str) -> Cluster {
    let path = format!("{}/../../bench/sched/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    hex_sim::replay::parse(&text, &hex_sim::replay::Limits::default()).unwrap()
}

fn main() {
    record();
}

/// The record: each node model's sets and clocks, measured under every arm.
fn record() {
    for (model, name, hosts) in [
        ("m5d.metal", "huawei-r5-day30", None),
        ("fleet", "huawei-r5-day30.fleet", None),
        // The fleet as it is, one of each host, as context (bench/sched/packing.md): the fleet trace
        // lists its five copies in order, so its first 10 nodes are one of each.
        ("fleet as it is", "huawei-r5-day30.fleet", Some(10)),
    ] {
        let load = |file: String| {
            let mut day = trace(&file);
            if let Some(hosts) = hosts {
                day.nodes.truncate(hosts);
            }
            day
        };
        let recorded = load(format!("{name}.trace"));
        let kept_alive = load(format!("{name}.keepalive.trace"));
        for (set, class) in [
            ("mix", None),
            ("oci", Some(Class::Oci)),
            ("firecracker", Some(Class::Firecracker)),
            ("shards", Some(Class::Shards)),
        ] {
            let set = format!("{model} {set}");
            let as_set = |day: &Cluster| match class {
                None => day.clone(),
                Some(class) => alone(day, class).unwrap(),
            };
            let (recorded, kept_alive) = (as_set(&recorded), as_set(&kept_alive));
            measure(&set, "no completions", &recorded, Clock::Held);
            for (lives, day) in [
                ("lifetimes as recorded", &recorded),
                ("with the keep-alive", &kept_alive),
            ] {
                let Some(speed) = slowest(&set, lives, day, pick(day)) else {
                    println!(
                        "{set}, {lives}: no speed reaches 90% on every seed: with no completions, some seed doesn't"
                    );
                    continue;
                };
                measure(
                    &set,
                    &format!("completions at {speed}x, {lives}"),
                    day,
                    Clock::Compressed(speed),
                );
            }
        }
    }
}

fn pick(cluster: &Cluster) -> Pick {
    Pick {
        shapes: Shapes::of(minimal_shapes(cluster)).unwrap(),
        ..Pick::default()
    }
}

/// `f` of every seed, in seed order, the seeds spread over the host's threads: a pack is a function
/// of its seed alone, so the tables don't depend on how they're spread.
fn per_seed<T: Send>(f: impl Fn(u64) -> T + Sync) -> Vec<T> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let seeds: Vec<u64> = (0..SEEDS).collect();
    let f = &f;
    std::thread::scope(|s| {
        let parts: Vec<_> = seeds
            .chunks(seeds.len().div_ceil(threads))
            .map(|part| s.spawn(move || part.iter().map(|&seed| f(seed)).collect::<Vec<T>>()))
            .collect();
        parts
            .into_iter()
            .flat_map(|part| part.join().unwrap())
            .collect()
    })
}

/// Each seed packed at `clock` under Hex and both best fits, in that order.
fn hex_and_best_fits(cluster: &Cluster, pick: Pick, clock: Clock) -> Vec<[PackReport; 3]> {
    per_seed(|seed| {
        let from = start(seed, cluster.workloads.len());
        [Scorer::HEX, Scorer::BestFit, Scorer::MostAllocated].map(|scorer| {
            pack(
                cluster,
                &Pick { scorer, ..pick },
                LOAD_PER_MILLE,
                from,
                clock,
            )
            .unwrap()
        })
    })
}

/// Packs every seed at `clock` under Hex and both best fits and prints the row: the seeds reaching
/// 90% by arm, the fewest completions by arm, the seeds on which Hex meets the bar, and the seeds on
/// which a best fit strands nothing. Whether every seed reached 90% under all three.
fn probe(cluster: &Cluster, pick: Pick, clock: Clock) -> bool {
    let mut reached = [0u64; 3];
    let mut done = [u64::MAX; 3];
    let (mut meets, mut nothing) = (0, 0);
    for runs in hex_and_best_fits(cluster, pick, clock) {
        for (i, p) in runs.iter().enumerate() {
            reached[i] += u64::from(p.reached);
            done[i] = done[i].min(p.completions);
        }
        meets += u64::from(runs[1..].iter().all(|b| cut(&runs[0], b) >= 30.0));
        nothing += u64::from(runs[1..].iter().any(|b| b.stranded_per_mille() == 0));
    }
    let speed = match clock {
        Clock::Held => "held".to_string(),
        Clock::Compressed(speed) => format!("{speed}x"),
    };
    println!("{speed:>6} | {reached:?} | {done:?} | {meets}/{SEEDS} | {nothing}");
    reached == [SEEDS; 3]
}

/// The slowest whole speed at which Hex and both best fits reach 90% on every seed, so lifetimes
/// enter as far as the nodes allow. None if they don't with no completions at all, which every
/// speed comes to once the day's arrivals fall within a nanosecond. Else doubling from the recorded
/// speed finds one at which they do, and every speed below it is packed: the first at which they do
/// is the slowest, with no assumption that reaching 90% gets easier as the speed rises (it doesn't
/// always: a seed of one arm can reach it at one speed and miss it at the next). Each speed tried
/// prints its row.
fn slowest(set: &str, lives: &str, cluster: &Cluster, pick: Pick) -> Option<Time> {
    println!(
        "speeds, {set}, {lives}: speed | seeds reaching 90% (Hex, best_fit, most_allocated) | fewest completions | seeds where Hex meets the bar | seeds where a best fit strands nothing, so none can be cut"
    );
    if !probe(cluster, pick, Clock::Held) {
        return None;
    }
    let mut reaches: Time = 1;
    while !probe(cluster, pick, Clock::Compressed(reaches)) {
        reaches = reaches.checked_mul(2)?;
    }
    // The doubling's speeds below `reaches` fell short already.
    (1..reaches)
        .filter(|speed| !speed.is_power_of_two())
        .find(|&speed| probe(cluster, pick, Clock::Compressed(speed)))
        .or(Some(reaches))
}

fn measure(set: &str, name: &str, cluster: &Cluster, clock: Clock) {
    let pick = pick(cluster);
    let shapes = minimal_shapes(cluster);
    println!(
        "== {set}, {name}: {} nodes, {} launches, minimal shapes {shapes:?}",
        cluster.nodes.len(),
        cluster.workloads.len()
    );
    println!(
        "seed from arm | stranded per-mille (* = 90% load not reached) | passed over: all [oci, firecracker, shards] | completions | first refusal at load per-mille | first refusal for room"
    );
    let runs: Vec<Vec<PackReport>> = per_seed(|seed| {
        let from = start(seed, cluster.workloads.len());
        ARMS.iter()
            .map(|&(_, scorer)| {
                pack(
                    cluster,
                    &Pick { scorer, ..pick },
                    LOAD_PER_MILLE,
                    from,
                    clock,
                )
                .unwrap()
            })
            .collect()
    });
    for (seed, arms) in (0..SEEDS).zip(&runs) {
        let from = start(seed, cluster.workloads.len());
        for ((arm, _), p) in ARMS.iter().zip(arms) {
            println!(
                "{seed:>4} {from:>5} {arm} | {}{} | {} {:?} | {} | {} | {}",
                p.stranded_per_mille(),
                if p.reached { "" } else { "*" },
                p.passed_over,
                p.passed_over_by_class,
                p.completions,
                load_at(p.first_refusal_per_mille),
                load_at(p.first_full_per_mille)
            );
        }
    }
    println!(
        "arm | seeds at least 30% below both best fits | least cut vs best_fit, vs most_allocated | passed over: all [oci, firecracker, shards] | earliest first refusal | earliest first refusal for room"
    );
    for (i, (arm, _)) in ARMS.iter().enumerate() {
        let cuts: Vec<(f64, f64)> = runs
            .iter()
            .map(|arms| (cut(&arms[i], &arms[0]), cut(&arms[i], &arms[1])))
            .collect();
        let meets = cuts
            .iter()
            .filter(|(b, m)| *b >= 30.0 && *m >= 30.0)
            .count();
        let least_best = cuts.iter().map(|c| c.0).fold(f64::INFINITY, f64::min);
        let least_most = cuts.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
        let passed: u64 = runs.iter().map(|arms| arms[i].passed_over).sum();
        let mut by_class = [0u64; 3];
        for arms in &runs {
            for (all, n) in by_class.iter_mut().zip(arms[i].passed_over_by_class) {
                *all += n;
            }
        }
        let first = runs
            .iter()
            .filter_map(|arms| arms[i].first_refusal_per_mille)
            .min();
        let first_full = runs
            .iter()
            .filter_map(|arms| arms[i].first_full_per_mille)
            .min();
        println!(
            "{set}, {name}: {arm} | {meets}/{SEEDS} | {least_best:.1}%, {least_most:.1}% | {passed} {by_class:?} | {} | {}",
            load_at(first),
            load_at(first_full)
        );
    }
}
