//! CRC-32C (Castagnoli), the checksum on every block file (`docs/observe.md` §4.5): the polynomial
//! of iSCSI (RFC 3720 §B.4), ext4, LevelDB and RocksDB, chosen by Castagnoli, Bräuer and Herrmann
//! ("Optimization of cyclic redundancy-check codes with 24 and 32 parity bits", IEEE Trans. Comm.
//! 1993) for its Hamming distance at storage block sizes. Computed eight bytes a step with Intel's
//! slicing-by-8 tables (Kounavis and Berry, 2005), in safe code.

/// The polynomial, reflected.
const POLY: u32 = 0x82f6_3b78;

/// Evaluated at compile time only (it initialises a `static`): an index out of bounds here is a
/// compile error, never a panic at run time.
#[allow(clippy::indexing_slicing)]
const fn tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static TABLES: [[u32; 256]; 8] = tables();

#[inline]
fn at(table: usize, i: u32) -> u32 {
    TABLES.get(table).and_then(|t| t.get((i & 0xff) as usize)).copied().unwrap_or(0)
}

/// Continues a checksum over `data` (start with 0).
pub fn update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        let lo = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) ^ c;
        let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        c = at(7, lo) ^ at(6, lo >> 8) ^ at(5, lo >> 16) ^ at(4, lo >> 24) ^ at(3, hi) ^ at(2, hi >> 8) ^ at(1, hi >> 16) ^ at(0, hi >> 24);
    }
    for &b in rest {
        c = (c >> 8) ^ at(0, c ^ u32::from(b));
    }
    !c
}

pub fn checksum(data: &[u8]) -> u32 {
    update(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_check_values() {
        // RFC 3720 §B.4 and the "123456789" check value of the CRC catalogue (CRC-32/ISCSI).
        assert_eq!(checksum(b"123456789"), 0xe306_9283);
        assert_eq!(checksum(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(checksum(&[0xffu8; 32]), 0x62a8_ab43);
        let up: Vec<u8> = (0u8..32).collect();
        assert_eq!(checksum(&up), 0x46dd_794e);
        let down: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(checksum(&down), 0x113f_db5c);
        assert_eq!(checksum(b""), 0);
    }

    #[test]
    fn in_pieces_as_whole() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        let whole = checksum(&data);
        for cut in [0, 1, 7, 8, 9, 500, 999, 1000] {
            let (a, b) = data.split_at(cut);
            assert_eq!(update(update(0, a), b), whole);
        }
    }
}
