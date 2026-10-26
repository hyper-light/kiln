//! Admission by load (docs/ci.md W24-R19): heavy legs are held off a machine whose disk is under
//! pressure or nearly full, so one build doesn't starve what else runs there (the builds of
//! 2026-10-19 starved a disk, not a CPU). A runner reports its I/O pressure (Linux's PSI, `full
//! avg10`: [`crate::facts::WorkFacts`]) and its work volume's free disk ([`crate::facts::Disk`]) in
//! its facts. A leg is heavy when its job says so (`resources: { io: heavy }`) or its history does
//! (its job's legs lately took `heavy_after` or longer); a heavy leg starts only on a runner within
//! the limits, and waits, saying which runners it's held off and why ([`Held`], the scheduler's
//! `WaitReason::Load`); other legs go anywhere. The pressure limit honours the leg's I/O weight on
//! the `Sandbox` seam's scale (cgroup v2's `io.weight`, 1 to 10,000, 100 an even share; the job's
//! `resources: { io-weight }`): a leg that yields the disk (a lower weight) may start under more
//! pressure, one that takes more of it (a higher weight) under less. Off until
//! `[ci.scheduler.admission]` is set; Hex's placement takes it over (CP-T7).

use serde::Serialize;

use crate::facts::RunnerFacts;
use crate::wf::ir::YVal;

/// The `Sandbox` seam's weight scale (`kiln_seams::sandbox::WEIGHT_MIN` to `WEIGHT_MAX`) and its
/// even share; the kiln crate holds them to the seam's.
pub const WEIGHT_MIN: u16 = 1;
pub const WEIGHT_MAX: u16 = 10_000;
pub const WEIGHT_EVEN: u16 = 100;

/// `[ci.scheduler.admission]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    /// The I/O pressure, in percent (`full avg10`), past which a heavy leg of an even I/O weight
    /// isn't started on a runner.
    pub io_full: f64,
    /// The work volume's free bytes under which no heavy leg is started there.
    pub disk_free: u64,
    /// The seconds a job's legs lately took (their median, W19) from which its legs count as heavy;
    /// `None`: only legs whose job says so are.
    pub heavy_after: Option<f64>,
}

/// What admission knows of a leg.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Load {
    /// Its job says it's heavy on I/O (`resources: { io: heavy }`).
    pub heavy: bool,
    /// Its I/O weight on the seam's scale (`resources: { io-weight }`); 0 for the even share.
    pub io_weight: u16,
}

/// What a job's `resources:` says of its legs' I/O: `io: heavy` (or `light`, the default) and
/// `io-weight`, a weight on the `Sandbox` seam's scale. Anything else is left as the default.
pub fn load_of(resources: Option<&YVal>) -> Load {
    let Some(r) = resources else { return Load::default() };
    let heavy = r.get("io").and_then(YVal::as_str).is_some_and(|s| s.trim().eq_ignore_ascii_case("heavy"));
    let weight = match r.get("io-weight") {
        Some(YVal::Num(n)) => Some(*n),
        Some(YVal::Str(t)) => t.src.trim().parse::<f64>().ok(),
        _ => None,
    };
    let io_weight = weight.filter(|w| w.is_finite() && *w >= 1.0).map_or(0, |w| w.min(f64::from(WEIGHT_MAX)).round() as u16);
    Load { heavy, io_weight }
}

/// Why a leg counts as heavy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "by", rename_all = "kebab-case")]
pub enum Heavy {
    Declared,
    /// Its job's legs lately took this many seconds.
    Learned { seconds: u64 },
}

/// Whether a leg is heavy: declared, else learned from `cost` (its job's median seconds lately).
pub fn heavy(cfg: &Admission, load: &Load, cost: Option<f64>) -> Option<Heavy> {
    if load.heavy {
        return Some(Heavy::Declared);
    }
    let (after, cost) = (cfg.heavy_after?, cost?);
    (cost.is_finite() && cost >= after).then(|| Heavy::Learned { seconds: cost.max(0.0).round() as u64 })
}

/// The I/O pressure, in hundredths of a percent, past which a leg of I/O weight `weight` (0: even)
/// isn't started: the configured limit times the even share over its weight, at most 100 %.
pub fn limit(cfg: &Admission, weight: u16) -> u32 {
    let w = if weight == 0 { WEIGHT_EVEN } else { weight.clamp(WEIGHT_MIN, WEIGHT_MAX) };
    let pct = if cfg.io_full.is_finite() { (cfg.io_full * f64::from(WEIGHT_EVEN) / f64::from(w)).clamp(0.0, 100.0) } else { 100.0 };
    (pct * 100.0).round() as u32
}

/// A runner a heavy leg is held off, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Held {
    pub runner: String,
    /// Its I/O pressure (`full avg10`), in hundredths of a percent; `None` when it doesn't say.
    pub pressure: Option<u32>,
    /// The leg's pressure limit there, in the same unit.
    pub limit: u32,
    /// Its work volume's free bytes, and the floor under which no heavy leg starts.
    pub free: u64,
    pub floor: u64,
}

/// Whether a heavy leg of I/O weight `weight` may start on `runner` (its facts `facts`) now: else
/// which limit it's past. A runner that says nothing of its pressure, or of its disk, isn't held
/// for it.
pub fn admit(cfg: &Admission, runner: &str, weight: u16, facts: &RunnerFacts) -> Result<(), Held> {
    let limit = limit(cfg, weight);
    let pressure = facts.work.as_ref().and_then(|w| w.io_full).filter(|p| p.is_finite()).map(|p| (p.clamp(0.0, 100.0) * 100.0).round() as u32);
    let over = pressure.is_some_and(|p| p > limit);
    let low = facts.disk.total > 0 && facts.disk.free < cfg.disk_free;
    if over || low { Err(Held { runner: runner.to_owned(), pressure, limit, free: facts.disk.free, floor: cfg.disk_free }) } else { Ok(()) }
}

/// What holds a leg (`load`, its job's median seconds `cost`) off `runner` now: nothing when
/// admission is off (`cfg`), when the leg isn't heavy, or when the runner is within the limits.
pub fn held(cfg: Option<&Admission>, load: &Load, cost: Option<f64>, runner: &str, facts: &RunnerFacts) -> Option<Held> {
    let cfg = cfg?;
    heavy(cfg, load, cost)?;
    admit(cfg, runner, load.io_weight, facts).err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{Disk, WorkFacts};
    use crate::sched::routing::{LegRoute, RunnerView};
    use crate::sched::sim::Sim;
    use crate::sched::state::{Config, Input, LegIn, RunnerIn, WaitReason};
    use crate::wf::ir::Tpl;

    const GIB: u64 = 1 << 30;
    const MIN: i64 = 60_000;

    fn cfg() -> Admission {
        Admission { io_full: 40.0, disk_free: 20 * GIB, heavy_after: Some(600.0) }
    }

    fn facts(pressure: Option<f64>, free: u64) -> RunnerFacts {
        RunnerFacts { os: "linux".into(), arch: "x86_64".into(), disk: Disk { total: 1_000 * GIB, free }, work: Some(WorkFacts { io_full: pressure }), ..RunnerFacts::default() }
    }

    fn runner(name: &str, pressure: Option<f64>, free: u64) -> RunnerIn {
        RunnerIn { id: name.into(), view: RunnerView { name: name.into(), labels: vec!["k8s".into()], facts: facts(pressure, free), caps: 1 << 10, pool: None }, slots: 4, capacity: None, executor: None }
    }

    fn leg(id: &str, load: Load, cost: Option<f64>) -> LegIn {
        LegIn { id: id.into(), run: format!("run-{id}"), repo: "acme/x".into(), route: LegRoute { labels: vec!["k8s".into()], ..LegRoute::default() }, priority: 60, cost, load, ..LegIn::default() }
    }

    const BUILD: Load = Load { heavy: true, io_weight: 0 };

    #[test]
    fn w24_r19_heavy_legs_declared_or_learned() {
        let c = cfg();
        assert_eq!(heavy(&c, &BUILD, None), Some(Heavy::Declared));
        assert_eq!(heavy(&c, &Load::default(), Some(1_800.0)), Some(Heavy::Learned { seconds: 1_800 }), "its legs lately took 30 minutes");
        assert_eq!(heavy(&c, &Load::default(), Some(30.0)), None);
        assert_eq!(heavy(&c, &Load::default(), None), None, "no history: not heavy");
        assert_eq!(heavy(&Admission { heavy_after: None, ..c }, &Load::default(), Some(1e9)), None, "learning off: only declared ones");
    }

    /// A job's `resources:` says what admission knows of its legs.
    #[test]
    fn w24_r19_a_jobs_resources_say_its_load() {
        let s = |t: &str| YVal::Str(Tpl { src: t.into(), ..Tpl::default() });
        let map = |kv: Vec<(&str, YVal)>| YVal::Map(kv.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
        assert_eq!(load_of(None), Load::default());
        assert_eq!(load_of(Some(&map(vec![("cpu", s("4"))]))), Load::default());
        assert_eq!(load_of(Some(&map(vec![("io", s("Heavy"))]))), Load { heavy: true, io_weight: 0 });
        assert_eq!(load_of(Some(&map(vec![("io", s("light")), ("io-weight", YVal::Num(50.0))]))), Load { heavy: false, io_weight: 50 });
        assert_eq!(load_of(Some(&map(vec![("io", s("heavy")), ("io-weight", s("20"))]))), Load { heavy: true, io_weight: 20 });
        assert_eq!(load_of(Some(&map(vec![("io-weight", YVal::Num(1e9))]))).io_weight, WEIGHT_MAX, "past the scale's top, its top");
        assert_eq!(load_of(Some(&map(vec![("io-weight", YVal::Num(0.0))]))).io_weight, 0, "under its bottom: the even share");
        assert_eq!(load_of(Some(&map(vec![("io-weight", s("lots"))]))).io_weight, 0);
    }

    /// The Sandbox seam's I/O weight: a leg that yields the disk starts under more pressure, one
    /// that takes more of it under less; the even share has the configured limit.
    #[test]
    fn w24_r19_the_limit_honours_the_io_weight() {
        let c = cfg();
        assert_eq!((limit(&c, 0), limit(&c, 100)), (4_000, 4_000));
        assert_eq!(limit(&c, 20), 10_000, "a fifth of the share: up to 200 %, held at most to 100 %");
        assert_eq!(limit(&c, 50), 8_000);
        assert_eq!(limit(&c, 400), 1_000);
        assert_eq!(limit(&c, u16::MAX), 40, "beyond the scale's top, its top");
        let busy = facts(Some(55.5), 500 * GIB);
        assert_eq!(admit(&c, "r", 100, &busy), Err(Held { runner: "r".into(), pressure: Some(5_550), limit: 4_000, free: 500 * GIB, floor: 20 * GIB }));
        assert_eq!(admit(&c, "r", 50, &busy), Ok(()), "a leg at half the share may start there");
        assert!(admit(&c, "r", 100, &facts(Some(39.0), 10 * GIB)).is_err(), "under the disk floor");
        assert_eq!(admit(&c, "r", 100, &facts(None, 500 * GIB)), Ok(()), "a runner that says nothing of its pressure isn't held for it");
        assert_eq!(admit(&c, "r", 100, &RunnerFacts::default()), Ok(()), "nor one that says nothing of its disk");
        assert_eq!(held(None, &BUILD, None, "r", &busy), None, "admission off");
        assert_eq!(held(Some(&c), &Load::default(), Some(30.0), "r", &busy), None, "a light leg");
        assert!(held(Some(&c), &BUILD, None, "r", &busy).is_some());
    }

    /// W24-R19 on the scheduler, simulated: heavy legs go to the machine under the limits, and
    /// once none is they wait, saying which machines hold them off and why, while light legs keep
    /// going; when a machine's pressure falls they start there.
    #[test]
    fn w24_r19_admission_by_load_in_the_scheduler() -> Result<(), String> {
        let mut sim = Sim::new(Config { admission: Some(cfg()), ..Config::default() });
        sim.runner(0, runner("busy", Some(70.0), 500 * GIB));
        sim.runner(0, runner("quiet", Some(5.0), 500 * GIB));
        sim.runner(0, runner("full", Some(1.0), 5 * GIB));
        sim.leg(1, leg("build1", BUILD, None), 30 * MIN);
        sim.run(2);
        assert_eq!(sim.assigned.first().map(|a| (a.leg.as_str(), a.runner.as_str())), Some(("build1", "quiet")), "the one machine within the limits");
        // quiet gets busy: no machine is within them now.
        sim.input(3, Input::RunnerChanged(Box::new(runner("quiet", Some(65.0), 500 * GIB))));
        sim.leg(4, leg("build2", BUILD, None), 30 * MIN);
        sim.leg(4, leg("learned", Load::default(), Some(2_400.0)), 10 * MIN);
        sim.leg(4, leg("lint", Load::default(), Some(30.0)), MIN);
        sim.run(5);
        assert!(sim.started("lint").is_some(), "a light leg goes anywhere");
        assert_eq!((sim.started("build2"), sim.started("learned")), (None, None));
        let held = match sim.last_wait("build2") {
            Some(WaitReason::Load { held }) => held.clone(),
            other => return Err(format!("build2 waits for {other:?}")),
        };
        let why: Vec<(&str, Option<u32>, u64)> = held.iter().map(|h| (h.runner.as_str(), h.pressure, h.free)).collect();
        assert_eq!(why, [("busy", Some(7_000), 500 * GIB), ("full", Some(100), 5 * GIB), ("quiet", Some(6_500), 500 * GIB)], "each machine it could take, and what's past its limit");
        let said = serde_json::to_value(sim.last_wait("build2")).unwrap_or_default();
        assert_eq!(said.pointer("/reason"), Some(&serde_json::json!("load")), "{said}");
        assert_eq!(said.pointer("/held/1"), Some(&serde_json::json!({ "runner": "full", "pressure": 100, "limit": 4_000, "free": 5 * GIB, "floor": 20 * GIB })), "{said}");
        assert!(matches!(sim.last_wait("learned"), Some(WaitReason::Load { .. })), "heavy by its history too");
        let room: Vec<(String, bool)> = sim.sched.explain("build2").into_iter().map(|e| (e.runner, e.room)).collect();
        assert!(room.iter().all(|(_, r)| !r), "no room on a runner past its limits: {room:?}");
        // A leg that yields the disk (I/O weight 30) starts under that pressure.
        sim.leg(6, leg("yielding", Load { heavy: true, io_weight: 30 }, None), MIN);
        sim.run(7);
        assert!(sim.started("yielding").is_some_and(|t| t <= 7), "its limit is 100 %");
        assert!(sim.assigned.iter().any(|a| a.leg == "yielding" && a.runner != "full"), "never under the disk floor: {:?}", sim.assigned);
        // busy's pressure falls: the held legs start there, in their order.
        sim.input(10 * MIN, Input::RunnerChanged(Box::new(runner("busy", Some(12.0), 500 * GIB))));
        sim.run(10 * MIN + 1);
        assert_eq!(sim.assigned.iter().filter(|a| a.runner == "busy" && (a.leg == "build2" || a.leg == "learned")).count(), 2, "{:?}", sim.assigned);
        Ok(())
    }

    /// Admission is opt-in: with no `[ci.scheduler.admission]`, heavy legs go anywhere.
    #[test]
    fn w24_r19_off_until_declared() {
        let mut sim = Sim::new(Config::default());
        sim.runner(0, runner("busy", Some(99.0), GIB));
        sim.leg(1, leg("build", BUILD, None), MIN);
        sim.run(2);
        assert_eq!(sim.started("build"), Some(1));
    }
}
