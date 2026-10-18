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

/// A metric query's series on the grid.
fn metric(store: &Store, q: &MetricQuery, grid: Grid, spans: Spans<'_>, max_series: usize, scratch: &mut Scratch, scanned: &mut (u64, u64)) -> Result<Vec<Series>, QueryError> {
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
    let (from, end) = (spans.start(0), spans.start(grid.n));
    // `.weighted()`: a gauge series' samples a bucket, then its weights; its interval, the median
    // gap between its samples (learned, so a series reporting every 10 s and one every 60 s weigh
    // alike over a bucket they both covered). One with too few samples to tell takes the query's,
    // the median of the others', once they are known.
    let mut counts = if q.weighted { vec![0.0; grid.n] } else { Vec::new() };
    let (mut gaps, mut intervals) = if q.weighted { (Vec::with_capacity(INTERVAL_GAPS), Vec::with_capacity(ids.len())) } else { (Vec::new(), Vec::new()) };
    let mut pending: Vec<(usize, usize, f64, f64)> = Vec::new();
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
        let weighted = q.weighted && s.kind() == Kind::Gauge;
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
