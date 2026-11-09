//! The budgets' model (docs/sim.md; LB08): three owners and one tenant's budget, runs asked at
//! random owners, links cut and mended, owners crashing and coming back from what they kept, words
//! said again and out of order, ends told twice, and time crossing periods. A word is lost only
//! with its link (reach's links are reliable while up): a cut drops what's on it. Checked after
//! every step, each property on its own so a planted bug shows which one it breaks:
//!
//! - **Zero overspend**: in each period, what the runs admitted against it could use (their worst
//!   case until they end, then what they used) is within the budget.
//! - **Rights conserved**: no owner has had more from a peer than the peer gave it; each minted at
//!   most its share; each owner's books (what it reserved) are its open runs' worst cases, its
//!   holding covers them and what it spent, and every run out there is reserved by its owner.
//! - **Progress when connected** (at the end: every link mended, every owner back, every word
//!   said): no right is in flight, every run held was told why, and none is held while the rights
//!   free across the owners cover the oldest held run.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::place::keys::SplitMix;
use crate::sched::state::Cpus;

const OWNERS: [&str; 3] = ["a", "b", "c"];
const PER: Ms = 1_000;
const TENANT: &str = "acme/app";
/// The budget a period: four CPUs the whole period.
const LIMIT: u64 = 4_000 * PER;

/// A bug planted in the model's shell rather than the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plant {
    /// The shell says what the core decided before it keeps what changed.
    PublishBeforeKeep,
}

#[derive(Debug, Clone, Copy)]
pub struct Checks {
    pub overspend: bool,
    pub conserved: bool,
    pub progress: bool,
}

pub const ALL: Checks = Checks { overspend: true, conserved: true, progress: true };

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum K {
    Pot(PotKey),
    Run(String),
    Held(String),
}

struct Node {
    name: String,
    epoch: u64,
    core: Budgets,
    kept: BTreeMap<K, Row>,
    pending: Vec<Do>,
    up: bool,
}

#[derive(Debug, Clone, Default)]
struct Truth {
    owner: usize,
    rate: Rights,
    timeout: Ms,
    worst: Rights,
    pot: Option<PotKey>,
    published: bool,
    started: Option<i64>,
    used: Option<Rights>,
    gone: bool,
    told: Option<Answer>,
}

struct World {
    rng: SplitMix,
    now: i64,
    nodes: Vec<Node>,
    cut: BTreeSet<(usize, usize)>,
    net: Vec<(usize, usize, Word)>,
    queued: Vec<(usize, usize, Word)>,
    said: Vec<(usize, usize, Word)>,
    runs: BTreeMap<String, Truth>,
    ends: Vec<(String, End)>,
    next: u64,
    decl: Decl,
    splits: Vec<Split>,
    plant: Option<Plant>,
    checks: Checks,
}

fn pair(i: usize, j: usize) -> (usize, usize) {
    (i.min(j), i.max(j))
}

fn cpu(r: &Rights) -> u64 {
    r.first().copied().unwrap_or(0)
}

impl World {
    fn new(seed: u64, plant: Option<Plant>, checks: Checks) -> World {
        let mut rng = SplitMix(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xB1D6);
        let decl = Decl { tenant: TENANT.into(), per: PER, limit: [Some(LIMIT), None, None, None], require_timeout: true };
        // Each period's split, as the server decides it: uneven, an owner sometimes left out.
        let mut splits = Vec::new();
        for p in 0..12u64 {
            let (x, y) = (u64::from(rng.below(5)) * LIMIT / 4, u64::from(rng.below(5)) * LIMIT / 4);
            let (lo, hi) = (x.min(y), x.max(y));
            let mut shares = BTreeMap::new();
            for (name, share) in OWNERS.iter().zip([lo, hi - lo, LIMIT - hi]) {
                if share > 0 {
                    shares.insert((*name).to_owned(), [share, 0, 0, 0]);
                }
            }
            splits.push(Split { key: decl.key(p), shares });
        }
        let peers: Vec<String> = OWNERS.iter().map(|s| (*s).to_owned()).collect();
        let mut nodes = Vec::new();
        for name in OWNERS {
            let mut core = Budgets::new(name, 1);
            let pending = core.declare(vec![decl.clone()], splits.clone(), peers.clone(), 0);
            nodes.push(Node { name: name.to_owned(), epoch: 1, core, kept: BTreeMap::new(), pending, up: true });
        }
        World { rng, now: 0, nodes, cut: BTreeSet::new(), net: Vec::new(), queued: Vec::new(), said: Vec::new(), runs: BTreeMap::new(), ends: Vec::new(), next: 0, decl, splits, plant, checks }
    }

    fn index(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    fn up(&self, i: usize) -> bool {
        self.nodes.get(i).is_some_and(|n| n.up)
    }

    fn linked(&self, i: usize, j: usize) -> bool {
        self.up(i) && self.up(j) && !self.cut.contains(&pair(i, j))
    }

    fn pick<T>(&mut self, v: &[T]) -> Option<usize> {
        let n = u32::try_from(v.len()).ok().filter(|n| *n > 0)?;
        Some(self.rng.below(n) as usize)
    }

    /// What node `i` decided goes out: at once with the plant, else once kept.
    fn decided(&mut self, i: usize, outs: Vec<Do>) {
        if self.plant == Some(Plant::PublishBeforeKeep) {
            self.publish(i, outs);
        } else if let Some(n) = self.nodes.get_mut(i) {
            n.pending.extend(outs);
        }
    }

    fn publish(&mut self, i: usize, outs: Vec<Do>) {
        for d in outs {
            match d {
                Do::Say { to, word } => {
                    let Some(j) = self.index(&to) else { continue };
                    if self.linked(i, j) {
                        self.net.push((i, j, word));
                    } else if matches!(word, Word::Need { .. }) && self.up(i) {
                        // A need goes on the node's link to the peer, sent once it's up.
                        self.queued.push((i, j, word));
                    }
                }
                Do::Release { run } => {
                    let pot = self.nodes.get(i).and_then(|n| n.core.reservation(&run)).and_then(|r| r.holds.first().map(|h| h.pot.clone()));
                    if let Some(n) = self.nodes.get_mut(i) {
                        n.core.go(&run);
                    }
                    if let Some(t) = self.runs.get_mut(&run) {
                        t.published = true;
                        t.pot = pot.or_else(|| Some(self.decl.key(self.decl.period(self.now))));
                    }
                }
                Do::Tell { run, answer } => {
                    if let Some(t) = self.runs.get_mut(&run) {
                        if matches!(answer, Answer::Never { .. }) {
                            t.gone = true;
                        }
                        t.told = Some(answer);
                    }
                }
            }
        }
    }

    fn commit(&mut self, i: usize) {
        let Some(n) = self.nodes.get_mut(i) else { return };
        if !n.up {
            return;
        }
        for c in n.core.changes() {
            match c {
                Change::Pot(k, Some(kept)) => {
                    n.kept.insert(K::Pot(k.clone()), Row::Pot(k, kept));
                }
                Change::Pot(k, None) => {
                    n.kept.remove(&K::Pot(k));
                }
                Change::Run(r, Some(row)) => {
                    n.kept.insert(K::Run(r.clone()), Row::Run(r, row));
                }
                Change::Run(r, None) => {
                    n.kept.remove(&K::Run(r));
                }
                Change::Held(r, Some(row)) => {
                    n.kept.insert(K::Held(r.clone()), Row::Held(r, row));
                }
                Change::Held(r, None) => {
                    n.kept.remove(&K::Held(r));
                }
            }
        }
        let outs = std::mem::take(&mut n.pending);
        self.publish(i, outs);
    }

    fn link_event(&mut self, i: usize, j: usize, up: bool) {
        let now = self.now;
        for (a, b) in [(i, j), (j, i)] {
            if !self.up(a) {
                continue;
            }
            let Some(peer) = self.nodes.get(b).map(|n| n.name.clone()) else { continue };
            let outs = self.nodes.get_mut(a).map(|n| n.core.link(&peer, up, now)).unwrap_or_default();
            self.decided(a, outs);
        }
        if up {
            let (go, stay): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queued).into_iter().partition(|(a, b, _)| pair(*a, *b) == pair(i, j));
            self.queued = stay;
            self.net.extend(go);
        }
    }

    fn cut_link(&mut self, i: usize, j: usize) {
        let was = self.linked(i, j);
        self.cut.insert(pair(i, j));
        self.net.retain(|(a, b, _)| pair(*a, *b) != pair(i, j));
        if was {
            self.link_event(i, j, false);
        }
    }

    fn mend(&mut self, i: usize, j: usize) {
        if self.cut.remove(&pair(i, j)) && self.linked(i, j) {
            self.link_event(i, j, true);
        }
    }

    fn crash(&mut self, i: usize) {
        let others: Vec<usize> = (0..self.nodes.len()).filter(|j| *j != i && self.linked(i, *j)).collect();
        if let Some(n) = self.nodes.get_mut(i) {
            n.up = false;
            n.pending.clear();
        }
        self.net.retain(|(a, b, _)| *a != i && *b != i);
        self.queued.retain(|(a, _, _)| *a != i);
        for j in others {
            if let Some(peer) = self.nodes.get(i).map(|n| n.name.clone()) {
                let now = self.now;
                let outs = self.nodes.get_mut(j).map(|n| n.core.link(&peer, false, now)).unwrap_or_default();
                self.decided(j, outs);
            }
        }
    }

    fn restart(&mut self, i: usize) {
        let now = self.now;
        let peers: Vec<String> = OWNERS.iter().map(|s| (*s).to_owned()).collect();
        let (decl, splits) = (self.decl.clone(), self.splits.clone());
        let Some(n) = self.nodes.get_mut(i) else { return };
        n.epoch += 1;
        n.core = Budgets::restore(&n.name, n.epoch, n.kept.values().cloned().collect());
        n.up = true;
        // The shell's reconciling, before anything else: a reservation whose run's start was never
        // said is let go. Then the declarations, which may release what's held.
        let unsaid: Vec<String> = n.core.reservations().map(|(r, _)| r.clone()).filter(|r| !self.runs.get(r).is_some_and(|t| t.published)).collect();
        let mut outs = Vec::new();
        for r in unsaid {
            outs.extend(n.core.ended(&r, End::NotStarted, now));
        }
        outs.extend(n.core.declare(vec![decl], splits, peers, now));
        let held: BTreeSet<String> = n.core.held().into_iter().map(|(r, _)| r).collect();
        // What it held and didn't keep, or reserved and never said, is gone with its process.
        for (run, t) in self.runs.iter_mut() {
            if t.owner == i && !t.published && !t.gone && !held.contains(run) {
                t.gone = true;
            }
        }
        self.decided(i, outs);
        for j in 0..self.nodes.len() {
            if j != i && self.linked(i, j) {
                self.link_event(i, j, true);
            }
        }
    }

    fn ask(&mut self, i: usize) {
        if !self.up(i) {
            return;
        }
        self.next += 1;
        let run = format!("r{}", self.next);
        let cores = u64::from(self.rng.below(4)) + 1;
        let timeout = u64::from(self.rng.below(400)) + 20;
        let request = Resources { cpu: Cpus::whole(cores), ..Resources::default() };
        let rate = super::rate(&request);
        let worst = [cores * 1000 * timeout, 0, 0, 0];
        self.runs.insert(run.clone(), Truth { owner: i, rate: [cpu(&rate), 0, 0, 0], timeout, worst, ..Truth::default() });
        let now = self.now;
        let Some(n) = self.nodes.get_mut(i) else { return };
        let (gate, mut outs) = n.core.ask(&run, TENANT, &request, Some(timeout), now);
        match gate {
            Gate::Reserved => outs.insert(0, Do::Release { run }),
            Gate::Never(a) => outs.push(Do::Tell { run, answer: a }),
            Gate::Held | Gate::Free => {}
        }
        self.decided(i, outs);
    }

    fn deliver(&mut self, k: usize, again: bool) {
        let m = if again { self.said.get(k).cloned() } else { (k < self.net.len()).then(|| self.net.remove(k)) };
        let Some((i, j, word)) = m else { return };
        if !self.linked(i, j) {
            return;
        }
        if !again {
            self.said.push((i, j, word.clone()));
            if self.said.len() > 64 {
                self.said.remove(0);
            }
        }
        let now = self.now;
        let Some(from) = self.nodes.get(i).map(|n| n.name.clone()) else { return };
        let outs = self.nodes.get_mut(j).map(|n| n.core.heard(&from, word, now)).unwrap_or_default();
        self.decided(j, outs);
    }

    fn start(&mut self) {
        let ready: Vec<String> = self.runs.iter().filter(|(_, t)| t.published && t.started.is_none() && !t.gone).map(|(r, _)| r.clone()).collect();
        let Some(k) = self.pick(&ready) else { return };
        let Some(run) = ready.get(k).cloned() else { return };
        let now = self.now;
        let Some(t) = self.runs.get_mut(&run) else { return };
        t.started = Some(now);
        let owner = t.owner;
        // Its runner's word reaches its owner only while it's up.
        if self.up(owner)
            && let Some(n) = self.nodes.get_mut(owner)
        {
            n.core.started(&run, now);
        }
    }

    fn end(&mut self) {
        let running: Vec<String> = self.runs.iter().filter(|(_, t)| t.started.is_some() && t.used.is_none()).map(|(r, _)| r.clone()).collect();
        let Some(k) = self.pick(&running) else { return };
        let Some(run) = running.get(k).cloned() else { return };
        let lost = self.rng.below(8) == 0;
        let now = self.now;
        let Some(t) = self.runs.get_mut(&run) else { return };
        let ran = u64::try_from(now - t.started.unwrap_or(now)).unwrap_or(0).min(t.timeout);
        t.used = Some([cpu(&t.rate) * ran, 0, 0, 0]);
        self.ends.push((run, if lost { End::Unknown } else { End::Ran }));
    }

    fn tell_end(&mut self) {
        let Some(k) = self.pick(&self.ends.clone()) else { return };
        let Some((run, end)) = self.ends.get(k).cloned() else { return };
        let Some(owner) = self.runs.get(&run).map(|t| t.owner) else { return };
        if !self.up(owner) {
            return;
        }
        // Its runner says it until its owner keeps it: sometimes twice.
        if self.rng.below(3) != 0 {
            self.ends.remove(k);
        }
        let now = self.now;
        let outs = self.nodes.get_mut(owner).map(|n| n.core.ended(&run, end, now)).unwrap_or_default();
        self.decided(owner, outs);
    }

    fn cancel(&mut self) {
        let held: Vec<(usize, String)> = self.nodes.iter().enumerate().filter(|(_, n)| n.up).flat_map(|(i, n)| n.core.held().into_iter().map(move |(r, _)| (i, r))).collect();
        let Some(k) = self.pick(&held) else { return };
        let Some((i, run)) = held.get(k).cloned() else { return };
        if let Some(t) = self.runs.get_mut(&run) {
            t.gone = true;
        }
        let now = self.now;
        let outs = self.nodes.get_mut(i).map(|n| n.core.cancel(&run, now)).unwrap_or_default();
        self.decided(i, outs);
    }

    fn tick(&mut self) {
        let now = self.now;
        for i in 0..self.nodes.len() {
            let due = self.nodes.get(i).is_some_and(|n| n.up && n.core.timer().is_some_and(|t| t <= now));
            if due || self.rng.below(4) == 0 {
                let outs = self.nodes.get_mut(i).filter(|n| n.up).map(|n| n.core.tick(now)).unwrap_or_default();
                self.decided(i, outs);
            }
        }
    }

    fn step(&mut self) {
        self.now += i64::from(self.rng.below(6));
        let n = u32::try_from(self.nodes.len()).unwrap_or(1);
        let i = self.rng.below(n) as usize;
        let j = (i + 1 + self.rng.below(n - 1) as usize) % self.nodes.len();
        match self.rng.below(26) {
            0..=3 => self.ask(i),
            4..=9 => {
                if let Some(k) = self.pick(&self.net.clone()) {
                    self.deliver(k, false);
                }
            }
            10 => {
                if let Some(k) = self.pick(&self.said.clone()) {
                    self.deliver(k, true);
                }
            }
            11 => self.cut_link(i, j),
            12 => self.mend(i, j),
            13 => {
                if self.up(i) && self.nodes.iter().filter(|n| n.up).count() > 1 {
                    self.crash(i);
                }
            }
            14 => {
                if !self.up(i) {
                    self.restart(i);
                }
            }
            15..=17 => self.commit(i),
            18..=19 => self.start(),
            20..=21 => self.end(),
            22 => self.tell_end(),
            23 => self.cancel(),
            _ => {
                self.now += i64::from(self.rng.below(250));
                self.tick();
            }
        }
    }

    /// What node `i` holds as far as the world can see: its process's, or, while it's down, what
    /// it kept.
    fn view(&self, i: usize) -> Option<std::borrow::Cow<'_, Budgets>> {
        let n = self.nodes.get(i)?;
        Some(if n.up { std::borrow::Cow::Borrowed(&n.core) } else { std::borrow::Cow::Owned(Budgets::restore(&n.name, n.epoch, n.kept.values().cloned().collect())) })
    }

    fn check(&self) -> Result<(), String> {
        let views: Vec<std::borrow::Cow<'_, Budgets>> = (0..self.nodes.len()).filter_map(|i| self.view(i)).collect();
        if self.checks.overspend {
            let mut exposure: BTreeMap<u64, u64> = BTreeMap::new();
            for t in self.runs.values().filter(|t| t.published) {
                let p = t.pot.as_ref().map_or(0, |k| k.period);
                *exposure.entry(p).or_default() += cpu(&t.used.unwrap_or(t.worst));
            }
            if let Some((p, e)) = exposure.iter().find(|(_, e)| **e > LIMIT) {
                return Err(format!("overspent: period {p}'s runs could use {e} of {LIMIT}"));
            }
        }
        if self.checks.conserved {
            let keys: BTreeSet<PotKey> = views.iter().flat_map(|v| v.pots()).collect();
            for key in &keys {
                let share = |name: &str| self.splits.iter().find(|s| s.key == *key).and_then(|s| s.shares.get(name).copied());
                for (i, v) in views.iter().enumerate() {
                    let name = OWNERS.get(i).copied().unwrap_or_default();
                    let Some(h) = v.holding(key) else { continue };
                    if h.minted.is_some() && h.minted != share(name) {
                        return Err(format!("{name} minted {:?} of {key:?}, its share {:?}", h.minted, share(name)));
                    }
                    let open: Rights = v.reservations().flat_map(|(_, r)| r.holds.iter()).filter(|x| x.pot == *key).fold(NONE, |a, x| plus(&a, &x.worst));
                    if open != h.reserved {
                        return Err(format!("{name}'s books in {key:?}: {:?} reserved, its runs' worst cases {open:?}", h.reserved));
                    }
                    if !covers(&plus(&h.minted.unwrap_or(NONE), &h.got), &plus(&plus(&h.gave, &h.spent), &h.reserved)) {
                        return Err(format!("{name} holds less than it spent and reserved in {key:?}: {h:?}"));
                    }
                    // A giver that let a period's pot go (it ended, nothing reserved there) has
                    // nothing to compare: those rights ended with their period.
                    for (j, w) in views.iter().enumerate().filter(|(_, w)| w.holding(key).is_some()) {
                        let other = OWNERS.get(j).copied().unwrap_or_default();
                        let (got, _) = v.exchanged(key, other);
                        let (_, gave) = w.exchanged(key, name);
                        if !covers(&gave, &got) {
                            return Err(format!("{name} had {got:?} from {other} in {key:?}, which gave it {gave:?}"));
                        }
                    }
                }
            }
            for (run, t) in self.runs.iter().filter(|(_, t)| t.published && t.used.is_none()) {
                if views.get(t.owner).is_some_and(|v| v.reservation(run).is_none()) {
                    return Err(format!("{run} is out there and its owner's books don't reserve it"));
                }
            }
        }
        Ok(())
    }

    /// Every link mended, every owner back, everything kept and every word said.
    fn heal(&mut self) -> Result<(), String> {
        for _ in 0..200 {
            for i in 0..self.nodes.len() {
                if !self.up(i) {
                    self.restart(i);
                }
            }
            let cut: Vec<(usize, usize)> = self.cut.iter().copied().collect();
            for (i, j) in cut {
                self.mend(i, j);
            }
            for i in 0..self.nodes.len() {
                self.commit(i);
            }
            while !self.net.is_empty() {
                self.deliver(0, false);
            }
            for i in 0..self.nodes.len() {
                self.commit(i);
            }
            if self.net.is_empty() && self.queued.is_empty() && self.nodes.iter().all(|n| n.pending.is_empty()) {
                return Ok(());
            }
        }
        Err("held while connected: words went on past 200 rounds".into())
    }

    fn progress(&self) -> Result<(), String> {
        let key = self.decl.key(self.decl.period(self.now));
        let views: Vec<std::borrow::Cow<'_, Budgets>> = (0..self.nodes.len()).filter_map(|i| self.view(i)).collect();
        for (i, v) in views.iter().enumerate() {
            for (j, w) in views.iter().enumerate() {
                let (got, _) = v.exchanged(&key, OWNERS.get(j).copied().unwrap_or_default());
                let (_, gave) = w.exchanged(&key, OWNERS.get(i).copied().unwrap_or_default());
                if got != gave {
                    return Err(format!("held while connected: rights in flight from {j} to {i}: gave {gave:?}, got {got:?}"));
                }
            }
        }
        let mut oldest: Option<(i64, usize, String)> = None;
        for (i, v) in views.iter().enumerate() {
            for (run, since) in v.held() {
                if self.runs.get(&run).is_none_or(|t| t.told.is_none()) {
                    return Err(format!("held while its asker wasn't told why: {run}"));
                }
                if oldest.as_ref().is_none_or(|(s, o, _)| (since, i) < (*s, *o)) {
                    oldest = Some((since, i, run));
                }
            }
        }
        let Some((_, i, run)) = oldest else { return Ok(()) };
        let free = views.iter().filter_map(|v| v.holding(&key)).fold(NONE, |a, h| plus(&a, &h.free));
        if let Some(worst) = views.get(i).and_then(|v| v.held_worst(&run, &key, self.now))
            && covers(&free, &worst)
        {
            return Err(format!("held while the rights free across owners ({}) cover the oldest held run {run} ({}) at {i}: {}", cpu(&free), cpu(&worst), self.dump(&key)));
        }
        Ok(())
    }

    /// Each owner's state in `key`, for a failure's words.
    fn dump(&self, key: &PotKey) -> String {
        let mut out = String::new();
        for n in &self.nodes {
            let c = &n.core;
            let p = c.pots.get(key);
            out.push_str(&format!(
                "\n{} up={} free={:?} held={:?} round={:?} needs={:?} belief={:?} away={:?}",
                n.name,
                n.up,
                p.map(|p| cpu(&p.free())),
                c.held(),
                p.map(|p| (&p.round.head, &p.round.at, p.round.told.iter().map(|(k, (v, g))| (k.clone(), cpu(v), cpu(g), cpu(&p.round.owed(k, &p.got(k))))).collect::<Vec<_>>())),
                p.map(|p| p.needs.iter().map(|(k, v)| (k.clone(), cpu(&v.need), cpu(&v.got), v.since, cpu(&p.owed(k, v)))).collect::<Vec<_>>()),
                p.map(|p| p.belief.iter().map(|(k, v)| (k.clone(), cpu(v))).collect::<Vec<_>>()),
                c.away,
            ));
        }
        out
    }

    fn run(&mut self, steps: u32) -> Result<(u32, u32), String> {
        let mut held = 0;
        for _ in 0..steps {
            self.step();
            self.check()?;
            held += u32::from(self.nodes.iter().any(|n| !n.core.held().is_empty()));
        }
        self.heal()?;
        self.check()?;
        if self.checks.progress {
            self.progress()?;
        }
        let released = u32::try_from(self.runs.values().filter(|t| t.published).count()).unwrap_or(0);
        Ok((held, released))
    }
}

/// Seeds `seeds`, each `steps` long: how many steps had a run held and how many runs were placed.
pub fn sweep(seeds: std::ops::Range<u64>, steps: u32, plant: Option<Plant>, m: Option<Mutant>, checks: Checks) -> Result<(u32, u32), String> {
    let mut total = (0, 0);
    for seed in seeds {
        let (h, r) = planted(m, || World::new(seed, plant, checks).run(steps)).map_err(|e| format!("seed {seed}: {e}"))?;
        total = (total.0 + h, total.1 + r);
    }
    Ok(total)
}
