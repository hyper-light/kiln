//! Seeded fuzz inputs, made the same way here and in TypeScript (`ui/src/api/bp/fuzz.ts`): input
//! `i` of seed `s` comes from its own generator (`Rng::for_input`), so both languages make the same
//! bytes, and one input replays from its seed and index. Its outcome (each frame's re-encoding,
//! or its refusal) is folded into a digest the two sides compare.

use crate::frames::{Frame, Frames, VERSION, decode_body};
use crate::rng::Rng;
use crate::schema::fnv64;
use crate::wire::Writer;

/// How an input is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Made {
    /// Random bytes.
    Random,
    /// One to three frames drawn by type, encoded.
    Valid,
    /// Those, cut short.
    Truncated,
    /// Those, with random bytes after.
    Extended,
    /// Those, with one to three bits flipped.
    Flipped,
}

/// Input `i` of the run seeded `seed`.
pub fn input(seed: u64, i: u64) -> (Made, Vec<u8>) {
    let mut g = Rng::for_input(seed, i);
    let made = match g.below(5) {
        0 => Made::Random,
        1 => Made::Valid,
        2 => Made::Truncated,
        3 => Made::Extended,
        _ => Made::Flipped,
    };
    if made == Made::Random {
        let n = g.below(64);
        return (made, (0..n).map(|_| g.byte()).collect());
    }
    let count = g.below(3).saturating_add(1);
    let mut msg = Vec::new();
    for _ in 0..count {
        Frame::arb(&mut g).encode_into(VERSION, &mut msg);
    }
    match made {
        Made::Truncated => {
            let at = g.index(msg.len());
            msg.truncate(at);
        }
        Made::Extended => {
            let n = g.below(8).saturating_add(1);
            msg.extend((0..n).map(|_| g.byte()));
        }
        Made::Flipped => {
            let flips = g.below(3).saturating_add(1);
            for _ in 0..flips {
                let at = g.index(msg.len());
                let bit = g.below(8);
                if let Some(b) = msg.get_mut(at) {
                    *b ^= 1u8 << bit;
                }
            }
        }
        Made::Random | Made::Valid => {}
    }
    (made, msg)
}

/// What reading `msg` came to, as bytes both languages write the same: for each frame, `0`, its
/// kind, its newer trailing bytes and its re-encoding; or `1`, its kind plus one (0 when not read
/// that far) and why.
pub fn outcome(msg: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    let mut frames = Frames::new(msg);
    while let Some(raw) = frames.next_raw() {
        let res = raw.and_then(|(kind, body)| decode_body(kind, body));
        match res {
            Ok(r) => {
                w.byte(0);
                w.varint(u64::from(r.frame.kind().index()));
                w.len(r.extra);
                w.bytes(&r.frame.encode(VERSION));
            }
            Err(e) => {
                w.byte(1);
                w.varint(e.kind.map(|k| u64::from(k).saturating_add(1)).unwrap_or(0));
                w.varint(u64::from(e.why.index()));
            }
        }
    }
    w.buf
}

/// The digest of inputs `from..to` of a seed: FNV-1a over each input's outcome's own FNV.
pub fn digest(seed: u64, from: u64, to: u64) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for i in from..to {
        let (_, msg) = input(seed, i);
        let o = fnv64(&outcome(&msg));
        for b in o.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
        }
    }
    h
}
