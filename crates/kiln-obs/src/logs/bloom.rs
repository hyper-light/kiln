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

/// A trace id's two hashes in a block of spans' filter: from its bytes, not its hex as a token's
/// (`token_hashes` over hex correlates for ids of one family: Datadog's 64-bit ids padded with
/// zeros, X-Ray's that begin with their second, a generator's sequence; measured, one such id
/// passed 616 of 2,000 filters sized for 1%). Each 64-bit half mixed into the other's
/// (SplitMix64's finaliser on both: every input bit moves every output bit), so any 128 bits that
/// differ hash apart.
#[inline]
pub fn id_hashes(id: &[u8; 16]) -> (u64, u64) {
    let (lo, hi) = id.split_at(8);
    let lo = u64::from_le_bytes(<[u8; 8]>::try_from(lo).unwrap_or([0; 8]));
    let hi = u64::from_le_bytes(<[u8; 8]>::try_from(hi).unwrap_or([0; 8]));
    (mix(lo ^ mix(hi ^ 0x243f_6a88_85a3_08d3)), mix(hi ^ mix(lo ^ 0x1319_8a2e_0370_7344)) | 1)
}

/// A block of spans' salt: from what its summary holds (its first time and its rows), so a reader
/// salts as its writer did.
#[inline]
pub fn block_salt(min_ts: i64, rows: u32) -> u64 {
    mix((min_ts as u64) ^ (u64::from(rows) << 40) ^ 0xa409_3822_299f_31d0)
}

/// A token's (or an id's) hashes in one block of spans' filter: salted by the block, mixed again.
/// Filters of one size put a token at the same places in every one, and the tokens every block
/// holds (a service's name, `server`, `get`) set those places in every one: a query whose places
/// fall on them passed most filters (measured: a trace id read 654 of 2,000 blocks whose filters
/// were each 1% for a token of their own). Salted, a query's places differ block to block, its
/// false positives independent, each the 1% the filter is sized for.
#[inline]
pub fn salted(h: (u64, u64), salt: u64) -> (u64, u64) {
    (mix(h.0 ^ salt), mix(h.1 ^ salt.rotate_left(29)) | 1)
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

    /// Trace ids of families real tracers make (Datadog's 64-bit ids padded with zeros, X-Ray's
    /// that begin with their second, a sequence): each one's filter passes as few filters of other
    /// ids as a random token's would, the worst of a thousand among them too.
    #[test]
    fn trace_ids_of_one_family_pass_others_filters_at_the_rate_sized_for() {
        let families: [fn(u64) -> [u8; 16]; 3] = [
            |i| {
                let mut id = [0u8; 16];
                id[8..].copy_from_slice(&(i.wrapping_mul(0x9e37_79b9_7f4a_7c15)).to_be_bytes());
                id
            },
            |i| {
                let mut id = [0u8; 16];
                id[..4].copy_from_slice(&(1_760_000_000u32 + (i / 100) as u32).to_be_bytes());
                id[4..12].copy_from_slice(&(i % 100).wrapping_mul(0x2545_f491_4f6c_dd1d).to_be_bytes());
                id
            },
            |i| (u128::from(i + 1) * 0x9e37_79b9_7f4a_7c15).to_le_bytes(),
        ];
        for (fi, family) in families.iter().enumerate() {
            // 400 filters of 170 ids each (a block's traces), then 1,000 other ids against all.
            let filters: Vec<Bloom> = (0..400u64)
                .map(|f| {
                    let mut b = Bloom::with_rate(170, 0.01);
                    for j in 0..170 {
                        b.insert(id_hashes(&family(f * 1_000 + j)));
                    }
                    b
                })
                .collect();
            let mut worst = 0;
            let mut total = 0;
            for q in 0..1_000u64 {
                let h = id_hashes(&family(1_000_000 + q * 7));
                let yes = filters.iter().filter(|b| b.may_contain(h)).count();
                worst = worst.max(yes);
                total += yes;
            }
            let rate = total as f64 / (1_000.0 * 400.0);
            assert!(rate < 0.0125, "family {fi}: {rate}");
            // 4 expected of 400; a binomial's tail past 16 is under one in a billion.
            assert!(worst <= 16, "family {fi}: the worst id passed {worst} of 400");
        }
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
