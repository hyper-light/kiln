//! What the recorded-day tests share: reading a committed trace, sizing a cluster to a bar's
//! precondition, and running and measuring the policies on it.

#![allow(dead_code)]

use hex_sim::fair::{self, Policy, ShareReport};
use hex_sim::generate;
use hex_sim::model::{Cluster, Owner, OwnerId, Resources, Time, WorkloadId};
use hex_sim::replay;
use hex_sim::sim::{self, Config};

pub const DAY: u128 = 86_400_000_000_000;
pub const GIB: f64 = (1u64 << 30) as f64;

pub fn recorded(name: &str) -> Cluster {
    let text = std::fs::read_to_string(format!("../../bench/sched/{name}")).unwrap();
    replay::parse(&text, &replay::Limits::default()).unwrap()
}

/// Each owner's CPU (millicore-ns) and memory (byte-ns) asked for over the day.
pub fn asked(day: &Cluster) -> Vec<(u128, u128)> {
    let mut out = vec![(0u128, 0u128); day.owners.len()];
    for w in &day.workloads {
        let runs = u128::from(w.runs_for.unwrap());
        let o = &mut out[usize::try_from(w.tenancy.owner.0).unwrap()];
        o.0 += u128::from(w.need.cpu_milli) * runs;
        o.1 += u128::from(w.need.mem_bytes) * runs;
    }
    out
}

/// The largest cluster of one node at which every owner asks, on average over the day, for more
/// than a quarter of it in CPU or in memory: each owner `o` allows CPU up to `cpu_o` (just under four
/// times its mean millicores) or memory up to `mem_o`. Choosing which owners CPU satisfies (those
/// whose `cpu_o` is at least the CPU chosen) leaves the rest to memory, so the candidates are each
/// owner's `cpu_o` as the CPU, with the least `mem_o` of the owners it leaves as the memory (the
/// recorded cluster's memory if it leaves none). The largest is the one whose smaller fraction of
/// the recorded cluster (CPU or memory) is largest.
pub fn quarter_cluster(day: &Cluster) -> Resources {
    let total = day.nodes.iter().fold(Resources::default(), |a, n| {
        a.checked_add(&n.capacity).unwrap()
    });
    let limits: Vec<(u128, u128)> = asked(day)
        .iter()
        .map(|&(c, m)| ((4 * c - 1) / DAY, (4 * m - 1) / DAY))
        .collect();
    let mut best: Option<(u128, u128)> = None;
    for &(cpu, _) in &limits {
        let mem = limits
            .iter()
            .filter(|(c, _)| *c < cpu)
            .map(|&(_, m)| m)
            .min()
            .unwrap_or(u128::from(total.mem_bytes));
        let frac = |(c, m): (u128, u128)| {
            // The smaller of c / total cpu and m / total mem, compared by cross-multiplication.
            (c * u128::from(total.mem_bytes)).min(m * u128::from(total.cpu_milli))
        };
        if best.is_none_or(|b| frac((cpu, mem)) > frac(b)) {
            best = Some((cpu, mem));
        }
    }
    let (cpu, mem) = best.unwrap();
    Resources {
        cpu_milli: u64::try_from(cpu).unwrap(),
        mem_bytes: u64::try_from(mem).unwrap(),
        gpus: 0,
        disk_bytes: 0,
    }
}

pub fn hours(ns: Time) -> f64 {
    ns as f64 / 3.6e12
}

pub fn describe(label: &str, s: &ShareReport) {
    let mut iv = s.intervals.clone();
    iv.sort_unstable();
    eprintln!(
        "{label}: contended {:.3} h in {} intervals (median {:.1} s, longest {:.1} min); parts of the held share ppm {:?}; dominant share of the cluster ppm {:?}; worst from equal {} ppm",
        hours(s.contended_ns),
        iv.len(),
        iv.get(iv.len() / 2).map_or(0.0, |&t| t as f64 / 1e9),
        iv.last().map_or(0.0, |&t| t as f64 / 6e10),
        s.shares_ppm,
        s.of_cluster_ppm,
        s.worst_from_equal_ppm()
    );
}

pub fn run(day: &Cluster, policy: Policy) -> (ShareReport, u64) {
    run_with(day, policy, 0)
}

/// `day` under `policy` with up to `preemptions` workloads preempted: its shares under contention,
/// and the workloads refused at admission.
pub fn run_with(day: &Cluster, policy: Policy, preemptions: u64) -> (ShareReport, u64) {
    run_within(day, policy, preemptions, Time::MAX)
}

/// As [`run_with`], with the shares measured up to `until` (the recorded day's end), or the run's
/// end if that comes first.
pub fn run_within(
    day: &Cluster,
    policy: Policy,
    preemptions: u64,
    until: Time,
) -> (ShareReport, u64) {
    let run = sim::run(
        day,
        &Config {
            policy,
            preemptions,
            max_events: 1 << 22,
            ..Config::default()
        },
    )
    .unwrap();
    let refused = u64::try_from(run.refused.iter().filter(|&&r| r).count()).unwrap();
    (
        fair::measure(day, &run.ran, &run.refused, run.report.end.min(until)).unwrap(),
        refused,
    )
}

/// Each of the first `owners` owners' 95th-percentile leg-start latency (first start less arrival; a
/// leg never started counts as waiting for ever).
pub fn p95s(day: &Cluster, started: &[Option<Time>], owners: u32) -> Vec<Time> {
    (0..owners)
        .map(|o| {
            let mut waits: Vec<Time> = day
                .workloads
                .iter()
                .zip(started)
                .filter(|(w, _)| w.tenancy.owner == OwnerId(o))
                .map(|(w, s)| s.map_or(Time::MAX, |s| s - w.arrives))
                .collect();
            waits.sort_unstable();
            waits[(waits.len() * 95).div_ceil(100) - 1]
        })
        .collect()
}

pub fn starts(day: &Cluster, policy: Policy, preemptions: u64) -> (Vec<Option<Time>>, u64) {
    let run = sim::run(
        day,
        &Config {
            policy,
            preemptions,
            max_events: 1 << 22,
            ..Config::default()
        },
    )
    .unwrap();
    (
        run.ran.iter().map(|r| r.map(|r| r.started)).collect(),
        run.report.preempted,
    )
}

/// T-T7.5.1's burst on a recorded day: `usual` with 1,000 workloads from a new owner (one past the
/// last), all arriving at the start of the day's busiest hour (the hour, counted from its first
/// arrival, in which most arrived), each drawn from the day's own workloads
/// ([`generate::burst_from`], seed 7). Prints what it made under `label`.
pub fn with_burst(usual: &Cluster, label: &str) -> Cluster {
    const HOUR: Time = 3_600_000_000_000;
    let owners = u32::try_from(usual.owners.len()).unwrap();
    let mut per_hour = std::collections::BTreeMap::<Time, u64>::new();
    for w in &usual.workloads {
        *per_hour.entry(w.arrives / HOUR).or_default() += 1;
    }
    let (&busiest, &count) = per_hour
        .iter()
        .max_by_key(|(h, n)| (**n, std::cmp::Reverse(**h)))
        .unwrap();
    let burst =
        generate::burst_from(usual.clone(), 7, OwnerId(owners), busiest * HOUR, 1_000).unwrap();
    eprintln!(
        "{label}: {} workloads of {owners} owners, busiest hour {busiest} ({count} arrivals); with the burst {}",
        usual.workloads.len(),
        burst.workloads.len()
    );
    burst
}

/// Each of `usual`'s owners' start p95 without the burst and with it under one policy, and the
/// workloads preempted with it.
pub fn burst_row(
    usual: &Cluster,
    burst: &Cluster,
    policy: Policy,
    preemptions: u64,
) -> (Vec<Time>, Vec<Time>, u64) {
    let owners = u32::try_from(usual.owners.len()).unwrap();
    let (without, _) = starts(usual, policy, preemptions);
    let (with, preempted) = starts(burst, policy, preemptions);
    (
        p95s(usual, &without, owners),
        p95s(burst, &with, owners),
        preempted,
    )
}

/// T-T7.5.1's runs on a recorded day ([`with_burst`]): for Hex's policy (bands, weighted DRF,
/// preemption), plain DRF and submit order, each owner's start p95 without the burst and with it,
/// and the workloads preempted with it.
pub fn burst_rows(usual: &Cluster, label: &str) -> Vec<(&'static str, Vec<Time>, Vec<Time>, u64)> {
    let burst = with_burst(usual, label);
    [
        ("hex", Policy::Drf, 100_000),
        ("drf", Policy::Drf, 0),
        ("fifo", Policy::Fifo, 0),
    ]
    .into_iter()
    .map(|(name, policy, preemptions)| {
        let (a, b, preempted) = burst_row(usual, &burst, policy, preemptions);
        (name, a, b, preempted)
    })
    .collect()
}

/// Whether every owner's p95 with the burst is at most its p95 without.
pub fn holds(without: &[Time], with: &[Time]) -> bool {
    with.iter().zip(without).all(|(w, wo)| w <= wo)
}

/// `day` with only `owners`' workloads, those owners renumbered 0.. in the order given, each of
/// weight 1, and the workloads' ids made dense again in arrival order.
pub fn only_owners(mut day: Cluster, owners: &[u32]) -> Cluster {
    day.workloads
        .retain(|w| owners.contains(&w.tenancy.owner.0));
    for (id, w) in (0u64..).zip(day.workloads.iter_mut()) {
        w.id = WorkloadId(id);
        let at = owners.iter().position(|&o| o == w.tenancy.owner.0).unwrap();
        w.tenancy.owner = OwnerId(u32::try_from(at).unwrap());
    }
    day.owners = vec![Owner::default(); owners.len()];
    day
}
