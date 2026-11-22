//! Morsel-driven parallel execution: a scan with its filter and aggregates, and a hash join's
//! probe side, spread over the machine's cores (the query-speed plan's step 6, G5 step 3; Leis et
//! al., "Morsel-Driven Parallelism", SIGMOD 2014). docs/design/executor.md section 8 states the
//! design, its sources and what would falsify it.
//!
//! The input is cut into morsels, contiguous runs of rows that a dispatcher (one atomic counter)
//! hands to whichever worker asks next, so a slow worker takes fewer morsels and no worker waits
//! on a fixed share. Each worker folds its morsels into its own partial state; the partials merge
//! once every worker ends. Nothing here is a picked number: the worker count and the morsel size
//! come from [`choose_degree`], which minimizes a cost model whose terms are measured on the
//! running machine ([`MachineCosts`]) and in the running statement (a pilot run's cost per row).
//!
//! Answers are the same at every worker count and morsel size: counts and integer sums are exact,
//! REAL sums are exact until their one final rounding ([`ExactSum`], Shewchuk 1997), MIN and MAX
//! break ties by the earliest row, groups come out in key order, and a join's matches come out in
//! probe-row order. `ParallelMode::Off` (the default) runs the same code on one thread.

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::alloc::TryClone;
use crate::numeric::Numeric;
use crate::types::Value;

/// The switch: parallel execution is off until a caller turns it on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ParallelMode {
    /// One thread, the same pipeline code.
    #[default]
    Off,
    /// Workers as [`choose_degree`] picks them from measured costs.
    On,
}

/// What a parallel run refuses, and why.
#[derive(Debug, thiserror::Error)]
pub enum MorselError {
    #[error(
        "the statement's memory budget of {budget} bytes would be passed: {asked} more bytes asked with {used} in use"
    )]
    MemoryBudget {
        asked: usize,
        used: usize,
        budget: usize,
    },
    #[error("the machine's core count could not be read: {0}")]
    Cores(std::io::Error),
    #[error("a worker thread could not start: {0}")]
    Spawn(std::io::Error),
    #[error("worker {worker} ended without an answer")]
    WorkerLost { worker: usize },
    #[error("{what} overflowed")]
    Overflow { what: &'static str },
    #[error("integer overflow")]
    IntegerOverflow,
    #[error("column {column} is past the row's {width} columns")]
    NoColumn { column: usize, width: usize },
    #[error("SUM over a {kind} value is not a parallel aggregate")]
    Unsupported { kind: &'static str },
    #[error("out of memory")]
    Alloc,
    #[error("reading the table: {0}")]
    Storage(Box<crate::LimboError>),
    #[error("the table's b-tree is malformed: {0}")]
    Corrupt(&'static str),
}

impl From<std::collections::TryReserveError> for MorselError {
    fn from(_: std::collections::TryReserveError) -> Self {
        MorselError::Alloc
    }
}

/// A running mean of measured nanoseconds: O(1) memory, every sample weighted alike, so it
/// follows the machine as samples accumulate without a picked smoothing factor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeanNanos {
    samples: u64,
    mean: f64,
}

impl MeanNanos {
    pub fn record(&mut self, nanos: f64) {
        // A duration read from `Instant` is finite and non-negative; anything else is not a sample.
        if !nanos.is_finite() || nanos < 0.0 {
            return;
        }
        self.samples = self.samples.saturating_add(1);
        self.mean += (nanos - self.mean) / self.samples as f64;
    }

    pub fn mean(&self) -> Option<f64> {
        if self.samples == 0 {
            None
        } else {
            Some(self.mean)
        }
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }
}

fn nanos(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e9
}

/// The machine's side of the cost model, measured where it runs and refined by every parallel
/// run: its cores, what starting a worker costs, and what one dispatch (a morsel claimed) costs.
#[derive(Clone, Debug)]
pub struct MachineCosts {
    cores: NonZeroUsize,
    worker_start: MeanNanos,
    dispatch: MeanNanos,
}

impl MachineCosts {
    /// Measures this machine: its cores from the OS, one worker started and joined, one dispatch.
    pub fn measure() -> Result<Self, MorselError> {
        let cores = std::thread::available_parallelism().map_err(MorselError::Cores)?;
        let mut costs = MachineCosts {
            cores,
            worker_start: MeanNanos::default(),
            dispatch: MeanNanos::default(),
        };
        let before = Instant::now();
        let began = std::thread::scope(|scope| {
            let handle = std::thread::Builder::new()
                .spawn_scoped(scope, Instant::now)
                .map_err(MorselError::Spawn)?;
            handle
                .join()
                .map_err(|_| MorselError::WorkerLost { worker: 1 })
        })?;
        costs
            .worker_start
            .record(nanos(began.saturating_duration_since(before)));
        let next = AtomicUsize::new(0);
        let before = Instant::now();
        next.fetch_add(1, Ordering::Relaxed);
        costs.dispatch.record(nanos(before.elapsed()));
        Ok(costs)
    }

    /// Costs stated by the caller: a model fed from elsewhere, or a test's fixed machine.
    pub fn stated(cores: NonZeroUsize, worker_start_nanos: f64, dispatch_nanos: f64) -> Self {
        let mut costs = MachineCosts {
            cores,
            worker_start: MeanNanos::default(),
            dispatch: MeanNanos::default(),
        };
        costs.worker_start.record(worker_start_nanos);
        costs.dispatch.record(dispatch_nanos);
        costs
    }

    pub fn cores(&self) -> NonZeroUsize {
        self.cores
    }

    pub fn worker_start(&self) -> MeanNanos {
        self.worker_start
    }

    pub fn dispatch(&self) -> MeanNanos {
        self.dispatch
    }
}

/// How a pipeline runs: on one thread, or on `workers` threads (the caller's among them) taking
/// morsels of `morsel_rows` rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Degree {
    Serial,
    Parallel {
        workers: NonZeroUsize,
        morsel_rows: NonZeroUsize,
    },
}

/// The narrow hook that picks serial or parallel, for `rows` rows still to run at `row_nanos`
/// each (measured by the statement's pilot run).
///
/// The model: with `k` workers and morsels of `m` rows over `n` rows of work `W = n * r`, a run
/// takes `(k - 1) * s` to start the workers, `W / k` to do the work, `(n / m) * d / k` to claim the
/// morsels, and up to `m * r` waiting on the last morsel. For each `k` up to the cores, `m` is the
/// integer nearest the minimum of the last two terms (`m = sqrt(n * d / (k * r))`, bounded so each
/// worker has a morsel); the `k` with the least time wins, and serial (`W`) wins when no `k` beats
/// it, so a short statement stays on one thread.
pub fn choose_degree(
    mode: ParallelMode,
    rows: usize,
    row_nanos: f64,
    costs: &MachineCosts,
) -> Degree {
    if mode == ParallelMode::Off || rows < 2 || !row_nanos.is_finite() || row_nanos <= 0.0 {
        return Degree::Serial;
    }
    let (Some(start), Some(dispatch)) = (costs.worker_start.mean(), costs.dispatch.mean()) else {
        return Degree::Serial;
    };
    let n = rows as f64;
    let work = n * row_nanos;
    let mut best_time = work;
    let mut best = Degree::Serial;
    let most = costs.cores.get().min(rows);
    for k in 2..=most {
        let kf = k as f64;
        let Some(per_worker) = rows.checked_div(k) else {
            continue;
        };
        let m = morsel_rows(n, kf, row_nanos, dispatch, per_worker.max(1));
        let mf = m as f64;
        let time = (kf - 1.0) * start + work / kf + (n / mf) * dispatch / kf + mf * row_nanos;
        if time < best_time {
            if let (Some(workers), Some(morsel_rows)) = (NonZeroUsize::new(k), NonZeroUsize::new(m))
            {
                best_time = time;
                best = Degree::Parallel {
                    workers,
                    morsel_rows,
                };
            }
        }
    }
    best
}

/// The least integer `m` in `1..=most` with `k * r * m^2 >= n * d`: the minimum of
/// `(n / m) * d / k + m * r`, found by bisection (at most one step per bit of `most`).
fn morsel_rows(n: f64, k: f64, row_nanos: f64, dispatch: f64, most: usize) -> usize {
    let target = n * dispatch;
    let reaches = |m: usize| {
        let mf = m as f64;
        k * row_nanos * mf * mf >= target
    };
    let (mut low, mut high) = (1usize, most);
    while low < high {
        let mid = low.saturating_add(high.saturating_sub(low) / 2);
        if reaches(mid) {
            high = mid;
        } else {
            low = mid.saturating_add(1);
        }
    }
    low
}

/// The statement's memory, shared by its workers: each charge is refused once the total would pass
/// the limit, so no worker allocates past what the statement has left.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
}

impl MemoryBudget {
    pub fn new(limit: usize) -> Self {
        MemoryBudget {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    pub fn charge(&self, bytes: usize) -> Result<(), MorselError> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit)
            })
            .map(|_| ())
            .map_err(|used| MorselError::MemoryBudget {
                asked: bytes,
                used,
                budget: self.limit,
            })
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

/// One pipeline (scan to its next breaker), run morsel by morsel into partial states.
pub trait Pipeline: Sync {
    type Partial: Send;
    /// How many input rows the pipeline reads.
    fn rows(&self) -> usize;
    fn empty(&self) -> Self::Partial;
    /// Folds the rows of `rows` (input row positions) into `partial`.
    fn run(
        &self,
        rows: Range<usize>,
        partial: &mut Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError>;
    /// Folds a worker's partial into another.
    fn merge(
        &self,
        into: &mut Self::Partial,
        from: Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError>;
}

/// What a run did: the workers that ran, its morsels, and the pilot's share.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunStats {
    pub workers: usize,
    pub morsels: usize,
    pub morsel_rows: usize,
    pub pilot_rows: usize,
}

/// Runs a pipeline as the switch and the measured costs say: a pilot on the caller's thread first,
/// in morsels doubling from one row, until it has run as long as starting one worker costs (before
/// then no parallel plan can win) or the input ends; then the rest at [`choose_degree`]'s degree.
pub fn execute<P: Pipeline>(
    pipeline: &P,
    mode: ParallelMode,
    costs: &mut MachineCosts,
    budget: &MemoryBudget,
) -> Result<(P::Partial, RunStats), MorselError> {
    let total = pipeline.rows();
    let mut partial = pipeline.empty();
    let mut stats = RunStats {
        workers: 1,
        ..RunStats::default()
    };
    let Some(start_nanos) = costs
        .worker_start
        .mean()
        .filter(|_| mode == ParallelMode::On)
    else {
        pipeline.run(0..total, &mut partial, budget)?;
        stats.morsels = 1;
        stats.morsel_rows = total;
        stats.pilot_rows = total;
        return Ok((partial, stats));
    };
    let began = Instant::now();
    let mut done = 0usize;
    let mut size = 1usize;
    // Ends: every pass moves `done` forward by at least one row, and `done` stops at `total`.
    while done < total {
        let end = done.saturating_add(size).min(total);
        pipeline.run(done..end, &mut partial, budget)?;
        stats.morsels = stats.morsels.saturating_add(1);
        done = end;
        if nanos(began.elapsed()) >= start_nanos {
            break;
        }
        size = size.saturating_mul(2);
    }
    stats.pilot_rows = done;
    let rest = total.saturating_sub(done);
    if rest == 0 {
        return Ok((partial, stats));
    }
    let row_nanos = nanos(began.elapsed()) / done.max(1) as f64;
    let degree = choose_degree(mode, rest, row_nanos, costs);
    let (more, run) = execute_at(pipeline, done, degree, Some(costs), budget)?;
    pipeline.merge(&mut partial, more, budget)?;
    stats.workers = run.workers;
    stats.morsels = stats.morsels.saturating_add(run.morsels);
    stats.morsel_rows = run.morsel_rows;
    Ok((partial, stats))
}

/// Runs a pipeline's rows from `first` on at a stated degree; `costs`, when given, learns this
/// run's worker starts and dispatches.
pub fn execute_at<P: Pipeline>(
    pipeline: &P,
    first: usize,
    degree: Degree,
    costs: Option<&mut MachineCosts>,
    budget: &MemoryBudget,
) -> Result<(P::Partial, RunStats), MorselError> {
    let total = pipeline.rows();
    let rest = total.saturating_sub(first);
    let (workers, morsel_rows) = match degree {
        Degree::Parallel {
            workers,
            morsel_rows,
        } if workers.get() > 1 && rest > 0 => (workers.get(), morsel_rows.get()),
        _ => {
            let mut partial = pipeline.empty();
            pipeline.run(first..total.max(first), &mut partial, budget)?;
            return Ok((
                partial,
                RunStats {
                    workers: 1,
                    morsels: 1,
                    morsel_rows: rest,
                    pilot_rows: 0,
                },
            ));
        }
    };
    let morsels = rest.div_ceil(morsel_rows);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    // One worker's loop: claim a morsel, run it, until the morsels run out or a worker fails.
    let work = || -> WorkerResult<P::Partial> {
        let mut partial = pipeline.empty();
        let mut claim_nanos = 0.0;
        let mut claims = 0u64;
        // Ends: each pass takes the next morsel index, and indexes past `morsels` end the loop.
        loop {
            let before = Instant::now();
            let index = next.fetch_add(1, Ordering::Relaxed);
            claim_nanos += nanos(before.elapsed());
            claims = claims.saturating_add(1);
            if index >= morsels || stop.load(Ordering::Relaxed) {
                break;
            }
            let low = index
                .checked_mul(morsel_rows)
                .and_then(|offset| offset.checked_add(first))
                .ok_or(MorselError::Overflow {
                    what: "a morsel's first row",
                });
            let ran = low.and_then(|low| {
                pipeline.run(
                    low..low.saturating_add(morsel_rows).min(total),
                    &mut partial,
                    budget,
                )
            });
            if let Err(error) = ran {
                stop.store(true, Ordering::Relaxed);
                return WorkerResult {
                    partial: Err(error),
                    claim_nanos,
                    claims,
                    began: Instant::now(),
                };
            }
        }
        WorkerResult {
            partial: Ok(partial),
            claim_nanos,
            claims,
            began: Instant::now(),
        }
    };
    let work = &work;
    let mut started = 1usize;
    let outcome = std::thread::scope(|scope| -> Result<Joined<P::Partial>, MorselError> {
        let mut handles = Vec::new();
        handles.try_reserve_exact(workers.saturating_sub(1))?;
        for _ in 1..workers {
            let spawned_at = Instant::now();
            let spawned = std::thread::Builder::new().spawn_scoped(scope, move || {
                let began = Instant::now();
                let mut result = work();
                result.began = began;
                result
            });
            match spawned {
                Ok(handle) => {
                    handles.push((spawned_at, handle));
                    started = started.saturating_add(1);
                }
                // The workers that started, the caller's thread among them, take every morsel.
                Err(_) => break,
            }
        }
        let mine = work();
        let mut results = Vec::new();
        results.try_reserve_exact(handles.len().saturating_add(1))?;
        let mut starts = Vec::new();
        starts.try_reserve_exact(handles.len())?;
        results.push(mine);
        let mut lost = None;
        for (worker, (spawned_at, handle)) in handles.into_iter().enumerate() {
            match handle.join() {
                Ok(result) => {
                    starts.push(nanos(result.began.saturating_duration_since(spawned_at)));
                    results.push(result);
                }
                Err(_) => lost = Some(worker.saturating_add(1)),
            }
        }
        match lost {
            Some(worker) => Err(MorselError::WorkerLost { worker }),
            None => Ok((results, starts)),
        }
    })?;
    let (results, starts) = outcome;
    if let Some(costs) = costs {
        for start in starts {
            costs.worker_start.record(start);
        }
        for result in &results {
            if result.claims > 0 {
                costs
                    .dispatch
                    .record(result.claim_nanos / result.claims as f64);
            }
        }
    }
    let mut merged = pipeline.empty();
    for result in results {
        pipeline.merge(&mut merged, result.partial?, budget)?;
    }
    Ok((
        merged,
        RunStats {
            workers: started,
            morsels,
            morsel_rows,
            pilot_rows: 0,
        },
    ))
}

/// Every worker's result, and each started worker's start latency in nanoseconds.
type Joined<T> = (Vec<WorkerResult<T>>, Vec<f64>);

struct WorkerResult<T> {
    partial: Result<T, MorselError>,
    claim_nanos: f64,
    claims: u64,
    began: Instant,
}

/// A REAL sum kept exact until its one rounding: the non-overlapping partials of Shewchuk's
/// "Adaptive Precision Floating-Point Arithmetic" (1997), as Python's `math.fsum` keeps them. The
/// sum of a set of doubles is then one number whatever the order they arrive in, so a parallel
/// sum equals the serial one. Finite partials never overlap, so there are at most as many as the
/// double's exponent range over its precision (2098 / 53, about 40): the bound is the format's.
#[derive(Clone, Debug, Default)]
pub struct ExactSum {
    partials: Vec<f64>,
    /// The sum of the non-finite inputs, and of a sum that left the double's range.
    special: Option<f64>,
}

impl ExactSum {
    pub fn add(&mut self, value: f64) -> Result<(), MorselError> {
        if !value.is_finite() {
            self.special = Some(self.special.map_or(value, |special| special + value));
            return Ok(());
        }
        let mut x = value;
        let mut kept = 0usize;
        let count = self.partials.len();
        for j in 0..count {
            let Some(mut y) = self.partials.get(j).copied() else {
                break;
            };
            if x.abs() < y.abs() {
                std::mem::swap(&mut x, &mut y);
            }
            let high = x + y;
            let low = y - (high - x);
            if low != 0.0 {
                if let Some(slot) = self.partials.get_mut(kept) {
                    *slot = low;
                }
                kept = kept.saturating_add(1);
            }
            x = high;
        }
        self.partials.truncate(kept);
        if !x.is_finite() {
            // The exact sum left the double's range: it rounds to this infinity.
            self.special = Some(self.special.map_or(x, |special| special + x));
            return Ok(());
        }
        self.partials.try_reserve(1)?;
        self.partials.push(x);
        Ok(())
    }

    pub fn merge(&mut self, other: &ExactSum) -> Result<(), MorselError> {
        for partial in &other.partials {
            self.add(*partial)?;
        }
        if let Some(special) = other.special {
            self.add(special)?;
        }
        Ok(())
    }

    /// The exact sum rounded once to the nearest double, ties to even (`math.fsum`'s last step).
    pub fn value(&self) -> f64 {
        if let Some(special) = self.special {
            return special;
        }
        let mut rest = self.partials.iter().rev();
        let Some(mut high) = rest.next().copied() else {
            return 0.0;
        };
        let mut low = 0.0;
        let mut below = None;
        while let Some(y) = rest.next().copied() {
            let x = high;
            high = x + y;
            let rounded = high - x;
            low = y - rounded;
            if low != 0.0 {
                below = rest.next().copied();
                break;
            }
        }
        if let Some(next) = below {
            if (low < 0.0 && next < 0.0) || (low > 0.0 && next > 0.0) {
                let y = low * 2.0;
                let x = high + y;
                if y == x - high {
                    high = x;
                }
            }
        }
        high
    }
}

/// A value as a grouping or join key: SQLite's numeric equality (an integral REAL equals its
/// INTEGER), text and blobs by their bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum KeyPart {
    Null,
    Int(i64),
    Real(u64),
    Text(Box<str>),
    Blob(Box<[u8]>),
}

/// The doubles in `[-2^63, 2^63)`, the INTEGER range: an integral REAL in it is that INTEGER.
const I64_LOW: f64 = -9_223_372_036_854_775_808.0;
const I64_HIGH: f64 = 9_223_372_036_854_775_808.0;

fn key_part(value: &Value) -> KeyPart {
    match value {
        Value::Null => KeyPart::Null,
        Value::Numeric(Numeric::Integer(i)) => KeyPart::Int(*i),
        Value::Numeric(Numeric::Float(f)) => {
            let f = f64::from(*f);
            match integral(f) {
                Some(i) => KeyPart::Int(i),
                None => KeyPart::Real(f.to_bits()),
            }
        }
        Value::Text(text) => KeyPart::Text(Box::from(text.as_str())),
        Value::Blob(_) => KeyPart::Blob(Box::from(value.to_blob().unwrap_or_default())),
    }
}

/// The INTEGER an integral double in the INTEGER range equals, read from its bits (IEEE 754
/// binary64: 1 sign bit, 11 exponent bits biased by 1023, 52 fraction bits) without a lossy cast.
fn integral(f: f64) -> Option<i64> {
    if f.fract() != 0.0 || !(I64_LOW..I64_HIGH).contains(&f) {
        return None;
    }
    if f == 0.0 {
        return Some(0);
    }
    let bits = f.to_bits();
    let exponent = i32::try_from((bits >> 52) & 0x7ff)
        .ok()?
        .checked_sub(1075)?;
    let significand = i128::from((bits & 0xf_ffff_ffff_ffff) | 0x10_0000_0000_0000);
    let magnitude = if exponent >= 0 {
        significand.checked_shl(u32::try_from(exponent).ok()?)?
    } else {
        significand.checked_shr(exponent.unsigned_abs())?
    };
    let signed = if bits >> 63 == 1 {
        magnitude.checked_neg()?
    } else {
        magnitude
    };
    i64::try_from(signed).ok()
}

fn column(row: &[Value], column: usize) -> Result<&Value, MorselError> {
    row.get(column).ok_or(MorselError::NoColumn {
        column,
        width: row.len(),
    })
}

/// A comparison of a column with a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Le,
    Eq,
    Ne,
    Gt,
    Ge,
}

/// One term of a scan's filter; a row passes when every term holds.
#[derive(Clone, Debug)]
pub enum Predicate {
    /// `column op constant`: false when either side is NULL, values compared as SQLite orders them.
    Compare {
        column: usize,
        op: CmpOp,
        value: Value,
    },
    IsNull {
        column: usize,
    },
    NotNull {
        column: usize,
    },
}

impl Predicate {
    pub(crate) fn holds(&self, row: &[Value]) -> Result<bool, MorselError> {
        Ok(match self {
            Predicate::Compare {
                column: at,
                op,
                value,
            } => {
                let left = column(row, *at)?;
                if matches!(left, Value::Null) || matches!(value, Value::Null) {
                    return Ok(false);
                }
                let order = left.cmp(value);
                match op {
                    CmpOp::Lt => order == CmpOrdering::Less,
                    CmpOp::Le => order != CmpOrdering::Greater,
                    CmpOp::Eq => order == CmpOrdering::Equal,
                    CmpOp::Ne => order != CmpOrdering::Equal,
                    CmpOp::Gt => order == CmpOrdering::Greater,
                    CmpOp::Ge => order != CmpOrdering::Less,
                }
            }
            Predicate::IsNull { column: at } => matches!(column(row, *at)?, Value::Null),
            Predicate::NotNull { column: at } => !matches!(column(row, *at)?, Value::Null),
        })
    }
}

/// An aggregate a scan computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aggregate {
    CountStar,
    Count(usize),
    Sum(usize),
    Min(usize),
    Max(usize),
}

#[derive(Clone, Debug)]
enum Accumulator {
    Count(u64),
    /// Integers summed in 128 bits (no overflow before 2^64 rows), REALs exactly; `seen` is false
    /// while only NULLs have come.
    Sum {
        integers: i128,
        reals: ExactSum,
        any_real: bool,
        seen: bool,
    },
    /// The least or greatest value with the row it came from; ties keep the earliest row.
    Extreme {
        best: Option<(Value, usize)>,
        greatest: bool,
    },
}

impl Accumulator {
    fn new(aggregate: Aggregate) -> Self {
        match aggregate {
            Aggregate::CountStar | Aggregate::Count(_) => Accumulator::Count(0),
            Aggregate::Sum(_) => Accumulator::Sum {
                integers: 0,
                reals: ExactSum::default(),
                any_real: false,
                seen: false,
            },
            Aggregate::Min(_) => Accumulator::Extreme {
                best: None,
                greatest: false,
            },
            Aggregate::Max(_) => Accumulator::Extreme {
                best: None,
                greatest: true,
            },
        }
    }

    fn step(
        &mut self,
        aggregate: Aggregate,
        row: &[Value],
        position: usize,
    ) -> Result<(), MorselError> {
        match (self, aggregate) {
            (Accumulator::Count(count), Aggregate::CountStar) => *count = count.saturating_add(1),
            (Accumulator::Count(count), Aggregate::Count(at)) => {
                if !matches!(column(row, at)?, Value::Null) {
                    *count = count.saturating_add(1);
                }
            }
            (
                Accumulator::Sum {
                    integers,
                    reals,
                    any_real,
                    seen,
                },
                Aggregate::Sum(at),
            ) => match column(row, at)? {
                Value::Null => {}
                Value::Numeric(Numeric::Integer(i)) => {
                    *integers =
                        integers
                            .checked_add(i128::from(*i))
                            .ok_or(MorselError::Overflow {
                                what: "an integer sum",
                            })?;
                    *seen = true;
                }
                Value::Numeric(Numeric::Float(f)) => {
                    reals.add(f64::from(*f))?;
                    *any_real = true;
                    *seen = true;
                }
                Value::Text(_) => return Err(MorselError::Unsupported { kind: "TEXT" }),
                Value::Blob(_) => return Err(MorselError::Unsupported { kind: "BLOB" }),
            },
            (Accumulator::Extreme { best, greatest }, Aggregate::Min(at) | Aggregate::Max(at)) => {
                let value = column(row, at)?;
                if !matches!(value, Value::Null) {
                    let better = match best {
                        None => true,
                        Some((current, _)) => {
                            let order = value.cmp(current);
                            if *greatest {
                                order == CmpOrdering::Greater
                            } else {
                                order == CmpOrdering::Less
                            }
                        }
                    };
                    if better {
                        *best =
                            Some((value.try_clone().map_err(|_| MorselError::Alloc)?, position));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn merge(&mut self, other: Accumulator) -> Result<(), MorselError> {
        match (self, other) {
            (Accumulator::Count(count), Accumulator::Count(more)) => {
                *count = count.saturating_add(more)
            }
            (
                Accumulator::Sum {
                    integers,
                    reals,
                    any_real,
                    seen,
                },
                Accumulator::Sum {
                    integers: more,
                    reals: more_reals,
                    any_real: more_real,
                    seen: more_seen,
                },
            ) => {
                *integers = integers.checked_add(more).ok_or(MorselError::Overflow {
                    what: "an integer sum",
                })?;
                reals.merge(&more_reals)?;
                *any_real |= more_real;
                *seen |= more_seen;
            }
            (
                Accumulator::Extreme { best, greatest },
                Accumulator::Extreme {
                    best: Some((value, position)),
                    ..
                },
            ) => {
                let better = match best {
                    None => true,
                    Some((current, at)) => match value.cmp(current) {
                        CmpOrdering::Equal => position < *at,
                        CmpOrdering::Greater => *greatest,
                        CmpOrdering::Less => !*greatest,
                    },
                };
                if better {
                    *best = Some((value, position));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<Value, MorselError> {
        Ok(match self {
            Accumulator::Count(count) => {
                Value::from_i64(i64::try_from(count).map_err(|_| MorselError::IntegerOverflow)?)
            }
            Accumulator::Sum { seen: false, .. } => Value::Null,
            Accumulator::Sum {
                integers,
                any_real: false,
                ..
            } => {
                Value::from_i64(i64::try_from(integers).map_err(|_| MorselError::IntegerOverflow)?)
            }
            Accumulator::Sum {
                integers,
                mut reals,
                any_real: true,
                ..
            } => {
                // The integer part enters the exact sum in three pieces each a double holds exactly.
                // `integers` is top * 2^100 + middle * 2^50 + low, with middle and low in [0, 2^50)
                // and top within 2^28 of zero: each converts to a double exactly.
                let piece = |shift: u32, mask: i128| -> Result<f64, MorselError> {
                    let part = integers.checked_shr(shift).ok_or(MorselError::Overflow {
                        what: "a sum's piece",
                    })? & mask;
                    let whole = i64::try_from(part).map_err(|_| MorselError::Overflow {
                        what: "a sum's piece",
                    })? as f64;
                    Ok(whole
                        * 2f64.powi(i32::try_from(shift).map_err(|_| MorselError::Overflow {
                            what: "a sum's piece",
                        })?))
                };
                const LOW_50: i128 = 0x3_ffff_ffff_ffff;
                reals.add(piece(100, -1)?)?;
                reals.add(piece(50, LOW_50)?)?;
                reals.add(piece(0, LOW_50)?)?;
                Value::from_f64(reals.value())
            }
            Accumulator::Extreme { best, .. } => best.map_or(Value::Null, |(value, _)| value),
        })
    }
}

#[derive(Clone, Debug)]
struct Group {
    first_row: usize,
    key: Vec<Value>,
    accumulators: Vec<Accumulator>,
}

/// A scan with a filter, grouped or not, and its aggregates: the morsel-driven pipeline of a
/// whole-table aggregate (Leis et al. section 4.4: partial aggregates per worker, merged once).
pub struct ScanAggregate<'a> {
    pub rows: &'a [Vec<Value>],
    pub filter: &'a [Predicate],
    pub group_by: &'a [usize],
    pub aggregates: &'a [Aggregate],
}

/// The partial state of a [`ScanAggregate`]: its groups by key.
#[derive(Debug, Default)]
pub struct AggregatePartial {
    groups: HashMap<Vec<KeyPart>, Group>,
}

impl ScanAggregate<'_> {
    /// The answer: one row per group, `group_by`'s values then the aggregates, in key order; with
    /// no `group_by`, one row even over no input.
    pub fn finish(&self, partial: AggregatePartial) -> Result<Vec<Vec<Value>>, MorselError> {
        let mut groups: Vec<Group> = partial.groups.into_values().collect();
        if groups.is_empty() && self.group_by.is_empty() {
            groups.push(Group {
                first_row: 0,
                key: Vec::new(),
                accumulators: self
                    .aggregates
                    .iter()
                    .map(|a| Accumulator::new(*a))
                    .collect(),
            });
        }
        groups.sort_by(|a, b| a.key.cmp(&b.key).then(a.first_row.cmp(&b.first_row)));
        let mut out = Vec::new();
        out.try_reserve_exact(groups.len())?;
        for group in groups {
            let mut row = group.key;
            row.try_reserve_exact(group.accumulators.len())?;
            for accumulator in group.accumulators {
                row.push(accumulator.finish()?);
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Folds one input row, at `position` in the input's order (the order ties break by), into
    /// `partial`: the loop body every source of rows shares, rows in memory or a worker's cursor
    /// (`morsel_scan.rs`).
    pub(crate) fn fold(
        &self,
        row: &[Value],
        position: usize,
        partial: &mut AggregatePartial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        let mut passes = true;
        for term in self.filter {
            if !term.holds(row)? {
                passes = false;
                break;
            }
        }
        if !passes {
            return Ok(());
        }
        let mut key = Vec::new();
        key.try_reserve_exact(self.group_by.len())?;
        for at in self.group_by {
            key.push(key_part(column(row, *at)?));
        }
        let group = match partial.groups.get_mut(&key) {
            Some(group) => group,
            None => {
                budget.charge(self.group_bytes())?;
                let mut values = Vec::new();
                values.try_reserve_exact(self.group_by.len())?;
                for at in self.group_by {
                    values.push(
                        column(row, *at)?
                            .try_clone()
                            .map_err(|_| MorselError::Alloc)?,
                    );
                }
                let accumulators = self
                    .aggregates
                    .iter()
                    .map(|a| Accumulator::new(*a))
                    .collect();
                partial.groups.try_reserve(1)?;
                partial.groups.entry(key).or_insert(Group {
                    first_row: position,
                    key: values,
                    accumulators,
                })
            }
        };
        for (accumulator, aggregate) in group.accumulators.iter_mut().zip(self.aggregates) {
            accumulator.step(*aggregate, row, position)?;
        }
        Ok(())
    }

    fn group_bytes(&self) -> usize {
        let key = std::mem::size_of::<KeyPart>()
            .saturating_add(std::mem::size_of::<Value>())
            .saturating_mul(self.group_by.len());
        let accumulators = std::mem::size_of::<Accumulator>().saturating_mul(self.aggregates.len());
        std::mem::size_of::<Group>()
            .saturating_add(key)
            .saturating_add(accumulators)
    }
}

impl Pipeline for ScanAggregate<'_> {
    type Partial = AggregatePartial;

    fn rows(&self) -> usize {
        self.rows.len()
    }

    fn empty(&self) -> AggregatePartial {
        AggregatePartial::default()
    }

    fn run(
        &self,
        rows: Range<usize>,
        partial: &mut AggregatePartial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        let first = rows.start;
        let Some(morsel) = self.rows.get(rows) else {
            return Err(MorselError::Overflow {
                what: "a morsel's rows",
            });
        };
        for (offset, row) in morsel.iter().enumerate() {
            self.fold(row, first.saturating_add(offset), partial, budget)?;
        }
        Ok(())
    }

    fn merge(
        &self,
        into: &mut AggregatePartial,
        from: AggregatePartial,
        _budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        into.groups.try_reserve(from.groups.len())?;
        for (key, group) in from.groups {
            match into.groups.get_mut(&key) {
                None => {
                    into.groups.insert(key, group);
                }
                Some(mine) => {
                    if group.first_row < mine.first_row {
                        mine.first_row = group.first_row;
                        mine.key = group.key;
                    }
                    for (accumulator, other) in mine.accumulators.iter_mut().zip(group.accumulators)
                    {
                        accumulator.merge(other)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A hash join's table, cut into partitions by each key's hash so the workers fill it together
/// (QT.6; Leis et al. 2014 section 4.2 build theirs in the same two phases): each worker scatters
/// its rows' entries into one run per partition, then each partition is filled by one worker from
/// every run, with no lock on the table. A key's build rows stay in build order, whatever worker
/// read them, because the runs are filled in the order of their rows.
pub struct PartitionedTable {
    partitioner: Partitioner,
    partitions: Vec<HashMap<Vec<KeyPart>, Vec<usize>>>,
}

/// One entry scattered for the fill: a build row's key and its place in the run that holds it.
pub(crate) type Entry = (Vec<KeyPart>, usize);

/// A worker's entries for one run of build rows, one list per partition.
pub(crate) type Scattered = Vec<Vec<Entry>>;

/// Says which partition a key goes to: the partition count is the build's, one per worker that
/// may fill (the machine's cores when parallel mode is on, one when it is off), so no partition
/// count is picked.
#[derive(Clone)]
pub struct Partitioner {
    hasher: std::hash::RandomState,
    count: NonZeroUsize,
}

impl Partitioner {
    pub fn new(count: NonZeroUsize) -> Self {
        Partitioner {
            hasher: std::hash::RandomState::new(),
            count,
        }
    }

    pub fn count(&self) -> NonZeroUsize {
        self.count
    }

    fn partition(&self, key: &[KeyPart]) -> Result<usize, MorselError> {
        use std::hash::BuildHasher;
        let count = u64::try_from(self.count.get()).map_err(|_| MorselError::Overflow {
            what: "the partition count",
        })?;
        usize::try_from(self.hasher.hash_one(key) % count).map_err(|_| MorselError::Overflow {
            what: "a key's partition",
        })
    }

    /// A run's empty lists, one per partition.
    pub(crate) fn empty(&self) -> Result<Scattered, MorselError> {
        let mut out = Vec::new();
        out.try_reserve_exact(self.count.get())?;
        out.resize_with(self.count.get(), Vec::new);
        Ok(out)
    }

    /// Scatters `row`'s entry, its key at `key_at`, as the run's row `at`, charging the entry
    /// (its key's parts and their text or blob bytes, and its place) to the statement's budget.
    /// False when the key holds a NULL: such a row matches nothing and is not kept.
    pub(crate) fn scatter(
        &self,
        row: &[Value],
        key_at: &[usize],
        at: usize,
        into: &mut Scattered,
        budget: &MemoryBudget,
    ) -> Result<bool, MorselError> {
        let Some(key) = join_key(row, key_at)? else {
            return Ok(false);
        };
        let bytes = key.iter().fold(
            std::mem::size_of::<Entry>()
                .saturating_add(std::mem::size_of::<KeyPart>().saturating_mul(key.len())),
            |sum, part| {
                sum.saturating_add(match part {
                    KeyPart::Text(text) => text.len(),
                    KeyPart::Blob(blob) => blob.len(),
                    _ => 0,
                })
            },
        );
        budget.charge(bytes)?;
        let partition = self.partition(&key)?;
        let list = into.get_mut(partition).ok_or(MorselError::Overflow {
            what: "a key's partition",
        })?;
        list.try_reserve(1)?;
        list.push((key, at));
        Ok(true)
    }

    /// Fills the table from the workers' runs, in the order of their rows: run `r`'s row `at` is
    /// build row `offsets[r] + at`. Each partition is filled by one worker, the workers as the
    /// switch and the measured costs pick them.
    pub(crate) fn fill(
        &self,
        runs: Vec<Scattered>,
        offsets: &[usize],
        mode: ParallelMode,
        costs: &mut MachineCosts,
        budget: &MemoryBudget,
    ) -> Result<(PartitionedTable, RunStats), MorselError> {
        if runs.len() != offsets.len() {
            return Err(MorselError::Overflow {
                what: "the build's runs",
            });
        }
        let count = self.count.get();
        let mut by_partition: Vec<Vec<(usize, Vec<Entry>)>> = Vec::new();
        by_partition.try_reserve_exact(count)?;
        by_partition.resize_with(count, Vec::new);
        for (run, offset) in runs.into_iter().zip(offsets) {
            if run.len() != count {
                return Err(MorselError::Overflow {
                    what: "a run's partitions",
                });
            }
            for (list, entries) in by_partition.iter_mut().zip(run) {
                if !entries.is_empty() {
                    list.try_reserve(1)?;
                    list.push((*offset, entries));
                }
            }
        }
        let mut lists = Vec::new();
        lists.try_reserve_exact(count)?;
        lists.extend(by_partition.into_iter().map(crate::sync::Mutex::new));
        let fill = Fill { lists };
        let (mut filled, stats) = execute(&fill, mode, costs, budget)?;
        filled.sort_by_key(|(partition, _)| *partition);
        if filled.len() != count {
            return Err(MorselError::Overflow {
                what: "the filled partitions",
            });
        }
        let mut partitions = Vec::new();
        partitions.try_reserve_exact(count)?;
        partitions.extend(filled.into_iter().map(|(_, map)| map));
        Ok((
            PartitionedTable {
                partitioner: self.clone(),
                partitions,
            },
            stats,
        ))
    }
}

/// A partition's entries from every run, each run with the build row its rows are numbered from.
type PartitionRuns = Vec<(usize, Vec<Entry>)>;

/// The fill's pipeline: its rows are the partitions, each taken whole by the worker that claims it.
struct Fill {
    lists: Vec<crate::sync::Mutex<PartitionRuns>>,
}

impl Pipeline for Fill {
    type Partial = Vec<(usize, HashMap<Vec<KeyPart>, Vec<usize>>)>;

    fn rows(&self) -> usize {
        self.lists.len()
    }

    fn empty(&self) -> Self::Partial {
        Vec::new()
    }

    fn run(
        &self,
        partitions: Range<usize>,
        partial: &mut Self::Partial,
        _budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        for partition in partitions {
            let list = self.lists.get(partition).ok_or(MorselError::Overflow {
                what: "a fill's partition",
            })?;
            let runs = std::mem::take(&mut *list.lock());
            let mut map: HashMap<Vec<KeyPart>, Vec<usize>> = HashMap::new();
            // Runs in the order of their rows, each run's entries in its rows' order: a key's
            // build rows go in in build order. Every entry was charged when it was scattered;
            // its key moves into the table.
            for (offset, entries) in runs {
                map.try_reserve(entries.len())?;
                for (key, at) in entries {
                    let position = offset.checked_add(at).ok_or(MorselError::Overflow {
                        what: "a build row's position",
                    })?;
                    let rows = map.entry(key).or_default();
                    rows.try_reserve(1)?;
                    rows.push(position);
                }
            }
            partial.try_reserve(1)?;
            partial.push((partition, map));
        }
        Ok(())
    }

    fn merge(
        &self,
        into: &mut Self::Partial,
        from: Self::Partial,
        _budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        into.try_reserve(from.len())?;
        into.extend(from);
        Ok(())
    }
}

impl PartitionedTable {
    /// The build rows whose key is `key`, in build order.
    fn get(&self, key: &[KeyPart]) -> Result<&[usize], MorselError> {
        let partition = self.partitioner.partition(key)?;
        Ok(self
            .partitions
            .get(partition)
            .and_then(|map| map.get(key))
            .map_or(&[], Vec::as_slice))
    }

    /// How many partitions hold at least one key: a test's count that the fill was partitioned.
    pub fn filled_partitions(&self) -> usize {
        self.partitions.iter().filter(|map| !map.is_empty()).count()
    }
}

/// A hash join's probe side, run in parallel against a table built once (an inner equi-join; a
/// row with a NULL in its key matches nothing). Matches come out in probe-row order, each probe
/// row's matches in build-row order, as the serial probe gives them.
pub struct HashProbe<'a> {
    probe: &'a [Vec<Value>],
    probe_key: &'a [usize],
    table: PartitionedTable,
}

/// A worker's matches, each run of them tagged with the first probe row of its morsel.
pub type ProbePartial = Vec<(usize, Vec<(usize, usize)>)>;

impl<'a> HashProbe<'a> {
    /// Builds the table from `build`'s rows on this thread, in one partition, charging it to the
    /// statement's budget.
    pub fn build(
        build: &[Vec<Value>],
        build_key: &[usize],
        probe: &'a [Vec<Value>],
        probe_key: &'a [usize],
        budget: &MemoryBudget,
    ) -> Result<Self, MorselError> {
        let partitioner = Partitioner::new(NonZeroUsize::MIN);
        let mut run = partitioner.empty()?;
        for (at, row) in build.iter().enumerate() {
            partitioner.scatter(row, build_key, at, &mut run, budget)?;
        }
        let mut runs = Vec::new();
        runs.try_reserve_exact(1)?;
        runs.push(run);
        let mut costs = MachineCosts::stated(NonZeroUsize::MIN, 0.0, 0.0);
        let (table, _) = partitioner.fill(runs, &[0], ParallelMode::Off, &mut costs, budget)?;
        Ok(Self::over(table, probe, probe_key))
    }

    /// The probe of a table built already (`TableBuild` fills one from a table's leaves).
    pub fn over(table: PartitionedTable, probe: &'a [Vec<Value>], probe_key: &'a [usize]) -> Self {
        HashProbe {
            probe,
            probe_key,
            table,
        }
    }

    /// The table probed.
    pub fn table(&self) -> &PartitionedTable {
        &self.table
    }

    /// The build rows whose key equals `row`'s at the probe key's columns, in build order; none
    /// when the key holds a NULL or nothing matches.
    pub(crate) fn matches(&self, row: &[Value]) -> Result<&[usize], MorselError> {
        let Some(key) = join_key(row, self.probe_key)? else {
            return Ok(&[]);
        };
        self.table.get(&key)
    }

    /// The matches as (probe row, build row), in the serial order.
    pub fn finish(&self, mut partial: ProbePartial) -> Result<Vec<(usize, usize)>, MorselError> {
        partial.sort_by_key(|(first, _)| *first);
        let total = partial
            .iter()
            .map(|(_, run)| run.len())
            .fold(0usize, usize::saturating_add);
        let mut out = Vec::new();
        out.try_reserve_exact(total)?;
        for (_, run) in partial {
            out.extend(run);
        }
        Ok(out)
    }
}

fn join_key(row: &[Value], at: &[usize]) -> Result<Option<Vec<KeyPart>>, MorselError> {
    let mut key = Vec::new();
    key.try_reserve_exact(at.len())?;
    for column_at in at {
        match key_part(column(row, *column_at)?) {
            KeyPart::Null => return Ok(None),
            part => key.push(part),
        }
    }
    Ok(Some(key))
}

impl Pipeline for HashProbe<'_> {
    type Partial = ProbePartial;

    fn rows(&self) -> usize {
        self.probe.len()
    }

    fn empty(&self) -> ProbePartial {
        Vec::new()
    }

    fn run(
        &self,
        rows: Range<usize>,
        partial: &mut ProbePartial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        let first = rows.start;
        let Some(morsel) = self.probe.get(rows) else {
            return Err(MorselError::Overflow {
                what: "a morsel's rows",
            });
        };
        let mut found = Vec::new();
        for (offset, row) in morsel.iter().enumerate() {
            let matches = self.matches(row)?;
            if matches.is_empty() {
                continue;
            }
            budget.charge(std::mem::size_of::<(usize, usize)>().saturating_mul(matches.len()))?;
            found.try_reserve(matches.len())?;
            let position = first.saturating_add(offset);
            found.extend(matches.iter().map(|build| (position, *build)));
        }
        partial.try_reserve(1)?;
        partial.push((first, found));
        Ok(())
    }

    fn merge(
        &self,
        into: &mut ProbePartial,
        from: ProbePartial,
        _budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        into.try_reserve(from.len())?;
        into.extend(from);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;

    const BIG: usize = usize::MAX;

    fn random_value(rng: &mut ChaCha8Rng, column: usize) -> Value {
        if rng.random_range(0..10) == 0 {
            return Value::Null;
        }
        match column {
            0 => Value::from_i64(rng.random_range(-20..20)),
            1 => Value::from_f64(
                f64::from(rng.random_range(-1000i32..1000)) / 8.0
                    + if rng.random_bool(0.3) { 1e15 } else { 0.0 },
            ),
            2 => {
                if rng.random_bool(0.5) {
                    Value::from_i64(rng.random_range(i64::MIN / 1024..i64::MAX / 1024))
                } else {
                    Value::from_f64(f64::from(rng.random_range(-8i32..8)))
                }
            }
            _ => Value::build_text(format!("t{}", rng.random_range(0..12))),
        }
    }

    fn random_table(rng: &mut ChaCha8Rng, rows: usize) -> Vec<Vec<Value>> {
        (0..rows)
            .map(|_| (0..4).map(|c| random_value(rng, c)).collect())
            .collect()
    }

    fn random_query(rng: &mut ChaCha8Rng) -> (Vec<Predicate>, Vec<usize>, Vec<Aggregate>) {
        let ops = [
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Gt,
            CmpOp::Ge,
        ];
        let filter = (0..rng.random_range(0..3))
            .map(|_| {
                let column = rng.random_range(0..4);
                match rng.random_range(0..4) {
                    0 => Predicate::IsNull { column },
                    1 => Predicate::NotNull { column },
                    _ => Predicate::Compare {
                        column,
                        op: ops[rng.random_range(0..ops.len())],
                        value: random_value(rng, column),
                    },
                }
            })
            .collect();
        let group_by = (0..rng.random_range(0..3))
            .map(|_| [0, 3, 2][rng.random_range(0..3)])
            .collect();
        let aggregates = (0..rng.random_range(1..5))
            .map(|_| match rng.random_range(0..5) {
                0 => Aggregate::CountStar,
                1 => Aggregate::Count(rng.random_range(0..4)),
                2 => Aggregate::Sum(rng.random_range(0..3)),
                3 => Aggregate::Min(rng.random_range(0..4)),
                _ => Aggregate::Max(rng.random_range(0..4)),
            })
            .collect();
        (filter, group_by, aggregates)
    }

    fn degree(workers: usize, morsel_rows: usize) -> Degree {
        Degree::Parallel {
            workers: NonZeroUsize::new(workers).unwrap(),
            morsel_rows: NonZeroUsize::new(morsel_rows).unwrap(),
        }
    }

    /// The same value, REALs by their bits.
    fn same(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| {
                x.len() == y.len()
                    && x.iter().zip(y).all(|(p, q)| match (p, q) {
                        (Value::Numeric(Numeric::Float(f)), Value::Numeric(Numeric::Float(g))) => {
                            f64::from(*f).to_bits() == f64::from(*g).to_bits()
                        }
                        (
                            Value::Numeric(Numeric::Integer(i)),
                            Value::Numeric(Numeric::Integer(j)),
                        ) => i == j,
                        (Value::Numeric(_), Value::Numeric(_)) => false,
                        _ => p == q,
                    })
            })
    }

    /// T-QT.6.1: random filters, groupings and aggregates over random tables; at 2, 3, 4 and 8
    /// workers and random morsel sizes the answer equals the serial one, value for value.
    #[test]
    fn t_qt_6_1_parallel_scan_aggregates_equal_serial() {
        let mut nonempty = 0;
        let mut multi_group = 0;
        let mut ran_parallel = 0;
        for seed in 0..300u64 {
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            let count = rng.random_range(0..600);
            let rows = random_table(&mut rng, count);
            let (filter, group_by, aggregates) = random_query(&mut rng);
            let pipeline = ScanAggregate {
                rows: &rows,
                filter: &filter,
                group_by: &group_by,
                aggregates: &aggregates,
            };
            let budget = MemoryBudget::new(BIG);
            let serial = execute_at(&pipeline, 0, Degree::Serial, None, &budget)
                .and_then(|(p, _)| pipeline.finish(p));
            for workers in [2, 3, 4, 8] {
                let morsel = rng.random_range(1..64);
                let budget = MemoryBudget::new(BIG);
                let parallel = execute_at(&pipeline, 0, degree(workers, morsel), None, &budget)
                    .and_then(|(p, stats)| {
                        if stats.workers > 1 {
                            ran_parallel += 1;
                        }
                        pipeline.finish(p)
                    });
                match (&serial, &parallel) {
                    (Ok(s), Ok(p)) => assert!(
                        same(s, p),
                        "seed {seed} at {workers} workers, morsels of {morsel}:\n{s:?}\n{p:?}"
                    ),
                    (Err(s), Err(p)) => assert_eq!(format!("{s}"), format!("{p}"), "seed {seed}"),
                    _ => panic!("seed {seed} at {workers} workers: {serial:?} vs {parallel:?}"),
                }
            }
            if let Ok(answer) = &serial {
                if !answer.is_empty() {
                    nonempty += 1;
                }
                if answer.len() > 1 {
                    multi_group += 1;
                }
            }
        }
        // Non-vacuity: most queries answer, many with several groups, and the runs were parallel.
        assert!(nonempty > 200, "{nonempty}");
        assert!(multi_group > 100, "{multi_group}");
        assert!(ran_parallel > 1000, "{ran_parallel}");
    }

    /// T-QT.6.2: random equi-joins on one or two key columns: the parallel probe's matches equal the
    /// serial probe's, in the same order, and a plain nested loop's set of matches.
    #[test]
    fn t_qt_6_2_parallel_hash_probe_equals_serial() {
        let mut matched = 0usize;
        for seed in 0..200u64 {
            let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5eed);
            let count = rng.random_range(0..200);
            let build = random_table(&mut rng, count);
            let count = rng.random_range(0..400);
            let probe = random_table(&mut rng, count);
            let keys: Vec<usize> = (0..rng.random_range(1..3))
                .map(|_| [0, 3, 2][rng.random_range(0..3)])
                .collect();
            let budget = MemoryBudget::new(BIG);
            let join = HashProbe::build(&build, &keys, &probe, &keys, &budget).unwrap();
            let (partial, _) = execute_at(&join, 0, Degree::Serial, None, &budget).unwrap();
            let serial = join.finish(partial).unwrap();
            let mut nested = Vec::new();
            for (p, prow) in probe.iter().enumerate() {
                for (b, brow) in build.iter().enumerate() {
                    if keys.iter().all(|k| {
                        !matches!(prow[*k], Value::Null)
                            && key_part(&prow[*k]) == key_part(&brow[*k])
                    }) {
                        nested.push((p, b));
                    }
                }
            }
            assert_eq!(serial, nested, "seed {seed}");
            matched += serial.len();
            for workers in [2, 4, 8] {
                let morsel = rng.random_range(1..50);
                let (partial, _) =
                    execute_at(&join, 0, degree(workers, morsel), None, &budget).unwrap();
                assert_eq!(
                    join.finish(partial).unwrap(),
                    serial,
                    "seed {seed} at {workers} workers"
                );
            }
        }
        assert!(matched > 10_000, "{matched}");
    }

    /// T-QT.6.3: a REAL sum's answer does not depend on the order of its terms.
    #[test]
    fn t_qt_6_3_exact_sum_is_order_free() {
        let terms = [1e100, 1.0, -1e100, 1e-30, 3.5, -2.25, 1e16, -1e16];
        let mut forward = ExactSum::default();
        terms.iter().for_each(|t| forward.add(*t).unwrap());
        let mut backward = ExactSum::default();
        terms.iter().rev().for_each(|t| backward.add(*t).unwrap());
        assert_eq!(forward.value().to_bits(), backward.value().to_bits());
        assert_eq!(forward.value(), 1.0 + 1e-30 + 3.5 - 2.25);
        let mut halves = ExactSum::default();
        terms[..3].iter().for_each(|t| halves.add(*t).unwrap());
        let mut other = ExactSum::default();
        terms[3..].iter().for_each(|t| other.add(*t).unwrap());
        halves.merge(&other).unwrap();
        assert_eq!(halves.value().to_bits(), forward.value().to_bits());
        let mut special = ExactSum::default();
        special.add(f64::INFINITY).unwrap();
        special.add(1.0).unwrap();
        assert_eq!(special.value(), f64::INFINITY);
    }

    /// T-QT.6.4: a statement whose workers together pass its memory budget is refused with the typed
    /// error at every worker count, and the budget's use never passes its limit.
    #[test]
    fn t_qt_6_4_memory_budget_refuses() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let rows = random_table(&mut rng, 2_000);
        let group_by = [0, 3];
        let aggregates = [Aggregate::CountStar];
        let pipeline = ScanAggregate {
            rows: &rows,
            filter: &[],
            group_by: &group_by,
            aggregates: &aggregates,
        };
        let limit = pipeline.group_bytes() * 10;
        for workers in [1, 2, 4, 8] {
            let budget = MemoryBudget::new(limit);
            let result = execute_at(&pipeline, 0, degree(workers, 16), None, &budget);
            assert!(
                matches!(result, Err(MorselError::MemoryBudget { .. })),
                "{workers}: {result:?}"
            );
            assert!(budget.used() <= limit);
        }
    }

    /// T-QT.6.5: the degree comes from the costs: off is serial, a short statement is serial, a long
    /// cheap-to-start one uses more than one worker and no more than the cores, and its morsels
    /// grow as the dispatch cost grows against the cost of a row.
    #[test]
    fn t_qt_6_5_degree_from_measured_costs() {
        let cores = NonZeroUsize::new(16).unwrap();
        let costs = MachineCosts::stated(cores, 50_000.0, 20.0);
        assert_eq!(
            choose_degree(ParallelMode::Off, 10_000_000, 50.0, &costs),
            Degree::Serial
        );
        assert_eq!(
            choose_degree(ParallelMode::On, 100, 50.0, &costs),
            Degree::Serial
        );
        let Degree::Parallel {
            workers,
            morsel_rows,
        } = choose_degree(ParallelMode::On, 10_000_000, 50.0, &costs)
        else {
            panic!("serial")
        };
        assert!(workers.get() > 1 && workers <= cores);
        let dearer = MachineCosts::stated(cores, 50_000.0, 2_000.0);
        let Degree::Parallel {
            morsel_rows: larger,
            ..
        } = choose_degree(ParallelMode::On, 10_000_000, 50.0, &dearer)
        else {
            panic!("serial")
        };
        assert!(larger > morsel_rows, "{larger} vs {morsel_rows}");
        let one = MachineCosts::stated(NonZeroUsize::new(1).unwrap(), 1.0, 1.0);
        assert_eq!(
            choose_degree(ParallelMode::On, 10_000_000, 50.0, &one),
            Degree::Serial
        );
    }

    /// T-QT.6.6: the whole path, pilot and measured machine included: the answer equals the serial
    /// one, and the machine's costs learn from the run.
    #[test]
    fn t_qt_6_6_execute_with_measured_costs_equals_serial() {
        let mut costs = MachineCosts::measure().unwrap();
        let mut rng = ChaCha8Rng::seed_from_u64(11);
        let rows = random_table(&mut rng, 50_000);
        let group_by = [3];
        let aggregates = [
            Aggregate::CountStar,
            Aggregate::Sum(1),
            Aggregate::Min(2),
            Aggregate::Max(0),
        ];
        let pipeline = ScanAggregate {
            rows: &rows,
            filter: &[],
            group_by: &group_by,
            aggregates: &aggregates,
        };
        let budget = MemoryBudget::new(BIG);
        let (serial, _) = execute(&pipeline, ParallelMode::Off, &mut costs, &budget).unwrap();
        let serial = pipeline.finish(serial).unwrap();
        let (parallel, stats) = execute(&pipeline, ParallelMode::On, &mut costs, &budget).unwrap();
        assert!(same(&serial, &pipeline.finish(parallel).unwrap()));
        assert!(stats.pilot_rows >= 1 && stats.workers <= costs.cores().get());
    }
}
