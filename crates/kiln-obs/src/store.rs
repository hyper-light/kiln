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
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use hashbrown::HashTable;

use crate::batch::{BatchError, Item, Kind, Reader};
use crate::block::{self, Block, BlockError, SeriesData};
use crate::command::{self, Command, CommandError};
use crate::gorilla::{Chunk, ChunkRef, DecodeError, Encoder};
use crate::hash::Keyed;
use crate::head::{Appended, CHUNK_SAMPLES, HeadSeries};
use crate::image::{FileKind, Image, ImageFile};
use crate::index::{Defined, Index, IndexError, Selector, SeriesInfo};
use crate::logs::file::LogFile;
use crate::logs::metrics::{BatchWindows, Touched};
use crate::logs::search::Search;
use crate::logs::store::{Found, HeldFile, LogStore, LogStoreError};
use crate::logs::tail::Feed;
use crate::manifest::{self, BlockEntry, Manifest, ManifestError, Membership, Point, Session};
use crate::rollup::{self, Col, Rolled};
use crate::symbols::{Sym, Symbols};

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
    #[error("the store changed while its cut was written")]
    Moved,
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
    /// Samples more than [`AHEAD_MS`] past their write's time, and log records more than
    /// [`LOGS_AHEAD_MS`]: dropped (a sender's clock, or its data, is wrong).
    pub ahead: u64,
    pub logs_ahead: u64,
    /// Spans, and spans more than [`LOGS_AHEAD_MS`] ahead (dropped).
    pub spans: u64,
    pub spans_ahead: u64,
}

/// How far past its write's time a sample may be: ten minutes, as far as Datadog's intake takes a
/// metric's point ahead. The write's time is its leader's clock (the leader stamps every write it
/// proposes), so every replica judges alike.
pub const AHEAD_MS: i64 = 10 * 60_000;
/// How far ahead a log record may be: two hours, Datadog's for log events.
pub const LOGS_AHEAD_MS: i64 = 2 * 3_600_000;

impl std::ops::AddAssign for Counts {
    fn add_assign(&mut self, o: Self) {
        self.runs += o.runs;
        self.new_series += o.new_series;
        self.samples += o.samples;
        self.late += o.late;
        self.duplicates += o.duplicates;
        self.expired += o.expired;
        self.logs += o.logs;
        self.ahead += o.ahead;
        self.logs_ahead += o.logs_ahead;
        self.spans += o.spans;
        self.spans_ahead += o.spans_ahead;
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
    RetainSpans { spans_cutoff: i64 },
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
    /// Samples applied since the store opened, by metric (its symbol's number): what each metric
    /// costs, as a summary says it ([`Store::metric_summaries`]). This process's alone, not
    /// replicated state.
    metric_samples: Vec<u64>,
    blocks: Vec<OpenBlock>,
    logs: LogStore,
    /// Spans: records of a store of their own (`crate::otlp::traces`), kept for their own time.
    spans: LogStore,
    dir: Option<PathBuf>,
    /// What is durable: the files, and the replicated state at `durable`.
    manifest: Manifest,
    /// The replicated state as of the last entry applied.
    cutoff: i64,
    logs_cutoff: i64,
    spans_cutoff: i64,
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
    /// Who follows the log records applied (live tail), and whether the entry being applied is
    /// new to them.
    feed: Option<Arc<Feed>>,
    publishing: bool,
    /// Who is told the records' times applied (metrics from logs count those windows again), and
    /// the spans' (metrics from spans).
    touched: Option<Arc<Touched>>,
    spans_touched: Option<Arc<Touched>>,
    /// Whether [`Store::read_rolled`] reads rollups (always, but to measure or check against them).
    read_rollups: bool,
    /// Held by whoever changes the manifest (a cut, a merge taken in, files expired, an install)
    /// from reading the store to taking the change in: they write their files under a shared hold
    /// of the store (or none), one at a time. Taken before the store's own lock, never after.
    manifest_lock: Arc<Mutex<()>>,
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

/// What a metric is and costs: its series (its cardinality), by kind; each tag key's values and
/// the series carrying it, the most values first, with the values of most series; the samples
/// applied since the store opened; its newest sample's time (ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricSummary {
    pub name: String,
    pub series: usize,
    pub kinds: Vec<(Kind, usize)>,
    pub tags: Vec<TagSummary>,
    pub samples: u64,
    pub last_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSummary {
    pub key: String,
    pub values: usize,
    pub series: usize,
    pub top: Vec<(String, usize)>,
}

/// Files wholly older than the durable cutoff, and the manifest without them.
#[derive(Debug)]
struct Expired {
    dir: PathBuf,
    manifest: Manifest,
    blocks: Vec<String>,
    logs: Vec<String>,
    spans: Vec<String>,
}

impl Expired {
    fn files(&self) -> Vec<PathBuf> {
        self.logs.iter().chain(&self.spans).chain(&self.blocks).map(|f| self.dir.join(f)).collect()
    }
}

/// A cut written ([`Store::write_cut`]), to be taken in ([`Store::install_cut`]).
#[derive(Debug)]
pub struct CutWritten {
    /// The store as it was written from: what was applied, and its installs.
    applied: u64,
    epoch: u64,
    manifest: Manifest,
    block: Option<OpenBlock>,
    logs: Option<HeldFile>,
    spans: Option<HeldFile>,
    /// The series whose samples the block holds.
    series: Vec<u32>,
    done: Maintained,
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
        // Every series of every input, by fingerprint; of one, the newest input's kind and key.
        let mut keys: Vec<(u64, Kind, Vec<u8>)> = Vec::new();
        for blk in blocks.iter().rev() {
            let k = blk.keys()?;
            for s in k.iter() {
                let (fp, kind, key) = s?;
                keys.push((fp, kind, key.to_vec()));
            }
        }
        // Stable: of one fingerprint, the newest input's first.
        keys.sort_by_key(|k| k.0);
        keys.dedup_by_key(|k| k.0);
        // Written as it is read, a series at a time: what a merge holds is a series, not a block.
        // Each series' rollups (`crate::rollup`) are its input's when it is copied whole and its
        // input has them; else made here, from its samples.
        let mut w = block::Writer::create(&self.dir.join(&self.file))?;
        let mut buf = Vec::new();
        let mut regions: Vec<Vec<u8>> = blocks.iter().map(|_| Vec::new()).collect();
        let (mut ts, mut vs) = (Vec::new(), Vec::new());
        let (mut builder, mut rollups, mut held) = (rollup::Builder::default(), Vec::new(), Vec::new());
        let mut holders: Vec<(&Block, usize, (i64, i64))> = Vec::with_capacity(blocks.len());
        for (fp, kind, key) in &keys {
            holders.clear();
            holders.extend(blocks.iter().filter_map(|blk| {
                let i = blk.find(*fp)?;
                Some((blk, i, blk.span(i)?))
            }));
            let uncut = holders.iter().all(|h| h.2.0 >= self.keep_from);
            // One input holds it and the cutoff takes none of it: its region as it is, nothing
            // decoded or encoded.
            if let [(blk, i, span)] = holders.as_slice()
                && uncut
                && blk.format() == block::FORMAT
            {
                let crc = blk.load(*i, &mut buf)?;
                let had = blk.rollups(*i, &mut held)?;
                if !had.is_empty() && blk.kind(*i) == Some(*kind) {
                    w.region(*fp, *kind, key, &buf, crc, *span, had)?;
                    continue;
                }
                let mut bytes = 0usize;
                builder.begin();
                for c in blk.chunks_in(*i, &buf, i64::MIN, i64::MAX)? {
                    let (_, _, count, chunk) = c?;
                    stream(&mut builder, chunk, count, blk.path())?;
                    bytes += chunk.len();
                }
                builder.finish(*kind, bytes, &mut rollups);
                w.region(*fp, *kind, key, &buf, crc, *span, &rollups)?;
                continue;
            }
            // Several, each input's after the one before (the samples came in order), none cut:
            // their chunks one after another, nothing decoded, so long as they are full enough
            // (half a chunk's samples each on average; fewer, and they are packed anew below).
            if uncut && holders.windows(2).all(|p| matches!(p, [a, b] if a.2.1 < b.2.0)) {
                for ((blk, i, _), region) in holders.iter().zip(regions.iter_mut()) {
                    blk.load(*i, region)?;
                }
                let mut chunks: Vec<(i64, i64, u32, &[u8])> = Vec::new();
                for ((blk, i, _), region) in holders.iter().zip(&regions) {
                    for c in blk.chunks_in(*i, region, i64::MIN, i64::MAX)? {
                        chunks.push(c?);
                    }
                }
                let samples: u64 = chunks.iter().map(|c| u64::from(c.2)).sum();
                if !chunks.is_empty() && samples / chunks.len() as u64 >= u64::from(CHUNK_SAMPLES / 2) {
                    builder.begin();
                    for ((blk, i, _), region) in holders.iter().zip(&regions) {
                        for c in blk.chunks_in(*i, region, i64::MIN, i64::MAX)? {
                            let (_, _, count, chunk) = c?;
                            stream(&mut builder, chunk, count, blk.path())?;
                        }
                    }
                    let bytes = chunks.iter().map(|c| c.3.len()).sum();
                    builder.finish(*kind, bytes, &mut rollups);
                    w.series(*fp, *kind, key, &chunks, &rollups)?;
                    continue;
                }
            }
            let mut parts: Vec<(Vec<i64>, Vec<f64>)> = Vec::with_capacity(holders.len());
            for (blk, i, _) in &holders {
                let (mut t, mut v) = (Vec::new(), Vec::new());
                blk.read(*i, self.keep_from, i64::MAX, &mut buf, &mut t, &mut v)?;
                parts.push((t, v));
            }
            ts.clear();
            vs.clear();
            if parts.len() == 1 {
                (ts, vs) = parts.pop().unwrap_or_default();
            } else {
                merge_parts(parts, &mut ts, &mut vs);
            }
            let chunks = rechunk(&ts, &vs);
            let refs: Vec<(i64, i64, u32, &[u8])> = chunks.iter().map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect();
            rollup::build(*kind, &ts, &vs, chunks.iter().map(|c| c.bytes.len()).sum(), &mut builder, &mut rollups);
            w.series(*fp, *kind, key, &refs, &rollups)?;
        }
        Ok(w.finish()?)
    }

    /// Bytes its inputs hold.
    pub fn input_bytes(&self) -> u64 {
        self.from.iter().filter_map(|(_, f)| std::fs::metadata(self.dir.join(f)).ok()).map(|m| m.len()).sum()
    }
}

/// A chunk's samples into a rollups' builder, as they decode: nothing of them held.
fn stream(b: &mut rollup::Builder, chunk: &[u8], count: u32, path: &Path) -> Result<(), StoreError> {
    let mut d = ChunkRef::new(chunk, count).iter();
    while let Some((t, v)) = d.next_sample().map_err(|e| BlockError::Decode(path.to_owned(), e))? {
        b.push(t, v);
    }
    Ok(())
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
            metric_samples: Vec::new(),
            blocks: Vec::new(),
            logs: LogStore::default(),
            spans: LogStore::spans(),
            dir: None,
            manifest: Manifest::default(),
            cutoff: i64::MIN,
            logs_cutoff: i64::MIN,
            spans_cutoff: i64::MIN,
            max_series: 0,
            sessions: BTreeMap::new(),
            conf: Membership::default(),
            totals: Counts::default(),
            epoch: 0,
            applied: 0,
            pending: Vec::new(),
            pending_at: HashTable::new(),
            feed: None,
            publishing: false,
            touched: None,
            spans_touched: None,
            read_rollups: true,
            manifest_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Log records applied from here on are published to `feed` (live tail, [`crate::logs::tail`]).
    pub fn set_feed(&mut self, feed: Option<Arc<Feed>>) {
        self.feed = feed;
    }

    /// The times of log records applied from here on are told to `touched` (metrics from logs,
    /// [`crate::logs::metrics`]).
    pub fn set_touched(&mut self, touched: Option<Arc<Touched>>) {
        self.touched = touched;
    }

    /// As [`Store::set_touched`], of spans (metrics from spans).
    pub fn set_spans_touched(&mut self, touched: Option<Arc<Touched>>) {
        self.spans_touched = touched;
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
            store.logs.files.push(HeldFile { seq: e.seq, file: e.file.clone(), log: Arc::new(log) });
        }
        for e in &manifest.spans {
            let log = LogFile::open(&dir.join(&e.file))?;
            store.spans.files.push(HeldFile { seq: e.seq, file: e.file.clone(), log: Arc::new(log) });
        }
        store.cutoff = manifest.cutoff;
        store.logs_cutoff = manifest.logs_cutoff;
        store.spans_cutoff = manifest.spans_cutoff;
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
            let listed = self.manifest.blocks.iter().chain(&self.manifest.logs).chain(&self.manifest.spans).any(|b| b.file == name);
            if ((name.ends_with(".block") || name.ends_with(".logs") || name.ends_with(".spans")) && !listed) || name == "MANIFEST.tmp" {
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
            let keys = b.block.keys()?;
            for (i, s) in keys.iter().enumerate() {
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
        // The metrics' counts carried over by name: the symbols are numbered anew.
        let mut counted = Vec::new();
        for (i, &n) in self.metric_samples.iter().enumerate() {
            if n == 0 {
                continue;
            }
            let Some(new) = u32::try_from(i).ok().and_then(|i| symbols.find(self.symbols.get(Sym(i)))) else { continue };
            let at = new.0 as usize;
            if counted.len() <= at {
                counted.resize(at + 1, 0);
            }
            if let Some(c) = counted.get_mut(at) {
                *c += n;
            }
        }
        self.metric_samples = counted;
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

    /// No span older is kept or read (ms), as of the last entry applied.
    pub fn spans_cutoff(&self) -> i64 {
        self.spans_cutoff
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
        self.publishing = self.feed.as_ref().is_some_and(|f| f.entry(index));
        let answer = self.apply_command(index, data);
        self.publishing = false;
        answer
    }

    fn apply_command(&mut self, index: u64, data: &[u8]) -> Result<Answer, Newer> {
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
            Command::RetainSpans { spans, .. } => {
                // Never back. Sessions age by the metrics' and the logs' cutoffs alone, as they did
                // before spans were kept.
                self.spans_cutoff = self.spans_cutoff.max(spans);
                Answer::RetainSpans { spans_cutoff: self.spans_cutoff }
            }
        })
    }

    /// A sender's write at `index`: applied unless its sender's session holds it or a later one.
    fn write(&mut self, index: u64, sender: u64, seq: u64, at: i64, batch: &[u8]) -> Written {
        if self.sessions.get(&sender).is_some_and(|s| seq <= s.seq) {
            return Written::Duplicate;
        }
        let applied = self.apply_within(batch, at.saturating_add(AHEAD_MS), at.saturating_add(LOGS_AHEAD_MS).saturating_mul(1_000_000));
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

    /// Applies a write batch, item by item, every sample's time taken.
    pub fn apply(&mut self, batch: &[u8]) -> Result<Applied, ApplyError> {
        self.apply_within(batch, i64::MAX, i64::MAX)
    }

    /// Applies a write batch, its samples no later than `horizon` (ms) and its log records no
    /// later than `logs_horizon` (ns): later ones are counted and dropped.
    pub fn apply_within(&mut self, batch: &[u8], horizon: i64, logs_horizon: i64) -> Result<Applied, ApplyError> {
        let mut done = Applied::default();
        self.pending.clear();
        self.pending_at.clear();
        // The batch's records one batch to metrics from logs, however many items hold them (a
        // write's segments): the windows they reach, and how late the newest of them came (an item
        // alone would say its own, and only an entry's first is taken); spans alike.
        let mut windows: [Option<BatchWindows>; 2] = [None, None];
        let result = self.apply_items(batch, &mut done, horizon, logs_horizon, &mut windows);
        let [logs, spans] = &windows;
        for (t, w) in [(&self.touched, logs), (&self.spans_touched, spans)] {
            if let (Some(t), Some(w)) = (t, w) {
                t.note(w, self.applied);
            }
        }
        self.pending.clear();
        self.pending_at.clear();
        self.totals += done.counts;
        result.map(|()| done)
    }

    fn find_pending(&self, fp: u64) -> Option<Pending> {
        self.pending_at.find(fp, |&i| self.pending.get(i as usize).is_some_and(|p| p.fp == fp)).and_then(|&i| self.pending.get(i as usize)).copied()
    }

    fn apply_items(&mut self, batch: &[u8], done: &mut Applied, horizon: i64, logs_horizon: i64, windows: &mut [Option<BatchWindows>; 2]) -> Result<(), ApplyError> {
        let [log_windows, span_windows] = windows;
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
                    let windows = if self.touched.is_some() { Some(log_windows.get_or_insert_with(BatchWindows::default)) } else { None };
                    let (raw, rows, ahead) = within_logs(raw, rows, logs_horizon, windows).map_err(|_| ApplyError::LogRows)?;
                    done.counts.logs_ahead += ahead;
                    if rows > 0 {
                        if self.publishing
                            && let Some(f) = self.feed.as_ref().filter(|f| f.following())
                        {
                            f.publish(&raw, rows);
                        }
                        self.logs.append(raw, rows).map_err(|e| ApplyError::Logs(e.to_string()))?;
                    }
                    done.counts.logs += u64::from(rows);
                }
                Item::Spans { rows, raw_len, packed } => {
                    let mut raw = Vec::with_capacity(raw_len);
                    crate::lz4::decompress(packed, &mut raw, raw_len).map_err(|_| ApplyError::LogRows)?;
                    if raw.len() != raw_len {
                        return Err(ApplyError::LogRows);
                    }
                    let windows = if self.spans_touched.is_some() { Some(span_windows.get_or_insert_with(BatchWindows::default)) } else { None };
                    let (raw, rows, ahead) = within_logs(raw, rows, logs_horizon, windows).map_err(|_| ApplyError::LogRows)?;
                    done.counts.spans_ahead += ahead;
                    if rows > 0 {
                        self.spans.append(raw, rows).map_err(|e| ApplyError::Logs(e.to_string()))?;
                    }
                    done.counts.spans += u64::from(rows);
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
                            Some((t, _)) if t > horizon => done.counts.ahead += 1,
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
                    let before = done.counts.samples;
                    let mut next = Some(first);
                    while let Some((t, v)) = next {
                        if t < cutoff {
                            done.counts.expired += 1;
                        } else if t > horizon {
                            done.counts.ahead += 1;
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
                    let taken = done.counts.samples - before;
                    if taken > 0
                        && let Some(info) = self.index.info(id)
                    {
                        let at = info.metric.0 as usize;
                        if self.metric_samples.len() <= at {
                            self.metric_samples.resize(at + 1, 0);
                        }
                        if let Some(c) = self.metric_samples.get_mut(at) {
                            *c += taken;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Who changes the manifest holds this, from reading the store to taking the change in, and
    /// takes it before the store's own lock.
    pub fn manifest_lock(&self) -> Arc<Mutex<()>> {
        self.manifest_lock.clone()
    }

    /// Writes the head into files, durably, and records that everything up to `at` (the log point
    /// of the last entry applied) is durable, with the replicated state there; the head is emptied.
    /// A head with nothing in it writes no file and still records `at`. If a file cannot be
    /// written, the head is kept (nothing of it was taken). Under the caller's hold of the store; a
    /// cut beside queries is [`Store::write_cut`] then [`Store::install_cut`].
    pub fn cut(&mut self, at: Point) -> Result<Maintained, StoreError> {
        let w = self.write_cut(at)?;
        self.install_cut(w)
    }

    /// A cut's files written, and the manifest naming them saved, from the store as it is (a
    /// shared hold: queries go on). Nothing of the store changes until [`Store::install_cut`]; the
    /// caller holds [`Store::manifest_lock`] from before this until after that, and nothing applies
    /// meanwhile (the group's thread is the one cutting).
    pub fn write_cut(&self, at: Point) -> Result<CutWritten, StoreError> {
        let dir = self.dir.clone().ok_or(StoreError::NoDir)?;
        let mut manifest = self.manifest.clone();
        manifest.durable = at;
        manifest.cutoff = self.cutoff;
        manifest.logs_cutoff = self.logs_cutoff;
        manifest.spans_cutoff = self.spans_cutoff;
        manifest.max_series = self.max_series;
        manifest.sessions.clone_from(&self.sessions);
        manifest.conf.clone_from(&self.conf);
        let mut w = CutWritten { applied: self.applied, epoch: self.epoch, manifest, block: None, logs: None, spans: None, series: Vec::new(), done: Maintained::default() };
        let mut written: Vec<PathBuf> = Vec::new();
        let made = (|| -> Result<(), StoreError> {
            // Logs first: a file of their own, listed with the blocks in the same manifest.
            if self.logs.unwritten_rows() > 0 {
                let seq = w.manifest.next_seq;
                let file = manifest::logs_file(seq);
                written.push(dir.join(&file));
                if let Some(log) = self.logs.write_unwritten(&dir.join(&file))? {
                    w.done.bytes_written += log.bytes;
                    w.manifest.next_seq = seq + 1;
                    w.manifest.logs.push(BlockEntry { seq, file: file.clone() });
                    w.logs = Some(HeldFile { seq, file, log: Arc::new(log) });
                }
            }
            if self.spans.unwritten_rows() > 0 {
                let seq = w.manifest.next_seq;
                let file = manifest::spans_file(seq);
                written.push(dir.join(&file));
                if let Some(log) = self.spans.write_unwritten(&dir.join(&file))? {
                    w.done.bytes_written += log.bytes;
                    w.manifest.next_seq = seq + 1;
                    w.manifest.spans.push(BlockEntry { seq, file: file.clone() });
                    w.spans = Some(HeldFile { seq, file, log: Arc::new(log) });
                }
            }
            let mut borrowed: Vec<(u32, &[Chunk])> = Vec::new();
            let mut made: Vec<(u32, Vec<Chunk>)> = Vec::new();
            for (id, h) in self.head.iter().enumerate() {
                if h.span().is_some() {
                    let (closed, new) = h.cut_chunks()?;
                    borrowed.push((id as u32, closed));
                    made.push((id as u32, new));
                }
            }
            if borrowed.is_empty() {
                return Ok(());
            }
            let seq = w.manifest.next_seq;
            let file = manifest::block_file(seq);
            let series: Vec<SeriesData<'_>> = borrowed
                .iter()
                .zip(&made)
                .filter_map(|((id, closed), (_, new))| {
                    let info = self.index.info(*id)?;
                    // A cut writes no rollups (it decodes nothing it need not): the first merge
                    // to take its block makes them.
                    let chunks = closed.iter().chain(new).map(|c| (c.first, c.last, c.count, c.bytes.as_slice())).collect();
                    Some(SeriesData { fp: self.index.fp(*id)?, kind: info.kind, key: self.index.key(*id)?, chunks, rollups: &[] })
                })
                .collect();
            written.push(dir.join(&file));
            let block = block::write(&dir.join(&file), series)?;
            w.series = borrowed.iter().map(|(id, _)| *id).collect();
            w.done.blocks_written = 1;
            w.done.bytes_written += block.bytes;
            w.manifest.next_seq = seq + 1;
            w.manifest.blocks.push(BlockEntry { seq, file: file.clone() });
            w.block = Some(OpenBlock { seq, file, block });
            Ok(())
        })()
        .and_then(|()| manifest::sync_dir(&dir).map_err(io_err(&dir)))
        .and_then(|()| w.manifest.save(&dir).map_err(StoreError::from));
        if let Err(e) = made {
            for p in &written {
                let _ = remove_if_there(p);
            }
            return Err(e);
        }
        Ok(w)
    }

    /// Takes in a cut [`Store::write_cut`] wrote (an exclusive hold, brief: no file is written):
    /// the head's samples and the logs' rows it wrote leave memory, its files are read from. Refused
    /// ([`StoreError::Moved`]) if anything was applied since: then its files are listed by a
    /// manifest that this store's next one replaces, and are deleted as the store next opens.
    pub fn install_cut(&mut self, w: CutWritten) -> Result<Maintained, StoreError> {
        if w.applied != self.applied || w.epoch != self.epoch {
            return Err(StoreError::Moved);
        }
        for id in &w.series {
            if let Some(h) = self.head.get_mut(*id as usize) {
                h.clear();
            }
        }
        self.manifest = w.manifest;
        if let Some(b) = w.block {
            self.blocks.push(b);
        }
        if let Some(l) = w.logs {
            self.logs.written(l);
        }
        if let Some(s) = w.spans {
            self.spans.written(s);
        }
        Ok(w.done)
    }

    /// Compacts blocks by the logarithmic method (Bentley and Saxe, "Decomposable searching
    /// problems I: static-to-dynamic transformation", J. Algorithms 1980): two neighbouring blocks
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
        // The method's invariant holds of every pair: each block more than twice the next newer.
        // The oldest pair that breaks it is merged, in its place (the newer's), so merges carry on
        // like a binary counter's, whatever small cut lands at the newest end meanwhile.
        let (older, newer) = self.blocks.windows(2).find_map(|w| match w {
            [older, newer] if newer.block.bytes.saturating_mul(2) >= older.block.bytes => Some((older, newer)),
            _ => None,
        })?;
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
    /// building was done apart ([`commit_shared`] saves the manifest and deletes the files apart
    /// too).
    pub fn commit(&mut self, built: Built) -> Result<Maintained, StoreError> {
        let Some(manifest) = self.commit_manifest(&built) else {
            let path = built.rewrite.dir.join(&built.rewrite.file);
            drop(built);
            remove_if_there(&path)?;
            return Ok(Maintained::default());
        };
        let dir = built.rewrite.dir.clone();
        manifest::sync_dir(&dir).map_err(io_err(&dir))?;
        manifest.save(&dir)?;
        let (done, gone) = self.take_commit(built, manifest)?;
        for p in gone {
            remove_if_there(&p)?;
        }
        Ok(done)
    }

    /// The manifest a rewrite's block makes, in its inputs' place: none if the store no longer
    /// holds every input (an install replaced it, or the inputs went).
    fn commit_manifest(&self, built: &Built) -> Option<Manifest> {
        let r = &built.rewrite;
        let held = r.epoch == self.epoch && r.from.iter().all(|(seq, f)| self.blocks.iter().any(|b| b.seq == *seq && b.file == *f));
        if !held {
            return None;
        }
        let mut manifest = self.manifest.clone();
        manifest.blocks.retain(|e| !r.from.iter().any(|(_, f)| *f == e.file));
        if !built.block.is_empty() {
            manifest.blocks.push(BlockEntry { seq: r.seq, file: r.file.clone() });
            manifest.blocks.sort_by_key(|e| e.seq);
        }
        Some(manifest)
    }

    /// A rewrite taken in, `saved` (its manifest) durable already: the files to delete after (its
    /// inputs', and its own if it holds nothing).
    fn take_commit(&mut self, built: Built, saved: Manifest) -> Result<(Maintained, Vec<PathBuf>), StoreError> {
        let Built { rewrite: r, block } = built;
        let held = r.epoch == self.epoch && r.from.iter().all(|(seq, f)| self.blocks.iter().any(|b| b.seq == *seq && b.file == *f));
        if !held {
            return Err(StoreError::Moved);
        }
        let bytes = block.bytes;
        let empty = block.is_empty();
        let mut manifest = saved;
        // The manifest's numbering went on while this was built: keep what this store reserved.
        manifest.next_seq = manifest.next_seq.max(self.manifest.next_seq);
        self.manifest = manifest;
        self.blocks.retain(|b| !r.from.iter().any(|(_, f)| *f == b.file));
        let mut gone: Vec<PathBuf> = r.from.iter().map(|(_, f)| r.dir.join(f)).collect();
        if empty {
            drop(block);
            gone.push(r.dir.join(&r.file));
        } else {
            let at = self.blocks.partition_point(|x| x.seq < r.seq);
            self.blocks.insert(at, OpenBlock { seq: r.seq, file: r.file.clone(), block });
        }
        Ok((Maintained { blocks_written: u64::from(!empty), blocks_removed: r.from.len() as u64, bytes_written: if empty { 0 } else { bytes }, series_dropped: 0 }, gone))
    }

    /// Deletes the files wholly older than the durable cutoff (quick: no file is rewritten). The
    /// index is the cutoff's already ([`Store::retain_logical`]); only the files change.
    pub fn drop_expired(&mut self) -> Result<Maintained, StoreError> {
        let Some(e) = self.expired() else { return Ok(Maintained::default()) };
        e.manifest.save(&e.dir)?;
        let done = self.take_expired(&e);
        for f in e.files() {
            remove_if_there(&f)?;
        }
        Ok(done)
    }

    /// The files wholly older than the durable cutoff, and the manifest without them; none if none.
    fn expired(&self) -> Option<Expired> {
        let dir = self.dir.clone()?;
        let cutoff = self.manifest.cutoff;
        let logs = self.logs.expired(self.manifest.logs_cutoff.saturating_mul(1_000_000));
        let spans = self.spans.expired(self.manifest.spans_cutoff.saturating_mul(1_000_000));
        let blocks: Vec<String> = self.blocks.iter().filter(|b| b.block.max_ts < cutoff).map(|b| b.file.clone()).collect();
        if logs.is_empty() && spans.is_empty() && blocks.is_empty() {
            return None;
        }
        let mut manifest = self.manifest.clone();
        manifest.logs.retain(|e| !logs.contains(&e.file));
        manifest.spans.retain(|e| !spans.contains(&e.file));
        manifest.blocks.retain(|e| !blocks.contains(&e.file));
        Some(Expired { dir, manifest, blocks, logs, spans })
    }

    /// Expired files taken out, their manifest durable already.
    fn take_expired(&mut self, e: &Expired) -> Maintained {
        self.logs.drop_files(&e.logs);
        self.spans.drop_files(&e.spans);
        self.blocks.retain(|b| !e.blocks.contains(&b.file));
        let mut manifest = e.manifest.clone();
        manifest.next_seq = manifest.next_seq.max(self.manifest.next_seq);
        self.manifest = manifest;
        Maintained { blocks_removed: (e.logs.len() + e.spans.len() + e.blocks.len()) as u64, ..Maintained::default() }
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
        let mut files: Vec<ImageFile> = Vec::with_capacity(self.blocks.len() + self.logs.files.len() + self.spans.files.len());
        for b in &self.blocks {
            files.push(ImageFile { kind: FileKind::Block, seq: b.seq, name: b.file.clone(), bytes: b.block.bytes, crc: b.block.crc });
        }
        for l in &self.logs.files {
            files.push(ImageFile { kind: FileKind::Logs, seq: l.seq, name: l.file.clone(), bytes: l.log.bytes, crc: l.log.crc });
        }
        for l in &self.spans.files {
            files.push(ImageFile { kind: FileKind::Spans, seq: l.seq, name: l.file.clone(), bytes: l.log.bytes, crc: l.log.crc });
        }
        files.sort_by_key(|f| f.seq);
        Image { cutoff: self.manifest.cutoff, logs_cutoff: self.manifest.logs_cutoff, spans_cutoff: self.manifest.spans_cutoff, max_series: self.manifest.max_series, sessions: self.manifest.sessions.clone(), files }
    }

    /// Bytes of the files the durable state is in.
    pub fn image_bytes(&self) -> u64 {
        self.disk_bytes() + self.log_bytes() + self.span_bytes()
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
        for e in self.manifest.blocks.iter().chain(&self.manifest.logs).chain(&self.manifest.spans) {
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
                FileKind::Logs | FileKind::Spans => LogFile::open(&p).map(|l| (l.bytes, l.crc)).map_err(|_| StoreError::Staged(f.name.clone()))?,
            };
            if bytes != f.bytes || crc != f.crc {
                return Err(StoreError::Staged(f.name.clone()));
            }
        }
        let mut manifest = Manifest {
            durable: at,
            next_seq: self.manifest.next_seq,
            cutoff: image.cutoff,
            logs_cutoff: image.logs_cutoff,
            spans_cutoff: image.spans_cutoff,
            max_series: image.max_series,
            conf,
            sessions: image.sessions.clone(),
            blocks: Vec::new(),
            logs: Vec::new(),
            spans: Vec::new(),
        };
        let mut files: Vec<&ImageFile> = image.files.iter().collect();
        files.sort_by_key(|f| f.seq);
        for f in files {
            let seq = manifest.next_seq;
            manifest.next_seq += 1;
            let name = match f.kind {
                FileKind::Block => manifest::block_file(seq),
                FileKind::Logs => manifest::logs_file(seq),
                FileKind::Spans => manifest::spans_file(seq),
            };
            let to = dir.join(&name);
            std::fs::rename(staging.join(&f.name), &to).map_err(io_err(&to))?;
            match f.kind {
                FileKind::Block => manifest.blocks.push(BlockEntry { seq, file: name }),
                FileKind::Logs => manifest.logs.push(BlockEntry { seq, file: name }),
                FileKind::Spans => manifest.spans.push(BlockEntry { seq, file: name }),
            }
        }
        manifest::sync_dir(&dir).map_err(io_err(&dir))?;
        manifest.save(&dir)?;
        for e in self.manifest.blocks.iter().chain(&self.manifest.logs).chain(&self.manifest.spans) {
            remove_if_there(&dir.join(&e.file))?;
        }
        let (epoch, feed, touched, spans_touched, lock) = (self.epoch + 1, self.feed.take(), self.touched.take(), self.spans_touched.take(), self.manifest_lock.clone());
        *self = Self::open(&dir, self.hash)?;
        self.epoch = epoch;
        self.feed = feed;
        self.manifest_lock = lock;
        for t in touched.iter().chain(&spans_touched) {
            t.everything();
        }
        self.touched = touched;
        self.spans_touched = spans_touched;
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

    /// Whether [`Store::read_rolled`] reads rollups or every sample: the same answers either way
    /// (`crate::rollup`), so only to measure the one against the other, or check it.
    pub fn set_read_rollups(&mut self, on: bool) {
        self.read_rollups = on;
    }

    /// [`Store::read_with`], but a window of `roll`'s tier that lies wholly in `roll`'s range and
    /// that one block alone holds of the series (no other block, nor the head, has a sample in
    /// it) is read from that block's rollups into `rolled`, its samples not read; every other
    /// sample in `[from, to]` is read as `read_with` reads it. `ts` and `rolled` are each in time
    /// order, and no window of `rolled` holds a sample of `ts`. Sources that overlap in time (late
    /// writes not yet merged) are read as samples.
    #[allow(clippy::too_many_arguments)]
    pub fn read_rolled(&self, id: u32, from: i64, to: i64, roll: &Roll<'_>, scratch: &mut Scratch, ts: &mut Vec<i64>, values: &mut Vec<f64>, rolled: &mut Rolled) -> Result<(), StoreError> {
        let from = from.max(self.cutoff);
        if to < from {
            return Ok(());
        }
        let Some(fp) = self.index.fp(id) else { return Ok(()) };
        if !self.read_rollups {
            return self.read_with(id, from, to, scratch, ts, values);
        }
        let head = self.head.get(id as usize);
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
        if !sources.windows(2).all(|w| matches!(w, [a, b] if a.2 < b.1)) {
            return self.read_with(id, from, to, scratch, ts, values);
        }
        let width = roll.width;
        for (si, &(src, f, l)) in sources.iter().enumerate() {
            let block = src.and_then(|bi| self.blocks.get(bi)).and_then(|b| Some((&b.block, b.block.find(fp)?)));
            let Some((blk, i)) = block else {
                if let Some(h) = head {
                    h.read(from, to, ts, values)?;
                }
                continue;
            };
            let rollups = blk.rollups(i, &mut scratch.rollups)?;
            let damaged = |_| BlockError::Damaged(blk.path().to_owned(), "series rollups");
            let tier = if blk.kind(i) == Some(roll.kind) { rollup::tier(rollups, width, roll.kind).map_err(damaged)? } else { None };
            let Some(tier) = tier else {
                blk.read(i, from, to, &mut scratch.buf, ts, values)?;
                continue;
            };
            // Its neighbours' spans: a window either reaches is not this block's alone.
            let before = si.checked_sub(1).and_then(|p| sources.get(p)).map(|s| s.2);
            let after = sources.get(si + 1).map(|s| s.1);
            let base = rolled.len();
            tier.read(from.max(f), to.min(l), roll.cols, rolled).map_err(damaged)?;
            // Each window kept, or what of the range it covers read as samples: every sample of
            // the block lies in a window it lists.
            let ranges = &mut scratch.ranges;
            ranges.clear();
            let mut kept = base;
            for j in base..rolled.len() {
                let (Some(&start), Some(&n)) = (rolled.start.get(j), rolled.n.get(j)) else { continue };
                let end = start.saturating_add(width);
                let alone = before.is_none_or(|b| b < start) && after.is_none_or(|a| a >= end);
                if n > 0 && alone && start >= roll.from && end <= roll.to && start >= from {
                    rolled.keep(j, kept);
                    kept += 1;
                    continue;
                }
                let (a, b) = (start.max(from), end.saturating_sub(1).min(to));
                match ranges.last_mut() {
                    Some(last) if last.1.saturating_add(1) >= a => last.1 = b,
                    _ => ranges.push((a, b)),
                }
            }
            rolled.truncate(kept);
            if !ranges.is_empty() {
                blk.read_ranges(i, ranges, &mut scratch.buf, ts, values)?;
            }
        }
        Ok(())
    }

    /// The logs as they are now, to read with no hold of the store ([`LogsAt`]): what an API's
    /// reading takes under a brief hold, and reads after.
    pub fn logs_at(&self) -> LogsAt {
        LogsAt { snap: self.logs.snapshot(), cutoff_ns: self.logs_cutoff.saturating_mul(1_000_000) }
    }

    /// The spans as they are now ([`LogsAt`]).
    pub fn spans_at(&self) -> LogsAt {
        LogsAt { snap: self.spans.snapshot(), cutoff_ns: self.spans_cutoff.saturating_mul(1_000_000) }
    }

    /// The newest `limit` log records in `[from, to]` (ns, none older than the logs' cutoff) that
    /// `search` matches.
    pub fn search_logs(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, StoreError> {
        self.logs_at().search(search, from, to, limit)
    }

    /// Log records in `[from, to]` (ns, none older than the logs' cutoff) that `search` matches,
    /// grouped by `by` and measured (log analytics).
    pub fn aggregate_logs(&self, search: &Search, from: i64, to: i64, by: &[crate::logs::store::Facet], measure: &crate::logs::store::Measure) -> Result<crate::logs::store::Aggregated, crate::logs::store::AggregateError> {
        self.logs_at().aggregate(search, from, to, by, measure)
    }

    /// As [`aggregate_logs`](Self::aggregate_logs), in buckets of `step` ns aligned to it (the
    /// buckets follow `from` as asked; what the cutoff has dropped is in none).
    pub fn aggregate_logs_buckets(&self, search: &Search, from: i64, to: i64, step: i64, by: &[crate::logs::store::Facet], measure: &crate::logs::store::Measure) -> Result<crate::logs::store::Bucketed, crate::logs::store::AggregateError> {
        self.logs_at().aggregate_buckets(search, from, to, step, by, measure)
    }

    /// Every metric from logs' points for the window that begins at `window` (ns): none when the
    /// logs' cutoff reaches into it (not all its records are held).
    pub fn count_log_metrics(&self, metrics: &[crate::logs::metrics::LogMetric], window: i64) -> Result<Option<crate::logs::metrics::Counted>, StoreError> {
        if window < self.logs_cutoff.saturating_mul(1_000_000) {
            return Ok(None);
        }
        self.logs_at().count_metrics(metrics, window)
    }

    /// Each facet's most common values over the logs a search matches, in one pass
    /// ([`LogStore::facet_counts`]).
    pub fn log_facets(&self, search: &Search, from: i64, to: i64, facets: &[crate::logs::store::Facet], top: usize) -> Result<crate::logs::store::FacetCounts, crate::logs::store::AggregateError> {
        self.logs_at().facets(search, from, to, facets, top)
    }

    /// Bytes of logs on disk.
    pub fn log_bytes(&self) -> u64 {
        self.logs.disk_bytes()
    }

    /// The newest `limit` spans in `[from, to]` (ns, none older than the spans' cutoff) that
    /// `search` matches (a search of spans: `crate::logs::search::parse_spans`).
    pub fn search_spans(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, StoreError> {
        self.spans_at().search(search, from, to, limit)
    }

    /// Spans in `[from, to]` that `search` matches, grouped by `by` and measured (trace analytics).
    pub fn aggregate_spans(&self, search: &Search, from: i64, to: i64, by: &[crate::logs::store::Facet], measure: &crate::logs::store::Measure) -> Result<crate::logs::store::Aggregated, crate::logs::store::AggregateError> {
        self.spans_at().aggregate(search, from, to, by, measure)
    }

    /// As [`aggregate_spans`](Self::aggregate_spans), in buckets of `step` ns aligned to it.
    pub fn aggregate_spans_buckets(&self, search: &Search, from: i64, to: i64, step: i64, by: &[crate::logs::store::Facet], measure: &crate::logs::store::Measure) -> Result<crate::logs::store::Bucketed, crate::logs::store::AggregateError> {
        self.spans_at().aggregate_buckets(search, from, to, step, by, measure)
    }

    /// Each facet's most common values over the spans a search matches, in one pass.
    pub fn span_facets(&self, search: &Search, from: i64, to: i64, facets: &[crate::logs::store::Facet], top: usize) -> Result<crate::logs::store::FacetCounts, crate::logs::store::AggregateError> {
        self.spans_at().facets(search, from, to, facets, top)
    }

    /// Every metric from spans' points for the window that begins at `window` (ns): none when the
    /// spans' cutoff reaches into it.
    pub fn count_span_metrics(&self, metrics: &[crate::logs::metrics::LogMetric], window: i64) -> Result<Option<crate::logs::metrics::Counted>, StoreError> {
        if window < self.spans_cutoff.saturating_mul(1_000_000) {
            return Ok(None);
        }
        self.spans_at().count_metrics(metrics, window)
    }

    /// Every span in `[from, to]` (ns) that `search` matches, to `f`, one at a time, none kept (a
    /// trace's spans, a service map's).
    pub fn each_span(&self, search: &Search, from: i64, to: i64, f: impl FnMut(&crate::logs::record::RecordRef<'_>)) -> Result<(), StoreError> {
        self.spans_at().each(search, from, to, f)
    }

    /// Spans a scan of `[from, to]` (ns) reads at the most, from the blocks' summaries.
    pub fn span_rows_between(&self, from: i64, to: i64) -> u64 {
        self.spans.rows_between(from.max(self.spans_cutoff.saturating_mul(1_000_000)), to)
    }

    /// Bytes of spans on disk.
    pub fn span_bytes(&self) -> u64 {
        self.spans.disk_bytes()
    }

    /// Whether the store holds spans, or their cutoff: the group has taken spans.
    pub fn holds_spans(&self) -> bool {
        self.spans_cutoff != i64::MIN || !self.spans.files.is_empty() || self.spans.unwritten_rows() > 0
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

    /// Each metric `matching` its name, what it is and costs ([`MetricSummary`]): the `limit` of
    /// most series (their tags' cardinalities read for those alone), the most first. Series and
    /// samples counted from the index and this process's counts: nothing read from disk.
    pub fn metric_summaries(&self, matching: &dyn Fn(&str) -> bool, limit: usize, top_values: usize) -> (usize, Vec<MetricSummary>) {
        let mut all: Vec<(Sym, usize)> = self.index.metrics().iter().filter(|&&m| matching(self.symbols.get(m))).map(|&m| (m, self.index.series_of(m).len())).collect();
        let matched = all.len();
        all.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| self.symbols.get(a.0).cmp(self.symbols.get(b.0))));
        all.truncate(limit);
        (matched, all.into_iter().map(|(m, _)| self.summarize(m, top_values)).collect())
    }

    /// Every metric's samples applied since the store opened: what a rate is reckoned from.
    pub fn metric_samples(&self) -> Vec<(&str, u64)> {
        self.index.metrics().iter().map(|&m| (self.symbols.get(m), self.metric_samples.get(m.0 as usize).copied().unwrap_or(0))).collect()
    }

    /// Metric `name`'s summary, each tag's `top_values` values with the most series.
    pub fn metric_summary(&self, name: &str, top_values: usize) -> Option<MetricSummary> {
        let m = self.symbols.find(name)?;
        self.index.metrics().contains(&m).then(|| self.summarize(m, top_values))
    }

    fn summarize(&self, m: Sym, top_values: usize) -> MetricSummary {
        let ids = self.index.series_of(m);
        let mut kinds: Vec<(Kind, usize)> = Vec::new();
        // Each tag key: its values' series counts.
        let mut keys: std::collections::HashMap<Sym, std::collections::HashMap<Sym, usize>> = std::collections::HashMap::new();
        let mut last = i64::MIN;
        for &id in ids {
            let Some(info) = self.index.info(id) else { continue };
            match kinds.iter_mut().find(|(k, _)| *k == info.kind) {
                Some(k) => k.1 += 1,
                None => kinds.push((info.kind, 1)),
            }
            for &(k, v) in self.index.tags(&info) {
                *keys.entry(k).or_default().entry(v).or_default() += 1;
            }
            last = last.max(self.last.get(id as usize).copied().unwrap_or(i64::MIN));
        }
        kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
        let mut tags: Vec<TagSummary> = keys
            .into_iter()
            .map(|(k, values)| {
                let series = values.values().sum();
                let mut top: Vec<(Sym, usize)> = values.iter().map(|(&v, &n)| (v, n)).collect();
                top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| self.symbols.get(a.0).cmp(self.symbols.get(b.0))));
                top.truncate(top_values);
                TagSummary { key: self.symbols.get(k).to_owned(), values: values.len(), series, top: top.into_iter().map(|(v, n)| (self.symbols.get(v).to_owned(), n)).collect() }
            })
            .collect();
        tags.sort_by(|a, b| b.values.cmp(&a.values).then_with(|| a.key.cmp(&b.key)));
        MetricSummary {
            name: self.symbols.get(m).to_owned(),
            series: ids.len(),
            kinds,
            tags,
            samples: self.metric_samples.get(m.0 as usize).copied().unwrap_or(0),
            last_ms: (last > i64::MIN).then_some(last),
        }
    }

    pub fn series_count(&self) -> usize {
        self.index.len()
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Bytes the blocks take, and of them their rollups'.
    pub fn block_bytes(&self) -> (u64, u64) {
        self.blocks.iter().fold((0, 0), |(b, r), o| (b + o.block.bytes, r + o.block.rollups_bytes()))
    }

    /// The blocks' places (their sequence numbers), files and bytes, oldest first.
    pub fn block_list(&self) -> Vec<(u64, &str, u64)> {
        self.blocks.iter().map(|b| (b.seq, b.file.as_str(), b.block.bytes)).collect()
    }

    /// Bytes of blocks on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.blocks.iter().map(|b| b.block.bytes).sum()
    }

    /// Bytes held in memory, by part.
    pub fn heap(&self) -> Heap {
        Heap {
            symbols: self.symbols.heap_bytes(),
            index: self.index.heap_bytes() + self.blocks.iter().map(|b| b.block.heap_bytes()).sum::<usize>() + self.logs.heap_bytes() + self.spans.heap_bytes(),
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
        // The spans' cutoff only once there is one: a store that never held spans digests as a
        // build that knows none does.
        if self.spans_cutoff != i64::MIN {
            d = d.wrapping_add(mix(self.spans_cutoff as u64 ^ 0x3));
        }
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
            // Every record since the logs' cutoff, visited (a sum's terms: in no order, none kept),
            // each as its own row.
            let mut row = Vec::new();
            self.logs.each_record(self.logs_cutoff.saturating_mul(1_000_000), i64::MAX, |r| {
                row.clear();
                crate::logs::record::put_row_ref(&mut row, r, 0);
                d = d.wrapping_add(mix(crate::crc32c::checksum(&row) as u64 ^ mix(r.ts as u64)));
            })?;
            // Spans as records are, apart from them (a span is not the log it would be).
            self.spans.each_record(self.spans_cutoff.saturating_mul(1_000_000), i64::MAX, |r| {
                row.clear();
                crate::logs::record::put_row_ref(&mut row, r, 0);
                d = d.wrapping_add(mix(mix(crate::crc32c::checksum(&row) as u64 ^ mix(r.ts as u64)) ^ 0x5));
            })?;
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
    rollups: Vec<u8>,
    /// What [`Store::read_rolled`] reads of a block as samples.
    ranges: Vec<(i64, i64)>,
}

/// What [`Store::read_rolled`] may read from rollups: windows of a tier's `width` (one of
/// `crate::rollup::TIERS`) wholly in `[from, to)`, of a series of `kind` (a block that held it as
/// another kind is read as samples), the columns `cols`.
#[derive(Debug, Clone, Copy)]
pub struct Roll<'c> {
    pub width: i64,
    pub from: i64,
    pub to: i64,
    pub kind: Kind,
    pub cols: &'c [Col],
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


/// A batch's rows within `horizon` (ns): the rows as they are when none is past it (as nearly
/// always), else written again without those that are; how many there are, and how many were
/// dropped; the windows of those kept told to `windows`, if given (metrics from logs). Every row is
/// read whole, its attributes' text too: a batch with a malformed row (a remote writer's) is refused
/// here, never taken into the head, whose seals and cuts read every row.
fn within_logs(raw: Vec<u8>, rows: u32, horizon: i64, mut windows: Option<&mut BatchWindows>) -> Result<(Vec<u8>, u32, u64), crate::logs::record::RowError> {
    use crate::logs::record::{RowError, Rows, put_row_ref};
    let mut it = Rows::new(&raw, rows as usize);
    let mut any = false;
    while let Some(r) = it.next_row()? {
        if r.attrs().count() != r.attr_count {
            return Err(RowError);
        }
        if r.ts > horizon {
            any = true;
            break;
        }
        if let Some(w) = windows.as_deref_mut() {
            w.add(r.ts);
        }
    }
    if !any {
        return Ok((raw, rows, 0));
    }
    let mut kept = Vec::with_capacity(raw.len());
    let (mut n, mut ahead, mut prev) = (0u32, 0u64, 0i64);
    let mut it = Rows::new(&raw, rows as usize);
    while let Some(r) = it.next_row()? {
        if r.attrs().count() != r.attr_count {
            return Err(RowError);
        }
        if r.ts > horizon {
            ahead += 1;
            continue;
        }
        if let Some(w) = windows.as_deref_mut() {
            w.add(r.ts);
        }
        put_row_ref(&mut kept, &r, prev);
        prev = r.ts;
        n += 1;
    }
    Ok((kept, n, ahead))
}

fn read_store(store: &RwLock<Store>) -> RwLockReadGuard<'_, Store> {
    store.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_store(store: &RwLock<Store>) -> RwLockWriteGuard<'_, Store> {
    store.write().unwrap_or_else(PoisonError::into_inner)
}

// The store's files changed beside queries (`docs/observe.md` §4.5). A cut, a rewrite taken in and
// files expired each write their files and save the manifest under a shared hold of the store, or
// none, and take the change in under an exclusive hold that writes nothing: a query never waits on
// the disk. Each holds the manifest lock throughout, taken before the store's own and never while
// holding it: one change of the files at a time, and no thread waits for the store's exclusive hold
// while another holds a shared one to write (a waiting writer would hold every new reader back).

/// A cut ([`Store::cut`]) beside queries, by the one thread that applies (nothing changes the
/// head meanwhile): its files written and its manifest saved under a shared hold, taken in under
/// an exclusive one.
pub fn cut_shared(store: &RwLock<Store>, at: Point) -> Result<Maintained, StoreError> {
    let lock = read_store(store).manifest_lock();
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let written = read_store(store).write_cut(at)?;
    let taken = write_store(store).install_cut(written);
    match taken {
        // Something applied meanwhile (not as the group's thread cuts): cut under the hold.
        Err(StoreError::Moved) => write_store(store).cut(at),
        done => done,
    }
}

/// A log store's records as they were at a moment ([`Store::logs_at`], [`Store::spans_at`]): read
/// with no hold of the store, so a scan however long never keeps a write waiting, nor a reader
/// behind that write; none older than the store's cutoff then.
#[derive(Debug, Clone)]
pub struct LogsAt {
    snap: crate::logs::store::LogSnapshot,
    cutoff_ns: i64,
}

impl LogsAt {
    /// The newest `limit` records in `[from, to]` (ns) that `search` matches.
    pub fn search(&self, search: &Search, from: i64, to: i64, limit: usize) -> Result<Found, StoreError> {
        Ok(self.snap.search(search, from.max(self.cutoff_ns), to, limit)?)
    }

    /// Records in `[from, to]` that `search` matches, grouped by `by` and measured.
    pub fn aggregate(
        &self,
        search: &Search,
        from: i64,
        to: i64,
        by: &[crate::logs::store::Facet],
        measure: &crate::logs::store::Measure,
    ) -> Result<crate::logs::store::Aggregated, crate::logs::store::AggregateError> {
        self.snap.aggregate(search, from.max(self.cutoff_ns), to, by, measure)
    }

    /// As [`LogsAt::aggregate`], in buckets of `step` ns aligned to it (the buckets follow `from`
    /// as asked; what the cutoff dropped is in none).
    pub fn aggregate_buckets(
        &self,
        search: &Search,
        from: i64,
        to: i64,
        step: i64,
        by: &[crate::logs::store::Facet],
        measure: &crate::logs::store::Measure,
    ) -> Result<crate::logs::store::Bucketed, crate::logs::store::AggregateError> {
        let buckets = crate::logs::store::Buckets::over(from, to, step)?;
        self.snap.aggregate_buckets(search, from.max(self.cutoff_ns), to, buckets, by, measure)
    }

    /// Each facet's most common values over the records a search matches, in one pass.
    pub fn facets(&self, search: &Search, from: i64, to: i64, facets: &[crate::logs::store::Facet], top: usize) -> Result<crate::logs::store::FacetCounts, crate::logs::store::AggregateError> {
        self.snap.facet_counts(search, from.max(self.cutoff_ns), to, facets, top)
    }

    /// Every record in `[from, to]` that `search` matches, to `f`, one at a time, none kept.
    pub fn each(&self, search: &Search, from: i64, to: i64, f: impl FnMut(&crate::logs::record::RecordRef<'_>)) -> Result<(), StoreError> {
        Ok(self.snap.each_matching(search, from.max(self.cutoff_ns), to, f)?)
    }

    /// Every metric from these records' points for the window that begins at `window` (ns): none
    /// when the cutoff reaches into it (not all its records are held).
    pub fn count_metrics(&self, metrics: &[crate::logs::metrics::LogMetric], window: i64) -> Result<Option<crate::logs::metrics::Counted>, StoreError> {
        if window < self.cutoff_ns {
            return Ok(None);
        }
        Ok(Some(crate::logs::metrics::count(&self.snap, metrics, window)?))
    }
}

/// The rewrite maintenance makes next (a merge, or a block retention halves), its file reserved;
/// built apart ([`Rewrite::build`]) and taken in by [`commit_shared`].
pub fn plan_shared(store: &RwLock<Store>) -> Option<Rewrite> {
    let lock = read_store(store).manifest_lock();
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let mut s = write_store(store);
    s.plan_merge().or_else(|| s.plan_retention())
}

/// A rewrite built apart, taken in beside queries ([`Store::commit`]): its manifest saved with no
/// hold of the store, taken in under an exclusive one, its inputs deleted after.
pub fn commit_shared(store: &RwLock<Store>, built: Built) -> Result<Maintained, StoreError> {
    let lock = read_store(store).manifest_lock();
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let manifest = read_store(store).commit_manifest(&built);
    let dir = built.rewrite.dir.clone();
    let Some(manifest) = manifest else {
        let path = dir.join(&built.rewrite.file);
        drop(built);
        remove_if_there(&path)?;
        return Ok(Maintained::default());
    };
    manifest::sync_dir(&dir).map_err(io_err(&dir))?;
    manifest.save(&dir)?;
    let (done, gone) = write_store(store).take_commit(built, manifest)?;
    for p in gone {
        remove_if_there(&p)?;
    }
    Ok(done)
}

/// Files wholly older than the durable cutoff deleted beside queries ([`Store::drop_expired`]):
/// the manifest without them saved with no hold of the store, taken in under an exclusive one, the
/// files deleted after.
pub fn drop_expired_shared(store: &RwLock<Store>) -> Result<Maintained, StoreError> {
    let lock = read_store(store).manifest_lock();
    let _held = lock.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(e) = read_store(store).expired() else { return Ok(Maintained::default()) };
    e.manifest.save(&e.dir)?;
    let done = write_store(store).take_expired(&e);
    for f in e.files() {
        remove_if_there(&f)?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests;
