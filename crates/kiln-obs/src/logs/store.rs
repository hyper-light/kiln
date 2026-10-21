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
    #[error("a thread reading log blocks failed")]
    Helper,
}

/// Rows a scan's thread takes at the least before another is asked for: some ten milliseconds of
/// reading at 190 ns a row (`docs/observe.md` §12.8), against the tens of microseconds a thread
/// takes to start and its groups' merging.
pub const SPLIT_ROWS: u64 = 1 << 16;

/// A block a scan reads: one sealed in memory, or a file's (its `i`th).
#[derive(Clone, Copy)]
enum Src<'a> {
    Sealed(&'a LogBlock),
    File(&'a LogFile, usize, &'a LogBlock),
}

impl<'a> Src<'a> {
    fn block(&self) -> &'a LogBlock {
        match self {
            Self::Sealed(b) | Self::File(_, _, b) => b,
        }
    }
}

/// One thread's reading of a scan: what it matches, its room kept from block to block.
struct Scan<'q> {
    search: &'q Search,
    from: i64,
    to: i64,
    scratch: RowScratch,
    text: Vec<u8>,
    raw: Vec<u8>,
}

impl<'q> Scan<'q> {
    fn new(search: &'q Search, from: i64, to: i64) -> Self {
        Self { search, from, to, scratch: RowScratch::default(), text: Vec::new(), raw: Vec::new() }
    }

    /// A row read: to `f` if it is in the range and matches.
    fn row<E>(&mut self, r: &RecordRef<'_>, f: &mut impl FnMut(&RecordRef<'_>) -> Result<(), E>) -> Result<(), E> {
        if r.ts < self.from || r.ts > self.to || !self.search.row(r, &mut self.scratch) {
            return Ok(());
        }
        f(r)
    }

    /// The head's rows: how many.
    fn head<E: From<LogStoreError>>(&mut self, store: &LogStore, f: &mut impl FnMut(&RecordRef<'_>) -> Result<(), E>) -> Result<u64, E> {
        let mut n = 0;
        for (seg, rows) in &store.head {
            let mut it = Rows::new(seg, *rows as usize);
            while let Some(r) = it.next_row().map_err(LogStoreError::from)? {
                n += 1;
                self.row(&r, f)?;
            }
        }
        Ok(n)
    }

    /// A block's rows: how many.
    fn block<E: From<LogStoreError>>(&mut self, src: Src<'_>, f: &mut impl FnMut(&RecordRef<'_>) -> Result<(), E>) -> Result<u64, E> {
        let mut raw = std::mem::take(&mut self.raw);
        let read = match src {
            Src::Sealed(b) => b.decompress(&mut raw).map_err(|_| LogStoreError::Text),
            Src::File(file, i, b) => match file.read_text(i, &mut self.text) {
                Ok(_) => b.decompress_text(&self.text, &mut raw).map_err(|_| LogStoreError::Text),
                Err(e) => Err(LogStoreError::from(e)),
            },
        };
        let result = (|| {
            read?;
            let mut n = 0;
            let mut it = src.block().rows(&raw);
            while let Some(r) = it.next_row().map_err(LogStoreError::from)? {
                n += 1;
                self.row(&r, f)?;
            }
            Ok(n)
        })();
        self.raw = raw;
        result
    }
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
    /// A store of spans: its blocks' filters hold their trace ids ([`LogBlock::seal_in`]).
    spans: bool,
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
    /// A store of spans (`crate::otlp::traces`): records as logs are, its blocks' filters holding
    /// their trace ids, so a trace's spans are found without reading the blocks that hold none.
    pub fn spans() -> Self {
        Self { spans: true, ..Self::default() }
    }

    /// Whether it holds spans.
    pub fn is_spans(&self) -> bool {
        self.spans
    }

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
        let block = LogBlock::seal_in(&refs, self.spans);
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

    /// Writes every unwritten row to `path` (the head sealed as a copy), the store left as it is
    /// (a cut writing under a shared hold); none if there was nothing. [`LogStore::written`] takes
    /// the file in after.
    pub fn write_unwritten(&self, path: &Path) -> Result<Option<LogFile>, LogStoreError> {
        let head = if self.head.is_empty() {
            None
        } else {
            let mut refs: Vec<RecordRef<'_>> = Vec::new();
            for (raw, n) in &self.head {
                let mut rows = Rows::new(raw, *n as usize);
                while let Some(r) = rows.next_row()? {
                    refs.push(r);
                }
            }
            Some(LogBlock::seal_in(&refs, self.spans))
        };
        let blocks: Vec<&LogBlock> = self.sealed.iter().chain(head.as_ref()).collect();
        if blocks.is_empty() {
            return Ok(None);
        }
        Ok(Some(super::file::write_refs(path, &blocks)?))
    }

    /// The file [`LogStore::write_unwritten`] wrote, taken in: the rows it holds leave memory.
    pub fn written(&mut self, file: HeldFile) {
        self.head.clear();
        self.head_bytes = 0;
        self.sealed.clear();
        self.files.push(file);
    }

    /// Rows of the blocks (and the head) whose time reaches into `[from, to]` (ns), from their
    /// summaries alone: what a scan of the range reads, at the most.
    pub fn rows_between(&self, from: i64, to: i64) -> u64 {
        let head: u64 = self.head.iter().map(|h| u64::from(h.1)).sum();
        let blocks = self.sealed.iter().chain(self.files.iter().flat_map(|f| f.log.blocks.iter()));
        head + blocks.filter(|b| b.max_ts >= from && b.min_ts <= to).map(|b| u64::from(b.rows)).sum::<u64>()
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
        let gone = self.expired(cutoff);
        self.drop_files(&gone);
        gone
    }

    /// The names of the files wholly older than `cutoff` (ns).
    pub fn expired(&self, cutoff: i64) -> Vec<String> {
        self.files.iter().filter(|f| f.log.max_ts < cutoff).map(|f| f.file.clone()).collect()
    }

    /// Files named no longer read (their manifest durable without them).
    pub fn drop_files(&mut self, names: &[String]) {
        self.files.retain(|f| !names.contains(&f.file));
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
        let (mut text, mut raw) = (Vec::new(), Vec::new());
        // Newest first; records of one time by their contents, so that which of them a limit keeps
        // is the same on every replica, however its blocks fell.
        let before = |x: &(i64, Record), r: &RecordRef<'_>| x.0 > r.ts || (x.0 == r.ts && cmp_contents(r, &x.1) != std::cmp::Ordering::Less);
        let consider = |r: &RecordRef<'_>, best: &mut Vec<(i64, Record)>, scratch: &mut RowScratch, found: &mut Found| {
            found.rows_scanned += 1;
            if r.ts < from || r.ts > to || !search.row(r, scratch) {
                return;
            }
            if best.len() >= limit && best.last().is_some_and(|w| before(w, r)) {
                return;
            }
            let at = best.partition_point(|x| before(x, r));
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
            if summary.is_some_and(|b| search.block_in(b, self.spans) == Maybe::No) {
                found.blocks_pruned += 1;
                continue;
            }
            let block: &LogBlock = match src {
                Src::File(f, i) => {
                    let b = f.log.read_text(i, &mut text)?;
                    b.decompress_text(&text, &mut raw).map_err(|_| LogStoreError::Text)?;
                    b
                }
                Src::Sealed(b) => {
                    b.decompress(&mut raw).map_err(|_| LogStoreError::Text)?;
                    b
                }
            };
            let mut rows = block.rows(&raw);
            while let Some(r) = rows.next_row()? {
                consider(&r, &mut best, &mut scratch, &mut found);
            }
        }
        found.records = best.into_iter().map(|(_, r)| r).collect();
        Ok(found)
    }
}

/// What records are grouped by: a field of their own, or an attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Facet {
    Service,
    Host,
    Source,
    /// The severity's level (`error`, `warn`, ...).
    Status,
    /// A span's name, its message (Datadog's `resource_name`).
    Name,
    Attr(String),
}

impl Facet {
    /// `service`, `host`, `source`, `status` (or `level`), else an attribute (`@key` or `key`).
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "service" => Self::Service,
            "host" => Self::Host,
            "source" => Self::Source,
            "status" | "level" => Self::Status,
            other => Self::Attr(other.trim_start_matches('@').to_owned()),
        }
    }

    /// As [`Facet::parse`], of spans: `resource_name` (or `resource`) their name too.
    pub fn parse_span(s: &str) -> Self {
        match s.trim() {
            "resource_name" | "resource" => Self::Name,
            other => Self::parse(other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Service => "service".into(),
            Self::Host => "host".into(),
            Self::Source => "source".into(),
            Self::Status => "status".into(),
            Self::Name => "resource_name".into(),
            Self::Attr(k) => format!("@{k}"),
        }
    }

    /// A record's value of it.
    pub fn of<'a>(&self, r: &RecordRef<'a>) -> Option<&'a str> {
        match self {
            Self::Service => Some(r.service),
            Self::Host => Some(r.host),
            Self::Source => Some(r.source),
            Self::Status => Some(super::record::level(r.severity)),
            Self::Name => Some(r.message),
            Self::Attr(k) => r.attr(k),
        }
    }
}

/// What each group's records come to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Measure {
    Count,
    /// Distinct values of a facet.
    Cardinality(Facet),
    /// Of an attribute's numeric values (a record without one, or not a number, is left out).
    Sum(String),
    Avg(String),
    Min(String),
    Max(String),
}

impl Measure {
    /// Datadog's rollup of records, as a log monitor's `.rollup()` names it: `count` (or none), or
    /// `cardinality`, `sum`, `avg`, `min`, `max` of a facet (`@duration`), a span's with `spans`.
    pub fn parse(name: Option<&str>, facet: Option<&str>, spans: bool) -> Option<Self> {
        let facet = facet.map(|f| f.trim().trim_start_matches('@'));
        Some(match (name.map(str::trim), facet) {
            (Some("count") | None, _) => Self::Count,
            (Some("cardinality"), Some(f)) => Self::Cardinality(if spans { Facet::parse_span(f) } else { Facet::parse(f) }),
            (Some("sum"), Some(f)) => Self::Sum(f.to_owned()),
            (Some("avg"), Some(f)) => Self::Avg(f.to_owned()),
            (Some("min"), Some(f)) => Self::Min(f.to_owned()),
            (Some("max"), Some(f)) => Self::Max(f.to_owned()),
            _ => return None,
        })
    }

    /// Whether it counts (records, or values), so a bucket nothing fell in is 0 rather than none.
    pub fn counts(&self) -> bool {
        matches!(self, Self::Count | Self::Cardinality(_))
    }
}

/// Groups an aggregation keeps at the most: a query's answer's (`crate::query::eval`'s).
pub const MAX_GROUPS: usize = 10_000;
/// Facets an aggregation groups by at the most (Datadog's log analytics takes four).
pub const MAX_BY: usize = 8;
/// Buckets a bucketed aggregation has at the most: a day by the minute, a week by the 7 minutes.
pub const MAX_BUCKETS: usize = 1_500;
/// Cells (a group's bucket) a bucketed aggregation keeps at the most, and its answer holds: 2²⁰
/// values are 16 MiB.
pub const MAX_CELLS: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Aggregated {
    /// Each group's facet values, in `by`'s order (`N/A` where a record had none), and its value;
    /// sorted by the values.
    pub groups: Vec<(Vec<String>, f64)>,
    pub rows_scanned: u64,
    pub blocks_pruned: u64,
}

/// Where a bucketed aggregation's buckets fall: `n` of `step` ns from `start`, a multiple of `step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buckets {
    pub start: i64,
    pub step: i64,
    pub n: usize,
}

impl Buckets {
    /// The buckets of `step` that cover `[from, to]`, aligned to `step` (the first and the last may
    /// hold only part of the range).
    pub fn over(from: i64, to: i64, step: i64) -> Result<Self, AggregateError> {
        if step <= 0 {
            return Err(AggregateError::Step);
        }
        let start = from - from.rem_euclid(step);
        let span = to.saturating_sub(start).max(0) / step;
        let n = usize::try_from(span).map_or(usize::MAX, |s| s.saturating_add(1));
        if n > MAX_BUCKETS {
            return Err(AggregateError::TooManyBuckets);
        }
        Ok(Self { start, step, n })
    }

    /// The bucket a time falls in, if any.
    fn of(&self, ts: i64) -> Option<u32> {
        let i = ts.checked_sub(self.start)?.checked_div(self.step)?;
        if ts < self.start || i >= self.n as i64 {
            return None;
        }
        u32::try_from(i).ok()
    }
}

/// An aggregation in time buckets: `count() by {status}` a minute, the explorer's histogram.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucketed {
    pub buckets: Buckets,
    /// Each group's facet values and its value in each bucket (none where no record fell), sorted
    /// by the values.
    pub groups: Vec<(Vec<String>, Vec<Option<f64>>)>,
    pub rows_scanned: u64,
    pub blocks_pruned: u64,
}

/// What [`LogStore::facet_counts`] found: each facet's most common values with their counts, and
/// how many values it has; the records that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetCounts {
    pub facets: Vec<(Vec<(String, u64)>, usize)>,
    pub matched: u64,
    pub rows_scanned: u64,
    pub blocks_pruned: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AggregateError {
    #[error(transparent)]
    Store(#[from] LogStoreError),
    #[error("more than {MAX_GROUPS} groups: a narrower search, or fewer facets")]
    TooManyGroups,
    #[error("more than {MAX_BY} facets to group by")]
    TooManyFacets,
    #[error("more than {MAX_BUCKETS} buckets: a wider step or a shorter range")]
    TooManyBuckets,
    #[error("more than {MAX_CELLS} groups' buckets: a wider step, a narrower search, or fewer facets")]
    TooManyCells,
    #[error("a bucket's width: more than 0")]
    Step,
}

/// One cell's running value.
#[derive(Debug)]
struct Acc {
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
    distinct: Option<std::collections::HashSet<String>>,
}

impl Acc {
    fn new() -> Self {
        Self { n: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, distinct: None }
    }

    /// Another's, of the same group and bucket, taken in.
    fn merge(&mut self, o: Acc) {
        self.n += o.n;
        self.sum += o.sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        if let Some(d) = o.distinct {
            match &mut self.distinct {
                Some(mine) => mine.extend(d),
                None => self.distinct = Some(d),
            }
        }
    }

    fn value(&self, measure: &Measure) -> f64 {
        match measure {
            Measure::Count => self.n as f64,
            Measure::Cardinality(_) => self.distinct.as_ref().map_or(0, std::collections::HashSet::len) as f64,
            Measure::Sum(_) => self.sum,
            Measure::Avg(_) => self.sum / self.n.max(1) as f64,
            Measure::Min(_) => self.min,
            Measure::Max(_) => self.max,
        }
    }
}

/// The groups an aggregation finds, each one's facet values kept once (a new group allocates; a
/// record of a group already found, nothing), and their cells: a group's bucket, or its whole.
struct Grouping {
    hash: crate::hash::Keyed,
    keys: Vec<Vec<String>>,
    /// Each group's place in `keys`, by its values' hash.
    by_key: hashbrown::HashTable<u32>,
    /// A cell's group (high 32 bits) and bucket (low), and its value.
    cells: hashbrown::HashTable<(u64, Acc)>,
}

fn values_hash<'s>(hash: &crate::hash::Keyed, values: impl Iterator<Item = &'s str>) -> u64 {
    values.fold(0, |h, v| hash.word(h ^ hash.bytes(v.as_bytes())))
}

impl Grouping {
    fn new(by: &[Facet]) -> Result<Self, AggregateError> {
        if by.len() > MAX_BY {
            return Err(AggregateError::TooManyFacets);
        }
        Ok(Self::empty())
    }

    /// A grouping of nothing yet (its facets checked by [`Grouping::new`] first).
    fn empty() -> Self {
        Self { hash: crate::hash::Keyed::new(), keys: Vec::new(), by_key: hashbrown::HashTable::new(), cells: hashbrown::HashTable::new() }
    }

    /// The cell of the group of `values` and `bucket`, made if new.
    fn cell(&mut self, values: &[&str], bucket: u32, cells_max: usize) -> Result<&mut Acc, AggregateError> {
        let hash = self.hash;
        let h = values_hash(&hash, values.iter().copied());
        let keys = &mut self.keys;
        let found = self.by_key.find(h, |&i| keys.get(i as usize).is_some_and(|k| k.iter().map(String::as_str).eq(values.iter().copied()))).copied();
        let group = match found {
            Some(i) => i,
            None => {
                if keys.len() >= MAX_GROUPS {
                    return Err(AggregateError::TooManyGroups);
                }
                let i = u32::try_from(keys.len()).map_err(|_| AggregateError::TooManyGroups)?;
                keys.push(values.iter().map(|v| (*v).to_owned()).collect());
                self.by_key.insert_unique(h, i, |&j| keys.get(j as usize).map_or(0, |k| values_hash(&hash, k.iter().map(String::as_str))));
                i
            }
        };
        let c = (u64::from(group) << 32) | u64::from(bucket);
        let full = self.cells.len() >= cells_max;
        match self.cells.entry(hash.word(c), |(k, _)| *k == c, |(k, _)| hash.word(*k)) {
            hashbrown::hash_table::Entry::Occupied(o) => Ok(&mut o.into_mut().1),
            hashbrown::hash_table::Entry::Vacant(_) if full => Err(AggregateError::TooManyCells),
            hashbrown::hash_table::Entry::Vacant(v) => Ok(&mut v.insert((c, Acc::new())).into_mut().1),
        }
    }

    /// Counts a record that matched into its cell.
    fn take(&mut self, r: &RecordRef<'_>, by: &[Facet], measure: &Measure, bucket: u32, cells_max: usize) -> Result<(), AggregateError> {
        let value = match measure {
            Measure::Sum(k) | Measure::Avg(k) | Measure::Min(k) | Measure::Max(k) => match r.attr(k).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()) {
                Some(v) => Some(v),
                None => return Ok(()),
            },
            _ => None,
        };
        let mut values: [&str; MAX_BY] = [""; MAX_BY];
        for (slot, f) in values.iter_mut().zip(by) {
            *slot = f.of(r).unwrap_or("N/A");
        }
        let acc = self.cell(values.get(..by.len()).unwrap_or_default(), bucket, cells_max)?;
        acc.n += 1;
        if let Some(v) = value {
            acc.sum += v;
            acc.min = acc.min.min(v);
            acc.max = acc.max.max(v);
        }
        if let Measure::Cardinality(f) = measure
            && let Some(v) = f.of(r)
        {
            let d = acc.distinct.get_or_insert_with(std::collections::HashSet::new);
            if !d.contains(v) {
                d.insert(v.to_owned());
            }
        }
        Ok(())
    }

    /// Another grouping's cells taken into this one's, each into the cell of its group's values
    /// and its bucket (a scan's threads' groupings, merged in their blocks' order).
    fn merge(&mut self, other: Grouping, cells_max: usize) -> Result<(), AggregateError> {
        let Grouping { keys, cells, .. } = other;
        for (c, a) in cells {
            let Some(key) = keys.get((c >> 32) as usize) else { continue };
            let mut values: [&str; MAX_BY] = [""; MAX_BY];
            for (slot, v) in values.iter_mut().zip(key) {
                *slot = v;
            }
            self.cell(values.get(..key.len()).unwrap_or_default(), (c & u64::from(u32::MAX)) as u32, cells_max)?.merge(a);
        }
        Ok(())
    }
}

/// Whether a measure's value is the same whatever order its records come in (and so a scan of it
/// may be split among threads): a sum's bits are not.
fn order_free(measure: &Measure) -> bool {
    matches!(measure, Measure::Count | Measure::Cardinality(_) | Measure::Min(_) | Measure::Max(_))
}

impl LogStore {
    /// Every record in `[from, to]` (ns) of every head segment, sealed block and file, in no order,
    /// handed to `f`: one at a time, none kept.
    pub fn each_record(&self, from: i64, to: i64, mut f: impl FnMut(&RecordRef<'_>)) -> Result<(), LogStoreError> {
        self.each_match(&Search::all(), from, to, |r| {
            f(r);
            Ok::<(), LogStoreError>(())
        })
        .map(|_| ())
    }

    /// Every record in `[from, to]` (ns) that `search` matches, to `f`, one at a time, none kept:
    /// the blocks it rules out never read.
    pub fn each_matching(&self, search: &Search, from: i64, to: i64, mut f: impl FnMut(&RecordRef<'_>)) -> Result<(), LogStoreError> {
        self.each_match(search, from, to, |r| {
            f(r);
            Ok::<(), LogStoreError>(())
        })
        .map(|_| ())
    }

    /// Every record in `[from, to]` (ns) that `search` matches, to `f`, across the head, sealed
    /// blocks and files (blocks a search rules out are never read); the rows read and the blocks
    /// ruled out.
    fn each_match<E: From<LogStoreError>>(&self, search: &Search, from: i64, to: i64, mut f: impl FnMut(&RecordRef<'_>) -> Result<(), E>) -> Result<(u64, u64), E> {
        if to < from {
            return Ok((0, 0));
        }
        let mut scan = Scan::new(search, from, to);
        let mut rows_scanned = scan.head(self, &mut f)?;
        let (srcs, blocks_pruned) = self.candidates(search, from, to);
        for s in &srcs {
            rows_scanned += scan.block(*s, &mut f)?;
        }
        Ok((rows_scanned, blocks_pruned))
    }

    /// The blocks a scan of `[from, to]` for `search` reads, sealed then files' in order, and how
    /// many it rules out.
    fn candidates(&self, search: &Search, from: i64, to: i64) -> (Vec<Src<'_>>, u64) {
        let consider = |b: &LogBlock| b.max_ts >= from && b.min_ts <= to && search.block_in(b, self.spans) != Maybe::No;
        let all = self.sealed.iter().map(Src::Sealed).chain(self.files.iter().flat_map(|f| f.log.blocks.iter().enumerate().map(|(i, b)| Src::File(&f.log, i, b))));
        let (mut srcs, mut pruned) = (Vec::new(), 0u64);
        for s in all {
            if consider(s.block()) {
                srcs.push(s);
            } else {
                pruned += 1;
            }
        }
        (srcs, pruned)
    }

    /// As [`each_match`](Self::each_match), each matching record to `fold` of a state its thread
    /// holds (`init`'s): where the blocks hold enough rows ([`SPLIT_ROWS`] a thread) and the
    /// queries' helper threads are free, split among them in order, each thread its run of blocks.
    /// The states in the blocks' order, this thread's first (the head's in it), to be merged; the
    /// rows read and the blocks ruled out. For what does not hang on the order records come in
    /// (counts, distinct values, the least and most): a sum's bits would.
    fn each_match_split<S: Send, E: From<LogStoreError> + Send>(
        &self,
        search: &Search,
        from: i64,
        to: i64,
        threads: Option<usize>,
        init: &(dyn Fn() -> S + Sync),
        fold: &(dyn Fn(&mut S, &RecordRef<'_>) -> Result<(), E> + Sync),
    ) -> Result<(Vec<S>, u64, u64), E> {
        let mut first = init();
        if to < from {
            return Ok((vec![first], 0, 0));
        }
        let mut scan = Scan::new(search, from, to);
        let mut rows_scanned = scan.head(self, &mut |r| fold(&mut first, r))?;
        let (srcs, blocks_pruned) = self.candidates(search, from, to);
        let rows: u64 = srcs.iter().map(|s| u64::from(s.block().rows)).sum();
        let want = usize::try_from(rows / SPLIT_ROWS).unwrap_or(usize::MAX).min(srcs.len()).saturating_sub(1);
        let taken = match threads {
            Some(_) => None,
            None => Some(crate::query::eval::take_helpers(want)),
        };
        let n = threads.unwrap_or_else(|| taken.as_ref().map_or(0, crate::query::eval::Taken::count) + 1).clamp(1, srcs.len().max(1));
        if n == 1 {
            for s in &srcs {
                rows_scanned += scan.block(*s, &mut |r| fold(&mut first, r))?;
            }
            return Ok((vec![first], rows_scanned, blocks_pruned));
        }
        // Runs of blocks of about equal rows, in order: the first this thread's.
        let mut runs: Vec<&[Src<'_>]> = Vec::with_capacity(n);
        let (mut at, mut acc, per) = (0usize, 0u64, rows.div_ceil(n as u64).max(1));
        for (i, s) in srcs.iter().enumerate() {
            acc += u64::from(s.block().rows);
            if acc >= per && runs.len() + 1 < n {
                runs.push(srcs.get(at..=i).unwrap_or_default());
                (at, acc) = (i + 1, 0);
            }
        }
        runs.push(srcs.get(at..).unwrap_or_default());
        let failed = || E::from(LogStoreError::Helper);
        std::thread::scope(|scope| {
            let mut helpers = Vec::with_capacity(runs.len().saturating_sub(1));
            for run in runs.iter().skip(1).copied() {
                let job = move || -> Result<(S, u64), E> {
                    crate::query::eval::helper_started();
                    let (mut state, mut sc, mut rows) = (init(), Scan::new(search, from, to), 0u64);
                    for s in run {
                        rows += sc.block(*s, &mut |r| fold(&mut state, r))?;
                    }
                    Ok((state, rows))
                };
                // A helper that does not start: its run is this thread's to read, in its place.
                helpers.push(std::thread::Builder::new().name("kiln-query".into()).spawn_scoped(scope, job).map_err(|_| run));
            }
            if let Some(run) = runs.first() {
                for s in *run {
                    rows_scanned += scan.block(*s, &mut |r| fold(&mut first, r))?;
                }
            }
            let mut states = Vec::with_capacity(runs.len());
            states.push(first);
            for h in helpers {
                let (state, rows) = match h {
                    Ok(h) => h.join().map_err(|_| failed())??,
                    Err(run) => {
                        let (mut state, mut rows) = (init(), 0u64);
                        for s in run {
                            rows += scan.block(*s, &mut |r| fold(&mut state, r))?;
                        }
                        (state, rows)
                    }
                };
                rows_scanned += rows;
                states.push(state);
            }
            Ok((states, rows_scanned, blocks_pruned))
        })
    }

    /// Every record in `[from, to]` (ns) that `search` matches, grouped by `by` and measured: log
    /// analytics' `count() by {service}`, and what a log monitor counts. Every matching record
    /// counts.
    pub fn aggregate(&self, search: &Search, from: i64, to: i64, by: &[Facet], measure: &Measure) -> Result<Aggregated, AggregateError> {
        self.aggregate_on(search, from, to, by, measure, None)
    }

    /// [`aggregate`](Self::aggregate), its scan split among `threads` (a test's), else as many as
    /// it is worth and are free; one, a sum's or an average's.
    fn aggregate_on(&self, search: &Search, from: i64, to: i64, by: &[Facet], measure: &Measure, threads: Option<usize>) -> Result<Aggregated, AggregateError> {
        Grouping::new(by)?;
        let threads = if order_free(measure) { threads } else { Some(1) };
        let (states, rows_scanned, blocks_pruned) = self.each_match_split(search, from, to, threads, &Grouping::empty, &|g: &mut Grouping, r: &RecordRef<'_>| g.take(r, by, measure, 0, MAX_GROUPS))?;
        let mut states = states.into_iter();
        let mut g = states.next().unwrap_or_else(Grouping::empty);
        for other in states {
            g.merge(other, MAX_GROUPS)?;
        }
        let mut accs: Vec<Option<Acc>> = std::iter::repeat_with(|| None).take(g.keys.len()).collect();
        for (c, a) in g.cells {
            if let Some(slot) = accs.get_mut((c >> 32) as usize) {
                *slot = Some(a);
            }
        }
        let mut groups: Vec<(Vec<String>, f64)> = g.keys.into_iter().zip(accs).filter_map(|(k, a)| Some((k, a?.value(measure)))).collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Aggregated { groups, rows_scanned, blocks_pruned })
    }

    /// Each facet's values and how many records of `[from, to]` that `search` matches have each,
    /// most first (ties by value), the first `top` of each: the explorer's facet list, every facet
    /// in one pass over the records.
    pub fn facet_counts(&self, search: &Search, from: i64, to: i64, facets: &[Facet], top: usize) -> Result<FacetCounts, AggregateError> {
        self.facet_counts_on(search, from, to, facets, top, None)
    }

    /// [`facet_counts`](Self::facet_counts), its scan split among `threads` (a test's), else as
    /// many as it is worth and are free.
    fn facet_counts_on(&self, search: &Search, from: i64, to: i64, facets: &[Facet], top: usize, threads: Option<usize>) -> Result<FacetCounts, AggregateError> {
        let init = || -> (Vec<Grouping>, u64) { (facets.iter().map(|_| Grouping::empty()).collect(), 0) };
        let fold = |(gs, matched): &mut (Vec<Grouping>, u64), r: &RecordRef<'_>| {
            *matched += 1;
            for (g, f) in gs.iter_mut().zip(facets) {
                g.take(r, std::slice::from_ref(f), &Measure::Count, 0, MAX_GROUPS)?;
            }
            Ok::<(), AggregateError>(())
        };
        let (states, rows_scanned, blocks_pruned) = self.each_match_split(search, from, to, threads, &init, &fold)?;
        let mut states = states.into_iter();
        let (mut gs, mut matched) = states.next().unwrap_or_else(init);
        for (others, m) in states {
            matched += m;
            for (g, o) in gs.iter_mut().zip(others) {
                g.merge(o, MAX_GROUPS)?;
            }
        }
        let facets = gs
            .into_iter()
            .map(|g| {
                let mut counts: Vec<(String, u64)> = vec![(String::new(), 0); g.keys.len()];
                for (c, a) in g.cells {
                    if let Some(slot) = counts.get_mut((c >> 32) as usize) {
                        slot.1 = a.n;
                    }
                }
                for (slot, k) in counts.iter_mut().zip(g.keys) {
                    slot.0 = k.into_iter().next().unwrap_or_default();
                }
                let distinct = counts.len();
                counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                counts.truncate(top);
                (counts, distinct)
            })
            .collect();
        Ok(FacetCounts { facets, matched, rows_scanned, blocks_pruned })
    }

    /// As [`aggregate`](Self::aggregate), each group in `buckets` of time: the records of `[from,
    /// to]` that fall in one.
    pub fn aggregate_buckets(&self, search: &Search, from: i64, to: i64, buckets: Buckets, by: &[Facet], measure: &Measure) -> Result<Bucketed, AggregateError> {
        self.aggregate_buckets_on(search, from, to, buckets, by, measure, None)
    }

    /// [`aggregate_buckets`](Self::aggregate_buckets), its scan split among `threads` (a test's),
    /// else as many as it is worth and are free; one, a sum's or an average's.
    #[allow(clippy::too_many_arguments)]
    fn aggregate_buckets_on(&self, search: &Search, from: i64, to: i64, buckets: Buckets, by: &[Facet], measure: &Measure, threads: Option<usize>) -> Result<Bucketed, AggregateError> {
        if buckets.n > MAX_BUCKETS {
            return Err(AggregateError::TooManyBuckets);
        }
        Grouping::new(by)?;
        let threads = if order_free(measure) { threads } else { Some(1) };
        let fold = |g: &mut Grouping, r: &RecordRef<'_>| match buckets.of(r.ts) {
            Some(b) => g.take(r, by, measure, b, MAX_CELLS),
            None => Ok(()),
        };
        let (states, rows_scanned, blocks_pruned) = self.each_match_split(search, from, to, threads, &Grouping::empty, &fold)?;
        let mut states = states.into_iter();
        let mut g = states.next().unwrap_or_else(Grouping::empty);
        for other in states {
            g.merge(other, MAX_CELLS)?;
        }
        // Each group's every bucket is in the answer: as many values as the cells could be.
        if g.keys.len().saturating_mul(buckets.n) > MAX_CELLS {
            return Err(AggregateError::TooManyCells);
        }
        let mut series: Vec<Vec<Option<f64>>> = std::iter::repeat_with(|| vec![None; buckets.n]).take(g.keys.len()).collect();
        for (c, a) in g.cells {
            if let Some(v) = series.get_mut((c >> 32) as usize).and_then(|s| s.get_mut((c & u64::from(u32::MAX)) as usize)) {
                *v = Some(a.value(measure));
            }
        }
        let mut groups: Vec<(Vec<String>, Vec<Option<f64>>)> = g.keys.into_iter().zip(series).collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Bucketed { buckets, groups, rows_scanned, blocks_pruned })
    }
}

/// Two records of one time, by their contents: severity, service, host, source, message,
/// attributes, trace and span.
fn cmp_contents(a: &RecordRef<'_>, b: &Record) -> std::cmp::Ordering {
    a.severity
        .cmp(&b.severity)
        .then_with(|| a.service.cmp(b.service.as_str()))
        .then_with(|| a.host.cmp(b.host.as_str()))
        .then_with(|| a.source.cmp(b.source.as_str()))
        .then_with(|| a.message.cmp(b.message.as_str()))
        .then_with(|| a.attrs().cmp(b.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str()))))
        .then_with(|| a.trace_id.cmp(&b.trace_id))
        .then_with(|| a.span_id.cmp(&b.span_id))
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

    /// The store of 30,000 records the tests read: in batches of 500, several blocks sealed, two
    /// files written.
    fn store_of_30k(dir: &Path) -> LogStore {
        let mut s = LogStore::default();
        for chunk in (0..30_000).collect::<Vec<i64>>().chunks(500) {
            let recs: Vec<Record> = chunk.iter().map(|&i| rec(i)).collect();
            let (raw, n) = segment(&recs);
            s.append(raw, n).unwrap();
            if chunk[0] == 10_000 || chunk[0] == 20_000 {
                let path = dir.join(format!("{}.logs", chunk[0]));
                let f = s.cut(&path).unwrap().unwrap();
                s.files.push(HeldFile { seq: chunk[0] as u64, file: path.file_name().unwrap().to_string_lossy().into_owned(), log: f });
            }
        }
        s
    }

    #[test]
    fn bucketed_aggregations_agree_with_counting_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_of_30k(dir.path());
        let sec = 1_000_000_000i64;
        let searches = [("*", None), ("status:error", Some(0))];
        let bys: [&[Facet]; 3] = [&[], &[Facet::Service], &[Facet::Service, Facet::Host]];
        let measures = [Measure::Count, Measure::Sum("n".into()), Measure::Max("n".into()), Measure::Cardinality(Facet::Host)];
        let mut cases = 0;
        for (q, every_tenth) in searches {
            let search = parse(q).unwrap();
            for (from, to) in [(0, 29_999 * sec), (123 * sec + 1, 4_567 * sec), (29_990 * sec, 40_000 * sec)] {
                for step in [sec, 7 * sec, 60 * sec, 1_000 * sec] {
                    let Ok(buckets) = Buckets::over(from, to, step) else { continue };
                    for by in bys {
                        for m in &measures {
                            let got = s.aggregate_buckets(&search, from, to, buckets, by, m).unwrap();
                            // By hand: every record, its bucket, its group.
                            let mut want: std::collections::BTreeMap<Vec<String>, Vec<Vec<i64>>> = std::collections::BTreeMap::new();
                            for i in 0..30_000i64 {
                                let ts = i * sec;
                                if ts < from || ts > to || every_tenth.is_some_and(|_| i % 10 != 0) {
                                    continue;
                                }
                                let r = rec(i);
                                let key: Vec<String> = by.iter().map(|f| if *f == Facet::Service { r.service.clone() } else { r.host.clone() }).collect();
                                let b = ((ts - buckets.start) / step) as usize;
                                let cells = want.entry(key).or_insert_with(|| vec![Vec::new(); buckets.n]);
                                cells[b].push(i);
                            }
                            let want: Vec<(Vec<String>, Vec<Option<f64>>)> = want
                                .into_iter()
                                .map(|(k, cells)| {
                                    let vals = cells
                                        .iter()
                                        .map(|is| {
                                            (!is.is_empty()).then(|| match m {
                                                Measure::Count => is.len() as f64,
                                                Measure::Sum(_) => is.iter().sum::<i64>() as f64,
                                                Measure::Max(_) => *is.iter().max().unwrap() as f64,
                                                _ => is.iter().map(|i| i % 4).collect::<std::collections::BTreeSet<_>>().len() as f64,
                                            })
                                        })
                                        .collect();
                                    (k, vals)
                                })
                                .collect();
                            assert_eq!(got.groups, want, "{q} [{from}, {to}] by {step} by {by:?} {m:?}");
                            // The buckets add up to the whole, counted.
                            if *m == Measure::Count {
                                let whole = s.aggregate(&search, from, to, by, m).unwrap();
                                let summed: Vec<(Vec<String>, f64)> = got.groups.iter().map(|(k, v)| (k.clone(), v.iter().flatten().sum())).collect();
                                assert_eq!(whole.groups, summed);
                            }
                            cases += 1;
                        }
                    }
                }
            }
        }
        // Steps that would make more than MAX_BUCKETS are left out: 192 of the 288.
        assert_eq!(cases, 192);
        // Every facet in one pass: as each counted alone, most first.
        let all = parse("*").unwrap();
        let fc = s.facet_counts(&all, 0, i64::MAX, &[Facet::Service, Facet::Host, Facet::Status], 3).unwrap();
        assert_eq!(fc.matched, 30_000);
        for ((counts, distinct), f) in fc.facets.iter().zip([Facet::Service, Facet::Host, Facet::Status]) {
            let mut alone: Vec<(String, u64)> = s.aggregate(&all, 0, i64::MAX, std::slice::from_ref(&f), &Measure::Count).unwrap().groups.into_iter().map(|(k, v)| (k[0].clone(), v as u64)).collect();
            assert_eq!(*distinct, alone.len());
            alone.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            alone.truncate(3);
            assert_eq!(*counts, alone, "{f:?}");
        }
        // Bounds: a step of nothing, too many buckets, too many facets.
        assert!(matches!(Buckets::over(0, 10, 0), Err(AggregateError::Step)));
        assert!(matches!(Buckets::over(0, MAX_BUCKETS as i64 * sec, sec), Err(AggregateError::TooManyBuckets)));
        assert!(Buckets::over(0, (MAX_BUCKETS as i64 - 1) * sec, sec).is_ok());
        let nine = [Facet::Service, Facet::Host, Facet::Source, Facet::Status, Facet::Service, Facet::Host, Facet::Source, Facet::Status, Facet::Service];
        assert!(matches!(s.aggregate(&parse("*").unwrap(), 0, i64::MAX, &nine, &Measure::Count), Err(AggregateError::TooManyFacets)));
        // More groups than kept: refused, not cut short.
        assert!(matches!(s.aggregate(&parse("*").unwrap(), 0, i64::MAX, &[Facet::Attr("n".into())], &Measure::Count), Err(AggregateError::TooManyGroups)));
    }

    /// A scan split among threads answers as one thread does, to the bit, however many and whatever
    /// the blocks (the head, sealed ones, files'), for every measure that does not hang on the order
    /// records come in, bucketed or whole, and the explorer's facets.
    #[test]
    fn a_scan_split_among_threads_answers_as_one_thread() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_of_30k(dir.path());
        let sec = 1_000_000_000i64;
        let measures = [Measure::Count, Measure::Min("n".into()), Measure::Max("n".into()), Measure::Cardinality(Facet::Host)];
        let bys: [&[Facet]; 3] = [&[], &[Facet::Service], &[Facet::Service, Facet::Host]];
        let mut cases = 0;
        for q in ["*", "status:error", "service:web -status:error", "failed"] {
            let search = parse(q).unwrap();
            for (from, to) in [(0, 29_999 * sec), (123 * sec + 1, 24_567 * sec)] {
                for by in bys {
                    for m in &measures {
                        let one = s.aggregate_on(&search, from, to, by, m, Some(1)).unwrap();
                        let buckets = Buckets::over(from, to, 600 * sec).unwrap();
                        let one_b = s.aggregate_buckets_on(&search, from, to, buckets, by, m, Some(1)).unwrap();
                        for t in 2..=5 {
                            let many = s.aggregate_on(&search, from, to, by, m, Some(t)).unwrap();
                            assert_eq!((&many.groups, many.rows_scanned, many.blocks_pruned), (&one.groups, one.rows_scanned, one.blocks_pruned), "{q} by {by:?} {m:?} on {t}");
                            let many_b = s.aggregate_buckets_on(&search, from, to, buckets, by, m, Some(t)).unwrap();
                            assert_eq!((&many_b.groups, many_b.rows_scanned), (&one_b.groups, one_b.rows_scanned), "{q} by {by:?} {m:?} in buckets on {t}");
                            cases += 1;
                        }
                    }
                }
                let facets = [Facet::Service, Facet::Host, Facet::Status];
                let one = s.facet_counts_on(&search, from, to, &facets, 3, Some(1)).unwrap();
                for t in 2..=5 {
                    let many = s.facet_counts_on(&search, from, to, &facets, 3, Some(t)).unwrap();
                    assert_eq!((&many.facets, many.matched, many.rows_scanned), (&one.facets, one.matched, one.rows_scanned), "{q}: facets on {t}");
                }
            }
        }
        assert_eq!(cases, 4 * 2 * 3 * 4 * 4);
        // A sum's scan stays one thread's, its bits the order's.
        let sum = s.aggregate_on(&parse("*").unwrap(), 0, i64::MAX, &[], &Measure::Sum("n".into()), Some(4)).unwrap();
        assert_eq!(sum.groups, vec![(Vec::new(), (0..30_000i64).sum::<i64>() as f64)]);
    }

    /// Blocks of spans that share what every block of a service holds (its name, its kind, its
    /// resource) and each its traces: a trace found reads its own block and about one in a hundred
    /// besides, whichever trace, as the filters are sized for (unsalted, the places of a query fell
    /// on the bits the shared tokens set in every filter alike, and some traces read most blocks).
    #[test]
    fn a_trace_reads_its_own_block_and_about_one_in_a_hundred_besides_whichever_trace() {
        let mut s = LogStore::spans();
        let id = |t: u64| (u128::from(t) * 0x9e37_79b9_7f4a_7c15).to_le_bytes();
        for b in 0..400u64 {
            let mut raw = Vec::new();
            let mut prev = 0;
            for i in 0..510u64 {
                let trace = b * 1_000 + i / 3;
                let r = Record {
                    ts: (b * 510 + i) as i64 * 1_000_000,
                    severity: 9,
                    service: "checkout".into(),
                    host: "web-1".into(),
                    source: "opentelemetry.instrumentation.flask".into(),
                    message: "GET /checkout/{id}".into(),
                    attrs: vec![("span.kind".into(), "server".into()), ("url.path".into(), format!("/checkout/{}", trace % 1_000)), ("pad".into(), "p".repeat(420))],
                    trace_id: Some(id(trace)),
                    span_id: Some((b * 1_000 + i + 1).to_le_bytes()),
                };
                put_row(&mut raw, &r, prev);
                prev = r.ts;
            }
            // Each request's spans a block of its own (past the block's text).
            assert!(raw.len() >= BLOCK_TEXT);
            s.append(raw, 510).unwrap();
        }
        assert_eq!(s.sealed.len(), 400);
        let mut reads: Vec<u64> = (0..300u64)
            .map(|q| {
                let t = (q * 7_919 % 400) * 1_000 + 77;
                let found = s.search(&Search::trace(id(t)), i64::MIN, i64::MAX, 100).unwrap();
                assert_eq!(found.records.len(), 3);
                found.blocks_total - found.blocks_pruned
            })
            .collect();
        reads.sort_unstable();
        // Its own block, and of 399 others each 1% (Binomial(399, 0.01): mean 4, P(> 19) < 1e-7).
        let (median, most) = (reads[150], reads[299]);
        assert!(median <= 8, "median {median}");
        assert!(most <= 20, "the worst trace read {most} of 400 blocks");
    }

    #[test]
    fn searches_find_the_newest_matches_across_head_sealed_blocks_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = store_of_30k(dir.path());
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
        // Analytics: every match counted, by service, across head, sealed blocks and files.
        let agg = s.aggregate(&parse("status:error").unwrap(), 0, i64::MAX, &[Facet::Service], &Measure::Count).unwrap();
        // Errors are every 10th (3,000); i % 3 picks the service: 0 → api, 1 → web, 2 → db.
        let want: Vec<(Vec<String>, f64)> = vec![(vec!["api".into()], 1000.0), (vec!["db".into()], 1000.0), (vec!["web".into()], 1000.0)];
        assert_eq!(agg.groups, want);
        // A numeric attribute: the sum of n over records 0..100 (4,950); its distinct hosts.
        let some = s.aggregate(&parse("*").unwrap(), 0, 99 * 1_000_000_000, &[], &Measure::Sum("n".into())).unwrap();
        assert_eq!(some.groups, vec![(Vec::<String>::new(), 4950.0)]);
        let hosts = s.aggregate(&parse("*").unwrap(), 0, i64::MAX, &[Facet::Status], &Measure::Cardinality(Facet::Host)).unwrap();
        assert_eq!(hosts.groups, vec![(vec!["error".into()], 2.0), (vec!["info".into()], 4.0)]);
        // A word no block holds: every block ruled out, nothing read but the head.
        let none = s.aggregate(&parse("nosuchwordanywhere").unwrap(), 0, i64::MAX, &[Facet::Host], &Measure::Count).unwrap();
        assert!(none.groups.is_empty() && none.blocks_pruned > 0);
    }
}
