//! What a run reports: a hash of everything it did, in order, so two runs of one seed are compared
//! by one number, and its placement latencies.

use crate::model::Time;

/// FNV-1a, 64-bit (Fowler, Noll and Vo; the parameters of IETF draft-eastlake-fnv). Not a
/// cryptographic hash: it tells two runs apart, it doesn't defend against a forger.
#[derive(Debug, Clone)]
pub struct TraceHash(u64);

impl Default for TraceHash {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl TraceHash {
    pub fn word(&mut self, word: u64) {
        for byte in word.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Placement latencies in virtual nanoseconds, one per placement, so bounded by the run's workloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Latencies {
    sorted: Vec<Time>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quantiles {
    pub count: usize,
    pub p50: Time,
    pub p99: Time,
    pub p999: Time,
    pub max: Time,
}

impl Latencies {
    pub fn new(mut all: Vec<Time>) -> Self {
        all.sort_unstable();
        Self { sorted: all }
    }

    /// The nearest-rank quantile at `per_mille / 1000`, or `None` with no samples.
    pub fn at(&self, per_mille: usize) -> Option<Time> {
        let n = self.sorted.len();
        // rank = ceil(n * q), at least 1.
        let rank = n.checked_mul(per_mille)?.div_ceil(1000).max(1);
        self.sorted.get(rank.checked_sub(1)?).copied()
    }

    pub fn quantiles(&self) -> Option<Quantiles> {
        Some(Quantiles {
            count: self.sorted.len(),
            p50: self.at(500)?,
            p99: self.at(990)?,
            p999: self.at(999)?,
            max: *self.sorted.last()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_quantiles() {
        let l = Latencies::new((1..=1000).rev().collect());
        let q = l.quantiles().unwrap();
        assert_eq!(
            (q.count, q.p50, q.p99, q.p999, q.max),
            (1000, 500, 990, 999, 1000)
        );
        assert_eq!(Latencies::new(vec![]).quantiles(), None);
        assert_eq!(Latencies::new(vec![7]).at(990), Some(7));
    }

    #[test]
    fn the_hash_sees_order() {
        let (mut a, mut b) = (TraceHash::default(), TraceHash::default());
        a.word(1);
        a.word(2);
        b.word(2);
        b.word(1);
        assert_ne!(a.finish(), b.finish());
    }
}
