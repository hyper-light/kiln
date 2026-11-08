//! The owner's side of placement (docs/reach.md §2; docs/needs.md §9, §11 and §13): the node a
//! run is asked of owns it (Ray's ownership). There's no queue at the owner: each run is a ticket
//! whose probes wait at the runners that could take it, and a runner with room starts it (late
//! binding: Sparrow).
//!
//! - **Asked**: a run no runner could run even idle (a constraint none meets, or more than any is
//!   known to have) is answered never at once, naming the need and the nearest runner; a size not
//!   yet measured is unknown, never too small. Otherwise [`PROBES`] runners are probed, drawn at
//!   random among those that could take it, are linked and admit probes (the power of two choices:
//!   Mitzenmacher, "The Power of Two Choices in Randomized Load Balancing", IEEE TPDS 2001, the
//!   most loaded of n at log log n / log d + O(1) where one random choice leaves log n / log log n;
//!   Sparrow's batch sampling, Ousterhout et al., SOSP 2013, probes d·m for m tasks, here m = 1).
//!   The draw is from a seed the shell passes in ([`Owner::seeded`]): kiln-ci takes no randomness
//!   of its own. Which runners suit an ask is worked out once per kind of ask while the directory
//!   stands (Borg's equivalence classes, Verma et al., EuroSys 2015), so an ask costs O(d), not
//!   O(N). A run is probed further, d more, only when every runner probed said never or its link
//!   went down: one that says it waits keeps the run in its line, with news. When every runner
//!   probed says it waits, d more are drawn among the runners whose published room ([`Owner::hint`],
//!   the `offers` named feature) shows room for it, and only those: by data, not a count picked.
//! - **Offered**: the first offer is held while the other runners probed answer, for at most the
//!   offering link's retransmission timeout less its round trip (RFC 6298: the runner holds its
//!   offer one RTO from when it sent it, so a take sent later than that, less the round trip,
//!   would find it lapsed). Then the emptiest offer is taken, by the engine's fullness (the largest
//!   share of any dimension the runner's size knows that isn't free, `sched::state::held_share`'s
//!   rule); between equals, by class (docs/needs.md §5): `critical`, `land` and `interactive` take
//!   the most room (latency), `batch` and `background` the least (packing, big holes left); then
//!   by name. The others are declined, at most d − 1. An offer gives way when another runner is
//!   sure to finish it sooner: its latest start plus its predicted run there before this one's
//!   predicted end (both runners' predictions, the shell's from history, docs/needs.md E6). A
//!   runner holding the take gets the run: the asker is told it's starting there, and the shell
//!   copies its tree and starts it under that claim if [`Owner::starting`] still says so.
//! - **Started** only on the runner's word that its process started ([`ToOwner::Started`]); a
//!   claim that lapses before that is told to the asker as a correction.
//! - **Once**: a run's start goes to one runner. It goes back to waiting only on that runner's word
//!   that it didn't start it ([`ToOwner::Lapsed`]), never on its silence. A runner whose link is
//!   down is waited for, with everything it holds; only its absence from the directory, or a new
//!   epoch of its process, ends a claim there, and a run there that may have started is then lost
//!   with it.
//! - **News**: the asker hears its first answer once every runner probed has answered (or its
//!   link's retransmission timeout passed, RFC 6298), then each change of its answer as it happens:
//!   each runner's word of a place or reason changed, corrected to this node's clock by the link's
//!   measured offset (RFC 5905).
//! - **Older runners** (a rolling upgrade): a run no runner that places could start now goes to an
//!   older one the shell's own pick can start it on, idle as the directory last saw it; refused
//!   there, it waits at the runners that place.
//! - **Fast** (docs/needs.md §11, AUDIT P6): a word's runner and run are looked up once, by hash
//!   ([`super::keys`]); past that, runners and tickets are integer handles into slabs, a ticket's
//!   words a short list, its ask shared by every probe of it, and a word about a ticket gone
//!   answered in O(1).

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::answer::{self, Answer, At, Count, Nearest};
use super::ask::{Ask, Peer};
use super::keys::{Fx, FxMap, Interner, SplitMix};
use super::link::{Offset, Rtt};
use super::msg::{Dim, Down, Estimate, Place, Reason, ToOwner, ToRunner, Up};
use super::msg::FastWord;
use crate::facts::RunnerFacts;
use crate::lb;
use crate::pace::Pace;
use crate::sched::needs;
use crate::sched::routing::{Constraint, Op};
use crate::sched::state::{Class, Cpus, Fence, Resources};
use crate::sched::units::Ms;

/// How many runners a run is probed at, and probed further by: two (Mitzenmacher, IEEE TPDS 2001:
/// from one choice to two, the most loaded of n falls from log n / log log n to log log n / log 2;
/// each choice past two takes off only a constant factor more, log 2 / log d).
pub const PROBES: u32 = 2;

/// What a runner has, as far as the directory knows: each dimension `None` until it's measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub cpu: Option<Cpus>,
    pub mem: Option<u64>,
    pub disk: Option<u64>,
    pub gpu: Option<u32>,
}

impl Size {
    /// Every dimension of `r`, known.
    pub fn known(r: &Resources) -> Size {
        Size { cpu: Some(r.cpu), mem: Some(r.mem), disk: Some(r.disk), gpu: Some(r.gpu) }
    }
}

/// A runner as the owner's directory has it. A runner not in the directory is gone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub name: String,
    /// What constraints are checked against: its facts.
    pub facts: RunnerFacts,
    pub labels: Vec<String>,
    pub size: Size,
    /// Its process's epoch: a new one ends every claim made under an older one.
    pub epoch: u64,
    /// The link to it is up. One that's down is still there: what it holds waits for it.
    pub connected: bool,
    /// It admits probes (it announced placement). An older one doesn't: the shell places on it as
    /// before placement.
    pub places: bool,
    /// For an older runner, what's idle there as the directory last saw it (`None`: not known).
    pub idle: Option<Resources>,
    /// It admits the placement kernel's starts (it named `place.fast`, docs/lb.md): an ordinary
    /// run it suits is started there at once when its room covers it, with no probe.
    pub fast: bool,
    /// It answers `Sync` (the `place.sync` named feature, LB03): what it started is asked of it
    /// when its owner or it started again, never said again.
    pub syncs: bool,
}

/// A run's hard state, as its owner keeps it on its disk across its own restarts (W05-R25, LB03;
/// docs/reach.md §2.3): what was asked and in which of the owner's epochs, the attempt its start
/// went to (the runner, the claim's fence, whether that runner said it started), and, once it's
/// over, the answer its asker is owed until it's told. Everything else (probes, offers, words,
/// hints, the runners suited) is soft: rebuilt from the directory and runners' words.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Kept {
    pub ask: Ask,
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<Answer>,
}

/// Where a run's start went: the runner, its claim there (`None`: the runner said it ran before,
/// under a claim not said), and whether the runner said its process started.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Attempt {
    pub runner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<Fence>,
    #[serde(default)]
    pub started: bool,
}

/// What a link to a runner measures, as the shell measures it ([`super::link`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    pub rtt: Option<Rtt>,
    pub offset: Offset,
}

/// What the owner's shell is to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Say this to that runner.
    Tell { runner: String, msg: ToRunner },
    /// Tell the run's asker this: its first answer, then each change as news.
    Answer { run: String, answer: Answer },
    /// Copy the run's tree to `runner`, then start it there under claim `fence` if
    /// [`Owner::starting`] still says so.
    Go { run: String, runner: String, fence: Fence },
    /// No runner that places could start it now, and older ones could (`runners`): place it as
    /// before placement, by the shell's own pick among them, and say how that went
    /// ([`Owner::fallback_started`], [`Owner::fallback_refused`]).
    Fallback { run: String, runners: Vec<String> },
    /// Say the run's start to `runner` again, under claim `fence`, as it was said (its link came
    /// back, or it started again): it starts nothing twice, and answers that it ran, how it ended,
    /// or that the claim lapsed.
    Restart { run: String, runner: String, fence: Fence },
}

/// What the owner's shell is to do, by handle (docs/needs.md §11, AUDIT P6): runners by their
/// handle here ([`Owner::runner_id`], [`Owner::runner_name`]), runs by their shared id, words to
/// runners as [`Down`]. No names copied: [`Owner::act`] makes the wire's [`Act`] of it at the edge.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Tell { to: u32, run: Arc<str>, msg: Down },
    Answer { run: Arc<str>, answer: Answer },
    Go { run: Arc<str>, to: u32, fence: Fence },
    Fallback { run: Arc<str>, runners: Vec<String> },
    Restart { run: Arc<str>, to: u32, fence: Fence },
}

/// A runner's handle: its name interned.
type Rid = u32;
/// A ticket's handle: its place in the slab.
type Tid = u32;

fn ix(h: u32) -> usize {
    h as usize
}

/// A probed runner's last word on a ticket.
#[derive(Debug, Clone, PartialEq)]
enum Word {
    /// Probed, not answered yet.
    Asked,
    Waiting { reason: Reason, place: Place, start: Estimate, predicted: Option<Ms>, free: Resources },
    /// It offered: the offer held while the owner chooses, or taken.
    Offered,
    Never(String),
    /// Its link is down: probed again when it's back.
    Away,
    /// It offered, and the offer was declined for a runner sure to finish sooner: probed again
    /// when what's known of the others changes.
    Passed,
}

impl Word {
    /// Whether the runner may yet start the run: it hasn't said never, its link is up, and its
    /// health doesn't bar new work there now (AUDIT L1, L2: a strained or barred runner is
    /// widened past as a never is, so retries don't land where the gate would yield or the work
    /// would fail for the machine's sake; it's probed again once its word says it's well).
    fn live(&self) -> bool {
        !matches!(self, Word::Never(_) | Word::Away | Word::Waiting { reason: Reason::Strained { .. } | Reason::Barred { .. }, .. })
    }
}

/// Where a ticket stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Probes out, no offer taken.
    Waiting,
    /// Its runner's offer `fence` taken: waiting for the runner to hold it.
    Taking { fence: Fence },
    /// Its runner holds it: its tree is being copied there.
    Held { fence: Fence },
    /// Its start went to its runner under `fence`.
    Starting { fence: Fence },
    /// It runs on its runner, by the runner's word.
    Running { fence: Option<Fence> },
    /// An older runner is being tried by the shell's own pick.
    Fallback,
}

/// An offer held while the owner chooses: its runner, its claim, what it gets and what was free.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Offer {
    runner: Rid,
    fence: Fence,
    got: Resources,
    free: Resources,
}

/// The runners a kind of ask suits, as the directory stood (Borg's equivalence classes): asks
/// with the same constraints and amounts suit the same runners, worked out once.
#[derive(Debug)]
struct Suit {
    requires: Vec<String>,
    together: bool,
    request: [u64; 4],
    among: Vec<String>,
    /// Suited, placing and linked: what's drawn from (by handle, in order).
    live: Vec<Rid>,
    /// Suited and placing, the link down.
    away: Vec<Rid>,
    /// Suited, older (no placement).
    older: Vec<Rid>,
    /// The runners not suited or older, by reason, and how many.
    passed: Vec<(String, u32)>,
}

impl Suit {
    fn is(&self, ask: &Ask) -> bool {
        self.together == ask.needs.together.is_some() && self.request == super::ask::amounts(&ask.needs.request) && self.requires == ask.needs.requires && self.among == ask.among
    }

    /// The same kind of ask as `o`.
    fn same(&self, o: &Suit) -> bool {
        self.together == o.together && self.request == o.request && self.requires == o.requires && self.among == o.among
    }
}

/// The hash of what decides which runners an ask suits.
fn suit_key(ask: &Ask) -> u64 {
    let mut h = Fx::default();
    ask.needs.requires.hash(&mut h);
    ask.needs.together.is_some().hash(&mut h);
    super::ask::amounts(&ask.needs.request).hash(&mut h);
    ask.among.hash(&mut h);
    h.finish()
}

#[derive(Debug, Clone)]
struct Ticket {
    run: Arc<str>,
    /// Shared by every probe of it.
    ask: Arc<Ask>,
    /// The runners it suits, as its answer counts them.
    suit: Arc<Suit>,
    stage: Stage,
    /// The runner of a stage past waiting.
    runner: Option<Rid>,
    /// What it got there and what was free: the offer taken.
    got: Option<(Resources, Resources)>,
    /// Each runner probed, and its last word.
    words: Vec<(Rid, Word)>,
    /// Each runner probed that's gone since, and why.
    gone: Vec<(Rid, String)>,
    /// Offers held while it chooses, and when it chooses at the latest.
    offers: Vec<Offer>,
    choose_at: Option<i64>,
    /// What its asker was told last.
    told: Option<Answer>,
    /// When its first answer is due, if it hasn't been given.
    due: Option<i64>,
    /// The owner's epoch it was asked in (an earlier one for a run kept across a restart, LB03).
    epoch: u64,
    /// Kept across a restart of its owner or its runner, and not yet reconciled by its runner's
    /// `Synced`; and said again by its runner since the `Sync` went.
    unsynced: bool,
    confirmed: bool,
    /// A second offer taken: what [`super::Mutant::TakeTwo`] plants.
    #[cfg(test)]
    second: Option<(Rid, Fence)>,
    /// Its handle in the placement kernel while the kernel places it (docs/lb.md): its start
    /// decided there, sent or about to be, or it runs by the kernel's start.
    fast: Option<lb::Handle>,
}

impl Ticket {
    fn word(&self, r: Rid) -> Option<&Word> {
        self.words.iter().find(|(x, _)| *x == r).map(|(_, w)| w)
    }

    fn offer_of(&self, r: Rid) -> Option<Fence> {
        self.offers.iter().find(|o| o.runner == r).map(|o| o.fence)
    }

    /// Waiting, with no runner probed that may yet start it.
    fn hungry(&self) -> bool {
        self.stage == Stage::Waiting && !self.words.iter().any(|(_, w)| w.live())
    }

    /// Every runner probed said it waits there, or can't take it: none offered, none passed, none
    /// still to answer, one at least waiting.
    fn full(&self) -> bool {
        self.stage == Stage::Waiting
            && self.offers.is_empty()
            && self.words.iter().any(|(_, w)| matches!(w, Word::Waiting { .. }))
            && self.words.iter().all(|(_, w)| matches!(w, Word::Waiting { .. } | Word::Never(_) | Word::Away))
    }

    /// Waiting at fewer than [`PROBES`] runners that may yet start it (some were away as it was
    /// drawn).
    fn short(&self) -> bool {
        self.stage == Stage::Waiting && self.words.iter().filter(|(_, w)| w.live()).count() < PROBES as usize
    }
}

/// A runner as the owner keeps it: as the directory has it (`None`: gone), its link, and the
/// tickets with a word or a claim there.
#[derive(Debug, Clone, Default)]
struct Slot {
    /// Its latest published room (the `offers` named feature): by its count, and what was free.
    hint: Option<(u64, Resources)>,
    /// Its place in `roomy`, while its hint shows room.
    roomy_at: Option<u32>,
    machine: Option<Machine>,
    link: Link,
    at: BTreeSet<Tid>,
    /// Its process's epoch, as its words say it (0: not heard yet).
    epoch: u64,
    /// Its pace as its latest word said it (W05-R26): what the load-balancing kernel ranks by.
    pace: Pace,
}

/// The tickets a node owns.
#[derive(Debug, Clone, Default)]
pub struct Owner {
    me: String,
    epoch: u64,
    rng: SplitMix,
    names: Interner,
    slots: Vec<Slot>,
    /// The runners in the directory, by name.
    present: Vec<Rid>,
    /// Which runners suit each kind of ask, as the directory stands: dropped as it changes.
    suits: FxMap<u64, Vec<Arc<Suit>>>,
    tickets: Vec<Option<Ticket>>,
    spare: Vec<Tid>,
    by_run: FxMap<Arc<str>, Tid>,
    /// Each ticket slot's generation: a token names a ticket in one ([`Owner::token`]).
    gens: Vec<u32>,
    /// First answers due, and choices due, by when.
    due: BTreeSet<(i64, Tid)>,
    choose: BTreeSet<(i64, Tid)>,
    draw: Vec<u32>,
    /// The runners whose latest hint shows any room, in no order (each knows its place).
    roomy: Vec<Rid>,
    /// Hard state kept by the shell (LB03, [`Owner::keeping`]): the runs whose [`Kept`] changed
    /// since the shell last took them, the runs over whose answer their asker is still owed, the
    /// runs kept from before a restart waiting for the directory to be asked again, the runners to
    /// say `Sync` to once they're linked, those whose `Synced` is awaited, and those said it since
    /// this owner was restored.
    keeping: bool,
    dirty: BTreeSet<Arc<str>>,
    owed: FxMap<Arc<str>, Kept>,
    pending: Vec<Kept>,
    sync_due: BTreeSet<Rid>,
    syncing: BTreeSet<Rid>,
    synced_once: BTreeSet<Rid>,
    sync_all: bool,
    /// The placement kernel: the hot path of every ordinary ask (docs/lb.md, "Adopted").
    fast: Fast,
    /// Its tenants' usage across the fleet, carried on its probes and starts (W05-R32).
    usage: super::msg::Usage,
}

/// The placement kernel at an owner (docs/lb.md; W05-R03, W05-R04, W05-R23): it decides at once
/// where an ordinary run starts, from what the runners said of their room less what this owner
/// sent them since, and sends its start to that runner alone. When no member has room (or a
/// decline leaves none that has), the ticket is probing's, where waiting, fair share, classes and
/// kept room are decided: the kernel's own line is never used.
#[derive(Debug, Clone)]
struct Fast {
    lb: lb::Owner,
    out: lb::Out,
    /// Each runner the kernel knows, by handle: its budget as it last said it.
    joined: Vec<Option<lb::Amounts>>,
    /// Counts runners joining and leaving the kernel: a signature's members are synced again.
    era: u64,
    /// Each kind of ask's signature (Borg's equivalence classes, as [`Suit`]s are): by its suit's
    /// key, the suit and generation its members were synced from, its id and its members.
    sigs: FxMap<u64, Vec<FastSig>>,
    /// Each kernel run's ticket, by its handle's slot: the handle's generation and the ticket.
    by_slot: Vec<Option<(u32, Tid)>>,
}

#[derive(Debug, Clone)]
struct FastSig {
    suit: Arc<Suit>,
    era: u64,
    sig: lb::SigId,
    members: Vec<Rid>,
}

impl Default for Fast {
    fn default() -> Self {
        Fast { lb: lb::Owner::new(lb::OwnerId(0)), out: lb::Out::default(), joined: Vec::new(), era: 0, sigs: FxMap::default(), by_slot: Vec::new() }
    }
}

/// The claim a kernel start goes under at its owner (the place core's stages are fenced): its
/// kernel epoch and its handle, the high bit set, so it is no runner's claim.
fn fast_fence(kernel: lb::Handle, epoch: u32) -> Fence {
    Fence { epoch: u64::from(epoch), n: (1 << 63) | (u64::from(kernel.generation) << 32) | u64::from(kernel.slot) }
}

/// Why `m` could never run `ask` as asked, if that's known: a constraint it doesn't meet (in the
/// words a person reads, "not linux"), more than it's known to have, or a gang. A dimension of its
/// size not measured yet is no reason: the runner itself answers for it.
pub fn unsuited(m: &Machine, ask: &Ask) -> Option<String> {
    if !ask.among.is_empty() && !ask.among.contains(&m.name) {
        return Some("not among the machines that can run it".into());
    }
    for src in &ask.needs.requires {
        match Constraint::parse(src) {
            Ok(c) if c.eval(&m.facts, &m.labels) => {}
            Ok(Constraint::Cmp { fact, op: Op::Eq, value }) if fact == "os" || fact == "arch" => return Some(format!("not {}", value.show())),
            Ok(Constraint::Label(l)) => return Some(format!("no label {l}")),
            Ok(_) => return Some(format!("not {src}")),
            Err(e) => return Some(e),
        }
    }
    if ask.needs.together.is_some() {
        return Some("commands can't start together yet".into());
    }
    #[cfg(test)]
    let unknown_small = super::mutant() == Some(super::Mutant::NeverOnUnknown);
    #[cfg(not(test))]
    let unknown_small = false;
    let small = |asked: u64, has: Option<u64>| match has {
        Some(h) => asked > h,
        None => unknown_small && asked > 0,
    };
    let (q, s) = (&ask.needs.request, &m.size);
    let short: Vec<String> = [
        small(q.cpu.get(), s.cpu.map(Cpus::get)).then(|| needs::cpus(s.cpu.unwrap_or_default())),
        small(q.mem, s.mem).then(|| format!("{} of memory", needs::show_size(s.mem.unwrap_or_default()))),
        small(q.disk, s.disk).then(|| format!("{} of disk", needs::show_size(s.disk.unwrap_or_default()))),
        small(u64::from(q.gpu), s.gpu.map(u64::from)).then(|| needs::gpus(s.gpu.unwrap_or_default())),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!short.is_empty()).then(|| format!("too small ({})", short.join(", ")))
}

/// How full a runner is by an offer's word, in millionths, as the engine's `held_share` counts a
/// share: the largest, over the dimensions its size knows, of what isn't free over its size.
/// Nothing known ranks last.
pub fn fullness(size: &Size, free: &Resources) -> u64 {
    let dims = [(size.cpu.map(Cpus::get), free.cpu.get()), (size.mem, free.mem), (size.disk, free.disk), (size.gpu.map(u64::from), u64::from(free.gpu))];
    dims.into_iter()
        .filter_map(|(s, f)| {
            let s = s.filter(|s| *s > 0)?;
            let held = s.saturating_sub(f.min(s));
            Some(u64::try_from(u128::from(held).saturating_mul(1_000_000) / u128::from(s)).unwrap_or(u64::MAX))
        })
        .max()
        .unwrap_or(u64::MAX)
}

/// Whether a class wants the most room (latency) rather than the least that fits (packing), as
/// docs/needs.md §5 sorts them.
fn wants_room(c: Class) -> bool {
    matches!(c, Class::Critical | Class::Land | Class::Interactive)
}

/// A reason in the words counted across runners.
fn reason_words(r: &Reason) -> String {
    match r {
        Reason::Room { short } => match short.first().map(|s| s.dim) {
            Some(Dim::Cpu) => "too few idle CPUs".into(),
            Some(Dim::Mem) => "too little free memory".into(),
            Some(Dim::Disk) => "too little free disk".into(),
            Some(Dim::Gpu) => "too few free GPUs".into(),
            None => "no room".into(),
        },
        Reason::Kept { .. } => "room kept for it".into(),
        Reason::Line => "others ahead of it".into(),
        Reason::Whole { .. } => "other kiln work on it".into(),
        Reason::Draining => "draining".into(),
        Reason::Away => "link down".into(),
        Reason::Strained { .. } => "machine under strain".into(),
        Reason::Barred { cause, .. } => format!("barred there by its health ({cause})"),
    }
}

/// What a runner suited and not probed counts as.
const NOT_PROBED: &str = "not probed (two drawn at random)";

/// The lesser of two times, either known.
fn min_some(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

fn later(now: i64, ms: Ms) -> i64 {
    now.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// How a runner changed between two directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Same,
    Gone,
    Restarted,
    Away,
    Back,
}

// Handles, bookkeeping, words and answers.
impl Owner {
    /// The tickets of the node `me`, in its epoch (one past the last it took, kept on its disk),
    /// its draws from `seed`: the shell's own randomness (the OS's), so that owners draw apart.
    pub fn seeded(me: &str, epoch: u64, seed: u64) -> Owner {
        // The kernel's cursors start at this owner's place among owners (docs/lb.md, "Several
        // owners"): a number of its name's, so that owners pick apart among equals.
        let mut h = Fx::default();
        me.hash(&mut h);
        let fast = Fast { lb: lb::Owner::new(lb::OwnerId((h.finish() >> 32) as u32)), ..Fast::default() };
        Owner { me: me.to_owned(), epoch, rng: SplitMix(seed), fast, ..Owner::default() }
    }

    /// [`Self::seeded`] with seed 0, for tests and tools: a shell passes its own.
    pub fn new(me: &str, epoch: u64) -> Owner {
        Owner::seeded(me, epoch, 0)
    }

    /// Who it is to a runner.
    pub fn peer(&self) -> Peer<'_> {
        Peer { name: &self.me, epoch: self.epoch }
    }

    fn rid(&mut self, name: &str) -> Rid {
        let r = self.names.intern(name);
        while self.slots.len() <= ix(r) {
            self.slots.push(Slot::default());
        }
        r
    }

    fn name(&self, r: Rid) -> String {
        self.names.name(r).to_owned()
    }

    fn machine(&self, r: Rid) -> Option<&Machine> {
        self.slots.get(ix(r)).and_then(|s| s.machine.as_ref())
    }

    fn connected(&self, r: Rid) -> bool {
        self.machine(r).is_some_and(|m| m.connected)
    }

    fn t(&self, t: Tid) -> Option<&Ticket> {
        self.tickets.get(ix(t)).and_then(Option::as_ref)
    }

    fn t_mut(&mut self, t: Tid) -> Option<&mut Ticket> {
        self.tickets.get_mut(ix(t)).and_then(Option::as_mut)
    }

    /// Ticket `t`'s token: its slot and that slot's generation, never 0 (0 is no token).
    fn token(&self, t: Tid) -> u64 {
        let generation = self.gens.get(ix(t)).copied().unwrap_or(0);
        (u64::from(generation).saturating_add(1) << 32) | u64::from(t)
    }

    /// The ticket a token names, if it still does.
    fn by_token(&self, token: u64) -> Option<Tid> {
        let t = u32::try_from(token & u64::from(u32::MAX)).ok()?;
        let generation = u64::from(self.gens.get(ix(t)).copied().unwrap_or(0)).saturating_add(1);
        (token >> 32 == generation && self.t(t).is_some()).then_some(t)
    }

    /// What the link to `runner` measures now (its round trip, RFC 6298; its clock's offset,
    /// RFC 5905).
    pub fn link(&mut self, runner: &str, link: Link) {
        let r = self.rid(runner);
        if let Some(s) = self.slots.get_mut(ix(r)) {
            s.link = link;
        }
    }

    fn rtt(&self, r: Rid) -> Option<&Rtt> {
        self.slots.get(ix(r)).and_then(|s| s.link.rtt.as_ref())
    }

    fn rto(&self, r: Rid) -> Ms {
        Rtt::rto_of(self.rtt(r))
    }

    /// How long an offer from `r` is held while the others probed answer: its link's RTO less its
    /// SRTT (RFC 6298 §2), which follows from both ends' clocks, not a number picked. The runner
    /// holds an offer one RTO from when it sends it (`runner::Admission::offer`), by its own clock.
    /// The offer reaches this node one way later, and a take sent at `hold` after that reaches the
    /// runner a second way later: the take is in time when `one way + hold + one way <= RTO`, that
    /// is `hold <= RTO − RTT`. The round trip's best estimate is SRTT, so `hold = RTO − SRTT =
    /// max(G, 4·RTTVAR)`: the variation term RFC 6298 adds for a late answer is the time left to
    /// wait. Unmeasured, SRTT is unknown and taken as 0, leaving the initial RTO (§2.1, 1 s) at both
    /// ends.
    fn hold(&self, r: Rid) -> Ms {
        self.rto(r).saturating_sub(self.rtt(r).map_or(0, |x| x.srtt))
    }

    /// A time in `runner`'s clock, in this node's.
    fn owner_time(&self, r: Rid, at: i64) -> i64 {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::SkewedClock) {
            return at;
        }
        self.slots.get(ix(r)).map_or(at, |s| s.link.offset.owner_time(at))
    }

    fn note(&mut self, r: Rid, t: Tid) {
        if let Some(s) = self.slots.get_mut(ix(r)) {
            s.at.insert(t);
        }
    }

    fn unnote(&mut self, r: Rid, t: Tid) {
        if let Some(s) = self.slots.get_mut(ix(r)) {
            s.at.remove(&t);
        }
    }

    fn runs_at(&self, r: Rid) -> Vec<Tid> {
        self.slots.get(ix(r)).map(|s| s.at.iter().copied().collect()).unwrap_or_default()
    }

    fn set_word(&mut self, t: Tid, r: Rid, w: Word) {
        let Some(tk) = self.t_mut(t) else { return };
        match tk.words.iter_mut().find(|(x, _)| *x == r) {
            Some((_, old)) => *old = w,
            None => tk.words.push((r, w)),
        }
        self.note(r, t);
    }

    fn drop_word(&mut self, t: Tid, r: Rid) {
        if let Some(tk) = self.t_mut(t) {
            tk.words.retain(|(x, _)| *x != r);
            tk.offers.retain(|o| o.runner != r);
        }
        self.unnote(r, t);
        self.unhold_if_none(t);
    }

    /// An offer held from `r` gone (it lapsed, its runner went or said otherwise).
    fn unhold(&mut self, t: Tid, r: Rid) {
        if let Some(tk) = self.t_mut(t) {
            tk.offers.retain(|o| o.runner != r);
        }
        self.unhold_if_none(t);
    }

    fn unhold_if_none(&mut self, t: Tid) {
        let at = match self.t_mut(t) {
            Some(tk) if tk.offers.is_empty() => tk.choose_at.take(),
            _ => None,
        };
        if let Some(at) = at {
            self.choose.remove(&(at, t));
        }
    }

    fn undue(&mut self, t: Tid) {
        if let Some(d) = self.t_mut(t).and_then(|tk| tk.due.take()) {
            self.due.remove(&(d, t));
        }
    }

    fn remove(&mut self, t: Tid) -> Option<Ticket> {
        let tk = self.tickets.get_mut(ix(t)).and_then(Option::take)?;
        if self.keeping {
            self.dirty.insert(Arc::clone(&tk.run));
        }
        for (r, _) in &tk.words {
            self.unnote(*r, t);
        }
        if let Some(r) = tk.runner {
            self.unnote(r, t);
        }
        if let Some(d) = tk.due {
            self.due.remove(&(d, t));
        }
        if let Some(c) = tk.choose_at {
            self.choose.remove(&(c, t));
        }
        self.by_run.remove(&tk.run);
        if let Some(h) = tk.fast {
            self.fast_unnote(h);
        }
        self.usage.forget(&tk.run);
        if let Some(g) = self.gens.get_mut(ix(t)) {
            *g = g.wrapping_add(1);
        }
        self.spare.push(t);
        Some(tk)
    }

    /// The runners `ask` suits, as the directory stands: worked out once for each kind of ask.
    fn suit(&mut self, ask: &Ask) -> Arc<Suit> {
        let key = suit_key(ask);
        if let Some(s) = self.suits.get(&key).and_then(|v| v.iter().find(|s| s.is(ask))) {
            return Arc::clone(s);
        }
        let (mut live, mut away, mut older) = (Vec::new(), Vec::new(), Vec::new());
        let mut passed: BTreeMap<String, u32> = BTreeMap::new();
        for &r in &self.present {
            let Some(m) = self.machine(r) else { continue };
            match unsuited(m, ask) {
                Some(why) => *passed.entry(why).or_default() += 1,
                None if !m.places => {
                    older.push(r);
                    *passed.entry("an older runner, without placement".into()).or_default() += 1;
                }
                None if !m.connected => away.push(r),
                None => live.push(r),
            }
        }
        live.sort_unstable();
        away.sort_unstable();
        let s = Arc::new(Suit { requires: ask.needs.requires.clone(), together: ask.needs.together.is_some(), request: super::ask::amounts(&ask.needs.request), among: ask.among.clone(), live, away, older, passed: passed.into_iter().collect() });
        self.suits.entry(key).or_default().push(Arc::clone(&s));
        s
    }

    /// The directory changed (a runner came, went, or its link did): which runners suit what is
    /// worked out again as it's next asked.
    fn redraw(&mut self) {
        self.suits.clear();
    }

    /// Probes the ticket at `r`, its ask shared.
    fn probe_at(&mut self, t: Tid, r: Rid, out: &mut Vec<Cmd>) {
        let Some((ask, run)) = self.t(t).map(|tk| (Arc::clone(&tk.ask), Arc::clone(&tk.run))) else { return };
        self.set_word(t, r, Word::Asked);
        let token = self.token(t);
        let size = self.size_of(r);
        let elsewhere = self.usage.word(&run, &ask, r, &size);
        out.push(Cmd::Tell { to: r, run, msg: Down::Probe { ask, token, elsewhere } });
    }

    /// Probes `t` at up to [`PROBES`] more runners, drawn at random among those that suit it now,
    /// are linked and haven't been probed: how many.
    fn widen(&mut self, t: Tid, out: &mut Vec<Cmd>) -> usize {
        let Some(ask) = self.t(t).map(|tk| Arc::clone(&tk.ask)) else { return 0 };
        let suit = self.suit(&ask);
        let Some(tk) = self.t(t) else { return 0 };
        let fresh: Vec<Rid> = suit.live.iter().copied().filter(|r| tk.word(*r).is_none()).collect();
        let n = u32::try_from(fresh.len()).unwrap_or(u32::MAX);
        let mut draw = std::mem::take(&mut self.draw);
        self.rng.sample(n, PROBES, &mut draw);
        let picked: Vec<Rid> = draw.iter().filter_map(|i| fresh.get(ix(*i)).copied()).collect();
        self.draw = draw;
        for r in &picked {
            self.probe_at(t, *r, out);
        }
        picked.len()
    }

    /// Withdraws every probe of `t` but at `keep` and the runners that said never; an offer held
    /// is declined.
    fn withdraw_others(&mut self, t: Tid, keep: Option<Rid>, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t(t) else { return };
        let run = tk.run.clone();
        let others: Vec<(Rid, Option<Fence>)> = tk.words.iter().filter(|(r, w)| Some(*r) != keep && !matches!(w, Word::Never(_))).map(|(r, _)| (*r, tk.offer_of(*r))).collect();
        for (r, offer) in others {
            self.drop_word(t, r);
            let msg = match offer {
                Some(fence) => Down::Decline { fence },
                None => Down::Withdraw,
            };
            out.push(Cmd::Tell { to: r, run: Arc::clone(&run), msg });
        }
    }

    /// Probes a ticket afresh (after a claim lapsed or its runner went): its words but the nevers
    /// dropped, then [`PROBES`] runners drawn again.
    fn probe_afresh(&mut self, t: Tid, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t(t) else { return };
        let stale: Vec<Rid> = tk.words.iter().filter(|(_, w)| !matches!(w, Word::Never(_))).map(|(r, _)| *r).collect();
        for r in stale {
            self.drop_word(t, r);
        }
        self.widen(t, out);
    }

    /// Probes again each runner passed over for one sure to finish sooner (what's known of the
    /// others changed): news, never a timer, decides when.
    fn unpass(&mut self, t: Tid, out: &mut Vec<Cmd>) {
        let passed: Vec<Rid> = self.t(t).map(|tk| tk.words.iter().filter(|(_, w)| *w == Word::Passed).map(|(r, _)| *r).collect()).unwrap_or_default();
        for r in passed {
            if self.connected(r) {
                self.probe_at(t, r, out);
            }
        }
    }

    /// The queued answer, from every runner's word, its times in this node's clock.
    fn queued(&self, t: &Ticket) -> Answer {
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        let mut place: Option<At> = None;
        let (mut soonest, mut latest): (Option<i64>, Option<i64>) = (None, None);
        let mut nearest: Option<(u64, Nearest)> = None;
        for (r, w) in &t.words {
            let why = match w {
                Word::Asked => "haven't answered".to_owned(),
                Word::Offered => "offered it".to_owned(),
                Word::Away => "link down".to_owned(),
                Word::Passed => "offered it, another sure to finish sooner".to_owned(),
                Word::Never(why) => why.clone(),
                Word::Waiting { reason, place: p, start, free, .. } => {
                    if p.lo > 0 && place.as_ref().is_none_or(|b| p.lo < b.lo) {
                        place = Some(At { lo: p.lo, hi: p.hi, runner: self.name(*r) });
                    }
                    soonest = min_some(soonest, start.soonest.map(|at| self.owner_time(*r, at)));
                    latest = min_some(latest, start.latest.map(|at| self.owner_time(*r, at)));
                    if matches!(reason, Reason::Room { .. } | Reason::Kept { .. })
                        && let Some(n) = answer::nearest(&t.ask.needs.request, free)
                    {
                        let k = answer::kept(&t.ask.needs.request, &n);
                        if nearest.as_ref().is_none_or(|(best, _)| k > *best) {
                            nearest = Some((k, Nearest { ask: n, runner: self.name(*r) }));
                        }
                    }
                    reason_words(reason)
                }
            };
            *counts.entry(why).or_default() += 1;
        }
        for (_, why) in &t.gone {
            *counts.entry(why.clone()).or_default() += 1;
        }
        for (why, n) in &t.suit.passed {
            *counts.entry(why.clone()).or_default() += n;
        }
        // The runners it suited and wasn't probed at, counted from the handles that were.
        let mut seen: Vec<Rid> = t.words.iter().map(|(r, _)| *r).chain(t.gone.iter().map(|(r, _)| *r)).collect();
        seen.sort_unstable();
        seen.dedup();
        let among = |set: &[Rid]| set.len().saturating_sub(seen.iter().filter(|r| set.binary_search(r).is_ok()).count());
        for (why, n) in [(NOT_PROBED, among(&t.suit.live)), ("link down", among(&t.suit.away))] {
            if n > 0 {
                *counts.entry(why.to_owned()).or_default() += u32::try_from(n).unwrap_or(u32::MAX);
            }
        }
        let of = counts.values().fold(0u32, |a, n| a.saturating_add(*n));
        let mut reasons: Vec<Count> = counts.into_iter().map(|(reason, runners)| Count { reason, runners }).collect();
        reasons.sort_by(|a, b| b.runners.cmp(&a.runners).then_with(|| a.reason.cmp(&b.reason)));
        Answer::Queued { of, reasons, place, start: Estimate { soonest, latest }, nearest: nearest.map(|(_, n)| n) }
    }

    /// Tells the asker `answer`, if it isn't what it was told last (only real changes are news).
    fn tell(&mut self, t: Tid, answer: Answer, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t_mut(t) else { return };
        if tk.told.as_ref() == Some(&answer) {
            return;
        }
        #[cfg(test)]
        if tk.told.is_some() && matches!(answer, Answer::Queued { .. }) && super::mutant() == Some(super::Mutant::QuietAnswer) {
            return;
        }
        tk.told = Some(answer.clone());
        let run = tk.run.clone();
        out.push(Cmd::Answer { run, answer });
    }

    /// Tells a waiting ticket's asker where it stands now: its first answer, or news. Not while
    /// it holds offers: it's choosing, and its next answer is where it starts.
    fn answer(&mut self, t: Tid, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t(t) else { return };
        if tk.stage != Stage::Waiting || !tk.offers.is_empty() {
            return;
        }
        let first = tk.due.is_some() || tk.told.is_none();
        let a = self.queued(tk);
        self.undue(t);
        self.tell(t, a, out);
        if first {
            self.try_older_for(t, out);
        }
    }

    /// Whether every runner probed has answered.
    fn all_answered(&self, t: Tid) -> bool {
        self.t(t).is_some_and(|tk| tk.words.iter().all(|(_, w)| !matches!(w, Word::Asked)))
    }

    /// A ticket's runner's word changed what's known of it: its first answer once every runner
    /// probed has answered, news after.
    fn heard_of(&mut self, t: Tid, out: &mut Vec<Cmd>) {
        let pending = self.t(t).is_some_and(|tk| tk.due.is_some());
        if !pending || self.all_answered(t) {
            self.answer(t, out);
        }
    }

    /// What follows a word on a waiting ticket: the choice, once every runner probed has answered
    /// and an offer is held; more runners probed, once none probed may yet start it; and the
    /// asker told.
    fn settle(&mut self, t: Tid, now: i64, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t(t) else { return };
        if tk.stage == Stage::Waiting && !tk.offers.is_empty() && self.all_answered(t) {
            self.choose_now(t, now, out);
            return;
        }
        if tk.hungry() {
            self.widen(t, out);
        } else if tk.full() {
            self.widen_by_hints(t, out);
        }
        self.heard_of(t, out);
    }

    /// Every runner probed for `t` said it waits there (and none offered or is still to answer):
    /// it's probed at up to [`PROBES`] more, drawn at random among the runners whose latest hint
    /// shows room for it, and only those; none does, it waits where it is. Drawn at random, not the
    /// emptiest by hint: hints are old by the time they're read, and owners reading the same ones
    /// would all go to the same runner (Mitzenmacher, "How useful is old information?", IEEE TPDS
    /// 2000: with stale load information, choices at random among the few do better than the least
    /// loaded).
    fn widen_by_hints(&mut self, t: Tid, out: &mut Vec<Cmd>) -> usize {
        let Some(ask) = self.t(t).map(|tk| Arc::clone(&tk.ask)) else { return 0 };
        let suit = self.suit(&ask);
        let q = super::ask::amounts(&ask.needs.request);
        // A partial Fisher–Yates over the runners hinting room: each drawn at random, looked at
        // once, until d fit (O(d / the share that fits): O(d) when most do, all of them only when
        // few do).
        let mut picked: Vec<Rid> = Vec::new();
        let n = self.roomy.len();
        let mut i = 0;
        while i < n && picked.len() < PROBES as usize {
            let left = u32::try_from(n.saturating_sub(i)).unwrap_or(u32::MAX);
            let j = i.saturating_add(ix(self.rng.below(left)));
            self.roomy_swap(i, j);
            if let Some(&r) = self.roomy.get(i) {
                let fits = self.slots.get(ix(r)).and_then(|s| s.hint).is_some_and(|(_, free)| super::ask::fits(&q, &super::ask::amounts(&free)));
                if fits && suit.live.binary_search(&r).is_ok() && self.t(t).is_some_and(|tk| tk.word(r).is_none()) {
                    picked.push(r);
                }
            }
            i += 1;
        }
        for r in &picked {
            self.probe_at(t, *r, out);
        }
        picked.len()
    }

    /// The older runners the shell's pick could start `ask` on now: suited to it, linked, and idle
    /// enough as the directory last saw them.
    fn older_for(&mut self, t: Tid) -> Vec<String> {
        let Some(ask) = self.t(t).map(|tk| Arc::clone(&tk.ask)) else { return Vec::new() };
        let q = super::ask::amounts(&ask.needs.request);
        let suit = self.suit(&ask);
        suit.older.iter().filter_map(|r| self.machine(*r)).filter(|m| !m.places && m.connected && m.idle.is_some_and(|i| super::ask::fits(&q, &super::ask::amounts(&i)))).map(|m| m.name.clone()).collect()
    }

    /// A waiting ticket that no runner that places offered goes to an older runner idle enough for
    /// it, if there's one (a rolling upgrade: placed as before placement rather than waiting).
    fn try_older_for(&mut self, t: Tid, out: &mut Vec<Cmd>) {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::SkipOlder) {
            return;
        }
        let Some(tk) = self.t(t) else { return };
        if tk.stage != Stage::Waiting || tk.words.iter().any(|(_, w)| *w == Word::Offered) {
            return;
        }
        let older = self.older_for(t);
        self.fallback(t, older, out);
    }

    fn fallback(&mut self, t: Tid, runners: Vec<String>, out: &mut Vec<Cmd>) {
        if runners.is_empty() {
            return;
        }
        let Some(tk) = self.t_mut(t) else { return };
        tk.stage = Stage::Fallback;
        let run = tk.run.clone();
        out.push(Cmd::Fallback { run, runners });
    }

    /// Never: what no runner meets, and the runner nearest to it.
    fn never(&self, ask: &Ask) -> Answer {
        let machines: Vec<&Machine> = self.present.iter().filter_map(|r| self.machine(*r)).collect();
        if machines.is_empty() {
            return Answer::Never { need: "is known yet".into(), nearest: None };
        }
        let meets = |m: &Machine, src: &str| Constraint::parse(src).is_ok_and(|c| c.eval(&m.facts, &m.labels));
        let need = ask
            .needs
            .requires
            .iter()
            .find(|src| !machines.iter().any(|m| meets(m, src)))
            .map(|src| format!("meets {src}"))
            .or_else(|| (!ask.needs.requires.is_empty() && !machines.iter().any(|m| ask.needs.requires.iter().all(|s| meets(m, s)))).then(|| format!("meets all of {}", ask.needs.requires.join(", "))))
            .or_else(|| ask.needs.together.as_ref().map(|_| "starts commands together yet".to_owned()))
            .unwrap_or_else(|| format!("has room for {}", needs::request_words(&ask.needs.request).join(", ")));
        let met = |m: &Machine| ask.needs.requires.iter().filter(|s| meets(m, s)).count();
        let nearest = machines.iter().max_by(|a, b| met(a).cmp(&met(b)).then_with(|| (a.size.cpu, a.size.mem).cmp(&(b.size.cpu, b.size.mem))).then_with(|| b.name.cmp(&a.name)));
        let nearest = nearest.map(|m| {
            let cpus = m.size.cpu.map_or_else(|| "CPUs not measured".to_owned(), needs::cpus);
            let mem = m.size.mem.map_or_else(|| "memory not measured".to_owned(), |b| format!("{} of memory", needs::show_size(b)));
            format!("{}, {}/{}, {cpus}, {mem}", m.name, m.facts.os, m.facts.arch)
        });
        Answer::Never { need, nearest }
    }

    /// Whether another runner is sure to finish `t` before an offer from `runner` would: its
    /// latest start (in this node's clock) plus its predicted run there, before now plus the
    /// predicted run here. Unknown predictions are never sure.
    fn sooner_elsewhere(&self, t: Tid, runner: Rid, predicted: Option<Ms>, now: i64) -> bool {
        let Some(here) = predicted.map(|p| later(now, p)) else { return false };
        let Some(tk) = self.t(t) else { return false };
        tk.words.iter().any(|(o, w)| match w {
            Word::Waiting { start: Estimate { latest: Some(l), .. }, predicted: Some(p), .. } if *o != runner => later(self.owner_time(*o, *l), *p) < here,
            _ => false,
        })
    }

    /// The offer to take of those held: the emptiest by fullness, then by class the most room or
    /// the least, then by name.
    fn best(&self, tk: &Ticket) -> Option<Offer> {
        let room = wants_room(tk.ask.class);
        let size = |r: Rid| self.machine(r).map(|m| m.size).unwrap_or_default();
        let key = |o: &Offer| {
            let f = fullness(&size(o.runner), &o.free);
            let free = (o.free.cpu.get(), o.free.mem, o.free.disk, o.free.gpu);
            (f, free)
        };
        tk.offers.iter().copied().min_by(|a, b| {
            let ((fa, ra), (fb, rb)) = (key(a), key(b));
            fa.cmp(&fb).then_with(|| if room { rb.cmp(&ra) } else { ra.cmp(&rb) }).then_with(|| self.names.name(a.runner).cmp(self.names.name(b.runner)))
        })
    }

    /// Takes the best offer held and declines the others (one each, at most d − 1); the probes
    /// still out are withdrawn.
    fn choose_now(&mut self, t: Tid, now: i64, out: &mut Vec<Cmd>) {
        let _ = now;
        let Some(best) = self.t(t).and_then(|tk| self.best(tk)) else { return };
        let Some(tk) = self.t_mut(t) else { return };
        tk.stage = Stage::Taking { fence: best.fence };
        tk.runner = Some(best.runner);
        tk.got = Some((best.got, best.free));
        tk.offers.retain(|o| o.runner != best.runner);
        let run = tk.run.clone();
        out.push(Cmd::Tell { to: best.runner, run, msg: Down::Take { fence: best.fence } });
        self.withdraw_others(t, Some(best.runner), out);
        if let Some(tk) = self.t_mut(t) {
            tk.offers.clear();
        }
        self.unhold_if_none(t);
        self.set_word(t, best.runner, Word::Offered);
        self.undue(t);
    }
}

// The inputs: the directory, the asker, the runners' words, the links and the time.
impl Owner {
    /// The runners as the directory has them now. One no longer in it is gone: its claims end, and
    /// a run there that may have started is lost. One whose epoch changed started again: claims of
    /// its old epoch end, and a start sent to it is said again for its record to answer. One whose
    /// link went down is waited for; back, what it may have missed is said again. One new, or newly
    /// placing, is probed with each waiting run it could take (idle runners take waiting work), and
    /// an older one newly idle is tried for waiting runs no runner that places offered.
    pub fn machines_c(&mut self, ms: Vec<Machine>, out: &mut Vec<Cmd>) {
        let old: BTreeMap<String, (Rid, Machine)> = self.present.iter().filter_map(|r| self.slots.get_mut(ix(*r)).and_then(|s| s.machine.take()).map(|m| (m.name.clone(), (*r, m)))).collect();
        let mut new: BTreeMap<String, Rid> = BTreeMap::new();
        for m in ms {
            let r = self.rid(&m.name);
            new.insert(m.name.clone(), r);
            if let Some(s) = self.slots.get_mut(ix(r)) {
                s.machine = Some(m);
            }
        }
        self.present = new.values().copied().collect();
        self.redraw();
        for (r, m) in old.values() {
            let change = match self.machine(*r) {
                None => Change::Gone,
                Some(n) if n.epoch != m.epoch => Change::Restarted,
                Some(n) if m.connected && !n.connected => Change::Away,
                Some(n) if !m.connected && n.connected => Change::Back,
                Some(_) => Change::Same,
            };
            match change {
                Change::Gone => {
                    self.unhint(*r);
                    self.gone(*r, out);
                }
                Change::Restarted => {
                    self.unhint(*r);
                    self.restarted(*r, out);
                }
                Change::Away => self.went_away(*r, out),
                Change::Back => self.came_back(*r, out),
                Change::Same => {}
            }
        }
        // Runners new to the directory, or newly placing (a runner started again is `restarted`'s):
        // each waiting run it could take is probed there, or, its link down, waits for it.
        let fresh: Vec<(Rid, bool)> = self.present.iter().filter_map(|r| self.machine(*r).map(|m| (*r, m))).filter(|(_, m)| m.places && old.get(&m.name).is_none_or(|(_, o)| !o.places)).map(|(r, m)| (r, m.connected)).collect();
        if !fresh.is_empty() {
            let waiting: Vec<Tid> = self.waiting();
            for t in waiting {
                for (r, connected) in &fresh {
                    let probe = match (self.t(t), self.machine(*r)) {
                        (Some(tk), Some(m)) => tk.stage == Stage::Waiting && !matches!(tk.word(*r), Some(Word::Asked | Word::Waiting { .. } | Word::Offered | Word::Never(_))) && unsuited(m, &tk.ask).is_none(),
                        _ => false,
                    };
                    if probe {
                        if let Some(tk) = self.t_mut(t) {
                            tk.gone.retain(|(g, _)| g != r);
                        }
                        if *connected {
                            self.probe_at(t, *r, out);
                        } else {
                            self.set_word(t, *r, Word::Away);
                        }
                        self.heard_of(t, out);
                    }
                }
            }
        }
        // An older runner new, back, or idle otherwise than it was: waiting runs are tried there.
        let older = self.present.iter().filter_map(|r| self.machine(*r)).any(|m| !m.places && old.get(&m.name).is_none_or(|(_, o)| o.places || o.idle != m.idle || o.connected != m.connected));
        if older {
            self.retry_older(out);
        }
    }

    /// The tickets waiting, in order.
    fn waiting(&self) -> Vec<Tid> {
        self.tickets.iter().enumerate().filter_map(|(i, t)| t.as_ref().filter(|t| t.stage == Stage::Waiting).and_then(|_| u32::try_from(i).ok())).collect()
    }

    /// Each waiting run no runner that places has offered, tried on the older runners again.
    fn retry_older(&mut self, out: &mut Vec<Cmd>) {
        let waiting: Vec<Tid> = self.waiting().into_iter().filter(|t| self.t(*t).is_some_and(|tk| tk.due.is_none())).collect();
        for t in waiting {
            self.try_older_for(t, out);
        }
    }

    /// A ticket's stage, whether `r` holds its claim, and that runner's word on it.
    fn where_is(&self, t: Tid, r: Rid) -> Option<(Stage, bool, Option<Word>)> {
        let tk = self.t(t)?;
        Some((tk.stage, tk.runner == Some(r), tk.word(r).cloned()))
    }

    fn back_to_waiting(&mut self, t: Tid) {
        let had = self.t(t).is_some_and(|tk| matches!(tk.stage, Stage::Starting { .. } | Stage::Running { .. }));
        if let Some(tk) = self.t_mut(t) {
            tk.stage = Stage::Waiting;
            tk.runner = None;
            tk.got = None;
            tk.unsynced = false;
        }
        if had {
            self.keep(t);
        }
    }

    /// `r` is gone from the directory: what it held ends.
    fn gone(&mut self, r: Rid, out: &mut Vec<Cmd>) {
        self.fast_leave(r);
        for t in self.runs_at(r) {
            let Some((stage, here, _)) = self.where_is(t, r) else { continue };
            match stage {
                Stage::Starting { .. } | Stage::Running { .. } if here => {
                    {
                        self.finish(t, Answer::Lost { runner: self.name(r), why: "is gone from the directory, as the run started or ran there: it may have run, and it isn't started again".into() }, out);
                    }
                }
                Stage::Taking { .. } | Stage::Held { .. } if here => {
                    self.back_to_waiting(t);
                    self.drop_word(t, r);
                    self.probe_afresh(t, out);
                    self.answer(t, out);
                }
                _ => {
                    self.drop_word(t, r);
                    if let Some(tk) = self.t_mut(t) {
                        tk.gone.retain(|(g, _)| *g != r);
                        tk.gone.push((r, "gone".into()));
                    }
                    self.unpass(t, out);
                    self.settle(t, 0, out);
                }
            }
        }
    }

    /// `r` started again under a new epoch: its claims of the old one end; a start sent to it is
    /// said again, for the record on its disk to answer (it ran, how it ended, or the claim
    /// lapsed); a run there under the old epoch is lost; its probes are made again.
    fn restarted(&mut self, r: Rid, out: &mut Vec<Cmd>) {
        let connected = self.connected(r);
        let syncs = self.machine(r).is_some_and(|m| m.syncs);
        for t in self.runs_at(r) {
            let Some((stage, here, word)) = self.where_is(t, r) else { continue };
            match stage {
                // Its record on its disk says what it started (LB03): asked, never said again.
                Stage::Running { .. } | Stage::Starting { .. } if here && syncs => {
                    if let Some(tk) = self.t_mut(t) {
                        tk.unsynced = true;
                    }
                    self.sync_due.insert(r);
                }
                Stage::Running { .. } if here => {
                    self.finish(t, Answer::Lost { runner: self.name(r), why: "started again under a new epoch as the run ran there: it isn't started again".into() }, out);
                }
                Stage::Starting { fence } if here => {
                    if connected && let Some(run) = self.t(t).map(|tk| tk.run.clone()) {
                        out.push(Cmd::Restart { run, to: r, fence });
                    }
                }
                Stage::Taking { .. } | Stage::Held { .. } if here => {
                    self.back_to_waiting(t);
                    self.probe_afresh(t, out);
                    self.answer(t, out);
                }
                // Its probe went with the old process (an offer held with it): made again, or, its
                // link down, when it's back.
                Stage::Waiting if !matches!(word, Some(Word::Never(_))) => {
                    self.unhold(t, r);
                    if connected {
                        self.probe_at(t, r, out);
                    } else {
                        self.set_word(t, r, Word::Away);
                    }
                    self.settle(t, 0, out);
                }
                _ => {}
            }
        }
        if connected && self.sync_due.contains(&r) && self.pending.is_empty() {
            self.sync_with(r, out);
        }
    }

    /// The link to `r` went down: it's still there, and what it holds waits for it. An offer held
    /// from it lapses there.
    fn went_away(&mut self, r: Rid, out: &mut Vec<Cmd>) {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::LostOnDisconnect) {
            self.gone(r, out);
            return;
        }
        for t in self.runs_at(r) {
            let Some((stage, here, word)) = self.where_is(t, r) else { continue };
            match stage {
                Stage::Waiting if !matches!(word, Some(Word::Never(_))) => {
                    self.unhold(t, r);
                    self.set_word(t, r, Word::Away);
                    self.unpass(t, out);
                    self.settle(t, 0, out);
                }
                #[cfg(test)]
                Stage::Starting { .. } if here && super::mutant() == Some(super::Mutant::ReplaceOnSilence) => {
                    self.back_to_waiting(t);
                    self.drop_word(t, r);
                    self.probe_afresh(t, out);
                }
                _ => {
                    let _ = here;
                }
            }
        }
    }

    /// The link to `r` is back: what it may have missed is said again: a waiting ticket's probe
    /// (answered as it stands), a take not yet held, and a start (said again, it starts nothing
    /// twice). A waiting ticket probed at fewer than [`PROBES`] runners that may yet start it (they
    /// were away, or said never) is probed there.
    fn came_back(&mut self, r: Rid, out: &mut Vec<Cmd>) {
        for t in self.runs_at(r) {
            let Some((stage, here, word)) = self.where_is(t, r) else { continue };
            let Some(run) = self.t(t).map(|tk| tk.run.clone()) else { continue };
            match stage {
                Stage::Waiting if matches!(word, Some(Word::Away | Word::Asked | Word::Passed)) => {
                    self.probe_at(t, r, out);
                    self.heard_of(t, out);
                }
                Stage::Taking { fence } if here => out.push(Cmd::Tell { to: r, run, msg: Down::Take { fence } }),
                // A runner that answers `Sync` is asked what it started (LB03): the start is never
                // said again, so one it never had is placed again only on its record's word.
                Stage::Starting { .. } | Stage::Running { .. } if here && self.machine(r).is_some_and(|m| m.syncs) => {
                    self.sync_due.insert(r);
                }
                Stage::Starting { fence } if here => out.push(Cmd::Restart { run, to: r, fence }),
                _ => {}
            }
        }
        if self.sync_due.contains(&r) && self.pending.is_empty() {
            self.sync_with(r, out);
        }
        let Some(m) = self.machine(r).filter(|m| m.places).cloned() else { return };
        let hungry: Vec<Tid> = self.waiting().into_iter().filter(|t| self.t(*t).is_some_and(|tk| tk.short() && tk.word(r).is_none() && unsuited(&m, &tk.ask).is_none())).collect();
        for t in hungry {
            self.probe_at(t, r, out);
            self.heard_of(t, out);
        }
    }

    /// `runner`'s published room (the `offers` named feature, docs/needs.md §11): what it had
    /// free as it said so, `seq` its own count of what it published (one not later than the last
    /// is ignored). A hint, never a claim: only an offer holds room. Read when a run's probed
    /// runners all say it waits ([`PROBES`] more are drawn among those hinting room for it).
    /// Runner `runner`'s pace as its latest word said it (W05-R26): unmeasured for one that never
    /// said one, or isn't known.
    pub fn pace(&self, runner: &str) -> Pace {
        self.names.get(runner).and_then(|r| self.slots.get(ix(r))).map(|s| s.pace).unwrap_or_default()
    }

    pub fn hint(&mut self, runner: &str, free: Resources, seq: u64) {
        let r = self.rid(runner);
        self.hint_c(r, free, seq);
    }

    /// [`Self::hint`], by handle.
    pub fn hint_c(&mut self, r: u32, free: Resources, seq: u64) {
        while self.slots.len() <= ix(r) {
            self.slots.push(Slot::default());
        }
        let Some(s) = self.slots.get_mut(ix(r)) else { return };
        if s.hint.is_some_and(|(last, _)| last >= seq) {
            return;
        }
        s.hint = Some((seq, free));
        let room = super::ask::amounts(&free).iter().any(|a| *a > 0);
        match (room, s.roomy_at) {
            (true, None) => {
                s.roomy_at = u32::try_from(self.roomy.len()).ok();
                self.roomy.push(r);
            }
            (false, Some(_)) => self.unroomy(r),
            _ => {}
        }
    }

    /// `r` no longer hints room: out of `roomy`, the last in its place (O(1)).
    fn unroomy(&mut self, r: Rid) {
        let Some(at) = self.slots.get_mut(ix(r)).and_then(|s| s.roomy_at.take()) else { return };
        self.roomy.swap_remove(ix(at));
        if let Some(&moved) = self.roomy.get(ix(at))
            && let Some(s) = self.slots.get_mut(ix(moved))
        {
            s.roomy_at = Some(at);
        }
    }

    fn roomy_swap(&mut self, i: usize, j: usize) {
        self.roomy.swap(i, j);
        for k in [i, j] {
            if let Some(&r) = self.roomy.get(k)
                && let Some(s) = self.slots.get_mut(ix(r))
            {
                s.roomy_at = u32::try_from(k).ok();
            }
        }
    }

    /// A runner's hint is no longer its own (it's gone, or started again).
    fn unhint(&mut self, r: Rid) {
        if let Some(s) = self.slots.get_mut(ix(r)) {
            s.hint = None;
        }
        self.unroomy(r);
    }

    /// The link to `runner` went down (the shell's word, between directories).
    pub fn away_c(&mut self, runner: &str, out: &mut Vec<Cmd>) {
        let Some(r) = self.names.get(runner) else { return };
        if let Some(m) = self.slots.get_mut(ix(r)).and_then(|s| s.machine.as_mut())
            && m.connected
        {
            m.connected = false;
            self.redraw();
            self.went_away(r, out);
        }
    }

    /// The link to `runner` is back (the shell's word, between directories).
    pub fn back_c(&mut self, runner: &str, out: &mut Vec<Cmd>) {
        let Some(r) = self.names.get(runner) else { return };
        if let Some(m) = self.slots.get_mut(ix(r)).and_then(|s| s.machine.as_mut())
            && !m.connected
        {
            m.connected = true;
            let older = !m.places;
            self.redraw();
            self.came_back(r, out);
            if older {
                self.retry_older(out);
            }
        }
    }

    /// A run is asked of this node: answered never at once if no runner could run it even idle,
    /// handed to the older pick if only older runners could, else probed at [`PROBES`] runners
    /// drawn among those that could, are linked and place; its first answer comes once each has
    /// answered, or by the last of their links' timeouts.
    pub fn ask_c(&mut self, ask: Ask, now: i64, out: &mut Vec<Cmd>) {
        self.ask_in(ask, now, true, out);
    }

    /// [`Self::ask_c`]; `kernel`: whether the kernel may start it at once. A run kept from before a
    /// restart may not: runners may hold it from before, and a hand-off may have claimed it while
    /// this owner was down, so it reaches new runners only through probing, with their members'
    /// word (LB03).
    fn ask_in(&mut self, ask: Ask, now: i64, kernel: bool, out: &mut Vec<Cmd>) {
        if self.by_run.contains_key(ask.run.as_str()) {
            return;
        }
        let suit = self.suit(&ask);
        if suit.live.is_empty() && suit.away.is_empty() && suit.older.is_empty() {
            let run: Arc<str> = Arc::from(ask.run.as_str());
            if self.keeping {
                self.dirty.insert(Arc::clone(&run));
            }
            out.push(Cmd::Answer { run, answer: self.never(&ask) });
            return;
        }
        let run: Arc<str> = Arc::from(ask.run.as_str());
        let tk = Ticket {
            run: Arc::clone(&run),
            ask: Arc::new(ask),
            suit: Arc::clone(&suit),
            stage: Stage::Waiting,
            runner: None,
            got: None,
            words: Vec::new(),
            gone: Vec::new(),
            offers: Vec::new(),
            choose_at: None,
            told: None,
            due: None,
            epoch: self.epoch,
            unsynced: false,
            confirmed: false,
            #[cfg(test)]
            second: None,
            fast: None,
        };
        let t = match self.spare.pop() {
            Some(t) => {
                if let Some(slot) = self.tickets.get_mut(ix(t)) {
                    *slot = Some(tk);
                }
                t
            }
            None => {
                self.tickets.push(Some(tk));
                u32::try_from(self.tickets.len().saturating_sub(1)).unwrap_or(u32::MAX)
            }
        };
        self.by_run.insert(run, t);
        self.keep(t);
        // The kernel first: an ordinary ask a member has room for starts there now (docs/lb.md).
        if kernel && self.fast_place(t, out) {
            return;
        }
        self.probe_new(t, now, out);
    }

    /// Ticket `t`, waiting with no probe out: handed to the older pick if only older runners could
    /// take it, else probed at [`PROBES`] runners drawn among those that could, are linked and
    /// place; its first answer comes once each has answered, or by the last of their links'
    /// timeouts.
    fn probe_new(&mut self, t: Tid, now: i64, out: &mut Vec<Cmd>) {
        let Some(suit) = self.t(t).map(|tk| Arc::clone(&tk.suit)) else { return };
        if suit.live.is_empty() && suit.away.is_empty() {
            let older = suit.older.iter().map(|r| self.name(*r)).collect();
            self.fallback(t, older, out);
            return;
        }
        let n = u32::try_from(suit.live.len()).unwrap_or(u32::MAX);
        let mut draw = std::mem::take(&mut self.draw);
        self.rng.sample(n, PROBES, &mut draw);
        let mut due: Option<i64> = None;
        for i in &draw {
            let Some(r) = suit.live.get(ix(*i)).copied() else { continue };
            self.probe_at(t, r, out);
            let at = later(now, self.rto(r));
            due = Some(due.map_or(at, |d| d.max(at)));
        }
        self.draw = draw;
        match due {
            Some(d) => {
                if let Some(tk) = self.t_mut(t) {
                    tk.due = Some(d);
                }
                self.due.insert((d, t));
            }
            // Only runners whose link is down could take it: answered so now.
            None => self.answer(t, out),
        }
    }

    /// Runner `r`'s word about a run, by handle: the run by the token this owner gave it (0: none,
    /// found by its id), as the in-process path carries it ([`super::runner::Reply`]).
    pub fn heard_c(&mut self, r: u32, token: u64, run_id: &Arc<str>, msg: Up, now: i64, out: &mut Vec<Cmd>) {
        if matches!(msg, Up::FastStarted { .. } | Up::FastDeclined { .. } | Up::FastEnded { .. }) {
            self.fast_heard(r, run_id, msg, now, out);
            return;
        }
        if let Up::Synced { since, .. } = msg {
            self.synced(r, since, now, out);
            return;
        }
        let found = if token == 0 { None } else { self.by_token(token) };
        let Some(t) = found.or_else(|| self.by_run.get(run_id.as_ref()).copied()) else {
            // Its ticket is gone (ended, cancelled, answered never): an offer's room goes back, a
            // probe still waiting there is withdrawn, and an end is acknowledged. O(1).
            let run = Arc::clone(run_id);
            match msg {
                Up::Offer { fence, .. } | Up::Held { fence } => out.push(Cmd::Tell { to: r, run, msg: Down::Decline { fence } }),
                Up::Waiting { .. } => out.push(Cmd::Tell { to: r, run, msg: Down::Withdraw }),
                Up::Ended { .. } => out.push(Cmd::Tell { to: r, run, msg: Down::Ack }),
                _ => {}
            }
            return;
        };
        while self.slots.len() <= ix(r) {
            self.slots.push(Slot::default());
        }
        if self.syncing.contains(&r)
            && let Some(tk) = self.t_mut(t)
            && tk.runner == Some(r)
            && matches!(msg, Up::Started { .. } | Up::Ended { .. } | Up::Ran)
        {
            tk.confirmed = true;
        }
        let Some(tk) = self.t(t) else { return };
        let stage = tk.stage;
        let here = tk.runner == Some(r);
        let probed = tk.word(r).is_some();
        let got = tk.got;
        let run = tk.run.clone();
        #[cfg(test)]
        let second = tk.second.is_some_and(|(s, _)| s == r);
        if let (Up::Offer { pace, .. } | Up::Waiting { pace, .. }, Some(slot)) = (&msg, self.slots.get_mut(ix(r))) {
            slot.pace = *pace;
        }
        match msg {
            Up::Offer { fence, got: g, free, predicted, .. } => match stage {
                Stage::Waiting => {
                    if self.sooner_elsewhere(t, r, predicted, now) {
                        // Another runner is sure to finish it sooner: this one's room goes back, and
                        // it's probed again when what's known of the others changes.
                        self.unhold(t, r);
                        out.push(Cmd::Tell { to: r, run, msg: Down::Decline { fence } });
                        self.set_word(t, r, Word::Passed);
                        self.heard_of(t, out);
                        return;
                    }
                    if self.t(t).and_then(|tk| tk.offer_of(r)) == Some(fence) {
                        return;
                    }
                    let at = later(now, self.hold(r));
                    let Some(tk) = self.t_mut(t) else { return };
                    tk.offers.retain(|o| o.runner != r);
                    tk.offers.push(Offer { runner: r, fence, got: g, free });
                    // The first offer starts the hold.
                    if tk.choose_at.is_none() {
                        tk.choose_at = Some(at);
                        self.choose.insert((at, t));
                    }
                    self.set_word(t, r, Word::Offered);
                    // Its next answer is where it starts, or, the offers lapsing, where it waits.
                    self.undue(t);
                    if self.all_answered(t) {
                        self.choose_now(t, now, out);
                    }
                }
                // An older runner is being tried: the offer lapses back into that runner's line.
                Stage::Fallback => {}
                Stage::Taking { fence: f } | Stage::Held { fence: f } | Stage::Starting { fence: f } | Stage::Running { fence: Some(f) } if here && f == fence => {}
                #[cfg(test)]
                _ if super::mutant() == Some(super::Mutant::TakeTwo) && self.t(t).is_some_and(|tk| tk.second.is_none()) => {
                    if let Some(tk) = self.t_mut(t) {
                        tk.second = Some((r, fence));
                    }
                    out.push(Cmd::Tell { to: r, run, msg: Down::Take { fence } });
                }
                _ => out.push(Cmd::Tell { to: r, run, msg: Down::Decline { fence } }),
            },
            Up::Held { fence } => match stage {
                Stage::Taking { fence: f } if f == fence && here => {
                    if let Some(tk) = self.t_mut(t) {
                        tk.stage = Stage::Held { fence };
                    }
                    let name = self.name(r);
                    out.push(Cmd::Go { run, to: r, fence });
                    let (g, free) = got.unwrap_or_default();
                    #[cfg(test)]
                    if super::mutant() == Some(super::Mutant::StartedAtHeld) {
                        self.tell(t, Answer::Started { runner: name, got: g, why: format!("room for it there: {} free", words(&free)) }, out);
                        return;
                    }
                    let _ = free;
                    self.tell(t, Answer::Starting { runner: name, got: g }, out);
                }
                Stage::Held { fence: f } | Stage::Starting { fence: f } if f == fence && here => {}
                #[cfg(test)]
                _ if second => {
                    if let Some(tk) = self.t_mut(t) {
                        tk.second = None;
                        tk.stage = Stage::Held { fence };
                        tk.runner = Some(r);
                    }
                    out.push(Cmd::Go { run, to: r, fence });
                }
                _ => out.push(Cmd::Tell { to: r, run, msg: Down::Decline { fence } }),
            },
            // The runner's word that the process started: the one on which the asker is told so.
            Up::Started { fence } => {
                let unsynced = self.t(t).is_some_and(|tk| tk.unsynced);
                if matches!(stage, Stage::Starting { fence: f } | Stage::Held { fence: f } if f == fence) && here {
                    if let Some(tk) = self.t_mut(t) {
                        tk.stage = Stage::Running { fence: Some(fence) };
                    }
                    self.charge_started(t, r, now);
                    self.keep(t);
                    let (g, free) = got.unwrap_or_default();
                    self.tell(t, Answer::Started { runner: self.name(r), got: g, why: format!("room for it there: {} free", words(&free)) }, out);
                } else if unsynced && here && matches!(stage, Stage::Running { fence: f } if f.is_none_or(|f| f == fence)) {
                    // Kept across a restart, and its runner says it runs: its asker hears so again, and
                    // its tenant's usage, gone with the restart, counts it from now (W05-R32).
                    if let Some(tk) = self.t_mut(t) {
                        tk.stage = Stage::Running { fence: Some(fence) };
                    }
                    self.keep(t);
                    self.charge_started(t, r, now);
                    let (g, _) = got.unwrap_or_default();
                    self.tell(t, Answer::Started { runner: self.name(r), got: g, why: "it runs there, as its runner said after a restart".to_owned() }, out);
                }
            }
            // It started there before: it runs there, and nowhere else.
            Up::Ran => {
                if matches!(stage, Stage::Running { .. }) {
                    return;
                }
                if let (Stage::Taking { fence } | Stage::Held { fence }, false) = (stage, here)
                    && let Some(w) = self.t(t).and_then(|tk| tk.runner)
                {
                    self.drop_word(t, w);
                    out.push(Cmd::Tell { to: w, run: Arc::clone(&run), msg: Down::Decline { fence } });
                }
                if let Some(tk) = self.t_mut(t) {
                    tk.stage = Stage::Running { fence: None };
                    tk.runner = Some(r);
                }
                self.charge_started(t, r, now);
                self.keep(t);
                self.unhold(t, r);
                self.withdraw_others(t, Some(r), out);
                self.set_word(t, r, Word::Offered);
                self.undue(t);
                let (g, free) = got.unwrap_or_default();
                self.tell(t, Answer::Started { runner: self.name(r), got: g, why: format!("it started there before ({} free as it was offered)", words(&free)) }, out);
            }
            Up::Ended { ok, fence } => {
                // Its one result (LB03): taken only from the runner its start went to, under that
                // claim; any other end (a stale record, a duplicate) is let go, never taken.
                let ours = here && (matches!(stage, Stage::Starting { fence: f } | Stage::Held { fence: f } if f == fence) || matches!(stage, Stage::Running { fence: f } if f.is_none_or(|f| f == fence)));
                #[cfg(test)]
                let ours = ours || super::mutant() == Some(super::Mutant::AcceptAnyEnd);
                if !ours {
                    out.push(Cmd::Tell { to: r, run, msg: Down::Ack });
                    return;
                }
                self.withdraw_others(t, Some(r), out);
                self.usage.ended(&run, now);
                self.finish(t, Answer::Ended { runner: self.name(r), ok }, out);
                out.push(Cmd::Tell { to: r, run, msg: Down::Ack });
            }
            Up::Waiting { reason, place, start, predicted, free, .. } => {
                if stage == Stage::Waiting && probed {
                    // An offer held from it lapsed back into its line.
                    self.unhold(t, r);
                    self.set_word(t, r, Word::Waiting { reason, place, start, predicted, free });
                    self.unpass(t, out);
                    self.settle(t, now, out);
                }
            }
            Up::Never { why } => {
                if probed {
                    self.unhold(t, r);
                    self.set_word(t, r, Word::Never(why));
                }
                if stage != Stage::Waiting {
                    return;
                }
                if self.t(t).is_some_and(Ticket::hungry) {
                    self.widen(t, out);
                }
                // Every runner that places and suits it said never: older ones, or never.
                let Some(ask) = self.t(t).map(|tk| Arc::clone(&tk.ask)) else { return };
                let suit = self.suit(&ask);
                let all_never = self.t(t).is_some_and(|tk| !tk.words.is_empty() && tk.words.iter().all(|(_, w)| matches!(w, Word::Never(_))) && suit.live.iter().chain(&suit.away).all(|r| tk.word(*r).is_some()));
                if all_never && !suit.older.is_empty() {
                    let older = suit.older.iter().map(|r| self.name(*r)).collect();
                    self.fallback(t, older, out);
                } else if all_never {
                    let Some(tk) = self.remove(t) else { return };
                    let need = tk.words.iter().filter_map(|(_, w)| if let Word::Never(why) = w { Some(why.clone()) } else { None }).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("; ");
                    let nearest = match self.never(&tk.ask) {
                        Answer::Never { nearest, .. } => nearest,
                        _ => None,
                    };
                    out.push(Cmd::Answer { run, answer: Answer::Never { need: format!("takes it as asked ({need})"), nearest } });
                } else {
                    self.unpass(t, out);
                    self.settle(t, now, out);
                }
            }
            // The runner's word that the claim isn't live there and the run wasn't started under
            // it: the one word that sends a ticket past waiting back to waiting. The asker, told it
            // was starting there, is told where it stands instead. An offer held lapsed: it waits
            // there again, its place told next.
            // The kernel's words were heard above.
            Up::FastStarted { .. } | Up::FastDeclined { .. } | Up::FastEnded { .. } => {}
            // Handled before its ticket is looked up: it's about no run.
            Up::Synced { .. } => {}
            Up::Lapsed { fence, .. } => {
                if stage == Stage::Waiting && self.t(t).and_then(|tk| tk.offer_of(r)) == Some(fence) {
                    self.unhold(t, r);
                    self.set_word(t, r, Word::Asked);
                    self.settle(t, now, out);
                } else if matches!(stage, Stage::Taking { fence: f } | Stage::Held { fence: f } | Stage::Starting { fence: f } if f == fence) && here {
                    self.back_to_waiting(t);
                    self.probe_afresh(t, out);
                    self.answer(t, out);
                }
            }
        }
    }

    /// The shell is about to start a run on `runner` under `fence`, its tree copied: whether it
    /// still may (its runner holds it under that claim). Once this says yes, the run's start has
    /// gone to that runner, and goes nowhere else unless it says it never started it.
    /// Whether the shell keeps this owner's hard state (LB03): from then on each change to a run's
    /// [`Kept`] is noted for [`Self::kept`], and an answer that ends a run is owed until
    /// [`Self::delivered`].
    pub fn keeping(&mut self, on: bool) {
        self.keeping = on;
    }

    fn keep(&mut self, t: Tid) {
        if self.keeping
            && let Some(tk) = self.t(t)
        {
            self.dirty.insert(Arc::clone(&tk.run));
        }
    }

    fn attempt_of(&self, t: Tid) -> Option<Attempt> {
        let tk = self.t(t)?;
        let runner = self.name(tk.runner?);
        match tk.stage {
            Stage::Starting { fence } => Some(Attempt { runner, fence: Some(fence), started: false }),
            Stage::Running { fence } => Some(Attempt { runner, fence, started: true }),
            _ => None,
        }
    }

    fn kept_of(&self, t: Tid) -> Option<Kept> {
        let tk = self.t(t)?;
        Some(Kept { ask: (*tk.ask).clone(), epoch: tk.epoch, attempt: self.attempt_of(t), done: None })
    }

    /// Its result (LB03): the ticket goes, its asker is answered, and, kept, the answer is owed
    /// until it's told ([`Self::delivered`]), so a restart tells it again and never takes a second.
    fn finish(&mut self, t: Tid, answer: Answer, out: &mut Vec<Cmd>) {
        let kept = if self.keeping { self.kept_of(t).map(|k| Kept { done: Some(answer.clone()), ..k }) } else { None };
        let Some(tk) = self.remove(t) else { return };
        if let Some(k) = kept {
            self.owed.insert(Arc::clone(&tk.run), k);
        }
        out.push(Cmd::Answer { run: tk.run, answer });
    }

    /// The runs whose hard state changed since this was last asked, each as it is now (`None`: it
    /// goes), for the shell to keep on its disk in one transaction before it acts on any of the
    /// commands that came with the change (LB03: nothing is published before its durable point).
    pub fn kept(&mut self) -> Vec<(Arc<str>, Option<Kept>)> {
        let dirty = std::mem::take(&mut self.dirty);
        dirty
            .into_iter()
            .map(|run| {
                let k = match self.by_run.get(&run).copied() {
                    Some(t) => self.kept_of(t),
                    None => self.owed.get(&run).cloned(),
                };
                (run, k)
            })
            .collect()
    }

    /// The answer that ended `run` was told its asker: it's owed no more.
    pub fn delivered(&mut self, run: &str) {
        if let Some((k, _)) = self.owed.remove_entry(run) {
            self.dirty.insert(k);
        }
    }

    /// What this owner's process before this one kept (LB03), taken up in this one's epoch: each
    /// answer still owed is told again (re-delivered: its asker may never have had it); the rest
    /// are asked again once the directory is known ([`Self::reask_c`]).
    pub fn restore_c(&mut self, kept: Vec<Kept>, out: &mut Vec<Cmd>) {
        self.keeping = true;
        for k in kept {
            let run: Arc<str> = Arc::from(k.ask.run.as_str());
            if let Some(a) = k.done.clone() {
                out.push(Cmd::Answer { run: Arc::clone(&run), answer: a });
                self.owed.insert(run, k);
                continue;
            }
            self.sync_all = true;
            self.pending.push(k);
        }
    }

    /// The directory is known: the runs kept from before a restart are taken up. One whose start
    /// went is held where it went, unreconciled, until that runner says what it started (`Sync`);
    /// one whose runner is gone, or can't say, is lost (or placed again if it's safe to repeat);
    /// the rest are asked again, in the epoch they were asked in. Then every runner that answers
    /// `Sync` is asked it once, so the claims this owner's earlier process left there go.
    pub fn reask_c(&mut self, now: i64, out: &mut Vec<Cmd>) {
        for k in std::mem::take(&mut self.pending) {
            let epoch = k.epoch;
            let run: Arc<str> = Arc::from(k.ask.run.as_str());
            match k.attempt {
                None => {
                    self.ask_in(k.ask, now, false, out);
                    if let Some(tk) = self.by_run.get(&run).copied().and_then(|t| self.t_mut(t)) {
                        tk.epoch = epoch;
                    }
                }
                Some(a) => self.readmit(k.ask, epoch, a, now, out),
            }
        }
        self.sync_pending(out);
    }

    /// A run kept with its attempt, held at that attempt's runner until it says where it stands.
    fn readmit(&mut self, ask: Ask, epoch: u64, a: Attempt, now: i64, out: &mut Vec<Cmd>) {
        if self.by_run.contains_key(ask.run.as_str()) {
            return;
        }
        let r = self.rid(&a.runner);
        let stage = match (a.started, a.fence) {
            (false, Some(fence)) => Stage::Starting { fence },
            (_, fence) => Stage::Running { fence },
        };
        let run: Arc<str> = Arc::from(ask.run.as_str());
        let suit = self.suit(&ask);
        let tk = Ticket {
            run: Arc::clone(&run),
            ask: Arc::new(ask),
            suit,
            stage,
            runner: Some(r),
            fast: None,
            got: None,
            words: vec![(r, Word::Offered)],
            gone: Vec::new(),
            offers: Vec::new(),
            choose_at: None,
            told: None,
            due: None,
            epoch,
            unsynced: true,
            confirmed: false,
            #[cfg(test)]
            second: None,
        };
        let t = match self.spare.pop() {
            Some(t) => {
                if let Some(slot) = self.tickets.get_mut(ix(t)) {
                    *slot = Some(tk);
                }
                t
            }
            None => {
                self.tickets.push(Some(tk));
                u32::try_from(self.tickets.len().saturating_sub(1)).unwrap_or(u32::MAX)
            }
        };
        self.by_run.insert(run, t);
        self.note(r, t);
        match self.machine(r).map(|m| m.syncs) {
            Some(true) => {
                self.sync_due.insert(r);
            }
            // Gone from the directory, or a runner that can't say what it started.
            Some(false) => self.unknown(t, r, now, out),
            None => {
                if let Some(tk) = self.t_mut(t) {
                    tk.unsynced = false;
                }
                self.finish(t, Answer::Lost { runner: a.runner, why: "is gone from the directory, and the run's start went there before its owner started again: it may have run, and it isn't started again".to_owned() }, out);
            }
        }
    }

    /// Each runner due a `Sync` that's linked and answers it (none while kept runs wait for the
    /// directory, whose words it would come before).
    fn sync_pending(&mut self, out: &mut Vec<Cmd>) {
        if !self.pending.is_empty() {
            return;
        }
        let due: Vec<Rid> = self
            .present
            .iter()
            .copied()
            .filter(|r| self.machine(*r).is_some_and(|m| m.connected && m.places && m.syncs) && (self.sync_due.contains(r) || self.sync_all && !self.synced_once.contains(r)))
            .collect();
        for r in due {
            self.sync_with(r, out);
        }
    }

    fn sync_with(&mut self, r: Rid, out: &mut Vec<Cmd>) {
        self.sync_due.remove(&r);
        self.synced_once.insert(r);
        self.syncing.insert(r);
        for t in self.runs_at(r) {
            if let Some(tk) = self.t_mut(t) {
                tk.confirmed = false;
            }
        }
        out.push(Cmd::Tell { to: r, run: Arc::from(""), msg: Down::Sync });
    }

    /// Runner `r` said again every run of this owner's it holds a record of (LB03): each run whose
    /// start went there and that it didn't say is reconciled. It never started there if the claim
    /// is of an epoch whose records it keeps (`since` on): placed again, which retries nothing.
    /// Otherwise whether it ran isn't known: lost, unless it's safe to repeat.
    fn synced(&mut self, r: Rid, since: Option<u64>, now: i64, out: &mut Vec<Cmd>) {
        if !self.syncing.remove(&r) {
            return;
        }
        for t in self.runs_at(r) {
            let Some(tk) = self.t_mut(t) else { continue };
            let (here, confirmed, stage) = (tk.runner == Some(r), tk.confirmed, tk.stage);
            tk.confirmed = false;
            tk.unsynced = false;
            if !here || confirmed {
                continue;
            }
            match stage {
                Stage::Starting { fence } if since.is_some_and(|s| s <= fence.epoch) => {
                    self.back_to_waiting(t);
                    self.drop_word(t, r);
                    self.probe_afresh(t, out);
                    self.answer(t, out);
                }
                Stage::Starting { .. } | Stage::Running { .. } => self.unknown(t, r, now, out),
                _ => {}
            }
        }
    }

    /// A run whose start went to `r`, which can't say whether it started: lost, never started a
    /// second time, unless its asker declared it safe to repeat (LB03).
    fn unknown(&mut self, t: Tid, r: Rid, now: i64, out: &mut Vec<Cmd>) {
        let repeat = self.t(t).is_some_and(|tk| tk.ask.restartable);
        #[cfg(test)]
        let repeat = repeat || super::mutant() == Some(super::Mutant::StartAgainOnUnknown);
        let _ = now;
        if repeat {
            self.back_to_waiting(t);
            self.drop_word(t, r);
            self.probe_afresh(t, out);
            self.answer(t, out);
        } else {
            self.finish(t, Answer::Lost { runner: self.name(r), why: "its start went there, and whether it started there can't be known: it isn't started again".to_owned() }, out);
        }
    }

    /// Runner `r`'s process said its epoch: a later one than it said before is a restart.
    pub fn runner_epoch_c(&mut self, r: u32, epoch: u64, out: &mut Vec<Cmd>) {
        let Some(slot) = self.slots.get_mut(ix(r)) else { return };
        let was = slot.epoch;
        if epoch <= was {
            return;
        }
        slot.epoch = epoch;
        if was != 0 {
            self.unhint(r);
            self.restarted(r, out);
        }
    }

    pub fn starting(&mut self, run: &str, runner: &str, fence: Fence) -> bool {
        let (Some(t), Some(r)) = (self.by_run.get(run).copied(), self.names.get(runner)) else { return false };
        match self.t_mut(t) {
            Some(tk) if tk.stage == (Stage::Held { fence }) && tk.runner == Some(r) => {
                tk.stage = Stage::Starting { fence };
                self.keep(t);
                true
            }
            _ => false,
        }
    }

    /// Its start under `fence` couldn't be kept on disk (LB03): it isn't said, so it can't have
    /// started. The claim is declined and the run waits again.
    pub fn unstarted_c(&mut self, run: &str, out: &mut Vec<Cmd>) {
        let Some(t) = self.by_run.get(run).copied() else { return };
        let Some((Stage::Starting { fence }, Some(r))) = self.t(t).map(|tk| (tk.stage, tk.runner)) else { return };
        out.push(Cmd::Tell { to: r, run: Arc::from(run), msg: Down::Decline { fence } });
        self.back_to_waiting(t);
        self.drop_word(t, r);
        self.probe_afresh(t, out);
        self.answer(t, out);
    }

    /// What the start of `run` carries to its runner (W05-R32): its tenant's usage elsewhere since
    /// this owner's last word about it there ([`super::msg::Usage`]). Asked as each start is said.
    pub fn start_carry(&mut self, run: &str) -> i64 {
        let Some(t) = self.by_run.get(run).copied() else { return 0 };
        let Some((ask, run, r)) = self.t(t).and_then(|tk| tk.runner.map(|r| (Arc::clone(&tk.ask), Arc::clone(&tk.run), r))) else { return 0 };
        let size = self.size_of(r);
        self.usage.word(&run, &ask, r, &size)
    }

    /// A runner's size, as the directory has it.
    fn size_of(&self, r: Rid) -> Size {
        self.slots.get(ix(r)).and_then(|s| s.machine.as_ref()).map(|m| m.size).unwrap_or_default()
    }

    /// Ticket `t`'s run started on `r`, by its word: its tenant's usage counts it (W05-R32).
    fn charge_started(&mut self, t: Tid, r: Rid, now: i64) {
        let Some((ask, run)) = self.t(t).map(|tk| (Arc::clone(&tk.ask), Arc::clone(&tk.run))) else { return };
        let size = self.size_of(r);
        self.usage.started(&run, &ask, r, &size, now);
    }

    /// The asker doesn't want it any more: its probes are withdrawn, and a claim it took or an
    /// offer it held declined. A run starting or running is stopped by the shell, as any run is.
    pub fn cancel_c(&mut self, run: &str, out: &mut Vec<Cmd>) {
        let Some(t) = self.by_run.get(run).copied() else { return };
        let Some(tk) = self.remove(t) else { return };
        if let Some(h) = tk.fast {
            // Its start not sent yet, the kernel's room goes back at once; sent, it's ended where
            // it starts, as any run is, and the kernel hears so.
            if matches!(tk.stage, Stage::Held { .. }) {
                let _ = self.fast.lb.unsent(h);
            } else {
                self.fast.out.clear();
                let _ = self.fast.lb.cancel(h, &mut self.fast.out);
                self.fast.out.clear();
            }
            return;
        }
        if let (Stage::Taking { fence } | Stage::Held { fence }, Some(r)) = (tk.stage, tk.runner) {
            out.push(Cmd::Tell { to: r, run: Arc::clone(&tk.run), msg: Down::Decline { fence } });
        }
        for (r, w) in &tk.words {
            if tk.runner != Some(*r) && !matches!(w, Word::Never(_)) {
                let msg = match tk.offer_of(*r) {
                    Some(fence) => Down::Decline { fence },
                    None => Down::Withdraw,
                };
                out.push(Cmd::Tell { to: *r, run: Arc::clone(&tk.run), msg });
            }
        }
    }

    /// The shell's pick started the run on an older runner: it's followed as before placement,
    /// and its probes are withdrawn.
    /// Run `run` was handed off (LB03, `super::handoff`): a runner it was placed at promised `by`'s
    /// claim and refused this owner's word. Its ticket goes with no word to any runner (each would
    /// be refused), its asker is told, and nothing of it is kept to tell again: its adopter tells its
    /// agent now.
    pub fn handed_c(&mut self, run: &str, by: &str, out: &mut Vec<Cmd>) {
        let Some(t) = self.by_run.get(run).copied() else { return };
        let Some(tk) = self.remove(t) else { return };
        self.owed.remove(run);
        let why = format!("took it over: a runner it was placed at couldn't reach this one ({} asks its runners afresh if it's asked again)", self.me);
        out.push(Cmd::Answer { run: tk.run, answer: Answer::Lost { runner: by.to_owned(), why } });
    }

    /// A run handed to this owner (LB03, `super::handoff`) that runs, or ran, at `runner` under claim
    /// `fence`: held there as running, its end taken from that runner under that claim alone, as
    /// any run's is, and kept with this owner's runs. False if a ticket has its id already.
    pub fn adopt_running(&mut self, ask: Ask, runner: &str, fence: Fence) -> bool {
        if self.by_run.contains_key(ask.run.as_str()) {
            return false;
        }
        let r = self.rid(runner);
        let run: Arc<str> = Arc::from(ask.run.as_str());
        let suit = self.suit(&ask);
        let tk = Ticket {
            run: Arc::clone(&run),
            ask: Arc::new(ask),
            suit,
            stage: Stage::Running { fence: Some(fence) },
            runner: Some(r),
            got: None,
            words: vec![(r, Word::Offered)],
            gone: Vec::new(),
            offers: Vec::new(),
            choose_at: None,
            told: None,
            due: None,
            epoch: self.epoch,
            unsynced: false,
            confirmed: true,
            #[cfg(test)]
            second: None,
            fast: None,
        };
        let t = match self.spare.pop() {
            Some(t) => {
                if let Some(slot) = self.tickets.get_mut(ix(t)) {
                    *slot = Some(tk);
                }
                t
            }
            None => {
                self.tickets.push(Some(tk));
                u32::try_from(self.tickets.len().saturating_sub(1)).unwrap_or(u32::MAX)
            }
        };
        self.by_run.insert(run, t);
        self.note(r, t);
        self.keep(t);
        true
    }

    pub fn fallback_started_c(&mut self, run: &str, out: &mut Vec<Cmd>) {
        let Some(t) = self.by_run.get(run).copied() else { return };
        if self.t(t).is_some_and(|tk| tk.stage == Stage::Fallback) {
            self.withdraw_others(t, None, out);
            self.remove(t);
        }
    }

    /// The shell's pick couldn't start it on an older runner: it waits at the runners that place,
    /// its probes still in their lines (offers it let lapse meanwhile went back to their places).
    pub fn fallback_refused_c(&mut self, run: &str, out: &mut Vec<Cmd>) {
        let Some(t) = self.by_run.get(run).copied() else { return };
        if let Some(tk) = self.t_mut(t)
            && tk.stage == Stage::Fallback
        {
            tk.stage = Stage::Waiting;
            if tk.words.is_empty() {
                // Only older runners could take it: it waits for one, or for a runner that places.
                self.answer(t, out);
            } else {
                self.heard_of(t, out);
            }
        }
    }

    /// The time: choices due (an offer held its time) are made with the offers held, and first
    /// answers due (a runner probed hasn't answered within its link's timeout) are given with
    /// what's known.
    pub fn tick_c(&mut self, now: i64, out: &mut Vec<Cmd>) {
        while let Some(&(at, t)) = self.choose.first() {
            if at > now {
                break;
            }
            self.choose.remove(&(at, t));
            if let Some(tk) = self.t_mut(t) {
                tk.choose_at = None;
            }
            if self.t(t).is_some_and(|tk| tk.stage == Stage::Waiting && !tk.offers.is_empty()) {
                self.choose_now(t, now, out);
            }
        }
        while let Some(&(at, t)) = self.due.first() {
            if at > now {
                break;
            }
            self.due.remove(&(at, t));
            if let Some(tk) = self.t_mut(t) {
                tk.due = None;
            }
            self.answer(t, out);
        }
    }

    /// When it's to be told the time again ([`Self::tick`]): the soonest choice or first answer
    /// due.
    pub fn timer(&self) -> Option<i64> {
        [self.choose.first().map(|(at, _)| *at), self.due.first().map(|(at, _)| *at)].into_iter().flatten().min()
    }

    /// Where a run's ticket stands, in a word, and its runner, if it has one.
    pub fn stage(&self, run: &str) -> Option<(&'static str, Option<&str>)> {
        let t = self.by_run.get(run)?;
        self.t(*t).map(|tk| {
            let s = match tk.stage {
                Stage::Waiting => "waiting",
                Stage::Taking { .. } => "taking",
                Stage::Held { .. } => "held",
                Stage::Starting { .. } => "starting",
                Stage::Running { .. } => "running",
                Stage::Fallback => "fallback",
            };
            (s, tk.runner.map(|r| self.names.name(r)))
        })
    }

    /// What a run's asker was told last.
    pub fn told(&self, run: &str) -> Option<&Answer> {
        let t = self.by_run.get(run)?;
        self.t(*t).and_then(|tk| tk.told.as_ref())
    }

    /// What a waiting run's asker would be told now, once its first answer has been given (for
    /// `needs.explain`, and the property that every waiting asker hears each change).
    pub fn answer_now(&self, run: &str) -> Option<Answer> {
        let t = self.by_run.get(run)?;
        let tk = self.t(*t).filter(|tk| tk.stage == Stage::Waiting && tk.offers.is_empty() && tk.due.is_none() && tk.told.is_some())?;
        Some(self.queued(tk))
    }

    /// Its tickets' runs, in order.
    pub fn runs(&self) -> Vec<String> {
        let mut runs: Vec<String> = self.by_run.keys().map(|r| r.to_string()).collect();
        runs.sort_unstable();
        runs
    }

    /// The runners a run is probed at now (for tests and views).
    pub fn probed(&self, run: &str) -> Vec<String> {
        let Some(tk) = self.by_run.get(run).and_then(|t| self.t(*t)) else { return Vec::new() };
        let mut v: Vec<String> = tk.words.iter().map(|(r, _)| self.name(*r)).collect();
        v.sort_unstable();
        v
    }
}

// The wire's inputs and outputs: each input as its `_c` twin has it, its commands made [`Act`]s.
impl Owner {
    /// A [`Cmd`] as the wire has it.
    pub fn act(&self, c: Cmd) -> Act {
        match c {
            Cmd::Tell { to, run, msg } => Act::Tell { runner: self.name(to), msg: msg.wire(&run) },
            Cmd::Answer { run, answer } => Act::Answer { run: run.to_string(), answer },
            Cmd::Go { run, to, fence } => Act::Go { run: run.to_string(), runner: self.name(to), fence },
            Cmd::Fallback { run, runners } => Act::Fallback { run: run.to_string(), runners },
            Cmd::Restart { run, to, fence } => Act::Restart { run: run.to_string(), runner: self.name(to), fence },
        }
    }

    fn wire(&mut self, f: impl FnOnce(&mut Owner, &mut Vec<Cmd>)) -> Vec<Act> {
        let mut c = Vec::new();
        f(self, &mut c);
        c.into_iter().map(|c| self.act(c)).collect()
    }

    /// A runner's handle here, given one if it has none: what [`Cmd`]s name it by.
    pub fn runner_id(&mut self, name: &str) -> u32 {
        self.rid(name)
    }

    /// A runner's name, by its handle.
    pub fn runner_name(&self, r: u32) -> &str {
        self.names.name(r)
    }

    pub fn machines(&mut self, ms: Vec<Machine>) -> Vec<Act> {
        self.wire(|o, c| o.machines_c(ms, c))
    }

    pub fn away(&mut self, runner: &str) -> Vec<Act> {
        self.wire(|o, c| o.away_c(runner, c))
    }

    pub fn back(&mut self, runner: &str) -> Vec<Act> {
        self.wire(|o, c| o.back_c(runner, c))
    }

    pub fn ask(&mut self, ask: Ask, now: i64) -> Vec<Act> {
        self.wire(|o, c| o.ask_c(ask, now, c))
    }

    /// A runner's word about a run, as the wire has it.
    pub fn heard(&mut self, runner: &str, msg: ToOwner, now: i64) -> Vec<Act> {
        let r = self.rid(runner);
        let (run, up) = msg.split();
        let run: Arc<str> = Arc::from(run);
        self.wire(|o, c| o.heard_c(r, 0, &run, up, now, c))
    }

    pub fn cancel(&mut self, run: &str) -> Vec<Act> {
        self.wire(|o, c| o.cancel_c(run, c))
    }

    pub fn handed(&mut self, run: &str, by: &str) -> Vec<Act> {
        self.wire(|o, out| o.handed_c(run, by, out))
    }

    pub fn fallback_started(&mut self, run: &str) -> Vec<Act> {
        self.wire(|o, c| o.fallback_started_c(run, c))
    }

    pub fn fallback_refused(&mut self, run: &str) -> Vec<Act> {
        self.wire(|o, c| o.fallback_refused_c(run, c))
    }

    pub fn restore(&mut self, kept: Vec<Kept>) -> Vec<Act> {
        self.wire(|o, out| o.restore_c(kept, out))
    }

    pub fn reask(&mut self, now: i64) -> Vec<Act> {
        self.wire(|o, out| o.reask_c(now, out))
    }

    pub fn unstarted(&mut self, run: &str) -> Vec<Act> {
        self.wire(|o, out| o.unstarted_c(run, out))
    }

    pub fn runner_epoch(&mut self, runner: &str, epoch: u64) -> Vec<Act> {
        let r = self.rid(runner);
        self.wire(|o, out| o.runner_epoch_c(r, epoch, out))
    }

    pub fn tick(&mut self, now: i64) -> Vec<Act> {
        self.wire(|o, c| o.tick_c(now, c))
    }

    /// [`Self::starting`], by handle: the run by its id, its runner by its handle here.
    pub fn starting_c(&mut self, run: &str, r: u32, fence: Fence) -> bool {
        let Some(t) = self.by_run.get(run).copied() else { return false };
        match self.t_mut(t) {
            Some(tk) if tk.stage == (Stage::Held { fence }) && tk.runner == Some(r) => {
                tk.stage = Stage::Starting { fence };
                self.keep(t);
                true
            }
            _ => false,
        }
    }

}

/// An amount in words: "12 CPUs, 14 GiB".
fn words(r: &Resources) -> String {
    let mut w = vec![needs::cpus(r.cpu), needs::show_size(r.mem)];
    if r.gpu > 0 {
        w.push(needs::gpus(r.gpu));
    }
    w.join(", ")
}

// The placement kernel's part (docs/lb.md, "Adopted"; W05-R03, W05-R04, W05-R16, W05-R23).
impl Owner {
    /// The ticket of kernel run `h`, while the kernel places it.
    fn fast_tid(&self, h: lb::Handle) -> Option<Tid> {
        let (generation, t) = self.fast.by_slot.get(ix(h.slot)).copied().flatten()?;
        (generation == h.generation && self.t(t).is_some_and(|tk| tk.fast == Some(h))).then_some(t)
    }

    fn fast_note(&mut self, h: lb::Handle, t: Tid) {
        while self.fast.by_slot.len() <= ix(h.slot) {
            self.fast.by_slot.push(None);
        }
        if let Some(x) = self.fast.by_slot.get_mut(ix(h.slot)) {
            *x = Some((h.generation, t));
        }
    }

    fn fast_unnote(&mut self, h: lb::Handle) {
        if let Some(x) = self.fast.by_slot.get_mut(ix(h.slot))
            && x.is_some_and(|(g, _)| g == h.generation)
        {
            *x = None;
        }
    }

    /// Runner `runner`'s room as its kernel ledger says it (its hints, the `offers` feature's
    /// `fast`): it joins the kernel if the directory has it admitting kernel starts, its budget is
    /// followed, and its room is what the kernel picks by from now (less what this owner sent it
    /// since). O(1) but on joining.
    pub fn fast_hint(&mut self, runner: &str, w: &FastWord) {
        let Some(r) = self.names.get(runner) else { return };
        self.fast_hint_c(r, w);
    }

    /// [`Self::fast_hint`], by handle.
    pub fn fast_hint_c(&mut self, r: Rid, w: &FastWord) {
        if !self.machine(r).is_some_and(|m| m.fast && m.places) {
            return;
        }
        self.fast_join(r, w);
        self.fast.out.clear();
        // A hint starts, declines and ends nothing, and nothing waits in the kernel: no news.
        let _ = self.fast.lb.hear(lb::ToOwner::Hint { runner: lb::RunnerId(r), word: w.word() }, &mut self.fast.out);
        self.fast.out.clear();
    }

    /// `r` known to the kernel with the budget `w` says: joined at that word, or its budget
    /// followed (what could ever fit its kinds of ask with it).
    fn fast_join(&mut self, r: Rid, w: &FastWord) {
        while self.fast.joined.len() <= ix(r) {
            self.fast.joined.push(None);
        }
        match self.fast.joined.get(ix(r)).copied().flatten() {
            Some(b) if b == w.budget => {}
            Some(_) => {
                self.fast.lb.set_budget(lb::RunnerId(r), w.budget);
                if let Some(j) = self.fast.joined.get_mut(ix(r)) {
                    *j = Some(w.budget);
                }
            }
            None => {
                self.fast.out.clear();
                self.fast.lb.join(lb::RunnerId(r), w.budget, w.word(), &mut self.fast.out);
                self.fast.out.clear();
                if let Some(j) = self.fast.joined.get_mut(ix(r)) {
                    *j = Some(w.budget);
                }
                self.fast.era = self.fast.era.wrapping_add(1);
            }
        }
    }

    /// `r` left the directory: the kernel forgets it (what ran there is answered as for any runner
    /// gone).
    fn fast_leave(&mut self, r: Rid) {
        if self.fast.joined.get(ix(r)).copied().flatten().is_none() {
            return;
        }
        self.fast.out.clear();
        self.fast.lb.leave(lb::RunnerId(r), &mut self.fast.out);
        self.fast.out.clear();
        if let Some(j) = self.fast.joined.get_mut(ix(r)) {
            *j = None;
        }
        self.fast.era = self.fast.era.wrapping_add(1);
    }

    /// The kernel's signature for `suit`'s kind of ask (W05-R04): its members the suit's live
    /// runners the kernel knows, synced when the suit or the kernel's runners change (O(members
    /// log members) then; nothing on the asks between).
    fn fast_sig(&mut self, key: u64, suit: &Arc<Suit>) -> lb::SigId {
        let era = self.fast.era;
        let at = self.fast.sigs.get(&key).and_then(|v| v.iter().position(|f| f.suit.same(suit)));
        if let Some(f) = at.and_then(|i| self.fast.sigs.get(&key).and_then(|v| v.get(i)))
            && f.era == era
            && Arc::ptr_eq(&f.suit, suit)
        {
            return f.sig;
        }
        let members: Vec<Rid> = suit.live.iter().copied().filter(|r| self.fast.joined.get(ix(*r)).copied().flatten().is_some() && self.machine(*r).is_some_and(|m| m.fast)).collect();
        let (sig, old) = match at.and_then(|i| self.fast.sigs.get_mut(&key).and_then(|v| v.get_mut(i))) {
            Some(f) => (f.sig, std::mem::take(&mut f.members)),
            None => (self.fast.lb.sig(), Vec::new()),
        };
        self.fast.out.clear();
        // Both in handle order (a suit's live runners are sorted).
        for r in old.iter().filter(|r| members.binary_search(r).is_err()) {
            self.fast.lb.remove(sig, lb::RunnerId(*r));
        }
        for r in members.iter().filter(|r| old.binary_search(r).is_err()) {
            self.fast.lb.add(sig, lb::RunnerId(*r), &mut self.fast.out);
        }
        self.fast.out.clear();
        let entry = FastSig { suit: Arc::clone(suit), era, sig, members };
        match at.and_then(|i| self.fast.sigs.get_mut(&key).and_then(|v| v.get_mut(i))) {
            Some(f) => *f = entry,
            None => self.fast.sigs.entry(key).or_default().push(entry),
        }
        sig
    }

    /// The kernel's decision for ticket `t` (docs/lb.md): an ordinary ask that a member of its kind
    /// of ask has room for starts there now, its start that member's alone. False: the kernel
    /// can't start it now (no member has room, none admits kernel starts, or the ask isn't
    /// ordinary: exclusive, a gang), and probing places it.
    fn fast_place(&mut self, t: Tid, out: &mut Vec<Cmd>) -> bool {
        let Some((ask, suit)) = self.t(t).map(|tk| (Arc::clone(&tk.ask), Arc::clone(&tk.suit))) else { return false };
        if ask.needs.exclusive || ask.needs.together.is_some() || suit.live.is_empty() || self.fast.joined.is_empty() {
            return false;
        }
        let sig = self.fast_sig(suit_key(&ask), &suit);
        let need = super::ask::amounts(&ask.needs.request);
        self.fast.out.clear();
        let asked = self.fast.lb.ask(sig, need, &mut self.fast.out);
        self.fast.out.clear();
        match asked {
            Ok(lb::Asked { run, send: Some((r, lb::ToRunner::Start(st))) }) => {
                self.fast_go(t, run, r.0, st.epoch, out);
                true
            }
            Ok(lb::Asked { run, .. }) => {
                // No member has room now: the kernel would have it wait, and waiting is probing's.
                let _ = self.fast.lb.cancel(run, &mut self.fast.out);
                self.fast.out.clear();
                false
            }
            Err(_) => false,
        }
    }

    /// Ticket `t` starts on `r` by kernel run `kernel`, decided under `epoch`: held there as a
    /// taken offer is (its tree copied, then its start sent, [`Self::fast_start`]), its asker told
    /// it's starting there.
    fn fast_go(&mut self, t: Tid, kernel: lb::Handle, r: Rid, epoch: u32, out: &mut Vec<Cmd>) {
        let fence = fast_fence(kernel, epoch);
        let view = self.fast.lb.view(lb::RunnerId(r)).unwrap_or_default();
        let prior = self.t(t).and_then(|tk| tk.runner);
        let Some(tk) = self.t_mut(t) else { return };
        let got = tk.ask.needs.request;
        tk.stage = Stage::Held { fence };
        tk.runner = Some(r);
        tk.fast = Some(kernel);
        // What was free there as it was decided: the kernel's view, with its own need.
        tk.got = Some((got, super::ask::resources(&super::ask::plus(&view, &super::ask::amounts(&got)))));
        let run = Arc::clone(&tk.run);
        if let Some(p) = prior.filter(|p| *p != r) {
            self.unnote(p, t);
        }
        self.note(r, t);
        self.fast_note(kernel, t);
        out.push(Cmd::Go { run, to: r, fence });
        let name = self.name(r);
        self.tell(t, Answer::Starting { runner: name, got }, out);
    }

    /// The kernel's start of run `run` under `fence`, for the shell to send once its tree is there
    /// (`ToRunner::Fast`): its ask, its ticket's token, its kernel handle and the runner's kernel
    /// epoch it was decided under. `None`: not a run the kernel places (its start is the claim's,
    /// `ToRunner::Start`).
    pub fn fast_start(&self, run: &str, fence: Fence) -> Option<(Arc<Ask>, u64, lb::Handle, u32)> {
        let t = self.by_run.get(run).copied()?;
        let tk = self.t(t)?;
        let kernel = tk.fast?;
        match tk.stage {
            Stage::Held { fence: f } | Stage::Starting { fence: f } if f == fence => Some((Arc::clone(&tk.ask), self.token(t), kernel, u32::try_from(fence.epoch).unwrap_or(u32::MAX))),
            _ => None,
        }
    }

    /// Runner `r`'s word on a kernel start (docs/lb.md), about run `run_id`: heard by the kernel
    /// first (its view of `r`, and what that decides of the run: started, placed again on another
    /// member, or no member has room now), then told to the run's asker. A run the kernel can't
    /// place any more goes to probing with the same ask (W05-R16).
    fn fast_heard(&mut self, r: Rid, run_id: &Arc<str>, msg: Up, now: i64, out: &mut Vec<Cmd>) {
        let runner = lb::RunnerId(r);
        let (word, heard, ok, kernel) = match msg {
            Up::FastStarted { kernel, at, word } => (word, lb::ToOwner::Started { runner, run: kernel, at, word: word.word() }, None, kernel),
            Up::FastDeclined { kernel, why, word } => (word, lb::ToOwner::Declined { runner, run: kernel, word: word.word(), why, of: lb::Of::Start }, None, kernel),
            Up::FastEnded { kernel, ok, word } => (word, lb::ToOwner::Ended { runner, run: kernel, word: word.word() }, Some(ok), kernel),
            _ => return,
        };
        if self.fast.joined.get(ix(r)).copied().flatten().is_some() {
            self.fast_join(r, &word);
        }
        let ticket = self.fast_tid(kernel);
        let mut lbo = std::mem::take(&mut self.fast.out);
        lbo.clear();
        let told = self.fast.lb.hear(heard, &mut lbo);
        let news: Vec<lb::Told> = told.into_iter().chain(lbo.told.drain(..)).collect();
        let starts: Vec<(Rid, lb::Start)> = lbo.to.drain(..).filter_map(|(to, m)| if let lb::ToRunner::Start(st) = m { Some((to.0, st)) } else { None }).collect();
        lbo.clear();
        self.fast.out = lbo;
        for n in news {
            self.fast_told(n, ok, now, out);
        }
        for (to, st) in starts {
            match self.fast_tid(st.run) {
                Some(t) => self.fast_go(t, st.run, to, st.epoch, out),
                None => {
                    let _ = self.fast.lb.unsent(st.run);
                }
            }
        }
        // An end heard for a run no ticket follows (its asker gave up): its record there can go.
        if ok.is_some() && ticket.is_none() {
            out.push(Cmd::Tell { to: r, run: Arc::clone(run_id), msg: Down::Ack });
        }
    }

    /// What the kernel decided of a run, told to its ticket's asker.
    fn fast_told(&mut self, n: lb::Told, ok: Option<bool>, now: i64, out: &mut Vec<Cmd>) {
        match n {
            // Its runner's word that it was admitted and its process started.
            lb::Told::Started { run, runner } => {
                let Some(t) = self.fast_tid(run) else { return };
                let Some(tk) = self.t_mut(t) else { return };
                let (Stage::Held { fence } | Stage::Starting { fence }) = tk.stage else { return };
                if tk.runner != Some(runner.0) {
                    return;
                }
                tk.stage = Stage::Running { fence: Some(fence) };
                let (g, free) = tk.got.unwrap_or_default();
                let name = self.name(runner.0);
                // Kept as it runs (LB03): a restart holds it where it started, never starts it again.
                self.keep(t);
                // Its tenant's usage counts it, as a claim's start (W05-R32).
                self.charge_started(t, runner.0, now);
                self.tell(t, Answer::Started { runner: name, got: g, why: format!("room for it there: {} free", words(&free)) }, out);
            }
            lb::Told::Ended { run } => {
                let Some(t) = self.fast_tid(run) else { return };
                let Some((r, run)) = self.t(t).and_then(|tk| Some((tk.runner?, Arc::clone(&tk.run)))) else { return };
                // Its usage settled to the time it ran (W05-R32), then through `finish` (LB03): kept, its
                // answer is owed until it's told, so a restart tells it again and never takes a second;
                // the runner's record goes once that's kept.
                self.usage.ended(&run, now);
                self.finish(t, Answer::Ended { runner: self.name(r), ok: ok.unwrap_or(false) }, out);
                out.push(Cmd::Tell { to: r, run, msg: Down::Ack });
            }
            lb::Told::Waiting { run } | lb::Told::Never { run } => match self.fast_tid(run) {
                Some(t) => self.fast_handback(t, now, out),
                None => {
                    self.fast.out.clear();
                    let _ = self.fast.lb.cancel(run, &mut self.fast.out);
                    self.fast.out.clear();
                }
            },
            lb::Told::Lost { run } => {
                let Some(t) = self.fast_tid(run) else { return };
                let runner = self.t(t).and_then(|tk| tk.runner).map(|r| self.name(r)).unwrap_or_default();
                self.finish(t, Answer::Lost { runner, why: "its runner started again while it ran there".to_owned() }, out);
            }
        }
    }

    /// Ticket `t` leaves the kernel (no member has room now, or none could ever take it as the
    /// kernel knows them): its kernel run forgotten, any wait the kernel decided for it dropped
    /// unsent, and it is probed as if just asked, its kinds of runner as the directory has them
    /// now: where its line's order, fair share, classes and kept room decide.
    fn fast_handback(&mut self, t: Tid, now: i64, out: &mut Vec<Cmd>) {
        let Some(tk) = self.t_mut(t) else { return };
        let (kernel, r) = (tk.fast.take(), tk.runner.take());
        tk.stage = Stage::Waiting;
        tk.got = None;
        let ask = Arc::clone(&tk.ask);
        // Its attempt is no more (LB03): kept so, a restart asks it again rather than syncing a runner for it.
        self.keep(t);
        if let Some(r) = r {
            self.unnote(r, t);
        }
        if let Some(h) = kernel {
            self.fast_unnote(h);
            self.fast.out.clear();
            let _ = self.fast.lb.cancel(h, &mut self.fast.out);
            self.fast.out.clear();
        }
        let suit = self.suit(&ask);
        if suit.live.is_empty() && suit.away.is_empty() && suit.older.is_empty() {
            if let Some(tk) = self.remove(t) {
                out.push(Cmd::Answer { run: tk.run, answer: self.never(&ask) });
            }
            return;
        }
        if let Some(tk) = self.t_mut(t) {
            tk.suit = suit;
        }
        self.probe_new(t, now, out);
    }

    /// What the kernel counted off its fast paths (docs/lb.md), for views and tests.
    pub fn fast_counts(&self) -> lb::Counts {
        self.fast.lb.counts
    }
}
