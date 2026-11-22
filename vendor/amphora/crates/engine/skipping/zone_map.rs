//! Zone maps over leaf runs (G5 step 4): each zone covers a contiguous span of rowids, from its
//! `start` up to the next zone's start (the last zone to the largest rowid), and holds the min and
//! max of the integer values its rows carry, with counts that let it tighten once it holds none.
//!
//! Invariant (T-G5.4.1 checks it after every write): every live row's value lies in its zone's
//! `[min, max]`, or the value isn't an integer and the zone's `others` count is above zero. A write
//! only widens a zone or lowers its counts, so the invariant holds without reading the table again.

use super::{RowWrite, SkipError};

/// A span of rowids a scan reads, both ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowidRange {
    pub first: i64,
    pub last: i64,
}

#[derive(Debug, Clone, Copy)]
struct Zone {
    /// The smallest rowid the zone covers.
    start: i64,
    /// The largest rowid written into the zone (where a tail zone ends and the next begins).
    last_rowid: i64,
    min: i64,
    max: i64,
    /// Rows whose value is an integer: `min`/`max` mean something only while this is above zero.
    ints: u64,
    /// Rows whose value isn't an integer: while above zero, the zone matches every filter.
    others: u64,
}

impl Zone {
    fn new(start: i64) -> Self {
        Zone {
            start,
            last_rowid: start,
            min: 0,
            max: 0,
            ints: 0,
            others: 0,
        }
    }

    fn rows(&self) -> Result<u64, SkipError> {
        self.ints
            .checked_add(self.others)
            .ok_or(SkipError::Overflow)
    }

    fn add(&mut self, rowid: i64, value: Option<i64>) -> Result<(), SkipError> {
        self.last_rowid = self.last_rowid.max(rowid);
        match value {
            Some(v) => {
                if self.ints == 0 {
                    self.min = v;
                    self.max = v;
                } else {
                    self.min = self.min.min(v);
                    self.max = self.max.max(v);
                }
                self.ints = self.ints.checked_add(1).ok_or(SkipError::Overflow)?;
            }
            None => self.others = self.others.checked_add(1).ok_or(SkipError::Overflow)?,
        }
        Ok(())
    }

    /// A row left: its counts fall; the bounds stay (a loose bound never misses a row) until the
    /// zone holds no integer at all.
    fn remove(&mut self, rowid: i64, value: Option<i64>) -> Result<(), SkipError> {
        let count = match value {
            Some(_) => &mut self.ints,
            None => &mut self.others,
        };
        *count = count
            .checked_sub(1)
            .ok_or(SkipError::MissingRow { rowid, value })?;
        Ok(())
    }

    fn may_match(&self, lo: i64, hi: i64) -> bool {
        self.others > 0 || (self.ints > 0 && self.min <= hi && self.max >= lo)
    }
}

/// Builds a [`ZoneMap`] from a table's rows in B-tree order, one zone per leaf: the scan that
/// feeds it calls [`ZoneMapBuilder::end_run`] where a leaf ends, so a zone holds what a leaf holds.
#[derive(Debug, Default)]
pub struct ZoneMapBuilder {
    zones: Vec<Zone>,
    open: Option<Zone>,
    previous: Option<i64>,
    rows: u64,
}

impl ZoneMapBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next row of the table, in rowid order.
    pub fn row(&mut self, rowid: i64, value: Option<i64>) -> Result<(), SkipError> {
        if let Some(previous) = self.previous {
            if rowid <= previous {
                return Err(SkipError::RowidOrder { previous, rowid });
            }
        }
        self.previous = Some(rowid);
        self.rows = self.rows.checked_add(1).ok_or(SkipError::Overflow)?;
        self.open
            .get_or_insert_with(|| Zone::new(rowid))
            .add(rowid, value)
    }

    /// The leaf the rows came from ended: the next row starts a new zone.
    pub fn end_run(&mut self) {
        if let Some(zone) = self.open.take() {
            self.zones.push(zone);
        }
    }

    pub fn finish(mut self) -> ZoneMap {
        self.end_run();
        // The leaves' measured fill: the rows a run held on average, which a growing tail reaches
        // before it starts a new zone. An empty table measured none: its tail grows as one zone
        // until the map is built again from the leaves its rows filled.
        let runs = u64::try_from(self.zones.len()).unwrap_or(u64::MAX);
        let run_rows = self.rows.checked_div(runs).filter(|rows| *rows > 0);
        ZoneMap {
            zones: self.zones,
            run_rows,
            stats: ZoneStats::default(),
        }
    }
}

/// What the zone map did (non-vacuity counters for the tests and the harness).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZoneStats {
    pub zones_read: u64,
    pub zones_skipped: u64,
    pub writes: u64,
    pub zones_split: u64,
}

/// Min/max summaries over a table's leaf runs, kept from the write path.
#[derive(Debug)]
pub struct ZoneMap {
    /// Ascending by `start`, disjoint.
    zones: Vec<Zone>,
    /// The rows a built leaf run held on average; `None` when built from no rows.
    run_rows: Option<u64>,
    stats: ZoneStats,
}

impl ZoneMap {
    pub fn stats(&self) -> ZoneStats {
        self.stats
    }

    pub fn zone_count(&self) -> usize {
        self.zones.len()
    }

    /// The rows the zones hold, all told.
    pub fn rows(&self) -> Result<u64, SkipError> {
        self.zones.iter().try_fold(0u64, |sum, zone| {
            sum.checked_add(zone.rows()?).ok_or(SkipError::Overflow)
        })
    }

    /// The zone covering `rowid`, made or widened to cover it when no zone does yet.
    fn zone_for_insert(&mut self, rowid: i64) -> Result<&mut Zone, SkipError> {
        let after = self.zones.partition_point(|zone| zone.start <= rowid);
        let index = match after.checked_sub(1) {
            Some(index) => {
                let tail = after == self.zones.len();
                let full = match self.zones.get(index) {
                    Some(zone) => match self.run_rows {
                        Some(run_rows) => zone.rows()? >= run_rows && rowid > zone.last_rowid,
                        None => false,
                    },
                    None => false,
                };
                if tail && full {
                    // Appended past a full tail: a new leaf would start here, so a new zone does.
                    self.zones.push(Zone::new(rowid));
                    self.stats.zones_split = self
                        .stats
                        .zones_split
                        .checked_add(1)
                        .ok_or(SkipError::Overflow)?;
                    after
                } else {
                    index
                }
            }
            None => {
                // Below every zone (or none yet): the first zone reaches down to it.
                match self.zones.first_mut() {
                    Some(first) => first.start = rowid,
                    None => self.zones.push(Zone::new(rowid)),
                }
                0
            }
        };
        self.zones.get_mut(index).ok_or(SkipError::Inconsistent)
    }

    fn zone_for_existing(
        &mut self,
        rowid: i64,
        value: Option<i64>,
    ) -> Result<&mut Zone, SkipError> {
        let after = self.zones.partition_point(|zone| zone.start <= rowid);
        after
            .checked_sub(1)
            .and_then(|index| self.zones.get_mut(index))
            .ok_or(SkipError::MissingRow { rowid, value })
    }

    /// The write path's hook for this summary.
    pub fn on_write(&mut self, write: RowWrite) -> Result<(), SkipError> {
        self.stats.writes = self
            .stats
            .writes
            .checked_add(1)
            .ok_or(SkipError::Overflow)?;
        match write {
            RowWrite::Insert { rowid, value } => self.zone_for_insert(rowid)?.add(rowid, value),
            RowWrite::Delete { rowid, old } => {
                self.zone_for_existing(rowid, old)?.remove(rowid, old)
            }
            RowWrite::Update { rowid, old, new } => {
                let zone = self.zone_for_existing(rowid, old)?;
                zone.remove(rowid, old)?;
                zone.add(rowid, new)
            }
        }
    }

    /// The rowid ranges a filter `lo <= value <= hi` may find rows in: each zone that may match,
    /// adjacent ones joined into one range.
    pub fn candidate_ranges(&mut self, lo: i64, hi: i64) -> Vec<RowidRange> {
        let mut ranges: Vec<RowidRange> = Vec::new();
        let mut read: u64 = 0;
        let mut skipped: u64 = 0;
        let mut previous_matched = false;
        let mut zones = self.zones.iter().peekable();
        while let Some(zone) = zones.next() {
            let matched = zone.may_match(lo, hi);
            if matched {
                read = read.saturating_add(1);
                // A zone ends where the next begins; the last reaches every larger rowid.
                let last = zones
                    .peek()
                    .and_then(|next| next.start.checked_sub(1))
                    .unwrap_or(i64::MAX);
                match ranges.last_mut() {
                    Some(range) if previous_matched => range.last = last,
                    _ => ranges.push(RowidRange {
                        first: zone.start,
                        last,
                    }),
                }
            } else {
                skipped = skipped.saturating_add(1);
            }
            previous_matched = matched;
        }
        // Counters for the harness: saturating is their stated meaning (a count past u64 stays at
        // its top).
        self.stats.zones_read = self.stats.zones_read.saturating_add(read);
        self.stats.zones_skipped = self.stats.zones_skipped.saturating_add(skipped);
        ranges
    }

    /// Checks the invariant against the live rows (T-G5.4.1): every row's value is within its zone.
    pub fn covers(&self, rowid: i64, value: Option<i64>) -> bool {
        let after = self.zones.partition_point(|zone| zone.start <= rowid);
        match after.checked_sub(1).and_then(|index| self.zones.get(index)) {
            Some(zone) => match value {
                Some(v) => zone.ints > 0 && (zone.min..=zone.max).contains(&v),
                None => zone.others > 0,
            },
            None => false,
        }
    }
}
