//! The scheduler's model (docs/needs.md E1.6): kiln's CI engine (`kiln_ci::sched::Scheduler`) on
//! the server's node, and runners as nodes taking or refusing what it assigns by the runner's own
//! admission (`agent::admits`), with assignments, the runners' answers (started, refused) and
//! results as frames over kiln-sim's network. The model plays the shells around them as kiln's
//! hub and dispatcher do (`ci::dispatch`): each runner's connection to the server is an ordered
//! lane, which a lost frame breaks; a runner that hears nothing from the server for a while
//! connects again and says what it runs (`Hello.running`, its unacknowledged results among them);
//! the server takes a runner silent past its grace as lost, takes the legs a runner no longer has
//! as lost with it, has it stop the legs it runs that are no longer its own, and acknowledges each
//! result (`Received`), which the runner sends again on each connection until it is.
//!
//! The nemesis, until it stops: frames lost, each breaking its connection; frames late; links cut
//! (a partition); runners refusing what they're assigned for a while (an upgrade pending, as
//! kiln's runner refuses then). Results then come twice, and hellos again, as kiln's runners send
//! them.
//!
//! The checker: at every step, no runner holds more than its capacity (W05-R03, counted plainly
//! from the jobs it runs); once the nemesis has stopped, every leg settles, but one that waits, as
//! W05-R13 says it does, for another runner than the only one big enough to hold it, which refused
//! it: the model counts those (`stranded`). It's the skeleton E3 grows with leases and fencing:
//! without them, a runner cut off goes on with an attempt the engine has retried elsewhere, and an
//! answer about one attempt is taken as another's. The model counts those (`twice`, `stale`) and
//! doesn't fail on them yet.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kiln_ci::facts::{Cpu, Mem, RunnerFacts};
use kiln_ci::sched::retry::{Failed, FailureClass};
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::state::{Command, Config, Cpus, Fence, Incoming, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, WaitReason};
use kiln_sim::net::{Fate, Node};
use kiln_sim::{Faults, Net, Queue, Rng, Trace};

/// A runner says it's there this often.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// A runner that hears nothing from the server this long connects again.
const QUIET: Duration = Duration::from_secs(6);
/// The server takes a runner it hasn't heard from this long as lost (its reconnect grace).
const GRACE: Duration = Duration::from_secs(10);
/// How long the model runs at most after the nemesis stops: every leg settled by then.
const SETTLE: Duration = Duration::from_secs(60 * 60);
/// Most events one seed takes.
const STEPS_MAX: u64 = 1_000_000;
const GIB: u64 = 1 << 30;

/// What crosses a runner's connection, one way or the other.
#[derive(Debug, Clone)]
enum Frame {
    /// The server's: run this leg's attempt.
    Assign { leg: String, attempt: u32, holds: Incoming },
    /// The server's: stop it.
    Cancel { leg: String },
    /// The server's: it heard the runner (its hello, a heartbeat).
    Ack,
    /// The server's: it has the leg's result.
    Received { leg: String },
    /// The runner's, first on each connection: what it runs, and the results it holds.
    Hello { running: Vec<String> },
    Heartbeat,
    Started { leg: String, attempt: u32 },
    Refused { leg: String, attempt: u32 },
    Finished { leg: String, attempt: u32, outcome: Outcome },
}

#[derive(Debug)]
enum Ev {
    /// A frame arrives at a runner from the server, on the runner's connection `session`.
    ToRunner { runner: usize, session: u32, frame: Frame },
    /// A frame arrives at the server from a runner.
    ToServer { runner: usize, session: u32, frame: Frame },
    /// A runner's beat: a heartbeat, or a new connection when it has heard nothing for a while.
    Beat(usize),
    /// A runner's job ends.
    Ends { runner: usize, leg: String, attempt: u32 },
    /// The server looks for runners silent past their grace.
    Watch,
    Queued(Box<LegIn>),
    /// A timer the engine asked for.
    Tick,
    /// The nemesis cuts a runner off from the server.
    Cut(usize),
    /// A runner's upgrade is pending: it refuses what it's assigned for a while.
    Upgrade(usize),
}

/// What a run counted: across seeds, a fault that never happened tested nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub legs: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub assigned: u64,
    pub refused: u64,
    /// Connections a lost frame broke.
    pub broken: u64,
    pub reconnects: u64,
    /// Runners the server took as lost.
    pub lost: u64,
    pub partitions: u64,
    /// Results the server heard again.
    pub again: u64,
    /// Legs that ran on two runners at once (E3's to end).
    pub twice: u64,
    /// A runner's answer about one attempt taken as the answer of the leg's attempt now: the
    /// dispatcher names no attempt (E3's to fence).
    pub stale: u64,
    /// Legs left waiting for another runner than the only one big enough, which refused them
    /// (W05-R13's `other-runner`).
    pub stranded: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Self) {
        self.legs += o.legs;
        self.succeeded += o.succeeded;
        self.failed += o.failed;
        self.assigned += o.assigned;
        self.refused += o.refused;
        self.broken += o.broken;
        self.reconnects += o.reconnects;
        self.lost += o.lost;
        self.partitions += o.partitions;
        self.again += o.again;
        self.twice += o.twice;
        self.stale += o.stale;
        self.stranded += o.stranded;
    }
}

/// A runner node: its connection, and its jobs as it counts them.
struct Runner {
    node: Node,
    /// As the server knows it.
    input: RunnerIn,
    /// Its own size, as its admission counts it.
    size: Option<Resources>,
    /// Its connection now (0: none yet), and when it last heard the server on it.
    session: u32,
    heard: Duration,
    /// Each job it runs: its attempt, and what it holds of the runner.
    jobs: BTreeMap<String, (u32, Incoming)>,
    /// Results it holds until the server says it has them.
    unacked: BTreeMap<String, (u32, Outcome)>,
    /// Until when it refuses what it's assigned (an upgrade pending).
    refusing: Duration,
}

struct Sim {
    rng: Rng,
    net: Net,
    trace: Trace,
    queue: Queue<Ev>,
    now: Duration,
    until: Duration,
    server: Node,
    engine: Scheduler,
    runners: Vec<Runner>,
    /// The server's side of each runner's connection: its session and when it last heard it;
    /// `None` while it has none (never had one, or took the runner as lost).
    links: Vec<Option<(u32, Duration)>>,
    /// The legs queued, as queued.
    legs: BTreeMap<String, LegIn>,
    /// Each leg's runner and attempt, as the engine's commands said (no runner between attempts),
    /// and its last grant's fence, which the dispatcher names a refusal by.
    placed: BTreeMap<String, (Option<usize>, u32)>,
    fences: BTreeMap<String, Fence>,
    settled: BTreeMap<String, Outcome>,
    took: BTreeMap<String, Duration>,
    fails: BTreeSet<String>,
    /// Connections a lost frame broke: (runner, session).
    broken: BTreeSet<(usize, u32)>,
    counts: Counts,
    steps: u64,
}

fn runner_in(id: &str, slots: u32, threads: u32, mem: u64) -> RunnerIn {
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), cpu: Cpu { threads, cores: threads, ..Cpu::default() }, mem: Mem { total: mem, free: mem }, ..RunnerFacts::default() };
    RunnerIn { id: id.into(), view: RunnerView { name: id.into(), labels: vec!["kiln".into()], facts, caps: 0, pool: None }, slots, capacity: None, executor: None, draining: false }
}

/// What the runners' rule (W05-R03) lets a runner of `slots` and `size` hold, counted plainly:
/// one job holding it whole, alone; shares within its size, beside ordinary jobs taking a slot's
/// share each; ordinary jobs within the slots no share holds.
fn within(slots: u32, size: Option<Resources>, jobs: &[Incoming]) -> Result<(), String> {
    let (n, many) = (u128::from(slots.max(1)), jobs.len() > 1);
    if many && jobs.contains(&Incoming::Whole) {
        return Err("a job holding it whole runs beside others".into());
    }
    let shares: Vec<Resources> = jobs.iter().filter_map(|j| if let Incoming::Share(q) = j { Some(*q) } else { None }).collect();
    let ordinary = jobs.iter().filter(|j| **j == Incoming::Ordinary).count() as u128;
    let Some(s) = size else {
        return if !shares.is_empty() && many { Err("a share on a runner of no known size runs beside others".into()) } else if ordinary > n { Err(format!("{ordinary} jobs in {n} slots")) } else { Ok(()) };
    };
    let (cpu, mem) = shares.iter().fold((0u128, 0u128), |(c, m), q| (c + u128::from(q.cpu.get()), m + u128::from(q.mem)));
    let (scpu, smem) = (u128::from(s.cpu.get()), u128::from(s.mem));
    // The ordinary jobs take a slot's share each: ordinary / n of it.
    // Their memory rounds down, as the rule's does.
    if cpu * n + ordinary * scpu > scpu * n || mem * n + ordinary * smem > (smem * n + n).saturating_sub(1) {
        return Err(format!("shares of {cpu}m CPUs and {mem} bytes, {ordinary} ordinary jobs beside them, on {scpu}m and {smem} in {n} slots"));
    }
    let left = (n * scpu.saturating_sub(cpu)).checked_div(scpu).unwrap_or(n).min((n * smem.saturating_sub(mem)).checked_div(smem).unwrap_or(n));
    if !shares.is_empty() && ordinary > left {
        return Err(format!("{ordinary} ordinary jobs in the {left} slots the shares leave"));
    }
    if ordinary > n {
        return Err(format!("{ordinary} jobs in {n} slots"));
    }
    Ok(())
}

impl Sim {
    fn new(seed: u64, keep: usize) -> Sim {
        let rng = Rng::new(seed);
        let mut model = rng.stream("model");
        let until = model.span(Duration::from_secs(120), Duration::from_secs(480));
        let faults = Faults { drop: model.range(0, 60), dup: 0, slow: model.span(Duration::from_millis(50), Duration::from_secs(2)), fast: Duration::from_millis(50), until };
        let net = Net::new(faults, rng.stream("net"));
        let n = model.range(2, 4);
        let runners = (0..n)
            .map(|i| {
                let id = format!("r{i}");
                let slots = u32::try_from(model.range(1, 4)).unwrap_or(1);
                let sized = model.chance(700);
                let (threads, mem) = if sized { (u32::try_from(model.range(2, 16)).unwrap_or(2), model.range(4, 64) * GIB) } else { (0, 0) };
                let input = runner_in(&id, slots, threads, mem);
                let size = sized.then(|| Resources { cpu: Cpus::whole(u64::from(threads)), mem, ..Resources::default() });
                Runner { node: Node(u32::try_from(i).unwrap_or(0) + 1), input, size, session: 0, heard: Duration::ZERO, jobs: BTreeMap::new(), unacked: BTreeMap::new(), refusing: Duration::ZERO }
            })
            .collect::<Vec<_>>();
        let links = runners.iter().map(|_| None).collect();
        let mut sim = Sim {
            rng: rng.stream("events"),
            net,
            trace: Trace::new(seed, keep),
            queue: Queue::new(),
            now: Duration::ZERO,
            until,
            server: Node(0),
            engine: Scheduler::new(Config::default()),
            runners,
            links,
            legs: BTreeMap::new(),
            placed: BTreeMap::new(),
            fences: BTreeMap::new(),
            settled: BTreeMap::new(),
            took: BTreeMap::new(),
            fails: BTreeSet::new(),
            broken: BTreeSet::new(),
            counts: Counts::default(),
            steps: 0,
        };
        for r in 0..sim.runners.len() {
            let at = model.span(Duration::ZERO, HEARTBEAT);
            sim.queue.push(at, Ev::Beat(r));
        }
        sim.queue.push(Duration::from_secs(1), Ev::Watch);
        // Legs queued while the nemesis lasts, in runs of a few, over three repositories. Each asks
        // for what one of the runners could hold at least: one no runner could ever hold waits,
        // unroutable, for good (W05-R24), and settles never.
        let sizes: Vec<(u64, u64)> = sim.runners.iter().filter_map(|x| x.size).map(|s| (s.cpu.get() / 1000, s.mem / GIB)).collect();
        for i in 0..model.range(6, 24) {
            let at = model.span(Duration::ZERO, until);
            let (cpus, gib) = model.pick(&sizes).copied().unwrap_or((u64::MAX, u64::MAX));
            let request = match model.below(3) {
                0 => Resources::default(),
                _ => Resources { cpu: Cpus::whole(model.range(1, 4).min(cpus)), mem: model.range(1, 8).min(gib) * GIB, ..Resources::default() },
            };
            let took = model.span(Duration::from_secs(5), Duration::from_secs(90));
            let id = format!("l{i}");
            if model.chance(150) {
                sim.fails.insert(id.clone());
            }
            sim.took.insert(id.clone(), took);
            let leg = LegIn {
                id,
                run: format!("run{}", i / 3),
                repo: format!("acme/r{}", model.below(3)),
                route: LegRoute::default(),
                priority: [20u8, 60, 80, 100].get(model.index(4)).copied().unwrap_or(60),
                exclusive: model.chance(150),
                request,
                cost: u64::try_from(took.as_millis()).ok(),
                ..LegIn::default()
            };
            sim.queue.push(at, Ev::Queued(Box::new(leg)));
        }
        // Partitions: a runner cut off from the server for a while.
        for _ in 0..model.range(0, 4) {
            let at = model.span(Duration::ZERO, until);
            let r = model.index(sim.runners.len());
            sim.queue.push(at, Ev::Cut(r));
        }
        for _ in 0..model.range(0, 2) {
            let at = model.span(Duration::ZERO, until);
            let r = model.index(sim.runners.len());
            sim.queue.push(at, Ev::Upgrade(r));
        }
        sim
    }

    fn ms(&self) -> i64 {
        i64::try_from(self.now.as_millis()).unwrap_or(i64::MAX)
    }

    /// A step, folded into the digest and kept in words.
    fn step(&mut self, what: String) {
        self.fold(&what);
        let at = self.now;
        self.trace.note(|| format!("{:>9.3}s {what}", at.as_secs_f64()));
    }

    /// A step folded into the digest alone: heartbeats and their answers, which would crowd
    /// everything else out of the words.
    fn fold(&mut self, what: &str) {
        self.trace.fold(u64::try_from(self.now.as_nanos()).unwrap_or(u64::MAX));
        self.trace.fold_str(what);
    }

    /// A frame from the server to runner `r` on its connection `session`: lost (and the
    /// connection with it) or arriving in order.
    fn tell_runner(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(node) = self.runners.get(r).map(|x| x.node) else { return };
        if self.broken.contains(&(r, session)) {
            return;
        }
        match self.net.send_in_order(self.now, self.server, node, session) {
            Fate::Lost => self.breaks(r, session),
            fate => {
                for at in fate.arrivals() {
                    self.queue.push(at, Ev::ToRunner { runner: r, session, frame: frame.clone() });
                }
            }
        }
    }

    fn tell_server(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(node) = self.runners.get(r).map(|x| x.node) else { return };
        if self.broken.contains(&(r, session)) {
            return;
        }
        // Ordered lanes one way and the other: lanes are numbered by session, one set per side.
        match self.net.send_in_order(self.now, node, self.server, session) {
            Fate::Lost => self.breaks(r, session),
            fate => {
                for at in fate.arrivals() {
                    self.queue.push(at, Ev::ToServer { runner: r, session, frame: frame.clone() });
                }
            }
        }
    }

    fn breaks(&mut self, r: usize, session: u32) {
        if self.broken.insert((r, session)) {
            self.counts.broken += 1;
            self.step(format!("r{r}'s connection {session} broke"));
        }
    }

    /// The engine's commands, carried out as the dispatcher does.
    fn apply(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            self.step(format!("engine: {c:?}"));
            match c {
                Command::Assign { leg, runner, attempt, fence, .. } => {
                    let Some(r) = self.runners.iter().position(|x| x.input.id == runner) else { continue };
                    self.fences.insert(leg.clone(), fence);
                    self.counts.assigned += 1;
                    if self.placed.iter().any(|(l, (on, _))| *l == leg && on.is_some_and(|o| o != r)) || self.runners.iter().enumerate().any(|(i, x)| i != r && x.jobs.contains_key(&leg)) {
                        self.counts.twice += 1;
                    }
                    self.placed.insert(leg.clone(), (Some(r), attempt));
                    let holds = self.legs.get(&leg).map_or(Incoming::Ordinary, |l| Incoming::of(l.exclusive, &l.request));
                    if let Some(Some((session, _))) = self.links.get(r).copied() {
                        self.tell_runner(r, session, Frame::Assign { leg, attempt, holds });
                    }
                }
                Command::Cancel { leg, .. } => {
                    if let Some((Some(r), _)) = self.placed.get(&leg).copied()
                        && let Some(Some((session, _))) = self.links.get(r).copied()
                    {
                        self.tell_runner(r, session, Frame::Cancel { leg });
                    }
                }
                Command::Retry { leg, attempt, .. } => {
                    self.placed.insert(leg, (None, attempt));
                }
                Command::Settled { leg, outcome, .. } => {
                    self.placed.remove(&leg);
                    match outcome {
                        Outcome::Success => self.counts.succeeded += 1,
                        _ => self.counts.failed += 1,
                    }
                    self.settled.insert(leg, outcome);
                }
                Command::Timer { at } => {
                    let at = Duration::from_millis(u64::try_from(at).unwrap_or(0));
                    self.queue.push(at.max(self.now), Ev::Tick);
                }
                _ => {}
            }
        }
    }

    fn input(&mut self, i: Input) {
        let now = self.ms();
        let cmds = self.engine.handle(i, now);
        self.apply(cmds);
    }

    /// A frame at the server, as the hub and dispatcher take it.
    fn at_server(&mut self, r: usize, session: u32, frame: Frame) {
        let link = self.links.get(r).copied().flatten();
        if let Frame::Hello { running } = frame {
            if link.is_some_and(|(s, _)| session <= s) {
                return;
            }
            if let Some(l) = self.links.get_mut(r) {
                *l = Some((session, self.now));
            }
            // Legs it was running that it no longer has were lost; legs it runs that aren't its
            // own now (given up while it was away) it stops.
            let gone: Vec<(String, u32)> = self.placed.iter().filter(|(leg, (on, _))| *on == Some(r) && !running.contains(leg)).map(|(leg, (_, a))| (leg.clone(), *a)).collect();
            let stale: Vec<String> = running.iter().filter(|leg| self.legs.contains_key(*leg) && self.placed.get(*leg).map(|p| p.0) != Some(Some(r))).cloned().collect();
            for leg in stale {
                self.tell_runner(r, session, Frame::Cancel { leg });
            }
            let Some(input) = self.runners.get(r).map(|x| x.input.clone()) else { return };
            self.input(Input::RunnerUp(Box::new(input)));
            for (leg, attempt) in gone {
                let lost = Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() });
                self.input(Input::AttemptFinished { leg, attempt, outcome: lost });
            }
            self.tell_runner(r, session, Frame::Ack);
            return;
        }
        // A frame of a connection it no longer has, or never had.
        if link.map(|(s, _)| s) != Some(session) {
            return;
        }
        if let Some(Some((_, heard))) = self.links.get_mut(r) {
            *heard = self.now;
        }
        match frame {
            Frame::Heartbeat => self.tell_runner(r, session, Frame::Ack),
            Frame::Started { leg, attempt } => {
                if self.placed.get(&leg) != Some(&(Some(r), attempt)) {
                    self.counts.stale += 1;
                }
            }
            // As the dispatcher takes them, by leg alone: a refusal is the leg's now, and a result
            // is its attempt's now when the leg is this runner's.
            Frame::Refused { leg, attempt } => {
                if let Some(p) = self.placed.get_mut(&leg) {
                    if *p != (Some(r), attempt) {
                        self.counts.stale += 1;
                    }
                    p.0 = None;
                }
                let fence = self.fences.get(&leg).copied().unwrap_or_default();
                self.input(Input::Rejected { leg, fence });
            }
            Frame::Finished { leg, attempt, outcome } => {
                self.tell_runner(r, session, Frame::Received { leg: leg.clone() });
                match self.placed.get(&leg).copied() {
                    Some((Some(on), now)) if on == r => {
                        if now != attempt {
                            self.counts.stale += 1;
                        }
                        self.input(Input::AttemptFinished { leg, attempt: now, outcome });
                    }
                    _ => self.counts.again += 1,
                }
            }
            _ => {}
        }
    }

    /// A frame at runner `r`, as the runner takes it.
    fn at_runner(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(x) = self.runners.get_mut(r) else { return };
        if x.session != session {
            return;
        }
        x.heard = self.now;
        match frame {
            Frame::Received { leg } => {
                x.unacked.remove(&leg);
            }
            Frame::Cancel { leg } => {
                if let Some((attempt, _)) = x.jobs.remove(&leg) {
                    x.unacked.insert(leg.clone(), (attempt, Outcome::Cancelled));
                    self.tell_server(r, session, Frame::Finished { leg, attempt, outcome: Outcome::Cancelled });
                }
            }
            Frame::Assign { leg, attempt, holds } => {
                let room = crate::agent::admits(x.input.slots, x.size, x.jobs.values().map(|(_, h)| (false, *h)), false, holds);
                if x.jobs.contains_key(&leg) || room.is_err() || self.now < x.refusing {
                    self.counts.refused += 1;
                    self.tell_server(r, session, Frame::Refused { leg, attempt });
                    return;
                }
                x.jobs.insert(leg.clone(), (attempt, holds));
                let took = self.took.get(&leg).copied().unwrap_or(Duration::from_secs(30));
                self.queue.push(self.now.saturating_add(took), Ev::Ends { runner: r, leg: leg.clone(), attempt });
                self.tell_server(r, session, Frame::Started { leg, attempt });
            }
            _ => {}
        }
    }

    fn act(&mut self, ev: Ev) -> Result<(), String> {
        match ev {
            Ev::ToServer { runner, session, frame } => {
                let what = format!("server hears r{runner} on {session}: {frame:?}");
                if matches!(frame, Frame::Heartbeat) { self.fold(&what) } else { self.step(what) }
                self.at_server(runner, session, frame);
            }
            Ev::ToRunner { runner, session, frame } => {
                let what = format!("r{runner} hears the server on {session}: {frame:?}");
                if matches!(frame, Frame::Ack) { self.fold(&what) } else { self.step(what) }
                self.at_runner(runner, session, frame);
            }
            Ev::Beat(r) => {
                let Some(x) = self.runners.get_mut(r) else { return Ok(()) };
                if x.session == 0 || self.now.saturating_sub(x.heard) > QUIET {
                    // It connects again: its hello, then the results it holds.
                    x.session += 1;
                    x.heard = self.now;
                    let session = x.session;
                    let running: Vec<String> = x.jobs.keys().chain(x.unacked.keys().filter(|l| !x.jobs.contains_key(*l))).cloned().collect();
                    let results: Vec<(String, u32, Outcome)> = x.unacked.iter().map(|(l, (a, o))| (l.clone(), *a, o.clone())).collect();
                    if session > 1 {
                        self.counts.reconnects += 1;
                    }
                    self.step(format!("r{r} connects ({session}), running {running:?}"));
                    self.tell_server(r, session, Frame::Hello { running });
                    for (leg, attempt, outcome) in results {
                        self.tell_server(r, session, Frame::Finished { leg, attempt, outcome });
                    }
                } else {
                    let session = x.session;
                    self.tell_server(r, session, Frame::Heartbeat);
                }
                self.queue.push(self.now.saturating_add(HEARTBEAT), Ev::Beat(r));
            }
            Ev::Ends { runner, leg, attempt } => {
                let Some(x) = self.runners.get_mut(runner) else { return Ok(()) };
                if x.jobs.get(&leg).map(|(a, _)| *a) != Some(attempt) {
                    return Ok(());
                }
                x.jobs.remove(&leg);
                let outcome = if self.fails.contains(&leg) { Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: Some(1), log_hits: Vec::new() }) } else { Outcome::Success };
                x.unacked.insert(leg.clone(), (attempt, outcome.clone()));
                let session = x.session;
                self.step(format!("r{runner}: {leg}'s attempt {attempt} ends"));
                self.tell_server(runner, session, Frame::Finished { leg, attempt, outcome });
            }
            Ev::Watch => {
                let silent: Vec<usize> = self.links.iter().enumerate().filter(|(_, l)| l.is_some_and(|(_, heard)| self.now.saturating_sub(heard) > GRACE)).map(|(r, _)| r).collect();
                for r in silent {
                    if let Some(l) = self.links.get_mut(r) {
                        *l = None;
                    }
                    self.counts.lost += 1;
                    self.step(format!("server: r{r} silent past its grace, lost"));
                    let Some(id) = self.runners.get(r).map(|x| x.input.id.clone()) else { continue };
                    self.input(Input::RunnerLost { runner: id });
                }
                self.queue.push(self.now.saturating_add(Duration::from_secs(1)), Ev::Watch);
            }
            Ev::Queued(leg) => {
                self.counts.legs += 1;
                self.step(format!("{} queued", leg.id));
                self.legs.insert(leg.id.clone(), (*leg).clone());
                self.input(Input::LegQueued(leg));
            }
            Ev::Tick => self.input(Input::Tick),
            Ev::Upgrade(r) => {
                let Some(x) = self.runners.get_mut(r) else { return Ok(()) };
                x.refusing = self.now.saturating_add(self.rng.span(Duration::from_secs(5), Duration::from_secs(30)));
                let until = x.refusing;
                self.step(format!("r{r}'s upgrade pending: it refuses until {:.3}s", until.as_secs_f64()));
            }
            Ev::Cut(r) => {
                let Some(node) = self.runners.get(r).map(|x| x.node) else { return Ok(()) };
                let until = self.now.saturating_add(self.rng.span(Duration::from_secs(3), Duration::from_secs(30)));
                self.net.cut(self.server, node, until);
                self.counts.partitions += 1;
                self.step(format!("r{r} cut off until {:.3}s", until.as_secs_f64()));
            }
        }
        self.check()
    }

    /// At every step: no runner holds more than its capacity.
    fn check(&self) -> Result<(), String> {
        for x in &self.runners {
            let jobs: Vec<Incoming> = x.jobs.values().map(|(_, h)| *h).collect();
            within(x.input.slots, x.size, &jobs).map_err(|why| format!("{}: {why}: {:?}", x.input.id, x.jobs))?;
        }
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        let end = self.until.saturating_add(SETTLE);
        while let Some((at, ev)) = self.queue.pop() {
            self.steps += 1;
            if self.steps > STEPS_MAX {
                return Err(format!("more than {STEPS_MAX} steps"));
            }
            if at > end {
                break;
            }
            self.now = at;
            self.act(ev)?;
            if self.now > self.until && self.settled.len() == self.legs.len() && self.runners.iter().all(|x| x.jobs.is_empty() && x.unacked.is_empty()) {
                return Ok(());
            }
        }
        self.check_end()
    }

    /// Once the nemesis has stopped: every leg settled, but those left, as W05-R13 says, waiting
    /// for another runner than the only one big enough, which refused them.
    fn check_end(&mut self) -> Result<(), String> {
        let waiting: Vec<&String> = self.legs.keys().filter(|l| !self.settled.contains_key(*l)).collect();
        let stranded = waiting.iter().filter(|l| matches!(self.engine.reason(l), Some(WaitReason::OtherRunner { refused: true, .. }))).count();
        let open: Vec<String> = waiting
            .iter()
            .filter(|l| !matches!(self.engine.reason(l), Some(WaitReason::OtherRunner { refused: true, .. })))
            .map(|l| format!("{l} ({:?}, waits: {:?})", self.placed.get(*l), self.engine.reason(l)))
            .collect();
        self.counts.stranded += u64::try_from(stranded).unwrap_or(u64::MAX);
        if !open.is_empty() {
            return Err(format!("{:.0} s after the nemesis stopped, legs not settled: {}", SETTLE.as_secs_f64(), open.join(", ")));
        }
        Ok(())
    }
}

/// One seed's run: what it counted and its digest, or how it failed.
pub fn run(seed: u64, keep: usize) -> Result<(Counts, u64), String> {
    let mut sim = Sim::new(seed, keep);
    let ran = sim.run();
    let tail = if sim.trace.keeps() { format!("\n--- its last steps:\n{}", sim.trace.tail_text()) } else { String::new() };
    match ran {
        Ok(()) => {
            if !tail.is_empty() {
                eprintln!("seed {seed}: every leg settled{tail}");
            }
            Ok((sim.counts.clone(), sim.trace.digest()))
        }
        Err(why) => Err(format!("{why}{tail}")),
    }
}

#[cfg(test)]
mod tests {
    use kiln_sim::seeds;

    use super::*;

    /// docs/needs.md E1.6: the engine and runners over the seed budget, under the nemesis: no
    /// runner holds more than its capacity at any step, and every leg settles once the nemesis
    /// stops; and every fault happened.
    #[test]
    fn the_engine_and_runners_hold_their_capacity_and_settle_every_leg_under_the_nemesis() -> Result<(), String> {
        let mut all = Counts::default();
        let report = seeds::run_budget("sched", 200, |seed| {
            let keep = if seeds::replay().is_some() { 400 } else { 0 };
            let (counts, digest) = run(seed, keep)?;
            all.add(&counts);
            Ok(digest)
        });
        eprintln!("{}\n  counted: {all:?}", report.line());
        report.into_result()?;
        if seeds::replay().is_none() {
            let none = [("legs refused", all.refused), ("connections broken", all.broken), ("reconnects", all.reconnects), ("runners lost", all.lost), ("partitions", all.partitions), ("results heard again", all.again), ("failed legs", all.failed)];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// docs/sim.md's bug 10: the seeds where a leg lost with its runner waited for good for another
    /// runner too small to hold it (W05-R16), settled now.
    #[test]
    fn the_seeds_where_a_lost_leg_waited_for_a_runner_too_small_settle() -> Result<(), String> {
        for seed in [222, 284, 699, 902] {
            run(seed, 0).map_err(|why| format!("seed {seed}: {why}"))?;
        }
        Ok(())
    }

    /// docs/needs.md E1.6: a seed runs the same every time.
    #[test]
    fn a_sched_seed_replays_exactly() -> Result<(), String> {
        for seed in [3, 41, 4242] {
            seeds::twice(seed, |s| run(s, 0).map(|(_, d)| d))?;
        }
        Ok(())
    }

    /// The plain rule the checker holds runners to.
    #[test]
    fn the_checker_counts_what_a_runner_holds() {
        let size = Some(Resources { cpu: Cpus::whole(8), mem: 16 * GIB, ..Resources::default() });
        let share = Incoming::Share(Resources { cpu: Cpus::whole(4), mem: 8 * GIB, ..Resources::default() });
        let o = Incoming::Ordinary;
        assert!(within(4, size, &[o, o, o, o]).is_ok());
        assert!(within(4, size, &[o, o, o, o, o]).is_err());
        assert!(within(4, size, &[share, o, o]).is_ok());
        assert!(within(4, size, &[share, o, o, o]).is_err(), "the share leaves half the slots");
        assert!(within(4, size, &[Incoming::Whole]).is_ok());
        assert!(within(4, size, &[Incoming::Whole, o]).is_err());
        assert!(within(2, None, &[o, o]).is_ok());
        assert!(within(2, None, &[share, o]).is_err());
    }
}
