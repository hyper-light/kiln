//! A logs file: the log blocks one cut wrote (`docs/observe.md` §4.6).
//!
//! ```text
//! file   := MAGIC 8 | FORMAT u32-le | (summary | text)* | index | footer
//! index  := blocks varint | per block: summary offset, summary length, text offset, text length
//!           (varints) | summary crc32c u32-le | text crc32c u32-le
//! footer := index offset u64-le | index length u64-le | min ts i64-le | max ts i64-le
//!           | crc32c of the index and the footer's first 32 bytes u32-le | TAIL 4      (40 bytes)
//! ```
//!
//! Opening reads the index and every block's summary (its time range, dictionaries and token
//! filter: about 3 KB a 256 KiB block, §12.7) and keeps them; a block's text is read with one
//! positioned read when a search cannot rule the block out, and checked.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use super::block::LogBlock;
use crate::crc32c;
use crate::varint;

pub const MAGIC: [u8; 8] = *b"KOLOGS1\n";
pub const FORMAT: u32 = 1;
const TAIL: [u8; 4] = *b"KOL1";
const FOOTER: usize = 40;
const HEADER: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum LogFileError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}: damaged: {1}")]
    Damaged(PathBuf, &'static str),
}

/// Where a block's text lies in its file.
#[derive(Debug, Clone, Copy)]
struct TextAt {
    off: u64,
    len: u32,
    crc: u32,
}

/// An open logs file: every block's summary (its text left in the file).
#[derive(Debug)]
pub struct LogFile {
    path: PathBuf,
    file: File,
    pub min_ts: i64,
    pub max_ts: i64,
    pub bytes: u64,
    /// The footer's checksum: with `bytes`, what names this file's contents.
    pub crc: u32,
    /// Summaries: each block with its `text` empty.
    pub blocks: Vec<LogBlock>,
    texts: Vec<TextAt>,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> LogFileError + '_ {
    move |source| LogFileError::Io { path: path.to_owned(), source }
}

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0usize;
    while let Some(rest) = buf.get_mut(done..)
        && !rest.is_empty()
    {
        let n = f.seek_read(rest, off + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "logs file ended early"));
        }
        done += n;
    }
    Ok(())
}

/// Writes `blocks` to `path`, durably; returns it open.
pub fn write(path: &Path, blocks: &[LogBlock]) -> Result<LogFile, LogFileError> {
    let mut out = Vec::with_capacity(HEADER + blocks.iter().map(|b| b.text.len() + b.summary_bytes()).sum::<usize>() + FOOTER);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    let mut index = Vec::new();
    varint::put(&mut index, blocks.len() as u64);
    let (mut min_ts, mut max_ts) = (i64::MAX, i64::MIN);
    for b in blocks {
        let summary_off = out.len() as u64;
        let mut summary = Vec::new();
        let stripped = LogBlock { text: Vec::new(), ..b.clone() };
        stripped.encode(&mut summary);
        out.extend_from_slice(&summary);
        let text_off = out.len() as u64;
        out.extend_from_slice(&b.text);
        for v in [summary_off, summary.len() as u64, text_off, b.text.len() as u64] {
            varint::put(&mut index, v);
        }
        index.extend_from_slice(&crc32c::checksum(&summary).to_le_bytes());
        index.extend_from_slice(&crc32c::checksum(&b.text).to_le_bytes());
        min_ts = min_ts.min(b.min_ts);
        max_ts = max_ts.max(b.max_ts);
    }
    let index_off = out.len() as u64;
    out.extend_from_slice(&index);
    let mut foot = Vec::with_capacity(FOOTER);
    foot.extend_from_slice(&index_off.to_le_bytes());
    foot.extend_from_slice(&(index.len() as u64).to_le_bytes());
    foot.extend_from_slice(&min_ts.to_le_bytes());
    foot.extend_from_slice(&max_ts.to_le_bytes());
    let crc = crc32c::update(crc32c::checksum(&index), &foot);
    foot.extend_from_slice(&crc.to_le_bytes());
    foot.extend_from_slice(&TAIL);
    out.extend_from_slice(&foot);
    let mut f = File::create(path).map_err(io_err(path))?;
    f.write_all(&out).map_err(io_err(path))?;
    f.sync_all().map_err(io_err(path))?;
    drop(f);
    LogFile::open(path)
}

impl LogFile {
    pub fn open(path: &Path) -> Result<Self, LogFileError> {
        let file = File::open(path).map_err(io_err(path))?;
        let bytes = file.metadata().map_err(io_err(path))?.len();
        let damaged = |why| LogFileError::Damaged(path.to_owned(), why);
        if bytes < (HEADER + FOOTER) as u64 {
            return Err(damaged("too short"));
        }
        let mut head = [0u8; HEADER];
        read_at(&file, &mut head, 0).map_err(io_err(path))?;
        if head.get(..8) != Some(&MAGIC[..]) || head.get(8..12) != Some(&FORMAT.to_le_bytes()[..]) {
            return Err(damaged("not a logs file of this format"));
        }
        let mut foot = [0u8; FOOTER];
        read_at(&file, &mut foot, bytes - FOOTER as u64).map_err(io_err(path))?;
        if foot.get(36..40) != Some(&TAIL[..]) {
            return Err(damaged("footer"));
        }
        let word = |at: usize| -> Result<[u8; 8], LogFileError> { foot.get(at..at + 8).and_then(|b| b.try_into().ok()).ok_or_else(|| damaged("footer")) };
        let index_off = u64::from_le_bytes(word(0)?);
        let index_len = u64::from_le_bytes(word(8)?);
        let min_ts = i64::from_le_bytes(word(16)?);
        let max_ts = i64::from_le_bytes(word(24)?);
        let crc = u32::from_le_bytes(foot.get(32..36).and_then(|b| b.try_into().ok()).ok_or_else(|| damaged("footer"))?);
        if index_off < HEADER as u64 || index_off.checked_add(index_len) != Some(bytes - FOOTER as u64) {
            return Err(damaged("index bounds"));
        }
        let mut index = vec![0u8; usize::try_from(index_len).map_err(|_| damaged("index length"))?];
        read_at(&file, &mut index, index_off).map_err(io_err(path))?;
        if crc32c::update(crc32c::checksum(&index), foot.get(..32).unwrap_or(&[])) != crc {
            return Err(damaged("index checksum"));
        }
        let mut at = index.as_slice();
        let n = usize::try_from(varint::get(&mut at).ok_or_else(|| damaged("index"))?).map_err(|_| damaged("index"))?;
        if n > index.len() {
            return Err(damaged("index count"));
        }
        let mut blocks = Vec::with_capacity(n);
        let mut texts = Vec::with_capacity(n);
        for _ in 0..n {
            let mut v = [0u64; 4];
            for x in &mut v {
                *x = varint::get(&mut at).ok_or_else(|| damaged("index"))?;
            }
            let (scrc, rest) = at.split_first_chunk::<4>().ok_or_else(|| damaged("index"))?;
            let (tcrc, rest) = rest.split_first_chunk::<4>().ok_or_else(|| damaged("index"))?;
            at = rest;
            let [s_off, s_len, t_off, t_len] = v;
            if s_off.checked_add(s_len).is_none_or(|e| e > index_off) || t_off.checked_add(t_len).is_none_or(|e| e > index_off) {
                return Err(damaged("block bounds"));
            }
            let mut summary = vec![0u8; usize::try_from(s_len).map_err(|_| damaged("summary"))?];
            read_at(&file, &mut summary, s_off).map_err(io_err(path))?;
            if crc32c::checksum(&summary) != u32::from_le_bytes(*scrc) {
                return Err(damaged("a summary's checksum"));
            }
            let b = LogBlock::decode(&summary).map_err(|_| damaged("a summary"))?;
            blocks.push(b);
            texts.push(TextAt { off: t_off, len: u32::try_from(t_len).map_err(|_| damaged("text length"))?, crc: u32::from_le_bytes(*tcrc) });
        }
        Ok(Self { path: path.to_owned(), file, min_ts, max_ts, bytes, crc, blocks, texts })
    }

    /// Block `i` whole: its summary with its text read and checked.
    pub fn load(&self, i: usize, into: &mut LogBlock) -> Result<(), LogFileError> {
        let mut text = std::mem::take(&mut into.text);
        let b = self.read_text(i, &mut text)?;
        *into = LogBlock { text, ..b.clone() };
        Ok(())
    }

    /// Block `i`'s text into `text` (reused across blocks: a block read allocates nothing),
    /// checked against its checksum; the block's summary, which says how it decompresses
    /// ([`LogBlock::decompress_text`]).
    pub fn read_text(&self, i: usize, text: &mut Vec<u8>) -> Result<&LogBlock, LogFileError> {
        let (Some(b), Some(t)) = (self.blocks.get(i), self.texts.get(i)) else { return Err(LogFileError::Damaged(self.path.clone(), "block index")) };
        text.clear();
        text.resize(t.len as usize, 0);
        read_at(&self.file, text, t.off).map_err(io_err(&self.path))?;
        if crc32c::checksum(text) != t.crc {
            return Err(LogFileError::Damaged(self.path.clone(), "a block's text checksum"));
        }
        Ok(b)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads every block's text whole, checked against its checksum and decompressed, and its rows
    /// read: what a file fetched from another replica is put through before it is taken.
    pub fn verify(&self) -> Result<(), LogFileError> {
        let (mut text, mut raw) = (Vec::new(), Vec::new());
        for i in 0..self.blocks.len() {
            let b = self.read_text(i, &mut text)?;
            b.decompress_text(&text, &mut raw).map_err(|_| LogFileError::Damaged(self.path.clone(), "a block's text"))?;
            let mut rows = b.rows(&raw);
            while rows.next_row().map_err(|_| LogFileError::Damaged(self.path.clone(), "a block's rows"))?.is_some() {}
        }
        Ok(())
    }

    /// Bytes held in memory: the summaries.
    pub fn heap_bytes(&self) -> usize {
        self.blocks.iter().map(LogBlock::summary_bytes).sum::<usize>() + self.texts.capacity() * 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::Record;

    #[test]
    fn a_logs_file_holds_its_blocks_and_finds_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.logs");
        let blocks: Vec<LogBlock> = (0..3)
            .map(|b| {
                LogBlock::seal_owned(
                    &(0..200)
                        .map(|i| Record { ts: (b * 1000 + i) * 1_000_000, severity: 9, service: format!("s{b}"), host: "h".into(), source: "x".into(), message: format!("line {i} of block {b}"), ..Record::default() })
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let f = write(&path, &blocks).unwrap();
        assert_eq!(f.blocks.len(), 3);
        assert!(f.blocks.iter().all(|b| b.text.is_empty()));
        let mut whole = LogBlock::seal_owned(&[]);
        for (i, want) in blocks.iter().enumerate() {
            f.load(i, &mut whole).unwrap();
            assert_eq!(&whole, want);
        }
        let again = LogFile::open(&path).unwrap();
        assert_eq!((again.min_ts, again.max_ts), (0, 2199 * 1_000_000));
        // A flipped bit in a text is found when it is loaded; in an index or summary, at open.
        let mut bytes = std::fs::read(&path).unwrap();
        let t = again.texts[1];
        bytes[t.off as usize + 3] ^= 0x40;
        std::fs::write(&path, &bytes).unwrap();
        let damaged = LogFile::open(&path).unwrap();
        assert!(damaged.load(1, &mut whole).is_err());
        damaged.load(0, &mut whole).unwrap();
        let mut rng = crate::testkit::Rng::new(9);
        let good = std::fs::read(&path).unwrap();
        for _ in 0..300 {
            let mut bad = good.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] ^= 1 << rng.below(8);
            std::fs::write(&path, &bad).unwrap();
            if let Ok(f) = LogFile::open(&path) {
                for i in 0..f.blocks.len() {
                    let _ = f.load(i, &mut whole);
                }
            }
        }
    }
}
