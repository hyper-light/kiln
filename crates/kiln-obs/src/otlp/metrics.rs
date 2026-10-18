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

use std::collections::HashMap;

use super::pb::{Fields, PbError, doubles, fixed64s, varints};
use crate::batch::{Kind, MAX_NAME_BYTES, MAX_TAGS, SeriesKey};

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

/// Builds series as points arrive.
struct Sink {
    out: Decoded,
    by_key: HashMap<Vec<u8>, usize>,
    scratch_tags: Vec<(String, String)>,
}

impl Sink {
    fn point(&mut self, metric: &str, tags: &[(String, String)], extra: Option<(&str, &str)>, kind: Kind, ts: i64, v: f64) {
        self.scratch_tags.clear();
        self.scratch_tags.extend(tags.iter().cloned());
        if let Some((k, x)) = extra {
            self.scratch_tags.retain(|(tk, _)| tk != k);
            self.scratch_tags.push((k.to_owned(), x.to_owned()));
        }
        if self.scratch_tags.len() > MAX_TAGS {
            self.scratch_tags.sort_unstable();
            self.out.dropped.tags += (self.scratch_tags.len() - MAX_TAGS) as u64;
            self.scratch_tags.truncate(MAX_TAGS);
        }
        let key = match SeriesKey::new(metric, self.scratch_tags.iter().map(|(k, v)| (k.as_str(), v.as_str())), kind) {
            Ok(k) => k,
            Err(_) => {
                self.out.dropped.invalid += 1;
                return;
            }
        };
        self.out.points += 1;
        match self.by_key.get(&key.key) {
            Some(&i) => {
                if let Some(s) = self.out.series.get_mut(i) {
                    s.1.push((ts, v));
                }
            }
            None => {
                self.by_key.insert(key.key.clone(), self.out.series.len());
                self.out.series.push((key, vec![(ts, v)]));
            }
        }
    }
}

/// Decodes an `ExportMetricsServiceRequest`; `now_ms` is the time a point without one takes.
pub fn decode(body: &[u8], now_ms: i64) -> Result<Decoded, OtlpError> {
    let mut sink = Sink { out: Decoded::default(), by_key: HashMap::new(), scratch_tags: Vec::new() };
    for f in Fields::new(body) {
        let (n, w) = f?;
        if n == 1 {
            resource_metrics(w.bytes().unwrap_or(&[]), now_ms, &mut sink)?;
        }
    }
    Ok(sink.out)
}

fn resource_metrics(msg: &[u8], now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
    let mut resource: Vec<(String, String)> = Vec::new();
    let mut scopes: Vec<&[u8]> = Vec::new();
    for f in Fields::new(msg) {
        match f? {
            (1, w) => {
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    if let (1, kv) = f?
                        && let Some(t) = key_value(kv.bytes().unwrap_or(&[]))?
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

fn metric(msg: &[u8], resource: &[(String, String)], now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
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
    for p in points {
        match shape {
            Shape::Gauge | Shape::Sum { .. } => {
                let kind = match shape {
                    Shape::Sum { cumulative: true, monotonic: true } => Kind::Counter,
                    Shape::Sum { cumulative: false, .. } => Kind::Count,
                    _ => Kind::Gauge,
                };
                number_point(p, name, resource, kind, now_ms, sink)?;
            }
            Shape::Histogram { cumulative } => histogram_point(p, name, resource, count_kind(cumulative), now_ms, sink)?,
            Shape::Exponential { cumulative } => exponential_point(p, name, resource, count_kind(cumulative), now_ms, sink)?,
            Shape::Summary => summary_point(p, name, resource, now_ms, sink)?,
        }
    }
    Ok(())
}

/// The point's tags: its attributes over the resource's, then the unified service tags.
fn tags_of(point_attrs: Vec<(String, String)>, resource: &[(String, String)]) -> Vec<(String, String)> {
    let mut tags: Vec<(String, String)> = resource.iter().filter(|(k, _)| !point_attrs.iter().any(|(pk, _)| pk == k)).cloned().collect();
    tags.extend(point_attrs);
    for (alias, sources) in [("service", &["service.name"][..]), ("env", &["deployment.environment.name", "deployment.environment"][..]), ("host", &["host.name"][..])] {
        if tags.iter().any(|(k, _)| k == alias) {
            continue;
        }
        if let Some(v) = sources.iter().find_map(|s| tags.iter().find(|(k, _)| k == s).map(|(_, v)| v.clone())) {
            tags.push((alias.to_owned(), v));
        }
    }
    tags
}

fn time_ms(ns: u64, now_ms: i64) -> i64 {
    if ns == 0 { now_ms } else { (ns / 1_000_000) as i64 }
}

fn number_point(msg: &[u8], name: &str, resource: &[(String, String)], kind: Kind, now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
    let (mut attrs, mut t, mut v, mut flags) = (Vec::new(), 0u64, None, 0u64);
    for f in Fields::new(msg) {
        match f? {
            (7, w) => {
                if let Some(kv) = key_value(w.bytes().unwrap_or(&[]))? {
                    attrs.push(kv);
                }
            }
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
    let tags = tags_of(attrs, resource);
    sink.point(name, &tags, None, kind, time_ms(t, now_ms), v.unwrap_or(f64::NAN));
    Ok(())
}

fn histogram_point(msg: &[u8], name: &str, resource: &[(String, String)], count_kind: Kind, now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
    let (mut attrs, mut t, mut flags) = (Vec::new(), 0u64, 0u64);
    let (mut count, mut sum, mut min, mut max) = (None, None, None, None);
    let (mut counts, mut bounds) = (Vec::new(), Vec::new());
    for f in Fields::new(msg) {
        match f? {
            (9, w) => {
                if let Some(kv) = key_value(w.bytes().unwrap_or(&[]))? {
                    attrs.push(kv);
                }
            }
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
        return Ok(());
    }
    let tags = tags_of(attrs, resource);
    let ts = time_ms(t, now_ms);
    let sum_kind = if count_kind == Kind::Counter { Kind::Counter } else { Kind::Count };
    if let Some(c) = count {
        sink.point(&format!("{name}.count"), &tags, None, count_kind, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&format!("{name}.sum"), &tags, None, sum_kind, ts, s);
    }
    for (label, v) in [("min", min), ("max", max)] {
        if let Some(v) = v {
            sink.point(&format!("{name}.{label}"), &tags, None, Kind::Gauge, ts, v);
        }
    }
    // Bucket i holds (bounds[i-1], bounds[i]]; the last, (bounds[last], +Inf). Cumulative `le`.
    let mut cum = 0u64;
    let bucket = format!("{name}.bucket");
    for (i, &c) in counts.iter().enumerate() {
        cum = cum.saturating_add(c);
        let le = bounds.get(i).map_or_else(|| "+Inf".to_owned(), |b| format_f64(*b));
        sink.point(&bucket, &tags, Some(("le", &le)), count_kind, ts, cum as f64);
    }
    Ok(())
}

fn exponential_point(msg: &[u8], name: &str, resource: &[(String, String)], count_kind: Kind, now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
    let (mut attrs, mut t, mut flags) = (Vec::new(), 0u64, 0u64);
    let (mut count, mut sum, mut min, mut max, mut scale, mut zero) = (None, None, None, None, 0i64, 0u64);
    let (mut pos_off, mut pos, mut neg_off, mut neg) = (0i64, Vec::new(), 0i64, Vec::new());
    for f in Fields::new(msg) {
        match f? {
            (1, w) => {
                if let Some(kv) = key_value(w.bytes().unwrap_or(&[]))? {
                    attrs.push(kv);
                }
            }
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
    let tags = tags_of(attrs, resource);
    let ts = time_ms(t, now_ms);
    let sum_kind = if count_kind == Kind::Counter { Kind::Counter } else { Kind::Count };
    if let Some(c) = count {
        sink.point(&format!("{name}.count"), &tags, None, count_kind, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&format!("{name}.sum"), &tags, None, sum_kind, ts, s);
    }
    for (label, v) in [("min", min), ("max", max)] {
        if let Some(v) = v {
            sink.point(&format!("{name}.{label}"), &tags, None, Kind::Gauge, ts, v);
        }
    }
    let h = ExpHist { scale: scale.clamp(-10, 20) as i32, zero, pos_off, pos: &pos, neg_off, neg: &neg };
    for (label, q) in [("p50", 0.50), ("p90", 0.90), ("p95", 0.95), ("p99", 0.99)] {
        if let Some(v) = h.quantile(q) {
            sink.point(&format!("{name}.{label}"), &tags, None, Kind::Gauge, ts, v);
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

fn summary_point(msg: &[u8], name: &str, resource: &[(String, String)], now_ms: i64, sink: &mut Sink) -> Result<(), OtlpError> {
    let (mut attrs, mut t, mut flags, mut count, mut sum) = (Vec::new(), 0u64, 0u64, None, None);
    let mut quantiles: Vec<(f64, f64)> = Vec::new();
    for f in Fields::new(msg) {
        match f? {
            (7, w) => {
                if let Some(kv) = key_value(w.bytes().unwrap_or(&[]))? {
                    attrs.push(kv);
                }
            }
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
    let tags = tags_of(attrs, resource);
    let ts = time_ms(t, now_ms);
    if let Some(c) = count {
        sink.point(&format!("{name}.count"), &tags, None, Kind::Counter, ts, c as f64);
    }
    if let Some(s) = sum {
        sink.point(&format!("{name}.sum"), &tags, None, Kind::Counter, ts, s);
    }
    let qname = format!("{name}.quantile");
    for (q, v) in quantiles {
        sink.point(&qname, &tags, Some(("quantile", &format_f64(q))), Kind::Gauge, ts, v);
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
fn key_value(msg: &[u8]) -> Result<Option<(String, String)>, OtlpError> {
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

fn truncate(mut s: String) -> String {
    if s.len() > MAX_NAME_BYTES {
        let mut cut = MAX_NAME_BYTES;
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
            if out.len() > MAX_NAME_BYTES {
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
    Ok(truncate(out))
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
