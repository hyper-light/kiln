//! The query language's parser (`docs/observe.md` §6): Dijkstra's shunting-yard algorithm ("Algol
//! 60 translation", 1961) over the tokens, building the expression in an arena, children before
//! their parents. Nothing here recurses: a query's depth costs no stack, and every pass over the
//! tree is a loop over the arena.

use super::calendar::{self, Calendar};
use super::lex::{LexError, Spanned, Tok, lex};
use crate::index::{Clause, NameMatch, ValueMatch};

/// A node's place in the arena.
pub type NodeId = usize;

/// How series are combined into a group, a bucket at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpaceAgg {
    Avg,
    Sum,
    Min,
    Max,
    Count,
    /// A percentile across the group's series, 0–100.
    Pct(f64),
}

/// How a series' samples are combined into a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rollup {
    Avg,
    Sum,
    Min,
    Max,
    Count,
    Last,
}

/// What `.fill()` puts in buckets without data, in gaps of at most `limit` seconds
/// ([`FILL_LIMIT_S`] unless said).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    Null,
    Zero(Option<i64>),
    /// Linear between neighbours.
    Linear(Option<i64>),
    /// The last value.
    Last(Option<i64>),
}

/// The widest gap a fill fills unless it says (s): Datadog's default, "the maximum size of a gap you
/// want to interpolate" (its `fill()`: default 300, at most 600; kiln takes wider when asked).
/// `default_zero()` fills every gap.
pub const FILL_LIMIT_S: i64 = 300;

/// For counts and counters: a total a bucket, or a rate a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountMode {
    Count,
    Rate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricQuery {
    pub space: SpaceAgg,
    pub metric: NameMatch,
    pub filters: Vec<Clause>,
    pub by: Vec<String>,
    pub rollup: Option<(Rollup, Option<i64>)>,
    /// A calendar rollup's periods (`.rollup(sum, daily|weekly|monthly[, alignment][, 'tz'])`).
    pub calendar: Option<Calendar>,
    pub mode: Option<CountMode>,
    pub fill: Option<Fill>,
    /// `.weighted()`: a gauge's series counts in a bucket for the share of it it reported over.
    pub weighted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// `pow(a, b)`'s: no operator writes it.
    Pow,
}

/// The functions, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Rate,
    PerSecond,
    PerMinute,
    PerHour,
    Derivative,
    Diff,
    Abs,
    Log2,
    Log10,
    Cumsum,
    Integral,
    Top,
    Bottom,
    Timeshift,
    MovingAvg,
    Ewma,
    Clamp,
    /// `anomalies(query, 'basic'|'agile'|'robust', bounds, 'both'|'above'|'below')`: the series
    /// and its band (`crate::anomaly`).
    Anomalies,
    /// `forecast(query, 'linear'|'seasonal', deviations, model=, seasonality=, history=,
    /// interval=)`: the series, and past its last point what it will do and its bounds
    /// (`crate::forecast`).
    Forecast,
    /// `outliers(query, 'dbscan'|'mad'|'scaleddbscan'|'scaledmad', tolerance, pct)`: the series,
    /// each said an outlier of its group over the range or not (`crate::outlier`).
    Outliers,
    // Datadog's function catalog (docs.datadoghq.com/dashboards/functions), each as its page says.
    /// `round(query[, decimals])`, `ceil`, `floor`, `pow(a, b)`: arithmetic.
    Round,
    Ceil,
    Floor,
    Pow,
    /// `count_nonzero` (and `count_nonzero_finite`), `count_not_null`: how many of the group's series
    /// are non-zero, or have a value, at each point.
    CountNonzero,
    CountNotNull,
    /// `exclude_null`: the groups with an `N/A` tag value left out; `clamp_min`, `clamp_max`
    /// (values past the bound, and missing ones, the bound); `cutoff_min`, `cutoff_max` (values
    /// past it none).
    ExcludeNull,
    ClampMin,
    ClampMax,
    CutoffMin,
    CutoffMax,
    /// `default_zero`: a missing value 0.
    DefaultZero,
    /// `dt`: the seconds between points (of the whole group); `monotonic_diff`: `diff`, rises only;
    /// `throughput`: a value per second of its bucket.
    Dt,
    MonotonicDiff,
    Throughput,
    /// `robust_trend` (Huber), `trend_line` (least squares), `piecewise_constant` (PELT).
    RobustTrend,
    TrendLine,
    PiecewiseConstant,
    /// `autosmooth`: a moving average of a span chosen by ASAP.
    Autosmooth,
    /// `median`, `median_3`, `_5`, `_7`, `_9`: the median of the last `n` points.
    Median(u8),
    /// `ewma_1`, `_3`, `_5`, `_7`, `_10`, `_20`: `ewma` at that span.
    EwmaSpan(u8),
    /// `rollingavg_5`, `_13`, `_21`, `_29`: the mean of the last `n` points.
    RollingAvg(u8),
    /// `hour_before`, `day_before`, `week_before`, `month_before` (28 days), and
    /// `calendar_shift(query, '-1d'|'-2w'|'-1mo', 'UTC')`: the query as it was that long before.
    HourBefore,
    DayBefore,
    WeekBefore,
    MonthBefore,
    CalendarShift,
    /// `top5`, `bottom10_max`, ... (`[top|bottom][5|10|15|20][_measure]`): `top` and `bottom` in one.
    Ranked { n: u8, bottom: bool, by: Rank },
}

/// How `top` and `bottom` rank series: by their mean (Datadog's unless said), least, most, last,
/// sum, signed area, or L2 norm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rank {
    Mean,
    Min,
    Max,
    Last,
    Sum,
    Area,
    Norm,
}

impl Rank {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mean" | "avg" => Self::Mean,
            "min" => Self::Min,
            "max" => Self::Max,
            "last" => Self::Last,
            "sum" => Self::Sum,
            "area" => Self::Area,
            "norm" | "l2norm" => Self::Norm,
            _ => return None,
        })
    }
}

impl Func {
    fn by_name(n: &str) -> Option<Self> {
        Some(match n {
            "rate" => Self::Rate,
            "per_second" => Self::PerSecond,
            "per_minute" => Self::PerMinute,
            "per_hour" => Self::PerHour,
            "derivative" => Self::Derivative,
            "diff" => Self::Diff,
            "abs" => Self::Abs,
            "log2" => Self::Log2,
            "log10" => Self::Log10,
            "cumsum" => Self::Cumsum,
            "integral" => Self::Integral,
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "timeshift" => Self::Timeshift,
            "moving_avg" | "moving_rollup" => Self::MovingAvg,
            "ewma" => Self::Ewma,
            "clamp" => Self::Clamp,
            "anomalies" => Self::Anomalies,
            "forecast" => Self::Forecast,
            "outliers" => Self::Outliers,
            "round" => Self::Round,
            "ceil" => Self::Ceil,
            "floor" => Self::Floor,
            "pow" => Self::Pow,
            "count_nonzero" | "count_nonzero_finite" => Self::CountNonzero,
            "count_not_null" => Self::CountNotNull,
            "exclude_null" => Self::ExcludeNull,
            "clamp_min" => Self::ClampMin,
            "clamp_max" => Self::ClampMax,
            "cutoff_min" => Self::CutoffMin,
            "cutoff_max" => Self::CutoffMax,
            "default_zero" => Self::DefaultZero,
            "dt" => Self::Dt,
            "monotonic_diff" => Self::MonotonicDiff,
            "throughput" => Self::Throughput,
            "robust_trend" => Self::RobustTrend,
            "trend_line" => Self::TrendLine,
            "piecewise_constant" => Self::PiecewiseConstant,
            "autosmooth" => Self::Autosmooth,
            "median" => Self::Median(3),
            "hour_before" => Self::HourBefore,
            "day_before" => Self::DayBefore,
            "week_before" => Self::WeekBefore,
            "month_before" => Self::MonthBefore,
            "calendar_shift" => Self::CalendarShift,
            other => return Self::spanned(other),
        })
    }

    /// The functions a number in their name says the span of: `median_5`, `ewma_10`,
    /// `rollingavg_13`, `top10_max`.
    fn spanned(n: &str) -> Option<Self> {
        let num = |s: &str, allowed: &[u8]| s.parse::<u8>().ok().filter(|v| allowed.contains(v));
        if let Some(k) = n.strip_prefix("median_") {
            return num(k, &[3, 5, 7, 9]).map(Self::Median);
        }
        if let Some(k) = n.strip_prefix("ewma_") {
            return num(k, &[1, 3, 5, 7, 10, 20]).map(Self::EwmaSpan);
        }
        if let Some(k) = n.strip_prefix("rollingavg_") {
            return num(k, &[5, 13, 21, 29]).map(Self::RollingAvg);
        }
        let (bottom, rest) = match (n.strip_prefix("top"), n.strip_prefix("bottom")) {
            (Some(r), _) => (false, r),
            (_, Some(r)) => (true, r),
            _ => return None,
        };
        let (count, by) = rest.split_once('_').map_or((rest, Rank::Mean), |(c, m)| (c, Rank::parse(m).unwrap_or(Rank::Mean)));
        if rest.split_once('_').is_some_and(|(_, m)| Rank::parse(m).is_none()) {
            return None;
        }
        num(count, &[5, 10, 15, 20]).map(|n| Self::Ranked { n, bottom, by })
    }

    /// The fewest and most arguments.
    pub fn arity(self) -> (usize, usize) {
        match self {
            Self::Top | Self::Bottom => (1, 4),
            Self::Timeshift => (2, 2),
            // `moving_rollup(query, seconds[, aggregator])`; `ewma(query[, span])`, 20 unless said.
            Self::MovingAvg => (2, 3),
            Self::Ewma => (1, 2),
            Self::Round => (1, 2),
            Self::Pow | Self::ClampMin | Self::ClampMax | Self::CutoffMin | Self::CutoffMax => (2, 2),
            Self::CalendarShift => (2, 3),
            Self::Clamp => (3, 3),
            Self::Anomalies => (1, 4),
            // The query and its algorithm; deviations; then named arguments.
            Self::Forecast => (2, 8),
            Self::Outliers => (1, 4),
            _ => (1, 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Num(f64),
    Str(String),
    Metric(MetricQuery),
    Neg(NodeId),
    Bin(BinOp, NodeId, NodeId),
    Call(Func, Vec<NodeId>),
    /// A named argument, `model='reactive'`, its value as written (a string's, a number's or a
    /// name's): a function's argument, nothing else.
    Named(String, String),
}

/// A parsed query: its nodes, children before their parents, and the root.
#[derive(Debug, Clone, PartialEq)]
pub struct Ast {
    pub nodes: Vec<Node>,
    pub root: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error(transparent)]
    Lex(#[from] LexError),
    #[error("{what} at byte {at}")]
    Syntax { at: usize, what: String },
    #[error("an empty query")]
    Empty,
}

fn syntax(at: usize, what: impl Into<String>) -> ParseError {
    ParseError::Syntax { at, what: what.into() }
}

/// An operator waiting on the stack.
#[derive(Debug, Clone, Copy)]
enum Op {
    Bin(BinOp),
    Neg,
    /// `(` of a group.
    Group(usize),
    /// `name(` of a call, with the arguments seen so far.
    Call(Func, usize, usize),
}

fn precedence(op: BinOp) -> u8 {
    match op {
        BinOp::Add | BinOp::Sub => 1,
        BinOp::Mul | BinOp::Div => 2,
        BinOp::Pow => 3,
    }
}

/// The tightest binding: a sign binds tighter than any binary operator.
const NEG_PRECEDENCE: u8 = 3;

struct Builder {
    nodes: Vec<Node>,
    operands: Vec<NodeId>,
}

impl Builder {
    fn push(&mut self, n: Node) {
        self.operands.push(self.nodes.len());
        self.nodes.push(n);
    }

    fn reduce(&mut self, op: Op, at: usize) -> Result<(), ParseError> {
        match op {
            Op::Bin(b) => {
                let (Some(r), Some(l)) = (self.operands.pop(), self.operands.pop()) else { return Err(syntax(at, "an operator without its operands")) };
                self.push(Node::Bin(b, l, r));
            }
            Op::Neg => {
                let Some(x) = self.operands.pop() else { return Err(syntax(at, "a sign without its operand")) };
                self.push(Node::Neg(x));
            }
            Op::Group(open) | Op::Call(_, _, open) => return Err(syntax(open, "an unclosed (")),
        }
        Ok(())
    }
}

/// Parses a query.
pub fn parse(q: &str) -> Result<Ast, ParseError> {
    let toks = lex(q)?;
    if toks.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut b = Builder { nodes: Vec::new(), operands: Vec::new() };
    let mut ops: Vec<Op> = Vec::new();
    let mut expect_operand = true;
    let mut i = 0usize;
    while let Some((at, tok)) = toks.get(i) {
        let at = *at;
        if expect_operand {
            match tok {
                Tok::Number(n) => {
                    b.push(Node::Num(*n));
                    expect_operand = false;
                    i += 1;
                }
                Tok::Str(s) => {
                    b.push(Node::Str(s.clone()));
                    expect_operand = false;
                    i += 1;
                }
                Tok::Minus => {
                    ops.push(Op::Neg);
                    i += 1;
                }
                Tok::LParen => {
                    ops.push(Op::Group(at));
                    i += 1;
                }
                // A named argument: `key='value'` (the lexer reads `key=` as a name), `key=value` (one
                // name), or `key = value`.
                Tok::Name(name) if matches!(ops.last(), Some(Op::Call(..))) && (name.find('=').is_some_and(|p| p > 0) || matches!(toks.get(i + 1), Some((_, Tok::Name("="))))) => {
                    let (key, inline, at_value) = match name.split_once('=') {
                        Some((k, v)) if !v.is_empty() => (k, Some(v), i),
                        Some((k, _)) => (k, None, i + 1),
                        None => (*name, None, i + 2),
                    };
                    let value = match (inline, toks.get(at_value)) {
                        (Some(v), _) => v.to_owned(),
                        (None, Some((_, Tok::Str(v)))) => v.clone(),
                        (None, Some((_, Tok::Number(x)))) => x.to_string(),
                        (None, Some((_, Tok::Name(v)))) => (*v).to_owned(),
                        _ => return Err(syntax(at, format!("{key}= needs a value"))),
                    };
                    if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
                        return Err(syntax(at, format!("a named argument's name: {key}")));
                    }
                    b.push(Node::Named(key.to_ascii_lowercase(), value));
                    expect_operand = false;
                    i = at_value + 1;
                }
                Tok::Name(name) if matches!(toks.get(i + 1), Some((_, Tok::LParen))) && Func::by_name(&name.to_ascii_lowercase()).is_some() => {
                    let f = Func::by_name(&name.to_ascii_lowercase()).ok_or_else(|| syntax(at, "a function"))?;
                    ops.push(Op::Call(f, b.operands.len(), at));
                    i += 2;
                }
                Tok::Name(_) => {
                    let (mq, next) = metric_query(&toks, i)?;
                    b.push(Node::Metric(mq));
                    expect_operand = false;
                    i = next;
                }
                // An empty argument list: `f()`.
                Tok::RParen if matches!(ops.last(), Some(Op::Call(..))) => {
                    return Err(syntax(at, "a function needs arguments"));
                }
                t => return Err(syntax(at, format!("a value expected, found {t:?}"))),
            }
            continue;
        }
        match tok {
            Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash => {
                let op = match tok {
                    Tok::Plus => BinOp::Add,
                    Tok::Minus => BinOp::Sub,
                    Tok::Star => BinOp::Mul,
                    _ => BinOp::Div,
                };
                // Left-associative: reduce what binds as tight or tighter first.
                while let Some(&top) = ops.last() {
                    let tighter = match top {
                        Op::Bin(t) => precedence(t) >= precedence(op),
                        Op::Neg => NEG_PRECEDENCE >= precedence(op),
                        _ => false,
                    };
                    if !tighter {
                        break;
                    }
                    ops.pop();
                    b.reduce(top, at)?;
                }
                ops.push(Op::Bin(op));
                expect_operand = true;
                i += 1;
            }
            Tok::Comma => {
                // Ends an argument: reduce to the call.
                loop {
                    match ops.last() {
                        Some(Op::Call(..)) => break,
                        Some(&op @ (Op::Bin(_) | Op::Neg)) => {
                            ops.pop();
                            b.reduce(op, at)?;
                        }
                        _ => return Err(syntax(at, "a comma outside a function's arguments")),
                    }
                }
                expect_operand = true;
                i += 1;
            }
            Tok::RParen => {
                loop {
                    match ops.pop() {
                        Some(Op::Group(_)) => break,
                        Some(Op::Call(f, base, call_at)) => {
                            let argc = b.operands.len().saturating_sub(base);
                            let (lo, hi) = f.arity();
                            if argc < lo || argc > hi {
                                return Err(syntax(call_at, format!("{f:?} takes {lo} to {hi} arguments, given {argc}")));
                            }
                            let args = b.operands.split_off(base);
                            b.push(Node::Call(f, args));
                            break;
                        }
                        Some(op) => b.reduce(op, at)?,
                        None => return Err(syntax(at, "an unopened )")),
                    }
                }
                i += 1;
            }
            t => return Err(syntax(at, format!("an operator expected, found {t:?}"))),
        }
    }
    if expect_operand {
        return Err(syntax(q.len(), "the query ends where a value is expected"));
    }
    while let Some(op) = ops.pop() {
        b.reduce(op, q.len())?;
    }
    match b.operands.as_slice() {
        [root] => Ok(Ast { root: *root, nodes: b.nodes }),
        _ => Err(syntax(0, "more than one expression")),
    }
}

/// A space aggregation's name (`avg`, `p99`, `p99.9`).
fn space_agg(s: &str) -> Option<SpaceAgg> {
    Some(match s {
        "avg" => SpaceAgg::Avg,
        "sum" => SpaceAgg::Sum,
        "min" => SpaceAgg::Min,
        "max" => SpaceAgg::Max,
        "count" => SpaceAgg::Count,
        p if p.starts_with('p') => {
            let v: f64 = p.get(1..)?.parse().ok()?;
            if !(0.0..=100.0).contains(&v) {
                return None;
            }
            SpaceAgg::Pct(v)
        }
        _ => return None,
    })
}

fn rollup_fn(s: &str) -> Option<Rollup> {
    Some(match s {
        "avg" => Rollup::Avg,
        "sum" => Rollup::Sum,
        "min" => Rollup::Min,
        "max" => Rollup::Max,
        "count" => Rollup::Count,
        "last" => Rollup::Last,
        _ => return None,
    })
}

fn value_match(v: &str) -> ValueMatch {
    if v == "*" {
        ValueMatch::Any
    } else if v.contains(['*', '?']) {
        ValueMatch::Glob(v.to_owned())
    } else {
        ValueMatch::Exact(v.to_owned())
    }
}

/// A metric query starting at token `i`: `[agg:]metric[{filters}][ by {tags}][.modifier(...)]*`;
/// the index of the token after it.
fn metric_query(toks: &[Spanned<'_>], mut i: usize) -> Result<(MetricQuery, usize), ParseError> {
    let Some((at, Tok::Name(head))) = toks.get(i) else { return Err(syntax(0, "a metric")) };
    let at = *at;
    let (space, metric) = match head.split_once(':') {
        Some((agg, rest)) if !rest.is_empty() => match space_agg(agg) {
            Some(s) => (s, rest),
            None => (SpaceAgg::Avg, *head),
        },
        _ => (SpaceAgg::Avg, *head),
    };
    if metric.is_empty() {
        return Err(syntax(at, "a metric's name"));
    }
    let metric = if metric.contains(['*', '?']) { NameMatch::Glob(metric.to_owned()) } else { NameMatch::Exact(metric.to_owned()) };
    let mut q = MetricQuery { space, metric, filters: Vec::new(), by: Vec::new(), rollup: None, calendar: None, mode: None, fill: None, weighted: false };
    i += 1;
    if let Some((_, Tok::LBrace)) = toks.get(i) {
        i = filters(toks, i + 1, &mut q.filters)?;
    }
    if let Some((by_at, Tok::Name(n))) = toks.get(i)
        && n.eq_ignore_ascii_case("by")
    {
        let Some((_, Tok::LBrace)) = toks.get(i + 1) else { return Err(syntax(*by_at, "by takes {tags}")) };
        i += 2;
        loop {
            match toks.get(i) {
                Some((_, Tok::Name(t))) => {
                    q.by.push((*t).to_owned());
                    i += 1;
                }
                Some((_, Tok::Comma)) => i += 1,
                Some((_, Tok::RBrace)) => {
                    i += 1;
                    break;
                }
                Some((a, t)) => return Err(syntax(*a, format!("a tag expected in by {{}}, found {t:?}"))),
                None => return Err(syntax(*by_at, "an unclosed by {")),
            }
        }
    }
    // Modifiers.
    while let (Some((_, Tok::Dot)), Some((m_at, Tok::Name(m))), Some((_, Tok::LParen))) = (toks.get(i), toks.get(i + 1), toks.get(i + 2)) {
        let m_at = *m_at;
        i += 3;
        let mut args: Vec<&Tok<'_>> = Vec::new();
        loop {
            match toks.get(i) {
                Some((_, Tok::RParen)) => {
                    i += 1;
                    break;
                }
                Some((_, Tok::Comma)) => i += 1,
                Some((_, t)) => {
                    args.push(t);
                    i += 1;
                }
                None => return Err(syntax(m_at, "an unclosed modifier")),
            }
        }
        let name_arg = |a: Option<&&Tok<'_>>| -> Option<String> {
            match a {
                Some(Tok::Name(n)) => Some(n.to_ascii_lowercase()),
                Some(Tok::Str(s)) => Some(s.to_ascii_lowercase()),
                _ => None,
            }
        };
        let num_arg = |a: Option<&&Tok<'_>>| -> Option<i64> {
            match a {
                Some(Tok::Number(n)) if n.is_finite() && *n >= 0.0 && *n <= 1e12 => Some(*n as i64),
                _ => None,
            }
        };
        match *m {
            "rollup" => {
                let f = name_arg(args.first()).and_then(|n| rollup_fn(&n)).ok_or_else(|| syntax(m_at, "rollup(avg|sum|min|max|count|last[, seconds | daily|weekly|monthly[, alignment][, 'timezone']])"))?;
                let secs = match args.get(1) {
                    None => None,
                    Some(Tok::Number(_)) => Some(num_arg(args.get(1)).filter(|&s| s > 0).ok_or_else(|| syntax(m_at, "a rollup's seconds"))?),
                    Some(t) => {
                        q.calendar = Some(calendar(t, args.get(2..).unwrap_or(&[])).map_err(|e| syntax(m_at, e))?);
                        None
                    }
                };
                if secs.is_some() && args.len() > 2 {
                    return Err(syntax(m_at, "a rollup in seconds takes no alignment: rollup(sum, 60)"));
                }
                q.rollup = Some((f, secs));
            }
            "weighted" if args.is_empty() => q.weighted = true,
            "as_count" if args.is_empty() => q.mode = Some(CountMode::Count),
            "as_rate" if args.is_empty() => q.mode = Some(CountMode::Rate),
            "fill" => {
                let how = name_arg(args.first()).ok_or_else(|| syntax(m_at, "fill(null|zero|linear|last[, seconds])"))?;
                let limit = match args.get(1) {
                    None => None,
                    a => Some(num_arg(a).ok_or_else(|| syntax(m_at, "a fill's seconds"))?),
                };
                q.fill = Some(match how.as_str() {
                    "null" => Fill::Null,
                    "zero" => Fill::Zero(limit),
                    "linear" => Fill::Linear(limit),
                    "last" => Fill::Last(limit),
                    _ => return Err(syntax(m_at, "fill(null|zero|linear|last[, seconds])")),
                });
            }
            other => return Err(syntax(m_at, format!("no modifier .{other}()"))),
        }
    }
    // A gauge's weight is its average's share of a bucket: a sum, a count, an extreme of its
    // samples has no share to weigh.
    if q.weighted && q.rollup.is_some_and(|r| r.0 != Rollup::Avg) {
        return Err(syntax(at, "weighted() weighs a gauge's averages: rollup(avg), or no rollup"));
    }
    Ok((q, i))
}

/// A calendar rollup's periods from its unit and what follows it: an alignment, a timezone, or
/// both in that order (Datadog's `.rollup(sum, weekly, monday, 'America/New_York')`).
fn calendar(unit: &Tok<'_>, rest: &[&Tok<'_>]) -> Result<Calendar, String> {
    fn word<'t>(t: &'t Tok<'_>) -> Option<std::borrow::Cow<'t, str>> {
        match t {
            Tok::Name(n) => Some((*n).into()),
            Tok::Str(s) => Some(s.as_str().into()),
            // A day of the month written bare (`15`).
            Tok::Number(n) if n.fract() == 0.0 && (1.0..=31.0).contains(n) => Some(format!("{n}").into()),
            _ => None,
        }
    }
    let unit = word(unit).and_then(|u| calendar::Unit::parse(&u)).ok_or("a rollup's interval: seconds, or daily, weekly or monthly")?;
    let how = match unit {
        calendar::Unit::Daily => "a daily rollup's start: a time of day (12am, 9:30am, 21:30)",
        calendar::Unit::Weekly => "a weekly rollup's start: a day (monday … sunday)",
        calendar::Unit::Monthly => "a monthly rollup's start: a day of the month (1 to 31, 1st, 15th)",
    };
    let (mut align, mut zone) = (unit.default_alignment(), calendar::Zone::UTC);
    match rest {
        [] => {}
        [one] => {
            let w = word(one).ok_or(how)?;
            match unit.alignment(&w) {
                Some(a) => align = a,
                // One argument that is no alignment: a timezone, or neither.
                None => zone = calendar::Zone::named(&w).map_err(|e| if matches!(one, Tok::Str(_)) { e } else { how.to_owned() })?,
            }
        }
        [a, z] => {
            align = word(a).and_then(|w| unit.alignment(&w)).ok_or(how)?;
            zone = calendar::Zone::named(&word(z).ok_or("a rollup's timezone: an IANA name, quoted ('America/New_York')")?)?;
        }
        _ => return Err("a calendar rollup takes at most four arguments: rollup(sum, weekly, monday, 'America/New_York')".into()),
    }
    Ok(Calendar { unit, align, zone })
}

/// Filters after `{`: `*`, `key:value`, `!key:value`, `key:*`, `key:(a OR b)`, `key IN (a, b)`,
/// `key NOT IN (a, b)`, comma-separated (AND); the index after `}`.
fn filters(toks: &[Spanned<'_>], mut i: usize, out: &mut Vec<Clause>) -> Result<usize, ParseError> {
    let open = toks.get(i.saturating_sub(1)).map_or(0, |t| t.0);
    loop {
        let mut negate = false;
        if let Some((_, Tok::Bang)) = toks.get(i) {
            negate = true;
            i += 1;
        }
        match toks.get(i) {
            Some((_, Tok::RBrace)) => return Ok(i + 1),
            Some((_, Tok::Comma)) => i += 1,
            Some((_, Tok::Name("*"))) => i += 1,
            Some((at, Tok::Name(item))) => {
                let at = *at;
                // `key IN (...)` and `key NOT IN (...)`.
                let in_list = match (toks.get(i + 1), toks.get(i + 2)) {
                    (Some((_, Tok::Name(w))), _) if w.eq_ignore_ascii_case("in") => Some((false, i + 2)),
                    (Some((_, Tok::Name(w))), Some((_, Tok::Name(w2)))) if w.eq_ignore_ascii_case("not") && w2.eq_ignore_ascii_case("in") => Some((true, i + 3)),
                    _ => None,
                };
                if let Some((not, j)) = in_list {
                    let (values, next) = value_list(toks, j, false)?;
                    out.push(Clause { key: (*item).to_owned(), values, negate: negate ^ not });
                    i = next;
                    continue;
                }
                let Some((key, value)) = item.split_once(':') else { return Err(syntax(at, format!("a filter is key:value, found {item:?}"))) };
                if key.is_empty() {
                    return Err(syntax(at, "a filter's key"));
                }
                if let (true, Some((_, Tok::Str(quoted)))) = (value.is_empty(), toks.get(i + 1)) {
                    // `key:"a value"`: any value, spaces and all, exactly (no wildcards).
                    out.push(Clause { key: key.to_owned(), values: vec![ValueMatch::Exact(quoted.clone())], negate });
                    i += 2;
                } else if value.is_empty() {
                    // `key:(a OR b)`.
                    let (values, next) = value_list(toks, i + 1, true)?;
                    out.push(Clause { key: key.to_owned(), values, negate });
                    i = next;
                } else {
                    out.push(Clause { key: key.to_owned(), values: vec![value_match(value)], negate });
                    i += 1;
                }
            }
            Some((at, t)) => return Err(syntax(*at, format!("a filter expected, found {t:?}"))),
            None => return Err(syntax(open, "an unclosed {")),
        }
    }
}

/// `(a OR b OR c)` or `(a, b, c)`: the values, and the index after `)`.
fn value_list(toks: &[Spanned<'_>], i: usize, ors: bool) -> Result<(Vec<ValueMatch>, usize), ParseError> {
    let Some((open, Tok::LParen)) = toks.get(i) else { return Err(syntax(toks.get(i).map_or(0, |t| t.0), "a ( of values")) };
    let mut values = Vec::new();
    let mut j = i + 1;
    loop {
        match toks.get(j) {
            Some((_, Tok::RParen)) => break,
            Some((_, Tok::Comma)) if !ors => j += 1,
            Some((_, Tok::Name(w))) if ors && w.eq_ignore_ascii_case("or") => j += 1,
            Some((_, Tok::Name(v))) => {
                values.push(value_match(v));
                j += 1;
            }
            Some((_, Tok::Str(v))) => {
                values.push(value_match(v));
                j += 1;
            }
            Some((_, Tok::Number(_))) => {
                // A number-like value (a status code): its text as written.
                values.push(ValueMatch::Exact(number_text(toks, j)));
                j += 1;
            }
            Some((at, t)) => return Err(syntax(*at, format!("a value expected, found {t:?}"))),
            None => return Err(syntax(*open, "an unclosed (")),
        }
    }
    if values.is_empty() {
        return Err(syntax(*open, "an empty list of values"));
    }
    Ok((values, j + 1))
}

fn number_text(toks: &[Spanned<'_>], j: usize) -> String {
    match toks.get(j) {
        Some((_, Tok::Number(n))) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
        Some((_, Tok::Number(n))) => n.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(ast: &Ast, id: NodeId) -> &MetricQuery {
        match &ast.nodes[id] {
            Node::Metric(m) => m,
            n => panic!("not a metric: {n:?}"),
        }
    }

    #[test]
    fn a_full_metric_query() {
        let ast = parse("p99:kiln.http.duration{route:/api/*, !env:dev, host:(a OR b), code IN (500, 503), zone NOT IN (x)} by {route,host}.rollup(max, 60).as_rate().fill(linear, 300)").unwrap();
        let m = metric(&ast, ast.root);
        assert_eq!(m.space, SpaceAgg::Pct(99.0));
        assert_eq!(m.metric, NameMatch::Exact("kiln.http.duration".into()));
        assert_eq!(
            m.filters,
            vec![
                Clause { key: "route".into(), values: vec![ValueMatch::Glob("/api/*".into())], negate: false },
                Clause { key: "env".into(), values: vec![ValueMatch::Exact("dev".into())], negate: true },
                Clause { key: "host".into(), values: vec![ValueMatch::Exact("a".into()), ValueMatch::Exact("b".into())], negate: false },
                Clause { key: "code".into(), values: vec![ValueMatch::Exact("500".into()), ValueMatch::Exact("503".into())], negate: false },
                Clause { key: "zone".into(), values: vec![ValueMatch::Exact("x".into())], negate: true },
            ]
        );
        assert_eq!(m.by, vec!["route", "host"]);
        assert_eq!(m.rollup, Some((Rollup::Max, Some(60))));
        assert_eq!(m.mode, Some(CountMode::Rate));
        assert_eq!(m.fill, Some(Fill::Linear(Some(300))));
        // A quoted value: spaces, commas, braces and quotes in it, matched exactly.
        let ast = parse(r#"a{version:"1.2 beta, {rc}", !note:"say \"hi\"", glob:"x*"}"#).unwrap();
        assert_eq!(
            metric(&ast, ast.root).filters,
            vec![
                Clause { key: "version".into(), values: vec![ValueMatch::Exact("1.2 beta, {rc}".into())], negate: false },
                Clause { key: "note".into(), values: vec![ValueMatch::Exact("say \"hi\"".into())], negate: true },
                Clause { key: "glob".into(), values: vec![ValueMatch::Exact("x*".into())], negate: false },
            ]
        );
    }

    #[test]
    fn precedence_signs_groups_and_calls() {
        let ast = parse("100 * a{*} / b{*} - -c{*}").unwrap();
        // ((100 * a) / b) - (-c)
        let Node::Bin(BinOp::Sub, l, r) = &ast.nodes[ast.root] else { panic!("{:?}", ast.nodes[ast.root]) };
        assert!(matches!(ast.nodes[*r], Node::Neg(_)));
        let Node::Bin(BinOp::Div, ll, _) = &ast.nodes[*l] else { panic!() };
        assert!(matches!(ast.nodes[*ll], Node::Bin(BinOp::Mul, _, _)));
        let ast = parse("top(avg:cpu{*} by {host}, 10, 'mean', 'desc')").unwrap();
        let Node::Call(Func::Top, args) = &ast.nodes[ast.root] else { panic!() };
        assert_eq!(args.len(), 4);
        assert!(matches!(ast.nodes[args[1]], Node::Num(n) if n == 10.0));
        let ast = parse("timeshift((a{*} + b{*}) * 2, -86400)").unwrap();
        let Node::Call(Func::Timeshift, args) = &ast.nodes[ast.root] else { panic!() };
        assert!(matches!(ast.nodes[args[0]], Node::Bin(BinOp::Mul, _, _)));
        assert!(matches!(ast.nodes[args[1]], Node::Neg(_)));
        // A Prometheus-style name with colons is a metric, not an aggregation.
        let ast = parse("job:http_requests:rate5m{*}").unwrap();
        assert_eq!(metric(&ast, ast.root).metric, NameMatch::Exact("job:http_requests:rate5m".into()));
        assert_eq!(metric(&ast, ast.root).space, SpaceAgg::Avg);
    }

    #[test]
    fn mistakes_are_errors_that_say_where() {
        for bad in ["", "a{", "a{host}", "a{*} +", "(a{*}", "a{*})", "top()", "top(a{*}, 1, 2, 3, 4)", "a{*}.rollup(median)", "a{*}.nope()", "a{*} a{*}", "a by host", "abs(a{*}, 2)", "a{host:()}"] {
            assert!(parse(bad).is_err(), "{bad:?} parsed");
        }
        assert!(matches!(parse("a{*} + + b{*}"), Ok(_) | Err(ParseError::Syntax { .. })));
    }

    #[test]
    fn deep_nesting_costs_no_stack_and_random_input_never_panics() {
        let deep = format!("{}a{{*}}{}", "abs(".repeat(1500), ")".repeat(1500));
        assert!(deep.len() <= super::super::lex::MAX_QUERY_BYTES);
        let ast = parse(&deep).unwrap();
        assert_eq!(ast.nodes.len(), 1501);
        let mut rng = crate::testkit::Rng::new(8);
        let words = ["avg:", "m", "{", "}", "(", ")", ",", "*", "+", "-", "/", "!", "by", "host:a", " OR ", " IN ", ".rollup(", "sum", "60", "'x'", "top(", "timeshift(", "abs(", ".fill(", "linear", " "];
        for _ in 0..20_000 {
            let q: String = (0..rng.below(16)).map(|_| words[rng.below(words.len() as u64) as usize]).collect();
            let _ = parse(&q);
        }
    }
}
