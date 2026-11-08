//! Latencies in constant memory, whatever the rate: HdrHistogram's log-linear buckets. Every value under
//! 2^[`SUB_BITS`] (1,024 ns) has a bucket of its own; each octave past that is split in [`HALF`] buckets, so a
//! value's bucket is within 1/[`HALF`] (0.2%) of it. At a million requests a second for ten seconds the lab
//! keeps 28,672 counters, not ten million samples, and the generator's threads add theirs together at the end.
//! Quantiles are by the nearest rank, as `crate::measure::quantile`'s are: the smallest value with at least `q`
//! of the samples at or under it, here its bucket's highest value (never past the largest seen).

/// Values under 2^10 ns (a microsecond) are kept exact; past it, 2^9 buckets an octave (0.2%).
const SUB_BITS: u32 = 10;
const SUB: u64 = 1 << SUB_BITS;
const HALF: u64 = SUB / 2;
/// The exact buckets, then an octave of [`HALF`] buckets for each bit past [`SUB_BITS`]: 28,672.
const BUCKETS: usize = (SUB + (64 - SUB_BITS as u64) * HALF) as usize;

/// Nanoseconds (or anything counted in u64), by bucket.
#[derive(Clone, Debug)]
pub struct Hist {
    counts: Vec<u64>,
    n: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Hist {
    fn default() -> Self {
        Self { counts: vec![0; BUCKETS], n: 0, min: u64::MAX, max: 0, sum: 0 }
    }
}

/// `v`'s bucket.
fn index(v: u64) -> usize {
    if v < SUB {
        return usize::try_from(v).unwrap_or(0);
    }
    let msb = 63 - v.leading_zeros();
    // `v >> shift` is in [HALF, SUB): its top SUB_BITS bits.
    let shift = msb - (SUB_BITS - 1);
    let m = v >> shift;
    usize::try_from(SUB + u64::from(shift - 1) * HALF + (m - HALF)).unwrap_or(BUCKETS - 1)
}

/// The highest value bucket `i` holds.
fn highest(i: usize) -> u64 {
    let i = i as u64;
    if i < SUB {
        return i;
    }
    let past = i - SUB;
    let shift = past / HALF + 1;
    let m = HALF + past % HALF;
    (m << shift).saturating_add((1u64 << shift) - 1)
}

impl Hist {
    pub fn add(&mut self, v: u64) {
        if let Some(c) = self.counts.get_mut(index(v)) {
            *c += 1;
        }
        self.n += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += u128::from(v);
    }

    /// Another's samples added to this one's.
    pub fn merge(&mut self, other: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += *b;
        }
        self.n += other.n;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    #[cfg(test)]
    pub fn n(&self) -> u64 {
        self.n
    }

    pub fn max(&self) -> Option<u64> {
        (self.n > 0).then_some(self.max)
    }

    pub fn mean(&self) -> Option<f64> {
        #[allow(clippy::cast_precision_loss)]
        let mean = self.sum as f64 / self.n as f64;
        (self.n > 0).then_some(mean)
    }

    /// `q` (0 to 1) by the nearest rank: the smallest bucket with at least `q` of the samples at or under it,
    /// its highest value, never past the largest seen; the smallest and the largest exact; none without samples.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.n == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rank = ((q.clamp(0.0, 1.0) * self.n as f64).ceil() as u64).clamp(1, self.n);
        if rank == 1 {
            return Some(self.min);
        }
        if rank == self.n {
            return Some(self.max);
        }
        let mut seen = 0u64;
        for (i, c) in self.counts.iter().enumerate() {
            seen += *c;
            if seen >= rank {
                return Some(highest(i).min(self.max).max(self.min));
            }
        }
        Some(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standin::Lcg;

    /// The buckets tile every u64: each value falls in the bucket whose range holds it, the next bucket begins
    /// one past the last one's highest value, and the last ends at u64::MAX.
    #[test]
    fn rate_the_buckets_tile_every_value() {
        for v in (0..5000u64).chain([u64::MAX, u64::MAX - 1, 1 << 40, (1 << 40) - 1, 1 << 63]) {
            let i = index(v);
            assert!(highest(i) >= v, "{v} in bucket {i} past its highest {}", highest(i));
            assert!(i == 0 || highest(i - 1) < v, "{v} belongs to bucket {} too", i - 1);
        }
        assert_eq!(index(u64::MAX), BUCKETS - 1);
        assert_eq!(highest(BUCKETS - 1), u64::MAX);
        for i in 1..BUCKETS {
            assert_eq!(index(highest(i - 1) + 1), i, "bucket {i} begins right after bucket {}", i - 1);
        }
    }

    /// Below a microsecond the quantiles are exact; past it they're within 0.2% of the exact nearest rank, and
    /// never under it.
    #[test]
    fn rate_quantiles_are_the_nearest_rank_within_a_fifth_of_a_percent() {
        let mut exact = Hist::default();
        for v in 1..=1000u64 {
            exact.add(v);
        }
        assert_eq!((exact.quantile(0.5), exact.quantile(0.99), exact.quantile(0.999), exact.quantile(1.0)), (Some(500), Some(990), Some(999), Some(1000)));
        let mut r = Lcg(0x5eed);
        let mut h = Hist::default();
        // Latencies from 20 µs to about 2 s, most near the bottom, a long tail, as a server's are.
        let mut v: Vec<u64> = (0..200_000).map(|_| 20_000 + r.below(300_000) * (1 + r.below(8) * r.below(8) * r.below(8))).collect();
        for x in &v {
            h.add(*x);
        }
        v.sort_unstable();
        for q in [0.0, 0.25, 0.5, 0.9, 0.99, 0.999, 0.9999, 1.0] {
            let want = crate::measure::quantile(&v, q).unwrap_or(0);
            let got = h.quantile(q).unwrap_or(0);
            assert!(got >= want, "q {q}: {got} under the exact {want}");
            #[allow(clippy::cast_precision_loss)]
            let off = (got - want) as f64 / want as f64;
            assert!(off <= 1.0 / HALF as f64, "q {q}: {got} is {off} past the exact {want}");
        }
        assert_eq!(h.quantile(1.0), v.last().copied(), "the largest is exact");
        assert_eq!(h.quantile(0.0), v.first().copied(), "the smallest is exact");
        assert_eq!(Hist::default().quantile(0.5), None);
    }

    /// Two threads' histograms added together are the histogram of all their samples.
    #[test]
    fn rate_merged_histograms_are_all_their_samples() {
        let (mut a, mut b, mut all) = (Hist::default(), Hist::default(), Hist::default());
        let mut r = Lcg(7);
        for i in 0..10_000u64 {
            let x = r.below(5_000_000);
            if i % 3 == 0 { a.add(x) } else { b.add(x) }
            all.add(x);
        }
        a.merge(&b);
        assert_eq!(a.n(), all.n());
        for q in [0.5, 0.99, 0.999, 1.0] {
            assert_eq!(a.quantile(q), all.quantile(q));
        }
        assert_eq!(a.mean(), all.mean());
    }
}
