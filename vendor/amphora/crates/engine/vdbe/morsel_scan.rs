//! Morsels over a table's own pages (QT.6, the query-speed plan's step 6; G5 step 3): each worker
//! opens its own cursor over a run of the table b-tree's leaf pages, in the statement's one
//! snapshot (every cursor reads through the statement's pager), and runs the pipeline's loop body
//! (`morsel.rs`: the scan's filter and aggregates, or a hash join's probe) over the rows it finds.
//! The partials merge as `morsel.rs`'s do. docs/design/executor.md section 8 states the design.
//!
//! A morsel is a run of leaf pages, so the unit `choose_degree` sizes is a page: the pilot measures
//! a page's cost, and the model picks the workers and the pages per morsel. The leaves are found
//! by reading the table's interior pages level by level (no recursion; a level's pages in key
//! order), keeping each leaf's greatest rowid from its parent's divider. A worker's cursor seeks
//! past the divider below its first leaf and reads until a rowid passes the divider above its last.
//! A hash join's probe with its aggregates (`TableJoinAggregate`) builds its table once from the
//! build side's filtered rows, then each worker probes and folds over its own leaves.

use std::ops::Range;

use super::morsel::{
    execute, execute_at, Degree, HashProbe, MachineCosts, MemoryBudget, MorselError, ParallelMode,
    Pipeline, Predicate, RunStats, ScanAggregate,
};
use crate::storage::btree::{BTreeCursor, CursorTrait};
use crate::storage::pager::{PageRef, Pager};
use crate::storage::sqlite3_ondisk::PageType;
use crate::sync::Arc;
use crate::types::{IOResult, SeekKey, SeekOp, SeekResult, Value};
use crate::util::IOExt;

fn storage(error: crate::LimboError) -> MorselError {
    MorselError::Storage(Box::new(error))
}

/// A table b-tree read in morsels of leaf pages, through the statement's pager.
pub struct TableSource {
    pager: Arc<Pager>,
    root: i64,
    columns: usize,
    rowid_alias: Option<usize>,
    /// Each leaf's greatest rowid in key order; `None` for the last leaf, which has no bound.
    upper: Vec<Option<i64>>,
}

/// One page entry of a level of the walk: its number, the greatest rowid under it, and whether it
/// is known to be a leaf.
type LevelPage = (i64, Option<i64>, bool);

impl TableSource {
    /// Finds the leaves of the table b-tree rooted at `root` (a table of `columns` columns, the
    /// rowid standing in for column `rowid_alias` when it is an INTEGER PRIMARY KEY).
    pub fn open(
        pager: Arc<Pager>,
        root: i64,
        columns: usize,
        rowid_alias: Option<usize>,
    ) -> Result<Self, MorselError> {
        // The bound is the format's: no b-tree has more pages than the database may hold.
        let most_pages =
            usize::try_from(pager.get_max_page_count()).map_err(|_| MorselError::Overflow {
                what: "the database's page count",
            })?;
        let mut level: Vec<LevelPage> = Vec::new();
        level.try_reserve_exact(1)?;
        level.push((root, None, false));
        let mut visited = 1usize;
        // Ends: each pass replaces every interior page with its children, one level deeper; a
        // tree has fewer levels than pages, and `visited` refuses a walk past the page count.
        loop {
            if level.iter().all(|(_, _, leaf)| *leaf) {
                break;
            }
            let mut next: Vec<LevelPage> = Vec::new();
            for (number, upper, leaf) in level {
                if leaf {
                    next.try_reserve(1)?;
                    next.push((number, upper, true));
                    continue;
                }
                let page = read_page(&pager, number)?;
                let contents = page.get_contents();
                match contents.page_type().map_err(storage)? {
                    PageType::TableLeaf => {
                        next.try_reserve(1)?;
                        next.push((number, upper, true));
                    }
                    PageType::TableInterior => {
                        let cells = contents.cell_count();
                        next.try_reserve(cells.saturating_add(1))?;
                        for cell in 0..cells {
                            let child = contents
                                .cell_interior_read_left_child_page(cell)
                                .map_err(storage)?;
                            let divider = contents
                                .cell_table_interior_read_rowid(cell)
                                .map_err(storage)?;
                            next.push((i64::from(child), Some(divider), false));
                        }
                        let right = contents
                            .rightmost_pointer()
                            .map_err(storage)?
                            .ok_or(MorselError::Corrupt("an interior page has no right child"))?;
                        next.push((i64::from(right), upper, false));
                        visited = visited.saturating_add(cells.saturating_add(1));
                        if visited > most_pages {
                            return Err(MorselError::Corrupt(
                                "the walk passed the database's page count",
                            ));
                        }
                    }
                    PageType::IndexInterior | PageType::IndexLeaf => {
                        return Err(MorselError::Corrupt("a table b-tree holds an index page"));
                    }
                }
            }
            level = next;
        }
        let mut upper = Vec::new();
        upper.try_reserve_exact(level.len())?;
        upper.extend(level.into_iter().map(|(_, bound, _)| bound));
        Ok(TableSource {
            pager,
            root,
            columns,
            rowid_alias,
            upper,
        })
    }

    /// How many leaf pages the table has: the units its morsels are cut in.
    pub fn leaves(&self) -> usize {
        self.upper.len()
    }

    /// Reads the rows of leaves `leaves` in rowid order with a cursor of its own, handing each
    /// row (its rowid, then its columns) to `each`.
    pub fn scan(
        &self,
        leaves: Range<usize>,
        mut each: impl FnMut(i64, &[Value]) -> Result<(), MorselError>,
    ) -> Result<(), MorselError> {
        if leaves.start >= leaves.end {
            return Ok(());
        }
        let lower = match leaves.start.checked_sub(1) {
            None => None,
            Some(below) => Some(self.upper.get(below).copied().flatten().ok_or(
                MorselError::Corrupt("a leaf before the last has no greatest rowid"),
            )?),
        };
        let last = leaves.end.checked_sub(1).ok_or(MorselError::Overflow {
            what: "a morsel's last leaf",
        })?;
        let upper = *self.upper.get(last).ok_or(MorselError::Overflow {
            what: "a morsel's leaves",
        })?;
        let io = &self.pager.io;
        let mut cursor = BTreeCursor::new_table(self.pager.clone(), self.root, self.columns);
        let positioned = match lower {
            None => {
                io.block(|| cursor.rewind()).map_err(storage)?;
                true
            }
            Some(lower) => {
                match io
                    .block(|| cursor.seek(SeekKey::TableRowId(lower), SeekOp::GT))
                    .map_err(storage)?
                {
                    SeekResult::Found => true,
                    SeekResult::NotFound => false,
                    SeekResult::TryAdvance => {
                        io.block(|| cursor.next()).map_err(storage)?;
                        true
                    }
                }
            }
        };
        if !positioned {
            return Ok(());
        }
        let mut row: Vec<Value> = Vec::new();
        row.try_reserve_exact(self.columns)?;
        // Ends: each pass moves the cursor to the next row; the loop stops when the rows run out
        // or a rowid passes the morsel's last leaf.
        while let Some(rowid) = io.block(|| cursor.rowid()).map_err(storage)? {
            if upper.is_some_and(|bound| rowid > bound) {
                break;
            }
            let values = io
                .block(|| {
                    Ok(match cursor.record()? {
                        IOResult::Done(Some(record)) => {
                            IOResult::Done(Some(record.get_values_owned()?))
                        }
                        IOResult::Done(None) => IOResult::Done(None),
                        IOResult::IO(wait) => IOResult::IO(wait),
                    })
                })
                .map_err(storage)?
                .ok_or(MorselError::Corrupt("a row has no record"))?;
            row.clear();
            row.extend(values.into_iter().take(self.columns));
            // A column added after the row was written reads as NULL.
            row.resize(self.columns, Value::Null);
            if let Some(at) = self.rowid_alias {
                let slot = row.get_mut(at).ok_or(MorselError::NoColumn {
                    column: at,
                    width: self.columns,
                })?;
                *slot = Value::from_i64(rowid);
            }
            each(rowid, &row)?;
            io.block(|| cursor.next()).map_err(storage)?;
        }
        Ok(())
    }

    /// Every row in rowid order, read on the caller's thread: a hash join's build side.
    pub fn rows(&self) -> Result<Vec<(i64, Vec<Value>)>, MorselError> {
        let mut out = Vec::new();
        self.scan(0..self.leaves(), |rowid, row| {
            out.try_reserve(1)?;
            let mut owned = Vec::new();
            owned.try_reserve_exact(row.len())?;
            owned.extend_from_slice(row);
            out.push((rowid, owned));
            Ok(())
        })?;
        Ok(out)
    }
}

fn read_page(pager: &Pager, number: i64) -> Result<PageRef, MorselError> {
    let (page, completion) = pager
        .io
        .block(|| pager.read_page(number))
        .map_err(storage)?;
    if let Some(completion) = completion {
        pager.io.wait_for_completion(completion).map_err(storage)?;
    }
    Ok(page)
}

/// A rowid's place in rowid order as a row position (the order MIN, MAX and a group's key values
/// break ties by): the sign bit flipped, so every rowid maps in order onto 0..2^64.
fn position(rowid: i64) -> Result<usize, MorselError> {
    let ordered = u64::from_ne_bytes(rowid.to_ne_bytes()) ^ (1u64 << 63);
    usize::try_from(ordered).map_err(|_| MorselError::Overflow {
        what: "a rowid's position",
    })
}

/// A scan with its filter and aggregates over a table's leaves: `shape` names the filter, the
/// grouping and the aggregates (its `rows` are not read).
pub struct TableAggregate<'a> {
    pub table: &'a TableSource,
    pub shape: &'a ScanAggregate<'a>,
}

impl Pipeline for TableAggregate<'_> {
    type Partial = super::morsel::AggregatePartial;

    fn rows(&self) -> usize {
        self.table.leaves()
    }

    fn empty(&self) -> Self::Partial {
        self.shape.empty()
    }

    fn run(
        &self,
        leaves: Range<usize>,
        partial: &mut Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        self.table.scan(leaves, |rowid, row| {
            self.shape.fold(row, position(rowid)?, partial, budget)
        })
    }

    fn merge(
        &self,
        into: &mut Self::Partial,
        from: Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        self.shape.merge(into, from, budget)
    }
}

impl TableAggregate<'_> {
    /// The answer as the switch and the measured costs pick the degree.
    pub fn run_mode(
        &self,
        mode: ParallelMode,
        costs: &mut MachineCosts,
        budget: &MemoryBudget,
    ) -> Result<(Vec<Vec<Value>>, RunStats), MorselError> {
        let (partial, stats) = execute(self, mode, costs, budget)?;
        Ok((self.shape.finish(partial)?, stats))
    }

    /// The answer at a stated degree, its morsels counted in leaf pages.
    pub fn run_at(
        &self,
        degree: Degree,
        budget: &MemoryBudget,
    ) -> Result<(Vec<Vec<Value>>, RunStats), MorselError> {
        let (partial, stats) = execute_at(self, 0, degree, None, budget)?;
        Ok((self.shape.finish(partial)?, stats))
    }
}

/// A hash join's probe side read from a table's leaves against a table built once.
pub struct TableProbe<'a> {
    pub table: &'a TableSource,
    pub probe: &'a HashProbe<'a>,
}

/// A worker's matches as (probe rowid, build row), each run tagged with its morsel's first leaf.
pub type TableProbePartial = Vec<(usize, Vec<(i64, usize)>)>;

impl Pipeline for TableProbe<'_> {
    type Partial = TableProbePartial;

    fn rows(&self) -> usize {
        self.table.leaves()
    }

    fn empty(&self) -> TableProbePartial {
        Vec::new()
    }

    fn run(
        &self,
        leaves: Range<usize>,
        partial: &mut TableProbePartial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        let first = leaves.start;
        let mut found = Vec::new();
        self.table.scan(leaves, |rowid, row| {
            let matches = self.probe.matches(row)?;
            if matches.is_empty() {
                return Ok(());
            }
            budget.charge(std::mem::size_of::<(i64, usize)>().saturating_mul(matches.len()))?;
            found.try_reserve(matches.len())?;
            found.extend(matches.iter().map(|build| (rowid, *build)));
            Ok(())
        })?;
        partial.try_reserve(1)?;
        partial.push((first, found));
        Ok(())
    }

    fn merge(
        &self,
        into: &mut TableProbePartial,
        from: TableProbePartial,
        _budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        into.try_reserve(from.len())?;
        into.extend(from);
        Ok(())
    }
}

impl TableProbe<'_> {
    /// The matches as (probe rowid, build row), in probe-rowid order and each probe row's in build
    /// order, as the serial probe gives them.
    pub fn finish(mut partial: TableProbePartial) -> Result<Vec<(i64, usize)>, MorselError> {
        partial.sort_by_key(|(first, _)| *first);
        let total = partial
            .iter()
            .map(|(_, run)| run.len())
            .fold(0usize, usize::saturating_add);
        let mut out = Vec::new();
        out.try_reserve_exact(total)?;
        for (_, run) in partial {
            out.extend(run);
        }
        Ok(out)
    }

    /// The matches as the switch and the measured costs pick the degree.
    pub fn run_mode(
        &self,
        mode: ParallelMode,
        costs: &mut MachineCosts,
        budget: &MemoryBudget,
    ) -> Result<(Vec<(i64, usize)>, RunStats), MorselError> {
        let (partial, stats) = execute(self, mode, costs, budget)?;
        Ok((Self::finish(partial)?, stats))
    }

    /// The matches at a stated degree, its morsels counted in leaf pages.
    pub fn run_at(
        &self,
        degree: Degree,
        budget: &MemoryBudget,
    ) -> Result<(Vec<(i64, usize)>, RunStats), MorselError> {
        let (partial, stats) = execute_at(self, 0, degree, None, budget)?;
        Ok((Self::finish(partial)?, stats))
    }
}

impl TableSource {
    /// The rows that pass `filter`, in rowid order, read on the caller's thread: a hash join's
    /// build side (QT.6). Each row kept is charged to the statement's budget, its values' own
    /// bytes and their text or blob bytes.
    pub fn filtered_rows(
        &self,
        filter: &[Predicate],
        budget: &MemoryBudget,
    ) -> Result<Vec<Vec<Value>>, MorselError> {
        let mut out = Vec::new();
        self.scan(0..self.leaves(), |_, row| {
            for term in filter {
                if !term.holds(row)? {
                    return Ok(());
                }
            }
            let bytes = row.iter().fold(
                std::mem::size_of::<Value>().saturating_mul(row.len()),
                |sum, value| {
                    sum.saturating_add(match value {
                        Value::Text(text) => text.as_str().len(),
                        Value::Blob(blob) => blob.len(),
                        _ => 0,
                    })
                },
            );
            budget.charge(bytes)?;
            out.try_reserve(1)?;
            let mut owned = Vec::new();
            owned.try_reserve_exact(row.len())?;
            owned.extend_from_slice(row);
            out.push(owned);
            Ok(())
        })?;
        Ok(out)
    }
}

/// A hash join's probe side read from a table's leaves, each joined row (the probe row's columns,
/// then the build row's) folded into the aggregates: the probe and the aggregates are one loop
/// body, run by every worker over its own leaves (QT.6).
pub struct TableJoinAggregate<'a> {
    pub table: &'a TableSource,
    /// Terms over the probe row alone, tested before it is probed.
    pub probe_filter: &'a [Predicate],
    pub probe: &'a HashProbe<'a>,
    /// The build side's rows, as `probe` numbers them.
    pub build: &'a [Vec<Value>],
    /// The grouping and the aggregates over a joined row (its filter is not read).
    pub shape: &'a ScanAggregate<'a>,
}

impl Pipeline for TableJoinAggregate<'_> {
    type Partial = super::morsel::AggregatePartial;

    fn rows(&self) -> usize {
        self.table.leaves()
    }

    fn empty(&self) -> Self::Partial {
        self.shape.empty()
    }

    fn run(
        &self,
        leaves: Range<usize>,
        partial: &mut Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        let mut joined: Vec<Value> = Vec::new();
        self.table.scan(leaves, |rowid, row| {
            for term in self.probe_filter {
                if !term.holds(row)? {
                    return Ok(());
                }
            }
            let matches = self.probe.matches(row)?;
            if matches.is_empty() {
                return Ok(());
            }
            // A probe row's matches share its place: the hook takes only columns whose equal
            // values are the same value (translate/parallel.rs), so a tie among them is no choice.
            let at = position(rowid)?;
            for build in matches {
                let build_row = self.build.get(*build).ok_or(MorselError::Overflow {
                    what: "a match's build row",
                })?;
                joined.clear();
                joined.try_reserve(row.len().saturating_add(build_row.len()))?;
                joined.extend_from_slice(row);
                joined.extend_from_slice(build_row);
                self.shape.fold(&joined, at, partial, budget)?;
            }
            Ok(())
        })
    }

    fn merge(
        &self,
        into: &mut Self::Partial,
        from: Self::Partial,
        budget: &MemoryBudget,
    ) -> Result<(), MorselError> {
        self.shape.merge(into, from, budget)
    }
}

impl TableJoinAggregate<'_> {
    /// The answer as the switch and the measured costs pick the degree.
    pub fn run_mode(
        &self,
        mode: ParallelMode,
        costs: &mut MachineCosts,
        budget: &MemoryBudget,
    ) -> Result<(Vec<Vec<Value>>, RunStats), MorselError> {
        let (partial, stats) = execute(self, mode, costs, budget)?;
        Ok((self.shape.finish(partial)?, stats))
    }
}

#[cfg(test)]
#[path = "morsel_scan_tests.rs"]
mod tests;
