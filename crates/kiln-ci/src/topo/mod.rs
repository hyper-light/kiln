//! The fleet's topology, for the load balancer (LB08; docs/reach.md §2.3): latency between any two
//! nodes, transfer rates, how steady links are and what fails together, all learned from traffic
//! that already flows. Nothing here probes, and nothing here keeps time: samples come in, with the
//! time they came, and estimates go out.
//!
//! - [`coord`]: network coordinates (Vivaldi with heights, Ledlie et al.'s latency filter and
//!   neighbor decay), each node its own, from the round trips its links measure (QUIC's own).
//! - [`rate`]: a pair's rate from kiln's own transfers (Hockney's `a + n/r`, He et al.'s recent
//!   history), bounded by its ends' rates where never seen.
//! - [`net`]: a runner's own network rate, from its interfaces' counters.
//! - [`stability`]: a link's drops and outages, from its connects and disconnects.
//! - [`domains`]: what fails together: facts (machine, subnet) and drops seen together.
//! - [`sites`]: nodes near one another: subnets and clusters in coordinate space.
//! - [`estimate`]: what the asking node's owner is given for each runner it might pick.
//!
//! Pure, as the rest of kiln-ci, and integer only ([`fixed`]): the same inputs give the same
//! answers on every machine. Every constant is a measurement passed in, follows from one, comes
//! from cited research, or is a setting the person decides (docs/needs.md §21).
#![cfg_attr(not(test), deny(clippy::float_arithmetic))]

pub mod coord;
pub mod domains;
pub mod estimate;
pub mod fixed;
pub mod net;
pub mod rate;
pub mod sites;
pub mod stability;

#[cfg(test)]
mod tests;

/// A mistake planted for the tests to catch, one per rule (docs/sim.md): in tests only, on one
/// thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// The error estimate never learns (`c_e = 0`): a node stays as unsure as it began.
    ErrorFrozen,
    /// No neighbor decay: a sample moves the node by its sender's force alone.
    OnlyLatest,
    /// Drift is never measured: an old coordinate is taken as sure as a fresh one.
    DriftIgnored,
    /// A level shift in a pair's transfers is never seen: the old rate lingers.
    NoLevelShift,
    /// A pair never seen ignores what its sites' measured pairs say.
    SiteIgnored,
    /// The stability term ignores drops.
    StallIgnoresDrops,
    /// Every co-drop joins two runners' domains, chance or not.
    EveryCoDropCounts,
    /// Sites are cut where the gap is within the estimates' own error.
    CutInsideNoise,
    /// A relayed link's latency ignores its measured legs.
    LegsIgnored,
}

#[cfg(test)]
thread_local! {
    pub static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
}

/// The mistake planted on this thread, if one is.
#[cfg(test)]
pub(crate) fn mutant() -> Option<Mutant> {
    MUTANT.with(std::cell::Cell::get)
}

/// Runs `f` with `m` planted on this thread, and takes it out after.
#[cfg(test)]
pub(crate) fn planted<T>(m: Option<Mutant>, f: impl FnOnce() -> T) -> T {
    MUTANT.with(|c| c.set(m));
    let out = f();
    MUTANT.with(|c| c.set(None));
    out
}

pub use coord::{Announce, Coord, Node};
pub use estimate::{End, Input, Latency, Leg, Link};
pub use rate::{Access, Fit, Rate, Transfer};
pub use stability::{History, Path};
