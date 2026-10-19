//! Evaluating a query over a store (`docs/observe.md` §6).
//!
//! Datadog's model, step by step: a metric query selects series; each series is rolled up into
//! the buckets of a time grid (its kind decides the default: a gauge averages, a count sums, a
//! cumulative counter's increases are summed, resets found); series are combined into groups by
//! their `by` tags, a bucket at a time (the space aggregation); then fills, functions and
//! arithmetic act on whole series, matched by their group tags. Evaluation visits the arena's
//! nodes in order, children before parents: no recursion.

use std::collections::HashMap;

use super::calendar::{self, Calendar};
use super::parse::{Ast, BinOp, CountMode, Fill, Func, MetricQuery, Node, ParseError, Rollup, SpaceAgg, parse};
use crate::anomaly::{Algorithm, Scorer};
use crate::batch::Kind;
use crate::index::{NameMatch, Selector};
use crate::rollup::{self, Agg, Col, Rolled};
use crate::store::{Roll, Scratch, Store, StoreError};

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
    /// The present (ms): a `forecast()` forecasts the buckets after its series' last point, and
    /// reads history for those after this. None: the range's end (nothing ahead).
    pub now: Option<i64>,
}

impl<'a> Request<'a> {
    pub fn new(query: &'a str, from: i64, to: i64) -> Self {
        Self { query, from, to, step: None, points: DEFAULT_POINTS, max_series: DEFAULT_MAX_SERIES, now: None }
    }
}

/// A time grid: `n` buckets of `step` milliseconds from `from`; or, for a calendar rollup, `n` of a
/// calendar's periods from its `k`th (`from` that one's start, `step` the period's usual length:
/// the step a function goes by where it wants one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub from: i64,
    pub step: i64,
    pub n: usize,
    pub calendar: Option<(Calendar, i64)>,
}

impl Grid {
    pub fn uniform(from: i64, step: i64, n: usize) -> Self {
        Self { from, step, n, calendar: None }
    }

    /// Bucket `i`'s start (`n`'s: the last one's end).
    pub fn time(&self, i: usize) -> i64 {
        match self.calendar {
            None => self.from + self.step * i as i64,
            Some((c, k)) => c.start(&c.zone.tz(), k + i as i64).unwrap_or(self.from + self.step * i as i64),
        }
    }

    /// The grid begun `e` buckets earlier.
    fn earlier(&self, e: usize) -> Self {
        let n = self.n + e;
        match self.calendar {
            None => Self { from: self.from - self.step * e as i64, n, ..*self },
            Some((c, k)) => {
                let k = k - e as i64;
                Self { from: c.start(&c.zone.tz(), k).unwrap_or(self.from - self.step * e as i64), n, calendar: Some((c, k)), ..*self }
            }
        }
    }

    /// A calendar grid's edges, shifted by `off`: each bucket's start and the last one's end.
    pub fn edges(&self, off: i64) -> Option<Vec<i64>> {
        let (c, k) = self.calendar?;
        let tz = c.zone.tz();
        Some((0..=self.n as i64).map(|i| c.start(&tz, k + i).map_or(i64::MAX, |t| t.saturating_add(off))).collect())
    }
}

/// Where a time's bucket is and how wide each bucket is: a uniform grid's arithmetic, or a
/// calendar's edges (`Grid::edges`).
#[derive(Debug, Clone, Copy)]
struct Spans<'a> {
    from: i64,
    step: i64,
    n: usize,
    edges: Option<&'a [i64]>,
}

impl<'a> Spans<'a> {
    fn new(grid: Grid, off: i64, edges: Option<&'a [i64]>) -> Self {
        Self { from: grid.from.saturating_add(off), step: grid.step.max(1), n: grid.n, edges }
    }

    /// Bucket `i`'s start (`n`'s: the end).
    fn start(&self, i: usize) -> i64 {
        match self.edges {
            Some(e) => e.get(i).copied().unwrap_or(i64::MAX),
            None => self.from.saturating_add(self.step.saturating_mul(i as i64)),
        }
    }

    fn index(&self, t: i64) -> Option<usize> {
        match self.edges {
            // The last edge at or before `t` begins its bucket (an empty period's, a day a
            // timezone skipped, is passed over), unless that edge is the end.
            Some(e) => e.partition_point(|&x| x <= t).checked_sub(1).filter(|&i| i < self.n),
            None if t < self.from => None,
            None => usize::try_from((t - self.from) / self.step).ok().filter(|&i| i < self.n),
        }
    }

    fn width_ms(&self, i: usize) -> i64 {
        match self.edges {
            Some(_) => self.start(i + 1).saturating_sub(self.start(i)),
            None => self.step,
        }
    }

    fn width_s(&self, i: usize) -> f64 {
        self.width_ms(i) as f64 / 1000.0
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
    /// An `outliers()` series': whether it is an outlier of its group over the range.
    pub outlier: Option<bool>,
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
    /// An `anomalies()` band's: each bucket's error in deviations (NaN for a forecast's).
    pub score: Vec<f64>,
    pub k: f64,
    pub direction: Direction,
    /// What drew it: `basic`, `agile`, `robust` (anomalies) or `linear`, `seasonal` (a forecast).
    pub algorithm: &'static str,
    /// A forecast's first bucket: the values from it on are forecast, not data.
    pub forecast_from: Option<usize>,
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
    /// A named argument (`forecast()`'s).
    Named(String, String),
}

/// Evaluates a query.
pub fn eval(store: &Store, req: &Request<'_>) -> Result<Answer, QueryError> {
    let ast = parse(req.query)?;
    if req.to <= req.from {
        return Err(QueryError::EmptyRange);
    }
    let grid = grid_for(&ast, req)?;
    let shifts = shifts(&ast)?;
    let ext = extensions(&ast, grid, req.now);
    // A node's grid: the answer's, begun `ext` buckets earlier beneath `anomalies()`.
    let at = |e: usize| grid.earlier(e);
    let mut values: Vec<Option<Value>> = vec![None; ast.nodes.len()];
    let mut scanned = (0u64, 0u64);
    let mut scratch = Scratch::default();
    for (id, node) in ast.nodes.iter().enumerate() {
        let take = |values: &mut Vec<Option<Value>>, i: usize| values.get_mut(i).and_then(Option::take).ok_or_else(|| QueryError::Invalid("an operand used twice".into()));
        let v = match node {
            Node::Num(n) => Value::Scalar(*n),
            Node::Str(s) => Value::Str(s.clone()),
            Node::Named(k, v) => Value::Named(k.clone(), v.clone()),
            Node::Metric(q) => {
                let g = at(ext.get(id).copied().unwrap_or(0));
                let edges = reading(g, shifts.get(id).map_or(&[], Vec::as_slice), req.to)?;
                Value::Set(metric(store, q, g, Spans::new(g, edges.0, edges.1.as_deref()), req.max_series, &mut scratch, &mut scanned)?)
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
                Value::Str(_) | Value::Named(..) => return Err(QueryError::Invalid("a string has no sign".into())),
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
                } else if *f == Func::Forecast {
                    forecast(taken, at(own))?
                } else if *f == Func::Pow {
                    let mut it = taken.into_iter();
                    let (Some(a), Some(b)) = (it.next(), it.next()) else { return Err(QueryError::Invalid("pow(a, b)".into())) };
                    binary(BinOp::Pow, a, b)?
                } else {
                    if taken.iter().any(|v| matches!(v, Value::Named(..))) {
                        return Err(QueryError::Invalid(format!("{f:?} takes no named arguments (forecast() does)")));
                    }
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
        Value::Str(_) | Value::Named(..) => return Err(QueryError::Invalid("a query's result is a string".into())),
    };
    if series.len() > req.max_series {
        return Err(QueryError::TooManySeries(series.len(), req.max_series));
    }
    Ok(Answer { grid, series, scalar, scanned_series: scanned.0, scanned_samples: scanned.1 })
}

/// The grid: the step asked, or the smallest nice step that fits the range in the points asked,
/// and never finer than a rollup the query names; `from` aligned down to it.
fn grid_for(ast: &Ast, req: &Request<'_>) -> Result<Grid, QueryError> {
    let mut calendars = ast.nodes.iter().filter_map(|n| match n {
        Node::Metric(m) => m.calendar,
        _ => None,
    });
    if let Some(c) = calendars.next() {
        // A calendar's periods are the grid: every calendar rollup the query names, the same one.
        if calendars.any(|o| o != c) {
            return Err(QueryError::Invalid("a query's calendar rollups must agree: one period, start and timezone".into()));
        }
        if ast.nodes.iter().any(|n| matches!(n, Node::Metric(m) if m.rollup.is_some_and(|r| r.1.is_some()))) {
            return Err(QueryError::Invalid("a query with a calendar rollup takes no rollup in seconds".into()));
        }
        let tz = c.zone.tz();
        let past = || QueryError::Invalid("a range past the calendar's years (±9999)".into());
        let first = c.period(&tz, req.from).ok_or_else(past)?;
        let last = c.period(&tz, req.to - 1).ok_or_else(past)?;
        let n = usize::try_from(last - first + 1).unwrap_or(usize::MAX);
        if n > MAX_POINTS {
            return Err(QueryError::TooManyPoints(n));
        }
        c.start(&tz, last + 1).ok_or_else(past)?;
        return Ok(Grid { from: c.start(&tz, first).ok_or_else(past)?, step: c.unit.nominal_ms(), n, calendar: Some((c, first)) });
    }
    let range = req.to.saturating_sub(req.from);
    let rollup_ms = ast
        .nodes
        .iter()
        .filter_map(|n| match n {
            Node::Metric(m) => m.rollup.and_then(|r| r.1).map(|s| s.saturating_mul(1000)),
            // A forecast's `interval=` (seconds): Datadog's rollup of its points.
            Node::Named(k, v) if k == "interval" => v.parse::<f64>().ok().filter(|s| *s >= 1.0).map(|s| (s * 1000.0) as i64),
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
    Ok(Grid::uniform(from, step, n))
}

/// A shift of the reading beneath a function: fixed (`timeshift`, `hour_before` …), or a calendar's
/// (`calendar_shift`: the same local time some days or months before, in a timezone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shift {
    Ms(i64),
    Calendar { days: i64, months: i64, zone: calendar::Zone },
}

/// Each node's shifts, outermost first: a shifting function's, passed to everything beneath it.
/// Parents come after their children in the arena, so one pass from the end visits a parent
/// before its children.
fn shifts(ast: &Ast) -> Result<Vec<Vec<Shift>>, QueryError> {
    let mut all: Vec<Vec<Shift>> = vec![Vec::new(); ast.nodes.len()];
    for id in (0..ast.nodes.len()).rev() {
        let here = all.get(id).cloned().unwrap_or_default();
        let (children, shift): (&[usize], Option<Shift>) = match ast.nodes.get(id) {
            Some(Node::Neg(x)) => (std::slice::from_ref(x), None),
            Some(Node::Bin(_, l, r)) => {
                if let Some(o) = all.get_mut(*l) {
                    o.clone_from(&here);
                }
                (std::slice::from_ref(r), None)
            }
            Some(Node::Call(Func::Timeshift, args)) => {
                let secs = args.get(1).and_then(|&a| constant(ast, a)).ok_or_else(|| QueryError::Invalid("timeshift(query, seconds): the seconds a number".into()))?;
                (args.get(..1).unwrap_or(&[]), Some(Shift::Ms((secs * 1000.0) as i64)))
            }
            // Datadog's: an hour, a day, a week, 28 days before.
            Some(Node::Call(f @ (Func::HourBefore | Func::DayBefore | Func::WeekBefore | Func::MonthBefore), args)) => {
                let back: i64 = match f {
                    Func::HourBefore => 3_600_000,
                    Func::DayBefore => 86_400_000,
                    Func::WeekBefore => 7 * 86_400_000,
                    _ => 28 * 86_400_000,
                };
                (args.get(..1).unwrap_or(&[]), Some(Shift::Ms(-back)))
            }
            Some(Node::Call(Func::CalendarShift, args)) => {
                let text = |i: usize| match args.get(i).and_then(|&a| ast.nodes.get(a)) {
                    Some(Node::Str(v)) => Some(v.as_str()),
                    _ => None,
                };
                let zone = match text(2) {
                    Some(tz) => calendar::Zone::named(tz).map_err(QueryError::Invalid)?,
                    None => calendar::Zone::UTC,
                };
                let shift = text(1).ok_or_else(|| QueryError::Invalid("calendar_shift(query, '-1d'|'-2w'|'-1mo'[, 'America/New_York'])".into()))?;
                let (days, months) = calendar_shift(shift)?;
                (args.get(..1).unwrap_or(&[]), Some(Shift::Calendar { days, months, zone }))
            }
            Some(Node::Call(_, args)) => (args.as_slice(), None),
            _ => (&[], None),
        };
        for &c in children {
            if let Some(o) = all.get_mut(c) {
                o.clone_from(&here);
                o.extend(shift);
            }
        }
    }
    Ok(all)
}

/// Where a metric query beneath `shifts` reads on `grid`: a fixed offset (and a calendar grid's
/// edges at it), or every bucket's edges shifted one by one.
///
/// A calendar shift on a calendar grid of its timezone, by whole periods of it (`-1d` of days,
/// `-2w` of days or weeks, `-1mo` of months), reads the periods that many before: yesterday's
/// local day, 23 hours or 25. Otherwise each edge moves to the same local time the days or months
/// before; a month back from a month's last days lands on a shorter month's last more than once,
/// out of order, so a shift of months that would is the one at the range's end (`to`), throughout.
fn reading(grid: Grid, shifts: &[Shift], to: i64) -> Result<(i64, Option<Vec<i64>>), QueryError> {
    if shifts.iter().all(|s| matches!(s, Shift::Ms(_))) {
        let off = shifts.iter().map(|s| if let Shift::Ms(ms) = s { *ms } else { 0 }).fold(0i64, i64::saturating_add);
        return Ok((off, grid.edges(off)));
    }
    let past = || QueryError::Invalid("a shift past the calendar's years (±9999)".into());
    let mut edges = grid.edges(0).unwrap_or_else(|| (0..=grid.n).map(|i| grid.time(i)).collect());
    // Whether the edges are still the grid's calendar's periods.
    let mut periods = grid.calendar;
    for &shift in shifts {
        match shift {
            Shift::Ms(ms) => {
                edges.iter_mut().for_each(|e| *e = e.saturating_add(ms));
                periods = None;
            }
            Shift::Calendar { days, months, zone } => {
                let tz = zone.tz();
                let whole = periods.filter(|(c, _)| c.zone == zone).and_then(|(c, k)| {
                    let n = match (c.unit, days, months) {
                        (calendar::Unit::Daily, d, 0) => d,
                        (calendar::Unit::Weekly, d, 0) if d % 7 == 0 => d / 7,
                        (calendar::Unit::Monthly, 0, m) => m,
                        _ => return None,
                    };
                    Some((c, k - n))
                });
                if let Some((c, k)) = whole {
                    for (i, e) in edges.iter_mut().enumerate() {
                        *e = c.start(&tz, k + i as i64).ok_or_else(past)?;
                    }
                    periods = Some((c, k));
                    continue;
                }
                periods = None;
                let moved: Option<Vec<i64>> = edges.iter().map(|&e| calendar::shift_back(&tz, e, days, months).map(|d| e + d)).collect();
                match moved {
                    Some(m) if m.windows(2).all(|w| w.first() <= w.get(1)) => edges = m,
                    _ => {
                        let d = calendar::shift_back(&tz, to, days, months).ok_or_else(past)?;
                        edges.iter_mut().for_each(|e| *e = e.saturating_add(d));
                    }
                }
            }
        }
    }
    Ok((0, Some(edges)))
}

/// Each node's history in buckets: beneath an `anomalies()`, the buckets of
/// [`ANOMALY_HISTORY_MS`] its query reads before the range (all the answer's grid allows), as
/// `offsets` passes a time shift down.
fn extensions(ast: &Ast, grid: Grid, now: Option<i64>) -> Vec<usize> {
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
            Some(Node::Call(Func::Forecast, args)) => (args.get(..1).unwrap_or(&[]), forecast_history(ast, args, grid, now).min(MAX_POINTS.saturating_sub(grid.n))),
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
        out.push(Series { tags: s.tags, values: now.to_vec(), bounds: Some(Box::new(Bounds { lower, upper, score, k, direction, algorithm: algorithm.as_str(), forecast_from: None })) , outlier: None });
    }
    Ok(Value::Set(out))
}

/// A forecast's settings, read from its arguments (`forecast(query, algorithm, deviations, name=...)`).
struct ForecastArgs {
    linear: bool,
    model: crate::forecast::Model,
    /// The season, in ms (seasonal).
    season_ms: i64,
    /// History asked (linear), in ms.
    history_ms: Option<i64>,
    deviations: f64,
}

/// `1w`, `3d`, `12h`, `30m`, `90s` in ms.
fn duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = n.parse().ok()?;
    let per = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return None,
    };
    n.checked_mul(per).filter(|&ms| ms > 0)
}

fn forecast_args<'a>(positional: impl Iterator<Item = Option<&'a str>>, deviations: Option<f64>, named: impl Iterator<Item = (&'a str, &'a str)>) -> Result<ForecastArgs, QueryError> {
    let usage = |why: &str| QueryError::Invalid(format!("forecast(query, 'linear'|'seasonal', deviations, model='default'|'simple'|'reactive', seasonality='hourly'|'daily'|'weekly', history='1w', interval=seconds): {why}"));
    let mut positional = positional;
    let linear = match positional.next().flatten() {
        Some("linear") => true,
        Some("seasonal") => false,
        Some(other) => return Err(usage(&format!("the algorithm {other}"))),
        None => return Err(usage("the algorithm")),
    };
    let deviations = deviations.unwrap_or(2.0);
    if !(deviations.is_finite() && deviations > 0.0) {
        return Err(usage("deviations: a number above 0"));
    }
    let mut a = ForecastArgs { linear, model: crate::forecast::Model::Default, season_ms: 86_400_000, history_ms: None, deviations };
    for (k, v) in named {
        match k {
            "model" => a.model = crate::forecast::Model::parse(v).ok_or_else(|| usage(&format!("the model {v}")))?,
            "seasonality" => {
                a.season_ms = match v {
                    "hourly" => 3_600_000,
                    "daily" => 86_400_000,
                    "weekly" => 7 * 86_400_000,
                    _ => return Err(usage(&format!("the seasonality {v}"))),
                }
            }
            "history" => a.history_ms = Some(duration_ms(v).ok_or_else(|| usage(&format!("the history {v}")))?),
            "interval" => {}
            // kiln's seasons are UTC's.
            "timezone" if v.eq_ignore_ascii_case("utc") => {}
            other => return Err(usage(&format!("a named argument {other}"))),
        }
    }
    Ok(a)
}

/// The history a forecast reads before the range, in buckets: a seasonal one's [`SEASONS`]
/// seasons; a linear one's `history`, or [`LINEAR_HORIZONS`] times the buckets it forecasts (those
/// after `now`).
///
/// [`SEASONS`]: crate::forecast::SEASONS
/// [`LINEAR_HORIZONS`]: crate::forecast::LINEAR_HORIZONS
fn forecast_history(ast: &Ast, args: &[usize], grid: Grid, now: Option<i64>) -> usize {
    let step = grid.step.max(1);
    let positional = args.iter().skip(1).filter_map(|&a| match ast.nodes.get(a) {
        Some(Node::Str(s)) => Some(Some(s.as_str())),
        _ => None,
    });
    let deviations = args.get(2).and_then(|&a| constant(ast, a));
    let named = args.iter().filter_map(|&a| match ast.nodes.get(a) {
        Some(Node::Named(k, v)) => Some((k.as_str(), v.as_str())),
        _ => None,
    });
    let Ok(a) = forecast_args(positional, deviations, named) else { return 0 };
    let ahead = now.map_or(1, |now| {
        let end = grid.time(grid.n);
        usize::try_from((end - now).max(0) / step).unwrap_or(0).max(1)
    });
    let buckets = if a.linear {
        match a.history_ms {
            Some(ms) => usize::try_from(ms / step).unwrap_or(0),
            None => crate::forecast::LINEAR_HORIZONS.saturating_mul(ahead),
        }
    } else {
        crate::forecast::SEASONS.saturating_mul(usize::try_from(a.season_ms / step).unwrap_or(0))
    };
    buckets.min(crate::forecast::MAX_AHEAD * crate::forecast::LINEAR_HORIZONS)
}

/// `forecast(query, algorithm, deviations, ...)`: each series as it is up to its last point, and past
/// it, to the range's end, what `crate::forecast` expects and its bounds at `deviations` (2 unless
/// said), learned from the `history` buckets before the range and the range's own up to that point.
fn forecast(mut args: Vec<Value>, grid: Grid) -> Result<Value, QueryError> {
    let usage = || QueryError::Invalid("forecast(query, 'linear'|'seasonal', deviations, ...): a query first".into());
    if args.is_empty() {
        return Err(usage());
    }
    let set = match args.remove(0) {
        Value::Set(s) => s,
        _ => return Err(usage()),
    };
    if args.iter().any(|v| matches!(v, Value::Set(_))) {
        return Err(QueryError::Invalid("forecast(query, ...): one query, then its algorithm, deviations and named arguments".into()));
    }
    let positional: Vec<Option<&str>> = args.iter().filter_map(|v| match v {
        Value::Str(s) => Some(Some(s.as_str())),
        _ => None,
    }).collect();
    let deviations = args.iter().find_map(|v| match v {
        Value::Scalar(x) => Some(*x),
        _ => None,
    });
    let named: Vec<(&str, &str)> = args.iter().filter_map(|v| match v {
        Value::Named(k, v) => Some((k.as_str(), v.as_str())),
        _ => None,
    }).collect();
    let a = forecast_args(positional.into_iter(), deviations, named.into_iter())?;
    let algo = if a.linear {
        crate::forecast::Algorithm::Linear(a.model)
    } else {
        let period = usize::try_from(a.season_ms / grid.step.max(1)).unwrap_or(0);
        if period < 2 {
            return Err(QueryError::Invalid(format!("a season of {} s at a step of {} s: under two points a season; a finer step", a.season_ms / 1000, grid.step / 1000)));
        }
        crate::forecast::Algorithm::Seasonal { period }
    };
    let algorithm = if a.linear { "linear" } else { "seasonal" };
    let mut out = Vec::with_capacity(set.len());
    for s in set {
        let total = s.values.len();
        let start = total.saturating_sub(grid.n);
        let Some(last) = s.values.iter().rposition(|v| v.is_finite()) else {
            out.push(Series { tags: s.tags, values: s.values.get(start..).unwrap_or(&[]).to_vec(), bounds: None , outlier: None });
            continue;
        };
        let ahead = total - 1 - last;
        let fc = if ahead > 0 { s.values.get(..=last).and_then(|h| crate::forecast::forecast(h, algo, ahead)) } else { None };
        let mut values: Vec<f64> = s.values.get(start..).unwrap_or(&[]).to_vec();
        let Some(fc) = fc else {
            out.push(Series { tags: s.tags, values, bounds: None , outlier: None });
            continue;
        };
        let (mut lower, mut upper) = (vec![f64::NAN; values.len()], vec![f64::NAN; values.len()]);
        for (i, v) in values.iter_mut().enumerate() {
            let abs = start + i;
            if abs <= last {
                continue;
            }
            let k = abs - last - 1;
            if let (Some(&e), Some((lo, hi))) = (fc.expected.get(k), fc.bounds(k, a.deviations)) {
                *v = e;
                if let (Some(l), Some(u)) = (lower.get_mut(i), upper.get_mut(i)) {
                    (*l, *u) = (lo, hi);
                }
            }
        }
        let forecast_from = Some((last + 1).saturating_sub(start).min(values.len()));
        let score = vec![f64::NAN; values.len()];
        out.push(Series { tags: s.tags, values, bounds: Some(Box::new(Bounds { lower, upper, score, k: a.deviations, direction: Direction::Both, algorithm, forecast_from })) , outlier: None });
    }
    Ok(Value::Set(out))
}

/// `top`'s ranking: each series by `by` (Datadog's: "area" the signed area, "l2norm" the norm), the
/// first `n` in that order (`desc` the greatest first); a series with nothing to rank left out.
fn ranked(set: Vec<Series>, n: usize, by: crate::query::parse::Rank, desc: bool) -> Vec<Series> {
    use crate::query::parse::Rank;
    let score = |s: &Series| -> f64 {
        let vals = s.values.iter().copied().filter(|v| !v.is_nan());
        match by {
            Rank::Max => vals.reduce(f64::max).unwrap_or(f64::NAN),
            Rank::Min => vals.reduce(f64::min).unwrap_or(f64::NAN),
            Rank::Last => s.values.iter().rev().copied().find(|v| !v.is_nan()).unwrap_or(f64::NAN),
            Rank::Sum | Rank::Area => vals.fold(None, |a: Option<f64>, v| Some(a.unwrap_or(0.0) + v)).unwrap_or(f64::NAN),
            Rank::Norm => vals.fold(None, |a: Option<f64>, v| Some(a.unwrap_or(0.0) + v * v)).map_or(f64::NAN, f64::sqrt),
            Rank::Mean => {
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
    scored.into_iter().filter(|(s, _)| !s.is_nan()).take(n).map(|(_, s)| s).collect()
}

/// An exponentially weighted moving average, α = 2 / (span + 1): the span "twice the weighted
/// average age" (Datadog's words; Hunter, J. Quality Technology 1986; pandas' `span`).
fn ewma(set: &mut [Series], span: f64) {
    let alpha = 2.0 / (span + 1.0);
    for s in set {
        let mut m = f64::NAN;
        for v in &mut s.values {
            if !v.is_nan() {
                m = if m.is_nan() { *v } else { alpha * *v + (1.0 - alpha) * m };
                *v = m;
            }
        }
    }
}

/// A rolling function of the last `n` points (finite ones; a bucket without one stays without),
/// as Datadog's smoothing functions read "the last 5 data points".
fn rolling(values: &[f64], n: usize, mut f: impl FnMut(&mut Vec<f64>) -> f64) -> Vec<f64> {
    let n = n.max(1);
    let mut last: std::collections::VecDeque<f64> = std::collections::VecDeque::with_capacity(n);
    let mut win: Vec<f64> = Vec::with_capacity(n);
    values
        .iter()
        .map(|&v| {
            if !v.is_finite() {
                return f64::NAN;
            }
            if last.len() == n {
                last.pop_front();
            }
            last.push_back(v);
            win.clear();
            win.extend(last.iter().copied());
            f(&mut win)
        })
        .collect()
}

/// A line through the finite points, by bucket (`a + b·i`): least squares, or Huber's robust
/// regression (`robust`: the loss quadratic within 1.345 of the residuals' scale and linear past it,
/// 95% as efficient as least squares on normal errors; Huber, "Robust Statistics", 1981), fitted
/// by iteratively reweighted least squares from the least-squares line, the scale the residuals'
/// MAD (1.4826 of it).
fn line(values: &[f64], robust: bool) -> Option<(f64, f64)> {
    let pts: Vec<(f64, f64)> = values.iter().enumerate().filter(|(_, v)| v.is_finite()).map(|(i, &v)| (i as f64, v)).collect();
    let fit = |w: &dyn Fn(usize) -> f64| -> Option<(f64, f64)> {
        let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (k, &(x, y)) in pts.iter().enumerate() {
            let wk = w(k);
            sw += wk;
            sx += wk * x;
            sy += wk * y;
            sxx += wk * x * x;
            sxy += wk * x * y;
        }
        let den = sw * sxx - sx * sx;
        if sw <= 0.0 {
            return None;
        }
        if den.abs() < f64::EPSILON * sw * sxx.max(1.0) {
            return Some((sy / sw, 0.0));
        }
        let b = (sw * sxy - sx * sy) / den;
        Some(((sy - b * sx) / sw, b))
    };
    let (mut a, mut b) = fit(&|_| 1.0)?;
    if !robust || pts.len() < 3 {
        return Some((a, b));
    }
    const HUBER_K: f64 = 1.345;
    for _ in 0..50 {
        let resid: Vec<f64> = pts.iter().map(|&(x, y)| y - a - b * x).collect();
        let mut abs: Vec<f64> = resid.iter().map(|r| r.abs()).collect();
        abs.sort_unstable_by(f64::total_cmp);
        let mad = abs.get(abs.len() / 2).copied().unwrap_or(0.0);
        let scale = crate::anomaly::MAD_NORMAL * mad;
        if scale <= 0.0 {
            break;
        }
        let w = |k: usize| resid.get(k).map_or(1.0, |r| (HUBER_K * scale / r.abs()).min(1.0));
        let Some((na, nb)) = fit(&w) else { break };
        let done = (na - a).abs() <= 1e-10 * (1.0 + a.abs()) && (nb - b).abs() <= 1e-10 * (1.0 + b.abs());
        (a, b) = (na, nb);
        if done {
            break;
        }
    }
    Some((a, b))
}

/// The series as constant segments: PELT's exact optimum of the squared error plus a penalty a
/// change (Killick, Fearnhead and Eckley, "Optimal detection of changepoints with a linear
/// computational cost", JASA 2012), the penalty BIC's, 2σ² ln n, σ from the first differences'
/// MAD (√2 of a point's noise); each segment its mean, a bucket without a point left without.
fn piecewise_constant(values: &[f64]) -> Vec<f64> {
    let at: Vec<usize> = values.iter().enumerate().filter(|(_, v)| v.is_finite()).map(|(i, _)| i).collect();
    let y: Vec<f64> = at.iter().filter_map(|&i| values.get(i).copied()).collect();
    let n = y.len();
    let mut out = vec![f64::NAN; values.len()];
    if n == 0 {
        return out;
    }
    let mut d: Vec<f64> = y.windows(2).filter_map(|w| Some((w.get(1)? - w.first()?).abs())).collect();
    d.sort_unstable_by(f64::total_cmp);
    let sigma = crate::anomaly::MAD_NORMAL * d.get(d.len() / 2).copied().unwrap_or(0.0) / std::f64::consts::SQRT_2;
    let beta = 2.0 * sigma * sigma * (n.max(2) as f64).ln();
    // Prefix sums: a segment's cost is its squared error about its mean.
    let (mut s1, mut s2) = (Vec::with_capacity(n + 1), Vec::with_capacity(n + 1));
    let (mut a1, mut a2) = (0.0, 0.0);
    s1.push(0.0);
    s2.push(0.0);
    for &v in &y {
        a1 += v;
        a2 += v * v;
        s1.push(a1);
        s2.push(a2);
    }
    let cost = |a: usize, b: usize| -> f64 {
        let (sum, sq) = (s1.get(b).copied().unwrap_or(0.0) - s1.get(a).copied().unwrap_or(0.0), s2.get(b).copied().unwrap_or(0.0) - s2.get(a).copied().unwrap_or(0.0));
        (sq - sum * sum / (b - a) as f64).max(0.0)
    };
    let mut f = vec![0.0; n + 1];
    if let Some(f0) = f.get_mut(0) {
        *f0 = -beta;
    }
    let mut last = vec![0usize; n + 1];
    let mut candidates: Vec<usize> = vec![0];
    for t in 1..=n {
        let (mut best, mut arg) = (f64::INFINITY, 0usize);
        for &c in &candidates {
            let v = f.get(c).copied().unwrap_or(0.0) + cost(c, t) + beta;
            if v < best {
                (best, arg) = (v, c);
            }
        }
        if let (Some(ft), Some(lt)) = (f.get_mut(t), last.get_mut(t)) {
            (*ft, *lt) = (best, arg);
        }
        // Pruning: a start that can no longer be optimal is dropped.
        candidates.retain(|&c| f.get(c).copied().unwrap_or(0.0) + cost(c, t) <= best);
        candidates.push(t);
    }
    let mut end = n;
    while end > 0 {
        let start = last.get(end).copied().unwrap_or(0);
        let mean = (s1.get(end).copied().unwrap_or(0.0) - s1.get(start).copied().unwrap_or(0.0)) / (end - start) as f64;
        for &i in at.get(start..end).unwrap_or(&[]) {
            if let Some(o) = out.get_mut(i) {
                *o = mean;
            }
        }
        end = start;
    }
    out
}

/// ASAP's span for a moving average (Rong and Bailis, "ASAP: Prioritizing Attention via Time Series
/// Smoothing", VLDB 2017): of spans 1 to a tenth of the series, the one whose average is smoothest
/// (least standard deviation of its first differences) while its kurtosis stays at least the
/// series' own (its trends and outlying swings kept).
fn asap_span(values: &[f64]) -> usize {
    let y: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    let n = y.len();
    if n < 10 {
        return 1;
    }
    let kurtosis = |x: &[f64]| -> f64 {
        let m = x.iter().sum::<f64>() / x.len() as f64;
        let (m2, m4) = x.iter().fold((0.0, 0.0), |(a, b), v| {
            let d = (v - m) * (v - m);
            (a + d, b + d * d)
        });
        let (m2, m4) = (m2 / x.len() as f64, m4 / x.len() as f64);
        if m2 > 0.0 { m4 / (m2 * m2) } else { 0.0 }
    };
    let roughness = |x: &[f64]| -> f64 {
        let d: Vec<f64> = x.windows(2).filter_map(|w| Some(w.get(1)? - w.first()?)).collect();
        if d.is_empty() {
            return 0.0;
        }
        let m = d.iter().sum::<f64>() / d.len() as f64;
        (d.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / d.len() as f64).sqrt()
    };
    let k0 = kurtosis(&y);
    let mut prefix = Vec::with_capacity(n + 1);
    let mut acc = 0.0;
    prefix.push(0.0);
    for &v in &y {
        acc += v;
        prefix.push(acc);
    }
    let mut best = (roughness(&y), 1usize);
    for w in 2..=n / 10 {
        let sma: Vec<f64> = (w..=n).map(|e| (prefix.get(e).copied().unwrap_or(0.0) - prefix.get(e - w).copied().unwrap_or(0.0)) / w as f64).collect();
        if kurtosis(&sma) < k0 {
            continue;
        }
        let r = roughness(&sma);
        if r < best.0 {
            best = (r, w);
        }
    }
    best.1
}

/// `calendar_shift`'s shift as (days, months) back: `-Nd`, `-Nw` (7N days), `-Nmo`.
fn calendar_shift(shift: &str) -> Result<(i64, i64), QueryError> {
    let bad = || QueryError::Invalid(format!("calendar_shift's shift: {shift} (a negative count of d, w or mo: -1d, -2w, -1mo)"));
    let s = shift.trim().strip_prefix('-').ok_or_else(bad)?;
    let digits = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (n, unit) = s.split_at(digits);
    let n: i64 = n.parse().map_err(|_| bad())?;
    if n <= 0 {
        return Err(bad());
    }
    match unit {
        "d" => Ok((n, 0)),
        "w" => Ok((n.checked_mul(7).ok_or_else(bad)?, 0)),
        "mo" => Ok((0, n)),
        _ => Err(bad()),
    }
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

/// Whether `q` names a distribution (`crate::dist`): a metric of its own name holds nothing, its
/// parts do: its buckets' series (`<name>.dist`), or, one kept without percentiles (a metric from
/// logs may be), its `.count` and `.sum`.
fn distribution_of<'q>(store: &Store, q: &'q MetricQuery) -> Option<&'q str> {
    let NameMatch::Exact(name) = &q.metric else { return None };
    let has = |suffix: &str| {
        let mut ids = Vec::new();
        store.select(&Selector { metric: NameMatch::Exact(format!("{name}{suffix}")), clauses: q.filters.clone() }, &mut ids);
        !ids.is_empty()
    };
    (has(crate::dist::SUFFIX) || (has(".count") && has(".sum") && !has(""))).then_some(name.as_str())
}

/// A percentile of an explicit-bucket histogram (an OTLP histogram's `<name>.bucket{le:…}`,
/// cumulative over `le` as Prometheus's are), as Prometheus's `histogram_quantile` reads one: in
/// each bucket of time, each group's counts by `le` (what they rose by), the rank's bucket found
/// and the value interpolated linearly within it, from its lower bound (0 for the first, its upper
/// bound positive) to its upper; in the `+Inf` bucket, the highest finite bound. Within a bucket's
/// width of the truth, as any reading of explicit buckets is.
#[allow(clippy::too_many_arguments)]
fn explicit_quantile(store: &Store, q: &MetricQuery, grid: Grid, spans: Spans<'_>, p: f64, max_series: usize, scratch: &mut Scratch, scanned: &mut (u64, u64)) -> Result<Vec<Series>, QueryError> {
    let NameMatch::Exact(name) = &q.metric else { return Ok(Vec::new()) };
    let mut by = q.by.clone();
    by.push("le".into());
    let part = MetricQuery { metric: NameMatch::Exact(format!("{name}.bucket")), space: SpaceAgg::Sum, by, mode: Some(CountMode::Count), rollup: None, calendar: None, weighted: false, fill: None, ..q.clone() };
    let per_le = metric(store, &part, grid, spans, max_series.saturating_mul(64), scratch, scanned)?;
    // Each group (its tags but `le`): its bounds and their counts in each bucket of time.
    let mut groups: std::collections::BTreeMap<Vec<(String, String)>, Vec<(f64, Vec<f64>)>> = std::collections::BTreeMap::new();
    for s in per_le {
        let Some(le) = s.tags.iter().find(|(k, _)| k == "le").and_then(|(_, v)| if v == "+Inf" { Some(f64::INFINITY) } else { v.parse::<f64>().ok() }) else { continue };
        let tags: Vec<(String, String)> = s.tags.into_iter().filter(|(k, _)| k != "le").collect();
        groups.entry(tags).or_default().push((le, s.values));
    }
    let q01 = (p / 100.0).clamp(0.0, 1.0);
    let mut out = Vec::with_capacity(groups.len());
    for (tags, mut les) in groups {
        les.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut values = vec![f64::NAN; grid.n];
        for (i, v) in values.iter_mut().enumerate() {
            // Counts by bound, made monotonic (rounding can make a sum a bound's below the last's).
            let mut cum: Vec<(f64, f64)> = Vec::with_capacity(les.len());
            let mut most = 0.0f64;
            for (le, vals) in &les {
                let c = vals.get(i).copied().filter(|c| c.is_finite()).unwrap_or(0.0);
                most = most.max(c);
                cum.push((*le, most));
            }
            let total = cum.last().map_or(0.0, |c| c.1);
            if total <= 0.0 {
                continue;
            }
            let rank = q01 * total;
            let Some(at) = cum.iter().position(|&(_, c)| c >= rank) else { continue };
            let (upper, c_up) = cum.get(at).copied().unwrap_or((f64::NAN, 0.0));
            let (lower, c_low) = if at == 0 { (if upper > 0.0 { 0.0 } else { upper }, 0.0) } else { cum.get(at - 1).copied().unwrap_or((0.0, 0.0)) };
            *v = if upper.is_infinite() {
                // The highest finite bound: past it, nothing says where.
                lower
            } else if c_up > c_low {
                lower + (upper - lower) * (rank - c_low) / (c_up - c_low)
            } else {
                upper
            };
        }
        out.push(Series { tags, values, bounds: None, outlier: None });
    }
    Ok(out)
}

/// Whether `q` names a distribution whose buckets are kept (`<name>.dist`).
fn buckets_of<'q>(store: &Store, q: &'q MetricQuery) -> Option<&'q str> {
    let NameMatch::Exact(name) = &q.metric else { return None };
    let mut ids = Vec::new();
    store.select(&Selector { metric: NameMatch::Exact(format!("{name}{}", crate::dist::SUFFIX)), clauses: q.filters.clone() }, &mut ids);
    (!ids.is_empty()).then_some(name.as_str())
}

/// A distribution's series on the grid, Datadog's: a percentile (`p99:`) of its values, every
/// series' buckets of a group merged for each bucket of time (at the coarsest scale among them),
/// read as `crate::dist::Merged` reads them; `avg` its sums over its counts; `sum`, `count`, `min`
/// and `max` its own of those, each a bucket's total.
fn distribution(store: &Store, q: &MetricQuery, grid: Grid, spans: Spans<'_>, max_series: usize, scratch: &mut Scratch, scanned: &mut (u64, u64)) -> Result<Vec<Series>, QueryError> {
    let NameMatch::Exact(name) = &q.metric else { return Ok(Vec::new()) };
    let part = |suffix: &str, space: SpaceAgg| MetricQuery { metric: NameMatch::Exact(format!("{name}{suffix}")), space, mode: Some(CountMode::Count), rollup: None, calendar: None, weighted: false, ..q.clone() };
    let p = match q.space {
        SpaceAgg::Pct(p) => p,
        SpaceAgg::Avg => {
            let sums = metric(store, &part(".sum", SpaceAgg::Sum), grid, spans, max_series, scratch, scanned)?;
            let counts = metric(store, &part(".count", SpaceAgg::Sum), grid, spans, max_series, scratch, scanned)?;
            return Ok(sums
                .into_iter()
                .filter_map(|mut s| {
                    let c = counts.iter().find(|c| c.tags == s.tags)?;
                    for (v, n) in s.values.iter_mut().zip(&c.values) {
                        *v = if *n > 0.0 { *v / *n } else { f64::NAN };
                    }
                    Some(s)
                })
                .collect());
        }
        SpaceAgg::Sum => return metric(store, &part(".sum", SpaceAgg::Sum), grid, spans, max_series, scratch, scanned),
        SpaceAgg::Count => return metric(store, &part(".count", SpaceAgg::Sum), grid, spans, max_series, scratch, scanned),
        SpaceAgg::Min => return metric(store, &MetricQuery { mode: None, ..part(".min", SpaceAgg::Min) }, grid, spans, max_series, scratch, scanned),
        SpaceAgg::Max => return metric(store, &MetricQuery { mode: None, ..part(".max", SpaceAgg::Max) }, grid, spans, max_series, scratch, scanned),
    };
    let mut ids = Vec::new();
    store.select(&Selector { metric: NameMatch::Exact(format!("{name}{}", crate::dist::SUFFIX)), clauses: q.filters.clone() }, &mut ids);
    if ids.is_empty() {
        // No exponential buckets: an explicit histogram's, if it has them.
        return explicit_quantile(store, q, grid, spans, p, max_series, scratch, scanned);
    }
    // In the order of the series' fingerprints, the same on every replica: each group's buckets
    // summed in one order everywhere.
    ids.sort_unstable_by_key(|&id| store.fingerprint(id).unwrap_or(0));
    // Groups by the query's tags' values, each its buckets' series: (id, scale, bucket). The key
    // built in one buffer, copied only for a new group.
    let mut groups: std::collections::BTreeMap<Vec<&str>, Vec<(u32, i32, crate::dist::Bucket)>> = std::collections::BTreeMap::new();
    let mut key: Vec<&str> = Vec::with_capacity(q.by.len());
    for &id in &ids {
        let Some(s) = store.series(id) else { continue };
        let Some((scale, b)) = s.tag(crate::dist::BUCKET_TAG).and_then(crate::dist::parse_tag_value) else { continue };
        key.clear();
        key.extend(q.by.iter().map(|k| s.tag(k).unwrap_or("N/A")));
        if let Some(members) = groups.get_mut(key.as_slice()) {
            members.push((id, scale, b));
            continue;
        }
        if groups.len() >= max_series {
            return Err(QueryError::TooManySeries(groups.len() + 1, max_series));
        }
        groups.insert(key.clone(), vec![(id, scale, b)]);
    }
    let mut out = Vec::with_capacity(groups.len());
    let mut table = DistTable::default();
    let mut totals = vec![0.0; grid.n];
    let q01 = (p / 100.0).clamp(0.0, 1.0);
    for (values, members) in groups {
        let scale = dist_counts(store, q, grid, spans, &members, scratch, scanned, &mut table)?;
        let tags = q.by.iter().zip(values).map(|(k, v)| (k.clone(), v.to_owned())).collect();
        // Each bucket of time: the rank's bucket, DDSketch's way (`crate::dist::Merged::quantile`).
        totals.fill(0.0);
        for &row in &table.sorted {
            for (t, c) in totals.iter_mut().zip(table.counts(row)) {
                *t += c;
            }
        }
        let values: Vec<f64> = totals
            .iter()
            .enumerate()
            .map(|(i, &total)| {
                if total <= 0.0 {
                    return f64::NAN;
                }
                let rank = q01 * (total - 1.0).max(0.0);
                let (mut seen, mut last) = (0.0, f64::NAN);
                for &row in &table.sorted {
                    let c = table.counts(row).get(i).copied().unwrap_or(0.0);
                    if c <= 0.0 {
                        continue;
                    }
                    last = table.buckets.get(row).map_or(f64::NAN, |b| b.value(scale));
                    seen += c;
                    if rank < seen {
                        return last;
                    }
                }
                last
            })
            .collect();
        out.push(Series { tags, values, bounds: None, outlier: None });
    }
    Ok(out)
}

/// A distribution's buckets, each its counts in each bucket of time: one table, its room kept from
/// group to group (a query by host allocates for its largest group, not for each bucket of each).
#[derive(Debug, Default)]
struct DistTable {
    n: usize,
    buckets: Vec<crate::dist::Bucket>,
    /// `counts[row * n + t]`.
    counts: Vec<f64>,
    rows: HashMap<(u8, i64), usize>,
    /// The rows, ascending by value.
    sorted: Vec<usize>,
    ts: Vec<i64>,
    vs: Vec<f64>,
    each: Vec<f64>,
}

impl DistTable {
    fn clear(&mut self, n: usize) {
        self.n = n;
        self.buckets.clear();
        self.counts.clear();
        self.rows.clear();
        self.sorted.clear();
    }

    /// `b`'s row, made if new.
    fn row(&mut self, b: crate::dist::Bucket) -> usize {
        let Self { n, buckets, counts, rows, .. } = self;
        *rows.entry(b.order()).or_insert_with(|| {
            buckets.push(b);
            counts.resize(counts.len() + *n, 0.0);
            buckets.len() - 1
        })
    }

    fn counts(&self, row: usize) -> &[f64] {
        self.counts.get(row * self.n..(row + 1) * self.n).unwrap_or(&[])
    }

    /// Adds `from` to `row`'s counts, each positive finite one.
    fn add(&mut self, row: usize, from: &[f64]) {
        if let Some(counts) = self.counts.get_mut(row * self.n..(row + 1) * self.n) {
            for (c, v) in counts.iter_mut().zip(from) {
                if v.is_finite() && *v > 0.0 {
                    *c += v;
                }
            }
        }
    }

    /// Adds a count series' samples to `row`, each in its bucket of time (what `rollup` sums for
    /// it, without a pass over every bucket of time for each series).
    fn add_samples(&mut self, row: usize, spans: Spans<'_>, ts: &[i64], vs: &[f64]) {
        let Some(counts) = self.counts.get_mut(row * self.n..(row + 1) * self.n) else { return };
        for (&t, &v) in ts.iter().zip(vs) {
            if v.is_finite()
                && v > 0.0
                && let Some(c) = spans.index(t).and_then(|i| counts.get_mut(i))
            {
                *c += v;
            }
        }
    }

    /// The rows in order, once filled.
    fn sort(&mut self) {
        let Self { buckets, sorted, .. } = self;
        sorted.clear();
        sorted.extend(0..buckets.len());
        sorted.sort_unstable_by_key(|&r| buckets.get(r).map(|b| b.order()));
    }
}

/// A distribution's buckets in a bucket of time each, into `table`: `members`' (series id, scale,
/// bucket), each made the coarsest scale's among them, their counts summed; the scale.
#[allow(clippy::too_many_arguments)]
fn dist_counts(store: &Store, q: &MetricQuery, grid: Grid, spans: Spans<'_>, members: &[(u32, i32, crate::dist::Bucket)], scratch: &mut Scratch, scanned: &mut (u64, u64), table: &mut DistTable) -> Result<i32, QueryError> {
    let counted = MetricQuery { mode: Some(CountMode::Count), rollup: Some((Rollup::Sum, None)), calendar: None, weighted: false, fill: None, ..q.clone() };
    let (from, end) = (spans.start(0), spans.start(grid.n));
    let scale = members.iter().map(|m| m.1).min().unwrap_or(crate::dist::SCALE);
    table.clear(grid.n);
    let (mut ts, mut vs, mut each) = (std::mem::take(&mut table.ts), std::mem::take(&mut table.vs), std::mem::take(&mut table.each));
    each.clear();
    each.resize(grid.n, f64::NAN);
    let tier = tier_for(spans);
    let (mut rolled, mut window_n) = (rolled_for(tier, from, end, &counted), Vec::new());
    for &(id, at, b) in members {
        let Some(s) = store.series(id) else { continue };
        ts.clear();
        vs.clear();
        let lookback = if s.kind() == Kind::Counter { grid.step } else { 0 };
        let row = table.row(b.coarser(u32::try_from(at - scale).unwrap_or(0)));
        if let Some(width) = tier {
            rolled.clear();
            let roll = Roll { width, from, to: end, kind: s.kind(), cols: columns_for(s.kind(), &counted) };
            store.read_rolled(id, from.saturating_sub(lookback), end.saturating_sub(1), &roll, scratch, &mut ts, &mut vs, &mut rolled)?;
            scanned.0 += 1;
            scanned.1 += ts.len() as u64 + rolled.n.iter().map(|&n| u64::from(n)).sum::<u64>();
            rollup_windows(s.kind(), &counted, spans, width, &ts, &vs, &rolled, &mut each, &mut window_n);
            table.add(row, &each);
            continue;
        }
        store.read_with(id, from.saturating_sub(lookback), end.saturating_sub(1), scratch, &mut ts, &mut vs)?;
        scanned.0 += 1;
        scanned.1 += ts.len() as u64;
        if s.kind() == Kind::Count {
            table.add_samples(row, spans, &ts, &vs);
        } else {
            rollup(s.kind(), &counted, spans, &ts, &vs, &mut each, None);
            table.add(row, &each);
        }
    }
    (table.ts, table.vs, table.each) = (ts, vs, each);
    table.sort();
    Ok(scale)
}

/// A heatmap of a query (`docs/observe.md` §6): each bucket of time, how many values fell in each
/// row. A distribution's rows are its buckets, merged across its series and made coarser a scale
/// at a time until they are `rows` at the most; any other query's are `rows` equal ranges between
/// its least and most value, its series' values counted (Datadog's heatmap of a metric across its
/// tags).
#[derive(Debug, Clone, PartialEq)]
pub struct Heatmap {
    pub grid: Grid,
    /// Each row's range, ascending (a distribution's bucket's bounds).
    pub rows: Vec<(f64, f64)>,
    /// `counts[t][r]`: bucket of time `t`, row `r`.
    pub counts: Vec<Vec<f64>>,
    pub distribution: bool,
}

/// The most rows a heatmap has: a chart's height in pixels' worth, a few to a row.
pub const HEATMAP_ROWS: usize = 120;

pub fn heatmap(store: &Store, req: &Request<'_>, rows: usize) -> Result<Heatmap, QueryError> {
    let rows = rows.clamp(1, HEATMAP_ROWS);
    let ast = parse(req.query)?;
    if req.to <= req.from {
        return Err(QueryError::EmptyRange);
    }
    if let Some(Node::Metric(q)) = ast.nodes.get(ast.root)
        && buckets_of(store, q).is_some()
        && let NameMatch::Exact(name) = &q.metric
    {
        let grid = grid_for(&ast, req)?;
        let edges = grid.edges(0);
        let spans = Spans::new(grid, 0, edges.as_deref());
        let mut ids = Vec::new();
        store.select(&Selector { metric: NameMatch::Exact(format!("{name}{}", crate::dist::SUFFIX)), clauses: q.filters.clone() }, &mut ids);
        ids.sort_unstable_by_key(|&id| store.fingerprint(id).unwrap_or(0));
        let members: Vec<(u32, i32, crate::dist::Bucket)> = ids.iter().filter_map(|&id| store.series(id)?.tag(crate::dist::BUCKET_TAG).and_then(crate::dist::parse_tag_value).map(|(s, b)| (id, s, b))).collect();
        let (mut table, mut coarser) = (DistTable::default(), DistTable::default());
        let mut scale = dist_counts(store, q, grid, spans, &members, &mut Scratch::default(), &mut (0, 0), &mut table)?;
        // Coarser until the rows fit: each step halves the buckets of each sign, exactly.
        while table.buckets.len() > rows && scale > crate::dist::MIN_SCALE {
            scale -= 1;
            coarser.clear(grid.n);
            for (row, &b) in table.buckets.iter().enumerate() {
                let into = coarser.row(b.coarser(1));
                coarser.add(into, table.counts(row));
            }
            coarser.sort();
            std::mem::swap(&mut table, &mut coarser);
        }
        let rows: Vec<(f64, f64)> = table.sorted.iter().filter_map(|&r| table.buckets.get(r).map(|b| b.bounds(scale))).collect();
        let counts = (0..grid.n).map(|t| table.sorted.iter().map(|&r| table.counts(r).get(t).copied().unwrap_or(0.0)).collect()).collect();
        return Ok(Heatmap { grid, rows, counts, distribution: true });
    }
    let a = eval(store, req)?;
    let finite = a.series.iter().flat_map(|s| s.values.iter().copied()).filter(|v| v.is_finite());
    let (lo, hi) = finite.fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| (l.min(v), h.max(v)));
    if lo > hi {
        return Ok(Heatmap { grid: a.grid, rows: Vec::new(), counts: vec![Vec::new(); a.grid.n], distribution: false });
    }
    // A range of one value is one row; else `rows` equal ones, the last closed at the most.
    let n = if hi > lo { rows } else { 1 };
    let width = if hi > lo { (hi - lo) / n as f64 } else { 1.0 };
    let edges: Vec<(f64, f64)> = (0..n).map(|r| (lo + width * r as f64, if r + 1 == n { hi } else { lo + width * (r + 1) as f64 })).collect();
    let mut counts = vec![vec![0.0; n]; a.grid.n];
    for s in &a.series {
        for (t, &v) in s.values.iter().enumerate() {
            if !v.is_finite() {
                continue;
            }
            let r = (((v - lo) / width) as usize).min(n - 1);
            if let Some(c) = counts.get_mut(t).and_then(|row| row.get_mut(r)) {
                *c += 1.0;
            }
        }
    }
    Ok(Heatmap { grid: a.grid, rows: edges, counts, distribution: false })
}

/// A metric query's series on the grid.
fn metric(store: &Store, q: &MetricQuery, grid: Grid, spans: Spans<'_>, max_series: usize, scratch: &mut Scratch, scanned: &mut (u64, u64)) -> Result<Vec<Series>, QueryError> {
    if distribution_of(store, q).is_some() {
        return distribution(store, q, grid, spans, max_series, scratch, scanned);
    }
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
    let mut group_of: Vec<usize> = Vec::with_capacity(ids.len());
    for &id in &ids {
        let Some(s) = store.series(id) else {
            group_of.push(usize::MAX);
            continue;
        };
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
        group_of.push(g);
    }
    let (from, end) = (spans.start(0), spans.start(grid.n));
    // A grid of whole windows of a tier is read window by window (`crate::rollup`): from rollups
    // where a block holds them.
    let tier = if q.weighted { None } else { tier_for(spans) };
    let mut pending: Vec<(usize, usize, f64, f64)> = Vec::new();
    let mut intervals = Vec::new();
    if q.weighted {
        // `.weighted()`: a gauge series' samples a bucket, then its weights; its interval, the median
        // gap between its samples (learned, so a series reporting every 10 s and one every 60 s weigh
        // alike over a bucket they both covered). One with too few samples to tell takes the query's,
        // the median of the others', once they are known.
        let mut counts = vec![0.0; grid.n];
        let mut gaps = Vec::with_capacity(INTERVAL_GAPS);
        intervals.reserve(ids.len());
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        let mut buckets = vec![f64::NAN; grid.n];
        for (&id, &g) in ids.iter().zip(&group_of) {
            let Some(s) = store.series(id) else { continue };
            ts.clear();
            vs.clear();
            // A counter's increase into the first bucket needs the sample before it.
            let lookback = if s.kind() == Kind::Counter { grid.step } else { 0 };
            let weighted = s.kind() == Kind::Gauge;
            store.read_with(id, from.saturating_sub(lookback), end.saturating_sub(1), scratch, &mut ts, &mut vs)?;
            scanned.0 += 1;
            scanned.1 += ts.len() as u64;
            rollup(s.kind(), q, spans, &ts, &vs, &mut buckets, if weighted { Some(&mut counts) } else { None });
            let Some(a) = acc.get_mut(g) else { continue };
            if !weighted {
                a.add(&buckets, None);
                continue;
            }
            match interval_ms(&ts, &vs, &mut gaps) {
                Some(d) => {
                    intervals.push(d);
                    weigh(&mut counts, d, spans);
                    a.add(&buckets, Some(&counts));
                }
                // Under two samples: one bucket at the most.
                None => pending.extend(buckets.iter().zip(&counts).enumerate().filter(|(_, (v, _))| !v.is_nan()).map(|(i, (&v, &c))| (g, i, v, c))),
            }
        }
    } else {
        let at = Reading { store, q, grid, spans, tier, from, end };
        let mut reader = Reader::new(&at, std::mem::take(scratch));
        let read = read_groups(&at, &ids, &group_of, &mut acc, &mut reader, scanned);
        *scratch = reader.scratch;
        read?;
    }
    if !pending.is_empty() {
        let d = if intervals.is_empty() {
            None
        } else {
            let mid = intervals.len() / 2;
            Some(*intervals.select_nth_unstable(mid).1)
        };
        for (g, i, v, c) in pending {
            // Not knowing the interval, a series counts whole.
            let w = d.map_or(1.0, |d| weight(c, d, spans.width_ms(i)));
            if let Some(a) = acc.get_mut(g) {
                a.put(i, v, w);
            }
        }
    }
    let mut out = Vec::with_capacity(acc.len());
    for (tags, a) in group_tags.into_iter().zip(acc) {
        let mut values = a.finish();
        if let Some(fill) = q.fill {
            apply_fill(fill, grid, &mut values);
        }
        out.push(Series { tags, values, bounds: None , outlier: None });
    }
    out.sort_by(|a, b| a.tags.cmp(&b.tags));
    Ok(out)
}

/// What every series of a query is read at: its store, its query, its grid and the range read.
#[derive(Clone, Copy)]
struct Reading<'a> {
    store: &'a Store,
    q: &'a MetricQuery,
    grid: Grid,
    spans: Spans<'a>,
    tier: Option<i64>,
    from: i64,
    end: i64,
}

/// What a thread reading series holds, reused series to series.
struct Reader {
    scratch: Scratch,
    ts: Vec<i64>,
    vs: Vec<f64>,
    buckets: Vec<f64>,
    rolled: Rolled,
    window_n: Vec<f64>,
}

impl Reader {
    fn new(at: &Reading<'_>, scratch: Scratch) -> Self {
        Self { scratch, ts: Vec::new(), vs: Vec::new(), buckets: vec![f64::NAN; at.grid.n], rolled: rolled_for(at.tier, at.from, at.end, at.q), window_n: Vec::new() }
    }

    /// Series `id`'s values on the grid, into `self.buckets`: the samples it read, or none if the
    /// store holds no such series.
    fn read(&mut self, at: &Reading<'_>, id: u32) -> Result<Option<u64>, QueryError> {
        let Some(s) = at.store.series(id) else { return Ok(None) };
        self.ts.clear();
        self.vs.clear();
        // A counter's increase into the first bucket needs the sample before it.
        let lookback = if s.kind() == Kind::Counter { at.grid.step } else { 0 };
        if let Some(width) = at.tier {
            self.rolled.clear();
            let roll = Roll { width, from: at.from, to: at.end, kind: s.kind(), cols: columns_for(s.kind(), at.q) };
            at.store.read_rolled(id, at.from.saturating_sub(lookback), at.end.saturating_sub(1), &roll, &mut self.scratch, &mut self.ts, &mut self.vs, &mut self.rolled)?;
            rollup_windows(s.kind(), at.q, at.spans, width, &self.ts, &self.vs, &self.rolled, &mut self.buckets, &mut self.window_n);
            Ok(Some(self.ts.len() as u64 + self.rolled.n.iter().map(|&n| u64::from(n)).sum::<u64>()))
        } else {
            at.store.read_with(id, at.from.saturating_sub(lookback), at.end.saturating_sub(1), &mut self.scratch, &mut self.ts, &mut self.vs)?;
            rollup(s.kind(), at.q, at.spans, &self.ts, &self.vs, &mut self.buckets, None);
            Ok(Some(self.ts.len() as u64))
        }
    }
}

/// How long a query reads its series on its own thread before it asks for helpers: one that is
/// done by then does without them (a thread costs some tens of microseconds to start).
const ALONE: std::time::Duration = std::time::Duration::from_millis(2);
/// Series a thread reads a round, at the most, before what was read is added in: what a round of
/// many threads holds is bounded (`grid.n` values a series).
const ROUND: usize = 64;

/// Helper threads queries may read on beside their own, taken and given back: half the machine's
/// cores, less one, shared by every query at once.
fn helpers_free() -> &'static std::sync::atomic::AtomicUsize {
    static FREE: std::sync::OnceLock<std::sync::atomic::AtomicUsize> = std::sync::OnceLock::new();
    FREE.get_or_init(|| std::sync::atomic::AtomicUsize::new(std::thread::available_parallelism().map_or(1, std::num::NonZero::get).div_ceil(2).saturating_sub(1)))
}

/// Helpers taken, given back as they go.
struct Helpers(usize);

impl Helpers {
    fn take(want: usize) -> Self {
        use std::sync::atomic::Ordering;
        let free = helpers_free();
        let got = free.try_update(Ordering::AcqRel, Ordering::Acquire, |f| Some(f - f.min(want))).map_or(0, |f| f.min(want));
        Self(got)
    }
}

impl Drop for Helpers {
    fn drop(&mut self) {
        helpers_free().fetch_add(self.0, std::sync::atomic::Ordering::AcqRel);
    }
}

/// The threads every query reads on, when a test says so.
#[cfg(test)]
fn forced_threads() -> Option<usize> {
    tests::threads()
}

#[cfg(not(test))]
fn forced_threads() -> Option<usize> {
    None
}

/// Every series of `ids` (in their fingerprints' order) read, and its values added into its
/// group's (`group_of`) in that order, as one thread would add them: the same bits however they
/// were read. On this thread alone while it is quick ([`ALONE`]); then, helpers free, a round at a
/// time on as many threads, each round's values added in order after it.
fn read_groups(at: &Reading<'_>, ids: &[u32], group_of: &[usize], acc: &mut [Acc], reader: &mut Reader, scanned: &mut (u64, u64)) -> Result<(), QueryError> {
    let started = std::time::Instant::now();
    let n = at.grid.n;
    let mut helpers: Option<Helpers> = None;
    let mut i = 0;
    while i < ids.len() {
        let left = ids.len() - i;
        let forced = forced_threads();
        if helpers.is_none() && forced.is_none() && left > ROUND && started.elapsed() >= ALONE {
            helpers = Some(Helpers::take(left.div_ceil(ROUND) - 1));
        }
        let workers = forced.unwrap_or_else(|| helpers.as_ref().map_or(0, |h| h.0) + 1);
        if workers == 1 {
            let (Some(&id), Some(&g)) = (ids.get(i), group_of.get(i)) else { break };
            if let Some(samples) = reader.read(at, id)? {
                scanned.0 += 1;
                scanned.1 += samples;
                if let Some(a) = acc.get_mut(g) {
                    a.add(&reader.buckets, None);
                }
            }
            i += 1;
            continue;
        }
        let round = left.min(workers * ROUND);
        let Some(part_ids) = ids.get(i..i + round) else { break };
        let per = round.div_ceil(workers);
        // Each part: its series' values, whether each was there, and the samples read.
        type Part = Result<(Vec<f64>, Vec<bool>, u64), QueryError>;
        let read_part = |r: &mut Reader, part: &[u32]| -> Part {
            let mut values = Vec::with_capacity(part.len() * n);
            let mut there = Vec::with_capacity(part.len());
            let mut samples = 0;
            for &id in part {
                match r.read(at, id)? {
                    Some(k) => {
                        samples += k;
                        values.extend_from_slice(&r.buckets);
                        there.push(true);
                    }
                    None => {
                        values.resize(values.len() + n, f64::NAN);
                        there.push(false);
                    }
                }
            }
            Ok((values, there, samples))
        };
        let parts: Vec<Part> = std::thread::scope(|scope| {
            let mut chunks = part_ids.chunks(per);
            let mine = chunks.next().unwrap_or(&[]);
            let handles: Vec<_> = chunks.map(|c| scope.spawn(move || read_part(&mut Reader::new(at, Scratch::default()), c))).collect();
            let mut out = vec![read_part(reader, mine)];
            out.extend(handles.into_iter().map(|h| h.join().unwrap_or(Err(QueryError::Invalid("a thread reading series failed".into())))));
            out
        });
        let mut k = i;
        for part in parts {
            let (values, there, samples) = part?;
            scanned.1 += samples;
            for (series, &present) in values.chunks(n.max(1)).zip(&there) {
                if present {
                    scanned.0 += 1;
                    if let Some(a) = group_of.get(k).and_then(|&g| acc.get_mut(g)) {
                        a.add(series, None);
                    }
                }
                k += 1;
            }
        }
        i += round;
    }
    Ok(())
}

/// Gaps [`interval_ms`] reads at the most, spread over the series: a median of 64 has a standard
/// error of 0.16 of the gaps' own spread (√(π/2) / √64 of their σ, a normal law's), and a series
/// reporting at an interval has gaps all but equal, so reading every gap of a long one buys nothing.
const INTERVAL_GAPS: usize = 64;

/// A series' submission interval (ms): the median gap between consecutive finite samples, of at
/// most [`INTERVAL_GAPS`] of them spread over it; none under two samples.
fn interval_ms(ts: &[i64], vs: &[f64], gaps: &mut Vec<i64>) -> Option<i64> {
    gaps.clear();
    let pairs = ts.len().saturating_sub(1);
    let stride = pairs.div_ceil(INTERVAL_GAPS).max(1);
    for i in (0..pairs).step_by(stride) {
        if let (Some(&a), Some(&b), Some(va), Some(vb)) = (ts.get(i), ts.get(i + 1), vs.get(i), vs.get(i + 1))
            && va.is_finite()
            && vb.is_finite()
            && b > a
        {
            gaps.push(b - a);
        }
    }
    if gaps.is_empty() {
        return None;
    }
    let mid = gaps.len() / 2;
    Some(*gaps.select_nth_unstable(mid).1)
}

/// A bucket's weight for a series with `count` samples in it at an interval of `d` ms: the share of
/// the bucket they cover (Datadog's `weighted()`: a pod that lived 10 s of a minute counts a sixth).
fn weight(count: f64, d: i64, width_ms: i64) -> f64 {
    if count <= 0.0 || width_ms <= 0 {
        return 0.0;
    }
    (count * d as f64 / width_ms as f64).min(1.0)
}

/// Sample counts a bucket into weights, in place.
fn weigh(counts: &mut [f64], d: i64, spans: Spans<'_>) {
    for (i, c) in counts.iter_mut().enumerate() {
        *c = weight(*c, d, spans.width_ms(i));
    }
}

/// The coarsest tier (`crate::rollup::TIERS`) of which every bucket of `spans` is whole windows: a
/// step of five minutes or more on the grid's ladder, a calendar's periods, shifted by a multiple
/// of the tier. None: the buckets are read sample by sample ([`rollup`]).
fn tier_for(spans: Spans<'_>) -> Option<i64> {
    rollup::TIERS.iter().rev().copied().find(|&w| match spans.edges {
        Some(e) => e.get(..=spans.n).is_some_and(|e| e.iter().all(|x| x.rem_euclid(w) == 0)),
        None => spans.from.rem_euclid(w) == 0 && spans.step % w == 0,
    })
}

/// Room for a series' windows of `tier` in `[from, end)` in the columns `q` reads of a gauge, a count
/// or a counter; none without a tier.
fn rolled_for(tier: Option<i64>, from: i64, end: i64, q: &MetricQuery) -> Rolled {
    let Some(w) = tier else { return Rolled::default() };
    let n = usize::try_from(end.saturating_sub(from) / w).unwrap_or(0).saturating_add(1).min(MAX_POINTS * 12);
    let mut cols: Vec<Col> = [Kind::Gauge, Kind::Count, Kind::Counter].iter().flat_map(|&k| columns_for(k, q).iter().copied()).collect();
    cols.dedup();
    Rolled::with_capacity(n, &cols)
}

/// The rollup columns a query of a series of `kind` reads (`crate::rollup::columns`).
fn columns_for(kind: Kind, q: &MetricQuery) -> &'static [Col] {
    if kind == Kind::Counter {
        return rollup::columns(Kind::Counter);
    }
    match q.rollup.map(|r| r.0).unwrap_or(if kind == Kind::Count { Rollup::Sum } else { Rollup::Avg }) {
        Rollup::Avg | Rollup::Sum => &[Col::Sum],
        Rollup::Min => &[Col::Min],
        Rollup::Max => &[Col::Max],
        Rollup::Last => &[Col::Last],
        Rollup::Count => &[],
    }
}

/// A window's fold, from its samples or its rollup: whether it held any sample, its finite ones'
/// count and aggregates; a counter's increase into it from the sample before (`cross`) and its
/// increases after its first (`inc`).
#[derive(Clone, Copy)]
struct Window {
    has: bool,
    agg: Agg,
    cross: f64,
}

/// One series' samples and rolled windows into `buckets` (NaN where none), window by window
/// (`crate::rollup`): each window of `width` folded in time order, from its rollup or from its
/// samples alike, then each bucket's windows in time order. Every bucket is whole windows
/// ([`tier_for`]), so a bucket's value does not depend on which of its windows were rolled up:
/// what keeps replicas' answers alike to the bit. Its functions are [`rollup`]'s; `n`, room for a
/// bucket's count.
#[allow(clippy::too_many_arguments)]
fn rollup_windows(kind: Kind, q: &MetricQuery, spans: Spans<'_>, width: i64, ts: &[i64], vs: &[f64], rolled: &Rolled, buckets: &mut [f64], n: &mut Vec<f64>) {
    buckets.fill(f64::NAN);
    n.clear();
    n.resize(buckets.len(), 0.0);
    let f = q.rollup.map(|r| r.0).unwrap_or(match kind {
        Kind::Count => Rollup::Sum,
        _ => Rollup::Avg,
    });
    let empty = Agg { n: 0, sum: f64::NAN, min: f64::NAN, max: f64::NAN, first: f64::NAN, last: f64::NAN, inc: f64::NAN };
    // A counter's last finite sample before the window.
    let mut prev: Option<f64> = None;
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let raw = ts.get(i).map(|&t| t.div_euclid(width).saturating_mul(width));
        let up = rolled.start.get(j).copied();
        let (start, w) = match (raw, up) {
            (None, None) => break,
            (Some(r), Some(u)) if r < u => (r, None),
            (Some(r), None) => (r, None),
            (_, Some(u)) => (u, rolled.agg(j)),
        };
        let w = match w {
            Some(agg) => {
                j += 1;
                let cross = prev.map_or(f64::NAN, |p| increase_into(p, agg.first));
                prev = Some(agg.last);
                Window { has: true, agg, cross }
            }
            None if up == Some(start) => {
                // A window rolled up without a count: not one a reader keeps.
                j += 1;
                continue;
            }
            None => {
                // The window's samples, folded as a rollup folds them; a counter's from the sample
                // before.
                let end = start.saturating_add(width);
                let mut w = Window { has: false, agg: empty, cross: f64::NAN };
                let mut first = true;
                while let (Some(&t), Some(&v)) = (ts.get(i), vs.get(i)) {
                    if t >= end {
                        break;
                    }
                    i += 1;
                    w.has = true;
                    if kind == Kind::Counter {
                        if let Some(p) = prev {
                            let inc = increase_into(p, v);
                            if first {
                                w.cross = inc;
                            } else {
                                w.agg.inc = rollup::join(w.agg.inc, inc);
                            }
                        }
                        first = false;
                        if v.is_finite() {
                            prev = Some(v);
                        }
                        continue;
                    }
                    if !v.is_finite() {
                        continue;
                    }
                    let a = &mut w.agg;
                    a.n += 1;
                    a.sum = rollup::join(a.sum, v);
                    a.min = if a.min.is_nan() { v } else { a.min.min(v) };
                    a.max = if a.max.is_nan() { v } else { a.max.max(v) };
                    a.last = v;
                }
                w
            }
        };
        let Some(b) = spans.index(start) else { continue };
        let (Some(slot), Some(count)) = (buckets.get_mut(b), n.get_mut(b)) else { continue };
        if kind == Kind::Counter {
            *slot = rollup::join(*slot, rollup::join(w.cross, w.agg.inc));
            continue;
        }
        let a = w.agg;
        match f {
            Rollup::Avg | Rollup::Sum => *slot = rollup::join(*slot, a.sum),
            Rollup::Min if !a.min.is_nan() => *slot = if slot.is_nan() { a.min } else { slot.min(a.min) },
            Rollup::Max if !a.max.is_nan() => *slot = if slot.is_nan() { a.max } else { slot.max(a.max) },
            Rollup::Last if !a.last.is_nan() => *slot = a.last,
            Rollup::Count if w.has => *slot = (if slot.is_nan() { 0.0 } else { *slot }) + f64::from(a.n),
            _ => {}
        }
        *count += f64::from(a.n);
    }
    match (kind, f) {
        (Kind::Counter, _) => {
            if q.mode != Some(CountMode::Count) {
                for (i, b) in buckets.iter_mut().enumerate() {
                    *b /= spans.width_s(i);
                }
            }
        }
        (_, Rollup::Avg) => {
            for (b, &c) in buckets.iter_mut().zip(n.iter()) {
                *b = if c > 0.0 { *b / c } else { f64::NAN };
            }
        }
        _ => {}
    }
    if kind == Kind::Count && q.mode == Some(CountMode::Rate) {
        for (i, b) in buckets.iter_mut().enumerate() {
            *b /= spans.width_s(i);
        }
    }
}

/// A counter's increase into a sample from the one before (`crate::rollup::increase`).
#[inline]
fn increase_into(p: f64, v: f64) -> f64 {
    rollup::increase(p, v)
}

/// One series' samples into `buckets` (NaN where none): its rollup function, by its kind unless
/// the query names one; for counts and counters, a total a bucket or a rate a second. `counts`:
/// each bucket's finite samples, for `.weighted()`.
fn rollup(kind: Kind, q: &MetricQuery, spans: Spans<'_>, ts: &[i64], vs: &[f64], buckets: &mut [f64], mut counts: Option<&mut Vec<f64>>) {
    for b in buckets.iter_mut() {
        *b = f64::NAN;
    }
    if let Some(c) = counts.as_deref_mut() {
        c.iter_mut().for_each(|c| *c = 0.0);
    }
    let bucket_of = |t: i64| spans.index(t);
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
                for (i, b) in buckets.iter_mut().enumerate() {
                    *b /= spans.width_s(i);
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
                // The run of samples in bucket b (the samples come in time order).
                let start = i;
                let next = spans.start(b + 1);
                while ts.get(i).is_some_and(|&t2| t2 < next) {
                    i += 1;
                }
                let run = vs.get(start..i).unwrap_or(&[]);
                if let Some(c) = counts.as_deref_mut().and_then(|c| c.get_mut(b)) {
                    *c = run.iter().filter(|v| v.is_finite()).count() as f64;
                }
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
                for (i, b) in buckets.iter_mut().enumerate() {
                    *b /= spans.width_s(i);
                }
            }
        }
    }
}

/// A group's space aggregation, a bucket at a time.
enum Acc {
    /// Sum and count for avg, sum and count (each series' value times its weight, and the weights:
    /// one a series, unless `.weighted()`); min and max as their own.
    Moments { kind: SpaceAgg, a: Vec<f64>, n: Vec<f64> },
    /// Every series' value a bucket, as (bucket, value), for a percentile.
    Values { pct: f64, n: usize, v: Vec<(u32, f64)> },
}

impl Acc {
    fn new(kind: SpaceAgg, n: usize) -> Self {
        match kind {
            SpaceAgg::Pct(p) => Self::Values { pct: p, n, v: Vec::new() },
            k => Self::Moments { kind: k, a: vec![f64::NAN; n], n: vec![0.0; n] },
        }
    }

    /// A series' values, each weighing its weight (one unless given).
    fn add(&mut self, buckets: &[f64], weights: Option<&[f64]>) {
        for (i, &v) in buckets.iter().enumerate() {
            if !v.is_nan() {
                self.put(i, v, weights.and_then(|w| w.get(i)).copied().unwrap_or(1.0));
            }
        }
    }

    /// A series' value in bucket `i`, weighing `w`: a sum's and an average's share, a count's;
    /// a minimum, a maximum and a percentile take the values as they are.
    fn put(&mut self, i: usize, v: f64, w: f64) {
        match self {
            Self::Moments { kind, a, n } => {
                let (Some(x), Some(c)) = (a.get_mut(i), n.get_mut(i)) else { return };
                *c += w;
                *x = match kind {
                    SpaceAgg::Min if !x.is_nan() => x.min(v),
                    SpaceAgg::Max if !x.is_nan() => x.max(v),
                    SpaceAgg::Min | SpaceAgg::Max => v,
                    _ if x.is_nan() => v * w,
                    _ => *x + v * w,
                };
            }
            Self::Values { v: vals, n, .. } => {
                if i < *n {
                    vals.push((i as u32, v));
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
                    SpaceAgg::Avg if c > 0.0 => x / c,
                    SpaceAgg::Count => {
                        if c > 0.0 {
                            c
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
    // A gap's size is from the point before it to the point after, in buckets (an end's, from its
    // edge): `limit` seconds' worth at the most.
    let limit_buckets = |limit: Option<i64>| usize::try_from(limit.unwrap_or(super::parse::FILL_LIMIT_S).saturating_mul(1000) / grid.step.max(1)).unwrap_or(usize::MAX);
    match fill {
        Fill::Null => {}
        Fill::Zero(limit) => {
            let max = limit_buckets(limit);
            let mut i = 0;
            while i < v.len() {
                if !v.get(i).is_some_and(|x| x.is_nan()) {
                    i += 1;
                    continue;
                }
                let start = i;
                while v.get(i).is_some_and(|x| x.is_nan()) {
                    i += 1;
                }
                if i - start < max {
                    v.get_mut(start..i).into_iter().flatten().for_each(|x| *x = 0.0);
                }
            }
        }
        Fill::Last(limit) => {
            let max = limit_buckets(limit);
            let mut prev: Option<(usize, f64)> = None;
            let mut i = 0;
            while i < v.len() {
                match v.get(i) {
                    Some(x) if !x.is_nan() => {
                        prev = Some((i, *x));
                        i += 1;
                    }
                    _ => {
                        let start = i;
                        while v.get(i).is_some_and(|x| x.is_nan()) {
                            i += 1;
                        }
                        if let Some((a, last)) = prev
                            && i - a <= max
                        {
                            v.get_mut(start..i).into_iter().flatten().for_each(|x| *x = last);
                        }
                    }
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
        BinOp::Pow => x.powf(y),
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
    // A bucket's width: the step's, or a calendar period's own (a month's days, a day's hours where
    // the clocks change).
    let edges = grid.edges(0);
    let spans = Spans::new(grid, 0, edges.as_deref());
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
            for s in &mut set {
                let mut prev = f64::NAN;
                for (i, v) in s.values.iter_mut().enumerate() {
                    let per = if f == Func::Diff { 1.0 } else { spans.width_s(i.saturating_sub(1)) };
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
                for (i, v) in s.values.iter_mut().enumerate() {
                    *v = *v / spans.width_s(i) * unit;
                }
            }
        }
        Func::Cumsum | Func::Integral => {
            for s in &mut set {
                let mut sum = 0.0;
                for (i, v) in s.values.iter_mut().enumerate() {
                    if !v.is_nan() {
                        sum += *v * if f == Func::Integral { spans.width_s(i) } else { 1.0 };
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
            let measure = text(args.get(1), "mean");
            let by = crate::query::parse::Rank::parse(&measure).ok_or_else(|| QueryError::Invalid(format!("top's measure: {measure} (mean, min, max, last, sum, area, l2norm)")))?;
            let desc = text(args.get(2), if f == Func::Top { "desc" } else { "asc" }) == "desc";
            set = ranked(set, n, by, desc);
        }
        // Evaluated apart (`anomalies`, `forecast`: they read their query's history; the shifts:
        // they move their query's reading; `pow`: either side a query or a number).
        Func::Timeshift | Func::Anomalies | Func::Forecast | Func::Pow | Func::HourBefore | Func::DayBefore | Func::WeekBefore | Func::MonthBefore | Func::CalendarShift => {}
        Func::Round => {
            let places = match args.first() {
                Some(v) => num(Some(v), "the decimal places")?,
                None => 0.0,
            };
            let k = 10f64.powi(places.round().clamp(-15.0, 15.0) as i32);
            for s in &mut set {
                for v in &mut s.values {
                    *v = (*v * k).round() / k;
                }
            }
        }
        Func::Ceil | Func::Floor => {
            for s in &mut set {
                for v in &mut s.values {
                    *v = if f == Func::Ceil { v.ceil() } else { v.floor() };
                }
            }
        }
        Func::CountNonzero | Func::CountNotNull => {
            // At each point, how many of the group's series are (non-zero and) finite: one series.
            let mut counts = vec![0.0; grid.n];
            for s in &set {
                for (c, v) in counts.iter_mut().zip(&s.values) {
                    if v.is_finite() && (f == Func::CountNotNull || *v != 0.0) {
                        *c += 1.0;
                    }
                }
            }
            set = vec![Series { tags: Vec::new(), values: counts, bounds: None, outlier: None }];
        }
        Func::ExcludeNull => set.retain(|s| s.tags.iter().all(|(_, v)| v != "N/A")),
        Func::ClampMin | Func::ClampMax | Func::CutoffMin | Func::CutoffMax => {
            let t = num(args.first(), "the threshold")?;
            for s in &mut set {
                for v in &mut s.values {
                    *v = match f {
                        // A missing value is the bound too, as Datadog's page says.
                        Func::ClampMin => if v.is_nan() || *v < t { t } else { *v },
                        Func::ClampMax => if v.is_nan() || *v > t { t } else { *v },
                        Func::CutoffMin => if *v < t { f64::NAN } else { *v },
                        _ => if *v > t { f64::NAN } else { *v },
                    };
                }
            }
        }
        Func::DefaultZero => {
            for s in &mut set {
                for v in &mut s.values {
                    if v.is_nan() {
                        *v = 0.0;
                    }
                }
            }
        }
        Func::Dt => {
            // The seconds since the point before, of any of the group's series: one series.
            let mut dt = vec![f64::NAN; grid.n];
            let mut prev: Option<usize> = None;
            for (i, d) in dt.iter_mut().enumerate() {
                if set.iter().any(|s| s.values.get(i).is_some_and(|v| v.is_finite())) {
                    if let Some(p) = prev {
                        *d = spans.start(i).saturating_sub(spans.start(p)) as f64 / 1000.0;
                    }
                    prev = Some(i);
                }
            }
            set = vec![Series { tags: Vec::new(), values: dt, bounds: None, outlier: None }];
        }
        Func::MonotonicDiff => {
            for s in &mut set {
                let mut prev = f64::NAN;
                for v in &mut s.values {
                    let cur = *v;
                    let d = cur - prev;
                    // A fall (a counter's reset) is no delta.
                    *v = if d >= 0.0 { d } else { f64::NAN };
                    if !cur.is_nan() {
                        prev = cur;
                    }
                }
            }
        }
        Func::Throughput => {
            for s in &mut set {
                for (i, v) in s.values.iter_mut().enumerate() {
                    *v /= spans.width_s(i);
                }
            }
        }
        Func::RobustTrend | Func::TrendLine => {
            for s in &mut set {
                if let Some((a, b)) = line(&s.values, f == Func::RobustTrend) {
                    for (i, v) in s.values.iter_mut().enumerate() {
                        *v = a + b * i as f64;
                    }
                }
            }
        }
        Func::PiecewiseConstant => {
            for s in &mut set {
                s.values = piecewise_constant(&s.values);
            }
        }
        Func::Autosmooth => {
            // One span for the group (Datadog's), ASAP's for the group's mean series.
            let mean: Vec<f64> = (0..grid.n)
                .map(|i| {
                    let (sum, c) = set.iter().filter_map(|s| s.values.get(i)).filter(|v| v.is_finite()).fold((0.0, 0u32), |(a, c), v| (a + v, c + 1));
                    if c == 0 { f64::NAN } else { sum / f64::from(c) }
                })
                .collect();
            let w = asap_span(&mean);
            for s in &mut set {
                s.values = rolling(&s.values, w, |win| win.iter().sum::<f64>() / win.len() as f64);
            }
        }
        Func::Median(n) => {
            for s in &mut set {
                s.values = rolling(&s.values, usize::from(n), |win| {
                    win.sort_unstable_by(f64::total_cmp);
                    let m = win.len() / 2;
                    if win.len() % 2 == 1 { win.get(m).copied().unwrap_or(f64::NAN) } else { (win.get(m - 1).copied().unwrap_or(f64::NAN) + win.get(m).copied().unwrap_or(f64::NAN)) / 2.0 }
                });
            }
        }
        Func::RollingAvg(n) => {
            for s in &mut set {
                s.values = rolling(&s.values, usize::from(n), |win| win.iter().sum::<f64>() / win.len() as f64);
            }
        }
        Func::EwmaSpan(n) => ewma(&mut set, f64::from(n)),
        Func::Ranked { n, bottom, by } => set = ranked(set, usize::from(n), by, !bottom),
        Func::Outliers => {
            let usage = || QueryError::Invalid("outliers(query, 'dbscan'|'mad'|'scaleddbscan'|'scaledmad', tolerance, pct)".into());
            let algo = match args.first() {
                Some(Value::Str(a)) => crate::outlier::Algorithm::parse(a).ok_or_else(usage)?,
                None => crate::outlier::Algorithm::Dbscan,
                _ => return Err(usage()),
            };
            let tolerance = match args.get(1) {
                Some(Value::Scalar(t)) if t.is_finite() && *t > 0.0 => *t,
                None => 3.0,
                _ => return Err(usage()),
            };
            let pct = match args.get(2) {
                Some(Value::Scalar(p)) if (0.0..=100.0).contains(p) => *p,
                None => 20.0,
                _ => return Err(usage()),
            };
            let rows: Vec<&[f64]> = set.iter().map(|s| s.values.as_slice()).collect();
            let flags = crate::outlier::outliers(&rows, algo, tolerance, pct);
            for (s, f) in set.iter_mut().zip(flags) {
                s.outlier = Some(f);
            }
        }
        Func::MovingAvg => {
            // `moving_rollup(query, seconds, aggregator)`: the last seconds' points combined.
            let secs = num(args.first(), "the window's seconds")?;
            let how = text(args.get(1), "avg");
            if !matches!(how.as_str(), "avg" | "sum" | "min" | "max" | "count") {
                return Err(QueryError::Invalid(format!("moving_rollup's aggregator: {how} (avg, sum, min, max, count)")));
            }
            let k = ((secs * 1000.0 / grid.step as f64).round() as usize).max(1);
            for s in &mut set {
                let src = s.values.clone();
                for (i, v) in s.values.iter_mut().enumerate() {
                    let win = src.get(i.saturating_sub(k - 1)..=i).unwrap_or(&[]).iter().copied().filter(|x| !x.is_nan());
                    *v = match how.as_str() {
                        "sum" => win.fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x)).unwrap_or(f64::NAN),
                        "min" => win.reduce(f64::min).unwrap_or(f64::NAN),
                        "max" => win.reduce(f64::max).unwrap_or(f64::NAN),
                        "count" => win.count() as f64,
                        _ => {
                            let (sum, c) = win.fold((0.0, 0u32), |(s, c), x| (s + x, c + 1));
                            if c == 0 { f64::NAN } else { sum / f64::from(c) }
                        }
                    };
                }
            }
        }
        Func::Ewma => {
            // `ewma(query)`: Datadog's default span, 20.
            let span = match args.first() {
                Some(v) => num(Some(v), "the span")?,
                None => 20.0,
            };
            if span < 1.0 {
                return Err(QueryError::Invalid("ewma: a span of at least 1".into()));
            }
            ewma(&mut set, span);
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
    /// Threads every query reads its series on (a test's: none said, as the store chooses).
    static THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    pub(super) fn threads() -> Option<usize> {
        match THREADS.load(std::sync::atomic::Ordering::SeqCst) {
            0 => None,
            n => Some(n),
        }
    }

    /// Queries over many series read on one thread and on several (as many as a round holds, and
    /// rounds after rounds): the same answers to the bit, the values summed in one order.
    #[test]
    fn a_query_read_on_many_threads_answers_as_on_one_to_the_bit() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut rng = crate::testkit::Rng::new(29);
        let mut b = Builder::new();
        for i in 0..700u32 {
            let host = format!("h{}", i % 5);
            let kind = [Kind::Gauge, Kind::Count, Kind::Counter][i as usize % 3];
            let k = SeriesKey::new(["a", "b", "c"][i as usize % 3], [("host", host.as_str()), ("s", i.to_string().as_str())], kind).unwrap();
            b.define(&k);
            let mut c = 0.0;
            // Values whose sums depend on their order: twelve orders of magnitude apart.
            let mut pts = Vec::new();
            for j in 0..400i64 {
                if rng.below(9) == 0 {
                    continue;
                }
                c += rng.f64() * 1e6;
                pts.push((j * 9_000, if kind == Kind::Counter { c } else { rng.f64() * 10f64.powi(rng.below(12) as i32) }));
            }
            b.run(k.fp, pts);
        }
        s.apply(&b.finish()).unwrap();
        let queries = ["sum:a{*} by {host}", "avg:a{*}", "sum:b{*} by {host}.as_count()", "sum:c{*} by {host}.as_rate()", "max:a{*} by {host}", "p90:a{*}", "sum:a{*} by {host}.rollup(sum, 300)"];
        for q in queries {
            let mut r = Request::new(q, 0, 3_600_000);
            r.step = Some(if q.contains("300") { 300_000 } else { 30_000 });
            THREADS.store(1, std::sync::atomic::Ordering::SeqCst);
            let one = eval(&s, &r).unwrap();
            for threads in [2usize, 3, 7] {
                THREADS.store(threads, std::sync::atomic::Ordering::SeqCst);
                let many = eval(&s, &r).unwrap();
                assert_eq!(many.series.len(), one.series.len(), "{q}");
                for (a, b) in many.series.iter().zip(&one.series) {
                    assert_eq!(a.tags, b.tags, "{q}");
                    assert_eq!(a.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), b.values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), "{q} on {threads} threads");
                }
                assert_eq!((many.scanned_series, many.scanned_samples), (one.scanned_series, one.scanned_samples), "{q}");
            }
        }
        THREADS.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    use super::*;
    use crate::batch::{Builder, SeriesKey};
    use crate::query::calendar::{civil_from_days, days_from_civil};
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

    /// Rollups change no answer: three replicas given the same writes, two cutting and merging
    /// their blocks on schedules of their own (so a window one holds rolled up the other reads
    /// as samples) and one holding everything in its head, answer every query whose buckets are
    /// whole windows to the bit, rollups read or not: gauges of noise, counts of fractions,
    /// counters that reset, samples that are not finite, writes late into merged time and late
    /// writes not yet merged, a distribution; steps of five minutes to three hours, calendar days
    /// in three timezones (Kathmandu's +5:45 a five-minute tier's, New York's an hour's), every
    /// rollup function, shifts.
    #[test]
    fn rollups_answer_to_the_bit_as_samples_do_whatever_the_blocks() {
        // `KILN_OBS_SEEDS=n` runs n seeds, each its own writes and cut schedules; one by default.
        let seeds: u64 = std::env::var("KILN_OBS_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
        for seed in 0..seeds {
            rollups_alike(seed);
        }
    }

    fn rollups_alike(seed: u64) {
        let start: i64 = 1_700_006_400_000; // a UTC midnight
        let hours = 50i64;
        let mut rng = crate::testkit::Rng::new(41 + seed);
        let schedules = [5 + (seed % 3) as usize, 2 + (seed % 2) as usize];
        // An hour of writes a batch; at the end, late writes into hour 10 (merged by then) and hour
        // 48 (still in the head).
        let mut batches = Vec::new();
        let keys: Vec<SeriesKey> = vec![
            SeriesKey::new("noise", [("host", "a")], Kind::Gauge).unwrap(),
            SeriesKey::new("noise", [("host", "b")], Kind::Gauge).unwrap(),
            SeriesKey::new("level", [("host", "a")], Kind::Gauge).unwrap(),
            SeriesKey::new("frac", [("host", "a")], Kind::Count).unwrap(),
            SeriesKey::new("frac", [("host", "b")], Kind::Count).unwrap(),
            SeriesKey::new("req", [("host", "a")], Kind::Counter).unwrap(),
            SeriesKey::new("req", [("host", "b")], Kind::Counter).unwrap(),
        ];
        let mut counters = [0.0f64; 2];
        for h in 0..hours {
            let mut b = Builder::new();
            for k in &keys {
                b.define(k);
            }
            for (ki, k) in keys.iter().enumerate() {
                let mut pts = Vec::new();
                let mut t = start + h * 3_600_000;
                while t < start + (h + 1) * 3_600_000 {
                    let v = match ki {
                        0 | 1 => rng.f64() * 100.0 - 50.0,
                        2 => 7.25,
                        3 | 4 => 0.1 * (1 + rng.below(9)) as f64,
                        _ => {
                            let c = &mut counters[ki - 5];
                            *c = if rng.below(700) == 0 { rng.f64() } else { *c + rng.f64() * 3.0 };
                            *c
                        }
                    };
                    let v = if rng.below(3_000) == 0 { f64::NAN } else { v };
                    pts.push((t, v));
                    // Ten seconds, jittered, a gap now and then.
                    t += 10_000 + rng.below(7) as i64 - 3 + if rng.below(400) == 0 { 600_000 } else { 0 };
                }
                b.run(k.fp, pts);
            }
            // A distribution beside them: each host's values of the hour, a minute at a time.
            for host in ["a", "b"] {
                let points: Vec<(i64, Vec<f64>)> = (0..60).map(|m| (start + h * 3_600_000 + m * 60_000, (0..20).map(|_| (rng.f64() * 6.0).exp()).collect())).collect();
                for (key, pts) in crate::dist::series_of("lat", &[("host", host)], &points).unwrap() {
                    b.define(&key);
                    b.run(key.fp, pts);
                }
            }
            batches.push(b.finish());
        }
        for late_hour in [10i64, 48] {
            let mut b = Builder::new();
            for k in keys.iter().take(1) {
                b.define(k);
                b.run(k.fp, (0..30).map(|i| (start + late_hour * 3_600_000 + 1_234 + i * 61_000, rng.f64())));
            }
            batches.push(b.finish());
        }
        let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
        let mut stores: Vec<Store> = dirs.iter().map(|d| Store::open(d.path(), Keyed::with_keys(1, 2)).unwrap()).collect();
        stores.push(Store::new(Keyed::with_keys(1, 2)));
        for (i, batch) in batches.iter().enumerate() {
            for (si, st) in stores.iter_mut().enumerate() {
                st.apply(batch).unwrap();
                let every = schedules.get(si).copied();
                // The last late write stays in the head: a source overlapping a block's.
                if let Some(e) = every
                    && i % e == e - 1
                    && i + 1 < batches.len()
                {
                    st.cut(crate::manifest::Point { index: i as u64 + 1, term: 1 }).unwrap();
                    st.compact().unwrap();
                }
            }
        }
        // Rollups are there to be read: some blocks hold each tier of the gauges, and some tier
        // of every kind.
        for (metric, kind) in [("noise", Kind::Gauge), ("frac", Kind::Count), ("req", Kind::Counter), ("lat.dist", Kind::Count)] {
            let mut found = [0usize; 2];
            for (t, &w) in rollup::TIERS.iter().enumerate() {
                let mut rolled = Rolled::default();
                for st in &stores[..2] {
                    let mut ids = Vec::new();
                    st.select(&Selector::metric(metric), &mut ids);
                    for &id in &ids {
                        let (mut ts, mut vs) = (Vec::new(), Vec::new());
                        rolled.clear();
                        let roll = Roll { width: w, from: start, to: start + hours * 3_600_000, kind, cols: rollup::columns(kind) };
                        st.read_rolled(id, start, start + hours * 3_600_000, &roll, &mut Scratch::default(), &mut ts, &mut vs, &mut rolled).unwrap();
                        found[t] += rolled.len();
                    }
                }
            }
            assert!(found.iter().any(|&n| n > 0), "{metric}: no window read from rollups");
            if metric == "noise" {
                assert!(found.iter().all(|&n| n > 0), "{metric}: a tier never read from rollups: {found:?}");
            }
        }
        let queries = [
            "avg:noise{*}",
            "sum:noise{*} by {host}",
            "max:noise{*}.rollup(min)",
            "min:noise{*}.rollup(max)",
            "avg:noise{*}.rollup(last)",
            "sum:noise{*}.rollup(count)",
            "avg:noise{*}.rollup(sum)",
            "p90:noise{*}",
            "avg:level{*}",
            "sum:frac{*}",
            "sum:frac{*}.as_rate()",
            "sum:frac{*} by {host}.rollup(avg)",
            "sum:req{*}",
            "sum:req{*}.as_count()",
            "sum:req{*} by {host}",
            "p99:lat{*}",
            "p50:lat{*} by {host}",
            "avg:lat{*}",
            "timeshift(avg:noise{*}, -3600)",
            "avg:noise{*} - timeshift(avg:noise{*}, -86400)",
        ];
        let same = |a: f64, b: f64| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan());
        let mut checked = 0usize;
        for step in [300_000i64, 600_000, 3_600_000, 10_800_000] {
            for query in queries {
                let mut r = Request::new(query, start + 3 * 3_600_000 + 17, start + hours * 3_600_000);
                r.step = Some(step);
                let mut answers = Vec::new();
                for st in stores.iter_mut() {
                    for on in [true, false] {
                        st.set_read_rollups(on);
                        answers.push(eval(st, &r).unwrap());
                    }
                    st.set_read_rollups(true);
                }
                let first = &answers[0];
                assert!(first.series.iter().any(|s| s.values.iter().any(|v| v.is_finite())), "{query} at {step}: nothing to compare");
                for (n, other) in answers.iter().enumerate().skip(1) {
                    assert_eq!(other.series.len(), first.series.len(), "{query} at {step}, answer {n}");
                    assert_eq!(other.scanned_samples, first.scanned_samples, "{query} at {step}, answer {n}");
                    for (x, y) in first.series.iter().zip(&other.series) {
                        assert_eq!(x.tags, y.tags);
                        for (i, (a, b)) in x.values.iter().zip(&y.values).enumerate() {
                            assert!(same(*a, *b), "{query} at {step}, answer {n} (store {}, rollups {}), bucket {i}: {a} against {b}", n / 2, n % 2 == 0);
                        }
                    }
                }
                checked += 1;
            }
        }
        // Calendar days: whole hours in UTC and New York, whole quarter hours in Kathmandu.
        for tz in ["UTC", "America/New_York", "Asia/Kathmandu"] {
            for query in ["avg:noise{*}", "sum:frac{*}", "sum:req{*}", "p99:lat{*}"] {
                let full = format!("{query}.rollup({}, daily, '{tz}')", if query.starts_with("avg") { "avg" } else { "sum" });
                let r = Request::new(&full, start, start + hours * 3_600_000);
                let mut answers = Vec::new();
                for st in stores.iter_mut() {
                    for on in [true, false] {
                        st.set_read_rollups(on);
                        answers.push(eval(st, &r).unwrap());
                    }
                    st.set_read_rollups(true);
                }
                for other in &answers[1..] {
                    for (x, y) in answers[0].series.iter().zip(&other.series) {
                        assert!(x.values.iter().zip(&y.values).all(|(a, b)| same(*a, *b)), "{full}: {:?} against {:?}", x.values, y.values);
                    }
                }
                checked += 1;
            }
        }
        assert_eq!(checked, 4 * queries.len() + 12);
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
        assert_eq!(a.grid, Grid::uniform(0, 60_000, 10));
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
        // A 300 s gap: past a limit of 120, none of it; within one of 300, all of it.
        assert!(q(&s, "avg:gap{*}.fill(last, 120)").series[0].values[1..5].iter().all(|v| v.is_nan()));
        assert!(q(&s, "avg:gap{*}.fill(last, 300)").series[0].values[1..5].iter().all(|&v| v == 0.0));
        // A gap wider than a fill's limit (300 s unless said) stays a gap, whatever the fill.
        let mut b = Builder::new();
        let k = SeriesKey::new("wide", [], Kind::Gauge).unwrap();
        b.define(&k);
        b.run(k.fp, [(0, 0.0), (420_000, 70.0)]);
        s.apply(&b.finish()).unwrap();
        for how in ["linear", "last", "zero"] {
            assert!(q(&s, &format!("avg:wide{{*}}.fill({how})")).series[0].values[1].is_nan(), "{how}");
        }
        assert_eq!(q(&s, "avg:wide{*}.fill(linear, 420)").series[0].values[1], 10.0);
        assert_eq!(q(&s, "avg:wide{*}.fill(zero, 420)").series[0].values[1], 0.0);
        assert_eq!(q(&s, "default_zero(avg:wide{*})").series[0].values[1], 0.0);
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

    /// Ten days of a daily cycle every half hour, then a forecast over a range a day past now: up to
    /// now the series itself, past it the cycle forecast (within a few percent of what the cycle
    /// will do), its band about it; a ramp's linear forecast carries the ramp on.
    #[test]
    fn a_forecast_carries_a_season_and_a_trend_past_now() {
        let mut s = Store::new(Keyed::with_keys(5, 6));
        let mut b = Builder::new();
        let now: i64 = 1_800_000_000_000 / 1_800_000 * 1_800_000;
        let cycle = |t: i64| 50.0 + 20.0 * (t as f64 / 86_400_000.0 * std::f64::consts::TAU).sin();
        let ramp = |t: i64| 10.0 + (t - now) as f64 / 3_600_000.0;
        let (load, disk) = (SeriesKey::new("load", [("host", "a")], Kind::Gauge).unwrap(), SeriesKey::new("disk", [("host", "a")], Kind::Gauge).unwrap());
        b.define(&load);
        b.define(&disk);
        let n = 48 * 10;
        b.run(load.fp, (0..n).map(|i| {
            let t = now - (n - 1 - i) * 1_800_000;
            (t, cycle(t))
        }));
        b.run(disk.fp, (0..n).map(|i| {
            let t = now - (n - 1 - i) * 1_800_000;
            (t, ramp(t))
        }));
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str| {
            let mut r = Request::new(q, now - 86_400_000, now + 86_400_000);
            r.step = Some(1_800_000);
            r.now = Some(now);
            eval(&s, &r).unwrap()
        };
        let a = ask("forecast(avg:load{*}, 'seasonal', 2, seasonality='daily')");
        let [one] = a.series.as_slice() else { panic!("one series") };
        let b = one.bounds.as_ref().expect("a forecast's band");
        let from = b.forecast_from.expect("where the forecast starts");
        assert_eq!(a.grid.time(from - 1), now, "data to now, forecast after");
        assert_eq!(b.algorithm, "seasonal");
        for i in 0..a.grid.n {
            let t = a.grid.time(i);
            let (v, lo, hi) = (one.values[i], b.lower[i], b.upper[i]);
            if i < from {
                assert!((v - cycle(t)).abs() < 1e-9 && lo.is_nan(), "data at {i}");
            } else {
                assert!((v - cycle(t)).abs() < 2.0, "the forecast at {i}: {v} for {}", cycle(t));
                assert!(lo <= v && v <= hi, "its band at {i}");
            }
        }
        let d = ask("forecast(avg:disk{*}, 'linear', 2, model='default')");
        let [one] = d.series.as_slice() else { panic!("one series") };
        let last = one.values[d.grid.n - 1];
        // The ramp a day on: 34; half the last value (Naive) and half Holt's, the default's.
        assert!(last > 20.0 && last <= ramp(d.grid.time(d.grid.n - 1)) + 1.0, "{last}");
        let r = ask("forecast(avg:disk{*}, 'linear', 2, model='reactive', history='2d')");
        let last = r.series[0].values[r.grid.n - 1];
        assert!((last - ramp(r.grid.time(r.grid.n - 1))).abs() < 1.0, "the reactive model carries the ramp on: {last}");
    }

    #[test]
    fn named_arguments_are_forecast_s_and_its_own_are_checked() {
        let s = store();
        let r = |q: &str| {
            let mut r = Request::new(q, 0, 600_000);
            r.now = Some(600_000);
            eval(&s, &r)
        };
        assert!(r("forecast(cpu{*}, 'linear', 1, model='reactive', history='1h')").is_ok());
        assert!(r("forecast(cpu{*}, 'linear', 1, model = 'simple')").is_ok());
        for bad in [
            "top(cpu{*} by {host}, 2, 'mean', model='reactive')",
            "forecast(cpu{*}, 'linear', 1, model='psychic')",
            "forecast(cpu{*}, 'quadratic', 1)",
            "forecast(cpu{*}, 'linear', 0)",
            "forecast(cpu{*}, 'linear', 1, history='forever')",
            "forecast(cpu{*}, 'linear', 1, seasonality='yearly')",
            "forecast(cpu{*}, 'linear', 1, timezone='America/New_York')",
            "forecast(cpu{*}, 'linear', 1, mystery=1)",
            // A day's season at a 10-minute range's step is fine; an hour's at a day's step is not.
        ] {
            assert!(r(bad).is_err(), "{bad} was taken");
        }
        let mut coarse = Request::new("forecast(cpu{*}, 'seasonal', 2, seasonality='hourly')", 0, 86_400_000 * 2);
        coarse.step = Some(3_600_000);
        assert!(eval(&s, &coarse).is_err(), "an hourly season at an hour's step");
    }

    /// Five hosts, one running hot: `outliers()` returns every series, the hot one alone said an
    /// outlier, by DBSCAN and by MAD; a wrong algorithm or tolerance is refused.
    #[test]
    fn outliers_says_which_member_is_unlike_its_peers() {
        let mut s = Store::new(Keyed::with_keys(7, 8));
        let mut b = Builder::new();
        let mut seed = 13u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        for (host, level) in [("a", 40.0), ("b", 40.0), ("c", 40.0), ("d", 40.0), ("hot", 70.0)] {
            let k = SeriesKey::new("cpu", [("host", host)], Kind::Gauge).unwrap();
            b.define(&k);
            let pts: Vec<(i64, f64)> = (0..60).map(|i| (i * 60_000, level + 4.0 * noise())).collect();
            b.run(k.fp, pts);
        }
        s.apply(&b.finish()).unwrap();
        for q in ["outliers(avg:cpu{*} by {host}, 'dbscan', 1)", "outliers(avg:cpu{*} by {host}, 'MAD', 3, 20)", "outliers(avg:cpu{*} by {host}, 'scaledMAD', 3, 20)"] {
            let mut r = Request::new(q, 0, 3_600_000);
            r.step = Some(60_000);
            let a = eval(&s, &r).unwrap();
            assert_eq!(a.series.len(), 5, "{q}");
            let flagged: Vec<&str> = a.series.iter().filter(|x| x.outlier == Some(true)).filter_map(|x| x.tags.iter().find(|(k, _)| k == "host").map(|(_, v)| v.as_str())).collect();
            assert_eq!(flagged, ["hot"], "{q}");
        }
        for bad in ["outliers(avg:cpu{*} by {host}, 'kmeans', 1)", "outliers(avg:cpu{*} by {host}, 'dbscan', 0)", "outliers(avg:cpu{*} by {host}, 'mad', 3, 150)"] {
            let r = Request::new(bad, 0, 3_600_000);
            assert!(eval(&s, &r).is_err(), "{bad} was taken");
        }
    }

    /// Datadog's function catalog, each function as its page says, on series whose answers are
    /// known: a ramp, one alternating with zeros, one with gaps, a counter that resets, a step, a
    /// line with one wild point.
    #[test]
    fn datadog_s_functions_do_what_their_pages_say() {
        let mut s = Store::new(Keyed::with_keys(9, 10));
        let mut b = Builder::new();
        let nan = f64::NAN;
        let series: [(&str, &[(&str, &str)], Vec<f64>); 7] = [
            ("ramp", &[("host", "a")], (1..=10).map(f64::from).collect()),
            ("ramp", &[("host", "b")], (0..10).map(|i| if i % 2 == 0 { 10.0 } else { 0.0 }).collect()),
            ("ramp", &[("host", "c"), ("zone", "z1")], vec![2.0, nan, nan, 5.0, 6.0, nan, 8.0, 9.0, 10.0, 11.0]),
            ("ctr", &[("host", "a")], vec![5.0, 10.0, 15.0, 3.0, 8.0, 13.0, 18.0, 23.0, 28.0, 33.0]),
            ("step", &[("host", "a")], vec![1.0, 1.1, 0.9, 1.0, 1.05, 5.0, 5.1, 4.9, 5.0, 5.05]),
            ("wild", &[("host", "a")], (0..10).map(|i| if i == 7 { 100.0 } else { 2.0 * f64::from(i) + 1.0 }).collect()),
            ("long", &[("host", "a")], (0..240).map(f64::from).collect()),
        ];
        for (metric, tags, values) in &series {
            let k = SeriesKey::new(metric, tags.iter().copied(), Kind::Gauge).unwrap();
            b.define(&k);
            let pts: Vec<(i64, f64)> = values.iter().enumerate().filter(|(_, v)| v.is_finite()).map(|(i, &v)| (i as i64 * 60_000, v)).collect();
            b.run(k.fp, pts);
        }
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str, to: i64| -> Vec<(String, Vec<f64>)> {
            let mut r = Request::new(q, 0, to);
            r.step = Some(60_000);
            let a = eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}"));
            let mut out: Vec<(String, Vec<f64>)> = a.series.into_iter().map(|x| (x.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(","), x.values)).collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        };
        let one = |q: &str| -> Vec<f64> {
            let r = ask(q, 600_000);
            assert_eq!(r.len(), 1, "{q}: {r:?}");
            r.into_iter().next().unwrap().1
        };
        let same = |a: &[f64], b: &[f64]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x.is_nan() && y.is_nan()) || (x - y).abs() < 1e-9);
        let a = "avg:ramp{host:a}";
        // Arithmetic.
        assert!(same(&one(&format!("round({a} / 3, 1)")), &[0.3, 0.7, 1.0, 1.3, 1.7, 2.0, 2.3, 2.7, 3.0, 3.3]));
        assert!(same(&one(&format!("ceil({a} / 3)")), &[1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 3.0, 3.0, 3.0, 4.0]));
        assert!(same(&one(&format!("floor({a} / 3)")), &[0.0, 0.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 3.0, 3.0]));
        assert!(same(&one(&format!("pow({a}, 2)")), &(1..=10).map(|i| f64::from(i * i)).collect::<Vec<_>>()));
        assert!(same(&one(&format!("pow(2, {a})")), &(1..=10).map(|i| 2f64.powi(i)).collect::<Vec<_>>()));
        // Count: across the group, at each point.
        assert!(same(&one("count_nonzero(avg:ramp{*} by {host})"), &[3.0, 1.0, 2.0, 2.0, 3.0, 1.0, 3.0, 2.0, 3.0, 2.0]));
        assert!(same(&one("count_not_null(avg:ramp{*} by {host})"), &[3.0, 2.0, 2.0, 3.0, 3.0, 2.0, 3.0, 3.0, 3.0, 3.0]));
        // Exclusion: missing values are the bound to a clamp; equal ones survive a cutoff.
        let c = "avg:ramp{host:c}";
        assert!(same(&one(&format!("clamp_min({c}, 6)")), &[6.0, 6.0, 6.0, 6.0, 6.0, 6.0, 8.0, 9.0, 10.0, 11.0]));
        assert!(same(&one(&format!("clamp_max({c}, 6)")), &[2.0, 6.0, 6.0, 5.0, 6.0, 6.0, 6.0, 6.0, 6.0, 6.0]));
        assert!(same(&one(&format!("cutoff_min({c}, 6)")), &[nan, nan, nan, nan, 6.0, nan, 8.0, 9.0, 10.0, 11.0]));
        assert!(same(&one(&format!("cutoff_max({c}, 6)")), &[2.0, nan, nan, 5.0, 6.0, nan, nan, nan, nan, nan]));
        let by_zone = ask("exclude_null(avg:ramp{*} by {zone})", 600_000);
        assert_eq!(by_zone.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["zone:z1"], "the N/A group left out");
        // Interpolation.
        assert!(same(&one(&format!("default_zero({c})")), &[2.0, 0.0, 0.0, 5.0, 6.0, 0.0, 8.0, 9.0, 10.0, 11.0]));
        // Rate.
        assert!(same(&one(&format!("dt({c})")), &[nan, nan, nan, 180.0, 60.0, nan, 120.0, 60.0, 60.0, 60.0]));
        assert!(same(&one("monotonic_diff(avg:ctr{*})"), &[nan, 5.0, 5.0, nan, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0]));
        assert!(same(&one(&format!("throughput({a})")), &(1..=10).map(|i| f64::from(i) / 60.0).collect::<Vec<_>>()));
        // Regression: least squares through the line exactly; Huber's barely moved by a wild point,
        // least squares pulled up by it.
        assert!(same(&one(&format!("trend_line({a})")), &(1..=10).map(f64::from).collect::<Vec<_>>()));
        let robust = one("robust_trend(avg:wild{*})");
        let ols = one("trend_line(avg:wild{*})");
        let truth: Vec<f64> = (0..10).map(|i| 2.0 * f64::from(i) + 1.0).collect();
        let err = |fit: &[f64]| fit.iter().zip(&truth).map(|(f, t)| (f - t).abs()).fold(0.0, f64::max);
        assert!(err(&robust) < 0.5 && err(&ols) > 3.0, "Huber {} off, least squares {}", err(&robust), err(&ols));
        // A step, as two levels.
        let pc = one("piecewise_constant(avg:step{*})");
        assert!(pc[..5].iter().all(|v| (v - 1.01).abs() < 1e-9) && pc[5..].iter().all(|v| (v - 5.01).abs() < 1e-9), "{pc:?}");
        // Smoothing: the last n points.
        let m3 = one("median_3(avg:ramp{host:b})");
        assert!(same(&m3, &[10.0, 5.0, 10.0, 0.0, 10.0, 0.0, 10.0, 0.0, 10.0, 0.0]), "{m3:?}");
        assert!(same(&one(&format!("median({a})")), &one(&format!("median_3({a})"))));
        assert!(same(&one(&format!("rollingavg_5({a})")), &[1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]));
        assert!(same(&one(&format!("ewma_3({a})")), &one(&format!("ewma({a}, 3)"))));
        assert!(same(&one(&format!("ewma({a})")), &one(&format!("ewma_20({a})"))));
        assert!(same(&one(&format!("ewma_1({a})")), &(1..=10).map(f64::from).collect::<Vec<_>>()));
        assert!(same(&one(&format!("moving_rollup({a}, 180, 'max')")), &(1..=10).map(f64::from).collect::<Vec<_>>()));
        assert!(same(&one(&format!("moving_rollup({a}, 180, 'sum')")), &[1.0, 3.0, 6.0, 9.0, 12.0, 15.0, 18.0, 21.0, 24.0, 27.0]));
        // Timeshift: the query as it was before, on this range's grid.
        let long = |q: &str| -> Vec<f64> { ask(q, 240 * 60_000).into_iter().next().map(|x| x.1).unwrap_or_default() };
        let shifted = long("hour_before(avg:long{*})");
        assert!(shifted[..60].iter().all(|v| v.is_nan()) && (60..240).all(|i| (shifted[i] - (i as f64 - 60.0)).abs() < 1e-9), "{:?}", &shifted[55..65]);
        assert!(same(&long("calendar_shift(avg:long{*}, '-1d', 'UTC')"), &long("day_before(avg:long{*})")));
        assert!(same(&long("timeshift(avg:long{*}, -3600)"), &shifted));
        // Rank: the shortcuts are top and bottom.
        let alike = |x: &[(String, Vec<f64>)], y: &[(String, Vec<f64>)]| x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.0 == q.0 && same(&p.1, &q.1));
        assert!(alike(&ask("top5_max(avg:ramp{*} by {host})", 600_000), &ask("top(avg:ramp{*} by {host}, 5, 'max', 'desc')", 600_000)));
        assert!(alike(&ask("bottom5(avg:ramp{*} by {host})", 600_000), &ask("top(avg:ramp{*} by {host}, 5, 'mean', 'asc')", 600_000)));
        // In rank order: means b 5, a 5.5, c 51/7.
        let order = |q: &str| -> Vec<String> {
            let mut r = Request::new(q, 0, 600_000);
            r.step = Some(60_000);
            eval(&s, &r).unwrap().series.into_iter().map(|x| x.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect()).collect()
        };
        assert_eq!(order("bottom5(avg:ramp{*} by {host})"), ["host:b", "host:a", "host:c"]);
        assert_eq!(order("top10_mean(avg:ramp{*} by {host})"), ["host:c", "host:a", "host:b"]);
        assert_eq!(order("top(avg:ramp{*} by {host}, 1, 'l2norm', 'desc')"), ["host:b"]);
        // autosmooth: a noisy series is smoothed (a span above one), its trend kept.
        let smooth = ask("autosmooth(avg:long{*})", 240 * 60_000);
        assert_eq!(smooth.len(), 1);
    }

    /// Calendar rollups: a count of one an hour through 2026, rolled up by days, weeks and months,
    /// in UTC and in New York (whose days are 23 and 25 hours where its clocks change).
    #[test]
    fn calendar_rollups_follow_the_calendar_and_its_clocks() {
        const H: i64 = 3_600_000;
        let day = |y, m, d| days_from_civil(y, m, d) * 86_400_000;
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let k = SeriesKey::new("hits", [], Kind::Count).unwrap();
        let mut b = Builder::new();
        b.define(&k);
        b.run(k.fp, (0..366 * 24).map(|h| (day(2026, 1, 1) + h * H, 1.0)));
        // And the hour itself, from 2026's first.
        let k = SeriesKey::new("hour", [], Kind::Gauge).unwrap();
        b.define(&k);
        b.run(k.fp, (0..366 * 24).map(|h| (day(2026, 1, 1) + h * H, h as f64)));
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str, from: i64, to: i64| -> Answer {
            let r = Request::new(q, from, to);
            eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}"))
        };
        let values = |a: &Answer| a.series.first().map(|x| x.values.clone()).unwrap_or_default();
        // UTC days: 24 each, the grid the calendar's.
        let a = ask("sum:hits{*}.rollup(sum, daily)", day(2026, 3, 7), day(2026, 3, 10));
        assert_eq!(values(&a), [24.0, 24.0, 24.0]);
        assert_eq!((a.grid.from, a.grid.n, a.grid.time(3)), (day(2026, 3, 7), 3, day(2026, 3, 10)));
        // New York's: the range's first and last instants are in its 6th and 9th of March (UTC's
        // 7th begins on its 6th); its 8th is 23 hours.
        for q in ["sum:hits{*}.rollup(sum, daily, 'America/New_York')", "sum:hits{*}.rollup(sum, daily, 12am, 'America/New_York')"] {
            let a = ask(q, day(2026, 3, 7), day(2026, 3, 10));
            assert_eq!(values(&a), [24.0, 24.0, 23.0, 24.0], "{q}");
            assert_eq!(a.grid.from, day(2026, 3, 6) + 5 * H, "midnight EST");
            assert_eq!(a.grid.time(4), day(2026, 3, 10) + 4 * H, "midnight EDT");
        }
        let a = ask("sum:hits{*}.rollup(sum, daily, 'America/New_York')", day(2026, 11, 1) + 12 * H, day(2026, 11, 2) + 12 * H);
        assert_eq!(values(&a), [25.0, 24.0]);
        // A rate a second divides by each period's own length: an hour's one whatever the day.
        let a = ask("sum:hits{*}.rollup(sum, daily, 'America/New_York').as_rate()", day(2026, 3, 7), day(2026, 3, 10));
        assert!(values(&a).iter().all(|v| (v * 3600.0 - 1.0).abs() < 1e-12), "{:?}", values(&a));
        // Days begun at 9am: 2026-03-07 00:00 is in the 6th's.
        let a = ask("sum:hits{*}.rollup(sum, daily, 9am)", day(2026, 3, 7), day(2026, 3, 8));
        assert_eq!(a.grid.from, day(2026, 3, 6) + 9 * H);
        assert_eq!(values(&a), [24.0, 24.0]);
        // Weeks begun on Monday; 2026-03-09 is one.
        let a = ask("sum:hits{*}.rollup(sum, weekly, monday)", day(2026, 3, 9), day(2026, 3, 23));
        assert_eq!((a.grid.from, values(&a)), (day(2026, 3, 9), vec![168.0, 168.0]));
        // Sunday's by default; New York's week of the 8th is an hour short.
        let a = ask("sum:hits{*}.rollup(sum, weekly, 'America/New_York')", day(2026, 3, 9), day(2026, 3, 10));
        assert_eq!(values(&a), [167.0]);
        // Months: their own days; per_hour divides each by its own hours.
        let a = ask("sum:hits{*}.rollup(sum, monthly)", day(2026, 1, 1), day(2026, 4, 1));
        assert_eq!(values(&a), [744.0, 672.0, 744.0]);
        assert_eq!(a.grid.step, 2_629_746_000);
        let a = ask("per_hour(sum:hits{*}.rollup(sum, monthly))", day(2026, 1, 1), day(2026, 4, 1));
        assert!(values(&a).iter().all(|v| (v - 1.0).abs() < 1e-12), "{:?}", values(&a));
        // Months begun on the 15th: 2026-01-01 is in December's.
        let a = ask("sum:hits{*}.rollup(sum, monthly, 15th)", day(2026, 1, 1), day(2026, 1, 20));
        assert_eq!(a.grid.from, day(2025, 12, 15));
        assert_eq!(values(&a), [14.0 * 24.0, 31.0 * 24.0]);
        // Every rollup in a query, the same calendar.
        let r = Request::new("sum:hits{*}.rollup(sum, daily) + sum:hits{*}.rollup(sum, weekly)", 0, 86_400_000);
        assert!(eval(&s, &r).is_err());
        let r = Request::new("sum:hits{*}.rollup(sum, daily) + sum:hits{*}.rollup(sum, 60)", 0, 86_400_000);
        assert!(eval(&s, &r).is_err());
        // A year of days fits; a calendar_shift in a timezone moves by its local days.
        assert_eq!(ask("sum:hits{*}.rollup(sum, daily)", day(2026, 1, 1), day(2027, 1, 1)).grid.n, 365);
        let a = ask("calendar_shift(sum:hits{*}.rollup(sum, daily, 'America/New_York'), '-1d', 'America/New_York')", day(2026, 3, 10), day(2026, 3, 10) + H);
        assert_eq!(values(&a), [23.0], "the day before New York's 9th: its 8th");
        // On a grid of hours, each hour reads the same local hour the day before: New York's 9th's
        // 2am reads an hour its 8th never had; its 1am, 25 hours back (EST then), its 3am, 24.
        let hour = |t: i64| ((t - day(2026, 1, 1)) / H) as f64;
        let mut r = Request::new("calendar_shift(avg:hour{*}, '-1d', 'America/New_York')", day(2026, 3, 9) + 5 * H, day(2026, 3, 9) + 8 * H);
        r.step = Some(3_600_000);
        let v = values(&eval(&s, &r).unwrap());
        assert_eq!(v[0], hour(day(2026, 3, 8) + 6 * H), "1am EDT, a day back: 1am EST");
        assert!(v[1].is_nan(), "2am: none the day before");
        assert_eq!(v[2], hour(day(2026, 3, 8) + 7 * H), "3am EDT, a day back: 3am EDT");
        // A month back from a month's last days would read February's 28th thrice: the range's end's
        // shift instead, 31 days, throughout.
        let mut r = Request::new("calendar_shift(avg:hour{*}, '-1mo')", day(2026, 3, 29), day(2026, 4, 1));
        r.step = Some(3_600_000);
        let v = values(&eval(&s, &r).unwrap());
        assert!(v.iter().enumerate().all(|(i, &x)| x == hour(day(2026, 3, 29) + i as i64 * H) - 31.0 * 24.0), "{v:?}");
    }

    /// `weighted()`, Datadog's worked example: pods reporting every 10 s, each living part of a
    /// minute, summed by version, count for the share of the minute they lived.
    #[test]
    fn weighted_counts_a_short_lived_series_for_its_share() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        // (version, value, samples in the minute)
        for (i, (version, value, n)) in [("1", 12.0, 1), ("1", 12.0, 3), ("1", 12.0, 2), ("2", 12.0, 1), ("2", 16.0, 3), ("2", 18.0, 2)].into_iter().enumerate() {
            let app = format!("app{i}");
            let k = SeriesKey::new("pod.uptime", [("version", version), ("app", app.as_str())], Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, (0..n).map(|j| (i64::from(j) * 10_000, value)));
        }
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str| -> Vec<f64> {
            let mut r = Request::new(q, 0, 60_000);
            r.step = Some(60_000);
            eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}")).series.iter().map(|x| x.values[0]).collect()
        };
        assert_eq!(ask("sum:pod.uptime{*} by {version}"), [36.0, 46.0]);
        assert_eq!(ask("sum:pod.uptime{*} by {version}.weighted()"), [12.0, 16.0]);
        // An average weighs each pod by its share: version 2's 16 (its pods' 12, 16, 18 for a
        // sixth, a half, a third), not their plain mean.
        let avg = ask("avg:pod.uptime{*} by {version}.weighted()");
        assert!((avg[0] - 12.0).abs() < 1e-12 && (avg[1] - 16.0).abs() < 1e-12, "{avg:?}");
        assert!((ask("avg:pod.uptime{*} by {version}")[1] - 46.0 / 3.0).abs() < 1e-12);
        // A count: pods alive on average over the minute.
        let n = ask("count:pod.uptime{*} by {version}.weighted()");
        assert!((n[0] - 1.0).abs() < 1e-12 && (n[1] - 1.0).abs() < 1e-12, "{n:?}");
        assert_eq!(ask("max:pod.uptime{*} by {version}.weighted()"), [12.0, 18.0]);
        for bad in ["sum:pod.uptime{*}.rollup(sum).weighted()", "sum:pod.uptime{*}.weighted().rollup(max, 60)"] {
            assert!(eval(&s, &Request::new(bad, 0, 60_000)).is_err(), "{bad}");
        }
    }

    /// A distribution's percentiles across hosts: three hosts' values (log-normal, each its own
    /// level), their buckets written as a distribution's series; `p50`, `p99` of all of them within
    /// the buckets' relative error of the exact ones, by host too; `avg` the sums over the counts;
    /// `count` and `max` its own.
    #[test]
    fn a_distribution_s_percentiles_merge_across_hosts() {
        use crate::dist::{BUCKET_TAG, SCALE, bucket_of, tag_value};
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        let mut rng = crate::testkit::Rng::new(3);
        let mut all: Vec<f64> = Vec::new();
        let mut by_host: Vec<Vec<f64>> = Vec::new();
        for (h, level) in [("a", -4.0), ("b", -3.0), ("c", -2.0)] {
            let mut values = Vec::new();
            for _ in 0..3000 {
                let n = (-2.0 * rng.f64().max(1e-300).ln()).sqrt() * (std::f64::consts::TAU * rng.f64()).cos();
                values.push((n * 0.5 + level).exp());
            }
            // The buckets' counts, as an exporter's delta point at t = 30 s.
            let mut counts: std::collections::BTreeMap<(u8, i64), (crate::dist::Bucket, f64)> = std::collections::BTreeMap::new();
            for &v in &values {
                let bk = bucket_of(v, SCALE).unwrap();
                counts.entry(bk.order()).or_insert((bk, 0.0)).1 += 1.0;
            }
            for (bk, n) in counts.values() {
                let tag = tag_value(SCALE, *bk);
                let k = SeriesKey::new("lat.dist", [("host", h), (BUCKET_TAG, tag.as_str())], Kind::Count).unwrap();
                b.define(&k);
                b.run(k.fp, [(30_000, *n)]);
            }
            for (metric, kind, v) in [
                ("lat.count", Kind::Count, values.len() as f64),
                ("lat.sum", Kind::Count, values.iter().sum::<f64>()),
                ("lat.max", Kind::Gauge, values.iter().copied().fold(f64::MIN, f64::max)),
            ] {
                let k = SeriesKey::new(metric, [("host", h)], kind).unwrap();
                b.define(&k);
                b.run(k.fp, [(30_000, v)]);
            }
            all.extend_from_slice(&values);
            by_host.push(values);
        }
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str| -> Vec<(String, f64)> {
            let mut r = Request::new(q, 0, 60_000);
            r.step = Some(60_000);
            eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}")).series.into_iter().map(|x| (x.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(","), x.values[0])).collect()
        };
        let exact = |v: &mut Vec<f64>, q: f64| {
            v.sort_by(f64::total_cmp);
            v[((v.len() - 1) as f64 * q).floor() as usize]
        };
        let bound = ((2f64).powi(-SCALE) * 0.5).exp2() - 1.0;
        let within = |got: f64, want: f64| ((got - want) / want).abs() <= bound * 1.000_001;
        for (q, p) in [("p50", 0.5), ("p99", 0.99), ("p99.9", 0.999)] {
            let got = ask(&format!("{q}:lat{{*}}"));
            let want = exact(&mut all.clone(), p);
            assert!(got.len() == 1 && within(got[0].1, want), "{q}: {got:?} for {want}");
        }
        let per = ask("p99:lat{*} by {host}");
        for ((tags, got), values) in per.iter().zip(&by_host) {
            let want = exact(&mut values.clone(), 0.99);
            assert!(within(*got, want), "{tags}: {got} for {want}");
        }
        // The average is the sums over the counts, exactly; count and max the distribution's own.
        let avg = ask("avg:lat{*}")[0].1;
        let mean = all.iter().sum::<f64>() / all.len() as f64;
        assert!((avg - mean).abs() < 1e-9 * mean, "{avg} {mean}");
        assert_eq!(ask("count:lat{*}")[0].1, 9000.0);
        assert_eq!(ask("max:lat{*}")[0].1, all.iter().copied().fold(f64::MIN, f64::max));
        // A metric of its own name is no distribution: its series are read as they are.
        assert!(ask("p99:lat.count{*}")[0].1 > 0.0);
    }

    /// A distribution kept without its buckets (a metric from logs without percentiles): its
    /// average, count, sum, least and most are read from its parts; a percentile, nothing.
    #[test]
    fn a_distribution_without_buckets_answers_all_but_percentiles() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        for (h, n, sum, min, max) in [("a", 4.0, 10.0, 1.0, 4.0), ("b", 1.0, 30.0, 30.0, 30.0)] {
            for (metric, kind, v) in [("took.count", Kind::Count, n), ("took.sum", Kind::Count, sum), ("took.min", Kind::Gauge, min), ("took.max", Kind::Gauge, max)] {
                let k = SeriesKey::new(metric, [("host", h)], kind).unwrap();
                b.define(&k);
                b.run(k.fp, [(30_000, v)]);
            }
        }
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str| -> Vec<f64> {
            let mut r = Request::new(q, 0, 60_000);
            r.step = Some(60_000);
            eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}")).series.into_iter().map(|x| x.values[0]).collect()
        };
        assert_eq!(ask("avg:took{*}"), vec![8.0]);
        assert_eq!(ask("avg:took{*} by {host}"), vec![2.5, 30.0]);
        assert_eq!(ask("count:took{*}"), vec![5.0]);
        assert_eq!(ask("sum:took{*}"), vec![40.0]);
        assert_eq!(ask("min:took{*}"), vec![1.0]);
        assert_eq!(ask("max:took{*}"), vec![30.0]);
        assert!(ask("p99:took{*}").is_empty());
    }

    /// An explicit-bucket histogram (OTLP's default, its `le` counts cumulative): percentiles as
    /// Prometheus's histogram_quantile reads them, by group, the +Inf bucket's its highest bound.
    #[test]
    fn an_explicit_histogram_answers_percentiles_interpolated_within_its_buckets() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        // Host a: 1,000 values spread evenly over (0, 100], a hundred a bound of ten. Host b: ten
        // times as many, ten more past the last bound.
        for (host, per, past) in [("a", 100.0, 0.0), ("b", 1_000.0, 10.0)] {
            let mut cum = 0.0;
            for le in (10..=100).step_by(10) {
                cum += per;
                let k = SeriesKey::new("lat.bucket", [("host", host), ("le", le.to_string().as_str())], Kind::Count).unwrap();
                b.define(&k);
                b.run(k.fp, [(30_000, cum)]);
            }
            let k = SeriesKey::new("lat.bucket", [("host", host), ("le", "+Inf")], Kind::Count).unwrap();
            b.define(&k);
            b.run(k.fp, [(30_000, cum + past)]);
            for (m, v) in [("lat.count", cum + past), ("lat.sum", 50.0 * cum)] {
                let k = SeriesKey::new(m, [("host", host)], Kind::Count).unwrap();
                b.define(&k);
                b.run(k.fp, [(30_000, v)]);
            }
        }
        s.apply(&b.finish()).unwrap();
        let ask = |q: &str| -> Vec<f64> {
            let mut r = Request::new(q, 0, 60_000);
            r.step = Some(60_000);
            eval(&s, &r).unwrap_or_else(|e| panic!("{q}: {e}")).series.into_iter().map(|x| x.values[0]).collect()
        };
        assert_eq!(ask("p50:lat{host:a}"), vec![50.0]);
        assert_eq!(ask("p99:lat{host:a}"), vec![99.0]);
        assert_eq!(ask("p5:lat{host:a}"), vec![5.0], "in the first bucket, from 0");
        let by = ask("p90:lat{*} by {host}");
        assert_eq!(by.len(), 2);
        assert!((by[0] - 90.0).abs() < 1e-9 && (by[1] - 90.0 * 10_010.0 / 10_000.0).abs() < 0.1, "{by:?}");
        assert_eq!(ask("p100:lat{host:b}"), vec![100.0], "past the last bound: the last bound");
        // Together: host b's ten thousand outweigh a's thousand.
        let all = ask("p50:lat{*}");
        assert!((all[0] - 50.0).abs() < 0.1, "{all:?}");
    }

    /// A heatmap: a distribution's rows its buckets (made coarser until they fit), each bucket of
    /// time its merged counts; a gauge's, its series' values counted in equal rows.
    #[test]
    fn a_heatmap_counts_a_distribution_s_buckets_and_a_metric_s_values() {
        use crate::dist::{BUCKET_TAG, Bucket, tag_value};
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        // Two hosts' buckets at scale 5: p0 3 and p40 1 from a, p1 2 from b, in the first minute;
        // p40 4 from b in the second.
        for (h, bucket, t, n) in [("a", Bucket::Pos(0), 10_000, 3.0), ("a", Bucket::Pos(40), 10_000, 1.0), ("b", Bucket::Pos(1), 20_000, 2.0), ("b", Bucket::Pos(40), 70_000, 4.0)] {
            let tag = tag_value(5, bucket);
            let k = SeriesKey::new("lat.dist", [("host", h), (BUCKET_TAG, tag.as_str())], Kind::Count).unwrap();
            b.define(&k);
            b.run(k.fp, [(t, n)]);
        }
        for (h, v) in [("a", 1.0), ("b", 2.0), ("c", 4.0)] {
            let k = SeriesKey::new("cpu", [("host", h)], Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, [(10_000, v)]);
        }
        s.apply(&b.finish()).unwrap();
        let mut r = Request::new("p50:lat{*}", 0, 120_000);
        r.step = Some(60_000);
        let h = heatmap(&s, &r, 10).unwrap();
        assert!(h.distribution);
        assert_eq!(h.rows, vec![Bucket::Pos(0).bounds(5), Bucket::Pos(1).bounds(5), Bucket::Pos(40).bounds(5)]);
        assert_eq!(h.counts, vec![vec![3.0, 2.0, 1.0], vec![0.0, 0.0, 4.0]]);
        // Two rows at the most: one scale coarser, p0 and p1 are scale 4's p0 and p40 its p20.
        let h2 = heatmap(&s, &r, 2).unwrap();
        assert_eq!(h2.rows.len(), 2);
        assert_eq!(h2.counts.iter().map(|c| c.iter().sum::<f64>()).collect::<Vec<_>>(), vec![6.0, 4.0], "every count kept");
        // A gauge: its three hosts' values in equal rows from 1 to 4.
        let mut r = Request::new("avg:cpu{*} by {host}", 0, 60_000);
        r.step = Some(60_000);
        let g = heatmap(&s, &r, 3).unwrap();
        assert!(!g.distribution);
        assert_eq!(g.rows, vec![(1.0, 2.0), (2.0, 3.0), (3.0, 4.0)]);
        assert_eq!(g.counts, vec![vec![1.0, 1.0, 1.0]]);
    }

    #[test]
    fn a_calendar_month_back_is_the_same_day_or_the_month_s_last() {
        // 2026-03-31 12:00 UTC, a month back: February has no 31st, so 2026-02-28 12:00.
        let to = days_from_civil(2026, 3, 31) * 86_400_000 + 12 * 3_600_000;
        let calendar_shift_ms = |shift: &str, to: i64| -> Result<i64, QueryError> {
            let (days, months) = calendar_shift(shift)?;
            calendar::shift_back(&calendar::Zone::UTC.tz(), to, days, months).ok_or_else(|| QueryError::Invalid("past".into()))
        };
        assert_eq!(calendar_shift_ms("-1mo", to).unwrap(), -31 * 86_400_000);
        // 2026-05-15, two months back: 2026-03-15 (61 days).
        let to = days_from_civil(2026, 5, 15) * 86_400_000;
        assert_eq!(calendar_shift_ms("-2mo", to).unwrap(), -61 * 86_400_000);
        assert_eq!(calendar_shift_ms("-2w", to).unwrap(), -14 * 86_400_000);
        for bad in ["1d", "-0d", "-1y", "-d", ""] {
            assert!(calendar_shift_ms(bad, to).is_err(), "{bad}");
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    /// ASAP's span: noise about a level with a sustained dip is smoothed (a span above one: the
    /// noise goes, the dip stands out, its kurtosis kept); a clean ramp is left as it is; and a
    /// noisy wave too, as ASAP means (smoothed, it would read flatter than it is: a sine's kurtosis,
    /// 1.5, is under the noisy wave's).
    #[test]
    fn autosmooth_smooths_noise_and_keeps_what_stands_out() {
        let mut seed = 23u64;
        let mut noise = || {
            // Near normal: the sum of four uniforms.
            (0..4)
                .map(|_| {
                    seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                    ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
                })
                .sum::<f64>()
        };
        let dip: Vec<f64> = (0..300).map(|i| noise() + if (150..180).contains(&i) { -4.0 } else { 0.0 }).collect();
        let w = asap_span(&dip);
        assert!(w > 1, "{w}");
        let clean: Vec<f64> = (0..300).map(|i| f64::from(i) * 0.1).collect();
        assert_eq!(asap_span(&clean), 1);
        let wave: Vec<f64> = (0..300).map(|i| (f64::from(i) / 50.0).sin() * 5.0 + noise()).collect();
        assert_eq!(asap_span(&wave), 1);
    }
}
