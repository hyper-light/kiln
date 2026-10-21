//! Metrics from logs (`docs/observe.md` §8.3), Datadog's "generate metrics from ingested logs": a
//! count of the records a search matches, or a distribution of an attribute's values, grouped by
//! facets that become its tags, a point each [`WINDOW`] of the records' time.
//!
//! A window's points are a function of the records a replica holds in it: [`count`] reads them, for
//! every metric at once, in one pass over the blocks the window reaches. The server's replica holds
//! every record, so it alone counts (each series has one writer, OpenTelemetry's single-writer
//! rule); when records come for a window it counted, it counts the window again, whole, and writes
//! its points over the ones before (a correction: at a time the later write wins, `crate::head`).
//! So no record is counted twice and a restart loses none. [`Touched`] says which windows records
//! came in, as the replica applies them.

use std::sync::{Mutex, PoisonError};

use hashbrown::HashTable;

use super::search::{RowScratch, Search};
use super::store::{Facet, LogSnapshot, LogStoreError};
use crate::batch::{Kind, SeriesKey};
use crate::dist::{self, Bucket};
use crate::hash::Keyed;

/// A window of records' time: Datadog's ten seconds ("Datadog generates datapoints for log-based
/// metrics at 10-second intervals").
pub const WINDOW: i64 = 10_000_000_000;
/// Groups a metric keeps in a window at the most: OpenTelemetry's default cardinality limit (2000,
/// the metrics SDK's). The records of groups past it are counted in one group, tagged
/// [`OVERFLOW_TAG`]`:true`, as the SDK's overflow attribute is.
pub const GROUPS: usize = 2_000;
pub const OVERFLOW_TAG: &str = "otel.metric.overflow";
/// A tag's value at the most, in characters (Datadog's tags' 200); a longer one is cut.
pub const TAG_CHARS: usize = 200;
/// Facets a metric groups by at the most: an aggregation's ([`super::store::MAX_BY`]).
pub const MAX_BY: usize = super::store::MAX_BY;
/// The value of a facet a record does not have (or has empty), as log analytics says it.
pub const NONE: &str = "N/A";

/// What a metric measures of the records it matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compute {
    /// The records.
    Count,
    /// An attribute's numeric values (a record without one, or not a number, is left out): their
    /// `.count`, `.sum`, `.min` and `.max`, and with `percentiles` their buckets (`.dist`,
    /// `crate::dist`), so a query reads any percentile of them.
    Distribution { path: String, percentiles: bool },
    /// A span's duration (its `duration`, ns) in seconds, as Datadog's trace metrics have it: a
    /// distribution as [`Compute::Distribution`] is.
    Duration { percentiles: bool },
}

/// A metric from logs, compiled.
#[derive(Debug, Clone)]
pub struct LogMetric {
    pub name: String,
    pub search: Search,
    /// Each facet grouped by, and the tag it becomes.
    pub by: Vec<(Facet, String)>,
    pub compute: Compute,
    /// Windows that begin before this (ns) are not counted by this definition: by the one it
    /// replaced, or none.
    pub since: i64,
}

impl LogMetric {
    /// The series' metric names it writes, and their kinds.
    pub fn names(&self) -> Vec<(String, Kind)> {
        match &self.compute {
            Compute::Count => vec![(self.name.clone(), Kind::Count)],
            Compute::Distribution { percentiles, .. } | Compute::Duration { percentiles } => {
                let mut out: Vec<(String, Kind)> = [(".count", Kind::Count), (".sum", Kind::Count), (".min", Kind::Gauge), (".max", Kind::Gauge)].into_iter().map(|(s, k)| (format!("{}{s}", self.name), k)).collect();
                if *percentiles {
                    out.push((format!("{}{}", self.name, dist::SUFFIX), Kind::Count));
                }
                out
            }
        }
    }

    /// Whether its series can be written: its names, and its tags (none twice, none of kiln's).
    pub fn check(&self) -> Result<(), String> {
        if self.by.len() > MAX_BY {
            return Err(format!("{MAX_BY} attributes to group by at the most"));
        }
        let mut tags: Vec<(&str, &str)> = Vec::with_capacity(self.by.len() + 1);
        for (_, t) in &self.by {
            if t == dist::BUCKET_TAG || t == OVERFLOW_TAG {
                return Err(format!("{t}: a tag kiln writes itself"));
            }
            tags.push((t.as_str(), "v"));
        }
        if let Compute::Distribution { path, .. } = &self.compute
            && path.trim_start_matches('@').is_empty()
        {
            return Err("a distribution's attribute: @name".into());
        }
        for (name, kind) in self.names() {
            SeriesKey::new(&name, tags.iter().copied(), kind).map_err(|e| format!("{name}: {e}"))?;
        }
        if let Compute::Distribution { percentiles: true, .. } | Compute::Duration { percentiles: true } = &self.compute {
            let with: Vec<(&str, &str)> = tags.iter().copied().chain([(dist::BUCKET_TAG, "5:p0")]).collect();
            SeriesKey::new(&format!("{}{}", self.name, dist::SUFFIX), with, Kind::Count).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// The start of the window `ts` (ns) falls in.
pub fn window_of(ts: i64) -> i64 {
    ts - ts.rem_euclid(WINDOW)
}

/// A group's running values.
#[derive(Debug)]
struct Acc {
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
    /// A distribution's buckets at `dist::SCALE`, by `Bucket::order`.
    buckets: Vec<(Bucket, u64)>,
}

impl Acc {
    fn new() -> Self {
        Self { n: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, buckets: Vec::new() }
    }

    fn take(&mut self, v: Option<f64>, percentiles: bool) {
        self.n += 1;
        let Some(v) = v else { return };
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        if percentiles && let Some(b) = dist::bucket_of(v, dist::SCALE) {
            let o = b.order();
            match self.buckets.binary_search_by(|(x, _)| x.order().cmp(&o)) {
                Ok(i) => {
                    if let Some((_, n)) = self.buckets.get_mut(i) {
                        *n += 1;
                    }
                }
                Err(i) => self.buckets.insert(i, (b, 1)),
            }
        }
    }
}

/// A metric's groups in a window: each one's facet values kept once (a new group allocates; a
/// record of a group already found, nothing).
struct Groups {
    hash: Keyed,
    groups: Vec<(Vec<String>, Acc)>,
    by_key: HashTable<u32>,
    overflow: Option<Acc>,
}

fn values_hash<'s>(hash: &Keyed, values: impl Iterator<Item = &'s str>) -> u64 {
    values.fold(0, |h, v| hash.word(h ^ hash.bytes(v.as_bytes())))
}

impl Groups {
    fn new(hash: Keyed) -> Self {
        Self { hash, groups: Vec::new(), by_key: HashTable::new(), overflow: None }
    }

    /// The group of `values`, made if new; the overflow's once [`GROUPS`] are made.
    fn acc(&mut self, values: &[&str]) -> &mut Acc {
        let Self { hash, groups, by_key, overflow } = self;
        let h = values_hash(hash, values.iter().copied());
        let found = by_key.find(h, |&i| groups.get(i as usize).is_some_and(|(k, _)| k.iter().map(String::as_str).eq(values.iter().copied()))).copied();
        let i = match found {
            Some(i) => i as usize,
            None if groups.len() >= GROUPS => return overflow.get_or_insert_with(Acc::new),
            None => {
                let i = groups.len();
                groups.push((values.iter().map(|v| (*v).to_owned()).collect(), Acc::new()));
                #[allow(clippy::cast_possible_truncation)]
                by_key.insert_unique(h, i as u32, |&j| groups.get(j as usize).map_or(0, |(k, _)| values_hash(hash, k.iter().map(String::as_str))));
                i
            }
        };
        // `i` is a group's: the overflow's only were it not.
        groups.get_mut(i).map(|(_, a)| a).unwrap_or_else(|| overflow.get_or_insert_with(Acc::new))
    }
}

/// What counting a window found of each metric.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Records the metric took (a distribution's: those with a value).
    pub records: u64,
    /// Its groups (the overflow's among them).
    pub groups: usize,
    /// Records counted in the overflow group.
    pub overflowed: u64,
}

/// A window counted.
#[derive(Debug, Default)]
pub struct Counted {
    /// Every series of every metric counted, with its point at the window's start (ms).
    pub series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    /// Each metric's, in the order given (those the window is before are left at nothing).
    pub tallies: Vec<Tally>,
    pub rows_scanned: u64,
}

/// A tag's value as a metric keeps it: [`NONE`] for none, cut to [`TAG_CHARS`] characters.
pub(crate) fn tag_value(v: Option<&str>) -> &str {
    match v {
        None | Some("") => NONE,
        Some(v) if v.len() <= TAG_CHARS => v,
        Some(v) => v.char_indices().nth(TAG_CHARS).and_then(|(i, _)| v.get(..i)).unwrap_or(v),
    }
}

/// A measure's value: the attribute, a number (finite).
fn number(v: Option<&str>) -> Option<f64> {
    v.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())
}

/// Every metric's points for the window that starts at `window` (ns, a multiple of [`WINDOW`]):
/// the records `logs` holds in it, read once.
pub fn count(logs: &LogSnapshot, metrics: &[LogMetric], window: i64) -> Result<Counted, LogStoreError> {
    let mut out = Counted { tallies: vec![Tally::default(); metrics.len()], ..Counted::default() };
    let live: Vec<usize> = metrics.iter().enumerate().filter(|(_, m)| m.since <= window).map(|(i, _)| i).collect();
    if live.is_empty() {
        return Ok(out);
    }
    let hash = Keyed::new();
    let mut groups: Vec<Groups> = metrics.iter().map(|_| Groups::new(hash)).collect();
    let mut scratch = RowScratch::default();
    let mut rows = 0u64;
    logs.each_record(window, window.saturating_add(WINDOW - 1), |r| {
        rows += 1;
        for &i in &live {
            let (Some(m), Some(g)) = (metrics.get(i), groups.get_mut(i)) else { continue };
            if !m.search.row(r, &mut scratch) {
                continue;
            }
            let (v, percentiles) = match &m.compute {
                Compute::Count => (None, false),
                Compute::Distribution { path, percentiles } => match number(r.attr(path.trim_start_matches('@'))) {
                    Some(v) => (Some(v), *percentiles),
                    None => continue,
                },
                Compute::Duration { percentiles } => match number(r.attr("duration")) {
                    Some(ns) => (Some(ns / 1e9), *percentiles),
                    None => continue,
                },
            };
            let n = m.by.len().min(MAX_BY);
            let mut values: [&str; MAX_BY] = [""; MAX_BY];
            for (slot, (f, _)) in values.iter_mut().zip(&m.by) {
                *slot = tag_value(f.of(r));
            }
            g.acc(values.get(..n).unwrap_or_default()).take(v, percentiles);
        }
    })?;
    out.rows_scanned = rows;
    let ms = window.div_euclid(1_000_000);
    for &i in &live {
        let (Some(m), Some(g), Some(tally)) = (metrics.get(i), groups.get(i), out.tallies.get_mut(i)) else { continue };
        let named = g.groups.iter().map(|(k, a)| (Some(k), a));
        for (key, acc) in named.chain(g.overflow.as_ref().map(|a| (None, a))) {
            tally.records += acc.n;
            tally.groups += 1;
            let tags: Vec<(&str, &str)> = match key {
                Some(k) => m.by.iter().map(|(_, t)| t.as_str()).zip(k.iter().map(String::as_str)).collect(),
                None => {
                    tally.overflowed += acc.n;
                    vec![(OVERFLOW_TAG, "true")]
                }
            };
            points(m, &tags, acc, ms, &mut out.series);
        }
    }
    Ok(out)
}

/// A group's points at `ms`: a count's, or a distribution's parts. A key that cannot be made (the
/// definition was checked, so none) is left out.
fn points(m: &LogMetric, tags: &[(&str, &str)], acc: &Acc, ms: i64, out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>) {
    let mut put = |name: &str, tags: &[(&str, &str)], kind: Kind, v: f64| {
        if let Ok(k) = SeriesKey::new(name, tags.iter().copied(), kind) {
            out.push((k, vec![(ms, v)]));
        }
    };
    #[allow(clippy::cast_precision_loss)]
    match &m.compute {
        Compute::Count => put(&m.name, tags, Kind::Count, acc.n as f64),
        Compute::Distribution { percentiles, .. } | Compute::Duration { percentiles } => {
            put(&format!("{}.count", m.name), tags, Kind::Count, acc.n as f64);
            put(&format!("{}.sum", m.name), tags, Kind::Count, acc.sum);
            put(&format!("{}.min", m.name), tags, Kind::Gauge, acc.min);
            put(&format!("{}.max", m.name), tags, Kind::Gauge, acc.max);
            if *percentiles {
                let name = format!("{}{}", m.name, dist::SUFFIX);
                let bucket_tags: Vec<String> = acc.buckets.iter().map(|(b, _)| dist::tag_value(dist::SCALE, *b)).collect();
                let mut with: Vec<(&str, &str)> = tags.to_vec();
                for ((_, n), tag) in acc.buckets.iter().zip(&bucket_tags) {
                    with.truncate(tags.len());
                    with.push((dist::BUCKET_TAG, tag.as_str()));
                    put(&name, &with, Kind::Count, *n as f64);
                }
            }
        }
    }
}

/// How long after a window ends records still count in it: Datadog's twenty minutes ("aggregation
/// only considers logs ingested with a timestamp within the past 20 minutes").
pub const HORIZON: i64 = 20 * 60 * 1_000_000_000;
/// How far ahead of now a record's window is noted (a clock ahead): the store keeps none further.
const AHEAD: i64 = 10 * 60 * 1_000_000_000;
/// How late a batch's newest record may come and still be taken into how late records come: past
/// it, a backlog (a machine back from sleep), not the usual wait.
pub const LAG_CAP_MS: i64 = 60_000;
/// A batch's lateness kept in buckets of a quarter octave of milliseconds, up to 2¹⁶ ms (65 s).
pub const LAG_BUCKETS: usize = 65;
/// Spans [`Touched`] holds before it merges them.
const SPANS: usize = 64;

/// The bucket of a lateness of `ms`: 0 for a millisecond or less, then four an octave.
pub fn lag_bucket(ms: i64) -> usize {
    if ms <= 1 {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let i = ((ms as f64).log2() * 4.0).floor() as usize + 1;
    i.min(LAG_BUCKETS - 1)
}

/// The most lateness bucket `i` holds (whole ms): bucket `i > 0` holds `[2^((i-1)/4), 2^(i/4))`.
pub fn lag_bound_ms(i: usize) -> i64 {
    if i == 0 {
        return 1;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let ms = (i as f64 / 4.0).exp2().ceil() as i64 - 1;
    ms.max(1)
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// The windows a batch's records fall in, read as the batch is: each distinct one within the
/// horizon (a batch's rows mostly fall in one or two; however old some of them are, at most the
/// horizon's windows), and its newest record's time.
#[derive(Debug)]
pub struct BatchWindows {
    from: i64,
    to: i64,
    windows: Vec<i64>,
    last: i64,
    newest: i64,
}

impl Default for BatchWindows {
    fn default() -> Self {
        Self::at(now_ns())
    }
}

impl BatchWindows {
    /// For a batch applied at `now` (ns).
    pub fn at(now: i64) -> Self {
        Self { from: window_of(now.saturating_sub(HORIZON)), to: now.saturating_add(AHEAD), windows: Vec::new(), last: i64::MIN, newest: i64::MIN }
    }

    pub fn add(&mut self, ts: i64) {
        self.newest = self.newest.max(ts);
        if ts < self.from || ts > self.to {
            return;
        }
        let w = window_of(ts);
        if w != self.last {
            self.last = w;
            if !self.windows.contains(&w) {
                self.windows.push(w);
            }
        }
    }
}

/// The windows records came in, as a replica applies them, for whoever counts them again.
#[derive(Debug, Default)]
pub struct Touched {
    inner: Mutex<Spans>,
    /// The newest entry known (its index): one applied again, as a replica opens, says which
    /// windows it reaches but nothing of how late records come.
    newest: std::sync::atomic::AtomicU64,
}

/// What [`Touched`] holds, taken whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spans {
    /// Windows records came in: each span from its first window's start to its last's (ns),
    /// merged where they meet.
    pub windows: Vec<(i64, i64)>,
    /// Every window may have changed (the replica's records replaced whole: an image installed).
    pub all: bool,
    /// How late each batch's newest record came ([`lag_bucket`]'s counts), of those within
    /// [`LAG_CAP_MS`]; and the batches later than that.
    pub lags: [u32; LAG_BUCKETS],
    pub batches: u64,
    pub backlogs: u64,
}

impl Default for Spans {
    fn default() -> Self {
        Self { windows: Vec::new(), all: false, lags: [0; LAG_BUCKETS], batches: 0, backlogs: 0 }
    }
}

impl Touched {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Spans> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Entries through `index` were applied before (the log held them as its replica opened).
    pub fn seen(&self, index: u64) {
        self.newest.fetch_max(index, std::sync::atomic::Ordering::AcqRel);
    }

    /// Another group's entries from here on: their indexes start again.
    pub fn reset(&self) {
        self.newest.store(0, std::sync::atomic::Ordering::Release);
    }

    /// A batch of entry `index` applied, now (its records whole, whichever items held them): the
    /// windows its records fall in, and, the entry new, how late its newest came.
    pub fn note(&self, b: &BatchWindows, index: u64) {
        if b.newest == i64::MIN {
            return;
        }
        let fresh = self.newest.fetch_max(index, std::sync::atomic::Ordering::AcqRel) < index;
        let lag = (now_ns().saturating_sub(b.newest) / 1_000_000).max(0);
        let mut s = self.lock();
        s.windows.extend(b.windows.iter().map(|&w| (w, w)));
        if s.windows.len() > SPANS {
            merge(&mut s.windows);
        }
        if !fresh {
            return;
        }
        s.batches += 1;
        if lag <= LAG_CAP_MS {
            if let Some(n) = s.lags.get_mut(lag_bucket(lag)) {
                *n = n.saturating_add(1);
            }
        } else {
            s.backlogs += 1;
        }
    }

    /// Every window may have changed.
    pub fn everything(&self) {
        self.lock().all = true;
    }

    /// What was noted since it was last taken.
    pub fn take(&self) -> Spans {
        let mut s = std::mem::take(&mut *self.lock());
        merge(&mut s.windows);
        s
    }
}

/// Spans sorted, and those that overlap or meet made one.
fn merge(spans: &mut Vec<(i64, i64)>) {
    spans.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for &(a, b) in spans.iter() {
        match out.last_mut() {
            Some(last) if a <= last.1.saturating_add(WINDOW) => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    *spans = out;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{Record, put_row};
    use crate::logs::store::LogStore;
    use crate::testkit::Rng;

    const T0: i64 = 1_792_324_800_000_000_000;

    fn store(records: &[Record]) -> LogStore {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in records {
            put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        let mut s = LogStore::default();
        #[allow(clippy::cast_possible_truncation)]
        s.append(raw, records.len() as u32).unwrap();
        s
    }

    fn rec(ts: i64, service: &str, status: &str, ms: Option<f64>) -> Record {
        let mut attrs = vec![("http.status_code".to_owned(), status.to_owned())];
        if let Some(ms) = ms {
            attrs.push(("duration".to_owned(), ms.to_string()));
        }
        Record { ts, severity: if status.starts_with('5') { 17 } else { 9 }, service: service.into(), host: "h".into(), source: "nginx".into(), message: format!("GET / {status}"), attrs, ..Record::default() }
    }

    fn metric(name: &str, query: &str, by: &[&str], compute: Compute) -> LogMetric {
        LogMetric { name: name.into(), search: crate::logs::search::parse(query).unwrap(), by: by.iter().map(|b| (Facet::parse(b), b.trim_start_matches('@').to_owned())).collect(), compute, since: 0 }
    }

    fn value(c: &Counted, name: &str, tags: &[(&str, &str)]) -> Option<f64> {
        let want = SeriesKey::new(name, tags.iter().copied(), Kind::Gauge).unwrap().key;
        let (_, p) = c.series.iter().find(|(k, _)| k.key == want)?;
        p.first().map(|p| p.1)
    }

    /// Counted from the records, as a search and a sum over them would.
    #[test]
    fn a_window_s_counts_and_distributions_are_its_records_grouped() {
        let mut rng = Rng::new(7);
        let mut records = Vec::new();
        let mut want_count = std::collections::BTreeMap::new();
        let mut durations: std::collections::BTreeMap<String, Vec<f64>> = std::collections::BTreeMap::new();
        for i in 0..5_000 {
            // Some before the window and after it: not its.
            let ts = T0 - WINDOW / 2 + (rng.below(2 * WINDOW as u64) as i64);
            let service = ["web", "api", "db"][rng.below(3) as usize];
            let status = ["200", "404", "502"][rng.below(3) as usize];
            let ms = (i % 7 != 0).then(|| 1.0 + rng.below(5_000) as f64 / 10.0);
            if (T0..T0 + WINDOW).contains(&ts) && service != "db" {
                *want_count.entry((service.to_owned(), status.to_owned())).or_insert(0u64) += 1;
                if let Some(ms) = ms {
                    durations.entry(service.to_owned()).or_default().push(ms);
                }
            }
            records.push(rec(ts, service, status, ms));
        }
        records.sort_by_key(|r| r.ts);
        let s = store(&records);
        let metrics = [
            metric("hits", "-service:db", &["service", "@http.status_code"], Compute::Count),
            metric("latency", "-service:db", &["service"], Compute::Distribution { path: "@duration".into(), percentiles: true }),
            metric("later", "*", &[], Compute::Count),
        ];
        let mut later = metrics[2].clone();
        later.since = T0 + WINDOW;
        let metrics = [metrics[0].clone(), metrics[1].clone(), later];
        let c = count(&s.snapshot(), &metrics, T0).unwrap();
        for ((service, status), n) in &want_count {
            assert_eq!(value(&c, "hits", &[("service", service), ("http.status_code", status)]), Some(*n as f64), "{service} {status}");
        }
        assert_eq!(c.tallies[0].records, want_count.values().sum::<u64>());
        assert_eq!(c.tallies[0].groups, want_count.len());
        assert_eq!(c.tallies[2], Tally::default(), "a window before a definition is not its");
        for (service, vs) in &durations {
            let tags = [("service", service.as_str())];
            assert_eq!(value(&c, "latency.count", &tags), Some(vs.len() as f64));
            let sum: f64 = vs.iter().sum();
            assert!((value(&c, "latency.sum", &tags).unwrap() - sum).abs() < 1e-6 * sum);
            assert_eq!(value(&c, "latency.min", &tags), vs.iter().copied().reduce(f64::min));
            assert_eq!(value(&c, "latency.max", &tags), vs.iter().copied().reduce(f64::max));
            // The buckets hold every value once, each where `dist` puts it.
            let mut want: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
            for v in vs {
                *want.entry(dist::tag_value(dist::SCALE, dist::bucket_of(*v, dist::SCALE).unwrap())).or_default() += 1.0;
            }
            for (b, n) in &want {
                assert_eq!(value(&c, "latency.dist", &[("service", service.as_str()), (dist::BUCKET_TAG, b.as_str())]), Some(*n), "{service} {b}");
            }
            let all = c.series.iter().filter(|(k, _)| k.key.windows(12).any(|w| w == b"latency.dist") && k.key.windows(service.len()).any(|w| w == service.as_bytes())).count();
            assert_eq!(all, want.len(), "{service}: no bucket besides");
        }
        assert!(c.series.iter().all(|(_, p)| p == &[(T0 / 1_000_000, p[0].1)]), "each at the window's start");
    }

    #[test]
    fn groups_past_the_limit_are_counted_in_the_overflow() {
        let records: Vec<Record> = (0..(GROUPS as i64 + 500)).map(|i| Record { ts: T0 + i, service: format!("s{i}"), ..Record::default() }).collect();
        let s = store(&records);
        let c = count(&s.snapshot(), &[metric("by_service", "*", &["service"], Compute::Count)], T0).unwrap();
        assert_eq!(c.tallies[0], Tally { records: GROUPS as u64 + 500, groups: GROUPS + 1, overflowed: 500 });
        assert_eq!(value(&c, "by_service", &[(OVERFLOW_TAG, "true")]), Some(500.0));
        assert_eq!(value(&c, "by_service", &[("service", "s0")]), Some(1.0));
    }

    #[test]
    fn missing_and_long_values_are_tagged_as_a_metric_keeps_them() {
        let long = "é".repeat(TAG_CHARS + 10);
        let records = vec![Record { ts: T0, service: String::new(), ..Record::default() }, Record { ts: T0 + 1, service: long.clone(), ..Record::default() }];
        let c = count(&store(&records).snapshot(), &[metric("m", "*", &["service", "@missing"], Compute::Count)], T0).unwrap();
        assert_eq!(value(&c, "m", &[("service", NONE), ("missing", NONE)]), Some(1.0));
        let cut: String = long.chars().take(TAG_CHARS).collect();
        assert_eq!(value(&c, "m", &[("service", cut.as_str()), ("missing", NONE)]), Some(1.0));
    }

    #[test]
    fn a_definition_s_tags_and_names_are_checked() {
        assert!(metric("ok.name", "*", &["service"], Compute::Count).check().is_ok());
        assert!(metric("m", "*", &["dist_bucket"], Compute::Count).check().is_err());
        let twice = LogMetric { by: vec![(Facet::Service, "s".into()), (Facet::Host, "s".into())], ..metric("m", "*", &[], Compute::Count) };
        assert!(twice.check().is_err());
        assert!(metric("m", "*", &[], Compute::Distribution { path: "@".into(), percentiles: false }).check().is_err());
        assert_eq!(metric("lat", "*", &[], Compute::Distribution { path: "@d".into(), percentiles: true }).names().len(), 5);
    }

    #[test]
    fn touched_windows_are_each_batch_s_own_merged_and_taken_once() {
        let t = Touched::new();
        let now = now_ns();
        let w = window_of(now);
        let batch = |ts: &[i64]| {
            let mut b = BatchWindows::at(now);
            for &x in ts {
                b.add(x);
            }
            b
        };
        // Old and new records in one batch: their windows, not every one between.
        t.note(&batch(&[now, now - 1, now - 5 * WINDOW, now - 5 * WINDOW + 1, now - 25 * 60 * 1_000_000_000]), 1);
        t.note(&batch(&[now - WINDOW]), 2);
        let s = t.take();
        assert_eq!(s.windows, vec![(w - 5 * WINDOW, w - 5 * WINDOW), (w - WINDOW, w)], "the record 25 minutes back is past the horizon");
        assert_eq!((s.batches, s.backlogs), (2, 0));
        assert_eq!(s.lags.iter().sum::<u32>(), 2);
        assert_eq!(t.take(), Spans::default());
        for i in 0..100 {
            t.note(&batch(&[now - i * 2 * WINDOW]), 3 + i as u64);
        }
        assert_eq!(t.take().windows.len(), 61, "apart, they stay apart (those within the horizon)");
        // Entries applied again as a replica opens: their windows, not their lateness.
        t.seen(200);
        t.note(&batch(&[now - 60 * 1_000_000_000]), 150);
        let s = t.take();
        assert_eq!((s.windows.len(), s.batches, s.lags.iter().sum::<u32>()), (1, 0, 0));
        t.note(&batch(&[now]), 201);
        assert_eq!(t.take().batches, 1);
    }

    #[test]
    fn a_lateness_falls_in_its_bucket() {
        for ms in [0, 1, 2, 3, 10, 999, 1_000, 1_001, 2_500, 30_000, 60_000] {
            let i = lag_bucket(ms);
            assert!(ms <= lag_bound_ms(i), "{ms} in bucket {i}, of up to {}", lag_bound_ms(i));
            assert!(i == 0 || ms > lag_bound_ms(i - 1), "{ms} in bucket {i}, past {}", lag_bound_ms(i - 1));
        }
        assert_eq!(lag_bucket(i64::MAX), LAG_BUCKETS - 1);
    }
}
