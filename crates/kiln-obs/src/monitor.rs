//! Monitors (`docs/observe.md` §9): a query, a condition, and what to say. The engine's part,
//! which kiln drives on the group's leader: Datadog's monitor queries parsed and evaluated over a
//! store into a value a group, each group's state moved by its thresholds, and the message
//! rendered from Datadog's template syntax, its `@` mentions naming where it goes.
//!
//! Queries (Datadog's "Monitor API" syntax, so a monitor written for Datadog reads here):
//!
//! ```text
//! metric := time_aggr "(" window ")" ":" query op threshold
//! change := ("change" | "pct_change") "(" time_aggr "(" window ")" "," shift ")" ":" query op threshold
//! logs   := "logs(" '"' search '"' ")" [".index(" '"' * '"' ")"] ".rollup(" '"' measure '"' ["," '"' facet '"'] ")"
//!           [".by(" '"' facets '"' ")"] ".last(" '"' window '"' ")" op threshold
//! time_aggr := avg | sum | min | max | last
//! window := last_<n>(m|h|d|w)  (logs: <n>(m|h|d|w))
//! op     := > | >= | < | <= | == | !=
//! ```
//!
//! States move as Datadog's do: a group alerts once its value passes the critical threshold, and
//! recovers only once it no longer passes the recovery threshold (the critical one when none is
//! set), so a value that hovers at a threshold does not flap; warnings the same below it. A group
//! with no data keeps its state, or goes to NO DATA when the monitor asks to be told.

use crate::logs::search::Search;
use crate::logs::store::{Facet, Measure};
use crate::query::eval::{self, Request};
use crate::store::Store;

/// The longest a monitor's window may be: a week (Datadog's longest, `last_1w`), so that one
/// evaluation reads a bounded range.
pub const MAX_WINDOW_MS: i64 = 7 * 86_400_000;

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
}

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
    let (mut depth, mut quoted) = (0i32, false);
    let mut at = None;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'"' | b'\'' => quoted = !quoted,
            b'(' | b'{' if !quoted => depth += 1,
            b')' | b'}' if !quoted => depth -= 1,
            b'>' | b'<' | b'=' | b'!' if !quoted && depth == 0 => {
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

/// The string literal `"..."` that `s` starts with, and what follows.
fn literal(s: &str) -> Result<(&str, &str), MonitorError> {
    let s = s.trim_start();
    let body = s.strip_prefix('"').ok_or_else(|| syntax("a quoted string"))?;
    let end = body.find('"').ok_or_else(|| syntax("an unterminated string"))?;
    Ok((body.get(..end).unwrap_or(""), body.get(end + 1..).unwrap_or("")))
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
        return parse_logs(q);
    }
    let (left, op, threshold) = comparison(q)?;
    let colon = left.find("):").ok_or_else(|| syntax("time_aggr(window): before the query"))?;
    let (head, query) = (left.get(..=colon).unwrap_or(""), left.get(colon + 2..).unwrap_or("").trim().to_owned());
    if query.is_empty() {
        return Err(syntax("a metric query after the colon"));
    }
    let kind = if let Some(inner) = head.strip_prefix("change(").or_else(|| head.strip_prefix("pct_change(")) {
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
        Kind::Metric { query, .. } | Kind::Change { query, .. } => query,
        Kind::Logs { .. } => "",
    })
    .map_err(|e| syntax(e.to_string()))?;
    Ok(Query { kind, op, threshold })
}

fn parse_logs(q: &str) -> Result<Query, MonitorError> {
    let (left, op, threshold) = comparison(q)?;
    let Some((args, mut rest)) = call(left, "logs")? else { return Err(syntax("logs(\"search\")")) };
    let search = args.first().map(|s| (*s).to_owned()).unwrap_or_default();
    crate::logs::search::parse(&search).map_err(|e| syntax(e.to_string()))?;
    let (mut measure, mut by, mut window_ms) = (Measure::Count, Vec::new(), None);
    while !rest.trim().is_empty() {
        if let Some((_, r)) = call(rest, "index")? {
            rest = r;
        } else if let Some((a, r)) = call(rest, "rollup")? {
            let facet = a.get(1).map(|f| f.trim_start_matches('@').to_owned());
            measure = match (a.first().copied(), facet) {
                (Some("count") | None, _) => Measure::Count,
                (Some("cardinality"), Some(f)) => Measure::Cardinality(Facet::parse(&f)),
                (Some("sum"), Some(f)) => Measure::Sum(f),
                (Some("avg"), Some(f)) => Measure::Avg(f),
                (Some("min"), Some(f)) => Measure::Min(f),
                (Some("max"), Some(f)) => Measure::Max(f),
                (Some(m), _) => return Err(syntax(format!("a log rollup: {m} (count; cardinality, sum, avg, min, max of a facet)"))),
            };
            rest = r;
        } else if let Some((a, r)) = call(rest, "by")? {
            by = a.iter().flat_map(|s| s.split(',')).filter(|s| !s.trim().is_empty()).map(Facet::parse).collect();
            rest = r;
        } else if let Some((a, r)) = call(rest, "last")? {
            window_ms = Some(window(a.first().copied().unwrap_or(""))?);
            rest = r;
        } else {
            return Err(syntax(format!("after logs(...): {}", rest.trim())));
        }
    }
    Ok(Query { kind: Kind::Logs { search, measure, by, window_ms: window_ms.ok_or_else(|| syntax(".last(\"5m\")"))? }, op, threshold })
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

fn metric_values(store: &Store, query: &str, aggr: TimeAggr, window_ms: i64, to: i64) -> Result<Vec<Value>, MonitorError> {
    let (from, to, step) = aligned(window_ms, to);
    let mut req = Request::new(query, from, to);
    req.step = Some(step);
    let a = eval::eval(store, &req).map_err(|e| MonitorError::Eval(e.to_string()))?;
    let mut out = Vec::new();
    if let Some(v) = a.scalar.filter(|v| v.is_finite()) {
        out.push(Value { group: Vec::new(), value: v });
    }
    for s in a.series {
        if let Some(v) = reduce(aggr, &s.values) {
            let mut group = s.tags;
            group.sort();
            out.push(Value { group, value: v });
        }
    }
    Ok(out)
}

/// The query's value a group, as of `now` (ms), the window ending `delay_ms` before it (data a
/// little late still counts).
pub fn evaluate(store: &Store, q: &Query, now: i64, delay_ms: i64) -> Result<Vec<Value>, MonitorError> {
    let to = now - delay_ms.max(0);
    match &q.kind {
        Kind::Metric { aggr, window_ms, query } => metric_values(store, query, *aggr, *window_ms, to),
        Kind::Change { aggr, window_ms, shift_ms, pct, query } => {
            let now_v = metric_values(store, query, *aggr, *window_ms, to)?;
            let then_v = metric_values(store, query, *aggr, *window_ms, to - shift_ms)?;
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
        Kind::Logs { search, measure, by, window_ms } => {
            let parsed: Search = crate::logs::search::parse(search).map_err(|e| MonitorError::Eval(e.to_string()))?;
            let ns = |ms: i64| ms.saturating_mul(1_000_000);
            let agg = store.aggregate_logs(&parsed, ns(to - window_ms), ns(to) - 1, by, measure).map_err(|e| MonitorError::Eval(e.to_string()))?;
            let mut out: Vec<Value> = agg.groups.into_iter().map(|(vals, value)| Value { group: by.iter().map(Facet::name).zip(vals).collect(), value }).collect();
            // A count of nothing is a count: an ungrouped monitor of "fewer than 1 a minute" must
            // see zero when nothing came.
            if out.is_empty() && by.is_empty() && *measure == Measure::Count {
                out.push(Value { group: Vec::new(), value: 0.0 });
            }
            Ok(out)
        }
    }
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
}

fn num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 { format!("{v:.0}") } else { format!("{v:.4}").trim_end_matches('0').to_owned() }
}

/// A message rendered from Datadog's template syntax: `{{value}}`, `{{threshold}}`,
/// `{{warn_threshold}}`, `{{monitor.name}}`, `{{group}}`, `{{<tag>.name}}` (the group's tag),
/// `{{link}}`, and sections `{{#is_alert}}…{{/is_alert}}` (`^` for not) over the transition's
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
            _ => match group_tag {
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
        let q = parse(r#"logs("*").rollup("cardinality", "@user").by("service,@region").last("1h") >= 100"#).unwrap();
        assert_eq!(q.kind, Kind::Logs { search: "*".into(), measure: Measure::Cardinality(Facet::Attr("user".into())), by: vec![Facet::Service, Facet::Attr("region".into())], window_ms: 3_600_000 });
        for bad in ["avg(last_5m):avg:x{*}", "avg(last_5x):avg:x{*} > 1", "median(last_5m):avg:x{*} > 1", "avg(last_9w):avg:x{*} > 1", "avg(last_5m): > 1", r#"logs("x").last("5m")"#, r#"logs("(x").last("5m") > 1"#, "avg(last_5m):avg:x{ > 1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
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
        let v = Vars { monitor: "CPU high", value: Some(93.25), threshold: 90.0, warn_threshold: Some(80.0), group: &group, link: "https://kiln/monitors/1" };
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
        // Every window is whole buckets, at least ten.
        for w in [60_000, 300_000, 420_000, 900_000, 3_600_000, 86_400_000, 7 * 86_400_000] {
            let st = step_for(w);
            assert!(w % st == 0 && w / st >= (w / 10_000).min(10), "{w}: {st}");
        }
    }
}
