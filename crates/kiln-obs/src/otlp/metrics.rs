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
use crate::dist::{self, Bucket};
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

/// A request's series and their samples: each series' key once, and the samples flat, each with
/// its series' place in `keys` (a request allocates for its samples once, not once a series).
#[derive(Debug, Default)]
pub struct Decoded {
    pub keys: Vec<SeriesKey>,
    /// Each sample's series (its place in `keys`), time and value, in the request's order.
    pub samples: Vec<(u32, i64, f64)>,
    pub points: u64,
    pub dropped: Dropped,
    /// What decoding works with, kept with it for the next request ([`decode_into`]).
    scratch: Scratch,
}

/// A decoding's buffers that hold nothing of a request once it is decoded.
#[derive(Debug, Default)]
struct Scratch {
    found: HashTable<(u64, usize)>,
    key: Vec<u8>,
    counts: Vec<u64>,
    bounds: Vec<f64>,
}

impl Decoded {
    /// The bytes its buffers hold room for (what keeping it for the next request keeps).
    pub fn capacity_bytes(&self) -> usize {
        let s = &self.scratch;
        self.keys.capacity() * std::mem::size_of::<SeriesKey>()
            + self.samples.capacity() * std::mem::size_of::<(u32, i64, f64)>()
            + s.found.capacity() * std::mem::size_of::<(u64, usize)>()
            + s.key.capacity()
            + s.counts.capacity() * 8
            + s.bounds.capacity() * 8
    }

    /// Each series with its samples, in the order series came.
    pub fn series(&self) -> Vec<(SeriesKey, Vec<(i64, f64)>)> {
        let mut out: Vec<(SeriesKey, Vec<(i64, f64)>)> = self.keys.iter().map(|k| (k.clone(), Vec::new())).collect();
        for &(i, ts, v) in &self.samples {
            if let Some(s) = out.get_mut(i as usize) {
                s.1.push((ts, v));
            }
        }
        out
    }
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
    /// When the receiver began (Unix ns): a cumulative distribution begun after it is new to it.
    started_ns: u64,
    /// Cumulative distributions' last buckets, to give each next point's as a delta.
    cumulative: Vec<Mutex<CumShard>>,
}

/// A cumulative distribution's last buckets by its series' hash: the OpenTelemetry Collector's
/// `cumulativetodelta` (and the Datadog Agent's) for distributions, whose buckets, each a series of
/// its own, would otherwise lose the counts of a bucket's first appearance (a series' first point
/// has no increase). It holds as long as an exporter's series come to one receiver, as an app's to
/// its runner's on its machine.
#[derive(Default)]
struct CumShard {
    last: std::collections::HashMap<u64, Cum>,
    bytes: usize,
}

struct Cum {
    start_ns: u64,
    at_ns: u64,
    scale: i32,
    buckets: Vec<(Bucket, u64)>,
    count: u64,
    sum: f64,
}

/// What the cumulative distributions' state may hold, all shards: past a shard's share it starts
/// again (each series then loses one interval).
const CUMULATIVE_BYTES: usize = 32 << 20;

#[derive(Default)]
struct Shard {
    /// Each key with its hash: a table that grows moves its entries without hashing a key again.
    table: HashTable<(u64, SeriesKey)>,
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
        let started_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        Self::started_at(started_ns)
    }

    /// Keys of a receiver begun at `started_ns` (Unix ns).
    pub fn started_at(started_ns: u64) -> Self {
        Self { hash: Keyed::new(), shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(), started_ns, cumulative: (0..SHARDS).map(|_| Mutex::new(CumShard::default())).collect() }
    }

    /// A cumulative distribution's point (`buckets` at `scale`, its `count` and `sum`) as the delta
    /// since its last: its buckets into `out`, its count and sum returned; none when it is the
    /// first this receiver has of a series begun before it (what came before is not known) or a
    /// point it has had. Its count and sum are the same interval's as its buckets, so a
    /// distribution's average and its percentiles are of the same values.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn delta(&self, id: u64, start_ns: u64, at_ns: u64, scale: i32, buckets: &[(Bucket, u64)], count: u64, sum: f64, out: &mut Vec<(Bucket, u64)>) -> Option<(u64, f64)> {
        out.clear();
        let shard = self.cumulative.get((id >> 60) as usize % SHARDS)?;
        let mut guard = shard.lock().unwrap_or_else(PoisonError::into_inner);
        let CumShard { last: kept, bytes: held } = &mut *guard;
        let entry = |scale: i32| Cum { start_ns, at_ns, scale, buckets: buckets.to_vec(), count, sum };
        let new_bytes = std::mem::size_of_val(buckets) + std::mem::size_of::<Cum>();
        match kept.get_mut(&id) {
            Some(c) if c.start_ns == start_ns && at_ns <= c.at_ns => None,
            Some(c) if c.start_ns == start_ns && scale <= c.scale => {
                // The last at this point's scale (an SDK only ever makes a cumulative one coarser),
                // each bucket's increase; a fall anywhere is a reset: all of it is new.
                let k = u32::try_from(c.scale - scale).unwrap_or(0);
                let mut last: Vec<(Bucket, u64)> = Vec::with_capacity(c.buckets.len());
                for &(b, n) in &c.buckets {
                    let b = b.coarser(k);
                    match last.last_mut() {
                        Some((l, m)) if *l == b => *m = m.saturating_add(n),
                        _ => last.push((b, n)),
                    }
                }
                let mut fell = false;
                let mut j = 0;
                for &(b, n) in buckets {
                    while last.get(j).is_some_and(|(l, _)| l.order() < b.order()) {
                        fell = true;
                        j += 1;
                    }
                    let before = match last.get(j) {
                        Some(&(l, m)) if l == b => {
                            j += 1;
                            m
                        }
                        _ => 0,
                    };
                    if n < before {
                        fell = true;
                    }
                    if n > before {
                        out.push((b, n - before));
                    }
                }
                let reset = fell || j < last.len() || count < c.count;
                let since = if reset {
                    out.clear();
                    out.extend_from_slice(buckets);
                    (count, sum)
                } else {
                    (count - c.count, sum - c.sum)
                };
                *held = held.saturating_sub(c.buckets.len() * std::mem::size_of::<(Bucket, u64)>()).saturating_add(std::mem::size_of_val(buckets));
                *c = entry(scale);
                Some(since)
            }
            _ => {
                // First seen here, or begun again: what it holds is new if it began after this
                // receiver did.
                let fresh = start_ns >= self.started_ns;
                if fresh {
                    out.extend_from_slice(buckets);
                }
                if held.saturating_add(new_bytes) > CUMULATIVE_BYTES / SHARDS {
                    kept.clear();
                    *held = 0;
                }
                if let Some(old) = kept.insert(id, entry(scale)) {
                    *held = held.saturating_sub(old.buckets.len() * std::mem::size_of::<(Bucket, u64)>() + std::mem::size_of::<Cum>());
                }
                *held += new_bytes;
                fresh.then_some((count, sum))
            }
        }
    }

    /// The key of `key`'s bytes and `kind` (`h`, their hash): found, or made by `make` (outside
    /// the shard's lock) and kept. None when `make` finds it is no key.
    fn get_or_make(&self, h: u64, key: &[u8], kind: Kind, make: impl FnOnce() -> Option<SeriesKey>) -> Option<SeriesKey> {
        let shard = self.shards.get((h >> 60) as usize % SHARDS)?;
        if let Some((_, k)) = shard.lock().unwrap_or_else(PoisonError::into_inner).table.find(h, |(eh, k)| *eh == h && k.kind == kind && *k.key == *key) {
            return Some(k.clone());
        }
        let made = make()?;
        let mut s = shard.lock().unwrap_or_else(PoisonError::into_inner);
        if s.table.len() >= KEYS_MAX / SHARDS || s.bytes + made.key.len() > KEY_BYTES_MAX / SHARDS {
            s.table.clear();
            s.bytes = 0;
        }
        // Another thread may have made it meanwhile: one is kept.
        if s.table.find(h, |(eh, k)| *eh == h && k.kind == kind && *k.key == *key).is_none() {
            s.bytes += made.key.len();
            s.table.insert_unique(h, (h, made.clone()), |e| e.0);
        }
        Some(made)
    }

    /// The key of `metric` with `tags` (sorted by key, each key once) and `kind`: found, or made
    /// once in the receiver's life. `key` is room for its canonical bytes, kept by the caller.
    /// None when no series can be so (a name too long, too many tags).
    pub fn key(&self, metric: &str, tags: &[(&str, &str)], kind: Kind, key: &mut Vec<u8>) -> Option<SeriesKey> {
        put_canonical_head(key, metric, tags.len());
        for (k, v) in tags {
            put_canonical_tag(key, k, v);
        }
        let h = key_hash(&self.hash, key, kind);
        self.get_or_make(h, key, kind, || SeriesKey::new(metric, tags.iter().copied(), kind).ok())
    }

    /// What a series' state is kept by ([`Keys::delta`]): its canonical key's hash, its kind
    /// apart. `key` is room for the bytes.
    pub(crate) fn identity(&self, metric: &str, tags: &[(&str, &str)], key: &mut Vec<u8>) -> u64 {
        put_canonical_head(key, metric, tags.len());
        for (k, v) in tags {
            put_canonical_tag(key, k, v);
        }
        key_hash(&self.hash, key, Kind::Count)
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
    /// This request's series by their keys' hashes: each hash and its place in `out.keys` (kept
    /// beside it, so a table that grows hashes no key again).
    found: HashTable<(u64, usize)>,
    /// A point's attributes as read; its tags, merged and sorted; its canonical key.
    attrs: Vec<Tag<'a>>,
    tags: Vec<Tag<'a>>,
    key: Vec<u8>,
    /// A histogram point's counts and bounds, a summary point's quantiles.
    counts: Vec<u64>,
    bounds: Vec<f64>,
    /// An exponential histogram point's buckets, coarsened, and a cumulative one's delta.
    buckets: Vec<(Bucket, u64)>,
    delta: Vec<(Bucket, u64)>,
}

impl<'a> Sink<'a, '_> {
    /// The series the point's tags make of `metric`, as its canonical key's hash: what a cumulative
    /// distribution's state is kept by.
    fn identity(&mut self, metric: &str) -> u64 {
        let Self { keys, tags, key, .. } = self;
        put_canonical_head(key, metric, tags.len());
        for (k, v) in tags.iter() {
            put_canonical_tag(key, k, v);
        }
        key_hash(&keys.hash, key, Kind::Count)
    }

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
        let made = &mut out.keys;
        let place = match found.find(h, |&(eh, i)| eh == h && made.get(i).is_some_and(|k| k.kind == kind && *k.key == key[..])) {
            Some(&(_, i)) => i,
            None => {
                let Some(k) = keys.get_or_make(h, key, kind, || SeriesKey::new(metric, merged(), kind).ok()) else {
                    out.dropped.invalid += 1;
                    return;
                };
                debug_assert_eq!(*k.key, key[..], "the canonical key as SeriesKey makes it");
                found.insert_unique(h, (h, made.len()), |e| e.0);
                made.push(k);
                made.len() - 1
            }
        };
        out.samples.push((place as u32, ts, v));
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
    let mut out = Decoded::default();
    decode_into(body, now_ms, keys, &mut out)?;
    Ok(out)
}

/// [`decode_with`] into `out`, emptied first and its room kept: a receiver that hands its
/// [`Decoded`] back once its samples are taken decodes request after request without allocating
/// for them.
pub fn decode_into(body: &[u8], now_ms: i64, keys: &Keys, out: &mut Decoded) -> Result<(), OtlpError> {
    let mut d = std::mem::take(out);
    d.keys.clear();
    d.samples.clear();
    d.points = 0;
    d.dropped = Dropped::default();
    // A sample takes some 32 bytes of a request: room for them all at once, most of the time.
    d.keys.reserve(body.len() / 32);
    d.samples.reserve(body.len() / 32);
    let Scratch { mut found, key, counts, bounds } = std::mem::take(&mut d.scratch);
    found.clear();
    let mut sink = Sink { out: d, keys, found, attrs: Vec::new(), tags: Vec::new(), key, counts, bounds, buckets: Vec::new(), delta: Vec::new() };
    let mut read = || -> Result<(), OtlpError> {
        for f in Fields::new(body) {
            let (n, w) = f?;
            if n == 1 {
                resource_metrics(w.bytes().unwrap_or(&[]), now_ms, &mut sink)?;
            }
        }
        Ok(())
    };
    let done = read();
    let Sink { out: mut d, found, key, counts, bounds, .. } = sink;
    d.scratch = Scratch { found, key, counts, bounds };
    *out = d;
    done
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
    /// A distribution's buckets' (`crate::dist`).
    dist: String,
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
            dist: format!("{name}{}", dist::SUFFIX),
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
    let (mut t, mut start, mut flags) = (0u64, 0u64, 0u64);
    let (mut count, mut sum, mut min, mut max, mut scale, mut zero) = (None, None, None, None, 0i64, 0u64);
    let (mut pos_off, mut pos, mut neg_off, mut neg) = (0i64, Vec::new(), 0i64, Vec::new());
    sink.attrs.clear();
    for f in Fields::new(msg) {
        match f? {
            (1, w) => sink.attr(w.bytes().unwrap_or(&[]))?,
            (2, w) => start = w.u64().unwrap_or(0),
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
    // Its buckets, a distribution's: each a count of its interval, a series of its own, merged with
    // others' and read as percentiles at query time (`crate::dist`); its count and sum the same
    // interval's (a cumulative one's turned to deltas with them), its least and most as they came.
    let mut buckets = std::mem::take(&mut sink.buckets);
    let at = dist::coarsened(i32::try_from(scale).unwrap_or(0), zero, pos_off, &pos, neg_off, &neg, &mut buckets);
    let mut delta = std::mem::take(&mut sink.delta);
    let (counts, since): (&[(Bucket, u64)], Option<(u64, f64)>) = if count_kind == Kind::Counter {
        let id = sink.identity(&d.dist);
        let since = sink.keys.delta(id, start, t.max(1), at, &buckets, count.unwrap_or(0), sum.unwrap_or(0.0), &mut delta);
        (&delta, since)
    } else {
        (&buckets, Some((count.unwrap_or(0), sum.unwrap_or(0.0))))
    };
    if let Some((c, s)) = since {
        if count.is_some() {
            sink.point(&d.count, None, Kind::Count, ts, c as f64);
        }
        if sum.is_some() {
            sink.point(&d.sum, None, Kind::Count, ts, s);
        }
    }
    if let Some(v) = min {
        sink.point(&d.min, None, Kind::Gauge, ts, v);
    }
    if let Some(v) = max {
        sink.point(&d.max, None, Kind::Gauge, ts, v);
    }
    for &(b, n) in counts {
        let tag = dist::tag_value(at, b);
        sink.point(&d.dist, Some((dist::BUCKET_TAG, &tag)), Kind::Count, ts, n as f64);
    }
    sink.buckets = buckets;
    sink.delta = delta;
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

    fn find(d: &Decoded, metric: &str, tag: Option<(&str, &str)>) -> (SeriesKey, Vec<(i64, f64)>) {
        d.series()
            .into_iter()
            .find(|(k, _)| {
                let parsed = crate::batch::parse_key(&k.key).unwrap();
                parsed.metric == metric && tag.is_none_or(|t| parsed.tags.contains(&t))
            })
            .unwrap_or_else(|| panic!("no {metric} {tag:?}"))
    }

    /// A decoding kept from request to request reads each as a new one would: nothing of the last
    /// left in it, a failed request's part included.
    #[test]
    fn a_kept_decoding_reads_each_request_as_a_new_one_would() {
        let keys = Keys::new();
        let full = request();
        let mut small = Writer::default();
        small.message(1, |rm| {
            rm.message(2, |sm| {
                sm.message(2, |m| {
                    m.str(1, "solo");
                    m.message(5, |g| {
                        g.message(1, |p| {
                            p.fixed64(3, 1_700_000_000_000_000_000);
                            p.double(4, 7.0);
                        });
                    });
                });
            });
        });
        let small = small.buf;
        let fresh = |b: &[u8]| decode_with(b, 0, &keys).unwrap();
        let same = |a: &Decoded, b: &Decoded| a.keys.iter().map(|k| &*k.key).eq(b.keys.iter().map(|k| &*k.key)) && a.samples == b.samples && a.points == b.points && a.dropped == b.dropped;
        let mut kept = Decoded::default();
        for b in [&full, &small, &full, &full] {
            decode_into(b, 0, &keys, &mut kept).unwrap();
            assert!(same(&kept, &fresh(b)));
        }
        // A request cut short fails; the next reads clean.
        assert!(decode_into(&full[..full.len() / 2], 0, &keys, &mut kept).is_err());
        decode_into(&small, 0, &keys, &mut kept).unwrap();
        assert!(same(&kept, &fresh(&small)));
        assert!(kept.capacity_bytes() > 0);
    }

    #[test]
    fn every_metric_type_maps_to_series() {
        let d = decode(&request(), 5).unwrap();
        let (k, s) = find(&d, "temp", Some(("room", "a")));
        assert_eq!(k.kind, Kind::Gauge);
        assert_eq!(s, vec![(1_700_000_000_000, 21.5)]);
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
        // All 100 in the bucket (8, 16] at scale 0: a distribution's bucket, its count.
        assert_eq!(find(&d, "size.dist", Some((dist::BUCKET_TAG, "0:p3"))).1, vec![(1_700_000_000_000, 100.0)]);
        assert_eq!(find(&d, "rpc.quantile", Some(("quantile", "0.99"))).1[0].1, 9.5);
        assert_eq!(find(&d, "rpc.count", None).1[0].1, 7.0);
    }

    /// One exponential histogram point of `lat` (host `h`): its temporality, start and time (ns),
    /// scale, positive buckets from `pos.0` and negative ones from `neg.0`.
    fn exponential(cumulative: bool, start: u64, t: u64, scale: i64, pos: (i64, &[u64]), neg: (i64, &[u64])) -> Vec<u8> {
        let mut w = Writer::default();
        w.message(1, |rm| {
            rm.message(1, |r| kv(r, 1, "host.name", "h"));
            rm.message(2, |sm| {
                sm.message(2, |m| {
                    m.str(1, "lat");
                    m.message(10, |h| {
                        h.message(1, |p| {
                            p.fixed64(2, start);
                            p.fixed64(3, t);
                            p.fixed64(4, pos.1.iter().chain(neg.1).sum());
                            p.varint(6, crate::varint::zig(scale));
                            p.message(8, |b| {
                                b.varint(1, crate::varint::zig(pos.0));
                                for &c in pos.1 {
                                    b.varint(2, c);
                                }
                            });
                            p.message(9, |b| {
                                b.varint(1, crate::varint::zig(neg.0));
                                for &c in neg.1 {
                                    b.varint(2, c);
                                }
                            });
                        });
                        h.varint(2, if cumulative { 2 } else { 1 });
                    });
                });
            });
        });
        w.buf
    }

    /// A decoding's distribution buckets: each bucket's tag and its counts' sum, ascending by value.
    fn dist_buckets(d: &Decoded) -> Vec<(String, f64)> {
        let mut out: Vec<(crate::dist::Bucket, String, f64)> = d
            .series()
            .into_iter()
            .filter_map(|(k, pts)| {
                let p = crate::batch::parse_key(&k.key).ok()?;
                let tag = p.tags.iter().find(|(tk, _)| *tk == dist::BUCKET_TAG)?.1.to_owned();
                (p.metric == "lat.dist").then(|| (dist::parse_tag_value(&tag).unwrap().1, tag, pts.iter().map(|x| x.1).sum()))
            })
            .collect();
        out.sort_by_key(|(b, _, _)| b.order());
        out.into_iter().map(|(_, t, n)| (t, n)).collect()
    }

    /// An exponential histogram's buckets are a distribution's: each a series, its count an
    /// interval's, coarsened to scale 5 at the finest; a cumulative one's the increase since its
    /// last point, and nothing for its first when it began before the receiver did (what came
    /// before is not known); a fall, or a new start, is all of it anew.
    #[test]
    fn exponential_histograms_are_distributions_each_bucket_an_interval_s_count() {
        let decode = |b: &[u8], keys: &Keys| decode_with(b, 0, keys).unwrap();
        let tagged = |v: &[(&str, f64)]| v.iter().map(|(t, n)| ((*t).to_owned(), *n)).collect::<Vec<_>>();
        // Delta at scale 7: buckets 40..44 are 10, 10, 10, 10 and 11 at scale 5 (i >> 2), counts summed;
        // negatives ascending by value (bucket n11's values below n10's).
        let keys = Keys::started_at(1_000);
        let d = decode(&exponential(false, 0, 2_000_000_000, 7, (40, &[1, 2, 3, 4, 5]), (40, &[6, 0, 0, 0, 7])), &keys);
        assert_eq!(dist_buckets(&d), tagged(&[("5:n11", 7.0), ("5:n10", 6.0), ("5:p10", 10.0), ("5:p11", 5.0)]));
        // Cumulative, begun before the receiver: the first point tells nothing of an interval.
        let keys = Keys::started_at(10_000_000_000);
        let start = 5_000_000_000;
        let first = decode(&exponential(true, start, 20_000_000_000, 3, (8, &[2, 5]), (2, &[1])), &keys);
        assert!(dist_buckets(&first).is_empty());
        assert!(first.series().iter().all(|(k, _)| crate::batch::parse_key(&k.key).unwrap().metric != "lat.count"), "nor its count");
        // The next: each bucket's increase, a bucket new to it whole; its count the same interval's.
        let second = decode(&exponential(true, start, 30_000_000_000, 3, (8, &[3, 9, 1]), (2, &[4])), &keys);
        assert_eq!(dist_buckets(&second), tagged(&[("3:n2", 3.0), ("3:p8", 1.0), ("3:p9", 4.0), ("3:p10", 1.0)]));
        let count = find(&second, "lat.count", None);
        assert_eq!((count.0.kind, count.1[0].1), (Kind::Count, 9.0), "8 values, then 17: 9 in the interval");
        // The same point again (sent twice): nothing.
        assert!(dist_buckets(&decode(&exponential(true, start, 30_000_000_000, 3, (8, &[3, 9, 1]), (2, &[4])), &keys)).is_empty());
        // Coarser (the SDK widened its range): the last made as coarse (p8 and p9 are p4 at scale 2,
        // 12; p10 is p5; n2 is n1), then the increase.
        let coarser = decode(&exponential(true, start, 40_000_000_000, 2, (4, &[14, 1]), (1, &[5])), &keys);
        assert_eq!(dist_buckets(&coarser), tagged(&[("2:n1", 1.0), ("2:p4", 2.0)]));
        // A fall with no new start (a reset): all of it, anew.
        let fell = decode(&exponential(true, start, 50_000_000_000, 2, (4, &[2]), (1, &[])), &keys);
        assert_eq!(dist_buckets(&fell), tagged(&[("2:p4", 2.0)]));
        // Begun again, after the receiver: its first point is all new.
        let again = decode(&exponential(true, 45_000_000_000, 60_000_000_000, 2, (4, &[1]), (1, &[])), &keys);
        assert_eq!(dist_buckets(&again), tagged(&[("2:p4", 1.0)]));
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
