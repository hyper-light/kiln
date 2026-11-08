//! Contention now as slowness, and the cores' clocks now as speed now (W05-R26, W13-R01;
//! docs/needs.md "Pace").
//!
//! **The signals** ([`Signals`]) are each the share of time lost to one resource, as the runner
//! reads it now: its tasks waiting for a CPU, stalled on memory, stalled on I/O (Linux's pressure
//! stall information, `some`: Documentation/accounting/psi.rst), or, where the kernel keeps no
//! stall times, what queueing says of what it does keep (a run queue's length under processor
//! sharing, [`cpu_share`]; a device's latency against its own uncontended, `super::device`); and
//! swap-ins' share of time by the utilization law (U = X·S: Denning & Buzen, "The operational
//! analysis of queueing network models", ACM Computing Surveys 10(3), 1978).
//!
//! **The form.** Under processor sharing a job of size x takes x / (1 − ρ), whatever the sizes'
//! distribution (M/G/1-PS: Kleinrock, *Queueing Systems* vol. 2, Wiley 1976, §4.4; Harchol-Balter,
//! *Performance Modeling and Design of Computer Systems*, CUP 2013, ch. 22): a task that progresses
//! only in the share 1 − p of its time it isn't stalled takes 1/(1 − p) as long. A task stalled on
//! one resource isn't progressing on another, so each stall dilates by its own:
//!
//! ```text
//! slowness = Π_r (1 − p_r)^−w_r,     ln slowness = Σ_r w_r z_r,     z_r = −ln(1 − p_r)
//! ```
//!
//! with every w_r = 1 by the form. **Learned:** this runner's own runs say how much each signal at a
//! run's start slowed it. Within each kind of work here, a run's ln wall time is fitted against its
//! z's (the within, fixed-effects estimator: Wooldridge, *Econometric Analysis of Cross Section and
//! Panel Data*, MIT Press 2010, §10.5): each kind's intercept is its uncontended time here, so what
//! is fitted is each run's observed slowdown against the uncontended time of its kind on this
//! machine, and that time never has to be known first. The w's are non-negative least squares
//! (contention never speeds a run up), solved exactly over every support (Lawson & Hanson, *Solving
//! Least Squares Problems*, 1974; as `runtime`'s fit), and they take the form's place once the fit
//! is determined with a residual degree of freedom: the terms measured here and one, the rule of
//! `runtime::ROWS_KEPT`.
//!
//! **Thermal throttling** is speed now, not contention. Core-bound work's time goes as 1/f (Weiser
//! et al., "Scheduling for reduced CPU energy", OSDI 1994), so with z = −ln(f / f_max), the clocks
//! the cores at work run at against their most, the form's w is 1, and speed now is the fleet's
//! speed times exp(−w (z_now − z̄)), z̄ this machine's mean over its runs: the clocks its speed was
//! measured at, whatever all-core turbo leaves of f_max. macOS gives a thermal pressure level, not a
//! clock (Apple, ProcessInfo.ThermalState: `nominal` "within normal limits", `fair` "slightly
//! elevated", `serious` "high", `critical` "significantly impacting the performance of the system
//! and the device needs to cool down"), and no figure for any level, so its w is learned alone: 0,
//! no effect claimed, until the runs here say one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Dense, in_ones};

/// The model's terms: CPU, memory, I/O, swap, thermal.
pub const TERMS: usize = 5;
/// The contention terms among them, the first four.
pub const CONTENTION: usize = 4;
const THERMAL: usize = 4;

/// What a runner reads of its contention and clocks at a moment (a run's start, a measure).
/// Shares are of time, in [0, 1); none: not measured here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Signals {
    /// Its tasks waited for a CPU: PSI `cpu` `some`, or the run queue's share ([`cpu_share`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    /// Its tasks stalled on memory: PSI `memory` `some`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<f64>,
    /// Its tasks stalled on I/O: PSI `io` `some`; where there's none, its work device's
    /// utilization against its own uncontended latency (`super::device::utilization`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io: Option<f64>,
    /// Swap-ins' share of time: swap-ins a second times the device's service time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<f64>,
    /// The clocks the cores at work run at, against their most (Σ f / Σ f_max).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock: Option<f64>,
    /// macOS's thermal pressure level (0 nominal, 1 moderate, 2 heavy, 3 trapping, 4 sleeping:
    /// `OSThermalNotification.h`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
}

/// −ln(1 − p) for a share p, 0 for none; a share at or past 1 (a stall all the time) is held to
/// just under it, so the figure stays finite.
fn dilation(p: Option<f64>) -> f64 {
    match p {
        Some(p) if p.is_finite() && p > 0.0 => -(1.0 - p.min(1.0 - f64::EPSILON)).ln(),
        _ => 0.0,
    }
}

impl Signals {
    /// Its z's, in [`TERMS`]' order.
    pub fn z(&self) -> [f64; TERMS] {
        let thermal = match (self.clock, self.level) {
            (Some(f), _) if f.is_finite() && f > 0.0 => -f.ln(),
            (_, Some(l)) => f64::from(l),
            _ => 0.0,
        };
        [dilation(self.cpu), dilation(self.memory), dilation(self.io), dilation(self.swap), thermal]
    }
}

/// The share of time a new task waits for a CPU under processor sharing, from the run queue: with
/// `runnable` tasks wanting the `cpus` CPUs on average (running or waiting), one more gets
/// `cpus / (runnable + 1)` of a CPU once they're more than the CPUs, so it is stalled
/// `1 − cpus / (runnable + 1)` of its time (Kleinrock 1976, §4.4's sharing among c servers).
pub fn cpu_share(runnable: f64, cpus: u32) -> Option<f64> {
    let c = f64::from(cpus);
    (runnable.is_finite() && runnable >= 0.0 && c > 0.0).then(|| (1.0 - c / (runnable + 1.0)).max(0.0))
}

/// Sums over one kind's runs here: count, Σy, Σz, Σzzᵀ, Σzy, Σy².
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Sums {
    n: u64,
    y: f64,
    yy: f64,
    z: [f64; TERMS],
    zz: [[f64; TERMS]; TERMS],
    zy: [f64; TERMS],
}

/// The within-kind cross products: G = Σzzᵀ − ΣzΣzᵀ/n, h = Σzy − ΣzΣy/n, s = Σy² − (Σy)²/n.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Within {
    g: [[f64; TERMS]; TERMS],
    h: [f64; TERMS],
    s: f64,
    dof: u64,
}

impl Sums {
    fn add(&mut self, y: f64, z: &[f64; TERMS]) {
        self.n = self.n.saturating_add(1);
        self.y += y;
        self.yy += y * y;
        for (i, zi) in z.iter().enumerate() {
            if let Some(v) = self.z.get_mut(i) {
                *v += zi;
            }
            if let Some(v) = self.zy.get_mut(i) {
                *v += zi * y;
            }
            if let Some(row) = self.zz.get_mut(i) {
                for (j, zj) in z.iter().enumerate() {
                    if let Some(v) = row.get_mut(j) {
                        *v += zi * zj;
                    }
                }
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn within(&self) -> Within {
        let mut w = Within::default();
        if self.n == 0 {
            return w;
        }
        let n = self.n as f64;
        for i in 0..TERMS {
            let zi = self.z.get(i).copied().unwrap_or(0.0);
            if let Some(v) = w.h.get_mut(i) {
                *v = self.zy.get(i).copied().unwrap_or(0.0) - zi * self.y / n;
            }
            for j in 0..TERMS {
                let zj = self.z.get(j).copied().unwrap_or(0.0);
                let zz = self.zz.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0);
                if let Some(v) = w.g.get_mut(i).and_then(|r| r.get_mut(j)) {
                    *v = zz - zi * zj / n;
                }
            }
        }
        w.s = (self.yy - self.y * self.y / n).max(0.0);
        w.dof = self.n.saturating_sub(1);
        w
    }
}

impl Within {
    fn plus(&mut self, o: &Within, sign: f64) {
        for i in 0..TERMS {
            if let (Some(v), Some(x)) = (self.h.get_mut(i), o.h.get(i)) {
                *v += sign * x;
            }
            for j in 0..TERMS {
                if let (Some(v), Some(x)) = (self.g.get_mut(i).and_then(|r| r.get_mut(j)), o.g.get(i).and_then(|r| r.get(j))) {
                    *v += sign * x;
                }
            }
        }
        self.s += sign * o.s;
        self.dof = if sign > 0.0 { self.dof.saturating_add(o.dof) } else { self.dof.saturating_sub(o.dof) };
    }

    fn g(&self, i: usize, j: usize) -> f64 {
        self.g.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0)
    }
}

/// The weights of the form's terms, and whether this machine's runs learned them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    pub w: [f64; TERMS],
    pub learned: bool,
}

impl Weights {
    /// The form's own: 1 for every stall share and for a clock; 0 for a thermal level, which
    /// Apple gives no figure for.
    pub fn form(level: bool) -> Weights {
        Weights { w: [1.0, 1.0, 1.0, 1.0, if level { 0.0 } else { 1.0 }], learned: false }
    }

    /// Slowness under `s`, in 1/1024 of uncontended: exp(Σ w z) over the contention terms.
    pub fn slowness(&self, s: &Signals) -> u32 {
        let z = s.z();
        let e: f64 = self.w.iter().zip(z.iter()).take(CONTENTION).map(|(w, z)| w * z).sum();
        in_ones(e.exp())
    }

    /// `speed` (in 1/1024 of the reference core's; 0 unmeasured) as the clocks now leave it,
    /// against the clocks its runs here ran at on average (`mean_z`, the thermal z's).
    pub fn speed_now(&self, speed: u32, s: &Signals, mean_z: Option<f64>) -> u32 {
        if speed == 0 {
            return 0;
        }
        let now = s.z().get(THERMAL).copied().unwrap_or(0.0);
        let measured = s.clock.is_some() || s.level.is_some();
        let (Some(mean), true) = (mean_z, measured) else { return speed };
        let w = self.w.get(THERMAL).copied().unwrap_or(0.0);
        in_ones(f64::from(speed) / 1024.0 * (-w * (now - mean)).exp())
    }
}

/// This runner's runs, kept to learn the form's weights from ([`Learner::weights`]) and its
/// clocks' mean ([`Learner::mean_thermal`]). Kept by the runner, folded in as each run ends.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Learner {
    kinds: BTreeMap<String, Sums>,
    within: Within,
    /// The thermal z's of every run here: their count and sum.
    thermal: (u64, f64),
    /// Its thermal signal is a level (macOS), not a clock.
    level: bool,
}

impl Learner {
    /// A finished run of kind `key`: its wall time in seconds and the signals at its start.
    pub fn observe(&mut self, key: &str, wall_s: f64, at_start: &Signals) {
        if !(wall_s.is_finite() && wall_s > 0.0) || key.is_empty() {
            return;
        }
        let (y, z) = (wall_s.ln(), at_start.z());
        let sums = self.kinds.entry(key.to_owned()).or_default();
        let before = sums.within();
        sums.add(y, &z);
        let after = sums.within();
        self.within.plus(&before, -1.0);
        self.within.plus(&after, 1.0);
        if at_start.clock.is_some() || at_start.level.is_some() {
            self.thermal = (self.thermal.0.saturating_add(1), self.thermal.1 + z.get(THERMAL).copied().unwrap_or(0.0));
            self.level = at_start.clock.is_none();
        }
    }

    /// The mean thermal z over the runs here; none before one was measured.
    #[allow(clippy::cast_precision_loss)]
    pub fn mean_thermal(&self) -> Option<f64> {
        (self.thermal.0 > 0).then(|| self.thermal.1 / self.thermal.0 as f64)
    }

    /// The uncontended time of kind `key` here, in seconds: its runs' intercept at no contention
    /// under `w` (what a run's observed slowdown is against).
    #[allow(clippy::cast_precision_loss)]
    pub fn uncontended_s(&self, key: &str, w: &Weights) -> Option<f64> {
        let s = self.kinds.get(key).filter(|s| s.n > 0)?;
        let n = s.n as f64;
        let lost: f64 = w.w.iter().zip(s.z.iter()).take(CONTENTION).map(|(w, z)| w * z / n).sum();
        Some((s.y / n - lost).exp())
    }

    /// The weights: learned from the runs here once their fit is determined with a residual
    /// degree of freedom, else the form's. A term that never varied within a kind here can't be
    /// learned, and keeps the form's.
    pub fn weights(&self) -> Weights {
        let form = Weights::form(self.level);
        let active: Vec<usize> = (0..TERMS).filter(|i| self.within.g(*i, *i) > 0.0).collect();
        if active.is_empty() || self.within.dof < active.len() as u64 + 1 {
            return form;
        }
        let Some(best) = nnls(&self.within, &active) else { return form };
        let mut w = form.w;
        for (i, v) in active.iter().zip(best) {
            if let Some(x) = w.get_mut(*i) {
                *x = v;
            }
        }
        Weights { w, learned: true }
    }
}

/// Non-negative least squares on the within cross products, over the terms `active`, exact: every
/// support's unconstrained solution, the feasible one of least residual kept (the optimum is one
/// of them). None when no support's equations can be solved.
fn nnls(within: &Within, active: &[usize]) -> Option<Vec<f64>> {
    let k = active.len();
    let mut best: Option<(f64, Vec<f64>)> = None;
    for mask in 0u32..(1u32 << k) {
        let support: Vec<usize> = (0..k).filter(|b| (mask >> b) & 1 == 1).filter_map(|b| active.get(b).copied()).collect();
        let mut w = vec![0.0; k];
        // The residual's sum of squares: s − 2wᵀh + wᵀGw, which at the support's solution is s − wᵀh.
        let mut rss = within.s;
        if !support.is_empty() {
            let mut g = Dense::zero(support.len());
            for (a, i) in support.iter().enumerate() {
                for (b, j) in support.iter().enumerate() {
                    g.set(a, b, within.g(*i, *j));
                }
            }
            let Some(inv) = g.inverse() else { continue };
            let h: Vec<f64> = support.iter().map(|i| within.h.get(*i).copied().unwrap_or(0.0)).collect();
            let sol = inv.times(&h);
            if sol.iter().any(|v| !(v.is_finite() && *v >= 0.0)) {
                continue;
            }
            rss -= sol.iter().zip(h.iter()).map(|(a, b)| a * b).sum::<f64>();
            for (b, v) in (0..k).filter(|b| (mask >> b) & 1 == 1).zip(sol) {
                if let Some(x) = w.get_mut(b) {
                    *x = v;
                }
            }
        }
        if best.as_ref().is_none_or(|(r, _)| rss < *r) {
            best = Some((rss, w));
        }
    }
    best.map(|(_, w)| w)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Draws(u64);

    impl Draws {
        fn uniform(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            let (u, v) = (self.uniform(), self.uniform());
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    /// W05-R26: the form is processor sharing's dilation: a task stalled half its time takes
    /// twice as long, a quarter of it 4/3 as long, and none of it as long as uncontended. A
    /// mapping of 1 + p (planted: linear in the stall) would say 1.5 and 1.25.
    #[test]
    fn w05_r26_form_is_processor_sharing_dilation() {
        let form = Weights::form(false);
        assert_eq!(form.slowness(&Signals::default()), 1024);
        assert_eq!(form.slowness(&Signals { cpu: Some(0.5), ..Signals::default() }), 2048);
        assert_eq!(form.slowness(&Signals { io: Some(0.25), ..Signals::default() }), 1365);
        // Stalls on two resources each dilate by their own.
        assert_eq!(form.slowness(&Signals { cpu: Some(0.5), memory: Some(0.5), ..Signals::default() }), 4096);
        // A stall all the time stays finite.
        assert!(form.slowness(&Signals { cpu: Some(1.0), ..Signals::default() }) > 1 << 20);
        // The clock isn't contention.
        assert_eq!(form.slowness(&Signals { clock: Some(0.5), ..Signals::default() }), 1024);
    }

    /// W05-R26: a run queue's share under processor sharing: no wait while the runnable tasks and
    /// one more fit the CPUs; past that, one more gets cpus / (runnable + 1).
    #[test]
    fn w05_r26_run_queue_share() {
        assert_eq!(cpu_share(3.0, 8), Some(0.0));
        assert_eq!(cpu_share(7.0, 8), Some(0.0));
        assert!(matches!(cpu_share(15.0, 8), Some(p) if (p - 0.5).abs() < 1e-12));
        assert_eq!(cpu_share(1.0, 0), None);
        let form = Weights::form(false);
        assert_eq!(form.slowness(&Signals { cpu: cpu_share(15.0, 8), ..Signals::default() }), 2048);
    }

    /// W05-R26: the runs here teach the weights. Planted: kinds whose time a CPU stall dilates by
    /// 1.5 times the form's and an I/O stall not at all; the learned weights say so, and the form's
    /// would not. Each kind's uncontended time differs, and is never told.
    #[test]
    fn w05_r26_weights_are_learned_within_kinds() -> Result<(), String> {
        let mut d = Draws(21);
        let mut l = Learner::default();
        assert!(!l.weights().learned);
        for k in 0..12 {
            let base = (5.0 + 200.0 * d.uniform()).ln();
            for _ in 0..8 {
                let s = Signals { cpu: Some(0.6 * d.uniform()), io: Some(0.5 * d.uniform()), memory: Some(0.0), ..Signals::default() };
                let z = s.z();
                let wall = (base + 1.5 * z.first().copied().unwrap_or(0.0) + 0.01 * d.normal()).exp();
                l.observe(&format!("k{k}"), wall, &s);
            }
        }
        let w = l.weights();
        assert!(w.learned, "{w:?}");
        let (cpu, io) = (w.w.first().copied().ok_or("cpu")?, w.w.get(2).copied().ok_or("io")?);
        assert!((cpu - 1.5).abs() < 0.05, "{w:?}");
        assert!(io < 0.05, "{w:?}");
        // Memory never varied: the form's.
        assert_eq!(w.w.get(1).copied(), Some(1.0));
        // A kind's uncontended time is its intercept.
        let s = Signals { cpu: Some(0.5), ..Signals::default() };
        let pred = l.uncontended_s("k0", &w).ok_or("k0")?;
        assert!(pred > 0.0 && w.slowness(&s) > 1024);
        Ok(())
    }

    /// W05-R26: contention never speeds a run up: a signal that seems to (runs faster under it)
    /// gets weight 0, not a negative one.
    #[test]
    fn w05_r26_weights_never_negative() {
        let mut d = Draws(4);
        let mut l = Learner::default();
        for k in 0..6 {
            for _ in 0..6 {
                let s = Signals { cpu: Some(0.5 * d.uniform()), ..Signals::default() };
                let z0 = s.z().first().copied().unwrap_or(0.0);
                l.observe(&format!("k{k}"), (3.0 - 0.8 * z0 + 0.01 * d.normal()).exp(), &s);
            }
        }
        let w = l.weights();
        assert!(w.learned && w.w.first().copied() == Some(0.0), "{w:?}");
    }

    /// W05-R26: a machine's clocks now against its runs' usual: down by half from its mean is half
    /// its speed by the form; at its mean, its speed. A level (macOS) claims nothing until learned.
    #[test]
    fn w05_r26_clock_is_speed_now() {
        let form = Weights::form(false);
        let at = |f: f64| Signals { clock: Some(f), ..Signals::default() };
        let mean = Some(-(0.9f64).ln());
        assert_eq!(form.speed_now(1024, &at(0.9), mean), 1024);
        assert_eq!(form.speed_now(1024, &at(0.45), mean), 512);
        assert_eq!(form.speed_now(0, &at(0.45), mean), 0);
        assert_eq!(form.speed_now(1024, &Signals::default(), mean), 1024);
        let mac = Weights::form(true);
        assert_eq!(mac.speed_now(1024, &Signals { level: Some(2), ..Signals::default() }, Some(0.0)), 1024);
    }

    /// W05-R26: the thermal mean is of the runs' clocks, and a level makes the form claim nothing.
    #[test]
    fn w05_r26_learner_keeps_thermal_mean() {
        let mut l = Learner::default();
        assert_eq!(l.mean_thermal(), None);
        l.observe("k", 10.0, &Signals { clock: Some(0.5), ..Signals::default() });
        l.observe("k", 10.0, &Signals { clock: Some(1.0), ..Signals::default() });
        assert!(matches!(l.mean_thermal(), Some(m) if (m - 0.5 * 2f64.ln()).abs() < 1e-12));
        let mut mac = Learner::default();
        mac.observe("k", 10.0, &Signals { level: Some(1), ..Signals::default() });
        assert_eq!(mac.weights().w.get(4).copied(), Some(0.0));
    }
}
