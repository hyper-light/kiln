//! QPACK (RFC 9204) with no dynamic table, and a decoded-size budget counted field by field
//! (goal 4 step 2, plan 01a1a832eb597110a6dcfda227981604).
//!
//! The door advertises `SETTINGS_QPACK_MAX_TABLE_CAPACITY` 0, so a peer's encoder may insert
//! nothing (RFC 9204 §3.2.3): every field section has Required Insert Count 0, and a nonzero one,
//! any dynamic or post-base reference, or a table capacity above 0 is a connection error. That
//! removes the dynamic-table "header accordion" (one inserted entry referenced by 1-byte lines,
//! 4064:1; FoxIO, 2026-10-20) outright. What's left, many literal fields and repeated references to
//! large static entries, is what the budget bounds, and it avoids the five mistakes FoxIO lists:
//!
//! 1. it counts the *decoded* size, not the encoded frame's;
//! 2. it checks as each field decodes, before the field is materialised, and stops at the first
//!    one past the budget (nothing of the section is kept: the door answers 431 or resets);
//! 3. each field counts its name, its value and the 32-byte overhead (RFC 9204 §3.2.1, RFC 9114
//!    §4.2.2);
//! 4. there is no "unlimited": [`Budget`] has no default, and the door's comes from one constant;
//! 5. the limit enforced is the one the door advertises (`SETTINGS_MAX_FIELD_SECTION_SIZE`).
//!
//! A Huffman string is decoded at most as far as the budget left allows ([`huffman::decode_bounded`]),
//! so the decoder's own allocation is at most the budget (its one scratch buffer) plus a field count
//! cap. It holds no clock and does no I/O. Encoding uses static references and literals only, so
//! the door opens no encoder or decoder stream of its own (RFC 9204 §4.2 allows that at capacity 0).

pub(crate) mod table;

use crate::h2::hpack::huffman::{self, Bounded};

/// QPACK's error codes (RFC 9204 §6).
pub mod code {
    /// The decoder failed to interpret a field section (a connection error).
    pub const DECOMPRESSION_FAILED: u64 = 0x0200;
    /// The decoder failed to interpret an instruction on the encoder stream.
    pub const ENCODER_STREAM_ERROR: u64 = 0x0201;
    /// The encoder failed to interpret an instruction on the decoder stream.
    pub const DECODER_STREAM_ERROR: u64 = 0x0202;
}

/// The most a field section may decode to: its size (each field's name, value and 32 bytes,
/// summed) and its field count. No default: whoever decodes says how much.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// `SETTINGS_MAX_FIELD_SECTION_SIZE`: the most the fields' sizes may sum to.
    pub bytes: usize,
    /// The most fields a section may hold.
    pub fields: usize,
}

/// Each field's overhead in its size (RFC 9204 §3.2.1).
pub const FIELD_OVERHEAD: usize = 32;

/// Why a field section was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused<E> {
    /// It's not a field section a conformant encoder writes to a table of capacity 0: a
    /// connection error, QPACK_DECOMPRESSION_FAILED.
    Failed,
    /// It decodes past the budget (bytes or fields): refused at the first field past it, with
    /// nothing of it kept (431, or a stream reset).
    TooLarge,
    /// The caller stopped it at a field.
    Caller(E),
}

/// What a decoded section came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    /// Its fields.
    pub fields: usize,
    /// Its size: names, values and 32 bytes a field.
    pub size: usize,
}

/// Reads an integer with an `n`-bit prefix (RFC 9204 §4.1.1, RFC 7541 §5.1) at `*pos`. Values
/// past 2^62 are refused, as are encodings longer than any such value needs.
fn read_int(b: &[u8], pos: &mut usize, n: u32) -> Result<u64, ()> {
    let mask = (1u64 << n) - 1;
    let first = u64::from(*b.get(*pos).ok_or(())?);
    *pos += 1;
    let mut value = first & mask;
    if value < mask {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let x = u64::from(*b.get(*pos).ok_or(())?);
        *pos += 1;
        if shift > 56 {
            return Err(());
        }
        value = value.checked_add((x & 0x7f) << shift).ok_or(())?;
        shift += 7;
        if x & 0x80 == 0 {
            return if value <= super::varint::MAX { Ok(value) } else { Err(()) };
        }
    }
}

/// Writes `value` with an `n`-bit prefix whose other bits are `flags`.
pub(crate) fn put_int(out: &mut Vec<u8>, flags: u8, n: u32, mut value: u64) {
    let mask = (1u64 << n) - 1;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// A string's place in the section: raw bytes, or Huffman-coded ones.
#[derive(Clone, Copy)]
struct Str {
    start: usize,
    len: usize,
    huffman: bool,
}

/// Reads a string's header (the H bit at `hbit` of its first byte, the length in an `n`-bit prefix)
/// and steps past it, checking it's all there.
fn read_str(b: &[u8], pos: &mut usize, hbit: u8, n: u32) -> Result<Str, ()> {
    let huffman = b.get(*pos).ok_or(())? & hbit != 0;
    let len = usize::try_from(read_int(b, pos, n)?).map_err(|_| ())?;
    let start = *pos;
    let end = start.checked_add(len).filter(|e| *e <= b.len()).ok_or(())?;
    *pos = end;
    Ok(Str { start, len, huffman })
}

/// Materialises `s` onto `scratch`, at most `room` bytes of it: `Err(TooLarge)` past that, without
/// decoding the rest. Returns its decoded length.
fn materialise<E>(b: &[u8], s: Str, scratch: &mut Vec<u8>, room: usize) -> Result<usize, Refused<E>> {
    let raw = b.get(s.start..s.start + s.len).ok_or(Refused::Failed)?;
    if !s.huffman {
        if raw.len() > room {
            return Err(Refused::TooLarge);
        }
        scratch.extend_from_slice(raw);
        return Ok(raw.len());
    }
    let before = scratch.len();
    match huffman::decode_bounded(raw, scratch, room) {
        Ok(()) => Ok(scratch.len() - before),
        Err(Bounded::Over) => Err(Refused::TooLarge),
        Err(Bounded::Invalid) => Err(Refused::Failed),
    }
}

/// A field's name: in the static table, or a string of the section.
enum NameRef {
    Static(&'static [u8]),
    Literal(Str),
}

/// Decodes one field section (a HEADERS frame's payload), handing each field to `each` as it's
/// decoded, within `budget`.
///
/// Before each field is materialised its size so far (the name, the value, 32) is added to the
/// running total and checked against the budget; a Huffman string is decoded only as far as the
/// budget left allows. The first field past the budget, or past the field count, ends the decode
/// with [`Refused::TooLarge`]: fields already handed over were the caller's to keep or drop (the
/// door keeps none of a refused section). `scratch` holds one field at a time (it's cleared per
/// field, and never grows past the budget), so a caller may reuse it across sections.
pub fn decode<E>(section: &[u8], budget: Budget, scratch: &mut Vec<u8>, mut each: impl FnMut(&[u8], &[u8]) -> Result<(), E>) -> Result<Decoded, Refused<E>> {
    let b = section;
    let mut pos = 0usize;
    // The prefix (RFC 9204 §4.5.1): Required Insert Count, which must be 0 at capacity 0 (any other
    // encoded value can't come from a conformant encoder: MaxEntries is 0), then the Base's sign and
    // delta, which nothing here refers to.
    let ric = read_int(b, &mut pos, 8).map_err(|_| Refused::Failed)?;
    if ric != 0 {
        return Err(Refused::Failed);
    }
    read_int(b, &mut pos, 7).map_err(|_| Refused::Failed)?;
    let mut done = Decoded::default();
    while let Some(&first) = b.get(pos) {
        if done.fields >= budget.fields {
            return Err(Refused::TooLarge);
        }
        let (name, value): (NameRef, Str) = if first & 0x80 != 0 {
            // Indexed field line (§4.5.2): T is the static bit.
            if first & 0x40 == 0 {
                return Err(Refused::Failed);
            }
            let i = read_int(b, &mut pos, 6).map_err(|_| Refused::Failed)?;
            let (n, v) = usize::try_from(i).ok().and_then(|i| table::STATIC.get(i)).copied().ok_or(Refused::Failed)?;
            scratch.clear();
            let size = n.len() + v.len() + FIELD_OVERHEAD;
            if done.size + size > budget.bytes {
                return Err(Refused::TooLarge);
            }
            done.size += size;
            done.fields += 1;
            each(n, v).map_err(Refused::Caller)?;
            continue;
        } else if first & 0xc0 == 0x40 {
            // Literal field line with name reference (§4.5.4): N, T, a 4-bit index.
            if first & 0x10 == 0 {
                return Err(Refused::Failed);
            }
            let i = read_int(b, &mut pos, 4).map_err(|_| Refused::Failed)?;
            let (n, _) = usize::try_from(i).ok().and_then(|i| table::STATIC.get(i)).copied().ok_or(Refused::Failed)?;
            let v = read_str(b, &mut pos, 0x80, 7).map_err(|_| Refused::Failed)?;
            (NameRef::Static(n), v)
        } else if first & 0xe0 == 0x20 {
            // Literal field line with literal name (§4.5.6): N, H, a 3-bit length.
            let n = read_str(b, &mut pos, 0x08, 3).map_err(|_| Refused::Failed)?;
            let v = read_str(b, &mut pos, 0x80, 7).map_err(|_| Refused::Failed)?;
            (NameRef::Literal(n), v)
        } else {
            // Indexed field line with post-base index (0001xxxx) or literal with post-base name
            // reference (0000xxxx): both refer to the dynamic table.
            return Err(Refused::Failed);
        };
        scratch.clear();
        // The room the budget leaves this field's name and value.
        let room = budget.bytes.checked_sub(done.size + FIELD_OVERHEAD).ok_or(Refused::TooLarge)?;
        let name_len = match name {
            NameRef::Static(n) => {
                if n.len() > room {
                    return Err(Refused::TooLarge);
                }
                n.len()
            }
            NameRef::Literal(s) => materialise(b, s, scratch, room)?,
        };
        let value_len = materialise(b, value, scratch, room - name_len)?;
        done.size += name_len + value_len + FIELD_OVERHEAD;
        done.fields += 1;
        match name {
            NameRef::Static(n) => each(n, scratch.get(..value_len).unwrap_or_default()).map_err(Refused::Caller)?,
            NameRef::Literal(_) => {
                let (n, v) = scratch.split_at_checked(name_len).ok_or(Refused::Failed)?;
                each(n, v).map_err(Refused::Caller)?;
            }
        }
    }
    Ok(done)
}

/// The peer's encoder stream, read as it arrives (RFC 9204 §4.3): at table capacity 0 the only
/// instruction it may carry is Set Dynamic Table Capacity 0, the one byte 0x20. Anything else
/// (a larger capacity, an insert, a duplicate) is QPACK_ENCODER_STREAM_ERROR. Holds nothing.
pub fn encoder_stream(bytes: &[u8]) -> Result<(), u64> {
    if bytes.iter().all(|&b| b == 0x20) { Ok(()) } else { Err(code::ENCODER_STREAM_ERROR) }
}

/// The peer's decoder stream, read as it arrives (RFC 9204 §4.4). The door's encoder never refers
/// to a dynamic table, so a Section Acknowledgment or an Insert Count Increment can't be right:
/// QPACK_DECODER_STREAM_ERROR. A Stream Cancellation is parsed and dropped; its integer may span
/// reads, which is all this holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecoderStream {
    /// Continuation bytes of a Stream Cancellation's stream id are being read, `shift` bits in.
    shift: Option<u32>,
}

impl DecoderStream {
    /// Takes the next bytes of the stream.
    pub fn read(&mut self, bytes: &[u8]) -> Result<(), u64> {
        for &b in bytes {
            match self.shift {
                Some(shift) => {
                    // A stream id is a 62-bit value: a longer encoding can't be one.
                    if shift > 56 {
                        return Err(code::DECODER_STREAM_ERROR);
                    }
                    self.shift = if b & 0x80 != 0 { Some(shift + 7) } else { None };
                }
                None => {
                    if b & 0xc0 != 0x40 {
                        // 1xxxxxxx Section Acknowledgment, 00xxxxxx Insert Count Increment.
                        return Err(code::DECODER_STREAM_ERROR);
                    }
                    if b & 0x3f == 0x3f {
                        self.shift = Some(0);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Writes field sections with static references and literals only (no table, no Huffman): what
/// the door answers a CONNECT with.
pub(crate) struct Encoder;

impl Encoder {
    /// Starts a section: Required Insert Count 0, Base 0.
    pub(crate) fn begin(out: &mut Vec<u8>) {
        out.extend_from_slice(&[0x00, 0x00]);
    }

    /// Writes one field: an indexed line when the static table holds it whole, a literal with a
    /// static name reference when it holds the name, else a literal with a literal name.
    pub(crate) fn field(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
        if let Some(i) = table::index(name, value) {
            put_int(out, 0xc0, 6, i as u64);
            return;
        }
        match table::name_index(name) {
            Some(i) => put_int(out, 0x50, 4, i as u64),
            None => {
                put_int(out, 0x20, 3, name.len() as u64);
                out.extend_from_slice(name);
            }
        }
        put_int(out, 0x00, 7, value.len() as u64);
        out.extend_from_slice(value);
    }
}

#[cfg(test)]
mod tests;
