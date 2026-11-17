//! Monitors (`docs/observe.md` §9): a query, a condition, and what to say. The engine's part,
//! which kiln drives on the group's leader: Datadog's monitor queries parsed and evaluated over a
//! store into a value a group, each group's state moved by its thresholds, and the message
//! rendered from Datadog's template syntax, its `@` mentions naming where it goes.
//!
//! Queries (Datadog's "Monitor API" syntax, so a monitor written for Datadog reads here):
//!
//! ```text
//! metric := time_aggr "(" window ")" ":" query op threshold
//! forecast := ("max" | "min") "(" "next_" n unit ")" ":" "forecast(" query "," algorithm ["," deviations] ["," name "=" value]* ")" op threshold
//! change := ("change" | "pct_change") "(" time_aggr "(" window ")" "," shift ")" ":" query op threshold
//! shift  := <n>(m|h|d|w)"_ago" | window          (Datadog's 5m_ago, 1h_ago, 1d_ago)
//! logs   := "logs(" '"' search '"' ")" [".index(" '"' * '"' ")"] ".rollup(" '"' measure '"' ["," '"' facet '"'] ")"
//!           [".by(" '"' facets '"' ")"] ".last(" '"' window '"' ")" op threshold
//! spans  := ("trace-analytics(" | "spans(") '"' search '"' ")" ... as logs   (a span search, its facets a span's)
//! slo    := "slo(" '"' id '"' ")" ( ".burn_rate(" '"' long '"' "," '"' short '"' ")" | ".error_budget()" ) op threshold
//! errors := "error-tracking(" '"' search '"' ")" [".source(" '"' ("all" | "backend") '"' ")"] (".new()" | ".impact()")
//!           [".rollup(" '"' "count" '"' ")"] ".by(" '"' "issue.id" ["," facets] '"' ")" ".last(" '"' window '"' ")" op threshold
//! time_aggr := avg | sum | min | max | last | percentile
//! window := last_<n>(m|h|d|w)  (logs: <n>(m|h|d|w))
//!         | current_1h | current_1d | current_1mo   (a metric monitor's cumulative window)
//! op     := > | >= | < | <= | == | !=
//! threshold := a finite number
//! ```
//!
//! A metric monitor reads its window as Datadog's evaluation paths do: bucket by bucket, the time
//! aggregation over the values the formula gives (its classic path); with `sum` and a query read
//! `.as_count()`, each query summed over the window before the formula (its `.as_count()` path);
//! with `percentile`, a distribution's values over the whole window at its query's `pXX`. A change
//! monitor's classic path compares each bucket with the one its shift before, then aggregates.
//! Groups with an `N/A` tag value are left out ("`N/A` groups are not included in monitors"). A
//! cumulative window reads its period so far: from where the hour, day or month began
//! ([`Cumulative`]) to the evaluation's last whole step of it.
//!
//! A string's `\"` is a quote within it and `\\` a backslash, as Datadog's monitor API has them:
//! `logs("service:web \"connection refused\"")`.
//!
//! States move as Datadog's do: a group alerts once its value passes the critical threshold, and
//! recovers only once it no longer passes the recovery threshold (the critical one when none is
//! set), so a value that hovers at a threshold does not flap; warnings the same below it. A group
//! with no data keeps its state, or goes to NO DATA when the monitor asks to be told.

pub mod checks;
pub mod composite;
pub mod downtime;
pub mod template;

pub use template::{Rendered, Vars, render, render_message};

use std::collections::HashMap;

use crate::logs::search::Search;
use crate::logs::store::{Facet, Measure};
use crate::query::calendar::{Calendar, Unit, Zone};
use crate::query::eval::{Answer, Path, Request, Series, Source};
use crate::query::parse::{Ast, CountMode, Func, Node, RecordSource, SpaceAgg};
use crate::store::{LogsAt, Store};

/// The longest window of logs, spans, errors or an SLO's burn: a week (Datadog's API's longest,
/// `last_1w`), so that one evaluation reads a bounded range.
pub const MAX_WINDOW_MS: i64 = 7 * 86_400_000;
/// The longest a metric monitor's window, and a change monitor's shift, may be: 730 hours, Datadog's
/// ("a value between 1 minute and 730 hours (1 month)", its metric monitor's page; "a custom time
/// window of up to 1 month", its monitor configuration's). A window of whole hours is read in 365
/// buckets at the most (`step_for`; any window in [`MAX_POINTS`]), from rollups where blocks hold
/// them (`docs/observe.md` §4.7).
///
/// [`MAX_POINTS`]: crate::query::eval::MAX_POINTS
pub const MAX_METRIC_WINDOW_MS: i64 = 730 * 3_600_000;
/// The furthest a forecast monitor looks ahead: three months (Datadog's "between 12 hours and 3
/// months"; shorter here too, an hour at the least).
pub const MAX_AHEAD_MS: i64 = 92 * 86_400_000;
/// Buckets a forecast monitor reads its horizon in, unless its forecast names an `interval`: a
/// day's season at a quarter hour, a week's horizon at two hours.
pub const FORECAST_POINTS: usize = 96;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeAggr {
    Avg,
    Sum,
    Min,
    Max,
    Last,
    /// Datadog's `percentile(pXX)` of a distribution: its values over the whole window, at the pXX
    /// its query's aggregator names (`percentile(last_5m):p95:latency{*}`).
    Percentile,
    /// A percentile of the window's values, in hundredths (a formula's `.rollup("p99")`: 9900).
    Pct(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
    Ne,
}

impl Op {
    /// Whether `v` passes `threshold`.
    pub fn passes(self, v: f64, threshold: f64) -> bool {
        match self {
            Self::Gt => v > threshold,
            Self::Ge => v >= threshold,
            Self::Lt => v < threshold,
            Self::Le => v <= threshold,
            Self::Eq => v == threshold,
            Self::Ne => v != threshold,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Eq => "==",
            Self::Ne => "!=",
        }
    }
}

/// The period of Datadog's cumulative evaluation windows (its monitor configuration's "Current hour",
/// "Current day", "Current month"): `current_1h`, `current_1d`, `current_1mo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Hour,
    Day,
    Month,
}

impl Period {
    /// The step its window is read at: Datadog's evaluation frequency for a window of its length
    /// ("window < 24 hours: 1 minute; 24 hours <= window < 48 hours: 10 minutes; window >= 48 hours:
    /// 30 minutes"), so each evaluation reads every whole step its period has had.
    fn step_ms(self) -> i64 {
        match self {
            Self::Hour => 60_000,
            Self::Day => 600_000,
            Self::Month => 1_800_000,
        }
    }

    /// Its longest: an hour; a day of 25 hours (the clocks put back); 31 days, one of them such a day.
    fn longest_ms(self) -> i64 {
        match self {
            Self::Hour => 3_600_000,
            Self::Day => 25 * 3_600_000,
            Self::Month => (31 * 24 + 1) * 3_600_000,
        }
    }
}

/// A metric monitor's cumulative window: its period so far, begun where its monitor's
/// `scheduling_options.evaluation_window` says (Datadog's): an hour at its `hour_starts` minute (UTC:
/// Datadog takes no other field beside it); a day at `day_starts`, and a month on its 1st
/// (`month_starts`, which can only be 1) at `day_starts`, both in its `timezone`. Unless it says, at
/// the hour, midnight UTC and the 1st ("resets itself on the first of each month at midnight UTC").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cumulative {
    pub period: Period,
    /// The minute of the hour an hour begins at, 0 to 59.
    pub hour_starts: u32,
    /// The second of the day a day or a month begins at.
    pub day_starts: u32,
    pub zone: Zone,
}

impl Cumulative {
    fn of(period: Period) -> Self {
        Self { period, hour_starts: 0, day_starts: 0, zone: Zone::UTC }
    }

    /// When the period `t` (ms) is in began; none past the calendar's years.
    pub fn began(self, t: i64) -> Option<i64> {
        match self.period {
            Period::Hour => {
                let off = i64::from(self.hour_starts) * 60_000;
                Some((t - off).div_euclid(3_600_000) * 3_600_000 + off)
            }
            Period::Day => {
                let (c, tz) = (Calendar { unit: Unit::Daily, align: self.day_starts, zone: self.zone }, self.zone.tz());
                c.start(&tz, c.period(&tz, t)?)
            }
            Period::Month => crate::query::calendar::month_began(&self.zone.tz(), t, self.day_starts),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// `cumulative`: its window the current hour, day or month so far (`window_ms`, that period's
    /// longest); none: the last `window_ms`. `recovery_ms`: an anomaly monitor's `recovery_window`
    /// ([`Query::windows`]).
    Metric { aggr: TimeAggr, window_ms: i64, query: String, cumulative: Option<Cumulative>, recovery_ms: Option<i64> },
    /// The value now against the value `shift_ms` before (`pct`: as a percent of it).
    Change { aggr: TimeAggr, window_ms: i64, shift_ms: i64, pct: bool, query: String },
    /// Datadog's formula monitor of logs, spans or events (its options' `variables` of
    /// `data_source` `logs`, `spans` or `events`, [`parse_formula`]): each of its queries measured
    /// over the whole window, as a log monitor measures its one ("Datadog evaluates the number of
    /// logs over a selected time frame", its log monitor page), then its formula, a value a group.
    RecordsFormula { query: String, window_ms: i64 },
    Logs { search: String, measure: Measure, by: Vec<Facet>, window_ms: i64 },
    /// Datadog's trace analytics monitor: a span search, as a log monitor's is of logs.
    Spans { search: String, measure: Measure, by: Vec<Facet>, window_ms: i64 },
    /// Datadog's forecast monitor: the edge of a forecast's bounds over the next `ahead_ms`, its
    /// highest upper bound (`max`, alerting above) or its lowest lower one (`min`, below).
    Forecast { highest: bool, ahead_ms: i64, query: String },
    /// Datadog's SLO alerts (`crate::slo`): the SLO's burn rate over a long window and a short one
    /// at once, the lesser of the two (both must pass: the workbook's multiwindow alert), or with
    /// `burn` none, its error budget left (percent). Its SLO is kiln's to know: kiln evaluates it.
    Slo { id: String, burn: Option<(i64, i64)> },
    /// Datadog's Error Tracking monitor: the errors over the window of each issue it watches, by
    /// issue and the other facets it names ([`ERROR_FACETS`]). With `new` (Datadog's New Issue
    /// alert) it watches issues new or regressed, for review; else (its High Impact alert) issues
    /// for review or reviewed. Its issues are kiln's to know: kiln evaluates it.
    ErrorTracking { search: String, new: bool, by: Vec<String>, window_ms: i64 },
}

/// What an Error Tracking monitor may group by: the tags its errors are counted by
/// (`crate::errors::METRIC`).
pub const ERROR_FACETS: [&str; 4] = ["issue.id", "service", "env", "version"];

/// A monitor's query, parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub kind: Kind,
    pub op: Op,
    /// The threshold the query states (the critical one); none stated (`f64::NAN`, never compared:
    /// [`Thresholds::dynamic`]) where it says Datadog's `threshold` and `dynamic` says each group's.
    pub threshold: f64,
    /// Datadog's dynamic thresholds (a formula monitor's `> threshold`, its options'
    /// `thresholds.critical_query` and `critical_recovery_query`): each a formula, read as the
    /// monitor's own is and evaluated with it, a value for each group ([`Own::critical`]).
    pub dynamic: Option<Box<Dynamic>>,
}

/// A formula monitor's dynamic thresholds ([`parse_formula`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Dynamic {
    pub critical: Formula,
    pub critical_recovery: Option<Formula>,
}

/// A formula read as a monitor reads one: its time aggregation over its window of its query, its
/// variables in place, the value times `multiplier` (a threshold's `.multiplier(1.5)`; 1 else).
#[derive(Debug, Clone, PartialEq)]
pub struct Formula {
    pub aggr: TimeAggr,
    pub window_ms: i64,
    pub query: String,
    pub multiplier: f64,
}

impl Query {
    /// An anomaly monitor's `threshold_windows` (Datadog's anomaly page): both windows, timeframes
    /// (`last_15m`); its `trigger_window` "must match the `alert_window` from the query", and its
    /// `recovery_window` is "the time range that [is] analyzed for anomalies when evaluating whether
    /// a triggered monitor should recover", within the monitor's window. A monitor of no
    /// `anomalies()` takes none.
    pub fn windows(mut self, trigger: &str, recovery: &str) -> Result<Self, MonitorError> {
        let Kind::Metric { query, window_ms, recovery_ms, .. } = &mut self.kind else {
            return Err(syntax("threshold_windows: an anomaly monitor's (avg(last_1h):anomalies(…) >= 1)"));
        };
        let ast = crate::query::parse::parse(query).map_err(|e| syntax(e.to_string()))?;
        if !ast.nodes.iter().any(|n| matches!(n, Node::Call(Func::Anomalies, _))) {
            return Err(syntax("threshold_windows: an anomaly monitor's (avg(last_1h):anomalies(…) >= 1)"));
        }
        let (trigger, recovery) = (window(trigger, MAX_METRIC_WINDOW_MS)?, window(recovery, MAX_METRIC_WINDOW_MS)?);
        if anomaly_of(&ast)?.alert_ms != Some(trigger) {
            return Err(syntax("threshold_windows.trigger_window: the query's alert_window='…', the same window (Datadog's \"must match\")"));
        }
        if recovery > *window_ms {
            return Err(syntax("threshold_windows.recovery_window: within the monitor's window"));
        }
        *recovery_ms = Some(recovery);
        Ok(self)
    }

    /// Its cumulative window begun where its monitor's `scheduling_options.evaluation_window` says,
    /// held to Datadog's rules for it: `hour_starts` (0 to 59) with `current_1h` and nothing beside
    /// it; `day_starts` (a time of day, `04:00`), `month_starts` (1) and `timezone` (an IANA name) with
    /// `current_1d` and `current_1mo`. A query without a cumulative window takes none of them.
    pub fn starting(mut self, hour_starts: Option<u32>, day_starts: Option<&str>, month_starts: Option<u32>, timezone: Option<&str>) -> Result<Self, MonitorError> {
        let Kind::Metric { cumulative: Some(c), .. } = &mut self.kind else {
            return Err(syntax("scheduling_options.evaluation_window: for a metric monitor's cumulative window (current_1h, current_1d, current_1mo)"));
        };
        match c.period {
            Period::Hour => {
                if day_starts.is_some() || month_starts.is_some() || timezone.is_some() {
                    return Err(syntax("current_1h's evaluation_window: hour_starts alone (Datadog's: no other field beside it)"));
                }
                if let Some(m) = hour_starts {
                    c.hour_starts = Some(m).filter(|m| *m < 60).ok_or_else(|| syntax(format!("hour_starts: {m} (a minute, 0 to 59)")))?;
                }
            }
            Period::Day | Period::Month => {
                if hour_starts.is_some() {
                    return Err(syntax("hour_starts: for current_1h (current_1d and current_1mo take day_starts, month_starts and timezone)"));
                }
                if let Some(m) = month_starts.filter(|m| *m != 1) {
                    return Err(syntax(format!("month_starts: {m} (Datadog's: 1)")));
                }
                if let Some(d) = day_starts {
                    c.day_starts = Unit::Daily.alignment(d).ok_or_else(|| syntax(format!("day_starts: {d} (a time of day, 04:00)")))?;
                }
                if let Some(z) = timezone {
                    c.zone = Zone::named(z).map_err(syntax)?;
                }
            }
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MonitorError {
    #[error("the query: {0}")]
    Syntax(String),
    #[error("the evaluation: {0}")]
    Eval(String),
}

fn syntax(why: impl Into<String>) -> MonitorError {
    MonitorError::Syntax(why.into())
}

/// `last_5m`, `last_1h`, `5m` (a log monitor's), ... in ms, `most` at the most.
fn window(s: &str, most: i64) -> Result<i64, MonitorError> {
    let s = s.trim().trim_matches('"');
    let s = s.strip_prefix("last_").unwrap_or(s);
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| syntax(format!("a window without a unit: {s}")))?);
    let n: i64 = n.parse().map_err(|_| syntax(format!("a window: {s}")))?;
    let ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        // Datadog's month: 730 hours ("730 hours (1 month)").
        "mo" => 730 * 3_600_000,
        _ => return Err(syntax(format!("a window's unit: {unit}"))),
    };
    let w = n.checked_mul(ms).filter(|&w| w > 0 && w <= most).ok_or_else(|| syntax(format!("a window of {s}: at most {} hours", most / 3_600_000)))?;
    Ok(w)
}

/// A change monitor's shift, in ms: Datadog's `5m_ago`, `1h_ago`, `1d_ago` (its API's `#m_ago`,
/// `#h_ago`, `1d_ago`), or a window's `last_1h`.
fn shift(s: &str) -> Result<i64, MonitorError> {
    let s = s.trim();
    window(s.strip_suffix("_ago").unwrap_or(s), MAX_METRIC_WINDOW_MS)
}

/// A forecast monitor's horizon (`1w`, `24h`, `3d`, ...): an hour to three months.
fn ahead(s: &str) -> Result<i64, MonitorError> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| syntax(format!("a horizon without a unit: {s}")))?);
    let n: i64 = n.parse().map_err(|_| syntax(format!("a horizon: {s}")))?;
    let per = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        "mo" => 30 * 86_400_000,
        _ => return Err(syntax(format!("a horizon's unit: {unit}"))),
    };
    n.checked_mul(per).filter(|&ms| (3_600_000..=MAX_AHEAD_MS).contains(&ms)).ok_or_else(|| syntax(format!("a horizon of {s}: an hour to three months")))
}

fn aggr(s: &str) -> Result<TimeAggr, MonitorError> {
    Ok(match s.trim() {
        "avg" => TimeAggr::Avg,
        "sum" => TimeAggr::Sum,
        "min" => TimeAggr::Min,
        "max" => TimeAggr::Max,
        "last" => TimeAggr::Last,
        "percentile" => TimeAggr::Percentile,
        other => return Err(syntax(format!("a time aggregation: {other} (avg, sum, min, max, last, percentile)"))),
    })
}

/// The comparison at the end of a query, outside any parentheses, braces or quotes: its operator,
/// its threshold, and what comes before.
fn comparison(s: &str) -> Result<(&str, Op, f64), MonitorError> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    // Within a string: the quote that opened it (a quote of the other kind, or one escaped, does
    // not close it).
    let (mut quote, mut escaped) = (None::<u8>, false);
    let mut at = None;
    for (i, &c) in b.iter().enumerate() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'"' | b'\'' => quote = Some(c),
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth -= 1,
            b'>' | b'<' | b'=' | b'!' if depth == 0 => {
                at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let i = at.ok_or_else(|| syntax("a comparison (> 90, <= 1, ...) at the end"))?;
    let rest = s.get(i..).unwrap_or("");
    let (op, len) = if rest.starts_with(">=") {
        (Op::Ge, 2)
    } else if rest.starts_with("<=") {
        (Op::Le, 2)
    } else if rest.starts_with("==") {
        (Op::Eq, 2)
    } else if rest.starts_with("!=") {
        (Op::Ne, 2)
    } else if rest.starts_with('>') {
        (Op::Gt, 1)
    } else if rest.starts_with('<') {
        (Op::Lt, 1)
    } else {
        return Err(syntax("a comparison's operator"));
    };
    // Datadog's "an integer or decimal number": no infinity, no NaN (which no value would pass).
    let threshold: f64 = rest.get(len..).unwrap_or("").trim().parse().ok().filter(|t: &f64| t.is_finite()).ok_or_else(|| syntax("a comparison's threshold, a number"))?;
    Ok((s.get(..i).unwrap_or("").trim(), op, threshold))
}

/// The string literal `"..."` that `s` starts with (as written: its escapes kept, [`unescape`]
/// reads them), and what follows.
fn literal(s: &str) -> Result<(&str, &str), MonitorError> {
    let s = s.trim_start();
    let body = s.strip_prefix('"').ok_or_else(|| syntax("a quoted string"))?;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Ok((body.get(..i).unwrap_or(""), body.get(i + 1..).unwrap_or(""))),
            _ => escaped = false,
        }
    }
    Err(syntax("an unterminated string"))
}

/// A string literal's text: `\"` a quote, `\\` a backslash.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => out.extend(it.next()),
            c => out.push(c),
        }
    }
    out
}

/// `name(` `"a"` [`, "b"`] `)` at the start of `s`: its arguments, and what follows.
fn call<'a>(s: &'a str, name: &str) -> Result<Option<(Vec<&'a str>, &'a str)>, MonitorError> {
    let s = s.trim_start().trim_start_matches('.');
    let Some(rest) = s.strip_prefix(name).and_then(|r| r.trim_start().strip_prefix('(')) else { return Ok(None) };
    let mut args = Vec::new();
    let mut rest = rest;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix(')') {
            return Ok(Some((args, r)));
        }
        let (a, r) = literal(rest)?;
        args.push(a);
        rest = r.trim_start().trim_start_matches(',');
    }
}

pub fn parse(q: &str) -> Result<Query, MonitorError> {
    let q = q.trim();
    if q.starts_with("logs(") {
        return parse_records(q, "logs", false);
    }
    // Datadog's Event V2 monitor, `events(query).rollup(…).last(…)`: a log monitor of kiln's events
    // (log records of [`EVENTS`]).
    if q.starts_with("events(") {
        return parse_records(q, "events", false);
    }
    for name in ["trace-analytics", "spans"] {
        if q.strip_prefix(name).is_some_and(|r| r.starts_with('(')) {
            return parse_records(q, name, true);
        }
    }
    if q.starts_with("slo(") {
        return parse_slo(q);
    }
    if q.starts_with("formula(") {
        return Err(syntax("formula(…): a formula monitor reads its options' variables (parse_formula)"));
    }
    if q.strip_prefix("error-tracking").is_some_and(|r| r.starts_with('(')) {
        return parse_error_tracking(q);
    }
    let (left, op, threshold) = comparison(q)?;
    let colon = left.find("):").ok_or_else(|| syntax("time_aggr(window): before the query"))?;
    let (head, query) = (left.get(..=colon).unwrap_or(""), left.get(colon + 2..).unwrap_or("").trim().to_owned());
    if query.is_empty() {
        return Err(syntax("a metric query after the colon"));
    }
    let kind = if let Some((a, rest)) = head.split_once('(').filter(|(_, r)| r.trim_start().starts_with("next_")) {
        // max(next_1w):forecast(...)
        let highest = match a.trim() {
            "max" => true,
            "min" => false,
            other => return Err(syntax(format!("a forecast monitor's aggregation: {other} (max, alerting above; min, below)"))),
        };
        let w = rest.trim().trim_end_matches(')').trim().trim_start_matches("next_");
        let ahead_ms = ahead(w)?;
        if !query.trim_start().starts_with("forecast(") {
            return Err(syntax("next_…: a forecast(...) after the colon"));
        }
        Kind::Forecast { highest, ahead_ms, query }
    } else if let Some(inner) = head.strip_prefix("change(").or_else(|| head.strip_prefix("pct_change(")) {
        // change(avg(last_5m),5m_ago)
        let pct = head.starts_with("pct_change(");
        let inner = inner.strip_suffix(')').ok_or_else(|| syntax("change(...)'s closing parenthesis"))?;
        let (a, rest) = inner.split_once('(').ok_or_else(|| syntax("change(time_aggr(window), shift)"))?;
        let (w, back) = rest.split_once("),").ok_or_else(|| syntax("change(time_aggr(window), shift)"))?;
        Kind::Change { aggr: aggr(a)?, window_ms: window(w, MAX_METRIC_WINDOW_MS)?, shift_ms: shift(back)?, pct, query }
    } else {
        let (a, rest) = head.split_once('(').ok_or_else(|| syntax("time_aggr(window):"))?;
        let w = rest.strip_suffix(')').ok_or_else(|| syntax("time_aggr(window)'s closing parenthesis"))?;
        match current(w)? {
            Some(c) => Kind::Metric { aggr: aggr(a)?, window_ms: c.period.longest_ms(), query, cumulative: Some(c), recovery_ms: None },
            None => Kind::Metric { aggr: aggr(a)?, window_ms: window(w, MAX_METRIC_WINDOW_MS)?, query, cumulative: None, recovery_ms: None },
        }
    };
    // The metric query must parse as one.
    let ast = crate::query::parse::parse(match &kind {
        Kind::Metric { query, .. } | Kind::Change { query, .. } | Kind::Forecast { query, .. } | Kind::RecordsFormula { query, .. } => query,
        Kind::Logs { .. } | Kind::Spans { .. } | Kind::Slo { .. } | Kind::ErrorTracking { .. } => "",
    })
    .map_err(|e| syntax(e.to_string()))?;
    if let Kind::Metric { aggr, window_ms, .. } | Kind::Change { aggr, window_ms, .. } = &kind {
        // Datadog's `percentile(pXX)`: "a matching percentile threshold is automatically specified"
        // from the query's percentile aggregator.
        if *aggr == TimeAggr::Percentile && !matches!(ast.nodes.get(ast.root), Some(Node::Metric(m)) if matches!(m.space, SpaceAgg::Pct(_))) {
            return Err(syntax("percentile(window): a distribution's percentile alone, its pXX the window's (percentile(last_5m):p95:latency{*})"));
        }
        let anomaly = anomaly_of(&ast)?;
        if anomaly.alert_ms.is_some_and(|a| a > *window_ms) || anomaly.step_ms.is_some_and(|s| s > *window_ms) {
            return Err(syntax("anomalies(…, alert_window=, interval=): each within the monitor's window"));
        }
    }
    // Datadog's anomaly and outlier monitors read a rolling window (their pages': "last_…").
    if matches!(kind, Kind::Metric { cumulative: Some(_), .. }) && ast.nodes.iter().any(|n| matches!(n, Node::Call(Func::Anomalies | Func::Outliers, _))) {
        return Err(syntax("a cumulative window (current_…) reads its period so far: anomalies() and outliers() read a rolling one (last_…)"));
    }
    Ok(Query { kind, op, threshold, dynamic: None })
}

/// `current_1h`, `current_1d`, `current_1mo`: a metric monitor's cumulative window, begun where
/// Datadog begins one unless its monitor says ([`Query::starting`]); none for another window.
fn current(s: &str) -> Result<Option<Cumulative>, MonitorError> {
    let Some(p) = s.trim().strip_prefix("current_") else { return Ok(None) };
    let period = match p {
        "1h" => Period::Hour,
        "1d" => Period::Day,
        "1mo" => Period::Month,
        other => return Err(syntax(format!("a cumulative window: current_{other} (current_1h, current_1d, current_1mo)"))),
    };
    Ok(Some(Cumulative::of(period)))
}

/// The longest a formula monitor's window, or its dynamic threshold's, may be: Datadog's longest
/// in its API's own dynamic thresholds (`formula("2 * query1").rollup("avg").last("6mo")`), its
/// months 730 hours ("730 hours (1 month)").
pub const MAX_FORMULA_WINDOW_MS: i64 = 6 * 730 * 3_600_000;

/// Datadog's formula monitor (its API client's own `formula("query1 / query2").rollup("avg")
/// .last("5m") > threshold`): an expression of its queries by their names, each one of its
/// monitor's `options.variables` (`variables`: a name and its metric query), read over its
/// `.last()` window by its `.rollup()` (`avg`, `sum`, `min`, `max`, `pXX`; `avg` unless it says, as
/// a metric monitor's "monitor query aggregation" defaults to "average"). Its threshold a number,
/// or Datadog's `threshold`: then `critical_query` (and `critical_recovery_query`, beside it alone)
/// says each group's, a formula of the same variables with its own `.multiplier()` ("Only
/// supported on metric monitors with a formula query and options['variables']. Cannot be combined
/// with static thresholds.").
pub fn parse_formula(q: &str, variables: &[(&str, &str)], critical_query: Option<&str>, critical_recovery_query: Option<&str>) -> Result<Query, MonitorError> {
    let q = q.trim();
    let (left, op, threshold) = match dynamic_comparison(q) {
        Some((left, op)) => (left, op, None),
        None => {
            let (left, op, t) = comparison(q)?;
            (left, op, Some(t))
        }
    };
    let f = formula_of(left, variables, false)?;
    let metrics_only = |d: Formula| -> Result<Formula, MonitorError> {
        if of_records(&d.query)? {
            return Err(syntax("a dynamic threshold (> threshold, thresholds.critical_query): a formula of metrics (Datadog's: \"only supported on metric monitors with a formula query\")"));
        }
        Ok(d)
    };
    if of_records(&f.query)? {
        if threshold.is_none() || critical_query.is_some() || critical_recovery_query.is_some() {
            return Err(syntax("a dynamic threshold (> threshold, thresholds.critical_query): a formula of metrics (Datadog's: \"only supported on metric monitors with a formula query\")"));
        }
        if f.window_ms > MAX_WINDOW_MS {
            return Err(syntax(format!(".last(…) of logs, spans or events: at most {} hours", MAX_WINDOW_MS / 3_600_000)));
        }
        return Ok(Query { kind: Kind::RecordsFormula { query: f.query, window_ms: f.window_ms }, op, threshold: threshold.unwrap_or(f64::NAN), dynamic: None });
    }
    let dynamic = match (threshold, critical_query, critical_recovery_query) {
        (None, Some(c), r) => Some(Box::new(Dynamic { critical: metrics_only(formula_of(c, variables, true)?)?, critical_recovery: r.map(|r| formula_of(r, variables, true).and_then(metrics_only)).transpose()? })),
        (None, None, _) => return Err(syntax("> threshold: its monitor's thresholds.critical_query says the threshold")),
        (Some(_), None, None) => None,
        (Some(_), _, _) => return Err(syntax("thresholds.critical_query and critical_recovery_query: with `> threshold` alone (Datadog's: \"cannot be combined with static thresholds\")")),
    };
    Ok(Query { kind: Kind::Metric { aggr: f.aggr, window_ms: f.window_ms, query: f.query, cumulative: None, recovery_ms: None }, op, threshold: threshold.unwrap_or(f64::NAN), dynamic })
}

/// Whether a formula's query reads logs, spans or events (records queries) rather than metrics;
/// refused where it reads both: a formula of metrics is read bucket by bucket, one of records over
/// its whole window ([`Kind::RecordsFormula`]).
fn of_records(query: &str) -> Result<bool, MonitorError> {
    let ast = crate::query::parse::parse(query).map_err(|e| syntax(e.to_string()))?;
    let records = ast.nodes.iter().any(|n| matches!(n, Node::Records(_)));
    if records && ast.nodes.iter().any(|n| matches!(n, Node::Metric(_))) {
        return Err(syntax("formula(…): its variables metrics, or logs, spans and events, not both (a formula of metrics is read bucket by bucket, one of records over its whole window)"));
    }
    Ok(records)
}

/// A formula variable of Datadog's event platform (`data_source` `logs`, `spans` or `events`; its
/// `search.query`, `compute.aggregation` and `.metric`, `group_by`'s facets) as kiln's query
/// language writes a records query: `logs("<search>").rollup("<aggregation>"[, "<metric>"])
/// [.by("<facets>")]`, each string quoted as the language reads one (`\"`, `\\`).
pub fn records_query(of: RecordSource, search: &str, aggregation: &str, metric: Option<&str>, by: &[&str]) -> String {
    let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let name = match of {
        RecordSource::Logs => "logs",
        RecordSource::Spans => "spans",
        RecordSource::Events => "events",
    };
    let rollup = match metric {
        Some(m) => format!("{}, {}", quoted(aggregation), quoted(m)),
        None => quoted(aggregation),
    };
    let by = if by.is_empty() { String::new() } else { format!(".by({})", quoted(&by.join(","))) };
    format!("{name}({}).rollup({rollup}){by}", quoted(search))
}

/// A formula monitor's comparison with Datadog's `threshold` keyword for its threshold: what comes
/// before, and its operator.
fn dynamic_comparison(s: &str) -> Option<(&str, Op)> {
    let head = s.trim_end().strip_suffix("threshold")?.trim_end();
    [(">=", Op::Ge), ("<=", Op::Le), ("==", Op::Eq), ("!=", Op::Ne), (">", Op::Gt), ("<", Op::Lt)].into_iter().find_map(|(sym, op)| head.strip_suffix(sym).map(|left| (left.trim_end(), op)))
}

/// `formula("…")[.rollup("…")].last("…")`, and with `threshold` its `.multiplier(x)`, read as a
/// [`Formula`]: its expression's variables' names each its query, in brackets.
fn formula_of(s: &str, variables: &[(&str, &str)], threshold: bool) -> Result<Formula, MonitorError> {
    let Some((args, mut rest)) = call(s, "formula")? else { return Err(syntax("formula(\"query1 / query2\")")) };
    let [expr] = args.as_slice() else { return Err(syntax("formula(\"…\"): one expression of its variables")) };
    let (mut aggr, mut window_ms, mut multiplier) = (TimeAggr::Avg, None, 1.0);
    while !rest.trim().is_empty() {
        if let Some((a, r)) = call(rest, "rollup")? {
            let [m] = a.as_slice() else { return Err(syntax(".rollup(\"avg\")")) };
            aggr = rollup_aggr(m)?;
            rest = r;
        } else if let Some((a, r)) = call(rest, "last")? {
            window_ms = Some(window(a.first().copied().unwrap_or(""), MAX_FORMULA_WINDOW_MS)?);
            rest = r;
        } else if let Some((x, r)) = number_call(rest, "multiplier").filter(|_| threshold) {
            multiplier = x.ok_or_else(|| syntax(".multiplier(1.5): a finite number"))?;
            rest = r;
        } else {
            return Err(syntax(format!("after formula(...): {}", rest.trim())));
        }
    }
    let window_ms = window_ms.ok_or_else(|| syntax(".last(\"5m\")"))?;
    let query = substituted(&unescape(expr), variables)?;
    crate::query::parse::parse(&query).map_err(|e| syntax(e.to_string()))?;
    Ok(Formula { aggr, window_ms, query, multiplier })
}

/// A formula's `.rollup()`: Datadog's `avg`, `sum`, `min`, `max`, or a percentile, `pXX`.
fn rollup_aggr(m: &str) -> Result<TimeAggr, MonitorError> {
    Ok(match m.trim() {
        "avg" => TimeAggr::Avg,
        "sum" => TimeAggr::Sum,
        "min" => TimeAggr::Min,
        "max" => TimeAggr::Max,
        p => {
            let x: f64 = p.strip_prefix('p').and_then(|x| x.parse().ok()).filter(|x: &f64| *x > 0.0 && *x <= 100.0).ok_or_else(|| syntax(format!(".rollup(\"{p}\"): avg, sum, min, max or pXX")))?;
            TimeAggr::Pct((x * 100.0).round() as u16)
        }
    })
}

/// `.name(x)` at the start of `s`, `x` a number: it (none if not a finite one), and what follows.
fn number_call<'a>(s: &'a str, name: &str) -> Option<(Option<f64>, &'a str)> {
    let rest = s.trim_start().trim_start_matches('.').strip_prefix(name)?.trim_start().strip_prefix('(')?;
    let (x, after) = rest.split_once(')')?;
    Some((x.trim().parse::<f64>().ok().filter(|x| x.is_finite()), after))
}

/// A formula's expression with each of its variables' names its query, in brackets: a name not
/// followed by `(` (a function's) is a variable's, or refused; strings (`'desc'`) and numbers
/// (`1e5`) as they are.
fn substituted(expr: &str, variables: &[(&str, &str)]) -> Result<String, MonitorError> {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::with_capacity(expr.len());
    let mut i = 0;
    while let Some(c) = expr.get(i..).and_then(|r| r.chars().next()) {
        let rest = expr.get(i..).unwrap_or("");
        let len = if c == '\'' || c == '"' {
            // A string, to its closing quote (or the end).
            rest.get(1..).and_then(|r| r.find(c)).map_or(rest.len(), |e| e + 2)
        } else if c.is_ascii_digit() || c == '.' {
            rest.find(|c: char| !(word(c) || c == '.')).unwrap_or(rest.len())
        } else if word(c) {
            let len = rest.find(|c: char| !word(c)).unwrap_or(rest.len());
            let (name, after) = rest.split_at(len);
            if !after.trim_start().starts_with('(') {
                let (_, query) = variables.iter().find(|(n, _)| *n == name).ok_or_else(|| syntax(format!("formula(…): {name} names none of its variables")))?;
                out.push('(');
                out.push_str(query);
                out.push(')');
                i += len;
                continue;
            }
            len
        } else {
            c.len_utf8()
        };
        out.push_str(rest.get(..len).unwrap_or(rest));
        i += len.max(1);
    }
    Ok(out)
}

/// An anomaly monitor's reading, from Datadog's named arguments of its `anomalies()`: the last part
/// of its window its share past the band is of (`alert_window`; all of it unless said) and its
/// buckets' width (`interval`, whole seconds; the window's own step unless said), in ms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Anomaly {
    alert_ms: Option<i64>,
    step_ms: Option<i64>,
}

fn anomaly_of(ast: &Ast) -> Result<Anomaly, MonitorError> {
    let mut a = Anomaly::default();
    for node in &ast.nodes {
        let Node::Call(Func::Anomalies, args) = node else { continue };
        for arg in args.iter().filter_map(|&id| ast.nodes.get(id)) {
            match arg {
                Node::Named(k, v) if k == "alert_window" => a.alert_ms = Some(window(v, MAX_METRIC_WINDOW_MS)?),
                Node::Named(k, v) if k == "interval" => {
                    let s = v.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 1.0 && s.fract() == 0.0).ok_or_else(|| syntax(format!("anomalies' interval: {v} (whole seconds, at least 1)")))?;
                    a.step_ms = Some((s * 1000.0) as i64);
                }
                _ => {}
            }
        }
    }
    Ok(a)
}

fn parse_slo(q: &str) -> Result<Query, MonitorError> {
    let (left, op, threshold) = comparison(q)?;
    let Some((args, rest)) = call(left, "slo")? else { return Err(syntax("slo(\"<the SLO's id>\")")) };
    let id = args.first().map(|s| (*s).to_owned()).filter(|s| !s.is_empty()).ok_or_else(|| syntax("slo(\"<the SLO's id>\")"))?;
    let burn = if let Some((a, r)) = call(rest, "burn_rate")? {
        let [long, short] = a.as_slice() else { return Err(syntax("burn_rate(\"1h\", \"5m\"): a long window and a short one")) };
        let (long, short) = (window(long, MAX_WINDOW_MS)?, window(short, MAX_WINDOW_MS)?);
        if short >= long || !r.trim().is_empty() {
            return Err(syntax("burn_rate(long, short): the short window shorter than the long, and nothing after"));
        }
        Some((long, short))
    } else if let Some((a, r)) = call(rest, "error_budget")? {
        if !a.is_empty() || !r.trim().is_empty() {
            return Err(syntax("error_budget(), and nothing after"));
        }
        None
    } else {
        return Err(syntax("slo(\"id\").burn_rate(\"1h\", \"5m\") or slo(\"id\").error_budget()"));
    };
    Ok(Query { kind: Kind::Slo { id, burn }, op, threshold, dynamic: None })
}

/// An Error Tracking monitor's query (`error-tracking(...)`), as Datadog's API has it: a search of
/// the errors, which issues it watches (`.new()` or `.impact()`), by issue, over a window. Kiln's
/// issues are of backends (spans and logs): `.source()` is `all` or `backend` (`all` unless it
/// says), and the rollup a count, Datadog's Error Occurrences.
fn parse_error_tracking(q: &str) -> Result<Query, MonitorError> {
    let (left, op, threshold) = comparison(q)?;
    let Some((args, mut rest)) = call(left, "error-tracking")? else { return Err(syntax("error-tracking(\"search\")")) };
    let search = args.first().map(|s| unescape(s)).unwrap_or_default();
    crate::logs::search::parse(&search).map_err(|e| syntax(e.to_string()))?;
    let (mut source, mut new, mut by, mut window_ms) = (false, None, None, None);
    while !rest.trim().is_empty() {
        if let Some((a, r)) = call(rest, "source")? {
            match a.as_slice() {
                _ if source => return Err(syntax("one .source()")),
                ["all" | "backend"] => source = true,
                [s @ ("browser" | "mobile")] => return Err(syntax(format!(".source(\"{s}\"): kiln's issues are backends' (all, backend)"))),
                _ => return Err(syntax(".source(\"all\") or .source(\"backend\")")),
            }
            rest = r;
        } else if let Some((is_new, (a, r))) = match call(rest, "new")? {
            Some(c) => Some((true, c)),
            None => call(rest, "impact")?.map(|c| (false, c)),
        } {
            if !a.is_empty() || new.is_some() {
                return Err(syntax("one of .new() (new issues and regressions) and .impact() (issues for review or reviewed)"));
            }
            new = Some(is_new);
            rest = r;
        } else if let Some((a, r)) = call(rest, "rollup")? {
            if a.as_slice() != ["count"] {
                return Err(syntax("an Error Tracking monitor's rollup: count (its issues' errors)"));
            }
            rest = r;
        } else if let Some((a, r)) = call(rest, "by")? {
            let mut facets: Vec<String> = Vec::new();
            for f in a.iter().flat_map(|s| s.split(',')).map(|f| f.trim().trim_start_matches('@')).filter(|f| !f.is_empty()) {
                if !ERROR_FACETS.contains(&f) {
                    return Err(syntax(format!("an Error Tracking monitor's group: {f} ({})", ERROR_FACETS.join(", "))));
                }
                if !facets.iter().any(|x| x == f) {
                    facets.push(f.to_owned());
                }
            }
            by = Some(facets);
            rest = r;
        } else if let Some((a, r)) = call(rest, "last")? {
            window_ms = Some(window(a.first().copied().unwrap_or(""), MAX_WINDOW_MS)?);
            rest = r;
        } else {
            return Err(syntax(format!("after error-tracking(...): {}", rest.trim())));
        }
    }
    let new = new.ok_or_else(|| syntax(".new() (new issues and regressions) or .impact() (issues for review or reviewed)"))?;
    let by = by.filter(|b| b.iter().any(|f| f == "issue.id")).ok_or_else(|| syntax(".by(\"issue.id\"): an Error Tracking monitor tells of each issue"))?;
    let window_ms = window_ms.ok_or_else(|| syntax(".last(\"1d\")"))?;
    Ok(Query { kind: Kind::ErrorTracking { search, new, by, window_ms }, op, threshold, dynamic: None })
}

/// A log monitor's query (`logs(...)`), or with `spans` a trace analytics monitor's (`name(...)`:
/// its search a span search, its facets a span's).
fn parse_records(q: &str, name: &str, spans: bool) -> Result<Query, MonitorError> {
    let (left, op, threshold) = comparison(q)?;
    let Some((args, mut rest)) = call(left, name)? else { return Err(syntax(format!("{name}(\"search\")"))) };
    let search = args.first().map(|s| unescape(s)).unwrap_or_default();
    if spans {
        crate::logs::search::parse_spans(&search).map_err(|e| syntax(e.to_string()))?;
    } else {
        crate::logs::search::parse(&search).map_err(|e| syntax(e.to_string()))?;
    }
    let facet_of = |f: &str| if spans { Facet::parse_span(f) } else { Facet::parse(f) };
    let (mut measure, mut by, mut window_ms) = (Measure::Count, Vec::new(), None);
    while !rest.trim().is_empty() {
        if let Some((_, r)) = call(rest, "index")? {
            rest = r;
        } else if let Some((a, r)) = call(rest, "rollup")? {
            measure = Measure::parse(a.first().copied(), a.get(1).copied(), spans)
                .ok_or_else(|| syntax(format!("a log rollup: {} (count; cardinality, sum, avg, min, max of a facet)", a.first().copied().unwrap_or(""))))?;
            rest = r;
        } else if let Some((a, r)) = call(rest, "by")? {
            by = a.iter().flat_map(|s| s.split(',')).filter(|s| !s.trim().is_empty()).map(facet_of).collect();
            rest = r;
        } else if let Some((a, r)) = call(rest, "last")? {
            window_ms = Some(window(a.first().copied().unwrap_or(""), MAX_WINDOW_MS)?);
            rest = r;
        } else {
            return Err(syntax(format!("after {name}(...): {}", rest.trim())));
        }
    }
    let window_ms = window_ms.ok_or_else(|| syntax(".last(\"5m\")"))?;
    let kind = match (spans, name) {
        (true, _) => Kind::Spans { search, measure, by, window_ms },
        (false, "events") => Kind::Logs { search: events_only(&search), measure, by, window_ms },
        (false, _) => Kind::Logs { search, measure, by, window_ms },
    };
    Ok(Query { kind, op, threshold, dynamic: None })
}

/// Where events are kept: log records of this source (kiln's own, its integrations', the API's).
pub const EVENTS: &str = "kiln.event";

/// An event search as an event monitor reads it: events alone (records of [`EVENTS`]).
pub fn events_only(search: &str) -> String {
    let s = search.trim();
    if s.is_empty() || s == "*" { format!("source:{EVENTS}") } else { format!("source:{EVENTS} ({s})") }
}

/// A log search as a log monitor reads it: logs, not events (records of [`EVENTS`]), unless the
/// search names them. Events are records too, and a monitor's own alerts are events at error's
/// severity: a monitor of `status:error` that counted them would count its own alerting.
pub fn logs_only(search: &str) -> String {
    let s = search.trim();
    if s.contains(EVENTS) {
        s.to_owned()
    } else if s.is_empty() || s == "*" {
        format!("-source:{EVENTS}")
    } else {
        format!("({s}) -source:{EVENTS}")
    }
}

/// One group's value, as an evaluation found it (none: the group was there before, with no data
/// now).
#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    /// The group's tags, sorted.
    pub group: Vec<(String, String)>,
    pub value: f64,
    /// What else its state moves by, where its monitor makes it the group's own.
    pub own: Own,
}

impl Value {
    pub fn of(group: Vec<(String, String)>, value: f64) -> Self {
        Self { group, value, own: Own::default() }
    }
}

/// What a group's state moves by besides its value, where its monitor makes it the group's own.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Own {
    /// An anomaly monitor's: the share of its `threshold_windows`' `recovery_window` past the band
    /// (its trigger window's where it names none), which its recovery thresholds are of, recovered
    /// at or below them (Datadog's anomaly page: "a `warning_recovery` value of 0 means that the
    /// monitor recovers from the warning state only when 0% of the points in the `recovery_window`
    /// are anomalous").
    pub share: Option<f64>,
    /// A dynamic threshold's, the group's (Datadog's `critical_query`, `critical_recovery_query`).
    pub critical: Option<f64>,
    pub critical_recovery: Option<f64>,
}

fn reduce(aggr: TimeAggr, values: &[f64]) -> Option<f64> {
    let mut finite = values.iter().copied().filter(|v| v.is_finite());
    match aggr {
        // A percentile's window is one bucket (`Path::Whole`).
        TimeAggr::Last | TimeAggr::Percentile => values.iter().rev().copied().find(|v| v.is_finite()),
        TimeAggr::Avg => {
            let (s, n) = finite.fold((0.0, 0u32), |(s, n), v| (s + v, n + 1));
            (n > 0).then(|| s / f64::from(n))
        }
        TimeAggr::Sum => {
            let first = finite.next()?;
            Some(finite.fold(first, |a, v| a + v))
        }
        TimeAggr::Min => finite.reduce(f64::min),
        TimeAggr::Max => finite.reduce(f64::max),
        TimeAggr::Pct(p) => {
            let mut v: Vec<f64> = finite.collect();
            (!v.is_empty()).then(|| crate::query::eval::percentile(&mut v, f64::from(p) / 100.0))
        }
    }
}

/// The step an evaluation's grid reads its window at: the coarsest of the query grid's nice steps
/// that divides the window and is at most a tenth of it (10 s at the finest: what kiln keeps of
/// most series), so the window is whole buckets, ten at least where it holds ten of 10 s.
fn step_for(window_ms: i64) -> i64 {
    let most = (window_ms / 10).max(10_000);
    [86_400_000, 43_200_000, 14_400_000, 7_200_000, 3_600_000, 1_800_000, 900_000, 600_000, 300_000, 120_000, 60_000, 30_000, 15_000, 10_000, 5_000, 1_000].into_iter().find(|&s| s <= most && window_ms % s == 0).unwrap_or(1_000)
}

/// The window ending at or before `to` that is whole buckets of its step: `(from, to)`.
fn aligned(window_ms: i64, to: i64) -> (i64, i64, i64) {
    let step = step_for(window_ms);
    let end = to.div_euclid(step) * step;
    (end - window_ms, end, step)
}

/// How a metric monitor reads its window: a percentile, the window whole; a sum of a query read
/// `.as_count()`, each query summed before its formula; else bucket by bucket.
fn path_of(aggr: TimeAggr, ast: &Ast) -> Path {
    match aggr {
        TimeAggr::Percentile => Path::Whole,
        TimeAggr::Sum if ast.nodes.iter().any(|n| matches!(n, Node::Metric(m) if m.mode == Some(CountMode::Count))) => Path::Summed,
        _ => Path::Buckets,
    }
}

/// `query` over the window of `window_ms` ending at or before `to`, read on `path`, and the
/// window's step: the window is whole buckets of it (an anomaly monitor's `interval`, when it says
/// one, aligned to it).
fn read_window(src: Source<'_>, query: &str, path: Path, anomaly: Anomaly, window_ms: i64, to: i64) -> Result<(Answer, i64), MonitorError> {
    let (from, to, step) = match anomaly.step_ms.filter(|&s| s > 0) {
        Some(step) => {
            let end = to.div_euclid(step) * step;
            let buckets = window_ms / step + i64::from(window_ms % step != 0);
            (end - buckets * step, end, step)
        }
        None => aligned(window_ms, to),
    };
    let mut req = Request::new(query, from, to);
    req.step = Some(step);
    let a = src.ask_path(&req, path).map_err(|e| MonitorError::Eval(e.to_string()))?;
    Ok((a, step))
}

/// A group with a tag without a value: Datadog's "`N/A` groups are not included in monitors, so tag
/// keys must have a value".
fn not_applicable(s: &Series) -> bool {
    s.tags.iter().any(|(_, v)| v == "N/A")
}

/// `query` over a cumulative window as of `to`: from where its period began to the last whole step
/// of it before `to` ([`Period::step_ms`]), its grid begun at its start; none before a step of it
/// has passed (no data yet, as a rolling window's last bucket waits until it is whole).
fn read_cumulative(src: Source<'_>, query: &str, path: Path, c: Cumulative, to: i64) -> Result<Option<(Answer, i64)>, MonitorError> {
    let began = c.began(to).ok_or_else(|| MonitorError::Eval("a cumulative window's start: past the calendar's years".into()))?;
    let step = c.period.step_ms();
    let end = began + (to - began).div_euclid(step) * step;
    if end <= began {
        return Ok(None);
    }
    let mut req = Request::new(query, began, end);
    req.step = Some(step);
    req.anchored = true;
    let a = src.ask_path(&req, path).map_err(|e| MonitorError::Eval(e.to_string()))?;
    Ok(Some((a, step)))
}

fn metric_values(src: Source<'_>, query: &str, aggr: TimeAggr, window_ms: i64, cumulative: Option<Cumulative>, recovery_ms: Option<i64>, to: i64) -> Result<Vec<Value>, MonitorError> {
    let ast = crate::query::parse::parse(query).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let anomaly = anomaly_of(&ast)?;
    let path = path_of(aggr, &ast);
    let (a, step) = match cumulative {
        Some(c) => match read_cumulative(src, query, path, c, to)? {
            Some(read) => read,
            None => return Ok(Vec::new()),
        },
        None => read_window(src, query, path, anomaly, window_ms, to)?,
    };
    // The buckets of an anomaly monitor's `alert_window` (its `trigger_window`) and of its
    // `recovery_window`, each its window's last.
    let buckets = |ms: i64| usize::try_from(ms / step.max(1)).unwrap_or(usize::MAX).max(1);
    let (alert, recovery) = (anomaly.alert_ms.map(buckets), recovery_ms.map(buckets));
    let mut out = Vec::new();
    if let Some(v) = a.scalar.filter(|v| v.is_finite()) {
        out.push(Value::of(Vec::new(), v));
    }
    for s in a.series.into_iter().filter(|s| !not_applicable(s)) {
        // An `anomalies()` series: the share of its alert window's buckets past its band, as
        // Datadog's anomaly monitors compare it (`>= 1`: all of them); buckets it could not score
        // left out. Its recovery reads its recovery window's share ([`Own::share`]).
        let mut own = Own::default();
        let v = match &s.bounds {
            // An `outliers()` series: 1 an outlier over the window, 0 not (Datadog's `> 0`).
            _ if s.outlier.is_some() => s.outlier.map(|o| f64::from(u8::from(o))),
            Some(b) if b.forecast_from.is_none() => {
                let past = |last: Option<usize>| -> Option<f64> {
                    let first = last.map_or(0, |k| s.values.len().saturating_sub(k));
                    let read: Vec<bool> = (first..s.values.len()).filter_map(|i| b.anomalous(i)).collect();
                    (!read.is_empty()).then(|| read.iter().filter(|&&x| x).count() as f64 / read.len() as f64)
                };
                let v = past(alert);
                own.share = if recovery.is_some() { past(recovery) } else { v };
                v
            }
            _ => reduce(aggr, &s.values),
        };
        if let Some(v) = v {
            let mut group = s.tags;
            group.sort();
            out.push(Value { group, value: v, own });
        }
    }
    Ok(out)
}

/// The query's value a group, as of `now` (ms), the window ending `delay_ms` before it (data a
/// little late still counts), its caller holding the store throughout.
pub fn evaluate(store: &Store, q: &Query, now: i64, delay_ms: i64) -> Result<Vec<Value>, MonitorError> {
    match &q.kind {
        Kind::Logs { .. } => evaluate_records(&store.logs_at(), q, now, delay_ms),
        Kind::Spans { .. } => evaluate_records(&store.spans_at(), q, now, delay_ms),
        _ => evaluate_metrics(Source::Held(store), q, now, delay_ms),
    }
}

/// [`evaluate`] of a shared store, holding it as little as it may: a monitor of metrics read a part
/// at a time (`eval::Lease`: a write waits for one part of its queries at the most), one of logs
/// or spans from its records as they are, read with no hold of it.
pub fn evaluate_shared(store: &crate::lock::RwLock<Store>, q: &Query, now: i64, delay_ms: i64) -> Result<Vec<Value>, MonitorError> {
    match reads_records(q) {
        Some(spans) => {
            let at = {
                let s = store.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                if spans { s.spans_at() } else { s.logs_at() }
            };
            evaluate_records(&at, q, now, delay_ms)
        }
        None => evaluate_metrics(Source::Shared(store), q, now, delay_ms),
    }
}

/// A monitor of metrics' values (its kind's: a metric's, a change's, a forecast's), its queries
/// asked of `src`.
fn evaluate_metrics(src: Source<'_>, q: &Query, now: i64, delay_ms: i64) -> Result<Vec<Value>, MonitorError> {
    let to = now - delay_ms.max(0);
    match &q.kind {
        Kind::Metric { aggr, window_ms, query, cumulative, recovery_ms } => {
            let mut out = metric_values(src, query, *aggr, *window_ms, *cumulative, *recovery_ms, to)?;
            if let Some(d) = &q.dynamic {
                let critical = threshold_values(src, &d.critical, to)?;
                let recovery = d.critical_recovery.as_ref().map(|r| threshold_values(src, r, to)).transpose()?;
                for v in &mut out {
                    v.own.critical = of_group(&critical, &v.group);
                    v.own.critical_recovery = recovery.as_ref().and_then(|r| of_group(r, &v.group));
                }
            }
            Ok(out)
        }
        Kind::Change { aggr, window_ms, shift_ms, pct, query } => {
            let changed = |now: f64, then: f64| -> Option<f64> {
                let change = now - then;
                match *pct {
                    _ if !change.is_finite() => None,
                    false => Some(change),
                    true => (then != 0.0).then(|| change / then.abs() * 100.0),
                }
            };
            let ast = crate::query::parse::parse(query).map_err(|e| MonitorError::Eval(e.to_string()))?;
            if path_of(*aggr, &ast) != Path::Buckets {
                // Datadog's `.as_count()` path (and a percentile's): each window aggregated first,
                // then the change at the window's level.
                let now_v = metric_values(src, query, *aggr, *window_ms, None, None, to)?;
                let then_v = metric_values(src, query, *aggr, *window_ms, None, None, to - shift_ms)?;
                return Ok(now_v
                    .into_iter()
                    .filter_map(|v| {
                        let then = then_v.iter().find(|t| t.group == v.group)?.value;
                        Some(Value::of(v.group, changed(v.value, then)?))
                    })
                    .collect());
            }
            // Its classic path: each bucket against the one `shift` before it, then the window's
            // aggregation ("pct_change() is computed per-bucket and those values are summed over
            // the evaluation window").
            let (now_a, _) = read_window(src, query, Path::Buckets, Anomaly::default(), *window_ms, to)?;
            let (then_a, _) = read_window(src, query, Path::Buckets, Anomaly::default(), *window_ms, to - shift_ms)?;
            let then: HashMap<&Vec<(String, String)>, &Vec<f64>> = then_a.series.iter().map(|t| (&t.tags, &t.values)).collect();
            let mut out = Vec::new();
            for s in now_a.series.iter().filter(|s| !not_applicable(s)) {
                let Some(before) = then.get(&s.tags) else { continue };
                let changes: Vec<f64> = s.values.iter().zip(before.iter()).map(|(&n, &t)| changed(n, t).unwrap_or(f64::NAN)).collect();
                if let Some(value) = reduce(*aggr, &changes) {
                    let mut group = s.tags.clone();
                    group.sort();
                    out.push(Value::of(group, value));
                }
            }
            Ok(out)
        }
        Kind::Forecast { highest, ahead_ms, query } => forecast_values(src, query, *highest, *ahead_ms, to),
        Kind::RecordsFormula { query, window_ms } => records_formula_values(src, query, *window_ms, to),
        Kind::Slo { .. } => Err(MonitorError::Eval("an SLO's monitor is evaluated with its SLO".into())),
        Kind::ErrorTracking { .. } => Err(MonitorError::Eval("an Error Tracking monitor is evaluated with its issues".into())),
        Kind::Logs { .. } | Kind::Spans { .. } => Err(MonitorError::Eval("a monitor of logs or spans reads its records (`evaluate_records`)".into())),
    }
}

/// A formula monitor of logs, spans or events' values ([`Kind::RecordsFormula`]): its window ending
/// at `to` read as one bucket, each of its queries' measure over all of it, then its formula, a
/// value a group (a group with a tag without a value left out, as a metric monitor's).
fn records_formula_values(src: Source<'_>, query: &str, window_ms: i64, to: i64) -> Result<Vec<Value>, MonitorError> {
    let a = src.ask_path(&Request::new(query, to - window_ms, to), Path::Whole).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(v) = a.scalar.filter(|v| v.is_finite()) {
        out.push(Value::of(Vec::new(), v));
    }
    for s in a.series.into_iter().filter(|s| !not_applicable(s)) {
        if let Some(v) = s.values.last().copied().filter(|v| v.is_finite()) {
            let mut group = s.tags;
            group.sort();
            out.push(Value::of(group, v));
        }
    }
    Ok(out)
}

/// A dynamic threshold's values ([`Formula`]), a group's each, times its multiplier.
fn threshold_values(src: Source<'_>, f: &Formula, to: i64) -> Result<Vec<Value>, MonitorError> {
    let mut out = metric_values(src, &f.query, f.aggr, f.window_ms, None, None, to)?;
    for v in &mut out {
        v.value *= f.multiplier;
    }
    Ok(out)
}

/// Group `group`'s value of `values`: its own, else an ungrouped one's (every group's, as a lone
/// ungrouped series is in arithmetic).
fn of_group(values: &[Value], group: &[(String, String)]) -> Option<f64> {
    values.iter().find(|v| v.group == group).or_else(|| values.iter().find(|v| v.group.is_empty())).map(|v| v.value)
}

/// Whether a monitor reads log records or spans (`Some(true)`: spans), which
/// [`evaluate_records`] reads as they were taken, with no hold of the store.
pub fn reads_records(q: &Query) -> Option<bool> {
    match q.kind {
        Kind::Logs { .. } => Some(false),
        Kind::Spans { .. } => Some(true),
        _ => None,
    }
}

/// A log or span monitor's values ([`evaluate`]) from its records as they were taken
/// ([`Store::logs_at`], [`Store::spans_at`]: which, [`reads_records`] says), read with no hold of
/// the store.
pub fn evaluate_records(at: &LogsAt, q: &Query, now: i64, delay_ms: i64) -> Result<Vec<Value>, MonitorError> {
    let to = now - delay_ms.max(0);
    let (parsed, measure, by, window_ms) = match &q.kind {
        Kind::Logs { search, measure, by, window_ms } => (crate::logs::search::parse(&logs_only(search)), measure, by, *window_ms),
        Kind::Spans { search, measure, by, window_ms } => (crate::logs::search::parse_spans(search), measure, by, *window_ms),
        _ => return Err(MonitorError::Eval("a monitor of neither logs nor spans".into())),
    };
    let parsed: Search = parsed.map_err(|e| MonitorError::Eval(e.to_string()))?;
    let ns = |ms: i64| ms.saturating_mul(1_000_000);
    let agg = at.aggregate(&parsed, ns(to - window_ms), ns(to) - 1, by, measure).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let mut out: Vec<Value> = agg.groups.into_iter().map(|(vals, value)| Value::of(by.iter().map(Facet::name).zip(vals).collect(), value)).collect();
    // A count of nothing is a count: an ungrouped monitor of "fewer than 1 a minute" must see zero
    // when nothing came.
    if out.is_empty() && by.is_empty() && *measure == Measure::Count {
        out.push(Value::of(Vec::new(), 0.0));
    }
    Ok(out)
}

/// What a log, span or event monitor's group's records are, for its message's variables (Datadog's
/// matching attribute variables, `{{log.message}}`; a formula's "resolve based on events extracted
/// from the first query") and its notification's samples.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupRecords {
    pub of: RecordSource,
    /// A search of the group's own records (`crate::logs::search`'s; spans' by `parse_spans`): its
    /// query's, its facets' values said.
    pub search: String,
    pub window_ms: i64,
}

/// [`GroupRecords`] of a monitor's group, searched by the facets it has a value of (a notification
/// group of `notify_by` has some of them); none for a monitor of other data.
pub fn group_records(q: &Query, group: &[(String, String)]) -> Option<GroupRecords> {
    let (of, base, by, window_ms) = match &q.kind {
        Kind::Logs { search, by, window_ms, .. } => {
            let of = if search.starts_with(&format!("source:{EVENTS}")) { RecordSource::Events } else { RecordSource::Logs };
            (of, logs_only(search), by.clone(), *window_ms)
        }
        Kind::Spans { search, by, window_ms, .. } => (RecordSource::Spans, search.clone(), by.clone(), *window_ms),
        Kind::RecordsFormula { query, window_ms } => {
            let ast = crate::query::parse::parse(query).ok()?;
            let r = ast.nodes.into_iter().find_map(|n| if let Node::Records(r) = n { Some(r) } else { None })?;
            let base = match r.of {
                RecordSource::Logs => logs_only(&r.text),
                RecordSource::Events => events_only(&r.text),
                RecordSource::Spans => r.text.clone(),
            };
            (r.of, base, r.by, *window_ms)
        }
        _ => return None,
    };
    let mut terms: Vec<String> = Vec::with_capacity(by.len());
    for f in &by {
        let key = f.name();
        let bare = key.trim_start_matches('@');
        let Some((_, value)) = group.iter().find(|(k, _)| k == &key || k == bare) else { continue };
        terms.push(format!("{key}:\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")));
    }
    let base = base.trim();
    let search = match (base.is_empty() || base == "*", terms.is_empty()) {
        (true, true) => "*".to_owned(),
        (true, false) => terms.join(" "),
        (false, true) => base.to_owned(),
        (false, false) => format!("({base}) {}", terms.join(" ")),
    };
    Some(GroupRecords { of, search, window_ms })
}

/// Whether a monitor counts (records, distinct values, an issue's errors; a formula of records
/// each of whose queries does), so a group without data counts 0: Datadog's `on_missing_data`
/// default ("monitors using a count query treat an empty evaluation as `0`").
pub fn counts(q: &Query) -> bool {
    match &q.kind {
        Kind::Logs { measure, .. } | Kind::Spans { measure, .. } => measure.counts(),
        Kind::ErrorTracking { .. } => true,
        Kind::RecordsFormula { query, .. } => crate::query::parse::parse(query).is_ok_and(|ast| ast.nodes.iter().all(|n| !matches!(n, Node::Records(r) if !r.measure.counts()))),
        _ => false,
    }
}

/// The tags a monitor's groups are by, as its groups name them (a metric query's `by {…}`, a
/// records query's facets, an Error Tracking monitor's): what Datadog's `notify_by` may name.
pub fn group_keys(q: &Query) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    match &q.kind {
        Kind::Logs { by, .. } | Kind::Spans { by, .. } => keys.extend(by.iter().map(Facet::name)),
        Kind::ErrorTracking { by, .. } => keys.extend(by.iter().cloned()),
        Kind::Metric { query, .. } | Kind::Change { query, .. } | Kind::Forecast { query, .. } | Kind::RecordsFormula { query, .. } => {
            if let Ok(ast) = crate::query::parse::parse(query) {
                for n in &ast.nodes {
                    match n {
                        Node::Metric(m) => keys.extend(m.by.iter().cloned()),
                        Node::Records(r) => keys.extend(r.by.iter().map(|f| f.name().trim_start_matches('@').to_owned())),
                        Node::Group(tags) => keys.extend(tags.iter().cloned()),
                        _ => {}
                    }
                }
            }
        }
        Kind::Slo { .. } => {}
    }
    keys.sort();
    keys.dedup();
    keys
}

/// The bucket Datadog judges a full window by (its metric monitor page, "Data window"): a window
/// "in minutes" by the minute, "in hours" by ten minutes, "in days" by the hour, "in month" by four
/// hours (kiln's windows are of ms: under an hour, a day, and 730 hours, Datadog's month).
fn full_bucket_ms(window_ms: i64) -> i64 {
    if window_ms < 3_600_000 {
        60_000
    } else if window_ms < 86_400_000 {
        600_000
    } else if window_ms < MAX_METRIC_WINDOW_MS {
        3_600_000
    } else {
        14_400_000
    }
}

/// The groups of a metric monitor (a rolling window's, a change monitor's now) whose window as of
/// `now` is full, as Datadog's "Require a full window of data" judges one: data in the first of
/// [`full_bucket_ms`]'s buckets, and no more than three of them without ("Otherwise, the
/// evaluation is canceled and the monitor state is unchanged"). None for a monitor it does not
/// apply to ("This setting only applies to metric monitors").
pub fn full_windows(src: Source<'_>, q: &Query, now: i64, delay_ms: i64) -> Result<Option<Vec<Vec<(String, String)>>>, MonitorError> {
    let (query, window_ms) = match &q.kind {
        Kind::Metric { query, window_ms, cumulative: None, .. } | Kind::Change { query, window_ms, .. } => (query.as_str(), *window_ms),
        _ => return Ok(None),
    };
    let (from, to, _) = aligned(window_ms, now - delay_ms.max(0));
    let mut req = Request::new(query, from, to);
    req.step = Some(full_bucket_ms(window_ms));
    req.anchored = true;
    let a = src.ask(&req).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let full = |values: &[f64]| values.first().is_some_and(|v| v.is_finite()) && values.iter().filter(|v| !v.is_finite()).count() <= 3;
    let mut out: Vec<Vec<(String, String)>> = a
        .series
        .into_iter()
        .filter(|s| full(&s.values))
        .map(|s| {
            let mut g = s.tags;
            g.sort();
            g
        })
        .collect();
    if a.scalar.is_some_and(f64::is_finite) {
        out.push(Vec::new());
    }
    Ok(Some(out))
}

/// A forecast monitor's values: over the next `ahead_ms` (in [`FORECAST_POINTS`] buckets, or its
/// `interval`'s), each group's forecast's highest upper bound or lowest lower one. A group whose
/// forecast could not be made (too little history) has none.
fn forecast_values(src: Source<'_>, query: &str, highest: bool, ahead_ms: i64, now: i64) -> Result<Vec<Value>, MonitorError> {
    let mut req = Request::new(query, now, now + ahead_ms);
    req.points = FORECAST_POINTS;
    req.now = Some(now);
    let a = src.ask(&req).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let mut out = Vec::new();
    for s in a.series {
        let Some(b) = &s.bounds else { continue };
        let from = b.forecast_from.unwrap_or(s.values.len());
        let edge = if highest { b.upper.get(from..) } else { b.lower.get(from..) };
        let v = edge.into_iter().flatten().copied().filter(|v| v.is_finite()).reduce(if highest { f64::max } else { f64::min });
        if let Some(v) = v {
            let mut group = s.tags;
            group.sort();
            out.push(Value::of(group, v));
        }
    }
    Ok(out)
}

/// A monitor's thresholds (Datadog's `options.thresholds`): the critical one the query states,
/// and a warning and recoveries beside it. `dynamic`: its critical one is a query's instead
/// (Datadog's `> threshold` with `critical_query`), each group's its own ([`Own::critical`]).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Thresholds {
    pub critical: f64,
    pub warning: Option<f64>,
    pub critical_recovery: Option<f64>,
    pub warning_recovery: Option<f64>,
    pub dynamic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum State {
    Ok,
    Warn,
    Alert,
    NoData,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Alert => "ALERT",
            Self::NoData => "NO DATA",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "OK" => Self::Ok,
            "WARN" => Self::Warn,
            "ALERT" => Self::Alert,
            "NO DATA" => Self::NoData,
            _ => return None,
        })
    }
}

/// A group's next state, from its last and its value now (none: no data), and what else is its own
/// (its dynamic thresholds over the monitor's, an anomaly monitor's recovery share). A group with a
/// dynamic threshold and none for it now has no data.
pub fn next(prev: State, value: Option<f64>, own: Own, op: Op, t: &Thresholds, notify_no_data: bool) -> State {
    let no_data = if notify_no_data { State::NoData } else { prev };
    let Some(v) = value.filter(|v| !v.is_nan()) else { return no_data };
    let critical_at = match (own.critical, t.dynamic) {
        (Some(c), _) => c,
        (None, false) => t.critical,
        (None, true) => return no_data,
    };
    let t = Thresholds { critical: critical_at, critical_recovery: own.critical_recovery.or(t.critical_recovery), ..*t };
    // In trouble still: Datadog's recovery thresholds ("> alert threshold: <= alert recovery
    // threshold", and so on), an anomaly's share recovered at or below its recovery threshold.
    let r = own.share.unwrap_or(v);
    let holds = |recovery: Option<f64>, at: f64| match recovery {
        Some(rt) if own.share.is_some() => r > rt,
        Some(rt) => op.passes(r, rt),
        None => op.passes(r, at),
    };
    let warning = t.warning.is_some_and(|w| op.passes(v, w));
    match prev {
        State::Alert if holds(t.critical_recovery, t.critical) => State::Alert,
        _ if op.passes(v, t.critical) => State::Alert,
        State::Warn | State::Alert if t.warning.is_some_and(|w| holds(t.warning_recovery, w)) => State::Warn,
        _ if warning => State::Warn,
        _ => State::Ok,
    }
}

/// What a transition is, for a template's sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub from: State,
    pub to: State,
}

impl Transition {
    fn is(&self, section: &str) -> Option<bool> {
        Some(match section {
            "is_alert" => self.to == State::Alert,
            "is_warning" => self.to == State::Warn,
            "is_no_data" => self.to == State::NoData,
            "is_recovery" => self.to == State::Ok && self.from != State::Ok,
            "is_alert_recovery" => self.to != State::Alert && self.from == State::Alert,
            "is_warning_recovery" => self.to == State::Ok && self.from == State::Warn,
            "is_alert_to_warning" => self.from == State::Alert && self.to == State::Warn,
            "is_warning_to_alert" => self.from == State::Warn && self.to == State::Alert,
            "is_no_data_recovery" => self.from == State::NoData && self.to == State::Ok,
            _ => return None,
        })
    }
}

/// The `@` mentions in a message: where it goes (`@slack-ops`, `@pagerduty-oncall`,
/// `@webhook-deploys`, an e-mail address), in order, once each.
pub fn mentions(message: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, _) in message.match_indices('@') {
        // A mention starts a word.
        if i > 0 && message.as_bytes().get(i - 1).is_some_and(|b| !b.is_ascii_whitespace() && *b != b'(') {
            continue;
        }
        let m: String = message.get(i + 1..).unwrap_or("").chars().take_while(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
        let m = m.trim_end_matches('.').to_owned();
        if !m.is_empty() && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Builder, Kind as SKind, SeriesKey};
    use crate::hash::Keyed;
    use crate::logs::record::Record;

    #[test]
    fn datadog_s_monitor_queries_parse() {
        let q = parse("avg(last_5m):avg:system.cpu.user{env:prod} by {host} > 90").unwrap();
        assert_eq!(q, Query { kind: Kind::Metric { aggr: TimeAggr::Avg, window_ms: 300_000, query: "avg:system.cpu.user{env:prod} by {host}".into(), cumulative: None, recovery_ms: None }, op: Op::Gt, threshold: 90.0, dynamic: None });
        let q = parse("max(last_1h):sum:kiln.ci.jobs.finished{status:failed}.as_count() / sum:kiln.ci.jobs.finished{*}.as_count() >= 0.25").unwrap();
        assert_eq!((q.op, q.threshold), (Op::Ge, 0.25));
        let q = parse("pct_change(avg(last_5m),last_1h):avg:http.requests{*} < -50").unwrap();
        assert!(matches!(q.kind, Kind::Change { pct: true, window_ms: 300_000, shift_ms: 3_600_000, .. }));
        assert_eq!((q.op, q.threshold), (Op::Lt, -50.0));
        let q = parse(r#"logs("service:web status:error").index("*").rollup("count").by("host").last("5m") > 10"#).unwrap();
        assert_eq!(q.kind, Kind::Logs { search: "service:web status:error".into(), measure: Measure::Count, by: vec![Facet::Host], window_ms: 300_000 });
        // A search's quotes escaped, an apostrophe and a comparison of its own within it.
        let q = parse(r#"logs("service:web \"can't connect\" @latency:>5").last("5m") > 2"#).unwrap();
        assert_eq!(q.kind, Kind::Logs { search: r#"service:web "can't connect" @latency:>5"#.into(), measure: Measure::Count, by: Vec::new(), window_ms: 300_000 });
        assert_eq!((q.op, q.threshold), (Op::Gt, 2.0));
        // Datadog's trace analytics monitor: a span search, its facets a span's.
        let q = parse(r#"trace-analytics("service:checkout status:error resource_name:\"GET /cart\" @duration:>250ms").rollup("count").by("resource_name").last("10m") >= 5"#).unwrap();
        assert_eq!(q.kind, Kind::Spans { search: r#"service:checkout status:error resource_name:"GET /cart" @duration:>250ms"#.into(), measure: Measure::Count, by: vec![Facet::Name], window_ms: 600_000 });
        let q = parse(r#"spans("*").rollup("avg", "@duration").last("5m") > 1e9"#).unwrap();
        assert_eq!(q.kind, Kind::Spans { search: "*".into(), measure: Measure::Avg("duration".into()), by: Vec::new(), window_ms: 300_000 });
        assert!(parse(r#"trace-analytics("resource_name:(").last("5m") > 1"#).is_err());
        let q = parse(r#"logs("*").rollup("cardinality", "@user").by("service,@region").last("1h") >= 100"#).unwrap();
        assert_eq!(q.kind, Kind::Logs { search: "*".into(), measure: Measure::Cardinality(Facet::Attr("user".into())), by: vec![Facet::Service, Facet::Attr("region".into())], window_ms: 3_600_000 });
        for bad in ["avg(last_5m):avg:x{*}", "avg(last_5x):avg:x{*} > 1", "median(last_5m):avg:x{*} > 1", "avg(last_9w):avg:x{*} > 1", "avg(last_5m): > 1", r#"logs("x").last("5m")"#, r#"logs("(x").last("5m") > 1"#, "avg(last_5m):avg:x{ > 1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    /// Datadog's Error Tracking monitors as its API has them (its documentation's two examples),
    /// and what kiln refuses of them: browsers' and phones' issues (kiln's are backends'), a rollup
    /// but a count, groups without the issue.
    #[test]
    fn datadog_s_error_tracking_monitors_parse() {
        let q = parse(r#"error-tracking("service:shop env:prod").source("backend").new().rollup("count").by("issue.id").last("1d") > 0"#).unwrap();
        assert_eq!(q.kind, Kind::ErrorTracking { search: "service:shop env:prod".into(), new: true, by: vec!["issue.id".into()], window_ms: 86_400_000 });
        assert_eq!((q.op, q.threshold), (Op::Gt, 0.0));
        let q = parse(r#"error-tracking("*").source("all").impact().rollup("count").by("issue.id,@env").last("1h") >= 100"#).unwrap();
        assert_eq!(q.kind, Kind::ErrorTracking { search: "*".into(), new: false, by: vec!["issue.id".into(), "env".into()], window_ms: 3_600_000 });
        // Its source and its rollup may go unsaid.
        let q = parse(r#"error-tracking("@error.type:ValueError").new().by("issue.id").last("1d") > 0"#).unwrap();
        assert!(matches!(q.kind, Kind::ErrorTracking { new: true, .. }));
        for bad in [
            r#"error-tracking("*").source("browser").new().by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").source("backend").source("all").new().by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").new().impact().by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").new("x").by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").impact().rollup("cardinality", "@usr.email").by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").new().by("env").last("1d") > 0"#,
            r#"error-tracking("*").new().by("issue.id,host").last("1d") > 0"#,
            r#"error-tracking("*").new().last("1d") > 0"#,
            r#"error-tracking("*").new().by("issue.id") > 0"#,
            r#"error-tracking("*").new().by("issue.id").last("2w") > 0"#,
            r#"error-tracking("(x").new().by("issue.id").last("1d") > 0"#,
            r#"error-tracking("*").new().by("issue.id").last("1d")"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // Evaluated by kiln, with its issues.
        assert!(evaluate(&Store::new(crate::hash::Keyed::new()), &parse(r#"error-tracking("*").new().by("issue.id").last("1d") > 0"#).unwrap(), 0, 0).is_err());
    }

    #[test]
    fn states_alert_and_recover_with_hysteresis() {
        let t = Thresholds { critical: 90.0, warning: Some(80.0), critical_recovery: Some(85.0), warning_recovery: Some(70.0), dynamic: false };
        let steps = [(50.0, State::Ok), (82.0, State::Warn), (91.0, State::Alert), (88.0, State::Alert), (84.0, State::Warn), (75.0, State::Warn), (69.0, State::Ok), (95.0, State::Alert), (60.0, State::Ok)];
        let mut s = State::Ok;
        for (v, want) in steps {
            s = next(s, Some(v), Own::default(), Op::Gt, &t, false);
            assert_eq!(s, want, "at {v}");
        }
        // Below: an availability that alerts under 99.
        let t = Thresholds { critical: 99.0, ..Thresholds::default() };
        assert_eq!(next(State::Ok, Some(98.5), Own::default(), Op::Lt, &t, false), State::Alert);
        assert_eq!(next(State::Alert, Some(99.5), Own::default(), Op::Lt, &t, false), State::Ok);
        // No data: the state stands, or is NO DATA when asked.
        assert_eq!(next(State::Alert, None, Own::default(), Op::Gt, &t, false), State::Alert);
        assert_eq!(next(State::Alert, None, Own::default(), Op::Gt, &t, true), State::NoData);
        assert_eq!(next(State::NoData, Some(1.0), Own::default(), Op::Gt, &Thresholds { critical: 5.0, ..Thresholds::default() }, true), State::Ok);
    }

    #[test]
    fn messages_render_datadog_s_template_syntax() {
        let group = vec![("host".to_owned(), "web-1".to_owned()), ("env".to_owned(), "prod".to_owned())];
        let v = Vars { monitor: "CPU high", value: Some(93.25), threshold: 90.0, warn_threshold: Some(80.0), group: &group, link: "https://kiln/monitors/1", extra: &[], ..Vars::default() };
        let template = "{{#is_alert}}CPU on {{host.name}} is {{value}}% (> {{threshold}}) @slack-ops @pagerduty-oncall{{/is_alert}}{{#is_recovery}}{{host.name}} recovered @slack-ops{{/is_recovery}}{{^is_recovery}} [{{group}}]{{/is_recovery}} {{link}} {{unknown.var}}";
        let alert = render(template, Transition { from: State::Ok, to: State::Alert }, &v);
        assert_eq!(alert, "CPU on web-1 is 93.25% (> 90) @slack-ops @pagerduty-oncall [host:web-1,env:prod] https://kiln/monitors/1 {{unknown.var}}");
        let rec = render(template, Transition { from: State::Alert, to: State::Ok }, &v);
        assert_eq!(rec, "web-1 recovered @slack-ops https://kiln/monitors/1 {{unknown.var}}");
        assert_eq!(mentions(&alert), vec!["slack-ops", "pagerduty-oncall"]);
        assert_eq!(mentions("mail ops@example.com, and @webhook-deploys. (@teams-oncall)"), vec!["webhook-deploys", "teams-oncall"]);
        // Unbalanced and deep sections never panic.
        let deep = "{{#is_alert}}".repeat(10_000) + "x" + &"{{/is_alert}}".repeat(9_999) + "{{";
        let _ = render(&deep, Transition { from: State::Ok, to: State::Alert }, &v);
        // Variables besides the group's tags: an Error Tracking monitor's issue's.
        let extra = vec![("issue.attributes.error.type".to_owned(), "ValueError".to_owned())];
        let v = Vars { extra: &extra, ..v };
        let said = render("{{issue.attributes.error.type}} on {{host.name}}: {{issue.attributes.error.stack}}", Transition { from: State::Ok, to: State::Alert }, &v);
        assert_eq!(said, "ValueError on web-1: {{issue.attributes.error.stack}}");
    }

    /// The rest of Datadog's template syntax (its "Variables" page and its "Template Variable
    /// Evaluation" guide, their own examples where they give one): conditions on a variable,
    /// `{{else}}`, renotifications, priorities, the triggered times, recovery thresholds and the
    /// comparator, `{{eval}}` and its functions, `local_time`, `urlencode`, keys with dots and
    /// facets, comments, raw blocks and `{{{…}}}`; what does not read left as written, and nothing
    /// deep or unclosed panics.
    #[test]
    fn messages_take_the_rest_of_datadog_s_template_syntax() {
        let group = vec![("@machine_id".to_owned(), "m-7".to_owned()), ("dot.key.test".to_owned(), "five".to_owned()), ("host".to_owned(), "db-cassandra-1".to_owned()), ("service".to_owned(), "ad server".to_owned())];
        // 2021-05-31: first triggered 14:19 UTC, last 14:27, now 14:49 (the page's table's A, B, ...).
        let at = |h: i64, m: i64| crate::query::calendar::days_from_civil(2021, 5, 31) * 86_400_000 + h * 3_600_000 + m * 60_000;
        let v = Vars {
            monitor: "Disk",
            value: Some(5.0),
            threshold: 90.0,
            warn_threshold: Some(80.0),
            critical_recovery: Some(85.0),
            comparator: Some(Op::Gt),
            group: &group,
            link: "https://kiln/m/1",
            first_triggered: Some(at(14, 19)),
            last_triggered: Some(at(14, 27)),
            now: at(14, 49),
            priority: Some(2),
            ..Vars::default()
        };
        let alert = Transition { from: State::Ok, to: State::Alert };
        let r = |t: &str| render(t, alert, &v);
        assert_eq!(r(r#"{{#is_match "host.name" "db"}}db team{{else}}others{{/is_match}}"#), "db team");
        assert_eq!(r(r#"{{#is_match "host.name" "web" "database"}}x{{else}}y{{/is_match}}{{^is_match "host.name" "db"}}z{{/is_match}}"#), "y");
        assert_eq!(r(r#"{{#is_exact_match "value" "5.0"}}five{{/is_exact_match}}{{#is_exact_match "host.datacenter" ""}} none{{/is_exact_match}}"#), "five none");
        assert_eq!(r(r#"{{#is_exact_match "[dot.key.test].name" "five"}}dotted{{/is_exact_match}} {{[dot.key.test].name}} {{@machine_id.name}}"#), "dotted five m-7");
        assert_eq!(r("{{#is_priority 'P2'}}p2{{/is_priority}}{{#is_priority 'P1'}}p1{{/is_priority}}"), "p2");
        assert_eq!(r("{{#is_warning}}{{#is_alert}}a{{else}}b{{/is_alert}}{{/is_warning}}"), "");
        let renotified = Vars { renotify: true, ..v.clone() };
        let again = "{{^is_renotify}}first{{/is_renotify}}{{#is_renotify}}again{{/is_renotify}}";
        assert_eq!((render(again, alert, &v), render(again, alert, &renotified)), ("first".to_owned(), "again".to_owned()));
        assert_eq!(render("{{#is_no_data_recovery}}back{{/is_no_data_recovery}}", Transition { from: State::NoData, to: State::Ok }, &v), "back");
        assert_eq!(render("{{#is_unknown}}u{{/is_unknown}}", Transition { from: State::Ok, to: State::NoData }, &Vars { unknown: true, ..v.clone() }), "u");
        assert_eq!(r("{{first_triggered_at}} {{last_triggered_at_epoch}} {{triggered_duration_sec}}"), format!("2021-05-31 14:19:00+00:00 {} 1800", at(14, 27)));
        assert_eq!(r("{{local_time 'last_triggered_at' 'Asia/Tokyo'}}"), "2021-05-31 23:27:00+09:00");
        // The comparator a variable, so its `>` HTML-encoded as what any variable says is (Datadog's
        // "Character escape"); `{{{…}}}` says it raw, and the message's own text stays as written.
        assert_eq!(r("{{comparator}} {{threshold}} {{warn_threshold}} {{alert_recovery_threshold}} {{warn_recovery_threshold}} {{ok_threshold}}"), "&gt; 90 80 85 80 ");
        assert_eq!(r("value > {{{comparator}}} {{threshold}}"), "value > > 90");
        assert_eq!(r(r#"{{eval "last_triggered_at_epoch-15*60*1000"}}"#), (at(14, 27) - 900_000).to_string());
        assert_eq!(r(r#"{{#is_exact_match (eval "int(last_triggered_at_epoch / 3600000 % 24)") "8" "14"}}day shift{{/is_exact_match}}"#), "day shift");
        assert_eq!(
            r(r#"{{eval "round(12.376, 2)"}} {{eval "dec(12.25)"}} {{eval "-2^2"}} {{eval "-1 % 24"}} {{eval "humanize_bytes(1500000)"}} {{eval "upper(host.name)"}} {{eval "substring('host:D', 5)"}} {{eval "strip('abchost:Eabc', 'abc')"}}"#),
            "12.38 0.25 -4 23 1.5 MB DB-CASSANDRA-1 D host:E"
        );
        assert_eq!(r(r#"https://kiln/services/{{urlencode "service.name"}}"#), "https://kiln/services/ad%20server");
        let said = render_message("{{#is_alert}}{{override_priority 'P1'}}page{{/is_alert}}{{#is_warning}}{{override_priority 'P4'}}{{/is_warning}}", alert, &v);
        assert_eq!(said, Rendered { text: "page".into(), priority: Some(1) });
        assert_eq!(r("a{{!-- a comment --}}b{{! another }}c{{!-- unclosed dashes }}d {{{{raw}}}}{{ x }} {{value}}{{{{/raw}}}} {{{value}}}"), "abcd {{ x }} {{value}} 5");
        assert_eq!(r(r#"{{{{is_match "host.name" "db"}}}}{{ .matched }} the host{{{{/is_match}}}}"#), "{{ .matched }} the host");
        let unread = r#"{{eval "1 +"}} {{ nope }} {{local_time 'last_triggered_at' 'Mars/Base'}} {{eval "f(1,)"}}"#;
        assert_eq!(r(unread), unread);
        // Its "Character escape" example: a value HTML-encoded, `{{{…}}}` raw, kiln's own links as
        // they are; a tag of several values sorted, by commas; a variable of a record the message
        // reads that the record lacks, empty; a number's point dropped with its zeros.
        let url = "https://status.example.com/check?service=web&region=us-east";
        let tagged = vec![("check".to_owned(), url.to_owned()), ("role".to_owned(), "db_postgres".to_owned()), ("role".to_owned(), "db_cassandra".to_owned())];
        let extra = vec![("log.link".to_owned(), "https://kiln/observe/logs?q=a&range=1h".to_owned()), ("log.message".to_owned(), "a < b".to_owned())];
        let blank = vec!["log".to_owned()];
        let w = Vars { group: &tagged, extra: &extra, blank: &blank, ..v.clone() };
        assert_eq!(
            render(r#"{{check.name}} {{{check.name}}} {{log.link}} {{role.name}} [{{log.attributes.gone}}] {{log.message}} {{eval "1.00001"}} {{span.message}}"#, alert, &w),
            format!("https://status.example.com/check?service=web&amp;region=us-east {url} https://kiln/observe/logs?q=a&range=1h db_cassandra,db_postgres [] a &lt; b 1 {{{{span.message}}}}")
        );
        // An item by its name, as its page's `{{service.links[Runbook]}}`, a name with a space too;
        // a service's variable its definition lacks, empty.
        let items = vec![("service.links[Runbook]".to_owned(), "https://wiki/r".to_owned()), ("service.links[On-call Runbook]".to_owned(), "https://wiki/o".to_owned())];
        let read = vec!["service".to_owned()];
        let s = Vars { extra: &items, blank: &read, ..v.clone() };
        assert_eq!(render("{{service.links[Runbook]}} {{service.links[On-call Runbook]}} [{{service.team}}] {{service.name}}", alert, &s), "https://wiki/r https://wiki/o [] ad server");
        // No recursion: deep brackets read; unclosed sections and blocks never panic.
        assert_eq!(r(&format!("{{{{eval \"{}1{}\"}}}}", "(".repeat(100_000), ")".repeat(100_000))), "1");
        let _ = r(&("{{#is_match \"host.name\" \"db\"}}".repeat(10_000) + "{{{{raw}}}}x{{!--"));
    }

    /// Ten days of a daily cycle every five minutes, a spike in the last few minutes: the anomaly
    /// monitor's value is the share of its window past the band, the spike's bucket alone.
    #[test]
    fn an_anomaly_monitor_reads_the_share_of_its_window_past_the_band() {
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        let now: i64 = 1_800_000_000_000;
        let k = SeriesKey::new("cpu", [("host", "a")], SKind::Gauge).unwrap();
        b.define(&k);
        let mut seed = 5u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        let n = 288 * 10;
        let points: Vec<(i64, f64)> = (0..n)
            .map(|i| {
                let t = now - (n - i) * 300_000;
                let v = 50.0 + 20.0 * (t as f64 / 86_400_000.0 * std::f64::consts::TAU).sin() + noise();
                (t, if i == n - 1 { v + 60.0 } else { v })
            })
            .collect();
        b.run(k.fp, points.iter().copied());
        s.apply(&b.finish()).unwrap();
        let q = parse(r#"slo("abc").burn_rate("1h", "5m") > 14.4"#).unwrap();
        assert!(matches!(&q.kind, Kind::Slo { id, burn: Some((3_600_000, 300_000)) } if id == "abc") && q.op == Op::Gt && q.threshold == 14.4);
        let q = parse(r#"slo("abc").error_budget() < 25"#).unwrap();
        assert!(matches!(&q.kind, Kind::Slo { burn: None, .. }) && q.op == Op::Lt);
        for bad in [r#"slo("").error_budget() < 1"#, r#"slo("a").burn_rate("5m", "1h") > 1"#, r#"slo("a").burn_rate("1h") > 1"#, r#"slo("a").latency() > 1"#, r#"slo("a").error_budget()"#] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let q = parse("avg(last_1h):anomalies(avg:cpu{*}, 'robust', 3) >= 0.05").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        // A 1-hour window at 5-minute buckets: twelve, one of them the spike.
        assert_eq!(got.len(), 1);
        assert!((got[0].value - 1.0 / 12.0).abs() < 1e-9, "{got:?}");
        // Without the spike's bucket, nothing past the band.
        let before = evaluate(&s, &q, now - 300_000, 0).unwrap();
        assert!(before[0].value.abs() < 1e-9, "{before:?}");
        // Datadog's named arguments: the share is of the alert window's last buckets (the spike's
        // one of three, one of one), the direction named.
        let q = parse("avg(last_1h):anomalies(avg:cpu{*}, 'robust', 3, direction='above', alert_window='last_15m') >= 0.05").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        assert!((got[0].value - 1.0 / 3.0).abs() < 1e-9, "{got:?}");
        let q = parse("avg(last_1h):anomalies(avg:cpu{*}, 'robust', 3, alert_window='last_5m') >= 1").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        assert!((got[0].value - 1.0).abs() < 1e-9, "{got:?}");
        // Datadog's `threshold_windows`: its trigger window the alert window; its recovery window's
        // share (the spike's bucket one of six) what its recovery thresholds are of.
        let anomalous = "avg(last_1h):anomalies(avg:cpu{*}, 'robust', 3, alert_window='last_5m') >= 1";
        let q = parse(anomalous).unwrap().windows("last_5m", "last_30m").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        assert!((got[0].value - 1.0).abs() < 1e-9 && got[0].own.share.is_some_and(|r| (r - 1.0 / 6.0).abs() < 1e-9), "{got:?}");
        let alone = evaluate(&s, &parse(anomalous).unwrap(), now, 0).unwrap();
        assert_eq!(alone[0].own.share, Some(alone[0].value), "without a recovery window: its trigger window's");
        for (trigger, recovery) in [("last_15m", "last_30m"), ("last_5m", "last_2h"), ("last_5x", "last_5m")] {
            assert!(parse(anomalous).unwrap().windows(trigger, recovery).is_err(), "{trigger} {recovery}");
        }
        assert!(parse("avg(last_1h):avg:cpu{*} >= 1").unwrap().windows("last_5m", "last_5m").is_err(), "no anomalies(): no threshold_windows");
    }

    /// An anomaly monitor's recovery reads its recovery window's share, recovered at or below its
    /// recovery threshold (Datadog's anomaly page: `critical: 1`, `critical_recovery: 0`, recovered
    /// "only when 0% of the points in the `recovery_window` are anomalous"); a dynamic threshold is
    /// each group's own, and a group without one now has no data.
    #[test]
    fn anomaly_recoveries_and_dynamic_thresholds_move_as_datadog_s_do() {
        let t = Thresholds { critical: 1.0, critical_recovery: Some(0.0), ..Thresholds::default() };
        let share = |r: f64| Own { share: Some(r), ..Own::default() };
        assert_eq!(next(State::Ok, Some(1.0), share(1.0), Op::Ge, &t, false), State::Alert);
        assert_eq!(next(State::Alert, Some(0.0), share(0.2), Op::Ge, &t, false), State::Alert, "its recovery window anomalous still");
        assert_eq!(next(State::Alert, Some(0.0), share(0.0), Op::Ge, &t, false), State::Ok, "none of it: recovered");
        // Datadog's general rule (`>=`: recovered below the recovery threshold) never would at 0.
        assert_eq!(next(State::Alert, Some(0.0), Own::default(), Op::Ge, &t, false), State::Alert);
        let d = Thresholds { dynamic: true, ..Thresholds::default() };
        let at = |c: f64| Own { critical: Some(c), ..Own::default() };
        assert_eq!(next(State::Ok, Some(12.0), at(10.0), Op::Gt, &d, false), State::Alert);
        assert_eq!(next(State::Ok, Some(12.0), at(15.0), Op::Gt, &d, false), State::Ok);
        assert_eq!(next(State::Alert, Some(12.0), Own { critical: Some(15.0), critical_recovery: Some(11.0), ..Own::default() }, Op::Gt, &d, false), State::Alert, "past its recovery threshold still");
        assert_eq!(next(State::Alert, Some(12.0), Own::default(), Op::Gt, &d, true), State::NoData, "no threshold now: no data");
        assert_eq!(next(State::Alert, Some(12.0), Own::default(), Op::Gt, &d, false), State::Alert);
    }

    /// What Datadog's metric monitors take besides: a change's `5m_ago`, `1h_ago` and `1d_ago`
    /// shifts (its API's) and a window's; windows up to 730 hours; `percentile(…)` of a
    /// distribution's percentile alone; anomaly monitors' named arguments, as its page's example
    /// writes them, each within the window; a threshold a finite number.
    #[test]
    fn datadog_s_metric_monitor_forms_parse() {
        for (q, back) in [
            ("change(avg(last_5m),5m_ago):avg:cpu{*} > 1", 300_000),
            ("pct_change(avg(last_1h), 1h_ago):avg:cpu{*} < -50", 3_600_000),
            ("change(max(last_1d),1d_ago):avg:cpu{*} > 1", 86_400_000),
            ("change(avg(last_5m),last_30m):avg:cpu{*} > 1", 1_800_000),
        ] {
            assert!(matches!(parse(q).unwrap().kind, Kind::Change { shift_ms, .. } if shift_ms == back), "{q}");
        }
        assert!(matches!(parse("avg(last_730h):avg:cpu{*} > 1").unwrap().kind, Kind::Metric { window_ms: 2_628_000_000, .. }));
        assert!(matches!(parse("avg(last_30d):avg:cpu{*} > 1").unwrap().kind, Kind::Metric { window_ms: 2_592_000_000, .. }));
        let q = parse("percentile(last_5m):p95:trace.http.request.duration{service:web} by {resource_name} > 0.5").unwrap();
        assert!(matches!(q.kind, Kind::Metric { aggr: TimeAggr::Percentile, window_ms: 300_000, .. }));
        let q = parse("avg(last_1h):anomalies(avg:system.cpu.system{name:cassandra}, 'basic', 3, direction='above', alert_window='last_5m', interval=20, count_default_zero='true') >= 1").unwrap();
        assert_eq!((q.op, q.threshold), (Op::Ge, 1.0));
        for bad in [
            "avg(last_731h):avg:cpu{*} > 1",
            "change(avg(last_5m),5x_ago):avg:cpu{*} > 1",
            "change(avg(last_5m),_ago):avg:cpu{*} > 1",
            "percentile(last_5m):avg:latency{*} > 1",
            "percentile(last_5m):p95:a{*} / p95:b{*} > 1",
            "avg(last_5m):avg:cpu{*} > inf",
            "avg(last_5m):avg:cpu{*} > NaN",
            "avg(last_1h):anomalies(avg:cpu{*}, 'basic', 2, alert_window='last_2h') >= 1",
            "avg(last_1h):anomalies(avg:cpu{*}, 'basic', 2, interval=7200) >= 1",
            "avg(last_1h):anomalies(avg:cpu{*}, 'basic', 2, interval=1.5) >= 1",
            r#"logs("x").last("8d") > 1"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    /// Datadog's evaluation paths: an error rate of counts read `.as_count()` and summed is its
    /// errors' sum over its requests' (its guide's worked example: errors 1 to 5, ten requests a
    /// bucket, 0.3), not the sum of each bucket's rate (1.5, the classic path's, which `max` keeps);
    /// a change's classic path compares bucket with bucket; a group with a tag without a value is no
    /// monitor's; a distribution's `percentile(…)` reads every value of the window, where `avg(…)`
    /// of its percentile averages each bucket's, and a metric that is no distribution has none.
    #[test]
    fn monitors_read_their_window_as_datadog_s_evaluation_paths_do() {
        use crate::dist::{BUCKET_TAG, SCALE, bucket_of, tag_value};
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        // A five-minute window ending at `now`: ten buckets of 30 s.
        let now: i64 = 1_000_200_000;
        let (errors, total) = (SeriesKey::new("req.errors", [("host", "a")], SKind::Count).unwrap(), SeriesKey::new("req.total", [("host", "a")], SKind::Count).unwrap());
        for k in [&errors, &total] {
            b.define(k);
        }
        b.run(errors.fp, (0..5).map(|i| (now - 300_000 + i * 60_000 + 1_000, (i + 1) as f64)));
        b.run(total.fp, (0..5).map(|i| (now - 300_000 + i * 60_000 + 1_000, 10.0)));
        // A load at 10 and 20 by turns a bucket, then at 20 throughout.
        let load = SeriesKey::new("load", [("host", "a")], SKind::Gauge).unwrap();
        b.define(&load);
        b.run(load.fp, (0..20).map(|j| (now - 600_000 + j * 30_000 + 1_000, if j < 10 && j % 2 == 0 { 10.0 } else { 20.0 })));
        // A cpu of a host, and one without a host.
        for (k, v) in [(SeriesKey::new("cpu", [("host", "a")], SKind::Gauge).unwrap(), 5.0), (SeriesKey::new("cpu", [("zone", "z")], SKind::Gauge).unwrap(), 7.0)] {
            b.define(&k);
            b.run(k.fp, [(now - 60_000, v)]);
        }
        // A latency's distribution: 300 values of 1 in one minute, 100 of 100 in another.
        for (t, v, n) in [(now - 240_000, 1.0, 300.0), (now - 120_000, 100.0, 100.0)] {
            let bk = bucket_of(v, SCALE).unwrap();
            let tag = tag_value(SCALE, bk);
            let k = SeriesKey::new("lat.dist", [(BUCKET_TAG, tag.as_str())], SKind::Count).unwrap();
            b.define(&k);
            b.run(k.fp, [(t, n)]);
        }
        s.apply(&b.finish()).unwrap();
        let value = |q: &str| -> Vec<Value> { evaluate(&s, &parse(q).unwrap(), now, 0).unwrap_or_else(|e| panic!("{q}: {e}")) };
        let one = |q: &str| -> f64 {
            let v = value(q);
            assert_eq!(v.len(), 1, "{q}: {v:?}");
            v[0].value
        };
        assert!((one("sum(last_5m):sum:req.errors{*}.as_count() / sum:req.total{*}.as_count() > 0.25") - 0.3).abs() < 1e-12);
        assert!((one("sum(last_5m):sum:req.errors{*} / sum:req.total{*} > 0.25") - 1.5).abs() < 1e-12);
        assert!((one("max(last_5m):sum:req.errors{*}.as_count() / sum:req.total{*}.as_count() > 0.25") - 0.5).abs() < 1e-12);
        // Each bucket's change, then their average: 100% and 0% by turns, 50 (the averages' change
        // would be 33.3).
        assert!((one("pct_change(avg(last_5m),5m_ago):avg:load{*} > 40") - 50.0).abs() < 1e-9);
        assert!((one("change(avg(last_5m),5m_ago):avg:load{*} > 1") - 5.0).abs() < 1e-9);
        assert_eq!(value("avg(last_5m):avg:cpu{*} by {host} > 1"), vec![Value::of(vec![("host".into(), "a".into())], 5.0)]);
        let bound = ((2f64).powi(-SCALE) * 0.5).exp2() - 1.0;
        let within = |got: f64, want: f64| ((got - want) / want).abs() <= bound * 1.000_001;
        let whole = one("percentile(last_5m):p50:lat{*} > 50");
        assert!(within(whole, 1.0), "{whole}");
        let averaged = one("avg(last_5m):p50:lat{*} > 50");
        assert!(within(averaged, 50.5), "{averaged}");
        assert!(evaluate(&s, &parse("percentile(last_5m):p50:cpu{*} > 1").unwrap(), now, 0).is_err());
    }

    /// Datadog's formula monitors (its API client's own `formula("query1 / query2").rollup("avg")
    /// .last("5m") > threshold`, its variables metric queries by their names) and their dynamic
    /// thresholds (`critical_query`, `.multiplier(1.5)`): each group's value and threshold, read as
    /// the monitor's own is; a static threshold read as the metric monitor of the formula is.
    #[test]
    fn formula_monitors_and_dynamic_thresholds_read_as_datadog_s_do() {
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        let now: i64 = 1_000_200_000;
        // An hour of two services' errors and hits a minute: web's rate 0.1, 0.3 in the last five
        // minutes; api's 0.2 throughout.
        for (svc, early, late) in [("web", 1.0, 3.0), ("api", 2.0, 2.0)] {
            let errors = SeriesKey::new("req.errors", [("service", svc)], SKind::Count).unwrap();
            let hits = SeriesKey::new("req.hits", [("service", svc)], SKind::Count).unwrap();
            b.define(&errors);
            b.define(&hits);
            b.run(errors.fp, (0..60).map(|i| (now - (60 - i) * 60_000 + 1_000, if i >= 55 { late } else { early })));
            b.run(hits.fp, (0..60).map(|i| (now - (60 - i) * 60_000 + 1_000, 10.0)));
        }
        s.apply(&b.finish()).unwrap();
        let vars = [("query1", "sum:req.errors{*} by {service}.as_count()"), ("query2", "sum:req.hits{*} by {service}.as_count()")];
        let q = parse_formula(r#"formula("query1 / query2").rollup("avg").last("5m") > threshold"#, &vars, Some(r#"formula("query1 / query2").rollup("p50").last("1h").multiplier(1.5)"#), None).unwrap();
        assert!(q.threshold.is_nan() && q.dynamic.is_some(), "{q:?}");
        let got = evaluate(&s, &q, now, 0).unwrap();
        let of = |svc: &str| got.iter().find(|v| v.group == [("service".to_owned(), svc.to_owned())]).cloned().unwrap_or_else(|| panic!("{svc}: {got:?}"));
        let (web, api) = (of("web"), of("api"));
        // Its threshold a group's own: the median of its hour's rates (five-minute buckets), times 1.5.
        assert!((web.value - 0.3).abs() < 1e-9 && web.own.critical.is_some_and(|t| (t - 0.15).abs() < 1e-9), "{web:?}");
        assert!((api.value - 0.2).abs() < 1e-9 && api.own.critical.is_some_and(|t| (t - 0.3).abs() < 1e-9), "{api:?}");
        let t = Thresholds { dynamic: true, ..Thresholds::default() };
        assert_eq!(next(State::Ok, Some(web.value), web.own, q.op, &t, false), State::Alert);
        assert_eq!(next(State::Ok, Some(api.value), api.own, q.op, &t, false), State::Ok);
        let plain = parse_formula(r#"formula("query1 / query2").last("5m") > 0.25"#, &vars, None, None).unwrap();
        assert_eq!(plain, parse("avg(last_5m):(sum:req.errors{*} by {service}.as_count()) / (sum:req.hits{*} by {service}.as_count()) > 0.25").unwrap());
        assert!(parse_formula(r#"formula("top(query1, 5, 'mean', 'desc') * 1e2").rollup("p99").last("6mo") > 1"#, &vars, None, None).is_ok(), "strings, numbers, a percentile, months");
        for (bad, critical, recovery) in [
            (r#"formula("query1 / query2").last("5m") > threshold"#, None, None),
            (r#"formula("query1 / query2").last("5m") > 0.25"#, Some(r#"formula("query1").last("1h")"#), None),
            (r#"formula("query1 / query2").last("5m") > threshold"#, None, Some(r#"formula("query1").last("1h")"#)),
            (r#"formula("query1 / query3").last("5m") > 1"#, None, None),
            (r#"formula("query1").rollup("median").last("5m") > 1"#, None, None),
            (r#"formula("query1").last("5m").multiplier(2) > 1"#, None, None),
            (r#"formula("query1").last("7mo") > 1"#, None, None),
        ] {
            assert!(parse_formula(bad, &vars, critical, recovery).is_err(), "{bad} {critical:?} {recovery:?}");
        }
        assert!(parse(r#"formula("query1").last("5m") > 1"#).is_err(), "a formula's variables are its monitor's");
    }

    /// Datadog's formula monitors of its event platform (its API client's own RUM and CI examples,
    /// `formula("query2 / query1 * 100").last("15m") >= 0.8` of `search`/`compute`/`group_by`
    /// variables) of logs, spans and events: each query measured over its whole window, as a log
    /// monitor measures its one, not the store's buckets (which align to their step, and split a
    /// window that does not), then the formula, a value a group; refused beside metrics, with a
    /// dynamic threshold (Datadog's: metric monitors'), past a week.
    #[test]
    fn formula_monitors_of_logs_spans_and_events_measure_their_whole_window() {
        let mut s = Store::new(Keyed::new());
        // No multiple of 15 minutes: the window straddles a 15-minute bucket's edge.
        let now: i64 = 1_000_000_000_000 + 7 * 60_000 + 30_000;
        let rec = |at: i64, service: &str, severity: u8, ms: i64| Record {
            ts: at * 1_000_000,
            severity,
            service: service.into(),
            host: "h".into(),
            source: "app".into(),
            message: "req".into(),
            attrs: vec![("ms".into(), ms.to_string())],
            ..Record::default()
        };
        // In the window, a minute apart: web 4 errors of 10 logs, api 1 of 5; web 3 errors an hour
        // before, outside it.
        let mut logs: Vec<Record> = (0..10).map(|i| rec(now - 14 * 60_000 + i * 60_000, "web", if i < 4 { 17 } else { 9 }, 100 + i)).collect();
        logs.extend((0..5).map(|i| rec(now - 13 * 60_000 + i * 60_000, "api", if i == 0 { 17 } else { 9 }, 7)));
        logs.extend((0..3).map(|i| rec(now - 3_600_000 + i * 1_000, "web", 17, 1)));
        let mut b = Builder::new();
        b.logs(&logs);
        s.apply(&b.finish()).unwrap();
        let (errors, all) = (records_query(RecordSource::Logs, "status:error", "count", None, &["service"]), records_query(RecordSource::Logs, "", "count", None, &["service"]));
        assert_eq!(errors, r#"logs("status:error").rollup("count").by("service")"#);
        let vars = [("query1", errors.as_str()), ("query2", all.as_str())];
        let q = parse_formula(r#"formula("query1 / query2 * 100").last("15m") >= 30"#, &vars, None, None).unwrap();
        assert!(matches!(&q.kind, Kind::RecordsFormula { window_ms: 900_000, .. }) && q.threshold == 30.0, "{q:?}");
        let mut got = evaluate(&s, &q, now, 0).unwrap();
        got.sort_by(|a, b| a.group.cmp(&b.group));
        let of = |svc: &str| got.iter().find(|v| v.group == [("service".to_owned(), svc.to_owned())]).map(|v| v.value);
        assert!(got.len() == 2 && of("web").is_some_and(|v| (v - 40.0).abs() < 1e-9) && of("api").is_some_and(|v| (v - 20.0).abs() < 1e-9), "{got:?}");
        // A measure over the whole window: web's ms, 100 to 109, average 104.5.
        let avg = records_query(RecordSource::Logs, "service:web", "avg", Some("@ms"), &[]);
        let q = parse_formula(r#"formula("query1").last("15m") > 100"#, &[("query1", avg.as_str())], None, None).unwrap();
        assert_eq!(evaluate(&s, &q, now, 0).unwrap(), vec![Value::of(Vec::new(), 104.5)]);
        // Events alone, and a count of nothing is 0.
        let events = records_query(RecordSource::Events, "*", "count", None, &[]);
        let q = parse_formula(r#"formula("query1").last("5m") > 0"#, &[("query1", events.as_str())], None, None).unwrap();
        assert_eq!(evaluate(&s, &q, now, 0).unwrap(), vec![Value::of(Vec::new(), 0.0)]);
        // A shared store reads the same.
        let shared = crate::lock::RwLock::new(s);
        let q = parse_formula(r#"formula("query1 / query2 * 100").last("15m") >= 30"#, &vars, None, None).unwrap();
        assert_eq!(evaluate_shared(&shared, &q, now, 0).unwrap(), evaluate(&shared.read().unwrap(), &q, now, 0).unwrap());
        // Spans by their facets, a search's quotes kept.
        assert_eq!(
            records_query(RecordSource::Spans, r#"service:"a b""#, "pc99", Some("@duration"), &["resource_name", "@http.status_code"]),
            r#"spans("service:\"a b\"").rollup("pc99", "@duration").by("resource_name,@http.status_code")"#
        );
        let metric = [("query1", errors.as_str()), ("query2", "sum:req.hits{*}.as_count()")];
        for (bad, vars, critical) in [
            (r#"formula("query1 / query2").last("5m") > 1"#, &metric[..], None),
            (r#"formula("query1 / query2").last("5m") > threshold"#, &vars[..], Some(r#"formula("query1").last("1h")"#)),
            (r#"formula("query1 / query2").last("8d") > 1"#, &vars[..], None),
            (r#"formula("query2").last("5m") > threshold"#, &metric[..], Some(r#"formula("query1").last("1h")"#)),
        ] {
            assert!(parse_formula(bad, vars, critical, None).is_err(), "{bad} {critical:?}");
        }
    }

    /// Datadog's cumulative windows (its API client's own `avg(current_1mo):…`, its Terraform
    /// provider's `avg(current_1h):…` with `hour_starts`): the period so far, from where it began to
    /// its last whole step (a minute of an hour, ten of a day, half an hour of a month), begun where
    /// `evaluation_window` says, in its timezone; nothing before a step of it has passed; held to
    /// Datadog's rules for `evaluation_window`.
    #[test]
    fn a_cumulative_window_reads_its_period_so_far() {
        let day = |y, m, d| crate::query::calendar::days_from_civil(y, m, d) * 86_400_000;
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        // A count of one a minute, a second into each, from 2026-02-28 through 2026-03-01 (UTC).
        let k = SeriesKey::new("hits", [("host", "a")], SKind::Count).unwrap();
        b.define(&k);
        b.run(k.fp, (0..2 * 24 * 60).map(|i| (day(2026, 2, 28) + i * 60_000 + 1_000, 1.0)));
        s.apply(&b.finish()).unwrap();
        // 10:30:30 on the 1st of March, UTC; New York is at 05:30:30 (EST, its clocks change on the 8th).
        let now = day(2026, 3, 1) + 10 * 3_600_000 + 30 * 60_000 + 30_000;
        let sum = |w: &str, starts: (Option<u32>, Option<&str>, Option<u32>, Option<&str>), at: i64| -> Vec<Value> {
            let q = parse(&format!("sum({w}):sum:hits{{*}}.as_count() > 0")).unwrap().starting(starts.0, starts.1, starts.2, starts.3).unwrap();
            evaluate(&s, &q, at, 0).unwrap_or_else(|e| panic!("{w}: {e}"))
        };
        let none = (None, None, None, None);
        let one = |w: &str, starts, at| -> f64 {
            let v = sum(w, starts, at);
            assert_eq!(v.len(), 1, "{w}: {v:?}");
            v[0].value
        };
        assert_eq!(one("current_1h", none, now), 30.0);
        assert_eq!(one("current_1h", (Some(45), None, None, None), now), 45.0);
        assert_eq!(one("current_1d", none, now), 630.0);
        assert_eq!(one("current_1d", (None, Some("04:00"), Some(1), None), now), 390.0);
        assert_eq!(one("current_1d", (None, None, None, Some("America/New_York")), now), 330.0);
        assert_eq!(one("current_1mo", none, now), 630.0);
        assert_eq!(one("current_1mo", (None, Some("00:00"), Some(1), Some("America/New_York")), now), 330.0);
        // Before its first minute is whole, an hour has no value yet.
        assert!(sum("current_1h", none, day(2026, 3, 1) + 10 * 3_600_000 + 30_000).is_empty());
        let q = parse("avg(current_1mo):avg:system.load.5{*} > 0.5").unwrap();
        assert!(matches!(q.kind, Kind::Metric { cumulative: Some(Cumulative { period: Period::Month, .. }), .. }));
        let starting = |q: &str, starts: (Option<u32>, Option<&str>, Option<u32>, Option<&str>)| parse(q).unwrap().starting(starts.0, starts.1, starts.2, starts.3);
        assert!(starting("avg(current_1h):avg:x{*} > 1", (Some(30), None, None, None)).is_ok());
        for (q, starts) in [
            ("avg(last_1h):avg:x{*} > 1", (Some(0), None, None, None)),
            ("avg(current_1h):avg:x{*} > 1", (Some(60), None, None, None)),
            ("avg(current_1h):avg:x{*} > 1", (Some(5), Some("04:00"), Some(1), None)),
            ("avg(current_1d):avg:x{*} > 1", (Some(5), None, None, None)),
            ("avg(current_1mo):avg:x{*} > 1", (None, Some("04:00"), Some(2), None)),
            ("avg(current_1d):avg:x{*} > 1", (None, Some("25:00"), Some(1), None)),
            ("avg(current_1d):avg:x{*} > 1", (None, Some("04:00"), Some(1), Some("Mars/Olympus"))),
        ] {
            assert!(starting(q, starts).is_err(), "{q} {starts:?}");
        }
        for bad in [
            "avg(current_1w):avg:x{*} > 1",
            "change(avg(current_1h),1h_ago):avg:x{*} > 1",
            "avg(current_1h):anomalies(avg:x{*}, 'basic', 2) >= 1",
            "avg(current_1h):outliers(avg:x{*} by {host}, 'dbscan', 3) > 0",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn evaluation_reads_the_window_by_group() {
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        // On the 30 s grid a 5-minute window reads at.
        let now = 999_990_000;
        for (host, base) in [("a", 50.0), ("b", 95.0)] {
            let k = SeriesKey::new("cpu", [("host", host)], SKind::Gauge).unwrap();
            b.define(&k);
            // The last ten minutes, a point every 10 s; the last five minutes a little higher.
            b.run(k.fp, (0..60).map(|i| (now - 600_000 + i * 10_000, if i >= 30 { base + 1.0 } else { base })));
        }
        s.apply(&b.finish()).unwrap();
        let q = parse("avg(last_5m):avg:cpu{*} by {host} > 90").unwrap();
        let mut got = evaluate(&s, &q, now, 0).unwrap();
        got.sort_by(|a, b| a.group.cmp(&b.group));
        assert_eq!(got.len(), 2);
        assert!((got[0].value - 51.0).abs() < 1e-9 && (got[1].value - 96.0).abs() < 1e-9, "{got:?}");
        let q = parse("change(avg(last_5m),last_5m):avg:cpu{*} by {host} > 0").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        assert!(got.iter().all(|v| (v.value - 1.0).abs() < 1e-9), "{got:?}");
        // Logs: a count, zero when nothing came.
        let q = parse(r#"logs("status:error").last("5m") > 3"#).unwrap();
        assert_eq!(evaluate(&s, &q, now, 0).unwrap(), vec![Value::of(Vec::new(), 0.0)]);
        // An error logged, and an alert said (an event, at error's severity): the monitor counts the
        // log, not its own alert; a monitor of events counts the event.
        let rec = |source: &str, message: &str| Record { ts: (now - 60_000) * 1_000_000, severity: 17, service: "api".into(), host: "a".into(), source: source.into(), message: message.into(), ..Record::default() };
        let mut b = Builder::new();
        b.logs(&[rec("app", "connection refused"), rec(EVENTS, "errors in api WARN -> ALERT")]);
        s.apply(&b.finish()).unwrap();
        let count = |q: &str| evaluate(&s, &parse(q).unwrap(), now, 0).unwrap().first().map(|v| v.value);
        assert_eq!(count(r#"logs("status:error").last("5m") > 3"#), Some(1.0));
        assert_eq!(count(r#"logs("*").last("5m") > 3"#), Some(1.0));
        assert_eq!(count(r#"logs("source:kiln.event status:error").last("5m") > 3"#), Some(1.0));
        assert_eq!(logs_only("service:api OR status:error"), "(service:api OR status:error) -source:kiln.event");
        // Datadog's Event V2 monitor, `events(…)`: events alone.
        assert_eq!(count(r#"events("*").rollup("count").last("5m") > 0"#), Some(1.0));
        assert_eq!(count(r#"events("connection").last("5m") > 0"#), Some(0.0));
        assert!(matches!(parse(r#"events("status:error").rollup("cardinality", "@host").by("service").last("1h") > 1"#).unwrap().kind, Kind::Logs { ref search, .. } if search == "source:kiln.event (status:error)"));
        // A measure's percentiles, as Datadog's log monitors take them (`median`, `pc75` … `pc99`):
        // within 1.09% (a distribution's bucket) of the values' own, between their least and most.
        let mut b = Builder::new();
        let timed: Vec<Record> = (1..=100)
            .map(|i| Record { ts: (now - 120_000) * 1_000_000 + i, service: "timed".into(), source: "app".into(), message: "done".into(), attrs: vec![("ms".into(), i.to_string())], ..Record::default() })
            .collect();
        b.logs(&timed);
        s.apply(&b.finish()).unwrap();
        let count = |q: &str| evaluate(&s, &parse(q).unwrap(), now, 0).unwrap().first().map(|v| v.value);
        for (rollup, want) in [("median", 50.5), ("pc75", 75.25), ("pc95", 95.05), ("pc99", 99.01)] {
            let got = count(&format!(r#"logs("service:timed").rollup("{rollup}", "@ms").last("5m") > 0"#)).unwrap_or(f64::NAN);
            assert!((got - want).abs() <= want * 0.0109 + 1.0, "{rollup}: {got}, the values' {want}");
        }
        // Every window is whole buckets, at least ten.
        for w in [60_000, 300_000, 420_000, 900_000, 3_600_000, 86_400_000, 7 * 86_400_000] {
            let st = step_for(w);
            assert!(w % st == 0 && w / st >= (w / 10_000).min(10), "{w}: {st}");
        }
        // Of a shared store (its metrics read a part at a time, its logs as they are): the same.
        let shared = crate::lock::RwLock::new(s);
        for q in [
            "avg(last_5m):avg:cpu{*} by {host} > 90",
            "change(avg(last_5m),last_5m):avg:cpu{*} by {host} > 0",
            r#"logs("status:error").last("5m") > 3"#,
            r#"logs("source:kiln.event status:error").last("5m") > 3"#,
        ] {
            let q = parse(q).unwrap();
            let held = evaluate(&shared.read().unwrap(), &q, now, 0).unwrap();
            assert_eq!(evaluate_shared(&shared, &q, now, 0).unwrap(), held, "{q:?}");
        }
    }

    /// Datadog's forecast monitor: two hosts' disks, one filling a percent an hour from 80 and one
    /// steady at 50; `max(next_1d):forecast(..., 'linear', 2) >= 95` alerts the filling one alone,
    /// its value the forecast's highest upper bound over the next day.
    #[test]
    fn a_forecast_monitor_alerts_what_will_cross_its_threshold_within_its_horizon() {
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        let now: i64 = 1_800_000_000_000;
        let mut seed = 2u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        for (host, rate) in [("filling", 1.0), ("steady", 0.0)] {
            let k = SeriesKey::new("disk", [("host", host)], SKind::Gauge).unwrap();
            b.define(&k);
            let n = 24 * 6 * 4;
            let points: Vec<(i64, f64)> = (0..n)
                .map(|i| {
                    let t = now - (n - 1 - i) * 600_000;
                    let base = if rate > 0.0 { 80.0 } else { 50.0 };
                    (t, base + rate * (t - now) as f64 / 3_600_000.0 + 0.2 * noise())
                })
                .collect();
            b.run(k.fp, points.iter().copied());
        }
        s.apply(&b.finish()).unwrap();
        let q = parse("max(next_1d):forecast(avg:disk{*} by {host}, 'linear', 2) >= 95").unwrap();
        assert!(matches!(q.kind, Kind::Forecast { highest: true, ahead_ms: 86_400_000, .. }));
        let got = evaluate(&s, &q, now, 0).unwrap();
        let value = |h: &str| got.iter().find(|v| v.group.iter().any(|(_, x)| x == h)).map(|v| v.value);
        let (filling, steady) = (value("filling").unwrap(), value("steady").unwrap());
        assert!(filling >= 95.0, "the filling disk's forecast reaches {filling}");
        assert!(steady < 60.0, "the steady disk's stays {steady}");
        // Below: min with <=, the lowest lower bound.
        let low = parse("min(next_12h):forecast(avg:disk{*} by {host}, 'linear', 2) <= 40").unwrap();
        let got = evaluate(&s, &low, now, 0).unwrap();
        assert!(got.iter().all(|v| v.value > 40.0), "{got:?}");
        // Of a shared store, read a part at a time: the same.
        let shared = crate::lock::RwLock::new(s);
        for q in [&q, &low] {
            assert_eq!(evaluate_shared(&shared, q, now, 0).unwrap(), evaluate(&shared.read().unwrap(), q, now, 0).unwrap());
        }
        for bad in ["max(next_10m):forecast(avg:disk{*}, 'linear', 2) >= 95", "max(next_1y):forecast(avg:disk{*}, 'linear', 2) >= 95", "avg(next_1d):forecast(avg:disk{*}, 'linear', 2) >= 95", "max(next_1d):avg:disk{*} >= 95"] {
            assert!(parse(bad).is_err(), "{bad} parsed");
        }
    }

    /// Datadog's outlier monitor, `avg(last_1h):outliers(..., 'dbscan', 1) > 0`: each host's value 1
    /// if it is an outlier of its group over the window, 0 if not.
    #[test]
    fn an_outlier_monitor_reads_one_for_the_member_unlike_its_peers() {
        let mut s = Store::new(Keyed::new());
        let mut b = Builder::new();
        let now: i64 = 1_800_000_000_000;
        let mut seed = 17u64;
        let mut noise = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        for (host, level) in [("a", 40.0), ("b", 41.0), ("c", 39.0), ("d", 40.0), ("odd", 80.0)] {
            let k = SeriesKey::new("cpu", [("host", host)], SKind::Gauge).unwrap();
            b.define(&k);
            let points: Vec<(i64, f64)> = (0..60).map(|i| (now - (60 - i) * 60_000, level + 3.0 * noise())).collect();
            b.run(k.fp, points.iter().copied());
        }
        s.apply(&b.finish()).unwrap();
        let q = parse("avg(last_1h):outliers(avg:cpu{*} by {host}, 'dbscan', 1) > 0").unwrap();
        let got = evaluate(&s, &q, now, 0).unwrap();
        assert_eq!(got.len(), 5, "{got:?}");
        for v in &got {
            let odd = v.group.iter().any(|(_, h)| h == "odd");
            assert_eq!(v.value, if odd { 1.0 } else { 0.0 }, "{v:?}");
        }
    }
}
