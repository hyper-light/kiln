//! A reference scheduler (docs/needs.md E1.5), for tests: slow and plain, to hold the engine to
//! what docs/ci.md W05 says. After every input it works out from scratch, from its legs and runners
//! alone, what runs where: while some queued leg may run on some runner, the first such leg in queue
//! order goes to the best runner it may run on. It keeps no counts, signatures or indexes: what a
//! runner holds is the legs on it, added up when asked.
//!
//! - The queue order (W05-R04, R09, R10, R30, docs/needs.md E2.1 to E2.4), for a runner offering
//!   a set of the dimensions fair share counts (slots, CPUs, memory, GPUs: slots always, the rest in
//!   resource mode): by class (critical, land, interactive, batch, background); in a class by its
//!   tenant's standing in the class's own tree, level by level from its repository (then its actor,
//!   then its lane, as `fair_by` goes): at each node `max(V[parent][set], finish[node][set])`; then
//!   its tenant's own order, `aging_per_minute × queued-at − priority × 60,000,000`, least first;
//!   then arrival. A runner taking legs goes by the set it offers; legs going to their best runner go
//!   by every dimension. A leg retried or refused is queued again, in the class it came with.
//! - Floors (W05-R31): a runner takes from the first class with a leg it may run, unless a class
//!   after it holds less of the fleet than its floor (its running legs' holds, at their largest
//!   dimension over the connected runners' offers, millionths rounded down) and may go ahead: the
//!   first such class whose best leg there keeps it within its floor, or else whose pace has
//!   passed, which that leg then sets to `now + its dominant share × its cost / (what was left of
//!   the floor)`, rounded up, settled to the time it ran when it ends. Legs going to their best
//!   runner go in class order.
//! - Promotion (W05-R31): a queued leg is looked at a class's bound after it came to its class
//!   (queued, or promoted): if a leg of its class started at its signature in the bound before,
//!   it's looked at again a bound after that start; if none did, it goes up a class (background to
//!   batch, batch to interactive), keeping its place in its tenant's order. Looks due by an
//!   input's time are taken before the input.
//! - Fair share's charge (docs/needs.md E2.1 to E2.4): a leg placed on a runner charges each node of
//!   its tenant in its class's tree, in every set of dimensions, the largest over the set of its
//!   share of each dimension of the fleet (what it holds there, one slot and its request or the
//!   whole runner, over what the connected runners offer, in millionths rounded up) × its cost ×
//!   1000 / the node's weight (a repository's from the config, the levels below it 1000), from
//!   `start = max(V[parent][set], finish[node][set])`; `V[parent][set]` moves up to that start.
//!   When its attempt ends (it finished, failed, was refused or its runner was lost), each node
//!   gives back the charge for its cost and takes one for the time it ran.
//! - Where a leg may run (W05-R19, R03, R16, R02): on a connected runner carrying every label it
//!   asks for, with room for it, that it doesn't stay off: one that refused it, or one that lost it
//!   while another that could take it is connected (carrying its labels, and big enough: room for
//!   it, idle).
//! - Room (W05-R03): an exclusive leg asking for no CPUs, or on a runner whose size isn't known,
//!   takes the whole runner, empty; one asking for CPUs holds them and the memory it asks, of what
//!   no other share holds and the ordinary legs there don't take (a slot's share each in slots
//!   mode, what they ask in resource mode), never overcommitted. An ordinary leg takes a slot of
//!   the share no exclusive leg holds (slots mode), or fits beside what's held within capacity ×
//!   overcommit (resource mode); none while an exclusive leg holds the runner whole.
//! - The best runner (W05-R23, without preferences): least loaded, then most free slots, then
//!   name.
//! - Attempts end: success and failure settle the leg; `runner-lost` is retried once, then settles
//!   (W05-R16); a runner refusing a leg puts it back in the queue, same attempt (W05-R02); a runner
//!   lost loses its legs, and comes back with none. A leg that comes back from a runner still
//!   connected takes W05-R04's two looks in their order: it's offered to the runners it may run on,
//!   then the runner it left takes what fits it.
//!
//! Pools, warm placement, reservations, groups, quotas, admission, draining, preferences and retry
//! policies are outside it, and its cases don't make them.
//!
//! The differential test runs [`CASES`] small random cases (at most 4 runners, 12 legs and 40
//! inputs) through it and the engine, and compares what runs where and what's queued after every
//! input, checking the engine's invariants too: no runner holding more than it has room for, no
//! leg assigned while it runs or on two runners, its queues holding the queued legs alone. A case
//! where they part is cut down to the fewest inputs that still part and printed with its seed;
//! `KILN_SCHED_REFERENCE_SEED=<seed> cargo test -p kiln-ci --lib sched::reference -- --nocapture`
//! replays one, its inputs in words.

use std::collections::{BTreeMap, BTreeSet};

use super::retry::{Failed, FailureClass};
use super::routing::{LegRoute, RunnerView};
use super::state::{Class, ClassConfig, Command, Config, Cpus, FairBy, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, default_classes, own_order};
use super::tests::Draw;
use crate::facts::RunnerFacts;

const CASES: u64 = 10_000;
const MINUTE: i64 = 60_000;
const GIB: u64 = 1 << 30;
/// Fair share's dimensions: slots, milli-CPUs, memory bytes, GPUs; and its sets of them, bitmasks.
const DIMS: usize = 4;
const SETS: usize = 16;

/// What runs where (each running leg's runner and attempt), and what's queued.
type Placement = (BTreeMap<String, (String, u32)>, BTreeSet<String>);

/// One input of a case.
#[derive(Debug, Clone)]
enum Step {
    /// A runner connects, or comes back.
    Up(RunnerIn),
    Lost(String),
    Queued(LegIn),
    Ended { leg: String, attempt: u32, outcome: Outcome },
    /// The runner running the leg refused it.
    Refused(String),
}

impl Step {
    fn input(&self) -> Input {
        match self {
            Step::Up(r) => Input::RunnerUp(Box::new(r.clone())),
            Step::Lost(runner) => Input::RunnerLost { runner: runner.clone() },
            Step::Queued(l) => Input::LegQueued(Box::new(l.clone())),
            Step::Ended { leg, attempt, outcome } => Input::AttemptFinished { leg: leg.clone(), attempt: *attempt, outcome: outcome.clone() },
            Step::Refused(leg) => Input::Rejected { leg: leg.clone() },
        }
    }

    fn words(&self) -> String {
        match self {
            Step::Up(r) => {
                let size = r.capacity.map_or_else(String::new, |c| format!(", {} CPUs and {} GiB{}", c.cpu, c.mem / GIB, if c.gpu > 0 { format!(" and {} GPUs", c.gpu) } else { String::new() }));
                format!("{} up: {} slots{size}, labels {:?}", r.id, r.slots, r.view.labels)
            }
            Step::Lost(runner) => format!("{runner} lost"),
            Step::Queued(l) => {
                let asks = if l.request == Resources::default() { String::new() } else { format!(", asks {} CPUs and {} GiB{}", l.request.cpu, l.request.mem / GIB, if l.request.gpu > 0 { format!(" and {} GPUs", l.request.gpu) } else { String::new() }) };
                let cost = l.cost.map_or_else(|| "the default cost".to_owned(), |ms| format!("cost {} s", ms / 1000));
                format!("{} queued: {}, {}, priority {}{}{asks}, labels {:?}, {cost}", l.id, l.repo, l.class.as_str(), l.priority, if l.exclusive { ", exclusive" } else { "" }, l.route.labels)
            }
            Step::Ended { leg, attempt, outcome } => {
                let how = match outcome {
                    Outcome::Success => "success".to_owned(),
                    Outcome::Cancelled => "cancelled".to_owned(),
                    Outcome::Failed(f) => format!("failed, {}", f.class.as_str()),
                };
                format!("{leg}'s attempt {attempt} ended: {how}")
            }
            Step::Refused(leg) => format!("{leg} refused by its runner"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Off {
    Refused,
    Lost,
}

/// What a placement charged fair share: its class, its tenant's nodes with their weights, its
/// shares, its cost and when it was placed; what it holds of each dimension, for its class's floor;
/// and, placed over that floor, the pace it took and what was left of the floor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Charged {
    class: Class,
    nodes: Vec<(String, u128)>,
    shares: [u128; DIMS],
    ms: u128,
    at: i64,
    holds: [u64; DIMS],
    paced: Option<(i64, u128)>,
}

/// A queued leg's place: its class now, its tenant's own order, its arrival; when to look at its
/// promotion next, if its class has a bound and one above.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Queued {
    class: Class,
    own: i128,
    arrival: u64,
    look: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum At {
    Queued(Queued),
    /// Running there, the `nth` leg placed, and what its placing charged fair share.
    On { runner: String, nth: u64, charged: Charged },
}

#[derive(Debug, Clone)]
struct Leg {
    input: LegIn,
    attempt: u32,
    at: At,
    avoid: Option<(String, Off)>,
}

#[derive(Debug, Clone)]
struct Runner {
    input: RunnerIn,
    connected: bool,
}

/// What an exclusive leg holds of a runner: all of it, or a share.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Claim {
    Whole,
    Share(Resources),
}

/// A runner's size: what it offers in resource mode (its cases' runners have no facts).
fn size(r: &RunnerIn) -> Option<Resources> {
    r.capacity.filter(|c| !c.cpu.is_zero())
}

fn claim(l: &LegIn, size: Option<Resources>) -> Option<Claim> {
    match (l.exclusive, l.request.cpu.is_zero(), size) {
        (false, _, _) => None,
        (true, false, Some(_)) => Some(Claim::Share(l.request)),
        (true, _, _) => Some(Claim::Whole),
    }
}

fn sum(a: Resources, b: &Resources) -> Resources {
    Resources { cpu: a.cpu.saturating_add(b.cpu), mem: a.mem + b.mem, disk: a.disk + b.disk, gpu: a.gpu + b.gpu }
}

fn cpus(c: Cpus) -> u128 {
    u128::from(c.get())
}

/// Whether a runner has room for `leg` beside the legs on it (`on`), as W05-R03 says.
fn room(cfg: &Config, r: &RunnerIn, on: &[&LegIn], leg: &LegIn) -> bool {
    let size = size(r);
    if on.iter().any(|l| claim(l, size) == Some(Claim::Whole)) {
        return false;
    }
    let held = on.iter().filter_map(|l| match claim(l, size) {
        Some(Claim::Share(q)) => Some(q),
        _ => None,
    });
    let held = held.fold(Resources::default(), |a, q| sum(a, &q));
    let ordinary = on.iter().filter(|l| claim(l, size).is_none()).count() as u128;
    let used = on.iter().fold(Resources::default(), |a, l| sum(a, &l.request));
    let n = u128::from(r.slots.max(1));
    let resources = cfg.resources && r.capacity.is_some();
    match (claim(leg, size), size) {
        (Some(Claim::Whole), _) => on.is_empty(),
        (Some(Claim::Share(q)), Some(s)) if resources => cpus(used.cpu) + cpus(q.cpu) <= cpus(s.cpu) && used.mem + q.mem <= s.mem,
        // The ordinary legs there take a slot's share each: ordinary / n of the runner.
        (Some(Claim::Share(q)), Some(s)) => {
            (cpus(held.cpu) + cpus(q.cpu)) * n + ordinary * cpus(s.cpu) <= cpus(s.cpu) * n && u128::from(held.mem) + ordinary * u128::from(s.mem) / n + u128::from(q.mem) <= u128::from(s.mem)
        }
        (Some(Claim::Share(_)), None) => false,
        (None, _) if resources => {
            let Some(c) = r.capacity else { return false };
            let within = |used: u128, cap: u128, factor: u32| used * 1000 <= cap * u128::from(factor);
            within(cpus(used.cpu).saturating_sub(cpus(held.cpu)) + cpus(leg.request.cpu), cpus(c.cpu).saturating_sub(cpus(held.cpu)), cfg.overcommit_cpu)
                && within(u128::from(used.mem.saturating_sub(held.mem) + leg.request.mem), u128::from(c.mem.saturating_sub(held.mem)), cfg.overcommit_mem)
                && within(u128::from(used.disk + leg.request.disk), u128::from(c.disk), cfg.overcommit_disk)
                && used.gpu + leg.request.gpu <= c.gpu
        }
        // A slot of the share of the runner no exclusive leg holds, rounded down.
        (None, _) => {
            let left = match size {
                Some(s) if !(held.cpu.is_zero() && held.mem == 0) => {
                    let share = |total: u128, held: u128| (n * total.saturating_sub(held)).checked_div(total).unwrap_or(n);
                    share(cpus(s.cpu), cpus(held.cpu)).min(share(u128::from(s.mem), u128::from(held.mem)))
                }
                _ => n,
            };
            ordinary < left
        }
    }
}

/// A leg's signature as these cases make them: the labels it asks for, lower-cased, sorted, once
/// each (the engine's `Requirement::signature`, with no rules, groups or capabilities in play).
fn signature(l: &LegIn) -> String {
    let mut labels: Vec<String> = l.route.labels.iter().map(|x| x.to_ascii_lowercase()).collect();
    labels.sort();
    labels.dedup();
    labels.join(",")
}

/// Whether a runner carries every label a leg asks for.
fn carries(r: &RunnerIn, l: &LegIn) -> bool {
    l.route.labels.iter().all(|x| r.view.labels.iter().any(|y| y.eq_ignore_ascii_case(x)))
}

/// The reference scheduler.
struct Reference {
    cfg: Config,
    runners: BTreeMap<String, Runner>,
    legs: BTreeMap<String, Leg>,
    /// Fair share's tags, by class and node, for each set of dimensions.
    vtimes: BTreeMap<(Class, String), [u128; SETS]>,
    finish: BTreeMap<(Class, String), [u128; SETS]>,
    /// When a leg of each class last started at each signature, and when each class may next go
    /// over its floor.
    served: BTreeMap<(String, Class), i64>,
    paced: BTreeMap<Class, i64>,
    arrivals: u64,
    placed: u64,
    /// What it did that the cases are meant to make it do: legs promoted, placed ahead of a class
    /// before theirs by a floor, and placed over a floor.
    promoted: u64,
    ahead: u64,
    over: u64,
}

impl Reference {
    fn new(cfg: Config) -> Reference {
        Reference {
            cfg,
            runners: BTreeMap::new(),
            legs: BTreeMap::new(),
            vtimes: BTreeMap::new(),
            finish: BTreeMap::new(),
            served: BTreeMap::new(),
            paced: BTreeMap::new(),
            arrivals: 0,
            placed: 0,
            promoted: 0,
            ahead: 0,
            over: 0,
        }
    }

    fn running(&self, l: &Leg) -> bool {
        matches!(l.at, At::On { .. })
    }

    /// Whether a step applies now: a runner coming up that's connected, a leg queued that's known,
    /// an attempt ending or refused that isn't running don't, and a case leaves them out.
    fn applies(&self, step: &Step) -> bool {
        match step {
            Step::Up(r) => !self.runners.get(&r.id).is_some_and(|x| x.connected),
            Step::Lost(runner) => self.runners.get(runner).is_some_and(|x| x.connected),
            Step::Queued(l) => !self.legs.contains_key(&l.id),
            Step::Ended { leg, attempt, .. } => self.legs.get(leg).is_some_and(|l| l.attempt == *attempt && self.running(l)),
            Step::Refused(leg) => self.legs.get(leg).is_some_and(|l| self.running(l)),
        }
    }

    fn handle(&mut self, step: &Step, now: i64) {
        self.promote(now);
        match step {
            // W05-R04's second look, in the engine's order: the runners that legs stayed off while
            // this one was away take what fits them, then this one takes what fits it, each in its
            // own pick order (docs/needs.md E2.1: a runner goes by the dimensions it offers).
            Step::Up(r) => {
                self.runners.insert(r.id.clone(), Runner { input: r.clone(), connected: true });
                let avoided: BTreeSet<String> =
                    self.legs.values().filter(|l| matches!(l.at, At::Queued(..))).filter_map(|l| l.avoid.as_ref().filter(|(_, off)| *off == Off::Lost).map(|(runner, _)| runner.clone())).collect();
                for runner in avoided {
                    self.fill(&runner, now);
                }
                self.fill(&r.id, now);
            }
            Step::Lost(runner) => {
                if let Some(r) = self.runners.get_mut(runner) {
                    r.connected = false;
                }
                let lost = Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() });
                for id in self.on(runner) {
                    self.ended(&id, &lost, now);
                }
            }
            Step::Queued(l) => {
                self.legs.insert(l.id.clone(), Leg { input: l.clone(), attempt: 1, at: At::Queued(Queued { class: l.class, own: 0, arrival: 0, look: None }), avoid: None });
                self.enqueue(&l.id, now);
            }
            // W05-R04's two looks, in their order: the leg that came back is offered to the runners
            // it may run on, then the runner it left takes what fits it.
            Step::Ended { leg, outcome, .. } => {
                let left = self.runner_of(leg);
                self.ended(leg, outcome, now);
                if self.legs.get(leg).is_some_and(|l| matches!(l.at, At::Queued(..))) {
                    self.offer(leg, now);
                }
                if let Some(r) = left {
                    self.fill(&r, now);
                }
            }
            Step::Refused(leg) => {
                let left = self.runner_of(leg);
                self.settle(leg, now);
                if let Some(l) = self.legs.get_mut(leg)
                    && let Some(runner) = &left
                {
                    l.avoid = Some((runner.clone(), Off::Refused));
                    self.enqueue(leg, now);
                    self.offer(leg, now);
                    self.fill(runner, now);
                }
            }
        }
        self.place(now);
    }

    fn runner_of(&self, leg: &str) -> Option<String> {
        self.legs.get(leg).and_then(|l| if let At::On { runner, .. } = &l.at { Some(runner.clone()) } else { None })
    }

    fn class_config(&self, class: Class) -> ClassConfig {
        self.cfg.classes.get(&class).copied().unwrap_or_default()
    }

    /// When a leg that came to `class` at `since` is first looked at for promotion: a bound after,
    /// if the class has one and a class above.
    fn look(&self, class: Class, since: i64) -> Option<i64> {
        let bound = self.class_config(class).bound?;
        class.up()?;
        Some(since + i64::try_from(bound).unwrap_or(i64::MAX / 2))
    }

    /// Every look due by `now`, the earliest first: a leg whose class started a leg at its
    /// signature in the bound before the look is looked at again a bound after that start; one
    /// whose class started none there goes up a class.
    fn promote(&mut self, now: i64) {
        loop {
            let due = self.legs.iter().filter_map(|(id, l)| if let At::Queued(q) = &l.at { q.look.filter(|t| *t <= now).map(|t| (t, id.clone())) } else { None }).min();
            let Some((at, id)) = due else { return };
            let Some(l) = self.legs.get(&id) else { return };
            let At::Queued(q) = &l.at else { return };
            let (class, sig) = (q.class, signature(&l.input));
            let bound = self.class_config(class).bound.map_or(0, |b| i64::try_from(b).unwrap_or(i64::MAX / 2));
            let last = self.served.get(&(sig, class)).copied().filter(|t| *t > at - bound);
            let up = class.up().unwrap_or(class);
            let look = match last {
                Some(t) => Some(t + bound),
                None => self.look(up, at),
            };
            if last.is_none() {
                self.promoted += 1;
            }
            if let Some(At::Queued(q)) = self.legs.get_mut(&id).map(|l| &mut l.at) {
                if last.is_none() {
                    q.class = up;
                }
                q.look = look;
            }
        }
    }

    /// A runner takes the best queued legs it has room for, in its own pick order (by the dimensions
    /// it offers), until none fits (W05-R04: new capacity looks at what that runner can serve).
    fn fill(&mut self, runner: &str, now: i64) {
        loop {
            let Some((id, over)) = self.pick(runner, now) else { return };
            self.assign(&id, runner, now, over);
        }
    }

    /// The leg a runner takes next: the best of the first class with a leg it may run, unless a
    /// class after that one is short of its floor (W05-R31): then the first such class's best, if
    /// it keeps the class within its floor, or once its pace has passed if not (with what's left of
    /// its floor).
    fn pick(&mut self, runner: &str, now: i64) -> Option<(String, Option<u128>)> {
        let r = self.runners.get(runner)?;
        let dims = self.dims(&r.input);
        let mut best: BTreeMap<Class, (([u128; 3], i128, u64), String)> = BTreeMap::new();
        for (id, l) in &self.legs {
            let At::Queued(q) = &l.at else { continue };
            if !self.may_run(l, r) {
                continue;
            }
            let key = (self.standing(q.class, &l.input, dims), q.own, q.arrival);
            if best.get(&q.class).is_none_or(|(b, _)| key < *b) {
                best.insert(q.class, (key, id.clone()));
            }
        }
        let (first, (_, id)) = best.first_key_value()?;
        let fleet = self.fleet();
        for (class, (_, x)) in best.iter().filter(|(c, _)| *c > first) {
            let floor = u128::from(self.class_config(*class).floor) * 1000;
            let held = self.held(*class);
            if floor == 0 || share(&held, &fleet) >= floor {
                continue;
            }
            let mut with = held;
            if let Some(l) = self.legs.get(x) {
                for (w, h) in with.iter_mut().zip(self.holds(&l.input, &r.input)) {
                    *w += u128::from(h);
                }
            }
            if share(&with, &fleet) <= floor {
                self.ahead += 1;
                return Some((x.clone(), None));
            }
            if self.paced.get(class).is_none_or(|t| now >= *t) {
                self.ahead += 1;
                self.over += 1;
                return Some((x.clone(), Some(floor - share(&held, &fleet))));
            }
        }
        Some((id.clone(), None))
    }

    /// A leg queued again is offered to the runners it may run on, best first, each taking the best
    /// legs it has room for, until it's taken (W05-R04: queuing a leg looks at its runners).
    fn offer(&mut self, id: &str, now: i64) {
        let Some(l) = self.legs.get(id) else { return };
        let mut runners: Vec<&Runner> = self.runners.values().filter(|r| self.may_run(l, r)).collect();
        runners.sort_by(|a, b| self.better(a, b));
        let runners: Vec<String> = runners.into_iter().map(|r| r.input.id.clone()).collect();
        for r in runners {
            if !self.legs.get(id).is_some_and(|l| matches!(l.at, At::Queued(..))) {
                return;
            }
            self.fill(&r, now);
        }
    }

    /// An attempt ended: one its runner lost is retried once, elsewhere when it can be (W05-R16);
    /// the rest settle.
    fn ended(&mut self, id: &str, outcome: &Outcome, now: i64) {
        self.settle(id, now);
        let Some(l) = self.legs.get_mut(id) else { return };
        let At::On { runner, .. } = &l.at else { return };
        match outcome {
            Outcome::Failed(f) if f.class == FailureClass::RunnerLost && l.attempt < 2 => {
                l.attempt += 1;
                l.avoid = Some((runner.clone(), Off::Lost));
                self.enqueue(id, now);
            }
            _ => {
                self.legs.remove(id);
            }
        }
    }

    /// Queues a leg, or queues it again: in the class it came with, its place in its tenant's own
    /// order by its priority and `now`, its arrival (fair share's standing is taken as legs are
    /// picked).
    fn enqueue(&mut self, id: &str, now: i64) {
        self.arrivals += 1;
        let (arrival, aging) = (self.arrivals, self.cfg.aging_per_minute);
        let Some(class) = self.legs.get(id).map(|l| l.input.class) else { return };
        let look = self.look(class, now);
        if let Some(l) = self.legs.get_mut(id) {
            l.at = At::Queued(Queued { class, own: own_order(l.input.priority, aging, now), arrival, look });
        }
    }

    /// What a runner offers in each of fair share's dimensions: slots, and in resource mode its
    /// CPUs (thousandths), memory and GPUs.
    fn offers(&self, r: &RunnerIn) -> [u64; DIMS] {
        match size(r).filter(|_| self.cfg.resources) {
            Some(c) => [u64::from(r.slots), c.cpu.get(), c.mem, u64::from(c.gpu)],
            None => [u64::from(r.slots), 0, 0, 0],
        }
    }

    fn dims(&self, r: &RunnerIn) -> [bool; DIMS] {
        self.offers(r).map(|o| o > 0)
    }

    /// What the connected runners offer, in each dimension.
    fn fleet(&self) -> [u64; DIMS] {
        let mut fleet = [0u64; DIMS];
        for x in self.runners.values().filter(|x| x.connected) {
            for (f, o) in fleet.iter_mut().zip(self.offers(&x.input)) {
                *f += o;
            }
        }
        fleet
    }

    /// What a leg holds of a runner in each dimension: one slot and its request, or held whole, all
    /// the runner offers; nothing in a dimension it doesn't offer.
    fn holds(&self, l: &LegIn, r: &RunnerIn) -> [u64; DIMS] {
        let offers = self.offers(r);
        let q = l.request;
        let mut holds = if claim(l, size(r)) == Some(Claim::Whole) { offers } else { [1, q.cpu.get(), q.mem, u64::from(q.gpu)] };
        for (h, o) in holds.iter_mut().zip(offers) {
            if o == 0 {
                *h = 0;
            }
        }
        holds
    }

    /// What a class's running legs hold, in each dimension.
    fn held(&self, class: Class) -> [u128; DIMS] {
        let mut held = [0u128; DIMS];
        for l in self.legs.values() {
            if let At::On { charged, .. } = &l.at
                && charged.class == class
            {
                for (h, x) in held.iter_mut().zip(charged.holds) {
                    *h += u128::from(x);
                }
            }
        }
        held
    }

    /// A leg's tenant: its repository, then its actor and its lane as `fair_by` goes, each node with
    /// its weight (a repository's from the config, the rest 1000).
    fn nodes(&self, l: &LegIn) -> Vec<(String, u128)> {
        let mut nodes = vec![(l.repo.clone(), u128::from(self.cfg.weights.get(&l.repo).copied().filter(|w| *w > 0).unwrap_or(1000)))];
        let mut name = l.repo.clone();
        for level in &self.cfg.fair_by {
            let member = match level {
                FairBy::Repo => continue,
                FairBy::Actor => &l.actor,
                FairBy::Lane => &l.lane,
            };
            name = format!("{name}/{member}");
            nodes.push((name.clone(), 1000));
        }
        nodes
    }

    /// A leg's tenant's standing in `class`'s tree for a runner offering `dims`, level by level
    /// from its repository: at each node `max(V[parent][set], finish[node][set])`, `set` the
    /// dimensions it offers.
    fn standing(&self, class: Class, l: &LegIn, dims: [bool; DIMS]) -> [u128; 3] {
        let set = (0..DIMS).filter(|d| dims.get(*d).copied().unwrap_or(false)).fold(0, |m, d| m | (1 << d));
        let mut out = [0; 3];
        let mut parent = String::new();
        for (k, (node, _)) in self.nodes(l).into_iter().enumerate() {
            let v = self.vtimes.get(&(class, parent.clone())).and_then(|v| v.get(set)).copied().unwrap_or(0);
            let f = self.finish.get(&(class, node.clone())).and_then(|f| f.get(set)).copied().unwrap_or(0);
            if let Some(slot) = out.get_mut(k) {
                *slot = v.max(f);
            }
            parent = node;
        }
        out
    }

    /// While some queued leg may run somewhere, the first in queue order (by class, then by every
    /// dimension) goes to its best runner.
    fn place(&mut self, now: i64) {
        loop {
            let mut order: Vec<((Class, [u128; 3], i128, u64), String)> =
                self.legs.iter().filter_map(|(id, l)| if let At::Queued(q) = &l.at { Some(((q.class, self.standing(q.class, &l.input, [true; DIMS]), q.own, q.arrival), id.clone())) } else { None }).collect();
            order.sort();
            let next = order.into_iter().find_map(|(_, id)| self.best(&id).map(|r| (id, r)));
            let Some((leg, runner)) = next else { return };
            self.assign(&leg, &runner, now, None);
        }
    }

    /// A leg placed on a runner, each node of its tenant charged in its class's tree: in each
    /// dimension the runner offers, its share of the fleet (what it holds there over what the
    /// connected runners offer, millionths rounded up) × its cost × 1000 / the node's weight, from
    /// `max(V[parent][d], finish[node][d])`; what it charged kept, to settle when it ends. Its
    /// class started a leg at its signature; placed over its class's floor (`over`, what was left
    /// of it), the class's pace moves on.
    fn assign(&mut self, leg: &str, runner: &str, now: i64, over: Option<u128>) {
        let Some((l, r)) = self.legs.get(leg).zip(self.runners.get(runner)) else { return };
        let At::Queued(q) = &l.at else { return };
        let (class, fleet, holds) = (q.class, self.fleet(), self.holds(&l.input, &r.input));
        let mut shares = [0u128; DIMS];
        for d in 0..DIMS {
            let (h, cap) = (holds.get(d).copied().unwrap_or(0), fleet.get(d).copied().unwrap_or(0));
            if let Some(s) = shares.get_mut(d)
                && h > 0
                && cap > 0
            {
                *s = (u128::from(h) * 1_000_000).div_ceil(u128::from(cap));
            }
        }
        let (nodes, ms) = (self.nodes(&l.input), u128::from(l.input.cost.unwrap_or(self.cfg.default_cost)));
        let sig = signature(&l.input);
        let mut parent = String::new();
        for (node, weight) in &nodes {
            let mut v = self.vtimes.get(&(class, parent.clone())).copied().unwrap_or([0; SETS]);
            let mut f = self.finish.get(&(class, node.clone())).copied().unwrap_or([0; SETS]);
            for set in 0..SETS {
                let cost = set_cost(&shares, ms, *weight, set);
                if let (Some(f), Some(v)) = (f.get_mut(set), v.get_mut(set))
                    && cost > 0
                {
                    let start = (*v).max(*f);
                    *f = start + cost;
                    *v = (*v).max(start);
                }
            }
            self.vtimes.insert((class, parent), v);
            self.finish.insert((class, node.clone()), f);
            parent = node.clone();
        }
        let paced = over.map(|left| {
            let took = i64::try_from((shares.iter().copied().max().unwrap_or(0) * ms).div_ceil(left)).unwrap_or(i64::MAX / 2);
            self.paced.insert(class, now + took);
            (took, left)
        });
        self.served.insert((sig, class), now);
        self.placed += 1;
        let charged = Charged { class, nodes, shares, ms, at: now, holds, paced };
        if let Some(l) = self.legs.get_mut(leg) {
            l.at = At::On { runner: runner.to_owned(), nth: self.placed, charged };
        }
    }

    /// A placement's charge settled to the time it ran: each node gives back the charge for its cost
    /// and takes one for the time from its placing to `now`; and a pace it set, alike.
    fn settle(&mut self, leg: &str, now: i64) {
        let Some(At::On { charged, .. }) = self.legs.get(leg).map(|l| l.at.clone()) else { return };
        let ran = u128::try_from((now - charged.at).max(0)).unwrap_or(0);
        for (node, weight) in &charged.nodes {
            if let Some(f) = self.finish.get_mut(&(charged.class, node.clone())) {
                for (set, f) in f.iter_mut().enumerate() {
                    if set_cost(&charged.shares, charged.ms, *weight, set) > 0 {
                        *f = f.saturating_sub(set_cost(&charged.shares, charged.ms, *weight, set)) + set_cost(&charged.shares, ran, *weight, set);
                    }
                }
            }
        }
        if let Some((took, left)) = charged.paced
            && let Some(at) = self.paced.get_mut(&charged.class)
        {
            let used = (charged.shares.iter().copied().max().unwrap_or(0) * ran).div_ceil(left);
            *at = *at - took + i64::try_from(used).unwrap_or(i64::MAX / 2);
        }
    }

    /// The legs on a runner, in the order they came to it.
    fn on(&self, runner: &str) -> Vec<String> {
        let mut on: Vec<(u64, &String)> = self.legs.iter().filter_map(|(id, l)| if let At::On { runner: r, nth, .. } = &l.at { (r == runner).then_some((*nth, id)) } else { None }).collect();
        on.sort();
        on.into_iter().map(|(_, id)| id.clone()).collect()
    }

    /// The best runner a leg may run on now: least loaded, then most free slots, then name.
    fn best(&self, id: &str) -> Option<String> {
        let l = self.legs.get(id)?;
        let best = self.runners.values().filter(|r| self.may_run(l, r)).min_by(|a, b| self.better(a, b));
        best.map(|r| r.input.id.clone())
    }

    /// Which of two runners a leg goes to first: the less loaded, then the one with more free
    /// slots, then by name.
    fn better(&self, a: &Runner, b: &Runner) -> std::cmp::Ordering {
        let load = |r: &Runner| (self.on(&r.input.id).len() as u128, u128::from(r.input.slots.max(1)));
        let ((na, sa), (nb, sb)) = (load(a), load(b));
        (na * sb).cmp(&(nb * sa)).then_with(|| sb.saturating_sub(nb).cmp(&sa.saturating_sub(na))).then_with(|| a.input.id.cmp(&b.input.id))
    }

    fn may_run(&self, l: &Leg, r: &Runner) -> bool {
        let on: Vec<&LegIn> = self.on(&r.input.id).iter().filter_map(|id| self.legs.get(id)).map(|l| &l.input).collect();
        r.connected && carries(&r.input, &l.input) && !self.stays_off(l, &r.input.id) && room(&self.cfg, &r.input, &on, &l.input)
    }

    /// A runner the leg stays off: one that refused it; one that lost it, while another that
    /// could take it is connected: one carrying its labels with room for it when idle.
    fn stays_off(&self, l: &Leg, runner: &str) -> bool {
        match &l.avoid {
            Some((r, Off::Refused)) => r == runner,
            Some((r, Off::Lost)) => r == runner && self.runners.values().any(|o| o.input.id != runner && o.connected && carries(&o.input, &l.input) && room(&self.cfg, &o.input, &[], &l.input)),
            None => false,
        }
    }

    fn placement(&self) -> Placement {
        let mut p = Placement::default();
        for (id, l) in &self.legs {
            match &l.at {
                At::On { runner, .. } => {
                    p.0.insert(id.clone(), (runner.clone(), l.attempt));
                }
                At::Queued(..) => {
                    p.1.insert(id.clone());
                }
            }
        }
        p
    }
}

/// What a placement costs a node of `weight` in a set of dimensions (a bitmask): the largest over
/// the set of each dimension's share × `ms` × 1000 / `weight`.
fn set_cost(shares: &[u128; DIMS], ms: u128, weight: u128, set: usize) -> u128 {
    let mut most = 0;
    for (d, share) in shares.iter().enumerate() {
        if (set >> d) & 1 == 1 {
            most = most.max(share * ms * 1000 / weight);
        }
    }
    most
}

/// What `held` (each dimension) is of `fleet`: its largest dimension's share, millionths rounded
/// down, over the dimensions the fleet has.
fn share(held: &[u128; DIMS], fleet: &[u64; DIMS]) -> u128 {
    let mut most = 0;
    for (h, f) in held.iter().zip(fleet) {
        if *f > 0 {
            most = most.max(h * 1_000_000 / u128::from(*f));
        }
    }
    most
}

fn ms(d: &mut Draw, below: i64) -> i64 {
    i64::try_from(d.below(u64::try_from(below).unwrap_or(1))).unwrap_or(0)
}

fn labels(d: &mut Draw) -> Vec<String> {
    match d.below(4) {
        0 => Vec::new(),
        1 => vec!["x".into()],
        2 => vec!["y".into()],
        _ => vec!["x".into(), "y".into()],
    }
}

fn runner(d: &mut Draw, n: u64) -> RunnerIn {
    let id = format!("r{n}");
    let mut view = RunnerView { name: id.clone(), labels: labels(d), facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 0, pool: None };
    if view.labels.is_empty() {
        view.labels = vec!["x".into()];
    }
    let slots = 1 + u32::try_from(d.below(3)).unwrap_or(0);
    // Sized half the time, and now and then with a GPU or two (fair share's fourth dimension).
    let capacity = (d.below(2) == 0).then(|| Resources { cpu: Cpus::whole(2 + d.below(7)), mem: (4 + d.below(29)) * GIB, gpu: if d.below(3) == 0 { 1 + u32::try_from(d.below(2)).unwrap_or(0) } else { 0 }, ..Resources::default() });
    RunnerIn { id, view, slots, capacity, executor: None }
}

fn leg(d: &mut Draw, n: u64) -> LegIn {
    let mut request = match d.below(3) {
        0 => Resources::default(),
        1 => Resources { cpu: Cpus::milli(500 * (1 + d.below(8))), mem: (1 + d.below(8)) * GIB, ..Resources::default() },
        _ => Resources { cpu: Cpus::whole(1 + d.below(4)), mem: (1 + d.below(16)) * GIB, ..Resources::default() },
    };
    let exclusive = d.below(5) == 0;
    // An ordinary leg asking CPUs asks a GPU now and then.
    if !exclusive && !request.cpu.is_zero() && d.below(4) == 0 {
        request.gpu = 1;
    }
    LegIn {
        id: format!("l{n}"),
        run: format!("run{}", n / 3),
        repo: format!("acme/r{}", d.below(3)),
        route: LegRoute { labels: labels(d), ..LegRoute::default() },
        priority: [0u8, 20, 60, 80, 100].get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or(60),
        exclusive,
        request,
        cost: (d.below(2) == 0).then(|| 10_000 * (1 + d.below(60))),
        actor: format!("a{}", d.below(2)),
        lane: format!("l{}", d.below(2)),
        class: Class::ALL.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default(),
        ..LegIn::default()
    }
}

/// A seed's case: resource mode or slots, and up to 40 steps, each at its time. Made beside a
/// reference, so each step applies when it's made: an attempt ends or is refused while it runs, a
/// runner is lost while connected and comes back once lost.
fn case(seed: u64) -> (Config, Vec<(Step, i64)>) {
    let mut d = Draw(seed);
    let resources = d.below(2) == 0;
    // Fair share by repository, then by actor, then by lane (W28-R66, docs/needs.md E2.2).
    let fair_by = match d.below(3) {
        0 => vec![FairBy::Repo],
        1 => vec![FairBy::Repo, FairBy::Actor],
        _ => vec![FairBy::Repo, FairBy::Actor, FairBy::Lane],
    };
    // The classes' floors and bounds (W05-R31): the defaults, or drawn, bounds short enough to pass
    // in a case; and aging (W05-R09) off, as by default, or fast.
    let classes = match d.below(3) {
        0 => default_classes(),
        _ => Class::ALL.into_iter().map(|c| (c, ClassConfig { floor: [0, 0, 100, 250, 500].get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or(0), bound: (d.below(2) == 0).then(|| 60_000 * (1 + d.below(20))) })).collect(),
    };
    let aging_per_minute = [0, 1000, 10_000].get(usize::try_from(d.below(3)).unwrap_or(0)).copied().unwrap_or(1000);
    let cfg = Config { resources, fair_by, classes, aging_per_minute, ..Config::default() };
    let mut r = Reference::new(cfg.clone());
    let shapes: Vec<RunnerIn> = (0..1 + d.below(4)).map(|n| runner(&mut d, n)).collect();
    let (most, mut queued, mut now, mut steps) = (1 + d.below(12), 0, 0, Vec::new());
    for _ in 0..40 {
        now += ms(&mut d, 3 * MINUTE);
        let running: Vec<String> = r.placement().0.into_keys().collect();
        let up: Vec<String> = r.runners.values().filter(|x| x.connected).map(|x| x.input.id.clone()).collect();
        let step = match d.below(10) {
            0 | 1 => shapes.iter().find(|s| !up.contains(&s.id)).cloned().map(Step::Up),
            2..=4 if queued < most => {
                queued += 1;
                Some(Step::Queued(leg(&mut d, queued - 1)))
            }
            5..=7 => d.pick(&running).and_then(|id| r.legs.get(id)).map(|l| {
                let outcome = match d.below(4) {
                    0 | 1 => Outcome::Success,
                    2 => Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: Some(1), log_hits: Vec::new() }),
                    _ => Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }),
                };
                Step::Ended { leg: l.input.id.clone(), attempt: l.attempt, outcome }
            }),
            8 => d.pick(&running).cloned().map(Step::Refused),
            9 => d.pick(&up).cloned().map(Step::Lost),
            _ => None,
        };
        if let Some(step) = step.filter(|s| r.applies(s)) {
            r.handle(&step, now);
            steps.push((step, now));
        }
    }
    (cfg, steps)
}

/// The engine's own word for what runs where, and its invariants: an `Assign` at most once, and
/// only for a leg not running (or one the step took off its runner: refused, ended, or on a runner
/// lost); each runner's legs the legs running there, each once; its queues holding the queued legs
/// alone; no runner holding more than it has room for.
fn engine(cfg: &Config, s: &Scheduler, step: &Step, before: &Placement, commands: &[Command], runners: &BTreeMap<String, RunnerIn>, legs: &BTreeMap<String, LegIn>) -> Result<Placement, String> {
    let (running, on, queued) = s.placement();
    let off: BTreeSet<&String> = match step {
        Step::Refused(leg) | Step::Ended { leg, .. } => BTreeSet::from([leg]),
        Step::Lost(runner) => before.0.iter().filter(|(_, (r, _))| r == runner).map(|(l, _)| l).collect(),
        Step::Up(_) | Step::Queued(_) => BTreeSet::new(),
    };
    let mut assigned = BTreeSet::new();
    for c in commands {
        if let Command::Assign { leg, .. } = c
            && ((before.0.contains_key(leg) && !off.contains(leg)) || !assigned.insert(leg.clone()))
        {
            return Err(format!("{leg} assigned while it ran"));
        }
    }
    let mut seen = BTreeSet::new();
    for (r, ids) in &on {
        for id in ids {
            if !seen.insert(id.clone()) {
                return Err(format!("{id} on two runners' lists, or twice on {r}'s"));
            }
            if running.get(id).map(|x| &x.0) != Some(r) {
                return Err(format!("{id} on {r}'s list, but not running there"));
            }
        }
        let (Some(runner), Some(ids)) = (runners.get(r), on.get(r)) else { continue };
        let ins: Vec<&LegIn> = ids.iter().filter_map(|id| legs.get(id)).collect();
        for (k, l) in ins.iter().enumerate() {
            if !room(cfg, runner, ins.get(..k).unwrap_or_default(), l) {
                return Err(format!("{r} holds more than it has room for: {} beside {:?}", l.id, ins.get(..k).unwrap_or_default().iter().map(|x| &x.id).collect::<Vec<_>>()));
            }
        }
    }
    if seen.len() != running.len() {
        return Err("a running leg on no runner's list".into());
    }
    if s.queued() != queued.len() {
        return Err(format!("its queues hold {} entries for {} queued legs", s.queued(), queued.len()));
    }
    Ok((running, queued))
}

fn show(p: &Placement) -> String {
    let on: Vec<String> = p.0.iter().map(|(l, (r, a))| format!("{l}@{r}#{a}")).collect();
    format!("running [{}], queued {:?}", on.join(" "), p.1)
}

/// A case's steps through both, leaving out those that don't apply: `Err` where the engine breaks
/// an invariant or the two part, with the steps that applied up to there.
fn run(cfg: &Config, steps: &[(Step, i64)], mut tell: impl FnMut(&str)) -> Result<(), (Vec<(Step, i64)>, String)> {
    let (mut s, mut r) = (Scheduler::new(cfg.clone()), Reference::new(cfg.clone()));
    let (mut runners, mut legs, mut applied) = (BTreeMap::new(), BTreeMap::new(), Vec::new());
    for (step, now) in steps {
        if !r.applies(step) {
            continue;
        }
        applied.push((step.clone(), *now));
        match step {
            Step::Up(x) => {
                runners.insert(x.id.clone(), x.clone());
            }
            Step::Queued(l) => {
                legs.insert(l.id.clone(), l.clone());
            }
            _ => {}
        }
        let before = r.placement();
        let commands = s.handle(step.input(), *now);
        r.handle(step, *now);
        let mine = r.placement();
        tell(&format!("{:>4}:{:02} {}\n      {}", now / MINUTE, now % MINUTE / 1000, step.words(), show(&mine)));
        let theirs = engine(cfg, &s, step, &before, &commands, &runners, &legs).map_err(|why| (applied.clone(), format!("the engine broke an invariant: {why}")))?;
        if theirs != mine {
            return Err((applied, format!("they part:\n  engine:    {}\n  reference: {}", show(&theirs), show(&mine))));
        }
    }
    Ok(())
}

/// A failing case cut down: steps left out one at a time while it still fails, until leaving out
/// any one makes it pass.
fn shrink(cfg: &Config, steps: Vec<(Step, i64)>, why: String) -> (Vec<(Step, i64)>, String) {
    let (mut steps, mut why) = (steps, why);
    'smaller: loop {
        for i in 0..steps.len() {
            let mut fewer = steps.clone();
            fewer.remove(i);
            if let Err((applied, w)) = run(cfg, &fewer, |_| {}) {
                (steps, why) = (applied, w);
                continue 'smaller;
            }
        }
        return (steps, why);
    }
}

/// A case's settings in words: its mode, fair share's levels, aging, and each class's floor and
/// bound.
fn settings(cfg: &Config) -> String {
    let classes: Vec<String> = cfg.classes.iter().map(|(c, cc)| format!("{} floor {}‰{}", c.as_str(), cc.floor, cc.bound.map_or_else(String::new, |b| format!(" bound {} min", b / 60_000)))).collect();
    format!("{}, fair by {:?}, aging {}‰ a minute, {}", if cfg.resources { "resource mode" } else { "slots" }, cfg.fair_by, cfg.aging_per_minute, classes.join(", "))
}

fn report(seed: u64, cfg: &Config, steps: &[(Step, i64)], why: &str) -> String {
    let mode = settings(cfg);
    let words: Vec<String> = steps.iter().map(|(s, now)| format!("  {:>4}:{:02} {}", now / MINUTE, now % MINUTE / 1000, s.words())).collect();
    format!(
        "seed {seed} ({mode}), cut down to {} inputs:\n{}\n{why}\nreplay it: KILN_SCHED_REFERENCE_SEED={seed} cargo test -p kiln-ci --lib sched::reference -- --nocapture",
        steps.len(),
        words.join("\n")
    )
}

/// E1.5: the engine places legs as the reference does, after every input of 10,000 small random
/// cases, and keeps its invariants.
#[test]
fn e1_5_the_engine_agrees_with_the_reference() {
    if let Some(seed) = std::env::var("KILN_SCHED_REFERENCE_SEED").ok().and_then(|s| s.trim().parse::<u64>().ok()) {
        let (cfg, steps) = case(seed);
        println!("seed {seed} ({}):", settings(&cfg));
        let failed = run(&cfg, &steps, |line| println!("{line}")).err().map(|(applied, why)| {
            let (fewer, why) = shrink(&cfg, applied, why);
            report(seed, &cfg, &fewer, &why)
        });
        assert!(failed.is_none(), "{}", failed.unwrap_or_default());
        return;
    }
    let mut parted = Vec::new();
    for seed in 0..CASES {
        let (cfg, steps) = case(seed);
        if let Err((applied, why)) = run(&cfg, &steps, |_| {}) {
            parted.push((seed, cfg, applied, why));
        }
    }
    let first = parted.first().map(|(seed, cfg, applied, why)| {
        let (fewer, why) = shrink(cfg, applied.clone(), why.clone());
        report(*seed, cfg, &fewer, &why)
    });
    assert!(parted.is_empty(), "{} of {CASES} cases part; the first:\n{}", parted.len(), first.unwrap_or_default());
}

/// The cases cover what the reference is for: both modes, exclusive legs held whole and in
/// shares, legs refused and retried after a runner was lost, runners coming back, every class,
/// and legs promoted, placed ahead by a floor and placed over one.
#[test]
fn e1_5_the_cases_cover_the_rules() {
    let (mut kinds, mut modes, mut depths, mut classes) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut promoted, mut ahead, mut over) = (0, 0, 0);
    for seed in 0..500 {
        let (cfg, steps) = case(seed);
        modes.insert(cfg.resources);
        depths.insert(cfg.fair_by.len());
        let mut r = Reference::new(cfg.clone());
        for (s, now) in &steps {
            if r.applies(s) {
                r.handle(s, *now);
            }
            if let Step::Queued(l) = s {
                classes.insert(l.class);
            }
        }
        (promoted, ahead, over) = (promoted + r.promoted, ahead + r.ahead, over + r.over);
        let mut lost = BTreeSet::new();
        for (s, _) in &steps {
            kinds.insert(match s {
                Step::Up(r) if lost.contains(&r.id) => "back",
                Step::Up(r) if r.capacity.is_some_and(|c| c.gpu > 0) => "up with GPUs",
                Step::Up(_) => "up",
                Step::Lost(r) => {
                    lost.insert(r.clone());
                    "lost"
                }
                Step::Queued(l) if l.exclusive && l.request.cpu.is_zero() => "exclusive whole",
                Step::Queued(l) if l.exclusive => "exclusive share",
                Step::Queued(l) if l.request.gpu > 0 => "asks a GPU",
                Step::Queued(_) => "queued",
                Step::Ended { outcome: Outcome::Failed(f), .. } if f.class == FailureClass::RunnerLost => "ended lost",
                Step::Ended { attempt: 2, .. } => "ended retried",
                Step::Ended { .. } => "ended",
                Step::Refused(_) => "refused",
            });
        }
    }
    assert_eq!(modes.len(), 2);
    assert_eq!(depths.len(), 3, "fair share by repository, actor and lane");
    assert_eq!(classes.len(), Class::ALL.len(), "every class");
    assert!(promoted > 0 && ahead > 0 && over > 0, "promoted {promoted}, ahead by a floor {ahead}, over one {over}");
    for k in ["up", "up with GPUs", "back", "lost", "queued", "asks a GPU", "exclusive whole", "exclusive share", "ended", "ended lost", "ended retried", "refused"] {
        assert!(kinds.contains(k), "no case has `{k}`");
    }
}
