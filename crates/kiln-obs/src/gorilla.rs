//! Gorilla's compression of a series' samples (Pelkonen et al., "Gorilla: a fast, scalable,
//! in-memory time series database", VLDB 2015, §4.1; `docs/observe.md` §4.3).
//!
//! A chunk holds a run of samples with strictly increasing timestamps, in milliseconds:
//! - the first sample's timestamp and value in 64 bits each;
//! - each later timestamp as its delta-of-delta (the change in the gap from the one before), in the
//!   smallest of the buckets [`DOD_BITS`] names, behind a prefix of ones ended by a zero, or in 64
//!   bits behind a prefix of ones alone;
//! - each later value as its XOR with the one before: a zero bit when equal; else a one, then a
//!   zero and the meaningful bits when they fall inside the previous window of leading and
//!   trailing zeros, or a one, the leading zeros in 5 bits, the meaningful length in 6 (64 written
//!   as 0) and the meaningful bits.
//!
//! Arithmetic on timestamps wraps, alike on both sides, so every `i64` round-trips and no input
//! overflows.

use crate::bits::{BitReader, BitWriter, Bits, Truncated};

/// The delta-of-delta buckets, in bits, each behind its prefix (`0` for a zero delta-of-delta,
/// then `10`, `110`, `1110` for these, `1111` and 64 bits beyond them).
///
/// Gorilla's are 7, 9 and 12 bits for second timestamps (§4.1.1); Prometheus's TSDB
/// (`tsdb/chunkenc/xor.go`) widens them to 14, 17 and 20 for milliseconds. Measured on 16.3 M
/// recorded samples (`docs/observe.md` §12.2), 91.7% of delta-of-deltas are zero and the rest
/// cluster at 3–8 bits (scrape jitter) and 11–16 (a missed scrape): the cheapest set whose outer
/// bucket keeps Prometheus's ±524 s (so a missed 60 s export or a restart stays out of the 68-bit
/// fallback) is 7, 14 and 20, 2.124 timestamp bits a sample against Prometheus's 2.290.
pub const DOD_BITS: [u32; 3] = [7, 14, 20];

/// Bits that state a value's leading zeros (Gorilla §4.1.2): counts above 31 are written as 31,
/// their extra zeros carried among the meaningful bits.
const LEADING_BITS: u32 = 5;
/// The most leading zeros [`LEADING_BITS`] can state.
const LEADING_MAX: u32 = (1 << LEADING_BITS) - 1;
/// Bits that state a value's meaningful length, 1 to 64 (64 written as 0).
const LENGTH_BITS: u32 = 6;

/// A sample that cannot go in a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PushError {
    /// Its timestamp is not after the chunk's last.
    #[error("timestamp {ts} is not after the chunk's last, {last}")]
    NotAfter { ts: i64, last: i64 },
}

/// A chunk that does not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("the chunk ended early")]
    Truncated,
    #[error("the chunk's timestamps do not increase")]
    NotIncreasing,
}

impl From<Truncated> for DecodeError {
    fn from(_: Truncated) -> Self {
        Self::Truncated
    }
}

/// Encodes samples into a chunk.
#[derive(Debug, Clone, Default)]
pub struct Encoder {
    w: BitWriter,
    count: u32,
    first: i64,
    t: i64,
    /// The last gap, as wrapping arithmetic leaves it.
    delta: u64,
    v: u64,
    /// The window of the last value written with its own: leading and trailing zeros.
    window: Option<(u32, u32)>,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// An encoder whose first `bytes` bytes need no reallocation.
    pub fn with_capacity(bytes: usize) -> Self {
        Self { w: BitWriter::with_capacity(bytes), ..Self::default() }
    }

    /// Appends a sample; its timestamp must be after the last one's.
    #[inline]
    pub fn push(&mut self, ts: i64, value: f64) -> Result<(), PushError> {
        let bits = value.to_bits();
        if self.count == 0 {
            self.w.write(ts as u64, 64);
            self.w.write(bits, 64);
            self.first = ts;
            self.t = ts;
            self.v = bits;
            self.count = 1;
            return Ok(());
        }
        if ts <= self.t {
            return Err(PushError::NotAfter { ts, last: self.t });
        }
        let delta = (ts as u64).wrapping_sub(self.t as u64);
        let dod = delta.wrapping_sub(self.delta) as i64;
        self.write_dod(dod);
        self.write_value(bits);
        self.t = ts;
        self.delta = delta;
        self.v = bits;
        self.count += 1;
        Ok(())
    }

    #[inline]
    fn write_dod(&mut self, dod: i64) {
        if dod == 0 {
            self.w.write_bit(false);
            return;
        }
        let mut prefix: u64 = 0b10;
        let mut prefix_len = 2;
        for &n in &DOD_BITS {
            if fits_signed(dod, n) {
                self.w.write(prefix, prefix_len);
                self.w.write(dod as u64, n);
                return;
            }
            prefix = (prefix << 1) | 0b10;
            prefix_len += 1;
        }
        // Every bucket's prefix bit set: ones alone, then the whole value.
        self.w.write((1u64 << (prefix_len - 1)) - 1, prefix_len - 1);
        self.w.write(dod as u64, 64);
    }

    #[inline]
    fn write_value(&mut self, bits: u64) {
        let x = bits ^ self.v;
        if x == 0 {
            self.w.write_bit(false);
            return;
        }
        let leading = x.leading_zeros().min(LEADING_MAX);
        let trailing = x.trailing_zeros();
        if let Some((wl, wt)) = self.window
            && leading >= wl
            && trailing >= wt
        {
            self.w.write(0b10, 2);
            self.w.write(x >> wt, 64 - wl - wt);
            return;
        }
        let len = 64 - leading - trailing;
        self.w.write(0b11, 2);
        self.w.write(u64::from(leading), LEADING_BITS);
        self.w.write(u64::from(len % 64), LENGTH_BITS);
        self.w.write(x >> trailing, len);
        self.window = Some((leading, trailing));
    }

    /// Samples in the chunk.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// The first and last timestamps, once there is a sample.
    pub fn span(&self) -> Option<(i64, i64)> {
        (self.count > 0).then_some((self.first, self.t))
    }

    /// Bytes the chunk takes.
    pub fn byte_len(&self) -> usize {
        self.w.byte_len()
    }

    /// Bytes allocated for it.
    pub fn capacity(&self) -> usize {
        self.w.capacity()
    }

    /// The chunk as it is, readable while samples are still appended.
    pub fn view(&self) -> ChunkRef<'_> {
        ChunkRef { bits: self.w.view(), count: self.count }
    }

    /// Forgets its samples, keeping its allocation.
    pub fn clear(&mut self) {
        self.w.clear();
        self.count = 0;
        self.first = 0;
        self.t = 0;
        self.delta = 0;
        self.v = 0;
        self.window = None;
    }

    /// Appends the chunk's bytes to `out`, the encoder left as it was.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        self.w.write_to(out);
    }

    /// The finished chunk.
    pub fn finish(self) -> Chunk {
        Chunk { first: self.first, last: self.t, count: self.count, bytes: self.w.finish() }
    }
}

/// Whether `v` fits `n` bits in two's complement.
#[inline]
fn fits_signed(v: i64, n: u32) -> bool {
    let half = 1i64 << (n - 1);
    v >= -half && v < half
}

/// A finished chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub first: i64,
    pub last: i64,
    pub count: u32,
    pub bytes: Vec<u8>,
}

impl Chunk {
    pub fn as_ref(&self) -> ChunkRef<'_> {
        ChunkRef { bits: Bits::bytes(&self.bytes), count: self.count }
    }
}

/// A chunk to read: its bits and how many samples they hold.
#[derive(Debug, Clone, Copy)]
pub struct ChunkRef<'a> {
    pub bits: Bits<'a>,
    pub count: u32,
}

impl<'a> ChunkRef<'a> {
    /// From stored bytes and the count stored beside them.
    pub fn new(bytes: &'a [u8], count: u32) -> Self {
        Self { bits: Bits::bytes(bytes), count }
    }

    pub fn iter(&self) -> Decoder<'a> {
        Decoder::new(*self)
    }

    /// Appends every sample to `ts` and `values`, the columns a query works on.
    pub fn decode_into(&self, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), DecodeError> {
        let n = self.count as usize;
        ts.reserve(n);
        values.reserve(n);
        let mut d = self.iter();
        while let Some((t, v)) = d.next_sample()? {
            ts.push(t);
            values.push(v);
        }
        Ok(())
    }
}

/// Decodes a chunk's samples in order.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    r: BitReader<'a>,
    left: u32,
    started: bool,
    t: i64,
    delta: u64,
    v: u64,
    window: (u32, u32),
    failed: bool,
}

impl<'a> Decoder<'a> {
    pub fn new(chunk: ChunkRef<'a>) -> Self {
        Self { r: BitReader::new(chunk.bits), left: chunk.count, started: false, t: 0, delta: 0, v: 0, window: (0, 0), failed: false }
    }

    /// The next sample, none after the last; an error ends the chunk.
    #[inline]
    pub fn next_sample(&mut self) -> Result<Option<(i64, f64)>, DecodeError> {
        if self.left == 0 || self.failed {
            return Ok(None);
        }
        match self.step() {
            Ok(s) => {
                self.left -= 1;
                Ok(Some(s))
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    #[inline]
    fn step(&mut self) -> Result<(i64, f64), DecodeError> {
        if !self.started {
            self.t = self.r.read(64)? as i64;
            self.v = self.r.read(64)?;
            self.started = true;
            return Ok((self.t, f64::from_bits(self.v)));
        }
        let dod = self.read_dod()?;
        let delta = self.delta.wrapping_add(dod as u64);
        let t = (self.t as u64).wrapping_add(delta) as i64;
        if t <= self.t {
            return Err(DecodeError::NotIncreasing);
        }
        self.t = t;
        self.delta = delta;
        if self.r.read_bit()? {
            if self.r.read_bit()? {
                let leading = self.r.read(LEADING_BITS)? as u32;
                let len = match self.r.read(LENGTH_BITS)? as u32 {
                    0 => 64,
                    l => l,
                };
                let trailing = 64u32.saturating_sub(leading).saturating_sub(len);
                let bits = self.r.read(len)?;
                self.v ^= bits.checked_shl(trailing).unwrap_or(0);
                self.window = (leading, trailing);
            } else {
                let (wl, wt) = self.window;
                let bits = self.r.read(64u32.saturating_sub(wl).saturating_sub(wt))?;
                self.v ^= bits.checked_shl(wt).unwrap_or(0);
            }
        }
        Ok((self.t, f64::from_bits(self.v)))
    }

    #[inline]
    fn read_dod(&mut self) -> Result<i64, DecodeError> {
        if !self.r.read_bit()? {
            return Ok(0);
        }
        for &n in &DOD_BITS {
            if !self.r.read_bit()? {
                let raw = self.r.read(n)?;
                // Sign-extend from n bits.
                let shift = 64 - n;
                return Ok(((raw << shift) as i64) >> shift);
            }
        }
        Ok(self.r.read(64)? as i64)
    }
}

impl Iterator for Decoder<'_> {
    type Item = Result<(i64, f64), DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_sample().transpose()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    fn round_trip(samples: &[(i64, f64)]) -> Chunk {
        let mut e = Encoder::new();
        for &(t, v) in samples {
            e.push(t, v).unwrap();
        }
        // The live view and the finished chunk decode alike, and to what went in, bit for bit.
        let mut live = Vec::new();
        for s in e.view().iter() {
            live.push(s.unwrap());
        }
        let chunk = e.finish();
        let mut ts = Vec::new();
        let mut vs = Vec::new();
        chunk.as_ref().decode_into(&mut ts, &mut vs).unwrap();
        assert_eq!(ts.len(), samples.len());
        for (i, &(t, v)) in samples.iter().enumerate() {
            assert_eq!(ts[i], t);
            assert_eq!(vs[i].to_bits(), v.to_bits(), "value {i}");
            assert_eq!(live[i].0, t);
            assert_eq!(live[i].1.to_bits(), v.to_bits());
        }
        chunk
    }

    #[test]
    fn every_delta_of_delta_bucket_and_its_edges() {
        let mut edges = vec![0i64, 1, -1];
        for &n in &DOD_BITS {
            let half = 1i64 << (n - 1);
            edges.extend([half - 1, half, -half, -half - 1]);
        }
        edges.extend([i64::MAX / 4, i64::MIN / 4]);
        for &dod in &edges {
            // Gaps of 1_000_000 ms, then one that differs by `dod` (kept positive).
            let base = 1_000_000i64;
            let gap2 = base + dod.max(-base + 1);
            round_trip(&[(0, 1.0), (base, 2.0), (base + gap2, 3.0), (base + gap2 + gap2, 4.0)]);
        }
    }

    #[test]
    fn values_round_trip_bit_for_bit_including_nan_infinities_signed_zero_and_subnormals() {
        let specials = [0.0, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE, 5e-324, f64::MAX, f64::MIN, 1.0, 1.0 + f64::EPSILON];
        let samples: Vec<(i64, f64)> = specials.iter().enumerate().map(|(i, &v)| (i as i64 * 10_000, v)).collect();
        round_trip(&samples);
        // A NaN with a payload keeps it.
        let payload = f64::from_bits(0x7ff8_0000_dead_beef);
        round_trip(&[(1, payload), (2, payload), (3, 0.0), (4, payload)]);
    }

    #[test]
    fn extreme_timestamps_wrap_alike_on_both_sides() {
        round_trip(&[(i64::MIN, 1.0), (i64::MIN + 1, 2.0), (0, 3.0), (i64::MAX - 1, 4.0), (i64::MAX, 5.0)]);
        round_trip(&[(-5, 1.0), (-4, 1.0), (-2, 1.0)]);
    }

    #[test]
    fn random_series_of_every_shape_round_trip() {
        let mut rng = Rng::new(42);
        for shape in 0..8 {
            for len in [1usize, 2, 3, 17, 120, 1000] {
                let mut t = rng.next_u64() as i64 / 4;
                let mut v = rng.f64() * 1e6;
                let mut samples = Vec::with_capacity(len);
                for i in 0..len {
                    t += match shape {
                        0 => 10_000,
                        1 => 10_000 + (rng.below(21) as i64 - 10),
                        2 => 1 + rng.below(1 << 40) as i64,
                        _ => 1 + rng.below(100_000) as i64,
                    };
                    v = match shape {
                        0 => v,
                        1 => v + rng.f64() - 0.5,
                        2 => f64::from_bits(rng.next_u64()),
                        3 => i as f64,
                        4 => (v + rng.below(5) as f64).floor(),
                        5 => (rng.f64() * 100.0).round() / 100.0,
                        6 => if rng.below(10) == 0 { rng.f64() } else { v },
                        _ => -v * 1.0001,
                    };
                    samples.push((t, v));
                }
                round_trip(&samples);
            }
        }
    }

    #[test]
    fn a_sample_not_after_the_last_is_refused_and_leaves_the_chunk_as_it_was() {
        let mut e = Encoder::new();
        e.push(100, 1.0).unwrap();
        assert_eq!(e.push(100, 2.0), Err(PushError::NotAfter { ts: 100, last: 100 }));
        assert_eq!(e.push(99, 2.0), Err(PushError::NotAfter { ts: 99, last: 100 }));
        e.push(101, 3.0).unwrap();
        let c = e.finish();
        let got: Vec<_> = c.as_ref().iter().map(Result::unwrap).collect();
        assert_eq!(got, vec![(100, 1.0), (101, 3.0)]);
    }

    #[test]
    fn corrupted_and_truncated_chunks_fail_without_panicking() {
        let mut rng = Rng::new(9);
        let samples: Vec<(i64, f64)> = (0..300).map(|i| (i * 15_000 + rng.below(30) as i64, rng.f64())).collect();
        let chunk = round_trip(&samples);
        // Every truncation.
        for cut in 0..chunk.bytes.len() {
            let r = ChunkRef::new(&chunk.bytes[..cut], chunk.count);
            let mut n = 0;
            for s in r.iter() {
                if s.is_err() {
                    break;
                }
                n += 1;
            }
            assert!(n < chunk.count as usize || cut == chunk.bytes.len());
        }
        // Random bit flips and random bytes: an error or wrong samples, never a panic.
        for _ in 0..2000 {
            let mut bytes = chunk.bytes.clone();
            let i = rng.below(bytes.len() as u64) as usize;
            bytes[i] ^= 1 << rng.below(8);
            for s in ChunkRef::new(&bytes, chunk.count).iter() {
                if s.is_err() {
                    break;
                }
            }
            let junk: Vec<u8> = (0..rng.below(64)).map(|_| rng.next_u64() as u8).collect();
            for s in ChunkRef::new(&junk, rng.below(500) as u32).iter() {
                if s.is_err() {
                    break;
                }
            }
        }
    }

    #[test]
    fn regular_samples_compress_as_gorilla_says() {
        // A gauge scraped every 10 s with small jitter, slowly varying: a few bits a sample.
        let mut rng = Rng::new(3);
        let mut e = Encoder::new();
        let mut v = 50.0f64;
        for i in 0..120 {
            v += (rng.below(3) as f64 - 1.0) * 0.5;
            e.push(1_700_000_000_000 + i * 10_000 + rng.below(4) as i64, v).unwrap();
        }
        let per_sample = e.byte_len() as f64 / 120.0;
        assert!(per_sample < 4.0, "{per_sample} bytes a sample");
    }
}
