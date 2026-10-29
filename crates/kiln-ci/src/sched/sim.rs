//! A simulator (docs/ci.md W05 design): runners, legs, durations and failures on a virtual clock,
//! driving the real [`Scheduler`]. Tests and `kiln ci bench --scheduler` use it.
//!
//! [`replay`] replays a recorded week of v1 runs under supersession policies (W25 step 1).

pub mod replay;

#[cfg(test)]
mod tests;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

use super::retry::{Failed, FailureClass};
use super::state::{Command, Config, Input, LegIn, Outcome, Resources, RunnerIn, Sample, Scheduler, WaitReason};

/// Something the simulation does at a time.
#[derive(Debug, Clone)]
enum Ev {
    Input(Box<Input>),
    Finish { leg: String, attempt: u32 },
    Tick,
    /// A runner samples itself (W05), and again `every` ms on.
    Sample { runner: String, every: i64 },
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

/// A simulated fleet.
pub struct Sim {
    pub sched: Scheduler,
    pub now: i64,
    queue: BinaryHeap<Reverse<(i64, u64)>>,
    events: HashMap<u64, Ev>,
    seq: u64,
    durations: HashMap<String, i64>,
    /// What each leg asks.
    requests: HashMap<String, Resources>,
    /// The runners that sample themselves (W05), by id.
    machines: HashMap<String, Machine>,
    /// What runs on each of those as it changes: (time, runner, what the legs there ask in all).
    pub placed: Vec<(i64, String, Resources)>,
    /// A leg's duration on one runner, where it differs (a warm runner's, W24-R18): (leg, runner).
    pub durations_on: HashMap<(String, String), i64>,
    failures: HashMap<String, Vec<FailureClass>>,
    repos: HashMap<String, String>,
    running: HashMap<String, (String, i64)>,
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
    /// How long a host takes to start a pool's executor (W13-R09).
    pub executor_start_ms: i64,
    /// Hosts whose executors fail to start.
    pub failing_hosts: HashSet<String>,
    /// Executors started: (time, executor, host); and stopped: (time, executor).
    pub executors_started: Vec<(i64, String, String)>,
    pub executors_stopped: Vec<(i64, String)>,
}

impl Sim {
    pub fn new(cfg: Config) -> Sim {
        Sim {
            sched: Scheduler::new(cfg),
            now: 0,
            queue: BinaryHeap::new(),
            events: HashMap::new(),
            seq: 0,
            durations: HashMap::new(),
            requests: HashMap::new(),
            machines: HashMap::new(),
            placed: Vec::new(),
            durations_on: HashMap::new(),
            failures: HashMap::new(),
            repos: HashMap::new(),
            running: HashMap::new(),
            assigned: Vec::new(),
            settled: Vec::new(),
            waits: Vec::new(),
            runs: Vec::new(),
            adopted: Vec::new(),
            decisions: Vec::new(),
            busy: Vec::new(),
            busy_legs: Vec::new(),
            executor_start_ms: 500,
            failing_hosts: HashSet::new(),
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
                Ev::Sample { runner, every } => {
                    self.at(t.saturating_add(every), Ev::Sample { runner: runner.clone(), every });
                    let sample = self.sample(&runner);
                    self.sched.handle(Input::RunnerSample { runner, sample }, t)
                }
                Ev::Finish { leg, attempt } => {
                    let Some((runner, start)) = self.running.remove(&leg) else { continue };
                    self.note_placed(&runner);
                    if let Some(repo) = self.repos.get(&leg) {
                        self.busy.push((repo.clone(), start, t));
                    }
                    self.busy_legs.push((leg.clone(), start, t));
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
            match c {
                Command::Assign { leg, runner, attempt, .. } => {
                    self.assigned.push(Assigned { at: self.now, leg: leg.clone(), runner: runner.clone(), attempt });
                    let d = self.durations_on.get(&(leg.clone(), runner.clone())).or_else(|| self.durations.get(&leg)).copied().unwrap_or(60_000);
                    self.running.insert(leg.clone(), (runner.clone(), self.now));
                    self.note_placed(&runner);
                    self.at(self.now + d, Ev::Finish { leg, attempt });
                }
                Command::Cancel { leg, attempt, .. } => {
                    // The runner stops at once.
                    if self.running.contains_key(&leg) {
                        self.failures.remove(&leg);
                        let now = self.now;
                        if let Some((runner, start)) = self.running.remove(&leg) {
                            if let Some(repo) = self.repos.get(&leg) {
                                self.busy.push((repo.clone(), start, now));
                            }
                            self.note_placed(&runner);
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

    /// What the legs running on a runner ask, in all.
    pub fn on(&self, runner: &str) -> Resources {
        let mut all = Resources::default();
        for (leg, (r, _)) in &self.running {
            if r == runner
                && let Some(q) = self.requests.get(leg)
            {
                all.cpu += q.cpu;
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

    /// A runner's sample now: its own work, and what its legs ask, used.
    fn sample(&self, runner: &str) -> Sample {
        let m = self.machines.get(runner).copied().unwrap_or_default();
        let on = self.on(runner);
        let busy = m.own_cpu + on.cpu;
        let cpus = f64::from(m.cpus);
        Sample { cpus: m.cpus, busy: if cpus > 0.0 { (busy / cpus).min(1.0) } else { 0.0 }, load1: busy, mem_total: m.mem, mem_used: m.own_mem.saturating_add(on.mem), io_full: Some(m.io_full) }
    }

    /// Each repository's share of the slot-time in `[from, to)`.
    pub fn shares(&self, from: i64, to: i64) -> HashMap<String, f64> {
        let mut by: HashMap<String, f64> = HashMap::new();
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
    /// lane).
    pub fn shares_by(&self, from: i64, to: i64, key: impl Fn(&str) -> String) -> HashMap<String, f64> {
        let mut by: HashMap<String, f64> = HashMap::new();
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

    /// When a leg was first assigned.
    pub fn started(&self, leg: &str) -> Option<i64> {
        self.assigned.iter().find(|a| a.leg == leg).map(|a| a.at)
    }

    /// The last wait reason a leg was given.
    pub fn last_wait(&self, leg: &str) -> Option<&WaitReason> {
        self.waits.iter().rev().find(|(_, l, _)| l == leg).map(|(_, _, r)| r)
    }
}
