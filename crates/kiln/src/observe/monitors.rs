//! Monitors (`docs/observe.md` §9): what the server keeps of them, evaluates, and says.
//!
//! A monitor is Datadog's: a query (`kiln_obs::monitor`'s syntax), a message (its template, its
//! `@` mentions naming integrations), and options: thresholds (warning, recoveries), NO DATA after
//! a while without data, renotification, an evaluation delay for late data, and how often it is
//! evaluated (every minute, Datadog's, unless it says; ten seconds at the most often). The server,
//! the group's leader, evaluates each on the blocking pool under the store's read lock; each
//! group's change of state is an event (`source:kiln.event @event.type:monitor.<state>`) and, unless
//! the monitor is muted, its message to every integration it mentions; a group that stays in
//! trouble is told again each renotification interval.
//!
//! A composite (`type: "composite"`, Datadog's) reads other monitors' states, its query an
//! expression of their ids (`kiln_obs::monitor::composite`): evaluated right after any monitor it
//! reads, and at its own interval besides. A monitor a composite reads is not deleted while it does.
//!
//! Definitions are `<observe>/monitors.json`, states `<observe>/monitor-states.json` (saved as they
//! change, so a restart neither forgets an alert nor says it again).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kiln_obs::machine::Shared;
use kiln_obs::monitor::checks::{self, CheckQuery, CheckThresholds};
use kiln_obs::monitor::composite::{self, Composite, Group};
use kiln_obs::monitor::{self, State, Thresholds, Transition, Vars};
use serde::{Deserialize, Serialize};

use super::notify::{Integration, Integrations, Notice, Notifier};

/// How often a monitor is evaluated unless it says: Datadog's metric monitors' minute.
const EVERY_S: u32 = 60;
/// The most often it may be: ten seconds, kiln's finest resolution for most series.
const EVERY_MIN_S: u32 = 10;
/// Groups a monitor keeps states for at the most (an answer's series' bound), and how long a group
/// with no data at all is kept.
const GROUPS: usize = 10_000;
const FORGET_MS: i64 = 86_400_000;
/// How long a service check's group may go without a run before it is not found (and NO DATA, when
/// asked), unless its monitor says: Datadog's default for a service check, a day.
const CHECK_NO_DATA_MS: i64 = 86_400_000;
/// The longest evaluation delay: Datadog's ("The maximum configurable evaluation delay is 86400
/// seconds (24 hours)", its monitor configuration page).
const MAX_DELAY_S: u32 = 86_400;
/// The longest `timeout_h`: Datadog's ("The maximum allowed value is 24 hours", its API).
const MAX_TIMEOUT_H: u32 = 24;
/// `group_retention_duration`'s bounds, ms: Datadog's ("Minimum: one hour. Maximum: 72 hours").
const RETENTION_MS: std::ops::RangeInclusive<i64> = 3_600_000..=72 * 3_600_000;
/// The log samples a notification shows at the most: Datadog's ("Up to 10 log samples", its log
/// monitor page).
const SAMPLES: usize = 10;
/// How long a group is unseen before it is new again (`new_group_delay`): Datadog's, "every new
/// group (which has not been seen in the last 24 hours)", its monitor configuration page.
const NEW_AFTER_MS: i64 = 86_400_000;
/// The explorers' ranges (`ui/src/views/observe/parts.tsx`, `RANGES`), ms: a record link's range
/// is the first that covers its monitor's window.
const EXPLORER_RANGES: [(&str, i64); 6] = [("15m", 900_000), ("1h", 3_600_000), ("4h", 14_400_000), ("1d", 86_400_000), ("7d", 604_800_000), ("30d", 2_592_000_000)];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The store, held to read: by one monitor's evaluation at a time, or to take its records as they
/// are, so a write waits for no more than one (and no reader behind that write).
fn held(store: &Shared) -> kiln_obs::lock::ReadGuard<'_, kiln_obs::store::Store> {
    store.read().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThresholdsIn {
    pub critical: Option<f64>,
    pub warning: Option<f64>,
    pub critical_recovery: Option<f64>,
    pub warning_recovery: Option<f64>,
    /// A service check monitor's: OK runs in a row to recover.
    pub ok: Option<f64>,
    /// A service check monitor's: UNKNOWN runs in a row to NO DATA (none: never).
    pub unknown: Option<f64>,
    /// Datadog's dynamic thresholds, a formula monitor's (`> threshold`): formulas of its variables
    /// (`formula("query1 / query2").rollup("p99").last("1w").multiplier(1.5)`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub critical_recovery_query: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub thresholds: ThresholdsIn,
    pub notify_no_data: bool,
    /// Minutes without data before NO DATA (two windows unless it says).
    pub no_data_timeframe: Option<u32>,
    /// Minutes between notifications while a group stays in trouble (none: once).
    pub renotify_interval: Option<u32>,
    /// Seconds the window ends before now: what late data is given.
    pub evaluation_delay: Option<u32>,
    /// Seconds between evaluations.
    pub every: Option<u32>,
    /// Datadog's: where a cumulative window (`current_1h`, `current_1d`, `current_1mo`) begins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduling_options: Option<SchedulingOptions>,
    /// Datadog's, an anomaly monitor's: the window its trigger reads (its query's `alert_window`)
    /// and the one its recovery reads (`kiln_obs::monitor::Query::windows`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_windows: Option<ThresholdWindows>,
    /// Datadog's, a formula monitor's queries by their names ([`Variable`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<Variable>,
    /// Datadog's: the states told again each renotify interval (unsaid, with an interval, ALERT and
    /// NO DATA: its "Defaults to `alert` and `no data` when `renotify_interval` is set").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renotify_statuses: Option<Vec<Renotify>>,
    /// Datadog's: how many times a state is told again at the most (unsaid: no end).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renotify_occurrences: Option<u32>,
    /// Datadog's: a template said after the message when a state is told again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation_message: Option<String>,
    /// Datadog's: hours without data after which a group in trouble resolves, 0 to 24.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_h: Option<u32>,
    /// Datadog's, a metric monitor's: a group evaluated only once its window is full
    /// (`kiln_obs::monitor::full_windows`). Other monitors take it and read no window so.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub require_full_window: bool,
    /// Datadog's: seconds a group the monitor has not had is not evaluated (unsaid: none, its API's
    /// default; its form's is 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_group_delay: Option<u32>,
    /// Datadog's: whether a notification's title names its group (unsaid: it does).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_tags: Option<bool>,
    /// Datadog's, a monitor of logs, spans, events or Error Tracking's: what a group the evaluation
    /// finds no data of comes to ([`OnMissingData`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_missing_data: Option<OnMissingData>,
    /// Datadog's: the message's mentions told when the monitor is made, changed or deleted.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub notify_audit: bool,
    /// Datadog's: what notifications are by, some of the query's group tags (`*`: one for all):
    /// such a group's state the worst of its groups', told as it changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_by: Option<Vec<String>>,
    /// Datadog's, a monitor of logs, spans, events or Error Tracking's: how long a group without
    /// data is kept (`60m`, `1h`, `2d`; an hour to 72), a day unless said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_retention_duration: Option<String>,
    /// Datadog's: what a notification shows besides its message ([`Preset`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_preset_name: Option<Preset>,
    /// Datadog's, a log monitor's: its notifications of trouble show its group's latest logs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub enable_logs_sample: bool,
}

/// Why `on_missing_data` or `group_retention_duration` is refused of a monitor of other data.
const RECORDS_ONLY: &str = "on_missing_data and group_retention_duration: a monitor of logs, spans, events or Error Tracking's (Datadog's: \"only available for APM Trace Analytics, Audit Trail, CI, Error Tracking, Event, Logs, and RUM monitors\"); a metric monitor's missing data is notify_no_data's";

impl Options {
    /// Datadog's bounds on what its options say, whatever the monitor.
    fn check(&self) -> Result<(), String> {
        if let Some(d) = self.evaluation_delay.filter(|&d| d > MAX_DELAY_S) {
            return Err(format!("evaluation_delay {d}: {MAX_DELAY_S} seconds at the most (Datadog's)"));
        }
        if let Some(h) = self.timeout_h.filter(|&h| h > MAX_TIMEOUT_H) {
            return Err(format!("timeout_h {h}: 0 to {MAX_TIMEOUT_H} hours (Datadog's)"));
        }
        if self.renotify_occurrences == Some(0) {
            return Err("renotify_occurrences: 1 or more (none: no end)".into());
        }
        if let Some(r) = &self.group_retention_duration
            && !duration_ms(r).is_some_and(|ms| RETENTION_MS.contains(&ms))
        {
            return Err(format!("group_retention_duration {r}: an hour to 72 (60m, 1h, 2d), Datadog's"));
        }
        if self.on_missing_data.is_some() && self.notify_no_data {
            return Err("on_missing_data and notify_no_data: one or the other (on_missing_data says whether a group without data shows NO DATA, and whether it is told)".into());
        }
        if let Some(by) = &self.notify_by
            && (by.is_empty() || by.iter().any(|t| t.trim().is_empty()) || (by.len() > 1 && by.iter().any(|t| t == "*")))
        {
            return Err("notify_by: some of the query's group tags, or [\"*\"] alone".into());
        }
        Ok(())
    }
}

/// A state Datadog's `renotify_statuses` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Renotify {
    #[serde(rename = "alert")]
    Alert,
    #[serde(rename = "warn")]
    Warn,
    #[serde(rename = "no data")]
    NoData,
}

/// Datadog's `on_missing_data`: what a group comes to when an evaluation finds no data of it, at
/// once ("the missing data window is not configurable").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnMissingData {
    /// A count's 0, compared with the thresholds; another measure's last state.
    Default,
    /// NO DATA, not told.
    ShowNoData,
    ShowAndNotifyNoData,
    /// OK.
    Resolve,
}

/// Datadog's `notification_preset_name`. kiln's notifications show neither the query nor the
/// mentions (`notify::Notice::body`) and draw no snapshot: what is left to hide is the footer's
/// link back to kiln, which `hide_all`, `hide_handles_and_footer` and `show_only_snapshot` drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    ShowAll,
    HideQuery,
    HideHandles,
    HideAll,
    HideQueryAndHandles,
    ShowOnlySnapshot,
    HideHandlesAndFooter,
}

impl Preset {
    fn footer(self) -> bool {
        !matches!(self, Self::HideAll | Self::HideHandlesAndFooter | Self::ShowOnlySnapshot)
    }
}

/// `60m`, `1h`, `2d` as ms (Datadog's `group_retention_duration`).
fn duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (n, unit) = s.split_at_checked(s.find(|c: char| !c.is_ascii_digit())?)?;
    let per = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.parse::<i64>().ok()?.checked_mul(per)
}

/// Datadog's `threshold_windows` of an anomaly monitor, timeframes (`last_15m`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThresholdWindows {
    pub trigger_window: Option<String>,
    pub recovery_window: Option<String>,
}

/// A monitor's query read with what its options give it (Datadog's): a formula monitor's variables
/// and dynamic thresholds (`options.variables`, `thresholds.critical_query`), a cumulative window's
/// start (`scheduling_options.evaluation_window`), an anomaly monitor's `threshold_windows`.
pub fn query_of(query: &str, o: &Options) -> Result<monitor::Query, String> {
    let q = if query.trim_start().starts_with("formula(") {
        let named = o.variables.iter().map(Variable::named).collect::<Result<Vec<(&str, String)>, String>>()?;
        let vars: Vec<(&str, &str)> = named.iter().map(|(n, q)| (*n, q.as_str())).collect();
        monitor::parse_formula(query, &vars, o.thresholds.critical_query.as_deref(), o.thresholds.critical_recovery_query.as_deref()).map_err(|e| e.to_string())?
    } else {
        if !o.variables.is_empty() || o.thresholds.critical_query.is_some() || o.thresholds.critical_recovery_query.is_some() {
            return Err("options.variables and thresholds.critical_query: a formula monitor's (formula(\"query1 / query2\").last(\"5m\") > threshold)".into());
        }
        monitor::parse(query).map_err(|e| e.to_string())?
    };
    let q = match o.scheduling_options.as_ref().and_then(|s| s.evaluation_window.as_ref()) {
        Some(w) => q.starting(w.hour_starts, w.day_starts.as_deref(), w.month_starts, w.timezone.as_deref()).map_err(|e| e.to_string())?,
        None => q,
    };
    match &o.threshold_windows {
        // Datadog's rule: both windows, or none.
        Some(ThresholdWindows { trigger_window: Some(trigger), recovery_window: Some(recovery) }) => q.windows(trigger, recovery).map_err(|e| e.to_string()),
        Some(_) => Err("threshold_windows: both trigger_window and recovery_window (Datadog's \"must include both\")".into()),
        None => Ok(q),
    }
}

/// A formula monitor's query by its name (Datadog's `options.variables`): a metric query
/// (`data_source: metrics`, its `query`), or a query of Datadog's event platform of logs, spans or
/// events (`data_source` `logs`, `spans`, `events`: its `search.query`, `compute` and `group_by`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Variable {
    pub name: String,
    pub data_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<EventSearch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute: Option<Compute>,
    /// Its groups' facets. Each one's `limit` and `sort` (Datadog's top values a facet) are kept as
    /// said, but kiln evaluates every group, as its log monitors do: none is left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<GroupBy>,
    /// Datadog's log indexes it reads, taken as a log monitor's `.index()` is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<String>,
}

/// An event platform query's search (Datadog's `search.query`, its log search syntax).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventSearch {
    pub query: String,
}

/// What an event platform query measures: Datadog's `compute.aggregation` (`count`,
/// `cardinality`, `median`, `pc75` to `pc99`, `sum`, `min`, `max`, `avg`) of its `metric` (a facet,
/// `@duration`). Its `interval` (a graph's step, ms) is kept as said: a monitor measures its whole
/// window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compute {
    pub aggregation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<i64>,
}

/// One of an event platform query's groups: its facet (`service`, `@http.status_code`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupBy {
    pub facet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<serde_json::Value>,
}

impl Variable {
    /// Its name and its query in kiln's language: a metric query as it is; one of logs, spans or
    /// events as `kiln_obs::monitor::records_query` writes it.
    fn named(&self) -> Result<(&str, String), String> {
        use kiln_obs::query::parse::RecordSource;
        if self.name.is_empty() {
            return Err("a formula's variable: its name".into());
        }
        let of = match self.data_source.as_str() {
            "metrics" => return self.query.clone().map(|q| (self.name.as_str(), q)).ok_or_else(|| format!("{}: its query (data_source metrics)", self.name)),
            "logs" => RecordSource::Logs,
            "spans" => RecordSource::Spans,
            "events" => RecordSource::Events,
            other => return Err(format!("{}: data_source {other} (metrics, logs, spans or events: {other} is data kiln does not keep)", self.name)),
        };
        let compute = self.compute.as_ref().ok_or_else(|| format!("{}: its compute (an aggregation: count, cardinality, median, pc75, pc90, pc95, pc98, pc99, sum, min, max, avg)", self.name))?;
        if let Some(i) = compute.interval.filter(|&i| i <= 0) {
            return Err(format!("{}: compute.interval {i} (milliseconds, above 0)", self.name));
        }
        if let Some(g) = self.group_by.iter().find(|g| g.facet.trim().is_empty() || g.limit.is_some_and(|l| l < 1)) {
            return Err(format!("{}: group_by {:?} (a facet; a limit of 1 or more)", self.name, g.facet));
        }
        let by: Vec<&str> = self.group_by.iter().map(|g| g.facet.trim()).collect();
        let search = self.search.as_ref().map_or("", |s| s.query.as_str());
        Ok((self.name.as_str(), kiln_obs::monitor::records_query(of, search, compute.aggregation.trim(), compute.metric.as_deref(), &by)))
    }
}

/// Datadog's `scheduling_options` of a monitor: its `evaluation_window`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulingOptions {
    pub evaluation_window: Option<EvaluationWindow>,
}

/// Where a metric monitor's cumulative window begins, as Datadog's API has it
/// (`kiln_obs::monitor::Query::starting` holds it to Datadog's rules): `hour_starts`, the minute of
/// the hour; `day_starts` (`04:00`) and `month_starts` (1), in `timezone`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvaluationWindow {
    pub hour_starts: Option<u32>,
    pub day_starts: Option<String>,
    pub month_starts: Option<u32>,
    pub timezone: Option<String>,
}

/// Datadog's type for a composite monitor.
pub const COMPOSITE: &str = "composite";
/// Datadog's type for a service check monitor.
pub const SERVICE_CHECK: &str = "service check";
/// Datadog's types of the monitors whose kind their query says, as its API names them.
pub const TYPES: [&str; 8] = ["metric alert", "query alert", "log alert", "trace-analytics alert", "slo alert", "error-tracking alert", "event-v2 alert", SERVICE_CHECK];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Datadog's monitor type: [`COMPOSITE`] for a composite, its query an expression of other
    /// monitors' ids; none (or one of [`TYPES`]) for the rest, whose kind their query says.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub query: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub options: Options,
    /// Unix ms until which it says nothing (`i64::MAX`: until unmuted).
    #[serde(default)]
    pub muted_until: Option<i64>,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub modified: i64,
}

/// An SLO monitor's value (`monitor::Kind::Slo`): the lesser of its SLO's burn rates over the long
/// window and the short (both must pass for it to; none while either has no data), or its error
/// budget left, percent.
fn slo_values(store: &Shared, id: &str, burn: Option<(i64, i64)>, now: i64) -> Result<Vec<monitor::Value>, monitor::MonitorError> {
    // Its queries read a part at a time (`kiln_obs::query::eval::Source`).
    let store = kiln_obs::query::eval::Source::Shared(store);
    let def = super::api::slo_def(id).ok_or_else(|| monitor::MonitorError::Eval(format!("no SLO {id}")))?;
    let slo = def.slo().map_err(monitor::MonitorError::Eval)?;
    let eval = |e: kiln_obs::slo::SloError| monitor::MonitorError::Eval(e.to_string());
    let value = match burn {
        Some((long, short)) => match (kiln_obs::slo::burn_rate(store, &slo, long, now).map_err(eval)?, kiln_obs::slo::burn_rate(store, &slo, short, now).map_err(eval)?) {
            (Some(l), Some(s)) => Some(l.min(s)),
            _ => None,
        },
        None => kiln_obs::slo::status(store, &slo, now).map_err(eval)?.budget_left,
    };
    Ok(value.map(|v| vec![monitor::Value::of(Vec::new(), v)]).unwrap_or_default())
}

/// An Error Tracking monitor's values (`monitor::Kind::ErrorTracking`): its issues' errors over
/// its window (`super::errors::Issues::monitor_values`), of the issues it watches since `since`
/// (when it was made or last changed).
fn error_values(store: &Shared, search: &str, new: bool, by: &[String], window_ms: i64, since: i64, now: i64) -> Result<Vec<monitor::Value>, monitor::MonitorError> {
    // Its metric read a round of series at a time (`kiln_obs::query::eval::Source`).
    let store = kiln_obs::query::eval::Source::Shared(store);
    let eval = |e: String| monitor::MonitorError::Eval(e);
    let issues = super::api::issues().map_err(|_| eval("Error Tracking's issues are kept by the server".into()))?;
    let search = kiln_obs::logs::search::parse(search).map_err(|e| eval(e.to_string()))?;
    let from = now.saturating_sub(window_ms);
    issues.monitor_values(&search, new, by, since, now, |few| {
        match few {
            Some(ids) => kiln_obs::errors::seen_of(store, ids, from, now),
            None => kiln_obs::errors::seen(store, &[], from, now),
        }
        .map_err(|e| eval(e.to_string()))
    })
}

/// The issue an Error Tracking monitor's group is of (none for other monitors' groups).
fn issue_of(m: &Monitor, g: &GroupState) -> Option<super::errors::Issue> {
    if !matches!(m.compiled(), Ok(Compiled::Query(q, _)) if matches!(q.kind, monitor::Kind::ErrorTracking { .. })) {
        return None;
    }
    let id = g.group.iter().find(|(k, _)| k == "issue.id").map(|(_, v)| v.as_str())?;
    super::api::issues().ok()?.get(id)
}

/// An issue's variables for its monitor's message, Datadog's (`issue.attributes.error.*`): its
/// type, message and file, and its latest error's stack when the message asks for it (a search);
/// what only a browser's or a phone's error says (`is_crash`, `category`, `handling`) nothing.
fn issue_vars(store: &Shared, i: &super::errors::Issue, template: &str, now: i64) -> Vec<(String, String)> {
    let var = |k: &str, v: &str| (format!("issue.attributes.error.{k}"), v.to_owned());
    let mut out = vec![var("type", &i.kind), var("message", &i.message), var("file", &i.file), var("is_crash", ""), var("category", ""), var("handling", "")];
    if template.contains("issue.attributes.error.stack") {
        out.push(var("stack", &super::errors::latest_stack(store, i, now).unwrap_or_default()));
    }
    out
}

/// A service check monitor's group's variables for its message: Datadog's `{{check_message}}`, its
/// latest run's message, and that run's status (`{{check_status}}`: OK, WARNING, CRITICAL, UNKNOWN).
fn check_vars(store: &Shared, q: &CheckQuery, group: &[(String, String)], now: i64) -> Vec<(String, String)> {
    let logs = held(store).logs_at();
    let run = checks::latest_run(&logs, &q.check, group, now, CHECK_NO_DATA_MS);
    let status = run.as_ref().and_then(|r| r.attrs.iter().find(|(k, _)| k == "check_status")).map_or(checks::UNKNOWN, |(_, v)| checks::code(v));
    vec![("check_message".into(), run.map(|r| r.message).unwrap_or_default()), ("check_status".into(), checks::name(status).into())]
}

/// A monitor, compiled: a query of the store and its thresholds, or a composite of other monitors.
#[derive(Debug, Clone)]
pub enum Compiled {
    Query(monitor::Query, Thresholds),
    Composite(Composite),
    /// A service check monitor: its query, and its thresholds of runs in a row.
    Check(CheckQuery, CheckThresholds),
}

impl Monitor {
    pub fn is_composite(&self) -> bool {
        self.kind.as_deref() == Some(COMPOSITE)
    }

    /// Its query parsed and its thresholds, the query's critical one and the options' others (the
    /// options' critical must be the query's, as Datadog requires); a composite's expression.
    pub fn compiled(&self) -> Result<Compiled, String> {
        if let Some(e) = self.options.every
            && e < EVERY_MIN_S
        {
            return Err(format!("evaluated every {e} s: at most every {EVERY_MIN_S}"));
        }
        self.options.check()?;
        let check = checks::is_check(&self.query) || self.kind.as_deref() == Some(SERVICE_CHECK);
        if (self.is_composite() || check) && (self.options.on_missing_data.is_some() || self.options.group_retention_duration.is_some()) {
            return Err(RECORDS_ONLY.into());
        }
        match self.kind.as_deref() {
            Some(COMPOSITE) => return composite::parse(&self.query).map(Compiled::Composite).map_err(|e| e.to_string()),
            None => {}
            Some(t) if TYPES.contains(&t) => {}
            Some(other) => return Err(format!("a monitor's type: {other} ({COMPOSITE}, {}, or none: the query says)", TYPES.join(", "))),
        }
        if check {
            return self.compiled_check().map(|(q, t)| Compiled::Check(q, t));
        }
        let q = query_of(&self.query, &self.options)?;
        if matches!(q.kind, monitor::Kind::ErrorTracking { .. }) && self.options.notify_no_data {
            return Err("an Error Tracking monitor has no NO DATA: an issue without errors in its window counts none".into());
        }
        let records = matches!(q.kind, monitor::Kind::Logs { .. } | monitor::Kind::Spans { .. } | monitor::Kind::ErrorTracking { .. } | monitor::Kind::RecordsFormula { .. });
        if !records && (self.options.on_missing_data.is_some() || self.options.group_retention_duration.is_some()) {
            return Err(RECORDS_ONLY.into());
        }
        if let Some(by) = &self.options.notify_by
            && !by.iter().any(|t| t == "*")
        {
            let keys = monitor::group_keys(&q);
            if let Some(t) = by.iter().find(|t| !keys.iter().any(|k| k.trim_start_matches('@') == t.trim().trim_start_matches('@'))) {
                return Err(format!("notify_by {t}: not one of the query's group tags ({}): Datadog's \"must be a subset of the grouping tags in the query\"", keys.join(", ")));
            }
        }
        let t = &self.options.thresholds;
        if q.dynamic.is_some() && (t.critical.is_some() || t.critical_recovery.is_some()) {
            return Err("a dynamic threshold (`> threshold`): no critical or critical_recovery number beside it (Datadog's \"cannot be combined with static thresholds\")".into());
        }
        if let Some(c) = t.critical
            && c != q.threshold
        {
            return Err(format!("the critical threshold {c} is not the query's {}", q.threshold));
        }
        let t = Thresholds { critical: q.threshold, warning: t.warning, critical_recovery: t.critical_recovery, warning_recovery: t.warning_recovery, dynamic: q.dynamic.is_some() };
        Ok(Compiled::Query(q, t))
    }

    /// A service check monitor's query and thresholds: runs in a row, whole numbers 1 to
    /// [`checks::MAX_RUNS`] (critical and ok 1 unless said, Datadog's), its query reading at least
    /// the most of them (Datadog's rule).
    fn compiled_check(&self) -> Result<(CheckQuery, CheckThresholds), String> {
        if let Some(t) = self.kind.as_deref()
            && t != SERVICE_CHECK
        {
            return Err(format!("a service check's query in a monitor of type {t}: its type is {SERVICE_CHECK}"));
        }
        let q = checks::parse(&self.query).map_err(|e| e.to_string())?;
        let runs = |what: &str, v: Option<f64>| match v {
            None => Ok(None),
            Some(n) if n.fract() == 0.0 && (1.0..=f64::from(checks::MAX_RUNS)).contains(&n) => Ok(Some(n as u32)),
            Some(n) => Err(format!("a service check's {what} threshold: {n} runs in a row, 1 to {}", checks::MAX_RUNS)),
        };
        let (o, d) = (&self.options.thresholds, CheckThresholds::default());
        let t = CheckThresholds {
            critical: runs("critical", o.critical)?.unwrap_or(d.critical),
            warning: runs("warning", o.warning)?,
            ok: runs("ok", o.ok)?.unwrap_or(d.ok),
            unknown: runs("unknown", o.unknown)?,
        };
        if q.last < t.most() {
            return Err(format!(".last({}) reads fewer runs than its thresholds ask in a row ({}): at least that many", q.last, t.most()));
        }
        Ok((q, t))
    }

    /// The monitors a composite reads (none for the rest).
    pub fn reads(&self) -> Vec<String> {
        if !self.is_composite() {
            return Vec::new();
        }
        composite::parse(&self.query).map(|c| c.ids).unwrap_or_default()
    }

    fn window_ms(q: &monitor::Query) -> i64 {
        match &q.kind {
            monitor::Kind::Metric { window_ms, .. }
            | monitor::Kind::Change { window_ms, .. }
            | monitor::Kind::RecordsFormula { window_ms, .. }
            | monitor::Kind::Logs { window_ms, .. }
            | monitor::Kind::Spans { window_ms, .. }
            | monitor::Kind::ErrorTracking { window_ms, .. } => *window_ms,
            // Ahead, not behind: an hour without a forecast is two "windows" without data.
            monitor::Kind::Forecast { .. } => 3_600_000,
            monitor::Kind::Slo { burn, .. } => burn.map_or(3_600_000, |b| b.0),
        }
    }

    pub fn muted(&self, now: i64) -> bool {
        self.muted_until.is_some_and(|u| u > now)
    }
}

/// A group's state, as the monitor last found it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupState {
    pub group: Vec<(String, String)>,
    pub state: String,
    pub value: Option<f64>,
    /// Unix ms: since when it is in this state, when it last had data, when it was last told.
    pub since: i64,
    pub last_data: i64,
    pub notified: i64,
    /// A downtime that held it ended with it in trouble, asking that its first recovery after say
    /// nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet_recovery: bool,
    /// Unix ms: when it last went from OK into trouble, and last went into a state of trouble
    /// (Datadog's `{{first_triggered_at}}`, `{{last_triggered_at}}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_triggered: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_triggered: Option<i64>,
    /// Unix ms: when the monitor first had it (`new_group_delay`); 0 for a group kept before.
    #[serde(default)]
    pub seen: i64,
    /// Times its state has been told again since it changed (`renotify_occurrences`).
    #[serde(default)]
    pub renotified: u32,
}

/// What a monitor's messages say of its thresholds (a composite has none: NaN, null in a notice).
#[derive(Debug, Clone, Copy)]
struct Said {
    threshold: f64,
    warning: Option<f64>,
    critical_recovery: Option<f64>,
    warning_recovery: Option<f64>,
    /// A service check monitor's OK runs in a row.
    ok: Option<f64>,
    op: Option<monitor::Op>,
}

impl Said {
    const NONE: Self = Self { threshold: f64::NAN, warning: None, critical_recovery: None, warning_recovery: None, ok: None, op: None };

    fn of_query(q: &monitor::Query, t: &Thresholds) -> Self {
        Self { threshold: q.threshold, warning: t.warning, critical_recovery: t.critical_recovery, warning_recovery: t.warning_recovery, ok: None, op: Some(q.op) }
    }

    fn of_check(t: &CheckThresholds) -> Self {
        Self { threshold: f64::from(t.critical), warning: t.warning.map(f64::from), ok: Some(f64::from(t.ok)), ..Self::NONE }
    }
}

/// What a monitor's messages say of its thresholds, as its definition has them.
fn said_of(m: &Monitor) -> Said {
    match m.compiled() {
        Ok(Compiled::Query(q, t)) => Said::of_query(&q, &t),
        Ok(Compiled::Check(_, t)) => Said::of_check(&t),
        _ => Said::NONE,
    }
}

/// What a group the evaluation did not find comes to (Datadog's `on_missing_data`, and before it
/// `notify_no_data` and its timeframe).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Missing {
    /// Its state stands, until it is forgotten.
    Keep,
    /// NO DATA once it has had none for `after_ms` (told unless `on_missing_data` is
    /// `show_no_data`: `Monitors::say`).
    NoData { after_ms: i64 },
    /// A count of nothing: a group in trouble is moved by a value of 0.
    Zero,
    /// OK at once.
    Resolve,
}

impl Missing {
    /// A monitor of a query's: its `on_missing_data`; else NO DATA after `no_data_ms` if it asks to be
    /// told; else, of logs, spans, events or Error Tracking, a count's 0 (Datadog's default: "Count
    /// queries evaluate missing data as zero", its log monitor page) and another's last state.
    fn of(o: &Options, q: &monitor::Query, no_data_ms: i64) -> Self {
        let records = matches!(q.kind, monitor::Kind::Logs { .. } | monitor::Kind::Spans { .. } | monitor::Kind::ErrorTracking { .. } | monitor::Kind::RecordsFormula { .. });
        let default = if monitor::counts(q) { Self::Zero } else { Self::Keep };
        match o.on_missing_data {
            Some(OnMissingData::ShowNoData | OnMissingData::ShowAndNotifyNoData) => Self::NoData { after_ms: 0 },
            Some(OnMissingData::Resolve) => Self::Resolve,
            Some(OnMissingData::Default) => default,
            None if o.notify_no_data => Self::NoData { after_ms: no_data_ms },
            None if records => default,
            None => Self::Keep,
        }
    }

    /// Of a monitor that is no query's (a service check's, a composite's): NO DATA after `no_data_ms`
    /// if it asks to be told.
    fn told(o: &Options, no_data_ms: i64) -> Self {
        if o.notify_no_data { Self::NoData { after_ms: no_data_ms } } else { Self::Keep }
    }
}

/// Why a monitor was not deleted.
#[derive(Debug)]
pub enum Refused {
    /// Composites read it (their names).
    Read(Vec<String>),
    Io(String),
}

#[derive(Default)]
struct Due {
    next: HashMap<String, i64>,
}

pub struct Monitors {
    dir: PathBuf,
    defs: Mutex<BTreeMap<String, Monitor>>,
    states: Mutex<BTreeMap<String, BTreeMap<String, GroupState>>>,
    pub integrations: Integrations,
    notifier: Notifier,
    link_base: String,
    due: Mutex<Due>,
    /// Told when a monitor is put or deleted: its evaluation's due time moves ([`Monitors::run`]).
    changed: crate::rt::Changed,
    /// Downtimes (`super::downtimes`): when what monitors say is held.
    pub downtimes: super::downtimes::Downtimes,
}

fn group_key(g: &[(String, String)]) -> String {
    g.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
}

/// A group's move from `prev` to `to` at `now`: since when, Datadog's triggered times (the first:
/// from OK into trouble; the last: into any state of trouble, "independently from its previous
/// state"), and its renotifications counted afresh.
fn moved(g: &mut GroupState, prev: State, to: State, now: i64) {
    g.state = to.as_str().into();
    g.since = now;
    g.renotified = 0;
    if to != State::Ok {
        if prev == State::Ok {
            g.first_triggered = Some(now);
        }
        g.last_triggered = Some(now);
    }
}

/// Whether a group that stays in `state` is told again now: Datadog's `renotify_interval` (minutes
/// since it was last told), `renotify_statuses` (ALERT and NO DATA unless said) and
/// `renotify_occurrences`.
fn renotifies(o: &Options, state: State, g: &GroupState, now: i64) -> bool {
    let Some(every) = o.renotify_interval.map(|mins| i64::from(mins) * 60_000).filter(|&r| r > 0) else { return false };
    let status = match state {
        State::Alert => Renotify::Alert,
        State::Warn => Renotify::Warn,
        State::NoData => Renotify::NoData,
        State::Ok => return false,
    };
    o.renotify_statuses.as_deref().unwrap_or(&[Renotify::Alert, Renotify::NoData]).contains(&status) && now - g.notified >= every && o.renotify_occurrences.is_none_or(|n| g.renotified < n)
}

/// A group's tags of those `notify_by` names (`*`: none), its notification group's.
fn projected(group: &[(String, String)], by: &[String]) -> Vec<(String, String)> {
    if by.iter().any(|b| b == "*") {
        return Vec::new();
    }
    group.iter().filter(|(k, _)| by.iter().any(|b| b.trim().trim_start_matches('@') == k.trim_start_matches('@'))).cloned().collect()
}

/// Datadog's `notify_by`: each notification group, by its key, its tags, the worst state of its
/// groups (ALERT over WARN over NO DATA over OK, as composites weigh them) and a group in that
/// state, by its key.
fn notified_by(groups: &BTreeMap<String, GroupState>, by: &[String]) -> BTreeMap<String, (Vec<(String, String)>, State, String)> {
    let mut out: BTreeMap<String, (Vec<(String, String)>, State, String)> = BTreeMap::new();
    for (key, g) in groups {
        let p = projected(&g.group, by);
        let s = State::parse(&g.state).unwrap_or(State::Ok);
        let e = out.entry(group_key(&p)).or_insert_with(|| (p, s, key.clone()));
        if composite::severity(s) > composite::severity(e.1) {
            e.1 = s;
            e.2.clone_from(key);
        }
    }
    out
}

/// The explorers' range that covers a window ([`EXPLORER_RANGES`]; past them all, the longest).
fn explorer_range(window_ms: i64) -> &'static str {
    EXPLORER_RANGES.iter().find(|(_, ms)| window_ms <= *ms).or(EXPLORER_RANGES.last()).map_or("30d", |&(r, _)| r)
}

/// A log, span or event monitor's group's latest records over its window (newest first, `n` at the
/// most): the prefix a message reads them by (Datadog's matching attribute variables, `{{log.…}}`,
/// `{{span.…}}`, `{{event.…}}`), the records (none where its search finds none), and its explorer's
/// link to them (`{{log.link}}`: "scoped on the events matching the query"). A formula's are its
/// first query's ("values resolve based on events extracted from the first query"). None for a
/// monitor of other data. The store is let go before the search, as a monitor of records reads them.
fn latest_records(store: &Shared, m: &Monitor, q: &monitor::Query, group: &[(String, String)], link_base: &str, now: i64, n: usize) -> Option<(&'static str, Vec<kiln_obs::logs::record::Record>, String)> {
    use kiln_obs::query::parse::RecordSource;
    let r = monitor::group_records(q, group)?;
    let (prefix, page) = match r.of {
        RecordSource::Logs => ("log", "logs"),
        RecordSource::Spans => ("span", "traces"),
        RecordSource::Events => ("event", "events"),
    };
    let link = format!("{link_base}/observe/{page}?q={}&range={}", monitor::template::urlencode(&r.search), explorer_range(r.window_ms));
    let parsed = if r.of == RecordSource::Spans { kiln_obs::logs::search::parse_spans(&r.search) } else { kiln_obs::logs::search::parse(&r.search) };
    let to = now - i64::from(m.options.evaluation_delay.unwrap_or(0)) * 1000;
    let ns = |ms: i64| ms.saturating_mul(1_000_000);
    let records = match parsed {
        Ok(search) => {
            let at = if r.of == RecordSource::Spans { held(store).spans_at() } else { held(store).logs_at() };
            at.search(&search, ns(to - r.window_ms), ns(to) - 1, n).map(|f| f.records).unwrap_or_default()
        }
        Err(e) => {
            tracing::debug!(monitor = %m.name, error = %e, "a monitor's group's records were not searched");
            Vec::new()
        }
    };
    Some((prefix, records, link))
}

/// A record's variables, Datadog's matching attribute variables of `prefix` (`log`, `span`, `event`;
/// a composite's monitor's, `a.log`): each attribute (`{{log.attributes.<key>}}`, and as a tag,
/// `{{log.tags.<key>}}`: kiln keeps a record's tags as attributes), and the reserved attributes its
/// page lists for the kind: a log's `message`, `service`, `status`, `source`, `host`, `span_id`,
/// `trace_id`, `timestamp`; a span's `env`, `operation_name`, `resource_name` (its name), `service`,
/// `status` (`ok`, `error`), `type`, ids and `timestamp`; an event's `id` (`super::api::event_id`),
/// `title`, `text`, `host.name`, `attributes` (an object, as the API writes one), `tags`
/// (`key:value`, by commas) and `timestamp`; each with its `link`. A log's and a span's ids are hex,
/// as kiln's API writes them; times as `{{last_triggered_at}}` is.
fn record_vars(prefix: &str, r: &kiln_obs::logs::record::Record, link: &str) -> Vec<(String, String)> {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let attr = |k: &str| r.attrs.iter().rev().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    let mut out: Vec<(String, String)> = Vec::with_capacity(2 * r.attrs.len() + 10);
    for (k, v) in &r.attrs {
        out.push((format!("{prefix}.attributes.{k}"), v.clone()));
        out.push((format!("{prefix}.tags.{k}"), v.clone()));
    }
    let mut put = |k: &str, v: String| out.push((format!("{prefix}.{k}"), v));
    put("link", link.to_owned());
    put("timestamp", monitor::template::local_time(r.ts.div_euclid(1_000_000), kiln_obs::query::calendar::Zone::UTC).unwrap_or_default());
    put("service", r.service.clone());
    let ids = |put: &mut dyn FnMut(&str, String)| {
        put("trace_id", r.trace_id.map(|t| hex(&t)).unwrap_or_default());
        put("span_id", r.span_id.map(|s| hex(&s)).unwrap_or_default());
    };
    match prefix.rsplit('.').next().unwrap_or(prefix) {
        "span" => {
            ids(&mut put);
            put("env", attr("env").unwrap_or_default());
            put("operation_name", attr("operation_name").unwrap_or_else(|| r.message.clone()));
            put("resource_name", r.message.clone());
            put("status", if kiln_obs::logs::record::level(r.severity) == "error" { "error" } else { "ok" }.to_owned());
            put("type", attr("span.type").unwrap_or_default());
        }
        "event" => {
            let title = attr("title").unwrap_or_else(|| r.message.lines().next().unwrap_or_default().to_owned());
            let text = r.message.strip_prefix(title.as_str()).map_or(r.message.as_str(), |t| t.trim_start_matches('\n'));
            put("id", super::api::event_id(r).to_string());
            put("text", text.to_owned());
            put("title", title);
            put("host.name", r.host.clone());
            put("attributes", serde_json::to_string(&super::api::AttrsOut(&r.attrs)).unwrap_or_default());
            put("tags", r.attrs.iter().map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}:{v}") }).collect::<Vec<_>>().join(","));
        }
        _ => {
            ids(&mut put);
            put("message", r.message.clone());
            put("status", kiln_obs::logs::record::level(r.severity).to_owned());
            put("source", r.source.clone());
            put("host", r.host.clone());
        }
    }
    out
}

/// Whether a message reads a host's own variables (`{{host.ip}}`, `{{host.metadata_…}}`, its tags'
/// `{{host.<key>.name}}`), beyond its group's `{{host.name}}`: `host.` not within a longer name
/// (`{{event.host.name}}` is an event's).
fn reads_host(texts: &str) -> bool {
    texts.match_indices("host.").any(|(i, _)| {
        let part = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '@' | '[' | ']' | '-');
        let before = texts.get(..i).and_then(|b| b.chars().next_back());
        let rest: String = texts.get(i + "host.".len()..).unwrap_or_default().chars().take_while(|c| part(*c)).collect();
        !before.is_some_and(part) && rest != "name"
    })
}

/// A group's variables of its host and its service (Datadog's "Customize the notification based on
/// the group"), where `texts` reads them: grouped by `host`, the host as the infrastructure's last
/// inventory has it ([`super::infra::Infra::last`]: a monitor's evaluation, on the blocking pool,
/// reads it as it is, and `Monitors::run` asks for one where a message reads a host's own
/// variables) ([`host_vars`]); grouped by `service`, its Service Catalog definition
/// ([`service_vars`]). Also the prefixes read (`host`, `service`), so what the host or the service
/// lacks says nothing.
fn group_vars(group: &[(String, String)], texts: &str) -> (Vec<(String, String)>, Vec<String>) {
    let (mut out, mut read) = (Vec::new(), Vec::new());
    let value = |k: &str| group.iter().find(|(g, _)| g == k).map(|(_, v)| v.as_str());
    if let Some(name) = value("host")
        && reads_host(texts)
    {
        read.push("host".to_owned());
        if let Some(inventory) = super::infra::get().and_then(|i| i.last())
            && let Some(h) = inventory.host(name)
        {
            out.extend(host_vars(h));
        }
    }
    if let Some(service) = value("service")
        && texts.contains("service.")
    {
        read.push("service".to_owned());
        if let Some(k) = super::catalog::get().and_then(|c| c.definition(service)) {
            out.extend(service_vars(&k.definition));
        }
    }
    (out, read)
}

/// A host's variables, Datadog's for a monitor grouped by `host`: `{{host.ip}}` (the Agent's
/// inventory's `ip_address`, else its resource's `host.ip`); `{{host.metadata_agent_version}}`,
/// `_machine`, `_platform` and `_processor` (the Agent's host metadata, Datadog's own `meta` fields,
/// else what kiln knows of its platform); and each of its tags, of every source
/// (`{{host.role.name}}`, Datadog's `is_match` example's), several values sorted, by commas.
fn host_vars(h: &super::infra::Host) -> Vec<(String, String)> {
    let p = h.platform();
    let agent = h.agent.as_deref();
    let meta = |k: &str| agent.and_then(|a| a.meta.get(k)).and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    let ip = agent.and_then(|a| a.field("ip_address")).or_else(|| h.tags.iter().find(|(k, _)| k == "host.ip").map(|(_, v)| v.clone()));
    let mut out = vec![
        ("host.ip".to_owned(), ip.unwrap_or_default()),
        ("host.metadata_agent_version".to_owned(), p.agent_version.unwrap_or_default()),
        ("host.metadata_machine".to_owned(), meta("machine").or(p.arch).unwrap_or_default()),
        ("host.metadata_platform".to_owned(), meta("platform").or(p.os).unwrap_or_default()),
        ("host.metadata_processor".to_owned(), meta("processor").or(p.cpu_model).unwrap_or_default()),
    ];
    let mut tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in h.tags_by_source().into_values().flatten() {
        let (k, v) = t.split_once(':').unwrap_or((t.as_str(), ""));
        if !k.is_empty() {
            tags.entry(k.to_owned()).or_default().push(v.to_owned());
        }
    }
    for (k, mut values) in tags {
        values.sort_unstable();
        values.dedup();
        out.push((format!("host.{k}.name"), values.join(",")));
    }
    out
}

/// A service's variables, Datadog's for a monitor grouped by `service`, from its Service Catalog
/// definition (§10.8): `{{service.team}}`; `{{service.links}}`, each link `name: url`, by commas,
/// and `{{service.docs}}`, those of type `doc` (its schema's docs since v2.1); and one by its name,
/// `{{service.links[Runbook]}}`, `{{service.docs[API]}}`: its URL.
fn service_vars(d: &super::catalog::definition::Definition) -> Vec<(String, String)> {
    let said = |l: &super::catalog::definition::Link| format!("{}: {}", l.name, l.url);
    let docs = || d.links.iter().filter(|l| l.kind == "doc");
    let mut out = vec![
        ("service.team".to_owned(), d.team.clone().unwrap_or_default()),
        ("service.links".to_owned(), d.links.iter().map(said).collect::<Vec<_>>().join(", ")),
        ("service.docs".to_owned(), docs().map(said).collect::<Vec<_>>().join(", ")),
    ];
    out.extend(d.links.iter().map(|l| (format!("service.links[{}]", l.name), l.url.clone())));
    out.extend(docs().map(|l| (format!("service.docs[{}]", l.name), l.url.clone())));
    out
}

/// Datadog's `enable_logs_sample`: a log monitor's notification of trouble shows its group's latest
/// logs, each a line: its time, status, service and host, and its message's first line.
fn samples_text(rs: &[kiln_obs::logs::record::Record]) -> String {
    let mut out = String::from("Log samples:");
    for r in rs {
        let at = monitor::template::local_time(r.ts.div_euclid(1_000_000), kiln_obs::query::calendar::Zone::UTC).unwrap_or_default();
        let _ = write!(out, "\n- {at} {} {} {}: {}", kiln_obs::logs::record::level(r.severity), r.service, r.host, r.message.lines().next().unwrap_or_default());
    }
    out
}

impl Monitors {
    pub fn open(dir: &Path, http: Arc<crate::http::client::Client>, link_base: &str) -> Arc<Self> {
        let load = |name: &str| std::fs::read(dir.join(name)).ok();
        let defs = load("monitors.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let states = load("monitor-states.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Arc::new(Self {
            dir: dir.to_owned(),
            defs: Mutex::new(defs),
            states: Mutex::new(states),
            integrations: Integrations::open(dir),
            notifier: Notifier::start(http),
            link_base: link_base.trim_end_matches('/').to_owned(),
            due: Mutex::new(Due::default()),
            changed: crate::rt::Changed::new(),
            downtimes: super::downtimes::Downtimes::open(dir),
        })
    }

    fn save(&self) -> anyhow::Result<()> {
        let defs = serde_json::to_vec_pretty(&*lock(&self.defs))?;
        crate::sync::group::save_atomic(&self.dir.join("monitors.json"), &defs)?;
        Ok(())
    }

    fn save_states(&self) {
        let Ok(b) = serde_json::to_vec(&*lock(&self.states)) else { return };
        if let Err(e) = crate::sync::group::save_atomic(&self.dir.join("monitor-states.json"), &b) {
            tracing::warn!(error = %e, "monitors' states were not saved");
        }
    }

    pub fn list(&self) -> Vec<Monitor> {
        lock(&self.defs).values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Monitor> {
        lock(&self.defs).get(id).cloned()
    }

    /// Creates or replaces a monitor; its query and options checked first, and a composite's
    /// monitors (each there, none a composite, not itself).
    pub fn put(&self, mut m: Monitor) -> Result<Monitor, String> {
        let compiled = m.compiled()?;
        if m.name.trim().is_empty() {
            return Err("a monitor's name".into());
        }
        let now = crate::util::now_ms();
        let mut defs = lock(&self.defs);
        if let Compiled::Composite(c) = &compiled {
            for id in &c.ids {
                match defs.get(id) {
                    _ if *id == m.id => return Err("a composite does not read itself".into()),
                    None => return Err(format!("no monitor {id}")),
                    Some(o) if o.is_composite() => return Err(format!("{} is a composite: a composite reads other kinds of monitor", o.name)),
                    Some(_) => {}
                }
            }
        }
        // A monitor composites read cannot become one: they read other kinds.
        if m.is_composite() && !m.id.is_empty() {
            let readers: Vec<&str> = defs.values().filter(|c| c.reads().contains(&m.id)).map(|c| c.name.as_str()).collect();
            if !readers.is_empty() {
                return Err(format!("composites read it ({}): it cannot be one", readers.join(", ")));
            }
        }
        let made = m.id.is_empty() || !defs.contains_key(&m.id);
        if m.id.is_empty() {
            m.id = super::fresh_id("m", |id| defs.contains_key(id));
            m.created = now;
        } else if let Some(old) = defs.get(&m.id) {
            m.created = old.created;
            // A changed query starts its groups over.
            if old.query != m.query {
                lock(&self.states).remove(&m.id);
            }
        } else {
            m.created = now;
        }
        m.modified = now;
        defs.insert(m.id.clone(), m.clone());
        drop(defs);
        lock(&self.due).next.remove(&m.id);
        self.changed.notify();
        self.save().map_err(|e| format!("{e:#}"))?;
        self.tell_audit(&m, if made { "created" } else { "modified" });
        Ok(m)
    }

    /// Deletes a monitor, unless a composite reads it (then `Refused::Read`, naming them).
    pub fn delete(&self, id: &str) -> Result<bool, Refused> {
        let mut defs = lock(&self.defs);
        let readers: Vec<String> = defs.values().filter(|c| c.reads().iter().any(|r| r == id)).map(|c| c.name.clone()).collect();
        if !readers.is_empty() {
            return Err(Refused::Read(readers));
        }
        let gone = defs.remove(id);
        drop(defs);
        lock(&self.due).next.remove(id);
        self.changed.notify();
        lock(&self.states).remove(id);
        self.save().map_err(|e| Refused::Io(format!("{e:#}")))?;
        self.save_states();
        if let Some(m) = &gone {
            self.tell_audit(m, "deleted");
        }
        Ok(gone.is_some())
    }

    pub fn mute(&self, id: &str, until: Option<i64>) -> Result<Option<Monitor>, String> {
        let mut defs = lock(&self.defs);
        let Some(m) = defs.get_mut(id) else { return Ok(None) };
        m.muted_until = until;
        let m = m.clone();
        drop(defs);
        self.save().map_err(|e| format!("{e:#}"))?;
        Ok(Some(m))
    }

    pub fn groups(&self, id: &str) -> Vec<GroupState> {
        lock(&self.states).get(id).map(|g| g.values().cloned().collect()).unwrap_or_default()
    }

    /// The monitor's overall state: its worst group's.
    pub fn overall(&self, id: &str) -> &'static str {
        let worst = lock(&self.states).get(id).and_then(|g| g.values().filter_map(|s| State::parse(&s.state)).max_by_key(|s| match s {
            State::Ok => 0,
            State::NoData => 1,
            State::Warn => 2,
            State::Alert => 3,
        }));
        worst.unwrap_or(State::Ok).as_str()
    }

    /// When the next evaluation is due (unix ms) by what's kept at `now`: the first monitor's (one not evaluated yet:
    /// now), or a downtime's window beginning or ending ([`super::downtimes::Downtimes::next_change`]); none with
    /// neither (goal 9, plan 01a1a87b2cec step 5).
    fn next_due(&self, now: i64) -> Option<i64> {
        let first = {
            let d = lock(&self.due);
            lock(&self.defs).values().map(|m| d.next.get(&m.id).copied().unwrap_or(now)).min()
        };
        [first, self.downtimes.next_change(now)].into_iter().flatten().min()
    }

    /// Evaluates every monitor due by `now`, each under a hold of the store of its own.
    fn tick(self: &Arc<Self>, store: &Shared, now: i64) {
        let due: Vec<Monitor> = {
            let mut d = lock(&self.due);
            lock(&self.defs)
                .values()
                .filter(|m| d.next.get(&m.id).is_none_or(|&t| t <= now))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .inspect(|m| {
                    d.next.insert(m.id.clone(), now + i64::from(m.options.every.unwrap_or(EVERY_S).max(EVERY_MIN_S)) * 1000);
                })
                .collect()
        };
        let mut changed = false;
        // Queries first: composites read what they found.
        let mut evaluated: HashSet<&str> = HashSet::new();
        for m in due.iter().filter(|m| !m.is_composite()) {
            changed |= self.evaluate(store, m, now);
            evaluated.insert(m.id.as_str());
        }
        let composites: Vec<Monitor> = lock(&self.defs).values().filter(|m| m.is_composite()).cloned().collect();
        for c in composites {
            let Ok(Compiled::Composite(x)) = c.compiled() else { continue };
            if due.iter().any(|d| d.id == c.id) || x.ids.iter().any(|id| evaluated.contains(id.as_str())) {
                changed |= self.evaluate_composite(store, &c, &x, now);
            }
        }
        changed |= self.tell_ends(store, now);
        if changed {
            self.save_states();
        }
    }

    /// Downtimes' windows that ended since last looked at: each group one held that is in one of
    /// its end states told once (as a renotification), and asked, if the downtime says so, to keep
    /// its first recovery quiet. Whether any state changed.
    fn tell_ends(&self, store: &Shared, now: i64) -> bool {
        let ended = self.downtimes.ended(now);
        if ended.is_empty() {
            return false;
        }
        let defs: Vec<Monitor> = lock(&self.defs).values().cloned().collect();
        let mut changed = false;
        for e in ended {
            let d = &e.downtime;
            let Ok(scope) = kiln_obs::monitor::downtime::Scope::parse(&d.scope) else { continue };
            let tell = d.notify_end_types.iter().any(|t| t == e.how);
            for m in defs.iter().filter(|m| d.names(&m.id, &m.tags)) {
                let groups: Vec<GroupState> = lock(&self.states).get(&m.id).map(|gs| gs.values().filter(|g| scope.holds(&g.group)).cloned().collect()).unwrap_or_default();
                for g in groups {
                    let state = State::parse(&g.state).unwrap_or(State::Ok);
                    if state == State::Ok {
                        continue;
                    }
                    if d.mute_first_recovery_notification
                        && let Some(gs) = lock(&self.states).get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group)))
                    {
                        gs.quiet_recovery = true;
                        changed = true;
                    }
                    let named = match state {
                        State::Alert => "alert",
                        State::Warn => "warn",
                        State::NoData => "no data",
                        State::Ok => "ok",
                    };
                    if tell && d.notify_end_states.iter().any(|s| s == named) {
                        tracing::debug!(monitor = %m.name, downtime = %d.id, how = e.how, group = %group_key(&g.group), "a downtime ended with a group in trouble: told");
                        self.say(store, m, said_of(m), &g, state, state, now);
                    }
                }
            }
        }
        changed
    }

    /// One monitor's evaluation: each group's next state, events and notifications for changes.
    /// Whether any state changed. A monitor of logs or spans reads its records as they are, the
    /// store let go (`kiln_obs::store::LogsAt`); one of metrics, an SLO's or Error Tracking's, a
    /// round of series at a time (`kiln_obs::query::eval::Lease`).
    fn evaluate(&self, store: &Shared, m: &Monitor, now: i64) -> bool {
        let (q, t) = match m.compiled() {
            Ok(Compiled::Query(q, t)) => (q, t),
            Ok(Compiled::Check(q, t)) => return self.evaluate_check(store, m, &q, &t, now),
            _ => return false,
        };
        let delay = i64::from(m.options.evaluation_delay.unwrap_or(0)) * 1000;
        let values = match &q.kind {
            monitor::Kind::Slo { id, burn } => slo_values(store, id, *burn, now - delay),
            monitor::Kind::ErrorTracking { search, new, by, window_ms } => error_values(store, search, *new, by, *window_ms, m.modified, now - delay),
            _ => monitor::evaluate_shared(store, &q, now, delay),
        };
        // Datadog's `require_full_window` (a metric monitor's): a group whose window is not full is
        // not evaluated now, its state as it was.
        let full = if m.options.require_full_window { monitor::full_windows(kiln_obs::query::eval::Source::Shared(store), &q, now, delay) } else { Ok(None) };
        let (mut values, full) = match (values, full) {
            (Ok(v), Ok(f)) => (v, f),
            (Err(e), _) | (_, Err(e)) => {
                tracing::debug!(monitor = %m.name, error = %e, "a monitor did not evaluate");
                return false;
            }
        };
        let mut not_full: HashSet<Group> = HashSet::new();
        if let Some(full) = &full {
            let (kept, held): (Vec<monitor::Value>, Vec<monitor::Value>) = values.into_iter().partition(|v| full.contains(&v.group));
            not_full = held.into_iter().map(|v| v.group).collect();
            values = kept;
        }
        let no_data_ms = m.options.no_data_timeframe.map_or(2 * Monitor::window_ms(&q), |mins| i64::from(mins) * 60_000);
        // What each group's state moves by besides its value (`kiln_obs::monitor::Own`).
        let owns: HashMap<Group, monitor::Own> = values.iter().map(|v| (v.group.clone(), v.own)).collect();
        let found: Vec<(Group, Option<f64>, Option<State>)> = values.into_iter().map(|v| (v.group, Some(v.value), None)).collect();
        // A group it no longer finds: an Error Tracking monitor's (its issue resolved, ignored, no
        // longer new, or without errors in the window) counts none and recovers, as a log monitor's
        // count does ([`Missing::of`]).
        let missing = Missing::of(&m.options, &q, no_data_ms);
        let notify_no_data = matches!(missing, Missing::NoData { .. });
        self.apply(store, m, now, missing, found, &not_full, |group, prev, value, _| monitor::next(prev, value, owns.get(group).copied().unwrap_or_default(), q.op, &t, notify_no_data), Said::of_query(&q, &t))
    }

    /// A service check monitor's evaluation: each group moved by its last runs
    /// (`kiln_obs::monitor::checks`), or left as it is where they settle nothing. A group is found
    /// while it has a run within the no-data timeframe (a day unless the monitor says, Datadog's for
    /// a service check); one not found has had none for that long, and goes to NO DATA at once
    /// when the monitor asks to be told.
    fn evaluate_check(&self, store: &Shared, m: &Monitor, q: &CheckQuery, t: &CheckThresholds, now: i64) -> bool {
        let delay = i64::from(m.options.evaluation_delay.unwrap_or(0)) * 1000;
        let lookback = m.options.no_data_timeframe.map_or(CHECK_NO_DATA_MS, |mins| i64::from(mins) * 60_000);
        let groups = checks::evaluate(&held(store), q, t, now - delay, lookback);
        let found = groups.into_iter().map(|g| (g.group, Some(f64::from(g.status)), g.state)).collect();
        self.apply(store, m, now, Missing::told(&m.options, 0), found, &HashSet::new(), |_, prev, _, made| made.unwrap_or(prev), Said::of_check(t))
    }

    /// A composite's evaluation: its groups' states from the states of the monitors it reads, as
    /// they last found them. A group that comes out NO DATA keeps its last state, unless the
    /// composite asks to be told (Datadog's "last known state").
    fn evaluate_composite(&self, store: &Shared, m: &Monitor, c: &Composite, now: i64) -> bool {
        let inputs: Vec<Vec<(Group, State)>> = {
            let all = lock(&self.states);
            c.ids.iter().map(|id| all.get(id).map(|gs| gs.values().map(|g| (g.group.clone(), State::parse(&g.state).unwrap_or(State::Ok))).collect()).unwrap_or_default()).collect()
        };
        let Some(results) = composite::evaluate(c, &inputs) else { return false };
        let every_ms = i64::from(m.options.every.unwrap_or(EVERY_S).max(EVERY_MIN_S)) * 1000;
        let no_data_ms = m.options.no_data_timeframe.map_or(2 * every_ms, |mins| i64::from(mins) * 60_000);
        let notify = m.options.notify_no_data;
        let found: Vec<(Group, Option<f64>, Option<State>)> = results.into_iter().map(|(g, s)| (g, None, Some(s))).collect();
        self.apply(store, m, now, Missing::told(&m.options, no_data_ms), found, &HashSet::new(), |_, prev, _, s| s.filter(|&s| s != State::NoData || notify).unwrap_or(prev), Said::NONE)
    }

    /// What an evaluation found, applied: each group found moves from its last state to what
    /// `next(last, its value, the state the evaluation found it in, if any)` says, unless it is new
    /// within the monitor's `new_group_delay`; a group in `held` (its window not full) stays as it
    /// is; one not found comes to what `missing` says, a group in trouble resolves after `timeout_h`
    /// without data, and a group is forgotten after its retention (a day unless said). Changes are
    /// said, and groups in trouble said again when due; under `notify_by`, its notification groups'
    /// instead. Whether any state changed.
    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        store: &Shared,
        m: &Monitor,
        now: i64,
        missing: Missing,
        found: Vec<(Group, Option<f64>, Option<State>)>,
        held: &HashSet<Group>,
        next: impl Fn(&Group, State, Option<f64>, Option<State>) -> State,
        said: Said,
    ) -> bool {
        let o = &m.options;
        let delay_ms = i64::from(o.new_group_delay.unwrap_or(0)) * 1000;
        let timeout_ms = o.timeout_h.map(|h| i64::from(h) * 3_600_000);
        let forget_ms = o.group_retention_duration.as_deref().and_then(duration_ms).unwrap_or(FORGET_MS);
        let by = o.notify_by.as_deref();
        let mut transitions: Vec<(GroupState, State)> = Vec::new();
        let mut renotify: Vec<GroupState> = Vec::new();
        // Under `notify_by`: each notification group told, the state it came from, whether again,
        // and its key.
        let mut by_told: Vec<(GroupState, State, bool, String)> = Vec::new();
        let mut changed = false;
        {
            let mut all = lock(&self.states);
            let groups = all.entry(m.id.clone()).or_default();
            let before = by.map(|b| notified_by(groups, b));
            let mut seen: HashSet<String> = held.iter().map(|g| group_key(g)).collect();
            for (group, value, made) in found {
                let key = group_key(&group);
                if !groups.contains_key(&key) && groups.len() >= GROUPS {
                    continue;
                }
                seen.insert(key.clone());
                let g = groups.entry(key).or_insert_with(|| GroupState {
                    group,
                    state: State::Ok.as_str().into(),
                    value: None,
                    since: now,
                    last_data: now,
                    notified: 0,
                    quiet_recovery: false,
                    first_triggered: None,
                    last_triggered: None,
                    seen: now,
                    renotified: 0,
                });
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                // Datadog's `new_group_delay`: "Time (in seconds) to skip evaluations for new groups",
                // a group new that "has not been seen in the last 24 hours".
                if now - g.last_data >= NEW_AFTER_MS {
                    g.seen = now;
                }
                let to = if g.seen > 0 && now - g.seen < delay_ms { prev } else { next(&g.group, prev, value, made) };
                g.value = value;
                g.last_data = now;
                if to != prev {
                    moved(g, prev, to, now);
                    transitions.push((g.clone(), prev));
                    changed = true;
                } else if by.is_none() && renotifies(o, to, g, now) {
                    renotify.push(g.clone());
                }
            }
            groups.retain(|key, g| seen.contains(key) || now - g.last_data < forget_ms);
            for (key, g) in groups.iter_mut() {
                if seen.contains(key) {
                    continue;
                }
                let prev = State::parse(&g.state).unwrap_or(State::Ok);
                let quiet = now - g.last_data;
                let (to, value) = match missing {
                    // Datadog's `timeout_h`: "Hours without data before the monitor auto-resolves from
                    // a triggered state", ALERT or WARN ("Monitors do not auto-resolve from an ALERT
                    // or WARN state if data is still reporting"); NO DATA is the lack itself.
                    _ if matches!(prev, State::Alert | State::Warn) && timeout_ms.is_some_and(|t| quiet >= t) => (State::Ok, g.value),
                    Missing::NoData { after_ms, .. } if quiet >= after_ms => (State::NoData, None),
                    // "Empty result is replaced with zero and is compared to the alert/warning
                    // thresholds", in whatever state (a count's `< 1` alerts on its group gone quiet).
                    Missing::Zero => (next(&g.group, prev, Some(0.0), None), Some(0.0)),
                    Missing::Resolve => (State::Ok, g.value),
                    Missing::Keep | Missing::NoData { .. } => (prev, g.value),
                };
                if to != prev {
                    g.value = value;
                    moved(g, prev, to, now);
                    transitions.push((g.clone(), prev));
                    changed = true;
                }
            }
            if let (Some(by), Some(before)) = (by, before) {
                // What is told is its notification groups' moves, not their groups'.
                transitions.clear();
                for (k, (p, to, rep)) in notified_by(groups, by) {
                    let Some(r) = groups.get(&rep) else { continue };
                    let mut told = r.clone();
                    told.group = p;
                    let was = before.get(&k).map_or(State::Ok, |b| b.1);
                    if to != was {
                        by_told.push((told, was, false, k));
                        continue;
                    }
                    let (notified, renotified) = groups.values().filter(|g| group_key(&projected(&g.group, by)) == k).fold((0_i64, 0_u32), |(n, c), g| (n.max(g.notified), c.max(g.renotified)));
                    if renotifies(o, to, &GroupState { notified, renotified, ..told.clone() }, now) {
                        by_told.push((told, to, true, k));
                    }
                }
            }
        }
        for (g, prev) in transitions {
            // The group as it is now (its new state and value).
            let now_g = lock(&self.states).get(&m.id).and_then(|gs| gs.get(&group_key(&g.group)).cloned()).unwrap_or(g);
            let to = State::parse(&now_g.state).unwrap_or(State::Ok);
            self.say(store, m, said, &now_g, prev, to, now);
        }
        for g in renotify {
            let s = State::parse(&g.state).unwrap_or(State::Ok);
            self.say(store, m, said, &g, s, s, now);
        }
        if let Some(by) = by {
            for (g, from, again, key) in by_told {
                let to = State::parse(&g.state).unwrap_or(State::Ok);
                // Its groups told with it (`say` keeps a group's own, where the two are one).
                if self.say(store, m, said, &g, if again { to } else { from }, to, now)
                    && let Some(gs) = lock(&self.states).get_mut(&m.id)
                {
                    let own = group_key(&g.group);
                    for member in gs.iter_mut().filter(|(k, x)| **k != own && group_key(&projected(&x.group, by)) == key).map(|(_, x)| x) {
                        member.notified = now;
                        if again {
                            member.renotified = member.renotified.saturating_add(1);
                        }
                    }
                }
            }
        }
        changed
    }

    /// A composite's monitors' variables for its group's message (Datadog's "Composite monitor
    /// variables"): `a`, `b`, … its monitors as its query first names them (its form letters them as
    /// they are chosen, and its API writes the condition with their ids), each its group's that the
    /// composite's group holds (by its tags' values, as composites match groups) or its one group:
    /// `{{a.value}}`, `{{a.status}}` (`OK`, `Alert`, `Warn`, `No Data`), and where `texts` reads
    /// them, its records' (`{{a.log.message}}`). Also the prefixes of the records read.
    fn composite_vars(&self, store: &Shared, c: &Composite, group: &[(String, String)], texts: &str, now: i64) -> (Vec<(String, String)>, Vec<String>) {
        let (mut out, mut blank) = (Vec::new(), Vec::new());
        let values: HashSet<&str> = group.iter().map(|(_, v)| v.as_str()).collect();
        for (id, letter) in c.ids.iter().zip('a'..='z') {
            let found = lock(&self.states).get(id).and_then(|gs| gs.values().find(|g| g.group.iter().all(|(_, v)| values.contains(v.as_str()))).cloned());
            let Some(g) = found else { continue };
            let status = match State::parse(&g.state).unwrap_or(State::Ok) {
                State::Ok => "OK",
                State::Alert => "Alert",
                State::Warn => "Warn",
                State::NoData => "No Data",
            };
            out.push((format!("{letter}.value"), g.value.map_or_else(|| "no data".to_owned(), monitor::template::num)));
            out.push((format!("{letter}.status"), status.to_owned()));
            if !["log.", "span.", "event."].iter().any(|p| texts.contains(&format!("{letter}.{p}"))) {
                continue;
            }
            let Some(sub) = self.get(id) else { continue };
            let Ok(Compiled::Query(q, _)) = sub.compiled() else { continue };
            if let Some((prefix, rs, link)) = latest_records(store, &sub, &q, &g.group, &self.link_base, now, 1) {
                let p = format!("{letter}.{prefix}");
                match rs.first() {
                    Some(r) => out.extend(record_vars(&p, r, &link)),
                    None => out.push((format!("{p}.link"), link)),
                }
                blank.push(p);
            }
        }
        (out, blank)
    }

    /// A group's change of state (or its renotification, `from` its state still): its event, and,
    /// unless the monitor is muted, its message to every integration the message mentions. An Error
    /// Tracking monitor's group is an issue's: its message given the issue's variables, its link the
    /// issue's page, and nothing said of it while the issue is ignored (Datadog's). A log, span or
    /// event monitor's message reads the group's latest record (Datadog's `{{log.message}}`, ...),
    /// a composite's its monitors' (`{{a.value}}`). Whether it was told.
    #[allow(clippy::too_many_arguments)]
    fn say(&self, store: &Shared, m: &Monitor, said: Said, g: &GroupState, from: State, to: State, now: i64) -> bool {
        let issue = issue_of(m, g);
        let link = match &issue {
            Some(i) => format!("{}/observe/errors/{}", self.link_base, i.id),
            None => format!("{}/observe/monitors/{}", self.link_base, m.id),
        };
        let renotify = from == to;
        let compiled = m.compiled();
        // What the message and its escalation read.
        let texts = format!("{}\n{}", m.message, m.options.escalation_message.as_deref().unwrap_or_default());
        let mut blank: Vec<String> = Vec::new();
        let mut extra = match (&issue, &compiled) {
            (Some(i), _) => issue_vars(store, i, &texts, now),
            (None, Ok(Compiled::Check(q, _))) => check_vars(store, q, &g.group, now),
            (None, Ok(Compiled::Composite(c))) => {
                let (vars, read) = self.composite_vars(store, c, &g.group, &texts, now);
                blank.extend(read);
                vars
            }
            _ => Vec::new(),
        };
        if issue.is_some() {
            extra.push(("issue.link".into(), link.clone()));
        }
        let (of_group, read) = group_vars(&g.group, &texts);
        extra.extend(of_group);
        blank.extend(read);
        // Datadog's `{{#is_unknown}}`: a service check's group in NO DATA by its UNKNOWN runs.
        let unknown = to == State::NoData && extra.iter().any(|(k, v)| k == "check_status" && v == checks::name(checks::UNKNOWN));
        // The group's latest records, searched for only where its message reads them or its
        // notification shows them (Datadog's `enable_logs_sample`: "not displayed for recovery").
        let samples = m.options.enable_logs_sample && matches!(to, State::Alert | State::Warn);
        let reads = ["log.", "span.", "event."].iter().any(|p| texts.contains(p));
        let records = match &compiled {
            Ok(Compiled::Query(q, _)) if reads || samples => latest_records(store, m, q, &g.group, &self.link_base, now, if samples { SAMPLES } else { 1 }),
            _ => None,
        };
        if let Some((prefix, rs, records_link)) = &records {
            match rs.first() {
                Some(r) => extra.extend(record_vars(prefix, r, records_link)),
                None => extra.push((format!("{prefix}.link"), records_link.clone())),
            }
            blank.push((*prefix).to_owned());
        }
        let tr = Transition { from, to };
        let vars = Vars {
            monitor: &m.name,
            value: g.value,
            threshold: said.threshold,
            warn_threshold: said.warning,
            critical_recovery: said.critical_recovery,
            warning_recovery: said.warning_recovery,
            ok_threshold: said.ok,
            comparator: said.op,
            group: &g.group,
            link: &link,
            extra: &extra,
            first_triggered: g.first_triggered,
            last_triggered: g.last_triggered,
            now,
            priority: m.priority,
            renotify,
            unknown,
            blank: &blank,
        };
        let rendered = monitor::render_message(&m.message, tr, &vars);
        let mut message = rendered.text;
        // Datadog's `escalation_message`: "Message to include with a re-notification".
        if renotify && let Some(e) = &m.options.escalation_message {
            let more = monitor::render(e, tr, &vars);
            if !more.trim().is_empty() {
                message.push_str("\n\n");
                message.push_str(&more);
            }
        }
        if samples
            && let Some(("log", rs, _)) = &records
            && !rs.is_empty()
        {
            message.push_str("\n\n");
            message.push_str(&samples_text(rs));
        }
        let downtime = self.downtimes.holding(&m.id, &m.tags, &g.group, now);
        // A recovery a downtime that ended asked to keep quiet: this one, once.
        let quiet = to == State::Ok && from != State::Ok && {
            let mut all = lock(&self.states);
            let gs = all.get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group)));
            gs.is_some_and(|gs| std::mem::take(&mut gs.quiet_recovery))
        };
        let ignored = issue.as_ref().is_some_and(|i| i.status == super::errors::Status::Ignored);
        // Datadog's `on_missing_data: show_no_data`: NO DATA shown, not told.
        let unsaid = to == State::NoData && m.options.on_missing_data == Some(OnMissingData::ShowNoData);
        let muted = m.muted(now) || downtime.is_some() || quiet || ignored || unsaid;
        if let Some(o) = super::api::get() {
            let mut attrs: Vec<(String, String)> = vec![
                ("event.type".into(), format!("monitor.{}", to.as_str().to_ascii_lowercase().replace(' ', "_"))),
                ("monitor_id".into(), m.id.clone()),
                ("monitor".into(), m.name.clone()),
                ("state".into(), to.as_str().into()),
                ("previous".into(), from.as_str().into()),
                ("muted".into(), muted.to_string()),
            ];
            if let Some(id) = &downtime {
                attrs.push(("downtime_id".into(), id.clone()));
            }
            if let Some(v) = g.value {
                attrs.push(("value".into(), v.to_string()));
            }
            for (k, v) in &g.group {
                attrs.push((format!("group.{k}"), v.clone()));
            }
            let severity = match to {
                State::Alert => 17,
                State::Warn | State::NoData => 13,
                State::Ok => 9,
            };
            let title = format!("{} {} → {}{}", m.name, from.as_str(), to.as_str(), if g.group.is_empty() { String::new() } else { format!(" on {}", group_key(&g.group)) });
            o.log(vec![kiln_obs::logs::record::Record { ts: now.saturating_mul(1_000_000), severity, service: "kiln.monitor".into(), host: String::new(), source: "kiln.event".into(), message: format!("{title}: {message}"), attrs, trace_id: None, span_id: None }]);
            if let Ok(k) = kiln_obs::batch::SeriesKey::new("kiln.monitor.transitions", [("monitor_id", m.id.as_str()), ("state", to.as_str())], kiln_obs::batch::Kind::Count) {
                o.write(vec![(k, vec![(now, 1.0)])]);
            }
        }
        if muted {
            return false;
        }
        let notice = Notice {
            monitor_id: m.id.clone(),
            monitor: m.name.clone(),
            state: to.as_str(),
            previous: from.as_str(),
            group: g.group.clone(),
            value: g.value,
            threshold: said.threshold,
            message: message.clone(),
            link,
            at: now,
            // Datadog's `{{override_priority 'P1'}}`, else the monitor's own.
            priority: rendered.priority.or(m.priority),
            tags: m.tags.clone(),
            event: "monitor.transition",
            headline: None,
            facts: None,
            include_tags: m.options.include_tags.unwrap_or(true),
            footer: m.options.notification_preset_name.is_none_or(Preset::footer),
        };
        let mentions = monitor::mentions(&message);
        let mut incident = false;
        for mention in &mentions {
            // `@incident` (`@incident-sev2`, `@incident-<name>`): an incident of the group declared,
            // or its timeline told (`super::incidents`); the first such mention alone.
            if mention == "incident" || mention.starts_with("incident-") {
                if !std::mem::replace(&mut incident, true)
                    && let Some(k) = super::incidents::get()
                {
                    let from = super::incidents::FromMonitor { id: m.id.clone(), name: m.name.clone(), group: g.group.clone() };
                    k.from_monitor(&from, mention, to.as_str(), g.value, &message, &mentions, now);
                }
                continue;
            }
            match self.integrations.mentioned(mention) {
                Some((name, i)) => self.notifier.send(&name, i, notice.clone()),
                None => tracing::debug!(monitor = %m.name, %mention, "a mention names no integration"),
            }
        }
        if let Some(gs) = lock(&self.states).get_mut(&m.id).and_then(|gs| gs.get_mut(&group_key(&g.group))) {
            gs.notified = now;
            // Datadog's `renotify_occurrences` counts what is told again.
            if renotify {
                gs.renotified = gs.renotified.saturating_add(1);
            }
        }
        true
    }

    /// Datadog's `notify_audit`: the handles a monitor's message mentions told it was made, changed
    /// or deleted ("tagged users are notified of changes to the monitor"). A change of definition is
    /// no alert: told where people read (chat, email, webhooks), not paged or ticketed (PagerDuty,
    /// Opsgenie, Jira, ServiceNow, whose every notice raises or resolves one).
    fn tell_audit(&self, m: &Monitor, what: &str) {
        if !m.options.notify_audit {
            return;
        }
        let notice = Notice {
            monitor_id: m.id.clone(),
            monitor: m.name.clone(),
            state: "",
            previous: "",
            group: Vec::new(),
            value: None,
            threshold: 0.0,
            message: format!("{} was {what}.\n\n{}", m.name, m.query),
            link: format!("{}/observe/monitors/{}", self.link_base, m.id),
            at: crate::util::now_ms(),
            priority: m.priority,
            tags: m.tags.clone(),
            event: "monitor.audit",
            headline: Some(format!("[Monitor {what}] {}", m.name)),
            facts: Some(vec![("Query".into(), m.query.clone())]),
            include_tags: true,
            footer: m.options.notification_preset_name.is_none_or(Preset::footer),
        };
        for mention in monitor::mentions(&m.message) {
            match self.integrations.mentioned(&mention) {
                Some((_, Integration::Pagerduty { .. } | Integration::Opsgenie { .. } | Integration::Jira { .. } | Integration::Servicenow { .. })) => {}
                Some((name, i)) => self.notifier.send(&name, i, notice.clone()),
                None => tracing::debug!(monitor = %m.name, %mention, "a mention names no integration"),
            }
        }
    }

    /// Whether a monitor's message (or its escalation) reads a host's own variables ([`reads_host`]).
    fn reads_hosts(&self) -> bool {
        lock(&self.defs).values().any(|m| reads_host(&m.message) || m.options.escalation_message.as_deref().is_some_and(reads_host))
    }

    /// An incident's change told to its handles (`@slack-ops`, …) as a monitor's notifications go:
    /// its declaring and each change of its state or severity, one PagerDuty incident or Opsgenie
    /// alert an incident (its public id), resolved as it is; its severity its priority.
    pub fn tell_incident(&self, t: &super::incidents::Told) {
        let i = &t.incident;
        let (state, word) = match (t.what, i.state.as_str()) {
            ("declared", _) => ("ALERT", "Declared"),
            (_, "resolved") => ("OK", "Resolved"),
            (_, "stable") => ("WARN", "Stable"),
            ("state", _) => ("ALERT", "Active"),
            _ => (if i.state == "active" { "ALERT" } else { "WARN" }, i.severity.as_str()),
        };
        let event = match t.what {
            "declared" => "incident.declared",
            "state" => "incident.state",
            _ => "incident.severity",
        };
        let notice = Notice {
            monitor_id: format!("incident-{}", i.public_id),
            monitor: i.title.clone(),
            state,
            previous: "",
            group: Vec::new(),
            value: None,
            threshold: 0.0,
            message: super::incidents::http::told_text(i, t.what),
            link: format!("{}/observe/incidents/{}", self.link_base, i.public_id),
            at: i.modified,
            // SEV-0 and SEV-1 first (Opsgenie's P1), on to SEV-5.
            priority: i.severity.strip_prefix("SEV-").and_then(|d| d.parse::<u8>().ok()).map(|d| d.max(1)),
            tags: vec![format!("severity:{}", i.severity), format!("state:{}", i.state)],
            event,
            headline: Some(format!("[{word}] Incident #{}: {}", i.public_id, i.title)),
            facts: Some(vec![("Severity".into(), i.severity.clone()), ("State".into(), i.state.clone()), ("Commander".into(), i.commander.clone().unwrap_or_else(|| "none".into()))]),
            include_tags: true,
            footer: true,
        };
        for mention in super::incidents::mentions(&i.notification_handles) {
            match self.integrations.mentioned(&mention) {
                Some((name, integration)) => self.notifier.send(&name, integration, notice.clone()),
                None => tracing::debug!(incident = i.public_id, %mention, "an incident's handle names no integration"),
            }
        }
    }

    /// A test notification to integration `name`.
    pub fn test(&self, name: &str) -> Result<(), String> {
        let i = self.integrations.get(name).ok_or_else(|| format!("no integration {name}"))?;
        let now = crate::util::now_ms();
        let notice = Notice {
            monitor_id: "test".into(),
            monitor: "A test from kiln".into(),
            state: "ALERT",
            previous: "OK",
            group: vec![("host".into(), "kiln".into())],
            value: Some(1.0),
            threshold: 0.0,
            message: "This is a test notification from kiln's monitors.".into(),
            link: format!("{}/observe/monitors", self.link_base),
            at: now,
            priority: None,
            tags: Vec::new(),
            event: "monitor.transition",
            headline: None,
            facts: None,
            include_tags: true,
            footer: true,
        };
        self.notifier.send(name, i, notice);
        Ok(())
    }

    /// Evaluates each monitor at its own interval, on the blocking pool, while the server leads its group with its
    /// store replayed (a replica that does not lead says nothing), until the process ends: woken when that changes,
    /// when a monitor or a downtime is put or goes, and when the next evaluation is due ([`Monitors::next_due`]); by
    /// nothing else (goal 9, plan 01a1a87b2cec step 5: it looked every second, about 0.6 timer wakes a second in the
    /// lab, train 34's census).
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        use futures::FutureExt as _;
        let observe = super::api::get();
        loop {
            // Listened for before anything is looked at: a change after the look ends this wait.
            let (changed, downtimes, leads) = (self.changed.listen(), self.downtimes.changed(), observe.map(super::Observe::leading));
            let leading = observe.and_then(|o| {
                let v = o.view();
                o.store_replayed().filter(|_| v.leader != 0 && v.leader == v.member)
            });
            let next = match leading {
                Some(store) => {
                    // A message that reads a host's own variables reads the infrastructure's last
                    // inventory as the evaluation finds it: one asked for first (made again in the
                    // background once it is past its freshness), else none would be made unless the
                    // hosts' page were open.
                    if self.reads_hosts()
                        && let Some(i) = super::infra::get()
                        && let Err(e) = i.inventory().await
                    {
                        tracing::debug!(error = ?e, "the hosts' inventory was not made for monitors' messages");
                    }
                    let me = Arc::clone(&self);
                    match crate::rt::blocking(move || {
                        me.tick(&store, crate::util::now_ms());
                        me.next_due(crate::util::now_ms())
                    })
                    .await
                    {
                        Ok(next) => next,
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "monitors weren't evaluated: looked at again as something changes");
                            None
                        }
                    }
                }
                None => None,
            };
            let now = (std::time::Instant::now(), crate::util::now_ms());
            let at = next.map(|ms| now.0 + Duration::from_millis(u64::try_from(ms.saturating_sub(now.1)).unwrap_or(0)));
            let leads = async {
                match leads {
                    Some(l) => l.await,
                    None => std::future::pending().await,
                }
            };
            futures::select_biased! {
                () = shutdown.cancelled().fuse() => return,
                () = changed.fuse() => {}
                () = downtimes.fuse() => {}
                () = leads.fuse() => {}
                () = crate::rt::due(at).fuse() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Datadog's types of monitor are taken (an Error Tracking monitor's among them), another is
    /// not; an Error Tracking monitor asking for NO DATA is refused (an issue without errors counts
    /// none).
    #[test]
    fn datadog_s_types_are_taken_and_an_error_tracking_monitor_has_no_no_data() {
        let m = |kind: Option<&str>, notify_no_data: bool| Monitor {
            id: String::new(),
            name: "new issues".into(),
            kind: kind.map(str::to_owned),
            query: r#"error-tracking("service:shop").source("backend").new().rollup("count").by("issue.id").last("1d") > 0"#.into(),
            message: String::new(),
            tags: Vec::new(),
            priority: None,
            options: Options { notify_no_data, ..Options::default() },
            muted_until: None,
            created: 0,
            modified: 0,
        };
        assert!(m(Some("error-tracking alert"), false).compiled().is_ok());
        assert!(m(None, false).compiled().is_ok());
        assert!(m(Some("rum alert"), false).compiled().is_err());
        assert!(m(Some("error-tracking alert"), true).compiled().is_err());
    }

    /// A service check monitor: of Datadog's type or none, its thresholds runs in a row (an alert
    /// on the first failure and a recovery on the first success unless it says), its query reading
    /// at least the most of them; another type's, or thresholds that are not runs, refused.
    #[test]
    fn a_service_check_monitor_counts_runs_in_a_row() -> anyhow::Result<()> {
        let m = |kind: Option<&str>, query: &str, thresholds: ThresholdsIn| Monitor {
            id: String::new(),
            name: "agent up".into(),
            kind: kind.map(str::to_owned),
            query: query.into(),
            message: String::new(),
            tags: Vec::new(),
            priority: None,
            options: Options { thresholds, ..Options::default() },
            muted_until: None,
            created: 0,
            modified: 0,
        };
        let compiled = |kind: Option<&str>, query: &str, thresholds: ThresholdsIn| match m(kind, query, thresholds).compiled() {
            Ok(Compiled::Check(_, t)) => Ok(t),
            Ok(_) => Err("not a service check's".to_owned()),
            Err(e) => Err(e),
        };
        let runs = |critical: Option<f64>, warning: Option<f64>, ok: Option<f64>, unknown: Option<f64>| ThresholdsIn { critical, warning, ok, unknown, ..ThresholdsIn::default() };
        let q = r#""datadog.agent.up".over("*").by("host").last(3).count_by_status()"#;
        anyhow::ensure!(compiled(Some(SERVICE_CHECK), q, ThresholdsIn::default()) == Ok(CheckThresholds::default()));
        anyhow::ensure!(compiled(None, q, runs(Some(3.0), Some(2.0), Some(2.0), Some(1.0))) == Ok(CheckThresholds { critical: 3, warning: Some(2), ok: 2, unknown: Some(1) }));
        for (kind, query, thresholds) in [
            (Some(SERVICE_CHECK), q, runs(Some(4.0), None, None, None)),
            (Some(SERVICE_CHECK), q, runs(Some(1.5), None, None, None)),
            (Some(SERVICE_CHECK), q, runs(None, None, Some(0.0), None)),
            (Some("metric alert"), q, ThresholdsIn::default()),
            (Some(SERVICE_CHECK), "avg(last_5m):cpu{*} > 1", ThresholdsIn::default()),
        ] {
            anyhow::ensure!(m(kind, query, thresholds).compiled().is_err(), "{kind:?} {query} taken");
        }
        Ok(())
    }

    fn metric_monitor() -> Monitor {
        Monitor {
            id: String::new(),
            name: "cpu".into(),
            kind: None,
            query: "avg(last_5m):cpu{*} > 1".into(),
            message: String::new(),
            tags: Vec::new(),
            priority: None,
            options: Options::default(),
            muted_until: None,
            created: 0,
            modified: 0,
        }
    }

    /// Datadog's formula monitor of its event platform (its API client's RUM and CI examples' shape:
    /// `search`, `indexes`, `compute`, `group_by` with its `limit` and `sort`) of logs: compiled to
    /// a formula of kiln's records queries, measured over the whole window, its variables kept as
    /// said; a data source kiln does not keep, a variable without its compute or with a group of no
    /// facet, refused.
    #[test]
    fn a_formula_monitor_of_logs_reads_records_and_other_products_are_refused() -> anyhow::Result<()> {
        let v = |name: &str, data_source: &str, search: &str| -> anyhow::Result<Variable> {
            Ok(serde_json::from_value(serde_json::json!({
                "data_source": data_source, "name": name, "search": { "query": search }, "indexes": ["*"],
                "compute": { "aggregation": "count" }, "group_by": [{ "facet": "service", "limit": 10, "sort": { "aggregation": "count", "order": "desc" } }],
            }))?)
        };
        let m = |variables: Vec<Variable>| Monitor { query: r#"formula("query1 / query2 * 100").last("15m") >= 5"#.into(), options: Options { variables, ..Options::default() }, ..metric_monitor() };
        let logs = m(vec![v("query1", "logs", "status:error")?, v("query2", "logs", "")?]);
        match logs.compiled() {
            Ok(Compiled::Query(q, _)) => anyhow::ensure!(
                matches!(&q.kind, monitor::Kind::RecordsFormula { query, window_ms: 900_000 } if query == r#"(logs("status:error").rollup("count").by("service")) / (logs("").rollup("count").by("service")) * 100"#),
                "{q:?}"
            ),
            other => anyhow::bail!("{other:?}"),
        }
        let kept = serde_json::to_value(&logs.options.variables)?;
        anyhow::ensure!(kept.pointer("/0/group_by/0/sort/order") == Some(&serde_json::json!("desc")) && kept.pointer("/0/indexes/0") == Some(&serde_json::json!("*")), "{kept}");
        let mut no_compute = v("query2", "logs", "")?;
        no_compute.compute = None;
        let mut no_facet = v("query2", "spans", "")?;
        if let Some(g) = no_facet.group_by.first_mut() {
            g.facet = " ".into();
        }
        for bad in [vec![v("query1", "rum", "")?, v("query2", "rum", "")?], vec![v("query1", "logs", "")?, no_compute], vec![v("query1", "spans", "")?, no_facet]] {
            anyhow::ensure!(m(bad.clone()).compiled().is_err(), "{bad:?} taken");
        }
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5: the next evaluation is due when the first monitor's is (one not evaluated yet:
    /// at once), and nothing is due with no monitor and no downtime. Do: monitors with none; one put; it evaluated, its
    /// next half a minute on; it deleted. Expect: none; now; its next; none.
    #[test]
    fn g9_s5_the_next_evaluation_is_the_first_monitors_own() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ms = Monitors::open(dir.path(), Arc::new(crate::http::client::Client::public()?), "http://kiln.test");
        let now = crate::util::now_ms();
        anyhow::ensure!(ms.next_due(now).is_none(), "nothing kept: nothing due");
        let m = ms.put(metric_monitor()).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(ms.next_due(now) == Some(now), "not evaluated yet: due now");
        lock(&ms.due).next.insert(m.id.clone(), now + 30_000);
        anyhow::ensure!(ms.next_due(now) == Some(now + 30_000), "its own next evaluation");
        ms.delete(&m.id).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        anyhow::ensure!(ms.next_due(now).is_none(), "deleted: nothing due");
        Ok(())
    }

    /// Goal 9, plan 01a1a87b2cec step 5 (train 34's census: the monitors' loop, about 0.6 timer wakes a second, every
    /// second whatever was due): with nothing due the monitors arm no clock, and a monitor put wakes them. Do: the
    /// monitors' loop started and settled; two and a half seconds; a monitor put. Expect: no poll in the quiet (it woke
    /// every second); the put wakes it.
    #[test]
    fn g9_s5_the_monitors_arm_no_clock_with_nothing_due_and_wake_for_a_monitor_put() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        // Each bound here only ends a hang: a loaded machine takes what time it takes.
        let until = std::time::Instant::now() + Duration::from_secs(300);
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let ms = Monitors::open(dir.path(), Arc::new(crate::http::client::Client::public()?), "http://kiln.test");
            let (polls, shutdown) = (Arc::new(AtomicU64::new(0)), crate::rt::Shutdown::new());
            drop(crate::rt::spawn(crate::rt::counting_polls(Arc::clone(&polls), Arc::clone(&ms).run(shutdown.clone()))));
            let seen = crate::rt::settled(&polls, until).await?;
            crate::rt::sleep(Duration::from_millis(2_500)).await;
            let woke = polls.load(Ordering::Relaxed).saturating_sub(seen);
            anyhow::ensure!(woke == 0, "{woke} polls in 2.5 s with nothing due");
            ms.put(metric_monitor()).map_err(anyhow::Error::msg)?;
            while polls.load(Ordering::Relaxed) == seen {
                anyhow::ensure!(std::time::Instant::now() < until, "a monitor put never woke the loop");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            shutdown.cancel();
            Ok(())
        })
    }

    fn test_store() -> Shared {
        Arc::new(kiln_obs::lock::RwLock::new(kiln_obs::store::Store::new(kiln_obs::hash::Keyed::new())))
    }

    /// Datadog's notification options as `apply` moves and tells groups: `new_group_delay` (a new
    /// group not evaluated within it; one unseen a day new again), `renotify_interval` with
    /// `renotify_statuses` and `renotify_occurrences` (told again in ALERT alone, twice), the
    /// triggered times as its variables page's table has them (WARN after ALERT moves the last, not
    /// the first), `timeout_h` (WARN resolved after an hour without data; NO DATA, the lack itself,
    /// not), a count's group gone quiet compared as 0 (`< 1` alerts), and `notify_by` (a service's
    /// one notification, told as its worst group moves, not as each of its hosts does).
    #[test]
    fn datadog_s_notification_options_move_and_tell_groups_as_its_pages_say() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ms = Monitors::open(dir.path(), Arc::new(crate::http::client::Client::public()?), "http://kiln.test");
        let store = test_store();
        let host = |h: &str| vec![("host".to_owned(), h.to_owned())];
        // Above 1 alerts, above 0.5 warns, no value keeps the state.
        let next = |_: &Group, prev: State, v: Option<f64>, _: Option<State>| match v {
            Some(v) if v > 1.0 => State::Alert,
            Some(v) if v > 0.5 => State::Warn,
            Some(_) => State::Ok,
            None => prev,
        };
        let state = |id: &str, g: &Group| lock(&ms.states).get(id).and_then(|gs| gs.get(&group_key(g)).cloned()).ok_or_else(|| anyhow::anyhow!("no group {g:?}"));
        let (min, t0) = (60_000, 1_800_000_000_000);
        let none: HashSet<Group> = HashSet::new();

        let m = Monitor { options: Options { new_group_delay: Some(120), renotify_interval: Some(10), renotify_statuses: Some(vec![Renotify::Alert]), renotify_occurrences: Some(2), timeout_h: Some(1), ..Options::default() }, ..metric_monitor() };
        let m = ms.put(m).map_err(anyhow::Error::msg)?;
        let run = |at: i64, found: Vec<(Group, Option<f64>, Option<State>)>| ms.apply(&store, &m, at, Missing::Keep, found, &none, next, Said::NONE);
        let a = host("a");
        run(t0, vec![(a.clone(), Some(5.0), None)]);
        anyhow::ensure!(state(&m.id, &a)?.state == "OK", "new: not evaluated within its delay");
        run(t0 + 2 * min, vec![(a.clone(), Some(5.0), None)]);
        let g = state(&m.id, &a)?;
        anyhow::ensure!(g.state == "ALERT" && g.first_triggered == Some(t0 + 2 * min) && g.last_triggered == Some(t0 + 2 * min) && g.notified == t0 + 2 * min, "{g:?}");
        for k in [12, 22, 32] {
            run(t0 + k * min, vec![(a.clone(), Some(5.0), None)]);
        }
        let g = state(&m.id, &a)?;
        anyhow::ensure!(g.renotified == 2 && g.notified == t0 + 22 * min, "told again twice, not a third time: {g:?}");
        run(t0 + 33 * min, vec![(a.clone(), Some(0.8), None)]);
        run(t0 + 50 * min, vec![(a.clone(), Some(0.8), None)]);
        let g = state(&m.id, &a)?;
        anyhow::ensure!(g.state == "WARN" && g.first_triggered == Some(t0 + 2 * min) && g.last_triggered == Some(t0 + 33 * min) && g.notified == t0 + 33 * min && g.renotified == 0, "WARN not told again: {g:?}");
        run(t0 + 90 * min, Vec::new());
        anyhow::ensure!(state(&m.id, &a)?.state == "WARN", "40 minutes without data: within timeout_h");
        run(t0 + 111 * min, Vec::new());
        let g = state(&m.id, &a)?;
        anyhow::ensure!(g.state == "OK" && g.first_triggered == Some(t0 + 2 * min), "an hour without data: resolved, its triggered times kept: {g:?}");
        let back = t0 + 50 * min + NEW_AFTER_MS;
        run(back, vec![(a.clone(), Some(5.0), None)]);
        anyhow::ensure!(state(&m.id, &a)?.state == "OK", "unseen a day: new again, its delay anew");
        run(back + 2 * min, vec![(a.clone(), Some(5.0), None)]);
        anyhow::ensure!(state(&m.id, &a)?.state == "ALERT");

        // NO DATA (asked for after 10 minutes) is not resolved by timeout_h.
        let n = ms.put(Monitor { name: "no data".into(), options: Options { notify_no_data: true, timeout_h: Some(1), ..Options::default() }, ..metric_monitor() }).map_err(anyhow::Error::msg)?;
        let nd = |at: i64, found: Vec<(Group, Option<f64>, Option<State>)>| ms.apply(&store, &n, at, Missing::NoData { after_ms: 10 * min }, found, &none, next, Said::NONE);
        nd(t0, vec![(a.clone(), Some(5.0), None)]);
        nd(t0 + 11 * min, Vec::new());
        nd(t0 + 120 * min, Vec::new());
        let g = state(&n.id, &a)?;
        anyhow::ensure!(g.state == "NO DATA" && g.last_triggered == Some(t0 + 11 * min), "{g:?}");

        // A count's group gone quiet is 0, compared with the thresholds.
        let below = |_: &Group, _: State, v: Option<f64>, _: Option<State>| if v.is_some_and(|v| v < 1.0) { State::Alert } else { State::Ok };
        let c = ms.put(Monitor { name: "quiet".into(), ..metric_monitor() }).map_err(anyhow::Error::msg)?;
        ms.apply(&store, &c, t0, Missing::Zero, vec![(a.clone(), Some(3.0), None)], &none, below, Said::NONE);
        ms.apply(&store, &c, t0 + min, Missing::Zero, Vec::new(), &none, below, Said::NONE);
        let g = state(&c.id, &a)?;
        anyhow::ensure!(g.state == "ALERT" && g.value == Some(0.0), "{g:?}");

        // notify_by service: one notification for web, as its worst host moves.
        let svc = |h: &str| vec![("host".to_owned(), h.to_owned()), ("service".to_owned(), "web".to_owned())];
        let b = Monitor { name: "by service".into(), query: "avg(last_5m):cpu{*} by {host,service} > 1".into(), options: Options { notify_by: Some(vec!["service".into()]), ..Options::default() }, ..metric_monitor() };
        let b = ms.put(b).map_err(anyhow::Error::msg)?;
        let by = |at: i64, x: f64, y: f64| ms.apply(&store, &b, at, Missing::Keep, vec![(svc("x"), Some(x), None), (svc("y"), Some(y), None)], &none, next, Said::NONE);
        by(t0, 0.0, 0.0);
        by(t0 + min, 5.0, 0.0);
        by(t0 + 2 * min, 5.0, 5.0);
        let (x, y) = (state(&b.id, &svc("x"))?, state(&b.id, &svc("y"))?);
        anyhow::ensure!(x.state == "ALERT" && y.state == "ALERT" && x.notified == t0 + min && y.notified == t0 + min, "web told once, as x alerted: {x:?} {y:?}");
        Ok(())
    }

    /// Datadog's bounds on its options, each refused past them (`evaluation_delay` over a day,
    /// `timeout_h` over 24, `renotify_occurrences` of 0, `group_retention_duration` outside an hour
    /// to 72 hours, `on_missing_data` beside `notify_no_data` or of a metric monitor, `notify_by` of
    /// a tag the query is not grouped by or `*` beside another) and taken within them.
    #[test]
    fn datadog_s_option_bounds_are_held() -> anyhow::Result<()> {
        let logs = Monitor { query: r#"logs("status:error").rollup("count").by("service").last("5m") > 10"#.into(), ..metric_monitor() };
        let grouped = Monitor { query: "avg(last_5m):cpu{*} by {host,service} > 1".into(), ..metric_monitor() };
        let with = |m: &Monitor, options: &Options| Monitor { options: options.clone(), ..m.clone() };
        for (m, o) in [
            (&grouped, Options { evaluation_delay: Some(86_401), ..Options::default() }),
            (&grouped, Options { timeout_h: Some(25), ..Options::default() }),
            (&grouped, Options { renotify_occurrences: Some(0), ..Options::default() }),
            (&logs, Options { group_retention_duration: Some("30m".into()), ..Options::default() }),
            (&logs, Options { group_retention_duration: Some("4d".into()), ..Options::default() }),
            (&logs, Options { on_missing_data: Some(OnMissingData::Resolve), notify_no_data: true, ..Options::default() }),
            (&grouped, Options { on_missing_data: Some(OnMissingData::ShowNoData), ..Options::default() }),
            (&grouped, Options { notify_by: Some(vec!["region".into()]), ..Options::default() }),
            (&grouped, Options { notify_by: Some(vec!["*".into(), "host".into()]), ..Options::default() }),
        ] {
            anyhow::ensure!(with(m, &o).compiled().is_err(), "{o:?} taken");
        }
        for (m, o) in [
            (&grouped, Options { evaluation_delay: Some(86_400), timeout_h: Some(24), renotify_occurrences: Some(1), notify_by: Some(vec!["service".into()]), ..Options::default() }),
            (&grouped, Options { notify_by: Some(vec!["*".into()]), ..Options::default() }),
            (&logs, Options { group_retention_duration: Some("2d".into()), on_missing_data: Some(OnMissingData::ShowAndNotifyNoData), ..Options::default() }),
        ] {
            if let Err(e) = with(m, &o).compiled() {
                anyhow::bail!("{o:?} refused: {e}");
            }
        }
        Ok(())
    }

    /// Datadog's matching attribute variables of a log, a span and an event (a composite's monitor's,
    /// `a.event`): attributes and tags by key, the reserved attributes its page lists for each kind,
    /// ids in hex, times as `{{last_triggered_at}}` writes them.
    #[test]
    fn a_record_s_variables_are_datadog_s_matching_attribute_variables() -> anyhow::Result<()> {
        use kiln_obs::logs::record::Record;
        // 2021-05-31 14:22:27 UTC.
        let log = Record {
            ts: 1_622_470_947_000_000_000,
            severity: 17,
            service: "web".into(),
            host: "h1".into(),
            source: "nginx".into(),
            message: "connection refused\nat x".into(),
            attrs: vec![("error.message".into(), "refused".into()), ("env".into(), "prod".into())],
            trace_id: Some([0xab; 16]),
            span_id: Some([1; 8]),
        };
        let is = |vars: &[(String, String)], k: &str, want: &str| vars.iter().find(|(n, _)| n == k).is_some_and(|(_, v)| v == want);
        let v = record_vars("log", &log, "L");
        for (k, want) in [
            ("log.message", "connection refused\nat x"),
            ("log.status", "error"),
            ("log.service", "web"),
            ("log.source", "nginx"),
            ("log.host", "h1"),
            ("log.attributes.error.message", "refused"),
            ("log.tags.env", "prod"),
            ("log.timestamp", "2021-05-31 14:22:27+00:00"),
            ("log.trace_id", "abababababababababababababababab"),
            ("log.span_id", "0101010101010101"),
            ("log.link", "L"),
        ] {
            anyhow::ensure!(is(&v, k, want), "{k}: {v:?}");
        }
        let span = Record { message: "GET /users".into(), attrs: vec![("operation_name".into(), "flask.request".into()), ("span.type".into(), "web".into()), ("env".into(), "prod".into())], ..log.clone() };
        let v = record_vars("span", &span, "S");
        for (k, want) in [("span.resource_name", "GET /users"), ("span.operation_name", "flask.request"), ("span.type", "web"), ("span.status", "error"), ("span.env", "prod"), ("span.service", "web")] {
            anyhow::ensure!(is(&v, k, want), "{k}: {v:?}");
        }
        let event = Record { severity: 9, source: "kiln.event".into(), message: "Deploy\n\nweb 1.2 out".into(), attrs: vec![("title".into(), "Deploy".into()), ("dot.key.test".into(), "five".into())], trace_id: None, span_id: None, ..log };
        let v = record_vars("a.event", &event, "E");
        for (k, want) in [("a.event.title", "Deploy"), ("a.event.text", "web 1.2 out"), ("a.event.host.name", "h1"), ("a.event.tags.dot.key.test", "five"), ("a.event.tags", "title:Deploy,dot.key.test:five"), ("a.event.link", "E")] {
            anyhow::ensure!(is(&v, k, want), "{k}: {v:?}");
        }
        anyhow::ensure!(v.iter().any(|(k, x)| k == "a.event.attributes" && x.contains(r#""dot.key.test":"five""#)), "{v:?}");
        Ok(())
    }

    /// Datadog's composite monitor variables: each monitor it reads lettered as its query first names
    /// it, `{{a.value}}` and `{{a.status}}` of its group the composite's holds (or of its one group).
    #[test]
    fn a_composite_s_message_reads_its_monitors_by_letter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let ms = Monitors::open(dir.path(), Arc::new(crate::http::client::Client::public()?), "http://kiln.test");
        let store = test_store();
        let cpu = ms.put(metric_monitor()).map_err(anyhow::Error::msg)?;
        let mem = ms.put(Monitor { name: "mem".into(), ..metric_monitor() }).map_err(anyhow::Error::msg)?;
        let host = |h: &str| vec![("host".to_owned(), h.to_owned())];
        let none: HashSet<Group> = HashSet::new();
        let alerting = |_: &Group, _: State, v: Option<f64>, _: Option<State>| if v.is_some_and(|v| v > 1.0) { State::Alert } else { State::Ok };
        ms.apply(&store, &cpu, 1, Missing::Keep, vec![(host("web1"), Some(5.5), None), (host("web2"), Some(0.0), None)], &none, alerting, Said::NONE);
        ms.apply(&store, &mem, 1, Missing::Keep, vec![(Vec::new(), Some(0.25), None)], &none, alerting, Said::NONE);
        let c = composite::parse(&format!("{} && !{}", mem.id, cpu.id)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (vars, read) = ms.composite_vars(&store, &c, &host("web1"), "", 2);
        let var = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        anyhow::ensure!(var("a.value") == Some("0.25") && var("a.status") == Some("OK") && var("b.value") == Some("5.5") && var("b.status") == Some("Alert") && read.is_empty(), "{vars:?}");
        Ok(())
    }

    /// Datadog's variables of a group by host and by service: the host's IP, its Agent's metadata
    /// (Datadog's own `meta` fields) and its tags of every source, several values sorted; the
    /// service's team, its links and docs, and each by its name; and which messages read a host's own
    /// variables (asking the hosts' inventory for them), `{{host.name}}` being the group's.
    #[test]
    fn a_group_s_host_and_service_variables_are_datadog_s() -> anyhow::Result<()> {
        use crate::observe::infra::{Figures, Host, agent::AgentHost};
        let agent = AgentHost {
            meta: serde_json::from_value(serde_json::json!({ "agent_version": "7.84.4", "machine": "amd64", "platform": "linux", "processor": "Intel(R) Xeon(R) Platinum 8375C" }))?,
            inventory: serde_json::from_value(serde_json::json!({ "ip_address": "10.0.4.17" }))?,
            tags_by_source: [("Datadog".to_owned(), vec!["role:db_postgres".to_owned(), "role:db_cassandra".to_owned(), "env:prod".to_owned()])].into(),
            ..AgentHost::default()
        };
        let h = Host {
            name: "db-1".into(),
            aliases: Vec::new(),
            apps: Vec::new(),
            sources: Vec::new(),
            last_reported_ms: None,
            seen_ms: 0,
            series: 0,
            figures: Figures::default(),
            tags: vec![("cloud.region".into(), "eu-west-1".into())],
            agent: Some(Arc::new(agent)),
            runner: None,
            mute: None,
        };
        let is = |vars: &[(String, String)], k: &str, want: &str| vars.iter().any(|(n, x)| n == k && x == want);
        let v = host_vars(&h);
        for (k, want) in [
            ("host.ip", "10.0.4.17"),
            ("host.metadata_agent_version", "7.84.4"),
            ("host.metadata_machine", "amd64"),
            ("host.metadata_platform", "linux"),
            ("host.metadata_processor", "Intel(R) Xeon(R) Platinum 8375C"),
            ("host.role.name", "db_cassandra,db_postgres"),
            ("host.env.name", "prod"),
            ("host.cloud.region.name", "eu-west-1"),
        ] {
            anyhow::ensure!(is(&v, k, want), "{k}: {v:?}");
        }
        let d: crate::observe::catalog::definition::Definition = serde_json::from_value(serde_json::json!({
            "schema-version": "v2.2",
            "dd-service": "web",
            "team": "payments",
            "links": [{ "name": "Runbook", "type": "runbook", "url": "https://wiki.example/web?a=1&b=2" }, { "name": "API", "type": "doc", "url": "https://docs.example/api" }],
        }))?;
        let v = service_vars(&d);
        for (k, want) in [
            ("service.team", "payments"),
            ("service.links", "Runbook: https://wiki.example/web?a=1&b=2, API: https://docs.example/api"),
            ("service.docs", "API: https://docs.example/api"),
            ("service.links[Runbook]", "https://wiki.example/web?a=1&b=2"),
            ("service.docs[API]", "https://docs.example/api"),
        ] {
            anyhow::ensure!(is(&v, k, want), "{k}: {v:?}");
        }
        for (text, reads) in [("{{host.name}} is down", false), ("{{host.ip}}", true), (r#"{{#is_match "host.role.name" "db"}}x{{/is_match}}"#, true), ("{{event.host.name}}", false), ("{{ host.metadata_platform }}", true)] {
            anyhow::ensure!(reads_host(text) == reads, "{text}: not {reads}");
        }
        Ok(())
    }
}
