//! A store's manifest: the blocks it holds and the point of the group's log its head was last cut at
//! (`docs/observe.md` §4.5, §5).
//!
//! Text, one record a line, checksummed, so an operator can read it; replaced whole by writing a
//! new file, syncing it, renaming it over the old and syncing the directory (POSIX `rename` is
//! atomic, and the directory sync makes the rename durable), so a crash leaves the old manifest or
//! the new, never a mix.
//!
//! ```text
//! kiln-obs manifest 1
//! durable <index> <term>
//! next <seq>
//! block <seq> <file name>
//! logs <seq> <file name>
//! crc <crc32c of the lines above, hex>
//! ```

use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::crc32c;

pub const NAME: &str = "MANIFEST";
const TMP: &str = "MANIFEST.tmp";
const FIRST_LINE: &str = "kiln-obs manifest 1";

/// A point of the group's log: an entry's index and its term.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub index: u64,
    pub term: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockEntry {
    pub seq: u64,
    pub file: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub durable: Point,
    pub next_seq: u64,
    pub blocks: Vec<BlockEntry>,
    /// Logs files.
    pub logs: Vec<BlockEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}: damaged: {1}")]
    Damaged(PathBuf, &'static str),
}

/// A block file's name for its sequence number.
pub fn block_file(seq: u64) -> String {
    format!("{seq:016}.block")
}

/// A logs file's name for its sequence number.
pub fn logs_file(seq: u64) -> String {
    format!("{seq:016}.logs")
}

impl Manifest {
    fn encode(&self) -> String {
        let mut body = String::new();
        body.push_str(FIRST_LINE);
        body.push('\n');
        body.push_str(&format!("durable {} {}\nnext {}\n", self.durable.index, self.durable.term, self.next_seq));
        for b in &self.blocks {
            body.push_str(&format!("block {} {}\n", b.seq, b.file));
        }
        for b in &self.logs {
            body.push_str(&format!("logs {} {}\n", b.seq, b.file));
        }
        let crc = crc32c::checksum(body.as_bytes());
        body.push_str(&format!("crc {crc:08x}\n"));
        body
    }

    fn decode(text: &str, path: &Path) -> Result<Self, ManifestError> {
        let damaged = |why| ManifestError::Damaged(path.to_owned(), why);
        let crc_at = text.rfind("crc ").ok_or_else(|| damaged("no checksum"))?;
        let (body, crc_line) = text.split_at(crc_at);
        let want = u32::from_str_radix(crc_line.trim_start_matches("crc ").trim(), 16).map_err(|_| damaged("checksum"))?;
        if crc32c::checksum(body.as_bytes()) != want {
            return Err(damaged("checksum"));
        }
        let mut lines = body.lines();
        if lines.next() != Some(FIRST_LINE) {
            return Err(damaged("not a kiln-obs manifest of this version"));
        }
        let mut m = Self::default();
        for line in lines {
            let mut f = line.split(' ');
            match (f.next(), f.next(), f.next(), f.next()) {
                (Some("durable"), Some(i), Some(t), None) => {
                    m.durable = Point { index: i.parse().map_err(|_| damaged("durable"))?, term: t.parse().map_err(|_| damaged("durable"))? };
                }
                (Some("next"), Some(n), None, None) => m.next_seq = n.parse().map_err(|_| damaged("next"))?,
                (Some(kind @ ("block" | "logs")), Some(seq), Some(file), None) => {
                    // A file is a name in the store's directory, never a path.
                    if file.contains(['/', '\\']) || file.starts_with('.') {
                        return Err(damaged("block file"));
                    }
                    let e = BlockEntry { seq: seq.parse().map_err(|_| damaged("block"))?, file: file.to_owned() };
                    if kind == "block" {
                        m.blocks.push(e);
                    } else {
                        m.logs.push(e);
                    }
                }
                _ => return Err(damaged("a line")),
            }
        }
        Ok(m)
    }

    /// The manifest in `dir`, or none.
    pub fn load(dir: &Path) -> Result<Option<Self>, ManifestError> {
        let path = dir.join(NAME);
        match fs::read_to_string(&path) {
            Ok(text) => Self::decode(&text, &path).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ManifestError::Io { path, source }),
        }
    }

    /// Replaces the manifest in `dir`, durably.
    pub fn save(&self, dir: &Path) -> Result<(), ManifestError> {
        let tmp = dir.join(TMP);
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| ManifestError::Io { path, source }
        };
        let mut f = File::create(&tmp).map_err(io_err(&tmp))?;
        f.write_all(self.encode().as_bytes()).map_err(io_err(&tmp))?;
        f.sync_all().map_err(io_err(&tmp))?;
        drop(f);
        let path = dir.join(NAME);
        fs::rename(&tmp, &path).map_err(io_err(&path))?;
        sync_dir(dir).map_err(io_err(dir))
    }
}

/// Makes a directory's entries durable (a rename, a new file). Windows has no directory handle
/// to sync; NTFS journals its metadata.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_round_trips_and_damage_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), None);
        let m = Manifest { durable: Point { index: 42, term: 3 }, next_seq: 7, blocks: vec![BlockEntry { seq: 5, file: block_file(5) }, BlockEntry { seq: 6, file: block_file(6) }], logs: vec![BlockEntry { seq: 6, file: logs_file(6) }] };
        m.save(dir.path()).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), Some(m.clone()));
        let path = dir.path().join(NAME);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("durable 42", "durable 43")).unwrap();
        assert!(Manifest::load(dir.path()).is_err());
        // A block "file" that is a path is refused even under a valid checksum.
        let evil = Manifest { blocks: vec![BlockEntry { seq: 1, file: "../../etc/passwd".into() }], ..m };
        fs::write(&path, evil.encode()).unwrap();
        assert!(Manifest::load(dir.path()).is_err());
    }
}
