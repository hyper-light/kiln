//! A replica's image (`docs/observe.md` §5): what a snapshot carries to a member that lacks entries
//! the log no longer holds.
//!
//! It is small, whatever the store holds: the replicated state its files do not (the cutoff, the
//! bound on series, the senders' sessions) and its files by name, size and footer checksum. The
//! files travel apart and are in place before the snapshot is installed, as TiKV sends a region's
//! SST files by its snapshot service and steps the Raft message that names them once they are
//! received (tikv `components/raftstore/src/store/snap.rs`). A Raft snapshot is held whole in
//! memory by the leader that serves it (hyper-durable's `Replica::compact`), so a store's gigabytes
//! never are.
//!
//! ```text
//! image := MAGIC 8 | metrics' cutoff i64-le | logs' cutoff i64-le | max series varint
//!        | sessions varint | (sender u64-le | seq varint | at zigzag | index varint)*
//!        | files varint | (kind u8 | seq varint | bytes varint | crc u32-le | name varint-length)*
//!        | crc32c of everything before u32-le
//! ```

use std::collections::BTreeMap;

use crate::crc32c;
use crate::manifest::{Session, plain_name};
use crate::varint;

pub const MAGIC: [u8; 8] = *b"KOIMAGE1";

/// Files an image names at the most: a store keeps O(log n) blocks of n samples (Bentley–Saxe) and
/// a logs file a cut, so this many is damage, not a store.
pub const MAX_FILES: usize = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Block,
    Logs,
}

/// A file an image names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageFile {
    pub kind: FileKind,
    /// Its place in write order: a later block's sample wins at a time.
    pub seq: u64,
    /// Its name in the snapshot it is fetched from.
    pub name: String,
    pub bytes: u64,
    /// Its footer's checksum.
    pub crc: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub cutoff: i64,
    pub logs_cutoff: i64,
    pub max_series: u64,
    pub sessions: BTreeMap<u64, Session>,
    /// In write order.
    pub files: Vec<ImageFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    #[error("not an image of this format")]
    Magic,
    #[error("an image's checksum")]
    Checksum,
    #[error("an image is damaged: {0}")]
    Damaged(&'static str),
}

impl Image {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.cutoff.to_le_bytes());
        out.extend_from_slice(&self.logs_cutoff.to_le_bytes());
        varint::put(out, self.max_series);
        varint::put(out, self.sessions.len() as u64);
        for (sender, s) in &self.sessions {
            out.extend_from_slice(&sender.to_le_bytes());
            varint::put(out, s.seq);
            varint::put(out, varint::zig(s.at));
            varint::put(out, s.index);
        }
        varint::put(out, self.files.len() as u64);
        for f in &self.files {
            out.push(match f.kind {
                FileKind::Block => 0,
                FileKind::Logs => 1,
            });
            varint::put(out, f.seq);
            varint::put(out, f.bytes);
            out.extend_from_slice(&f.crc.to_le_bytes());
            varint::put(out, f.name.len() as u64);
            out.extend_from_slice(f.name.as_bytes());
        }
        let crc = crc32c::checksum(out.get(start..).unwrap_or(&[]));
        out.extend_from_slice(&crc.to_le_bytes());
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ImageError> {
        let (body, crc) = bytes.split_last_chunk::<4>().ok_or(ImageError::Magic)?;
        let mut at = body.strip_prefix(&MAGIC[..]).ok_or(ImageError::Magic)?;
        if crc32c::checksum(body) != u32::from_le_bytes(*crc) {
            return Err(ImageError::Checksum);
        }
        let d = ImageError::Damaged;
        let (cutoff, rest) = at.split_first_chunk::<8>().ok_or(d("cutoff"))?;
        let (logs_cutoff, rest) = rest.split_first_chunk::<8>().ok_or(d("cutoff"))?;
        at = rest;
        let mut image = Self { cutoff: i64::from_le_bytes(*cutoff), logs_cutoff: i64::from_le_bytes(*logs_cutoff), max_series: varint::get(&mut at).ok_or(d("max series"))?, ..Self::default() };
        let sessions = varint::get(&mut at).ok_or(d("sessions"))?;
        // Each session takes 11 bytes at least: a count past that is damage.
        if sessions > (at.len() / 11) as u64 {
            return Err(d("sessions"));
        }
        for _ in 0..sessions {
            let (sender, rest) = at.split_first_chunk::<8>().ok_or(d("a session"))?;
            at = rest;
            let seq = varint::get(&mut at).ok_or(d("a session"))?;
            let at_ms = varint::zag(varint::get(&mut at).ok_or(d("a session"))?);
            let index = varint::get(&mut at).ok_or(d("a session"))?;
            image.sessions.insert(u64::from_le_bytes(*sender), Session { seq, at: at_ms, index });
        }
        let files = usize::try_from(varint::get(&mut at).ok_or(d("files"))?).map_err(|_| d("files"))?;
        if files > MAX_FILES || files > at.len() / 8 {
            return Err(d("files"));
        }
        image.files.reserve_exact(files);
        for _ in 0..files {
            let (&kind, rest) = at.split_first().ok_or(d("a file"))?;
            at = rest;
            let kind = match kind {
                0 => FileKind::Block,
                1 => FileKind::Logs,
                _ => return Err(d("a file's kind")),
            };
            let seq = varint::get(&mut at).ok_or(d("a file"))?;
            let bytes = varint::get(&mut at).ok_or(d("a file"))?;
            let (crc, rest) = at.split_first_chunk::<4>().ok_or(d("a file"))?;
            at = rest;
            let len = usize::try_from(varint::get(&mut at).ok_or(d("a file's name"))?).map_err(|_| d("a file's name"))?;
            let (name, rest) = at.split_at_checked(len).ok_or(d("a file's name"))?;
            at = rest;
            let name = std::str::from_utf8(name).map_err(|_| d("a file's name"))?;
            if !plain_name(name) {
                return Err(d("a file's name"));
            }
            image.files.push(ImageFile { kind, seq, name: name.to_owned(), bytes, crc: u32::from_le_bytes(*crc) });
        }
        if !at.is_empty() {
            return Err(d("bytes after the files"));
        }
        Ok(image)
    }

    /// Bytes of the files it names.
    pub fn file_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_image_round_trips_and_damage_is_refused() {
        let image = Image {
            cutoff: 1_700_000_000_000,
            logs_cutoff: 1_700_000_000_007,
            max_series: 100_000,
            sessions: [(u64::MAX, Session { seq: 9, at: -4, index: 77 }), (3, Session { seq: 1, at: 1_700_000_000_001, index: 5 })].into(),
            files: vec![
                ImageFile { kind: FileKind::Block, seq: 4, name: "0000000000000004.block".into(), bytes: 123_456, crc: 0xdead_beef },
                ImageFile { kind: FileKind::Logs, seq: 5, name: "0000000000000005.logs".into(), bytes: 9, crc: 1 },
            ],
        };
        let mut bytes = Vec::new();
        image.encode(&mut bytes);
        assert_eq!(Image::decode(&bytes), Ok(image.clone()));
        assert_eq!(image.file_bytes(), 123_465);
        let mut empty = Vec::new();
        Image::default().encode(&mut empty);
        assert_eq!(Image::decode(&empty), Ok(Image::default()));
        // Every flipped bit and every cut is refused, never a panic.
        for i in 0..bytes.len() * 8 {
            let mut bad = bytes.clone();
            bad[i / 8] ^= 1 << (i % 8);
            assert!(Image::decode(&bad).is_err(), "bit {i}");
        }
        for cut in 0..bytes.len() {
            assert!(Image::decode(&bytes[..cut]).is_err());
        }
        // A name that is a path is refused under a valid checksum.
        let evil = Image { files: vec![ImageFile { kind: FileKind::Block, seq: 0, name: "../MANIFEST".into(), bytes: 1, crc: 0 }], ..Image::default() };
        let mut bytes = Vec::new();
        evil.encode(&mut bytes);
        assert!(Image::decode(&bytes).is_err());
    }
}
