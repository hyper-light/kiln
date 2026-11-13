//! T-T7.5.1 (CP-T7.5, `docs/design/scheduler-fairness.md` F5): a burst of workloads from one owner leaves
//! the other owners' start latency as it was, in hex-sim, beside the competitors' policies.
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

use hex_sim::baseline;
use hex_sim::fair::Policy;
use hex_sim::generate::{self, Day, Tenant};
use hex_sim::model::{Cluster, OwnerId, Resources, Time};
use hex_sim::sim::{self, Config};

const GIB: u64 = 1 << 30;
const HOUR: u64 = 3_600_000_000_000;
const MINUTE: u64 = 60_000_000_000;

/// Three owners' usual day on 16 nodes of 16 cores and 64 GiB: each asks for 20% of the
/// cluster in the busy hours and 8% out of them, with workloads of different shapes; a fourth asks for
/// nothing.
fn usual() -> Cluster {
    let tenant = |cores, gib, busy_ppm, quiet_ppm| Tenant {
        weight: 1,
        cores,
        gib,
        busy_ppm,
        quiet_ppm,
    };
    generate::day(
        5,
        &Day {
            nodes: 16,
            node: Resources {
                cpu_milli: 16_000,
                mem_bytes: 64 * GIB,
                gpus: 0,
                disk_bytes: 0,
            },
            tenants: vec![
                tenant((1, 2), (1, 4), 200_000, 80_000),
                tenant((2, 4), (4, 8), 200_000, 80_000),
                tenant((1, 4), (8, 16), 200_000, 80_000),
                tenant((1, 1), (1, 1), 0, 0),
            ],
            length: 24 * HOUR,
            busy: (9 * HOUR, 19 * HOUR),
            mean_run: 10 * MINUTE,
            max_vms: 100_000,
        },
    )
    .unwrap()
}

/// The usual day with 1,000 workloads of the fourth owner's, of 1 to 8 cores, all arriving at noon.
fn with_burst() -> Cluster {
    generate::burst(usual(), 6, OwnerId(3), 12 * HOUR, 1_000, 8, 10 * MINUTE).unwrap()
}

/// Each of the three usual owners' 95th-percentile start latency (first start less arrival;
/// a workload never started counts as waiting to the end of time).
fn p95s(cluster: &Cluster, started: &[Option<Time>]) -> [Time; 3] {
    let mut out = [0; 3];
    for (owner, p) in out.iter_mut().enumerate() {
        let mut waits: Vec<Time> = cluster
            .workloads
            .iter()
            .zip(started)
            .filter(|(w, _)| w.tenancy.owner == OwnerId(u32::try_from(owner).unwrap()))
            .map(|(w, s)| s.map_or(Time::MAX, |s| s - w.arrives))
            .collect();
        waits.sort_unstable();
        *p = waits[(waits.len() * 95).div_ceil(100) - 1];
    }
    out
}

/// The three's p95s under the control plane, and the workloads it preempted.
fn control_plane(cluster: &Cluster, policy: Policy, preemptions: u64) -> ([Time; 3], u64) {
    let run = sim::run(
        cluster,
        &Config {
            policy,
            preemptions,
            max_events: 1 << 22,
            ..Config::default()
        },
    )
    .unwrap();
    let started: Vec<Option<Time>> = run.ran.iter().map(|r| r.map(|r| r.started)).collect();
    (p95s(cluster, &started), run.report.preempted)
}

fn kueue(cluster: &Cluster) -> [Time; 3] {
    let out = baseline::kueue_cohort(cluster, 1 << 26).unwrap();
    p95s(cluster, &out.started)
}

/// T-T7.5.1: do the three owners' usual day, then the same day with 1,000 workloads from a fourth
/// arriving at once; expect, under Hex's policy (bands, weighted DRF, preemption of over-share work),
/// each of the three's start p95 with the burst to be at most its p95 without, with burst workloads
/// preempted to do it; and (non-vacuity) submit order to fail that on the same days. Plain DRF (no preemption) and Kueue's cohort sharing
/// are measured beside it, for the record (F5).
#[test]
fn a_burst_leaves_the_other_owners_start_p95_as_it_was() {
    let (usual, burst) = (usual(), with_burst());
    let us = |c: &Cluster| control_plane(c, Policy::Drf, 100_000);
    let (hex_burst, preempted) = us(&burst);
    let rows = [
        ("hex", us(&usual).0, hex_burst),
        (
            "drf",
            control_plane(&usual, Policy::Drf, 0).0,
            control_plane(&burst, Policy::Drf, 0).0,
        ),
        (
            "fifo",
            control_plane(&usual, Policy::Fifo, 0).0,
            control_plane(&burst, Policy::Fifo, 0).0,
        ),
        ("kueue", kueue(&usual), kueue(&burst)),
    ];
    for (name, without, with) in &rows {
        eprintln!(
            "T-T7.5.1 {name}: workloads {} / {}; p95 ns without {without:?} with {with:?}; holds {}",
            usual.workloads.len(),
            burst.workloads.len(),
            with.iter().zip(without).all(|(a, b)| a <= b)
        );
    }
    let holds = |(_, without, with): &(&str, [Time; 3], [Time; 3])| {
        with.iter().zip(without).all(|(a, b)| a <= b)
    };
    eprintln!("T-T7.5.1 hex preempted {preempted} burst workloads");
    assert!(holds(&rows[0]), "hex: {:?}", rows[0]);
    // Non-vacuity: the burst took room the others then took back.
    assert!(preempted > 0);
    assert!(!holds(&rows[2]), "FIFO kept the bar: the test can't fail");
}
