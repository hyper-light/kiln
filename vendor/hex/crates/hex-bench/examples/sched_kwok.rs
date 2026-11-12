//! AC-T7.11.3's cluster, on both sides of the comparison (`docs/design/hex-sim.md` H63, H69,
//! `bench/sched/kube_scheduler_kwok.md`): 10,000 nodes of one of G2's node models, each with
//! a pod limit from its own capacity, and 50,000 of its pause pods (100m CPU, 500Mi). The model is
//! `m5d` (EC2 m5d.metal, 48 cores and 384 GiB, every class; the main one) or `fleet` (Hex's fleet's ten
//! hosts repeated in G2's order, beside it).
//!
//!   cargo run --release -p hex-bench --example sched_kwok -- manifests <out-dir> <files> <m5d|fleet>
//!   cargo run --release -p hex-bench --example sched_kwok -- hex <runs> <shards,...> <m5d|fleet>
//!
//! `manifests` writes the cluster for kube-scheduler on kwok's fake nodes (`bench/sched/kwok/run.sh`
//! drives it): `nodes-<i>.yaml` and `pods-<i>.yaml` for `i` in `0..files`, multi-document YAML, so
//! the files can be created side by side. Each node carries kwok's annotation and taint
//! (kwok.x-k8s.io/node=fake, NoSchedule) and the shape as both capacity and allocatable; each pod
//! requests the pause pod's shape and tolerates kwok's taint.
//!
//! Both sides place Hex's three classes (`docs/design/hex-sim.md` H69). Each node runs its OS's:
//! Linux all three, Windows OCI containers and Shards microVMs, macOS Shards alone; each pod is an OCI container, a Firecracker or a Shards microVM by G2's rule (owners at 75
//! launches each, a third of the pods each), and its memory request carries its class's overhead (OCI
//! 0.42 MiB, Firecracker 5 MiB, Shards 5 MiB, a stand-in). For kube-scheduler a node carries a label
//! `hex.dev/<class>: "true"` for each class it runs (and `kubernetes.io/os` its platform's), and a
//! pod a required node affinity on its class's label (`requiredDuringSchedulingIgnoredDuringExecution`),
//! so kube-scheduler is held to where each class may run, as Hex's index is.
//!
//! `hex` places the same 50,000 on the same 10,000 with hex-sim's own fast path, as
//! `benches/sched_million.rs` does: every pod submitted to the store before the clock starts, the
//! nodes split into `shards` disjoint partitions (node `n` to shard `n % shards`, pod `id % shards`),
//! one thread per shard running the pick by Hex's packing score (`FitIndex::fit`
//! with `Scorer::HEX`, H11; the kernel's walk, no sample floors
//! of 100 nodes or 5%), the compare-and-set commit (`Store::apply`) and the index update, in submit
//! order. The time runs from the first shard starting to the last one ending. After each run
//! (untimed) every record must be placed, and no node above its capacity or its pod limit.
// An example (not production, CLAUDE.md): it takes the test-style allowances, as the benches do.
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

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::Path;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use hex_sim::generate::{self, NodeModel};
use hex_sim::index::{FitIndex, Pick, Scorer, Shapes};
use hex_sim::model::{
    Class, Cluster, Kind, NodeId, NodeSpec, Resources, Tenancy, WorkloadId, WorkloadSpec,
};
use hex_sim::store::{Outcome, State, Store, Write};

const NODES: u32 = 10_000;
const PODS: u64 = 50_000;
const MIB: u64 = 1 << 20;
/// `scheduler_perf`'s pause pod: 100m CPU, 500Mi.
const POD: Resources = Resources {
    cpu_milli: 100,
    mem_bytes: 500 * MIB,
    gpus: 0,
    disk_bytes: 0,
};
/// A node's pod limit, from its own capacity: how many of the pause pod, with the largest class
/// overhead, its CPU and its memory hold. Both sides keep to it: kube-scheduler as the kwok node's
/// `pods` capacity, Hex in `check`. (Kubernetes' own default is 110, a copied number; this one is
/// the node's.)
fn max_pods(node: &NodeSpec) -> u64 {
    let o = generate::overheads();
    let most = [Class::Oci, Class::Firecracker, Class::Shards]
        .iter()
        .filter_map(|&c| o.of(c, POD.mem_bytes))
        .max()
        .unwrap_or(0);
    (node.capacity.cpu_milli / POD.cpu_milli).min(node.capacity.mem_bytes / (POD.mem_bytes + most))
}

fn model(name: &str) -> NodeModel {
    match name {
        "m5d" => NodeModel::M5dMetal,
        "fleet" => NodeModel::Fleet,
        other => panic!("node model {other:?} is neither m5d nor fleet"),
    }
}

fn pods() -> Vec<WorkloadSpec> {
    let work = (0..PODS)
        .map(|id| WorkloadSpec {
            id: WorkloadId(id),
            need: POD,
            kind: Kind::Plain,
            arrives: 0,
            runs_for: None,
            tenancy: Tenancy::default(),
            class: Class::Oci,
            ask: Default::default(),
        })
        .collect();
    let mut c = Cluster::single_owner(Vec::new(), work);
    generate::owners(&mut c, 7, 75).unwrap();
    generate::by_owner(&mut c, generate::overheads()).unwrap();
    c.workloads
}

/// Each class's node label, and its name there.
fn label(class: Class) -> &'static str {
    match class {
        Class::Oci => "oci",
        Class::Firecracker => "firecracker",
        Class::Shards => "shards",
    }
}

const CLASSES: [Class; 3] = [Class::Oci, Class::Firecracker, Class::Shards];

fn manifests(out: &Path, files: u32, model: NodeModel) {
    assert!(files > 0, "at least one file");
    std::fs::create_dir_all(out).unwrap();
    let open = |what: &str, i: u32| {
        BufWriter::new(File::create(out.join(format!("{what}-{i:03}.yaml"))).unwrap())
    };
    let mut w: Vec<_> = (0..files).map(|i| open("nodes", i)).collect();
    for (i, node) in model.nodes(NODES).iter().enumerate() {
        let f = &mut w[i % files as usize];
        let name = format!("kwok-node-{}", node.id.0);
        let r = format!(
            "\n    cpu: {}m\n    memory: \"{}\"\n    pods: \"{}\"",
            node.capacity.cpu_milli,
            node.capacity.mem_bytes,
            max_pods(node)
        );
        let classes: String = CLASSES
            .iter()
            .filter(|&&c| node.classes.has(c))
            .map(|&c| format!("\n    hex.dev/{}: \"true\"", label(c)))
            .collect();
        let os = if node.classes.has(Class::Firecracker) {
            "linux"
        } else if node.classes.has(Class::Oci) {
            "windows"
        } else {
            "darwin"
        };
        write!(
            f,
            "---
apiVersion: v1
kind: Node
metadata:
  name: {name}
  annotations:
    kwok.x-k8s.io/node: fake
    node.alpha.kubernetes.io/ttl: \"0\"
  labels:
    kubernetes.io/arch: amd64
    kubernetes.io/hostname: {name}
    kubernetes.io/os: {os}
    kubernetes.io/role: agent
    type: kwok{classes}
spec:
  taints:
  - effect: NoSchedule
    key: kwok.x-k8s.io/node
    value: fake
status:
  allocatable:{r}
  capacity:{r}
  nodeInfo:
    architecture: amd64
    operatingSystem: {os}
  phase: Running
"
        )
        .unwrap();
    }
    w.iter_mut().for_each(|f| f.flush().unwrap());
    let mut w: Vec<_> = (0..files).map(|i| open("pods", i)).collect();
    for (i, pod) in pods().iter().enumerate() {
        let f = &mut w[i % files as usize];
        write!(
            f,
            "---
apiVersion: v1
kind: Pod
metadata:
  name: pause-{id}
  namespace: default
  labels:
    hex.dev/class: {class}
spec:
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
        - matchExpressions:
          - key: hex.dev/{class}
            operator: In
            values: [\"true\"]
  automountServiceAccountToken: false
  enableServiceLinks: false
  containers:
  - name: pause
    image: registry.k8s.io/pause:3.10
    resources:
      requests:
        cpu: {cpu}m
        memory: \"{mem}\"
  tolerations:
  - effect: NoSchedule
    key: kwok.x-k8s.io/node
    operator: Exists
",
            id = pod.id.0,
            cpu = pod.need.cpu_milli,
            mem = pod.need.mem_bytes,
            class = label(pod.class),
        )
        .unwrap();
    }
    w.iter_mut().for_each(|f| f.flush().unwrap());
}

/// One shard: its nodes (ids dense from 0 within it), its pending pods with the version each submit
/// returned, its slice of the store and its index.
struct Shard {
    nodes: Vec<NodeSpec>,
    pending: Vec<(WorkloadSpec, u64)>,
    store: Store,
    index: FitIndex,
}

fn shards_of(n: usize, model: NodeModel) -> Vec<Shard> {
    let mut parts: Vec<Vec<NodeSpec>> = vec![Vec::new(); n];
    for node in model.nodes(NODES) {
        let part = &mut parts[node.id.0 as usize % n];
        let local = NodeId(part.len() as u32);
        part.push(NodeSpec { id: local, ..node });
    }
    let mut work: Vec<Vec<WorkloadSpec>> = vec![Vec::new(); n];
    for w in pods() {
        work[w.id.0 as usize % n].push(w);
    }
    parts
        .into_iter()
        .zip(work)
        .map(|(nodes, work)| {
            let mut store = Store::new(&nodes, work.len(), work.len() * 2).unwrap();
            let pending = work
                .into_iter()
                .map(|w| match store.apply(0, Write::Submit(w)).unwrap() {
                    Outcome::Applied { version, .. } => (w, version),
                    other => panic!("submit of {:?}: {other:?}", w.id),
                })
                .collect();
            let index = FitIndex::new(&nodes).unwrap();
            Shard {
                nodes,
                pending,
                store,
                index,
            }
        })
        .collect()
}

struct Placed {
    shard: Shard,
    placed: u64,
    began: Instant,
    ended: Instant,
}

fn place(mut shard: Shard, pick: &Pick, start: &Barrier) -> Placed {
    start.wait();
    let began = Instant::now();
    let mut placed = 0;
    for (spec, version) in &shard.pending {
        let demand = spec.demand();
        let Some(best) = shard.index.fit(&demand, pick).best else {
            continue;
        };
        let write = Write::Place {
            workload: spec.id,
            node: best.node,
            version: *version,
        };
        match shard.store.apply(0, write).unwrap() {
            Outcome::Applied { .. } => {}
            other => panic!(
                "the store refused the index's choice for {:?}: {other:?}",
                spec.id
            ),
        }
        shard.index.take(best.node, spec.id, &demand).unwrap();
        placed += 1;
    }
    Placed {
        ended: Instant::now(),
        shard,
        placed,
        began,
    }
}

/// Every record placed on a node that runs its class, and no node above its capacity or its pod
/// limit.
fn check(p: &Placed) -> Result<(), String> {
    let mut used = vec![(Resources::default(), 0u64); p.shard.nodes.len()];
    for r in p.shard.store.records() {
        let State::Placed(node) = r.state else {
            return Err(format!("{:?} is {:?}", r.spec.id, r.state));
        };
        if !p.shard.nodes[node.0 as usize].classes.has(r.spec.class) {
            return Err(format!("{:?} of {:?} on {node:?}", r.spec.id, r.spec.class));
        }
        let u = &mut used[node.0 as usize];
        u.0 = u.0.checked_add(&r.spec.need).ok_or("overflow")?;
        u.1 += 1;
    }
    for (node, (u, pods)) in p.shard.nodes.iter().zip(&used) {
        if !node.capacity.holds(u) || *pods > max_pods(node) {
            return Err(format!("{:?} holds {u:?} in {pods} pods", node.id));
        }
    }
    Ok(())
}

fn hex(runs: u32, shard_counts: &[usize], model: NodeModel) {
    // Hex's packing score (H11, `Scorer::HEX`), against each class's one need.
    let pick = Pick {
        scorer: Scorer::HEX,
        shapes: Shapes::of(
            pods()
                .iter()
                .map(|w| (w.need, w.class))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
        .unwrap(),
    };
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    println!(
        "sched_kwok hex: {PODS} pending pods of {POD:?} on {NODES} nodes of {}, {cpus} CPUs, {runs} runs each",
        model.name()
    );
    for &n in shard_counts {
        let mut best: Option<Duration> = None;
        for run in 0..runs {
            let parts = shards_of(n, model);
            let start = Barrier::new(parts.len());
            let done: Vec<Placed> = std::thread::scope(|s| {
                let handles: Vec<_> = parts
                    .into_iter()
                    .map(|part| {
                        let (start, pick) = (&start, &pick);
                        s.spawn(move || place(part, pick, start))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let began = done.iter().map(|p| p.began).min().unwrap();
            let ended = done.iter().map(|p| p.ended).max().unwrap();
            let wall = ended.saturating_duration_since(began);
            let placed: u64 = done.iter().map(|p| p.placed).sum();
            let mut by_class = [0u64; 3];
            for r in done.iter().flat_map(|p| p.shard.store.records()) {
                if matches!(r.state, State::Placed(_)) {
                    by_class[r.spec.class as usize] += 1;
                }
            }
            let checked: Result<Vec<()>, String> = done.iter().map(check).collect();
            let ok = placed == PODS && checked.is_ok();
            println!(
                "run shards={n} run={run} placed={placed} placed_oci_firecracker_shards={by_class:?} wall_s={:.6} placements_per_s={:.0} check={}",
                wall.as_secs_f64(),
                placed as f64 / wall.as_secs_f64(),
                checked.map_or_else(|e| format!("FAILED {e}"), |_| "ok".into()),
            );
            if ok && best.is_none_or(|b| wall < b) {
                best = Some(wall);
            }
        }
        match best {
            Some(wall) => println!(
                "best shards={n} of {runs}: wall_s={:.6} placements_per_s={:.0}",
                wall.as_secs_f64(),
                PODS as f64 / wall.as_secs_f64()
            ),
            None => println!("best shards={n}: NONE (no run placed every pod within limits)"),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| {
            a.into_string()
                .unwrap_or_else(|a| panic!("an argument isn't Unicode: {a:?}"))
        })
        .collect();
    let usage = "usage: sched_kwok manifests <out-dir> <files> <m5d|fleet> | hex <runs> <shards,...> <m5d|fleet>";
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["manifests", out, files, m] => {
            manifests(Path::new(out), files.parse().expect(usage), model(m));
        }
        ["hex", runs, shards, m] => hex(
            runs.parse().expect(usage),
            &shards
                .split(',')
                .map(|s| s.parse().expect(usage))
                .collect::<Vec<_>>(),
            model(m),
        ),
        _ => panic!("{usage}"),
    }
}
