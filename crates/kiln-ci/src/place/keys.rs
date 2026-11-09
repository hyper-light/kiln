//! Keys on placement's decision path (docs/needs.md §11; AUDIT P6): names and run ids are looked
//! up once, as each word comes in, in a hash map whose hash is the same on every machine and every
//! run (no seed: kiln-ci takes no randomness of its own, docs/needs.md §9's "replay is exact"), and
//! everything past that is an integer handle into a slab.
//!
//! The hash is FxHash's (the Rust compiler's own: Firefox's, a rotate, xor and multiply a word at a
//! time). Its keys here are the owner's own run ids and the directory's runner names, not a
//! stranger's input, so a keyed hash's defence against chosen collisions buys nothing here.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// FxHash's multiplier (rustc-hash: 0x51_7c_c1_b7_27_22_0a_95, from the golden ratio's bits).
const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[derive(Debug, Clone, Copy, Default)]
pub struct Fx(u64);

impl Fx {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(SEED);
    }
}

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.add(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            if let Some(dst) = b.get_mut(..rest.len()) {
                dst.copy_from_slice(rest);
            }
            self.add(u64::from_le_bytes(b));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A map keyed by FxHash: the same on every machine.
pub type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// Names to handles, and back: a name keeps its handle for as long as its interner lives.
#[derive(Debug, Clone, Default)]
pub struct Interner {
    ids: FxMap<String, u32>,
    names: Vec<String>,
}

impl Interner {
    /// The handle of `name`, given one if it has none.
    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.ids.get(name) {
            return i;
        }
        let i = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.push(name.to_owned());
        self.ids.insert(name.to_owned(), i);
        i
    }

    /// The handle of `name`, if it has one.
    pub fn get(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
    }

    /// The name of handle `i`.
    pub fn name(&self, i: u32) -> &str {
        self.names.get(i as usize).map_or("", String::as_str)
    }

    /// How many names it holds.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014): the owner's draws, from the seed its shell
/// passes in.
#[derive(Debug, Clone, Copy, Default)]
pub struct SplitMix(pub u64);

impl SplitMix {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A draw below `n` (Lemire, "Fast random integer generation in an interval", TOMACS 2019:
    /// the high word of a 64 by 64 bit product; its bias, under n / 2^64, is left in).
    pub fn below(&mut self, n: u32) -> u32 {
        let x = u128::from(self.next_u64()) * u128::from(n);
        u32::try_from(x >> 64).unwrap_or(0)
    }

    /// A draw below `n` as [`Self::below`] draws, for a bound past a u32 (a learned weight).
    pub fn below64(&mut self, n: u64) -> u64 {
        let x = u128::from(self.next_u64()) * u128::from(n);
        u64::try_from(x >> 64).unwrap_or(0)
    }

    /// `d` distinct places in `0..n`, uniformly (Floyd's algorithm, Bentley and Floyd, "A sample of
    /// brilliance", CACM 1987): `d` draws, none wasted, nothing the size of `n` touched.
    pub fn sample(&mut self, n: u32, d: u32, out: &mut Vec<u32>) {
        out.clear();
        let d = d.min(n);
        for j in n.saturating_sub(d)..n {
            let t = self.below(j.saturating_add(1));
            out.push(if out.contains(&t) { j } else { t });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p6_an_interner_keeps_handles_and_a_sample_is_distinct_and_uniform() {
        let mut i = Interner::default();
        let (a, b) = (i.intern("lin-a"), i.intern("lin-b"));
        assert_eq!((i.intern("lin-a"), i.get("lin-b"), i.name(a), i.len()), (a, Some(b), "lin-a", 2));
        let mut r = SplitMix(7);
        let mut out = Vec::new();
        let mut hits = [0u32; 10];
        for _ in 0..20_000 {
            r.sample(10, 2, &mut out);
            assert_eq!(out.len(), 2);
            assert_ne!(out.first(), out.get(1));
            for x in &out {
                if let Some(h) = hits.get_mut(*x as usize) {
                    *h += 1;
                }
            }
        }
        // Each of 10 drawn 4,000 times in expectation; a binomial's sd is about 57.
        assert!(hits.iter().all(|h| (3_700..=4_300).contains(h)), "{hits:?}");
        r.sample(1, 2, &mut out);
        assert_eq!(out, vec![0]);
    }
}
