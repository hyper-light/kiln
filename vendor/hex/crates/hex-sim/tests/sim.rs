//! hex-sim's done condition (implementation goal 0, `docs/design/hex-sim.md`): determinism at 10,000
//! nodes, a crash at every control-plane write, and the model-based equivalence of `T-T7.4.1`.

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

use hex_sim::SimError;
use hex_sim::fair::Policy;
use hex_sim::generate::{self, Shape};
use hex_sim::index::{Pick, Scorer, Shapes};
use hex_sim::model::{
    Class, Classes, Cluster, GangId, Kind, NodeId, NodeSpec, Owner, OwnerId, Resources, Tenancy,
    Time, WorkloadId, WorkloadSpec,
};
use hex_sim::packing::{self, Clock};
use hex_sim::rng::Rng;
use hex_sim::sim::{self, Config, Crash, Nemesis, Report, Run, When};
use hex_sim::spec::{self, Violation};
use hex_sim::store::{State, Store, Write};

fn small(seed: u64) -> Shape {
    let mut r = Rng::new(seed ^ 0x5eed);
    Shape {
        nodes: u32::try_from(r.range(1, 24)).unwrap(),
        workloads: r.range(1, 160),
        mean_gap: r.range(0, 40_000),
        mean_run: if r.chance(150) {
            None
        } else {
            Some(r.range(1, 2_000_000))
        },
        gpu_nodes_per_mille: r.range(0, 500),
        max_share_per_mille: r.range(50, 1000),
    }
}

fn config(seed: u64) -> Config {
    let mut r = Rng::new(seed ^ 0xc0f1);
    Config {
        workers: u32::try_from(r.range(1, 8)).unwrap(),
        decide_ns: r.range(0, 200_000),
        commit_ns: r.range(0, 100_000),
        change_window: usize::try_from(r.range(1, 64)).unwrap(),
        pick: pick(&mut r),
        ..Config::default()
    }
}

/// A pick's scorer and shapes, drawn: the kernel's walk reads every node with the room, so there
/// is no sample size to draw.
fn pick(r: &mut Rng) -> Pick {
    Pick {
        scorer: [Scorer::BestFit, Scorer::Align, Scorer::HEX]
            [usize::try_from(r.range(0, 2)).unwrap()],
        shapes: Shapes::of(vec![
            (
                Resources {
                    cpu_milli: r.range(1, 4000),
                    mem_bytes: r.range(1, 1 << 33),
                    gpus: 0,
                    disk_bytes: 0,
                },
                Class::Oci,
            ),
            (
                Resources {
                    cpu_milli: r.range(1, 16000),
                    mem_bytes: r.range(1, 1 << 35),
                    gpus: 0,
                    disk_bytes: 0,
                },
                Class::Oci,
            ),
        ])
        .unwrap(),
    }
}

/// T-T7.4.1: do 1,000 generated clusters and workloads, each run by 1 to 8 parallel deciders with
/// late-binding commit; expect every run's committed log to be one the serial specification makes,
/// nothing lost or started twice, and (non-vacuity) late binding to have redone decisions in some runs.
#[test]
fn placements_equal_the_serial_specification_on_generated_clusters() {
    let (mut conflicts, mut stale, mut placements, mut waited) = (0u64, 0u64, 0u64, 0u64);
    for seed in 0..1_000 {
        let cluster = generate::cluster(seed, &small(seed)).unwrap();
        let run = sim::run(&cluster, &config(seed)).unwrap();
        let log =
            spec::check_log(&cluster.nodes, &run).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
        let end = spec::check_end(&cluster, &run);
        assert_eq!(end.lost_or_double(), 0, "seed {seed}: {end:?}");
        let filters = spec::check_filters(&cluster, &run).unwrap();
        assert_eq!(filters.violations(), 0, "seed {seed}: {filters:?}");
        conflicts += run.report.conflicts;
        stale += run.report.stale;
        placements += log.placements;
        waited += log.waiting;
    }
    assert!(
        conflicts > 0,
        "no decision was ever made stale: the late binding went untested"
    );
    assert!(stale > 0, "no decision ever outlived the change window");
    assert!(placements > 0, "no run placed anything");
    assert!(waited > 0, "no run left work waiting for room");
}

/// T-T7.4.1 under G4's order (AC-T7.5.2): do 500 generated clusters with their workloads spread over 1
/// to 5 owners of weights 1 to 4 and 1 to 3 bands; expect every run's log to be one the serial
/// specification (bands, then weighted DRF, then submit order) makes and nothing lost or started
/// twice, and (non-vacuity) the order to have placed differently from submit order in some runs.
#[test]
fn placements_equal_the_serial_specification_with_owners_and_bands() {
    let (mut reordered, mut placements) = (0u64, 0u64);
    for seed in 0..500 {
        let mut r = Rng::new(seed ^ 0x6a4e);
        let owners = u32::try_from(r.range(1, 5)).unwrap();
        let bands = u8::try_from(r.range(1, 3)).unwrap();
        let cluster = generate::tenants(
            generate::cluster(seed, &small(seed)).unwrap(),
            seed,
            owners,
            bands,
        )
        .unwrap();
        let config = config(seed);
        let run = sim::run(&cluster, &config).unwrap();
        let log =
            spec::check_log(&cluster.nodes, &run).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
        let end = spec::check_end(&cluster, &run);
        assert_eq!(end.lost_or_double(), 0, "seed {seed}: {end:?}");
        placements += log.placements;
        let fifo = sim::run(
            &cluster,
            &Config {
                policy: Policy::Fifo,
                ..config
            },
        )
        .unwrap();
        spec::check_log(&cluster.nodes, &fifo).unwrap_or_else(|v| panic!("seed {seed} fifo: {v}"));
        reordered += u64::from(run.store.log() != fifo.store.log());
    }
    assert!(placements > 20_000, "{placements} placements");
    assert!(
        reordered > 50,
        "the order changed only {reordered} of 500 runs"
    );
}

/// AC-T7.5.1 (gangs of workloads admitted whole), G4 step 3: do 500 generated clusters over 1 to 5 owners
/// and 1 to 3 bands with a quarter of their workloads in gangs of 2 to 8 (a gang's whole need often
/// more than any one node's room); expect every run's log to be one the specification makes, every
/// gang placed in one write (partial gangs = 0), nothing lost or started twice, and (non-vacuity)
/// gangs placed.
#[test]
fn gangs_are_placed_whole_on_generated_clusters() {
    let (mut placed, mut members) = (0u64, 0u64);
    for seed in 0..500 {
        let mut r = Rng::new(seed ^ 0x9a46);
        let owners = u32::try_from(r.range(1, 5)).unwrap();
        let bands = u8::try_from(r.range(1, 3)).unwrap();
        let cluster = generate::gangs(
            generate::tenants(
                generate::cluster(seed, &small(seed)).unwrap(),
                seed,
                owners,
                bands,
            )
            .unwrap(),
            seed,
            120,
            8,
        );
        let run = sim::run(&cluster, &config(seed)).unwrap();
        spec::check_log(&cluster.nodes, &run).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
        let end = spec::check_end(&cluster, &run);
        assert_eq!(end.lost_or_double(), 0, "seed {seed}: {end:?}");
        let gangs = spec::check_gangs(&run);
        assert_eq!(gangs.partial, 0, "seed {seed}: {gangs:?}");
        assert_eq!(gangs.placed, run.report.gangs, "seed {seed}");
        placed += gangs.placed;
        members += run.gangs.values().map(|m| m.len() as u64).sum::<u64>();
    }
    assert!(placed > 500, "only {placed} gangs placed");
    assert!(members > 5_000, "only {members} gang members");
}

/// The oracle catches a gang placed member by member (the mutant AC-T7.5.1 guards against): two
/// members in two writes is a partial gang, refused, and the write that left it part placed counted.
#[test]
fn the_oracle_refuses_a_gang_placed_member_by_member() {
    let nodes = two_nodes();
    let mut s = Store::new(&nodes, 4, 16).unwrap();
    let g = |id| WorkloadSpec {
        tenancy: Tenancy {
            gang: Some(GangId(0)),
            ..Tenancy::default()
        },
        ..work(id, 500)
    };
    s.apply(0, Write::Submit(g(0))).unwrap();
    s.apply(0, Write::Submit(g(1))).unwrap();
    for id in 0..2 {
        s.apply(
            1,
            Write::Place {
                workload: WorkloadId(id),
                node: NodeId(0),
                version: 1,
            },
        )
        .unwrap();
    }
    let mut run = run_of(s, 2);
    run.gangs = [(GangId(0), vec![WorkloadId(0), WorkloadId(1)])].into();
    assert_eq!(
        spec::check_log(&nodes, &run),
        Err(Violation::PartialGang(WorkloadId(0)))
    );
    assert_eq!(spec::check_gangs(&run).partial, 1);
}

/// The oracle refuses a placement that jumped a queue the policy puts first (G4, AC-T7.5.2): two
/// owners, owner 0 already holding half the cluster (node 0's memory); owner 1's workload, submitted
/// later, must go before owner 0's second. Under submit order the same log is correct: the second
/// takes node 1, and owner 1's then holds nowhere.
#[test]
fn the_oracle_refuses_a_placement_that_jumps_a_lower_share() {
    let nodes = two_nodes();
    let mut s = Store::new(&nodes, 4, 16).unwrap();
    let w = |id, owner| WorkloadSpec {
        tenancy: Tenancy {
            owner: OwnerId(owner),
            ..Tenancy::default()
        },
        ..work(id, 500)
    };
    s.apply(0, Write::Submit(w(0, 0))).unwrap();
    s.apply(
        1,
        Write::Place {
            workload: WorkloadId(0),
            node: NodeId(0),
            version: 1,
        },
    )
    .unwrap();
    s.apply(2, Write::Submit(w(1, 0))).unwrap();
    s.apply(3, Write::Submit(w(2, 1))).unwrap();
    s.apply(
        4,
        Write::Place {
            workload: WorkloadId(1),
            node: NodeId(1),
            version: 1,
        },
    )
    .unwrap();
    let mut run = run_of(s, 3);
    run.owners = vec![Owner::default(); 2];
    assert_eq!(
        spec::check_log(&nodes, &run),
        Err(Violation::JumpedQueue {
            placed: WorkloadId(1),
            ahead: WorkloadId(2),
        })
    );
    run.policy = Policy::Fifo;
    assert!(spec::check_log(&nodes, &run).is_ok());
}

/// T-T7.4.1, directly: do a cluster whose workloads never end; expect each workload on exactly the
/// node the specification run serially puts it on.
#[test]
fn with_capacity_only_shrinking_placements_equal_a_serial_run() {
    for seed in 0..200 {
        let shape = Shape {
            mean_run: None,
            ..small(seed)
        };
        let cluster = generate::cluster(seed, &shape).unwrap();
        let config = config(seed);
        let run = sim::run(&cluster, &config).unwrap();
        let serial = spec::serial_place(&cluster.nodes, &cluster.workloads, &config.pick).unwrap();
        for (w, want) in cluster.workloads.iter().zip(serial) {
            let got = match run.store.record(w.id).unwrap().state {
                State::Placed(n) => Some(n),
                State::Pending => None,
                State::Complete(_) => panic!("seed {seed}: a workload that never ends completed"),
            };
            assert_eq!(got, want, "seed {seed}, {:?}", w.id);
        }
    }
}

fn crash_cluster() -> Cluster {
    let shape = Shape {
        nodes: 6,
        workloads: 40,
        mean_gap: 20_000,
        mean_run: Some(400_000),
        gpu_nodes_per_mille: 300,
        max_share_per_mille: 700,
    };
    generate::cluster(11, &shape).unwrap()
}

/// AC-T7.3.3's nemesis, at every write: do the same run once per control-plane store write, crashing
/// the control plane before that write and again after it; expect each run's log to satisfy the
/// specification and nothing lost or started twice, and (non-vacuity) exactly one crash per run and
/// crashes injected before writes == writes seen and crashes injected after writes == writes seen.
/// G4 (AC-T7.5.2): the same with the workloads spread over three owners and two bands.
#[test]
fn a_crash_at_every_control_plane_write_loses_and_doubles_nothing() {
    crash_sweep(&crash_cluster());
    crash_sweep(&generate::tenants(crash_cluster(), 11, 3, 2).unwrap());
    crash_sweep(&generate::gangs(
        generate::tenants(crash_cluster(), 11, 3, 2).unwrap(),
        11,
        250,
        4,
    ));
}

fn crash_sweep(cluster: &Cluster) {
    let cluster = cluster.clone();
    let base = Config {
        workers: 3,
        ..Config::default()
    };
    let clean = sim::run(&cluster, &base).unwrap();
    assert_eq!(clean.report.crashes, 0);
    // Non-vacuity: where the cluster has gangs, the sweep crashes around gang writes.
    let has_gangs = cluster.workloads.iter().any(|w| w.tenancy.gang.is_some());
    assert!(!has_gangs || clean.report.gangs > 0, "no gang placed");
    let writes = clean.report.writes;
    // A submit and a completion each, and a placement write per single or per gang.
    assert!(
        writes >= 2 * u64::try_from(cluster.workloads.len()).unwrap() + clean.report.gangs,
        "{writes} writes"
    );
    let clean_end = spec::check_end(&cluster, &clean);
    let (mut before, mut after, mut changed_history) = (0u64, 0u64, 0u64);
    for write in 0..writes {
        for when in [When::Before, When::After] {
            let run = sim::run(
                &cluster,
                &Config {
                    nemesis: Nemesis::At(Crash { write, when }),
                    ..base
                },
            )
            .unwrap();
            assert_eq!(run.report.crashes, 1, "write {write} {when:?}");
            assert_eq!(run.report.crashes_before + run.report.crashes_after, 1);
            before += run.report.crashes_before;
            after += run.report.crashes_after;
            spec::check_log(&cluster.nodes, &run)
                .unwrap_or_else(|v| panic!("write {write} {when:?}: {v}"));
            let end = spec::check_end(&cluster, &run);
            assert_eq!(end.lost_or_double(), 0, "write {write} {when:?}: {end:?}");
            // Every workload completes but those admission refuses (a gang the empty cluster
            // can't hold whole), the same ones as without the crash.
            assert_eq!(
                end.refused, clean_end.refused,
                "write {write} {when:?}: {end:?}"
            );
            assert_eq!(
                end.complete + end.refused,
                u64::try_from(cluster.workloads.len()).unwrap(),
                "write {write} {when:?}: {end:?}"
            );
            let gangs = spec::check_gangs(&run);
            assert_eq!(gangs.partial, 0, "write {write} {when:?}: {gangs:?}");
            let kept = spec::check_kept_running(&cluster, &run);
            assert_eq!(kept.disturbed, 0, "write {write} {when:?}: {kept:?}");
            if run.store.log() != clean.store.log() {
                changed_history += 1;
            }
        }
    }
    assert_eq!(before, writes, "crashes before writes");
    assert_eq!(after, writes, "crashes after writes");
    assert!(
        changed_history > writes,
        "crashes changed only {changed_history} of {} histories",
        2 * writes
    );
}

fn same(a: &Report, b: &Report) -> bool {
    (
        a.trace_hash,
        a.writes,
        a.placements,
        a.conflicts,
        a.end,
        &a.latencies,
    ) == (
        b.trace_hash,
        b.writes,
        b.placements,
        b.conflicts,
        b.end,
        &b.latencies,
    )
}

/// Implementation goal 0 (a): do 10,000 fake nodes and 100,000 workloads twice from one seed; expect
/// the same trace hash and report, a different seed a different one, and the run correct by the
/// specification.
#[test]
fn ten_thousand_nodes_run_deterministically() {
    let shape = Shape {
        nodes: 10_000,
        workloads: 100_000,
        mean_gap: 20_000,
        mean_run: Some(100_000_000),
        gpu_nodes_per_mille: 100,
        max_share_per_mille: 250,
    };
    let config = Config {
        workers: 16,
        ..Config::default()
    };
    let cluster = generate::cluster(42, &shape).unwrap();
    let a = sim::run(&cluster, &config).unwrap();
    let b = sim::run(&cluster, &config).unwrap();
    assert!(same(&a.report, &b.report));
    assert_eq!(a.store.log(), b.store.log());
    let other = sim::run(&generate::cluster(43, &shape).unwrap(), &config).unwrap();
    assert_ne!(a.report.trace_hash, other.report.trace_hash);
    let end = spec::check_end(&cluster, &a);
    assert_eq!(end.lost_or_double(), 0, "{end:?}");
    assert_eq!(end.complete, 100_000);
    spec::check_log(&cluster.nodes, &a).unwrap();
    assert_eq!(spec::check_filters(&cluster, &a).unwrap().violations(), 0);
    let q = a.report.latencies.quantiles().unwrap();
    assert_eq!(q.count, 100_000);
    assert!(q.p50 <= q.p99 && q.p99 <= q.p999 && q.p999 <= q.max);
}

// The oracle's own non-vacuity: hand it histories the specification can't make and expect each refused.

fn res(cpu: u64) -> Resources {
    Resources {
        cpu_milli: cpu,
        mem_bytes: 1,
        gpus: 0,
        disk_bytes: 0,
    }
}

fn two_nodes() -> Vec<NodeSpec> {
    vec![
        NodeSpec {
            routes: None,
            costs: [None; hex_sim::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(0),
            capacity: res(1000),
            background_milli: 0,
            classes: Classes::ALL,
            platform: Default::default(),
            virt: Default::default(),
            devices: Vec::new(),
        },
        NodeSpec {
            routes: None,
            costs: [None; hex_sim::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(1),
            capacity: res(4000),
            background_milli: 0,
            classes: Classes::ALL,
            platform: Default::default(),
            virt: Default::default(),
            devices: Vec::new(),
        },
    ]
}

fn work(id: u64, cpu: u64) -> WorkloadSpec {
    WorkloadSpec {
        id: WorkloadId(id),
        need: res(cpu),
        kind: Kind::Plain,
        arrives: 0,
        runs_for: None,
        tenancy: Tenancy::default(),
        class: Class::Oci,
        ask: Default::default(),
    }
}

fn run_of(store: Store, n: usize) -> Run {
    Run {
        report: Report::default(),
        store,
        starts: vec![1; n],
        acked: vec![true; n],
        refused: vec![false; n],
        ran: vec![None; n],
        pick: Pick::default(),
        policy: Policy::default(),
        owners: vec![Owner::default()],
        gangs: Default::default(),
        preemption: false,
        groups: Vec::new(),
        planes: Vec::new(),
    }
}

#[test]
fn the_oracle_refuses_a_placement_that_is_not_the_best_fit() {
    let mut s = Store::new(&two_nodes(), 4, 16).unwrap();
    s.apply(0, Write::Submit(work(0, 500))).unwrap();
    s.apply(
        1,
        Write::Place {
            workload: WorkloadId(0),
            node: NodeId(1),
            version: 1,
        },
    )
    .unwrap();
    let v = spec::check_log(&two_nodes(), &run_of(s, 1)).unwrap_err();
    assert_eq!(
        v,
        Violation::NotSerialChoice {
            workload: WorkloadId(0),
            chose: NodeId(1),
            spec: Some(NodeId(0))
        }
    );
}

#[test]
fn the_oracle_refuses_a_placement_that_jumps_the_queue() {
    let mut s = Store::new(&two_nodes(), 4, 16).unwrap();
    s.apply(0, Write::Submit(work(0, 3000))).unwrap();
    s.apply(0, Write::Submit(work(1, 500))).unwrap();
    s.apply(
        1,
        Write::Place {
            workload: WorkloadId(1),
            node: NodeId(0),
            version: 1,
        },
    )
    .unwrap();
    let v = spec::check_log(&two_nodes(), &run_of(s, 2)).unwrap_err();
    assert_eq!(
        v,
        Violation::JumpedQueue {
            placed: WorkloadId(1),
            ahead: WorkloadId(0)
        }
    );
}

#[test]
fn the_oracle_refuses_work_left_pending_with_room() {
    let mut s = Store::new(&two_nodes(), 4, 16).unwrap();
    s.apply(0, Write::Submit(work(0, 500))).unwrap();
    let v = spec::check_log(&two_nodes(), &run_of(s, 1)).unwrap_err();
    assert_eq!(v, Violation::LeftPending(WorkloadId(0)));
}

/// AC-T7.3.3's counter isn't vacuous: a workload started twice and one acknowledged but never
/// stored are each counted.
#[test]
fn the_end_check_counts_doubles_and_losses() {
    let nodes = two_nodes();
    let cluster = Cluster::single_owner(nodes.clone(), vec![work(0, 500), work(1, 500)]);
    let mut s = Store::new(&nodes, 4, 16).unwrap();
    s.apply(0, Write::Submit(work(0, 500))).unwrap();
    s.apply(
        1,
        Write::Place {
            workload: WorkloadId(0),
            node: NodeId(0),
            version: 1,
        },
    )
    .unwrap();
    let mut run = run_of(s, 2);
    run.starts = vec![2, 0];
    let end = spec::check_end(&cluster, &run);
    assert_eq!((end.started_twice, end.lost), (1, 1));
}

/// CLAUDE.md §2: every bound refuses, typed.
#[test]
fn bounds_refuse_typed() {
    let cluster = crash_cluster();
    assert_eq!(
        sim::run(
            &cluster,
            &Config {
                workers: 0,
                ..Config::default()
            }
        )
        .unwrap_err(),
        SimError::Config("the control plane has at least one decider")
    );
    assert_eq!(
        sim::run(
            &cluster,
            &Config {
                max_steps: 10,
                ..Config::default()
            }
        )
        .unwrap_err(),
        SimError::StepBudget(10)
    );
    assert!(matches!(
        sim::run(
            &cluster,
            &Config {
                max_events: 1,
                ..Config::default()
            }
        )
        .unwrap_err(),
        SimError::Full {
            what: "the event queue",
            ..
        }
    ));
    let restart_forever = Config {
        nemesis: Nemesis::At(Crash {
            write: 0,
            when: When::Before,
        }),
        restart_ns: u64::MAX / 2,
        client_attempts: 2,
        ..Config::default()
    };
    assert!(matches!(
        sim::run(&cluster, &restart_forever).unwrap_err(),
        SimError::ClientGaveUp(WorkloadId(0), 2)
    ));
}

/// One of AC-T7.3.3's crash seeds: a generated cluster and control plane, and a nemesis that
/// crashes the plane at a write the seed picks (one of the submits, so every run has it) and then at
/// a seeded rate, up to a seeded cap.
fn crash_seed(seed: u64) -> (Cluster, Config) {
    let mut r = Rng::new(seed ^ 0xdead_beef);
    let workloads = r.range(20, 60);
    let shape = Shape {
        nodes: u32::try_from(r.range(1, 12)).unwrap(),
        workloads,
        mean_gap: r.range(0, 40_000),
        mean_run: if r.chance(100) {
            None
        } else {
            Some(r.range(1, 4_000_000))
        },
        gpu_nodes_per_mille: r.range(0, 500),
        max_share_per_mille: r.range(100, 1000),
    };
    let cluster = generate::cluster(seed, &shape).unwrap();
    let config = Config {
        workers: u32::try_from(r.range(1, 8)).unwrap(),
        decide_ns: r.range(0, 200_000),
        commit_ns: r.range(0, 100_000),
        change_window: usize::try_from(r.range(1, 64)).unwrap(),
        pick: pick(&mut r),
        restart_ns: r.range(0, 5_000_000),
        nemesis: Nemesis::Random {
            seed: r.next_u64(),
            first: r.range(0, workloads - 1),
            per_mille: r.range(5, 200),
            max_crashes: u32::try_from(r.range(1, 8)).unwrap(),
        },
        ..Config::default()
    };
    (cluster, config)
}

/// AC-T7.3.3: do 10,000 crash seeds, each a generated cluster whose control plane the nemesis
/// crashes one to eight times, before or after its writes; expect every run's log one the serial
/// specification makes, workloads lost or started twice exactly 0, and running work undisturbed;
/// and (non-vacuity) every seed crashed at least once, with work running through crashes.
#[test]
fn ten_thousand_crash_seeds_lose_and_double_nothing() {
    let (mut seeds_crashed, mut through, mut lost_or_double) = (0u64, 0u64, 0u64);
    for seed in 0..10_000 {
        let (cluster, config) = crash_seed(seed);
        let run = sim::run(&cluster, &config).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        spec::check_log(&cluster.nodes, &run).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
        let end = spec::check_end(&cluster, &run);
        lost_or_double += end.lost_or_double();
        assert_eq!(end.lost_or_double(), 0, "seed {seed}: {end:?}");
        let kept = spec::check_kept_running(&cluster, &run);
        assert_eq!(kept.disturbed, 0, "seed {seed}: {kept:?}");
        let filters = spec::check_filters(&cluster, &run).unwrap();
        assert_eq!(filters.violations(), 0, "seed {seed}: {filters:?}");
        seeds_crashed += u64::from(run.report.crashes > 0);
        through += run.report.running_through_crashes;
    }
    assert_eq!(lost_or_double, 0);
    assert_eq!(seeds_crashed, 10_000, "a seed ran without a crash");
    assert!(through > 0, "no workload ran through a crash");
}

/// AC-T7.3.4: do long-running workloads (and ones that never end) while the nemesis crashes and
/// restarts the control plane several times; expect every workload started once, on the node it was
/// placed on, run its whole time there and recorded done there (or still running there), with
/// (non-vacuity) dozens of them running through crashes.
#[test]
fn a_control_plane_restart_leaves_running_workloads_running() {
    for mean_run in [Some(50_000_000), None] {
        let shape = Shape {
            nodes: 8,
            workloads: 60,
            mean_gap: 200_000,
            mean_run,
            gpu_nodes_per_mille: 200,
            max_share_per_mille: 300,
        };
        let cluster = generate::cluster(5, &shape).unwrap();
        let config = Config {
            nemesis: Nemesis::Random {
                seed: 9,
                first: 20,
                per_mille: 100,
                max_crashes: 6,
            },
            ..Config::default()
        };
        let run = sim::run(&cluster, &config).unwrap();
        assert!(
            run.report.crashes >= 2,
            "{mean_run:?}: {} crashes",
            run.report.crashes
        );
        assert!(
            run.report.running_through_crashes >= 20,
            "{mean_run:?}: {} running through crashes",
            run.report.running_through_crashes
        );
        let kept = spec::check_kept_running(&cluster, &run);
        assert_eq!(kept.disturbed, 0, "{mean_run:?}: {kept:?}");
        assert!(kept.kept >= 40, "{mean_run:?}: {kept:?}");
        spec::check_log(&cluster.nodes, &run).unwrap();
        assert_eq!(spec::check_end(&cluster, &run).lost_or_double(), 0);
    }
}

/// Step 1's re-take bound: do a saturated cluster whose workloads come in three demands, each a need
/// and a class (equivalence classes), and complete one by one; expect workloads found holding nowhere to be at most one per
/// class per stretch between completions, the rest of each class passed over undecided, and the log one
/// the serial specification makes.
#[test]
fn a_completion_under_saturation_rechecks_each_class_once() {
    let shape = Shape {
        nodes: 4,
        workloads: 600,
        mean_gap: 100,
        mean_run: Some(2_000_000),
        gpu_nodes_per_mille: 0,
        max_share_per_mille: 600,
    };
    let mut cluster = generate::cluster(21, &shape).unwrap();
    let demands: Vec<_> = cluster
        .workloads
        .iter()
        .take(3)
        .map(|w| (w.need, w.class))
        .collect();
    for (i, w) in cluster.workloads.iter_mut().enumerate() {
        (w.need, w.class) = demands[i % 3];
        w.kind = Kind::Plain;
    }
    let config = Config {
        workers: 4,
        ..Config::default()
    };
    let run = sim::run(&cluster, &config).unwrap();
    spec::check_log(&cluster.nodes, &run).unwrap();
    assert_eq!(spec::check_end(&cluster, &run).lost_or_double(), 0);
    let completions = run
        .store
        .log()
        .iter()
        .filter(|e| matches!(e.op, hex_sim::store::Op::Complete { .. }))
        .count();
    let completions = u64::try_from(completions).unwrap();
    let r = &run.report;
    let classes = u64::try_from(demands.len()).unwrap();
    assert!(
        r.class_checks <= classes * (completions + 1),
        "{} class checks over {completions} completions",
        r.class_checks
    );
    assert!(
        r.class_skips > 10 * r.class_checks,
        "{} skips, {} checks",
        r.class_skips,
        r.class_checks
    );
}

/// G2 step 2 (AC-T7.6.3 and S3's filters), in Hex's three kinds: do generated clusters of Linux,
/// Windows and macOS nodes, each running the kinds its platform can, some with GPUs and many above
/// 90% CPU from their co-tenants, with OCI containers, Firecracker microVMs and Shards microVMs as
/// plain work, builds, benchmarks and agent sessions, and one microVM in fifty asking a GPU; expect,
/// by a replay of each log against the cluster's facts alone, none placed on a node that can't run
/// its kind, an OCI container that needs a GPU only on a GPU node, every microVM that asks one
/// refused at admission (neither Firecracker nor Shards can be given a GPU), a benchmark of any kind
/// never beside a build, and no session on a node above 90% while another had room; and
/// (non-vacuity) each filter refused nodes that had the room, every kind placed as a benchmark and
/// as a session, and GPUs given to OCI containers.
#[test]
fn the_hard_filters_keep_every_placement_in_bounds() {
    let mut total = spec::FilterReport::default();
    let mut refused = hex_sim::index::Refusals::default();
    let (mut asked, mut turned_away) = (0u64, 0u64);
    // By kind (OCI, Firecracker, Shards): benchmarks and sessions placed; OCI containers given GPUs.
    let (mut benchmarks, mut sessions, mut gpus) = ([0u64; 3], [0u64; 3], 0u64);
    for seed in 0..500 {
        let shape = Shape {
            gpu_nodes_per_mille: 300,
            ..small(seed)
        };
        let mut cluster = generate::cluster(seed, &shape).unwrap();
        for w in cluster
            .workloads
            .iter_mut()
            .filter(|w| w.class != Class::Oci)
            .step_by(50)
        {
            w.need.gpus = 1;
            asked += 1;
        }
        let run = sim::run(&cluster, &config(seed)).unwrap();
        spec::check_log(&cluster.nodes, &run).unwrap_or_else(|v| panic!("seed {seed}: {v}"));
        let end = spec::check_end(&cluster, &run);
        assert_eq!(end.lost_or_double(), 0, "seed {seed}");
        turned_away += end.refused;
        let f = spec::check_filters(&cluster, &run).unwrap();
        total.placements += f.placements;
        total.gpu_off_gpu += f.gpu_off_gpu;
        total.benchmark_with_build += f.benchmark_with_build;
        total.hot_sessions += f.hot_sessions;
        total.class_off_node += f.class_off_node;
        refused.gpu += run.report.refused.gpu;
        refused.shares += run.report.refused.shares;
        refused.hot += run.report.refused.hot;
        refused.class += run.report.refused.class;
        for w in cluster.workloads.iter().filter(|w| {
            matches!(
                run.store.record(w.id).map(|r| r.state),
                Some(State::Placed(_) | State::Complete(_))
            )
        }) {
            let at = match w.class {
                Class::Oci => 0,
                Class::Firecracker => 1,
                Class::Shards => 2,
            };
            match w.kind {
                Kind::Benchmark => benchmarks[at] += 1,
                Kind::Session => sessions[at] += 1,
                Kind::Plain | Kind::Build => {}
            }
            gpus += u64::from(w.need.gpus > 0);
        }
    }
    assert_eq!(total.hot_sessions, 0, "AC-T7.6.3: {total:?}");
    assert_eq!(total.violations(), 0, "{total:?}");
    assert_eq!(turned_away, asked, "a microVM asking a GPU wasn't refused");
    assert!(
        refused.gpu > 0 && refused.shares > 0 && refused.hot > 0 && refused.class > 0,
        "a filter never refused: {refused:?}"
    );
    assert!(
        asked > 100 && gpus > 100 && total.placements > 20_000,
        "{asked} asked, {gpus} GPUs, {total:?}"
    );
    assert!(
        benchmarks.iter().chain(&sessions).all(|&n| n > 100),
        "benchmarks {benchmarks:?}, sessions {sessions:?}"
    );
}

/// The recorded workload (bench/sched/huawei_workload.md) on one node model: a day of Huawei Cloud's
/// serverless sandboxes, each an OCI container, a Firecracker microVM or a Shards microVM by its
/// owner.
struct NodeModel {
    name: &'static str,
    nodes: usize,
    /// The day with lifetimes as recorded, and with Huawei's one-minute keep-alive added.
    days: [Cluster; 2],
    /// The speeds the completions of the mix, OCI, Firecracker and Shards sets run at, as recorded
    /// and with the keep-alive: each the slowest whole speed at which Hex and both best fits reach 90%
    /// on every seed, as `sched_packing` finds it, so lifetimes enter as far as the nodes allow
    /// (bench/sched/packing.md).
    speeds: [[Time; 2]; 4],
}

/// The two node models: 10 EC2 m5d.metal hosts that each run every kind (the main one), and Hex's
/// fleet's host types five times over, each running the kinds its OS runs.
fn node_models() -> [NodeModel; 2] {
    let limits = hex_sim::replay::Limits::default();
    let parse = |text: &str| hex_sim::replay::parse(text, &limits).unwrap();
    [
        NodeModel {
            name: "m5d.metal",
            nodes: 10,
            days: [
                parse(include_str!("../../../bench/sched/huawei-r5-day30.trace")),
                parse(include_str!(
                    "../../../bench/sched/huawei-r5-day30.keepalive.trace"
                )),
            ],
            speeds: [[35, 14]; 4],
        },
        NodeModel {
            name: "fleet",
            nodes: 50,
            days: [
                parse(include_str!(
                    "../../../bench/sched/huawei-r5-day30.fleet.trace"
                )),
                parse(include_str!(
                    "../../../bench/sched/huawei-r5-day30.fleet.keepalive.trace"
                )),
            ],
            speeds: [[126, 42], [35, 14], [22, 8], [122, 37]],
        },
    ]
}

/// The day as recorded, a mix of the three kinds, and the same day as each kind alone.
fn mix_and_alone(day: &Cluster) -> [(&'static str, Cluster); 4] {
    [
        ("mix", day.clone()),
        ("oci", packing::alone(day, Class::Oci).unwrap()),
        (
            "firecracker",
            packing::alone(day, Class::Firecracker).unwrap(),
        ),
        ("shards", packing::alone(day, Class::Shards).unwrap()),
    ]
}

/// G2 step 3 (`AC-T7.4.3`'s recorded workload): read a day of 25,295 recorded launches of 339
/// owners, each an OCI container, a Firecracker microVM or a Shards microVM, on each node model,
/// through the typed parser, and replay it twice from one seed, with its lifetimes as recorded and
/// with the keep-alive; expect the same trace hash, report and log, every launch done once, and the
/// log and the hard filters correct by the specification. Non-vacuity: the kind filter refuses nodes
/// on the fleet, where some nodes run only some kinds, and none on m5d.metal, where each runs every
/// kind.
#[test]
fn a_recorded_workload_replays_the_same_twice() {
    for model in node_models() {
        for cluster in model.days {
            replays_the_same_twice(model.name, model.nodes, &cluster);
        }
    }
}

fn replays_the_same_twice(model: &str, nodes: usize, cluster: &Cluster) {
    assert_eq!(
        (
            cluster.nodes.len(),
            cluster.workloads.len(),
            cluster.owners.len()
        ),
        (nodes, 25_295, 339),
        "{model}"
    );
    let config = Config::default();
    let a = sim::run(cluster, &config).unwrap();
    let b = sim::run(cluster, &config).unwrap();
    assert!(same(&a.report, &b.report), "{model}");
    assert_eq!(a.store.log(), b.store.log(), "{model}");
    let end = spec::check_end(cluster, &a);
    assert_eq!(end.lost_or_double(), 0, "{model}: {end:?}");
    assert_eq!(end.complete, 25_295, "{model}");
    spec::check_log(&cluster.nodes, &a).unwrap();
    assert_eq!(
        spec::check_filters(cluster, &a).unwrap().violations(),
        0,
        "{model}"
    );
    let every_node_runs_every_kind = cluster.nodes.iter().all(|n| n.classes == Classes::ALL);
    assert_eq!(
        a.report.refused.class == 0,
        every_node_runs_every_kind,
        "{model}: {:?}",
        a.report.refused
    );
}

/// AC-T7.4.3 (G2 step 4): pack the recorded workload to 90% load from `packing::SEEDS` starting
/// arrivals drawn from the day, on each node model, as a mix of the three kinds and as each kind alone, under best
/// fit alone (by free CPU first, and across resources as Kubernetes' `MostAllocated` does) and under
/// Hex's scorer, three ways: with no completions, and with completions at each set's speeds,
/// lifetimes as recorded and with the keep-alive; expect Hex's stranded capacity at least 30% below
/// each best fit's on every seed of each. Non-vacuity: best fit strands something, every arm reaches 90%, and under
/// completions some launch ends on the way, on every seed. Not bought by refusing work: Hex's scorer
/// passes over no more launches on the way than either best fit.
#[test]
fn the_recorded_workload_strands_at_least_30_percent_less_than_best_fit() {
    for model in node_models() {
        let [recorded, kept_alive] = model.days;
        let sets = mix_and_alone(&recorded)
            .into_iter()
            .zip(mix_and_alone(&kept_alive))
            .zip(model.speeds);
        for (((set, recorded), (_, kept_alive)), [as_recorded, with_keep_alive]) in sets {
            let set = format!("{} {set}", model.name);
            for (cluster, clock) in [
                (&recorded, Clock::Held),
                (&recorded, Clock::Compressed(as_recorded)),
                (&kept_alive, Clock::Compressed(with_keep_alive)),
            ] {
                strands_less(&set, cluster, clock);
            }
        }
    }
}

fn strands_less(set: &str, cluster: &Cluster, clock: Clock) {
    let pick = Pick {
        shapes: Shapes::of(packing::minimal_shapes(cluster)).unwrap(),
        ..Pick::default()
    };
    let pack = |scorer, from| {
        packing::pack(
            cluster,
            &Pick { scorer, ..pick },
            packing::LOAD_PER_MILLE,
            from,
            clock,
        )
        .unwrap()
    };
    let (mut passed_hex, mut passed_best, mut passed_most) = (0, 0, 0);
    for seed in 0..packing::SEEDS {
        let from = packing::start(seed, cluster.workloads.len());
        let hex = pack(Scorer::HEX, from);
        let best = pack(Scorer::BestFit, from);
        let most = pack(Scorer::MostAllocated, from);
        for p in [&hex, &best, &most] {
            assert!(p.reached, "{set} {clock:?} seed {seed}: {p:?}");
            assert_eq!(
                p.completions > 0,
                clock != Clock::Held,
                "{set} {clock:?} seed {seed}: {p:?}"
            );
        }
        for base in [&best, &most] {
            assert!(
                base.stranded_per_mille() > 0,
                "{set} {clock:?} seed {seed}: {base:?}"
            );
            assert!(
                hex.stranded_per_mille() * 10 <= base.stranded_per_mille() * 7,
                "{set} {clock:?} seed {seed}: Hex strands {} per-mille, best fit {}",
                hex.stranded_per_mille(),
                base.stranded_per_mille()
            );
        }
        passed_hex += hex.passed_over;
        passed_best += best.passed_over;
        passed_most += most.passed_over;
    }
    assert!(
        passed_hex <= passed_best.min(passed_most),
        "{set} {clock:?}: {passed_hex} {passed_best} {passed_most}"
    );
}

/// G2 step 3 (`AC-T7.4.3`'s recorded kiln workload): read fourteen days of kiln's own CI
/// (bench/sched/kiln_workload.md says where and when it was recorded) through the typed parser and
/// replay it twice from one seed; expect the same trace hash, report and log, every leg done once,
/// and the log and the hard filters correct by the specification.
#[test]
fn a_recorded_kiln_workload_replays_the_same_twice() {
    let text = include_str!("../../../bench/sched/kiln-ci-20261022-20261105.trace");
    let cluster = hex_sim::replay::parse(text, &hex_sim::replay::Limits::default()).unwrap();
    assert_eq!((cluster.nodes.len(), cluster.workloads.len()), (10, 4_655));
    let config = Config::default();
    let a = sim::run(&cluster, &config).unwrap();
    let b = sim::run(&cluster, &config).unwrap();
    assert!(same(&a.report, &b.report));
    assert_eq!(a.store.log(), b.store.log());
    let end = spec::check_end(&cluster, &a);
    assert_eq!(end.lost_or_double(), 0, "{end:?}");
    assert_eq!(end.complete, 4_655);
    spec::check_log(&cluster.nodes, &a).unwrap();
    assert_eq!(spec::check_filters(&cluster, &a).unwrap().violations(), 0);
}
