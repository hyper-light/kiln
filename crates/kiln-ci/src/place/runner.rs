//! The runner's side of placement (docs/reach.md §2; docs/needs.md §5, §6, §8, §9 and E5): it
//! admits what owners probe it with against its own measured room, and its ledger is the last word
//! on that room (G1). Every event is O(log n) in the runs waiting here, but for news to an owner
//! whose link went down or came back, which is a message for each of its runs.
//!
//! - **Its line** ([`super::line::Line`]): class first (W05-R30), then fair share within the class,
//!   start-time fair queuing (Goyal, Vin and Cheng, SIGCOMM 1996) over the runs' tenants (repository
//!   and actor), each run's start tag set as it comes: the later of the class's virtual time and its
//!   tenant's last finish tag, its finish tag that plus its cost, its dominant share of this runner
//!   times its predicted run time over its repository's weight (DRF's dominant share, Ghodsi et al.,
//!   NSDI 2011; docs/needs.md §5). A run with no prediction costs nothing until it ends, when what it
//!   ran is charged to its tenant: start-time fair queuing needs no time beforehand.
//! - **Admission**: the first in line starts if it fits what's free (its room never more than the
//!   budget, memory, disk and GPUs never overcommitted, docs/needs.md §3). If it doesn't and
//!   `backfill` is declared, room is kept for it (EASY backfilling, Lifka 1995; W05-R39): from when
//!   the claims here are predicted to have left it room; only what fits beside that room, or is
//!   predicted to end by then, starts ahead of it. A run that takes the runner whole is kept room
//!   the same way, so a stream of small runs never starves it.
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

use super::ask::{self, Amounts, Ask, Peer, Sample};
use super::line::{Item, Key, Line, Query};
use super::link::Rtt;
use super::msg::{Estimate, Place, Reason, ToOwner, shortfall};
use crate::facts::RunnerFacts;
use crate::sched::routing::Constraint;
use crate::sched::state::{Fence, PerMille, Resources};
use crate::sched::units::Ms;

/// What a runner admits by: each a setting the person decides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
}

/// A message for an owner: whose (its node's name), and what.
pub type Said = (String, ToOwner);

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

/// What a waiting run's owner was last told, as news counts it: its place and its reason's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Told {
    place: Place,
    reason: (u8, Option<i64>),
}

#[derive(Debug, Clone)]
struct Probe {
    owner: u32,
    run: String,
    ask: Ask,
    amounts: Amounts,
    predicted: Option<Ms>,
    state: State,
    /// Its place in the line, from when it first came in: it keeps it while aside.
    key: Option<Key>,
    tenant: String,
    /// What fair share charged it as it came in.
    charged: u128,
    /// A claim's predicted end and hard end (its timeout), when it has them.
    end: Option<i64>,
    hard: Option<i64>,
    told: Option<Told>,
}

/// An owner's process, as this runner hears it.
#[derive(Debug, Clone)]
struct OwnerAt {
    name: String,
    epoch: u64,
    away: bool,
    runs: BTreeMap<String, u32>,
    rtt: Option<Rtt>,
}

/// One class's fair share: its virtual time, and each tenant's last finish tag.
#[derive(Debug, Clone, Default)]
struct Fair {
    vtime: u128,
    finish: BTreeMap<String, u128>,
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
    /// What it may admit against, from the last sample, and what its claims hold of it.
    avail: Amounts,
    held: Amounts,
    /// What its running claims use within what they asked, from the last sample.
    own: Amounts,
    claims: u32,
    exclusive_claims: u32,
    unpredicted_claims: u32,
    untimed_claims: u32,
    draining: bool,
    slots: Vec<Option<Probe>>,
    spare: Vec<u32>,
    owners: Vec<OwnerAt>,
    /// Each owner's processes by name: (epoch, index), the last the current one.
    owner_ix: BTreeMap<String, Vec<(u64, u32)>>,
    line: Line,
    seq: u64,
    fair: [Fair; 5],
    /// Offers by when they lapse; claims by predicted end and by hard end.
    deadlines: BTreeSet<(i64, u32)>,
    ends: BTreeSet<(i64, u32)>,
    hard_ends: BTreeSet<(i64, u32)>,
    records: Records,
    /// Room kept for the first in line: its slot, and from when.
    kept: Option<(u32, Option<i64>)>,
    /// Many left the line at once (an owner gone or started again): every waiting run's place is
    /// looked at again, not only the boundaries'.
    stale: bool,
}

fn ix(s: u32) -> usize {
    s as usize
}

fn later(now: i64, ms: Ms) -> i64 {
    now.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
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
            held: [0; ask::DIMS],
            own: [0; ask::DIMS],
            claims: 0,
            exclusive_claims: 0,
            unpredicted_claims: 0,
            untimed_claims: 0,
            draining: false,
            slots: Vec::new(),
            spare: Vec::new(),
            owners: Vec::new(),
            owner_ix: BTreeMap::new(),
            line: Line::default(),
            seq: 0,
            fair: Default::default(),
            deadlines: BTreeSet::new(),
            ends: BTreeSet::new(),
            hard_ends: BTreeSet::new(),
            records,
            kept: None,
            stale: false,
        };
        a.avail = a.room_now();
        let _ = now;
        a
    }

    /// Its name and epoch.
    pub fn me(&self) -> (&str, u64) {
        (&self.me, self.epoch)
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

    /// What kiln may place here in all: the budget, else the machine (CPUs as overcommitted).
    pub fn cap(&self) -> Amounts {
        let size = self.settings.budget.as_ref().map_or_else(|| ask::amounts(&self.sample.size), ask::amounts);
        let [cpu, mem, disk, gpu] = size;
        let cpu = u64::try_from(u128::from(cpu).saturating_mul(u128::from(self.settings.overcommit_cpu.max(1000))) / 1000).unwrap_or(u64::MAX);
        [cpu, mem, disk, gpu]
    }

    fn room_now(&self) -> Amounts {
        ask::avail(&self.sample, self.settings.budget.as_ref(), &self.own, self.settings.overcommit_cpu)
    }

    /// What's free for another run now: the room less what's held.
    fn free_now(&self) -> Amounts {
        let free = ask::less(&self.avail, &self.held);
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::OfferPastRoom) {
            let [cpu, mem, disk, gpu] = free;
            return [cpu, mem.saturating_mul(2), disk, gpu];
        }
        free
    }

    fn weight(&self, repo: &str) -> PerMille {
        self.settings.weights.get(repo).copied().unwrap_or(1000).max(1)
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
        self.owners.push(OwnerAt { name: from.name.to_owned(), epoch: from.epoch, away: false, runs: BTreeMap::new(), rtt: None });
        self.owner_ix.entry(from.name.to_owned()).or_default().push((from.epoch, o));
        Some(o)
    }

    /// The owner process a word comes from ([`Self::owner_for`]); if that retired an earlier one,
    /// every waiting run is told where it stands now.
    fn speaker(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Said>) -> Option<u32> {
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
        let (amounts, exclusive) = (p.amounts, p.ask.needs.exclusive);
        let end = p.predicted.map(|ms| later(from, ms));
        let hard = p.ask.timeout.map(|ms| later(from, ms));
        self.held = ask::plus(&self.held, &amounts);
        self.claims = self.claims.saturating_add(1);
        if exclusive {
            self.exclusive_claims = self.exclusive_claims.saturating_add(1);
        }
        match end {
            Some(at) => {
                self.ends.insert((at, s));
            }
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

    /// A probe's claim stops holding room.
    fn release(&mut self, s: u32) {
        let Some(p) = self.get(s) else { return };
        let (state, amounts, exclusive, end, hard) = (p.state, p.amounts, p.ask.needs.exclusive, p.end, p.hard);
        if let State::Offered { until, .. } = state {
            self.deadlines.remove(&(until, s));
        }
        if !matches!(state, State::Offered { .. } | State::Taken { .. } | State::Running { .. }) {
            return;
        }
        self.held = ask::less(&self.held, &amounts);
        self.claims = self.claims.saturating_sub(1);
        if exclusive {
            self.exclusive_claims = self.exclusive_claims.saturating_sub(1);
        }
        match end {
            Some(at) => {
                self.ends.remove(&(at, s));
            }
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_sub(1),
        }
        match hard {
            Some(at) => {
                self.hard_ends.remove(&(at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_sub(1),
        }
        if let Some(p) = self.get_mut(s) {
            p.end = None;
            p.hard = None;
        }
    }

    /// Takes a probe out of the line, if it's in it.
    fn leave(&mut self, s: u32) {
        let Some(p) = self.get(s) else { return };
        if let (State::Waiting, Some(k)) = (p.state, p.key) {
            self.line.remove(&k);
            if self.kept.is_some_and(|(k, _)| k == s) {
                self.kept = None;
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
        self.release(s);
        let Some(p) = self.slots.get_mut(ix(s)).and_then(Option::take) else { return };
        if let Some(w) = self.owners.get_mut(ix(p.owner)) {
            w.runs.remove(&p.run);
        }
        self.spare.push(s);
    }

    /// Puts a probe in the line: at the place it had, or, coming in for the first time, at its
    /// start tag in its class's fair share.
    fn enter(&mut self, s: u32) {
        let cap = self.cap();
        let Some(p) = self.get(s) else { return };
        let (class, had) = (p.ask.class, p.key);
        let key = match had {
            Some(k) => k,
            None => {
                let cost = u128::from(ask::dominant_ppm(&p.amounts, &cap)).saturating_mul(u128::from(p.predicted.unwrap_or(0))).saturating_mul(1000) / u128::from(self.weight(&p.ask.repo));
                self.seq = self.seq.saturating_add(1);
                let seq = self.seq;
                // The slot and the fair share borrowed apart, so the tenant's name is copied only
                // the first time it's seen.
                let Some(Some(p)) = self.slots.get(ix(s)) else { return };
                let Some(f) = self.fair.get_mut(class.index()) else { return };
                let tag = match f.finish.get_mut(p.tenant.as_str()) {
                    Some(last) => {
                        let tag = f.vtime.max(*last);
                        *last = tag.saturating_add(cost);
                        tag
                    }
                    None => {
                        let tag = f.vtime;
                        f.finish.insert(p.tenant.clone(), tag.saturating_add(cost));
                        tag
                    }
                };
                if let Some(p) = self.get_mut(s) {
                    p.charged = cost;
                }
                Key { class: u8::try_from(class.index()).unwrap_or(u8::MAX), tag, seq }
            }
        };
        let Some(p) = self.get_mut(s) else { return };
        p.key = Some(key);
        p.state = State::Waiting;
        p.told = None;
        let item = Item { slot: s, ask: p.amounts, predicted: p.predicted, timeout: p.ask.timeout };
        self.line.insert(key, item);
    }

    /// What a run that ended charges its tenant: what it ran, at its dominant share of this
    /// runner, less what it was charged as it came (start-time fair queuing settles at the end).
    fn settle(&mut self, s: u32, ran: Ms) {
        let cap = self.cap();
        let Some(p) = self.get(s) else { return };
        let cost = u128::from(ask::dominant_ppm(&p.amounts, &cap)).saturating_mul(u128::from(ran)).saturating_mul(1000) / u128::from(self.weight(&p.ask.repo));
        let (charged, class) = (p.charged, p.ask.class);
        // The slot and the fair share borrowed apart, so the tenant's name isn't copied.
        let Some(Some(p)) = self.slots.get(ix(s)) else { return };
        let Some(f) = self.fair.get_mut(class.index()) else { return };
        let vtime = f.vtime;
        let caught_up = match f.finish.get_mut(p.tenant.as_str()) {
            Some(last) => {
                *last = last.saturating_add(cost).saturating_sub(charged);
                *last <= vtime
            }
            None => false,
        };
        if caught_up {
            f.finish.remove(p.tenant.as_str());
        }
    }
}

// Admission, kept room and news.
impl Admission {
    /// Whether a probe could start now in `free`: it fits, nothing kiln placed holds the runner
    /// whole, and one that takes it whole finds nothing else of kiln's here (no claim, no CI leg).
    fn can_start(&self, s: u32, free: &Amounts) -> bool {
        let Some(p) = self.get(s) else { return false };
        if self.exclusive_claims > 0 || (p.ask.needs.exclusive && (self.claims > 0 || self.sample.ci_jobs > 0)) {
            return false;
        }
        ask::fits(&p.amounts, free)
    }

    /// Offers what can start, first in line first, keeping room for the first that can't when
    /// `backfill` is declared (W05-R39): each offer is one decision, O(log n).
    fn admit(&mut self, now: i64, out: &mut Vec<Said>) {
        loop {
            if self.draining {
                self.kept = None;
                return;
            }
            let Some((hk, hs)) = self.line.first() else {
                self.kept = None;
                return;
            };
            let free = self.free_now();
            if self.can_start(hs, &free) {
                self.offer(hs, now, out);
                continue;
            }
            let head_whole = self.get(hs).is_some_and(|p| p.ask.needs.exclusive);
            #[cfg(test)]
            let starve = head_whole && super::mutant() == Some(super::Mutant::ExclusiveStarves);
            #[cfg(not(test))]
            let starve = false;
            let found = if self.settings.backfill && !starve {
                let at = self.kept_time(hs, now);
                self.kept = Some((hs, at));
                let need = self.get(hs).map(|p| p.amounts).unwrap_or_default();
                let beside = ask::less(&free, &need);
                // What fits beside the room kept (nothing beside a run that takes it whole).
                let a = if head_whole { None } else { self.line.find(&Query { after: Some(&hk), free: &beside, within: u64::MAX }, &mut |s| self.can_start(s, &beside)) };
                // What's predicted to end by the time the room is kept from.
                let b = at.and_then(|at| {
                    let within = u64::try_from(at.saturating_sub(now)).unwrap_or(0);
                    self.line.find(&Query { after: Some(&hk), free: &free, within }, &mut |s| self.can_start(s, &free))
                });
                match (a, b) {
                    (Some(x), Some(y)) => Some(if x.0 <= y.0 { x } else { y }),
                    (x, y) => x.or(y),
                }
            } else {
                self.kept = None;
                self.line.find(&Query { after: Some(&hk), free: &free, within: u64::MAX }, &mut |s| self.can_start(s, &free))
            };
            match found {
                Some((_, s)) => self.offer(s, now, out),
                None => return,
            }
        }
    }

    /// When the first in line would have room, as the claims here end at their predicted ends:
    /// `None` when that's never known (a claim it waits for has no prediction or ran past it, or a
    /// CI leg holds a runner it would take whole).
    fn kept_time(&self, s: u32, now: i64) -> Option<i64> {
        let p = self.get(s)?;
        let (need, whole) = (p.amounts, p.ask.needs.exclusive);
        if whole && self.sample.ci_jobs > 0 {
            return None;
        }
        let mut free = self.free_now();
        let (mut left, mut whole_left) = (self.claims, self.exclusive_claims);
        for (end, c) in &self.ends {
            if *end < now {
                // Past its predicted end: when it ends isn't known.
                continue;
            }
            let Some(claim) = self.get(*c) else { continue };
            free = ask::plus(&free, &claim.amounts);
            left = left.saturating_sub(1);
            if claim.ask.needs.exclusive {
                whole_left = whole_left.saturating_sub(1);
            }
            let room = if whole { left == 0 } else { whole_left == 0 && ask::fits(&need, &free) };
            if room {
                return Some(*end);
            }
        }
        None
    }

    /// Gives a waiting probe room: offered to its owner under a new claim, held for the owner's
    /// link's retransmission timeout.
    fn offer(&mut self, s: u32, now: i64, out: &mut Vec<Said>) {
        let free = self.free_now();
        let fence = self.next_fence();
        let Some(p) = self.get(s) else { return };
        let (o, key) = (p.owner, p.key);
        let until = later(now, self.rto(o));
        self.leave(s);
        if let Some(p) = self.get_mut(s) {
            p.state = State::Offered { fence, until };
        }
        self.claim(s, now);
        self.deadlines.insert((until, s));
        if let Some(k) = key
            && let Some(f) = self.fair.get_mut(usize::from(k.class))
        {
            f.vtime = f.vtime.max(k.tag);
        }
        self.say_offer(s, Some(free), out);
        self.boundaries(now, out);
    }

    fn say_offer(&self, s: u32, free: Option<Amounts>, out: &mut Vec<Said>) {
        let Some(p) = self.get(s) else { return };
        let State::Offered { fence, .. } = p.state else { return };
        let Some(w) = self.owner(p.owner).filter(|w| !w.away) else { return };
        let free = ask::resources(&free.unwrap_or_else(|| self.free_now()));
        out.push((w.name.clone(), ToOwner::Offer { run: p.run.clone(), fence, got: ask::resources(&p.amounts), free, predicted: p.predicted, disk: self.sample.disk }));
    }

    /// The runs at the boundaries between powers of two in the line, and the first: each told its
    /// place and reason if either changed. After one arrival or departure, only these can have
    /// changed their place's range (docs/needs.md §11: O(log n) of them, each O(log n) to find).
    fn boundaries(&mut self, now: i64, out: &mut Vec<Said>) {
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
    fn tell_all(&mut self, now: i64, out: &mut Vec<Said>) {
        for (_, s) in self.line.entries() {
            self.tell_if_changed(s, now, out);
        }
    }

    fn tell_if_changed(&mut self, s: u32, now: i64, out: &mut Vec<Said>) {
        let Some(rank) = self.rank_of(s) else { return };
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
        let to = self.owner(p.owner).filter(|w| !w.away).map(|w| w.name.clone());
        if let Some(p) = self.get_mut(s) {
            p.told = Some(told);
        }
        if let (Some(to), Some(msg)) = (to, msg) {
            out.push((to, msg));
        }
    }

    /// A waiting probe's place in line, the first 0.
    fn rank_of(&self, s: u32) -> Option<u32> {
        let p = self.get(s)?;
        match (p.state, p.key) {
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
        if ask::fits(&p.amounts, &free) && (self.exclusive_claims > 0 || p.ask.needs.exclusive) {
            return Reason::Whole { jobs: self.claims.saturating_add(self.sample.ci_jobs) };
        }
        match kept {
            Some(at) => Reason::Kept { at },
            None => Reason::Room { short: shortfall(&ask::resources(&p.amounts), &ask::resources(&free)) },
        }
    }

    /// What a waiting probe's owner is told of where it stands.
    fn waiting_msg(&self, s: u32, rank: u32, now: i64) -> Option<ToOwner> {
        let p = self.get(s)?;
        let k = p.key?;
        Some(ToOwner::Waiting {
            run: p.run.clone(),
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
            let mut work = ahead.work;
            for (end, c) in &self.ends {
                let (Some(claim), Ok(left)) = (self.get(*c), u64::try_from(end.saturating_sub(now))) else { continue };
                for (w, a) in work.iter_mut().zip(&claim.amounts) {
                    *w = w.saturating_add(u128::from(*a).saturating_mul(u128::from(left)));
                }
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
    fn standing(&mut self, s: u32, now: i64, out: &mut Vec<Said>) {
        let Some(p) = self.get(s) else { return };
        let Some(to) = self.owner(p.owner).filter(|w| !w.away).map(|w| w.name.clone()) else { return };
        let (state, predicted, run) = (p.state, p.predicted, p.run.clone());
        match state {
            State::Offered { .. } => self.say_offer(s, None, out),
            State::Taken { fence } => out.push((to, ToOwner::Held { run, fence })),
            State::Running { .. } => out.push((to, ToOwner::Ran { run })),
            State::Aside => out.push((to, ToOwner::Waiting { run, reason: Reason::Away, place: Place::default(), start: Estimate::default(), predicted, free: ask::resources(&self.free_now()) })),
            State::Waiting => {
                if let Some(p) = self.get_mut(s) {
                    p.told = None;
                }
                self.tell_if_changed(s, now, out);
            }
        }
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
    fn recorded(&self, from: Peer<'_>, run: &str, out: &mut Vec<Said>) -> bool {
        let Some(r) = self.record(from.name, from.epoch, run) else { return false };
        let run = run.to_owned();
        out.push((from.name.to_owned(), match r.ended {
            None => ToOwner::Ran { run },
            Some(ok) => ToOwner::Ended { run, fence: r.fence, ok },
        }));
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
        let (old_end, old_hard) = (p.end, p.hard);
        let end = p.predicted.map(|ms| later(from, ms));
        let hard = p.ask.timeout.map(|ms| later(from, ms));
        match old_end {
            Some(at) => {
                self.ends.remove(&(at, s));
            }
            None => self.unpredicted_claims = self.unpredicted_claims.saturating_sub(1),
        }
        match old_hard {
            Some(at) => {
                self.hard_ends.remove(&(at, s));
            }
            None => self.untimed_claims = self.untimed_claims.saturating_sub(1),
        }
        match end {
            Some(at) => {
                self.ends.insert((at, s));
            }
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
    pub fn probe(&mut self, from: Peer<'_>, ask: Ask, predicted: Option<Ms>, now: i64, out: &mut Vec<Said>) {
        let Some(o) = self.speaker(from, now, out) else { return };
        if self.recorded(from, &ask.run, out) {
            return;
        }
        if let Some(s) = self.owner(o).and_then(|w| w.runs.get(&ask.run)).copied() {
            self.standing(s, now, out);
            return;
        }
        if let Some(why) = self.never(&ask) {
            out.push((from.name.to_owned(), ToOwner::Never { run: ask.run, why }));
            return;
        }
        let amounts = ask::amounts(&ask.needs.request);
        let tenant = format!("{}\0{}", ask.repo, ask.actor);
        let run = ask.run.clone();
        let p = Probe { owner: o, run: run.clone(), ask, amounts, predicted, state: State::Aside, key: None, tenant, charged: 0, end: None, hard: None, told: None };
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
        if away {
            return;
        }
        self.enter(s);
        self.admit(now, out);
        self.boundaries(now, out);
        if self.get(s).is_some_and(|p| p.state == State::Waiting && p.told.is_none()) {
            self.tell_if_changed(s, now, out);
        }
    }

    /// The owner withdraws a run (placed elsewhere, ended, not wanted): its place or claim goes,
    /// its room free in the same input. A running run isn't stopped by it: that's its own signal's.
    pub fn withdraw(&mut self, from: Peer<'_>, run: &str, now: i64, out: &mut Vec<Said>) {
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
    pub fn take(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) {
        self.take_in(from, run, fence, now, out);
        // Offers that lapsed by now went back to their places: what they free is offered.
        self.admit(now, out);
        self.boundaries(now, out);
    }

    fn take_in(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) {
        self.lapse_due(now, out);
        let Some(o) = self.speaker(from, now, out) else { return };
        if self.recorded(from, run, out) {
            return;
        }
        let s = self.owner(o).and_then(|w| w.runs.get(run)).copied();
        let to = from.name.to_owned();
        match s.and_then(|s| self.state(s).map(|st| (s, st))) {
            Some((s, State::Offered { fence: f, until })) if f == fence => {
                self.deadlines.remove(&(until, s));
                if let Some(p) = self.get_mut(s) {
                    p.state = State::Taken { fence };
                }
                out.push((to, ToOwner::Held { run: run.to_owned(), fence }));
            }
            Some((_, State::Taken { fence: f })) if f == fence => out.push((to, ToOwner::Held { run: run.to_owned(), fence })),
            Some((_, State::Running { .. })) => out.push((to, ToOwner::Ran { run: run.to_owned() })),
            _ => out.push((to, ToOwner::Lapsed { run: run.to_owned(), fence, why: "its offer isn't held for it any more".into() })),
        }
    }

    /// The owner declines offer `fence` (it took another's): its room is free in the same input,
    /// and the run leaves the line.
    pub fn decline(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) {
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
    pub fn start(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) -> Option<Start> {
        let started = self.start_in(from, run, fence, now, out);
        // Offers that lapsed by now went back to their places: what they free is offered.
        self.admit(now, out);
        self.boundaries(now, out);
        started
    }

    fn start_in(&mut self, from: Peer<'_>, run: &str, fence: Fence, now: i64, out: &mut Vec<Said>) -> Option<Start> {
        self.lapse_due(now, out);
        let o = self.speaker(from, now, out)?;
        if self.recorded(from, run, out) {
            return Some(Start::Already);
        }
        let s = self.owner(o).and_then(|w| w.runs.get(run)).copied();
        let state = s.and_then(|s| self.state(s));
        if let (Some(_), Some(State::Running { .. })) = (s, state) {
            out.push((from.name.to_owned(), ToOwner::Ran { run: run.to_owned() }));
            return Some(Start::Already);
        }
        let live = matches!(state, Some(State::Offered { fence: f, until }) if f == fence && until > now) || matches!(state, Some(State::Taken { fence: f }) if f == fence);
        #[cfg(test)]
        let live = live || (s.is_some() && super::mutant() == Some(super::Mutant::StartLapsed));
        let Some(s) = s.filter(|_| live) else {
            out.push((from.name.to_owned(), ToOwner::Lapsed { run: run.to_owned(), fence, why: "its claim isn't live here".into() }));
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
        self.records.entry(from.name.to_owned()).or_default().entry(from.epoch).or_default().insert(run.to_owned(), Record { fence, ended: None });
        Some(Start::New)
    }

    /// Its shell started the run's process: its owner is told it started (the runner's word).
    pub fn spawned(&mut self, from: Peer<'_>, run: &str, out: &mut Vec<Said>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(State::Running { fence, .. }) = self.state(s) else { return };
        let to = self.get(s).and_then(|p| self.owner(p.owner)).filter(|w| !w.away).map(|w| w.name.clone());
        if let Some(to) = to {
            out.push((to, ToOwner::Started { run: run.to_owned(), fence }));
        }
    }

    /// A run it started ended (`ok`: its process exited; not: it couldn't start, or was lost): its
    /// room is free in the same input, its tenant charged what it ran, and its owner told.
    pub fn ended(&mut self, from: Peer<'_>, run: &str, ok: bool, now: i64, out: &mut Vec<Said>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(State::Running { fence, from: t0 }) = self.state(s) else { return };
        self.settle(s, u64::try_from(now.saturating_sub(t0)).unwrap_or(0));
        if let Some(r) = self.record_mut(from.name, from.epoch, run) {
            r.ended = Some(ok);
        }
        let away = self.get(s).and_then(|p| self.owner(p.owner)).is_none_or(|w| w.away);
        self.drop_probe(s);
        if !away {
            out.push((from.name.to_owned(), ToOwner::Ended { run: run.to_owned(), fence, ok }));
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
    pub fn measured(&mut self, sample: Sample, now: i64, out: &mut Vec<Said>) {
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
        self.avail = self.room_now();
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// Draining (an upgrade pending) or not: draining, it offers nothing new.
    pub fn drain(&mut self, draining: bool, now: i64, out: &mut Vec<Said>) {
        self.draining = draining;
        self.admit(now, out);
        self.tell_all(now, out);
    }

    /// An owner's link went down: its probes leave the line, offered nothing, an offer it hadn't
    /// taken lapses at once; a claim it took stays held, as does what runs: only its absence
    /// ([`Self::forget`]) or its epoch changing ends them.
    pub fn away(&mut self, name: &str, now: i64, out: &mut Vec<Said>) {
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
                }
                _ => {}
            }
        }
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// An owner's link is back: its probes are in the line again at the places they had, and it's
    /// told where each stands, and how each run that ended meanwhile ended.
    pub fn back(&mut self, from: Peer<'_>, now: i64, out: &mut Vec<Said>) {
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
            out.push((from.name.to_owned(), ToOwner::Ended { run, fence: r.fence, ok: r.ended == Some(true) }));
        }
    }

    /// An owner is gone (its lease ended, or the directory no longer has its node): what it left
    /// waiting or claimed goes, its room free; what runs runs on to its end. Its record stays, so
    /// that if it comes back in the same epoch nothing of its starts twice.
    pub fn forget(&mut self, name: &str, now: i64, out: &mut Vec<Said>) {
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
    pub fn predict(&mut self, from: Peer<'_>, run: &str, predicted: Option<Ms>, now: i64, out: &mut Vec<Said>) {
        let Some(s) = self.find(from, run) else { return };
        let Some(p) = self.get_mut(s) else { return };
        p.predicted = predicted;
        let (state, key, item) = (p.state, p.key, Item { slot: s, ask: p.amounts, predicted, timeout: p.ask.timeout });
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

    fn lapse_due(&mut self, now: i64, out: &mut Vec<Said>) {
        while let Some(&(until, s)) = self.deadlines.first() {
            if until > now {
                break;
            }
            self.deadlines.remove(&(until, s));
            let Some(State::Offered { fence, .. }) = self.state(s) else { continue };
            self.release(s);
            let Some(p) = self.get(s) else { continue };
            let (run, to) = (p.run.clone(), self.owner(p.owner).filter(|w| !w.away).map(|w| w.name.clone()));
            self.enter(s);
            if let Some(to) = to {
                out.push((to, ToOwner::Lapsed { run, fence, why: "its offer wasn't taken in time".into() }));
            }
            self.boundaries(now, out);
            self.tell_if_changed(s, now, out);
        }
    }

    /// The time: offers not taken within their link's timeout lapse (each run waits again at its
    /// place, its owner told), and what that frees is offered.
    pub fn tick(&mut self, now: i64, out: &mut Vec<Said>) {
        self.lapse_due(now, out);
        self.admit(now, out);
        self.boundaries(now, out);
    }

    /// When it's to be told the time again ([`Self::tick`]): the soonest offer to lapse.
    pub fn timer(&self) -> Option<i64> {
        self.deadlines.first().map(|(at, _)| *at)
    }

    /// The record of runs started here, as the shell keeps it on disk.
    pub fn records(&self) -> &Records {
        &self.records
    }

    /// The room it admits against now, and what its claims hold of it.
    pub fn room(&self) -> (Amounts, Amounts) {
        (self.avail, self.held)
    }

    /// How many wait in its line.
    pub fn queued(&self) -> u32 {
        self.line.len()
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
            out.push(View { owner, epoch, run: p.run.clone(), state, fence, ask: p.amounts });
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
    /// the line holds exactly the runs waiting. O(n): for tests and the model.
    pub fn check(&self) -> Result<(), String> {
        let mut held = [0u64; ask::DIMS];
        let (mut claims, mut whole, mut waiting) = (0u32, 0u32, 0u32);
        for p in self.slots.iter().flatten() {
            match p.state {
                State::Offered { .. } | State::Taken { .. } | State::Running { .. } => {
                    held = ask::plus(&held, &p.amounts);
                    claims = claims.saturating_add(1);
                    whole = whole.saturating_add(u32::from(p.ask.needs.exclusive));
                }
                State::Waiting => waiting = waiting.saturating_add(1),
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
            && let Some((_, h)) = self.line.first()
            && self.can_start(h, &self.free_now())
        {
            return Err(format!("{}: the first in line could start, and wasn't offered", self.me));
        }
        Ok(())
    }
}
