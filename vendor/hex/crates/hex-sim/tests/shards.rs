//! The sharded control plane (`docs/design/hex-sim.md` H60, H61): a lost shard's slices taken over
//! by a live shard within `T-T7.11.1`'s 5 s, its running work undisturbed.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::time::Instant;

use hex_sim::SimError;
use hex_sim::fair::Policy;
use hex_sim::generate::{self, NodeModel, Shape};
use hex_sim::model::{Class, Cluster, Kind};
use hex_sim::sim::{self, Config, ControlPlane, Layout, Nemesis, Run};
use hex_sim::spec;

const SECOND: u64 = 1_000_000_000;

/// 1,000,000 workloads on 10,000 nodes, arriving over about 20 virtual seconds and running about 4 s
/// each: some 200,000 run at once, 20 a node, at a tenth of the cluster's CPU, so the queue stays
/// short and the serial specification's replay stays affordable. No GPU nodes and no benchmarks: at
/// 20 workloads a node a node's few GPUs are taken and nearly every node runs a build, which leaves
/// GPU work and benchmarks queued (S3's filters, tested in `tests/sim.rs`), not what this test is
/// about. The nodes are one of G2's node models (EC2 m5d.metal, the main one, or Hex's fleet beside
/// it); workloads take G2's rule, a third of them each OCI containers, Firecracker and Shards microVMs
/// by owner, at 75 launches an owner, each with its class's overhead (H69).
fn million(model: NodeModel) -> Cluster {
    let shape = Shape {
        nodes: 10_000,
        workloads: 1_000_000,
        mean_gap: 20_000,
        mean_run: Some(4 * SECOND),
        gpu_nodes_per_mille: 0,
        max_share_per_mille: 10,
    };
    let mut cluster = generate::cluster(7, &shape).unwrap();
    for w in &mut cluster.workloads {
        if w.kind == Kind::Benchmark {
            w.kind = Kind::Plain;
        }
    }
    cluster.nodes = model.nodes(10_000);
    generate::owners(&mut cluster, 7, 75).unwrap();
    generate::by_owner(&mut cluster, generate::overheads()).unwrap();
    cluster
}

/// Eight shards; node agents resend a done report every 500 ms up to 16 times (8 s, past the 5 s
/// takeover bound), so a report made while its shard is lost waits for the shard that takes over.
fn eight_shards(lose: Nemesis) -> Config {
    Config {
        shards: 8,
        // Submit order: G6's bars are of record under it, the only order when they were measured
        // (`docs/design/hex-sim.md`, "What it doesn't do yet").
        policy: Policy::Fifo,
        workers: 16,
        report_retry_ns: 500_000_000,
        report_attempts: 16,
        client_retry_ns: 500_000_000,
        client_attempts: 16,
        max_events: 1 << 24,
        nemesis: lose,
        ..Config::default()
    }
}

/// When shard 3 is lost: after every workload has been submitted, with most of the last ones running.
fn lose_after_submits(cluster: &Cluster) -> Nemesis {
    let last = cluster.workloads.last().unwrap().arrives;
    Nemesis::Lose {
        shard: 3,
        at: last + SECOND / 2 + 12_345,
    }
}

/// T-T7.11.1, on the main node model (10,000 EC2 m5d.metal): do 1,000,000 workloads submitted under 8
/// shards and lose shard 3 for good; expect lost-or-double 0, the lost shard's running workloads undisturbed (kept-running 0),
/// every slice's log one the serial specification makes, and its slice rebuilt by a live shard and
/// committing at most 5 s after the loss; and (non-vacuity) tens of thousands running on the lost
/// slice when it was lost and completions it recorded after the takeover.
#[test]
fn a_lost_shard_is_taken_over_within_five_seconds() {
    lose_a_shard(NodeModel::M5dMetal);
}

/// T-T7.11.1 on Hex's fleet beside it (its ten hosts repeated to 10,000 nodes, Firecracker on the
/// Linux two in ten): the same loss and the same expectations.
#[test]
fn a_lost_shard_is_taken_over_within_five_seconds_on_the_fleet() {
    lose_a_shard(NodeModel::Fleet);
}

fn lose_a_shard(model: NodeModel) {
    let cluster = million(model);
    let config = eight_shards(lose_after_submits(&cluster));
    let mut run = sim::run(&cluster, &config).unwrap();
    check_takeover(model, &cluster, &config, &mut run);
}

fn check_takeover(model: NodeModel, cluster: &Cluster, config: &Config, run: &mut Run) {
    let end = spec::check_end(cluster, run);
    assert_eq!(end.lost_or_double(), 0, "{end:?}");
    assert_eq!(end.complete, 1_000_000, "{end:?}");
    let kept = spec::check_kept_running(cluster, run);
    assert_eq!(kept.disturbed, 0, "{kept:?}");
    let slices = spec::check_slices(cluster, run).unwrap();
    assert_eq!(slices.len(), 8);
    // A run that kept no planes is checked against the layout it was made on, to the same verdicts.
    let layout = Layout::new(&cluster.nodes, config.shards).unwrap();
    // AC-T7.4.1: this oracle needs the same finished records, not a copied live FitIndex.
    let planes = std::mem::take(&mut run.planes);
    let bare = spec::check_layout(&layout, run);
    run.planes = planes;
    assert_eq!(bare.unwrap(), slices);
    assert!(slices.iter().all(|s| s.placements > 0), "{slices:?}");
    assert!(run.planes.iter().all(|p| p.pending() == 0));

    let [t] = run.report.takeovers.as_slice() else {
        panic!("takeovers: {:?}", run.report.takeovers);
    };
    assert_eq!((t.slice, t.lost), (3, 3), "{t:?}");
    assert_ne!(t.by, 3, "{t:?}");
    assert!(
        t.detection_ns <= config.lease_ns && t.detection_ns > config.lease_ns - config.renew_ns,
        "{t:?}"
    );
    assert_eq!(
        t.rebuild_ns,
        t.records * config.rebuild_ns_per_record,
        "{t:?}"
    );
    let takeover = t
        .takeover_ns
        .expect("the slice never committed after its takeover");
    assert!(takeover >= t.detection_ns + t.rebuild_ns, "{t:?}");
    assert!(takeover <= 5 * SECOND, "T-T7.11.1: {t:?}");
    // Non-vacuity: the lost slice held running work, and recorded it done after the takeover.
    // Every workload running on the lost slice was read back by the rebuild and recorded done after
    // the takeover.
    assert!(t.running > 0, "{t:?}");
    assert!(t.records >= t.running, "{t:?}");
    assert!(t.committed_after >= t.running, "{t:?}");
    // By class: every workload of each class done, and each class running on the lost slice when its
    // shard was lost.
    let (mut done, mut total, mut on_lost) = ([0u64; 3], [0u64; 3], [0u64; 3]);
    for (k, w) in cluster.workloads.iter().enumerate() {
        let c = [Class::Oci, Class::Firecracker, Class::Shards]
            .iter()
            .position(|&c| c == w.class)
            .unwrap();
        total[c] += 1;
        let Some(ran) = run.ran[k] else { continue };
        done[c] += u64::from(ran.ended.is_some());
        let running = ran.started <= t.lost_at && ran.ended.is_none_or(|e| e > t.lost_at);
        on_lost[c] += u64::from(running && layout.slice_of(ran.node) == Some(3));
    }
    assert_eq!(done, total, "done by class");
    assert!(
        on_lost.iter().all(|&n| n > 0),
        "{on_lost:?} running on the lost slice, by class"
    );
    println!(
        "T-T7.11.1 on {}: detection {} ms, rebuild of {} records {} ms, takeover {} ms; {} running on the \
         lost slice (OCI, Firecracker, Shards {on_lost:?}), {} commits after",
        model.name(),
        t.detection_ns / 1_000_000,
        t.records,
        t.rebuild_ns / 1_000_000,
        takeover / 1_000_000,
        t.running,
        t.committed_after,
    );
}

/// The measurement behind `sim::REBUILD_NS_PER_RECORD` (CLAUDE.md §5): do 1,000,000 workloads that
/// never end on 10,000 nodes under 8 shards, so each slice holds an eighth of them placed, then rebuild
/// slice 3's plane from that store five times; print the best wall time per record read. Run in release
/// on fedora: `cargo test -p hex-sim --release --test shards -- --ignored --nocapture rebuild_cost`.
#[test]
#[ignore = "a wall-clock measurement, run by hand in release"]
fn rebuild_cost() {
    let mut cluster = million(NodeModel::M5dMetal);
    for w in &mut cluster.workloads {
        w.runs_for = None;
    }
    let config = eight_shards(Nemesis::None);
    let run = sim::run(&cluster, &config).unwrap();
    let layout = Layout::new(&cluster.nodes, config.shards).unwrap();
    let mut best = u128::MAX;
    let mut records = 0;
    for _ in 0..5 {
        let start = Instant::now();
        let (plane, n) = ControlPlane::rebuild(&run.store, &cluster, &layout, 3, &config).unwrap();
        best = best.min(start.elapsed().as_nanos());
        records = n;
        drop(plane);
    }
    assert!(records > 0, "{records} records");
    println!(
        "rebuild of slice 3: {records} of {} records, best of 5 {best} ns, {} ns a record",
        run.store.records().count(),
        best / u128::from(records)
    );
}

/// The lease's learned-wait rule (H61, the lead 2026-11-06): do read the default configuration;
/// expect a shard to renew once per recorded store write round trip (Amphora's K0 write p99.9,
/// `sim::STORE_WRITE_RTT_NS`) and its lease to run ten of them (etcd's rule: an election timeout of at
/// least ten round trips), so T-T7.11.1's takeover follows the recorded figure, not a copied default.
#[test]
fn the_lease_is_ten_recorded_store_write_round_trips() {
    let c = Config::default();
    assert_eq!(sim::STORE_WRITE_RTT_NS, 22_270_000);
    assert_eq!(c.renew_ns, sim::STORE_WRITE_RTT_NS);
    assert_eq!(c.lease_ns, 10 * sim::STORE_WRITE_RTT_NS);
}

/// H60 beside `scheduler-fairness.md` F3 and F4: a gang's members are routed by their own ids, so a
/// slice may hold only some of them, and a disruption group's budget spans every slice while a
/// slice's plane sees its own. Do run a cluster with gangs, and one with preemption on, under two
/// shards; expect each refused, typed, naming why. Do the same on one shard; expect both to run.
#[test]
fn gangs_and_preemption_run_on_one_shard() {
    let shape = Shape {
        nodes: 8,
        workloads: 64,
        mean_gap: 1_000_000,
        mean_run: Some(SECOND),
        gpu_nodes_per_mille: 0,
        max_share_per_mille: 100,
    };
    let plain = generate::cluster(3, &shape).unwrap();
    let ganged = generate::gangs(plain.clone(), 3, 500, 4);
    assert!(ganged.workloads.iter().any(|w| w.tenancy.gang.is_some()));
    let two = Config {
        shards: 2,
        ..Config::default()
    };
    let preempting = Config {
        preemptions: 8,
        ..two
    };
    assert!(
        matches!(sim::run(&ganged, &two), Err(SimError::Config(why)) if why.starts_with("gangs run on one shard")),
        "gangs under two shards"
    );
    assert!(
        matches!(sim::run(&plain, &preempting), Err(SimError::Config(why)) if why.starts_with("preemption runs on one shard")),
        "preemption under two shards"
    );
    let one = Config {
        shards: 1,
        ..preempting
    };
    sim::run(&ganged, &Config::default()).unwrap();
    sim::run(&plain, &one).unwrap();
}
