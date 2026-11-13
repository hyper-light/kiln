//! G4's bars on a recorded day of workloads (CP-T7.5, `docs/design/scheduler-fairness.md`): Huawei
//! Cloud's public serverless trace, region 5, day 30 (Joosen et al., EuroSys 2025; CC BY 4.0;
//! `bench/sched/huawei_workload.md`), each sandbox a workload and each user an owner, replayed in
//! hex-sim as recorded.
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

mod common;

use common::{
    DAY, GIB, asked, burst_row, describe, holds, only_owners, quarter_cluster, recorded, run,
    run_with, with_burst,
};
use hex_sim::fair::Policy;
use hex_sim::model::{Classes, Cluster, NodeId, NodeSpec, Resources, Time};

const TRACE: &str = "huawei-r5-day30.trace";

/// The four owners asking for the largest dominant share of the recorded cluster over the day,
/// largest first.
fn busiest_four() -> Vec<u32> {
    let day = recorded(TRACE);
    let total = day.nodes.iter().fold(Resources::default(), |a, n| {
        a.checked_add(&n.capacity).unwrap()
    });
    let mut by_share: Vec<(u128, u32)> = asked(&day)
        .iter()
        .zip(0u32..)
        .map(|(&(c, m), o)| {
            // Each fraction scaled to the other's denominator, so the two compare as integers.
            let cpu = c * u128::from(total.mem_bytes);
            let mem = m * u128::from(total.cpu_milli);
            (cpu.max(mem), o)
        })
        .collect();
    by_share.sort_unstable_by(|a, b| b.cmp(a));
    by_share.iter().take(4).map(|&(_, o)| o).collect()
}

/// AC-T7.5.2, measured on a recorded day of workloads (`bench/sched/g4_huawei.md`): do day 30 of Huawei
/// Cloud's region 5 with its four busiest owners as four equal-weight owners (their sandboxes'
/// launches, lifetimes and shapes as recorded), on the largest cluster at which each of the four
/// asks, on average over the day, for more than a quarter of it (`common::quarter_cluster`),
/// counting only the workloads that cluster can hold; under Hex's policy (bands,
/// weighted DRF, preemption of over-share work: F1, F4 and F5), plain DRF and submit order; expect
/// the precondition to hold, the four to contend, and workloads larger than the cluster refused. It
/// does not meet the bar (2026-11-06: Hex and plain DRF worst 461,564 ppm from 25%, FIFO 503,860):
/// that cluster is 2.777 cores, and two of the owners run single workloads of 1.8 and 2.6 cores, 65%
/// and 94% of it. An owner holding one such workload is never a victim under F4's same-band rule (it
/// would fall below the preemptor), so the owners of 0.3-core workloads wait for days. DRF's equal
/// shares assume tasks small beside the cluster (NSDI 2011 §4), and the bar's precondition sizes the
/// cluster below this trace's grain.
#[test]
fn four_workload_owners_on_the_largest_quarter_cluster() {
    let four = busiest_four();
    let mut day = only_owners(recorded(TRACE), &four);
    let node = quarter_cluster(&day);
    // The precondition, on work the cluster can run: a workload larger than it is refused.
    let mut runnable = day.clone();
    runnable.workloads.retain(|w| node.holds(&w.need));
    for (o, &(c, m)) in asked(&runnable).iter().enumerate() {
        let (cpu_day, mem_day) = (
            u128::from(node.cpu_milli) * DAY,
            u128::from(node.mem_bytes) * DAY,
        );
        assert!(
            4 * c > cpu_day || 4 * m > mem_day,
            "owner {o} asks for no more than a quarter"
        );
    }
    day.nodes = vec![NodeSpec {
        routes: None,
        costs: [None; hex_sim::model::Class::COUNT],
        priors: Default::default(),
        runtime_pools: Vec::new(),
        id: NodeId(0),
        capacity: node,
        background_milli: 0,
        classes: Classes::ALL,
        platform: Default::default(),
        virt: Default::default(),
        devices: hex_sim::model::unnamed_gpus(node.gpus),
    }];
    eprintln!(
        "AC-T7.5.2 huawei r5 day 30: owners {four:?} (by userID index), {} workloads, one node of {} millicores and {:.2} GiB",
        day.workloads.len(),
        node.cpu_milli,
        node.mem_bytes as f64 / GIB
    );
    let (hex, refused) = run_with(&day, Policy::Drf, 1_000_000);
    let (drf, _) = run(&day, Policy::Drf);
    let (fifo, _) = run(&day, Policy::Fifo);
    eprintln!(
        "AC-T7.5.2 huawei: {refused} workloads larger than the cluster, refused at admission"
    );
    describe("AC-T7.5.2 huawei hex", &hex);
    describe("AC-T7.5.2 huawei plain DRF", &drf);
    describe("AC-T7.5.2 huawei FIFO", &fifo);
    assert!(
        hex.contended_ns > 0 && fifo.contended_ns > 0,
        "no contention"
    );
    assert!(refused > 0);
}

/// T-T7.5.1 on a recorded day of workloads (`bench/sched/g4_huawei.md`): do day 30 of Huawei Cloud's
/// region 5 as recorded (all its owners) on 10 down to 1 of the trace's m5d.metal hosts, each with
/// and without 1,000 workloads from an owner that launched none that day, all at the start of the
/// day's busiest hour, each drawn from the day's own (`common::with_burst`). The host count of record
/// is fixed by a rule set before Hex's numbers are read: the largest count at which submit order
/// (FIFO) lets the burst slow another owner, so the burst is a stress there. Expect, at that count,
/// every owner's start p95 with the burst at most its p95 without under Hex's policy (bands,
/// weighted DRF, preemption of over-share work). Hex's policy, plain DRF and FIFO are measured at
/// every count, for the record's full table.
#[test]
fn a_burst_of_workloads_leaves_the_other_owners_start_p95_as_it_was() {
    let full = recorded(TRACE);
    let report = |name: &str, hosts: usize, (a, b, preempted): &(Vec<Time>, Vec<Time>, u64)| {
        let worse = b.iter().zip(a).filter(|(w, wo)| w > wo).count();
        let worst = b.iter().zip(a).map(|(w, wo)| w.saturating_sub(*wo)).max();
        eprintln!(
            "T-T7.5.1 huawei {hosts} hosts {name}: owners {}, slower with the burst {worse}, most slower by {worst:?} ns; preempted {preempted}; holds {}",
            a.len(),
            holds(a, b)
        );
    };
    let days: Vec<(usize, Cluster, Cluster)> = (1..=10)
        .rev()
        .map(|hosts| {
            let mut usual = full.clone();
            usual.nodes.truncate(hosts);
            let burst = with_burst(&usual, &format!("T-T7.5.1 huawei r5 day 30, {hosts} hosts"));
            (hosts, usual, burst)
        })
        .collect();
    // The rule, applied to FIFO alone before Hex runs at all.
    let fifo: Vec<_> = days
        .iter()
        .map(|(hosts, usual, burst)| {
            let row = burst_row(usual, burst, Policy::Fifo, 0);
            report("fifo", *hosts, &row);
            row
        })
        .collect();
    let chosen = fifo
        .iter()
        .position(|(a, b, _)| !holds(a, b))
        .expect("FIFO keeps the bar at every host count from 1 to 10");
    let (hosts, _, _) = &days[chosen];
    eprintln!("T-T7.5.1 huawei: the rule picks {hosts} hosts");
    let mut hex_at_chosen = None;
    for (at, (hosts, usual, burst)) in days.iter().enumerate() {
        let hex = burst_row(usual, burst, Policy::Drf, 100_000);
        let drf = burst_row(usual, burst, Policy::Drf, 0);
        report("hex", *hosts, &hex);
        report("drf", *hosts, &drf);
        if at == chosen {
            hex_at_chosen = Some(hex);
        }
    }
    let (a, b, _) = hex_at_chosen.unwrap();
    assert!(holds(&a, &b), "Hex's policy lets the burst delay an owner");
}
