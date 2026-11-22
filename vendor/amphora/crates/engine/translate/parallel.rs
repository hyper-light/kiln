//! Amphora (QT.6, the query-speed plan's step 6): the VDBE's hook for morsel-driven parallel
//! execution. A top-level SELECT whose whole work is one table scan with its filter and
//! aggregates is handed to `Insn::MorselAggregate`, which reads the table's leaf pages in
//! morsels, a cursor per worker (`vdbe/morsel_scan.rs`, docs/design/executor.md section 8). The
//! statement's serial loop is still emitted after it and runs when the parallel run declines a
//! value, so the answer never depends on the switch.
//!
//! Only the shapes whose answers equal the serial loop's value for value qualify: one rowid
//! table read whole, no index, every column it reads stored as written (INTEGER, TEXT or BLOB
//! affinity, BINARY collation, no default a short row would read, no generated column); terms
//! comparing a column with an integer or text literal of its own kind, or testing it for NULL;
//! plain-column GROUP BY keys; COUNT, SUM over INTEGER columns, MIN and MAX; no HAVING, ORDER
//! BY, LIMIT, DISTINCT, window or subquery.
//!
//! A hash join's probe with its aggregates qualifies too (`shape_join`): two rowid tables the
//! planner joins inner by a hash join on equalities of columns of one affinity, every other term
//! a filter on one table, every column read of INTEGER or TEXT affinity and BINARY collation.
//! The build side is read and its hash table built once; each worker probes over its own run of
//! the probe table's leaves and folds the joined rows into its partial aggregates.

use crate::function::AggFunc;
use crate::schema::{BTreeTable, Table};
use crate::translate::collate::CollationSeq;
use crate::translate::plan::{
    Distinctness, HashJoinType, JoinType, JoinedTable, Operation, QueryDestination, Scan,
    SelectPlan, WhereTermOrigin,
};
use crate::types::Value;
use crate::vdbe::affinity::Affinity;
use crate::vdbe::builder::ProgramBuilder;
use crate::vdbe::insn::{Insn, MorselAggregateData, MorselJoinData};
use crate::vdbe::morsel::{Aggregate, CmpOp, Predicate};
use amphora_engine_parser::ast::{Expr, Literal, Operator, UnaryOperator};

/// The labels the hook's code jumps to: the serial loop it falls back to, and the statement's end.
pub(crate) struct MorselHook {
    pub end: crate::vdbe::BranchOffset,
}

/// Emits the parallel run and its row loop ahead of the serial loop when `plan` qualifies.
pub(crate) fn emit_morsel_hook(
    program: &mut ProgramBuilder,
    plan: &SelectPlan,
) -> Option<MorselHook> {
    let data = shape(plan).or_else(|| shape_join(plan))?;
    let serial = program.allocate_label();
    let end = program.allocate_label();
    let next = program.allocate_label();
    let count = plan.result_columns.len();
    let dest_start = program.alloc_registers(count);
    program.emit_insn(Insn::MorselAggregate {
        data: Box::new(MorselAggregateData {
            target_pc_serial: serial,
            ..data
        }),
    });
    program.preassign_label_to_next_insn(next);
    program.emit_insn(Insn::MorselNext {
        dest_start,
        count,
        target_pc_done: end,
    });
    program.emit_insn(Insn::ResultRow {
        start_reg: dest_start,
        count,
    });
    program.emit_insn(Insn::Goto { target_pc: next });
    program.preassign_label_to_next_insn(serial);
    Some(MorselHook { end })
}

fn shape(plan: &SelectPlan) -> Option<MorselAggregateData> {
    if !plain(plan) {
        return None;
    }
    let [joined] = plan.table_references.joined_tables() else {
        return None;
    };
    let Operation::Scan(Scan::BTreeTable { index: None, .. }) = &joined.op else {
        return None;
    };
    let table = stored(joined)?;
    let columns = table.columns();
    let id = joined.internal_id;
    // A column the hook reads: of this table, stored as written, compared bytewise.
    let column = |expr: &Expr| -> Option<usize> {
        let Expr::Column {
            table: t, column, ..
        } = expr
        else {
            return None;
        };
        if *t != id {
            return None;
        }
        let col = columns.get(*column)?;
        let plain = matches!(
            col.affinity(),
            Affinity::Integer | Affinity::Text | Affinity::Blob | Affinity::None
        ) && matches!(col.collation(), CollationSeq::Binary | CollationSeq::Unset);
        plain.then_some(*column)
    };
    let mut filter = Vec::new();
    for term in &plan.where_clause {
        if term.consumed {
            continue;
        }
        filter.push(predicate(&term.expr, &column, table)?);
    }
    let integer = |at: usize| {
        columns
            .get(at)
            .is_some_and(|c| c.affinity() == Affinity::Integer)
    };
    let (group_by, aggregates, result_map) = outputs(plan, &column, &integer)?;
    Some(MorselAggregateData {
        root_page: table.root_page,
        columns: columns.len(),
        rowid_alias: rowid_alias(table),
        filter,
        group_by,
        aggregates,
        result_map,
        join: None,
        target_pc_serial: crate::vdbe::BranchOffset::Placeholder,
    })
}

fn rowid_alias(table: &BTreeTable) -> Option<usize> {
    table.get_rowid_alias_column().map(|(at, _)| at)
}

fn predicate(
    expr: &Expr,
    column: &impl Fn(&Expr) -> Option<usize>,
    table: &BTreeTable,
) -> Option<Predicate> {
    match expr {
        Expr::IsNull(inner) => Some(Predicate::IsNull {
            column: column(inner)?,
        }),
        Expr::NotNull(inner) => Some(Predicate::NotNull {
            column: column(inner)?,
        }),
        Expr::Binary(left, op, right) => {
            let (at, op, value) = match (column(left), column(right)) {
                (Some(at), None) => (at, *op, literal(right)?),
                (None, Some(at)) => (at, flip(*op)?, literal(left)?),
                _ => return None,
            };
            let op = match op {
                Operator::Less => CmpOp::Lt,
                Operator::LessEquals => CmpOp::Le,
                Operator::Equals => CmpOp::Eq,
                Operator::NotEquals => CmpOp::Ne,
                Operator::Greater => CmpOp::Gt,
                Operator::GreaterEquals => CmpOp::Ge,
                _ => return None,
            };
            // A literal of the column's own kind needs no conversion before it is compared.
            let affinity = table.columns().get(at)?.affinity();
            let same_kind = match &value {
                Value::Text(_) => affinity == Affinity::Text,
                Value::Numeric(crate::numeric::Numeric::Integer(_)) => {
                    affinity == Affinity::Integer
                }
                _ => false,
            };
            same_kind.then_some(Predicate::Compare {
                column: at,
                op,
                value,
            })
        }
        _ => None,
    }
}

fn flip(op: Operator) -> Option<Operator> {
    Some(match op {
        Operator::Less => Operator::Greater,
        Operator::LessEquals => Operator::GreaterEquals,
        Operator::Greater => Operator::Less,
        Operator::GreaterEquals => Operator::LessEquals,
        Operator::Equals => Operator::Equals,
        Operator::NotEquals => Operator::NotEquals,
        _ => return None,
    })
}

/// An integer or a text literal's value; anything else declines the hook.
fn literal(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Literal(Literal::Numeric(text)) => text.parse::<i64>().ok().map(Value::from_i64),
        Expr::Unary(UnaryOperator::Negative, inner) => match inner.as_ref() {
            Expr::Literal(Literal::Numeric(text)) => text
                .parse::<i64>()
                .ok()
                .and_then(i64::checked_neg)
                .map(Value::from_i64),
            _ => None,
        },
        Expr::Literal(Literal::String(quoted)) => {
            let inner = quoted.strip_prefix('\'')?.strip_suffix('\'')?;
            Some(Value::build_text(inner.replace("''", "'")))
        }
        _ => None,
    }
}

/// The checks every shape shares: a top-level SELECT whose rows are its aggregates' groups, with
/// none of what the pipelines don't run (ORDER BY, LIMIT, DISTINCT, windows, subqueries).
fn plain(plan: &SelectPlan) -> bool {
    !(plan.aggregates.is_empty()
        || !plan.order_by.is_empty()
        || plan.limit.is_some()
        || plan.offset.is_some()
        || plan.contains_constant_false_condition
        || !matches!(plan.query_destination, QueryDestination::ResultRows)
        || !matches!(plan.distinctness, Distinctness::NonDistinct)
        || !plan.values.is_empty()
        || plan.window.is_some()
        || !plan.non_from_clause_subqueries.is_empty()
        || plan.simple_aggregate.is_some()
        || !plan.table_references.outer_query_refs().is_empty())
}

/// A rowid table whose rows the morsels read as stored: no default a short row would read, no
/// generated column.
fn stored(joined: &JoinedTable) -> Option<&BTreeTable> {
    let Table::BTree(table) = &joined.table else {
        return None;
    };
    let table: &BTreeTable = table;
    if !table.has_rowid
        || table.root_page <= 0
        || table.has_virtual_columns()
        || table
            .columns()
            .iter()
            .any(|c| c.default.is_some() || c.is_generated())
    {
        return None;
    }
    Some(table)
}

/// The GROUP BY keys, the aggregates and each result column's place in the pipeline's row (the
/// keys, then the aggregates), the columns read through `column`; `integer` says whether a column
/// is one SUM may take (INTEGER affinity).
fn outputs(
    plan: &SelectPlan,
    column: &impl Fn(&Expr) -> Option<usize>,
    integer: &impl Fn(usize) -> bool,
) -> Option<(Vec<usize>, Vec<Aggregate>, Vec<usize>)> {
    let mut group_by = Vec::new();
    if let Some(group) = &plan.group_by {
        if group.having.is_some() {
            return None;
        }
        for expr in &group.exprs {
            group_by.push(column(expr)?);
        }
    }
    let mut aggregates = Vec::new();
    for aggregate in &plan.aggregates {
        if aggregate.filter_expr.is_some()
            || !matches!(aggregate.distinctness, Distinctness::NonDistinct)
        {
            return None;
        }
        aggregates.push(match (&aggregate.func, aggregate.args.as_slice()) {
            (AggFunc::Count0, []) => Aggregate::CountStar,
            (AggFunc::Count, [arg]) => Aggregate::Count(column(arg)?),
            (AggFunc::Sum, [arg]) => {
                let at = column(arg)?;
                if !integer(at) {
                    return None;
                }
                Aggregate::Sum(at)
            }
            (AggFunc::Min, [arg]) => Aggregate::Min(column(arg)?),
            (AggFunc::Max, [arg]) => Aggregate::Max(column(arg)?),
            _ => return None,
        });
    }
    let mut result_map = Vec::new();
    for result in &plan.result_columns {
        let at = match group_by
            .iter()
            .position(|g| Some(*g) == column(&result.expr))
        {
            Some(at) if !result.contains_aggregates => at,
            _ => {
                let found = plan
                    .aggregates
                    .iter()
                    .position(|a| a.original_expr == result.expr)?;
                group_by.len().checked_add(found)?
            }
        };
        result_map.push(at);
    }
    Some((group_by, aggregates, result_map))
}

/// A hash join's probe with the aggregates over its matches (QT.6): two rowid tables joined inner
/// by the planner's hash join on equalities of columns of one affinity, every other term a filter
/// on one table's columns. The pipeline's row is the probe table's columns, then the build's.
///
/// Every column read has INTEGER or TEXT affinity and BINARY collation: two such values that
/// compare equal are the same value, so which joined row a MIN, a MAX or a group's key values take
/// on a tie (the serial loop's order of a probe row's matches is its hash table's) cannot change
/// the answer.
fn shape_join(plan: &SelectPlan) -> Option<MorselAggregateData> {
    if !plain(plan) {
        return None;
    }
    let [first, second] = plan.table_references.joined_tables() else {
        return None;
    };
    let tables = [first, second];
    let hash = tables.iter().find_map(|t| match &t.op {
        Operation::HashJoin(op) => Some(op),
        _ => None,
    })?;
    if hash.join_type != HashJoinType::Inner
        || hash.probe_table_idx == hash.build_table_idx
        || tables.iter().any(|t| {
            t.join_info
                .as_ref()
                .is_some_and(|j| j.join_type != JoinType::Inner)
        })
    {
        return None;
    }
    let probe = tables.get(hash.probe_table_idx)?;
    let build = tables.get(hash.build_table_idx)?;
    let probe_table = stored(probe)?;
    let build_table = stored(build)?;
    let width = probe_table.columns().len();
    // A column the hook reads, and whether it is the probe table's.
    let side = |expr: &Expr| -> Option<(bool, usize)> {
        let Expr::Column {
            table: t, column, ..
        } = expr
        else {
            return None;
        };
        let (is_probe, table) = if *t == probe.internal_id {
            (true, probe_table)
        } else if *t == build.internal_id {
            (false, build_table)
        } else {
            return None;
        };
        let col = table.columns().get(*column)?;
        let plain = matches!(col.affinity(), Affinity::Integer | Affinity::Text)
            && matches!(col.collation(), CollationSeq::Binary | CollationSeq::Unset);
        plain.then_some((is_probe, *column))
    };
    let joined_at = |expr: &Expr| -> Option<usize> {
        let (is_probe, at) = side(expr)?;
        if is_probe {
            Some(at)
        } else {
            width.checked_add(at)
        }
    };
    let integer = |at: usize| -> bool {
        let col = match at.checked_sub(width) {
            None => probe_table.columns().get(at),
            Some(at) => build_table.columns().get(at),
        };
        col.is_some_and(|c| c.affinity() == Affinity::Integer)
    };
    let mut probe_key = Vec::new();
    let mut build_key = Vec::new();
    let mut keys = Vec::new();
    for key in &hash.join_keys {
        let term = plan.where_clause.get(key.where_clause_idx)?;
        let Expr::Binary(left, Operator::Equals, right) = &term.expr else {
            return None;
        };
        let (at_probe, at_build) = match (side(left)?, side(right)?) {
            ((true, p), (false, b)) | ((false, b), (true, p)) => (p, b),
            _ => return None,
        };
        // Keys of one affinity compare with no conversion, as the join's hash table does.
        if probe_table.columns().get(at_probe)?.affinity()
            != build_table.columns().get(at_build)?.affinity()
        {
            return None;
        }
        probe_key.push(at_probe);
        build_key.push(at_build);
        keys.push(key.where_clause_idx);
    }
    if keys.is_empty() {
        return None;
    }
    let on_probe = |expr: &Expr| side(expr).and_then(|(p, at)| p.then_some(at));
    let on_build = |expr: &Expr| side(expr).and_then(|(p, at)| (!p).then_some(at));
    let mut probe_filter = Vec::new();
    let mut build_filter = Vec::new();
    // Every term that is not a key is a filter on one table, consumed or not: one the planner
    // made into a seek still holds of every row the join reads.
    for (at, term) in plan.where_clause.iter().enumerate() {
        if keys.contains(&at) {
            continue;
        }
        if matches!(term.origin, WhereTermOrigin::TableFunction(_)) {
            return None;
        }
        match predicate(&term.expr, &on_probe, probe_table) {
            Some(found) => probe_filter.push(found),
            None => build_filter.push(predicate(&term.expr, &on_build, build_table)?),
        }
    }
    let (group_by, aggregates, result_map) = outputs(plan, &joined_at, &integer)?;
    Some(MorselAggregateData {
        root_page: probe_table.root_page,
        columns: width,
        rowid_alias: rowid_alias(probe_table),
        filter: probe_filter,
        group_by,
        aggregates,
        result_map,
        join: Some(MorselJoinData {
            root_page: build_table.root_page,
            columns: build_table.columns().len(),
            rowid_alias: rowid_alias(build_table),
            filter: build_filter,
            probe_key,
            build_key,
        }),
        target_pc_serial: crate::vdbe::BranchOffset::Placeholder,
    })
}
