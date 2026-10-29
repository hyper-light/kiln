//! A simulator (docs/ci.md W05 design): runners, legs, durations and failures on a virtual clock,
//! driving the real [`Scheduler`]. Tests and `kiln ci bench --scheduler` use it.
//!
//! [`replay`] replays a recorded week of v1 runs under supersession policies (W25 step 1).

pub mod replay;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use super::retry::{Failed, FailureClass};
use super::state::{Command, Config, DIMS, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, WaitReason, offers};

/// Something the simulation does at a time.
#[derive(Debug, Clone)]
enum Ev {
    Input(Box<Input>),
    Finish { leg: String, attempt: u32 },
    Tick,
}

/// One assignment, as it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Assigned {
    pub at: i64,
    pub leg: String,
    pub runner: String,
    pub attempt: u32,
}

/// A simulated fleet.
pub struct Sim {
    pub sched: Scheduler,
    pub now: i64,
    queue: BinaryHeap<Reverse<(i64, u64)>>,
    events: BTreeMap<u64, Ev>,
    seq: u64,
    durations: BTreeMap<String, i64>,
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
        }
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
        self.input(t, Input::RunnerUp(Box::new(r)));
    }

    /// A leg queued at `t` that takes `duration` ms once it runs.
    pub fn leg(&mut self, t: i64, leg: LegIn, duration: i64) {
        self.durations.insert(leg.id.clone(), duration);
        self.repos.insert(leg.id.clone(), leg.repo.clone());
        self.asks.insert(leg.id.clone(), (leg.request, leg.exclusive));
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
            let cmds = match ev {
                Ev::Input(i) => self.sched.handle(*i, t),
                Ev::Tick => self.sched.handle(Input::Tick, t),
                Ev::Finish { leg, attempt } => {
                    // An attempt that isn't the one running ended already (its runner was lost).
                    if self.running.get(&leg).is_none_or(|r| r.2 != attempt) {
                        continue;
                    }
                    let Some((runner, start, _)) = self.running.remove(&leg) else { continue };
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
        }
        self.now = self.now.max(until);
    }

    fn apply(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            if let Some(trace) = self.trace.as_mut() {
                trace.push((self.now, c.clone()));
            }
            match c {
                Command::Assign { leg, runner, attempt, .. } => {
                    self.assigned.push(Assigned { at: self.now, leg: leg.clone(), runner: runner.clone(), attempt });
                    let d = self.durations_on.get(&(leg.clone(), runner.clone())).or_else(|| self.durations.get(&leg)).copied().unwrap_or(60_000);
                    self.running.insert(leg.clone(), (runner, self.now, attempt));
                    self.at(self.now + d, Ev::Finish { leg, attempt });
                }
                Command::Cancel { leg, attempt, .. } => {
                    // The runner stops at once.
                    if self.running.get(&leg).is_some_and(|r| r.2 == attempt) {
                        self.failures.remove(&leg);
                        let now = self.now;
                        if let Some((runner, start, _)) = self.running.remove(&leg) {
                            if let Some(repo) = self.repos.get(&leg) {
                                self.busy.push((repo.clone(), start, now));
                            }
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
                Command::Demand { leg, decision } => self.decisions.push((self.now, leg, decision.why())),
            }
        }
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
