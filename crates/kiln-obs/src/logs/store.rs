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
            if summary.is_some_and(|b| search.block(b) == Maybe::No) {
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

    pub fn name(&self) -> String {
        match self {
            Self::Service => "service".into(),
            Self::Host => "host".into(),
            Self::Source => "source".into(),
            Self::Status => "status".into(),
            Self::Attr(k) => format!("@{k}"),
        }
    }

    fn of<'a>(&self, r: &RecordRef<'a>) -> Option<&'a str> {
        match self {
            Self::Service => Some(r.service),
            Self::Host => Some(r.host),
            Self::Source => Some(r.source),
            Self::Status => Some(super::record::level(r.severity)),
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
        Ok(Self { hash: crate::hash::Keyed::new(), keys: Vec::new(), by_key: hashbrown::HashTable::new(), cells: hashbrown::HashTable::new() })
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
}

impl LogStore {
    /// Every record in `[from, to]` (ns) that `search` matches, to `f`, across the head, sealed
    /// blocks and files (blocks a search rules out are never read); the rows read and the blocks
    /// ruled out.
    /// Every record in `[from, to]` (ns) of every head segment, sealed block and file, in no order,
    /// handed to `f`: one at a time, none kept.
    pub fn each_record(&self, from: i64, to: i64, mut f: impl FnMut(&RecordRef<'_>)) -> Result<(), LogStoreError> {
        self.each_match(&Search::all(), from, to, |r| {
            f(r);
            Ok::<(), LogStoreError>(())
        })
        .map(|_| ())
    }

    fn each_match<E: From<LogStoreError>>(&self, search: &Search, from: i64, to: i64, mut f: impl FnMut(&RecordRef<'_>) -> Result<(), E>) -> Result<(u64, u64), E> {
        let (mut rows_scanned, mut blocks_pruned) = (0u64, 0u64);
        if to < from {
            return Ok((0, 0));
        }
        let mut scratch = RowScratch::default();
        let mut row = |r: &RecordRef<'_>, rows_scanned: &mut u64| -> Result<(), E> {
            *rows_scanned += 1;
            if r.ts < from || r.ts > to || !search.row(r, &mut scratch) {
                return Ok(());
            }
            f(r)
        };
        for (seg, n) in &self.head {
            let mut rows = Rows::new(seg, *n as usize);
            while let Some(r) = rows.next_row().map_err(LogStoreError::from)? {
                row(&r, &mut rows_scanned)?;
            }
        }
        let (mut text, mut raw) = (Vec::new(), Vec::new());
        let consider = |b: &LogBlock| b.max_ts >= from && b.min_ts <= to && search.block(b) != Maybe::No;
        for b in &self.sealed {
            if !consider(b) {
                blocks_pruned += 1;
                continue;
            }
            b.decompress(&mut raw).map_err(|_| LogStoreError::Text)?;
            let mut rows = b.rows(&raw);
            while let Some(r) = rows.next_row().map_err(LogStoreError::from)? {
                row(&r, &mut rows_scanned)?;
            }
        }
        for file in &self.files {
            for (i, b) in file.log.blocks.iter().enumerate() {
                if !consider(b) {
                    blocks_pruned += 1;
                    continue;
                }
                file.log.read_text(i, &mut text).map_err(LogStoreError::from)?;
                b.decompress_text(&text, &mut raw).map_err(|_| LogStoreError::Text)?;
                let mut rows = b.rows(&raw);
                while let Some(r) = rows.next_row().map_err(LogStoreError::from)? {
                    row(&r, &mut rows_scanned)?;
                }
            }
        }
        Ok((rows_scanned, blocks_pruned))
    }

    /// Every record in `[from, to]` (ns) that `search` matches, grouped by `by` and measured: log
    /// analytics' `count() by {service}`, and what a log monitor counts. Every matching record
    /// counts.
    pub fn aggregate(&self, search: &Search, from: i64, to: i64, by: &[Facet], measure: &Measure) -> Result<Aggregated, AggregateError> {
        let mut g = Grouping::new(by)?;
        let (rows_scanned, blocks_pruned) = self.each_match(search, from, to, |r| g.take(r, by, measure, 0, MAX_GROUPS))?;
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
        let mut gs: Vec<Grouping> = facets.iter().map(|_| Grouping::new(&[])).collect::<Result<_, _>>()?;
        let mut matched = 0u64;
        let (rows_scanned, blocks_pruned) = self.each_match(search, from, to, |r| {
            matched += 1;
            for (g, f) in gs.iter_mut().zip(facets) {
                g.take(r, std::slice::from_ref(f), &Measure::Count, 0, MAX_GROUPS)?;
            }
            Ok::<(), AggregateError>(())
        })?;
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
        if buckets.n > MAX_BUCKETS {
            return Err(AggregateError::TooManyBuckets);
        }
        let mut g = Grouping::new(by)?;
        let (rows_scanned, blocks_pruned) = self.each_match(search, from, to, |r| match buckets.of(r.ts) {
            Some(b) => g.take(r, by, measure, b, MAX_CELLS),
            None => Ok(()),
        })?;
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
