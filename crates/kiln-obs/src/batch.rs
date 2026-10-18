//! A write batch: what one Raft entry carries to every replica (`docs/observe.md` §5).
//!
//! The node that takes a write canonicalises it once, before it is replicated: a series' key (its
//! metric and its tags, sorted) is made once and kept as a [`SeriesKey`], whose fingerprint names
//! the series in every batch after the one that defines it; samples are sorted, de-duplicated and
//! Gorilla-encoded. A replica's apply is a fingerprint lookup and a decode a run.
//!
//! ```text
//! batch := VERSION u8 | items varint | item*
//! item  := DEF u8 | fp u64-le | kind u8 | key varint-length bytes
//!        | RUN u8 | fp u64-le | samples varint | chunk varint-length bytes
//! key   := metric | tags varint | (tag-key, tag-value)*          (each varint-length bytes)
//! ```
//!
//! A run names a series defined in its batch (earlier) or in an earlier batch. A replica that does
//! not know a fingerprint says so in its answer ([`crate::store::Applied::unknown`]), and the
//! sending node defines the series again.

use sha2::{Digest, Sha256};

use crate::gorilla::{ChunkRef, Encoder};
use crate::varint;

/// The format's version: a replica refuses a batch of another, never guesses.
pub const VERSION: u8 = 1;

const DEF: u8 = 0;
const RUN: u8 = 1;

/// The longest metric name, tag key or value: Datadog's limit on a tag and a metric name, 200
/// characters (docs.datadoghq.com, "Getting Started with Tags" and "Metrics: naming"), in bytes of
/// UTF-8, four a character at most.
pub const MAX_NAME_BYTES: usize = 200 * 4;

/// The most tags a series: OpenTelemetry's default attribute count limit
/// (`OTEL_ATTRIBUTE_COUNT_LIMIT`, 128; specification, "Attribute Limits").
pub const MAX_TAGS: usize = 128;

/// What a series is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Gauge = 0,
    /// A delta for each interval.
    Count = 1,
    /// A cumulative value; resets are found at query time.
    Counter = 2,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Gauge),
            1 => Some(Self::Count),
            2 => Some(Self::Counter),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BatchError {
    #[error("an empty metric name")]
    EmptyMetric,
    #[error("a name or value of {0} bytes (at most {MAX_NAME_BYTES})")]
    TooLong(usize),
    #[error("{0} tags (at most {MAX_TAGS})")]
    TooManyTags(usize),
    #[error("tag key {0:?} given twice")]
    DuplicateKey(String),
    #[error("an empty tag key")]
    EmptyKey,
    #[error("batch version {0} (this build reads {VERSION})")]
    Version(u8),
    #[error("the batch is malformed: {0}")]
    Malformed(&'static str),
}

fn check_name(s: &str) -> Result<(), BatchError> {
    if s.len() > MAX_NAME_BYTES {
        return Err(BatchError::TooLong(s.len()));
    }
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    varint::put(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn get_bytes<'a>(at: &mut &'a [u8]) -> Option<&'a [u8]> {
    let n = usize::try_from(varint::get(at)?).ok()?;
    let (b, rest) = at.split_at_checked(n)?;
    *at = rest;
    Some(b)
}

fn get_u64(at: &mut &[u8]) -> Option<u64> {
    let (w, rest) = at.split_first_chunk::<8>()?;
    *at = rest;
    Some(u64::from_le_bytes(*w))
}

/// A series' fingerprint: the first eight bytes of the SHA-256 of its canonical key, the same on
/// every node and in every build. Finding two keys with one fingerprint takes about 2^32 hashes
/// (a birthday search, which only collides keys the searcher made up); reaching an existing
/// series' fingerprint takes about 2^64. A replica refuses a definition whose fingerprint names
/// another key (`IndexError::Collision`).
pub fn fingerprint(key: &[u8]) -> u64 {
    let d = Sha256::digest(key);
    let mut w = [0u8; 8];
    for (a, b) in w.iter_mut().zip(d.iter()) {
        *a = *b;
    }
    u64::from_le_bytes(w)
}

/// A series' identity, made once on the node that takes its writes, and kept there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesKey {
    pub fp: u64,
    pub key: Vec<u8>,
    pub kind: Kind,
}

impl SeriesKey {
    /// The canonical key of `metric` with `tags` (sorted here).
    pub fn new<'t>(metric: &str, tags: impl IntoIterator<Item = (&'t str, &'t str)>, kind: Kind) -> Result<Self, BatchError> {
        if metric.is_empty() {
            return Err(BatchError::EmptyMetric);
        }
        check_name(metric)?;
        let mut pairs: Vec<(&str, &str)> = Vec::new();
        for (k, v) in tags {
            if k.is_empty() {
                return Err(BatchError::EmptyKey);
            }
            check_name(k)?;
            check_name(v)?;
            if pairs.len() >= MAX_TAGS {
                return Err(BatchError::TooManyTags(pairs.len() + 1));
            }
            pairs.push((k, v));
        }
        pairs.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for w in pairs.windows(2) {
            if let [a, b] = w
                && a.0 == b.0
            {
                return Err(BatchError::DuplicateKey(a.0.to_owned()));
            }
        }
        let mut key = Vec::with_capacity(metric.len() + pairs.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>() + 3);
        put_bytes(&mut key, metric.as_bytes());
        varint::put(&mut key, pairs.len() as u64);
        for (k, v) in &pairs {
            put_bytes(&mut key, k.as_bytes());
            put_bytes(&mut key, v.as_bytes());
        }
        Ok(Self { fp: fingerprint(&key), key, kind })
    }
}

/// Builds a batch on the node that took the write.
#[derive(Debug, Default)]
pub struct Builder {
    body: Vec<u8>,
    items: u64,
    samples: u64,
    /// Reused across runs.
    sorted: Vec<(i64, f64)>,
    enc: Encoder,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Defines a series, for replicas that do not know it yet. Runs of it may follow in this
    /// batch and later ones.
    pub fn define(&mut self, key: &SeriesKey) {
        self.body.push(DEF);
        self.body.extend_from_slice(&key.fp.to_le_bytes());
        self.body.push(key.kind as u8);
        put_bytes(&mut self.body, &key.key);
        self.items += 1;
    }

    /// A run of samples of series `fp`. Samples are sorted by time, and of samples at one time
    /// the last given is kept (a later write corrects an earlier one).
    pub fn run(&mut self, fp: u64, samples: impl IntoIterator<Item = (i64, f64)>) {
        self.sorted.clear();
        self.sorted.extend(samples);
        if self.sorted.is_empty() {
            return;
        }
        self.sorted.sort_by_key(|s| s.0);
        self.enc.clear();
        let mut i = 0;
        while let Some(&(t, v)) = self.sorted.get(i) {
            let mut last = v;
            let mut j = i + 1;
            while let Some(&(t2, v2)) = self.sorted.get(j)
                && t2 == t
            {
                last = v2;
                j += 1;
            }
            // Sorted and de-duplicated: the push cannot be refused.
            let _ = self.enc.push(t, last);
            i = j;
        }
        self.body.push(RUN);
        self.body.extend_from_slice(&fp.to_le_bytes());
        varint::put(&mut self.body, u64::from(self.enc.count()));
        varint::put(&mut self.body, self.enc.byte_len() as u64);
        self.enc.write_to(&mut self.body);
        self.items += 1;
        self.samples += u64::from(self.enc.count());
    }

    pub fn items(&self) -> u64 {
        self.items
    }

    /// Samples in the batch's runs.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn byte_len(&self) -> usize {
        self.body.len() + 1 + varint::MAX_LEN
    }

    /// The batch, empty when it holds nothing; the builder starts over.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.items == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(1 + varint::MAX_LEN + self.body.len());
        out.push(VERSION);
        varint::put(&mut out, self.items);
        out.extend_from_slice(&self.body);
        self.body.clear();
        self.items = 0;
        self.samples = 0;
        out
    }
}

/// One item of a batch.
#[derive(Debug, Clone, Copy)]
pub enum Item<'a> {
    Def { fp: u64, kind: Kind, key: &'a [u8] },
    Run { fp: u64, samples: ChunkRef<'a> },
}

/// A batch's items, read in place.
pub struct Reader<'a> {
    at: &'a [u8],
    left: u64,
}

impl<'a> Reader<'a> {
    pub fn new(batch: &'a [u8]) -> Result<Self, BatchError> {
        if batch.is_empty() {
            return Ok(Self { at: batch, left: 0 });
        }
        let (&v, mut at) = batch.split_first().ok_or(BatchError::Malformed("empty"))?;
        if v != VERSION {
            return Err(BatchError::Version(v));
        }
        let left = varint::get(&mut at).ok_or(BatchError::Malformed("item count"))?;
        Ok(Self { at, left })
    }

    pub fn next_item(&mut self) -> Result<Option<Item<'a>>, BatchError> {
        if self.left == 0 {
            return if self.at.is_empty() { Ok(None) } else { Err(BatchError::Malformed("bytes after the last item")) };
        }
        let (&tag, rest) = self.at.split_first().ok_or(BatchError::Malformed("item tag"))?;
        self.at = rest;
        let fp = get_u64(&mut self.at).ok_or(BatchError::Malformed("fingerprint"))?;
        let item = match tag {
            DEF => {
                let (&k, rest) = self.at.split_first().ok_or(BatchError::Malformed("kind"))?;
                self.at = rest;
                let kind = Kind::from_u8(k).ok_or(BatchError::Malformed("kind"))?;
                let key = get_bytes(&mut self.at).ok_or(BatchError::Malformed("key"))?;
                Item::Def { fp, kind, key }
            }
            RUN => {
                let count = varint::get(&mut self.at).and_then(|c| u32::try_from(c).ok()).ok_or(BatchError::Malformed("sample count"))?;
                let bytes = get_bytes(&mut self.at).ok_or(BatchError::Malformed("samples"))?;
                Item::Run { fp, samples: ChunkRef::new(bytes, count) }
            }
            _ => return Err(BatchError::Malformed("item tag")),
        };
        self.left -= 1;
        Ok(Some(item))
    }
}

/// A canonical key, read: the metric and its tags in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key<'a> {
    pub metric: &'a str,
    pub tags: Vec<(&'a str, &'a str)>,
}

fn utf8(b: &[u8]) -> Result<&str, BatchError> {
    std::str::from_utf8(b).map_err(|_| BatchError::Malformed("not UTF-8"))
}

/// Reads a canonical key.
pub fn parse_key(key: &[u8]) -> Result<Key<'_>, BatchError> {
    let mut at = key;
    let metric = utf8(get_bytes(&mut at).ok_or(BatchError::Malformed("key metric"))?)?;
    if metric.is_empty() {
        return Err(BatchError::EmptyMetric);
    }
    let n = varint::get(&mut at).ok_or(BatchError::Malformed("key tags"))?;
    if n > MAX_TAGS as u64 {
        return Err(BatchError::TooManyTags(usize::try_from(n).unwrap_or(usize::MAX)));
    }
    let mut tags = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let k = utf8(get_bytes(&mut at).ok_or(BatchError::Malformed("tag key"))?)?;
        let v = utf8(get_bytes(&mut at).ok_or(BatchError::Malformed("tag value"))?)?;
        tags.push((k, v));
    }
    if !at.is_empty() {
        return Err(BatchError::Malformed("bytes after the key"));
    }
    Ok(Key { metric, tags })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_series_key_is_canonical_whatever_order_its_tags_came_in() {
        let a = SeriesKey::new("system.cpu.user", [("host", "web-1"), ("env", "prod")], Kind::Gauge).unwrap();
        let b = SeriesKey::new("system.cpu.user", [("env", "prod"), ("host", "web-1")], Kind::Gauge).unwrap();
        assert_eq!(a, b);
        let k = parse_key(&a.key).unwrap();
        assert_eq!(k.metric, "system.cpu.user");
        assert_eq!(k.tags, vec![("env", "prod"), ("host", "web-1")]);
        let c = SeriesKey::new("system.cpu.user", [("env", "prod"), ("host", "web-2")], Kind::Gauge).unwrap();
        assert_ne!(a.fp, c.fp);
        // Fixed by the format, in every build: SHA-256("") begins e3 b0 c4 42 98 fc 1c 14.
        assert_eq!(fingerprint(b""), 0x141c_fc98_42c4_b0e3);
    }

    #[test]
    fn a_batch_carries_definitions_and_sorted_deduplicated_runs() {
        let a = SeriesKey::new("m", [("host", "a")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.define(&a);
        b.run(a.fp, [(30, 3.0), (10, 1.0), (20, 2.0), (20, 2.5)]);
        b.run(a.fp, []);
        b.run(77, [(5, 1.0)]);
        let batch = b.finish();
        assert!(b.finish().is_empty(), "the builder starts over");
        let mut r = Reader::new(&batch).unwrap();
        match r.next_item().unwrap().unwrap() {
            Item::Def { fp, kind, key } => assert_eq!((fp, kind, key), (a.fp, Kind::Gauge, a.key.as_slice())),
            Item::Run { .. } => panic!("a definition first"),
        }
        match r.next_item().unwrap().unwrap() {
            Item::Run { fp, samples } => {
                assert_eq!(fp, a.fp);
                assert_eq!(samples.iter().map(Result::unwrap).collect::<Vec<_>>(), vec![(10, 1.0), (20, 2.5), (30, 3.0)]);
            }
            Item::Def { .. } => panic!("a run"),
        }
        assert!(matches!(r.next_item().unwrap().unwrap(), Item::Run { fp: 77, .. }));
        assert!(r.next_item().unwrap().is_none());
    }

    #[test]
    fn bad_series_are_refused_where_their_key_is_made() {
        assert_eq!(SeriesKey::new("", [], Kind::Gauge).err(), Some(BatchError::EmptyMetric));
        assert_eq!(SeriesKey::new("m", [("", "v")], Kind::Gauge).err(), Some(BatchError::EmptyKey));
        assert_eq!(SeriesKey::new("m", [("a", "1"), ("a", "2")], Kind::Gauge).err(), Some(BatchError::DuplicateKey("a".into())));
        let long = "x".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(SeriesKey::new(&long, [], Kind::Gauge).err(), Some(BatchError::TooLong(MAX_NAME_BYTES + 1)));
        let many: Vec<(String, String)> = (0..=MAX_TAGS).map(|i| (format!("k{i}"), "v".to_owned())).collect();
        assert!(matches!(SeriesKey::new("m", many.iter().map(|(k, v)| (k.as_str(), v.as_str())), Kind::Gauge), Err(BatchError::TooManyTags(_))));
    }

    #[test]
    fn malformed_batches_are_errors_never_panics() {
        let mut b = Builder::new();
        for i in 0..20 {
            let k = SeriesKey::new("m", [("host", format!("h{i}").as_str())], Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, (0..50).map(|j| (j * 1000 + i, j as f64)));
        }
        let batch = b.finish();
        assert_eq!(Reader::new(&[9]).err(), Some(BatchError::Version(9)));
        let walk = |bytes: &[u8]| {
            if let Ok(mut r) = Reader::new(bytes) {
                while let Ok(Some(item)) = r.next_item() {
                    match item {
                        Item::Def { key, .. } => {
                            let _ = parse_key(key);
                        }
                        Item::Run { samples, .. } => {
                            for s in samples.iter() {
                                if s.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        };
        for cut in 0..batch.len() {
            walk(&batch[..cut]);
        }
        let mut rng = crate::testkit::Rng::new(11);
        for _ in 0..3000 {
            let mut bad = batch.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] = rng.next_u64() as u8;
            walk(&bad);
        }
    }
}
