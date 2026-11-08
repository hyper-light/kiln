//! Tenant budgets across owners with no center (docs/needs.md §9.1; docs/ci.md W05-R11; LB08). A
//! person's budget for a tenant is a number of integer rights a period, each dimension a resource
//! × time ([`Rights`]: milli-CPU·ms, MiB·ms of memory or disk, GPU·ms), and the rights are held by
//! the owners that place the tenant's runs. The research of 2026-11-08 (federation research §4,
//! synthesis N4) rests it on four results:
//!
//! - **Escrow** (O'Neil, "The Escrow Transactional Method", ACM TODS 11(4):405–430, 1986): a run
//!   reserves its worst case, what it asks × its timeout, against the rights its owner holds before
//!   it's placed, and settles to what it used when it ends. The bound holds however the runs
//!   outstanding end.
//! - **The bounded counter** (Balegas, Serra, Duarte, Shapiro, Najafzadeh, Ferreira, Rodrigues,
//!   Preguiça, "Extending Eventually Consistent Cloud Databases for Enforcing Numeric Invariants",
//!   SRDS 2015): rights are a CRDT. An owner spends only what it holds, and rights move pairwise,
//!   each transfer a grow-only total its receiver merges by maximum, so a word lost, said twice or
//!   out of order changes nothing, and the bound holds under any partition.
//! - **Demarcation** (Barbará, Garcia-Molina, "The Demarcation Protocol", VLDB Journal 3(3), 1994):
//!   the giver lowers its own holding, and keeps that, before it sends.
//! - **Why nothing weaker** (Bailis, Fekete, Franklin, Ghodsi, Hellerstein, Stoica, "Coordination
//!   Avoidance in Database Systems", PVLDB 8(3), 2014, claims 12–14): spend under a cap isn't
//!   invariant-confluent for increments, so zero overspend needs coordination. Escrow puts it ahead
//!   of the spend: once a period, the period's rights are split among the owners ([`split`]), where
//!   the person's declaration is kept, and kept there before anyone sees it. No run's placement
//!   waits on it.
//!
//! An owner short of rights for the run first in its line asks one peer at a time ([`Word::Need`],
//! the peer believed to hold the most first), and each peer answers with what it gave in all
//! ([`Word::Give`]). A peer that can't give it all keeps the need, and gives as its rights come
//! free, its own older runs first: needs are ordered by when their runs were held (then by owner),
//! so the oldest run held anywhere gets rights from every peer it asks, and no two owners wait on
//! each other. How much it asks for: its shortfall, plus what it spent a millisecond this period
//! over the round trip it measured to that peer (a lot from measured rate × delay, not a constant).
//! When its link to a peer comes back it says its need again with what it got, and the peer
//! answers with what it gave: a transfer lost with a link arrives then. Nothing rebalances on a
//! timer; a period's end is a due time ([`Budgets::timer`]).
//!
//! Pure, as the rest of kiln-ci: time comes in, words and releases go out, and what's kept
//! ([`Budgets::changes`]) is committed by the shell before anything that came with it is said.
//! Every amount is an integer; every number is a person's setting or a measurement.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::answer::{Answer, Count};
use super::ask::{DIMS, amounts};
use super::msg::Estimate;
use crate::sched::state::Resources;
use crate::sched::units::Ms;

/// Rights in each dimension, in [`DIMS`]' order: milli-CPU·ms, MiB·ms of memory, MiB·ms of disk,
/// GPU·ms.
pub type Rights = [u64; DIMS];

/// No rights.
pub const NONE: Rights = [0; DIMS];

/// Memory and disk are counted in MiB (a byte·ms right would overflow a u64 at a few TiB·hours).
pub const MIB: u64 = 1 << 20;

/// A bug planted in budgets for their model and tests to catch (docs/sim.md): in tests only.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// The giver sends a transfer but keeps the rights it sent (demarcation's order broken).
    GiveBeforeLower,
    /// A run's end settles each time it's told: a repeated end refunds its reservation again.
    SettleTwice,
    /// An owner short for a run of its own gives nothing, whoever asks.
    HoardWhenShort,
}

#[cfg(test)]
thread_local! {
    static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn mutant() -> Option<Mutant> {
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

fn each(a: &Rights, b: &Rights, f: impl Fn(u64, u64) -> u64) -> Rights {
    let mut out = NONE;
    for (o, (x, y)) in out.iter_mut().zip(a.iter().zip(b.iter())) {
        *o = f(*x, *y);
    }
    out
}

pub fn plus(a: &Rights, b: &Rights) -> Rights {
    each(a, b, u64::saturating_add)
}

pub fn less(a: &Rights, b: &Rights) -> Rights {
    each(a, b, u64::saturating_sub)
}

pub fn least(a: &Rights, b: &Rights) -> Rights {
    each(a, b, u64::min)
}

pub fn most(a: &Rights, b: &Rights) -> Rights {
    each(a, b, u64::max)
}

/// `a` is at least `b` in every dimension.
pub fn covers(a: &Rights, b: &Rights) -> bool {
    a.iter().zip(b.iter()).all(|(x, y)| x >= y)
}

pub fn nothing(a: &Rights) -> bool {
    a.iter().all(|x| *x == 0)
}

fn times(a: &Rights, ms: u64) -> Rights {
    a.map(|x| x.saturating_mul(ms))
}

fn sum<'a>(it: impl Iterator<Item = &'a Rights>) -> Rights {
    it.fold(NONE, |acc, r| plus(&acc, r))
}

/// What a run holds of each dimension a millisecond while it runs, as rights count it: its
/// milli-CPUs, its memory and disk in MiB (rounded up) and its GPUs.
pub fn rate(r: &Resources) -> Rights {
    let [cpu, mem, disk, gpu] = amounts(r);
    [cpu, mem.div_ceil(MIB), disk.div_ceil(MIB), gpu]
}

/// A person's budget for a tenant (`[ci.budgets."<tenant>"]`, W05-R11): rights a period in the
/// dimensions it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decl {
    /// A repository (`acme/app`), or an organization (`acme`: each of its repositories).
    pub tenant: String,
    /// The period, in ms: periods run from the Unix epoch, `per` each (a day's from 00:00 UTC).
    pub per: Ms,
    /// A period's rights in each dimension; `None`: that dimension isn't budgeted.
    pub limit: [Option<u64>; DIMS],
    /// A run of the tenant must say its timeout (the default): without one it has no worst case to
    /// reserve. Off, a run without one reserves what it asks until the period ends, and what it
    /// uses past that is taken from what's free where it ends, or counted as overspent.
    #[serde(default = "yes")]
    pub require_timeout: bool,
}

fn yes() -> bool {
    true
}

impl Decl {
    /// Whether it budgets runs of repository `repo`.
    pub fn covers(&self, repo: &str) -> bool {
        !self.tenant.is_empty() && (repo == self.tenant || (!self.tenant.contains('/') && repo.strip_prefix(&self.tenant).is_some_and(|r| r.starts_with('/'))))
    }

    /// The period `now` is in.
    pub fn period(&self, now: i64) -> u64 {
        u64::try_from(now).unwrap_or(0).checked_div(self.per).unwrap_or(0)
    }

    /// When period `p` starts.
    pub fn start(&self, p: u64) -> i64 {
        i64::try_from(p.saturating_mul(self.per)).unwrap_or(i64::MAX)
    }

    /// When period `p` ends: the next one's start.
    pub fn end(&self, p: u64) -> i64 {
        self.start(p.saturating_add(1))
    }

    pub fn key(&self, p: u64) -> PotKey {
        PotKey { tenant: self.tenant.clone(), per: self.per, period: p }
    }

    /// A period's rights, nothing where a dimension isn't budgeted.
    pub fn limits(&self) -> Rights {
        self.limit.map(|l| l.unwrap_or(0))
    }

    fn mask(&self, r: &Rights) -> Rights {
        let mut out = NONE;
        for (o, (x, l)) in out.iter_mut().zip(r.iter().zip(self.limit.iter())) {
            if l.is_some() {
                *o = *x;
            }
        }
        out
    }
}

/// One budget's rights in one period: its tenant, its period's length (a changed length is
/// another budget) and its period.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PotKey {
    pub tenant: String,
    pub per: Ms,
    pub period: u64,
}

/// Who holds a period's rights as it starts, decided once, where the person's declaration is kept,
/// and never changed after (`split`): what each owner may mint, once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Split {
    pub key: PotKey,
    pub shares: BTreeMap<String, Rights>,
}

/// A period's rights split evenly among `owners` (the runners that speak budgets as it's decided),
/// what doesn't divide one unit each to the first by name. `None` with no owner.
pub fn split(decl: &Decl, period: u64, owners: &[String]) -> Option<Split> {
    let names: BTreeSet<&String> = owners.iter().collect();
    let n = u64::try_from(names.len()).ok().filter(|n| *n > 0)?;
    let limits = decl.limits();
    let mut shares = BTreeMap::new();
    for (k, name) in (0u64..).zip(names) {
        let share = limits.map(|l| l.checked_div(n).unwrap_or(0).saturating_add(u64::from(k < l.checked_rem(n).unwrap_or(0))));
        shares.insert(name.clone(), share);
    }
    Some(Split { key: decl.key(period), shares })
}

/// What owners say to each other about a budget (JSON, in placement's envelope between nodes that
/// name `place.budget`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "word", rename_all = "snake_case")]
pub enum Word {
    /// The asker is short in `pot`: beyond the `got` the peer has given it in all, it needs `need`
    /// more, for a run held since `since` (the oldest first, then by owner name). Its latest by
    /// (`epoch`, `seq`) counts; a need of nothing takes it back.
    Need { pot: PotKey, epoch: u64, seq: u64, need: Rights, got: Rights, since: i64 },
    /// What the giver has given the asker in `pot` in all (lowered from its own holding, and kept,
    /// first), and what it has free after.
    Give { pot: PotKey, total: Rights, free: Rights },
}

/// A pot's hard state at an owner: kept with the owner's runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    /// Its share of the period's split, once minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minted: Option<Rights>,
    /// What each peer has given it in all, as it last heard.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub got: BTreeMap<String, Rights>,
    /// What it has given each peer in all.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gave: BTreeMap<String, Rights>,
    /// What runs that ended here were charged.
    #[serde(default)]
    pub spent: Rights,
    /// What runs without a timeout used past what they reserved and the rights free here.
    #[serde(default)]
    pub over: Rights,
}

/// One budget's part of a run's reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub pot: PotKey,
    pub worst: Rights,
    /// What it holds a millisecond, in the dimensions the budget names.
    pub rate: Rights,
}

/// A run admitted against its budgets: kept until it's settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reserved {
    pub holds: Vec<Hold>,
    /// Its start may have been said: only its end settles it now.
    #[serde(default)]
    pub go: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Ms>,
}

/// A run held back for budget: kept until it's released, never, or cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub repo: String,
    pub rate: Rights,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Ms>,
    pub since: i64,
    /// Its arrival among this owner's.
    pub n: u64,
}

/// A row of budgets' hard state, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Pot(PotKey, Kept),
    Run(String, Reserved),
    Held(String, Waiting),
}

/// A change to keep: the row as it is now, or `None`, gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Pot(PotKey, Option<Kept>),
    Run(String, Option<Reserved>),
    Held(String, Option<Waiting>),
}

/// How a run ended, for its settling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// It ran: charged what it holds × the time from its start (its timeout at most).
    Ran,
    /// It never started: charged nothing.
    NotStarted,
    /// Whether or how long it ran isn't known (lost): charged its worst case.
    Unknown,
}

/// A run asked of an owner, at the budgets' door.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    /// No budget covers it: place it.
    Free,
    /// Its worst case is reserved: place it.
    Reserved,
    /// Held back for budget: it's told why ([`Do::Tell`]), and released when rights come.
    Held,
    /// It can never be reserved as asked.
    Never(Answer),
}

/// What the shell is to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Do {
    /// Say `word` to owner `to`: a [`Word::Need`] on this node's link to it, a [`Word::Give`] on the
    /// asker's.
    Say { to: String, word: Word },
    /// A held run is reserved now (or no budget covers it any more): place it.
    Release { run: String },
    /// Tell a held run's asker: why it's held (news as that changes), or never.
    Tell { run: String, answer: Answer },
}

/// What an owner holds of a pot, for a view and the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Holding {
    pub minted: Option<Rights>,
    pub got: Rights,
    pub gave: Rights,
    pub spent: Rights,
    pub reserved: Rights,
    pub over: Rights,
    pub free: Rights,
}

/// The latest need a peer said.
#[derive(Debug, Clone, Copy)]
struct NeedIn {
    epoch: u64,
    seq: u64,
    need: Rights,
    got: Rights,
    since: i64,
}

/// An asker's asking for its first held run: whose answer it waits for (and since when), and what
/// it last told each peer it needs, with what it had got from that peer then.
#[derive(Debug, Clone, Default)]
struct Round {
    head: Option<String>,
    at: Option<(String, i64)>,
    told: BTreeMap<String, (Rights, Rights)>,
}

impl Round {
    /// What `peer` still owes on the need it was last told, as far as this owner has heard: that
    /// need, less what it gave since. The giver works out the same from its side.
    fn owed(&self, peer: &str, got: &Rights) -> Rights {
        self.told.get(peer).map_or(NONE, |(need, then)| less(need, &less(got, then)))
    }
}

#[derive(Debug, Clone, Default)]
struct Pot {
    kept: Kept,
    /// What its open reservations hold (from the runs, not kept apart).
    reserved: Rights,
    needs: BTreeMap<String, NeedIn>,
    /// What each peer last said it had free (its share of the split, until it says).
    belief: BTreeMap<String, Rights>,
    round: Round,
    rtt: BTreeMap<String, Ms>,
}

impl Pot {
    fn inflow(&self) -> Rights {
        plus(&self.kept.minted.unwrap_or(NONE), &sum(self.kept.got.values()))
    }

    fn outflow(&self) -> Rights {
        plus(&plus(&sum(self.kept.gave.values()), &self.kept.spent), &self.reserved)
    }

    fn free(&self) -> Rights {
        less(&self.inflow(), &self.outflow())
    }

    fn gave(&self, peer: &str) -> Rights {
        self.kept.gave.get(peer).copied().unwrap_or(NONE)
    }

    fn got(&self, peer: &str) -> Rights {
        self.kept.got.get(peer).copied().unwrap_or(NONE)
    }

    /// What's still owed against `n`, the latest need `peer` said: its need, less what was given it
    /// since that need was said.
    fn owed(&self, peer: &str, n: &NeedIn) -> Rights {
        less(&n.need, &less(&self.gave(peer), &n.got))
    }
}

/// An owner's budgets: what it holds of each pot, its reservations and the runs it holds back.
#[derive(Debug, Clone, Default)]
pub struct Budgets {
    me: String,
    epoch: u64,
    seq: u64,
    n: u64,
    decls: BTreeMap<String, Decl>,
    splits: BTreeMap<PotKey, BTreeMap<String, Rights>>,
    peers: BTreeSet<String>,
    away: BTreeSet<String>,
    pots: BTreeMap<PotKey, Pot>,
    runs: BTreeMap<String, Reserved>,
    held: BTreeMap<(i64, u64), String>,
    waiting: BTreeMap<String, Waiting>,
    told: BTreeMap<String, String>,
    due: Option<i64>,
    dirty_pots: BTreeSet<PotKey>,
    dirty_runs: BTreeSet<String>,
    dirty_held: BTreeSet<String>,
}

impl Budgets {
    /// Owner `me` in its `epoch` (one past its last: its needs said now outrank those before).
    pub fn new(me: &str, epoch: u64) -> Budgets {
        Budgets { me: me.to_owned(), epoch, ..Budgets::default() }
    }

    /// Owner `me` in `epoch`, from what it kept.
    pub fn restore(me: &str, epoch: u64, rows: Vec<Row>) -> Budgets {
        let mut b = Budgets::new(me, epoch);
        for row in rows {
            match row {
                Row::Pot(k, kept) => b.pots.entry(k).or_default().kept = kept,
                Row::Run(run, r) => {
                    for h in &r.holds {
                        let p = b.pots.entry(h.pot.clone()).or_default();
                        p.reserved = plus(&p.reserved, &h.worst);
                    }
                    b.runs.insert(run, r);
                }
                Row::Held(run, w) => {
                    b.n = b.n.max(w.n);
                    b.held.insert((w.since, w.n), run.clone());
                    b.waiting.insert(run, w);
                }
            }
        }
        b
    }

    /// Whether any budget is declared.
    pub fn any(&self) -> bool {
        !self.decls.is_empty()
    }

    /// The person's declarations, the splits decided for them so far, and the owners that speak
    /// budgets. A split, once had, is kept as it first came.
    pub fn declare(&mut self, decls: Vec<Decl>, splits: Vec<Split>, peers: Vec<String>, now: i64) -> Vec<Do> {
        self.decls = decls.into_iter().filter(|d| d.per > 0 && !d.tenant.is_empty()).map(|d| (d.tenant.clone(), d)).collect();
        for s in splits {
            self.splits.entry(s.key).or_insert(s.shares);
        }
        let live: BTreeSet<PotKey> = self.decls.values().map(|d| d.key(d.period(now))).collect();
        self.splits.retain(|k, _| live.iter().any(|l| l.tenant == k.tenant && l.per == k.per && k.period.saturating_add(1) >= l.period));
        self.peers = peers.into_iter().filter(|p| *p != self.me).collect();
        // A peer gone from the directory won't answer: its round asks on.
        for pot in self.pots.values_mut() {
            if pot.round.at.as_ref().is_some_and(|(p, _)| !self.peers.contains(p)) {
                pot.round.at = None;
            }
        }
        let keys: Vec<PotKey> = self.pots.keys().cloned().collect();
        for k in keys {
            self.mint(&k);
        }
        self.changed(now)
    }

    /// Run `run` of repository `repo`, asking `request`, its timeout `timeout`: placed at once
    /// unless a budget covers it, then only once its worst case is reserved in each.
    pub fn ask(&mut self, run: &str, repo: &str, request: &Resources, timeout: Option<Ms>, now: i64) -> (Gate, Vec<Do>) {
        if self.runs.contains_key(run) {
            return (Gate::Reserved, Vec::new());
        }
        if self.waiting.contains_key(run) {
            return (Gate::Held, Vec::new());
        }
        let rate = rate(request);
        let holds = match self.holds(repo, &rate, timeout, now) {
            Ok(h) => h,
            Err(need) => return (Gate::Never(Answer::Never { need, nearest: None }), Vec::new()),
        };
        if holds.is_empty() {
            return (Gate::Free, Vec::new());
        }
        self.n = self.n.saturating_add(1);
        let w = Waiting { repo: repo.to_owned(), rate, timeout, since: now, n: self.n };
        let behind = self.waiting.values().any(|o| self.decls.values().any(|d| d.covers(&o.repo) && d.covers(repo)));
        if !behind && self.fits(&holds, (now, &self.me.clone())) {
            self.reserve(run, holds, timeout);
            return (Gate::Reserved, self.changed(now));
        }
        self.held.insert((now, w.n), run.to_owned());
        self.waiting.insert(run.to_owned(), w);
        self.dirty_held.insert(run.to_owned());
        (Gate::Held, self.changed(now))
    }

    /// The owner said run `run`'s start: once it may have started, only its end settles it.
    pub fn go(&mut self, run: &str) {
        if let Some(r) = self.runs.get_mut(run)
            && !r.go
        {
            r.go = true;
            self.dirty_runs.insert(run.to_owned());
        }
    }

    /// Run `run` started, by its runner's word.
    pub fn started(&mut self, run: &str, now: i64) {
        if let Some(r) = self.runs.get_mut(run)
            && r.started.is_none()
        {
            r.started = Some(now);
            r.go = true;
            self.dirty_runs.insert(run.to_owned());
        }
    }

    /// Run `run` ended: settled once, to what it used (its worst case at most, unless it has no
    /// timeout), the rest of its reservation free again.
    pub fn ended(&mut self, run: &str, end: End, now: i64) -> Vec<Do> {
        #[cfg(test)]
        let r = if mutant() == Some(Mutant::SettleTwice) { self.runs.get(run).cloned() } else { self.runs.remove(run) };
        #[cfg(not(test))]
        let r = self.runs.remove(run);
        let Some(r) = r else { return Vec::new() };
        for h in &r.holds {
            let used = match (end, r.started) {
                (End::NotStarted, _) => NONE,
                (End::Ran, Some(s)) => {
                    let ran = u64::try_from(now.saturating_sub(s)).unwrap_or(0);
                    times(&h.rate, r.timeout.map_or(ran, |t| ran.min(t)))
                }
                _ => h.worst,
            };
            let past = less(&used, &h.worst);
            let pot = self.pots.entry(h.pot.clone()).or_default();
            pot.reserved = less(&pot.reserved, &h.worst);
            pot.kept.spent = plus(&pot.kept.spent, &least(&used, &h.worst));
            // Past its reservation (only a run without a timeout): from what's free here, then over.
            let from_free = least(&past, &pot.free());
            pot.kept.spent = plus(&pot.kept.spent, &from_free);
            pot.kept.over = plus(&pot.kept.over, &less(&past, &from_free));
            self.dirty_pots.insert(h.pot.clone());
        }
        self.dirty_runs.insert(run.to_owned());
        self.changed(now)
    }

    /// Its asker doesn't want run `run` any more: held, it's let go; reserved and never said to
    /// start, its reservation is free again.
    pub fn cancel(&mut self, run: &str, now: i64) -> Vec<Do> {
        if let Some(w) = self.waiting.remove(run) {
            self.held.remove(&(w.since, w.n));
            self.told.remove(run);
            self.dirty_held.insert(run.to_owned());
            return self.changed(now);
        }
        match self.runs.get(run) {
            Some(r) if !r.go => self.ended(run, End::NotStarted, now),
            _ => Vec::new(),
        }
    }

    /// A word from owner `from`.
    pub fn heard(&mut self, from: &str, word: Word, now: i64) -> Vec<Do> {
        if from == self.me {
            return Vec::new();
        }
        let mut out = Vec::new();
        match word {
            Word::Need { pot: key, epoch, seq, need, got, since } => {
                if !self.live(&key, now) {
                    // A pot it has no declaration for, or one whose period is over: nothing to give.
                    let total = self.pots.get(&key).map(|p| p.gave(from)).unwrap_or(NONE);
                    if !nothing(&need) || !covers(&got, &total) {
                        out.push(Do::Say { to: from.to_owned(), word: Word::Give { pot: key, total, free: NONE } });
                    }
                    return out;
                }
                let pot = self.pot(&key);
                if pot.needs.get(from).is_none_or(|n| (epoch, seq) > (n.epoch, n.seq)) {
                    pot.needs.insert(from.to_owned(), NeedIn { epoch, seq, need, got, since });
                }
                out.extend(self.changed(now));
                let answered = out.iter().any(|d| matches!(d, Do::Say { to, word: Word::Give { pot, .. } } if to == from && *pot == key));
                if !answered {
                    let pot = self.pot(&key);
                    let total = pot.gave(from);
                    if !nothing(&need) || !covers(&got, &total) {
                        out.push(Do::Say { to: from.to_owned(), word: Word::Give { pot: key, total, free: pot.free() } });
                    }
                }
            }
            Word::Give { pot: key, total, free } => {
                if !self.live(&key, now) && !self.pots.contains_key(&key) {
                    return out;
                }
                let pot = self.pot(&key);
                let had = pot.got(from);
                let merged = most(&had, &total);
                pot.belief.insert(from.to_owned(), free);
                // The answer it waited for: the round trip to that peer, measured.
                if pot.round.at.as_ref().is_some_and(|(p, _)| p == from)
                    && let Some((_, at)) = pot.round.at.take()
                {
                    pot.rtt.insert(from.to_owned(), u64::try_from(now.saturating_sub(at)).unwrap_or(0));
                }
                if merged != had {
                    pot.kept.got.insert(from.to_owned(), merged);
                    self.dirty_pots.insert(key);
                }
                out.extend(self.changed(now));
            }
        }
        out
    }

    /// This node's link to owner `peer` went down, or came back. Down, it isn't asked till it's
    /// back; back, each pot of a period under way says its need again with what it got, so a
    /// transfer lost with the link comes now.
    pub fn link(&mut self, peer: &str, up: bool, now: i64) -> Vec<Do> {
        if peer == self.me {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !up {
            self.away.insert(peer.to_owned());
            for pot in self.pots.values_mut() {
                if pot.round.at.as_ref().is_some_and(|(p, _)| p == peer) {
                    pot.round.at = None;
                }
            }
            return self.changed(now);
        }
        self.away.remove(peer);
        let keys: Vec<PotKey> = self.pots.keys().filter(|k| self.live(k, now)).cloned().collect();
        for key in keys {
            let head = self.head(&key, now);
            let (short, since) = head.as_ref().map_or((NONE, 0), |h| (h.short, h.since));
            self.seq = self.seq.saturating_add(1);
            let (epoch, seq) = (self.epoch, self.seq);
            let pot = self.pot(&key);
            // A need it told the peer is said again as it is now; to any other, a need of nothing
            // with what it got, which the peer answers only if it gave more.
            let got = pot.got(peer);
            let told = pot.round.told.get(peer).is_some_and(|(t, _)| !nothing(t));
            let need = if told { short } else { NONE };
            if told {
                pot.round.told.insert(peer.to_owned(), (need, got));
            }
            out.push(Do::Say { to: peer.to_owned(), word: Word::Need { pot: key, epoch, seq, need, got, since } });
        }
        out.extend(self.changed(now));
        out
    }

    /// The time: a period that ended takes the runs held for it to the next, whose rights each
    /// owner mints, and pots over with nothing reserved go.
    pub fn tick(&mut self, now: i64) -> Vec<Do> {
        let gone: Vec<PotKey> = self.pots.iter().filter(|(k, p)| !self.live(k, now) && nothing(&p.reserved)).map(|(k, _)| k.clone()).collect();
        for k in gone {
            self.pots.remove(&k);
            self.dirty_pots.insert(k);
        }
        self.changed(now)
    }

    /// When to be told the time: the end of a period a held run waits in.
    pub fn timer(&self) -> Option<i64> {
        self.due
    }

    /// What changed since this was last asked, to keep before anything that came with it is said.
    pub fn changes(&mut self) -> Vec<Change> {
        let mut out = Vec::new();
        for k in std::mem::take(&mut self.dirty_pots) {
            let kept = self.pots.get(&k).map(|p| p.kept.clone());
            out.push(Change::Pot(k, kept));
        }
        for r in std::mem::take(&mut self.dirty_runs) {
            let row = self.runs.get(&r).cloned();
            out.push(Change::Run(r, row));
        }
        for r in std::mem::take(&mut self.dirty_held) {
            let row = self.waiting.get(&r).cloned();
            out.push(Change::Held(r, row));
        }
        out
    }

    /// Whether run `run` is held back for budget.
    pub fn holds_back(&self, run: &str) -> bool {
        self.waiting.contains_key(run)
    }

    /// The runs held back, oldest first, with when each was held.
    pub fn held(&self) -> Vec<(String, i64)> {
        self.held.iter().map(|((since, _), r)| (r.clone(), *since)).collect()
    }

    /// Run `run`'s reservation, if it has one.
    pub fn reservation(&self, run: &str) -> Option<&Reserved> {
        self.runs.get(run)
    }

    /// Every run reserved, by id.
    pub fn reservations(&self) -> impl Iterator<Item = (&String, &Reserved)> {
        self.runs.iter()
    }

    /// The worst case run `run`, held, reserves in `key`, as it would be reserved at `now`.
    pub fn held_worst(&self, run: &str, key: &PotKey, now: i64) -> Option<Rights> {
        let w = self.waiting.get(run)?;
        self.holds(&w.repo, &w.rate, w.timeout, now).ok()?.into_iter().find(|h| h.pot == *key).map(|h| h.worst)
    }

    /// What this owner holds of pot `key`.
    pub fn holding(&self, key: &PotKey) -> Option<Holding> {
        let p = self.pots.get(key)?;
        Some(Holding { minted: p.kept.minted, got: sum(p.kept.got.values()), gave: sum(p.kept.gave.values()), spent: p.kept.spent, reserved: p.reserved, over: p.kept.over, free: p.free() })
    }

    /// What this owner has had from `peer` in pot `key` in all, and what it has given it.
    pub fn exchanged(&self, key: &PotKey, peer: &str) -> (Rights, Rights) {
        self.pots.get(key).map_or((NONE, NONE), |p| (p.got(peer), p.gave(peer)))
    }

    /// The pots it has.
    pub fn pots(&self) -> Vec<PotKey> {
        self.pots.keys().cloned().collect()
    }

    /// The declaration of `tenant`.
    pub fn decl(&self, tenant: &str) -> Option<&Decl> {
        self.decls.get(tenant)
    }

    // ---- inside ----

    fn live(&self, key: &PotKey, now: i64) -> bool {
        self.decls.get(&key.tenant).is_some_and(|d| d.per == key.per && key.period >= d.period(now))
    }

    /// The pot, made (and its share minted, its peers believed to hold their shares) if new.
    fn pot(&mut self, key: &PotKey) -> &mut Pot {
        if !self.pots.contains_key(key) {
            let mut p = Pot::default();
            if let Some(s) = self.splits.get(key) {
                p.belief = s.iter().filter(|(o, _)| **o != self.me).map(|(o, r)| (o.clone(), *r)).collect();
            }
            self.pots.insert(key.clone(), p);
        }
        self.mint(key);
        self.pots.entry(key.clone()).or_default()
    }

    /// Its share of the pot's split, minted once.
    fn mint(&mut self, key: &PotKey) {
        let Some(split) = self.splits.get(key) else { return };
        let share = split.get(&self.me).copied();
        let beliefs: Vec<(String, Rights)> = split.iter().filter(|(o, _)| **o != self.me).map(|(o, r)| (o.clone(), *r)).collect();
        let Some(p) = self.pots.get_mut(key) else { return };
        for (o, r) in beliefs {
            p.belief.entry(o).or_insert(r);
        }
        if p.kept.minted.is_none()
            && let Some(s) = share
        {
            p.kept.minted = Some(s);
            self.dirty_pots.insert(key.clone());
        }
    }

    /// What a run reserves in each budget that covers it, or why it never can.
    fn holds(&self, repo: &str, rate: &Rights, timeout: Option<Ms>, now: i64) -> Result<Vec<Hold>, String> {
        let mut out = Vec::new();
        for d in self.decls.values().filter(|d| d.covers(repo)) {
            let period = d.period(now);
            let r = d.mask(rate);
            let ms = match timeout {
                Some(t) => t,
                None if d.require_timeout => return Err(format!("takes {}'s runs without a timeout: its budget reserves each run's worst case, what it asks × its timeout (`--timeout`)", d.tenant)),
                None => u64::try_from(d.end(period).saturating_sub(now)).unwrap_or(0),
            };
            let worst = times(&r, ms);
            if !covers(&d.limits(), &worst) {
                return Err(format!("takes {} within {}'s budget of {} a period: it reserves what it asks × its timeout", words(&worst), d.tenant, words(&d.limits())));
            }
            out.push(Hold { pot: d.key(period), worst, rate: r });
        }
        Ok(out)
    }

    /// Whether each hold fits what's free in its pot, and no older need a peer said is owed there
    /// (`(since, by)`: the run's priority).
    fn fits(&mut self, holds: &[Hold], (since, by): (i64, &String)) -> bool {
        holds.iter().all(|h| {
            let pot = self.pot(&h.pot);
            let older = pot.needs.iter().any(|(peer, n)| (n.since, peer) < (since, by) && !nothing(&pot.owed(peer, n)));
            !older && covers(&pot.free(), &h.worst)
        })
    }

    fn reserve(&mut self, run: &str, holds: Vec<Hold>, timeout: Option<Ms>) {
        for h in &holds {
            let pot = self.pot(&h.pot);
            pot.reserved = plus(&pot.reserved, &h.worst);
        }
        self.runs.insert(run.to_owned(), Reserved { holds, go: false, started: None, timeout });
        self.dirty_runs.insert(run.to_owned());
    }

    fn unhold(&mut self, at: &(i64, u64), run: &str) {
        self.held.remove(at);
        self.waiting.remove(run);
        self.told.remove(run);
        self.dirty_held.insert(run.to_owned());
    }

    /// Something moved: needs owed to older runs elsewhere are given, the runs held here that now
    /// fit are released (oldest first, none past an older one of its budget), what's left goes to
    /// the needs behind, and each pot a held run waits on asks on.
    fn changed(&mut self, now: i64) -> Vec<Do> {
        let mut out = Vec::new();
        let live: Vec<PotKey> = self.pots.keys().filter(|k| self.live(k, now)).cloned().collect();
        for k in &live {
            out.extend(self.serve(k));
        }
        out.extend(self.retry(now));
        let live: Vec<PotKey> = self.pots.keys().filter(|k| self.live(k, now)).cloned().collect();
        for k in &live {
            out.extend(self.serve(k));
        }
        for k in &live {
            out.extend(self.ask_more(k, now));
        }
        out
    }

    /// The runs held, in order: each released that fits, each still held told why as that changes.
    fn retry(&mut self, now: i64) -> Vec<Do> {
        let mut out = Vec::new();
        let mut blocked: BTreeMap<String, u32> = BTreeMap::new();
        self.due = None;
        let order: Vec<((i64, u64), String)> = self.held.iter().map(|(k, r)| (*k, r.clone())).collect();
        for (at, run) in order {
            let Some(w) = self.waiting.get(&run).cloned() else {
                self.held.remove(&at);
                continue;
            };
            let holds = match self.holds(&w.repo, &w.rate, w.timeout, now) {
                Ok(h) => h,
                Err(need) => {
                    self.unhold(&at, &run);
                    out.push(Do::Tell { run, answer: Answer::Never { need, nearest: None } });
                    continue;
                }
            };
            if holds.is_empty() {
                self.unhold(&at, &run);
                out.push(Do::Release { run });
                continue;
            }
            let behind = holds.iter().any(|h| blocked.contains_key(&h.pot.tenant));
            if !behind && self.fits(&holds, (w.since, &self.me.clone())) {
                self.unhold(&at, &run);
                self.reserve(&run, holds, w.timeout);
                out.push(Do::Release { run });
                continue;
            }
            let reason = self.reason(&holds, &blocked);
            for h in &holds {
                *blocked.entry(h.pot.tenant.clone()).or_default() += 1;
                let end = self.decls.get(&h.pot.tenant).map(|d| d.end(h.pot.period));
                self.due = match (self.due, end) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
            }
            if self.told.get(&run) != Some(&reason) {
                self.told.insert(run.clone(), reason.clone());
                let answer = Answer::Queued { of: 0, reasons: vec![Count { reason, runners: 0 }], place: None, start: Estimate::default(), nearest: None };
                out.push(Do::Tell { run, answer });
            }
        }
        out
    }

    /// Why a held run waits, in words: the budget it's short in, what it reserves and what's free
    /// here; or how many of its budget's runs are held here before it.
    fn reason(&mut self, holds: &[Hold], blocked: &BTreeMap<String, u32>) -> String {
        for h in holds {
            if let Some(n) = blocked.get(&h.pot.tenant) {
                return format!("{}'s budget: {n} of its runs held here before it", h.pot.tenant);
            }
        }
        for h in holds {
            let free = self.pot(&h.pot).free();
            if !covers(&free, &h.worst) {
                return format!("{}'s budget: it reserves {}, and {} is free here this period", h.pot.tenant, words(&h.worst), words(&least(&free, &h.worst)));
            }
        }
        holds.first().map(|h| format!("{}'s budget: older runs elsewhere are owed its rights first", h.pot.tenant)).unwrap_or_default()
    }

    /// The first run held under `key`'s tenant: its id, when it was held, what it's short in `key`.
    fn head(&self, key: &PotKey, now: i64) -> Option<Head> {
        let d = self.decls.get(&key.tenant)?;
        let ((since, _), run) = self.held.iter().find(|(_, r)| self.waiting.get(*r).is_some_and(|w| d.covers(&w.repo)))?;
        let worst = self.held_worst(run, key, now)?;
        let free = self.pots.get(key).map_or(NONE, Pot::free);
        Some(Head { run: run.clone(), since: *since, short: less(&worst, &free) })
    }

    /// Needs said here, oldest run first, given what's free, until this owner's own older run.
    fn serve(&mut self, key: &PotKey) -> Vec<Do> {
        let mine = self.held.iter().find(|(_, r)| self.waiting.get(*r).is_some_and(|w| self.decls.get(&key.tenant).is_some_and(|d| d.covers(&w.repo)))).map(|((since, _), _)| *since);
        let me = self.me.clone();
        let Some(pot) = self.pots.get_mut(key) else { return Vec::new() };
        let mut order: Vec<(i64, String)> = pot.needs.iter().map(|(p, n)| (n.since, p.clone())).collect();
        order.sort();
        let mut out = Vec::new();
        let mut free = pot.free();
        for (since, peer) in order {
            if nothing(&free) {
                break;
            }
            #[cfg(test)]
            if mutant() == Some(Mutant::HoardWhenShort) && mine.is_some() {
                break;
            }
            if mine.is_some_and(|m| (m, &me) < (since, &peer)) {
                break;
            }
            let Some(n) = pot.needs.get(&peer).copied() else { continue };
            let g = least(&pot.owed(&peer, &n), &free);
            if nothing(&g) {
                continue;
            }
            let total = plus(&pot.gave(&peer), &g);
            #[cfg(test)]
            let keeps = mutant() == Some(Mutant::GiveBeforeLower);
            #[cfg(not(test))]
            let keeps = false;
            if !keeps {
                pot.kept.gave.insert(peer.clone(), total);
                free = less(&free, &g);
                self.dirty_pots.insert(key.clone());
            }
            out.push(Do::Say { to: peer, word: Word::Give { pot: key.clone(), total, free } });
        }
        out
    }

    /// The pot's first held run short in it asks a peer, one at a time, until every peer owes it its
    /// shortfall: the one believed to hold the most of it first. A peer whose debt no longer covers
    /// the shortfall (rights came and went to an older run meanwhile) is asked again. With none
    /// held, the needs it said are taken back.
    fn ask_more(&mut self, key: &PotKey, now: i64) -> Vec<Do> {
        let Some(head) = self.head(key, now) else { return self.withdraw(key) };
        if nothing(&head.short) {
            return Vec::new();
        }
        let start = self.decls.get(&key.tenant).map_or(now, |d| d.start(key.period));
        let candidates: Vec<String> = self.peers.iter().filter(|p| !self.away.contains(*p)).cloned().collect();
        let pot = self.pot(key);
        pot.round.head = Some(head.run.clone());
        if pot.round.at.is_some() {
            return Vec::new();
        }
        let mut pick: Option<(u64, String)> = None;
        for p in candidates {
            if covers(&pot.round.owed(&p, &pot.got(&p)), &head.short) {
                continue;
            }
            let b = pot.belief.get(&p).copied().unwrap_or(NONE);
            let score = least(&b, &head.short).iter().fold(0u64, |a, x| a.saturating_add(*x));
            if pick.as_ref().is_none_or(|(s, _)| score > *s) {
                pick = Some((score, p));
            }
        }
        let Some((_, peer)) = pick else { return Vec::new() };
        // Its shortfall, and what it spends over the round trip it measured to that peer.
        let elapsed = u64::try_from(now.saturating_sub(start)).unwrap_or(0).max(1);
        let spend = plus(&pot.kept.spent, &pot.reserved).map(|x| x.checked_div(elapsed).unwrap_or(0));
        let need = plus(&head.short, &times(&spend, pot.rtt.get(&peer).copied().unwrap_or(0)));
        let got = pot.got(&peer);
        pot.round.at = Some((peer.clone(), now));
        pot.round.told.insert(peer.clone(), (need, got));
        self.dirty_pots.insert(key.clone());
        self.seq = self.seq.saturating_add(1);
        vec![Do::Say { to: peer, word: Word::Need { pot: key.clone(), epoch: self.epoch, seq: self.seq, need, got, since: head.since } }]
    }

    /// Needs said in `key` taken back: nothing's held for it here.
    fn withdraw(&mut self, key: &PotKey) -> Vec<Do> {
        let epoch = self.epoch;
        let Some(pot) = self.pots.get_mut(key) else { return Vec::new() };
        pot.round.head = None;
        pot.round.at = None;
        let mut out = Vec::new();
        for (peer, told) in pot.round.told.iter_mut() {
            if nothing(&told.0) {
                continue;
            }
            let got = pot.kept.got.get(peer).copied().unwrap_or(NONE);
            *told = (NONE, got);
            self.seq = self.seq.saturating_add(1);
            out.push(Do::Say { to: peer.clone(), word: Word::Need { pot: key.clone(), epoch, seq: self.seq, need: NONE, got, since: 0 } });
        }
        out
    }
}

struct Head {
    run: String,
    since: i64,
    short: Rights,
}

/// Rights in words, each dimension it has: `2.5 core-hours`, `40 GiB-minutes of memory`,
/// `12 GPU-seconds`; `nothing` for none.
pub fn words(r: &Rights) -> String {
    const UNITS: [(u64, &str, &str); DIMS] = [(1000, "core", ""), (1024, "GiB", " of memory"), (1024, "GiB", " of disk"), (1, "GPU", "")];
    let parts: Vec<String> = r
        .iter()
        .zip(UNITS.iter())
        .filter(|(x, _)| **x > 0)
        .map(|(x, (unit, name, of))| {
            let ms = x.div_ceil(*unit);
            if ms >= 3_600_000 {
                let tenths = ms.saturating_mul(10).div_ceil(3_600_000);
                format!("{}.{} {name}-hours{of}", tenths / 10, tenths % 10)
            } else if ms >= 60_000 {
                format!("{} {name}-minutes{of}", ms.div_ceil(60_000))
            } else {
                format!("{} {name}-seconds{of}", ms.div_ceil(1000))
            }
        })
        .collect();
    if parts.is_empty() { "nothing".to_owned() } else { parts.join(", ") }
}

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;
