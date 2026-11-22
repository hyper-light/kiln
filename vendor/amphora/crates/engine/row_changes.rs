//! Amphora (docs/design/planner.md, decision (lane pl9); PL.9): each table's rows inserted less rows deleted
//! since ANALYZE last counted it, kept as writes finish, so the planner's row counts follow the table
//! between ANALYZE runs, and a prepared statement plans again once its tables have moved past what
//! its estimates could already tell apart.
//!
//! The write path pays one atomic add per INSERT or DELETE statement, whatever its rows: the
//! statement's own change count, folded in when it finishes. A table's counter is found once, when
//! the statement is planned, so neither path takes a lock or a lookup as it runs.

use crate::stats::TableStat;
use crate::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use crate::sync::{Arc, Mutex};
use crate::util::normalize_ident;
use std::collections::HashMap;

/// Every table's counter, one per table name a statement planned against or wrote to. Bound: one
/// entry for each table the database's schema has named while it is open (a dropped table's entry
/// stays, a few bytes, until the database closes).
#[derive(Debug, Default)]
pub struct RowChanges {
    tables: Mutex<HashMap<String, Arc<AtomicI64>>>,
    /// How many times a finished write folded its change count in (T-PL.9.2: the write path's
    /// overhead, counted rather than timed).
    folds: AtomicU64,
}

impl RowChanges {
    pub fn new() -> Self {
        Self::default()
    }

    /// The counter of `table`, made at zero the first time. Keyed by the normalized name, as
    /// ANALYZE's statistics are.
    pub(crate) fn counter(&self, table: &str) -> Arc<AtomicI64> {
        let table = normalize_ident(table);
        let mut tables = self.tables.lock();
        if let Some(counter) = tables.get(&table) {
            return counter.clone();
        }
        let counter = Arc::new(AtomicI64::new(0));
        tables.insert(table, counter.clone());
        counter
    }

    /// Rows inserted less rows deleted in `table` since ANALYZE last counted it.
    pub fn changed_since_analyze(&self, table: &str) -> i64 {
        self.tables
            .lock()
            .get(&normalize_ident(table))
            .map_or(0, |counter| counter.load(Ordering::Acquire))
    }

    /// ANALYZE counted these tables again (their normalized names, as `AnalyzeStats` keys them):
    /// their changes start from zero.
    pub(crate) fn analyzed<'a>(&self, tables: impl Iterator<Item = &'a String>) {
        let known = self.tables.lock();
        for table in tables {
            if let Some(counter) = known.get(table) {
                counter.store(0, Ordering::Release);
            }
        }
    }

    /// How many finished writes have folded their change count in since the database opened.
    pub fn folds(&self) -> u64 {
        self.folds.load(Ordering::Relaxed)
    }
}

/// What a finished INSERT or DELETE adds to its table's counter: its change count, with this sign.
#[derive(Debug, Clone)]
pub struct WriteTarget {
    pub(crate) counter: Arc<AtomicI64>,
    pub(crate) changes: Arc<RowChanges>,
    /// 1 for an INSERT, -1 for a DELETE.
    pub(crate) sign: i64,
}

impl WriteTarget {
    /// The statement finished having changed `rows` rows of its table.
    pub(crate) fn fold(&self, rows: i64) {
        if rows == 0 {
            return;
        }
        self.counter
            .fetch_add(self.sign.saturating_mul(rows), Ordering::AcqRel);
        self.changes.folds.fetch_add(1, Ordering::Relaxed);
    }
}

/// One table a statement's plan counted: the rows ANALYZE gave it, the changes since that the plan
/// added, and how far the count may move before the plan's estimates could tell it apart.
#[derive(Debug, Clone)]
pub struct PlannedTable {
    pub(crate) table: String,
    pub(crate) counter: Arc<AtomicI64>,
    pub(crate) analyzed_rows: f64,
    pub(crate) planned_changes: i64,
    pub(crate) error: f64,
}

impl PlannedTable {
    /// The rows the plan was made for.
    pub(crate) fn planned_rows(&self) -> f64 {
        live_rows(self.analyzed_rows, self.planned_changes)
    }

    /// Whether the table has moved past the plan's own error since it was planned.
    pub(crate) fn moved(&self) -> bool {
        let now = live_rows(self.analyzed_rows, self.counter.load(Ordering::Acquire));
        moved_past_error(self.planned_rows(), now, self.error)
    }
}

/// A table's rows now: ANALYZE's count with the changes since, at least one row (the planner's
/// floor for a table it has statistics for).
pub(crate) fn live_rows(analyzed_rows: f64, changes: i64) -> f64 {
    (analyzed_rows + changes as f64).max(1.0)
}

/// The relative standard error of the estimates a table's statistics give, which is how far its row
/// count can move before a plan made on them could tell the move from their own noise:
/// - with a sample of `n` rows (decision 20), a share `p` of the table estimated from it has the
///   standard error `sqrt(p (1 - p) / n)` (the binomial's), which relative to the half the sample
///   resolves best is `1 / sqrt(n)`; a filter on the sample (decision 21) is estimated this way;
/// - with no sample but each column's HyperLogLog sketch, a distinct count's relative standard
///   error is `1.04 / sqrt(m)` for the sketch's `m` registers (Flajolet et al. 2007);
/// - with only `sqlite_stat1`'s counts, every row was counted: `n` is the table's rows.
///
/// Nothing in it is picked: `n` and `m` are the statistics' own sizes, read where they are kept.
pub(crate) fn statistics_error(stat: &TableStat, analyzed_rows: f64) -> f64 {
    if let Some(sample) = stat.sample.as_ref().filter(|s| !s.rows.is_empty()) {
        return (sample.rows.len() as f64).sqrt().recip();
    }
    if !stat.columns.is_empty() {
        return crate::column_stats::relative_standard_error();
    }
    analyzed_rows.max(1.0).sqrt().recip()
}

/// Whether a count gone from `planned` to `now` has moved by more than the relative `error`, in
/// either direction (a table halved moves as far as one doubled).
pub(crate) fn moved_past_error(planned: f64, now: f64, error: f64) -> bool {
    let bound = 1.0 + error;
    now > planned * bound || now * bound < planned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-PL.9.1: a count within its estimate's error has not moved; one past it has, either way.
    #[test]
    fn t_pl_9_1_a_count_moves_only_past_its_error() {
        let error = 1.0 / 1024f64.sqrt();
        assert!(!moved_past_error(10_000.0, 10_000.0, error));
        assert!(!moved_past_error(10_000.0, 10_300.0, error));
        assert!(moved_past_error(10_000.0, 10_400.0, error));
        assert!(moved_past_error(10_000.0, 100_000.0, error));
        assert!(!moved_past_error(10_000.0, 9_700.0, error));
        assert!(moved_past_error(10_000.0, 9_600.0, error));
    }

    /// T-PL.9.2: a write folds its rows in with one add, and only a write that changed rows does.
    #[test]
    fn t_pl_9_2_a_write_folds_its_rows_in_once() {
        let changes = Arc::new(RowChanges::new());
        let insert = WriteTarget {
            counter: changes.counter("t"),
            changes: changes.clone(),
            sign: 1,
        };
        let delete = WriteTarget {
            sign: -1,
            ..insert.clone()
        };
        insert.fold(1_000);
        delete.fold(250);
        delete.fold(0);
        assert_eq!(changes.changed_since_analyze("t"), 750);
        assert_eq!(changes.folds(), 2);
        changes.analyzed([String::from("t")].iter());
        assert_eq!(changes.changed_since_analyze("t"), 0);
        assert_eq!(changes.changed_since_analyze("never"), 0);
    }
}
