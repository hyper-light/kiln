//! One replica's store: symbols, the index, the head and the files on disk, fed the group's entries
//! in the order it committed them, and asked for series, samples and logs (`docs/observe.md` §4,
//! §5).
//!
//! Applying is deterministic: replicas that apply the same entries in the same order hold the same
//! series, samples and logs, and answer every query alike, whenever each cut, compacted, retained
//! or restarted. That rests on four rules:
//! - a series is defined with the first of its samples kept, never alone, so every series a
//!   replica holds is in its head or its blocks, and a replica that opens from its blocks holds
//!   the series the others hold;
//! - retention is an entry ([`crate::command`]): the cutoffs (one for samples, one for log records)
//!   move at one point of the log on every replica, and a series goes once its newest sample is
//!   older; files are deleted only behind the cutoffs that are durable;
//! - nothing older than the cutoff is kept by an entry or read by a query, so what a replica's files
//!   still hold past it is never seen;
//! - each sender's last write is kept with the rest ([`Session`]), so a write sent twice is applied
//!   once.
//!
//! Series ids are each replica's own, assigned in the order it learns series, and never leave it;
//! what a query answers is ordered by the series' own keys.
//!
//! Durability: the group's log is the write-ahead log. [`Store::cut`] writes the head into files,
//! syncs them, and records in the manifest the log point it holds everything up to
//! ([`Store::durable`]) with the replicated state there; a store opened again holds exactly that,
//! and the log is applied from that point. Reads merge the blocks and the head, the later write
//! winning at a time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hashbrown::HashTable;

use crate::batch::{BatchError, Item, Kind, Reader};
use crate::block::{self, Block, BlockError, SeriesData};
use crate::command::{self, Command, CommandError};
use crate::gorilla::{Chunk, DecodeError, Encoder};
use crate::hash::Keyed;
use crate::head::{Appended, CHUNK_SAMPLES, HeadSeries};
use crate::image::{FileKind, Image, ImageFile};
use crate::index::{Defined, Index, IndexError, Selector, SeriesInfo};
use crate::logs::file::LogFile;
use crate::logs::search::Search;
use crate::logs::store::{Found, HeldFile, LogStore, LogStoreError};
use crate::manifest::{self, BlockEntry, Manifest, ManifestError, Membership, Point, Session};
use crate::symbols::Symbols;

/// Senders a store keeps sessions for at the most; past it, the one whose last write is oldest in
/// the log goes (Ongaro's thesis §6.3 expires sessions by a rule every server applies alike). A
/// session is 32 bytes in memory and a line of the manifest, which every cut rewrites: this many
/// is under 200 KB of it. A session also goes once its last write is older than the cutoff.
pub const MAX_SESSIONS: usize = 4096;

/// Snapshots of files a store keeps linked for members fetching them: the image its replica holds
/// now, and the one before, which a member may be fetching still when it is replaced.
pub const KEEP_SNAPSHOTS: usize = 2;

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

/// An entry this build cannot apply: a command or a batch format newer than it. Every replica of
/// this build would refuse it alike; one of a newer build would apply it, so it is never skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Newer {
    #[error("an entry of command {0}, which this build does not know")]
    Command(u8),
    #[error("a write batch of format {0}, which this build does not read")]
    Batch(u8),
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
    #[error("{0}: not staged as the image names it")]
    Staged(String),
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
    /// Samples older than the cutoff, dropped.
    pub expired: u64,
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
        self.expired += o.expired;
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
    /// Series not defined for the bound on series ([`Command::Limits`]).
    pub over_limit: Vec<u64>,
}

/// What a write came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Written {
    Applied(Applied),
    /// Its sender's write of this place or a later one was applied already: nothing again.
    Duplicate,
    /// Its batch stopped at an item that does not decode, alike on every replica; what came before
    /// it is applied.
    Failed(ApplyError),
}

/// What applying an entry answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A leader's empty entry.
    Empty,
    Write { sender: u64, seq: u64, outcome: Written },
    Retain { cutoff: i64, logs_cutoff: i64, series_dropped: u64 },
    Limits { max_series: u64 },
    /// An entry that holds no command whole: applied as nothing, alike on every replica.
    Malformed(CommandError),
}

#[derive(Debug)]
struct OpenBlock {
    /// Write order: a later block's sample wins over an earlier one's at a time.
    seq: u64,
    file: String,
    block: Block,
}

/// A definition of the batch being applied, not yet taken: its key's place in the batch.
#[derive(Debug, Clone, Copy)]
struct Pending {
    fp: u64,
    kind: Kind,
    start: usize,
    end: usize,
}

#[derive(Debug)]
pub struct Store {
    hash: Keyed,
    symbols: Symbols,
    index: Index,
    head: Vec<HeadSeries>,
    /// Each series' newest sample's time, by id: what retention weighs it by.
    last: Vec<i64>,
    blocks: Vec<OpenBlock>,
    logs: LogStore,
    dir: Option<PathBuf>,
    /// What is durable: the files, and the replicated state at `durable`.
    manifest: Manifest,
    /// The replicated state as of the last entry applied.
    cutoff: i64,
    logs_cutoff: i64,
    max_series: u64,
    sessions: BTreeMap<u64, Session>,
    conf: Membership,
    totals: Counts,
    /// Installs this store took: a rewrite planned before one is not taken in after.
    epoch: u64,
    /// The last entry of the group's log applied (its index), as the machine says.
    applied: u64,
    pending: Vec<Pending>,
    /// `pending` by fingerprint (a fingerprint is a hash already: it is its own).
    pending_at: HashTable<u32>,
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

impl std::ops::AddAssign for Maintained {
    fn add_assign(&mut self, o: Self) {
        self.blocks_written += o.blocks_written;
        self.blocks_removed += o.blocks_removed;
        self.bytes_written += o.bytes_written;
        self.series_dropped += o.series_dropped;
    }
}

/// A rewrite of blocks (a merge's two, or one retention cuts short), planned under the store's
/// lock with its file's name reserved, built from the files alone ([`Rewrite::build`]), and taken
/// in under the lock again ([`Store::commit`]): the building, which reads and writes whole blocks,
/// holds nothing, so the group goes on applying meanwhile.
#[derive(Debug)]
pub struct Rewrite {
    dir: PathBuf,
    /// Its inputs, in write order.
    from: Vec<(u64, String)>,
    /// Samples older are left out: the durable cutoff when it was planned.
    keep_from: i64,
    file: String,
    /// Its place in write order: its newest input's.
    seq: u64,
    epoch: u64,
}

/// A rewrite built, to be taken in.
#[derive(Debug)]
pub struct Built {
    rewrite: Rewrite,
    block: Block,
}

impl Rewrite {
    /// Reads every input whole and writes the rewritten block: every series of any input, its
    /// samples merged (a later input's winning at a time) without those older than the cutoff,
    /// and a series left with none left out; its kind the newest input's, as the index had it when
    /// that block was cut. An input gone since the plan (another rewrite or an install took it)
    /// is an error, and so the plan is stale: nothing of it is left on disk.
    pub fn build(self) -> Result<Built, StoreError> {
        let out = self.dir.join(&self.file);
        match self.build_inner() {
            Ok(block) => Ok(Built { rewrite: self, block }),
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                Err(e)
            }
        }
    }

    fn build_inner(&self) -> Result<Block, StoreError> {
        let blocks: Vec<Block> = self.from.iter().map(|(_, f)| Block::open(&self.dir.join(f))).collect::<Result<_, _>>()?;
        let mut keys: Vec<(u64, Kind, Vec<u8>)> = Vec::new();
        for blk in blocks.iter().rev() {
            for s in blk.series() {
                let (fp, kind, key) = s?;
                keys.push((fp, kind, key.to_vec()));
            }
        }
        // Stable: of one fingerprint, the newest input's first.
        keys.sort_by_key(|k| k.0);
        keys.dedup_by_key(|k| k.0);
        let mut buf = Vec::new();
        let mut chunks_of: Vec<Vec<Chunk>> = Vec::with_capacity(keys.len());
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        for (fp, _, _) in &keys {
            ts.clear();
            vs.clear();
            let mut parts: Vec<(Vec<i64>, Vec<f64>)> = Vec::new();
            for blk in &blocks {
                if let Some(i) = blk.find(*fp) {
                    let (mut t, mut v) = (Vec::new(), Vec::new());
                    blk.read(i, self.keep_from, i64::MAX, &mut buf, &mut t, &mut v)?;
                    parts.push((t, v));
                }
            }
            if parts.len() == 1 {
                if let Some((t, v)) = parts.pop() {
                    chunks_of.push(rechunk(&t, &v));
                }
            } else {
                merge_parts(parts, &mut ts, &mut vs);
                chunks_of.push(rechunk(&ts, &vs));
            }
        }
        let series: Vec<SeriesData<'_>> = keys.iter().zip(&chunks_of).map(|((fp, kind, key), chunks)| SeriesData { fp: *fp, kind: *kind, key, chunks: chunks.iter().map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect() }).collect();
        Ok(block::write(&self.dir.join(&self.file), series)?)
    }

    /// Bytes its inputs hold.
    pub fn input_bytes(&self) -> u64 {
        self.from.iter().filter_map(|(_, f)| std::fs::metadata(self.dir.join(f)).ok()).map(|m| m.len()).sum()
    }
}

fn remove_if_there(path: &Path) -> Result<(), StoreError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::Io { path: path.to_owned(), source }),
    }
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io { path: path.to_owned(), source }
}

/// Where `part` lies within `whole`, which holds it.
fn place_in(whole: &[u8], part: &[u8]) -> Option<(usize, usize)> {
    let start = (part.as_ptr() as usize).checked_sub(whole.as_ptr() as usize)?;
    let end = start.checked_add(part.len())?;
    (whole.get(start..end) == Some(part)).then_some((start, end))
}

impl Store {
    /// A store in memory: it applies and answers, and cannot cut.
    pub fn new(hash: Keyed) -> Self {
        Self {
            hash,
            symbols: Symbols::new(hash),
            index: Index::new(hash),
            head: Vec::new(),
            last: Vec::new(),
            blocks: Vec::new(),
            logs: LogStore::default(),
            dir: None,
            manifest: Manifest::default(),
            cutoff: i64::MIN,
            logs_cutoff: i64::MIN,
            max_series: 0,
            sessions: BTreeMap::new(),
            conf: Membership::default(),
            totals: Counts::default(),
            epoch: 0,
            applied: 0,
            pending: Vec::new(),
            pending_at: HashTable::new(),
        }
    }

    /// Opens the store in `dir` (made if missing): the state its manifest states, its files as it
    /// lists them; files it does not list (a cut a crash interrupted) are deleted.
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
        store.cutoff = manifest.cutoff;
        store.logs_cutoff = manifest.logs_cutoff;
        store.max_series = manifest.max_series;
        store.sessions = manifest.sessions.clone();
        store.conf = manifest.conf.clone();
        store.applied = manifest.durable.index;
        store.manifest = manifest;
        store.remove_orphans()?;
        store.rebuild_from_blocks()?;
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

    /// The index from the blocks: every series with a sample at or after the cutoff, its newest
    /// sample's time the newest its blocks hold. A series the cutoff took holds no such sample (it
    /// went because its newest was older), so this is the index every replica holds at `durable`.
    fn rebuild_from_blocks(&mut self) -> Result<(), StoreError> {
        let mut symbols = Symbols::new(self.hash);
        let mut index = Index::new(self.hash);
        let mut last: Vec<i64> = Vec::new();
        for b in &self.blocks {
            for (i, s) in b.block.series().enumerate() {
                let (fp, kind, key) = s?;
                let Some((_, newest)) = b.block.span(i) else { continue };
                if newest < self.cutoff {
                    continue;
                }
                match index.define(&mut symbols, fp, key, kind)? {
                    Defined::New(_) => last.push(newest),
                    Defined::Known(id) => {
                        if let Some(l) = last.get_mut(id as usize) {
                            *l = (*l).max(newest);
                        }
                    }
                }
            }
        }
        self.head = (0..last.len()).map(|_| HeadSeries::default()).collect();
        self.symbols = symbols;
        self.index = index;
        self.last = last;
        Ok(())
    }

    /// Keeps the series `keep` says, with their heads; the rest leave the index.
    fn keep_series(&mut self, keep: &[bool]) -> Result<u64, IndexError> {
        let mut symbols = Symbols::new(self.hash);
        let mut index = Index::new(self.hash);
        let mut kept: Vec<u32> = Vec::new();
        for id in 0..self.index.len() as u32 {
            if !keep.get(id as usize).copied().unwrap_or(false) {
                continue;
            }
            let (Some(fp), Some(info), Some(key)) = (self.index.fp(id), self.index.info(id), self.index.key(id)) else { continue };
            index.define(&mut symbols, fp, key, info.kind)?;
            kept.push(id);
        }
        let dropped = (self.index.len() - kept.len()) as u64;
        let mut head = Vec::with_capacity(kept.len());
        let mut last = Vec::with_capacity(kept.len());
        for &id in &kept {
            head.push(self.head.get_mut(id as usize).map(std::mem::take).unwrap_or_default());
            last.push(self.last.get(id as usize).copied().unwrap_or(i64::MIN));
        }
        self.symbols = symbols;
        self.index = index;
        self.head = head;
        self.last = last;
        Ok(dropped)
    }

    /// The point of the group's log everything is durable up to.
    pub fn durable(&self) -> Point {
        self.manifest.durable
    }

    /// No sample older is kept or read (ms), as of the last entry applied.
    pub fn cutoff(&self) -> i64 {
        self.cutoff
    }

    /// No log record older is kept or read (ms), as of the last entry applied.
    pub fn logs_cutoff(&self) -> i64 {
        self.logs_cutoff
    }

    pub fn max_series(&self) -> u64 {
        self.max_series
    }

    pub fn sessions(&self) -> &BTreeMap<u64, Session> {
        &self.sessions
    }

    /// The group's configuration as of the last entry applied.
    pub fn membership(&self) -> &Membership {
        &self.conf
    }

    /// The group's configuration at `durable`.
    pub fn durable_membership(&self) -> &Membership {
        &self.manifest.conf
    }

    /// A change of the group's configuration, applied.
    pub fn set_membership(&mut self, conf: Membership) {
        self.conf = conf;
    }

    /// The last entry applied was `index` (the machine says so of every entry, a change of
    /// configuration's and a leader's empty one's too).
    pub fn note_applied(&mut self, index: u64) {
        self.applied = index;
    }

    /// The last entry applied, as the machine said: what a digest is of.
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Applies the command an entry at `index` holds.
    pub fn apply_entry(&mut self, index: u64, data: &[u8]) -> Result<Answer, Newer> {
        self.applied = index;
        let c = match command::decode(data) {
            Ok(c) => c,
            Err(CommandError::Unknown(tag)) => return Err(Newer::Command(tag)),
            Err(e) => return Ok(Answer::Malformed(e)),
        };
        Ok(match c {
            Command::Empty => Answer::Empty,
            Command::Write { sender, seq, at, batch } => {
                if batch.first().is_some_and(|&v| v != crate::batch::VERSION) {
                    return Err(Newer::Batch(batch.first().copied().unwrap_or(0)));
                }
                Answer::Write { sender, seq, outcome: self.write(index, sender, seq, at, batch) }
            }
            Command::Retain { metrics, logs, .. } => {
                let series_dropped = self.retain_logical(metrics, logs);
                Answer::Retain { cutoff: self.cutoff, logs_cutoff: self.logs_cutoff, series_dropped }
            }
            Command::Limits { max_series } => {
                self.max_series = max_series;
                Answer::Limits { max_series }
            }
        })
    }

    /// A sender's write at `index`: applied unless its sender's session holds it or a later one.
    fn write(&mut self, index: u64, sender: u64, seq: u64, at: i64, batch: &[u8]) -> Written {
        if self.sessions.get(&sender).is_some_and(|s| seq <= s.seq) {
            return Written::Duplicate;
        }
        let applied = self.apply(batch);
        self.sessions.insert(sender, Session { seq, at, index });
        if self.sessions.len() > MAX_SESSIONS
            && let Some(oldest) = self.sessions.iter().min_by_key(|(_, s)| s.index).map(|(&k, _)| k)
        {
            self.sessions.remove(&oldest);
        }
        match applied {
            Ok(a) => Written::Applied(a),
            Err(e) => Written::Failed(e),
        }
    }

    /// Moves the cutoffs on to `metrics` and `logs` (never back): series whose newest sample is
    /// older than `metrics` leave the index, and sessions whose last write is older than the later
    /// of the two go. The files are left to [`Store::retain_files`], once a cut has made these
    /// cutoffs durable.
    pub fn retain_logical(&mut self, metrics: i64, logs: i64) -> u64 {
        self.logs_cutoff = self.logs_cutoff.max(logs);
        let sessions = self.cutoff.max(metrics).max(self.logs_cutoff);
        self.sessions.retain(|_, s| s.at >= sessions);
        if metrics <= self.cutoff {
            return 0;
        }
        let cutoff = metrics;
        self.cutoff = cutoff;
        let keep: Vec<bool> = self.last.iter().map(|&l| l >= cutoff).collect();
        if keep.iter().all(|&k| k) {
            return 0;
        }
        // Defining series that were defined already cannot fail; were it to, every replica would
        // fail alike, and the series stay.
        self.keep_series(&keep).unwrap_or(0)
    }

    /// Applies a write batch, item by item.
    pub fn apply(&mut self, batch: &[u8]) -> Result<Applied, ApplyError> {
        let mut done = Applied::default();
        self.pending.clear();
        self.pending_at.clear();
        let result = self.apply_items(batch, &mut done);
        self.pending.clear();
        self.pending_at.clear();
        self.totals += done.counts;
        result.map(|()| done)
    }

    fn find_pending(&self, fp: u64) -> Option<Pending> {
        self.pending_at.find(fp, |&i| self.pending.get(i as usize).is_some_and(|p| p.fp == fp)).and_then(|&i| self.pending.get(i as usize)).copied()
    }

    fn apply_items(&mut self, batch: &[u8], done: &mut Applied) -> Result<(), ApplyError> {
        let mut r = Reader::new(batch)?;
        while let Some(item) = r.next_item()? {
            match item {
                Item::Def { fp, kind, key } => {
                    if self.index.by_fp(fp).is_some() {
                        match self.index.define(&mut self.symbols, fp, key, kind) {
                            Ok(_) => {}
                            Err(IndexError::Collision(fp) | IndexError::WrongFingerprint(fp)) => done.refused.push(fp),
                            Err(e) => return Err(e.into()),
                        }
                    } else if self.find_pending(fp).is_none()
                        && let Some((start, end)) = place_in(batch, key)
                    {
                        let at = self.pending.len() as u32;
                        self.pending.push(Pending { fp, kind, start, end });
                        let pending = &self.pending;
                        self.pending_at.insert_unique(fp, at, |&i| pending.get(i as usize).map_or(0, |p| p.fp));
                    }
                }
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
                    let known = self.index.by_fp(fp);
                    let pending = if known.is_none() {
                        match self.find_pending(fp) {
                            Some(p) => Some(p),
                            None => {
                                done.unknown.push(fp);
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    done.counts.runs += 1;
                    let cutoff = self.cutoff;
                    let mut d = samples.iter();
                    // The first sample kept; a series the batch defined is defined with it.
                    let first = loop {
                        match d.next_sample()? {
                            None => break None,
                            Some((t, _)) if t < cutoff => done.counts.expired += 1,
                            Some(s) => break Some(s),
                        }
                    };
                    let Some(first) = first else { continue };
                    let id = match (known, pending) {
                        (Some(id), _) => id,
                        (None, Some(p)) => {
                            if self.max_series > 0 && self.index.len() as u64 >= self.max_series {
                                done.over_limit.push(fp);
                                continue;
                            }
                            let Some(key) = batch.get(p.start..p.end) else { continue };
                            match self.index.define(&mut self.symbols, fp, key, p.kind) {
                                Ok(Defined::New(id)) => {
                                    done.counts.new_series += 1;
                                    self.head.push(HeadSeries::default());
                                    self.last.push(i64::MIN);
                                    id
                                }
                                Ok(Defined::Known(id)) => id,
                                Err(IndexError::Collision(fp) | IndexError::WrongFingerprint(fp)) => {
                                    done.refused.push(fp);
                                    continue;
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        (None, None) => continue,
                    };
                    let (Some(series), Some(last)) = (self.head.get_mut(id as usize), self.last.get_mut(id as usize)) else { continue };
                    let mut next = Some(first);
                    while let Some((t, v)) = next {
                        if t < cutoff {
                            done.counts.expired += 1;
                        } else {
                            match series.append(t, v)? {
                                Appended::InOrder => {}
                                Appended::Late => done.counts.late += 1,
                                Appended::Duplicate => done.counts.duplicates += 1,
                            }
                            *last = (*last).max(t);
                            done.counts.samples += 1;
                        }
                        next = d.next_sample()?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes the head into files, durably, and records that everything up to `at` (the log point
    /// of the last entry applied) is durable, with the replicated state there; the head is emptied.
    /// A head with nothing in it writes no file and still records `at`. If a file cannot be
    /// written, the head is kept.
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
        manifest.cutoff = self.cutoff;
        manifest.logs_cutoff = self.logs_cutoff;
        manifest.max_series = self.max_series;
        manifest.sessions.clone_from(&self.sessions);
        manifest.conf.clone_from(&self.conf);
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
            done.bytes_written += block.bytes;
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
    /// O(log n) blocks of n samples and a sample is rewritten O(log n) times. Here, under the
    /// caller's hold of the store; [`Store::plan_merge`] does it a merge at a time apart.
    pub fn compact(&mut self) -> Result<Maintained, StoreError> {
        let mut done = Maintained::default();
        while let Some(plan) = self.plan_merge() {
            done += self.commit(plan.build()?)?;
        }
        Ok(done)
    }

    /// The merge the logarithmic method calls for now, if any ([`Store::compact`]): its file's
    /// name reserved, to be built apart from the store ([`Rewrite::build`]) and taken in after
    /// ([`Store::commit`]).
    pub fn plan_merge(&mut self) -> Option<Rewrite> {
        let dir = self.dir.clone()?;
        let [.., older, newer] = self.blocks.as_slice() else { return None };
        if newer.block.bytes.saturating_mul(2) < older.block.bytes {
            return None;
        }
        let from = vec![(older.seq, older.file.clone()), (newer.seq, newer.file.clone())];
        let seq = newer.seq;
        Some(self.reserve(dir, from, seq))
    }

    /// The rewrite retention calls for now, if any: a block more than half older than the durable
    /// cutoff, rewritten without its older samples.
    pub fn plan_retention(&mut self) -> Option<Rewrite> {
        let dir = self.dir.clone()?;
        let cutoff = self.manifest.cutoff;
        let b = self.blocks.iter().find(|b| {
            let span = i128::from(b.block.max_ts) - i128::from(b.block.min_ts);
            let expired = i128::from(cutoff) - i128::from(b.block.min_ts);
            span > 0 && expired * 2 > span
        })?;
        let (from, seq) = (vec![(b.seq, b.file.clone())], b.seq);
        Some(self.reserve(dir, from, seq))
    }

    fn reserve(&mut self, dir: PathBuf, from: Vec<(u64, String)>, seq: u64) -> Rewrite {
        let file = manifest::block_file(self.manifest.next_seq);
        self.manifest.next_seq += 1;
        Rewrite { dir, from, keep_from: self.manifest.cutoff, file, seq, epoch: self.epoch }
    }

    /// Takes a built rewrite in: its block in its inputs' place, the manifest saved, the inputs
    /// deleted, if the store still holds every input; else (an install replaced the store, or the
    /// inputs went) its file is deleted. Under the caller's hold of the store, and quick: the
    /// building was done apart.
    pub fn commit(&mut self, built: Built) -> Result<Maintained, StoreError> {
        let Built { rewrite: r, block } = built;
        let held = r.epoch == self.epoch && r.from.iter().all(|(seq, f)| self.blocks.iter().any(|b| b.seq == *seq && b.file == *f));
        let path = r.dir.join(&r.file);
        if !held {
            drop(block);
            remove_if_there(&path)?;
            return Ok(Maintained::default());
        }
        let bytes = block.bytes;
        let empty = block.is_empty();
        let mut manifest = self.manifest.clone();
        manifest.blocks.retain(|e| !r.from.iter().any(|(_, f)| *f == e.file));
        if !empty {
            manifest.blocks.push(BlockEntry { seq: r.seq, file: r.file.clone() });
            manifest.blocks.sort_by_key(|e| e.seq);
        }
        manifest::sync_dir(&r.dir).map_err(io_err(&r.dir))?;
        manifest.save(&r.dir)?;
        // The manifest's numbering went on while this was built: keep what this store reserved.
        manifest.next_seq = manifest.next_seq.max(self.manifest.next_seq);
        self.manifest = manifest;
        self.blocks.retain(|b| !r.from.iter().any(|(_, f)| *f == b.file));
        if empty {
            drop(block);
            remove_if_there(&path)?;
        } else {
            let at = self.blocks.partition_point(|x| x.seq < r.seq);
            self.blocks.insert(at, OpenBlock { seq: r.seq, file: r.file.clone(), block });
        }
        for (_, f) in &r.from {
            remove_if_there(&r.dir.join(f))?;
        }
        Ok(Maintained { blocks_written: u64::from(!empty), blocks_removed: r.from.len() as u64, bytes_written: if empty { 0 } else { bytes }, series_dropped: 0 })
    }

    /// Deletes the files wholly older than the durable cutoff (quick: no file is rewritten). The
    /// index is the cutoff's already ([`Store::retain_logical`]); only the files change.
    pub fn drop_expired(&mut self) -> Result<Maintained, StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        let cutoff = self.manifest.cutoff;
        let mut done = Maintained::default();
        let logs_gone = self.logs.retain(self.manifest.logs_cutoff.saturating_mul(1_000_000));
        let gone: Vec<String> = self.blocks.iter().filter(|b| b.block.max_ts < cutoff).map(|b| b.file.clone()).collect();
        if logs_gone.is_empty() && gone.is_empty() {
            return Ok(done);
        }
        let mut manifest = self.manifest.clone();
        manifest.logs.retain(|e| !logs_gone.contains(&e.file));
        manifest.blocks.retain(|e| !gone.contains(&e.file));
        manifest.save(&dir)?;
        self.manifest = manifest;
        self.blocks.retain(|b| !gone.contains(&b.file));
        for f in logs_gone.iter().chain(&gone) {
            remove_if_there(&dir.join(f))?;
        }
        done.blocks_removed = (logs_gone.len() + gone.len()) as u64;
        Ok(done)
    }

    /// Deletes what the durable cutoff left behind: files wholly older are removed, and a block
    /// partly older is rewritten without its older samples once that is more than half of it.
    pub fn retain_files(&mut self) -> Result<Maintained, StoreError> {
        let mut done = self.drop_expired()?;
        while let Some(plan) = self.plan_retention() {
            done += self.commit(plan.build()?)?;
        }
        Ok(done)
    }
    /// The image of what is durable: its replicated state and its files, in write order.
    pub fn image(&self) -> Image {
        let mut files: Vec<ImageFile> = Vec::with_capacity(self.blocks.len() + self.logs.files.len());
        for b in &self.blocks {
            files.push(ImageFile { kind: FileKind::Block, seq: b.seq, name: b.file.clone(), bytes: b.block.bytes, crc: b.block.crc });
        }
        for l in &self.logs.files {
            files.push(ImageFile { kind: FileKind::Logs, seq: l.seq, name: l.file.clone(), bytes: l.log.bytes, crc: l.log.crc });
        }
        files.sort_by_key(|f| f.seq);
        Image { cutoff: self.manifest.cutoff, logs_cutoff: self.manifest.logs_cutoff, max_series: self.manifest.max_series, sessions: self.manifest.sessions.clone(), files }
    }

    /// Bytes of the files the durable state is in.
    pub fn image_bytes(&self) -> u64 {
        self.disk_bytes() + self.log_bytes()
    }

    /// Links the files of what is durable into a snapshot of their own, `snaps/<index>`, named by
    /// the durable point, so that compaction and retention cannot take them from a member still
    /// fetching them (hard links: the store's names go, the files stay while a snapshot names
    /// them; copied where the file system has no links). The newest [`KEEP_SNAPSHOTS`] are kept.
    pub fn link_snapshot(&self, snaps: &Path) -> Result<PathBuf, StoreError> {
        let dir = self.dir.as_ref().ok_or(StoreError::NoDir)?;
        let snap = snaps.join(snapshot_name(self.manifest.durable));
        if snap.exists() {
            std::fs::remove_dir_all(&snap).map_err(io_err(&snap))?;
        }
        std::fs::create_dir_all(&snap).map_err(io_err(&snap))?;
        for e in self.manifest.blocks.iter().chain(&self.manifest.logs) {
            let (from, to) = (dir.join(&e.file), snap.join(&e.file));
            if std::fs::hard_link(&from, &to).is_err() {
                std::fs::copy(&from, &to).map_err(io_err(&to))?;
            }
        }
        let mut all: Vec<(String, PathBuf)> = std::fs::read_dir(snaps)
            .map_err(io_err(snaps))?
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        // Names sort as their points do.
        all.sort();
        let excess = all.len().saturating_sub(KEEP_SNAPSHOTS);
        for (_, path) in all.into_iter().take(excess) {
            std::fs::remove_dir_all(&path).map_err(io_err(&path))?;
        }
        Ok(snap)
    }

    /// Replaces everything with `image`, of `at` under `conf`, its files staged in `staging` under
    /// the names the image gives them: they are moved in under names of this store's, the manifest
    /// is saved (the point the install turns on: a crash before it leaves the store as it was, and
    /// the moved files orphans, deleted at open), the old files are deleted, and the store is
    /// opened again. Durable before it returns.
    pub fn install(&mut self, staging: &Path, image: &Image, at: Point, conf: Membership) -> Result<(), StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        for f in &image.files {
            let p = staging.join(&f.name);
            let (bytes, crc) = match f.kind {
                FileKind::Block => Block::open(&p).map(|b| (b.bytes, b.crc)).map_err(|_| StoreError::Staged(f.name.clone()))?,
                FileKind::Logs => LogFile::open(&p).map(|l| (l.bytes, l.crc)).map_err(|_| StoreError::Staged(f.name.clone()))?,
            };
            if bytes != f.bytes || crc != f.crc {
                return Err(StoreError::Staged(f.name.clone()));
            }
        }
        let mut manifest = Manifest { durable: at, next_seq: self.manifest.next_seq, cutoff: image.cutoff, logs_cutoff: image.logs_cutoff, max_series: image.max_series, conf, sessions: image.sessions.clone(), blocks: Vec::new(), logs: Vec::new() };
        let mut files: Vec<&ImageFile> = image.files.iter().collect();
        files.sort_by_key(|f| f.seq);
        for f in files {
            let seq = manifest.next_seq;
            manifest.next_seq += 1;
            let name = match f.kind {
                FileKind::Block => manifest::block_file(seq),
                FileKind::Logs => manifest::logs_file(seq),
            };
            let to = dir.join(&name);
            std::fs::rename(staging.join(&f.name), &to).map_err(io_err(&to))?;
            match f.kind {
                FileKind::Block => manifest.blocks.push(BlockEntry { seq, file: name }),
                FileKind::Logs => manifest.logs.push(BlockEntry { seq, file: name }),
            }
        }
        manifest::sync_dir(&dir).map_err(io_err(&dir))?;
        manifest.save(&dir)?;
        for e in self.manifest.blocks.iter().chain(&self.manifest.logs) {
            remove_if_there(&dir.join(&e.file))?;
        }
        let epoch = self.epoch + 1;
        *self = Self::open(&dir, self.hash)?;
        self.epoch = epoch;
        Ok(())
    }

    /// The ids of the series `sel` selects, sorted.
    pub fn select(&self, sel: &Selector, out: &mut Vec<u32>) {
        self.index.select(&self.symbols, sel, out);
    }

    pub fn series(&self, id: u32) -> Option<SeriesRef<'_>> {
        self.index.info(id).map(|info| SeriesRef { store: self, id, info })
    }

    /// Series `id`'s fingerprint: the same on every replica, where its id is this one's own.
    pub fn fingerprint(&self, id: u32) -> Option<u64> {
        self.index.fp(id)
    }

    /// Appends series `id`'s samples in `[from, to]` (none older than the cutoff), in time order;
    /// at a time, the latest write (the head over the blocks, a later block over an earlier).
    pub fn read(&self, id: u32, from: i64, to: i64, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), StoreError> {
        self.read_with(id, from, to, &mut Scratch::default(), ts, values)
    }

    /// [`Store::read`] with buffers kept across calls: a query reading many series allocates
    /// nothing a series once they have grown.
    pub fn read_with(&self, id: u32, from: i64, to: i64, scratch: &mut Scratch, ts: &mut Vec<i64>, values: &mut Vec<f64>) -> Result<(), StoreError> {
        let from = from.max(self.cutoff);
        if to < from {
            return Ok(());
        }
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

    /// The newest `limit` log records in `[from, to]` (ns, none older than the logs' cutoff) that
    /// `search` matches.
    pub fn search_logs(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, StoreError> {
        Ok(self.logs.search(search, from.max(self.logs_cutoff.saturating_mul(1_000_000)), to, limit)?)
    }

    /// Log records in `[from, to]` (ns, none older than the logs' cutoff) that `search` matches,
    /// grouped by `by` and measured (log analytics).
    pub fn aggregate_logs(&self, search: &Search, from: i64, to: i64, by: &[crate::logs::store::Facet], measure: &crate::logs::store::Measure) -> Result<crate::logs::store::Aggregated, crate::logs::store::AggregateError> {
        self.logs.aggregate(search, from.max(self.logs_cutoff.saturating_mul(1_000_000)), to, by, measure)
    }

    /// Bytes of logs on disk.
    pub fn log_bytes(&self) -> u64 {
        self.logs.disk_bytes()
    }

    /// Every metric's name, sorted.
    pub fn metric_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.index.metrics().iter().map(|&m| self.symbols.get(m)).collect();
        names.sort_unstable();
        names
    }

    /// Every value of tag `key`, sorted.
    pub fn tag_values(&self, key: &str) -> Vec<&str> {
        let mut values: Vec<&str> = self.index.tag_values(&self.symbols, key).into_iter().map(|s| self.symbols.get(s)).collect();
        values.sort_unstable();
        values
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
            samples: self.head.iter().map(HeadSeries::heap_bytes).sum::<usize>() + self.head.capacity() * std::mem::size_of::<HeadSeries>() + self.last.capacity() * 8,
        }
    }

    pub fn kind_conflicts(&self) -> u64 {
        self.index.kind_conflicts
    }

    /// A digest of what this replica holds, the same on every replica that applied the same
    /// entries however its files fell: of every series (its fingerprint and newest sample's
    /// time), the cutoffs, the bound and the sessions, and with `full` of every sample and every
    /// log record too (a read of everything: asked for, not kept). A sum of mixed terms, so the
    /// order series are visited in, each replica's own, does not count.
    pub fn digest(&self, full: bool) -> Result<u64, StoreError> {
        let mix = |x: u64| {
            // SplitMix64's finaliser (Steele, Lea and Flood 2014): every input bit moves every
            // output bit, so terms that differ anywhere sum apart.
            let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        let mut d = mix(self.cutoff as u64).wrapping_add(mix(self.logs_cutoff as u64 ^ 0x1)).wrapping_add(mix(self.max_series ^ 0x2));
        for (sender, s) in &self.sessions {
            d = d.wrapping_add(mix(mix(*sender) ^ s.seq));
        }
        let mut scratch = Scratch::default();
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        for id in 0..self.index.len() as u32 {
            let Some(fp) = self.index.fp(id) else { continue };
            let last = self.last.get(id as usize).copied().unwrap_or(i64::MIN);
            let mut term = mix(fp ^ mix(last as u64));
            if full {
                ts.clear();
                vs.clear();
                self.read_with(id, i64::MIN, i64::MAX, &mut scratch, &mut ts, &mut vs)?;
                for (&t, &v) in ts.iter().zip(&vs) {
                    term = mix(term ^ mix(t as u64) ^ v.to_bits());
                }
            }
            d = d.wrapping_add(term);
        }
        if full {
            let all = crate::logs::search::Search::all();
            for r in self.search_logs(&all, i64::MIN, i64::MAX, usize::MAX)?.records {
                let mut row = Vec::new();
                crate::logs::record::put_row(&mut row, &r, 0);
                d = d.wrapping_add(mix(crate::crc32c::checksum(&row) as u64 ^ mix(r.ts as u64)));
            }
        }
        Ok(d)
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// A snapshot's directory name for a durable point: names sort as points do.
pub fn snapshot_name(at: Point) -> String {
    format!("{:020}-{:020}", at.index, at.term)
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
mod tests;
