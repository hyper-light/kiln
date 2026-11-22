//! Database cracking (G5 step 4; Idreos, Kersten and Manegold, "Database Cracking", CIDR 2007, and
//! "Updating a Cracked Database", SIGMOD 2007): a copy of one column as (value, rowid) pairs that
//! each filter partitions in place at its bounds. A crack at bound `b` sits at position `p`: every
//! entry before `p` is below `b`, every entry from `p` on is at or above it. A filter repeated on
//! the column finds both its bounds already cracked and reads only its piece.
//!
//! Writes ripple (the SIGMOD 2007 paper's Ripple, applied at the write rather than deferred to the
//! next query): an insert goes to the end of its value's piece by moving one entry per crack above
//! it, a delete leaves by the same moves backwards. Its cost, one swap per crack above the value plus
//! the search of the row's piece on a delete, is what [`CrackerStats::entries_moved`] and
//! [`CrackerStats::entries_searched`] count.
//!
//! Bounds: one entry per row of the table (the column's copy, charged as the table's rows are), and
//! at most one crack more than there are entries: past that, a filter's bound is still found by
//! partitioning but isn't kept (an extra crack can't split a piece the others haven't).

use super::{RowWrite, SkipError};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound::{Excluded, Included, Unbounded};

/// What cracking did: its scan cost (entries partitioned and returned) and write cost (entries
/// searched and moved), counted for the tests and the harness.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CrackerStats {
    pub queries: u64,
    pub cracks: u64,
    pub entries_partitioned: u64,
    pub entries_returned: u64,
    pub writes: u64,
    pub entries_searched: u64,
    pub entries_moved: u64,
}

/// One column cracked by the filters asked of it, kept from the write path.
#[derive(Debug, Default)]
pub struct CrackerColumn {
    /// (value, rowid), partitioned at every crack.
    entries: Vec<(i64, i64)>,
    /// Bound -> position (see the module's comment).
    cracks: BTreeMap<i64, usize>,
    /// Rowids whose value isn't an integer: a candidate for every filter.
    others: BTreeSet<i64>,
    stats: CrackerStats,
}

/// Adds to a cost counter; saturating is the counter's stated meaning (past u64 it stays at its top).
fn count(counter: &mut u64, n: usize) {
    *counter = counter.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
}

impl CrackerColumn {
    /// The column's copy, from the table's rows (in any order).
    pub fn new(rows: impl IntoIterator<Item = (i64, Option<i64>)>) -> Self {
        let mut column = CrackerColumn::default();
        for (rowid, value) in rows {
            match value {
                Some(v) => column.entries.push((v, rowid)),
                None => {
                    column.others.insert(rowid);
                }
            }
        }
        column
    }

    pub fn stats(&self) -> CrackerStats {
        self.stats
    }

    pub fn crack_count(&self) -> usize {
        self.cracks.len()
    }

    /// The piece that holds (or would hold) `value`: from the crack at the largest bound at or
    /// below it to the crack at the smallest bound above it.
    fn piece_of(&self, value: i64) -> (usize, usize) {
        let start = self
            .cracks
            .range((Unbounded, Included(value)))
            .next_back()
            .map_or(0, |(_, position)| *position);
        let end = self
            .cracks
            .range((Excluded(value), Unbounded))
            .next()
            .map_or(self.entries.len(), |(_, position)| *position);
        (start, end)
    }

    /// The position of `bound`: partitions its piece at it the first time it is asked.
    fn crack(&mut self, bound: i64) -> Result<usize, SkipError> {
        if let Some(position) = self.cracks.get(&bound) {
            return Ok(*position);
        }
        let start = self
            .cracks
            .range((Unbounded, Excluded(bound)))
            .next_back()
            .map_or(0, |(_, position)| *position);
        let end = self
            .cracks
            .range((Excluded(bound), Unbounded))
            .next()
            .map_or(self.entries.len(), |(_, position)| *position);
        let piece = self
            .entries
            .get_mut(start..end)
            .ok_or(SkipError::Inconsistent)?;
        let mut split: usize = 0;
        for index in 0..piece.len() {
            if piece.get(index).is_some_and(|(value, _)| *value < bound) {
                piece.swap(split, index);
                split = split.checked_add(1).ok_or(SkipError::Overflow)?;
            }
        }
        count(&mut self.stats.entries_partitioned, piece.len());
        let position = start.checked_add(split).ok_or(SkipError::Overflow)?;
        if self.cracks.len() <= self.entries.len() {
            self.cracks.insert(bound, position);
            self.stats.cracks = self.stats.cracks.saturating_add(1);
        }
        Ok(position)
    }

    /// The rowids a filter `lo <= value <= hi` may need, ascending: its piece, and every row whose
    /// value isn't an integer.
    pub fn candidate_rowids(&mut self, lo: i64, hi: i64) -> Result<Vec<i64>, SkipError> {
        self.stats.queries = self.stats.queries.saturating_add(1);
        let mut rowids: Vec<i64> = self.others.iter().copied().collect();
        let (from, to) = self.cracked(lo, hi)?;
        let piece = self.entries.get(from..to).ok_or(SkipError::Inconsistent)?;
        count(&mut self.stats.entries_returned, piece.len());
        rowids.extend(piece.iter().map(|(_, rowid)| *rowid));
        rowids.sort_unstable();
        Ok(rowids)
    }

    /// The positions of the piece `lo <= value <= hi` cuts out, cracking at both bounds.
    fn cracked(&mut self, lo: i64, hi: i64) -> Result<(usize, usize), SkipError> {
        if lo > hi {
            return Ok((0, 0));
        }
        let from = self.crack(lo)?;
        let to = match hi.checked_add(1) {
            Some(above) => self.crack(above)?,
            None => self.entries.len(),
        };
        Ok((from, to))
    }

    /// The (value, rowid) entries a filter `lo <= value <= hi` matches among the integers, as the
    /// cracker holds them (T-G5.4.1 checks each against the table: none stale).
    pub fn matching_entries(&mut self, lo: i64, hi: i64) -> Result<Vec<(i64, i64)>, SkipError> {
        let (from, to) = self.cracked(lo, hi)?;
        Ok(self
            .entries
            .get(from..to)
            .ok_or(SkipError::Inconsistent)?
            .to_vec())
    }

    /// The rowids whose value isn't an integer.
    pub fn others(&self) -> impl Iterator<Item = i64> + '_ {
        self.others.iter().copied()
    }

    /// The write path's hook for this summary.
    pub fn on_write(&mut self, write: RowWrite) -> Result<(), SkipError> {
        self.stats.writes = self.stats.writes.saturating_add(1);
        match write {
            RowWrite::Insert { rowid, value } => self.insert(rowid, value),
            RowWrite::Delete { rowid, old } => self.remove(rowid, old),
            RowWrite::Update { rowid, old, new } => {
                self.remove(rowid, old)?;
                self.insert(rowid, new)
            }
        }
    }

    fn insert(&mut self, rowid: i64, value: Option<i64>) -> Result<(), SkipError> {
        let Some(value) = value else {
            self.others.insert(rowid);
            return Ok(());
        };
        // The new entry starts at the end and moves down: each crack above its value, from the
        // highest, gives up its piece's first entry to the hole and moves up by one.
        let mut hole = self.entries.len();
        self.entries.push((value, rowid));
        for position in self
            .cracks
            .range_mut((Excluded(value), Unbounded))
            .rev()
            .map(|(_, p)| p)
        {
            self.entries.swap(*position, hole);
            hole = *position;
            *position = position.checked_add(1).ok_or(SkipError::Overflow)?;
            self.stats.entries_moved = self.stats.entries_moved.saturating_add(1);
        }
        Ok(())
    }

    fn remove(&mut self, rowid: i64, value: Option<i64>) -> Result<(), SkipError> {
        let missing = SkipError::MissingRow { rowid, value };
        let Some(value) = value else {
            return if self.others.remove(&rowid) {
                Ok(())
            } else {
                Err(missing)
            };
        };
        let (start, end) = self.piece_of(value);
        let piece = self
            .entries
            .get(start..end)
            .ok_or(SkipError::Inconsistent)?;
        let found = piece.iter().position(|entry| *entry == (value, rowid));
        count(
            &mut self.stats.entries_searched,
            found.map_or(piece.len(), |at| at.saturating_add(1)),
        );
        let at = found.and_then(|at| start.checked_add(at)).ok_or(missing)?;
        // The entry moves to its piece's end, then the hole moves up: each crack above it moves
        // down by one, its piece's last entry taking the hole at its front.
        let mut hole = end.checked_sub(1).ok_or(SkipError::Overflow)?;
        self.entries.swap(at, hole);
        let above: Vec<usize> = self
            .cracks
            .range((Excluded(value), Unbounded))
            .map(|(_, position)| *position)
            .collect();
        let ends = above
            .iter()
            .skip(1)
            .copied()
            .chain(std::iter::once(self.entries.len()));
        for (position, piece_end) in self
            .cracks
            .range_mut((Excluded(value), Unbounded))
            .map(|(_, p)| p)
            .zip(ends)
        {
            let last = piece_end.checked_sub(1).ok_or(SkipError::Overflow)?;
            self.entries.swap(hole, last);
            hole = last;
            *position = position.checked_sub(1).ok_or(SkipError::Overflow)?;
            self.stats.entries_moved = self.stats.entries_moved.saturating_add(1);
        }
        match self.entries.pop() {
            Some(entry) if entry == (value, rowid) => Ok(()),
            _ => Err(SkipError::Inconsistent),
        }
    }
}
