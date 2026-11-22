//! Amphora (PL.7; `docs/design/planner.md`, "Decision (lane pl7)"): the cost model's weights fitted
//! to the times statements take where the engine runs. Each sampled statement gives its counted
//! work, one count per term the planner weighs (rows visited, seeks, hash table rows added and
//! looked up, sort units, page reads from the file), and the time it ran. A regression of the time
//! on the counts gives each term's nanoseconds a unit; the planner reads them as multiples of a page
//! read, its unit (`CostModelParams::with_fitted`). A term with no samples yet, or one the samples
//! can't tell apart from the others or from nothing, has no fitted weight and keeps the learned
//! calibration's (`learned_costs`).
//!
//! The counts are the work counted as the statement ran, not the planner's estimates of it: the
//! fit is of the cost model, and estimated rows would fold the cardinality errors into the weights
//! (Leis et al., "How Good Are Query Optimizers, Really?", PVLDB 9(3), 2015, section 5: the cost
//! model is judged and tuned on true cardinalities; AC-PL.7.1 measures it so).
//!
//! The fit:
//! - minimizes the residuals relative to each statement's time, as statement times span orders of
//!   magnitude and their noise grows with them (a weighted least squares with weights 1/time²);
//! - weighs each sample by Huber's function of its residual against the fit as it stood when the
//!   sample came (Huber, "Robust Estimation of a Location Parameter", Ann. Math. Statist. 35(1),
//!   1964), at the tuning constant 1.345 residual scales that keeps 95% of least squares' efficiency
//!   on normal noise (Holland and Welsch, "Robust regression using iteratively reweighted
//!   least-squares", Comm. Statist. A6(9), 1977); the scale is the residuals' mean absolute value
//!   times sqrt(pi/2), a normal's standard deviation from its mean absolute deviation;
//! - keeps no weight negative: non-negative least squares by Lawson and Hanson's active-set method
//!   ("Solving Least Squares Problems", 1974, chapter 23), on the normal equations as Bro and De
//!   Jong give it ("A fast non-negativity-constrained least squares algorithm", J. Chemometrics
//!   11(5), 1997).
//!
//! Memory is bounded by construction: the fit keeps the normal equations (a square of the terms
//! and a vector) of two epochs, never the samples. The current epoch fills while the previous one
//! stands; the fit reads both. An epoch closes once it alone determines every weight the previous
//! one did and holds its learned length; its length doubles while consecutive epochs' fits agree
//! within their standard errors and halves when they don't, so the fit averages over long windows while conditions hold and follows them when they
//! move (the adaptive window of Bifet and Gavaldà, "Learning from Time-Changing Data with Adaptive
//! Windowing", SDM 2007, on the fit's own spread).
//!
//! Statements are sampled at a rate learned against the timer's cost and the measurement's spread
//! (`Sampler`): timing a statement adds its clock reads to its run, so a statement is timed only
//! as often as that cost stays within the spread the measurement has anyway.

use crate::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

/// The terms the planner weighs, each counted in its own unit as the statement runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Term {
    /// A row visited by a cursor's step.
    Row,
    /// One search of a b-tree for a key.
    Seek,
    /// One row added to a hash table.
    HashBuild,
    /// One lookup of a hash table.
    HashProbe,
    /// Sorting, in rows times log2 rows, as the planner counts a sort.
    Sort,
    /// One page read from the database's file. The planner's unit.
    PageRead,
}

/// How many terms there are: the length of `Term`'s list, a format value.
pub(crate) const TERMS: usize = 6;

impl Term {
    #[cfg(test)]
    pub(crate) const ALL: [Term; TERMS] = [
        Term::Row,
        Term::Seek,
        Term::HashBuild,
        Term::HashProbe,
        Term::Sort,
        Term::PageRead,
    ];

    fn slot(self) -> usize {
        match self {
            Term::Row => 0,
            Term::Seek => 1,
            Term::HashBuild => 2,
            Term::HashProbe => 3,
            Term::Sort => 4,
            Term::PageRead => 5,
        }
    }
}

/// One statement's counted work, a count per term.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Counts([f64; TERMS]);

impl Counts {
    pub(crate) fn add(&mut self, term: Term, units: f64) {
        if let Some(c) = self.0.get_mut(term.slot()) {
            *c += units;
        }
    }

    #[cfg(test)]
    pub(crate) fn get(&self, term: Term) -> f64 {
        self.0.get(term.slot()).copied().unwrap_or(0.0)
    }
}

/// Huber's tuning constant in residual scales (Holland and Welsch 1977: 95% efficiency at the
/// normal).
const HUBER_K: f64 = 1.345;

/// Two fits agree when each weight both determine differs by no more than its combined standard
/// errors times the normal quantile of a two-sided 95% test, Bonferroni-corrected for the `m`
/// weights compared (Dunn, "Multiple Comparisons Among Means", JASA 56(293), 1961): the quantile
/// of 1 - 0.025/m, entry m - 1 here, from the standard normal distribution. 95% is the confidence
/// Huber's constant is chosen at.
const AGREE_Z: [f64; TERMS] = [
    1.959_964, 2.241_403, 2.393_980, 2.497_705, 2.575_829, 2.638_257,
];

type Square = [[f64; TERMS]; TERMS];

/// One epoch's weighted normal equations, in the relative-error form: each sample's counts divided
/// by its time, its target 1.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Normal {
    gram: Square,
    rhs: [f64; TERMS],
    /// Sum of the samples' weighted squared targets (each 1 times its weight): with the solution,
    /// the weighted residual sum of squares, `yty - 2 w·rhs + w·G·w`.
    yty: f64,
    samples: u64,
    /// The sum of the absolute relative residuals against the fit as each sample came, and how
    /// many: the residual scale.
    abs_residuals: f64,
    residuals: u64,
}

impl Normal {
    fn add(&mut self, x: &[f64; TERMS], weight: f64) {
        for (row, xi) in self.gram.iter_mut().zip(x.iter()) {
            for (g, xj) in row.iter_mut().zip(x.iter()) {
                *g += weight * xi * xj;
            }
        }
        for (r, xi) in self.rhs.iter_mut().zip(x.iter()) {
            *r += weight * xi;
        }
        self.yty += weight;
        self.samples = self.samples.saturating_add(1);
    }

    fn plus(&self, other: &Normal) -> Normal {
        let mut sum = *self;
        for (row, orow) in sum.gram.iter_mut().zip(other.gram.iter()) {
            for (g, o) in row.iter_mut().zip(orow.iter()) {
                *g += o;
            }
        }
        for (r, o) in sum.rhs.iter_mut().zip(other.rhs.iter()) {
            *r += o;
        }
        sum.yty += other.yty;
        sum.samples = sum.samples.saturating_add(other.samples);
        sum.abs_residuals += other.abs_residuals;
        sum.residuals = sum.residuals.saturating_add(other.residuals);
        sum
    }

    /// The residuals' scale, a normal's standard deviation from their mean absolute value; None
    /// before any residual.
    fn scale(&self) -> Option<f64> {
        (self.residuals > 0)
            .then(|| {
                self.abs_residuals / self.residuals as f64 * std::f64::consts::FRAC_PI_2.sqrt()
            })
            .filter(|s| s.is_finite() && *s > 0.0)
    }
}

/// A fit: each term's nanoseconds a unit where the samples determine it, and its standard error.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Solution {
    weights: [Option<f64>; TERMS],
    errors: [f64; TERMS],
}

impl Solution {
    fn determined(&self) -> [bool; TERMS] {
        self.weights.map(|w| w.is_some())
    }

    /// The weights the samples tell from nothing, the ones the planner may read (T-PL.7.6): each
    /// above zero by more than its standard error times [AGREE_Z]'s quantile for the weights
    /// determined, the same two-sided 95% test, Bonferroni-corrected, that epochs agree by. A
    /// weight within that of zero (few samples, a loaded host's spread, no degree of freedom left
    /// and so an unbounded error) is no measurement of its cost: like a weight held at zero, it
    /// keeps the learned value, so noise alone never moves a plan.
    fn told_from_nothing(&self) -> FittedWeights {
        let determined = self.weights.iter().filter(|w| w.is_some()).count();
        let mut nanos = [None; TERMS];
        let Some(z) = determined.checked_sub(1).and_then(|at| AGREE_Z.get(at)) else {
            return FittedWeights { nanos };
        };
        for ((slot, w), e) in nanos
            .iter_mut()
            .zip(self.weights.iter())
            .zip(self.errors.iter())
        {
            *slot = w.filter(|w| *w > z * e);
        }
        FittedWeights { nanos }
    }

    /// Whether `other` agrees with this fit on every weight both determine.
    fn agrees(&self, other: &Solution) -> bool {
        let pairs = || {
            self.weights
                .iter()
                .zip(other.weights.iter())
                .zip(self.errors.iter().zip(other.errors.iter()))
                .filter_map(|((a, b), (ea, eb))| Some((a.as_ref()?, b.as_ref()?, ea, eb)))
        };
        let compared = pairs().count();
        let Some(z) = compared.checked_sub(1).and_then(|at| AGREE_Z.get(at)) else {
            return true;
        };
        pairs().all(|(a, b, ea, eb)| (a - b).abs() <= z * (ea * ea + eb * eb).sqrt())
    }
}

/// The fitted weights, nanoseconds a unit of each term; None where no samples determine it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct FittedWeights {
    nanos: [Option<f64>; TERMS],
}

impl FittedWeights {
    pub(crate) fn nanos(&self, term: Term) -> Option<f64> {
        self.nanos.get(term.slot()).copied().flatten()
    }

    /// `term`'s weight in the planner's unit, page reads, given `fallback`, the weight the
    /// parameters hold now for it, and `row_fallback`, a row visit's. A term whose weight and the
    /// page read's are fitted is their ratio; one whose weight and the row visit's are fitted is
    /// that ratio times the row visit's weight in pages (fitted or learned); otherwise `fallback`.
    pub(crate) fn in_pages(&self, term: Term, fallback: f64, row_fallback: f64) -> f64 {
        let Some(w) = self.nanos(term) else {
            return fallback;
        };
        if let Some(page) = self.nanos(Term::PageRead).filter(|p| *p > 0.0) {
            return w / page;
        }
        match self.nanos(Term::Row).filter(|r| *r > 0.0) {
            Some(row) if term != Term::Row => row_fallback * w / row,
            _ => fallback,
        }
    }
}

/// A database's fit, shared by its connections behind a lock.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CostFit {
    previous: Normal,
    current: Normal,
    /// The previous epoch's own fit, what the current one is compared with when it closes.
    previous_fit: Option<Solution>,
    /// Samples an epoch holds before it may close: 0, the least that determines the weights, at
    /// first; doubled while consecutive epochs agree, halved when they don't.
    epoch: u64,
    /// The fit over both epochs as it stands.
    fitted: Option<Solution>,
}

impl CostFit {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds a statement's counted work, `counts`, and the nanoseconds it ran, `nanos`. A sample
    /// with no time or no work says nothing about the weights and is left out.
    pub(crate) fn observe(&mut self, counts: &Counts, nanos: f64) {
        if !(nanos.is_finite() && nanos > 0.0) {
            return;
        }
        if counts.0.iter().any(|c| !c.is_finite() || *c < 0.0) || counts.0.iter().all(|c| *c == 0.0)
        {
            return;
        }
        let x = counts.0.map(|c| c / nanos);
        let mut weight = 1.0;
        if let Some(fit) = self.fitted.as_ref() {
            let predicted: f64 = fit
                .weights
                .iter()
                .zip(x.iter())
                .map(|(w, xi)| w.unwrap_or(0.0) * xi)
                .sum();
            let residual = (1.0 - predicted).abs();
            if let Some(scale) = self.previous.plus(&self.current).scale() {
                let cut = HUBER_K * scale;
                if residual > cut {
                    weight = cut / residual;
                }
            }
            self.current.abs_residuals += residual;
            self.current.residuals = self.current.residuals.saturating_add(1);
        }
        self.current.add(&x, weight);
        self.close_epoch();
        self.fitted = solve(&self.previous.plus(&self.current));
    }

    /// Closes the current epoch once it holds its length and alone determines every weight the
    /// previous one did, and learns the next length from whether the two agree.
    fn close_epoch(&mut self) {
        if self.current.samples < self.epoch {
            return;
        }
        let Some(current) = solve(&self.current) else {
            return;
        };
        if let Some(previous) = self.previous_fit.as_ref() {
            let covers = previous
                .determined()
                .iter()
                .zip(current.determined().iter())
                .all(|(p, c)| !p || *c);
            // An epoch that doesn't determine a term the previous one did still closes once it
            // holds as many samples as that one: the work no longer comes, and the term goes back
            // to its learned weight.
            if !covers && self.current.samples < self.previous.samples {
                return;
            }
            // Agreement doubles the window; disagreement halves it, so one false alarm at the
            // test's 5% costs half a window, not all of it.
            self.epoch = if previous.agrees(&current) {
                self.current.samples.saturating_mul(2)
            } else {
                self.current.samples.checked_div(2).unwrap_or(0)
            };
        }
        self.previous = self.current;
        self.previous_fit = Some(current);
        self.current = Normal::default();
    }

    /// The weights as they stand; None before any term is determined.
    pub(crate) fn weights(&self) -> Option<FittedWeights> {
        Some(self.fitted.as_ref()?.told_from_nothing())
    }

    /// The relative spread of the measurement against the fit, the residuals' scale; None before
    /// a fit.
    pub(crate) fn spread(&self) -> Option<f64> {
        self.previous.plus(&self.current).scale()
    }

    #[cfg(test)]
    fn epoch_len(&self) -> u64 {
        self.epoch
    }
}

/// The terms the normal equations can determine apart from one another: each column with samples
/// whose part the columns taken before it can't explain (its Schur complement, the pivot a Cholesky
/// factorization would take) keeps more than the arithmetic's precision of its own size (Golub and
/// Van Loan, "Matrix Computations", 4th ed., section 4.2.8, on semidefinite matrices).
fn identifiable(gram: &Square) -> [bool; TERMS] {
    let mut keep = [false; TERMS];
    let mut taken: Vec<usize> = Vec::with_capacity(TERMS);
    for k in 0..TERMS {
        let Some(column) = gram.get(k) else {
            continue;
        };
        let diag = column.get(k).copied().unwrap_or(0.0);
        if !(diag.is_finite() && diag > 0.0) {
            continue;
        }
        // The column's part the columns taken can't explain: its Schur complement,
        // G_kk - G_kT G_TT^-1 G_Tk (the Gram matrix is symmetric, so its row is its column).
        let explained = if taken.is_empty() {
            0.0
        } else {
            match sub_solve(gram, &taken, Some(column)) {
                Some(x) => taken
                    .iter()
                    .zip(x.iter())
                    .map(|(&j, xj)| column.get(j).copied().unwrap_or(0.0) * xj)
                    .sum(),
                None => continue,
            }
        };
        if diag - explained > diag * f64::EPSILON * TERMS as f64 {
            taken.push(k);
            if let Some(slot) = keep.get_mut(k) {
                *slot = true;
            }
        }
    }
    keep
}

/// Solves the normal equations restricted to `set` for `rhs` (or the unit vector `unit` of the
/// set's last member when `rhs` is None, for an inverse's column): Gaussian elimination with
/// partial pivoting on the small square. Returns the solution over `set`'s order, or None if the
/// square is singular.
fn sub_solve(gram: &Square, set: &[usize], rhs: Option<&[f64; TERMS]>) -> Option<Vec<f64>> {
    let n = set.len();
    let mut m: Vec<Vec<f64>> = Vec::with_capacity(n);
    for (row_at, &i) in set.iter().enumerate() {
        let grow = gram.get(i)?;
        let mut row: Vec<f64> = Vec::with_capacity(n.checked_add(1)?);
        for &j in set {
            row.push(*grow.get(j)?);
        }
        let b = match rhs {
            Some(rhs) => *rhs.get(i)?,
            None => {
                if row_at.checked_add(1)? == n {
                    1.0
                } else {
                    0.0
                }
            }
        };
        row.push(b);
        m.push(row);
    }
    for col in 0..n {
        let (best, _) = m
            .iter()
            .enumerate()
            .skip(col)
            .map(|(r, row)| (r, row.get(col).copied().unwrap_or(0.0).abs()))
            .fold(
                (col, -1.0),
                |acc, cur| if cur.1 > acc.1 { cur } else { acc },
            );
        m.swap(col, best);
        let pivot_row = m.get(col)?.clone();
        let pivot = *pivot_row.get(col)?;
        if !(pivot.is_finite() && pivot.abs() > 0.0) {
            return None;
        }
        for row in m.iter_mut().skip(col.checked_add(1)?) {
            let factor = row.get(col).copied().unwrap_or(0.0) / pivot;
            for (v, p) in row.iter_mut().zip(pivot_row.iter()).skip(col) {
                *v -= factor * p;
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let row = m.get(r)?;
        let mut acc = *row.get(n)?;
        for (c, xc) in x.iter().enumerate().skip(r.checked_add(1)?) {
            acc -= row.get(c)? * xc;
        }
        let v = acc / row.get(r)?;
        *x.get_mut(r)? = v;
    }
    Some(x)
}

/// The non-negative least squares fit of the normal equations `normal` over its identifiable
/// terms (Lawson and Hanson's NNLS, in Bro and De Jong's normal-equation form), with each
/// weight's standard error; None if no term is identifiable.
fn solve(normal: &Normal) -> Option<Solution> {
    let keep = identifiable(&normal.gram);
    let candidates: Vec<usize> = (0..TERMS)
        .filter(|k| keep.get(*k).copied().unwrap_or(false))
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let g = &normal.gram;
    let b = &normal.rhs;
    let scale_b = b.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    let tol = scale_b * f64::EPSILON * TERMS as f64;
    let mut w = [0.0; TERMS];
    let mut passive: Vec<usize> = Vec::with_capacity(TERMS);
    // Lawson and Hanson prove the method ends; the outer loop is bounded at three passes a term,
    // the bound MATLAB's lsqnonneg uses (its documented default, 3 times the variables).
    let most = TERMS.saturating_mul(3);
    for _ in 0..most {
        let gradient = |w: &[f64; TERMS], k: usize| -> f64 {
            let row = g
                .get(k)
                .map(|r| r.iter().zip(w.iter()).map(|(a, x)| a * x).sum::<f64>())
                .unwrap_or(0.0);
            b.get(k).copied().unwrap_or(0.0) - row
        };
        let next = candidates
            .iter()
            .filter(|k| !passive.contains(k))
            .map(|&k| (k, gradient(&w, k)))
            .filter(|(_, d)| *d > tol)
            .fold(None, |acc: Option<(usize, f64)>, cur| match acc {
                Some(a) if a.1 >= cur.1 => Some(a),
                _ => Some(cur),
            });
        let Some((t, _)) = next else {
            break;
        };
        passive.push(t);
        // The inner loop drops at least one member a pass, so it ends within the passive set's
        // size.
        for _ in 0..=TERMS {
            let s = sub_solve(g, &passive, Some(b))?;
            if s.iter().all(|v| *v > 0.0) {
                for (&k, v) in passive.iter().zip(s.iter()) {
                    if let Some(slot) = w.get_mut(k) {
                        *slot = *v;
                    }
                }
                break;
            }
            let mut alpha = f64::INFINITY;
            for (&k, v) in passive.iter().zip(s.iter()) {
                let wk = w.get(k).copied().unwrap_or(0.0);
                if *v <= 0.0 && wk - v > 0.0 {
                    alpha = alpha.min(wk / (wk - v));
                }
            }
            if !alpha.is_finite() {
                alpha = 0.0;
            }
            for (&k, v) in passive.iter().zip(s.iter()) {
                if let Some(slot) = w.get_mut(k) {
                    *slot += alpha * (v - *slot);
                }
            }
            passive.retain(|k| w.get(*k).copied().unwrap_or(0.0) > 0.0);
            for k in 0..TERMS {
                if !passive.contains(&k) {
                    if let Some(slot) = w.get_mut(k) {
                        *slot = 0.0;
                    }
                }
            }
            if passive.is_empty() {
                break;
            }
        }
    }
    // The residual variance per degree of freedom, then each weight's standard error from the
    // inverse of the normal equations on the terms the fit kept (a weight held at zero keeps no
    // error of its own: it is determined at its bound).
    let wgw: f64 = g
        .iter()
        .zip(w.iter())
        .map(|(row, wi)| wi * row.iter().zip(w.iter()).map(|(a, wj)| a * wj).sum::<f64>())
        .sum();
    let wb: f64 = w.iter().zip(b.iter()).map(|(a, c)| a * c).sum();
    let rss = (normal.yty - 2.0 * wb + wgw).max(0.0);
    let dof = normal.samples.saturating_sub(passive.len() as u64);
    // With no degree of freedom left the spread is unknown: the error is unbounded.
    let variance = if dof > 0 {
        rss / dof as f64
    } else {
        f64::INFINITY
    };
    let mut errors = [0.0; TERMS];
    for &k in &passive {
        let mut order: Vec<usize> = passive.iter().copied().filter(|&p| p != k).collect();
        order.push(k);
        if let Some(col) = sub_solve(g, &order, None) {
            let inv_kk = col.last().copied().unwrap_or(0.0).max(0.0);
            if let Some(e) = errors.get_mut(k) {
                *e = if inv_kk > 0.0 {
                    (variance * inv_kk).sqrt()
                } else {
                    f64::INFINITY
                };
            }
        }
    }
    // A weight the fit holds at zero is the constraint, not a measurement: the samples can't tell
    // its cost from nothing, so it is left undetermined and keeps its learned value.
    let mut weights = [None; TERMS];
    for &k in &passive {
        if let (Some(slot), Some(v)) = (weights.get_mut(k), w.get(k)) {
            *slot = Some(*v);
        }
    }
    Some(Solution { weights, errors })
}

/// How often statements are timed: as often as what timing one adds (its clock reads and the fit's
/// update) stays within the spread its measurement has anyway,
/// `rate = min(1, spread * statement time / timing's time)`; every statement until the fit has a
/// spread. Deterministic: a statement is timed each time the
/// rate's running credit reaches one.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sampler {
    /// Nanoseconds one clock read takes, measured at start (`clock_read_nanos`).
    clock_read: f64,
    /// The sampled statements' time and what timing them added, summed.
    nanos: f64,
    overhead: f64,
    credit: f64,
}

impl Sampler {
    pub(crate) fn new(clock_read: f64) -> Self {
        Self {
            clock_read,
            nanos: 0.0,
            overhead: 0.0,
            credit: 0.0,
        }
    }

    /// The share of statements to time, given the fit's `spread`.
    pub(crate) fn rate(&self, spread: Option<f64>) -> f64 {
        let Some(spread) = spread else {
            return 1.0;
        };
        if !(self.overhead > 0.0 && self.nanos > 0.0) {
            return 1.0;
        }
        (spread * self.nanos / self.overhead).clamp(0.0, 1.0)
    }

    /// Whether to time the next statement.
    pub(crate) fn take(&mut self, spread: Option<f64>) -> bool {
        self.credit += self.rate(spread);
        if self.credit >= 1.0 {
            self.credit -= 1.0;
            true
        } else {
            false
        }
    }

    /// Records a timed statement: `nanos` it ran, `overhead` nanoseconds its timing added.
    pub(crate) fn timed(&mut self, nanos: f64, overhead: f64) {
        if nanos.is_finite() && nanos > 0.0 && overhead.is_finite() && overhead > 0.0 {
            self.nanos += nanos;
            self.overhead += overhead;
        }
    }

    /// Nanoseconds a clock read takes.
    pub(crate) fn clock_read(&self) -> f64 {
        self.clock_read
    }
}

/// A database's fit and its sampler, shared by its connections, and the switch the planner reads
/// them behind (a prototype, off by default until the coordinator's JOB measurement of AC-PL.7.1
/// says otherwise). Off, nothing is sampled and the planner reads the learned weights alone.
#[derive(Debug)]
pub(crate) struct DatabaseFit {
    on: AtomicBool,
    state: Mutex<(CostFit, Option<Sampler>)>,
}

impl DatabaseFit {
    /// A fit with nothing sampled, its switch as `AMPHORA_FITTED_COSTS` says (`1` on; anything
    /// else, or unset, off), so the planner suites run with it either way.
    pub(crate) fn new() -> Self {
        let on = std::env::var("AMPHORA_FITTED_COSTS").is_ok_and(|v| v == "1");
        Self {
            on: AtomicBool::new(on),
            state: Mutex::new((CostFit::new(), None)),
        }
    }

    pub(crate) fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Release);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on.load(Ordering::Acquire)
    }

    /// The weights the planner reads: None with the switch off or nothing fitted.
    pub(crate) fn weights(&self) -> Option<FittedWeights> {
        if !self.is_on() {
            return None;
        }
        self.state.lock().0.weights()
    }

    /// Whether to time the statement starting now; never with the switch off. The clock read is
    /// measured at the first ask.
    pub(crate) fn take(&self) -> bool {
        if !self.is_on() {
            return false;
        }
        let mut state = self.state.lock();
        let spread = state.0.spread();
        state
            .1
            .get_or_insert_with(|| Sampler::new(clock_read_nanos()))
            .take(spread)
    }

    /// Records a timed statement's counted work, the nanoseconds its steps ran, and how many
    /// clock reads timing them made: the fit learns from it, and the sampler from what timing it
    /// cost (the reads, and this update's own time).
    pub(crate) fn record(&self, counts: &Counts, nanos: f64, clock_reads: f64) {
        let start = Instant::now();
        let mut state = self.state.lock();
        let clock_read = state.1.as_ref().map_or(0.0, Sampler::clock_read);
        // The steps' time holds one clock read a step's timing window; it is the timer's, not the
        // statement's.
        let work = (nanos - clock_reads * clock_read / 2.0).max(0.0);
        state.0.observe(counts, work);
        let update = start.elapsed().as_nanos() as f64;
        if let Some(sampler) = state.1.as_mut() {
            sampler.timed(work, clock_reads * clock_read + update);
        }
    }
}

/// Measures a clock read where the engine runs: reads timed back to back, doubling in number until
/// the per-read time resolves to the clock's step (two doublings agree within the step over the
/// reads), or the reads number calibration's repetition bound, one per key of a page
/// (`learned_costs::calibrate`).
pub(crate) fn clock_read_nanos() -> f64 {
    let most =
        crate::storage::buffer_pool::BufferPool::DEFAULT_PAGE_SIZE / std::mem::size_of::<u64>();
    let origin = Instant::now();
    let mut reads: usize = 1;
    let mut last: Option<f64> = None;
    loop {
        let start = origin.elapsed().as_nanos() as f64;
        let mut end = start;
        for _ in 0..reads {
            end = std::hint::black_box(origin.elapsed().as_nanos() as f64);
        }
        let per = (end - start) / reads as f64;
        let step = 1.0 / reads as f64;
        if let Some(prev) = last {
            if per > 0.0 && (per - prev).abs() <= step {
                return per;
            }
        }
        last = Some(per);
        match reads.checked_mul(2) {
            Some(next) if next <= most => reads = next,
            _ => return per.max(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64-driven uniform in [0, 1): a fixed sequence, so the tests are deterministic.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(1);
            (super::super::learned_costs::mix(self.0) >> 11) as f64 / (1u64 << 53) as f64
        }
        /// A normal deviate by Box and Muller.
        fn normal(&mut self) -> f64 {
            let u = self.next().max(f64::MIN_POSITIVE);
            let v = self.next();
            (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
        }
    }

    fn sample(rng: &mut Rng, truth: &[f64; TERMS], noise: f64) -> (Counts, f64) {
        let mut counts = Counts::default();
        for (term, _) in Term::ALL.iter().zip(truth.iter()) {
            // Statements of very different sizes and mixes: each term from none to 10^4 units.
            let units = if rng.next() < 0.3 {
                0.0
            } else {
                (rng.next() * 4.0).exp2().powi(2) * 40.0
            };
            counts.add(*term, units);
        }
        let exact: f64 = Term::ALL
            .iter()
            .zip(truth.iter())
            .map(|(t, w)| counts.get(*t) * w)
            .sum();
        (counts, exact * (1.0 + noise * rng.normal()).max(0.05))
    }

    /// Do: feed samples drawn from known weights with 5% multiplicative normal noise. Expect: each
    /// fitted weight within four of its standard errors of the truth, and within the noise's share
    /// of it (T-PL.7.2).
    #[test]
    fn t_pl_7_2_fit_recovers_known_weights_within_noise() {
        let truth = [3.0, 40.0, 25.0, 18.0, 6.0, 900.0];
        let noise = 0.05;
        let mut rng = Rng(7);
        let mut fit = CostFit::new();
        for _ in 0..4000 {
            let (c, t) = sample(&mut rng, &truth, noise);
            fit.observe(&c, t);
        }
        let w = fit.weights().expect("fitted");
        let sol = fit.fitted.expect("fitted");
        for (k, term) in Term::ALL.iter().enumerate() {
            let got = w.nanos(*term).expect("every term sampled");
            let err = sol.errors[k];
            assert!(
                (got - truth[k]).abs() <= 4.0 * err,
                "{term:?}: got {got}, truth {}, se {err}",
                truth[k]
            );
            assert!(
                (got - truth[k]).abs() <= noise * truth[k],
                "{term:?}: got {got}, truth {}",
                truth[k]
            );
        }
    }

    /// Do: feed exact samples where one in ten is ten times too slow (a stall). Expect: the fit
    /// still lands within 5% of every weight (T-PL.7.2: Huber's weights hold the outliers off).
    #[test]
    fn t_pl_7_2_fit_holds_off_outliers() {
        let truth = [2.0, 30.0, 20.0, 10.0, 5.0, 500.0];
        let mut rng = Rng(11);
        let mut fit = CostFit::new();
        for i in 0..4000 {
            let (c, t) = sample(&mut rng, &truth, 0.01);
            fit.observe(&c, if i % 10 == 9 { t * 10.0 } else { t });
        }
        let w = fit.weights().expect("fitted");
        for (k, term) in Term::ALL.iter().enumerate() {
            let got = w.nanos(*term).expect("sampled");
            assert!(
                (got - truth[k]).abs() <= 0.05 * truth[k],
                "{term:?}: got {got}, truth {}",
                truth[k]
            );
        }
    }

    /// Do: feed samples that never count hash work. Expect: those terms have no fitted weight, so
    /// the planner keeps their learned values; the others are fitted (T-PL.7.3: a weight with no
    /// samples keeps the learned calibration value).
    #[test]
    fn t_pl_7_3_unsampled_terms_keep_their_learned_weight() {
        let truth = [2.0, 30.0, 0.0, 0.0, 5.0, 500.0];
        let mut rng = Rng(3);
        let mut fit = CostFit::new();
        for _ in 0..500 {
            let (mut c, _) = sample(&mut rng, &truth, 0.0);
            c.0[Term::HashBuild.slot()] = 0.0;
            c.0[Term::HashProbe.slot()] = 0.0;
            let t: f64 = Term::ALL
                .iter()
                .zip(truth.iter())
                .map(|(t, w)| c.get(*t) * w)
                .sum();
            fit.observe(&c, t);
        }
        let w = fit.weights().expect("fitted");
        assert_eq!(w.nanos(Term::HashBuild), None);
        assert_eq!(w.nanos(Term::HashProbe), None);
        assert_eq!(w.in_pages(Term::HashBuild, 0.123, 0.01), 0.123);
        let seek = w.in_pages(Term::Seek, 0.5, 0.01);
        assert!((seek - 30.0 / 500.0).abs() < 1e-6, "{seek}");
    }

    /// Do: two hundred thousand samples. Expect: the fit's size never changes and it holds no heap memory
    /// (its state is fixed-size arrays), and the epoch length grows past the minimum while the
    /// weights hold still (T-PL.7.4: memory is bounded; the window learns its length).
    #[test]
    fn t_pl_7_4_memory_is_bounded() {
        let truth = [2.0, 30.0, 20.0, 10.0, 5.0, 500.0];
        let mut rng = Rng(5);
        let mut fit = CostFit::new();
        let before = std::mem::size_of_val(&fit);
        for _ in 0..200_000 {
            let (c, t) = sample(&mut rng, &truth, 0.02);
            fit.observe(&c, t);
        }
        assert_eq!(std::mem::size_of_val(&fit), before);
        assert!(fit.epoch_len() > TERMS as u64, "{}", fit.epoch_len());
    }

    /// Do: fit stationary samples, then move every weight to twice its value. Expect: the fit
    /// follows to the new weights (T-PL.7.3: the window drops the old epoch once the fits disagree).
    #[test]
    fn t_pl_7_3_fit_follows_moved_conditions() {
        let before = [2.0, 30.0, 20.0, 10.0, 5.0, 500.0];
        let after = before.map(|w| w * 2.0);
        let mut rng = Rng(9);
        let mut fit = CostFit::new();
        for _ in 0..20_000 {
            let (c, t) = sample(&mut rng, &before, 0.02);
            fit.observe(&c, t);
        }
        for _ in 0..200_000 {
            let (c, t) = sample(&mut rng, &after, 0.02);
            fit.observe(&c, t);
        }
        let w = fit.weights().expect("fitted");
        for (k, term) in Term::ALL.iter().enumerate() {
            let got = w.nanos(*term).expect("sampled");
            assert!(
                (got - after[k]).abs() <= 0.05 * after[k],
                "{term:?}: got {got}, after {}",
                after[k]
            );
        }
    }

    /// Do: ask the sampler with no spread yet, then with statements a hundred times the timing's
    /// cost and a 1% spread, then with statements as cheap as their timing. Expect: every
    /// statement, then all of them (1% of 100 is 1), then one in a hundred (T-PL.7.5).
    #[test]
    fn t_pl_7_5_sample_rate_follows_overhead_and_spread() {
        let mut s = Sampler::new(20.0);
        assert_eq!(s.rate(None), 1.0);
        s.timed(4000.0, 40.0);
        assert!((s.rate(Some(0.01)) - 1.0).abs() < 1e-12);
        let mut cheap = Sampler::new(20.0);
        cheap.timed(40.0, 40.0);
        assert!((cheap.rate(Some(0.01)) - 0.01).abs() < 1e-12);
        let taken = (0..1000).filter(|_| cheap.take(Some(0.01))).count();
        assert_eq!(taken, 10);
    }

    /// Do: open a database in memory, turn its switch on, and run scans, key lookups and sorts.
    /// Expect: no weights before (switch off), then rows and seeks fitted from the statements' own
    /// times, and no weight for the hash tables none of them built nor for page reads none of
    /// them made, which keep their learned values (T-PL.7.5).
    #[test]
    fn t_pl_7_5_statements_run_with_the_switch_on_fit_weights() {
        let io: crate::sync::Arc<dyn crate::io::IO> = crate::sync::Arc::new(crate::MemoryIO::new());
        let db =
            crate::Database::open_file(io, ":memory:", crate::sync::Arc::new(crate::SqliteDialect))
                .unwrap();
        let conn = db.connect(crate::ConnectOpts::new()).unwrap();
        // Off whatever `AMPHORA_FITTED_COSTS` says, so the suite runs either way.
        conn.set_fitted_costs(false);
        conn.execute("CREATE TABLE t(a INTEGER PRIMARY KEY, b INTEGER)")
            .unwrap();
        let rows: Vec<String> = (1..=400).map(|i| format!("({i}, {})", i % 7)).collect();
        conn.execute(format!("INSERT INTO t VALUES {}", rows.join(",")))
            .unwrap();
        conn.query("SELECT count(*) FROM t WHERE b = 3")
            .unwrap()
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(db.cost_fit.weights(), None);
        conn.set_fitted_costs(true);
        for i in 0..200 {
            let sql = match i % 3 {
                0 => format!("SELECT count(*) FROM t WHERE b = {}", i % 7),
                1 => format!("SELECT b FROM t WHERE a = {}", 1 + i % 400),
                _ => format!("SELECT a FROM t WHERE a > {} ORDER BY b", i % 400),
            };
            conn.query(sql)
                .unwrap()
                .unwrap()
                .run_collect_rows()
                .unwrap();
        }
        let w = db.cost_fit.weights().expect("fitted");
        assert!(w.nanos(Term::Row).is_some(), "{w:?}");
        assert!(w.nanos(Term::Seek).is_some(), "{w:?}");
        assert_eq!(w.nanos(Term::HashBuild), None, "{w:?}");
        assert_eq!(w.nanos(Term::HashProbe), None, "{w:?}");
        assert_eq!(w.nanos(Term::PageRead), None, "{w:?}");
        conn.set_fitted_costs(false);
        assert_eq!(db.cost_fit.weights(), None);
    }

    /// Do: give the planner's parameters fitted weights with a page read, then with no page read
    /// but a row visit, then none. Expect: each fitted weight in page reads (its ratio to the page
    /// read, or to the row visit times the row's weight in pages); each unfitted one as it stood;
    /// none, the parameters unchanged (T-PL.7.3: the planner reads the fit through one hook).
    #[test]
    fn t_pl_7_3_the_planner_reads_fitted_weights_in_page_reads() {
        let base = super::super::cost_params::CostModelParams::new();
        let mut nanos = [None; TERMS];
        nanos[Term::Row.slot()] = Some(5.0);
        nanos[Term::Seek.slot()] = Some(50.0);
        nanos[Term::PageRead.slot()] = Some(1000.0);
        let with_page = base.with_fitted(Some(FittedWeights { nanos }));
        assert!((with_page.cpu_cost_per_row - 0.005).abs() < 1e-12);
        assert!((with_page.cpu_cost_per_seek - 0.05).abs() < 1e-12);
        assert_eq!(with_page.sort_cpu_per_row, base.sort_cpu_per_row);
        assert_eq!(with_page.hash_insert_cost, base.hash_insert_cost);
        nanos[Term::PageRead.slot()] = None;
        let without_page = base.with_fitted(Some(FittedWeights { nanos }));
        assert_eq!(without_page.cpu_cost_per_row, base.cpu_cost_per_row);
        assert!((without_page.cpu_cost_per_seek - base.cpu_cost_per_row * 10.0).abs() < 1e-12);
        let none = base.with_fitted(None);
        assert_eq!(none.cpu_cost_per_seek, base.cpu_cost_per_seek);
        assert_eq!(none.cpu_cost_per_row, base.cpu_cost_per_row);
    }

    /// Do: measure a clock read. Expect: a finite, non-negative time (T-PL.7.5).
    #[test]
    fn t_pl_7_5_clock_read_is_measured() {
        let c = clock_read_nanos();
        assert!(c.is_finite() && c >= 0.0, "{c}");
    }

    /// Do: a fit that determines three weights, one of them within its standard error times the
    /// 95% Bonferroni quantile of zero, one with an unbounded error (no degree of freedom left).
    /// Expect: the planner is given only the weight told from nothing; the other two keep the
    /// learned value (T-PL.7.6).
    #[test]
    fn t_pl_7_6_weights_not_told_from_nothing_keep_the_learned_value() {
        let mut fit = Solution::default();
        let (row, seek, page) = (Term::Row.slot(), Term::Seek.slot(), Term::PageRead.slot());
        fit.weights[row] = Some(10.0);
        fit.errors[row] = 1.0;
        fit.weights[seek] = Some(2.0);
        fit.errors[seek] = 1.0;
        fit.weights[page] = Some(500.0);
        fit.errors[page] = f64::INFINITY;
        let told = fit.told_from_nothing();
        assert_eq!(told.nanos(Term::Row), Some(10.0));
        assert_eq!(
            told.nanos(Term::Seek),
            None,
            "2 is within 2.39 errors of zero"
        );
        assert_eq!(
            told.nanos(Term::PageRead),
            None,
            "an unbounded error measures nothing"
        );
        assert_eq!(told.in_pages(Term::Seek, 0.25, 0.01), 0.25);
    }
}
