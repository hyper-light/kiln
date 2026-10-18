//! One replica's store: symbols, the index, the head and the blocks on disk, fed write batches in
//! the order the group committed them, and asked for series and their samples (`docs/observe.md`
//! §4, §5).
//!
//! Applying is deterministic: replicas that apply the same batches in the same order hold the same
//! series and samples (series ids are each replica's own, assigned in the order it learns series,
//! and never leave it). A batch is applied item by item; an item that does not decode stops the
//! batch there, alike on every replica.
//!
//! Durability: the group's log is the write-ahead log. [`Store::cut`] writes the head into a block,
//! syncs it, records the log point it holds everything up to in the manifest ([`Store::durable`]),
//! and empties the head; a store opened again holds its blocks, and the log is replayed from that
//! point. Reads merge the blocks and the head, the later write winning at a time.

use std::path::{Path, PathBuf};

use crate::batch::{BatchError, Item, Kind, Reader};
use crate::block::{self, Block, BlockError, SeriesData};
use crate::gorilla::{Chunk, DecodeError, Encoder};
use crate::hash::Keyed;
use crate::head::{Appended, CHUNK_SAMPLES, HeadSeries};
use crate::index::{Defined, Index, IndexError, Selector, SeriesInfo};
use crate::logs::file::LogFile;
use crate::logs::search::Search;
use crate::logs::store::{Found, HeldFile, LogStore, LogStoreError};
use crate::manifest::{self, BlockEntry, Manifest, ManifestError, Point};
use crate::symbols::Symbols;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Batch(#[from] BatchError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("a series' samples: {0}")]
    Samples(#[from] DecodeError),
    #[error("a batch's log rows do not decompress")]
    LogRows,
    #[error("log rows: {0}")]
    Logs(String),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Block(#[from] BlockError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("a series' samples: {0}")]
    Decode(#[from] DecodeError),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("the store has no directory (it was made in memory)")]
    NoDir,
    #[error(transparent)]
    Logs(#[from] LogStoreError),
    #[error(transparent)]
    LogFile(#[from] crate::logs::file::LogFileError),
}

/// What applying batches did, counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Runs applied.
    pub runs: u64,
    pub new_series: u64,
    pub samples: u64,
    /// Late samples kept (a correction or a backfill).
    pub late: u64,
    /// Late samples identical to one held: a replayed write, dropped.
    pub duplicates: u64,
    /// Log records.
    pub logs: u64,
}

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Self) {
        self.runs += o.runs;
        self.new_series += o.new_series;
        self.samples += o.samples;
        self.late += o.late;
        self.duplicates += o.duplicates;
        self.logs += o.logs;
    }
}

/// What applying a batch did, and what its sender must act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub counts: Counts,
    /// Runs of series this replica does not know: the sender defines them again.
    pub unknown: Vec<u64>,
    /// Definitions refused: a fingerprint not its key's, or one naming another series.
    pub refused: Vec<u64>,
}

#[derive(Debug)]
struct OpenBlock {
    /// Write order: a later block's sample wins over an earlier one's at a time.
    seq: u64,
    file: String,
    block: Block,
}

#[derive(Debug)]
pub struct Store {
    hash: Keyed,
    symbols: Symbols,
    index: Index,
    head: Vec<HeadSeries>,
    blocks: Vec<OpenBlock>,
    logs: LogStore,
    dir: Option<PathBuf>,
    manifest: Manifest,
    totals: Counts,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(Keyed::new())
    }
}

/// A series, as a query sees it.
#[derive(Debug, Clone, Copy)]
pub struct SeriesRef<'a> {
    store: &'a Store,
    pub id: u32,
    info: SeriesInfo,
}

impl<'a> SeriesRef<'a> {
    pub fn metric(&self) -> &'a str {
        self.store.symbols.get(self.info.metric)
    }

    pub fn kind(&self) -> Kind {
        self.info.kind
    }

    pub fn tags(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        let s = self.store;
        let info = self.info;
        s.index.tags(&info).iter().map(move |&(k, v)| (s.symbols.get(k), s.symbols.get(v)))
    }

    /// The value of tag `key`.
    pub fn tag(&self, key: &str) -> Option<&'a str> {
        self.tags().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

/// What a cut, a compaction or a retention pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Maintained {
    pub blocks_written: u64,
    pub blocks_removed: u64,
    pub bytes_written: u64,
    pub series_dropped: u64,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io { path: path.to_owned(), source }
}

impl Store {
    /// A store in memory: it applies and answers, and cannot cut.
    pub fn new(hash: Keyed) -> Self {
        Self { hash, symbols: Symbols::new(hash), index: Index::new(hash), head: Vec::new(), blocks: Vec::new(), logs: LogStore::default(), dir: None, manifest: Manifest::default(), totals: Counts::default() }
    }

    /// Opens the store in `dir` (made if missing): its blocks, as its manifest lists them; block
    /// files the manifest does not list (a cut a crash interrupted) are deleted.
    pub fn open(dir: &Path, hash: Keyed) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(io_err(dir))?;
        let manifest = Manifest::load(dir)?.unwrap_or_default();
        let mut store = Self::new(hash);
        store.dir = Some(dir.to_owned());
        for e in &manifest.blocks {
            let block = Block::open(&dir.join(&e.file))?;
            store.blocks.push(OpenBlock { seq: e.seq, file: e.file.clone(), block });
        }
        store.blocks.sort_by_key(|b| b.seq);
        for e in &manifest.logs {
            let log = LogFile::open(&dir.join(&e.file))?;
            store.logs.files.push(HeldFile { seq: e.seq, file: e.file.clone(), log });
        }
        store.manifest = manifest;
        store.remove_orphans()?;
        store.reindex(Vec::new())?;
        Ok(store)
    }

    fn remove_orphans(&self) -> Result<(), StoreError> {
        let Some(dir) = &self.dir else { return Ok(()) };
        for entry in std::fs::read_dir(dir).map_err(io_err(dir))? {
            let entry = entry.map_err(io_err(dir))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let listed = self.manifest.blocks.iter().chain(&self.manifest.logs).any(|b| b.file == name);
            if ((name.ends_with(".block") || name.ends_with(".logs")) && !listed) || name == "MANIFEST.tmp" {
                std::fs::remove_file(entry.path()).map_err(io_err(&entry.path()))?;
            }
        }
        Ok(())
    }

    /// Builds the index again from the blocks and the head series given (`(fp, kind, key, head)`
    /// of each series that holds samples in the head).
    fn reindex(&mut self, head: Vec<(u64, Kind, Vec<u8>, HeadSeries)>) -> Result<(), StoreError> {
        let mut symbols = Symbols::new(self.hash);
        let mut index = Index::new(self.hash);
        let mut series: Vec<HeadSeries> = Vec::new();
        for b in &self.blocks {
            for s in b.block.series() {
                let (fp, kind, key) = s?;
                if let Defined::New(_) = index.define(&mut symbols, fp, key, kind)? {
                    series.push(HeadSeries::default());
                }
            }
        }
        for (fp, kind, key, h) in head {
            let id = match index.define(&mut symbols, fp, &key, kind)? {
                Defined::New(id) => {
                    series.push(HeadSeries::default());
                    id
                }
                Defined::Known(id) => id,
            };
            if let Some(slot) = series.get_mut(id as usize) {
                *slot = h;
            }
        }
        self.symbols = symbols;
        self.index = index;
        self.head = series;
        Ok(())
    }

    /// The point of the group's log everything is durable up to.
    pub fn durable(&self) -> Point {
        self.manifest.durable
    }

    /// Applies a write batch, item by item.
    pub fn apply(&mut self, batch: &[u8]) -> Result<Applied, ApplyError> {
        let mut done = Applied::default();
        let result = self.apply_items(batch, &mut done);
        self.totals += done.counts;
        result.map(|()| done)
    }

    fn apply_items(&mut self, batch: &[u8], done: &mut Applied) -> Result<(), ApplyError> {
        let mut r = Reader::new(batch)?;
        while let Some(item) = r.next_item()? {
            match item {
                Item::Def { fp, kind, key } => match self.index.define(&mut self.symbols, fp, key, kind) {
                    Ok(Defined::New(_)) => {
                        done.counts.new_series += 1;
                        self.head.push(HeadSeries::default());
                    }
                    Ok(Defined::Known(_)) => {}
                    Err(IndexError::Collision(fp) | IndexError::WrongFingerprint(fp)) => done.refused.push(fp),
                    Err(e) => return Err(e.into()),
                },
                Item::Logs { rows, raw_len, packed } => {
                    let mut raw = Vec::with_capacity(raw_len);
                    crate::lz4::decompress(packed, &mut raw, raw_len).map_err(|_| ApplyError::LogRows)?;
                    if raw.len() != raw_len {
                        return Err(ApplyError::LogRows);
                    }
                    self.logs.append(raw, rows).map_err(|e| ApplyError::Logs(e.to_string()))?;
                    done.counts.logs += u64::from(rows);
                }
                Item::Run { fp, samples } => {
                    let Some(series) = self.index.by_fp(fp).and_then(|id| self.head.get_mut(id as usize)) else {
                        done.unknown.push(fp);
                        continue;
                    };
                    done.counts.runs += 1;
                    let mut d = samples.iter();
                    while let Some((t, v)) = d.next_sample()? {
                        match series.append(t, v)? {
                            Appended::InOrder => {}
                            Appended::Late => done.counts.late += 1,
                            Appended::Duplicate => done.counts.duplicates += 1,
                        }
                        done.counts.samples += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes the head into a block, durably, and records that everything up to `at` (the log
    /// point of the last batch applied) is durable; the head is emptied. A head with no samples
    /// writes no block and still records `at`. If the block cannot be written, the head is kept.
    pub fn cut(&mut self, at: Point) -> Result<Maintained, StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        // Take every series' head; put it back if anything fails.
        let mut taken: Vec<(u32, Vec<Chunk>)> = Vec::new();
        for (id, h) in self.head.iter_mut().enumerate() {
            if h.span().is_some() {
                match h.drain_chunks() {
                    Ok(c) => taken.push((id as u32, c)),
                    Err(e) => {
                        restore(&mut self.head, taken);
                        return Err(e.into());
                    }
                }
            }
        }
        let mut done = Maintained::default();
        let mut manifest = self.manifest.clone();
        manifest.durable = at;
        // Logs first: a file of their own, listed with the blocks in the same manifest.
        let mut new_logs = None;
        if self.logs.unwritten_rows() > 0 {
            let seq = manifest.next_seq;
            let file = manifest::logs_file(seq);
            match self.logs.cut(&dir.join(&file)) {
                Ok(Some(log)) => {
                    done.bytes_written += log.bytes;
                    manifest.next_seq = seq + 1;
                    manifest.logs.push(BlockEntry { seq, file: file.clone() });
                    new_logs = Some(HeldFile { seq, file, log });
                }
                Ok(None) => {}
                Err(e) => {
                    restore(&mut self.head, taken);
                    return Err(e.into());
                }
            }
        }
        let mut new_block = None;
        if !taken.is_empty() {
            let seq = manifest.next_seq;
            let file = manifest::block_file(seq);
            let written = {
                let series: Vec<SeriesData<'_>> = taken
                    .iter()
                    .filter_map(|(id, chunks)| {
                        let info = self.index.info(*id)?;
                        Some(SeriesData { fp: self.index.fp(*id)?, kind: info.kind, key: self.index.key(*id)?, chunks: chunks.iter().map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect() })
                    })
                    .collect();
                block::write(&dir.join(&file), series)
            };
            let block = match written {
                Ok(b) => b,
                Err(e) => {
                    restore(&mut self.head, taken);
                    return Err(e.into());
                }
            };
            done.blocks_written = 1;
            done.bytes_written = block.bytes;
            manifest.next_seq = seq + 1;
            manifest.blocks.push(BlockEntry { seq, file: file.clone() });
            new_block = Some(OpenBlock { seq, file, block });
        }
        if let Err(e) = manifest::sync_dir(&dir).map_err(io_err(&dir)).and_then(|()| manifest.save(&dir).map_err(StoreError::from)) {
            restore(&mut self.head, taken);
            return Err(e);
        }
        self.manifest = manifest;
        if let Some(b) = new_block {
            self.blocks.push(b);
        }
        if let Some(l) = new_logs {
            self.logs.files.push(l);
        }
        Ok(done)
    }

    /// Compacts blocks by the logarithmic method (Bentley and Saxe, "Decomposable searching
    /// problems I: static-to-dynamic transformation", J. Algorithms 1980): the two newest blocks
    /// are merged while the newer holds at least half of what the older does, so a store keeps
    /// O(log n) blocks of n samples and a sample is rewritten O(log n) times.
    pub fn compact(&mut self) -> Result<Maintained, StoreError> {
        let mut done = Maintained::default();
        while let [.., older, newer] = self.blocks.as_slice()
            && newer.block.bytes.saturating_mul(2) >= older.block.bytes
        {
            let (a, b) = (self.blocks.len() - 2, self.blocks.len() - 1);
            let d = self.merge(a, b)?;
            done.blocks_written += d.blocks_written;
            done.blocks_removed += d.blocks_removed;
            done.bytes_written += d.bytes_written;
        }
        Ok(done)
    }

    /// Merges blocks `a` and `b` (adjacent in write order, `a` the older) into one; samples older
    /// than `keep_from` are left out.
    fn merge(&mut self, a: usize, b: usize) -> Result<Maintained, StoreError> {
        self.merge_from(a, b, i64::MIN)
    }

    fn merge_from(&mut self, a: usize, b: usize, keep_from: i64) -> Result<Maintained, StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        let (Some(older), Some(newer)) = (self.blocks.get(a), self.blocks.get(b)) else { return Ok(Maintained::default()) };
        // Every series of either, its samples merged (the newer's winning at a time), re-chunked.
        let mut keys: Vec<(u64, Kind, Vec<u8>)> = Vec::new();
        for blk in [&older.block, &newer.block] {
            for s in blk.series() {
                let (fp, kind, key) = s?;
                keys.push((fp, kind, key.to_vec()));
            }
        }
        keys.sort_by_key(|k| k.0);
        keys.dedup_by_key(|k| k.0);
        let mut buf = Vec::new();
        let mut chunks_of: Vec<Vec<Chunk>> = Vec::with_capacity(keys.len());
        for (fp, _, _) in &keys {
            let (mut ts, mut vs) = (Vec::new(), Vec::new());
            let mut parts: Vec<(Vec<i64>, Vec<f64>)> = Vec::new();
            for blk in [&older.block, &newer.block] {
                if let Some(i) = blk.find(*fp) {
                    let (mut t, mut v) = (Vec::new(), Vec::new());
                    blk.read(i, keep_from, i64::MAX, &mut buf, &mut t, &mut v)?;
                    parts.push((t, v));
                }
            }
            merge_parts(parts, &mut ts, &mut vs);
            chunks_of.push(rechunk(&ts, &vs));
        }
        let seq = newer.seq;
        let file = manifest::block_file(self.manifest.next_seq);
        let series: Vec<SeriesData<'_>> = keys.iter().zip(&chunks_of).map(|((fp, kind, key), chunks)| SeriesData { fp: *fp, kind: *kind, key, chunks: chunks.iter().map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect() }).collect();
        let block = block::write(&dir.join(&file), series)?;
        let bytes = block.bytes;
        let removed: Vec<String> = vec![older.file.clone(), newer.file.clone()];
        let mut manifest = self.manifest.clone();
        manifest.next_seq += 1;
        manifest.blocks.retain(|e| !removed.contains(&e.file));
        manifest.blocks.push(BlockEntry { seq, file: file.clone() });
        manifest.blocks.sort_by_key(|e| e.seq);
        manifest::sync_dir(&dir).map_err(io_err(&dir))?;
        manifest.save(&dir)?;
        self.manifest = manifest;
        self.blocks.remove(b);
        self.blocks.remove(a);
        let at = self.blocks.partition_point(|x| x.seq < seq);
        self.blocks.insert(at, OpenBlock { seq, file, block });
        for f in &removed {
            let p = dir.join(f);
            std::fs::remove_file(&p).map_err(io_err(&p))?;
        }
        Ok(Maintained { blocks_written: 1, blocks_removed: 2, bytes_written: bytes, series_dropped: 0 })
    }

    /// Drops what is older than `cutoff`: blocks wholly older are removed; a block partly older is
    /// rewritten without its older samples once that is more than half of it; series left with no
    /// samples leave the index.
    pub fn retain(&mut self, cutoff: i64) -> Result<Maintained, StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        let mut done = Maintained::default();
        let before = self.index.len();
        let logs_gone = self.logs.retain(cutoff.saturating_mul(1_000_000));
        if !logs_gone.is_empty() {
            let mut manifest = self.manifest.clone();
            manifest.logs.retain(|e| !logs_gone.contains(&e.file));
            manifest.save(&dir)?;
            self.manifest = manifest;
            for f in &logs_gone {
                let p = dir.join(f);
                std::fs::remove_file(&p).map_err(io_err(&p))?;
            }
            done.blocks_removed += logs_gone.len() as u64;
        }
        let gone: Vec<String> = self.blocks.iter().filter(|b| b.block.max_ts < cutoff).map(|b| b.file.clone()).collect();
        if !gone.is_empty() {
            let mut manifest = self.manifest.clone();
            manifest.blocks.retain(|e| !gone.contains(&e.file));
            manifest.save(&dir)?;
            self.manifest = manifest;
            self.blocks.retain(|b| !gone.contains(&b.file));
            for f in &gone {
                let p = dir.join(f);
                std::fs::remove_file(&p).map_err(io_err(&p))?;
            }
            done.blocks_removed += gone.len() as u64;
        }
        // A block more than half expired: rewritten alone (merged with nothing).
        let mut i = 0;
        while let Some(b) = self.blocks.get(i) {
            let span = (b.block.max_ts as i128) - (b.block.min_ts as i128);
            let expired = (cutoff as i128) - (b.block.min_ts as i128);
            if span > 0 && expired * 2 > span {
                let d = self.rewrite_alone(i, cutoff)?;
                done.blocks_written += d.blocks_written;
                done.bytes_written += d.bytes_written;
            }
            i += 1;
        }
        // The index again, without series that hold nothing.
        let head: Vec<(u64, Kind, Vec<u8>, HeadSeries)> = (0..self.head.len() as u32)
            .filter_map(|id| {
                let h = self.head.get_mut(id as usize)?;
                h.span()?;
                let info = self.index.info(id)?;
                Some((self.index.fp(id)?, info.kind, self.index.key(id)?.to_vec(), std::mem::take(h)))
            })
            .collect();
        self.reindex(head)?;
        done.series_dropped = before.saturating_sub(self.index.len()) as u64;
        Ok(done)
    }

    fn rewrite_alone(&mut self, i: usize, keep_from: i64) -> Result<Maintained, StoreError> {
        // Merging a block with itself: one input, the older samples left out.
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        let Some(b) = self.blocks.get(i) else { return Ok(Maintained::default()) };
        let mut buf = Vec::new();
        let mut keys = Vec::new();
        let mut chunks_of = Vec::new();
        for s in b.block.series() {
            let (fp, kind, key) = s?;
            let (mut ts, mut vs) = (Vec::new(), Vec::new());
            if let Some(j) = b.block.find(fp) {
                b.block.read(j, keep_from, i64::MAX, &mut buf, &mut ts, &mut vs)?;
            }
            keys.push((fp, kind, key.to_vec()));
            chunks_of.push(rechunk(&ts, &vs));
        }
        let (seq, old_file) = (b.seq, b.file.clone());
        let file = manifest::block_file(self.manifest.next_seq);
        let series: Vec<SeriesData<'_>> = keys.iter().zip(&chunks_of).map(|((fp, kind, key), chunks)| SeriesData { fp: *fp, kind: *kind, key, chunks: chunks.iter().map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect() }).collect();
        let block = block::write(&dir.join(&file), series)?;
        let bytes = block.bytes;
        let mut manifest = self.manifest.clone();
        manifest.next_seq += 1;
        for e in &mut manifest.blocks {
            if e.file == old_file {
                e.file = file.clone();
            }
        }
        manifest::sync_dir(&dir).map_err(io_err(&dir))?;
        manifest.save(&dir)?;
        self.manifest = manifest;
        if let Some(slot) = self.blocks.get_mut(i) {
            *slot = OpenBlock { seq, file, block };
        }
        let p = dir.join(&old_file);
        std::fs::remove_file(&p).map_err(io_err(&p))?;
        Ok(Maintained { blocks_written: 1, blocks_removed: 1, bytes_written: bytes, series_dropped: 0 })
    }

    /// The ids of the series `sel` selects, sorted.
    pub fn select(&self, sel: &Selector, out: &mut Vec<u32>) {
        self.index.select(&self.symbols, sel, out);
    }

    pub fn series(&self, id: u32) -> Option<SeriesRef<'_>> {
        self.index.info(id).map(|info| SeriesRef { store: self, id, info })
    }

    /// Appends series `id`'s samples in `[from, to]`, in time order; at a time, the latest write
    /// (the head over the blocks, a later block over an earlier).
    pub fn read(&self, id: u32, from: i64, to: i64, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), StoreError> {
        self.read_with(id, from, to, &mut Scratch::default(), ts, values)
    }

    /// [`Store::read`] with buffers kept across calls: a query reading many series allocates
    /// nothing a series once they have grown.
    pub fn read_with(&self, id: u32, from: i64, to: i64, scratch: &mut Scratch, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), StoreError> {
        let Some(fp) = self.index.fp(id) else { return Ok(()) };
        let head = self.head.get(id as usize);
        // The sources that hold the series in range, in write order, with their spans.
        let sources = &mut scratch.sources;
        sources.clear();
        for (bi, b) in self.blocks.iter().enumerate() {
            if let Some(i) = b.block.find(fp)
                && let Some((f, l)) = b.block.span(i)
                && l >= from
                && f <= to
            {
                sources.push((Some(bi), f, l));
            }
        }
        if let Some((f, l)) = head.and_then(HeadSeries::span)
            && l >= from
            && f <= to
        {
            sources.push((None, f, l));
        }
        let buf = &mut scratch.buf;
        let read_one = |src: Option<usize>, buf: &mut Vec<u8>, ts: &mut Vec<i64>, vs: &mut Vec<f64>| -> Result<(), StoreError> {
            match src {
                Some(bi) => {
                    if let Some(b) = self.blocks.get(bi)
                        && let Some(i) = b.block.find(fp)
                    {
                        b.block.read(i, from, to, buf, ts, vs)?;
                    }
                }
                None => {
                    if let Some(h) = head {
                        h.read(from, to, ts, vs)?;
                    }
                }
            }
            Ok(())
        };
        // In write order and disjoint in time, oldest first: appended as they are.
        let disjoint = sources.windows(2).all(|w| matches!(w, [a, b] if a.2 < b.1));
        if disjoint {
            for &(src, _, _) in sources.iter() {
                read_one(src, buf, ts, values)?;
            }
            return Ok(());
        }
        let mut parts = Vec::with_capacity(sources.len());
        for &(src, _, _) in sources.iter() {
            let (mut t, mut v) = (Vec::new(), Vec::new());
            read_one(src, buf, &mut t, &mut v)?;
            parts.push((t, v));
        }
        merge_parts(parts, ts, values);
        Ok(())
    }

    /// The newest `limit` log records in `[from, to]` (ns) that `search` matches.
    pub fn search_logs(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, StoreError> {
        Ok(self.logs.search(search, from, to, limit)?)
    }

    /// Bytes of logs on disk.
    pub fn log_bytes(&self) -> u64 {
        self.logs.disk_bytes()
    }

    /// Every metric's name.
    pub fn metric_names(&self) -> impl Iterator<Item = &str> {
        self.index.metrics().iter().map(|&m| self.symbols.get(m))
    }

    /// Every value of tag `key`.
    pub fn tag_values(&self, key: &str) -> Vec<&str> {
        self.index.tag_values(&self.symbols, key).into_iter().map(|s| self.symbols.get(s)).collect()
    }

    pub fn totals(&self) -> Counts {
        self.totals
    }

    pub fn series_count(&self) -> usize {
        self.index.len()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Bytes of blocks on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.blocks.iter().map(|b| b.block.bytes).sum()
    }

    /// Bytes held in memory, by part.
    pub fn heap(&self) -> Heap {
        Heap {
            symbols: self.symbols.heap_bytes(),
            index: self.index.heap_bytes() + self.blocks.iter().map(|b| b.block.heap_bytes()).sum::<usize>() + self.logs.heap_bytes(),
            samples: self.head.iter().map(HeadSeries::heap_bytes).sum::<usize>() + self.head.capacity() * std::mem::size_of::<HeadSeries>(),
        }
    }

    pub fn kind_conflicts(&self) -> u64 {
        self.index.kind_conflicts
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// Buffers a reader keeps across [`Store::read_with`] calls.
#[derive(Debug, Default)]
pub struct Scratch {
    buf: Vec<u8>,
    sources: Vec<(Option<usize>, i64, i64)>,
}

fn restore(head: &mut [HeadSeries], taken: Vec<(u32, Vec<Chunk>)>) {
    for (id, chunks) in taken {
        if let Some(h) = head.get_mut(id as usize) {
            h.restore(chunks);
        }
    }
}

/// Merges sorted parts (oldest write first) into `ts` and `values`: at a time, the last part's
/// sample.
fn merge_parts(parts: Vec<(Vec<i64>, Vec<f64>)>, ts: &mut Vec<i64>, values: &mut Vec<f64>) {
    let mut all: Vec<(i64, usize, f64)> = Vec::with_capacity(parts.iter().map(|p| p.0.len()).sum());
    for (rank, (t, v)) in parts.into_iter().enumerate() {
        all.extend(t.into_iter().zip(v).map(|(t, v)| (t, rank, v)));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut i = 0;
    while let Some(&(t, _, _)) = all.get(i) {
        let mut j = i;
        while all.get(j + 1).is_some_and(|n| n.0 == t) {
            j += 1;
        }
        if let Some(&(_, _, v)) = all.get(j) {
            ts.push(t);
            values.push(v);
        }
        i = j + 1;
    }
}

/// Sorted, distinct samples into chunks of [`CHUNK_SAMPLES`].
fn rechunk(ts: &[i64], values: &[f64]) -> Vec<Chunk> {
    let mut out = Vec::with_capacity(ts.len().div_ceil(CHUNK_SAMPLES as usize));
    let mut e = Encoder::new();
    for (&t, &v) in ts.iter().zip(values) {
        if e.count() >= CHUNK_SAMPLES {
            out.push(std::mem::take(&mut e).finish());
        }
        let _ = e.push(t, v);
    }
    if e.count() > 0 {
        out.push(e.finish());
    }
    out
}

/// What a store holds in memory, by part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Heap {
    pub symbols: usize,
    pub index: usize,
    pub samples: usize,
}

impl Heap {
    pub fn total(&self) -> usize {
        self.symbols + self.index + self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{Builder, SeriesKey};
    use crate::testkit::Rng;

    fn read_all(s: &Store, sel: &Selector) -> Vec<(String, Vec<(i64, u64)>)> {
        let mut ids = Vec::new();
        s.select(sel, &mut ids);
        let mut out: Vec<(String, Vec<(i64, u64)>)> = ids
            .iter()
            .map(|&id| {
                let r = s.series(id).unwrap();
                let name = format!("{}{:?}", r.metric(), r.tags().collect::<Vec<_>>());
                let (mut ts, mut vs) = (Vec::new(), Vec::new());
                s.read(id, i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
                (name, ts.into_iter().zip(vs.into_iter().map(f64::to_bits)).collect())
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn written_samples_are_selected_and_read_back() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let a = SeriesKey::new("system.cpu.user", [("host", "a"), ("env", "prod")], Kind::Gauge).unwrap();
        let b_key = SeriesKey::new("system.cpu.user", [("host", "b"), ("env", "prod")], Kind::Gauge).unwrap();
        let m = SeriesKey::new("system.mem.used", [("host", "a")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        for k in [&a, &b_key, &m] {
            b.define(k);
        }
        b.run(a.fp, (0..10).map(|i| (i * 1000, i as f64)));
        b.run(b_key.fp, (0..10).map(|i| (i * 1000, 100.0 + i as f64)));
        b.run(m.fp, [(0, 1e9)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.counts, Counts { runs: 3, new_series: 3, samples: 21, late: 0, duplicates: 0, logs: 0 });
        let mut ids = Vec::new();
        s.select(&Selector::metric("system.cpu.user").tag("env", "prod"), &mut ids);
        assert_eq!(ids.len(), 2);
        let b_id = ids.iter().copied().find(|&id| s.series(id).and_then(|r| r.tag("host")) == Some("b")).unwrap();
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(b_id, 2000, 4000, &mut ts, &mut vs).unwrap();
        assert_eq!((ts.clone(), vs.clone()), (vec![2000, 3000, 4000], vec![102.0, 103.0, 104.0]));
        let mut b = Builder::new();
        b.run(b_key.fp, [(10_000, 110.0), (500, 0.5)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!((applied.counts.new_series, applied.counts.late), (0, 1));
        ts.clear();
        vs.clear();
        s.read(b_id, 0, 1000, &mut ts, &mut vs).unwrap();
        assert_eq!(ts, vec![0, 500, 1000]);
        assert_eq!(s.metric_names().collect::<Vec<_>>(), vec!["system.cpu.user", "system.mem.used"]);
        let mut hosts = s.tag_values("host");
        hosts.sort_unstable();
        assert_eq!(hosts, vec!["a", "b"]);
    }

    #[test]
    fn a_run_of_an_unknown_series_is_reported_and_a_bad_definition_refused() {
        let mut s = Store::new(Keyed::with_keys(1, 2));
        let a = SeriesKey::new("m", [("host", "a")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.run(a.fp, [(1, 1.0)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.unknown, vec![a.fp]);
        assert_eq!(s.series_count(), 0);
        b.define(&a);
        b.run(a.fp, [(1, 1.0)]);
        let applied = s.apply(&b.finish()).unwrap();
        assert!(applied.unknown.is_empty());
        assert_eq!(applied.counts.samples, 1);
        let mut forged = SeriesKey::new("m", [("host", "evil")], Kind::Gauge).unwrap();
        forged.fp = a.fp;
        b.define(&forged);
        let applied = s.apply(&b.finish()).unwrap();
        assert_eq!(applied.refused, vec![a.fp]);
        assert_eq!(s.series_count(), 1);
    }

    /// Batches of random series, times and late writes, with the model of what a read must give.
    fn random_batches(seed: u64, rounds: usize) -> (Vec<Vec<u8>>, std::collections::BTreeMap<(String, i64), u64>) {
        let mut rng = Rng::new(seed);
        let mut model = std::collections::BTreeMap::new();
        let mut batches = Vec::new();
        let mut b = Builder::new();
        for round in 0..rounds {
            for _ in 0..20 {
                let host = format!("h{}", rng.below(15));
                let k = SeriesKey::new("m", [("host", host.as_str())], Kind::Gauge).unwrap();
                b.define(&k);
                let samples: Vec<(i64, f64)> = (0..4)
                    .map(|i| {
                        let late = rng.below(8) == 0;
                        let t = if late { rng.below(round as u64 * 1000 + 1) as i64 } else { round as i64 * 1000 + i * 100 + rng.below(50) as i64 };
                        (t, rng.f64())
                    })
                    .collect();
                // Within a run, the last given at a time wins.
                let mut sorted = samples.clone();
                sorted.sort_by_key(|s| s.0);
                let mut dedup: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
                for (t, v) in &samples {
                    dedup.insert(*t, *v);
                }
                for (t, v) in dedup {
                    model.insert((format!("m{:?}", vec![("host", host.as_str())]), t), v.to_bits());
                }
                b.run(k.fp, samples);
            }
            batches.push(b.finish());
        }
        (batches, model)
    }

    fn model_view(model: &std::collections::BTreeMap<(String, i64), u64>) -> Vec<(String, Vec<(i64, u64)>)> {
        let mut out: std::collections::BTreeMap<String, Vec<(i64, u64)>> = std::collections::BTreeMap::new();
        for ((name, t), v) in model {
            out.entry(name.clone()).or_default().push((*t, *v));
        }
        out.into_iter().collect()
    }

    #[test]
    fn cuts_compactions_and_a_reopen_keep_exactly_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let (batches, model) = random_batches(7, 60);
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        for (i, batch) in batches.iter().enumerate() {
            s.apply(batch).unwrap();
            if i % 7 == 6 {
                s.cut(Point { index: i as u64 + 1, term: 1 }).unwrap();
                s.compact().unwrap();
            }
        }
        let sel = Selector::metric("m");
        assert_eq!(read_all(&s, &sel), model_view(&model), "live");
        // Blocks stay logarithmic in what they hold.
        assert!(s.block_count() <= 1 + (batches.len() / 7).ilog2() as usize + 1, "{} blocks", s.block_count());
        let durable = s.durable();
        assert_eq!(durable.index, 56);
        // A restart: blocks reopen, and the log is replayed from the durable point.
        drop(s);
        let mut s = Store::open(dir.path(), Keyed::with_keys(5, 6)).unwrap();
        assert_eq!(s.durable(), durable);
        for batch in batches.iter().skip(durable.index as usize) {
            s.apply(batch).unwrap();
        }
        assert_eq!(read_all(&s, &sel), model_view(&model), "after a restart");
    }

    #[test]
    fn a_crash_between_a_block_and_its_manifest_leaves_no_trace() {
        let dir = tempfile::tempdir().unwrap();
        let (batches, model) = random_batches(9, 20);
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        for batch in batches.iter().take(10) {
            s.apply(batch).unwrap();
        }
        s.cut(Point { index: 10, term: 1 }).unwrap();
        for batch in batches.iter().skip(10) {
            s.apply(batch).unwrap();
        }
        // A block written whole whose manifest never was (the crash after the block's sync): the
        // block is an orphan, deleted at open; its samples come back from the log.
        let orphan = dir.path().join(manifest::block_file(99));
        std::fs::copy(dir.path().join(manifest::block_file(0)), &orphan).unwrap();
        std::fs::write(dir.path().join("MANIFEST.tmp"), b"half a manifest").unwrap();
        drop(s);
        let mut s = Store::open(dir.path(), Keyed::with_keys(3, 4)).unwrap();
        assert!(!orphan.exists() && !dir.path().join("MANIFEST.tmp").exists());
        assert_eq!(s.durable().index, 10);
        for batch in batches.iter().skip(10) {
            s.apply(batch).unwrap();
        }
        assert_eq!(read_all(&s, &Selector::metric("m")), model_view(&model));
    }

    #[test]
    fn a_correction_after_a_cut_wins_over_the_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        let k = SeriesKey::new("m", [], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.define(&k);
        b.run(k.fp, (0..10).map(|i| (i * 10, i as f64)));
        s.apply(&b.finish()).unwrap();
        s.cut(Point { index: 1, term: 1 }).unwrap();
        b.run(k.fp, [(50, -5.0), (55, 5.5), (200, 20.0)]);
        s.apply(&b.finish()).unwrap();
        let mut ids = Vec::new();
        s.select(&Selector::metric("m"), &mut ids);
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
        assert_eq!(ts, vec![40, 50, 55, 60]);
        assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
        // Cut again: two blocks overlap in time, the later wins; compacted into one, the same.
        s.cut(Point { index: 2, term: 1 }).unwrap();
        ts.clear();
        vs.clear();
        s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
        assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
        s.compact().unwrap();
        assert_eq!(s.block_count(), 1);
        ts.clear();
        vs.clear();
        s.read(ids[0], 40, 60, &mut ts, &mut vs).unwrap();
        assert_eq!(vs, vec![4.0, -5.0, 5.5, 6.0]);
    }

    #[test]
    fn retention_drops_old_blocks_and_series_that_hold_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        let old = SeriesKey::new("m", [("host", "old")], Kind::Gauge).unwrap();
        let live = SeriesKey::new("m", [("host", "live")], Kind::Gauge).unwrap();
        let mut b = Builder::new();
        b.define(&old);
        b.define(&live);
        b.run(old.fp, (0..10).map(|i| (i * 10, 1.0)));
        b.run(live.fp, (0..10).map(|i| (i * 10, 2.0)));
        s.apply(&b.finish()).unwrap();
        s.cut(Point { index: 1, term: 1 }).unwrap();
        b.run(live.fp, (100..110).map(|i| (i * 10, 3.0)));
        s.apply(&b.finish()).unwrap();
        s.cut(Point { index: 2, term: 1 }).unwrap();
        let done = s.retain(1000).unwrap();
        assert_eq!(done.blocks_removed, 1);
        assert_eq!(done.series_dropped, 1);
        assert_eq!(s.tag_values("host"), vec!["live"]);
        let mut ids = Vec::new();
        s.select(&Selector::metric("m"), &mut ids);
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        s.read(ids[0], i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
        assert_eq!(ts.first(), Some(&1000));
        // And so after a restart.
        drop(s);
        let s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        assert_eq!(s.series_count(), 1);
    }

    #[test]
    fn logs_ride_batches_through_cuts_a_reopen_and_retention() {
        use crate::logs::record::Record;
        let dir = tempfile::tempdir().unwrap();
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        let rec = |i: i64| Record { ts: i * 1_000_000_000, severity: if i % 7 == 0 { 17 } else { 9 }, service: "api".into(), host: "h".into(), source: "kiln".into(), message: format!("event {i} {}", if i % 7 == 0 { "boom" } else { "fine" }), ..Record::default() };
        let mut b = Builder::new();
        for batch in 0..20 {
            let recs: Vec<Record> = (batch * 1000..(batch + 1) * 1000).map(rec).collect();
            b.logs(&recs);
            let applied = s.apply(&b.finish()).unwrap();
            assert_eq!(applied.counts.logs, 1000);
            if batch % 6 == 5 {
                s.cut(Point { index: batch as u64 + 1, term: 1 }).unwrap();
            }
        }
        let q = crate::logs::search::parse("status:error boom").unwrap();
        let newest = |s: &Store| s.search_logs(&q, 0, i64::MAX, 3).unwrap().records.iter().map(|r| r.ts / 1_000_000_000).collect::<Vec<_>>();
        assert_eq!(newest(&s), vec![19_999, 19_992, 19_985]);
        // After a restart: the files reopen; the batches past the durable point come again.
        let durable = s.durable();
        drop(s);
        let mut s = Store::open(dir.path(), Keyed::with_keys(1, 2)).unwrap();
        for batch in (durable.index as i64)..20 {
            let recs: Vec<Record> = (batch * 1000..(batch + 1) * 1000).map(rec).collect();
            b.logs(&recs);
            s.apply(&b.finish()).unwrap();
        }
        assert_eq!(newest(&s), vec![19_999, 19_992, 19_985]);
        assert!(s.log_bytes() > 0);
        // Retention drops whole files: those wholly before 13,000 s go (0–5,999 and 6,000–11,999);
        // the one holding 12,000–17,999 stays whole.
        s.cut(Point { index: 20, term: 1 }).unwrap();
        let before = s.log_bytes();
        s.retain(13_000_000).unwrap();
        assert!(s.log_bytes() < before);
        let all = crate::logs::search::parse("*").unwrap();
        assert!(s.search_logs(&all, 0, 11_999_999_999_999, 1).unwrap().records.is_empty());
        assert_eq!(s.search_logs(&all, 0, 12_000_000_000_000, 1).unwrap().records.len(), 1);
    }

    #[test]
    fn replicas_that_apply_the_same_batches_hold_the_same_contents() {
        let (batches, model) = random_batches(77, 30);
        let mut a = Store::new(Keyed::with_keys(1, 2));
        let mut b = Store::new(Keyed::with_keys(9, 9));
        for batch in &batches {
            a.apply(batch).unwrap();
            b.apply(batch).unwrap();
        }
        let sel = Selector::metric("m");
        assert_eq!(read_all(&a, &sel), read_all(&b, &sel));
        assert_eq!(read_all(&a, &sel), model_view(&model));
    }
}
