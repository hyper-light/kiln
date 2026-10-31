//! The scheduler (docs/ci.md W05-R01 to W05-R13, R19 to R24): an event-driven state machine.
//! [`Scheduler::handle`] takes one [`Input`] and the time and returns the [`Command`]s that follow;
//! it never polls and never reads storage. The dispatcher (`crates/kiln/src/ci/dispatch.rs`)
//! turns hub and store events into inputs and carries out the commands.
//!
//! Legs with the same requirement share a **signature**, with its eligible runners and its queue:
//! by class (W05-R30), then by tenant, each tenant's legs in its own order (W05-R09's priority,
//! aged as they wait), then arrival. A runner that frees takes from the first class with a leg
//! that fits it, unless a class after that one is short of its floor (W05-R31); in a class, from
//! the tenant whose fair-share standing is least (start-time fair queuing over the class's tree:
//! repositories, or inside one its actors and lanes, W28-R66; in the dimensions the runner offers,
//! docs/needs.md E2.1, E2.2). A class passed over at a signature for its bound has its legs there
//! promoted a class, never above `interactive` (W05-R31). Capacity that frees looks only at the
//! signatures that runner can serve.
//!
//! Each assignment is a grant (W05-R34, docs/needs.md §8): its [`Fence`] (the epoch, then its
//! number in it, no other grant's) and, with a lease, an expiry its runner's renewals move on. A
//! grant past its expiry is lost before anything else is decided at that time, as its runner's
//! loss would end it ([`Command::Lost`]); a refusal or renewal naming a grant that isn't the leg's
//! now changes nothing.
//!
//! A leg whose preferences name `warm` or `affinity` (W24-R18) goes where it's expected to finish
//! first, from what each runner holds warm ([`Input::Warm`], [`super::warm`]), and may wait a
//! bounded while for a busy runner that would finish it sooner ([`WaitReason::Warm`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::demand::{self, Ask, Demand, Demands, End, Fate, Next, Running, Work};
use super::admit::{self, Admit, Held, Skipped};
use super::groups::{GroupAction, GroupConfig, Groups};
use super::needs;
use super::pools::{self, ExecState, Executor, Pool, PoolView};
use super::reserve::Reservation;
use super::retry::{self, Failed, FailureClass, RetryPolicy, RunnerChoice};
use super::routing::{self, Check, LegRoute, Preference, Requirement, Rule, RunnerView};
use super::table::Table;
use super::units::{self, Ms, Ppm};
use super::warm::{self, Held as WarmHeld, Wants};

/// What a leg asks for and a runner offers, in resource mode (W05-R03); and what an exclusive leg
/// holds of its runner. Every amount is an integer, so they add, subtract and compare exactly, the
/// same on every machine (docs/needs.md, E1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Resources {
    pub cpu: Cpus,
    pub mem: u64,
    pub disk: u64,
    pub gpu: u32,
}

/// CPUs in thousandths, as Kubernetes counts them (`500m`), nothing finer asked for or offered.
/// It travels as a number of CPUs (`16.0`, `0.5`), as the float it replaces did, so a runner or a
/// server a version apart reads and writes the same JSON.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cpus(u64);

impl Cpus {
    pub const ZERO: Cpus = Cpus(0);

    /// `n` thousandths of a CPU.
    pub const fn milli(n: u64) -> Cpus {
        Cpus(n)
    }

    /// `n` whole CPUs.
    pub const fn whole(n: u64) -> Cpus {
        Cpus(n.saturating_mul(1000))
    }

    /// Its thousandths of a CPU.
    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn saturating_add(self, b: Cpus) -> Cpus {
        Cpus(self.0.saturating_add(b.0))
    }

    pub const fn saturating_sub(self, b: Cpus) -> Cpus {
        Cpus(self.0.saturating_sub(b.0))
    }

    /// The whole CPUs it takes, a part counting as one (`0.5` takes 1, `2.25` takes 3).
    pub const fn ceil(self) -> u64 {
        self.0.div_ceil(1000)
    }

    /// A number of CPUs as it comes from outside (JSON, a measurement), to the nearest
    /// thousandth: none for one that isn't a finite number, nothing below zero, the most there is
    /// above it. A boundary: what the engine decides with is the integer it gives.
    #[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn of_f64(cpus: f64) -> Option<Cpus> {
        cpus.is_finite().then(|| Cpus((cpus * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64))
    }

    /// As a number of CPUs, for JSON and people. A boundary: nothing decides with it.
    #[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / 1000.0
    }
}

/// `8`, `0.5`, `1.25`, `0.001`: as JavaScript writes the number, from the integer.
impl std::fmt::Display for Cpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (whole, part) = (self.0 / 1000, self.0 % 1000);
        if part == 0 {
            return write!(f, "{whole}");
        }
        let part = format!("{part:03}");
        write!(f, "{whole}.{}", part.trim_end_matches('0'))
    }
}

impl Serialize for Cpus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.as_f64())
    }
}

impl<'de> serde::Deserialize<'de> for Cpus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Number;
        impl serde::de::Visitor<'_> for Number {
            type Value = Cpus;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a number of CPUs")
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Cpus, E> {
                Cpus::of_f64(v).ok_or_else(|| E::custom("not a number of CPUs"))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Cpus, E> {
                Ok(Cpus::whole(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Cpus, E> {
                Ok(Cpus::whole(u64::try_from(v).unwrap_or(0)))
            }
        }
        // As the f64 it was: JSON gives whole numbers to `visit_u64`, and a binary format reads
        // the eight bytes it wrote.
        d.deserialize_f64(Number)
    }
}

/// A factor in thousandths (`1.5` is `1500`): what overcommit multiplies capacity by, exactly.
pub type PerMille = u32;

/// A factor as config gives it (`1.5`), in thousandths, at least 1. A boundary: the engine
/// multiplies by the integer.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn per_mille(factor: f64) -> PerMille {
    if !factor.is_finite() {
        return 1000;
    }
    (factor * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32
}

/// Whether `used` fits within `cap` multiplied by `factor` (thousandths), exactly.
fn within(used: u64, cap: u64, factor: PerMille) -> bool {
    u128::from(used).saturating_mul(1000) <= u128::from(cap).saturating_mul(u128::from(factor))
}

/// An item's class (docs/needs.md §7, W05-R30): classes go strictly in order, `critical` first,
/// but for floors and promotion (W05-R31). kiln sets it from what the work is for, never the job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Critical,
    Land,
    Interactive,
    #[default]
    Batch,
    Background,
}

impl Class {
    /// Every class, in order.
    pub const ALL: [Class; 5] = [Class::Critical, Class::Land, Class::Interactive, Class::Batch, Class::Background];

    /// The class an item that waited past its class's bound goes up to (W05-R31): never above
    /// `interactive`, so `interactive`, `land` and `critical` aren't promoted.
    pub fn up(self) -> Option<Class> {
        match self {
            Class::Background => Some(Class::Batch),
            Class::Batch => Some(Class::Interactive),
            Class::Critical | Class::Land | Class::Interactive => None,
        }
    }

    /// Its place in [`Class::ALL`].
    pub fn index(self) -> usize {
        match self {
            Class::Critical => 0,
            Class::Land => 1,
            Class::Interactive => 2,
            Class::Batch => 3,
            Class::Background => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Critical => "critical",
            Class::Land => "land",
            Class::Interactive => "interactive",
            Class::Batch => "batch",
            Class::Background => "background",
        }
    }
}

/// A class's floor and bound (docs/needs.md §7, W05-R31): `[sched.classes.<class>]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassConfig {
    /// Its floor share of the fleet, in thousandths of its dominant share: while its running items
    /// hold less, it goes ahead of the classes before it when room frees: within its floor, and
    /// over it as often as its floor of the fleet's time allows.
    pub floor: PerMille,
    /// How long one of its items waits, the class starting nothing at its signature meanwhile,
    /// before it's promoted a class.
    pub bound: Option<Ms>,
}

/// A tenant's own order (W05-R09, docs/needs.md E2.3): by priority, aged as it waits (`priority +
/// aging × minutes waited`, the highest first), as a key that doesn't move with the clock, the
/// least first: `aging × queued-at − priority × 60,000,000`, in thousandths of a priority per
/// millisecond.
pub fn own_order(priority: u8, aging: PerMille, queued_at: i64) -> i128 {
    i128::from(aging).saturating_mul(i128::from(queued_at)).saturating_sub(i128::from(priority).saturating_mul(60_000_000))
}

/// A queued leg's place in its signature's queue: its class now (promoted or not), its place in its
/// tenant's own order ([`own_order`]), and when it arrived. Its tenant's fair-share standing is
/// taken as legs are picked, for the runner picking (docs/needs.md E2.1, E2.2).
type Key = (Class, i128, u64);

/// The dimensions fair share counts (W05-R33, docs/needs.md E2.1): slots, milli-CPUs, memory bytes
/// and GPUs, in that order.
pub const DIMS: usize = 4;

/// A share of the fleet, in millionths.
const MILLION: u128 = 1_000_000;

/// What a runner offers in each dimension fair share counts (docs/needs.md E2.1): its slots, and in
/// resource mode (`resources`, W05-R03) its CPUs, memory and GPUs; nothing in the rest.
pub fn offers(r: &RunnerIn, resources: bool) -> [u64; DIMS] {
    match r.capacity.filter(|c| resources && !c.cpu.is_zero()) {
        Some(c) => [u64::from(r.slots), c.cpu.get(), c.mem, u64::from(c.gpu)],
        None => [u64::from(r.slots), 0, 0, 0],
    }
}

/// An item's share of each dimension of the fleet, in millionths, rounded up (docs/needs.md E2.1,
/// DRF's shares): what it asks of each over what the fleet has; nothing where it asks nothing or
/// the fleet has none. Its dominant share is the largest.
pub fn shares(asks: &[u64; DIMS], fleet: &[u64; DIMS]) -> [u128; DIMS] {
    let mut s = [0u128; DIMS];
    for ((s, a), f) in s.iter_mut().zip(asks).zip(fleet) {
        if *a > 0 && *f > 0 {
            *s = (u128::from(*a) * MILLION).div_ceil(u128::from(*f));
        }
    }
    s
}

/// What `held` (in each dimension) is of the fleet, in millionths of its dominant share: the
/// largest over the dimensions the fleet has of `held` there over the fleet's, rounded down.
fn held_share(held: &[u128; DIMS], fleet: &[u64; DIMS]) -> u128 {
    held.iter().zip(fleet).filter(|(_, f)| **f > 0).map(|(h, f)| h.saturating_mul(MILLION) / u128::from(*f)).max().unwrap_or(0)
}

/// What running costs a node of the tenant tree in each dimension (W05-R33, as DRFQ
/// charges): its share of it times its milliseconds, over the node's weight (thousandths), in
/// fair share's virtual time.
pub fn charges(shares: &[u128; DIMS], ms: Ms, weight: PerMille) -> [u128; DIMS] {
    shares.map(|s| s.saturating_mul(u128::from(ms)).saturating_mul(1000) / u128::from(weight.max(1)))
}

/// Fair share's tags are kept once for every set of dimensions a runner may offer (W05-R33,
/// docs/needs.md E2.4): a set is a bitmask over [`DIMS`], bit `d` for dimension `d`.
pub const PROFILES: usize = 1 << DIMS;

/// The set of dimensions `dims` marks, as a bitmask ([`PROFILES`]).
pub fn profile(dims: &[bool; DIMS]) -> usize {
    dims.iter().enumerate().filter(|(_, on)| **on).fold(0, |m, (d, _)| m | (1 << d))
}

/// What charges in each dimension (`costs`) cost in each set of dimensions: the largest there, as
/// DRFQ charges an item the time of its dominant share (docs/needs.md E2.4). A runner ranks tenants
/// by the set it offers, so they compare by their dominant resource among what it has: one tag
/// for each set, not one for each dimension, whose virtual times drift apart when tenants'
/// dominant resources differ, until a tenant's dominant resource no longer counts.
pub fn profile_costs(costs: &[u128; DIMS]) -> [u128; PROFILES] {
    let mut out = [0u128; PROFILES];
    for (m, o) in out.iter_mut().enumerate() {
        *o = costs.iter().enumerate().filter(|(d, _)| (m >> d) & 1 == 1).map(|(_, c)| *c).max().unwrap_or(0);
    }
    out
}

/// The tenant tree's levels below its root (W28-R66): the repository, its actors, their lanes.
pub const LEVELS: usize = 3;

/// A leaf of the tenant tree and the nodes above it, from its repository down: `repo`,
/// `repo\0actor`, `repo\0actor\0lane`.
fn nodes(leaf: &str) -> impl Iterator<Item = &str> {
    leaf.match_indices('\0').map(|(i, _)| i).chain(std::iter::once(leaf.len())).filter_map(|i| leaf.get(..i))
}

/// What a leg's dispatch charged fair share (W05-R32), settled when its attempt ends:
/// the leaf whose nodes it charged and their weights, top down; its shares of the fleet; the
/// milliseconds it was charged for; when it started.
#[derive(Debug, Clone, PartialEq)]
struct Charge {
    leaf: String,
    weights: [PerMille; LEVELS],
    shares: [u128; DIMS],
    ms: Ms,
    at: i64,
    /// The class it ran as, and what it holds in each dimension: its class's share of the fleet,
    /// held against the class's floor (W05-R31).
    class: Class,
    holds: [u64; DIMS],
    /// Started over its class's floor, ahead of a class before its own: the milliseconds of the
    /// floor's pace it took and what was left of the floor ([`Scheduler::pace`]), settled to the
    /// time it ran as the charge is.
    paced: Option<(i64, u128)>,
}

/// `ms` after `now` (unix ms), as far as an `i64` goes.
fn later(now: i64, ms: Ms) -> i64 {
    now.saturating_add(ms_i64(ms))
}

/// Milliseconds as an `i64`, as far as one goes.
fn ms_i64(ms: Ms) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}

/// Whether an ordinary leg (or a pool's executor) asking `q` fits a runner of capacity `cap` in
/// resource mode (W05-R03): beside what its exclusive legs hold (`held`), with what its other work
/// uses (`used`, `held` among it), overcommitted as `c` says, GPUs never.
fn fits_beside(used: &Resources, held: &Resources, cap: &Resources, q: &Resources, c: &Config) -> bool {
    within(used.cpu.saturating_sub(held.cpu).saturating_add(q.cpu).get(), cap.cpu.saturating_sub(held.cpu).get(), c.overcommit_cpu)
        && within(used.mem.saturating_sub(held.mem).saturating_add(q.mem), cap.mem.saturating_sub(held.mem), c.overcommit_mem)
        && within(used.disk.saturating_add(q.disk), cap.disk, c.overcommit_disk)
        && used.gpu.saturating_add(q.gpu) <= cap.gpu
}

/// The scheduler's settings (`[ci.scheduler]`, `[ci.fairness]`, routing rules).
#[derive(Debug, Clone)]
pub struct Config {
    /// Priority a waiting leg gains per minute in its tenant's own order (W05-R09, docs/needs.md
    /// E2.3), in thousandths.
    pub aging_per_minute: PerMille,
    /// Fair-share weights by repository, in thousandths (default 1, `1000`; W05-R10).
    pub weights: HashMap<String, PerMille>,
    /// How long a leg is expected to take when its history doesn't say.
    pub default_cost: Ms,
    /// Resource mode instead of slots (W05-R03).
    pub resources: bool,
    /// What a runner's capacity is multiplied by for ordinary legs, in thousandths ([`per_mille`]).
    pub overcommit_cpu: PerMille,
    pub overcommit_mem: PerMille,
    pub overcommit_disk: PerMille,
    /// Routing rules, in order (W05-R21).
    pub rules: Vec<Rule>,
    /// Legs in progress at most, per repository (W05-R11).
    pub quotas: HashMap<String, usize>,
    /// Pools, in order: a leg goes to the first that serves its labels (W13-R09).
    pub pools: Vec<Pool>,
    /// Runners and pools kept for matching legs in windows (W05-R12).
    pub reservations: Vec<Reservation>,
    /// Seconds of work left within which work no live demand wants finishes when live work
    /// contends for its capacity, if it seeds trusted entries (W28-R43).
    pub finish_within: i64,
    /// Each lane's speculation window (W28-R68).
    pub window: admit::Window,
    /// What fair share is between (W05-R10, W28-R66): repositories, then inside each its actors,
    /// then their lanes, as far as this lists; each level splits its parent's share equally.
    pub fair_by: Vec<FairBy>,
    /// Admission by load (`[ci.scheduler.admission]`, W24-R19): off when `None`.
    pub admission: Option<super::load::Admission>,
    /// Warm-aware placement's settings (W24-R18).
    pub warm: warm::Model,
    /// Each class's floor and bound (docs/needs.md §7, W05-R31); a class not here has neither.
    pub classes: BTreeMap<Class, ClassConfig>,
    /// How long a grant lives without being renewed (docs/needs.md §8, W05-R34): one not renewed
    /// by then is lost. `None`: grants don't expire, as where nothing renews them; kiln's `[sched]
    /// lease` sets it, 60 s by default.
    pub lease: Option<Ms>,
    /// How long past a lost grant's expiry its item's next attempt waits when the item stops on
    /// losing its lease ([`LeaseLoss::Stop`], W05-R34): its runner has stopped it by then.
    /// kiln's `[sched] lock_delay`, 15 s by default.
    pub lock_delay: Ms,
    /// Reservations for big items (W05-R39, docs/needs.md §5): the first of each class that fits
    /// no runner keeps room where it frees first, and only what ends before then, or fits beside
    /// it, is granted there. kiln's `[sched] backfill`: off until declared (docs/ci.md §0.3).
    pub backfill: bool,
}

/// A level of fair share (W28-R66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FairBy {
    Repo,
    /// An agent or a person (W28-R67).
    Actor,
    /// A repository's workflow on a branch.
    Lane,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            aging_per_minute: 1000,
            weights: HashMap::new(),
            default_cost: 60_000,
            resources: false,
            overcommit_cpu: 1500,
            overcommit_mem: 1000,
            overcommit_disk: 1000,
            rules: vec![routing::native_rule()],
            quotas: HashMap::new(),
            pools: Vec::new(),
            reservations: Vec::new(),
            finish_within: demand::FINISH_WITHIN,
            window: admit::Window::default(),
            fair_by: vec![FairBy::Repo],
            admission: None,
            warm: warm::Model::default(),
            classes: default_classes(),
            lease: None,
            lock_delay: 15_000,
            backfill: false,
        }
    }
}

/// What an item's runner does with it once its lease is lost (docs/needs.md §8, W05-R34).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaseLoss {
    /// It runs to its end, and what it writes is refused once its grant is gone (a CI leg).
    #[default]
    Finish,
    /// Its runner stops it at the lease's end, by its own clock; its next attempt waits the
    /// lock-delay past the grant's expiry.
    Stop,
}

/// A grant's fencing token (docs/needs.md §8, W05-R34): the epoch it was made in, then its number
/// among that epoch's grants. A later grant's is greater, across failovers too (a later leader's
/// epoch is), and no two grants share one, not even an attempt granted again after a refusal: so
/// what names a grant by it (a renewal, a refusal, a write) is told from what a stale one sends,
/// as Chubby's sequencers and Kleppmann's fencing tokens are checked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Fence {
    pub epoch: u64,
    pub n: u64,
}

/// Why the engine took a grant back ([`Command::Lost`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// Not renewed by its expiry (W05-R34).
    Expired,
    /// Its runner was lost.
    RunnerLost,
    /// Another member of its gang was refused, couldn't be granted, or was lost: its gang starts
    /// together or not at all (W05-R40).
    Gang,
}

impl Loss {
    /// In words, as the attempt's record says why it ended.
    pub fn why(self) -> &'static str {
        match self {
            Loss::Expired => "its lease ran out: its runner didn't renew it",
            Loss::RunnerLost => "its runner was lost",
            Loss::Gang => "a member of its gang was refused or lost: its gang starts together or not at all",
        }
    }
}

/// A gang (W05-R40, `together:<name>/<n>`): items that start together or not at all, `size` of
/// them, named by `id`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gang {
    pub id: String,
    pub size: u32,
}

/// The classes' floors and bounds by default (docs/needs.md §7, E-D2): `background` 5 % of the
/// fleet and promoted after an hour, `batch` 15 % and promoted after half an hour.
pub fn default_classes() -> BTreeMap<Class, ClassConfig> {
    BTreeMap::from([(Class::Batch, ClassConfig { floor: 150, bound: Some(30 * 60_000) }), (Class::Background, ClassConfig { floor: 50, bound: Some(60 * 60_000) })])
}

/// A leg to schedule.
#[derive(Debug, Clone, Default)]
pub struct LegIn {
    pub id: String,
    pub run: String,
    pub repo: String,
    pub route: LegRoute,
    /// 0–100 (W05-R09): it orders its tenant's own items.
    pub priority: u8,
    /// Its class (docs/needs.md §7, W05-R30), from what it's for; `batch` unless said.
    pub class: Class,
    pub exclusive: bool,
    pub request: Resources,
    /// The job-level concurrency group: key (scope and name), name, behaviour.
    pub group: Option<(String, String, GroupConfig)>,
    pub retry: Option<RetryPolicy>,
    /// How long it's expected to take (history, W19).
    pub cost: Option<Ms>,
    /// How long it takes at its 90th percentile (history, W19): when a grant of it is expected
    /// to end, as reservations go by it (W05-R38).
    pub p90: Option<Ms>,
    /// Its job's `timeout-minutes`: its runner stops it past that (W05-R38).
    /// [`DEFAULT_TIMEOUT_MS`] when unsaid.
    pub timeout: Option<Ms>,
    pub prefer: Vec<Preference>,
    /// It has a `container:` or `services:`, or is a kiln-dialect leg that sets `toolchain:`,
    /// `image:` or `resources:`: on a hosted pool (`k8s`), it runs in an executor made for it (a
    /// pod of that container, those services beside it, W13-R07; a kiln-dialect leg's, v1's job
    /// pod, W02-R06).
    pub own_executor: bool,
    /// Its work (W28-R42): the key it adopts or executes by, its domain and its warm lane. A leg
    /// without stays out of demands.
    pub work: Option<Work>,
    /// After a restart: when the attempt its runner still runs started (unix ms), for how long it
    /// has left (W28-R43).
    pub started: Option<i64>,
    /// After a restart: the attempt it's queued for, as the store has it (W05-R25), so an attempt
    /// that ended is never granted again. A new leg's is its first (0 is taken as 1).
    pub attempt: u32,
    /// Its workflow's `supersede.finish-within` (W28-R43), over the scheduler's.
    pub finish_within: Option<i64>,
    /// Its lane, unit and place, and its workflow's `coalesce:` and `cadence:` (W28-R60): legs
    /// without go straight to their queues.
    pub admission: Option<admit::Demand>,
    /// Whose it is: the agent or person its commit is attributed to (W28-R67).
    pub actor: String,
    /// Its lane (repository, workflow, branch), for fair share by lane (W28-R66).
    pub lane: String,
    /// Its intent class and why (W28-R64), when priority is by intent.
    pub intent: Option<(super::intent::Intent, String)>,
    /// What admission by load knows of it (W24-R19): heavy on I/O, and its I/O weight.
    pub load: super::load::Load,
    /// Its lane (W24-R11) as placement matches lanes (`work::slots::placement`): the workspaces
    /// its `warm` term looks for (W24-R18), on each runner those made with the toolchain it would
    /// run with there.
    pub warm_lane: Option<String>,
    /// How long it runs cold (runners' history of its lane, W24-R18); its `cost` when that's
    /// unknown.
    pub cold: Option<Ms>,
    /// Its job's container image (`container:`): with the executor and image a runner gives it,
    /// the toolchain its lane's workspaces there must have been made with (W24-R11).
    pub container: Option<String>,
    /// Its workflow's file (`.kiln/ci.yaml`), as runners' lanes name its jobs: what `affinity: {
    /// job }` and a `warm` term without a lane go by (W24-R18).
    pub workflow_file: String,
    /// Where a person's retry asked it to run (W05-R28).
    pub placement: Placement,
    /// What its runner does with it once its lease is lost (W05-R34): a CI leg runs to its end.
    pub on_lease_loss: LeaseLoss,
    /// Its gang (W05-R40): it starts with the rest of its gang or not at all.
    pub gang: Option<Gang>,
}

/// Where a person's retry asked a leg to run (W05-R28): only on `runner`, or on any runner but
/// `avoid` (the one it ran on). It narrows what the leg's requirements allow, never widens it, and
/// holds through the attempt's own retries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct Placement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid: Option<String>,
}

impl Placement {
    pub fn is_empty(&self) -> bool {
        self.runner.is_none() && self.avoid.is_none()
    }

    /// Whether it lets the leg run on `runner`.
    pub fn allows(&self, runner: &str) -> bool {
        self.runner.as_deref().is_none_or(|r| r == runner) && self.avoid.as_deref() != Some(runner)
    }
}

/// A runner that can take legs.
#[derive(Debug, Clone, Default)]
pub struct RunnerIn {
    pub id: String,
    pub view: RunnerView,
    pub slots: u32,
    /// What it offers in resource mode; slots are used when `None`.
    pub capacity: Option<Resources>,
    /// The hosted executor it is (a `k8s` pool's pod, W13-R07): it takes its pool's legs and no
    /// other's; `Some("")` when the server doesn't know whose it is (one left from before a
    /// restart), which takes none.
    pub executor: Option<String>,
    /// Draining (W05-R35): it keeps running what it holds and takes nothing new, as its operator
    /// had it. Said with the rest of it, so that a `RunnerChanged` or `RunnerUp` doesn't take back
    /// a drain, and one with `false` is its undrain.
    pub draining: bool,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Failed(Failed),
    Cancelled,
}

/// What `supersede:` does to older runs (W05-R08).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supersede {
    Cancel,
    Deprioritize,
    /// Withdraws their demands instead: their work goes on while a live demand wants it (W28-R40).
    Adopt,
}

/// Something that happened.
#[derive(Debug, Clone)]
pub enum Input {
    /// A run is about to start: its workflow-level group may hold it, and `supersede` may stop
    /// older runs with the same key (workflow and branch, stack or pull request).
    RunQueued { run: String, group: Option<(String, String, GroupConfig)>, supersede: Option<(String, Supersede)> },
    RunFinished { run: String },
    /// A run's demand (W28-R40), before its `RunQueued`: that withdraws older demands on its lane
    /// when its `supersede` adopts. On its own (a rerun's), it is live again and withdraws nothing.
    Demand(Box<Demand>),
    /// W24-R04's check of what an ended execution read came back, at an adopter's own commit.
    Checked { leg: String, matched: bool },
    LegQueued(Box<LegIn>),
    /// After a restart: a leg the runner still runs (W05-R25), under the grant the store holds,
    /// its fence kept. It gets a lease afresh.
    LegRunning { leg: Box<LegIn>, runner: String, attempt: u32, fence: Fence },
    /// The plan cancelled a leg (fail-fast, the run cancelled).
    LegCancelled { leg: String, reason: String },
    /// A leg gone with its repository: dropped without settling (its records are gone with it),
    /// its runner told to stop it.
    LegGone {
        leg: String,
    },
    AttemptFinished {
        leg: String,
        attempt: u32,
        outcome: Outcome,
    },
    /// The runner refused the grant `fence` (no free slot, draining): the leg goes back to its
    /// queue, the same attempt, and not to that runner. A refusal of a grant that isn't the leg's
    /// now changes nothing (W05-R34).
    Rejected { leg: String, fence: Fence },
    /// A grant the server couldn't make durable before its Assign (W05-R05, G10): its Assign never
    /// went, so the leg goes back to its queue, the same attempt, [`REFUSED_AGAIN_MS`] later, and
    /// is kept off no runner: it says nothing of the leg or the runner. One that isn't the leg's
    /// grant now changes nothing.
    Ungranted { leg: String, fence: Fence },
    /// A runner renews the grants it holds (its heartbeat, W05-R34): each it names by its fence
    /// that's its own lives a lease from now. A grant it doesn't name isn't renewed.
    Renewed { runner: String, held: Vec<(String, Fence)> },
    RunnerUp(Box<RunnerIn>),
    RunnerChanged(Box<RunnerIn>),
    RunnerLost { runner: String },
    RunnerDraining { runner: String },
    RulesChanged(Vec<Rule>),
    /// A pool's executor its host started is ready for a leg (W13-R09).
    ExecutorReady { executor: String },
    /// Its host couldn't start it, or it's gone.
    ExecutorFailed { executor: String, error: String },
    /// Pools and reservations, read again (the Pools page, W13-R09, W05-R12).
    PoolsChanged { pools: Vec<Pool>, reservations: Vec<Reservation> },
    /// The time a [`Command::Timer`] asked for.
    Tick,
    /// What a runner holds warm (W24-R18), in place of what it held before: its workspaces, how
    /// far each is behind and what warm runs took there, and its bandwidth. From the runners'
    /// reports of their workspaces and the ledger's history.
    Warm { runner: String, held: WarmHeld },
}

/// Why a leg waits (W05-R13): exactly one, the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum WaitReason {
    Capacity { eligible: usize, busy: usize, draining: usize },
    NoRunner { offline: Vec<String> },
    Unroutable { failing: Vec<String> },
    Capability { runners: Vec<String> },
    Concurrency { group: String, holders: Vec<String> },
    Quota { limit: usize },
    Exclusive,
    Retry { at: i64 },
    /// Its pool is starting executors for it (W13-R09): how many, since when (unix ms).
    Scaling { pool: String, starting: usize, since: i64 },
    /// The runners or pool that could take it are kept for other legs until `until` (W05-R12).
    Reservation { reservation: String, until: i64 },
    /// Only `avoided` could take it, and it waits for another: as its retry policy asks
    /// (`runner: different`), as that one refused it (W05-R16), or as a person's retry asked
    /// (`asked`, W05-R28).
    OtherRunner {
        avoided: String,
        refused: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        asked: bool,
    },
    /// Only `runner` may run it, as a person's retry asked (W05-R28), and that runner can't take
    /// it now.
    Pinned { runner: String, state: PinnedState },
    /// It adopted `leg` of `run`, the same work in flight, and waits for it to end (W28-R42).
    Adopted { leg: String, run: String },
    /// Held to coalesce a burst of its lane while the runners it needs are busy (W28-R61): until
    /// its quiet period ends, never past `deadline` (unix ms).
    Coalescing { lane: String, until: i64, deadline: i64 },
    /// On its workflow's cadence (`every`, W28-R62): not before `next` (unix ms).
    Cadence { lane: String, next: i64 },
    /// Its lane's speculation window is full (W28-R68).
    Speculation { lane: String, size: u32, in_flight: u32 },
    /// It's heavy on I/O, and the runners that could take it are past admission's limits
    /// (W24-R19): each, with its I/O pressure, the leg's limit there, its free disk and the floor.
    Load { held: Vec<super::load::Held> },
    /// It waits for `runner`, busy, where it's expected to finish `saves_s` seconds sooner than
    /// where it could start now, until `until` (unix ms) at most: its `warm` or `affinity` term's
    /// patience (W24-R18).
    Warm { runner: String, saves_s: u64, until: i64 },
    /// It asks for more than any runner that could take it has, even idle (resource mode,
    /// W05-R03): what it asks, and what each of those runners has of what it's short of.
    Oversized { asked: String, short: Vec<String> },
    /// The first of its class that fits no runner now: its room is kept on `runner` from `at`
    /// (unix ms), when the grants there are expected to have left it room (W05-R39).
    Reserved { runner: String, at: i64 },
    /// It would fit on `runner`, but only into the room kept there for `leg` from `at` (unix ms),
    /// past which it would run (W05-R39).
    Behind { leg: String, runner: String, at: i64 },
    /// Its gang `gang` starts together or not at all (W05-R40): `have` of its `of` members are
    /// queued.
    Gang { gang: String, have: u32, of: u32 },
}

/// Why the runner a retry asked for can't take its leg now (W05-R28).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PinnedState {
    /// Every slot taken (or its room held by a reservation or another's exclusive leg).
    Busy,
    Draining,
    Offline,
    /// It no longer meets the leg's requirements.
    Unable,
}

impl From<Held> for WaitReason {
    fn from(h: Held) -> Self {
        match h {
            Held::Coalescing { lane, until, deadline } => WaitReason::Coalescing { lane, until, deadline },
            Held::Cadence { lane, next } => WaitReason::Cadence { lane, next },
            Held::Speculation { lane, size, in_flight } => WaitReason::Speculation { lane, size, in_flight },
        }
    }
}

/// A decision about a leg's work in demands (W28-R45), for its routing and its run's page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Decision {
    /// What its ask got: it executes, adopts, or (its run withdrawn) is dropped (W28-R42).
    Asked(Ask),
    /// What becomes of its execution: kept, dropped, run on, finished or cancelled (W28-R43).
    Fate(Fate),
    /// What it does once the execution it adopted ended, or its check came back (W28-R44).
    Next(Next),
}

impl Decision {
    /// Why, in words.
    pub fn why(&self) -> String {
        match self {
            Decision::Asked(a) => a.why(),
            Decision::Fate(f) => f.why(),
            Decision::Next(n) => n.why(),
        }
    }
}

/// What to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    StartRun { run: String },
    CancelRun { run: String, reason: String },
    /// `image` is the routing rule's, for container and microVM executors (W13); `docker`, the
    /// Docker access the rules give the leg (W13-R04); `slot`, the pool's executor it runs in
    /// (W13-R09). The grant (W05-R34): its fence, until when it lives (unix ms; `None` with no
    /// lease) unless renewed, and what its runner does once its lease is lost.
    Assign {
        leg: String,
        runner: String,
        attempt: u32,
        executor: Option<String>,
        image: Option<String>,
        docker: Option<String>,
        slot: Option<String>,
        fence: Fence,
        expires: Option<i64>,
        lease_loss: LeaseLoss,
    },
    /// Start one of a pool's executors on `runner`, ready for a leg (W13-R09).
    /// `leg`: the one it's made for (a `k8s` pod of that leg's containers, W13-R07).
    StartExecutor {
        pool: String,
        executor: String,
        runner: String,
        kind: String,
        image: Option<String>,
        leg: Option<String>,
    },
    /// Stop an idle executor (past its pool's `idle_timeout`, or its pool gone).
    StopExecutor { executor: String, runner: String },
    /// Stop a leg's attempt on its runner.
    Cancel { leg: String, attempt: u32, reason: String },
    /// A failed attempt will be retried at `at` as `attempt`.
    Retry { leg: String, attempt: u32, at: i64 },
    Wait { leg: String, reason: WaitReason },
    /// The leg is done for good: the plan hears of it now.
    Settled { leg: String, outcome: Outcome, reason: Option<String> },
    Timer { at: i64 },
    /// What became of a leg's work in demands, and why (W28-R45).
    Demand { leg: String, decision: Decision },
    /// An adopter's execution ended: it's done, reused from `from`, as `from` ended (W28-R44).
    Adopted { leg: String, from: String, outcome: Outcome },
    /// The engine took a grant back (W05-R34): `attempt` of `leg` on `runner`, under `fence`, is
    /// over, lost as `why` says; a `Retry` or `Settled` for the leg follows. Its runner is to stop
    /// it, and what it writes under that fence is to be refused.
    Lost { leg: String, attempt: u32, runner: String, fence: Fence, why: Loss },
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    /// Waiting in its concurrency group.
    Pending,
    /// Held by admission (W28-R60).
    Held,
    Queued,
    Retrying { at: i64 },
    Assigned { runner: String },
    /// It adopted `exec` (W28-R42): it waits for it, holding no runner, slot or quota.
    Adopted { exec: String },
    /// `from` ended as `outcome`, and it checks what `from` read first (W24-R04).
    Checking { from: String, outcome: Outcome },
    Done,
}

/// How long a leg a person's retry placed on a runner waits when that runner refuses it (W05-R28).
pub const REFUSED_AGAIN_MS: i64 = 15_000;

/// A leg's timeout when its job says none (W05-R38): 360 minutes, GitHub's.
pub const DEFAULT_TIMEOUT_MS: Ms = 360 * 60_000;

/// Why a leg stays off a runner (W05-R16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Off {
    /// It lost the leg: the retry goes to another runner when one could take it, else to it.
    Lost,
    /// Its retry policy asks for a different runner (`runner: different`): it waits for one.
    Different,
    /// It refused the leg: the leg waits for another.
    Refused,
}

#[derive(Debug, Clone)]
struct Leg {
    input: LegIn,
    req: Requirement,
    sig: String,
    state: State,
    attempt: u32,
    /// When it was (last) queued, for aging.
    queued_at: i64,
    /// When it arrived: a leg admission held counts its wait from then.
    arrived: i64,
    /// Its place: priority, arrival.
    key: Option<Key>,
    /// Its tenant's leaf in fair share's tree (docs/needs.md E2.2): the queue keeps its legs by it.
    flow: String,
    /// What its dispatch charged fair share, while it runs.
    charge: Option<Charge>,
    reason: Option<WaitReason>,
    /// A runner it stays off, and why (W05-R16).
    avoid: Option<(String, Off)>,
    pin: Option<String>,
    last_runner: Option<String>,
    /// Cancelled while it ran, and why: however its attempt ends, it ends cancelled, never retried.
    cancelling: Option<String>,
    /// Gone with its repository while it ran: its attempt's end frees its runner, nothing else.
    gone: bool,
    /// When its attempt started (unix ms), while one runs: how long it has left (W28-R43).
    started: Option<i64>,
    /// The busy warm runner it waits for (W24-R18).
    hold: Option<Hold>,
    /// It waited for a warm runner in this attempt, and doesn't again.
    held: bool,
    /// While it runs: when it's expected to end (unix ms), so its runner has room again.
    ends: Option<i64>,
    /// While it runs, its grant (W05-R34): its fence, and when it expires unless renewed (`None`
    /// with no lease), as [`Scheduler::leases`] keeps it.
    fence: Fence,
    expires: Option<i64>,
    /// Not queued before then (W05-R34): a stop item queued again at a later attempt, as after a
    /// restart, waits out the lease of any earlier grant a runner cut off still holds.
    not_before: Option<i64>,
    /// While it runs, its grant's two ends (W05-R38).
    due: Option<Due>,
}

/// A running grant's two ends (W05-R38), unix ms: when it's expected to end, the end
/// reservations go by, and when it ends at the latest. Its median-based end, which ranks
/// runners (W24-R18), is [`Leg::ends`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Due {
    /// When it started.
    from: i64,
    /// Its start plus its p90, else its cost, else its timeout; never past `hard`.
    expected: i64,
    /// Its start plus its timeout: its runner stops it then.
    hard: i64,
}

impl Due {
    /// The ends of a grant of `l` started at `from`.
    fn of(l: &LegIn, from: i64) -> Due {
        let hard = later(from, l.timeout.unwrap_or(DEFAULT_TIMEOUT_MS));
        let expected = l.p90.or(l.cost).map_or(hard, |ms| later(from, ms).min(hard));
        Due { from, expected, hard }
    }

    /// The ends as looked at at `now`: an expected end that has passed, the grant still running,
    /// moves on to its start plus twice the time it has run, never past its hard end. The
    /// prediction is corrected once it's proved wrong, its kill-time kept apart (Tsafrir, Etsion
    /// and Feitelson, EASY++).
    fn at(self, now: i64) -> Due {
        if self.expected > now {
            return self;
        }
        let ran = now.saturating_sub(self.from).max(0);
        Due { expected: self.from.saturating_add(ran.saturating_mul(2)).min(self.hard), ..self }
    }
}

/// A leg's wait for a busy runner where it would finish sooner (W24-R18).
#[derive(Debug, Clone, PartialEq)]
struct Hold {
    runner: String,
    /// The runner's name, as the wait reason says it.
    name: String,
    saves_s: u64,
    until: i64,
    /// How long it's expected to take there: another runner where it takes no longer will do.
    there_ms: Ms,
}

#[derive(Debug, Clone)]
struct Runner {
    input: RunnerIn,
    running: Vec<String>,
    used: Resources,
    connected: bool,
    draining: bool,
}

/// What an exclusive leg holds of its runner (W05-R03): asking for CPUs, those and the memory it
/// asks, for itself alone, other legs running in the rest; asking for none, or on a runner whose
/// size isn't known, the whole runner.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Claim {
    Share(Resources),
    Whole,
}

fn claim(l: &LegIn, size: Option<Resources>) -> Option<Claim> {
    if !l.exclusive {
        return None;
    }
    Some(if !l.request.cpu.is_zero() && size.is_some() { Claim::Share(l.request) } else { Claim::Whole })
}

/// A runner's own size: its CPUs (threads) and memory, as it offers them in resource mode or else
/// as its facts say; `None` when they don't.
fn size(r: &Runner) -> Option<Resources> {
    if let Some(c) = r.input.capacity {
        return (!c.cpu.is_zero()).then_some(c);
    }
    let f = &r.input.view.facts;
    let cpus = if f.cpu.threads > 0 { f.cpu.threads } else { f.cpu.cores };
    (cpus > 0 && f.mem.total > 0).then(|| Resources { cpu: Cpus::whole(u64::from(cpus)), mem: f.mem.total, disk: f.disk.free, gpu: u32::try_from(f.gpus.len()).unwrap_or(u32::MAX) })
}

/// What a leg holds of a runner in each dimension fair share counts while it runs there (W05-R03,
/// docs/needs.md E2.1): one slot and its request, or, exclusive and asking for no CPUs, all of the
/// runner; nothing in a dimension the runner doesn't offer.
fn asks(l: &Leg, r: &Runner, resources: bool) -> [u64; DIMS] {
    let offers = offers(&r.input, resources);
    let q = l.input.request;
    let mut asks = if claim(&l.input, size(r)) == Some(Claim::Whole) { offers } else { [1, q.cpu.get(), q.mem, u64::from(q.gpu)] };
    for (a, o) in asks.iter_mut().zip(offers) {
        if o == 0 {
            *a = 0;
        }
    }
    asks
}

/// What a leg coming to a runner holds of it (W05-R03): nothing of its own, the whole runner, or
/// a share of its CPUs and memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Incoming {
    Ordinary,
    Whole,
    Share(Resources),
}

impl Incoming {
    /// An exclusive leg asking for CPUs holds them and the memory it asks; asking for none, the
    /// whole runner; any other leg, nothing.
    pub fn of(exclusive: bool, request: &Resources) -> Self {
        match (exclusive, !request.cpu.is_zero()) {
            (false, _) => Incoming::Ordinary,
            (true, true) => Incoming::Share(*request),
            (true, false) => Incoming::Whole,
        }
    }
}

/// The rule both dispatchers and a runner's own admission use, counting in slots (W05-R03):
/// whether a runner of `slots` and `size` (its CPUs and memory; `None` not known) has room for
/// one more, its exclusive legs holding `held` (one holding it `whole`), `ordinary` of its
/// `running` legs running beside them. An ordinary leg takes a slot of what isn't held; one
/// holding the whole runner needs it empty; a share needs as much neither held nor taken by the
/// ordinary legs there (a slot's share each), never more than the runner is, and on a runner
/// whose size isn't known, the runner empty. A share held on a runner whose size isn't known holds
/// it whole, as the engine claims one there: nothing has room beside it.
pub fn room(slots: u32, size: Option<Resources>, held: &Resources, whole: bool, ordinary: usize, running: usize, incoming: Incoming) -> bool {
    if whole || (size.is_none() && !(held.cpu.is_zero() && held.mem == 0)) {
        return false;
    }
    match incoming {
        Incoming::Ordinary => ordinary < slots_left(slots, size, held),
        Incoming::Whole => running == 0,
        Incoming::Share(q) => {
            let Some(s) = size else { return running == 0 };
            // The ordinary legs there take a slot's share each, `ordinary / slots` of the runner:
            // compared without dividing, so exactly.
            let (n, ordinary) = (u128::from(slots.max(1)), u128::try_from(ordinary).unwrap_or(u128::MAX));
            let cpu = u128::from(held.cpu.saturating_add(q.cpu).get()).saturating_mul(n).saturating_add(ordinary.saturating_mul(u128::from(s.cpu.get())));
            // Their memory rounds down, as it always has.
            let mem = ordinary.saturating_mul(u128::from(s.mem)) / n;
            cpu <= u128::from(s.cpu.get()).saturating_mul(n) && u128::from(held.mem).saturating_add(mem).saturating_add(u128::from(q.mem)) <= u128::from(s.mem)
        }
    }
}

/// The slots a runner keeps for ordinary legs beside what's held of it: its slots in the share of
/// its CPUs and memory no exclusive leg holds, rounded down.
pub fn slots_left(slots: u32, size: Option<Resources>, held: &Resources) -> usize {
    let n = slots.max(1);
    let Some(s) = size else { return n as usize };
    if held.cpu.is_zero() && held.mem == 0 {
        return n as usize;
    }
    let share = |total: u64, held: u64| -> u128 {
        if total == 0 {
            return u128::from(n);
        }
        u128::from(n) * u128::from(total.saturating_sub(held)) / u128::from(total)
    };
    usize::try_from(share(s.cpu.get(), held.cpu.get()).min(share(s.mem, held.mem))).unwrap_or(usize::MAX)
}

#[derive(Debug, Clone, Default)]
struct Sig {
    req: Requirement,
    eligible: BTreeSet<String>,
    /// Its legs queued, by class (promoted or not), then by their tenant's leaf, each leaf's in its
    /// tenant's own order, then arrival: which leaf goes first depends on the runner picking
    /// (docs/needs.md E2.1, E2.2, E2.3).
    queue: BTreeMap<Class, BTreeMap<String, BTreeSet<(i128, u64, String)>>>,
    /// When a leg of each class last started here: a class that did within its bound isn't passed
    /// over here, and its legs aren't promoted (W05-R31).
    served: BTreeMap<Class, i64>,
}

impl Sig {
    fn len(&self) -> usize {
        self.queue.values().flat_map(BTreeMap::values).map(BTreeSet::len).sum()
    }

    /// Its queued legs, in no order that decides anything.
    fn ids(&self) -> impl Iterator<Item = &String> {
        self.queue.values().flat_map(BTreeMap::values).flat_map(|legs| legs.iter().map(|(_, _, id)| id))
    }
}

/// One class's legs, as a runner takes them ([`Scheduler::walk`]): each leaf's next leg by (its
/// leaf's standing, its tenant's own order, its arrival), the least first. A class with one leaf
/// is walked as it's kept.
enum Walk<'a> {
    One { standing: [u128; LEVELS], legs: std::collections::btree_set::Iter<'a, (i128, u64, String)> },
    /// Each leaf's standing and its legs still to come: the next is the least (standing, own
    /// order, arrival) of their heads, found by looking at each (a class has few leaves).
    Many { leaves: Vec<([u128; LEVELS], std::iter::Peekable<std::collections::btree_set::Iter<'a, (i128, u64, String)>>)> },
}

impl<'a> Iterator for Walk<'a> {
    /// A leg: its leaf's standing, its place in its tenant's own order, its arrival, its id.
    type Item = ([u128; LEVELS], i128, u64, &'a String);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Walk::One { standing, legs } => legs.next().map(|(k, q, id)| (*standing, *k, *q, id)),
            Walk::Many { leaves } => {
                let mut best: Option<(([u128; LEVELS], i128, u64), usize)> = None;
                for (i, (standing, legs)) in leaves.iter_mut().enumerate() {
                    if let Some((k, q, _)) = legs.peek()
                        && best.is_none_or(|(b, _)| (*standing, *k, *q) < b)
                    {
                        best = Some(((*standing, *k, *q), i));
                    }
                }
                let ((standing, k, q), i) = best?;
                let (_, _, id) = leaves.get_mut(i)?.1.next()?;
                Some((standing, k, q, id))
            }
        }
    }
}

/// The dimensions a runner offers (docs/needs.md E2.1), in resource mode or not (`resources`).
pub fn dims(r: &RunnerIn, resources: bool) -> [bool; DIMS] {
    offers(r, resources).map(|o| o > 0)
}

/// The scheduler. Whatever it walks while deciding, it walks in one order on every machine and in
/// every replay (docs/needs.md, E1): what it keeps is ordered (`BTreeMap`, `BTreeSet`), but for
/// the legs, which it looks up on every input and keeps in a `Table` that walks in key order;
/// hash maps are left to what it only looks up by key and never walks: its settings and the
/// decision it last told each leg.
#[derive(Debug, Clone, Default)]
pub struct Scheduler {
    cfg: Config,
    runners: BTreeMap<String, Runner>,
    legs: Table<Leg>,
    sigs: BTreeMap<String, Sig>,
    groups: Groups,
    /// Runs by supersede key, oldest first.
    superseding: BTreeMap<String, Vec<String>>,
    run_groups: BTreeMap<String, String>,
    /// Fair share over the tenant tree, one tree per class (W05-R32, R33): each
    /// node's virtual finish under its parent, and each parent's virtual time (the root's under
    /// `""`), for every set of dimensions a runner may offer ([`profile_costs`]): start-time fair
    /// queuing's tags at every node, as DRFQ keeps them.
    finish: BTreeMap<Class, BTreeMap<String, [u128; PROFILES]>>,
    vtimes: BTreeMap<Class, BTreeMap<String, [u128; PROFILES]>>,
    /// What the fleet's connected runners that aren't draining offer in each dimension.
    fleet: [u64; DIMS],
    /// What each class's running items hold, in each dimension (W05-R31's floors).
    held: BTreeMap<Class, [u128; DIMS]>,
    /// When each class may next go over its floor to go ahead of the classes before it (W05-R31):
    /// what's left of its floor of the fleet's time, taken by what went over it.
    paced: BTreeMap<Class, i64>,
    /// Promotions to look at, by when: (leg, when it was queued, the class it goes up from)
    /// (W05-R31).
    promotions: BTreeMap<i64, Vec<(String, i64, Class)>>,
    /// The epoch its grants are made in (docs/needs.md §8: the store group's term), how many it
    /// has made in it (the last one's fence), and the grants that expire, by (expiry, leg)
    /// (W05-R34).
    epoch: u64,
    fences: u64,
    leases: BTreeSet<(i64, String)>,
    /// The time of the last timer asked for to look at the next promotion or expiry.
    timer: Option<i64>,
    seq: u64,
    timers: BTreeMap<i64, Vec<String>>,
    in_progress: BTreeMap<String, usize>,
    /// Pools' executors, by id.
    executors: BTreeMap<String, Executor>,
    /// (pool, runner) left alone until, and the wait after the next failure.
    backoff: BTreeMap<(String, String), (i64, i64)>,
    /// Each pool's last failure to start an executor: (runner, error).
    pool_errors: BTreeMap<String, (String, String)>,
    /// The reservations' windows open now (index, end), and when they're next looked at (W05-R12).
    windows: Vec<(usize, i64)>,
    windows_at: Option<i64>,
    /// Demands and the work in flight (W28-R40 to R45).
    demands: Demands,
    /// Each leg's last decision in demands, said once.
    decided: HashMap<String, Decision>,
    /// What waits to be admitted, and what admission knows of the legs it let through (W28-R60).
    admit: Admit,
    /// Runners kept from other work while an exclusive leg on a cadence waits for them (W28-R62):
    /// runner, leg.
    drained: BTreeMap<String, String>,
    /// Exclusive legs on a cadence that are queued.
    cadenced: BTreeSet<String>,
    /// What each runner holds warm, by runner (W24-R18).
    warm: BTreeMap<String, WarmHeld>,
    /// While a runner's loss is handled: the legs it lost that came back to the queue, offered once
    /// it's handled, in queue order with the others whose place changed (W05-R04).
    requeued: Option<Vec<String>>,
    /// Each class's reservation, when it has one (W05-R39): room kept for its first item that
    /// fits no runner now, or for each member of its first gang (W05-R40). Not stored: worked out
    /// again from the grants after a restart.
    reserved: BTreeMap<Class, Kept>,
    /// Gangs by id (W05-R40): their members as they've come, and how they go.
    gangs: BTreeMap<String, Crew>,
    /// The time of the last input: explanations reckon waits from it.
    now: i64,
}

/// Room kept for a class's first item that fits no runner now (W05-R39), or for each member of
/// its first gang (W05-R40): each item's request on its runner (`items`: leg, runner) from `at`,
/// when the grants there are expected to have left room for them all; worked out at `made`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    items: Vec<(String, String)>,
    at: i64,
    made: i64,
}

impl Kept {
    /// Whether it keeps room for `leg`.
    fn holds(&self, leg: &str) -> bool {
        self.items.iter().any(|(l, _)| l == leg)
    }

    /// Whether it keeps room on `runner`.
    fn on(&self, runner: &str) -> bool {
        self.items.iter().any(|(_, r)| r == runner)
    }

    /// The items whose room it keeps on `runner`.
    fn legs_on<'a>(&'a self, runner: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        self.items.iter().filter(move |(_, r)| r == runner).map(|(l, _)| l)
    }
}

/// A gang as the engine knows it (W05-R40): how many make it whole, its members as they've
/// come, those it last started together (`round`; a member come since waits for its next start),
/// and whether one of their attempts has ended since, after which none is tried again alone.
#[derive(Debug, Clone, Default)]
struct Crew {
    size: u32,
    members: BTreeSet<String>,
    round: BTreeSet<String>,
    ended: bool,
}

/// What a runner holds, as the room rule counts it (W05-R03): the share its exclusive legs hold,
/// whether one holds it whole, how many ordinary legs run beside them, what's used of its
/// resources, and how many legs it runs.
#[derive(Debug, Clone, Copy, Default)]
struct Holding {
    held: Resources,
    whole: bool,
    ordinary: usize,
    used: Resources,
    running: usize,
}

impl Scheduler {
    pub fn new(cfg: Config) -> Scheduler {
        Scheduler::with_epoch(cfg, 0)
    }

    /// A scheduler whose grants are made in `epoch` (docs/needs.md §8: the store group's term, so
    /// a later leader's are later).
    pub fn with_epoch(cfg: Config, epoch: u64) -> Scheduler {
        let admit = Admit::new(cfg.window);
        Scheduler { cfg, admit, epoch, ..Scheduler::default() }
    }

    fn config(&self) -> &Config {
        &self.cfg
    }

    /// The epoch its grants are made in (W05-R34).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A new epoch (W05-R25): its node leads again, the group's term risen. Grants from now are
    /// made in it, numbered afresh (each fence still greater than any of an earlier epoch's);
    /// those made before keep their fences. Never back.
    pub fn new_epoch(&mut self, epoch: u64) {
        if epoch > self.epoch {
            (self.epoch, self.fences) = (epoch, 0);
        }
    }

    /// How long its grants live unrenewed (W05-R34), if they're leases.
    pub fn lease(&self) -> Option<Ms> {
        self.cfg.lease
    }

    /// Handles one input at `now` (unix milliseconds).
    pub fn handle(&mut self, input: Input, now: i64) -> Vec<Command> {
        let mut out = Vec::new();
        self.now = now;
        // Every leg in the class its wait has brought it to by now (W05-R31).
        self.promote_due(now);
        // Every grant not renewed by now is lost before anything else is decided (W05-R34): a
        // renewal that comes at or after its expiry comes too late.
        self.expire_due(now, &mut out);
        // Reservations' windows that opened or closed since: what may run where changes.
        if !self.cfg.reservations.is_empty() && self.windows_at.is_none_or(|at| now >= at) {
            self.refresh_windows(now, &mut out);
        }
        match input {
            Input::RunQueued { run, group, supersede } => self.run_queued(run, group, supersede, now, &mut out),
            Input::RunFinished { run } => {
                for v in self.superseding.values_mut() {
                    v.retain(|r| *r != run);
                }
                if let Some(key) = self.run_groups.remove(&run) {
                    for a in self.groups.leave(&key, &format!("run:{run}")) {
                        self.group_action(a, now, &mut out);
                    }
                }
                self.demands.leave(&run);
            }
            Input::Demand(d) => {
                let _ = self.demands.arrive(*d, false);
            }
            Input::Checked { leg, matched } => {
                let outcome = match self.legs.get(&leg).map(|l| &l.state) {
                    Some(State::Checking { outcome, .. }) => Some(outcome.clone()),
                    _ => None,
                };
                if let Some(outcome) = outcome
                    && let Some(next) = self.demands.checked(&leg, matched)
                {
                    self.next_for(&leg, next, Some(&outcome), now, &mut out);
                }
            }
            Input::LegQueued(leg) => self.leg_queued(*leg, now, &mut out),
            Input::LegRunning { leg, runner, attempt, fence } => self.adopt(*leg, runner, attempt, fence, now, &mut out),
            Input::LegCancelled { leg, reason } => self.cancel(&leg, reason, now, &mut out),
            Input::LegGone { leg } => self.gone(&leg, now, &mut out),
            Input::AttemptFinished { leg, attempt, outcome } => self.finished(&leg, attempt, outcome, now, &mut out),
            Input::Rejected { leg, fence } => {
                if self.live(&leg, fence)
                    && let Some(runner) = self.release(&leg, now)
                {
                    self.stopped(&leg);
                    self.discard_used(&leg, &mut out);
                    // Its gang's other members go back too, to go again whole (W05-R40).
                    let freed = self.take_back_gang(&leg, now, &mut out);
                    // Refused by the runner a person's retry asked for (W05-R28): only it may run
                    // the leg, so it's asked again in a while, as v1 offers a declined job again.
                    if self.legs.get(&leg).is_some_and(|l| l.input.placement.runner.as_deref() == Some(runner.as_str())) {
                        let at = now + REFUSED_AGAIN_MS;
                        if let Some(l) = self.legs.get_mut(&leg) {
                            l.state = State::Retrying { at };
                        }
                        self.timers.entry(at).or_default().push(leg.clone());
                        out.push(Command::Timer { at });
                        let name = self.runners.get(&runner).map_or_else(|| runner.clone(), |r| r.input.view.name.clone());
                        self.set_reason(&leg, WaitReason::Pinned { runner: name, state: PinnedState::Busy }, &mut out);
                    } else {
                        // Refused by a runner away or draining (an assignment that couldn't reach
                        // it): that says nothing of the leg, and it may take it once it's back
                        // (W05-R35), so the leg isn't kept off it; whichever runner is first takes
                        // it.
                        let away = self.runners.get(&runner).is_some_and(|r| r.draining || !r.connected);
                        if let Some(l) = self.legs.get_mut(&leg).filter(|_| !away) {
                            l.avoid = Some((runner.clone(), Off::Refused));
                        }
                        self.enqueue(&leg, now, &mut out);
                    }
                    self.try_runner(&runner, now, &mut out);
                    for r in freed {
                        self.try_runner(&r, now, &mut out);
                    }
                }
            }
            Input::Ungranted { leg, fence } => {
                if self.live(&leg, fence)
                    && let Some(runner) = self.release(&leg, now)
                {
                    self.stopped(&leg);
                    self.discard_used(&leg, &mut out);
                    // Its gang's other members go back too, to go again whole (W05-R40).
                    let freed = self.take_back_gang(&leg, now, &mut out);
                    let at = now + REFUSED_AGAIN_MS;
                    if let Some(l) = self.legs.get_mut(&leg) {
                        l.state = State::Retrying { at };
                    }
                    self.timers.entry(at).or_default().push(leg.clone());
                    out.push(Command::Timer { at });
                    self.try_runner(&runner, now, &mut out);
                    for r in freed {
                        self.try_runner(&r, now, &mut out);
                    }
                }
            }
            Input::Renewed { runner, held } => self.renew(&runner, &held, now),
            Input::RunnerUp(r) | Input::RunnerChanged(r) => self.runner_up(*r, now, &mut out),
            Input::RunnerLost { runner } => self.runner_lost(&runner, now, &mut out),
            Input::RunnerDraining { runner } => {
                if let Some(r) = self.runners.get_mut(&runner) {
                    r.draining = true;
                }
                self.refresh_fleet();
                self.release_holds(now, &mut out);
                self.try_avoided(now, &mut out);
            }
            Input::RulesChanged(rules) => self.rules_changed(rules, now, &mut out),
            Input::ExecutorReady { executor } => self.executor_ready(&executor, now, &mut out),
            Input::ExecutorFailed { executor, error } => self.executor_failed(&executor, error, now, &mut out),
            Input::PoolsChanged { pools, reservations } => {
                self.cfg.pools = pools;
                self.cfg.reservations = reservations;
                self.windows_at = None;
                self.refresh_windows(now, &mut out);
                // A pool gone: its idle and starting executors stop; busy ones end with their legs.
                let gone: Vec<Executor> = self.executors.values().filter(|e| !matches!(e.state, ExecState::Busy { .. }) && !self.cfg.pools.iter().any(|p| p.name == e.pool)).cloned().collect();
                for e in gone {
                    self.drop_executor(&e.id);
                    out.push(Command::StopExecutor { executor: e.id, runner: e.runner });
                }
                let rules = self.cfg.rules.clone();
                self.rules_changed(rules, now, &mut out);
                self.scale_all(now, &mut out);
            }
            Input::Tick => self.tick(now, &mut out),
            Input::Warm { runner, held } => self.warm_changed(runner, held),
        }
        // A reservation whose item left the queue, or whose runner went, keeps nothing (W05-R39).
        self.drop_stale_reservations();
        // G4 (W05-R40), held after every input of every test: no gang runs in part.
        #[cfg(test)]
        {
            let g4 = self.check_gangs();
            assert!(g4.is_ok(), "{g4:?}");
        }
        // A timer for the next promotion to look at or grant to expire, so each is looked at then
        // and not at whatever input comes next (W05-R31, R34): one at a time, the earlier of the
        // two, asked for again once it has passed or when an earlier one is due.
        let next = [self.promotions.first_key_value().map(|(at, _)| *at), self.leases.first().map(|(at, _)| *at)].into_iter().flatten().min().map(|at| at.max(now));
        if let Some(next) = next
            && self.timer.is_none_or(|t| t <= now || next < t)
        {
            self.timer = Some(next);
            out.push(Command::Timer { at: next });
        }
        out
    }

    /// Whether `fence` is `leg`'s grant now: it's assigned, under that grant (W05-R34).
    fn live(&self, leg: &str, fence: Fence) -> bool {
        self.legs.get(leg).is_some_and(|l| l.fence == fence && matches!(l.state, State::Assigned { .. }))
    }

    /// `runner` renews the grants it names (W05-R34): each that's its own, under that fence, lives
    /// a lease from now. One it doesn't hold, or that's been granted again since, isn't renewed.
    fn renew(&mut self, runner: &str, held: &[(String, Fence)], now: i64) {
        let Some(lease) = self.cfg.lease else { return };
        let until = later(now, lease);
        for (id, fence) in held {
            let Some(l) = self.legs.get_mut(id) else { continue };
            if l.fence != *fence || !matches!(&l.state, State::Assigned { runner: r } if r == runner) {
                continue;
            }
            if let Some(was) = l.expires.filter(|was| *was < until) {
                l.expires = Some(until);
                self.leases.remove(&(was, id.clone()));
                self.leases.insert((until, id.clone()));
            }
        }
    }

    /// Every grant past its expiry by `now` is lost, the earliest first (W05-R34): its attempt
    /// ends as its runner's loss would end it (W05-R16's retry), and a stop item's next attempt
    /// waits the lock-delay past the expiry ([`Self::finished`]).
    fn expire_due(&mut self, now: i64, out: &mut Vec<Command>) {
        while let Some((at, id)) = self.leases.first().cloned().filter(|(at, _)| *at <= now) {
            self.leases.remove(&(at, id.clone()));
            let Some(l) = self.legs.get(&id) else { continue };
            let State::Assigned { runner } = &l.state else { continue };
            // One gone with its repository leaves nothing to tell (its records are gone).
            if !l.gone {
                out.push(Command::Lost { leg: id.clone(), attempt: l.attempt, runner: runner.clone(), fence: l.fence, why: Loss::Expired });
            }
            let attempt = l.attempt;
            self.finished(&id, attempt, Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }), now, out);
        }
    }

    fn run_queued(&mut self, run: String, group: Option<(String, String, GroupConfig)>, supersede: Option<(String, Supersede)>, now: i64, out: &mut Vec<Command>) {
        if let Some((key, action)) = supersede {
            if action == Supersede::Adopt {
                self.withdraw(&run, key, now, out);
            } else {
                let older = self.superseding.entry(key).or_default();
                let olds: Vec<String> = std::mem::take(older);
                older.push(run.clone());
                for o in olds {
                    if action == Supersede::Cancel {
                        out.push(Command::CancelRun { run: o, reason: "superseded by a newer run".into() });
                    } else {
                        self.deprioritize(&o);
                    }
                }
            }
        }
        match group {
            Some((key, name, cfg)) => {
                self.run_groups.insert(run.clone(), key.clone());
                for a in self.groups.arrive(&key, &name, &format!("run:{run}"), &cfg) {
                    self.group_action(a, now, out);
                }
            }
            None => out.push(Command::StartRun { run }),
        }
    }

    /// The older run's queued legs drop to the lowest priority.
    fn deprioritize(&mut self, run: &str) {
        let ids: Vec<String> = self.legs.iter().filter(|(_, l)| l.input.run == run && l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in ids {
            self.reprioritize(&id, 0);
        }
    }

    fn group_action(&mut self, a: GroupAction, now: i64, out: &mut Vec<Command>) {
        match a {
            GroupAction::Start(m) => {
                if let Some(run) = m.strip_prefix("run:") {
                    out.push(Command::StartRun { run: run.to_owned() });
                } else if let Some(leg) = m.strip_prefix("leg:") {
                    self.arrive(leg, now, out);
                }
            }
            GroupAction::Cancel { member, running, reason } => {
                if let Some(run) = member.strip_prefix("run:") {
                    out.push(Command::CancelRun { run: run.to_owned(), reason });
                } else if let Some(leg) = member.strip_prefix("leg:") {
                    let leg = leg.to_owned();
                    if running {
                        if let Some(l) = self.legs.get(&leg)
                            && matches!(l.state, State::Assigned { .. })
                        {
                            out.push(Command::Cancel { leg: leg.clone(), attempt: l.attempt, reason });
                        }
                    } else {
                        self.unqueue(&leg);
                        self.legs.remove(&leg);
                        out.push(Command::Settled { leg: leg.clone(), outcome: Outcome::Cancelled, reason: Some(reason) });
                        self.left_admission(&leg, None, now, out);
                    }
                }
            }
        }
    }

    /// The pool that runs a leg of `route` (W13-R09): the first that serves its labels, and for an
    /// untrusted leg one whose executors may run untrusted code (W18-R07).
    pub fn pool_for(&self, route: &LegRoute) -> Option<&Pool> {
        let untrusted_ok = |p: &Pool| route.untrusted.as_ref().is_none_or(|u| u.executors.contains(&p.executor));
        self.config().pools.iter().find(|p| p.serves(&route.labels) && untrusted_ok(p))
    }

    fn requirement(&self, input: &LegIn) -> Requirement {
        // A pool that serves the leg's labels runs it, on the hosts the pool names.
        if let Some(p) = self.pool_for(&input.route) {
            let mut constraints = p.hosts.clone();
            for c in &input.route.requires {
                match routing::Constraint::parse(c) {
                    Ok(x) => constraints.push((c.clone(), x)),
                    Err(e) => constraints.push((e, routing::Constraint::Or(Vec::new()))),
                }
            }
            return Requirement {
                constraints,
                caps: input.route.caps | p.caps,
                executors: vec![p.executor.clone()],
                image: p.image.clone(),
                pool: Some(p.name.clone()),
                rootless_docker: input.route.untrusted.as_ref().is_some_and(|u| !u.rootful_docker),
                ..Requirement::default()
            };
        }
        routing::resolve(&input.route, &self.config().rules).unwrap_or_else(|e| {
            // A constraint that doesn't parse: no runner can satisfy it, and the leg says why.
            Requirement { labels: input.route.labels.clone(), constraints: vec![(e, routing::Constraint::Or(Vec::new()))], caps: input.route.caps, ..Requirement::default() }
        })
    }

    fn leg_queued(&mut self, mut input: LegIn, now: i64, out: &mut Vec<Command>) {
        let id = input.id.clone();
        if self.legs.get(&id).is_some_and(|l| l.state != State::Done) {
            return;
        }
        let mut req = self.requirement(&input);
        // A pool's leg takes what its executor holds already.
        if req.pool.is_some() {
            input.request = Resources::default();
        }
        req.prefer.extend(input.prefer.iter().cloned());
        let sig = req.signature();
        self.ensure_sig(&sig, &req);
        let group = input.group.clone();
        let attempt = input.attempt.max(1);
        // Queued at a later attempt, which only a restart's rebuild does (W05-R25): an earlier grant
        // of a stop item may still be held by its runner, cut off, until its lease runs out by that
        // runner's clock, and the store doesn't keep when the engine would have let the next
        // attempt go (the grant's expiry and the lock-delay). It waits out a whole lease and the
        // lock-delay from now, past the end of any lease from before (W05-R34).
        let not_before = match (attempt > 1 && input.on_lease_loss == LeaseLoss::Stop, self.cfg.lease) {
            (true, Some(lease)) => Some(later(now, lease).saturating_add(ms_i64(self.cfg.lock_delay))),
            _ => None,
        };
        self.join_gang(&id, input.gang.as_ref());
        self.legs.insert(
            id.clone(),
            Leg { input, req, sig, state: State::Pending, attempt, queued_at: now, arrived: now, key: None, flow: String::new(), charge: None, reason: None, avoid: None, pin: None, last_runner: None, cancelling: None, gone: false, started: None, hold: None, held: false, ends: None, fence: Fence::default(), expires: None, not_before, due: None },
        );
        match group {
            Some((key, name, cfg)) => {
                let actions = self.groups.arrive(&key, &name, &format!("leg:{id}"), &cfg);
                let started = actions.iter().any(|a| *a == GroupAction::Start(format!("leg:{id}")));
                for a in actions {
                    self.group_action(a, now, out);
                }
                if !started && self.legs.get(&id).is_some_and(|l| l.state == State::Pending) {
                    let holders = self.groups.holders(&key).into_iter().map(|h| h.trim_start_matches("leg:").to_owned()).collect();
                    self.set_reason(&id, WaitReason::Concurrency { group: name, holders }, out);
                }
            }
            None => self.arrive(&id, now, out),
        }
    }

    // ------------------------------------------------------------------ admission (W28-R60)

    /// A leg ready to queue goes through admission first, when its workflow asks for it.
    fn arrive(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(d) = self.legs.get(id).and_then(|l| l.input.admission.clone()) else { return self.enqueue(id, now, out) };
        if let Some(l) = self.legs.get_mut(id) {
            l.arrived = now;
        }
        let saturated = self.saturated(id);
        let ds = self.admit.arrive(id, d, saturated, now);
        self.admitted(ds, now, out);
    }

    /// Whether a leg would wait for capacity now: no runner that could take it has room for it.
    fn saturated(&self, id: &str) -> bool {
        let Some(l) = self.legs.get(id) else { return false };
        let Some(s) = self.sigs.get(&l.sig) else { return true };
        !s.eligible.iter().filter_map(|r| self.runners.get(r)).any(|r| self.fits(r, l))
    }

    /// Carries out admission's decisions.
    fn admitted(&mut self, ds: Vec<admit::Decision>, now: i64, out: &mut Vec<Command>) {
        // A leg that takes the place of one on the cadence keeps that one's wait.
        let mut inherit: HashMap<String, i64> = HashMap::new();
        for d in ds {
            match d {
                admit::Decision::Admit { id } => {
                    let Some(l) = self.legs.get(&id) else { continue };
                    if !matches!(l.state, State::Pending | State::Held) {
                        continue;
                    }
                    let since = inherit.get(&id).copied().map_or(l.arrived, |t| t.min(l.arrived));
                    self.enqueue_since(&id, now, since, out);
                }
                admit::Decision::Hold { id, held } => {
                    if let Some(l) = self.legs.get_mut(&id) {
                        l.state = State::Held;
                    }
                    self.set_reason(&id, held.into(), out);
                }
                admit::Decision::Skip { id, by, why } => {
                    let Some(l) = self.legs.get(&id) else { continue };
                    let (queued, since) = (l.state == State::Queued, l.queued_at);
                    if why == Skipped::Cadence && queued {
                        inherit.insert(by.clone(), since);
                    }
                    let run = self.legs.get(&by).map_or(by.clone(), |b| b.input.run.clone());
                    let reason = match why {
                        Skipped::Coalesced => format!("coalesced into run {run}, newer in its lane, while the runners it needs were busy"),
                        Skipped::Cadence => format!("skipped by its workflow's cadence for run {run}, newer in its lane"),
                    };
                    if queued {
                        self.unqueue(&id);
                    }
                    self.settle(&id, Outcome::Cancelled, Some(reason), now, out);
                }
                admit::Decision::Timer { at } => {
                    self.timers.entry(at).or_default();
                    out.push(Command::Timer { at });
                }
            }
        }
    }

    /// Lets go what admission held that may go now: its time came, the runners it needs freed, or
    /// its window has room.
    fn review_admission(&mut self, now: i64, out: &mut Vec<Command>) {
        self.refresh_drains();
        if self.admit.idle() {
            return;
        }
        let free: BTreeSet<String> = self.admit.coalescing().filter(|id| !self.saturated(id)).map(str::to_owned).collect();
        let ds = self.admit.review(now, &|id| free.contains(id));
        self.admitted(ds, now, out);
    }

    /// Exclusive legs on a cadence each keep one runner from other work until it's theirs
    /// (W28-R62): the least busy that could take them, then by name.
    fn refresh_drains(&mut self) {
        if self.drained.is_empty() && self.cadenced.is_empty() {
            return;
        }
        let legs = &self.legs;
        let runners = &self.runners;
        self.cadenced.retain(|id| legs.get(id).is_some_and(|l| l.state == State::Queued));
        let cadenced = &self.cadenced;
        self.drained.retain(|r, id| cadenced.contains(id) && runners.get(r).is_some_and(|x| x.connected && !x.draining));
        let waiting: Vec<String> = self.cadenced.iter().filter(|id| !self.drained.values().any(|l| l == *id)).cloned().collect();
        for id in waiting {
            let Some(s) = self.legs.get(&id).and_then(|l| self.sigs.get(&l.sig)) else { continue };
            let target = s
                .eligible
                .iter()
                .filter_map(|r| self.runners.get(r))
                .filter(|r| r.connected && !r.draining && !self.drained.contains_key(&r.input.id))
                .min_by(|a, b| a.running.len().cmp(&b.running.len()).then_with(|| a.input.id.cmp(&b.input.id)))
                .map(|r| r.input.id.clone());
            if let Some(t) = target {
                self.drained.insert(t, id);
            }
        }
    }

    fn adopt(&mut self, input: LegIn, runner: String, attempt: u32, fence: Fence, now: i64, out: &mut Vec<Command>) {
        let id = input.id.clone();
        // What runs there changed under them: the reservations there are worked out again when
        // next they matter (W05-R39).
        self.reserved.retain(|_, k| !k.on(&runner));
        // Its grant keeps its fence, and gets a lease afresh: its runner renews it once it's back,
        // or it's lost then (W05-R34).
        let expires = self.cfg.lease.map(|ms| later(now, ms));
        if let Some(was) = self.legs.get(&id).and_then(|l| l.expires) {
            self.leases.remove(&(was, id.clone()));
        }
        let req = self.requirement(&input);
        let sig = req.signature();
        self.ensure_sig(&sig, &req);
        if let Some((key, name, cfg)) = &input.group {
            let _ = self.groups.arrive(key, name, &format!("leg:{id}"), cfg);
        }
        *self.in_progress.entry(input.repo.clone()).or_default() += 1;
        let request = input.request;
        let (work, run, priority, started) = (input.work.clone(), input.run.clone(), input.priority, input.started);
        // Its ends from when it started, as the store has it, else from now (W05-R38).
        let due = Due::of(&input, started.unwrap_or(now));
        // Running, it's of its gang's last start (W05-R40).
        self.join_gang(&id, input.gang.as_ref());
        if let Some(c) = input.gang.as_ref().and_then(|g| self.gangs.get_mut(&g.id)) {
            c.round.insert(id.clone());
        }
        self.legs.insert(
            id.clone(),
            Leg {
                input,
                req,
                sig,
                state: State::Assigned { runner: runner.clone() },
                attempt,
                queued_at: 0,
                arrived: 0,
                key: None,
                flow: String::new(),
                charge: None,
                reason: None,
                avoid: None,
                pin: None,
                last_runner: Some(runner.clone()),
                cancelling: None,
                gone: false,
                started,
                hold: None,
                held: false,
                ends: None,
                fence,
                expires,
                not_before: None,
                due: Some(due),
            },
        );
        if let Some(at) = expires {
            self.leases.insert((at, id.clone()));
        }
        // When it started isn't known: it's expected to take its whole run from now (W24-R18).
        let ends = self.legs.get(&id).map(|l| later(now, self.expected_ms(l, &runner)));
        if let Some(l) = self.legs.get_mut(&id) {
            l.ends = ends;
        }
        // The runner may not have reconnected yet: keep what it runs until it does.
        let r = self.runners.entry(runner.clone()).or_insert_with(|| Runner { input: RunnerIn { id: runner.clone(), ..RunnerIn::default() }, running: Vec::new(), used: Resources::default(), connected: false, draining: false });
        r.running.push(id.clone());
        add(&mut r.used, &request);
        // W28-R42: what its runner still runs is the execution of its work. A queued duplicate
        // asked for before this was announced adopts it instead, with its adopters.
        let Some(work) = work else { return };
        if let Running::Took { .. } = self.demands.running(&id, &run, &work, priority, started.unwrap_or(now)) {
            for a in self.demands.adopters(&id).into_iter().map(str::to_owned).collect::<Vec<_>>() {
                if self.legs.get(&a).is_some_and(|l| l.state != State::Adopted { exec: id.clone() }) {
                    self.decide(&a, Decision::Asked(Ask::Adopt { exec: id.clone(), run: run.clone() }), out);
                    self.adopted(&a, id.clone(), run.clone(), now, out);
                }
            }
        }
    }

    fn ensure_sig(&mut self, sig: &str, req: &Requirement) {
        if self.sigs.contains_key(sig) {
            return;
        }
        let eligible: BTreeSet<String> = self.runners.iter().filter(|(_, r)| self.eligible_for(req, r)).map(|(k, _)| k.clone()).collect();
        self.sigs.insert(sig.to_owned(), Sig { req: req.clone(), eligible, ..Sig::default() });
    }

    fn weight(&self, repo: &str) -> PerMille {
        self.config().weights.get(repo).copied().filter(|w| *w > 0).unwrap_or(1000)
    }

    /// Puts a leg in its signature's queue and tries the runners that could take it. A leg with
    /// work first adopts the execution of its key in flight, if there's one its domain may adopt
    /// (W28-R42); live work that can't be placed then contends for the capacity of work no one
    /// wants (W28-R43).
    fn enqueue(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        self.enqueue_since(id, now, now, out);
    }

    /// [`Self::enqueue`], its wait counted from `since` (a leg admission held, from its arrival).
    fn enqueue_since(&mut self, id: &str, now: i64, since: i64, out: &mut Vec<Command>) {
        let Some(leg) = self.legs.get(id) else { return };
        if leg.state == State::Done {
            return;
        }
        // Not before its time (W05-R34): it waits as a retry does, and the retry's tick queues it.
        if let Some(at) = leg.not_before.filter(|at| *at > now) {
            if let Some(l) = self.legs.get_mut(id) {
                l.state = State::Retrying { at };
            }
            self.timers.entry(at).or_default().push(id.to_owned());
            out.push(Command::Timer { at });
            self.set_reason(id, WaitReason::Retry { at }, out);
            return;
        }
        let Some(leg) = self.legs.get(id) else { return };
        if let Some(work) = leg.input.work.clone()
            && !self.demands.is_execution(id)
        {
            let (run, priority) = (leg.input.run.clone(), leg.input.priority);
            let ask = self.demands.ask(id, &run, &work, priority);
            self.decide(id, Decision::Asked(ask.clone()), out);
            match ask {
                Ask::Adopt { exec, run } => return self.adopted(id, exec, run, now, out),
                Ask::Drop => return self.settle(id, Outcome::Cancelled, Some(Ask::Drop.why()), now, out),
                Ask::Execute => {}
            }
        }
        self.queue(id, now, since, out);
        if self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
            self.contend(id, now, out);
        }
    }

    fn queue(&mut self, id: &str, now: i64, since: i64, out: &mut Vec<Command>) {
        let Some(leg) = self.legs.get(id) else { return };
        let cadenced = leg.input.exclusive && leg.input.admission.as_ref().is_some_and(|a| a.cadence.is_some());
        // Start-time fair queuing over the tenant tree (W05-R10, W28-R66, docs/needs.md E2.2): it
        // waits in its leaf, whose tags are taken as it's dispatched, in the dimensions its runner
        // offers (E2.1).
        let flow = self.leaf(&leg.input);
        if let Some(l) = self.legs.get_mut(id) {
            l.queued_at = since;
            l.flow = flow;
        }
        self.set_key(id);
        self.insert_queue(id);
        if cadenced {
            self.cadenced.insert(id.to_owned());
            self.refresh_drains();
        }
        if let Some(back) = self.requeued.as_mut() {
            back.push(id.to_owned());
            return;
        }
        self.offer(id, false, now, out);
    }

    /// Offers a queued leg to the runners with room, best first: each takes the best legs it has
    /// room for, this one among them. `alone`: the leg goes to the best of them, which takes no
    /// other, as when several legs' places changed at once and each goes in turn. Then its pool
    /// scales for it, or it says why it waits.
    fn offer(&mut self, id: &str, alone: bool, now: i64, out: &mut Vec<Command>) {
        // A gang's member goes with its whole gang, or waits for it (W05-R40).
        if let Some(g) = self.legs.get(id).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone()) {
            if !self.place_gang(&g, now, out) {
                self.gang_reasons(&g, out);
            }
            return;
        }
        // The runners with room, best first; none once it waits for a busy warm one (W24-R18).
        let candidates = self.candidates(id);
        for r in candidates {
            if !self.legs.get(id).is_some_and(|l| l.state == State::Queued && l.hold.is_none()) {
                break;
            }
            if !alone {
                self.try_runner(&r, now, out);
            } else if self.runners.get(&r).zip(self.legs.get(id)).is_some_and(|(x, l)| self.allowed(x, l)) && !self.hold_for_warm(id, &r, now, out) {
                // As the room kept there lets it (W05-R39).
                self.assign(id, &r, now, out);
            }
        }
        if let Some(pool) = self.legs.get(id).filter(|l| l.state == State::Queued).and_then(|l| l.req.pool.clone()) {
            self.scale(&pool, now, out);
        }
        if self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
            let reason = self.wait_reason(id);
            self.set_reason(id, reason, out);
        }
    }

    /// The queue key: its class (as it's queued, before any promotion), its place in its tenant's
    /// own order from its priority and when it was queued, its arrival; and its promotion, if its
    /// class has a bound (W05-R31). Fair share's standing is applied as legs are picked
    /// ([`Self::try_runner`]).
    fn set_key(&mut self, id: &str) {
        self.seq += 1;
        let (seq, aging) = (self.seq, self.config().aging_per_minute);
        let Some(l) = self.legs.get_mut(id) else { return };
        let class = l.input.class;
        l.key = Some((class, own_order(l.input.priority, aging, l.queued_at), seq));
        l.state = State::Queued;
        let at = l.queued_at;
        self.promote_at(id, at, at, class);
    }

    /// When to look at the leg's promotion from `class`, which it reached at `since`, if the class
    /// has a bound: a bound after (W05-R31). `queued` is when it was queued, so a look kept from an
    /// earlier queuing does nothing.
    fn promote_at(&mut self, id: &str, queued: i64, since: i64, class: Class) {
        if let (Some(bound), Some(_)) = (self.class_config(class).bound, class.up()) {
            self.promotions.entry(since.saturating_add(ms_i64(bound))).or_default().push((id.to_owned(), queued, class));
        }
    }

    /// A class's floor and bound (W05-R31): none unless set.
    fn class_config(&self, class: Class) -> ClassConfig {
        self.config().classes.get(&class).copied().unwrap_or_default()
    }

    /// Looks at each promotion due by `now`, in turn (W05-R31), as each input comes (a timer asks
    /// for one when the next is due), so what's decided at `now` sees every leg in its class then.
    /// A leg that has been in its class for its bound goes up a class (never above `interactive`)
    /// if its class was passed over at its signature: started no leg there in the bound before. If
    /// it did, the class isn't starved there, and the leg is looked at again a bound after that
    /// start: in a class served, the order is fair share's and its tenant's own (docs/needs.md
    /// E2.3), never the longest wait's. A promoted leg keeps its place in its tenant's order and
    /// its arrival, and preempts nothing.
    fn promote_due(&mut self, now: i64) {
        while let Some(at) = self.promotions.first_key_value().map(|(at, _)| *at).filter(|at| *at <= now) {
            for (id, queued, from) in self.promotions.remove(&at).unwrap_or_default() {
                let (Some(to), Some(bound)) = (from.up(), self.class_config(from).bound.map(ms_i64)) else { continue };
                let Some(l) = self.legs.get(&id).filter(|l| l.state == State::Queued && l.queued_at == queued && l.key.is_some_and(|k| k.0 == from)) else { continue };
                if let Some(last) = self.sigs.get(&l.sig).and_then(|s| s.served.get(&from)).copied().filter(|t| *t > at.saturating_sub(bound)) {
                    self.promotions.entry(last.saturating_add(bound)).or_default().push((id, queued, from));
                    continue;
                }
                self.unqueue(&id);
                if let Some(l) = self.legs.get_mut(&id) {
                    l.key = l.key.map(|(_, k, q)| (to, k, q));
                }
                self.insert_queue(&id);
                self.promote_at(&id, queued, at, to);
            }
        }
    }

    /// What a class's running items hold of the fleet, in millionths of its dominant share: the
    /// largest over the dimensions the fleet has of their total there over the fleet's (W05-R31,
    /// as YARN measures a minimum share under DRF).
    fn held_of(&self, class: Class) -> u128 {
        self.held.get(&class).map_or(0, |held| held_share(held, &self.fleet))
    }

    /// Whether a class is short of its floor (docs/needs.md §7, W05-R31): it has one, and its
    /// running items hold less of the fleet than that.
    fn short(&self, class: Class) -> bool {
        let floor = self.class_config(class).floor;
        floor > 0 && self.held_of(class) < u128::from(floor) * 1000
    }

    /// A class short of its floor takes room ahead of the classes before it (W05-R31) with its best
    /// leg for the runner, `id`: when the leg keeps it within its floor, or else, when it would
    /// take it over, as the floor's pace allows ([`Self::pace`]); then with what's left of its
    /// floor, for the pace. Nothing when its best leg would take it over before then.
    fn ahead_of<'a>(&self, class: Class, id: &'a String, r: &Runner, now: i64) -> Option<(&'a String, Option<u128>)> {
        let l = self.legs.get(id)?;
        let floor = u128::from(self.class_config(class).floor) * 1000;
        let mut with = self.held.get(&class).copied().unwrap_or([0; DIMS]);
        for (h, a) in with.iter_mut().zip(asks(l, r, self.config().resources)) {
            *h = h.saturating_add(u128::from(a));
        }
        if held_share(&with, &self.fleet) <= floor {
            return Some((id, None));
        }
        let left = floor.saturating_sub(self.held_of(class)).max(1);
        self.paced.get(&class).is_none_or(|at| now >= *at).then_some((id, Some(left)))
    }

    /// The milliseconds of its floor's pace a leg going over its class's floor takes (W05-R31): its
    /// dominant share of the fleet × `ms` over what was left of the floor (`left`, millionths),
    /// rounded up. Taking room again no sooner, the class holds, on average, what was left of its
    /// floor with it: on one slot, a 15 % floor is a slot 15 % of the time, not every time it
    /// frees.
    fn pace_ms(shares: &[u128; DIMS], ms: Ms, left: u128) -> i64 {
        let dominant = shares.iter().copied().max().unwrap_or(0);
        i64::try_from(dominant.saturating_mul(u128::from(ms)).div_ceil(left.max(1))).unwrap_or(i64::MAX)
    }

    /// A leg went over its class's floor ([`Self::ahead_of`]): its class may next do so once the
    /// pace it takes has passed, kept with its charge to settle to the time it ran.
    fn pace(&mut self, id: &str, left: u128, now: i64) {
        let Some(c) = self.legs.get_mut(id).and_then(|l| l.charge.as_mut()) else { return };
        let took = Self::pace_ms(&c.shares, c.ms, left);
        c.paced = Some((took, left));
        let class = c.class;
        self.paced.insert(class, now.saturating_add(took));
    }

    /// A queued leg's place in queue order, as legs whose places changed together go in turn
    /// ([`Self::runner_lost`]): its class, its leaf's standing over every dimension, its place in
    /// its tenant's own order, its arrival; the least first. A runner picking goes by the
    /// dimensions it offers, and lets a class short of its floor go ahead ([`Self::try_runner`]).
    fn pick_key(&self, l: &Leg) -> Option<(Class, [u128; LEVELS], i128, u64)> {
        let (class, k, q) = l.key?;
        Some((class, self.standing_of(class, &l.flow, PROFILES - 1), k, q))
    }

    /// A leaf's standing for a runner offering the set of dimensions `set` ([`profile`];
    /// W05-R32, R33): at each node from its repository down, where it would start
    /// under its parent in that set (start-time fair queuing's tag: the parent's virtual time, or
    /// the node's own finish when that's later). Leaves compare level by level, as hierarchical SFQ
    /// walks its tree from the root. Each level is DRF over time among the dimensions the runner
    /// offers: one it doesn't (GPUs on a runner without them) doesn't count there.
    fn standing_of(&self, class: Class, leaf: &str, set: usize) -> [u128; LEVELS] {
        let (vtimes, finish) = (self.vtimes.get(&class), self.finish.get(&class));
        let (mut out, mut parent) = ([0; LEVELS], "");
        for (slot, node) in out.iter_mut().zip(nodes(leaf)) {
            let v = vtimes.and_then(|v| v.get(parent)).and_then(|v| v.get(set)).copied().unwrap_or(0);
            let f = finish.and_then(|f| f.get(node)).and_then(|f| f.get(set)).copied().unwrap_or(0);
            *slot = v.max(f);
            parent = node;
        }
        out
    }

    /// One class's legs in the order a runner offering the set of dimensions `set` takes them: by
    /// their leaf's standing, level by level, then their tenant's own order, then arrival
    /// (docs/needs.md E2.1 to E2.4).
    fn walk<'a>(&self, class: Class, level: &'a BTreeMap<String, BTreeSet<(i128, u64, String)>>, set: usize) -> Walk<'a> {
        if let (1, Some((leaf, legs))) = (level.len(), level.first_key_value()) {
            return Walk::One { standing: self.standing_of(class, leaf, set), legs: legs.iter() };
        }
        Walk::Many { leaves: level.iter().map(|(leaf, legs)| (self.standing_of(class, leaf, set), legs.iter().peekable())).collect() }
    }

    fn insert_queue(&mut self, id: &str) {
        let Some(l) = self.legs.get(id) else { return };
        let Some((class, k, q)) = l.key else { return };
        let (sig, flow) = (l.sig.clone(), l.flow.clone());
        if let Some(s) = self.sigs.get_mut(&sig) {
            s.queue.entry(class).or_default().entry(flow).or_default().insert((k, q, id.to_owned()));
        }
    }

    fn unqueue(&mut self, id: &str) {
        let Some(l) = self.legs.get(id) else { return };
        let Some((class, k, q)) = l.key else { return };
        let (sig, flow) = (l.sig.clone(), l.flow.clone());
        if let Some(s) = self.sigs.get_mut(&sig)
            && let Some(level) = s.queue.get_mut(&class)
        {
            if let Some(legs) = level.get_mut(&flow) {
                legs.remove(&(k, q, id.to_owned()));
                if legs.is_empty() {
                    level.remove(&flow);
                }
            }
            if level.is_empty() {
                s.queue.remove(&class);
            }
        }
    }

    /// A leg's tenant: the tenant tree's leaf it waits in, its repository, then its actor and its
    /// lane as `fair_by` goes (W05-R10, W28-R66, docs/needs.md E2.2): `repo\0actor\0lane`.
    fn leaf(&self, l: &LegIn) -> String {
        let mut leaf = l.repo.clone();
        for level in self.config().fair_by.iter().skip_while(|f| **f == FairBy::Repo) {
            let member = match level {
                FairBy::Actor => l.actor.as_str(),
                FairBy::Lane => l.lane.as_str(),
                FairBy::Repo => continue,
            };
            leaf.push('\0');
            leaf.push_str(member);
        }
        leaf
    }

    /// The weights of a leg's tenant's nodes, top down, in thousandths: its repository's from
    /// `[ci.fairness] weights`, the levels below it equal.
    fn weights_of(&self, l: &LegIn) -> [PerMille; LEVELS] {
        [self.weight(&l.repo), 1000, 1000]
    }

    /// Eligible runners with room for the leg, in the order to try them: by declared preference,
    /// else least loaded, then most free slots, then name (W05-R23).
    fn candidates(&self, id: &str) -> Vec<String> {
        let Some(l) = self.legs.get(id) else { return Vec::new() };
        let Some(s) = self.sigs.get(&l.sig) else { return Vec::new() };
        let rooms: Vec<&Runner> = s.eligible.iter().filter_map(|r| self.runners.get(r)).filter(|r| self.fits(r, l)).collect();
        let scores = self.scores(l, &rooms, None);
        let mut order: Vec<(usize, &Runner)> = rooms.into_iter().enumerate().collect();
        order.sort_by(|(i, a), (j, b)| {
            let (sa, sb) = (scores.get(*i).copied().unwrap_or(0), scores.get(*j).copied().unwrap_or(0));
            sb.cmp(&sa).then_with(|| lighter(a, b)).then_with(|| free(b).cmp(&free(a))).then_with(|| a.input.id.cmp(&b.input.id))
        });
        order.into_iter().map(|(_, r)| r.input.id.clone()).collect()
    }

    /// Whether a runner other than `avoided` could take a leg (W05-R16, "when one can take it"):
    /// one connected, eligible for it, not draining, allowed by its placement and big enough to hold
    /// it, with room now or not.
    fn another_could(&self, l: &Leg, avoided: &str) -> bool {
        self.sigs.get(&l.sig).is_some_and(|s| s.eligible.iter().any(|id| id != avoided && l.input.placement.allows(id) && self.runners.get(id).is_some_and(|r| !r.draining && self.could_hold(r, l))))
    }

    /// Whether a runner is big enough to hold a leg (W05-R03): would it have room for it, idle. A
    /// share within its size; in resource mode, a leg's request within its capacity × overcommit.
    fn could_hold(&self, r: &Runner, l: &Leg) -> bool {
        if l.req.pool.is_some() {
            return true;
        }
        let c = self.config();
        let size = size(r);
        match claim(&l.input, size) {
            Some(Claim::Whole) => true,
            Some(Claim::Share(q)) => size.is_some_and(|s| q.cpu <= s.cpu && q.mem <= s.mem),
            None => match (c.resources, r.input.capacity) {
                (true, Some(cap)) => fits_beside(&Resources::default(), &Resources::default(), &cap, &l.input.request, c),
                _ => true,
            },
        }
    }

    /// Tries again the runners that queued legs stay off only while another could take them
    /// (W05-R16's default): after a runner goes, drains or changes, that one may be the only one.
    fn try_avoided(&mut self, now: i64, out: &mut Vec<Command>) {
        let avoided: BTreeSet<String> = self.legs.values().filter(|l| l.state == State::Queued).filter_map(|l| l.avoid.as_ref().filter(|(_, off)| *off == Off::Lost).map(|(r, _)| r.clone())).collect();
        for r in avoided {
            self.try_runner(&r, now, out);
        }
    }

    /// What a runner's legs hold of it now: the share its exclusive legs hold, whether one holds
    /// it whole, and how many ordinary legs of its own run beside them (a pool's legs run in their
    /// executors). Read from the legs it runs, so a leg adopted after a restart holds as it did.
    fn holding(&self, r: &Runner) -> (Resources, bool, usize) {
        let size = size(r);
        let mut held = Resources::default();
        let (mut whole, mut ordinary) = (false, 0);
        for id in &r.running {
            let Some(l) = self.legs.get(id) else {
                ordinary += 1;
                continue;
            };
            if l.req.pool.is_some() {
                continue;
            }
            match claim(&l.input, size) {
                Some(Claim::Share(q)) => add(&mut held, &q),
                Some(Claim::Whole) => whole = true,
                None => ordinary += 1,
            }
        }
        (held, whole, ordinary)
    }

    /// Whether a runner takes a leg now: it could ([`Self::room`]); and for a leg that waits for a
    /// busy warm runner, it's that one or one where the leg would take no longer (W24-R18).
    fn fits(&self, r: &Runner, l: &Leg) -> bool {
        self.room(r, l) && l.hold.as_ref().is_none_or(|h| h.runner == r.input.id || self.estimate(l, &r.input.id, &Self::wants(l)).busy_ms() <= h.there_ms)
    }

    /// Whether a runner could take a leg now: it would ([`Self::admits`]), and has room for it
    /// ([`Self::capacity`]).
    fn room(&self, r: &Runner, l: &Leg) -> bool {
        self.admits(r, l) && self.capacity(r, l)
    }

    /// Whether a runner would take a leg once it has room: connected, not draining, not past
    /// admission's limits for a heavy leg (W24-R19), not kept for other legs, not one the leg stays
    /// off, the one it's pinned to, and the repository within its quota.
    fn admits(&self, r: &Runner, l: &Leg) -> bool {
        if !r.connected || r.draining {
            return false;
        }
        // A heavy leg isn't started on a runner past admission's limits (W24-R19).
        if self.held_by_load(r, l).is_some() {
            return false;
        }
        // Kept for an exclusive leg on a cadence (W28-R62).
        if self.drained.get(&r.input.id).is_some_and(|owner| *owner != l.input.id) {
            return false;
        }
        if self.reserved_against(r, l).is_some() {
            return false;
        }
        if let Some((a, off)) = &l.avoid
            && *a == r.input.id
            // The runner a person's retry asked for (W05-R28): the leg waits for it, not another.
            && l.input.placement.runner.as_deref() != Some(a.as_str())
            && (*off != Off::Lost || self.another_could(l, a))
        {
            return false;
        }
        if l.pin.as_deref().is_some_and(|p| p != r.input.id) {
            return false;
        }
        if !l.input.placement.allows(&r.input.id) {
            return false;
        }
        if let Some(q) = self.config().quotas.get(&l.input.repo)
            && self.in_progress.get(&l.input.repo).copied().unwrap_or(0) >= *q
        {
            return false;
        }
        true
    }

    /// Whether a runner has room for a leg now: none while an exclusive leg holds it whole; one of
    /// a pool's idle executors there for a pool's leg; for an exclusive leg, the whole runner
    /// empty when it asks for nothing, else a share of what no other share holds and the ordinary
    /// legs there don't take; for an ordinary leg, a slot or the resources beside what's held.
    fn capacity(&self, r: &Runner, l: &Leg) -> bool {
        let (held, whole, ordinary) = self.holding(r);
        if whole {
            return false;
        }
        // A pool's leg runs in one of its idle executors on that host: one made for it when it
        // needs that (W13-R07).
        if let Some(pool) = &l.req.pool {
            let own = self.own_leg(l);
            return self.executors.values().any(|e| e.pool == *pool && e.runner == r.input.id && e.state == ExecState::Idle && e.leg.as_deref() == own);
        }
        self.room_in(r, &Holding { held, whole, ordinary, used: r.used, running: r.running.len() }, l)
    }

    /// Whether a runner holding `h` has room for a leg that isn't a pool's ([`Self::capacity`]):
    /// none while a leg holds it whole; for an exclusive leg, the whole runner empty when it asks
    /// for nothing, else a share of what no other share holds and the ordinary legs there don't
    /// take; for an ordinary leg, a slot or the resources beside what's held.
    fn room_in(&self, r: &Runner, h: &Holding, l: &Leg) -> bool {
        if h.whole {
            return false;
        }
        let c = self.config();
        let size = size(r);
        match claim(&l.input, size) {
            // The whole runner: it waits for it to be empty.
            Some(Claim::Whole) => return h.running == 0,
            // A share: what it holds is its own, so of the runner's CPUs and memory that no other
            // share holds and the ordinary legs there don't take, never more than the runner is.
            Some(Claim::Share(q)) => {
                let Some(s) = size else { return false };
                return match (c.resources, r.input.capacity) {
                    (true, Some(_)) => {
                        let (cpu, mem) = (h.used.cpu.saturating_sub(h.held.cpu), h.used.mem.saturating_sub(h.held.mem));
                        h.held.cpu.saturating_add(cpu).saturating_add(q.cpu) <= s.cpu && h.held.mem.saturating_add(mem).saturating_add(q.mem) <= s.mem
                    }
                    _ => room(r.input.slots, size, &h.held, false, h.ordinary, h.running, Incoming::Share(q)),
                };
            }
            None => {}
        }
        // An ordinary leg: in what no exclusive leg holds.
        match (c.resources, r.input.capacity) {
            (true, Some(cap)) => fits_beside(&h.used, &h.held, &cap, &l.input.request, c),
            _ => room(r.input.slots, size, &h.held, false, h.ordinary, h.running, Incoming::Ordinary),
        }
    }

    /// What a runner would hold of the legs it runs that `keep` keeps, with `plus` beside them
    /// (W05-R39): as [`Self::holding`] counts them, its resources used less what the legs it
    /// doesn't keep ask and more what `plus` ask.
    fn holding_with(&self, r: &Runner, keep: impl Fn(&Leg) -> bool, plus: &[&Leg]) -> Holding {
        let size = size(r);
        let mut h = Holding { used: r.used, ..Holding::default() };
        let count = |h: &mut Holding, l: &Leg| match claim(&l.input, size) {
            Some(Claim::Share(q)) => add(&mut h.held, &q),
            Some(Claim::Whole) => h.whole = true,
            None => h.ordinary += 1,
        };
        for id in &r.running {
            let Some(l) = self.legs.get(id) else {
                h.ordinary += 1;
                h.running += 1;
                continue;
            };
            if !keep(l) {
                sub(&mut h.used, &l.input.request);
                continue;
            }
            h.running += 1;
            if l.req.pool.is_none() {
                count(&mut h, l);
            }
        }
        for l in plus {
            add(&mut h.used, &l.input.request);
            h.running += 1;
            count(&mut h, l);
        }
        h
    }

    /// What holds a heavy leg off a runner by the runner's load, when admission is on (W24-R19).
    fn held_by_load(&self, r: &Runner, l: &Leg) -> Option<super::load::Held> {
        super::load::held(self.cfg.admission.as_ref(), &l.input.load, l.input.cost, &r.input.view.name, &r.input.view.facts)
    }

    /// When not one of the runners that could take `l` (`eligible`) could hold it even idle, in
    /// resource mode: what each has of what it's short of ("small has 16 GiB of memory"), by name.
    /// `None` when one could, or one counts slots.
    fn oversized(&self, eligible: &[&Runner], l: &Leg) -> Option<Vec<String>> {
        let c = self.config();
        if !c.resources {
            return None;
        }
        let q = &l.input.request;
        let mut short = Vec::new();
        for r in eligible {
            let cap = r.input.capacity?;
            let mut has = Vec::new();
            if !within(q.cpu.get(), cap.cpu.get(), c.overcommit_cpu) {
                has.push(needs::cpus(cap.cpu));
            }
            if !within(q.mem, cap.mem, c.overcommit_mem) {
                has.push(format!("{} of memory", needs::show_size(cap.mem)));
            }
            if !within(q.disk, cap.disk, c.overcommit_disk) {
                has.push(format!("{} of disk", needs::show_size(cap.disk)));
            }
            if q.gpu > cap.gpu {
                has.push(needs::gpus(cap.gpu));
            }
            if has.is_empty() {
                return None;
            }
            short.push(format!("{} has {}", r.input.view.name, has.join(", ")));
        }
        short.sort();
        Some(short)
    }

    /// Gives a runner the best queued legs that fit it, until none does: from the first class in
    /// order with one, unless a class after it is short of its floor and may take room ahead of it
    /// (W05-R31, [`Self::ahead_of`]); in a class, the least standing of its tenant in the
    /// dimensions this runner offers, level by level (W05-R32, R33), then the tenant's own order
    /// (W05-R09's aged priority), then arrival.
    fn try_runner(&mut self, runner: &str, now: i64, out: &mut Vec<Command>) {
        self.drop_stale_reservations();
        // Whole gangs that couldn't start now, passed over as this runner picks on.
        let mut stuck: BTreeSet<String> = BTreeSet::new();
        loop {
            let Some(r) = self.runners.get(runner) else { return };
            let set = profile(&dims(&r.input, self.config().resources));
            let (bests, mut passed, gangs) = self.bests(r, set, &stuck);
            // A whole gang before the best leg of the first class with either starts whole, or is
            // passed over as an item with no room is, keeping room with backfilling (W05-R40).
            if let Some(c) = Class::ALL.into_iter().find(|c| bests.get(c.index()).is_some_and(Option::is_some) || gangs.get(c.index()).is_some_and(Option::is_some))
                && let Some(g) = gangs.get(c.index()).copied().flatten().map(str::to_owned)
            {
                if self.place_gang(&g, now, out) {
                    continue;
                }
                stuck.insert(g);
                if let Some(p) = passed.get_mut(c.index()) {
                    *p = true;
                }
                if self.cfg.backfill {
                    self.reserve(runner, passed, c, now, out);
                }
                continue;
            }
            let mut bests = Class::ALL.into_iter().zip(bests).filter_map(|(c, id)| id.map(|id| (c, id)));
            let Some((class, id)) = bests.next() else { return };
            let ahead = bests.filter(|(c, _)| self.short(*c)).find_map(|(c, id)| self.ahead_of(c, id, r, now).map(|(id, over)| (c, id, over)));
            let (class, id, over) = ahead.map_or_else(|| (class, id.clone(), None), |(c, id, over)| (c, id.clone(), over));
            // Before it takes an item while the first of that item's class, or of a class before
            // it, had no room here, those firsts keep room where it frees first (W05-R39); then
            // it picks again, held to them.
            if self.cfg.backfill && passed.iter().take(class.index() + 1).any(|p| *p) && self.reserve(runner, passed, class, now, out) {
                continue;
            }
            // One that would rather wait for a busy runner where it's warm holds out for it, and
            // then fits this one no more (W24-R18).
            if self.hold_for_warm(&id, runner, now, out) {
                continue;
            }
            self.assign(&id, runner, now, out);
            if let Some(left) = over {
                self.pace(&id, left, now);
            }
        }
    }

    /// Each class's best queued leg that fits a runner offering the set of dimensions `set`, by
    /// [`Class::index`]: the least (its leaf's standing, its tenant's own order, its arrival) over
    /// the signatures the runner may take, walked once, held to the reservations standing there
    /// (W05-R39). For each class, too, whether a leg before its best (or a leg at all, when it has
    /// none) was passed over for want of room: the runner would take it, but hasn't room for it;
    /// and the whole gang whose first member comes before its best, if one does, and isn't `stuck`
    /// (W05-R40). A gang's members are none of them its best: they go only with their gang.
    fn bests<'a>(&'a self, r: &Runner, set: usize, stuck: &BTreeSet<String>) -> ([Option<&'a String>; 5], [bool; 5], [Option<&'a str>; 5]) {
        type Key = ([u128; LEVELS], i128, u64);
        let mut best: [Option<(Key, &String)>; 5] = Default::default();
        let mut short: [Option<Key>; 5] = Default::default();
        let mut gang: [Option<(Key, &str)>; 5] = Default::default();
        let held = self.reserved.values().any(|k| k.on(&r.input.id));
        for s in self.sigs.values().filter(|s| s.eligible.contains(&r.input.id)) {
            for (class, level) in &s.queue {
                let (Some(b), Some(u), Some(w)) = (best.get_mut(class.index()), short.get_mut(class.index()), gang.get_mut(class.index())) else { continue };
                for (standing, k, q, id) in self.walk(*class, level, set) {
                    if b.as_ref().is_some_and(|(key, _)| (standing, k, q) >= *key) {
                        break;
                    }
                    let Some(l) = self.legs.get(id) else { continue };
                    // A gang's member goes only with its whole gang (W05-R40): the first member of a
                    // whole gang takes the gang's place.
                    if let Some(g) = &l.input.gang {
                        if w.is_none_or(|(key, _)| (standing, k, q) < key) && !stuck.contains(&g.id) && self.whole(&g.id) {
                            *w = Some(((standing, k, q), g.id.as_str()));
                        }
                        continue;
                    }
                    if self.fits(r, l) {
                        if held && !self.allowed(r, l) {
                            continue;
                        }
                        *b = Some(((standing, k, q), id));
                        break;
                    }
                    if u.is_none_or(|u| (standing, k, q) < u) && self.admits(r, l) && !self.capacity(r, l) {
                        *u = Some((standing, k, q));
                    }
                }
            }
        }
        let mut passed = [false; 5];
        for ((p, b), u) in passed.iter_mut().zip(&best).zip(&short) {
            *p = u.is_some_and(|u| b.as_ref().is_none_or(|(key, _)| u < *key));
        }
        // A whole gang counts where it comes before its class's best leg, or its class has none.
        let mut gangs: [Option<&str>; 5] = Default::default();
        for ((g, b), w) in gangs.iter_mut().zip(&best).zip(&gang) {
            *g = w.filter(|(key, _)| b.as_ref().is_none_or(|(bk, _)| key < bk)).map(|(_, id)| id);
        }
        (best.map(|b| b.map(|(_, id)| id)), passed, gangs)
    }

    /// Charges a leg's tenant for its dispatch to `runner` (W05-R32, R33): in its
    /// class's tree, at every node from its repository down, in each set of dimensions, its
    /// largest share there of what the fleet has (of what it holds on that runner) times its
    /// expected milliseconds over the node's weight ([`profile_costs`]), from where start-time
    /// fair queuing starts it under its parent; the parent's virtual time moves up to that start.
    /// What it charged is kept, to settle when its attempt ends ([`Self::settle_charge`]).
    fn charge(&mut self, id: &str, runner: &str, now: i64) {
        let (Some(l), Some(r)) = (self.legs.get(id), self.runners.get(runner)) else { return };
        let ms = l.input.cost.unwrap_or(self.config().default_cost);
        let holds = asks(l, r, self.config().resources);
        let shares = shares(&holds, &self.fleet);
        let (leaf, weights, class) = (l.flow.clone(), self.weights_of(&l.input), l.key.map_or(l.input.class, |k| k.0));
        let mut parent = "";
        for (node, weight) in nodes(&leaf).zip(weights) {
            let costs = profile_costs(&charges(&shares, ms, weight));
            let (vtimes, finish) = (self.vtimes.entry(class).or_default(), self.finish.entry(class).or_default());
            if !vtimes.contains_key(parent) {
                vtimes.insert(parent.to_owned(), [0; PROFILES]);
            }
            if !finish.contains_key(node) {
                finish.insert(node.to_owned(), [0; PROFILES]);
            }
            if let (Some(v), Some(f)) = (vtimes.get_mut(parent), finish.get_mut(node)) {
                for ((f, v), c) in f.iter_mut().zip(v.iter_mut()).zip(costs) {
                    if c == 0 {
                        continue;
                    }
                    let start = (*v).max(*f);
                    *f = start.saturating_add(c);
                    *v = (*v).max(start);
                }
            }
            parent = node;
        }
        let held = self.held.entry(class).or_insert([0; DIMS]);
        for (h, a) in held.iter_mut().zip(holds) {
            *h = h.saturating_add(u128::from(a));
        }
        // The class started a leg at this signature: it isn't passed over here (W05-R31).
        if let Some(s) = self.sigs.get_mut(&l.sig) {
            s.served.insert(class, now);
        }
        if let Some(l) = self.legs.get_mut(id) {
            l.charge = Some(Charge { leaf, weights, shares, ms, at: now, class, holds, paced: None });
        }
    }

    /// Settles what a leg's dispatch charged fair share to the time its attempt ran (W05-R10, R32;
    /// start-time fair queuing needn't know it beforehand, as DRFQ has it): each node's finish
    /// gives back what it was charged for the expected time and takes the time it ran instead. Its
    /// class holds what it held no more, and a floor's pace it moved is settled alike (W05-R31).
    fn settle_charge(&mut self, id: &str, now: i64) {
        let Some(c) = self.legs.get_mut(id).and_then(|l| l.charge.take()) else { return };
        if let Some(held) = self.held.get_mut(&c.class) {
            for (h, a) in held.iter_mut().zip(c.holds) {
                *h = h.saturating_sub(u128::from(a));
            }
        }
        let ran = Ms::try_from(now.saturating_sub(c.at).max(0)).unwrap_or(0);
        if let Some((took, left)) = c.paced {
            let used = Self::pace_ms(&c.shares, ran, left);
            if let Some(at) = self.paced.get_mut(&c.class) {
                *at = at.saturating_sub(took).saturating_add(used);
            }
        }
        for (node, weight) in nodes(&c.leaf).zip(c.weights) {
            let (charged, used) = (profile_costs(&charges(&c.shares, c.ms, weight)), profile_costs(&charges(&c.shares, ran, weight)));
            if let Some(f) = self.finish.get_mut(&c.class).and_then(|f| f.get_mut(node)) {
                for ((f, charged), used) in f.iter_mut().zip(charged).zip(used) {
                    *f = f.saturating_sub(charged).saturating_add(used);
                }
            }
        }
    }

    /// The fleet's capacity in each dimension: what its connected runners that aren't draining
    /// offer (docs/needs.md E2.1).
    fn refresh_fleet(&mut self) {
        let (mut fleet, resources) = ([0u64; DIMS], self.config().resources);
        for r in self.runners.values().filter(|r| r.connected && !r.draining) {
            for (f, o) in fleet.iter_mut().zip(offers(&r.input, resources)) {
                *f = f.saturating_add(o);
            }
        }
        self.fleet = fleet;
    }

    fn assign(&mut self, id: &str, runner: &str, now: i64, out: &mut Vec<Command>) {
        self.granted_over(id, runner);
        self.charge(id, runner, now);
        self.unqueue(id);
        let expected = self.legs.get(id).map(|l| self.expected_ms(l, runner));
        let Some(l) = self.legs.get_mut(id) else { return };
        l.state = State::Assigned { runner: runner.to_owned() };
        l.reason = None;
        l.hold = None;
        l.ends = expected.map(|ms| later(now, ms));
        l.last_runner = Some(runner.to_owned());
        l.started = Some(now);
        l.due = Some(Due::of(&l.input, now));
        // Its grant (W05-R34): the next fence of this epoch, living a lease unless renewed.
        let expires = self.cfg.lease.map(|ms| later(now, ms));
        self.fences += 1;
        let fence = Fence { epoch: self.epoch, n: self.fences };
        (l.fence, l.expires) = (fence, expires);
        if let Some(at) = expires {
            self.leases.insert((at, id.to_owned()));
        }
        self.demands.started(id, now);
        let Some(l) = self.legs.get_mut(id) else { return };
        let (attempt, request, repo, lease_loss) = (l.attempt, l.input.request, l.input.repo.clone(), l.input.on_lease_loss);
        let req = l.req.clone();
        let (image, docker) = (req.image.clone(), req.docker.clone());
        *self.in_progress.entry(repo).or_default() += 1;
        self.admit.started(id, now);
        if self.cadenced.remove(id) {
            self.drained.retain(|_, owner| owner.as_str() != id);
        }
        let mut executor = None;
        if let Some(r) = self.runners.get_mut(runner) {
            r.running.push(id.to_owned());
            add(&mut r.used, &request);
            executor = req.executors.iter().find(|e| super::routing::usable(&req, &r.input.view.facts, e)).cloned();
        }
        // Its pool's idle executor there, which it alone uses.
        let own = self.legs.get(id).and_then(|l| self.own_leg(l)).map(str::to_owned);
        let slot = req.pool.as_ref().and_then(|p| self.executors.values_mut().find(|e| e.pool == *p && e.runner == runner && e.state == ExecState::Idle && e.leg == own)).map(|e| {
            e.state = ExecState::Busy { leg: id.to_owned() };
            e.since = now;
            e.id.clone()
        });
        out.push(Command::Assign { leg: id.to_owned(), runner: runner.to_owned(), attempt, executor, image, docker, slot, fence, expires, lease_loss });
    }

    fn release(&mut self, id: &str, now: i64) -> Option<String> {
        self.settle_charge(id, now);
        // Its grant is over: it expires no more (W05-R34), and has no ends (W05-R38).
        let expired = self.legs.get_mut(id).and_then(|l| {
            l.due = None;
            l.expires.take()
        });
        if let Some(at) = expired {
            self.leases.remove(&(at, id.to_owned()));
        }
        let l = self.legs.get(id)?;
        let State::Assigned { runner } = &l.state else { return None };
        let (runner, request, repo) = (runner.clone(), l.input.request, l.input.repo.clone());
        if let Some(n) = self.in_progress.get_mut(&repo) {
            *n = n.saturating_sub(1);
        }
        if let Some(r) = self.runners.get_mut(&runner) {
            r.running.retain(|x| x != id);
            sub(&mut r.used, &request);
        }
        Some(runner)
    }

    /// The executor a leg ran in took its one leg: it's discarded. Its host removed it as the leg
    /// ended; one the leg never reached (refused, failed before it started) goes now.
    fn discard_used(&mut self, id: &str, out: &mut Vec<Command>) {
        // And one made for it that it never got to (cancelled while its pod started).
        let used: Vec<(String, String)> =
            self.executors.values().filter(|e| e.state == ExecState::Busy { leg: id.to_owned() } || e.leg.as_deref() == Some(id)).map(|e| (e.id.clone(), e.runner.clone())).collect();
        for (e, runner) in used {
            self.drop_executor(&e);
            out.push(Command::StopExecutor { executor: e, runner });
        }
    }

    fn finished(&mut self, id: &str, attempt: u32, outcome: Outcome, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        if l.attempt != attempt || !(l.state == State::Queued || matches!(l.state, State::Assigned { .. })) {
            return;
        }
        // A gang's member is tried again only with its whole gang (W05-R40): lost while it ran,
        // before any of its members' attempts ended, it takes its gang with it; else it ends as it
        // ended, and once one has, none is tried again alone.
        if let Some(g) = l.input.gang.as_ref().map(|g| g.id.clone())
            && !l.gone
        {
            let lost = matches!(&outcome, Outcome::Failed(f) if f.class == FailureClass::RunnerLost) && matches!(l.state, State::Assigned { .. });
            if lost && self.gangs.get(&g).is_some_and(|c| !c.ended) {
                return self.gang_lost(&g, id, attempt, outcome, now, out);
            }
            if let Some(c) = self.gangs.get_mut(&g) {
                c.ended = true;
            }
            return self.finish(id, attempt, outcome, false, now, out);
        }
        self.finish(id, attempt, outcome, true, now, out);
    }

    /// An attempt's end ([`Self::finished`]'s): it's settled, or tried again as its retry policy
    /// says when `retry`.
    fn finish(&mut self, id: &str, attempt: u32, outcome: Outcome, retry: bool, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        // The attempt that runs ends, or one that ended before it ran (its executor couldn't be
        // made while it was queued, W13-R07): an end of any other changes nothing (W05-R34).
        let queued = l.state == State::Queued;
        if l.attempt != attempt || !(queued || matches!(l.state, State::Assigned { .. })) {
            return;
        }
        if l.gone {
            self.drop_gone(id, now, out);
            return;
        }
        // A stop item lost with its grant: its next attempt waits the lock-delay past the grant's
        // expiry, by which its runner has stopped it by its own clock (W05-R34).
        let lock = match &outcome {
            Outcome::Failed(f) if f.class == FailureClass::RunnerLost && l.input.on_lease_loss == LeaseLoss::Stop => Some(l.expires.unwrap_or(now).max(now).saturating_add(ms_i64(self.cfg.lock_delay))),
            _ => None,
        };
        // Out of its queue before it's queued again or settled, so it's queued once.
        if queued {
            self.unqueue(id);
        }
        let runner = self.release(id, now);
        self.stopped(id);
        self.discard_used(id, out);
        // Cancelled while it ran: a failure now (killed, its runner gone) is the cancel's doing.
        if let Some(why) = self.legs.get(id).and_then(|l| l.cancelling.clone()) {
            let (outcome, reason) = match outcome {
                Outcome::Failed(_) => (Outcome::Cancelled, Some(why)),
                o => (o, None),
            };
            self.settle(id, outcome, reason, now, out);
            if let Some(r) = runner {
                self.try_runner(&r, now, out);
            }
            self.scale_all(now, out);
            self.review_admission(now, out);
            return;
        }
        let decision = match &outcome {
            Outcome::Failed(f) if retry => retry::decide(self.legs.get(id).and_then(|l| l.input.retry.as_ref()), attempt, f),
            _ => None,
        };
        match decision {
            Some(d) => {
                let at = (now + i64::try_from(d.delay_ms).unwrap_or(i64::MAX / 2)).max(lock.unwrap_or(now));
                if let Some(l) = self.legs.get_mut(id) {
                    l.attempt += 1;
                    l.state = State::Retrying { at };
                    l.avoid = None;
                    l.pin = None;
                    // A new attempt may wait for a warm runner again (W24-R18).
                    l.held = false;
                    match d.runner {
                        RunnerChoice::Different => l.avoid = runner.clone().map(|r| (r, Off::Different)),
                        RunnerChoice::Another => l.avoid = runner.clone().map(|r| (r, Off::Lost)),
                        RunnerChoice::Same => l.pin = runner.clone(),
                        RunnerChoice::Any | RunnerChoice::Default => {}
                    }
                    out.push(Command::Retry { leg: id.to_owned(), attempt: l.attempt, at });
                }
                if at <= now {
                    self.enqueue(id, now, out);
                } else {
                    self.timers.entry(at).or_default().push(id.to_owned());
                    out.push(Command::Timer { at });
                    self.set_reason(id, WaitReason::Retry { at }, out);
                }
            }
            None => self.settle(id, outcome, None, now, out),
        }
        if let Some(r) = runner {
            self.try_runner(&r, now, out);
        }
        self.scale_all(now, out);
        self.review_admission(now, out);
    }

    fn settle(&mut self, id: &str, outcome: Outcome, reason: Option<String>, now: i64, out: &mut Vec<Command>) {
        // An executor made for it goes with it (a pod still starting for a cancelled leg).
        self.discard_used(id, out);
        let passed = match &outcome {
            Outcome::Success => Some(true),
            Outcome::Failed(_) => Some(false),
            Outcome::Cancelled => None,
        };
        let gone = self.legs.remove(id);
        self.left_gang(id, gone.as_ref());
        let group = gone.and_then(|l| l.input.group);
        self.decided.remove(id);
        out.push(Command::Settled { leg: id.to_owned(), outcome: outcome.clone(), reason });
        if let Some((key, _, _)) = group {
            for a in self.groups.leave(&key, &format!("leg:{id}")) {
                self.group_action(a, now, out);
            }
        }
        self.left_admission(id, passed, now, out);
        self.answer(id, &outcome, now, out);
    }

    /// A leg is gone from the scheduler: admission hears how it ended (W28-R68's window learns
    /// from it), and lets go what it held back for it.
    fn left_admission(&mut self, id: &str, passed: Option<bool>, now: i64, out: &mut Vec<Command>) {
        if self.cadenced.remove(id) {
            self.drained.retain(|_, owner| owner.as_str() != id);
        }
        let ds = self.admit.ended(id, passed, now);
        self.admitted(ds, now, out);
    }

    // ------------------------------------------------------------------ demands (W28-R40 to R45)

    /// Its attempt ended: until the next starts, it has no time left to count (W28-R43).
    fn stopped(&mut self, id: &str) {
        if let Some(l) = self.legs.get_mut(id) {
            l.started = None;
        }
        self.demands.stopped(id);
    }

    /// Says a leg's decision in demands, once (W28-R45).
    fn decide(&mut self, leg: &str, d: Decision, out: &mut Vec<Command>) {
        if self.decided.get(leg) != Some(&d) {
            self.decided.insert(leg.to_owned(), d.clone());
            out.push(Command::Demand { leg: leg.to_owned(), decision: d });
        }
    }

    /// `run`'s demand arrives on `lane` and withdraws the older ones there (W28-R40); what answered
    /// them is decided again (W28-R43).
    fn withdraw(&mut self, run: &str, lane: String, now: i64, out: &mut Vec<Command>) {
        let mut d = self.demands.demand(run).cloned().unwrap_or_else(|| Demand { run: run.to_owned(), at: now, ..Demand::default() });
        d.lane = lane;
        let withdrawn = self.demands.arrive(d, true);
        for exec in self.demands.affected(&withdrawn) {
            self.reconsider(&exec, false, now, out);
        }
    }

    /// The seconds an execution is expected to take still: its cost (history, W19) less its
    /// attempt's time so far, to the nearest second (half away from zero); `None` while no attempt
    /// of it runs.
    fn left(&self, id: &str, now: i64) -> Option<i64> {
        let l = self.legs.get(id)?;
        let started = l.started?;
        let cost = l.input.cost.unwrap_or(self.config().default_cost);
        let left = i64::try_from(cost).unwrap_or(i64::MAX).saturating_sub(now.saturating_sub(started));
        Some(left.saturating_add(if left < 0 { -500 } else { 500 }) / 1000)
    }

    /// Decides what becomes of an execution (W28-R43), and carries it out: kept at the highest
    /// priority its live demands give, dropped, run on, finished, or cancelled through the
    /// cancelling path.
    fn reconsider(&mut self, exec: &str, contended: bool, now: i64, out: &mut Vec<Command>) -> Option<Fate> {
        let left = self.left(exec, now);
        let finish_within = self.legs.get(exec).and_then(|l| l.input.finish_within).unwrap_or(self.config().finish_within);
        let fate = self.demands.fate(exec, left, contended, finish_within)?;
        self.decide(exec, Decision::Fate(fate.clone()), out);
        match &fate {
            Fate::Kept { priority, .. } => self.rekey(exec, *priority),
            Fate::Drop { .. } => self.cancel(exec, fate.why(), now, out),
            Fate::Cancel { .. } => {
                // What's being cancelled is over as far as its work goes: no one adopts it now.
                for (leg, next) in self.demands.ended(exec, End::Cancelled) {
                    self.next_for(&leg, next, None, now, out);
                }
                self.cancel(exec, fate.why(), now, out);
            }
            Fate::Run { .. } | Fate::Finish { .. } => {}
        }
        Some(fate)
    }

    /// A queued execution takes `priority`, the highest its live demands give (W28-R42, R43).
    fn rekey(&mut self, exec: &str, priority: u8) {
        if !self.legs.get(exec).is_some_and(|l| l.state == State::Queued && l.input.priority != priority) {
            return;
        }
        self.reprioritize(exec, priority);
    }

    /// A queued leg takes another priority, keeping its class and arrival: its place in its
    /// tenant's own order moves with it.
    fn reprioritize(&mut self, id: &str, priority: u8) {
        self.unqueue(id);
        let aging = self.config().aging_per_minute;
        if let Some(l) = self.legs.get_mut(id) {
            l.input.priority = priority;
            let k = own_order(priority, aging, l.queued_at);
            l.key = l.key.map(|(class, _, q)| (class, k, q));
        }
        self.insert_queue(id);
    }

    /// `id` adopted `exec` of `run` (W28-R42): it waits for it, holding no runner, slot or quota,
    /// and `exec` takes its priority if that's higher. Wanted again, `exec` says so.
    fn adopted(&mut self, id: &str, exec: String, run: String, now: i64, out: &mut Vec<Command>) {
        self.unqueue(id);
        if let Some(l) = self.legs.get_mut(id) {
            l.state = State::Adopted { exec: exec.clone() };
            l.key = None;
        }
        self.set_reason(id, WaitReason::Adopted { leg: exec.clone(), run }, out);
        if matches!(self.decided.get(&exec), Some(Decision::Fate(Fate::Run { .. } | Fate::Finish { .. }))) {
            self.reconsider(&exec, false, now, out);
        } else if let Some(p) = self.demands.priority(&exec) {
            self.rekey(&exec, p);
        }
    }

    /// A leg left (settled, gone): if it executed work, its adopters take its result over or carry
    /// on without it (W28-R44); if it adopted, its execution may be wanted no more (W28-R43).
    fn answer(&mut self, id: &str, outcome: &Outcome, now: i64, out: &mut Vec<Command>) {
        if self.demands.is_execution(id) {
            let end = match outcome {
                Outcome::Success => End::Success,
                Outcome::Failed(f) => End::Failed(f.class),
                Outcome::Cancelled => End::Cancelled,
            };
            for (leg, next) in self.demands.ended(id, end) {
                self.next_for(&leg, next, Some(outcome), now, out);
            }
        } else if let Some(exec) = self.demands.forget(id) {
            self.reconsider(&exec, false, now, out);
        }
    }

    /// What an adopter does once its execution ended (as `outcome`), or its check came back
    /// (W28-R44).
    fn next_for(&mut self, leg: &str, next: Next, outcome: Option<&Outcome>, now: i64, out: &mut Vec<Command>) {
        if !self.legs.contains_key(leg) {
            return;
        }
        self.decide(leg, Decision::Next(next.clone()), out);
        match next {
            Next::Reuse { from } => self.reused(leg, from, outcome.cloned().unwrap_or(Outcome::Cancelled), now, out),
            Next::Check { from } => {
                if let Some(l) = self.legs.get_mut(leg) {
                    l.state = State::Checking { from, outcome: outcome.cloned().unwrap_or(Outcome::Cancelled) };
                }
            }
            Next::Execute => {
                if let Some(l) = self.legs.get_mut(leg) {
                    l.state = State::Pending;
                }
                self.enqueue(leg, now, out);
            }
            Next::Adopt { exec, run } => self.adopted(leg, exec, run, now, out),
            Next::Drop => self.settle(leg, Outcome::Cancelled, Some(Next::Drop.why()), now, out),
        }
    }

    /// An adopter takes its execution's result over: it's done, reused from `from`, as `from`
    /// ended (W28-R44).
    fn reused(&mut self, id: &str, from: String, outcome: Outcome, now: i64, out: &mut Vec<Command>) {
        self.discard_used(id, out);
        let passed = match &outcome {
            Outcome::Success => Some(true),
            Outcome::Failed(_) => Some(false),
            Outcome::Cancelled => None,
        };
        let gone = self.legs.remove(id);
        self.left_gang(id, gone.as_ref());
        let group = gone.and_then(|l| l.input.group);
        self.decided.remove(id);
        out.push(Command::Adopted { leg: id.to_owned(), from, outcome });
        if let Some((key, _, _)) = group {
            for a in self.groups.leave(&key, &format!("leg:{id}")) {
                self.group_action(a, now, out);
            }
        }
        self.left_admission(id, passed, now, out);
    }

    /// Live work that can't be placed contends for the capacity of running work no live demand
    /// wants (W28-R43): of that on a runner it could use, or in its pool, the one with the most
    /// time left is cancelled, unless it finishes within finish_within, seeding trusted entries.
    fn contend(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        if !self.demands.live(&l.input.run) {
            return;
        }
        let pool = l.req.pool.clone();
        let eligible = self.sigs.get(&l.sig).map(|s| s.eligible.clone()).unwrap_or_default();
        let mut unwanted: Vec<(i64, String)> = self
            .demands
            .unwanted_running()
            .into_iter()
            .filter_map(|x| {
                let u = self.legs.get(&x)?;
                let State::Assigned { runner } = &u.state else { return None };
                let could = match (&pool, &u.req.pool) {
                    (Some(p), Some(q)) => p == q,
                    (None, None) => eligible.contains(runner),
                    _ => false,
                };
                (could && u.cancelling.is_none()).then(|| (self.left(&x, now).unwrap_or(i64::MAX), x))
            })
            .collect();
        unwanted.sort_by(|a, b| b.cmp(a));
        for (_, x) in unwanted {
            if matches!(self.reconsider(&x, true, now, out), Some(Fate::Cancel { .. })) {
                return;
            }
        }
    }

    fn gone(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get_mut(id) else { return };
        // Running: its runner stops it, and its slot is free once the runner says it ended.
        if matches!(l.state, State::Assigned { .. }) {
            l.gone = true;
            out.push(Command::Cancel { leg: id.to_owned(), attempt: l.attempt, reason: "its repository was deleted".into() });
            return;
        }
        self.unqueue(id);
        self.drop_gone(id, now, out);
    }

    /// A leg gone with its repository, off its runner and out of the scheduler: nothing settled.
    /// Its adopters, in other runs, carry on without it (W28-R44).
    fn drop_gone(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let runner = self.release(id, now);
        self.discard_used(id, out);
        let gone = self.legs.remove(id);
        self.left_gang(id, gone.as_ref());
        let group = gone.and_then(|l| l.input.group);
        self.decided.remove(id);
        if let Some((key, _, _)) = group {
            for a in self.groups.leave(&key, &format!("leg:{id}")) {
                self.group_action(a, now, out);
            }
        }
        self.left_admission(id, None, now, out);
        self.answer(id, &Outcome::Cancelled, now, out);
        if let Some(r) = runner {
            self.try_runner(&r, now, out);
        }
        self.review_admission(now, out);
    }

    fn cancel(&mut self, id: &str, reason: String, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get(id) else { return };
        match l.state.clone() {
            State::Assigned { .. } => {
                out.push(Command::Cancel { leg: id.to_owned(), attempt: l.attempt, reason: reason.clone() });
                if let Some(l) = self.legs.get_mut(id) {
                    l.cancelling = Some(reason);
                }
            }
            State::Queued => {
                self.unqueue(id);
                self.settle(id, Outcome::Cancelled, Some(reason), now, out);
            }
            // An adopter stops waiting: what it adopted may be wanted no more (W28-R43).
            State::Pending | State::Held | State::Retrying { .. } | State::Adopted { .. } | State::Checking { .. } => self.settle(id, Outcome::Cancelled, Some(reason), now, out),
            State::Done => {}
        }
    }

    fn runner_up(&mut self, r: RunnerIn, now: i64, out: &mut Vec<Command>) {
        let id = r.id.clone();
        // What it offers may have changed: the reservations on it are worked out again when next
        // they matter (W05-R39).
        self.reserved.retain(|_, k| !k.on(&id));
        // A hosted executor's own runner: the executor is ready, there (W13-R07). One the
        // scheduler doesn't know is told to go.
        if let Some(e) = r.executor.clone() {
            match self.executors.get_mut(&e) {
                Some(x) if x.state == ExecState::Starting => {
                    x.runner = id.clone();
                    x.state = ExecState::Idle;
                    x.since = now;
                    let pool = x.pool.clone();
                    self.backoff.remove(&(pool.clone(), String::new()));
                    self.idle_timer(&pool, now, out);
                }
                Some(_) => {}
                // One from before a restart, or stopped since: it goes. An ephemeral runner that
                // is no executor's (`""`, a v1 job's pod) is left alone, and takes nothing.
                None if !e.is_empty() => out.push(Command::StopExecutor { executor: e, runner: id.clone() }),
                None => {}
            }
        }
        let (running, used) = self.runners.get(&id).map(|x| (x.running.clone(), x.used)).unwrap_or_default();
        let draining = r.draining;
        self.runners.insert(id.clone(), Runner { input: r, running, used, connected: true, draining });
        self.refresh_fleet();
        self.refresh_eligibility(&id);
        self.release_holds(now, out);
        self.try_avoided(now, out);
        self.try_runner(&id, now, out);
        self.scale_all(now, out);
        self.refresh_reasons(out);
        self.review_admission(now, out);
    }

    fn refresh_eligibility(&mut self, runner: &str) {
        let Some(r) = self.runners.get(runner) else { return };
        let ok: Vec<(String, bool)> = self.sigs.iter().map(|(k, s)| (k.clone(), self.eligible_for(&s.req, r))).collect();
        for (k, yes) in ok {
            if let Some(s) = self.sigs.get_mut(&k) {
                if yes {
                    s.eligible.insert(runner.to_owned());
                } else {
                    s.eligible.remove(runner);
                }
            }
        }
    }

    /// Whether a runner may take legs of `req`: connected, and [`Scheduler::routable`].
    fn eligible_for(&self, req: &Requirement, r: &Runner) -> bool {
        r.connected && self.routable(req, r)
    }

    /// Whether a runner, connected or not, could take legs of `req`: a hosted executor's own
    /// runner only its pool's (W13-R07), and no other runner those; the rest as routing decides.
    fn routable(&self, req: &Requirement, r: &Runner) -> bool {
        let hosted = req.pool.as_deref().and_then(|p| self.pool(p)).is_some_and(|p| p.hosted);
        match r.input.executor.as_deref() {
            Some(e) => hosted && self.executors.get(e).is_some_and(|x| Some(x.pool.as_str()) == req.pool.as_deref()),
            None => !hosted && routing::eligible(req, &r.input.view),
        }
    }

    fn runner_lost(&mut self, runner: &str, now: i64, out: &mut Vec<Command>) {
        let legs = match self.runners.get_mut(runner) {
            Some(r) => {
                r.connected = false;
                r.running.clone()
            }
            None => return,
        };
        self.refresh_fleet();
        self.refresh_eligibility(runner);
        self.release_holds(now, out);
        // Its pools' executors went with it.
        let gone: Vec<String> = self.executors.values().filter(|e| e.runner == runner && !matches!(e.state, ExecState::Busy { .. })).map(|e| e.id.clone()).collect();
        for e in gone {
            self.drop_executor(&e);
        }
        // The legs it ran come back to the queue together (W05-R16's retry). Then every leg whose
        // place changed goes, in queue order, to the best runner it has room on (W05-R04, R23):
        // those, and the legs that stayed off another runner only while this one could take them.
        // One lost here takes no runner before a leg ahead of it in the queue, nor one that leg
        // would rather have.
        self.requeued = Some(Vec::new());
        for leg in legs {
            let Some((attempt, fence, gone)) = self.legs.get(&leg).map(|l| (l.attempt, l.fence, l.gone)) else { continue };
            // Its grant is lost with its runner (W05-R34).
            if !gone {
                out.push(Command::Lost { leg: leg.clone(), attempt, runner: runner.to_owned(), fence, why: Loss::RunnerLost });
            }
            self.finished(&leg, attempt, Outcome::Failed(Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() }), now, out);
        }
        let mut changed: BTreeSet<String> = self.requeued.take().unwrap_or_default().into_iter().collect();
        changed.extend(self.legs.iter().filter(|(_, l)| l.state == State::Queued && l.avoid.as_ref().is_some_and(|(_, off)| *off == Off::Lost)).map(|(id, _)| id.clone()));
        // The next in queue order goes, its place taken again each time: a leg placed moves its
        // flow's standing, and so the order of the rest (docs/needs.md E2.1).
        loop {
            let next = changed.iter().filter_map(|id| self.legs.get(id).filter(|l| l.state == State::Queued && l.hold.is_none()).and_then(|l| self.pick_key(l)).map(|key| (key, id.clone()))).min();
            let Some((_, id)) = next else { break };
            changed.remove(&id);
            self.offer(&id, true, now, out);
        }
        self.scale_all(now, out);
        self.refresh_reasons(out);
        self.review_admission(now, out);
    }

    fn rules_changed(&mut self, rules: Vec<Rule>, now: i64, out: &mut Vec<Command>) {
        self.cfg.rules = rules;
        // Where each item may run changed: reservations are worked out again (W05-R39).
        self.reserved.clear();
        let queued: Vec<String> = self.legs.iter().filter(|(_, l)| l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in &queued {
            self.unqueue(id);
        }
        let ids: Vec<String> = self.legs.keys().cloned().collect();
        for id in ids {
            let Some(input) = self.legs.get(&id).map(|l| l.input.clone()) else { continue };
            let mut req = self.requirement(&input);
            req.prefer.extend(input.prefer.iter().cloned());
            let sig = req.signature();
            self.ensure_sig(&sig, &req);
            if let Some(l) = self.legs.get_mut(&id) {
                l.req = req;
                l.sig = sig;
            }
        }
        for id in &queued {
            self.insert_queue(id);
        }
        let runners: Vec<String> = self.runners.keys().cloned().collect();
        for r in &runners {
            self.refresh_eligibility(r);
        }
        self.release_holds(now, out);
        for r in runners {
            self.try_runner(&r, now, out);
        }
        self.refresh_reasons(out);
    }

    fn tick(&mut self, now: i64, out: &mut Vec<Command>) {
        let due: Vec<i64> = self.timers.range(..=now).map(|(k, _)| *k).collect();
        for at in due {
            for id in self.timers.remove(&at).unwrap_or_default() {
                let (retrying, waited) = self.legs.get(&id).map_or((false, false), |l| (matches!(l.state, State::Retrying { .. }), l.hold.as_ref().is_some_and(|h| h.until <= now)));
                if retrying {
                    self.enqueue(&id, now, out);
                } else if waited {
                    // Its wait for a warm runner is up (W24-R18).
                    self.unhold(&id, now, out);
                }
            }
        }
        self.reap_idle(now, out);
        self.scale_all(now, out);
        self.review_admission(now, out);
    }

    // ------------------------------------------------------------------ warm placement (W24-R18)

    /// What a runner holds warm now. Legs already placed stay; the next decisions use it.
    fn warm_changed(&mut self, runner: String, held: WarmHeld) {
        if held.workspaces.is_empty() && held.bandwidth.is_none() {
            self.warm.remove(&runner);
        } else {
            self.warm.insert(runner, held);
        }
    }

    /// A leg's `warm` and `affinity` terms, each with its weight and the workspaces it looks at:
    /// `warm`, its lane's (its job's when it has no lane); `affinity: { job }`, that job's in its
    /// repository and workflow (W05-R23's affinity, backed by real workspaces).
    fn terms(l: &Leg) -> Vec<(i64, Wants<'_>)> {
        let mine = match &l.input.warm_lane {
            Some(lane) => Wants::Lane(lane),
            None => Wants::Job { repo: &l.input.repo, workflow: &l.input.workflow_file, job: &l.input.route.job },
        };
        let mut out = Vec::new();
        for p in &l.req.prefer {
            let w = units::milli_of(p.weight.unwrap_or(1.0));
            if p.warm {
                out.push((w, mine));
            }
            if let Some(a) = &p.affinity {
                out.push((w, Wants::Job { repo: &l.input.repo, workflow: &l.input.workflow_file, job: &a.job }));
            }
        }
        out
    }

    /// What a leg's terms look at together: on each runner, the workspace that would finish it
    /// first.
    fn wants(l: &Leg) -> Vec<Wants<'_>> {
        let mut out: Vec<Wants<'_>> = Vec::new();
        for (_, w) in Self::terms(l) {
            if !out.contains(&w) {
                out.push(w);
            }
        }
        out
    }

    /// A leg's cold duration: as history has it, else its cost, else the default.
    fn cold_ms(&self, l: &Leg) -> Ms {
        l.input.cold.or(l.input.cost).unwrap_or(self.cfg.default_cost)
    }

    /// The share of what a busy warm runner saves that a leg may wait for it: the most its terms
    /// give (`patience:`, else the server's), 0 without terms.
    fn patience(&self, l: &Leg) -> Ppm {
        l.req.prefer.iter().filter(|p| p.warm || p.affinity.is_some()).map(|p| p.patience.map_or(self.cfg.warm.patience, units::ppm_of)).max().unwrap_or(0)
    }

    /// A leg's estimate on a runner, from the workspaces `wants` names there: of its lane's, those
    /// made with the toolchain it would run with there.
    fn estimate(&self, l: &Leg, runner: &str, wants: &[Wants]) -> warm::Estimate {
        let toolchain = self.runners.get(runner).map(|r| Self::toolchain(l, r));
        warm::estimate(self.warm.get(runner), wants, self.cold_ms(l), &self.cfg.warm, toolchain.as_deref())
    }

    /// The toolchain a leg would run with on a runner (W24-R11): the first executor its rules
    /// allow that the runner has (the host where they name none), as an assignment there would
    /// carry it, that executor's image where it runs one, and the job's container.
    fn toolchain(l: &Leg, r: &Runner) -> String {
        let executor = l.req.executors.iter().find(|e| routing::usable(&l.req, &r.input.view.facts, e)).map_or("host", String::as_str);
        let image = matches!(executor, "docker" | "firecracker").then_some(l.req.image.as_deref()).flatten();
        crate::work::slots::toolchain(executor, image, l.input.container.as_deref())
    }

    /// How long a leg is expected to take on a runner, for when that runner will have room again:
    /// its estimate there from its lane and terms, else its cold duration.
    fn expected_ms(&self, l: &Leg, runner: &str) -> Ms {
        let mut wants = Self::wants(l);
        if let Some(lane) = &l.input.warm_lane
            && !wants.contains(&Wants::Lane(lane))
        {
            wants.push(Wants::Lane(lane));
        }
        if wants.is_empty() { self.cold_ms(l) } else { self.estimate(l, runner, &wants).busy_ms() }
    }

    /// When a busy runner is expected to have room (unix ms): as the first leg in its own slots
    /// is expected to end, and not before `now`. `None` when it runs none.
    fn free_at(&self, r: &Runner, now: i64) -> Option<i64> {
        r.running.iter().filter_map(|id| self.legs.get(id)).filter(|l| l.req.pool.is_none()).map(|l| l.ends.unwrap_or(now)).min().map(|t| t.max(now))
    }

    /// How long a leg is expected to wait for room on a runner that would take it: none with room
    /// now; `None` for one that wouldn't take it, or busy with no end in view.
    fn wait_ms(&self, l: &Leg, r: &Runner, now: i64) -> Option<Ms> {
        if !self.admits(r, l) || !self.sigs.get(&l.sig).is_some_and(|s| s.eligible.contains(&r.input.id)) {
            return None;
        }
        if self.capacity(r, l) {
            return Some(0);
        }
        self.free_at(r, now).map(|t| ms_until(now, t))
    }

    /// Each runner's preference score for a leg (W05-R23): its fact and label terms
    /// ([`routing::scores`]), then its `warm` and `affinity` terms, each its weight × how soon it's
    /// expected to finish there, normalized across `runners` (W24-R18). With `now`, a busy
    /// runner's expected wait counts; without, every runner has room. Empty without preferences.
    fn scores(&self, l: &Leg, runners: &[&Runner], now: Option<i64>) -> Vec<i64> {
        let views: Vec<&RunnerView> = runners.iter().map(|r| &r.input.view).collect();
        let mut total = routing::scores(&l.req.prefer, &views);
        for (weight, wants) in Self::terms(l) {
            let finish: Vec<Option<Ms>> = runners
                .iter()
                .map(|r| {
                    let wait = match now {
                        Some(now) => self.wait_ms(l, r, now)?,
                        None => 0,
                    };
                    Some(wait.saturating_add(self.estimate(l, &r.input.id, std::slice::from_ref(&wants)).busy_ms()))
                })
                .collect();
            for (t, v) in total.iter_mut().zip(warm::sooner(&finish)) {
                *t = t.saturating_add(weight.saturating_mul(i64::from(v)));
            }
        }
        total
    }

    /// A leg's warm side of one runner, for explanations.
    fn seen(&self, l: &Leg, r: &Runner, wants: &[Wants], now: i64) -> Warmth {
        let estimate = self.estimate(l, &r.input.id, wants);
        let wait_ms = self.wait_ms(l, r, now).unwrap_or(0);
        Warmth { finish_s: units::secs_or_never(wait_ms.saturating_add(estimate.busy_ms())), wait_s: units::secs(wait_ms), estimate }
    }

    /// Patience, as `offered` would take a leg now: the busy runner it waits for instead, if any
    /// ([`warm::patience`]). Of those that would take it once they have room, and where it would
    /// take less time than on any runner with room now, the one where it's expected to finish
    /// first, wait included; never a pool's leg, nor twice in an attempt.
    fn warm_hold(&self, id: &str, offered: &str, now: i64) -> Option<Hold> {
        let l = self.legs.get(id)?;
        let share = self.patience(l);
        if l.held || l.hold.is_some() || l.req.pool.is_some() || share == 0 {
            return None;
        }
        let wants = Self::wants(l);
        let here = self.estimate(l, offered, &wants).busy_ms();
        let eligible: Vec<&Runner> = self.sigs.get(&l.sig)?.eligible.iter().filter_map(|r| self.runners.get(r)).collect();
        let soonest_free = eligible.iter().filter(|r| self.room(r, l)).map(|r| self.estimate(l, &r.input.id, &wants).busy_ms()).fold(here, Ms::min);
        let mut best: Option<(Ms, Hold)> = None;
        for r in eligible.into_iter().filter(|r| r.input.id != offered) {
            if !self.admits(r, l) || self.capacity(r, l) {
                continue;
            }
            let Some(free) = self.free_at(r, now) else { continue };
            let wait = ms_until(now, free);
            let there = self.estimate(l, &r.input.id, &wants).busy_ms();
            if there >= soonest_free {
                continue;
            }
            let Some((saves_s, until)) = warm::patience(here, there, wait, share, now) else { continue };
            let finish = wait.saturating_add(there);
            if best.as_ref().is_none_or(|(f, _)| finish < *f) {
                best = Some((finish, Hold { runner: r.input.id.clone(), name: r.input.view.name.clone(), saves_s, until, there_ms: there }));
            }
        }
        best.map(|(_, h)| h)
    }

    /// Holds a leg out for a busy warm runner rather than start on `offered` now, when its
    /// patience says so: its wait reason says for which runner, what it saves and until when, and
    /// a timer ends the wait then. Whether it holds.
    fn hold_for_warm(&mut self, id: &str, offered: &str, now: i64, out: &mut Vec<Command>) -> bool {
        let Some(h) = self.warm_hold(id, offered, now) else { return false };
        let reason = WaitReason::Warm { runner: h.name.clone(), saves_s: h.saves_s, until: h.until };
        self.timers.entry(h.until).or_default().push(id.to_owned());
        out.push(Command::Timer { at: h.until });
        if let Some(l) = self.legs.get_mut(id) {
            l.held = true;
            l.hold = Some(h);
        }
        self.set_reason(id, reason, out);
        true
    }

    /// A leg's wait for a warm runner ends (its time is up, or that runner can't take it): it goes
    /// where it can now, as any queued leg, and doesn't wait for one again in this attempt.
    fn unhold(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(l) = self.legs.get_mut(id) else { return };
        if l.hold.take().is_none() || l.state != State::Queued {
            return;
        }
        for r in self.candidates(id) {
            if !self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
                break;
            }
            self.try_runner(&r, now, out);
        }
        if self.legs.get(id).is_some_and(|l| l.state == State::Queued) {
            let reason = self.wait_reason(id);
            self.set_reason(id, reason, out);
        }
    }

    /// Ends the waits for warm runners that can't take their legs any more: lost, draining, or no
    /// longer eligible. In queue order, so the first in line goes first.
    fn release_holds(&mut self, now: i64, out: &mut Vec<Command>) {
        let mut ended: Vec<(Key, String)> = self
            .legs
            .iter()
            .filter(|(_, l)| l.hold.as_ref().is_some_and(|h| !self.sigs.get(&l.sig).is_some_and(|s| s.eligible.contains(&h.runner)) || !self.runners.get(&h.runner).is_some_and(|r| self.admits(r, l))))
            .map(|(id, l)| (l.key.unwrap_or_default(), id.clone()))
            .collect();
        ended.sort();
        for (_, id) in ended {
            self.unhold(&id, now, out);
        }
    }

    // ------------------------------------------------------------------ reservations (W05-R12)

    /// Which reservation keeps `r` (or the leg's pool) from `l` now, and until when.
    fn reserved_against(&self, r: &Runner, l: &Leg) -> Option<(String, i64)> {
        self.windows.iter().find_map(|(i, end)| {
            let res = self.cfg.reservations.get(*i)?;
            let keeps = match &res.pool {
                Some(p) => l.req.pool.as_deref() == Some(p.as_str()),
                None => res.holds(&r.input.view),
            };
            (keeps && !res.admits(&l.input.route)).then(|| (res.name.clone(), *end))
        })
    }

    /// The windows open at `now`, and a timer for the next that opens or closes; then every
    /// runner looks again at what waits, as what may run where changed.
    fn refresh_windows(&mut self, now: i64, out: &mut Vec<Command>) {
        let before = self.windows.clone();
        self.windows = self.cfg.reservations.iter().enumerate().filter_map(|(i, r)| r.window_end(now).map(|end| (i, end))).collect();
        let next = self.windows.iter().map(|(_, end)| *end).chain(self.cfg.reservations.iter().filter_map(|r| r.next_start(now))).filter(|t| *t > now).min();
        // Looked at again within a week at the latest.
        let at = next.unwrap_or(now + 7 * 86_400_000);
        self.windows_at = Some(at);
        self.timers.entry(at).or_default();
        out.push(Command::Timer { at });
        if self.windows != before {
            let runners: Vec<String> = self.runners.keys().cloned().collect();
            for r in runners {
                self.try_runner(&r, now, out);
            }
            self.refresh_reasons(out);
        }
    }

    // ------------------------------------------------------------------ reservations for big items (W05-R39)

    /// How many of a class's first queued items [`Self::head`] looks at for one that waits for
    /// room, so a queue of legs held back for other reasons (a quota) costs a bounded look.
    const HEAD_LOOK: usize = 64;

    /// Works out the reservation of each class up to `upto` whose first item `runner` passed over
    /// for want of room (`passed`, [`Self::bests`]): the class's first queued item by every
    /// dimension that waits for room keeps room on the runner where it would fit soonest; a whole
    /// gang, each of its members on the runner where its room frees soonest, all from the latest
    /// of their times (W05-R40). A reservation stands until its item is granted, cancelled or no
    /// longer first, and an item's moves only to where it would fit sooner; one whose time has
    /// passed is worked out again, from the expected ends moved on since. Whether one was made,
    /// moved or dropped.
    fn reserve(&mut self, runner: &str, passed: [bool; 5], upto: Class, now: i64, out: &mut Vec<Command>) -> bool {
        let mut changed = false;
        for class in Class::ALL.into_iter().filter(|c| *c <= upto && passed.get(c.index()).copied().unwrap_or(false)) {
            let Some(head) = self.head(class) else {
                changed |= self.reserved.remove(&class).is_some();
                continue;
            };
            let gang = self.legs.get(&head).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone());
            let standing = self.reserved.get(&class).filter(|k| k.holds(&head) && k.at > now).cloned();
            let next = match (&standing, gang) {
                // A gang's stands as it was worked out.
                (Some(_), Some(_)) => None,
                (None, Some(g)) => self.soonest_gang(&g, now),
                // An item's stands, moved only where it would fit sooner.
                (Some(k), None) if k.on(runner) => None,
                (Some(k), None) => self.earliest(&head, runner, now, &[]).filter(|at| *at < k.at).map(|at| (vec![(head.clone(), runner.to_owned())], at)),
                (None, None) => self.soonest(&head, now).map(|(on, at)| (vec![(head.clone(), on)], at)),
            };
            let Some((items, at)) = next else {
                // No runner could take it even once its grants end: nothing kept for it.
                if standing.is_none() {
                    changed |= self.reserved.remove(&class).is_some();
                }
                continue;
            };
            // Its class goes before those after it: their reservations on its runners are worked
            // out again when next they matter.
            self.reserved.retain(|c, k| *c <= class || !items.iter().any(|(_, r)| k.on(r)));
            let legs: Vec<String> = items.iter().map(|(l, _)| l.clone()).collect();
            self.reserved.insert(class, Kept { items, at, made: now });
            for l in legs {
                let reason = self.wait_reason(&l);
                self.set_reason(&l, reason, out);
            }
            changed = true;
        }
        changed
    }

    /// A class's first queued item by every dimension (as legs going to their best runner go:
    /// [`Self::pick_key`]) that waits for room ([`Self::waits_for_room`]), among its first
    /// [`Self::HEAD_LOOK`]. A gang is its first member there (W05-R40).
    fn head(&self, class: Class) -> Option<String> {
        let set = PROFILES - 1;
        let mut walks: Vec<std::iter::Peekable<Walk<'_>>> = self.sigs.values().filter_map(|s| s.queue.get(&class)).map(|level| self.walk(class, level, set).peekable()).collect();
        for _ in 0..Self::HEAD_LOOK {
            let mut least: Option<(([u128; LEVELS], i128, u64), usize)> = None;
            for (i, w) in walks.iter_mut().enumerate() {
                if let Some((standing, k, q, _)) = w.peek()
                    && least.is_none_or(|(key, _)| (*standing, *k, *q) < key)
                {
                    least = Some(((*standing, *k, *q), i));
                }
            }
            let (_, i) = least?;
            let (_, _, _, id) = walks.get_mut(i)?.next()?;
            if self.waits_for_room(id) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Whether a queued leg waits for room alone (W05-R39): some runner it may run on would take
    /// it and could hold it idle, and none has room for it now. A gang's member waits for room
    /// when its gang is whole, can't start now, and each of its members could be held somewhere
    /// (W05-R40). One a pool's executor or a warm runner holds isn't one.
    fn waits_for_room(&self, id: &str) -> bool {
        let Some(l) = self.legs.get(id).filter(|l| l.state == State::Queued && l.hold.is_none() && l.req.pool.is_none()) else { return false };
        if let Some(g) = &l.input.gang {
            return self.whole(&g.id) && self.gang_plan(&g.id).is_none() && self.crew_queued(&g.id).iter().all(|m| self.could_be_held(m));
        }
        let Some(s) = self.sigs.get(&l.sig) else { return false };
        let mut could = false;
        for r in s.eligible.iter().filter_map(|r| self.runners.get(r)) {
            if !self.admits(r, l) {
                continue;
            }
            if self.capacity(r, l) {
                return false;
            }
            could |= self.could_hold(r, l);
        }
        could
    }

    /// Whether some runner a leg may run on would take it and could hold it idle.
    fn could_be_held(&self, id: &str) -> bool {
        let Some(l) = self.legs.get(id) else { return false };
        self.sigs.get(&l.sig).is_some_and(|s| s.eligible.iter().filter_map(|r| self.runners.get(r)).any(|r| self.admits(r, l) && self.could_hold(r, l)))
    }

    /// The runner where a leg would fit soonest ([`Self::earliest`]), and when: ties to where it
    /// would finish first (W24-R18), then by name.
    fn soonest(&mut self, id: &str, now: i64) -> Option<(String, i64)> {
        self.soonest_beside(id, now, &[])
    }

    /// [`Self::soonest`], with `items` (leg, runner) put on their runners from now.
    fn soonest_beside(&mut self, id: &str, now: i64, items: &[(String, String)]) -> Option<(String, i64)> {
        let eligible: Vec<String> = self.legs.get(id).and_then(|l| self.sigs.get(&l.sig)).map(|s| s.eligible.iter().cloned().collect())?;
        let mut best: Option<(i64, i64, String)> = None;
        for r in eligible {
            let beside: Vec<String> = items.iter().filter(|(_, on)| *on == r).map(|(l, _)| l.clone()).collect();
            let Some(at) = self.earliest(id, &r, now, &beside) else { continue };
            let finish = at.saturating_add(self.legs.get(id).map_or(0, |l| ms_i64(self.expected_ms(l, &r))));
            if best.as_ref().is_none_or(|(a, f, n)| (at, finish, &r) < (*a, *f, n)) {
                best = Some((at, finish, r));
            }
        }
        best.map(|(at, _, r)| (r, at))
    }

    /// Where each member of a whole gang would fit soonest (W05-R40): in queue order, each on the
    /// runner where its room frees first beside the members put there before it; and the latest
    /// of their times, from which room is kept for them all. `None` when one fits nowhere.
    fn soonest_gang(&mut self, g: &str, now: i64) -> Option<(Vec<(String, String)>, i64)> {
        let mut items: Vec<(String, String)> = Vec::new();
        let mut at = now;
        for m in self.crew_queued(g) {
            let (on, t) = self.soonest_beside(&m, now, &items)?;
            at = at.max(t);
            items.push((m, on));
        }
        (!items.is_empty()).then_some((items, at))
    }

    /// When a runner is expected to have room for a leg (W05-R39): now, or as soon as enough of
    /// the grants it runs are expected to have ended (their expected ends looked at now: W05-R38),
    /// beside the items of the classes before the leg's kept there, each held from now until it's
    /// expected to end, and beside `mates` (legs put there with it). `None` when it wouldn't take
    /// the leg, couldn't hold it idle, or wouldn't have room for it even so.
    fn earliest(&mut self, id: &str, runner: &str, now: i64, mates: &[String]) -> Option<i64> {
        let running = self.runners.get(runner)?.running.clone();
        let ends: Vec<i64> = running.iter().filter_map(|g| self.look(g, now)).collect();
        let (r, l) = (self.runners.get(runner)?, self.legs.get(id)?);
        let class = l.key.map(|(c, _, _)| c)?;
        if !self.admits(r, l) || !self.could_hold(r, l) {
            return None;
        }
        let before = self.reserved_on(runner, |c, k| c < class && !k.holds(id));
        let mates: Vec<&Leg> = mates.iter().filter_map(|m| self.legs.get(m)).collect();
        let mut times: Vec<i64> = ends.into_iter().chain(before.iter().map(|(_, end)| *end)).map(|t| t.max(now)).collect();
        times.sort_unstable();
        times.dedup();
        std::iter::once(now).chain(times).find(|t| {
            let plus: Vec<&Leg> = before.iter().filter(|(_, end)| end > t).map(|(item, _)| *item).chain(mates.iter().copied()).collect();
            self.room_in(r, &self.holding_with(r, |g| g.due.is_none_or(|d| d.expected > *t), &plus), l)
        })
    }

    /// The items kept on a runner by the reservations `keep` keeps (W05-R39), each with when it's
    /// expected to end: its reservation's time plus its expected duration (W05-R38).
    fn reserved_on(&self, runner: &str, keep: impl Fn(Class, &Kept) -> bool) -> Vec<(&Leg, i64)> {
        self.reserved
            .iter()
            .filter(|(c, k)| keep(**c, k))
            .flat_map(|(_, k)| k.legs_on(runner).filter_map(|id| self.legs.get(id)).map(|item| (item, Due::of(&item.input, k.at).expected)))
            .collect()
    }

    /// A running leg's expected end looked at at `now` (W05-R38), kept as it was moved, so a
    /// reservation worked out from it stays put until it passes again.
    fn look(&mut self, id: &str, now: i64) -> Option<i64> {
        let d = self.legs.get_mut(id)?.due.as_mut()?;
        *d = d.at(now);
        Some(d.expected)
    }

    /// Whether a runner may take a leg now as the reservations kept there go (W05-R39): each of
    /// the leg's class or a class before it, not its own, lets it ([`Self::respects`]).
    fn allowed(&self, r: &Runner, l: &Leg) -> bool {
        self.allowed_beside(r, l, &[])
    }

    /// [`Self::allowed`], with `mates` put on the runner with it.
    fn allowed_beside(&self, r: &Runner, l: &Leg, mates: &[&Leg]) -> bool {
        let class = l.key.map_or(Class::Background, |(c, _, _)| c);
        self.reserved.iter().filter(|(c, k)| **c <= class && k.on(&r.input.id) && !k.holds(&l.input.id)).all(|(_, k)| self.respects(r, l, k, mates))
    }

    /// Whether a leg granted on a runner now, `mates` beside it, leaves a reservation there its
    /// room (W05-R39): it's expected to end by the reservation's time, or fits there then beside
    /// the items kept there, the grants still expected to run then, the other items kept there
    /// that will be running then, and its mates.
    fn respects(&self, r: &Runner, l: &Leg, k: &Kept, mates: &[&Leg]) -> bool {
        if k.holds(&l.input.id) || l.req.pool.is_some() || Due::of(&l.input, self.now).expected <= k.at {
            return true;
        }
        let id = &r.input.id;
        let mut plus: Vec<&Leg> = k.legs_on(id).filter_map(|i| self.legs.get(i)).collect();
        plus.extend(self.reserved_on(id, |_, o| o != k && o.at <= k.at).into_iter().filter(|(_, end)| *end > k.at).map(|(other, _)| other));
        plus.extend(mates.iter().copied());
        self.room_in(r, &self.holding_with(r, |g| g.due.is_none_or(|d| d.at(self.now).expected > k.at), &plus), l)
    }

    /// The reservations a leg granted on a runner doesn't leave room (W05-R39): those of classes
    /// after its own, which it goes before, are dropped, and worked out again when next they
    /// matter. A grant of an item kept for ends its reservation.
    fn granted_over(&mut self, id: &str, runner: &str) {
        if self.reserved.is_empty() {
            return;
        }
        let (Some(l), Some(r)) = (self.legs.get(id), self.runners.get(runner)) else { return };
        let gone: Vec<Class> = self.reserved.iter().filter(|(_, k)| k.holds(id) || (k.on(runner) && !self.respects(r, l, k, &[]))).map(|(c, _)| *c).collect();
        for c in gone {
            self.reserved.remove(&c);
        }
    }

    /// Drops the reservations with an item no longer waiting in the queue in their class, or a
    /// runner gone, away or draining (W05-R39).
    fn drop_stale_reservations(&mut self) {
        if self.reserved.is_empty() {
            return;
        }
        let (legs, runners) = (&self.legs, &self.runners);
        self.reserved.retain(|class, k| {
            k.items.iter().all(|(leg, runner)| legs.get(leg).is_some_and(|l| l.state == State::Queued && l.key.is_some_and(|(c, _, _)| c == *class)) && runners.get(runner).is_some_and(|r| r.connected && !r.draining))
        });
    }

    // ------------------------------------------------------------------ gangs (W05-R40)

    /// A gang's members queued now, in queue order ([`Self::pick_key`]).
    fn crew_queued(&self, g: &str) -> Vec<String> {
        let Some(c) = self.gangs.get(g) else { return Vec::new() };
        let mut queued: Vec<_> = c.members.iter().filter_map(|id| self.legs.get(id).filter(|l| l.state == State::Queued).and_then(|l| self.pick_key(l)).map(|k| (k, id.clone()))).collect();
        queued.sort();
        queued.into_iter().map(|(_, id)| id).collect()
    }

    /// Whether a gang is whole (W05-R40): as many of its members queued as make it.
    fn whole(&self, g: &str) -> bool {
        let Some(c) = self.gangs.get(g) else { return false };
        let queued = c.members.iter().filter(|m| self.legs.get(m.as_str()).is_some_and(|l| l.state == State::Queued)).count();
        u32::try_from(queued).unwrap_or(u32::MAX) >= c.size.max(1)
    }

    /// Where a whole gang would start now (W05-R40), if it can: each member, in queue order, on a
    /// runner that would take it and has room for it beside the members put there before it, as
    /// the reservations there let it, the least loaded first, then by name; where a member has
    /// no such runner, the members before it are put elsewhere, in that order, trying at most
    /// [`Self::GANG_LOOK`] places in all. A pool's member takes one of the pool's idle executors
    /// there that the members before it haven't. `None` when no way is found.
    fn gang_plan(&self, g: &str) -> Option<Vec<(String, String)>> {
        let members = self.crew_queued(g);
        let mut plan: Vec<(String, String)> = Vec::new();
        let mut budget = Self::GANG_LOOK;
        (!members.is_empty() && self.plan_from(&members, &mut plan, &mut budget)).then_some(plan)
    }

    /// How many places [`Self::gang_plan`] tries for a gang's members at most, so a gang that
    /// can't start costs a bounded look.
    const GANG_LOOK: usize = 4096;

    /// Puts the members after those in `plan` ([`Self::gang_plan`]). Whether all were put.
    fn plan_from(&self, members: &[String], plan: &mut Vec<(String, String)>, budget: &mut usize) -> bool {
        let Some(m) = members.get(plan.len()) else { return true };
        for r in self.gang_places(m, plan) {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            plan.push((m.clone(), r));
            if self.plan_from(members, plan, budget) {
                return true;
            }
            plan.pop();
        }
        false
    }

    /// The runners a gang's member could start on beside the members `plan` puts there, best
    /// first: the least loaded, then by name.
    fn gang_places(&self, m: &str, plan: &[(String, String)]) -> Vec<String> {
        let (Some(l), Some(s)) = (self.legs.get(m), self.legs.get(m).and_then(|l| self.sigs.get(&l.sig))) else { return Vec::new() };
        let mut places: Vec<(usize, &String)> = Vec::new();
        for rid in &s.eligible {
            let Some(r) = self.runners.get(rid).filter(|r| self.admits(r, l)) else { continue };
            let mates: Vec<&Leg> = plan.iter().filter(|(_, on)| on == rid).filter_map(|(x, _)| self.legs.get(x)).collect();
            let room = match &l.req.pool {
                Some(pool) => {
                    let own = self.own_leg(l);
                    let idle = self.executors.values().filter(|e| e.pool == *pool && e.runner == *rid && e.state == ExecState::Idle && e.leg.as_deref() == own).count();
                    idle > mates.iter().filter(|x| x.req.pool.as_ref() == Some(pool)).count()
                }
                None => {
                    let beside: Vec<&Leg> = mates.iter().copied().filter(|x| x.req.pool.is_none()).collect();
                    self.room_in(r, &self.holding_with(r, |_| true, &beside), l)
                }
            };
            if room && self.allowed_beside(r, l, &mates) {
                places.push((r.running.len() + mates.len(), rid));
            }
        }
        places.sort();
        places.into_iter().map(|(_, r)| r.clone()).collect()
    }

    /// Starts a whole gang if it can start now (W05-R40): each member granted where
    /// [`Self::gang_plan`] puts it, in one input. Whether it started.
    fn place_gang(&mut self, g: &str, now: i64, out: &mut Vec<Command>) -> bool {
        if !self.whole(g) {
            return false;
        }
        let Some(plan) = self.gang_plan(g) else { return false };
        if let Some(c) = self.gangs.get_mut(g) {
            c.ended = false;
            c.round = plan.iter().map(|(m, _)| m.clone()).collect();
        }
        for (m, r) in plan {
            self.assign(&m, &r, now, out);
        }
        true
    }

    /// A leg of a gang joins it (W05-R40): the gang is as big as its members say it is.
    fn join_gang(&mut self, id: &str, gang: Option<&Gang>) {
        let Some(g) = gang else { return };
        let c = self.gangs.entry(g.id.clone()).or_default();
        c.size = c.size.max(g.size);
        c.members.insert(id.to_owned());
    }

    /// A leg that left the engine leaves its gang (W05-R40), and the gang goes with its last.
    fn left_gang(&mut self, id: &str, gone: Option<&Leg>) {
        let Some(g) = gone.and_then(|l| l.input.gang.as_ref()) else { return };
        if let Some(c) = self.gangs.get_mut(&g.id) {
            c.members.remove(id);
            c.round.remove(id);
            if c.members.is_empty() {
                self.gangs.remove(&g.id);
            }
        }
    }

    /// Says why each queued member of a gang waits (W05-R40).
    fn gang_reasons(&mut self, g: &str, out: &mut Vec<Command>) {
        for m in self.crew_queued(g) {
            let reason = self.wait_reason(&m);
            self.set_reason(&m, reason, out);
        }
    }

    /// Takes back the grants of a gang's members other than `but` (W05-R40), refused or not
    /// granted: each is told lost (`Loss::Gang`) and queued again at its attempt, so the gang goes
    /// again whole. The runners whose room they leave.
    fn take_back_gang(&mut self, but: &str, now: i64, out: &mut Vec<Command>) -> Vec<String> {
        let Some(g) = self.legs.get(but).and_then(|l| l.input.gang.as_ref()).map(|g| g.id.clone()) else { return Vec::new() };
        let peers: Vec<String> = self.gangs.get(&g).map(|c| c.members.iter().filter(|m| *m != but).cloned().collect()).unwrap_or_default();
        let mut freed = Vec::new();
        for p in peers {
            let Some(l) = self.legs.get(&p) else { continue };
            let State::Assigned { runner } = &l.state else { continue };
            out.push(Command::Lost { leg: p.clone(), attempt: l.attempt, runner: runner.clone(), fence: l.fence, why: Loss::Gang });
            if let Some(r) = self.release(&p, now) {
                freed.push(r);
            }
            self.stopped(&p);
            self.discard_used(&p, out);
            self.enqueue(&p, now, out);
        }
        freed
    }

    /// A gang's member lost before any of its members' attempts ended (W05-R40): the others' grants
    /// are taken back (`Loss::Gang`) and every member's attempt ends lost; the gang goes again whole
    /// at the next attempts if each may be tried again (W05-R16), else every member ends failed.
    fn gang_lost(&mut self, g: &str, lost: &str, attempt: u32, outcome: Outcome, now: i64, out: &mut Vec<Command>) {
        let peers: Vec<(String, u32)> = self.gangs.get(g).map(|c| c.members.iter().filter(|m| *m != lost).filter_map(|m| self.legs.get(m).filter(|l| matches!(l.state, State::Assigned { .. })).map(|l| (m.clone(), l.attempt))).collect()).unwrap_or_default();
        let failed = Failed { class: FailureClass::RunnerLost, exit_code: None, log_hits: Vec::new() };
        let again = std::iter::once((lost, attempt)).chain(peers.iter().map(|(p, a)| (p.as_str(), *a))).all(|(m, a)| retry::decide(self.legs.get(m).and_then(|l| l.input.retry.as_ref()), a, &failed).is_some());
        if let Some(c) = self.gangs.get_mut(g) {
            c.ended = true;
        }
        for (p, a) in &peers {
            if let Some(l) = self.legs.get(p)
                && let State::Assigned { runner } = &l.state
            {
                out.push(Command::Lost { leg: p.clone(), attempt: *a, runner: runner.clone(), fence: l.fence, why: Loss::Gang });
            }
        }
        self.finish(lost, attempt, outcome, again, now, out);
        for (p, a) in &peers {
            self.finish(p, *a, Outcome::Failed(failed.clone()), again, now, out);
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
            if !self.place_gang(g, now, out) {
                self.gang_reasons(g, out);
            }
        }
    }

    // ------------------------------------------------------------------ pools (W13-R09)

    fn pool(&self, name: &str) -> Option<&Pool> {
        self.cfg.pools.iter().find(|p| p.name == name)
    }

    fn drop_executor(&mut self, id: &str) {
        if let Some(e) = self.executors.remove(id)
            && let Some(r) = self.runners.get_mut(&e.runner)
        {
            sub(&mut r.used, &e.reserved);
        }
    }

    /// Legs queued for `pool`.
    fn waiting_for(&self, pool: &str) -> usize {
        self.sigs.values().filter(|s| s.req.pool.as_deref() == Some(pool)).map(Sig::len).sum()
    }

    /// The hosts that may run `pool`'s executors now, with room for another: connected, not
    /// draining, eligible for its legs, under `per_host_max`, with the resources in resource
    /// mode, and not backing off; the least loaded with the pool first.
    fn hosts_for(&self, p: &Pool, now: i64) -> Vec<String> {
        let req = Requirement { constraints: p.hosts.clone(), caps: p.caps, executors: vec![p.executor.clone()], pool: Some(p.name.clone()), ..Requirement::default() };
        let mut hosts: Vec<(usize, String)> = self
            .runners
            .values()
            .filter(|r| r.connected && !r.draining && routing::eligible(&req, &r.input.view))
            .filter(|r| self.backoff.get(&(p.name.clone(), r.input.id.clone())).is_none_or(|(until, _)| *until <= now))
            .filter_map(|r| {
                let here = self.executors.values().filter(|e| e.pool == p.name && e.runner == r.input.id).count();
                if here >= p.per_host_max {
                    return None;
                }
                let c = self.config();
                if let (true, Some(cap)) = (c.resources, r.input.capacity) {
                    // Beside what exclusive legs hold there, never in it.
                    let q = &p.request;
                    let (held, whole, _) = self.holding(r);
                    if whole || !fits_beside(&r.used, &held, &cap, q, c) {
                        return None;
                    }
                }
                Some((here, r.input.id.clone()))
            })
            .collect();
        hosts.sort();
        hosts.into_iter().map(|(_, h)| h).collect()
    }

    /// Starts as many of `pool`'s executors as its waiting legs and `min_idle` want.
    fn scale(&mut self, pool: &str, now: i64, out: &mut Vec<Command>) {
        let Some(p) = self.pool(pool).cloned() else { return };
        let count = |s: fn(&ExecState) -> bool| self.executors.values().filter(|e| e.pool == pool && s(&e.state)).count();
        let (starting, idle, busy) = (count(|s| *s == ExecState::Starting), count(|s| *s == ExecState::Idle), count(|s| matches!(s, ExecState::Busy { .. })));
        let mut want = pools::wanted(&p, starting, idle, busy, self.waiting_for(pool));
        // The server's executors (a `k8s` pool's pods): started by it, each its own runner once
        // it connects; not while a failed start's backoff lasts.
        if p.hosted {
            if self.backoff.get(&(p.name.clone(), String::new())).is_some_and(|(until, _)| *until > now) {
                return;
            }
            // Each waiting leg that needs a pod of its own (W13-R07) gets one, within `max`; the
            // shared ones are for the others.
            let waiting: Vec<(String, bool)> = self
                .sigs
                .values()
                .filter(|s| s.req.pool.as_deref() == Some(pool))
                .flat_map(|s| s.ids().cloned())
                .filter_map(|id| self.legs.get(&id).map(|l| (id, l.input.own_executor)))
                .collect();
            let mut room = p.max.saturating_sub(self.executors.values().filter(|e| e.pool == pool).count());
            for (leg, _) in waiting.iter().filter(|(_, own)| *own) {
                if room == 0 {
                    break;
                }
                if self.executors.values().any(|e| e.leg.as_deref() == Some(leg.as_str())) {
                    continue;
                }
                room -= 1;
                self.start_hosted(&p, Some(leg.clone()), now, out);
            }
            let shared = |s: fn(&ExecState) -> bool| self.executors.values().filter(|e| e.pool == pool && e.leg.is_none() && s(&e.state)).count();
            let (starting, idle, busy) = (shared(|s| *s == ExecState::Starting), shared(|s| *s == ExecState::Idle), shared(|s| matches!(s, ExecState::Busy { .. })));
            let others = waiting.iter().filter(|(_, own)| !*own).count();
            for _ in 0..pools::wanted(&p, starting, idle, busy, others).min(room) {
                self.start_hosted(&p, None, now, out);
            }
            return;
        }
        while want > 0 {
            let Some(host) = self.hosts_for(&p, now).into_iter().next() else { break };
            self.seq += 1;
            let id = format!("pool-{pool}-{now:x}-{:x}", self.seq);
            self.executors.insert(id.clone(), Executor { id: id.clone(), pool: pool.to_owned(), runner: host.clone(), state: ExecState::Starting, since: now, reserved: p.request, leg: None });
            if let Some(r) = self.runners.get_mut(&host) {
                add(&mut r.used, &p.request);
            }
            out.push(Command::StartExecutor { pool: pool.to_owned(), executor: id, runner: host, kind: p.executor.clone(), image: p.image.clone(), leg: None });
            want -= 1;
        }
    }

    /// One of a hosted pool's executors started by the server: made for `leg`, or for any.
    fn start_hosted(&mut self, p: &Pool, leg: Option<String>, now: i64, out: &mut Vec<Command>) {
        self.seq += 1;
        let id = format!("pool-{}-{now:x}-{:x}", p.name, self.seq);
        self.executors
            .insert(id.clone(), Executor { id: id.clone(), pool: p.name.clone(), runner: String::new(), state: ExecState::Starting, since: now, reserved: Resources::default(), leg: leg.clone() });
        out.push(Command::StartExecutor { pool: p.name.clone(), executor: id, runner: String::new(), kind: p.executor.clone(), image: p.image.clone(), leg });
    }

    /// The leg's own id when it needs an executor made for it: it has containers, and its pool's
    /// executors are the server's (W13-R07).
    fn own_leg<'l>(&self, l: &'l Leg) -> Option<&'l str> {
        let hosted = l.req.pool.as_deref().and_then(|p| self.pool(p)).is_some_and(|p| p.hosted);
        (hosted && l.input.own_executor).then_some(l.input.id.as_str())
    }

    fn scale_all(&mut self, now: i64, out: &mut Vec<Command>) {
        let names: Vec<String> = self.cfg.pools.iter().map(|p| p.name.clone()).collect();
        for n in names {
            self.scale(&n, now, out);
        }
    }

    fn executor_ready(&mut self, id: &str, now: i64, out: &mut Vec<Command>) {
        let Some(e) = self.executors.get_mut(id) else { return };
        if e.state != ExecState::Starting {
            return;
        }
        e.state = ExecState::Idle;
        e.since = now;
        let (pool, runner) = (e.pool.clone(), e.runner.clone());
        self.backoff.remove(&(pool.clone(), runner.clone()));
        self.try_runner(&runner, now, out);
        self.idle_timer(&pool, now, out);
        self.refresh_reasons(out);
        self.review_admission(now, out);
    }

    fn executor_failed(&mut self, id: &str, error: String, now: i64, out: &mut Vec<Command>) {
        let Some(e) = self.executors.get(id).cloned() else { return };
        self.drop_executor(id);
        // Gone under its leg: the leg's runner reports how the attempt ended.
        if matches!(e.state, ExecState::Busy { .. }) {
            return;
        }
        // The host is left alone a while, longer each time it fails.
        let key = (e.pool.clone(), e.runner.clone());
        let delay = self.backoff.get(&key).map_or(pools::FIRST_BACKOFF_MS, |(_, d)| *d);
        self.backoff.insert(key, (now + delay, (delay * 2).min(pools::MAX_BACKOFF_MS)));
        self.pool_errors.insert(e.pool.clone(), (e.runner.clone(), error));
        self.timers.entry(now + delay).or_default();
        out.push(Command::Timer { at: now + delay });
        self.scale(&e.pool, now, out);
        self.refresh_reasons(out);
    }

    /// A timer for when `pool`'s oldest idle executor passes its `idle_timeout`.
    fn idle_timer(&mut self, pool: &str, now: i64, out: &mut Vec<Command>) {
        let Some(p) = self.pool(pool) else { return };
        let timeout = p.idle_timeout_ms;
        if let Some(at) = self.executors.values().filter(|e| e.pool == pool && e.state == ExecState::Idle).map(|e| e.since + timeout).min()
            && at > now
        {
            self.timers.entry(at).or_default();
            out.push(Command::Timer { at });
        }
    }

    /// Stops executors idle past their pool's `idle_timeout`, oldest first, while the pool keeps
    /// `min_idle` and nothing waits for it.
    fn reap_idle(&mut self, now: i64, out: &mut Vec<Command>) {
        let pools: Vec<Pool> = self.cfg.pools.clone();
        for p in pools {
            if self.waiting_for(&p.name) > 0 {
                continue;
            }
            let mut idle: Vec<(i64, String, String)> = self.executors.values().filter(|e| e.pool == p.name && e.state == ExecState::Idle).map(|e| (e.since, e.id.clone(), e.runner.clone())).collect();
            idle.sort();
            let spare = idle.len() + self.executors.values().filter(|e| e.pool == p.name && e.state == ExecState::Starting).count();
            let mut over = spare.saturating_sub(p.min_idle);
            for (since, id, runner) in idle {
                if over == 0 || now - since < p.idle_timeout_ms {
                    break;
                }
                self.drop_executor(&id);
                out.push(Command::StopExecutor { executor: id, runner });
                over -= 1;
            }
            self.idle_timer(&p.name, now, out);
        }
    }

    /// The pools and their executors now (the Pools page, `GET /api/ci/pools`).
    pub fn pools(&self) -> Vec<PoolView> {
        self.cfg
            .pools
            .iter()
            .map(|p| {
                let mine: Vec<Executor> = self.executors.values().filter(|e| e.pool == p.name).cloned().collect();
                let count = |f: fn(&ExecState) -> bool| mine.iter().filter(|e| f(&e.state)).count();
                PoolView {
                    name: p.name.clone(),
                    executor: p.executor.clone(),
                    image: p.image.clone(),
                    labels: p.labels.clone(),
                    hosts: p.hosts.first().map(|(s, _)| s.clone()),
                    min_idle: p.min_idle,
                    max: p.max,
                    per_host_max: p.per_host_max,
                    idle_timeout_ms: p.idle_timeout_ms,
                    starting: count(|s| *s == ExecState::Starting),
                    idle: count(|s| *s == ExecState::Idle),
                    busy: count(|s| matches!(s, ExecState::Busy { .. })),
                    waiting: self.waiting_for(&p.name),
                    last_error: self.pool_errors.get(&p.name).cloned(),
                    executors: mine,
                }
            })
            .collect()
    }

    fn set_reason(&mut self, id: &str, reason: WaitReason, out: &mut Vec<Command>) {
        if let Some(l) = self.legs.get_mut(id)
            && l.reason.as_ref() != Some(&reason)
        {
            l.reason = Some(reason.clone());
            out.push(Command::Wait { leg: id.to_owned(), reason });
        }
    }

    /// Re-decides every queued leg's reason after the fleet changed.
    fn refresh_reasons(&mut self, out: &mut Vec<Command>) {
        let ids: Vec<String> = self.legs.iter().filter(|(_, l)| l.state == State::Queued).map(|(k, _)| k.clone()).collect();
        for id in ids {
            let r = self.wait_reason(&id);
            self.set_reason(&id, r, out);
        }
    }

    /// Why a queued leg waits (W05-R13).
    fn wait_reason(&self, id: &str) -> WaitReason {
        let Some(l) = self.legs.get(id) else { return WaitReason::Unroutable { failing: Vec::new() } };
        if let Some(q) = self.config().quotas.get(&l.input.repo)
            && self.in_progress.get(&l.input.repo).copied().unwrap_or(0) >= *q
        {
            return WaitReason::Quota { limit: *q };
        }
        // A gang's member waits for the rest of its gang (W05-R40).
        if let Some(g) = &l.input.gang
            && !self.whole(&g.id)
        {
            let have = self.gangs.get(&g.id).map_or(0, |c| c.members.iter().filter(|m| self.legs.get(m.as_str()).is_some_and(|x| x.state == State::Queued)).count());
            let of = self.gangs.get(&g.id).map_or(g.size, |c| c.size);
            return WaitReason::Gang { gang: g.id.clone(), have: u32::try_from(have).unwrap_or(u32::MAX), of };
        }
        if let Some(h) = &l.hold {
            return WaitReason::Warm { runner: h.name.clone(), saves_s: h.saves_s, until: h.until };
        }
        let routed: Vec<&Runner> = self.sigs.get(&l.sig).map(|s| s.eligible.iter().filter_map(|r| self.runners.get(r)).collect()).unwrap_or_default();
        // Of those, the ones a person's retry lets it run on (W05-R28).
        let eligible: Vec<&Runner> = routed.iter().copied().filter(|r| l.input.placement.allows(&r.input.id)).collect();
        // Every runner that could take it is kept for other legs: until the first window ends.
        if !eligible.is_empty()
            && let Some((name, until)) = eligible.iter().map(|r| self.reserved_against(r, l)).collect::<Option<Vec<_>>>().and_then(|all| all.into_iter().min_by_key(|(_, u)| *u))
        {
            return WaitReason::Reservation { reservation: name, until };
        }
        let name = |id: &str| self.runners.get(id).map_or_else(|| id.to_owned(), |r| r.input.view.name.clone());
        if let Some(p) = &l.input.placement.runner {
            let state = match self.runners.get(p) {
                Some(r) if !r.connected => PinnedState::Offline,
                Some(r) if !self.routable(&l.req, r) => PinnedState::Unable,
                Some(r) if r.draining => PinnedState::Draining,
                Some(_) => PinnedState::Busy,
                None => PinnedState::Offline,
            };
            return WaitReason::Pinned { runner: name(p), state };
        }
        if let Some(a) = &l.input.placement.avoid
            && eligible.is_empty()
            && !routed.is_empty()
        {
            return WaitReason::OtherRunner { avoided: name(a), refused: false, asked: true };
        }
        if let (Some(pool), false) = (&l.req.pool, eligible.is_empty()) {
            let mine: Vec<&Executor> = self.executors.values().filter(|e| e.pool == *pool).collect();
            let starting: Vec<&&Executor> = mine.iter().filter(|e| e.state == ExecState::Starting).collect();
            if let Some(since) = starting.iter().map(|e| e.since).min() {
                return WaitReason::Scaling { pool: pool.clone(), starting: starting.len(), since };
            }
            // At its `max` (or no host has room): its executors are the capacity.
            let busy = mine.iter().filter(|e| matches!(e.state, ExecState::Busy { .. })).count();
            return WaitReason::Capacity { eligible: mine.len(), busy, draining: 0 };
        }
        // Only the runner it stays off could take it: the only one eligible, or the only one big
        // enough to hold it (W05-R16, R13).
        let could: Vec<&&Runner> = eligible.iter().filter(|r| self.could_hold(r, l)).collect();
        if let Some((a, off)) = &l.avoid
            && *off != Off::Lost
            && !could.is_empty()
            && could.iter().all(|r| r.input.id == *a)
        {
            return WaitReason::OtherRunner { avoided: name(a), refused: *off == Off::Refused, asked: false };
        }
        // Heavy, and held off by their load the runners that could take it (W24-R19).
        let held: Vec<super::load::Held> = eligible.iter().filter(|r| r.connected && !r.draining).filter_map(|r| self.held_by_load(r, l)).collect();
        if !held.is_empty() {
            return WaitReason::Load { held };
        }
        if !eligible.is_empty() {
            // A share no runner it could run on is big enough to hold.
            let q = &l.input.request;
            if l.input.exclusive && !q.cpu.is_zero() && eligible.iter().all(|r| size(r).is_some_and(|s| q.cpu > s.cpu || q.mem > s.mem)) {
                return WaitReason::Unroutable { failing: vec![format!("{} CPUs and {} MiB held for it alone: no runner it could run on is that big", q.cpu, q.mem >> 20)] };
            }
            if let Some(short) = self.oversized(&eligible, l) {
                return WaitReason::Oversized { asked: needs::request_words(&l.input.request).join(", "), short };
            }
            // The first of its class that fits no runner, its room kept where it frees first
            // (W05-R39).
            if let Some((runner, at)) = self.reserved.values().find_map(|k| k.items.iter().find(|(leg, _)| leg == id).map(|(_, r)| (r, k.at))) {
                return WaitReason::Reserved { runner: name(runner), at };
            }
            if l.input.exclusive && eligible.iter().all(|r| matches!(claim(&l.input, size(r)), Some(Claim::Whole)) && (!r.running.is_empty() || r.draining)) {
                return WaitReason::Exclusive;
            }
            // It would fit, but only into room kept for another (W05-R39): the soonest that keeps it.
            if !self.reserved.is_empty() {
                let fitting: Vec<&Runner> = eligible.iter().copied().filter(|r| self.fits(r, l)).collect();
                if !fitting.is_empty() && fitting.iter().all(|r| !self.allowed(r, l)) {
                    let class = l.key.map_or(Class::Background, |(c, _, _)| c);
                    let kept = fitting.iter().flat_map(|r| self.reserved.iter().filter(move |(c, k)| **c <= class && k.on(&r.input.id) && !k.holds(id) && !self.respects(r, l, k, &[])).map(move |(_, k)| (k, r)));
                    if let Some((k, r)) = kept.min_by_key(|(k, r)| (k.at, r.input.id.clone())) {
                        let leg = k.legs_on(&r.input.id).next().cloned().unwrap_or_default();
                        return WaitReason::Behind { leg, runner: name(&r.input.id), at: k.at };
                    }
                }
            }
            let draining = eligible.iter().filter(|r| r.draining).count();
            let busy = eligible.iter().filter(|r| !r.draining && !self.fits(r, l)).count();
            return WaitReason::Capacity { eligible: eligible.len(), busy, draining };
        }
        let connected: Vec<&Runner> = self.runners.values().filter(|r| r.connected).collect();
        let lacking: Vec<String> = connected.iter().filter(|r| routing::lacks_only_capability(&l.req, &r.input.view)).map(|r| r.input.view.name.clone()).collect();
        if !lacking.is_empty() {
            return WaitReason::Capability { runners: lacking };
        }
        let offline: Vec<String> = self.runners.values().filter(|r| !r.connected && routing::eligible(&l.req, &r.input.view)).map(|r| r.input.view.name.clone()).collect();
        if !offline.is_empty() {
            return WaitReason::NoRunner { offline };
        }
        let mut failing: Vec<String> = Vec::new();
        for r in self.runners.values() {
            for c in routing::checks(&l.req, &r.input.view) {
                if !c.pass && !failing.contains(&c.requirement) {
                    failing.push(c.requirement);
                }
            }
        }
        if failing.is_empty() {
            failing = l.req.constraints.iter().map(|(s, _)| s.clone()).chain(std::iter::once(format!("runs-on [{}]", l.req.labels.join(", ")))).collect();
        }
        WaitReason::Unroutable { failing }
    }

    /// Why each runner can or can't take a leg (W05-R20), and for a leg with `warm` or `affinity`
    /// terms what each one's warm state makes of it (W24-R18).
    pub fn explain(&self, id: &str) -> Vec<Explained> {
        let Some(l) = self.legs.get(id) else { return Vec::new() };
        let all: Vec<&Runner> = self.runners.values().collect();
        let scores = self.scores(l, &all, Some(self.now));
        let wants = Self::wants(l);
        self.runners
            .values()
            .enumerate()
            .map(|(i, r)| {
                let mut checks = routing::checks(&l.req, &r.input.view);
                checks.extend(placement_check(&l.input.placement, r));
                Explained {
                    runner: r.input.id.clone(),
                    name: r.input.view.name.clone(),
                    connected: r.connected,
                    eligible: checks.iter().all(|c| c.pass),
                    room: self.room(r, l),
                    score: scores.get(i).copied().map(units::score),
                    checks,
                    warm: (!wants.is_empty()).then(|| self.seen(l, r, &wants, self.now)),
                }
            })
            .collect()
    }

    /// How each runner measures up against a leg that isn't queued (W04-R29's preview): the
    /// requirement routing makes of `route`, and each runner's checks.
    pub fn explain_route(&self, route: &LegRoute) -> (Requirement, Vec<Explained>) {
        let req = self.requirement(&LegIn { route: route.clone(), ..LegIn::default() });
        let views: Vec<&RunnerView> = self.runners.values().map(|r| &r.input.view).collect();
        let scores = routing::scores(&req.prefer, &views);
        let list = self
            .runners
            .values()
            .enumerate()
            .map(|(i, r)| {
                let checks = routing::checks(&req, &r.input.view);
                Explained {
                    runner: r.input.id.clone(),
                    name: r.input.view.name.clone(),
                    connected: r.connected,
                    eligible: checks.iter().all(|c| c.pass),
                    room: r.connected && !r.draining && {
                        let (held, whole, ordinary) = self.holding(r);
                        !whole && ordinary < slots_left(r.input.slots, size(r), &held)
                    },
                    score: scores.get(i).copied().map(units::score),
                    checks,
                    warm: None,
                }
            })
            .collect();
        (req, list)
    }

    /// Why a leg stands where it does among those waiting (W28-R64, R66, R67, W05-R30, R31): its
    /// priority, its intent class and why, whose it is, its lane, its class and the one it was
    /// queued in if it was promoted, and its tenant in fair share's tree.
    pub fn standing(&self, id: &str) -> Option<Standing> {
        let l = self.legs.get(id)?;
        let class = l.key.map_or(l.input.class, |k| k.0);
        let tenant = if l.flow.is_empty() { self.leaf(&l.input) } else { l.flow.clone() };
        Some(Standing {
            priority: l.input.priority,
            intent: l.input.intent.clone(),
            actor: l.input.actor.clone(),
            lane: l.input.lane.clone(),
            class,
            promoted_from: (class != l.input.class).then_some(l.input.class),
            tenant: tenant.split('\0').map(str::to_owned).collect(),
        })
    }

    /// The current wait reason of a leg, if it waits.
    pub fn reason(&self, id: &str) -> Option<&WaitReason> {
        self.legs.get(id).and_then(|l| l.reason.as_ref())
    }

    /// Where a leg stands as grants go (W05-R34): its attempt now and, while it runs under a grant,
    /// that grant's runner and fence. What kiln-sim's model holds the store to (docs/sim.md).
    pub fn grant(&self, id: &str) -> Option<(u32, Option<(&str, Fence)>)> {
        let l = self.legs.get(id)?;
        let held = match &l.state {
            State::Assigned { runner } => Some((runner.as_str(), l.fence)),
            _ => None,
        };
        Some((l.attempt, held))
    }

    /// A running leg's ends as looked at at `now` (W05-R38): when it's expected to end (its p90's
    /// end, moved on once it has passed) and when it ends at the latest (its timeout's).
    pub fn due(&self, id: &str, now: i64) -> Option<(i64, i64)> {
        self.legs.get(id).and_then(|l| l.due).map(|d| d.at(now)).map(|d| (d.expected, d.hard))
    }

    /// Legs queued now (for metrics).
    pub fn queued(&self) -> usize {
        self.sigs.values().map(Sig::len).sum()
    }

    /// Each running leg's grant (W05-R34): its runner, attempt, fence and expiry.
    #[cfg(test)]
    pub(super) fn grants(&self) -> BTreeMap<String, (String, u32, Fence, Option<i64>)> {
        let mut out = BTreeMap::new();
        for (id, l) in self.legs.iter() {
            if let State::Assigned { runner } = &l.state {
                out.insert(id.clone(), (runner.clone(), l.attempt, l.fence, l.expires));
            }
        }
        out
    }

    /// The legs waiting to be retried, each with when (a stop item's lock-delay among them).
    #[cfg(test)]
    pub(super) fn retrying(&self) -> BTreeMap<String, i64> {
        self.legs.iter().filter_map(|(id, l)| if let State::Retrying { at } = l.state { Some((id.clone(), at)) } else { None }).collect()
    }

    /// The grants' invariants (W05-R34, docs/needs.md G2's half in the engine), checked after every
    /// input in tests: each running leg holds one grant, its fence its own, made in this epoch (no
    /// later than the last fence) or an earlier one, expiring as the expiry index has it when
    /// leases are on; the index holds nothing else; no leg runs on two runners or twice on one;
    /// and each running leg's ends go in order, from its start to its expected end to its hard end
    /// (W05-R38), a leg not running having none.
    #[cfg(test)]
    pub(super) fn check_grants(&self) -> Result<(), String> {
        let (mut indexed, mut fences) = (BTreeSet::new(), BTreeSet::new());
        for (id, l) in self.legs.iter() {
            match (&l.state, l.due) {
                (State::Assigned { .. }, Some(d)) if d.from <= d.expected && d.expected <= d.hard => {}
                (State::Assigned { .. }, due) => return Err(format!("{id} runs with ends {due:?}")),
                (_, Some(d)) => return Err(format!("{id} isn't running but has ends {d:?}")),
                (_, None) => {}
            }
            match (&l.state, l.expires) {
                (State::Assigned { runner }, expires) => {
                    if l.fence > (Fence { epoch: self.epoch, n: self.fences }) {
                        return Err(format!("{id}'s fence {:?} is past this epoch's last, {:?}", l.fence, Fence { epoch: self.epoch, n: self.fences }));
                    }
                    if l.fence != Fence::default() && !fences.insert(l.fence) {
                        return Err(format!("{id}'s fence {:?} is another grant's too", l.fence));
                    }
                    if expires.is_some() != self.cfg.lease.is_some() {
                        return Err(format!("{id} on {runner} has expiry {expires:?} with lease {:?}", self.cfg.lease));
                    }
                    if let Some(at) = expires {
                        indexed.insert((at, id.clone()));
                    }
                    let on = self.runners.values().filter(|r| r.running.iter().any(|x| x == id)).count();
                    let here = self.runners.get(runner).map_or(0, |r| r.running.iter().filter(|x| *x == id).count());
                    if on != 1 || here != 1 {
                        return Err(format!("{id} runs on {on} runners' lists, {here} times on {runner}'s"));
                    }
                }
                (_, Some(at)) => return Err(format!("{id} isn't running but its grant expires at {at}")),
                (_, None) => {}
            }
        }
        if indexed != self.leases {
            return Err(format!("the expiry index holds {:?}, the running legs' grants {:?}", self.leases, indexed));
        }
        self.check_reservations()
    }

    /// The reservations' invariants (W05-R39, G6's half in the engine): each is for items queued
    /// in its class, on runners connected and not draining, and their room is still left at its
    /// time: by the grants' own expected ends (a grant run past its end aside), on each of its
    /// runners its items fit there together beside what's expected to run then and the items of the
    /// classes before it kept there.
    #[cfg(test)]
    fn check_reservations(&self) -> Result<(), String> {
        for (class, k) in &self.reserved {
            for (leg, runner) in &k.items {
                if !self.legs.get(leg).is_some_and(|l| l.state == State::Queued && l.key.is_some_and(|(c, _, _)| c == *class)) {
                    return Err(format!("{class:?}'s reservation keeps room for {leg}, not queued in it"));
                }
                if !self.runners.get(runner).is_some_and(|r| r.connected && !r.draining) {
                    return Err(format!("{class:?}'s reservation keeps room for {leg} on {runner}, gone or draining"));
                }
            }
            for runner in k.items.iter().map(|(_, r)| r).collect::<BTreeSet<_>>() {
                let Some(r) = self.runners.get(runner) else { continue };
                let mut plus: Vec<&Leg> = self.reserved_on(runner, |c, _| c < *class).into_iter().filter(|(_, end)| *end > k.at).map(|(other, _)| other).collect();
                for item in k.legs_on(runner).filter_map(|id| self.legs.get(id)) {
                    let h = self.holding_with(r, |g| g.due.is_none_or(|d| Due::of(&g.input, d.from).expected > k.at), &plus);
                    if !self.room_in(r, &h, item) {
                        return Err(format!("{}'s room on {runner} at {} isn't left: it runs {:?}", item.input.id, k.at, r.running));
                    }
                    plus.push(item);
                }
            }
        }
        self.check_gangs()
    }

    /// G4 for gangs (W05-R40), after every input in tests: no gang runs in part. While any of the
    /// members a gang last started together runs, none of them waits to start, queued or to be
    /// tried again; and no member runs that it didn't start together.
    #[cfg(test)]
    fn check_gangs(&self) -> Result<(), String> {
        for (g, c) in &self.gangs {
            if let Some(m) = c.members.iter().find(|m| !c.round.contains(m.as_str()) && self.legs.get(m.as_str()).is_some_and(|l| matches!(l.state, State::Assigned { .. }))) {
                return Err(format!("gang {g}'s {m} runs, not started with it"));
            }
            let states: Vec<(&String, &State)> = c.round.iter().filter_map(|m| self.legs.get(m).map(|l| (m, &l.state))).collect();
            let running: Vec<&&String> = states.iter().filter(|(_, s)| matches!(s, State::Assigned { .. })).map(|(m, _)| m).collect();
            let waiting: Vec<&&String> = states.iter().filter(|(_, s)| matches!(s, State::Pending | State::Queued | State::Retrying { .. })).map(|(m, _)| m).collect();
            if !running.is_empty() && !waiting.is_empty() {
                return Err(format!("gang {g} runs in part: {running:?} run, {waiting:?} wait"));
            }
        }
        Ok(())
    }

    /// What runs where (each assigned leg's runner and attempt), each runner's legs as it counts
    /// them, and the legs queued: the reference engine's differential test compares and checks
    /// them (docs/needs.md E1.5).
    #[cfg(test)]
    pub(super) fn placement(&self) -> (BTreeMap<String, (String, u32)>, BTreeMap<String, Vec<String>>, BTreeSet<String>) {
        let (mut running, mut queued) = (BTreeMap::new(), BTreeSet::new());
        for (id, l) in self.legs.iter() {
            match &l.state {
                State::Assigned { runner } => {
                    running.insert(id.clone(), (runner.clone(), l.attempt));
                }
                State::Queued => {
                    queued.insert(id.clone());
                }
                _ => {}
            }
        }
        let on = self.runners.iter().map(|(id, r)| (id.clone(), r.running.clone())).collect();
        (running, on, queued)
    }
}

/// Where a leg stands among those waiting ([`Scheduler::standing`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Standing {
    pub priority: u8,
    pub intent: Option<(super::intent::Intent, String)>,
    pub actor: String,
    pub lane: String,
    /// Its class now (W05-R30), and the one it was queued in when it was promoted since (W05-R31).
    pub class: Class,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promoted_from: Option<Class>,
    /// Its tenant in fair share's tree, from its repository down (W28-R66, docs/needs.md E2.2).
    pub tenant: Vec<String>,
}

/// One runner in a routing explanation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explained {
    pub runner: String,
    pub name: String,
    pub connected: bool,
    pub eligible: bool,
    pub room: bool,
    pub score: Option<f64>,
    pub checks: Vec<Check>,
    /// For a leg with `warm` or `affinity` terms: the workspace it would start from here, and when
    /// it's expected to finish (W24-R18).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<Warmth>,
}

/// What one runner's warm state makes of a leg (W24-R18): the workspace it would start from, how
/// warm, and the expected seconds: waiting for room, running, fetching what it lacks, and the
/// finish they add up to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Warmth {
    #[serde(flatten)]
    pub estimate: warm::Estimate,
    pub wait_s: f64,
    pub finish_s: f64,
}

/// The milliseconds from `now` until `then` (unix ms), none when it's past.
fn ms_until(now: i64, then: i64) -> Ms {
    u64::try_from(then.saturating_sub(now)).unwrap_or(0)
}

/// A person's retry's placement as one more check of a runner, for explanations (W05-R28).
fn placement_check(p: &Placement, r: &Runner) -> Option<Check> {
    let (requirement, pass) = match (&p.runner, &p.avoid) {
        (Some(x), _) => (format!("runner == {x} (asked by its retry)"), r.input.id == *x),
        (None, Some(x)) => (format!("runner != {x} (asked by its retry)"), r.input.id != *x),
        (None, None) => return None,
    };
    Some(Check { requirement, value: format!("runner = {}", r.input.view.name), pass })
}

fn add(a: &mut Resources, b: &Resources) {
    a.cpu = a.cpu.saturating_add(b.cpu);
    a.mem = a.mem.saturating_add(b.mem);
    a.disk = a.disk.saturating_add(b.disk);
    a.gpu = a.gpu.saturating_add(b.gpu);
}

fn sub(a: &mut Resources, b: &Resources) {
    a.cpu = a.cpu.saturating_sub(b.cpu);
    a.mem = a.mem.saturating_sub(b.mem);
    a.disk = a.disk.saturating_sub(b.disk);
    a.gpu = a.gpu.saturating_sub(b.gpu);
}

/// The less loaded of two runners first: the smaller share of its slots running, compared
/// without dividing.
fn lighter(a: &Runner, b: &Runner) -> std::cmp::Ordering {
    let share = |r: &Runner, of: &Runner| u128::try_from(r.running.len()).unwrap_or(u128::MAX).saturating_mul(u128::from(of.input.slots.max(1)));
    share(a, b).cmp(&share(b, a))
}

fn free(r: &Runner) -> usize {
    (r.input.slots.max(1) as usize).saturating_sub(r.running.len())
}
