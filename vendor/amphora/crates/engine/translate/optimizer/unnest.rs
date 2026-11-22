//! Changes a correlated subquery into a join when both forms give the same rows.
//!
//! - `EXISTS` becomes a semi-join. It keeps an outer row when an inner row matches.
//! - `NOT EXISTS` becomes an anti-join. It keeps an outer row when no inner row matches.
//! - A direct positive `IN` becomes a semi-join with one extra equality test.
//! - A subquery that returns one aggregate value is grouped by the columns that
//!   link it to the outer query.
//!
//! The last rewrite turns this query:
//!
//! ```sql
//! SELECT * FROM outer_table o
//! WHERE o.value < (SELECT avg(i.value) FROM inner_table i WHERE i.key = o.key)
//! ```
//!
//! into this form:
//!
//! ```sql
//! SELECT * FROM outer_table o
//! LEFT JOIN (
//!   SELECT avg(value) AS avg_value, key FROM inner_table GROUP BY key
//! ) i
//!   ON i.key = o.key
//! WHERE o.value < i.avg_value
//! ```
//!
//! This is the **group-first** form: it groups the whole inner table first and
//! then joins the groups to the outer rows. In this module, a "key" means one
//! distinct value of the column that links the two queries (`i.key` above).
//! Group-first:
//!
//! - computes the aggregate for each key once, even when many outer rows ask
//!   for the same key;
//! - also computes the aggregate for keys that no outer row asks for; and
//! - is used for `avg`, `count`, `min`, `max`, and `total` when their inputs,
//!   aggregate `FILTER` expressions, and inner `WHERE` expressions cannot fail.
//!
//! The second point is the dangerous one. The original subquery only reads the
//! keys that outer rows ask for. Group-first reads every key, so it can hit an
//! error that the original query never hits. For example:
//!
//! - `sum` can overflow for unused key 99 when its values are
//!   `9223372036854775807` and `1`, although no outer row asks for 99;
//! - `group_concat` or `string_agg` can build an unused string larger than the
//!   largest SQL value that Turso allows; and
//! - `avg(json_extract(value, '$'))` can read invalid JSON for an unused key.
//!
//! When group-first is unsafe, a direct `WHERE` comparison can instead use the
//! **join-first** form: it joins each outer row to its matching inner rows
//! first and then groups the joined rows back into one group per outer row.
//!
//! ```sql
//! -- Before
//! SELECT o.id
//! FROM outer_table o
//! WHERE o.limit > (
//!   SELECT sum(i.value) FROM inner_table i WHERE i.key = o.key
//! );
//!
//! -- After
//! SELECT o.id
//! FROM outer_table o
//! LEFT JOIN inner_table i ON i.key = o.key
//! GROUP BY o.rowid
//! HAVING o.limit > sum(i.value) FILTER (WHERE i.rowid IS NOT NULL)
//! ```
//!
//! The join only finds inner rows whose key some outer row asks for, so
//! join-first never computes an aggregate for an unused key. The `i.rowid`
//! filter keeps the NULL-filled row that a left join makes when no inner row
//! matches out of the aggregate. Without it, `sum(1)` would read that row and
//! return 1, while the original subquery returns NULL when nothing matches.
//!
//! Join-first is not used for every aggregate subquery:
//!
//! - Group-first does less work when several outer rows ask for the same key,
//!   because join-first computes the aggregate again for each of those rows.
//! - Join-first needs one outer B-tree table with a rowid so that
//!   `GROUP BY o.rowid` makes exactly one group for each outer row.
//! - It needs one inner B-tree table with a rowid, because `i.rowid IS NOT
//!   NULL` is how it recognizes the NULL-filled row made by a left join with
//!   no match.
//! - The current code moves only one direct `WHERE` comparison to `HAVING`. It
//!   does not handle the subquery value in a `SELECT` list, `ORDER BY`,
//!   `HAVING`, or inside a larger expression.
//! - The join makes each outer row appear once for each matching inner row,
//!   and the other `WHERE` terms then run once per copy instead of once per
//!   outer row. A term such as `random() % 2 = 0` could accept some copies and
//!   reject others, so the aggregate would see only some of the row's inner
//!   rows. Join-first is skipped when another `WHERE` term calls a
//!   nondeterministic function or reads a correlated subquery.
//! - Join-first must group by `o.rowid` to keep outer rows separate. An existing
//!   `GROUP BY o.key` may instead combine several outer rows. Combining these
//!   two groups needs another rewrite. Outer aggregates, `DISTINCT`, window
//!   functions, `ORDER BY`, `LIMIT`, and `OFFSET` also need separate rules
//!   around the new group. Those rules are not implemented.
//! - Extension aggregates stay as subqueries. With no matching inner row,
//!   `count` returns 0 and `sum` returns NULL. An extension aggregate may return
//!   something else, and this code does not know which value to use.
//!
//! The `EXISTS`, `NOT EXISTS`, and direct positive `IN` rewrites can move
//! comparisons between inner and outer expressions.
//! They keep an inner `WHERE` expression in the subquery if it can fail.
//! A join can skip a row that the subquery tests and hide an error.
//! Aggregate rewrites only move direct `=` checks between inner and outer columns.
//! A one-value subquery stays as it is unless its result for an empty input is known.
//!
//! The forms above are the fixed patterns ([`Reach::Patterns`]). PL.10 adds the general forms
//! ([`Reach::General`]), Neumann and Kemper's dependent join taken apart by pulling the correlated
//! predicates up out of the subquery ("pull predicates up to eliminate correlations", slide 8 of
//! the talk cited below) and, where a predicate can't be pulled above the subquery's own operators,
//! by joining on the columns that stand in for the outer values (slide 18: the domain `D` "inferred"
//! from equivalent attributes, a superset the final join trims):
//!
//! - `NOT IN` in a `WHERE` conjunct becomes an anti-join whose condition also holds where a NULL
//!   would make the test NULL (`x = y OR x IS NULL OR y IS NULL`, the parts a NOT NULL column can't
//!   meet left out): in `WHERE` a NULL test fails the row as FALSE does.
//! - An `EXISTS`, `NOT EXISTS`, `IN` or `NOT IN` conjunct whose subquery the splice can't take
//!   (several tables, `DISTINCT`, `ORDER BY`) becomes a semi- or anti-join with a derived table: the
//!   subquery with its correlated conjuncts lifted out, reading no outer row, run once.
//! - An `EXISTS` anywhere else (under `OR`, in the `SELECT` list) correlated by `=` alone becomes a
//!   left join with its keys grouped, one row each, and the test `key IS NOT NULL`: a mark join.
//!
//! Each subquery is taken from a worklist, the query's own first (top-down, Neumann's 2025 order:
//! each operator visited once), with a counted bound, never by recursion (CLAUDE.md §11). A
//! derived table's own correlated subqueries are unnested when the planner plans it, after this
//! query: top-down again. A subquery none of the forms takes stays correlated, and EXPLAIN shows it
//! as a correlated subquery.
//!
//! References:
//! - SQLite subquery results: <https://sqlite.org/lang_expr.html#subquery_expressions>
//! - PostgreSQL subquery results: <https://www.postgresql.org/docs/current/functions-subquery.html>
//! - MySQL semi-joins: <https://dev.mysql.com/doc/refman/8.4/en/semijoins-antijoins.html>
//! - MySQL scalar decorrelation: <https://dev.mysql.com/doc/refman/8.4/en/correlated-subqueries.html>
//! - MySQL optimizer switches: <https://dev.mysql.com/doc/refman/8.0/en/switchable-optimizations.html>
//! - MariaDB semi-joins: <https://mariadb.com/docs/server/ha-and-performance/optimization-and-tuning/query-optimizations/subquery-optimizations/semi-join-subquery-optimizations>
//! - MariaDB materialization: <https://mariadb.com/docs/server/ha-and-performance/optimization-and-tuning/query-optimizations/subquery-optimizations/optimization-strategies/semi-join-materialization-strategy>
//! - MariaDB subquery cache: <https://mariadb.com/docs/server/ha-and-performance/optimization-and-tuning/query-optimizations/subquery-optimizations/subquery-cache>
//! - Neumann and Kemper, Unnesting Arbitrary Queries: <https://db.cs.tum.edu/teaching/ws2122/foundationsde/unnesting.pdf>
//! - Neumann, A Formalization of Top-Down Unnesting: <https://arxiv.org/abs/2412.04294>

use std::collections::VecDeque;

use amphora_engine_parser::ast::{
    self, Expr, FunctionTail, Name, SortOrder, TableInternalId, UnaryOperator,
};
use rustc_hash::FxHashMap as HashMap;
use smallvec::SmallVec;

use crate::translate::plan::Plan;

use super::Optimizable;
use crate::function::AggFunc;
use crate::schema::Table;
use crate::sync::Arc;
use crate::translate::{
    collate::get_collseq_from_expr,
    emitter::Resolver,
    expr::{
        expr_contains_nondeterministic_scalar_function, expr_references_any_subquery,
        expr_references_subquery_id, expression_can_fail_on_input, get_expr_affinity,
        in_compares_as_equality, walk_expr, walk_expr_mut, WalkControl,
    },
    plan::{
        outer_columns_read, plan_is_correlated, Distinctness, GroupBy, JoinInfo, JoinOrigin,
        JoinType, JoinedTable, QueryDestination, ResultSetColumn, SelectPlan, SubqueryState,
        TableReferences, WhereTerm, WhereTermOrigin,
    },
};
use crate::util::exprs_are_equivalent;
use crate::{LimboError, Result};

/// How far a pass unnests (PL.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The fixed patterns alone, as before PL.10: the planner plans this form beside the general
    /// one, so a query the patterns served keeps its plan when the general form costs more.
    Patterns,
    /// The patterns, and the general forms where the patterns refuse.
    General,
}

/// What a pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Rewrites {
    /// A subquery was unnested.
    pub any: bool,
    /// A subquery was unnested by a general form, one the patterns refuse.
    pub general: bool,
}

/// Which rewrite a worklist entry tries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// An `EXISTS` or `NOT EXISTS` conjunct of `WHERE`, as a semi- or anti-join.
    Join,
    /// The rest: `IN`, `NOT IN`, a one-value aggregate, and an `EXISTS` no join took (General).
    Value,
}

/// Unnest the correlated subqueries of `plan` as far as `reach` goes, and say what changed.
///
/// The subqueries are taken from a worklist in this order: every correlated `EXISTS` first, in the
/// order written, then the rest and each `EXISTS` no semi- or anti-join took (the order the patterns
/// have always been tried in, so [`Reach::Patterns`] gives the plan it gave before PL.10). A
/// subquery spliced in (General) brings its own correlated subqueries into `plan`, and they join
/// the end of the list: the next level down, after this one (top-down). A derived table keeps its
/// own. So each entry is a correlated subquery nested in `plan`'s subqueries, each taken at most
/// once a step, and the loop is bounded by twice their number (CLAUDE.md §2, §11).
pub(crate) fn rewrite_correlated_subqueries(
    plan: &mut SelectPlan,
    resolver: &Resolver<'_>,
    reach: Reach,
) -> Result<Rewrites> {
    let mut rewrites = Rewrites::default();
    if plan.table_references.has_right_or_full_join() {
        // SQLite keeps correlated subqueries inside the shared join body. A
        // later semi-join cannot take part in the unmatched-right scan.
        return Ok(rewrites);
    }
    let is_exists =
        |query_type: &ast::SubqueryType| matches!(query_type, ast::SubqueryType::Exists { .. });
    let correlated = || {
        plan.non_from_clause_subqueries
            .iter()
            .filter(|s| s.correlated)
    };
    let mut worklist: VecDeque<(TableInternalId, Step)> = correlated()
        .filter(|s| is_exists(&s.query_type))
        .map(|s| (s.internal_id, Step::Join))
        .chain(
            correlated()
                .filter(|s| !is_exists(&s.query_type))
                .map(|s| (s.internal_id, Step::Value)),
        )
        .collect();
    let mut seen: Vec<TableInternalId> = worklist.iter().map(|(id, _)| *id).collect();
    // Saturating: a bound past usize::MAX is one no statement's subqueries reach.
    let mut budget = correlated_subqueries_within(plan).saturating_mul(2);

    let mut aggregate_replacements = HashMap::default();
    while let Some((subquery_id, step)) = worklist.pop_front() {
        let Some(left) = budget.checked_sub(1) else {
            break;
        };
        budget = left;
        let Some(subquery_index) = plan
            .non_from_clause_subqueries
            .iter()
            .position(|s| s.internal_id == subquery_id)
        else {
            continue;
        };
        let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
            continue;
        };
        if reads_semi_or_anti_joined_table(plan, subquery_index) {
            // It stays nested: evaluated in its table's loop, before that join's early out.
            continue;
        }
        let query_type = subquery.query_type.clone();
        let same_query = subquery.same_query;
        match (step, query_type) {
            (Step::Join, _) => {
                if try_rewrite_exists(plan, subquery_index, resolver, Reach::Patterns)? {
                    rewrites.any = true;
                    queue_new_subqueries(plan, &mut worklist, &mut seen);
                    continue;
                }
                if reach == Reach::Patterns {
                    continue;
                }
                if try_rewrite_exists(plan, subquery_index, resolver, Reach::General)? {
                    rewrites = Rewrites {
                        any: true,
                        general: true,
                    };
                    queue_new_subqueries(plan, &mut worklist, &mut seen);
                    continue;
                }
                if let Some(term) = find_exists_in_where(&plan.where_clause, subquery_id) {
                    let join_type = if term.negated {
                        JoinType::Anti
                    } else {
                        JoinType::Semi
                    };
                    if rewrite_through_derived_table(
                        plan,
                        subquery_index,
                        term.index,
                        join_type,
                        None,
                        resolver,
                    )? {
                        rewrites = Rewrites {
                            any: true,
                            general: true,
                        };
                        continue;
                    }
                }
                worklist.push_back((subquery_id, Step::Value));
            }
            (Step::Value, ast::SubqueryType::Exists { .. }) => {
                if reach == Reach::General
                    && try_rewrite_exists_as_grouped_join(plan, subquery_index, resolver)?
                {
                    rewrites = Rewrites {
                        any: true,
                        general: true,
                    };
                }
            }
            (Step::Value, ast::SubqueryType::In { .. }) => {
                match try_rewrite_in(plan, subquery_index, resolver, reach)? {
                    Some(Reach::Patterns) => rewrites.any = true,
                    Some(Reach::General) => {
                        rewrites = Rewrites {
                            any: true,
                            general: true,
                        }
                    }
                    None => continue,
                }
                queue_new_subqueries(plan, &mut worklist, &mut seen);
            }
            (Step::Value, ast::SubqueryType::RowValue { num_regs: 1, .. }) => {
                if let Some(replacement) = same_query
                    .and_then(|same_query| aggregate_replacements.get(&same_query))
                    .cloned()
                {
                    if replace_subquery_result(plan, subquery_id, &replacement, Answer::Value)? {
                        plan.non_from_clause_subqueries.remove(subquery_index);
                        rewrites.any = true;
                        continue;
                    }
                }
                if let Some(rewrite) =
                    try_rewrite_single_value_aggregate(plan, subquery_index, resolver)?
                {
                    if let AggregateRewrite::GroupedTable(replacement) = rewrite {
                        aggregate_replacements.insert(subquery_id, replacement);
                    }
                    rewrites.any = true;
                }
            }
            (Step::Value, ast::SubqueryType::RowValue { .. }) => {}
        }
    }
    Ok(rewrites)
}

/// Whether the subquery at `subquery_index` reads a table of `plan` that a semi- or anti-join
/// brought in: a subquery the general reach spliced in leaves its own correlated subqueries reading
/// that table. The semi- and anti-join loops stop after a match in their one table (the early out
/// of `main_loop/close.rs` runs after the whole body), so a join hung off that table would be a
/// joined inner query those loops don't run: T-PL.10.1 found `EXISTS` in `EXISTS` keeping 24 of
/// 46 customers that way. Such a subquery stays nested.
fn reads_semi_or_anti_joined_table(plan: &SelectPlan, subquery_index: usize) -> bool {
    let Some(inner) = select_subquery_plan(plan, subquery_index) else {
        return false;
    };
    inner
        .table_references
        .outer_query_refs()
        .iter()
        .filter(|outer| outer.is_used())
        .any(|outer| {
            plan.table_references
                .find_joined_table_by_internal_id(outer.internal_id)
                .and_then(|table| table.join_info.as_ref())
                .is_some_and(|join_info| join_info.is_semi_or_anti())
        })
}

/// Queue each correlated subquery of `plan` the worklist hasn't seen: those a splice brought in.
fn queue_new_subqueries(
    plan: &SelectPlan,
    worklist: &mut VecDeque<(TableInternalId, Step)>,
    seen: &mut Vec<TableInternalId>,
) {
    for subquery in plan
        .non_from_clause_subqueries
        .iter()
        .filter(|s| s.correlated)
    {
        if seen.contains(&subquery.internal_id) {
            continue;
        }
        seen.push(subquery.internal_id);
        let step = if matches!(subquery.query_type, ast::SubqueryType::Exists { .. }) {
            Step::Join
        } else {
            Step::Value
        };
        worklist.push_back((subquery.internal_id, step));
    }
}

/// The correlated subqueries nested in `plan`'s own subqueries (FROM subqueries are planned
/// apart), `plan`'s included, counted from a stack (CLAUDE.md §11).
fn correlated_subqueries_within(plan: &SelectPlan) -> usize {
    let mut count: usize = 0;
    let mut selects: Vec<&SelectPlan> = vec![plan];
    while let Some(select) = selects.pop() {
        for subquery in &select.non_from_clause_subqueries {
            if subquery.correlated {
                // Saturating: a count past usize::MAX is a bound no loop reaches.
                count = count.saturating_add(1);
            }
            if let SubqueryState::Unevaluated {
                plan: Some(inner), ..
            } = &subquery.state
            {
                if let Plan::Select(inner) = inner.as_ref() {
                    selects.push(inner);
                }
            }
        }
    }
    count
}

/// Whether the SELECTs nested in `plan`'s subqueries and FROM subqueries (not `plan` itself) may
/// run where an unnested form runs them: none reads a table of `outside`, and none holds an
/// expression that can fail or change between calls, since the unnested form may run them on rows,
/// or in an order, the nested one didn't. Walked from a stack, each nested plan once (§11).
fn nested_selects_allow(
    plan: &SelectPlan,
    outside: &[TableInternalId],
    resolver: &Resolver<'_>,
) -> Result<bool> {
    let mut plans: Vec<&Plan> = Vec::new();
    let mut selects: Vec<&SelectPlan> = Vec::new();
    fn push_children<'a>(select: &'a SelectPlan, plans: &mut Vec<&'a Plan>) {
        for subquery in &select.non_from_clause_subqueries {
            if let SubqueryState::Unevaluated {
                plan: Some(inner), ..
            } = &subquery.state
            {
                plans.push(inner.as_ref());
            }
        }
        for joined in select.table_references.joined_tables() {
            if let Table::FromClauseSubquery(subquery) = &joined.table {
                plans.push(subquery.plan.as_ref());
            }
        }
    }
    push_children(plan, &mut plans);
    while let Some(next) = plans.pop() {
        match next {
            Plan::Select(select) => selects.push(select),
            Plan::CompoundSelect {
                left, right_most, ..
            } => {
                selects.extend(left.iter().map(|(select, _)| select));
                selects.push(right_most);
            }
            Plan::RecursiveCte(recursive) => {
                plans.push(&recursive.initial_query);
                plans.push(&recursive.recursive_query);
            }
            Plan::Delete(_) | Plan::Update(_) => return Ok(false),
        }
        while let Some(select) = selects.pop() {
            if select
                .table_references
                .outer_query_refs()
                .iter()
                .any(|outer| outer.is_used() && outside.contains(&outer.internal_id))
            {
                return Ok(false);
            }
            let group = select.group_by.as_ref();
            let exprs = select
                .where_clause
                .iter()
                .map(|term| &term.expr)
                .chain(select.result_columns.iter().map(|column| &column.expr))
                .chain(
                    select
                        .aggregates
                        .iter()
                        .flat_map(|aggregate| aggregate.args.iter().chain(&aggregate.filter_expr)),
                )
                .chain(group.into_iter().flat_map(|group| group.exprs.iter()))
                .chain(
                    group
                        .and_then(|group| group.having.as_ref())
                        .into_iter()
                        .flatten(),
                )
                .chain(select.order_by.iter().map(|(expr, _, _)| expr.as_ref()));
            for expr in exprs {
                if expression_can_fail_on_input(expr)
                    || expr_contains_nondeterministic_scalar_function(expr, resolver)?
                {
                    return Ok(false);
                }
            }
            push_children(select, &mut plans);
        }
    }
    Ok(true)
}

/// Return the SELECT plan for a subquery that has not run yet.
fn select_subquery_plan(plan: &SelectPlan, subquery_index: usize) -> Option<&SelectPlan> {
    let subquery = plan.non_from_clause_subqueries.get(subquery_index)?;
    let SubqueryState::Unevaluated { plan: inner } = &subquery.state else {
        return None;
    };
    let Plan::Select(inner) = inner.as_ref()?.as_ref() else {
        return None;
    };
    Some(inner)
}

/// Take a SELECT plan after all rewrite checks have passed. The checks found it with
/// [`select_subquery_plan`], so its absence is an internal error, before anything changed.
fn take_select_subquery_plan(
    plan: &mut SelectPlan,
    subquery_index: usize,
) -> Result<Box<SelectPlan>> {
    let missing = || LimboError::InternalError("an unnested subquery has no SELECT plan".into());
    let subquery = plan
        .non_from_clause_subqueries
        .get_mut(subquery_index)
        .ok_or_else(missing)?;
    let SubqueryState::Unevaluated { plan: inner } = &mut subquery.state else {
        return Err(missing());
    };
    if !matches!(inner.as_deref(), Some(Plan::Select(_))) {
        return Err(missing());
    }
    match inner.take().map(|inner| *inner) {
        Some(Plan::Select(inner)) => Ok(inner),
        _ => Err(missing()),
    }
}

/// Try to change one `EXISTS` or `NOT EXISTS` into a join.
fn try_rewrite_exists(
    plan: &mut SelectPlan,
    subquery_index: usize,
    resolver: &Resolver<'_>,
    reach: Reach,
) -> Result<bool> {
    let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
        return Ok(false);
    };
    let subquery_id = subquery.internal_id;

    let Some(term) = find_exists_in_where(&plan.where_clause, subquery_id) else {
        return Ok(false);
    };
    if plan.table_references.joined_tables().is_empty() {
        return Ok(false);
    }
    if select_subquery_plan(plan, subquery_index).is_none() {
        return Ok(false);
    }

    let join_type = if term.negated {
        JoinType::Anti
    } else {
        JoinType::Semi
    };

    rewrite_as_semi_or_anti_join(
        plan,
        subquery_index,
        term.index,
        join_type,
        None,
        resolver,
        reach,
    )
}

/// Turn a direct `IN` test into a semi-join, and (General) a direct `NOT IN` test into an
/// anti-join, by splicing the subquery's one table in or through a derived table. Say which reach
/// the rewrite needed, or `None` when the test stays a subquery.
fn try_rewrite_in(
    plan: &mut SelectPlan,
    subquery_index: usize,
    resolver: &Resolver<'_>,
    reach: Reach,
) -> Result<Option<Reach>> {
    let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
        return Ok(None);
    };
    let subquery_id = subquery.internal_id;
    let Some(InTerm {
        index: where_term_index,
        left,
        not_in,
    }) = find_in_term(&plan.where_clause, subquery_id)
    else {
        return Ok(None);
    };
    if not_in && reach == Reach::Patterns {
        return Ok(None);
    }
    if plan.table_references.joined_tables().is_empty() {
        return Ok(None);
    }
    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(None);
    };
    let Some(right) = inner_plan
        .result_columns
        .first()
        .map(|column| column.expr.clone())
    else {
        return Ok(None);
    };
    // A semi-join changes which expressions run and how many times they run:
    //
    // - IN runs the inner query to the end. For example,
    //   `1 IN (SELECT json_extract(value, '$') FROM t)` must fail when any row
    //   of `t` holds invalid JSON, even a row after the match. A semi-join
    //   stops at the first match, so it would skip the invalid row and hide
    //   the error.
    // - IN evaluates its left side once per outer row. A semi-join uses the
    //   left side as a join condition, so it can evaluate the left side once
    //   for every inner row it scans.
    //
    // So: keep IN as a subquery when its left side, its result expression, or
    // any inner WHERE expression can fail. JSON is only one example; the same
    // rule applies to any function or operator that can return an error.
    if inner_plan.result_columns.len() != 1
        || expression_can_fail_on_input(&left)
        || expression_can_fail_on_input(&right)
    {
        return Ok(None);
    }
    // Amphora: the semi-join compares by `=`, which may convert otherwise than IN: an integer probe
    // of a lone REAL column IN makes a REAL, `=` compares exactly. Where they differ, IN stays.
    if !in_compares_as_equality(
        get_expr_affinity(&left, Some(&plan.table_references), None),
        get_expr_affinity(&right, Some(&inner_plan.table_references), None),
    ) {
        return Ok(None);
    }
    let test = InTest {
        left_nonnull: column_is_nonnull(&left, &plan.table_references),
        right_nonnull: column_is_nonnull(&right, &inner_plan.table_references),
        left,
        not_in,
    };
    let join_type = if not_in {
        JoinType::Anti
    } else {
        JoinType::Semi
    };
    let extra_term = WhereTerm {
        expr: test.join_condition(right),
        origin: WhereTermOrigin::Where,
        consumed: false,
    };
    // NOT IN needs the general reach; IN tries the patterns' splice first.
    let reaches: &[Reach] = match (not_in, reach) {
        (false, Reach::Patterns) => &[Reach::Patterns],
        (false, Reach::General) => &[Reach::Patterns, Reach::General],
        (true, _) => &[Reach::General],
    };
    for &splice_reach in reaches {
        if rewrite_as_semi_or_anti_join(
            plan,
            subquery_index,
            where_term_index,
            join_type,
            Some(extra_term.clone()),
            resolver,
            splice_reach,
        )? {
            return Ok(Some(splice_reach));
        }
    }
    if reach == Reach::General
        && rewrite_through_derived_table(
            plan,
            subquery_index,
            where_term_index,
            join_type,
            Some(test),
            resolver,
        )?
    {
        return Ok(Some(Reach::General));
    }
    Ok(None)
}

/// The `IN` or `NOT IN` test a join takes over.
struct InTest {
    /// The test's left side, read in the outer query.
    left: Expr,
    /// Whether the test is `NOT IN`.
    not_in: bool,
    /// The left side is a column that can't be NULL.
    left_nonnull: bool,
    /// The subquery's value is a column that can't be NULL.
    right_nonnull: bool,
}

impl InTest {
    /// The join's condition for one subquery value `right`: `left = right` for `IN`. For `NOT IN`,
    /// whose row passes only when no value equals it and neither it nor a value is NULL (SQLite's
    /// lang_expr.html "The IN and NOT IN operators" table; in `WHERE` a NULL test fails the row),
    /// the anti-join's condition is any value that keeps the test from being TRUE:
    /// `left = right OR left IS NULL OR right IS NULL`, less the parts a NOT NULL column can't meet.
    /// An empty subquery still passes every row, a NULL left side included, as `NOT IN ()` does.
    fn join_condition(&self, right: Expr) -> Expr {
        let equal = Expr::Binary(
            Box::new(self.left.clone()),
            ast::Operator::Equals,
            Box::new(right.clone()),
        );
        if !self.not_in {
            return equal;
        }
        let mut condition = equal;
        let null_sides = [
            (self.left_nonnull, self.left.clone()),
            (self.right_nonnull, right),
        ];
        for (nonnull, side) in null_sides {
            if !nonnull {
                condition = Expr::Binary(
                    Box::new(condition),
                    ast::Operator::Or,
                    Box::new(Expr::IsNull(Box::new(side))),
                );
            }
        }
        condition
    }
}

/// Whether `expr` is a column that can't be NULL where `tables` read it: declared NOT NULL and not
/// on the side of an outer join that fills it with NULL (`Optimizable::is_nonnull`). Any other
/// expression may be NULL, as far as this says.
fn column_is_nonnull(expr: &Expr, tables: &TableReferences) -> bool {
    matches!(expr, Expr::Column { .. }) && expr.is_nonnull(tables)
}

/// Move one simple subquery into the outer query as a semi-join or anti-join.
fn rewrite_as_semi_or_anti_join(
    plan: &mut SelectPlan,
    subquery_index: usize,
    where_term_index: usize,
    join_type: JoinType,
    extra_term: Option<WhereTerm>,
    resolver: &Resolver<'_>,
    reach: Reach,
) -> Result<bool> {
    // A semi join needs a left table. Keep the subquery when the outer SELECT
    // has no FROM clause.
    if plan.table_references.joined_tables().is_empty() {
        return Ok(false);
    }

    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(false);
    };
    if !can_rewrite_as_semi_join(inner_plan, resolver, reach)? {
        return Ok(false);
    }

    let outer_table_ids: Vec<TableInternalId> = inner_plan
        .table_references
        .outer_query_refs()
        .iter()
        .map(|reference| reference.internal_id)
        .collect();
    let inner_table_ids: Vec<TableInternalId> = inner_plan
        .table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();

    for term in inner_plan.where_clause.iter().chain(extra_term.iter()) {
        if !can_move_join_term(&term.expr, &outer_table_ids, &inner_table_ids)? {
            return Ok(false);
        }
    }

    // Every NOT EXISTS term must use an inner table. Moving a constant false
    // term into the outer query would reject a row that NOT EXISTS keeps.
    if join_type == JoinType::Anti {
        for term in inner_plan.where_clause.iter().chain(extra_term.iter()) {
            let tables = collect_table_refs(&term.expr)?;
            if !tables.iter().any(|table| inner_table_ids.contains(table)) {
                return Ok(false);
            }
        }
    }

    // Keep a link to a table that an outer join may fill with NULL. Moving the
    // link into a later join could remove that row too early.
    let terms = inner_plan.where_clause.iter().chain(extra_term.iter());
    if reads_table_an_outer_join_may_null(plan, terms.map(|term| &term.expr))? {
        return Ok(false);
    }

    let mut inner_plan = take_select_subquery_plan(plan, subquery_index)?;
    if let Some(extra_term) = extra_term {
        inner_plan.where_clause.push(extra_term);
    }
    let inner_tables = std::mem::take(inner_plan.table_references.joined_tables_mut());
    for (index, mut table) in inner_tables.into_iter().enumerate() {
        if index == 0 {
            table.join_info = Some(JoinInfo {
                join_type,
                using: vec![],
                no_reorder: false,
                lateral: false,
            });
        }
        plan.table_references.add_joined_table(table);
    }

    // The terms now run in the outer plan, so prior access choices do not apply.
    for mut term in inner_plan.where_clause {
        term.consumed = false;
        plan.where_clause.push(term);
    }

    for inner_subquery in inner_plan.non_from_clause_subqueries {
        // A subquery correlated with the spliced table now reads a table of this query: its
        // columns are this query's to read (as the binder marks an outer query's columns).
        if let SubqueryState::Unevaluated {
            plan: Some(subquery_plan),
        } = &inner_subquery.state
        {
            for read in outer_columns_read(subquery_plan, &plan.table_references) {
                if let Expr::Column { table, column, .. } = read {
                    if let Some(joined) = plan
                        .table_references
                        .find_joined_table_by_internal_id_mut(table)
                    {
                        joined.mark_column_used(column);
                    }
                }
            }
        }
        plan.non_from_clause_subqueries.push(inner_subquery);
    }

    // EXISTS ignores its SELECT list. Keep any parameters from that list so
    // callers can still bind them.
    if plan
        .non_from_clause_subqueries
        .get(subquery_index)
        .is_some_and(|subquery| matches!(subquery.query_type, ast::SubqueryType::Exists { .. }))
    {
        keep_parameters(
            &mut plan.phantom_params,
            inner_plan.result_columns.iter().map(|column| &column.expr),
        )?;
    }

    replace_subquery_term_with_true(&mut plan.where_clause, where_term_index)?;

    plan.non_from_clause_subqueries.remove(subquery_index);

    Ok(true)
}

/// Unnest one `EXISTS`, `NOT EXISTS`, `IN` or `NOT IN` conjunct of `WHERE` (at `where_term_index`)
/// through a derived table: the subquery with its correlated conjuncts lifted out, so that it reads
/// no outer row and runs once, joined by a semi- or anti-join on those conjuncts (Neumann and
/// Kemper's simple unnesting, "pull predicates up to eliminate correlations", for a subquery of
/// any number of tables).
///
/// ```sql
/// -- Before
/// SELECT c.id FROM c WHERE EXISTS (
///   SELECT * FROM o JOIN l ON l.o = o.id WHERE o.c = c.id AND l.qty > 5);
/// -- After
/// SELECT c.id FROM c SEMI JOIN (
///   SELECT o.c AS unnested_0 FROM o JOIN l ON l.o = o.id WHERE l.qty > 5) d
/// WHERE d.unnested_0 = c.id;
/// ```
///
/// A lifted conjunct reads the subquery's tables through the derived table's columns. It stays
/// where the subquery's operators could change which rows it sees, so the subquery must be one
/// block of tables and conditions: no `GROUP BY`, aggregate, window, `LIMIT` or `OFFSET`.
/// `DISTINCT` and `ORDER BY` change no answer of these tests and are dropped. The derived table runs
/// its conditions over every row of its tables, where the subquery ran them only on the rows the
/// outer row selects, so none may fail or change between calls (`derived_table_can_hold`).
fn rewrite_through_derived_table(
    plan: &mut SelectPlan,
    subquery_index: usize,
    where_term_index: usize,
    join_type: JoinType,
    in_test: Option<InTest>,
    resolver: &Resolver<'_>,
) -> Result<bool> {
    if plan.table_references.joined_tables().is_empty() {
        return Ok(false);
    }
    let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
        return Ok(false);
    };
    let subquery_id = subquery.internal_id;
    let is_exists = matches!(subquery.query_type, ast::SubqueryType::Exists { .. });
    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(false);
    };
    if !derived_table_can_hold(inner_plan, resolver)?
        || !nested_selects_allow(inner_plan, &tables_outside(plan), resolver)?
    {
        return Ok(false);
    }
    let outer_table_ids: Vec<TableInternalId> = plan
        .table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();
    let inner_table_ids: Vec<TableInternalId> = inner_plan
        .table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();
    // A lifted conjunct is read in this query, so it may read this query's tables only.
    if inner_plan
        .table_references
        .outer_query_refs()
        .iter()
        .any(|outer| outer.is_used() && !outer_table_ids.contains(&outer.internal_id))
    {
        return Ok(false);
    }

    let mut lifted: Vec<Expr> = Vec::new();
    let mut kept: Vec<WhereTerm> = Vec::new();
    for term in &inner_plan.where_clause {
        let tables = collect_table_refs(&term.expr)?;
        if !tables.iter().any(|table| outer_table_ids.contains(table)) {
            let mut term = term.clone();
            term.consumed = false;
            kept.push(term);
            continue;
        }
        // An outer join's condition can't leave its join; a conjunct with no inner column has no
        // derived column to join on (and under NOT EXISTS would reject what the anti-join keeps).
        if term.origin.is_outer_join()
            || expr_references_any_subquery(&term.expr)
            || !tables.iter().any(|table| inner_table_ids.contains(table))
        {
            return Ok(false);
        }
        lifted.push(term.expr.clone());
    }
    if lifted.is_empty() {
        return Ok(false);
    }
    let right = match &in_test {
        Some(_) => {
            let Some(column) = inner_plan.result_columns.first() else {
                return Ok(false);
            };
            if collect_table_refs(&column.expr)?
                .iter()
                .any(|table| outer_table_ids.contains(table))
            {
                return Ok(false);
            }
            Some(column.expr.clone())
        }
        None => None,
    };
    let left = in_test.as_ref().map(|test| &test.left);
    if reads_table_an_outer_join_may_null(plan, lifted.iter().chain(left))? {
        return Ok(false);
    }

    let mut inner_plan = take_select_subquery_plan(plan, subquery_index)?;
    // EXISTS ignores its SELECT list, and none of these tests reads ORDER BY: keep the parameters
    // they held so callers can still bind them.
    let dropped = inner_plan.order_by.iter().map(|(expr, _, _)| expr.as_ref());
    if is_exists {
        let results = inner_plan.result_columns.iter().map(|column| &column.expr);
        keep_parameters(&mut plan.phantom_params, dropped.chain(results))?;
    } else {
        keep_parameters(&mut plan.phantom_params, dropped)?;
    }

    // The derived table's columns: the IN value first, then each inner column a lifted conjunct
    // reads, once.
    let mut columns: Vec<Expr> = right.into_iter().collect();
    for expr in &mut lifted {
        walk_expr_mut(expr, &mut |expr: &mut Expr| -> Result<WalkControl> {
            let (Expr::Column { table, .. } | Expr::RowId { table, .. }) = expr else {
                return Ok(WalkControl::Continue);
            };
            if !inner_table_ids.contains(table) {
                return Ok(WalkControl::Continue);
            }
            let column = match columns
                .iter()
                .position(|existing| exprs_are_equivalent(existing, &*expr))
            {
                Some(column) => column,
                None => {
                    let column = columns.len();
                    columns.push(expr.clone());
                    column
                }
            };
            *expr = Expr::Column {
                database: None,
                table: subquery_id,
                column,
                is_rowid_alias: false,
            };
            Ok(WalkControl::SkipChildren)
        })?;
    }

    inner_plan.where_clause = kept;
    inner_plan.result_columns = columns
        .into_iter()
        .enumerate()
        .map(|(index, expr)| ResultSetColumn {
            expr,
            alias: Some(format!("unnested_{index}")),
            implicit_column_name: None,
            contains_aggregates: false,
        })
        .collect();
    inner_plan.order_by.clear();
    inner_plan.distinctness = Distinctness::NonDistinct;
    inner_plan.query_destination = QueryDestination::placeholder_for_subquery();
    inner_plan.simple_aggregate = None;
    inner_plan.input_cardinality_hint = None;
    inner_plan.estimated_output_rows = None;
    inner_plan.estimated_cost = None;
    inner_plan.table_references.clear_outer_query_refs();

    let mut derived = JoinedTable::new_subquery(
        format!("unnested_subquery_{subquery_id}"),
        *inner_plan,
        Some(JoinInfo {
            join_type,
            using: vec![],
            no_reorder: false,
            lateral: false,
        }),
        subquery_id,
    )?;
    for column in 0..derived.columns().len() {
        derived.mark_column_used(column);
    }
    plan.table_references.add_joined_table(derived);
    for expr in lifted {
        plan.where_clause.push(WhereTerm {
            expr,
            origin: WhereTermOrigin::Where,
            consumed: false,
        });
    }
    if let Some(test) = in_test {
        let value = Expr::Column {
            database: None,
            table: subquery_id,
            column: 0,
            is_rowid_alias: false,
        };
        plan.where_clause.push(WhereTerm {
            expr: test.join_condition(value),
            origin: WhereTermOrigin::Where,
            consumed: false,
        });
    }
    replace_subquery_term_with_true(&mut plan.where_clause, where_term_index)?;
    plan.non_from_clause_subqueries.remove(subquery_index);
    Ok(true)
}

/// Unnest one correlated `EXISTS` that no semi- or anti-join can take (under `OR`, in the `SELECT`
/// list, beside another test) as a mark join: a left join with the subquery grouped by the inner
/// columns its `=` conjuncts match to one outer table, one row a key, and the test replaced by
/// `key IS NOT NULL`. This is Neumann and Kemper's domain `D` inferred from the equivalent inner
/// attributes (slide 18) and the grouping pushed below the join (slide 11: `D ⋈ Γ` is
/// `Γ` over `A ∪ A(D)`), the form [`try_rewrite_single_value_aggregate`] gives an aggregate.
///
/// ```sql
/// -- Before
/// SELECT c.id FROM c WHERE c.vip OR EXISTS (SELECT * FROM o WHERE o.c = c.id AND o.total > 5);
/// -- After
/// SELECT c.id FROM c LEFT JOIN (SELECT o.c FROM o WHERE o.total > 5 GROUP BY o.c) d ON d.c = c.id
/// WHERE c.vip OR d.c IS NOT NULL;
/// ```
///
/// A matched key equals a non-NULL outer value, so it is never NULL itself; an unmatched row reads
/// NULL. The grouping runs its conditions over every key, so they must not fail or change between
/// calls, and the keys must group as the `=` compares (`column_pair_compares_the_same`).
fn try_rewrite_exists_as_grouped_join(
    plan: &mut SelectPlan,
    subquery_index: usize,
    resolver: &Resolver<'_>,
) -> Result<bool> {
    // A window plan keeps another copy of its expressions (try_rewrite_single_value_aggregate).
    if plan.window.is_some() || plan.table_references.joined_tables().is_empty() {
        return Ok(false);
    }
    let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
        return Ok(false);
    };
    let subquery_id = subquery.internal_id;
    // The test's value must be known before an outer join decides on its NULL row.
    if plan.where_clause.iter().any(|term| {
        term.origin.is_outer_join() && expr_references_subquery_id(&term.expr, subquery_id)
    }) {
        return Ok(false);
    }
    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(false);
    };
    if !derived_table_can_hold(inner_plan, resolver)?
        || !nested_selects_allow(inner_plan, &tables_outside(plan), resolver)?
    {
        return Ok(false);
    }
    let Some(outer_table_id) = single_outer_table(plan, inner_plan) else {
        return Ok(false);
    };
    let outer_table_ids = [outer_table_id];
    let inner_table_ids: Vec<TableInternalId> = inner_plan
        .table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();
    let mut pairs = Vec::new();
    let mut kept = Vec::new();
    for term in &inner_plan.where_clause {
        let tables = collect_table_refs(&term.expr)?;
        if !tables.iter().any(|table| outer_table_ids.contains(table)) {
            let mut term = term.clone();
            term.consumed = false;
            kept.push(term);
            continue;
        }
        if term.origin.is_outer_join() {
            return Ok(false);
        }
        let Some(pair) = read_column_pair(&term.expr, &outer_table_ids, &inner_table_ids) else {
            return Ok(false);
        };
        if !column_pair_compares_the_same(&pair, &inner_plan.table_references)? {
            return Ok(false);
        }
        pairs.push(pair);
    }
    if pairs.is_empty() {
        return Ok(false);
    }

    let key = Expr::Column {
        database: None,
        table: subquery_id,
        column: 0,
        is_rowid_alias: false,
    };
    let replacement = Expr::NotNull(Box::new(key));
    if !replace_subquery_result(plan, subquery_id, &replacement, Answer::Exists)? {
        return Ok(false);
    }
    let mut inner_plan = take_select_subquery_plan(plan, subquery_index)?;
    let dropped = inner_plan
        .order_by
        .iter()
        .map(|(expr, _, _)| expr.as_ref())
        .chain(inner_plan.result_columns.iter().map(|column| &column.expr));
    keep_parameters(&mut plan.phantom_params, dropped)?;

    let mut group_columns: Vec<Expr> = Vec::new();
    let mut positions = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let position = match group_columns
            .iter()
            .position(|existing| exprs_are_equivalent(existing, &pair.inner))
        {
            Some(position) => position,
            None => {
                let position = group_columns.len();
                group_columns.push(pair.inner.clone());
                position
            }
        };
        positions.push(position);
    }
    inner_plan.where_clause = kept;
    inner_plan.result_columns = group_columns
        .iter()
        .enumerate()
        .map(|(index, expr)| ResultSetColumn {
            expr: expr.clone(),
            alias: Some(format!("correlation_key_{index}")),
            implicit_column_name: None,
            contains_aggregates: false,
        })
        .collect();
    let column_count = group_columns.len();
    inner_plan.group_by = Some(GroupBy {
        exprs: group_columns,
        sort_order: vec![SortOrder::Asc; column_count],
        nulls_order: vec![None; column_count],
        sort_elided: false,
        having: None,
        carried: Vec::new(),
    });
    inner_plan.order_by.clear();
    inner_plan.distinctness = Distinctness::NonDistinct;
    inner_plan.query_destination = QueryDestination::placeholder_for_subquery();
    inner_plan.simple_aggregate = None;
    inner_plan.input_cardinality_hint = None;
    inner_plan.estimated_output_rows = None;
    inner_plan.estimated_cost = None;
    inner_plan.table_references.clear_outer_query_refs();

    let mut grouped = JoinedTable::new_subquery(
        format!("exists_subquery_{subquery_id}"),
        *inner_plan,
        Some(JoinInfo {
            join_type: JoinType::LeftOuter,
            using: vec![],
            no_reorder: false,
            lateral: false,
        }),
        subquery_id,
    )?;
    for column in 0..grouped.columns().len() {
        grouped.mark_column_used(column);
    }
    for (pair, column) in pairs.into_iter().zip(positions) {
        let key = Expr::Column {
            database: None,
            table: subquery_id,
            column,
            is_rowid_alias: false,
        };
        let (left, right) = if pair.inner_was_left {
            (key, pair.outer)
        } else {
            (pair.outer, key)
        };
        plan.where_clause.push(WhereTerm {
            expr: Expr::Binary(Box::new(left), ast::Operator::Equals, Box::new(right)),
            origin: WhereTermOrigin::Join(JoinOrigin::Outer(subquery_id)),
            consumed: false,
        });
    }
    plan.table_references.add_joined_table(grouped);
    plan.non_from_clause_subqueries.remove(subquery_index);
    Ok(true)
}

/// Whether a subquery can become a derived table that runs once over all its rows: one block of
/// tables and conditions (no grouping, aggregate, window, `VALUES`, `LIMIT` or `OFFSET`), reading
/// no virtual table and no correlated FROM subquery, and with no condition that can fail or that
/// may change between calls. Its conditions then run on rows the correlated subquery never read,
/// which must not raise an error the query didn't. Its own subqueries stay in it, unnested when
/// the planner plans it, if they read none of the outer query's tables and hold nothing that can
/// fail ([`nested_selects_allow`], with [`tables_outside`]).
fn derived_table_can_hold(plan: &SelectPlan, resolver: &Resolver<'_>) -> Result<bool> {
    if plan.table_references.joined_tables().is_empty()
        || plan.group_by.is_some()
        || !plan.aggregates.is_empty()
        || plan.window.is_some()
        || !plan.values.is_empty()
        || plan.limit.is_some()
        || plan.offset.is_some()
    {
        return Ok(false);
    }
    if plan
        .table_references
        .joined_tables()
        .iter()
        .any(|table| match &table.table {
            Table::Virtual(_) => true,
            Table::FromClauseSubquery(subquery) => plan_is_correlated(&subquery.plan),
            _ => false,
        })
    {
        return Ok(false);
    }
    for term in &plan.where_clause {
        if expression_can_fail_on_input(&term.expr)
            || expr_contains_nondeterministic_scalar_function(&term.expr, resolver)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The tables a derived table made in `plan` must not read: `plan`'s own and those it reads from
/// further out.
fn tables_outside(plan: &SelectPlan) -> Vec<TableInternalId> {
    plan.table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .chain(
            plan.table_references
                .outer_query_refs()
                .iter()
                .map(|outer| outer.internal_id),
        )
        .collect()
}

/// The one table of `plan` that `inner` reads as an outer reference, if it reads exactly one and
/// that one is `plan`'s own (not a table further out). A grouped table linked to more than one
/// outer table can move before one of those tables after its left join becomes an inner join.
fn single_outer_table(plan: &SelectPlan, inner: &SelectPlan) -> Option<TableInternalId> {
    let mut used = inner
        .table_references
        .outer_query_refs()
        .iter()
        .filter(|outer| outer.is_used());
    let outer = used.next()?;
    if used.next().is_some()
        || outer.scope_depth != 0
        || plan
            .table_references
            .find_joined_table_by_internal_id(outer.internal_id)
            .is_none()
    {
        return None;
    }
    Some(outer.internal_id)
}

/// Whether any of `exprs` reads a table of `plan` that an outer join may fill with NULL. A term
/// joined later could then remove that row before the outer join keeps it.
fn reads_table_an_outer_join_may_null<'a>(
    plan: &SelectPlan,
    exprs: impl Iterator<Item = &'a Expr>,
) -> Result<bool> {
    let may_be_null: Vec<TableInternalId> = plan
        .table_references
        .joined_tables()
        .iter()
        .filter(|table| {
            table
                .join_info
                .as_ref()
                .is_some_and(|join_info| join_info.is_outer() && !join_info.is_full_outer())
        })
        .map(|table| table.internal_id)
        .collect();
    if may_be_null.is_empty() {
        return Ok(false);
    }
    for expr in exprs {
        if collect_table_refs(expr)?
            .iter()
            .any(|table| may_be_null.contains(table))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Keep the parameters of expressions a rewrite drops, so callers can still bind them.
fn keep_parameters<'a>(
    params: &mut Vec<ast::Variable>,
    exprs: impl Iterator<Item = &'a Expr>,
) -> Result<()> {
    for expr in exprs {
        walk_expr(expr, &mut |expr: &Expr| -> Result<WalkControl> {
            if let Expr::Variable(variable) = expr {
                params.push(variable.clone());
            }
            Ok(WalkControl::Continue)
        })?;
    }
    Ok(())
}

/// Which answer of a subquery [`replace_subquery_result`] replaces.
#[derive(Clone, Copy)]
enum Answer {
    /// A one-value subquery's value.
    Value,
    /// An `EXISTS` test's truth.
    Exists,
}

/// The value returned when no input row matches.
#[derive(Clone, Copy)]
enum EmptyInputValue {
    /// SQL NULL.
    Null,
    /// Integer zero, as returned by `count`.
    IntegerZero,
    /// Real zero, as returned by `total`.
    RealZero,
}

/// The inner and outer columns used by one `=` check.
struct ColumnPair {
    /// The column read by the inner query.
    inner: Expr,
    /// The column read from the outer query.
    outer: Expr,
    /// Whether the inner column was on the left side of the `=` check.
    inner_was_left: bool,
}

/// Where the aggregate result is stored after a rewrite.
enum AggregateRewrite {
    /// The result is a column in a grouped table. Another reference to the same
    /// subquery can read that column instead of building another grouped table.
    GroupedTable(Expr),
    /// The aggregate moved into the outer query and is used by its `HAVING`
    /// clause. There is no result column for another subquery to reuse.
    Joined,
}

/// Rewrite one correlated aggregate subquery using group-first or join-first.
///
/// - Try group-first when computing the aggregate for unused keys cannot fail.
///   This form computes each key once, and a second identical subquery can
///   reuse its result.
/// - Otherwise, try join-first. This form never computes an aggregate for an
///   unused key, but it supports fewer query shapes.
/// - Both forms use a left join. The original subquery returns one value even
///   when no inner row matches, so the rewrite must keep such outer rows too.
fn try_rewrite_single_value_aggregate(
    plan: &mut SelectPlan,
    subquery_index: usize,
    resolver: &Resolver<'_>,
) -> Result<Option<AggregateRewrite>> {
    // A window plan stores another copy of its expressions. Rewriting only the
    // copy here would leave the other copy pointing at the removed subquery.
    if plan.window.is_some() {
        return Ok(None);
    }

    let Some(subquery) = plan.non_from_clause_subqueries.get(subquery_index) else {
        return Ok(None);
    };
    let subquery_id = subquery.internal_id;

    // A value used by an outer join condition must be ready before that join
    // decides whether to fill its right side with NULL values.
    if plan.where_clause.iter().any(|term| {
        term.origin.is_outer_join() && expr_references_subquery_id(&term.expr, subquery_id)
    }) {
        return Ok(None);
    }
    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(None);
    };

    if !can_rewrite_single_value_aggregate(inner_plan, resolver)? {
        return Ok(None);
    }
    let Some(empty_value) = result_on_empty_input(inner_plan) else {
        return Ok(None);
    };

    let outer_tables = inner_plan.table_references.outer_query_refs();
    if outer_tables.iter().any(|outer_table| {
        outer_table.is_used()
            && (outer_table.scope_depth != 0
                || plan
                    .table_references
                    .find_joined_table_by_internal_id(outer_table.internal_id)
                    .is_none())
    }) {
        return Ok(None);
    }
    // A grouped table linked to more than one outer table can move before one
    // of those tables after its left join becomes an inner join.
    if outer_tables
        .iter()
        .filter(|outer_table| outer_table.is_used())
        .count()
        != 1
    {
        return Ok(None);
    }
    let outer_table_ids: Vec<_> = outer_tables.iter().map(|table| table.internal_id).collect();
    let inner_table_ids: Vec<_> = inner_plan
        .table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();

    let mut pairs = Vec::new();
    let mut inner_where = Vec::new();
    for term in &inner_plan.where_clause {
        let tables = collect_table_refs(&term.expr)?;
        if !tables.iter().any(|table| outer_table_ids.contains(table)) {
            let mut term = term.clone();
            term.consumed = false;
            inner_where.push(term);
            continue;
        }
        if term.origin.is_outer_join() {
            return Ok(None);
        }
        let Some(pair) = read_column_pair(&term.expr, &outer_table_ids, &inner_table_ids) else {
            return Ok(None);
        };
        if !column_pair_compares_the_same(&pair, &inner_plan.table_references)? {
            return Ok(None);
        }
        pairs.push(pair);
    }
    if pairs.is_empty() || uses_outer_tables_outside_where(inner_plan, &outer_table_ids)? {
        return Ok(None);
    }

    if !aggregate_can_run_for_unused_rows(inner_plan) {
        return rewrite_aggregate_as_join_then_group(plan, subquery_index, subquery_id, resolver);
    }

    let mut inner_plan = *take_select_subquery_plan(plan, subquery_index)?;
    inner_plan.where_clause = inner_where;
    inner_plan.limit = None;
    inner_plan.query_destination = QueryDestination::placeholder_for_subquery();
    inner_plan.simple_aggregate = None;
    inner_plan.input_cardinality_hint = None;
    inner_plan.estimated_output_rows = None;
    inner_plan.estimated_cost = None;
    inner_plan.table_references.clear_outer_query_refs();

    let mut group_columns: Vec<Expr> = Vec::new();
    let mut result_positions = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        let column_index = match group_columns
            .iter()
            .position(|existing| exprs_are_equivalent(existing, &pair.inner))
        {
            Some(column_index) => column_index,
            None => {
                let column_index = group_columns.len();
                group_columns.push(pair.inner.clone());
                column_index
            }
        };
        // The aggregate is the grouped table's first column, the keys after it.
        let position = column_index.checked_add(1).ok_or_else(|| {
            LimboError::InternalError("too many correlation keys to number".into())
        })?;
        result_positions.push(position);
    }

    for (index, column) in group_columns.iter().enumerate() {
        inner_plan.result_columns.push(ResultSetColumn {
            expr: column.clone(),
            alias: Some(format!("correlation_key_{index}")),
            implicit_column_name: None,
            contains_aggregates: false,
        });
    }
    let column_count = group_columns.len();
    inner_plan.group_by = Some(GroupBy {
        exprs: group_columns,
        sort_order: vec![SortOrder::Asc; column_count],
        nulls_order: vec![None; column_count],
        sort_elided: false,
        having: None,
        carried: Vec::new(),
    });

    let mut grouped_table = JoinedTable::new_subquery(
        format!("scalar_subquery_{subquery_id}"),
        inner_plan,
        Some(JoinInfo {
            join_type: JoinType::LeftOuter,
            using: vec![],
            no_reorder: false,
            lateral: false,
        }),
        subquery_id,
    )?;
    // A scalar subquery result has no text order. Do not give its replacement
    // the text order of the grouped table's first result column.
    let first_column = match &mut grouped_table.table {
        Table::FromClauseSubquery(grouped_subquery) => Arc::get_mut(grouped_subquery)
            .and_then(|grouped_subquery| grouped_subquery.columns.first_mut()),
        _ => None,
    };
    let Some(first_column) = first_column else {
        return Err(LimboError::InternalError(
            "a new grouped table holds no subquery of its own".into(),
        ));
    };
    first_column.set_collation(None);
    for column in 0..grouped_table.columns().len() {
        grouped_table.mark_column_used(column);
    }

    let result_column = Expr::Column {
        database: None,
        table: subquery_id,
        column: 0,
        is_rowid_alias: false,
    };
    let replacement = match empty_value {
        EmptyInputValue::Null => result_column,
        EmptyInputValue::IntegerZero => coalesce_with_zero(result_column, "0"),
        EmptyInputValue::RealZero => coalesce_with_zero(result_column, "0.0"),
    };
    if !replace_subquery_result(plan, subquery_id, &replacement, Answer::Value)? {
        return Ok(None);
    }

    for (pair, column) in pairs.into_iter().zip(result_positions) {
        let grouped_column = Expr::Column {
            database: None,
            table: subquery_id,
            column,
            is_rowid_alias: false,
        };
        let (left, right) = if pair.inner_was_left {
            (grouped_column, pair.outer)
        } else {
            (pair.outer, grouped_column)
        };
        plan.where_clause.push(WhereTerm {
            expr: Expr::Binary(Box::new(left), ast::Operator::Equals, Box::new(right)),
            origin: WhereTermOrigin::Join(JoinOrigin::Outer(subquery_id)),
            consumed: false,
        });
    }
    plan.table_references.add_joined_table(grouped_table);
    plan.non_from_clause_subqueries.remove(subquery_index);
    Ok(Some(AggregateRewrite::GroupedTable(replacement)))
}

/// Join matching rows before computing an aggregate such as `sum`.
///
/// ```sql
/// -- Before
/// SELECT o.id FROM outer_rows o
/// WHERE o.limit > (SELECT sum(i.x) FROM inner_rows i WHERE i.key = o.key);
///
/// -- After
/// SELECT o.id FROM outer_rows o
/// LEFT JOIN inner_rows i ON i.key = o.key
/// GROUP BY o.rowid
/// HAVING o.limit > sum(i.x) FILTER (WHERE i.rowid IS NOT NULL);
/// ```
///
/// Use this form only when all of these rules hold:
///
/// - The outer query reads one B-tree table with a rowid. `GROUP BY o.rowid`
///   then makes one group for each original outer row.
/// - The inner query reads one B-tree table with a rowid. The rewrite uses
///   `i.rowid IS NOT NULL` to keep the NULL-filled row made by a left join with
///   no match out of the aggregate.
/// - The outer query has no aggregate, `GROUP BY`, `DISTINCT`, window function,
///   `ORDER BY`, `LIMIT`, `OFFSET`, or `VALUES`. The current rewrite does not
///   preserve these operations around its new grouping step.
/// - The subquery is one complete side of one top-level `WHERE` comparison.
///   That whole comparison can move to `HAVING` without leaving another use of
///   the removed subquery.
/// - Every other `WHERE` term keeps the same value each time it runs. After
///   the rewrite, an outer row appears once for each matching inner row, and
///   the other `WHERE` terms run once per copy instead of once per outer row.
///   A term such as `random() % 2 = 0` could then accept some copies and
///   reject others, and the aggregate would see only some of the row's inner
///   rows. So a term must not call a nondeterministic function and must not
///   read a correlated subquery, which runs again for each copy. Reading an
///   uncorrelated subquery is fine because it runs once and its stored result
///   is the same for every copy.
fn rewrite_aggregate_as_join_then_group(
    plan: &mut SelectPlan,
    subquery_index: usize,
    subquery_id: TableInternalId,
    resolver: &Resolver<'_>,
) -> Result<Option<AggregateRewrite>> {
    if plan.table_references.joined_tables().len() != 1
        || !plan.aggregates.is_empty()
        || plan.group_by.is_some()
        || !plan.order_by.is_empty()
        || plan.limit.is_some()
        || plan.offset.is_some()
        || !plan.values.is_empty()
        || plan.window.is_some()
        || plan.distinctness.is_distinct()
    {
        return Ok(None);
    }

    let Some(outer_table) = plan.table_references.joined_tables().first() else {
        return Ok(None);
    };
    if !matches!(&outer_table.table, Table::BTree(table) if table.has_rowid) {
        return Ok(None);
    }
    let outer_table_id = outer_table.internal_id;

    let Some(inner_plan) = select_subquery_plan(plan, subquery_index) else {
        return Ok(None);
    };
    if inner_plan.table_references.joined_tables().len() != 1
        || !matches!(
            inner_plan.table_references.joined_tables().first().map(|table| &table.table),
            Some(Table::BTree(table)) if table.has_rowid
        )
    {
        return Ok(None);
    }

    let Some(result) = inner_plan.result_columns.first() else {
        return Ok(None);
    };
    let result = result.expr.clone();
    let Some((where_index, having)) =
        find_direct_aggregate_comparison(plan, subquery_id, &result, resolver)?
    else {
        return Ok(None);
    };

    let mut inner_plan = *take_select_subquery_plan(plan, subquery_index)?;
    let mut inner_tables = std::mem::take(inner_plan.table_references.joined_tables_mut());
    let mut inner_table = inner_tables.pop().ok_or_else(|| {
        LimboError::InternalError("a checked aggregate subquery has no table".into())
    })?;
    let inner_table_id = inner_table.internal_id;
    inner_table.join_info = Some(JoinInfo {
        join_type: JoinType::LeftOuter,
        using: vec![],
        no_reorder: false,
        lateral: false,
    });
    plan.table_references.add_joined_table(inner_table);

    for mut term in inner_plan.where_clause {
        term.origin = match term.origin {
            WhereTermOrigin::TableFunction(_) => {
                WhereTermOrigin::TableFunction(JoinOrigin::Outer(inner_table_id))
            }
            WhereTermOrigin::Where | WhereTermOrigin::Join(_) => {
                WhereTermOrigin::Join(JoinOrigin::Outer(inner_table_id))
            }
        };
        term.consumed = false;
        plan.where_clause.push(term);
    }

    let inner_row_exists = Expr::NotNull(Box::new(Expr::RowId {
        database: None,
        table: inner_table_id,
    }));
    for aggregate in &mut inner_plan.aggregates {
        aggregate.filter_expr = Some(match aggregate.filter_expr.take() {
            Some(filter) => Expr::Binary(
                Box::new(inner_row_exists.clone()),
                ast::Operator::And,
                Box::new(filter),
            ),
            None => inner_row_exists.clone(),
        });
    }
    plan.aggregates.extend(inner_plan.aggregates);
    plan.where_clause.remove(where_index);
    plan.group_by = Some(GroupBy {
        exprs: vec![Expr::RowId {
            database: None,
            table: outer_table_id,
        }],
        sort_order: vec![SortOrder::Asc],
        nulls_order: vec![None],
        sort_elided: false,
        having: Some(vec![having]),
        carried: Vec::new(),
    });
    plan.non_from_clause_subqueries.remove(subquery_index);
    Ok(Some(AggregateRewrite::Joined))
}

/// Find one comparison that can move from `WHERE` to `HAVING`.
///
/// This form is accepted:
///
/// ```sql
/// WHERE o.limit > (SELECT sum(i.x) FROM inner_rows i WHERE i.key = o.key)
/// ```
///
/// These forms are rejected:
///
/// - `SELECT (SELECT sum(...))`: the value is used outside `WHERE`.
/// - `WHERE o.limit > 1 + (SELECT sum(...))`: the subquery is inside a larger
///   expression.
/// - `WHERE (SELECT sum(...)) > (SELECT max(...))`: moving one subquery would
///   leave another subquery in the new `HAVING` clause.
/// - The same subquery is used by two `WHERE` terms.
/// - The comparison came from an outer join condition. Moving it to `HAVING`
///   would change which rows the outer join fills with NULL values.
/// - Another `WHERE` term calls a nondeterministic function or reads a
///   correlated subquery. After the rewrite these terms run once per joined
///   copy of an outer row, so their value must be the same for every copy.
///
/// The rewrite removes the subquery, so every use of its value must be handled
/// here. This function handles only one complete comparison.
fn find_direct_aggregate_comparison(
    plan: &SelectPlan,
    subquery_id: TableInternalId,
    replacement: &Expr,
    resolver: &Resolver<'_>,
) -> Result<Option<(usize, Expr)>> {
    if plan
        .result_columns
        .iter()
        .any(|column| expr_references_subquery_id(&column.expr, subquery_id))
    {
        return Ok(None);
    }

    let mut found = None;
    for (index, term) in plan.where_clause.iter().enumerate() {
        if !expr_references_subquery_id(&term.expr, subquery_id) {
            if expr_contains_nondeterministic_scalar_function(&term.expr, resolver)?
                || expr_references_correlated_subquery(&term.expr, plan)
            {
                return Ok(None);
            }
            continue;
        }
        if found.is_some() || term.origin.is_outer_join() {
            return Ok(None);
        }
        let Expr::Binary(left, operator, right) = &term.expr else {
            return Ok(None);
        };
        if !operator.is_comparison() {
            return Ok(None);
        }

        let is_subquery = |expr: &Expr| {
            matches!(
                expr,
                Expr::SubqueryResult {
                    subquery_id: id,
                    query_type: ast::SubqueryType::RowValue { num_regs: 1, .. },
                    ..
                } if *id == subquery_id
            )
        };
        let expr = if is_subquery(left) && !expr_references_any_subquery(right) {
            Expr::Binary(Box::new(replacement.clone()), *operator, right.clone())
        } else if is_subquery(right) && !expr_references_any_subquery(left) {
            Expr::Binary(left.clone(), *operator, Box::new(replacement.clone()))
        } else {
            return Ok(None);
        };
        found = Some((index, expr));
    }
    Ok(found)
}

/// Return whether an expression reads the result of a correlated subquery.
fn expr_references_correlated_subquery(expr: &Expr, plan: &SelectPlan) -> bool {
    plan.non_from_clause_subqueries
        .iter()
        .filter(|subquery| subquery.correlated)
        .any(|subquery| expr_references_subquery_id(expr, subquery.internal_id))
}

/// Check whether an aggregate subquery uses a form that both rewrites support.
///
/// For example, this is supported:
///
/// ```sql
/// SELECT avg(i.value) FROM inner_rows i WHERE i.key = o.key
/// ```
///
/// The shared rules are:
///
/// - The subquery returns one expression, contains an aggregate, and reads at
///   least one table.
/// - It has no `GROUP BY`, `ORDER BY`, window function, `DISTINCT`, or `VALUES`.
///   The two rewrites change the grouping and the location of the inner tables.
///   Each of these operations needs its own rule to keep the same result, and
///   those rules are not implemented.
/// - It has no nested subquery. A nested subquery has a separate plan, and this
///   rewrite does not move that plan with the inner tables.
/// - Its only limit is `LIMIT 1`. Planning a scalar subquery adds this limit.
///   `LIMIT 0` removes the result row, and `OFFSET 1` skips it.
/// - It reads no virtual table. For example, the hidden columns of
///   `generate_series(o.first, o.last)` are function arguments. They cannot be
///   moved like normal `WHERE` checks.
/// - It reads no correlated subquery in `FROM`. Such a subquery still needs
///   values from the current outer row and needs a separate rewrite.
/// - Its result and `WHERE` clause contain no function such as `random()` whose
///   result can change between calls. A rewrite can change the number of calls.
fn can_rewrite_single_value_aggregate(plan: &SelectPlan, resolver: &Resolver<'_>) -> Result<bool> {
    if plan.result_columns.len() != 1
        || plan.aggregates.is_empty()
        || plan.table_references.joined_tables().is_empty()
        || plan.group_by.is_some()
        || !plan.order_by.is_empty()
        || plan.offset.is_some()
        || !plan.values.is_empty()
        || plan.window.is_some()
        || !matches!(plan.distinctness, Distinctness::NonDistinct)
        || !plan.non_from_clause_subqueries.is_empty()
    {
        return Ok(false);
    }
    if !matches!(
        plan.limit.as_deref(),
        Some(Expr::Literal(ast::Literal::Numeric(value))) if value.parse::<i64>() == Ok(1)
    ) {
        return Ok(false);
    }
    if plan
        .table_references
        .joined_tables()
        .iter()
        .any(|table| match &table.table {
            crate::schema::Table::Virtual(_) => true,
            crate::schema::Table::FromClauseSubquery(subquery) => {
                plan_is_correlated(&subquery.plan)
            }
            _ => false,
        })
    {
        return Ok(false);
    }
    for column in &plan.result_columns {
        if expr_contains_nondeterministic_scalar_function(&column.expr, resolver)? {
            return Ok(false);
        }
    }
    for term in &plan.where_clause {
        if expr_contains_nondeterministic_scalar_function(&term.expr, resolver)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Return whether group-first may compute the aggregate for every key,
/// including keys that no outer row asks for.
///
/// Suppose the outer query asks only for key 1, while the inner table also has
/// key 99. Group-first computes the aggregate for both keys. This function
/// returns true only when:
///
/// - every aggregate is `avg`, `count`, `min`, `max`, or `total`; and
/// - no aggregate input, aggregate `FILTER`, or inner `WHERE` expression can
///   fail for data stored under unused key 99.
///
/// `sum` can overflow. String aggregates can grow too large. Any aggregate not
/// listed above also returns false because this code has not proved that it is
/// safe for unused keys. The caller then tries join-first, which computes only
/// keys requested by outer rows. If join-first does not support this kind of
/// query, the correlated subquery stays unchanged.
fn aggregate_can_run_for_unused_rows(plan: &SelectPlan) -> bool {
    if !plan.aggregates.iter().all(|aggregate| {
        matches!(
            aggregate.func,
            AggFunc::Avg
                | AggFunc::Count
                | AggFunc::Count0
                | AggFunc::Max
                | AggFunc::Min
                | AggFunc::Total
        )
    }) {
        return false;
    }

    let aggregate_expressions = plan
        .aggregates
        .iter()
        .flat_map(|aggregate| aggregate.args.iter().chain(aggregate.filter_expr.iter()));
    let filter_expressions = plan.where_clause.iter().map(|term| &term.expr);
    !aggregate_expressions
        .chain(filter_expressions)
        .any(expression_can_fail_on_input)
}

/// Return the result for no input rows, if it is known.
fn result_on_empty_input(plan: &SelectPlan) -> Option<EmptyInputValue> {
    let expr = &plan.result_columns.first()?.expr;
    for aggregate in &plan.aggregates {
        if !exprs_are_equivalent(expr, &aggregate.original_expr) {
            continue;
        }
        return match aggregate.func {
            AggFunc::Count | AggFunc::Count0 => Some(EmptyInputValue::IntegerZero),
            AggFunc::Total => Some(EmptyInputValue::RealZero),
            AggFunc::Avg
            | AggFunc::GroupConcat
            | AggFunc::Max
            | AggFunc::Min
            | AggFunc::StringAgg
            | AggFunc::Sum => Some(EmptyInputValue::Null),
            _ => None,
        };
    }
    is_null_on_empty_input(expr, &plan.aggregates).then_some(EmptyInputValue::Null)
}

/// Return whether an expression must be NULL when its input is empty: it is, or passes through
/// arithmetic, a unary operator, a cast, a collation or parentheses, an aggregate that is NULL over
/// no rows. Walked from a stack, each node of the expression once (CLAUDE.md §11).
fn is_null_on_empty_input(expr: &Expr, aggregates: &[crate::translate::plan::Aggregate]) -> bool {
    let mut pending: SmallVec<[&Expr; 4]> = SmallVec::new();
    pending.push(expr);
    while let Some(expr) = pending.pop() {
        if aggregates.iter().any(|aggregate| {
            exprs_are_equivalent(expr, &aggregate.original_expr)
                && matches!(
                    aggregate.func,
                    AggFunc::Avg
                        | AggFunc::GroupConcat
                        | AggFunc::Max
                        | AggFunc::Min
                        | AggFunc::StringAgg
                        | AggFunc::Sum
                )
        }) {
            return true;
        }
        match expr {
            Expr::Binary(
                left,
                ast::Operator::Add
                | ast::Operator::Subtract
                | ast::Operator::Multiply
                | ast::Operator::Divide
                | ast::Operator::Modulus
                | ast::Operator::BitwiseAnd
                | ast::Operator::BitwiseOr
                | ast::Operator::LeftShift
                | ast::Operator::RightShift
                | ast::Operator::Concat,
                right,
            ) => {
                pending.push(right);
                pending.push(left);
            }
            Expr::Unary(_, inner) | Expr::Cast { expr: inner, .. } | Expr::Collate(inner, _) => {
                pending.push(inner);
            }
            Expr::Parenthesized(exprs) if exprs.len() == 1 => {
                if let Some(inner) = exprs.first() {
                    pending.push(inner);
                }
            }
            _ => {}
        }
    }
    false
}

/// Read an `=` check between one inner column and one outer column.
fn read_column_pair(
    expr: &Expr,
    outer_table_ids: &[TableInternalId],
    inner_table_ids: &[TableInternalId],
) -> Option<ColumnPair> {
    let Expr::Binary(left, ast::Operator::Equals, right) = expr else {
        return None;
    };
    match (left.as_ref(), right.as_ref()) {
        (
            inner @ Expr::Column {
                table: inner_table, ..
            },
            outer @ Expr::Column {
                table: outer_table, ..
            },
        ) if inner_table_ids.contains(inner_table) && outer_table_ids.contains(outer_table) => {
            Some(ColumnPair {
                inner: inner.clone(),
                outer: outer.clone(),
                inner_was_left: true,
            })
        }
        (
            outer @ Expr::Column {
                table: outer_table, ..
            },
            inner @ Expr::Column {
                table: inner_table, ..
            },
        ) if outer_table_ids.contains(outer_table) && inner_table_ids.contains(inner_table) => {
            Some(ColumnPair {
                inner: inner.clone(),
                outer: outer.clone(),
                inner_was_left: false,
            })
        }
        _ => None,
    }
}

/// Return whether grouping and joining use the same comparison rules.
///
/// For example, an inner `BINARY` key can put `A` and `a` in two groups, while
/// an outer `NOCASE` key can join to both groups. An inner key can also put the
/// number `1` and the text `'1'` in two groups, while a numeric outer key joins
/// to both. Either case would return the outer row twice. Use the grouped form
/// only when both columns use the same number/text conversion rule and the same
/// text order.
fn column_pair_compares_the_same(pair: &ColumnPair, tables: &TableReferences) -> Result<bool> {
    if get_expr_affinity(&pair.inner, Some(tables), None)
        != get_expr_affinity(&pair.outer, Some(tables), None)
    {
        return Ok(false);
    }

    let inner_collation = get_collseq_from_expr(&pair.inner, tables)?.unwrap_or_default();
    let outer_collation = get_collseq_from_expr(&pair.outer, tables)?.unwrap_or_default();
    Ok(inner_collation == outer_collation)
}

/// Return whether the plan uses an outer table outside its `WHERE` clause.
fn uses_outer_tables_outside_where(
    plan: &SelectPlan,
    outer_table_ids: &[TableInternalId],
) -> Result<bool> {
    let group = plan.group_by.as_ref();
    let exprs = plan
        .result_columns
        .iter()
        .map(|column| &column.expr)
        .chain(plan.order_by.iter().map(|(expr, _, _)| expr.as_ref()))
        .chain(plan.limit.as_deref())
        .chain(plan.offset.as_deref())
        .chain(group.into_iter().flat_map(|group| group.exprs.iter()))
        .chain(
            group
                .and_then(|group| group.having.as_ref())
                .into_iter()
                .flatten(),
        );
    for expr in exprs {
        if collect_table_refs(expr)?
            .iter()
            .any(|table| outer_table_ids.contains(table))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Keep the zero that `count` and `total` return for an empty input.
fn coalesce_with_zero(value: Expr, zero: &str) -> Expr {
    Expr::FunctionCall {
        name: Name::exact("coalesce".to_string()),
        distinctness: None,
        args: vec![
            Box::new(value),
            Box::new(Expr::Literal(ast::Literal::Numeric(zero.to_string()))),
        ],
        order_by: vec![],
        within_group: vec![],
        filter_over: FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

/// Replace each use of one subquery value with a joined column.
fn replace_subquery_result(
    plan: &mut SelectPlan,
    subquery_id: TableInternalId,
    replacement: &Expr,
    answer: Answer,
) -> Result<bool> {
    let mut found = false;
    let mut replace = |expr: &mut Expr| -> Result<WalkControl> {
        let Expr::SubqueryResult {
            subquery_id: id,
            query_type,
            lhs,
            ..
        } = expr
        else {
            return Ok(WalkControl::Continue);
        };
        let answers = match answer {
            Answer::Value => matches!(query_type, ast::SubqueryType::RowValue { num_regs: 1, .. }),
            Answer::Exists => {
                lhs.is_none() && matches!(query_type, ast::SubqueryType::Exists { .. })
            }
        };
        if *id == subquery_id && answers {
            *expr = replacement.clone();
            found = true;
            return Ok(WalkControl::SkipChildren);
        }
        Ok(WalkControl::Continue)
    };

    for column in &mut plan.result_columns {
        walk_expr_mut(&mut column.expr, &mut replace)?;
    }
    for term in &mut plan.where_clause {
        walk_expr_mut(&mut term.expr, &mut replace)?;
    }
    if let Some(group) = &mut plan.group_by {
        for expr in &mut group.exprs {
            walk_expr_mut(expr, &mut replace)?;
        }
        if let Some(having) = &mut group.having {
            for expr in having {
                walk_expr_mut(expr, &mut replace)?;
            }
        }
    }
    for (expr, _, _) in &mut plan.order_by {
        walk_expr_mut(expr, &mut replace)?;
    }
    if let Some(limit) = &mut plan.limit {
        walk_expr_mut(limit, &mut replace)?;
    }
    if let Some(offset) = &mut plan.offset {
        walk_expr_mut(offset, &mut replace)?;
    }
    for row in &mut plan.values {
        for expr in row {
            walk_expr_mut(expr, &mut replace)?;
        }
    }
    for aggregate in &mut plan.aggregates {
        walk_expr_mut(&mut aggregate.original_expr, &mut replace)?;
        for arg in &mut aggregate.args {
            walk_expr_mut(arg, &mut replace)?;
        }
        if let Some(filter) = &mut aggregate.filter_expr {
            walk_expr_mut(filter, &mut replace)?;
        }
    }
    Ok(found)
}

/// Return whether a subquery is simple enough for a semi-join or anti-join.
///
/// The patterns take no subquery that holds a correlated subquery of its own. The general reach
/// takes one, its own subqueries moving into the outer query with its table, where the worklist
/// takes them next (top-down), when nothing they hold can fail or change between calls: the join
/// may run them on rows in another order than the subquery did.
fn can_rewrite_as_semi_join(
    plan: &SelectPlan,
    resolver: &Resolver<'_>,
    reach: Reach,
) -> Result<bool> {
    // The current semi-join and anti-join loops stop after a match in one table.
    // A joined inner query needs a separate implementation.
    if plan.table_references.joined_tables().len() != 1 {
        return Ok(false);
    }
    if plan.limit.is_some()
        || plan.group_by.is_some()
        || !plan.order_by.is_empty()
        || !matches!(plan.distinctness, Distinctness::NonDistinct)
        || plan.window.is_some()
        || plan.offset.is_some()
        || !plan.values.is_empty()
    {
        return Ok(false);
    }

    // An aggregate can return a row when its input table is empty.
    if !plan.aggregates.is_empty() {
        return Ok(false);
    }

    if plan
        .non_from_clause_subqueries
        .iter()
        .any(|subquery| subquery.correlated)
        && (reach == Reach::Patterns || !nested_selects_allow(plan, &[], resolver)?)
    {
        return Ok(false);
    }
    if plan
        .table_references
        .joined_tables()
        .iter()
        .any(|table| match &table.table {
            crate::schema::Table::FromClauseSubquery(subquery) => {
                plan_is_correlated(&subquery.plan)
            }
            _ => false,
        })
    {
        return Ok(false);
    }

    for term in &plan.where_clause {
        if expression_can_fail_on_input(&term.expr)
            || expr_contains_nondeterministic_scalar_function(&term.expr, resolver)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The top-level `WHERE` term that uses an `EXISTS` result.
struct ExistsTerm {
    /// The term's position in the `WHERE` list.
    index: usize,
    /// Whether the term is `NOT EXISTS`.
    negated: bool,
}

/// Find the WHERE term that references the EXISTS subquery with the given ID.
/// Returns `None` if the subquery appears where it cannot be moved into a join
/// (e.g., inside OR, or referenced multiple times).
fn find_exists_in_where(
    where_clause: &[WhereTerm],
    subquery_id: TableInternalId,
) -> Option<ExistsTerm> {
    for (index, term) in where_clause.iter().enumerate() {
        // An EXISTS inside an outer join condition must still allow the outer
        // row through when it is false.
        if term.origin.is_outer_join() {
            continue;
        }
        if let Expr::SubqueryResult {
            subquery_id: id,
            query_type: ast::SubqueryType::Exists { .. },
            ..
        } = &term.expr
        {
            if *id == subquery_id {
                return Some(ExistsTerm {
                    index,
                    negated: false,
                });
            }
        }
        if let Expr::Unary(UnaryOperator::Not, inner) = &term.expr {
            if let Expr::SubqueryResult {
                subquery_id: id,
                query_type: ast::SubqueryType::Exists { .. },
                ..
            } = inner.as_ref()
            {
                if *id == subquery_id {
                    return Some(ExistsTerm {
                        index,
                        negated: true,
                    });
                }
            }
        }
    }
    None
}

/// Find a direct IN term. IN under OR and NOT IN stay as subqueries.
fn find_in_term(where_clause: &[WhereTerm], subquery_id: TableInternalId) -> Option<InTerm> {
    where_clause.iter().enumerate().find_map(|(index, term)| {
        if term.origin.is_outer_join() {
            return None;
        }
        let Expr::SubqueryResult {
            subquery_id: id,
            lhs: Some(left),
            not_in,
            query_type: ast::SubqueryType::In { .. },
        } = &term.expr
        else {
            return None;
        };
        (*id == subquery_id).then(|| InTerm {
            index,
            left: left.as_ref().clone(),
            not_in: *not_in,
        })
    })
}

/// A direct `IN` or `NOT IN` conjunct of `WHERE`.
struct InTerm {
    /// The term's position in the `WHERE` list.
    index: usize,
    /// The test's left side.
    left: Expr,
    /// Whether the test is `NOT IN`.
    not_in: bool,
}

/// Return whether an inner `WHERE` term can move into the outer query.
fn can_move_join_term(
    expr: &Expr,
    outer_table_ids: &[TableInternalId],
    inner_table_ids: &[TableInternalId],
) -> Result<bool> {
    let has_outer_ref = collect_table_refs(expr)?
        .iter()
        .any(|table| outer_table_ids.contains(table));
    if !has_outer_ref {
        return Ok(true);
    }
    is_inner_outer_comparison(expr, outer_table_ids, inner_table_ids)
}

/// One side of a comparison may use inner tables and the other may use outer tables.
fn is_inner_outer_comparison(
    expr: &Expr,
    outer_table_ids: &[TableInternalId],
    inner_table_ids: &[TableInternalId],
) -> Result<bool> {
    let Expr::Binary(left, operator, right) = expr else {
        return Ok(false);
    };
    if !operator.is_comparison() {
        return Ok(false);
    }

    let left_tables = collect_table_refs(left)?;
    let right_tables = collect_table_refs(right)?;

    let left_is_outer = left_tables
        .iter()
        .all(|table| outer_table_ids.contains(table))
        && !left_tables.is_empty();
    let left_is_inner = left_tables
        .iter()
        .all(|table| inner_table_ids.contains(table))
        && !left_tables.is_empty();
    let right_is_outer = right_tables
        .iter()
        .all(|table| outer_table_ids.contains(table))
        && !right_tables.is_empty();
    let right_is_inner = right_tables
        .iter()
        .all(|table| inner_table_ids.contains(table))
        && !right_tables.is_empty();

    Ok((left_is_outer && right_is_inner) || (left_is_inner && right_is_outer))
}

/// Return each table used by an expression.
fn collect_table_refs(expr: &Expr) -> Result<SmallVec<[TableInternalId; 2]>> {
    let mut tables = SmallVec::new();
    walk_expr(expr, &mut |expr: &Expr| -> Result<WalkControl> {
        if let Expr::Column { table, .. } | Expr::RowId { table, .. } = expr {
            if !tables.contains(table) {
                tables.push(*table);
            }
        }
        Ok(WalkControl::Continue)
    })?;
    Ok(tables)
}

/// Replace a WHERE term after its join now performs the same test.
fn replace_subquery_term_with_true(where_clause: &mut [WhereTerm], index: usize) -> Result<()> {
    let term = where_clause.get_mut(index).ok_or_else(|| {
        LimboError::InternalError("an unnested subquery's WHERE term is gone".into())
    })?;
    term.expr = Expr::Literal(ast::Literal::Numeric("1".to_string()));
    Ok(())
}
