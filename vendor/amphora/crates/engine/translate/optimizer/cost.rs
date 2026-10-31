use crate::schema::Index;
use crate::stats::AnalyzeStats;
use crate::sync::Arc;
use crate::translate::expr::{walk_expr, WalkControl};
use crate::translate::optimizer::constraints::RangeConstraintRef;
use crate::translate::plan::JoinedTable;
use amphora_engine_parser::ast;
use smallvec::SmallVec;

use super::constraints::Constraint;
use super::cost_params::CostModelParams;

/// A simple newtype wrapper over a f64 that represents the cost of an operation.
///
/// This is used to estimate the cost of scans, seeks, and joins.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost(pub f64);

/// Count the operations needed to check one `WHERE` expression.
pub fn where_expr_steps(expr: &ast::Expr) -> usize {
    let mut steps = 0;
    walk_expr(expr, &mut |expr| {
        steps += where_node_steps(expr);
        Ok(WalkControl::Continue)
    })
    .expect("counting WHERE operations cannot fail");
    steps.max(1)
}

pub fn where_node_steps(expr: &ast::Expr) -> usize {
    match expr {
        ast::Expr::Between { .. } => 2,
        ast::Expr::InList { rhs, .. } => rhs.len().max(1),
        ast::Expr::Case {
            when_then_pairs, ..
        } => when_then_pairs.len().max(1),
        ast::Expr::Register(_)
        | ast::Expr::Collate(..)
        | ast::Expr::DoublyQualified(..)
        | ast::Expr::Id(_)
        | ast::Expr::Column { .. }
        | ast::Expr::RowId { .. }
        | ast::Expr::Literal(_)
        | ast::Expr::Name(_)
        | ast::Expr::Parenthesized(_)
        | ast::Expr::Qualified(..)
        | ast::Expr::Variable(_)
        | ast::Expr::Default => 0,
        _ => 1,
    }
}

impl std::ops::Add for Cost {
    type Output = Cost;

    fn add(self, other: Cost) -> Cost {
        Cost(self.0 + other.0)
    }
}

impl std::ops::Deref for Cost {
    type Target = f64;

    fn deref(&self) -> &f64 {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexInfo {
    pub unique: bool,
    pub column_count: usize,
    /// Whether the index satisfies the query without table lookups.
    /// True for genuinely covering indexes and for multi-index branches
    /// that only harvest rowids into a RowSet.
    pub covering: bool,
    /// Estimated rows per index leaf page, derived from the column count
    /// ratio between the index and its parent table.
    pub rows_per_leaf_page: f64,
}

/// Estimate rows per index leaf page based on the index/table width ratio.
/// Narrower indexes have smaller entries, fitting more rows per page.
pub fn index_leaf_rows_per_page(
    index_column_count: usize,
    table_column_count: usize,
    has_rowid_alias: bool,
    rows_per_table_page: f64,
) -> f64 {
    // Table width: all columns + implicit rowid (unless one column IS the rowid alias).
    let table_width = table_column_count as f64 + if has_rowid_alias { 0.0 } else { 1.0 };
    // Index width: indexed columns + rowid suffix (always present).
    let index_width = index_column_count as f64 + 1.0;
    (rows_per_table_page * table_width / index_width).max(1.0)
}

/// Compute `rows_per_leaf_page` for an index on the given table.
pub fn rows_per_leaf_page_for_index(
    index_column_count: usize,
    rhs_table: &JoinedTable,
    rows_per_table_page: f64,
) -> f64 {
    let table_column_count = rhs_table.columns().len();
    let has_rowid_alias = rhs_table
        .btree()
        .is_some_and(|bt| bt.get_rowid_alias_column().is_some());
    index_leaf_rows_per_page(
        index_column_count,
        table_column_count,
        has_rowid_alias,
        rows_per_table_page,
    )
}

/// Estimate IO and CPU cost for a full table scan.
///
/// # Arguments
/// * `base_row_count` - Total rows in the table
/// * `num_scans` - Number of times we scan the table (e.g., from outer loop in nested loop join)
/// * `params` - Cost model parameters
pub(super) fn estimate_scan_cost(
    base_row_count: f64,
    num_scans: f64,
    params: &CostModelParams,
) -> Cost {
    let table_pages = (base_row_count / params.rows_per_table_page).max(1.0);

    // First scan reads all pages; subsequent scans benefit from caching
    let io_cost = if num_scans <= 1.0 {
        table_pages
    } else {
        // First scan + discounted cost for subsequent scans
        table_pages + (num_scans - 1.0) * table_pages * params.cache_reuse_factor
    };

    // CPU cost for processing all rows on each scan
    let cpu_cost = num_scans * base_row_count * params.cpu_cost_per_row;

    Cost(io_cost + cpu_cost)
}

/// Estimate the work to add every row to a new in-memory index.
///
/// Each insert searches the part of the index that was already built. A
/// balanced index needs about log2(rows) comparisons per insert.
pub(super) fn estimate_ephemeral_index_build_cost(
    row_count: f64,
    params: &CostModelParams,
) -> Cost {
    let comparisons_per_row = row_count.max(2.0).log2();
    Cost(row_count * comparisons_per_row * params.cpu_cost_per_seek)
}

pub(super) fn estimate_sort_cpu_cost(row_count: f64, params: &CostModelParams) -> Cost {
    Cost(row_count * row_count.max(1.0).log2() * params.sort_cpu_per_row)
}

/// Amphora: what reading `rows` out of `target`'s order costs a plan: a sort of them, or, for a
/// GROUP BY that groups in a hash table, a probe a row and a sort of the groups as the table
/// drains (`docs/design/planner.md` decision 7). Its plans' other costs are the same either way.
pub(super) fn order_cost(
    target: &super::order::OrderTarget,
    rows: f64,
    params: &CostModelParams,
) -> Cost {
    match target.hashed_groups {
        Some(groups) => Cost(
            rows * params.cpu_cost_per_row + estimate_sort_cpu_cost(groups.min(rows), params).0,
        ),
        None => estimate_sort_cpu_cost(rows, params),
    }
}

/// Estimate how many B-tree levels an index search reads.
pub(super) fn estimate_btree_depth(row_count: f64, rows_per_page: f64) -> f64 {
    if row_count <= 1.0 {
        1.0
    } else {
        (row_count.ln() / rows_per_page.ln()).ceil().max(1.0)
    }
}

/// Amphora: the table pages that `rows` looked up at random among `table_pages` read from the file,
/// `cache_pages` kept between reads: Mackert and Lohman's approximation for a finite LRU buffer
/// ("Index Scans Using a Finite LRU Buffer: A Validated I/O Model", ACM TODS 14(3), 1989), as
/// PostgreSQL's `index_pages_fetched` computes it (`docs/design/planner.md`, decision 5).
pub(super) fn table_pages_fetched(rows: f64, table_pages: f64, cache_pages: f64) -> f64 {
    let t = table_pages.max(1.0);
    let b = cache_pages.max(1.0);
    let n = rows.max(0.0);
    if t <= b {
        (2.0 * t * n / (2.0 * t + n)).min(t)
    } else {
        let fills = 2.0 * t * b / (2.0 * t - b);
        if n <= fills {
            2.0 * t * n / (2.0 * t + n)
        } else {
            b + (n - fills) * (t - b) / t
        }
    }
}

/// Estimate IO and CPU cost for index-based access.
///
/// This properly separates the number of B-tree seeks from the number of rows
/// returned per seek. A range scan does ONE seek followed by sequential leaf
/// page reads, not one seek per row.
///
/// # Arguments
/// * `base_row_count` - Total rows in the table (for estimating tree depth and page counts)
/// * `tree_depth` - Number of B-tree pages read by the first search
/// * `index_info` - Index properties (covering, unique, etc.)
/// * `input_cardinality` - Number of searches, usually one per outer row
/// * `rows_per_seek` - Expected rows returned per seek (1 for point lookup, more for range)
/// * `params` - Cost model parameters
pub fn estimate_index_cost(
    base_row_count: f64,
    tree_depth: f64,
    index_info: IndexInfo,
    input_cardinality: f64,
    rows_per_seek: f64,
    params: &CostModelParams,
) -> Cost {
    // Detect full index scan: when rows_per_seek equals base_row_count, we're scanning
    // the entire index, not seeking to specific positions.
    let is_full_scan = (rows_per_seek - base_row_count).abs() < 1.0;

    let seek_cost = if is_full_scan {
        // Full scan: one seek to start, then sequential reads.
        // When re-scanned (nested loop inner), first scan is cold, rest are cached.
        if input_cardinality <= 1.0 {
            tree_depth
        } else {
            tree_depth + (input_cardinality - 1.0) * tree_depth * params.cache_reuse_factor
        }
    } else if input_cardinality <= 1.0 {
        input_cardinality * tree_depth
    } else {
        let cached_root_page_cost = params.cache_reuse_factor;
        let leaf_page_costed_below = if rows_per_seek > 1.0 { 1.0 } else { 0.0 };
        let repeated_tree_search_cost = (tree_depth - 1.0 + cached_root_page_cost
            - leaf_page_costed_below)
            .max(cached_root_page_cost);
        tree_depth + (input_cardinality - 1.0) * repeated_tree_search_cost
    };

    let index_leaf_pages_count = (rows_per_seek / index_info.rows_per_leaf_page).max(1.0);
    let leaf_scan_cost = if is_full_scan {
        // Full scan of all leaf pages. Repeated scans benefit from caching.
        if input_cardinality <= 1.0 {
            index_leaf_pages_count
        } else {
            index_leaf_pages_count
                + (input_cardinality - 1.0) * index_leaf_pages_count * params.cache_reuse_factor
        }
    } else if rows_per_seek <= 1.0 {
        // Point lookup: the leaf page is the last page of the B-tree traversal,
        // already counted in seek_cost.
        0.0
    } else if input_cardinality <= 1.0 {
        index_leaf_pages_count
    } else {
        // Range scan in a nested-loop join: after the first iteration the inner
        // table's pages are largely in the buffer pool.  Apply the same caching
        // discount as full scans.
        index_leaf_pages_count
            + (input_cardinality - 1.0) * index_leaf_pages_count * params.cache_reuse_factor
    };

    // For non-covering indexes, we need to fetch from the table for each row. Amphora: each row
    // is a seek of the table of its own, its page read unless the cache holds it (decision 5).
    let table_lookup_cost = if index_info.covering {
        0.0
    } else {
        let table_pages_count = (base_row_count / params.rows_per_table_page).max(1.0);
        let looked_up = input_cardinality * rows_per_seek;
        table_pages_fetched(looked_up, table_pages_count, params.cache_pages)
            + looked_up * params.cpu_cost_per_seek
    };

    // Amphora: many searches of an index the cache holds whole read each of its pages once at
    // most (decision 6): its leaves as Mackert and Lohman count them, its inner pages once. One
    // pass is read in order, and an index bigger than the cache keeps its searches' pages, as
    // estimated above.
    let leaf_pages = (base_row_count / index_info.rows_per_leaf_page).max(1.0);
    let index_pages = leaf_pages + leaf_pages / index_info.rows_per_leaf_page.max(2.0) + tree_depth;
    let index_io = if input_cardinality > 1.0 && index_pages <= params.cache_pages {
        let reads = table_pages_fetched(
            input_cardinality * index_leaf_pages_count,
            leaf_pages,
            params.cache_pages,
        ) + (tree_depth - 1.0).max(0.0);
        (seek_cost + leaf_scan_cost).min(reads)
    } else {
        seek_cost + leaf_scan_cost
    };

    let io_cost = index_io + table_lookup_cost;

    // CPU cost: key comparisons during seeks + row processing
    let total_rows = input_cardinality * rows_per_seek;
    let cpu_cost =
        input_cardinality * params.cpu_cost_per_seek + total_rows * params.cpu_cost_per_row;

    Cost((io_cost + cpu_cost - params.index_bonus).max(0.001))
}

pub(crate) fn is_unique_point_lookup(
    index_info: IndexInfo,
    usable_constraint_refs: &[RangeConstraintRef],
) -> bool {
    // Only plain `=` equalities count. An `IS` equality matches NULL keys, and
    // a UNIQUE index can store many NULL keys, so a key with an `IS` component
    // can return many rows.
    let eq_count = usable_constraint_refs
        .iter()
        .take_while(|cref| cref.eq.as_ref().is_some_and(|eq| !eq.null_matching))
        .count();
    index_info.unique && eq_count >= index_info.column_count
}

/// Returns true when one index search can return at most one row.
///
/// A rowid search is unique when `index` is `None` and a plain `=` constrains the rowid.
/// A secondary-index search is unique only when `=` constrains every column of a unique index.
/// `IS` does not qualify because a unique index can contain many `NULL` values.
pub(crate) fn index_access_is_unique_point_lookup(
    index: Option<&Index>,
    usable_constraint_refs: &[RangeConstraintRef],
) -> bool {
    let index_info = match index {
        Some(index) => IndexInfo {
            unique: index.unique,
            column_count: index.columns.len(),
            covering: false,
            rows_per_leaf_page: 0.0,
        },
        None => IndexInfo {
            unique: true,
            column_count: 1,
            covering: false,
            rows_per_leaf_page: 0.0,
        },
    };
    is_unique_point_lookup(index_info, usable_constraint_refs)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowCountEstimate {
    HardcodedFallback(f64),
    AnalyzeStats(f64),
}

impl RowCountEstimate {
    /// Create a hardcoded fallback using the given params.
    pub fn hardcoded_fallback(params: &CostModelParams) -> Self {
        RowCountEstimate::HardcodedFallback(params.rows_per_table_fallback)
    }
}

impl std::ops::Deref for RowCountEstimate {
    type Target = f64;

    fn deref(&self) -> &f64 {
        match self {
            RowCountEstimate::HardcodedFallback(val) => val,
            RowCountEstimate::AnalyzeStats(val) => val,
        }
    }
}

/// ANALYZE-based context for cost estimation.
/// Uses sqlite_stat1 histogram data for row estimates when available,
/// otherwise falls through to heuristic selectivity multipliers.
pub struct AnalyzeCtx<'a> {
    pub rhs_table: &'a JoinedTable,
    pub index: Option<&'a Arc<Index>>,
    pub stats: &'a AnalyzeStats,
}

/// The fraction of rows one column's range keeps. A closed range keeps `sel_closed_range`, not
/// the product of its bounds' own estimates, the same as [constraints_selectivity] gives it as a
/// filter: a range must not cost five times more driving a seek than filtering one, or the
/// planner probes a table once per row of another to avoid it (docs/design/planner.md, decision 1).
fn range_selectivity(
    cref: &RangeConstraintRef,
    constraints: &[Constraint],
    params: &CostModelParams,
) -> f64 {
    let bound = |pos: usize| constraints.get(pos).map_or(1.0, |c| c.selectivity);
    match (cref.lower_bound, cref.upper_bound) {
        (Some(_), Some(_)) => params.sel_closed_range,
        (Some(pos), None) | (None, Some(pos)) => bound(pos),
        (None, None) => 1.0,
    }
}

/// A column's range bounds, as [constraints_selectivity] gathers them.
struct ColumnBounds {
    /// The column's position in its table, and whether it is the rowid.
    key: (Option<usize>, bool),
    lower: bool,
    upper: bool,
    /// The product of its bounds' selectivities.
    product: f64,
}

/// The fraction of rows `constraints` keep together: the product of their selectivities, except
/// that a column bounded from both sides keeps `sel_closed_range` in place of its bounds' product,
/// as a seek's range does ([range_selectivity]).
pub(crate) fn constraints_selectivity<'a>(
    constraints: impl Iterator<Item = &'a Constraint>,
    params: &CostModelParams,
) -> f64 {
    let mut fraction = 1.0;
    let mut columns: SmallVec<[ColumnBounds; 4]> = SmallVec::new();
    for constraint in constraints {
        // Amphora: an implied bound filters nothing its term doesn't (decision 3).
        if constraint.implied {
            continue;
        }
        let (lower, upper) = match constraint.operator.as_ast_operator() {
            Some(ast::Operator::Greater | ast::Operator::GreaterEquals) => (true, false),
            Some(ast::Operator::Less | ast::Operator::LessEquals) => (false, true),
            _ => (false, false),
        };
        // A bound on an expression stands alone: two need not bound the same expression.
        if !(lower || upper) || (constraint.table_col_pos.is_none() && !constraint.is_rowid) {
            fraction *= constraint.selectivity;
            continue;
        }
        let key = (constraint.table_col_pos, constraint.is_rowid);
        match columns.iter_mut().find(|column| column.key == key) {
            Some(column) => {
                column.lower |= lower;
                column.upper |= upper;
                column.product *= constraint.selectivity;
            }
            None => columns.push(ColumnBounds {
                key,
                lower,
                upper,
                product: constraint.selectivity,
            }),
        }
    }
    for column in columns {
        fraction *= if column.lower && column.upper {
            params.sel_closed_range
        } else {
            column.product
        };
    }
    fraction
}

pub(crate) fn estimate_rows_per_seek(
    index_info: IndexInfo,
    constraints: &[Constraint],
    usable_constraint_refs: &[RangeConstraintRef],
    base_row_count: RowCountEstimate,
    analyze_ctx: Option<&AnalyzeCtx>,
    params: &CostModelParams,
) -> f64 {
    let join_probe_constant_selectivity =
        join_probe_constant_selectivity(constraints, usable_constraint_refs);
    if is_unique_point_lookup(index_info, usable_constraint_refs) {
        return join_probe_constant_selectivity;
    }

    if let Some(ctx) = analyze_ctx {
        if !usable_constraint_refs.is_empty() {
            if let Some(eq_prefix_rows) =
                estimate_rows_from_analyze_stats(ctx, usable_constraint_refs)
            {
                // Apply range selectivity for any trailing non-equality constraints
                // beyond the equality prefix. SQLite does this via whereRangeAdjust
                // which divides by ~4 per range bound.
                let eq_prefix_len = usable_constraint_refs
                    .iter()
                    .take_while(|cref| cref.eq.is_some())
                    .count();
                let range_selectivity: f64 = usable_constraint_refs[eq_prefix_len..]
                    .iter()
                    .map(|cref| range_selectivity(cref, constraints, params))
                    .product();
                return (eq_prefix_rows * range_selectivity).max(1.0)
                    * join_probe_constant_selectivity;
            }
        }
    }

    let selectivity_multiplier: f64 = usable_constraint_refs
        .iter()
        .map(|cref| {
            if let Some(ref eq) = cref.eq {
                return constraints[eq.constraint_pos].selectivity;
            }
            range_selectivity(cref, constraints, params)
        })
        .product();

    (selectivity_multiplier * *base_row_count).max(1.0) * join_probe_constant_selectivity
}

fn join_probe_constant_selectivity(
    constraints: &[Constraint],
    usable_constraint_refs: &[RangeConstraintRef],
) -> f64 {
    let equality_constraints = usable_constraint_refs
        .iter()
        .take_while(|constraint| constraint.eq.is_some())
        .map(|constraint| &constraints[constraint.eq.as_ref().unwrap().constraint_pos]);
    if !equality_constraints
        .clone()
        .any(|constraint| !constraint.lhs_mask.is_empty())
    {
        return 1.0;
    }
    equality_constraints
        .filter(|constraint| constraint.lhs_mask.is_empty())
        .map(|constraint| constraint.selectivity)
        .product()
}

/// Estimate rows per seek using ANALYZE stats (sqlite_stat1 histogram data).
/// Returns `None` when no actual stats exist for this index, signaling the
/// caller to fall back to selectivity-based estimation.
fn estimate_rows_from_analyze_stats(
    ctx: &AnalyzeCtx,
    constraint_refs: &[RangeConstraintRef],
) -> Option<f64> {
    let index = ctx.index?;

    // Amphora: a skip-scan's refs start past the index's first column, where the per-prefix
    // averages don't line up with them; its rows are the selectivity estimate's (decision 4).
    if constraint_refs
        .first()
        .is_some_and(|cref| cref.index_col_pos > 0)
    {
        return None;
    }

    // Only count leading equality-constrained columns. ANALYZE stats give
    // avg rows per distinct prefix value, which is only meaningful for
    // equality lookups. A range constraint (>, <, >=, <=) scans a portion
    // of the index rather than fixing a prefix value, so it should not
    // consume a prefix position in the stats lookup.
    let eq_prefix_len = constraint_refs
        .iter()
        .take_while(|cref| cref.eq.is_some())
        .count();

    // The per-key average must not be applied to a key that can be NULL:
    // NULL keys pile up in one bucket (think `deleted_at IS NULL` matching
    // most of the table), so the average across all keys says nothing about
    // that bucket. Decline, so the caller falls back to the pessimistic
    // heuristic estimate for `IS`.
    if constraint_refs[..eq_prefix_len]
        .iter()
        .any(|cref| cref.eq.as_ref().is_some_and(|eq| eq.null_matching))
    {
        return None;
    }

    if eq_prefix_len == 0 {
        // Pure range scan — ANALYZE per-distinct-value stats don't apply.
        return None;
    }

    let table_name = ctx.rhs_table.table.get_name();

    let table_stats = ctx.stats.table_stats(table_name)?;
    let idx_stats = table_stats.index_stats.get(&index.name)?;

    if eq_prefix_len <= idx_stats.avg_rows_per_distinct_prefix.len() {
        Some(idx_stats.avg_rows_per_distinct_prefix[eq_prefix_len - 1] as f64)
    } else {
        // Stats exist for this index but don't cover the equality prefix length.
        // Return None to fall through to selectivity-based estimation.
        None
    }
}

/// Amphora: SQLite counts a skip-scan's seeks 1.375 times over, its estimates being uncertain, to
/// make one "slightly less likely" (`whereLoopAddBtreeIndex`, "TUNING").
const SKIP_SCAN_SEEK_FUDGE: f64 = 1.375;

/// Amphora: a skip-scan's cost (`docs/design/planner.md`, decision 4), as SQLite's
/// `whereLoopAddBtreeIndex` reckons one: a seek for each distinct value of the index's first column
/// (`rows_per_value` rows share one, as ANALYZE counted), each reading the rows of its value that
/// the constraints on the columns after it keep.
pub(super) fn estimate_skip_scan_cost(
    index_info: IndexInfo,
    constraints: &[Constraint],
    constraint_refs: &[RangeConstraintRef],
    input_cardinality: f64,
    base_row_count: RowCountEstimate,
    rows_per_value: f64,
    params: &CostModelParams,
) -> Cost {
    let rows = estimate_rows_per_seek(
        index_info,
        constraints,
        constraint_refs,
        base_row_count,
        None,
        params,
    );
    let base_row_count = *base_row_count;
    let values = (base_row_count / rows_per_value.max(1.0)).max(1.0);
    estimate_index_cost(
        base_row_count,
        estimate_btree_depth(base_row_count, params.rows_per_table_page),
        index_info,
        input_cardinality * values * SKIP_SCAN_SEEK_FUDGE,
        rows / values,
        params,
    )
}

/// Amphora: what an index read in the query's order costs when it stops after `goal_rows`
/// rows, the rows a literal LIMIT keeps over what the remaining terms keep of them
/// (`docs/design/planner.md` decision 16): a seek, then at most `goal_rows` of its range.
#[expect(clippy::too_many_arguments)]
pub fn estimate_ordered_cost_to_goal(
    index_info: IndexInfo,
    constraints: &[Constraint],
    usable_constraint_refs: &[RangeConstraintRef],
    input_cardinality: f64,
    base_row_count: RowCountEstimate,
    goal_rows: f64,
    params: &CostModelParams,
    analyze_ctx: Option<&AnalyzeCtx>,
) -> Cost {
    let tree_depth = estimate_btree_depth(*base_row_count, params.rows_per_table_page);
    let rows_per_seek = if is_unique_point_lookup(index_info, usable_constraint_refs) {
        1.0
    } else {
        estimate_rows_per_seek(
            index_info,
            constraints,
            usable_constraint_refs,
            RowCountEstimate::AnalyzeStats(*base_row_count),
            analyze_ctx,
            params,
        )
    };
    estimate_index_cost(
        *base_row_count,
        tree_depth,
        index_info,
        input_cardinality,
        rows_per_seek.min(goal_rows.max(1.0)),
        params,
    )
}

/// Estimate the cost of a scan or seek operation.
#[expect(clippy::too_many_arguments)]
pub fn estimate_cost_for_scan_or_seek(
    index_info: Option<IndexInfo>,
    constraints: &[Constraint],
    usable_constraint_refs: &[RangeConstraintRef],
    input_cardinality: f64,
    base_row_count: RowCountEstimate,
    is_index_ordered: bool,
    params: &CostModelParams,
    analyze_ctx: Option<&AnalyzeCtx>,
) -> Cost {
    let base_row_count = *base_row_count;

    let tree_depth = estimate_btree_depth(base_row_count, params.rows_per_table_page);

    let Some(index_info) = index_info else {
        // Full table scan (no index)
        return estimate_scan_cost(base_row_count, input_cardinality, params);
    };

    if is_unique_point_lookup(index_info, usable_constraint_refs) {
        // Unique point lookup: 1 seek per input row, 1 row returned per seek
        return estimate_index_cost(
            base_row_count,
            tree_depth,
            index_info,
            input_cardinality, // num_seeks = outer cardinality
            1.0,               // rows_per_seek = 1 for unique point lookup
            params,
        );
    }

    let rows_per_seek = estimate_rows_per_seek(
        index_info,
        constraints,
        usable_constraint_refs,
        RowCountEstimate::AnalyzeStats(base_row_count),
        analyze_ctx,
        params,
    );

    let base_cost = estimate_index_cost(
        base_row_count,
        tree_depth,
        index_info,
        input_cardinality, // num_seeks = outer cardinality
        rows_per_seek,
        params,
    );

    let is_full_scan = usable_constraint_refs.is_empty();
    // Penalize non-covering indexes doing full scans when not ordered by the index.
    // Without ordering benefit, a full scan on a non-covering index requires random
    // table lookups for each row, which is expensive.
    if !index_info.covering && is_full_scan && !is_index_ordered {
        // Full index scan without ordering benefit - prefer table scan instead
        Cost(base_cost.0 * 2.0)
    } else {
        base_cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::optimizer::cost_params::DEFAULT_PARAMS;

    fn covering_index() -> IndexInfo {
        IndexInfo {
            unique: false,
            column_count: 2,
            covering: true,
            rows_per_leaf_page: 100.0,
        }
    }

    /// Amphora: do: price reading 10 million rows out of a GROUP BY's order, for a GROUP BY that
    /// sorts and for one that hashes its 100 groups. Expect: a sort of the rows for the first; a
    /// probe a row and a sort of the 100 groups for the second, far less (decision 7).
    #[test]
    fn a_hashed_group_by_costs_a_probe_a_row_and_a_sort_of_its_groups() {
        use crate::translate::optimizer::order::{
            EliminatesSortBy, OrderTarget, OrderTargetPurpose,
        };
        let mut target = OrderTarget {
            columns: Vec::new(),
            purpose: OrderTargetPurpose::EliminatesSort(EliminatesSortBy::Group),
            hashed_groups: None,
        };
        let rows = 10_000_000.0;
        let sorted = order_cost(&target, rows, &DEFAULT_PARAMS).0;
        assert!((sorted - estimate_sort_cpu_cost(rows, &DEFAULT_PARAMS).0).abs() < 1e-6);
        target.hashed_groups = Some(100.0);
        let hashed = order_cost(&target, rows, &DEFAULT_PARAMS).0;
        let want = rows * DEFAULT_PARAMS.cpu_cost_per_row
            + estimate_sort_cpu_cost(100.0, &DEFAULT_PARAMS).0;
        assert!((hashed - want).abs() < 1e-6);
        assert!(hashed * 10.0 < sorted);
    }

    /// Amphora: do: look up rows at random in a table bigger than the cache and in one it holds.
    /// Expect: Mackert and Lohman's three cases (decision 5): a page a row while the cache fills,
    /// then every lookup a read but for the fraction of the table cached; at most the table's
    /// pages once the cache holds it.
    #[test]
    fn table_pages_fetched_follows_mackert_and_lohman() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6 * b.max(1.0);
        // T > b, Ns > 2Tb/(2T - b): 200,000 lookups, 33,000 pages, 2,000 cached.
        let fills = 2.0 * 33_000.0 * 2_000.0 / (2.0 * 33_000.0 - 2_000.0);
        let want = 2_000.0 + (200_000.0 - fills) * 31_000.0 / 33_000.0;
        assert!(close(
            table_pages_fetched(200_000.0, 33_000.0, 2_000.0),
            want
        ));
        assert!((187_000.0..189_000.0).contains(&want), "{want}");
        // T > b, Ns <= 2Tb/(2T - b): a lookup a page, nearly.
        let few = table_pages_fetched(100.0, 33_000.0, 2_000.0);
        assert!(close(few, 2.0 * 33_000.0 * 100.0 / (66_000.0 + 100.0)));
        assert!((99.0..100.0).contains(&few), "{few}");
        // T <= b: never more than the table's pages.
        assert!(close(
            table_pages_fetched(200_000.0, 4_000.0, 100_000.0),
            4_000.0
        ));
        assert!(close(
            table_pages_fetched(1_000.0, 4_000.0, 100_000.0),
            2.0 * 4_000.0 * 1_000.0 / (8_000.0 + 1_000.0)
        ));
        assert_eq!(table_pages_fetched(0.0, 4_000.0, 2_000.0), 0.0);
    }

    /// Amphora: do: search an index of 1,000 leaf pages 200 times, with a cache that holds it and
    /// with one of a page. Expect: held, no more than its leaf pages and its inner pages once
    /// (decision 6); not held, every search's pages as before.
    #[test]
    fn repeated_searches_read_no_more_than_the_cache_cannot_hold() {
        let held = DEFAULT_PARAMS.with_cache_pages(100_000);
        let cost = estimate_index_cost(100_000.0, 4.0, covering_index(), 200.0, 1.0, &held);
        let cpu = 200.0 * held.cpu_cost_per_seek + 200.0 * held.cpu_cost_per_row;
        let reads = table_pages_fetched(200.0, 1_000.0, 100_000.0) + 3.0;
        assert!(((cost.0 + held.index_bonus - cpu) - reads).abs() < 1e-9);
        assert!(reads < 200.0, "{reads}");
        let none = DEFAULT_PARAMS.with_cache_pages(1);
        let uncached = estimate_index_cost(100_000.0, 4.0, covering_index(), 200.0, 1.0, &none);
        let per_search = 4.0 + 199.0 * (3.0 + none.cache_reuse_factor);
        assert!(((uncached.0 + none.index_bonus - cpu) - per_search).abs() < 1e-9);
        assert!(cost < uncached);
    }

    #[test]
    fn repeated_point_seeks_keep_non_root_page_costs() {
        // Amphora: a cache of one page, which holds none of the index (decision 6).
        let cost = estimate_index_cost(
            100_000.0,
            4.0,
            covering_index(),
            10.0,
            1.0,
            &DEFAULT_PARAMS.with_cache_pages(1),
        );
        let repeated_tree_search_cost = 3.0 + DEFAULT_PARAMS.cache_reuse_factor;
        let expected_tree_search_cost = 4.0 + 9.0 * repeated_tree_search_cost;
        let expected_cpu_cost =
            10.0 * DEFAULT_PARAMS.cpu_cost_per_seek + 10.0 * DEFAULT_PARAMS.cpu_cost_per_row;
        let expected = expected_tree_search_cost + expected_cpu_cost - DEFAULT_PARAMS.index_bonus;
        assert!((cost.0 - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn repeated_range_seeks_reuse_cached_root_and_leaf_pages() {
        let cost = estimate_index_cost(
            100_000.0,
            4.0,
            covering_index(),
            10.0,
            100.0,
            &DEFAULT_PARAMS.with_cache_pages(1),
        );
        let repeated_tree_search_cost = 2.0 + DEFAULT_PARAMS.cache_reuse_factor;
        let expected_tree_search_cost = 4.0 + 9.0 * repeated_tree_search_cost;
        let expected_leaf_cost = 1.0 + 9.0 * DEFAULT_PARAMS.cache_reuse_factor;
        let expected_cpu_cost =
            10.0 * DEFAULT_PARAMS.cpu_cost_per_seek + 1_000.0 * DEFAULT_PARAMS.cpu_cost_per_row;
        let expected = expected_tree_search_cost + expected_leaf_cost + expected_cpu_cost
            - DEFAULT_PARAMS.index_bonus;
        assert!((cost.0 - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn lower_cache_reuse_factor_reduces_repeated_read_cost() {
        let low_reuse_cost = CostModelParams {
            cache_reuse_factor: 0.8,
            cache_pages: 1.0,
            ..DEFAULT_PARAMS.clone()
        };
        let high_reuse_cost = CostModelParams {
            cache_reuse_factor: 0.2,
            cache_pages: 1.0,
            ..DEFAULT_PARAMS.clone()
        };

        assert!(
            estimate_scan_cost(100_000.0, 10.0, &high_reuse_cost)
                < estimate_scan_cost(100_000.0, 10.0, &low_reuse_cost)
        );
        for rows_per_seek in [1.0, 100.0] {
            assert!(
                estimate_index_cost(
                    100_000.0,
                    4.0,
                    covering_index(),
                    10.0,
                    rows_per_seek,
                    &high_reuse_cost,
                ) < estimate_index_cost(
                    100_000.0,
                    4.0,
                    covering_index(),
                    10.0,
                    rows_per_seek,
                    &low_reuse_cost,
                )
            );
        }
    }
}
