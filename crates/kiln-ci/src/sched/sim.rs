//! A simulator (docs/ci.md W05 design): runners, legs, durations and failures on a virtual clock,
//! driving the real [`Scheduler`]. Tests and `kiln ci bench --scheduler` use it.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use super::retry::{Failed, FailureClass};
use super::state::{Command, Config, Input, LegIn, Outcome, RunnerIn, Scheduler, WaitReason};

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
    events: HashMap<u64, Ev>,
    seq: u64,
    durations: HashMap<String, i64>,
    failures: HashMap<String, Vec<FailureClass>>,
    repos: HashMap<String, String>,
    running: HashMap<String, (String, i64)>,
    pub assigned: Vec<Assigned>,
    pub settled: Vec<(i64, String, Outcome)>,
    pub waits: Vec<(i64, String, WaitReason)>,
    pub runs: Vec<(i64, Command)>,
    /// Slot-time: (repo, start, end).
    pub busy: Vec<(String, i64, i64)>,
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
            failures: HashMap::new(),
            repos: HashMap::new(),
            running: HashMap::new(),
            assigned: Vec::new(),
            settled: Vec::new(),
            waits: Vec::new(),
            runs: Vec::new(),
            busy: Vec::new(),
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

    /// A leg queued at `t` that takes `duration` ms once it runs.
    pub fn leg(&mut self, t: i64, leg: LegIn, duration: i64) {
        self.durations.insert(leg.id.clone(), duration);
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
                Ev::Finish { leg, attempt } => {
                    let Some((_, start)) = self.running.remove(&leg) else { continue };
                    if let Some(repo) = self.repos.get(&leg) {
                        self.busy.push((repo.clone(), start, t));
                    }
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
                    self.running.insert(leg.clone(), (runner, self.now));
                    let d = self.durations.get(&leg).copied().unwrap_or(60_000);
                    self.at(self.now + d, Ev::Finish { leg, attempt });
                }
                Command::Cancel { leg, attempt, .. } => {
                    // The runner stops at once.
                    if self.running.contains_key(&leg) {
                        self.failures.remove(&leg);
                        let now = self.now;
                        if let Some((_, start)) = self.running.remove(&leg)
                            && let Some(repo) = self.repos.get(&leg)
                        {
                            self.busy.push((repo.clone(), start, now));
                        }
                        let more = self.sched.handle(Input::AttemptFinished { leg, attempt, outcome: Outcome::Cancelled }, now);
                        self.apply(more);
                    }
                }
                Command::Timer { at } => self.at(at, Ev::Tick),
                Command::Settled { leg, outcome, .. } => self.settled.push((self.now, leg, outcome)),
                Command::Wait { leg, reason } => self.waits.push((self.now, leg, reason)),
                c @ (Command::StartRun { .. } | Command::CancelRun { .. }) => self.runs.push((self.now, c)),
                Command::Retry { .. } => {}
            }
        }
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

    /// When a leg was first assigned.
    pub fn started(&self, leg: &str) -> Option<i64> {
        self.assigned.iter().find(|a| a.leg == leg).map(|a| a.at)
    }

    /// The last wait reason a leg was given.
    pub fn last_wait(&self, leg: &str) -> Option<&WaitReason> {
        self.waits.iter().rev().find(|(_, l, _)| l == leg).map(|(_, _, r)| r)
    }
}
