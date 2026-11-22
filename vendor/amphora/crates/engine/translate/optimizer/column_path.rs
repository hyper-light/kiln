//! Amphora: the planner's access path "scan the column copy" against the row scan, chosen by cost
//! (G11 step 5, CS.5; `docs/design/columnar.md`, "The steps after": whole-table and wide-range
//! statements to the copy, point reads to the rows, by measured cost [c95]).
//!
//! Every cost here is nanoseconds, measured where the engine runs, never a picked weight:
//! - the row path's: a row's visit and a seek, the database's learned rates
//!   (`learned_costs::LearnedCosts::nanos_per`, calibrated at start and moved by its operators);
//! - the copy's: its own measured scan time per row at the block size it is cut at
//!   (`storage::versioned::columns::Columns::plan_view`), which already carries what its deltas
//!   cost, and the keys its queue holds at the statement's snapshot, each read from the rows (a
//!   seek and a row's visit) and merged into the copy's answer.
//!
//! A copy that can't answer the snapshot exactly (behind the latest commit by records its full queue
//! dropped, or made after the snapshot with no older generation kept) is no path at all: the
//! statement reads rows. A copy never yet measured at its current block size is tried only for a
//! whole-table statement, where it reads the same rows the row scan would, so its first scan is
//! what measures it; it is never guessed cheaper than a seek.
//!
//! The copy's reads are the rows its scan visits: today every row of the generation
//! (`Columns::scan`); once G11 step 4's vectorized scan lands, the rows of the granules its
//! summaries keep, which the caller passes as [`CopyView::reads`] with nothing else here changing.
//!
//! Behind a switch until step 4's scan is on main: [`asked`], `AMPHORA_COLUMN_PATH=on`. Nothing
//! outside the tests calls this yet: the copy is not yet kept for SQL tables, and the storage's
//! reader (`storage::versioned::columns_path`) is where it plugs in.
#![cfg_attr(not(test), allow(dead_code))]

/// The environment variable that turns the copy's access path on: `on`.
pub const ENV: &str = "AMPHORA_COLUMN_PATH";

/// Whether the environment turns the copy's access path on.
pub fn asked() -> bool {
    std::env::var(ENV).is_ok_and(|v| v == "on")
}

/// What the statement asks of one table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// One row by its key: the row path seeks it.
    Point,
    /// A range of the key estimated at `rows` rows: the row path seeks its start and visits them.
    Range { rows: f64 },
    /// Every row (a whole-table aggregate): the row path visits them all.
    Whole,
}

/// The row path's learned rates, nanoseconds each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowRates {
    /// A row visited: read and tested once.
    pub row: f64,
    /// One search of the tree for a key.
    pub seek: f64,
}

impl RowRates {
    /// The database's learned rates for a row's visit and a seek; None before either was timed.
    pub fn learned(costs: &super::learned_costs::LearnedCosts) -> Option<Self> {
        use super::learned_costs::Work;
        Some(Self {
            row: costs.nanos_per(Work::Row)?,
            seek: costs.nanos_per(Work::Seek)?,
        })
    }
}

/// The column copy as the planner sees it at the statement's snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CopyView {
    /// A generation answers the snapshot exactly (with the queue's keys read from the rows).
    pub exact: bool,
    /// Rows the copy's scan visits for the statement.
    pub reads: f64,
    /// Distinct keys the queue holds at or before the snapshot: each read from the rows and merged.
    pub queued: f64,
    /// The copy's measured scan time per row at its current block size; None before its first
    /// scan at that size.
    pub nanos_per_row: Option<f64>,
}

/// The path a table's statement takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    Rows,
    Copy,
}

/// The choice and the costs it was made by, in nanoseconds (shown, so the choice says why).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Choice {
    pub path: Path,
    pub rows_cost: f64,
    /// None: the copy was no path (switched off, not exact, or unmeasured for this shape).
    pub copy_cost: Option<f64>,
}

/// The row path's cost for `shape` over a table of `table_rows` rows.
pub fn rows_cost(shape: Shape, table_rows: f64, r: RowRates) -> f64 {
    match shape {
        Shape::Point => r.seek + r.row,
        Shape::Range { rows } => r.seek + rows.max(0.0) * r.row,
        Shape::Whole => table_rows.max(0.0) * r.row,
    }
}

/// The copy's cost for `shape`, None when it is no path.
pub fn copy_cost(shape: Shape, table_rows: f64, r: RowRates, c: CopyView) -> Option<f64> {
    if !c.exact {
        return None;
    }
    let merge = c.queued.max(0.0) * (r.seek + r.row);
    match c.nanos_per_row {
        Some(per_row) => Some(c.reads.max(0.0) * per_row + merge),
        // Unmeasured: tried where it visits what the row scan visits, priced as that scan (its
        // queued keys are part of the scan that measures it).
        None if shape == Shape::Whole => Some(rows_cost(shape, table_rows, r)),
        None => None,
    }
}

/// The path for `shape`: the copy when it is on, a path, and cheaper; else the rows. A tie goes
/// to the rows, the record.
pub fn choose(on: bool, shape: Shape, table_rows: f64, r: RowRates, c: CopyView) -> Choice {
    let rows = rows_cost(shape, table_rows, r);
    let copy = if on {
        copy_cost(shape, table_rows, r, c).filter(|v| v.is_finite())
    } else {
        None
    };
    let unmeasured_whole = c.nanos_per_row.is_none() && shape == Shape::Whole;
    let path = match copy {
        Some(cost) if cost < rows || (unmeasured_whole && cost <= rows) => Path::Copy,
        _ => Path::Rows,
    };
    Choice {
        path,
        rows_cost: rows,
        copy_cost: copy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Rates as a database learns them: a seek costs many rows' visits, the copy's row fewer.
    const R: RowRates = RowRates {
        row: 40.0,
        seek: 900.0,
    };

    fn fresh(rows: f64, per_row: f64) -> CopyView {
        CopyView {
            exact: true,
            reads: rows,
            queued: 0.0,
            nanos_per_row: Some(per_row),
        }
    }

    #[test]
    fn cs5_wide_aggregate_takes_the_copy_point_read_the_rows() {
        let c = fresh(100_000.0, 4.0);
        assert_eq!(choose(true, Shape::Whole, 100_000.0, R, c).path, Path::Copy);
        assert_eq!(
            choose(true, Shape::Range { rows: 50_000.0 }, 100_000.0, R, c).path,
            Path::Copy
        );
        assert_eq!(choose(true, Shape::Point, 100_000.0, R, c).path, Path::Rows);
        assert_eq!(
            choose(true, Shape::Range { rows: 3.0 }, 100_000.0, R, c).path,
            Path::Rows
        );
    }

    #[test]
    fn cs5_switched_off_or_stale_copy_is_no_path() {
        let c = fresh(100_000.0, 4.0);
        let off = choose(false, Shape::Whole, 100_000.0, R, c);
        assert_eq!((off.path, off.copy_cost), (Path::Rows, None));
        let stale = CopyView { exact: false, ..c };
        let s = choose(true, Shape::Whole, 100_000.0, R, stale);
        assert_eq!((s.path, s.copy_cost), (Path::Rows, None));
    }

    #[test]
    fn cs5_queued_keys_are_merged_at_their_cost() {
        // As many queued keys as make the merge dearer than the rows: the rows win.
        let mut c = fresh(1_000.0, 4.0);
        assert_eq!(choose(true, Shape::Whole, 1_000.0, R, c).path, Path::Copy);
        c.queued = 1_000.0;
        let ch = choose(true, Shape::Whole, 1_000.0, R, c);
        assert_eq!(ch.path, Path::Rows);
        assert_eq!(ch.copy_cost, Some(1_000.0 * 4.0 + 1_000.0 * (900.0 + 40.0)));
    }

    #[test]
    fn cs5_unmeasured_copy_tried_only_for_whole_tables() {
        let c = CopyView {
            nanos_per_row: None,
            ..fresh(10_000.0, 0.0)
        };
        assert_eq!(choose(true, Shape::Whole, 10_000.0, R, c).path, Path::Copy);
        let queued = CopyView { queued: 50.0, ..c };
        assert_eq!(
            choose(true, Shape::Whole, 10_000.0, R, queued).path,
            Path::Copy
        );
        assert_eq!(choose(true, Shape::Point, 10_000.0, R, c).path, Path::Rows);
        assert_eq!(
            choose(true, Shape::Range { rows: 9_000.0 }, 10_000.0, R, c).path,
            Path::Rows
        );
        // A copy measured dearer than the rows loses the whole table too.
        let slow = fresh(10_000.0, 80.0);
        assert_eq!(
            choose(true, Shape::Whole, 10_000.0, R, slow).path,
            Path::Rows
        );
    }
}
