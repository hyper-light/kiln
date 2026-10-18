//! A store's manifest: the files it holds and the state of the group's log it was last cut at
//! (`docs/observe.md` §4.5, §5).
//!
//! Text, one record a line, checksummed, so an operator can read it; replaced whole by writing a
//! new file, syncing it, renaming it over the old and syncing the directory (POSIX `rename` is
//! atomic, and the directory sync makes the rename durable), so a crash leaves the old manifest or
//! the new, never a mix.
//!
//! ```text
//! kiln-obs manifest 2
//! durable <index> <term>
//! next <seq>
//! cutoff <ms>
//! max_series <n>
//! conf <voters> <learners> <outgoing voters> <next learners> <auto leave 0|1>   (ids, commas; "-")
//! session <sender, hex> <seq> <at ms> <index>
//! block <seq> <file name>
//! logs <seq> <file name>
//! crc <crc32c of the lines above, hex>
//! ```
//!
//! Everything but the files is the replicated state at `durable` that the files do not hold: what
//! retention cut off, the bound on series, each sender's last write, and the group's configuration.
//! A replica that opens holds exactly what it held at `durable`, and the log brings the rest.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::crc32c;

pub const NAME: &str = "MANIFEST";
const TMP: &str = "MANIFEST.tmp";
const FIRST_LINE: &str = "kiln-obs manifest 2";

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

/// The group's configuration, as Raft states it (hyper-raft's `ConfState`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Membership {
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub voters_outgoing: Vec<u64>,
    pub learners_next: Vec<u64>,
    pub auto_leave: bool,
}

/// A sender's last write applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Session {
    pub seq: u64,
    /// The sender's clock as it sent it (ms).
    pub at: i64,
    /// The entry that applied it.
    pub index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub durable: Point,
    pub next_seq: u64,
    /// Nothing older is kept (ms).
    pub cutoff: i64,
    /// 0: no bound.
    pub max_series: u64,
    pub conf: Membership,
    pub sessions: BTreeMap<u64, Session>,
    pub blocks: Vec<BlockEntry>,
    /// Logs files.
    pub logs: Vec<BlockEntry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self { durable: Point::default(), next_seq: 0, cutoff: i64::MIN, max_series: 0, conf: Membership::default(), sessions: BTreeMap::new(), blocks: Vec::new(), logs: Vec::new() }
    }
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

/// Whether `file` may name a file of a store: a plain name in its directory, never a path.
pub fn plain_name(file: &str) -> bool {
    !file.is_empty() && !file.contains(['/', '\\']) && !file.starts_with('.')
}

fn ids(v: &[u64]) -> String {
    if v.is_empty() {
        return "-".into();
    }
    v.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

fn parse_ids(s: &str) -> Option<Vec<u64>> {
    if s == "-" {
        return Some(Vec::new());
    }
    s.split(',').map(|x| x.parse().ok()).collect()
}

impl Manifest {
    fn encode(&self) -> String {
        use std::fmt::Write as _;
        let mut body = String::new();
        let c = &self.conf;
        // Writing to a String cannot fail.
        let _ = writeln!(body, "{FIRST_LINE}");
        let _ = writeln!(body, "durable {} {}", self.durable.index, self.durable.term);
        let _ = writeln!(body, "next {}", self.next_seq);
        let _ = writeln!(body, "cutoff {}", self.cutoff);
        let _ = writeln!(body, "max_series {}", self.max_series);
        let _ = writeln!(body, "conf {} {} {} {} {}", ids(&c.voters), ids(&c.learners), ids(&c.voters_outgoing), ids(&c.learners_next), u8::from(c.auto_leave));
        for (sender, s) in &self.sessions {
            let _ = writeln!(body, "session {sender:016x} {} {} {}", s.seq, s.at, s.index);
        }
        for b in &self.blocks {
            let _ = writeln!(body, "block {} {}", b.seq, b.file);
        }
        for b in &self.logs {
            let _ = writeln!(body, "logs {} {}", b.seq, b.file);
        }
        let crc = crc32c::checksum(body.as_bytes());
        let _ = writeln!(body, "crc {crc:08x}");
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
            let f: Vec<&str> = line.split(' ').collect();
            match f.as_slice() {
                ["durable", i, t] => m.durable = Point { index: i.parse().map_err(|_| damaged("durable"))?, term: t.parse().map_err(|_| damaged("durable"))? },
                ["next", n] => m.next_seq = n.parse().map_err(|_| damaged("next"))?,
                ["cutoff", c] => m.cutoff = c.parse().map_err(|_| damaged("cutoff"))?,
                ["max_series", n] => m.max_series = n.parse().map_err(|_| damaged("max_series"))?,
                ["conf", v, l, vo, ln, al] => {
                    let p = |s: &str| parse_ids(s).ok_or_else(|| damaged("conf"));
                    m.conf = Membership { voters: p(v)?, learners: p(l)?, voters_outgoing: p(vo)?, learners_next: p(ln)?, auto_leave: *al == "1" };
                }
                ["session", sender, seq, at, index] => {
                    let sender = u64::from_str_radix(sender, 16).map_err(|_| damaged("session"))?;
                    let s = Session { seq: seq.parse().map_err(|_| damaged("session"))?, at: at.parse().map_err(|_| damaged("session"))?, index: index.parse().map_err(|_| damaged("session"))? };
                    m.sessions.insert(sender, s);
                }
                [kind @ ("block" | "logs"), seq, file] => {
                    if !plain_name(file) {
                        return Err(damaged("block file"));
                    }
                    let e = BlockEntry { seq: seq.parse().map_err(|_| damaged("block"))?, file: (*file).to_owned() };
                    if *kind == "block" {
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
        let m = Manifest {
            durable: Point { index: 42, term: 3 },
            next_seq: 7,
            cutoff: 1_700_000_000_000,
            max_series: 250_000,
            conf: Membership { voters: vec![1, 2, 3], learners: vec![9], voters_outgoing: Vec::new(), learners_next: vec![4], auto_leave: true },
            sessions: [(0xdead_beef, Session { seq: 5, at: 1_700_000_000_123, index: 40 }), (7, Session { seq: 1, at: -1, index: 2 })].into(),
            blocks: vec![BlockEntry { seq: 5, file: block_file(5) }, BlockEntry { seq: 6, file: block_file(6) }],
            logs: vec![BlockEntry { seq: 6, file: logs_file(6) }],
        };
        m.save(dir.path()).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), Some(m.clone()));
        let empty = Manifest::default();
        empty.save(dir.path()).unwrap();
        assert_eq!(Manifest::load(dir.path()).unwrap(), Some(empty));
        m.save(dir.path()).unwrap();
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
