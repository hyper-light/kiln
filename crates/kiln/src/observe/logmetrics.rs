//! Metrics from logs (`docs/observe.md` §8.3): what the server keeps of them, counts and says.
//!
//! A definition is Datadog's (`/api/v2/logs/config/metrics`): a name, what it computes (`count`, or
//! a `distribution` of an attribute's values, with or without percentiles), a filter (a log search)
//! and the attributes it groups by, each a tag. Each window of ten seconds of records' time is
//! counted on the server, the one replica sure to hold every record ([`kiln_obs::logs::metrics`]),
//! once the records it waits for have come, and counted again, whole, when more come for it within
//! twenty minutes (Datadog's "aggregation only considers logs ingested with a timestamp within the
//! past 20 minutes"); each count's points are written over the one's before.
//!
//! This is the Dataflow model's (Akidau et al., VLDB 2015): a window fired once its watermark
//! passes, fired again for records late to it, its panes accumulating (each count whole, so
//! overwriting needs no retraction), until its allowed lateness. The watermark is heuristic, learned
//! from how late records come ([`Lags::settle_ms`]): the 99th percentile of how late each batch's
//! newest record came over the last five minutes, a quarter more, 2 to 30 s; so in the steady
//! state each window is counted once, and a backfill now and then does not hold every window back.
//! A window counted again waits at least half as long as it has been since it ended, so however
//! records trickle in, one is counted some fifteen times at the most; and counting takes a quarter
//! of each second at the most.
//!
//! Definitions are `<observe>/log_metrics.json`.
//!
//! Metrics from spans are counted alike, over the span store ([`LogMetrics::open_spans`],
//! `<observe>/span_metrics.json`): Datadog's "generate metrics from spans", their filters read as
//! the trace explorer reads a search (`kiln_obs::logs::search::parse_spans`), a distribution of
//! `@duration` in seconds; and kiln's trace metrics among them always (`kiln_obs::apm`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kiln_obs::logs::metrics::{Compute, LAG_BUCKETS, LogMetric, Spans, Touched, WINDOW, lag_bound_ms, window_of};
use kiln_obs::logs::store::Facet;
use serde::{Deserialize, Serialize};

/// Metrics from logs kept at the most.
pub const METRICS: usize = 500;
/// How long after a window ends records still count in it: Datadog's twenty minutes.
pub const HORIZON_MS: i64 = kiln_obs::logs::metrics::HORIZON / 1_000_000;
/// How long counting may take each second at the most.
const BUDGET: Duration = Duration::from_millis(250);
/// When a window is first counted, after it ends, while nothing says how late records come.
const SETTLE_MS: i64 = 5_000;
const SETTLE_MIN_MS: i64 = 2_000;
const SETTLE_MAX_MS: i64 = 30_000;
/// The least a window counted waits to be counted again.
const AGAIN_MIN_MS: i64 = 1_000;
/// Names kiln writes itself, and Datadog keeps for its own.
const RESERVED: [&str; 5] = ["kiln.", "system.", "datadog.", "error_tracking.", kiln_obs::llm::metrics::PREFIX];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregation {
    #[default]
    Count,
    Distribution,
}

/// What a metric computes (Datadog's `compute`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeDef {
    pub aggregation_type: Aggregation,
    /// A distribution's attribute (`@duration`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub include_percentiles: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterDef {
    /// A log search; empty or `*`, every record.
    #[serde(default)]
    pub query: String,
}

/// An attribute grouped by, and the tag it becomes (its path without the `@`, unless named).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupByDef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag_name: Option<String>,
}

impl GroupByDef {
    pub fn tag(&self) -> String {
        self.tag_name.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| self.path.trim().trim_start_matches('@').to_owned())
    }
}

/// A metric from logs as the API takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogMetricDef {
    pub name: String,
    pub compute: ComputeDef,
    #[serde(default)]
    pub filter: FilterDef,
    #[serde(default)]
    pub group_by: Vec<GroupByDef>,
}

/// What the API takes: kiln's shape, or Datadog's (`{"data": {"id", "type": "logs_metrics",
/// "attributes": {...}}}`), so a definition of theirs is taken as it is.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum DefIn {
    Datadog { data: DatadogData },
    Kiln(LogMetricDef),
}

#[derive(Debug, Deserialize)]
pub struct DatadogData {
    #[serde(default)]
    id: Option<String>,
    attributes: DatadogAttributes,
}

#[derive(Debug, Deserialize)]
pub struct DatadogAttributes {
    compute: ComputeDef,
    #[serde(default)]
    filter: FilterDef,
    #[serde(default)]
    group_by: Vec<GroupByDef>,
}

impl DefIn {
    /// The definition, named `name` when it names none (an update's).
    pub fn def(self, name: Option<&str>) -> LogMetricDef {
        match self {
            Self::Kiln(d) => d,
            Self::Datadog { data } => LogMetricDef { name: data.id.or_else(|| name.map(str::to_owned)).unwrap_or_default(), compute: data.attributes.compute, filter: data.attributes.filter, group_by: data.attributes.group_by },
        }
    }
}

/// Whether `name` is a metric's name: Datadog's custom metrics' (a letter first, then ASCII
/// letters, digits, `_` and `.`; 200 at the most), and none kiln writes (of spans, its trace
/// metrics' `trace.*` neither).
fn check_name(name: &str, spans: bool) -> Result<(), String> {
    let ok = name.len() <= 200 && name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !ok {
        return Err("a metric's name: a letter, then letters, digits, _ and . (200 at the most)".into());
    }
    if let Some(p) = RESERVED.iter().find(|p| name.starts_with(*p)) {
        return Err(format!("{p}*: names kiln writes itself"));
    }
    if spans && name.starts_with(kiln_obs::apm::PREFIX) {
        return Err(format!("{}*: kiln's trace metrics' names", kiln_obs::apm::PREFIX));
    }
    Ok(())
}

impl LogMetricDef {
    /// Compiled, its windows counted from `since_ms` on.
    pub fn compile(&self, since_ms: i64) -> Result<LogMetric, String> {
        self.compile_for(since_ms, false)
    }

    /// As [`LogMetricDef::compile`], of spans when `spans`.
    pub fn compile_for(&self, since_ms: i64, spans: bool) -> Result<LogMetric, String> {
        check_name(&self.name, spans)?;
        let q = self.filter.query.trim();
        let q = if q.is_empty() { "*" } else { q };
        let search = if spans { kiln_obs::logs::search::parse_spans(q) } else { kiln_obs::logs::search::parse(q) }.map_err(|e| format!("the filter: {e}"))?;
        let compute = match self.compute.aggregation_type {
            Aggregation::Count => {
                if self.compute.path.as_deref().is_some_and(|p| !p.trim().is_empty() && p.trim() != "*") {
                    return Err("a count counts records: no path (a distribution takes one)".into());
                }
                Compute::Count
            }
            Aggregation::Distribution => {
                let path = self.compute.path.as_deref().map(str::trim).filter(|p| p.starts_with('@') && p.len() > 1).ok_or("a distribution's path: the attribute whose values it takes (@duration)")?;
                if spans && path == "@duration" {
                    // A span's duration in seconds, as kiln's trace metrics have it.
                    Compute::Duration { percentiles: self.compute.include_percentiles }
                } else {
                    Compute::Distribution { path: path.to_owned(), percentiles: self.compute.include_percentiles }
                }
            }
        };
        let mut by = Vec::with_capacity(self.group_by.len());
        for g in &self.group_by {
            if g.path.trim().is_empty() || g.path.trim() == "@" {
                return Err("a group's path: an attribute (@http.status_code) or service, host, source or status".into());
            }
            by.push((if spans { Facet::parse_span(&g.path) } else { Facet::parse(&g.path) }, g.tag()));
        }
        // A window that begins as it changed is the first it counts (none of its records counted
        // by the definition before).
        let since = since_ms.saturating_mul(1_000_000);
        let m = LogMetric { name: self.name.clone(), search, by, compute, since: window_of(since.saturating_add(WINDOW - 1)) };
        m.check()?;
        Ok(m)
    }
}

/// A metric from logs as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    #[serde(flatten)]
    pub def: LogMetricDef,
    #[serde(default)]
    pub created: i64,
    /// Windows from this (ms) on are counted as it is now.
    #[serde(default)]
    pub modified: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    metrics: Vec<Kept>,
    /// Names of metrics deleted: their series are a metric from logs' to write again.
    #[serde(default)]
    retired: BTreeSet<String>,
}

/// What a metric's latest window with records of it held.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MetricStatus {
    /// The latest window with records of it (its start, ms), and what it held.
    pub window: i64,
    pub records: u64,
    pub groups: usize,
    /// Records counted in its overflow group (those of groups past the limit).
    pub overflowed: u64,
}

/// How counting goes, as the API says it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counting {
    /// How long after a window ends it is first counted, as learned.
    pub settle_ms: i64,
    pub windows_counted: u64,
    /// Counted again, records having come for them after.
    pub windows_recounted: u64,
    /// Windows records came for past the horizon: those records are not counted.
    pub windows_expired: u64,
    /// Windows with records not counted yet.
    pub windows_waiting: usize,
    /// Batches whose newest record came over a minute late (a machine back from sleep).
    pub late_batches: u64,
    pub rows_scanned: u64,
    pub count_ms: u64,
    pub error: Option<String>,
}

/// How late batches' newest records came, each of the last five minutes ([`lag_bucket`]'s
/// counts).
///
/// [`lag_bucket`]: kiln_obs::logs::metrics::lag_bucket
#[derive(Debug, Default)]
struct Lags {
    minutes: [(i64, Option<Box<[u32; LAG_BUCKETS]>>); 5],
}

/// The share of batches a window waits for before it is first counted.
const SETTLE_QUANTILE: f64 = 0.99;

impl Lags {
    fn note(&mut self, now_ms: i64, lags: &[u32; LAG_BUCKETS]) {
        let minute = now_ms.div_euclid(60_000);
        let slot = usize::try_from(minute.rem_euclid(5)).unwrap_or(0);
        if let Some(m) = self.minutes.get_mut(slot) {
            if m.0 != minute || m.1.is_none() {
                *m = (minute, Some(Box::new([0; LAG_BUCKETS])));
            }
            if let Some(h) = m.1.as_deref_mut() {
                for (a, b) in h.iter_mut().zip(lags) {
                    *a = a.saturating_add(*b);
                }
            }
        }
    }

    /// When a window is first counted after it ends: the 99th percentile of the last five minutes'
    /// lateness, and a quarter more, 2 to 30 s.
    fn settle_ms(&self, now_ms: i64) -> i64 {
        let minute = now_ms.div_euclid(60_000);
        let mut all = [0u64; LAG_BUCKETS];
        for (m, h) in &self.minutes {
            if let Some(h) = h.as_deref().filter(|_| minute - m < 5) {
                for (a, b) in all.iter_mut().zip(h.iter()) {
                    *a += u64::from(*b);
                }
            }
        }
        let total: u64 = all.iter().sum();
        if total == 0 {
            return SETTLE_MS;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rank = ((total as f64) * SETTLE_QUANTILE).ceil() as u64;
        let mut seen = 0;
        let i = all.iter().position(|n| {
            seen += n;
            seen >= rank
        });
        let l = lag_bound_ms(i.unwrap_or(LAG_BUCKETS - 1));
        (l + l / 4).clamp(SETTLE_MIN_MS, SETTLE_MAX_MS)
    }
}

/// A window within the horizon.
#[derive(Debug, Default, Clone, Copy)]
struct Win {
    /// Records came for it since it was counted.
    dirty: bool,
    counted_ms: Option<i64>,
}

#[derive(Debug, Default)]
struct State {
    /// Whether every window within the horizon was taken to need counting (as the server starts).
    started: bool,
    windows: BTreeMap<i64, Win>,
    lags: Lags,
    counting: Counting,
    status: HashMap<String, MetricStatus>,
}

impl State {
    /// What came, taken: the windows it reaches within the horizon (and not far ahead) need counting.
    fn mark(&mut self, spans: &Spans, now_ms: i64) {
        let first = window_of((now_ms - HORIZON_MS).saturating_mul(1_000_000));
        let last = window_of(now_ms.saturating_add(10 * 60_000).saturating_mul(1_000_000));
        let all = spans.all || !self.started;
        self.started = true;
        let reach: Vec<(i64, i64)> = if all { vec![(first, window_of(now_ms.saturating_mul(1_000_000)))] } else { spans.windows.clone() };
        for (a, b) in reach {
            let mut w = a.max(first);
            while w <= b.min(last) {
                self.windows.entry(w).or_default().dirty = true;
                w += WINDOW;
            }
        }
        if spans.batches > spans.backlogs {
            self.lags.note(now_ms, &spans.lags);
        }
        self.counting.late_batches += spans.backlogs;
        // Past the horizon: what is not counted by now never is.
        let gone: Vec<i64> = self.windows.range(..first).map(|(w, _)| *w).collect();
        for w in gone {
            if self.windows.remove(&w).is_some_and(|x| x.dirty) {
                self.counting.windows_expired += 1;
            }
        }
        self.counting.settle_ms = self.lags.settle_ms(now_ms);
        self.counting.windows_waiting = self.windows.values().filter(|w| w.dirty).count();
    }

    /// The window to count now, if any: one never counted first, oldest first; then one counted
    /// again, oldest first.
    fn due(&self, now_ms: i64) -> Option<i64> {
        let settle = self.counting.settle_ms;
        let ended = |w: i64| (w + WINDOW).div_euclid(1_000_000);
        let ready = |w: i64, win: &Win| {
            let end = ended(w);
            win.dirty
                && now_ms >= end + settle
                && match win.counted_ms {
                    None => true,
                    Some(c) => now_ms >= c + ((c - end) / 2).max(AGAIN_MIN_MS),
                }
        };
        let fresh = self.windows.iter().find(|(w, win)| win.counted_ms.is_none() && ready(**w, win));
        fresh.or_else(|| self.windows.iter().find(|(w, win)| ready(**w, win))).map(|(w, _)| *w)
    }
}

/// The server's metrics from logs (or from spans).
pub struct LogMetrics {
    path: PathBuf,
    /// Of spans, over the span store, kiln's trace metrics among them.
    spans: bool,
    saved: Mutex<Saved>,
    compiled: Mutex<Arc<Vec<LogMetric>>>,
    state: Mutex<State>,
}

impl LogMetrics {
    pub fn open(dir: &Path) -> Arc<Self> {
        Self::open_at(dir.join("log_metrics.json"), false)
    }

    /// The server's metrics from spans: its trace metrics, and those defined.
    pub fn open_spans(dir: &Path) -> Arc<Self> {
        Self::open_at(dir.join("span_metrics.json"), true)
    }

    fn open_at(path: PathBuf, spans: bool) -> Arc<Self> {
        let saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let me = Arc::new(Self { path, spans, saved: Mutex::new(saved), compiled: Mutex::new(Arc::new(Vec::new())), state: Mutex::new(State::default()) });
        me.recompile();
        me
    }

    /// Whether these are metrics from spans.
    pub fn of_spans(&self) -> bool {
        self.spans
    }

    /// kiln's own metrics counted with those defined: of spans, its trace metrics and LLM
    /// Observability's (`kiln_obs::llm::metrics`); of both, Error Tracking's
    /// (`kiln_obs::errors::metrics`).
    pub fn builtin(&self) -> Vec<LogMetric> {
        let mut out = if self.spans { kiln_obs::apm::trace_metrics() } else { Vec::new() };
        if self.spans {
            out.extend(kiln_obs::llm::metrics::metrics());
        }
        out.extend(kiln_obs::errors::metrics(self.spans));
        out
    }

    fn recompile(&self) {
        let saved = lock(&self.saved);
        let mut out = self.builtin();
        for k in &saved.metrics {
            match k.def.compile_for(k.modified, self.spans) {
                Ok(m) => out.push(m),
                Err(e) => tracing::warn!(metric = %k.def.name, error = %e, "a metric from logs does not compile here; it is not counted"),
            }
        }
        *lock(&self.compiled) = Arc::new(out);
    }

    pub fn list(&self) -> Vec<Kept> {
        lock(&self.saved).metrics.clone()
    }

    pub fn get(&self, name: &str) -> Option<Kept> {
        lock(&self.saved).metrics.iter().find(|k| k.def.name == name).cloned()
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        crate::sync::group::save_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    /// A new metric, counted from the next window on. `written` says whether a metric name holds
    /// series already (written by something else, its series are not a metric from logs' to
    /// write): refused, as one whose series another metric from logs writes is.
    pub fn create(&self, def: LogMetricDef, written: impl Fn(&str) -> bool) -> Result<Kept, (u16, String)> {
        let now = super::clock_ms();
        let m = def.compile_for(now, self.spans).map_err(|e| (400, e))?;
        let names: Vec<String> = m.names().into_iter().map(|(n, _)| n).collect();
        let held = names.iter().find(|n| written(n)).cloned();
        let mut saved = lock(&self.saved);
        if saved.metrics.len() >= METRICS {
            return Err((400, format!("{METRICS} metrics from logs at the most")));
        }
        for k in &saved.metrics {
            let theirs = k.def.compile_for(k.modified, self.spans).map(|o| o.names().into_iter().map(|(n, _)| n).collect::<Vec<_>>()).unwrap_or_else(|_| vec![k.def.name.clone()]);
            if k.def.name == def.name || theirs.iter().any(|n| names.contains(n)) {
                return Err((409, format!("{}: written by the metric from logs {} already", def.name, k.def.name)));
            }
        }
        if !saved.retired.contains(&def.name)
            && let Some(n) = held
        {
            return Err((409, format!("{n}: series of that name are written already (by something besides a metric from logs); a metric has one writer")));
        }
        let kept = Kept { def, created: now, modified: now };
        saved.metrics.push(kept.clone());
        self.save(&saved).map_err(|e| (500, e))?;
        drop(saved);
        self.recompile();
        Ok(kept)
    }

    /// A metric changed: its filter, its groups and whether it keeps percentiles (what it computes
    /// and of what stay: a new metric changes those, Datadog's rule). Windows from the next on are
    /// counted as it is now.
    pub fn put(&self, name: &str, def: LogMetricDef) -> Result<Option<Kept>, (u16, String)> {
        let now = super::clock_ms();
        if def.name != name {
            return Err((400, "a metric's name stays: a new metric takes another".into()));
        }
        def.compile_for(now, self.spans).map_err(|e| (400, e))?;
        let mut saved = lock(&self.saved);
        let Some(k) = saved.metrics.iter_mut().find(|k| k.def.name == name) else { return Ok(None) };
        if k.def.compute.aggregation_type != def.compute.aggregation_type || k.def.compute.path.as_deref().map(str::trim) != def.compute.path.as_deref().map(str::trim) {
            return Err((400, "what a metric computes, and of what, stay: a new metric changes them".into()));
        }
        k.def = def;
        k.modified = now;
        let out = k.clone();
        self.save(&saved).map_err(|e| (500, e))?;
        drop(saved);
        self.recompile();
        Ok(Some(out))
    }

    /// A metric removed (its series stay, until retention): whether it was there.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let mut saved = lock(&self.saved);
        let before = saved.metrics.len();
        saved.metrics.retain(|k| k.def.name != name);
        let had = saved.metrics.len() != before;
        if had {
            saved.retired.insert(name.to_owned());
            self.save(&saved)?;
        }
        drop(saved);
        lock(&self.state).status.remove(name);
        self.recompile();
        Ok(had)
    }

    pub fn status(&self, name: &str) -> Option<MetricStatus> {
        lock(&self.state).status.get(name).cloned()
    }

    pub fn counting(&self) -> Counting {
        lock(&self.state).counting.clone()
    }

    /// Counts what is due, for a quarter of a second at the most: one window a brief hold of the
    /// store, what came taken under it with the records as they are (so a window counted holds
    /// every record noted for it), counted once the hold is let go (`kiln_obs::store::LogsAt`: no
    /// write waits on a count), its points offered after. Points not taken (the server is behind)
    /// leave the window to count again.
    pub fn tick(&self, store: &kiln_obs::machine::Shared, touched: &Touched, offer: &dyn Fn(Vec<(kiln_obs::batch::SeriesKey, Vec<(i64, f64)>)>) -> bool) {
        let metrics = lock(&self.compiled).clone();
        let started = Instant::now();
        loop {
            let now = super::clock_ms();
            let s = store.read().unwrap_or_else(PoisonError::into_inner);
            let spans = touched.take();
            if metrics.is_empty() {
                // Nothing to count: a metric made now counts from the next window on.
                return;
            }
            let mut st = lock(&self.state);
            st.mark(&spans, now);
            let Some(w) = st.due(now) else { return };
            drop(st);
            // Every part's records: those not kept for search are counted too (Datadog's).
            let at = if self.spans { s.spans_at() } else { s.logs_every() };
            drop(s);
            let counting = Instant::now();
            let counted = at.count_metrics(&metrics, w);
            let took = u64::try_from(counting.elapsed().as_millis()).unwrap_or(u64::MAX);
            let mut st = lock(&self.state);
            st.counting.count_ms += took;
            match counted {
                Ok(Some(c)) => {
                    st.counting.rows_scanned += c.rows_scanned;
                    if !c.series.is_empty() && !offer(c.series) {
                        // Behind: counted again next time.
                        return;
                    }
                    let again = st.windows.get(&w).is_some_and(|x| x.counted_ms.is_some());
                    st.windows.insert(w, Win { dirty: false, counted_ms: Some(now) });
                    if again {
                        st.counting.windows_recounted += 1;
                    } else {
                        st.counting.windows_counted += 1;
                    }
                    st.counting.error = None;
                    let ms = w.div_euclid(1_000_000);
                    for (m, t) in metrics.iter().zip(&c.tallies) {
                        if m.since > w {
                            continue;
                        }
                        if t.records == 0 {
                            continue;
                        }
                        let e = st.status.entry(m.name.clone()).or_default();
                        if ms >= e.window {
                            *e = MetricStatus { window: ms, records: t.records, groups: t.groups, overflowed: t.overflowed };
                        }
                    }
                }
                // The logs' retention reaches into it: not counted, as not all of it is held.
                Ok(None) => {
                    st.windows.remove(&w);
                }
                Err(e) => {
                    st.counting.error = Some(e.to_string());
                    st.windows.insert(w, Win { dirty: false, counted_ms: Some(now) });
                }
            }
            if started.elapsed() >= BUDGET {
                return;
            }
        }
    }

    /// Counts every second on the server, while it leads, until the process ends.
    pub async fn run(self: Arc<Self>, shutdown: crate::rt::Shutdown) {
        while shutdown.sleep(Duration::from_secs(1)).await {
            let Some(o) = super::api::get() else { continue };
            let v = o.view();
            if v.leader == 0 || v.leader != v.member {
                continue;
            }
            let Some(store) = o.store() else { continue };
            let me = self.clone();
            let _ = crate::rt::blocking(move || {
                let touched = if me.spans { o.spans_touched() } else { o.touched() }.clone();
                me.tick(&store, &touched, &|series| o.offer(series));
            })
            .await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn def(json: serde_json::Value) -> LogMetricDef {
        serde_json::from_value::<DefIn>(json).unwrap().def(None)
    }

    #[test]
    fn definitions_are_taken_in_kiln_s_shape_and_datadog_s() {
        let dd = def(serde_json::json!({ "data": { "id": "checkout.errors.count", "type": "logs_metrics", "attributes": { "compute": { "aggregation_type": "count" }, "filter": { "query": "service:checkout status:error" }, "group_by": [{ "path": "@env", "tag_name": "environment" }] } } }));
        let kiln = def(serde_json::json!({ "name": "checkout.errors.count", "compute": { "aggregation_type": "count" }, "filter": { "query": "service:checkout status:error" }, "group_by": [{ "path": "@env", "tag_name": "environment" }] }));
        assert_eq!(dd, kiln);
        let m = kiln.compile(0).unwrap();
        assert_eq!(m.by, vec![(Facet::Attr("env".into()), "environment".into())]);
        for bad in [
            serde_json::json!({ "name": "kiln.mine", "compute": { "aggregation_type": "count" } }),
            serde_json::json!({ "name": "9lives", "compute": { "aggregation_type": "count" } }),
            serde_json::json!({ "name": "a b", "compute": { "aggregation_type": "count" } }),
            serde_json::json!({ "name": "d", "compute": { "aggregation_type": "distribution" } }),
            serde_json::json!({ "name": "d", "compute": { "aggregation_type": "distribution", "path": "duration" } }),
            serde_json::json!({ "name": "c", "compute": { "aggregation_type": "count", "path": "@duration" } }),
            serde_json::json!({ "name": "c", "compute": { "aggregation_type": "count" }, "filter": { "query": "(unclosed" } }),
            serde_json::json!({ "name": "c", "compute": { "aggregation_type": "count" }, "group_by": [{ "path": "@a", "tag_name": "x" }, { "path": "@b", "tag_name": "x" }] }),
        ] {
            assert!(def(bad.clone()).compile(0).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_metric_has_one_writer_and_keeps_what_it_computes() {
        let dir = tempfile::tempdir().unwrap();
        let ms = LogMetrics::open(dir.path());
        let lat = def(serde_json::json!({ "name": "lat", "compute": { "aggregation_type": "distribution", "path": "@duration", "include_percentiles": true } }));
        ms.create(lat.clone(), |_| false).unwrap();
        // Its parts' names are its: another metric may not write them.
        assert_eq!(ms.create(def(serde_json::json!({ "name": "lat.count", "compute": { "aggregation_type": "count" } })), |_| false).unwrap_err().0, 409);
        assert_eq!(ms.create(lat.clone(), |_| false).unwrap_err().0, 409);
        // Series written by something else: refused, unless a metric from logs wrote them.
        let other = def(serde_json::json!({ "name": "hits", "compute": { "aggregation_type": "count" } }));
        assert_eq!(ms.create(other.clone(), |n| n == "hits").unwrap_err().0, 409);
        ms.create(other.clone(), |_| false).unwrap();
        assert!(ms.delete("hits").unwrap());
        ms.create(other, |n| n == "hits").unwrap();
        // What it computes stays; its filter and groups change.
        let mut changed = lat.clone();
        changed.compute.aggregation_type = Aggregation::Count;
        changed.compute.path = None;
        assert_eq!(ms.put("lat", changed).unwrap_err().0, 400);
        let mut changed = lat;
        changed.filter.query = "service:web".into();
        changed.group_by = vec![GroupByDef { path: "service".into(), tag_name: None }];
        let k = ms.put("lat", changed.clone()).unwrap().unwrap();
        assert_eq!(k.def, changed);
        let again = LogMetrics::open(dir.path());
        assert_eq!(again.list().iter().map(|k| k.def.name.as_str()).collect::<Vec<_>>(), vec!["lat", "hits"]);
        assert_eq!(lock(&again.compiled).len(), again.builtin().len() + 2, "kiln's own (Error Tracking's) and the two defined");
    }

    #[test]
    fn windows_are_counted_once_settled_and_again_ever_less_often() {
        let mut st = State::default();
        let now = 1_792_324_800_000i64;
        let w0 = window_of((now - 60_000) * 1_000_000);
        let mut lags = [0u32; LAG_BUCKETS];
        lags[kiln_obs::logs::metrics::lag_bucket(800)] = 1;
        let spans = Spans { windows: vec![(w0, w0)], batches: 1, lags, ..Spans::default() };
        // As the server starts, every window of the horizon is taken to need counting.
        st.mark(&spans, now);
        assert_eq!(st.counting.windows_waiting, (HORIZON_MS / 10_000 + 1) as usize);
        assert_eq!(st.counting.settle_ms, SETTLE_MIN_MS, "800 ms late at the most: 2 s");
        st.windows.clear();
        st.mark(&spans, now);
        assert_eq!(st.due(now), Some(w0));
        // A window that has just ended waits for the settle.
        let w1 = window_of(now * 1_000_000) - WINDOW;
        st.mark(&Spans { windows: vec![(w1, w1)], ..Spans::default() }, now);
        st.windows.insert(w0, Win { dirty: false, counted_ms: Some(now) });
        let end1 = (w1 + WINDOW) / 1_000_000;
        assert_eq!(st.due(end1 + 1_000), None);
        assert_eq!(st.due(end1 + 2_000), Some(w1));
        // Counted 60 s after it ended, then dirty again: 30 s later at the earliest.
        st.windows.clear();
        let end0 = (w0 + WINDOW) / 1_000_000;
        st.windows.insert(w0, Win { dirty: true, counted_ms: Some(end0 + 60_000) });
        assert_eq!(st.due(end0 + 89_999), None);
        assert_eq!(st.due(end0 + 90_000), Some(w0));
        // Past the horizon, given up.
        st.mark(&Spans::default(), end0 + HORIZON_MS + 10_000);
        assert!(st.windows.is_empty());
        assert_eq!(st.counting.windows_expired, 1);
    }

    #[test]
    fn the_settle_is_learned_from_how_late_records_come() {
        use kiln_obs::logs::metrics::lag_bucket;
        let mut l = Lags::default();
        let t = 1_792_324_800_000i64;
        let of = |pairs: &[(i64, u32)]| {
            let mut h = [0u32; LAG_BUCKETS];
            for &(ms, n) in pairs {
                h[lag_bucket(ms)] += n;
            }
            h
        };
        assert_eq!(l.settle_ms(t), SETTLE_MS, "nothing yet");
        // A thousand batches 3 to 4 s late: their 99th percentile (its bucket's bound, within a
        // quarter octave), and a quarter.
        l.note(t, &of(&[(3_000, 500), (3_900, 500)]));
        let s = l.settle_ms(t);
        assert!((4_875..=5_120).contains(&s), "{s}");
        // A backfill now and then (five batches a minute late) holds nothing back.
        l.note(t + 60_000, &of(&[(59_000, 5)]));
        assert_eq!(l.settle_ms(t + 60_000), s);
        // A machine whose records all come 20 s late, a tenth of the batches: waited for.
        l.note(t + 120_000, &of(&[(20_000, 120)]));
        assert!(l.settle_ms(t + 120_000) >= 25_000, "{}", l.settle_ms(t + 120_000));
        assert_eq!(l.settle_ms(t + 7 * 60_000), SETTLE_MS, "forgotten after five minutes");
        // Quick records: two seconds at the least.
        let mut q = Lags::default();
        q.note(t, &of(&[(40, 1_000)]));
        assert_eq!(q.settle_ms(t), SETTLE_MIN_MS);
    }
}
