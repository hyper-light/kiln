//! A fixed-size log histogram of positive values: four buckets per power of two, so a quantile read
//! from it is within 25% of the true value with a fixed 1 KiB per histogram however many values come.
//! Nothing is dropped: every value is counted in its bucket (CLAUDE.md §3).

use crate::HealthError;

/// Sub-buckets per power of two.
const SUB_BITS: u32 = 2;
const SUB: usize = 1 << SUB_BITS;
/// 64 powers of two of `SUB` buckets each cover every `u64`.
pub const BUCKETS: usize = 64 * SUB;

#[derive(Debug, Clone)]
pub struct Histogram {
    counts: Vec<u32>,
    total: u32,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS],
            total: 0,
        }
    }

    pub fn total(&self) -> u32 {
        self.total
    }

    pub fn clear(&mut self) {
        self.counts.iter_mut().for_each(|c| *c = 0);
        self.total = 0;
    }

    pub fn record(&mut self, value: u64) -> Result<(), HealthError> {
        self.record_n(value, 1)
    }

    /// Counts `value` `n` times: a recorded distribution taken in at once.
    pub fn record_n(&mut self, value: u64, n: u32) -> Result<(), HealthError> {
        let full = HealthError::Full {
            what: "histogram count",
            bound: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
        };
        let slot = self
            .counts
            .get_mut(bucket(value))
            .ok_or(HealthError::Overflow("histogram bucket"))?;
        let total = self.total.checked_add(n).ok_or(full.clone())?;
        *slot = slot.checked_add(n).ok_or(full)?;
        self.total = total;
        Ok(())
    }

    /// Each bucket that holds a value, in rising order: the highest value it holds, and its count.
    /// Every value counted in it is at most that highest value, and above the one before.
    pub fn counted(&self) -> impl Iterator<Item = (u64, u32)> + '_ {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(i, c)| (highest(i), *c))
    }

    /// The lower bound of the bucket holding the median, or `None` when empty.
    pub fn median(&self) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        // The lower median: the value at rank ceil(total / 2).
        let rank = self.total.div_ceil(2);
        let mut seen: u32 = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen = seen.saturating_add(*c);
            if seen >= rank {
                return Some(lower_bound(i));
            }
        }
        None
    }
}

fn bucket(value: u64) -> usize {
    if value < SUB as u64 {
        // 0..SUB map to themselves.
        return usize::try_from(value).unwrap_or(0);
    }
    let msb = 63u32.saturating_sub(value.leading_zeros());
    let shift = msb.saturating_sub(SUB_BITS);
    let sub = (value >> shift) & ((SUB as u64).saturating_sub(1));
    let index = u64::from(msb)
        .saturating_sub(u64::from(SUB_BITS))
        .saturating_add(1)
        .saturating_mul(SUB as u64)
        .saturating_add(sub);
    usize::try_from(index).unwrap_or(BUCKETS.saturating_sub(1))
}

/// The highest value bucket `index` holds: one below the next bucket's lower bound, or `u64::MAX` for
/// the top bucket, whose next would pass u64.
fn highest(index: usize) -> u64 {
    let next = index.checked_add(1).map(lower_bound).unwrap_or(0);
    if next > lower_bound(index) {
        next.saturating_sub(1)
    } else {
        u64::MAX
    }
}

fn lower_bound(index: usize) -> u64 {
    let i = u64::try_from(index).unwrap_or(u64::MAX);
    if i < SUB as u64 {
        return i;
    }
    let group = i >> SUB_BITS;
    let sub = i & ((SUB as u64).saturating_sub(1));
    let msb = group.saturating_add(u64::from(SUB_BITS)).saturating_sub(1);
    let shift = u32::try_from(msb.saturating_sub(u64::from(SUB_BITS))).unwrap_or(0);
    let lead = (SUB as u64).saturating_add(sub);
    lead.checked_shl(shift).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bucket of every value holds it, and its lower bound is within 25% below it.
    #[test]
    fn every_value_lands_in_a_bucket_within_a_quarter() {
        let mut v: u64 = 1;
        while v < u64::MAX / 3 {
            for x in [v, v + 1, v + v / 4, v + v / 2] {
                let b = bucket(x);
                assert!(b < BUCKETS, "{x}");
                let lo = lower_bound(b);
                assert!(lo <= x, "{x} {lo}");
                assert!(x - lo <= x / 4 + 1, "{x} {lo}");
            }
            v = v + v / 2 + 1;
        }
        assert!(bucket(u64::MAX) < BUCKETS);
        assert_eq!(bucket(0), 0);
    }

    /// A recorded distribution taken in at once reads as the same values recorded one by one: each
    /// bucket's highest value at or above every value counted in it, the buckets rising, none lost.
    #[test]
    fn counted_buckets_bound_their_values_and_lose_none() {
        let mut one = Histogram::new();
        let mut many = Histogram::new();
        let values = [
            0u64,
            3,
            4,
            5,
            7,
            8,
            1_000,
            1_249,
            1_250,
            147_330_000,
            u64::MAX,
        ];
        for v in values {
            one.record(v).unwrap();
            many.record_n(v, 3).unwrap();
        }
        let buckets: Vec<_> = one.counted().collect();
        assert!(buckets.windows(2).all(|w| w[0].0 < w[1].0));
        for v in values {
            let (hi, _) = buckets.iter().find(|(hi, _)| v <= *hi).unwrap();
            assert_eq!(bucket(*hi), bucket(v), "{v} {hi}");
        }
        assert_eq!(buckets.iter().map(|b| b.1).sum::<u32>(), 11);
        assert_eq!(many.total(), 33);
        assert!(
            many.counted()
                .zip(one.counted())
                .all(|(m, o)| m.0 == o.0 && m.1 == 3 * o.1)
        );
        let mut full = Histogram::new();
        full.record_n(9, u32::MAX).unwrap();
        assert!(full.record(9).is_err());
        assert_eq!(full.total(), u32::MAX);
    }

    /// The median of 1..=99 read from the histogram is within 25% of 50.
    #[test]
    fn the_median_is_within_a_quarter() {
        let mut h = Histogram::new();
        assert_eq!(h.median(), None);
        for x in 1..=99u64 {
            h.record(x * 1000).unwrap();
        }
        let m = h.median().unwrap();
        assert!((37_500..=50_000).contains(&m), "{m}");
        h.clear();
        assert_eq!(h.total(), 0);
    }
}
