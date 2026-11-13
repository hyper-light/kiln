//! A side measurement, not of record: G4's bars run on days of kiln's own CI, replayed in hex-sim
//! as recorded (`bench/sched/kiln_workload.md`). Hex schedules OCI containers, Firecracker microVMs
//! and Shards microVMs, not CI jobs (Ada, 2026-11-06: "You are NOT Kiln's load balancer for jobs, CI,
//! and general work. Your scheduler is for *microvms*."), so these days stand for no workload of
//! Hex's; G4's bars are measured on days of its workloads (`tests/fair.rs`, `tests/burst.rs`,
//! `tests/huawei.rs`). Kept for what they showed about the bars' preconditions.

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

use common::{DAY, GIB, asked, burst_rows, describe, holds, quarter_cluster, recorded, run};
use hex_sim::fair::Policy;
use hex_sim::model::{Classes, NodeId, NodeSpec};

/// Side measurement (kiln CI, not of record): AC-T7.5.2 over a recorded CI day, measured (`bench/sched/kiln_workload.md`): do
/// 2026-10-25 of kiln's CI (kiln, slates, hyperscale and hyper-raft as four equal-weight owners;
/// arrivals, run times and needs as recorded) on the largest cluster at which each of the four asks
/// for more than a quarter of it, under bands-then-DRF and under submit order; expect the
/// precondition to hold, the four to contend, and legs too large for the cluster refused at
/// admission. It does not meet the bar (2026-11-06: DRF worst 231,270 ppm from 25%, FIFO 448,998):
/// kiln asks for more than a quarter only through legs the cluster can't hold, and what it can run
/// asks for under 1% of it.
#[test]
fn four_recorded_repositories_on_the_largest_quarter_cluster() {
    let mut day = recorded("kiln-ci-20261025-four-repos.trace");
    let node = quarter_cluster(&day);
    for (o, &(c, m)) in asked(&day).iter().enumerate() {
        let cpu_day = u128::from(node.cpu_milli) * DAY;
        let mem_day = u128::from(node.mem_bytes) * DAY;
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
        "kiln-CI side: AC-T7.5.2 2026-10-25: {} legs, one node of {} millicores and {:.2} GiB",
        day.workloads.len(),
        node.cpu_milli,
        node.mem_bytes as f64 / GIB
    );
    let (drf, refused) = run(&day, Policy::Drf);
    let (fifo, _) = run(&day, Policy::Fifo);
    eprintln!(
        "kiln-CI side: AC-T7.5.2: {refused} legs larger than the cluster, refused at admission"
    );
    describe("kiln-CI side: AC-T7.5.2 DRF", &drf);
    describe("kiln-CI side: AC-T7.5.2 FIFO", &fifo);
    assert!(
        drf.contended_ns > 0 && fifo.contended_ns > 0,
        "no contention"
    );
    assert!(refused > 0);
}

/// Side measurement (kiln CI, not of record): do 2026-10-30, the day of the fourteen with the most kiln-slates
/// contention, with kiln and slates as two equal-weight owners on the 10 runners kiln ran them on,
/// under bands-then-DRF and under submit order, against 50% within 2%; expect them to contend. They
/// barely do (2026-11-05: 560 us in all under DRF, 420 us under FIFO, the control plane's own commit
/// latency): at the real cluster the two never wait together for long.
#[test]
fn kiln_and_slates_at_the_recorded_runners() {
    let day = recorded("kiln-ci-20261030-kiln-slates.trace");
    let (drf, _) = run(&day, Policy::Drf);
    let (fifo, _) = run(&day, Policy::Fifo);
    describe("kiln-slates 2026-10-30 DRF", &drf);
    describe("kiln-slates 2026-10-30 FIFO", &fifo);
    assert!(
        drf.contended_ns > 0 && fifo.contended_ns > 0,
        "no contention"
    );
}

/// Side measurement (kiln CI, not of record): T-T7.5.1 on a recorded CI day: do 2026-10-30 of kiln's CI as recorded (focal, hyperscale, kiln and
/// slates as owners, on the 10 runners kiln ran it on), then the same day with 1,000 legs from a
/// repository that ran nothing that day, all arriving at the start of the day's busiest hour (the
/// hour, counted from its first leg, in which most legs arrived), each leg's need, run time and kind
/// drawn from the day's own legs; expect submit order (FIFO) to let the burst delay the others, and
/// Hex's policy (bands, weighted DRF, preemption of over-share work) and plain DRF to be measured
/// beside it for the record (`bench/sched/kiln_workload.md`). Measured 2026-11-05: Hex's policy keeps
/// focal's, hyperscale's and kiln's p95 as they were and slates' 20 us longer (380 to 400 us: one
/// commit, as a slates leg arrived while a burst leg's commit was in flight), so it misses "at most"
/// by that; plain DRF delays kiln by 167 s and slates by 87 s; FIFO by 3.5 h and 1.5 h.
#[test]
fn a_burst_on_a_recorded_day_leaves_the_others_start_p95_as_it_was() {
    let usual = recorded("kiln-ci-20261030.trace");
    let rows = burst_rows(&usual, "kiln-CI side: T-T7.5.1 2026-10-30");
    for (name, a, b, preempted) in &rows {
        eprintln!(
            "kiln-CI side: T-T7.5.1 {name}: p95 ns without {a:?} with {b:?}; preempted {preempted}; holds {}",
            holds(a, b)
        );
    }
    assert!(
        !holds(&rows[2].1, &rows[2].2),
        "FIFO kept the bar: the test can't fail"
    );
}
