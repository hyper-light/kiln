//! The kernel's model (kiln `crates/kiln-ci/src/lb/model.rs` at f20858c9c, adapted to boxes):
//! several owners and nodes, every pair linked, driven by a seeded generator through random
//! interleavings of asks, deliveries (each link in order, links in any order against each other),
//! deletes, Hints, links that drop (losing what was on them, both ends told) and come back, askers
//! deleting boxes, room taken outside the ledger, and node agents that start again. Hex's
//! adaptation: an agent that starts again drops its links and recovers the boxes that outlived it
//! (kiln's runner started again empty, its links up). It checks at every step:
//!
//! - no oversubscription: what each node has admitted and holds, by the model's own count of the
//!   boxes it saw start (and still running, agent restarts or not), never passes its budget in any
//!   dimension, and the node's ledger agrees with that count;
//! - no box starts twice;
//! - fences: a start, wait or Go sent under an epoch that isn't the node's is declined;
//!
//! and at quiescence (every link up, everything delivered, every Hint drained):
//!
//! - no ask lost: each is started (and its node runs it), waiting (and a node keeps it in line), or
//!   gone and its asker told how, and each asker was told its box's state; nothing runs that its
//!   owner doesn't know of;
//! - every owner's view of every node equals that node's room;
//!
//! and at the end, with every box deleted, that every ask ended or never fits, and that the same
//! seed gives the same trace (determinism).

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use super::*;

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014): small, fast, the same everywhere.
#[derive(Debug, Clone)]
pub(super) struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform below `n` (Lemire's multiply-shift), 0 for `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        u64::try_from((u128::from(self.next()) * u128::from(n)) >> 64).unwrap()
    }

    fn below32(&mut self, n: u64) -> u32 {
        u32::try_from(self.below(n)).unwrap()
    }

    fn below_usize(&mut self, n: usize) -> usize {
        usize::try_from(self.below(u64::try_from(n).unwrap())).unwrap()
    }
}

#[derive(Debug, Default)]
struct Link {
    down: VecDeque<ToRunner>,
    up: VecDeque<ToOwner>,
    dead: bool,
}

#[derive(Debug, Clone)]
struct Ask {
    owner: u32,
    run: Handle,
    need: Amounts,
    /// The last thing its asker heard.
    told: Option<Told>,
    /// Where it started (node, handle there), once.
    started: Option<(u32, Handle)>,
    /// It ended, was lost or never fits: its owner is done with it.
    done: bool,
    /// Its asker deleted it.
    cancelled: bool,
}

struct World {
    owners: Vec<Owner>,
    runners: Vec<Runner>,
    sigs: Vec<Vec<SigId>>,
    links: Vec<Link>,
    asks: Vec<Ask>,
    by: BTreeMap<(u32, Handle), usize>,
    /// What runs at each node, by the model's own count of the boxes it saw start.
    live: Vec<BTreeMap<Handle, usize>>,
    rng: Rng,
    trace: u64,
}

/// FNV-1a over a line of the trace.
fn fold(h: u64, s: &str) -> u64 {
    s.bytes()
        .fold(h, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn u(x: u32) -> usize {
    usize::try_from(x).unwrap()
}

impl World {
    fn new(seed: u64, owners: u32, runners: u32) -> World {
        let mut rng = Rng(seed);
        let mut rs = Vec::new();
        for i in 0..runners {
            // Mixed shapes: some with GPUs, some short of memory.
            let cpus = 1 + rng.below(6);
            let budget = [
                cpus * CPU,
                4 + rng.below(12),
                0,
                if rng.below(3) == 0 {
                    1 + rng.below(2)
                } else {
                    0
                },
            ];
            rs.push(Runner::new(RunnerId(i), budget, 1));
        }
        let mut os = Vec::new();
        let mut sigs = Vec::new();
        for o in 0..owners {
            let mut ow = Owner::new(OwnerId(o));
            let mut out = Out::default();
            for r in rs.iter_mut() {
                r.watch(OwnerId(o));
                ow.join(r.id(), r.budget(), r.word(), &mut out).unwrap();
            }
            // Every node, and a subset.
            let all = ow.sig().unwrap();
            let some = ow.sig().unwrap();
            for i in 0..runners {
                ow.add(all, RunnerId(i), &mut out).unwrap();
                if rng.below(2) == 0 || i == 0 {
                    ow.add(some, RunnerId(i), &mut out).unwrap();
                }
            }
            sigs.push(vec![all, some]);
            os.push(ow);
        }
        let links = (0..owners * runners).map(|_| Link::default()).collect();
        World {
            owners: os,
            runners: rs,
            sigs,
            links,
            asks: Vec::new(),
            by: BTreeMap::new(),
            live: vec![BTreeMap::new(); u(runners)],
            rng,
            trace: 0xcbf2_9ce4_8422_2325,
        }
    }

    fn nr(&self) -> u32 {
        u32::try_from(self.runners.len()).unwrap()
    }

    fn no(&self) -> u32 {
        u32::try_from(self.owners.len()).unwrap()
    }

    fn link(&mut self, o: u32, r: u32) -> Option<&mut Link> {
        let i = u(o) * self.runners.len() + u(r);
        self.links.get_mut(i)
    }

    fn note(&mut self, what: std::fmt::Arguments<'_>) {
        let mut s = String::new();
        let _ = s.write_fmt(what);
        self.trace = fold(self.trace, &s);
    }

    /// Sends what an owner's call left in `out`, and records what its askers heard.
    fn owner_out(&mut self, o: u32, out: &mut Out) -> Result<(), String> {
        for (r, m) in std::mem::take(&mut out.to) {
            if let Some(l) = self.link(o, r.0)
                && !l.dead
            {
                l.down.push_back(m);
            }
        }
        for t in std::mem::take(&mut out.told) {
            self.told(o, t)?;
        }
        Ok(())
    }

    fn told(&mut self, o: u32, t: Told) -> Result<(), String> {
        self.note(format_args!("told {o} {t:?}"));
        let run = match t {
            Told::Started { run, .. }
            | Told::Waiting { run }
            | Told::Never { run }
            | Told::Ended { run }
            | Told::Lost { run } => run,
        };
        let Some(&i) = self.by.get(&(o, run)) else {
            return Err(format!("owner {o} told {t:?} of a box never asked"));
        };
        let a = self.asks.get_mut(i).ok_or("ask")?;
        a.told = Some(t);
        if matches!(
            t,
            Told::Never { .. } | Told::Ended { .. } | Told::Lost { .. }
        ) {
            a.done = true;
        }
        Ok(())
    }

    /// Sends what a node said to owner `o`, watching for starts.
    fn runner_say(&mut self, r: u32, o: OwnerId, m: ToOwner) -> Result<(), String> {
        if let ToOwner::Started { run, at, .. } = m {
            let Some(&i) = self.by.get(&(o.0, run)) else {
                return Err(format!(
                    "node {r} started a box owner {} never asked ({run:?})",
                    o.0
                ));
            };
            let live = self.live.get_mut(u(r)).ok_or("node")?;
            match live.get(&at) {
                Some(&j) if j == i => {}
                Some(&j) => {
                    return Err(format!(
                        "node {r} reports ask {i} at {at:?}, where ask {j} runs"
                    ));
                }
                None => {
                    let a = self.asks.get_mut(i).ok_or("ask")?;
                    if let Some(before) = a.started {
                        return Err(format!(
                            "ask {i} ({run:?} of owner {}) started twice: at {before:?} and at node {r} {at:?}",
                            o.0
                        ));
                    }
                    a.started = Some((r, at));
                    live.insert(at, i);
                }
            }
        }
        if let Some(l) = self.link(o.0, r)
            && !l.dead
        {
            l.up.push_back(m);
        }
        Ok(())
    }

    fn runner_out(&mut self, r: u32, rout: &mut RunnerOut) -> Result<(), String> {
        for (o, m) in std::mem::take(rout) {
            self.runner_say(r, o, m)?;
        }
        Ok(())
    }

    fn ask(&mut self) -> Result<(), String> {
        let o = self.rng.below32(u64::from(self.no()));
        let pick = self.rng.below_usize(2);
        let s = *self.sigs.get(u(o)).and_then(|v| v.get(pick)).ok_or("sig")?;
        let cpu = match self.rng.below(8) {
            0 => CPU / 2,
            7 => 7 * CPU,
            n => n * CPU / 2,
        };
        let mem = if self.rng.below(2) == 0 {
            0
        } else {
            1 + self.rng.below(8)
        };
        let gpu = u64::from(self.rng.below(6) == 0);
        let need = [cpu, mem, 0, gpu];
        let mut out = Out::default();
        let ow = self.owners.get_mut(u(o)).ok_or("owner")?;
        let res = ow.ask(s, need, &mut out);
        self.note(format_args!("ask {o} {need:?} {res:?}"));
        match res {
            Ok(a) => {
                let i = self.asks.len();
                let told = if a.send.is_some() {
                    None
                } else {
                    Some(Told::Waiting { run: a.run })
                };
                self.asks.push(Ask {
                    owner: o,
                    run: a.run,
                    need,
                    told,
                    started: None,
                    done: false,
                    cancelled: false,
                });
                if self.by.insert((o, a.run), i).is_some() {
                    return Err(format!("owner {o} gave out handle {:?} twice", a.run));
                }
                if let Some((r, m)) = a.send {
                    out.to.insert(0, (r, m));
                }
            }
            Err(Refused::Never) => {}
            Err(e) => return Err(format!("ask refused: {e:?}")),
        }
        self.owner_out(o, &mut out)
    }

    fn deliver_down(&mut self, o: u32, r: u32) -> Result<(), String> {
        let Some(m) = self.link(o, r).and_then(|l| l.down.pop_front()) else {
            return Ok(());
        };
        let runner = self.runners.get_mut(u(r)).ok_or("node")?;
        let epoch = runner.epoch();
        let mut rout = RunnerOut::new();
        let reply = runner.hear(m, &mut rout);
        self.note(format_args!("down {o} {r} {m:?} -> {reply:?}"));
        // The fence: a start, wait or Go under another epoch is declined.
        let sent = match m {
            ToRunner::Start(s) | ToRunner::Wait(s) => Some(s.epoch),
            ToRunner::Go { epoch, .. } => Some(epoch),
            ToRunner::Withdraw { .. } => None,
        };
        if sent.is_some_and(|e| e != epoch)
            && !matches!(
                reply,
                Some(ToOwner::Declined {
                    why: Why::Epoch,
                    ..
                })
            )
        {
            return Err(format!(
                "node {r} under epoch {epoch} took {m:?}: {reply:?}"
            ));
        }
        if let Some(reply) = reply {
            self.runner_say(r, OwnerId(o), reply)?;
        }
        self.runner_out(r, &mut rout)
    }

    fn deliver_up(&mut self, o: u32, r: u32) -> Result<(), String> {
        let Some(m) = self.link(o, r).and_then(|l| l.up.pop_front()) else {
            return Ok(());
        };
        let mut out = Out::default();
        let told = self.owners.get_mut(u(o)).ok_or("owner")?.hear(m, &mut out);
        self.note(format_args!("up {o} {r} {m:?} -> {told:?}"));
        if let Some(t) = told {
            out.told.insert(0, t);
        }
        self.owner_out(o, &mut out)
    }

    /// A box on node `r` is deleted.
    fn end(&mut self, r: u32) -> Result<(), String> {
        let runner = self.runners.get_mut(u(r)).ok_or("node")?;
        let runs = runner.admitted();
        if runs.is_empty() {
            return Ok(());
        }
        let pick = self.rng.below_usize(runs.len());
        let (_, _, at, _) = *runs.get(pick).ok_or("box")?;
        let mut rout = RunnerOut::new();
        let ended = self.runners.get_mut(u(r)).ok_or("node")?.end(at, &mut rout);
        self.note(format_args!("end {r} {at:?} {ended:?}"));
        let live = self.live.get_mut(u(r)).ok_or("node")?;
        if live.remove(&at).is_none() {
            return Err(format!(
                "node {r} ran {at:?}, which the model never saw start"
            ));
        }
        if let Some((o, m)) = ended {
            self.runner_say(r, o, m)?;
        }
        self.runner_out(r, &mut rout)
    }

    fn drain(&mut self, r: u32) -> Result<(), String> {
        let mut rout = RunnerOut::new();
        self.runners.get_mut(u(r)).ok_or("node")?.drain(&mut rout);
        self.runner_out(r, &mut rout)
    }

    fn reset(&mut self, o: u32, r: u32) -> Result<(), String> {
        let Some(l) = self.link(o, r) else {
            return Ok(());
        };
        if l.dead {
            return Ok(());
        }
        l.dead = true;
        l.down.clear();
        l.up.clear();
        self.note(format_args!("reset {o} {r}"));
        let mut out = Out::default();
        self.owners
            .get_mut(u(o))
            .ok_or("owner")?
            .link_down(RunnerId(r), &mut out);
        self.owner_out(o, &mut out)?;
        let mut rout = RunnerOut::new();
        self.runners
            .get_mut(u(r))
            .ok_or("node")?
            .link_down(OwnerId(o), &mut rout);
        self.runner_out(r, &mut rout)
    }

    fn relink(&mut self, o: u32, r: u32) -> Result<(), String> {
        let Some(l) = self.link(o, r) else {
            return Ok(());
        };
        if !l.dead {
            return Ok(());
        }
        l.dead = false;
        self.note(format_args!("relink {o} {r}"));
        let mut rout = RunnerOut::new();
        self.runners
            .get_mut(u(r))
            .ok_or("node")?
            .link_up(OwnerId(o), &mut rout);
        self.runner_out(r, &mut rout)
    }

    /// An asker deletes one of its boxes before it is known to run.
    fn cancel(&mut self) -> Result<(), String> {
        let open: Vec<usize> = self
            .asks
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.done && !a.cancelled)
            .map(|(i, _)| i)
            .collect();
        let pick = self.rng.below_usize(open.len());
        let Some(&i) = open.get(pick) else {
            return Ok(());
        };
        let (o, run) = self.asks.get(i).map(|a| (a.owner, a.run)).ok_or("ask")?;
        let mut out = Out::default();
        let forgotten = self
            .owners
            .get_mut(u(o))
            .ok_or("owner")?
            .cancel(run, &mut out);
        self.note(format_args!("cancel {o} {run:?} {forgotten}"));
        if let Some(a) = self.asks.get_mut(i) {
            a.cancelled = true;
            a.done |= forgotten;
        }
        self.owner_out(o, &mut out)
    }

    /// The room what runs beside Hex takes on a node changes (or goes).
    fn outside(&mut self, r: u32) -> Result<(), String> {
        let runner = self.runners.get_mut(u(r)).ok_or("node")?;
        let [c, m, _, g] = runner.budget();
        let used = if self.rng.below(2) == 0 {
            [0; 4]
        } else {
            [
                self.rng.below(c + 1),
                self.rng.below(m + 1),
                0,
                self.rng.below(g + 1),
            ]
        };
        let mut rout = RunnerOut::new();
        self.runners
            .get_mut(u(r))
            .ok_or("node")?
            .outside(used, &mut rout);
        self.note(format_args!("outside {r} {used:?}"));
        self.runner_out(r, &mut rout)
    }

    /// Node `r`'s agent starts again (Hex's adaptation): its links drop, both ends told and what
    /// was on them lost, and it recovers the boxes that outlived it, each under a new handle there.
    /// They still run: the model moves them to their new handles, and keeps any the node forgot
    /// under its old one (it runs, whatever the ledger says).
    fn restart(&mut self, r: u32) -> Result<(), String> {
        for o in 0..self.no() {
            self.reset(o, r)?;
        }
        let runner = self.runners.get_mut(u(r)).ok_or("node")?;
        let admitted = runner.admitted();
        let kept: Vec<Kept> = admitted
            .iter()
            .map(|&(owner, run, _, need)| Kept { owner, run, need })
            .collect();
        let new = runner.recover(&kept);
        self.note(format_args!("restart {r} {new:?}"));
        let live = self.live.get_mut(u(r)).ok_or("node")?;
        for ((_, _, old, _), now) in admitted.iter().zip(new) {
            if let Some(now) = now
                && let Some(i) = live.remove(old)
            {
                live.insert(now, i);
            }
        }
        Ok(())
    }

    /// One random event.
    fn step(&mut self) -> Result<(), String> {
        let (no, nr) = (u64::from(self.no()), u64::from(self.nr()));
        let (o, r) = (self.rng.below32(no), self.rng.below32(nr));
        match self.rng.below(100) {
            0..=20 => self.ask(),
            21..=43 => self.deliver_down(o, r),
            44..=66 => self.deliver_up(o, r),
            67..=78 => self.end(r),
            79..=87 => self.drain(r),
            88..=91 => self.reset(o, r),
            92..=95 => self.relink(o, r),
            96 => self.cancel(),
            97 => self.outside(r),
            _ => self.restart(r),
        }?;
        self.check_always()
    }

    /// What holds after every step: no node past its budget (by the model's count of the boxes it
    /// saw start and the room its line holds), and the node's ledger agrees with that count.
    fn check_always(&self) -> Result<(), String> {
        for (r, runner) in self.runners.iter().enumerate() {
            let budget = runner.budget();
            let mut taken = [0u64; DIMS];
            let live = self.live.get(r).ok_or("node")?;
            for &i in live.values() {
                let a = self.asks.get(i).ok_or("ask")?;
                for (t, n) in taken.iter_mut().zip(a.need) {
                    *t = t.saturating_add(n);
                }
            }
            for (_, _, _, held, need) in runner.waiting() {
                if held {
                    for (t, n) in taken.iter_mut().zip(need) {
                        *t = t.saturating_add(n);
                    }
                }
            }
            if taken.iter().zip(&budget).any(|(t, b)| t > b) {
                return Err(format!("node {r} oversubscribed: {taken:?} of {budget:?}"));
            }
            let admitted: Vec<Handle> = runner.admitted().iter().map(|a| a.2).collect();
            let seen: Vec<Handle> = live.keys().copied().collect();
            if admitted != seen {
                return Err(format!(
                    "node {r} runs {admitted:?}, the model saw {seen:?} start"
                ));
            }
        }
        Ok(())
    }

    /// Brings every link up and delivers everything, in random order, until nothing moves.
    fn settle(&mut self) -> Result<(), String> {
        let (no, nr) = (self.no(), self.nr());
        for o in 0..no {
            for r in 0..nr {
                self.relink(o, r)?;
            }
        }
        loop {
            let busy: Vec<(u32, u32, bool)> = (0..no)
                .flat_map(|o| (0..nr).map(move |r| (o, r)))
                .flat_map(|(o, r)| {
                    let l = self.links.get(u(o) * u(nr) + u(r));
                    let d = l.is_some_and(|l| !l.down.is_empty());
                    let up = l.is_some_and(|l| !l.up.is_empty());
                    [(o, r, true, d), (o, r, false, up)]
                })
                .filter(|x| x.3)
                .map(|x| (x.0, x.1, x.2))
                .collect();
            if busy.is_empty() {
                for r in 0..nr {
                    self.drain(r)?;
                }
                if self
                    .links
                    .iter()
                    .all(|l| l.up.is_empty() && l.down.is_empty())
                {
                    return Ok(());
                }
                continue;
            }
            let pick = self.rng.below_usize(busy.len());
            let &(o, r, down) = busy.get(pick).ok_or("busy")?;
            if down {
                self.deliver_down(o, r)?;
            } else {
                self.deliver_up(o, r)?;
            }
            self.check_always()?;
        }
    }

    /// What holds at quiescence.
    fn check_quiet(&self) -> Result<(), String> {
        // Views.
        for (o, ow) in self.owners.iter().enumerate() {
            for r in &self.runners {
                let room = r.word().room();
                if ow.view(r.id()) != Some(room) {
                    return Err(format!(
                        "owner {o} sees node {} with {:?}, it has {room:?}",
                        r.id().0,
                        ow.view(r.id())
                    ));
                }
            }
        }
        // Every ask.
        for (i, a) in self.asks.iter().enumerate() {
            let ow = self.owners.get(u(a.owner)).ok_or("owner")?;
            match ow.state(a.run) {
                Some(State::Started { runner, at }) => {
                    if self.live.get(u(runner.0)).and_then(|l| l.get(&at)) != Some(&i) {
                        return Err(format!(
                            "ask {i}: its owner says it runs on {} {at:?}, which doesn't run it",
                            runner.0
                        ));
                    }
                    if !matches!(a.told, Some(Told::Started { .. })) {
                        return Err(format!("ask {i} runs, its asker heard {:?}", a.told));
                    }
                }
                Some(State::Waiting { at }) => {
                    let places: Vec<RunnerId> = at.iter().flatten().copied().collect();
                    if places.is_empty() {
                        return Err(format!("ask {i} waits nowhere, with every link up"));
                    }
                    for p in places {
                        let r = self.runners.get(u(p.0)).ok_or("node")?;
                        if !r
                            .waiting()
                            .iter()
                            .any(|w| w.0.0 == a.owner && w.1 == a.run && !w.3)
                        {
                            return Err(format!(
                                "ask {i} waits at node {}, whose line hasn't it: {:?}",
                                p.0,
                                r.waiting()
                            ));
                        }
                    }
                    if !matches!(a.told, Some(Told::Waiting { .. })) {
                        return Err(format!("ask {i} waits, its asker heard {:?}", a.told));
                    }
                }
                Some(s) => return Err(format!("ask {i} is {s:?} with nothing in flight")),
                None => {
                    if !a.done && !a.cancelled {
                        return Err(format!(
                            "ask {i} is gone from its owner, its asker last heard {:?}",
                            a.told
                        ));
                    }
                }
            }
        }
        // Nothing runs or waits that its owner doesn't know of.
        for (r, live) in self.live.iter().enumerate() {
            for (at, &i) in live {
                let a = self.asks.get(i).ok_or("ask")?;
                let ow = self.owners.get(u(a.owner)).ok_or("owner")?;
                let here = State::Started {
                    runner: RunnerId(u32::try_from(r).unwrap()),
                    at: *at,
                };
                if ow.state(a.run) != Some(here) {
                    return Err(format!(
                        "node {r} runs ask {i}, whose owner has it {:?}",
                        ow.state(a.run)
                    ));
                }
            }
        }
        for r in &self.runners {
            for (o, run, _, held, _) in r.waiting() {
                if held {
                    return Err(format!(
                        "node {} holds room for {run:?} with nothing in flight",
                        r.id().0
                    ));
                }
                let ow = self.owners.get(u(o.0)).ok_or("owner")?;
                let here = matches!(ow.state(run), Some(State::Waiting { at }) if at.contains(&Some(r.id())));
                if !here {
                    return Err(format!(
                        "node {} keeps {run:?} in line for owner {}, which has it {:?}",
                        r.id().0,
                        o.0,
                        ow.state(run)
                    ));
                }
            }
        }
        Ok(())
    }

    /// Deletes every box, settling between, until nothing waits: every ask must be done. The room
    /// what runs beside Hex takes goes first.
    fn finish(&mut self) -> Result<(), String> {
        for r in 0..self.nr() {
            let mut rout = RunnerOut::new();
            self.runners
                .get_mut(u(r))
                .ok_or("node")?
                .outside([0; 4], &mut rout);
            self.runner_out(r, &mut rout)?;
        }
        for _ in 0..self.asks.len() + 8 {
            self.settle()?;
            self.check_quiet()?;
            if self.asks.iter().all(|a| {
                a.done
                    || (a.cancelled
                        && self
                            .owners
                            .get(u(a.owner))
                            .is_none_or(|o| o.state(a.run).is_none()))
            }) {
                return Ok(());
            }
            for r in 0..self.nr() {
                while !self.runners.get(u(r)).ok_or("node")?.admitted().is_empty() {
                    self.end(r)?;
                }
            }
        }
        let left: Vec<usize> = self
            .asks
            .iter()
            .enumerate()
            .filter(|(_, a)| !a.done && !a.cancelled)
            .map(|(i, _)| i)
            .collect();
        Err(format!("asks never done: {left:?}"))
    }
}

/// One run of the model: `steps` random events, settling and checking every `every`, then the
/// finish. Its trace.
fn run(seed: u64, steps: usize, every: usize) -> Result<u64, String> {
    let mut w = World::new(seed, 3, 5);
    for i in 0..steps {
        w.step().map_err(|e| format!("seed {seed} step {i}: {e}"))?;
        if (i + 1) % every == 0 {
            w.settle()
                .map_err(|e| format!("seed {seed} settling at {i}: {e}"))?;
            w.check_quiet()
                .map_err(|e| format!("seed {seed} quiet at {i}: {e}"))?;
        }
    }
    w.finish()
        .map_err(|e| format!("seed {seed} finishing: {e}"))?;
    Ok(w.trace)
}

/// Every property over many seeds, and determinism: the same seed, the same trace.
fn model(seeds: u64, steps: usize) -> Result<(), String> {
    for seed in 0..seeds {
        let a = run(seed, steps, 97)?;
        let b = run(seed, steps, 97)?;
        if a != b {
            return Err(format!("seed {seed}: two runs, two traces"));
        }
    }
    Ok(())
}

/// Never past a budget, never started twice, a refused box goes to another node that can take it,
/// and a box outlives its node's agent: the model, unplanted, over kiln's 48 seeds of 2,000 steps.
#[test]
fn the_model_holds_every_property() -> Result<(), String> {
    model(48, 2000)
}

/// Each bug planted against one of the model's properties is caught by the model. Those against a
/// pick's quality (NoDeduct, NoKeep, NoTurn, MostFree) and the fence (NoFence: the model's agent
/// restarts drop the links, so nothing of the epoch before reaches the new one) are caught by
/// their unit tests in tests.rs.
#[test]
fn the_model_catches_every_mutant() -> Result<(), String> {
    let mut missed = Vec::new();
    for m in [
        Mutant::AdmitPastRoom,
        Mutant::GoTwice,
        Mutant::DropDeclined,
        Mutant::NoGeneration,
        Mutant::NoHint,
        Mutant::Unordered,
        Mutant::RecoverForgets,
    ] {
        if with_mutant(Some(m), || model(48, 2000)).is_ok() {
            missed.push(m);
        }
    }
    if !missed.is_empty() {
        return Err(format!("the model missed {missed:?}"));
    }
    Ok(())
}
