//! Amphora: a row-value range, `(a, b, c) > (?, ?, ?)`, as a range of an index its columns lead
//! (`docs/design/planner.md`, decision 14), as SQLite plans one: a bound on its first column, which a
//! search of an index takes by as many of its elements as the index's columns from there on match
//! (`whereRangeVectorLen`), from the first key at or past the bound whatever the operator, the
//! comparison filtering every row read (`sqlite3WhereCodeOneLoopStart`: `startEq` and `endEq` set,
//! the term never disabled). So a NULL in any element but the first only positions the seek, and the
//! comparison decides the rows it reads.

use super::constraints::{BinaryExprSide, Constraint, RangeConstraintRef};
use super::cost_params::CostModelParams;
use crate::schema::Index;
use crate::translate::collate::{resolve_comparison_collseq, CollationSeq};
use crate::translate::emitter::Resolver;
use crate::translate::expr::{
    comparison_affinity, expr_references_any_subquery, get_expr_affinity, unwrap_parens,
};
use crate::translate::plan::{
    IterationDirection, JoinedTable, NonFromClauseSubquery, SeekDef, SeekKeyComponent,
    TableReferences, WhereTerm,
};
use crate::translate::planner::{table_mask_from_expr, TableMask};
use crate::vdbe::affinity::Affinity;
use crate::Result;
use amphora_engine_parser::ast::{self, SortOrder, TableInternalId};

type Row = [Box<ast::Expr>];

/// A row-value range comparison as written: its left row, its operator and its right row, of one
/// length, two elements or more.
fn rows(expr: &ast::Expr) -> Option<(&Row, ast::Operator, &Row)> {
    let ast::Expr::Binary(lhs, op, rhs) = unwrap_parens(expr).ok()? else {
        return None;
    };
    let (ast::Expr::Parenthesized(left), ast::Expr::Parenthesized(right)) =
        (unwrap_parens(lhs).ok()?, unwrap_parens(rhs).ok()?)
    else {
        return None;
    };
    let range = matches!(
        op,
        ast::Operator::Greater
            | ast::Operator::GreaterEquals
            | ast::Operator::Less
            | ast::Operator::LessEquals
    );
    if range && left.len() > 1 && left.len() == right.len() {
        Some((left.as_slice(), *op, right.as_slice()))
    } else {
        None
    }
}

/// The table and column `expr` names, if it is a plain column.
fn column_of(expr: &ast::Expr) -> Option<(TableInternalId, usize)> {
    match unwrap_parens(expr).ok()? {
        ast::Expr::Column { table, column, .. } => Some((*table, *column)),
        _ => None,
    }
}

/// The constraint the row-value range at `pos` puts on `table_reference`, when the first element of
/// one side is a column of the table and the other side reads no subquery: a bound on that column at
/// or past the other side's first element, implied by the term, which still filters every row (so a
/// seek on it consumes nothing, and as a filter it counts for nothing beside the term).
pub(super) fn constraint(
    pos: usize,
    term: &WhereTerm,
    table_reference: &JoinedTable,
    table_references: &TableReferences,
    subqueries: &[NonFromClauseSubquery],
    outer_join_compatible: bool,
    params: &CostModelParams,
) -> Result<Option<Constraint>> {
    let Some((left, op, right)) = rows(&term.expr) else {
        return Ok(None);
    };
    let leading_column = |row: &Row| {
        row.first()
            .and_then(|e| column_of(e))
            .filter(|(table, _)| *table == table_reference.internal_id)
            .map(|(_, column)| column)
    };
    let greater = matches!(op, ast::Operator::Greater | ast::Operator::GreaterEquals);
    let (columns, bound, lower, bound_side) = match (leading_column(left), leading_column(right)) {
        (Some(_), _) => (left, right, greater, BinaryExprSide::Rhs),
        (None, Some(_)) => (right, left, !greater, BinaryExprSide::Lhs),
        (None, None) => return Ok(None),
    };
    let (Some(column), Some(table_col_pos), Some(value)) =
        (columns.first(), leading_column(columns), bound.first())
    else {
        return Ok(None);
    };
    if bound.iter().any(|e| expr_references_any_subquery(e)) {
        return Ok(None);
    }
    let mut lhs_mask = TableMask::default();
    for e in bound {
        lhs_mask.union_with(&table_mask_from_expr(e, table_references, subqueries)?)?;
    }
    let operator = if lower {
        ast::Operator::GreaterEquals
    } else {
        ast::Operator::LessEquals
    };
    let compared_by = comparison_affinity(
        column.as_ref(),
        value.as_ref(),
        Some(table_references),
        None,
    );
    let converts_by = if compared_by.expr_needs_no_affinity_change(value) {
        Affinity::Blob
    } else {
        compared_by
    };
    Ok(Some(Constraint {
        where_clause_pos: (pos, bound_side),
        operator: operator.into(),
        table_col_pos: Some(table_col_pos),
        expr: None,
        constraining_expr: Some((operator, value.as_ref().clone(), converts_by)),
        lhs_mask,
        selectivity: params.sel_range,
        usable: true,
        outer_join_compatible,
        is_rowid: false,
        comparison_affinity: Some(compared_by),
        null_matching: false,
        implied: true,
    }))
}

/// The rows of the row-value range `constraint` comes from, as written, and whether the columns'
/// row is the left one; None for any other constraint.
fn written_rows<'a>(
    constraint: &Constraint,
    where_clause: &'a [WhereTerm],
) -> Option<(&'a Row, &'a Row, bool)> {
    let term = where_clause.get(constraint.where_clause_pos.0)?;
    let (left, _, right) = rows(&term.expr)?;
    Some((
        left,
        right,
        constraint.where_clause_pos.1 == BinaryExprSide::Rhs,
    ))
}

/// The collation a row-value range `constraint` compares its first elements by, which a seek on its
/// first column must share (SQLite takes a vector's collation from its first element); None for any
/// other constraint.
pub(super) fn first_collation(
    constraint: &Constraint,
    where_clause: &[WhereTerm],
    table_references: &TableReferences,
) -> Result<Option<CollationSeq>> {
    let Some((left, right, _)) = written_rows(constraint, where_clause) else {
        return Ok(None);
    };
    let (Some(l), Some(r)) = (left.first(), right.first()) else {
        return Ok(None);
    };
    Ok(Some(resolve_comparison_collseq(l, r, table_references)?))
}

/// Whether SQLite's comparison affinity of a bound element of affinity `bound` and a column of
/// affinity `column` is the column's own (`sqlite3CompareAffinity`, which `whereRangeVectorLen`
/// requires of each element after the first): a bound of no affinity compares by the column's, two
/// affinities by NUMERIC if either is numeric, else by BLOB.
fn compares_by_column_affinity(column: Affinity, bound: Affinity) -> bool {
    if bound == Affinity::None {
        true
    } else if column.is_numeric() || bound.is_numeric() {
        column == Affinity::Numeric
    } else {
        column == Affinity::Blob
    }
}

/// The elements of the row-value range `constraint` a search of `index` takes, from the index
/// column at `first` on, each with the affinity its comparison converts it by: as many as the
/// index's columns match one by one (`whereRangeVectorLen`: the element is that column, sorted as the
/// first, compared by its affinity and its collation), and never a column of a custom type, whose
/// index order is its encoding's. None unless two or more match.
fn row_elements(
    constraint: &Constraint,
    index: &Index,
    first: usize,
    where_clause: &[WhereTerm],
    table_references: &TableReferences,
    resolver: &Resolver,
) -> Result<Option<Vec<(ast::Expr, Affinity)>>> {
    let Some((left, right, columns_left)) = written_rows(constraint, where_clause) else {
        return Ok(None);
    };
    let (columns, bound) = if columns_left {
        (left, right)
    } else {
        (right, left)
    };
    let (Some(order), Some((table, _))) = (
        index.columns.get(first).map(|c| c.order),
        columns.first().and_then(|e| column_of(e)),
    ) else {
        return Ok(None);
    };
    let Some((_, table_ref)) = table_references.find_table_by_internal_id(table) else {
        return Ok(None);
    };
    let mut elements = Vec::with_capacity(columns.len());
    let pairs = columns
        .iter()
        .zip(bound)
        .zip(index.columns.iter().skip(first));
    for ((column, value), index_column) in pairs {
        if !elements.is_empty() {
            let Some(table_col_pos) = column_of(column)
                .filter(|(t, _)| *t == table)
                .map(|(_, c)| c)
            else {
                break;
            };
            let custom_type = table_ref.get_column_at(table_col_pos).is_some_and(|c| {
                resolver
                    .schema()
                    .get_type_def(&c.ty_str, table_ref.is_strict())
                    .is_some()
            });
            let (l, r) = if columns_left {
                (column, value)
            } else {
                (value, column)
            };
            let collation = resolve_comparison_collseq(l, r, table_references)?;
            if index_column.expr.is_some()
                || index_column.pos_in_table != table_col_pos
                || index_column.order != order
                || custom_type
                || collation != index_column.collation.unwrap_or_default()
                || !compares_by_column_affinity(
                    get_expr_affinity(column, Some(table_references), Some(resolver)),
                    get_expr_affinity(value, Some(table_references), Some(resolver)),
                )
            {
                break;
            }
        }
        let compared_by = comparison_affinity(
            column.as_ref(),
            value.as_ref(),
            Some(table_references),
            Some(resolver),
        );
        let converts_by = if compared_by.expr_needs_no_affinity_change(value) {
            Affinity::Blob
        } else {
            compared_by
        };
        elements.push((value.as_ref().clone(), converts_by));
    }
    Ok(if elements.len() > 1 {
        Some(elements)
    } else {
        None
    })
}

/// A search of `index` whose range is a row value takes as many of its elements as the index's
/// columns match: the key the bound sets, the start or the end of the scan, becomes that row. Not
/// for a skip-scan or an automatic index, which keep the bound on the first column.
pub(super) fn extend_row_bounds(
    seek_def: &mut SeekDef,
    index: &Index,
    constraints: &[Constraint],
    constraint_refs: &[RangeConstraintRef],
    where_clause: &[WhereTerm],
    table_references: &TableReferences,
    resolver: &Resolver,
) -> Result<()> {
    if index.ephemeral || seek_def.skip > 0 {
        return Ok(());
    }
    let Some(range) = constraint_refs.last().filter(|r| r.eq.is_none()) else {
        return Ok(());
    };
    // A forward scan of an ascending column starts at its lower bound, as does a backward scan of a
    // descending one (`build_seek_def`).
    let lower_starts =
        (seek_def.iter_dir == IterationDirection::Forwards) == (range.sort_order == SortOrder::Asc);
    for (bound, lower) in [(range.lower_bound, true), (range.upper_bound, false)] {
        let Some(constraint) = bound.and_then(|pos| constraints.get(pos)) else {
            continue;
        };
        let Some(row) = row_elements(
            constraint,
            index,
            range.index_col_pos,
            where_clause,
            table_references,
            resolver,
        )?
        else {
            continue;
        };
        let key = if lower == lower_starts {
            &mut seek_def.start
        } else {
            &mut seek_def.end
        };
        // The key holds the bound's first element, the one its row starts with.
        let holds_first = matches!(
            (&key.last_component, row.first()),
            (SeekKeyComponent::Expr(expr), Some((first, _))) if expr == first
        );
        if holds_first {
            key.last_component = SeekKeyComponent::Row(row);
        }
    }
    Ok(())
}
