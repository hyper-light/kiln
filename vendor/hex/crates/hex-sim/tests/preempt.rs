//! G4 step 4 (CP-T7.5, `docs/design/scheduler-fairness.md` F4): preemption that keeps disruption
//! budgets, over generated clusters with the crash nemesis.
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

use hex_sim::generate::{self, Shape};
use hex_sim::model::{
    Class, Classes, Cluster, Group, GroupId, Kind, NodeId, NodeSpec, Owner, Resources, Tenancy,
    WorkloadId, WorkloadSpec,
};
use hex_sim::rng::Rng;
use hex_sim::sim::{self, Config, Crash, Nemesis, Report, Run, When};
use hex_sim::spec::{self, Violation};
use hex_sim::store::{Store, Write};
use proptest::prelude::*;

/// A cluster where preemption has work to do: the first half of its workloads, in band 0, never
/// end and fill the nodes; the second half arrive later in bands 1 and 2 and run for a while. Up to
/// three owners; most workloads in one of up to four disruption groups with budgets of 1 to 3.
fn contested(seed: u64) -> Cluster {
    let mut r = Rng::new(seed ^ 0x9ee3);
    let shape = Shape {
        nodes: u32::try_from(r.range(1, 8)).unwrap(),
        workloads: r.range(8, 90),
        mean_gap: r.range(0, 20_000),
        mean_run: Some(r.range(100_000, 2_000_000)),
        gpu_nodes_per_mille: r.range(0, 300),
        max_share_per_mille: r.range(100, 700),
    };
    let owners = u32::try_from(r.range(1, 3)).unwrap();
    let cluster =
        generate::tenants(generate::cluster(seed, &shape).unwrap(), seed, owners, 1).unwrap();
    let mut cluster = generate::groups(
        cluster,
        seed,
        u32::try_from(r.range(1, 4)).unwrap(),
        3,
        r.range(500, 1000),
    );
    let half = cluster.workloads.len() / 2;
    for (i, w) in cluster.workloads.iter_mut().enumerate() {
        if i < half {
            w.tenancy.band = 0;
            w.runs_for = None;
        } else {
            w.tenancy.band = u8::try_from(r.range(1, 2)).unwrap();
        }
    }
    cluster
}

fn preempting(nemesis: Nemesis) -> Config {
    Config {
        workers: 3,
        preemptions: 10_000,
        nemesis,
        ..Config::default()
    }
}

/// Every check the step holds a run to: its log one the specification makes (no budget broken, no
/// preemption the rules don't make, no higher band left waiting while it could preempt), and
/// nothing lost or started twice.
fn holds_up(cluster: &Cluster, run: &Run) -> Result<(), String> {
    spec::check_log(&cluster.nodes, run).map_err(|v| v.to_string())?;
    let end = spec::check_end(cluster, run);
    if end.lost_or_double() != 0 {
        return Err(format!("{end:?}"));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// AC-T7.5.3: do generated clusters with tight budgets and high-band arrivals, with the crash
    /// nemesis striking at random writes; expect disruption budgets broken by preemption = 0 and no
    /// priority inversion (both refused by the specification's replay), and nothing lost or started
    /// twice.
    #[test]
    fn preemption_never_breaks_a_budget(seed in any::<u64>(), crash in any::<u64>()) {
        let cluster = contested(seed);
        let nemesis = Nemesis::Random { seed: crash, first: crash % 40, per_mille: 30, max_crashes: 3 };
        let run = sim::run(&cluster, &preempting(nemesis)).unwrap();
        prop_assert_eq!(holds_up(&cluster, &run), Ok(()));
    }
}

/// AC-T7.5.3's non-vacuity: over 300 such clusters, workloads were preempted, and candidates were
/// passed over because their group was at its budget, so the budgets bound.
#[test]
fn preemptions_happen_and_budgets_bind() {
    let mut total = Report::default();
    for seed in 0..300 {
        let cluster = contested(seed);
        let run = sim::run(&cluster, &preempting(Nemesis::None)).unwrap();
        holds_up(&cluster, &run).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        total.preempted += run.report.preempted;
        total.preemptions += run.report.preemptions;
        total.budget_passes += run.report.budget_passes;
        total.preemptions_refused += run.report.preemptions_refused;
    }
    eprintln!(
        "AC-T7.5.3: preempted {} in {} preemptions, budget passes {}, refused {}",
        total.preempted, total.preemptions, total.budget_passes, total.preemptions_refused
    );
    assert!(total.preemptions > 100, "{total:?}");
    assert!(total.budget_passes > 100, "{total:?}");
    assert_eq!(total.preemptions_refused, 0);
}

/// AC-T7.3.3's nemesis with preemption on: do one contested cluster once per store write, crashing
/// before that write and again after it; expect each run's log to satisfy the specification and
/// nothing lost or started twice, and (non-vacuity) the sweep to cross preemption writes.
#[test]
fn a_crash_at_every_write_with_preemption_loses_and_doubles_nothing() {
    let cluster = (0..)
        .map(contested)
        .find(|c| {
            let run = sim::run(c, &preempting(Nemesis::None)).unwrap();
            run.report.preemptions >= 3 && c.workloads.len() >= 30
        })
        .unwrap();
    let clean = sim::run(&cluster, &preempting(Nemesis::None)).unwrap();
    for write in 0..clean.report.writes {
        for when in [When::Before, When::After] {
            let run = sim::run(&cluster, &preempting(Nemesis::At(Crash { write, when }))).unwrap();
            assert_eq!(run.report.crashes, 1, "write {write} {when:?}");
            holds_up(&cluster, &run).unwrap_or_else(|e| panic!("write {write} {when:?}: {e}"));
        }
    }
}

fn node(cpu: u64) -> NodeSpec {
    NodeSpec {
        routes: None,
        costs: [None; hex_sim::model::Class::COUNT],
        priors: Default::default(),
        runtime_pools: Vec::new(),
        id: NodeId(0),
        capacity: Resources {
            cpu_milli: cpu,
            mem_bytes: 100,
            gpus: 0,
            disk_bytes: 0,
        },
        background_milli: 0,
        classes: Classes::ALL,
        platform: Default::default(),
        virt: Default::default(),
        devices: Vec::new(),
    }
}

fn work(id: u64, cpu: u64, band: u8, group: Option<u32>) -> WorkloadSpec {
    WorkloadSpec {
        id: WorkloadId(id),
        need: Resources {
            cpu_milli: cpu,
            mem_bytes: 1,
            gpus: 0,
            disk_bytes: 0,
        },
        kind: Kind::Plain,
        class: Class::Oci,
        arrives: 0,
        runs_for: None,
        tenancy: Tenancy {
            band,
            group: group.map(GroupId),
            ..Tenancy::default()
        },
        ask: Default::default(),
    }
}

fn run_of(store: Store, n: usize, groups: Vec<Group>) -> Run {
    Run {
        report: Report::default(),
        store,
        starts: vec![1; n],
        acked: vec![true; n],
        refused: vec![false; n],
        ran: vec![None; n],
        pick: Default::default(),
        policy: Default::default(),
        owners: vec![Owner::default()],
        gangs: Default::default(),
        preemption: true,
        groups,
        planes: Vec::new(),
    }
}

/// The mutant AC-T7.5.3 guards against, a preemption that ignores budgets: two band-0 workloads of
/// group 0 (budget 1) fill the node; a band-1 workload takes both off it in one write. The store,
/// told a budget of 2, lets it; the specification, holding the group's real budget, refuses it.
#[test]
fn the_oracle_refuses_a_preemption_past_a_budget() {
    let nodes = [node(1000)];
    let mut s = Store::new(&nodes, 4, 16).unwrap();
    for id in 0..2 {
        s.apply(0, Write::Submit(work(id, 500, 0, Some(0))))
            .unwrap();
        s.apply(
            0,
            Write::Place {
                workload: WorkloadId(id),
                node: NodeId(0),
                version: 1,
            },
        )
        .unwrap();
    }
    s.apply(0, Write::Submit(work(2, 1000, 1, None))).unwrap();
    let victims = [(WorkloadId(0), NodeId(0), 2), (WorkloadId(1), NodeId(0), 2)];
    s.apply_preempt(
        1,
        &victims,
        (WorkloadId(2), NodeId(0), 1),
        &[Group { budget: 2 }],
    )
    .unwrap();
    let run = run_of(s, 3, vec![Group { budget: 1 }]);
    assert_eq!(
        spec::check_log(&nodes, &run),
        Err(Violation::BudgetBroken(GroupId(0)))
    );
}

/// No priority inversion: a band-1 workload left waiting behind a band-0 one it could preempt within
/// its group's budget is refused by the specification; with the budget spent, the same wait is right.
#[test]
fn the_oracle_refuses_a_higher_band_left_waiting() {
    let nodes = [node(1000)];
    let log = |budget| {
        let mut s = Store::new(&nodes, 4, 16).unwrap();
        s.apply(0, Write::Submit(work(0, 1000, 0, Some(0))))
            .unwrap();
        s.apply(
            0,
            Write::Place {
                workload: WorkloadId(0),
                node: NodeId(0),
                version: 1,
            },
        )
        .unwrap();
        s.apply(0, Write::Submit(work(1, 1000, 1, None))).unwrap();
        run_of(s, 2, vec![Group { budget }])
    };
    assert_eq!(
        spec::check_log(&nodes, &log(1)),
        Err(Violation::Inversion(WorkloadId(1)))
    );
    assert!(spec::check_log(&nodes, &log(0)).is_ok());
}

/// Found by `preemption_never_breaks_a_budget` (seeds 8430366258476278859 and 9529349841844759332,
/// nemesis seeds 7163849987678827747 and 7893282436069096247): a preempted workload placed again
/// (in the first, write 148, workload 12) gives its group back a disruption, so work ahead of it in
/// its own queue (workload 7) may preempt now; the committer set that queue's cursor past the
/// placement anyway and placed a later workload (17) next. Do those runs; expect each log to be one
/// the specification makes.
#[test]
fn a_disruption_ended_by_a_placement_reopens_its_queue() {
    for (seed, crash) in [
        (8430366258476278859u64, 7163849987678827747u64),
        (9529349841844759332, 7893282436069096247),
    ] {
        let cluster = contested(seed);
        let nemesis = Nemesis::Random {
            seed: crash,
            first: crash % 40,
            per_mille: 30,
            max_crashes: 3,
        };
        let run = sim::run(&cluster, &preempting(nemesis)).unwrap();
        assert_eq!(holds_up(&cluster, &run), Ok(()), "seed {seed}");
    }
}

/// Found by `preemption_never_breaks_a_budget` (seed 2869266174429984972, nemesis seed
/// 1647584078049989115): a node reported workload 32's run done after the store had preempted it,
/// before the node saw the order to stop it; the plane's completion write met a record back in
/// pending and stopped the run with `UnexpectedConflict`. Do that run; expect it to finish and its
/// log to be one the specification makes, nothing lost or started twice.
#[test]
fn a_run_reported_done_as_it_is_preempted_is_dropped() {
    let crash = 1647584078049989115u64;
    let cluster = contested(2869266174429984972);
    let nemesis = Nemesis::Random {
        seed: crash,
        first: crash % 40,
        per_mille: 30,
        max_crashes: 3,
    };
    let run = sim::run(&cluster, &preempting(nemesis)).unwrap();
    assert_eq!(holds_up(&cluster, &run), Ok(()));
}

/// AC-T7.5.3: seed 17157580157969519194, crash seed 12065407125461531292. Placing 61
/// grows owner 1's share and makes earlier work 58 able to preempt [23, 13, 8]. The retained
/// own-queue cursor skipped it and placed 69. Keep this exact generated case independent of
/// proptest's random seed and shrinker state; the unchanged serial oracle must accept its log.
#[test]
fn ac_t7_5_3_generated_preemption_seed_17157580157969519194_keeps_policy_order() {
    let seed = 17157580157969519194u64;
    let crash = 12065407125461531292u64;
    let cluster = contested(seed);
    let nemesis = Nemesis::Random {
        seed: crash,
        first: crash % 40,
        per_mille: 30,
        max_crashes: 3,
    };
    let run = sim::run(&cluster, &preempting(nemesis)).unwrap();
    assert_eq!(holds_up(&cluster, &run), Ok(()));
}
