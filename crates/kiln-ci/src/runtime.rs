//! Learned run times (docs/needs.md; W05): how long a command or a CI leg takes on a runner, as it
//! is now, learned from what runs took before. Pure: no clock, no disk, no randomness but what the
//! caller passes in.
//!
//! **The model** is Ernest's (Venkataraman et al., NSDI'16), with the CPU and disk terms of the
//! research for kiln's sensing (Apollo, OSDI'14: a run's time is its CPU time plus its I/O time):
//!
//! ```text
//! wall ≈ θ0 + θ1·cpu_s/cpus + θ2·bytes/throughput + θ3·ops·latency
//! ```
//!
//! - `cpu_s`: the CPU time the run used; `cpus`: the CPUs idle on its runner when it started.
//! - `bytes`, `ops`: the I/O it did; `throughput`: the most its runner's work device was seen to
//!   move (a lower bound on what it can; passive, so no probe); `latency`: the device's mean
//!   request time over the run.
//!
//! θ ≥ 0, one θ per key, fitted by non-negative least squares (Lawson & Hanson 1974; Ernest fits
//! its terms the same way, so a term can only add time). With at most four terms the fit is exact:
//! every support (subset of terms) is solved and the best whose solution is non-negative kept,
//! which is the NNLS optimum.
//!
//! **Its data are its sufficient statistics**: per key, the weighted Gram matrix `Σ w·x·xᵀ`, `Σ
//! w·x·y`, `Σ w·y²`, the count, and the work's sums ([`Stats`]). A fit from them equals a fit from
//! every row, so nothing is forgotten and nothing grows past a 4×4 matrix per key: raw rows are
//! kept only as many as one fit with a residual needs ([`ROWS_KEPT`]).
//!
//! **Our own placements bias the data** (a key always sent to the fast machine never shows how it
//! does on the slow one). Each row carries the probability its placement had, and counts with
//! weight `1/p` (inverse propensity, Bottou et al., JMLR 2013 §5): the fit is that of the data a
//! uniform placement would have given. A placement made with certainty has `p = 1`.
//!
//! **Uncertainty, for exploring**: [`Prediction`] carries the fit's posterior (Gaussian, from the
//! weighted residual variance and the Gram's inverse, the standard linear-regression result), so a
//! placer can draw from it (Thompson sampling, Agrawal & Goyal, COLT 2012 / AISTATS 2013): a key
//! with few runs has a wide posterior and gets tried; one with many, a narrow one, and is placed by
//! its mean. No exploration rate is picked.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::facts::RunnerFacts;

/// The model's terms: constant, CPU, bytes, operations.
pub const TERMS: usize = 4;

/// Raw rows kept per key: one more than the model's terms, the fewest that determine a fit and
/// leave one degree of freedom for its residual. Every row is in [`Stats`] as well, so none is
/// lost when an older one goes; the rows are kept to be looked at, and for a refit should the
/// terms change.
pub const ROWS_KEPT: usize = TERMS + 1;

/// One finished run on a runner, as the runner measured it (docs/needs.md). Sent in its facts
/// (`RunnerFacts.runs`), kept by the server, folded into [`Stats`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[serde(default)]
pub struct Observation {
    /// Unique (a UUIDv7): a row sent twice is kept once.
    pub id: String,
    /// What ran: `cmd:<digest>` for a command, `leg:<repo>/<workflow>/<job>` for a CI leg.
    pub key: String,
    /// `command` or `leg`.
    pub kind: String,
    /// What it is, for a person: the command's first words, the job's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When it started, unix ms (the runner's clock).
    pub started_ms: i64,
    /// Its wall time, seconds.
    pub wall_s: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// CPU time its processes used, seconds: its cgroup's `usage_usec`, its job object's, or its
    /// process tree's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_s: Option<f64>,
    /// CPUs idle on the runner when it started (CPUs × idle share, within the runner's quota).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_cpus: Option<f64>,
    /// Bytes it read and wrote, and its I/O operations: its cgroup's `io.stat`, or its job
    /// object's I/O accounting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_ops: Option<u64>,
    /// The share of its time its tasks were stalled on I/O (its cgroup's `io.pressure`), some and
    /// full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_stall_some: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_stall_full: Option<f64>,
    /// The work device over the run: its mean request latency (µs), the bytes per second it moved
    /// (this run's and everything else's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_latency_us: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_bps: Option<f64>,
    /// The most bytes per second the device was seen to move before the run (a lower bound on
    /// what it can).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_peak_bps: Option<f64>,
    /// The probability its placement chose this runner with (Bottou et al. 2013); none for a
    /// placement made with certainty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
    /// The memory its largest process held at its peak (`wait4`'s `ru_maxrss`), bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_rss: Option<u64>,
    /// What of the machine's ended it, when the machine was the cause (`no_space`,
    /// `out_of_memory`, `missing_tool[:<name>]`; `crate`'s sense health, S6): such a run says
    /// nothing of the key's time, and isn't folded into its model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

/// Which disk terms a row (or a prediction) has: bit 0 bytes over throughput, bit 1 operations
/// times latency. The constant and CPU terms are always there.
pub type Mask = u8;
pub const BYTES: Mask = 1;
pub const OPS: Mask = 2;

/// A run's work: what it does wherever it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Work {
    pub cpu_s: f64,
    pub bytes: Option<f64>,
    pub ops: Option<f64>,
}

/// A runner as it is when a run starts there: what the terms divide or multiply the work by.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Now {
    /// CPUs idle (CPUs × idle share, within its quota).
    pub idle_cpus: Option<f64>,
    /// The most bytes per second its work device was seen to move.
    pub peak_bps: Option<f64>,
    /// Its work device's mean request latency now, µs.
    pub latency_us: Option<f64>,
}

/// The terms' values for `work` on a runner `now`, and which disk terms there are; none without
/// idle CPUs (no CPU, no estimate: the run waits for one).
pub fn terms(work: &Work, now: &Now) -> Option<([f64; TERMS], Mask)> {
    let cpus = now.idle_cpus.filter(|c| c.is_finite() && *c > 0.0)?;
    let bytes = work.bytes.zip(now.peak_bps.filter(|t| t.is_finite() && *t > 0.0)).map(|(b, t)| b / t);
    let ops = work.ops.zip(now.latency_us.filter(|l| l.is_finite() && *l >= 0.0)).map(|(o, l)| o * l / 1e6);
    let mask = (if bytes.is_some() { BYTES } else { 0 }) | (if ops.is_some() { OPS } else { 0 });
    Some(([1.0, work.cpu_s / cpus, bytes.unwrap_or(0.0), ops.unwrap_or(0.0)], mask))
}

impl Observation {
    /// Its work, and the runner as it was: none without its CPU time (nothing to learn from).
    pub fn split(&self) -> Option<(Work, Now)> {
        let cpu_s = self.cpu_s.filter(|c| c.is_finite() && *c >= 0.0)?;
        let bytes = match (self.read_bytes, self.write_bytes) {
            (None, None) => None,
            (r, w) => Some(r.unwrap_or(0).saturating_add(w.unwrap_or(0)) as f64),
        };
        let work = Work { cpu_s, bytes, ops: self.io_ops.map(|o| o as f64) };
        Some((work, Now { idle_cpus: self.idle_cpus, peak_bps: self.device_peak_bps, latency_us: self.device_latency_us }))
    }

    /// Its weight: one over its placement's probability.
    pub fn weight(&self) -> f64 {
        self.p.filter(|p| p.is_finite() && *p > 0.0 && *p <= 1.0).map_or(1.0, |p| 1.0 / p)
    }
}

fn at(m: &[[f64; TERMS]; TERMS], i: usize, j: usize) -> f64 {
    m.get(i).and_then(|r| r.get(j)).copied().unwrap_or(0.0)
}

fn put(m: &mut [[f64; TERMS]; TERMS], i: usize, j: usize, v: f64) {
    if let Some(c) = m.get_mut(i).and_then(|r| r.get_mut(j)) {
        *c = v;
    }
}

fn vat(v: &[f64], i: usize) -> f64 {
    v.get(i).copied().unwrap_or(0.0)
}

/// A key's sufficient statistics for one [`Mask`]: what any fit over its rows needs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Rows.
    pub n: u64,
    /// Their weights' sum.
    pub w: f64,
    /// `Σ w·x·xᵀ`, `Σ w·x·y`, `Σ w·y²`.
    pub gram: [[f64; TERMS]; TERMS],
    pub xy: [f64; TERMS],
    pub yy: f64,
    /// `Σ w·cpu_s`, `Σ w·bytes`, `Σ w·ops`: the key's work, on average.
    pub work: [f64; 3],
}

impl Stats {
    /// Adds a row: terms `x`, wall time `y`, its work, its weight.
    pub fn add(&mut self, x: &[f64; TERMS], y: f64, work: &Work, weight: f64) {
        if !(y.is_finite() && weight.is_finite() && weight > 0.0 && x.iter().all(|v| v.is_finite())) {
            return;
        }
        self.n += 1;
        self.w += weight;
        for (i, xi) in x.iter().enumerate() {
            for (j, xj) in x.iter().enumerate() {
                let v = at(&self.gram, i, j) + weight * xi * xj;
                put(&mut self.gram, i, j, v);
            }
            if let Some(c) = self.xy.get_mut(i) {
                *c += weight * xi * y;
            }
        }
        self.yy += weight * y * y;
        let [c, b, o] = &mut self.work;
        *c += weight * work.cpu_s;
        *b += weight * work.bytes.unwrap_or(0.0);
        *o += weight * work.ops.unwrap_or(0.0);
    }

    /// The key's mean work.
    pub fn mean_work(&self, mask: Mask) -> Option<Work> {
        (self.w > 0.0).then(|| {
            let [c, b, o] = self.work;
            Work { cpu_s: c / self.w, bytes: (mask & BYTES != 0).then_some(b / self.w), ops: (mask & OPS != 0).then_some(o / self.w) }
        })
    }
}

/// Solves `a·x = b` for the terms in `s` (a symmetric positive definite submatrix) by Cholesky;
/// none when it isn't positive definite. Also gives the submatrix's inverse.
fn solve(a: &[[f64; TERMS]; TERMS], b: &[f64; TERMS], s: &[usize]) -> Option<([f64; TERMS], [[f64; TERMS]; TERMS])> {
    let k = s.len();
    let mut l = [[0.0f64; TERMS]; TERMS];
    // The scale of the matrix, for what counts as a zero pivot (the arithmetic's own precision).
    let scale = s.iter().map(|i| at(a, *i, *i).abs()).fold(0.0f64, f64::max);
    for i in 0..k {
        for j in 0..=i {
            let (si, sj) = (*s.get(i)?, *s.get(j)?);
            let mut sum = at(a, si, sj);
            for p in 0..j {
                sum -= at(&l, i, p) * at(&l, j, p);
            }
            if i == j {
                if sum <= scale * f64::EPSILON * k as f64 {
                    return None;
                }
                put(&mut l, i, i, sum.sqrt());
            } else {
                let v = sum / at(&l, j, j);
                put(&mut l, i, j, v);
            }
        }
    }
    // L·Lᵀ·x = b, and its inverse column by column.
    let back = |rhs: &[f64; TERMS]| -> [f64; TERMS] {
        let mut y = [0.0; TERMS];
        for i in 0..k {
            let mut v = vat(rhs, i);
            for p in 0..i {
                v -= at(&l, i, p) * vat(&y, p);
            }
            if let Some(c) = y.get_mut(i) {
                *c = v / at(&l, i, i);
            }
        }
        let mut x = [0.0; TERMS];
        for i in (0..k).rev() {
            let mut v = vat(&y, i);
            for p in i + 1..k {
                v -= at(&l, p, i) * vat(&x, p);
            }
            if let Some(c) = x.get_mut(i) {
                *c = v / at(&l, i, i);
            }
        }
        x
    };
    let sub_b: [f64; TERMS] = std::array::from_fn(|i| s.get(i).map_or(0.0, |si| vat(b, *si)));
    let x_sub = back(&sub_b);
    let mut x = [0.0; TERMS];
    let mut inv = [[0.0; TERMS]; TERMS];
    for (i, si) in s.iter().enumerate() {
        if let Some(c) = x.get_mut(*si) {
            *c = vat(&x_sub, i);
        }
        let e: [f64; TERMS] = std::array::from_fn(|r| if r == i { 1.0 } else { 0.0 });
        let col = back(&e);
        for (r, sr) in s.iter().enumerate() {
            put(&mut inv, *sr, *si, vat(&col, r));
        }
    }
    Some((x, inv))
}

/// A key's fitted model.
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    /// θ ≥ 0, zero for a term outside the support.
    pub theta: [f64; TERMS],
    /// θ's covariance (over the support), from the weighted residual variance.
    pub cov: [[f64; TERMS]; TERMS],
    /// The residual variance of one run, seconds².
    pub sigma2: f64,
    /// Degrees of freedom: rows less terms fitted.
    pub dof: u64,
    pub mask: Mask,
}

/// The NNLS fit of `stats`, over the terms `mask` has; none with no more rows than terms fitted
/// (no residual to say its uncertainty with).
pub fn fit(stats: &Stats, mask: Mask) -> Option<Fit> {
    if stats.n == 0 || stats.w <= 0.0 {
        return None;
    }
    // Weights normalized to a mean of one: inverse propensities reweigh the rows, they don't
    // add rows.
    let norm = stats.n as f64 / stats.w;
    let g: [[f64; TERMS]; TERMS] = std::array::from_fn(|i| std::array::from_fn(|j| at(&stats.gram, i, j) * norm));
    let b: [f64; TERMS] = std::array::from_fn(|i| vat(&stats.xy, i) * norm);
    let yy = stats.yy * norm;
    let usable: Vec<usize> = (0..TERMS).filter(|t| *t < 2 || (*t == 2 && mask & BYTES != 0) || (*t == 3 && mask & OPS != 0)).collect();
    let mut best: Option<(f64, Vec<usize>, [f64; TERMS], [[f64; TERMS]; TERMS])> = None;
    for bits in 0u32..(1 << usable.len()) {
        let s: Vec<usize> = usable.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, t)| *t).collect();
        let (theta, inv) = if s.is_empty() {
            ([0.0; TERMS], [[0.0; TERMS]; TERMS])
        } else {
            // A support whose terms don't vary independently in the data has no solution of its own.
            match solve(&g, &b, &s) {
                Some(r) => r,
                None => continue,
            }
        };
        if theta.iter().any(|v| *v < 0.0) {
            continue;
        }
        // Σ w·(y − θ·x)² = yy − 2θ·b + θᵀ·G·θ.
        let quad: f64 = (0..TERMS).map(|i| (0..TERMS).map(|j| vat(&theta, i) * at(&g, i, j) * vat(&theta, j)).sum::<f64>()).sum();
        let rss = (yy - 2.0 * (0..TERMS).map(|i| vat(&theta, i) * vat(&b, i)).sum::<f64>() + quad).max(0.0);
        if best.as_ref().is_none_or(|(r, ..)| rss < *r) {
            best = Some((rss, s, theta, inv));
        }
    }
    let (rss, support, theta, inv) = best?;
    let dof = stats.n.checked_sub(support.len() as u64).filter(|d| *d > 0)?;
    let sigma2 = rss / dof as f64;
    let cov = std::array::from_fn(|i| std::array::from_fn(|j| at(&inv, i, j) * sigma2));
    Some(Fit { theta, cov, sigma2, dof, mask })
}

/// A run's predicted wall time on a runner, and how sure: the fit's mean, the parameters'
/// uncertainty at this point (`sd_mean`), and one run's own spread (`sd_run`). Student-t with
/// `dof` degrees of freedom, as a regression's predictive is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Prediction {
    pub mean_s: f64,
    pub sd_mean_s: f64,
    pub sd_run_s: f64,
    pub dof: u64,
    /// Rows the fit rests on.
    pub runs: u64,
}

impl Prediction {
    /// The predictive spread of one run: both uncertainties.
    pub fn sd_s(&self) -> f64 {
        (self.sd_mean_s * self.sd_mean_s + self.sd_run_s * self.sd_run_s).sqrt()
    }

    /// A Thompson draw of the expected wall time (Agrawal & Goyal): the mean moved by the
    /// parameters' uncertainty times `z`, a standard normal draw the caller makes; never below
    /// zero.
    pub fn thompson(&self, z: f64) -> f64 {
        (self.mean_s + self.sd_mean_s * z).max(0.0)
    }
}

/// Every key's statistics, by mask: what [`Model::predict`] answers from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub stats: BTreeMap<String, BTreeMap<Mask, Stats>>,
}

impl Model {
    /// Folds a finished run in.
    pub fn observe(&mut self, o: &Observation) {
        if o.cause.is_some() {
            return;
        }
        let Some((work, now)) = o.split() else { return };
        let Some((x, mask)) = terms(&work, &now) else { return };
        if !(o.wall_s.is_finite() && o.wall_s >= 0.0) {
            return;
        }
        self.stats.entry(o.key.clone()).or_default().entry(mask).or_default().add(&x, o.wall_s, &work, o.weight());
    }

    /// How long `key` would take on the runner `facts` describes, as it is `now`: from the
    /// richest model of the key that the runner's measures can feed and that has a fit; none for
    /// a key without enough runs (the caller explores), or a runner without idle CPUs.
    pub fn predict(&self, key: &str, facts: &RunnerFacts, now: &Now) -> Option<Prediction> {
        let by_mask = self.stats.get(key)?;
        // Idle CPUs can't be more than the runner has.
        let threads = f64::from(facts.cpu.threads);
        let now = Now { idle_cpus: now.idle_cpus.map(|c| if threads > 0.0 { c.min(threads) } else { c }), ..*now };
        let mut masks: Vec<&Mask> = by_mask.keys().collect();
        masks.sort_by_key(|m| std::cmp::Reverse(m.count_ones()));
        masks.into_iter().find_map(|mask| {
            let stats = by_mask.get(mask)?;
            let f = fit(stats, *mask)?;
            let work = stats.mean_work(*mask)?;
            let (x, have) = terms(&work, &now)?;
            if have & mask != *mask {
                return None;
            }
            let mean: f64 = f.theta.iter().zip(x.iter()).map(|(t, v)| t * v).sum();
            let var_mean: f64 = (0..TERMS).map(|i| (0..TERMS).map(|j| vat(&x, i) * at(&f.cov, i, j) * vat(&x, j)).sum::<f64>()).sum();
            Some(Prediction { mean_s: mean, sd_mean_s: var_mean.max(0.0).sqrt(), sd_run_s: f.sigma2.sqrt(), dof: f.dof, runs: stats.n })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic stream of uniform and normal draws for the tests' data.
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

    /// A run of `key`: its CPU seconds and the idle CPUs it started with, its bytes and the
    /// device's peak bytes/s, its operations and the device's latency (µs), its wall time by `theta`.
    fn obs(key: &str, (cpu_s, idle): (f64, f64), (bytes, peak): (u64, f64), (ops, lat_us): (u64, f64), theta: [f64; 4], noise: f64) -> Observation {
        let wall = theta[0] + theta[1] * cpu_s / idle + theta[2] * bytes as f64 / peak + theta[3] * ops as f64 * lat_us / 1e6 + noise;
        Observation { key: key.into(), wall_s: wall, cpu_s: Some(cpu_s), idle_cpus: Some(idle), read_bytes: Some(bytes), io_ops: Some(ops), device_peak_bps: Some(peak), device_latency_us: Some(lat_us), ..Observation::default() }
    }

    /// docs/needs.md: with exact data, the fit is the model that made it.
    #[test]
    fn w05_nnls_recovers_a_known_model() {
        let theta = [3.0, 1.2, 0.7, 2.0];
        let mut d = Draws(7);
        let mut m = Model::default();
        for _ in 0..40 {
            let o = obs("k", (10.0 + 100.0 * d.uniform(), 1.0 + 15.0 * d.uniform()), ((1e9 * d.uniform()) as u64, 2e8 + 2e9 * d.uniform()), ((1e5 * d.uniform()) as u64, 50.0 + 500.0 * d.uniform()), theta, 0.0);
            m.observe(&o);
        }
        let stats = m.stats.get("k").and_then(|s| s.get(&(BYTES | OPS))).cloned().unwrap_or_default();
        let f = fit(&stats, BYTES | OPS);
        let got = f.map(|f| f.theta.map(|t| (t * 1e6).round() / 1e6));
        assert_eq!(got, Some(theta));
        // And predicts a new runner's: 8 idle CPUs, a device seen to move 1 GB/s at 100 µs now.
        let p = m.predict("k", &RunnerFacts::default(), &Now { idle_cpus: Some(8.0), peak_bps: Some(1e9), latency_us: Some(100.0) });
        let w = stats.mean_work(BYTES | OPS).unwrap_or_default();
        let want = 3.0 + 1.2 * w.cpu_s / 8.0 + 0.7 * w.bytes.unwrap_or(0.0) / 1e9 + 2.0 * w.ops.unwrap_or(0.0) * 100.0 / 1e6;
        assert!(p.is_some_and(|p| (p.mean_s - want).abs() < 1e-6 && p.sd_run_s < 1e-6), "{p:?} vs {want}");
    }

    /// A term that doesn't matter is held at zero, never fitted below it: noise can't make a disk
    /// term take time away.
    #[test]
    fn w05_nnls_keeps_terms_non_negative() {
        let mut d = Draws(11);
        let mut m = Model::default();
        for _ in 0..30 {
            // CPU-bound: wall = 2 + 1·cpu/idle, plus noise; I/O varies and plays no part.
            let o = obs("cpu", (5.0 + 50.0 * d.uniform(), 2.0 + 6.0 * d.uniform()), ((1e8 * d.uniform()) as u64, 1e9), ((1e4 * d.uniform()) as u64, 100.0), [2.0, 1.0, 0.0, 0.0], 0.3 * d.normal());
            m.observe(&o);
        }
        let stats = m.stats.get("cpu").and_then(|s| s.get(&(BYTES | OPS))).cloned().unwrap_or_default();
        let f = fit(&stats, BYTES | OPS);
        assert!(f.as_ref().is_some_and(|f| f.theta.iter().all(|t| *t >= 0.0)), "{f:?}");
        assert!(f.as_ref().is_some_and(|f| (f.theta[1] - 1.0).abs() < 0.1 && (f.theta[0] - 2.0).abs() < 0.6), "{f:?}");
        assert!(f.as_ref().is_some_and(|f| (f.sigma2.sqrt() - 0.3).abs() < 0.15), "the residual says the noise: {f:?}");
    }

    /// No posterior without a residual: rows no more than terms give none, and the caller
    /// explores. The posterior narrows as runs come in, so Thompson draws spread less.
    #[test]
    fn w05_uncertainty_narrows_with_runs() {
        let mut d = Draws(3);
        let mut m = Model::default();
        let now = Now { idle_cpus: Some(4.0), peak_bps: None, latency_us: None };
        let facts = RunnerFacts::default();
        let add = |m: &mut Model, d: &mut Draws| {
            let o = Observation { key: "c".into(), cpu_s: Some(10.0 + 40.0 * d.uniform()), idle_cpus: Some(1.0 + 7.0 * d.uniform()), ..Observation::default() };
            let wall = 1.0 + 0.9 * o.cpu_s.unwrap_or(0.0) / o.idle_cpus.unwrap_or(1.0) + 0.5 * d.normal();
            m.observe(&Observation { wall_s: wall, ..o });
        };
        add(&mut m, &mut d);
        add(&mut m, &mut d);
        assert_eq!(m.predict("c", &facts, &now), None, "two rows, two terms: no residual");
        add(&mut m, &mut d);
        let few = m.predict("c", &facts, &now);
        for _ in 0..200 {
            add(&mut m, &mut d);
        }
        let many = m.predict("c", &facts, &now);
        assert!(few.zip(many).is_some_and(|(f, m)| m.sd_mean_s < f.sd_mean_s / 3.0), "{few:?} → {many:?}");
        assert!(many.is_some_and(|p| (p.thompson(1.0) - p.thompson(-1.0) - 2.0 * p.sd_mean_s).abs() < 1e-9 && p.thompson(-1e9) == 0.0));
        assert_eq!(m.predict("c", &facts, &Now { idle_cpus: Some(0.0), ..now }), None, "no idle CPU, no estimate");
        assert_eq!(m.predict("unknown", &facts, &now), None);
    }

    /// Bottou et al. 2013: a row placed with probability p counts 1/p, as p⁻¹ copies of it would.
    #[test]
    fn w05_rows_count_by_inverse_propensity() {
        let mut d = Draws(5);
        let rows: Vec<Observation> = (0..12)
            .map(|i| Observation { key: "w".into(), wall_s: 3.0 + 2.0 * d.uniform(), cpu_s: Some(1.0 + 9.0 * d.uniform()), idle_cpus: Some(2.0), p: Some(if i % 3 == 0 { 0.25 } else { 1.0 }), ..Observation::default() })
            .collect();
        let mut weighted = Model::default();
        let mut copied = Model::default();
        for o in &rows {
            weighted.observe(o);
            let copies = (o.weight()).round() as usize;
            for _ in 0..copies {
                copied.observe(&Observation { p: None, ..o.clone() });
            }
        }
        let theta = |m: &Model| m.stats.get("w").and_then(|s| s.get(&0)).and_then(|s| fit(s, 0)).map(|f| f.theta.map(|t| (t * 1e9).round() / 1e9));
        assert!(theta(&weighted).is_some());
        assert_eq!(theta(&weighted), theta(&copied));
        assert_eq!(Observation { p: Some(0.0), ..Observation::default() }.weight(), 1.0, "no probability is none");
    }
}
