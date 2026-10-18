//! Bits, most significant first: what Gorilla's chunks are made of (`docs/observe.md` §4.3).
//!
//! The writer keeps up to 63 pending bits in a word and emits whole bytes; what it has written can
//! be read while writing goes on ([`BitWriter::view`]), so a query reads a series' open chunk
//! without copying it. The reader loads a word at a time.

/// Writes bits most significant first.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Pending bits, left-aligned: the first written is the word's most significant.
    acc: u64,
    /// How many of `acc`'s bits are pending, 0 to 63.
    used: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer whose first `bytes` bytes need no reallocation.
    pub fn with_capacity(bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes), acc: 0, used: 0 }
    }

    /// Writes the low `n` bits of `v`, most significant first; `n` above 64 writes 64.
    #[inline]
    pub fn write(&mut self, v: u64, n: u32) {
        if n == 0 {
            return;
        }
        let n = n.min(64);
        let v = if n == 64 { v } else { v & ((1u64 << n) - 1) };
        let free = 64 - self.used;
        if n < free {
            self.acc |= v << (free - n);
            self.used += n;
        } else if n == free {
            self.acc |= v;
            self.flush_word();
        } else {
            let rest = n - free;
            self.acc |= v >> rest;
            self.flush_word();
            self.acc = v << (64 - rest);
            self.used = rest;
        }
    }

    #[inline]
    pub fn write_bit(&mut self, bit: bool) {
        self.write(u64::from(bit), 1);
    }

    #[inline]
    fn flush_word(&mut self) {
        self.bytes.extend_from_slice(&self.acc.to_be_bytes());
        self.acc = 0;
        self.used = 0;
    }

    /// How many bits are written.
    pub fn bit_len(&self) -> u64 {
        (self.bytes.len() as u64).saturating_mul(8).saturating_add(u64::from(self.used))
    }

    /// Bytes the finished stream takes.
    pub fn byte_len(&self) -> usize {
        self.bytes.len().saturating_add(self.used.div_ceil(8) as usize)
    }

    /// Bytes allocated for it.
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
    }

    /// What is written so far, readable while writing goes on.
    pub fn view(&self) -> Bits<'_> {
        Bits { head: &self.bytes, tail: self.acc, tail_bits: self.used }
    }

    /// Forgets what was written, keeping the allocation.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.acc = 0;
        self.used = 0;
    }

    /// Appends the bytes (the last padded with zero bits) to `out`, the writer left as it was.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes);
        let tail = self.acc.to_be_bytes();
        out.extend(tail.iter().take(self.used.div_ceil(8) as usize));
    }

    /// The bytes, the last padded with zero bits, in an allocation sized to them.
    pub fn finish(mut self) -> Vec<u8> {
        let tail = self.acc.to_be_bytes();
        let n = self.used.div_ceil(8) as usize;
        self.bytes.extend(tail.iter().take(n));
        self.bytes.shrink_to_fit();
        self.bytes
    }
}

/// Bits to read: whole bytes, then up to 63 bits more (a writer's pending word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bits<'a> {
    head: &'a [u8],
    tail: u64,
    tail_bits: u32,
}

impl<'a> Bits<'a> {
    /// A finished stream: every bit of `bytes` (a padded last byte reads as zeros).
    pub fn bytes(bytes: &'a [u8]) -> Self {
        Self { head: bytes, tail: 0, tail_bits: 0 }
    }

    pub fn bit_len(&self) -> u64 {
        (self.head.len() as u64).saturating_mul(8).saturating_add(u64::from(self.tail_bits))
    }
}

/// Reading past the end of the bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the bits ended early")]
pub struct Truncated;

/// Reads bits most significant first.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    /// Bytes not loaded yet.
    head: &'a [u8],
    /// Bits after them not loaded yet, left-aligned.
    tail: u64,
    tail_bits: u32,
    /// Loaded bits, left-aligned.
    acc: u64,
    /// How many of `acc`'s bits are loaded.
    avail: u32,
}

impl<'a> BitReader<'a> {
    pub fn new(bits: Bits<'a>) -> Self {
        Self { head: bits.head, tail: bits.tail, tail_bits: bits.tail_bits, acc: 0, avail: 0 }
    }

    /// Loads bits until more than 56 are loaded or none are left.
    #[inline]
    fn refill(&mut self) {
        // While eight bytes last, one load: as many of them as the word has room for, in one shift
        // (no loop a byte; the byte after them stays unread).
        if let Some(word) = self.head.first_chunk::<8>() {
            let word = u64::from_be_bytes(*word);
            if self.avail == 0 {
                self.acc = word;
                self.avail = 64;
                self.head = self.head.get(8..).unwrap_or(&[]);
                return;
            }
            let bytes = (64 - self.avail) / 8;
            if bytes > 0 {
                let bits = bytes * 8;
                self.acc |= (word >> (64 - bits)) << (64 - self.avail - bits);
                self.avail += bits;
                self.head = self.head.get(bytes as usize..).unwrap_or(&[]);
            }
            return;
        }
        while self.avail <= 56 {
            if let Some((&b, rest)) = self.head.split_first() {
                self.acc |= u64::from(b) << (56 - self.avail);
                self.avail += 8;
                self.head = rest;
            } else if self.tail_bits > 0 {
                let take = self.tail_bits.min(64 - self.avail);
                let bits = self.tail >> (64 - take);
                self.acc |= bits << (64 - self.avail - take);
                self.avail += take;
                self.tail = if take >= 64 { 0 } else { self.tail << take };
                self.tail_bits -= take;
            } else {
                break;
            }
        }
    }

    /// Reads `n` bits, at most 64.
    #[inline]
    pub fn read(&mut self, n: u32) -> Result<u64, Truncated> {
        if n == 0 {
            return Ok(0);
        }
        let n = n.min(64);
        if self.avail < n {
            self.refill();
        }
        if self.avail >= n {
            let v = self.acc >> (64 - n);
            self.acc = if n == 64 { 0 } else { self.acc << n };
            self.avail -= n;
            return Ok(v);
        }
        // More than the word holds after a refill: what it holds, then the rest.
        let hi_n = self.avail;
        if hi_n == 0 {
            return Err(Truncated);
        }
        let hi = self.acc >> (64 - hi_n);
        self.acc = 0;
        self.avail = 0;
        self.refill();
        let lo_n = n - hi_n;
        if self.avail < lo_n {
            return Err(Truncated);
        }
        let lo = self.acc >> (64 - lo_n);
        self.acc <<= lo_n;
        self.avail -= lo_n;
        Ok((hi << lo_n) | lo)
    }

    #[inline]
    pub fn read_bit(&mut self) -> Result<bool, Truncated> {
        Ok(self.read(1)? == 1)
    }

    /// Bits left to read.
    pub fn remaining(&self) -> u64 {
        (self.head.len() as u64)
            .saturating_mul(8)
            .saturating_add(u64::from(self.tail_bits))
            .saturating_add(u64::from(self.avail))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::testkit::Rng;

    #[test]
    fn what_is_written_reads_back_in_every_width_and_alignment() {
        let mut rng = Rng::new(7);
        for round in 0..200 {
            let mut w = BitWriter::new();
            let mut wrote = Vec::new();
            for _ in 0..(round % 37 + 1) {
                let n = rng.below(65) as u32;
                let v = rng.next_u64();
                w.write(v, n);
                let masked = if n == 64 { v } else if n == 0 { 0 } else { v & ((1u64 << n) - 1) };
                wrote.push((masked, n));
            }
            // Read from the live view, then from the finished bytes: the same either way.
            let mut live = BitReader::new(w.view());
            for &(v, n) in &wrote {
                assert_eq!(live.read(n).unwrap(), v, "live, {n} bits");
            }
            let bits = w.bit_len();
            let bytes = w.finish();
            assert_eq!(bytes.len() as u64, bits.div_ceil(8));
            let mut done = BitReader::new(Bits::bytes(&bytes));
            for &(v, n) in &wrote {
                assert_eq!(done.read(n).unwrap(), v, "finished, {n} bits");
            }
        }
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_a_panic() {
        let mut w = BitWriter::new();
        w.write(0b101, 3);
        let mut r = BitReader::new(w.view());
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(1), Err(Truncated));
        let mut r = BitReader::new(Bits::bytes(&[0xAB]));
        assert_eq!(r.read(64), Err(Truncated));
        let mut r = BitReader::new(Bits::bytes(&[]));
        assert_eq!(r.read(1), Err(Truncated));
    }

    #[test]
    fn a_view_taken_mid_word_reads_only_what_was_written() {
        let mut w = BitWriter::new();
        w.write(u64::MAX, 64);
        w.write(0b11, 2);
        let v = w.view();
        assert_eq!(v.bit_len(), 66);
        let mut r = BitReader::new(v);
        assert_eq!(r.read(64).unwrap(), u64::MAX);
        assert_eq!(r.read(2).unwrap(), 0b11);
        assert_eq!(r.remaining(), 0);
        assert_eq!(r.read(1), Err(Truncated));
    }
}
