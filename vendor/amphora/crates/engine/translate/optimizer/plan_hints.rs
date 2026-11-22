//! Amphora (G6 step 3, AC-G6.3.1, T-G6.3.1 to T-G6.3.3; `docs/design/planner.md`, the learned
//! plan hints decision): Bao's learned hints (Marcus et al., "Bao: Making Learned Query
//! Optimization Practical", SIGMOD 2021) over Amphora's planner, a prototype behind its own switch
//! (a connection plans with hints only when the caller applies one, `HintSet::apply`).
//!
//! Four parts, each bounded in memory:
//! - **Hint sets** (`HintSet`): which operators the planner may choose, as on/off masks it obeys
//!   through the connection's switches (hash joins, nested loops, automatic indexes, sorted
//!   fetches). The cost model still chooses among what a mask allows.
//! - **Plan features** (`PlanFeatures`): a fixed-length vector read from a prepared plan's
//!   `EXPLAIN QUERY PLAN` nodes (operator counts and the planner's own row and cost estimates),
//!   Bao's per-node operator and cardinality features summed over the plan.
//! - **The model** (`HintModel`): Bayesian linear regression of a run's log time on its plan's
//!   features, kept as sufficient statistics (a fixed `FEATURES` x `FEATURES` matrix, whatever the
//!   number of runs). Its prior and noise precisions are not picked: they are re-estimated from
//!   the measured runs by the evidence approximation (MacKay 1992; Bishop, PRML §3.5.2), one
//!   fixed-point step a run, starting from the unit-information prior (Kass and Wasserman 1995).
//!   Thompson sampling draws weights from the posterior to choose a hint set (Bao §3.2; Agrawal
//!   and Goyal, "Thompson Sampling for Contextual Bandits with Linear Payoffs", ICML 2013).
//! - **The guard** (`HintGuard`): a hint set is used for a query class only after its held-out
//!   runs beat the unhinted plan's, and it is dropped when they show it slower. Held-out runs never
//!   train the model. It keeps at most the number of classes its caller gives it, evicting the
//!   least recently used class past it.
//!
//! This is a pure core: the caller runs the statements and measures their times (the harness,
//! `bench/plan_hints`, is the shell), and passes the seed of the sampler.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use crate::translate::eqp::{EqpDetail, EqpGroupMethod, EqpRowSource, EqpSearchKind};
use crate::translate::plan::TablePlanEstimate;
use crate::vdbe::insn::Insn;
use crate::{Connection, JoinMethods, LimboError, Result, Statement};

/// Which operators the planner may choose for a statement: Bao's hint set, a mask of on/off bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HintSet(u8);

impl HintSet {
    const HASH_JOIN: u8 = 1;
    const NESTED_LOOP: u8 = 2;
    const AUTOMATIC_INDEX: u8 = 4;
    const SORTED_FETCH: u8 = 8;

    /// The planner as it plans without hints: every join method by cost, automatic indexes on,
    /// sorted fetches off (their own prototype's default, `Connection::set_sorted_fetches`).
    pub const UNHINTED: HintSet =
        HintSet(Self::HASH_JOIN | Self::NESTED_LOOP | Self::AUTOMATIC_INDEX);

    /// Every mask the planner can obey, the unhinted one first. A mask needs a join method, and
    /// nested loops alone (`JoinMethods::NestedLoopsOnly`) build no automatic index, so the masks
    /// that differ only there are one.
    pub const ARMS: [HintSet; ARMS] = [
        Self::UNHINTED,
        HintSet(Self::HASH_JOIN | Self::NESTED_LOOP | Self::AUTOMATIC_INDEX | Self::SORTED_FETCH),
        HintSet(Self::HASH_JOIN | Self::NESTED_LOOP),
        HintSet(Self::HASH_JOIN | Self::NESTED_LOOP | Self::SORTED_FETCH),
        HintSet(Self::HASH_JOIN | Self::AUTOMATIC_INDEX),
        HintSet(Self::HASH_JOIN | Self::AUTOMATIC_INDEX | Self::SORTED_FETCH),
        HintSet(Self::HASH_JOIN),
        HintSet(Self::HASH_JOIN | Self::SORTED_FETCH),
        HintSet(Self::NESTED_LOOP),
        HintSet(Self::NESTED_LOOP | Self::SORTED_FETCH),
    ];

    /// The arm's place in `ARMS`, None for a mask the planner can't obey.
    pub fn arm(self) -> Option<usize> {
        Self::ARMS.iter().position(|arm| *arm == self)
    }

    /// The join methods the mask allows, as the planner's switch has them.
    fn join_methods(self) -> JoinMethods {
        match (
            self.0 & Self::HASH_JOIN != 0,
            self.0 & Self::NESTED_LOOP != 0,
        ) {
            (true, false) => JoinMethods::HashJoinsOnly,
            (false, true) => JoinMethods::NestedLoopsOnly,
            _ => JoinMethods::ByCost,
        }
    }

    /// Sets the connection's switches so its newly prepared statements obey this mask.
    pub fn apply(self, conn: &Connection) {
        conn.set_join_methods(self.join_methods());
        conn.set_automatic_indexes(self.0 & Self::AUTOMATIC_INDEX != 0);
        conn.set_sorted_fetches(self.0 & Self::SORTED_FETCH != 0);
    }

    /// The operators the mask allows, for records: `hash+loop+autoindex` and the like.
    pub fn name(self) -> String {
        let mut parts = Vec::new();
        for (bit, name) in [
            (Self::HASH_JOIN, "hash"),
            (Self::NESTED_LOOP, "loop"),
            (Self::AUTOMATIC_INDEX, "autoindex"),
            (Self::SORTED_FETCH, "sortedfetch"),
        ] {
            if self.0 & bit != 0 {
                parts.push(name);
            }
        }
        parts.join("+")
    }
}

/// The number of masks in `HintSet::ARMS` (a format value: the masks the four switches allow).
pub const ARMS: usize = 10;

/// The length of a plan's feature vector (a format value: the features `PlanFeatures` lists).
pub const FEATURES: usize = 16;

/// A plan's features: a constant term, counts of each operator, and the planner's estimates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanFeatures(pub [f64; FEATURES]);

impl PlanFeatures {
    // Each feature's place: a format, listed once.
    const BIAS: usize = 0;
    const TABLE_SCAN: usize = 1;
    const INDEX_SCAN: usize = 2;
    const AUTOMATIC_INDEX: usize = 3;
    const ROWID_SEARCH: usize = 4;
    const INDEX_SEARCH: usize = 5;
    const MULTI_INDEX: usize = 6;
    const HASH_JOIN: usize = 7;
    const HASH_BUILD: usize = 8;
    const SORT: usize = 9;
    const SORTER_GROUP: usize = 10;
    const HASH_GROUP: usize = 11;
    const SUBQUERY: usize = 12;
    const LOG_ROWS: usize = 13;
    const LOG_ACCESS_COST: usize = 14;
    const LOG_TOTAL_COST: usize = 15;

    fn add(&mut self, at: usize, value: f64) {
        if let Some(slot) = self.0.get_mut(at) {
            *slot += value;
        }
    }

    fn estimate(&mut self, estimate: Option<&TablePlanEstimate>) {
        let Some(estimate) = estimate else {
            return;
        };
        self.add(Self::LOG_ROWS, log_size(estimate.output_rows));
        self.add(Self::LOG_ACCESS_COST, log_size(estimate.access_cost));
        let total = log_size(estimate.total_cost);
        if let Some(slot) = self.0.get_mut(Self::LOG_TOTAL_COST) {
            *slot = slot.max(total);
        }
    }
}

/// log2(1 + x) of a size the planner estimated, 0 for one that is not a finite positive number.
fn log_size(x: f64) -> f64 {
    if x.is_finite() && x > 0.0 {
        x.ln_1p() / std::f64::consts::LN_2
    } else {
        0.0
    }
}

/// The features of a statement prepared as `EXPLAIN QUERY PLAN <query>`, read from its plan's
/// nodes. Refused for a statement with no plan nodes (one not prepared that way).
pub fn plan_features(stmt: &Statement) -> Result<PlanFeatures> {
    let mut features = PlanFeatures([0.0; FEATURES]);
    features.add(PlanFeatures::BIAS, 1.0);
    let mut nodes = 0usize;
    for (insn, _) in &stmt.program.insns {
        let Insn::Explain { detail, .. } = insn else {
            continue;
        };
        nodes = nodes.saturating_add(1);
        match detail.as_ref() {
            EqpDetail::Scan {
                index,
                source,
                estimate,
                ..
            } => {
                let at = match (index, source) {
                    (Some(index), _) if index.ephemeral => PlanFeatures::AUTOMATIC_INDEX,
                    (Some(index), _) if !index.primary_key => PlanFeatures::INDEX_SCAN,
                    (_, EqpRowSource::Subquery | EqpRowSource::RecursiveCteInput) => {
                        PlanFeatures::SUBQUERY
                    }
                    _ => PlanFeatures::TABLE_SCAN,
                };
                features.add(at, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::Search {
                kind,
                index,
                estimate,
                ..
            } => {
                let at = match (index, kind) {
                    (Some(index), _) if index.ephemeral => PlanFeatures::AUTOMATIC_INDEX,
                    (None, _) | (_, EqpSearchKind::RowidEq) => PlanFeatures::ROWID_SEARCH,
                    _ => PlanFeatures::INDEX_SEARCH,
                };
                features.add(at, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::MultiIndex { estimate, .. } => {
                features.add(PlanFeatures::MULTI_INDEX, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::IndexMethod { estimate, .. } => {
                features.add(PlanFeatures::INDEX_SEARCH, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::HashJoin { estimate, .. } => {
                features.add(PlanFeatures::HASH_JOIN, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::HashBuild { estimate, .. } => {
                features.add(PlanFeatures::HASH_BUILD, 1.0);
                features.estimate(estimate.as_ref());
            }
            EqpDetail::OrderBy { .. } => features.add(PlanFeatures::SORT, 1.0),
            EqpDetail::GroupBy {
                method: EqpGroupMethod::Sorter,
            } => features.add(PlanFeatures::SORTER_GROUP, 1.0),
            EqpDetail::GroupBy {
                method: EqpGroupMethod::HashTable,
            }
            | EqpDetail::Distinct
            | EqpDetail::DistinctAggregate { .. } => features.add(PlanFeatures::HASH_GROUP, 1.0),
            EqpDetail::ListSubquery { .. } | EqpDetail::ScalarSubquery { .. } => {
                features.add(PlanFeatures::SUBQUERY, 1.0)
            }
            EqpDetail::ConstantRow
            | EqpDetail::RightJoin { .. }
            | EqpDetail::Compound
            | EqpDetail::CompoundArm { .. }
            | EqpDetail::RecursiveSetup
            | EqpDetail::RecursiveStep => {}
        }
    }
    if nodes == 0 {
        return Err(LimboError::InvalidArgument(
            "plan features are read from a statement prepared as EXPLAIN QUERY PLAN; this one has no plan nodes"
                .to_string(),
        ));
    }
    Ok(features)
}

/// Each hint set's plan for `sql` on this connection: prepared (not run) under each mask in
/// `HintSet::ARMS`, its features read. The connection is left unhinted.
pub fn candidate_plans(conn: &Arc<Connection>, sql: &str) -> Result<Vec<(HintSet, PlanFeatures)>> {
    let mut plans = Vec::with_capacity(ARMS);
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut outcome = Ok(());
    for arm in HintSet::ARMS {
        arm.apply(conn);
        match conn.prepare(&explain).and_then(|stmt| plan_features(&stmt)) {
            Ok(features) => plans.push((arm, features)),
            Err(error) => {
                outcome = Err(error);
                break;
            }
        }
    }
    HintSet::UNHINTED.apply(conn);
    outcome.map(|()| plans)
}

/// A square matrix of the features' size.
type Matrix = [[f64; FEATURES]; FEATURES];

/// The lower Cholesky factor of a symmetric positive-definite matrix, None when it is not one.
fn cholesky(a: &Matrix) -> Option<Matrix> {
    let mut l = [[0.0; FEATURES]; FEATURES];
    for i in 0..FEATURES {
        for j in 0..=i {
            let mut sum = *a.get(i)?.get(j)?;
            for k in 0..j {
                sum -= l.get(i)?.get(k)? * l.get(j)?.get(k)?;
            }
            let value = if i == j {
                if !(sum.is_finite() && sum > 0.0) {
                    return None;
                }
                sum.sqrt()
            } else {
                sum / l.get(j)?.get(j)?
            };
            *l.get_mut(i)?.get_mut(j)? = value;
        }
    }
    Some(l)
}

/// The inverse of a symmetric positive-definite matrix from its Cholesky factor.
fn inverse_from_cholesky(l: &Matrix) -> Option<Matrix> {
    let mut inverse = [[0.0; FEATURES]; FEATURES];
    for column in 0..FEATURES {
        // Solve L y = e_column, then L^T x = y.
        let mut y = [0.0; FEATURES];
        for i in 0..FEATURES {
            let mut sum = if i == column { 1.0 } else { 0.0 };
            for k in 0..i {
                sum -= l.get(i)?.get(k)? * y.get(k)?;
            }
            *y.get_mut(i)? = sum / l.get(i)?.get(i)?;
        }
        let mut x = [0.0; FEATURES];
        for i in (0..FEATURES).rev() {
            let mut sum = *y.get(i)?;
            for k in (i..FEATURES).skip(1) {
                sum -= l.get(k)?.get(i)? * x.get(k)?;
            }
            *x.get_mut(i)? = sum / l.get(i)?.get(i)?;
        }
        for (row, value) in x.iter().enumerate() {
            *inverse.get_mut(row)?.get_mut(column)? = *value;
        }
    }
    Some(inverse)
}

fn dot(a: &[f64; FEATURES], b: &[f64; FEATURES]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn times(m: &Matrix, v: &[f64; FEATURES]) -> [f64; FEATURES] {
    let mut out = [0.0; FEATURES];
    for (slot, row) in out.iter_mut().zip(m) {
        *slot = dot(row, v);
    }
    out
}

/// The posterior the model samples from: its mean weights and the Cholesky factor of their
/// covariance.
#[derive(Debug, Clone, Copy)]
struct Posterior {
    mean: [f64; FEATURES],
    spread: Matrix,
    /// The runs' mean features and mean log time, the point the weights pivot on.
    mean_x: [f64; FEATURES],
    mean_y: f64,
}

/// The model's sufficient statistics centered on the runs' means (`HintModel::centered`).
struct Centered {
    gram: Matrix,
    xty: [f64; FEATURES],
    yty: f64,
    mean_x: [f64; FEATURES],
    mean_y: f64,
}

/// Bayesian linear regression of a run's log time on its plan's features, in constant memory: the
/// sufficient statistics of every run it was given, and the precisions the evidence estimates.
#[derive(Debug, Clone)]
pub struct HintModel {
    xtx: Matrix,
    xty: [f64; FEATURES],
    yty: f64,
    runs: u64,
    /// The prior's precision of each weight, and the precision of a run's log time about the fit,
    /// both estimated from the runs (None before two runs).
    precisions: Option<(f64, f64)>,
    posterior: Option<Posterior>,
}

impl Default for HintModel {
    fn default() -> Self {
        Self::new()
    }
}

impl HintModel {
    pub fn new() -> Self {
        Self {
            xtx: [[0.0; FEATURES]; FEATURES],
            xty: [0.0; FEATURES],
            yty: 0.0,
            runs: 0,
            precisions: None,
            posterior: None,
        }
    }

    /// The runs the model was trained on.
    pub fn runs(&self) -> u64 {
        self.runs
    }

    /// Trains on one measured run: the plan that ran and the nanoseconds it took. A time that is
    /// not a finite positive number is refused, not dropped.
    pub fn observe(&mut self, features: &PlanFeatures, nanos: f64) -> Result<()> {
        if !(nanos.is_finite() && nanos > 0.0) {
            return Err(LimboError::InvalidArgument(format!(
                "a plan hint's training run needs a finite positive time, not {nanos} ns"
            )));
        }
        if features.0.iter().any(|x| !x.is_finite()) {
            return Err(LimboError::InvalidArgument(
                "a plan hint's training run has a feature that is not a finite number".to_string(),
            ));
        }
        let y = nanos.ln();
        for (row, xi) in self.xtx.iter_mut().zip(features.0) {
            for (cell, xj) in row.iter_mut().zip(features.0) {
                *cell += xi * xj;
            }
        }
        for (cell, x) in self.xty.iter_mut().zip(features.0) {
            *cell += x * y;
        }
        self.yty += y * y;
        self.runs = self.runs.saturating_add(1);
        self.refit();
        Ok(())
    }

    /// The runs' features and log times centered on their means: the Gram matrix of the centered
    /// features, their products with the centered times, the centered times' sum of squares, and
    /// the means. The constant term (feature 0, 1 on every run) centers to nothing, so its row and
    /// column are zero: the intercept is the times' mean, not a weight the prior shrinks (Bishop,
    /// PRML §3.1.1). Without centering, features every plan shares (a join's row estimates) swamp
    /// the prior's scale and the differences between plans, the signal, are shrunk away.
    fn centered(&self) -> Option<Centered> {
        if self.runs < 2 {
            return None;
        }
        let n = self.runs as f64;
        let sums = self.xtx.first().copied()?;
        let mut mean_x = [0.0; FEATURES];
        for (m, sum) in mean_x.iter_mut().zip(sums) {
            *m = sum / n;
        }
        let mean_y = self.xty.first().copied()? / n;
        let mut gram = [[0.0; FEATURES]; FEATURES];
        let mut xty = [0.0; FEATURES];
        for i in 1..FEATURES {
            let mi = *mean_x.get(i)?;
            *xty.get_mut(i)? = self.xty.get(i)? - n * mi * mean_y;
            for j in 1..FEATURES {
                *gram.get_mut(i)?.get_mut(j)? =
                    self.xtx.get(i)?.get(j)? - n * mi * mean_x.get(j)?;
            }
        }
        let yty = self.yty - n * mean_y * mean_y;
        Some(Centered {
            gram,
            xty,
            yty,
            mean_x,
            mean_y,
        })
    }

    /// One evidence step: the posterior under the current precisions, then the precisions the
    /// evidence approximation gives from it (MacKay 1992), the posterior again under those.
    fn refit(&mut self) {
        let Some(c) = self.centered() else {
            return;
        };
        let n = self.runs as f64;
        if !(c.yty.is_finite() && c.yty > 0.0) {
            return;
        }
        let (alpha, beta) = self.precisions.unwrap_or_else(|| {
            // The unit-information prior (Kass and Wasserman 1995): the noise precision is the
            // inverse of the times' variance, and each weight's prior is worth one average run
            // of its centered feature (the constant term is not a weight: FEATURES - 1 of them).
            let beta = n / c.yty;
            let trace: f64 = (1..FEATURES)
                .filter_map(|i| c.gram.get(i).and_then(|row| row.get(i)))
                .sum();
            (beta * trace / (n * (FEATURES - 1) as f64), beta)
        });
        let Some((posterior, trace_covariance)) = Self::posterior(&c, alpha, beta) else {
            return;
        };
        // MacKay's fixed point: gamma, the weights the data determine, sets both precisions. The
        // constant term's weight is the prior's alone (its centered feature is zero), so it adds
        // nothing to gamma: FEATURES less alpha times the covariance's trace counts it out.
        let gamma = FEATURES as f64 - alpha * trace_covariance;
        let weight_norm = dot(&posterior.mean, &posterior.mean);
        let fit = times(&c.gram, &posterior.mean);
        let residual = c.yty - 2.0 * dot(&posterior.mean, &c.xty) + dot(&posterior.mean, &fit);
        let next = (gamma / weight_norm, (n - gamma) / residual);
        let next_ok = next.0.is_finite() && next.0 > 0.0 && next.1.is_finite() && next.1 > 0.0;
        let (alpha, beta) = if next_ok { next } else { (alpha, beta) };
        self.precisions = Some((alpha, beta));
        self.posterior = Self::posterior(&c, alpha, beta).map(|(p, _)| p);
    }

    /// The weights' posterior under prior precision `alpha` and noise precision `beta`, and the
    /// trace of its covariance.
    fn posterior(c: &Centered, alpha: f64, beta: f64) -> Option<(Posterior, f64)> {
        let mut precision = [[0.0; FEATURES]; FEATURES];
        for (i, (row, data)) in precision.iter_mut().zip(&c.gram).enumerate() {
            for (j, (cell, x)) in row.iter_mut().zip(data).enumerate() {
                *cell = beta * x + if i == j { alpha } else { 0.0 };
            }
        }
        let covariance = inverse_from_cholesky(&cholesky(&precision)?)?;
        let mut mean = times(&covariance, &c.xty);
        for w in &mut mean {
            *w *= beta;
        }
        let trace = (0..FEATURES)
            .filter_map(|i| covariance.get(i).and_then(|row| row.get(i)))
            .sum();
        let spread = cholesky(&covariance)?;
        Some((
            Posterior {
                mean,
                spread,
                mean_x: c.mean_x,
                mean_y: c.mean_y,
            },
            trace,
        ))
    }

    /// The posterior mean's predicted log time (natural log of nanoseconds) of a plan, None before
    /// the model is fit.
    pub fn predict(&self, features: &PlanFeatures) -> Option<f64> {
        self.posterior
            .map(|p| p.mean_y + dot(&p.mean, &features.0) - dot(&p.mean, &p.mean_x))
    }

    /// Weights drawn from the posterior (Thompson sampling), None before the model is fit.
    fn sample(&self, rng: &mut SplitMix64) -> Option<[f64; FEATURES]> {
        let posterior = self.posterior?;
        let mut z = [0.0; FEATURES];
        for slot in &mut z {
            *slot = rng.normal();
        }
        let mut weights = posterior.mean;
        for (w, row) in weights.iter_mut().zip(&posterior.spread) {
            *w += dot(row, &z);
        }
        Some(weights)
    }
}

/// SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number Generators", OOPSLA
/// 2014): its constants are the algorithm's. Seeded by the caller, so a run can be repeated.
#[derive(Debug, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ z.wrapping_shr(31)
    }

    /// A uniform draw in (0, 1]: the top 53 bits, the precision of an f64's mantissa.
    fn unit(&mut self) -> f64 {
        // 2^53, an f64's mantissa precision.
        (self.next().wrapping_shr(11) as f64 + 1.0) / 9_007_199_254_740_992.0
    }

    /// A standard normal draw (Box and Muller 1958).
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit(), self.unit());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Running count, mean and spread of a hint set's held-out log times (Welford 1962).
#[derive(Debug, Clone, Copy, Default)]
struct Spread {
    runs: u64,
    mean: f64,
    m2: f64,
}

impl Spread {
    fn add(&mut self, y: f64) {
        self.runs = self.runs.saturating_add(1);
        let delta = y - self.mean;
        self.mean += delta / self.runs as f64;
        self.m2 += delta * (y - self.mean);
    }

    /// The mean's standard error, None before two runs.
    fn standard_error(&self) -> Option<f64> {
        if self.runs < 2 {
            return None;
        }
        let n = self.runs as f64;
        Some((self.m2 / (n - 1.0) / n).sqrt())
    }
}

/// What the guard holds of a hint set for a class after a held-out run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not yet shown faster or slower than the unhinted plan.
    OnTrial,
    /// Its held-out runs beat the unhinted plan's: the class uses it.
    Admitted,
    /// Its held-out runs were slower than the unhinted plan's: the class never uses it.
    Dropped,
}

#[derive(Debug, Clone)]
struct ClassState {
    arms: [Spread; ARMS],
    admitted: Option<HintSet>,
    /// Each arm's: whether it was dropped.
    dropped: [bool; ARMS],
    last_used: u64,
}

/// The guard over hint sets, per query class (the caller's key: a query's text, a join class).
#[derive(Debug, Clone)]
pub struct HintGuard {
    classes: FxHashMap<u64, ClassState>,
    capacity: usize,
    tick: u64,
}

impl HintGuard {
    /// A guard keeping at most `capacity` classes (the caller's bound: its workload's classes, or
    /// what its memory budget holds), the least recently used evicted past it. Refused at 0.
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(LimboError::InvalidArgument(
                "a plan hint guard needs room for at least one query class".to_string(),
            ));
        }
        Ok(Self {
            classes: FxHashMap::default(),
            capacity,
            tick: 0,
        })
    }

    /// The classes held.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// The bytes the guard's classes take: its map's slots times a class's size.
    pub fn heap_bytes(&self) -> usize {
        self.classes
            .capacity()
            .saturating_mul(std::mem::size_of::<(u64, ClassState)>())
    }

    /// The hint set the class runs with: the admitted one, else the unhinted plan.
    pub fn use_for(&self, class: u64) -> HintSet {
        self.classes
            .get(&class)
            .and_then(|state| state.admitted)
            .unwrap_or(HintSet::UNHINTED)
    }

    /// Whether `hint` was dropped for the class.
    pub fn is_dropped(&self, class: u64, hint: HintSet) -> bool {
        match (self.classes.get(&class), hint.arm()) {
            (Some(state), Some(arm)) => state.dropped.get(arm).copied().unwrap_or(false),
            _ => false,
        }
    }

    /// Records one held-out run of the class under `hint`, and what it now holds of the hint (the
    /// unhinted plan's runs are its baseline, `OnTrial` always). A hint is admitted when its mean
    /// log time and its standard error lie below the unhinted mean less its standard error; it is
    /// dropped when its mean lies above the unhinted mean plus that standard error. Both spreads
    /// are the runs' own, measured.
    pub fn record(&mut self, class: u64, hint: HintSet, nanos: f64) -> Result<Verdict> {
        let Some(arm) = hint.arm() else {
            return Err(LimboError::InvalidArgument(format!(
                "{} is not a hint set the planner can obey",
                hint.name()
            )));
        };
        if !(nanos.is_finite() && nanos > 0.0) {
            return Err(LimboError::InvalidArgument(format!(
                "a plan hint's held-out run needs a finite positive time, not {nanos} ns"
            )));
        }
        self.tick = self.tick.saturating_add(1);
        if !self.classes.contains_key(&class) && self.classes.len() >= self.capacity {
            let oldest = self
                .classes
                .iter()
                .min_by_key(|(_, state)| state.last_used)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                self.classes.remove(&oldest);
            }
        }
        let tick = self.tick;
        let state = self.classes.entry(class).or_insert_with(|| ClassState {
            arms: [Spread::default(); ARMS],
            admitted: None,
            dropped: [false; ARMS],
            last_used: tick,
        });
        state.last_used = tick;
        let Some(spread) = state.arms.get_mut(arm) else {
            return Err(LimboError::InternalError(
                "a hint set's arm is outside the guard's arms".to_string(),
            ));
        };
        spread.add(nanos.ln());
        if hint == HintSet::UNHINTED {
            return Ok(Verdict::OnTrial);
        }
        if state.dropped.get(arm).copied().unwrap_or(false) {
            return Ok(Verdict::Dropped);
        }
        let (Some(base), Some(this)) = (state.arms.first().copied(), state.arms.get(arm).copied())
        else {
            return Ok(Verdict::OnTrial);
        };
        let (Some(base_se), Some(this_se)) = (base.standard_error(), this.standard_error()) else {
            return Ok(if state.admitted == Some(hint) {
                Verdict::Admitted
            } else {
                Verdict::OnTrial
            });
        };
        if this.mean > base.mean + base_se {
            if let Some(dropped) = state.dropped.get_mut(arm) {
                *dropped = true;
            }
            if state.admitted == Some(hint) {
                state.admitted = None;
            }
            return Ok(Verdict::Dropped);
        }
        if this.mean + this_se < base.mean - base_se {
            let better_than_admitted = match state.admitted.and_then(HintSet::arm) {
                Some(current) if current != arm => state
                    .arms
                    .get(current)
                    .is_none_or(|admitted| this.mean < admitted.mean),
                _ => true,
            };
            if better_than_admitted {
                state.admitted = Some(hint);
            }
        }
        Ok(if state.admitted == Some(hint) {
            Verdict::Admitted
        } else {
            Verdict::OnTrial
        })
    }
}

/// Bao's chooser: the model, the guard and the sampler's state.
#[derive(Debug, Clone)]
pub struct HintChooser {
    pub model: HintModel,
    pub guard: HintGuard,
    rng: SplitMix64,
}

impl HintChooser {
    /// A chooser sampling from `seed`, its guard holding at most `classes` query classes.
    pub fn new(seed: u64, classes: usize) -> Result<Self> {
        Ok(Self {
            model: HintModel::new(),
            guard: HintGuard::new(classes)?,
            rng: SplitMix64(seed),
        })
    }

    /// A training run's hint set, by Thompson sampling: the candidate whose plan the sampled
    /// weights predict fastest. The unhinted plan before the model is fit, and wherever a hint
    /// set's plan is the unhinted one's.
    pub fn explore(&mut self, candidates: &[(HintSet, PlanFeatures)]) -> HintSet {
        match self.model.sample(&mut self.rng) {
            Some(weights) => fastest(candidates, |f| dot(&weights, &f.0)),
            None => HintSet::UNHINTED,
        }
    }

    /// The hint set the posterior mean predicts fastest, for a held-out trial against the
    /// unhinted plan.
    pub fn best(&self, candidates: &[(HintSet, PlanFeatures)]) -> HintSet {
        if self.model.posterior.is_none() {
            return HintSet::UNHINTED;
        }
        fastest(candidates, |f| {
            self.model.predict(f).unwrap_or(f64::INFINITY)
        })
    }
}

/// The candidate predicted fastest; a plan equal to the unhinted one's is the unhinted set.
fn fastest(
    candidates: &[(HintSet, PlanFeatures)],
    predict: impl Fn(&PlanFeatures) -> f64,
) -> HintSet {
    let unhinted = candidates
        .iter()
        .find(|(hint, _)| *hint == HintSet::UNHINTED)
        .map(|(_, features)| *features);
    let mut best = (HintSet::UNHINTED, f64::INFINITY);
    for (hint, features) in candidates {
        let time = predict(features);
        if time < best.1 {
            best = (*hint, time);
        }
    }
    match (
        candidates.iter().find(|(hint, _)| *hint == best.0),
        unhinted,
    ) {
        (Some((_, features)), Some(base)) if *features == base => HintSet::UNHINTED,
        _ => best.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(counts: &[(usize, f64)]) -> PlanFeatures {
        let mut f = PlanFeatures([0.0; FEATURES]);
        f.0[0] = 1.0;
        for (at, value) in counts {
            f.0[*at] = *value;
        }
        f
    }

    /// T-G6.3.2: do record held-out runs where a hint set is first faster than the unhinted plan,
    /// then slower; expect it admitted, then dropped, and the class back on the unhinted plan.
    #[test]
    fn t_g6_3_2_the_guard_drops_a_regressing_hint() {
        let mut guard = HintGuard::new(4).unwrap();
        let hint = HintSet::ARMS[6];
        let mut rng = SplitMix64(7);
        let mut jitter = || 1.0 + 0.01 * rng.unit();
        for _ in 0..4 {
            guard
                .record(1, HintSet::UNHINTED, 1000.0 * jitter())
                .unwrap();
            guard.record(1, hint, 500.0 * jitter()).unwrap();
        }
        assert_eq!(guard.use_for(1), hint);
        let mut last = Verdict::Admitted;
        for _ in 0..16 {
            last = guard.record(1, hint, 5000.0 * jitter()).unwrap();
            if last == Verdict::Dropped {
                break;
            }
        }
        assert_eq!(last, Verdict::Dropped);
        assert!(guard.is_dropped(1, hint));
        assert_eq!(guard.use_for(1), HintSet::UNHINTED);
        // Dropped stays dropped: fast runs after it don't bring it back.
        for _ in 0..4 {
            assert_eq!(guard.record(1, hint, 10.0).unwrap(), Verdict::Dropped);
        }
        assert_eq!(guard.use_for(1), HintSet::UNHINTED);
    }

    /// T-G6.3.2: do give a hint set held-out runs no faster than the unhinted plan's; expect it
    /// never admitted.
    #[test]
    fn t_g6_3_2_a_hint_no_faster_is_never_used() {
        let mut guard = HintGuard::new(1).unwrap();
        let hint = HintSet::ARMS[2];
        let mut rng = SplitMix64(11);
        for _ in 0..32 {
            let noise = 1.0 + 0.2 * rng.unit();
            guard.record(9, HintSet::UNHINTED, 1000.0 * noise).unwrap();
            let verdict = guard
                .record(9, hint, 1000.0 * (1.05 + 0.2 * rng.unit()))
                .unwrap();
            assert_ne!(verdict, Verdict::Admitted);
        }
        assert_eq!(guard.use_for(9), HintSet::UNHINTED);
    }

    /// T-G6.3.3: do train the model on many runs and record runs of many more classes than the
    /// guard holds; expect the model's state unchanged in size and the guard at its bound.
    #[test]
    fn t_g6_3_3_memory_stays_bounded() {
        let mut chooser = HintChooser::new(3, 8).unwrap();
        let model_bytes = std::mem::size_of_val(&chooser.model);
        for i in 0..10_000u64 {
            let f = plan(&[
                (1, (i % 3) as f64),
                (7, (i % 2) as f64),
                (13, (i % 17) as f64),
            ]);
            chooser
                .model
                .observe(&f, 1000.0 + (i % 17) as f64 * 50.0)
                .unwrap();
            chooser.guard.record(i, HintSet::UNHINTED, 1000.0).unwrap();
            assert!(chooser.guard.len() <= 8);
        }
        assert_eq!(chooser.model.runs(), 10_000);
        assert_eq!(std::mem::size_of_val(&chooser.model), model_bytes);
        let bytes = chooser.guard.heap_bytes();
        for i in 10_000..20_000u64 {
            chooser.guard.record(i, HintSet::UNHINTED, 1000.0).unwrap();
        }
        assert_eq!(chooser.guard.len(), 8);
        assert_eq!(chooser.guard.heap_bytes(), bytes);
    }

    /// AC-G6.3.1: do train on runs where hash-join plans take a tenth of the time of plans
    /// without; expect the posterior mean to predict each plan's time and choose the hash join's
    /// hint set, and sampling to choose it most of the time.
    #[test]
    fn ac_g6_3_1_the_model_learns_which_plan_is_faster() {
        let mut chooser = HintChooser::new(5, 4).unwrap();
        let loops = plan(&[(5, 2.0), (13, 20.0), (15, 22.0)]);
        let hashes = plan(&[(1, 2.0), (7, 1.0), (13, 20.0), (15, 21.0)]);
        let candidates = [(HintSet::UNHINTED, loops), (HintSet::ARMS[6], hashes)];
        assert_eq!(chooser.explore(&candidates), HintSet::UNHINTED);
        let mut rng = SplitMix64(1);
        for _ in 0..64 {
            let noise = 1.0 + 0.1 * rng.unit();
            chooser.model.observe(&loops, 10_000.0 * noise).unwrap();
            chooser.model.observe(&hashes, 1_000.0 * noise).unwrap();
        }
        assert_eq!(chooser.best(&candidates), HintSet::ARMS[6]);
        // Each plan's predicted log time lies among its own runs' (their noise is up to a tenth):
        // the prior doesn't shrink the difference between the plans away, the features every
        // plan shares (rows 13 and 15) notwithstanding.
        for (features, nanos) in [(&loops, 10_000.0f64), (&hashes, 1_000.0f64)] {
            let predicted = chooser.model.predict(features).unwrap();
            assert!(
                (nanos.ln()..=(nanos * 1.1).ln()).contains(&predicted),
                "predicted {predicted} for runs of {nanos} ns to a tenth more"
            );
        }
        let chosen = (0..100)
            .filter(|_| chooser.explore(&candidates) == HintSet::ARMS[6])
            .count();
        assert!(
            chosen > 50,
            "sampling chose the hash join {chosen} times of 100"
        );
    }

    /// A plan equal to the unhinted one's is the unhinted set, so a hint never stands for the
    /// plan the cost model gives anyway.
    #[test]
    fn a_hint_whose_plan_is_the_unhinted_one_is_no_hint() {
        let same = plan(&[(1, 1.0)]);
        let candidates = [(HintSet::ARMS[3], same), (HintSet::UNHINTED, same)];
        assert_eq!(fastest(&candidates, |_| 1.0), HintSet::UNHINTED);
    }

    #[test]
    fn every_arm_is_obeyable_and_distinct() {
        for (i, arm) in HintSet::ARMS.iter().enumerate() {
            assert_eq!(arm.arm(), Some(i));
            assert!(arm.0 & (HintSet::HASH_JOIN | HintSet::NESTED_LOOP) != 0);
        }
        assert_eq!(HintSet(HintSet::AUTOMATIC_INDEX).arm(), None);
    }

    #[test]
    fn non_positive_times_are_refused() {
        let mut model = HintModel::new();
        assert!(model.observe(&plan(&[]), 0.0).is_err());
        assert!(model.observe(&plan(&[]), f64::NAN).is_err());
        let mut guard = HintGuard::new(1).unwrap();
        assert!(guard.record(1, HintSet::UNHINTED, -1.0).is_err());
        assert!(HintGuard::new(0).is_err());
    }
}
