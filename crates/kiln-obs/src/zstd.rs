//! Zstandard decompression (RFC 8878), what Datadog's Agent compresses its payloads with
//! (`docs/observe.md` §8.6), and the OpenTelemetry Collector may: frames of raw, RLE and compressed
//! blocks, literals raw, RLE or Huffman-coded (one or four streams, the tree's weights direct or
//! FSE-coded), sequences FSE-coded (predefined, RLE, given or repeated tables), repeat offsets, the
//! content checksum (XXH64's low 32 bits) checked; skippable frames skipped. Dictionaries are not
//! taken (none of the senders uses one).
//!
//! Safe code, every malformed input an error and never a panic or a read past what was written;
//! what is decompressed is bounded by the caller (`max`), checked before it is written.

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ZstdError {
    #[error("not a Zstandard frame")]
    Magic,
    #[error("the frame ends inside what it says follows")]
    Truncated,
    #[error("the frame's content is not what Zstandard writes")]
    Corrupt,
    #[error("a frame that needs a dictionary")]
    Dictionary,
    #[error("the content is larger than taken")]
    TooLarge,
    #[error("the content's checksum does not match")]
    Checksum,
}

use ZstdError::{Corrupt, Truncated};

const MAGIC: u32 = 0xFD2F_B528;
const BLOCK_MAX: usize = 128 << 10;

/// A frame's (or frames') content into `out` (cleared first), `max` bytes at the most.
pub fn decompress(input: &[u8], out: &mut Vec<u8>, max: usize) -> Result<(), ZstdError> {
    out.clear();
    let mut at = input;
    if at.is_empty() {
        return Err(ZstdError::Magic);
    }
    while !at.is_empty() {
        let (m, _) = at.split_first_chunk::<4>().ok_or(Truncated)?;
        let magic = u32::from_le_bytes(*m);
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            // A skippable frame: its size, then that many bytes.
            let (s, rest) = at.get(4..).and_then(|r| r.split_first_chunk::<4>()).ok_or(Truncated)?;
            let n = usize::try_from(u32::from_le_bytes(*s)).map_err(|_| Corrupt)?;
            at = rest.get(n..).ok_or(Truncated)?;
            continue;
        }
        if magic != MAGIC {
            return Err(ZstdError::Magic);
        }
        at = frame(at.get(4..).unwrap_or_default(), out, max)?;
    }
    Ok(())
}

/// One frame (after its magic): its blocks' content appended to `out`; what follows it.
fn frame<'a>(input: &'a [u8], out: &mut Vec<u8>, max: usize) -> Result<&'a [u8], ZstdError> {
    let (&fhd, mut at) = input.split_first().ok_or(Truncated)?;
    let fcs_flag = fhd >> 6;
    let single = fhd & 0x20 != 0;
    if fhd & 0x08 != 0 {
        return Err(Corrupt);
    }
    let checksum = fhd & 0x04 != 0;
    let dict_bytes = match fhd & 3 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    if !single {
        // The window descriptor: how far back a match may reach (all of what this frame wrote is
        // kept, so any offset within it is taken).
        let (_, rest) = at.split_first().ok_or(Truncated)?;
        at = rest;
    }
    let (dict, rest) = at.split_at_checked(dict_bytes).ok_or(Truncated)?;
    at = rest;
    if dict.iter().any(|&b| b != 0) {
        return Err(ZstdError::Dictionary);
    }
    let fcs_bytes = match fcs_flag {
        0 if single => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let (fcs, rest) = at.split_at_checked(fcs_bytes).ok_or(Truncated)?;
    at = rest;
    let declared = (fcs_bytes > 0).then(|| {
        let v = fcs.iter().rev().fold(0u64, |a, &b| (a << 8) | u64::from(b));
        if fcs_bytes == 2 { v + 256 } else { v }
    });
    if declared.is_some_and(|d| d > max as u64) {
        return Err(ZstdError::TooLarge);
    }
    let start = out.len();
    let mut state = State::default();
    loop {
        let (h, rest) = at.split_first_chunk::<3>().ok_or(Truncated)?;
        at = rest;
        let header = u32::from(h[0]) | (u32::from(h[1]) << 8) | (u32::from(h[2]) << 16);
        let last = header & 1 != 0;
        let size = usize::try_from(header >> 3).map_err(|_| Corrupt)?;
        match (header >> 1) & 3 {
            0 => {
                let (raw, rest) = at.split_at_checked(size).ok_or(Truncated)?;
                at = rest;
                if size > BLOCK_MAX || out.len() + size > max {
                    return Err(if size > BLOCK_MAX { Corrupt } else { ZstdError::TooLarge });
                }
                out.extend_from_slice(raw);
            }
            1 => {
                let (&b, rest) = at.split_first().ok_or(Truncated)?;
                at = rest;
                if size > BLOCK_MAX || out.len() + size > max {
                    return Err(if size > BLOCK_MAX { Corrupt } else { ZstdError::TooLarge });
                }
                out.resize(out.len() + size, b);
            }
            2 => {
                let (block, rest) = at.split_at_checked(size).ok_or(Truncated)?;
                at = rest;
                if size > BLOCK_MAX {
                    return Err(Corrupt);
                }
                compressed(block, out, start, max, &mut state)?;
            }
            _ => return Err(Corrupt),
        }
        if last {
            break;
        }
    }
    if declared.is_some_and(|d| d != (out.len() - start) as u64) {
        return Err(Corrupt);
    }
    if checksum {
        let (c, rest) = at.split_first_chunk::<4>().ok_or(Truncated)?;
        at = rest;
        let want = u32::from_le_bytes(*c);
        if (xxh64(out.get(start..).unwrap_or_default(), 0) as u32) != want {
            return Err(ZstdError::Checksum);
        }
    }
    Ok(at)
}

/// What a frame's blocks carry from one to the next: the literals' Huffman table, the sequences'
/// tables, the repeat offsets.
struct State {
    huffman: Option<Huffman>,
    ll: Option<Fse>,
    of: Option<Fse>,
    ml: Option<Fse>,
    rep: [usize; 3],
    literals: Vec<u8>,
}

impl Default for State {
    fn default() -> Self {
        Self { huffman: None, ll: None, of: None, ml: None, rep: [1, 4, 8], literals: Vec::new() }
    }
}

// ------------------------------------------------------------------------------------- bits

/// `n` bits (≤ 57) of `data` from bit `lo` up (bit `i` is `data[i / 8] >> (i % 8)`), the bit at `lo`
/// the value's lowest; bits below 0 or past the end are zeros.
fn bits(data: &[u8], lo: i64, n: u32) -> u64 {
    if n == 0 || lo + i64::from(n) <= 0 {
        return 0;
    }
    let (below, lo) = if lo < 0 { (u32::try_from(-lo).unwrap_or(64), 0usize) } else { (0, usize::try_from(lo).unwrap_or(usize::MAX)) };
    let n = n.saturating_sub(below);
    let byte = lo / 8;
    let mut w = [0u8; 8];
    for (d, s) in w.iter_mut().zip(data.get(byte..).unwrap_or_default()) {
        *d = *s;
    }
    let v = (u64::from_le_bytes(w) >> (lo % 8)) & ((1u64 << n) - 1);
    v << below
}

/// A bitstream read backwards, as Zstandard's FSE and Huffman streams are: from its end, past its
/// last byte's marker bit, down to its start (and, overread, zeros: what tells a decoding it is
/// done).
struct Back<'a> {
    data: &'a [u8],
    /// Bits not yet read, from the stream's start; below zero once overread.
    left: i64,
}

impl<'a> Back<'a> {
    fn new(data: &'a [u8]) -> Result<Self, ZstdError> {
        let &last = data.last().ok_or(Corrupt)?;
        if last == 0 {
            return Err(Corrupt);
        }
        let marker = 7 - i64::from(last.leading_zeros());
        Ok(Self { data, left: (data.len() as i64 - 1) * 8 + marker })
    }

    fn read(&mut self, n: u32) -> u64 {
        self.left -= i64::from(n);
        bits(self.data, self.left, n)
    }

    /// The next `n` bits, not consumed.
    fn peek(&self, n: u32) -> u64 {
        bits(self.data, self.left - i64::from(n), n)
    }

    fn overread(&self) -> bool {
        self.left < 0
    }

    fn done(&self) -> bool {
        self.left == 0
    }
}

// -------------------------------------------------------------------------------------- FSE

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    symbol: u8,
    bits: u8,
    base: u16,
}

#[derive(Debug, Clone, Default)]
struct Fse {
    log: u32,
    table: Vec<Entry>,
}

/// A table from its symbols' normalised counts (`-1`: less than one) at `log`.
fn fse_table(counts: &[i16], log: u32) -> Result<Fse, ZstdError> {
    let size = 1usize << log;
    let mut table = vec![Entry::default(); size];
    let mut next = vec![0u32; counts.len()];
    let mut high = size - 1;
    for (s, &c) in counts.iter().enumerate() {
        if c == -1 {
            table.get_mut(high).ok_or(Corrupt)?.symbol = u8::try_from(s).map_err(|_| Corrupt)?;
            high = high.checked_sub(1).ok_or(Corrupt)?;
            *next.get_mut(s).ok_or(Corrupt)? = 1;
        }
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let mask = size - 1;
    let mut pos = 0usize;
    for (s, &c) in counts.iter().enumerate() {
        if c <= 0 {
            continue;
        }
        *next.get_mut(s).ok_or(Corrupt)? = u32::try_from(c).map_err(|_| Corrupt)?;
        for _ in 0..c {
            table.get_mut(pos).ok_or(Corrupt)?.symbol = u8::try_from(s).map_err(|_| Corrupt)?;
            loop {
                pos = (pos + step) & mask;
                if pos <= high {
                    break;
                }
            }
        }
    }
    if pos != 0 {
        return Err(Corrupt);
    }
    for e in &mut table {
        let n = next.get_mut(usize::from(e.symbol)).ok_or(Corrupt)?;
        let state = *n;
        *n += 1;
        let high_bit = 31 - state.leading_zeros();
        let nb = log.checked_sub(high_bit).ok_or(Corrupt)?;
        e.bits = u8::try_from(nb).map_err(|_| Corrupt)?;
        e.base = u16::try_from((state << nb).checked_sub(u32::try_from(size).map_err(|_| Corrupt)?).ok_or(Corrupt)?).map_err(|_| Corrupt)?;
    }
    Ok(Fse { log, table })
}

/// A table described as RFC 8878 §4.1.1 has it (its counts, read forward), `max_log` its accuracy
/// at the most and `max_symbol` its largest symbol; the bytes it took.
fn read_fse(input: &[u8], max_log: u32, max_symbol: usize) -> Result<(Fse, usize), ZstdError> {
    let mut pos: i64 = 0;
    let data = input;
    let total = data.len() as i64 * 8;
    let log = 5 + bits(data, 0, 4) as u32;
    pos += 4;
    if log > max_log {
        return Err(Corrupt);
    }
    let mut remaining: i64 = (1 << log) + 1;
    let mut threshold: i64 = 1 << log;
    let mut nb: u32 = log + 1;
    let mut counts: Vec<i16> = Vec::with_capacity(max_symbol + 1);
    while remaining > 1 {
        if counts.len() > max_symbol {
            return Err(Corrupt);
        }
        let max = 2 * threshold - 1 - remaining;
        let low = bits(data, pos, nb - 1) as i64;
        let value = if low < max {
            pos += i64::from(nb - 1);
            low
        } else {
            let v = bits(data, pos, nb) as i64;
            pos += i64::from(nb);
            if v >= threshold { v - max } else { v }
        };
        let count = value - 1;
        remaining -= count.abs();
        counts.push(i16::try_from(count).map_err(|_| Corrupt)?);
        if count == 0 {
            // Zeros after a zero: two bits a time, 3 meaning three more and another two bits.
            loop {
                let repeat = bits(data, pos, 2);
                pos += 2;
                counts.resize(counts.len() + repeat as usize, 0);
                if counts.len() > max_symbol + 1 {
                    return Err(Corrupt);
                }
                if repeat != 3 {
                    break;
                }
            }
        }
        while remaining < threshold && threshold > 1 {
            nb -= 1;
            threshold >>= 1;
        }
        if pos > total {
            return Err(Truncated);
        }
    }
    if remaining != 1 || counts.len() > max_symbol + 1 {
        return Err(Corrupt);
    }
    let used = usize::try_from((pos + 7) / 8).map_err(|_| Corrupt)?;
    Ok((fse_table(&counts, log)?, used))
}

impl Fse {
    fn entry(&self, state: usize) -> Entry {
        self.table.get(state).copied().unwrap_or_default()
    }

    /// A table of one symbol (RLE).
    fn rle(symbol: u8) -> Self {
        Self { log: 0, table: vec![Entry { symbol, bits: 0, base: 0 }] }
    }
}

// ---------------------------------------------------------------------------------- Huffman

#[derive(Debug, Clone, Copy, Default)]
struct HuffEntry {
    symbol: u8,
    bits: u8,
}

#[derive(Debug, Clone, Default)]
struct Huffman {
    log: u32,
    table: Vec<HuffEntry>,
}

/// A Huffman table from its description (RFC 8878 §4.2.1): its weights, direct or FSE-coded; the
/// bytes it took.
fn read_huffman(input: &[u8]) -> Result<(Huffman, usize), ZstdError> {
    let (&header, rest) = input.split_first().ok_or(Truncated)?;
    let mut weights: Vec<u8> = Vec::with_capacity(256);
    let used = if header < 128 {
        // FSE-coded weights: a table (its accuracy 6 at the most), then two states read
        // alternately from a backward stream until it is spent.
        let n = usize::from(header);
        let block = rest.get(..n).ok_or(Truncated)?;
        let (fse, took) = read_fse(block, 6, 255)?;
        let mut s = Back::new(block.get(took..).ok_or(Truncated)?)?;
        let mut s1 = s.read(fse.log) as usize;
        let mut s2 = s.read(fse.log) as usize;
        loop {
            let e = fse.entry(s1);
            weights.push(e.symbol);
            s1 = usize::from(e.base) + s.read(u32::from(e.bits)) as usize;
            if s.overread() {
                weights.push(fse.entry(s2).symbol);
                break;
            }
            let e = fse.entry(s2);
            weights.push(e.symbol);
            s2 = usize::from(e.base) + s.read(u32::from(e.bits)) as usize;
            if s.overread() {
                weights.push(fse.entry(s1).symbol);
                break;
            }
            if weights.len() > 255 {
                return Err(Corrupt);
            }
        }
        1 + n
    } else {
        let n = usize::from(header - 127);
        let packed = rest.get(..n.div_ceil(2)).ok_or(Truncated)?;
        for i in 0..n {
            let b = packed.get(i / 2).copied().ok_or(Truncated)?;
            weights.push(if i % 2 == 0 { b >> 4 } else { b & 15 });
        }
        1 + n.div_ceil(2)
    };
    if weights.len() > 255 || weights.iter().any(|&w| w > 11) {
        return Err(Corrupt);
    }
    // The last symbol's weight is what makes the weights' sum a power of two.
    let total: u32 = weights.iter().filter(|&&w| w > 0).map(|&w| 1u32 << (w - 1)).sum();
    if total == 0 {
        return Err(Corrupt);
    }
    let log = 32 - total.leading_zeros();
    let rest_weight = (1u32 << log) - total;
    if !rest_weight.is_power_of_two() || log > 11 {
        return Err(Corrupt);
    }
    weights.push(u8::try_from(rest_weight.trailing_zeros() + 1).map_err(|_| Corrupt)?);
    let mut rank = [0u32; 13];
    for &w in &weights {
        if let Some(r) = rank.get_mut(usize::from(w)) {
            *r += 1;
        }
    }
    let mut start = [0u32; 13];
    let mut next = 0u32;
    for w in 1..=log as usize {
        if let (Some(s), Some(&r)) = (start.get_mut(w), rank.get(w)) {
            *s = next;
            next += r << (w - 1);
        }
    }
    let mut table = vec![HuffEntry::default(); 1usize << log];
    for (symbol, &w) in weights.iter().enumerate() {
        if w == 0 {
            continue;
        }
        let len = 1u32 << (w - 1);
        let s = start.get_mut(usize::from(w)).ok_or(Corrupt)?;
        let bits = u8::try_from(log + 1 - u32::from(w)).map_err(|_| Corrupt)?;
        for e in table.get_mut(*s as usize..(*s + len) as usize).ok_or(Corrupt)? {
            *e = HuffEntry { symbol: u8::try_from(symbol).map_err(|_| Corrupt)?, bits };
        }
        *s += len;
    }
    Ok((Huffman { log, table }, used))
}

impl Huffman {
    /// `n` symbols of one stream into `out`; the stream spent exactly.
    fn stream(&self, data: &[u8], n: usize, out: &mut Vec<u8>) -> Result<(), ZstdError> {
        let mut s = Back::new(data)?;
        for _ in 0..n {
            let e = self.table.get(s.peek(self.log) as usize).copied().ok_or(Corrupt)?;
            if e.bits == 0 {
                return Err(Corrupt);
            }
            out.push(e.symbol);
            s.left -= i64::from(e.bits);
        }
        if s.done() { Ok(()) } else { Err(Corrupt) }
    }
}

// --------------------------------------------------------------------------------- literals

/// The literals section: its literals into `state.literals`; the bytes it took.
fn literals(block: &[u8], state: &mut State) -> Result<usize, ZstdError> {
    let &b0 = block.first().ok_or(Truncated)?;
    let kind = b0 & 3;
    let format = (b0 >> 2) & 3;
    let byte = |i: usize| block.get(i).copied().map(usize::from).ok_or(Truncated);
    state.literals.clear();
    match kind {
        0 | 1 => {
            let (size, header) = match format {
                0 | 2 => (usize::from(b0 >> 3), 1),
                1 => ((usize::from(b0) >> 4) + (byte(1)? << 4), 2),
                _ => ((usize::from(b0) >> 4) + (byte(1)? << 4) + (byte(2)? << 12), 3),
            };
            if size > BLOCK_MAX {
                return Err(Corrupt);
            }
            if kind == 0 {
                state.literals.extend_from_slice(block.get(header..header + size).ok_or(Truncated)?);
                Ok(header + size)
            } else {
                let b = block.get(header).copied().ok_or(Truncated)?;
                state.literals.resize(size, b);
                Ok(header + 1)
            }
        }
        _ => {
            let (streams, header, regen, comp) = match format {
                0 | 1 => {
                    let v = usize::from(b0) | (byte(1)? << 8) | (byte(2)? << 16);
                    (if format == 0 { 1 } else { 4 }, 3, (v >> 4) & 0x3FF, (v >> 14) & 0x3FF)
                }
                2 => {
                    let v = usize::from(b0) | (byte(1)? << 8) | (byte(2)? << 16) | (byte(3)? << 24);
                    (4, 4, (v >> 4) & 0x3FFF, (v >> 18) & 0x3FFF)
                }
                _ => {
                    let v = usize::from(b0) | (byte(1)? << 8) | (byte(2)? << 16) | (byte(3)? << 24) | (byte(4)? << 32);
                    (4, 5, (v >> 4) & 0x3FFFF, (v >> 22) & 0x3FFFF)
                }
            };
            if regen > BLOCK_MAX {
                return Err(Corrupt);
            }
            let mut data = block.get(header..header + comp).ok_or(Truncated)?;
            if kind == 2 {
                let (h, used) = read_huffman(data)?;
                state.huffman = Some(h);
                data = data.get(used..).ok_or(Truncated)?;
            }
            let h = state.huffman.take().ok_or(Corrupt)?;
            let result = (|| {
                if streams == 1 {
                    return h.stream(data, regen, &mut state.literals);
                }
                let (jump, rest) = data.split_first_chunk::<6>().ok_or(Truncated)?;
                let sizes = [usize::from(u16::from_le_bytes([jump[0], jump[1]])), usize::from(u16::from_le_bytes([jump[2], jump[3]])), usize::from(u16::from_le_bytes([jump[4], jump[5]]))];
                let segment = regen.div_ceil(4);
                let last = regen.checked_sub(3 * segment).ok_or(Corrupt)?;
                let mut at = rest;
                for (i, &n) in sizes.iter().enumerate() {
                    let (s, r) = at.split_at_checked(n).ok_or(Truncated)?;
                    at = r;
                    h.stream(s, if i < 3 { segment } else { last }, &mut state.literals)?;
                }
                h.stream(at, last, &mut state.literals)
            })();
            state.huffman = Some(h);
            result?;
            Ok(header + comp)
        }
    }
}

// -------------------------------------------------------------------------------- sequences

const LL_BASE: [u32; 36] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536];
const LL_BITS: [u8; 36] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027, 2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u8; 53] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
/// RFC 8878's predefined distributions (§3.1.1.3.2.2.1), at accuracy 6, 6 and 5.
const LL_DEFAULT: [i16; 36] = [4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1, -1, -1, -1, -1];
const ML_DEFAULT: [i16; 53] = [1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1];
const OF_DEFAULT: [i16; 29] = [1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1];

/// A sequence table by its mode: predefined, RLE, given, or the block before's.
fn table_for(mode: u8, data: &[u8], default: &[i16], default_log: u32, max_log: u32, max_symbol: usize, prev: &mut Option<Fse>) -> Result<usize, ZstdError> {
    match mode {
        0 => {
            *prev = Some(fse_table(default, default_log)?);
            Ok(0)
        }
        1 => {
            let &s = data.first().ok_or(Truncated)?;
            if usize::from(s) > max_symbol {
                return Err(Corrupt);
            }
            *prev = Some(Fse::rle(s));
            Ok(1)
        }
        2 => {
            let (t, used) = read_fse(data, max_log, max_symbol)?;
            *prev = Some(t);
            Ok(used)
        }
        _ => {
            if prev.is_none() {
                return Err(Corrupt);
            }
            Ok(0)
        }
    }
}

/// A compressed block: its literals and sequences executed into `out` (the frame's content from
/// `start`).
fn compressed(block: &[u8], out: &mut Vec<u8>, start: usize, max: usize, state: &mut State) -> Result<(), ZstdError> {
    let lit = literals(block, state)?;
    let mut at = block.get(lit..).ok_or(Truncated)?;
    let (&b0, rest) = at.split_first().ok_or(Truncated)?;
    at = rest;
    let n = match b0 {
        0 => 0usize,
        1..=127 => usize::from(b0),
        128..=254 => {
            let (&b1, rest) = at.split_first().ok_or(Truncated)?;
            at = rest;
            ((usize::from(b0) - 128) << 8) + usize::from(b1)
        }
        255 => {
            let (b, rest) = at.split_first_chunk::<2>().ok_or(Truncated)?;
            at = rest;
            usize::from(b[0]) + (usize::from(b[1]) << 8) + 0x7F00
        }
    };
    if n == 0 {
        if out.len() + state.literals.len() > max {
            return Err(ZstdError::TooLarge);
        }
        out.extend_from_slice(&state.literals);
        return Ok(());
    }
    let (&modes, rest) = at.split_first().ok_or(Truncated)?;
    at = rest;
    if modes & 3 != 0 {
        return Err(Corrupt);
    }
    let used = table_for(modes >> 6, at, &LL_DEFAULT, 6, 9, 35, &mut state.ll)?;
    at = at.get(used..).ok_or(Truncated)?;
    let used = table_for((modes >> 4) & 3, at, &OF_DEFAULT, 5, 8, 31, &mut state.of)?;
    at = at.get(used..).ok_or(Truncated)?;
    let used = table_for((modes >> 2) & 3, at, &ML_DEFAULT, 6, 9, 52, &mut state.ml)?;
    at = at.get(used..).ok_or(Truncated)?;
    let (Some(ll), Some(of), Some(ml)) = (state.ll.as_ref(), state.of.as_ref(), state.ml.as_ref()) else { return Err(Corrupt) };
    let mut s = Back::new(at)?;
    let mut ll_state = s.read(ll.log) as usize;
    let mut of_state = s.read(of.log) as usize;
    let mut ml_state = s.read(ml.log) as usize;
    let mut lit_at = 0usize;
    for i in 0..n {
        let (le, oe, me) = (ll.entry(ll_state), of.entry(of_state), ml.entry(ml_state));
        let of_code = u32::from(oe.symbol);
        if of_code > 31 {
            return Err(Corrupt);
        }
        let offset_value = (1u64 << of_code) + s.read(of_code);
        let ml_code = usize::from(me.symbol);
        let match_len = ML_BASE.get(ml_code).copied().ok_or(Corrupt)? as usize + s.read(u32::from(*ML_BITS.get(ml_code).ok_or(Corrupt)?)) as usize;
        let ll_code = usize::from(le.symbol);
        let lit_len = LL_BASE.get(ll_code).copied().ok_or(Corrupt)? as usize + s.read(u32::from(*LL_BITS.get(ll_code).ok_or(Corrupt)?)) as usize;
        // The offset: a new one past 3, else one of the three last (shifted when there are no
        // literals before it).
        let offset = if offset_value > 3 {
            let o = usize::try_from(offset_value - 3).map_err(|_| Corrupt)?;
            state.rep = [o, state.rep[0], state.rep[1]];
            o
        } else {
            let idx = offset_value as usize + usize::from(lit_len == 0);
            match idx {
                1 => state.rep[0],
                2 => {
                    let o = state.rep[1];
                    state.rep = [o, state.rep[0], state.rep[2]];
                    o
                }
                3 => {
                    let o = state.rep[2];
                    state.rep = [o, state.rep[0], state.rep[1]];
                    o
                }
                _ => {
                    let o = state.rep[0].checked_sub(1).ok_or(Corrupt)?;
                    state.rep = [o, state.rep[0], state.rep[1]];
                    o
                }
            }
        };
        // The literals, then the match.
        let lits = state.literals.get(lit_at..lit_at + lit_len).ok_or(Corrupt)?;
        lit_at += lit_len;
        if out.len() + lit_len + match_len > max {
            return Err(ZstdError::TooLarge);
        }
        out.extend_from_slice(lits);
        let have = out.len() - start;
        if offset == 0 || offset > have {
            return Err(Corrupt);
        }
        let from = out.len() - offset;
        let mut left = match_len;
        while left > 0 {
            let take = left.min(out.len() - from);
            out.extend_from_within(from..from + take);
            left -= take;
        }
        if i + 1 < n {
            ll_state = usize::from(le.base) + s.read(u32::from(le.bits)) as usize;
            ml_state = usize::from(me.base) + s.read(u32::from(me.bits)) as usize;
            of_state = usize::from(oe.base) + s.read(u32::from(oe.bits)) as usize;
        }
    }
    if !s.done() {
        return Err(Corrupt);
    }
    let tail = state.literals.get(lit_at..).ok_or(Corrupt)?;
    if out.len() + tail.len() > max {
        return Err(ZstdError::TooLarge);
    }
    out.extend_from_slice(tail);
    Ok(())
}

// ------------------------------------------------------------------------------------ XXH64

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

fn merge(acc: u64, v: u64) -> u64 {
    (acc ^ round(0, v)).wrapping_mul(P1).wrapping_add(P4)
}

/// XXH64 (Yann Collet's, as Zstandard's checksum uses it).
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let (chunks, mut tail) = data.as_chunks::<32>();
    let mut h = if chunks.is_empty() {
        seed.wrapping_add(P5)
    } else {
        let mut v = [seed.wrapping_add(P1).wrapping_add(P2), seed.wrapping_add(P2), seed, seed.wrapping_sub(P1)];
        for c in chunks {
            let (lanes, _) = c.as_chunks::<8>();
            for (acc, lane) in v.iter_mut().zip(lanes) {
                *acc = round(*acc, u64::from_le_bytes(*lane));
            }
        }
        let mut h = v[0].rotate_left(1).wrapping_add(v[1].rotate_left(7)).wrapping_add(v[2].rotate_left(12)).wrapping_add(v[3].rotate_left(18));
        for x in v {
            h = merge(h, x);
        }
        h
    };
    h = h.wrapping_add(data.len() as u64);
    while let Some((w, rest)) = tail.split_first_chunk::<8>() {
        h = (h ^ round(0, u64::from_le_bytes(*w))).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        tail = rest;
    }
    if let Some((w, rest)) = tail.split_first_chunk::<4>() {
        h = (h ^ u64::from(u32::from_le_bytes(*w)).wrapping_mul(P1)).rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        tail = rest;
    }
    for &b in tail {
        h = (h ^ u64::from(b).wrapping_mul(P5)).rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XXH64's published values (xxHash's sanity checks): the empty input, and a byte's.
    #[test]
    fn xxh64_gives_its_published_values() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        assert_eq!(xxh64(b"Nobody inspects the spammish repetition", 0), 0xFBCE_A83C_8A37_8BF1);
    }

    /// A raw block and an RLE block, written by hand as RFC 8878 lays them out.
    #[test]
    fn raw_and_rle_blocks_read_as_written() {
        let mut f = MAGIC.to_le_bytes().to_vec();
        // Single segment, content size 1 byte field, no checksum: FHD 0x20, FCS 8.
        f.extend_from_slice(&[0x20, 8]);
        // A raw block of 5 ("hello"), not last: type 0, size 5.
        let h = 5u32 << 3;
        f.extend_from_slice(&[h as u8, (h >> 8) as u8, (h >> 16) as u8]);
        f.extend_from_slice(b"hello");
        // An RLE block of 3 'x', last: type 1.
        let h = (3u32 << 3) | (1 << 1) | 1;
        f.extend_from_slice(&[h as u8, (h >> 8) as u8, (h >> 16) as u8, b'x']);
        let mut out = Vec::new();
        decompress(&f, &mut out, 1 << 20).unwrap();
        assert_eq!(out, b"helloxxx");
        assert_eq!(decompress(&f, &mut out, 4), Err(ZstdError::TooLarge), "its declared size past the bound");
        // A skippable frame before it is passed over.
        let mut s = 0x184D_2A53u32.to_le_bytes().to_vec();
        s.extend_from_slice(&3u32.to_le_bytes());
        s.extend_from_slice(b"abc");
        s.extend_from_slice(&f);
        decompress(&s, &mut out, 1 << 20).unwrap();
        assert_eq!(out, b"helloxxx");
        assert_eq!(decompress(b"nope", &mut out, 100), Err(ZstdError::Magic));
    }

    /// Frames the reference CLI (zstd 1.5.7) wrote, at levels from --fast=5 to 19, with and without
    /// checksums, one of two blocks: each reads back to what was compressed (its length and XXH64,
    /// from an implementation of its own).
    #[test]
    fn the_reference_cli_s_frames_read_back() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/zstd");
        let manifest = std::fs::read_to_string(dir.join("manifest.txt")).unwrap();
        let mut out = Vec::new();
        let mut n = 0;
        for line in manifest.lines().filter(|l| !l.is_empty()) {
            let mut parts = line.split_whitespace();
            let (file, len, hash) = (parts.next().unwrap(), parts.next().unwrap().parse::<usize>().unwrap(), u64::from_str_radix(parts.next().unwrap(), 16).unwrap());
            let frame = std::fs::read(dir.join(file)).unwrap();
            decompress(&frame, &mut out, 1 << 24).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!((out.len(), xxh64(&out, 0)), (len, hash), "{file}");
            // Cut anywhere, it is an error, never a panic.
            for cut in [1, frame.len() / 3, frame.len() / 2, frame.len().saturating_sub(1)] {
                if cut < frame.len() {
                    assert!(decompress(&frame[..cut], &mut out, 1 << 24).is_err(), "{file} cut at {cut}");
                }
            }
            n += 1;
        }
        assert!(n >= 7);
    }

    /// The reference CLI at every level on inputs of every shape (text, binary, runs, repeats far
    /// apart, sizes from nothing to a megabyte), read back to the byte. Needs `zstd` on the path:
    /// `cargo test -p kiln-obs --lib zstd -- --ignored`.
    #[test]
    #[ignore = "runs the zstd CLI"]
    fn every_level_of_the_reference_cli_reads_back() {
        use std::io::Write as _;
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut out = Vec::new();
        let mut checked = 0;
        for round in 0..60 {
            let size = [0usize, 1, 7, 100, 1000, 20_000, 131_072, 131_073, 300_000, 1_000_000][round % 10];
            let mut input = Vec::with_capacity(size);
            while input.len() < size {
                match next() % 5 {
                    0 => input.extend((0..next() % 64).map(|_| next() as u8)),
                    1 => input.extend(std::iter::repeat_n(next() as u8, (next() % 500) as usize)),
                    2 if !input.is_empty() => {
                        let a = (next() as usize) % input.len();
                        let n = ((next() % 2000) as usize).min(input.len() - a);
                        let copy = input[a..a + n].to_vec();
                        input.extend(copy);
                    }
                    _ => input.extend(format!("key{}=value{} ", next() % 50, next() % 1000).bytes()),
                }
            }
            input.truncate(size);
            for args in [&["-1"][..], &["-3", "-C"], &["-9"], &["-19"], &["--ultra", "-22"], &["--fast=7"], &["-19", "--long=27"]] {
                let mut child = std::process::Command::new("zstd").args(args).args(["-q", "-c", "-"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().expect("zstd on the path");
                let mut stdin = child.stdin.take().unwrap();
                let data = input.clone();
                let writer = std::thread::spawn(move || stdin.write_all(&data));
                let frame = child.wait_with_output().unwrap().stdout;
                writer.join().unwrap().unwrap();
                decompress(&frame, &mut out, 1 << 24).unwrap_or_else(|e| panic!("round {round} {args:?}: {e}"));
                assert!(out == input, "round {round} {args:?}: {} bytes back of {}", out.len(), input.len());
                checked += 1;
            }
        }
        eprintln!("{checked} frames read back");
    }

    /// Junk, and frames cut short or bit-flipped: an error or bytes, never a panic.
    #[test]
    fn whatever_comes_is_an_error_or_bytes() {
        let mut out = Vec::new();
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..50_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 96) as usize;
            let mut junk: Vec<u8> = MAGIC.to_le_bytes().to_vec();
            junk.extend((0..n).map(|i| (x >> (i % 8 * 8)) as u8 ^ (i as u8).wrapping_mul(97)));
            let _ = decompress(&junk, &mut out, 1 << 16);
        }
    }
}
