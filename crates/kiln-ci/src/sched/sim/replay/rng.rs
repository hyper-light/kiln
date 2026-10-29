//! Randomness from a seed alone: SplitMix64 with named streams, drawn as kiln-sim draws it
//! (`crates/kiln-sim/src/rng.rs`), so a seed means the same in both.

/// SplitMix64's increment.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64's finalizer.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// FNV-1a: how a stream's name becomes a number.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3))
}

/// One named sequence of numbers from a seed: the same seed and name give the same draws, however
/// many other streams have drawn.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn stream(seed: u64, name: &str) -> Rng {
        Rng { state: mix(seed ^ fnv(name.as_bytes())) }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        mix(self.state)
    }

    /// Below `n` (0 for `n` of 0).
    pub fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        usize::try_from(self.next_u64().checked_rem(n).unwrap_or(0)).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W25 step 1: SplitMix64 as kiln-sim draws it: a stream is `mix(seed ^ fnv(name))`, and its
    /// first draw adds the increment first (seed 0's first draw is SplitMix64's known first output).
    #[test]
    fn w25_s1_rng_is_splitmix64() {
        let mut r = Rng { state: 0 };
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        let (mut a, mut b) = (Rng::stream(7, "x"), Rng::stream(7, "x"));
        assert_eq!(a.next_u64(), b.next_u64());
        assert_ne!(Rng::stream(7, "x").next_u64(), Rng::stream(7, "y").next_u64());
        assert_eq!(Rng::stream(1, "z").below(0), 0);
    }
}
