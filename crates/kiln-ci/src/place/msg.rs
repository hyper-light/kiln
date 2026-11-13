//! What owners and runners say to each other about runs (docs/reach.md §2, docs/needs.md §9 and
//! §13). Each message travels as JSON inside the reach link's one placement variant each way
//! (`reach::wire`'s `Ask::Place` and `Say::Place`), tagged by `op`: a field added later is
//! optional, and a message a peer doesn't know is one it ignores, so neither side breaks the link.
//! The link itself says whose words they are: the owner's node and its epoch ([`super::ask::Peer`]).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::ask::{Ask, DiskFacts};
use super::keys::{FxMap, Interner};
use super::owner::Size;
use crate::lb;
use crate::pace::Pace;
use crate::sched::state::{Cpus, Fence, Resources};
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
        /// Its tenant's usage elsewhere since my last word to you about it ([`Usage`], W05-R32).
        /// Appended: optional, 0 from an owner before it.
        #[serde(default, skip_serializing_if = "is_zero")]
        elsewhere: i64,
    },
    /// Forget it: it was placed elsewhere, ended, or is no longer wanted.
    Withdraw { run: String },
    /// I take your offer `fence`: hold its room for me until I start the run.
    Take { run: String, fence: Fence },
    /// I don't take your offer `fence`: its room is yours again.
    Decline { run: String, fence: Fence },
    /// Start the run, under claim `fence`; its tenant's usage elsewhere as with a probe. Appended:
    /// `elsewhere` optional, 0 from an owner before it.
    Start {
        run: String,
        fence: Fence,
        #[serde(default, skip_serializing_if = "is_zero")]
        elsewhere: i64,
    },
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
        /// Its tenant's usage elsewhere, as a claim's start carries it (W05-R32). Optional: 0 is not
        /// said.
        #[serde(default, skip_serializing_if = "is_zero")]
        elsewhere: i64,
    },
    /// Say where my runs stand here (W05-R25, LB03): I started again and kept what I own, or you
    /// did. Each run of mine you hold a record of, from any of my epochs, said again (`Started`
    /// while it runs, `Ended` once it has), then `Synced`. My earlier epochs' claims that hold no
    /// running run go. Appended: a runner from before ignores it, and is never sent it.
    Sync,
    /// Under room tokens (docs/lb.md "Room tokens"; LB08, W05-R03): I can't use `amount` of what I
    /// hold here, your kernel epoch `epoch`'s (your word said another owner's run waited here for
    /// room, and none of mine waits for you): forward it once, to the owner whose run last waited
    /// here (`crate::lb::ToRunner::Release`). About no run. Appended: sent only on a link where the
    /// runner named `place.tokens`, in answer to its word's `wants`.
    FastRelease { epoch: u32, amount: lb::Amounts },
}

/// How a run ended on its runner, as that runner's shell judges it when the run ends (docs/lb.md
/// "Good places"): what its owner's kernel is told of the assignment. The shell has what the
/// judgment needs (the run's exit, what it printed, what its OS counted while it ran:
/// `sense::health`); the kernel only remembers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// Nothing said: a runner from before this, a run said again after its link came back, or
    /// one its shell doesn't judge.
    #[default]
    Unsaid,
    /// It ran to its end and succeeded.
    Well,
    /// The node let it down: it couldn't start here, was lost, or ended of a cause that is the
    /// machine's (no space, killed for memory, a tool that isn't here).
    LetDown,
    /// It failed and its shell knows no cause: the node's fault or the work's own, settled by
    /// where its kind's next run goes and what becomes of it (`lb::Owner::unsure`).
    Unsure,
}

impl How {
    pub fn is_unsaid(&self) -> bool {
        *self == How::Unsaid
    }
}

fn is_zero(x: &i64) -> bool {
    *x == 0
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
    /// Its pace (W05-R26): appended, so an owner from before reads none of it, and none is said
    /// while nothing is measured.
    Offer {
        run: String,
        fence: Fence,
        got: Resources,
        free: Resources,
        predicted: Option<Ms>,
        disk: DiskFacts,
        #[serde(default, skip_serializing_if = "Pace::is_unmeasured")]
        pace: Pace,
    },
    /// It waits here: its one reason, its place in this runner's line, when it would start here,
    /// how long it's predicted to run here, what's free here as this was said, and its pace.
    Waiting {
        run: String,
        reason: Reason,
        place: Place,
        start: Estimate,
        predicted: Option<Ms>,
        free: Resources,
        #[serde(default, skip_serializing_if = "Pace::is_unmeasured")]
        pace: Pace,
    },
    /// It can never run here as asked, and why ("not os == macos (os = linux)", "64 GiB of
    /// memory, under 128 GiB").
    Never { run: String, why: String },
    /// Your take of claim `fence` holds: its room is yours until you start the run or go away.
    Held { run: String, fence: Fence },
    /// Claim `fence` isn't yours any more (the offer wasn't taken in time, this runner started
    /// again, or its machine's capacity keeper isn't synced, so no reserve for it can be said, or
    /// didn't grant its reserve): the run wasn't started under it. It waits here again if you still
    /// want it.
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
    /// A run the kernel started here ended (`ok`: its process exited), and the room left. `how`:
    /// its shell's judgment of how it ended. Appended: optional, nothing said by a runner from
    /// before it.
    FastEnded {
        run: String,
        kernel: FastHandle,
        ok: bool,
        word: FastWord,
        #[serde(default, skip_serializing_if = "How::is_unsaid")]
        how: How,
    },
    /// Every run of yours this runner holds a record of was said again since your `Sync` (W05-R25,
    /// LB03). Any other you had here it never started if its claim's epoch is `since` or later
    /// (the first of this runner's epochs whose records it keeps on its disk), and isn't known
    /// otherwise (`None`: it keeps none). `ended`: how many of those said again had ended.
    Synced { ended: u32, since: Option<u64> },
    /// Under room tokens (docs/lb.md "Room tokens"; LB08, W05-R03): this runner's word to you alone,
    /// what you hold here now (`crate::lb::ToOwner::Hint` as its kernel owes it: what you held cut
    /// back as another took the room under it, a token forwarded to you, room come back, a word
    /// that another owner's run waits here). Your kernel hears it as it hears hints. About no run.
    /// Appended: said only on a link that named `place.tokens`.
    FastHint { word: FastWord },
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
    /// Under room tokens (`crate::lb::tokens`): another owner's run waited here for room since this
    /// owner last heard (`crate::lb::Word::wants`). Optional: false is not said.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wants: bool,
    /// Under room tokens (docs/lb.md "Room tokens"; LB08): this word is to its owner alone, what it
    /// holds here (`crate::lb::Runner::word_for`), and this runner says that owner its words on
    /// placement's channel ([`ToOwner::FastHint`]): of this kernel epoch the owner takes no word
    /// of its hints' (the room every owner hears). Optional: false is not said, and a word from a
    /// runner before it is one every owner hears.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tokens: bool,
}

impl FastWord {
    /// The word of runner `w` with budget `budget`, as every owner hears it.
    pub fn of(w: &lb::Word, budget: lb::Amounts) -> FastWord {
        FastWord { epoch: w.epoch, room: w.room(), whole: w.whole, budget, speed: w.speed, slowness: w.slowness, disk: w.disk, net: w.net, measured: w.measured, wants: w.wants, tokens: false }
    }

    /// The kernel's word.
    pub fn word(&self) -> lb::Word {
        let [cpu, m, d, g] = self.room;
        lb::Word { epoch: self.epoch, whole: self.whole, cpu, rest: [m, d, g], speed: self.speed, slowness: self.slowness, disk: self.disk, net: self.net, measured: self.measured, wants: self.wants }
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
            ToOwner::Synced { .. } | ToOwner::FastHint { .. } => "",
        }
    }
}

/// An owner's word to a runner without its names, as the in-process path carries it (docs/needs.md
/// §11, AUDIT P6): the run by its shared id, its owner by the link it comes on. The wire carries
/// [`ToRunner`], made from it at the edge ([`Down::wire`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Down {
    /// `token`: the owner's own handle of the run, said back with every word about it ([`Up`]);
    /// `elsewhere`: its tenant's usage elsewhere ([`Usage`]).
    Probe { ask: Arc<Ask>, token: u64, elsewhere: i64 },
    Withdraw,
    Take { fence: Fence },
    Decline { fence: Fence },
    Start { fence: Fence, elsewhere: i64 },
    Ack,
    /// The kernel's start ([`ToRunner::Fast`]): `kernel`, the owner's handle in its kernel;
    /// `elsewhere` as with a start.
    Fast { ask: Arc<Ask>, token: u64, kernel: lb::Handle, epoch: u32, elsewhere: i64 },
    /// [`ToRunner::Sync`]: about no run (its run is empty).
    Sync,
    /// [`ToRunner::FastRelease`]: about no run (its run is empty).
    FastRelease { epoch: u32, amount: lb::Amounts },
}

impl Down {
    /// The word on the wire, about `run`.
    pub fn wire(self, run: &str) -> ToRunner {
        let run = run.to_owned();
        match self {
            Down::Probe { ask, elsewhere, .. } => ToRunner::Probe { ask, elsewhere },
            Down::Withdraw => ToRunner::Withdraw { run },
            Down::Take { fence } => ToRunner::Take { run, fence },
            Down::Decline { fence } => ToRunner::Decline { run, fence },
            Down::Start { fence, elsewhere } => ToRunner::Start { run, fence, elsewhere },
            Down::Ack => ToRunner::Ack { run },
            Down::Fast { ask, kernel, epoch, elsewhere, .. } => ToRunner::Fast { ask, kernel: FastHandle::of(kernel), epoch, elsewhere },
            Down::Sync => ToRunner::Sync,
            Down::FastRelease { epoch, amount } => ToRunner::FastRelease { epoch, amount },
        }
    }
}

/// A runner's word to an owner without its names, as the in-process path carries it: the wire's
/// [`ToOwner`] less its run ([`Up::wire`], [`ToOwner::split`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Up {
    Offer { fence: Fence, got: Resources, free: Resources, predicted: Option<Ms>, disk: DiskFacts, pace: Pace },
    Waiting { reason: Reason, place: Place, start: Estimate, predicted: Option<Ms>, free: Resources, pace: Pace },
    Never { why: String },
    Held { fence: Fence },
    Lapsed { fence: Fence, why: &'static str },
    Started { fence: Fence },
    Ran,
    Ended { fence: Fence, ok: bool },
    FastStarted { kernel: lb::Handle, at: lb::Handle, word: FastWord },
    FastDeclined { kernel: lb::Handle, why: lb::Why, word: FastWord },
    FastEnded { kernel: lb::Handle, ok: bool, word: FastWord, how: How },
    /// [`ToOwner::Synced`]: about no run (its run is empty).
    Synced { ended: u32, since: Option<u64> },
    /// [`ToOwner::FastHint`]: about no run (its run is empty).
    FastHint { word: FastWord },
}

impl Up {
    /// The word on the wire, about `run`.
    pub fn wire(self, run: &str) -> ToOwner {
        let run = run.to_owned();
        match self {
            Up::Offer { fence, got, free, predicted, disk, pace } => ToOwner::Offer { run, fence, got, free, predicted, disk, pace },
            Up::Waiting { reason, place, start, predicted, free, pace } => ToOwner::Waiting { run, reason, place, start, predicted, free, pace },
            Up::Never { why } => ToOwner::Never { run, why },
            Up::Held { fence } => ToOwner::Held { run, fence },
            Up::Lapsed { fence, why } => ToOwner::Lapsed { run, fence, why: why.to_owned() },
            Up::Started { fence } => ToOwner::Started { run, fence },
            Up::Ran => ToOwner::Ran { run },
            Up::Ended { fence, ok } => ToOwner::Ended { run, fence, ok },
            Up::FastStarted { kernel, at, word } => ToOwner::FastStarted { run, kernel: FastHandle::of(kernel), at: FastHandle::of(at), word },
            Up::FastDeclined { kernel, why, word } => ToOwner::FastDeclined { run, kernel: FastHandle::of(kernel), why: FastWhy::of(why), word },
            Up::FastEnded { kernel, ok, word, how } => ToOwner::FastEnded { run, kernel: FastHandle::of(kernel), ok, word, how },
            Up::Synced { ended, since } => ToOwner::Synced { ended, since },
            Up::FastHint { word } => ToOwner::FastHint { word },
        }
    }
}

impl ToOwner {
    /// Its run, and the word without it. A lapse's words aren't kept: the owner reads only that it
    /// lapsed.
    pub fn split(self) -> (String, Up) {
        match self {
            ToOwner::Offer { run, fence, got, free, predicted, disk, pace } => (run, Up::Offer { fence, got, free, predicted, disk, pace }),
            ToOwner::Waiting { run, reason, place, start, predicted, free, pace } => (run, Up::Waiting { reason, place, start, predicted, free, pace }),
            ToOwner::Never { run, why } => (run, Up::Never { why }),
            ToOwner::Held { run, fence } => (run, Up::Held { fence }),
            ToOwner::Lapsed { run, fence, .. } => (run, Up::Lapsed { fence, why: "lapsed" }),
            ToOwner::Started { run, fence } => (run, Up::Started { fence }),
            ToOwner::Ran { run } => (run, Up::Ran),
            ToOwner::Ended { run, fence, ok } => (run, Up::Ended { fence, ok }),
            ToOwner::FastStarted { run, kernel, at, word } => (run, Up::FastStarted { kernel: kernel.handle(), at: at.handle(), word }),
            ToOwner::FastDeclined { run, kernel, why, word } => (run, Up::FastDeclined { kernel: kernel.handle(), why: why.why(), word }),
            ToOwner::FastEnded { run, kernel, ok, word, how } => (run, Up::FastEnded { kernel: kernel.handle(), ok, word, how }),
            ToOwner::Synced { ended, since } => (String::new(), Up::Synced { ended, since }),
            ToOwner::FastHint { word } => (String::new(), Up::FastHint { word }),
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
    /// The runner's machine has a capacity keeper (Hex's node keeps its room, kiln's runs hold room
    /// there through it: docs/lb.md "A machine's keeper") and the runner holds no synced session
    /// with it (`why`): it takes nothing new there until it does (Hex's capacity-view.md CV11).
    /// Appended: an owner from before reads it as `Strained` where it reads health's words, else as
    /// `Draining`.
    Unsynced { why: String },
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
            Reason::Unsynced { .. } => (8, None),
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

/// Fair share's dimensions as an owner counts its tenants' usage (W05-R33): a slot, milli-CPUs,
/// memory bytes and GPUs, each times milliseconds.
const FAIR: usize = crate::sched::state::DIMS;

/// What an owner's probes and starts carry (W05-R32, docs/needs.md §5): each tenant's usage
/// elsewhere, one integer on words it says anyway, so that a runner ranks tenants by what they get
/// across the fleet with no word of its own and no center (DSFQ's delay: Wang and Merchant,
/// "Proportional-Share Scheduling for Distributed Storage Systems", FAST 2007; dmClock's δ: Gulati,
/// Merchant and Varman, OSDI 2010).
///
/// - **What it counts**: each tenant's runs this owner placed (by class and leaf: repository, actor,
///   lane), in each of fair share's dimensions times milliseconds: charged as its runner says it
///   started, for its median (none: nothing yet, as a runner charges it), and settled to the time
///   it ran as it ends (DRFQ), the signed difference counted then. A run that takes its runner whole
///   counts that runner as the directory has it.
/// - **What a word carries**: what the tenant was charged since this owner's last word about it to
///   that runner, less what ran there (that runner charges it itself), in milliseconds of runs like
///   the one the word is about: the largest ratio over the dimensions that run asks (PS-DSF's
///   per-server count of a user's tasks, Khamse-Ashari et al., IEEE ICC 2017; the runner turns it
///   into its own dominant share). Runs of a tenant that differ only in size count exactly; runs
///   that differ in shape, at most as much as their largest ratio.
/// - **What it keeps**: per tenant its total and its total as each runner last heard it, from the
///   tenant's first run here until its last is gone: a tenant idle here comes back with no credit
///   or debt, as start-time fair queuing has it, and the first word to a runner carries nothing.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    names: Interner,
    buf: String,
    tenants: Vec<Tenant>,
    runs: FxMap<Arc<str>, Charged>,
}

#[derive(Debug, Clone, Default)]
struct Tenant {
    charged: [i128; FAIR],
    live: u32,
    said: FxMap<u32, [i128; FAIR]>,
}

/// A run of this owner's: its tenant, and once it started, where, what it holds there, the time
/// it's charged for and since when.
#[derive(Debug, Clone, Copy)]
struct Charged {
    tenant: u32,
    at: Option<(u32, [u64; FAIR], Ms, i64)>,
}

/// What a run holds of a runner, as fair share counts it (the runner's own `holds`): a slot and
/// its request, or the runner whole as the directory has it (`exclusive`, asking no CPUs).
fn holds(ask: &Ask, size: &Size) -> [u64; FAIR] {
    let q = &ask.needs.request;
    if ask.needs.exclusive && q.cpu.is_zero() {
        [1, size.cpu.map_or(0, Cpus::get), size.mem.unwrap_or(0), size.gpu.map_or(0, u64::from)]
    } else {
        [1, q.cpu.get(), q.mem, u64::from(q.gpu)]
    }
}

fn plus(a: [i128; FAIR], b: [i128; FAIR]) -> [i128; FAIR] {
    let mut out = a;
    for (o, x) in out.iter_mut().zip(b) {
        *o = o.saturating_add(x);
    }
    out
}

/// `u` (each dimension times milliseconds) in milliseconds of runs that hold `x`: the largest
/// ratio over the dimensions past the slot that `x` holds any of, else the slot's (runs times
/// milliseconds).
fn in_runs_like(u: &[i128; FAIR], x: &[u64; FAIR]) -> i64 {
    let ratio = |d: usize| u.get(d).copied().unwrap_or(0) / i128::from(x.get(d).copied().unwrap_or(1).max(1));
    let k = (1..FAIR).filter(|d| x.get(*d).is_some_and(|a| *a > 0)).map(ratio).max().unwrap_or_else(|| ratio(0));
    i64::try_from(k).unwrap_or(if k > 0 { i64::MAX } else { i64::MIN })
}

impl Usage {
    /// The tenant `ask` is, by handle: its class and its leaf.
    fn tenant_of(&mut self, ask: &Ask) -> u32 {
        self.buf.clear();
        self.buf.push(char::from(b'0'.saturating_add(u8::try_from(ask.class.index()).unwrap_or(0))));
        for part in [&ask.repo, &ask.actor, &ask.lane] {
            self.buf.push('\0');
            self.buf.push_str(part);
        }
        let t = self.names.intern(&self.buf);
        if self.tenants.len() <= t as usize {
            self.tenants.resize_with((t as usize).saturating_add(1), Tenant::default);
        }
        t
    }

    /// Its tenant, counting run `run` from the first word about it.
    fn note(&mut self, run: &Arc<str>, ask: &Ask) -> u32 {
        if let Some(c) = self.runs.get(run) {
            return c.tenant;
        }
        let t = self.tenant_of(ask);
        self.runs.insert(Arc::clone(run), Charged { tenant: t, at: None });
        if let Some(x) = self.tenants.get_mut(t as usize) {
            x.live = x.live.saturating_add(1);
        }
        t
    }

    /// What a word about `run` (asking `ask`) to runner `to` carries: its tenant's usage elsewhere
    /// since this owner's last word about it there, in milliseconds of runs like it (`size`: that
    /// runner's, as the directory has it, for a run that takes it whole).
    pub fn word(&mut self, run: &Arc<str>, ask: &Ask, to: u32, size: &Size) -> i64 {
        let t = self.note(run, ask);
        let Some(x) = self.tenants.get_mut(t as usize) else { return 0 };
        let now = x.charged;
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::DoubleDelta) {
            let before = *x.said.entry(to).or_insert(now);
            return in_runs_like(&plus(now, before.map(i128::saturating_neg)), &holds(ask, size));
        }
        match x.said.insert(to, now) {
            Some(before) => in_runs_like(&plus(now, before.map(i128::saturating_neg)), &holds(ask, size)),
            None => 0,
        }
    }

    /// Charges `t` for `c` (each dimension times milliseconds), and the word `on` heard alike: `on`
    /// charges what runs there itself.
    fn charge(&mut self, t: u32, on: u32, c: [i128; FAIR]) {
        let Some(x) = self.tenants.get_mut(t as usize) else { return };
        x.charged = plus(x.charged, c);
        if let Some(s) = x.said.get_mut(&on) {
            *s = plus(*s, c);
        }
    }

    /// Run `run` (asking `ask`) started on runner `on` at `now`, by that runner's word: its tenant
    /// is charged what it holds there for its median.
    pub fn started(&mut self, run: &Arc<str>, ask: &Ask, on: u32, size: &Size, now: i64) {
        let t = self.note(run, ask);
        let Some(c) = self.runs.get_mut(run) else { return };
        if c.at.is_some() {
            return;
        }
        let (h, ms) = (holds(ask, size), ask.median.unwrap_or(0));
        c.at = Some((on, h, ms, now));
        self.charge(t, on, h.map(|a| i128::from(a).saturating_mul(i128::from(ms))));
    }

    /// Run `run` ended at `now`, by its runner's word: its charge is settled to the time it ran.
    pub fn ended(&mut self, run: &str, now: i64) {
        let Some(c) = self.runs.get_mut(run) else { return };
        let Some((on, h, ms, since)) = c.at else { return };
        let ran = Ms::try_from(now.saturating_sub(since)).unwrap_or(0);
        c.at = Some((on, h, ran, since));
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::NoSettle) {
            return;
        }
        let diff = i128::from(ran).saturating_sub(i128::from(ms));
        let t = c.tenant;
        self.charge(t, on, h.map(|a| i128::from(a).saturating_mul(diff)));
    }

    /// Run `run` is gone from this owner: once none of its tenant's runs is here, the tenant's
    /// counts go.
    pub fn forget(&mut self, run: &str) {
        let Some(c) = self.runs.remove(run) else { return };
        let Some(x) = self.tenants.get_mut(c.tenant as usize) else { return };
        x.live = x.live.saturating_sub(1);
        if x.live == 0 {
            *x = Tenant::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            pace: Pace::default(),
        };
        assert_eq!(serde_json::from_str::<ToOwner>(&serde_json::to_string(&w)?)?, w);
        // W05-R26: a pace is said once measured, and read back; unmeasured, it isn't said, and an
        // owner from before (which knows no `pace`) reads the rest as it did.
        assert!(!serde_json::to_string(&w)?.contains("pace"));
        let mut paced = w.clone();
        if let ToOwner::Waiting { pace, .. } = &mut paced {
            *pace = Pace { speed: 1300, slowness: 1500, disk_bps: 900_000_000 };
        }
        let said = serde_json::to_string(&paced)?;
        assert!(said.contains(r#""pace":{"speed":1300,"slowness":1500,"disk_bps":900000000}"#), "{said}");
        assert_eq!(serde_json::from_str::<ToOwner>(&said)?, paced);
        assert!(serde_json::from_str::<ToOwner>(r#"{"op":"a-newer-one","run":"r1"}"#).is_err());
        Ok(())
    }

    /// LB08: room tokens' words travel as placement's others do, tagged by `op` (`fast_hint`,
    /// `fast_release`), each about no run; a word's `tokens` is said only when true, and a word
    /// from a runner before it reads as one every owner hears.
    #[test]
    fn lb08_room_tokens_words_are_ops_and_a_words_tokens_is_optional() -> Result<(), serde_json::Error> {
        let word = FastWord { epoch: 3, room: [2000, 1 << 30, 0, 0], budget: [8000, 8 << 30, 0, 0], tokens: true, ..FastWord::default() };
        let hint = ToOwner::FastHint { word };
        let s = serde_json::to_string(&hint)?;
        assert!(s.starts_with(r#"{"op":"fast_hint","word":{"#) && s.contains(r#""tokens":true"#), "{s}");
        assert_eq!(serde_json::from_str::<ToOwner>(&s)?, hint);
        assert_eq!(hint.run(), "");
        let release = ToRunner::FastRelease { epoch: 3, amount: [1000, 0, 0, 0] };
        let s = serde_json::to_string(&release)?;
        assert_eq!(s, r#"{"op":"fast_release","epoch":3,"amount":[1000,0,0,0]}"#);
        assert_eq!(serde_json::from_str::<ToRunner>(&s)?, release);
        // The word every owner hears says no `tokens`, and one from before it reads as that.
        assert!(!serde_json::to_string(&FastWord { tokens: false, ..word })?.contains("tokens"));
        let before = r#"{"epoch":3,"room":[2000,0,0,0],"budget":[8000,0,0,0]}"#;
        assert!(!serde_json::from_str::<FastWord>(before)?.tokens);
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

    /// The usage a probe or start carries is optional on the wire (W05-R32): an owner before it
    /// says none and is read as carrying nothing, and none is not said.
    #[test]
    fn w05_r32_usage_elsewhere_is_an_optional_field_on_probes_and_starts() -> Result<(), serde_json::Error> {
        let start = ToRunner::Start { run: "r1".into(), fence: Fence { epoch: 3, n: 7 }, elsewhere: 0 };
        assert_eq!(serde_json::to_string(&start)?, r#"{"op":"start","run":"r1","fence":{"epoch":3,"n":7}}"#);
        let carried = ToRunner::Start { run: "r1".into(), fence: Fence { epoch: 3, n: 7 }, elsewhere: -250 };
        assert_eq!(serde_json::from_str::<ToRunner>(&serde_json::to_string(&carried)?)?, carried);
        let probe = r#"{"op":"probe","ask":{"run":"r2","needs":{"request":{"cpu":1000,"mem":0,"disk":0,"gpu":0}},"class":"batch","repo":"acme","actor":"a"},"elsewhere":1200}"#;
        assert!(matches!(serde_json::from_str::<ToRunner>(probe)?, ToRunner::Probe { elsewhere: 1200, .. }));
        Ok(())
    }

    fn one(run: &str, cpus: u64, median: Option<Ms>) -> Ask {
        Ask { run: run.into(), needs: crate::sched::needs::Needs { request: Resources { cpu: Cpus::whole(cpus), ..Resources::default() }, ..Default::default() }, repo: "acme".into(), actor: "a".into(), median, ..Ask::default() }
    }

    /// What an owner's words carry (W05-R32): a pair's first word nothing; then what its tenant was
    /// charged elsewhere since the last word there, never what ran there; in runs like the one the
    /// word is about; settled to the time run; and nothing once the tenant is idle here.
    #[test]
    fn w05_r32_an_owner_carries_its_tenant_s_usage_elsewhere_since_its_last_word() {
        let size = Size { cpu: Some(Cpus::whole(8)), mem: Some(1 << 30), disk: None, gpu: Some(0) };
        let (x, y, z) = (one("x", 1, Some(1000)), one("y", 1, Some(1000)), one("z", 2, None));
        let (rx, ry, rz): (Arc<str>, Arc<str>, Arc<str>) = (Arc::from("x"), Arc::from("y"), Arc::from("z"));
        let mut u = Usage::default();
        assert_eq!((u.word(&rx, &x, 1, &size), u.word(&ry, &y, 2, &size)), (0, 0), "a pair's first word carries nothing");
        u.started(&ry, &y, 2, &size, 10_000);
        assert_eq!(u.word(&rx, &x, 1, &size), 1000, "y's second of one CPU on runner 2, to runner 1");
        assert_eq!(u.word(&rx, &x, 1, &size), 0, "said once");
        assert_eq!(u.word(&ry, &y, 2, &size), 0, "runner 2 charges y itself");
        u.ended("y", 11_500);
        assert_eq!(u.word(&rz, &z, 1, &size), 250, "settled: 500 ms more of one CPU is 250 ms of z's two");
        assert_eq!(u.word(&ry, &y, 2, &size), 0, "runner 2 settles y itself");
        u.started(&rz, &z, 1, &size, 12_000);
        u.ended("z", 12_400);
        assert_eq!(u.word(&rx, &x, 2, &size), 800, "z had no median: charged as it ended, 400 ms of two CPUs");
        for r in ["x", "y", "z"] {
            u.forget(r);
        }
        assert_eq!(u.word(&rx, &x, 1, &size), 0, "idle here: no credit or debt");
        let mut d = Usage::default();
        super::super::planted(Some(super::super::Mutant::DoubleDelta), || {
            d.word(&rx, &x, 1, &size);
            d.word(&ry, &y, 2, &size);
            d.started(&ry, &y, 2, &size, 0);
            d.ended("y", 1000);
            let (a, b) = (d.word(&rx, &x, 1, &size), d.word(&rx, &x, 1, &size));
            assert_eq!((a, b), (1000, 1000), "DoubleDelta says it again, and the check above catches it");
        });
    }
}
