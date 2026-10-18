//! A replica's logs (`docs/observe.md` §4.6): a head of rows as batches brought them, sealed into
//! blocks of [`BLOCK_TEXT`] as it fills, written to a logs file at each cut, searched newest first.

use std::path::Path;

use super::block::{BLOCK_TEXT, LogBlock};
use super::file::{LogFile, LogFileError};
use super::record::{Record, RecordRef, RowError, Rows};
use super::search::{Maybe, RowScratch, Search};

#[derive(Debug, thiserror::Error)]
pub enum LogStoreError {
    #[error(transparent)]
    File(#[from] LogFileError),
    #[error(transparent)]
    Row(#[from] RowError),
    #[error("a log block's text does not decompress")]
    Text,
}

/// A logs file the store holds.
#[derive(Debug)]
pub struct HeldFile {
    pub seq: u64,
    pub file: String,
    pub log: LogFile,
}

#[derive(Debug, Default)]
pub struct LogStore {
    /// Rows as batches brought them: each segment's times chained from 0.
    head: Vec<(Vec<u8>, u32)>,
    head_bytes: usize,
    sealed: Vec<LogBlock>,
    pub files: Vec<HeldFile>,
}

/// A search's answer.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Found {
    /// Newest first.
    pub records: Vec<Record>,
    pub blocks_total: u64,
    /// Blocks ruled out by their summary, never read.
    pub blocks_pruned: u64,
    pub rows_scanned: u64,
}

impl LogStore {
    /// Takes a batch's rows (their encoding, decompressed).
    pub fn append(&mut self, raw: Vec<u8>, rows: u32) -> Result<(), LogStoreError> {
        if rows == 0 {
            return Ok(());
        }
        self.head_bytes += raw.len();
        self.head.push((raw, rows));
        if self.head_bytes >= BLOCK_TEXT {
            self.seal()?;
        }
        Ok(())
    }

    /// Seals the head into a block.
    fn seal(&mut self) -> Result<(), LogStoreError> {
        if self.head.is_empty() {
            return Ok(());
        }
        let mut refs: Vec<RecordRef<'_>> = Vec::new();
        for (raw, n) in &self.head {
            let mut rows = Rows::new(raw, *n as usize);
            while let Some(r) = rows.next_row()? {
                refs.push(r);
            }
        }
        let block = LogBlock::seal(&refs);
        drop(refs);
        self.sealed.push(block);
        self.head.clear();
        self.head_bytes = 0;
        Ok(())
    }

    /// Seals what is left and writes every unwritten block to `path`; none if there was nothing.
    pub fn cut(&mut self, path: &Path) -> Result<Option<LogFile>, LogStoreError> {
        self.seal()?;
        if self.sealed.is_empty() {
            return Ok(None);
        }
        let f = super::file::write(path, &self.sealed)?;
        self.sealed.clear();
        Ok(Some(f))
    }

    /// Rows held in memory (head and sealed blocks not yet written).
    pub fn unwritten_rows(&self) -> u64 {
        self.head.iter().map(|h| u64::from(h.1)).sum::<u64>() + self.sealed.iter().map(|b| u64::from(b.rows)).sum::<u64>()
    }

    pub fn heap_bytes(&self) -> usize {
        self.head.iter().map(|h| h.0.capacity()).sum::<usize>() + self.sealed.iter().map(|b| b.text.capacity() + b.summary_bytes()).sum::<usize>() + self.files.iter().map(|f| f.log.heap_bytes()).sum::<usize>()
    }

    pub fn disk_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.log.bytes).sum()
    }

    /// Drops files wholly older than `cutoff` (ns); their names, for the caller to delete.
    pub fn retain(&mut self, cutoff: i64) -> Vec<String> {
        let gone: Vec<String> = self.files.iter().filter(|f| f.log.max_ts < cutoff).map(|f| f.file.clone()).collect();
        self.files.retain(|f| f.log.max_ts >= cutoff);
        gone
    }

    /// The newest `limit` records in `[from, to]` (ns) that `search` matches.
    pub fn search(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, LogStoreError> {
        let mut found = Found::default();
        if limit == 0 || to < from {
            return Ok(found);
        }
        // Every block, newest end first; the head last examined but always (it has no summary).
        enum Src<'a> {
            File(&'a HeldFile, usize),
            Sealed(&'a LogBlock),
        }
        let mut srcs: Vec<(i64, i64, Src<'_>)> = Vec::new();
        for f in &self.files {
            for (i, b) in f.log.blocks.iter().enumerate() {
                srcs.push((b.min_ts, b.max_ts, Src::File(f, i)));
            }
        }
        for b in &self.sealed {
            srcs.push((b.min_ts, b.max_ts, Src::Sealed(b)));
        }
        srcs.sort_by_key(|s| std::cmp::Reverse(s.1));
        found.blocks_total = srcs.len() as u64;
        // The best `limit` so far, kept sorted newest first.
        let mut best: Vec<(i64, Record)> = Vec::with_capacity(limit.min(4096));
        let mut scratch = RowScratch::default();
        let mut raw = Vec::new();
        let mut whole = LogBlock::seal(&[]);
        let consider = |r: &RecordRef<'_>, best: &mut Vec<(i64, Record)>, scratch: &mut RowScratch, found: &mut Found| {
            found.rows_scanned += 1;
            if r.ts < from || r.ts > to || !search.row(r, scratch) {
                return;
            }
            if best.len() >= limit && best.last().is_some_and(|w| w.0 >= r.ts) {
                return;
            }
            let at = best.partition_point(|x| x.0 >= r.ts);
            best.insert(at, (r.ts, r.to_owned()));
            best.truncate(limit);
        };
        // The head first: it is the newest, in arrival order.
        for (seg, n) in &self.head {
            let mut rows = Rows::new(seg, *n as usize);
            while let Some(r) = rows.next_row()? {
                consider(&r, &mut best, &mut scratch, &mut found);
            }
        }
        for (min_ts, max_ts, src) in srcs {
            if max_ts < from || min_ts > to {
                found.blocks_pruned += 1;
                continue;
            }
            // Nothing in this block or any after it is newer than the worst kept.
            if best.len() >= limit && best.last().is_some_and(|w| w.0 > max_ts) {
                break;
            }
            let summary = match &src {
                Src::File(f, i) => f.log.blocks.get(*i),
                Src::Sealed(b) => Some(*b),
            };
            if summary.is_some_and(|b| search.block(b) == Maybe::No) {
                found.blocks_pruned += 1;
                continue;
            }
            let block: &LogBlock = match src {
                Src::File(f, i) => {
                    f.log.load(i, &mut whole)?;
                    &whole
                }
                Src::Sealed(b) => b,
            };
            block.decompress(&mut raw).map_err(|_| LogStoreError::Text)?;
            let mut rows = block.rows(&raw);
            while let Some(r) = rows.next_row()? {
                consider(&r, &mut best, &mut scratch, &mut found);
            }
        }
        found.records = best.into_iter().map(|(_, r)| r).collect();
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::put_row;
    use crate::logs::search::parse;

    fn segment(recs: &[Record]) -> (Vec<u8>, u32) {
        let mut raw = Vec::new();
        let mut prev = 0;
        for r in recs {
            put_row(&mut raw, r, prev);
            prev = r.ts;
        }
        (raw, recs.len() as u32)
    }

    fn rec(i: i64) -> Record {
        Record {
            ts: i * 1_000_000_000,
            severity: if i % 10 == 0 { 17 } else { 9 },
            service: ["api", "web", "db"][(i % 3) as usize].into(),
            host: format!("h{}", i % 4),
            source: "kiln".into(),
            message: format!("event {i} {}", if i % 10 == 0 { "failed: connection reset" } else { "ok" }),
            attrs: vec![("n".into(), i.to_string())],
            ..Record::default()
        }
    }

    #[test]
    fn searches_find_the_newest_matches_across_head_sealed_blocks_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = LogStore::default();
        // 30,000 records in batches of 500: several blocks sealed, two files written.
        for chunk in (0..30_000).collect::<Vec<i64>>().chunks(500) {
            let recs: Vec<Record> = chunk.iter().map(|&i| rec(i)).collect();
            let (raw, n) = segment(&recs);
            s.append(raw, n).unwrap();
            if chunk[0] == 10_000 || chunk[0] == 20_000 {
                let path = dir.path().join(format!("{}.logs", chunk[0]));
                let f = s.cut(&path).unwrap().unwrap();
                s.files.push(HeldFile { seq: chunk[0] as u64, file: path.file_name().unwrap().to_string_lossy().into_owned(), log: f });
            }
        }
        let q = parse("status:error \"connection reset\" service:api").unwrap();
        let found = s.search(&q, 0, i64::MAX, 5).unwrap();
        // Errors are every 10th; service api is i % 3 == 0: i ≡ 0 (mod 30), newest first.
        let want: Vec<i64> = (0..30_000).rev().filter(|i| i % 30 == 0).take(5).collect();
        assert_eq!(found.records.iter().map(|r| r.ts / 1_000_000_000).collect::<Vec<_>>(), want);
        // A word no block holds: every block ruled out by its filter, none read.
        let found = s.search(&parse("nosuchwordanywhere").unwrap(), 0, i64::MAX, 10).unwrap();
        assert!(found.records.is_empty());
        assert_eq!(found.blocks_pruned, found.blocks_total);
        // A time range: only its blocks.
        let found = s.search(&parse("*").unwrap(), 5_000_000_000_000, 5_009_000_000_000, 100).unwrap();
        assert_eq!(found.records.len(), 10);
        assert!(found.blocks_pruned > 0);
        // The newest 3 of everything, with an early stop: few rows scanned.
        let found = s.search(&parse("").unwrap(), 0, i64::MAX, 3).unwrap();
        assert_eq!(found.records.iter().map(|r| r.ts / 1_000_000_000).collect::<Vec<_>>(), vec![29_999, 29_998, 29_997]);
        assert!(found.rows_scanned < 15_000, "{}", found.rows_scanned);
    }
}
