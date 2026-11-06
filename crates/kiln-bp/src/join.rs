//! Joining a lane's fragments back into the frame they cut (docs/browser-protocol.md,
//! "Fragmentation"): a lane's fragments come in its order, `last` ends one frame, and the bytes
//! joined are exactly one frame, never a fragment itself.

use std::collections::BTreeMap;

use crate::frames::{Fault, Fragment, Frame, Frames, Kind, MAX_FRAME, Read};
use crate::wire::Why;

/// Each lane's fragments so far.
#[derive(Debug, Default)]
pub struct Joiner {
    lanes: BTreeMap<u32, Vec<u8>>,
    /// Most bytes a frame joined may have.
    max: usize,
}

impl Joiner {
    pub fn new() -> Self {
        Self { lanes: BTreeMap::new(), max: MAX_FRAME }
    }

    pub fn with_max(max: usize) -> Self {
        Self { lanes: BTreeMap::new(), max }
    }

    /// Bytes held for lanes not yet whole.
    pub fn held(&self) -> usize {
        self.lanes.values().map(Vec::len).sum()
    }

    /// Takes a fragment: the frame it ends, when it's `last`. A lane past the bound is let go
    /// and refused (`too_large`); joined bytes that aren't exactly one frame, or are a fragment,
    /// are refused (`left_over`, `too_deep`, or the frame's own refusal).
    pub fn take(&mut self, f: Fragment) -> Option<Result<Read, Fault>> {
        let buf = self.lanes.entry(f.lane).or_default();
        if buf.len().saturating_add(f.data.0.len()) > self.max.saturating_add(16) {
            self.lanes.remove(&f.lane);
            return Some(Err(Fault { kind: Some(Kind::Fragment.index()), why: Why::TooLarge }));
        }
        buf.extend_from_slice(&f.data.0);
        if !f.last {
            return None;
        }
        let whole = self.lanes.remove(&f.lane).unwrap_or_default();
        Some(read_one(&whole, self.max))
    }
}

/// Reads bytes that must be one frame, not a fragment.
pub fn read_one(whole: &[u8], max: usize) -> Result<Read, Fault> {
    let mut it = Frames::with_max(whole, max);
    let Some(first) = it.next_raw() else {
        return Err(Fault { kind: Some(Kind::Fragment.index()), why: Why::CutShort });
    };
    let (kind, body) = first?;
    if it.next_raw().is_some() {
        return Err(Fault { kind: Some(kind), why: Why::LeftOver });
    }
    if kind == Kind::Fragment.index() || kind == Kind::Compressed.index() {
        return Err(Fault { kind: Some(kind), why: Why::TooDeep });
    }
    crate::frames::decode_body(kind, body)
}

/// Cuts an encoded frame into fragments of a lane, each frame at most `bound` bytes encoded.
pub fn cut(lane: u32, frame: &[u8], bound: usize) -> Vec<Frame> {
    // A fragment's own framing: kind 1, length up to 3, lane up to 5, last 1, data length up to 3.
    let room = bound.saturating_sub(13).max(1);
    let n = frame.len().div_ceil(room).max(1);
    frame
        .chunks(room)
        .enumerate()
        .map(|(i, c)| Frame::Fragment(Fragment { lane, last: i.saturating_add(1) == n, data: c.into() }))
        .collect()
}
