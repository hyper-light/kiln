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
//! - Promotion (W05-R31): a queued leg in a class with one above (background, batch, interactive)
//!   has a bound: once none of the legs of the classes before its own queued or running when it
//!   came to its class (queued, or promoted) waits any more, the latest expected end of those still
//!   running then, or then, less when it came. With none of them queued or running when it came,
//!   it has none until the first input after which one is: its wait starts at that input's time,
//!   behind those. A person's bound for its class (`[sched.classes.<class>] bound`) is one too,
//!   looked at a bound after it came to its class. At a look, if a leg of its class started at its
//!   signature in the bound before, it's looked at again a bound after that start; if none did, it
//!   goes up a class (background to batch, batch to interactive, interactive to land) with its
//!   gang's queued members of its class, keeping its place in its tenant's order. Looks due by an
//!   input's time are taken before the input.
//! - Fair share's charge (docs/needs.md E2.1 to E2.4): a leg placed on a runner charges each node of
//!   its tenant in its class's tree, in every set of dimensions, the largest over the set of its
//!   share of each dimension of the fleet (what it holds there, one slot and its request or the
//!   whole runner, over what the connected runners offer, in millionths rounded up) × its cost ×
//!   1000 / the node's weight (a repository's from the config, the levels below it 1000), from
//!   `start = max(V[parent][set], finish[node][set])`; `V[parent][set]` moves up to that start.
//!   Where the runner offers no CPUs (slots mode), the slot's share counts the CPUs the leg asks,
//!   one at the least (main's 8305d5c80; a leg holding the runner whole counts as it is).
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
//!   (W05-R16); a runner refusing a leg puts it back in the queue, same attempt, kept off that
//!   runner unless it was draining (W05-R02, R35); refused by a runner at work that alone could
//!   hold it (no other connected, not draining, carrying its labels, with room for it idle), it
//!   waits REFUSED_AGAIN_MS and is queued again at a tick from then, kept off nothing (main's
//!   W05-R16, c4d07e523); a runner lost loses its legs, and comes back
//!   with none. A leg that comes back from a runner still
//!   connected takes W05-R04's two looks in their order: it's offered to the runners it may run on,
//!   then the runner it left takes what fits it.
//! - Grants (W05-R34): each placement is a grant, its fence the case's epoch and the placement's
//!   number. With a lease, it expires a lease after it's made unless its runner renews it, naming
//!   it by its fence; at each input, before the input, every grant past its expiry is lost, the
//!   earliest first (then by leg), as its runner's loss would end it, with the second look. A leg
//!   that stops on losing its lease waits the lock-delay past the expiry (or past its runner's
//!   loss, if later) before its retry is queued, at the next tick from then, in the order they
//!   came to wait. A refusal naming a grant that isn't the leg's, a renewal naming one that isn't
//!   the runner's, and an end of an attempt the leg isn't at change nothing.
//! - Draining (W05-R34): a draining runner takes nothing, and its runner's offers leave the fleet;
//!   the runners that legs stay off while another could take them take what fits them, in name
//!   order; undrained, it's back as a runner that came back is.
//! - A grant's two ends (W05-R38): expected at its start plus its p90, else its cost, else its
//!   timeout (360 minutes unsaid), never past its hard end, its start plus its timeout; an
//!   expected end that has passed is moved on, when room kept looks at it, to its start plus twice
//!   the time it has run, never past its hard end, and kept there.
//! - Room kept (W05-R39), with backfilling: as a runner picks, the first of a class, or of a class
//!   before the leg it would take, that it would take but has no room for has that class's first
//!   queued leg by every dimension that waits only for room keep room where it would fit soonest
//!   (trying every runner it may run on, adding up the expected ends there, beside the room kept
//!   for the classes before it; ties to where it would finish first, then by name), or keep the
//!   room it had, moved only to that runner if it would fit there sooner, or worked out again once
//!   its time has passed. A leg may then go to a runner where room is kept for its class or one
//!   before it only if it's expected to end by the room's time, or fits beside it then. Room kept
//!   ends when its leg is granted, leaves the queue or its class, or its runner goes, drains or
//!   comes back; a leg granted over room of a class after its own ends that room.
//! - Gangs (W05-R40): a gang's member goes only with its whole gang, once as many members as make
//!   it are queued, all granted at once (trying every way of putting its members on runners with
//!   room, as many as 4,096 places) or none; a whole gang that can't start now keeps room for each
//!   member, all from the latest of their times. A member refused takes the others' grants back,
//!   each queued again at its attempt once its runner has stopped it (the lock-delay); one lost
//!   before any member ended takes them back to their next attempts, or all fail; after one ended,
//!   none is tried again alone.
//!
//! Pools, warm placement, windows kept for legs, groups, quotas, admission, preferences and retry
//! policies are outside it, and its cases don't make them.
//!
//! The differential test runs [`CASES`] small random cases (at most 4 runners, 12 legs or a gang
//! more, and 40 inputs; big legs, gangs of 2 to 4 and backfilling in half of them) through it and
//! the engine, and compares what runs where (with each grant's fence and
//! expiry), what's queued and what waits to be retried after every input, checking the engine's
//! invariants too: no runner holding more than it has room for, no leg assigned while it runs or on
//! two runners, its queues holding the queued legs alone, its grants as `check_grants` holds. A case
//! where they part is cut down to the fewest inputs that still part and printed with its seed;
//! `KILN_SCHED_REFERENCE_SEED=<seed> cargo test -p kiln-ci --lib sched::reference -- --nocapture`
//! replays one, its inputs in words.

use std::collections::{BTreeMap, BTreeSet};

use super::retry::{Failed, FailureClass};
use super::routing::{LegRoute, RunnerView};
use super::state::{Class, ClassConfig, Command, Config, Cpus, FairBy, Fence, Gang, Input, LeaseLoss, LegIn, Outcome, REFUSED_AGAIN_MS, Resources, RunnerIn, Scheduler, default_classes, own_order};
use super::tests::Draw;
use crate::facts::RunnerFacts;

/// A runner's own admission held to this reference (docs/needs.md E5, one engine).
mod one_runner;

const CASES: u64 = 10_000;
const MINUTE: i64 = 60_000;
const GIB: u64 = 1 << 30;
/// Fair share's dimensions: slots, milli-CPUs, memory bytes, GPUs; and its sets of them, bitmasks.
const DIMS: usize = 4;
const SETS: usize = 16;

/// What runs where (each running leg's runner, attempt, grant's fence and expiry), what's queued,
/// and what waits to be retried, until when.
type Placement = (BTreeMap<String, (String, u32, Fence, Option<i64>)>, BTreeSet<String>, BTreeMap<String, i64>);

/// One input of a case.
#[derive(Debug, Clone)]
enum Step {
    /// A runner connects, or comes back.
    Up(RunnerIn),
    Lost(String),
    Queued(Box<LegIn>),
    Ended { leg: String, attempt: u32, outcome: Outcome },
    /// A runner refused the grant `fence` of the leg: the one it holds, or (a refusal come late)
    /// one that isn't the leg's any more.
    Refused { leg: String, fence: Fence },
    /// A runner renews the grants it names (W05-R34).
    Renew { runner: String, held: Vec<(String, Fence)> },
    Drain(String),
    /// A draining runner is back to taking legs.
    Undrain(RunnerIn),
    /// The time a timer asked for: retries due by now are queued.
    Tick,
    /// An attempt the leg isn't at says it ended (its grant was lost, its runner answers late).
    Stale { leg: String, attempt: u32 },
}

impl Step {
    fn input(&self) -> Input {
        match self {
            Step::Up(r) => Input::RunnerUp(Box::new(r.clone())),
            Step::Lost(runner) => Input::RunnerLost { runner: runner.clone() },
            Step::Queued(l) => Input::LegQueued(l.clone()),
            Step::Ended { leg, attempt, outcome } => Input::AttemptFinished { leg: leg.clone(), attempt: *attempt, outcome: outcome.clone() },
            Step::Refused { leg, fence } => Input::Rejected { leg: leg.clone(), fence: *fence },
            Step::Renew { runner, held } => Input::Renewed { runner: runner.clone(), held: held.clone() },
            Step::Drain(runner) => Input::RunnerDraining { runner: runner.clone() },
            Step::Undrain(r) => Input::RunnerChanged(Box::new(r.clone())),
            Step::Tick => Input::Tick,
            Step::Stale { leg, attempt } => Input::AttemptFinished { leg: leg.clone(), attempt: *attempt, outcome: Outcome::Success },
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
                let stop = if l.on_lease_loss == LeaseLoss::Stop { ", stops on losing its lease" } else { "" };
                let p90 = l.p90.map_or_else(String::new, |ms| format!(", p90 {} s", ms / 1000));
                let timeout = l.timeout.map_or_else(String::new, |ms| format!(", timeout {} min", ms / 60_000));
                let gang = l.gang.as_ref().map_or_else(String::new, |g| format!(", one of gang {} of {}", g.id, g.size));
                format!("{} queued: {}, {}, priority {}{}{asks}, labels {:?}, {cost}{p90}{timeout}{stop}{gang}", l.id, l.repo, l.class.as_str(), l.priority, if l.exclusive { ", exclusive" } else { "" }, l.route.labels)
            }
            Step::Ended { leg, attempt, outcome } => {
                let how = match outcome {
                    Outcome::Success => "success".to_owned(),
                    Outcome::Cancelled => "cancelled".to_owned(),
                    Outcome::Failed(f) => format!("failed, {}", f.class.as_str()),
                };
                format!("{leg}'s attempt {attempt} ended: {how}")
            }
            Step::Refused { leg, fence } => format!("{leg}'s grant {}.{} refused", fence.epoch, fence.n),
            Step::Renew { runner, held } => {
                let named: Vec<String> = held.iter().map(|(l, f)| format!("{l} {}.{}", f.epoch, f.n)).collect();
                format!("{runner} renews [{}]", named.join(", "))
            }
            Step::Drain(runner) => format!("{runner} drains"),
            Step::Undrain(r) => format!("{} undrained", r.id),
            Step::Tick => "tick".to_owned(),
            Step::Stale { leg, attempt } => format!("{leg}'s attempt {attempt} ended, late"),
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
    /// What fair share's tags take: `shares`, the slot's counting the CPUs asked in slots mode.
    fair: [u128; DIMS],
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
    /// In a class with one above: the legs of the classes before its own queued or running when
    /// its wait began, those of them queued then that haven't started since, once none is left
    /// when its bound is due, and when its wait began.
    ahead: Option<Ahead>,
}

/// A queued leg's wait behind the classes before its own (W05-R31): those legs, those of them
/// still to start, when its wait began; once none is left to start, when its bound was due, and
/// when it's looked at next by that bound.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ahead {
    legs: BTreeSet<String>,
    waiting: BTreeSet<String>,
    since: i64,
    due: Option<i64>,
    next: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum At {
    Queued(Queued),
    /// Running there, the `nth` leg placed, and what its placing charged fair share; its grant's
    /// fence and expiry, and its two ends (W05-R38).
    On { runner: String, nth: u64, charged: Box<Charged>, fence: Fence, expires: Option<i64>, ends: Ends },
    /// Waiting to be retried at `at`, the `seq`th to wait (a stop leg's lock-delay, or a gang's
    /// member waiting for its runner to stop it).
    Retrying { at: i64, seq: u64 },
    /// Off its runner, its grant over, before it's queued again or settled in the same input.
    Off,
}

#[derive(Debug, Clone)]
struct Leg {
    input: LegIn,
    attempt: u32,
    at: At,
    avoid: Option<(String, Off)>,
    /// Not queued again before then (a gang's member taken back, W05-R40).
    not_before: Option<i64>,
}

/// A running grant's two ends (W05-R38): when it's expected to end, the end reservations go by,
/// and when it ends at the latest; from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ends {
    from: i64,
    expected: i64,
    hard: i64,
}

impl Ends {
    /// A grant of `l` started at `from`: its hard end is its start plus its timeout (360 minutes
    /// when it says none), its expected end its start plus its p90, else its cost, never past its
    /// hard end, else its hard end.
    fn of(l: &LegIn, from: i64) -> Ends {
        let ms = |m: u64| i64::try_from(m).unwrap_or(i64::MAX / 4);
        let hard = from + ms(l.timeout.unwrap_or(360 * 60_000));
        let expected = l.p90.or(l.cost).map_or(hard, |m| (from + ms(m)).min(hard));
        Ends { from, expected, hard }
    }

    /// As looked at at `now`: an expected end that has passed moves on to its start plus twice the
    /// time it has run, never past its hard end.
    fn at(self, now: i64) -> Ends {
        if self.expected > now {
            return self;
        }
        Ends { expected: (self.from + 2 * (now - self.from).max(0)).min(self.hard), ..self }
    }
}

/// Room kept for a class's first item that no runner has room for (W05-R39), or for each member
/// of its first gang (W05-R40): each item on its runner, from `at`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    items: Vec<(String, String)>,
    at: i64,
}

impl Kept {
    fn holds(&self, leg: &str) -> bool {
        self.items.iter().any(|(l, _)| l == leg)
    }

    fn on(&self, runner: &str) -> bool {
        self.items.iter().any(|(_, r)| r == runner)
    }

    fn on_runner<'a>(&'a self, runner: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        self.items.iter().filter(move |(_, r)| r == runner).map(|(l, _)| l)
    }
}

/// A gang (W05-R40): how many make it whole, its members as they came, and whether one of their
/// attempts has ended since it last started, after which none is tried again alone.
#[derive(Debug, Clone, Default)]
struct Crew {
    size: u32,
    members: BTreeSet<String>,
    ended: bool,
}

/// A leg's place as a runner picks: its tenant's standing in the dimensions the runner offers, its
/// tenant's own order, its arrival.
type Key = ([u128; 3], i128, u64);

/// What a runner sees of a class as it picks (W05-R04, R39, R40): its best queued leg, the least
/// by the runner's dimensions that it would take, has room for and the room kept there lets it;
/// the least of those before it (or of all, with none) that it would take but has no room for;
/// and the whole gang, not passed over already, whose first member comes before it.
#[derive(Debug, Clone, Default)]
struct Sees {
    best: Option<(Key, String)>,
    short: Option<Key>,
    gang: Option<(Key, String)>,
}

#[derive(Debug, Clone)]
struct Runner {
    input: RunnerIn,
    connected: bool,
    draining: bool,
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
    // W05-R03: CPU/memory slot accounting never creates physical GPUs. These cases
    // declare counts without inventories, independently of the engine's per-card matcher.
    if leg.request.gpu > 0 {
        let wanted = on.iter().fold(u64::from(leg.request.gpu), |n, l| n.saturating_add(u64::from(l.request.gpu)));
        if wanted > size.map_or(0, |s| u64::from(s.gpu)) { return false; }
    }
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
    /// The epoch its grants are made in.
    epoch: u64,
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
    /// Legs that came to wait for a retry.
    waited: u64,
    /// What it did that the cases are meant to make it do: legs promoted, placed ahead of a class
    /// before theirs by a floor, and placed over a floor; grants renewed and expired, and stop legs
    /// that waited the lock-delay.
    promoted: u64,
    ahead: u64,
    over: u64,
    renewed: u64,
    expired: u64,
    locked: u64,
    /// Room kept, by class (W05-R39, R40).
    reserved: BTreeMap<Class, Kept>,
    /// Runners whose kept room ended in this input: each takes what fits it before the input is
    /// done (W05-R04).
    unkept: BTreeSet<String>,
    gangs: BTreeMap<String, Crew>,
    /// The input's time.
    now: i64,
    /// While a lost runner's legs come back together (W05-R16), none is offered until all are.
    batch: bool,
    /// What the cases are meant to make it do with room kept and gangs: reservations made, moved,
    /// and worked out again once their time passed; legs granted on a runner where room is kept
    /// for another (backfilled), and legs that fit one but were kept out of its kept room; gangs
    /// granted whole, kept room for whole, and taken back whole.
    made: u64,
    moved: u64,
    again: u64,
    backfilled: u64,
    kept_out: u64,
    whole: u64,
    kept_whole: u64,
    back: u64,
}

impl Reference {
    fn new(cfg: Config, epoch: u64) -> Reference {
        Reference {
            cfg,
            epoch,
            runners: BTreeMap::new(),
            legs: BTreeMap::new(),
            vtimes: BTreeMap::new(),
            finish: BTreeMap::new(),
            served: BTreeMap::new(),
            paced: BTreeMap::new(),
            arrivals: 0,
            placed: 0,
            waited: 0,
            promoted: 0,
            ahead: 0,
            over: 0,
            renewed: 0,
            expired: 0,
            locked: 0,
            reserved: BTreeMap::new(),
            unkept: BTreeSet::new(),
            gangs: BTreeMap::new(),
            now: 0,
            batch: false,
            made: 0,
            moved: 0,
            again: 0,
            backfilled: 0,
            kept_out: 0,
            whole: 0,
            kept_whole: 0,
            back: 0,
        }
    }

    fn running(&self, l: &Leg) -> bool {
        matches!(l.at, At::On { .. })
    }

    /// Whether a step applies now: a runner coming up that's connected, a leg queued that's known,
    /// an attempt ending that isn't running, a refusal of a leg not known, a drain of a runner that
    /// isn't up or drains already (an undrain of one that doesn't), and a late end of the attempt a
    /// leg is at don't, and a case leaves them out.
    fn applies(&self, step: &Step) -> bool {
        match step {
            Step::Up(r) => !self.runners.get(&r.id).is_some_and(|x| x.connected),
            Step::Lost(runner) => self.runners.get(runner).is_some_and(|x| x.connected),
            Step::Queued(l) => !self.legs.contains_key(&l.id),
            Step::Ended { leg, attempt, .. } => self.legs.get(leg).is_some_and(|l| l.attempt == *attempt && self.running(l)),
            Step::Refused { leg, .. } => self.legs.contains_key(leg),
            Step::Renew { runner, .. } => self.runners.contains_key(runner),
            Step::Drain(runner) => self.runners.get(runner).is_some_and(|x| x.connected && !x.draining),
            Step::Undrain(r) => self.runners.get(&r.id).is_some_and(|x| x.connected && x.draining),
            Step::Tick => true,
            Step::Stale { leg, attempt } => self.legs.get(leg).is_some_and(|l| l.attempt != *attempt),
        }
    }

    fn handle(&mut self, step: &Step, now: i64) {
        self.now = now;
        self.promote(now);
        self.expire(now);
        match step {
            // W05-R04's second look, in the engine's order: the runners that legs stayed off while
            // this one was away take what fits them, then this one takes what fits it, each in its
            // own pick order (docs/needs.md E2.1: a runner goes by the dimensions it offers). What
            // it offers may have changed: the room kept on it is worked out again (W05-R39).
            Step::Up(r) | Step::Undrain(r) => {
                self.keep_reserved(|_, k| !k.on(&r.id));
                self.runners.insert(r.id.clone(), Runner { input: r.clone(), connected: true, draining: false });
                for runner in self.avoided() {
                    self.fill(&runner, now);
                }
                self.fill(&r.id, now);
            }
            // Its legs come back together, then go in queue order (`place`); a gang's member takes
            // its gang back as it comes (W05-R40). One its gang took back already ended its
            // attempt here.
            Step::Lost(runner) => {
                if let Some(r) = self.runners.get_mut(runner) {
                    r.connected = false;
                }
                self.batch = true;
                for id in self.on(runner) {
                    if self.runner_of(&id).as_deref() == Some(runner.as_str()) {
                        self.finished(&id, &runner_lost(), now);
                    }
                }
                self.batch = false;
                self.place(now);
            }
            // Queued, it's offered to the runners with room for it; a gang's member starts its
            // gang if that's whole now (W05-R40).
            Step::Queued(l) => {
                self.legs.insert(l.id.clone(), Leg { input: (**l).clone(), attempt: 1, at: At::Queued(Queued { class: l.class, own: 0, arrival: 0, look: None, ahead: None }), avoid: None, not_before: None });
                if let Some(g) = &l.gang {
                    let c = self.gangs.entry(g.id.clone()).or_default();
                    c.size = c.size.max(g.size);
                    c.members.insert(l.id.clone());
                }
                self.enqueue(&l.id, now);
                self.offer(&l.id, now);
            }
            // The attempt may have lost its grant at this input's time, before it.
            Step::Ended { leg, attempt, outcome } => {
                if self.legs.get(leg).is_some_and(|l| l.attempt == *attempt && self.running(l)) {
                    self.finished(leg, outcome, now);
                }
            }
            // W05-R04's two looks, in their order: the leg that came back is offered to the runners
            // it may run on, then the runner it left takes what fits it, then those its gang's other
            // members left, taken back with it (W05-R40).
            Step::Refused { leg, fence } => {
                let live = self.legs.get(leg).is_some_and(|l| matches!(&l.at, At::On { fence: f, .. } if f == fence));
                // A gang's member refused once one of its gang's attempts has ended ends failed.
                let ended = self.legs.get(leg).and_then(|l| l.input.gang.as_ref()).is_some_and(|g| self.gangs.get(&g.id).is_some_and(|c| c.ended));
                if live && ended {
                    self.finished(leg, &Outcome::Failed(Failed { class: FailureClass::Infra, exit_code: None, log_hits: Vec::new() }), now);
                } else if live {
                    let left = self.runner_of(leg);
                    self.release(leg, now);
                    let freed = self.take_back_gang(leg, now);
                    // A runner draining refused it for that, not for the leg: the leg isn't kept off it.
                    let away = left.as_ref().and_then(|r| self.runners.get(r)).is_some_and(|r| r.draining || !r.connected);
                    // Main's W05-R16 (c4d07e523): refused by a runner at work when no other connected
                    // runner that isn't draining carries its labels and is big enough to hold it
                    // (room for it, idle), it's asked again in REFUSED_AGAIN_MS, on the same
                    // attempt, kept off nothing; that runner takes what fits it meanwhile.
                    let alone = !away
                        && left.as_ref().zip(self.legs.get(leg)).is_some_and(|(runner, l)| !self.runners.values().any(|o| o.input.id != *runner && o.connected && !o.draining && carries(&o.input, &l.input) && room(&self.cfg, &o.input, &[], &l.input)));
                    if alone && let Some(runner) = &left {
                        self.waited += 1;
                        let seq = self.waited;
                        if let Some(l) = self.legs.get_mut(leg) {
                            l.at = At::Retrying { at: now + REFUSED_AGAIN_MS, seq };
                        }
                        self.fill(runner, now);
                    } else if let Some(l) = self.legs.get_mut(leg)
                        && let Some(runner) = &left
                    {
                        if !away {
                            l.avoid = Some((runner.clone(), Off::Refused));
                        }
                        self.enqueue(leg, now);
                        self.offer(leg, now);
                        self.fill(runner, now);
                    }
                    for r in freed {
                        self.fill(&r, now);
                    }
                }
            }
            // Each grant it names that's its own, by that fence, lives a lease from now.
            Step::Renew { runner, held } => {
                let until = self.cfg.lease.map(|ms| now + i64::try_from(ms).unwrap_or(i64::MAX / 4));
                for (id, fence) in held {
                    if let (Some(until), Some(l)) = (until, self.legs.get_mut(id))
                        && let At::On { runner: r, fence: f, expires: Some(e), .. } = &mut l.at
                        && r == runner
                        && f == fence
                        && *e < until
                    {
                        *e = until;
                        self.renewed += 1;
                    }
                }
            }
            // The runners legs stay off while another could take them take what fits them.
            Step::Drain(runner) => {
                if let Some(r) = self.runners.get_mut(runner) {
                    r.draining = true;
                }
                for r in self.avoided() {
                    self.fill(&r, now);
                }
            }
            // Retries due by now are queued, the earliest first, then in the order they came to
            // wait, each offered as it's queued.
            Step::Tick => {
                let mut due: Vec<(i64, u64, String)> = self.legs.iter().filter_map(|(id, l)| if let At::Retrying { at, seq } = l.at { (at <= now).then(|| (at, seq, id.clone())) } else { None }).collect();
                due.sort();
                for (_, _, id) in due {
                    self.enqueue(&id, now);
                    self.offer(&id, now);
                }
            }
            Step::Stale { .. } => {}
        }
        // The runners whose kept room ended take what fits them; then nothing is left that may run
        // somewhere, as the engine leaves it.
        self.unkept_fill(now);
        self.place(now);
        self.unkept_fill(now);
        self.arm_ahead(now);
        self.ahead_due(now);
    }

    /// Each runner whose kept room ended takes what fits it, in name order, until none is left.
    fn unkept_fill(&mut self, now: i64) {
        let mut turns = 0;
        loop {
            turns += 1;
            assert!(turns < 10_000, "runners whose kept room ended take what fits them forever: {:?}, room kept {:?}", self.unkept, self.reserved);
            self.drop_stale();
            let Some(r) = self.unkept.pop_first() else { return };
            self.fill(&r, now);
        }
    }

    /// The runners that queued legs stay off only while another could take them (W05-R16).
    fn avoided(&self) -> BTreeSet<String> {
        self.legs.values().filter(|l| matches!(l.at, At::Queued(..))).filter_map(|l| l.avoid.as_ref().filter(|(_, off)| *off == Off::Lost).map(|(runner, _)| runner.clone())).collect()
    }

    /// Every grant past its expiry by `now` is lost, the earliest first, then by leg (W05-R34): as
    /// a runner-lost end of its attempt, with W05-R04's two looks.
    fn expire(&mut self, now: i64) {
        loop {
            let due = self.legs.iter().filter_map(|(id, l)| if let At::On { expires: Some(e), .. } = &l.at { (*e <= now).then(|| (*e, id.clone())) } else { None }).min();
            let Some((_, id)) = due else { return };
            self.expired += 1;
            self.finished(&id, &runner_lost(), now);
        }
    }

    /// The earliest grant's expiry or retry to come: when the engine's timer is due.
    fn next_timer(&self) -> Option<i64> {
        self.legs
            .values()
            .filter_map(|l| match &l.at {
                At::On { expires, .. } => *expires,
                At::Retrying { at, .. } => Some(*at),
                At::Queued(..) | At::Off => None,
            })
            .min()
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

    /// Every look due by `now`, the earliest time first, every leg looked at then (by id) before
    /// any of them takes its watch in its new class: a leg whose class started a leg at its
    /// signature in its bound before the look is looked at again a bound after that start; one
    /// whose class started none there goes up a class. Its bound: once the work ahead of it was
    /// due done, the time from its wait's start to then; else its class's, if set.
    fn promote(&mut self, now: i64) {
        let times = |q: &Queued| [q.look, q.ahead.as_ref().and_then(|a| a.next)];
        loop {
            let at = self.legs.values().filter_map(|l| if let At::Queued(q) = &l.at { times(q).into_iter().flatten().min() } else { None }).filter(|t| *t <= now).min();
            let Some(at) = at else { return };
            let ids: Vec<String> = self.legs.iter().filter(|(_, l)| matches!(&l.at, At::Queued(q) if times(q).contains(&Some(at)))).map(|(id, _)| id.clone()).collect();
            let mut came: Vec<String> = Vec::new();
            for id in ids {
                let Some(l) = self.legs.get(&id) else { continue };
                let At::Queued(q) = &l.at else { continue };
                if !times(q).contains(&Some(at)) {
                    continue;
                }
                let (class, sig) = (q.class, signature(&l.input));
                let derived = q.ahead.as_ref().filter(|a| a.waiting.is_empty()).and_then(|a| a.due.filter(|d| *d <= at).map(|d| (d - a.since).max(0)));
                let bound = derived.or_else(|| self.class_config(class).bound.map(|b| i64::try_from(b).unwrap_or(i64::MAX / 2)));
                let (Some(bound), Some(up)) = (bound, class.up()) else {
                    if let Some(At::Queued(q)) = self.legs.get_mut(&id).map(|l| &mut l.at) {
                        q.look = q.look.filter(|t| *t != at);
                        if let Some(a) = q.ahead.as_mut() {
                            a.next = a.next.filter(|t| *t != at);
                        }
                    }
                    continue;
                };
                if let Some(t) = self.served.get(&(sig, class)).copied().filter(|t| *t > at - bound) {
                    // Served there: each look due now is taken again a bound after that start.
                    if let Some(At::Queued(q)) = self.legs.get_mut(&id).map(|l| &mut l.at) {
                        if q.look == Some(at) {
                            q.look = Some(t + bound);
                        }
                        if let Some(a) = q.ahead.as_mut().filter(|a| a.next == Some(at)) {
                            a.next = Some(t + bound);
                        }
                    }
                    continue;
                }
                self.promoted += 1;
                let look = self.look(up, at);
                // A gang's members share a class (W05-R40): its queued members in that class go up
                // with it.
                for m in std::iter::once(id.clone()).chain(self.mates(&id)) {
                    if let Some(At::Queued(q)) = self.legs.get_mut(&m).map(|l| &mut l.at)
                        && q.class == class
                    {
                        (q.class, q.look, q.ahead) = (up, look, None);
                        came.push(m);
                    }
                }
            }
            // Each come to its class at the look, all of them in it: what's ahead of it then, and
            // what of that is due already is due from then.
            for m in came {
                let class = if let Some(At::Queued(q)) = self.legs.get(&m).map(|l| &l.at) { q.class } else { continue };
                let ahead = self.ahead_of(&m, class, at);
                if let Some(At::Queued(q)) = self.legs.get_mut(&m).map(|l| &mut l.at) {
                    q.ahead = ahead;
                }
            }
            self.ahead_due(at);
        }
    }


    /// A gang's members other than `id`, if it's one's.
    fn mates(&self, id: &str) -> Vec<String> {
        let Some(g) = self.legs.get(id).and_then(|l| l.input.gang.as_ref()) else { return Vec::new() };
        self.gangs.get(&g.id).map(|c| c.members.iter().filter(|m| m.as_str() != id).cloned().collect()).unwrap_or_default()
    }

    /// A runner takes the best queued legs it has room for, in its own pick order (by the
    /// dimensions it offers), until none fits (W05-R04: new capacity looks at what that runner can
    /// serve). A whole gang whose first member comes before the best of the first class with
    /// either starts whole, or is passed over as an item without room is (W05-R40), once the room
    /// worked out again for its class and those before stands as it was. With
    /// backfilling, before it takes a leg while the first of that leg's class, or of a class before
    /// it, had no room there, those firsts keep room where it frees first, and it picks again,
    /// held to that room (W05-R39).
    fn fill(&mut self, runner: &str, now: i64) {
        self.drop_stale();
        let mut stuck: BTreeSet<String> = BTreeSet::new();
        let mut turns = 0;
        loop {
            turns += 1;
            let sees = self.bests(runner, &stuck);
            assert!(turns < 10_000, "{runner} picks forever: it sees {sees:?}, room kept {:?}, gangs passed over {stuck:?}", self.reserved);
            let mut passed: BTreeMap<Class, bool> = sees.iter().map(|(c, s)| (*c, s.short.is_some_and(|u| s.best.as_ref().is_none_or(|(b, _)| u < *b)))).collect();
            if let Some((c, g)) = sees.iter().find(|(_, s)| s.best.is_some() || s.gang.is_some()).and_then(|(c, s)| s.gang.as_ref().map(|(_, g)| (*c, g.clone()))) {
                if self.place_gang(&g, now) {
                    continue;
                }
                passed.insert(c, true);
                // Room worked out again may let it start: it's tried again, else passed over.
                if self.cfg.backfill && self.reserve(runner, &passed, c, now) {
                    continue;
                }
                stuck.insert(g);
                continue;
            }
            let Some((class, id, over)) = self.choose(runner, &sees, now) else { return };
            if self.cfg.backfill && passed.iter().any(|(c, p)| *p && *c <= class) && self.reserve(runner, &passed, class, now) {
                continue;
            }
            self.assign(&id, runner, now, over);
        }
    }

    /// What a runner sees of each class it may take legs of ([`Sees`]), its gang's members none of
    /// them its best, `stuck` gangs passed over already.
    fn bests(&mut self, runner: &str, stuck: &BTreeSet<String>) -> BTreeMap<Class, Sees> {
        let mut sees: BTreeMap<Class, Sees> = BTreeMap::new();
        let Some(r) = self.runners.get(runner).filter(|r| r.connected) else { return sees };
        let (dims, held) = (self.dims(&r.input), self.reserved.values().any(|k| k.on(runner)));
        let mut kept_out = 0;
        for (id, l) in &self.legs {
            let At::Queued(q) = &l.at else { continue };
            if !carries(&r.input, &l.input) {
                continue;
            }
            let key = (self.standing(q.class, &l.input, dims), q.own, q.arrival);
            let s = sees.entry(q.class).or_default();
            if let Some(g) = &l.input.gang {
                if !stuck.contains(&g.id) && self.whole(&g.id) && s.gang.as_ref().is_none_or(|(k, _)| key < *k) {
                    s.gang = Some((key, g.id.clone()));
                }
                continue;
            }
            if self.may_run(l, r) {
                if held && !self.allowed(r, l, &[]) {
                    kept_out += 1;
                    continue;
                }
                if s.best.as_ref().is_none_or(|(k, _)| key < *k) {
                    s.best = Some((key, id.clone()));
                }
            } else if self.admits(r, l) && s.short.is_none_or(|k| key < k) {
                s.short = Some(key);
            }
        }
        // A whole gang counts where it comes before its class's best.
        for s in sees.values_mut() {
            if let (Some((g, _)), Some((b, _))) = (&s.gang, &s.best)
                && g >= b
            {
                s.gang = None;
            }
        }
        self.kept_out += kept_out;
        sees
    }

    /// The leg a runner takes next of those it sees: the best of the first class with one, unless
    /// a class after that one is short of its floor (W05-R31): then the first such class's best,
    /// if it keeps the class within its floor, or once its pace has passed if not (with what's
    /// left of its floor). Its class too.
    fn choose(&mut self, runner: &str, sees: &BTreeMap<Class, Sees>, now: i64) -> Option<(Class, String, Option<u128>)> {
        let r = self.runners.get(runner)?;
        let mut bests = sees.iter().filter_map(|(c, s)| s.best.as_ref().map(|(_, id)| (*c, id)));
        let (first, id) = bests.next()?;
        let fleet = self.fleet();
        for (class, x) in bests {
            let floor = u128::from(self.class_config(class).floor) * 1000;
            let held = self.held(class);
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
                return Some((class, x.clone(), None));
            }
            if self.paced.get(&class).is_none_or(|t| now >= *t) {
                self.ahead += 1;
                self.over += 1;
                return Some((class, x.clone(), Some(floor - share(&held, &fleet))));
            }
        }
        Some((first, id.clone(), None))
    }

    /// A leg queued again is offered to the runners it may run on, best first, each taking the best
    /// legs it has room for, until it's taken (W05-R04: queuing a leg looks at its runners); a
    /// gang's member starts its gang if that's whole and can start now (W05-R40). None while a
    /// lost runner's legs come back together.
    fn offer(&mut self, id: &str, now: i64) {
        if self.batch || !self.legs.get(id).is_some_and(|l| matches!(l.at, At::Queued(..))) {
            return;
        }
        if let Some(g) = self.legs.get(id).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone()) {
            self.place_gang(&g, now);
            return;
        }
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

    /// An attempt ended (W05-R16, R40): a gang's member lost while it ran, before any of its
    /// gang's attempts ended, takes its gang with it ([`Self::gang_lost`]); else a gang's member
    /// ends as it ended, and once one has, none of its gang is tried again alone. The rest as
    /// [`Self::finish`] says.
    fn finished(&mut self, id: &str, outcome: &Outcome, now: i64) {
        let Some(l) = self.legs.get(id) else { return };
        if let Some(g) = l.input.gang.as_ref().map(|g| g.id.clone()) {
            let lost = matches!(outcome, Outcome::Failed(f) if f.class == FailureClass::RunnerLost) && self.running(l);
            if lost && self.gangs.get(&g).is_some_and(|c| !c.ended) {
                return self.gang_lost(&g, id, outcome, now);
            }
            if let Some(c) = self.gangs.get_mut(&g) {
                c.ended = true;
            }
            return self.finish(id, outcome, false, now);
        }
        self.finish(id, outcome, true, now);
    }

    /// An attempt's end: one its runner lost is retried once when `retry`, elsewhere when it can be
    /// (W05-R16), a stop leg's retry waiting the lock-delay past its grant's expiry or `now`, the
    /// later (W05-R34), a gang's member taken back not before its runner has stopped it (W05-R40);
    /// the rest settle. Then W05-R04's two looks: the leg queued again is offered, and the runner
    /// it left takes what fits it.
    fn finish(&mut self, id: &str, outcome: &Outcome, retry: bool, now: i64) {
        let Some(l) = self.legs.get(id) else { return };
        let At::On { runner, expires, .. } = &l.at else { return };
        let (left, expires) = (runner.clone(), *expires);
        self.release(id, now);
        let lock = i64::try_from(self.cfg.lock_delay).unwrap_or(i64::MAX / 4);
        let Some(l) = self.legs.get_mut(id) else { return };
        match outcome {
            Outcome::Failed(f) if f.class == FailureClass::RunnerLost && retry && l.attempt < 2 => {
                l.attempt += 1;
                l.avoid = Some((left.clone(), Off::Lost));
                let stop = l.input.on_lease_loss == LeaseLoss::Stop;
                let mut at = if stop { expires.unwrap_or(now).max(now) + lock } else { now };
                if let Some(t) = l.not_before.filter(|t| *t > now) {
                    at = at.max(t);
                }
                if at > now {
                    self.waited += 1;
                    if stop {
                        self.locked += 1;
                    }
                    l.at = At::Retrying { at, seq: self.waited };
                } else {
                    self.enqueue(id, now);
                    self.offer(id, now);
                }
            }
            _ => self.forget(id),
        }
        self.fill(&left, now);
    }

    /// A leg done for good, out of its gang.
    fn forget(&mut self, id: &str) {
        let gone = self.legs.remove(id);
        if let Some(g) = gone.as_ref().and_then(|l| l.input.gang.as_ref())
            && let Some(c) = self.gangs.get_mut(&g.id)
        {
            c.members.remove(id);
            if c.members.is_empty() {
                self.gangs.remove(&g.id);
            }
        }
    }

    /// Its grant over: what its placing charged settled, and off its runner.
    fn release(&mut self, id: &str, now: i64) {
        self.settle(id, now);
        if let Some(l) = self.legs.get_mut(id) {
            l.at = At::Off;
        }
    }

    /// Queues a leg, or queues it again: in the class it came with, its place in its tenant's own
    /// order by its priority and `now`, its arrival (fair share's standing is taken as legs are
    /// picked). One not to go again before a time waits for it as a retry does (W05-R40).
    fn enqueue(&mut self, id: &str, now: i64) {
        if let Some(at) = self.legs.get(id).and_then(|l| l.not_before).filter(|at| *at > now) {
            self.waited += 1;
            if let Some(l) = self.legs.get_mut(id) {
                l.at = At::Retrying { at, seq: self.waited };
            }
            return;
        }
        self.arrivals += 1;
        let (arrival, aging) = (self.arrivals, self.cfg.aging_per_minute);
        let Some(class) = self.legs.get(id).map(|l| l.input.class) else { return };
        // A gang's members share a class (W05-R40): one queued while others wait joins theirs.
        let class = self.mates(id).into_iter().find_map(|m| if let Some(At::Queued(q)) = self.legs.get(&m).map(|x| &x.at) { Some(q.class) } else { None }).unwrap_or(class);
        let look = self.look(class, now);
        let ahead = self.ahead_of(id, class, now);
        if let Some(l) = self.legs.get_mut(id) {
            l.at = At::Queued(Queued { class, own: own_order(l.input.priority, aging, now), arrival, look, ahead });
        }
    }

    /// For a leg coming to `class` at `since`, if a class is above it: the other legs of the
    /// classes before it queued or running now (none: its wait hasn't begun).
    fn ahead_of(&self, id: &str, class: Class, since: i64) -> Option<Ahead> {
        class.up()?;
        let legs: BTreeSet<String> = self
            .legs
            .iter()
            .filter(|(other, l)| {
                other.as_str() != id
                    && match &l.at {
                        At::Queued(q) => q.class < class,
                        At::On { charged, .. } => charged.class < class,
                        At::Retrying { .. } | At::Off => false,
                    }
            })
            .map(|(other, _)| other.clone())
            .collect();
        let waiting = legs.iter().filter(|a| matches!(self.legs.get(*a).map(|x| &x.at), Some(At::Queued(_)))).cloned().collect();
        Some(Ahead { legs, waiting, since, due: None, next: None })
    }

    /// Each queued leg with nothing of the classes before its own ahead of it yet, once some is
    /// queued or running: its wait starts now, behind that.
    fn arm_ahead(&mut self, now: i64) {
        let unarmed: Vec<(String, Class)> = self.legs.iter().filter_map(|(id, l)| if let At::Queued(Queued { class, ahead: Some(a), .. }) = &l.at { a.legs.is_empty().then(|| (id.clone(), *class)) } else { None }).collect();
        for (id, class) in unarmed {
            let Some(ahead) = self.ahead_of(&id, class, now).filter(|a| !a.legs.is_empty()) else { continue };
            if let Some(At::Queued(q)) = self.legs.get_mut(&id).map(|l| &mut l.at) {
                q.ahead = Some(ahead);
            }
        }
    }

    /// A leg placed: no `interactive` leg's wait counts it as waiting any more.
    fn ahead_started(&mut self, leg: &str) {
        for l in self.legs.values_mut() {
            if let At::Queued(Queued { ahead: Some(a), .. }) = &mut l.at {
                a.waiting.remove(leg);
            }
        }
    }

    /// Each queued leg none of whose work ahead waits any more, with no time set yet: its bound is
    /// due at `now` or the latest expected end of that work still running.
    fn ahead_due(&mut self, now: i64) {
        let mut set: Vec<(String, i64)> = Vec::new();
        for (id, l) in &self.legs {
            let At::Queued(Queued { ahead: Some(Ahead { legs, waiting, due: None, .. }), .. }) = &l.at else { continue };
            if legs.is_empty() {
                continue;
            }
            // Each queued when it came has started since or left; and none is queued now (one lost
            // and queued again waits again).
            if !waiting.is_empty() && waiting.iter().any(|a| matches!(self.legs.get(a).map(|x| &x.at), Some(At::Queued(_)))) {
                continue;
            }
            if legs.iter().any(|a| matches!(self.legs.get(a).map(|x| &x.at), Some(At::Queued(_)))) {
                continue;
            }
            let due = legs.iter().filter_map(|a| if let Some(At::On { ends, .. }) = self.legs.get(a).map(|x| &x.at) { Some(ends.at(now).expected) } else { None }).fold(now, i64::max);
            set.push((id.clone(), due));
        }
        for (id, due) in set {
            if let Some(At::Queued(Queued { ahead: Some(a), .. })) = self.legs.get_mut(&id).map(|l| &mut l.at) {
                a.waiting.clear();
                (a.due, a.next) = (Some(due), Some(due));
            }
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

    /// What the connected runners that aren't draining offer, in each dimension.
    fn fleet(&self) -> [u64; DIMS] {
        let mut fleet = [0u64; DIMS];
        for x in self.runners.values().filter(|x| x.connected && !x.draining) {
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
    /// dimension) goes to the best runner the room kept there lets it go to (W05-R39), a gang's
    /// member with its whole gang, if that can start now (W05-R40): as legs whose places changed
    /// together go in turn.
    fn place(&mut self, now: i64) {
        let mut turns = 0;
        'next: loop {
            turns += 1;
            assert!(turns < 10_000, "legs placed forever: {:?}", self.queue_order());
            for id in self.queue_order() {
                if let Some(g) = self.legs.get(&id).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone()) {
                    if self.place_gang(&g, now) {
                        continue 'next;
                    }
                    continue;
                }
                if let Some(r) = self.best(&id) {
                    self.assign(&id, &r, now, None);
                    continue 'next;
                }
            }
            return;
        }
    }

    /// The queued legs in queue order: by class, then by every dimension (their tenants'
    /// standing, their own order, their arrival).
    fn queue_order(&self) -> Vec<String> {
        let mut order: Vec<((Class, [u128; 3], i128, u64), String)> =
            self.legs.iter().filter_map(|(id, l)| if let At::Queued(q) = &l.at { Some(((q.class, self.standing(q.class, &l.input, [true; DIMS]), q.own, q.arrival), id.clone())) } else { None }).collect();
        order.sort();
        order.into_iter().map(|(_, id)| id).collect()
    }

    // ------------------------------------------------------------------ gangs (W05-R40)

    /// A gang's members queued now, in queue order.
    fn crew_queued(&self, g: &str) -> Vec<String> {
        let members = self.gangs.get(g).map(|c| c.members.clone()).unwrap_or_default();
        self.queue_order().into_iter().filter(|id| members.contains(id)).collect()
    }

    /// Whether a gang is whole: as many of its members queued as make it.
    fn whole(&self, g: &str) -> bool {
        let Some(c) = self.gangs.get(g) else { return false };
        let queued = c.members.iter().filter(|m| self.legs.get(m.as_str()).is_some_and(|l| matches!(l.at, At::Queued(..)))).count();
        queued >= usize::try_from(c.size.max(1)).unwrap_or(usize::MAX)
    }

    /// Where a whole gang would start now, if it can: each member, in queue order, on a runner
    /// that would take it with room for it beside the members put there before it, as the room
    /// kept there lets it, the least loaded first (the fewest legs, counting those members), then
    /// by name; where a member has no such runner, the members before it put elsewhere, in that
    /// order, trying at most 4,096 places in all.
    fn gang_plan(&self, g: &str) -> Option<Vec<(String, String)>> {
        self.plan_gang(g, true)
    }

    /// [`Self::gang_plan`] by room alone, the room kept aside.
    fn room_plan(&self, g: &str) -> Option<Vec<(String, String)>> {
        self.plan_gang(g, false)
    }

    fn plan_gang(&self, g: &str, held: bool) -> Option<Vec<(String, String)>> {
        let members = self.crew_queued(g);
        let (mut plan, mut budget) = (Vec::new(), 4096usize);
        (!members.is_empty() && self.plan_from(&members, &mut plan, &mut budget, held)).then_some(plan)
    }

    fn plan_from(&self, members: &[String], plan: &mut Vec<(String, String)>, budget: &mut usize, held: bool) -> bool {
        let Some(m) = members.get(plan.len()) else { return true };
        for r in self.gang_places(m, plan, held) {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            plan.push((m.clone(), r));
            if self.plan_from(members, plan, budget, held) {
                return true;
            }
            plan.pop();
        }
        false
    }

    fn gang_places(&self, m: &str, plan: &[(String, String)], held: bool) -> Vec<String> {
        let Some(l) = self.legs.get(m) else { return Vec::new() };
        let mut places: Vec<(usize, String)> = Vec::new();
        for r in self.runners.values().filter(|r| r.connected && carries(&r.input, &l.input) && self.admits(r, l)) {
            let mates: Vec<&LegIn> = plan.iter().filter(|(_, on)| *on == r.input.id).filter_map(|(x, _)| self.legs.get(x)).map(|x| &x.input).collect();
            let mut on: Vec<&LegIn> = self.on(&r.input.id).iter().filter_map(|id| self.legs.get(id)).map(|x| &x.input).collect();
            let running = on.len();
            on.extend(mates.iter().copied());
            if room(&self.cfg, &r.input, &on, &l.input) && (!held || self.allowed(r, l, &mates)) {
                places.push((running + mates.len(), r.input.id.clone()));
            }
        }
        places.sort();
        places.into_iter().map(|(_, r)| r).collect()
    }

    /// A whole gang starts if it can, each member granted where [`Self::gang_plan`] puts it.
    fn place_gang(&mut self, g: &str, now: i64) -> bool {
        if !self.whole(g) {
            return false;
        }
        let Some(plan) = self.gang_plan(g).or_else(|| self.forgiven_plan(g)) else { return false };
        if let Some(c) = self.gangs.get_mut(g) {
            c.ended = false;
        }
        self.whole += 1;
        for (m, r) in plan {
            self.assign(&m, &r, now, None);
        }
        true
    }

    /// A whole gang with no place while its members stay off runners that refused them: its place
    /// with those refusals forgiven, if it then has one.
    fn forgiven_plan(&mut self, g: &str) -> Option<Vec<(String, String)>> {
        let refused: Vec<(String, (String, Off))> = self.crew_queued(g).into_iter().filter_map(|m| self.legs.get(&m).and_then(|l| l.avoid.clone()).filter(|(_, off)| *off == Off::Refused).map(|a| (m, a))).collect();
        if refused.is_empty() {
            return None;
        }
        for (m, _) in &refused {
            if let Some(l) = self.legs.get_mut(m) {
                l.avoid = None;
            }
        }
        let plan = self.gang_plan(g);
        if plan.is_none() {
            for (m, a) in refused {
                if let Some(l) = self.legs.get_mut(&m) {
                    l.avoid = Some(a);
                }
            }
        }
        plan
    }

    /// The grants of a gang's members other than `but`, refused, are taken back: each queued again
    /// at its attempt once its runner has stopped it (the lock-delay). The runners they leave.
    fn take_back_gang(&mut self, but: &str, now: i64) -> Vec<String> {
        let Some(g) = self.legs.get(but).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone()) else { return Vec::new() };
        let peers: Vec<String> = self.gangs.get(&g).map(|c| c.members.iter().filter(|m| *m != but).cloned().collect()).unwrap_or_default();
        let mut freed = Vec::new();
        for p in peers {
            let Some(runner) = self.runner_of(&p) else { continue };
            self.back += 1;
            freed.push(runner);
            self.release(&p, now);
            self.stopping(&p, now);
            self.enqueue(&p, now);
            self.offer(&p, now);
        }
        freed
    }

    /// A gang's member taken back goes again no sooner than the lock-delay, by which its runner,
    /// told to stop it, has.
    fn stopping(&mut self, id: &str, now: i64) {
        let at = now + i64::try_from(self.cfg.lock_delay).unwrap_or(i64::MAX / 4);
        if let Some(l) = self.legs.get_mut(id) {
            l.not_before = Some(at);
        }
    }

    /// A gang's member lost while it ran, before any of its gang's attempts ended: the others are
    /// taken back, and every member's attempt ends lost; the gang goes again whole at the next
    /// attempts if each may be tried again (once, W05-R16), else every member ends failed.
    fn gang_lost(&mut self, g: &str, lost: &str, outcome: &Outcome, now: i64) {
        let peers: Vec<(String, u32)> = self.gangs.get(g).map(|c| c.members.iter().filter(|m| *m != lost).filter_map(|m| self.legs.get(m).filter(|l| self.running(l)).map(|l| (m.clone(), l.attempt))).collect()).unwrap_or_default();
        let again = self.legs.get(lost).is_some_and(|l| l.attempt < 2) && peers.iter().all(|(_, a)| *a < 2);
        if let Some(c) = self.gangs.get_mut(g) {
            c.ended = true;
        }
        self.back += 1;
        self.finish(lost, outcome, again, now);
        for (p, _) in &peers {
            self.stopping(p, now);
            self.finish(p, &runner_lost(), again, now);
        }
        if again {
            // Only the lost member's runner was lost: the others may go back to theirs.
            for (p, _) in &peers {
                if let Some(l) = self.legs.get_mut(p)
                    && l.avoid.as_ref().is_some_and(|(_, off)| *off == Off::Lost)
                {
                    l.avoid = None;
                }
            }
            self.place_gang(g, now);
        }
    }

    /// A leg placed on a runner, each node of its tenant charged in its class's tree: in each
    /// dimension the runner offers, its share of the fleet (what it holds there over what the
    /// connected runners offer, millionths rounded up) × its cost × 1000 / the node's weight, from
    /// `max(V[parent][d], finish[node][d])`; what it charged kept, to settle when it ends. Its
    /// class started a leg at its signature; placed over its class's floor (`over`, what was left
    /// of it), the class's pace moves on.
    fn assign(&mut self, leg: &str, runner: &str, now: i64, over: Option<u128>) {
        self.ahead_started(leg);
        if self.reserved.values().any(|k| k.on(runner) && !k.holds(leg)) {
            self.backfilled += 1;
        }
        self.granted_over(leg, runner);
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
        // Main's 8305d5c80 (W05-R10): where the runner offers no CPUs to count (slots mode), the
        // slot a leg holds counts the CPUs it asks, one at the least, in thousandths rounded up; a
        // leg holding the runner whole counts as it is. Fair share's tags take these; the floor's
        // holds and pace take `shares`.
        let mut fair = shares;
        if self.offers(&r.input).get(1).copied().unwrap_or(0) == 0
            && claim(&l.input, size(&r.input)) != Some(Claim::Whole)
            && let Some(slot) = fair.first_mut()
        {
            *slot = (*slot * u128::from(l.input.request.cpu.get().max(1000))).div_ceil(1000);
        }
        let (nodes, ms) = (self.nodes(&l.input), u128::from(l.input.cost.unwrap_or(self.cfg.default_cost)));
        let sig = signature(&l.input);
        let mut parent = String::new();
        for (node, weight) in &nodes {
            let mut v = self.vtimes.get(&(class, parent.clone())).copied().unwrap_or([0; SETS]);
            let mut f = self.finish.get(&(class, node.clone())).copied().unwrap_or([0; SETS]);
            for set in 0..SETS {
                let cost = set_cost(&fair, ms, *weight, set);
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
        let charged = Charged { class, nodes, shares, fair, ms, at: now, holds, paced };
        // Its grant: the next fence, and a lease from now (W05-R34).
        let (fence, expires) = (Fence { epoch: self.epoch, n: self.placed }, self.cfg.lease.map(|ms| now + i64::try_from(ms).unwrap_or(i64::MAX / 4)));
        if let Some(l) = self.legs.get_mut(leg) {
            let ends = Ends::of(&l.input, now);
            l.at = At::On { runner: runner.to_owned(), nth: self.placed, charged: Box::new(charged), fence, expires, ends };
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
                    if set_cost(&charged.fair, charged.ms, *weight, set) > 0 {
                        *f = f.saturating_sub(set_cost(&charged.fair, charged.ms, *weight, set)) + set_cost(&charged.fair, ran, *weight, set);
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

    /// The best runner a leg may run on now, as the room kept there lets it: least loaded, then
    /// most free slots, then name.
    fn best(&self, id: &str) -> Option<String> {
        let l = self.legs.get(id)?;
        let best = self.runners.values().filter(|r| self.may_run(l, r) && self.allowed(r, l, &[])).min_by(|a, b| self.better(a, b));
        best.map(|r| r.input.id.clone())
    }

    /// Which of two runners a leg goes to first: the less loaded, then the one with more free
    /// slots, then by name.
    fn better(&self, a: &Runner, b: &Runner) -> std::cmp::Ordering {
        let load = |r: &Runner| (self.on(&r.input.id).len() as u128, u128::from(r.input.slots.max(1)));
        let ((na, sa), (nb, sb)) = (load(a), load(b));
        (na * sb).cmp(&(nb * sa)).then_with(|| sb.saturating_sub(nb).cmp(&sa.saturating_sub(na))).then_with(|| a.input.id.cmp(&b.input.id))
    }

    /// Whether a runner takes a leg now: it would ([`Self::admits`]), it carries the leg's labels,
    /// and it has room for it.
    fn may_run(&self, l: &Leg, r: &Runner) -> bool {
        let on: Vec<&LegIn> = self.on(&r.input.id).iter().filter_map(|id| self.legs.get(id)).map(|l| &l.input).collect();
        self.admits(r, l) && carries(&r.input, &l.input) && room(&self.cfg, &r.input, &on, &l.input)
    }

    /// Whether a runner would take a leg once it has room: connected, not draining, not one the
    /// leg stays off.
    fn admits(&self, r: &Runner, l: &Leg) -> bool {
        r.connected && !r.draining && !self.stays_off(l, &r.input.id)
    }

    // ------------------------------------------------------------------ room kept (W05-R39)

    /// Works out the room kept for each class up to `upto` whose first leg the runner passed over
    /// for want of room: the class's first queued leg by every dimension that waits only for room
    /// keeps room on the runner where it would fit soonest; a whole gang, each member on the
    /// runner where its room frees soonest, all from the latest of their times. Room kept stands
    /// until its leg is granted, cancelled or no longer first, and moves only to where it would fit
    /// sooner (here); once its time has passed it's worked out again. Whether any was made, moved
    /// or dropped.
    fn reserve(&mut self, runner: &str, passed: &BTreeMap<Class, bool>, upto: Class, now: i64) -> bool {
        let mut changed = false;
        for class in Class::ALL.into_iter().filter(|c| *c <= upto && passed.get(c).copied().unwrap_or(false)) {
            let Some(head) = self.head(class) else {
                changed |= self.unkeep(class);
                continue;
            };
            let gang = self.legs.get(&head).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone());
            let was = self.reserved.get(&class).filter(|k| k.holds(&head)).cloned();
            let standing = was.clone().filter(|k| k.at > now);
            let next = match (&standing, &gang) {
                (Some(_), Some(_)) => None,
                (None, Some(g)) => self.soonest_gang(g, now),
                (Some(k), None) if k.on(runner) => None,
                (Some(k), None) => self.earliest(&head, runner, now, &[]).filter(|at| *at < k.at).map(|at| (vec![(head.clone(), runner.to_owned())], at)),
                (None, None) => self.soonest(&head, now).map(|(on, at)| (vec![(head.clone(), on)], at)),
            };
            let Some((items, at)) = next else {
                if standing.is_none() {
                    changed |= self.unkeep(class);
                }
                continue;
            };
            // Worked out again as it stood: nothing changed.
            if self.reserved.get(&class).is_some_and(|k| k.items == items && k.at == at) {
                continue;
            }
            match (&standing, &was, &gang) {
                (Some(_), _, _) => self.moved += 1,
                (None, Some(_), _) => self.again += 1,
                (None, None, Some(_)) => self.kept_whole += 1,
                (None, None, None) => self.made += 1,
            }
            // Its class goes before those after it: their room on its runners is worked out again.
            self.keep_reserved(|c, k| c <= class || !items.iter().any(|(_, r)| k.on(r)));
            let kept: BTreeSet<String> = items.iter().map(|(_, r)| r.clone()).collect();
            // Room it kept elsewhere before is room no more.
            if let Some(was) = self.reserved.insert(class, Kept { items, at }) {
                self.unkept.extend(was.items.into_iter().map(|(_, r)| r).filter(|r| !kept.contains(r)));
            }
            changed = true;
        }
        changed
    }

    /// A class's first queued leg by every dimension, among its first 64, that waits only for
    /// room; a gang's member stands for its gang.
    fn head(&self, class: Class) -> Option<String> {
        self.queue_order().into_iter().filter(|id| self.legs.get(id).is_some_and(|l| matches!(&l.at, At::Queued(q) if q.class == class))).take(64).find(|id| self.waits_for_room(id))
    }

    /// Whether a queued leg waits for room alone: some runner it may run on would take it and
    /// could hold it idle, and none has room for it now. A gang's member, when its gang is whole,
    /// hasn't room to start now (by room alone, the room kept aside, as a leg), and each of its
    /// members could be held somewhere.
    fn waits_for_room(&self, id: &str) -> bool {
        let Some(l) = self.legs.get(id).filter(|l| matches!(l.at, At::Queued(..))) else { return false };
        if let Some(g) = &l.input.gang {
            return self.whole(&g.id) && self.room_plan(&g.id).is_none() && self.crew_queued(&g.id).iter().all(|m| self.could_be_held(m));
        }
        let mut could = false;
        for r in self.runners.values().filter(|r| r.connected && carries(&r.input, &l.input)) {
            if !self.admits(r, l) {
                continue;
            }
            if self.may_run(l, r) {
                return false;
            }
            could |= room(&self.cfg, &r.input, &[], &l.input);
        }
        could
    }

    /// Whether some runner a leg may run on would take it and could hold it idle.
    fn could_be_held(&self, id: &str) -> bool {
        let Some(l) = self.legs.get(id) else { return false };
        self.runners.values().any(|r| r.connected && carries(&r.input, &l.input) && self.admits(r, l) && room(&self.cfg, &r.input, &[], &l.input))
    }

    /// The runner where a leg would fit soonest, and when: ties to where it would finish first,
    /// then by name.
    fn soonest(&mut self, id: &str, now: i64) -> Option<(String, i64)> {
        self.soonest_beside(id, now, &[])
    }

    /// [`Self::soonest`], with `items` (leg, runner) put on their runners from now.
    fn soonest_beside(&mut self, id: &str, now: i64, items: &[(String, String)]) -> Option<(String, i64)> {
        let l = self.legs.get(id)?;
        let ms = i64::try_from(l.input.cold.or(l.input.cost).unwrap_or(self.cfg.default_cost)).unwrap_or(i64::MAX / 4);
        let eligible: Vec<String> = self.runners.values().filter(|r| r.connected && carries(&r.input, &l.input)).map(|r| r.input.id.clone()).collect();
        let mut best: Option<(i64, i64, String)> = None;
        for r in eligible {
            let beside: Vec<String> = items.iter().filter(|(_, on)| *on == r).map(|(l, _)| l.clone()).collect();
            let Some(at) = self.earliest(id, &r, now, &beside) else { continue };
            if best.as_ref().is_none_or(|(a, f, n)| (at, at + ms, &r) < (*a, *f, n)) {
                best = Some((at, at + ms, r));
            }
        }
        best.map(|(at, _, r)| (r, at))
    }

    /// Where each member of a whole gang would fit soonest: in queue order, each on the runner
    /// where its room frees first beside the members put there before it; and the latest of their
    /// times. `None` when one fits nowhere.
    fn soonest_gang(&mut self, g: &str, now: i64) -> Option<(Vec<(String, String)>, i64)> {
        let (mut items, mut at) = (Vec::new(), now);
        for m in self.crew_queued(g) {
            let (on, t) = self.soonest_beside(&m, now, &items)?;
            at = at.max(t);
            items.push((m, on));
        }
        (!items.is_empty()).then_some((items, at))
    }

    /// When a runner is expected to have room for a queued leg: now, or as soon as enough of its
    /// grants are expected to have ended (their expected ends looked at now, and kept as moved:
    /// W05-R38), beside the legs of the classes before the leg's kept there, each until it's
    /// expected to end, and beside `mates`. `None` when it wouldn't take the leg, couldn't hold it
    /// idle, or wouldn't have room for it even so.
    fn earliest(&mut self, id: &str, runner: &str, now: i64, mates: &[String]) -> Option<i64> {
        self.runners.get(runner)?;
        let ends: Vec<i64> = self.on(runner).iter().filter_map(|g| self.look_end(g, now)).collect();
        let (r, l) = (self.runners.get(runner)?, self.legs.get(id)?);
        let At::Queued(q) = &l.at else { return None };
        let class = q.class;
        if !r.connected || !carries(&r.input, &l.input) || !self.admits(r, l) || !room(&self.cfg, &r.input, &[], &l.input) {
            return None;
        }
        let before = self.reserved_on(runner, |c, k| c < class && !k.holds(id));
        let mut times: Vec<i64> = ends.into_iter().chain(before.iter().map(|(_, end)| *end)).map(|t| t.max(now)).collect();
        times.sort_unstable();
        times.dedup();
        std::iter::once(now).chain(times).find(|t| {
            let mut on: Vec<&LegIn> = self.on(runner).iter().filter_map(|g| self.legs.get(g)).filter(|g| matches!(&g.at, At::On { ends, .. } if ends.expected > *t)).map(|g| &g.input).collect();
            on.extend(before.iter().filter(|(_, end)| end > t).filter_map(|(item, _)| self.legs.get(item)).map(|x| &x.input));
            on.extend(mates.iter().filter_map(|m| self.legs.get(m)).map(|x| &x.input));
            room(&self.cfg, &r.input, &on, &l.input)
        })
    }

    /// A running leg's expected end looked at at `now`, kept as it was moved (W05-R38).
    fn look_end(&mut self, id: &str, now: i64) -> Option<i64> {
        let At::On { ends, .. } = &mut self.legs.get_mut(id)?.at else { return None };
        *ends = ends.at(now);
        Some(ends.expected)
    }

    /// The legs kept on a runner by the room `keep` keeps, each with when it's expected to end:
    /// its room's time plus its expected duration.
    fn reserved_on(&self, runner: &str, keep: impl Fn(Class, &Kept) -> bool) -> Vec<(String, i64)> {
        let mut out = Vec::new();
        for (c, k) in &self.reserved {
            if keep(*c, k) {
                for id in k.on_runner(runner) {
                    if let Some(item) = self.legs.get(id) {
                        out.push((id.clone(), Ends::of(&item.input, k.at).expected));
                    }
                }
            }
        }
        out
    }

    /// Whether a runner may take a leg now, `mates` put there with it, as the room kept there for
    /// the leg's class and those before it, not for the leg, lets it ([`Self::respects`]).
    fn allowed(&self, r: &Runner, l: &Leg, mates: &[&LegIn]) -> bool {
        let class = match &l.at {
            At::Queued(q) => q.class,
            _ => Class::Background,
        };
        self.reserved.iter().filter(|(c, k)| **c <= class && k.on(&r.input.id) && !k.holds(&l.input.id)).all(|(c, k)| self.respects(r, l, *c, k, mates))
    }

    /// Whether a leg granted on a runner now, `mates` beside it, leaves the room kept there (of
    /// class `of`) its room: it's expected to end by the room's time, or fits there then beside
    /// the legs kept there, the grants still expected to run then, the other legs kept there that
    /// will be running then, and its mates.
    fn respects(&self, r: &Runner, l: &Leg, of: Class, k: &Kept, mates: &[&LegIn]) -> bool {
        if k.holds(&l.input.id) || Ends::of(&l.input, self.now).expected <= k.at {
            return true;
        }
        let id = &r.input.id;
        let mut on: Vec<&LegIn> = self.on(id).iter().filter_map(|g| self.legs.get(g)).filter(|g| matches!(&g.at, At::On { ends, .. } if ends.at(self.now).expected > k.at)).map(|g| &g.input).collect();
        on.extend(k.on_runner(id).filter_map(|i| self.legs.get(i)).map(|x| &x.input));
        on.extend(self.reserved_on(id, |c, o| c != of && o.at <= k.at).into_iter().filter(|(_, end)| *end > k.at).filter_map(|(i, _)| self.legs.get(&i)).map(|x| &x.input));
        on.extend(mates.iter().copied());
        room(&self.cfg, &r.input, &on, &l.input)
    }

    /// Room kept that a leg granted on a runner doesn't leave: room kept for it ends, and room of a
    /// class after its own on that runner, which it goes before, is worked out again.
    fn granted_over(&mut self, id: &str, runner: &str) {
        let (Some(l), Some(r)) = (self.legs.get(id), self.runners.get(runner)) else { return };
        let gone: Vec<Class> = self.reserved.iter().filter(|(c, k)| k.holds(id) || (k.on(runner) && !self.respects(r, l, **c, k, &[]))).map(|(c, _)| *c).collect();
        for c in gone {
            self.unkeep(c);
        }
    }

    /// Ends `class`'s room kept, if it has some, its runners taking what fits them before the input
    /// is done. Whether it had some.
    fn unkeep(&mut self, class: Class) -> bool {
        let Some(k) = self.reserved.remove(&class) else { return false };
        self.unkept.extend(k.items.into_iter().map(|(_, r)| r));
        true
    }

    /// Keeps the room `keep` keeps, and ends the rest.
    fn keep_reserved(&mut self, keep: impl Fn(Class, &Kept) -> bool) {
        let gone: Vec<Class> = self.reserved.iter().filter(|(c, k)| !keep(**c, k)).map(|(c, _)| *c).collect();
        for c in gone {
            self.unkeep(c);
        }
    }

    /// Room kept for a leg no longer waiting in the queue in its class, or on a runner that
    /// wouldn't take it now (gone, away, draining, or one it stays off), is dropped.
    fn drop_stale(&mut self) {
        let (legs, runners) = (&self.legs, &self.runners);
        let gone: Vec<Class> = self
            .reserved
            .iter()
            .filter(|(class, k)| !k.items.iter().all(|(leg, runner)| legs.get(leg).zip(runners.get(runner)).is_some_and(|(l, r)| matches!(&l.at, At::Queued(q) if q.class == **class) && self.admits(r, l))))
            .map(|(c, _)| *c)
            .collect();
        for c in gone {
            self.unkeep(c);
        }
    }

    /// A runner the leg stays off: one that refused it; one that lost it, while another that
    /// could take it is connected and not draining: one carrying its labels with room for it when
    /// idle.
    fn stays_off(&self, l: &Leg, runner: &str) -> bool {
        match &l.avoid {
            Some((r, Off::Refused)) => r == runner,
            Some((r, Off::Lost)) => r == runner && self.runners.values().any(|o| o.input.id != runner && o.connected && !o.draining && carries(&o.input, &l.input) && room(&self.cfg, &o.input, &[], &l.input)),
            None => false,
        }
    }

    fn placement(&self) -> Placement {
        let mut p = Placement::default();
        for (id, l) in &self.legs {
            match &l.at {
                At::On { runner, fence, expires, .. } => {
                    p.0.insert(id.clone(), (runner.clone(), l.attempt, *fence, *expires));
                }
                At::Queued(..) => {
                    p.1.insert(id.clone());
                }
                At::Retrying { at, .. } => {
                    p.2.insert(id.clone(), *at);
                }
                At::Off => {}
            }
        }
        p
    }
}

/// A runner-lost end of an attempt (W05-R16).
fn runner_lost() -> Outcome {
    Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() })
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
    let mut view = RunnerView { name: id.clone(), label: None, labels: labels(d), facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 0, pool: None };
    if view.labels.is_empty() {
        view.labels = vec!["x".into()];
    }
    let slots = 1 + u32::try_from(d.below(3)).unwrap_or(0);
    // Sized half the time, and now and then with a GPU or two (fair share's fourth dimension).
    let capacity = (d.below(2) == 0).then(|| Resources { cpu: Cpus::whole(2 + d.below(7)), mem: (4 + d.below(29)) * GIB, gpu: if d.below(3) == 0 { 1 + u32::try_from(d.below(2)).unwrap_or(0) } else { 0 }, ..Resources::default() });
    RunnerIn { id, view, slots, capacity, executor: None, draining: false }
}

fn leg(d: &mut Draw, n: u64) -> LegIn {
    let mut request = match d.below(4) {
        0 => Resources::default(),
        1 => Resources { cpu: Cpus::milli(500 * (1 + d.below(8))), mem: (1 + d.below(8)) * GIB, ..Resources::default() },
        2 => Resources { cpu: Cpus::whole(1 + d.below(4)), mem: (1 + d.below(16)) * GIB, ..Resources::default() },
        // A big one, about as big as a runner (W05-R39's items).
        _ => Resources { cpu: Cpus::whole(4 + d.below(5)), mem: (8 + d.below(25)) * GIB, ..Resources::default() },
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
        // A p90 half the time, short enough that grants run past it, and a timeout now and then
        // (W05-R38).
        p90: (d.below(2) == 0).then(|| 5_000 * (1 + d.below(36))),
        timeout: (d.below(3) == 0).then(|| 60_000 * (2 + d.below(20))),
        actor: format!("a{}", d.below(2)),
        lane: format!("l{}", d.below(2)),
        class: Class::ALL.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default(),
        // A quarter stop on losing their lease (W05-R34).
        on_lease_loss: if d.below(4) == 0 { LeaseLoss::Stop } else { LeaseLoss::Finish },
        ..LegIn::default()
    }
}

/// A seed's case: resource mode or slots, grants as leases or not, its epoch, and up to 40 steps,
/// each at its time. Made beside a reference, so each step applies when it's made: an attempt ends
/// or is refused while it runs, a runner is lost while connected and comes back once lost, renews
/// the grants it holds (now and then leaving one out, or naming one that isn't its own), drains and
/// is undrained.
fn case(seed: u64) -> (Config, u64, Vec<(Step, i64)>) {
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
    // Grants as leases half the time, of 1 to 8 minutes, the lock-delay as kiln has it or drawn
    // (W05-R34).
    let lease = (d.below(2) == 0).then(|| [1, 2, 4, 8].get(usize::try_from(d.below(4)).unwrap_or(0)).copied().unwrap_or(1) * 60_000);
    let lock_delay = if d.below(2) == 0 { Config::default().lock_delay } else { 15_000 * d.below(9) };
    // Room kept for big items in half the seeds (W05-R39).
    let backfill = d.below(2) == 0;
    let cfg = Config { resources, fair_by, classes, aging_per_minute, lease, lock_delay, backfill, ..Config::default() };
    let epoch = d.below(5);
    let mut r = Reference::new(cfg.clone(), epoch);
    // With backfilling, two runners at least and more legs: room kept is worked out, and moved, where
    // runners free at different times.
    let shapes: Vec<RunnerIn> = (0..if backfill { 2 + d.below(3) } else { 1 + d.below(4) }).map(|n| runner(&mut d, n)).collect();
    let (most, mut queued, mut now, mut steps) = (if backfill { 4 + d.below(12) } else { 1 + d.below(12) }, 0, 0, Vec::new());
    // A gang's members still to be queued.
    let mut members: std::collections::VecDeque<LegIn> = std::collections::VecDeque::new();
    for _ in 0..40 {
        let before = now;
        now += ms(&mut d, 3 * MINUTE);
        let placed = r.placement();
        let running: Vec<String> = placed.0.keys().cloned().collect();
        let up: Vec<String> = r.runners.values().filter(|x| x.connected).map(|x| x.input.id.clone()).collect();
        let known: Vec<String> = r.legs.keys().cloned().collect();
        let step = match d.below(16) {
            0 | 1 => shapes.iter().find(|s| !up.contains(&s.id)).cloned().map(Step::Up),
            2..=4 if queued < most || !members.is_empty() => {
                // A gang of 2 to 4 now and then (W05-R40): a job's legs, alike, queued one by one.
                if members.is_empty() {
                    let l = leg(&mut d, queued);
                    if d.below(4) == 0 {
                        let size = 2 + d.below(3);
                        let gang = Gang { id: format!("g{queued}"), size: u32::try_from(size).unwrap_or(2) };
                        members = (0..size).map(|i| LegIn { id: format!("l{}", queued + i), gang: Some(gang.clone()), ..l.clone() }).collect();
                    } else {
                        members.push_back(l);
                    }
                }
                queued += 1;
                members.pop_front().map(|l| Step::Queued(Box::new(l)))
            }
            5..=7 => d.pick(&running).and_then(|id| r.legs.get(id)).map(|l| {
                let outcome = match d.below(4) {
                    0 | 1 => Outcome::Success,
                    2 => Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: Some(1), log_hits: Vec::new() }),
                    _ => Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }),
                };
                Step::Ended { leg: l.input.id.clone(), attempt: l.attempt, outcome }
            }),
            8 => d.pick(&running).and_then(|id| placed.0.get(id).map(|g| Step::Refused { leg: id.clone(), fence: g.2 })),
            9 => d.pick(&up).cloned().map(Step::Lost),
            // A runner renews the grants it holds, now and then one left out, and now and then
            // naming another runner's grant or one gone (W05-R34).
            10..=12 => d.pick(&up).cloned().map(|runner| {
                let mut held: Vec<(String, Fence)> = placed.0.iter().filter(|(_, g)| g.0 == runner).filter(|_| d.below(5) > 0).map(|(id, g)| (id.clone(), g.2)).collect();
                if d.below(4) == 0
                    && let Some(id) = d.pick(&known)
                {
                    held.push((id.clone(), Fence { epoch, n: d.below(r.placed + 1) }));
                }
                Step::Renew { runner, held }
            }),
            13 => match r.runners.values().find(|x| x.connected && x.draining) {
                Some(x) => Some(Step::Undrain(x.input.clone())),
                None => d.pick(&up).cloned().map(Step::Drain),
            },
            // A tick, half the time at the moment the engine's timer asks for: the next expiry or
            // retry since the step before.
            14 => {
                if d.below(2) == 0
                    && let Some(at) = r.next_timer().filter(|at| (before..=now).contains(at))
                {
                    now = at;
                }
                Some(Step::Tick)
            }
            // A late refusal, or a late end of an attempt the leg has left.
            _ => d.pick(&known).and_then(|id| r.legs.get(id)).map(|l| {
                if d.below(2) == 0 && l.attempt > 1 {
                    Step::Stale { leg: l.input.id.clone(), attempt: l.attempt - 1 }
                } else {
                    Step::Refused { leg: l.input.id.clone(), fence: Fence { epoch, n: d.below(r.placed + 1) } }
                }
            }),
        };
        if let Some(step) = step.filter(|s| r.applies(s)) {
            r.handle(&step, now);
            steps.push((step, now));
        }
    }
    (cfg, epoch, steps)
}

/// The engine's own word for what runs where, and its invariants: an `Assign` at most once, and
/// only for a leg not running (or one the step, or a grant expiring before it, took off its runner:
/// refused, ended, on a runner lost, or lost); each runner's legs the legs running there, each
/// once; its queues holding the queued legs alone; no runner holding more than it has room for;
/// its grants as `check_grants` holds them.
fn engine(cfg: &Config, s: &Scheduler, step: &Step, before: &Placement, commands: &[Command], runners: &BTreeMap<String, RunnerIn>, legs: &BTreeMap<String, LegIn>) -> Result<Placement, String> {
    let (_, on, queued) = s.placement();
    let running: BTreeMap<String, (String, u32, Fence, Option<i64>)> = s.grants();
    let mut off: BTreeSet<&String> = match step {
        Step::Refused { leg, .. } | Step::Ended { leg, .. } => BTreeSet::from([leg]),
        Step::Lost(runner) => before.0.iter().filter(|(_, g)| g.0 == *runner).map(|(l, _)| l).collect(),
        _ => BTreeSet::new(),
    };
    for c in commands {
        if let Command::Lost { leg, .. } = c
            && let Some((l, _)) = before.0.get_key_value(leg)
        {
            off.insert(l);
        }
    }
    s.check_grants()?;
    // A leg assigned and lost in one input (its grant expiring before the input, its runner lost
    // by it) may be assigned again in it.
    let mut assigned = BTreeSet::new();
    for c in commands {
        match c {
            Command::Assign { leg, .. } if (before.0.contains_key(leg) && !off.contains(leg)) || !assigned.insert(leg.clone()) => return Err(format!("{leg} assigned while it ran")),
            Command::Lost { leg, .. } => {
                assigned.remove(leg);
            }
            _ => {}
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
    Ok((running, queued, s.retrying()))
}

fn show(p: &Placement) -> String {
    let on: Vec<String> = p.0.iter().map(|(l, (r, a, f, e))| format!("{l}@{r}#{a}/{}.{}{}", f.epoch, f.n, e.map_or_else(String::new, |e| format!(" until {}:{:02}", e / MINUTE, e % MINUTE / 1000)))).collect();
    let waiting: Vec<String> = p.2.iter().map(|(l, at)| format!("{l} at {}:{:02}", at / MINUTE, at % MINUTE / 1000)).collect();
    format!("running [{}], queued {:?}, retrying [{}]", on.join(" "), p.1, waiting.join(", "))
}

/// A case's steps through both, leaving out those that don't apply: `Err` where the engine breaks
/// an invariant or the two part, with the steps that applied up to there.
fn run(cfg: &Config, epoch: u64, steps: &[(Step, i64)], mut tell: impl FnMut(&str)) -> Result<(), (Vec<(Step, i64)>, String)> {
    let (mut s, mut r) = (Scheduler::with_epoch(cfg.clone(), epoch), Reference::new(cfg.clone(), epoch));
    let (mut runners, mut legs, mut applied) = (BTreeMap::new(), BTreeMap::new(), Vec::new());
    for (step, now) in steps {
        if !r.applies(step) {
            continue;
        }
        applied.push((step.clone(), *now));
        match step {
            Step::Up(x) | Step::Undrain(x) => {
                runners.insert(x.id.clone(), x.clone());
            }
            Step::Queued(l) => {
                legs.insert(l.id.clone(), (**l).clone());
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
fn shrink(cfg: &Config, epoch: u64, steps: Vec<(Step, i64)>, why: String) -> (Vec<(Step, i64)>, String) {
    let (mut steps, mut why) = (steps, why);
    'smaller: loop {
        for i in 0..steps.len() {
            let mut fewer = steps.clone();
            fewer.remove(i);
            if let Err((applied, w)) = run(cfg, epoch, &fewer, |_| {}) {
                (steps, why) = (applied, w);
                continue 'smaller;
            }
        }
        return (steps, why);
    }
}

/// A case's settings in words: its mode, fair share's levels, aging, each class's floor and bound,
/// and its leases.
fn settings(cfg: &Config) -> String {
    let classes: Vec<String> = cfg.classes.iter().map(|(c, cc)| format!("{} floor {}‰{}", c.as_str(), cc.floor, cc.bound.map_or_else(String::new, |b| format!(" bound {} min", b / 60_000)))).collect();
    let leases = cfg.lease.map_or_else(|| "no leases".to_owned(), |ms| format!("leases of {} min, lock-delay {} s", ms / 60_000, cfg.lock_delay / 1000));
    format!("{}, fair by {:?}, aging {}‰ a minute, {}, {leases}{}", if cfg.resources { "resource mode" } else { "slots" }, cfg.fair_by, cfg.aging_per_minute, classes.join(", "), if cfg.backfill { ", backfill" } else { "" })
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
fn w05_r03_slot_mode_still_reserves_declared_gpus() {
    let cfg = Config { resources: false, ..Config::default() };
    let mut engine = Scheduler::with_epoch(cfg.clone(), 0);
    let mut reference = Reference::new(cfg, 0);
    let mut runner = RunnerIn {
        id: "gpu-node".into(),
        view: RunnerView { name: "gpu-node".into(), labels: vec!["x".into()], ..RunnerView::default() },
        slots: 3,
        capacity: Some(Resources { cpu: Cpus::whole(4), mem: 11 * GIB, ..Resources::default() }),
        ..RunnerIn::default()
    };
    let leg = |id: &str| LegIn {
        id: id.into(), run: id.into(), repo: "acme/gpu".into(),
        route: LegRoute { labels: vec!["x".into()], ..LegRoute::default() },
        // CPU/memory are intentionally slot-accounted here; an exclusive GPU is still finite.
        request: Resources { cpu: Cpus::whole(4), mem: 15 * GIB, gpu: 1, ..Resources::default() },
        ..LegIn::default()
    };
    let mut apply = |step: Step, now, expected: &[&str]| {
        engine.handle(step.input(), now);
        reference.handle(&step, now);
        let expected: BTreeSet<String> = expected.iter().map(|id| (*id).to_owned()).collect();
        assert_eq!(engine.grants().keys().cloned().collect::<BTreeSet<_>>(), expected, "engine at {now}");
        assert_eq!(reference.placement().0.keys().cloned().collect::<BTreeSet<_>>(), expected, "reference at {now}");
    };
    apply(Step::Up(runner.clone()), 0, &[]);
    apply(Step::Queued(Box::new(leg("a"))), 1, &[]);
    apply(Step::Drain(runner.id.clone()), 2, &[]);
    if let Some(capacity) = &mut runner.capacity { capacity.gpu = 1; }
    apply(Step::Undrain(runner), 3, &["a"]);
    apply(Step::Queued(Box::new(leg("b"))), 4, &["a"]);
    apply(Step::Ended { leg: "a".into(), attempt: 1, outcome: Outcome::Success }, 5, &["b"]);
}

/// E1.5: compare all generated histories, retaining the original case count and invariants.
#[test]
fn e1_5_the_engine_agrees_with_the_reference() {
    if let Some(seed) = std::env::var("KILN_SCHED_REFERENCE_SEED").ok().and_then(|s| s.trim().parse::<u64>().ok()) {
        let (cfg, epoch, steps) = case(seed);
        println!("seed {seed} ({}):", settings(&cfg));
        let failed = run(&cfg, epoch, &steps, |line| println!("{line}")).err().map(|(applied, why)| {
            let (fewer, why) = shrink(&cfg, epoch, applied, why);
            report(seed, &cfg, &fewer, &why)
        });
        assert!(failed.is_none(), "{}", failed.unwrap_or_default());
        return;
    }
    let mut parted = Vec::new();
    for seed in 0..CASES {
        let (cfg, epoch, steps) = case(seed);
        if let Err((applied, why)) = run(&cfg, epoch, &steps, |_| {}) {
            parted.push((seed, cfg, epoch, applied, why));
        }
    }
    let first = parted.first().map(|(seed, cfg, epoch, applied, why)| {
        let (fewer, why) = shrink(cfg, *epoch, applied.clone(), why.clone());
        report(*seed, cfg, &fewer, &why)
    });
    assert!(parted.is_empty(), "{} of {CASES} cases part; the first:\n{}", parted.len(), first.unwrap_or_default());
}

/// The example of docs/needs.md E4.5: three runners of 4 CPUs, a stream of 1-CPU legs two to a
/// runner, and a gang of two 4-CPU members queued among them. Both engines keep room for the gang
/// on the same two runners from the same time (where their 1-CPU legs are expected to have ended),
/// keep a long 1-CPU leg that comes later out of that room, and start the gang there together once
/// those legs have ended; they agree after every input.
#[test]
fn e4_5_both_engines_keep_room_for_a_gang_and_start_it_together() {
    use super::state::WaitReason;
    // Runners that aren't overcommitted: two 1-CPU legs leave a 4-CPU member no room.
    let cfg = Config { resources: true, backfill: true, overcommit_cpu: 1000, overcommit_mem: 1000, ..Config::default() };
    let up = |id: &str| {
        let view = RunnerView { name: id.into(), label: None, labels: vec!["x".into()], facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 0, pool: None };
        Step::Up(RunnerIn { id: id.into(), view, slots: 4, capacity: Some(Resources { cpu: Cpus::whole(4), mem: 16 * GIB, ..Resources::default() }), executor: None, draining: false })
    };
    let small = |id: &str| LegIn {
        id: id.into(),
        run: "run".into(),
        repo: "acme/r".into(),
        route: LegRoute { labels: vec!["x".into()], ..LegRoute::default() },
        request: Resources { cpu: Cpus::whole(1), mem: 2 * GIB, ..Resources::default() },
        p90: Some(MINUTE.unsigned_abs()),
        class: Class::Batch,
        ..LegIn::default()
    };
    let member = |id: &str| LegIn { gang: Some(Gang { id: "sim".into(), size: 2 }), request: Resources { cpu: Cpus::whole(4), mem: 8 * GIB, ..Resources::default() }, ..small(id) };
    let mut steps = vec![(up("r0"), 0), (up("r1"), 0), (up("r2"), 0)];
    for (n, at) in (1..=6).map(|n| (n, n * 1000)) {
        steps.push((Step::Queued(Box::new(small(&format!("s{n}")))), at));
    }
    steps.push((Step::Queued(Box::new(member("m1"))), 10_000));
    steps.push((Step::Queued(Box::new(member("m2"))), 10_000));
    steps.push((Step::Queued(Box::new(LegIn { p90: Some(10 * MINUTE.unsigned_abs()), ..small("long") })), 20_000));
    for (n, at) in (1..=6).map(|n| (n, MINUTE + n * 1000)) {
        steps.push((Step::Ended { leg: format!("s{n}"), attempt: 1, outcome: Outcome::Success }, at));
    }
    let parted = run(&cfg, 0, &steps, |_| {}).err().map(|(applied, why)| report(0, &cfg, &applied, &why));
    assert!(parted.is_none(), "{}", parted.unwrap_or_default());
    let (mut s, mut r) = (Scheduler::with_epoch(cfg.clone(), 0), Reference::new(cfg.clone(), 0));
    let (mut kept, mut started) = (None, None);
    for (step, now) in &steps {
        s.handle(step.input(), *now);
        r.handle(step, *now);
        let mut engine: Vec<(String, i64)> = ["m1", "m2"].iter().filter_map(|m| if let Some(WaitReason::Reserved { runner, at }) = s.reason(m) { Some((runner.clone(), *at)) } else { None }).collect();
        engine.sort();
        if kept.is_none() && engine.len() == 2 {
            let reference = r.reserved.values().find(|k| k.holds("m1") && k.holds("m2")).map(|k| {
                let mut on: Vec<(String, i64)> = k.items.iter().map(|(_, runner)| (runner.clone(), k.at)).collect();
                on.sort();
                on
            });
            assert_eq!(reference.as_ref(), Some(&engine), "both keep room for the gang on the same two runners from the same time");
            kept = Some(engine);
        }
        let on: Vec<String> = ["m1", "m2"].iter().filter_map(|m| r.runner_of(m)).collect();
        if started.is_none() && !on.is_empty() {
            assert_eq!(on.len(), 2, "the gang starts whole, at {now}: {on:?}");
            started = Some((on, *now));
        }
    }
    let kept = kept.unwrap_or_default();
    let (on, at) = started.unwrap_or_default();
    assert!(kept.len() == 2 && kept.iter().all(|(_, t)| *t == at), "room kept {kept:?}, the gang started at {at}");
    let mut on = on;
    on.sort();
    assert_eq!(on, kept.iter().map(|(runner, _)| runner.clone()).collect::<Vec<_>>(), "where room was kept for it");
    assert!(r.runner_of("long").is_some_and(|x| !on.contains(&x)), "the long leg kept out of that room: {:?}", r.runner_of("long"));
}

/// The first of the cases that parts the engine from the reference with `m` planted in the
/// engine, cut down and named; none when none does.
fn first_parting(m: super::state::Mutant) -> Option<String> {
    super::state::MUTANT.with(|c| c.set(Some(m)));
    let first = (0..CASES).find_map(|seed| {
        let (cfg, epoch, steps) = case(seed);
        run(&cfg, epoch, &steps, |_| {}).err().map(|(applied, why)| (seed, cfg, epoch, applied, why))
    });
    let named = first.map(|(seed, cfg, epoch, applied, why)| {
        let (fewer, why) = shrink(&cfg, epoch, applied, why);
        report(seed, &cfg, &fewer, &why)
    });
    super::state::MUTANT.with(|c| c.set(None));
    named
}

/// E4.5: a backfill granted past its reservation's time (room kept holding nothing back) parts
/// the engine from the reference within the cases, which name the first.
#[test]
fn e4_5_a_backfill_past_its_reservation_parts() {
    let named = first_parting(super::state::Mutant::PastReservation);
    assert!(named.is_some(), "no case parts with backfills granted past their reservations' time");
    println!("{}", named.unwrap_or_default());
}

/// E4.5: a gang granted in part (started without its last member) parts the engine from the
/// reference within the cases, which name the first.
#[test]
fn e4_5_a_gang_granted_in_part_parts() {
    let named = first_parting(super::state::Mutant::GangInPart);
    assert!(named.is_some(), "no case parts with gangs granted in part");
    println!("{}", named.unwrap_or_default());
}

/// The cases cover what the reference is for: both modes, exclusive legs held whole and in
/// shares, legs refused and retried after a runner was lost, runners coming back, every class,
/// legs promoted, placed ahead by a floor and placed over one; grants renewed and expired, stop
/// legs waiting the lock-delay, runners draining and undrained, ticks, and refusals and ends that
/// come late; and with backfilling, big legs among small ones, room kept made, moved and worked
/// out again once its time passed, legs granted on a runner where room is kept for another and
/// legs kept out of it; gangs of 2 to 4 granted whole, kept room for whole and taken back whole.
#[test]
fn e1_5_the_cases_cover_the_rules() {
    let (mut kinds, mut modes, mut depths, mut classes) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let (mut promoted, mut ahead, mut over, mut renewed, mut expired, mut locked) = (0, 0, 0, 0, 0, 0);
    let (mut made, mut moved, mut again, mut backfilled, mut kept_out) = (0, 0, 0, 0, 0);
    let (mut whole, mut kept_whole, mut back, mut sizes, mut backfill) = (0, 0, 0, BTreeSet::new(), BTreeSet::new());
    // The cases the twin test runs (`CASES`): what it holds the engine to is what they cover. (It
    // was the first 4,000; room kept moved in one of them, and no longer does once `interactive`
    // goes up into `land`, merge-notes.md.)
    for seed in 0..CASES {
        let (cfg, epoch, steps) = case(seed);
        modes.insert(cfg.resources);
        depths.insert(cfg.fair_by.len());
        backfill.insert(cfg.backfill);
        let mut r = Reference::new(cfg.clone(), epoch);
        for (s, now) in &steps {
            if r.applies(s) {
                r.handle(s, *now);
            }
            if let Step::Queued(l) = s {
                classes.insert(l.class);
                sizes.extend(l.gang.as_ref().map(|g| g.size));
            }
        }
        (promoted, ahead, over) = (promoted + r.promoted, ahead + r.ahead, over + r.over);
        (renewed, expired, locked) = (renewed + r.renewed, expired + r.expired, locked + r.locked);
        (made, moved, again, backfilled, kept_out) = (made + r.made, moved + r.moved, again + r.again, backfilled + r.backfilled, kept_out + r.kept_out);
        (whole, kept_whole, back) = (whole + r.whole, kept_whole + r.kept_whole, back + r.back);
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
                Step::Queued(l) if l.gang.is_some() => "one of a gang",
                Step::Queued(l) if l.request.cpu >= Cpus::whole(4) && !l.exclusive => "big",
                Step::Queued(l) if l.exclusive && l.request.cpu.is_zero() => "exclusive whole",
                Step::Queued(l) if l.exclusive => "exclusive share",
                Step::Queued(l) if l.request.gpu > 0 => "asks a GPU",
                Step::Queued(_) => "queued",
                Step::Ended { outcome: Outcome::Failed(f), .. } if f.class == FailureClass::RunnerLost => "ended lost",
                Step::Ended { attempt: 2, .. } => "ended retried",
                Step::Ended { .. } => "ended",
                Step::Refused { .. } => "refused",
                Step::Renew { .. } => "renewed",
                Step::Drain(_) => "drained",
                Step::Undrain(_) => "undrained",
                Step::Tick => "tick",
                Step::Stale { .. } => "ended late",
            });
        }
    }
    assert_eq!(modes.len(), 2);
    assert_eq!(depths.len(), 3, "fair share by repository, actor and lane");
    assert_eq!(classes.len(), Class::ALL.len(), "every class");
    assert!(promoted > 0 && ahead > 0 && over > 0, "promoted {promoted}, ahead by a floor {ahead}, over one {over}");
    assert!(renewed > 0 && expired > 0 && locked > 0, "renewed {renewed}, expired {expired}, stop legs locked {locked}");
    eprintln!("the cases' room kept: made {made}, moved {moved}, worked out again {again}; legs granted beside it {backfilled}, kept out {kept_out}; gangs granted whole {whole}, kept room for whole {kept_whole}, taken back {back}");
    assert_eq!(backfill.len(), 2, "backfilling on and off");
    assert!(made > 0 && moved > 0 && again > 0, "room kept made {made}, moved {moved}, worked out again once passed {again}");
    assert!(backfilled > 0 && kept_out > 0, "legs granted beside room kept {backfilled}, kept out of it {kept_out}");
    assert!(whole > 0 && kept_whole > 0 && back > 0, "gangs granted whole {whole}, kept room for whole {kept_whole}, taken back whole {back}");
    assert_eq!(sizes, BTreeSet::from([2, 3, 4]), "gangs of 2 to 4");
    for k in ["up", "up with GPUs", "back", "lost", "queued", "big", "one of a gang", "asks a GPU", "exclusive whole", "exclusive share", "ended", "ended lost", "ended retried", "refused", "renewed", "drained", "undrained", "tick", "ended late"] {
        assert!(kinds.contains(k), "no case has `{k}`");
    }
}
