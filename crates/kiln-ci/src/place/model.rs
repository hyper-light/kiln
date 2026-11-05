//! Placement's model under kiln-sim's nemesis (docs/sim.md; docs/reach.md §2; docs/needs.md E5):
//! owners and runners running [`super::owner::Owner`] and [`super::runner::Admission`], their words
//! over kiln-sim's network, with the shells around them played as kiln's reach node plays them:
//!
//! - Each owner and runner talk over a link of their own, an ordered lane that a lost frame breaks;
//!   each end hears it went a while later, and they link again once nothing cuts them off. A link
//!   down is not a runner gone: the directory still shows the runner, disconnected.
//! - Each runner's clock is hours off its owners' (its offset measured on the link, RFC 5905).
//! - A held run's tree is copied (a while), then its start is said if the owner still may. A runner
//!   starts a process only on [`Start::New`], keeping the run in its record on disk first.
//! - Some runners are older ones, without placement: a run goes to one by the shell's own pick.
//! - Some runners' disks aren't measured in the directory; some runs take a runner whole; a
//!   restarted owner asks its runs again under the same ids, in its new epoch.
//!
//! The nemesis, until it stops: frames lost (breaking their link), late, and said twice; links
//! cut; the relay lost for a while (every link cut, the directory still); and any one node killed
//! at a time the seed picks, in half the seeds started again.
//!
//! The checker, at every step: no run starts twice; a runner starts a run only under its live
//! claim; no runner's claims pass its budget (G1) or an offer its room; an asker is told a run
//! started only once its process has; a run is answered lost, or ended unstarted, only once its
//! runner is gone or started again; never only when no runner that's there could take it; no
//! estimate is told in another clock than the asker's. Once the nemesis stops and everything
//! settles: every run its live owner was asked ended, or was answered never or lost, but one only
//! runners gone for good could take; no claim is held for anything.
//!
//! The simulation's own numbers (how long the nemesis lasts, how far clocks are apart, how long a
//! link takes to be noticed down) are the test's inputs, not numbers placement acts on.
//!
//! [`super::Mutant`]s plant, one at a time, the bugs the checks are there for.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kiln_sim::net::{Fate, Node};
use kiln_sim::{Faults, Net, Queue, Rng, Trace, seeds};

use super::answer::Answer;
use super::ask::{Ask, Peer, RunUse, Sample};
use super::link::{Offset, Rtt};
use super::msg::{ToOwner, ToRunner};
use super::owner::{Act, Link, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Said, Settings, Start};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::sched::needs::Needs;
use crate::sched::state::{Cpus, Fence, Resources};

/// Longest a link takes to be noticed down at either end.
const NOTICE: Duration = Duration::from_secs(3);
/// How long a runner keeps what an owner gone left (its lease, as the shell keeps it).
const LINGER: Duration = Duration::from_secs(12);
/// The nemesis lasts this long; then the model runs until everything settles, at most `SETTLE`.
const NEMESIS: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_secs(4 * 60 * 60);
const STEPS_MAX: u64 = 2_000_000;
const GIB: u64 = 1 << 30;
const HOUR_MS: i64 = 3_600_000;

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn at_ms(t: i64) -> Duration {
    Duration::from_millis(u64::try_from(t.max(0)).unwrap_or(0))
}

/// What crosses a link.
#[derive(Debug, Clone)]
enum Msg {
    Down(ToRunner),
    Up(ToOwner),
}

#[derive(Debug)]
enum Ev {
    Deliver { owner: usize, runner: usize, link: u64, msg: Box<Msg> },
    /// One end hears its link went (`at_owner`: the owner's end).
    Down { owner: usize, runner: usize, link: u64, at_owner: bool },
    /// The two try to link again.
    Up { owner: usize, runner: usize },
    Ask { owner: usize, n: usize },
    /// A held run's tree copied: its start said, if its owner still may.
    Synced { owner: usize, life: u64, run: String, runner: usize, fence: Fence },
    /// A placed run's process ends.
    End { runner: usize, life: u64, owner: usize, olife: u64, run: String },
    /// A run on an older runner ends.
    OldEnd { runner: usize, life: u64, owner: usize, olife: u64, run: String },
    Load { runner: usize },
    Tick { owner: Option<usize>, runner: Option<usize> },
    Directory,
    Crash { owner: Option<usize>, runner: Option<usize> },
    Restart { owner: Option<usize>, runner: Option<usize> },
    Forget { runner: usize, owner: usize, life: u64 },
    RelayLost { till: Duration },
    RelayBack,
}

#[derive(Debug, Clone, Copy, Default)]
struct Wire {
    n: u64,
    up: bool,
}

/// A run an owner's process was asked, as its asker sees it.
#[derive(Debug, Clone)]
struct Asked {
    ask: Ask,
    first: Option<Answer>,
    last: Option<Answer>,
    end: Option<&'static str>,
    /// The runners that could take it, by what they are.
    could: Vec<usize>,
    /// Where its start went, and that runner's life then.
    started_to: Option<(usize, u64)>,
    /// Where its process started, that runner's life then, and whether it ran to its end.
    spawned: Option<(usize, u64)>,
    exited: bool,
}

struct OwnerNode {
    alive: bool,
    life: u64,
    core: Owner,
    died: Option<Duration>,
    /// Runs it said a start for, when.
    starts: BTreeMap<String, Duration>,
    next_tick: Option<i64>,
}

struct RunnerNode {
    alive: bool,
    life: u64,
    places: bool,
    core: Option<Admission>,
    size: Resources,
    /// The directory knows its disk's size.
    disk_known: bool,
    /// Other work's use: CPUs (milli) and memory.
    other: (u64, u64),
    /// Its clock less the owners' (negative: behind them).
    theta: i64,
    /// Processes running: (owner, owner's life, run) to what they use.
    procs: BTreeMap<(usize, u64, String), Resources>,
    /// On disk: the runs it started, and its last epoch.
    records: Records,
    epoch: u64,
    next_tick: Option<i64>,
}

/// What a run counted: across seeds, a fault that never happened tested nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub asked: u64,
    pub started: u64,
    pub ended: u64,
    pub never: u64,
    pub lost: u64,
    pub queued_first: u64,
    pub news: u64,
    pub offers: u64,
    pub declined: u64,
    pub lapsed: u64,
    pub breaks: u64,
    pub crashes: u64,
    pub restarts: u64,
    pub relay_lost: u64,
    pub doubled: u64,
    pub excused: u64,
    pub fallbacks: u64,
    pub older_started: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Self) {
        let pairs = [
            (&mut self.asked, o.asked),
            (&mut self.started, o.started),
            (&mut self.ended, o.ended),
            (&mut self.never, o.never),
            (&mut self.lost, o.lost),
            (&mut self.queued_first, o.queued_first),
            (&mut self.news, o.news),
            (&mut self.offers, o.offers),
            (&mut self.declined, o.declined),
            (&mut self.lapsed, o.lapsed),
            (&mut self.breaks, o.breaks),
            (&mut self.crashes, o.crashes),
            (&mut self.restarts, o.restarts),
            (&mut self.relay_lost, o.relay_lost),
            (&mut self.doubled, o.doubled),
            (&mut self.excused, o.excused),
            (&mut self.fallbacks, o.fallbacks),
            (&mut self.older_started, o.older_started),
        ];
        for (a, b) in pairs {
            *a = a.saturating_add(b);
        }
    }
}

struct Sim {
    rng: Rng,
    net: Net,
    q: Queue<Ev>,
    now: Duration,
    trace: Trace,
    owners: Vec<OwnerNode>,
    runners: Vec<RunnerNode>,
    wires: BTreeMap<(usize, usize), Wire>,
    /// Every run asked, by (owner, its life, run).
    asked: BTreeMap<(usize, u64, String), Asked>,
    /// Each owner's runs to ask: (when, ask), and each run's predicted time (the runners' shells'
    /// prediction from history; `None`: none).
    plans: Vec<Vec<(Duration, Ask)>>,
    predicted: BTreeMap<String, Option<u64>>,
    /// Processes started, by (owner, life, run): never more than one.
    spawns: BTreeMap<(usize, u64, String), u32>,
    /// Each runner's claims offered so far: one offered again as it stands was checked when made.
    offered: BTreeSet<(usize, Fence)>,
    /// The runners the directory last showed owners.
    shown: Vec<bool>,
    /// Each link's round trip as the shells measure it: the longest a frame takes, there and back.
    rtt: Rtt,
    counts: Counts,
    failed: Option<String>,
}

fn rname(j: usize) -> String {
    format!("r{j}")
}

fn oname(i: usize) -> String {
    format!("o{i}")
}

fn runner_of(name: &str) -> Option<usize> {
    name.strip_prefix('r').and_then(|x| x.parse().ok())
}

fn owner_of(name: &str) -> Option<usize> {
    name.strip_prefix('o').and_then(|x| x.parse().ok())
}

fn linux() -> RunnerFacts {
    RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }
}

impl Sim {
    fn new(seed: u64, keep: usize) -> Sim {
        let rng = Rng::new(seed);
        let mut draw = rng.stream("fleet");
        let faults = Faults::draw(&mut rng.stream("faults"), Duration::from_millis(2), Duration::from_millis(400), NEMESIS);
        let slow = ms(faults.slow.max(faults.fast));
        let net = Net::new(faults, rng.stream("net"));
        let n_runners = usize::try_from(draw.range(2, 5)).unwrap_or(2);
        let n_owners = usize::try_from(draw.range(1, 3)).unwrap_or(1);
        // In a third of the seeds, one runner is an older one, without placement.
        let older = draw.chance(333).then(|| usize::try_from(draw.below(u64::try_from(n_runners).unwrap_or(1))).unwrap_or(0));
        let mut runners = Vec::new();
        for j in 0..n_runners {
            let cpus = *draw.pick(&[8u64, 16, 32]).unwrap_or(&8);
            let mem = *draw.pick(&[16u64, 32, 64]).unwrap_or(&16);
            let hours = i64::try_from(draw.range(1, 10)).unwrap_or(1);
            runners.push(RunnerNode {
                alive: true,
                life: 1,
                places: older != Some(j),
                core: None,
                size: Resources { cpu: Cpus::whole(cpus), mem: mem * GIB, disk: 500 * GIB, gpu: 0 },
                disk_known: draw.chance(600),
                other: (draw.below(cpus * 500), draw.below(mem * GIB / 2)),
                theta: -hours.saturating_mul(HOUR_MS),
                procs: BTreeMap::new(),
                records: Records::new(),
                epoch: 0,
                next_tick: None,
            });
        }
        let mut plans = Vec::new();
        let mut predicted = BTreeMap::new();
        for i in 0..n_owners {
            let mut p = Vec::new();
            for n in 0..draw.range(3, 8) {
                let cpus = *draw.pick(&[0u64, 1, 2, 4, 8, 12]).unwrap_or(&1);
                let mem = *draw.pick(&[0u64, 1, 2, 4, 8, 16]).unwrap_or(&1);
                let disk = *draw.pick(&[0u64, 0, 10, 100]).unwrap_or(&0);
                let mut needs = Needs { request: Resources { cpu: Cpus::whole(cpus), mem: mem * GIB, disk: disk * GIB, gpu: 0 }, ..Needs::default() };
                // Now and then one no runner could take, one on a platform none is, or one that
                // takes its runner whole.
                if draw.chance(60) {
                    needs.request.cpu = Cpus::whole(64);
                } else if draw.chance(40) {
                    needs.requires.push("os == macos".into());
                } else if draw.chance(100) {
                    needs.exclusive = true;
                }
                let median = draw.range(500, 30_000);
                let run = format!("o{i}-n{n}");
                predicted.insert(run.clone(), draw.chance(667).then_some(median));
                let ask = Ask { run, needs, repo: format!("repo{}", draw.below(2)), actor: format!("agent{i}"), median: Some(median), p90: Some(median.saturating_mul(3) / 2), ..Ask::default() };
                let at = Duration::from_millis(draw.below(u64::try_from(NEMESIS.as_millis()).unwrap_or(60_000)));
                p.push((at, ask));
            }
            plans.push(p);
        }
        let owners = (0..n_owners).map(|i| OwnerNode { alive: true, life: 1, core: Owner::seeded(&oname(i), 1, draw.next_u64()), died: None, starts: BTreeMap::new(), next_tick: None }).collect();
        let rtt = Rtt { srtt: u64::try_from(slow).unwrap_or(0), rttvar: u64::try_from(slow / 2).unwrap_or(0), granularity: 1 };
        let mut s = Sim {
            rng,
            net,
            q: Queue::new(),
            now: Duration::ZERO,
            trace: Trace::new(seed, keep),
            owners,
            runners,
            wires: BTreeMap::new(),
            asked: BTreeMap::new(),
            plans,
            predicted,
            spawns: BTreeMap::new(),
            offered: BTreeSet::new(),
            shown: Vec::new(),
            rtt,
            counts: Counts::default(),
            failed: None,
        };
        for j in 0..n_runners {
            s.boot_runner(j);
        }
        s.directory();
        for (i, plan) in s.plans.clone().iter().enumerate() {
            for (n, (at, _)) in plan.iter().enumerate() {
                s.q.push(*at, Ev::Ask { owner: i, n });
            }
        }
        for i in 0..n_owners {
            for j in 0..n_runners {
                s.q.push(Duration::ZERO, Ev::Up { owner: i, runner: j });
            }
        }
        for j in 0..n_runners {
            let at = s.rng.span(Duration::from_secs(1), Duration::from_secs(20));
            s.q.push(at, Ev::Load { runner: j });
            s.q.push(NEMESIS, Ev::Load { runner: j });
        }
        let mut chaos = s.rng.stream("chaos");
        let pick = usize::try_from(chaos.below(u64::try_from(n_owners + n_runners).unwrap_or(1))).unwrap_or(0);
        let at = chaos.span(Duration::from_secs(1), NEMESIS);
        let (owner, runner) = if pick < n_owners { (Some(pick), None) } else { (None, Some(pick - n_owners)) };
        s.q.push(at, Ev::Crash { owner, runner });
        if chaos.chance(500) {
            let back = at.saturating_add(chaos.span(Duration::from_millis(100), Duration::from_secs(30)));
            s.q.push(back, Ev::Restart { owner, runner });
        }
        if chaos.chance(333) {
            let from = chaos.span(Duration::ZERO, NEMESIS);
            let till = from.saturating_add(chaos.span(Duration::from_millis(500), Duration::from_secs(15)));
            s.q.push(from, Ev::RelayLost { till });
        }
        s
    }

    fn onode(&self, i: usize) -> Node {
        Node(u32::try_from(i).unwrap_or(0))
    }

    fn rnode(&self, j: usize) -> Node {
        Node(u32::try_from(self.owners.len() + j).unwrap_or(0))
    }

    fn fail(&mut self, why: String) {
        if self.failed.is_none() {
            self.failed = Some(format!("{:?}: {why}", self.now));
        }
    }

    fn note(&mut self, what: impl FnOnce() -> String) {
        let t = self.now;
        self.trace.note(|| format!("{t:?} {}", what()));
    }

    /// Runner `j`'s clock now.
    fn rnow(&self, j: usize) -> i64 {
        ms(self.now).saturating_add(self.runners.get(j).map_or(0, |r| r.theta))
    }

    fn life(&self, i: usize) -> u64 {
        self.owners.get(i).map_or(0, |o| o.life)
    }

    /// A runner's sample: its runs using what they asked.
    fn sample(&self, j: usize) -> Sample {
        let Some(r) = self.runners.get(j) else { return Sample::default() };
        let used = r.procs.values().fold(Resources::default(), |a, u| super::ask::resources(&super::ask::plus(&super::ask::amounts(&a), &super::ask::amounts(u))));
        Sample {
            size: r.size,
            used: Resources { cpu: Cpus::milli(r.other.0).saturating_add(used.cpu), mem: r.other.1.saturating_add(used.mem), disk: used.disk, gpu: 0 },
            ci_jobs: 0,
            runs: r.procs.iter().map(|((i, life, run), u)| RunUse { owner: oname(*i), epoch: *life, run: run.clone(), used: *u }).collect(),
            disk: super::ask::DiskFacts::default(),
        }
    }

    fn boot_runner(&mut self, j: usize) {
        let sample = self.sample(j);
        let now = self.rnow(j);
        let Some(r) = self.runners.get_mut(j) else { return };
        r.epoch = r.epoch.saturating_add(1);
        // What ran when it went down ended with it.
        for rec in r.records.values_mut().flat_map(|m| m.values_mut()).flat_map(|m| m.values_mut()) {
            if rec.ended.is_none() {
                rec.ended = Some(false);
            }
        }
        if !r.places {
            return;
        }
        let settings = Settings { budget: None, overcommit_cpu: 1000, backfill: true, weights: BTreeMap::new(), ..Settings::default() };
        let init = Init { me: rname(j), epoch: r.epoch, facts: linux(), labels: Vec::new(), sample, records: r.records.clone(), settings };
        r.core = Some(Admission::new(init, now));
    }

    /// Runner `j` as owner `i`'s directory shows it, if it's there.
    fn machine(&self, i: usize, j: usize) -> Option<Machine> {
        let r = self.runners.get(j).filter(|r| r.alive)?;
        let mut facts = linux();
        facts.cpu.threads = u32::try_from(r.size.cpu.ceil()).unwrap_or(0);
        facts.mem.total = r.size.mem;
        let mut size = Size::known(&r.size);
        if !r.disk_known {
            size.disk = None;
        }
        let used = r.procs.values().fold([0u64; super::ask::DIMS], |a, u| super::ask::plus(&a, &super::ask::amounts(u)));
        let idle = (!r.places).then(|| super::ask::resources(&super::ask::less(&super::ask::less(&super::ask::amounts(&r.size), &[r.other.0, r.other.1, 0, 0]), &used)));
        let connected = self.wires.get(&(i, j)).is_some_and(|w| w.up) || !r.places;
        Some(Machine { name: rname(j), facts, labels: Vec::new(), size, epoch: r.epoch, connected, places: r.places, idle })
    }

    fn directory(&mut self) {
        self.shown = self.runners.iter().map(|r| r.alive).collect();
        for i in 0..self.owners.len() {
            if !self.owners.get(i).is_some_and(|o| o.alive) {
                continue;
            }
            let list: Vec<Machine> = (0..self.runners.len()).filter_map(|j| self.machine(i, j)).collect();
            let acts = match self.owners.get_mut(i) {
                Some(o) => o.core.machines(list),
                None => continue,
            };
            self.owner_acts(i, acts);
        }
    }

    /// Sends `msg` on the link between owner `i` and runner `j`, if it's up: its fate is the
    /// network's; one lost breaks the link; now and then a shell says it twice.
    fn send(&mut self, i: usize, j: usize, msg: Msg) {
        let w = self.wires.get(&(i, j)).copied().unwrap_or_default();
        if !w.up {
            return;
        }
        let to_runner = matches!(msg, Msg::Down(_));
        let (from, to) = if to_runner { (self.onode(i), self.rnode(j)) } else { (self.rnode(j), self.onode(i)) };
        let twice = self.net.faulty(self.now) && self.rng.chance(self.net.faults().dup);
        for k in 0..if twice { 2 } else { 1 } {
            if k == 1 {
                self.counts.doubled += 1;
            }
            match self.net.send_in_order(self.now, from, to, u32::try_from(w.n).unwrap_or(0)) {
                Fate::Lost => {
                    self.break_link(i, j);
                    return;
                }
                fate => {
                    for at in fate.arrivals() {
                        self.q.push(at, Ev::Deliver { owner: i, runner: j, link: w.n, msg: Box::new(msg.clone()) });
                    }
                }
            }
        }
    }

    /// The link between owner `i` and runner `j` breaks: each end hears it within a while, and
    /// they try to link again after.
    fn break_link(&mut self, i: usize, j: usize) {
        let n = {
            let w = self.wires.entry((i, j)).or_default();
            if !w.up {
                return;
            }
            w.up = false;
            w.n
        };
        self.counts.breaks += 1;
        let (a, b) = (self.rng.span(Duration::from_millis(1), NOTICE), self.rng.span(Duration::from_millis(1), NOTICE));
        self.q.push(self.now.saturating_add(a), Ev::Down { owner: i, runner: j, link: n, at_owner: true });
        self.q.push(self.now.saturating_add(b), Ev::Down { owner: i, runner: j, link: n, at_owner: false });
        let again = self.now.saturating_add(a.max(b)).saturating_add(self.rng.span(Duration::from_millis(200), Duration::from_secs(5)));
        self.q.push(again, Ev::Up { owner: i, runner: j });
    }

    fn schedule_owner(&mut self, i: usize) {
        let Some(o) = self.owners.get_mut(i) else { return };
        let next = o.core.timer();
        if next.is_some() && next != o.next_tick {
            o.next_tick = next;
            if let Some(at) = next {
                self.q.push(at_ms(at), Ev::Tick { owner: Some(i), runner: None });
            }
        }
    }

    fn schedule_runner(&mut self, j: usize) {
        let Some(r) = self.runners.get_mut(j) else { return };
        let next = r.core.as_ref().and_then(Admission::timer);
        if next.is_some() && next != r.next_tick {
            r.next_tick = next;
            if let Some(at) = next {
                let theta = r.theta;
                self.q.push(at_ms(at.saturating_sub(theta)), Ev::Tick { owner: None, runner: Some(j) });
            }
        }
    }
}

// The shells: what each core's words become.
impl Sim {
    fn owner_acts(&mut self, i: usize, acts: Vec<Act>) {
        let life = self.life(i);
        for a in acts {
            match a {
                Act::Tell { runner, msg } => {
                    if let Some(j) = runner_of(&runner) {
                        if matches!(msg, ToRunner::Decline { .. }) {
                            self.counts.declined += 1;
                        }
                        self.send(i, j, Msg::Down(msg));
                    }
                }
                Act::Answer { run, answer } => self.answered(i, life, run, answer),
                Act::Go { run, runner, fence } => {
                    if let Some(j) = runner_of(&runner) {
                        let at = self.now.saturating_add(self.rng.span(Duration::from_millis(1), Duration::from_secs(2)));
                        self.q.push(at, Ev::Synced { owner: i, life, run, runner: j, fence });
                    }
                }
                Act::Restart { run, runner, fence } => {
                    if let Some(j) = runner_of(&runner) {
                        self.send(i, j, Msg::Down(ToRunner::Start { run, fence }));
                    }
                }
                Act::Fallback { run, runners } => self.fall_back(i, life, run, &runners),
            }
        }
        self.schedule_owner(i);
    }

    /// What an asker was told, checked against what happened.
    fn answered(&mut self, i: usize, life: u64, run: String, answer: Answer) {
        let now = ms(self.now);
        let key = (i, life, run.clone());
        let Some(a) = self.asked.get(&key).cloned() else { return };
        let alive_same = |s: &Sim, j: usize, rl: u64| s.runners.get(j).is_some_and(|r| r.alive && r.life == rl);
        match &answer {
            Answer::Started { runner, .. } => {
                let ok = runner_of(runner).is_some_and(|j| a.spawned.is_some_and(|(sj, _)| sj == j));
                if !ok {
                    self.fail(format!("{run}: its asker was told it started on {runner} before its process started there"));
                }
            }
            Answer::Lost { runner, .. } => {
                let excused = runner_of(runner).is_some_and(|j| !self.runners.get(j).is_some_and(|r| r.alive) || a.started_to.is_some_and(|(sj, rl)| sj == j && !alive_same(self, j, rl)));
                if !excused {
                    self.fail(format!("{run}: answered lost on {runner}, which is still there, in the same epoch"));
                }
                self.counts.lost += 1;
            }
            Answer::Ended { runner, ok } => {
                let spawned = runner_of(runner).and_then(|j| a.spawned.filter(|(sj, _)| *sj == j));
                match (spawned, *ok) {
                    (None, _) => self.fail(format!("{run}: answered ended on {runner}, where its process never started")),
                    (Some(_), true) if !a.exited => self.fail(format!("{run}: answered ended on {runner} before its process exited")),
                    (Some((j, rl)), false) if alive_same(self, j, rl) => self.fail(format!("{run}: answered lost with {runner}'s process, which didn't go")),
                    _ => {}
                }
                self.counts.ended += 1;
            }
            Answer::Never { .. } => {
                let there: Vec<usize> = a.could.iter().copied().filter(|j| self.shown.get(*j).copied().unwrap_or(false)).collect();
                if !there.is_empty() {
                    self.fail(format!("{run}: answered never, though {there:?} could take it"));
                }
                self.counts.never += 1;
            }
            Answer::Queued { start, .. } => {
                // Its estimate in this node's clock: a runner's start is never before its own now,
                // so corrected it's never before when the runner said it; a word told an hour or
                // more out (the runners' clocks are) was told in a runner's clock.
                for t in [start.soonest, start.latest].into_iter().flatten() {
                    if t < now.saturating_sub(HOUR_MS / 2) {
                        self.fail(format!("{run}: told an expected start {} ms in the past (in a runner's clock?)", now.saturating_sub(t)));
                    }
                }
                if a.first.is_some() {
                    self.counts.news += 1;
                } else {
                    self.counts.queued_first += 1;
                }
            }
            Answer::Starting { .. } => {}
        }
        let end = match &answer {
            Answer::Never { .. } => Some("never"),
            Answer::Lost { .. } => Some("lost"),
            Answer::Ended { .. } => Some("ended"),
            _ => None,
        };
        if let Some(a) = self.asked.get_mut(&key) {
            if a.first.is_none() {
                a.first = Some(answer.clone());
            }
            if a.end.is_none() {
                a.end = end;
            }
            a.last = Some(answer);
        }
    }

    /// The shell's own pick among older runners: the first idle enough starts it.
    fn fall_back(&mut self, i: usize, life: u64, run: String, runners: &[String]) {
        self.counts.fallbacks += 1;
        let key = (i, life, run.clone());
        let ask = self.asked.get(&key).map(|a| a.ask.clone()).unwrap_or_default();
        let q = super::ask::amounts(&ask.needs.request);
        let pick = runners.iter().filter_map(|r| runner_of(r.as_str())).find(|j| self.machine(i, *j).and_then(|m| m.idle).is_some_and(|idle| super::ask::fits(&q, &super::ask::amounts(&idle))));
        self.note(|| format!("o{i}.{life} falls back {run} to {runners:?}: {pick:?}"));
        let acts = match pick {
            Some(j) => {
                self.spawn_count(&key, j);
                self.counts.older_started += 1;
                let rlife = self.runners.get(j).map_or(0, |r| r.life);
                if let Some(r) = self.runners.get_mut(j) {
                    r.procs.insert(key.clone(), ask.needs.request);
                }
                if let Some(a) = self.asked.get_mut(&key) {
                    a.spawned = Some((j, rlife));
                }
                let dur = Duration::from_millis(ask.median.unwrap_or(1000));
                self.q.push(self.now.saturating_add(dur), Ev::OldEnd { runner: j, life: rlife, owner: i, olife: life, run: run.clone() });
                match self.owners.get_mut(i) {
                    Some(o) => o.core.fallback_started(&run),
                    None => Vec::new(),
                }
            }
            None => match self.owners.get_mut(i) {
                Some(o) => o.core.fallback_refused(&run),
                None => Vec::new(),
            },
        };
        self.owner_acts(i, acts);
        // The directory shows an older runner's room as it changes (docs/needs.md §6).
        if pick.is_some() {
            self.directory();
        }
    }

    fn spawn_count(&mut self, key: &(usize, u64, String), j: usize) {
        let n = self.spawns.entry(key.clone()).or_default();
        *n += 1;
        if *n > 1 {
            let why = format!("{} started twice (now on r{j})", key.2);
            self.fail(why);
        }
        self.counts.started += 1;
    }

    /// What a runner's core said: each word sent to its owner's live process, each offer checked
    /// against the room, the runner's books checked (G1).
    fn runner_said(&mut self, j: usize, said: Vec<Said>) {
        for (owner, m) in said {
            match &m {
                ToOwner::Offer { got, fence, .. } => {
                    self.counts.offers += 1;
                    if self.offered.insert((j, *fence)) && got.mem > 0 {
                        self.check_room(j);
                    }
                }
                ToOwner::Lapsed { .. } => self.counts.lapsed += 1,
                _ => {}
            }
            if let Some(i) = owner_of(&owner)
                && self.owners.get(i).is_some_and(|o| o.alive)
            {
                self.send(i, j, Msg::Up(m));
            }
        }
        if let Some(Err(why)) = self.runners.get(j).and_then(|r| r.core.as_ref()).map(Admission::check) {
            self.fail(why);
        }
        self.schedule_runner(j);
    }

    /// No memory offered past the room: what the runner has less what other work uses holds every
    /// claim it made.
    fn check_room(&mut self, j: usize) {
        let Some(r) = self.runners.get(j) else { return };
        let Some(core) = r.core.as_ref() else { return };
        let (_, held) = core.room();
        let [_, held_mem, _, _] = held;
        let room = r.size.mem.saturating_sub(r.other.1);
        if held_mem > room {
            let why = format!("r{j} offered past its room: its claims hold {} GiB of memory, its room is {} GiB", held_mem / GIB, room / GIB);
            self.fail(why);
        }
    }

    fn at_runner(&mut self, i: usize, j: usize, msg: ToRunner) {
        let (life, name) = (self.life(i), oname(i));
        let now = self.rnow(j);
        let peer = Peer { name: &name, epoch: life };
        let mut out = Vec::new();
        let mut spawned: Option<String> = None;
        {
            let predicted = match &msg {
                ToRunner::Probe { ask } => self.predicted.get(&ask.run).copied().flatten(),
                _ => None,
            };
            let Some(core) = self.runners.get_mut(j).and_then(|r| r.core.as_mut()) else { return };
            match msg {
                ToRunner::Probe { ask } => core.probe(peer, ask, predicted, now, &mut out),
                ToRunner::Withdraw { run } => core.withdraw(peer, &run, now, &mut out),
                ToRunner::Take { run, fence } => core.take(peer, &run, fence, now, &mut out),
                ToRunner::Decline { run, fence } => core.decline(peer, &run, fence, now, &mut out),
                ToRunner::Ack { run } => core.ack(peer, &run),
                ToRunner::Start { run, fence } => {
                    // A runner starts a run only under its live claim for it.
                    let live = core.probes().iter().any(|v| v.owner == name && v.epoch == life && v.run == run && (v.state == "offered" || v.state == "taken") && v.fence == Some(fence));
                    if core.start(peer, &run, fence, now, &mut out) == Some(Start::New) {
                        if !live && self.failed.is_none() {
                            self.failed = Some(format!("r{j} started {run} under claim {fence:?}, not live there"));
                        }
                        spawned = Some(run);
                    }
                }
            }
        }
        if let Some(run) = spawned {
            self.spawn(i, j, &run);
            if let Some(core) = self.runners.get_mut(j).and_then(|r| r.core.as_mut()) {
                core.spawned(peer, &run, &mut out);
            }
        }
        self.runner_said(j, out);
    }

    /// Runner `j` starts owner `i`'s run: on its record first (the core wrote it), then the process.
    fn spawn(&mut self, i: usize, j: usize, run: &str) {
        let life = self.life(i);
        let key = (i, life, run.to_owned());
        self.spawn_count(&key, j);
        // A run of an owner gone starts only if its owner said its start before it went.
        if let Some(o) = self.owners.get(i)
            && let Some(died) = o.died.filter(|_| !o.alive)
            && o.starts.get(run).is_none_or(|said| *said > died)
        {
            let why = format!("{run} started on r{j} after its owner o{i} went, its start never said before");
            self.fail(why);
        }
        let ask = self.asked.get(&key).map(|a| a.ask.clone()).unwrap_or_default();
        let dur = Duration::from_millis(ask.median.unwrap_or(1000));
        let rlife = self.runners.get(j).map_or(0, |r| r.life);
        if let Some(a) = self.asked.get_mut(&key) {
            a.spawned = Some((j, rlife));
        }
        let Some(r) = self.runners.get_mut(j) else { return };
        if let Some(core) = r.core.as_ref() {
            r.records = core.records().clone();
        }
        r.procs.insert(key.clone(), ask.needs.request);
        self.q.push(self.now.saturating_add(dur), Ev::End { runner: j, life: rlife, owner: i, olife: life, run: run.to_owned() });
        self.note(|| format!("r{j} starts {run} for o{i}.{life}"));
        self.resample(j);
    }

    /// A new sample at runner `j`.
    fn resample(&mut self, j: usize) {
        let (sample, now) = (self.sample(j), self.rnow(j));
        let mut out = Vec::new();
        match self.runners.get_mut(j).and_then(|r| r.core.as_mut()) {
            Some(core) => core.measured(sample, now, &mut out),
            None => return,
        }
        self.runner_said(j, out);
    }

    fn at_owner(&mut self, i: usize, j: usize, msg: ToOwner) {
        let now = ms(self.now);
        let acts = match self.owners.get_mut(i).filter(|o| o.alive) {
            Some(o) => o.core.heard(&rname(j), msg, now),
            None => return,
        };
        self.owner_acts(i, acts);
    }
}

// The events.
impl Sim {
    /// Whether runner `j` could take `ask` by what it is (its facts and its whole size).
    fn could(&self, j: usize, ask: &Ask) -> bool {
        let Some(r) = self.runners.get(j) else { return false };
        let mut facts = linux();
        facts.cpu.threads = u32::try_from(r.size.cpu.ceil()).unwrap_or(0);
        facts.mem.total = r.size.mem;
        let m = Machine { name: rname(j), facts, labels: Vec::new(), size: Size::known(&r.size), epoch: r.epoch, connected: true, places: r.places, idle: None };
        super::owner::unsuited(&m, ask).is_none()
    }

    fn act(&mut self, ev: Ev) {
        let now = ms(self.now);
        match ev {
            Ev::Deliver { owner, runner, link, msg } => {
                // Only on the link it was sent on: a link that went took what was in flight with it.
                if self.wires.get(&(owner, runner)).is_none_or(|w| !w.up || w.n != link) {
                    return;
                }
                match *msg {
                    Msg::Down(m) => {
                        if self.runners.get(runner).is_some_and(|r| r.alive) {
                            self.at_runner(owner, runner, m);
                        }
                    }
                    Msg::Up(m) => {
                        if self.owners.get(owner).is_some_and(|o| o.alive) {
                            self.at_owner(owner, runner, m);
                        }
                    }
                }
            }
            Ev::Down { owner, runner, link, at_owner } => {
                // A newer link came up since: nothing to hear of this one.
                if self.wires.get(&(owner, runner)).is_some_and(|w| w.n != link) {
                    return;
                }
                if at_owner {
                    let acts = match self.owners.get_mut(owner).filter(|o| o.alive) {
                        Some(o) => o.core.away(&rname(runner)),
                        None => return,
                    };
                    self.owner_acts(owner, acts);
                } else {
                    let (name, rn) = (oname(owner), self.rnow(runner));
                    let mut out = Vec::new();
                    match self.runners.get_mut(runner).and_then(|r| r.core.as_mut()) {
                        Some(core) => core.away(&name, rn, &mut out),
                        None => return,
                    }
                    self.runner_said(runner, out);
                }
            }
            Ev::Up { owner, runner } => {
                let alive = self.owners.get(owner).is_some_and(|o| o.alive) && self.runners.get(runner).is_some_and(|r| r.alive);
                if !alive || self.wires.get(&(owner, runner)).is_some_and(|w| w.up) {
                    return;
                }
                if self.net.is_cut(self.now, self.onode(owner), self.rnode(runner)) {
                    let again = self.now.saturating_add(self.rng.span(Duration::from_millis(200), Duration::from_secs(5)));
                    self.q.push(again, Ev::Up { owner, runner });
                    return;
                }
                let w = self.wires.entry((owner, runner)).or_default();
                w.n = w.n.saturating_add(1);
                w.up = true;
                let (life, name, rn, rtt) = (self.life(owner), oname(owner), self.rnow(runner), self.rtt);
                // The runner: what it holds for the owner answered as it stands, its ends told.
                let mut out = Vec::new();
                if let Some(core) = self.runners.get_mut(runner).and_then(|r| r.core.as_mut()) {
                    core.link(&name, rtt);
                    core.back(Peer { name: &name, epoch: life }, rn, &mut out);
                }
                self.runner_said(runner, out);
                // The owner: what its runner may have missed, said again. A new link means the one
                // before it went, whether or not the owner heard so (its word of that is dropped as
                // stale once this one is up): it's told, then told the link is back.
                let theta = self.runners.get(runner).map_or(0, |r| r.theta);
                let acts = match self.owners.get_mut(owner) {
                    Some(o) => {
                        o.core.link(&rname(runner), Link { rtt: Some(rtt), offset: Offset { theta, delta: 0 } });
                        let mut acts = o.core.away(&rname(runner));
                        acts.extend(o.core.back(&rname(runner)));
                        acts
                    }
                    None => Vec::new(),
                };
                self.owner_acts(owner, acts);
            }
            Ev::Ask { owner, n } => {
                let Some(o) = self.owners.get(owner).filter(|o| o.alive) else { return };
                let life = o.life;
                let Some((_, ask)) = self.plans.get(owner).and_then(|p| p.get(n)).cloned() else { return };
                let could: Vec<usize> = (0..self.runners.len()).filter(|j| self.could(*j, &ask)).collect();
                self.asked.insert((owner, life, ask.run.clone()), Asked { ask: ask.clone(), first: None, last: None, end: None, could, started_to: None, spawned: None, exited: false });
                self.counts.asked += 1;
                self.note(|| format!("o{owner}.{life} asked {}", ask.run));
                let acts = match self.owners.get_mut(owner) {
                    Some(o) => o.core.ask(ask, now),
                    None => return,
                };
                self.owner_acts(owner, acts);
            }
            Ev::Synced { owner, life, run, runner, fence } => {
                let may = match self.owners.get_mut(owner).filter(|o| o.alive && o.life == life) {
                    Some(o) => o.core.starting(&run, &rname(runner), fence),
                    None => false,
                };
                if may {
                    let rlife = self.runners.get(runner).map_or(0, |r| r.life);
                    if let Some(a) = self.asked.get_mut(&(owner, life, run.clone())) {
                        a.started_to = Some((runner, rlife));
                    }
                    let t = self.now;
                    if let Some(o) = self.owners.get_mut(owner) {
                        o.starts.insert(run.clone(), t);
                    }
                    self.send(owner, runner, Msg::Down(ToRunner::Start { run, fence }));
                }
            }
            Ev::End { runner, life, owner, olife, run } => {
                let key = (owner, olife, run.clone());
                let ended = match self.runners.get_mut(runner).filter(|r| r.alive && r.life == life) {
                    Some(r) => r.procs.remove(&key).is_some(),
                    None => false,
                };
                if !ended {
                    return;
                }
                if let Some(a) = self.asked.get_mut(&key) {
                    a.exited = true;
                }
                let (name, rn) = (oname(owner), self.rnow(runner));
                let mut out = Vec::new();
                if let Some(r) = self.runners.get_mut(runner)
                    && let Some(core) = r.core.as_mut()
                {
                    core.ended(Peer { name: &name, epoch: olife }, &run, true, rn, &mut out);
                    r.records = core.records().clone();
                }
                self.runner_said(runner, out);
                self.resample(runner);
            }
            Ev::OldEnd { runner, life, owner, olife, run } => {
                let key = (owner, olife, run);
                let ended = match self.runners.get_mut(runner).filter(|r| r.alive && r.life == life) {
                    Some(r) => r.procs.remove(&key).is_some(),
                    None => false,
                };
                if ended {
                    if let Some(a) = self.asked.get_mut(&key) {
                        a.exited = true;
                        if a.end.is_none() {
                            a.end = Some("ended");
                        }
                    }
                    self.counts.ended += 1;
                    self.directory();
                }
            }
            Ev::Load { runner } => {
                let quiet = self.now >= NEMESIS;
                let Some(r) = self.runners.get(runner) else { return };
                let (cpus, mem) = (r.size.cpu.get(), r.size.mem);
                // Once the nemesis stops, other work goes quiet, so every run a runner could hold
                // fits it in the end.
                let other = if quiet { (0, 0) } else { (self.rng.below(cpus / 2 + 1), self.rng.below(mem / 2 + 1)) };
                let (alive, places) = match self.runners.get_mut(runner) {
                    Some(r) => {
                        r.other = other;
                        (r.alive, r.places)
                    }
                    None => return,
                };
                if alive && places {
                    self.resample(runner);
                } else if alive {
                    self.directory();
                }
                if !quiet {
                    let at = self.now.saturating_add(self.rng.span(Duration::from_secs(1), Duration::from_secs(20))).min(NEMESIS);
                    self.q.push(at, Ev::Load { runner });
                }
            }
            Ev::Tick { owner, runner } => {
                if let Some(i) = owner {
                    let acts = match self.owners.get_mut(i).filter(|o| o.alive) {
                        Some(o) => {
                            o.next_tick = None;
                            o.core.tick(now)
                        }
                        None => return,
                    };
                    self.owner_acts(i, acts);
                }
                if let Some(j) = runner {
                    let rn = self.rnow(j);
                    let mut out = Vec::new();
                    match self.runners.get_mut(j).filter(|r| r.alive) {
                        Some(r) => {
                            r.next_tick = None;
                            if let Some(core) = r.core.as_mut() {
                                core.tick(rn, &mut out);
                            }
                        }
                        None => return,
                    }
                    self.runner_said(j, out);
                }
            }
            Ev::Directory => self.directory(),
            Ev::Crash { owner, runner } => {
                self.counts.crashes += 1;
                if let Some(i) = owner {
                    self.note(|| format!("o{i} dies"));
                    let (t, life) = (self.now, self.life(i));
                    if let Some(o) = self.owners.get_mut(i) {
                        o.alive = false;
                        o.died = Some(t);
                    }
                    for j in 0..self.runners.len() {
                        self.break_link(i, j);
                        self.q.push(t.saturating_add(LINGER), Ev::Forget { runner: j, owner: i, life });
                    }
                }
                if let Some(j) = runner {
                    self.note(|| format!("r{j} dies"));
                    // What an older runner ran goes with it: the shell's own pick, as before
                    // placement, says so (placement's runs are told lost by their owners).
                    let older: Vec<(usize, u64, String)> = self.runners.get(j).filter(|r| !r.places).map(|r| r.procs.keys().cloned().collect()).unwrap_or_default();
                    for key in older {
                        if let Some(a) = self.asked.get_mut(&key)
                            && a.end.is_none()
                        {
                            a.end = Some("lost");
                        }
                    }
                    if let Some(r) = self.runners.get_mut(j) {
                        r.alive = false;
                        r.core = None;
                        r.procs.clear();
                        r.life = r.life.saturating_add(1);
                    }
                    for i in 0..self.owners.len() {
                        self.break_link(i, j);
                    }
                    let at = self.now.saturating_add(self.rng.span(Duration::from_millis(1), NOTICE));
                    self.q.push(at, Ev::Directory);
                }
            }
            Ev::Restart { owner, runner } => {
                self.counts.restarts += 1;
                if let Some(i) = owner {
                    self.note(|| format!("o{i} starts again"));
                    let life = self.life(i).saturating_add(1);
                    if let Some(o) = self.owners.get_mut(i) {
                        o.alive = true;
                        o.life = life;
                        o.core = Owner::seeded(&oname(i), life, self.rng.stream(&format!("owner {i} life {life}")).next_u64());
                        o.next_tick = None;
                    }
                    self.directory();
                    // Its runs asked so far are asked again by the new process, under the same ids.
                    let plan = self.plans.get(i).cloned().unwrap_or_default();
                    for (n, (at, _)) in plan.iter().enumerate() {
                        if *at > self.now {
                            continue;
                        }
                        let again = self.now.saturating_add(Duration::from_millis(u64::try_from(n).unwrap_or(0)));
                        self.q.push(again, Ev::Ask { owner: i, n });
                    }
                    for j in 0..self.runners.len() {
                        self.q.push(self.now, Ev::Up { owner: i, runner: j });
                    }
                }
                if let Some(j) = runner {
                    self.note(|| format!("r{j} starts again"));
                    if let Some(r) = self.runners.get_mut(j) {
                        r.alive = true;
                    }
                    self.boot_runner(j);
                    let at = self.now.saturating_add(self.rng.span(Duration::from_millis(1), NOTICE));
                    self.q.push(at, Ev::Directory);
                    for i in 0..self.owners.len() {
                        self.q.push(self.now, Ev::Up { owner: i, runner: j });
                    }
                }
            }
            Ev::Forget { runner, owner, life } => {
                // Gone past its lease, and not back: what the owner left is let go.
                if self.owners.get(owner).is_some_and(|o| o.alive || o.life != life) {
                    return;
                }
                let (name, rn) = (oname(owner), self.rnow(runner));
                let mut out = Vec::new();
                match self.runners.get_mut(runner).and_then(|r| r.core.as_mut()) {
                    Some(core) => core.forget(&name, rn, &mut out),
                    None => return,
                }
                self.runner_said(runner, out);
            }
            Ev::RelayLost { till } => {
                self.counts.relay_lost += 1;
                let n = self.owners.len() + self.runners.len();
                let nodes: Vec<Node> = (0..n).map(|k| Node(u32::try_from(k).unwrap_or(0))).collect();
                for a in &nodes {
                    self.net.isolate(*a, &nodes, till);
                }
                for i in 0..self.owners.len() {
                    for j in 0..self.runners.len() {
                        self.break_link(i, j);
                    }
                }
                self.q.push(till, Ev::RelayBack);
            }
            Ev::RelayBack => self.directory(),
        }
    }

    fn run(&mut self) -> Result<u64, String> {
        let mut steps = 0u64;
        while let Some((at, ev)) = self.q.pop() {
            if at > NEMESIS.saturating_add(SETTLE) {
                break;
            }
            self.now = self.now.max(at);
            steps += 1;
            if steps > STEPS_MAX {
                return Err(format!("ran past {STEPS_MAX} steps"));
            }
            let what = format!("{ev:?}");
            self.trace.fold_str(&what);
            self.act(ev);
            if let Some(f) = self.failed.take() {
                let tail = self.trace.tail_text();
                return Err(if tail.is_empty() { f } else { format!("{f}\n{tail}") });
            }
            if self.now > NEMESIS.saturating_add(LINGER).saturating_add(NOTICE.saturating_mul(3)) && self.settled() && !self.claims_left() {
                break;
            }
        }
        self.check_end().map(|()| self.trace.digest())
    }

    /// Whether a runner still holds an offered or taken claim (a decline or a lapse in flight).
    fn claims_left(&self) -> bool {
        self.runners.iter().filter_map(|r| r.core.as_ref()).any(|c| c.probes().iter().any(|v| v.state == "offered" || v.state == "taken"))
    }

    fn live(&self, key: &(usize, u64, String)) -> bool {
        self.owners.get(key.0).is_some_and(|o| o.alive && o.life == key.1)
    }

    /// A run only runners gone for good could take: it waits for them, as it should.
    fn excused(&self, a: &Asked) -> bool {
        !a.could.is_empty() && a.could.iter().all(|j| self.runners.get(*j).is_none_or(|r| !r.alive))
    }

    /// Every run asked of a live owner has its end (or only runners gone for good could take it).
    fn settled(&self) -> bool {
        self.asked.iter().all(|(k, a)| a.end.is_some() || !self.live(k) || self.excused(a))
    }

    fn check_end(&mut self) -> Result<(), String> {
        let mut problems = Vec::new();
        let mut excused = 0u64;
        for (k, a) in &self.asked {
            if !self.live(k) || a.end.is_some() {
                continue;
            }
            if self.excused(a) {
                excused += 1;
                continue;
            }
            problems.push(format!("{} of o{}.{} never settled: answered first {:?}, last {:?}", k.2, k.0, k.1, a.first, a.last));
        }
        self.counts.excused += excused;
        let live: BTreeSet<(String, u64)> = self.owners.iter().enumerate().filter(|(_, o)| o.alive).map(|(i, o)| (oname(i), o.life)).collect();
        for (j, r) in self.runners.iter().enumerate() {
            let Some(core) = r.core.as_ref() else { continue };
            for v in core.probes() {
                if v.state == "offered" || v.state == "taken" {
                    problems.push(format!("r{j} still holds a claim for {} ({}) of {}.{}", v.run, v.state, v.owner, v.epoch));
                } else if v.state != "running" && !live.contains(&(v.owner.clone(), v.epoch)) {
                    problems.push(format!("r{j} still holds {} ({}) of {}.{}, gone", v.run, v.state, v.owner, v.epoch));
                }
            }
        }
        if problems.is_empty() {
            return Ok(());
        }
        let tail = self.trace.tail_text();
        Err(format!("{}{}", problems.join("; "), if tail.is_empty() { String::new() } else { format!("\n{tail}") }))
    }
}

/// Runs one seed (`keep`: steps kept in words for a replay): its counts and digest, or what failed.
pub fn run(seed: u64, keep: usize) -> Result<(Counts, u64), String> {
    let mut s = Sim::new(seed, keep);
    let d = s.run()?;
    Ok((s.counts, d))
}

/// Runs one seed with `m` planted.
pub fn run_with(seed: u64, keep: usize, m: Option<Mutant>) -> Result<(Counts, u64), String> {
    planted(m, || run(seed, keep))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/needs.md E5, the goal's own words: no run lost or started twice when any one node dies,
    /// over the 200 seeds `cargo test` runs (more with KILN_SIM_SEEDS), every fault the nemesis
    /// has counted across them.
    #[test]
    fn e5_no_run_is_lost_or_started_twice_when_any_one_node_dies() -> Result<(), String> {
        let mut total = Counts::default();
        let report = seeds::run_budget("place", 200, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (c, d) = run(seed, keep)?;
            total.add(&c);
            Ok(d)
        });
        let line = report.into_result()?;
        println!("{line}\n{total:?}");
        if seeds::replay().is_none() {
            // A nemesis that never acted tested nothing.
            for (what, n) in [
                ("runs asked", total.asked),
                ("runs started", total.started),
                ("runs ended", total.ended),
                ("answered never", total.never),
                ("first answered queued", total.queued_first),
                ("offers", total.offers),
                ("declines", total.declined),
                ("lapses", total.lapsed),
                ("links broken", total.breaks),
                ("crashes", total.crashes),
                ("restarts", total.restarts),
                ("the relay lost", total.relay_lost),
                ("words said twice", total.doubled),
            ] {
                if n == 0 {
                    return Err(format!("no {what} in {} seeds: {total:?}", seeds::budget(200)));
                }
            }
        }
        Ok(())
    }

    /// A seed runs the same twice.
    #[test]
    fn e5_a_place_seed_replays_exactly() -> Result<(), String> {
        for seed in [0, 7, 99] {
            seeds::twice(seed, |s| run(s, 0).map(|(_, d)| d))?;
        }
        Ok(())
    }

    /// Each planted bug fails a seed of the 200, named, that replays alone.
    fn caught(m: Mutant) -> Result<(), String> {
        let (keep, list): (usize, Vec<u64>) = match seeds::replay() {
            Some(s) => (400, vec![s]),
            None => (0, (seeds::from()..seeds::from().saturating_add(seeds::budget(200))).collect()),
        };
        for seed in list {
            if let Err(why) = run_with(seed, keep, Some(m)) {
                println!("{m:?} caught by seed {seed}: {}", why.lines().next().unwrap_or_default());
                return Ok(());
            }
        }
        Err(format!("{m:?} wasn't caught"))
    }

    #[test]
    fn e5_an_owner_taking_two_offers_is_caught() -> Result<(), String> {
        caught(Mutant::TakeTwo)
    }

    #[test]
    fn e5_a_runner_offering_past_its_room_is_caught() -> Result<(), String> {
        caught(Mutant::OfferPastRoom)
    }

    #[test]
    fn e5_an_owner_placing_again_on_silence_is_caught() -> Result<(), String> {
        caught(Mutant::ReplaceOnSilence)
    }

    /// Defect a: a runner whose link is down is still there.
    #[test]
    fn e5_an_owner_losing_runs_on_a_disconnect_is_caught() -> Result<(), String> {
        caught(Mutant::LostOnDisconnect)
    }

    /// Defect b: started only on the runner's word.
    #[test]
    fn e5_an_owner_telling_started_at_held_is_caught() -> Result<(), String> {
        caught(Mutant::StartedAtHeld)
    }

    /// Defect c: a size not measured is unknown, never too small.
    #[test]
    fn e5_an_owner_answering_never_on_an_unknown_size_is_caught() -> Result<(), String> {
        caught(Mutant::NeverOnUnknown)
    }

    /// Defect d: a restarted owner's reused id is another run.
    #[test]
    fn e5_a_runner_reading_a_reused_id_as_ran_is_caught() -> Result<(), String> {
        caught(Mutant::ReusedIdRan)
    }

    /// Defect j: estimates in the asker's clock.
    #[test]
    fn e5_an_owner_showing_a_runner_s_clock_is_caught() -> Result<(), String> {
        caught(Mutant::SkewedClock)
    }
}
