//! AC-T7.11.2's bench: wall time to place 1,000,000 pending workloads on 10,000 nodes, on a release
//! build, against a bar of 120 s (G6 step 4; the method is `docs/design/hex-sim.md` H63).
//!
//!   cargo bench -p hex-bench --bench sched_million -- [runs] [slices] [pods|mixed] [check] [fleet]
//!
//! The nodes are 10,000 of one of G2's node models (H69): EC2 m5d.metal, the main one (48 cores,
//! 384 GiB, every class), or with `fleet` Hex's fleet beside it, its ten hosts repeated in G2's order
//! (`generate::NodeModel`). Row `pods` is 1,000,000 of `scheduler_perf`'s pause pod, 100m and 500Mi
//! (a fifth of the m5d.metal nodes' CPU). On the fleet a million don't fit (its Linux and Windows
//! hosts can't hold the OCI and Firecracker thirds), so the fleet's row reports what each class placed,
//! not the bar. Row `mixed` draws each need from G2's recorded Huawei Cloud day
//! (`huawei-r5-day30.trace`), scaled so its means are the pod's (the same budget), so a one-shape best
//! case never stands alone (`recorded_needs`).
//!
//! The control plane is sharded as H60 decides, by hex-sim's own `sim::Layout`: the nodes are split
//! into `slices` contiguous ranges, and each workload is routed to the first slice, from slice
//! `id mod slices` on, holding a node that would admit it empty. Every workload is submitted to the one store before the clock starts, so all
//! 1,000,000 are pending at once and none ends. Then one thread per slice runs hex-sim's own fast path
//! over its slice's pending workloads in submit order: the best node by Hex's packing score
//! (`FitIndex::fit` with `Scorer::HEX`, H11; the kernel's walk over every node with the room), the commit to the one store
//! by compare-and-set on the version the submit returned (`Store::apply`, behind a mutex: the store is
//! the planes' one shared point, as Amphora's will be), and the index update. The time runs from the
//! first decision to the last commit, wall-clock.
//!
//! With `check`, after each timed run (untimed), each slice's part of the store's log is replayed, in
//! the store's order, into a store of that slice alone, and the serial specification judges it
//! (`spec::check_log`, H4 per slice): each placement the slice's pick at its point, no
//! queue jumped, nothing left pending that a node holds.
//!
//! Every workload is one of Hex's three classes by G2's rule (`docs/design/hex-sim.md` H69): owners
//! at 75 launches each, each owner's workloads all one class, a third of the launches each OCI
//! containers, Firecracker microVMs and Shards microVMs, each with its class's overhead in its memory
//! (OCI 0.42 MiB, Firecracker 5 MiB, Shards 5 MiB, a stand-in). The nodes declare the classes they run,
//! six in ten Linux (all three), two Windows (OCI and Shards), two macOS (Shards), by node id, and a
//! workload is placed only on a node that runs its class. Each run reports its placements by class.
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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hex_sim::fair::Policy;
use hex_sim::generate::{self, NodeModel};
use hex_sim::index::{FitIndex, Pick, Scorer, Shapes};
use hex_sim::model::{
    Class, Cluster, Kind, NodeId, NodeSpec, Owner, Resources, Tenancy, WorkloadId, WorkloadSpec,
};
use hex_sim::rng::Rng;
use hex_sim::sim::{Layout, Report, Run};
use hex_sim::spec;
use hex_sim::store::{Op, Outcome, State, Store, Write};

const NODES: u32 = 10_000;
const WORKLOADS: u64 = 1_000_000;
const MIB: u64 = 1 << 20;
const BAR: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Pods,
    Mixed,
}

/// The mixed row's needs: the recorded day's launches (`bench/sched/huawei-r5-day30.trace`, G2's
/// Huawei Cloud day), each less its recorded class overhead, with CPU and memory scaled so their means
/// are the pause pod's. The day's own means (722m, 669 MiB) times a million exceed what 10,000
/// m5d.metal nodes hold, which AC-T7.11.2 fixes; the scale keeps the day's spread of shapes at the
/// pod's fill. Its launches are all plain, so the row is too.
fn recorded_needs() -> Vec<Resources> {
    let path = format!(
        "{}/../../bench/sched/huawei-r5-day30.trace",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).unwrap();
    let day = hex_sim::replay::parse(&text, &hex_sim::replay::Limits::default()).unwrap();
    let raw: Vec<(u64, u64)> = day
        .workloads
        .iter()
        .map(|w| {
            (
                w.need.cpu_milli,
                day.overheads.own(w.class, w.need.mem_bytes).unwrap(),
            )
        })
        .collect();
    let n = raw.len() as u128;
    let cpu_mean = raw.iter().map(|r| u128::from(r.0)).sum::<u128>() / n;
    let mem_mean = raw.iter().map(|r| u128::from(r.1)).sum::<u128>() / n;
    raw.iter()
        .map(|&(c, m)| Resources {
            cpu_milli: ((u128::from(c) * 100 / cpu_mean) as u64).max(1),
            mem_bytes: ((u128::from(m) * u128::from(500 * MIB) / mem_mean) as u64).max(1),
            gpus: 0,
            disk_bytes: 0,
        })
        .collect()
}

fn workloads(row: Row, seed: u64) -> Vec<WorkloadSpec> {
    let mut rng = Rng::new(seed);
    let recorded = if row == Row::Mixed {
        recorded_needs()
    } else {
        Vec::new()
    };
    let work = (0..WORKLOADS)
        .map(|id| {
            let (need, kind) = match row {
                Row::Pods => (
                    Resources {
                        cpu_milli: 100,
                        mem_bytes: 500 * MIB,
                        gpus: 0,
                        disk_bytes: 0,
                    },
                    Kind::Plain,
                ),
                Row::Mixed => {
                    let at = rng.range(0, recorded.len() as u64 - 1) as usize;
                    (recorded[at], Kind::Plain)
                }
            };
            WorkloadSpec {
                id: WorkloadId(id),
                need,
                kind,
                arrives: 0,
                runs_for: None,
                tenancy: Tenancy::default(),
                class: Class::Oci,
                ask: Default::default(),
            }
        })
        .collect::<Vec<_>>();
    // G2's rule (H69): owners at 75 launches each, each owner's workloads one class, a third each,
    // with its class's overhead in its memory.
    let mut c = Cluster::single_owner(Vec::new(), work);
    generate::owners(&mut c, seed, 75).unwrap();
    generate::by_owner(&mut c, generate::overheads()).unwrap();
    c.workloads
}

/// Each class's Pareto-minimal needs (`packing::minimal_shapes`' set, found by a sort and a sweep:
/// these needs ask for no GPU, so the front is two-dimensional, and the mixed row's ~600,000
/// distinct needs are too many for its pairwise scan).
fn shapes(work: &[WorkloadSpec]) -> Shapes {
    assert!(work.iter().all(|w| w.need.gpus == 0));
    let mut needs: Vec<(Class, u64, u64)> = work
        .iter()
        .map(|w| (w.class, w.need.cpu_milli, w.need.mem_bytes))
        .collect();
    needs.sort_unstable();
    needs.dedup();
    let mut front = Vec::new();
    let mut last: Option<(Class, u64)> = None;
    for (class, cpu, mem) in needs {
        if last.is_none_or(|(c, m)| c != class || mem < m) {
            front.push((
                Resources {
                    cpu_milli: cpu,
                    mem_bytes: mem,
                    gpus: 0,
                    disk_bytes: 0,
                },
                class,
            ));
            last = Some((class, mem));
        }
    }
    assert!(front.len() <= Shapes::MAX, "{} minimal shapes", front.len());
    Shapes::of(front).unwrap()
}

/// One slice: its first global node id, its nodes (ids dense from 0 within it), and its pending
/// workloads in submit order with the version each submit returned.
struct Slice {
    base: u32,
    nodes: Vec<NodeSpec>,
    pending: Vec<(WorkloadSpec, u64)>,
}

struct Placed {
    placed: usize,
    /// Placed by class: OCI, Firecracker, Shards.
    by_class: [usize; 3],
    unplaced: usize,
    began: Instant,
    ended: Instant,
}

/// The nodes, the slices with their routed workloads, and the one store with every workload
/// submitted.
fn setup(row: Row, seed: u64, slices: u32, model: NodeModel) -> (Vec<NodeSpec>, Vec<Slice>, Store) {
    let nodes = model.nodes(NODES);
    let layout = Layout::new(&nodes, slices).unwrap();
    let mut base = 0;
    let mut parts: Vec<Slice> = (0..layout.len())
        .map(|s| {
            let nodes = layout.nodes(s).unwrap().to_vec();
            let part = Slice {
                base,
                nodes,
                pending: Vec::new(),
            };
            base += part.nodes.len() as u32;
            part
        })
        .collect();
    let work = workloads(row, seed);
    let mut store = Store::new(&nodes, work.len(), work.len() * 2).unwrap();
    for w in work {
        let to = layout.route(&w).expect("every workload fits an empty node");
        let Outcome::Applied { version, .. } = store.apply(0, Write::Submit(w)).unwrap() else {
            panic!("submit of {:?}", w.id)
        };
        parts[to].pending.push((w, version));
    }
    (nodes, parts, store)
}

/// The fast path over one slice's pending workloads, in submit order.
fn place(slice: &Slice, store: &Mutex<Store>, pick: &Pick) -> Placed {
    let mut index = FitIndex::new(&slice.nodes).unwrap();
    let began = Instant::now();
    let (mut placed, mut unplaced, mut by_class) = (0, 0, [0usize; 3]);
    for (spec, version) in &slice.pending {
        let demand = spec.demand();
        let Some(best) = index.fit(&demand, pick).best else {
            unplaced += 1;
            continue;
        };
        let write = Write::Place {
            workload: spec.id,
            node: NodeId(slice.base + best.node.0),
            version: *version,
        };
        match store.lock().unwrap().apply(0, write).unwrap() {
            Outcome::Applied { .. } => {}
            other => panic!(
                "the store refused the index's choice for {:?}: {other:?}",
                spec.id
            ),
        }
        index.take(best.node, spec.id, &demand).unwrap();
        placed += 1;
        by_class[spec.class as usize] += 1;
    }
    Placed {
        placed,
        by_class,
        unplaced,
        began,
        ended: Instant::now(),
    }
}

/// The serial specification over one slice's part of the store's log, replayed into a store of that
/// slice alone; and every workload routed there placed.
fn check(slice: &Slice, store: &Store, pick: Pick) -> Result<u64, String> {
    let hi = slice.base + slice.nodes.len() as u32;
    let ours = |w: WorkloadId| {
        slice
            .pending
            .binary_search_by_key(&w, |(s, _)| s.id)
            .is_ok()
    };
    let mut own = Store::new(&slice.nodes, slice.pending.len(), slice.pending.len() * 2).unwrap();
    let mut versions = std::collections::HashMap::with_capacity(slice.pending.len());
    for entry in store.log() {
        match entry.op {
            Op::Submit { workload, .. } if ours(workload) => {
                let spec = store.record(workload).unwrap().spec;
                let Outcome::Applied { version, .. } =
                    own.apply(entry.at, Write::Submit(spec)).unwrap()
                else {
                    return Err(format!("{workload:?} submitted twice"));
                };
                versions.insert(workload, version);
            }
            Op::Place { workload, node }
                if ours(workload) != (slice.base..hi).contains(&node.0) =>
            {
                return Err(format!(
                    "{workload:?} placed on {node:?} across a slice's edge"
                ));
            }
            Op::Place { workload, node } if ours(workload) => {
                let write = Write::Place {
                    workload,
                    node: NodeId(node.0.checked_sub(slice.base).ok_or("node below slice")?),
                    version: versions[&workload],
                };
                match own.apply(entry.at, write).unwrap() {
                    Outcome::Applied { version, .. } => versions.insert(workload, version),
                    other => return Err(format!("replay of {workload:?}: {other:?}")),
                };
            }
            Op::Submit { .. } | Op::Place { .. } => {}
            Op::Complete { .. } => return Err("a completion in a run where none ends".into()),
            Op::Preempt { .. } => return Err("a preemption in a run that preempts none".into()),
        }
    }
    let run = Run {
        report: Report::default(),
        store: own,
        starts: Vec::new(),
        acked: Vec::new(),
        refused: Vec::new(),
        ran: Vec::new(),
        pick,
        // The bench's own fast path takes each slice's work in submit order; that order weighs no
        // owner.
        policy: Policy::Fifo,
        owners: vec![Owner::default()],
        gangs: BTreeMap::new(),
        preemption: false,
        groups: Vec::new(),
        planes: Vec::new(),
    };
    let report = spec::check_log(&slice.nodes, &run).map_err(|v| v.to_string())?;
    if report.submits != slice.pending.len() as u64 || report.placements != report.submits {
        return Err(format!(
            "{} routed, {} submitted, {} placed",
            slice.pending.len(),
            report.submits,
            report.placements
        ));
    }
    Ok(report.placements)
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .filter_map(|a| a.into_string().ok())
        .filter(|a| a != "--bench")
        .collect();
    let runs: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(5);
    let slices: u32 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(8);
    let row = if args.iter().any(|a| a == "mixed") {
        Row::Mixed
    } else {
        Row::Pods
    };
    let checking = args.iter().any(|a| a == "check");
    let model = if args.iter().any(|a| a == "fleet") {
        NodeModel::Fleet
    } else {
        NodeModel::M5dMetal
    };
    let cpus = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    // Hex's packing score (H11, `Scorer::HEX`), as `sched_fast_path.rs` measures it: least stranded
    // against each class's smallest needs, then the node running the fewest classes. With classes,
    // plain best fit puts OCI and Shards work on the Linux nodes Firecracker alone can use.
    let pick = Pick {
        scorer: Scorer::HEX,
        shapes: shapes(&workloads(row, 0)),
    };
    println!(
        "sched_million: {WORKLOADS} pending {} on {NODES} nodes of {}, {slices} slices on {cpus} CPUs, {runs} runs, check={checking}",
        if row == Row::Pods {
            "pods of 100m/500Mi"
        } else {
            "mixed workloads"
        },
        model.name()
    );
    let mut best: Option<(Duration, String)> = None;
    let mut all_ok = true;
    for seed in 0..runs {
        let built = Instant::now();
        let (_nodes, parts, store) = setup(row, seed, slices, model);
        let setup_s = built.elapsed().as_secs_f64();
        let store = Mutex::new(store);
        let done: Vec<Placed> = std::thread::scope(|s| {
            let handles: Vec<_> = parts
                .iter()
                .map(|part| {
                    let (store, pick) = (&store, &pick);
                    s.spawn(move || place(part, store, pick))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let store = store.into_inner().unwrap();
        let began = done.iter().map(|p| p.began).min().unwrap();
        let ended = done.iter().map(|p| p.ended).max().unwrap();
        let wall = ended.saturating_duration_since(began);
        let slowest = done
            .iter()
            .map(|p| p.ended.saturating_duration_since(p.began))
            .max()
            .unwrap();
        let placed: usize = done.iter().map(|p| p.placed).sum();
        let unplaced: usize = done.iter().map(|p| p.unplaced).sum();
        let by_class = done.iter().fold([0usize; 3], |a, p| {
            [
                a[0] + p.by_class[0],
                a[1] + p.by_class[1],
                a[2] + p.by_class[2],
            ]
        });
        let routed: Vec<usize> = parts.iter().map(|p| p.pending.len()).collect();
        let in_store = store
            .records()
            .filter(|r| matches!(r.state, State::Placed(_)))
            .count();
        let mut line = format!(
            "run seed={seed} placed={placed} placed_oci_firecracker_shards={by_class:?} unplaced={unplaced} store_placed={in_store} wall_s={:.3} slowest_slice_s={:.3} placements_per_s={:.0} setup_s={setup_s:.1} routed={routed:?}",
            wall.as_secs_f64(),
            slowest.as_secs_f64(),
            placed as f64 / wall.as_secs_f64(),
        );
        let mut ok = unplaced == 0 && placed as u64 == WORKLOADS && in_store as u64 == WORKLOADS;
        if checking {
            let checked = Instant::now();
            let verdicts: Vec<Result<u64, String>> = std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .iter()
                    .map(|part| {
                        let store = &store;
                        s.spawn(move || check(part, store, pick))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let placements: u64 = verdicts.iter().filter_map(|v| v.as_ref().ok()).sum();
            let failed: Vec<String> = verdicts.into_iter().filter_map(Result::err).collect();
            ok &= failed.is_empty() && placements == WORKLOADS;
            let verdict = if failed.is_empty() {
                "accepted".to_string()
            } else {
                format!("REJECTED {failed:?}")
            };
            write!(
                line,
                " spec_placements={placements} spec={verdict} check_s={:.1}",
                checked.elapsed().as_secs_f64()
            )
            .unwrap();
        }
        all_ok &= ok;
        println!("{line}");
        if ok && best.as_ref().is_none_or(|(b, _)| wall < *b) {
            best = Some((wall, line));
        }
    }
    match best {
        Some((wall, line)) => {
            println!("best-of-{runs} (by wall time): {line}");
            println!(
                "AC-T7.11.2 bar wall <= {} s, every workload placed{} in every run: {}",
                BAR.as_secs(),
                if checking { ", spec accepted" } else { "" },
                if wall <= BAR && all_ok {
                    "MET"
                } else {
                    "NOT MET"
                }
            );
        }
        None => println!("AC-T7.11.2: NOT MET (no run placed every workload)"),
    }
}
