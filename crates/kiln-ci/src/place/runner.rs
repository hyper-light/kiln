//! The runner's side of placement (docs/reach.md §2; docs/needs.md §5, §6, §7, §8, §9 and E5): it
//! admits what owners probe it with against its own measured room, and its ledger is the last word
//! on that room (G1). It decides by the engine's policy (`crate::sched::state`, docs/needs.md §5
//! and §7), the same pure functions, over its own line: one engine, decided where the room is.
//! Every event is O(log n) in the runs waiting here, but for news to an owner whose link went down
//! or came back, which is a message for each of its runs, and fair share's standing moving (below).
//!
//! - **Its line** ([`super::line::Line`]), in the engine's order: class first (W05-R30), then the
//!   run's tenant's standing in its class's tree (hierarchical start-time fair queuing over the
//!   tenant tree, Goyal, Vin and Cheng, SIGCOMM 1996; W05-R32: repository, then actor, then lane as
//!   `fair_by` goes, each level `max(V[parent], finish[node])`), then its place in its tenant's own
//!   order (W05-R09's priority, aged as it waits, `sched::state::own_order`), then arrival.
//! - **Fair share's charge** (W05-R32, R33, as the engine charges it): a run is charged as it's
//!   offered (dispatched, Kueue's entry penalty), at every node from its repository down, its
//!   dominant share of this runner (DRF, Ghodsi et al., NSDI 2011: the largest of its slot, CPUs,
//!   memory and GPUs over what the runner offers) times its median time over the node's weight; and
//!   settled to the time it ran when it ends (DRFQ: start-time fair queuing needs no time
//!   beforehand). With no median and no `default_cost` set, it costs nothing until it ends.
//!   A tenant's standing moves as it's charged or settled, and so do its runs' keys: each is put
//!   in again at its new place, O(log n) each, over the leaves of the class charged.
//! - **Classes** (W05-R30, R31, docs/needs.md §7): strictly in order, but a class short of its floor
//!   share (its running runs' holds, at their largest dimension over what the runner offers) goes
//!   ahead with a run that keeps it within its floor, or with one that takes it over once the
//!   floor's pace allows (`sched::state::pace_ms`). A run that waited its class's bound while its
//!   class started nothing here goes up a class (never into `critical`, never preempting). Its
//!   bound, in every class with one above (`background`, `batch`, `interactive`), is the work of
//!   the classes before its own here when its wait began: once none of that waits any more, the
//!   time from then to the latest expected end of that work still running (its p90,
//!   else its median, else its timeout, GitHub's 360 minutes unsaid: `sched::state::Due`); with
//!   none of it here, none until some comes; a person's bound for its class besides, whichever
//!   comes first (docs/needs.md E5: as the engine and its reference decide). Each look is a timer ([`Self::timer`]), and every input takes those due by its time first.
//! - **Admission**: of each class, the first in order that has room starts (work-conserving first
//!   fit, as the engine's pick); room is the engine's rule (W05-R03): an `exclusive` run asking no
//!   CPUs takes the runner whole and waits for it empty (its CI legs too), one asking CPUs holds
//!   them and its memory alone, never overcommitted, and any other fits beside what's held, its
//!   CPUs overcommitted as the person's `overcommit_cpu` says. Memory, disk and GPUs are never
//!   overcommitted (docs/needs.md §3), and nothing passes the budget (G1).
//! - **Kept room** (`backfill` declared, EASY backfilling, Lifka 1995; W05-R39): the first in line
//!   that has no room keeps room from when the claims here are predicted to have left it room; only
//!   what fits beside that room, or is predicted to end by then, starts ahead of it. A run that
//!   takes the runner whole is kept room the same way, so a stream of small runs never starves it.
//! - **Offers**: a run given room is offered to its owner as a claim fenced by this runner's epoch
//!   and its number in it (W05-R34), held for the link's retransmission timeout (RFC 6298, measured
//!   by the shell, [`super::link::Rtt`]); taken, its room is held until the owner starts it or is
//!   gone. A claim ends only by the owner's word, its absence, or its epoch changing.
//! - **The record**: each run started here is kept by its owner, the owner's epoch and its id until
//!   the owner says it heard how it ended, or the owner's epoch changes: a run is never started twice,
//!   and a restarted owner's run that reuses an id is another run.
//! - **News**: each waiting run's owner is told its place to within a power of two
//!   ([`super::msg::Place`]) and its one reason, each time either changes: after every change to the
//!   line, the runs at the boundaries between powers of two are looked at, O(log n) of them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::ask::{self, Amounts, Ask, Peer, Sample};
use super::keys::{FxMap, Interner};
use super::line::{Item, Key, Line, Query};
use super::link::Rtt;
use super::msg::{Estimate, Place, Reason, ToOwner, Up, shortfall};
use crate::facts::RunnerFacts;
use crate::sched::routing::Constraint;
use crate::sched::state::{self as engine, Class, ClassConfig, Due, FairBy, Fence, LEVELS, PerMille, Resources};
use crate::sched::units::Ms;



/// Fair share's dimensions, as the engine counts them (W05-R33): slots, milli-CPUs, memory bytes
/// and GPUs.
const FAIR: usize = engine::DIMS;

/// What a runner admits by: each a setting the person decides, as the engine's are (`[ci.scheduler]`,
/// `[sched.classes]`, `[ci.fairness]`), defaulting as the engine's do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// What kiln may place here (`[runner] budget`, docs/needs.md §6): the machine when `None`.
    pub budget: Option<Resources>,
    /// CPUs offered beyond the room, in thousandths (`[reach] overcommit_cpu`): 1000 offers none.
    pub overcommit_cpu: PerMille,
    /// Room kept for the first in line that doesn't fit (`[reach] backfill`, W05-R39).
    pub backfill: bool,
    /// Fair share's weights by repository, in thousandths (`[ci.fairness] weights`, W05-R10); a
    /// repository not named weighs 1, as every other not named does.
    pub weights: BTreeMap<String, PerMille>,
    /// Each class's floor and bound (`[sched.classes]`, W05-R31): the engine's defaults
    /// ([`engine::default_classes`]) unless set.
    pub classes: BTreeMap<Class, ClassConfig>,
    /// Priority a waiting run gains a minute in its tenant's own order (W05-R09), in thousandths.
    pub aging_per_minute: PerMille,
    /// What fair share is between (W05-R10, W28-R66): repositories, then their actors, then lanes.
    pub fair_by: Vec<FairBy>,
    /// The runner's slots (`[runner] slots`): one of fair share's dimensions, each run holding one;
    /// none counted when 0.
    pub slots: u32,
    /// What a run with no median is charged as it starts (`[ci.scheduler] default_cost`): `None`,
    /// nothing until it ends, when it's charged what it ran (start-time fair queuing needs no time
    /// beforehand).
    pub default_cost: Option<Ms>,
}

impl Default for Settings {
    fn default() -> Self {
        let engine = engine::Config::default();
        Settings {
            budget: None,
            overcommit_cpu: 1000,
            backfill: false,
            weights: BTreeMap::new(),
            classes: engine.classes,
            aging_per_minute: engine.aging_per_minute,
            fair_by: engine.fair_by,
            slots: 0,
            default_cost: None,
        }
    }
}

/// A message for an owner, as the wire has it: whose (its node's name), and what.
pub type Said = (String, ToOwner);

/// A message for an owner by handle, as the in-process path carries it (docs/needs.md §11, AUDIT
/// P6): to which of its owners' processes ([`Admission::owner_name`]), the owner's own handle of
/// the run (0 when it gave none), the run, and what. No names copied: [`Admission::said`] makes
/// the wire's [`Said`] of it at the edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub owner: u32,
    pub token: u64,
    pub run: Arc<str>,
    pub msg: Up,
}

/// A run started here, as the record the shell keeps on disk has it: under which claim, and how
/// it ended (`None` while it runs; `Some(true)` its process exited, `Some(false)` it was lost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub fence: Fence,
    pub ended: Option<bool>,
}

/// The record of runs started here, by owner, the owner's epoch and run: nested, so that looking
/// one up borrows its names rather than building a key.
pub type Records = BTreeMap<String, BTreeMap<u64, BTreeMap<String, Record>>>;

/// What a start under a live claim comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Its runner starts the process now (the shell writes the record first).
    New,
    /// It runs or ran here already: nothing starts.
    Already,
}

/// A runner as it starts: its name, its epoch (one past the last it used, kept on its disk, so a
/// claim from before a restart is never taken for one after), its own facts and labels (what
/// probes' constraints are checked against), its first sample, the record of what it started, and
/// its settings.
#[derive(Debug, Clone, Default)]
pub struct Init {
    pub me: String,
    pub epoch: u64,
    pub facts: RunnerFacts,
    pub labels: Vec<String>,
    pub sample: Sample,
    pub records: Records,
    pub settings: Settings,
}

/// Where a probe stands on the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Out of the line while its owner's link is down.
    Aside,
    /// In the line.
    Waiting,
    /// Room given: offered to its owner until `until`.
    Offered { fence: Fence, until: i64 },
    /// Its owner took the offer: its room is held for it until it starts.
    Taken { fence: Fence },
    /// It runs here, from `from`.
    Running { fence: Fence, from: i64 },
}

/// What a run holds of the runner while it holds room (W05-R03, the engine's `Claim`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Room beside what's held, its CPUs overcommitted as the setting says.
    Ordinary,
    /// `exclusive`, asking CPUs: those and its memory, held alone, never overcommitted.
    Share,
    /// `exclusive`, asking no CPUs: the runner whole, empty.
    Whole,
}

fn kind_of(a: &Ask) -> Kind {
    match (a.needs.exclusive, a.needs.request.cpu.is_zero()) {
        (false, _) => Kind::Ordinary,
        (true, false) => Kind::Share,
        (true, true) => Kind::Whole,
    }
}

/// What a waiting run's owner was last told, as news counts it: its place and its reason's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    place: Place,
    reason: (u8, Option<i64>),
}

/// What a run's dispatch charged fair share and its class's floor (the engine's `Charge`), settled
/// when its claim ends.
#[derive(Debug, Clone, Copy)]
struct Charge {
    class: Class,
    /// Its shares of what the runner offers (millionths), and what fair share's tags take of them.
    shares: [u128; FAIR],
    fair: [u128; FAIR],
    /// The milliseconds it was charged for, and when.
    ms: Ms,
    at: i64,
    /// What it holds in each of fair share's dimensions, against its class's floor.
    holds: [u64; FAIR],
    /// Over its class's floor: the pace it took and what was left of the floor.
    paced: Option<(i64, u128)>,
}

#[derive(Debug, Clone)]
struct Probe {
    owner: u32,
    /// The owner's own handle of it, said back with every word (0: none given).
    token: u64,
    run: Arc<str>,
    /// The ask as its owner's probe shares it: never copied.
    ask: Arc<Ask>,
    amounts: Amounts,
    predicted: Option<Ms>,
    state: State,
    /// Its place in the line, from when it came in: it keeps its own order and arrival while aside,
    /// its standing taken as it goes back in.
    key: Option<Key>,
    /// Its tenant's leaf in the tenant tree (`repo`, `repo\0actor`, `repo\0actor\0lane`): its
    /// nodes top down, interned (the first `depth` of `path`; the leaf the last), and their
    /// weights.
    path: [u32; LEVELS],
    depth: u8,
    weights: [PerMille; LEVELS],
    /// Its class now: the one it came with, or the one its wait brought it to (W05-R31).
    class: Class,
    /// Its place in its tenant's own order, its arrival here, and when it came.
    own: i128,
    seq: u64,
    /// When it came to each class it's been in, counted over this runner as runs come to classes
    /// (`u64::MAX`: never there): the work of the classes before a run's that came there before
    /// its wait began is what it waits behind (W05-R31, the engine's `Behind`).
    stamps: [u64; 5],
    /// Its wait behind that work, in a class with one above.
    watch: Watch,
    charge: Option<Charge>,
    /// A claim's predicted end and hard end (its timeout), by the shell's prediction, when it has
    /// them; and its ends as the engine reckons them (p90, else median, else timeout).
    end: Option<i64>,
    hard: Option<i64>,
    due: Option<Due>,
    told: Option<Told>,
    /// Its promotion's look (W05-R31), when it has one: when, and from which class. Out of
    /// `promotions` while it isn't waiting, put back if it waits again before then (a look finding
    /// it offered or holding room does nothing), so looks for runs long started don't pile up
    /// for a class's bound (docs/needs.md §11, AUDIT P6).
    look: Option<(i64, Class)>,
}

/// A run's wait behind the work of the classes before its own (W05-R31, docs/needs.md E5: the
/// engine's `Behind`, the reference's `Ahead`), in a class with one above: the runs of those
/// classes here (waiting or holding room) when it began (`at`, counted as [`Probe::stamps`] are;
/// `since`, the time) are ahead of it. With none here then (`armed` false) it waits unbounded until
/// some comes, and its wait is from then. Once none of them waits any more, its bound is due at the
/// latest expected end of those still holding room, or then (`due`), and that bound is looked at
/// (`next`, moved a bound after a start of its class, as a person's bound's look is).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Watch {
    at: u64,
    since: i64,
    armed: bool,
    due: Option<i64>,
    next: Option<i64>,
}

/// Whether runs of `class` wait behind the classes before theirs: those with a class above.
fn watched(class: Class) -> bool {
    class.up().is_some()
}

/// When run `p` first came to a class before `c`, if it's in one now: where it stands among the
/// work ahead of `c`'s runs.
fn below(p: &Probe, c: Class) -> Option<u64> {
    (p.class < c).then(|| p.stamps.iter().take(c.index()).copied().min().unwrap_or(u64::MAX))
}

/// Where run `p` stands among the work ahead of each class with one above, by the class's index.
fn marks(p: &Probe) -> [Option<u64>; 5] {
    Class::ALL.map(|c| below(p, c).filter(|_| watched(c)))
}

/// An owner's process, as this runner hears it.
#[derive(Debug, Clone)]
struct OwnerAt {
    name: String,
    epoch: u64,
    away: bool,
    runs: FxMap<Arc<str>, u32>,
    rtt: Option<Rtt>,
}

/// One class's fair share and floor, as the engine keeps them: each parent node's virtual time and
/// each node's finish tag (the runner's own set of dimensions alone), what its running runs hold,
/// when its floor's pace lets it go over its floor again, when it last started a run here, and the
/// runs waiting in it, by leaf, with the standing their keys hold.
/// Nodes are the runner's interned tenant-tree nodes (`Admission::nodes`; 0 the root, the parent of
/// every repository): virtual times and finish tags by node, in vectors.
#[derive(Debug, Clone, Default)]
struct Fair {
    vtimes: Vec<u128>,
    finish: Vec<Option<u128>>,
    held: [u128; FAIR],
    paced: Option<i64>,
    served: Option<i64>,
    leaves: BTreeMap<u32, ([u128; LEVELS], BTreeSet<u32>)>,
    /// Its tags moved since its runs' keys were last put right.
    moved: bool,
}

impl Fair {
    fn vtime(&self, n: u32) -> u128 {
        self.vtimes.get(ix(n)).copied().unwrap_or(0)
    }

    fn set_vtime(&mut self, n: u32, v: u128) {
        if self.vtimes.len() <= ix(n) {
            self.vtimes.resize(ix(n).saturating_add(1), 0);
        }
        if let Some(x) = self.vtimes.get_mut(ix(n)) {
            *x = v;
        }
    }

    fn finish_of(&self, n: u32) -> Option<u128> {
        self.finish.get(ix(n)).copied().flatten()
    }

    fn set_finish(&mut self, n: u32, v: u128) {
        if self.finish.len() <= ix(n) {
            self.finish.resize(ix(n).saturating_add(1), None);
        }
        if let Some(x) = self.finish.get_mut(ix(n)) {
            *x = Some(v);
        }
    }
}

/// The runner's admission of probed runs: pure, its inputs the probes and their owners' words, the
/// runner's samples, the ends of its runs and the time; its outputs what it says to owners.
#[derive(Debug, Clone)]
pub struct Admission {
    me: String,
    epoch: u64,
    fences: u64,
    settings: Settings,
    facts: RunnerFacts,
    labels: Vec<String>,
    sample: Sample,
    /// What it may admit against, from the last sample: CPUs overcommitted (`avail`) and not
    /// (`base`); what its claims hold of it, and of that what shares hold.
    avail: Amounts,
    base: Amounts,
    held: Amounts,
    held_share: Amounts,
    /// What its running claims use within what they asked, from the last sample.
    own: Amounts,
    claims: u32,
    whole_claims: u32,
    unpredicted_claims: u32,
    untimed_claims: u32,
    draining: bool,
    /// Its machine is under strain, and why (AUDIT L1, the shell's measure): it offers nothing new.
    strained: Option<String>,
    slots: Vec<Option<Probe>>,
    spare: Vec<u32>,
    owners: Vec<OwnerAt>,
    /// Each owner's processes by name: (epoch, index), the last the current one.
    owner_ix: FxMap<String, Vec<(u64, u32)>>,
    /// The tenant tree's nodes by name (`""`, the root, is 0), for fair share's tags by handle.
    nodes: Interner,
    /// Each leaf's nodes top down, and how many.
    paths: FxMap<u32, ([u32; LEVELS], u8)>,
    line: Line,
    seq: u64,
    came: u64,
    fair: [Fair; 5],
    /// Offers by when they lapse; claims by predicted end and by hard end.
    deadlines: BTreeSet<(i64, u32)>,
    /// Claims by predicted end, twice over (docs/needs.md §11, AUDIT P6: backfill's search and the
    /// soonest start O(log n), not a walk of every claim): one summing what each holds (its
    /// work's milliseconds 1; a claim of the runner whole counted untimed), one each amount times
    /// its end (shifted by 2^63, so the sum is of whole numbers).
    ends: Line,
    ends_at: Line,
    hard_ends: BTreeSet<(i64, u32)>,
    records: Records,
    /// Room kept for the first in line: its slot, and from when.
    kept: Option<(u32, Option<i64>)>,
    /// Many left the line at once (an owner gone or started again): every waiting run's place is
    /// looked at again, not only the boundaries'.
    stale: bool,
    /// Promotion's looks (W05-R31), by when: the run's slot, its arrival (so a look kept for a
    /// run gone does nothing) and the class it's looked at in, in the order they were made.
    promotions: BTreeMap<i64, Vec<(u32, u64, Class)>>,
    /// For each class with one above (by its index): the runs of the classes before it in the
    /// line, and those holding claims, by when they first came to one of those classes
    /// ([`below`]); its runs in the line whose time to go up isn't set yet, by when their wait
    /// began, and those with nothing ahead yet ([`Watch`]).
    ahead_waiting: [BTreeSet<(u64, u32)>; 5],
    ahead_claims: [BTreeSet<(u64, u32)>; 5],
    watching: [BTreeSet<(u64, u32)>; 5],
    unarmed: [BTreeSet<u32>; 5],
    /// The looks the work ahead's bounds make ([`Watch::next`]), by when, as `promotions`.
    behinds: BTreeMap<i64, Vec<(u32, u64, Class)>>,
    /// A probe just come, not told it waits until admitting has had its say in the same input: one
    /// offered at once is told only the offer (an owner choosing between offers takes a word that
    /// it waits for its answer, docs/needs.md §11, AUDIT P6), and a message is saved.
    hush: Option<u32>,
    /// A buffer of replies, reused by the wire's inputs; and one of the line's slots in order.
    replies: Vec<Reply>,
    order: Vec<u32>,
}

fn ix(s: u32) -> usize {
    s as usize
}

fn later(now: i64, ms: Ms) -> i64 {
    now.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// Each dimension's work in `all` less in `before`.
fn less_sums(all: &super::line::Sums, before: &super::line::Sums) -> [u128; ask::DIMS] {
    let mut out = all.work;
    for (o, b) in out.iter_mut().zip(&before.work) {
        *o = o.saturating_sub(*b);
    }
    out
}

/// A probe's tenant's nodes, top down.
fn path_of(p: &Probe) -> &[u32] {
    p.path.get(..usize::from(p.depth)).unwrap_or(&[])
}

/// A probe's tenant's leaf: its last node.
fn leaf_of(p: &Probe) -> u32 {
    path_of(p).last().copied().unwrap_or(0)
}

/// A claim's place among the claims by predicted end.
fn end_key(at: i64, s: u32) -> Key {
    Key { class: 0, standing: [0; LEVELS], own: i128::from(at), seq: u64::from(s) }
}

/// A time as a whole number, in its order: shifted by 2^63.
fn shifted(t: i64) -> u64 {
    u64::try_from(i128::from(t).saturating_sub(i128::from(i64::MIN))).unwrap_or(u64::MAX)
}

fn class_ix(c: Class) -> u8 {
    u8::try_from(c.index()).unwrap_or(u8::MAX)
}

impl Admission {
    /// A runner as `init` says, at `now`.
    pub fn new(init: Init, now: i64) -> Admission {
        let Init { me, epoch, facts, labels, sample, records, settings } = init;
        let mut a = Admission {
            me,
            epoch,
            fences: 0,
            settings,
            facts,
            labels,
            sample,
            avail: [0; ask::DIMS],
            base: [0; ask::DIMS],
            held: [0; ask::DIMS],
            held_share: [0; ask::DIMS],
            own: [0; ask::DIMS],
            claims: 0,
            whole_claims: 0,
            unpredicted_claims: 0,
            untimed_claims: 0,
            draining: false,
            strained: None,
            slots: Vec::new(),
            spare: Vec::new(),
            owners: Vec::new(),
            owner_ix: FxMap::default(),
            nodes: Interner::default(),
            paths: FxMap::default(),
            line: Line::default(),
            seq: 0,
            came: 0,
            fair: Default::default(),
            deadlines: BTreeSet::new(),
            ends: Line::default(),
            ends_at: Line::default(),
            hard_ends: BTreeSet::new(),
            records,
            kept: None,
            stale: false,
            promotions: BTreeMap::new(),
            ahead_waiting: Default::default(),
            ahead_claims: Default::default(),
            watching: Default::default(),
            unarmed: Default::default(),
            behinds: BTreeMap::new(),
            hush: None,
            replies: Vec::new(),
            order: Vec::new(),
        };
        a.nodes.intern("");
        a.remeasure();
        let _ = now;
        a
    }

    /// Its name and epoch.
    pub fn me(&self) -> (&str, u64) {
        (&self.me, self.epoch)
    }

    /// Claim `s`, holding `amounts`, is predicted to end at `at`.
    fn add_end(&mut self, at: i64, s: u32, amounts: Amounts, whole: bool) {
        let k = end_key(at, s);
        self.ends.insert(k, Item { slot: s, ask: amounts, predicted: Some(1), timeout: (!whole).then_some(0) });
        self.ends_at.insert(k, Item { slot: s, ask: amounts, predicted: Some(shifted(at)), timeout: Some(0) });
    }

    fn drop_end(&mut self, at: i64, s: u32) {
        let k = end_key(at, s);
        self.ends.remove(&k);
        self.ends_at.remove(&k);
    }

    fn get(&self, s: u32) -> Option<&Probe> {
        self.slots.get(ix(s)).and_then(Option::as_ref)
    }

    fn get_mut(&mut self, s: u32) -> Option<&mut Probe> {
        self.slots.get_mut(ix(s)).and_then(Option::as_mut)
    }

    fn owner(&self, o: u32) -> Option<&OwnerAt> {
        self.owners.get(ix(o))
    }

    fn fair_of(&self, c: Class) -> Option<&Fair> {
        self.fair.get(c.index())
    }

    /// What kiln may place here in all: the budget, else the machine (CPUs as overcommitted).
    pub fn cap(&self) -> Amounts {
        let size = self.settings.budget.as_ref().map_or_else(|| ask::amounts(&self.sample.size), ask::amounts);
        let [cpu, mem, disk, gpu] = size;
        let cpu = u64::try_from(u128::from(cpu).saturating_mul(u128::from(self.settings.overcommit_cpu.max(1000))) / 1000).unwrap_or(u64::MAX);
        [cpu, mem, disk, gpu]
    }

    /// The room worked out again from the last sample: as overcommitted and not.
    fn remeasure(&mut self) {
        self.avail = ask::avail(&self.sample, self.settings.budget.as_ref(), &self.own, self.settings.overcommit_cpu);
        self.base = ask::avail(&self.sample, self.settings.budget.as_ref(), &self.own, 1000);
    }

    /// What's free for an ordinary run now (W05-R03, as the engine's `fits_beside`): its CPUs
    /// overcommitted beside what shares hold, the rest never.
    fn free_now(&self) -> Amounts {
        let [bc, bm, bd, bg] = self.base;
        let [hc, hm, hd, hg] = self.held;
        let [sc, sm, _, _] = self.held_share;
        let cpus = u64::try_from(u128::from(bc.saturating_sub(sc)).saturating_mul(u128::from(self.settings.overcommit_cpu.max(1000))) / 1000).unwrap_or(u64::MAX);
        let free = [cpus.saturating_sub(hc.saturating_sub(sc)), bm.saturating_sub(sm).saturating_sub(hm.saturating_sub(sm)), bd.saturating_sub(hd), bg.saturating_sub(hg)];
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::OfferPastRoom) {
            let [cpu, mem, disk, gpu] = free;
            return [cpu, mem.saturating_mul(2), disk, gpu];
        }
        free
    }

    /// What the runner offers in each of fair share's dimensions (the engine's `offers` in resource
    /// mode): its slots, and its CPUs, memory and GPUs, of the budget or else the machine.
    fn offers(&self) -> [u64; FAIR] {
        let size = self.settings.budget.unwrap_or(self.sample.size);
        [u64::from(self.settings.slots), size.cpu.get(), size.mem, u64::from(size.gpu)]
    }

    /// What a run holds of the runner in each of fair share's dimensions (the engine's `asks`): a
    /// slot and its request, or all the runner offers when it holds it whole; nothing in a
    /// dimension the runner doesn't offer.
    fn holds(&self, p: &Probe) -> [u64; FAIR] {
        let offers = self.offers();
        let q = &p.ask.needs.request;
        let mut holds = if kind_of(&p.ask) == Kind::Whole { offers } else { [1, q.cpu.get(), q.mem, u64::from(q.gpu)] };
        for (h, o) in holds.iter_mut().zip(offers) {
            if o == 0 {
                *h = 0;
            }
        }
        holds
    }

    fn weight(&self, repo: &str) -> PerMille {
        self.settings.weights.get(repo).copied().filter(|w| *w > 0).unwrap_or(1000)
    }

    fn class_config(&self, class: Class) -> ClassConfig {
        self.settings.classes.get(&class).copied().unwrap_or_default()
    }

    fn rto(&self, o: u32) -> Ms {
        Rtt::rto_of(self.owner(o).and_then(|w| w.rtt.as_ref()))
    }

    fn next_fence(&mut self) -> Fence {
        self.fences = self.fences.saturating_add(1);
        Fence { epoch: self.epoch, n: self.fences }
    }

    /// The owner process a word comes from: a new epoch retires the one before it (its runs that
    /// don't run go, its record goes); a word from an epoch older than the current one is stale.
    fn owner_for(&mut self, from: Peer<'_>) -> Option<u32> {
        let current = self.owner_ix.get(from.name).and_then(|v| v.last()).copied();
        match current {
            Some((e, o)) if e == from.epoch => return Some(o),
            Some((e, _)) if e > from.epoch => return None,
            Some((_, old)) => self.retire(old, from.name, from.epoch),
            None => {}
        }
        let o = u32::try_from(self.owners.len()).unwrap_or(u32::MAX);
        self.owners.push(OwnerAt { name: from.name.to_owned(), epoch: from.epoch, away: false, runs: FxMap::default(), rtt: None });
        self.owner_ix.entry(from.name.to_owned()).or_default().push((from.epoch, o));
        Some(o)
    }

    /// The owner process a word comes from ([`Self::owner_for`]); if that retired an earlier one,
    /// every waiting run is told where it stands now.
    fn speaker(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Reply>) -> Option<u32> {
        let o = self.owner_for(from);
        if self.stale {
            self.boundaries(now, out);
        }
        o
    }

    /// The owner process of `name` in `epoch`, current or not.
    fn incarnation(&self, name: &str, epoch: u64) -> Option<u32> {
        self.owner_ix.get(name).and_then(|v| v.iter().find(|(e, _)| *e == epoch)).map(|(_, o)| *o)
    }

    fn current(&self, name: &str) -> Option<u32> {
        self.owner_ix.get(name).and_then(|v| v.last()).map(|(_, o)| *o)
    }

    /// An owner's process `old` gives way to one of `epoch`: its runs that don't run go (their
    /// claims free), and the record of what it started goes, as nothing of its can ask again.
    fn retire(&mut self, old: u32, name: &str, epoch: u64) {
        let slots: Vec<u32> = self.owner(old).map(|w| w.runs.values().copied().collect()).unwrap_or_default();
        for s in slots {
            if !matches!(self.get(s).map(|p| p.state), Some(State::Running { .. })) {
                self.drop_probe(s);
                self.stale = true;
            }
        }
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::ReusedIdRan) {
            return;
        }
        if let Some(by_epoch) = self.records.get_mut(name) {
            by_epoch.retain(|e, _| *e >= epoch);
            if by_epoch.is_empty() {
                self.records.remove(name);
            }
        }
    }

    /// A probe's claim starts holding room, from `from`.
    fn claim(&mut self, s: u32, from: i64) {
        let Some(p) = self.get(s) else { return };
        let (amounts, kind, marks) = (p.amounts, kind_of(&p.ask), marks(p));
        let end = p.predicted.map(|ms| later(from, ms));
        let hard = p.ask.timeout.map(|ms| later(from, ms));
        let due = Due::of_parts(p.ask.p90, p.ask.median, p.ask.timeout, from);
        self.held = ask::plus(&self.held, &amounts);
        self.claims = self.claims.saturating_add(1);
        match kind {
            Kind::Share => self.held_share = ask::plus(&self.held_share, &amounts),
            Kind::Whole => self.whole_claims = self.whole_claims.saturating_add(1),
            Kind::Ordinary => {}
        }
        match end {
            Some(at) => self.add_end(at, s, amounts, kind == Kind::Whole),
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_add(1),
        }
        match hard {
            Some(at) => {
                self.hard_ends.insert((at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_add(1),
        }
        for (set, mark) in self.ahead_claims.iter_mut().zip(marks) {
            if let Some(b) = mark {
                set.insert((b, s));
            }
        }
        if let Some(p) = self.get_mut(s) {
            p.end = end;
            p.hard = hard;
            p.due = Some(due);
        }
    }

    /// A probe's claim stops holding room; what its dispatch charged is settled to nothing, as it
    /// never ran (one that ran is settled as it ends, [`Self::ended`]).
    fn release(&mut self, s: u32) {
        let Some(p) = self.get(s) else { return };
        let (state, amounts, kind, end, hard, marks) = (p.state, p.amounts, kind_of(&p.ask), p.end, p.hard, marks(p));
        if let State::Offered { until, .. } = state {
            self.deadlines.remove(&(until, s));
        }
        if !matches!(state, State::Offered { .. } | State::Taken { .. } | State::Running { .. }) {
            return;
        }
        self.settle(s, 0);
        self.held = ask::less(&self.held, &amounts);
        self.claims = self.claims.saturating_sub(1);
        match kind {
            Kind::Share => self.held_share = ask::less(&self.held_share, &amounts),
            Kind::Whole => self.whole_claims = self.whole_claims.saturating_sub(1),
            Kind::Ordinary => {}
        }
        match end {
            Some(at) => self.drop_end(at, s),
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_sub(1),
        }
        match hard {
            Some(at) => {
                self.hard_ends.remove(&(at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_sub(1),
        }
        for (set, mark) in self.ahead_claims.iter_mut().zip(marks) {
            if let Some(b) = mark {
                set.remove(&(b, s));
            }
        }
        if let Some(p) = self.get_mut(s) {
            p.end = None;
            p.hard = None;
            p.due = None;
        }
    }

    /// A waiting probe's key in the line: its own order and arrival as it came, its standing its
    /// tenant's, as the class's leaves record it (every waiting run of a tenant stands where its
    /// tenant does: a tenant moved as one block isn't written run by run).
    fn key_of(&self, s: u32) -> Option<Key> {
        let p = self.get(s)?;
        let k = p.key?;
        let standing = self.fair_of(p.class).and_then(|f| f.leaves.get(&leaf_of(p))).map_or(k.standing, |(st, _)| *st);
        Some(Key { standing, ..k })
    }

    /// Takes a probe out of the line, if it's in it.
    fn leave(&mut self, s: u32) {
        let Some(p) = self.get(s) else { return };
        if let (State::Waiting, Some(k)) = (p.state, self.key_of(s)) {
            let (marks, class) = (marks(p), p.class);
            self.line.remove(&k);
            if self.kept.is_some_and(|(k, _)| k == s) {
                self.kept = None;
            }
            for (set, mark) in self.ahead_waiting.iter_mut().zip(marks) {
                if let Some(b) = mark {
                    set.remove(&(b, s));
                }
            }
            self.unwatch(s);
            // The slot and the class's leaves borrowed apart, so the leaf's name isn't copied.
            if let (Some(Some(p)), Some(f)) = (self.slots.get(ix(s)), self.fair.get_mut(class.index()))
                && let Some((_, set)) = f.leaves.get_mut(&leaf_of(p))
            {
                set.remove(&s);
                if set.is_empty() {
                    f.leaves.remove(&leaf_of(p));
                }
            }
        }
        if let Some(p) = self.get_mut(s) {
            p.told = None;
            if p.state == State::Waiting {
                p.state = State::Aside;
            }
        }
    }

    /// Forgets a probe: out of the line, its claim freed, its slot free.
    fn drop_probe(&mut self, s: u32) {
        self.leave(s);
        self.unlook(s);
        self.release(s);
        let Some(p) = self.slots.get_mut(ix(s)).and_then(Option::take) else { return };
        if let Some(w) = self.owners.get_mut(ix(p.owner)) {
            w.runs.remove(p.run.as_ref());
        }
        self.spare.push(s);
    }

    /// A leaf's standing in `class`'s tree (the engine's `standing_of`, for this runner's set of
    /// dimensions): at each node from its repository down, `max(V[parent], finish[node])`.
    ///
    /// As the line keys it: with one level (the default, fair share between repositories), a
    /// repository at the floor (its finish tag at or under the root's virtual time, so its standing
    /// the root's) is keyed 0, one above it by its finish tag. The order is the same (every one at
    /// the floor ties at V, every one above it is above V), but V rising moves no one at the floor:
    /// only the tenant charged, and one V passes, is put in again (docs/needs.md §11, AUDIT P6).
    fn standing_of(&self, class: Class, path: &[u32]) -> [u128; LEVELS] {
        let mut out = [0; LEVELS];
        let Some(f) = self.fair_of(class) else { return out };
        if let [leaf] = path {
            let fin = f.finish_of(*leaf).unwrap_or(0);
            if let Some(slot) = out.first_mut() {
                *slot = if fin > f.vtime(0) { fin } else { 0 };
            }
            return out;
        }
        let mut parent = 0;
        for (slot, node) in out.iter_mut().zip(path) {
            *slot = f.vtime(parent).max(f.finish_of(*node).unwrap_or(0));
            parent = *node;
        }
        out
    }

    /// Puts a probe in the line, in its class now: its own order and arrival as it came, its
    /// tenant's standing as it is now.
    fn enter(&mut self, s: u32) {
        let Some(p) = self.get(s) else { return };
        let (class, marks) = (p.class, marks(p));
        // At its tenant's standing as its other runs here hold it, so they all stand alike; if
        // that's moved, restand moves them all.
        let now_standing = self.standing_of(class, path_of(p));
        let had = self.fair_of(class).and_then(|f| f.leaves.get(&leaf_of(p))).map(|(st, _)| *st);
        let standing = had.unwrap_or(now_standing);
        let key = Key { class: class_ix(class), standing, own: p.own, seq: p.seq };
        let item = Item { slot: s, ask: p.amounts, predicted: p.predicted, timeout: p.ask.timeout };
        self.line.insert(key, item);
        if let (Some(Some(p)), Some(f)) = (self.slots.get(ix(s)), self.fair.get_mut(class.index())) {
            match f.leaves.get_mut(&leaf_of(p)) {
                Some((_, set)) => {
                    set.insert(s);
                    if standing != now_standing {
                        f.moved = true;
                    }
                }
                None => {
                    f.leaves.insert(leaf_of(p), (standing, BTreeSet::from([s])));
                }
            }
        }
        for (set, mark) in self.ahead_waiting.iter_mut().zip(marks) {
            if let Some(b) = mark {
                set.insert((b, s));
            }
        }
        if let Some(p) = self.get_mut(s) {
            p.key = Some(key);
            p.state = State::Waiting;
            p.told = None;
        }
        self.rewatch(s);
    }

    /// Run `s`'s wait behind the work of the classes before its own begins (it came to its class
    /// at `since`), from `at` ([`Watch`]): what of that work is here now is ahead of it.
    fn start_watch(&mut self, s: u32, at: u64, since: i64) {
        let Some(class) = self.get(s).map(|p| p.class) else { return };
        self.unwatch(s);
        let i = class.index();
        let armed = watched(class) && (self.ahead_waiting.get(i).is_some_and(|w| !w.is_empty()) || self.ahead_claims.get(i).is_some_and(|c| !c.is_empty()));
        let waiting = match self.get_mut(s) {
            Some(p) => {
                p.watch = Watch { at, since, armed, due: None, next: None };
                p.state == State::Waiting
            }
            None => false,
        };
        if waiting {
            self.rewatch(s);
        }
    }

    /// Takes run `s` out of its class's watches.
    fn unwatch(&mut self, s: u32) {
        let Some((i, at)) = self.get(s).map(|p| (p.class.index(), p.watch.at)) else { return };
        if let Some(w) = self.watching.get_mut(i) {
            w.remove(&(at, s));
        }
        if let Some(u) = self.unarmed.get_mut(i) {
            u.remove(&s);
        }
    }

    /// Puts waiting run `s` in its class's watches, if its time to go up isn't set yet.
    fn rewatch(&mut self, s: u32) {
        let Some((class, w)) = self.get(s).map(|p| (p.class, p.watch)) else { return };
        if !watched(class) || w.due.is_some() {
            return;
        }
        if w.armed {
            if let Some(set) = self.watching.get_mut(class.index()) {
                set.insert((w.at, s));
            }
        } else if let Some(set) = self.unarmed.get_mut(class.index()) {
            set.insert(s);
        }
    }

    /// Each class whose tags moved has its waiting runs put in again where their tenants stand
    /// now, each told if its place or reason changed.
    fn restand(&mut self, now: i64, out: &mut Vec<Reply>) {
        for c in Class::ALL {
            if !self.fair.get(c.index()).is_some_and(|f| f.moved) {
                continue;
            }
            let leaves: Vec<(u32, [u128; LEVELS])> = self.fair.get(c.index()).map(|f| f.leaves.iter().map(|(l, (st, _))| (*l, *st)).collect()).unwrap_or_default();
            if let Some(f) = self.fair.get_mut(c.index()) {
                f.moved = false;
            }
            let mut moves: Vec<(u32, [u128; LEVELS], [u128; LEVELS])> = Vec::new();
            // Standings still held in the line by tenants whose runs are put in again one by one,
            // after this loop: no block is taken from or put at them.
            let mut held: Vec<[u128; LEVELS]> = Vec::new();
            let mut blocks = 0u32;
            // A block's places before and after it moved, and its length.
            let mut block: Option<(u32, u32, u32)> = None;
            for (leaf, had) in leaves {
                let (path, depth) = self.paths.get(&leaf).copied().unwrap_or(([0; LEVELS], 0));
                let standing = self.standing_of(c, path.get(..usize::from(depth)).unwrap_or(&[]));
                if standing == had {
                    continue;
                }
                let Some(f) = self.fair.get_mut(c.index()) else { continue };
                // Alone where it stood and where it goes: its runs are one run of the order, moved
                // as a block, O(log n). Else each is put in again, ties interleaving by arrival.
                let alone = !f.leaves.iter().any(|(l, (st, _))| *l != leaf && (*st == had || *st == standing)) && !held.iter().any(|h| *h == had || *h == standing);
                let Some((st, set)) = f.leaves.get_mut(&leaf) else { continue };
                *st = standing;
                if alone {
                    let cx = class_ix(c);
                    let (lo, hi) = (Key { class: cx, standing: had, own: i128::MIN, seq: 0 }, Key { class: cx, standing: had, own: i128::MAX, seq: u64::MAX });
                    let (p, k) = (self.line.rank(&lo), self.line.rank(&hi).saturating_sub(self.line.rank(&lo)));
                    self.line.restand_block(cx, had, standing);
                    let q = self.line.rank(&Key { class: cx, standing, own: i128::MIN, seq: 0 });
                    block = Some((p, q, k));
                    blocks += 1;
                } else {
                    held.push(had);
                    moves.extend(set.iter().map(|s| (*s, had, standing)));
                }
            }
            // Each move looked at alone costs the boundaries before and after it, two runs at each
            // power of two, and the run itself: 4·⌈log2 n⌉ + 1 tells to look at. Looking at every
            // waiting run once costs n. Many moves at once (a tenant with many runs waiting, its
            // standing moved by a charge) are made together, then every run looked at once
            // (docs/needs.md §11, AUDIT P6: restand was O(k log² n) for k runs moved); so is a
            // tenant moved as a block.
            let n = u64::from(self.line.len());
            let per_move = 4 * u64::from(n.max(1).ilog2() + 1) + 1;
            let together = blocks > 0 || (moves.len() as u64).saturating_mul(per_move) > n;
            let one_block = blocks == 1 && moves.is_empty();
            for (s, had, standing) in moves {
                let Some(p) = self.get(s) else { continue };
                let (Some(stored), item) = (p.key, Item { slot: s, ask: p.amounts, predicted: p.predicted, timeout: p.ask.timeout }) else { continue };
                let (old, key) = (Key { standing: had, ..stored }, Key { standing, ..stored });
                self.line.remove(&old);
                if !together {
                    self.boundaries(now, out);
                }
                self.line.insert(key, item);
                if let Some(p) = self.get_mut(s) {
                    p.key = Some(key);
                }
                // The others moved a place at most, at the boundaries; this one may have moved
                // many, and is told itself.
                if !together {
                    self.boundaries(now, out);
                    self.tell_if_changed(s, now, out);
                }
            }
            match block {
                Some((p, q, k)) if one_block => self.tell_moved(p, q, k, now, out),
                _ if together => self.tell_all(now, out),
                _ => {}
            }
        }
    }

    /// After one block of `k` runs moved from place `p` to `q` in line (the first 0), each run
    /// whose place to within a power of two changed is told: the block's own runs, and of those it
    /// passed (each moved by `k` the other way) only those within `k` of a power of two, the only
    /// ones whose range a move of `k` can change. O(k log n) runs looked at, the full line only
    /// when that's fewer (docs/needs.md §11, AUDIT P6).
    fn tell_moved(&mut self, p: u32, q: u32, k: u32, now: i64, out: &mut Vec<Reply>) {
        let n = self.line.len();
        let bits = u64::from(n.max(1).ilog2() + 1);
        if u64::from(k).saturating_mul(bits.saturating_add(1)) >= u64::from(n) {
            self.tell_all(now, out);
            return;
        }
        let mut ranks: Vec<u32> = (q..q.saturating_add(k)).collect();
        // Those passed: back (q > p) they came forward by k, to [p, q); forward (q < p) they went
        // back by k, to [q + k, p + k). A run at place y (from 1) changed range iff a power of two
        // b lies in (y, y + k] (it came forward) or (y − k, y] (it went back).
        let (from, to, forward) = if q > p { (p, q, true) } else { (q.saturating_add(k), p.saturating_add(k), false) };
        let mut b: u32 = 1;
        while b <= n {
            // Places y (from 1) with the boundary b in range, as ranks y − 1.
            let (lo, hi) = if forward { (b.saturating_sub(k).max(1), b.saturating_sub(1)) } else { (b, b.saturating_add(k).saturating_sub(1)) };
            for y in lo..=hi {
                let r = y.saturating_sub(1);
                if (from..to).contains(&r) && y >= 1 {
                    ranks.push(r);
                }
            }
            let Some(next) = b.checked_mul(2) else { break };
            b = next;
        }
        ranks.sort_unstable();
        ranks.dedup();
        for r in ranks {
            if let Some((_, s)) = self.line.select(r) {
                self.tell_at(s, r, now, out);
            }
        }
    }

    /// Charges a run's tenant for its dispatch here (the engine's `charge`, W05-R32, R33): at every
    /// node from its repository down, its dominant share of what the runner offers times its median
    /// (else `default_cost`) over the node's weight, from where start-time fair queuing starts it
    /// under its parent; the parent's virtual time moves up to that start. Its class holds what it
    /// holds, and started a run here now.
    fn charge(&mut self, s: u32, now: i64) {
        let offers = self.offers();
        let Some(p) = self.get(s) else { return };
        let holds = self.holds(p);
        let shares = engine::shares(&holds, &offers);
        let mut fair = shares;
        // Where the runner offers no CPUs to count, the slot counts the CPUs the run asks, one at
        // the least (main's 8305d5c80, as the engine's `fair_shares`).
        if offers.get(1).copied().unwrap_or(0) == 0
            && kind_of(&p.ask) != Kind::Whole
            && let Some(slot) = fair.first_mut()
        {
            *slot = slot.saturating_mul(u128::from(p.ask.needs.request.cpu.get().max(1000))).div_ceil(1000);
        }
        let ms = p.ask.median.or(self.settings.default_cost).unwrap_or(0);
        let class = p.class;
        let (Some(Some(p)), Some(f)) = (self.slots.get(ix(s)), self.fair.get_mut(class.index())) else { return };
        let mut parent = 0;
        for (node, weight) in path_of(p).iter().zip(p.weights) {
            let c = engine::charges(&fair, ms, weight).into_iter().max().unwrap_or(0);
            if c > 0 {
                let v = f.vtime(parent);
                let start = v.max(f.finish_of(*node).unwrap_or(0));
                f.set_finish(*node, start.saturating_add(c));
                f.set_vtime(parent, v.max(start));
                f.moved = true;
            }
            parent = *node;
        }
        for (h, a) in f.held.iter_mut().zip(holds) {
            *h = h.saturating_add(u128::from(a));
        }
        f.served = Some(now);
        if let Some(p) = self.get_mut(s) {
            p.charge = Some(Charge { class, shares, fair, ms, at: now, holds, paced: None });
        }
    }

    /// Settles what a run's dispatch charged to the time it ran (`ran`; the engine's
    /// `settle_charge`): each node gives back the charge for its median and takes one for the time
    /// it ran; its class holds what it held no more, and a pace it set is settled alike.
    fn settle(&mut self, s: u32, ran: Ms) {
        let Some(c) = self.get_mut(s).and_then(|p| p.charge.take()) else { return };
        let (Some(Some(p)), Some(f)) = (self.slots.get(ix(s)), self.fair.get_mut(c.class.index())) else { return };
        for (h, a) in f.held.iter_mut().zip(c.holds) {
            *h = h.saturating_sub(u128::from(a));
        }
        if let (Some((took, left)), Some(at)) = (c.paced, f.paced.as_mut()) {
            *at = at.saturating_sub(took).saturating_add(engine::pace_ms(&c.shares, ran, left));
        }
        for (node, weight) in path_of(p).iter().zip(p.weights) {
            let charged = engine::charges(&c.fair, c.ms, weight).into_iter().max().unwrap_or(0);
            let used = engine::charges(&c.fair, ran, weight).into_iter().max().unwrap_or(0);
            match f.finish_of(*node) {
                Some(x) => {
                    f.set_finish(*node, x.saturating_sub(charged).saturating_add(used));
                    f.moved = true;
                }
                None if used > 0 => {
                    f.set_finish(*node, used);
                    f.moved = true;
                }
                None => {}
            }
        }
    }
}

// Classes: floors and promotion (W05-R31).
impl Admission {
    /// Whether a class is short of its floor (the engine's `short`): it has one, and its running
    /// runs hold less of the runner than that.
    fn short(&self, class: Class) -> bool {
        let floor = self.class_config(class).floor;
        floor > 0 && self.fair_of(class).map_or(0, |f| engine::held_share(&f.held, &self.offers())) < u128::from(floor) * 1000
    }

    /// A class short of its floor takes room ahead of the classes before it with run `s` (the
    /// engine's `ahead_of`): when the run keeps it within its floor (`Some(None)`), or else as the
    /// floor's pace allows, with what's left of its floor (`Some(Some(left))`).
    fn ahead_of(&self, class: Class, s: u32, now: i64) -> Option<Option<u128>> {
        let p = self.get(s)?;
        let f = self.fair_of(class)?;
        let (offers, floor) = (self.offers(), u128::from(self.class_config(class).floor) * 1000);
        let mut with = f.held;
        for (h, a) in with.iter_mut().zip(self.holds(p)) {
            *h = h.saturating_add(u128::from(a));
        }
        if engine::held_share(&with, &offers) <= floor {
            return Some(None);
        }
        let left = floor.saturating_sub(engine::held_share(&f.held, &offers)).max(1);
        f.paced.is_none_or(|at| now >= at).then_some(Some(left))
    }

    /// Run `s` went over its class's floor: the class may next do so once the pace it took has
    /// passed (the engine's `pace`).
    fn pace(&mut self, s: u32, left: u128, now: i64) {
        let Some(c) = self.get_mut(s).and_then(|p| p.charge.as_mut()) else { return };
        let took = engine::pace_ms(&c.shares, c.ms, left);
        c.paced = Some((took, left));
        let class = c.class;
        if let Some(f) = self.fair.get_mut(class.index()) {
            f.paced = Some(now.saturating_add(took));
        }
    }

    /// When to look at run `s`'s promotion from `class`, which it reached at `since`, if the class
    /// has a bound and one above it: a bound after.
    fn promote_at(&mut self, s: u32, since: i64, class: Class) {
        let Some(seq) = self.get(s).map(|p| p.seq) else { return };
        if let (Some(bound), Some(_)) = (self.class_config(class).bound, class.up()) {
            self.look_at(s, seq, later(since, bound), class);
        }
    }

    /// Run `s`'s look from `class`, which it reached at `since`, kept on it but not filed.
    fn plan_look(&mut self, s: u32, since: i64, class: Class) {
        if let (Some(bound), Some(_)) = (self.class_config(class).bound, class.up())
            && let Some(p) = self.get_mut(s)
        {
            p.look = Some((later(since, bound), class));
        }
    }

    /// Run `s`'s look filed in `promotions`, if it has one not filed.
    fn file_look(&mut self, s: u32) {
        let Some((seq, Some((at, class)))) = self.get(s).map(|p| (p.seq, p.look)) else { return };
        let v = self.promotions.entry(at).or_default();
        if !v.contains(&(s, seq, class)) {
            v.push((s, seq, class));
        }
    }

    fn look_at(&mut self, s: u32, seq: u64, at: i64, class: Class) {
        self.promotions.entry(at).or_default().push((s, seq, class));
        if let Some(p) = self.get_mut(s) {
            p.look = Some((at, class));
        }
    }

    /// A run leaves the line (offered, or gone): its looks are taken out, kept on the run.
    fn unlook(&mut self, s: u32) {
        let Some((seq, look, next, class)) = self.get(s).map(|p| (p.seq, p.look, p.watch.next, p.class)) else { return };
        let drop = |m: &mut BTreeMap<i64, Vec<(u32, u64, Class)>>, at: i64, e: (u32, u64, Class)| {
            if let Some(v) = m.get_mut(&at) {
                if let Some(i) = v.iter().position(|x| *x == e) {
                    v.swap_remove(i);
                }
                if v.is_empty() {
                    m.remove(&at);
                }
            }
        };
        if let Some((at, from)) = look {
            drop(&mut self.promotions, at, (s, seq, from));
        }
        if let Some(at) = next {
            drop(&mut self.behinds, at, (s, seq, class));
        }
    }

    /// A run waits again (its offer lapsed, or its owner's link went): its looks are put back if
    /// they're still to come.
    fn relook(&mut self, s: u32, now: i64) {
        let Some((seq, look, next, class)) = self.get(s).map(|p| (p.seq, p.look, p.watch.next, p.class)) else { return };
        let file = |m: &mut BTreeMap<i64, Vec<(u32, u64, Class)>>, at: i64, e: (u32, u64, Class)| {
            if at > now && !m.get(&at).is_some_and(|v| v.contains(&e)) {
                m.entry(at).or_default().push(e);
            }
        };
        if let Some((at, from)) = look {
            file(&mut self.promotions, at, (s, seq, from));
        }
        if let Some(at) = next {
            file(&mut self.behinds, at, (s, seq, class));
        }
    }

    /// Each look due by `now`, the earliest first (the engine's `promote_due`, the reference's
    /// `promote`): a run's bound is, once the work ahead of it is due done ([`Watch`]), the time
    /// from its wait's start to then; else its class's, if a person set one. A run that has been in
    /// its class its bound goes up a class if its class started nothing here in the bound before
    /// (else that look is taken again a bound after that start). A promoted run keeps its own order
    /// and arrival, and preempts nothing. The runs that go up at one time all go up before any
    /// begins its wait in its new class, so what's ahead of each there doesn't depend on the order
    /// they were looked at in.
    fn promote_due(&mut self, now: i64, out: &mut Vec<Reply>) {
        loop {
            let first = [self.promotions.first_key_value(), self.behinds.first_key_value()].into_iter().flatten().map(|(at, _)| *at).min();
            let Some(at) = first.filter(|at| *at <= now) else { return };
            let mut due = self.promotions.remove(&at).unwrap_or_default();
            due.extend(self.behinds.remove(&at).unwrap_or_default());
            let mut up: Vec<u32> = Vec::new();
            for (s, seq, from) in due {
                let Some(p) = self.get(s).filter(|p| p.seq == seq && p.class == from) else { continue };
                let (is_look, is_next) = (p.look == Some((at, from)), p.watch.next == Some(at));
                if !is_look && !is_next {
                    continue;
                }
                let waits = matches!(p.state, State::Waiting | State::Aside);
                let derived = p.watch.due.filter(|d| *d <= at).map(|d| d.saturating_sub(p.watch.since).max(0));
                let bound = derived.or_else(|| self.class_config(from).bound.map(|b| i64::try_from(b).unwrap_or(i64::MAX)));
                let (Some(bound), Some(to), true) = (bound, from.up(), waits) else {
                    if let Some(p) = self.get_mut(s) {
                        if is_look {
                            p.look = None;
                        }
                        if is_next {
                            p.watch.next = None;
                        }
                    }
                    continue;
                };
                if let Some(last) = self.fair_of(from).and_then(|f| f.served).filter(|t| *t > at.saturating_sub(bound)) {
                    let again = last.saturating_add(bound);
                    if is_look {
                        self.look_at(s, seq, again, from);
                    }
                    if is_next {
                        if let Some(p) = self.get_mut(s) {
                            p.watch.next = Some(again);
                        }
                        self.behinds.entry(again).or_default().push((s, seq, from));
                    }
                    continue;
                }
                self.reclass(s, to, at, now, out);
                up.push(s);
            }
            // Each come to its class at the look, all of them in it: what's ahead of it then, and
            // what of that is due already is due from then.
            if !up.is_empty() {
                self.came = self.came.saturating_add(1);
                let stamp = self.came;
                for s in up {
                    self.start_watch(s, stamp, at);
                }
                self.dues(at);
            }
        }
    }

    /// Run `s` comes to class `to` at `at`: in the line at its place there, and looked at for its
    /// promotion from there by its class's bound (its wait there begins once every run going up
    /// at `at` has, [`Self::promote_due`]).
    fn reclass(&mut self, s: u32, to: Class, at: i64, now: i64, out: &mut Vec<Reply>) {
        let waiting = self.get(s).is_some_and(|p| p.state == State::Waiting);
        if waiting {
            self.leave(s);
            self.boundaries(now, out);
        }
        self.unlook(s);
        self.came = self.came.saturating_add(1);
        let came = self.came;
        if let Some(p) = self.get_mut(s) {
            p.class = to;
            if let Some(stamp) = p.stamps.get_mut(to.index()) {
                *stamp = came;
            }
            p.look = None;
            p.watch = Watch::default();
        }
        if waiting {
            self.enter(s);
            self.boundaries(now, out);
            self.tell_if_changed(s, now, out);
        }
        self.promote_at(s, at, to);
    }

    /// Each run in the line none of whose work ahead waits any more (each that was waiting when
    /// its wait began has started since, or left, and none waits now, as one put back would), its
    /// time to go up not set yet: its bound is due at the latest expected end of that work still
    /// holding room here, or `now` (the engine's `ahead_due`).
    fn dues(&mut self, now: i64) {
        for c in Class::ALL.into_iter().filter(|c| watched(*c)) {
            let i = c.index();
            let bound = self.ahead_waiting.get(i).and_then(|w| w.first()).map_or(u64::MAX, |(b, _)| *b);
            while let Some((at, s)) = self.watching.get(i).and_then(|w| w.first()).copied().filter(|(at, _)| *at <= bound) {
                if let Some(w) = self.watching.get_mut(i) {
                    w.remove(&(at, s));
                }
                let Some(seq) = self.get(s).map(|p| p.seq) else { continue };
                let due = self.ahead_claims.get(i).map_or(now, |claims| claims.range(..(at, 0)).filter_map(|(_, c)| self.get(*c)).filter_map(|c| c.due.map(|d| d.at(now).expected)).fold(now, i64::max));
                if let Some(p) = self.get_mut(s) {
                    p.watch.due = Some(due);
                    p.watch.next = Some(due);
                }
                self.behinds.entry(due).or_default().push((s, seq, c));
            }
        }
    }

    /// The runs in the line with nothing of the classes before theirs ahead of them yet, now that
    /// some of that work is here: their wait behind it begins now ([`Watch`]).
    fn arm(&mut self, now: i64) {
        for c in Class::ALL.into_iter().filter(|c| watched(*c)) {
            let i = c.index();
            let here = self.ahead_waiting.get(i).is_some_and(|w| !w.is_empty()) || self.ahead_claims.get(i).is_some_and(|w| !w.is_empty());
            if !here || self.unarmed.get(i).is_none_or(BTreeSet::is_empty) {
                continue;
            }
            self.came = self.came.saturating_add(1);
            let at = self.came;
            for s in self.unarmed.get_mut(i).map(std::mem::take).unwrap_or_default() {
                if let Some(p) = self.get_mut(s) {
                    p.watch = Watch { at, since: now, armed: true, due: None, next: None };
                }
                if let Some(w) = self.watching.get_mut(i) {
                    w.insert((at, s));
                }
            }
        }
    }

    /// What every input does first (as the engine's `handle` does): the looks due by now, and the
    /// runs whose tenants' standing moved put where they stand.
    fn begin(&mut self, now: i64, out: &mut Vec<Reply>) {
        self.promote_due(now, out);
        self.restand(now, out);
    }

    /// What every input does last, once it's done, so the order its runs were handled in doesn't
    /// matter: the waits behind the work of the classes before theirs that begin now, then the
    /// times to go up of those whose work ahead no longer waits.
    fn close(&mut self, now: i64) {
        self.arm(now);
        self.dues(now);
    }
}

// Admission, kept room and news.
impl Admission {
    /// Whether a probe could start now (W05-R03, the engine's `room_in`): nothing kiln placed holds
    /// the runner whole; one that takes it whole finds nothing else of kiln's here (no claim, no CI
    /// leg); a share fits in what's not held, never overcommitted; any other fits beside.
    fn can_start(&self, s: u32) -> bool {
        let Some(p) = self.get(s) else { return false };
        if self.whole_claims > 0 {
            return false;
        }
        match kind_of(&p.ask) {
            // Empty, and what it asks within what's measured free: the engine's runner is its
            // declared size, this one's room is measured, and memory is never overcommitted.
            Kind::Whole => self.claims == 0 && self.sample.ci_jobs == 0 && ask::fits(&p.amounts, &self.free_now()),
            Kind::Share => ask::fits(&ask::plus(&self.held, &p.amounts), &self.base),
            Kind::Ordinary => ask::fits(&p.amounts, &self.free_now()),
        }
    }


    /// The first of `class` in order that can start and that room kept for the first in line lets
    /// start ahead of it (W05-R39: it fits beside the room kept, nothing beside a run that takes the
    /// runner whole, or it's predicted to end by the room's time): with room kept, the earlier of
    /// the first that fits beside it and the first predicted to end by its time, each found by the line's own search, so that what it passes over is passed by
    /// what each subtree asks at the least and runs at the shortest, not looked at one by one
    /// (docs/needs.md §11, AUDIT P6: the test inside the search saw every run waiting).
    fn first_that_starts(&self, class: u8, free: &Amounts, kept: Option<(u32, Option<i64>)>, now: i64) -> Option<u32> {
        let find = |free: &Amounts, within: u64| self.line.find(&Query { class: Some(class), after: None, free, within }, &mut |s| self.can_start(s));
        let Some((k, at)) = kept else { return find(free, u64::MAX).map(|(_, s)| s) };
        let h = self.get(k)?;
        let beside = (kind_of(&h.ask) != Kind::Whole).then(|| ask::less(free, &h.amounts)).and_then(|b| find(&b, u64::MAX));
        let by = at.and_then(|at| u64::try_from(at.saturating_sub(now)).ok()).and_then(|within| find(free, within));
        match (beside, by) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a.1 } else { b.1 }),
            (a, b) => a.or(b).map(|(_, s)| s),
        }
    }

    /// Offers what can start, as the engine's pick takes it (W05-R04, R30, R31): of each class, the
    /// first in order that has room; the first class's, unless a class after it is short of its
    /// floor and may go ahead. With `backfill` declared, the first in line that has no room keeps
    /// room (W05-R39). Each offer is one decision, O(log n) in the line.
    fn admit(&mut self, now: i64, out: &mut Vec<Reply>) {
        loop {
            self.restand(now, out);
            if self.draining || self.strained.is_some() {
                self.kept = None;
                return;
            }
            let Some((_, hs)) = self.line.first() else {
                self.kept = None;
                return;
            };
            let head_whole = self.get(hs).is_some_and(|p| kind_of(&p.ask) == Kind::Whole);
            #[cfg(test)]
            let starve = head_whole && super::mutant() == Some(super::Mutant::ExclusiveStarves);
            #[cfg(not(test))]
            let starve = { let _ = head_whole; false };
            let kept = if self.settings.backfill && !starve && !self.can_start(hs) {
                let at = self.kept_time(hs, now);
                Some((hs, at))
            } else {
                None
            };
            self.kept = kept;
            let free = self.free_now();
            // Of each class anyone waits in (its bit in the line's mask, lowest first), the first
            // that can start.
            let mut bests: [Option<u32>; 5] = [None; 5];
            let mut mask = self.line.classes();
            while mask != 0 {
                let c = mask.trailing_zeros();
                mask &= mask.wrapping_sub(1);
                let (Some(best), Ok(class)) = (bests.get_mut(c as usize), u8::try_from(c)) else { continue };
                *best = self.first_that_starts(class, &free, kept, now);
            }
            let mut found = Class::ALL.into_iter().zip(bests).filter_map(|(c, b)| b.map(|s| (c, s)));
            let Some((_, first)) = found.next() else { return };
            let ahead = found.filter(|(c, _)| self.short(*c)).find_map(|(c, s)| self.ahead_of(c, s, now).map(|over| (s, over)));
            let (s, over) = ahead.unwrap_or((first, None));
            self.offer(s, now, out);
            if let Some(left) = over {
                self.pace(s, left, now);
            }
        }
    }

    /// When the first in line would have room, as the claims here end at their predicted ends:
    /// `None` when that's never known (a claim it waits for has no prediction or ran past it, or a
    /// CI leg holds a runner it would take whole).
    fn kept_time(&self, s: u32, now: i64) -> Option<i64> {
        let p = self.get(s)?;
        let (need, whole) = (p.amounts, kind_of(&p.ask) == Kind::Whole);
        if whole && self.sample.ci_jobs > 0 {
            return None;
        }
        // The first claim, by predicted end from now on, by whose end what's freed lets it in
        // (each claim past its predicted end left out: when it ends isn't known): O(log n).
        let free = self.free_now();
        let (claims, whole_claims) = (self.claims, self.whole_claims);
        let room = |sum: &super::line::Sums| {
            let mut freed = free;
            for (f, w) in freed.iter_mut().zip(&sum.work) {
                *f = u64::try_from(u128::from(*f).saturating_add(*w)).unwrap_or(u64::MAX);
            }
            let left = claims.saturating_sub(sum.count);
            let whole_left = whole_claims.saturating_sub(sum.untimed);
            if whole { left == 0 } else { whole_left == 0 && ask::fits(&need, &freed) }
        };
        self.ends.first_from(&end_key(now, 0), &mut |sum| room(sum)).and_then(|k| i64::try_from(k.own).ok())
    }

    /// Gives a waiting probe room: offered to its owner under a new claim, held for the owner's
    /// link's retransmission timeout, its tenant charged for it.
    fn offer(&mut self, s: u32, now: i64, out: &mut Vec<Reply>) {
        let free = self.free_now();
        let fence = self.next_fence();
        let Some(p) = self.get(s) else { return };
        let o = p.owner;
        let until = later(now, self.rto(o));
        self.leave(s);
        self.unlook(s);
        if let Some(p) = self.get_mut(s) {
            p.state = State::Offered { fence, until };
        }
        self.claim(s, now);
        self.charge(s, now);
        self.deadlines.insert((until, s));
        self.say_offer(s, Some(free), out);
        self.boundaries(now, out);
    }

    fn say_offer(&self, s: u32, free: Option<Amounts>, out: &mut Vec<Reply>) {
        let Some(p) = self.get(s) else { return };
        let State::Offered { fence, .. } = p.state else { return };
        if self.owner(p.owner).is_none_or(|w| w.away) {
            return;
        }
        let free = ask::resources(&free.unwrap_or_else(|| self.free_now()));
        out.push(Reply { owner: p.owner, token: p.token, run: Arc::clone(&p.run), msg: Up::Offer { fence, got: ask::resources(&p.amounts), free, predicted: p.predicted, disk: self.sample.disk } });
    }

    /// The runs at the boundaries between powers of two in the line, and the first: each told its
    /// place and reason if either changed. After one arrival or departure, only these can have
    /// changed their place's range (docs/needs.md §11: O(log n) of them, each O(log n) to find).
    fn boundaries(&mut self, now: i64, out: &mut Vec<Reply>) {
        if self.stale {
            self.stale = false;
            self.tell_all(now, out);
            return;
        }
        let n = self.line.len();
        let mut k: u32 = 1;
        // Up to the boundary just past the line's end: a line of 3 has its 3rd at 4 - 1.
        while k.saturating_sub(1) <= n {
            for r in [k.saturating_sub(1), k] {
                if (1..=n).contains(&r)
                    && let Some((_, s)) = self.line.select(r.saturating_sub(1))
                {
                    self.tell_if_changed(s, now, out);
                }
            }
            let Some(next) = k.checked_mul(2) else { break };
            k = next;
        }
    }

    /// Tells every waiting probe whose place or reason changed (a drain, an owner back): O(n).
    fn tell_all(&mut self, now: i64, out: &mut Vec<Reply>) {
        // In order: each one's place is where it comes, no search for it.
        let mut slots = std::mem::take(&mut self.order);
        self.line.slots_into(&mut slots);
        for (rank, s) in slots.iter().enumerate() {
            self.tell_at(*s, u32::try_from(rank).unwrap_or(u32::MAX), now, out);
        }
        self.order = slots;
    }

    fn tell_if_changed(&mut self, s: u32, now: i64, out: &mut Vec<Reply>) {
        let Some(rank) = self.rank_of(s) else { return };
        self.tell_at(s, rank, now, out);
    }

    /// Tells waiting probe `s`, at place `rank` (the first 0), where it stands, if that changed.
    fn tell_at(&mut self, s: u32, rank: u32, now: i64, out: &mut Vec<Reply>) {
        if self.hush == Some(s) {
            return;
        }
        let told = self.told_of(s, rank);
        let Some(p) = self.get(s) else { return };
        if p.told == Some(told) {
            return;
        }
        #[cfg(test)]
        if p.told.is_some() && super::mutant() == Some(super::Mutant::QuietPlace) {
            return;
        }
        let msg = self.waiting_msg(s, rank, now);
        let to = self.owner(p.owner).filter(|w| !w.away).map(|_| (p.owner, p.token, Arc::clone(&p.run)));
        if let Some(p) = self.get_mut(s) {
            p.told = Some(told);
        }
        if let (Some((owner, token, run)), Some(msg)) = (to, msg) {
            out.push(Reply { owner, token, run, msg });
        }
    }

    /// A waiting probe's place in line, the first 0.
    fn rank_of(&self, s: u32) -> Option<u32> {
        let p = self.get(s)?;
        match (p.state, self.key_of(s)) {
            (State::Waiting, Some(k)) => Some(self.line.rank(&k)),
            _ => None,
        }
    }

    fn told_of(&self, s: u32, rank: u32) -> Told {
        Told { place: Place::of(rank.saturating_add(1)), reason: self.reason(s, rank).kind() }
    }

    /// A waiting probe's one reason, at place `rank` (the first 0).
    fn reason(&self, s: u32, rank: u32) -> Reason {
        if self.draining {
            return Reason::Draining;
        }
        if let Some(why) = &self.strained {
            return Reason::Strained { why: why.clone() };
        }
        if rank > 0 {
            return Reason::Line;
        }
        let free = self.free_now();
        let Some(p) = self.get(s) else { return Reason::Line };
        let kept = match self.kept {
            Some((k, at)) if k == s => Some(at),
            _ => None,
        };
        if let Some(Some(at)) = kept {
            return Reason::Kept { at: Some(at) };
        }
        if ask::fits(&p.amounts, &free) && (self.whole_claims > 0 || kind_of(&p.ask) == Kind::Whole) {
            return Reason::Whole { jobs: self.claims.saturating_add(self.sample.ci_jobs) };
        }
        match kept {
            Some(at) => Reason::Kept { at },
            None => Reason::Room { short: shortfall(&ask::resources(&p.amounts), &ask::resources(&free)) },
        }
    }

    /// What a waiting probe's owner is told of where it stands.
    fn waiting_msg(&self, s: u32, rank: u32, now: i64) -> Option<Up> {
        let p = self.get(s)?;
        let k = self.key_of(s)?;
        Some(Up::Waiting {
            reason: self.reason(s, rank),
            place: Place::of(rank.saturating_add(1)),
            start: self.estimate(&k, now),
            predicted: p.predicted,
            free: ask::resources(&self.free_now()),
        })
    }

    /// When the run at `key` would start here, from the sums ahead of it in line ([`Line::ahead`],
    /// O(log n)) and the claims holding room ([`super::msg::Estimate`]). The soonest shares each
    /// dimension's room out as a fluid among the work ahead (amount times predicted time) and what
    /// the claims have left; the latest has every claim run to its timeout, then each run ahead run
    /// alone to its timeout.
    fn estimate(&self, key: &Key, now: i64) -> Estimate {
        let ahead = self.line.ahead(key);
        let soonest = if ahead.unpredicted == 0 && self.unpredicted_claims == 0 {
            // What the claims predicted to end from now on hold until then: each amount times
            // its end, less now times each amount (O(log n), from the sums by end).
            let mut work = ahead.work;
            let from = end_key(now, 0);
            let (held, held_by_end) = (less_sums(&self.ends.all(), &self.ends.ahead(&from)), less_sums(&self.ends_at.all(), &self.ends_at.ahead(&from)));
            for ((w, a), at) in work.iter_mut().zip(held).zip(held_by_end) {
                *w = w.saturating_add(at.saturating_sub(a.saturating_mul(u128::from(shifted(now)))));
            }
            let mut wait: Option<u128> = Some(0);
            for (w, a) in work.iter().zip(&self.avail) {
                if *w == 0 {
                    continue;
                }
                wait = match (wait, *a) {
                    (Some(t), a) if a > 0 => Some(t.max(w.div_ceil(u128::from(a)))),
                    _ => None,
                };
            }
            wait.map(|t| now.saturating_add(i64::try_from(t).unwrap_or(i64::MAX)))
        } else {
            None
        };
        let latest = if ahead.untimed == 0 && self.untimed_claims == 0 {
            let last = self.hard_ends.iter().next_back().map_or(now, |(at, _)| (*at).max(now));
            Some(last.saturating_add(i64::try_from(ahead.timeouts).unwrap_or(i64::MAX)))
        } else {
            None
        };
        Estimate { soonest, latest }
    }

    /// What a probe's owner is told of it as it stands now (a probe said again, an owner back).
    fn standing(&mut self, s: u32, now: i64, out: &mut Vec<Reply>) {
        let Some(p) = self.get(s) else { return };
        if self.owner(p.owner).is_none_or(|w| w.away) {
            return;
        }
        let (state, predicted, owner, token, run) = (p.state, p.predicted, p.owner, p.token, Arc::clone(&p.run));
        let reply = |msg| Reply { owner, token, run: Arc::clone(&run), msg };
        match state {
            State::Offered { .. } => self.say_offer(s, None, out),
            State::Taken { fence } => out.push(reply(Up::Held { fence })),
            State::Running { .. } => out.push(reply(Up::Ran)),
            State::Aside => out.push(reply(Up::Waiting { reason: Reason::Away, place: Place::default(), start: Estimate::default(), predicted, free: ask::resources(&self.free_now()) })),
            State::Waiting => {
                if let Some(p) = self.get_mut(s) {
                    p.told = None;
                }
                self.tell_if_changed(s, now, out);
            }
        }
    }
}

/// The wire's inputs and outputs: each input as its `_r` twin has it, its words made [`Said`]s.
impl Admission {
    /// The name of its owners' process `o`, as a [`Reply`] names it.
    pub fn owner_name(&self, o: u32) -> &str {
        self.owner(o).map_or("", |w| w.name.as_str())
    }

    /// A [`Reply`] as the wire has it.
    pub fn said(&self, r: Reply) -> Said {
        (self.owner_name(r.owner).to_owned(), r.msg.wire(&r.run))
    }

    fn wire(&mut self, out: &mut Vec<Said>, f: impl FnOnce(&mut Admission, &mut Vec<Reply>)) {
        let mut r = std::mem::take(&mut self.replies);
        f(self, &mut r);
        for x in r.drain(..) {
            let said = self.said(x);
            out.push(said);
        }
        self.replies = r;
    }

    /// [`Self::probe_r`], with no handle of the owner's.
    pub fn probe(&mut self, from: Peer<'_>, ask: impl Into<Arc<Ask>>, predicted: Option<Ms>, now: i64, out: &mut Vec<Said>) {
        let ask = ask.into();
        self.wire(out, |a, r| a.probe_r(from, ask, 0, predicted, now, r));
    }

    pub fn withdraw(&mut self, from: Peer<'_>, run: &str, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.withdraw_r(from, run, now, r));
    }

    pub fn take(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.take_r(from, run, fence, now, r));
    }

    pub fn decline(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.decline_r(from, run, fence, now, r));
    }

    pub fn start(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) -> Option<Start> {
        let mut started = None;
        self.wire(out, |a, r| started = a.start_r(from, run, fence, now, r));
        started
    }

    pub fn spawned(&mut self, from: Peer<'_>, run: &str, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.spawned_r(from, run, r));
    }

    pub fn ended(&mut self, from: Peer<'_>, run: &str, ok: bool, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.ended_r(from, run, ok, now, r));
    }

    pub fn measured(&mut self, sample: Sample, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.measured_r(sample, now, r));
    }

    pub fn drain(&mut self, draining: bool, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.drain_r(draining, now, r));
    }

    pub fn away(&mut self, name: &str, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.away_r(name, now, r));
    }

    pub fn back(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.back_r(from, now, r));
    }

    pub fn forget(&mut self, name: &str, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.forget_r(name, now, r));
    }

    pub fn predict(&mut self, from: Peer<'_>, run: &str, predicted: Option<Ms>, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.predict_r(from, run, predicted, now, r));
    }

    pub fn tick(&mut self, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.tick_r(now, r));
    }
}

/// A probe as a view or a check sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub owner: String,
    pub epoch: u64,
    pub run: String,
    /// `aside`, `waiting`, `offered`, `taken` or `running`.
    pub state: &'static str,
    pub fence: Option<Fence>,
    pub ask: Amounts,
    /// Its class now (W05-R31: as it came, or as its wait raised it).
    pub class: Class,
}

// The inputs: owners' words, the runner's own samples and ends, and the time.
impl Admission {
    /// Why this runner could never run `ask` as asked, if it couldn't: a constraint its own facts
    /// don't meet, more than kiln may place here even idle, or a gang (commands start together with
    /// docs/needs.md E5's gangs, not yet). A disk whose size isn't measured is no reason.
    fn never(&self, ask: &Ask) -> Option<String> {
        for src in &ask.needs.requires {
            match Constraint::parse(src) {
                Ok(c) if c.eval(&self.facts, &self.labels) => {}
                Ok(c) => {
                    let facts = c.facts().iter().map(|f| format!("{f} = {}", crate::sched::routing::fact(&self.facts, f).show())).collect::<Vec<_>>().join(", ");
                    return Some(if facts.is_empty() { format!("not {src}") } else { format!("not {src} ({facts})") });
                }
                Err(e) => return Some(e),
            }
        }
        if ask.needs.together.is_some() {
            return Some("commands can't start together yet".into());
        }
        let (q, s) = (&ask.needs.request, ask::resources(&self.cap()));
        let short: Vec<String> = [
            (q.cpu > s.cpu).then(|| format!("{}, under {}", crate::sched::needs::cpus(s.cpu), crate::sched::needs::cpus(q.cpu))),
            (q.mem > s.mem).then(|| format!("{} of memory, under {}", crate::sched::needs::show_size(s.mem), crate::sched::needs::show_size(q.mem))),
            (s.disk > 0 && q.disk > s.disk).then(|| format!("{} of disk, under {}", crate::sched::needs::show_size(s.disk), crate::sched::needs::show_size(q.disk))),
            (q.gpu > s.gpu).then(|| format!("{} GPUs, under {}", s.gpu, q.gpu)),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!short.is_empty()).then(|| short.join(", "))
    }

    fn record(&self, name: &str, epoch: u64, run: &str) -> Option<Record> {
        let by_epoch = self.records.get(name)?;
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::ReusedIdRan) {
            return by_epoch.values().find_map(|runs| runs.get(run)).copied();
        }
        by_epoch.get(&epoch).and_then(|runs| runs.get(run)).copied()
    }

    fn record_mut(&mut self, name: &str, epoch: u64, run: &str) -> Option<&mut Record> {
        self.records.get_mut(name).and_then(|m| m.get_mut(&epoch)).and_then(|m| m.get_mut(run))
    }

    /// If the record has the run, tells its owner it ran (or how it ended): true.
    fn recorded(&self, from: Peer<'_>, o: u32, token: u64, run: &str, out: &mut Vec<Reply>) -> bool {
        let Some(r) = self.record(from.name, from.epoch, run) else { return false };
        let msg = match r.ended {
            None => Up::Ran,
            Some(ok) => Up::Ended { fence: r.fence, ok },
        };
        out.push(Reply { owner: o, token, run: Arc::from(run), msg });
        true
    }

    /// The probe of `run` of the owner's process `from`, current or not.
    fn find(&self, from: Peer<'_>, run: &str) -> Option<u32> {
        let o = self.incarnation(from.name, from.epoch)?;
        self.owner(o)?.runs.get(run).copied()
    }

    fn state(&self, s: u32) -> Option<State> {
        self.get(s).map(|p| p.state)
    }

    /// A claim's predicted and hard ends, worked out again from `from`.
    fn retime(&mut self, s: u32, from: i64) {
        let Some(p) = self.get(s) else { return };
        let (old_end, old_hard, amounts, whole) = (p.end, p.hard, p.amounts, kind_of(&p.ask) == Kind::Whole);
        let end = p.predicted.map(|ms| later(from, ms));
        let hard = p.ask.timeout.map(|ms| later(from, ms));
        match old_end {
            Some(at) => self.drop_end(at, s),
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_sub(1),
        }
        match old_hard {
            Some(at) => {
                self.hard_ends.remove(&(at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_sub(1),
        }
        match end {
            Some(at) => self.add_end(at, s, amounts, whole),
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_add(1),
        }
        match hard {
            Some(at) => {
                self.hard_ends.insert((at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_add(1),
        }
        if let Some(p) = self.get_mut(s) {
            p.end = end;
            p.hard = hard;
        }
    }

    /// An owner probes it with `ask`, its run predicted to take `predicted` here (the shell's
    /// prediction from history, with this runner's disk among what it knows; `None`: unknown):
    /// answered with an offer, where it waits, that it never could, or that it ran it already; a
    /// probe again is answered as it stands.
    pub fn probe_r(&mut self, from: Peer<'_>, ask: impl Into<Arc<Ask>>, token: u64, predicted: Option<Ms>, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.probe_in(from, ask.into(), token, predicted, now, out);
        self.close(now);
    }

    fn probe_in(&mut self, from: Peer<'_>, ask: Arc<Ask>, token: u64, predicted: Option<Ms>, now: i64, out: &mut Vec<Reply>) {
        let Some(o) = self.speaker(from, now, out) else { return };
        if self.recorded(from, o, token, &ask.run, out) {
            return;
        }
        if let Some(s) = self.owner(o).and_then(|w| w.runs.get(ask.run.as_str())).copied() {
            self.standing(s, now, out);
            return;
        }
        if let Some(why) = self.never(&ask) {
            out.push(Reply { owner: o, token, run: Arc::from(ask.run.as_str()), msg: Up::Never { why } });
            return;
        }
        let amounts = ask::amounts(&ask.needs.request);
        let leaf = engine::leaf_of(&self.settings.fair_by, &ask.repo, &ask.actor, &ask.lane);
        let (mut path, mut depth) = ([0u32; LEVELS], 0u8);
        for (slot, node) in path.iter_mut().zip(engine::nodes(&leaf)) {
            *slot = self.nodes.intern(node);
            depth = depth.saturating_add(1);
        }
        let leaf_id = path.get(usize::from(depth).saturating_sub(1)).copied().unwrap_or(0);
        self.paths.entry(leaf_id).or_insert((path, depth));
        let weights = [self.weight(&ask.repo), 1000, 1000];
        let own = engine::own_order(ask.priority, self.settings.aging_per_minute, now);
        self.seq = self.seq.saturating_add(1);
        self.came = self.came.saturating_add(1);
        let (seq, came, class) = (self.seq, self.came, ask.class);
        let mut stamps = [u64::MAX; 5];
        if let Some(stamp) = stamps.get_mut(class.index()) {
            *stamp = came;
        }
        let run: Arc<str> = Arc::from(ask.run.as_str());
        let p = Probe { owner: o, token, run: Arc::clone(&run), ask, amounts, predicted, state: State::Aside, key: None, path, depth, weights, class, own, seq, stamps, watch: Watch::default(), charge: None, end: None, hard: None, due: None, told: None, look: None };
        let s = match self.spare.pop() {
            Some(s) => {
                if let Some(slot) = self.slots.get_mut(ix(s)) {
                    *slot = Some(p);
                }
                s
            }
            None => {
                self.slots.push(Some(p));
                u32::try_from(self.slots.len().saturating_sub(1)).unwrap_or(u32::MAX)
            }
        };
        let away = match self.owners.get_mut(ix(o)) {
            Some(w) => {
                w.runs.insert(run, s);
                w.away
            }
            None => true,
        };
        // Its wait behind the work of the classes before its own begins as it comes (W05-R31).
        self.start_watch(s, came, now);
        // Its promotion's look, filed once it's known to wait (one offered in this input would only
        // take it out again; it's kept on the run, filed if it ever waits).
        self.plan_look(s, now, class);
        if away {
            self.file_look(s);
            return;
        }
        // Nobody waits here and it has room: it's offered as it comes, which is what entering the
        // line, admitting and leaving it again come to with no one else in it (docs/needs.md §11,
        // AUDIT P6: an arrival at an empty line touches no line).
        if self.line.is_empty() && !self.draining && self.strained.is_none() && self.can_start(s) {
            self.kept = None;
            self.offer(s, now, out);
            return;
        }
        // One change to the line at a time, each looked at: its arrival, then what admitting does;
        // the run itself told where it stands once admitting has.
        self.enter(s);
        self.hush = Some(s);
        self.boundaries(now, out);
        self.admit(now, out);
        self.hush = None;
        self.boundaries(now, out);
        if self.get(s).is_some_and(|p| matches!(p.state, State::Waiting | State::Aside)) {
            self.file_look(s);
        }
        if self.get(s).is_some_and(|p| p.state == State::Waiting && p.told.is_none()) {
            self.tell_if_changed(s, now, out);
        }
    }

    /// The owner withdraws a run (placed elsewhere, ended, not wanted): its place or claim goes,
    /// its room free in the same input. A running run isn't stopped by it: that's its own signal's.
    pub fn withdraw_r(&mut self, from: Peer<'_>, run: &str, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.withdraw_in(from, run, now, out);
        self.close(now);
    }

    fn withdraw_in(&mut self, from: Peer<'_>, run: &str, now: i64, out: &mut Vec<Reply>) {
        let Some(o) = self.speaker(from, now, out) else { return };
        let Some(s) = self.owner(o).and_then(|w| w.runs.get(run)).copied() else { return };
        if matches!(self.state(s), Some(State::Running { .. })) {
            return;
        }
        // One change to the line at a time, each looked at: a run moves at most one place.
        self.drop_probe(s);
        self.boundaries(now, out);
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// The owner takes offer `fence`: held until it starts the run, if that's still its live claim.
    pub fn take_r(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.take_in(from, run, fence, now, out);
        // Offers that lapsed by now went back to their places: what they free is offered.
        self.admit(now, out);
        self.boundaries(now, out);
        self.close(now);
    }

    fn take_in(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) {
        self.lapse_due(now, out);
        let Some(o) = self.speaker(from, now, out) else { return };
        if self.recorded(from, o, 0, run, out) {
            return;
        }
        let s = self.owner(o).and_then(|w| w.runs.get(run)).copied();
        let (token, name) = self.handles(s, run);
        let reply = |msg| Reply { owner: o, token, run: Arc::clone(&name), msg };
        match s.and_then(|s| self.state(s).map(|st| (s, st))) {
            Some((s, State::Offered { fence: f, until })) if f == fence => {
                self.deadlines.remove(&(until, s));
                if let Some(p) = self.get_mut(s) {
                    p.state = State::Taken { fence };
                }
                out.push(reply(Up::Held { fence }));
            }
            Some((_, State::Taken { fence: f })) if f == fence => out.push(reply(Up::Held { fence })),
            Some((_, State::Running { .. })) => out.push(reply(Up::Ran)),
            _ => out.push(reply(Up::Lapsed { fence, why: "its offer isn't held for it any more" })),
        }
    }

    /// The owner declines offer `fence` (it took another's): its room is free in the same input,
    /// and the run leaves the line.
    pub fn decline_r(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.decline_in(from, run, fence, now, out);
        self.close(now);
    }

    fn decline_in(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) {
        let Some(o) = self.speaker(from, now, out) else { return };
        let Some(s) = self.owner(o).and_then(|w| w.runs.get(run)).copied() else { return };
        // An offered or taken claim isn't in the line: dropping it moves no run's place.
        if matches!(self.state(s), Some(State::Offered { fence: f, .. } | State::Taken { fence: f }) if f == fence) {
            self.drop_probe(s);
            self.admit(now, out);
            self.boundaries(now, out);
        }
    }

    /// The owner starts the run under claim `fence`: [`Start::New`] if that's its live claim (offered
    /// or taken), the run now its runner's to start and never to start again (the shell writes the
    /// record, [`Self::records`], before it starts the process, and says [`Self::spawned`] once it
    /// has); [`Start::Already`] if it runs or ran here (its owner told so: a start said again starts
    /// nothing, and is never told as a lapse); `None`, its owner told the claim lapsed: the one word
    /// on which it may place the run elsewhere.
    pub fn start_r(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) -> Option<Start> {
        self.begin(now, out);
        let started = self.start_in(from, run, fence, now, out);
        // Offers that lapsed by now went back to their places: what they free is offered.
        self.admit(now, out);
        self.boundaries(now, out);
        self.close(now);
        started
    }

    fn start_in(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Reply>) -> Option<Start> {
        self.lapse_due(now, out);
        let o = self.speaker(from, now, out)?;
        if self.recorded(from, o, 0, run, out) {
            return Some(Start::Already);
        }
        let s = self.owner(o).and_then(|w| w.runs.get(run)).copied();
        let state = s.and_then(|s| self.state(s));
        let (token, name) = self.handles(s, run);
        if let (Some(_), Some(State::Running { .. })) = (s, state) {
            out.push(Reply { owner: o, token, run: name, msg: Up::Ran });
            return Some(Start::Already);
        }
        let live = matches!(state, Some(State::Offered { fence: f, until }) if f == fence && until > now) || matches!(state, Some(State::Taken { fence: f }) if f == fence);
        #[cfg(test)]
        let live = live || (s.is_some() && super::mutant() == Some(super::Mutant::StartLapsed));
        let Some(s) = s.filter(|_| live) else {
            out.push(Reply { owner: o, token, run: name, msg: Up::Lapsed { fence, why: "its claim isn't live here" } });
            return None;
        };
        match state {
            Some(State::Offered { until, .. }) => {
                self.deadlines.remove(&(until, s));
                self.retime(s, now);
            }
            Some(State::Taken { .. }) => self.retime(s, now),
            // Only a planted bug starts what holds no claim: it holds one from now.
            _ => {
                self.leave(s);
                if let Some(p) = self.get_mut(s) {
                    p.state = State::Running { fence, from: now };
                }
                self.claim(s, now);
            }
        }
        if let Some(p) = self.get_mut(s) {
            p.state = State::Running { fence, from: now };
        }
        let record = Record { fence, ended: None };
        match self.records.get_mut(from.name) {
            Some(by_epoch) => {
                by_epoch.entry(from.epoch).or_default().insert(run.to_owned(), record);
            }
            None => {
                self.records.entry(from.name.to_owned()).or_default().entry(from.epoch).or_default().insert(run.to_owned(), record);
            }
        }
        Some(Start::New)
    }

    /// Its shell started the run's process: its owner is told it started (the runner's word).
    pub fn spawned_r(&mut self, from: Peer<'_>, run: &str, out: &mut Vec<Reply>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(State::Running { fence, .. }) = self.state(s) else { return };
        if let Some((owner, token, run)) = self.told_owner(s) {
            out.push(Reply { owner, token, run, msg: Up::Started { fence } });
        }
    }

    /// Whom to tell of probe `s`'s run: its owner, if its link is up and it's still the process
    /// that asked. A run of an owner's earlier epoch runs on to its end, but its words would reach
    /// the owner's new process by its name alone, which may have reused the id for another run
    /// (kiln-sim place, seed 88): it's told nothing.
    fn told_owner(&self, s: u32) -> Option<(u32, u64, Arc<str>)> {
        let p = self.get(s)?;
        let w = self.owner(p.owner).filter(|w| !w.away)?;
        (self.current(&w.name) == Some(p.owner)).then(|| (p.owner, p.token, Arc::clone(&p.run)))
    }

    /// A probe's token and run, or, with none, no token and `run` itself.
    fn handles(&self, s: Option<u32>, run: &str) -> (u64, Arc<str>) {
        match s.and_then(|s| self.get(s)) {
            Some(p) => (p.token, Arc::clone(&p.run)),
            None => (0, Arc::from(run)),
        }
    }

    /// A run it started ended (`ok`: its process exited; not: it couldn't start, or was lost): its
    /// room is free in the same input, its tenant charged what it ran, and its owner told.
    pub fn ended_r(&mut self, from: Peer<'_>, run: &str, ok: bool, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.ended_in(from, run, ok, now, out);
        self.close(now);
    }

    fn ended_in(&mut self, from: Peer<'_>, run: &str, ok: bool, now: i64, out: &mut Vec<Reply>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(State::Running { fence, .. }) = self.state(s) else { return };
        // From its dispatch, as the engine settles a grant.
        let at = self.get(s).and_then(|p| p.charge.map(|c| c.at)).unwrap_or(now);
        self.settle(s, u64::try_from(now.saturating_sub(at)).unwrap_or(0));
        if let Some(r) = self.record_mut(from.name, from.epoch, run) {
            r.ended = Some(ok);
        }
        let to = self.told_owner(s);
        self.drop_probe(s);
        if let Some((owner, token, run)) = to {
            out.push(Reply { owner, token, run, msg: Up::Ended { fence, ok } });
        }
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// The owner heard how its run ended (or that it ran here before): its record goes. The record
    /// of a run still running stays.
    pub fn ack(&mut self, from: Peer<'_>, run: &str) {
        if self.record(from.name, from.epoch, run).is_none_or(|r| r.ended.is_none()) {
            return;
        }
        let Some(by_epoch) = self.records.get_mut(from.name) else { return };
        if let Some(runs) = by_epoch.get_mut(&from.epoch) {
            runs.remove(run);
            if runs.is_empty() {
                by_epoch.remove(&from.epoch);
            }
        }
        if by_epoch.is_empty() {
            self.records.remove(from.name);
        }
    }

    /// A new sample: the room is worked out again, and what now fits is offered.
    pub fn measured_r(&mut self, sample: Sample, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.sample = sample;
        let mut own = [0; ask::DIMS];
        for u in &self.sample.runs {
            if let Some(s) = self.find(Peer { name: &u.owner, epoch: u.epoch }, &u.run)
                && let Some(p) = self.get(s)
                && matches!(p.state, State::Running { .. })
            {
                own = ask::plus(&own, &ask::least(&ask::amounts(&u.used), &p.amounts));
            }
        }
        self.own = own;
        self.remeasure();
        self.admit(now, out);
        self.boundaries(now, out);
        self.close(now);
    }

    /// Its machine under strain (`why`), or not (AUDIT L1, as the shell measured it at decision
    /// time): strained, it offers nothing new, and its runs wait saying why; no longer, what fits
    /// starts in the same input.
    pub fn strain(&mut self, why: Option<String>, now: i64, out: &mut Vec<Said>) {
        self.wire(out, |a, r| a.strain_r(why, now, r));
    }

    /// [`Self::strain`], its replies by handle.
    pub fn strain_r(&mut self, why: Option<String>, now: i64, out: &mut Vec<Reply>) {
        if self.strained == why {
            return;
        }
        self.begin(now, out);
        self.strained = why;
        self.admit(now, out);
        self.tell_all(now, out);
        self.close(now);
    }

    /// Draining (an upgrade pending) or not: draining, it offers nothing new.
    pub fn drain_r(&mut self, draining: bool, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.draining = draining;
        self.admit(now, out);
        self.tell_all(now, out);
        self.close(now);
    }

    /// An owner's link went down: its probes leave the line, offered nothing, an offer it hadn't
    /// taken lapses at once; a claim it took stays held, as does what runs: only its absence
    /// ([`Self::forget`]) or its epoch changing ends them.
    pub fn away_r(&mut self, name: &str, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.away_in(name, now, out);
        self.close(now);
    }

    fn away_in(&mut self, name: &str, now: i64, out: &mut Vec<Reply>) {
        let Some(o) = self.current(name) else { return };
        let slots: Vec<u32> = match self.owners.get_mut(ix(o)) {
            Some(w) => {
                w.away = true;
                w.runs.values().copied().collect()
            }
            None => return,
        };
        for s in slots {
            match self.state(s) {
                Some(State::Waiting) => {
                    self.leave(s);
                    self.boundaries(now, out);
                }
                Some(State::Offered { .. }) => {
                    self.release(s);
                    if let Some(p) = self.get_mut(s) {
                        p.state = State::Aside;
                    }
                    self.relook(s, now);
                }
                _ => {}
            }
        }
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// An owner's link is back: its probes are in the line again at the places they had, and it's
    /// told where each stands, and how each run that ended meanwhile ended.
    pub fn back_r(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.back_in(from, now, out);
        self.close(now);
    }

    fn back_in(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Reply>) {
        let Some(o) = self.speaker(from, now, out) else { return };
        let slots: Vec<u32> = match self.owners.get_mut(ix(o)) {
            Some(w) => {
                w.away = false;
                w.runs.values().copied().collect()
            }
            None => return,
        };
        for s in &slots {
            if self.state(*s) == Some(State::Aside) {
                self.enter(*s);
                self.boundaries(now, out);
            }
        }
        self.admit(now, out);
        self.boundaries(now, out);
        for s in slots {
            if !matches!(self.state(s), Some(State::Offered { .. })) {
                self.standing(s, now, out);
            }
        }
        let ended: Vec<(String, Record)> = self.records.get(from.name).and_then(|m| m.get(&from.epoch)).map(|runs| runs.iter().filter(|(_, r)| r.ended.is_some()).map(|(run, r)| (run.clone(), *r)).collect()).unwrap_or_default();
        for (run, r) in ended {
            out.push(Reply { owner: o, token: 0, run: Arc::from(run.as_str()), msg: Up::Ended { fence: r.fence, ok: r.ended == Some(true) } });
        }
    }

    /// An owner is gone (its lease ended, or the directory no longer has its node): what it left
    /// waiting or claimed goes, its room free; what runs runs on to its end. Its record stays, so
    /// that if it comes back in the same epoch nothing of its starts twice.
    pub fn forget_r(&mut self, name: &str, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        let incarnations: Vec<u32> = self.owner_ix.get(name).map(|v| v.iter().map(|(_, o)| *o).collect()).unwrap_or_default();
        for o in incarnations {
            let slots: Vec<u32> = self.owner(o).map(|w| w.runs.values().copied().collect()).unwrap_or_default();
            for s in slots {
                if !matches!(self.state(s), Some(State::Running { .. })) {
                    self.drop_probe(s);
                    self.stale = true;
                }
            }
        }
        self.admit(now, out);
        self.boundaries(now, out);
        self.close(now);
    }

    /// The link to an owner measured again (its round trip, RFC 6298): what its offers are held by.
    pub fn link(&mut self, name: &str, rtt: Rtt) {
        if let Some(o) = self.current(name)
            && let Some(w) = self.owners.get_mut(ix(o))
        {
            w.rtt = Some(rtt);
        }
    }

    /// A run's predicted time here, predicted again by the shell (a run past its prediction, given
    /// how long it has run; history learned more): its place in line stays, its claim's end moves.
    pub fn predict_r(&mut self, from: Peer<'_>, run: &str, predicted: Option<Ms>, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.predict_in(from, run, predicted, now, out);
        self.close(now);
    }

    fn predict_in(&mut self, from: Peer<'_>, run: &str, predicted: Option<Ms>, now: i64, out: &mut Vec<Reply>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(p) = self.get_mut(s) else { return };
        p.predicted = predicted;
        let (state, item) = (p.state, Item { slot: s, ask: p.amounts, predicted, timeout: p.ask.timeout });
        let key = self.key_of(s);
        match (state, key) {
            (State::Waiting, Some(k)) => {
                self.line.remove(&k);
                self.line.insert(k, item);
            }
            (State::Offered { .. } | State::Taken { .. }, _) => self.retime(s, now),
            (State::Running { from, .. }, _) => self.retime(s, from),
            _ => {}
        }
        self.admit(now, out);
        self.boundaries(now, out);
    }

    fn lapse_due(&mut self, now: i64, out: &mut Vec<Reply>) {
        while let Some(&(until, s)) = self.deadlines.first() {
            if until > now {
                break;
            }
            self.deadlines.remove(&(until, s));
            let Some(State::Offered { fence, .. }) = self.state(s) else { continue };
            self.release(s);
            let Some(p) = self.get(s) else { continue };
            let to = self.owner(p.owner).filter(|w| !w.away).map(|_| (p.owner, p.token, Arc::clone(&p.run)));
            self.enter(s);
            self.relook(s, now);
            if let Some((owner, token, run)) = to {
                out.push(Reply { owner, token, run, msg: Up::Lapsed { fence, why: "its offer wasn't taken in time" } });
            }
            self.boundaries(now, out);
            self.tell_if_changed(s, now, out);
        }
    }

    /// The time: promotion's looks due by now are taken, offers not taken within their link's
    /// timeout lapse (each run waits again at its place, its owner told), and what that frees is
    /// offered.
    pub fn tick_r(&mut self, now: i64, out: &mut Vec<Reply>) {
        self.begin(now, out);
        self.lapse_due(now, out);
        self.admit(now, out);
        self.boundaries(now, out);
        self.close(now);
    }

    /// When it's to be told the time again ([`Self::tick`]): the soonest offer to lapse or
    /// promotion's look to take, whichever is first.
    pub fn timer(&self) -> Option<i64> {
        [self.deadlines.first().map(|(at, _)| *at), self.promotions.first_key_value().map(|(at, _)| *at), self.behinds.first_key_value().map(|(at, _)| *at)].into_iter().flatten().min()
    }

    /// The record of runs started here, as the shell keeps it on disk.
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// What it publishes of its room (the `offers` named feature): what's free for an ordinary run
    /// now, nothing while draining or held whole. A hint for owners choosing where to probe, never
    /// a claim.
    pub fn hint(&self) -> Resources {
        if self.draining || self.strained.is_some() || self.whole_claims > 0 {
            return Resources::default();
        }
        ask::resources(&self.free_now())
    }

    /// The room it admits against now, and what its claims hold of it.
    pub fn room(&self) -> (Amounts, Amounts) {
        (self.avail, self.held)
    }

    /// How many wait in its line.
    pub fn queued(&self) -> u32 {
        self.line.len()
    }

    /// How many wait in its line of each class, in [`Class::ALL`]'s order: O(log n) each.
    pub fn queued_by_class(&self) -> [u32; 5] {
        let mut out = [0u32; 5];
        for (o, c) in out.iter_mut().zip(Class::ALL) {
            *o = self.line.count_of(class_ix(c));
        }
        out
    }

    /// Each probe here, for a view and the checks.
    pub fn probes(&self) -> Vec<View> {
        let mut out = Vec::new();
        for p in self.slots.iter().flatten() {
            let (state, fence) = match p.state {
                State::Aside => ("aside", None),
                State::Waiting => ("waiting", None),
                State::Offered { fence, .. } => ("offered", Some(fence)),
                State::Taken { fence } => ("taken", Some(fence)),
                State::Running { fence, .. } => ("running", Some(fence)),
            };
            let (owner, epoch) = self.owner(p.owner).map_or((String::new(), 0), |w| (w.name.clone(), w.epoch));
            out.push(View { owner, epoch, run: p.run.to_string(), state, fence, ask: p.amounts, class: p.class });
        }
        out
    }

    /// What a waiting run's owner should have heard last, as news counts it: its place's range and
    /// its reason's kind (for the property that every waiting asker hears each change).
    pub fn truth(&self, from: Peer<'_>, run: &str) -> Option<(Place, (u8, Option<i64>))> {
        let s = self.find(from, run)?;
        let rank = self.rank_of(s)?;
        let t = self.told_of(s, rank);
        Some((t.place, t.reason))
    }

    /// Its own books against what it holds: what's held is the sum of its claims, within what kiln
    /// may place here in every dimension (G1); one claim that takes the runner whole holds it alone;
    /// the line holds exactly the runs waiting, each where its tenant stands. O(n): for tests and
    /// the model.
    pub fn check(&self) -> Result<(), String> {
        let mut held = [0u64; ask::DIMS];
        let (mut claims, mut whole, mut waiting) = (0u32, 0u32, 0u32);
        for (s, p) in self.slots.iter().enumerate().filter_map(|(s, p)| p.as_ref().map(|p| (s, p))) {
            match p.state {
                State::Offered { .. } | State::Taken { .. } | State::Running { .. } => {
                    held = ask::plus(&held, &p.amounts);
                    claims = claims.saturating_add(1);
                    whole = whole.saturating_add(u32::from(kind_of(&p.ask) == Kind::Whole));
                }
                State::Waiting => {
                    waiting = waiting.saturating_add(1);
                    let standing = self.standing_of(p.class, path_of(p));
                    let key = self.key_of(u32::try_from(s).unwrap_or(u32::MAX));
                    if key.is_none_or(|k| k.standing != standing || k.class != class_ix(p.class) || !self.line.contains(&k)) && !self.fair_of(p.class).is_some_and(|f| f.moved) {
                        return Err(format!("{}: {} (slot {s}) waits at {:?}, its tenant standing at {standing:?} in {}", self.me, p.run, key, p.class.as_str()));
                    }
                }
                State::Aside => {}
            }
        }
        if held != self.held || claims != self.claims {
            return Err(format!("{}: its books say {:?} held by {} claims, its claims hold {held:?} ({claims})", self.me, self.held, self.claims));
        }
        if !ask::fits(&held, &self.cap()) {
            return Err(format!("{}: its claims hold {held:?}, past what kiln may place here, {:?} (G1)", self.me, self.cap()));
        }
        if whole > 0 && claims > 1 {
            return Err(format!("{}: a claim of the runner whole beside {} others", self.me, claims.saturating_sub(1)));
        }
        if waiting != self.line.len() {
            return Err(format!("{}: {waiting} waiting, its line holds {}", self.me, self.line.len()));
        }
        // Work-conserving: what can start is offered in the input that let it, nobody polling.
        if !self.draining
            && self.strained.is_none()
            && let Some((_, h)) = self.line.first()
            && self.can_start(h)
        {
            return Err(format!("{}: the first in line could start, and wasn't offered", self.me));
        }
        Ok(())
    }
}
