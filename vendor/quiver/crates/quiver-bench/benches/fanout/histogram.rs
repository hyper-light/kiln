//! Latencies in nanoseconds, in log-linear buckets: exact below 128, then 64 buckets each power of
//! two (within 1.6% of the value). Deliveries owed that never came are counted too, past every one
//! that did, so a system that drops messages can't show a tail better than its losses.

#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    /// Deliveries recorded.
    pub total: u64,
    pub max: u64,
    /// Deliveries owed that never came.
    pub undelivered: u64,
}

const SUB: u32 = 6;

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: vec![0; 64 * 64 + 128],
            total: 0,
            max: 0,
            undelivered: 0,
        }
    }

    fn index(v: u64) -> usize {
        if v < 128 {
            return v as usize;
        }
        let msb = 63 - v.leading_zeros();
        let shift = msb - SUB;
        ((shift as usize + 1) << SUB) + ((v >> shift) & 63) as usize
    }

    /// The largest value bucket `i` holds.
    fn upper(i: usize) -> u64 {
        if i < 128 {
            return i as u64;
        }
        let shift = (i >> SUB) as u32 - 1;
        let sub = (i & 63) as u64;
        ((64 + sub) << shift) | ((1 << shift) - 1)
    }

    pub fn record(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.total += 1;
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
        self.max = self.max.max(other.max);
        self.undelivered += other.undelivered;
    }

    /// The value at quantile `q` of every delivery owed: the upper end of the bucket its rank is in,
    /// or `None` where the rank is past every delivery that came.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        let owed = self.total + self.undelivered;
        if owed == 0 {
            return Some(0);
        }
        let rank = ((q * owed as f64).ceil() as u64).clamp(1, owed);
        if rank > self.total {
            return None;
        }
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Some(Self::upper(i).min(self.max));
            }
        }
        Some(self.max)
    }

    /// The largest latency, or `None` if a delivery owed never came.
    pub fn largest(&self) -> Option<u64> {
        if self.undelivered > 0 {
            None
        } else {
            Some(self.max)
        }
    }
}
