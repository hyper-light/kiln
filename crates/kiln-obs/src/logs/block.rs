//! A log block: a run of records sealed together (`docs/observe.md` §4.6).
//!
//! Its rows sorted by time, encoded ([`super::record`]) and compressed as one text; beside the
//! text, what lets a search rule the block out without reading it: its time range, the services,
//! hosts and sources it holds, the attribute keys, the severities present, and a Bloom filter of
//! every token of its messages and attribute values at a 1% false-positive rate (§12.7).

use super::bloom::{Bloom, token_hashes, tokens};
use super::record::{Record, RecordRef, RowError, Rows, put_row, put_row_ref};
use crate::lz4;
use crate::varint;

/// A block's uncompressed text: the smallest size within 2% of LZ4's ratio plateau on kiln's logs
/// (`docs/observe.md` §12.6).
pub const BLOCK_TEXT: usize = 256 << 10;

/// The false-positive rate of a block's token filter: RocksDB's default (10 bits a key, about 1%,
/// "RocksDB Bloom Filter" wiki) for the same trade, measured on kiln's logs at 8.3% of a block's
/// compressed text and 0.69 µs of wasted decompression a block a missing word (§12.7).
pub const BLOOM_RATE: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Lz4 = 0,
    Deflate = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LogBlockError {
    #[error(transparent)]
    Row(#[from] RowError),
    #[error("a log block's text does not decompress")]
    Text,
    #[error("a log block is malformed: {0}")]
    Malformed(&'static str),
}

/// What a block of spans' token filter leaves out: the values of a span's own numbers and ids and
/// its events' and links' JSON, a token or more each a span, which would double the filter for
/// searches no one makes by a word of them (a search of them is never ruled out by it).
pub const UNTOKENIZED: [&str; 4] = ["duration", "parent_id", "events", "links"];

/// A sealed block: its summary and its compressed text.
#[derive(Debug, Clone, PartialEq)]
pub struct LogBlock {
    pub min_ts: i64,
    pub max_ts: i64,
    pub rows: u32,
    pub services: Vec<String>,
    pub hosts: Vec<String>,
    pub sources: Vec<String>,
    pub attr_keys: Vec<String>,
    /// Bit `n` set: a row of severity number `n` (0–24).
    pub severities: u32,
    pub bloom: Bloom,
    pub codec: Codec,
    pub raw_len: u32,
    pub text: Vec<u8>,
}

fn insert_sorted(v: &mut Vec<String>, s: &str) {
    if let Err(i) = v.binary_search_by(|x| x.as_str().cmp(s)) {
        v.insert(i, s.to_owned());
    }
}

/// The tokens a block holds, each by its two hashes (`token_hashes`, mixed already), kept once
/// to size its filter. The table hashes them folded with a key this process draws, mixed once
/// (SplitMix64's finalizer): not SipHash's rounds, and still no sender can write tokens that
/// collide in it, as with the hashes taken as they are (they are fixed by the format: anyone may
/// compute them). Sorting them instead was measured slower: a log's words repeat.
type Seen = std::collections::HashSet<(u64, u64), Keyed>;

#[derive(Debug, Clone, Copy)]
struct Keyed(u64);

impl Keyed {
    fn drawn() -> Self {
        static KEY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        Self(*KEY.get_or_init(|| std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), 0x6b69_6c6e_u64)))
    }
}

impl std::hash::BuildHasher for Keyed {
    type Hasher = Folded;
    fn build_hasher(&self) -> Folded {
        Folded(self.0)
    }
}

struct Folded(u64);

impl std::hash::Hasher for Folded {
    fn finish(&self) -> u64 {
        super::bloom::mix(self.0)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(b);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = self.0.rotate_left(32) ^ x;
    }
}

/// What a seal works in, kept by the thread for its next (the text alone is past the allocator's
/// threshold for memory of its own, each new one faulted in a page at a time).
struct Scratch {
    order: Vec<usize>,
    raw: Vec<u8>,
    seen: Seen,
    buf: String,
}

impl Scratch {
    fn new() -> Self {
        Self { order: Vec::new(), raw: Vec::with_capacity(BLOCK_TEXT + BLOCK_TEXT / 8), seen: Seen::with_hasher(Keyed::drawn()), buf: String::new() }
    }

    /// What a seal left past what a block of logs takes let go, for the next to begin from: a
    /// wider block's (a merge's, a cut's whole head) is not kept for the thread's life.
    fn trim(&mut self) {
        if self.raw.capacity() > 4 * (BLOCK_TEXT + BLOCK_TEXT / 8) {
            self.raw = Vec::with_capacity(BLOCK_TEXT + BLOCK_TEXT / 8);
        }
        if self.seen.capacity() > SEEN_KEPT {
            self.seen = Seen::with_hasher(*self.seen.hasher());
        }
        if self.order.capacity() > SEEN_KEPT {
            self.order = Vec::new();
        }
    }
}

/// Tokens (and records, of the order) the scratch keeps room for between seals: a block of
/// logs' few thousand.
const SEEN_KEPT: usize = 1 << 16;

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

/// The block's distinct values of a field (its services, hosts, sources), sorted; the last one
/// taken remembered, as records of a block mostly repeat it.
struct Distinct<'a> {
    sorted: Vec<String>,
    last: Option<&'a str>,
}

impl<'a> Distinct<'a> {
    fn new() -> Self {
        Self { sorted: Vec::new(), last: None }
    }

    fn take(&mut self, s: &'a str) {
        if self.last != Some(s) {
            insert_sorted(&mut self.sorted, s);
            self.last = Some(s);
        }
    }
}

impl LogBlock {
    /// Seals records (in any order: sorted by time here, stably).
    pub fn seal(records: &[RecordRef<'_>]) -> Self {
        Self::seal_in(records, false)
    }

    /// As [`LogBlock::seal`], of spans when `spans`: the filter holds each one's trace id (its
    /// bytes' hashes, `super::bloom::id_hashes`) and not [`UNTOKENIZED`]'s values, every place
    /// salted by the block ([`LogBlock::salt`]).
    pub fn seal_in(records: &[RecordRef<'_>], spans: bool) -> Self {
        SCRATCH.with(|s| match s.try_borrow_mut() {
            Ok(mut scratch) => {
                let block = Self::seal_with(records, spans, &mut scratch);
                scratch.trim();
                block
            }
            Err(_) => Self::seal_with(records, spans, &mut Scratch::new()),
        })
    }

    fn seal_with(records: &[RecordRef<'_>], spans: bool, scratch: &mut Scratch) -> Self {
        let Scratch { order, raw, seen, buf } = scratch;
        order.clear();
        order.extend(0..records.len());
        order.sort_by_key(|&i| records.get(i).map_or(0, |r| r.ts));
        raw.clear();
        seen.clear();
        let mut prev = 0i64;
        let (mut services, mut hosts, mut sources, mut keys) = (Distinct::new(), Distinct::new(), Distinct::new(), Vec::new());
        // The last record's attribute keys, in its order: a record of the same kind names the same.
        let mut last_keys: Vec<&str> = Vec::new();
        let mut severities = 0u32;
        let (mut min_ts, mut max_ts) = (i64::MAX, i64::MIN);
        for &i in order.iter() {
            let Some(r) = records.get(i) else { continue };
            put_row_ref(raw, r, prev);
            prev = r.ts;
            min_ts = min_ts.min(r.ts);
            max_ts = max_ts.max(r.ts);
            services.take(r.service);
            hosts.take(r.host);
            sources.take(r.source);
            severities |= 1 << u32::from(r.severity.min(24));
            tokens(r.message, buf, |t| {
                seen.insert(token_hashes(t));
            });
            for (j, (k, v)) in r.attrs().enumerate() {
                match last_keys.get_mut(j) {
                    Some(last) if *last == k => {}
                    Some(last) => {
                        insert_sorted(&mut keys, k);
                        *last = k;
                    }
                    None => {
                        insert_sorted(&mut keys, k);
                        last_keys.push(k);
                    }
                }
                if spans && UNTOKENIZED.contains(&k) {
                    continue;
                }
                tokens(v, buf, |t| {
                    seen.insert(token_hashes(t));
                });
            }
            if spans && let Some(t) = r.trace_id {
                seen.insert(super::bloom::id_hashes(&t));
            }
        }
        let mut bloom = Bloom::with_rate(seen.len(), BLOOM_RATE);
        // A block of spans' filter salted by the block (`super::bloom::salted`); a block of logs'
        // as logs' have always been.
        let salt = spans.then(|| super::bloom::block_salt(if records.is_empty() { 0 } else { min_ts }, records.len() as u32));
        for &h in seen.iter() {
            bloom.insert(salt.map_or(h, |s| super::bloom::salted(h, s)));
        }
        let mut text = Vec::with_capacity(raw.len() / 4);
        lz4::compress(raw, &mut text);
        Self {
            min_ts: if records.is_empty() { 0 } else { min_ts },
            max_ts: if records.is_empty() { 0 } else { max_ts },
            rows: records.len() as u32,
            services: services.sorted,
            hosts: hosts.sorted,
            sources: sources.sorted,
            attr_keys: keys,
            severities,
            bloom,
            codec: Codec::Lz4,
            raw_len: raw.len() as u32,
            text,
        }
    }

    /// Seals owned records.
    pub fn seal_owned(records: &[Record]) -> Self {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in records {
            put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        let mut rows = Rows::new(&raw, records.len());
        let mut refs = Vec::with_capacity(records.len());
        while let Ok(Some(r)) = rows.next_row() {
            refs.push(r);
        }
        Self::seal(&refs)
    }

    /// The block's rows' encoding into `out` (reused across blocks).
    pub fn decompress(&self, out: &mut Vec<u8>) -> Result<(), LogBlockError> {
        self.decompress_text(&self.text, out)
    }

    /// As [`decompress`](Self::decompress), of this block's text held elsewhere (a file's block,
    /// read into a buffer: [`super::file::LogFile::read_text`]).
    pub fn decompress_text(&self, text: &[u8], out: &mut Vec<u8>) -> Result<(), LogBlockError> {
        out.clear();
        match self.codec {
            Codec::Lz4 => lz4::decompress(text, out, self.raw_len as usize).map_err(|_| LogBlockError::Text)?,
            Codec::Deflate => return Err(LogBlockError::Text),
        }
        if out.len() != self.raw_len as usize {
            return Err(LogBlockError::Text);
        }
        Ok(())
    }

    /// The rows, oldest first, from what [`LogBlock::decompress`] wrote.
    pub fn rows<'a>(&self, raw: &'a [u8]) -> Rows<'a> {
        Rows::new(raw, self.rows as usize)
    }

    /// Its filter's salt, a block of spans' (what `seal_in` salted it with).
    pub fn salt(&self) -> u64 {
        super::bloom::block_salt(self.min_ts, self.rows)
    }

    /// Bytes of its summary held in memory.
    pub fn summary_bytes(&self) -> usize {
        let strs = |v: &Vec<String>| v.iter().map(|s| s.len() + 24).sum::<usize>();
        strs(&self.services) + strs(&self.hosts) + strs(&self.sources) + strs(&self.attr_keys) + self.bloom.byte_len() + 64
    }

    /// The block's encoding: its summary, then its text.
    pub fn encode(&self, out: &mut Vec<u8>) {
        varint::put(out, varint::zig(self.min_ts));
        varint::put(out, (self.max_ts as u64).wrapping_sub(self.min_ts as u64));
        varint::put(out, u64::from(self.rows));
        for list in [&self.services, &self.hosts, &self.sources, &self.attr_keys] {
            varint::put(out, list.len() as u64);
            for s in list {
                varint::put(out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
        }
        out.extend_from_slice(&self.severities.to_le_bytes());
        out.push(self.bloom.k as u8);
        varint::put(out, self.bloom.bits.len() as u64);
        for w in &self.bloom.bits {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.push(self.codec as u8);
        varint::put(out, u64::from(self.raw_len));
        varint::put(out, self.text.len() as u64);
        out.extend_from_slice(&self.text);
    }

    /// Reads a block's encoding.
    pub fn decode(mut at: &[u8]) -> Result<Self, LogBlockError> {
        let bad = LogBlockError::Malformed;
        let at = &mut at;
        let min_ts = varint::zag(varint::get(at).ok_or(bad("min"))?);
        let max_ts = (min_ts as u64).wrapping_add(varint::get(at).ok_or(bad("max"))?) as i64;
        let rows = u32::try_from(varint::get(at).ok_or(bad("rows"))?).map_err(|_| bad("rows"))?;
        let mut lists: [Vec<String>; 4] = Default::default();
        for list in &mut lists {
            let n = usize::try_from(varint::get(at).ok_or(bad("list"))?).map_err(|_| bad("list"))?;
            if n > at.len() {
                return Err(bad("list length"));
            }
            for _ in 0..n {
                let len = usize::try_from(varint::get(at).ok_or(bad("string"))?).map_err(|_| bad("string"))?;
                let (s, rest) = at.split_at_checked(len).ok_or(bad("string"))?;
                *at = rest;
                list.push(std::str::from_utf8(s).map_err(|_| bad("utf-8"))?.to_owned());
            }
        }
        let (sev, rest) = at.split_first_chunk::<4>().ok_or(bad("severities"))?;
        *at = rest;
        let (&k, rest) = at.split_first().ok_or(bad("k"))?;
        *at = rest;
        let words = usize::try_from(varint::get(at).ok_or(bad("bloom"))?).map_err(|_| bad("bloom"))?;
        if words.saturating_mul(8) > at.len() {
            return Err(bad("bloom length"));
        }
        let mut bits = Vec::with_capacity(words);
        for _ in 0..words {
            let (w, rest) = at.split_first_chunk::<8>().ok_or(bad("bloom"))?;
            *at = rest;
            bits.push(u64::from_le_bytes(*w));
        }
        let (&codec, rest) = at.split_first().ok_or(bad("codec"))?;
        *at = rest;
        let codec = match codec {
            0 => Codec::Lz4,
            1 => Codec::Deflate,
            _ => return Err(bad("codec")),
        };
        let raw_len = u32::try_from(varint::get(at).ok_or(bad("raw length"))?).map_err(|_| bad("raw length"))?;
        let len = usize::try_from(varint::get(at).ok_or(bad("text"))?).map_err(|_| bad("text"))?;
        let (text, rest) = at.split_at_checked(len).ok_or(bad("text"))?;
        *at = rest;
        if !at.is_empty() {
            return Err(bad("bytes after the text"));
        }
        let [services, hosts, sources, attr_keys] = lists;
        Ok(Self { min_ts, max_ts, rows, services, hosts, sources, attr_keys, severities: u32::from_le_bytes(*sev), bloom: Bloom { bits, k: u32::from(k).clamp(1, 30) }, codec, raw_len, text: text.to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::Record;

    fn recs(n: i64) -> Vec<Record> {
        (0..n)
            .map(|i| Record {
                ts: 1_000_000_000 * (n - i),
                severity: [9u8, 17][(i % 2) as usize],
                service: ["api", "web"][(i % 2) as usize].into(),
                host: format!("h{}", i % 3),
                source: "kiln".into(),
                message: format!("job {i} finished with exit {}", i % 4),
                attrs: vec![("job".into(), format!("j{i}"))],
                trace_id: None,
                span_id: None,
            })
            .collect()
    }

    /// A seal as the format was first written: a character at a time, every value searched for
    /// in its list, the tokens' hashes in std's set.
    fn sealed_by_the_book(records: &[RecordRef<'_>], spans: bool) -> LogBlock {
        let mut order: Vec<usize> = (0..records.len()).collect();
        order.sort_by_key(|&i| records[i].ts);
        let mut raw = Vec::new();
        let mut prev = 0i64;
        let (mut services, mut hosts, mut sources, mut keys) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut severities = 0u32;
        let mut seen = std::collections::HashSet::new();
        let words = |text: &str, seen: &mut std::collections::HashSet<(u64, u64)>| {
            let mut buf = String::new();
            for c in text.chars().chain([' ']) {
                if c.is_alphanumeric() {
                    buf.extend(c.to_lowercase());
                } else if !buf.is_empty() {
                    seen.insert(token_hashes(&buf));
                    buf.clear();
                }
            }
        };
        let (mut min_ts, mut max_ts) = (i64::MAX, i64::MIN);
        for &i in &order {
            let r = &records[i];
            crate::logs::record::put_fields(&mut raw, r.ts, r.severity, [r.service, r.host, r.source], r.message, r.attrs(), r.attr_count, r.trace_id, r.span_id, prev, usize::MAX);
            prev = r.ts;
            min_ts = min_ts.min(r.ts);
            max_ts = max_ts.max(r.ts);
            insert_sorted(&mut services, r.service);
            insert_sorted(&mut hosts, r.host);
            insert_sorted(&mut sources, r.source);
            severities |= 1 << u32::from(r.severity.min(24));
            words(r.message, &mut seen);
            for (k, v) in r.attrs() {
                insert_sorted(&mut keys, k);
                if !(spans && UNTOKENIZED.contains(&k)) {
                    words(v, &mut seen);
                }
            }
            if spans && let Some(t) = r.trace_id {
                seen.insert(crate::logs::bloom::id_hashes(&t));
            }
        }
        let mut bloom = Bloom::with_rate(seen.len(), BLOOM_RATE);
        let salt = spans.then(|| crate::logs::bloom::block_salt(if records.is_empty() { 0 } else { min_ts }, records.len() as u32));
        for h in seen {
            bloom.insert(salt.map_or(h, |s| crate::logs::bloom::salted(h, s)));
        }
        let mut text = Vec::new();
        lz4::compress(&raw, &mut text);
        LogBlock {
            min_ts: if records.is_empty() { 0 } else { min_ts },
            max_ts: if records.is_empty() { 0 } else { max_ts },
            rows: records.len() as u32,
            services,
            hosts,
            sources,
            attr_keys: keys,
            severities,
            bloom,
            codec: Codec::Lz4,
            raw_len: raw.len() as u32,
            text,
        }
    }

    /// Blocks of records of every shape (fields repeated and changing, attributes in any order
    /// and number, text past ASCII; logs and spans, trace ids and the keys spans leave out of the
    /// filter; one record, and none) seal to the same block as the format was first written, to the
    /// byte, through one thread's scratch from block to block.
    #[test]
    fn a_seal_writes_the_block_the_format_first_wrote() {
        let mut rng = crate::testkit::Rng::new(11);
        let pick = |rng: &mut crate::testkit::Rng, of: &[&'static str]| of[rng.below(of.len() as u64) as usize];
        let words = ["GET", "/api/v1/orders/", "42", "paid", "in", "ms", "Straße", "İstanbul", "ΣΊΣΥΦΟΣ", "東京", "😀", "e\u{301}", "-", " ", "=", "x1Y2", "ﬃ"];
        for case in 0..60 {
            let n = match case {
                0 => 0,
                1 => 1,
                _ => 1 + rng.below(1500) as usize,
            };
            let spans = case % 3 == 2;
            let records: Vec<Record> = (0..n)
                .map(|i| {
                    let mut attrs: Vec<(String, String)> = Vec::new();
                    let keys = ["http.route", "user.id", "duration", "parent_id", "thread.name", "events", "status_code"];
                    for _ in 0..rng.below(6) {
                        let k = pick(&mut rng, &keys);
                        if attrs.iter().all(|(a, _)| a != k) {
                            let v: String = (0..1 + rng.below(4)).map(|_| pick(&mut rng, &words)).collect();
                            attrs.push((k.to_owned(), v));
                        }
                    }
                    if rng.below(4) != 0 {
                        // Most records of a kind name their keys alike, in one order.
                        attrs.sort();
                    }
                    let message: String = (0..1 + rng.below(10)).map(|_| pick(&mut rng, &words)).collect();
                    let services: &[&'static str] = if rng.below(8) == 0 { &["shop", "web", "api"] } else { &["shop"] };
                    Record {
                        ts: 1_700_000_000_000_000_000 + rng.below(60_000_000_000) as i64,
                        severity: [9u8, 13, 17][rng.below(3) as usize],
                        service: pick(&mut rng, services).into(),
                        host: format!("host-{}", if rng.below(16) == 0 { rng.below(5) } else { i as u64 / 300 }),
                        source: pick(&mut rng, &["otlp", "otlp", "otlp", "kiln.event"]).into(),
                        message,
                        attrs,
                        trace_id: (spans || rng.below(2) == 0).then(|| std::array::from_fn(|b| (rng.next_u64() >> (b % 8)) as u8)),
                        span_id: None,
                    }
                })
                .collect();
            let (mut raw, mut prev) = (Vec::new(), 0);
            for r in &records {
                put_row(&mut raw, r, prev);
                prev = r.ts;
            }
            let mut rows = Rows::new(&raw, records.len());
            let mut refs = Vec::new();
            while let Some(r) = rows.next_row().unwrap() {
                refs.push(r);
            }
            assert_eq!(LogBlock::seal_in(&refs, spans), sealed_by_the_book(&refs, spans), "case {case}: {n} records, spans {spans}");
        }
    }

    #[test]
    fn a_sealed_block_holds_its_rows_in_time_order_and_summarises_them() {
        let records = recs(500);
        let b = LogBlock::seal_owned(&records);
        assert_eq!(b.rows, 500);
        assert_eq!((b.min_ts, b.max_ts), (1_000_000_000, 500_000_000_000));
        assert_eq!(b.services, vec!["api", "web"]);
        assert_eq!(b.hosts, vec!["h0", "h1", "h2"]);
        assert_eq!(b.attr_keys, vec!["job"]);
        assert_eq!(b.severities, (1 << 9) | (1 << 17));
        for word in ["job", "finished", "exit", "j42"] {
            assert!(b.bloom.may_contain(token_hashes(word)), "{word}");
        }
        let mut raw = Vec::new();
        b.decompress(&mut raw).unwrap();
        let mut rows = b.rows(&raw);
        let mut last = i64::MIN;
        let mut n = 0;
        while let Some(r) = rows.next_row().unwrap() {
            assert!(r.ts >= last);
            last = r.ts;
            n += 1;
        }
        assert_eq!(n, 500);
        // Encoded and decoded: the same block.
        let mut enc = Vec::new();
        b.encode(&mut enc);
        assert_eq!(LogBlock::decode(&enc).unwrap(), b);
    }

    #[test]
    fn a_damaged_encoding_is_an_error_never_a_panic() {
        let b = LogBlock::seal_owned(&recs(100));
        let mut enc = Vec::new();
        b.encode(&mut enc);
        let mut rng = crate::testkit::Rng::new(3);
        let mut raw = Vec::new();
        for _ in 0..3000 {
            let mut bad = enc.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] = rng.next_u64() as u8;
            if let Ok(d) = LogBlock::decode(&bad[..rng.below(bad.len() as u64 + 1) as usize])
                && d.decompress(&mut raw).is_ok()
            {
                let mut rows = d.rows(&raw);
                while let Ok(Some(_)) = rows.next_row() {}
            }
        }
    }
}
