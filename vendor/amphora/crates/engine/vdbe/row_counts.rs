//! Amphora: the rows each table access and join of an executed statement gave, read back beside
//! the planner's estimates of them (docs/design/planner.md, decision 12). A connection asks for
//! them before it prepares (`Connection::set_row_counters`); a statement prepared without them
//! has no counting instruction, and one prepared with them has one counter a plan node, kept in
//! a vector its first count sizes, never one a row.

use crate::translate::plan::TablePlanEstimate;

/// What one counter counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCountKind {
    /// The times a query's loops were entered: once a run of the query, once a call of a
    /// correlated subquery. A prefix's rows over these are its rows a call, as its estimate is.
    Loops,
    /// The rows of the join prefix through `table`: the rows that passed every term the plan
    /// evaluates there, which the estimate's `output_rows` estimates.
    Prefix,
    /// The rows a hash join's build table put in its hash table.
    HashBuild,
}

/// One counter of a statement: where it counts and what the planner estimated there.
#[derive(Debug, Clone, PartialEq)]
pub struct RowCountSite {
    /// The query the counter belongs to, numbered in the order the statement's queries open
    /// their loops: 0 the first opened, its subqueries and the queries of its FROM after it.
    pub plan: usize,
    pub kind: RowCountKind,
    /// The table's name in the query (its alias, if it has one); `None` for [RowCountKind::Loops].
    pub table: Option<String>,
    /// The planner's estimate for the table's access, its `position` the place in the join order.
    pub estimate: Option<TablePlanEstimate>,
}

/// A counter and the rows it counted.
#[derive(Debug, Clone, PartialEq)]
pub struct RowCount {
    pub site: RowCountSite,
    pub rows: u64,
}
