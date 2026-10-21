//! Datadog's service checks (`docs/observe.md` §9.4). A check's run is a record
//! (`source:service_check`, [`crate::statsd::check_to_record`]) and a sample of its status besides
//! ([`status_sample`]): [`STATUS_METRIC`] of the series its check, host and tags name, 0 OK, 1
//! WARNING, 2 CRITICAL, 3 UNKNOWN (Datadog's codes), so a check's last runs are read from its own
//! series, a few compressed samples, never from the blocks of every record.
//!
//! Datadog's service check monitor ([`parse`], [`evaluate`]) moves each group by its last runs
//! ([`state`]): so many CRITICAL in a row to ALERT, WARNING (or worse) to WARN, OK back to OK, as
//! its thresholds say:
//!
//! ```text
//! check := '"' name '"' ".over(" ('"*"' | '"' tag '"' ("," '"' tag '"')*) ")"
//!          [".by(" '"' tag '"' ("," '"' tag '"')* ")"] ".last(" n ")" ".count_by_status()"
//! ```

use super::{MonitorError, State, literal, syntax, unescape};
use crate::batch::{Kind, SeriesKey};
use crate::index::{Clause, NameMatch, Selector, ValueMatch};
use crate::logs::record::Record;
use crate::store::{LogsAt, Scratch, Store};

/// The metric a check's runs are samples of: its status.
pub const STATUS_METRIC: &str = "kiln.service_check.status";
/// The source of a check's runs as records.
pub const SOURCE: &str = "service_check";

pub const OK: u8 = 0;
pub const WARNING: u8 = 1;
pub const CRITICAL: u8 = 2;
pub const UNKNOWN: u8 = 3;

/// The most runs in a row a threshold may ask for, and so the most a group's evaluation reads.
pub const MAX_RUNS: u32 = 100;

/// How far back a group's runs are looked for first: the Agent runs its checks every 15 s, and a
/// check of its own every few minutes at most, so a quarter of an hour holds the last runs of
/// nearly every group; a group with fewer is read back as far as the monitor looks.
pub const RECENT_MS: i64 = 15 * 60_000;

/// A status's code from its name as a check's record keeps it (`check_status`).
pub fn code(status: &str) -> u8 {
    match status {
        "ok" => OK,
        "warning" => WARNING,
        "critical" => CRITICAL,
        _ => UNKNOWN,
    }
}

/// A status's name, as Datadog writes it.
pub fn name(code: u8) -> &'static str {
    match code {
        OK => "OK",
        WARNING => "WARNING",
        CRITICAL => "CRITICAL",
        _ => "UNKNOWN",
    }
}

/// A check's record as a sample of its status: its series ([`STATUS_METRIC`] of `check`, `host`
/// and the check's tags) and its time (ms) and code. None for another record, or a check's whose
/// tags make no series (too many, or a name no series may have): its record is kept all the same.
pub fn status_sample(r: &Record) -> Option<(SeriesKey, i64, f64)> {
    if r.source != SOURCE {
        return None;
    }
    if !r.attrs.iter().any(|(k, _)| k == "check") {
        return None;
    }
    let status = r.attrs.iter().find(|(k, _)| k == "check_status").map_or(UNKNOWN, |(_, v)| code(v));
    let host = (!r.host.is_empty()).then_some(("host", r.host.as_str()));
    // The check's tags are its record's attributes, its name and service among them (never its
    // host, which is its own field).
    let tags = r.attrs.iter().filter(|(k, _)| k != "check_status").map(|(k, v)| (k.as_str(), v.as_str())).chain(host);
    let key = SeriesKey::new(STATUS_METRIC, tags, Kind::Gauge).ok()?;
    Some((key, r.ts.div_euclid(1_000_000), f64::from(status)))
}

/// Datadog's service check monitor's query.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckQuery {
    pub check: String,
    /// The runs it reads: those of the check's series whose tags hold these (none: `"*"`, all).
    pub over: Vec<Clause>,
    /// What a group is: its runs' values of these tags (none: one group of every run).
    pub by: Vec<String>,
    /// The latest runs of a group it reads: at least its thresholds' largest.
    pub last: u32,
}

/// A service check monitor's thresholds (Datadog's `options.thresholds` for one): runs in a row,
/// the latest among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckThresholds {
    pub critical: u32,
    pub warning: Option<u32>,
    pub ok: u32,
    /// Datadog's "notify for Unknown status": so many UNKNOWN in a row to NO DATA (none: never).
    pub unknown: Option<u32>,
}

impl Default for CheckThresholds {
    /// Datadog's: an alert on the first failure, a recovery on the first success.
    fn default() -> Self {
        Self { critical: 1, warning: None, ok: 1, unknown: None }
    }
}

impl CheckThresholds {
    /// The most runs any of them asks for.
    pub fn most(&self) -> u32 {
        self.critical.max(self.ok).max(self.warning.unwrap_or(0)).max(self.unknown.unwrap_or(0))
    }
}

/// Whether a monitor's query is a service check's: it starts with the check's name, quoted.
pub fn is_check(q: &str) -> bool {
    q.trim_start().starts_with('"')
}

/// `name(` `"a"`, `"b"` `)` or, for `.last(n)`, a number: its arguments, and what follows.
fn method(s: &str) -> Result<(&str, Vec<&str>, &str), MonitorError> {
    let s = s.trim_start().strip_prefix('.').ok_or_else(|| syntax(format!("a .method(...) after the check, found {}", s.trim())))?;
    let open = s.find('(').ok_or_else(|| syntax(format!("a .method(...): {s}")))?;
    let (name, mut rest) = (s.get(..open).unwrap_or("").trim(), s.get(open + 1..).unwrap_or(""));
    let mut args = Vec::new();
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix(')') {
            return Ok((name, args, r));
        }
        if rest.starts_with('"') {
            let (a, r) = literal(rest)?;
            args.push(a);
            rest = r;
        } else {
            let end = rest.find([',', ')']).ok_or_else(|| syntax(format!(".{name}(...)'s closing parenthesis")))?;
            args.push(rest.get(..end).unwrap_or("").trim());
            rest = rest.get(end..).unwrap_or("");
        }
        rest = rest.trim_start().strip_prefix(',').unwrap_or(rest.trim_start());
    }
}

/// A tag of `.over(...)` (or a check status widget's) as a condition: `key:value` (a value with
/// `*` a glob), or a bare tag.
pub fn tag_clause(tag: &str) -> Clause {
    let (key, value) = tag.split_once(':').unwrap_or((tag, ""));
    let value = if value.contains(['*', '?']) { ValueMatch::Glob(value.to_owned()) } else { ValueMatch::Exact(value.to_owned()) };
    Clause { key: key.to_owned(), values: vec![value], negate: false }
}

/// Parses a service check monitor's query: `"check".over(...)[.by(...)].last(n).count_by_status()`,
/// its methods in any order before `count_by_status()`.
pub fn parse(q: &str) -> Result<CheckQuery, MonitorError> {
    let (check, mut rest) = literal(q.trim())?;
    let check = unescape(check);
    if check.is_empty() {
        return Err(syntax("a service check's name, quoted"));
    }
    let (mut over, mut by, mut last, mut counted) = (None, None, None, false);
    while !rest.trim().is_empty() {
        if counted {
            return Err(syntax(format!("after .count_by_status(): {}", rest.trim())));
        }
        let (name, args, r) = method(rest)?;
        match name {
            "over" if over.is_none() => {
                let tags: Vec<String> = args.iter().flat_map(|a| a.split(',')).map(|t| unescape(t.trim())).filter(|t| !t.is_empty()).collect();
                if tags.is_empty() {
                    return Err(syntax(".over(\"*\") or .over(\"tag\", ...): it cannot be empty"));
                }
                over = Some(if tags.iter().any(|t| t == "*") { Vec::new() } else { tags.iter().map(|t| tag_clause(t)).collect() });
            }
            "by" if by.is_none() => {
                let mut tags: Vec<String> = Vec::new();
                for t in args.iter().flat_map(|a| a.split(',')).map(|t| unescape(t.trim())).filter(|t| !t.is_empty()) {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
                by = Some(tags);
            }
            "last" if last.is_none() => {
                let n = match args.as_slice() {
                    [n] => n.trim().parse::<u32>().ok(),
                    _ => None,
                };
                last = Some(n.filter(|n| (1..=MAX_RUNS).contains(n)).ok_or_else(|| syntax(format!(".last(n): the runs it reads, 1 to {MAX_RUNS}")))?);
            }
            "count_by_status" if args.is_empty() => counted = true,
            other => return Err(syntax(format!("a service check monitor's .{other}(...): .over, .by and .last once each, then .count_by_status()"))),
        }
        rest = r;
    }
    if !counted {
        return Err(syntax(".count_by_status() at the end"));
    }
    let over = over.ok_or_else(|| syntax(".over(\"*\"), or the tags of the runs it reads"))?;
    let last = last.ok_or_else(|| syntax(".last(n): the runs of a group it reads"))?;
    Ok(CheckQuery { check, over, by: by.unwrap_or_default(), last })
}

/// Where a group's last runs (oldest first) move it, as Datadog's service check monitor moves a
/// group: its latest `critical` CRITICAL, to ALERT; else its latest `warning` WARNING or CRITICAL,
/// to WARN; else its latest `unknown` UNKNOWN, to NO DATA (Datadog shows UNKNOWN as it shows no
/// data); else its latest `ok` OK, to OK; else nowhere, and it keeps its state.
pub fn state(runs: &[u8], t: &CheckThresholds) -> Option<State> {
    let latest = |n: u32, holds: fn(u8) -> bool| n > 0 && runs.len() >= n as usize && runs.iter().rev().take(n as usize).all(|&s| holds(s));
    if latest(t.critical, |s| s == CRITICAL) {
        Some(State::Alert)
    } else if t.warning.is_some_and(|w| latest(w, |s| s == WARNING || s == CRITICAL)) {
        Some(State::Warn)
    } else if t.unknown.is_some_and(|u| latest(u, |s| s == UNKNOWN)) {
        Some(State::NoData)
    } else if latest(t.ok, |s| s == OK) {
        Some(State::Ok)
    } else {
        None
    }
}

/// A group as an evaluation found it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckGroup {
    /// Its tags, sorted.
    pub group: Vec<(String, String)>,
    /// Its latest run's status.
    pub status: u8,
    /// Where its last runs move it (none: nowhere).
    pub state: Option<State>,
}

/// The check's series whose tags hold `over`, each with its group, side by side by group: its
/// values of `by` (a series without one `N/A`, as a metric's are), or with `by` none, its own tags
/// but the check's name (each series a group of its own).
fn grouped(store: &Store, check: &str, over: &[Clause], by: Option<&[String]>) -> Vec<(Vec<(String, String)>, u32)> {
    let mut clauses = Vec::with_capacity(over.len() + 1);
    clauses.push(Clause { key: "check".into(), values: vec![ValueMatch::Exact(check.to_owned())], negate: false });
    clauses.extend(over.iter().cloned());
    let sel = Selector { metric: NameMatch::Exact(STATUS_METRIC.into()), clauses, expr: None };
    let mut ids = Vec::new();
    store.select(&sel, &mut ids);
    let mut series: Vec<(Vec<(String, String)>, u32)> = ids
        .iter()
        .filter_map(|&id| {
            let s = store.series(id)?;
            let mut group: Vec<(String, String)> = match by {
                Some(by) => by.iter().map(|k| (k.clone(), s.tag(k).unwrap_or("N/A").to_owned())).collect(),
                None => s.tags().filter(|(k, _)| *k != "check").map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            };
            group.sort();
            Some((group, id))
        })
        .collect();
    series.sort_unstable();
    series
}

/// Series `ids`' runs in `[from, to]` (ms), appended to `runs`.
fn read_runs(store: &Store, ids: impl Iterator<Item = u32>, from: i64, to: i64, b: &mut Buffers, runs: &mut Vec<(i64, u8)>) {
    for id in ids {
        b.ts.clear();
        b.vs.clear();
        if store.read_with(id, from, to, &mut b.scratch, &mut b.ts, &mut b.vs).is_ok() {
            runs.extend(b.ts.iter().zip(&b.vs).map(|(&t, &v)| (t, v as u8)));
        }
    }
}

/// What reading series keeps across them.
#[derive(Default)]
struct Buffers {
    scratch: Scratch,
    ts: Vec<i64>,
    vs: Vec<f64>,
}

/// Each group's last runs up to `to` (ms), as far back as `lookback_ms`, and where they move it:
/// the groups with a run in that time, sorted. A group's series are read over the last
/// [`RECENT_MS`] first, and further back only when that holds fewer runs than the query reads.
pub fn evaluate(store: &Store, q: &CheckQuery, t: &CheckThresholds, to: i64, lookback_ms: i64) -> Vec<CheckGroup> {
    let series = grouped(store, &q.check, &q.over, Some(&q.by));
    let want = q.last.max(t.most()) as usize;
    let recent = RECENT_MS.min(lookback_ms);
    let mut b = Buffers::default();
    let (mut runs, mut last) = (Vec::<(i64, u8)>::new(), Vec::<u8>::new());
    let mut out = Vec::new();
    for members in series.chunk_by(|a, b| a.0 == b.0) {
        let Some((group, _)) = members.first() else { continue };
        let ids = || members.iter().map(|m| m.1);
        runs.clear();
        read_runs(store, ids(), to - recent, to, &mut b, &mut runs);
        if runs.len() < want && lookback_ms > recent {
            runs.clear();
            read_runs(store, ids(), to - lookback_ms, to, &mut b, &mut runs);
        }
        if runs.is_empty() {
            continue;
        }
        // In time order; runs of a group's series at one time, the worse the later.
        runs.sort_unstable();
        last.clear();
        last.extend(runs.get(runs.len().saturating_sub(want)..).unwrap_or_default().iter().map(|r| r.1));
        let status = last.last().copied().unwrap_or(UNKNOWN);
        out.push(CheckGroup { group: group.clone(), status, state: state(&last, t) });
    }
    out
}

/// How bad a status is to show: CRITICAL, then WARNING, then UNKNOWN, then OK.
pub fn rank(code: u8) -> u8 {
    match code {
        CRITICAL => 3,
        WARNING => 2,
        OK => 0,
        _ => 1,
    }
}

/// A group's status as Datadog's check status widget shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Latest {
    /// Its tags, sorted.
    pub group: Vec<(String, String)>,
    /// The worst of its series' latest runs ([`rank`]).
    pub status: u8,
    /// Its latest run's time, ms.
    pub at: i64,
}

/// Each group's status up to `to` (ms), as far back as `lookback_ms` (Datadog's check status
/// widget's): of each of its series the latest run, the worst of them ([`rank`]). Groups by `by`,
/// or with `by` none each series a group of its own; those with a run in that time, sorted.
pub fn latest(store: &Store, check: &str, over: &[Clause], by: Option<&[String]>, to: i64, lookback_ms: i64) -> Vec<Latest> {
    let series = grouped(store, check, over, by);
    let recent = RECENT_MS.min(lookback_ms);
    let mut b = Buffers::default();
    let mut runs = Vec::<(i64, u8)>::new();
    let mut out: Vec<Latest> = Vec::new();
    for (group, id) in &series {
        runs.clear();
        read_runs(store, std::iter::once(*id), to - recent, to, &mut b, &mut runs);
        if runs.is_empty() && lookback_ms > recent {
            read_runs(store, std::iter::once(*id), to - lookback_ms, to, &mut b, &mut runs);
        }
        let Some(&(at, status)) = runs.iter().max_by_key(|r| r.0) else { continue };
        match out.last_mut() {
            Some(l) if l.group == *group => {
                if rank(status) > rank(l.status) {
                    l.status = status;
                }
                l.at = l.at.max(at);
            }
            _ => out.push(Latest { group: group.clone(), status, at }),
        }
    }
    out
}

/// A group's latest run as its record, for what a notification says of it (Datadog's
/// `{{check_message}}`): the newest record of the check whose fields and attributes hold the
/// group's tags, up to `to` (ms) and as far back as `lookback_ms`, of the logs as they were taken
/// (`Store::logs_at`: read with no hold of the store).
pub fn latest_run(logs: &LogsAt, check: &str, group: &[(String, String)], to: i64, lookback_ms: i64) -> Option<Record> {
    let quoted = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
    let mut q = format!("source:{SOURCE} @check:{}", quoted(check));
    for (k, v) in group.iter().filter(|(_, v)| v != "N/A") {
        let field = if matches!(k.as_str(), "host" | "service") { k.clone() } else { format!("@{k}") };
        q.push_str(&format!(" {field}:{}", quoted(v)));
    }
    let search = crate::logs::search::parse(&q).ok()?;
    let ns = |ms: i64| ms.saturating_mul(1_000_000);
    logs.search(&search, ns(to - lookback_ms), ns(to), 1).ok()?.records.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::Builder;
    use crate::hash::Keyed;
    use crate::statsd::{Check, check_to_record};

    #[test]
    fn datadog_s_queries_parse_and_mistakes_say_why() {
        let q = parse(r#""datadog.agent.up".over("*").by("host").last(2).count_by_status()"#).unwrap();
        assert_eq!((q.check.as_str(), q.over.is_empty(), q.by.as_slice(), q.last), ("datadog.agent.up", true, &["host".to_owned()][..], 2));
        let q = parse(r#""http.can_connect".over("instance:main_site","url:http://localhost/").by("host","instance","url").last(4).count_by_status()"#).unwrap();
        assert_eq!(q.over.len(), 2);
        assert_eq!(q.over[1], Clause { key: "url".into(), values: vec![ValueMatch::Exact("http://localhost/".into())], negate: false });
        assert_eq!(q.by, ["host", "instance", "url"]);
        // Its methods in any order, `.by` left out (one group), a glob among the tags.
        let q = parse(r#" "ntp.in_sync".last(3).over("env:prod", "role:db-*").count_by_status() "#).unwrap();
        assert_eq!((q.by.len(), q.last), (0, 3));
        assert!(matches!(&q.over[1].values[0], ValueMatch::Glob(g) if g == "db-*"));
        for bad in [
            r#""x".over("*").last(2)"#,
            r#""x".over().last(2).count_by_status()"#,
            r#""x".by("host").last(2).count_by_status()"#,
            r#""x".over("*").count_by_status()"#,
            r#""x".over("*").last(0).count_by_status()"#,
            r#""x".over("*").last(101).count_by_status()"#,
            r#""x".over("*").last(2).last(3).count_by_status()"#,
            r#""x".over("*").last(2).count_by_status().by("host")"#,
            r#""x".over("*").last(2).rollup("count").count_by_status()"#,
            r#""".over("*").last(1).count_by_status()"#,
            r#""x.over("*")"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(is_check(r#" "x".over("*")"#) && !is_check("avg(last_5m):x{*} > 1"));
    }

    /// Runs in a row move a group, the latest last; what they do not settle leaves it be.
    #[test]
    fn a_group_moves_by_its_runs_in_a_row() {
        let (o, w, c, u) = (OK, WARNING, CRITICAL, UNKNOWN);
        let t = CheckThresholds { critical: 3, warning: Some(2), ok: 2, unknown: Some(1) };
        for (runs, want) in [
            (&[c, c, c][..], Some(State::Alert)),
            (&[o, c, c][..], Some(State::Warn)),
            (&[c, w, c][..], Some(State::Warn)),
            (&[c, c, w][..], Some(State::Warn)),
            (&[w, w, o][..], None),
            (&[c, o, o][..], Some(State::Ok)),
            (&[o, o, u][..], Some(State::NoData)),
            (&[c, c][..], Some(State::Warn)),
            (&[o][..], None),
            (&[][..], None),
        ] {
            assert_eq!(state(runs, &t), want, "{runs:?}");
        }
        // Datadog's defaults: the first failure alerts, the first success recovers, a warning or
        // an unknown says nothing.
        let d = CheckThresholds::default();
        assert_eq!((state(&[o, c], &d), state(&[c, o], &d), state(&[o, w], &d), state(&[c, u], &d)), (Some(State::Alert), Some(State::Ok), None, None));
    }

    fn run(name: &str, status: u8, ts_s: i64, host: &str, tags: &str) -> Record {
        check_to_record(&Check { name, status, ts: Some(ts_s), host: Some(host), tags, message: Some(format!("{name} on {host} at {ts_s}").into()) }, "", &[], 0)
    }

    /// A store holding `runs` as their records and their status samples, as kiln writes them.
    fn store_of(runs: &[Record]) -> Store {
        let mut s = Store::new(Keyed::with_keys(3, 4));
        let mut b = Builder::new();
        b.logs(runs);
        for r in runs {
            let (key, ms, v) = status_sample(r).unwrap();
            b.define(&key);
            b.run(key.fp, [(ms, v)]);
        }
        s.apply(&b.finish()).unwrap();
        s
    }

    /// Hosts' checks every 15 s: each group found by its series, its last runs read in order across
    /// its series, a sparse group's from further back, a run's record found for its message.
    #[test]
    fn groups_are_found_by_their_series_and_read_in_order() {
        let mut runs = Vec::new();
        for i in 0..40i64 {
            let t = 1_700_000_000 + i * 15;
            // web-1 fails from its 37th run; web-2 never; db-1's two instances, one of them
            // failing in its last two runs.
            runs.push(run("http.can_connect", if i >= 37 { CRITICAL } else { OK }, t, "web-1", "env:prod,service:shop"));
            runs.push(run("http.can_connect", OK, t, "web-2", "env:prod,service:shop"));
            runs.push(run("http.can_connect", if i >= 38 { CRITICAL } else { OK }, t, "db-1", "env:prod,instance:a"));
            runs.push(run("http.can_connect", OK, t + 1, "db-1", "env:prod,instance:b"));
            runs.push(run("http.can_connect", OK, t, "dev-1", "env:dev"));
        }
        // A check that ran twice, an hour before the rest's end.
        runs.push(run("backup.done", OK, 1_700_000_000 + 600 - 3_600, "db-1", "env:prod"));
        runs.push(run("backup.done", CRITICAL, 1_700_000_000 + 600 - 3_590, "db-1", "env:prod"));
        let s = store_of(&runs);
        let to = (1_700_000_000 + 40 * 15) * 1000;
        let q = parse(r#""http.can_connect".over("env:prod").by("host").last(3).count_by_status()"#).unwrap();
        let t = CheckThresholds { critical: 3, ..CheckThresholds::default() };
        let found = evaluate(&s, &q, &t, to, 86_400_000);
        let summary: Vec<(String, u8, Option<State>)> = found.iter().map(|g| (g.group[0].1.clone(), g.status, g.state)).collect();
        // web-1's last three runs CRITICAL; db-1's, across both its instances (an OK, a CRITICAL,
        // an OK a second after it), not three CRITICAL in a row, and OK by its latest.
        assert_eq!(summary, [("db-1".to_owned(), OK, Some(State::Ok)), ("web-1".to_owned(), CRITICAL, Some(State::Alert)), ("web-2".to_owned(), OK, Some(State::Ok))]);
        // Grouped by instance too: db-1's instances apart, a group of its own each.
        let q = parse(r#""http.can_connect".over("host:db-1").by("host","instance").last(2).count_by_status()"#).unwrap();
        let found = evaluate(&s, &q, &CheckThresholds { critical: 2, ..CheckThresholds::default() }, to, 86_400_000);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].group, [("host".to_owned(), "db-1".to_owned()), ("instance".to_owned(), "a".to_owned())]);
        assert_eq!((found[0].state, found[1].state), (Some(State::Alert), Some(State::Ok)));
        // One group of everything; then a sparse check read from an hour back, and not at all
        // when the monitor looks back less.
        let q = parse(r#""http.can_connect".over("*").last(1).count_by_status()"#).unwrap();
        assert_eq!(evaluate(&s, &q, &CheckThresholds::default(), to, 86_400_000).len(), 1);
        let q = parse(r#""backup.done".over("*").by("host").last(2).count_by_status()"#).unwrap();
        let t2 = CheckThresholds { critical: 2, ..CheckThresholds::default() };
        let sparse = evaluate(&s, &q, &t2, to, 86_400_000);
        assert_eq!((sparse.len(), sparse[0].status, sparse[0].state), (1, CRITICAL, None));
        assert!(evaluate(&s, &q, &t2, to, 1_800_000).is_empty(), "nothing within half an hour");
        // The widget's: each host's worst latest run (db-1's instance a's CRITICAL), and each series
        // a group of its own.
        let over = [Clause { key: "env".into(), values: vec![ValueMatch::Exact("prod".into())], negate: false }];
        let by_host: Vec<(String, u8)> = latest(&s, "http.can_connect", &over, Some(&["host".to_owned()]), to, 86_400_000).into_iter().map(|l| (l.group[0].1.clone(), l.status)).collect();
        assert_eq!(by_host, [("db-1".to_owned(), CRITICAL), ("web-1".to_owned(), CRITICAL), ("web-2".to_owned(), OK)]);
        let each = latest(&s, "http.can_connect", &over, None, to, 86_400_000);
        assert_eq!(each.len(), 4);
        assert!(each.iter().all(|l| l.group.iter().any(|(k, _)| k == "host") && !l.group.iter().any(|(k, _)| k == "check")));
        assert_eq!(each.iter().map(|l| l.at).max(), Some((1_700_000_000 + 39 * 15 + 1) * 1000));
        assert!(latest(&s, "backup.done", &[], None, to, 1_800_000).is_empty());
        // The latest run's record, for its message.
        let r = latest_run(&s.logs_at(), "http.can_connect", &[("host".into(), "web-1".into())], to, 3_600_000).unwrap();
        assert_eq!(r.message, format!("http.can_connect on web-1 at {}", 1_700_000_000 + 39 * 15));
        assert!(latest_run(&s.logs_at(), "http.can_connect", &[("host".into(), "nowhere".into())], to, 3_600_000).is_none());
    }

    /// A check's sample is of its check, host and tags; a record of another source has none, and
    /// two runs of a check on a host with the same tags are one series.
    #[test]
    fn a_run_s_status_is_a_sample_of_its_check_s_series() {
        let r = run("db.up", CRITICAL, 1_700_000_000, "db-1", "env:prod,service:db,role");
        let (key, ms, v) = status_sample(&r).unwrap();
        assert_eq!((ms, v), (1_700_000_000_000, 2.0));
        let again = status_sample(&run("db.up", OK, 1_700_000_015, "db-1", "env:prod,service:db,role")).unwrap();
        assert_eq!(again.0, key);
        assert_ne!(status_sample(&run("db.up", OK, 1_700_000_015, "db-2", "env:prod,service:db,role")).unwrap().0, key);
        let mut s = Store::new(Keyed::with_keys(3, 4));
        let mut b = Builder::new();
        b.define(&key);
        b.run(key.fp, [(ms, v)]);
        s.apply(&b.finish()).unwrap();
        let mut ids = Vec::new();
        s.select(&Selector::metric(STATUS_METRIC), &mut ids);
        let tags: Vec<(String, String)> = s.series(ids[0]).unwrap().tags().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        assert_eq!(tags, [("check", "db.up"), ("env", "prod"), ("host", "db-1"), ("role", ""), ("service", "db")].map(|(k, v)| (k.to_owned(), v.to_owned())));
        assert!(status_sample(&Record { source: "app".into(), ..r }).is_none());
    }
}
