//! Frame of reference and bit-packing, in the two layouts compared (CS.3).
//!
//! Each value is stored as its distance from the column's least value (the frame of reference),
//! in as many bits as the largest distance needs. The layouts differ in where a value's bits go:
//!
//! - [`Layout::Horizontal`]: value `i` at bit `i * width` of one stream of words, as the scalar
//!   bit-packing BtrBlocks' cascade ends in. A group of 64 values fills exactly `width` words.
//! - [`Layout::Interleaved`]: FastLanes' layout [c89]: vectors of 1,024 values, each spread over
//!   16 lanes of 64 bits (one 1,024-bit register), value `i` of a vector in lane `i % 16` at row
//!   `i / 16`. Every lane does the same shift and mask at each row, so the decoder is scalar code
//!   the compiler turns into vector instructions [c90].
//!
//! Decoders are monomorphized per width (0 to 64), so every shift is a constant after unrolling.
//! The 64, 16 and 1,024 are format values: the word's bits, the lanes of a 1,024-bit register of
//! 64-bit words, and FastLanes' vector [c89].

use crate::CodecError;

/// Bits in a word: a format value.
const WORD_BITS: u32 = 64;
/// Values in a horizontal group: `width` words hold exactly 64 values of `width` bits.
pub const GROUP: usize = 64;
/// Lanes of FastLanes' 1,024-bit register over 64-bit words [c89].
pub const LANES: usize = 16;
/// Values in a FastLanes vector [c89].
pub const VECTOR: usize = 1024;

/// Where a packed value's bits go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Horizontal,
    Interleaved,
}

/// Values packed by frame of reference: `base` plus `width` bits each, padded to whole groups or
/// vectors with zeros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed {
    pub layout: Layout,
    pub width: u32,
    pub base: u64,
    pub len: usize,
    pub words: Vec<u64>,
}

/// The bits a distance of `span` needs.
pub fn width_of(span: u64) -> u32 {
    WORD_BITS.saturating_sub(span.leading_zeros())
}

/// The mask of a value `w` bits wide.
const fn mask(w: u32) -> u64 {
    if w == 0 {
        0
    } else {
        u64::MAX.wrapping_shr(WORD_BITS.wrapping_sub(w))
    }
}

/// Bits a whole number of `unit`-value blocks takes for `len` values `width` bits wide.
pub fn padded_words(layout: Layout, width: u32, len: usize) -> Result<usize, CodecError> {
    let unit = match layout {
        Layout::Horizontal => GROUP,
        Layout::Interleaved => VECTOR,
    };
    let blocks = len.div_ceil(unit);
    let w = usize::try_from(width).map_err(|_| CodecError::Overflow)?;
    // A block of `unit` values of `w` bits is `unit * w / 64` words: `w` for a group, `16 * w` for a vector.
    let per_block = match layout {
        Layout::Horizontal => Some(w),
        Layout::Interleaved => w.checked_mul(LANES),
    };
    per_block
        .and_then(|p| p.checked_mul(blocks))
        .ok_or(CodecError::Overflow)
}

/// Packs `values` (each already a distance from `base`) in `layout`.
pub fn pack(layout: Layout, base: u64, distances: &[u64]) -> Result<Packed, CodecError> {
    let span = distances.iter().fold(0u64, |a, &v| a | v);
    let width = width_of(span);
    let mut words = vec![0u64; padded_words(layout, width, distances.len())?];
    if width > 0 {
        let w = usize::try_from(width).map_err(|_| CodecError::Overflow)?;
        for (i, &v) in distances.iter().enumerate() {
            // Where value i's bits start: its lane's word stream, and the bit within it.
            let (first_word, stride, bit) = match layout {
                Layout::Horizontal => (0usize, 1usize, i.checked_mul(w)),
                Layout::Interleaved => {
                    let vector = i.wrapping_div(VECTOR);
                    let within = i.wrapping_rem(VECTOR);
                    let lane = within.wrapping_rem(LANES);
                    let row = within.wrapping_div(LANES);
                    let start = vector
                        .checked_mul(w.checked_mul(LANES).ok_or(CodecError::Overflow)?)
                        .and_then(|s| s.checked_add(lane));
                    (
                        start.ok_or(CodecError::Overflow)?,
                        LANES,
                        row.checked_mul(w),
                    )
                }
            };
            let bit = bit.ok_or(CodecError::Overflow)?;
            put(&mut words, first_word, stride, bit, width, v)?;
        }
    }
    Ok(Packed {
        layout,
        width,
        base,
        len: distances.len(),
        words,
    })
}

/// Writes `v`'s low `width` bits at bit `bit` of the word stream `first, first + stride, ...`.
fn put(
    words: &mut [u64],
    first: usize,
    stride: usize,
    bit: usize,
    width: u32,
    v: u64,
) -> Result<(), CodecError> {
    let word_at = |k: usize| {
        k.checked_mul(stride)
            .and_then(|o| o.checked_add(first))
            .ok_or(CodecError::Overflow)
    };
    let k = bit.wrapping_div(GROUP);
    let shift = u32::try_from(bit.wrapping_rem(GROUP)).map_err(|_| CodecError::Overflow)?;
    let lo = words.get_mut(word_at(k)?).ok_or(CodecError::Overflow)?;
    *lo |= v.wrapping_shl(shift);
    if shift.checked_add(width).ok_or(CodecError::Overflow)? > WORD_BITS {
        let next = k.checked_add(1).ok_or(CodecError::Overflow)?;
        let hi = words.get_mut(word_at(next)?).ok_or(CodecError::Overflow)?;
        *hi |= v.wrapping_shr(WORD_BITS.wrapping_sub(shift));
    }
    Ok(())
}

/// Decodes `packed` into `out` (replacing what it held), `len` values, each `base` plus its distance.
pub fn unpack(packed: &Packed, out: &mut Vec<u64>) -> Result<(), CodecError> {
    let need = padded_words(packed.layout, packed.width, packed.len)?;
    if packed.words.len() != need {
        return Err(CodecError::Corrupt(
            "packed words do not match their width and length",
        ));
    }
    out.clear();
    out.resize(need_values(packed)?, packed.base);
    macro_rules! by_width {
        ($kernel:ident; $($w:literal)*) => {
            match packed.width {
                0 => {}
                $($w => $kernel::<$w>(&packed.words, packed.base, out),)*
                _ => return Err(CodecError::Corrupt("bit width above 64")),
            }
        };
    }
    match packed.layout {
        Layout::Horizontal => {
            by_width!(unpack_horizontal; 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64)
        }
        Layout::Interleaved => {
            by_width!(unpack_interleaved; 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47 48 49 50 51 52 53 54 55 56 57 58 59 60 61 62 63 64)
        }
    }
    out.truncate(packed.len);
    Ok(())
}

fn need_values(packed: &Packed) -> Result<usize, CodecError> {
    let unit = match packed.layout {
        Layout::Horizontal => GROUP,
        Layout::Interleaved => VECTOR,
    };
    packed
        .len
        .div_ceil(unit)
        .checked_mul(unit)
        .ok_or(CodecError::Overflow)
}

/// Horizontal groups of 64 values in `W` words each, written over `out` (whole groups long).
/// Value `j`'s word and shift depend on `j` and `W` alone, so the loop unrolls to constants.
fn unpack_horizontal<const W: u32>(words: &[u64], base: u64, out: &mut [u64]) {
    let m = mask(W);
    let w = W as usize;
    for (group, dst) in words
        .chunks_exact(w)
        .zip(out.as_chunks_mut::<GROUP>().0.iter_mut())
    {
        for (j, o) in (0u32..).zip(dst.iter_mut()) {
            let bit = j.wrapping_mul(W);
            let k = bit.wrapping_shr(6) as usize; // bit / 64
            let shift = bit & 63; // bit mod 64, the word's bits
            let lo = group.get(k).copied().unwrap_or(0).wrapping_shr(shift);
            let v = if shift.wrapping_add(W) > WORD_BITS {
                lo | group
                    .get(k.wrapping_add(1))
                    .copied()
                    .unwrap_or(0)
                    .wrapping_shl(WORD_BITS.wrapping_sub(shift))
            } else {
                lo
            };
            *o = (v & m).wrapping_add(base);
        }
    }
}

/// Zero words, for a packed row past the end of damaged input (its length is checked first).
const ZERO_ROW: [u64; LANES] = [0; LANES];

/// FastLanes vectors, written over `out` (whole vectors long): 64 rows of 16 lanes from `W`
/// packed rows of 16 lanes. Row `r`'s packed row and shift depend on `r` and `W` alone, and each
/// row is the same shift, mask and add on all 16 lanes: vector instructions [c90].
fn unpack_interleaved<const W: u32>(words: &[u64], base: u64, out: &mut [u64]) {
    let m = mask(W);
    let per_vector = (W as usize).wrapping_mul(LANES);
    for (vector, dst) in words
        .chunks_exact(per_vector)
        .zip(out.as_chunks_mut::<VECTOR>().0.iter_mut())
    {
        let (rows, _) = vector.as_chunks::<LANES>();
        let (dst_rows, _) = dst.as_chunks_mut::<LANES>();
        for (r, row) in (0u32..).zip(dst_rows.iter_mut()) {
            let bit = r.wrapping_mul(W);
            let k = bit.wrapping_shr(6) as usize; // bit / 64
            let shift = bit & 63; // bit mod 64
            let lo = rows.get(k).unwrap_or(&ZERO_ROW);
            if shift.wrapping_add(W) <= WORD_BITS {
                for (o, &c) in row.iter_mut().zip(lo) {
                    *o = (c.wrapping_shr(shift) & m).wrapping_add(base);
                }
            } else {
                let hi = rows.get(k.wrapping_add(1)).unwrap_or(&ZERO_ROW);
                let back = WORD_BITS.wrapping_sub(shift);
                for ((o, &c), &n) in row.iter_mut().zip(lo).zip(hi) {
                    *o = ((c.wrapping_shr(shift) | n.wrapping_shl(back)) & m).wrapping_add(base);
                }
            }
        }
    }
}

/// FastLanes' DELTA on its layout: within a vector, lane `l` of row `r` holds the value at
/// `16 r + l` less the one 16 before it, so each lane runs its own prefix sum and all 16 run in
/// step. Row 0's values are the vector's bases, kept apart (its deltas are zero). FastLanes'
/// transposed order, which puts neighbouring values in one lane, is not built: deltas here span 16
/// values, at most 4 bits more a value on sorted data. Returns the bases and the deltas.
pub fn delta_encode(values: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let mut bases = Vec::with_capacity(values.len().div_ceil(VECTOR).saturating_mul(LANES));
    let mut deltas = Vec::with_capacity(values.len());
    for vector in values.chunks(VECTOR) {
        for (i, &v) in vector.iter().enumerate() {
            match i.checked_sub(LANES).and_then(|p| vector.get(p)) {
                Some(&prev) => deltas.push(v.wrapping_sub(prev)),
                None => {
                    bases.push(v);
                    deltas.push(0);
                }
            }
        }
    }
    (bases, deltas)
}

/// The bases [`delta_encode`] keeps for `len` values: up to 16 a vector.
pub fn delta_bases(len: usize) -> usize {
    let whole = len.wrapping_div(VECTOR).saturating_mul(LANES);
    whole.saturating_add(len.wrapping_rem(VECTOR).min(LANES))
}

/// Undoes [`delta_encode`] in place: a prefix sum down each lane of each vector, from its base.
pub fn delta_decode(values: &mut [u64], bases: &[u64]) -> Result<(), CodecError> {
    if bases.len() != delta_bases(values.len()) {
        return Err(CodecError::Corrupt("delta bases do not match the values"));
    }
    for (vector, base) in values.chunks_mut(VECTOR).zip(bases.chunks(LANES)) {
        let (rows, tail) = vector.as_chunks_mut::<LANES>();
        let mut acc = [0u64; LANES];
        for (a, &b) in acc.iter_mut().zip(base) {
            *a = b;
        }
        for row in rows.iter_mut() {
            for (a, x) in acc.iter_mut().zip(row.iter_mut()) {
                *a = a.wrapping_add(*x);
                *x = *a;
            }
        }
        for (a, x) in acc.iter_mut().zip(tail.iter_mut()) {
            *a = a.wrapping_add(*x);
            *x = *a;
        }
    }
    Ok(())
}
