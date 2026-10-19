//! DogStatsD (`docs/observe.md` §8.4): the datagrams Datadog's DogStatsD server takes, read in
//! place, and their metrics aggregated as Datadog's Agent aggregates them.
//!
//! - A datagram is messages, a line each: a metric, `<name>:<value>[:<value>...]|<type>[|@<sample
//!   rate>][|#<tag>,<tag>...][|c:<container>][|T<seconds>]` (protocol 1.3, Datadog's "Datagram
//!   Format and Shell Usage"); an event, `_e{<title bytes>,<text bytes>}:<title>|<text>[|d:<seconds>]
//!   [|h:<host>][|k:<key>][|p:<priority>][|s:<source>][|t:<alert type>][|#<tags>]`; or a service
//!   check, `_sc|<name>|<status>[|d:<seconds>][|h:<host>][|#<tags>][|m:<message>]`. Fields kiln has
//!   no use for (an origin's `e:` and `card:`) are passed over, as the Agent passes over what it
//!   does not know.
//! - A metric's context is its name, how it aggregates, its host (a `host:` tag's, else the
//!   machine's) and its tags, in any order; a key given twice keeps its first value (a series has
//!   one value a key). A context's samples are aggregated by when they came, into buckets of ten
//!   seconds (the Agent's time sampler, `bucketSize`), and a bucket's points are written at its
//!   start once it is over:
//!   - a count (`c`): its values summed, each over its sample rate: a count a bucket (`.as_rate()`
//!     reads it a second, as Datadog shows a DogStatsD count). A count that stops is written as
//!     zero for five minutes after its last sample (the Agent's `dogstatsd_expiry_seconds`), so its
//!     graph falls to zero rather than ending where it stopped;
//!   - a gauge (`g`): its last value;
//!   - a set (`s`): how many distinct values;
//!   - a histogram or a timer (`h`, `ms`): the aggregates the Agent's `histogram_aggregates` and
//!     `histogram_percentiles` name (`max`, `median`, `avg`, `count` and the 95th percentile unless
//!     said; `min` and `sum` when asked): `<name>.max`, `.median`, `.avg`, `.count` (a count, each
//!     sample over its rate), `.95percentile`. The median and percentiles are the Agent's, nearest
//!     rank (the least value with at least that share of the samples' weight at or under it);
//!   - a distribution (`d`): kiln's distribution ([`crate::dist`]): its values' buckets, `.count`,
//!     `.sum`, `.min` and `.max`, merged exactly across hosts and time and read as percentiles.
//! - A gauge or count with a timestamp (`T`) is written at that time as it came, as the Agent
//!   forwards one unaggregated.
//! - Everything held is bounded: contexts ([`CONTEXTS`], a new one past it refused and counted),
//!   histogram samples held exactly ([`EXACT_SAMPLES`]; past it, a context's go into buckets and
//!   its percentiles are within 1.09%), a set's values ([`SET_VALUES`]) and a distribution's buckets
//!   ([`DIST_BUCKETS`]).
//!
//! Steady state allocates nothing a message: a context is found by a hash of its name, host and
//! tags (each tag hashed alone, so their order does not matter), and its series' keys are made once,
//! when it is first seen. What is written is gathered as an OTLP request's samples are
//! ([`crate::otlp::metrics::Decoded`]): each series' key once, the samples flat.

use std::borrow::Cow;
use std::collections::BTreeMap;

use hashbrown::HashTable;

use crate::batch::{Kind, SeriesKey};
use crate::dist::{self, Bucket};
use crate::hash::Keyed;
use crate::logs::record::Record;

/// The Agent's bucket: what a context's samples are aggregated over, by when they came.
pub const BUCKET_MS: i64 = 10_000;
/// How long a count that stopped is written as zero, and how long a context is kept after its
/// last sample: the Agent's `dogstatsd_expiry_seconds`, 300.
pub const EXPIRY_MS: i64 = 300_000;
/// Contexts held at the most; past it, a new context's samples are refused (counted). A node's
/// intake takes 3 Mi points waiting (three quarters of `WAITING_POINTS`); a bucket of this many
/// histograms' contexts, at five series each, is a fifth of it, so a flush is taken while other
/// writes wait.
pub const CONTEXTS: usize = 1 << 17;
/// Histogram samples held exactly, all contexts' together (16 MiB of them); and one context's. Past
/// either, a context's samples that bucket go into [`dist::SCALE`]'s buckets: its median and
/// percentiles then within 1.09% of the exact ones.
pub const EXACT_SAMPLES: usize = 1 << 20;
pub const EXACT_PER_CONTEXT: usize = 1 << 16;
/// A set's distinct values counted in a bucket at the most: past it, its count stays there (and
/// the values over are counted).
pub const SET_VALUES: usize = 1 << 16;
/// A distribution's (or a sketched histogram's) buckets at the most: the Agent's own sketches'
/// limit (`pkg/quantile`, `defaultBinLimit`, 4,096). A value past them counts in the nearest
/// bucket held, as DDSketch's bounded stores fold their extreme buckets.
pub const DIST_BUCKETS: usize = 4096;

/// A metric's type, as its message names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    Count,
    Gauge,
    Histogram,
    Timer,
    Set,
    Distribution,
}

/// How a context's samples are aggregated (a timer is a histogram).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Count,
    Gauge,
    Hist,
    Set,
    Dist,
}

impl Type {
    fn class(self) -> Class {
        match self {
            Self::Count => Class::Count,
            Self::Gauge => Class::Gauge,
            Self::Histogram | Self::Timer => Class::Hist,
            Self::Set => Class::Set,
            Self::Distribution => Class::Dist,
        }
    }
}

/// A metric's message, read in place.
#[derive(Debug, Clone, PartialEq)]
pub struct Metric<'a> {
    pub name: &'a str,
    /// Its values as sent, `:` between them (protocol 1.1's several values a message).
    pub values: &'a str,
    pub ty: Type,
    /// Its sample rate: 1 unless `@` says.
    pub rate: f64,
    /// Its tags as sent, `,` between them.
    pub tags: &'a str,
    pub container: Option<&'a str>,
    /// Seconds since the epoch (`T`, protocol 1.3).
    pub ts: Option<i64>,
}

/// An event's message.
#[derive(Debug, Clone, PartialEq)]
pub struct Event<'a> {
    pub title: Cow<'a, str>,
    pub text: Cow<'a, str>,
    pub ts: Option<i64>,
    pub host: Option<&'a str>,
    pub key: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub source: Option<&'a str>,
    pub alert: Option<&'a str>,
    pub tags: &'a str,
}

/// A service check's message.
#[derive(Debug, Clone, PartialEq)]
pub struct Check<'a> {
    pub name: &'a str,
    /// 0 OK, 1 WARNING, 2 CRITICAL, 3 UNKNOWN.
    pub status: u8,
    pub ts: Option<i64>,
    pub host: Option<&'a str>,
    pub tags: &'a str,
    pub message: Option<Cow<'a, str>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message<'a> {
    Metric(Metric<'a>),
    Event(Event<'a>),
    Check(Check<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("a metric without a name")]
    Name,
    #[error("a metric without a value")]
    Value,
    #[error("a metric's type: c, g, h, ms, s or d")]
    Type,
    #[error("a sample rate that is not a number over zero")]
    Rate,
    #[error("a timestamp that is not whole seconds")]
    Timestamp,
    #[error("an event's lengths do not match what it holds")]
    Event,
    #[error("a service check without a name, or its status not 0, 1, 2 or 3")]
    Check,
}

/// One message: a metric, an event or a service check.
pub fn parse(line: &str) -> Result<Message<'_>, ParseError> {
    if let Some(rest) = line.strip_prefix("_e{") {
        return parse_event(rest).map(Message::Event);
    }
    if let Some(rest) = line.strip_prefix("_sc|") {
        return parse_check(rest).map(Message::Check);
    }
    parse_metric(line).map(Message::Metric)
}

fn parse_metric(line: &str) -> Result<Metric<'_>, ParseError> {
    let mut fields = line.split('|');
    let head = fields.next().unwrap_or_default();
    let (name, values) = head.split_once(':').ok_or(if head.is_empty() { ParseError::Name } else { ParseError::Value })?;
    if name.is_empty() {
        return Err(ParseError::Name);
    }
    if values.is_empty() {
        return Err(ParseError::Value);
    }
    let ty = match fields.next() {
        Some("c") => Type::Count,
        Some("g") => Type::Gauge,
        Some("h") => Type::Histogram,
        Some("ms") => Type::Timer,
        Some("s") => Type::Set,
        Some("d") => Type::Distribution,
        _ => return Err(ParseError::Type),
    };
    let mut m = Metric { name, values, ty, rate: 1.0, tags: "", container: None, ts: None };
    for f in fields {
        if let Some(r) = f.strip_prefix('@') {
            m.rate = r.parse::<f64>().ok().filter(|r| r.is_finite() && *r > 0.0).ok_or(ParseError::Rate)?;
        } else if let Some(t) = f.strip_prefix('#') {
            m.tags = t;
        } else if let Some(c) = f.strip_prefix("c:") {
            m.container = Some(c);
        } else if let Some(t) = f.strip_prefix('T') {
            m.ts = Some(t.parse::<i64>().map_err(|_| ParseError::Timestamp)?);
        }
    }
    Ok(m)
}

/// An event's or check's text: `\n` as two characters is a line's end.
fn unescape(s: &str) -> Cow<'_, str> {
    if s.contains("\\n") { Cow::Owned(s.replace("\\n", "\n")) } else { Cow::Borrowed(s) }
}

fn parse_event(rest: &str) -> Result<Event<'_>, ParseError> {
    let (lens, rest) = rest.split_once("}:").ok_or(ParseError::Event)?;
    let (a, b) = lens.split_once(',').ok_or(ParseError::Event)?;
    let (title_len, text_len): (usize, usize) = (a.parse().map_err(|_| ParseError::Event)?, b.parse().map_err(|_| ParseError::Event)?);
    // Lengths in bytes, as the Agent reads them: each must end where a character does.
    let title = rest.get(..title_len).filter(|t| !t.is_empty()).ok_or(ParseError::Event)?;
    let after = rest.get(title_len..).and_then(|r| r.strip_prefix('|')).ok_or(ParseError::Event)?;
    let text = after.get(..text_len).ok_or(ParseError::Event)?;
    let tail = after.get(text_len..).ok_or(ParseError::Event)?;
    let mut e = Event { title: unescape(title), text: unescape(text), ts: None, host: None, key: None, priority: None, source: None, alert: None, tags: "" };
    if tail.is_empty() {
        return Ok(e);
    }
    let fields = tail.strip_prefix('|').ok_or(ParseError::Event)?;
    for f in fields.split('|') {
        if let Some(d) = f.strip_prefix("d:") {
            e.ts = Some(d.parse::<i64>().map_err(|_| ParseError::Timestamp)?);
        } else if let Some(h) = f.strip_prefix("h:") {
            e.host = Some(h);
        } else if let Some(k) = f.strip_prefix("k:") {
            e.key = Some(k);
        } else if let Some(p) = f.strip_prefix("p:") {
            e.priority = Some(p);
        } else if let Some(s) = f.strip_prefix("s:") {
            e.source = Some(s);
        } else if let Some(t) = f.strip_prefix("t:") {
            e.alert = Some(t);
        } else if let Some(t) = f.strip_prefix('#') {
            e.tags = t;
        }
    }
    Ok(e)
}

fn parse_check(rest: &str) -> Result<Check<'_>, ParseError> {
    let mut parts = rest.splitn(3, '|');
    let name = parts.next().filter(|n| !n.is_empty()).ok_or(ParseError::Check)?;
    let status = parts.next().and_then(|s| s.parse::<u8>().ok()).filter(|s| *s <= 3).ok_or(ParseError::Check)?;
    let mut c = Check { name, status, ts: None, host: None, tags: "", message: None };
    let mut at = parts.next().unwrap_or_default();
    while !at.is_empty() {
        // The message is the last field: what follows `m:` is its, `|` and all.
        if let Some(m) = at.strip_prefix("m:") {
            c.message = Some(unescape(m));
            break;
        }
        let (f, next) = at.split_once('|').unwrap_or((at, ""));
        if let Some(d) = f.strip_prefix("d:") {
            c.ts = Some(d.parse::<i64>().map_err(|_| ParseError::Timestamp)?);
        } else if let Some(h) = f.strip_prefix("h:") {
            c.host = Some(h);
        } else if let Some(t) = f.strip_prefix('#') {
            c.tags = t;
        }
        at = next;
    }
    Ok(c)
}

/// A message's tags: each `key:value`, a tag without a colon a key with no value; empty ones
/// passed over.
fn tag_pairs(raw: &str) -> impl Iterator<Item = (&str, &str)> {
    raw.split(',').filter(|t| !t.is_empty()).map(|t| t.split_once(':').unwrap_or((t, "")))
}

/// Tags the Agent keeps to itself (origin detection's), never a series'.
fn internal(key: &str) -> bool {
    key.starts_with("dd.internal.")
}

/// A histogram's aggregate (the Agent's `histogram_aggregates`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregate {
    Max,
    Median,
    Avg,
    Count,
    Min,
    Sum,
}

impl Aggregate {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "max" => Self::Max,
            "median" => Self::Median,
            "avg" => Self::Avg,
            "count" => Self::Count,
            "min" => Self::Min,
            "sum" => Self::Sum,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Median => "median",
            Self::Avg => "avg",
            Self::Count => "count",
            Self::Min => "min",
            Self::Sum => "sum",
        }
    }
}

/// What an aggregator is told: the machine its metrics are of, tags for every one, and what a
/// histogram is written as.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub host: String,
    pub tags: Vec<(String, String)>,
    pub aggregates: Vec<Aggregate>,
    /// Percentiles, in hundredths (95: `.95percentile`), 1 to 99.
    pub percentiles: Vec<u8>,
}

impl Default for Settings {
    /// The Agent's defaults: `histogram_aggregates` max, median, avg, count; `histogram_percentiles` 0.95.
    fn default() -> Self {
        Self { host: String::new(), tags: Vec::new(), aggregates: vec![Aggregate::Max, Aggregate::Median, Aggregate::Avg, Aggregate::Count], percentiles: vec![95] }
    }
}

/// What an aggregator took and wrote, counted since it began.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub datagrams: u64,
    pub metrics: u64,
    pub events: u64,
    pub checks: u64,
    /// Messages that did not read (or not as UTF-8), and values that are not numbers.
    pub invalid: u64,
    /// Samples refused: a new context past [`CONTEXTS`], or one whose series cannot be (a name or
    /// tag too long, too many tags).
    pub refused: u64,
    /// Tags whose key came again with another value (the first kept).
    pub conflicts: u64,
    /// Set values past [`SET_VALUES`] in a bucket.
    pub set_over: u64,
    /// Contexts held now.
    pub contexts: u64,
    /// Points written.
    pub points: u64,
}

/// A histogram's (or a distribution's) samples in a bucket.
#[derive(Debug, Default)]
struct Hist {
    count: f64,
    sum: f64,
    min: f64,
    max: f64,
    /// Samples (value, weight) held exactly, until [`EXACT_SAMPLES`] or [`EXACT_PER_CONTEXT`]
    /// says no more; then `sketched`, and every sample in `sketch`.
    exact: Vec<(f64, f64)>,
    sketched: bool,
    /// Weights by bucket of [`dist::SCALE`] (by `Bucket::order`), sorted.
    sketch: Vec<((u8, i64), f64)>,
}

/// A bucket from its place in the order ([`Bucket::order`]'s inverse).
fn bucket_at(order: (u8, i64)) -> Bucket {
    match order.0 {
        0 => Bucket::Neg(order.1.saturating_neg()),
        1 => Bucket::Zero,
        _ => Bucket::Pos(order.1),
    }
}

/// Adds `w` at `order` into a sorted sketch of at most [`DIST_BUCKETS`] buckets: past them, into
/// the nearest held.
fn sketch_add(sketch: &mut Vec<((u8, i64), f64)>, order: (u8, i64), w: f64) {
    match sketch.binary_search_by(|(o, _)| o.cmp(&order)) {
        Ok(i) => {
            if let Some(b) = sketch.get_mut(i) {
                b.1 += w;
            }
        }
        Err(i) if sketch.len() < DIST_BUCKETS => sketch.insert(i, (order, w)),
        Err(i) => {
            let nearest = i.min(sketch.len().saturating_sub(1));
            if let Some(b) = sketch.get_mut(nearest) {
                b.1 += w;
            }
        }
    }
}

impl Hist {
    fn clear(&mut self, held: &mut usize) {
        *held = held.saturating_sub(self.exact.len());
        self.exact.clear();
        self.sketch.clear();
        self.sketched = false;
        (self.count, self.sum, self.min, self.max) = (0.0, 0.0, 0.0, 0.0);
    }

    fn add(&mut self, v: f64, w: f64, exact_ok: bool, held: &mut usize) {
        if self.count == 0.0 {
            (self.min, self.max) = (v, v);
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.count += w;
        self.sum += v * w;
        if !self.sketched {
            if exact_ok && *held < EXACT_SAMPLES && self.exact.len() < EXACT_PER_CONTEXT {
                self.exact.push((v, w));
                *held += 1;
                return;
            }
            self.sketched = true;
            *held = held.saturating_sub(self.exact.len());
            for (x, xw) in std::mem::take(&mut self.exact) {
                if let Some(b) = dist::bucket_of(x, dist::SCALE) {
                    sketch_add(&mut self.sketch, b.order(), xw);
                }
            }
        }
        if let Some(b) = dist::bucket_of(v, dist::SCALE) {
            sketch_add(&mut self.sketch, b.order(), w);
        }
    }

    /// Its exact samples in order, for [`Hist::quantile`].
    fn sort(&mut self) {
        if !self.sketched {
            self.exact.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        }
    }

    /// The nearest-rank quantile `num / den` of the weight (its samples sorted first,
    /// [`Hist::sort`]): the least value with at least that share at or under it (the Agent's
    /// `samples[(p·n - 1) / 100]` when every weight is one).
    fn quantile(&self, num: u32, den: u32) -> f64 {
        let target = f64::from(num) * self.count / f64::from(den);
        if !self.sketched {
            let mut seen = 0.0;
            for &(v, w) in &self.exact {
                seen += w;
                if seen >= target {
                    return v;
                }
            }
            return self.max;
        }
        let mut seen = 0.0;
        for &(order, w) in &self.sketch {
            seen += w;
            if seen >= target {
                return bucket_at(order).value(dist::SCALE).clamp(self.min, self.max);
            }
        }
        self.max
    }
}

/// What a context holds of its bucket.
#[derive(Debug)]
enum State {
    Count { sum: f64, sampled: bool },
    Gauge { last: f64, sampled: bool },
    Set { seen: HashTable<u64>, n: usize },
    Hist(Box<Hist>),
}

/// A distribution's bucket's series, and where it is in the output of `round`.
#[derive(Debug)]
struct DistKey {
    key: SeriesKey,
    round: u32,
    at: u32,
}

#[derive(Debug)]
struct Context {
    name: Box<str>,
    class: Class,
    host: Box<str>,
    /// Its tags' hashes, sorted: with its name, class and host, what it is.
    ident: Box<[u64]>,
    hash: u64,
    /// Its series: a count's, gauge's or set's one; a histogram's aggregates', then its
    /// percentiles'; a distribution's `.count`, `.sum`, `.min`, `.max`.
    keys: Box<[SeriesKey]>,
    /// A distribution's buckets' series (made as its buckets are first written), and the tags
    /// they are made with.
    dist: BTreeMap<(u8, i64), DistKey>,
    dist_tags: Vec<(String, String)>,
    /// The bucket its state is of (its start, ms); when its last sample aggregated came, and its
    /// last of any kind (what it expires by).
    bucket: i64,
    last_ms: i64,
    seen_ms: i64,
    state: State,
    /// Where its series are in the output of `out_round`.
    out_round: u32,
    out_at: u32,
}

/// What is to be written: each series' key once, its samples flat (as
/// [`crate::otlp::metrics::Decoded`] holds them).
#[derive(Debug, Default)]
struct Out {
    keys: Vec<SeriesKey>,
    samples: Vec<(u32, i64, f64)>,
    round: u32,
    /// A histogram's values as they are worked out, its room kept from flush to flush.
    vals: Vec<f64>,
}

impl Out {
    /// Context `c`'s series `i` at `ts`.
    fn put(&mut self, c: &mut Context, i: usize, ts: i64, v: f64) {
        if c.out_round != self.round {
            c.out_round = self.round;
            c.out_at = self.keys.len() as u32;
            self.keys.extend(c.keys.iter().cloned());
        }
        self.samples.push((c.out_at + i as u32, ts, v));
    }
}

/// DogStatsD's metrics aggregated, a bucket at a time; its events and service checks made records.
#[derive(Debug)]
pub struct Aggregator {
    s: Settings,
    hasher: Keyed,
    contexts: Vec<Option<Context>>,
    free: Vec<u32>,
    /// Each context's hash and slot.
    table: HashTable<(u64, u32)>,
    /// Histogram samples held exactly, every context's.
    held: usize,
    out: Out,
    /// Buckets before this one are written.
    flushed: i64,
    /// A message's tags kept: each one's hash, and where its key lies in the message's tags.
    scratch: Vec<(u64, u32, u32)>,
    stats: Stats,
}

/// The bucket `ms` falls in: its start.
pub fn bucket_of(ms: i64) -> i64 {
    ms - ms.rem_euclid(BUCKET_MS)
}

impl Aggregator {
    pub fn new(s: Settings) -> Self {
        Self { s, hasher: Keyed::new(), contexts: Vec::new(), free: Vec::new(), table: HashTable::new(), held: 0, out: Out { round: 1, ..Out::default() }, flushed: i64::MIN, scratch: Vec::new(), stats: Stats::default() }
    }

    pub fn settings(&self) -> &Settings {
        &self.s
    }

    pub fn stats(&self) -> Stats {
        Stats { contexts: (self.contexts.len() - self.free.len()) as u64, ..self.stats }
    }

    /// A datagram's messages, come at `now_ms`: metrics aggregated, events and service checks
    /// made records (into `records`).
    pub fn take(&mut self, datagram: &[u8], now_ms: i64, records: &mut Vec<Record>) {
        self.stats.datagrams += 1;
        // UTF-8 checked once a datagram, as it nearly always is; else line by line, so one bad
        // line costs only itself.
        if let Ok(text) = std::str::from_utf8(datagram) {
            for line in text.split('\n') {
                self.line(line, now_ms, records);
            }
            return;
        }
        for line in datagram.split(|&b| b == b'\n') {
            match std::str::from_utf8(line) {
                Ok(line) => self.line(line, now_ms, records),
                Err(_) => self.stats.invalid += 1,
            }
        }
    }

    fn line(&mut self, line: &str, now_ms: i64, records: &mut Vec<Record>) {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            return;
        }
        match parse(line) {
            Ok(Message::Metric(m)) => {
                self.stats.metrics += 1;
                self.metric(&m, now_ms);
            }
            Ok(Message::Event(e)) => {
                self.stats.events += 1;
                records.push(self.event_record(&e, now_ms));
            }
            Ok(Message::Check(c)) => {
                self.stats.checks += 1;
                records.push(self.check_record(&c, now_ms));
            }
            Err(_) => self.stats.invalid += 1,
        }
    }

    fn metric(&mut self, m: &Metric<'_>, now_ms: i64) {
        let class = m.ty.class();
        let Some(slot) = self.resolve(m, class) else {
            self.stats.refused += m.values.split(':').count() as u64;
            return;
        };
        let Some(Some(c)) = self.contexts.get_mut(slot) else { return };
        let (out, held, stats) = (&mut self.out, &mut self.held, &mut self.stats);
        // A gauge or count with its own time: written at it, as it came. It keeps its context but
        // is no sample of its buckets (a count's zeros follow only what was aggregated).
        if let (Some(ts), Class::Count | Class::Gauge) = (m.ts, class) {
            let scale = if class == Class::Count { 1.0 / m.rate } else { 1.0 };
            for v in m.values.split(':') {
                match v.parse::<f64>() {
                    Ok(v) if v.is_finite() => {
                        out.put(c, 0, ts.saturating_mul(1000), v * scale);
                        stats.points += 1;
                        c.seen_ms = c.seen_ms.max(now_ms);
                    }
                    _ => stats.invalid += 1,
                }
            }
            return;
        }
        let bucket = bucket_of(now_ms).max(self.flushed).max(c.bucket);
        if bucket > c.bucket {
            stats.points += close(c, bucket, out, &self.s, held);
        }
        let w = 1.0 / m.rate;
        let mut taken = false;
        match &mut c.state {
            State::Set { seen, n } => {
                taken = true;
                for v in m.values.split(':') {
                    let h = self.hasher.bytes(v.as_bytes());
                    if seen.find(h, |&x| x == h).is_some() {
                        continue;
                    }
                    if *n >= SET_VALUES {
                        stats.set_over += 1;
                        continue;
                    }
                    seen.insert_unique(h, h, |&x| x);
                    *n += 1;
                }
            }
            state => {
                for v in m.values.split(':') {
                    let Some(v) = v.parse::<f64>().ok().filter(|v| v.is_finite()) else {
                        stats.invalid += 1;
                        continue;
                    };
                    taken = true;
                    match state {
                        State::Count { sum, sampled } => {
                            *sum += v * w;
                            *sampled = true;
                        }
                        State::Gauge { last, sampled } => {
                            *last = v;
                            *sampled = true;
                        }
                        State::Hist(h) => h.add(v, w, class == Class::Hist, held),
                        State::Set { .. } => {}
                    }
                }
            }
        }
        if taken {
            c.last_ms = c.last_ms.max(now_ms);
            c.seen_ms = c.seen_ms.max(now_ms);
        }
    }

    /// The context of `m`: found, or made (`None`: refused).
    fn resolve(&mut self, m: &Metric<'_>, class: Class) -> Option<usize> {
        let hasher = self.hasher;
        let mut host: Option<&str> = None;
        self.scratch.clear();
        let raw = m.tags;
        for t in raw.split(',') {
            if t.is_empty() {
                continue;
            }
            let (k, v) = t.split_once(':').unwrap_or((t, ""));
            if k == "host" {
                host = host.or(Some(v));
                continue;
            }
            // No series has a tag without a key: such a tag is passed over, not the metric.
            if k.is_empty() || internal(k) {
                continue;
            }
            // A tag hashed once, as its pair is (`key` and `key:` alike); a key given again found by
            // its bytes among those kept, by where they lie in the message.
            let th = hasher.bytes(if v.is_empty() { k } else { t }.as_bytes());
            let start = (k.as_ptr() as usize).wrapping_sub(raw.as_ptr() as usize);
            let seen = self.scratch.iter().find(|&&(_, s, n)| n as usize == k.len() && raw.get(s as usize..s as usize + k.len()) == Some(k));
            if let Some(&(prev, _, _)) = seen {
                if prev != th {
                    self.stats.conflicts += 1;
                }
                continue;
            }
            self.scratch.push((th, start as u32, k.len() as u32));
        }
        let host = host.unwrap_or(&self.s.host);
        self.scratch.sort_unstable_by_key(|&(t, _, _)| t);
        let mut hash = hasher.word(hasher.bytes(m.name.as_bytes()) ^ (class as u64)) ^ hasher.bytes(host.as_bytes());
        for &(t, _, _) in &self.scratch {
            hash = hasher.word(hash ^ t);
        }
        let (contexts, scratch) = (&self.contexts, &self.scratch);
        let found = self.table.find(hash, |&(h, slot)| {
            h == hash
                && contexts.get(slot as usize).and_then(Option::as_ref).is_some_and(|c| {
                    c.class == class && &*c.name == m.name && &*c.host == host && c.ident.len() == scratch.len() && c.ident.iter().zip(scratch.iter()).all(|(a, b)| *a == b.0)
                })
        });
        if let Some(&(_, slot)) = found {
            return Some(slot as usize);
        }
        if self.contexts.len() - self.free.len() >= CONTEXTS {
            return None;
        }
        let c = self.make(m, class, host, hash)?;
        let slot = match self.free.pop() {
            Some(s) => {
                if let Some(x) = self.contexts.get_mut(s as usize) {
                    *x = Some(c);
                }
                s
            }
            None => {
                self.contexts.push(Some(c));
                (self.contexts.len() - 1) as u32
            }
        };
        self.table.insert_unique(hash, (hash, slot), |&(h, _)| h);
        Some(slot as usize)
    }

    /// A new context: its series' keys made (`None` if they cannot be).
    fn make(&self, m: &Metric<'_>, class: Class, host: &str, hash: u64) -> Option<Context> {
        // Its tags: the message's, each key once (its first value), then the listener's on keys
        // the message has not, then its host.
        let mut tags: Vec<(&str, &str)> = Vec::new();
        for (k, v) in tag_pairs(m.tags) {
            if k.is_empty() || k == "host" || internal(k) || tags.iter().any(|t| t.0 == k) {
                continue;
            }
            tags.push((k, v));
        }
        for (k, v) in &self.s.tags {
            if k != "host" && !tags.iter().any(|t| t.0 == k.as_str()) {
                tags.push((k, v));
            }
        }
        if !host.is_empty() {
            tags.push(("host", host));
        }
        let key = |name: &str, kind: Kind| SeriesKey::new(name, tags.iter().copied(), kind).ok();
        let name = m.name;
        let (keys, state): (Vec<SeriesKey>, State) = match class {
            Class::Count => (vec![key(name, Kind::Count)?], State::Count { sum: 0.0, sampled: false }),
            Class::Gauge => (vec![key(name, Kind::Gauge)?], State::Gauge { last: 0.0, sampled: false }),
            Class::Set => (vec![key(name, Kind::Gauge)?], State::Set { seen: HashTable::new(), n: 0 }),
            Class::Hist => {
                let mut keys = Vec::with_capacity(self.s.aggregates.len() + self.s.percentiles.len());
                for a in &self.s.aggregates {
                    let kind = if *a == Aggregate::Count { Kind::Count } else { Kind::Gauge };
                    keys.push(key(&format!("{name}.{}", a.name()), kind)?);
                }
                for p in &self.s.percentiles {
                    keys.push(key(&format!("{name}.{p}percentile"), Kind::Gauge)?);
                }
                (keys, State::Hist(Box::default()))
            }
            Class::Dist => {
                let keys = vec![key(&format!("{name}.count"), Kind::Count)?, key(&format!("{name}.sum"), Kind::Count)?, key(&format!("{name}.min"), Kind::Gauge)?, key(&format!("{name}.max"), Kind::Gauge)?];
                (keys, State::Hist(Box::default()))
            }
        };
        let ident = self.scratch.iter().map(|&(t, _, _)| t).collect();
        let dist_tags = if class == Class::Dist { tags.iter().map(|&(k, v)| (k.to_owned(), v.to_owned())).collect() } else { Vec::new() };
        Some(Context {
            name: name.into(),
            class,
            host: host.into(),
            ident,
            hash,
            keys: keys.into(),
            dist: BTreeMap::new(),
            dist_tags,
            bucket: i64::MIN,
            last_ms: i64::MIN,
            seen_ms: i64::MIN,
            state,
            out_round: 0,
            out_at: 0,
        })
    }

    /// Every bucket over by `now_ms` written, and contexts that have had nothing for
    /// [`EXPIRY_MS`] forgotten.
    pub fn flush(&mut self, now_ms: i64) {
        let cutoff = bucket_of(now_ms);
        let (out, held, s) = (&mut self.out, &mut self.held, &self.s);
        for slot in 0..self.contexts.len() {
            let Some(cell) = self.contexts.get_mut(slot) else { continue };
            let Some(c) = cell else { continue };
            if c.bucket < cutoff {
                self.stats.points += close(c, cutoff, out, s, held);
            }
            if c.seen_ms.saturating_add(EXPIRY_MS) <= now_ms {
                let hash = c.hash;
                if let State::Hist(h) = &mut c.state {
                    h.clear(held);
                }
                *cell = None;
                if let Ok(e) = self.table.find_entry(hash, |&(_, x)| x as usize == slot) {
                    e.remove();
                }
                self.free.push(slot as u32);
            }
        }
        self.flushed = self.flushed.max(cutoff);
    }

    /// What was written since the last drain: its series' keys and samples moved into `keys` and
    /// `samples` (whose room this keeps for the next), the number of samples.
    pub fn drain(&mut self, keys: &mut Vec<SeriesKey>, samples: &mut Vec<(u32, i64, f64)>) -> usize {
        keys.clear();
        samples.clear();
        std::mem::swap(&mut self.out.keys, keys);
        std::mem::swap(&mut self.out.samples, samples);
        self.out.round = self.out.round.wrapping_add(1).max(1);
        samples.len()
    }

    /// Whether anything waits to be drained.
    pub fn pending(&self) -> bool {
        !self.out.samples.is_empty()
    }

    fn tagged(&self, raw: &str, attrs: &mut Vec<(String, String)>) -> (String, Option<String>) {
        let mut service = String::new();
        let mut host = None;
        for (k, v) in tag_pairs(raw).chain(self.s.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
            match k {
                "host" => host = host.or_else(|| Some(v.to_owned())),
                "service" if service.is_empty() => service = v.to_owned(),
                k if internal(k) => {}
                _ => {}
            }
            if k != "host" && !internal(k) && !attrs.iter().any(|(x, _)| x == k) {
                attrs.push((k.to_owned(), v.to_owned()));
            }
        }
        (service, host)
    }

    /// An event as kiln's events are (`source:kiln.event`), as the events API makes one.
    pub fn event_record(&self, e: &Event<'_>, now_ms: i64) -> Record {
        let severity = match e.alert {
            Some("error") => 17,
            Some("warning") => 13,
            _ => 9,
        };
        let mut attrs: Vec<(String, String)> = vec![("event.type".into(), "custom".into()), ("title".into(), e.title.clone().into_owned()), ("alert_type".into(), e.alert.unwrap_or("info").to_owned())];
        if let Some(k) = e.key {
            attrs.push(("aggregation_key".into(), k.to_owned()));
        }
        if let Some(p) = e.priority {
            attrs.push(("priority".into(), p.to_owned()));
        }
        let (service, tag_host) = self.tagged(e.tags, &mut attrs);
        attrs.push(("event.source".into(), e.source.unwrap_or("dogstatsd").to_owned()));
        let message = if e.text.is_empty() { e.title.clone().into_owned() } else { format!("{}\n\n{}", e.title, e.text) };
        let host = e.host.map(str::to_owned).or(tag_host).unwrap_or_else(|| self.s.host.clone());
        let ts_ms = e.ts.map_or(now_ms, |s| s.saturating_mul(1000));
        Record { ts: ts_ms.saturating_mul(1_000_000), severity, service, host, source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }
    }

    /// A service check as a log record (`source:service_check`): its name, its status (OK info,
    /// WARNING a warning, CRITICAL an error, UNKNOWN unspecified), its message and tags.
    pub fn check_record(&self, c: &Check<'_>, now_ms: i64) -> Record {
        let (severity, status) = match c.status {
            0 => (9, "ok"),
            1 => (13, "warning"),
            2 => (17, "critical"),
            _ => (0, "unknown"),
        };
        let mut attrs: Vec<(String, String)> = vec![("check".into(), c.name.to_owned()), ("check_status".into(), status.to_owned())];
        let (service, tag_host) = self.tagged(c.tags, &mut attrs);
        let message = c.message.as_deref().map_or_else(|| format!("{} is {}", c.name, status.to_ascii_uppercase()), str::to_owned);
        let host = c.host.map(str::to_owned).or(tag_host).unwrap_or_else(|| self.s.host.clone());
        let ts_ms = c.ts.map_or(now_ms, |s| s.saturating_mul(1000));
        Record { ts: ts_ms.saturating_mul(1_000_000), severity, service, host, source: "service_check".into(), message, attrs, trace_id: None, span_id: None }
    }
}

/// Context `c`'s bucket written (and a count's zeros after it, up to `upto` and as long as it is
/// not expired), its state then of `upto`; the points written.
fn close(c: &mut Context, upto: i64, out: &mut Out, s: &Settings, held: &mut usize) -> u64 {
    if c.bucket >= upto {
        return 0;
    }
    let at = c.bucket;
    let mut points = 0u64;
    if at != i64::MIN {
        match &mut c.state {
            State::Count { sum, sampled } => {
                let (v, was) = (*sum, *sampled);
                (*sum, *sampled) = (0.0, false);
                // Its bucket, then a zero for each bucket after it that starts within the expiry
                // of its last sample (the Agent's `countersSampleZeroValue`).
                let mut t = at;
                while t < upto && (t == at && was || t < c.last_ms.saturating_add(EXPIRY_MS)) {
                    out.put(c, 0, t, if t == at && was { v } else { 0.0 });
                    points += 1;
                    t += BUCKET_MS;
                }
            }
            State::Gauge { last, sampled } => {
                if *sampled {
                    let v = *last;
                    *sampled = false;
                    out.put(c, 0, at, v);
                    points += 1;
                }
            }
            State::Set { seen, n } => {
                if *n > 0 {
                    let v = *n as f64;
                    seen.clear();
                    *n = 0;
                    out.put(c, 0, at, v);
                    points += 1;
                }
            }
            State::Hist(h) => {
                if h.count > 0.0 {
                    points += if c.class == Class::Dist { write_dist(c, at, out) } else { write_hist(c, at, out, s) };
                }
                if let State::Hist(h) = &mut c.state {
                    h.clear(held);
                }
            }
        }
    }
    c.bucket = upto;
    points
}

fn write_hist(c: &mut Context, at: i64, out: &mut Out, s: &Settings) -> u64 {
    let State::Hist(h) = &mut c.state else { return 0 };
    h.sort();
    let mut vals = std::mem::take(&mut out.vals);
    vals.clear();
    for a in &s.aggregates {
        vals.push(match a {
            Aggregate::Max => h.max,
            Aggregate::Min => h.min,
            Aggregate::Median => h.quantile(1, 2),
            Aggregate::Avg => h.sum / h.count,
            Aggregate::Count => h.count,
            Aggregate::Sum => h.sum,
        });
    }
    for p in &s.percentiles {
        vals.push(h.quantile(u32::from(*p), 100));
    }
    for (i, v) in vals.iter().enumerate() {
        out.put(c, i, at, *v);
    }
    let n = vals.len() as u64;
    out.vals = vals;
    n
}

fn write_dist(c: &mut Context, at: i64, out: &mut Out) -> u64 {
    let State::Hist(h) = &mut c.state else { return 0 };
    let (count, sum, min, max) = (h.count, h.sum, h.min, h.max);
    let mut sketch = std::mem::take(&mut h.sketch);
    if !h.sketched {
        for &(v, w) in &h.exact {
            if let Some(b) = dist::bucket_of(v, dist::SCALE) {
                sketch_add(&mut sketch, b.order(), w);
            }
        }
    }
    let mut points = 0u64;
    for &(order, w) in &sketch {
        // Its bucket's series, made the first time it is written; past [`DIST_BUCKETS`] of them,
        // the nearest one's.
        let order = if c.dist.contains_key(&order) || c.dist.len() < DIST_BUCKETS {
            order
        } else {
            let below = c.dist.range(..order).next_back().map(|(o, _)| *o);
            let above = c.dist.range(order..).next().map(|(o, _)| *o);
            match (below, above) {
                (Some(b), Some(a)) => {
                    if order.0 == a.0 && (b.0 != order.0 || a.1 - order.1 < order.1 - b.1) {
                        a
                    } else {
                        b
                    }
                }
                (Some(b), None) => b,
                (None, Some(a)) => a,
                (None, None) => continue,
            }
        };
        if !c.dist.contains_key(&order) {
            let tag = dist::tag_value(dist::SCALE, bucket_at(order));
            let with = c.dist_tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).chain([(dist::BUCKET_TAG, tag.as_str())]);
            let Ok(key) = SeriesKey::new(&format!("{}{}", c.name, dist::SUFFIX), with, Kind::Count) else { continue };
            c.dist.insert(order, DistKey { key, round: 0, at: 0 });
        }
        let Some(d) = c.dist.get_mut(&order) else { continue };
        if d.round != out.round {
            d.round = out.round;
            d.at = out.keys.len() as u32;
            out.keys.push(d.key.clone());
        }
        // Two buckets folded into one held: their weights added where it is written.
        match out.samples.last_mut() {
            Some(last) if last.0 == d.at && last.1 == at => last.2 += w,
            _ => {
                out.samples.push((d.at, at, w));
                points += 1;
            }
        }
    }
    sketch.clear();
    if let State::Hist(h) = &mut c.state {
        h.sketch = sketch;
    }
    for (i, v) in [count, sum, min, max].into_iter().enumerate() {
        out.put(c, i, at, v);
        points += 1;
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agg() -> Aggregator {
        Aggregator::new(Settings { host: "web-1".into(), ..Settings::default() })
    }

    /// What was written: each series' name and tags (sorted, `k:v`), its time and value.
    fn written(a: &mut Aggregator) -> Vec<(String, i64, f64)> {
        let (mut keys, mut samples) = (Vec::new(), Vec::new());
        a.drain(&mut keys, &mut samples);
        let mut out: Vec<(String, i64, f64)> = samples
            .iter()
            .map(|&(i, ts, v)| {
                let k = crate::batch::parse_key(&keys[i as usize].key).unwrap();
                let tags: Vec<String> = k.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect();
                (format!("{}{{{}}}", k.metric, tags.join(",")), ts, v)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }

    #[test]
    fn messages_read_as_datadog_s_protocol_has_them() {
        let m = parse("page.views:1|c|@0.5|#env:prod,role:web|c:abc123|T1700000000|e:it|card:low").unwrap();
        assert_eq!(m, Message::Metric(Metric { name: "page.views", values: "1", ty: Type::Count, rate: 0.5, tags: "env:prod,role:web", container: Some("abc123"), ts: Some(1_700_000_000) }));
        let Message::Metric(m) = parse("lat:1.5:2:3e2|ms").unwrap() else { panic!() };
        assert_eq!((m.values, m.ty, m.rate, m.tags), ("1.5:2:3e2", Type::Timer, 1.0, ""));
        for (line, e) in [("", ParseError::Name), (":1|c", ParseError::Name), ("x|c", ParseError::Value), ("x:|c", ParseError::Value), ("x:1|q", ParseError::Type), ("x:1", ParseError::Type), ("x:1|c|@0", ParseError::Rate), ("x:1|c|@abc", ParseError::Rate), ("x:1|g|Tsoon", ParseError::Timestamp)] {
            assert_eq!(parse(line), Err(e), "{line}");
        }
        let Message::Event(e) = parse("_e{5,11}:hello|world\\nnews|d:1700000000|h:db-2|k:deploy|p:low|s:jenkins|t:warning|#env:prod").unwrap() else { panic!() };
        assert_eq!((&*e.title, &*e.text, e.ts, e.host, e.key, e.priority, e.source, e.alert, e.tags), ("hello", "world\nnews", Some(1_700_000_000), Some("db-2"), Some("deploy"), Some("low"), Some("jenkins"), Some("warning"), "env:prod"));
        // Lengths are bytes: é is two, and a length that ends inside a character does not read.
        let Message::Event(e) = parse("_e{8,0}:résumé|").unwrap() else { panic!() };
        assert_eq!((&*e.title, &*e.text), ("résumé", ""));
        assert_eq!(parse("_e{1,0}:é|"), Err(ParseError::Event));
        assert_eq!(parse("_e{3,2}:ab|cd"), Err(ParseError::Event));
        assert_eq!(parse("_e{0,2}:|cd"), Err(ParseError::Event));
        let Message::Check(c) = parse("_sc|db.up|2|d:1700000000|h:db-1|#env:prod|m:down | since 9:00\\nsee runbook").unwrap() else { panic!() };
        assert_eq!((c.name, c.status, c.ts, c.host, c.tags, c.message.as_deref()), ("db.up", 2, Some(1_700_000_000), Some("db-1"), "env:prod", Some("down | since 9:00\nsee runbook")));
        assert_eq!(parse("_sc|db.up|4"), Err(ParseError::Check));
        assert_eq!(parse("_sc||0"), Err(ParseError::Check));
    }

    /// Counts summed over their sample rates, a bucket of ten seconds each, written at the
    /// bucket's start once it is over; then zeros for five minutes; then nothing.
    #[test]
    fn counts_sum_by_bucket_and_fall_to_zero_until_they_expire() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"hits:1|c|#route:/a\nhits:2:3|c|@0.5|#route:/a", 1_000_005, &mut r);
        a.take(b"hits:4|c|#route:/a", 1_009_999, &mut r);
        a.flush(1_009_999);
        assert!(written(&mut a).is_empty(), "the bucket is not over");
        a.take(b"hits:7|c|#route:/a", 1_010_000, &mut r);
        // The first bucket is written as the next one's first sample comes.
        assert_eq!(written(&mut a), vec![("hits{host:web-1,route:/a}".into(), 1_000_000, 1.0 + 10.0 + 4.0)]);
        a.flush(1_020_000);
        assert_eq!(written(&mut a), vec![("hits{host:web-1,route:/a}".into(), 1_010_000, 7.0)]);
        // Nothing more: zeros while a bucket starts within five minutes of the last sample.
        a.flush(1_400_000);
        let w = written(&mut a);
        let zeros: Vec<i64> = w.iter().map(|x| x.1).collect();
        assert!(w.iter().all(|x| x.2 == 0.0));
        assert_eq!(zeros.first(), Some(&1_020_000));
        assert_eq!(zeros.last(), Some(&1_300_000), "the last bucket starting before 1,010,000 + 300,000");
        assert_eq!(zeros.len(), 29);
        assert_eq!(a.stats().contexts, 0, "forgotten once expired");
        a.flush(1_500_000);
        assert!(written(&mut a).is_empty());
    }

    #[test]
    fn gauges_keep_the_last_and_sets_count_distinct_values() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"temp:20|g\ntemp:21:22|g|@0.1\nusers:ada|s\nusers:bob:ada|s\nusers:cy|s|#x:y", 5_000, &mut r);
        a.flush(10_000);
        assert_eq!(written(&mut a), vec![("temp{host:web-1}".into(), 0, 22.0), ("users{host:web-1,x:y}".into(), 0, 1.0), ("users{host:web-1}".into(), 0, 2.0)]);
        a.flush(20_000);
        assert!(written(&mut a).is_empty(), "a gauge is written for a bucket it had samples in");
    }

    /// A histogram's aggregates as the Agent computes them: nearest rank, max, average of the
    /// weighted samples, the weighted count.
    #[test]
    fn histograms_write_the_agent_s_aggregates() {
        let mut a = agg();
        let mut r = Vec::new();
        let values: Vec<String> = (1..=20).map(|v| format!("lat:{v}|ms")).collect();
        a.take(values.join("\n").as_bytes(), 1, &mut r);
        a.take(b"lat:100|h|@0.5", 2, &mut r);
        a.flush(10_000);
        let w = written(&mut a);
        let get = |n: &str| w.iter().find(|x| x.0.starts_with(n)).map(|x| x.2).unwrap();
        // 1..=20 once each and 100 twice (a rate of a half): 22 in all.
        assert_eq!(get("lat.count"), 22.0);
        assert_eq!(get("lat.max"), 100.0);
        assert_eq!(get("lat.avg"), (210.0 + 200.0) / 22.0);
        // Nearest rank: the 11th of 22 for the median, the 21st (ceil(20.9)) for the 95th.
        assert_eq!(get("lat.median"), 11.0);
        assert_eq!(get("lat.95percentile"), 100.0);
        // The Agent's own on 20 samples of one weight: samples[(95·20 - 1) / 100] = samples[18].
        let mut b = agg();
        b.take(values.join("\n").as_bytes(), 1, &mut r);
        b.flush(10_000);
        let w = written(&mut b);
        assert_eq!(w.iter().find(|x| x.0.starts_with("lat.95percentile")).map(|x| x.2), Some(19.0));
        assert_eq!(w.iter().find(|x| x.0.starts_with("lat.median")).map(|x| x.2), Some(10.0), "samples[(20 - 1) / 2]");
    }

    /// Past what is held exactly, percentiles come from buckets: within 1.09% of the exact.
    #[test]
    fn a_histogram_past_its_exact_samples_is_within_its_buckets_bound() {
        let mut a = agg();
        let mut r = Vec::new();
        let n = EXACT_PER_CONTEXT + 10_000;
        let mut rng = crate::testkit::Rng::new(7);
        let mut all: Vec<f64> = Vec::with_capacity(n);
        let mut lines = String::new();
        for i in 0..n {
            let v = (rng.f64() * 8.0).exp();
            all.push(v);
            lines.push_str(&format!("rt:{v}|h\n"));
            if i % 1000 == 999 {
                a.take(lines.as_bytes(), 1, &mut r);
                lines.clear();
            }
        }
        a.take(lines.as_bytes(), 1, &mut r);
        a.flush(10_000);
        let w = written(&mut a);
        all.sort_by(f64::total_cmp);
        let exact = |p: usize| all[(p * n).div_ceil(100) - 1];
        for (name, p) in [("rt.median", 50), ("rt.95percentile", 95)] {
            let got = w.iter().find(|x| x.0.starts_with(name)).map(|x| x.2).unwrap();
            let want = exact(p);
            assert!((got - want).abs() <= want * 0.0109 + 1e-12, "{name}: {got} against {want}");
        }
        assert_eq!(a.held, 0, "what was held exactly is given back");
    }

    #[test]
    fn distributions_write_kiln_s_buckets_and_their_figures() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"size:1:2:2|d|#env:prod\nsize:1000|d|@0.25|#env:prod", 3, &mut r);
        a.flush(10_000);
        let w = written(&mut a);
        let get = |n: &str| w.iter().filter(|x| x.0.starts_with(n)).map(|x| x.2).collect::<Vec<_>>();
        assert_eq!(get("size.count"), vec![7.0]);
        assert_eq!(get("size.sum"), vec![5.0 + 4000.0]);
        assert_eq!((get("size.min"), get("size.max")), (vec![1.0], vec![1000.0]));
        let buckets = get("size.dist");
        assert_eq!(buckets.iter().sum::<f64>(), 7.0);
        assert_eq!(buckets.len(), 3, "1, 2 and 1000 fall in three buckets");
        assert!(w.iter().any(|x| x.0 == format!("size.dist{{dist_bucket:{},env:prod,host:web-1}}", dist::tag_value(dist::SCALE, dist::bucket_of(1000.0, dist::SCALE).unwrap()))));
    }

    #[test]
    fn a_gauge_or_count_with_a_time_is_written_at_it() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"q:5|g|T1700000000\nn:3|c|@0.5|T1700000001\nq:6|g", 1_000, &mut r);
        let w = written(&mut a);
        assert_eq!(w, vec![("n{host:web-1}".into(), 1_700_000_001_000, 6.0), ("q{host:web-1}".into(), 1_700_000_000_000, 5.0)]);
        a.flush(10_000);
        assert_eq!(written(&mut a), vec![("q{host:web-1}".into(), 0, 6.0)]);
    }

    /// Tags in any order are one context; a `host:` tag is the host; a key given twice keeps its
    /// first value; the listener's tags are added where the message has not the key.
    #[test]
    fn contexts_are_their_tags_in_any_order() {
        let mut a = Aggregator::new(Settings { host: "web-1".into(), tags: vec![("env".into(), "dev".into()), ("team".into(), "core".into())], ..Settings::default() });
        let mut r = Vec::new();
        a.take(b"c:1|c|#a:1,b:2,env:prod\nc:1|c|#b:2,env:prod,a:1\nc:1|c|#env:prod,a:1,b:2,a:1\nc:1|c|#a:1,b:2,env:prod,b:3\nc:1|c|#host:db-9,a:1\nc:1|c|#dd.internal.entity_id:x,a:1", 1, &mut r);
        a.flush(10_000);
        let w = written(&mut a);
        assert_eq!(
            w,
            vec![
                ("c{a:1,b:2,env:prod,host:web-1,team:core}".into(), 0, 4.0),
                ("c{a:1,env:dev,host:db-9,team:core}".into(), 0, 1.0),
                ("c{a:1,env:dev,host:web-1,team:core}".into(), 0, 1.0),
            ]
        );
        assert_eq!(a.stats().conflicts, 1);
        assert_eq!(a.stats().contexts, 3);
    }

    #[test]
    fn events_and_service_checks_are_records() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"_e{6,4}:Deploy|v1.2|t:error|#service:api,env:prod\n_sc|db.can_connect|2|#service:db|m:refused", 1_700_000_000_000, &mut r);
        assert_eq!(r.len(), 2);
        let (e, c) = (&r[0], &r[1]);
        assert_eq!((e.source.as_str(), e.severity, e.service.as_str(), e.host.as_str(), e.message.as_str()), ("kiln.event", 17, "api", "web-1", "Deploy\n\nv1.2"));
        assert!(e.attrs.contains(&("title".into(), "Deploy".into())) && e.attrs.contains(&("env".into(), "prod".into())) && e.attrs.contains(&("event.source".into(), "dogstatsd".into())));
        assert_eq!((c.source.as_str(), c.severity, c.service.as_str(), c.message.as_str()), ("service_check", 17, "db", "refused"));
        assert!(c.attrs.contains(&("check".into(), "db.can_connect".into())) && c.attrs.contains(&("check_status".into(), "critical".into())));
        assert_eq!(c.ts, 1_700_000_000_000 * 1_000_000);
    }

    #[test]
    fn what_does_not_read_is_counted_and_nothing_else_is_lost() {
        let mut a = agg();
        let mut r = Vec::new();
        a.take(b"ok:1|c\nbad\n\xff\xfe:1|c\nx:nan|g\nx:abc|c\ny:1|q\nok:1|c\r\n", 1, &mut r);
        a.flush(10_000);
        assert_eq!(written(&mut a), vec![("ok{host:web-1}".into(), 0, 2.0)]);
        let s = a.stats();
        assert_eq!((s.metrics, s.invalid), (4, 5));
    }

    #[test]
    fn contexts_past_the_bound_are_refused_and_counted() {
        let mut a = agg();
        let mut r = Vec::new();
        let mut d = String::new();
        for i in 0..CONTEXTS + 5 {
            d.push_str(&format!("m:1|c|#i:{i}\n"));
            if d.len() > 60_000 {
                a.take(d.as_bytes(), 1, &mut r);
                d.clear();
            }
        }
        a.take(d.as_bytes(), 1, &mut r);
        assert_eq!(a.stats().contexts, CONTEXTS as u64);
        assert_eq!(a.stats().refused, 5);
    }

    /// A set's distinct values are bounded: past the bound its count stays there.
    #[test]
    fn a_set_s_values_are_bounded() {
        let mut a = agg();
        let mut r = Vec::new();
        let mut d = String::new();
        for i in 0..SET_VALUES + 3 {
            d.push_str(&format!("u:{i}|s\n"));
            if d.len() > 60_000 {
                a.take(d.as_bytes(), 1, &mut r);
                d.clear();
            }
        }
        a.take(d.as_bytes(), 1, &mut r);
        a.flush(10_000);
        assert_eq!(written(&mut a), vec![("u{host:web-1}".into(), 0, SET_VALUES as f64)]);
        assert_eq!(a.stats().set_over, 3);
    }
}
