//! Tokens and Bloom filters: how a log search rules out blocks without reading their text
//! (`docs/observe.md` §4.6).
//!
//! A token is a maximal run of alphanumeric characters (Unicode's), lowercased: what a word search
//! matches. A block's filter holds every token of its messages and attribute values. A filter for
//! `n` tokens at false-positive rate `p` takes `m = −n ln p / (ln 2)²` bits and `k = (m/n) ln 2`
//! hashes (Bloom 1970; the optimum, e.g. Broder and Mitzenmacher, "Network applications of Bloom
//! filters: a survey", 2004), the `k` positions from two hashes as `h1 + i·h2` (Kirsch and
//! Mitzenmacher, "Less hashing, same performance", 2006).

/// Calls `f` with each token of `text`, lowercased into `buf`.
pub fn tokens(text: &str, buf: &mut String, mut f: impl FnMut(&str)) {
    buf.clear();
    for c in text.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                buf.push(l);
            }
        } else if !buf.is_empty() {
            f(buf);
            buf.clear();
        }
    }
    if !buf.is_empty() {
        f(buf);
        buf.clear();
    }
}

/// Two independent 64-bit hashes of a token. Filters are written to disk and read by every
/// replica and every build, so the hash is fixed by the format: the first 16 bytes of SHA-256
/// would be, but costs 300 ns a token; this is FNV-1a 64 (Fowler, Noll and Vo) for `h1` and the
/// same over the token's bytes reversed with another offset basis for `h2`, each finished with
/// SplitMix64's mixer (Steele et al. 2014) so low bits are as good as high.
#[inline]
pub fn token_hashes(token: &str) -> (u64, u64) {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x84222325_cbf29ce4;
    for &x in token.as_bytes() {
        a = (a ^ u64::from(x)).wrapping_mul(FNV_PRIME);
    }
    for &x in token.as_bytes().iter().rev() {
        b = (b ^ u64::from(x)).wrapping_mul(FNV_PRIME);
    }
    (mix(a), mix(b) | 1)
}

#[inline]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A Bloom filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom {
    pub bits: Vec<u64>,
    pub k: u32,
}

impl Bloom {
    /// A filter sized for `n` distinct tokens at false-positive rate `p`.
    pub fn with_rate(n: usize, p: f64) -> Self {
        let n = n.max(1) as f64;
        let ln2 = std::f64::consts::LN_2;
        let m = (-(n * p.clamp(1e-9, 0.5).ln()) / (ln2 * ln2)).ceil().max(64.0);
        let k = ((m / n) * ln2).round().clamp(1.0, 30.0) as u32;
        Self { bits: vec![0; (m as usize).div_ceil(64)], k }
    }

    fn m(&self) -> u64 {
        (self.bits.len() as u64) * 64
    }

    #[inline]
    pub fn insert(&mut self, h: (u64, u64)) {
        let m = self.m();
        let mut x = h.0;
        for _ in 0..self.k {
            let bit = x % m;
            if let Some(w) = self.bits.get_mut((bit / 64) as usize) {
                *w |= 1 << (bit % 64);
            }
            x = x.wrapping_add(h.1);
        }
    }

    #[inline]
    pub fn may_contain(&self, h: (u64, u64)) -> bool {
        let m = self.m();
        if m == 0 {
            return true;
        }
        let mut x = h.0;
        for _ in 0..self.k {
            let bit = x % m;
            if self.bits.get((bit / 64) as usize).is_none_or(|w| w & (1 << (bit % 64)) == 0) {
                return false;
            }
            x = x.wrapping_add(h.1);
        }
        true
    }

    /// Bytes on disk.
    pub fn byte_len(&self) -> usize {
        self.bits.len() * 8 + 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_on_anything_not_alphanumeric_and_lowercase() {
        let mut got = Vec::new();
        let mut buf = String::new();
        tokens("GET /api/v1/runs?id=42 -> 503 Service Unavailable (Ünïcode)", &mut buf, |t| got.push(t.to_owned()));
        assert_eq!(got, vec!["get", "api", "v1", "runs", "id", "42", "503", "service", "unavailable", "ünïcode"]);
    }

    #[test]
    fn no_false_negatives_and_the_false_positive_rate_it_was_sized_for() {
        let n = 20_000;
        for p in [0.01, 0.05] {
            let mut b = Bloom::with_rate(n, p);
            for i in 0..n {
                b.insert(token_hashes(&format!("token{i}")));
            }
            for i in 0..n {
                assert!(b.may_contain(token_hashes(&format!("token{i}"))));
            }
            let trials = 200_000;
            let fp = (0..trials).filter(|i| b.may_contain(token_hashes(&format!("other{i}")))).count();
            let rate = fp as f64 / trials as f64;
            // Within 25% of the rate asked (the binomial's spread at these counts is far smaller).
            assert!(rate < p * 1.25 && rate > p * 0.6, "p {p}: measured {rate}");
        }
    }
}
