//! SplitMix64, drawn exactly as kiln-sim's `rng::Rng` draws it (crates/kiln-sim/src/rng.rs), so a
//! seed means the same sequence here, in kiln-sim and in the TypeScript copy
//! (`ui/src/api/bp/runtime.ts`), which the differential fuzzing needs: both languages make the same
//! inputs from one seed. Kept here rather than taken from kiln-sim so the codec crate builds
//! with nothing under it; `it_is_kiln_sims_splitmix64` pins the two together.

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64's finalizer.
pub fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The generator for input `i` of a run seeded `seed`: each input made alone, so one replays
    /// from its seed and index.
    pub fn for_input(seed: u64, i: u64) -> Self {
        Self::new(mix(seed.wrapping_add(i)))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(GAMMA);
        mix(self.state)
    }

    /// Below `n` (0 for `n` of 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64().checked_rem(n).unwrap_or(0)
    }

    pub fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(u64::try_from(len).unwrap_or(u64::MAX))).unwrap_or(0)
    }

    pub fn byte(&mut self) -> u8 {
        (self.below(256) & 0xff) as u8
    }

    pub fn coin(&mut self) -> bool {
        self.below(2) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same first draw kiln-sim's test pins (crates/kiln-sim/src/rng.rs, `it_is_splitmix64`).
    #[test]
    fn it_is_kiln_sims_splitmix64() {
        assert_eq!(Rng::new(0).next_u64(), 0xE220_A839_7B1D_CDAF);
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            assert!(r.below(10) < 10);
        }
        assert_eq!(r.below(0), 0);
    }
}
