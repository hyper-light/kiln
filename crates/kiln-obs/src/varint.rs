//! LEB128 varints (DWARF 5 §7.6, protobuf's encoding): seven bits a byte, low first.

/// The most bytes a `u64` takes: ⌈64 / 7⌉.
pub const MAX_LEN: usize = 10;

#[inline]
pub fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Reads a varint from the front of `at`, advancing it; `None` when it ends early or overflows.
#[inline]
pub fn get(at: &mut &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    let mut shift = 0u32;
    for (i, &b) in at.iter().enumerate().take(MAX_LEN) {
        let low = u64::from(b & 0x7f);
        // The tenth byte may carry only the top bit.
        if shift == 63 && low > 1 {
            return None;
        }
        v |= low << shift;
        if b & 0x80 == 0 {
            *at = at.get(i + 1..)?;
            return Some(v);
        }
        shift += 7;
    }
    None
}

/// Zigzag (protobuf's `sint64`): small magnitudes of either sign in few bytes.
#[inline]
pub fn zig(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn zag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    #[test]
    fn varints_round_trip_and_bad_ones_are_refused() {
        let mut rng = Rng::new(5);
        let mut out = Vec::new();
        let mut want = vec![0, 1, 127, 128, 16_383, 16_384, u64::MAX, u64::MAX - 1];
        for _ in 0..1000 {
            want.push(rng.next_u64() >> rng.below(64));
        }
        for &v in &want {
            put(&mut out, v);
        }
        let mut at = out.as_slice();
        for &v in &want {
            assert_eq!(get(&mut at), Some(v));
        }
        assert!(at.is_empty());
        assert_eq!(get(&mut &[0x80u8, 0x80][..]), None, "ends early");
        assert_eq!(get(&mut &[0xff; 11][..]), None, "too long");
        assert_eq!(get(&mut &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02][..]), None, "overflows");
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -12345] {
            assert_eq!(zag(zig(v)), v);
        }
    }
}
