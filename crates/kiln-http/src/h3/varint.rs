//! QUIC's variable-length integers (RFC 9000 §16), which every HTTP/3 frame, stream type, setting
//! and capsule is written in: whole ones from a slice, and one at a time from bytes that arrive in
//! pieces, holding at most the eight bytes of one.

/// The largest value a variable-length integer holds, 2^62 - 1.
pub const MAX: u64 = (1 << 62) - 1;

/// The length of the integer whose first byte is `first`.
#[inline]
pub(crate) fn len_of_first(first: u8) -> usize {
    1 << (first >> 6)
}

/// The bytes `v` takes.
#[inline]
pub(crate) fn len(v: u64) -> usize {
    match v {
        0..=0x3f => 1,
        0x40..=0x3fff => 2,
        0x4000..=0x3fff_ffff => 4,
        _ => 8,
    }
}

/// The integer at the start of `b` and its length; `None` when `b` ends first.
#[inline]
pub(crate) fn decode(b: &[u8]) -> Option<(u64, usize)> {
    let first = *b.first()?;
    let n = len_of_first(first);
    let rest = b.get(1..n)?;
    let mut v = u64::from(first & 0x3f);
    for &x in rest {
        v = (v << 8) | u64::from(x);
    }
    Some((v, n))
}

/// Writes `v` (at most [`MAX`]; larger is written as [`MAX`]).
#[inline]
pub(crate) fn put(out: &mut Vec<u8>, v: u64) {
    let v = v.min(MAX);
    match len(v) {
        1 => out.push(v as u8),
        2 => out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes()),
        4 => out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(v | 0xc000_0000_0000_0000).to_be_bytes()),
    }
}

/// One integer read a byte at a time: what a stream's next read may split.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Partial {
    value: u64,
    /// Bytes still to come; 0 before the first.
    left: u8,
}

impl Partial {
    /// Takes `b`; the integer once it's whole.
    #[inline]
    pub(crate) fn push(&mut self, b: u8) -> Option<u64> {
        if self.left == 0 {
            self.value = u64::from(b & 0x3f);
            self.left = (len_of_first(b) as u8).saturating_sub(1);
        } else {
            self.value = (self.value << 8) | u64::from(b);
            self.left = self.left.saturating_sub(1);
        }
        if self.left == 0 {
            let v = self.value;
            self.value = 0;
            Some(v)
        } else {
            None
        }
    }

    /// The bytes to read to finish the integer under way (1 before its first: the first byte says
    /// how many follow), so a reader never takes a byte past it.
    #[inline]
    pub(crate) fn left(&self) -> usize {
        if self.left == 0 { 1 } else { usize::from(self.left) }
    }

    /// Whether part of an integer has been taken.
    #[inline]
    pub(crate) fn started(&self) -> bool {
        self.left != 0
    }

    /// Takes from the front of `b` until an integer is whole: it and the bytes taken, or `None`
    /// and all of `b` taken.
    #[inline]
    pub(crate) fn take(&mut self, b: &[u8]) -> (Option<u64>, usize) {
        for (i, &x) in b.iter().enumerate() {
            if let Some(v) = self.push(x) {
                return (Some(v), i + 1);
            }
        }
        (None, b.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc9000_appendix_a1_samples_decode_and_round_trip() {
        // RFC 9000 §A.1's samples.
        let samples: [(&[u8], u64); 4] = [
            (&[0xc2, 0x19, 0x7c, 0x5e, 0xff, 0x14, 0xe8, 0x8c], 151_288_809_941_952_652),
            (&[0x9d, 0x7f, 0x3e, 0x7d], 494_878_333),
            (&[0x7b, 0xbd], 15_293),
            (&[0x25], 37),
        ];
        for (bytes, v) in samples {
            assert_eq!(decode(bytes), Some((v, bytes.len())));
            let mut out = Vec::new();
            put(&mut out, v);
            assert_eq!(out, bytes);
            let mut p = Partial::default();
            let mut got = None;
            for &b in bytes {
                got = p.push(b);
            }
            assert_eq!(got, Some(v));
            assert!(!p.started());
        }
        assert_eq!(decode(&[0x40]), None);
        assert_eq!(decode(&[]), None);
    }
}
