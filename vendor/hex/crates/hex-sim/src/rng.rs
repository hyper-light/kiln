//! The one source of randomness in a run: SplitMix64 (Steele, Lea and Flood, "Fast Splittable
//! Pseudorandom Number Generators", OOPSLA 2014), seeded once, so a seed names a run.

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `low..=high` (`low` when the range is empty). The modulo's bias is below
    /// `span / 2^64`, immaterial for a generator of test clusters.
    pub fn range(&mut self, low: u64, high: u64) -> u64 {
        match high.checked_sub(low).and_then(|span| span.checked_add(1)) {
            // low + (x % width) <= high, so the add can't overflow; high stands in for the impossible.
            Some(width) => self
                .next_u64()
                .checked_rem(width)
                .and_then(|x| low.checked_add(x))
                .unwrap_or(high),
            // high - low + 1 overflowed: the range is every u64 from low.
            None if high >= low => self.next_u64(),
            None => low,
        }
    }

    /// True with probability `per_mille / 1000`.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.next_u64() % 1000 < per_mille
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-T7.4.1 (determinism under the model-based test): one seed, one sequence.
    #[test]
    fn a_seed_names_one_sequence() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let mut c = Rng::new(8);
        let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
        let zs: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(xs, zs);
    }

    #[test]
    fn range_stays_inside_its_bounds() {
        let mut r = Rng::new(1);
        for _ in 0..10_000 {
            let v = r.range(3, 9);
            assert!((3..=9).contains(&v));
        }
        assert_eq!(r.range(5, 5), 5);
        assert_eq!(r.range(9, 3), 9);
    }
}
