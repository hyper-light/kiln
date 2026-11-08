//! What owners and runners say to each other about runs (docs/reach.md §2, docs/needs.md §9 and
//! §13). Each message travels as JSON inside the reach link's one placement variant each way
//! (`reach::wire`'s `Ask::Place` and `Say::Place`), tagged by `op`: a field added later is
//! optional, and a message a peer doesn't know is one it ignores, so neither side breaks the link.
//! The link itself says whose words they are: the owner's node and its epoch ([`super::ask::Peer`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ask::{Ask, DiskFacts};
use crate::lb;
use crate::sched::state::{Fence, Resources};
use crate::sched::units::Ms;

/// An owner's word to a runner about one of its runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ToRunner {
    /// Keep this run's place in your line, and offer it to me once you have room for it. The ask is
    /// shared by every probe of the run, not copied (docs/needs.md §11, AUDIT P6); on the wire it's
    /// the ask itself, as it was.
    Probe {
        #[serde(with = "shared")]
        ask: Arc<Ask>,
    },
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
    /// Start it now if your room for a new run covers it, and say so with your room: the placement
    /// kernel's start (`crate::lb::ToRunner::Start`, docs/lb.md), decided at once from what you
    /// said of your room, with the spec it's for following. `kernel`: the owner's handle of it in
    /// its kernel; `epoch`: your kernel epoch it was decided under (a start under another is
    /// declined). Sent only on a link that named `place.fast` (W05-R03, W05-R23).
    Fast {
        #[serde(with = "shared")]
        ask: Arc<Ask>,
        kernel: FastHandle,
        epoch: u32,
    },
    /// Say where my runs stand here (W05-R25, LB03): I started again and kept what I own, or you
    /// did. Each run of mine you hold a record of, from any of my epochs, said again (`Started`
    /// while it runs, `Ended` once it has), then `Synced`. My earlier epochs' claims that hold no
    /// running run go. Appended: a runner from before ignores it, and is never sent it.
    Sync,
}

/// A shared ask, on the wire as the ask itself.
mod shared {
    use std::sync::Arc;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Ask;

    pub fn serialize<S: Serializer>(a: &Arc<Ask>, s: S) -> Result<S::Ok, S::Error> {
        a.as_ref().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<Ask>, D::Error> {
        Ask::deserialize(d).map(Arc::new)
    }
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
    /// The kernel's start ([`ToRunner::Fast`]) was admitted and its process started here (`at`:
    /// its handle in this runner's kernel ledger); `word`: the room left here for a new run.
    FastStarted { run: String, kernel: FastHandle, at: FastHandle, word: FastWord },
    /// The kernel's start wasn't taken, why, and the room here for a new run: its owner places it
    /// again by that room, or hands it to probing (docs/lb.md, "Off the fast path").
    FastDeclined { run: String, kernel: FastHandle, why: FastWhy, word: FastWord },
    /// A run the kernel started here ended (`ok`: its process exited), and the room left.
    FastEnded { run: String, kernel: FastHandle, ok: bool, word: FastWord },
    /// Every run of yours this runner holds a record of was said again since your `Sync` (W05-R25,
    /// LB03). Any other you had here it never started if its claim's epoch is `since` or later
    /// (the first of this runner's epochs whose records it keeps on its disk), and isn't known
    /// otherwise (`None`: it keeps none). `ended`: how many of those said again had ended.
    Synced { ended: u32, since: Option<u64> },
}

/// A handle in a placement kernel's slab (`crate::lb::Handle`) as messages carry it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastHandle {
    pub slot: u32,
    pub generation: u32,
}

impl FastHandle {
    pub fn of(h: lb::Handle) -> FastHandle {
        FastHandle { slot: h.slot, generation: h.generation }
    }

    pub fn handle(self) -> lb::Handle {
        lb::Handle { slot: self.slot, generation: self.generation }
    }
}

/// Why a runner's kernel didn't take a start (`crate::lb::Why`) as messages carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastWhy {
    Room,
    Epoch,
    Never,
    Gone,
}

impl FastWhy {
    pub fn of(w: lb::Why) -> FastWhy {
        match w {
            lb::Why::Room => FastWhy::Room,
            lb::Why::Epoch => FastWhy::Epoch,
            lb::Why::Never => FastWhy::Never,
            lb::Why::Gone => FastWhy::Gone,
        }
    }

    pub fn why(self) -> lb::Why {
        match self {
            FastWhy::Room => lb::Why::Room,
            FastWhy::Epoch => lb::Why::Epoch,
            FastWhy::Never => lb::Why::Never,
            FastWhy::Gone => lb::Why::Gone,
        }
    }
}

/// A runner's room as its kernel ledger says it (`crate::lb::Word`), and its whole budget: what an
/// owner's kernel picks by. In every answer to a kernel start and in the runner's hints (the
/// `offers` feature's `fast`), so an owner joins a runner and follows its room with no message
/// of its own. Fields added later are optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FastWord {
    /// The runner's kernel epoch: one per process, so a start decided under an older one is
    /// declined (`crate::lb::Why::Epoch`).
    pub epoch: u32,
    /// Room for a new run, in the kernel's amounts: CPUs in thousandths, memory and disk in bytes,
    /// GPUs. Nothing while runs wait in its line, it drains or is under strain: those go to
    /// probing, where its line's order decides.
    pub room: lb::Amounts,
    /// All of its memory, disk and GPUs are free for a new run.
    #[serde(default)]
    pub whole: bool,
    /// What it may ever admit: its budget, CPUs overcommitted as its setting says.
    pub budget: lb::Amounts,
    /// Measured per-core speed and slowness, in 1/1024 (0: not measured; docs/lb.md "Rank").
    #[serde(default)]
    pub speed: u32,
    #[serde(default)]
    pub slowness: u32,
    /// Its disk's effective rate under its load now and its own network's rate, bytes a second
    /// (0: not measured; docs/lb.md "Per-ask terms"), and how many times what it measured of
    /// itself changed.
    #[serde(default)]
    pub disk: u64,
    #[serde(default)]
    pub net: u64,
    #[serde(default)]
    pub measured: u32,
}

impl FastWord {
    /// The word of runner `w` with budget `budget`.
    pub fn of(w: &lb::Word, budget: lb::Amounts) -> FastWord {
        FastWord { epoch: w.epoch, room: w.room(), whole: w.whole, budget, speed: w.speed, slowness: w.slowness, disk: w.disk, net: w.net, measured: w.measured }
    }

    /// The kernel's word.
    pub fn word(&self) -> lb::Word {
        let [cpu, m, d, g] = self.room;
        lb::Word { epoch: self.epoch, whole: self.whole, cpu, rest: [m, d, g], speed: self.speed, slowness: self.slowness, disk: self.disk, net: self.net, measured: self.measured }
    }
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
            | ToOwner::Ended { run, .. }
            | ToOwner::FastStarted { run, .. }
            | ToOwner::FastDeclined { run, .. }
            | ToOwner::FastEnded { run, .. } => run,
            ToOwner::Synced { .. } => "",
        }
    }
}

/// An owner's word to a runner without its names, as the in-process path carries it (docs/needs.md
/// §11, AUDIT P6): the run by its shared id, its owner by the link it comes on. The wire carries
/// [`ToRunner`], made from it at the edge ([`Down::wire`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Down {
    /// `token`: the owner's own handle of the run, said back with every word about it ([`Up`]).
    Probe { ask: Arc<Ask>, token: u64 },
    Withdraw,
    Take { fence: Fence },
    Decline { fence: Fence },
    Start { fence: Fence },
    Ack,
    /// The kernel's start ([`ToRunner::Fast`]): `kernel`, the owner's handle in its kernel.
    Fast { ask: Arc<Ask>, token: u64, kernel: lb::Handle, epoch: u32 },
    /// [`ToRunner::Sync`]: about no run (its run is empty).
    Sync,
}

impl Down {
    /// The word on the wire, about `run`.
    pub fn wire(self, run: &str) -> ToRunner {
        let run = run.to_owned();
        match self {
            Down::Probe { ask, .. } => ToRunner::Probe { ask },
            Down::Withdraw => ToRunner::Withdraw { run },
            Down::Take { fence } => ToRunner::Take { run, fence },
            Down::Decline { fence } => ToRunner::Decline { run, fence },
            Down::Start { fence } => ToRunner::Start { run, fence },
            Down::Ack => ToRunner::Ack { run },
            Down::Fast { ask, kernel, epoch, .. } => ToRunner::Fast { ask, kernel: FastHandle::of(kernel), epoch },
            Down::Sync => ToRunner::Sync,
        }
    }
}

/// A runner's word to an owner without its names, as the in-process path carries it: the wire's
/// [`ToOwner`] less its run ([`Up::wire`], [`ToOwner::split`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Up {
    Offer { fence: Fence, got: Resources, free: Resources, predicted: Option<Ms>, disk: DiskFacts },
    Waiting { reason: Reason, place: Place, start: Estimate, predicted: Option<Ms>, free: Resources },
    Never { why: String },
    Held { fence: Fence },
    Lapsed { fence: Fence, why: &'static str },
    Started { fence: Fence },
    Ran,
    Ended { fence: Fence, ok: bool },
    FastStarted { kernel: lb::Handle, at: lb::Handle, word: FastWord },
    FastDeclined { kernel: lb::Handle, why: lb::Why, word: FastWord },
    FastEnded { kernel: lb::Handle, ok: bool, word: FastWord },
    /// [`ToOwner::Synced`]: about no run (its run is empty).
    Synced { ended: u32, since: Option<u64> },
}

impl Up {
    /// The word on the wire, about `run`.
    pub fn wire(self, run: &str) -> ToOwner {
        let run = run.to_owned();
        match self {
            Up::Offer { fence, got, free, predicted, disk } => ToOwner::Offer { run, fence, got, free, predicted, disk },
            Up::Waiting { reason, place, start, predicted, free } => ToOwner::Waiting { run, reason, place, start, predicted, free },
            Up::Never { why } => ToOwner::Never { run, why },
            Up::Held { fence } => ToOwner::Held { run, fence },
            Up::Lapsed { fence, why } => ToOwner::Lapsed { run, fence, why: why.to_owned() },
            Up::Started { fence } => ToOwner::Started { run, fence },
            Up::Ran => ToOwner::Ran { run },
            Up::Ended { fence, ok } => ToOwner::Ended { run, fence, ok },
            Up::FastStarted { kernel, at, word } => ToOwner::FastStarted { run, kernel: FastHandle::of(kernel), at: FastHandle::of(at), word },
            Up::FastDeclined { kernel, why, word } => ToOwner::FastDeclined { run, kernel: FastHandle::of(kernel), why: FastWhy::of(why), word },
            Up::FastEnded { kernel, ok, word } => ToOwner::FastEnded { run, kernel: FastHandle::of(kernel), ok, word },
            Up::Synced { ended, since } => ToOwner::Synced { ended, since },
        }
    }
}

impl ToOwner {
    /// Its run, and the word without it. A lapse's words aren't kept: the owner reads only that it
    /// lapsed.
    pub fn split(self) -> (String, Up) {
        match self {
            ToOwner::Offer { run, fence, got, free, predicted, disk } => (run, Up::Offer { fence, got, free, predicted, disk }),
            ToOwner::Waiting { run, reason, place, start, predicted, free } => (run, Up::Waiting { reason, place, start, predicted, free }),
            ToOwner::Never { run, why } => (run, Up::Never { why }),
            ToOwner::Held { run, fence } => (run, Up::Held { fence }),
            ToOwner::Lapsed { run, fence, .. } => (run, Up::Lapsed { fence, why: "lapsed" }),
            ToOwner::Started { run, fence } => (run, Up::Started { fence }),
            ToOwner::Ran { run } => (run, Up::Ran),
            ToOwner::Ended { run, fence, ok } => (run, Up::Ended { fence, ok }),
            ToOwner::FastStarted { run, kernel, at, word } => (run, Up::FastStarted { kernel: kernel.handle(), at: at.handle(), word }),
            ToOwner::FastDeclined { run, kernel, why, word } => (run, Up::FastDeclined { kernel: kernel.handle(), why: why.why(), word }),
            ToOwner::FastEnded { run, kernel, ok, word } => (run, Up::FastEnded { kernel: kernel.handle(), ok, word }),
            ToOwner::Synced { ended, since } => (String::new(), Up::Synced { ended, since }),
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
    /// The runner's machine is under strain as the gate's guard sees it (AUDIT L1: its tasks
    /// stalled on memory and I/O past the scheduler's reserve, `why`): it offers nothing new until
    /// a measure shows it isn't. Appended: an owner from before reads none of it.
    Strained { why: String },
    /// The runner's own health bars this kind of work here until its cause clears, by an event
    /// (AUDIT L2: its work volume ran full, a run was killed for memory, a tool wasn't found, its
    /// device's errors rose; `cause` as `sense::health` names it, `why` in words). Appended.
    Barred { cause: String, why: String },
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
            Reason::Strained { .. } => (6, None),
            Reason::Barred { .. } => (7, None),
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
