//! Headroom's decisions (docs/headroom.md), as pure code: the runner passes in what it measured
//! and when, and carries out what comes back. One [`Engine`] per machine:
//!
//! - [`detect`]: whether each protected target is in trouble (CUSUM, calibrated on the machine's
//!   own record), and how often to probe it;
//! - [`knee`]: the write and CPU budgets, each bisected between the last safe and the last unsafe
//!   level of kiln's own work (what headroom controls);
//! - [`admit`]: which waiting work starts, by learned demand, class and response ratio, with
//!   conservative backfilling;
//! - [`pace`]: each running group's share of time, water-filling the budget by class.
//!
//! Every decision carries its reason. What it learned serializes ([`Learned`]) for the runner to
//! keep, so a restart starts from it.

pub mod admit;
pub mod detect;
pub mod knee;
pub mod pace;
#[cfg(test)]
mod sim;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

pub use admit::{Class, Decision, Demand, Waiting};
pub use pace::Pace;

/// What a person decides (everything else is measured): how many false alarms a day they'll
/// accept, and how much of each target's time its probes may take.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    pub false_alarms_per_day: f64,
    pub probe_load_share: f64,
}

/// A target and how much it tolerates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub tolerance_us: f64,
    /// How soon trouble with it must be caught (µs), where that isn't its tolerance: a target that
    /// answers with a level it measured over time, not with how long the probe took (the kiln
    /// server's delay, kiln's docs/priority.md). Its probes are paced to this, and a probe still out
    /// this long is harm. None: its tolerance.
    #[serde(default)]
    pub window_us: Option<f64>,
    /// It comes first (kiln's own server's delay, docs/priority.md): a rise past its calm's noise is
    /// trouble, not only a shift toward its tolerance; a cut for it holds until it's back within that
    /// noise; and, as its latency is the CPU's, kiln's CPU is cut for it whatever the disks deliver.
    #[serde(default)]
    pub first: bool,
    /// The bytes one of this target's probes makes durable, where it's a sync of a file system's
    /// own (the kernel writes a whole page): with what the probe takes when calm, the least the
    /// disk under it is shown to take, on a machine whose disks don't say (`Engine::least_writes`).
    #[serde(default)]
    pub sync_bytes: Option<f64>,
}

/// The version of what's learned that this build reads as its own ([`Learned::version`]).
pub const LEARNED_VERSION: u32 = 3;

/// What the engine learned, kept between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    /// What build of the engine learned the budgets ([`LEARNED_VERSION`]): budgets an older one
    /// learned are let go of as they're read. Before version 1, trouble that wasn't kiln's was
    /// found out only once kiln's write budget reached exactly zero, which only Linux, knowing what
    /// its disks take, ever got to: on macOS and Windows each probe slowed by something else halved
    /// kiln's budgets for good, and the Macs' had come down to 0.03 cores and 164 kB/s for all their
    /// agents together (2026-11-05), which stopped them. Before version 2, trouble with almost
    /// nothing of kiln's in use cut its budgets to almost nothing, and from zero they never rose.
    /// Before version 3, kiln's CPU was cut for trouble whatever the CPU was doing (a Mac's agents
    /// held to 1 core of 18, with 8 idle), and how long a cut takes to show was kept from trouble
    /// that wasn't kiln's (45 minutes), which every raise of a budget waits out: both let go of.
    #[serde(default)]
    pub version: u32,
    pub detectors: BTreeMap<String, detect::Detector>,
    /// kiln's own writes a second.
    pub knee: knee::Knee,
    /// kiln's own CPU cores.
    #[serde(default)]
    pub cpu_knee: knee::Knee,
    pub demand: admit::Learned,
    /// What the machine delivered (bytes written a second) the last time trouble showed: with a
    /// queue built up, that is all its disks take, so kiln's budget is never raised past it less
    /// what others write. Unknown where the system doesn't say what it writes.
    #[serde(default)]
    pub capacity_bps: Option<f64>,
    /// What kiln wrote the last time stopping its writers didn't speed the probes up: the slowdown
    /// wasn't kiln's, and later trouble doesn't cut kiln while it writes no more than this.
    #[serde(default)]
    pub not_kiln: Option<NotKiln>,
}

/// What kiln wrote when stopping its writers didn't speed the probes up: bytes a second, and its
/// share of what the disks delivered where that's known.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NotKiln {
    pub write_bps: f64,
    pub share: Option<f64>,
}

impl NotKiln {
    /// Whether kiln writing `write_bps` (`share` of what the disks deliver) writes no more than
    /// this, both ways it is measured.
    fn covers(&self, write_bps: f64, share: Option<f64>) -> bool {
        write_bps <= self.write_bps && share.zip(self.share).is_none_or(|(s, t)| s <= t)
    }
}

/// The trouble under way: the budgets before it began, what kiln wrote then, and whether cutting
/// is held off (kiln shown not to be the cause, or not cut at all for that).
struct Episode {
    before: (knee::Knee, knee::Knee),
    write_bps: f64,
    /// Whether kiln's CPU was cut for it (the CPU was short).
    cpu_cut: bool,
    share: Option<f64>,
    held: bool,
}

impl Episode {
    /// Whether kiln's work is cut far enough that the trouble going on regardless isn't kiln's
    /// (LB05: replaces a chosen quarter, two halvings): what's left of its use (`write_bps`,
    /// `cpu_cores`, and the budgets that hold them) is below the midpoint of what the machine held
    /// calm before this trouble (`safe`: bisection's own next test of that range, and past it all
    /// that's left of kiln's is in a part of what held calm no level was learned wrong for), or
    /// down to the least a cut can change (`least_w`, `least_c`, nothing a further cut could
    /// show). Its write budget at zero is that too: nothing of kiln's writes at all. What was never
    /// cut ("too little in use", or a CPU that wasn't short) is no part of the question.
    fn cut_far(&self, write_bps: f64, cpu_cores: f64, write_budget: Option<f64>, cpu_budget: Option<f64>, least_w: f64, least_c: f64) -> bool {
        let far = |now: f64, budget: Option<f64>, before: &knee::Knee, least: f64| {
            budget.is_none_or(|b| b <= least) || now <= least || before.safe.is_some_and(|s| budget.map_or(now, |b| now.min(b)) < s / 2.0)
        };
        write_budget == Some(0.0) || (far(write_bps, write_budget, &self.before.0, least_w) && (!self.cpu_cut || far(cpu_cores, cpu_budget, &self.before.1, least_c)))
    }
}

/// A write budget as said: its rate, or no limit.
fn level(b: Option<f64>) -> String {
    b.map_or_else(|| "no limit".to_owned(), |b| knee::Unit::Writes.fmt(b))
}

/// ", n% of what the disks wrote", where that's known.
fn of_disks(share: Option<f64>) -> String {
    share.map(|s| format!(", {:.0}% of what the disks wrote", s * 100.0)).unwrap_or_default()
}

/// A running group of work, as measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    /// Its kind, for what's learned (a session, a command's shape).
    pub key: String,
    pub class: Class,
    pub protected: bool,
    /// Bytes written a second, as measured (slowed or not).
    pub write_bps: f64,
    /// CPU cores used, as measured (slowed or not).
    #[serde(default)]
    pub cpu_cores: f64,
    pub mem_bytes: f64,
    /// Its share of time when measured (1: full speed).
    pub share: f64,
}

/// One probe's answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Answer {
    Took(f64),
    /// No answer (refused, reset, not found): down for its own reasons, not counted.
    Unavailable,
    /// Answered with a failure after `us` (a 5xx, or a 429): failed service, however fast, counted
    /// as harm, a sample at the tolerance (LB05: a fast 503 can't look healthy,
    /// `crate::protect::objective::Outcome::met`).
    Failed(f64),
}

/// What the runner measured since it last asked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub now_us: u64,
    /// (target, answer).
    pub answers: Vec<(String, Answer)>,
    /// (target, how long its probe has been out).
    pub inflight: Vec<(String, f64)>,
    /// The machine's write rate (bytes a second), when the system says.
    pub delivered_bps: Option<f64>,
    /// The longest any disk written over the span sat idle (ms), when the system says (Linux's
    /// /proc/diskstats busy time): what's delivered while trouble shows is what the disks take only
    /// with every disk written busy throughout (`crate::protect::cause::at_capacity`; LB05 step 2).
    #[serde(default)]
    pub disks_idle_ms: Option<f64>,
    pub mem_free: f64,
    pub running: Vec<Group>,
    pub waiting: Vec<Waiting>,
    /// Groups whose run ended though the group goes on (a session's turn): learned from, and
    /// their next run starts from nothing.
    pub ended: Vec<String>,
    /// The machine's cores, where the system says: the CPU budget and any one group's CPU demand
    /// are held to them.
    #[serde(default)]
    pub cores: Option<f64>,
    /// The machine's cores idle since the last look, all processes counted, where the system says:
    /// with any idle, the CPU isn't short, and cutting kiln's CPU can't speed a target up.
    #[serde(default)]
    pub cpu_idle: Option<f64>,
}

/// What to do now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Plan {
    pub shares: Vec<Pace>,
    pub decisions: Vec<Decision>,
    /// How often to probe each target (µs).
    pub probe_every_us: BTreeMap<String, f64>,
    /// What changed, and why.
    pub events: Vec<String>,
    pub status: Status,
    /// When to step again with nothing else to tell (µs on the observations' clock): the soonest a
    /// probe out now would change a decision by its age alone (`Engine::due_us`). None: only new
    /// answers, probes going out, or work arriving can (LB05, G11: replaced a wake every probe
    /// interval).
    pub due_us: Option<u64>,
}

/// A target as the status shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TargetStatus {
    pub name: String,
    pub tolerance_us: f64,
    pub calm_us: Option<f64>,
    pub level: f64,
    pub threshold: f64,
    pub alarm: Option<detect::Alarm>,
    pub regime_since_us: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Status {
    pub targets: Vec<TargetStatus>,
    pub budget_bps: Option<f64>,
    pub safe_bps: Option<f64>,
    pub unsafe_bps: Option<f64>,
    pub cpu_budget: Option<f64>,
    pub cpu_safe: Option<f64>,
    pub cpu_unsafe: Option<f64>,
    pub lag_us: Option<f64>,
    /// The whole machine's write rate, when the system says (kiln's and everyone else's).
    pub delivered_bps: Option<f64>,
    /// What kiln's running work wants unslowed: writes a second, and CPU cores.
    pub wanted_bps: f64,
    pub wanted_cpu: f64,
}

/// A running group's history, for what's learned when it ends.
#[derive(Clone, Debug, PartialEq)]
struct Run {
    key: String,
    since_us: u64,
    peak_write: f64,
    peak_cpu: f64,
    peak_mem: f64,
    /// What it writes a second and the cores it uses unslowed, as last seen at full speed or
    /// scaled from its share.
    demand: f64,
    demand_cpu: f64,
}

pub struct Engine {
    pub policy: Policy,
    pub targets: Vec<Target>,
    pub learned: Learned,
    promises: Vec<admit::Promise>,
    runs: HashMap<String, Run>,
    /// Shares in force, and what they were decided from (they change only when that does).
    shares: Vec<Pace>,
    decided_from: Option<(Option<u64>, Option<u64>, Vec<(String, bool, Option<(u64, u64)>, Class)>)>,
    /// The most delivered during the trouble under way.
    trouble_bps: Option<f64>,
    /// The trouble under way, from when it began.
    episode: Option<Episode>,
    /// The last cut: when, and the first answer to a probe sent after it (its target, when it
    /// came, how long it took), which later probes of that target are measured against.
    cut: Option<(u64, Option<(String, u64, f64)>)>,
    /// The last cut a target that comes first has shown, by when it was made: its lag is when its
    /// first answer came back within calm's noise, not the last (its alarm holds a dwell after).
    shown_first: Option<u64>,
}

impl Engine {
    pub fn new(policy: Policy, targets: Vec<Target>, learned: Learned, now_us: u64) -> Self {
        let mut learned = learned;
        if learned.version < LEARNED_VERSION {
            // Budgets an older engine learned, which may have been cut for trouble that wasn't
            // kiln's, and how long its cuts took to show ([`Learned::version`]): let go of, and
            // learned again. What runs take is kept.
            learned.knee = knee::Knee::default();
            learned.cpu_knee = knee::Knee::default();
            learned.not_kiln = None;
            learned.version = LEARNED_VERSION;
        }
        // Each target's detector: kept, or new; a changed tolerance starts it again.
        let mut detectors = BTreeMap::new();
        for t in &targets {
            let mut d = learned.detectors.remove(&t.name).filter(|d| d.tolerance_us == t.tolerance_us).unwrap_or_else(|| detect::Detector::new(t.tolerance_us, now_us));
            d.window_us = t.window_us;
            d.first = t.first;
            // A target that comes first learned before its calm's spread was kept: what it learned
            // (calm with its rises in it) starts again.
            if t.first && d.spread().is_none() && d.calm_us().is_some() {
                d.restart(now_us);
            }
            detectors.insert(t.name.clone(), d);
        }
        learned.detectors = detectors;
        Self { policy, targets, learned, promises: Vec::new(), runs: HashMap::new(), shares: Vec::new(), decided_from: None, trouble_bps: None, episode: None, cut: None, shown_first: None }
    }

    /// Holds what's learned and estimated to what the machine has: its `cores`, and what its disks
    /// were measured to take.
    fn within_machine(&mut self, cores: Option<f64>) {
        let disks = self.learned.capacity_bps;
        self.learned.demand.within(cores, disks);
        if let Some(c) = cores {
            self.learned.cpu_knee.within(c);
        }
        if let Some(d) = disks {
            self.learned.knee.within(d);
        }
        for r in self.runs.values_mut() {
            if let Some(c) = cores {
                r.demand_cpu = r.demand_cpu.min(c);
                r.peak_cpu = r.peak_cpu.min(c);
            }
            if let Some(d) = disks {
                r.demand = r.demand.min(d);
                r.peak_write = r.peak_write.min(d);
            }
        }
    }

    fn alarmed(&self) -> bool {
        self.learned.detectors.values().any(|d| d.alarm.is_some())
    }

    /// Whether a target that comes first (`first`), or one that doesn't, is in trouble.
    fn alarmed_where(&self, first: bool) -> bool {
        self.targets.iter().filter(|t| t.first == first).any(|t| self.learned.detectors.get(&t.name).is_some_and(|d| d.alarm.is_some()))
    }

    /// The longest a cut of kiln's has taken to show, of its writes or its CPU (11-05: the CPU's
    /// alone was cut for the server's delay, and the writes' lag, read alone, stayed unknown).
    fn lag_us(&self) -> Option<f64> {
        match (self.learned.knee.lag_us, self.learned.cpu_knee.lag_us) {
            (Some(w), Some(c)) => Some(w.max(c)),
            (w, c) => w.or(c),
        }
    }

    /// The least the disks are shown to take where they don't say what they write (macOS):
    /// what a target that syncs a file system's own pages makes durable in its calm time (a
    /// lower bound on what they sustain).
    fn sync_bps(&self) -> Option<f64> {
        self.targets
            .iter()
            .filter_map(|t| {
                let calm = self.learned.detectors.get(&t.name)?.calm_us()?;
                t.sync_bytes.filter(|_| calm > 0.0).map(|b| b / calm * 1e6)
            })
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))))
    }

    /// The least a cut can change of kiln's writes on this machine (bytes a second): the pacer's
    /// step (a hundredth) of the most its disks are known to take, measured under trouble or at
    /// their calm sync pace, or delivering now; else of what its work wants: the pacer can
    /// apply nothing finer. LB05 (replaced 1 MB/s, chosen).
    fn least_writes(&self, o: &Observation, wanted: f64) -> f64 {
        let known = [self.learned.capacity_bps, self.sync_bps(), o.delivered_bps].into_iter().flatten().filter(|c| *c > 0.0).fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
        known.unwrap_or(wanted) / pace::SHARE_STEPS
    }

    /// The least a cut can change of kiln's CPU on this machine (cores): the pacer's step of its
    /// cores, the same that says the CPU is short (`cpu_short`); of what its work wants where
    /// they're unknown. LB05 (replaced 1 core, chosen).
    fn least_cpu(cores: Option<f64>, wanted_cpu: f64) -> f64 {
        cores.unwrap_or(wanted_cpu) / pace::SHARE_STEPS
    }

    /// Whether the CPU is short now, so cutting kiln's CPU could speed a target up: no core of the
    /// machine idle to within the pacer's step (a hundredth of its cores), all its processes
    /// counted, or the system doesn't say; or a target in trouble measures how late work is
    /// scheduled itself (one with a window of its own, or one that comes first: the server's delay),
    /// which kiln's CPU slows with cores to spare (docs/priority.md).
    fn cpu_short(&self, o: &Observation) -> bool {
        let scheduling = self.targets.iter().any(|t| (t.window_us.is_some() || t.first) && self.learned.detectors.get(&t.name).is_some_and(|d| d.alarm.is_some()));
        scheduling || o.cpu_idle.zip(o.cores).is_none_or(|(idle, cores)| idle < cores / pace::SHARE_STEPS)
    }

    /// How long a level must hold calm to count as held: the slowest target's detection delay
    /// (its samples to alarm, at its probe interval; for a target that comes first, a rise past
    /// calm's noise too) plus the longest a cut took to show.
    fn hold_us(&self) -> f64 {
        let lag = self.lag_us().unwrap_or(0.0);
        self.targets.iter().map(|t| self.interval(t) * self.learned.detectors.get(&t.name).map_or(1.0, |d| d.settle_samples(self.policy.false_alarms_per_day))).fold(0.0, f64::max) + lag
    }

    fn interval(&self, t: &Target) -> f64 {
        let d = self.learned.detectors.get(&t.name);
        let cost = d.and_then(detect::Detector::calm_us).unwrap_or(0.0);
        let delay = d.map_or(1.0, |d| d.delay_samples(self.policy.false_alarms_per_day));
        // What the target can afford of probes (LB05: derived, `Detector::probe_share`), under the
        // person's cap. Until it's known, probing faster wins no lag back: the target may not afford it.
        let afford = d.and_then(detect::Detector::probe_share);
        let share = afford.map_or(self.policy.probe_load_share, |a| a.min(self.policy.probe_load_share));
        let lag = if afford.is_some() { self.lag_us().unwrap_or(0.0) } else { 0.0 };
        detect::interval_us(t.window_us.unwrap_or(t.tolerance_us), lag, delay, cost, share)
    }

    /// The soonest a probe out now changes a decision by its age alone: when it would alarm
    /// (`Detector::inflight_due_us`), and, while kiln's work is cut, when it would be out no
    /// faster than the first answer since the cut (so the cut has shown and isn't draining it:
    /// `worse`). A due time for each such probe, computed from what's measured; none, none due.
    fn due_us(&self, o: &Observation) -> Option<u64> {
        let budget = self.policy.false_alarms_per_day;
        let first = self.cut.as_ref().and_then(|(_, f)| f.as_ref());
        let after = |name: &str, age: f64| -> Option<f64> {
            let d = self.learned.detectors.get(name)?;
            let alarm = d.inflight_due_us(age, budget);
            let worse = first.filter(|(n0, came, _)| n0 == name && o.now_us.saturating_sub(age as u64) >= *came).map(|(_, _, took)| (took.max(d.rises_at_us()) - age).max(0.0)).filter(|r| *r > 0.0);
            match (alarm, worse) {
                (Some(a), Some(w)) => Some(a.min(w)),
                (a, w) => a.or(w),
            }
        };
        o.inflight.iter().filter_map(|(n, age)| after(n, *age)).fold(None, |a: Option<f64>, r| Some(a.map_or(r, |a| a.min(r)))).map(|r| o.now_us.saturating_add(r.ceil() as u64).saturating_add(1))
    }

    pub fn step(&mut self, o: &Observation) -> Plan {
        let now = o.now_us;
        let budget_per_day = self.policy.false_alarms_per_day;
        let mut events = Vec::new();
        // What the machine has: no budget, and no demand of any one group or kind of work, is held
        // above its cores or above what its disks take.
        let cores = o.cores.filter(|c| *c > 0.0);
        self.within_machine(cores);
        let disks = self.learned.capacity_bps;
        let held = |v: f64, cap: Option<f64>| cap.map_or(v, |c| v.min(c));

        // Unslowed demand of what runs: measured at full speed, or scaled up from its share.
        for g in &o.running {
            // Until it's measured, what its kind was learned to want.
            let prior = self.learned.demand.demand(&g.key).unwrap_or_default();
            let r = self.runs.entry(g.id.clone()).or_insert_with(|| Run { key: g.key.clone(), since_us: now, peak_write: 0.0, peak_cpu: 0.0, peak_mem: 0.0, demand: held(prior.write_bps, disks), demand_cpu: held(prior.cpu_cores, cores) });
            let measured = g.write_bps > 0.0 || g.cpu_cores > 0.0;
            if !measured {
                // Not yet running anything measurable: the prior stands.
            } else if g.share >= 1.0 {
                r.demand = held(g.write_bps, disks);
                r.demand_cpu = held(g.cpu_cores, cores);
            } else if g.share > 0.0 {
                r.demand = held(g.write_bps / g.share, disks);
                r.demand_cpu = held(g.cpu_cores / g.share, cores);
            }
            r.peak_write = r.peak_write.max(r.demand);
            r.peak_cpu = r.peak_cpu.max(r.demand_cpu);
            r.peak_mem = r.peak_mem.max(g.mem_bytes);
        }
        // Runs that ended in groups that go on (a session's turn).
        for id in &o.ended {
            if let Some(r) = self.runs.get_mut(id) {
                self.learned.demand.record(&r.key, Demand { write_bps: r.peak_write, mem_bytes: r.peak_mem, duration_us: now.saturating_sub(r.since_us) as f64, cpu_cores: r.peak_cpu });
                *r = Run { key: r.key.clone(), since_us: now, peak_write: 0.0, peak_cpu: 0.0, peak_mem: 0.0, demand: r.demand, demand_cpu: r.demand_cpu };
            }
        }
        // What ended: learned from, and its promise kept no longer.
        let ended: Vec<String> = self.runs.keys().filter(|id| !o.running.iter().any(|g| &g.id == *id)).cloned().collect();
        for id in ended {
            if let Some(r) = self.runs.remove(&id) {
                self.learned.demand.record(&r.key, Demand { write_bps: r.peak_write, mem_bytes: r.peak_mem, duration_us: now.saturating_sub(r.since_us) as f64, cpu_cores: r.peak_cpu });
            }
        }
        // A promise holds until its work shows running, ends (it may never have got going), or its
        // expected run passes.
        self.promises.retain(|p| p.until_us > now && !o.running.iter().any(|g| g.id == p.id) && !o.ended.contains(&p.id));
        let quiet = o.running.iter().filter(|g| !g.protected).all(|g| g.write_bps <= 0.0 && g.cpu_cores <= 0.0);
        let wanted: f64 = o.running.iter().filter_map(|g| self.runs.get(&g.id)).map(|r| r.demand).sum();
        let wanted_cpu: f64 = o.running.iter().filter_map(|g| self.runs.get(&g.id)).map(|r| r.demand_cpu).sum();
        // What kiln's own work writes and uses now: what its budgets are measured against.
        let ours_w: f64 = o.running.iter().filter(|g| !g.protected).map(|g| g.write_bps).sum();
        let ours_c: f64 = o.running.iter().filter(|g| !g.protected).map(|g| g.cpu_cores).sum();
        let (least_w, least_c) = (self.least_writes(o, wanted), Self::least_cpu(cores, wanted_cpu));
        // The CPU's level: what kiln's work used, and no more than its budget let it. CPU goes to
        // whoever can run: what paced groups give up, unpaced ones take, so what kiln uses can stay
        // where it was while the pacer holds its work to less (11-05: 13-15 cores used under a
        // 7.1-core budget, taken as 14.3 cores held calm, which disproved the bound it was cut at).
        let ours_c_level = self.learned.cpu_knee.budget.map_or(ours_c, |b| ours_c.min(b));

        // Detection.
        let was = self.alarmed();
        let lag = self.lag_us();
        // What runs is held below what it wants: the shares in force as what's answering now was
        // measured.
        let paced = self.shares.iter().any(|p| p.share < 1.0);
        for d in self.learned.detectors.values_mut() {
            d.lag_us = lag;
            d.paced = paced;
        }
        // When what each answer measured began (a probe of a target with a window answers for the
        // time since its last): only what began after a cut can show it.
        let mut began: Vec<u64> = Vec::with_capacity(o.answers.len());
        for (name, a) in &o.answers {
            let Some(d) = self.learned.detectors.get_mut(name) else {
                began.push(if let Answer::Took(us) | Answer::Failed(us) = a { now.saturating_sub(*us as u64) } else { now });
                continue;
            };
            let ev = match a {
                Answer::Took(us) => {
                    began.push(d.began_us(now, *us));
                    d.sample(now, *us, quiet, budget_per_day)
                }
                Answer::Failed(us) => {
                    began.push(d.began_us(now, *us));
                    d.failed(now, *us, quiet, budget_per_day)
                }
                Answer::Unavailable => {
                    began.push(now);
                    detect::Event::Same
                }
            };
            match ev {
                detect::Event::Alarm(why) => events.push(format!("{name}: {why}")),
                detect::Event::Clear => events.push(format!("{name}: back to calm")),
                detect::Event::Regime(why) => {
                    events.push(format!("{name}: {why}"));
                    self.learned.knee.regime();
                    self.learned.cpu_knee.regime();
                    self.learned.not_kiln = None;
                }
                detect::Event::Same => {}
            }
        }
        for (name, age) in &o.inflight {
            if let Some(d) = self.learned.detectors.get_mut(name)
                && let detect::Event::Alarm(why) = d.inflight(now, *age, quiet, budget_per_day)
            {
                events.push(format!("{name}: {why}"));
            }
        }
        // The budgets: both cut when trouble begins (either may be the cause), each raised again on
        // its own as it holds calm where it limits.
        let hold = self.hold_us();
        let now_alarmed = self.alarmed();
        // Whether trouble is draining since the last cut. Probes of a target go one after another:
        // the first answer to one sent after the cut is the mark, and a later probe of it (sent
        // after that answer came) no faster says it isn't draining. Only what is itself trouble
        // (a sample the CUSUM counts as rising) says so.
        let rising = |n: &str, us: f64| self.learned.detectors.get(n).is_some_and(|d| d.rising(us));
        // What each answer says of the target's latency: a failure, however fast, as the tolerance.
        let tolerances: Vec<(String, f64)> = self.targets.iter().map(|t| (t.name.clone(), t.tolerance_us)).collect();
        let took = move |n: &str, a: &Answer| match a {
            Answer::Took(us) => Some(*us),
            Answer::Failed(_) => Some(tolerances.iter().find(|(t, _)| t == n).map_or(f64::INFINITY, |(_, tol)| *tol)),
            Answer::Unavailable => None,
        };
        let firsts: Vec<String> = self.targets.iter().filter(|t| t.first).map(|t| t.name.clone()).collect();
        let is_first = |n: &str| firsts.iter().any(|f| f == n);
        let mut worse = false;
        let mut shown = false;
        if let Some((at, first)) = &mut self.cut {
            for ((n, a), sent) in o.answers.iter().zip(began.iter().copied()) {
                let Some(us) = took(n, a) else { continue };
                let us = &us;
                if sent < *at {
                    continue;
                }
                if is_first(n) {
                    // A target that comes first: back within calm's noise, measured from after the
                    // cut, shows it, once; whether trouble persists, its own CUSUM says (one sample
                    // past calm's noise is noise as often as not).
                    if !rising(n, *us) && self.shown_first != Some(*at) {
                        self.shown_first = Some(*at);
                        shown = true;
                    }
                    continue;
                }
                if !rising(n, *us) {
                    // Calm again, sent after the cut: it has shown (and drained).
                    shown |= first.as_ref().is_none_or(|(n0, came, _)| n0 == n && sent >= *came);
                    continue;
                }
                match first {
                    None => *first = Some((n.clone(), now, *us)),
                    Some((n0, came, took)) if n == n0 && sent >= *came => {
                        worse |= us >= took;
                        shown |= us < took;
                    }
                    Some(_) => {}
                }
            }
            if let Some((n0, came, took)) = first {
                worse |= o.inflight.iter().any(|(n, age)| n == n0 && now.saturating_sub(*age as u64) >= *came && age >= took && rising(n, *age));
            }
        }
        // A target that comes first, still past calm's noise since kiln's last cut could show it.
        let persists = self.targets.iter().filter(|t| t.first).any(|t| self.learned.detectors.get(&t.name).is_some_and(|d| d.persists(budget_per_day)));
        // While trouble shows, a queue is built up and what's delivered is all the disks take: not
        // for a target that comes first, whose latency is the CPU's (11-05: a capacity read during
        // CPU trouble had a later alarm on the server's delay cut nothing, "the disks are the cause").
        let disk_trouble = self.alarmed_where(false);
        let first_trouble = self.alarmed_where(true);
        // What's delivered is what the disks take only with every disk written busy throughout: a disk
        // idle part of the span wasn't the bottleneck (a slow fsync with the CPU short, LB05 step 2).
        let saturated = crate::protect::cause::at_capacity(o.disks_idle_ms) == Some(true);
        if disk_trouble && saturated && let Some(d) = o.delivered_bps {
            self.trouble_bps = Some(self.trouble_bps.map_or(d, |t| t.max(d)));
        } else if !disk_trouble && let Some(t) = self.trouble_bps.take() {
            self.learned.capacity_bps = Some(t);
            self.within_machine(cores);
        }
        let tolerance = |name: &str| self.targets.iter().find(|t| t.name == name).map_or(f64::INFINITY, |t| t.tolerance_us);
        let others = o.delivered_bps.map_or(0.0, |d| (d - ours_w).max(0.0));
        // During trouble, what's delivered now is the freshest measure (the disks may have slowed).
        let capacity = self.trouble_bps.or(self.learned.capacity_bps);
        let cap_w = capacity.map(|c| c - others);
        // How far toward harm the worst target is now (its slowest answer or probe out, over its
        // tolerance): with the disks' capacity known, what's queued is that latency times it
        // (Little), and what drains it within the tolerance is the capacity less that and less
        // what others write.
        let worst: f64 = o
            .answers
            .iter()
            .filter(|(n, _)| !is_first(n))
            .filter_map(|(n, a)| took(n, a).map(|us| us / tolerance(n)))
            .chain(o.inflight.iter().filter(|(n, _)| !is_first(n)).map(|(n, age)| age / tolerance(n)))
            .fold(0.0, f64::max);
        let drain_w = capacity.map(|c| c * (1.0 - worst.min(1.0)) - others);
        if shown {
            self.learned.knee.shown(now);
            self.learned.cpu_knee.shown(now);
        }
        // The disks delivering all they were measured to take: they are the cause, not the CPU.
        let disks_full = matches!((o.delivered_bps, self.learned.capacity_bps), (Some(d), Some(c)) if d + wanted / pace::SHARE_STEPS >= c);
        use knee::Unit;
        // Whether kiln's work was cut this step (a target that comes first then counts its evidence
        // from when the cut can show).
        let mut cut_now = false;
        if now_alarmed && !was {
            // Trouble begins. If stopping kiln's writers didn't speed the probes up before, and kiln
            // writes no more now (in bytes a second and as a share of what the disks deliver),
            // cutting it again can't help either: it isn't cut.
            let share = o.delivered_bps.filter(|d| *d > 0.0).map(|d| (ours_w / d).min(1.0));
            let held = self.learned.not_kiln.as_ref().is_some_and(|n| n.covers(ours_w, share));
            // kiln's CPU is cut when it's short, and for a target that comes first whatever the disks
            // deliver (its latency is the CPU's: Target::first).
            let cpu_cut = !held && (first_trouble || (!disks_full && self.cpu_short(o)));
            self.episode = Some(Episode { before: (self.learned.knee.clone(), self.learned.cpu_knee.clone()), write_bps: ours_w, cpu_cut, share, held });
            if held {
                events.push(format!("kiln's writes not cut: it writes {}{}, no more than when stopping its writers last didn't speed the probes up", Unit::Writes.fmt(ours_w), of_disks(share)));
            } else {
                events.push(self.learned.knee.alarm(now, ours_w, drain_w, Unit::Writes, least_w));
                if cpu_cut {
                    events.push(self.learned.cpu_knee.alarm(now, ours_c_level, None, Unit::Cpu, least_c));
                } else if !disks_full && let Some(idle) = o.cpu_idle {
                    events.push(format!("kiln's CPU not cut: {} idle, so the CPU isn't what's short", Unit::Cpu.fmt(idle)));
                }
                self.cut = Some((now, None));
                cut_now = true;
            }
        } else if now_alarmed && self.episode.as_ref().is_some_and(|e| e.held) {
            // Shown not to be kiln's, or not cut for that: nothing more is cut until it clears.
        } else if now_alarmed
            && worse
            && let Some(ep) = self.episode.as_mut()
            && ep.cut_far(ours_w, ours_c, self.learned.knee.budget, self.learned.cpu_knee.budget, least_w, least_c)
        {
            // kiln's work is cut far down from where it was (its writers stopped, or its writes and
            // CPU each at a quarter or less), and a probe sent since is no faster than the one before
            // it: what's queued isn't kiln's. Its budgets go back to what they were before this
            // trouble, nothing is learned from the cuts, and what it wrote then is kept.
            let (writes, cpu) = ep.before.clone();
            self.learned.knee = writes;
            self.learned.cpu_knee = cpu;
            self.learned.not_kiln = Some(NotKiln { write_bps: ep.write_bps, share: ep.share });
            ep.held = true;
            events.push(format!(
                "kiln's writes back to {}: stopping them didn't speed the probes up, so the slowdown isn't kiln's (it wrote {}{})",
                level(self.learned.knee.budget),
                Unit::Writes.fmt(ep.write_bps),
                of_disks(ep.share)
            ));
            self.cut = None;
        } else if now_alarmed {
            let cut_w = self.learned.knee.persisting(now, ours_w, drain_w, worse, Unit::Writes, least_w);
            let cut_c = if !(first_trouble || (!disks_full && self.cpu_short(o))) {
                None
            } else if persists && !self.learned.cpu_knee.cutting() && ours_c_level > 0.0 {
                // Nothing of kiln's was in use when the trouble began, so nothing was cut; it runs now,
                // and the trouble persists past calm's noise: it's cut now.
                Some(self.learned.cpu_knee.alarm(now, ours_c_level, None, Unit::Cpu, least_c))
            } else {
                self.learned.cpu_knee.persisting(now, ours_c_level, None, worse || persists, Unit::Cpu, least_c)
            };
            if cut_w.is_some() || cut_c.is_some() {
                self.cut = Some((now, None));
                cut_now = true;
            }
            events.extend(cut_w);
            events.extend(cut_c);
        } else {
            self.cut = None;
            self.episode = None;
            if was {
                self.learned.knee.clear(now);
                self.learned.cpu_knee.clear(now);
            }
            // A budget limits when what runs wants more, or when work waiting would take it past.
            let next: Vec<Demand> = o.waiting.iter().filter_map(|w| self.learned.demand.demand(&w.key)).collect();
            let limited = self.learned.knee.budget.is_some_and(|b| wanted > b || next.iter().any(|d| wanted + d.write_bps > b));
            events.extend(self.learned.knee.calm(now, knee::Calm { delivered: ours_w, limited, hold_us: hold, resolution: wanted / pace::SHARE_STEPS, cap: cap_w }, Unit::Writes));
            let limited_cpu = self.learned.cpu_knee.budget.is_some_and(|b| wanted_cpu > b || next.iter().any(|d| wanted_cpu + d.cpu_cores > b));
            events.extend(self.learned.cpu_knee.calm(now, knee::Calm { delivered: ours_c_level, limited: limited_cpu, hold_us: hold, resolution: wanted_cpu / pace::SHARE_STEPS, cap: cores }, Unit::Cpu));
        }
        if cut_now {
            // What a target that comes first measures from when the cut can show says whether it
            // was enough: from now, plus the longest a cut has taken to show.
            let show = now.saturating_add(self.lag_us().unwrap_or(0.0) as u64);
            for t in self.targets.iter().filter(|t| t.first) {
                if let Some(d) = self.learned.detectors.get_mut(&t.name) {
                    d.cut(show);
                }
            }
        }
        let budget = self.learned.knee.budget;
        let cpu_budget = self.learned.cpu_knee.budget;

        // Shares: decided again only when the budget or the work running changed.
        // A group whose demand shows only once it has run (a CI job: nothing learned of its kind yet)
        // is decided again once it does: until then it's slowed for nothing (11-05: agents' turns
        // that started under a 14.7-core budget ran at 98-99% until the budget changed). Each group's
        // class too: a session that becomes a person's (something waits to reach its harness: kiln's
        // headroom/messages.rs) is let go at once, not at the next change of budget.
        // And each group's demand where it ran at full speed (LB05 step 2: a group whose demand changes under
        // the same id, an agent's turn that starts a build, is decided again, not held to the share its old
        // demand got). A paced group's demand is read through its share, and CPU it gives up goes to others
        // (11-05), so its estimate moves with the pacing itself: deciding again on it would chase that.
        let mut ids: Vec<(String, bool, Option<(u64, u64)>, Class)> = o
            .running
            .iter()
            .map(|g| {
                let r = self.runs.get(&g.id);
                let shows = r.is_some_and(|r| r.demand > 0.0 || r.demand_cpu > 0.0);
                let full = r.filter(|_| g.share >= 1.0).map(|r| (r.demand.to_bits(), r.demand_cpu.to_bits()));
                (g.id.clone(), shows, full, g.class)
            })
            .collect();
        ids.sort();
        let from = (budget.map(f64::to_bits), cpu_budget.map(f64::to_bits), ids);
        if self.decided_from.as_ref() != Some(&from) {
            let running: Vec<pace::Running> = o
                .running
                .iter()
                .map(|g| pace::Running {
                    id: g.id.clone(),
                    class: g.class,
                    demand_bps: self.runs.get(&g.id).map_or(g.write_bps, |r| r.demand),
                    demand_cpu: self.runs.get(&g.id).map_or(g.cpu_cores, |r| r.demand_cpu),
                    protected: g.protected,
                })
                .collect();
            self.shares = pace::plan(budget, cpu_budget, &running);
            self.decided_from = Some(from);
        }

        // Admission.
        let frees: Vec<admit::Free> = o
            .running
            .iter()
            .filter_map(|g| {
                let r = self.runs.get(&g.id)?;
                let d = self.learned.demand.demand(&r.key)?;
                Some(admit::Free { at_us: r.since_us.saturating_add(d.duration_us as u64), write_bps: r.demand, mem_bytes: g.mem_bytes, cpu_cores: r.demand_cpu })
            })
            .collect();
        let room = admit::Room { write_budget: budget, write_used: wanted, mem_free: o.mem_free, frees, cpu_budget, cpu_used: wanted_cpu };
        let decisions = admit::decide(now, &room, &self.promises, &o.waiting, &self.learned.demand);
        for d in decisions.iter().filter(|d| d.start) {
            let key = o.waiting.iter().find(|w| w.id == d.id).map(|w| w.key.clone()).unwrap_or_default();
            let demand = self.learned.demand.demand(&key);
            let until = demand.map_or(u64::MAX, |d| now.saturating_add(d.duration_us as u64));
            self.promises.push(admit::Promise { id: d.id.clone(), demand, until_us: until });
        }

        // Work started now is paced from its first moment, at what its kind was learned to want.
        let started: Vec<&Waiting> = decisions.iter().filter(|d| d.start).filter_map(|d| o.waiting.iter().find(|w| w.id == d.id)).collect();
        if !started.is_empty() {
            let running: Vec<pace::Running> = o
                .running
                .iter()
                .map(|g| pace::Running {
                    id: g.id.clone(),
                    class: g.class,
                    demand_bps: self.runs.get(&g.id).map_or(g.write_bps, |r| r.demand),
                    demand_cpu: self.runs.get(&g.id).map_or(g.cpu_cores, |r| r.demand_cpu),
                    protected: g.protected,
                })
                .chain(started.iter().map(|w| {
                    let d = self.learned.demand.demand(&w.key).unwrap_or_default();
                    pace::Running { id: w.id.clone(), class: w.class, demand_bps: d.write_bps, demand_cpu: d.cpu_cores, protected: false }
                }))
                .collect();
            self.shares = pace::plan(budget, cpu_budget, &running);
            // Decided again once they show as running.
            self.decided_from = None;
        }

        let probe_every_us = self.targets.iter().map(|t| (t.name.clone(), self.interval(t))).collect();
        let status = Status {
            targets: self
                .targets
                .iter()
                .filter_map(|t| {
                    let d = self.learned.detectors.get(&t.name)?;
                    Some(TargetStatus {
                        name: t.name.clone(),
                        tolerance_us: t.tolerance_us,
                        calm_us: d.calm_us(),
                        level: d.level(),
                        threshold: d.threshold(budget_per_day),
                        alarm: d.alarm.clone(),
                        regime_since_us: d.regime_since_us,
                    })
                })
                .collect(),
            budget_bps: budget,
            safe_bps: self.learned.knee.safe,
            unsafe_bps: self.learned.knee.unsafe_at,
            cpu_budget,
            cpu_safe: self.learned.cpu_knee.safe,
            cpu_unsafe: self.learned.cpu_knee.unsafe_at,
            lag_us: self.lag_us(),
            delivered_bps: o.delivered_bps,
            wanted_bps: wanted,
            wanted_cpu,
        };
        let due_us = self.due_us(o);
        Plan { shares: self.shares.clone(), decisions, probe_every_us, events, status, due_us }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn engine() -> Engine {
        Engine::new(
            Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 },
            vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: Some(4096.0) }],
            Learned::default(),
            0,
        )
    }

    fn group(id: &str, w: f64, share: f64) -> Group {
        Group { id: id.into(), key: id.into(), class: Class::Agent, protected: false, write_bps: w, cpu_cores: 1.0, mem_bytes: 1e9, share }
    }

    /// 2026-10-30: a cut lag learned on 10-28 (221 s), past the 1 s tolerance, had adacachy's
    /// runner probe production's server about 270 times a second and its disk back to back. A lag
    /// past the tolerance no longer speeds probing, and the disk is held to the load share too.
    #[test]
    fn a_lag_past_the_tolerance_leaves_probing_at_its_calm_pace() -> anyhow::Result<()> {
        let targets = vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: None }, Target { name: "server".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: None }];
        let run = |lag: Option<f64>| {
            let mut learned = Learned { version: LEARNED_VERSION, ..Learned::default() };
            learned.knee.lag_us = lag;
            let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, targets.clone(), learned, 0);
            let mut every = BTreeMap::new();
            for i in 0..100u64 {
                every = e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(9_200.0)), ("server".into(), Answer::Took(145.0))], mem_free: 1e11, ..Observation::default() }).probe_every_us;
            }
            (every, e.learned.knee.lag_us)
        };
        let (calm, _) = run(None);
        let (stale, kept) = run(Some(221e6));
        anyhow::ensure!(kept == Some(221e6), "the lag is still the one learned: {kept:?}");
        for name in ["disk", "server"] {
            let (c, s) = (calm.get(name).copied().unwrap_or(0.0), stale.get(name).copied().unwrap_or(0.0));
            anyhow::ensure!(c > 0.0 && (s - c).abs() <= c * 1e-9, "{name}: every {s} µs with the stale lag, {c} µs without");
        }
        // A 9.2 ms fsync takes at most a tenth of the disk's time.
        anyhow::ensure!(stale.get("disk").copied().unwrap_or(0.0) >= 92_000.0 * (1.0 - 1e-9), "disk probed every {:?} µs", stale.get("disk"));
        Ok(())
    }

    /// Goal 8 (plan 01a1a59c2b29 step 5, kiln's docs/priority.md): the kiln server's own delay as a
    /// target, as the lab measured it (2026-11-04, run 3): the mean lateness of its shards' probes
    /// between two reads, calm near 0.43 ms, its tolerance the shards' 10 ms period, caught within the
    /// platform's 1 s. With CI legs and agents busy on the CPU in its cgroup it rose to about 2.8 ms
    /// (1.9 to 4.5): the engine alarms within seconds, cuts kiln's CPU budget below what they use, and
    /// paces the leg and the agent alike; once the delay comes back down with them slowed, it's calm
    /// again, and the cut stays until the level holds. Its probes keep the window's pace, not the
    /// tolerance's.
    #[test]
    fn g8_s5_the_servers_delay_rising_slows_legs_and_agents() -> anyhow::Result<()> {
        let delay = "kiln server delay";
        let target = Target { name: delay.into(), tolerance_us: 10_000.0, window_us: Some(1e6), first: true, sync_bytes: None };
        let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![target], Learned::default(), 0);
        let running = |cores: f64, share: f64| vec![Group { cpu_cores: 8.0 * cores * share, ..group("job 01a1", 0.0, share) }, Group { cpu_cores: 4.0 * cores * share, ..group("01a1agent", 0.0, share) }];
        let at = |t: u64, us: f64, cores: f64, share: f64| Observation { now_us: t, answers: vec![(delay.into(), Answer::Took(us))], mem_free: 1e11, running: running(cores, share), cores: Some(32.0), ..Observation::default() };
        // Ten minutes calm, a read every 150 ms, the legs and agents idle: the run's spread, and every
        // two minutes a shard held 200 ms (a read's mean of 4 ms), which calibrates the threshold.
        let calm = [300.0, 410.0, 430.0, 380.0, 520.0, 680.0, 350.0, 450.0, 400.0, 1_020.0];
        let mut t = 0;
        let mut every = 0.0;
        for i in 0..4_000usize {
            t += 150_000;
            let us = if i % 800 == 799 { 4_000.0 } else { calm.get(i % calm.len()).copied().unwrap_or(430.0) };
            every = e.step(&at(t, us, 0.0, 1.0)).probe_every_us.get(delay).copied().unwrap_or(0.0);
        }
        anyhow::ensure!(e.learned.detectors.get(delay).is_some_and(|d| d.threshold(1.0) > 0.0), "calm's stalls calibrated nothing");
        anyhow::ensure!(every > 10_000.0, "probed every {every} µs: at its tolerance's pace, not its window's");
        // The legs and agents busy: the delay at the run's 1.9 to 4.5 ms.
        let busy = [2_600.0, 1_900.0, 4_500.0, 2_200.0, 3_100.0, 2_500.0, 2_900.0, 1_950.0];
        let mut alarmed = None;
        for i in 0..400usize {
            t += 150_000;
            let p = e.step(&at(t, busy.get(i % busy.len()).copied().unwrap_or(2_800.0), 1.0, 1.0));
            if p.events.iter().any(|ev| ev.starts_with(delay)) {
                alarmed = Some((i, p));
                break;
            }
        }
        let (n, p) = alarmed.ok_or_else(|| anyhow::anyhow!("no alarm on the server's delay in a minute of it at 2.8 ms"))?;
        anyhow::ensure!(n < 100, "the alarm took {n} reads ({} s)", n * 150 / 1000);
        anyhow::ensure!(p.status.cpu_budget.is_some_and(|b| b < 12.0), "kiln's CPU not cut below the 12 cores its work uses: {:?}", p.status);
        anyhow::ensure!(p.shares.len() == 2 && p.shares.iter().all(|s| s.share < 1.0), "the leg and the agent aren't both paced: {:?}", p.shares);
        let paced = p.shares.iter().map(|s| s.share).fold(1.0, f64::min);
        // Slowed, the server's delay comes back down.
        let mut back = false;
        for i in 0..400usize {
            t += 150_000;
            let p = e.step(&at(t, calm.get(i % calm.len()).copied().unwrap_or(430.0), 1.0, paced));
            back |= p.events.iter().any(|ev| ev.starts_with(delay) && ev.contains("back to calm"));
            anyhow::ensure!(p.status.cpu_budget.is_some_and(|b| b < 12.0), "the cut went before the level held: {:?}", p.status);
            if back {
                break;
            }
        }
        anyhow::ensure!(back, "the server's delay came back down and the alarm stayed");
        Ok(())
    }

    /// Goal 8 (plan 01a1a59c2b29 step 5): a probe of a target with a window is harm only once it's
    /// out the window, not at the tolerance (what it answers is what it measured), and the lab's
    /// milder rises, kiln.slice's weight's 1.4 to 1.8 times calm, aren't trouble.
    #[test]
    fn g8_s5_a_windowed_targets_probe_out_past_its_tolerance_is_no_harm_until_its_window() -> anyhow::Result<()> {
        let delay = "kiln server delay";
        let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![Target { name: delay.into(), tolerance_us: 10_000.0, window_us: Some(1e6), first: false, sync_bytes: None }], Learned::default(), 0);
        let legs = |cores: f64| vec![Group { cpu_cores: cores, ..group("job 01a1", 0.0, 1.0) }];
        let mut t = 0;
        for i in 0..2_000u64 {
            t += 150_000;
            e.step(&Observation { now_us: t, answers: vec![(delay.into(), Answer::Took(230.0 + (i % 9) as f64 * 25.0))], mem_free: 1e11, cores: Some(32.0), ..Observation::default() });
        }
        // The run's kiln.slice weight: the delay at 0.31 to 0.85 ms with 16 cores of legs beside it.
        for i in 0..400u64 {
            t += 150_000;
            let p = e.step(&Observation { now_us: t, answers: vec![(delay.into(), Answer::Took(310.0 + (i % 11) as f64 * 50.0))], mem_free: 1e11, running: legs(16.0), cores: Some(32.0), ..Observation::default() });
            anyhow::ensure!(p.events.is_empty() && p.status.cpu_budget.is_none(), "a rise the weights held made trouble: {:?}", p.events);
        }
        // A probe out twice the tolerance: no answer yet, and no harm.
        t += 150_000;
        let p = e.step(&Observation { now_us: t, inflight: vec![(delay.into(), 20_000.0)], mem_free: 1e11, running: legs(16.0), cores: Some(32.0), ..Observation::default() });
        anyhow::ensure!(p.events.is_empty(), "{:?}", p.events);
        // Out the whole window: harm.
        t += 150_000;
        let p = e.step(&Observation { now_us: t, inflight: vec![(delay.into(), 1e6)], mem_free: 1e11, running: legs(16.0), cores: Some(32.0), ..Observation::default() });
        anyhow::ensure!(p.events.iter().any(|ev| ev.contains("its window 1000 ms")), "{:?}", p.events);
        Ok(())
    }

    #[test]
    fn harm_cuts_the_budget_paces_writers_and_says_why_and_learning_round_trips() -> anyhow::Result<()> {
        let mut e = engine();
        for i in 0..100u64 {
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(4_000.0))], delivered_bps: Some(500e6), disks_idle_ms: Some(0.0), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
        }
        let p = e.step(&Observation { now_us: 101_000_000, answers: vec![("disk".into(), Answer::Took(2e6))], delivered_bps: Some(500e6), disks_idle_ms: Some(0.0), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
        anyhow::ensure!(p.events.iter().any(|e| e.contains("tolerance")), "{:?}", p.events);
        // Twice the tolerance with the disks delivering 500 MB/s: what's queued takes longer to
        // drain than the tolerance allows, so kiln's writers stop until it does.
        anyhow::ensure!(p.status.budget_bps == Some(0.0), "{:?}", p.status);
        anyhow::ensure!(p.shares.iter().all(|s| s.share == 0.0 && s.why.contains("stopped")), "{:?}", p.shares);
        let json = serde_json::to_string(&e.learned)?;
        let back: Learned = serde_json::from_str(&json)?;
        anyhow::ensure!(back == e.learned);
        Ok(())
    }

    #[test]
    fn trouble_kiln_cannot_cause_does_not_hold_its_work_back_for_good() {
        let mut e = engine();
        for i in 0..100u64 {
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(4_000.0))], mem_free: 1e11, running: vec![group("a", 300e6, 1.0)], ..Observation::default() });
        }
        // Harm: kiln's writers cut to nothing.
        let p = e.step(&Observation { now_us: 100_000_000, answers: vec![("disk".into(), Answer::Took(2e6))], delivered_bps: Some(300e6), mem_free: 1e11, running: vec![group("a", 300e6, 1.0)], ..Observation::default() });
        assert!(p.status.budget_bps.is_some_and(|b| b < 300e6), "{:?}", p.status);
        // Stopped, yet the disk stays slow (someone else's writes): after longer than a cut could
        // take to show, kiln's work runs again.
        let mut freed = false;
        for i in 101..200u64 {
            let p = e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(2e6))], delivered_bps: Some(300e6), mem_free: 1e11, running: vec![Group { cpu_cores: 0.0, ..group("a", 0.0, 0.0) }], ..Observation::default() });
            if p.status.budget_bps.is_none() {
                freed = true;
                assert!(p.events.iter().any(|e| e.contains("isn't kiln's")), "{:?}", p.events);
                break;
            }
        }
        assert!(freed, "kiln's work was held back for good by trouble it can't cause");
    }

    /// The 10-28 night on adacachy: other writers fill the disk while kiln writes 1% of it.
    /// Stopping kiln's writers doesn't speed the probes up, so they run again within 2 s with
    /// nothing learned, none of 50 later slowdowns with kiln writing no more cuts it, and once kiln
    /// writes half of what the disks take a cut is tried again.
    #[test]
    fn a_cut_that_does_not_help_is_undone_and_not_repeated() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, us: f64, kiln: f64, share: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: Some(500e6),
            disks_idle_ms: Some(0.0),
            mem_free: 1e11,
            running: vec![Group { cpu_cores: if share > 0.0 { 1.0 } else { 0.0 }, ..group("a", kiln, share) }],
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 4_000.0, 5e6, 1.0));
        }
        let before = e.learned.knee.clone();
        // Probes every second, at the 1 s tolerance.
        t += 1_000_000;
        let cut_at = t;
        let p = e.step(&at(t, 1e6, 5e6, 1.0));
        anyhow::ensure!(p.status.budget_bps == Some(0.0), "harm stops kiln's writers: {:?}", p.events);
        let mut back = None;
        for _ in 0..5 {
            t += 1_000_000;
            let p = e.step(&at(t, 1e6, 0.0, 0.0));
            if p.status.budget_bps.is_none() {
                anyhow::ensure!(p.events.iter().any(|e| e.contains("isn't kiln's")), "{:?}", p.events);
                back = Some(t);
                break;
            }
        }
        let back = back.ok_or_else(|| anyhow::anyhow!("kiln's writers never ran again"))?;
        anyhow::ensure!(back - cut_at <= 2_000_000, "restored {} s after the cut", (back - cut_at) as f64 / 1e6);
        let k = &e.learned.knee;
        anyhow::ensure!(k.safe == before.safe && k.unsafe_at == before.unsafe_at && k.lag_us == before.lag_us, "learned from a cut that didn't help: {k:?} (was {before:?})");
        // 50 more slowdowns, kiln writing no more: none cuts it.
        for n in 0..50 {
            let mut calm = 0;
            while e.alarmed() {
                t += 1_000_000;
                e.step(&at(t, 4_000.0, 5e6, 1.0));
                calm += 1;
                anyhow::ensure!(calm < 1_000, "slowdown {n} never cleared");
            }
            t += 1_000_000;
            let p = e.step(&at(t, 1e6, 5e6, 1.0));
            anyhow::ensure!(p.status.budget_bps.is_none() && p.events.iter().any(|e| e.contains("not cut")), "slowdown {n}: {:?}", p.events);
        }
        // kiln now writes half of what the disks take: a cut is tried again.
        let mut calm = 0;
        while e.alarmed() {
            t += 1_000_000;
            e.step(&at(t, 4_000.0, 250e6, 1.0));
            calm += 1;
            anyhow::ensure!(calm < 1_000, "never cleared");
        }
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, 250e6, 1.0));
        anyhow::ensure!(p.status.budget_bps.is_some(), "kiln writing half the disks' load is cut: {:?}", p.events);
        Ok(())
    }

    /// The Macs on 2026-11-05: macOS doesn't say what its disks write, so what they take is never
    /// known and a cut halves kiln's budgets without ever reaching zero. Something else keeps the
    /// probes slow while kiln writes 5 MB/s on 2 cores: once the cuts bring kiln's work down to a
    /// quarter and a probe since is no faster, the trouble isn't kiln's, its budgets go back to
    /// what they were (no limit), and nothing is learned from the cuts. Before, every such probe
    /// halved them again, for good: 0.03 cores and 164 kB/s for all of a Mac's agents.
    #[test]
    fn trouble_not_kilns_is_found_out_where_the_disks_dont_say_what_they_write() -> anyhow::Result<()> {
        let mut e = engine();
        // kiln's work as the pacer holds it to its budgets: all it wants (5 MB/s, 2 cores) within them.
        let at = |t: u64, us: f64, w: Option<f64>, c: Option<f64>| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: None,
            mem_free: 1e11,
            running: vec![Group { cpu_cores: c.map_or(2.0, |c| c.min(2.0)), ..group("a", w.map_or(5e6, |w| w.min(5e6)), 1.0) }],
            cores: Some(10.0),
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 4_000.0, None, None));
        }
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, None, None));
        anyhow::ensure!(p.status.budget_bps.is_some_and(|b| b < 5e6) && p.status.cpu_budget.is_some_and(|c| c < 2.0), "harm cuts kiln: {:?}", p.events);
        let (mut w, mut c) = (p.status.budget_bps, p.status.cpu_budget);
        let mut back = false;
        for _ in 0..10 {
            t += 1_000_000;
            let p = e.step(&at(t, 1e6, w, c));
            (w, c) = (p.status.budget_bps, p.status.cpu_budget);
            if p.events.iter().any(|e| e.contains("isn't kiln's")) {
                back = true;
                break;
            }
        }
        anyhow::ensure!(back, "trouble that isn't kiln's kept cutting it: {w:?} B/s, {c:?} cores");
        anyhow::ensure!(w.is_none() && c.is_none(), "kiln's budgets are back to no limit: {w:?} B/s, {c:?} cores");
        anyhow::ensure!(e.learned.knee.unsafe_at.is_none() && e.learned.cpu_knee.unsafe_at.is_none(), "learned from cuts that didn't help: {:?} {:?}", e.learned.knee, e.learned.cpu_knee);
        Ok(())
    }

    #[test]
    fn lb05_g11_a_probe_out_is_due_when_its_age_would_alarm_and_not_before() -> anyhow::Result<()> {
        let mut e = engine();
        let calm = |t: u64| Observation { now_us: t, answers: vec![("disk".into(), Answer::Took(4_000.0))], mem_free: 1e11, ..Observation::default() };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            anyhow::ensure!(e.step(&calm(t)).due_us.is_none(), "nothing out, nothing due");
        }
        // A probe out 1 ms: due when its age would alarm, after now and no later than its tolerance.
        t += 1_000_000;
        let out = |t: u64, age: f64| Observation { now_us: t, inflight: vec![("disk".into(), age)], mem_free: 1e11, ..Observation::default() };
        let due = e.step(&out(t, 1_000.0)).due_us.ok_or_else(|| anyhow::anyhow!("a probe out has no due time"))?;
        anyhow::ensure!(due > t && due <= t + 1_000_000, "due at {due}, now {t}");
        // Just before it's due, the same probe hasn't alarmed and is due at the same moment.
        let before = due - 2 - t;
        let p = e.step(&out(due - 2, 1_000.0 + before as f64));
        anyhow::ensure!(p.status.targets.iter().all(|t| t.alarm.is_none()), "alarmed before its due time: {:?}", p.events);
        anyhow::ensure!(p.due_us.is_some_and(|d| d.abs_diff(due) <= 2), "the due time moved: {:?} vs {due}", p.due_us);
        // At its due time it alarms, and nothing more is due of it.
        let p = e.step(&out(due + 1, 1_000.0 + (due + 1 - t) as f64));
        anyhow::ensure!(p.status.targets.iter().any(|t| t.alarm.is_some()), "due, and no alarm: {:?}", p.events);
        anyhow::ensure!(p.due_us.is_none(), "alarmed, yet due again: {:?}", p.due_us);
        Ok(())
    }

    #[test]
    fn lb05_g11_while_cut_a_probe_out_is_due_when_it_would_be_no_faster_than_the_first() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, answers: Vec<(String, Answer)>, inflight: Vec<(String, f64)>| Observation {
            now_us: t,
            answers,
            inflight,
            delivered_bps: None,
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 2.0, ..group("a", 5e6, 1.0) }],
            cores: Some(10.0),
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, vec![("disk".into(), Answer::Took(4_000.0))], vec![]));
        }
        // Harm cuts kiln's work.
        t += 1_000_000;
        let p = e.step(&at(t, vec![("disk".into(), Answer::Took(1e6))], vec![]));
        anyhow::ensure!(p.status.budget_bps.is_some(), "harm cut nothing: {:?}", p.events);
        // The first answer to a probe sent after the cut: 300 ms, still rising.
        t += 400_000;
        e.step(&at(t, vec![("disk".into(), Answer::Took(300_000.0))], vec![]));
        // A probe sent since, out 100 ms: due when it's been out 300 ms, no faster than the first.
        t += 150_000;
        let p = e.step(&at(t, vec![], vec![("disk".into(), 100_000.0)]));
        let due = p.due_us.ok_or_else(|| anyhow::anyhow!("a probe out while cut has no due time"))?;
        anyhow::ensure!(due > t && due <= t + 200_001, "due at {due}, now {t}: it reaches the first's 300 ms 200 ms from now");
        Ok(())
    }

    #[test]
    fn lb05_kilns_work_is_cut_far_past_the_midpoint_of_what_held_calm() {
        let held = |safe: Option<f64>| {
            let mut k = knee::Knee::default();
            k.safe = safe;
            k
        };
        let ep = |safe_w: Option<f64>, safe_c: Option<f64>, cpu_cut: bool| Episode { before: (held(safe_w), held(safe_c)), write_bps: 100e6, cpu_cut, share: None, held: false };
        let e = ep(Some(100e6), Some(8.0), true);
        // At the midpoint of what held calm isn't past it: bisection hasn't tried below it yet.
        assert!(!e.cut_far(50e6, 4.0, Some(50e6), Some(4.0), 1e5, 0.32), "one halving of 100 MB/s held calm");
        // Below it, writes and CPU alike.
        assert!(e.cut_far(49e6, 3.9, Some(49e6), Some(3.9), 1e5, 0.32));
        assert!(!e.cut_far(49e6, 4.0, Some(49e6), Some(4.0), 1e5, 0.32), "the CPU isn't past its midpoint");
        // What's left of it, measured or held: 80 MB/s measured, but held to 40.
        assert!(e.cut_far(80e6, 3.9, Some(40e6), Some(3.9), 1e5, 0.32));
        // Nothing held calm yet: the midpoint says nothing, the least a cut can change does.
        let unknown = ep(None, None, true);
        assert!(!unknown.cut_far(1e6, 1.0, Some(1e6), Some(1.0), 1e5, 0.32), "no level held calm, and well above the least");
        assert!(unknown.cut_far(1e6, 0.3, Some(1e6), Some(0.3), 1e6, 0.3), "budgets at the least");
        assert!(unknown.cut_far(9e4, 0.2, Some(1e6), Some(1.0), 1e5, 0.32), "what's left under the least, whatever it's held to");
        // Nothing of kiln's writes: enough, as before.
        assert!(unknown.cut_far(5e6, 1.0, Some(0.0), Some(1.0), 1e5, 0.32));
        // A CPU never cut for it is no part of the question.
        assert!(ep(Some(100e6), None, false).cut_far(49e6, 8.0, Some(49e6), None, 1e5, 0.32));
        // What was never cut (no budget): no part of the question either.
        assert!(ep(Some(100e6), Some(8.0), true).cut_far(49e6, 7.9, Some(49e6), None, 1e5, 0.32));
    }

    /// LB05: a cut's floor is a pacer's step of the machine, not 1 core and 1 MB/s: work using half
    /// a core of 10 and 500 kB/s, beside a disk whose page syncs in 8 ms, is cut, and not below it.
    #[test]
    fn lb05_work_above_a_pacers_step_of_the_machine_is_cut() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, us: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 0.5, ..group("a", 500e3, 1.0) }],
            cores: Some(10.0),
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 8_000.0));
        }
        t += 1_000_000;
        let p = e.step(&at(t, 1e6));
        let (w, c) = (p.status.budget_bps, p.status.cpu_budget);
        anyhow::ensure!(w.is_some_and(|w| (5_120.0..500e3).contains(&w)), "kiln's writes: {w:?}, {:?}", p.events);
        anyhow::ensure!(c.is_some_and(|c| (0.1..0.5).contains(&c)), "kiln's CPU: {c:?}, {:?}", p.events);
        Ok(())
    }

    #[test]
    fn lb05_the_least_a_cut_can_change_is_a_pacers_step_of_the_machine() {
        let mut e = engine();
        // Nothing known of the disks: a pacer's step of what the work wants.
        let idle = Observation { mem_free: 1e11, ..Observation::default() };
        assert_eq!(e.least_writes(&idle, 300e6), 3e6);
        // Delivering 200 MB/s: a pacer's step of that.
        let busy = Observation { delivered_bps: Some(200e6), ..idle.clone() };
        assert_eq!(e.least_writes(&busy, 1.0), 2e6);
        // Capacity measured under trouble: the most it's shown to take counts.
        e.learned.capacity_bps = Some(500e6);
        assert_eq!(e.least_writes(&busy, 1.0), 5e6);
        e.learned.capacity_bps = None;
        // The disk's own pace where it doesn't say (a sync of a page in 8 ms): 512 kB/s, a step of it.
        for i in 0..100u64 {
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(8_000.0))], mem_free: 1e11, ..Observation::default() });
        }
        assert!((e.least_writes(&idle, 1.0) - 5_120.0).abs() < 1.0, "{}", e.least_writes(&idle, 1.0));
        // Cores known: a step of them; unknown: of what the work wants.
        assert_eq!(Engine::least_cpu(Some(32.0), 7.0), 0.32);
        assert_eq!(Engine::least_cpu(None, 7.0), 0.07);
    }

    /// This Mac at 04:38 on 2026-11-05, with fresh budgets: its agents waiting on their models
    /// used about 0.01 cores and 5 kB/s between them, and one fsync took 330 ms against its usual
    /// 8 ms. That cut them to 0.0 cores and 0 MB/s, and every agent stopped. Now trouble with that
    /// little of kiln's in use cuts nothing (under a pacer's step of the machine: LB05), and work
    /// using more is never cut below that step, from which a budget can climb back.
    #[test]
    fn trouble_with_next_to_nothing_of_kilns_in_use_cuts_nothing() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, us: f64, w: f64, c: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: None,
            mem_free: 1e11,
            running: vec![Group { cpu_cores: c, ..group("a", w, 1.0) }],
            cores: Some(10.0),
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 8_000.0, 5e3, 0.01));
        }
        for _ in 0..5 {
            t += 1_000_000;
            let p = e.step(&at(t, 1e6, 5e3, 0.01));
            anyhow::ensure!(p.status.budget_bps.is_none() && p.status.cpu_budget.is_none(), "next to nothing of kiln's in use was cut: {:?}", p.events);
        }
        while e.alarmed() {
            t += 1_000_000;
            e.step(&at(t, 8_000.0, 5e3, 0.01));
        }
        // Work using 3 cores and 4 MB/s is cut, but not below a pacer's step of the machine: a
        // tenth of a core of 10, and a hundredth of the page an 8 ms sync makes durable: 5.12 kB/s.
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 8_000.0, 4e6, 3.0));
        }
        let mut least = (f64::INFINITY, f64::INFINITY);
        for _ in 0..10 {
            t += 1_000_000;
            let p = e.step(&at(t, 1e6, 4e6, 3.0));
            least = (least.0.min(p.status.budget_bps.unwrap_or(f64::INFINITY)), least.1.min(p.status.cpu_budget.unwrap_or(f64::INFINITY)));
        }
        anyhow::ensure!(least.0 >= 5_120.0 && least.1 >= 0.1, "cut below the least: {least:?}");
        anyhow::ensure!(least.0.is_finite() && least.1.is_finite(), "never cut at all: {least:?}");
        Ok(())
    }

    /// A session whose harness has something waiting for it is a person's while it waits (kiln's
    /// headroom/messages.rs): its share goes to all of it on the very next look, the budget
    /// unchanged, and back to its pace once nothing waits.
    #[test]
    fn a_session_with_a_message_waiting_runs_unpaced_at_once() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, us: f64, class: Class| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 4.0, class, ..group("s1", 50e6, 1.0) }, Group { cpu_cores: 4.0, ..group("s2", 50e6, 1.0) }],
            cores: Some(10.0),
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 4_000.0, Class::Agent));
        }
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, Class::Agent));
        let share = |p: &Plan, id: &str| p.shares.iter().find(|s| s.id == id).map_or(1.0, |s| s.share);
        anyhow::ensure!(share(&p, "s1") < 1.0, "harm paces the session: {:?}", p.shares);
        let budgets = (p.status.budget_bps, p.status.cpu_budget);
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, Class::Person));
        anyhow::ensure!((p.status.budget_bps, p.status.cpu_budget) == budgets, "the budget changed: {:?}", p.events);
        anyhow::ensure!(share(&p, "s1") >= 1.0, "a message waits, yet the session is paced: {:?}", p.shares);
        anyhow::ensure!(share(&p, "s2") < 1.0, "the session beside it is let go too: {:?}", p.shares);
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, Class::Agent));
        anyhow::ensure!(share(&p, "s1") < 1.0, "taken, it's paced again: {:?}", p.shares);
        Ok(())
    }

    /// The Mac at 06:01 on 2026-11-05: its disk's probe slowed while its agents used 1 core and 8 of
    /// its 18 sat idle, and kiln's CPU was cut to 1 core, which held five agents' builds, harnesses
    /// and all, stopped most of the time. With the CPU idle, cutting kiln's can't speed the probe
    /// up: the CPU isn't cut (its writes still are). With no core idle, it is.
    #[test]
    fn kilns_cpu_is_cut_only_when_the_cpu_is_short() -> anyhow::Result<()> {
        let run = |idle: f64| {
            let mut e = engine();
            let at = |t: u64, us: f64| Observation {
                now_us: t,
                answers: vec![("disk".into(), Answer::Took(us))],
                mem_free: 1e11,
                running: vec![Group { cpu_cores: 6.0, ..group("a", 20e6, 1.0) }],
                cores: Some(18.0),
                cpu_idle: Some(idle),
                ..Observation::default()
            };
            let mut t = 0;
            for _ in 0..100 {
                t += 1_000_000;
                e.step(&at(t, 4_000.0));
            }
            t += 1_000_000;
            e.step(&at(t, 1e6))
        };
        let p = run(8.0);
        anyhow::ensure!(p.status.cpu_budget.is_none() && p.events.iter().any(|e| e.contains("CPU not cut")), "the CPU was cut with 8 cores idle: {:?}", p.events);
        anyhow::ensure!(p.status.budget_bps.is_some(), "its writes are still cut: {:?}", p.events);
        let p = run(0.0);
        anyhow::ensure!(p.status.cpu_budget.is_some(), "with no core idle, kiln's CPU is cut: {:?}", p.events);
        Ok(())
    }

    /// Budgets an engine before [`LEARNED_VERSION`] learned are let go of as they're read (the
    /// Macs' 0.03 cores and 164 kB/s), and so is how long its cuts took to show (45 minutes, from
    /// trouble that wasn't kiln's, which every raise waited out).
    #[test]
    fn budgets_an_older_engine_learned_are_let_go_of() {
        let mut old = Learned::default();
        old.cpu_knee.budget = Some(0.027);
        old.cpu_knee.unsafe_at = Some(0.027);
        old.knee.budget = Some(164e3);
        old.knee.lag_us = Some(2.7e9);
        old.not_kiln = Some(NotKiln { write_bps: 1.0, share: None });
        let e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: None }], old, 0);
        assert_eq!((e.learned.knee.budget, e.learned.cpu_knee.budget, e.learned.cpu_knee.unsafe_at), (None, None, None));
        assert_eq!((e.learned.knee.lag_us, e.learned.not_kiln.clone(), e.learned.version), (None, None, LEARNED_VERSION));
        // One this build learned is kept.
        let mut mine = Learned { version: LEARNED_VERSION, ..Learned::default() };
        mine.cpu_knee.budget = Some(3.0);
        let e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![], mine, 0);
        assert_eq!(e.learned.cpu_knee.budget, Some(3.0));
    }

    /// The 10-26 crash's shape: kiln writes 90% of what the disks take. After the cut each probe
    /// comes back faster than the one before, so the cut stays and the rate trouble began at is
    /// learned, as before.
    #[test]
    fn a_cut_that_helps_stays() -> anyhow::Result<()> {
        let mut e = engine();
        let at = |t: u64, us: f64, kiln: f64, share: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: Some(500e6),
            disks_idle_ms: Some(0.0),
            mem_free: 1e11,
            running: vec![Group { cpu_cores: if share > 0.0 { 1.0 } else { 0.0 }, ..group("a", kiln, share) }],
            ..Observation::default()
        };
        let mut t = 0;
        for _ in 0..100 {
            t += 1_000_000;
            e.step(&at(t, 4_000.0, 450e6, 1.0));
        }
        t += 1_000_000;
        let p = e.step(&at(t, 1e6, 450e6, 1.0));
        anyhow::ensure!(p.status.budget_bps == Some(0.0), "{:?}", p.events);
        for us in [0.9e6, 0.6e6, 0.3e6, 0.1e6] {
            t += 1_000_000;
            let p = e.step(&at(t, us, 0.0, 0.0));
            anyhow::ensure!(p.status.budget_bps == Some(0.0) && !p.events.iter().any(|e| e.contains("isn't kiln's")), "{:?}", p.events);
        }
        anyhow::ensure!(e.learned.knee.unsafe_at == Some(450e6), "the rate trouble began at is learned: {:?}", e.learned.knee);
        anyhow::ensure!(e.learned.not_kiln.is_none(), "{:?}", e.learned.not_kiln);
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]
        /// However kiln's groups are paced, and whatever an earlier build learned, the CPU budget
        /// never exceeds the machine's cores, and no group's or kind of work's demand exceeds what
        /// the machine has (10-28: the CPU budget reached 4e13 cores on a 32-core machine).
        #[test]
        fn budgets_and_demands_stay_within_the_machine(
            cores in 1u32..=64,
            groups in prop::collection::vec((0.0f64..4.0, 1u32..=100), 1..300),
            inflated in 1e3f64..1e13,
        ) {
            let cores = f64::from(cores);
            let mut e = engine();
            // As an earlier build left it: demand learned from work measured at a tiny share, and a
            // CPU budget far past the machine.
            e.learned.demand.record("past", admit::Demand { write_bps: inflated * 1e6, mem_bytes: 1e6, duration_us: 1e6, cpu_cores: inflated });
            e.learned.cpu_knee.budget = Some(inflated);
            let running: Vec<Group> = groups
                .iter()
                .enumerate()
                .map(|(i, (cpu, steps))| {
                    let share = f64::from(*steps) / pace::SHARE_STEPS;
                    Group { id: format!("g{i}"), key: "past".into(), class: Class::Agent, protected: false, write_bps: 1e6 * share, cpu_cores: cpu * share, mem_bytes: 1e6, share }
                })
                .collect();
            let waiting = vec![Waiting { id: "w".into(), key: "past".into(), class: Class::Agent, since_us: 0 }];
            for i in 0..120u64 {
                // Two slowdowns: the budgets are cut, then raised again as they hold calm.
                let us = if i == 40 || i == 80 { 2e6 } else { 4_000.0 };
                let p = e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(us))], delivered_bps: Some(100e6), mem_free: 1e11, running: running.clone(), waiting: waiting.clone(), cores: Some(cores), ..Observation::default() });
                prop_assert!(p.status.cpu_budget.is_none_or(|b| b <= cores), "CPU budget {:?} over {} cores", p.status.cpu_budget, cores);
                prop_assert!(e.runs.values().all(|r| r.demand_cpu <= cores && r.peak_cpu <= cores), "a group's CPU demand over the cores");
                prop_assert!(e.learned.demand.demand("past").is_some_and(|d| d.cpu_cores <= cores), "learned CPU demand over the cores");
                if let Some(c) = e.learned.capacity_bps {
                    prop_assert!(e.runs.values().all(|r| r.demand <= c && r.peak_write <= c), "a group's write demand over what the disks take");
                    prop_assert!(e.learned.demand.demand("past").is_some_and(|d| d.write_bps <= c), "learned write demand over what the disks take");
                }
            }
        }
    }

    #[test]
    fn work_let_in_that_ends_without_running_frees_its_place() {
        let mut e = engine();
        let w = |id: &str| Waiting { id: id.into(), key: id.into(), class: Class::Agent, since_us: 0 };
        // Nothing learned: one unknown at a time.
        let p = e.step(&Observation { now_us: 1, mem_free: 1e11, waiting: vec![w("a"), w("b")], ..Observation::default() });
        assert_eq!(p.decisions.iter().filter(|d| d.start).map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["a"]);
        let p = e.step(&Observation { now_us: 2, mem_free: 1e11, waiting: vec![w("b")], ..Observation::default() });
        assert!(!p.decisions.iter().any(|d| d.start), "a still holds the one place");
        // a ended (its harness never started): b goes.
        let p = e.step(&Observation { now_us: 3, mem_free: 1e11, waiting: vec![w("b")], ended: vec!["a".into()], ..Observation::default() });
        assert_eq!(p.decisions.iter().filter(|d| d.start).map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["b"]);
    }

    /// LB05 step 2, the CPU-only counterexample: an fsync slow while the CPU is short and the disk sat
    /// idle most of the second (a journal thread starved, not a disk full). What the disks delivered
    /// then is no capacity: none is learned, and kiln's CPU is cut with its writes (either may be the
    /// cause). Learning it anyway, as before LB05, would hold kiln's writes to 50 MB/s for good.
    #[test]
    fn lb05_a_slow_fsync_with_the_disk_idle_learns_no_disk_capacity() {
        let mut e = engine();
        let at = |t: u64, us: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: Some(50e6),
            disks_idle_ms: Some(600.0),
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 30.0, ..group("a", 40e6, 1.0) }],
            cores: Some(32.0),
            cpu_idle: Some(0.0),
            ..Observation::default()
        };
        for i in 0..100u64 {
            e.step(&at(i * 1_000_000, 4_000.0));
        }
        let p = e.step(&at(100_000_000, 2e6));
        assert!(p.events.iter().any(|ev| ev.contains("kiln's CPU cut")), "the CPU was short: {:?}", p.events);
        for i in 101..400u64 {
            e.step(&at(i * 1_000_000, 4_000.0));
        }
        assert_eq!(e.learned.capacity_bps, None, "a disk idle 60% of the time was taken as full");
    }

    /// LB05 step 2, the disk-only counterexample: the disk busy throughout, the CPUs mostly idle.
    /// What it delivered is what it takes, and kiln's CPU isn't cut: with cores idle, cutting it
    /// can't speed the disk up.
    #[test]
    fn lb05_a_full_disk_with_cores_idle_learns_capacity_and_cuts_no_cpu() {
        let mut e = engine();
        let at = |t: u64, us: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            delivered_bps: Some(400e6),
            disks_idle_ms: Some(0.0),
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 2.0, ..group("a", 300e6, 1.0) }],
            cores: Some(32.0),
            cpu_idle: Some(20.0),
            ..Observation::default()
        };
        for i in 0..100u64 {
            e.step(&at(i * 1_000_000, 4_000.0));
        }
        let p = e.step(&at(100_000_000, 2e6));
        assert!(p.events.iter().any(|ev| ev.contains("kiln's CPU not cut")), "{:?}", p.events);
        assert!(p.status.cpu_budget.is_none(), "{:?}", p.status);
        for i in 101..400u64 {
            e.step(&at(i * 1_000_000, 4_000.0));
        }
        assert_eq!(e.learned.capacity_bps, Some(400e6));
    }

    /// LB05 step 1: injected fast rejection can't appear healthy. A target answering 503 in 200 µs is
    /// failed service, harm at once, said as a failure; before, a 5xx was "down", not counted.
    #[test]
    fn lb05_a_target_failing_fast_is_harm_not_health() {
        let mut e = engine();
        for i in 0..50u64 {
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(4_000.0))], mem_free: 1e11, running: vec![group("a", 300e6, 1.0)], ..Observation::default() });
        }
        let p = e.step(&Observation { now_us: 50_000_000, answers: vec![("disk".into(), Answer::Failed(200.0))], mem_free: 1e11, running: vec![group("a", 300e6, 1.0)], ..Observation::default() });
        assert!(p.events.iter().any(|ev| ev.contains("failed service")), "{:?}", p.events);
        assert!(p.status.targets.iter().any(|t| t.alarm.as_ref().is_some_and(|a| a.harm)), "{:?}", p.status.targets);
        assert!(p.status.budget_bps.is_some_and(|b| b < 300e6), "{:?}", p.status);
    }

    /// LB05 step 2: a group whose demand changes under the same id (an agent's turn that starts a
    /// build) is decided again: held to its old share, it would run at a fraction of the budget the
    /// pacer could give it.
    #[test]
    fn lb05_a_stable_group_whose_demand_changes_is_decided_again() {
        let mut e = engine();
        let at = |t: u64, us: f64, a: f64| Observation {
            now_us: t,
            answers: vec![("disk".into(), Answer::Took(us))],
            mem_free: 1e11,
            running: vec![Group { cpu_cores: 0.0, ..group("a", a, 1.0) }, Group { cpu_cores: 0.0, ..group("b", 100e6, 1.0) }],
            ..Observation::default()
        };
        for i in 0..100u64 {
            e.step(&at(i * 1_000_000, 4_000.0, 300e6));
        }
        // Harm: a budget below what both want.
        let p = e.step(&at(100_000_000, 2e6, 300e6));
        let budget = p.status.budget_bps;
        assert!(budget.is_some_and(|b| b < 400e6), "{:?}", p.status);
        let share_a = |p: &Plan| p.shares.iter().find(|s| s.id == "a").map(|s| s.share);
        let before = share_a(&p);
        // Same ids, same budget: a's demand falls to a tenth.
        let p = e.step(&at(101_000_000, 2e6, 30e6));
        assert_eq!(p.status.budget_bps, budget, "the budget moved: {:?}", p.events);
        assert!(share_a(&p) != before && share_a(&p) == Some(1.0), "a held to {before:?} with a tenth of its demand: {:?}", p.shares);
    }

    /// LB05 step 3: what a target can afford of probes is derived from its own answers, not borrowed
    /// (0.1, Kubernetes' period over its timeout): its idle share (fastest over calm) times
    /// 1 - e^-k, k its detector's own reference. A disk at 9 ms against a 1 s tolerance looks for a
    /// shift of half the way there; a target that comes first, for one of half calm's spread, so the
    /// steadier it is, the less it affords.
    #[test]
    fn lb05_the_probe_share_is_what_the_target_affords() {
        let feed = |base: f64, spread: f64, first: bool| {
            let mut d = detect::Detector::new(1e6, 0);
            d.first = first;
            for i in 0..400u64 {
                // A deterministic wobble of log-latency, its standard deviation about `spread`.
                let x = spread * std::f64::consts::SQRT_2 * ((i as f64) * 0.7).sin();
                d.sample(i * 1_000_000, base * x.exp(), false, 1.0);
            }
            d
        };
        let disk = feed(9_000.0, 0.5, false);
        let (calm, share) = (disk.calm_us().unwrap_or(0.0), disk.probe_share().unwrap_or(0.0));
        // The fastest answer is calm x e^(-sqrt 2 x 0.5) at the wobble's trough; the reference half of
        // ln(tolerance / calm).
        let fastest = 9_000.0 * (-(0.5f64 * std::f64::consts::SQRT_2)).exp();
        let k = (1e6f64.ln() - calm.ln()) / 2.0;
        let want = (fastest / calm).min(1.0) * (1.0 - (-k).exp());
        assert!((share - want).abs() <= want * 0.02, "share {share}, want {want} (calm {calm})");
        let (noisy, steady) = (feed(145.0, 0.3, true), feed(145.0, 0.05, true));
        let (sn, ss) = (noisy.probe_share().unwrap_or(0.0), steady.probe_share().unwrap_or(0.0));
        let want_steady = (-(0.05f64 * std::f64::consts::SQRT_2)).exp() * (1.0 - (-steady.spread().unwrap_or(0.0) / 2.0).exp());
        assert!((ss - want_steady).abs() <= want_steady * 0.05, "steady {ss}, want {want_steady}");
        assert!(ss < sn && sn < share, "a steadier target that comes first affords less: {ss} {sn} {share}");
        // Nothing learned of its spread: nothing derived, and no lag is won back by probing faster.
        assert_eq!(detect::Detector::new(1e6, 0).probe_share(), None);
        let mut learned = Learned { version: LEARNED_VERSION, ..Learned::default() };
        learned.knee.lag_us = Some(0.9e6);
        let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 1.0 }, vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: None }], learned, 0);
        let p = e.step(&Observation { now_us: 1, answers: vec![("disk".into(), Answer::Took(9_000.0))], mem_free: 1e11, ..Observation::default() });
        assert!(p.probe_every_us.get("disk").is_some_and(|every| *every >= 0.5e6 - 1.0), "probing sped up for a lag on a target that may not afford it: {:?}", p.probe_every_us);
        // Learned: the lag is won back only as far as the target affords.
        let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 1.0 }, vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false, sync_bytes: None }], Learned { detectors: [("disk".to_owned(), disk.clone())].into_iter().collect(), ..e.learned.clone() }, 0);
        let p = e.step(&Observation { now_us: 401_000_000, answers: vec![("disk".into(), Answer::Took(9_000.0))], mem_free: 1e11, ..Observation::default() });
        let every = p.probe_every_us.get("disk").copied().unwrap_or(0.0);
        assert!(every * share >= calm * (1.0 - 1e-6), "probes every {every} µs take more than the {share} the disk affords at {calm} µs each");
    }

    proptest! {
        /// Shares change only when the budgets, the work running, or a running group's demand does
        /// (LB05 step 2: a stable id with a changed demand is decided again).
        #[test]
        fn shares_change_only_when_the_budget_the_work_running_or_its_demand_does(
            noise in prop::collection::vec((1_000f64..60_000.0, 0f64..800e6), 50..300)
        ) {
            let mut e = engine();
            let mut last: Option<Vec<f64>> = None;
            let mut last_from = None;
            for (i, (lat, wr)) in noise.iter().enumerate() {
                let p = e.step(&Observation {
                    now_us: i as u64 * 1_000_000,
                    answers: vec![("disk".into(), Answer::Took(*lat))],
                    delivered_bps: Some(*wr),
                    mem_free: 1e11,
                    running: vec![group("a", *wr / 2.0, 1.0), group("b", *wr / 2.0, 1.0)],
                    ..Observation::default()
                });
                let shares: Vec<f64> = p.shares.iter().map(|s| s.share).collect();
                let from = (p.status.budget_bps, p.status.cpu_budget, p.status.wanted_bps.to_bits(), p.status.wanted_cpu.to_bits());
                if let Some(prev) = &last && *prev != shares {
                    prop_assert!(last_from != Some(from), "shares moved with the budgets and demands still {from:?}");
                }
                last = Some(shares);
                last_from = Some(from);
            }
        }
    }
}
