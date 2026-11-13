//! G4's fair-share bar (CP-T7.5, `docs/design/scheduler-fairness.md`): four owners' shares of
//! a contended day, measured in hex-sim.
//! A workload is one of Hex's: an OCI container, a Firecracker microVM or a Shards microVM, and its
//! owner the tenant or app it runs for.

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

use hex_sim::fair::{self, Policy};
use hex_sim::generate::{self, Day, Tenant};
use hex_sim::model::Resources;
use hex_sim::model::{Class, Cluster, Kind};
use hex_sim::replay;
use hex_sim::sim::{self, Config};
use hex_sim::spec;

const GIB: u64 = 1 << 30;
const HOUR: u64 = 3_600_000_000_000;

/// Four equal-weight owners on 64 nodes of 16 cores and 64 GiB. In the ten busy hours they ask
/// for 60%, 50%, 45% and 45% of the cluster (200% in all), with workloads of different shapes (one
/// memory-heavy); out of them, 8% each, so the backlog drains by night. Each asks well past the 28%
/// DRF gives it here (three CPU-bound owners and one memory-bound one share the CPU), so each stays
/// backlogged while all four wait: on a day where two asked for 30%, their arrivals often fell below
/// that 28%, and an owner short of work holds less than its share under any policy
/// (docs/design/scheduler-fairness.md F1, evidence).
fn four_tenants() -> Day {
    let tenant = |cores, gib, busy_ppm| Tenant {
        weight: 1,
        cores,
        gib,
        busy_ppm,
        quiet_ppm: 80_000,
    };
    Day {
        nodes: 64,
        node: Resources {
            cpu_milli: 16_000,
            mem_bytes: 64 * GIB,
            gpus: 0,
            disk_bytes: 0,
        },
        tenants: vec![
            tenant((1, 1), (1, 2), 600_000),
            tenant((4, 4), (4, 8), 500_000),
            tenant((2, 2), (12, 16), 450_000),
            tenant((1, 8), (2, 16), 450_000),
        ],
        length: 24 * HOUR,
        busy: (9 * HOUR, 19 * HOUR),
        mean_run: 600_000_000_000,
        max_vms: 400_000,
    }
}

/// AC-T7.5.2: do a generated day of four equal-weight owners (its workloads a third each OCI
/// containers, Firecracker microVMs and Shards microVMs), each asking for more than its
/// quarter for ten hours; expect each owner's part of the capacity held while all four wait to
/// be within 2% of 25% (0.5 percentage points) under bands-then-DRF, over at least eight contended
/// hours; and (non-vacuity) submit order alone to miss that bar on the same day.
#[test]
fn four_equal_weight_owners_each_hold_a_quarter_under_contention() {
    let day = generate::day(4, &four_tenants()).unwrap();
    // The day carries all three of Hex's workloads, a third each.
    for class in [Class::Oci, Class::Firecracker, Class::Shards] {
        let n = day.workloads.iter().filter(|w| w.class == class).count();
        assert!(n * 3 + 3 > day.workloads.len(), "{class:?}: {n}");
    }
    let base = Config {
        workers: 4,
        max_events: 1 << 22,
        ..Config::default()
    };
    let mut worst = Vec::new();
    for policy in [Policy::Drf, Policy::Fifo] {
        let run = sim::run(&day, &Config { policy, ..base }).unwrap();
        let shares = fair::measure(&day, &run.ran, &run.refused, run.report.end).unwrap();
        eprintln!(
            "AC-T7.5.2 {policy:?}: workloads {} placements {} contended {:.2} h shares_ppm {:?} of_cluster_ppm {:?} worst {} ppm",
            day.workloads.len(),
            run.report.placements,
            shares.contended_ns as f64 / HOUR as f64,
            shares.shares_ppm,
            shares.of_cluster_ppm,
            shares.worst_from_equal_ppm()
        );
        assert!(shares.contended_ns >= 8 * HOUR, "{policy:?}: {shares:?}");
        worst.push(shares.worst_from_equal_ppm());
    }
    // 2% of 25% is 0.5 percentage points: 5,000 parts per million of the cluster's held share.
    assert!(
        worst[0] <= 5_000,
        "DRF: worst {} ppm from a quarter",
        worst[0]
    );
    assert!(
        worst[1] > 5_000,
        "FIFO met the bar too: the test can't fail"
    );
}

/// AC-T7.5.1 (gangs of workloads admitted whole), G4 step 3, over a generated day: do the contended day of
/// AC-T7.5.2 with a tenth of its workloads made gangs of 2 to 12 (most needing more than one node,
/// and arriving while every node is busy, among single workloads); expect no write to leave a gang part
/// placed (check_gangs' partial = 0), every gang placed in one write, nothing lost or started
/// twice, and (non-vacuity) hundreds of gangs that spanned nodes and waited for capacity placed.
#[test]
fn gangs_on_a_contended_day_are_placed_whole() {
    let day = generate::gangs(generate::day(4, &four_tenants()).unwrap(), 4, 100, 12);
    let run = sim::run(
        &day,
        &Config {
            workers: 4,
            max_events: 1 << 22,
            ..Config::default()
        },
    )
    .unwrap();
    let end = spec::check_end(&day, &run);
    assert_eq!(end.lost_or_double(), 0, "{end:?}");
    assert_eq!(
        end.complete + end.refused,
        day.workloads.len() as u64,
        "{end:?}"
    );
    let gangs = spec::check_gangs(&run);
    assert_eq!(gangs.partial, 0, "{gangs:?}");
    assert_eq!(gangs.placed, run.report.gangs);
    let node = day.nodes.first().unwrap().capacity;
    let index: std::collections::BTreeMap<_, _> = day
        .workloads
        .iter()
        .enumerate()
        .map(|(i, w)| (w.id, i))
        .collect();
    let mut spanned_and_waited = 0u64;
    for members in run.gangs.values() {
        let mut need = Resources::default();
        let mut waited = false;
        for m in members {
            let at = index[m];
            let w = &day.workloads[at];
            need = need.checked_add(&w.need).unwrap();
            if let Some(r) = run.ran[at] {
                waited |= r.started - w.arrives > 60_000_000_000;
            }
        }
        spanned_and_waited += u64::from(!node.holds(&need) && waited);
    }
    eprintln!(
        "AC-T7.5.1 day: workloads {} gangs {} placed {} partial {} spanned_and_waited {} refused {}",
        day.workloads.len(),
        run.gangs.len(),
        gangs.placed,
        gangs.partial,
        spanned_and_waited,
        end.refused
    );
    assert!(spanned_and_waited > 200, "only {spanned_and_waited}");
}

/// `cluster` written in the recorded-workload format (`replay.rs`), every workload with its tenancy.
fn as_trace(cluster: &Cluster) -> String {
    let mut out = String::from("# source: tests/fair.rs, a generated day\n");
    for n in &cluster.nodes {
        let c = n.capacity;
        out += &format!(
            "node,{},{},{},{}\n",
            c.cpu_milli, c.mem_bytes, c.gpus, n.background_milli
        );
    }
    for o in &cluster.owners {
        out += &format!("owner,{}\n", o.weight);
    }
    for g in &cluster.groups {
        out += &format!("group,{}\n", g.budget);
    }
    for w in &cluster.workloads {
        let kind = match w.kind {
            Kind::Plain => "plain",
            Kind::Build => "build",
            Kind::Benchmark => "benchmark",
            Kind::Session => "session",
        };
        let t = w.tenancy;
        let class = match w.class {
            Class::Oci => "oci",
            Class::Firecracker => "firecracker",
            Class::Shards => "shards",
        };
        let opt = |v: Option<u64>| v.map_or("-".to_string(), |v| v.to_string());
        out += &format!(
            "leg,{},{},{},{},{},{kind},{},{},{},{},{class}\n",
            w.arrives,
            w.runs_for.unwrap(),
            w.need.cpu_milli,
            w.need.mem_bytes,
            w.need.gpus,
            t.owner.0,
            t.band,
            opt(t.gang.map(|g| g.0)),
            opt(t.group.map(|g| u64::from(g.0))),
        );
    }
    out
}

/// Compare every field, reporting the first mismatched row instead of printing the whole day.
fn assert_same_cluster(read: &Cluster, expected: &Cluster) {
    fn rows<T: std::fmt::Debug + PartialEq>(kind: &str, read: &[T], expected: &[T]) {
        assert_eq!(read.len(), expected.len(), "{kind} count");
        for (i, (read, expected)) in read.iter().zip(expected).enumerate() {
            assert_eq!(read, expected, "{kind} {i}");
        }
    }
    rows("node", &read.nodes, &expected.nodes);
    rows("workload", &read.workloads, &expected.workloads);
    rows("owner", &read.owners, &expected.owners);
    rows("group", &read.groups, &expected.groups);
    rows("model", &read.models, &expected.models);
    assert_eq!(read.overheads, expected.overheads, "overheads");
}

/// AC-T7.5.2: do the generated day's node-to-trace-to-node path with one node and no workloads;
/// expect its advertised microVM classes and the virtualization they require to survive replay.
#[test]
fn ac_t7_5_2_a_days_node_capabilities_survive_trace_replay() {
    let day = Cluster::single_owner(
        vec![hex_sim::model::NodeSpec::new(
            hex_sim::model::NodeId(0),
            Resources {
                cpu_milli: 1,
                mem_bytes: 1,
                ..Resources::default()
            },
        )],
        Vec::new(),
    );
    let read = replay::parse(&as_trace(&day), &replay::Limits::default()).unwrap();
    assert_same_cluster(&read, &day);
}

/// AC-T7.5.2 over a recorded day: do hex-sim over the day in `HEX_SIM_RECORDED_DAY` (a recorded
/// workload day in `replay.rs`'s format, an owner per tenant or app) when it is set; expect each owner's part
/// of the held share under contention within 2% of an equal part. Without a recording, do the
/// generated day of `four_equal_weight_...` written as a trace and read back; expect it to read as the
/// same cluster and measure the same shares, so a recording runs through the same path unchanged.
#[test]
fn four_equal_weight_owners_over_a_recorded_day() {
    let config = Config {
        workers: 4,
        max_events: 1 << 22,
        ..Config::default()
    };
    if let Ok(path) = std::env::var("HEX_SIM_RECORDED_DAY") {
        let text = std::fs::read_to_string(&path).unwrap();
        let day = replay::parse(&text, &replay::Limits::default()).unwrap();
        let run = sim::run(&day, &config).unwrap();
        let shares = fair::measure(&day, &run.ran, &run.refused, run.report.end).unwrap();
        let equal = 1_000_000 / shares.shares_ppm.len() as u64;
        eprintln!(
            "AC-T7.5.2 recorded {path}: workloads {} owners {} contended {:.2} h shares_ppm {:?} worst {} ppm (bar {})",
            day.workloads.len(),
            day.owners.len(),
            shares.contended_ns as f64 / HOUR as f64,
            shares.shares_ppm,
            shares.worst_from_equal_ppm(),
            equal / 50
        );
        assert!(shares.contended_ns > 0, "no contention in the recording");
        assert!(shares.worst_from_equal_ppm() <= equal / 50);
        return;
    }
    let day = generate::day(4, &four_tenants()).unwrap();
    let text = as_trace(&day);
    // A generated day is no recorded input, so the census's bounds (twice the committed traces') don't
    // hold it: it is read within its own size, as tests with inputs of their own pass their own limits
    // (`docs/design/hex-sim-constants.md`).
    let own = replay::Limits {
        max_bytes: text.len(),
        max_nodes: day.nodes.len(),
        max_legs: day.workloads.len(),
        max_owners: day.owners.len(),
        max_groups: day.groups.len(),
        max_overheads: 0,
    };
    let read = replay::parse(&text, &own).unwrap();
    assert_same_cluster(&read, &day);
    let run = sim::run(&read, &config).unwrap();
    let shares = fair::measure(&read, &run.ran, &run.refused, run.report.end).unwrap();
    assert!(shares.worst_from_equal_ppm() <= 5_000, "{shares:?}");
}
