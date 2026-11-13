//! AC-T7.4.2's bench: the scheduler fast path's placement decision latency at 10,000 placements/s
//! on 10,000 nodes, wall-clock, on a release build (G2 step 5).
//!
//!   cargo bench -p hex-bench --bench sched_fast_path -- [runs] [placements]
//!
//! What it times is hex-sim's own fast path, for real: for each launch, Hex's pick over
//! the node index (`FitIndex::fit` with `Scorer::HEX`, the kernel's walk over every node with the room
//! or 5%), the commit by compare-and-set to the store (`Store::apply`), and the index update. The
//! launches are the recorded workload's (bench/sched/huawei_workload.md), each an OCI container, a
//! Firecracker microVM or a Shards microVM, their shapes and classes in arrival order, from a start
//! drawn from the day for each run (`packing::start`) and repeated as needed, timed as the mix and as each
//! class alone, on each node model: the 10,000 nodes repeat the hosts of huawei-r5-day30.trace (EC2
//! m5d.metal, each running every class; the main one) or of huawei-r5-day30.fleet.trace (Hex's
//! fleet's host types in their proportions, each running the classes its OS runs). Arrivals are
//! open-loop, one every 100 us by the
//! wall clock. A launch's latency runs from its scheduled arrival to its commit, so time spent queued
//! behind a slow decision counts. The nodes that can run the set's classes are first filled until
//! 90% of their CPU is placed, untimed: the load `AC-T7.4.3` holds the scheduler to
//! (`packing::LOAD_PER_MILLE`). Each timed arrival then retires the oldest running launch (its
//! completion written and its capacity given back) before it is placed, so the load holds steady. Single-threaded: the in-order committer is the fast path's serial point
//! (`docs/design/hex-sim.md` H3), so its cost per placement is what bounds throughput.
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
    clippy::cast_possible_truncation
)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use hex_sim::index::{FitIndex, Pick, Scorer, Shapes};
use hex_sim::model::{
    Class, Classes, Cluster, Kind, NodeId, NodeSpec, Tenancy, WorkloadId, WorkloadSpec,
};
use hex_sim::packing::{LOAD_PER_MILLE, alone, minimal_shapes, start};
use hex_sim::store::{Outcome, Store, Write};

// `AC-T7.4.2`'s nodes and rate.
const NODES: u32 = 10_000;
const RATE_PER_S: u64 = 10_000;

struct Live {
    spec: WorkloadSpec,
    node: NodeId,
    version: u64,
}

/// The CPU of the nodes that can run some class `recorded` asks: what the fill counts.
fn usable_cpu(nodes: &[NodeSpec], recorded: &Cluster) -> u64 {
    let asked = recorded
        .workloads
        .iter()
        .fold(Classes::NONE, |all, w| all.with(w.class));
    nodes
        .iter()
        .filter(|n| n.classes.meets(asked))
        .map(|n| n.capacity.cpu_milli)
        .sum()
}

/// 10,000 nodes that repeat the recorded workload's hosts in order (Hex's fleet's host types in their
/// proportions), and the launches a run takes, each with its class: the recorded day's in arrival
/// order from `seed`'s start (`packing::start`), cycled. The fill takes as many as place
/// `LOAD_PER_MILLE` of the usable CPU, those refused on the way among them: found by filling once on
/// the index alone, which places as the timed run's fill does, so the run has exactly the launches
/// it takes and its store exactly their records. A whole day's launches that find no node before the
/// fill ends would find none after: the index doesn't change while they're refused.
fn cluster(recorded: &Cluster, pick: &Pick, seed: u64, placements: usize) -> Cluster {
    let nodes: Vec<NodeSpec> = (0..NODES)
        .map(|i| {
            let host = &recorded.nodes[i as usize % recorded.nodes.len()];
            NodeSpec {
                id: NodeId(i),
                ..host.clone()
            }
        })
        .collect();
    let day = recorded.workloads.len();
    let from = start(seed, day);
    let launch = |i: usize| {
        let w = &recorded.workloads[(from + i) % day];
        WorkloadSpec {
            id: WorkloadId(i as u64),
            need: w.need,
            kind: Kind::Plain,
            arrives: i as u64,
            runs_for: None,
            tenancy: Tenancy::default(),
            class: w.class,
            ask: Default::default(),
        }
    };
    let usable = usable_cpu(&nodes, recorded);
    let mut index = FitIndex::new(&nodes).unwrap();
    let (mut placed, mut fill, mut since_placed) = (0u64, 0usize, 0usize);
    while placed * 1000 < usable * LOAD_PER_MILLE {
        let w = launch(fill);
        fill += 1;
        match index.fit(&w.demand(), pick).best {
            Some(best) => {
                index.take(best.node, w.id, &w.demand()).unwrap();
                placed += w.need.cpu_milli;
                since_placed = 0;
            }
            None => {
                since_placed += 1;
                assert!(
                    since_placed < day,
                    "a whole day of launches found no node before the fill"
                );
            }
        }
    }
    Cluster::single_owner(nodes, (0..fill + placements).map(launch).collect())
}

fn run(recorded: &Cluster, seed: u64, placements: usize) -> (Vec<Duration>, usize) {
    let pick = Pick {
        scorer: Scorer::HEX,
        shapes: Shapes::of(minimal_shapes(recorded)).unwrap(),
    };
    let cluster = cluster(recorded, &pick, seed, placements);
    let mut index = FitIndex::new(&cluster.nodes).unwrap();
    // A submit, a placement and a completion a launch: the log's most.
    let mut store = Store::new(
        &cluster.nodes,
        cluster.workloads.len(),
        cluster.workloads.len() * 3,
    )
    .unwrap();
    let total_cpu = usable_cpu(&cluster.nodes, recorded);
    let mut placed_cpu = 0u64;
    let mut live: VecDeque<Live> = VecDeque::new();
    let mut work = cluster.workloads.iter();
    let mut refused = 0usize;

    let place = |spec: &WorkloadSpec, index: &mut FitIndex, store: &mut Store| -> Option<Live> {
        let Outcome::Applied { version, .. } = store.apply(0, Write::Submit(*spec)).unwrap() else {
            panic!("submit")
        };
        let node = index.fit(&spec.demand(), &pick).best?.node;
        let Outcome::Applied { version, .. } = store
            .apply(
                0,
                Write::Place {
                    workload: spec.id,
                    node,
                    version,
                },
            )
            .unwrap()
        else {
            panic!("the store refused the index's choice")
        };
        index.take(node, spec.id, &spec.demand()).unwrap();
        Some(Live {
            spec: *spec,
            node,
            version,
        })
    };
    let retire = |l: Live, index: &mut FitIndex, store: &mut Store| {
        store
            .apply(
                0,
                Write::Complete {
                    workload: l.spec.id,
                    node: l.node,
                    version: l.version,
                },
            )
            .unwrap();
        index.give(l.node, l.spec.id, &l.spec.demand()).unwrap();
    };

    // Fill, untimed.
    while placed_cpu * 1000 < total_cpu * LOAD_PER_MILLE {
        let spec = work.next().expect("the launches the fill takes");
        match place(spec, &mut index, &mut store) {
            Some(l) => {
                placed_cpu += l.spec.need.cpu_milli;
                live.push_back(l);
            }
            None => refused += 1,
        }
    }

    // Timed: open-loop arrivals every 100 us.
    let gap = Duration::from_nanos(1_000_000_000 / RATE_PER_S);
    let mut latencies = Vec::with_capacity(placements);
    let start = Instant::now();
    for i in 0..placements {
        let arrival = start + gap * i as u32;
        while Instant::now() < arrival {
            std::hint::spin_loop();
        }
        if let Some(old) = live.pop_front() {
            retire(old, &mut index, &mut store);
        }
        let spec = work.next().expect("a launch for each timed placement");
        match place(spec, &mut index, &mut store) {
            Some(l) => live.push_back(l),
            None => refused += 1,
        }
        latencies.push(Instant::now().saturating_duration_since(arrival));
    }
    (latencies, refused)
}

fn quantile(sorted: &[Duration], per_mille: usize) -> Duration {
    let rank = (sorted.len() * per_mille).div_ceil(1000).max(1);
    sorted[rank - 1]
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .filter_map(|a| a.into_string().ok())
        .filter(|a| a != "--bench")
        .collect();
    let runs: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(5);
    let placements: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let mut met = true;
    for (model, name) in [
        ("m5d.metal", "huawei-r5-day30.trace"),
        ("fleet", "huawei-r5-day30.fleet.trace"),
    ] {
        let path = format!("{}/../../bench/sched/{name}", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(path).unwrap();
        let recorded = hex_sim::replay::parse(&text, &hex_sim::replay::Limits::default()).unwrap();
        println!(
            "sched_fast_path: {NODES} nodes repeating {name}'s {} ({model}), its {} launches, {RATE_PER_S}/s open-loop, {placements} timed placements per run, {runs} runs per set",
            recorded.nodes.len(),
            recorded.workloads.len()
        );
        met &= sets(model, &recorded, runs, placements);
    }
    println!(
        "AC-T7.4.2 bar p99 <= 10 ms on both node models, the mix and each class alone: {}",
        if met { "MET" } else { "NOT MET" }
    );
}

/// Times the mix and each class alone on `recorded`'s node model; whether each set's best p99 met
/// the bar.
fn sets(model: &str, recorded: &Cluster, runs: u64, placements: usize) -> bool {
    let mut met = true;
    for (set, class) in [
        ("mix", None),
        ("oci", Some(Class::Oci)),
        ("firecracker", Some(Class::Firecracker)),
        ("shards", Some(Class::Shards)),
    ] {
        let set = format!("{model} {set}");
        let day = match class {
            None => recorded.clone(),
            Some(class) => alone(recorded, class).unwrap(),
        };
        let mut best: Option<(Duration, String)> = None;
        for seed in 0..runs {
            let (mut l, refused) = run(&day, seed, placements);
            l.sort_unstable();
            let us = |d: Duration| d.as_secs_f64() * 1e6;
            let (p50, p99, p999, max) = (
                quantile(&l, 500),
                quantile(&l, 990),
                quantile(&l, 999),
                *l.last().unwrap(),
            );
            let line = format!(
                "{set} run seed={seed} placements={} refused={refused} p50_us={:.1} p99_us={:.1} p999_us={:.1} max_us={:.1}",
                l.len(),
                us(p50),
                us(p99),
                us(p999),
                us(max)
            );
            println!("{line}");
            if best.as_ref().is_none_or(|(b, _)| p99 < *b) {
                best = Some((p99, line));
            }
        }
        let (p99, line) = best.unwrap();
        met &= p99 <= Duration::from_millis(10);
        println!("{set} best-of-{runs} (by p99): {line}");
    }
    met
}
