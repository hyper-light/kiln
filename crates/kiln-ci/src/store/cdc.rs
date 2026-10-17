//! Content-defined chunking (docs/ci.md W08-R01): FastCDC with normalized chunking. Where an
//! object is cut depends only on the bytes around the cut. An insertion or deletion moves only
//! the chunks near it, so a second version of a large object stores just the chunks that
//! changed.
//!
//! [`Cdc`] decides cuts over a buffer the caller keeps. The caller appends bytes, asks for the
//! next cut, and splits off that many bytes as a chunk. The scan's position and hash are kept
//! between calls, so bytes arriving in small pieces are scanned once.

use std::sync::LazyLock;

/// No chunk is shorter, except an object's last.
pub const MIN: usize = 512 << 10;
/// The size chunks tend to.
pub const AVG: usize = 2 << 20;
/// No chunk is longer.
pub const MAX: usize = 8 << 20;

/// Bits that must be zero for a cut before [`AVG`] (harder) and after it (easier): 21 bits for a
/// 2 MiB average, ±2 for normalized chunking level 2.
const MASK_S: u64 = spread(23);
const MASK_L: u64 = spread(19);

/// `bits` one-bits spread over bits 16 to 63, where the gear hash has seen at least 17 bytes.
const fn spread(bits: u32) -> u64 {
    let mut m = 0u64;
    let mut i = 0u32;
    while i < bits {
        m |= 1u64 << (63 - (i * 48 / bits));
        i += 1;
    }
    m
}

/// The gear table: 256 pseudo-random words (SplitMix64 from a fixed seed). Changing it would move
/// every cut point and end deduplication against what is stored, so it never changes.
static GEAR: LazyLock<[u64; 256]> = LazyLock::new(|| {
    let mut t = [0u64; 256];
    let mut s: u64 = 0x6b69_6c6e_2d63_6463;
    for w in t.iter_mut() {
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        *w = z ^ (z >> 31);
    }
    t
});

/// The cut-point scanner for one stream.
#[derive(Debug, Clone, Default)]
pub struct Cdc {
    /// The next position to scan within the pending chunk (0 until [`MIN`] is reached).
    pos: usize,
    hash: u64,
}

impl Cdc {
    pub fn new() -> Cdc {
        Cdc::default()
    }

    /// The length of the chunk at the start of `pending` (the bytes since the last cut), once it
    /// is known. `None` asks for more bytes. Between cuts `pending` may only grow. At `eof` the
    /// rest is a chunk (the last). After a cut, `pending` starts after it.
    pub fn next(&mut self, pending: &[u8], eof: bool) -> Option<usize> {
        let len = pending.len();
        if len == 0 {
            return None;
        }
        if len <= MIN {
            return self.done(eof.then_some(len));
        }
        let gear: &[u64; 256] = &GEAR;
        let start = self.pos.max(MIN);
        let end = len.min(MAX);
        let mut h = self.hash;
        // Below the average size, the harder mask; above it, the easier one.
        for (from, to, mask) in [(start, end.min(AVG), MASK_S), (start.max(AVG), end, MASK_L)] {
            if let Some(window) = pending.get(from..to) {
                for (k, b) in window.iter().enumerate() {
                    h = (h << 1).wrapping_add(gear.get(usize::from(*b)).copied().unwrap_or(0));
                    if h & mask == 0 {
                        return self.done(Some(from + k));
                    }
                }
            }
        }
        if end == MAX {
            return self.done(Some(MAX));
        }
        if eof {
            return self.done(Some(len));
        }
        self.pos = end;
        self.hash = h;
        None
    }

    fn done(&mut self, cut: Option<usize>) -> Option<usize> {
        if cut.is_some() {
            *self = Cdc::default();
        }
        cut
    }
}

/// Chunk lengths of a whole object, in order.
pub fn cuts(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cdc = Cdc::new();
    let mut at = 0usize;
    while let Some(rest) = data.get(at..) {
        match cdc.next(rest, true) {
            Some(n) if n > 0 => {
                out.push(n);
                at += n;
            }
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    /// Deterministic pseudo-random bytes (xorshift64*).
    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            out.extend_from_slice(&s.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(n);
        out
    }

    fn digests(data: &[u8]) -> Vec<String> {
        let mut at = 0;
        cuts(data)
            .into_iter()
            .map(|n| {
                let d = data.get(at..at + n).map(|c| Sha256::digest(c).iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap_or_default();
                at += n;
                d
            })
            .collect()
    }

    #[test]
    fn w08_r01_chunks_cover_the_object_within_the_bounds() {
        let data = noise(40 << 20, 7);
        let c = cuts(&data);
        assert_eq!(c.iter().sum::<usize>(), data.len());
        let (last, rest) = c.split_last().unwrap_or((&0, &[]));
        assert!(rest.iter().all(|n| (MIN..=MAX).contains(n)), "{c:?}");
        assert!(*last <= MAX);
        // About 2 MiB on average for random bytes.
        let avg = data.len() / c.len();
        assert!((3 << 19..=3 << 20).contains(&avg), "average {avg}");
        // Small objects are one chunk; nothing is no chunks.
        assert_eq!(cuts(data.get(..MIN).unwrap_or_default()), vec![MIN]);
        assert_eq!(cuts(&[1, 2, 3]), vec![3]);
        assert!(cuts(&[]).is_empty());
        // The same bytes cut alike, on every host and every time.
        assert_eq!(cuts(&data), c);
    }

    #[test]
    fn w08_r01_streaming_cuts_like_the_whole() {
        let data = noise(24 << 20, 11);
        let whole = cuts(&data);
        // Bytes arriving in uneven pieces, a chunk split off at each cut.
        let mut got = Vec::new();
        let mut cdc = Cdc::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut at = 0;
        let mut piece = 1usize;
        while at < data.len() {
            let n = piece.min(data.len() - at);
            pending.extend_from_slice(data.get(at..at + n).unwrap_or_default());
            at += n;
            piece = (piece * 7 + 13) % (3 << 20) + 1;
            while let Some(cut) = cdc.next(&pending, false) {
                got.push(cut);
                pending.drain(..cut);
            }
        }
        while let Some(cut) = cdc.next(&pending, true) {
            got.push(cut);
            pending.drain(..cut);
        }
        assert_eq!(got, whole);
    }

    #[test]
    fn w08_r01_an_insertion_moves_only_nearby_chunks() {
        let data = noise(64 << 20, 3);
        let before = digests(&data);
        let mut edited = data.clone();
        edited.splice(10 << 20..10 << 20, b"an insertion of a few bytes".iter().copied());
        edited.drain(40 << 20..(40 << 20) + 1000);
        let after = digests(&edited);
        let shared = after.iter().filter(|d| before.contains(d)).count();
        // Two edits disturb at most a couple of chunks each.
        assert!(shared + 6 >= before.len(), "{shared} of {} chunks kept", before.len());
    }

    #[test]
    fn w08_r01_uniform_bytes_cut_at_the_maximum() {
        let zeros = vec![0u8; 20 << 20];
        let c = cuts(&zeros);
        // Every chunk starts alike, so each is cut alike (and they are stored once).
        let (_, rest) = c.split_last().unwrap_or((&0, &[]));
        assert!(rest.windows(2).all(|w| w.first() == w.last()), "{c:?}");
        assert_eq!(c.iter().sum::<usize>(), zeros.len());
    }
}
