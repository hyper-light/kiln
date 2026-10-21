//! LZ4's block format (Yann Collet, "LZ4 Block Format Description"), in safe code: what recent log
//! blocks' text is compressed with (`docs/observe.md` §4.6). A log search decompresses every block
//! it cannot rule out, so decompression speed decides search speed: on kiln's logs this decodes at
//! 3.2–3.8 GB/s against deflate's 1.4–2.0 GB/s, at a ratio of 7.0 against deflate-6's 13.0 (§12.6);
//! blocks compaction rewrites, rarely searched, take deflate instead.
//!
//! A sequence: a token (literal length in the high nibble, match length − 4 in the low, 15 meaning
//! more bytes follow, each adding up to 255), the literals, a 2-byte little-endian offset back
//! into what is decoded, and the match length's extra bytes. The last sequence is literals only;
//! the last 5 bytes are always literals and a match starts at least 12 bytes before the end (the
//! format's end-of-block rules, which let fast decoders over-read; kept here for compatibility).

/// The shortest match the format encodes.
const MIN_MATCH: usize = 4;
/// The format's end-of-block rules.
const LAST_LITERALS: usize = 5;
const MF_LIMIT: usize = 12;
/// Hash table of 2^12 entries: the reference compressor's default (`LZ4_MEMORY_USAGE` 14, 16 KiB of
/// u32 positions).
const HASH_LOG: u32 = 12;
/// The farthest a match may reach back: the offset is 16 bits.
const MAX_OFFSET: usize = 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Lz4Error {
    #[error("the compressed block is malformed")]
    Malformed,
    #[error("the block decompresses past its stated size")]
    TooLarge,
}

#[inline]
fn hash(word: u32) -> usize {
    // Knuth's multiplicative hashing (TAOCP 6.4) with the golden-ratio constant, as the reference.
    (word.wrapping_mul(2_654_435_761) >> (32 - HASH_LOG)) as usize
}

#[inline]
fn read_u32(src: &[u8], at: usize) -> Option<u32> {
    src.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes)
}

#[inline]
fn read_u64(src: &[u8], at: usize) -> Option<u64> {
    src.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_le_bytes)
}

/// How many bytes on from `a` are those on from `b` (`a` before `b`), not reaching `end`: eight
/// compared at once (their first difference the lowest byte of the two words' that differs, read
/// little-endian), the rest one by one. A match's length, as a byte at a time finds it.
#[inline]
fn common(src: &[u8], a: usize, b: usize, end: usize) -> usize {
    let mut k = 0;
    while b + k + 8 <= end {
        let (Some(x), Some(y)) = (read_u64(src, a + k), read_u64(src, b + k)) else { break };
        let d = x ^ y;
        if d != 0 {
            return k + (d.trailing_zeros() / 8) as usize;
        }
        k += 8;
    }
    while b + k < end && src.get(a + k) == src.get(b + k) {
        k += 1;
    }
    k
}

fn push_length(out: &mut Vec<u8>, mut n: usize) {
    while n >= 255 {
        out.push(255);
        n -= 255;
    }
    out.push(n as u8);
}

/// Appends the compressed form of `src` to `out`.
pub fn compress(src: &[u8], out: &mut Vec<u8>) {
    let mut table = [0u32; 1 << HASH_LOG];
    let n = src.len();
    let mut anchor = 0usize;
    let mut i = 0usize;
    if n >= MF_LIMIT {
        let limit = n - MF_LIMIT;
        // Positions are stored + 1, so 0 means empty.
        while i <= limit {
            let Some(w) = read_u32(src, i) else { break };
            let h = hash(w);
            let cand = table.get(h).copied().unwrap_or(0) as usize;
            if let Some(slot) = table.get_mut(h) {
                *slot = (i + 1) as u32;
            }
            if cand == 0 || i - (cand - 1) > MAX_OFFSET || read_u32(src, cand - 1) != Some(w) {
                i += 1;
                continue;
            }
            let mut m = cand - 1;
            // Extend the match backwards over pending literals, then forwards.
            let mut start = i;
            while start > anchor && m > 0 && src.get(start - 1) == src.get(m - 1) {
                start -= 1;
                m -= 1;
            }
            let mut len = MIN_MATCH + (i - start);
            len += common(src, m + len, start + len, n - LAST_LITERALS);
            let lit = start - anchor;
            let token_pos = out.len();
            out.push(0);
            let lit_nibble = lit.min(15);
            if lit >= 15 {
                push_length(out, lit - 15);
            }
            out.extend_from_slice(src.get(anchor..start).unwrap_or(&[]));
            let offset = start - m;
            out.extend_from_slice(&(offset as u16).to_le_bytes());
            let ml = len - MIN_MATCH;
            let ml_nibble = ml.min(15);
            if ml >= 15 {
                push_length(out, ml - 15);
            }
            if let Some(t) = out.get_mut(token_pos) {
                *t = ((lit_nibble as u8) << 4) | ml_nibble as u8;
            }
            i = start + len;
            anchor = i;
            // Index a position inside the match, as the reference does, for the next search.
            if i >= 2
                && i - 2 <= limit
                && let Some(w2) = read_u32(src, i - 2)
                && let Some(slot) = table.get_mut(hash(w2))
            {
                *slot = (i - 2 + 1) as u32;
            }
        }
    }
    // The last literals.
    let lit = n - anchor;
    out.push(((lit.min(15)) as u8) << 4);
    if lit >= 15 {
        push_length(out, lit - 15);
    }
    out.extend_from_slice(src.get(anchor..).unwrap_or(&[]));
}

fn read_length(src: &[u8], at: &mut usize) -> Result<usize, Lz4Error> {
    let mut n = 0usize;
    loop {
        let b = *src.get(*at).ok_or(Lz4Error::Malformed)?;
        *at += 1;
        n = n.checked_add(b as usize).ok_or(Lz4Error::Malformed)?;
        if b != 255 {
            return Ok(n);
        }
    }
}

/// Appends the decompressed form of `src` to `out`, which may grow by at most `max` bytes.
pub fn decompress(src: &[u8], out: &mut Vec<u8>, max: usize) -> Result<(), Lz4Error> {
    let base = out.len();
    let mut at = 0usize;
    loop {
        let token = *src.get(at).ok_or(Lz4Error::Malformed)?;
        at += 1;
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            lit += read_length(src, &mut at)?;
        }
        let literals = src.get(at..at.checked_add(lit).ok_or(Lz4Error::Malformed)?).ok_or(Lz4Error::Malformed)?;
        if out.len() - base + lit > max {
            return Err(Lz4Error::TooLarge);
        }
        out.extend_from_slice(literals);
        at += lit;
        if at == src.len() {
            return Ok(());
        }
        let off: [u8; 2] = src.get(at..at + 2).and_then(|b| b.try_into().ok()).ok_or(Lz4Error::Malformed)?;
        let offset = u16::from_le_bytes(off) as usize;
        at += 2;
        let mut len = (token & 15) as usize;
        if len == 15 {
            len += read_length(src, &mut at)?;
        }
        len += MIN_MATCH;
        let decoded = out.len() - base;
        if offset == 0 || offset > decoded {
            return Err(Lz4Error::Malformed);
        }
        if decoded + len > max {
            return Err(Lz4Error::TooLarge);
        }
        let from = out.len() - offset;
        if offset >= len {
            out.extend_from_within(from..from + len);
        } else {
            // Overlapping: the match repeats what it copies (a run).
            for k in 0..len {
                let b = *out.get(from + k).ok_or(Lz4Error::Malformed)?;
                out.push(b);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    fn round_trip(data: &[u8]) -> usize {
        let mut c = Vec::new();
        compress(data, &mut c);
        let mut d = Vec::new();
        decompress(&c, &mut d, data.len()).unwrap();
        assert_eq!(d, data);
        c.len()
    }

    #[test]
    fn text_runs_and_noise_round_trip() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(b"hello hello hello hello hello hello");
        let log = "2026-10-18T05:00:00Z INFO kiln::agent job 01a14d0354c2 finished in 12.3s exit 0\n".repeat(500);
        let c = round_trip(log.as_bytes());
        assert!(c * 10 < log.len(), "{c} of {}", log.len());
        round_trip(&[0u8; 100_000]);
        let mut rng = Rng::new(1);
        for len in [1usize, 5, 12, 13, 64, 1000, 70_000, 200_000] {
            let noise: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            round_trip(&noise);
            let mixed: Vec<u8> = (0..len).map(|i| if (i / 37) % 3 == 0 { b"abcdefgh"[i % 8] } else { rng.below(4) as u8 }).collect();
            round_trip(&mixed);
        }
    }

    /// The compressor as first written, a match extended a byte at a time: what blocks on disk hold.
    fn compressed_by_byte(src: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut table = [0u32; 1 << HASH_LOG];
        let n = src.len();
        let (mut anchor, mut i) = (0usize, 0usize);
        if n >= MF_LIMIT {
            let limit = n - MF_LIMIT;
            while i <= limit {
                let w = read_u32(src, i).unwrap();
                let h = hash(w);
                let cand = table[h] as usize;
                table[h] = (i + 1) as u32;
                if cand == 0 || i - (cand - 1) > MAX_OFFSET || read_u32(src, cand - 1) != Some(w) {
                    i += 1;
                    continue;
                }
                let mut m = cand - 1;
                let mut start = i;
                while start > anchor && m > 0 && src[start - 1] == src[m - 1] {
                    start -= 1;
                    m -= 1;
                }
                let mut len = MIN_MATCH + (i - start);
                while start + len < n - LAST_LITERALS && src[start + len] == src[m + len] {
                    len += 1;
                }
                let lit = start - anchor;
                let token_pos = out.len();
                out.push(0);
                if lit >= 15 {
                    push_length(&mut out, lit - 15);
                }
                out.extend_from_slice(&src[anchor..start]);
                out.extend_from_slice(&((start - m) as u16).to_le_bytes());
                let ml = len - MIN_MATCH;
                if ml >= 15 {
                    push_length(&mut out, ml - 15);
                }
                out[token_pos] = ((lit.min(15) as u8) << 4) | ml.min(15) as u8;
                i = start + len;
                anchor = i;
                if i >= 2 && i - 2 <= limit {
                    table[hash(read_u32(src, i - 2).unwrap())] = (i - 2 + 1) as u32;
                }
            }
        }
        let lit = n - anchor;
        out.push((lit.min(15) as u8) << 4);
        if lit >= 15 {
            push_length(&mut out, lit - 15);
        }
        out.extend_from_slice(&src[anchor..]);
        out
    }

    /// Text, runs (long matches, short ones, matches running to the end), noise and mixtures of
    /// them, of every length to 64 and lengths past: compressed as a byte at a time compressed
    /// them, to the byte.
    #[test]
    fn matches_extended_a_word_at_a_time_compress_as_a_byte_at_a_time_did() {
        let mut rng = Rng::new(3);
        let log = "2026-10-20T19:01:35Z INFO kiln::observe: order 4242 paid in 37 ms by a card ending 4242 route=/api/v1/orders/17\n".repeat(300);
        let mut cases: Vec<Vec<u8>> = vec![log.into_bytes(), vec![0u8; 100_000], b"ab".repeat(5_000)];
        for len in (0..64).chain([65, 100, 1000, 4096, 70_000, 200_000]) {
            cases.push((0..len).map(|_| rng.next_u64() as u8).collect());
            cases.push((0..len).map(|i| if (i / 37) % 3 == 0 { b"abcdefgh"[i % 8] } else { rng.below(4) as u8 }).collect());
            let period = 1 + rng.below(40) as usize;
            cases.push((0..len).map(|i| if rng.below(50) == 0 { rng.next_u64() as u8 } else { (i % period) as u8 }).collect());
        }
        for (i, c) in cases.iter().enumerate() {
            let mut out = Vec::new();
            compress(c, &mut out);
            assert_eq!(out, compressed_by_byte(c), "case {i}: {} bytes", c.len());
        }
    }

    #[test]
    fn a_reference_block_decodes() {
        // From the format description: literals "abc" then a match at offset 3 of length 9 —
        // "abcabcabcabc" — then the last literals "abcde".
        let block = [0x35, b'a', b'b', b'c', 3, 0, 0x50, b'a', b'b', b'c', b'd', b'e'];
        let mut out = Vec::new();
        decompress(&block, &mut out, 100).unwrap();
        assert_eq!(out, b"abcabcabcabcabcde");
    }

    #[test]
    fn hostile_blocks_are_errors_and_never_grow_past_the_bound() {
        let mut rng = Rng::new(2);
        let text = "the quick brown fox jumps over the lazy dog ".repeat(200);
        let mut good = Vec::new();
        compress(text.as_bytes(), &mut good);
        for _ in 0..20_000 {
            let mut bad = good.clone();
            for _ in 0..1 + rng.below(3) {
                let i = rng.below(bad.len() as u64) as usize;
                bad[i] = rng.next_u64() as u8;
            }
            let mut out = Vec::new();
            let _ = decompress(&bad, &mut out, text.len());
            assert!(out.len() <= text.len());
            let junk: Vec<u8> = (0..rng.below(64)).map(|_| rng.next_u64() as u8).collect();
            let mut out = Vec::new();
            let _ = decompress(&junk, &mut out, 4096);
            assert!(out.len() <= 4096);
        }
        // A well-formed match longer than the bound is refused before it is copied; a length run
        // the block ends inside is malformed.
        let mut out = Vec::new();
        assert_eq!(decompress(&[0x1f, b'a', 1, 0, 255, 255, 255, 0, 0x00], &mut out, 100), Err(Lz4Error::TooLarge));
        assert!(out.len() <= 100);
        let mut out = Vec::new();
        assert_eq!(decompress(&[0x1f, b'a', 1, 0, 255, 255, 255, 255], &mut out, 1000), Err(Lz4Error::Malformed));
    }
}
