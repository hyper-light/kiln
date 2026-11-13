//! hex-health: how Hex finds bad nodes and workloads and isolates them (`docs/design/failure-isolation.md`,
//! `scheduler-architecture.md` S8). A pure core (CLAUDE.md §9): no I/O, no clock, no threads. Callers
//! hand it observations stamped with their own time in nanoseconds; hex-sim drives it with a nemesis in
//! virtual time, and a node agent drives it with real probes, so both run the same code (§8).
//!
//! - [`probe`]: a node's own disk probe, a stall judged by its age against an RFC 6298 timeout
//!   (`AC-T7.10.3`).
//! - [`grade`]: requester observations compared across peers, the gray and fail-slow grade.
//! - [`penalty`]: ejection multiplied by count and capped per cell, the mass-loss eviction rate and the
//!   crash-loop backoff with its forgiveness.
//! - [`retry`]: the retry budget.
//! - [`antagonist`]: CPI²-style victims, their antagonist found by correlation, and its capped throttle.
//! - [`agent`]: stuck tool loops, and provider errors told apart as quota, outage or a node's own path.
//! - [`danger`]: which runtime security signals trip (where the workload goes is hex-sim's `danger`, by
//!   the kinds Hex places).

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

pub mod agent;
pub mod antagonist;
pub mod danger;
pub mod error;
pub mod grade;
pub mod histogram;
pub mod penalty;
pub mod probe;
pub mod retry;

pub use error::HealthError;

/// One graded verdict on a node, which each consumer thresholds (φ-accrual's split, PF[25]): the
/// scheduler stops placing new work at `Suspect` and moves work off at `Drain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Healthy,
    /// No new placements.
    Suspect,
    /// Move its work off.
    Drain,
}
