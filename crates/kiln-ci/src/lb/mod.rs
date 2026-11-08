//! The placement kernel (docs/lb.md): where a run goes, decided in O(1) by the node that owns the
//! run and admitted by the runner whose room it takes (docs/ci.md W05-R03, W05-R04, W05-R23,
//! W05-R26). The place core's owner picks through it and its runner keeps its ledger in it.
//!
//! - [`index`]: a signature's members bucketed by free whole CPUs, first in first out within a
//!   bucket, with a bitmap of the buckets that hold any (W05-R04's signatures and eligible sets).
//! - [`owner`]: the asking node's side: its view of each runner (that runner's last word less what
//!   the owner has sent it since), the pick (the least loaded member that fits at that view, every
//!   dimension checked exactly: W05-R23's "least loaded, then most free"), and a run's life until
//!   its runner says it started, waits or never can.
//! - [`runner`]: the runner's ledger: its free room against its budget, never past it (W05-R03),
//!   its epoch, its admitted runs and its line of waiting runs, whose first keeps its room.
//! - [`msg`]: what they say to each other, small `Copy` messages.
//! - [`slab`]: generational handles, so a word about a run that is gone finds nothing.
//!
//! Pure like the rest of kiln-ci: no clock, disk, network or randomness; integer amounts; nothing
//! hashed, and nothing allocated on a decision once the tables have grown to the fleet. Runners,
//! owners and signatures are dense ids the caller interns once (when a runner joins, an owner
//! links, a distinct set of constraints is first seen), never per decision.
#![cfg_attr(not(test), deny(clippy::float_arithmetic))]

pub mod index;
pub mod msg;
pub mod order;
pub mod owner;
pub mod runner;
pub mod slab;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use index::{BUCKETS, Index};
pub use msg::{Brief, Of, Start, ToOwner, ToRunner, Why, Word};
pub use order::{Order, class_of};
pub use owner::{Asked, Counts, Out, Owner, PLACES, Refused, SCALE, SPEED_ONE, State, Told};
pub use runner::{Runner, RunnerOut};
pub use slab::{Handle, Slab};

/// How many dimensions an amount has.
pub const DIMS: usize = 4;

/// Room or need: CPUs in thousandths, memory and disk in bytes, GPUs (the dimensions and units of
/// a run's needs, docs/runners.md). A need of CPU alone (`[cpu, 0, 0, 0]`) reads no other
/// dimension on either side.
pub type Amounts = [u64; DIMS];

/// One CPU in the unit amounts count CPUs in. It is the index's bucket width: the core is the
/// unit the hardware schedules, so a bucket's lower bound is an exact fit for any need of at most
/// that many whole CPUs.
pub const CPU: u64 = 1000;

/// A dense id or a list link that points nowhere.
pub(crate) const NIL: u32 = u32::MAX;

/// A runner, as its owners and the shell number it: dense, from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RunnerId(pub u32);

/// An owner (a node that asks for runs), numbered as runners are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OwnerId(pub u32);

/// A constraint signature at one owner: the runners that meet a set of constraints, interned when
/// the set is first seen (Borg's equivalence classes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SigId(pub u32);

/// A u64 amount as a view (saturating: views top out at 4,294,967 thousandths, 4,294 CPUs; a
/// bigger runner admits exactly, its owner just sees less of it).
#[inline(always)]
pub(crate) fn narrow(x: u64) -> u32 {
    u32::try_from(x).unwrap_or(u32::MAX)
}

/// Whether `need` asks anything but CPU.
#[inline]
pub(crate) fn multi(need: &Amounts) -> bool {
    let [_, m, d, g] = *need;
    (m | d | g) != 0
}

/// Memory, disk and GPUs of an amount.
#[inline]
pub(crate) fn rest(a: &Amounts) -> [u64; 3] {
    let [_, m, d, g] = *a;
    [m, d, g]
}

/// Whether `need` fits in `room`, memory, disk and GPUs alone.
#[inline]
pub(crate) fn rest_fits(need: &[u64; 3], room: &[u64; 3]) -> bool {
    need.iter().zip(room).all(|(n, r)| n <= r)
}

/// A bug planted in the kernel for its tests and model to catch (docs/lb.md "Correctness"): in
/// tests only, on one thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// A runner checks a start's CPU alone, not its memory, disk or GPUs: oversubscription.
    AdmitPastRoom,
    /// The owner takes a second runner's offer for a run another runner already holds for it,
    /// and starts it there too: a run started twice.
    GoTwice,
    /// The owner drops a run its runner declined for room without asking anywhere else or saying
    /// so: an ask lost.
    DropDeclined,
    /// A runner admits a start sent under an epoch that isn't its own: no fence.
    NoFence,
    /// Slabs match handles by slot alone, not generation: a stale word reaches a newer run.
    NoGeneration,
    /// A runner tells only the owner it is answering, not the other owners that watch it: views
    /// stay stale at quiescence.
    NoHint,
    /// The owner doesn't deduct what it sends at once: starts in flight herd onto one runner.
    NoDeduct,
    /// The pick breaks ties by a counter kept outside the owner: the same inputs, another choice.
    Unordered,
    /// A runner lets a start pass its line's first run in room that run needs: it starves.
    NoKeep,
    /// The index's cursor never moves: the same member of a bucket is picked every time, equals
    /// don't take turns.
    NoTurn,
    /// The pick takes the lowest bucket that fits, not the highest: the most loaded member.
    MostLoaded,
    /// Not a bug to catch: the switch the herding measurement turns, every owner's cursors
    /// starting in one place instead of spread over the fleet (tests.rs, docs/lb.md "Several
    /// owners").
    NoSpread,
}

#[cfg(test)]
thread_local! {
    static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
}

/// Whether `m` is the bug planted on this thread.
#[cfg(test)]
pub(crate) fn planted(m: Mutant) -> bool {
    MUTANT.with(|c| c.get() == Some(m))
}

/// Whether any bug is planted on this thread (fast paths step aside for the general ones, where
/// the bugs are).
#[cfg(test)]
pub(crate) fn any_planted() -> bool {
    MUTANT.with(|c| c.get().is_some())
}

/// Runs `f` with `m` planted on this thread, and takes it out after.
#[cfg(test)]
pub(crate) fn with_mutant<T>(m: Option<Mutant>, f: impl FnOnce() -> T) -> T {
    MUTANT.with(|c| c.set(m));
    let out = f();
    MUTANT.with(|c| c.set(None));
    out
}
