//! MD5 (RFC 1321), for sqllogictest's hashed answers: a query whose answer holds more values than the
//! file's hash threshold is compared by the MD5 of its values, each followed by a newline, as
//! sqllogictest.c hashes them (`md5_add`). Not for security: the format names the digest.

/// Each round's shift (RFC 1321, 3.4).
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The table T[i] = floor(2^32 * |sin(i + 1)|) (RFC 1321, 3.4).
const SINES: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

/// A digest being computed: feed it bytes, then take its hex.
pub struct Md5 {
    state: [u32; 4],
    pending: Vec<u8>,
    bytes: u64,
}

impl Default for Md5 {
    fn default() -> Self {
        Self {
            // RFC 1321, 3.3.
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            pending: Vec::with_capacity(64),
            bytes: 0,
        }
    }
}

impl Md5 {
    pub fn update(&mut self, data: &[u8]) {
        // The message's length is kept modulo 2^64 bits (RFC 1321, 3.2): wrapping is the format.
        self.bytes = self.bytes.wrapping_add(data.len() as u64);
        self.pending.extend_from_slice(data);
        let (blocks, rest) = self.pending.as_chunks::<64>();
        for block in blocks {
            compress(&mut self.state, block);
        }
        self.pending = rest.to_vec();
    }

    /// The digest as 32 lowercase hex digits.
    pub fn hex(mut self) -> String {
        let bits = self.bytes.wrapping_mul(8);
        // Padding (RFC 1321, 3.1): a one bit, zeros to 56 bytes modulo 64, then the length.
        let mut tail = vec![0x80u8];
        let used = self.pending.len().wrapping_add(1).wrapping_rem(64);
        let zeros = if used <= 56 {
            56usize.wrapping_sub(used)
        } else {
            120usize.wrapping_sub(used)
        };
        tail.resize(zeros.wrapping_add(1), 0);
        tail.extend_from_slice(&bits.to_le_bytes());
        let bytes = self.bytes;
        self.update(&tail);
        self.bytes = bytes;
        let mut out = String::with_capacity(32);
        for word in self.state {
            for b in word.to_le_bytes() {
                out.push_str(&format!("{b:02x}"));
            }
        }
        out
    }
}

/// One 64-byte block into the state (RFC 1321, 3.4).
fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut m = [0u32; 16];
    for (word, bytes) in m.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for (i, (shift, sine)) in SHIFTS.iter().zip(SINES.iter()).enumerate() {
        let (f, g) = match i {
            0..=15 => ((b & c) | (!b & d), i),
            16..=31 => (
                (d & b) | (!d & c),
                i.wrapping_mul(5).wrapping_add(1).wrapping_rem(16),
            ),
            32..=47 => (
                b ^ c ^ d,
                i.wrapping_mul(3).wrapping_add(5).wrapping_rem(16),
            ),
            _ => (c ^ (b | !d), i.wrapping_mul(7).wrapping_rem(16)),
        };
        // g < 16 by each round's formula.
        let word = m.get(g).copied().unwrap_or_default();
        let sum = a.wrapping_add(f).wrapping_add(*sine).wrapping_add(word);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(*shift));
    }
    let [s0, s1, s2, s3] = state;
    *s0 = s0.wrapping_add(a);
    *s1 = s1.wrapping_add(b);
    *s2 = s2.wrapping_add(c);
    *s3 = s3.wrapping_add(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md5(s: &[u8]) -> String {
        let mut h = Md5::default();
        h.update(s);
        h.hex()
    }

    /// Do: hash RFC 1321's test suite (appendix A.5), whole and fed in pieces. Expect: its digests.
    #[test]
    fn rfc_1321_test_suite() {
        let cases: [(&[u8], &str); 7] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(md5(input), want);
            let mut h = Md5::default();
            for piece in input.chunks(7) {
                h.update(piece);
            }
            assert_eq!(h.hex(), want, "fed in pieces");
        }
    }
}
