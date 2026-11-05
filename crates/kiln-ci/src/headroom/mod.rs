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
}

/// The version of what's learned that this build reads as its own ([`Learned::version`]).
pub const LEARNED_VERSION: u32 = 2;

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
    /// kiln's CPU cores when it began.
    cpu_cores: f64,
    share: Option<f64>,
    held: bool,
}

/// How far kiln's work must be cut from where it was when trouble began, in writes and in CPU
/// alike, before a probe no faster since shows the trouble isn't kiln's: to a quarter, two halvings
/// shown.
const NOT_KILN_CUT: f64 = 4.0;

impl Episode {
    /// Whether kiln's work, now writing `write_bps` on `cpu_cores`, is cut far enough from where it
    /// was when this trouble began ([`NOT_KILN_CUT`]) that the trouble going on regardless isn't
    /// kiln's. Its write budget at zero is that too: nothing of kiln's writes at all.
    fn cut_far(&self, write_bps: f64, cpu_cores: f64, write_budget: Option<f64>) -> bool {
        // Each cut to a quarter, or to the least a cut goes to.
        let far = |now: f64, then: f64, unit: knee::Unit| now * NOT_KILN_CUT <= then || now <= unit.least();
        write_budget == Some(0.0) || (far(write_bps, self.write_bps, knee::Unit::Writes) && far(cpu_cores, self.cpu_cores, knee::Unit::Cpu))
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
    /// Refused or failed: down for its own reasons, not counted.
    Unavailable,
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
    decided_from: Option<(Option<u64>, Option<u64>, Vec<(String, bool, Class)>)>,
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
            // kiln's ([`Learned::version`]): let go of, and learned again. What runs take is kept.
            learned.knee.regime();
            learned.cpu_knee.regime();
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
        detect::interval_us(t.window_us.unwrap_or(t.tolerance_us), self.lag_us().unwrap_or(0.0), delay, cost, self.policy.probe_load_share)
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
                began.push(if let Answer::Took(us) = a { now.saturating_sub(*us as u64) } else { now });
                continue;
            };
            let ev = match a {
                Answer::Took(us) => {
                    began.push(d.began_us(now, *us));
                    d.sample(now, *us, quiet, budget_per_day)
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
        let firsts: Vec<String> = self.targets.iter().filter(|t| t.first).map(|t| t.name.clone()).collect();
        let is_first = |n: &str| firsts.iter().any(|f| f == n);
        let mut worse = false;
        let mut shown = false;
        if let Some((at, first)) = &mut self.cut {
            for ((n, a), sent) in o.answers.iter().zip(began.iter().copied()) {
                let Answer::Took(us) = a else { continue };
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
        if disk_trouble && let Some(d) = o.delivered_bps {
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
            .filter_map(|(n, a)| if let Answer::Took(us) = a { Some(us / tolerance(n)) } else { None })
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
            self.episode = Some(Episode { before: (self.learned.knee.clone(), self.learned.cpu_knee.clone()), write_bps: ours_w, cpu_cores: ours_c, share, held });
            if held {
                events.push(format!("kiln's writes not cut: it writes {}{}, no more than when stopping its writers last didn't speed the probes up", Unit::Writes.fmt(ours_w), of_disks(share)));
            } else {
                events.push(self.learned.knee.alarm(now, ours_w, drain_w, Unit::Writes));
                if !disks_full || first_trouble {
                    events.push(self.learned.cpu_knee.alarm(now, ours_c_level, None, Unit::Cpu));
                }
                self.cut = Some((now, None));
                cut_now = true;
            }
        } else if now_alarmed && self.episode.as_ref().is_some_and(|e| e.held) {
            // Shown not to be kiln's, or not cut for that: nothing more is cut until it clears.
        } else if now_alarmed
            && worse
            && let Some(ep) = self.episode.as_mut()
            && ep.cut_far(ours_w, ours_c, self.learned.knee.budget)
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
            let cut_w = self.learned.knee.persisting(now, ours_w, drain_w, worse, Unit::Writes);
            let cut_c = if disks_full && !first_trouble {
                None
            } else if persists && !self.learned.cpu_knee.cutting() && ours_c_level > 0.0 {
                // Nothing of kiln's was in use when the trouble began, so nothing was cut; it runs now,
                // and the trouble persists past calm's noise: it's cut now.
                Some(self.learned.cpu_knee.alarm(now, ours_c_level, None, Unit::Cpu))
            } else {
                self.learned.cpu_knee.persisting(now, ours_c_level, None, worse || persists, Unit::Cpu)
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
        let mut ids: Vec<(String, bool, Class)> = o.running.iter().map(|g| (g.id.clone(), self.runs.get(&g.id).is_some_and(|r| r.demand > 0.0 || r.demand_cpu > 0.0), g.class)).collect();
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
        Plan { shares: self.shares.clone(), decisions, probe_every_us, events, status }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn engine() -> Engine {
        Engine::new(
            Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 },
            vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false }],
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
        let targets = vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None, first: false }, Target { name: "server".into(), tolerance_us: 1e6, window_us: None, first: false }];
        let run = |lag: Option<f64>| {
            let mut learned = Learned::default();
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
        let target = Target { name: delay.into(), tolerance_us: 10_000.0, window_us: Some(1e6), first: true };
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
        let mut e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![Target { name: delay.into(), tolerance_us: 10_000.0, window_us: Some(1e6), first: false }], Learned::default(), 0);
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
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(4_000.0))], delivered_bps: Some(500e6), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
        }
        let p = e.step(&Observation { now_us: 101_000_000, answers: vec![("disk".into(), Answer::Took(2e6))], delivered_bps: Some(500e6), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
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

    /// This Mac at 04:38 on 2026-11-05, with fresh budgets: its agents waiting on their models
    /// used about 0.01 cores and 5 kB/s between them, and one fsync took 330 ms against its usual
    /// 8 ms. That cut them to 0.0 cores and 0 MB/s, and every agent stopped. Now trouble with that
    /// little of kiln's in use cuts nothing, and work using more is never cut below 1 core or
    /// 1 MB/s, from which a budget can climb back.
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
        // Work using 3 cores and 4 MB/s is cut, but not below 1 core and 1 MB/s.
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
        anyhow::ensure!(least.0 >= 1e6 && least.1 >= 1.0, "cut below the least: {least:?}");
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

    /// Budgets an engine before [`LEARNED_VERSION`] learned are let go of as they're read (the
    /// Macs' 0.03 cores and 164 kB/s), and how long a cut takes to show is kept.
    #[test]
    fn budgets_an_older_engine_learned_are_let_go_of() {
        let mut old = Learned::default();
        old.cpu_knee.budget = Some(0.027);
        old.cpu_knee.unsafe_at = Some(0.027);
        old.knee.budget = Some(164e3);
        old.knee.lag_us = Some(2.7e9);
        old.not_kiln = Some(NotKiln { write_bps: 1.0, share: None });
        let e = Engine::new(Policy { false_alarms_per_day: 1.0, probe_load_share: 0.1 }, vec![Target { name: "disk".into(), tolerance_us: 1e6, window_us: None }], old, 0);
        assert_eq!((e.learned.knee.budget, e.learned.cpu_knee.budget, e.learned.cpu_knee.unsafe_at), (None, None, None));
        assert_eq!((e.learned.knee.lag_us, e.learned.not_kiln.clone(), e.learned.version), (Some(2.7e9), None, LEARNED_VERSION));
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

    proptest! {
        #[test]
        fn shares_change_only_when_the_budget_or_the_work_running_does(
            noise in prop::collection::vec((1_000f64..60_000.0, 0f64..800e6), 50..300)
        ) {
            let mut e = engine();
            let mut last: Option<Vec<f64>> = None;
            let mut last_budget = None;
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
                let budgets = (p.status.budget_bps, p.status.cpu_budget);
                if let Some(prev) = &last && *prev != shares {
                    prop_assert!(last_budget != Some(budgets), "shares moved with the budgets still {budgets:?}");
                }
                last = Some(shares);
                last_budget = Some(budgets);
            }
        }
    }
}
