//! Hand-off of a run whose owner can't be reached, with no leader, no lease and no heartbeat
//! (LB03, W05-R25; docs/reach.md §2.4). The research behind it is the federation notes' §3
//! ("Leaderless hand-off of orphaned work, never twice"), novel synthesis N3.
//!
//! - **Who adopts.** The runner holding the news (a run of an owner it can't reach, with something
//!   to tell it) ranks the live nodes by rendezvous hashing on the run's handle ([`rank`]; Thaler and
//!   Ravishankar, "Using Name-Based Mappings to Increase Hit Rates", IEEE/ACM ToN 6(1), 1998): every
//!   node works out the same order alone, and only the gone owner's runs move. Views of who is live
//!   may differ, so two claimants are possible; safety never rests on agreeing who is alive.
//! - **Never twice.** Each run is single-decree Paxos (Lamport, "Paxos Made Simple", ACM SIGACT
//!   News 32(4), 2001) among the runners that hold it, its members ([`acceptor`]): one when it runs,
//!   two (its probes) while it waits. A claim is phase 1 under an integer ballot ([`Ballot`]: round,
//!   rendezvous rank, then name); the claimant owns the run once every member promised it
//!   ([`claim`]). A start is a phase-2 write at one runner, so phase 1 must reach every member:
//!   only phase-1 and phase-2 quorums need intersect, and they needn't be majorities (Howard,
//!   Malkhi and Spiegelman, "Flexible Paxos: Quorum Intersection Revisited", OPODIS 2016). The first
//!   owner speaks under [`Ballot::FIRST`], below every claim: once a member promised a claim, the
//!   first owner's later take, start or withdraw there is refused (kiln's epoch fence, per run).
//! - **Members only grow with the members' word** ([`members`]): an owner that would probe a run
//!   at runners past its members while one may hold it first has every such member take the new
//!   ones ([`Word::Extend`]); one that promised a claim refuses, and the owner stands down. So a
//!   claim's phase 1 always reaches every runner the run could start on.
//! - **Detection is unreliable, and that's fine** (Chandra and Toueg, "Unreliable Failure Detectors
//!   for Reliable Distributed Systems", JACM 43(2), 1996): a wrong suspicion costs one redundant
//!   claim, never a second start. The detector (`crate::detect`) suspects only on use.
//! - **Promises are kept on the member's disk** ([`acceptor::Row`]), as Paxos' acceptors keep
//!   theirs: a runner that starts again still refuses the first owner's word on a run it promised
//!   away, though its probes went with its process.
//!
//! What a hand-off can't do, by design: a run waiting at a runner that can't be reached can't be
//! adopted until that runner is reachable again (its promise is needed: it might start the run),
//! and for the same reason its owner can't move it to new runners meanwhile. A run handed to an
//! older runner that doesn't place can't be adopted at all (it can't promise).
//!
//! Pure, as the rest of kiln-ci: every number an integer, every input passed in.

use serde::{Deserialize, Serialize};

pub mod acceptor;
pub mod claim;
pub mod members;
pub mod msg;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use acceptor::{Acceptor, Gate, Held, Promise, Row};
pub use claim::{Claim, Decision};
pub use members::{Members, Released, Verdict};
pub use msg::Word;

/// What separates a handle's parts in the id an adopter owns its run by: no node name or run id
/// holds it.
const SEP: char = '\u{1}';

/// A run as its runners first knew it: its first owner's node, the owner's epoch the runners key
/// its words by, and its id. Every hand-off of the run names it so, however many times it moves.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Handle {
    pub owner: String,
    pub epoch: u64,
    pub run: String,
}

impl Handle {
    pub fn new(owner: &str, epoch: u64, run: &str) -> Handle {
        Handle { owner: owner.to_owned(), epoch, run: run.to_owned() }
    }

    /// The id an adopter owns the run by among its own: its handle's parts, apart.
    pub fn adopted_id(&self) -> String {
        format!("{}{SEP}{}{SEP}{}", self.owner, self.epoch, self.run)
    }

    /// The handle an adopter's id ([`Self::adopted_id`]) names, if it's one.
    pub fn of_adopted(id: &str) -> Option<Handle> {
        let mut parts = id.splitn(3, SEP);
        let (owner, epoch, run) = (parts.next()?, parts.next()?, parts.next()?);
        Some(Handle { owner: owner.to_owned(), epoch: epoch.parse().ok()?, run: run.to_owned() })
    }

    /// The handle of run `run` of owner `owner` in `epoch`, as a word about it names it: an adopted
    /// run's id names its first handle.
    pub fn of(owner: &str, epoch: u64, run: &str) -> Handle {
        Handle::of_adopted(run).unwrap_or_else(|| Handle::new(owner, epoch, run))
    }

    fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.owner.len().saturating_add(self.run.len()).saturating_add(10));
        b.extend_from_slice(self.owner.as_bytes());
        b.push(0);
        b.extend_from_slice(&self.epoch.to_be_bytes());
        b.extend_from_slice(self.run.as_bytes());
        b
    }
}

/// A claim's ballot, in a total order of integers: its round, then its claimant's rendezvous rank
/// for the run ([`rank`]), then the claimant's name (two ranks alike, one time in 2^64). The run's
/// first owner speaks under [`Ballot::FIRST`], below every claim. A claimant that finds a higher
/// ballot yields, and raises its round only once the one above it is itself gone, so claimants
/// never duel and no count is picked.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ballot {
    pub round: u64,
    pub rank: u64,
    pub by: String,
}

impl Ballot {
    /// The first owner's: round 0, below every claim.
    pub const FIRST: Ballot = Ballot { round: 0, rank: 0, by: String::new() };

    /// `by`'s ballot for `h` in `round`.
    pub fn of(round: u64, h: &Handle, by: &str) -> Ballot {
        Ballot { round, rank: rank(h, by), by: by.to_owned() }
    }

    /// `by`'s first claim of `h`, or its claim past `over` (whose claimant is gone): one round up.
    pub fn above(over: Option<&Ballot>, h: &Handle, by: &str) -> Ballot {
        Ballot::of(over.map_or(1, |b| b.round.saturating_add(1)), h, by)
    }
}

/// FNV-1a over `bytes` (Fowler, Noll and Vo; IETF draft-eastlake-fnv): the same on every platform.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3))
}

/// SplitMix64's finalizer (Steele, Lea and Flood, OOPSLA 2014), as placement's own draws mix.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Node `node`'s rendezvous weight for run `h` (Thaler and Ravishankar, 1998): the hash of the two
/// together, a fixed integer function, so every node ranks alike with no word between them.
pub fn rank(h: &Handle, node: &str) -> u64 {
    mix(fnv(&h.bytes()) ^ mix(fnv(node.as_bytes())))
}

/// `nodes` in the order they adopt `h`: the highest weight first, then by name.
pub fn ranked<'a>(h: &Handle, nodes: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut v: Vec<(u64, &str)> = nodes.into_iter().map(|n| (rank(h, n), n)).collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    v.dedup_by(|a, b| a.1 == b.1);
    v.into_iter().map(|(_, n)| n).collect()
}

/// A bug planted in the hand-off for its model and tests to catch (docs/sim.md): in tests only, on
/// one thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// A member promises a ballot not above the one it promised, and takes the first owner's word
    /// on a run it promised away.
    AcceptLowerBallot,
    /// A claimant decides with every member but one promised (phase 1 at one of two places).
    SkipPlace,
    /// A claimant acts on the news alone, before any member promised.
    AdoptWithoutPromise,
    /// An owner probes a run at new runners with no member's word (its members grow unseen).
    SkipExtend,
    /// A member's promises go with its process: a runner started again takes the first owner's word.
    ForgetPromise,
    /// The runner holding the news never hands the run off.
    NoHandOff,
}

#[cfg(test)]
thread_local! {
    static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
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
