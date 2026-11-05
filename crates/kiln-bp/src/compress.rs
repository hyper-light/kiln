//! Compression by trust class (docs/browser-protocol.md, "Compression"; plan step 8). Raw
//! deflate with a sync flush after each batch, so one long-lived `DecompressionStream
//! ("deflate-raw")` in the browser yields each batch as it comes. A context lives as long as the
//! connection, one per trust class (and per lane, on a carrier whose lanes are streams that keep
//! no one order). A secret's frame never reaches one: only `Trust::Records` and `Trust::Text` have
//! a context (`Trust::ctx`), and the scheduler hands a compressor nothing of a kind marked secret.

use std::collections::BTreeMap;

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::conn::{Lane, Trust, secret};
use crate::frames::Kind;

/// An encoded frame a compressor may take: made only by [`Compressible::of`], which refuses every
/// kind marked secret and every frame of the secret trust class. A compressor takes nothing else,
/// so no secret can enter a context's window (docs/browser-protocol.md, "Compression").
#[derive(Debug, Clone, Copy)]
pub struct Compressible<'a> {
    bytes: &'a [u8],
    ctx: u8,
}

impl<'a> Compressible<'a> {
    /// The frame `bytes` of `kind`, of trust class `trust`, when it may be compressed: never a
    /// secret's, a fragment's or a compressed batch's.
    pub fn of(kind: Kind, trust: Trust, bytes: &'a [u8]) -> Option<Self> {
        if secret(kind) || matches!(kind, Kind::Fragment | Kind::Compressed) {
            return None;
        }
        Some(Self { bytes, ctx: trust.ctx()? })
    }

    pub fn ctx(&self) -> u8 {
        self.ctx
    }
}

/// Most lanes with contexts of their own on a carrier whose lanes are streams; past it, a lane's
/// frames go uncompressed.
pub const MAX_LANE_CONTEXTS: usize = 8;

/// Deflates a batch, sync-flushed, appending to `out`.
fn deflate_into(z: &mut Compress, input: &[u8], out: &mut Vec<u8>) {
    out.reserve(input.len().saturating_add(64));
    let mut at = 0usize;
    loop {
        let before_in = z.total_in();
        let rest = input.get(at..).unwrap_or(&[]);
        if out.capacity() == out.len() {
            out.reserve(256);
        }
        match z.compress_vec(rest, out, FlushCompress::Sync) {
            Ok(_) => {}
            Err(_) => return,
        }
        at = at.saturating_add(usize::try_from(z.total_in().saturating_sub(before_in)).unwrap_or(0));
        // Done once all is in and the flush left room unused (its output is all out).
        if at >= input.len() && out.len() < out.capacity() {
            return;
        }
    }
}

/// The server's contexts.
pub struct Compressors {
    multistream: bool,
    by: BTreeMap<(u8, Option<u32>), Compress>,
}

impl std::fmt::Debug for Compressors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compressors").field("multistream", &self.multistream).field("contexts", &self.by.len()).finish()
    }
}

impl Compressors {
    pub fn new(multistream: bool) -> Self {
        Self { multistream, by: BTreeMap::new() }
    }

    pub fn multistream(&self) -> bool {
        self.multistream
    }

    /// A batch of frames of one context, deflated in it and sync-flushed: the batch's raw length
    /// and its deflate. Frames of another context than the first's are left out (none come: the
    /// scheduler batches by context).
    pub fn deflate(&mut self, lane: Lane, frames: &[Compressible<'_>]) -> (usize, Vec<u8>) {
        let Some(ctx) = frames.first().map(|f| f.ctx) else { return (0, Vec::new()) };
        let mut raw = Vec::new();
        for f in frames.iter().filter(|f| f.ctx == ctx) {
            raw.extend_from_slice(f.bytes);
        }
        let lane_id = match (self.multistream, lane) {
            (true, Lane::Sub(id)) => Some(id),
            _ => None,
        };
        let z = self.by.entry((ctx, lane_id)).or_insert_with(|| Compress::new(Compression::new(6), false));
        let mut out = Vec::new();
        deflate_into(z, &raw, &mut out);
        (raw.len(), out)
    }

    /// The context a batch goes in.
    pub fn ctx_of(frames: &[Compressible<'_>]) -> Option<u8> {
        frames.first().map(|f| f.ctx)
    }

    pub fn contexts(&self) -> usize {
        self.by.len()
    }
}

/// A client's contexts: each inflates what its context's batches hold, in order.
pub struct Inflaters {
    by: BTreeMap<(u8, Option<u32>), Decompress>,
    /// Most bytes one batch may inflate to.
    max: usize,
}

impl std::fmt::Debug for Inflaters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inflaters").field("contexts", &self.by.len()).finish()
    }
}

/// Why a batch didn't inflate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InflateError {
    /// Not deflate, or not its context's next.
    Corrupt,
    /// Past the bound on what one batch may hold.
    TooLarge,
}

impl Inflaters {
    pub fn new(max: usize) -> Self {
        Self { by: BTreeMap::new(), max }
    }

    pub fn inflate(&mut self, ctx: u8, lane: Option<u32>, data: &[u8]) -> Result<Vec<u8>, InflateError> {
        let z = self.by.entry((ctx, lane)).or_insert_with(|| Decompress::new(false));
        let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(4).min(self.max).max(64));
        let mut at = 0usize;
        loop {
            if out.len() >= self.max {
                return Err(InflateError::TooLarge);
            }
            if out.capacity() == out.len() {
                out.reserve(out.len().max(256).min(self.max.saturating_sub(out.len()).max(1)));
            }
            let before_in = z.total_in();
            let before_out = z.total_out();
            let rest = data.get(at..).unwrap_or(&[]);
            let st = z.decompress_vec(rest, &mut out, FlushDecompress::Sync).map_err(|_| InflateError::Corrupt)?;
            at = at.saturating_add(usize::try_from(z.total_in().saturating_sub(before_in)).unwrap_or(0));
            let made = z.total_out().saturating_sub(before_out);
            if st == Status::StreamEnd {
                return Err(InflateError::Corrupt);
            }
            if at >= data.len() && out.len() < out.capacity() {
                return Ok(out);
            }
            if made == 0 && at >= data.len() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(kind: Kind, trust: Trust, b: &[u8]) -> Vec<Compressible<'_>> {
        Compressible::of(kind, trust, b).into_iter().collect()
    }

    #[test]
    fn batches_inflate_in_order_through_one_context() {
        let mut c = Compressors::new(false);
        let mut d = Inflaters::new(1 << 20);
        for i in 0..50 {
            let raw = format!("{{\"cpu\":{i},\"mem\":12345,\"name\":\"adas-mac-studio\"}}").into_bytes();
            let (_, z) = c.deflate(Lane::Sub(3), &one(Kind::Change, Trust::Records, &raw));
            assert_eq!(d.inflate(1, None, &z), Ok(raw));
        }
        let big = vec![b'x'; 300_000];
        let (_, z) = c.deflate(Lane::Control, &one(Kind::Change, Trust::Text, &big));
        assert_eq!(d.inflate(2, None, &z), Ok(big));
        let (_, z) = Compressors::new(false).deflate(Lane::Control, &one(Kind::Change, Trust::Text, &[0u8; 5000]));
        assert_eq!(Inflaters::new(1000).inflate(2, None, &z), Err(InflateError::TooLarge));
        assert!(d.inflate(1, None, &[0xff, 0xff, 0xff]).is_err());
    }

    /// bp-g2 step 8, AC: no kind marked secret can reach a compressor, whatever its trust class.
    #[test]
    fn bp_g2_s8_no_secret_kind_reaches_a_compressor() {
        let mut reached = 0;
        for k in crate::frames::KINDS {
            for t in [Trust::Secret, Trust::Records, Trust::Text] {
                if secret(*k) && Compressible::of(*k, t, b"x").is_some() {
                    reached += 1;
                }
                if t == Trust::Secret && Compressible::of(*k, t, b"x").is_some() {
                    reached += 1;
                }
            }
        }
        assert_eq!(reached, 0);
        assert!(secret(Kind::SecretAnswer) && secret(Kind::Open));
    }
}
