//! A latency histogram in HdrHistogram's layout (Tene): values below 2^11 ns kept exactly, each
//! power of two above split into 1024 linear sub-buckets, so every recorded value is within
//! 1/1024 (< 0.1%) of the truth, from a nanosecond to past an hour, in fixed memory.

use std::time::Duration;

const SUB: u32 = 11;
const HALF: usize = 1 << (SUB - 1);
/// Exact below 2^11, then 1024 buckets for each of the 53 powers of two up to 2^64.
const LEN: usize = (1 << SUB) + (64 - SUB as usize) * HALF;

pub struct Hist {
    counts: Vec<u64>,
    n: u64,
    max: u64,
}

impl Default for Hist {
    fn default() -> Hist {
        Hist { counts: vec![0; LEN], n: 0, max: 0 }
    }
}

fn index(v: u64) -> usize {
    let bits = 64 - v.leading_zeros();
    if bits <= SUB {
        return usize::try_from(v).unwrap_or(0);
    }
    let shift = bits - SUB;
    (1 << SUB) + (shift as usize - 1) * HALF + (usize::try_from(v >> shift).unwrap_or(0) - HALF)
}

/// The highest value bucket `i` holds (what a percentile reports: never below the truth).
fn top(i: usize) -> u64 {
    if i < (1 << SUB) {
        return i as u64;
    }
    let j = i - (1 << SUB);
    let shift = (j / HALF) as u32 + 1;
    let sub = (j % HALF + HALF) as u64;
    ((sub + 1) << shift) - 1
}

impl Hist {
    pub fn add(&mut self, d: Duration) {
        let v = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        if let Some(c) = self.counts.get_mut(index(v)) {
            *c += 1;
        }
        self.n += 1;
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.n += o.n;
        self.max = self.max.max(o.max);
    }

    /// The value at or below which `p` (0..=1) of the recorded values lie.
    pub fn at(&self, p: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let want = ((p * self.n as f64).ceil() as u64).clamp(1, self.n);
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return top(i).min(self.max);
            }
        }
        self.max
    }

    pub fn json(&self) -> String {
        format!(
            "{{\"n\":{},\"p50_ns\":{},\"p90_ns\":{},\"p99_ns\":{},\"p999_ns\":{},\"max_ns\":{}}}",
            self.n,
            self.at(0.5),
            self.at(0.9),
            self.at(0.99),
            self.at(0.999),
            self.max
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lb14_every_value_within_a_thousandth() {
        for v in [0u64, 1, 2047, 2048, 2049, 4095, 4096, 1_000_003, 123_456_789_012, u64::MAX / 3] {
            let i = index(v);
            assert!(top(i) >= v, "{v}: bucket top {} below it", top(i));
            assert!(top(i) - v <= v / 1024 + 1, "{v}: bucket top {} too far", top(i));
            assert!(i < LEN);
        }
    }

    #[test]
    fn lb14_percentiles() {
        let mut h = Hist::default();
        for v in 1..=1000u64 {
            h.add(Duration::from_nanos(v * 1000));
        }
        let p50 = h.at(0.5);
        assert!((499_000..=501_000).contains(&p50), "{p50}");
        assert_eq!(h.at(1.0), 1_000_000);
    }
}
