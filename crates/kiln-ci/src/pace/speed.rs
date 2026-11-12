//! The fleet's per-core speeds, learned from kiln's own run history (W05-R26; docs/needs.md
//! "Pace").
//!
//! **Where the durations come from.** Each finished run's observation
//! ([`crate::runtime::Observation`]: its wall time, the CPU time its processes used, its exit),
//! which its runner measures from its start to its end and sends in its facts. The server keeps
//! it in `run_observations` and folds it in here as it keeps it, into three sums per kind and
//! machine ([`Cell`], the `run_speed_cells` table): a refit from them is the fit from every run ever
//! kept, while `run_observations` keeps only each key's newest rows.
//!
//! **The model.** A run r of kind k on machine m:
//!
//! ```text
//! ln wall_r = a_k − b_m + ln π_r,     π_r = wall_r / cpu_r
//! ```
//!
//! π_r, its parallelism term, is the run's wall-seconds per CPU-second it used: the inverse of the
//! mean number of CPUs it kept busy. How wide it ran, and how many CPUs were free for it, are in π,
//! not in b, so b_m is per-core speed alone: ln of machine m's core's speed against the reference
//! core's; a_k is ln of kind k's CPU-seconds on the reference core. With π on the left the fit is
//! of y_r = ln cpu_r = a_k − b_m + ε_r, by least squares over every key that ran on two or more
//! machines (a key that ran on one says nothing of speed, and drops out of the normal equations by
//! itself). On a complete design it's the geometric mean of the per-workload time ratios, the
//! summary SPEC reports (Fleming & Wallace, "How not to lie with statistics: the correct way to
//! summarize benchmark results", CACM 29(3), 1986); on kiln's unbalanced one, the two-way additive
//! model's least squares (Searle, *Linear Models for Unbalanced Data*, Wiley 1987, ch. 4).
//!
//! **What a kind's runs say of a machine, and what they don't.** Runs of a kind on one machine
//! agree with each other far better than the same kind does across machines: a command's work
//! depends on the machine's state (a build cached on one compiles everything on another), a CI
//! leg's on its platform (the steps it skips there). In production's history (2026-11-12) repeats
//! on one machine agreed within 1.8x while one kind was 4,000x apart across six machines and a leg
//! took 8,000 times the CPU on Linux it took on Windows. So a cell mean (a kind's runs on one
//! machine) is
//!
//! ```text
//! ȳ_km = a_k + θ_m + γ_km + ε̄_km,     θ = −b,  Var γ_km = g_k,  Var ε̄_km = s_e / n_km
//! ```
//!
//! γ_km the part of the kind's work that differs on that machine (g_k its kind's variance of it),
//! which no number of runs there averages away: a machine's speed is known as far as kinds that
//! agree elsewhere say it, kind by kind. Least squares of every run alike (this model with g = 0)
//! took one run of one such kind as a machine 478x slower than the fleet, and two as one 49x faster.
//!
//! **Fitted in one step, robustly.** s_e is the runs' own variance, pooled within the cells (their
//! sums of squares about their means, over their runs less one). The typical kind's g is the
//! variance at which the cells' median standardised residual, |r| / √(var (1 − h)), h the cell's
//! leverage, is the normal's, Φ⁻¹(3/4) (the median absolute deviation as a scale: Hampel, "The
//! influence curve and its role in robust estimation", JASA 69, 1974; Rousseeuw & Croux,
//! "Alternatives to the median absolute deviation", JASA 88, 1993), so kinds whose work differs
//! wildly don't set it. Each kind's own g_k is then the larger of that and what its own cells say
//! about that fit (Σ r² / Σ (1 − h)), taken once: a one-step estimator (Bickel, "One-step Huber
//! estimates in the linear model", JASA 70, 1975). Iterated with the fit it doesn't settle where two
//! kinds alone join a machine to the rest and disagree (it flips between them), and shrinking a
//! kind's noise toward the fleet's (Smyth's moderation, 2004) lets one noisy kind pass for a clean
//! one when most are clean; a kind is taken as at least as noisy as its own cells say. θ is then
//! weighted least squares on the cell means, weights w = 1 / (g_k + s_e / n): eliminating the a's
//! leaves L θ = c, L the machines' weighted Laplacian, L_mm' = Σ_k (w_km δ_mm' − w_km w_km' / W_k),
//! c_m = Σ_k w_km (ȳ_km − Σ_m' p_km' ȳ_km'), p = w / W, solved as (L + J/M) θ = c over the
//! connected set of machines with the most runs (L's null space is the constants on each), θ̂_m's
//! variance L⁺_mm, L⁺ = (L + J/M)⁻¹ − J/M. A machine outside the set shares no work with it: its
//! speed is unmeasured (0, which the load-balancing kernel reads as the reference). The fit is made
//! from the cells after each run that changes one of a shared kind: O(cells · a kind's machines² +
//! M³) for each of the few dozen evaluations its two root-findings take.
//!
//! **Shrunk toward the fleet's centre** (empirical Bayes: Efron & Morris, "Data analysis using
//! Stein's estimator and its generalizations", JASA 70, 1975; Morris, "Parametric empirical Bayes
//! inference", JASA 78, 1983): each machine's posterior mean is θ̂_m's deviation from the centre μ
//! times τ² / (τ² + v_m), one with few runs or noisy kinds (a large v) staying near the centre, one
//! with many clean ones keeping its own. The centre and spread are taken robustly too, so that a
//! machine or two measured only by noisy kinds move neither: μ the median of the θ̂ weighted by 1 /
//! (v + τ²), τ² the least at which the machines' median |θ̂ − μ| / √(v + τ²) is Φ⁻¹(3/4) (Paule &
//! Mandel's moment equation, "Consensus values and weighting factors", J. Res. NBS 87, 1982, with
//! the median for the mean). The reference core is the centre's: speed 1024. Against synthetic
//! fleets of known speeds (`w05_r26_the_robust_fit_beats_runs_alike_on_noisy_kinds`) it's as close
//! as runs alike on clean ones and several times closer where kinds' work differs. Until the fit
//! has a residual degree of freedom (runs past kinds and machines), nothing is measured, as
//! `runtime`'s fits wait for theirs.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Dense, in_ones};
use crate::runtime::Observation;

/// One kind's runs on one machine: how many, and their y's sum and sum of squares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cell {
    pub n: u64,
    pub sum: f64,
    pub sq: f64,
}

impl Cell {
    pub fn add(&mut self, y: f64) {
        self.n = self.n.saturating_add(1);
        self.sum += y;
        self.sq += y * y;
    }

    /// Two cells' runs together.
    pub fn plus(self, o: Cell) -> Cell {
        Cell { n: self.n.saturating_add(o.n), sum: self.sum + o.sum, sq: self.sq + o.sq }
    }

    #[allow(clippy::cast_precision_loss)]
    fn mean(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.sum / self.n as f64 }
    }

    /// Σ (y − ȳ)² of its runs.
    #[allow(clippy::cast_precision_loss)]
    fn within(&self) -> f64 {
        if self.n == 0 { 0.0 } else { (self.sq - self.sum * self.sum / self.n as f64).max(0.0) }
    }
}

/// A run's y, ln of the CPU-seconds its processes used: for a run that did its kind's whole work
/// (it didn't exit with a failure, and nothing of the machine's ended it: a CI leg's recorder
/// isn't told its exit) and whose CPU and wall times were measured.
pub fn y_of(o: &Observation) -> Option<f64> {
    if o.cause.is_some() || o.exit_code.is_some_and(|c| c != 0) {
        return None;
    }
    let cpu = o.cpu_s?;
    (cpu.is_finite() && cpu > 0.0 && o.wall_s.is_finite() && o.wall_s > 0.0).then(|| cpu.ln())
}

/// The key a run's kind is compared under across machines: a command's work key (its text, and its
/// folder as every machine names it), a CI leg's own key (its repository, workflow, job and matrix:
/// the same on every runner).
pub fn key_of(o: &Observation) -> Option<&str> {
    match &o.work_key {
        Some(k) if !k.is_empty() => Some(k),
        _ => (o.kind == "leg" && !o.key.is_empty()).then_some(o.key.as_str()),
    }
}

/// What the fit says of one machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Machine {
    /// Its runs in kinds that ran elsewhere too.
    pub runs: u64,
    /// θ̂, ln of its CPU time against the reference core's (the fleet's centre) for the same work.
    pub raw: f64,
    /// θ̂'s sampling variance.
    pub var: f64,
    /// θ̂ shrunk toward the fleet's centre.
    pub shrunk: f64,
    /// Its per-core speed, in 1/1024 of the reference core's: 1024 · exp(−shrunk).
    pub speed: u32,
}

/// The fleet's fit.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Fit {
    /// The machines of the reference set, by name.
    pub machines: BTreeMap<String, Machine>,
    /// A run's own variance of y (s_e: its repeats' on one machine), and the fit's residual degrees
    /// of freedom (runs past kinds and machines).
    pub sigma2: f64,
    pub dof: u64,
    /// The fleet's spread of θ about its centre (robust).
    pub tau2: f64,
    /// The typical kind's variance of the work that differs from machine to machine (g, robust).
    pub differs: f64,
}

impl Fit {
    /// Machine `name`'s per-core speed in 1/1024 of the reference core's; 0 when unmeasured.
    pub fn speed(&self, name: &str) -> u32 {
        self.machines.get(name).map_or(0, |m| m.speed)
    }
}

/// Every kind's cells, kept as each run is folded in: the fit is made from them.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    names: Vec<String>,
    index: BTreeMap<String, usize>,
    keys: BTreeMap<String, BTreeMap<usize, Cell>>,
}

impl Fleet {
    /// Machine `name`'s place, made for it if it's new.
    fn machine(&mut self, name: &str) -> usize {
        if let Some(i) = self.index.get(name) {
            return *i;
        }
        let i = self.names.len();
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), i);
        i
    }

    /// Cell `cell` of kind `key` on `machine` joins what's kept (one run's, or the store's sums
    /// as it starts). Whether it changed a kind that ran on two or more machines: what the fit is
    /// made from (a kind of one machine says nothing of speed).
    pub fn add(&mut self, key: &str, machine: &str, cell: Cell) -> bool {
        if cell.n == 0 || !(cell.sum.is_finite() && cell.sq.is_finite()) {
            return false;
        }
        let m = self.machine(machine);
        let cells = self.keys.entry(key.to_owned()).or_default();
        let had = cells.get(&m).copied().unwrap_or_default();
        cells.insert(m, had.plus(cell));
        cells.len() >= 2
    }

    /// Kind `key`'s work, ln of its CPU-seconds on the reference core (the model's `a_k`): over
    /// its runs on every machine, each run's `y` less its machine's θ as `fit` has it (shrunk
    /// toward the fleet; 0, the reference, for a machine it doesn't measure), weighted by runs. A
    /// kind that ran on one machine has it too: its runs there, that machine's speed taken out.
    /// `None`: no run of it kept. What the CI engine's load balancer ranks a leg's runners by
    /// (`crate::sched::balance::Profile`).
    #[allow(clippy::cast_precision_loss)]
    pub fn work(&self, key: &str, fit: &Fit) -> Option<f64> {
        let cells = self.keys.get(key)?;
        let (mut n, mut sum) = (0u64, 0.0);
        for (m, c) in cells {
            let theta = self.names.get(*m).and_then(|name| fit.machines.get(name)).map_or(0.0, |x| x.shrunk);
            sum += c.sum - c.n as f64 * theta;
            n = n.saturating_add(c.n);
        }
        (n > 0).then(|| sum / n as f64)
    }

    /// A finished run of kind `key` on `machine`, its `y` ([`y_of`]). Whether it changed what the
    /// fit is made from ([`Fleet::add`]).
    pub fn observe(&mut self, key: &str, machine: &str, y: f64) -> bool {
        let mut cell = Cell::default();
        cell.add(y);
        self.add(key, machine, cell)
    }

    /// The machines joined by kinds that ran on two or more, as sets of their places: each set's
    /// members in order, and its runs.
    fn components(&self) -> Vec<(Vec<usize>, u64)> {
        let n = self.names.len();
        let mut parent: Vec<usize> = (0..n).collect();
        let find = |p: &mut Vec<usize>, mut x: usize| -> usize {
            while let Some(&up) = p.get(x) {
                if up == x {
                    break;
                }
                // Path halving.
                let grand = p.get(up).copied().unwrap_or(up);
                if let Some(v) = p.get_mut(x) {
                    *v = grand;
                }
                x = grand;
            }
            x
        };
        for cells in self.keys.values().filter(|c| c.len() >= 2) {
            let mut it = cells.keys();
            let Some(first) = it.next() else { continue };
            for m in it {
                let (a, b) = (find(&mut parent, *first), find(&mut parent, *m));
                if a != b
                    && let Some(v) = parent.get_mut(a.max(b))
                {
                    *v = a.min(b);
                }
            }
        }
        let mut sets: BTreeMap<usize, (Vec<usize>, u64)> = BTreeMap::new();
        for cells in self.keys.values().filter(|c| c.len() >= 2) {
            for (m, c) in cells {
                let root = find(&mut parent, *m);
                let e = sets.entry(root).or_default();
                e.1 = e.1.saturating_add(c.n);
            }
        }
        for m in 0..n {
            let root = find(&mut parent, m);
            if let Some(e) = sets.get_mut(&root) {
                e.0.push(m);
            }
        }
        sets.into_values().collect()
    }

    /// The fit as it stands: every speed the reference set's runs can say, shrunk toward the
    /// fleet's centre; nothing before the fit has a residual degree of freedom.
    #[allow(clippy::cast_precision_loss)]
    pub fn fit(&self) -> Fit {
        // The reference set: the most runs; between equals, the one its first machine came first.
        let Some((members, _)) = self.components().into_iter().reduce(|a, b| if b.1 > a.1 { b } else { a }) else { return Fit::default() };
        let mm = members.len();
        if mm < 2 {
            return Fit::default();
        }
        let local: BTreeMap<usize, usize> = members.iter().enumerate().map(|(l, g)| (*g, l)).collect();
        // The kinds that ran on two or more of its machines, each its cells.
        let kinds: Vec<Vec<Obs>> = self
            .keys
            .values()
            .filter(|c| c.len() >= 2 && c.keys().all(|m| local.contains_key(m)))
            .map(|cells| cells.iter().filter_map(|(m, c)| Some(Obs { m: *local.get(m)?, n: c.n as f64, mean: c.mean() })).collect())
            .collect();
        let runs: u64 = self.keys.values().filter(|c| c.len() >= 2 && c.keys().all(|m| local.contains_key(m))).flat_map(|c| c.values()).map(|c| c.n).sum();
        let dof = runs.saturating_sub(kinds.len() as u64).saturating_sub(mm as u64 - 1);
        if dof == 0 {
            return Fit::default();
        }
        // s_e: the runs' own variance, pooled within cells.
        let (within, repeats) = self.keys.values().filter(|c| c.len() >= 2 && c.keys().all(|m| local.contains_key(m))).flat_map(|c| c.values()).fold((0.0, 0u64), |(w, r), c| (w + c.within(), r + c.n.saturating_sub(1)));
        let s_e = if repeats > 0 { within / repeats as f64 } else { 0.0 };
        let var_of = |g: f64| -> Vec<Vec<f64>> { kinds.iter().map(|cells| cells.iter().map(|o| g + s_e / o.n).collect()).collect() };
        // g, the typical kind's work-differs: the cells' median standardised residual at it is the
        // normal's. Its search starts at the cell means' variance about their kinds' means.
        let (dev, cells_less_kinds) = kinds.iter().fold((0.0, 0usize), |(d, c), cells| {
            let mean = cells.iter().map(|o| o.mean).sum::<f64>() / cells.len().max(1) as f64;
            (d + cells.iter().map(|o| (o.mean - mean).powi(2)).sum::<f64>(), c + cells.len().saturating_sub(1))
        });
        let scale = if cells_less_kinds > 0 { dev / cells_less_kinds as f64 } else { 0.0 };
        let g = least_root(|g| {
            let var = var_of(g);
            let w = weighted(&kinds, &var, mm)?;
            let z: Vec<f64> = w.resid.iter().zip(&var).flat_map(|(rs, vs)| rs.iter().zip(vs)).filter(|((_, h), _)| 1.0 - h > f64::EPSILON).map(|((r, h), v)| r.abs() / (v * (1.0 - h)).sqrt()).collect();
            Some(median(z).map_or(-NORMAL_MEDIAN_ABS, |m| m - NORMAL_MEDIAN_ABS))
        }, s_e > 0.0, scale)
        .unwrap_or(0.0);
        // One step: each kind at least as noisy as its own cells say about that fit.
        let var0 = var_of(g);
        let Some(first) = weighted(&kinds, &var0, mm) else { return Fit::default() };
        let var1: Vec<Vec<f64>> = first
            .resid
            .iter()
            .zip(&var0)
            .map(|(rs, vs)| {
                let (ss, d) = rs.iter().fold((0.0, 0.0), |(ss, d), (r, h)| (ss + r * r, d + (1.0 - h)));
                let own = if d > f64::EPSILON { ss / d } else { 0.0 };
                vs.iter().map(|v| v.max(own)).collect()
            })
            .collect();
        let Some(fit) = weighted(&kinds, &var1, mm) else { return Fit::default() };
        let theta = fit.theta;
        let var: Vec<f64> = (0..mm).map(|l| fit.lplus.get(l, l).max(0.0)).collect();
        let (centre, tau2) = centre_and_spread(&theta, &var);
        let mut per: BTreeMap<usize, u64> = BTreeMap::new();
        for cells in self.keys.values().filter(|c| c.len() >= 2 && c.keys().all(|m| local.contains_key(m))) {
            for (m, c) in cells {
                *per.entry(*m).or_default() += c.n;
            }
        }
        let mut machines = BTreeMap::new();
        for (l, gm) in members.iter().enumerate() {
            let (raw, v) = (theta.get(l).copied().unwrap_or(0.0) - centre, var.get(l).copied().unwrap_or(0.0));
            let shrunk = if tau2 + v > 0.0 { raw * tau2 / (tau2 + v) } else { 0.0 };
            let Some(name) = self.names.get(*gm) else { continue };
            machines.insert(name.clone(), Machine { runs: per.get(gm).copied().unwrap_or(0), raw, var: v, shrunk, speed: in_ones((-shrunk).exp()) });
        }
        Fit { machines, sigma2: s_e, dof, tau2, differs: g }
    }
}

/// Φ⁻¹(3/4): the median of |z| for a standard normal z, what a scale makes the median
/// standardised deviation (docs above).
const NORMAL_MEDIAN_ABS: f64 = 0.674_489_750_196_081_7;

/// A kind's cell in a fit: its machine's place in the reference set, its runs, their mean y.
#[derive(Debug, Clone, Copy)]
struct Obs {
    m: usize,
    n: f64,
    mean: f64,
}

/// Weighted least squares of the cell means: θ, L⁺, and each cell's residual and leverage, kind by
/// kind as the kinds have them.
struct Weighted {
    theta: Vec<f64>,
    lplus: Dense,
    resid: Vec<Vec<(f64, f64)>>,
}

/// The cell means of `kinds` weighted by 1 / `var` (one for each cell, as `kinds` has them), over
/// `mm` machines; none when a variance isn't positive or the system is singular.
#[allow(clippy::cast_precision_loss)]
fn weighted(kinds: &[Vec<Obs>], var: &[Vec<f64>], mm: usize) -> Option<Weighted> {
    if var.iter().flatten().any(|v| !(v.is_finite() && *v > 0.0)) {
        return None;
    }
    let j = 1.0 / mm as f64;
    let mut a = Dense::zero(mm);
    let mut c = vec![0.0; mm];
    let shares = |vs: &[f64]| -> (Vec<f64>, Vec<f64>) {
        let w: Vec<f64> = vs.iter().map(|v| 1.0 / v).collect();
        let total: f64 = w.iter().sum();
        let p = w.iter().map(|x| x / total).collect();
        (w, p)
    };
    for (cells, vs) in kinds.iter().zip(var) {
        let (w, p) = shares(vs);
        let mean: f64 = cells.iter().zip(&p).map(|(o, p)| p * o.mean).sum();
        for (o, wo) in cells.iter().zip(&w) {
            if let Some(x) = c.get_mut(o.m) {
                *x += wo * (o.mean - mean);
            }
            for (o2, p2) in cells.iter().zip(&p) {
                a.add(o.m, o2.m, if o.m == o2.m { *wo } else { 0.0 } - wo * p2);
            }
        }
    }
    for r in 0..mm {
        for col in 0..mm {
            a.add(r, col, j);
        }
    }
    let inv = a.inverse()?;
    let theta = inv.times(&c);
    let mut lplus = inv;
    for r in 0..mm {
        for col in 0..mm {
            lplus.add(r, col, -j);
        }
    }
    let th = |m: usize| theta.get(m).copied().unwrap_or(0.0);
    let resid = kinds
        .iter()
        .zip(var)
        .map(|(cells, vs)| {
            let (w, p) = shares(vs);
            let a_k: f64 = cells.iter().zip(&p).map(|(o, p)| p * (o.mean - th(o.m))).sum();
            cells
                .iter()
                .enumerate()
                .map(|(i, o)| {
                    // Its leverage: its kind's share p_i, and w_i qᵀ L⁺ q for θ, q = e_i − p.
                    let q = |x: usize| f64::from(u8::from(x == i)) - p.get(x).copied().unwrap_or(0.0);
                    let mut quad = 0.0;
                    for (x, ox) in cells.iter().enumerate() {
                        for (y, oy) in cells.iter().enumerate() {
                            quad += q(x) * lplus.get(ox.m, oy.m) * q(y);
                        }
                    }
                    (o.mean - a_k - th(o.m), p.get(i).copied().unwrap_or(0.0) + w.get(i).copied().unwrap_or(0.0) * quad)
                })
                .collect()
        })
        .collect();
    Some(Weighted { theta, lplus, resid })
}

/// The middle of `xs` (the mean of the two middle ones for an even count); none for none.
fn median(mut xs: Vec<f64>) -> Option<f64> {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n == 0 {
        return None;
    }
    let hi = xs.get(n / 2).copied()?;
    if n % 2 == 1 { Some(hi) } else { xs.get(n / 2 - 1).map(|lo| f64::midpoint(*lo, hi)) }
}

/// The least x ≥ 0 at which `f`, decreasing in x, is at most 0 (0 when it is there already; f(0)
/// asked only when `at_zero`, as a zero variance may not be): bisection to f64's own precision,
/// its upper end `scale` (the data's own, so a few dozen asks find it), doubled until f says it's
/// past. None when f has no answer or no end is found.
fn least_root(f: impl Fn(f64) -> Option<f64>, at_zero: bool, scale: f64) -> Option<f64> {
    if at_zero && f(0.0)? <= 0.0 {
        return Some(0.0);
    }
    // An upper end: the scale, else the smallest normal f64, doubled (at most f64's exponent range).
    let mut hi = if scale.is_finite() && scale > 0.0 { scale } else { f64::MIN_POSITIVE };
    while f(hi)? > 0.0 {
        hi *= 2.0;
        if !hi.is_finite() {
            return None;
        }
    }
    let mut lo = 0.0;
    while hi - lo > hi * f64::EPSILON {
        let mid = f64::midpoint(lo, hi);
        if f(mid)? > 0.0 {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(hi)
}

/// The fleet's centre and spread from each machine's θ̂ and its variance: the median of θ̂
/// weighted by 1 / (v + τ²), and τ² the least at which the machines' median |θ̂ − μ| / √(v + τ²) is
/// Φ⁻¹(3/4), each taken again with the other until the centre comes back to one it was.
fn centre_and_spread(theta: &[f64], var: &[f64]) -> (f64, f64) {
    let weighted_median = |tau2: f64| -> f64 {
        // Machines known exactly (no variance at all) outweigh every other: their own median.
        let exact: Vec<f64> = theta.iter().zip(var).filter(|(_, v)| *v + tau2 <= 0.0).map(|(t, _)| *t).collect();
        if let Some(m) = median(exact) {
            return m;
        }
        let mut pairs: Vec<(f64, f64)> = theta.iter().zip(var).map(|(t, v)| (*t, 1.0 / (v + tau2))).collect();
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total: f64 = pairs.iter().map(|p| p.1).sum();
        let mut acc = 0.0;
        for (t, w) in &pairs {
            acc += w;
            if acc >= total / 2.0 {
                return *t;
            }
        }
        pairs.last().map_or(0.0, |p| p.0)
    };
    let (mut tau2, mut seen) = (0.0, Vec::new());
    loop {
        let centre = weighted_median(tau2);
        if seen.contains(&centre.to_bits()) {
            return (centre, tau2);
        }
        seen.push(centre.to_bits());
        // At τ² = (the largest |θ̂ − μ| / Φ⁻¹(3/4))² every machine's |z| is at most Φ⁻¹(3/4): an end.
        let far = theta.iter().map(|t| (t - centre).abs()).fold(0.0, f64::max) / NORMAL_MEDIAN_ABS;
        let spread = least_root(
            |t2| {
                let z: Vec<f64> = theta.iter().zip(var).map(|(t, v)| if v + t2 > 0.0 { (t - centre).abs() / (v + t2).sqrt() } else if *t == centre { 0.0 } else { f64::MAX }).collect();
                Some(median(z)? - NORMAL_MEDIAN_ABS)
            },
            true,
            far * far,
        );
        tau2 = spread.unwrap_or(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic uniform and normal draws.
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

        fn below(&mut self, n: usize) -> usize {
            ((self.uniform() * n as f64) as usize).min(n.saturating_sub(1))
        }
    }

    /// A fleet whose cores' speeds are `speeds` (against an arbitrary unit), each of `kinds` kinds
    /// running `runs` times on each of 2 to all machines drawn at random, with log-normal noise `sd`:
    /// each run's CPU seconds are the kind's reference seconds over its machine's speed.
    fn fleet(speeds: &[f64], kinds: usize, runs: u64, sd: f64, seed: u64) -> Fleet {
        let mut d = Draws(seed);
        let mut f = Fleet::default();
        for k in 0..kinds {
            let work = (1.0 + 99.0 * d.uniform()).ln();
            let on = 2 + d.below(speeds.len() - 1);
            let mut machines: Vec<usize> = (0..speeds.len()).collect();
            for i in 0..machines.len() {
                let j = i + d.below(machines.len() - i);
                machines.swap(i, j);
            }
            for m in machines.iter().take(on) {
                for _ in 0..runs {
                    let s = speeds.get(*m).copied().unwrap_or(1.0);
                    f.observe(&format!("k{k}"), &format!("m{m}"), work - s.ln() + sd * d.normal());
                }
            }
        }
        f
    }

    /// docs/lb.md "The CI engine": a kind's work on the reference core is its runs' CPU-seconds
    /// with each machine's speed taken out: run on machines of every speed, it comes back within
    /// the noise of the work that made it (each machine's runs, its fitted speed put back); a kind
    /// run on one machine alone, likewise.
    #[test]
    fn a_kinds_work_comes_back_with_each_machines_speed_taken_out() -> Result<(), String> {
        let speeds = [0.5, 1.0, 2.0, 3.0];
        let mut f = fleet(&speeds, 40, 6, 0.02, 11);
        // A kind of 30 unit CPU-seconds: on m3 (3x the unit) it takes 10.
        let want = 30f64.ln();
        for m in 0..4 {
            let s = speeds.get(m).copied().unwrap_or(1.0);
            for _ in 0..5 {
                f.observe("probe", &format!("m{m}"), want - s.ln());
            }
        }
        f.observe("alone", "m3", 10f64.ln());
        let fit = f.fit();
        let got = f.work("probe", &fit).ok_or("no work")?;
        // The reference core's work, each machine's θ put back, is that machine's runs.
        for m in 0..4 {
            let s = speeds.get(m).copied().unwrap_or(1.0);
            let theta = fit.machines.get(&format!("m{m}")).map_or(0.0, |x| x.shrunk);
            if (got + theta - (want - s.ln())).abs() > 0.05 {
                return Err(format!("probe on m{m}: {} against {}", got + theta, want - s.ln()));
            }
        }
        let alone = f.work("alone", &fit).ok_or("no work alone")?;
        if (alone - got).abs() > 0.05 {
            return Err(format!("alone on m3: {alone} against the probe's {got}, the same 30 unit CPU-seconds"));
        }
        if f.work("never", &fit).is_some() {
            return Err("a kind never run has work".into());
        }
        Ok(())
    }

    /// The known speeds against their median, the fit's reference (an odd count: a machine's).
    fn centred(speeds: &[f64]) -> Vec<f64> {
        let mut sorted = speeds.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mid = sorted.get(sorted.len() / 2).copied().unwrap_or(1.0);
        speeds.iter().map(|s| s / mid).collect()
    }

    /// W05-R26: on a synthetic fleet with known speeds, the fit recovers each machine's per-core
    /// speed against the fleet's centre, its median machine. A sign mistake (speed for slowness) or
    /// a reference other than the centre would miss by far more than the noise allows (the
    /// geometric mean here, 1.16, is 16% from the median).
    #[test]
    fn w05_r26_fit_recovers_known_speeds() -> Result<(), String> {
        let speeds = [1.0, 1.6, 0.7, 2.1, 0.9];
        let f = fleet(&speeds, 60, 4, 0.05, 11);
        let fit = f.fit();
        assert!(fit.dof > 0, "{fit:?}");
        for (m, want) in centred(&speeds).iter().enumerate() {
            let got = f64::from(fit.speed(&format!("m{m}"))) / 1024.0;
            assert!((got / want - 1.0).abs() < 0.03, "m{m}: {got} for {want}: {fit:?}");
        }
        // σ² is the noise's.
        assert!((fit.sigma2.sqrt() / 0.05 - 1.0).abs() < 0.15, "{}", fit.sigma2.sqrt());
        Ok(())
    }

    /// W05-R26: a kind that ran on one machine says nothing of speed. Planted: one such kind with
    /// an absurd time on the slowest machine must not move any machine's fit.
    #[test]
    fn w05_r26_single_machine_kinds_drop_out() {
        let speeds = [1.0, 2.0, 0.5];
        let mut f = fleet(&speeds, 30, 3, 0.05, 5);
        let before = f.fit();
        for _ in 0..50 {
            f.observe("alone", "m2", 40.0);
        }
        let after = f.fit();
        for m in ["m0", "m1", "m2"] {
            let (b, a) = (before.machines.get(m).map(|x| x.raw), after.machines.get(m).map(|x| x.raw));
            assert!(matches!((b, a), (Some(b), Some(a)) if (a - b).abs() < 1e-9), "{m}: {b:?} {a:?}");
        }
    }

    /// W05-R26: folded in run by run in any order, the fit is the same (it's exact, not a running
    /// approximation), and a fit from the store's sums is the fit from the runs.
    #[test]
    fn w05_r26_incremental_fit_is_exact() -> Result<(), String> {
        let mut d = Draws(3);
        let mut rows = Vec::new();
        for k in 0..20 {
            for m in 0..4 {
                if (k + m) % 3 == 0 {
                    continue;
                }
                for _ in 0..3 {
                    rows.push((format!("k{k}"), format!("m{m}"), f64::from(k) * 0.1 - f64::from(m) * 0.2 + 0.05 * d.normal()));
                }
            }
        }
        let mut a = Fleet::default();
        for (k, m, y) in &rows {
            a.observe(k, m, *y);
        }
        let mut b = Fleet::default();
        for (k, m, y) in rows.iter().rev() {
            b.observe(k, m, *y);
        }
        let mut sums: BTreeMap<(String, String), Cell> = BTreeMap::new();
        for (k, m, y) in &rows {
            sums.entry((k.clone(), m.clone())).or_default().add(*y);
        }
        let mut c = Fleet::default();
        for ((k, m), cell) in &sums {
            c.add(k, m, *cell);
        }
        let (fa, fb, fc) = (a.fit(), b.fit(), c.fit());
        assert!(!fa.machines.is_empty());
        for (name, ma) in &fa.machines {
            for other in [&fb, &fc] {
                let mo = other.machines.get(name).ok_or("same machines")?;
                assert!((ma.raw - mo.raw).abs() < 1e-9 && (ma.shrunk - mo.shrunk).abs() < 1e-9, "{name}: {ma:?} {mo:?}");
            }
        }
        Ok(())
    }

    /// W05-R26: a machine with few runs is shrunk toward the fleet's centre; one with many keeps
    /// its own. Planted: a fit without shrinkage would leave the few-run machine at its raw figure.
    #[test]
    fn w05_r26_few_runs_shrink_toward_the_fleet() -> Result<(), String> {
        let speeds = [1.0, 1.5, 0.8, 1.2];
        let mut f = fleet(&speeds, 40, 3, 0.2, 9);
        // A new machine, much faster by its two runs of one kind the others ran.
        let k0 = (0..40).find(|k| f.keys.get(&format!("k{k}")).is_some_and(|c| c.len() >= 3)).ok_or("a shared kind")?;
        let mean = f.keys.get(&format!("k{k0}")).map(|c| c.values().map(|x| x.mean()).sum::<f64>() / c.len() as f64).ok_or("its cells")?;
        f.observe(&format!("k{k0}"), "new", mean - 2.0);
        f.observe(&format!("k{k0}"), "new", mean - 2.0);
        let fit = f.fit();
        let new = fit.machines.get("new").ok_or("new is measured")?;
        assert!(new.raw < -1.0, "{new:?}");
        assert!(new.shrunk > new.raw && new.shrunk < 0.0, "shrunk toward 0 but not to it: {new:?}");
        let many = fit.machines.get("m1").ok_or("m1")?;
        let kept = many.shrunk / many.raw;
        let new_kept = new.shrunk / new.raw;
        assert!(kept > 0.95 && kept > new_kept, "{many:?} {new:?}");
        Ok(())
    }

    /// W05-R26: nothing is measured before a residual degree of freedom, nor for a machine that
    /// shares no kind with the reference set.
    #[test]
    fn w05_r26_unmeasured_until_determined() {
        let mut f = Fleet::default();
        f.observe("k", "a", 1.0);
        f.observe("k", "b", 2.0);
        assert!(f.fit().machines.is_empty(), "two runs, two machines, one kind: no residual");
        f.observe("k", "a", 1.1);
        let fit = f.fit();
        assert!(fit.speed("a") > 0 && fit.speed("b") > 0, "{fit:?}");
        f.observe("x", "c", 1.0);
        f.observe("x", "d", 1.0);
        assert_eq!(f.fit().speed("c"), 0);
    }

    /// W05-R26: production's history on 2026-11-12, the 57 cells of the 22 kinds that ran on two
    /// or more of its 9 machines (names replaced). Weighing every run alike took one run of one kind
    /// as m8 478x slower than the fleet (speed 5) and two legs as m3 49x faster (50,234): m8's kind
    /// is 4,000x apart across six machines, m3's legs 8,000x apart between Linux and Windows. Taken
    /// kind by kind, the data tells no machine's speed from the fleet's yet, and none is far from it.
    #[test]
    fn w05_r26_a_kind_whose_work_differs_doesnt_make_a_machine_slow() {
        const CELLS: [(&str, &str, u64, f64, f64); 57] = [
            ("k0", "m0", 64, -223.78788900985313, 790.6818067186343), ("k0", "m1", 30, -117.79607974238655, 464.4241473136852),
            ("k0", "m2", 3, -8.278423373946543, 23.368201550575286), ("k1", "m1", 6, 45.78646026328011, 349.44138415419405),
            ("k1", "m3", 1, -1.3862943611198906, 1.9218120556728056), ("k2", "m1", 5, -15.97028133766223, 51.12005899447571),
            ("k2", "m3", 1, -0.9400072584914712, 0.8836136460166515), ("k3", "m2", 1, -4.579307526462364, 20.97005742191486),
            ("k3", "m0", 1, -6.921854203361638, 47.91206561259518), ("k4", "m4", 1, -3.7813018077051366, 14.298243360954134),
            ("k4", "m1", 1, -4.7673362365784815, 22.72749479259428), ("k4", "m0", 1, -4.723728522112227, 22.313611150616563),
            ("k5", "m5", 1, -3.1829538156921378, 10.131194992829139), ("k5", "m6", 1, -3.131529087451499, 9.806474425554818),
            ("k5", "m7", 1, -3.1514975895382524, 9.931937056865415), ("k5", "m2", 1, -2.987407024808001, 8.924600731872193),
            ("k6", "m1", 1, -4.075660207017973, 16.61100612306978), ("k6", "m0", 1, -3.4564986974472447, 11.9473832454545),
            ("k7", "m7", 1, -1.8347964149426141, 3.3664778842862697), ("k7", "m1", 1, -2.6824409355957903, 7.195489372960019),
            ("k8", "m1", 1, -1.8390271928983728, 3.3820210162196687), ("k8", "m5", 1, -0.703292470416385, 0.49462029894438175),
            ("k9", "m5", 1, 1.1541806983804401, 1.3321330845139605), ("k9", "m7", 1, 2.3567289008051815, 5.554171111890399),
            ("k10", "m6", 1, -2.469525547749698, 6.098556430988446), ("k10", "m2", 1, -3.525163241645551, 12.42677588024897),
            ("k10", "m5", 1, -3.277087734616615, 10.739304020374657), ("k10", "m7", 1, 0.94235411185056, 0.8880312721216577),
            ("k10", "m4", 1, 1.6150867277927001, 2.6085051382921316), ("k10", "m8", 1, 4.826432713285359, 23.294452735871076),
            ("k11", "m1", 1, -1.556010896051248, 2.421169908630208), ("k11", "m7", 1, -0.7796008948904859, 0.6077775553140464),
            ("k12", "m1", 1, -3.311402382545941, 10.965385739130934), ("k12", "m4", 1, -1.4161558091540531, 2.005497275800771),
            ("k13", "m4", 1, -3.579271969608787, 12.811187832427164), ("k13", "m1", 1, -3.553055690979562, 12.624204743202254),
            ("k14", "m4", 1, -2.933490137090578, 8.605364384407698), ("k14", "m7", 1, -2.607345506980578, 6.798250592771807),
            ("k14", "m1", 1, -2.8060048744994432, 7.873663355714636), ("k14", "m5", 1, -2.8492797198409474, 8.118394921896908),
            ("k14", "m2", 1, -2.297438360174516, 5.27822301880137), ("k14", "m6", 1, -2.244495446219444, 5.037759808099822),
            ("k15", "m4", 1, 1.6983994366499329, 2.8845606464128095), ("k15", "m1", 6, 10.499009631569463, 19.343634255215786),
            ("k16", "m4", 1, 8.35952126888422, 69.88159584492765), ("k16", "m0", 12, 108.97582497025711, 989.832709591859),
            ("k17", "m4", 1, 8.353588291524833, 69.78243734430077), ("k17", "m0", 11, 92.51824571941437, 778.1828447283277),
            ("k18", "m1", 1, -5.984687562828958, 35.81648522467961), ("k18", "m4", 1, -5.648442379623036, 31.90490131592155),
            ("k19", "m1", 12, 16.694886582172757, 58.34189712585294), ("k19", "m0", 1, -3.9806766487451943, 15.845786581865271),
            ("k19", "m4", 2, 5.275940391738183, 13.920196211630683), ("k20", "m0", 1, -2.802157604532129, 7.85208724063724),
            ("k20", "m4", 1, -2.2917638543624625, 5.25218156416229), ("k21", "m1", 1, -3.697153590566182, 13.66894467223641),
            ("k21", "m4", 1, -3.071771339839581, 9.435779164259854),
        ];
        let mut f = Fleet::default();
        for (k, m, n, sum, sq) in CELLS {
            f.add(k, m, Cell { n, sum, sq });
        }
        let fit = f.fit();
        assert_eq!(fit.machines.len(), 9, "{fit:?}");
        for (name, m) in &fit.machines {
            assert!((512..=2048).contains(&m.speed), "{name}: {m:?} in {fit:?}");
        }
        // m8 and m3 are as far out as their kinds have them; their variances say what that's worth.
        let (m8, m3) = (fit.machines.get("m8"), fit.machines.get("m3"));
        assert!(m8.is_some_and(|m| m.raw > 4.0 && m.var > 1.0), "{m8:?}");
        assert!(m3.is_some_and(|m| m.var > 1.0), "{m3:?}");
    }

    /// The fit as it was: every run weighed alike (this model with no work that differs), σ² from
    /// every residual, τ² by moments about the geometric-mean core; each machine's shrunk θ.
    #[allow(clippy::cast_precision_loss)]
    fn runs_alike(f: &Fleet) -> BTreeMap<String, f64> {
        let Some((members, _)) = f.components().into_iter().reduce(|a, b| if b.1 > a.1 { b } else { a }) else { return BTreeMap::new() };
        let local: BTreeMap<usize, usize> = members.iter().enumerate().map(|(l, g)| (*g, l)).collect();
        let shared: Vec<&BTreeMap<usize, Cell>> = f.keys.values().filter(|c| c.len() >= 2 && c.keys().all(|m| local.contains_key(m))).collect();
        let kinds: Vec<Vec<Obs>> = shared.iter().map(|cells| cells.iter().filter_map(|(m, c)| Some(Obs { m: *local.get(m)?, n: c.n as f64, mean: c.mean() })).collect()).collect();
        let var: Vec<Vec<f64>> = kinds.iter().map(|cells| cells.iter().map(|o| 1.0 / o.n).collect()).collect();
        let Some(w) = weighted(&kinds, &var, members.len()) else { return BTreeMap::new() };
        let mut ss: f64 = shared.iter().flat_map(|c| c.values()).map(Cell::within).sum();
        for (cells, rs) in kinds.iter().zip(&w.resid) {
            for (o, (r, _)) in cells.iter().zip(rs) {
                ss += o.n * r * r;
            }
        }
        let runs: u64 = shared.iter().flat_map(|c| c.values()).map(|c| c.n).sum();
        let mm = members.len();
        let dof = runs.saturating_sub(kinds.len() as u64).saturating_sub(mm as u64 - 1).max(1);
        let sigma2 = ss / dof as f64;
        let v: Vec<f64> = (0..mm).map(|l| sigma2 * w.lplus.get(l, l).max(0.0)).collect();
        let tau2 = ((w.theta.iter().map(|t| t * t).sum::<f64>() - v.iter().sum::<f64>()) / (mm as f64 - 1.0)).max(0.0);
        members
            .iter()
            .enumerate()
            .filter_map(|(l, g)| {
                let (t, vl) = (w.theta.get(l).copied()?, v.get(l).copied()?);
                Some((f.names.get(*g)?.clone(), if tau2 + vl > 0.0 { t * tau2 / (tau2 + vl) } else { 0.0 }))
            })
            .collect()
    }

    /// A fleet whose machines' ln speeds are `theta`, each of `kinds` kinds on 2 to all of them,
    /// one to three runs a cell: each cell's work differs there by a draw of sd `differs`, each run
    /// by one of sd `noise`. With `odd`, one more machine at the fleet's median speed seen in one
    /// kind alone, shared with three others, whose work differs by `odd` on it (the kind's spread
    /// half that). The fleet, and each machine's true ln speed.
    fn differing_fleet(theta: &[f64], kinds: usize, noise: f64, differs: f64, odd: Option<f64>, seed: u64) -> (Fleet, BTreeMap<String, f64>) {
        let mut d = Draws(seed);
        let mut f = Fleet::default();
        let mut truth: BTreeMap<String, f64> = theta.iter().enumerate().map(|(m, t)| (format!("m{m}"), *t)).collect();
        let pick = |d: &mut Draws, on: usize| -> Vec<usize> {
            let mut machines: Vec<usize> = (0..theta.len()).collect();
            for i in 0..machines.len() {
                let j = i + d.below(machines.len() - i);
                machines.swap(i, j);
            }
            machines.truncate(on);
            machines
        };
        for k in 0..kinds {
            let work = 8.0 * d.uniform() - 3.0;
            let on = 2 + d.below(theta.len() - 1);
            for m in pick(&mut d, on) {
                let gamma = differs * d.normal();
                for _ in 0..=d.below(3) {
                    // y = ln CPU-seconds: the kind's work less the machine's ln speed.
                    f.observe(&format!("k{k}"), &format!("m{m}"), work - theta.get(m).copied().unwrap_or(0.0) + gamma + noise * d.normal());
                }
            }
        }
        if let Some(shift) = odd {
            let work = 8.0 * d.uniform() - 3.0;
            let spread = shift.abs() / 2.0;
            for m in pick(&mut d, 3) {
                f.observe("odd", &format!("m{m}"), work - theta.get(m).copied().unwrap_or(0.0) + spread * d.normal());
            }
            let mut sorted = theta.to_vec();
            sorted.sort_by(f64::total_cmp);
            let mid = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
            f.observe("odd", "odd", work - mid + shift + spread * d.normal());
            truth.insert("odd".into(), mid);
        }
        (f, truth)
    }

    /// The root mean square of each machine's error in ln speed, the fit's θ and the truth each
    /// taken against their median over the machines of known speed (the reference doesn't count).
    #[allow(clippy::cast_precision_loss)]
    fn rms(theta: &BTreeMap<String, f64>, truth: &BTreeMap<String, f64>) -> f64 {
        let normal: Vec<&String> = truth.keys().filter(|m| m.starts_with('m')).collect();
        let mid = |x: &BTreeMap<String, f64>| median(normal.iter().filter_map(|m| x.get(*m).copied()).collect()).unwrap_or(0.0);
        // θ is ln CPU time against the centre: ln speed is −θ.
        let (cf, ct) = (mid(&theta.iter().map(|(k, v)| (k.clone(), -v)).collect()), mid(truth));
        let errs: Vec<f64> = truth.iter().filter_map(|(m, t)| Some(((-theta.get(m)?) - cf - (t - ct)).powi(2))).collect();
        (errs.iter().sum::<f64>() / errs.len().max(1) as f64).sqrt()
    }

    /// W05-R26: on synthetic fleets of known speeds, the fit kind by kind is as close as weighing
    /// runs alike where kinds do the same work everywhere, and several times closer where their
    /// work differs from machine to machine and a machine is seen in one such kind alone (both:
    /// five fleets each, the root mean square of their machines' errors in ln speed).
    #[test]
    fn w05_r26_the_robust_fit_beats_runs_alike_on_noisy_kinds() {
        let ln = |s: &[f64]| -> Vec<f64> { s.iter().map(|x| x.ln()).collect() };
        let clean = ln(&[0.5, 0.7, 1.0, 1.4, 2.0]);
        let noisy = ln(&[0.75, 0.82, 0.9, 1.0, 1.0, 1.1, 1.2]);
        let score = |theta: &[f64], kinds: usize, noise: f64, differs: f64, odd: Option<f64>| -> (f64, f64) {
            let (mut ours, mut alike) = (0.0, 0.0);
            for seed in 1..=5 {
                let (f, truth) = differing_fleet(theta, kinds, noise, differs, odd, seed);
                let fit: BTreeMap<String, f64> = f.fit().machines.into_iter().map(|(k, m)| (k, m.shrunk)).collect();
                ours += rms(&fit, &truth) / 5.0;
                alike += rms(&runs_alike(&f), &truth) / 5.0;
            }
            (ours, alike)
        };
        let (ours, alike) = score(&clean, 60, 0.1, 0.15, None);
        assert!(ours < alike + 0.02, "clean: ours {ours}, runs alike {alike}");
        let (ours, alike) = score(&noisy, 22, 0.6, 1.6, Some(6.0));
        assert!(ours < alike / 3.0, "kinds whose work differs: ours {ours}, runs alike {alike}");
    }

    /// W05-R26: a run's y is ln of its CPU seconds, only for one that did its whole work.
    #[test]
    fn w05_r26_y_of_takes_whole_runs_only() {
        let o = Observation { key: "leg:x".into(), kind: "leg".into(), wall_s: 10.0, cpu_s: Some(40.0), exit_code: Some(0), ..Observation::default() };
        assert!(matches!(y_of(&o), Some(y) if (y - 40f64.ln()).abs() < 1e-12));
        assert_eq!(key_of(&o), Some("leg:x"));
        assert_eq!(y_of(&Observation { exit_code: Some(1), ..o.clone() }), None);
        assert!(y_of(&Observation { exit_code: None, ..o.clone() }).is_some(), "a leg's exit isn't told");
        assert_eq!(y_of(&Observation { cause: Some("no_space".into()), ..o.clone() }), None);
        assert_eq!(y_of(&Observation { cpu_s: None, ..o.clone() }), None);
        let cmd = Observation { key: "cmd:here".into(), kind: "command".into(), ..o.clone() };
        assert_eq!(key_of(&cmd), None);
        assert_eq!(key_of(&Observation { work_key: Some("work:1".into()), ..cmd }), Some("work:1"));
    }
}
