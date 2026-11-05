//! What owners and runners say to each other about runs (docs/reach.md §2, docs/needs.md §9 and
//! §13). Each message travels as JSON inside the reach link's one placement variant each way
//! (`reach::wire`'s `Ask::Place` and `Say::Place`), tagged by `op`: a field added later is
//! optional, and a message a peer doesn't know is one it ignores, so neither side breaks the link.
//! The link itself says whose words they are: the owner's node and its epoch ([`super::ask::Peer`]).

use serde::{Deserialize, Serialize};

use super::ask::{Ask, DiskFacts};
use crate::sched::state::{Fence, Resources};
use crate::sched::units::Ms;

/// An owner's word to a runner about one of its runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToRunner {
    /// Keep this run's place in your line, and offer it to me once you have room for it.
    Probe { ask: Box<Ask> },
    /// Forget it: it was placed elsewhere, ended, or is no longer wanted.
    Withdraw { run: String },
    /// I take your offer `fence`: hold its room for me until I start the run.
    Take { run: String, fence: Fence },
    /// I don't take your offer `fence`: its room is yours again.
    Decline { run: String, fence: Fence },
    /// Start the run, under claim `fence`.
    Start { run: String, fence: Fence },
    /// I heard how it ended (or that it ran here before): the record of it can go.
    Ack { run: String },
}

/// A runner's word to an owner about one of the owner's runs. Times are the runner's clock's:
/// the owner corrects them by the link's measured offset ([`super::owner::Link`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToOwner {
    /// Room for it now, held for you as claim `fence` until you take it or the offer lapses: what
    /// it gets, what was free as it was admitted, how long it's predicted to run here (the shell's
    /// prediction from history, this runner's disk among what it knows), and this runner's disk.
    Offer { run: String, fence: Fence, got: Resources, free: Resources, predicted: Option<Ms>, disk: DiskFacts },
    /// It waits here: its one reason, its place in this runner's line, when it would start here,
    /// how long it's predicted to run here, and what's free here as this was said.
    Waiting { run: String, reason: Reason, place: Place, start: Estimate, predicted: Option<Ms>, free: Resources },
    /// It can never run here as asked, and why ("not os == macos (os = linux)", "64 GiB of
    /// memory, under 128 GiB").
    Never { run: String, why: String },
    /// Your take of claim `fence` holds: its room is yours until you start the run or go away.
    Held { run: String, fence: Fence },
    /// Claim `fence` isn't yours any more (the offer wasn't taken in time, or this runner started
    /// again): the run wasn't started under it. It waits here again if you still want it.
    Lapsed { run: String, fence: Fence, why: String },
    /// Its process started here, under claim `fence`: the runner's word, and the only one on
    /// which its owner tells its asker that it started.
    Started { run: String, fence: Fence },
    /// It started here before (the runner keeps a record of the runs it starts until their owner
    /// hears how they ended): it isn't started again.
    Ran { run: String },
    /// It ended here, under claim `fence`: `ok` when its process exited, not when it was lost
    /// with the runner's process.
    Ended { run: String, fence: Fence, ok: bool },
}

impl ToOwner {
    /// The run it's about.
    pub fn run(&self) -> &str {
        match self {
            ToOwner::Offer { run, .. }
            | ToOwner::Waiting { run, .. }
            | ToOwner::Never { run, .. }
            | ToOwner::Held { run, .. }
            | ToOwner::Lapsed { run, .. }
            | ToOwner::Started { run, .. }
            | ToOwner::Ran { run }
            | ToOwner::Ended { run, .. } => run,
        }
    }
}

/// A run's place in a runner's line, to within a power of two: from `lo` to `hi`, both included
/// (`1`, `2..=3`, `4..=7`, ...). An exact place changes for every run behind one that starts,
/// so telling each would cost a message per waiting run per start; to within a power of two (the
/// finest logarithmic scale on whole numbers), a start or an arrival moves at most one run across
/// each boundary, so a change in line costs a message to O(log n) runs (docs/needs.md §11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Place {
    pub lo: u32,
    pub hi: u32,
}

impl Place {
    /// The range a place `n` (1 first) is told in; `0`, out of line, as `0..=0`.
    pub fn of(n: u32) -> Place {
        let Some(bits) = n.checked_ilog2() else { return Place::default() };
        let lo = 1u32 << bits;
        Place { lo, hi: lo.saturating_sub(1).saturating_add(lo) }
    }
}

/// When a run would start on a runner, in that runner's clock (unix milliseconds): the soonest
/// (the work of the runs ahead of it and of the claims there, each its amount times its predicted
/// time, done with each dimension's room shared out as a fluid: an estimate, since a run that fits
/// beside room kept, or ends before it, can start sooner), and the latest (each claim there
/// running to its timeout, then each run ahead running alone to its timeout: with nothing more
/// arriving, a bound nothing overruns). `None`: not known (a run there or ahead with no
/// prediction, or no timeout).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub soonest: Option<i64>,
    pub latest: Option<i64>,
}

/// Why a run waits on a runner: exactly one reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Reason {
    /// It's first in line and its room isn't free: each dimension it's short of, as it was said.
    Room { short: Vec<Short> },
    /// It's first in line and room is kept for it here from `at` (W05-R39, with `[reach]
    /// backfill`): runs behind it start only beside that room or if they end by then. `None`: no
    /// run holding room here has a predicted end, so only what fits beside it starts.
    Kept { at: Option<i64> },
    /// Runs ahead of it in line go first (its place says how many).
    Line,
    /// It takes the runner whole, and `jobs` other pieces of kiln's work run there.
    Whole { jobs: u32 },
    /// The runner is draining (an upgrade is pending): it takes nothing new.
    Draining,
    /// Its owner's link to the runner is down: it keeps its probe there, offered nothing until the
    /// owner is back.
    Away,
}

impl Reason {
    /// The reason as a news event counts it: its kind, and for room kept, from when. What's free
    /// moves with every run that starts or ends, and is told with the news, not as news.
    pub fn kind(&self) -> (u8, Option<i64>) {
        match self {
            Reason::Room { .. } => (0, None),
            Reason::Kept { at } => (1, *at),
            Reason::Line => (2, None),
            Reason::Whole { .. } => (3, None),
            Reason::Draining => (4, None),
            Reason::Away => (5, None),
        }
    }
}

/// A dimension a run is short of on a runner: `free` of it there, `ask` asked (milli-CPUs, bytes,
/// GPUs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Short {
    pub dim: Dim,
    pub free: u64,
    pub ask: u64,
}

/// The dimensions a run asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dim {
    Cpu,
    Mem,
    Disk,
    Gpu,
}

impl Dim {
    /// Each dimension's amount in `r`, in its unit.
    pub fn of(self, r: &Resources) -> u64 {
        match self {
            Dim::Cpu => r.cpu.get(),
            Dim::Mem => r.mem,
            Dim::Disk => r.disk,
            Dim::Gpu => u64::from(r.gpu),
        }
    }

    pub const ALL: [Dim; 4] = [Dim::Cpu, Dim::Mem, Dim::Disk, Dim::Gpu];
}

/// What `ask` is short of in `free`, dimension by dimension.
pub fn shortfall(ask: &Resources, free: &Resources) -> Vec<Short> {
    Dim::ALL.into_iter().filter(|d| d.of(ask) > d.of(free)).map(|dim| Short { dim, free: dim.of(free), ask: dim.of(ask) }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::state::Cpus;

    /// The messages travel as JSON tagged by `op`; one a peer doesn't know doesn't parse, and is
    /// ignored rather than breaking the link (the reach link's shell decides that).
    #[test]
    fn e5_messages_are_json_tagged_by_op() -> Result<(), serde_json::Error> {
        let take = ToRunner::Take { run: "r1".into(), fence: Fence { epoch: 3, n: 7 } };
        let s = serde_json::to_string(&take)?;
        assert_eq!(s, r#"{"op":"take","run":"r1","fence":{"epoch":3,"n":7}}"#);
        assert_eq!(serde_json::from_str::<ToRunner>(&s)?, take);
        let w = ToOwner::Waiting {
            run: "r1".into(),
            reason: Reason::Room { short: vec![Short { dim: Dim::Cpu, free: 4000, ask: 8000 }] },
            place: Place::of(2),
            start: Estimate { soonest: Some(10), latest: None },
            predicted: Some(60_000),
            free: Resources { cpu: Cpus::whole(4), ..Resources::default() },
        };
        assert_eq!(serde_json::from_str::<ToOwner>(&serde_json::to_string(&w)?)?, w);
        assert!(serde_json::from_str::<ToOwner>(r#"{"op":"a-newer-one","run":"r1"}"#).is_err());
        Ok(())
    }

    /// A place is told to within a power of two.
    #[test]
    fn e5_a_place_is_told_to_within_a_power_of_two() {
        let p = |n| {
            let Place { lo, hi } = Place::of(n);
            (lo, hi)
        };
        assert_eq!([p(0), p(1), p(2), p(3), p(4), p(7), p(8), p(1000)], [(0, 0), (1, 1), (2, 3), (2, 3), (4, 7), (4, 7), (8, 15), (512, 1023)]);
    }
}
