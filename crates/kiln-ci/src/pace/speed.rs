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
//! **Solved exactly, incrementally.** Eliminating the a's leaves L θ = c in θ = −b: L the
//! machines' weighted Laplacian, L_mm' = Σ_k (n_km δ_mm' − n_km n_km' / N_k), and c_m = Σ_k (s_km −
//! n_km S_k / N_k), with n, s a cell's runs and their y's sum and N, S a kind's. Each kind adds its
//! own terms, so a run takes its kind's out, changes its cell and puts them back, O(its kind's
//! machines²); the solve is the machines' own system, O(M³). L's null space is the constants on
//! each connected set of machines: θ is fixed by the reference, the geometric-mean core (Σθ = 0) of
//! the connected set with the most runs, solved as (L + J/M) θ = c (J all ones); a machine outside
//! it shares no work with it and its speed is unmeasured (0, which the load-balancing kernel reads
//! as the reference: the fleet's centre, the prior below with no data).
//!
//! **Shrunk toward the fleet** (empirical Bayes: Efron & Morris, "Data analysis using Stein's
//! estimator and its generalizations", JASA 70, 1975; Morris, "Parametric empirical Bayes
//! inference", JASA 78, 1983). Each θ̂_m has sampling variance v_m = σ² L⁺_mm, with σ² the residual
//! variance (within each kind-and-machine cell, and of the cells about the additive fit) and L⁺ =
//! (L + J/M)⁻¹ − J/M. The fleet's spread τ² is estimated by moments, E Σθ̂² = (M − 1) τ² + Σ v, and
//! each machine's posterior mean is θ̂_m τ² / (τ² + v_m): one with few runs (a large v) stays near
//! the reference, one with many keeps its own. Until the fit has a residual degree of freedom (σ²
//! can't be estimated before), nothing is measured, as `runtime`'s fits wait for theirs.

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
    /// θ̂, ln of its CPU time against the reference core's for the same work (−b̂).
    pub raw: f64,
    /// θ̂'s sampling variance.
    pub var: f64,
    /// θ̂ shrunk toward the fleet.
    pub shrunk: f64,
    /// Its per-core speed, in 1/1024 of the reference core's: 1024 · exp(−shrunk).
    pub speed: u32,
}

/// The fleet's fit.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Fit {
    /// The machines of the reference set, by name.
    pub machines: BTreeMap<String, Machine>,
    /// The residual variance of a run's y, and its degrees of freedom.
    pub sigma2: f64,
    pub dof: u64,
    /// The fleet's spread of θ, by moments.
    pub tau2: f64,
}

impl Fit {
    /// Machine `name`'s per-core speed in 1/1024 of the reference core's; 0 when unmeasured.
    pub fn speed(&self, name: &str) -> u32 {
        self.machines.get(name).map_or(0, |m| m.speed)
    }
}

/// Every kind's cells and the normal equations they make, kept as each run is folded in.
#[derive(Debug, Clone, Default)]
pub struct Fleet {
    names: Vec<String>,
    index: BTreeMap<String, usize>,
    keys: BTreeMap<String, BTreeMap<usize, Cell>>,
    l: Dense,
    c: Vec<f64>,
}

impl Fleet {
    /// Machine `name`'s place in the equations, made room for if it's new.
    fn machine(&mut self, name: &str) -> usize {
        if let Some(i) = self.index.get(name) {
            return *i;
        }
        let i = self.names.len();
        self.names.push(name.to_owned());
        self.index.insert(name.to_owned(), i);
        if i >= self.l.n {
            let n = (self.l.n.saturating_mul(2)).max(i + 1);
            let mut l = Dense::zero(n);
            for r in 0..self.l.n {
                for c in 0..self.l.n {
                    l.set(r, c, self.l.get(r, c));
                }
            }
            self.l = l;
        }
        if self.c.len() <= i {
            self.c.resize(i + 1, 0.0);
        }
        i
    }

    /// A kind's terms in L and c, times `sign` (1 to put them in, −1 to take them out).
    #[allow(clippy::cast_precision_loss)]
    fn apply(&mut self, cells: &BTreeMap<usize, Cell>, sign: f64) {
        let total: u64 = cells.values().map(|c| c.n).sum();
        if cells.len() < 2 || total == 0 {
            return;
        }
        let nk = total as f64;
        let sk: f64 = cells.values().map(|c| c.sum).sum();
        for (m, cm) in cells {
            let n = cm.n as f64;
            if let Some(v) = self.c.get_mut(*m) {
                *v += sign * (cm.sum - n * sk / nk);
            }
            for (m2, c2) in cells {
                let d = if m == m2 { n } else { 0.0 } - n * c2.n as f64 / nk;
                self.l.add(*m, *m2, sign * d);
            }
        }
    }

    /// Cell `cell` of kind `key` on `machine` joins what's kept (one run's, or the store's sums
    /// as it starts).
    pub fn add(&mut self, key: &str, machine: &str, cell: Cell) {
        if cell.n == 0 || !(cell.sum.is_finite() && cell.sq.is_finite()) {
            return;
        }
        let m = self.machine(machine);
        let mut cells = self.keys.remove(key).unwrap_or_default();
        self.apply(&cells, -1.0);
        let had = cells.get(&m).copied().unwrap_or_default();
        cells.insert(m, had.plus(cell));
        self.apply(&cells, 1.0);
        self.keys.insert(key.to_owned(), cells);
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

    /// A finished run of kind `key` on `machine`, its `y` ([`y_of`]).
    pub fn observe(&mut self, key: &str, machine: &str, y: f64) {
        let mut cell = Cell::default();
        cell.add(y);
        self.add(key, machine, cell);
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
    /// fleet; nothing before the fit has a residual degree of freedom.
    #[allow(clippy::cast_precision_loss)]
    pub fn fit(&self) -> Fit {
        // The reference set: the most runs; between equals, the one its first machine came first.
        let Some((members, _)) = self.components().into_iter().reduce(|a, b| if b.1 > a.1 { b } else { a }) else { return Fit::default() };
        let mm = members.len();
        if mm < 2 {
            return Fit::default();
        }
        let local: BTreeMap<usize, usize> = members.iter().enumerate().map(|(l, g)| (*g, l)).collect();
        let j = 1.0 / mm as f64;
        let mut a = Dense::zero(mm);
        for (li, gi) in members.iter().enumerate() {
            for (lj, gj) in members.iter().enumerate() {
                a.set(li, lj, self.l.get(*gi, *gj) + j);
            }
        }
        let Some(inv) = a.inverse() else { return Fit::default() };
        let c: Vec<f64> = members.iter().map(|g| self.c.get(*g).copied().unwrap_or(0.0)).collect();
        let theta = inv.times(&c);
        let th = |g: usize| local.get(&g).and_then(|l| theta.get(*l)).copied().unwrap_or(0.0);
        // Residuals of the kinds in the set: within their cells, and of their cells about the fit.
        let (mut ss, mut runs, mut kinds) = (0.0, 0u64, 0u64);
        let mut per: BTreeMap<usize, u64> = BTreeMap::new();
        for cells in self.keys.values().filter(|c| c.len() >= 2) {
            if !cells.keys().all(|m| local.contains_key(m)) {
                continue;
            }
            let nk: u64 = cells.values().map(|c| c.n).sum();
            let sk: f64 = cells.values().map(|c| c.sum).sum();
            let ak = (sk - cells.iter().map(|(m, c)| c.n as f64 * th(*m)).sum::<f64>()) / nk as f64;
            for (m, cell) in cells {
                let d = cell.mean() - ak - th(*m);
                ss += cell.within() + cell.n as f64 * d * d;
                *per.entry(*m).or_default() += cell.n;
            }
            runs = runs.saturating_add(nk);
            kinds += 1;
        }
        let dof = runs.saturating_sub(kinds).saturating_sub(mm as u64 - 1);
        if dof == 0 {
            return Fit::default();
        }
        let sigma2 = ss / dof as f64;
        let var: Vec<f64> = (0..mm).map(|l| (sigma2 * (inv.get(l, l) - j)).max(0.0)).collect();
        let sum_sq: f64 = theta.iter().map(|t| t * t).sum();
        let tau2 = ((sum_sq - var.iter().sum::<f64>()) / (mm as f64 - 1.0)).max(0.0);
        let mut machines = BTreeMap::new();
        for (l, g) in members.iter().enumerate() {
            let (raw, v) = (theta.get(l).copied().unwrap_or(0.0), var.get(l).copied().unwrap_or(0.0));
            let shrunk = if tau2 + v > 0.0 { raw * tau2 / (tau2 + v) } else { 0.0 };
            let Some(name) = self.names.get(*g) else { continue };
            machines.insert(name.clone(), Machine { runs: per.get(g).copied().unwrap_or(0), raw, var: v, shrunk, speed: in_ones((-shrunk).exp()) });
        }
        Fit { machines, sigma2, dof, tau2 }
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
    /// the noise of the work that made it; a kind run on one machine alone, likewise.
    #[test]
    fn a_kinds_work_comes_back_with_each_machines_speed_taken_out() -> Result<(), String> {
        let speeds = [0.5, 1.0, 2.0, 3.0];
        let mut f = fleet(&speeds, 40, 6, 0.02, 11);
        // A kind of 30 reference CPU-seconds: on m3 (3x the unit) it takes 10.
        let want = 30f64.ln();
        let centre = centred(&speeds);
        for m in 0..4 {
            let s = speeds.get(m).copied().unwrap_or(1.0);
            for _ in 0..5 {
                f.observe("probe", &format!("m{m}"), want - s.ln());
            }
        }
        f.observe("alone", "m3", 10f64.ln());
        let fit = f.fit();
        // The fit's reference is the fleet's geometric-mean core g: 30 unit CPU-seconds are 30/g
        // of its (`centre` is each speed over g, so its first over the first speed is 1/g).
        let per_g = centre.first().copied().unwrap_or(1.0) / speeds.first().copied().unwrap_or(1.0);
        let got = f.work("probe", &fit).ok_or("no work")?;
        let expect = want + per_g.ln();
        if (got - expect).abs() > 0.05 {
            return Err(format!("probe: {got} against {expect}"));
        }
        let alone = f.work("alone", &fit).ok_or("no work alone")?;
        if (alone - expect).abs() > 0.05 {
            return Err(format!("alone on m3: {alone} against {expect}"));
        }
        if f.work("never", &fit).is_some() {
            return Err("a kind never run has work".into());
        }
        Ok(())
    }

    /// The known speeds against their geometric mean, the fit's reference.
    fn centred(speeds: &[f64]) -> Vec<f64> {
        let mean = speeds.iter().map(|s| s.ln()).sum::<f64>() / speeds.len() as f64;
        speeds.iter().map(|s| (s.ln() - mean).exp()).collect()
    }

    /// W05-R26: on a synthetic fleet with known speeds, the fit recovers each machine's per-core
    /// speed against the fleet's geometric-mean core. A sign mistake (speed for slowness) or a
    /// reference other than the geometric mean would miss by far more than the noise allows.
    #[test]
    fn w05_r26_fit_recovers_known_speeds() -> Result<(), String> {
        let speeds = [1.0, 1.6, 0.7, 2.1, 1.25];
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
