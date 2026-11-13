//! Cells and regions (`docs/design/hex-sim.md` H64-H67, H69): the differential suite on one node, one
//! cell and three regions, the cell-loss drill, and a lost region's work re-placed within the
//! others' capacity, each over Hex's three classes: every workload an OCI container, a Firecracker
//! microVM or a Shards microVM by G2's rule (by owner, in thirds, with each class's overhead), on
//! nodes, cells and regions that declare the classes they run. Every test runs on both of G2's node
//! models: EC2 m5d.metal (the main one, every class on every node) and Hex's fleet beside it (its ten
//! hosts repeated, Firecracker on the two Linux ones, OCI on those and the Windows one, Shards on all).

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

use hex_sim::fair::Policy;
use hex_sim::federation::{self, CellRun, FedConfig, FedRun, Loss, Moved, Topology};
use hex_sim::generate::{self, NodeModel, Shape};
use hex_sim::index::{FitIndex, Pick};
use hex_sim::model::{Class, Cluster, Kind, WorkloadId};
use hex_sim::rng::Rng;
use hex_sim::sim::{self, Config, Nemesis};
use hex_sim::spec;
use hex_sim::store::State;

const SECOND: u64 = 1_000_000_000;
const CLASSES: [Class; 3] = [Class::Oci, Class::Firecracker, Class::Shards];

/// The cells' configuration in submit order: G7's runs are of record under it, the only order when
/// they were measured (`docs/design/hex-sim.md`, "What it doesn't do yet").
fn submit_order() -> Config {
    Config {
        policy: Policy::Fifo,
        ..Config::default()
    }
}

fn at(class: Class) -> usize {
    CLASSES.iter().position(|&c| c == class).unwrap()
}

/// G2's rule on a generated cluster: owners at the recorded day's 75 launches each, each owner's
/// workloads all one class, a third of the launches each, and each class's overhead in its memory.
fn classed(mut cluster: Cluster, seed: u64) -> Cluster {
    generate::owners(&mut cluster, seed, 75).unwrap();
    generate::by_owner(&mut cluster, generate::overheads()).unwrap();
    cluster
}

const MODELS: [NodeModel; 2] = [NodeModel::M5dMetal, NodeModel::Fleet];

fn fed(topology: Topology, cell: Config, loss: Loss) -> FedConfig {
    FedConfig {
        topology,
        cell,
        loss,
        // One region has no grace (no other watches it), and no region loss to use one.
        region_grace_ns: federation::region_grace_ns(topology.regions).unwrap_or(0),
    }
}

/// What the specification fixes in every cell of a run (H67): its log one the serial specification
/// makes, nothing lost or started twice, the hard filters held.
fn check_cells<'a>(cells: impl IntoIterator<Item = &'a CellRun>, what: &str) {
    for c in cells {
        let at = format!("{what}, region {} cell {}", c.region, c.cell);
        spec::check_slices(&c.cluster, &c.run).unwrap_or_else(|v| panic!("{at}: {v}"));
        let end = spec::check_end(&c.cluster, &c.run);
        assert_eq!(end.lost_or_double(), 0, "{at}: {end:?}");
        let filters = spec::check_filters(&c.cluster, &c.run).unwrap();
        assert_eq!(filters.violations(), 0, "{at}: {filters:?}");
    }
}

fn suite_seed(seed: u64, model: NodeModel) -> (Cluster, Config) {
    let mut r = Rng::new(seed ^ 0xce11);
    let shape = Shape {
        nodes: u32::try_from(r.range(6, 30)).unwrap(),
        workloads: r.range(20, 200),
        mean_gap: r.range(0, 40_000),
        mean_run: if r.chance(150) {
            None
        } else {
            Some(r.range(1, 2_000_000))
        },
        gpu_nodes_per_mille: r.range(0, 500),
        max_share_per_mille: r.range(50, 1000),
    };
    let mut cluster = generate::cluster(seed, &shape).unwrap();
    cluster.nodes = model.nodes(shape.nodes);
    let cluster = classed(cluster, seed);
    let config = Config {
        workers: u32::try_from(r.range(1, 8)).unwrap(),
        decide_ns: r.range(0, 200_000),
        commit_ns: r.range(0, 100_000),
        change_window: usize::try_from(r.range(1, 64)).unwrap(),
        restart_ns: r.range(0, 5_000_000),
        ..submit_order()
    };
    (cluster, config)
}

fn crashing(config: &Config, seed: u64, workloads: u64) -> Config {
    let mut r = Rng::new(seed ^ 0xdead);
    Config {
        nemesis: Nemesis::Random {
            seed: r.next_u64(),
            first: r.range(0, workloads.max(2) - 1),
            per_mille: r.range(5, 200),
            max_crashes: u32::try_from(r.range(1, 8)).unwrap(),
        },
        ..*config
    }
}

/// AC-T7.12.3: do 300 generated seeds on each node model, each at one node, one cell and three
/// regions of two cells, clean and under the random crash nemesis; expect every cell of every run to keep what the
/// specification fixes (its log the serial specification's, lost-or-double 0, the filters held), one
/// cell through the federation to be `sim::run` exactly, no workload on a node that doesn't run its
/// class, and (non-vacuity) three regions to place work in every region, every class placed and
/// some refused for no node of its class, and the nemesis to crash.
#[test]
fn the_differential_suite_passes_on_one_node_one_cell_and_three_regions() {
    let (mut crashes, mut regions_placing, mut runs) = (0u64, 0u64, 0u64);
    let (mut placed_by_class, mut refused) = ([0u64; 3], 0u64);
    for (seed, model) in (0..300).flat_map(|s| MODELS.map(|m| (s, m))) {
        let (cluster, base) = suite_seed(seed, model);
        let n = u64::try_from(cluster.workloads.len()).unwrap();
        for config in [base, crashing(&base, seed, n)] {
            let one_node = Cluster {
                nodes: cluster.nodes[..1].to_vec(),
                ..cluster.clone()
            };
            let scales = [
                (
                    "one node",
                    &one_node,
                    Topology {
                        regions: 1,
                        cells: 1,
                    },
                ),
                (
                    "one cell",
                    &cluster,
                    Topology {
                        regions: 1,
                        cells: 1,
                    },
                ),
                (
                    "three regions",
                    &cluster,
                    Topology {
                        regions: 3,
                        cells: 2,
                    },
                ),
            ];
            for (what, c, topology) in scales {
                let what = format!(
                    "seed {seed} on {}, {what}, {:?}",
                    model.name(),
                    config.nemesis
                );
                let out = federation::run(c, &fed(topology, config, Loss::None))
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                check_cells(&out.cells, &what);
                for x in &out.cells {
                    for (k, w) in x.cluster.workloads.iter().enumerate() {
                        let id = WorkloadId(u64::try_from(k).unwrap());
                        if matches!(
                            x.run.store.record(id).map(|r| r.state),
                            Some(State::Placed(_) | State::Complete(_))
                        ) {
                            placed_by_class[at(w.class)] += 1;
                        }
                    }
                    refused += spec::check_end(&x.cluster, &x.run).refused;
                }
                let placed: u64 = out.cells.iter().map(|x| x.run.report.placements).sum();
                crashes += out.cells.iter().map(|x| x.run.report.crashes).sum::<u64>();
                runs += 1;
                if topology.cells == 1 {
                    // One cell is today's hex-sim, bit for bit (H64).
                    let alone = sim::run(c, &config).unwrap();
                    assert_eq!(out.cells.len(), 1);
                    assert_eq!(out.cells[0].run.report, alone.report, "{what}");
                    assert_eq!(out.cells[0].cluster, *c, "{what}");
                } else if placed > 0 {
                    let every = (0..3).all(|r| {
                        out.cells
                            .iter()
                            .any(|x| x.region == r && x.run.report.placements > 0)
                    });
                    regions_placing += u64::from(every);
                }
            }
        }
    }
    println!(
        "AC-T7.12.3 on both models: {runs} runs, {crashes} crashes, placed by class (OCI, Firecracker, Shards) \
         {placed_by_class:?}, {refused} refused, {regions_placing} three-region runs placing in every region"
    );
    assert_eq!(runs, 3_600);
    assert!(crashes > 0, "{crashes} crashes");
    assert!(
        placed_by_class.iter().all(|&p| p > 0),
        "{placed_by_class:?}"
    );
    assert!(refused > 0, "{refused} refused");
    assert!(
        regions_placing > 0,
        "{regions_placing} three-region runs placed in every region"
    );
}

/// Work that never ends, all of it arriving in the first virtual second, onto `nodes` nodes, at a
/// tenth of a node's CPU and memory each at most (about 15 a node), so a cell's room is about 15 times
/// its nodes, in G2's classes, on `model`'s nodes: each a node model's, so the regions and cells cut
/// from them hold the model's mix.
fn steady(seed: u64, nodes: u32, workloads: u64, model: NodeModel) -> Cluster {
    let shape = Shape {
        nodes,
        workloads,
        mean_gap: SECOND / workloads,
        mean_run: None,
        gpu_nodes_per_mille: 0,
        max_share_per_mille: 100,
    };
    let mut cluster = generate::cluster(seed, &shape).unwrap();
    for w in &mut cluster.workloads {
        if w.kind == Kind::Benchmark {
            w.kind = Kind::Plain;
        }
        w.arrives = w.arrives.min(SECOND - 1);
    }
    cluster.nodes = model.nodes(nodes);
    classed(cluster, seed)
}

/// Each class's capacity, measured: `steady`'s workloads on `nodes` nodes of `model`, through
/// `topology`, offered in a quantity that doubles, from one a node, until every class has work left
/// waiting (so each is saturated, seen rather than assumed), and the placements of each class
/// counted then. Each drill's size is set from this, so "room" and "tight" are what the model's
/// nodes hold, not a guess. At most 12 doublings (4,096 a node).
fn capacity(model: NodeModel, nodes: u32, topology: Topology) -> [u64; 3] {
    let config = Config {
        max_events: 1 << 25,
        ..submit_order()
    };
    let mut offer = u64::from(nodes);
    for _ in 0..=12 {
        let cluster = steady(99, nodes, offer, model);
        let out = federation::run(&cluster, &fed(topology, config, Loss::None)).unwrap();
        let (mut placed, mut waiting) = ([0u64; 3], [0u64; 3]);
        for c in &out.cells {
            for (k, w) in c.cluster.workloads.iter().enumerate() {
                let id = WorkloadId(u64::try_from(k).unwrap());
                match c.run.store.record(id).map(|r| r.state) {
                    Some(State::Placed(_)) => placed[at(w.class)] += 1,
                    Some(State::Pending) => waiting[at(w.class)] += 1,
                    _ => {}
                }
            }
        }
        if waiting.iter().all(|&w| w > 0) {
            return placed;
        }
        offer *= 2;
    }
    panic!(
        "{} didn't saturate every class at 4,096 workloads a node",
        model.name()
    );
}

/// Three regions of two cells, the region drills' topology.
const REGIONS: Topology = Topology {
    regions: 3,
    cells: 2,
};

/// AC-T7.12.2, on each node model: do the cell-loss drill: two regions of three cells, lose cell 1 of
/// region 0's control plane for good at 2 s; expect every other cell's run (report and trace) the same
/// as without the loss, so other cells' workloads disturbed is exactly 0, and the lost cell's running
/// work kept running; (non-vacuity) the lost cell had work running and waiting when it was lost, and
/// the other cells ran work of every class.
#[test]
fn a_cells_control_plane_loss_disturbs_no_workload_in_another_cell() {
    for model in MODELS {
        cell_loss(model);
    }
}

fn cell_loss(model: NodeModel) {
    // Every class offered 1.2 times what it can hold, so the lost cell has work waiting.
    let topology = Topology {
        regions: 2,
        cells: 3,
    };
    let cap = capacity(model, 60, topology);
    let n = cap.iter().max().unwrap() * 36 / 10;
    let cluster = steady(11, 60, n, model);
    let config = Config {
        workers: 4,
        ..submit_order()
    };
    let lost = Loss::Cell {
        region: 0,
        cell: 1,
        at: 2 * SECOND,
    };
    let base = federation::run(&cluster, &fed(topology, config, Loss::None)).unwrap();
    let drill = federation::run(&cluster, &fed(topology, config, lost)).unwrap();
    assert_eq!(base.cells.len(), 6);
    assert_eq!(drill.cells.len(), 6);
    let (mut others, mut disturbed, mut ran) = (0u64, 0u64, [0u64; 3]);
    for (b, d) in base.cells.iter().zip(&drill.cells) {
        assert_eq!((b.region, b.cell), (d.region, d.cell));
        if (d.region, d.cell) == (0, 1) {
            let kept = spec::check_kept_running(&d.cluster, &d.run);
            assert_eq!(kept.disturbed, 0, "{kept:?}");
            let end = spec::check_end(&d.cluster, &d.run);
            assert!(end.running > 0 && end.waiting > 0, "{end:?}");
            assert_eq!(
                d.run.report.takeovers.len(),
                1,
                "{:?}",
                d.run.report.takeovers
            );
            assert!(
                d.run
                    .report
                    .takeovers
                    .iter()
                    .all(|t| t.takeover_ns.is_none())
            );
            continue;
        }
        for (k, w) in d.cluster.workloads.iter().enumerate() {
            ran[at(w.class)] += u64::from(d.run.ran[k].is_some());
        }
        let kept = spec::check_kept_running(&d.cluster, &d.run);
        others += kept.kept;
        disturbed += kept.disturbed;
        assert_eq!(
            b.run.report, d.run.report,
            "region {} cell {}",
            d.region, d.cell
        );
        assert_eq!(b.run.ran, d.run.ran, "region {} cell {}", d.region, d.cell);
    }
    check_cells(&drill.cells, "cell-loss drill");
    println!(
        "AC-T7.12.2 on {}: other cells disturbed {disturbed}, kept {others}, ran by class (OCI, \
         Firecracker, Shards) {ran:?}",
        model.name()
    );
    assert_eq!(disturbed, 0);
    assert!(others > 0, "{others} workloads ran in the other cells");
    assert!(
        ran.iter().all(|&r| r > 0),
        "{ran:?} ran in the other cells, by class"
    );
}

/// Every moved workload placed where the federation sent it, and each refused one held by no
/// surviving node at the end (a brute-force scan of every node, not the federation's own check).
fn check_moved(cluster: &Cluster, out: &FedRun, lost_region: u32) -> ([u64; 3], [u64; 3]) {
    let (mut to, mut refused) = ([0u64; 3], [0u64; 3]);
    for &(id, moved) in &out.moved {
        match moved {
            Moved::To { region, cell } => {
                assert_ne!(region, lost_region);
                let c = out
                    .cells
                    .iter()
                    .find(|c| (c.region, c.cell) == (region, cell))
                    .unwrap();
                let local = c.ids.iter().position(|&g| g == id).unwrap();
                let state = c
                    .run
                    .store
                    .record(WorkloadId(u64::try_from(local).unwrap()))
                    .map(|r| r.state);
                assert!(
                    matches!(state, Some(State::Placed(_))),
                    "{id:?} moved to region {region} cell {cell} is {state:?}"
                );
                to[at(c.cluster.workloads[local].class)] += 1;
            }
            Moved::NoCapacity => {
                let spec = cluster.workloads[usize::try_from(id.0).unwrap()];
                for c in out.cells.iter().filter(|c| c.region != lost_region) {
                    let mut index = FitIndex::new(&c.cluster.nodes).unwrap();
                    for (at, w) in c.cluster.workloads.iter().enumerate() {
                        if let Some(State::Placed(node)) = c
                            .run
                            .store
                            .record(WorkloadId(u64::try_from(at).unwrap()))
                            .map(|r| r.state)
                        {
                            index.take(node, w.id, &w.demand()).unwrap();
                        }
                    }
                    let room = index.fit(&spec.demand(), &Pick::default()).best;
                    assert!(
                        room.is_none(),
                        "{id:?} refused while region {} cell {} had room on {:?}",
                        c.region,
                        c.cell,
                        room.map(|k| k.node)
                    );
                }
                refused[at(spec.class)] += 1;
            }
        }
    }
    (to, refused)
}

/// Lose region `lost` of three, two cells each, at 2 s; run until done; return the run and, by
/// class, the moved workloads placed and refused.
fn lose_region(model: NodeModel, cluster: &Cluster, lost: u32) -> (FedRun, [u64; 3], [u64; 3]) {
    let config = Config {
        workers: 4,
        max_events: 1 << 24,
        ..submit_order()
    };
    let loss = Loss::Region {
        region: lost,
        at: 2 * SECOND,
    };
    let out = federation::run(cluster, &fed(REGIONS, config, loss)).unwrap();
    check_cells(out.cells.iter().filter(|c| c.region != lost), "region lost");
    let (to, refused) = check_moved(cluster, &out, lost);
    println!(
        "T-T7.12.1 on {}: region {lost} lost, moved placed by class (OCI, Firecracker, Shards) \
         {to:?}, refused {refused:?}",
        model.name()
    );
    for &(id, m) in &out.moved {
        if let Moved::To { region, cell } = m {
            let c = out
                .cells
                .iter()
                .find(|c| (c.region, c.cell) == (region, cell))
                .unwrap();
            assert!(
                c.classes
                    .has(cluster.workloads[usize::try_from(id.0).unwrap()].class)
            );
        }
    }
    (out, to, refused)
}

/// T-T7.12.1, with room, of record on the main model (all m5d.metal, every node every kind) and beside
/// it on the fleet (every region the fleet's mix, so a lost region's Firecracker goes to the others'
/// Linux hosts): do lose one of three regions while each is about a third full; expect every workload
/// of the lost region re-placed in another region, in a cell and on a node that runs its class (the
/// cells' filter check), none refused, each where the federation sent it, every surviving cell's log the serial
/// specification's and nothing lost or doubled; (non-vacuity) every class moved.
#[test]
fn a_lost_regions_work_is_re_placed_in_the_others_when_they_have_room() {
    for model in MODELS {
        // The scarcest class at a third of what it can hold, the rest below.
        let n = *capacity(model, 300, REGIONS).iter().min().unwrap();
        let cluster = steady(21, 300, n, model);
        let (out, to, refused) = lose_region(model, &cluster, 1);
        assert_eq!(refused, [0, 0, 0], "{}", model.name());
        assert!(
            to.iter().all(|&t| t > 0),
            "{to:?} moved by class on {}",
            model.name()
        );
        assert_eq!(
            u64::try_from(out.moved.len()).unwrap(),
            to.iter().sum::<u64>()
        );
    }
}

/// T-T7.12.1, tight, on each node model: do lose one of three regions while each is past full; expect
/// the moved work placed only within the others' capacity for its class: each moved workload placed
/// where the federation sent it, each refused one held by no surviving node (brute force), no node over
/// its capacity (the specification's replay); and (non-vacuity) some moved work placed and some refused.
#[test]
fn a_lost_regions_work_is_re_placed_only_within_the_others_capacity() {
    for model in MODELS {
        // The scarcest class at 90% of what it can hold before the loss.
        let n = capacity(model, 300, REGIONS).iter().min().unwrap() * 27 / 10;
        let cluster = steady(22, 300, n, model);
        let (_, to, refused) = lose_region(model, &cluster, 1);
        let (to, refused): (u64, u64) = (to.iter().sum(), refused.iter().sum());
        assert!(
            to > 0 && refused > 0,
            "{to} placed, {refused} refused on {}",
            model.name()
        );
    }
}

/// H66's fence, on each node model: do lose a region; expect every workload the lost region held and
/// hadn't finished by the loss moved exactly once, and no workload of the lost region started in two
/// surviving cells.
#[test]
fn a_lost_regions_work_moves_once() {
    for model in MODELS {
        let n = *capacity(model, 300, REGIONS).iter().min().unwrap();
        let cluster = steady(23, 300, n, model);
        let (out, _, _) = lose_region(model, &cluster, 1);
        let held: u64 = out
            .cells
            .iter()
            .filter(|c| c.region == 1)
            .map(|c| u64::try_from(c.ids.len()).unwrap())
            .sum();
        assert_eq!(u64::try_from(out.moved.len()).unwrap(), held);
        let mut seen = std::collections::HashSet::new();
        for c in out.cells.iter().filter(|c| c.region != 1) {
            for (at, &g) in c.ids.iter().enumerate() {
                if c.run.starts[at] > 0 {
                    assert!(seen.insert(g), "{g:?} started in two cells");
                }
            }
        }
    }
}

/// T-T7.12.1 and H66's "assume and deduct", on each node model: do lose one of three regions a third
/// full, with 3,000 more workloads arriving in the very instant the federation acts, so the surviving
/// regions have them queued and not yet placed; expect the moved work placed only in room those queued
/// workloads leave (each moved one placed where it was sent, each refused one held by no surviving
/// node), and (non-vacuity) over 1,000 queued in the surviving regions then, some moved and some refused.
#[test]
fn a_lost_regions_work_waits_behind_what_the_others_have_queued() {
    for model in MODELS {
        queued_ahead(model);
    }
}

fn queued_ahead(model: NodeModel) {
    let when = 2 * SECOND + federation::region_grace_ns(REGIONS.regions).unwrap();
    // A class of capacity C, a third full (C/9 a region), loses a region's C/9 while the survivors
    // have 4C/9 free; a burst B of it arriving as the federation acts queues 2B/9 there. Moved work
    // then both lands and is refused while 2B/9 is between 3C/9 and 4C/9: B = 1.75C of each class,
    // 5.25C in all.
    let c = *capacity(model, 300, REGIONS).iter().min().unwrap();
    let mut cluster = steady(24, 300, c, model);
    let burst = steady(25, 300, c * 525 / 100, model);
    // The burst's workloads keep its own day's owners, so the cluster names every one of them (all of
    // weight 1): a workload naming an owner its cluster doesn't is refused (F1).
    if burst.owners.len() > cluster.owners.len() {
        cluster.owners.clone_from(&burst.owners);
    }
    let first = cluster.workloads.len();
    for (k, w) in burst.workloads.iter().enumerate() {
        cluster.workloads.push(hex_sim::model::WorkloadSpec {
            id: WorkloadId(u64::try_from(first + k).unwrap()),
            arrives: when,
            ..*w
        });
    }
    let (out, to, refused) = lose_region(model, &cluster, 1);
    let (to, refused): (u64, u64) = (to.iter().sum(), refused.iter().sum());
    let moved: std::collections::HashSet<WorkloadId> = out.moved.iter().map(|m| m.0).collect();
    let queued: usize = out
        .cells
        .iter()
        .filter(|c| c.region != 1)
        .map(|c| {
            c.ids
                .iter()
                .filter(|g| usize::try_from(g.0).unwrap() >= first && !moved.contains(g))
                .count()
        })
        .sum();
    let m = model.name();
    assert!(
        queued > 0,
        "{queued} queued in the surviving regions on {m}"
    );
    assert!(to > 0, "{to} moved on {m}");
    assert!(refused > 0, "{refused} refused on {m}");
}

/// H64's routing, on each node model: do route 9,000 small workloads over three regions of three
/// cells (home region `id mod regions`, home cell `(id / regions) mod cells`); expect every one of the
/// nine cells to get about a ninth, so a region's cell isn't tied to the region the id picked.
#[test]
fn every_cell_gets_work_when_regions_and_cells_share_a_factor() {
    for model in MODELS {
        let cluster = steady(31, 90, 9_000, model);
        let topology = Topology {
            regions: 3,
            cells: 3,
        };
        let out = federation::run(&cluster, &fed(topology, submit_order(), Loss::None)).unwrap();
        assert_eq!(out.cells.len(), 9);
        for c in &out.cells {
            assert!(
                (800..1_200).contains(&c.ids.len()),
                "{}: region {} cell {}: {} workloads",
                model.name(),
                c.region,
                c.cell,
                c.ids.len()
            );
        }
    }
}

/// T-T7.12.1's typed refusal, as its own case: a topology where no surviving node runs a kind. Three
/// regions of 100 nodes, each of one OS's fleet hosts (Linux: adacachy and fedora; Windows: jupiter;
/// macOS: the seven Macs). That isn't a fleet's regions, which each hold a mix; it's the case where one
/// region is the only place a kind runs. Do lose the Linux region; expect every one of its Firecracker
/// microVMs refused, typed (`Moved::NoCapacity`), each held by no surviving node (the brute-force
/// check), and its OCI and Shards work moved within the others' capacity.
#[test]
fn a_kind_no_surviving_node_runs_is_refused_typed() {
    let n = *capacity(NodeModel::Fleet, 300, REGIONS)
        .iter()
        .min()
        .unwrap();
    let mut cluster = steady(26, 300, n, NodeModel::Fleet);
    for (i, node) in cluster.nodes.iter_mut().enumerate() {
        let k = u32::try_from(i % 100).unwrap();
        let host = match i / 100 {
            0 => k % 2,     // Linux
            1 => 2,         // Windows
            _ => 3 + k % 7, // macOS
        };
        *node = hex_sim::model::NodeSpec {
            id: node.id,
            ..NodeModel::Fleet.node(host)
        };
    }
    let (out, to, refused) = lose_region(NodeModel::Fleet, &cluster, 0);
    let fc = out
        .moved
        .iter()
        .filter(|m| cluster.workloads[usize::try_from(m.0.0).unwrap()].class == Class::Firecracker)
        .count();
    let fc = u64::try_from(fc).unwrap();
    assert!(fc > 0, "{fc} Firecracker microVMs moved");
    assert_eq!(
        (to[1], refused[1]),
        (0, fc),
        "every Firecracker microVM refused, typed"
    );
    assert!(to[0] > 0 && to[2] > 0, "{to:?}: OCI and Shards moved");
}

/// H66's grace, by the detector's math (the lead 2026-11-06): do ask for the grace of three and of
/// eight regions; expect hyper-swim's crash bound, `(2(2m - 1) + 1)` periods for `m` other regions
/// (G5's H40), each period a direct stage and a relay stage at RFC 6298's timeout for the slowest
/// round trip between G3's Azure regions (331 ms: SRTT + 4 x RTTVAR = 3 x 331 ms after a first
/// sample, floored at 1 s), plus one round trip for another member's answer; and refused, typed, for
/// a single region, which has no other to watch it.
#[test]
fn a_region_is_called_lost_by_the_node_detectors_math() {
    assert_eq!(federation::SLOWEST_REGION_RTT_NS, 331_000_000);
    assert_eq!(
        federation::region_grace_ns(3).unwrap(),
        7 * 2 * SECOND + 331_000_000
    );
    assert_eq!(
        federation::region_grace_ns(8).unwrap(),
        27 * 2 * SECOND + 331_000_000
    );
    assert!(federation::region_grace_ns(1).is_err());
}
