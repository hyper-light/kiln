//! `AC-T7.11.4`: control-plane memory per workload, measured by a counting global allocator
//! (`docs/design/hex-sim.md` H62). `cap` wraps the system allocator and counts the bytes live on the
//! heap, as the callers' layouts ask for them (not the allocator's own per-chunk overhead). A run's
//! store and every shard's plane (eight shards, H63's sharded row) are dropped, and what the count
//! falls by is the heap they held.
//!
//! What's counted, for a placed workload: the store's record (its spec, queue position, version and
//! state) in the records map with the map's own nodes, its two log entries (submit and placement;
//! a third, its completion, comes when it ends), and every plane's version entry for it. Beside those,
//! amortised over the workloads, each plane's fit index over the nodes, its change window and its
//! queues, and the store's per-node capacity and use. The store's log is Amphora's in production, so
//! counting it here is conservative. The clients' and node agents' state (`Run::starts`, `acked`,
//! `ran`, ...) and the run's input (`Cluster`) are not the control plane's and are not counted.
//!
//! This binary holds one test, so nothing else allocates while it measures.

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

use std::alloc::System;

use cap::Cap;
use hex_sim::fair::Policy;
use hex_sim::generate::{self, NodeModel};
use hex_sim::model::{Class, Cluster, Kind, Resources, Tenancy, WorkloadId, WorkloadSpec};
use hex_sim::sim::{self, Config, Run};
use hex_sim::spec;
use hex_sim::store::{State, Store};

#[global_allocator]
static ALLOC: Cap<System> = Cap::new(System, usize::MAX);

/// `AC-T7.11.4`'s bar: at most 4 KiB of control-plane memory per workload.
const BAR_BYTES: usize = 4096;

const NODES: u32 = 10_000;
const WORKLOADS: u64 = 1_000_000;
const SHARDS: u32 = 8;
const MIB: u64 = 1 << 20;

/// H63's pods on G2's main node model, in G2's classes (H69): 10,000 EC2 m5d.metal nodes (48 cores,
/// 384 GiB, every class), and 1,000,000 of `scheduler_perf`'s pause pod (100m CPU, 500Mi), each an OCI
/// container, a Firecracker or a Shards microVM by owner, a third each, with its class's overhead in
/// its memory. All are submitted at once and none ends. Not run on the fleet model: there, a million
/// pods don't fit (its Linux and Windows hosts hold about 332 TB, and the OCI and Firecracker thirds
/// ask about 360 TB), and the count is of placed workloads.
fn h13() -> Cluster {
    let nodes = NodeModel::M5dMetal.nodes(NODES);
    let workloads = (0..WORKLOADS)
        .map(|id| WorkloadSpec {
            id: WorkloadId(id),
            need: Resources {
                cpu_milli: 100,
                mem_bytes: 500 * MIB,
                gpus: 0,
                disk_bytes: 0,
            },
            kind: Kind::Plain,
            arrives: 0,
            runs_for: None,
            tenancy: Tenancy::default(),
            class: Class::Oci,
            ask: Default::default(),
        })
        .collect();
    let mut cluster = Cluster::single_owner(nodes, workloads);
    generate::owners(&mut cluster, 7, 75).unwrap();
    generate::by_owner(&mut cluster, generate::overheads()).unwrap();
    cluster
}

/// H62's non-vacuity for the counter itself: do allocate a buffer of known size and free it; expect
/// the count to rise by exactly its bytes and fall back.
fn counter_counts() {
    let before = ALLOC.allocated();
    let buffer = vec![0u8; 1 << 20];
    assert_eq!(ALLOC.allocated() - before, 1 << 20);
    drop(buffer);
    assert_eq!(ALLOC.allocated(), before);
}

/// AC-T7.11.4: do run 1,000,000 workloads to all placed on 10,000 nodes under 8 shards, then drop the
/// store and every shard's plane; expect the heap they held, with their inline sizes, to be at most
/// 4096 bytes a workload. Non-vacuity: the counter moves by what's allocated, every workload is placed
/// and held by the store and by a plane when it reads, every slice's log is one the serial
/// specification makes, and the store's count is at least its records' layouts.
#[test]
fn a_million_placed_workloads_hold_at_most_4_kib_each() {
    counter_counts();
    let cluster = h13();
    let config = Config {
        shards: SHARDS,
        workers: 16,
        // Submit order: G6's bar is of record under it, the only order when it was measured
        // (`docs/design/hex-sim.md`, "What it doesn't do yet").
        policy: Policy::Fifo,
        ..Config::default()
    };
    let run = sim::run(&cluster, &config).unwrap();

    let workloads = usize::try_from(WORKLOADS).unwrap();
    let placed = run
        .store
        .records()
        .filter(|r| matches!(r.state, State::Placed(_)))
        .count();
    assert_eq!(placed, workloads);
    let end = spec::check_end(&cluster, &run);
    assert_eq!(end.lost_or_double(), 0, "{end:?}");
    assert_eq!(
        spec::check_slices(&cluster, &run).unwrap().len(),
        usize::try_from(SHARDS).unwrap()
    );
    assert_eq!(run.planes.len(), usize::try_from(SHARDS).unwrap());
    assert!(run.planes.iter().all(|p| p.pending() == 0));
    let held_by_planes: usize = run.planes.iter().map(|p| p.workloads()).sum();
    assert_eq!(held_by_planes, workloads);

    let Run { store, planes, .. } = run;
    let inline = size_of_val(&store) + size_of_val(&planes);
    let held = ALLOC.allocated();
    drop(store);
    let after_store = ALLOC.allocated();
    drop(planes);
    let after_planes = ALLOC.allocated();
    let store_bytes = held - after_store;
    let plane_bytes = after_store - after_planes;
    let total = store_bytes + plane_bytes + inline;
    eprintln!(
        "AC-T7.11.4: store {store_bytes} B, planes {plane_bytes} B, inline {inline} B; \
         {} B per workload over {WORKLOADS} on {NODES} EC2 m5d.metal nodes, {SHARDS} shards (bar {BAR_BYTES})",
        total / workloads
    );

    assert!(
        store_bytes >= Store::record_bytes() * placed,
        "the count missed the store's records: {store_bytes} B"
    );
    assert!(
        plane_bytes >= size_of::<(WorkloadId, u64)>() * placed,
        "the count missed the planes' versions: {plane_bytes} B"
    );
    assert!(
        total <= BAR_BYTES * workloads,
        "{total} B over {WORKLOADS} workloads, over {BAR_BYTES} each"
    );
}
