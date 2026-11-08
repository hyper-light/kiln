//! Placement's hard state across restarts (W05-R25, LB03; docs/reach.md §2.3), in the pure core:
//! one owner and two runners, with the shells around them played as kiln's reach node plays them.
//! The owner keeps its [`Kept`] rows and each runner its records on a disk that only a commit
//! writes, and a seed picks every step: an ask, a word delivered, said twice, or lost with its
//! link; a run's process ending; a crash of the owner or a runner at each durable point (before a
//! commit, after it and before what it gates is published, or after that), and its restart.
//!
//! In half the seeds a runner's runs are agent sessions, whose processes go on past its restart;
//! in a third, the second runner keeps no records on its disk (a runner from before), so what it
//! started can't be known after it restarts. Some runs declare themselves `restartable`.
//!
//! Checked: no run starts twice unless it declared itself `restartable`; each run's result is accepted once
//! (every answer that ends it, told first or told again after a restart, is the same one); and
//! once everything settles, each runner holds running exactly what runs there, and the owner holds
//! running at each runner what that runner holds for it. One planted mistake for each is caught.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use super::answer::Answer;
use super::ask::{Ask, Peer, Sample};
use super::keys::SplitMix;
use super::msg::{ToOwner, ToRunner};
use super::owner::{Act, Kept, Machine, Owner, Size};
use super::runner::{Admission, Init, Record, Records, Said, Settings, Start};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::sched::needs::Needs;
use crate::sched::state::{Class, Cpus, Resources};

const OWNER: &str = "owner";
const GIB: u64 = 1 << 30;

/// A mistake planted in the model's own shell, beside the core's [`Mutant`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plant {
    /// The owner's shell publishes what a change gates before the change is on its disk.
    PublishBeforeKept,
}

#[derive(Debug, Clone)]
enum Msg {
    Down { owner_epoch: u64, msg: ToRunner },
    Up { runner_epoch: u64, msg: ToOwner },
}

type Key = (String, u64, String);

struct Runner {
    name: String,
    epoch: u64,
    adm: Admission,
    /// Its records as its disk has them, and the ask each was started for.
    disk: BTreeMap<Key, (Record, Option<Arc<Ask>>)>,
    durable: bool,
    /// The processes running here.
    procs: BTreeSet<Key>,
    /// Its link to the owner, and what's on it each way, in order.
    up: bool,
    owner_away: bool,
    down_q: VecDeque<Msg>,
    up_q: VecDeque<Msg>,
}

struct World {
    rng: SplitMix,
    now: i64,
    owner: Option<Owner>,
    oepoch: u64,
    odisk: BTreeMap<String, Kept>,
    rs: Vec<Runner>,
    sessions: bool,
    plant: Option<Plant>,
    asked: BTreeMap<String, bool>,
    owned: BTreeSet<String>,
    starts: BTreeMap<String, u32>,
    results: BTreeMap<String, Answer>,
    fail: Option<String>,
    crashes: u32,
    next: u32,
    adopted: u32,
    /// Crashes and lost words happen (the nemesis), or not (settling).
    faults: bool,
}

fn linux() -> RunnerFacts {
    RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }
}

fn size() -> Resources {
    Resources { cpu: Cpus::whole(2), mem: 8 * GIB, disk: 500 * GIB, gpu: 0 }
}

fn admission(name: &str, epoch: u64, durable: bool, records: Records) -> Admission {
    let sample = Sample { size: size(), ..Sample::default() };
    let since = durable.then_some(1);
    Admission::new(Init { me: name.into(), epoch, since, facts: linux(), labels: Vec::new(), sample, records, settings: Settings::default() }, 0)
}

/// An answer that ends a run: its one result.
fn ends(a: &Answer) -> bool {
    matches!(a, Answer::Ended { .. } | Answer::Lost { .. } | Answer::Never { .. })
}

/// The same result, told again: by its kind and outcome (a run lost is lost whatever its words).
fn same(a: &Answer, b: &Answer) -> bool {
    match (a, b) {
        (Answer::Ended { runner: r1, ok: o1 }, Answer::Ended { runner: r2, ok: o2 }) => r1 == r2 && o1 == o2,
        (Answer::Lost { .. }, Answer::Lost { .. }) | (Answer::Never { .. }, Answer::Never { .. }) => true,
        _ => false,
    }
}

impl World {
    fn new(seed: u64, plant: Option<Plant>) -> World {
        let mut rng = SplitMix(seed);
        let sessions = rng.below(2) == 0;
        let forgetful = rng.below(3) == 0;
        let rs = (0..2u32)
            .map(|j| {
                let name = format!("r{j}");
                let durable = !(j == 1 && forgetful);
                Runner { adm: admission(&name, 1, durable, Records::new()), name, epoch: 1, disk: BTreeMap::new(), durable, procs: BTreeSet::new(), up: true, owner_away: false, down_q: VecDeque::new(), up_q: VecDeque::new() }
            })
            .collect();
        let mut w = World { rng, now: 1_000, owner: None, oepoch: 0, odisk: BTreeMap::new(), rs, sessions, plant, asked: BTreeMap::new(), owned: BTreeSet::new(), starts: BTreeMap::new(), results: BTreeMap::new(), fail: None, crashes: 0, next: 0, adopted: 0, faults: true };
        w.boot_owner();
        w
    }

    fn chance(&mut self, one_in: u32) -> bool {
        self.rng.below(one_in) == 0
    }

    fn crash_now(&mut self, one_in: u32) -> bool {
        self.faults && self.chance(one_in)
    }

    fn fail(&mut self, why: String) {
        if self.fail.is_none() {
            self.fail = Some(why);
        }
    }

    fn machines(&self) -> Vec<Machine> {
        self.rs.iter().map(|r| Machine { name: r.name.clone(), facts: linux(), size: Size::known(&size()), connected: r.up, places: true, syncs: true, ..Machine::default() }).collect()
    }

    /// The owner's process starts (again): a new epoch, what its disk kept restored, then the
    /// directory and the runs kept asked again.
    fn boot_owner(&mut self) {
        self.oepoch += 1;
        let mut o = Owner::seeded(OWNER, self.oepoch, self.oepoch);
        o.keeping(true);
        self.owner = Some(o);
        let kept: Vec<Kept> = self.odisk.values().cloned().collect();
        let ms = self.machines();
        let now = self.now;
        self.owner_call(true, |o| {
            let mut acts = o.restore(kept);
            acts.extend(o.machines(ms));
            acts.extend(o.reask(now));
            acts
        });
    }

    /// One input to the owner, as its shell takes it: its acts, its hard state's changes kept (a
    /// commit), then what they gate published, a crash possible at each point.
    fn owner_call(&mut self, may_crash: bool, f: impl FnOnce(&mut Owner) -> Vec<Act>) {
        let Some(o) = self.owner.as_mut() else { return };
        let acts = f(o);
        let kept = o.kept();
        if self.plant == Some(Plant::PublishBeforeKept) {
            self.publish(acts);
            if may_crash && self.crash_now(6) {
                return self.crash_owner();
            }
            return self.commit_owner(kept);
        }
        if may_crash && self.crash_now(40) {
            return self.crash_owner();
        }
        self.commit_owner(kept);
        if may_crash && self.crash_now(40) {
            return self.crash_owner();
        }
        self.publish(acts);
    }

    fn commit_owner(&mut self, kept: Vec<(Arc<str>, Option<Kept>)>) {
        for (run, k) in kept {
            match k {
                Some(k) => {
                    self.owned.insert(run.to_string());
                    self.odisk.insert(run.to_string(), k);
                }
                None => {
                    self.odisk.remove(run.as_ref());
                }
            }
        }
    }

    fn publish(&mut self, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Tell { runner, msg } => {
                    let e = self.oepoch;
                    if let Some(r) = self.rs.iter_mut().find(|r| r.name == runner).filter(|r| r.up) {
                        r.down_q.push_back(Msg::Down { owner_epoch: e, msg });
                    }
                }
                Act::Answer { run, answer } => {
                    if ends(&answer) {
                        match self.results.get(&run) {
                            Some(was) if !same(was, &answer) => self.fail(format!("{run}: two results, {was:?} and then {answer:?}")),
                            Some(_) => {}
                            None => {
                                self.results.insert(run.clone(), answer);
                            }
                        }
                        if let Some(o) = self.owner.as_mut() {
                            o.delivered(&run);
                        }
                    }
                }
                // Its tree copied, its start kept, then said.
                Act::Go { run, runner, fence } => {
                    let Some(o) = self.owner.as_mut() else { return };
                    let ok = o.starting(&run, &runner, fence);
                    let kept = o.kept();
                    if self.crash_now(30) {
                        return self.crash_owner();
                    }
                    self.commit_owner(kept);
                    if self.crash_now(30) {
                        return self.crash_owner();
                    }
                    let e = self.oepoch;
                    if ok && let Some(r) = self.rs.iter_mut().find(|r| r.name == runner).filter(|r| r.up) {
                        r.down_q.push_back(Msg::Down { owner_epoch: e, msg: ToRunner::Start { run, fence } });
                    }
                }
                Act::Restart { run, runner, .. } => self.fail(format!("{run}: its start said again to {runner}, which answers Sync")),
                Act::Fallback { run, .. } => self.fail(format!("{run}: a fallback, with no older runner")),
            }
        }
    }

    /// The owner's process ends: what it held in memory goes, its links with it (what it said that's
    /// on its way still arrives, as on a link the runner hadn't seen go yet).
    fn crash_owner(&mut self) {
        if self.owner.take().is_none() {
            return;
        }
        self.crashes += 1;
        let now = self.now;
        for j in 0..self.rs.len() {
            if let Some(r) = self.rs.get_mut(j) {
                r.up_q.clear();
                r.owner_away = true;
                let mut said = Vec::new();
                r.adm.away(OWNER, now, &mut said);
            }
        }
    }

    /// A runner's input: its words kept (records first, a crash possible before and after), then a
    /// process started if its admission said so, then its words said.
    fn runner_done(&mut self, j: usize, mut said: Vec<Said>, start: Option<(Peer<'_>, String)>) {
        let Some(r) = self.rs.get_mut(j) else { return };
        let changes = r.adm.record_changes();
        let crash = if self.faults { self.rng.below(50) } else { u32::MAX };
        if crash == 0 {
            return self.crash_runner(j);
        }
        let Some(r) = self.rs.get_mut(j) else { return };
        for (owner, epoch, run, rec, ask) in changes {
            let key = (owner, epoch, run);
            match rec {
                Some(rec) => {
                    let ask = ask.or_else(|| r.disk.get(&key).and_then(|(_, a)| a.clone()));
                    r.disk.insert(key, (rec, ask));
                }
                None => {
                    r.disk.remove(&key);
                }
            }
        }
        if crash == 1 {
            return self.crash_runner(j);
        }
        let Some(r) = self.rs.get_mut(j) else { return };
        if let Some((peer, run)) = start {
            let key = (peer.name.to_owned(), peer.epoch, run.clone());
            r.procs.insert(key);
            *self.starts.entry(run.clone()).or_default() += 1;
            r.adm.spawned(peer, &run, &mut said);
        }
        let e = r.epoch;
        if r.up {
            r.up_q.extend(said.into_iter().map(|(_, msg)| Msg::Up { runner_epoch: e, msg }));
        }
    }

    /// A runner's process ends: its sessions go on in half the seeds, its commands never; its link
    /// with it. It starts again at once, in a new epoch, from its disk: what goes on adopted, what
    /// didn't lost.
    fn crash_runner(&mut self, j: usize) {
        self.crashes += 1;
        let sessions = self.sessions;
        let now = self.now;
        let Some(r) = self.rs.get_mut(j) else { return };
        r.up = false;
        r.down_q.clear();
        r.up_q.clear();
        if !(sessions && r.durable) {
            r.procs.clear();
        }
        if !r.durable {
            r.disk.clear();
        }
        r.epoch += 1;
        let mut records = Records::new();
        for ((o, e, run), (rec, _)) in &r.disk {
            records.entry(o.clone()).or_default().entry(*e).or_default().insert(run.clone(), *rec);
        }
        r.adm = admission(&r.name, r.epoch, r.durable, records);
        let rows: Vec<(Key, Record, Option<Arc<Ask>>)> = r.disk.iter().map(|(k, (rec, a))| (k.clone(), *rec, a.clone())).collect();
        let mut n_adopted = 0u32;
        for ((o, e, run), rec, ask) in rows {
            if rec.ended.is_some() {
                continue;
            }
            let goes_on = r.procs.contains(&(o.clone(), e, run.clone()));
            let adopted = goes_on && ask.clone().is_some_and(|a| r.adm.adopt(Peer { name: &o, epoch: e }, a, now));
            if adopted {
                n_adopted += 1;
            } else {
                r.procs.remove(&(o.clone(), e, run.clone()));
                r.adm.lose(&o, e, &run);
            }
        }
        // Processes it has no record of can't be followed on: they end with it.
        let disk = &r.disk;
        r.procs.retain(|k| disk.contains_key(k));
        // Its ledger holds, as it starts, exactly what goes on here.
        let held: BTreeSet<String> = r.adm.probes().into_iter().filter(|v| v.state == "running").map(|v| v.run).collect();
        let runs: BTreeSet<String> = r.procs.iter().map(|(_, _, run)| run.clone()).collect();
        let name = r.name.clone();
        if held != runs {
            self.fail(format!("{name} holds {held:?} running as it starts again, and {runs:?} go on there"));
        }
        self.adopted += n_adopted;
        self.runner_done(j, Vec::new(), None);
        self.owner_call(false, |o| o.away(&name));
    }

    fn link_up(&mut self, j: usize) {
        let Some(r) = self.rs.get_mut(j) else { return };
        if r.up {
            return;
        }
        r.up = true;
        let name = r.name.clone();
        let e = r.epoch;
        self.owner_call(true, |o| {
            let mut acts = o.runner_epoch(&name, e);
            acts.extend(o.back(&name));
            acts
        });
    }

    fn link_down(&mut self, j: usize) {
        let now = self.now;
        let Some(r) = self.rs.get_mut(j) else { return };
        r.up = false;
        r.down_q.clear();
        r.up_q.clear();
        r.owner_away = true;
        let mut said = Vec::new();
        r.adm.away(OWNER, now, &mut said);
        let name = r.name.clone();
        self.owner_call(true, |o| o.away(&name));
    }

    fn at_runner(&mut self, j: usize, owner_epoch: u64, msg: ToRunner) {
        let now = self.now;
        let Some(r) = self.rs.get_mut(j) else { return };
        let peer = Peer { name: OWNER, epoch: owner_epoch };
        let mut said = Vec::new();
        if std::mem::take(&mut r.owner_away) {
            r.adm.back(peer, now, &mut said);
        }
        let mut start = None;
        match msg {
            ToRunner::Probe { ask } => r.adm.probe(peer, ask, None, now, &mut said),
            ToRunner::Withdraw { run } => r.adm.withdraw(peer, &run, now, &mut said),
            ToRunner::Take { run, fence } => r.adm.take(peer, &run, fence, now, &mut said),
            ToRunner::Decline { run, fence } => r.adm.decline(peer, &run, fence, now, &mut said),
            ToRunner::Start { run, fence } => {
                if r.adm.start(peer, &run, fence, now, &mut said) == Some(Start::New) {
                    start = Some(run);
                }
            }
            ToRunner::Ack { run } => r.adm.ack(peer, &run),
            ToRunner::Sync => r.adm.sync(peer, now, &mut said),
            // The kernel's starts: this model's runners don't name `place.fast`.
            ToRunner::Fast { .. } => {}
        }
        let start = start.map(|run| (Peer { name: OWNER, epoch: owner_epoch }, run));
        self.runner_done(j, said, start);
    }

    fn at_owner(&mut self, j: usize, runner_epoch: u64, msg: ToOwner) {
        let now = self.now;
        let Some(name) = self.rs.get(j).map(|r| r.name.clone()) else { return };
        self.owner_call(true, |o| {
            let mut acts = o.runner_epoch(&name, runner_epoch);
            acts.extend(o.heard(&name, msg, now));
            acts
        });
    }

    /// One word off a link with any, in order: said twice now and then, and now and then lost with
    /// its link (`faults`).
    fn deliver(&mut self, faults: bool) -> bool {
        let busy: Vec<(usize, bool)> = self.rs.iter().enumerate().flat_map(|(j, r)| [(j, true), (j, false)].into_iter().filter(move |(_, d)| if *d { !r.down_q.is_empty() } else { !r.up_q.is_empty() })).collect();
        if busy.is_empty() {
            return false;
        }
        let pick = self.rng.below(u32::try_from(busy.len()).unwrap_or(1));
        let Some(&(j, down)) = busy.get(pick as usize) else { return false };
        if faults && self.chance(60) {
            self.link_down(j);
            return true;
        }
        let twice = faults && self.chance(15);
        let Some(r) = self.rs.get_mut(j) else { return false };
        let q = if down { &mut r.down_q } else { &mut r.up_q };
        let Some(m) = q.pop_front() else { return false };
        if twice {
            q.push_front(m.clone());
        }
        match m {
            Msg::Down { owner_epoch, msg } => self.at_runner(j, owner_epoch, msg),
            Msg::Up { runner_epoch, msg } if self.owner.is_some() => self.at_owner(j, runner_epoch, msg),
            Msg::Up { .. } => {}
        }
        true
    }

    fn end_one(&mut self) {
        let procs: Vec<(usize, Key)> = self.rs.iter().enumerate().flat_map(|(j, r)| r.procs.iter().map(move |k| (j, k.clone()))).collect();
        if procs.is_empty() {
            return;
        }
        let pick = self.rng.below(u32::try_from(procs.len()).unwrap_or(1));
        let Some((j, (o, e, run))) = procs.get(pick as usize).cloned() else { return };
        let now = self.now;
        let Some(r) = self.rs.get_mut(j) else { return };
        r.procs.remove(&(o.clone(), e, run.clone()));
        let mut said = Vec::new();
        r.adm.ended(Peer { name: &o, epoch: e }, &run, true, now, &mut said);
        self.runner_done(j, said, None);
    }

    fn ask(&mut self) {
        self.next += 1;
        let run = format!("run-{}", self.next);
        let repeat = self.chance(4);
        let needs = Needs { request: Resources { cpu: Cpus::whole(1), mem: GIB, disk: 0, gpu: 0 }, ..Needs::default() };
        let ask = Ask { run: run.clone(), needs, class: Class::Interactive, repo: "repo".into(), actor: "agent".into(), restartable: repeat, ..Ask::default() };
        self.asked.insert(run, repeat);
        let now = self.now;
        self.owner_call(true, |o| o.ask(ask, now));
    }

    fn step(&mut self, faults: bool) {
        self.now += i64::from(self.rng.below(50));
        if self.owner.is_none() {
            if self.chance(3) {
                self.boot_owner();
            }
            return;
        }
        match self.rng.below(20) {
            0..=2 => self.ask(),
            3..=4 => self.end_one(),
            5 if faults => {
                let j = self.rng.below(2) as usize;
                self.crash_runner(j);
            }
            6 if faults && self.chance(3) => self.crash_owner(),
            7 => {
                let j = self.rng.below(2) as usize;
                self.link_up(j);
            }
            8 => {
                let now = self.now;
                self.owner_call(true, |o| o.tick(now));
                for r in &mut self.rs {
                    let mut said = Vec::new();
                    r.adm.tick(now, &mut said);
                    let e = r.epoch;
                    if r.up {
                        r.up_q.extend(said.into_iter().map(|(_, msg)| Msg::Up { runner_epoch: e, msg }));
                    }
                }
            }
            _ => {
                self.deliver(faults);
            }
        }
    }

    /// Everything said is heard, the links up, the owner running, and time passes enough for every
    /// offer and first answer: no fault meanwhile.
    fn settle(&mut self) {
        self.faults = false;
        for _ in 0..10_000 {
            if self.owner.is_none() {
                self.boot_owner();
                continue;
            }
            for j in 0..self.rs.len() {
                self.link_up(j);
            }
            if self.deliver(false) {
                continue;
            }
            // Nothing said: the next timer anyone set, if any.
            let due = self.owner.as_ref().and_then(Owner::timer).into_iter().chain(self.rs.iter().filter_map(|r| r.adm.timer())).min();
            let Some(at) = due else { return };
            self.now = self.now.max(at);
            let now = self.now;
            self.owner_call(false, |o| o.tick(now));
            for r in &mut self.rs {
                let mut said = Vec::new();
                r.adm.tick(now, &mut said);
                let e = r.epoch;
                r.up_q.extend(said.into_iter().map(|(_, msg)| Msg::Up { runner_epoch: e, msg }));
            }
        }
        let qs: Vec<String> = self.rs.iter().map(|r| format!("{}: down {:?} up {:?} timer {:?}", r.name, r.down_q.front(), r.up_q.front(), r.adm.timer())).collect();
        let why = format!("it never settled: now {}, owner timer {:?}, {qs:?}", self.now, self.owner.as_ref().and_then(Owner::timer));
        self.fail(why);
    }

    /// Each runner holds running exactly what runs there; the owner holds running at each runner
    /// what that runner holds for it.
    fn charges_agree(&mut self) {
        let Some(o) = self.owner.as_ref() else { return };
        let mut why = None;
        for r in &self.rs {
            let held: BTreeSet<String> = r.adm.probes().into_iter().filter(|v| v.state == "running").map(|v| v.run).collect();
            let runs: BTreeSet<String> = r.procs.iter().map(|(_, _, run)| run.clone()).collect();
            let owner_has: BTreeSet<String> = o.runs().into_iter().filter(|run| o.stage(run).is_some_and(|(s, at)| s == "running" && at == Some(r.name.as_str()))).collect();
            if held != runs {
                why = Some(format!("{} holds {held:?} running, and {runs:?} run there", r.name));
            } else if owner_has != runs {
                why = Some(format!("the owner holds {owner_has:?} running at {}, which runs {runs:?}", r.name));
            }
        }
        if let Some(w) = why {
            self.fail(w);
        }
    }

    fn run(&mut self) -> Result<u32, String> {
        for _ in 0..600 {
            self.step(true);
            if self.fail.is_some() {
                break;
            }
        }
        self.settle();
        self.charges_agree();
        // Every process ends; everything settles again; each run kept is over, with one result.
        for _ in 0..200 {
            if self.rs.iter().all(|r| r.procs.is_empty()) {
                break;
            }
            self.end_one();
            self.settle();
        }
        // Every process ended: nothing is held anywhere, and every run kept is over.
        self.charges_agree();
        if let Some(run) = self.owner.as_ref().and_then(|o| o.runs().into_iter().next()) {
            let why = format!("{run} is still the owner's after every process ended: {:?}", self.owner.as_ref().and_then(|o| o.stage(&run)));
            self.fail(why);
        }
        let twice = self.starts.iter().find(|(run, n)| **n > 1 && !self.asked.get(*run).copied().unwrap_or(false)).map(|(run, n)| format!("{run} started {n} times"));
        if let Some(w) = twice {
            self.fail(w);
        }
        let left: Vec<String> = self.owned.iter().filter(|r| !self.results.contains_key(*r)).cloned().collect();
        if let Some(o) = self.owner.as_ref()
            && let Some(run) = left.iter().find(|r| o.stage(r).is_none())
        {
            let why = format!("{run} was kept and is over without a result");
            self.fail(why);
        }
        match self.fail.take() {
            Some(f) => Err(f),
            None => Ok(self.crashes + 1_000_000 * self.adopted),
        }
    }
}

fn sweep(seeds: std::ops::Range<u64>, plant: Option<Plant>, mutant: Option<Mutant>) -> Result<u32, String> {
    let mut crashes = 0;
    for seed in seeds {
        let r = planted(mutant, || World::new(seed, plant).run());
        crashes += r.map_err(|e| format!("seed {seed}: {e}"))?;
    }
    Ok(crashes)
}

#[test]
fn lb03_restarts_at_every_durable_point_keep_one_start_one_result_and_agreeing_charges() -> Result<(), String> {
    let n = sweep(0..300, None, None)?;
    let (adopted, crashes) = (n / 1_000_000, n % 1_000_000);
    // Crashes came at every kind of point the seeds pick, many times over, and runs went on past
    // their runner's restarts.
    if crashes < 300 || adopted == 0 {
        return Err(format!("{crashes} crashes and {adopted} runs adopted over 300 seeds"));
    }
    Ok(())
}

#[test]
fn lb03_a_restarted_owner_that_places_an_unknown_run_again_starts_it_twice() {
    let r = sweep(0..300, None, Some(Mutant::StartAgainOnUnknown));
    assert!(r.as_ref().is_err_and(|e| e.contains("started 2 times") || e.contains("started 3 times")), "{r:?}");
}

#[test]
fn lb03_publishing_before_the_durable_point_tells_two_results() {
    let r = sweep(0..300, Some(Plant::PublishBeforeKept), None);
    assert!(r.as_ref().is_err_and(|e| e.contains("two results") || e.contains("started")), "{r:?}");
}

#[test]
fn lb03_a_restarted_runner_that_forgets_what_goes_on_holds_less_than_runs() {
    let r = sweep(0..300, None, Some(Mutant::ForgetAdopted));
    assert!(r.as_ref().is_err_and(|e| e.contains("holds")), "{r:?}");
}

/// An end from a runner the run's start didn't go to is never its result.
#[test]
fn lb03_an_end_from_elsewhere_is_never_taken_for_the_result() {
    let check = || {
        let mut o = Owner::new(OWNER, 1);
        o.keeping(true);
        let ms: Vec<Machine> = ["a", "b"].iter().map(|n| Machine { name: (*n).into(), facts: linux(), size: Size::known(&size()), connected: true, places: true, syncs: true, ..Machine::default() }).collect();
        let _ = o.machines(ms);
        let ask = Ask { run: "x".into(), needs: Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() }, ..Ask::default() };
        let acts = o.ask(ask, 0);
        let at: Vec<String> = acts.iter().filter_map(|a| if let Act::Tell { runner, .. } = a { Some(runner.clone()) } else { None }).collect();
        let (Some(here), Some(there)) = (at.first().cloned(), ["a", "b"].iter().map(|s| (*s).to_owned()).find(|n| !at.first().is_some_and(|f| f == n))) else { return 0 };
        let fence = crate::sched::state::Fence { epoch: 1, n: 1 };
        let _ = o.heard(&here, ToOwner::Offer { run: "x".into(), fence, got: Resources::default(), free: size(), predicted: None, disk: Default::default() }, 0);
        let _ = o.tick(60_000);
        let _ = o.heard(&here, ToOwner::Held { run: "x".into(), fence }, 60_000);
        let _ = o.starting("x", &here, fence);
        let _ = o.heard(&here, ToOwner::Started { run: "x".into(), fence }, 60_000);
        let stale = o.heard(&there, ToOwner::Ended { run: "x".into(), fence: crate::sched::state::Fence { epoch: 7, n: 3 }, ok: false }, 60_000);
        let theirs = o.heard(&here, ToOwner::Ended { run: "x".into(), fence, ok: true }, 60_000);
        stale.iter().chain(theirs.iter()).filter(|a| matches!(a, Act::Answer { answer, .. } if ends(answer))).count()
    };
    assert_eq!(check(), 1, "one result, its runner's");
    assert_eq!(planted(Some(Mutant::AcceptAnyEnd), check), 1, "a stale end taken ends the ticket before its own result");
    let stale_taken = planted(Some(Mutant::AcceptAnyEnd), || {
        let mut o = Owner::new(OWNER, 1);
        let _ = o.machines(vec![Machine { name: "a".into(), facts: linux(), size: Size::known(&size()), connected: true, places: true, syncs: true, ..Machine::default() }]);
        let ask = Ask { run: "x".into(), needs: Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() }, ..Ask::default() };
        let _ = o.ask(ask, 0);
        o.heard("b", ToOwner::Ended { run: "x".into(), fence: crate::sched::state::Fence { epoch: 7, n: 3 }, ok: false }, 0).iter().any(|a| matches!(a, Act::Answer { answer, .. } if ends(answer)))
    });
    assert!(stale_taken, "planted: an end from a runner never probed is taken as the result");
}

/// Runs `run` of `from` on `a`: probed, offered, taken, started, its process up.
fn started_on(a: &mut Admission, from: Peer<'_>, run: &str, now: i64) -> Result<(), String> {
    let mut said = Vec::new();
    let needs = Needs { request: Resources { cpu: Cpus::whole(1), mem: GIB, disk: 0, gpu: 0 }, ..Needs::default() };
    a.probe(from, Ask { run: run.into(), needs, ..Ask::default() }, None, now, &mut said);
    let fence = said.iter().find_map(|(_, m)| if let ToOwner::Offer { fence, .. } = m { Some(*fence) } else { None }).ok_or_else(|| format!("no offer: {said:?}"))?;
    a.take(from, run, fence, now, &mut said);
    if a.start(from, run, fence, now, &mut said) != Some(Start::New) {
        return Err(format!("not started: {said:?}"));
    }
    a.spawned(from, run, &mut said);
    Ok(())
}

/// An owner from before `Sync` started again and never asks after its earlier runs: the records of
/// those that ended go once it's gone from the directory (its forget), on that event alone, and one
/// still running goes as it ends.
#[test]
fn lb03_a_forgotten_owners_ended_records_go_on_its_forget() -> Result<(), String> {
    let mut a = admission("r", 1, true, Records::new());
    let (o1, o2) = (Peer { name: OWNER, epoch: 1 }, Peer { name: OWNER, epoch: 2 });
    started_on(&mut a, o1, "done", 0)?;
    started_on(&mut a, o1, "going", 0)?;
    let mut said = Vec::new();
    a.ended(o1, "done", true, 10, &mut said);
    // Its next process speaks, and never says Sync: the records stay, as nothing said they may go.
    started_on(&mut a, o2, "later", 20)?;
    let _ = a.record_changes();
    if a.records().get(OWNER).is_none_or(|m| !m.get(&1).is_some_and(|runs| runs.contains_key("done"))) {
        return Err(format!("kept until its owner can't ask: {:?}", a.records()));
    }
    a.forget(OWNER, 30, &mut said);
    let changes = a.record_changes();
    if !changes.iter().any(|(o, e, run, rec, _)| o == OWNER && *e == 1 && run == "done" && rec.is_none()) {
        return Err(format!("its ended record goes, on disk too: {changes:?}"));
    }
    if a.records().get(OWNER).is_some_and(|m| m.values().any(|runs| runs.values().any(|r| r.ended.is_some()))) {
        return Err(format!("{:?}", a.records()));
    }
    // A run of its still running goes as it ends.
    a.ended(o1, "going", true, 40, &mut said);
    if a.records().get(OWNER).is_some_and(|m| m.get(&1).is_some_and(|runs| runs.contains_key("going"))) {
        return Err(format!("let go as it ended: {:?}", a.records()));
    }
    // Should it ever ask, no claim of before its forget is vouched for: it's lost, never placed again.
    let mut said = Vec::new();
    a.sync(Peer { name: OWNER, epoch: 3 }, 50, &mut said);
    if !matches!(said.last(), Some((_, ToOwner::Synced { since: Some(2), .. }))) {
        return Err(format!("vouches only past its forget: {said:?}"));
    }
    if a.forgot_changes() != vec![(OWNER.to_owned(), 1)] {
        return Err("its forget is kept on disk".into());
    }
    Ok(())
}
