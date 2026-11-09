//! Placement with no engine in the middle (docs/reach.md §2; docs/needs.md §9, E5 and E9 as the
//! person's decisions of 2026-11-01 amend them, E-D7): `kiln run`, MCP's `run` and
//! `kiln.machines.run` are answered at once, started, queued or never, and a queued run waits as
//! probes at the runners that could take it until one admits it.
//!
//! - **The asking node owns its run** ([`owner`], Ray's ownership): it probes two runners drawn at
//!   random among those whose constraints the run meets (the power of two choices), takes the
//!   emptiest offer, declines the other, and sends the run's start to that one runner only. It goes back to waiting only on that runner's word
//!   that it never started it, never on its silence, so no run starts twice.
//! - **The runner that owns the room admits** ([`runner`], HTCondor's claiming): against its own
//!   measured room, never past its budget (G1), its line ordered by class and fair share
//!   ([`line`], O(log n) for every change and question), room kept for the first in line that
//!   doesn't fit (W05-R39), offers fenced by the runner's epoch and held for the link's measured
//!   retransmission timeout ([`link`]).
//! - **What they say** ([`msg`]): JSON inside the reach link's one placement variant each way.
//! - **What the asker is told** ([`answer`]): started (by the runner's word), queued (reasons
//!   counted across runners, its place, an expected start, the nearest ask that would start now),
//!   never (the need, and the nearest runner), and each change as news, never by polling.
//!
//! Pure, as the rest of kiln-ci: time comes in, messages and timers go out; every amount is an
//! integer (docs/needs.md E1), and every number is a measurement passed in, follows from one, comes
//! from cited research, or is a setting the person decides (docs/needs.md §21).
#![cfg_attr(not(test), deny(clippy::float_arithmetic))]

pub mod answer;
pub mod handoff;
pub mod ask;
pub mod budget;
pub mod keys;
pub mod line;
pub mod link;
pub mod msg;
pub mod owner;
pub mod runner;

#[cfg(test)]
mod balance_tests;
#[cfg(test)]
mod durable_tests;
#[cfg(test)]
mod fair_tests;
#[cfg(test)]
mod model;
#[cfg(test)]
mod owner_tests;
#[cfg(test)]
mod fast_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod weights_tests;
#[cfg(test)]
mod auction_tests;

/// A bug planted in placement for its model and property tests to catch (docs/sim.md): in tests
/// only, on one thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// The owner takes an offer that comes while its run is held elsewhere, and starts it there too.
    TakeTwo,
    /// A runner offers past its room: it counts twice the memory it has free.
    OfferPastRoom,
    /// The owner places a run again when the runner its start went to falls silent.
    ReplaceOnSilence,
    /// A runner starts a run under a claim that isn't live.
    StartLapsed,
    /// The owner takes a runner whose link is down for gone, and its runs for lost.
    LostOnDisconnect,
    /// The owner tells its asker the run started once its runner holds it, before it starts.
    StartedAtHeld,
    /// The owner takes a size not measured yet for too small, and answers never.
    NeverOnUnknown,
    /// A runner keeps its record by run alone, through its owner's epochs: a restarted owner's run
    /// that reuses an id reads as having run.
    ReusedIdRan,
    /// A runner tells a waiting run's place and reason once, never again.
    QuietPlace,
    /// The owner tells its asker the first answer, and no news after.
    QuietAnswer,
    /// A runner keeps no room for a run that takes it whole: small runs overtake it for ever.
    ExclusiveStarves,
    /// The owner never uses an older runner while any runner places.
    SkipOlder,
    /// The owner shows a runner's times as they are, in that runner's clock.
    SkewedClock,
    /// A restarted owner places a run again whose start went and whose outcome its runner can't
    /// vouch for, though the run didn't declare itself safe to repeat (LB03).
    StartAgainOnUnknown,
    /// The owner accepts a run's end from whichever runner says it, under whichever claim: a
    /// duplicate or stale end is taken as a second result (LB03).
    AcceptAnyEnd,
    /// A restarted runner doesn't hold the room of the runs whose processes went on past its
    /// restart (LB03).
    ForgetAdopted,
    /// The owner never moves what a runner last heard of a tenant's usage: each word says it all
    /// again (W05-R32).
    DoubleDelta,
    /// The owner never settles a run's usage to the time it ran: what ran without a median counts
    /// nothing elsewhere (W05-R32).
    NoSettle,
    /// A runner gives freed room by what its tenants got there alone: usage carried from elsewhere
    /// is dropped (W05-R32, Choosy's rule).
    LocalOnly,
    /// A runner orders a tenant's own runs by predicted time alone, as shortest predicted first
    /// does: a long one waits behind every shorter one that comes (W05-R09).
    Unbounded,
    /// Learned weights: a run's probes are drawn at random, whatever the weights (LB08).
    UnweightedProbes,
    /// Learned weights: the runs probed teach the weights nothing; Algorithm 1 never sees them
    /// (LB08).
    NoProbeLearn,
    /// Learned weights: a run's size is its CPUs alone, as if every run took as long (LB08).
    SizeByCpu,
    /// Learned weights: probes are still drawn by the weights after they fell back (LB08).
    KeepWeights,
    /// The auction: asks that come in one step are placed one at a time though it's declared
    /// (LB08).
    AuctionOneByOne,
    /// The auction: it decides where every assignment costs the same, and runs stack on one
    /// runner's slots (W05-R23).
    AuctionTies,
    /// The auction: what it couldn't start now isn't probed: asks lost (W05-R16).
    DropUnplaced,
    /// The auction: a runner's cost reads its speed alone, not its slowness under contention
    /// (W05-R26).
    SpeedAloneCost,
    /// A runner's measured pace never reaches its kernel's word: owners' kernels rank it, and the
    /// auction costs it, as the reference core (W05-R26).
    UnpacedKernel,
}

#[cfg(test)]
thread_local! {
    pub static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
}

/// The bug planted on this thread, if one is.
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

pub use answer::Answer;
pub use ask::{Ask, Peer, Sample};
pub use link::{Offset, Rtt};
pub use msg::{Down, Estimate, Place, Reason, Short, ToOwner, ToRunner, Up};
pub use owner::{Act, Attempt, Cmd, Kept, Link, Machine, Owner, Size};
pub use runner::{Admission, Init, Record, Records, Reply, Settings, Start};
