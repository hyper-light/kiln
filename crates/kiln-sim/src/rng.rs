//! Randomness: one seed, one sequence of numbers, whatever the platform or the crates' versions.
//! SplitMix64, drawn as the sends simulation (CP-T3.2) draws it, so a simulation moved onto
//! kiln-sim keeps what its seeds mean; and named streams, so a source that draws more (a model
//! that grows) leaves every other source's draws as they were.

/// SplitMix64's increment.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64's finalizer: a bijection of `u64` that scatters every bit.
pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// FNV-1a of `bytes`: how a stream's name becomes a number.
pub fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3))
}

/// A seeded sequence of numbers.
#[derive(Debug, Clone)]
pub struct Rng {
    /// The seed it started from: what its streams are made from.
    origin: u64,
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { origin: seed, state: seed }
    }

    /// The seed it started from.
    pub fn seed(&self) -> u64 {
        self.origin
    }

    /// Another sequence, named: the same seed and name give the same draws, however many this one
    /// has made, and different names give unrelated ones.
    pub fn stream(&self, name: &str) -> Self {
        Self::new(mix(self.origin ^ fnv(name.as_bytes())))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        mix(self.state)
    }

    /// Below `n` (0 for `n` of 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64().checked_rem(n).unwrap_or(0)
    }

    /// From `lo` to `hi`, both included (`lo` when `hi` is below it).
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo.saturating_add(self.below(hi.saturating_sub(lo).saturating_add(1)))
    }

    /// True `per_mille` times in a thousand.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }

    /// An index into something `len` long (0 when it's empty).
    pub fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(u64::try_from(len).unwrap_or(u64::MAX))).unwrap_or(0)
    }

    /// One of `items`, none when there are none.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            return None;
        }
        items.get(self.index(items.len()))
    }

    /// A span from `lo` to `hi`, both included, to the nanosecond.
    pub fn span(&mut self, lo: std::time::Duration, hi: std::time::Duration) -> std::time::Duration {
        let (lo_ns, hi_ns) = (u64::try_from(lo.as_nanos()).unwrap_or(u64::MAX), u64::try_from(hi.as_nanos()).unwrap_or(u64::MAX));
        std::time::Duration::from_nanos(self.range(lo_ns, hi_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T6.1, AC-T6.1.4: a seed is one sequence, the same every time.
    #[test]
    fn a_seed_draws_the_same_numbers_every_time() {
        let (mut a, mut b) = (Rng::new(42), Rng::new(42));
        let xs: Vec<u64> = (0..100).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..100).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
        assert_ne!(Some(Rng::new(43).next_u64()), xs.first().copied());
    }

    /// CP-T6.1, AC-T6.1.4: SplitMix64 as the sends simulation draws it (seed 0's first draw is
    /// SplitMix64's known first output), so its seeds keep their meaning on kiln-sim.
    #[test]
    fn it_is_splitmix64() {
        assert_eq!(Rng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
            let x = r.range(5, 9);
            assert!((5..=9).contains(&x));
        }
        assert_eq!(r.below(0), 0);
        assert_eq!(r.range(9, 5), 9);
    }

    /// CP-T6.1, AC-T6.1.4: a stream's draws don't move when another source draws more.
    #[test]
    fn a_stream_is_the_same_however_much_its_parent_drew() {
        let a = Rng::new(9);
        let mut b = Rng::new(9);
        for _ in 0..17 {
            b.next_u64();
        }
        assert_eq!(a.stream("net").next_u64(), b.stream("net").next_u64());
        assert_ne!(a.stream("net").next_u64(), a.stream("disk").next_u64());
    }
}
