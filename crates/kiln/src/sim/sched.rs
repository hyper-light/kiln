//! The scheduler's model (docs/needs.md E1.6, E3.8): kiln's CI engine (`kiln_ci::sched::Scheduler`)
//! on the server's node, beside its store, and runners taking or refusing what it grants by the
//! runner's own admission (`agent::admits`) and keeping their leases by the runner's own keeper
//! (`runner::lease::Leases`), with grants, renewals, the runners' answers, writes and results as
//! frames over kiln-sim's network. The model plays the shells around them as kiln's hub and
//! dispatcher do (`ci::dispatch`):
//!
//! - Each grant is in the store before its `Assign` goes (G10), with its fence (the epoch it was
//!   made in, then its number) and a lease (W05-R05, R34). A runner renews what it holds with each
//!   heartbeat (`Renew`); the server renews each grant it names that's its own under that fence,
//!   says the rest lost, and tells it once to stop each of those (`Stop`). A grant not renewed by
//!   its expiry is lost; a stop item's next attempt waits the lock-delay past it.
//! - What a runner sends about a leg names its grant: a refusal, a write or a result is taken only
//!   under the leg's live grant, its runner's; the rest is refused, and its runner told once to
//!   stop that grant (W05-R36).
//! - A runner keeps each grant's lease by its own clock, which counts sleep, and stops a stop grant
//!   at its lease's end (W05-R37).
//! - Each runner's connection to the server is an ordered lane, which a lost frame breaks; a runner
//!   that hears nothing for a while connects again and says what it runs (`Hello.running`): the
//!   server takes what it no longer has as lost, and has it stop what it runs that isn't its own.
//! - The server's crash loses what it holds in memory (its engine, connections and grant table);
//!   the store keeps each leg: queued at its attempt, running under its grant, or settled. Started
//!   again, the server takes the next epoch and rebuilds as the dispatcher does (W05-R25): what the
//!   store has running comes back as the grant it ran under, with a lease from then, and the rest
//!   queued at the attempt the store has.
//!
//! The nemesis, until it stops: frames lost (each breaking its connection), doubled (what a runner
//! sends, and the server's answers; its commands its command channel applies once however often
//! they come) and late; links cut; the server crashing and starting again; runner processes paused,
//! often past their lease (Kleppmann's pause: what they'd do waits until they go on); runners'
//! machines asleep, their jobs with them, their lease clock counting the sleep; runners refusing
//! what they're sent while an upgrade is pending (and so draining) until it's done.
//!
//! The checker, at every step:
//! - capacity: no runner holds more than its capacity (W05-R03, counted plainly from its jobs);
//! - G2: no two grants share a fence, each greater than every one before; a leg is granted only
//!   while none of its grants is live; whatever the server takes (a refusal, a write, a result, a
//!   renewal) names its leg's live grant, from that grant's runner; a stop grant never acts (writes
//!   or ends) past its runner's lease end, nor after a later grant of its leg has acted;
//! - G10: a runner acts only on a grant the store has had; the engine holds each leg's grant, and
//!   its attempt, exactly as the store does, through every restart; an attempt that ended is never
//!   granted again.
//! - G4 (docs/needs.md E4.6): no gang runs in part, some members under a grant, others waiting.
//! - G6: an item that held one room kept for it until it started (W05-R39, backfilling on, among
//!   legs with p90s, big items wanting a runner whole and gangs) started by its bound: the room's
//!   time plus how far the grants it waited for ran past their expected ends.
//!
//! Once the nemesis has stopped, every leg settles, but one that waits, as W05-R13 says it does, for
//! another runner than the only one big enough to hold it, which refused it (`stranded`).
//! [`Mutation`]s plant, one at a time, the bugs the checks are there for.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kiln_ci::facts::{Cpu, Mem, RunnerFacts};
use kiln_ci::sched::retry::{Failed, FailureClass, RetryOn, RetryPolicy, RunnerChoice};
use kiln_ci::sched::routing::{LegRoute, RunnerView};
use kiln_ci::sched::state::{Command, Config, Cpus, DEFAULT_TIMEOUT_MS, Fence, Gang, Incoming, Input, LeaseLoss, LegIn, Loss, MUTANT, Mutant, Outcome, Resources, RunnerIn, Scheduler, WaitReason};
use kiln_sim::net::{Fate, Node};
use kiln_sim::{Faults, Net, Queue, Rng, Trace};

use crate::runner::lease::Leases;

/// A runner renews what it holds this often (its heartbeat).
const HEARTBEAT: Duration = Duration::from_secs(2);
/// A runner that hears nothing from the server this long connects again.
const QUIET: Duration = Duration::from_secs(6);
/// How long the model runs at most after the nemesis stops: every leg settled by then.
const SETTLE: Duration = Duration::from_secs(60 * 60);
/// Most events one seed takes.
const STEPS_MAX: u64 = 1_000_000;
const GIB: u64 = 1 << 30;

/// A bug planted in the model's kiln, one at a time: what the checks are for (docs/needs.md E3.8).
/// Each fails a seed of the 200 `cargo test` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// What a runner sends about a leg (a refusal, a write, a result) is taken by the leg alone, as
    /// its live grant's, whatever grant it names.
    WriteByLeg,
    /// A renewal renews every grant it names, whatever its attempt: none is said lost.
    RenewAll,
    /// A restart doesn't raise the epoch: its grants are numbered afresh in the old one.
    SameEpoch,
    /// A runner lets a stop grant run on past its lease's end.
    StopRunsOn,
    /// The engine grants a leg into room kept for another, past that room's time, where it doesn't
    /// fit beside it (docs/needs.md E4.6; kiln-ci's `mutants`).
    BackfillPast,
    /// The engine starts a gang without its last member.
    GangInPart,
}

/// A grant as frames name it: its leg, attempt and fence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grant {
    leg: String,
    attempt: u32,
    fence: Fence,
}

/// What crosses a runner's connection, one way or the other.
#[derive(Debug, Clone)]
enum Frame {
    /// The server's: run this grant, its lease `lease` ms, stopped at its end if `stop`.
    Assign { grant: Grant, holds: Incoming, lease: u64, stop: bool },
    /// The server's: stop this grant, if it's the one the runner runs of its leg.
    Stop { grant: Grant },
    /// The server's, to a runner back that runs a leg not its own now: stop it (by its name, as
    /// the dispatcher's reconciliation of a hello does).
    Cancel { leg: String },
    /// The server's answer to a hello.
    Welcome,
    /// The server's: it has the leg's result.
    Received { leg: String },
    /// The server's answer to `Renew` number `seq`: its lease (ms), its epoch, and the grants
    /// named that aren't the runner's.
    Renewed { seq: u64, lease: u64, epoch: u64, lost: Vec<Grant> },
    /// The runner's, first on each connection: the legs it runs, and those it holds results of.
    Hello { running: Vec<String> },
    /// The runner's heartbeat: the grants it holds.
    Renew { seq: u64, held: Vec<Grant> },
    /// It refuses the grant: no room, or `draining` (an upgrade pending).
    Refused { grant: Grant, draining: bool },
    /// A job's write under its grant (a step's status, its log).
    Wrote { grant: Grant },
    Finished { grant: Grant, outcome: Outcome },
}

impl Frame {
    /// One of the server's commands: its command channel applies each once, however often it
    /// goes (docs/wire.md), so the nemesis doesn't double them.
    fn command(&self) -> bool {
        matches!(self, Frame::Assign { .. } | Frame::Stop { .. } | Frame::Cancel { .. })
    }

    /// A runner's heartbeat, or an answer that says nothing lost: folded into the digest, not
    /// kept in words.
    fn quiet(&self) -> bool {
        match self {
            Frame::Renew { .. } | Frame::Welcome => true,
            Frame::Renewed { lost, .. } => lost.is_empty(),
            _ => false,
        }
    }
}

#[derive(Debug)]
enum Ev {
    /// A frame arrives at a runner from the server, on the runner's connection `session`.
    ToRunner { runner: usize, session: u32, frame: Frame },
    /// A frame arrives at the server from a runner.
    ToServer { runner: usize, session: u32, frame: Frame },
    /// A runner's beat: a renewal, or a new connection when it has heard nothing for a while.
    Beat(usize),
    /// A job's next act, once it's due: a write, or its end.
    Act { runner: usize, leg: String, fence: Fence },
    /// A runner looks at its leases (at the earliest end of one that stops).
    Leases(usize),
    /// The server looks for runners silent past their grace.
    Watch,
    Queued(Box<LegIn>),
    /// A timer the engine asked for.
    Tick,
    /// The nemesis cuts a runner off from the server.
    Cut(usize),
    /// A runner's upgrade is pending: it refuses what it's sent, draining, until it's done; then
    /// its new process says hello.
    Upgrade(usize),
    Upgraded(usize),
    /// The server crashes; it starts again.
    Crash,
    Restart,
    /// A runner's process pauses, or its machine sleeps, for a while; it goes on.
    Pause(usize),
    Sleep(usize),
    Wake(usize),
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
    /// Results the server heard again, or under a grant that wasn't live: refused.
    pub again: u64,
    /// Writes under a grant that wasn't live: refused (W05-R36).
    pub fenced: u64,
    /// Frames the nemesis doubled.
    pub doubled: u64,
    /// The server's crashes, and the grants its restarts brought back from the store.
    pub crashes: u64,
    pub adopted: u64,
    pub pauses: u64,
    pub sleeps: u64,
    /// Grants lost unrenewed at their lease's end (W05-R34).
    pub expired: u64,
    /// Stop grants a runner stopped at its lease's end (W05-R37).
    pub stopped: u64,
    /// Legs left waiting for another runner than the only one big enough, which refused them
    /// (W05-R13's `other-runner`).
    pub stranded: u64,
    /// Room kept for an item (W05-R39), each one the engine kept; and those that stood until their
    /// item started (G6's).
    pub reserved: u64,
    pub reserved_started: u64,
    /// Gangs started whole (W05-R40).
    pub gangs: u64,
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
        self.fenced += o.fenced;
        self.doubled += o.doubled;
        self.crashes += o.crashes;
        self.adopted += o.adopted;
        self.pauses += o.pauses;
        self.sleeps += o.sleeps;
        self.expired += o.expired;
        self.stopped += o.stopped;
        self.stranded += o.stranded;
        self.reserved += o.reserved;
        self.reserved_started += o.reserved_started;
        self.gangs += o.gangs;
    }
}

/// A job a runner runs: its grant, what it holds of the runner, whether it stops when its lease
/// ends; when it ends, and when its next act (a write, or its end) is due.
struct Job {
    grant: Grant,
    holds: Incoming,
    stop: bool,
    end: Duration,
    due: Duration,
}

/// A runner that doesn't go on until `until`: its process paused, or its machine asleep (its jobs
/// with it, and its connection gone).
#[derive(Debug, Clone, Copy)]
struct Frozen {
    until: Duration,
    asleep: bool,
}

/// A runner node: its connection, its jobs as it counts them, and its leases.
struct Runner {
    node: Node,
    /// As the server knows it.
    input: RunnerIn,
    /// Its own size, as its admission counts it.
    size: Option<Resources>,
    /// Its connection now (0: none yet), and when it last heard the server on it.
    session: u32,
    heard: Duration,
    /// Each job it runs, by leg.
    jobs: BTreeMap<String, Job>,
    /// Results it holds until the server says it has them, each under its grant.
    unacked: BTreeMap<String, (Grant, Outcome)>,
    /// Until when it refuses what it's sent (an upgrade pending).
    refusing: Duration,
    /// Its leases, by the runner's own keeper, on its lease clock: the model's time, which counts
    /// sleep as kiln's clock does.
    leases: Leases,
    renews: u64,
    frozen: Option<Frozen>,
    /// When its next look at its leases is due.
    look: Option<Duration>,
}

/// A grant the engine made (G6): its leg, fence and runner, when it started, its two ends as the
/// engine reckons them from its leg (W05-R38), and when the store took it to have ended.
struct Seen {
    leg: String,
    fence: Fence,
    runner: usize,
    start: i64,
    expected: i64,
    hard: i64,
    end: Option<i64>,
}

/// An item's stretch holding one room kept for it (W05-R39, R40): since `from`, from the room's
/// time `at`, on `runner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Holding {
    from: i64,
    at: i64,
    runner: usize,
}

/// What the store keeps of a leg, which a crash keeps: queued at an attempt, running under a grant
/// on a runner, or settled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kept {
    Queued { attempt: u32 },
    Running { runner: usize, grant: Grant },
    Settled,
}

/// The server's process: what a crash loses.
struct Server {
    engine: Scheduler,
    /// Its side of each runner's connection: its session and when it last heard it; `None` while
    /// it has none (never had one, took the runner as lost, or started since).
    links: Vec<Option<(u32, Duration)>>,
    /// The dispatcher's grant table: each leg's live grant, and its runner.
    grants: BTreeMap<String, (usize, Grant)>,
    /// Grants a runner was told to stop, each once.
    stopped: BTreeSet<(String, Fence)>,
}

struct Sim {
    rng: Rng,
    net: Net,
    trace: Trace,
    queue: Queue<Ev>,
    now: Duration,
    until: Duration,
    mutation: Option<Mutation>,
    /// The engine's config (its lease and lock-delay), the lease, and the hub's grace for a runner
    /// it doesn't hear: the seed's.
    cfg: Config,
    lease: Duration,
    grace: Duration,
    node: Node,
    /// The server's process while it's up.
    server: Option<Server>,
    /// The store: each leg as kept, and the last epoch taken.
    store: BTreeMap<String, Kept>,
    epoch: u64,
    runners: Vec<Runner>,
    /// The legs queued, as queued.
    legs: BTreeMap<String, LegIn>,
    took: BTreeMap<String, Duration>,
    fails: BTreeSet<String>,
    /// Connections broken: (runner, session).
    broken: BTreeSet<(usize, u32)>,
    /// What the checker holds kiln to: every fence granted (with its leg and attempt), every grant
    /// the store has had, the attempts that ended, and each stop leg's latest grant that acted.
    issued: BTreeMap<Fence, (String, u32)>,
    stored: BTreeSet<(String, Fence)>,
    ended: BTreeSet<(String, u32)>,
    acted: BTreeMap<String, Fence>,
    /// G6 and G4 (docs/needs.md E4.6): every grant made, as [`Seen`]; each item's stretch holding a
    /// room kept for it; the gangs running whole.
    seen: Vec<Seen>,
    holding: BTreeMap<String, Holding>,
    whole: BTreeSet<String>,
    /// The first thing the checker found wrong.
    failed: Option<String>,
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

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// How an attempt lost with its runner ends, and one its runner stopped for its lease (W05-R37).
fn runner_lost() -> Outcome {
    Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() })
}

impl Sim {
    fn new(seed: u64, keep: usize, mutation: Option<Mutation>) -> Sim {
        let rng = Rng::new(seed);
        let mut model = rng.stream("model");
        let until = model.span(Duration::from_secs(120), Duration::from_secs(480));
        let faults = Faults { drop: model.range(0, 60), dup: model.range(0, 60), slow: model.span(Duration::from_millis(50), Duration::from_secs(2)), fast: Duration::from_millis(50), until };
        let net = Net::new(faults, rng.stream("net"));
        // The lease, its lock-delay and the hub's grace, the seed's: the grace either side of the
        // lease, so a runner gone is lost with it in some seeds and its grants run out first in
        // others.
        let lease = model.span(Duration::from_secs(8), Duration::from_secs(30));
        let lock = model.span(Duration::from_secs(1), Duration::from_secs(10));
        let grace = model.span(Duration::from_secs(4), Duration::from_secs(40));
        // Room kept for big items and gangs (W05-R39, R40), as backfilling keeps it.
        let cfg = Config { lease: Some(ms(lease)), lock_delay: ms(lock), backfill: true, ..Config::default() };
        let n = model.range(2, 4);
        let runners = (0..n)
            .map(|i| {
                let id = format!("r{i}");
                let slots = u32::try_from(model.range(1, 4)).unwrap_or(1);
                let sized = model.chance(700);
                let (threads, mem) = if sized { (u32::try_from(model.range(2, 16)).unwrap_or(2), model.range(4, 64) * GIB) } else { (0, 0) };
                let input = runner_in(&id, slots, threads, mem);
                let size = sized.then(|| Resources { cpu: Cpus::whole(u64::from(threads)), mem, ..Resources::default() });
                let node = Node(u32::try_from(i).unwrap_or(0) + 1);
                Runner { node, input, size, session: 0, heard: Duration::ZERO, jobs: BTreeMap::new(), unacked: BTreeMap::new(), refusing: Duration::ZERO, leases: Leases::default(), renews: 0, frozen: None, look: None }
            })
            .collect::<Vec<_>>();
        let mut sim = Sim {
            rng: rng.stream("events"),
            net,
            trace: Trace::new(seed, keep),
            queue: Queue::new(),
            now: Duration::ZERO,
            until,
            mutation,
            cfg,
            lease,
            grace,
            node: Node(0),
            server: None,
            store: BTreeMap::new(),
            epoch: 0,
            runners,
            legs: BTreeMap::new(),
            took: BTreeMap::new(),
            fails: BTreeSet::new(),
            broken: BTreeSet::new(),
            issued: BTreeMap::new(),
            stored: BTreeSet::new(),
            ended: BTreeSet::new(),
            acted: BTreeMap::new(),
            seen: Vec::new(),
            holding: BTreeMap::new(),
            whole: BTreeSet::new(),
            failed: None,
            counts: Counts::default(),
            steps: 0,
        };
        sim.start();
        for r in 0..sim.runners.len() {
            let at = model.span(Duration::ZERO, HEARTBEAT);
            sim.queue.push(at, Ev::Beat(r));
        }
        sim.queue.push(Duration::from_secs(1), Ev::Watch);
        // Legs queued while the nemesis lasts, in runs of a few, over three repositories. Each asks
        // for what one of the runners could hold at least: one no runner could ever hold waits,
        // unroutable, for good (W05-R24), and settles never. A quarter stop when their lease is
        // lost, as deployments do (W05-R34); half retry a lost runner up to three times.
        let sizes: Vec<(u64, u64)> = sim.runners.iter().filter_map(|x| x.size).map(|s| (s.cpu.get() / 1000, s.mem / GIB)).collect();
        // What docs/needs.md E4.6 draws besides, from a stream of its own, so the draws above are as
        // they were: each leg's p90, from three fifths of how long it takes to a fifth past it (W05-R38).
        let mut more = rng.stream("big");
        let p90 = |more: &mut Rng, took: Duration| u64::try_from(more.span(took * 3 / 5, took * 6 / 5).as_millis()).ok();
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
            let on_lease_loss = if model.chance(250) { LeaseLoss::Stop } else { LeaseLoss::Finish };
            let max = u32::try_from(model.range(1, 3)).unwrap_or(1);
            let retry = model.chance(500).then(|| RetryPolicy { max, on: vec![RetryOn::Class(FailureClass::RunnerLost)], initial_ms: 0, factor: 1000, max_ms: 0, runner: RunnerChoice::Default });
            let leg = LegIn {
                id,
                run: format!("run{}", i / 3),
                repo: format!("acme/r{}", model.below(3)),
                route: LegRoute::default(),
                priority: [20u8, 60, 80, 100].get(model.index(4)).copied().unwrap_or(60),
                exclusive: model.chance(150),
                request,
                cost: u64::try_from(took.as_millis()).ok(),
                p90: p90(&mut more, took),
                retry,
                on_lease_loss,
                ..LegIn::default()
            };
            sim.queue.push(at, Ev::Queued(Box::new(leg)));
        }
        // Big items: a leg or two wanting a runner whole, among the rest (W05-R39).
        for i in 0..more.range(1, 2) {
            let at = more.span(Duration::ZERO, until);
            let took = more.span(Duration::from_secs(10), Duration::from_secs(90));
            let id = format!("b{i}");
            sim.took.insert(id.clone(), took);
            let leg = LegIn { id, run: format!("big{i}"), repo: format!("acme/r{}", more.below(3)), exclusive: true, cost: u64::try_from(took.as_millis()).ok(), p90: p90(&mut more, took), ..LegIn::default() };
            sim.queue.push(at, Ev::Queued(Box::new(leg)));
        }
        // Gangs of two or three, a job's legs queued together (W05-R40), as many as the runners
        // have slots at most: a gang no fleet could hold waits for good, as a leg no runner could
        // hold does.
        let slots: u64 = sim.runners.iter().map(|x| u64::from(x.input.slots)).sum();
        for g in 0..more.range(0, 2) {
            let (at, size) = (more.span(Duration::ZERO, until), more.range(2, 3).min(slots));
            let took = more.span(Duration::from_secs(10), Duration::from_secs(60));
            if size < 2 {
                continue;
            }
            let gang = Gang { id: format!("gang{g}"), size: u32::try_from(size).unwrap_or(2) };
            for m in 0..size {
                let id = format!("g{g}m{m}");
                sim.took.insert(id.clone(), took);
                let leg = LegIn { id, run: gang.id.clone(), repo: "acme/r0".into(), gang: Some(gang.clone()), cost: u64::try_from(took.as_millis()).ok(), p90: p90(&mut more, took), ..LegIn::default() };
                sim.queue.push(at, Ev::Queued(Box::new(leg)));
            }
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
        // The server crashing; runners pausing and sleeping.
        for _ in 0..model.range(0, 2) {
            let at = model.span(Duration::ZERO, until);
            sim.queue.push(at, Ev::Crash);
        }
        for _ in 0..model.range(0, 3) {
            let at = model.span(Duration::ZERO, until);
            let r = model.index(sim.runners.len());
            sim.queue.push(at, Ev::Pause(r));
        }
        for _ in 0..model.range(0, 2) {
            let at = model.span(Duration::ZERO, until);
            let r = model.index(sim.runners.len());
            sim.queue.push(at, Ev::Sleep(r));
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

    /// What the checker found, the first of it kept.
    fn fail(&mut self, why: String) {
        self.step(format!("CHECK FAILED: {why}"));
        if self.failed.is_none() {
            self.failed = Some(why);
        }
    }

    fn mutated(&self, m: Mutation) -> bool {
        self.mutation == Some(m)
    }

    /// The server starts, in the store's next epoch (the same one, mutated), and rebuilds what the
    /// store holds as the dispatcher does (W05-R25): each leg running comes back as the grant it
    /// ran under, with a lease from now; each queued, at the attempt the store has.
    fn start(&mut self) {
        if !self.mutated(Mutation::SameEpoch) || self.epoch == 0 {
            self.epoch += 1;
        }
        let engine = Scheduler::with_epoch(self.cfg.clone(), self.epoch);
        self.server = Some(Server { engine, links: self.runners.iter().map(|_| None).collect(), grants: BTreeMap::new(), stopped: BTreeSet::new() });
        let restart = self.counts.crashes > 0;
        self.step(format!("the server starts in epoch {}", self.epoch));
        let kept: Vec<(String, Kept)> = self.store.iter().map(|(l, k)| (l.clone(), k.clone())).collect();
        for (id, kept) in kept {
            let Some(mut li) = self.legs.get(&id).cloned() else { continue };
            match kept {
                Kept::Running { runner, grant } => {
                    let Some(rid) = self.runners.get(runner).map(|x| x.input.id.clone()) else { continue };
                    if restart {
                        self.counts.adopted += 1;
                    }
                    if let Some(s) = self.server.as_mut() {
                        s.grants.insert(id, (runner, grant.clone()));
                    }
                    self.input(Input::LegRunning { leg: Box::new(li), runner: rid, attempt: grant.attempt, fence: grant.fence });
                }
                Kept::Queued { attempt } => {
                    li.attempt = attempt;
                    self.input(Input::LegQueued(Box::new(li)));
                }
                Kept::Settled => {}
            }
        }
    }

    /// A frame from the server to runner `r` on its connection `session`: lost (and the
    /// connection with it) or arriving in order, perhaps twice.
    fn tell_runner(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(node) = self.runners.get(r).map(|x| x.node) else { return };
        if self.broken.contains(&(r, session)) {
            return;
        }
        match self.net.send_in_order(self.now, self.node, node, session) {
            Fate::Lost => self.breaks(r, session),
            fate => {
                let twice = self.doubles(&frame);
                for at in fate.arrivals() {
                    if twice {
                        self.queue.push(at, Ev::ToRunner { runner: r, session, frame: frame.clone() });
                    }
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
        // A server that's down takes nothing: the connection is gone.
        if self.server.is_none() {
            self.breaks(r, session);
            return;
        }
        // Ordered lanes one way and the other: lanes are numbered by session, one set per side.
        match self.net.send_in_order(self.now, node, self.node, session) {
            Fate::Lost => self.breaks(r, session),
            fate => {
                let twice = self.doubles(&frame);
                for at in fate.arrivals() {
                    if twice {
                        self.queue.push(at, Ev::ToServer { runner: r, session, frame: frame.clone() });
                    }
                    self.queue.push(at, Ev::ToServer { runner: r, session, frame: frame.clone() });
                }
            }
        }
    }

    /// Whether the nemesis doubles `frame` (never a command).
    fn doubles(&mut self, frame: &Frame) -> bool {
        let twice = !frame.command() && self.net.faulty(self.now) && self.rng.chance(self.net.faults().dup);
        if twice {
            self.counts.doubled += 1;
        }
        twice
    }

    fn breaks(&mut self, r: usize, session: u32) {
        if self.broken.insert((r, session)) {
            self.counts.broken += 1;
            self.step(format!("r{r}'s connection {session} broke"));
        }
    }

    fn rid(&self, r: usize) -> Option<String> {
        self.runners.get(r).map(|x| x.input.id.clone())
    }

    /// The engine's commands, carried out as the dispatcher does: each grant in the store before
    /// its `Assign` goes.
    fn apply(&mut self, cmds: Vec<Command>) {
        for c in cmds {
            self.step(format!("engine: {c:?}"));
            match c {
                Command::Assign { leg, runner, attempt, fence, lease_loss, .. } => self.grant(leg, &runner, attempt, fence, lease_loss),
                Command::Lost { leg, attempt, why, .. } => {
                    if why == Loss::Expired {
                        self.counts.expired += 1;
                    }
                    if let Some(s) = self.server.as_mut() {
                        s.grants.remove(&leg);
                    }
                    // Its attempt over in the store; its next (a `Retry`) or its end follows. One its
                    // gang took back goes again at the same attempt (W05-R40), as a refused one does.
                    if why != Loss::Gang {
                        self.ended.insert((leg.clone(), attempt));
                    }
                    self.store.insert(leg, Kept::Queued { attempt });
                }
                Command::Cancel { leg, .. } => {
                    let live = self.server.as_ref().and_then(|s| s.grants.get(&leg).cloned().and_then(|(r, g)| s.links.get(r).copied().flatten().map(|(session, _)| (r, session, g))));
                    if let Some((r, session, grant)) = live {
                        self.tell_runner(r, session, Frame::Stop { grant });
                    }
                }
                Command::Retry { leg, attempt, .. } => {
                    if let Some(s) = self.server.as_mut() {
                        s.grants.remove(&leg);
                    }
                    self.store.insert(leg, Kept::Queued { attempt });
                }
                Command::Settled { leg, outcome, .. } => {
                    if let Some(s) = self.server.as_mut() {
                        s.grants.remove(&leg);
                    }
                    match outcome {
                        Outcome::Success => self.counts.succeeded += 1,
                        _ => self.counts.failed += 1,
                    }
                    self.store.insert(leg, Kept::Settled);
                }
                Command::Timer { at } => {
                    let at = Duration::from_millis(u64::try_from(at).unwrap_or(0));
                    self.queue.push(at.max(self.now), Ev::Tick);
                }
                _ => {}
            }
        }
    }

    /// A grant the engine made: checked (G2, G10), put in the store, then sent.
    fn grant(&mut self, leg: String, runner: &str, attempt: u32, fence: Fence, loss: LeaseLoss) {
        let Some(r) = self.runners.iter().position(|x| x.input.id == runner) else { return };
        self.counts.assigned += 1;
        if let Some((f, (l, a))) = self.issued.last_key_value().filter(|(f, _)| **f >= fence) {
            let why = format!("G2: {leg}'s attempt {attempt} granted under {fence:?}, not greater than {f:?}, {l}'s attempt {a}'s");
            self.fail(why);
        }
        if let Some(Kept::Running { runner: on, grant }) = self.store.get(&leg).cloned() {
            self.fail(format!("G2: {leg} granted to r{r} under {fence:?} while its grant {grant:?} on r{on} is live"));
        }
        if self.ended.contains(&(leg.clone(), attempt)) {
            self.fail(format!("G10: {leg}'s attempt {attempt} granted again (under {fence:?}), though it ended"));
        }
        self.issued.insert(fence, (leg.clone(), attempt));
        // Its two ends as the engine reckons them (W05-R38), for G6.
        let (now, input) = (self.ms(), self.legs.get(&leg));
        let ms_of = |m: u64| i64::try_from(m).unwrap_or(i64::MAX / 4);
        let hard = now.saturating_add(ms_of(input.and_then(|l| l.timeout).unwrap_or(DEFAULT_TIMEOUT_MS)));
        let expected = input.and_then(|l| l.p90.or(l.cost)).map_or(hard, |m| now.saturating_add(ms_of(m)).min(hard));
        self.seen.push(Seen { leg: leg.clone(), fence, runner: r, start: now, expected, hard, end: None });
        let grant = Grant { leg: leg.clone(), attempt, fence };
        self.store.insert(leg.clone(), Kept::Running { runner: r, grant: grant.clone() });
        self.stored.insert((leg.clone(), fence));
        let holds = self.legs.get(&leg).map_or(Incoming::Ordinary, |l| Incoming::of(l.exclusive, &l.request));
        let Some(s) = self.server.as_mut() else { return };
        s.grants.insert(leg, (r, grant.clone()));
        if let Some(Some((session, _))) = s.links.get(r).copied() {
            let lease = ms(self.lease);
            self.tell_runner(r, session, Frame::Assign { grant, holds, lease, stop: loss == LeaseLoss::Stop });
        }
    }

    fn input(&mut self, i: Input) {
        let now = self.ms();
        let Some(s) = self.server.as_mut() else { return };
        let cmds = s.engine.handle(i, now);
        self.apply(cmds);
    }

    /// Whether the server takes what runner `r` sends under `grant`: only under its leg's live
    /// grant, `r`'s (W05-R36); mutated, by the leg alone.
    fn takes(&self, r: usize, grant: &Grant) -> bool {
        let Some((on, live)) = self.server.as_ref().and_then(|s| s.grants.get(&grant.leg)) else { return false };
        self.mutated(Mutation::WriteByLeg) || (*on == r && live == grant)
    }

    /// G2: what the server took from runner `r`, `what`, names its leg's live grant, `r`'s, as the
    /// store has it.
    fn taken(&mut self, r: usize, grant: &Grant, what: &str) {
        match self.store.get(&grant.leg) {
            Some(Kept::Running { runner, grant: live }) if *runner == r && live == grant => {}
            kept => {
                let why = format!("G2: the server took {what} from r{r} under {grant:?}; its leg's live grant: {kept:?}");
                self.fail(why);
            }
        }
    }

    /// What runner `r` sent under a grant that isn't its leg's live one: refused, and `r` told,
    /// once a grant, to stop it (W05-R36).
    fn refuse(&mut self, r: usize, session: u32, grant: Grant) {
        let Some(s) = self.server.as_mut() else { return };
        if s.stopped.insert((grant.leg.clone(), grant.fence)) {
            self.tell_runner(r, session, Frame::Stop { grant });
        }
    }

    /// `gone`'s attempts lost with their runner, as the dispatcher loses them: over in the store,
    /// then the engine told. One that ended already, as one before it took its gang with it (W05-R40),
    /// is left as it is, as the store's attempt, ended, takes no end again.
    fn lose(&mut self, gone: Vec<(String, u32)>) {
        for (leg, attempt) in gone {
            if !matches!(self.store.get(&leg), Some(Kept::Running { grant, .. }) if grant.attempt == attempt) {
                continue;
            }
            if let Some(s) = self.server.as_mut() {
                s.grants.remove(&leg);
            }
            self.ended.insert((leg.clone(), attempt));
            self.store.insert(leg.clone(), Kept::Queued { attempt });
            self.input(Input::AttemptFinished { leg, attempt, outcome: runner_lost() });
        }
    }

    /// A frame at the server, as the hub and dispatcher take it.
    fn at_server(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(link) = self.server.as_ref().map(|s| s.links.get(r).copied().flatten()) else { return };
        if let Frame::Hello { running } = frame {
            if link.is_some_and(|(s, _)| session <= s) {
                return;
            }
            let Some(s) = self.server.as_mut() else { return };
            if let Some(l) = s.links.get_mut(r) {
                *l = Some((session, self.now));
            }
            // Legs it was running that it no longer has were lost; legs it runs that aren't its
            // own now it stops (by name, as the dispatcher's reconciliation does).
            let gone: Vec<(String, u32)> = s.grants.iter().filter(|(leg, (on, _))| *on == r && !running.contains(leg)).map(|(leg, (_, g))| (leg.clone(), g.attempt)).collect();
            let stale: Vec<String> = running.iter().filter(|leg| self.legs.contains_key(*leg) && s.grants.get(*leg).map(|(on, _)| *on) != Some(r)).cloned().collect();
            for leg in stale {
                self.tell_runner(r, session, Frame::Cancel { leg });
            }
            let Some(input) = self.runners.get(r).map(|x| x.input.clone()) else { return };
            self.input(Input::RunnerUp(Box::new(input)));
            self.lose(gone);
            self.tell_runner(r, session, Frame::Welcome);
            return;
        }
        // A frame of a connection it no longer has, or never had.
        if link.map(|(s, _)| s) != Some(session) {
            return;
        }
        if let Some(Some((_, heard))) = self.server.as_mut().and_then(|s| s.links.get_mut(r)) {
            *heard = self.now;
        }
        let Some(rid) = self.rid(r) else { return };
        match frame {
            Frame::Renew { seq, held } => self.renew(r, session, seq, held),
            Frame::Refused { grant, draining } => {
                if !self.takes(r, &grant) {
                    return;
                }
                self.taken(r, &grant, "a refusal");
                // Refused draining (an upgrade pending): the runner drains on its own (W05-R35).
                if draining {
                    self.input(Input::RunnerDraining { runner: rid });
                }
                let Some((_, live)) = self.server.as_mut().and_then(|s| s.grants.remove(&grant.leg)) else { return };
                self.store.insert(live.leg.clone(), Kept::Queued { attempt: live.attempt });
                self.input(Input::Rejected { leg: live.leg, fence: live.fence });
            }
            Frame::Wrote { grant } => {
                if self.takes(r, &grant) {
                    self.taken(r, &grant, "a write");
                } else {
                    self.counts.fenced += 1;
                    self.refuse(r, session, grant);
                }
            }
            Frame::Finished { grant, outcome } => {
                self.tell_runner(r, session, Frame::Received { leg: grant.leg.clone() });
                if !self.takes(r, &grant) {
                    self.counts.again += 1;
                    self.refuse(r, session, grant);
                    return;
                }
                self.taken(r, &grant, "a result");
                let Some((_, live)) = self.server.as_mut().and_then(|s| s.grants.remove(&grant.leg)) else { return };
                self.ended.insert((live.leg.clone(), live.attempt));
                self.input(Input::AttemptFinished { leg: live.leg, attempt: live.attempt, outcome });
            }
            _ => {}
        }
    }

    /// Runner `r`'s `Renew`: each grant it names that's its own under that fence lives a lease
    /// from now (W05-R34); the answer says the rest lost, and it's told once to stop each.
    /// Mutated, every grant it names is renewed: its leg's live grant, whatever attempt it names.
    fn renew(&mut self, r: usize, session: u32, seq: u64, held: Vec<Grant>) {
        let Some(s) = self.server.as_ref() else { return };
        let all = self.mutated(Mutation::RenewAll);
        let (mut named, mut lost) = (Vec::new(), Vec::new());
        for g in held {
            match s.grants.get(&g.leg) {
                Some((on, live)) if *on == r && (all || *live == g) => named.push((g, live.fence)),
                _ if all => named.push((g, Fence::default())),
                _ => lost.push(g),
            }
        }
        let epoch = s.engine.epoch();
        for (g, _) in &named {
            self.taken(r, g, "a renewal");
        }
        let held: Vec<(String, Fence)> = named.into_iter().filter(|(_, f)| *f != Fence::default()).map(|(g, f)| (g.leg, f)).collect();
        if !held.is_empty()
            && let Some(rid) = self.rid(r)
        {
            self.input(Input::Renewed { runner: rid, held });
        }
        for g in &lost {
            self.refuse(r, session, g.clone());
        }
        let lease = ms(self.lease);
        self.tell_runner(r, session, Frame::Renewed { seq, lease, epoch, lost });
    }

    /// A frame at runner `r`, as the runner takes it.
    fn at_runner(&mut self, r: usize, session: u32, frame: Frame) {
        let Some(x) = self.runners.get_mut(r) else { return };
        if x.session != session {
            return;
        }
        x.heard = self.now;
        match frame {
            Frame::Welcome => {}
            Frame::Received { leg } => {
                x.unacked.remove(&leg);
            }
            Frame::Renewed { seq, lease, epoch, lost } => {
                let lost: Vec<(String, Fence)> = lost.into_iter().map(|g| (g.leg, g.fence)).collect();
                x.leases.renewed(seq, lease, epoch, &lost);
                self.look(r);
            }
            // From an epoch older than the newest it has seen: not applied (W05-R37).
            Frame::Stop { grant } if x.leases.stale(grant.fence.epoch) => {}
            Frame::Stop { grant } => {
                if x.jobs.get(&grant.leg).is_some_and(|j| j.grant == grant) {
                    self.stop(r, &grant.leg, Outcome::Cancelled);
                }
            }
            Frame::Cancel { leg } => self.stop(r, &leg, Outcome::Cancelled),
            Frame::Assign { grant, .. } if x.leases.stale(grant.fence.epoch) => {}
            Frame::Assign { grant, holds, lease, stop } => {
                let room = crate::agent::admits(x.input.slots, x.size, x.jobs.values().map(|j| (false, j.holds)), false, holds);
                let upgrading = self.now < x.refusing;
                if x.jobs.contains_key(&grant.leg) || room.is_err() || upgrading {
                    self.counts.refused += 1;
                    self.tell_server(r, session, Frame::Refused { grant, draining: upgrading });
                    return;
                }
                x.leases.granted(&grant.leg, grant.fence, lease, stop);
                x.unacked.remove(&grant.leg);
                let took = self.took.get(&grant.leg).copied().unwrap_or(Duration::from_secs(30));
                let end = self.now.saturating_add(took);
                let due = self.now.saturating_add(self.rng.span(Duration::from_secs(1), Duration::from_secs(10))).min(end);
                let (leg, fence) = (grant.leg.clone(), grant.fence);
                // G10: a runner acts only on a grant the store has had.
                let stored = self.stored.contains(&(leg.clone(), fence));
                let Some(x) = self.runners.get_mut(r) else { return };
                x.jobs.insert(leg.clone(), Job { grant, holds, stop, end, due });
                self.queue.push(due, Ev::Act { runner: r, leg: leg.clone(), fence });
                if !stored {
                    self.fail(format!("G10: r{r} runs {leg}'s grant {fence:?}, which the store never had"));
                }
                self.look(r);
            }
            _ => {}
        }
    }

    /// Runner `r` stops its job of `leg`: its result, `outcome`, held until the server has it, and
    /// sent.
    fn stop(&mut self, r: usize, leg: &str, outcome: Outcome) {
        let Some(x) = self.runners.get_mut(r) else { return };
        let Some(job) = x.jobs.remove(leg) else { return };
        x.leases.ended(leg);
        x.unacked.insert(leg.to_owned(), (job.grant.clone(), outcome.clone()));
        let session = x.session;
        self.tell_server(r, session, Frame::Finished { grant: job.grant, outcome });
    }

    /// Runner `r` looks at its leases, by its clock: each stop grant whose lease has ended it stops,
    /// as lost with its runner (W05-R37). Its next look is at the earliest end of those left.
    fn look(&mut self, r: usize) {
        let now = ms(self.now);
        let Some(x) = self.runners.get_mut(r) else { return };
        if x.frozen.is_some() {
            return;
        }
        let due = x.leases.due(now);
        if !self.mutated(Mutation::StopRunsOn) {
            for leg in due {
                if self.runners.get(r).is_some_and(|x| x.jobs.contains_key(&leg)) {
                    self.counts.stopped += 1;
                    self.step(format!("r{r}: {leg}'s lease ended at {now} ms by its clock: stopped"));
                    self.stop(r, &leg, runner_lost());
                }
            }
        }
        let Some(x) = self.runners.get_mut(r) else { return };
        let next = x.jobs.iter().filter(|(_, j)| j.stop).filter_map(|(leg, _)| x.leases.end(leg)).filter(|end| *end > now).min().map(Duration::from_millis);
        if let Some(at) = next
            && x.look.is_none_or(|l| l <= self.now || at < l)
        {
            x.look = Some(at);
            self.queue.push(at, Ev::Leases(r));
        }
    }

    /// Runner `r` connects (again): a new connection, its leases' base its hello's sending; its
    /// hello, then the results it holds.
    fn connect(&mut self, r: usize) {
        let now = ms(self.now);
        let Some(x) = self.runners.get_mut(r) else { return };
        x.session += 1;
        x.heard = self.now;
        x.leases.away();
        x.leases.connected(now);
        let session = x.session;
        let running: Vec<String> = x.jobs.keys().chain(x.unacked.keys().filter(|l| !x.jobs.contains_key(*l))).cloned().collect();
        let results: Vec<(Grant, Outcome)> = x.unacked.values().cloned().collect();
        if session > 1 {
            self.counts.reconnects += 1;
        }
        self.step(format!("r{r} connects ({session}), running {running:?}"));
        self.tell_server(r, session, Frame::Hello { running });
        for (grant, outcome) in results {
            self.tell_server(r, session, Frame::Finished { grant, outcome });
        }
    }

    /// A stop grant acts (writes, or ends): G2 holds it to its runner's lease, and to its leg's
    /// order of grants.
    fn acts(&mut self, r: usize, grant: &Grant) {
        let now = ms(self.now);
        let end = self.runners.get(r).and_then(|x| x.leases.end(&grant.leg));
        if let Some(end) = end.filter(|e| now > *e) {
            self.fail(format!("G2: r{r}'s stop grant {grant:?} acts at {now} ms, past its lease's end at {end} ms by its clock"));
        }
        match self.acted.get(&grant.leg).copied() {
            Some(later) if later > grant.fence => self.fail(format!("G2: r{r}'s stop grant {grant:?} acts after {later:?}, a later grant of its leg, has")),
            _ => {
                self.acted.insert(grant.leg.clone(), grant.fence);
            }
        }
    }

    /// The runner an event is for, if it's frozen: what it'd do waits until it goes on (its
    /// `Wake`, which comes first of what's due then).
    fn frozen(&self, ev: &Ev) -> Option<Frozen> {
        let r = match ev {
            Ev::ToRunner { runner, .. } | Ev::Act { runner, .. } => *runner,
            Ev::Beat(r) | Ev::Leases(r) | Ev::Upgraded(r) => *r,
            _ => return None,
        };
        self.runners.get(r).and_then(|x| x.frozen)
    }

    fn act(&mut self, ev: Ev) -> Result<(), String> {
        if let Some(f) = self.frozen(&ev) {
            match &ev {
                // Asleep, its machine is off the network: what reaches it is lost.
                Ev::ToRunner { .. } if f.asleep => return Ok(()),
                Ev::Act { runner, leg, fence } => {
                    let now = self.now;
                    let Some(job) = self.runners.get_mut(*runner).and_then(|x| x.jobs.get_mut(leg)).filter(|j| j.grant.fence == *fence && j.due == now) else { return Ok(()) };
                    job.due = f.until;
                }
                _ => {}
            }
            self.queue.push(f.until, ev);
            return Ok(());
        }
        match ev {
            Ev::ToServer { runner, session, frame } => {
                let what = format!("server hears r{runner} on {session}: {frame:?}");
                if frame.quiet() { self.fold(&what) } else { self.step(what) }
                self.at_server(runner, session, frame);
            }
            Ev::ToRunner { runner, session, frame } => {
                let what = format!("r{runner} hears the server on {session}: {frame:?}");
                if frame.quiet() { self.fold(&what) } else { self.step(what) }
                self.at_runner(runner, session, frame);
            }
            Ev::Beat(r) => {
                let now = ms(self.now);
                let Some(x) = self.runners.get_mut(r) else { return Ok(()) };
                if x.session == 0 || self.now.saturating_sub(x.heard) > QUIET {
                    self.connect(r);
                } else {
                    // It renews what it holds: its jobs' grants, and those of results not yet heard.
                    x.renews += 1;
                    let seq = x.renews;
                    let held: Vec<Grant> = x.jobs.values().map(|j| j.grant.clone()).chain(x.unacked.iter().filter(|(l, _)| !x.jobs.contains_key(*l)).map(|(_, (g, _))| g.clone())).collect();
                    x.leases.renew_sent(seq, now, held.iter().map(|g| (g.leg.clone(), g.fence)).collect());
                    let session = x.session;
                    self.tell_server(r, session, Frame::Renew { seq, held });
                }
                self.queue.push(self.now.saturating_add(HEARTBEAT), Ev::Beat(r));
            }
            Ev::Act { runner, leg, fence } => {
                let now = self.now;
                let Some(job) = self.runners.get(runner).and_then(|x| x.jobs.get(&leg)).filter(|j| j.grant.fence == fence && j.due == now) else { return Ok(()) };
                let (grant, stop, end) = (job.grant.clone(), job.stop, job.end);
                if stop {
                    self.acts(runner, &grant);
                }
                if now >= end {
                    let outcome = if self.fails.contains(&leg) { Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: Some(1), log_hits: Vec::new() }) } else { Outcome::Success };
                    self.step(format!("r{runner}: {leg}'s attempt {} ends", grant.attempt));
                    self.stop(runner, &leg, outcome);
                } else {
                    let write = now.saturating_add(self.rng.span(Duration::from_secs(1), Duration::from_secs(10)));
                    let Some(x) = self.runners.get_mut(runner) else { return Ok(()) };
                    let session = x.session;
                    if let Some(job) = x.jobs.get_mut(&leg) {
                        job.due = write.min(end);
                        self.queue.push(job.due, Ev::Act { runner, leg, fence });
                    }
                    self.tell_server(runner, session, Frame::Wrote { grant });
                }
            }
            Ev::Leases(r) => {
                if let Some(x) = self.runners.get_mut(r).filter(|x| x.look == Some(self.now)) {
                    x.look = None;
                }
                self.look(r);
            }
            Ev::Watch => {
                let (now, grace) = (self.now, self.grace);
                let silent: Vec<usize> = self.server.as_ref().map(|s| s.links.iter().enumerate().filter(|(_, l)| l.is_some_and(|(_, heard)| now.saturating_sub(heard) > grace)).map(|(r, _)| r).collect()).unwrap_or_default();
                for r in silent {
                    if let Some(l) = self.server.as_mut().and_then(|s| s.links.get_mut(r)) {
                        *l = None;
                    }
                    self.counts.lost += 1;
                    self.step(format!("server: r{r} silent past its grace, lost"));
                    let Some(id) = self.rid(r) else { continue };
                    self.input(Input::RunnerLost { runner: id });
                }
                self.queue.push(self.now.saturating_add(Duration::from_secs(1)), Ev::Watch);
            }
            Ev::Queued(leg) => {
                // A run starts on a server that's up.
                if self.server.is_none() {
                    self.queue.push(self.now.saturating_add(Duration::from_secs(1)), Ev::Queued(leg));
                    return Ok(());
                }
                self.counts.legs += 1;
                self.step(format!("{} queued", leg.id));
                self.legs.insert(leg.id.clone(), (*leg).clone());
                self.store.insert(leg.id.clone(), Kept::Queued { attempt: 1 });
                self.input(Input::LegQueued(leg));
            }
            Ev::Tick => self.input(Input::Tick),
            Ev::Upgrade(r) => {
                let Some(x) = self.runners.get_mut(r) else { return Ok(()) };
                x.refusing = self.now.saturating_add(self.rng.span(Duration::from_secs(5), Duration::from_secs(30)));
                let until = x.refusing;
                self.queue.push(until, Ev::Upgraded(r));
                self.step(format!("r{r}'s upgrade pending: it refuses until {:.3}s", until.as_secs_f64()));
            }
            // Its new process says hello: it takes work again (W05-R35).
            Ev::Upgraded(r) => self.connect(r),
            Ev::Cut(r) => {
                let Some(node) = self.runners.get(r).map(|x| x.node) else { return Ok(()) };
                let until = self.now.saturating_add(self.rng.span(Duration::from_secs(3), Duration::from_secs(30)));
                self.net.cut(self.node, node, until);
                self.counts.partitions += 1;
                self.step(format!("r{r} cut off until {:.3}s", until.as_secs_f64()));
            }
            Ev::Crash => {
                if self.server.take().is_none() {
                    return Ok(());
                }
                self.counts.crashes += 1;
                // Room kept goes with it: a restart works it out again (W05-R39), the bound with it.
                self.holding.clear();
                // What was on its way to it, and its timers, go with it; and every connection to it.
                self.queue.retain(|e| !matches!(e, Ev::ToServer { .. } | Ev::Tick));
                let sessions: Vec<(usize, u32)> = self.runners.iter().enumerate().map(|(r, x)| (r, x.session)).collect();
                self.broken.extend(sessions);
                let down = self.rng.span(Duration::from_secs(1), Duration::from_secs(20));
                self.queue.push(self.now.saturating_add(down), Ev::Restart);
                self.step(format!("the server crashes, down {:.3}s", down.as_secs_f64()));
            }
            Ev::Restart => self.start(),
            Ev::Pause(r) | Ev::Sleep(r) => {
                let asleep = matches!(ev, Ev::Sleep(_));
                let d = if asleep { self.rng.span(Duration::from_secs(10), Duration::from_secs(300)) } else { self.rng.span(Duration::from_secs(2), Duration::from_secs(60)) };
                let until = self.now.saturating_add(d);
                let Some(x) = self.runners.get_mut(r).filter(|x| x.frozen.is_none()) else { return Ok(()) };
                x.frozen = Some(Frozen { until, asleep });
                let session = x.session;
                self.queue.push(until, Ev::Wake(r));
                if asleep {
                    // Its jobs sleep with it, their times moved on by the sleep; its connection goes.
                    let Some(x) = self.runners.get_mut(r) else { return Ok(()) };
                    let mut acts = Vec::new();
                    for (leg, job) in x.jobs.iter_mut() {
                        job.end = job.end.saturating_add(d);
                        job.due = job.due.saturating_add(d);
                        acts.push((job.due, leg.clone(), job.grant.fence));
                    }
                    for (at, leg, fence) in acts {
                        self.queue.push(at, Ev::Act { runner: r, leg, fence });
                    }
                    self.breaks(r, session);
                    self.counts.sleeps += 1;
                } else {
                    self.counts.pauses += 1;
                }
                self.step(format!("r{r} {} until {:.3}s", if asleep { "sleeps" } else { "pauses" }, until.as_secs_f64()));
            }
            // It goes on, and looks at its leases first, by its clock, which counted the time it
            // was stopped (W05-R37).
            Ev::Wake(r) => {
                if let Some(x) = self.runners.get_mut(r) {
                    x.frozen = None;
                }
                self.step(format!("r{r} goes on"));
                self.look(r);
            }
        }
        self.check()
    }

    /// At every step: no runner holds more than its capacity (W05-R03); the engine holds each leg's
    /// grant and attempt exactly as the store does (G10); G4 and G6 (docs/needs.md E4.6:
    /// [`Self::gangs_and_room`]); and nothing the checker found on the way.
    fn check(&mut self) -> Result<(), String> {
        if let Some(why) = self.failed.take() {
            return Err(why);
        }
        self.gangs_and_room()?;
        for x in &self.runners {
            let jobs: Vec<Incoming> = x.jobs.values().map(|j| j.holds).collect();
            within(x.input.slots, x.size, &jobs).map_err(|why| format!("{}: {why}: {:?}", x.input.id, x.jobs.keys().collect::<Vec<_>>()))?;
        }
        let Some(s) = self.server.as_ref() else { return Ok(()) };
        for (leg, kept) in &self.store {
            let held = s.engine.grant(leg);
            let agrees = match kept {
                Kept::Running { runner, grant } => self.runners.get(*runner).is_some_and(|x| held == Some((grant.attempt, Some((x.input.id.as_str(), grant.fence))))),
                Kept::Queued { attempt } => held.is_some_and(|(a, g)| a == *attempt && g.is_none()),
                Kept::Settled => true,
            };
            if !agrees {
                return Err(format!("G10: the store has {leg} {kept:?}, the engine {held:?}"));
            }
        }
        Ok(())
    }

    /// G4 and G6 (docs/needs.md E4.6). G4: no gang runs in part, some members under a grant while
    /// others wait in the store. G6: an item that held one room kept for it until it started, started
    /// by its bound: that room's time plus how far the grants it waited for (those on its runner, or
    /// on its gang's members' runners, expected to end by then) ran past their expected ends, as the
    /// store saw them end, each no further than its hard end. Room kept that goes (worked out again
    /// later or elsewhere, its runner gone, a class before its own granted into it, a restart) starts
    /// the bound again from the next.
    fn gangs_and_room(&mut self) -> Result<(), String> {
        let now = self.ms();
        for g in self.seen.iter_mut().filter(|g| g.end.is_none()) {
            if !matches!(self.store.get(&g.leg), Some(Kept::Running { grant, .. }) if grant.fence == g.fence) {
                g.end = Some(now);
            }
        }
        let mut gangs: BTreeMap<String, (Vec<String>, Vec<String>, u32)> = BTreeMap::new();
        for (id, l) in &self.legs {
            let Some(g) = &l.gang else { continue };
            let e = gangs.entry(g.id.clone()).or_insert_with(|| (Vec::new(), Vec::new(), g.size));
            match self.store.get(id) {
                Some(Kept::Running { .. }) => e.0.push(id.clone()),
                Some(Kept::Queued { .. }) => e.1.push(id.clone()),
                _ => {}
            }
        }
        for (g, (running, waiting, size)) in gangs {
            if !running.is_empty() && !waiting.is_empty() {
                return Err(format!("G4: gang {g} runs in part: {running:?} run, {waiting:?} wait"));
            }
            if running.is_empty() {
                self.whole.remove(&g);
            } else if u32::try_from(running.len()).unwrap_or(0) >= size && self.whole.insert(g) {
                self.counts.gangs += 1;
            }
        }
        let Some(s) = self.server.as_ref() else {
            self.holding.clear();
            return Ok(());
        };
        let kept: Vec<(String, usize, i64)> = s.engine.kept_items().into_iter().filter_map(|(l, r, at)| self.runners.iter().position(|x| x.input.id == r).map(|r| (l, r, at))).collect();
        for (leg, runner, at) in &kept {
            // Room worked out again, later or elsewhere, is other room: the bound starts again.
            if self.holding.get(leg).is_some_and(|h| h.at != *at || h.runner != *runner) {
                self.holding.remove(leg);
            }
            if !self.holding.contains_key(leg) {
                self.counts.reserved += 1;
                self.holding.insert(leg.clone(), Holding { from: now, at: *at, runner: *runner });
            }
        }
        let gone: Vec<String> = self.holding.keys().filter(|l| !kept.iter().any(|(k, _, _)| k == *l)).cloned().collect();
        let closed: Vec<(String, Holding)> = gone.iter().filter_map(|l| self.holding.remove(l).map(|h| (l.clone(), h))).collect();
        let seen = &self.seen;
        let started = |leg: &str| seen.iter().rev().find(|g| g.leg == leg).is_some_and(|g| g.start == now);
        let gang_of = |leg: &str| self.legs.get(leg).and_then(|l| l.gang.as_ref()).map(|g| g.id.clone());
        // A gang starts once room is free for all its members: each waited for all their runners.
        let mut crews: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        for (l, h) in closed.iter().filter(|(l, _)| started(l)) {
            if let Some(g) = gang_of(l) {
                crews.entry(g).or_default().insert(h.runner);
            }
        }
        let mut late = None;
        let mut n = 0;
        for (l, h) in closed.iter().filter(|(l, _)| started(l)) {
            n += 1;
            let runners = gang_of(l).and_then(|g| crews.get(&g).cloned()).unwrap_or_else(|| BTreeSet::from([h.runner]));
            let over = seen.iter().filter(|g| runners.contains(&g.runner) && g.leg != *l && g.expected <= h.at && g.start <= now && g.end.is_none_or(|e| e > h.from)).map(|g| (g.end.unwrap_or(now).min(g.hard) - g.expected).max(0)).max().unwrap_or(0);
            if now > h.at.saturating_add(over) && late.is_none() {
                late = Some(format!("G6: {l}, room kept for it on r{} from {} ms (since {} ms), started at {now} ms, past its bound {} ms", h.runner, h.at, h.from, h.at.saturating_add(over)));
            }
        }
        self.counts.reserved_started += n;
        late.map_or(Ok(()), Err)
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
            if self.now > self.until && self.server.is_some() && self.store.len() == self.legs.len() && self.store.values().all(|k| *k == Kept::Settled) && self.runners.iter().all(|x| x.jobs.is_empty() && x.unacked.is_empty()) {
                return Ok(());
            }
        }
        self.check_end()
    }

    /// Once the nemesis has stopped: every leg settled, but those left, as W05-R13 says, waiting
    /// for another runner than the only one big enough, which refused them.
    fn check_end(&mut self) -> Result<(), String> {
        let Some(s) = self.server.as_ref() else { return Err("the server never started again".into()) };
        let waiting: Vec<&String> = self.store.iter().filter(|(_, k)| **k != Kept::Settled).map(|(l, _)| l).collect();
        let stranded = |l: &str| matches!(s.engine.reason(l), Some(WaitReason::OtherRunner { refused: true, .. }));
        // Each with what the engine says of each runner for it (W05-R20), and the runners as they are.
        let open: Vec<String> = waiting
            .iter()
            .filter(|l| !stranded(l))
            .map(|l| {
                let each: Vec<String> = s.engine.explain(l).iter().map(|e| format!("{}: connected {}, eligible {}, room {}", e.runner, e.connected, e.eligible, e.room)).collect();
                format!("{l} ({:?}, waits: {:?}; {})", self.store.get(*l), s.engine.reason(l), each.join(", "))
            })
            .collect();
        let n = waiting.iter().filter(|l| stranded(l)).count();
        self.counts.stranded += u64::try_from(n).unwrap_or(u64::MAX);
        if !open.is_empty() {
            let runners: Vec<String> = self.runners.iter().map(|x| format!("{}: {} slots, size {:?}, jobs {:?}", x.input.id, x.input.slots, x.size.map(|z| (z.cpu, z.mem)), x.jobs.keys().collect::<Vec<_>>())).collect();
            return Err(format!("{:.0} s after the nemesis stopped, legs not settled: {}; runners: {}", SETTLE.as_secs_f64(), open.join(", "), runners.join("; ")));
        }
        Ok(())
    }
}

/// One seed's run: what it counted and its digest, or how it failed.
pub fn run(seed: u64, keep: usize) -> Result<(Counts, u64), String> {
    run_with(seed, keep, None)
}

/// One seed's run with `mutation` planted.
pub fn run_with(seed: u64, keep: usize, mutation: Option<Mutation>) -> Result<(Counts, u64), String> {
    // The engine's own planted bugs (kiln-ci's `mutants`), on this thread while the seed runs.
    let mutant = match mutation {
        Some(Mutation::BackfillPast) => Some(Mutant::PastReservation),
        Some(Mutation::GangInPart) => Some(Mutant::GangInPart),
        _ => None,
    };
    MUTANT.with(|m| m.set(mutant));
    let mut sim = Sim::new(seed, keep, mutation);
    let ran = sim.run();
    MUTANT.with(|m| m.set(None));
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

    /// docs/needs.md E1.6, E3.8 and E4.6: the engine, its store and runners over the seed budget,
    /// under the nemesis (frames lost, doubled and late, links cut, the server crashing, runners
    /// paused past their lease and asleep): at every step no runner holds more than its capacity,
    /// and G2, G4, G6 and G10 hold; every leg settles once the nemesis stops; and every fault
    /// happened, room was kept and gangs started.
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
            let none = [
                ("legs refused", all.refused),
                ("connections broken", all.broken),
                ("reconnects", all.reconnects),
                ("runners lost", all.lost),
                ("partitions", all.partitions),
                ("results heard again", all.again),
                ("writes refused", all.fenced),
                ("frames doubled", all.doubled),
                ("crashes", all.crashes),
                ("grants adopted through a restart", all.adopted),
                ("pauses", all.pauses),
                ("sleeps", all.sleeps),
                ("grants expired", all.expired),
                ("stop grants stopped at their lease's end", all.stopped),
                ("failed legs", all.failed),
                ("room kept for an item", all.reserved),
                ("room kept that stood until its item started", all.reserved_started),
                ("gangs started whole", all.gangs),
            ];
            if let Some((what, _)) = none.iter().find(|(_, n)| *n == 0) {
                return Err(format!("no {what} over the budget: the model didn't test what it says"));
            }
        }
        Ok(())
    }

    /// Whether a seed of the budget (or `KILN_SIM_SEED`'s alone) fails with `m` planted: the first
    /// that does, replayed with its last steps by `KILN_SIM_SEED`.
    fn caught(m: Mutation) -> Result<(), String> {
        let (keep, seeds): (usize, Vec<u64>) = match seeds::replay() {
            Some(s) => (400, vec![s]),
            None => (0, (seeds::from()..seeds::from().saturating_add(seeds::budget(200))).collect()),
        };
        let n = seeds.len();
        for seed in seeds {
            if let Err(why) = run_with(seed, keep, Some(m)) {
                eprintln!("{m:?} caught by seed {seed}: {why}\n  replay: KILN_SIM_SEED={seed}");
                return Ok(());
            }
        }
        Err(format!("{m:?}: none of {n} seeds caught it"))
    }

    /// docs/needs.md E3.8's mutations, each caught within the budget: a write taken by the leg
    /// alone (G2: a stale attempt's result, write or refusal taken as the live one's).
    #[test]
    fn a_write_taken_by_leg_alone_is_caught() -> Result<(), String> {
        caught(Mutation::WriteByLeg)
    }

    /// A renewal that renews every grant whatever its attempt (G2: a stale grant renewed).
    #[test]
    fn a_renewal_of_every_grant_whatever_its_attempt_is_caught() -> Result<(), String> {
        caught(Mutation::RenewAll)
    }

    /// A restart that doesn't raise the epoch (G2: a fence granted twice).
    #[test]
    fn a_restart_that_keeps_its_epoch_is_caught() -> Result<(), String> {
        caught(Mutation::SameEpoch)
    }

    /// A stop grant that doesn't stop itself (G2: it acts past its lease's end).
    #[test]
    fn a_stop_grant_that_runs_on_past_its_lease_is_caught() -> Result<(), String> {
        caught(Mutation::StopRunsOn)
    }

    /// docs/needs.md E4.6's bugs planted in the engine (kiln-ci's `mutants`): a backfill granted
    /// past the room kept for another, where it doesn't fit beside it (G6: that item starts past its
    /// bound).
    #[test]
    fn a_backfill_past_its_reservation_is_caught() -> Result<(), String> {
        caught(Mutation::BackfillPast)
    }

    /// A gang started without its last member (G4: it runs in part).
    #[test]
    fn a_gang_granted_in_part_is_caught() -> Result<(), String> {
        caught(Mutation::GangInPart)
    }

    /// docs/sim.md's bug 10: the seeds where a leg lost with its runner waited for good for another
    /// runner too small to hold it (W05-R16), settled now; in this model (docs/needs.md E3.8), the
    /// seeds that catch it with its fix undone.
    #[test]
    fn the_seeds_where_a_lost_leg_waited_for_a_runner_too_small_settle() -> Result<(), String> {
        for seed in [228, 678, 893, 1155] {
            run(seed, 0).map_err(|why| format!("seed {seed}: {why}"))?;
        }
        Ok(())
    }

    /// docs/sim.md's bug 11 (W05-R25): the seeds where a restart queued a leg at its first attempt
    /// again, whatever attempt the store had (seed 16's its third), hold G10 now.
    #[test]
    fn the_seeds_where_a_restart_queued_a_leg_at_its_first_attempt_again_hold() -> Result<(), String> {
        for seed in [0, 9, 16, 41] {
            run(seed, 0).map_err(|why| format!("seed {seed}: {why}"))?;
        }
        Ok(())
    }

    /// docs/sim.md's bug 12 (W05-R03): the seeds where a runner whose size isn't known took an
    /// ordinary leg beside a share it ran on after its grant was lost, holding both.
    #[test]
    fn the_seeds_where_a_runner_of_no_known_size_took_a_leg_beside_a_share_hold_their_capacity() -> Result<(), String> {
        for seed in [200, 390, 1364, 1652] {
            run(seed, 0).map_err(|why| format!("seed {seed}: {why}"))?;
        }
        Ok(())
    }

    /// docs/sim.md's bug 13 (W05-R34, R25): the seeds of the million where a restart granted a stop
    /// item's next attempt at once, while a runner cut off, its lease running still, went on with
    /// the one before: both acted.
    #[test]
    fn the_seeds_where_a_restart_granted_a_stop_item_again_within_its_lock_delay_hold() -> Result<(), String> {
        for seed in [44084, 112073, 312354, 772989] {
            run(seed, 0).map_err(|why| format!("seed {seed}: {why}"))?;
        }
        Ok(())
    }

    /// docs/sim.md's bug 14 (W05-R39, R40): the seeds of E4.6's probe where two whole gangs of a
    /// class, each with room only beside the room kept for it, took their class's room from each
    /// other for good, the engine never done with one input (1201, 11388); and where a gang with
    /// room took its class's room from the leg it waited behind, then started past its time (5000).
    #[test]
    fn the_seeds_where_gangs_took_their_class_s_room_while_they_had_room_hold() -> Result<(), String> {
        for seed in [1201, 5000, 11388] {
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
