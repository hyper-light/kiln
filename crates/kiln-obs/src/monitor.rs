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
//! logs   := "logs(" '"' search '"' ")" [".index(" '"' * '"' ")"] ".rollup(" '"' measure '"' ["," '"' facet '"'] ")"
//!           [".by(" '"' facets '"' ")"] ".last(" '"' window '"' ")" op threshold
//! spans  := ("trace-analytics(" | "spans(") '"' search '"' ")" ... as logs   (a span search, its facets a span's)
//! slo    := "slo(" '"' id '"' ")" ( ".burn_rate(" '"' long '"' "," '"' short '"' ")" | ".error_budget()" ) op threshold
//! errors := "error-tracking(" '"' search '"' ")" [".source(" '"' ("all" | "backend") '"' ")"] (".new()" | ".impact()")
//!           [".rollup(" '"' "count" '"' ")"] ".by(" '"' "issue.id" ["," facets] '"' ")" ".last(" '"' window '"' ")" op threshold
//! time_aggr := avg | sum | min | max | last
//! window := last_<n>(m|h|d|w)  (logs: <n>(m|h|d|w))
//! op     := > | >= | < | <= | == | !=
//! ```
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

use crate::logs::search::Search;
use crate::logs::store::{Facet, Measure};
use crate::query::eval::{Request, Source};
use crate::store::{LogsAt, Store};

/// The longest a monitor's window may be: a week (Datadog's longest, `last_1w`), so that one
/// evaluation reads a bounded range.
pub const MAX_WINDOW_MS: i64 = 7 * 86_400_000;
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

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Metric { aggr: TimeAggr, window_ms: i64, query: String },
    /// The value now against the value `shift_ms` before (`pct`: as a percent of it).
    Change { aggr: TimeAggr, window_ms: i64, shift_ms: i64, pct: bool, query: String },
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
    /// The threshold the query states (the critical one).
    pub threshold: f64,
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

/// `last_5m`, `last_1h`, `5m` (a log monitor's), ... in ms.
fn window(s: &str) -> Result<i64, MonitorError> {
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
        _ => return Err(syntax(format!("a window's unit: {unit}"))),
    };
    let w = n.checked_mul(ms).filter(|&w| w > 0 && w <= MAX_WINDOW_MS).ok_or_else(|| syntax(format!("a window of {s}: at most a week")))?;
    Ok(w)
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
        other => return Err(syntax(format!("a time aggregation: {other} (avg, sum, min, max, last)"))),
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
    let threshold: f64 = rest.get(len..).unwrap_or("").trim().parse().map_err(|_| syntax("a comparison's threshold, a number"))?;
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
    for name in ["trace-analytics", "spans"] {
        if q.strip_prefix(name).is_some_and(|r| r.starts_with('(')) {
            return parse_records(q, name, true);
        }
    }
    if q.starts_with("slo(") {
        return parse_slo(q);
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
        // change(avg(last_5m),last_1h)
        let pct = head.starts_with("pct_change(");
        let inner = inner.strip_suffix(')').ok_or_else(|| syntax("change(...)'s closing parenthesis"))?;
        let (a, rest) = inner.split_once('(').ok_or_else(|| syntax("change(time_aggr(window), shift)"))?;
        let (w, shift) = rest.split_once("),").ok_or_else(|| syntax("change(time_aggr(window), shift)"))?;
        Kind::Change { aggr: aggr(a)?, window_ms: window(w)?, shift_ms: window(shift)?, pct, query }
    } else {
        let (a, rest) = head.split_once('(').ok_or_else(|| syntax("time_aggr(window):"))?;
        let w = rest.strip_suffix(')').ok_or_else(|| syntax("time_aggr(window)'s closing parenthesis"))?;
        Kind::Metric { aggr: aggr(a)?, window_ms: window(w)?, query }
    };
    // The metric query must parse as one.
    crate::query::parse::parse(match &kind {
        Kind::Metric { query, .. } | Kind::Change { query, .. } | Kind::Forecast { query, .. } => query,
        Kind::Logs { .. } | Kind::Spans { .. } | Kind::Slo { .. } | Kind::ErrorTracking { .. } => "",
    })
    .map_err(|e| syntax(e.to_string()))?;
    Ok(Query { kind, op, threshold })
}

fn parse_slo(q: &str) -> Result<Query, MonitorError> {
    let (left, op, threshold) = comparison(q)?;
    let Some((args, rest)) = call(left, "slo")? else { return Err(syntax("slo(\"<the SLO's id>\")")) };
    let id = args.first().map(|s| (*s).to_owned()).filter(|s| !s.is_empty()).ok_or_else(|| syntax("slo(\"<the SLO's id>\")"))?;
    let burn = if let Some((a, r)) = call(rest, "burn_rate")? {
        let [long, short] = a.as_slice() else { return Err(syntax("burn_rate(\"1h\", \"5m\"): a long window and a short one")) };
        let (long, short) = (window(long)?, window(short)?);
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
    Ok(Query { kind: Kind::Slo { id, burn }, op, threshold })
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
            window_ms = Some(window(a.first().copied().unwrap_or(""))?);
            rest = r;
        } else {
            return Err(syntax(format!("after error-tracking(...): {}", rest.trim())));
        }
    }
    let new = new.ok_or_else(|| syntax(".new() (new issues and regressions) or .impact() (issues for review or reviewed)"))?;
    let by = by.filter(|b| b.iter().any(|f| f == "issue.id")).ok_or_else(|| syntax(".by(\"issue.id\"): an Error Tracking monitor tells of each issue"))?;
    let window_ms = window_ms.ok_or_else(|| syntax(".last(\"1d\")"))?;
    Ok(Query { kind: Kind::ErrorTracking { search, new, by, window_ms }, op, threshold })
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
            window_ms = Some(window(a.first().copied().unwrap_or(""))?);
            rest = r;
        } else {
            return Err(syntax(format!("after {name}(...): {}", rest.trim())));
        }
    }
    let window_ms = window_ms.ok_or_else(|| syntax(".last(\"5m\")"))?;
    let kind = if spans { Kind::Spans { search, measure, by, window_ms } } else { Kind::Logs { search, measure, by, window_ms } };
    Ok(Query { kind, op, threshold })
}

/// Where events are kept: log records of this source (kiln's own, its integrations', the API's).
pub const EVENTS: &str = "kiln.event";

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
}

fn reduce(aggr: TimeAggr, values: &[f64]) -> Option<f64> {
    let mut finite = values.iter().copied().filter(|v| v.is_finite());
    match aggr {
        TimeAggr::Last => values.iter().rev().copied().find(|v| v.is_finite()),
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

fn metric_values(src: Source<'_>, query: &str, aggr: TimeAggr, window_ms: i64, to: i64) -> Result<Vec<Value>, MonitorError> {
    let (from, to, step) = aligned(window_ms, to);
    let mut req = Request::new(query, from, to);
    req.step = Some(step);
    let a = src.ask(&req).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(v) = a.scalar.filter(|v| v.is_finite()) {
        out.push(Value { group: Vec::new(), value: v });
    }
    for s in a.series {
        // An `anomalies()` series: the share of its window's buckets past its band, as Datadog's
        // anomaly monitors compare it (`>= 1`: all of them); buckets it could not score left out.
        let v = match &s.bounds {
            // An `outliers()` series: 1 an outlier over the window, 0 not (Datadog's `> 0`).
            _ if s.outlier.is_some() => s.outlier.map(|o| f64::from(u8::from(o))),
            Some(b) if b.forecast_from.is_none() => {
                let read: Vec<bool> = (0..s.values.len()).filter_map(|i| b.anomalous(i)).collect();
                (!read.is_empty()).then(|| read.iter().filter(|&&x| x).count() as f64 / read.len() as f64)
            }
            _ => reduce(aggr, &s.values),
        };
        if let Some(v) = v {
            let mut group = s.tags;
            group.sort();
            out.push(Value { group, value: v });
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
        Kind::Metric { aggr, window_ms, query } => metric_values(src, query, *aggr, *window_ms, to),
        Kind::Change { aggr, window_ms, shift_ms, pct, query } => {
            let now_v = metric_values(src, query, *aggr, *window_ms, to)?;
            let then_v = metric_values(src, query, *aggr, *window_ms, to - shift_ms)?;
            Ok(now_v
                .into_iter()
                .filter_map(|v| {
                    let then = then_v.iter().find(|t| t.group == v.group)?.value;
                    let change = v.value - then;
                    let value = if *pct { if then == 0.0 { return None } else { change / then.abs() * 100.0 } } else { change };
                    Some(Value { group: v.group, value })
                })
                .collect())
        }
        Kind::Forecast { highest, ahead_ms, query } => forecast_values(src, query, *highest, *ahead_ms, to),
        Kind::Slo { .. } => Err(MonitorError::Eval("an SLO's monitor is evaluated with its SLO".into())),
        Kind::ErrorTracking { .. } => Err(MonitorError::Eval("an Error Tracking monitor is evaluated with its issues".into())),
        Kind::Logs { .. } | Kind::Spans { .. } => Err(MonitorError::Eval("a monitor of logs or spans reads its records (`evaluate_records`)".into())),
    }
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
    let mut out: Vec<Value> = agg.groups.into_iter().map(|(vals, value)| Value { group: by.iter().map(Facet::name).zip(vals).collect(), value }).collect();
    // A count of nothing is a count: an ungrouped monitor of "fewer than 1 a minute" must see zero
    // when nothing came.
    if out.is_empty() && by.is_empty() && *measure == Measure::Count {
        out.push(Value { group: Vec::new(), value: 0.0 });
    }
    Ok(out)
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
            out.push(Value { group, value: v });
        }
    }
    Ok(out)
}

/// A monitor's thresholds (Datadog's `options.thresholds`): the critical one the query states,
/// and a warning and recoveries beside it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Thresholds {
    pub critical: f64,
    pub warning: Option<f64>,
    pub critical_recovery: Option<f64>,
    pub warning_recovery: Option<f64>,
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

/// A group's next state, from its last and its value now (none: no data).
pub fn next(prev: State, value: Option<f64>, op: Op, t: &Thresholds, notify_no_data: bool) -> State {
    let Some(v) = value.filter(|v| !v.is_nan()) else { return if notify_no_data { State::NoData } else { prev } };
    let critical = op.passes(v, t.critical);
    let warning = t.warning.is_some_and(|w| op.passes(v, w));
    match prev {
        State::Alert if op.passes(v, t.critical_recovery.unwrap_or(t.critical)) => State::Alert,
        _ if critical => State::Alert,
        State::Warn | State::Alert if t.warning.is_some_and(|w| op.passes(v, t.warning_recovery.unwrap_or(w))) => State::Warn,
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
            _ => return None,
        })
    }
}

/// What a message's variables stand for.
#[derive(Debug, Clone, Default)]
pub struct Vars<'a> {
    pub monitor: &'a str,
    pub value: Option<f64>,
    pub threshold: f64,
    pub warn_threshold: Option<f64>,
    pub group: &'a [(String, String)],
    pub link: &'a str,
    /// Variables besides, by their whole name: an Error Tracking monitor's issue's
    /// (`issue.attributes.error.type`, ...).
    pub extra: &'a [(String, String)],
}

fn num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 { format!("{v:.0}") } else { format!("{v:.4}").trim_end_matches('0').to_owned() }
}

/// A message rendered from Datadog's template syntax: `{{value}}`, `{{threshold}}`,
/// `{{warn_threshold}}`, `{{monitor.name}}`, `{{group}}`, `{{<tag>.name}}` (the group's tag),
/// `{{link}}`, the variables [`Vars::extra`] names, and sections `{{#is_alert}}…{{/is_alert}}` (`^` for not) over the transition's
/// kind. Sections nest; the walk keeps a stack of its own. What it does not know it leaves as it
/// was written.
pub fn render(template: &str, t: Transition, v: &Vars<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    // Whether each open section shows its body.
    let mut shown: Vec<bool> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let visible = shown.iter().all(|&s| s);
        if visible {
            out.push_str(rest.get(..open).unwrap_or(""));
        }
        let after = rest.get(open + 2..).unwrap_or("");
        let Some(close) = after.find("}}") else {
            if visible {
                out.push_str(rest.get(open..).unwrap_or(""));
            }
            return out;
        };
        let tag = after.get(..close).unwrap_or("").trim();
        rest = after.get(close + 2..).unwrap_or("");
        if let Some(section) = tag.strip_prefix('#').or_else(|| tag.strip_prefix('^')) {
            let on = t.is(section.trim()).unwrap_or(false);
            shown.push(if tag.starts_with('^') { !on } else { on });
            continue;
        }
        if tag.starts_with('/') {
            shown.pop();
            continue;
        }
        if !visible {
            continue;
        }
        let group_tag = tag.strip_suffix(".name").and_then(|k| v.group.iter().find(|(gk, _)| gk == k)).map(|(_, gv)| gv.clone());
        let text = match tag {
            "value" => v.value.map_or_else(|| "no data".to_owned(), num),
            "threshold" => num(v.threshold),
            "warn_threshold" => v.warn_threshold.map(num).unwrap_or_default(),
            "monitor.name" | "name" => v.monitor.to_owned(),
            "group" => v.group.iter().map(|(k, gv)| format!("{k}:{gv}")).collect::<Vec<_>>().join(","),
            "link" => v.link.to_owned(),
            _ => match group_tag.or_else(|| v.extra.iter().find(|(k, _)| k == tag).map(|(_, x)| x.clone())) {
                Some(g) => g,
                None => format!("{{{{{tag}}}}}"),
            },
        };
        out.push_str(&text);
    }
    if shown.iter().all(|&s| s) {
        out.push_str(rest);
    }
    out
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
        assert_eq!(q, Query { kind: Kind::Metric { aggr: TimeAggr::Avg, window_ms: 300_000, query: "avg:system.cpu.user{env:prod} by {host}".into() }, op: Op::Gt, threshold: 90.0 });
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
        let t = Thresholds { critical: 90.0, warning: Some(80.0), critical_recovery: Some(85.0), warning_recovery: Some(70.0) };
        let steps = [(50.0, State::Ok), (82.0, State::Warn), (91.0, State::Alert), (88.0, State::Alert), (84.0, State::Warn), (75.0, State::Warn), (69.0, State::Ok), (95.0, State::Alert), (60.0, State::Ok)];
        let mut s = State::Ok;
        for (v, want) in steps {
            s = next(s, Some(v), Op::Gt, &t, false);
            assert_eq!(s, want, "at {v}");
        }
        // Below: an availability that alerts under 99.
        let t = Thresholds { critical: 99.0, ..Thresholds::default() };
        assert_eq!(next(State::Ok, Some(98.5), Op::Lt, &t, false), State::Alert);
        assert_eq!(next(State::Alert, Some(99.5), Op::Lt, &t, false), State::Ok);
        // No data: the state stands, or is NO DATA when asked.
        assert_eq!(next(State::Alert, None, Op::Gt, &t, false), State::Alert);
        assert_eq!(next(State::Alert, None, Op::Gt, &t, true), State::NoData);
        assert_eq!(next(State::NoData, Some(1.0), Op::Gt, &Thresholds { critical: 5.0, ..Thresholds::default() }, true), State::Ok);
    }

    #[test]
    fn messages_render_datadog_s_template_syntax() {
        let group = vec![("host".to_owned(), "web-1".to_owned()), ("env".to_owned(), "prod".to_owned())];
        let v = Vars { monitor: "CPU high", value: Some(93.25), threshold: 90.0, warn_threshold: Some(80.0), group: &group, link: "https://kiln/monitors/1", extra: &[] };
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
        assert_eq!(evaluate(&s, &q, now, 0).unwrap(), vec![Value { group: Vec::new(), value: 0.0 }]);
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
