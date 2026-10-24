// Dependency-free bench: a plain `harness = false` binary, no criterion (the
// workspace's deny.toml forbids unmaintained/unvetted deps). A measurement
// tool, not a pass/fail test.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unreachable,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_precision_loss,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    unreachable_pub
)]
//! What one proposal costs a leader as its backlog grows: slates'
//! `a_proposal_costs_the_leader_the_same_at_any_backlog` (mantle note 32 §2.13, R21) on this core.
//! A leader of five whose followers acknowledge nothing appends `<entries>` proposals, each taken and
//! written out as an owner does, its messages dropped (`tests/support/backlog.rs`); each thousand is
//! timed, and its allocations counted in a second pass of the same proposals, the owner's storage
//! set aside. slates recorded 129 µs a proposal at a backlog of 1,000 and 20 ms at 5,000 before its
//! fix, and 61 to 102 ns after it at every backlog to 50,000 (slates `docs/wip/BENCHMARKS.md`, "A
//! leader's cost per proposal at a growing backlog").
//!
//! `backlog [<entries>]`, 50,000 by default; it prints, at each thousand, the backlog, the time a
//! proposal took and the allocations, reallocations and bytes a proposal made.
#[path = "../tests/support/mod.rs"]
mod support;

use std::time::Instant;

use hyper_measure::alloc::{self, Counting};
use support::backlog::Backlog;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Proposals timed and counted together.
const EVERY: u64 = 1_000;

fn main() {
    let entries: u64 = std::env::args()
        .skip(1)
        .find(|argument| !argument.starts_with('-'))
        .and_then(|entries| entries.parse().ok())
        .unwrap_or(50_000);
    // The time, in a pass that counts nothing.
    let mut timed = Backlog::new(5);
    let mut times = Vec::new();
    let mut at = 0;
    while at < entries {
        let started = Instant::now();
        for proposal in at..at + EVERY {
            timed.propose(proposal);
        }
        times.push((
            timed.backlog(),
            started.elapsed().as_nanos() as f64 / EVERY as f64,
        ));
        at += EVERY;
    }
    // The counts, in a pass of the same proposals.
    let mut counted = Backlog::new(5);
    let mut at = 0;
    println!("backlog, ns a proposal, allocations, reallocations, bytes a proposal");
    for (backlog, ns) in times {
        alloc::begin();
        for proposal in at..at + EVERY {
            counted.propose(proposal);
        }
        let counts = alloc::end().less(&alloc::read_aside());
        at += EVERY;
        println!(
            "{backlog}, {ns:.0}, {:.2}, {:.2}, {:.1}",
            counts.allocations as f64 / EVERY as f64,
            counts.reallocations as f64 / EVERY as f64,
            counts.bytes as f64 / EVERY as f64
        );
    }
}
