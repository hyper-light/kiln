//! The archive's objects as bytes: each is its SHA-256 and then its body, so a restore refuses an
//! object the store gave back other than as it was made (CLAUDE.md §3). Bodies are postcard.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The archive's format: a format value, raised when a body's layout changes.
pub(super) const FORMAT: u8 = 1;

const SUM_BYTES: usize = 32;

/// The log generation a segment's frames are of, as its header names it: SQLite's WAL header's
/// checkpoint sequence and salts (sqlite.org/fileformat2.html, "WAL File Format"), which change
/// with each restart of the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Identity {
    pub seq: u32,
    pub salt1: u32,
    pub salt2: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct StoredMark {
    pub frame: u64,
    pub ns: u64,
    pub unknown_from: Option<u64>,
}

/// A segment: frames `first..` of the log, its header, and the marks of the commits it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Segment {
    pub format: u8,
    pub generation: u64,
    pub identity: Identity,
    pub first: u64,
    pub header: Vec<u8>,
    pub marks: Vec<StoredMark>,
    pub frames: Vec<u8>,
}

/// The manifest's own format: a format value. 1 had no `from` (every chunk its base's own); 2
/// names the base each chunk is stored under, so a base reuses an earlier base's unchanged chunks.
pub(super) const MANIFEST_FORMAT: u8 = 2;

/// A base: its pages are in `chunks` objects of `chunk_bytes` (the last shorter), `file_bytes` in
/// all. `from`: the base each chunk is stored under, in runs of (base id, chunks), in chunk order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub format: u8,
    pub id: u64,
    pub start_gen: u64,
    pub end: (u64, u64),
    pub file_bytes: u64,
    pub chunk_bytes: u64,
    pub chunks: u64,
    pub from: Vec<(u64, u64)>,
}

/// A manifest of format 1, as archives made before format 2 hold them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestV1 {
    format: u8,
    id: u64,
    start_gen: u64,
    end: (u64, u64),
    file_bytes: u64,
    chunk_bytes: u64,
    chunks: u64,
}

impl Manifest {
    /// The base each chunk is stored under, chunk by chunk.
    pub fn chunk_bases(&self) -> anyhow::Result<Vec<u64>> {
        if self.from.is_empty() {
            return Ok(vec![self.id; usize::try_from(self.chunks)?]);
        }
        let mut out = Vec::with_capacity(usize::try_from(self.chunks)?);
        for &(base, run) in &self.from {
            if u64::try_from(out.len())?.saturating_add(run) > self.chunks {
                anyhow::bail!("base {}'s manifest names more chunks than it has", self.id);
            }
            out.extend(std::iter::repeat_n(base, usize::try_from(run)?));
        }
        if u64::try_from(out.len())? != self.chunks {
            anyhow::bail!("base {}'s manifest names fewer chunks than it has", self.id);
        }
        Ok(out)
    }
}

/// `bases` (one per chunk) as runs of (base, chunks in a row).
pub(super) fn runs(bases: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &base in bases {
        match out.last_mut() {
            Some((b, n)) if *b == base => *n = n.saturating_add(1),
            _ => out.push((base, 1)),
        }
    }
    out
}

/// A manifest of either format.
pub(super) fn manifest(key: &str, object: &[u8]) -> anyhow::Result<Manifest> {
    let body = opened(key, object)?;
    let bad = |e: postcard::Error| anyhow::anyhow!("archive object {key} doesn't decode: {e}");
    match body.first() {
        Some(1) => {
            let v1: ManifestV1 = postcard::from_bytes(body).map_err(bad)?;
            Ok(Manifest {
                format: v1.format,
                id: v1.id,
                start_gen: v1.start_gen,
                end: v1.end,
                file_bytes: v1.file_bytes,
                chunk_bytes: v1.chunk_bytes,
                chunks: v1.chunks,
                from: Vec::new(),
            })
        }
        Some(&MANIFEST_FORMAT) => postcard::from_bytes(body).map_err(bad),
        other => anyhow::bail!("archive object {key} is a manifest of no known format ({other:?})"),
    }
}

/// A through mark ([`super::plan::ThroughKey`]): the log shipped through frame `last` of
/// `generation` held every commit stamped at or before `through`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Through {
    pub format: u8,
    pub generation: u64,
    pub last: u64,
    pub through: u64,
}

/// `body` with its sum before it.
pub(super) fn sealed(body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(
        body.len()
            .checked_add(SUM_BYTES)
            .ok_or_else(|| anyhow::anyhow!("an archive object's size overflows"))?,
    );
    out.extend_from_slice(&Sha256::digest(body));
    out.extend_from_slice(body);
    Ok(out)
}

/// The body of a sealed object, refused if its sum doesn't hold.
pub(super) fn opened<'a>(key: &str, object: &'a [u8]) -> anyhow::Result<&'a [u8]> {
    let (sum, body) = object
        .split_at_checked(SUM_BYTES)
        .ok_or_else(|| anyhow::anyhow!("archive object {key} is shorter than its sum"))?;
    if Sha256::digest(body).as_slice() != sum {
        anyhow::bail!("archive object {key} doesn't match its sum");
    }
    Ok(body)
}

pub(super) fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    sealed(&postcard::to_allocvec(value)?)
}

pub(super) fn decode<T: for<'de> Deserialize<'de>>(key: &str, object: &[u8]) -> anyhow::Result<T> {
    let value: T = postcard::from_bytes(opened(key, object)?)
        .map_err(|e| anyhow::anyhow!("archive object {key} doesn't decode: {e}"))?;
    Ok(value)
}

/// The bytes a sealed segment takes beyond its frames: what is left of an object for frames.
pub(super) fn segment_overhead(segment: &Segment) -> anyhow::Result<usize> {
    let empty = Segment {
        frames: Vec::new(),
        ..segment.clone()
    };
    // The frames' length prefix grows with them: a varint of a usize is at most 10 bytes
    // (postcard's wire format, "varint(usize)").
    encode(&empty)?
        .len()
        .checked_add(10)
        .ok_or_else(|| anyhow::anyhow!("a segment's size overflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-T1.12.P10: an object round-trips; a byte changed is refused.
    #[test]
    fn t_t1_12_p10_sealed_objects() {
        let m = Manifest {
            format: MANIFEST_FORMAT,
            id: 1,
            start_gen: 2,
            end: (3, 4),
            file_bytes: 8192,
            chunk_bytes: 4096,
            chunks: 2,
            from: vec![(1, 2)],
        };
        let mut bytes = encode(&m).unwrap();
        assert_eq!(manifest("k", &bytes).unwrap(), m);
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert!(decode::<Manifest>("k", &bytes).is_err());
        assert!(opened("k", &[0; 3]).is_err());
    }

    /// T-T1.12.I2: a manifest of format 1 still reads, every chunk its own base's; a format 2
    /// manifest's runs give each chunk's base, and runs that don't add up to its chunks are refused.
    #[test]
    fn t_t1_12_i2_manifest_formats() {
        let v1 = ManifestV1 {
            format: 1,
            id: 7,
            start_gen: 1,
            end: (1, 9),
            file_bytes: 3 * 4096,
            chunk_bytes: 4096,
            chunks: 3,
        };
        let read = manifest("k", &encode(&v1).unwrap()).unwrap();
        assert_eq!(read.chunk_bases().unwrap(), vec![7, 7, 7]);
        let bases = [3, 3, 9, 3, 9, 9];
        let m = Manifest {
            format: MANIFEST_FORMAT,
            id: 9,
            start_gen: 2,
            end: (2, 1),
            file_bytes: 6 * 4096,
            chunk_bytes: 4096,
            chunks: 6,
            from: runs(&bases),
        };
        assert_eq!(m.from, vec![(3, 2), (9, 1), (3, 1), (9, 2)]);
        assert_eq!(m.chunk_bases().unwrap(), bases.to_vec());
        let short = Manifest {
            from: vec![(3, 2)],
            ..m.clone()
        };
        assert!(short.chunk_bases().is_err());
        let long = Manifest {
            from: vec![(3, 7)],
            ..m
        };
        assert!(long.chunk_bases().is_err());
    }
}
