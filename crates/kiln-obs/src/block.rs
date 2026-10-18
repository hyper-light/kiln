//! Blocks: immutable files of a time range's samples (`docs/observe.md` §4.5).
//!
//! ```text
//! block  := MAGIC 8 | FORMAT u32-le | data | index | footer
//! data   := each series' region, contiguous, series after series by fingerprint
//! region := table length varint | table | the chunks' bytes, in time order
//! table  := chunks varint | per chunk: first zigzag | last−first varint | count varint | length varint
//! index  := series varint | per series, by fingerprint:
//!             fp u64-le | kind u8 | key varint-length bytes | region offset varint | region length varint
//!             | region crc32c u32-le | first zigzag | last−first varint
//! footer := index offset u64-le | index length u64-le | min ts i64-le | max ts i64-le
//!           | crc32c of the index and the footer's first 32 bytes u32-le | TAIL 4              (40 bytes)
//! ```
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
pub const FORMAT: u32 = 2;
const TAIL: [u8; 4] = *b"KOB1";
const FOOTER: usize = 40;
const HEADER: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{0}: not a block")]
    NotABlock(PathBuf),
    #[error("{0}: block format {1} (this build reads {FORMAT})")]
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
        w.series(s.fp, s.kind, s.key, &s.chunks)?;
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
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self, BlockError> {
        let file = File::create(path).map_err(io_err(path))?;
        let mut file = BufWriter::with_capacity(1 << 20, file);
        file.write_all(&MAGIC).map_err(io_err(path))?;
        file.write_all(&FORMAT.to_le_bytes()).map_err(io_err(path))?;
        Ok(Self { path: path.to_owned(), file, at: HEADER as u64, index: Vec::new(), count: 0, min_ts: i64::MAX, max_ts: i64::MIN, last_fp: None, table: Vec::new(), head: Vec::new() })
    }

    /// A series, its chunks in time order (`(first, last, count, bytes)` each); none, nothing.
    pub fn series(&mut self, fp: u64, kind: Kind, key: &[u8], chunks: &[(i64, i64, u32, &[u8])]) -> Result<(), BlockError> {
        let (Some(first), Some(last)) = (chunks.first(), chunks.last()) else { return Ok(()) };
        let (first, last) = (first.0, last.1);
        self.table.clear();
        varint::put(&mut self.table, chunks.len() as u64);
        for c in chunks {
            varint::put(&mut self.table, varint::zig(c.0));
            varint::put(&mut self.table, (c.1 as u64).wrapping_sub(c.0 as u64));
            varint::put(&mut self.table, u64::from(c.2));
            varint::put(&mut self.table, c.3.len() as u64);
        }
        self.head.clear();
        varint::put(&mut self.head, self.table.len() as u64);
        let mut crc = crc32c::update(crc32c::checksum(&self.head), &self.table);
        let mut len = (self.head.len() + self.table.len()) as u64;
        for c in chunks {
            crc = crc32c::update(crc, c.3);
            len += c.3.len() as u64;
        }
        self.entry(fp, kind, key, len, crc, first, last)?;
        let path = &self.path;
        self.file.write_all(&self.head).map_err(io_err(path))?;
        self.file.write_all(&self.table).map_err(io_err(path))?;
        for c in chunks {
            self.file.write_all(c.3).map_err(io_err(path))?;
        }
        self.at += len;
        Ok(())
    }

    /// A series' region as another block holds it (`region`, its checksum `crc`, its span), copied
    /// whole: what a merge does of a series nothing changes of.
    pub fn region(&mut self, fp: u64, kind: Kind, key: &[u8], region: &[u8], crc: u32, span: (i64, i64)) -> Result<(), BlockError> {
        self.entry(fp, kind, key, region.len() as u64, crc, span.0, span.1)?;
        self.file.write_all(region).map_err(io_err(&self.path))?;
        self.at += region.len() as u64;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(&mut self, fp: u64, kind: Kind, key: &[u8], len: u64, crc: u32, first: i64, last: i64) -> Result<(), BlockError> {
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

/// A block's index, read from disk for its keys ([`Block::keys`]).
pub struct Keys {
    path: PathBuf,
    index: Vec<u8>,
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
        if format != FORMAT {
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
        let mut b = Self { path: path.to_owned(), file, min_ts, max_ts, bytes, crc, index_off, index_len, fps: Vec::new(), series: Vec::new() };
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
            if data_off < HEADER as u64 || data_off.checked_add(u64::from(data_len))? > data_end {
                return None;
            }
            let (crc, rest) = at.split_first_chunk::<4>()?;
            at = rest;
            let first = varint::zag(varint::get(&mut at)?);
            let last = (first as u64).wrapping_add(varint::get(&mut at)?) as i64;
            self.fps.push(fp);
            self.series.push(SeriesMeta { kind, data_off, data_len, data_crc: u32::from_le_bytes(*crc), first, last });
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
        Ok(Keys { path: self.path.clone(), index })
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

    /// Series `i`'s chunks overlapping `[from, to]`, from its region as [`Block::load`] read it,
    /// read from its chunk table as they are asked for: nothing allocated.
    pub fn chunks_in<'b>(&'b self, i: usize, data: &'b [u8], from: i64, to: i64) -> Result<RegionChunks<'b>, BlockError> {
        let _ = i;
        let damaged = || BlockError::Damaged(self.path.clone(), "a series' chunk table");
        let mut at = data;
        let table_len = varint::get(&mut at).and_then(|n| usize::try_from(n).ok()).ok_or_else(damaged)?;
        let (mut table, chunks) = at.split_at_checked(table_len).ok_or_else(damaged)?;
        let left = varint::get(&mut table).and_then(|n| usize::try_from(n).ok()).filter(|&n| n <= table_len).ok_or_else(damaged)?;
        Ok(RegionChunks { path: &self.path, table, left, chunks, off: 0, from, to })
    }

    /// Appends series `i`'s samples in `[from, to]`, in time order.
    pub fn read(&self, i: usize, from: i64, to: i64, buf: &mut Vec<u8>, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), BlockError> {
        let Some((f, l)) = self.span(i) else { return Ok(()) };
        if l < from || f > to {
            return Ok(());
        }
        self.load(i, buf)?;
        for c in self.chunks_in(i, buf, from, to)? {
            let (first, last, count, bytes) = c?;
            ChunkRef::new(bytes, count).decode_range_into((first, last), from, to, ts, values).map_err(|e| BlockError::Decode(self.path.clone(), e))?;
        }
        Ok(())
    }

    pub fn kind(&self, i: usize) -> Option<Kind> {
        self.series.get(i).map(|s| s.kind)
    }

    /// Reads every series' region whole, checked against its checksum and decoded, and every key
    /// against its fingerprint: what a block fetched from another replica is put through before it
    /// is taken.
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
        let mut buf = Vec::new();
        for i in 0..self.len() {
            self.load(i, &mut buf)?;
            let mut samples = 0u64;
            for c in self.chunks_in(i, &buf, i64::MIN, i64::MAX)? {
                let (_, _, count, bytes) = c?;
                let mut d = ChunkRef::new(bytes, count).iter();
                while d.next_sample().map_err(|e| BlockError::Decode(self.path.clone(), e))?.is_some() {
                    samples += 1;
                }
            }
            if samples == 0 {
                return Err(BlockError::Damaged(self.path.clone(), "a series without samples"));
            }
        }
        Ok(())
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
        let series: Vec<SeriesData<'_>> = keys.iter().zip(&chunked).map(|(k, c)| SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect() }).collect();
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

    #[test]
    fn every_truncation_and_random_damage_is_an_error_never_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b");
        let k = SeriesKey::new("m", [], Kind::Gauge).unwrap();
        let c = chunks_of(&(0..500).map(|i| (i * 10, i as f64)).collect::<Vec<_>>(), 120);
        write(&path, vec![SeriesData { fp: k.fp, kind: k.kind, key: &k.key, chunks: c.iter().map(|(f, l, n, b)| (*f, *l, *n, b.as_slice())).collect() }]).unwrap();
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
