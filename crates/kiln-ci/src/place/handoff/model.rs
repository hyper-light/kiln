//! The hand-off's model under a nemesis (LB03, W05-R25; docs/sim.md): five nodes, each an owner and
//! a runner as every kiln node is, two of them owning runs probed at two runners each, with the
//! hand-off's own pieces inside ([`Acceptor`], [`Claim`], [`Members`]) and the shells around them
//! played as kiln's reach node plays them:
//!
//! - A runner with room offers a waiting run to its owner (again a while later if it's still
//!   waiting); the owner starts it at the first runner that offers and withdraws it from the rest.
//!   A run ends a while after it starts, and its runner tells its owner, which acknowledges.
//! - Words go on ordered links. A word lost (its link cut, its peer dead) is noticed by its sender
//!   a while later, the transport's own timeout: that's a suspicion. Contact again ends it, and
//!   what was owed is said again (probes, starts, ends, claims, a runner being added).
//! - A runner that needs a run's owner (room for it, its end to tell, or a claim it promised whose
//!   claimant went) while it suspects that owner hands the run off ([`ranked`]).
//! - An owner may probe a waiting run further at any time, through [`Members`]; it probes a
//!   restarted runner again, as kiln's owner does, and goes back to waiting on a lapsed start.
//!
//! The nemesis, until it stops: links cut and healed, suspicions that are wrong (of live, reachable
//! nodes), nodes killed for good (a claimant mid-claim among them), and runners that start again
//! (their probes lost, their promises kept, a running run lost with its process, their links
//! dropped as every node they talked to sees).
//!
//! The checker: at every step, no run starts twice, none starts once a node answered that it never
//! started, and no two answers of a run say different things (never owned twice). Once the nemesis
//! stops and everything settles: every run that a live node still holds, or holds a promise for,
//! and all of whose members are alive, was answered (adopted once its holders are reachable). A run
//! one of whose members is dead for good is held up for good: the limit the design states.
//!
//! The simulation's own numbers (how long things take, how long a lost word takes to be noticed,
//! how many faults) are the test's inputs, not numbers the hand-off acts on.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kiln_sim::{Queue, Rng, Trace, seeds};

use super::{Acceptor, Ballot, Claim, Decision, Gate, Handle, Held, Members, Mutant, Verdict, Word, planted, ranked};
use crate::place::ask::Ask;
use crate::sched::state::Fence;

const NODES: usize = 5;
/// Nodes `0..OWNERS` own runs; any node holds them and adopts them.
const OWNERS: usize = 2;
const RUNS_EACH: usize = 3;
const NEMESIS_MS: u64 = 30_000;
const SETTLE_MS: u64 = 900_000;
const STEPS_MAX: u64 = 400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    Waiting,
    Running,
    Ended(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Waiting,
    Starting(usize),
    Running(usize),
    Done,
    StoodDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Final {
    Ended(bool),
    NotStarted,
    /// It may have run: true whatever happened.
    Unsure,
}

#[derive(Debug, Clone)]
enum Msg {
    Probe { h: Handle, members: Vec<String>, b: Ballot },
    Start { h: Handle, b: Ballot },
    Withdraw { h: Handle, b: Ballot },
    Ack { h: Handle, b: Ballot },
    Offer { h: Handle },
    Started { h: Handle },
    Ended { h: Handle, ok: bool },
    Lapsed { h: Handle },
    W(Box<Word>),
}

#[derive(Debug, Clone)]
enum Ev {
    Deliver { from: usize, to: usize, msg: Msg },
    Room { at: usize, h: Handle },
    Finish { at: usize, h: Handle },
    Suspect { at: usize, of: usize },
    Cut { a: usize, b: usize, ms: u64 },
    Crash { node: usize },
    CrashClaimant,
    Restart { node: usize },
    Reprobe { owner: usize, at: usize },
    /// A link `at` lost to `of` is made again (kiln's redial, its backoff), once nothing cuts it.
    Relink { at: usize, of: usize },
    Widen { owner: usize, run: String },
    Calm,
}

#[derive(Default)]
struct Node {
    alive: bool,
    acc: Acceptor,
    holds: BTreeMap<Handle, Hold>,
    own: BTreeMap<String, (Handle, Stage)>,
    members: Members,
    claims: BTreeMap<Handle, Claim>,
    yielded: BTreeMap<Handle, Claim>,
    /// Runs it owns by a hand-off, under its claim's ballot: followed at a runner, or closed out
    /// (`None`).
    adopted: BTreeMap<Handle, (Ballot, Option<usize>)>,
    suspects: BTreeSet<usize>,
    handing: BTreeMap<Handle, usize>,
}

struct World {
    rng: Rng,
    now: u64,
    q: Queue<Ev>,
    nodes: Vec<Node>,
    names: Vec<String>,
    cut: BTreeMap<(usize, usize), u64>,
    lane: BTreeMap<(usize, usize), u64>,
    started: BTreeMap<Handle, Vec<usize>>,
    answers: BTreeMap<Handle, Vec<(usize, Final)>>,
    runs: Vec<Handle>,
    /// Every runner each run was ever said to have as a member: a dead one holds it up for good.
    ever: BTreeMap<Handle, BTreeSet<String>>,
    faults: bool,
    broken: Vec<String>,
    trace: Trace,
}

fn ask_of(h: &Handle) -> Ask {
    Ask { run: h.run.clone(), ..Ask::default() }
}

fn held_of(h: &Handle, hold: Hold) -> Held {
    match hold {
        Hold::Waiting => Held::Waiting { ask: ask_of(h) },
        Hold::Running => Held::Running { fence: Fence::default(), ask: Some(ask_of(h)) },
        Hold::Ended(ok) => Held::Ended { fence: Fence::default(), ok, ask: Some(ask_of(h)) },
    }
}

impl World {
    fn new(seed: u64) -> World {
        let names: Vec<String> = (0..NODES).map(|i| format!("n{i}")).collect();
        let nodes = (0..NODES).map(|_| Node { alive: true, ..Node::default() }).collect();
        World { rng: Rng::new(seed), now: 0, q: Queue::new(), nodes, names, cut: BTreeMap::new(), lane: BTreeMap::new(), started: BTreeMap::new(), answers: BTreeMap::new(), runs: Vec::new(), ever: BTreeMap::new(), faults: true, broken: Vec::new(), trace: Trace::new(seed, if seeds::replay().is_some() { 4_000 } else { 0 }) }
    }

    fn idx(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    fn name(&self, i: usize) -> String {
        self.names.get(i).cloned().unwrap_or_default()
    }

    fn alive(&self, i: usize) -> bool {
        self.nodes.get(i).is_some_and(|n| n.alive)
    }

    fn node(&mut self, i: usize) -> Option<&mut Node> {
        self.nodes.get_mut(i).filter(|n| n.alive)
    }

    fn at(&mut self, ms: u64, e: Ev) {
        let _ = self.q.push(Duration::from_millis(self.now.saturating_add(ms)), e);
    }

    fn is_cut(&self, a: usize, b: usize) -> bool {
        let k = (a.min(b), a.max(b));
        self.cut.get(&k).is_some_and(|until| *until > self.now)
    }

    /// A word from `from` to `to`: on its ordered link, or lost (its sender notices a while later).
    fn send(&mut self, from: usize, to: usize, msg: Msg) {
        if let Msg::Probe { h, members, .. } = &msg {
            let to_name = self.name(to);
            let e = self.ever.entry(h.clone()).or_default();
            e.extend(members.iter().cloned());
            e.insert(to_name);
        }
        if from == to {
            self.at(0, Ev::Deliver { from, to, msg });
            return;
        }
        if !self.alive(to) || self.is_cut(from, to) {
            let notice = self.rng.range(500, 3_000);
            self.at(notice, Ev::Suspect { at: from, of: to });
            let redial = notice.saturating_add(self.rng.range(200, 5_000));
            self.at(redial, Ev::Relink { at: from, of: to });
            return;
        }
        let delay = self.rng.range(1, 40);
        let at = self.now.saturating_add(delay).max(self.lane.get(&(from, to)).copied().unwrap_or(0));
        self.lane.insert((from, to), at);
        let _ = self.q.push(Duration::from_millis(at), Ev::Deliver { from, to, msg });
    }

    fn word(&mut self, from: usize, to: usize, w: Word) {
        self.send(from, to, Msg::W(Box::new(w)));
    }

    fn log_start(&mut self, h: &Handle, at: usize) {
        let before = self.started.get(h).cloned().unwrap_or_default();
        if !before.is_empty() {
            self.broken.push(format!("{h:?} started twice: at {before:?}, then at {at}"));
        }
        if self.answers.get(h).is_some_and(|a| a.iter().any(|(_, f)| *f == Final::NotStarted)) {
            self.broken.push(format!("{h:?} started at {at} after it was answered never started: owned twice"));
        }
        self.started.entry(h.clone()).or_default().push(at);
    }

    fn answer(&mut self, h: &Handle, by: usize, f: Final) {
        if f == Final::NotStarted && self.started.get(h).is_some_and(|s| !s.is_empty()) {
            self.broken.push(format!("{h:?} answered never started by {by}, but it started at {:?}: owned twice", self.started.get(h)));
        }
        let clash = |a: Final, b: Final| a != b && a != Final::Unsure && b != Final::Unsure;
        if let Some((o, g)) = self.answers.get(h).and_then(|a| a.iter().find(|(_, g)| clash(*g, f))).copied() {
            self.broken.push(format!("{h:?} answered {f:?} by {by} and {g:?} by {o}: owned twice"));
        }
        self.answers.entry(h.clone()).or_default().push((by, f));
    }

    /// Who a runner tells of run `h`: the claimant that owns it, its first owner if none does, or
    /// no one while a claim it promised is under way.
    fn owner_at(&self, at: usize, h: &Handle) -> Option<usize> {
        let n = self.nodes.get(at)?;
        match n.acc.row(h) {
            Some(r) if r.owned => self.idx(&r.ballot.by),
            Some(_) => None,
            None => self.idx(&h.owner),
        }
    }

    fn setup(&mut self) {
        for o in 0..OWNERS {
            for k in 0..RUNS_EACH {
                let h = Handle::new(&self.name(o), 1, &format!("r{k}"));
                let a = self.rng.index(NODES);
                let b = (a + 1 + self.rng.index(NODES - 1)) % NODES;
                let mut ms = Vec::new();
                for m in [a, b] {
                    let name = self.name(m);
                    if let Some(n) = self.nodes.get_mut(o)
                        && let Verdict::Pass(v) = n.members.write(&h.run, &name, true)
                    {
                        ms = v;
                    }
                }
                if let Some(n) = self.nodes.get_mut(o) {
                    n.own.insert(h.run.clone(), (h.clone(), Stage::Waiting));
                }
                for m in [a, b] {
                    self.send(o, m, Msg::Probe { h: h.clone(), members: ms.clone(), b: Ballot::FIRST });
                }
                self.runs.push(h);
            }
        }
        // The nemesis's faults, at times the seed picks.
        for _ in 0..6 {
            let (a, b) = (self.rng.index(NODES), self.rng.index(NODES));
            let (t, ms) = (self.rng.range(0, NEMESIS_MS), self.rng.range(500, 8_000));
            self.at(t, Ev::Cut { a, b, ms });
        }
        for _ in 0..8 {
            let (at, of, t) = (self.rng.index(NODES), self.rng.index(NODES), self.rng.range(0, NEMESIS_MS));
            self.at(t, Ev::Suspect { at, of });
        }
        for _ in 0..4 {
            let (owner, run, t) = (self.rng.index(OWNERS), format!("r{}", self.rng.index(RUNS_EACH)), self.rng.range(0, NEMESIS_MS));
            self.at(t, Ev::Widen { owner, run });
        }
        let t = self.rng.range(0, NEMESIS_MS);
        let node = self.rng.index(NODES);
        self.at(t, Ev::Crash { node });
        if self.rng.chance(500) {
            let t = self.rng.range(0, NEMESIS_MS);
            self.at(t, Ev::CrashClaimant);
        }
        if self.rng.chance(600) {
            let (node, t) = (OWNERS + self.rng.index(NODES - OWNERS), self.rng.range(0, NEMESIS_MS));
            self.at(t, Ev::Restart { node });
        }
        self.at(NEMESIS_MS, Ev::Calm);
    }

    fn step(&mut self, e: Ev) {
        match e {
            Ev::Deliver { from, to, msg } => {
                if !self.alive(to) {
                    return;
                }
                let was = self.nodes.get_mut(to).is_some_and(|n| n.suspects.remove(&from));
                if was {
                    self.contact(to, from);
                }
                self.on_msg(to, from, msg);
            }
            Ev::Room { at, h } => self.room(at, &h),
            Ev::Finish { at, h } => {
                if let Some(n) = self.node(at)
                    && n.holds.get(&h) == Some(&Hold::Running)
                {
                    n.holds.insert(h.clone(), Hold::Ended(true));
                    self.report_end(at, &h);
                }
            }
            Ev::Suspect { at, of } => self.suspect(at, of),
            Ev::Cut { a, b, ms } => {
                if a != b && self.faults {
                    self.cut.insert((a.min(b), a.max(b)), self.now.saturating_add(ms));
                    let (na, nb) = (self.rng.range(500, 3_000), self.rng.range(500, 3_000));
                    self.at(na, Ev::Suspect { at: a, of: b });
                    self.at(nb, Ev::Suspect { at: b, of: a });
                }
            }
            Ev::Crash { node } => self.crash(node),
            Ev::CrashClaimant => {
                let claiming = self.nodes.iter().position(|n| n.alive && !n.claims.is_empty());
                if let Some(node) = claiming {
                    self.crash(node);
                }
            }
            Ev::Restart { node } => self.restart(node),
            Ev::Reprobe { owner, at } => self.reprobe(owner, at),
            Ev::Relink { at, of } => {
                if !self.alive(at) || !self.alive(of) {
                    return;
                }
                if self.is_cut(at, of) {
                    let t = self.rng.range(200, 5_000);
                    self.at(t, Ev::Relink { at, of });
                } else if self.nodes.get_mut(at).is_some_and(|n| n.suspects.remove(&of)) {
                    self.contact(at, of);
                }
            }
            Ev::Widen { owner, run } => self.widen(owner, &run),
            Ev::Calm => {
                self.faults = false;
                self.cut.clear();
            }
        }
    }

    fn crash(&mut self, node: usize) {
        if !self.faults {
            return;
        }
        let Some(n) = self.nodes.get_mut(node) else { return };
        n.alive = false;
        for other in 0..NODES {
            if other != node {
                let t = self.rng.range(500, 3_000);
                self.at(t, Ev::Suspect { at: other, of: node });
            }
        }
    }

    /// A runner's process starts again: its probes and what it was claiming go, a running run is
    /// lost with its process (its record says it ran), its promises stay.
    fn restart(&mut self, node: usize) {
        if !self.faults {
            return;
        }
        let me = self.name(node);
        let Some(n) = self.node(node) else { return };
        n.holds.retain(|_, h| *h != Hold::Waiting);
        for h in n.holds.values_mut() {
            if *h == Hold::Running {
                *h = Hold::Ended(false);
            }
        }
        n.acc.restarted();
        n.claims.clear();
        n.yielded.clear();
        n.adopted.clear();
        n.handing.clear();
        n.suspects.clear();
        let ended: Vec<Handle> = n.holds.keys().cloned().collect();
        // Its own claims went with its process, its promises to them stayed: it claims again.
        let mine: Vec<Claim> = n.acc.rows().filter(|(_, r)| !r.owned && r.ballot.by == me).map(|(h, r)| Claim::new(h.clone(), r.ballot.clone(), r.members.clone(), r.held.clone().map(|held| (me.clone(), held)))).collect();
        for c in mine {
            self.claim(node, c);
        }
        for h in ended {
            self.report_end(node, &h);
        }
        for owner in 0..OWNERS {
            let t = self.rng.range(500, 3_000);
            self.at(t, Ev::Reprobe { owner, at: node });
        }
        // Its process went: its links dropped, which every node it talked to sees, and links again.
        for other in (0..NODES).filter(|o| *o != node) {
            let t = self.rng.range(500, 3_000);
            self.at(t, Ev::Suspect { at: other, of: node });
            let redial = t.saturating_add(self.rng.range(200, 5_000));
            self.at(redial, Ev::Relink { at: other, of: node });
        }
    }

    fn suspect(&mut self, at: usize, of: usize) {
        if at == of || !self.alive(at) {
            return;
        }
        let of_name = self.name(of);
        let Some(n) = self.nodes.get_mut(at) else { return };
        n.suspects.insert(of);
        // As a runner: an end to tell it, or a claim promised to it under way.
        let ends: Vec<Handle> = n.holds.iter().filter(|(_, h)| matches!(h, Hold::Ended(_))).map(|(h, _)| h.clone()).collect();
        let promised: Vec<Handle> = n.acc.rows().filter(|(_, r)| !r.owned && r.ballot.by == of_name).map(|(h, _)| h.clone()).collect();
        let handing: Vec<Handle> = n.handing.iter().filter(|(_, a)| **a == of).map(|(h, _)| h.clone()).collect();
        let again: Vec<Handle> = n.yielded.iter().filter(|(_, c)| matches!(c.decide(), Decision::Yield(b) if b.by == of_name)).map(|(h, _)| h.clone()).collect();
        for h in ends {
            if self.owner_at(at, &h) == Some(of) {
                self.report_end(at, &h);
            }
        }
        for h in promised.into_iter().chain(handing) {
            let held = self.held_here(at, &h);
            if let Some(held) = held {
                self.hand_off(at, &h, held, of);
            }
        }
        // As a claimant: one it yielded to is gone.
        for h in again {
            self.claim_again(at, &h);
        }
    }

    /// What runner `at` holds of run `h`: in its line or running, or in a promise it gave.
    fn held_here(&self, at: usize, h: &Handle) -> Option<Held> {
        let n = self.nodes.get(at)?;
        n.holds.get(h).map(|hold| held_of(h, *hold)).or_else(|| n.acc.row(h).and_then(|r| r.held.clone()))
    }

    /// `at` stopped suspecting `of`: what was owed it is said again.
    fn contact(&mut self, at: usize, of: usize) {
        let Some(n) = self.nodes.get(at) else { return };
        let of_name = self.name(of);
        let mut out: Vec<(usize, Msg)> = Vec::new();
        let mut probes: Vec<Handle> = Vec::new();
        for (run, (h, stage)) in &n.own {
            match stage {
                Stage::Waiting if n.members.members(run).contains(&of_name) => probes.push(h.clone()),
                Stage::Starting(x) if *x == of => out.push((of, Msg::Start { h: h.clone(), b: Ballot::FIRST })),
                _ => {}
            }
            for (m, add) in n.members.asking(run) {
                if m == of_name {
                    out.push((of, Msg::W(Box::new(Word::Extend { handle: h.clone(), add, ballot: Ballot::FIRST }))));
                }
            }
        }
        for (h, c) in &n.claims {
            if c.unpromised().contains(&of_name) {
                out.push((of, Msg::W(Box::new(Word::Claim { handle: h.clone(), ballot: c.ballot.clone(), members: c.members() }))));
            }
        }
        let ends: Vec<Handle> = n.holds.iter().filter(|(_, h)| matches!(h, Hold::Ended(_))).map(|(h, _)| h.clone()).collect();
        for (to, m) in out {
            self.send(at, to, m);
        }
        for h in probes {
            self.probe_at(at, &h, of);
        }
        for h in ends {
            if self.owner_at(at, &h) == Some(of) {
                self.report_end(at, &h);
            }
        }
    }

    fn on_msg(&mut self, at: usize, from: usize, msg: Msg) {
        match msg {
            Msg::Probe { h, members, b } => self.probe(at, from, h, members, &b),
            Msg::Start { h, b } => self.start(at, from, &h, &b),
            Msg::Withdraw { h, b } => {
                if self.gated(at, from, &h, &b)
                    && let Some(n) = self.node(at)
                    && n.holds.get(&h) == Some(&Hold::Waiting)
                {
                    n.holds.remove(&h);
                    n.acc.unheld(&h);
                }
            }
            Msg::Ack { h, b } => {
                if self.gated(at, from, &h, &b)
                    && let Some(n) = self.node(at)
                    && matches!(n.holds.get(&h), Some(Hold::Ended(_)))
                {
                    n.holds.remove(&h);
                    n.acc.unheld(&h);
                }
            }
            Msg::Offer { h } => self.offered(at, from, &h),
            Msg::Started { h } => {
                if let Some(n) = self.node(at)
                    && let Some((_, st)) = n.own.get_mut(&h.run).filter(|(o, _)| *o == h)
                    && *st == Stage::Starting(from)
                {
                    *st = Stage::Running(from);
                }
            }
            Msg::Ended { h, ok } => self.ended(at, from, &h, ok),
            Msg::Lapsed { h } => {
                let Some(n) = self.node(at) else { return };
                let Some((_, st)) = n.own.get_mut(&h.run).filter(|(o, _)| *o == h) else { return };
                if *st == Stage::Starting(from) {
                    *st = Stage::Waiting;
                    self.probe_at(at, &h, from);
                }
            }
            Msg::W(w) => self.on_word(at, from, *w),
        }
    }

    /// A word about `h` under ballot `b` at runner `at`: taken, or refused.
    fn gated(&mut self, at: usize, from: usize, h: &Handle, b: &Ballot) -> bool {
        let Some(n) = self.nodes.get(at) else { return false };
        match n.acc.gate(h, b) {
            Gate::Pass => true,
            Gate::Refuse(ballot) => {
                self.word(at, from, Word::Refused { handle: h.clone(), ballot });
                false
            }
        }
    }

    fn probe(&mut self, at: usize, from: usize, h: Handle, members: Vec<String>, b: &Ballot) {
        if !self.gated(at, from, &h, b) {
            return;
        }
        let Some(n) = self.node(at) else { return };
        n.acc.probed(&h, members);
        let hold = n.holds.get(&h).copied();
        if hold.is_none() {
            n.holds.insert(h.clone(), Hold::Waiting);
        }
        match hold {
            Some(Hold::Running) => self.send(at, from, Msg::Started { h }),
            Some(Hold::Ended(ok)) => self.send(at, from, Msg::Ended { h, ok }),
            Some(Hold::Waiting) => {}
            None => {
                let t = self.rng.range(100, 5_000);
                self.at(t, Ev::Room { at, h });
            }
        }
    }

    /// Runner `at` has room for waiting run `h`: it offers it to its owner, or, suspecting it,
    /// hands it off; offered again a while later if it still waits.
    fn room(&mut self, at: usize, h: &Handle) {
        if self.nodes.get(at).is_none_or(|n| !n.alive || n.holds.get(h) != Some(&Hold::Waiting)) {
            return;
        }
        let Some(o) = self.owner_at(at, h) else { return };
        if self.nodes.get(at).is_some_and(|n| n.suspects.contains(&o)) {
            self.hand_off(at, h, held_of(h, Hold::Waiting), o);
        } else {
            self.send(at, o, Msg::Offer { h: h.clone() });
        }
        let t = self.rng.range(1_000, 4_000);
        self.at(t, Ev::Room { at, h: h.clone() });
    }

    fn start(&mut self, at: usize, from: usize, h: &Handle, b: &Ballot) {
        if !self.gated(at, from, h, b) {
            return;
        }
        let Some(n) = self.node(at) else { return };
        match n.holds.get(h).copied() {
            Some(Hold::Waiting) => {
                n.holds.insert(h.clone(), Hold::Running);
                self.log_start(h, at);
                self.send(at, from, Msg::Started { h: h.clone() });
                let t = self.rng.range(200, 6_000);
                self.at(t, Ev::Finish { at, h: h.clone() });
            }
            Some(Hold::Running) => self.send(at, from, Msg::Started { h: h.clone() }),
            Some(Hold::Ended(ok)) => self.send(at, from, Msg::Ended { h: h.clone(), ok }),
            None => self.send(at, from, Msg::Lapsed { h: h.clone() }),
        }
    }

    fn report_end(&mut self, at: usize, h: &Handle) {
        let Some(Hold::Ended(ok)) = self.nodes.get(at).and_then(|n| n.holds.get(h)).copied() else { return };
        let held = held_of(h, Hold::Ended(ok));
        if let Some(n) = self.node(at) {
            n.acc.now_holds(h, Some(held.clone()));
        }
        let Some(o) = self.owner_at(at, h) else { return };
        if self.nodes.get(at).is_some_and(|n| n.suspects.contains(&o)) {
            self.hand_off(at, h, held, o);
        } else {
            self.send(at, o, Msg::Ended { h: h.clone(), ok });
        }
    }

    /// The owner takes the first offer of a waiting run, and withdraws it from its other members.
    fn offered(&mut self, at: usize, from: usize, h: &Handle) {
        let from_name = self.name(from);
        let Some(n) = self.node(at) else { return };
        let Some((_, st)) = n.own.get_mut(&h.run).filter(|(o, _)| o == h) else { return };
        if *st != Stage::Waiting {
            return;
        }
        *st = Stage::Starting(from);
        let others: Vec<String> = n.members.members(&h.run).into_iter().filter(|m| *m != from_name).collect();
        for m in &others {
            n.members.heard(&h.run, m, false);
        }
        self.send(at, from, Msg::Start { h: h.clone(), b: Ballot::FIRST });
        for m in others {
            if let Some(i) = self.idx(&m) {
                self.send(at, i, Msg::Withdraw { h: h.clone(), b: Ballot::FIRST });
            }
        }
    }

    fn ended(&mut self, at: usize, from: usize, h: &Handle, ok: bool) {
        let Some(n) = self.node(at) else { return };
        if let Some((b, _)) = n.adopted.get(h).filter(|(_, f)| *f == Some(from)).cloned() {
            n.adopted.remove(h);
            self.answer(h, at, Final::Ended(ok));
            self.send(at, from, Msg::Ack { h: h.clone(), b });
            return;
        }
        if let Some((_, st)) = n.own.get_mut(&h.run).filter(|(o, _)| o == h)
            && matches!(*st, Stage::Starting(x) | Stage::Running(x) if x == from)
        {
            *st = Stage::Done;
            n.members.forget(&h.run);
            self.answer(h, at, Final::Ended(ok));
            self.send(at, from, Msg::Ack { h: h.clone(), b: Ballot::FIRST });
        }
    }

    fn widen(&mut self, owner: usize, run: &str) {
        if !self.faults {
            return;
        }
        let y = self.rng.index(NODES);
        let Some(h) = self.nodes.get(owner).and_then(|n| n.own.get(run)).filter(|(_, st)| *st == Stage::Waiting).map(|(h, _)| h.clone()) else { return };
        self.probe_at(owner, &h, y);
    }

    /// Owner `owner` probes its run `h` at runner `to`, as every probe goes: through its members.
    fn probe_at(&mut self, owner: usize, h: &Handle, to: usize) {
        let to_name = self.name(to);
        let Some(n) = self.node(owner) else { return };
        match n.members.write(&h.run, &to_name, false) {
            Verdict::Pass(members) => self.send(owner, to, Msg::Probe { h: h.clone(), members, b: Ballot::FIRST }),
            Verdict::Ask { members, add } => {
                for m in members {
                    if let Some(i) = self.idx(&m) {
                        self.word(owner, i, Word::Extend { handle: h.clone(), add: add.clone(), ballot: Ballot::FIRST });
                    }
                }
            }
            Verdict::Wait => {}
        }
    }

    /// Owner `owner` sees runner `at` started again: its runs waiting there are probed there again.
    fn reprobe(&mut self, owner: usize, at: usize) {
        let at_name = self.name(at);
        let Some(n) = self.node(owner) else { return };
        let mut out = Vec::new();
        for (run, (h, st)) in &mut n.own {
            if matches!(*st, Stage::Waiting | Stage::Starting(_)) && n.members.members(run).contains(&at_name) {
                if *st == Stage::Starting(at) {
                    *st = Stage::Waiting;
                }
                if *st == Stage::Waiting {
                    out.push(h.clone());
                }
            }
        }
        for h in out {
            self.probe_at(owner, &h, at);
        }
    }

    /// Runner `at`, holding the news of run `h` (`held`) and suspecting its owner `owner`, asks the
    /// first live node in the run's rendezvous order to adopt it.
    fn hand_off(&mut self, at: usize, h: &Handle, held: Held, owner: usize) {
        if super::mutant() == Some(Mutant::NoHandOff) {
            return;
        }
        let names = self.names.clone();
        let Some(n) = self.node(at) else { return };
        if n.handing.get(h).is_some_and(|a| !n.suspects.contains(a)) {
            return;
        }
        let live: Vec<&str> = names.iter().enumerate().filter(|(i, _)| *i != owner && !n.suspects.contains(i)).map(|(_, s)| s.as_str()).collect();
        let Some(first) = ranked(h, live).first().map(|s| (*s).to_owned()) else { return };
        let Some(a) = names.iter().position(|s| *s == first) else { return };
        n.handing.insert(h.clone(), a);
        let mut members = n.acc.members(h);
        if let Some(me) = names.get(at)
            && !members.contains(me)
        {
            members.push(me.clone());
        }
        let why = names.get(owner).cloned().unwrap_or_default();
        let ballot = n.acc.row(h).map(|r| r.ballot.clone());
        if a == at {
            let me = self.name(at);
            self.adopt(at, at, h, members, Some((me, held)), ballot, Some(owner));
        } else {
            self.word(at, a, Word::Orphan { handle: h.clone(), members, held, ballot, why });
        }
    }

    /// Node `at` adopts run `h`, as runner `from` asked (or itself): it claims it at every member.
    #[allow(clippy::too_many_arguments)]
    fn adopt(&mut self, at: usize, from: usize, h: &Handle, members: Vec<String>, news: Option<(String, Held)>, promised: Option<Ballot>, gone: Option<usize>) {
        let me = self.name(at);
        let Some(n) = self.node(at) else { return };
        if let Some(g) = gone {
            n.suspects.insert(g);
        }
        // A claim under way goes on unless the news names a ballot at or above it; a run this node
        // thought it owned is claimed again (the holder of the news doesn't take it as owned).
        if n.claims.get(h).is_some_and(|c| promised.as_ref().is_none_or(|p| c.ballot > *p)) {
            return;
        }
        n.adopted.remove(h);
        n.claims.remove(h);
        let claim = match n.yielded.remove(h) {
            Some(y) if promised.as_ref().is_none_or(|p| y.ballot > *p) => match y.again() {
                Some(c) => c,
                None => {
                    n.yielded.insert(h.clone(), y);
                    return;
                }
            },
            _ => Claim::new(h.clone(), Ballot::above(promised.as_ref(), h, &me), members, news),
        };
        self.claim(at, claim);
        if from != at {
            let ballot = self.nodes.get(at).and_then(|n| n.claims.get(h)).map(|c| c.ballot.clone());
            self.word(at, from, Word::Adopting { handle: h.clone(), ballot, why: None });
        }
    }

    fn claim(&mut self, at: usize, c: Claim) {
        let (h, ballot, members, to) = (c.handle.clone(), c.ballot.clone(), c.members(), c.unpromised());
        if let Some(n) = self.node(at) {
            n.claims.insert(h.clone(), c);
        }
        for m in to {
            if let Some(i) = self.idx(&m) {
                self.word(at, i, Word::Claim { handle: h.clone(), ballot: ballot.clone(), members: members.clone() });
            }
        }
        self.decide(at, &h);
    }

    fn claim_again(&mut self, at: usize, h: &Handle) {
        let Some(n) = self.node(at) else { return };
        let Some(y) = n.yielded.remove(h) else { return };
        match y.again() {
            Some(c) => self.claim(at, c),
            None => {
                if let Some(n) = self.node(at) {
                    n.yielded.insert(h.clone(), y);
                }
            }
        }
    }

    fn decide(&mut self, at: usize, h: &Handle) {
        let Some(c) = self.nodes.get(at).and_then(|n| n.claims.get(h)) else { return };
        let (decision, ballot, members) = (c.decide(), c.ballot.clone(), c.members());
        // Its first owner claimed it back as any node would: its own ticket stands down, and it
        // follows or closes out the run as its claim says.
        let me = self.name(at);
        if h.owner == me
            && !matches!(decision, Decision::Waiting(_) | Decision::Yield(_))
            && let Some((_, st)) = self.node(at).and_then(|n| n.own.get_mut(&h.run))
            && *st != Stage::Done
        {
            *st = Stage::StoodDown;
        }
        let own = |w: &mut World| {
            for m in &members {
                if let Some(i) = w.idx(m) {
                    w.word(at, i, Word::Own { handle: h.clone(), ballot: ballot.clone() });
                }
            }
        };
        match decision {
            Decision::Waiting(_) => {}
            Decision::Yield(b) => {
                let Some(n) = self.node(at) else { return };
                let Some(c) = n.claims.remove(h) else { return };
                let gone = self.idx(&b.by).is_some_and(|i| self.nodes.get(at).is_some_and(|n| n.suspects.contains(&i)));
                if let Some(n) = self.node(at) {
                    n.yielded.insert(h.clone(), c);
                }
                if gone {
                    self.claim_again(at, h);
                }
            }
            Decision::Runs { at: runner, .. } => {
                let follow = self.idx(&runner);
                if let Some(n) = self.node(at) {
                    n.claims.remove(h);
                    n.adopted.insert(h.clone(), (ballot.clone(), follow));
                }
                own(self);
            }
            Decision::NotStarted { .. } => {
                if let Some(n) = self.node(at) {
                    n.claims.remove(h);
                    n.adopted.insert(h.clone(), (ballot.clone(), None));
                }
                own(self);
                self.answer(h, at, Final::NotStarted);
            }
            Decision::Unsure { .. } => {
                if let Some(n) = self.node(at) {
                    n.claims.remove(h);
                    n.adopted.insert(h.clone(), (ballot.clone(), None));
                }
                own(self);
                self.answer(h, at, Final::Unsure);
            }
            Decision::Gone => {
                if let Some(n) = self.node(at) {
                    n.claims.remove(h);
                }
            }
        }
    }

    fn on_word(&mut self, at: usize, from: usize, w: Word) {
        let from_name = self.name(from);
        match w {
            Word::Orphan { handle, members, held, ballot, why } => {
                let gone = self.idx(&why);
                self.adopt(at, from, &handle, members, Some((from_name, held)), ballot, gone);
            }
            Word::Adopting { .. } => {}
            Word::Claim { handle, ballot, members } => {
                let held = self.nodes.get(at).and_then(|n| n.holds.get(&handle)).map(|hold| held_of(&handle, *hold));
                let Some(n) = self.node(at) else { return };
                let reply = match n.acc.claim(&handle, &ballot, held, &members) {
                    Ok(p) => {
                        // Promised away: out of its line, its ask kept in the promise.
                        if n.holds.get(&handle) == Some(&Hold::Waiting) {
                            n.holds.remove(&handle);
                        }
                        Word::Promise { handle, ballot, held: p.held, members: p.members }
                    }
                    Err(b) => Word::Refused { handle, ballot: b },
                };
                self.word(at, from, reply);
            }
            Word::Promise { handle, ballot, held, members } => {
                let Some(n) = self.node(at) else { return };
                let Some(c) = n.claims.get_mut(&handle) else { return };
                let new = c.promised(&from_name, &ballot, held, &members);
                let all = c.members();
                for m in new {
                    if let Some(i) = self.idx(&m) {
                        self.word(at, i, Word::Claim { handle: handle.clone(), ballot: ballot.clone(), members: all.clone() });
                    }
                }
                self.decide(at, &handle);
            }
            Word::Refused { handle, ballot } => {
                let me = self.name(at);
                let Some(n) = self.node(at) else { return };
                if let Some(c) = n.claims.get_mut(&handle) {
                    c.refused(ballot);
                    self.decide(at, &handle);
                } else if handle.owner == me && !n.adopted.contains_key(&handle) {
                    // Its run was handed off: it stands down, saying nothing more of it.
                    if let Some((_, st)) = n.own.get_mut(&handle.run)
                        && !matches!(*st, Stage::Done)
                    {
                        *st = Stage::StoodDown;
                        n.members.forget(&handle.run);
                    }
                } else if ballot.by != me {
                    n.adopted.remove(&handle);
                }
            }
            Word::Own { handle, ballot } => {
                let Some(n) = self.node(at) else { return };
                if n.acc.own(&handle, &ballot) {
                    match n.holds.get(&handle).copied() {
                        Some(Hold::Running) => self.send(at, from, Msg::Started { h: handle }),
                        Some(Hold::Ended(_)) => self.report_end(at, &handle),
                        _ => {}
                    }
                }
            }
            Word::Extend { handle, add, ballot } => {
                let Some(n) = self.node(at) else { return };
                let reply = match n.acc.extend(&handle, &ballot, std::slice::from_ref(&add)) {
                    Ok(()) => Word::Extended { handle, add },
                    Err(ballot) => Word::Refused { handle, ballot },
                };
                self.word(at, from, reply);
            }
            Word::Extended { handle, add } => {
                let Some(n) = self.node(at) else { return };
                if n.own.get(&handle.run).is_none_or(|(o, st)| *o != handle || *st != Stage::Waiting) {
                    return;
                }
                let released = n.members.extended(&handle.run, &from_name, &add);
                for t in released.to {
                    if let Some(i) = self.idx(&t) {
                        self.send(at, i, Msg::Probe { h: handle.clone(), members: released.members.clone(), b: Ballot::FIRST });
                    }
                }
            }
        }
    }

    /// Once everything settled: every run a live node holds, or holds a promise for that isn't
    /// settled, was answered.
    fn settled(&mut self) {
        for h in self.runs.clone() {
            if self.answers.get(&h).is_some_and(|a| !a.is_empty()) {
                continue;
            }
            let holders: Vec<usize> = (0..NODES)
                .filter(|i| self.alive(*i))
                .filter(|i| self.nodes.get(*i).is_some_and(|n| n.holds.contains_key(&h) || n.acc.row(&h).is_some_and(|r| !r.owned || r.held.as_ref().is_some_and(Held::started))))
                .collect();
            // A member that can't be reached holds its run up until it can (and a dead one, for
            // good): the documented limit, not a miss.
            let reachable = self.ever.get(&h).is_none_or(|ms| ms.iter().all(|m| self.idx(m).is_some_and(|i| self.alive(i))));
            if !holders.is_empty() && reachable {
                let owner = self.idx(&h.owner).filter(|o| self.alive(*o)).and_then(|o| self.nodes.get(o)).and_then(|n| n.own.get(&h.run)).map(|(_, s)| *s);
                let what: Vec<String> = holders
                    .iter()
                    .filter_map(|i| self.nodes.get(*i).map(|n| format!("{i}: {:?} {:?} handing {:?} suspects {:?}", n.holds.get(&h), n.acc.row(&h), n.handing.get(&h), n.suspects)))
                    .collect();
                self.broken.push(format!("{h:?} is held at live {holders:?} and was never answered (its owner {}: {owner:?}): not adopted; {what:?}", h.owner));
            }
        }
    }
}

fn run_one(seed: u64) -> kiln_sim::seeds::Outcome {
    let mut w = World::new(seed);
    w.setup();
    let mut steps = 0u64;
    while let Some((at, e)) = w.q.pop() {
        w.now = u64::try_from(at.as_millis()).unwrap_or(u64::MAX);
        if w.now > NEMESIS_MS.saturating_add(SETTLE_MS) {
            break;
        }
        steps = steps.saturating_add(1);
        if steps > STEPS_MAX {
            return Err(format!("seed {seed}: past {STEPS_MAX} steps"));
        }
        w.trace.fold(w.now);
        let now = w.now;
        w.trace.note(|| format!("{now} {e:?}"));
        w.step(e);
        if let Some(b) = w.broken.first() {
            return Err(if w.trace.keeps() { format!("{b}\n{}", w.trace.tail_text()) } else { b.clone() });
        }
    }
    w.settled();
    match w.broken.first() {
        Some(b) => Err(if w.trace.keeps() { format!("{b}\n{}", w.trace.tail_text()) } else { b.clone() }),
        None => Ok(w.trace.digest()),
    }
}

/// LB03, W05-R25: under crashes, partitions and wrong suspicions, no run is started twice or owned
/// twice, and every run whose holders are reachable is adopted once its owner is gone; each seed
/// runs the same way twice.
#[test]
fn lb03_w05_r25_no_run_is_owned_or_started_twice_and_every_orphan_is_adopted() -> Result<(), String> {
    let report = seeds::run_budget("handoff", 400, |seed| seeds::twice(seed, run_one));
    report.into_result().map(|_| ())
}

/// Each planted bug is caught by the property it breaks, within the seeds a run of the model takes.
#[test]
fn lb03_each_planted_mistake_is_caught() -> Result<(), String> {
    let cases = [
        (Mutant::AcceptLowerBallot, "twice"),
        (Mutant::SkipPlace, "twice"),
        (Mutant::AdoptWithoutPromise, "twice"),
        (Mutant::SkipExtend, "twice"),
        (Mutant::ForgetPromise, "twice"),
        (Mutant::NoHandOff, "not adopted"),
    ];
    let mut missed = Vec::new();
    for (m, property) in cases {
        let caught = planted(Some(m), || (0..2_000u64).find_map(|seed| run_one(seed).err().filter(|e| e.contains(property))));
        if caught.is_none() {
            missed.push(format!("{m:?} (by {property})"));
        }
    }
    if missed.is_empty() { Ok(()) } else { Err(format!("not caught: {missed:?}")) }
}
