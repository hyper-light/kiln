//! HPACK's Huffman coding (RFC 7541 §5.2): decoding four bits at a time through a state table built
//! once from the code, and encoding by the code directly.
//!
//! The code's tree has 257 leaves and so 256 inner nodes: a decoder state is an inner node (a `u8`),
//! and one step reads a nibble from a state: the state it reaches, and the symbol it completed, if
//! any (no code is shorter than five bits, so a nibble completes at most one). A string may end only
//! in a state reached from the root by fewer than eight 1-bits (the padding: EOS's leading bits).

use std::sync::OnceLock;

use super::codes::{CODES, LENGTHS};

const EMIT: u8 = 1;
/// The string may end after this step.
const ACCEPT: u8 = 2;
/// The bits decode EOS, which a string never holds.
const FAIL: u8 = 4;

#[derive(Clone, Copy, Default)]
struct Step {
    next: u8,
    sym: u8,
    flags: u8,
}

type Table = Vec<[Step; 16]>;

/// A node of the code's tree: an inner node's index, or a leaf's symbol.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Child {
    None,
    Inner(usize),
    Leaf(u16),
}

fn build() -> Table {
    // The tree: inner node 0 is the root.
    let mut nodes: Vec<[Child; 2]> = vec![[Child::None; 2]];
    for (sym, (&code, &len)) in CODES.iter().zip(LENGTHS.iter()).enumerate() {
        let mut node = 0usize;
        for bit in (0..u32::from(len)).rev() {
            let b = ((code >> bit) & 1) as usize;
            let last = bit == 0;
            let child = nodes.get(node).and_then(|n| n.get(b)).copied().unwrap_or(Child::None);
            node = match (child, last) {
                (_, true) => {
                    if let Some(slot) = nodes.get_mut(node).and_then(|n| n.get_mut(b)) {
                        *slot = Child::Leaf(sym as u16);
                    }
                    break;
                }
                (Child::Inner(i), false) => i,
                (_, false) => {
                    nodes.push([Child::None; 2]);
                    let i = nodes.len() - 1;
                    if let Some(slot) = nodes.get_mut(node).and_then(|n| n.get_mut(b)) {
                        *slot = Child::Inner(i);
                    }
                    i
                }
            };
        }
    }
    // Which inner nodes are a short run of 1-bits from the root (where a string may end).
    let mut accepting = vec![false; nodes.len()];
    let mut node = 0usize;
    for _ in 0..8 {
        if let Some(a) = accepting.get_mut(node) {
            *a = true;
        }
        match nodes.get(node).and_then(|n| n.get(1)).copied() {
            Some(Child::Inner(i)) => node = i,
            _ => break,
        }
    }
    let mut table: Table = vec![[Step::default(); 16]; nodes.len()];
    for (state, steps) in table.iter_mut().enumerate() {
        for (nibble, step) in steps.iter_mut().enumerate() {
            let mut node = state;
            let mut flags = 0;
            let mut sym = 0u8;
            for bit in (0..4).rev() {
                let b = (nibble >> bit) & 1;
                match nodes.get(node).and_then(|n| n.get(b)).copied() {
                    Some(Child::Inner(i)) => node = i,
                    Some(Child::Leaf(256)) | Some(Child::None) | None => {
                        flags |= FAIL;
                        break;
                    }
                    Some(Child::Leaf(s)) => {
                        flags |= EMIT;
                        sym = s as u8;
                        node = 0;
                    }
                }
            }
            if accepting.get(node).copied().unwrap_or(false) {
                flags |= ACCEPT;
            }
            *step = Step { next: node as u8, sym, flags };
        }
    }
    table
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(build)
}

/// Decodes `src` onto `out`; `Err` when it isn't a valid Huffman string.
pub(crate) fn decode(src: &[u8], out: &mut Vec<u8>) -> Result<(), ()> {
    let t = table();
    out.reserve(src.len() * 8 / 5);
    let mut state = 0usize;
    let mut accept = true;
    for &b in src {
        for nibble in [b >> 4, b & 0xf] {
            let step = t.get(state).and_then(|s| s.get(usize::from(nibble))).copied().ok_or(())?;
            if step.flags & FAIL != 0 {
                return Err(());
            }
            if step.flags & EMIT != 0 {
                out.push(step.sym);
            }
            state = usize::from(step.next);
            accept = step.flags & ACCEPT != 0;
        }
    }
    if accept { Ok(()) } else { Err(()) }
}

/// The length of `src` Huffman-coded.
#[cfg(test)]
pub(crate) fn encoded_len(src: &[u8]) -> usize {
    let bits: usize = src.iter().map(|&b| usize::from(LENGTHS.get(usize::from(b)).copied().unwrap_or(30))).sum();
    bits.div_ceil(8)
}

/// Encodes `src` onto `out`, padded with 1-bits.
#[cfg(test)]
pub(crate) fn encode(src: &[u8], out: &mut Vec<u8>) {
    let mut bits: u64 = 0;
    let mut n: u32 = 0;
    for &b in src {
        let (code, len) = (CODES.get(usize::from(b)).copied().unwrap_or(0), LENGTHS.get(usize::from(b)).copied().unwrap_or(0));
        bits = (bits << len) | u64::from(code);
        n += u32::from(len);
        while n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
        bits &= (1u64 << n) - 1;
    }
    if n > 0 {
        out.push(((bits << (8 - n)) as u8) | (0xffu8 >> n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huffman_strings_round_trip_and_match_the_rfc() -> Result<(), ()> {
        // RFC 7541 C.4.1: "www.example.com".
        let rfc = [0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff];
        let mut out = Vec::new();
        decode(&rfc, &mut out)?;
        assert_eq!(out, b"www.example.com");
        let mut enc = Vec::new();
        encode(b"www.example.com", &mut enc);
        assert_eq!(enc, rfc);
        assert_eq!(encoded_len(b"www.example.com"), rfc.len());
        let all: Vec<u8> = (0..=255u8).collect();
        let mut enc = Vec::new();
        encode(&all, &mut enc);
        let mut dec = Vec::new();
        decode(&enc, &mut dec)?;
        assert_eq!(dec, all);
        Ok(())
    }

    #[test]
    fn bad_padding_and_eos_are_refused() {
        let mut out = Vec::new();
        // Padding of 0-bits ('0' is five 0-bits; three more aren't EOS's), and of eight 1-bits.
        assert!(decode(&[0x00], &mut out).is_err());
        out.clear();
        let mut enc = Vec::new();
        encode(b"a", &mut enc);
        let mut bad = enc.clone();
        bad.push(0xff);
        assert!(decode(&bad, &mut out).is_err());
        // EOS itself (thirty 1-bits).
        assert!(decode(&[0xff, 0xff, 0xff, 0xff], &mut out).is_err());
    }
}
