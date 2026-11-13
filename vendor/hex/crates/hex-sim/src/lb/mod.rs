//! The placement kernel, ported from kiln's load balancer (kiln `crates/kiln-ci/src/lb/` at
//! f20858c9c; `docs/design/hex-sim.md` "Ported from kiln's load balancer" gives every file's
//! provenance and what was adapted): where a box goes, decided by the node that owns the box and
//! admitted by the node whose room it takes. A box is one of the three kinds Hex runs, an OCI
//! container, a Firecracker microVM or a Shards microVM: it is created and started on its node,
//! runs, and holds its reserved room until it is deleted (or exits).
//!
//! - [`index`]: a signature's members bucketed by free whole CPUs, with a bitmap of the buckets
//!   that hold any (kiln's W05-R04 signatures and eligible sets).
//! - [`owner`]: the deciding side, a node's own placement or a plane's: its view of each node
//!   (that node's last word less what the owner has sent it since), the pick, and a box's life
//!   until its node says it started, waits or never can. Kiln picks the least loaded runner,
//!   spreading jobs that finish; a box lives until it is deleted, so the pick packs instead: the
//!   member with the least free room that holds the need.
//! - [`runner`]: the node's ledger: its free room against its budget, never past it, its epoch,
//!   its admitted boxes and its line of waiting boxes, whose first keeps its room.
//! - [`msg`]: what they say to each other, small `Copy` messages.
//! - [`slab`]: generational handles, so a word about a box that is gone finds nothing.
//!
//! Pure, as the rest of hex-sim: no clock, disk, network or randomness; integer amounts. Nodes,
//! owners and signatures are dense ids the caller interns once.

pub mod index;
pub mod msg;
pub mod owner;
pub mod runner;
pub mod slab;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use index::{BUCKETS, Index};
pub use msg::{Of, Start, ToOwner, ToRunner, Why, Word};
pub use owner::{Asked, Counts, Out, Owner, PLACES, Refused, SCALE, SPEED_ONE, State, Told};
pub use runner::{Kept, Runner, RunnerOut};
pub use slab::{Handle, Slab};

/// How many dimensions an amount has.
pub const DIMS: usize = 4;

/// Room or need, what a box reserves on its node: CPUs in thousandths, memory and disk in
/// bytes, GPUs. A need of CPU alone (`[cpu, 0, 0, 0]`) reads no other dimension on either side.
pub type Amounts = [u64; DIMS];

/// One CPU in the unit amounts count CPUs in: the index's bucket width. The core is the unit the
/// hardware schedules, so a bucket's lower bound is an exact fit for any need of at most that many
/// whole CPUs.
pub const CPU: u64 = 1000;

/// A dense id or a list link that points nowhere.
pub(crate) const NIL: u32 = u32::MAX;

/// A node as its owners number it (kiln's runner): dense, from 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RunnerId(pub u32);

/// An owner (a node's placement, or a plane, that asks for boxes), numbered as nodes are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OwnerId(pub u32);

/// A constraint signature at one owner: the nodes that can run a kind of box (its runtime class
/// and its image's OS and architecture), interned when first seen (Borg's equivalence classes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SigId(pub u32);

/// A u32 as an index: every u32 is one on the targets Hex builds for (CLAUDE.md §12), and one
/// that weren't would find nothing.
#[inline(always)]
pub(crate) fn ix(x: u32) -> usize {
    usize::try_from(x).unwrap_or(usize::MAX)
}

/// Whether `need` asks anything but CPU.
#[inline]
pub(crate) fn multi(need: &Amounts) -> bool {
    let [_, m, d, g] = *need;
    (m | d | g) != 0
}

/// CPU of an amount.
#[inline(always)]
pub(crate) fn cpu_of(a: &Amounts) -> u64 {
    let [c, ..] = *a;
    c
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

/// Whether `need` fits `budget` in every dimension.
#[inline]
pub(crate) fn within(need: &Amounts, budget: &Amounts) -> bool {
    need.iter().zip(budget).all(|(n, b)| n <= b)
}

/// A bug planted in the kernel for its tests and model to catch (kiln's, adapted to boxes: the
/// pick that spreads is the bug for boxes that pack). In tests only, on one thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// A node checks a start's CPU alone, not its memory, disk or GPUs: oversubscription.
    AdmitPastRoom,
    /// The owner takes a second node's offer for a box another node already holds for it, and
    /// starts it there too: a box started twice.
    GoTwice,
    /// The owner drops a box its node declined for room without asking anywhere else or saying
    /// so: an ask lost.
    DropDeclined,
    /// A node admits a start sent under an epoch that isn't its own: no fence.
    NoFence,
    /// Slabs match handles by slot alone, not generation: a stale word reaches a newer box.
    NoGeneration,
    /// A node tells only the owner it is answering, not the other owners that watch it: views
    /// stay stale at quiescence.
    NoHint,
    /// The owner doesn't deduct what it sends at once: starts in flight herd onto one node.
    NoDeduct,
    /// The pick breaks ties by a counter kept outside the owner: the same inputs, another choice.
    Unordered,
    /// A node lets a start pass its line's first box in room that box needs: it starves.
    NoKeep,
    /// The index's cursor never moves: the same member of a bucket is picked every time, equals
    /// don't take turns.
    NoTurn,
    /// The pick takes the member with the most room (kiln's least loaded, which spreads jobs that
    /// finish), not the least room that holds the need: boxes that live until deleted strand the
    /// room they spread.
    MostFree,
    /// A node that recovers forgets the boxes that outlived its agent: their room is free again
    /// beside them, and their owners take them for lost.
    RecoverForgets,
    /// Not a bug to catch: every owner's cursors start in one place instead of spread over the
    /// fleet.
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

/// Runs `f` with `m` planted on this thread, and takes it out after.
#[cfg(test)]
pub(crate) fn with_mutant<T>(m: Option<Mutant>, f: impl FnOnce() -> T) -> T {
    MUTANT.with(|c| c.set(m));
    let out = f();
    MUTANT.with(|c| c.set(None));
    out
}
