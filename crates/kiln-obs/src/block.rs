//! Blocks: immutable files of a time range's samples (`docs/observe.md` §4.5).
//!
//! ```text
//! block   := MAGIC 8 | FORMAT u32-le | data | index | footer
//! data    := each series' region and its rollups, series after series by fingerprint
//! region  := table length u32-le | crc32c of the table u32-le | table | the chunks' bytes, in time order
//! table   := chunks varint | per chunk: first zigzag | last−first varint | count varint | length varint
//!             | crc32c of its bytes u32-le
//! rollups := crc32c of its rollups u32-le | its rollups (`crate::rollup`), right after its region
//! index   := series varint | per series, by fingerprint:
//!             fp u64-le | kind u8 | key varint-length bytes | region offset varint | region length varint
//!             | region crc32c u32-le | first zigzag | last−first varint | rollups length varint (0: none)
//! footer  := index offset u64-le | index length u64-le | min ts i64-le | max ts i64-le
//!           | crc32c of the index and the footer's first 32 bytes u32-le | TAIL 4              (40 bytes)
//! ```
//!
//! Format 2 is format 3 without rollups (and without their length in the index), a region's table
//! after its length as a varint, without a checksum, and its chunks without theirs: still read.
//!
//! A region's table and each chunk check on their own, so a read of a part of a long region (a
//! day of a month's block) reads its table and the chunks it wants, not the rest ([`READ_WHOLE`]).
//!
//! An open block holds, of each series, its fingerprint and where its region is (some 40 bytes),
//! nothing a chunk or a key: what it holds in memory grows with its series, not its samples, so a
//! store keeps a year without keeping a year's index in memory (Prometheus's blocks and
//! ClickHouse's parts read their marks the same way, from disk, as a read needs them). A series'
//! region, its chunk table and its chunks, is read with one positioned read and checked against its
//! own CRC-32C, so damage anywhere is found when it is read, and nothing past it is trusted. Keys
//! are read from the index when they are wanted: when a store opens, and when blocks merge.
//!
//! A block is written as it goes ([`Writer`]): a region at a time, its index (small: a series'
//! entry) kept until the end. A merge copies a region another block holds whole when nothing of it
//! changes ([`Writer::region`]): no sample decoded, none encoded.

use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};

use crate::batch::{Kind, fingerprint};
use crate::crc32c;
use crate::gorilla::{ChunkRef, DecodeError};
use crate::varint;

pub const MAGIC: [u8; 8] = *b"KOBLOCK\n";
pub const FORMAT: u32 = 3;
/// The oldest format this build reads.
const OLDEST: u32 = 2;
const TAIL: [u8; 4] = *b"KOB1";
const FOOTER: usize = 40;
const HEADER: usize = 12;
/// A region's table length and checksum, before its table (format 3).
const REGION_HEAD: usize = 8;
/// A region this long or shorter is read whole, checked by its checksum, not its table and then
/// its chunks: a page, which one read costs as a few bytes do. Measured (`docs/observe.md` §12.16)
/// against 1, 16 and 64 KiB: a day's query over month-long blocks 13% slower at 1 KiB; a month's
/// rolled up 3% slower at 16 KiB and 30% at 64.
pub const READ_WHOLE: usize = 4 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}: not a block")]
    NotABlock(PathBuf),
    #[error("{0}: block format {1} (this build reads {OLDEST} to {FORMAT})")]
    Format(PathBuf, u32),
    #[error("{0}: damaged: {1}")]
    Damaged(PathBuf, &'static str),
    #[error("{0}: a chunk does not decode: {1}")]
    Decode(PathBuf, DecodeError),
    #[error("{0}: series written out of fingerprint order")]
    Order(PathBuf),
}

/// What an open block holds of a series.
#[derive(Debug, Clone, Copy)]
struct SeriesMeta {
    kind: Kind,
    data_off: u64,
    data_len: u32,
    data_crc: u32,
    /// Its rollups' bytes, their checksum's among them; 0, none.
    rollups_len: u32,
    first: i64,
    last: i64,
}

/// An open block.
#[derive(Debug)]
pub struct Block {
    path: PathBuf,
    file: File,
    pub min_ts: i64,
    pub max_ts: i64,
    pub bytes: u64,
    /// The footer's checksum: with `bytes`, what names this block's contents.
    pub crc: u32,
    index_off: u64,
    index_len: u64,
    format: u32,
    fps: Vec<u64>,
    series: Vec<SeriesMeta>,
}

/// A series to write: its identity and its chunks, in time order and not overlapping.
pub struct SeriesData<'a> {
    pub fp: u64,
    pub kind: Kind,
    pub key: &'a [u8],
    /// `(first, last, count, bytes)` each.
    pub chunks: Vec<(i64, i64, u32, &'a [u8])>,
    /// Its rollups (`crate::rollup`); empty, none.
    pub rollups: &'a [u8],
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> BlockError + '_ {
    move |source| BlockError::Io { path: path.to_owned(), source }
}

/// Writes a block of `series` (any order: it is sorted by fingerprint here) to `path`, durably.
/// Series with no chunk are left out.
pub fn write(path: &Path, mut series: Vec<SeriesData<'_>>) -> Result<Block, BlockError> {
    series.retain(|s| !s.chunks.is_empty());
    series.sort_unstable_by_key(|s| s.fp);
    let mut w = Writer::create(path)?;
    for s in &series {
        w.series(s.fp, s.kind, s.key, &s.chunks, s.rollups)?;
    }
    w.finish()
}

/// A block being written: regions as they come (by fingerprint), the index kept until
/// [`Writer::finish`] writes it and the footer, syncs the file and opens it.
pub struct Writer {
    path: PathBuf,
    file: BufWriter<File>,
    at: u64,
    index: Vec<u8>,
    count: u64,
    min_ts: i64,
    max_ts: i64,
    last_fp: Option<u64>,
    /// Reused: a region's chunk table, and its length before it.
    table: Vec<u8>,
    head: Vec<u8>,
    format: u32,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self, BlockError> {
        Self::create_format(path, FORMAT)
    }

    /// A block of format 2, as the build before rollups wrote it: what a test of reading one
    /// writes.
    #[cfg(test)]
    fn create_format_2(path: &Path) -> Result<Self, BlockError> {
        Self::create_format(path, 2)
    }

    fn create_format(path: &Path, format: u32) -> Result<Self, BlockError> {
        let file = File::create(path).map_err(io_err(path))?;
        let mut file = BufWriter::with_capacity(1 << 20, file);
        file.write_all(&MAGIC).map_err(io_err(path))?;
        file.write_all(&format.to_le_bytes()).map_err(io_err(path))?;
        Ok(Self { path: path.to_owned(), file, at: HEADER as u64, index: Vec::new(), count: 0, min_ts: i64::MAX, max_ts: i64::MIN, last_fp: None, table: Vec::new(), head: Vec::new(), format })
    }

    /// A series, its chunks in time order (`(first, last, count, bytes)` each) and its rollups
    /// (empty, none); no chunk, nothing.
    pub fn series(&mut self, fp: u64, kind: Kind, key: &[u8], chunks: &[(i64, i64, u32, &[u8])], rollups: &[u8]) -> Result<(), BlockError> {
        let (Some(first), Some(last)) = (chunks.first(), chunks.last()) else { return Ok(()) };
        let (first, last) = (first.0, last.1);
        self.table.clear();
        varint::put(&mut self.table, chunks.len() as u64);
        for c in chunks {
            varint::put(&mut self.table, varint::zig(c.0));
            varint::put(&mut self.table, (c.1 as u64).wrapping_sub(c.0 as u64));
            varint::put(&mut self.table, u64::from(c.2));
            varint::put(&mut self.table, c.3.len() as u64);
            if self.format >= 3 {
                self.table.extend_from_slice(&crc32c::checksum(c.3).to_le_bytes());
            }
        }
        self.head.clear();
        if self.format >= 3 {
            let table_len = u32::try_from(self.table.len()).map_err(|_| BlockError::Damaged(self.path.clone(), "a chunk table past 4 GiB"))?;
            self.head.extend_from_slice(&table_len.to_le_bytes());
            self.head.extend_from_slice(&crc32c::checksum(&self.table).to_le_bytes());
        } else {
            varint::put(&mut self.head, self.table.len() as u64);
        }
        let mut crc = crc32c::update(crc32c::checksum(&self.head), &self.table);
        let mut len = (self.head.len() + self.table.len()) as u64;
        for c in chunks {
            crc = crc32c::update(crc, c.3);
            len += c.3.len() as u64;
        }
        self.entry(fp, kind, key, len, crc, first, last, rollups)?;
        let path = &self.path;
        self.file.write_all(&self.head).map_err(io_err(path))?;
        self.file.write_all(&self.table).map_err(io_err(path))?;
        for c in chunks {
            self.file.write_all(c.3).map_err(io_err(path))?;
        }
        self.at += len;
        self.rollups(rollups)
    }

    /// A series' region as another block holds it (`region`, its checksum `crc`, its span), copied
    /// whole, and its rollups: what a merge does of a series nothing changes of.
    #[allow(clippy::too_many_arguments)]
    pub fn region(&mut self, fp: u64, kind: Kind, key: &[u8], region: &[u8], crc: u32, span: (i64, i64), rollups: &[u8]) -> Result<(), BlockError> {
        self.entry(fp, kind, key, region.len() as u64, crc, span.0, span.1, rollups)?;
        self.file.write_all(region).map_err(io_err(&self.path))?;
        self.at += region.len() as u64;
        self.rollups(rollups)
    }

    fn rollups(&mut self, rollups: &[u8]) -> Result<(), BlockError> {
        if rollups.is_empty() || self.format < 3 {
            return Ok(());
        }
        let path = &self.path;
        self.file.write_all(&crc32c::checksum(rollups).to_le_bytes()).map_err(io_err(path))?;
        self.file.write_all(rollups).map_err(io_err(path))?;
        self.at += 4 + rollups.len() as u64;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(&mut self, fp: u64, kind: Kind, key: &[u8], len: u64, crc: u32, first: i64, last: i64, rollups: &[u8]) -> Result<(), BlockError> {
        if self.last_fp.is_some_and(|l| l >= fp) {
            return Err(BlockError::Order(self.path.clone()));
        }
        self.last_fp = Some(fp);
        self.index.extend_from_slice(&fp.to_le_bytes());
        self.index.push(kind as u8);
        varint::put(&mut self.index, key.len() as u64);
        self.index.extend_from_slice(key);
        varint::put(&mut self.index, self.at);
        varint::put(&mut self.index, len);
        self.index.extend_from_slice(&crc.to_le_bytes());
        varint::put(&mut self.index, varint::zig(first));
        varint::put(&mut self.index, (last as u64).wrapping_sub(first as u64));
        if self.format >= 3 {
            varint::put(&mut self.index, if rollups.is_empty() { 0 } else { 4 + rollups.len() as u64 });
        }
        self.count += 1;
        self.min_ts = self.min_ts.min(first);
        self.max_ts = self.max_ts.max(last);
        Ok(())
    }

    /// The index and the footer written, the file synced, the block opened.
    pub fn finish(mut self) -> Result<Block, BlockError> {
        let mut index = Vec::with_capacity(self.index.len() + 10);
        varint::put(&mut index, self.count);
        index.extend_from_slice(&self.index);
        let index_off = self.at;
        let mut foot = Vec::with_capacity(FOOTER);
        foot.extend_from_slice(&index_off.to_le_bytes());
        foot.extend_from_slice(&(index.len() as u64).to_le_bytes());
        foot.extend_from_slice(&self.min_ts.to_le_bytes());
        foot.extend_from_slice(&self.max_ts.to_le_bytes());
        let crc = crc32c::update(crc32c::checksum(&index), &foot);
        foot.extend_from_slice(&crc.to_le_bytes());
        foot.extend_from_slice(&TAIL);
        let path = self.path.clone();
        self.file.write_all(&index).map_err(io_err(&path))?;
        self.file.write_all(&foot).map_err(io_err(&path))?;
        let f = self.file.into_inner().map_err(|e| io_err(&path)(e.into_error()))?;
        f.sync_all().map_err(io_err(&path))?;
        drop(f);
        Block::open(&path)
    }
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
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "block ended early"));
        }
        done += n;
    }
    Ok(())
}

/// A region's chunks overlapping a range ([`Block::chunks_in`]), its table read as they are asked
/// for; every chunk's bytes checked to lie in the region.
pub struct RegionChunks<'b> {
    path: &'b Path,
    table: &'b [u8],
    left: usize,
    chunks: &'b [u8],
    off: usize,
    from: i64,
    to: i64,
    /// Whether each entry ends in its chunk's checksum (format 3).
    crcs: bool,
}

impl<'b> Iterator for RegionChunks<'b> {
    /// A chunk: its first and last time, its samples, its bytes.
    type Item = Result<(i64, i64, u32, &'b [u8]), BlockError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.left > 0 {
            self.left -= 1;
            let damaged = || BlockError::Damaged(self.path.to_owned(), "a series' chunk table");
            let mut get = || varint::get(&mut self.table);
            let (Some(first), Some(span), Some(count), Some(len)) = (get(), get(), get(), get()) else { return Some(Err(damaged())) };
            if self.crcs {
                let Some((_, rest)) = self.table.split_first_chunk::<4>() else { return Some(Err(damaged())) };
                self.table = rest;
            }
            let first = varint::zag(first);
            let last = (first as u64).wrapping_add(span) as i64;
            let (Ok(count), Ok(len)) = (u32::try_from(count), usize::try_from(len)) else { return Some(Err(damaged())) };
            let Some(bytes) = self.off.checked_add(len).and_then(|end| self.chunks.get(self.off..end)) else { return Some(Err(damaged())) };
            self.off += len;
            if last >= self.from && first <= self.to {
                return Some(Ok((first, last, count, bytes)));
            }
        }
        None
    }
}

/// A table entry of format 3 ([`Entries`]).
#[derive(Debug, Clone, Copy)]
struct Entry {
    first: i64,
    last: i64,
    count: u32,
    len: usize,
    crc: u32,
}

/// A format 3 table's entries, in order; `None` where it is damaged.
struct Entries<'t> {
    at: &'t [u8],
    left: u64,
}

impl<'t> Entries<'t> {
    fn new(mut table: &'t [u8]) -> Self {
        let left = varint::get(&mut table).unwrap_or(0);
        Self { at: table, left }
    }
}

impl Iterator for Entries<'_> {
    type Item = Option<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let entry = self.entry();
        if entry.is_none() {
            self.left = 0;
        }
        Some(entry)
    }
}

impl Entries<'_> {
    fn entry(&mut self) -> Option<Entry> {
        let first = varint::zag(varint::get(&mut self.at)?);
        let last = (first as u64).wrapping_add(varint::get(&mut self.at)?) as i64;
        let count = u32::try_from(varint::get(&mut self.at)?).ok()?;
        let len = usize::try_from(varint::get(&mut self.at)?).ok()?;
        let (crc, rest) = self.at.split_first_chunk::<4>()?;
        self.at = rest;
        Some(Entry { first, last, count, len, crc: u32::from_le_bytes(*crc) })
    }
}

/// A block's index, read from disk for its keys ([`Block::keys`]).
pub struct Keys {
    path: PathBuf,
    index: Vec<u8>,
    format: u32,
}

impl Keys {
    /// Every series: fingerprint, kind, canonical key, in fingerprint order; each key checked
    /// against its fingerprint.
    pub fn iter(&self) -> impl Iterator<Item = Result<(u64, Kind, &[u8]), BlockError>> + '_ {
        let mut at = self.index.as_slice();
        let n = varint::get(&mut at).unwrap_or(0);
        (0..n).map_while(move |_| {
            let entry = (|| {
                let (fp, rest) = at.split_first_chunk::<8>()?;
                let (&kind, rest) = rest.split_first()?;
                at = rest;
                let key_len = usize::try_from(varint::get(&mut at)?).ok()?;
                let (key, rest) = at.split_at_checked(key_len)?;
                at = rest;
                varint::get(&mut at)?;
                varint::get(&mut at)?;
                let (_, rest) = at.split_first_chunk::<4>()?;
                at = rest;
                varint::get(&mut at)?;
                varint::get(&mut at)?;
                if self.format >= 3 {
                    varint::get(&mut at)?;
                }
                Some((u64::from_le_bytes(*fp), Kind::from_u8(kind)?, key))
            })();
            Some(match entry {
                Some((fp, kind, key)) if fingerprint(key) == fp => Ok((fp, kind, key)),
                Some(_) => Err(BlockError::Damaged(self.path.clone(), "a key is not its fingerprint's")),
                None => Err(BlockError::Damaged(self.path.clone(), "index")),
            })
        })
    }
}

impl Block {
    /// Opens a block: its footer and index, checked; of the index, each series' place kept.
    pub fn open(path: &Path) -> Result<Self, BlockError> {
        let file = File::open(path).map_err(io_err(path))?;
        let bytes = file.metadata().map_err(io_err(path))?.len();
        let damaged = |why| BlockError::Damaged(path.to_owned(), why);
        if bytes < (HEADER + FOOTER) as u64 {
            return Err(BlockError::NotABlock(path.to_owned()));
        }
        let mut head = [0u8; HEADER];
        read_at(&file, &mut head, 0).map_err(io_err(path))?;
        if head.get(..8) != Some(&MAGIC[..]) {
            return Err(BlockError::NotABlock(path.to_owned()));
        }
        let format = u32::from_le_bytes(head.get(8..12).and_then(|b| b.try_into().ok()).ok_or_else(|| damaged("header"))?);
        if !(OLDEST..=FORMAT).contains(&format) {
            return Err(BlockError::Format(path.to_owned(), format));
        }
        let mut foot = [0u8; FOOTER];
        read_at(&file, &mut foot, bytes - FOOTER as u64).map_err(io_err(path))?;
        let word = |at: usize| -> Result<[u8; 8], BlockError> { foot.get(at..at + 8).and_then(|b| b.try_into().ok()).ok_or_else(|| damaged("footer")) };
        if foot.get(36..40) != Some(&TAIL[..]) {
            return Err(damaged("footer"));
        }
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
        let mut b = Self { path: path.to_owned(), file, min_ts, max_ts, bytes, crc, index_off, index_len, format, fps: Vec::new(), series: Vec::new() };
        b.parse_index(&index, index_off).ok_or_else(|| damaged("index"))?;
        Ok(b)
    }

    fn parse_index(&mut self, index: &[u8], data_end: u64) -> Option<()> {
        let mut at = index;
        let n = usize::try_from(varint::get(&mut at)?).ok()?;
        // Each series takes at least 18 bytes of index: a count beyond that is damage.
        if n > index.len() / 18 + 1 {
            return None;
        }
        self.fps.reserve_exact(n);
        self.series.reserve_exact(n);
        let mut last_fp = None;
        for _ in 0..n {
            let (fp, rest) = at.split_first_chunk::<8>()?;
            let fp = u64::from_le_bytes(*fp);
            if last_fp.is_some_and(|l| l >= fp) {
                return None;
            }
            last_fp = Some(fp);
            let (&kind, rest) = rest.split_first()?;
            at = rest;
            let kind = Kind::from_u8(kind)?;
            let key_len = usize::try_from(varint::get(&mut at)?).ok()?;
            let (_, rest) = at.split_at_checked(key_len)?;
            at = rest;
            let data_off = varint::get(&mut at)?;
            let data_len = u32::try_from(varint::get(&mut at)?).ok()?;
            let (crc, rest) = at.split_first_chunk::<4>()?;
            at = rest;
            let first = varint::zag(varint::get(&mut at)?);
            let last = (first as u64).wrapping_add(varint::get(&mut at)?) as i64;
            let rollups_len = if self.format >= 3 { u32::try_from(varint::get(&mut at)?).ok()? } else { 0 };
            if data_off < HEADER as u64 || data_off.checked_add(u64::from(data_len))?.checked_add(u64::from(rollups_len))? > data_end || (1..5).contains(&rollups_len) {
                return None;
            }
            self.fps.push(fp);
            self.series.push(SeriesMeta { kind, data_off, data_len, data_crc: u32::from_le_bytes(*crc), rollups_len, first, last });
        }
        at.is_empty().then_some(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.fps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fps.is_empty()
    }

    /// The block's index of series `fp`.
    pub fn find(&self, fp: u64) -> Option<usize> {
        self.fps.binary_search(&fp).ok()
    }

    /// The block's keys, read from its index (checked again against the index's checksum).
    pub fn keys(&self) -> Result<Keys, BlockError> {
        let mut index = vec![0u8; usize::try_from(self.index_len).map_err(|_| BlockError::Damaged(self.path.clone(), "index length"))?];
        read_at(&self.file, &mut index, self.index_off).map_err(io_err(&self.path))?;
        let mut foot = [0u8; 32];
        read_at(&self.file, &mut foot, self.bytes - FOOTER as u64).map_err(io_err(&self.path))?;
        if crc32c::update(crc32c::checksum(&index), &foot) != self.crc {
            return Err(BlockError::Damaged(self.path.clone(), "index checksum"));
        }
        Ok(Keys { path: self.path.clone(), index, format: self.format })
    }

    /// The time span of series `i`'s data here.
    pub fn span(&self, i: usize) -> Option<(i64, i64)> {
        self.series.get(i).map(|s| (s.first, s.last))
    }

    /// Reads series `i`'s region (its chunk table and chunks) into `buf` (reused across calls),
    /// checked; its checksum.
    pub fn load(&self, i: usize, buf: &mut Vec<u8>) -> Result<u32, BlockError> {
        let s = self.series.get(i).ok_or_else(|| BlockError::Damaged(self.path.clone(), "series index"))?;
        buf.clear();
        buf.resize(s.data_len as usize, 0);
        read_at(&self.file, buf, s.data_off).map_err(io_err(&self.path))?;
        if crc32c::checksum(buf) != s.data_crc {
            return Err(BlockError::Damaged(self.path.clone(), "series data checksum"));
        }
        Ok(s.data_crc)
    }

    /// Reads series `i`'s rollups into `buf` (reused across calls), checked: what follows their
    /// checksum, empty when it has none (a block a cut wrote, or of format 2).
    pub fn rollups<'b>(&self, i: usize, buf: &'b mut Vec<u8>) -> Result<&'b [u8], BlockError> {
        let s = self.series.get(i).ok_or_else(|| BlockError::Damaged(self.path.clone(), "series index"))?;
        buf.clear();
        if s.rollups_len == 0 {
            return Ok(&[]);
        }
        buf.resize(s.rollups_len as usize, 0);
        read_at(&self.file, buf, s.data_off + u64::from(s.data_len)).map_err(io_err(&self.path))?;
        let damaged = || BlockError::Damaged(self.path.clone(), "series rollups checksum");
        let (crc, body) = buf.split_first_chunk::<4>().ok_or_else(damaged)?;
        if crc32c::checksum(body) != u32::from_le_bytes(*crc) {
            return Err(damaged());
        }
        Ok(body)
    }

    /// Series `i`'s chunks overlapping `[from, to]`, from its region as [`Block::load`] read it,
    /// read from its chunk table as they are asked for: nothing allocated.
    pub fn chunks_in<'b>(&'b self, i: usize, data: &'b [u8], from: i64, to: i64) -> Result<RegionChunks<'b>, BlockError> {
        let _ = i;
        let damaged = || BlockError::Damaged(self.path.clone(), "a series' chunk table");
        let mut at = data;
        let table_len = if self.format >= 3 {
            let (len, rest) = at.split_first_chunk::<4>().ok_or_else(damaged)?;
            let (_, rest) = rest.split_first_chunk::<4>().ok_or_else(damaged)?;
            at = rest;
            u32::from_le_bytes(*len) as usize
        } else {
            varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or_else(damaged)?
        };
        let (mut table, chunks) = at.split_at_checked(table_len).ok_or_else(damaged)?;
        let left = varint::get(&mut table).and_then(|n| usize::try_from(n).ok()).filter(|&n| n <= table_len).ok_or_else(damaged)?;
        Ok(RegionChunks { path: &self.path, table, left, chunks, off: 0, from, to, crcs: self.format >= 3 })
    }

    /// Appends series `i`'s samples in each of `ranges` (`[from, to]` each, in time order and apart),
    /// in time order. A region of format 3 longer than [`READ_WHOLE`] that is wanted in part is
    /// read in parts: its table, checked by its own checksum, and then the run of chunks each range
    /// overlaps, each chunk checked by its own; else the region is read whole, checked by its.
    pub fn read_ranges(&self, i: usize, ranges: &[(i64, i64)], buf: &mut Vec<u8>, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), BlockError> {
        let s = *self.series.get(i).ok_or_else(|| BlockError::Damaged(self.path.clone(), "series index"))?;
        let touched = |&&(a, b): &&(i64, i64)| s.last >= a && s.first <= b;
        let (Some(lo), Some(hi)) = (ranges.iter().find(touched), ranges.iter().rev().find(touched)) else { return Ok(()) };
        let whole_span = ranges.len() == 1 && lo.0 <= s.first && hi.1 >= s.last;
        if self.format < 3 || s.data_len as usize <= READ_WHOLE || whole_span {
            self.load(i, buf)?;
            for &(a, b) in ranges.iter().filter(|r| touched(r)) {
                for c in self.chunks_in(i, buf, a, b)? {
                    let (first, last, count, bytes) = c?;
                    ChunkRef::new(bytes, count).decode_range_into((first, last), a, b, ts, values).map_err(|e| BlockError::Decode(self.path.clone(), e))?;
                }
            }
            return Ok(());
        }
        let damaged = |why| BlockError::Damaged(self.path.clone(), why);
        // The table: its length and checksum first, read with a guess at its length (a chunk's
        // entry is some ten bytes of a chunk's few hundred), the rest if the guess fell short.
        let len = s.data_len as usize;
        buf.clear();
        buf.resize((REGION_HEAD + 64 + len / 16).min(len), 0);
        read_at(&self.file, buf, s.data_off).map_err(io_err(&self.path))?;
        let (table_len, table_crc) = match buf.split_first_chunk::<4>().and_then(|(l, rest)| Some((l, rest.split_first_chunk::<4>()?.0))) {
            Some((l, c)) => (u32::from_le_bytes(*l) as usize, u32::from_le_bytes(*c)),
            None => return Err(damaged("a series' chunk table")),
        };
        let table_end = REGION_HEAD.checked_add(table_len).filter(|&e| e <= len).ok_or_else(|| damaged("a series' chunk table"))?;
        let had = buf.len();
        if table_end > had {
            buf.resize(table_end, 0);
            let rest = buf.get_mut(had..).ok_or_else(|| damaged("a series' chunk table"))?;
            read_at(&self.file, rest, s.data_off + had as u64).map_err(io_err(&self.path))?;
        }
        buf.truncate(table_end);
        let table = buf.get(REGION_HEAD..).ok_or_else(|| damaged("a series' chunk table"))?;
        if crc32c::checksum(table) != table_crc {
            return Err(damaged("a series' chunk table checksum"));
        }
        let chunks_len = len - table_end;
        for &(a, b) in ranges.iter().filter(|r| touched(r)) {
            // Where the run of chunks the range overlaps lies among the chunks, then the run.
            let (mut run, mut off) = (None::<(usize, usize)>, 0usize);
            for e in Entries::new(buf.get(REGION_HEAD..table_end).unwrap_or(&[])) {
                let e = e.ok_or_else(|| damaged("a series' chunk table"))?;
                if e.last >= a && e.first <= b {
                    run = Some((run.map_or(off, |r| r.0), off + e.len));
                }
                off += e.len;
                if e.first > b {
                    break;
                }
            }
            let Some((start, end)) = run else { continue };
            if end > chunks_len {
                return Err(damaged("a series' chunk table"));
            }
            let at = buf.len();
            buf.resize(at + (end - start), 0);
            let into = buf.get_mut(at..).ok_or_else(|| damaged("a series' chunk table"))?;
            read_at(&self.file, into, s.data_off + (table_end + start) as u64).map_err(io_err(&self.path))?;
            let (head, got) = buf.split_at(at);
            let mut off = 0usize;
            for e in Entries::new(head.get(REGION_HEAD..table_end).unwrap_or(&[])) {
                let e = e.ok_or_else(|| damaged("a series' chunk table"))?;
                let (lo_off, hi_off) = (off, off + e.len);
                off = hi_off;
                if lo_off < start {
                    continue;
                }
                if hi_off > end {
                    break;
                }
                let bytes = got.get(lo_off - start..hi_off - start).ok_or_else(|| damaged("a series' chunk table"))?;
                if e.last < a || e.first > b {
                    continue;
                }
                if crc32c::checksum(bytes) != e.crc {
                    return Err(damaged("a chunk's checksum"));
                }
                ChunkRef::new(bytes, e.count).decode_range_into((e.first, e.last), a, b, ts, values).map_err(|e| BlockError::Decode(self.path.clone(), e))?;
            }
            buf.truncate(at);
        }
        Ok(())
    }

    /// Appends series `i`'s samples in `[from, to]`, in time order ([`Block::read_ranges`]).
    pub fn read(&self, i: usize, from: i64, to: i64, buf: &mut Vec<u8>, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), BlockError> {
        let Some((f, l)) = self.span(i) else { return Ok(()) };
        if l < from || f > to {
            return Ok(());
        }
        self.read_ranges(i, &[(from, to)], buf, ts, values)
    }

    /// The format it was written in.
    pub fn format(&self) -> u32 {
        self.format
    }

    /// A region of format 3, read whole: its table against its checksum, each chunk against its,
    /// and the chunks filling the region.
    fn check_region(&self, region: &[u8]) -> Result<(), BlockError> {
        let damaged = |why| BlockError::Damaged(self.path.clone(), why);
        let (len, rest) = region.split_first_chunk::<4>().ok_or_else(|| damaged("a series' chunk table"))?;
        let (crc, rest) = rest.split_first_chunk::<4>().ok_or_else(|| damaged("a series' chunk table"))?;
        let (table, mut chunks) = rest.split_at_checked(u32::from_le_bytes(*len) as usize).ok_or_else(|| damaged("a series' chunk table"))?;
        if crc32c::checksum(table) != u32::from_le_bytes(*crc) {
            return Err(damaged("a series' chunk table checksum"));
        }
        for e in Entries::new(table) {
            let e = e.ok_or_else(|| damaged("a series' chunk table"))?;
            let (bytes, rest) = chunks.split_at_checked(e.len).ok_or_else(|| damaged("a series' chunk table"))?;
            if crc32c::checksum(bytes) != e.crc {
                return Err(damaged("a chunk's checksum"));
            }
            chunks = rest;
        }
        if chunks.is_empty() { Ok(()) } else { Err(damaged("a series' chunk table")) }
    }

    pub fn kind(&self, i: usize) -> Option<Kind> {
        self.series.get(i).map(|s| s.kind)
    }

    /// Reads every series' region whole, checked against its checksum and decoded, its rollups
    /// against what its samples make of each window (`crate::rollup::check`), and every key against
    /// its fingerprint: what a block fetched from another replica is put through before it is
    /// taken. A replica's rollups are what keeps its answers its peers' (`crate::rollup`), so they
    /// are recomputed, not trusted.
    pub fn verify(&self) -> Result<(), BlockError> {
        let keys = self.keys()?;
        let mut n = 0usize;
        for k in keys.iter() {
            k?;
            n += 1;
        }
        if n != self.len() {
            return Err(BlockError::Damaged(self.path.clone(), "index"));
        }
        let (mut buf, mut rbuf, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in 0..self.len() {
            ts.clear();
            vs.clear();
            if self.format >= 3 {
                self.load(i, &mut buf)?;
                self.check_region(&buf)?;
            }
            self.read(i, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs)?;
            if ts.is_empty() {
                return Err(BlockError::Damaged(self.path.clone(), "a series without samples"));
            }
            let kind = self.kind(i).ok_or_else(|| BlockError::Damaged(self.path.clone(), "series index"))?;
            let rollups = self.rollups(i, &mut rbuf)?;
            crate::rollup::check(kind, &ts, &vs, rollups).map_err(|_| BlockError::Damaged(self.path.clone(), "a series' rollups are not its samples'"))?;
        }
        Ok(())
    }

    /// Bytes its series' rollups take, their checksums among them.
    pub fn rollups_bytes(&self) -> u64 {
        self.series.iter().map(|s| u64::from(s.rollups_len)).sum()
    }

    /// Bytes the open block holds in memory: a place a series.
    pub fn heap_bytes(&self) -> usize {
        self.fps.capacity() * 8 + self.series.capacity() * std::mem::size_of::<SeriesMeta>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::SeriesKey;
    use crate::gorilla::Encoder;
    use crate::testkit::Rng;

    fn chunks_of(samples: &[(i64, f64)], per: usize) -> Vec<(i64, i64, u32, Vec<u8>)> {
        samples
            .chunks(per)
            .map(|run| {
                let mut e = Encoder::new();
                for &(t, v) in run {
                    e.push(t, v).unwrap();
                }
                let c = e.finish();
                (c.first, c.last, c.count, c.bytes)
            })
            .collect()
    }

    #[test]
    fn a_block_reads_back_every_series_and_range_and_finds_damage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b1");
        let mut rng = Rng::new(5);
        let keys: Vec<SeriesKey> = (0..50).map(|i| SeriesKey::new("m", [("host", format!("h{i}").as_str())], Kind::Gauge).unwrap()).collect();
        let data: Vec<Vec<(i64, f64)>> = (0..50).map(|i| (0..(i * 37 + 1)).map(|j| (j as i64 * 1000 + i as i64, rng.f64())).collect()).collect();
        let chunked: Vec<Vec<(i64, i64, u32, Vec<u8>)>> = data.iter().map(|d| chunks_of(d, 100)).collect();
        let series: Vec<SeriesData<'_>> = keys.iter().zip(&chunked).map(|(k, c)| SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect(), rollups: &[] }).collect();
        let b = write(&path, series).unwrap();
        assert_eq!(b.len(), 50);
        let again = Block::open(&path).unwrap();
        let (mut buf, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new());
        for (k, d) in keys.iter().zip(&data) {
            let i = again.find(k.fp).unwrap();
            ts.clear();
            vs.clear();
            again.read(i, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs).unwrap();
            assert_eq!(ts, d.iter().map(|s| s.0).collect::<Vec<_>>());
            assert_eq!(vs.iter().map(|v| v.to_bits()).collect::<Vec<_>>(), d.iter().map(|s| s.1.to_bits()).collect::<Vec<_>>());
            // A range inside.
            if d.len() > 10 {
                let (from, to) = (d[3].0, d[d.len() - 3].0);
                ts.clear();
                vs.clear();
                again.read(i, from, to, &mut buf, &mut ts, &mut vs).unwrap();
                assert_eq!(ts.len(), d.len() - 5);
            }
        }
        let keys_read = again.keys().unwrap();
        let listed: Vec<u64> = keys_read.iter().map(|r| r.unwrap().0).collect();
        let mut fps: Vec<u64> = keys.iter().map(|k| k.fp).collect();
        fps.sort_unstable();
        assert_eq!(listed, fps);
        // Damage: a flipped bit in the data is found when that series is read, not before; in the
        // index or footer, at open.
        let mut bytes = std::fs::read(&path).unwrap();
        let i = again.find(keys[30].fp).unwrap();
        let off = again.series[i].data_off as usize;
        bytes[off + 1] ^= 0x10;
        std::fs::write(&path, &bytes).unwrap();
        let damaged = Block::open(&path).unwrap();
        assert!(matches!(damaged.read(i, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs), Err(BlockError::Damaged(_, "series data checksum"))));
        let other = damaged.find(keys[2].fp).unwrap();
        damaged.read(other, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs).unwrap();
        let n = bytes.len();
        bytes[n - 20] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        assert!(Block::open(&path).is_err());
        std::fs::write(&path, b"not a block at all, but long enough to have a footer....").unwrap();
        assert!(Block::open(&path).is_err());
    }

    /// Rollups ride beside their series' region: read back, checked by their checksum, and
    /// verified against what the samples make of them; a block of format 2 (no rollups) reads as
    /// it did.
    #[test]
    fn rollups_read_back_are_verified_against_the_samples_and_format_2_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b");
        let mut rng = Rng::new(13);
        let keys: Vec<SeriesKey> = (0..4).map(|i| SeriesKey::new("m", [("host", format!("h{i}").as_str())], Kind::Gauge).unwrap()).collect();
        let data: Vec<Vec<(i64, f64)>> = (0..4).map(|_| (0..4_000).map(|j| (j as i64 * 1_000, (rng.f64() * 8.0).floor())).collect()).collect();
        let chunked: Vec<Vec<(i64, i64, u32, Vec<u8>)>> = data.iter().map(|d| chunks_of(d, 240)).collect();
        let mut builder = crate::rollup::Builder::default();
        let rolled: Vec<Vec<u8>> = data
            .iter()
            .zip(&chunked)
            .map(|(d, c)| {
                let (ts, vs): (Vec<i64>, Vec<f64>) = d.iter().copied().unzip();
                let mut out = Vec::new();
                crate::rollup::build(Kind::Gauge, &ts, &vs, c.iter().map(|c| c.3.len()).sum(), &mut builder, &mut out);
                out
            })
            .collect();
        assert!(rolled.iter().all(|r| !r.is_empty()), "a second's samples of eight values: both tiers pay");
        fn with_rollups<'a>(keys: &'a [SeriesKey], chunked: &'a [Vec<(i64, i64, u32, Vec<u8>)>], rolled: &'a [Vec<u8>]) -> Vec<SeriesData<'a>> {
            keys.iter().zip(chunked).zip(rolled).map(|((k, c), r)| SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect(), rollups: r }).collect()
        }
        let b = write(&path, with_rollups(&keys, &chunked, &rolled)).unwrap();
        b.verify().unwrap();
        let mut buf = Vec::new();
        for (k, r) in keys.iter().zip(&rolled) {
            let i = b.find(k.fp).unwrap();
            assert_eq!(b.rollups(i, &mut buf).unwrap(), r.as_slice());
        }
        // Rollups that are not the samples' (one series' given to another): their checksum holds,
        // and verify still finds them.
        let mut swapped = rolled.clone();
        swapped.swap(0, 1);
        let b = write(&path, with_rollups(&keys, &chunked, &swapped)).unwrap();
        assert!(matches!(b.verify(), Err(BlockError::Damaged(_, "a series' rollups are not its samples'"))));
        // A flipped bit in a series' rollups: found by their checksum when they are read.
        let b = write(&path, with_rollups(&keys, &chunked, &rolled)).unwrap();
        let i = b.find(keys[2].fp).unwrap();
        let at = (b.series[i].data_off + u64::from(b.series[i].data_len)) as usize + 6;
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[at] ^= 0x04;
        std::fs::write(&path, &bytes).unwrap();
        let b = Block::open(&path).unwrap();
        assert!(matches!(b.rollups(i, &mut buf), Err(BlockError::Damaged(_, "series rollups checksum"))));
        // Format 2: no rollups, nor their length in the index; read as before.
        let mut w = Writer::create_format_2(&path).unwrap();
        let mut sorted: Vec<SeriesData<'_>> = with_rollups(&keys, &chunked, &rolled);
        sorted.sort_unstable_by_key(|s| s.fp);
        for s in &sorted {
            w.series(s.fp, s.kind, s.key, &s.chunks, s.rollups).unwrap();
        }
        let b = w.finish().unwrap();
        assert_eq!(b.format, 2);
        b.verify().unwrap();
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        for (k, d) in keys.iter().zip(&data) {
            let i = b.find(k.fp).unwrap();
            assert!(b.rollups(i, &mut buf).unwrap().is_empty());
            ts.clear();
            vs.clear();
            b.read(i, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs).unwrap();
            assert_eq!(ts.len(), d.len());
        }
        assert_eq!(b.keys().unwrap().iter().count(), keys.len());
    }

    /// A long region read in part reads what the whole would, range by range; a damaged chunk
    /// is found by a read that wants it and by verify, and not by one that does not want it.
    #[test]
    fn a_long_region_reads_in_parts_exactly_as_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b");
        let mut rng = Rng::new(17);
        let k = SeriesKey::new("m", [], Kind::Gauge).unwrap();
        let data: Vec<(i64, f64)> = (0..60_000).map(|j| (j as i64 * 10_000 + rng.below(7) as i64, rng.f64())).collect();
        let c = chunks_of(&data, 240);
        let b = write(&path, vec![SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect(), rollups: &[] }]).unwrap();
        assert!(b.series[0].data_len as usize > 20 * READ_WHOLE, "a long region: {}", b.series[0].data_len);
        b.verify().unwrap();
        let (mut buf, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new());
        let span = (data[0].0, data[data.len() - 1].0);
        for _ in 0..300 {
            let n = 1 + rng.below(3) as usize;
            let mut cuts: Vec<i64> = (0..n * 2).map(|_| span.0 - 50_000 + rng.below((span.1 - span.0 + 100_000) as u64) as i64).collect();
            cuts.sort_unstable();
            cuts.dedup();
            let ranges: Vec<(i64, i64)> = cuts.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).collect();
            ts.clear();
            vs.clear();
            b.read_ranges(0, &ranges, &mut buf, &mut ts, &mut vs).unwrap();
            let want: Vec<(i64, u64)> = data.iter().filter(|(t, _)| ranges.iter().any(|&(a, z)| *t >= a && *t <= z)).map(|&(t, v)| (t, v.to_bits())).collect();
            let got: Vec<(i64, u64)> = ts.iter().zip(&vs).map(|(&t, v)| (t, v.to_bits())).collect();
            assert_eq!(got, want, "{ranges:?}");
        }
        // A bit flipped in a chunk near the end: a read of the start does not see it, one of the end
        // does, and so does verify.
        let off = (b.series[0].data_off + u64::from(b.series[0].data_len)) as usize - 50;
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[off] ^= 0x01;
        std::fs::write(&path, &bytes).unwrap();
        let b = Block::open(&path).unwrap();
        ts.clear();
        vs.clear();
        b.read(0, span.0, span.0 + 3_600_000, &mut buf, &mut ts, &mut vs).unwrap();
        assert_eq!(ts.len(), 360);
        assert!(matches!(b.read(0, span.1 - 3_600_000, span.1, &mut buf, &mut ts, &mut vs), Err(BlockError::Damaged(_, "a chunk's checksum"))));
        assert!(b.verify().is_err());
    }

    #[test]
    fn every_truncation_and_random_damage_is_an_error_never_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b");
        let k = SeriesKey::new("m", [], Kind::Gauge).unwrap();
        let c = chunks_of(&(0..500).map(|i| (i * 10, i as f64)).collect::<Vec<_>>(), 120);
        write(&path, vec![SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect(), rollups: &[] }]).unwrap();
        let good = std::fs::read(&path).unwrap();
        let (mut buf, mut ts, mut vs) = (Vec::new(), Vec::new(), Vec::new());
        for cut in 0..good.len() {
            std::fs::write(&path, &good[..cut]).unwrap();
            if let Ok(b) = Block::open(&path) {
                let _ = b.read(0, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs);
            }
        }
        let mut rng = Rng::new(3);
        for _ in 0..500 {
            let mut bad = good.clone();
            let i = rng.below(bad.len() as u64) as usize;
            bad[i] ^= 1 << rng.below(8);
            std::fs::write(&path, &bad).unwrap();
            if let Ok(b) = Block::open(&path) {
                for s in 0..b.len() {
                    let _ = b.read(s, i64::MIN, i64::MAX, &mut buf, &mut ts, &mut vs);
                }
            }
        }
    }
}
