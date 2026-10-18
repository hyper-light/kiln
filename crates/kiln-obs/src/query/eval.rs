//! Evaluating a query over a store (`docs/observe.md` §6).
//!
//! Datadog's model, step by step: a metric query selects series; each series is rolled up into
//! the buckets of a time grid (its kind decides the default: a gauge averages, a count sums, a
//! cumulative counter's increases are summed, resets found); series are combined into groups by
//! their `by` tags, a bucket at a time (the space aggregation); then fills, functions and
//! arithmetic act on whole series, matched by their group tags. Evaluation visits the arena's
//! nodes in order, children before parents: no recursion.

use std::collections::HashMap;

use super::parse::{Ast, BinOp, CountMode, Fill, Func, MetricQuery, Node, ParseError, Rollup, SpaceAgg, parse};
use crate::anomaly::{Algorithm, Scorer};
use crate::batch::Kind;
use crate::index::Selector;
use crate::store::{Scratch, Store, StoreError};

/// The most buckets a series: Prometheus's cap on a range query's resolution (11,000 points a
/// series, `promql` engine, "exceeded maximum resolution of 11,000 points per timeseries").
pub const MAX_POINTS: usize = 11_000;

/// The points a series the grid aims for when no step is asked: a wide chart's pixels (a 4K
/// display is 3,840 across; a chart is at most about a third of it), so a series never carries
/// more points than it can draw.
pub const DEFAULT_POINTS: usize = 1_200;

/// The most series one answer carries, unless the request says otherwise: a dashboard widget that
/// draws more is unreadable, and the bound keeps an answer's memory at most 10,000 × 11,000 × 8
/// bytes.
pub const DEFAULT_MAX_SERIES: usize = 10_000;

/// Bucket widths a grid uses, in seconds: the "nice" intervals Datadog and Grafana step through, so a
/// graph's buckets stay put as its range moves.
const STEPS: [i64; 24] = [1, 2, 5, 10, 15, 20, 30, 60, 120, 300, 600, 900, 1200, 1800, 3600, 7200, 10_800, 14_400, 21_600, 43_200, 86_400, 172_800, 604_800, 2_592_000];

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the range must end after it starts")]
    EmptyRange,
    #[error("{0} points a series (at most {MAX_POINTS}): a coarser step, or a shorter range")]
    TooManyPoints(usize),
    #[error("{0} series (at most {1}): a narrower filter, or top()")]
    TooManySeries(usize, usize),
    #[error("{0}")]
    Invalid(String),
}

/// What to evaluate.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub query: &'a str,
    /// Milliseconds since the epoch.
    pub from: i64,
    pub to: i64,
    /// Bucket width in milliseconds; none: from the range and `points`.
    pub step: Option<i64>,
    pub points: usize,
    pub max_series: usize,
}

impl<'a> Request<'a> {
    pub fn new(query: &'a str, from: i64, to: i64) -> Self {
        Self { query, from, to, step: None, points: DEFAULT_POINTS, max_series: DEFAULT_MAX_SERIES }
    }
}

/// A time grid: `n` buckets of `step` milliseconds from `from`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub from: i64,
    pub step: i64,
    pub n: usize,
}

impl Grid {
    pub fn time(&self, i: usize) -> i64 {
        self.from + self.step * i as i64
    }
}

/// History `anomalies()`'s query reads before the range: two weeks, two cycles of a weekly season
/// (the scorer keeps a period only with two of it to learn from), at most
/// [`crate::anomaly::HISTORY`] buckets in all.
const ANOMALY_HISTORY_MS: i64 = 14 * 86_400_000;

/// One series of an answer: its group's tags and a value a bucket (NaN where there is none); an
/// `anomalies()` series' band besides.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub tags: Vec<(String, String)>,
    pub values: Vec<f64>,
    pub bounds: Option<Box<Bounds>>,
}

/// Which side of a band is anomalous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Both,
    Above,
    Below,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().trim_matches(['\'', '"']).to_ascii_lowercase().as_str() {
            "both" => Some(Self::Both),
            "above" => Some(Self::Above),
            "below" => Some(Self::Below),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Above => "above",
            Self::Below => "below",
        }
    }
}

/// An `anomalies()` series' band: each bucket's bounds, its score (its error in calibrated
/// deviations, signed), and what made them.
#[derive(Debug, Clone, PartialEq)]
pub struct Bounds {
    pub lower: Vec<f64>,
    pub upper: Vec<f64>,
    pub score: Vec<f64>,
    pub k: f64,
    pub direction: Direction,
    pub algorithm: Algorithm,
}

impl Bounds {
    /// Whether bucket `i` is past the band on the side asked.
    pub fn anomalous(&self, i: usize) -> Option<bool> {
        let s = self.score.get(i).copied().filter(|s| !s.is_nan())?;
        Some(match self.direction {
            Direction::Both => s.abs() > self.k,
            Direction::Above => s > self.k,
            Direction::Below => s < -self.k,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub grid: Grid,
    pub series: Vec<Series>,
    /// A plain number, when the query is one.
    pub scalar: Option<f64>,
    /// Series read and samples decoded.
    pub scanned_series: u64,
    pub scanned_samples: u64,
}

#[derive(Debug, Clone)]
enum Value {
    Scalar(f64),
    Str(String),
    Set(Vec<Series>),
}

/// Evaluates a query.
pub fn eval(store: &Store, req: &Request<'_>) -> Result<Answer, QueryError> {
    let ast = parse(req.query)?;
    if req.to <= req.from {
        return Err(QueryError::EmptyRange);
    }
    let grid = grid_for(&ast, req)?;
    let offsets = offsets(&ast)?;
    let ext = extensions(&ast, grid);
    // A node's grid: the answer's, begun `ext` buckets earlier beneath `anomalies()`.
    let at = |e: usize| Grid { from: grid.from - grid.step * e as i64, step: grid.step, n: grid.n + e };
    let mut values: Vec<Option<Value>> = vec![None; ast.nodes.len()];
    let mut scanned = (0u64, 0u64);
    let mut scratch = Scratch::default();
    for (id, node) in ast.nodes.iter().enumerate() {
        let take = |values: &mut Vec<Option<Value>>, i: usize| values.get_mut(i).and_then(Option::take).ok_or_else(|| QueryError::Invalid("an operand used twice".into()));
        let v = match node {
            Node::Num(n) => Value::Scalar(*n),
            Node::Str(s) => Value::Str(s.clone()),
            Node::Metric(q) => {
                let off = offsets.get(id).copied().unwrap_or(0);
                Value::Set(metric(store, q, at(ext.get(id).copied().unwrap_or(0)), off, req.max_series, &mut scratch, &mut scanned)?)
            }
            Node::Neg(x) => match take(&mut values, *x)? {
                Value::Scalar(a) => Value::Scalar(-a),
                Value::Set(mut s) => {
                    for x in &mut s {
                        for v in &mut x.values {
                            *v = -*v;
                        }
                    }
                    Value::Set(s)
                }
                Value::Str(_) => return Err(QueryError::Invalid("a string has no sign".into())),
            },
            Node::Bin(op, l, r) => {
                let (a, b) = (take(&mut values, *l)?, take(&mut values, *r)?);
                binary(*op, a, b)?
            }
            Node::Call(f, args) => {
                let mut taken = Vec::with_capacity(args.len());
                for &a in args {
                    taken.push(take(&mut values, a)?);
                }
                let own = ext.get(id).copied().unwrap_or(0);
                if *f == Func::Anomalies {
                    let inner = args.first().and_then(|&a| ext.get(a)).copied().unwrap_or(own);
                    anomalies(taken, inner.saturating_sub(own), at(own))?
                } else {
                    call(*f, taken, at(own))?
                }
            }
        };
        if let Some(slot) = values.get_mut(id) {
            *slot = Some(v);
        }
    }
    let root = values.get_mut(ast.root).and_then(Option::take).ok_or_else(|| QueryError::Invalid("no result".into()))?;
    let (series, scalar) = match root {
        Value::Set(s) => (s, None),
        Value::Scalar(x) => (Vec::new(), Some(x)),
        Value::Str(_) => return Err(QueryError::Invalid("a query's result is a string".into())),
    };
    if series.len() > req.max_series {
        return Err(QueryError::TooManySeries(series.len(), req.max_series));
    }
    Ok(Answer { grid, series, scalar, scanned_series: scanned.0, scanned_samples: scanned.1 })
}

/// The grid: the step asked, or the smallest nice step that fits the range in the points asked,
/// and never finer than a rollup the query names; `from` aligned down to it.
fn grid_for(ast: &Ast, req: &Request<'_>) -> Result<Grid, QueryError> {
    let range = req.to.saturating_sub(req.from);
    let rollup_ms = ast
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::Metric(m) => m.rollup.and_then(|r| r.1).map(|s| s.saturating_mul(1000)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let step = match req.step {
        Some(s) if s > 0 => s.max(rollup_ms),
        _ => {
            let raw = range / (req.points.max(1) as i64);
            let nice = STEPS.iter().map(|s| s * 1000).find(|&s| s >= raw).unwrap_or(raw.max(1000));
            nice.max(rollup_ms).max(1000)
        }
    };
    let from = req.from.div_euclid(step) * step;
    let n = usize::try_from((req.to - from + step - 1) / step).unwrap_or(usize::MAX);
    if n > MAX_POINTS {
        return Err(QueryError::TooManyPoints(n));
    }
    Ok(Grid { from, step, n })
}

/// Each node's time offset in milliseconds: a `timeshift`'s, added to everything beneath it.
/// Parents come after their children in the arena, so one pass from the end visits a parent
/// before its children.
fn offsets(ast: &Ast) -> Result<Vec<i64>, QueryError> {
    let mut off = vec![0i64; ast.nodes.len()];
    for id in (0..ast.nodes.len()).rev() {
        let here = off.get(id).copied().unwrap_or(0);
        let (children, shift): (&[usize], i64) = match ast.nodes.get(id) {
            Some(Node::Neg(x)) => (std::slice::from_ref(x), 0),
            Some(Node::Bin(_, l, r)) => {
                if let Some(o) = off.get_mut(*l) {
                    *o = here;
                }
                (std::slice::from_ref(r), 0)
            }
            Some(Node::Call(Func::Timeshift, args)) => {
                let secs = args.get(1).and_then(|&a| constant(ast, a)).ok_or_else(|| QueryError::Invalid("timeshift(query, seconds): the seconds a number".into()))?;
                (args.get(..1).unwrap_or(&[]), (secs * 1000.0) as i64)
            }
            Some(Node::Call(_, args)) => (args.as_slice(), 0),
            _ => (&[], 0),
        };
        for &c in children {
            if let Some(o) = off.get_mut(c) {
                *o = here.saturating_add(shift);
            }
        }
    }
    Ok(off)
}

/// Each node's history in buckets: beneath an `anomalies()`, the buckets of
/// [`ANOMALY_HISTORY_MS`] its query reads before the range (all the answer's grid allows), as
/// `offsets` passes a time shift down.
fn extensions(ast: &Ast, grid: Grid) -> Vec<usize> {
    let step = grid.step.max(1);
    let history = usize::try_from(ANOMALY_HISTORY_MS / step).unwrap_or(0).min(crate::anomaly::HISTORY.saturating_sub(grid.n)).min(MAX_POINTS.saturating_sub(grid.n));
    let mut ext = vec![0usize; ast.nodes.len()];
    for id in (0..ast.nodes.len()).rev() {
        let here = ext.get(id).copied().unwrap_or(0);
        let (children, more): (&[usize], usize) = match ast.nodes.get(id) {
            Some(Node::Neg(x)) => (std::slice::from_ref(x), 0),
            Some(Node::Bin(_, l, r)) => {
                if let Some(e) = ext.get_mut(*l) {
                    *e = here;
                }
                (std::slice::from_ref(r), 0)
            }
            Some(Node::Call(Func::Anomalies, args)) => (args.get(..1).unwrap_or(&[]), history),
            Some(Node::Call(_, args)) => (args.as_slice(), 0),
            _ => (&[], 0),
        };
        for &c in children {
            if let Some(e) = ext.get_mut(c) {
                *e = here + more;
            }
        }
    }
    ext
}

/// `anomalies(query, algorithm, bounds, direction)`: each series' last `grid.n` buckets, and its band
/// of `bounds` deviations (2 unless said) about what the algorithm (`basic` unless said) expected of
/// each before it saw it, learned from the `history` buckets before them; anomalous past the band on
/// the side asked (`both` unless said).
fn anomalies(mut args: Vec<Value>, history: usize, grid: Grid) -> Result<Value, QueryError> {
    let usage = || QueryError::Invalid("anomalies(query, 'basic'|'agile'|'robust', bounds, 'both'|'above'|'below')".into());
    if args.is_empty() {
        return Err(usage());
    }
    let set = match args.remove(0) {
        Value::Set(s) => s,
        _ => return Err(usage()),
    };
    let algorithm = match args.first() {
        Some(Value::Str(s)) => Algorithm::parse(s).ok_or_else(usage)?,
        None => Algorithm::Basic,
        _ => return Err(usage()),
    };
    let k = match args.get(1) {
        Some(Value::Scalar(x)) if *x > 0.0 && x.is_finite() => *x,
        None => 2.0,
        _ => return Err(usage()),
    };
    let direction = match args.get(2) {
        Some(Value::Str(s)) => Direction::parse(s).ok_or_else(usage)?,
        None => Direction::Both,
        _ => return Err(usage()),
    };
    let mut out = Vec::with_capacity(set.len());
    for s in set {
        let split = history.min(s.values.len());
        let (past, now) = s.values.split_at(split);
        // Learned from the finite points of the history, gaps left out; their spacing their mean gap
        // (a series sampled every 5 minutes is 5 minutes apart on a grid of 1, or of 2, where its
        // gaps are 2 and 3 buckets by turns).
        let at: Vec<usize> = past.iter().enumerate().filter(|(_, v)| v.is_finite()).map(|(i, _)| i).collect();
        let learn: Vec<f64> = at.iter().filter_map(|&i| past.get(i).copied()).collect();
        let span = match (at.first(), at.last()) {
            (Some(&a), Some(&b)) if at.len() > 1 => (b - a) as f64 / (at.len() - 1) as f64,
            _ => 1.0,
        };
        let spacing = span * grid.step as f64 / 1000.0;
        let mut scorer = Scorer::new(algorithm, &learn, spacing);
        let n = now.len();
        let (mut lower, mut upper, mut score) = (vec![f64::NAN; n], vec![f64::NAN; n], vec![f64::NAN; n]);
        if let Some(sc) = &mut scorer {
            for (i, &x) in now.iter().enumerate() {
                let Some((expected, deviation)) = sc.expect() else { continue };
                if let (Some(l), Some(u)) = (lower.get_mut(i), upper.get_mut(i)) {
                    *l = expected - k * deviation;
                    *u = expected + k * deviation;
                }
                if let (Some(p), Some(sl)) = (sc.step(x), score.get_mut(i)) {
                    *sl = p.score;
                }
            }
        }
        out.push(Series { tags: s.tags, values: now.to_vec(), bounds: Some(Box::new(Bounds { lower, upper, score, k, direction, algorithm })) });
    }
    Ok(Value::Set(out))
}

/// A node's value when it is a constant: a number, or a signed one.
fn constant(ast: &Ast, id: usize) -> Option<f64> {
    match ast.nodes.get(id)? {
        Node::Num(n) => Some(*n),
        Node::Neg(x) => match ast.nodes.get(*x)? {
            Node::Num(n) => Some(-*n),
            _ => None,
        },
        _ => None,
    }
}

/// A metric query's series on the grid.
fn metric(store: &Store, q: &MetricQuery, grid: Grid, off: i64, max_series: usize, scratch: &mut Scratch, scanned: &mut (u64, u64)) -> Result<Vec<Series>, QueryError> {
    let mut ids = Vec::new();
    store.select(&Selector { metric: q.metric.clone(), clauses: q.filters.clone() }, &mut ids);
    // In the order of the series' fingerprints, the same on every replica (ids are each one's
    // own), so a group's values are summed in one order everywhere: floating-point addition does
    // not associate, and every replica answers to the bit.
    ids.sort_unstable_by_key(|&id| store.fingerprint(id).unwrap_or(0));
    // Groups: by-tag values → index; the key built in one buffer, copied only for a new group.
    let mut groups: HashMap<Vec<&str>, usize> = HashMap::new();
    let mut key: Vec<&str> = Vec::with_capacity(q.by.len());
    let mut group_tags: Vec<Vec<(String, String)>> = Vec::new();
    let mut acc: Vec<Acc> = Vec::new();
    let (mut ts, mut vs) = (Vec::new(), Vec::new());
    let mut buckets = vec![f64::NAN; grid.n];
    let from = grid.from.saturating_add(off);
    let end = from.saturating_add(grid.step.saturating_mul(grid.n as i64));
    for &id in &ids {
        let Some(s) = store.series(id) else { continue };
        key.clear();
        key.extend(q.by.iter().map(|k| s.tag(k).unwrap_or("N/A")));
        let g = match groups.get(&key) {
            Some(&g) => g,
            None => {
                let g = acc.len();
                if g >= max_series {
                    return Err(QueryError::TooManySeries(g + 1, max_series));
                }
                group_tags.push(q.by.iter().zip(&key).map(|(k, v)| (k.clone(), (*v).to_owned())).collect());
                acc.push(Acc::new(q.space, grid.n));
                groups.insert(key.clone(), g);
                g
            }
        };
        ts.clear();
        vs.clear();
        // A counter's increase into the first bucket needs the sample before it.
        let lookback = if s.kind() == Kind::Counter { grid.step } else { 0 };
        store.read_with(id, from.saturating_sub(lookback), end.saturating_sub(1), scratch, &mut ts, &mut vs)?;
        scanned.0 += 1;
        scanned.1 += ts.len() as u64;
        rollup(s.kind(), q, grid, from, &ts, &vs, &mut buckets);
        if let Some(a) = acc.get_mut(g) {
            a.add(&buckets);
        }
    }
    let mut out = Vec::with_capacity(acc.len());
    for (tags, a) in group_tags.into_iter().zip(acc) {
        let mut values = a.finish();
        if let Some(fill) = q.fill {
            apply_fill(fill, grid, &mut values);
        }
        out.push(Series { tags, values, bounds: None });
    }
    out.sort_by(|a, b| a.tags.cmp(&b.tags));
    Ok(out)
}

/// One series' samples into `buckets` (NaN where none): its rollup function, by its kind unless
/// the query names one; for counts and counters, a total a bucket or a rate a second.
fn rollup(kind: Kind, q: &MetricQuery, grid: Grid, from: i64, ts: &[i64], vs: &[f64], buckets: &mut [f64]) {
    for b in buckets.iter_mut() {
        *b = f64::NAN;
    }
    let step_s = grid.step as f64 / 1000.0;
    let bucket_of = |t: i64| -> Option<usize> {
        if t < from {
            return None;
        }
        usize::try_from((t - from) / grid.step).ok().filter(|&i| i < grid.n)
    };
    match kind {
        Kind::Counter => {
            // Increases between consecutive samples, each in the later one's bucket; a fall is a
            // reset, and the increase is the new value (Prometheus's counter semantics).
            let mut prev: Option<f64> = None;
            for (&t, &v) in ts.iter().zip(vs) {
                if let (Some(p), Some(i)) = (prev, bucket_of(t))
                    && let Some(b) = buckets.get_mut(i)
                {
                    let inc = if v >= p { v - p } else { v };
                    *b = if b.is_nan() { inc } else { *b + inc };
                }
                if v.is_finite() {
                    prev = Some(v);
                }
            }
            if q.mode != Some(CountMode::Count) {
                for b in buckets.iter_mut() {
                    *b /= step_s;
                }
            }
        }
        _ => {
            let f = q.rollup.map(|r| r.0).unwrap_or(match kind {
                Kind::Count => Rollup::Sum,
                _ => Rollup::Avg,
            });
            let mut i = 0usize;
            while let Some(&t) = ts.get(i) {
                let Some(b) = bucket_of(t) else {
                    i += 1;
                    continue;
                };
                // The run of samples in bucket b.
                let start = i;
                while ts.get(i).is_some_and(|&t2| bucket_of(t2) == Some(b)) {
                    i += 1;
                }
                let run = vs.get(start..i).unwrap_or(&[]);
                let finite = run.iter().copied().filter(|v| v.is_finite());
                let v = match f {
                    Rollup::Avg => {
                        let (s, n) = finite.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
                        if n == 0 { f64::NAN } else { s / f64::from(n) }
                    }
                    Rollup::Sum => finite.fold(f64::NAN, |a, v| if a.is_nan() { v } else { a + v }),
                    Rollup::Min => finite.fold(f64::NAN, |a, v| if a.is_nan() { v } else { a.min(v) }),
                    Rollup::Max => finite.fold(f64::NAN, |a, v| if a.is_nan() { v } else { a.max(v) }),
                    Rollup::Count => finite.count() as f64,
                    Rollup::Last => run.iter().rev().copied().find(|v| v.is_finite()).unwrap_or(f64::NAN),
                };
                if let Some(slot) = buckets.get_mut(b) {
                    *slot = v;
                }
            }
            if kind == Kind::Count && q.mode == Some(CountMode::Rate) {
                for b in buckets.iter_mut() {
                    *b /= step_s;
                }
            }
        }
    }
}

/// A group's space aggregation, a bucket at a time.
enum Acc {
    /// Sum and count for avg, sum and count; min and max as their own.
    Moments { kind: SpaceAgg, a: Vec<f64>, n: Vec<u32> },
    /// Every series' value a bucket, as (bucket, value), for a percentile.
    Values { pct: f64, n: usize, v: Vec<(u32, f64)> },
}

impl Acc {
    fn new(kind: SpaceAgg, n: usize) -> Self {
        match kind {
            SpaceAgg::Pct(p) => Self::Values { pct: p, n, v: Vec::new() },
            k => Self::Moments { kind: k, a: vec![f64::NAN; n], n: vec![0; n] },
        }
    }

    fn add(&mut self, buckets: &[f64]) {
        match self {
            Self::Moments { kind, a, n } => {
                for ((x, c), &v) in a.iter_mut().zip(n.iter_mut()).zip(buckets) {
                    if v.is_nan() {
                        continue;
                    }
                    *c += 1;
                    *x = match kind {
                        SpaceAgg::Min => {
                            if x.is_nan() {
                                v
                            } else {
                                x.min(v)
                            }
                        }
                        SpaceAgg::Max => {
                            if x.is_nan() {
                                v
                            } else {
                                x.max(v)
                            }
                        }
                        _ => {
                            if x.is_nan() {
                                v
                            } else {
                                *x + v
                            }
                        }
                    };
                }
            }
            Self::Values { v, .. } => {
                for (i, &x) in buckets.iter().enumerate() {
                    if !x.is_nan() {
                        v.push((i as u32, x));
                    }
                }
            }
        }
    }

    fn finish(self) -> Vec<f64> {
        match self {
            Self::Moments { kind, a, n } => a
                .into_iter()
                .zip(n)
                .map(|(x, c)| match kind {
                    SpaceAgg::Avg if c > 0 => x / f64::from(c),
                    SpaceAgg::Count => {
                        if c > 0 {
                            f64::from(c)
                        } else {
                            f64::NAN
                        }
                    }
                    _ => x,
                })
                .collect(),
            Self::Values { pct, n, mut v } => {
                v.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
                let mut out = vec![f64::NAN; n];
                let mut i = 0;
                while let Some(&(b, _)) = v.get(i) {
                    let start = i;
                    while v.get(i).is_some_and(|x| x.0 == b) {
                        i += 1;
                    }
                    if let (Some(slot), Some(run)) = (out.get_mut(b as usize), v.get(start..i)) {
                        *slot = percentile_sorted(run.iter().map(|x| x.1), run.len(), pct);
                    }
                }
                out
            }
        }
    }
}

/// The `p`th percentile (0–100) by linear interpolation between order statistics (Hyndman and Fan
/// 1996, type 7: R's and NumPy's default); NaN for none.
pub fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    percentile_sorted(values.iter().copied(), values.len(), p)
}

/// [`percentile`] of `n` values already sorted.
fn percentile_sorted(mut sorted: impl Iterator<Item = f64>, n: usize, p: f64) -> f64 {
    if n == 0 {
        return f64::NAN;
    }
    let h = (n - 1) as f64 * (p / 100.0).clamp(0.0, 1.0);
    let lo = h.floor() as usize;
    let a = sorted.nth(lo).unwrap_or(f64::NAN);
    if h.fract() == 0.0 {
        return a;
    }
    let b = sorted.next().unwrap_or(a);
    a + (b - a) * (h - lo as f64)
}

fn apply_fill(fill: Fill, grid: Grid, v: &mut [f64]) {
    let limit_buckets = |limit: Option<i64>| limit.map_or(usize::MAX, |s| usize::try_from(s.saturating_mul(1000) / grid.step.max(1)).unwrap_or(usize::MAX));
    match fill {
        Fill::Null => {}
        Fill::Zero => {
            for x in v.iter_mut() {
                if x.is_nan() {
                    *x = 0.0;
                }
            }
        }
        Fill::Last(limit) => {
            let max = limit_buckets(limit);
            let mut last: Option<(usize, f64)> = None;
            for (i, x) in v.iter_mut().enumerate() {
                if x.is_nan() {
                    if let Some((at, l)) = last
                        && i - at <= max
                    {
                        *x = l;
                    }
                } else {
                    last = Some((i, *x));
                }
            }
        }
        Fill::Linear(limit) => {
            let max = limit_buckets(limit);
            let mut prev: Option<(usize, f64)> = None;
            let mut i = 0;
            while i < v.len() {
                if v.get(i).is_some_and(|x| x.is_nan()) {
                    let gap_start = i;
                    while v.get(i).is_some_and(|x| x.is_nan()) {
                        i += 1;
                    }
                    if let (Some((a, va)), Some(&vb)) = (prev, v.get(i))
                        && i - a <= max
                    {
                        let span = (i - a) as f64;
                        for j in gap_start..i {
                            if let Some(x) = v.get_mut(j) {
                                *x = va + (vb - va) * ((j - a) as f64 / span);
                            }
                        }
                    }
                } else {
                    prev = v.get(i).map(|&x| (i, x));
                    i += 1;
                }
            }
        }
    }
}

fn binary(op: BinOp, a: Value, b: Value) -> Result<Value, QueryError> {
    let f = |x: f64, y: f64| match op {
        BinOp::Add => x + y,
        BinOp::Sub => x - y,
        BinOp::Mul => x * y,
        BinOp::Div => {
            if y == 0.0 {
                f64::NAN
            } else {
                x / y
            }
        }
    };
    Ok(match (a, b) {
        (Value::Scalar(x), Value::Scalar(y)) => Value::Scalar(f(x, y)),
        (Value::Scalar(x), Value::Set(mut s)) => {
            for sr in &mut s {
                for v in &mut sr.values {
                    *v = f(x, *v);
                }
            }
            Value::Set(s)
        }
        (Value::Set(mut s), Value::Scalar(y)) => {
            for sr in &mut s {
                for v in &mut sr.values {
                    *v = f(*v, y);
                }
            }
            Value::Set(s)
        }
        (Value::Set(l), Value::Set(r)) => {
            // One side a single ungrouped series: broadcast it. Else matched by group tags.
            let lone = |s: &Vec<Series>| matches!(s.as_slice(), [one] if one.tags.is_empty());
            let mut out = Vec::new();
            if lone(&r) {
                let rv = r.into_iter().next().map(|s| s.values).unwrap_or_default();
                for mut s in l {
                    for (v, w) in s.values.iter_mut().zip(&rv) {
                        *v = f(*v, *w);
                    }
                    out.push(s);
                }
            } else if lone(&l) {
                let lv = l.into_iter().next().map(|s| s.values).unwrap_or_default();
                for mut s in r {
                    for (v, w) in s.values.iter_mut().zip(&lv) {
                        *v = f(*w, *v);
                    }
                    out.push(s);
                }
            } else {
                let index: HashMap<&Vec<(String, String)>, usize> = r.iter().enumerate().map(|(i, s)| (&s.tags, i)).collect();
                for mut s in l.clone() {
                    if let Some(rs) = index.get(&s.tags).and_then(|&i| r.get(i)) {
                        for (v, w) in s.values.iter_mut().zip(&rs.values) {
                            *v = f(*v, *w);
                        }
                        out.push(s);
                    }
                }
            }
            Value::Set(out)
        }
        _ => return Err(QueryError::Invalid("arithmetic on a string".into())),
    })
}

fn call(f: Func, mut args: Vec<Value>, grid: Grid) -> Result<Value, QueryError> {
    let step_s = grid.step as f64 / 1000.0;
    let num = |v: Option<&Value>, what: &str| -> Result<f64, QueryError> {
        match v {
            Some(Value::Scalar(x)) => Ok(*x),
            _ => Err(QueryError::Invalid(format!("{f:?}: {what} must be a number"))),
        }
    };
    let text = |v: Option<&Value>, default: &str| -> String {
        match v {
            Some(Value::Str(s)) => s.to_ascii_lowercase(),
            _ => default.to_owned(),
        }
    };
    // The series argument, first.
    let first = if args.is_empty() { None } else { Some(args.remove(0)) };
    let mut set = match first {
        Some(Value::Set(s)) => s,
        Some(Value::Scalar(x)) => return Ok(Value::Scalar(pointwise(f, x))),
        _ => return Err(QueryError::Invalid(format!("{f:?}: a query first"))),
    };
    match f {
        Func::Abs | Func::Log2 | Func::Log10 => {
            for s in &mut set {
                for v in &mut s.values {
                    *v = pointwise(f, *v);
                }
            }
        }
        Func::Diff | Func::Derivative | Func::Rate => {
            let per = if f == Func::Diff { 1.0 } else { step_s };
            for s in &mut set {
                let mut prev = f64::NAN;
                for v in &mut s.values {
                    let cur = *v;
                    *v = (cur - prev) / per;
                    if !cur.is_nan() {
                        prev = cur;
                    }
                }
            }
        }
        Func::PerSecond | Func::PerMinute | Func::PerHour => {
            let unit = match f {
                Func::PerSecond => 1.0,
                Func::PerMinute => 60.0,
                _ => 3600.0,
            };
            for s in &mut set {
                for v in &mut s.values {
                    *v = *v / step_s * unit;
                }
            }
        }
        Func::Cumsum | Func::Integral => {
            let k = if f == Func::Integral { step_s } else { 1.0 };
            for s in &mut set {
                let mut sum = 0.0;
                for v in &mut s.values {
                    if !v.is_nan() {
                        sum += *v * k;
                        *v = sum;
                    }
                }
            }
        }
        Func::Top | Func::Bottom => {
            let n = match args.first() {
                Some(v) => num(Some(v), "the count")?.max(0.0) as usize,
                None => 10,
            };
            let reducer = text(args.get(1), "mean");
            let desc = text(args.get(2), if f == Func::Top { "desc" } else { "asc" }) == "desc";
            let score = |s: &Series| -> f64 {
                let vals = s.values.iter().copied().filter(|v| !v.is_nan());
                match reducer.as_str() {
                    "max" => vals.fold(f64::NEG_INFINITY, f64::max),
                    "min" => vals.fold(f64::INFINITY, f64::min),
                    "last" => s.values.iter().rev().copied().find(|v| !v.is_nan()).unwrap_or(f64::NAN),
                    "sum" => vals.sum(),
                    "area" => vals.map(f64::abs).sum(),
                    "norm" => vals.map(|v| v * v).sum::<f64>().sqrt(),
                    _ => {
                        let (s, c) = vals.fold((0.0, 0u32), |(s, c), v| (s + v, c + 1));
                        if c == 0 { f64::NAN } else { s / f64::from(c) }
                    }
                }
            };
            let mut scored: Vec<(f64, Series)> = set.into_iter().map(|s| (score(&s), s)).collect();
            scored.sort_by(|a, b| {
                let o = a.0.total_cmp(&b.0);
                if desc { o.reverse() } else { o }
            });
            set = scored.into_iter().filter(|(s, _)| !s.is_nan()).take(n).map(|(_, s)| s).collect();
        }
        // Evaluated apart (`anomalies`): it reads its query's history.
        Func::Timeshift | Func::Anomalies => {}
        Func::MovingAvg => {
            let secs = num(args.first(), "the window's seconds")?;
            let k = ((secs * 1000.0 / grid.step as f64).round() as usize).max(1);
            for s in &mut set {
                let src = s.values.clone();
                for (i, v) in s.values.iter_mut().enumerate() {
                    let win = src.get(i.saturating_sub(k - 1)..=i).unwrap_or(&[]);
                    let (sum, c) = win.iter().filter(|x| !x.is_nan()).fold((0.0, 0u32), |(s, c), x| (s + x, c + 1));
                    *v = if c == 0 { f64::NAN } else { sum / f64::from(c) };
                }
            }
        }
        Func::Ewma => {
            // α = 2 / (span + 1), the span convention (Hunter, "The exponentially weighted moving
            // average", J. Quality Technology 1986; pandas' `span`).
            let span = num(args.first(), "the span")?;
            if span < 1.0 {
                return Err(QueryError::Invalid("ewma: a span of at least 1".into()));
            }
            let alpha = 2.0 / (span + 1.0);
            for s in &mut set {
                let mut m = f64::NAN;
                for v in &mut s.values {
                    if !v.is_nan() {
                        m = if m.is_nan() { *v } else { alpha * *v + (1.0 - alpha) * m };
                        *v = m;
                    }
                }
            }
        }
        Func::Clamp => {
            let lo = num(args.first(), "the lower bound")?;
            let hi = num(args.get(1), "the upper bound")?;
            for s in &mut set {
                for v in &mut s.values {
                    if !v.is_nan() {
                        *v = v.clamp(lo.min(hi), hi.max(lo));
                    }
                }
            }
        }
    }
    Ok(Value::Set(set))
}

fn pointwise(f: Func, x: f64) -> f64 {
    match f {
        Func::Abs => x.abs(),
        Func::Log2 => {
            if x > 0.0 {
                x.log2()
            } else {
                f64::NAN
            }
        }
        Func::Log10 => {
            if x > 0.0 {
                x.log10()
            } else {
                f64::NAN
            }
        }
        _ => x,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Builder, SeriesKey};
    use crate::hash::Keyed;

    /// Hosts a, b, c: cpu gauges (a constant per host), a request counter, an error count.
    fn store() -> Store {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        for (host, base, env) in [("a", 10.0, "prod"), ("b", 20.0, "prod"), ("c", 60.0, "dev")] {
            let cpu = SeriesKey::new("cpu", [("host", host), ("env", env)], Kind::Gauge).unwrap();
            let req = SeriesKey::new("requests", [("host", host)], Kind::Counter).unwrap();
            let err = SeriesKey::new("errors", [("host", host)], Kind::Count).unwrap();
            for k in [&cpu, &req, &err] {
                b.define(k);
            }
            // 10 minutes at 10 s.
            b.run(cpu.fp, (0..60).map(|i| (i * 10_000, base)));
            // 5 requests a second; host b restarts just before 300 s, its counter from zero again
            // (50 counted by its first sample after).
            b.run(req.fp, (0..60).map(|i| (i * 10_000, if host == "b" && i >= 30 { ((i - 29) * 50) as f64 } else { (i * 50) as f64 })));
            b.run(err.fp, (0..60).map(|i| (i * 10_000, 1.0)));
        }
        s.apply(&b.finish()).unwrap();
        s
    }

    fn q(s: &Store, query: &str) -> Answer {
        let mut r = Request::new(query, 0, 600_000);
        r.step = Some(60_000);
        eval(s, &r).unwrap()
    }

    fn by_tag<'a>(a: &'a Answer, k: &str, v: &str) -> &'a Series {
        a.series.iter().find(|s| s.tags.iter().any(|(tk, tv)| tk == k && tv == v)).unwrap()
    }

    /// Ten days of a daily cycle every five minutes and a spike at its end: `anomalies()` over the
    /// last six hours learns from the history before them; the band holds every point but the
    /// spike, whose score is past the bounds; its arguments are checked.
    #[test]
    fn anomalies_band_a_series_from_its_history_and_score_the_spike_past_it() {
        let mut s = Store::new(Keyed::with_keys(3, 4));
        let mut b = Builder::new();
        let end: i64 = 1_800_000_000_000;
        let k = SeriesKey::new("load", [("host", "a")], Kind::Gauge).unwrap();
        b.define(&k);
        let mut seed = 9u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        let n = 288 * 10;
        b.run(k.fp, (0..n).map(|i| {
            let t = end - (n - i) * 300_000;
            let v = 10.0 + 4.0 * (t as f64 / 86_400_000.0 * std::f64::consts::TAU).sin() + 0.2 * noise();
            (t, if i == n - 1 { v + 12.0 } else { v })
        }));
        s.apply(&b.finish()).unwrap();
        let mut r = Request::new("anomalies(avg:load{*}, 'robust', 3)", end - 6 * 3_600_000, end);
        r.step = Some(300_000);
        let a = eval(&s, &r).unwrap();
        let [one] = a.series.as_slice() else { panic!("one series: {:?}", a.series.len()) };
        let bounds = one.bounds.as_ref().unwrap();
        assert_eq!(one.values.len(), a.grid.n);
        assert_eq!((bounds.lower.len(), bounds.upper.len(), bounds.score.len()), (a.grid.n, a.grid.n, a.grid.n));
        let spike = a.grid.n - 1;
        for i in 0..a.grid.n {
            let (v, lo, hi) = (one.values[i], bounds.lower[i], bounds.upper[i]);
            if v.is_nan() {
                continue;
            }
            if i == spike {
                assert!(v > hi && bounds.anomalous(i) == Some(true), "the spike {v} past {hi}");
            } else {
                assert!(lo <= v && v <= hi && bounds.anomalous(i) == Some(false), "bucket {i}: {v} in [{lo}, {hi}]");
            }
        }
        // Above alone: the spike is above. Below alone: it is not below.
        let mut r = Request::new("anomalies(avg:load{*}, 'robust', 3, 'below')", end - 6 * 3_600_000, end);
        r.step = Some(300_000);
        let below = eval(&s, &r).unwrap();
        assert_eq!(below.series[0].bounds.as_ref().unwrap().anomalous(spike), Some(false));
        for bad in ["anomalies(avg:load{*}, 'sometimes')", "anomalies(avg:load{*}, 'robust', -1)", "anomalies(avg:load{*}, 'robust', 2, 'sideways')", "anomalies(3)"] {
            let mut r = Request::new(bad, end - 3_600_000, end);
            r.step = Some(300_000);
            assert!(eval(&s, &r).is_err(), "{bad}");
        }
    }

    #[test]
    fn space_aggregation_by_groups() {
        let s = store();
        let a = q(&s, "avg:cpu{*}");
        assert_eq!(a.grid, Grid { from: 0, step: 60_000, n: 10 });
        assert_eq!(a.series.len(), 1);
        assert!(a.series[0].values.iter().all(|&v| (v - 30.0).abs() < 1e-9));
        let a = q(&s, "sum:cpu{*} by {env}");
        assert_eq!(by_tag(&a, "env", "prod").values[0], 30.0);
        assert_eq!(by_tag(&a, "env", "dev").values[0], 60.0);
        let a = q(&s, "max:cpu{env:prod}");
        assert_eq!(a.series[0].values[3], 20.0);
        let a = q(&s, "count:cpu{*}");
        assert_eq!(a.series[0].values[0], 3.0);
        let a = q(&s, "p50:cpu{*}");
        assert_eq!(a.series[0].values[0], 20.0);
        let a = q(&s, "p90:cpu{*}");
        assert!((a.series[0].values[0] - 52.0).abs() < 1e-9, "{}", a.series[0].values[0]);
        let a = q(&s, "avg:cpu{!host:c} by {host}");
        assert_eq!(a.series.len(), 2);
    }

    #[test]
    fn counters_are_rates_with_resets_and_counts_are_totals() {
        let s = store();
        // 50 a 10 s = 5 a second; host b resets at 300 s: the bucket holding the reset counts the
        // new value as the increase, so the rate stays 5.
        let a = q(&s, "sum:requests{*} by {host}");
        for host in ["a", "b", "c"] {
            let v = &by_tag(&a, "host", host).values;
            for (i, &x) in v.iter().enumerate().skip(1) {
                assert!((x - 5.0).abs() < 1e-9, "host {host} bucket {i}: {x}");
            }
        }
        let a = q(&s, "sum:requests{host:a}.as_count()");
        assert_eq!(a.series[0].values[2], 300.0);
        // A count sums a bucket: 6 a minute a host, 18 in all.
        let a = q(&s, "sum:errors{*}");
        assert_eq!(a.series[0].values[0], 18.0);
        let a = q(&s, "sum:errors{*}.as_rate()");
        assert!((a.series[0].values[0] - 0.3).abs() < 1e-9);
    }

    #[test]
    fn arithmetic_matches_groups_and_broadcasts() {
        let s = store();
        let a = q(&s, "100 * sum:errors{*} by {host} / sum:errors{*}");
        assert_eq!(a.series.len(), 3);
        assert!((by_tag(&a, "host", "a").values[0] - 100.0 / 3.0).abs() < 1e-9);
        let a = q(&s, "avg:cpu{*} by {host} - avg:cpu{*} by {host}");
        assert!(a.series.iter().all(|s| s.values.iter().all(|&v| v == 0.0)));
        let a = q(&s, "-(2 + 3) * 4");
        assert_eq!(a.scalar, Some(-20.0));
        let a = q(&s, "avg:cpu{host:a} / 0");
        assert!(a.series[0].values[0].is_nan());
    }

    #[test]
    fn functions() {
        let s = store();
        let a = q(&s, "top(avg:cpu{*} by {host}, 2, 'mean', 'desc')");
        assert_eq!(a.series.len(), 2);
        assert_eq!(a.series[0].tags, vec![("host".to_owned(), "c".to_owned())]);
        let a = q(&s, "bottom(avg:cpu{*} by {host}, 1)");
        assert_eq!(a.series[0].tags, vec![("host".to_owned(), "a".to_owned())]);
        let a = q(&s, "cumsum(sum:errors{host:a})");
        assert_eq!(a.series[0].values[2], 18.0);
        let a = q(&s, "diff(cumsum(sum:errors{host:a}))");
        assert_eq!(a.series[0].values[2], 6.0);
        let a = q(&s, "per_minute(sum:errors{host:a})");
        assert_eq!(a.series[0].values[0], 6.0);
        let a = q(&s, "clamp(avg:cpu{*} by {host}, 0, 15)");
        assert!(a.series.iter().all(|s| s.values.iter().all(|&v| v <= 15.0)));
        let a = q(&s, "ewma(avg:cpu{host:a}, 3)");
        assert_eq!(a.series[0].values[5], 10.0);
        let a = q(&s, "abs(-avg:cpu{host:a})");
        assert_eq!(a.series[0].values[0], 10.0);
        // A timeshift reads earlier data: shifted by +5 minutes, buckets past the data are empty.
        let a = q(&s, "timeshift(avg:cpu{host:a}, 300)");
        assert_eq!(a.series[0].values[0], 10.0);
        assert!(a.series[0].values[5].is_nan());
        let a = q(&s, "timeshift(avg:cpu{host:a}, -300)");
        assert!(a.series[0].values[0].is_nan());
        assert_eq!(a.series[0].values[5], 10.0);
    }

    #[test]
    fn fills_and_rollups() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let k = SeriesKey::new("gap", [], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.define(&k);
        b.run(k.fp, [(0, 0.0), (300_000, 50.0)]);
        s.apply(&b.finish()).unwrap();
        let a = q(&s, "avg:gap{*}");
        assert!(a.series[0].values[1].is_nan());
        let a = q(&s, "avg:gap{*}.fill(zero)");
        assert_eq!(a.series[0].values[1], 0.0);
        let a = q(&s, "avg:gap{*}.fill(linear)");
        assert_eq!(a.series[0].values[1], 10.0);
        let a = q(&s, "avg:gap{*}.fill(last, 120)");
        assert_eq!(a.series[0].values[2], 0.0);
        assert!(a.series[0].values[3].is_nan());
        let a = q(&s, "avg:gap{*}.rollup(count, 600)");
        assert_eq!(a.grid.step, 600_000);
        assert_eq!(a.series[0].values[0], 2.0);
    }

    #[test]
    fn limits_and_bad_requests() {
        let s = store();
        assert!(matches!(eval(&s, &Request::new("avg:cpu{*}", 10, 10)), Err(QueryError::EmptyRange)));
        let mut r = Request::new("avg:cpu{*}", 0, 1_000_000_000);
        r.step = Some(1000);
        assert!(matches!(eval(&s, &r), Err(QueryError::TooManyPoints(_))));
        let mut r = Request::new("avg:cpu{*} by {host}", 0, 600_000);
        r.max_series = 2;
        assert!(matches!(eval(&s, &r), Err(QueryError::TooManySeries(_, 2))));
        assert!(matches!(eval(&s, &Request::new("avg:cpu{", 0, 1000)), Err(QueryError::Parse(_))));
        // The step chosen from the range: a nice one, at most the points asked.
        let a = eval(&s, &Request::new("avg:cpu{*}", 0, 86_400_000)).unwrap();
        assert_eq!(a.grid.step, 120_000);
        assert!(a.grid.n <= DEFAULT_POINTS);
    }

    #[test]
    fn percentiles_interpolate_between_order_statistics() {
        let mut v = vec![4.0, 1.0, 3.0, 2.0];
        assert_eq!(percentile(&mut v, 50.0), 2.5);
        assert_eq!(percentile(&mut v, 0.0), 1.0);
        assert_eq!(percentile(&mut v, 100.0), 4.0);
        assert!(percentile(&mut [], 50.0).is_nan());
    }
}
