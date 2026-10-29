//! Prometheus's remote write (`docs/observe.md` §8.5): what Prometheus (and Grafana Alloy,
//! VictoriaMetrics' vmagent, the OpenTelemetry Collector's `prometheusremotewrite`) send a
//! receiver. A protobuf message compressed with Snappy's block format, of protocol 1.0
//! (`prometheus.WriteRequest`, its series' labels and samples, families' metadata beside them) or
//! 2.0 (`io.prometheus.write.v2.Request`, its strings interned in a table and each series' type
//! with it).
//!
//! - A series is its labels: `__name__` its metric (a series without one is no series), the rest
//!   its tags, as a scrape of the same target tags them (§8), so a receiver and a scrape of one
//!   target write the same series. A label of no value is no label (Prometheus's own rule).
//! - Its kind as a scrape gives it: a counter's, a histogram's `_bucket`, `_count` and `_sum`, a
//!   summary's `_count` and `_sum` are counters; the rest gauges. Its type is its series' own
//!   (2.0), else its family's as metadata said (1.0 sends it apart, every minute: what a receiver
//!   learned is kept, [`Families`]), else its name's: a name ending `_total`, `_bucket`, `_count` or
//!   `_sum` a counter, as Prometheus's conventions name them.
//! - Samples keep their own times (ms). A value that is not a number is a staleness marker (or
//!   nothing): skipped. Exemplars are counted, not taken.
//! - A native histogram (Prometheus's sparse exponential one) is kiln's distribution (§4.4), as an
//!   OpenTelemetry exponential histogram is: its schema is the scale (made coarser past kiln's
//!   finest), its buckets' indices one past OpenTelemetry's (`(base^(i-1), base^i]`), read from its
//!   spans and its counts (integers, each the change from the one before), its zero bucket kiln's;
//!   a counter's (every one but a gauge's) turned into what rose since the sample before, as a
//!   cumulative OpenTelemetry histogram's is (the first of a series the receiver has not seen:
//!   none, what came before it not known). `.count` and `.sum` beside its buckets. A float
//!   histogram (recording rules') or one of custom buckets is counted, not yet taken.
//!
//! What a request decodes to is an OTLP request's shape ([`Decoded`]): each series' key once,
//! found in the receiver's cache ([`Keys`]) after its first time, its samples flat.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::batch::{Kind, MAX_TAGS};
use crate::dist;
use crate::otlp::metrics::{Decoded, Keys};
use crate::otlp::pb::{Fields, PbError, varints};
use crate::varint;

/// A request's body unzipped, at the most: Prometheus sends 2,000 samples a request unless told
/// otherwise (`max_samples_per_send`), some hundreds of KB; this bounds a hostile one, as an OTLP
/// request's bound does (the OpenTelemetry Collector's 20 MiB, compressed or not, taken as both).
pub const MAX_BODY: usize = 32 << 20;
/// Families whose metadata a receiver keeps: past it, it starts again (types then come from
/// names until metadata comes again, within a minute).
const FAMILIES_MAX: usize = 1 << 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnappyError {
    #[error("the body ends inside what Snappy says follows")]
    Truncated,
    #[error("the body unzips past {MAX_BODY} bytes")]
    TooLarge,
    #[error("the body is not Snappy's block format")]
    Corrupt,
}

/// Snappy's block format (google/snappy, `format_description.txt`): the length unzipped as a
/// varint, then literals and copies of what came before, into `out` (cleared first), `max` bytes
/// at the most. Every malformed input is an error, never a panic or a read past what was written.
pub fn unsnappy(input: &[u8], out: &mut Vec<u8>, max: usize) -> Result<(), SnappyError> {
    out.clear();
    let mut at = input;
    let n = varint::get(&mut at).ok_or(SnappyError::Truncated)?;
    let n = usize::try_from(n).ok().filter(|&n| n <= max).ok_or(SnappyError::TooLarge)?;
    out.reserve(n);
    while let Some((&tag, rest)) = at.split_first() {
        at = rest;
        let (offset, len) = match tag & 3 {
            0 => {
                // A literal: its length less one in the tag's six bits, or in the 1 to 4 bytes after.
                let mut len = usize::from(tag >> 2);
                if len >= 60 {
                    let (b, rest) = at.split_at_checked(len - 59).ok_or(SnappyError::Truncated)?;
                    at = rest;
                    len = b.iter().rev().fold(0usize, |a, &x| (a << 8) | usize::from(x));
                }
                let len = len.checked_add(1).ok_or(SnappyError::Corrupt)?;
                let (lit, rest) = at.split_at_checked(len).ok_or(SnappyError::Truncated)?;
                at = rest;
                if out.len() + len > n {
                    return Err(SnappyError::Corrupt);
                }
                out.extend_from_slice(lit);
                continue;
            }
            1 => {
                let (&b, rest) = at.split_first().ok_or(SnappyError::Truncated)?;
                at = rest;
                ((usize::from(tag >> 5) << 8) | usize::from(b), usize::from((tag >> 2) & 7) + 4)
            }
            2 => {
                let (b, rest) = at.split_first_chunk::<2>().ok_or(SnappyError::Truncated)?;
                at = rest;
                (usize::from(u16::from_le_bytes(*b)), usize::from(tag >> 2) + 1)
            }
            _ => {
                let (b, rest) = at.split_first_chunk::<4>().ok_or(SnappyError::Truncated)?;
                at = rest;
                (usize::try_from(u32::from_le_bytes(*b)).map_err(|_| SnappyError::Corrupt)?, usize::from(tag >> 2) + 1)
            }
        };
        // A copy: `len` bytes from `offset` back, overlapping what it writes when the offset is
        // short (a run): copied from where it starts, doubling, as it repeats.
        if offset == 0 || offset > out.len() || out.len() + len > n {
            return Err(SnappyError::Corrupt);
        }
        let start = out.len() - offset;
        let mut left = len;
        while left > 0 {
            let take = left.min(out.len() - start);
            out.extend_from_within(start..start + take);
            left -= take;
        }
    }
    if out.len() == n { Ok(()) } else { Err(SnappyError::Corrupt) }
}

/// Which protocol a request is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// `prometheus.WriteRequest`, remote write 1.0.
    V1,
    /// `io.prometheus.write.v2.Request`, remote write 2.0.
    V2,
}

impl Version {
    /// The protocol a request's `Content-Type` names: 2.0's message by name, else 1.0 (its
    /// default, named or not), as the specification's content negotiation has it. None for a
    /// message of neither.
    pub fn of(content_type: &str) -> Option<Self> {
        let mut parts = content_type.split(';').map(str::trim);
        if !parts.next().is_some_and(|t| t.eq_ignore_ascii_case("application/x-protobuf") || t.is_empty()) {
            return None;
        }
        match parts.find_map(|p| p.strip_prefix("proto=")).map(|p| p.trim_matches('"')) {
            None | Some("prometheus.WriteRequest") => Some(Self::V1),
            Some("io.prometheus.write.v2.Request") => Some(Self::V2),
            Some(_) => None,
        }
    }
}

/// A metric family's type, as Prometheus's metadata names it (1.0's `MetricMetadata.MetricType`,
/// 2.0's `Metadata.MetricType`: the same numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Unknown,
    Counter,
    Gauge,
    Histogram,
    GaugeHistogram,
    Summary,
    Info,
    StateSet,
}

impl Type {
    fn of(n: u64) -> Self {
        match n {
            1 => Self::Counter,
            2 => Self::Gauge,
            3 => Self::Histogram,
            4 => Self::GaugeHistogram,
            5 => Self::Summary,
            6 => Self::Info,
            7 => Self::StateSet,
            _ => Self::Unknown,
        }
    }
}

/// The suffixes a family's parts are named with, and the family's name without them.
fn family_of(name: &str) -> &str {
    for suffix in ["_total", "_bucket", "_count", "_sum"] {
        if let Some(base) = name.strip_suffix(suffix) {
            return base;
        }
    }
    name
}

/// A series' kind from its family's type: a scrape's (`crate::prom`) for the same type; an
/// unknown one by its name.
fn kind_of(name: &str, t: Type) -> Kind {
    let part = |s: &str| name.ends_with(s);
    match t {
        Type::Counter => Kind::Counter,
        Type::Histogram => Kind::Counter,
        Type::Summary if part("_count") || part("_sum") => Kind::Counter,
        Type::Unknown if part("_total") || part("_bucket") || part("_count") || part("_sum") => Kind::Counter,
        _ => Kind::Gauge,
    }
}

/// The metric families' types a receiver has learned from 1.0's metadata, which comes apart from
/// the series it is of.
#[derive(Debug, Default)]
pub struct Families {
    types: Mutex<HashMap<String, u8>>,
}

impl Families {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, family: &str) -> Type {
        self.types.lock().unwrap_or_else(PoisonError::into_inner).get(family).map_or(Type::Unknown, |&t| Type::of(u64::from(t)))
    }

    fn learn(&self, family: &str, t: u64) {
        let mut types = self.types.lock().unwrap_or_else(PoisonError::into_inner);
        if types.get(family).is_some_and(|&had| u64::from(had) == t) {
            return;
        }
        if types.len() >= FAMILIES_MAX {
            types.clear();
        }
        types.insert(family.to_owned(), u8::try_from(t).unwrap_or(0));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("{0}")]
    Pb(#[from] PbError),
    #[error("a label's name or value is not UTF-8")]
    Utf8,
    #[error("a series refers to a string the request does not have")]
    Symbol,
}

/// What a request held besides the samples taken.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    pub series: u64,
    pub samples: u64,
    /// Series that are none (no `__name__`, a name twice, past `MAX_TAGS` labels, a name too long)
    /// and their samples; values that are not numbers (staleness markers).
    pub invalid_series: u64,
    pub skipped_samples: u64,
    pub stale: u64,
    /// Native histograms, and those taken (a float one, or one of custom buckets, is not yet);
    /// exemplars, counted, not taken.
    pub histograms: u64,
    pub histograms_taken: u64,
    pub exemplars: u64,
    /// Metadata of families (1.0).
    pub metadata: u64,
}

/// Decodes a request (unzipped) into `out` (cleared first; its room kept), its series' keys
/// found in or made into `keys`, 1.0's metadata learned into `families`.
pub fn decode_into(body: &[u8], version: Version, keys: &Keys, families: &Families, out: &mut Decoded) -> Result<Taken, WriteError> {
    out.clear();
    let mut taken = Taken::default();
    let mut labels: Vec<(&str, &str)> = Vec::new();
    let mut key = Vec::new();
    match version {
        Version::V1 => {
            // Metadata first, wherever it is in the message: a request may hold its families' with
            // their series.
            for f in Fields::new(body) {
                if let (3, w) = f? {
                    let (mut t, mut name) = (0, "");
                    for g in Fields::new(w.bytes().unwrap_or_default()) {
                        match g? {
                            (1, w) => t = w.u64().unwrap_or(0),
                            (2, w) => name = w.str().ok_or(WriteError::Utf8)?,
                            _ => {}
                        }
                    }
                    if !name.is_empty() {
                        families.learn(name, t);
                        taken.metadata += 1;
                    }
                }
            }
            for f in Fields::new(body) {
                let (1, w) = f? else { continue };
                labels.clear();
                let ts = w.bytes().unwrap_or_default();
                for g in Fields::new(ts) {
                    match g? {
                        (1, w) => {
                            let (mut name, mut value) = ("", "");
                            for h in Fields::new(w.bytes().unwrap_or_default()) {
                                match h? {
                                    (1, w) => name = w.str().ok_or(WriteError::Utf8)?,
                                    (2, w) => value = w.str().ok_or(WriteError::Utf8)?,
                                    _ => {}
                                }
                            }
                            labels.push((name, value));
                        }
                        (3, _) => taken.exemplars += 1,
                        _ => {}
                    }
                }
                series(ts, 4, &mut labels, None, keys, families, &mut key, out, &mut taken)?;
            }
        }
        Version::V2 => {
            let mut symbols: Vec<&str> = Vec::new();
            for f in Fields::new(body) {
                if let (4, w) = f? {
                    symbols.push(w.str().ok_or(WriteError::Utf8)?);
                }
            }
            let mut refs: Vec<u64> = Vec::new();
            for f in Fields::new(body) {
                let (5, w) = f? else { continue };
                let ts = w.bytes().unwrap_or_default();
                refs.clear();
                let mut t = Type::Unknown;
                for g in Fields::new(ts) {
                    match g? {
                        (1, w) => varints(&w, &mut refs)?,
                        (4, _) => taken.exemplars += 1,
                        (5, w) => {
                            for h in Fields::new(w.bytes().unwrap_or_default()) {
                                if let (1, w) = h? {
                                    t = Type::of(w.u64().unwrap_or(0));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                labels.clear();
                for pair in refs.chunks(2) {
                    let [n, v] = pair else { return Err(WriteError::Symbol) };
                    let s = |i: u64| usize::try_from(i).ok().and_then(|i| symbols.get(i)).copied().ok_or(WriteError::Symbol);
                    labels.push((s(*n)?, s(*v)?));
                }
                series(ts, 3, &mut labels, Some(t), keys, families, &mut key, out, &mut taken)?;
            }
        }
    }
    Ok(taken)
}

/// One series' samples (`ts`, its message: its samples field 2, its native histograms
/// `hist_field`) taken into `out`.
#[allow(clippy::too_many_arguments)]
fn series(ts: &[u8], hist_field: u32, labels: &mut Vec<(&str, &str)>, own: Option<Type>, keys: &Keys, families: &Families, key: &mut Vec<u8>, out: &mut Decoded, taken: &mut Taken) -> Result<(), WriteError> {
    let samples = || Fields::new(ts).filter_map(|g| g.ok()).filter(|(n, _)| *n == 2);
    let histograms = || Fields::new(ts).filter_map(|g| g.ok()).filter(|(n, _)| *n == hist_field);
    // Its metric, and its other labels with a value, by name; a name twice is no series.
    let name = labels.iter().find(|(n, _)| *n == "__name__").map(|(_, v)| *v).unwrap_or_default();
    labels.retain(|(n, v)| *n != "__name__" && !v.is_empty());
    labels.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let twice = labels.windows(2).any(|w| matches!(w, [a, b] if a.0 == b.0));
    let made = if name.is_empty() || twice || labels.len() > MAX_TAGS {
        None
    } else {
        let t = match own {
            Some(t) if t != Type::Unknown => t,
            _ => families.get(name).max_known(families.get(family_of(name))),
        };
        keys.key(name, labels, kind_of(name, t), key)
    };
    let Some(k) = made else {
        taken.invalid_series += 1;
        taken.skipped_samples += samples().count() as u64;
        taken.histograms += histograms().count() as u64;
        return Ok(());
    };
    for (_, w) in histograms() {
        taken.histograms += 1;
        if native(w.bytes().unwrap_or_default(), name, labels, keys, key, out)? {
            taken.histograms_taken += 1;
        }
    }
    let place = out.keys.len() as u32;
    let mut any = false;
    for (_, w) in samples() {
        let (mut value, mut at) = (f64::NAN, 0i64);
        for s in Fields::new(w.bytes().unwrap_or_default()) {
            match s? {
                (1, w) => value = w.f64().unwrap_or(f64::NAN),
                (2, w) => at = w.i64().unwrap_or(0),
                _ => {}
            }
        }
        if !value.is_finite() {
            taken.stale += 1;
            continue;
        }
        out.samples.push((place, at, value));
        out.points += 1;
        taken.samples += 1;
        any = true;
    }
    if any {
        out.keys.push(k);
        taken.series += 1;
    }
    Ok(())
}

/// The most buckets a native histogram's spans may hold: Prometheus's own bound
/// (`native_histogram_bucket_limit`, 160 by default) many times over; past it, it is not taken.
const NATIVE_BUCKETS: u64 = 4096;

/// A native histogram (remote write's `Histogram`) of series `name` (`labels` sorted, each key
/// once) into `out` as kiln's distribution; whether it was taken.
fn native(msg: &[u8], name: &str, labels: &[(&str, &str)], keys: &Keys, key: &mut Vec<u8>, out: &mut Decoded) -> Result<bool, WriteError> {
    let (mut count, mut sum, mut schema, mut zero, mut reset, mut at) = (None::<u64>, 0.0f64, 0i64, 0u64, 0u64, 0i64);
    let mut float = false;
    let (mut pos_spans, mut neg_spans): (Vec<(i64, u64)>, Vec<(i64, u64)>) = (Vec::new(), Vec::new());
    let (mut pos, mut neg): (Vec<u64>, Vec<u64>) = (Vec::new(), Vec::new());
    let span = |b: &[u8], into: &mut Vec<(i64, u64)>| -> Result<(), WriteError> {
        let (mut offset, mut len) = (0i64, 0u64);
        for f in Fields::new(b) {
            match f? {
                (1, w) => offset = w.sint().unwrap_or(0),
                (2, w) => len = w.u64().unwrap_or(0),
                _ => {}
            }
        }
        into.push((offset, len));
        Ok(())
    };
    for f in Fields::new(msg) {
        match f? {
            (1, w) => count = w.u64(),
            (2 | 7 | 10 | 13, _) => float = true,
            (3, w) => sum = w.f64().unwrap_or(0.0),
            (4, w) => schema = w.sint().unwrap_or(0),
            (6, w) => zero = w.u64().unwrap_or(0),
            (8, w) => span(w.bytes().unwrap_or_default(), &mut neg_spans)?,
            (9, w) => varints(&w, &mut neg)?,
            (11, w) => span(w.bytes().unwrap_or_default(), &mut pos_spans)?,
            (12, w) => varints(&w, &mut pos)?,
            (14, w) => reset = w.u64().unwrap_or(0),
            (15, w) => at = w.i64().unwrap_or(0),
            _ => {}
        }
    }
    // A float histogram, one of custom buckets (schema -53), or a schema no exponential one has.
    let Ok(schema) = i32::try_from(schema) else { return Ok(false) };
    let Some(count) = count else { return Ok(false) };
    if float || !(dist::MIN_SCALE..=8).contains(&schema) || !sum.is_finite() {
        return Ok(false);
    }
    // Its buckets at kiln's scale (each the counts' running sum, its index one less), ascending.
    let to = schema.min(dist::SCALE);
    let k = u32::try_from(schema - to).unwrap_or(0);
    let mut buckets: Vec<(dist::Bucket, u64)> = Vec::new();
    let mut add = |b: dist::Bucket, n: u64| {
        if n == 0 {
            return;
        }
        match buckets.iter_mut().find(|(x, _)| *x == b) {
            Some(e) => e.1 += n,
            None => buckets.push((b, n)),
        }
    };
    for (spans, deltas, positive) in [(&neg_spans, &neg, false), (&pos_spans, &pos, true)] {
        if spans.iter().map(|s| s.1).sum::<u64>() > NATIVE_BUCKETS || spans.iter().map(|s| s.1).sum::<u64>() != deltas.len() as u64 {
            return Ok(false);
        }
        let (mut idx, mut running, mut d) = (0i64, 0i64, deltas.iter());
        for (i, &(offset, len)) in spans.iter().enumerate() {
            idx = if i == 0 { offset } else { idx.saturating_add(offset) };
            for _ in 0..len {
                let delta = d.next().map_or(0, |&z| ((z >> 1) as i64) ^ -((z & 1) as i64));
                running = running.saturating_add(delta);
                let Ok(n) = u64::try_from(running) else { return Ok(false) };
                let j = idx.saturating_sub(1);
                add(if positive { dist::Bucket::Pos(j) } else { dist::Bucket::Neg(j) }.coarser(k), n);
                idx = idx.saturating_add(1);
            }
        }
    }
    add(dist::Bucket::Zero, zero);
    buckets.sort_unstable_by_key(|(b, _)| b.order());
    let dist_name = format!("{name}{}", dist::SUFFIX);
    // A counter's: what rose since the sample before (a gauge's, reset hint 3, as it is).
    let mut delta = Vec::new();
    let at_ns = u64::try_from(at).unwrap_or(0).saturating_mul(1_000_000);
    let (counts, since): (&[(dist::Bucket, u64)], Option<(u64, f64)>) = if reset == 3 {
        (&buckets, Some((count, sum)))
    } else {
        let id = keys.identity(&dist_name, labels, key);
        let since = keys.delta(id, 0, at_ns.max(1), to, &buckets, count, sum, &mut delta);
        (&delta, since)
    };
    let Some((count, sum)) = since else { return Ok(true) };
    let put = |out: &mut Decoded, k: Option<crate::batch::SeriesKey>, v: f64| {
        if let Some(k) = k {
            out.samples.push((out.keys.len() as u32, at, v));
            out.keys.push(k);
            out.points += 1;
        }
    };
    // A label of the bucket tag's own name leaves no room for it: no buckets, as no series can be.
    if !labels.iter().any(|(k, _)| *k == dist::BUCKET_TAG) {
        let tags: Vec<String> = counts.iter().map(|&(b, _)| dist::tag_value(to, b)).collect();
        let mut with: Vec<(&str, &str)> = Vec::with_capacity(labels.len() + 1);
        for (&(_, n), tag) in counts.iter().zip(&tags) {
            with.clear();
            with.extend_from_slice(labels);
            let place = with.partition_point(|(k, _)| *k < dist::BUCKET_TAG);
            with.insert(place, (dist::BUCKET_TAG, tag.as_str()));
            put(out, keys.key(&dist_name, &with, Kind::Count, key), n as f64);
        }
    }
    put(out, keys.key(&format!("{name}.count"), labels, Kind::Count, key), count as f64);
    put(out, keys.key(&format!("{name}.sum"), labels, Kind::Count, key), sum);
    Ok(true)
}

impl Type {
    /// This type, or `other` where this one is unknown.
    fn max_known(self, other: Self) -> Self {
        if self == Self::Unknown { other } else { self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::pb::Writer;

    /// Snappy's block format compressed by a reference: literal runs and every copy form.
    fn snappy_literal(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint::put(&mut out, data.len() as u64);
        for chunk in data.chunks(64) {
            out.push(((chunk.len() - 1) as u8) << 2);
            out.extend_from_slice(chunk);
        }
        out
    }

    #[test]
    fn snappy_reads_literals_and_every_copy_and_refuses_what_is_not_it() {
        let mut out = Vec::new();
        // "abcabcabcabcX": a literal, a 1-byte-offset copy of 9 (overlapping: a run), a literal.
        let mut b = Vec::new();
        varint::put(&mut b, 13);
        b.push(2 << 2);
        b.extend_from_slice(b"abc");
        b.push(0b0001_0101); // a copy, its length 5 + 4 = 9 (bits 2-4), its offset's high bits 0
        b.push(3);
        b.push(0);
        b.push(b'X');
        unsnappy(&b, &mut out, MAX_BODY).unwrap();
        assert_eq!(out, b"abcabcabcabcX");
        // 2- and 4-byte offsets.
        let mut b = Vec::new();
        varint::put(&mut b, 8);
        b.push(3 << 2);
        b.extend_from_slice(b"wxyz");
        b.push((1 << 2) | 2);
        b.extend_from_slice(&4u16.to_le_bytes());
        b.push((1 << 2) | 3);
        b.extend_from_slice(&4u32.to_le_bytes());
        unsnappy(&b, &mut out, MAX_BODY).unwrap();
        assert_eq!(out, b"wxyzwxyz");
        // A long literal's length in the bytes after its tag.
        let long: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let mut b = Vec::new();
        varint::put(&mut b, 300);
        b.push(61 << 2);
        b.extend_from_slice(&299u16.to_le_bytes());
        b.extend_from_slice(&long);
        unsnappy(&b, &mut out, MAX_BODY).unwrap();
        assert_eq!(out, long);
        assert_eq!(unsnappy(&snappy_literal(b"hello"), &mut out, MAX_BODY), Ok(()));
        // What is not Snappy: an offset before the start, a length past the declared, a body short.
        for bad in [vec![4, 0b0000_0001, 1], vec![1, 4 << 2, b'a', b'b'], vec![10, 2 << 2, b'a']] {
            assert!(unsnappy(&bad, &mut out, MAX_BODY).is_err(), "{bad:?}");
        }
        let mut huge = Vec::new();
        varint::put(&mut huge, (MAX_BODY + 1) as u64);
        assert_eq!(unsnappy(&huge, &mut out, MAX_BODY), Err(SnappyError::TooLarge));
        // Anything at all: an error or bytes, never a panic.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 40) as usize;
            let junk: Vec<u8> = (0..n).map(|i| (x >> (i % 8 * 8)) as u8 ^ (i as u8)).collect();
            let _ = unsnappy(&junk, &mut out, 1 << 16);
        }
    }

    fn label(w: &mut Writer, field: u32, n: &str, v: &str) {
        w.message(field, |l| {
            l.str(1, n);
            l.str(2, v);
        });
    }

    fn sample(w: &mut Writer, field: u32, v: f64, at: i64) {
        w.message(field, |s| {
            s.double(1, v);
            s.varint(2, at as u64);
        });
    }

    /// The series of a decoding: name and tags (`k:v`), kind, samples.
    fn read(d: &Decoded) -> Vec<(String, Kind, Vec<(i64, f64)>)> {
        d.keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let p = crate::batch::parse_key(&k.key).unwrap();
                let tags: Vec<String> = p.tags.iter().map(|(a, b)| format!("{a}:{b}")).collect();
                (format!("{}{{{}}}", p.metric, tags.join(",")), k.kind, d.samples.iter().filter(|s| s.0 as usize == i).map(|s| (s.1, s.2)).collect())
            })
            .collect()
    }

    #[test]
    fn a_1_0_request_writes_its_series_as_a_scrape_would() {
        let mut w = Writer::default();
        // A counter, by its name; a gauge of a family metadata calls a counter (learned first,
        // wherever it is in the message); a histogram's bucket; a staleness marker; a series with
        // no name; a label without a value.
        w.message(1, |ts| {
            label(ts, 1, "__name__", "http_requests_total");
            label(ts, 1, "job", "api");
            label(ts, 1, "instance", "10.0.0.1:9100");
            label(ts, 1, "empty", "");
            sample(ts, 2, 10.0, 1_000);
            sample(ts, 2, 12.0, 16_000);
        });
        w.message(1, |ts| {
            label(ts, 1, "__name__", "jobs_done");
            sample(ts, 2, 3.0, 1_000);
            sample(ts, 2, f64::from_bits(0x7ff0_0000_0000_0002), 16_000);
        });
        w.message(1, |ts| {
            label(ts, 1, "__name__", "lat_seconds_bucket");
            label(ts, 1, "le", "0.5");
            sample(ts, 2, 4.0, 1_000);
        });
        w.message(1, |ts| {
            label(ts, 1, "job", "nameless");
            sample(ts, 2, 1.0, 1_000);
        });
        w.message(3, |m| {
            m.varint(1, 1);
            m.str(2, "jobs_done");
        });
        let (keys, families, mut d) = (Keys::new(), Families::new(), Decoded::default());
        let t = decode_into(&w.buf, Version::V1, &keys, &families, &mut d).unwrap();
        assert_eq!(
            read(&d),
            vec![
                ("http_requests_total{instance:10.0.0.1:9100,job:api}".into(), Kind::Counter, vec![(1_000, 10.0), (16_000, 12.0)]),
                ("jobs_done{}".into(), Kind::Counter, vec![(1_000, 3.0)]),
                ("lat_seconds_bucket{le:0.5}".into(), Kind::Counter, vec![(1_000, 4.0)]),
            ]
        );
        assert_eq!((t.series, t.samples, t.stale, t.invalid_series, t.skipped_samples, t.metadata), (3, 4, 1, 1, 1, 1));
        // The same request again: its keys found, not made.
        let before = keys.len();
        decode_into(&w.buf, Version::V1, &keys, &families, &mut d).unwrap();
        assert_eq!(keys.len(), before);
    }

    #[test]
    fn a_2_0_request_reads_its_interned_labels_and_its_series_types() {
        let mut w = Writer::default();
        for s in ["", "__name__", "temp_celsius", "room", "kitchen", "doors_opened", "le"] {
            w.str(4, s);
        }
        w.message(5, |ts| {
            let mut refs = Writer::default();
            for r in [1u64, 2, 3, 4] {
                refs.buf.push(r as u8);
            }
            ts.bytes(1, &refs.buf);
            sample(ts, 2, 21.5, 5_000);
            ts.message(5, |m| m.varint(1, 2));
        });
        w.message(5, |ts| {
            let mut refs = Writer::default();
            for r in [1u64, 5] {
                refs.buf.push(r as u8);
            }
            ts.bytes(1, &refs.buf);
            sample(ts, 2, 7.0, 5_000);
            ts.message(5, |m| m.varint(1, 1));
            ts.message(3, |_| {});
        });
        // A reference past the table: the request is refused.
        let mut bad = Writer { buf: w.buf.clone() };
        bad.message(5, |ts| ts.bytes(1, &[1, 99]));
        let (keys, families, mut d) = (Keys::new(), Families::new(), Decoded::default());
        let t = decode_into(&w.buf, Version::V2, &keys, &families, &mut d).unwrap();
        assert_eq!(read(&d), vec![("temp_celsius{room:kitchen}".into(), Kind::Gauge, vec![(5_000, 21.5)]), ("doors_opened{}".into(), Kind::Counter, vec![(5_000, 7.0)])]);
        assert_eq!((t.series, t.samples, t.histograms), (2, 2, 1));
        assert_eq!(decode_into(&bad.buf, Version::V2, &keys, &families, &mut d), Err(WriteError::Symbol));
    }

    /// A native histogram, Prometheus's own encoding: schema, spans (offset, length) and counts as
    /// the change from the one before (sint64, zigzag).
    #[allow(clippy::too_many_arguments)]
    fn histogram(w: &mut Writer, field: u32, count: u64, sum: f64, schema: i64, spans: &[(i64, u64)], deltas: &[i64], reset: u64, at: i64, float: bool) {
        w.message(field, |h| {
            if float {
                h.double(2, count as f64);
            } else {
                h.varint(1, count);
            }
            h.double(3, sum);
            h.varint(4, ((schema << 1) ^ (schema >> 63)) as u64);
            for &(offset, len) in spans {
                h.message(11, |s| {
                    s.varint(1, ((offset << 1) ^ (offset >> 63)) as u64);
                    s.varint(2, len);
                });
            }
            let mut packed = Vec::new();
            for &d in deltas {
                crate::varint::put(&mut packed, ((d << 1) ^ (d >> 63)) as u64);
            }
            h.bytes(12, &packed);
            if reset > 0 {
                h.varint(14, reset);
            }
            h.varint(15, at as u64);
        });
    }

    /// A native histogram is a distribution: a counter's buckets what rose since its sample before
    /// (its first, of a series begun before the receiver, only what it rises from), its index one
    /// past OpenTelemetry's; a gauge's as it is; a float one not yet taken.
    #[test]
    fn native_histograms_are_distributions() {
        let (keys, families, mut d) = (Keys::new(), Families::new(), Decoded::default());
        let send = |hist: &dyn Fn(&mut Writer), d: &mut Decoded| {
            let mut w = Writer::default();
            w.message(1, |ts| {
                label(ts, 1, "__name__", "rpc_seconds");
                label(ts, 1, "job", "api");
                hist(ts);
            });
            decode_into(&w.buf, Version::V1, &keys, &families, d).unwrap()
        };
        // Buckets (1,2], (2,4], (4,8] (indices 1 to 3 at schema 0): 2, 3 and 1, then 4, 5 and 1.
        let t = send(&|ts| histogram(ts, 4, 6, 20.0, 0, &[(1, 3)], &[2, 1, -2], 0, 1_000, false), &mut d);
        assert_eq!((t.histograms, t.histograms_taken, read(&d).len()), (1, 1, 0), "a first sample: what came before not known");
        send(&|ts| histogram(ts, 4, 10, 35.0, 0, &[(1, 3)], &[4, 1, -4], 0, 16_000, false), &mut d);
        let got = read(&d);
        let tag = |i: i64| dist::tag_value(0, dist::Bucket::Pos(i));
        assert_eq!(
            got,
            vec![
                (format!("rpc_seconds.dist{{dist_bucket:{},job:api}}", tag(0)), Kind::Count, vec![(16_000, 2.0)]),
                (format!("rpc_seconds.dist{{dist_bucket:{},job:api}}", tag(1)), Kind::Count, vec![(16_000, 2.0)]),
                ("rpc_seconds.count{job:api}".into(), Kind::Count, vec![(16_000, 4.0)]),
                ("rpc_seconds.sum{job:api}".into(), Kind::Count, vec![(16_000, 15.0)]),
            ]
        );
        // A gauge histogram (reset hint 3) as it is, its finer schema made kiln's (8 to 5).
        send(&|ts| histogram(ts, 4, 2, 3.0, 8, &[(0, 2)], &[1, 0], 3, 31_000, false), &mut d);
        let got = read(&d);
        assert_eq!(got.iter().filter(|g| g.0.starts_with("rpc_seconds.dist")).map(|g| g.2.iter().map(|p| p.1).sum::<f64>()).sum::<f64>(), 2.0, "{got:?}");
        assert!(got.iter().all(|g| !g.0.starts_with("rpc_seconds.dist") || g.0.contains("dist_bucket:5:")), "{got:?}");
        // A float one: counted, not taken.
        let t = send(&|ts| histogram(ts, 4, 2, 3.0, 0, &[(0, 1)], &[2], 0, 46_000, true), &mut d);
        assert_eq!((t.histograms, t.histograms_taken), (1, 0));
    }

    #[test]
    fn content_types_name_the_protocol() {
        assert_eq!(Version::of("application/x-protobuf"), Some(Version::V1));
        assert_eq!(Version::of("application/x-protobuf;proto=prometheus.WriteRequest"), Some(Version::V1));
        assert_eq!(Version::of("application/x-protobuf; proto=io.prometheus.write.v2.Request"), Some(Version::V2));
        assert_eq!(Version::of("application/x-protobuf;proto=something.Else"), None);
        assert_eq!(Version::of("application/json"), None);
    }

    /// Hostile bodies: an error or series, never a panic.
    #[test]
    fn whatever_comes_is_an_error_or_series() {
        let (keys, families, mut d) = (Keys::new(), Families::new(), Decoded::default());
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 64) as usize;
            let junk: Vec<u8> = (0..n).map(|i| (x >> (i % 8 * 8)) as u8 ^ (i as u8).wrapping_mul(31)).collect();
            let _ = decode_into(&junk, Version::V1, &keys, &families, &mut d);
            let _ = decode_into(&junk, Version::V2, &keys, &families, &mut d);
        }
    }
}
