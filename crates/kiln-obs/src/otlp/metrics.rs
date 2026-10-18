//! OTLP metrics into series (`docs/observe.md` §3, §8): an `ExportMetricsServiceRequest`
//! (opentelemetry-proto `collector/metrics/v1`) read in place and mapped to kiln's model.
//!
//! - Tags: the point's attributes over its resource's, each value as text; then Datadog's unified
//!   service tags where absent (`service` from `service.name`, `env` from
//!   `deployment.environment[.name]`, `host` from `host.name`), so OTel and kiln's own series
//!   filter alike.
//! - Gauge → gauge. Sum: monotonic and cumulative → counter; delta → count; non-monotonic
//!   cumulative (an up-down counter) → gauge.
//! - Histogram (explicit bounds) → `<name>.count`, `<name>.sum`, `<name>.min`, `<name>.max` and
//!   `<name>.bucket{le:…}` (cumulative counts, Prometheus's convention), counters or counts by
//!   temporality. ExponentialHistogram → the same `.count`, `.sum`, `.min`, `.max`, and its
//!   quantiles `<name>.p50|p90|p95|p99` read from the buckets at ingest (relative error at most
//!   (γ−1)/(γ+1), γ = 2^(2^−scale)). Summary → `.count`, `.sum`, `<name>.quantile{quantile:…}`.
//! - A point flagged `NO_RECORDED_VALUE` (flags bit 0) is skipped; a point without a time takes
//!   the time it was received.

use std::borrow::Cow;
use std::sync::{Mutex, PoisonError};

use hashbrown::HashTable;

use super::pb::{Fields, PbError, doubles, fixed64s, varints};
use crate::batch::{Kind, MAX_NAME_BYTES, MAX_TAGS, SeriesKey, put_canonical_head, put_canonical_tag};
use crate::hash::Keyed;

/// OTLP's flag on a point that carries no value (`DataPointFlags.FLAG_NO_RECORDED_VALUE_MASK`).
const NO_RECORDED_VALUE: u64 = 1;
/// How deep an attribute's nested value is rendered into a tag; past it, "…". A tag is a filter
/// and a label: a value nested deeper is not read as one.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OtlpError {
    #[error("the request is malformed: {0}")]
    Pb(#[from] PbError),
}

/// What was dropped, and why.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    /// Points flagged as carrying no value.
    pub no_value: u64,
    /// Points whose series could not be made (an empty name, a name too long, ...).
    pub invalid: u64,
    /// Tags past [`MAX_TAGS`] on a point, dropped in key order.
    pub tags: u64,
}

/// A request's series and their samples.
#[derive(Debug, Default)]
pub struct Decoded {
    pub series: Vec<(SeriesKey, Vec<(i64, f64)>)>,
    pub points: u64,
    pub dropped: Dropped,
}

/// Shards of [`Keys`]: a receiver decodes on several threads at once.
const SHARDS: usize = 16;
/// Keys a receiver keeps at the most, and their bytes: many times a node's series. Past either, a
/// shard starts again, its keys made again as their series come.
const KEYS_MAX: usize = 1 << 20;
const KEY_BYTES_MAX: usize = 64 << 20;

/// The series keys a receiver has made, by their canonical bytes and kind. An exporter sends the
/// same series every export: a key is made (checked, sorted, fingerprinted) the first time its
/// series comes and found after, its bytes shared with every write that holds it.
pub struct Keys {
    hash: Keyed,
    shards: Vec<Mutex<Shard>>,
}

#[derive(Default)]
struct Shard {
    table: HashTable<SeriesKey>,
    bytes: usize,
}

impl Default for Keys {
    fn default() -> Self {
        Self::new()
    }
}

fn key_hash(h: &Keyed, key: &[u8], kind: Kind) -> u64 {
    h.word(h.bytes(key) ^ u64::from(kind as u8))
}

impl Keys {
    pub fn new() -> Self {
        Self { hash: Keyed::new(), shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect() }
    }

    /// The key of `key`'s bytes and `kind` (`h`, their hash): found, or made by `make` (outside
    /// the shard's lock) and kept. None when `make` finds it is no key.
    fn get_or_make(&self, h: u64, key: &[u8], kind: Kind, make: impl FnOnce() -> Option<SeriesKey>) -> Option<SeriesKey> {
        let shard = self.shards.get((h >> 60) as usize % SHARDS)?;
        if let Some(k) = shard.lock().unwrap_or_else(PoisonError::into_inner).table.find(h, |k| k.kind == kind && *k.key == *key) {
            return Some(k.clone());
        }
        let made = make()?;
        let mut s = shard.lock().unwrap_or_else(PoisonError::into_inner);
        if s.table.len() >= KEYS_MAX / SHARDS || s.bytes + made.key.len() > KEY_BYTES_MAX / SHARDS {
            s.table.clear();
            s.bytes = 0;
        }
        // Another thread may have made it meanwhile: one is kept.
        if s.table.find(h, |k| k.kind == kind && *k.key == *key).is_none() {
            s.bytes += made.key.len();
            let hash = self.hash;
            s.table.insert_unique(h, made.clone(), move |k| key_hash(&hash, &k.key, k.kind));
        }
        Some(made)
    }

    /// Keys kept now.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).table.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A tag as the request holds it: its key, and its value as text, borrowed where the request has
/// it as text (most attributes): nothing is copied to read one.
type Tag<'a> = (&'a str, Cow<'a, str>);

/// Datadog's unified service tags, each from the first of its OpenTelemetry sources a point has.
const UNIFIED: [(&str, &[&str]); 3] = [("service", &["service.name"]), ("env", &["deployment.environment.name", "deployment.environment"]), ("host", &["host.name"])];

/// Builds series as points arrive, reusing what it builds them with: a point allocates nothing
/// but its sample's room, and its series' key once in the receiver's life ([`Keys`]).
struct Sink<'a, 'k> {
    out: Decoded,
    keys: &'k Keys,
    /// This request's series by their keys' hashes: their places in `out.series`.
    found: HashTable<usize>,
    /// A point's attributes as read; its tags, merged and sorted; its canonical key.
    attrs: Vec<Tag<'a>>,
    tags: Vec<Tag<'a>>,
    key: Vec<u8>,
    /// A histogram point's counts and bounds, a summary point's quantiles.
    counts: Vec<u64>,
    bounds: Vec<f64>,
}

impl<'a> Sink<'a, '_> {
    /// A point's attribute (a `KeyValue`), for [`Sink::merge`].
    fn attr(&mut self, kv: &'a [u8]) -> Result<(), OtlpError> {
        if let Some(t) = key_value_ref(kv)? {
            self.attrs.push(t);
        }
        Ok(())
    }

    /// The point's tags: its attributes (taken) over the resource's, then the unified service
    /// tags, sorted by key and value (a key twice makes the point no series: [`Sink::point`]).
    fn merge(&mut self, resource: &[Tag<'a>]) {
        self.tags.clear();
        for (k, v) in resource {
            if !self.attrs.iter().any(|(pk, _)| pk == k) {
                self.tags.push((k, v.clone()));
            }
        }
        self.tags.append(&mut self.attrs);
        for (alias, sources) in UNIFIED {
            if self.tags.iter().any(|(k, _)| *k == alias) {
                continue;
            }
            if let Some(v) = sources.iter().find_map(|s| self.tags.iter().find(|(k, _)| k == s).map(|(_, v)| v.clone())) {
                self.tags.push((alias, v));
            }
        }
        self.tags.sort_unstable_by(|a, b| a.0.cmp(b.0).then_with(|| a.1.cmp(&b.1)));
    }

    /// A sample of `metric` with the merged tags and `extra` (over a tag of its key): the first
    /// [`MAX_TAGS`] of them in key order, as [`SeriesKey::new`] would have them.
    fn point(&mut self, metric: &str, extra: Option<(&str, &str)>, kind: Kind, ts: i64, v: f64) {
        let Self { out, keys, found, tags, key, .. } = self;
        let ek = extra.map(|(k, _)| k);
        let total = tags.iter().filter(|(k, _)| Some(*k) != ek).count() + usize::from(extra.is_some());
        let kept = total.min(MAX_TAGS);
        out.dropped.tags += (total - kept) as u64;
        let merged = || Merged { tags, extra, at: 0, extra_out: extra.is_none() }.take(kept);
        // The canonical key, written as SeriesKey::new writes it; a key twice is no series.
        put_canonical_head(key, metric, kept);
        let mut prev: Option<&str> = None;
        for (k, v) in merged() {
            if prev == Some(k) {
                out.dropped.invalid += 1;
                return;
            }
            prev = Some(k);
            put_canonical_tag(key, k, v);
        }
        let h = key_hash(&keys.hash, key, kind);
        let series = &mut out.series;
        if let Some(&i) = found.find(h, |&i| series.get(i).is_some_and(|(k, _)| k.kind == kind && *k.key == key[..])) {
            if let Some(s) = series.get_mut(i) {
                s.1.push((ts, v));
                out.points += 1;
            }
            return;
        }
        let Some(made) = keys.get_or_make(h, key, kind, || SeriesKey::new(metric, merged(), kind).ok()) else {
            out.dropped.invalid += 1;
            return;
        };
        debug_assert_eq!(*made.key, key[..], "the canonical key as SeriesKey makes it");
        let hash = keys.hash;
        found.insert_unique(h, series.len(), |&i| series.get(i).map_or(0, |(k, _)| key_hash(&hash, &k.key, k.kind)));
        series.push((made, vec![(ts, v)]));
        out.points += 1;
    }
}

/// Tags sorted by key with `extra` in its place, over any of its key.
struct Merged<'t, 'a, 'e> {
    tags: &'t [Tag<'a>],
    extra: Option<(&'e str, &'e str)>,
    at: usize,
    extra_out: bool,
}

impl<'t> Iterator for Merged<'t, '_, 't> {
    type Item = (&'t str, &'t str);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next = self.tags.get(self.at);
            if !self.extra_out
                && let Some((ek, ev)) = self.extra
                && next.is_none_or(|(k, _)| *k >= ek)
            {
                self.extra_out = true;
                return Some((ek, ev));
            }
            let (k, v) = next?;
            self.at += 1;
            if self.extra.is_some_and(|(ek, _)| ek == *k) {
                continue;
            }
            return Some((k, v.as_ref()));
        }
    }
}

/// Decodes an `ExportMetricsServiceRequest`; `now_ms` is the time a point without one takes.
pub fn decode(body: &[u8], now_ms: i64) -> Result<Decoded, OtlpError> {
    decode_with(body, now_ms, &Keys::new())
}

/// [`decode`] with a receiver's [`Keys`]: what a node's receivers decode with, so a series' key is
/// made once in the node's life.
pub fn decode_with(body: &[u8], now_ms: i64, keys: &Keys) -> Result<Decoded, OtlpError> {
    let mut sink = Sink { out: Decoded::default(), keys, found: HashTable::new(), attrs: Vec::new(), tags: Vec::new(), key: Vec::new(), counts: Vec::new(), bounds: Vec::new() };
    for f in Fields::new(body) {
        let (n, w) = f?;
        if n == 1 {
            resource_metrics(w.bytes().unwrap_or(&[]), now_ms, &mut sink)?;
        }
    }
    Ok(sink.out)
}

fn resource_metrics<'a>(msg: &'a [u8], now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let mut resource: Vec<Tag<'a>> = Vec::new();
    let mut scopes: Vec<&[u8]> = Vec::new();
    for f in Fields::new(msg) {
        match f? {
            (1, w) => {
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    if let (1, kv) = f?
                        && let Some(t) = key_value_ref(kv.bytes().unwrap_or(&[]))?
                    {
                        resource.push(t);
                    }
                }
            }
            (2, w) => scopes.push(w.bytes().unwrap_or(&[])),
            _ => {}
        }
    }
    for scope in scopes {
        for f in Fields::new(scope) {
            if let (2, m) = f? {
                metric(m.bytes().unwrap_or(&[]), &resource, now_ms, sink)?;
            }
        }
    }
    Ok(())
}

/// The data of a metric: its points and, for sums and histograms, temporality and monotonicity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Gauge,
    Sum { cumulative: bool, monotonic: bool },
    Histogram { cumulative: bool },
    Exponential { cumulative: bool },
    Summary,
}

/// The series' names a histogram or a summary makes of its metric, made once a metric: `.count`,
/// `.sum`, `.min`, `.max`, and the buckets' (`.bucket`), the quantiles' (`.quantile`), or the
/// exponential histogram's quantiles' (`.p50` ...); and the last bounds' and quantiles' text.
struct Derived {
    count: String,
    sum: String,
    min: String,
    max: String,
    each: String,
    quantiles: [String; 4],
    labels_of: Vec<f64>,
    labels: Vec<String>,
}

impl Derived {
    fn new(name: &str, each: &str) -> Self {
        Self {
            count: format!("{name}.count"),
            sum: format!("{name}.sum"),
            min: format!("{name}.min"),
            max: format!("{name}.max"),
            each: format!("{name}.{each}"),
            quantiles: ["p50", "p90", "p95", "p99"].map(|q| format!("{name}.{q}")),
            labels_of: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// `labels` as the text of `values` as tag values (`le`, `quantile`): made again only when
    /// they differ from the last point's, as a metric's points' bounds rarely do.
    fn relabel(&mut self, values: &[f64]) {
        if self.labels_of.len() != values.len() || self.labels_of.iter().zip(values).any(|(a, b)| a.to_bits() != b.to_bits()) {
            self.labels_of.clear();
            self.labels_of.extend_from_slice(values);
            self.labels.clear();
            self.labels.extend(values.iter().map(|v| format_f64(*v)));
        }
    }
}

fn metric<'a>(msg: &'a [u8], resource: &[Tag<'a>], now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let mut name = "";
    let mut data: Option<(Shape, &[u8])> = None;
    for f in Fields::new(msg) {
        match f? {
            (1, w) => name = w.str().unwrap_or(""),
            (5, w) => data = Some((Shape::Gauge, w.bytes().unwrap_or(&[]))),
            (7, w) => data = Some((Shape::Sum { cumulative: true, monotonic: false }, w.bytes().unwrap_or(&[]))),
            (9, w) => data = Some((Shape::Histogram { cumulative: true }, w.bytes().unwrap_or(&[]))),
            (10, w) => data = Some((Shape::Exponential { cumulative: true }, w.bytes().unwrap_or(&[]))),
            (11, w) => data = Some((Shape::Summary, w.bytes().unwrap_or(&[]))),
            _ => {}
        }
    }
    let Some((mut shape, body)) = data else { return Ok(()) };
    // Temporality (2) and monotonicity (3) first: points follow.
    let mut points: Vec<&[u8]> = Vec::new();
    for f in Fields::new(body) {
        match (f?, &mut shape) {
            ((1, w), _) => points.push(w.bytes().unwrap_or(&[])),
            ((2, w), Shape::Sum { cumulative, .. } | Shape::Histogram { cumulative } | Shape::Exponential { cumulative }) => *cumulative = w.u64() == Some(2),
            ((3, w), Shape::Sum { monotonic, .. }) => *monotonic = w.u64() == Some(1),
            _ => {}
        }
    }
    let count_kind = |cumulative: bool| if cumulative { Kind::Counter } else { Kind::Count };
    let mut derived = match shape {
        Shape::Gauge | Shape::Sum { .. } => None,
        Shape::Histogram { .. } | Shape::Exponential { .. } => Some(Derived::new(name, "bucket")),
        Shape::Summary => Some(Derived::new(name, "quantile")),
    };
    for p in points {
        match (shape, derived.as_mut()) {
            (Shape::Gauge | Shape::Sum { .. }, _) => {
                let kind = match shape {
                    Shape::Sum { cumulative: true, monotonic: true } => Kind::Counter,
                    Shape::Sum { cumulative: false, .. } => Kind::Count,
                    _ => Kind::Gauge,
                };
                number_point(p, name, resource, kind, now_ms, sink)?;
            }
            (Shape::Histogram { cumulative }, Some(d)) => histogram_point(p, d, resource, count_kind(cumulative), now_ms, sink)?,
            (Shape::Exponential { cumulative }, Some(d)) => exponential_point(p, d, resource, count_kind(cumulative), now_ms, sink)?,
            (Shape::Summary, Some(d)) => summary_point(p, d, resource, now_ms, sink)?,
            _ => {}
        }
    }
    Ok(())
}

fn time_ms(ns: u64, now_ms: i64) -> i64 {
    if ns == 0 { now_ms } else { (ns / 1_000_000) as i64 }
}

fn number_point<'a>(msg: &'a [u8], name: &str, resource: &[Tag<'a>], kind: Kind, now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let (mut t, mut v, mut flags) = (0u64, None, 0u64);
    sink.attrs.clear();
    for f in Fields::new(msg) {
        match f? {
            (7, w) => sink.attr(w.bytes().unwrap_or(&[]))?,
            (3, w) => t = w.u64().unwrap_or(0),
            (4, w) => v = w.f64(),
            (6, w) => v = w.i64().map(|i| i as f64),
            (8, w) => flags = w.u64().unwrap_or(0),
            _ => {}
        }
    }
    if flags & NO_RECORDED_VALUE != 0 || v.is_none() {
        sink.out.dropped.no_value += 1;
        return Ok(());
    }
    sink.merge(resource);
    sink.point(name, None, kind, time_ms(t, now_ms), v.unwrap_or(f64::NAN));
    Ok(())
}

fn histogram_point<'a>(msg: &'a [u8], d: &mut Derived, resource: &[Tag<'a>], count_kind: Kind, now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let (mut t, mut flags) = (0u64, 0u64);
    let (mut count, mut sum, mut min, mut max) = (None, None, None, None);
    let (mut counts, mut bounds) = (std::mem::take(&mut sink.counts), std::mem::take(&mut sink.bounds));
    counts.clear();
    bounds.clear();
    sink.attrs.clear();
    for f in Fields::new(msg) {
        match f? {
            (9, w) => sink.attr(w.bytes().unwrap_or(&[]))?,
            (3, w) => t = w.u64().unwrap_or(0),
            (4, w) => count = w.u64(),
            (5, w) => sum = w.f64(),
            (6, w) => fixed64s(&w, &mut counts)?,
            (7, w) => doubles(&w, &mut bounds)?,
            (10, w) => flags = w.u64().unwrap_or(0),
            (11, w) => min = w.f64(),
            (12, w) => max = w.f64(),
            _ => {}
        }
    }
    if flags & NO_RECORDED_VALUE != 0 {
        sink.out.dropped.no_value += 1;
        (sink.counts, sink.bounds) = (counts, bounds);
        return Ok(());
    }
    sink.merge(resource);
    let ts = time_ms(t, now_ms);
    let sum_kind = if count_kind == Kind::Counter { Kind::Counter } else { Kind::Count };
    if let Some(c) = count {
        sink.point(&d.count, None, count_kind, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&d.sum, None, sum_kind, ts, s);
    }
    if let Some(v) = min {
        sink.point(&d.min, None, Kind::Gauge, ts, v);
    }
    if let Some(v) = max {
        sink.point(&d.max, None, Kind::Gauge, ts, v);
    }
    // Bucket i holds (bounds[i-1], bounds[i]]; the last, (bounds[last], +Inf). Cumulative `le`.
    d.relabel(&bounds);
    let mut cum = 0u64;
    for (i, &c) in counts.iter().enumerate() {
        cum = cum.saturating_add(c);
        let le = d.labels.get(i).map_or("+Inf", String::as_str);
        sink.point(&d.each, Some(("le", le)), count_kind, ts, cum as f64);
    }
    (sink.counts, sink.bounds) = (counts, bounds);
    Ok(())
}

fn exponential_point<'a>(msg: &'a [u8], d: &mut Derived, resource: &[Tag<'a>], count_kind: Kind, now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let (mut t, mut flags) = (0u64, 0u64);
    let (mut count, mut sum, mut min, mut max, mut scale, mut zero) = (None, None, None, None, 0i64, 0u64);
    let (mut pos_off, mut pos, mut neg_off, mut neg) = (0i64, Vec::new(), 0i64, Vec::new());
    sink.attrs.clear();
    for f in Fields::new(msg) {
        match f? {
            (1, w) => sink.attr(w.bytes().unwrap_or(&[]))?,
            (3, w) => t = w.u64().unwrap_or(0),
            (4, w) => count = w.u64(),
            (5, w) => sum = w.f64(),
            (6, w) => scale = w.sint().unwrap_or(0),
            (7, w) => zero = w.u64().unwrap_or(0),
            (8, w) => buckets(w.bytes().unwrap_or(&[]), &mut pos_off, &mut pos)?,
            (9, w) => buckets(w.bytes().unwrap_or(&[]), &mut neg_off, &mut neg)?,
            (10, w) => flags = w.u64().unwrap_or(0),
            (12, w) => min = w.f64(),
            (13, w) => max = w.f64(),
            _ => {}
        }
    }
    if flags & NO_RECORDED_VALUE != 0 {
        sink.out.dropped.no_value += 1;
        return Ok(());
    }
    sink.merge(resource);
    let ts = time_ms(t, now_ms);
    let sum_kind = if count_kind == Kind::Counter { Kind::Counter } else { Kind::Count };
    if let Some(c) = count {
        sink.point(&d.count, None, count_kind, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&d.sum, None, sum_kind, ts, s);
    }
    if let Some(v) = min {
        sink.point(&d.min, None, Kind::Gauge, ts, v);
    }
    if let Some(v) = max {
        sink.point(&d.max, None, Kind::Gauge, ts, v);
    }
    let h = ExpHist { scale: scale.clamp(-10, 20) as i32, zero, pos_off, pos: &pos, neg_off, neg: &neg };
    for (name, q) in d.quantiles.iter().zip([0.50, 0.90, 0.95, 0.99]) {
        if let Some(v) = h.quantile(q) {
            sink.point(name, None, Kind::Gauge, ts, v);
        }
    }
    Ok(())
}

fn buckets(msg: &[u8], offset: &mut i64, counts: &mut Vec<u64>) -> Result<(), OtlpError> {
    for f in Fields::new(msg) {
        match f? {
            (1, w) => *offset = w.sint().unwrap_or(0),
            (2, w) => varints(&w, counts)?,
            _ => {}
        }
    }
    Ok(())
}

/// An exponential histogram (OTel's `ExponentialHistogramDataPoint`).
pub struct ExpHist<'a> {
    pub scale: i32,
    pub zero: u64,
    pub pos_off: i64,
    pub pos: &'a [u64],
    pub neg_off: i64,
    pub neg: &'a [u64],
}

impl ExpHist<'_> {
    /// The `q` quantile (0–1): the bucket holding the rank's, at its midpoint in log space
    /// (γ^(i+½), which bounds the relative error by (γ−1)/(γ+1)), negative buckets mirrored.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let total = self.pos.iter().chain(self.neg).fold(self.zero, |a, &c| a.saturating_add(c));
        if total == 0 || !(0.0..=1.0).contains(&q) {
            return None;
        }
        let rank = (q * (total - 1) as f64).floor() as u64;
        let gamma_log2 = (2f64).powi(-self.scale);
        let mid = |i: i64| (((i as f64) + 0.5) * gamma_log2).exp2();
        // Ascending: the largest magnitudes of the negatives first, then zero, then positives.
        let mut seen = 0u64;
        for (j, &c) in self.neg.iter().enumerate().rev() {
            seen = seen.saturating_add(c);
            if rank < seen {
                return Some(-mid(self.neg_off + j as i64));
            }
        }
        seen = seen.saturating_add(self.zero);
        if rank < seen {
            return Some(0.0);
        }
        for (j, &c) in self.pos.iter().enumerate() {
            seen = seen.saturating_add(c);
            if rank < seen {
                return Some(mid(self.pos_off + j as i64));
            }
        }
        None
    }
}

fn summary_point<'a>(msg: &'a [u8], d: &mut Derived, resource: &[Tag<'a>], now_ms: i64, sink: &mut Sink<'a, '_>) -> Result<(), OtlpError> {
    let (mut t, mut flags, mut count, mut sum) = (0u64, 0u64, None, None);
    let mut quantiles: Vec<(f64, f64)> = Vec::new();
    sink.attrs.clear();
    for f in Fields::new(msg) {
        match f? {
            (7, w) => sink.attr(w.bytes().unwrap_or(&[]))?,
            (3, w) => t = w.u64().unwrap_or(0),
            (4, w) => count = w.u64(),
            (5, w) => sum = w.f64(),
            (6, w) => {
                let (mut q, mut v) = (None, None);
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => q = w.f64(),
                        (2, w) => v = w.f64(),
                        _ => {}
                    }
                }
                if let (Some(q), Some(v)) = (q, v) {
                    quantiles.push((q, v));
                }
            }
            (8, w) => flags = w.u64().unwrap_or(0),
            _ => {}
        }
    }
    if flags & NO_RECORDED_VALUE != 0 {
        sink.out.dropped.no_value += 1;
        return Ok(());
    }
    sink.merge(resource);
    let ts = time_ms(t, now_ms);
    if let Some(c) = count {
        sink.point(&d.count, None, Kind::Counter, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&d.sum, None, Kind::Counter, ts, s);
    }
    let mut qs = std::mem::take(&mut sink.bounds);
    qs.clear();
    qs.extend(quantiles.iter().map(|(q, _)| *q));
    d.relabel(&qs);
    sink.bounds = qs;
    for ((_, v), label) in quantiles.iter().zip(&d.labels) {
        sink.point(&d.each, Some(("quantile", label)), Kind::Gauge, ts, *v);
    }
    Ok(())
}

/// A number as a tag value: the shortest text that reads back to it.
fn format_f64(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    format!("{v}")
}

/// A `KeyValue`: its key and its value as text.
pub(crate) fn key_value(msg: &[u8]) -> Result<Option<(String, String)>, OtlpError> {
    let (mut key, mut value) = (None, None);
    for f in Fields::new(msg) {
        match f? {
            (1, w) => key = w.str().map(str::to_owned),
            (2, w) => value = Some(any_value_text(w.bytes().unwrap_or(&[]))?),
            _ => {}
        }
    }
    Ok(key.filter(|k| !k.is_empty()).map(|k| (truncate(k), value.unwrap_or_default())))
}

/// A `KeyValue` as a tag: its key, and its value as text, borrowed when the request holds text.
fn key_value_ref(msg: &[u8]) -> Result<Option<Tag<'_>>, OtlpError> {
    let (mut key, mut value) = (None, None);
    for f in Fields::new(msg) {
        match f? {
            (1, w) => key = w.str(),
            (2, w) => value = Some(any_value_cow(w.bytes().unwrap_or(&[]))?),
            _ => {}
        }
    }
    Ok(key.filter(|k| !k.is_empty()).map(|k| (cut(k, MAX_NAME_BYTES), value.unwrap_or(Cow::Borrowed("")))))
}

/// An `AnyValue` as text ([`any_value_text`]): a string, as most are, borrowed.
fn any_value_cow(msg: &[u8]) -> Result<Cow<'_, str>, OtlpError> {
    for f in Fields::new(msg) {
        match f? {
            (1, w) => return Ok(Cow::Borrowed(cut(w.str().unwrap_or(""), MAX_NAME_BYTES))),
            (2..=7, _) => return Ok(Cow::Owned(any_value_text(msg)?)),
            _ => {}
        }
    }
    Ok(Cow::Borrowed(""))
}

/// `s`, at most `max` bytes of it, cut where a character starts.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut at = max;
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    s.get(..at).unwrap_or("")
}

fn truncate(s: String) -> String {
    truncate_to(s, MAX_NAME_BYTES)
}

fn truncate_to(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// An `AnyValue` as text: scalars as themselves, bytes as hex, arrays and maps as compact JSON;
/// nesting is walked with a stack of its own, at most [`MAX_DEPTH`] deep.
fn any_value_text(msg: &[u8]) -> Result<String, OtlpError> {
    any_value_text_within(msg, MAX_NAME_BYTES)
}

/// [`any_value_text`] of at most `max` bytes: a tag's value is a name's length, a log's body a
/// message's.
pub(crate) fn any_value_text_within(msg: &[u8], max: usize) -> Result<String, OtlpError> {
    let mut out = String::new();
    // Each frame: an array or a map being written, its fields left, whether one was written.
    enum Frame<'a> {
        Array(Fields<'a>, bool),
        Map(Fields<'a>, bool),
    }
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut pending: Option<&[u8]> = Some(msg);
    loop {
        if let Some(v) = pending.take() {
            // Write one AnyValue: a scalar now, or open a frame.
            let mut wrote = false;
            for f in Fields::new(v) {
                match f? {
                    (1, w) => {
                        let s = w.str().unwrap_or("");
                        if stack.is_empty() {
                            out.push_str(s);
                        } else {
                            push_json_str(&mut out, s);
                        }
                        wrote = true;
                    }
                    (2, w) => {
                        out.push_str(if w.u64() == Some(1) { "true" } else { "false" });
                        wrote = true;
                    }
                    (3, w) => {
                        out.push_str(&w.i64().unwrap_or(0).to_string());
                        wrote = true;
                    }
                    (4, w) => {
                        out.push_str(&format_f64(w.f64().unwrap_or(f64::NAN)));
                        wrote = true;
                    }
                    (5, w) | (6, w) if stack.len() >= MAX_DEPTH => {
                        let _ = w;
                        out.push('…');
                        wrote = true;
                    }
                    (5, w) => {
                        out.push('[');
                        stack.push(Frame::Array(Fields::new(w.bytes().unwrap_or(&[])), false));
                        break;
                    }
                    (6, w) => {
                        out.push('{');
                        stack.push(Frame::Map(Fields::new(w.bytes().unwrap_or(&[])), false));
                        break;
                    }
                    (7, w) => {
                        for b in w.bytes().unwrap_or(&[]) {
                            out.push_str(&format!("{b:02x}"));
                        }
                        wrote = true;
                    }
                    _ => {}
                }
                if wrote {
                    break;
                }
            }
            if out.len() > max {
                break;
            }
        }
        // Continue the innermost frame.
        let Some(frame) = stack.last_mut() else { break };
        match frame {
            Frame::Array(fields, any) => match fields.next() {
                Some(f) => {
                    if let (1, w) = f? {
                        if *any {
                            out.push(',');
                        }
                        *any = true;
                        pending = Some(w.bytes().unwrap_or(&[]));
                    }
                }
                None => {
                    out.push(']');
                    stack.pop();
                }
            },
            Frame::Map(fields, any) => match fields.next() {
                Some(f) => {
                    if let (1, w) = f? {
                        let (mut k, mut v) = ("", None);
                        for f in Fields::new(w.bytes().unwrap_or(&[])) {
                            match f? {
                                (1, kw) => k = kw.str().unwrap_or(""),
                                (2, vw) => v = vw.bytes(),
                                _ => {}
                            }
                        }
                        if *any {
                            out.push(',');
                        }
                        *any = true;
                        push_json_str(&mut out, k);
                        out.push(':');
                        pending = Some(v.unwrap_or(&[]));
                    }
                }
                None => {
                    out.push('}');
                    stack.pop();
                }
            },
        }
    }
    Ok(truncate_to(out, max))
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::super::pb::Writer;
    use super::*;

    fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
        w.message(field, |m| {
            m.str(1, k);
            m.message(2, |a| a.str(1, v));
        });
    }

    /// A request: one resource (service.name=api, host.name=web-1), a gauge, a monotonic
    /// cumulative sum, a delta sum, an explicit histogram, an exponential histogram, a summary.
    fn request() -> Vec<u8> {
        let mut w = Writer::default();
        w.message(1, |rm| {
            rm.message(1, |r| {
                kv(r, 1, "service.name", "api");
                kv(r, 1, "host.name", "web-1");
            });
            rm.message(2, |sm| {
                sm.message(2, |m| {
                    m.str(1, "temp");
                    m.message(5, |g| {
                        g.message(1, |p| {
                            kv(p, 7, "room", "a");
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.double(4, 21.5);
                        });
                        g.message(1, |p| {
                            p.fixed64(3, 1_700_000_001_000_000_000);
                            p.varint(8, 1);
                        });
                    });
                });
                sm.message(2, |m| {
                    m.str(1, "requests");
                    m.message(7, |s| {
                        s.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.fixed64(6, 42);
                        });
                        s.varint(2, 2);
                        s.varint(3, 1);
                    });
                });
                sm.message(2, |m| {
                    m.str(1, "errors");
                    m.message(7, |s| {
                        s.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.double(4, 3.0);
                        });
                        s.varint(2, 1);
                        s.varint(3, 1);
                    });
                });
                sm.message(2, |m| {
                    m.str(1, "latency");
                    m.message(9, |h| {
                        h.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.fixed64(4, 10);
                            p.double(5, 55.0);
                            let mut counts = Vec::new();
                            for c in [2u64, 5, 3] {
                                counts.extend_from_slice(&c.to_le_bytes());
                            }
                            p.bytes(6, &counts);
                            let mut bounds = Vec::new();
                            for b in [1.0f64, 10.0] {
                                bounds.extend_from_slice(&b.to_le_bytes());
                            }
                            p.bytes(7, &bounds);
                        });
                        h.varint(2, 2);
                    });
                });
                sm.message(2, |m| {
                    m.str(1, "size");
                    m.message(10, |h| {
                        h.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.fixed64(4, 100);
                            p.varint(6, 0);
                            // scale 0: bucket i is (2^i, 2^(i+1)]; offset 3 → (8, 16]... 100 in one bucket.
                            p.message(8, |b| {
                                b.varint(1, crate::varint::zig(3));
                                b.varint(2, 100);
                            });
                        });
                        h.varint(2, 1);
                    });
                });
                sm.message(2, |m| {
                    m.str(1, "rpc");
                    m.message(11, |s| {
                        s.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.fixed64(4, 7);
                            p.double(5, 70.0);
                            p.message(6, |q| {
                                q.double(1, 0.99);
                                q.double(2, 9.5);
                            });
                        });
                    });
                });
            });
        });
        w.buf
    }

    fn find<'a>(d: &'a Decoded, metric: &str, tag: Option<(&str, &str)>) -> &'a (SeriesKey, Vec<(i64, f64)>) {
        d.series
            .iter()
            .find(|(k, _)| {
                let parsed = crate::batch::parse_key(&k.key).unwrap();
                parsed.metric == metric && tag.is_none_or(|t| parsed.tags.contains(&t))
            })
            .unwrap_or_else(|| panic!("no {metric} {tag:?}"))
    }

    #[test]
    fn every_metric_type_maps_to_series() {
        let d = decode(&request(), 5).unwrap();
        let (k, s) = find(&d, "temp", Some(("room", "a")));
        assert_eq!(k.kind, Kind::Gauge);
        assert_eq!(s, &vec![(1_700_000_000_000, 21.5)]);
        let tags = crate::batch::parse_key(&k.key).unwrap().tags;
        for t in [("service", "api"), ("host", "web-1"), ("service.name", "api"), ("host.name", "web-1")] {
            assert!(tags.contains(&t), "{t:?} in {tags:?}");
        }
        assert_eq!(d.dropped.no_value, 1);
        assert_eq!(find(&d, "requests", None).0.kind, Kind::Counter);
        assert_eq!(find(&d, "requests", None).1, vec![(1_700_000_000_000, 42.0)]);
        assert_eq!(find(&d, "errors", None).0.kind, Kind::Count);
        assert_eq!(find(&d, "latency.count", None).1[0].1, 10.0);
        assert_eq!(find(&d, "latency.sum", None).1[0].1, 55.0);
        assert_eq!(find(&d, "latency.bucket", Some(("le", "1"))).1[0].1, 2.0);
        assert_eq!(find(&d, "latency.bucket", Some(("le", "10"))).1[0].1, 7.0);
        assert_eq!(find(&d, "latency.bucket", Some(("le", "+Inf"))).1[0].1, 10.0);
        // All 100 in the bucket (8, 16] at scale 0: every quantile its log-midpoint, 2^3.5.
        let p50 = find(&d, "size.p50", None).1[0].1;
        assert!((p50 - 2f64.powf(3.5)).abs() < 1e-9, "{p50}");
        assert_eq!(find(&d, "rpc.quantile", Some(("quantile", "0.99"))).1[0].1, 9.5);
        assert_eq!(find(&d, "rpc.count", None).1[0].1, 7.0);
    }

    #[test]
    fn exponential_histogram_quantiles_stay_within_their_relative_error() {
        // Values 1..=1000 into scale-4 buckets: each quantile within (γ−1)/(γ+1) of the true one.
        let scale = 4;
        let gamma_log2 = (2f64).powi(-scale);
        let mut pos = vec![0u64; 0];
        let mut off = i64::MAX;
        let idx = |v: f64| ((v.log2() / gamma_log2).ceil() as i64) - 1;
        for v in 1..=1000 {
            off = off.min(idx(v as f64));
        }
        for v in 1..=1000 {
            let i = (idx(v as f64) - off) as usize;
            if pos.len() <= i {
                pos.resize(i + 1, 0);
            }
            pos[i] += 1;
        }
        let h = ExpHist { scale, zero: 0, pos_off: off, pos: &pos, neg_off: 0, neg: &[] };
        let gamma = 2f64.powf(gamma_log2);
        let bound = (gamma - 1.0) / (gamma + 1.0);
        for q in [0.5, 0.9, 0.95, 0.99] {
            let truth = (q * 999.0f64).floor() + 1.0;
            let got = h.quantile(q).unwrap();
            assert!(((got - truth) / truth).abs() <= bound + 1e-12, "q {q}: {got} vs {truth}, bound {bound}");
        }
    }

    #[test]
    fn nested_attribute_values_render_as_json_and_hostile_input_never_panics() {
        let mut w = Writer::default();
        w.str(1, "k");
        w.message(2, |a| {
            a.message(6, |kvl| {
                kvl.message(1, |e| {
                    e.str(1, "x");
                    e.message(2, |v| {
                        v.message(5, |arr| {
                            arr.message(1, |i| i.varint(3, 1));
                            arr.message(1, |i| i.str(1, "two"));
                        });
                    });
                });
            });
        });
        assert_eq!(key_value(&w.buf).unwrap(), Some(("k".to_owned(), r#"{"x":[1,"two"]}"#.to_owned())));
        // Nested past the bound: cut, not followed.
        let mut deep = Writer::default();
        deep.str(1, "v");
        let mut inner = Writer::default();
        inner.varint(3, 7);
        for _ in 0..50 {
            let mut arr = Writer::default();
            arr.message(1, |e| e.buf.extend_from_slice(&inner.buf));
            let mut any = Writer::default();
            any.bytes(5, &arr.buf);
            inner = any;
        }
        deep.bytes(2, &inner.buf);
        let (_, v) = key_value(&deep.buf).unwrap().unwrap();
        assert!(v.contains('…'), "{v}");
        let req = request();
        let mut rng = crate::testkit::Rng::new(6);
        for _ in 0..5000 {
            let mut bad = req.clone();
            for _ in 0..1 + rng.below(4) {
                let i = rng.below(bad.len() as u64) as usize;
                bad[i] = rng.next_u64() as u8;
            }
            let _ = decode(&bad, 0);
            let cut = rng.below(req.len() as u64) as usize;
            let _ = decode(&req[..cut], 0);
        }
    }
}
