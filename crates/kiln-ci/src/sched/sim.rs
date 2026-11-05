//! A simulator (docs/ci.md W05 design): runners, legs, durations and failures on a virtual clock,
//! driving the real [`Scheduler`]. Tests and `kiln ci bench --scheduler` use it.
//!
//! [`replay`] replays a recorded week of v1 runs under supersession policies (W25 step 1).

pub mod replay;

#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use super::retry::{Failed, FailureClass};
use super::state::{Command, Config, DEFAULT_TIMEOUT_MS, DIMS, Fence, Input, LegIn, Outcome, Resources, RunnerIn, Sample, Scheduler, WaitReason, offers};

/// Something the simulation does at a time.
#[derive(Debug, Clone)]
enum Ev {
    Input(Box<Input>),
    Finish { leg: String, attempt: u32 },
    Tick,
    /// A runner samples itself (W05), and again `every` ms on.
    Sample { runner: String, every: i64 },
    /// A runner's heartbeat, renewing the grants it holds (W05-R34).
    Heartbeat { runner: String },
}

/// One assignment, as it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Assigned {
    pub at: i64,
    pub leg: String,
    pub runner: String,
    pub attempt: u32,
}

/// A runner that samples itself as `kiln agent` does (W05): what it has, and what it runs of its
/// own beside kiln's legs (a production node's services), which its samples count in use.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Machine {
    pub cpus: u32,
    pub mem: u64,
    /// Its own work: the CPUs it keeps busy, the memory it has in use, and its I/O pressure
    /// (`full` avg10).
    pub own_cpu: f64,
    pub own_mem: u64,
    pub io_full: f64,
}

/// A grant as the simulation saw it (W05-R38): its leg, attempt and runner, when it started, its
/// expected and hard ends as the engine reckons them from its leg (its p90, else its cost, else its
/// timeout; its timeout), and when the engine took it to have ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub leg: String,
    pub attempt: u32,
    pub runner: String,
    pub start: i64,
    pub expected: i64,
    pub hard: i64,
    pub end: Option<i64>,
}

/// A stretch an item held one room kept for it (W05-R39, R40; docs/needs.md G6): from when the room
/// was kept (`from`), from that room's time (`at`), on its runner, until the item started
/// (`started`) or the room went, the item waiting (worked out again elsewhere or later, its runner
/// gone, a class before its own granted into it). Its `bound`, once it started: `at` plus how far
/// the grants it waited for (those on its runner, and on its gang's members' runners, expected to
/// end by `at`) ran past their expected ends, each no further than its hard end.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub leg: String,
    pub from: i64,
    pub at: i64,
    pub runners: BTreeSet<String>,
    pub started: Option<i64>,
    pub bound: Option<i64>,
}

/// A simulated fleet.
pub struct Sim {
    pub sched: Scheduler,
    pub now: i64,
    queue: BinaryHeap<Reverse<(i64, u64)>>,
    events: BTreeMap<u64, Ev>,
    seq: u64,
    durations: BTreeMap<String, i64>,
    /// What each leg asks.
    requests: BTreeMap<String, Resources>,
    /// The runners that sample themselves (W05), by id.
    machines: BTreeMap<String, Machine>,
    /// What runs on each of those as it changes: (time, runner, what the legs there ask in all).
    pub placed: Vec<(i64, String, Resources)>,
    /// A leg's duration on one runner, where it differs (a warm runner's, W24-R18): (leg, runner).
    pub durations_on: BTreeMap<(String, String), i64>,
    failures: BTreeMap<String, Vec<FailureClass>>,
    repos: BTreeMap<String, String>,
    /// Each running leg's runner, start and attempt.
    running: BTreeMap<String, (String, i64, u32)>,
    pub assigned: Vec<Assigned>,
    pub settled: Vec<(i64, String, Outcome)>,
    pub waits: Vec<(i64, String, WaitReason)>,
    pub runs: Vec<(i64, Command)>,
    /// Adopters done, reused from their execution: (time, leg, execution, outcome) (W28-R44).
    pub adopted: Vec<(i64, String, String, Outcome)>,
    /// Demand decisions, each said once: (time, leg, why) (W28-R45).
    pub decisions: Vec<(i64, String, String)>,
    /// Legs that overflowed their pool: (time, leg, why) (W28-R80).
    pub overflows: Vec<(i64, String, String)>,
    /// Slot-time: (repo, start, end).
    pub busy: Vec<(String, i64, i64)>,
    /// Slot-time by leg: (leg, start, end).
    pub busy_legs: Vec<(String, i64, i64)>,
    /// The same, with the runner: (leg, runner, start, end).
    pub busy_on: Vec<(String, String, i64, i64)>,
    /// What each leg asks (its request, exclusive or not) and each runner offers, and whether the
    /// fleet is in resource mode: what [`Self::dominant_by`] measures by.
    asks: BTreeMap<String, (Resources, bool)>,
    offered: BTreeMap<String, RunnerIn>,
    resources: bool,
    /// How long a host takes to start a pool's executor (W13-R09).
    pub executor_start_ms: i64,
    /// Hosts whose executors fail to start.
    pub failing_hosts: BTreeSet<String>,
    /// Every command the scheduler gave, with its time, when kept (`Some`): docs/needs.md E1's
    /// replay digests it.
    pub trace: Option<Vec<(i64, Command)>>,
    /// Executors started: (time, executor, host); and stopped: (time, executor).
    pub executors_started: Vec<(i64, String, String)>,
    pub executors_stopped: Vec<(i64, String)>,
    /// How often each runner renews its grants, once [`Self::renew_every`] says (W05-R34); the
    /// runners whose heartbeats are going; and when each runner is silent, its heartbeats lost.
    heartbeat: Option<i64>,
    beating: BTreeSet<String>,
    silences: Vec<(String, i64, i64)>,
    /// Grants the engine took back: (time, leg, attempt, runner) (W05-R34); and each leg's last
    /// grant's fence, which its runner's heartbeats name.
    pub lost: Vec<(i64, String, u32, String)>,
    fences: BTreeMap<String, Fence>,
    /// Each leg as it was queued, and how many of its next grants its runner refuses.
    inputs: BTreeMap<String, LegIn>,
    refusals: BTreeMap<String, u32>,
    /// Every grant, as [`Grant`] says; the stretches items held kept room, closed (`held`) and
    /// open; and each time a grant batch took part of a gang: (time, gang, members granted, its
    /// size) (G4).
    pub grants: Vec<Grant>,
    pub held: Vec<Held>,
    holding: BTreeMap<String, Held>,
    pub in_part: Vec<(i64, String, u32, u32)>,
}

impl Sim {
    pub fn new(cfg: Config) -> Sim {
        let resources = cfg.resources;
        Sim {
            sched: Scheduler::new(cfg),
            now: 0,
            queue: BinaryHeap::new(),
            events: BTreeMap::new(),
            seq: 0,
            durations: BTreeMap::new(),
            requests: BTreeMap::new(),
            machines: BTreeMap::new(),
            placed: Vec::new(),
            durations_on: BTreeMap::new(),
            failures: BTreeMap::new(),
            repos: BTreeMap::new(),
            running: BTreeMap::new(),
            assigned: Vec::new(),
            settled: Vec::new(),
            waits: Vec::new(),
            runs: Vec::new(),
            adopted: Vec::new(),
            decisions: Vec::new(),
            overflows: Vec::new(),
            busy: Vec::new(),
            busy_legs: Vec::new(),
            busy_on: Vec::new(),
            asks: BTreeMap::new(),
            offered: BTreeMap::new(),
            resources,
            executor_start_ms: 500,
            failing_hosts: BTreeSet::new(),
            trace: None,
            executors_started: Vec::new(),
            executors_stopped: Vec::new(),
            heartbeat: None,
            beating: BTreeSet::new(),
            silences: Vec::new(),
            lost: Vec::new(),
            fences: BTreeMap::new(),
            inputs: BTreeMap::new(),
            refusals: BTreeMap::new(),
            grants: Vec::new(),
            held: Vec::new(),
            holding: BTreeMap::new(),
            in_part: Vec::new(),
        }
    }

    /// `leg`'s next `times` grants are refused by the runners they're sent to, as a runner's own
    /// admission refuses one (W05-R02).
    pub fn refuse(&mut self, leg: &str, times: u32) {
        self.refusals.insert(leg.to_owned(), times);
    }

    /// Each runner renews the grants it holds every `ms` from when it comes up (W05-R34), naming
    /// the legs it runs at their attempts, those the engine took back among them, until they end.
    pub fn renew_every(&mut self, ms: i64) {
        self.heartbeat = Some(ms.max(1));
    }

    /// `runner`'s heartbeats from `from` until `until` are lost: its grants aren't renewed then.
    pub fn silence(&mut self, runner: &str, from: i64, until: i64) {
        self.silences.push((runner.to_owned(), from, until));
    }

    fn at(&mut self, t: i64, e: Ev) {
        self.seq += 1;
        self.events.insert(self.seq, e);
        self.queue.push(Reverse((t, self.seq)));
    }

    /// An input at time `t`.
    pub fn input(&mut self, t: i64, i: Input) {
        self.at(t, Ev::Input(Box::new(i)));
    }

    pub fn runner(&mut self, t: i64, r: RunnerIn) {
        self.offered.insert(r.id.clone(), r.clone());
        if let Some(every) = self.heartbeat
            && self.beating.insert(r.id.clone())
        {
            self.at(t + every, Ev::Heartbeat { runner: r.id.clone() });
        }
        self.input(t, Input::RunnerUp(Box::new(r)));
    }

    /// A runner that samples itself from `t` and every `every` ms on (W05; `kiln agent` does every
    /// 5 s): each sample says what it has in use, its own work's and what the legs running there
    /// ask, each leg using what it asked. Its samples go on without end: run the fleet to a time.
    pub fn sampled(&mut self, t: i64, runner: &str, m: Machine, every: i64) {
        self.machines.insert(runner.to_owned(), m);
        self.at(t, Ev::Sample { runner: runner.to_owned(), every: every.max(1) });
    }

    /// A leg queued at `t` that takes `duration` ms once it runs.
    pub fn leg(&mut self, t: i64, leg: LegIn, duration: i64) {
        self.durations.insert(leg.id.clone(), duration);
        self.requests.insert(leg.id.clone(), leg.request);
        self.repos.insert(leg.id.clone(), leg.repo.clone());
        self.asks.insert(leg.id.clone(), (leg.request, leg.exclusive));
        self.inputs.insert(leg.id.clone(), leg.clone());
        self.input(t, Input::LegQueued(Box::new(leg)));
    }

    /// The leg's first attempts fail, one class each.
    pub fn fail(&mut self, leg: &str, classes: &[FailureClass]) {
        self.failures.insert(leg.to_owned(), classes.to_vec());
    }

    /// Runs until no event is left before `until`.
    pub fn run(&mut self, until: i64) {
        while let Some(Reverse((t, id))) = self.queue.peek().copied() {
            if t > until {
                break;
            }
            self.queue.pop();
            self.now = t;
            let Some(ev) = self.events.remove(&id) else { continue };
            let granted = self.assigned.len();
            let cmds = match ev {
                Ev::Input(i) => self.sched.handle(*i, t),
                Ev::Tick => self.sched.handle(Input::Tick, t),
                Ev::Sample { runner, every } => {
                    self.at(t.saturating_add(every), Ev::Sample { runner: runner.clone(), every });
                    let sample = self.sample(&runner);
                    self.sched.handle(Input::RunnerSample { runner, sample }, t)
                }
                Ev::Heartbeat { runner } => {
                    if let Some(every) = self.heartbeat {
                        self.at(t + every, Ev::Heartbeat { runner: runner.clone() });
                    }
                    if self.silences.iter().any(|(r, from, until)| *r == runner && (*from..*until).contains(&t)) {
                        continue;
                    }
                    let held: Vec<(String, Fence)> = self.running.iter().filter(|(_, (r, _, _))| *r == runner).filter_map(|(leg, _)| self.fences.get(leg).map(|f| (leg.clone(), *f))).collect();
                    self.sched.handle(Input::Renewed { runner, held }, t)
                }
                Ev::Finish { leg, attempt } => {
                    // An attempt that isn't the one running here any more (its runner was lost,
                    // or its grant): its runner still says it ended, and the engine takes nothing
                    // from it (W05-R34).
                    if self.running.get(&leg).is_none_or(|r| r.2 != attempt) {
                        let cmds = self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Success }, t);
                        self.apply(cmds);
                        continue;
                    }
                    let Some((runner, start, _)) = self.running.remove(&leg) else { continue };
                    self.note_placed(&runner);
                    self.ended(&leg, attempt);
                    if let Some(repo) = self.repos.get(&leg) {
                        self.busy.push((repo.clone(), start, t));
                    }
                    self.busy_legs.push((leg.clone(), start, t));
                    self.busy_on.push((leg.clone(), runner, start, t));
                    let outcome = match self.failures.get_mut(&leg).filter(|f| !f.is_empty()) {
                        Some(f) => Outcome::Failed(Failed { class: f.remove(0), exit_code: None, log_hits: Vec::new() }),
                        None => Outcome::Success,
                    };
                    self.sched.handle(Input::AttemptFinished { leg, attempt, outcome }, t)
                }
            };
            self.apply(cmds);
            self.watch(granted);
        }
        self.now = self.now.max(until);
    }

    /// After each event: the stretches items hold kept room, opened, followed onto the runners
    /// their room moves to, and closed, each one that started then with its bound (G6); and each
    /// gang the event's grants took part of (G4).
    fn watch(&mut self, granted: usize) {
        let now = self.now;
        let started: BTreeSet<String> = self.assigned.get(granted..).unwrap_or_default().iter().map(|a| a.leg.clone()).collect();
        let kept = self.sched.kept_items();
        for (leg, runner, at) in &kept {
            // Room worked out again, later or elsewhere, is other room: the one it had went.
            if self.holding.get(leg).is_some_and(|h| h.at != *at || !h.runners.contains(runner))
                && let Some(h) = self.holding.remove(leg)
            {
                self.held.push(h);
            }
            self.holding.entry(leg.clone()).or_insert_with(|| Held { leg: leg.clone(), from: now, at: *at, runners: BTreeSet::from([runner.clone()]), started: None, bound: None });
        }
        let gone: Vec<String> = self.holding.keys().filter(|l| !kept.iter().any(|(k, _, _)| k == *l)).cloned().collect();
        let closed: Vec<Held> = gone.iter().filter_map(|l| self.holding.remove(l)).collect();
        // A gang starts once room is free for all its members: each waited for the grants on all
        // their runners.
        let gang_of = |leg: &str| self.inputs.get(leg).and_then(|l| l.gang.as_ref()).map(|g| g.id.clone());
        let mut crews: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for h in closed.iter().filter(|h| started.contains(&h.leg)) {
            if let Some(g) = gang_of(&h.leg) {
                crews.entry(g).or_default().extend(h.runners.iter().cloned());
            }
        }
        for mut h in closed {
            if started.contains(&h.leg) {
                let runners = gang_of(&h.leg).and_then(|g| crews.get(&g)).unwrap_or(&h.runners);
                let over = self
                    .grants
                    .iter()
                    .filter(|g| runners.contains(&g.runner) && g.leg != h.leg && g.expected <= h.at && g.start <= now && g.end.is_none_or(|e| e > h.from))
                    .map(|g| (g.end.unwrap_or(now).min(g.hard) - g.expected).max(0))
                    .max()
                    .unwrap_or(0);
                (h.started, h.bound) = (Some(now), Some(h.at + over));
            }
            self.held.push(h);
        }
        // A gang's grants come all at once or not at all.
        let mut gangs: BTreeMap<String, (u32, u32)> = BTreeMap::new();
        for leg in &started {
            if let Some(g) = self.inputs.get(leg).and_then(|l| l.gang.as_ref()) {
                let e = gangs.entry(g.id.clone()).or_insert((0, g.size));
                e.0 += 1;
            }
        }
        for (g, (n, size)) in gangs {
            if n < size {
                self.in_part.push((now, g, n, size));
            }
        }
    }

    /// The grant of `leg`'s `attempt` ended, as the engine takes it, now.
    fn ended(&mut self, leg: &str, attempt: u32) {
        let now = self.now;
        if let Some(g) = self.grants.iter_mut().rev().find(|g| g.leg == leg && g.attempt == attempt && g.end.is_none()) {
            g.end = Some(now);
        }
    }

    fn apply(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            if let Some(trace) = self.trace.as_mut() {
                trace.push((self.now, c.clone()));
            }
            match c {
                Command::Assign { leg, runner, attempt, fence, .. } => {
                    self.fences.insert(leg.clone(), fence);
                    self.assigned.push(Assigned { at: self.now, leg: leg.clone(), runner: runner.clone(), attempt });
                    // Its ends as the engine reckons them (W05-R38).
                    let input = self.inputs.get(&leg);
                    let ms = |m: u64| i64::try_from(m).unwrap_or(i64::MAX / 4);
                    let hard = self.now + ms(input.and_then(|l| l.timeout).unwrap_or(DEFAULT_TIMEOUT_MS));
                    let expected = input.and_then(|l| l.p90.or(l.cost)).map_or(hard, |m| (self.now + ms(m)).min(hard));
                    self.grants.push(Grant { leg: leg.clone(), attempt, runner: runner.clone(), start: self.now, expected, hard, end: None });
                    // One its runner refuses (W05-R02) never runs there.
                    if let Some(n) = self.refusals.get_mut(&leg).filter(|n| **n > 0) {
                        *n -= 1;
                        self.ended(&leg, attempt);
                        self.input(self.now, Input::Rejected { leg, fence });
                        continue;
                    }
                    let d = self.durations_on.get(&(leg.clone(), runner.clone())).or_else(|| self.durations.get(&leg)).copied().unwrap_or(60_000);
                    self.running.insert(leg.clone(), (runner.clone(), self.now, attempt));
                    self.note_placed(&runner);
                    self.at(self.now + d, Ev::Finish { leg, attempt });
                }
                Command::Cancel { leg, attempt, .. } => {
                    // The runner stops at once.
                    if self.running.get(&leg).is_some_and(|r| r.2 == attempt) {
                        self.failures.remove(&leg);
                        let now = self.now;
                        if let Some((runner, start, _)) = self.running.remove(&leg) {
                            self.ended(&leg, attempt);
                            if let Some(repo) = self.repos.get(&leg) {
                                self.busy.push((repo.clone(), start, now));
                            }
                            self.note_placed(&runner);
                            self.busy_on.push((leg.clone(), runner, start, now));
                        }
                        let more = self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Cancelled }, now);
                        self.apply(more);
                    }
                }
                Command::Timer { at } => self.at(at, Ev::Tick),
                Command::StartExecutor { executor, runner, .. } => {
                    self.executors_started.push((self.now, executor.clone(), runner.clone()));
                    let t = self.now + self.executor_start_ms;
                    if self.failing_hosts.contains(&runner) {
                        self.input(t, Input::ExecutorFailed { executor, error: format!("{runner} couldn't start it") });
                    } else if runner.is_empty() {
                        // The server's executor (a `k8s` pool's pod): its own runner connects.
                        self.input(t, Input::RunnerUp(Box::new(RunnerIn { id: format!("pod-{executor}"), slots: 1, executor: Some(executor), ..RunnerIn::default() })));
                    } else {
                        self.input(t, Input::ExecutorReady { executor });
                    }
                }
                Command::StopExecutor { executor, runner } => {
                    self.executors_stopped.push((self.now, executor));
                    // A pod goes with its runner.
                    if runner.starts_with("pod-") {
                        self.input(self.now, Input::RunnerLost { runner });
                    }
                }
                Command::Settled { leg, outcome, .. } => self.settled.push((self.now, leg, outcome)),
                Command::Wait { leg, reason } => self.waits.push((self.now, leg, reason)),
                c @ (Command::StartRun { .. } | Command::CancelRun { .. }) => self.runs.push((self.now, c)),
                Command::Retry { .. } => {}
                Command::Adopted { leg, from, outcome } => self.adopted.push((self.now, leg, from, outcome)),
                // The attempt runs on where it was, and says it ended when it does (a CI leg
                // runs to its end): the slot-time it takes there still counts.
                Command::Lost { leg, attempt, runner, .. } => {
                    self.ended(&leg, attempt);
                    self.lost.push((self.now, leg, attempt, runner));
                }
                Command::Demand { leg, decision } => self.decisions.push((self.now, leg, decision.why())),
                Command::Overflowed { leg, why } => self.overflows.push((self.now, leg, why)),
            }
        }
    }

    /// What the legs running on a runner ask, in all.
    pub fn on(&self, runner: &str) -> Resources {
        let mut all = Resources::default();
        for (leg, (r, _, _)) in &self.running {
            if r == runner
                && let Some(q) = self.requests.get(leg)
            {
                all.cpu = all.cpu.saturating_add(q.cpu);
                all.mem = all.mem.saturating_add(q.mem);
                all.disk = all.disk.saturating_add(q.disk);
                all.gpu = all.gpu.saturating_add(q.gpu);
            }
        }
        all
    }

    /// Notes what runs on a runner that samples itself, as it changes.
    fn note_placed(&mut self, runner: &str) {
        if self.machines.contains_key(runner) {
            let on = self.on(runner);
            self.placed.push((self.now, runner.to_owned(), on));
        }
    }

    /// A runner's sample now: its own work, and what its legs ask, used. A measurement, as a
    /// runner's own is: in floats.
    #[allow(clippy::float_arithmetic)]
    fn sample(&self, runner: &str) -> Sample {
        let m = self.machines.get(runner).copied().unwrap_or_default();
        let on = self.on(runner);
        let busy = m.own_cpu + on.cpu.as_f64();
        let cpus = f64::from(m.cpus);
        Sample { cpus: m.cpus, busy: if cpus > 0.0 { (busy / cpus).min(1.0) } else { 0.0 }, load1: busy, mem_total: m.mem, mem_used: m.own_mem.saturating_add(on.mem), io_full: Some(m.io_full), mem_full: None }
    }

    /// Each repository's share of the slot-time in `[from, to)`: a measurement of what the engine
    /// did, for tests and the bench, not a decision.
    #[allow(clippy::float_arithmetic)]
    pub fn shares(&self, from: i64, to: i64) -> BTreeMap<String, f64> {
        let mut by: BTreeMap<String, f64> = BTreeMap::new();
        for (repo, s, e) in &self.busy {
            let overlap = (e.min(&to) - s.max(&from)).max(0);
            *by.entry(repo.clone()).or_default() += overlap as f64;
        }
        let total: f64 = by.values().sum();
        if total > 0.0 {
            for v in by.values_mut() {
                *v /= total;
            }
        }
        by
    }

    /// Each group's share of the slot-time in `[from, to)`, legs grouped by `key` (their actor, their
    /// lane): a measurement, as [`Self::shares`] is.
    #[allow(clippy::float_arithmetic)]
    pub fn shares_by(&self, from: i64, to: i64, key: impl Fn(&str) -> String) -> BTreeMap<String, f64> {
        let mut by: BTreeMap<String, f64> = BTreeMap::new();
        for (leg, s, e) in &self.busy_legs {
            let overlap = (e.min(&to) - s.max(&from)).max(0);
            *by.entry(key(leg)).or_default() += overlap as f64;
        }
        let total: f64 = by.values().sum();
        if total > 0.0 {
            for v in by.values_mut() {
                *v /= total;
            }
        }
        by
    }

    /// Each group's share of the dominant-resource time in `[from, to)` (docs/needs.md G7, E2.1):
    /// each leg's time there weighted by its dominant share of the fleet (the largest of what it
    /// holds over what the runners offer, in slots, CPUs, memory and GPUs, as the engine counts
    /// them), legs grouped by `key`, and only legs on runners `on` keeps. A measurement, as
    /// [`Self::shares`] is.
    #[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
    pub fn dominant_by(&self, from: i64, to: i64, key: impl Fn(&str) -> String, on: impl Fn(&str) -> bool) -> BTreeMap<String, f64> {
        let mut fleet = [0u64; DIMS];
        for r in self.offered.values() {
            for (f, o) in fleet.iter_mut().zip(offers(r, self.resources)) {
                *f += o;
            }
        }
        let mut by: BTreeMap<String, f64> = BTreeMap::new();
        for (leg, runner, s, e) in &self.busy_on {
            let overlap = (e.min(&to) - s.max(&from)).max(0);
            let (Some((q, exclusive)), Some(r)) = (self.asks.get(leg), self.offered.get(runner)) else { continue };
            if overlap == 0 || !on(runner) {
                continue;
            }
            let has = offers(r, self.resources);
            let holds = if *exclusive && q.cpu.is_zero() { has } else { [1, q.cpu.get(), q.mem, u64::from(q.gpu)] };
            let dominant = holds.iter().zip(has).zip(fleet).filter(|((_, h), f)| *h > 0 && *f > 0).map(|((a, _), f)| *a as f64 / f as f64).fold(0.0, f64::max);
            *by.entry(key(leg)).or_default() += overlap as f64 * dominant;
        }
        let total: f64 = by.values().sum();
        if total > 0.0 {
            for v in by.values_mut() {
                *v /= total;
            }
        }
        by
    }

    /// When a leg was first assigned.
    pub fn started(&self, leg: &str) -> Option<i64> {
        self.assigned.iter().find(|a| a.leg == leg).map(|a| a.at)
    }

    /// The last wait reason a leg was given.
    pub fn last_wait(&self, leg: &str) -> Option<&WaitReason> {
        self.waits.iter().rev().find(|(_, l, _)| l == leg).map(|(_, _, r)| r)
    }
}
