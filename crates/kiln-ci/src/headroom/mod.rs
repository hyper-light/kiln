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

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

pub use admit::{Class, Decision, Demand, Waiting};
pub use pace::Pace;

/// What a person decides (everything else is measured): how many false alarms a day they'll
/// accept, and how much of a served target's time its probes may take.
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
    /// It serves others (an HTTP or TCP target): its probes are held to the load share.
    pub served: bool,
}

/// What the engine learned, kept between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub detectors: BTreeMap<String, detect::Detector>,
    /// kiln's own writes a second.
    pub knee: knee::Knee,
    /// kiln's own CPU cores.
    #[serde(default)]
    pub cpu_knee: knee::Knee,
    pub demand: admit::Learned,
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
    decided_from: Option<(Option<u64>, Option<u64>, Vec<String>)>,
}

impl Engine {
    pub fn new(policy: Policy, targets: Vec<Target>, learned: Learned, now_us: u64) -> Self {
        let mut learned = learned;
        // Each target's detector: kept, or new; a changed tolerance starts it again.
        let mut detectors = BTreeMap::new();
        for t in &targets {
            let d = learned.detectors.remove(&t.name).filter(|d| d.tolerance_us == t.tolerance_us).unwrap_or_else(|| detect::Detector::new(t.tolerance_us, now_us));
            detectors.insert(t.name.clone(), d);
        }
        learned.detectors = detectors;
        Self { policy, targets, learned, promises: Vec::new(), runs: HashMap::new(), shares: Vec::new(), decided_from: None }
    }

    fn alarmed(&self) -> bool {
        self.learned.detectors.values().any(|d| d.alarm.is_some())
    }

    /// How long a level must hold calm to count as held: the slowest target's detection delay
    /// (its samples to alarm, at its probe interval) plus the longest a cut took to show.
    fn hold_us(&self) -> f64 {
        let lag = self.learned.knee.lag_us.unwrap_or(0.0);
        self.targets.iter().map(|t| self.interval(t) * self.learned.detectors.get(&t.name).map_or(1.0, |d| d.delay_samples(self.policy.false_alarms_per_day))).fold(0.0, f64::max) + lag
    }

    fn interval(&self, t: &Target) -> f64 {
        let d = self.learned.detectors.get(&t.name);
        let cost = d.and_then(detect::Detector::calm_us).unwrap_or(0.0);
        let delay = d.map_or(1.0, |d| d.delay_samples(self.policy.false_alarms_per_day));
        let service = t.served.then_some((cost, self.policy.probe_load_share));
        detect::interval_us(t.tolerance_us, self.learned.knee.lag_us.unwrap_or(0.0), delay, cost, service)
    }

    pub fn step(&mut self, o: &Observation) -> Plan {
        let now = o.now_us;
        let budget_per_day = self.policy.false_alarms_per_day;
        let mut events = Vec::new();

        // Unslowed demand of what runs: measured at full speed, or scaled up from its share.
        for g in &o.running {
            let r = self.runs.entry(g.id.clone()).or_insert_with(|| Run { key: g.key.clone(), since_us: now, peak_write: 0.0, peak_cpu: 0.0, peak_mem: 0.0, demand: 0.0, demand_cpu: 0.0 });
            if g.share >= 1.0 {
                r.demand = g.write_bps;
                r.demand_cpu = g.cpu_cores;
            } else if g.share > 0.0 {
                r.demand = g.write_bps / g.share;
                r.demand_cpu = g.cpu_cores / g.share;
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
        self.promises.retain(|p| p.until_us > now && !o.running.iter().any(|g| g.id == p.id));
        let quiet = o.running.iter().filter(|g| !g.protected).all(|g| g.write_bps <= 0.0 && g.cpu_cores <= 0.0);
        let wanted: f64 = o.running.iter().filter_map(|g| self.runs.get(&g.id)).map(|r| r.demand).sum();
        let wanted_cpu: f64 = o.running.iter().filter_map(|g| self.runs.get(&g.id)).map(|r| r.demand_cpu).sum();
        // What kiln's own work writes and uses now: what its budgets are measured against.
        let ours_w: f64 = o.running.iter().filter(|g| !g.protected).map(|g| g.write_bps).sum();
        let ours_c: f64 = o.running.iter().filter(|g| !g.protected).map(|g| g.cpu_cores).sum();

        // Detection.
        let was = self.alarmed();
        for (name, a) in &o.answers {
            let Some(d) = self.learned.detectors.get_mut(name) else { continue };
            let ev = match a {
                Answer::Took(us) => d.sample(now, *us, quiet, budget_per_day),
                Answer::Unavailable => detect::Event::Same,
            };
            match ev {
                detect::Event::Alarm(why) => events.push(format!("{name}: {why}")),
                detect::Event::Clear => events.push(format!("{name}: back to calm")),
                detect::Event::Regime(why) => {
                    events.push(format!("{name}: {why}"));
                    self.learned.knee.regime();
                    self.learned.cpu_knee.regime();
                }
                detect::Event::Same => {}
            }
        }
        for (name, age) in &o.inflight {
            if let Some(d) = self.learned.detectors.get_mut(name)
                && let detect::Event::Alarm(why) = d.inflight(now, *age, quiet)
            {
                events.push(format!("{name}: {why}"));
            }
        }
        // An alarm with kiln quiet the whole time, outlasting the longest any cut of kiln's took
        // to show: kiln isn't the cause; the machine changed. A new regime learns it.
        if let Some(lag) = self.learned.knee.lag_us {
            for (name, d) in &mut self.learned.detectors {
                if d.alarm.as_ref().is_some_and(|a| a.quiet && !a.harm && now.saturating_sub(a.since_us) as f64 > lag) {
                    d.restart(now);
                    events.push(format!("{name}: in trouble with kiln quiet, longer than any cut took to show: the machine changed, a new regime"));
                }
            }
        }

        // The budgets: both cut when trouble begins (either may be the cause), each raised again on
        // its own as it holds calm where it limits.
        let hold = self.hold_us();
        let now_alarmed = self.alarmed();
        use knee::Unit;
        if now_alarmed && !was {
            events.push(self.learned.knee.alarm(now, ours_w, Unit::Writes));
            events.push(self.learned.cpu_knee.alarm(now, ours_c, Unit::Cpu));
        } else if now_alarmed {
            events.extend(self.learned.knee.persisting(now, ours_w, hold, Unit::Writes));
            events.extend(self.learned.cpu_knee.persisting(now, ours_c, hold, Unit::Cpu));
        } else {
            if was {
                self.learned.knee.clear(now);
                self.learned.cpu_knee.clear(now);
            }
            let limited = self.learned.knee.budget.is_some_and(|b| wanted > b);
            events.extend(self.learned.knee.calm(now, ours_w, limited, hold, Unit::Writes));
            let limited_cpu = self.learned.cpu_knee.budget.is_some_and(|b| wanted_cpu > b);
            events.extend(self.learned.cpu_knee.calm(now, ours_c, limited_cpu, hold, Unit::Cpu));
        }
        let budget = self.learned.knee.budget;
        let cpu_budget = self.learned.cpu_knee.budget;

        // Shares: decided again only when the budget or the work running changed.
        let mut ids: Vec<String> = o.running.iter().map(|g| g.id.clone()).collect();
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
            lag_us: self.learned.knee.lag_us,
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
            vec![Target { name: "disk".into(), tolerance_us: 1e6, served: false }],
            Learned::default(),
            0,
        )
    }

    fn group(id: &str, w: f64, share: f64) -> Group {
        Group { id: id.into(), key: id.into(), class: Class::Agent, protected: false, write_bps: w, cpu_cores: 1.0, mem_bytes: 1e9, share }
    }

    #[test]
    fn harm_cuts_the_budget_paces_writers_and_says_why_and_learning_round_trips() -> anyhow::Result<()> {
        let mut e = engine();
        for i in 0..100u64 {
            e.step(&Observation { now_us: i * 1_000_000, answers: vec![("disk".into(), Answer::Took(4_000.0))], delivered_bps: Some(500e6), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
        }
        let p = e.step(&Observation { now_us: 101_000_000, answers: vec![("disk".into(), Answer::Took(2e6))], delivered_bps: Some(500e6), mem_free: 1e11, running: vec![group("a", 300e6, 1.0), group("b", 200e6, 1.0)], ..Observation::default() });
        anyhow::ensure!(p.events.iter().any(|e| e.contains("tolerance")), "{:?}", p.events);
        anyhow::ensure!(p.status.budget_bps == Some(250e6), "{:?}", p.status);
        anyhow::ensure!(p.shares.iter().any(|s| s.share < 1.0 && s.why.contains("slowed")), "{:?}", p.shares);
        let json = serde_json::to_string(&e.learned)?;
        let back: Learned = serde_json::from_str(&json)?;
        anyhow::ensure!(back == e.learned);
        Ok(())
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
