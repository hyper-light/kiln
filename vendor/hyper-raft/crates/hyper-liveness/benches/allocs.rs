//! What a heartbeat costs the allocator and the page tables once every pair is configured:
//! `cargo bench -p hyper-liveness --bench allocs` (docs/benchmarks.md, "hyper-liveness").
//!
//! `N` nodes in one process, every pair sharing a group, run on one simulated clock (`world.rs`):
//! each polls at its wake and sends the heartbeats due, the owner makes the liveness write asked,
//! and each heartbeat is decoded and judged on arrival. Counted per heartbeat sent and taken, over
//! ten seconds of simulated time after ten seconds of warm-up past the last pair's configuration;
//! the time of the crate's calls is the window's alone. The bootstrap, from the first poll to every
//! pair configured, is reported apart: its time a heartbeat, and how long it took.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    missing_docs
)]

use std::time::Duration;

use hyper_measure::{alloc, faults};

#[path = "support/world.rs"]
mod world;
use world::World;

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

fn point(nodes: usize, groups: u32) {
    let mut world = World::new(nodes, groups, 0x2545_F491_4F6C_DD1D);
    world.warm();
    let (took, boot_beats, boot_busy) = world.bootstrap.unwrap();
    world.sent = 0;
    world.taken = 0;
    world.busy = Duration::ZERO;
    let before = faults::read().unwrap();
    alloc::begin();
    world.run(Duration::from_secs(10));
    let counts = alloc::end();
    let after = faults::read().unwrap();
    let beats = (world.sent + world.taken) as f64;
    println!(
        "  {nodes:>5} {groups:>7} {:>10} {:>10} {:>9.4} {:>9.4} {:>9.1} {:>9.4} {:>9.0} {:>9.0} {:>9.1}",
        world.sent,
        world.taken,
        counts.allocations as f64 / beats,
        counts.reallocations as f64 / beats,
        counts.bytes as f64 / beats,
        after.since(&before).minor as f64 / beats,
        world.busy.as_nanos() as f64 / beats,
        boot_busy.as_nanos() as f64 / boot_beats.max(1) as f64,
        took.as_secs_f64(),
    );
}

fn main() {
    assert!(alloc::installed(), "the counting allocator is installed");
    println!(
        "hyper-liveness: allocations, reallocations, bytes asked and minor faults per heartbeat \
         sent or taken, and the ns its calls took, over 10 s simulated after warm-up; the \
         bootstrap's ns a heartbeat and its simulated seconds"
    );
    println!(
        "  {:>5} {:>7} {:>10} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "nodes",
        "groups",
        "sent",
        "taken",
        "allocs",
        "reallocs",
        "bytes",
        "faults",
        "ns",
        "boot ns",
        "boot s"
    );
    for (nodes, groups) in [(2usize, 1u32), (4, 1), (8, 1), (8, 1_000)] {
        point(nodes, groups);
    }
}
