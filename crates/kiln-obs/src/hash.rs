//! A keyed 64-bit hash for the store's tables (`docs/observe.md` §4.1).
//!
//! The mixing step is a 64×64→128-bit multiply folded to 64 bits (wyhash, rapidhash and foldhash
//! use it): one multiply a word. The keys come from the operating system's randomness at start
//! ([`std::hash::RandomState`]), so tag values from outside cannot be chosen to collide (hash
//! flooding); they never leave the process, and nothing durable or replicated depends on a hash.

use std::hash::{BuildHasher, Hasher, RandomState};

/// wyhash's primes (Wang Yi, wyhash final version 4).
const P0: u64 = 0xa076_1d64_78bd_642f;
const P1: u64 = 0xe703_7ed1_a0b4_28db;
const P2: u64 = 0x8ebc_6af0_9c88_c6e3;
const P3: u64 = 0x5899_65cc_7537_4cc3;

#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = u128::from(a) * u128::from(b);
    (r as u64) ^ ((r >> 64) as u64)
}

/// The process's keyed hash.
#[derive(Debug, Clone, Copy)]
pub struct Keyed {
    k0: u64,
    k1: u64,
}

impl Default for Keyed {
    fn default() -> Self {
        Self::new()
    }
}

impl Keyed {
    /// Keys from the operating system's randomness.
    pub fn new() -> Self {
        let s = RandomState::new();
        Self { k0: s.hash_one(P0), k1: s.hash_one(P1) }
    }

    /// Fixed keys, for tests that need a repeatable layout.
    pub fn with_keys(k0: u64, k1: u64) -> Self {
        Self { k0, k1 }
    }

    #[inline]
    pub fn bytes(&self, b: &[u8]) -> u64 {
        let (words, rest) = b.as_chunks::<8>();
        let mut h = self.k0 ^ mum(b.len() as u64 ^ P0, self.k1 ^ P1);
        for w in words {
            h = mum(h ^ u64::from_le_bytes(*w), self.k1 ^ P1);
        }
        let mut tail = [0u8; 8];
        for (d, s) in tail.iter_mut().zip(rest) {
            *d = *s;
        }
        h = mum(h ^ u64::from_le_bytes(tail), self.k0 ^ P2);
        mum(h ^ P3, self.k1 ^ P3)
    }

    #[inline]
    pub fn word(&self, x: u64) -> u64 {
        mum(mum(x ^ self.k0, P1 ^ self.k1), P2 ^ self.k0)
    }
}

/// [`Keyed`] as a [`BuildHasher`], for maps keyed by words (ids, pool keys, the like): a word
/// written is one [`Keyed::word`], bytes [`Keyed::bytes`] folded in. Each map its own keys, from the
/// operating system's randomness, as [`Keyed::new`]'s.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyedState(pub Keyed);

/// [`KeyedState`]'s hasher.
#[derive(Debug, Clone, Copy)]
pub struct KeyedHasher {
    keyed: Keyed,
    h: u64,
}

impl BuildHasher for KeyedState {
    type Hasher = KeyedHasher;

    #[inline]
    fn build_hasher(&self) -> KeyedHasher {
        KeyedHasher { keyed: self.0, h: 0 }
    }
}

impl Hasher for KeyedHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.h
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.h = self.keyed.word(self.h ^ self.keyed.bytes(bytes));
    }

    #[inline]
    fn write_u8(&mut self, x: u8) {
        self.write_u64(u64::from(x));
    }

    #[inline]
    fn write_u16(&mut self, x: u16) {
        self.write_u64(u64::from(x));
    }

    #[inline]
    fn write_u32(&mut self, x: u32) {
        self.write_u64(u64::from(x));
    }

    #[inline]
    fn write_u64(&mut self, x: u64) {
        self.h = self.keyed.word(self.h ^ x);
    }

    #[inline]
    fn write_usize(&mut self, x: usize) {
        self.write_u64(x as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    #[test]
    fn equal_input_equal_hash_and_keys_change_everything() {
        let a = Keyed::with_keys(1, 2);
        let b = Keyed::with_keys(3, 4);
        assert_eq!(a.bytes(b"host:web-1"), a.bytes(b"host:web-1"));
        assert_ne!(a.bytes(b"host:web-1"), b.bytes(b"host:web-1"));
        assert_ne!(a.bytes(b""), a.bytes(b"\0"), "length is mixed in");
        assert_ne!(a.bytes(b"ab"), a.bytes(b"ba"));
    }

    /// A map on it: a word, a pair of words, bytes, each the same hash every time and another for
    /// another key or another map's keys.
    #[test]
    fn as_a_map_s_hasher_equal_keys_hash_alike() {
        let (a, b) = (KeyedState(Keyed::with_keys(1, 2)), KeyedState(Keyed::with_keys(3, 4)));
        assert_eq!(a.hash_one(7u64), a.hash_one(7u64));
        assert_ne!(a.hash_one(7u64), a.hash_one(8u64));
        assert_ne!(a.hash_one(7u64), b.hash_one(7u64));
        assert_ne!(a.hash_one((1u64, 2u64)), a.hash_one((2u64, 1u64)));
        assert_eq!(a.hash_one("cpu.pprof"), a.hash_one("cpu.pprof"));
        let mut m: hashbrown::HashMap<u64, u64, KeyedState> = hashbrown::HashMap::with_hasher(KeyedState::default());
        for i in 0..10_000u64 {
            m.insert(i * 4096, i);
        }
        assert!((0..10_000u64).all(|i| m.get(&(i * 4096)) == Some(&i)));
    }

    #[test]
    fn low_bits_spread_evenly_over_similar_keys() {
        // 2^16 keys differing in one counter, into 256 buckets by the low byte: every bucket within
        // 6 standard deviations of 256 (a binomial's spread).
        let h = Keyed::with_keys(7, 9);
        let mut buckets = [0u32; 256];
        for i in 0..65_536u32 {
            let key = format!("system.cpu.user{{host:web-{i}}}");
            buckets[(h.bytes(key.as_bytes()) & 255) as usize] += 1;
        }
        let sd = (65_536.0f64 * (1.0 / 256.0) * (255.0 / 256.0)).sqrt();
        for &b in &buckets {
            assert!((f64::from(b) - 256.0).abs() < 6.0 * sd, "{b}");
        }
        let mut rng = Rng::new(1);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..100_000 {
            seen.insert(h.word(rng.next_u64()));
        }
        assert_eq!(seen.len(), 100_000);
    }
}
