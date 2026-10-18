//! Blocks: immutable files of a time range's samples (`docs/observe.md` §4.5).
//!
//! ```text
//! block  := MAGIC 8 | FORMAT u32-le | data | index | footer
//! data   := each series' chunks, contiguous, series after series
//! index  := series varint | per series, by fingerprint:
//!             fp u64-le | kind u8 | key varint-length bytes | data offset varint | data length varint
//!             | data crc32c u32-le | chunks varint | per chunk: first zigzag | last−first varint | count varint | length varint
//! footer := index offset u64-le | index length u64-le | min ts i64-le | max ts i64-le
//!           | crc32c of the index and the footer's first 32 bytes u32-le | TAIL 4              (40 bytes)
//! ```
//!
//! Opening a block reads its footer and index only, so a store opens in time proportional to its
//! series, not its samples; a series' data is read with one positioned read and checked against
//! its own CRC-32C, so damage anywhere is found when it is read, and nothing past it is trusted.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use crate::batch::{Kind, fingerprint};
use crate::crc32c;
use crate::gorilla::{ChunkRef, DecodeError};
use crate::varint;

pub const MAGIC: [u8; 8] = *b"KOBLOCK\n";
pub const FORMAT: u32 = 1;
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
}

/// A chunk as a block holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMeta {
    pub first: i64,
    pub last: i64,
    pub count: u32,
    /// Offset in its series' data.
    off: u32,
    len: u32,
}

#[derive(Debug, Clone, Copy)]
struct SeriesMeta {
    kind: Kind,
    key_at: u32,
    key_len: u32,
    data_off: u64,
    data_len: u32,
    data_crc: u32,
    chunks_at: u32,
    chunks: u32,
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
    fps: Vec<u64>,
    series: Vec<SeriesMeta>,
    keys: Vec<u8>,
    chunks: Vec<ChunkMeta>,
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

/// Writes a block of `series` (any order: it is sorted by fingerprint here) to `path`, durably:
/// the file is written whole, then synced. Series with no chunk are left out.
pub fn write(path: &Path, mut series: Vec<SeriesData<'_>>) -> Result<Block, BlockError> {
    series.retain(|s| !s.chunks.is_empty());
    series.sort_unstable_by_key(|s| s.fp);
    let mut out: Vec<u8> = Vec::with_capacity(HEADER + series.iter().map(|s| s.chunks.iter().map(|c| c.3.len()).sum::<usize>()).sum::<usize>() + series.len() * 64 + FOOTER);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    let mut index = Vec::new();
    varint::put(&mut index, series.len() as u64);
    let (mut min_ts, mut max_ts) = (i64::MAX, i64::MIN);
    for s in &series {
        let data_off = out.len() as u64;
        for c in &s.chunks {
            out.extend_from_slice(c.3);
        }
        let data = out.get(data_off as usize..).unwrap_or(&[]);
        index.extend_from_slice(&s.fp.to_le_bytes());
        index.push(s.kind as u8);
        varint::put(&mut index, s.key.len() as u64);
        index.extend_from_slice(s.key);
        varint::put(&mut index, data_off);
        varint::put(&mut index, data.len() as u64);
        index.extend_from_slice(&crc32c::checksum(data).to_le_bytes());
        varint::put(&mut index, s.chunks.len() as u64);
        for c in &s.chunks {
            varint::put(&mut index, varint::zig(c.0));
            varint::put(&mut index, (c.1 as u64).wrapping_sub(c.0 as u64));
            varint::put(&mut index, u64::from(c.2));
            varint::put(&mut index, c.3.len() as u64);
            min_ts = min_ts.min(c.0);
            max_ts = max_ts.max(c.1);
        }
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
    Block::open(path)
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

impl Block {
    /// Opens a block: its footer and index, checked.
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
        let mut b = Self { path: path.to_owned(), file, min_ts, max_ts, bytes, crc, fps: Vec::new(), series: Vec::new(), keys: Vec::new(), chunks: Vec::new() };
        b.parse_index(&index, index_off).ok_or_else(|| damaged("index"))?;
        Ok(b)
    }

    fn parse_index(&mut self, index: &[u8], data_end: u64) -> Option<()> {
        let mut at = index;
        let n = usize::try_from(varint::get(&mut at)?).ok()?;
        // Each series takes at least 16 bytes of index: a count beyond that is damage.
        if n > index.len() / 16 + 1 {
            return None;
        }
        self.fps.reserve(n);
        self.series.reserve(n);
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
            let (key, rest) = at.split_at_checked(key_len)?;
            at = rest;
            let data_off = varint::get(&mut at)?;
            let data_len = u32::try_from(varint::get(&mut at)?).ok()?;
            if data_off < HEADER as u64 || data_off.checked_add(u64::from(data_len))? > data_end {
                return None;
            }
            let (crc, rest) = at.split_first_chunk::<4>()?;
            at = rest;
            let chunks = u32::try_from(varint::get(&mut at)?).ok()?;
            let chunks_at = u32::try_from(self.chunks.len()).ok()?;
            let mut off = 0u32;
            for _ in 0..chunks {
                let first = varint::zag(varint::get(&mut at)?);
                let last = (first as u64).wrapping_add(varint::get(&mut at)?) as i64;
                let count = u32::try_from(varint::get(&mut at)?).ok()?;
                let len = u32::try_from(varint::get(&mut at)?).ok()?;
                self.chunks.push(ChunkMeta { first, last, count, off, len });
                off = off.checked_add(len)?;
            }
            if off != data_len {
                return None;
            }
            let key_at = u32::try_from(self.keys.len()).ok()?;
            self.keys.extend_from_slice(key);
            self.fps.push(fp);
            self.series.push(SeriesMeta { kind, key_at, key_len: key_len as u32, data_off, data_len, data_crc: u32::from_le_bytes(*crc), chunks_at, chunks });
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

    /// Every series: fingerprint, kind, canonical key; each key checked against its fingerprint.
    pub fn series(&self) -> impl Iterator<Item = Result<(u64, Kind, &[u8]), BlockError>> + '_ {
        self.fps.iter().zip(&self.series).map(|(&fp, s)| {
            let key = self.keys.get(s.key_at as usize..(s.key_at + s.key_len) as usize).ok_or_else(|| BlockError::Damaged(self.path.clone(), "key"))?;
            if fingerprint(key) != fp {
                return Err(BlockError::Damaged(self.path.clone(), "a key is not its fingerprint's"));
            }
            Ok((fp, s.kind, key))
        })
    }

    /// The time span of series `i`'s data here.
    pub fn span(&self, i: usize) -> Option<(i64, i64)> {
        let chunks = self.chunk_metas(i)?;
        Some((chunks.first()?.first, chunks.last()?.last))
    }

    fn chunk_metas(&self, i: usize) -> Option<&[ChunkMeta]> {
        let s = self.series.get(i)?;
        self.chunks.get(s.chunks_at as usize..(s.chunks_at + s.chunks) as usize)
    }

    /// Reads series `i`'s data into `buf` (reused across calls), checked.
    pub fn load(&self, i: usize, buf: &mut Vec<u8>) -> Result<(), BlockError> {
        let s = self.series.get(i).ok_or_else(|| BlockError::Damaged(self.path.clone(), "series index"))?;
        buf.clear();
        buf.resize(s.data_len as usize, 0);
        read_at(&self.file, buf, s.data_off).map_err(io_err(&self.path))?;
        if crc32c::checksum(buf) != s.data_crc {
            return Err(BlockError::Damaged(self.path.clone(), "series data checksum"));
        }
        Ok(())
    }

    /// Series `i`'s chunks overlapping `[from, to]`, from its data as [`Block::load`] read it.
    pub fn chunks_in<'b>(&self, i: usize, data: &'b [u8], from: i64, to: i64) -> impl Iterator<Item = ChunkRef<'b>> + use<'b, '_> {
        self.chunk_metas(i).unwrap_or(&[]).iter().filter(move |c| c.last >= from && c.first <= to).filter_map(move |c| data.get(c.off as usize..(c.off + c.len) as usize).map(|b| ChunkRef::new(b, c.count)))
    }

    /// Appends series `i`'s samples in `[from, to]`, in time order.
    pub fn read(&self, i: usize, from: i64, to: i64, buf: &mut Vec<u8>, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), BlockError> {
        let Some((f, l)) = self.span(i) else { return Ok(()) };
        if l < from || f > to {
            return Ok(());
        }
        self.load(i, buf)?;
        for c in self.chunks_in(i, buf, from, to) {
            let mut d = c.iter();
            while let Some((t, v)) = d.next_sample().map_err(|e| BlockError::Decode(self.path.clone(), e))? {
                if t > to {
                    break;
                }
                if t >= from {
                    ts.push(t);
                    values.push(v);
                }
            }
        }
        Ok(())
    }

    pub fn kind(&self, i: usize) -> Option<Kind> {
        self.series.get(i).map(|s| s.kind)
    }

    /// Reads every series' data whole, checked against its checksum and decoded: what a block
    /// fetched from another replica is put through before it is taken.
    pub fn verify(&self) -> Result<(), BlockError> {
        let mut buf = Vec::new();
        for (i, s) in self.series().enumerate() {
            s?;
            self.load(i, &mut buf)?;
            for c in self.chunks_in(i, &buf, i64::MIN, i64::MAX) {
                let mut d = c.iter();
                while d.next_sample().map_err(|e| BlockError::Decode(self.path.clone(), e))?.is_some() {}
            }
        }
        Ok(())
    }

    /// Bytes the open block holds in memory: its index.
    pub fn heap_bytes(&self) -> usize {
        self.fps.capacity() * 8 + self.series.capacity() * std::mem::size_of::<SeriesMeta>() + self.keys.capacity() + self.chunks.capacity() * std::mem::size_of::<ChunkMeta>()
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
        let listed: Vec<u64> = again.series().map(|r| r.unwrap().0).collect();
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
