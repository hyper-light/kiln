//! One estimate per joined set (docs/design/planner.md, decision 13): the rows a join of a set of
//! tables returns, computed from the predicates over that set and nothing else, so every join order
//! and every access path the search tries for the set gets the same number. PostgreSQL keeps one
//! `rows` for a join relation, whatever paths it holds (`set_joinrel_size_estimates`, costsize.c);
//! CockroachDB derives a memo group's logical properties once for all its expressions (memo.go);
//! DuckDB caches the cardinality of each relation set (cardinality_estimator.cpp). An access method
//! changes only what reading the set costs.

use rustc_hash::FxHashMap as HashMap;
use smallvec::SmallVec;

use amphora_engine_parser::ast::{self, Operator};

use super::{
    constraints::{Constraint, TableConstraints},
    cost::{constraints_selectivity, RowCountEstimate},
    cost_params::CostModelParams,
};
use crate::schema::Table as SchemaTable;
use crate::translate::{
    expr::{literal_truth, simplified_and_or},
    plan::{JoinType, JoinedTable, Plan, WhereTerm},
    planner::TableMask,
};

/// How many sets the memo keeps. The search asks for at most its own subsets: the dynamic
/// programming search's 2^12 below `GREEDY_JOIN_THRESHOLD` tables, the greedy search's n² prefixes
/// above it, and the prefixes before each RIGHT or FULL JOIN. A set asked for past the bound is
/// computed again each time, to the same number.
const MEMO_SETS: usize = 1 << 13;

/// How a table's rows enter the sets that hold it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Inner,
    /// A LEFT JOIN's right table: its ON terms decide which left rows match, the rest are kept.
    Left,
    /// A RIGHT JOIN's right table: its unmatched rows are kept, the left side's unmatched aren't.
    Right,
    /// A FULL JOIN's right table: the unmatched rows of both sides are kept.
    Full,
    /// A semi or anti join's table: it keeps or drops left rows, never adds any.
    SemiOrAnti,
}

/// What a constraint counts for in its table's factor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A term of the table's own outer join's ON clause: it decides what matches.
    On,
    /// A filter on the join's rows.
    Filter,
    /// `col IS NULL` on a LEFT JOIN's right table, of a column never NULL in a row of it: it keeps
    /// only the left rows that matched nothing.
    Unmatched,
    /// The same test on a table no join null-extends: no row passes it. A filter, of no rows.
    Never,
}

struct Table {
    kind: Kind,
    rows: f64,
    /// Indexes into the table's constraints, each with what it counts for.
    counted: SmallVec<[(usize, Class); 4]>,
    /// For a derived table grouped by columns that `=` terms equate to other tables' values (a
    /// scalar subquery unnested, `unnest.rs`), one such constraint for each of its GROUP BY's
    /// columns: once all are ready, one row for each row of the rest, as the correlated subquery
    /// it replaced gives; else empty.
    keys: SmallVec<[usize; 2]>,
    /// A join's right table with an ON term no row passes: it matches no row.
    matches_none: bool,
}

pub(super) struct JoinedSetRows<'a> {
    tables: Vec<Table>,
    constraints: &'a [TableConstraints],
    params: &'a CostModelParams,
    initial: f64,
    memo: HashMap<TableMask, f64>,
}

impl<'a> JoinedSetRows<'a> {
    /// `homes` is each WHERE term's home table (`WhereTermInfo::home_table`): a term that isn't an
    /// outer join's ON term counts there and only there, once in every set that holds the tables
    /// it reads.
    pub(super) fn new(
        joined_tables: &[JoinedTable],
        constraints: &'a [TableConstraints],
        where_clause: &[WhereTerm],
        homes: impl Fn(usize) -> Option<usize>,
        base_table_rows: &[RowCountEstimate],
        initial: f64,
        params: &'a CostModelParams,
    ) -> Self {
        let mut tables = Vec::with_capacity(joined_tables.len());
        // The terms equating a grouped derived table's keys: they say which row of it each row
        // of the rest meets, which its one-row factor already counts, so they filter nothing.
        let mut key_terms: SmallVec<[usize; 4]> = SmallVec::new();
        let mut keys_by_table: Vec<SmallVec<[usize; 2]>> = Vec::with_capacity(joined_tables.len());
        for (table_idx, joined) in joined_tables.iter().enumerate() {
            let keys = grouped_by_keys(table_idx, joined, constraints, where_clause);
            for key in &keys {
                if let Some(c) = constraints
                    .get(table_idx)
                    .and_then(|c| c.constraints.get(*key))
                {
                    key_terms.push(c.where_clause_pos.0);
                }
            }
            keys_by_table.push(keys);
        }
        for ((table_idx, joined), keys) in joined_tables.iter().enumerate().zip(keys_by_table) {
            let kind = match joined.join_info.as_ref().map(|info| info.join_type) {
                Some(JoinType::LeftOuter) => Kind::Left,
                Some(JoinType::RightOuter) => Kind::Right,
                Some(JoinType::FullOuter) => Kind::Full,
                Some(JoinType::Semi | JoinType::Anti) => Kind::SemiOrAnti,
                _ => Kind::Inner,
            };
            let rows = base_table_rows
                .get(table_idx)
                .copied()
                .unwrap_or_else(|| RowCountEstimate::hardcoded_fallback(params));
            // Whether a join has null-extended the table by the time a term evaluated after the
            // join at place `point` reads it: its own LEFT or FULL JOIN, or a RIGHT or FULL JOIN
            // after it up to `point`. A WHERE term is evaluated after every join; an inner join's
            // ON term at its own join, before any join after it null-extends anything.
            let null_extended_by = |point: usize| {
                matches!(kind, Kind::Left | Kind::Full)
                    || joined_tables
                        .iter()
                        .enumerate()
                        .skip(table_idx)
                        .skip(1)
                        .take_while(|(later_idx, _)| *later_idx <= point)
                        .any(|(_, later)| {
                            matches!(
                                later.join_info.as_ref().map(|info| info.join_type),
                                Some(JoinType::RightOuter | JoinType::FullOuter)
                            )
                        })
            };
            // An ON term of the table's own outer join that no row of it passes.
            let mut on_never_true = false;
            let mut counted = SmallVec::new();
            let table_constraints = constraints
                .get(table_idx)
                .map_or(&[][..], |c| &c.constraints[..]);
            for (constraint_idx, constraint) in table_constraints.iter().enumerate() {
                let Some(term) = where_clause.get(constraint.where_clause_pos.0) else {
                    continue;
                };
                if term.consumed
                    || constraint.implied
                    || kind == Kind::SemiOrAnti
                    || key_terms.contains(&constraint.where_clause_pos.0)
                {
                    continue;
                }
                // A term with this table on both sides gives a constraint for each side, one
                // predicate: it counts once, by its most selective constraint.
                let same_term = |slot: &&mut (usize, Class)| {
                    table_constraints
                        .get(slot.0)
                        .is_some_and(|c| c.where_clause_pos.0 == constraint.where_clause_pos.0)
                };
                if let Some(slot) = counted.iter_mut().find(same_term) {
                    if table_constraints
                        .get(slot.0)
                        .is_some_and(|c| constraint.selectivity < c.selectivity)
                    {
                        slot.0 = constraint_idx;
                    }
                    continue;
                }
                let class = if term.origin.is_outer_join() {
                    // Constraints from an outer join's ON terms exist only on its right table,
                    // whose rows the ON terms see before any is null-extended.
                    on_never_true |= null_test_of_never_null(constraint, where_clause, joined);
                    Class::On
                } else if homes(constraint.where_clause_pos.0) != Some(table_idx) {
                    continue;
                } else if !null_test_of_never_null(constraint, where_clause, joined) {
                    Class::Filter
                } else if kind == Kind::Left {
                    Class::Unmatched
                } else if null_extended_by(
                    term.origin
                        .join_origin()
                        .and_then(|origin| {
                            joined_tables
                                .iter()
                                .position(|t| t.internal_id == origin.right_table())
                        })
                        .unwrap_or(joined_tables.len()),
                ) {
                    Class::Filter
                } else {
                    Class::Never
                };
                counted.push((constraint_idx, class));
            }
            let matches_none = on_never_true
                || where_clause.iter().any(|term| {
                    term.origin
                        .join_origin()
                        .is_some_and(|origin| origin.right_table() == joined.internal_id)
                        && never_true(&term.expr)
                });
            tables.push(Table {
                kind,
                rows: *rows,
                counted,
                keys,
                matches_none,
            });
        }
        // A WHERE term no row passes (`WHERE NULL`) leaves no rows in any set, whether or not
        // the parser folded it into a constant-false WHERE, which plans no table at all; an ON
        // term so makes its join's right table match nothing (`matches_none`).
        let none_pass = where_clause
            .iter()
            .any(|term| term.origin.join_origin().is_none() && never_true(&term.expr));
        Self {
            tables,
            constraints,
            params,
            initial: if none_pass { 0.0 } else { initial },
            memo: HashMap::default(),
        }
    }

    /// The rows the join of `set` returns, whatever order its tables are read in.
    pub(super) fn rows(&mut self, set: &TableMask) -> f64 {
        if let Some(rows) = self.memo.get(set) {
            return *rows;
        }
        let rows = self.compute(set);
        if self.memo.len() < MEMO_SETS {
            if let Ok(key) = crate::alloc::TryClone::try_clone(set) {
                self.memo.insert(key, rows);
            }
        }
        rows
    }

    /// The selectivity of `table`'s constraints of class `class` whose other side reads only
    /// tables of `ready`: the same function the filters of a single table use, so a closed range
    /// is one range wherever its bounds are counted.
    fn selectivity(&self, table: usize, class: Class, ready: &TableMask) -> f64 {
        self.selectivity_of(table, &[class], ready)
    }

    /// [Self::selectivity] of the constraints of any of `classes` together. Only bounds by a
    /// constant (a literal, a parameter, an uncorrelated subquery: no table read) make a range
    /// with each other; a bound by a column of this row or another table's (`x.c <= x.d`,
    /// `x.c <= y.d`) is a join predicate and stands alone, as PostgreSQL's
    /// `clauselist_selectivity` pairs only "a restriction clause with a pseudoconstant on one
    /// side". So a bound counts the same on whichever table it is counted.
    fn selectivity_of(&self, table: usize, classes: &[Class], ready: &TableMask) -> f64 {
        let (Some(t), Some(table_constraints)) =
            (self.tables.get(table), self.constraints.get(table))
        else {
            return 1.0;
        };
        if classes.contains(&Class::On) && t.matches_none {
            return 0.0;
        }
        let ready_in = |constraint: &Constraint| {
            constraint
                .lhs_mask
                .iter()
                .all(|other| other == table || ready.get(other))
        };
        if classes.contains(&Class::Filter)
            && t.counted.iter().any(|(idx, c)| {
                *c == Class::Never
                    && table_constraints
                        .constraints
                        .get(*idx)
                        .is_some_and(ready_in)
            })
        {
            return 0.0;
        }
        let counted = t
            .counted
            .iter()
            .filter(|(_, c)| classes.contains(c))
            .filter_map(|(idx, _)| table_constraints.constraints.get(*idx))
            .filter(|constraint| ready_in(constraint));
        let reads_a_table = |c: &&Constraint| !c.lhs_mask.is_empty();
        let alone: f64 = counted
            .clone()
            .filter(reads_a_table)
            .map(|c| c.selectivity)
            .product();
        alone * constraints_selectivity(counted.filter(|c| !reads_a_table(c)), self.params)
    }

    /// The share of an outer join's matches its other filters keep: what they restrict beyond its
    /// ON terms, `sel(ON and filters) / sel(ON)`, so a filter bounding a column its ON terms bound
    /// makes one range with them, and the matches are never more than the inner join's rows.
    fn filters_given_on(&self, table: usize, ready: &TableMask) -> f64 {
        let on = self.selectivity(table, Class::On, ready);
        if on > 0.0 {
            (self.selectivity_of(table, &[Class::On, Class::Filter], ready) / on).clamp(0.0, 1.0)
        } else {
            self.selectivity(table, Class::Filter, ready)
        }
    }

    /// What `table` multiplies a set's rows by, its terms ready in `ready`; a RIGHT or FULL JOIN's
    /// table is combined with the rows before it instead ([Self::combine]).
    fn factor(&self, table: usize, ready: &TableMask) -> f64 {
        let Some(t) = self.tables.get(table) else {
            return 1.0;
        };
        // One row for each binding of its keys, as the correlated subquery it was unnested from
        // gives; a term on its value counts as the same term on the correlated subquery's value
        // does, by its operator.
        if !t.keys.is_empty() && self.keys_ready(table, ready) {
            return self.selectivity(table, Class::Filter, ready);
        }
        match t.kind {
            Kind::SemiOrAnti => 1.0,
            Kind::Inner if t.matches_none => 0.0,
            Kind::Inner | Kind::Right | Kind::Full => {
                t.rows * self.selectivity(table, Class::Filter, ready)
            }
            Kind::Left => {
                // Statistics give an average match count, not its distribution: a Poisson model
                // gives the chance that a left row matches nothing, and that row is kept once.
                let matches = t.rows * self.selectivity(table, Class::On, ready);
                let unmatched = (-matches).exp();
                let only_unmatched = t
                    .counted
                    .iter()
                    .any(|(_, class)| *class == Class::Unmatched);
                let per_row = if only_unmatched {
                    unmatched
                } else {
                    matches + unmatched
                };
                per_row * self.filters_given_on(table, ready)
            }
        }
    }

    /// Whether every key constraint of `table` reads only tables of `ready`.
    fn keys_ready(&self, table: usize, ready: &TableMask) -> bool {
        let (Some(t), Some(table_constraints)) =
            (self.tables.get(table), self.constraints.get(table))
        else {
            return false;
        };
        t.keys.iter().all(|key| {
            table_constraints
                .constraints
                .get(*key)
                .is_some_and(|constraint| {
                    constraint
                        .lhs_mask
                        .iter()
                        .all(|other| other == table || ready.get(other))
                })
        })
    }

    /// A RIGHT or FULL JOIN of `left` rows with `table`: the matches, the right table's rows that
    /// match no left row, and for FULL the left rows that match no right row, each by the same
    /// Poisson model; then the filters ready in `ready`. So a FULL JOIN is never below its LEFT or
    /// RIGHT JOIN, and neither below the inner join.
    fn combine(&self, left: f64, table: usize, ready: &TableMask) -> f64 {
        let Some(t) = self.tables.get(table) else {
            return left;
        };
        let pair = self.selectivity(table, Class::On, ready);
        let matched = left * t.rows * pair;
        let unmatched_right = t.rows * (-(left * pair)).exp();
        let unmatched_left = if t.kind == Kind::Full {
            left * (-(t.rows * pair)).exp()
        } else {
            0.0
        };
        (matched + unmatched_right + unmatched_left) * self.filters_given_on(table, ready)
    }

    /// The set's rows, in its tables' FROM order, which no join order changes. Without a RIGHT or
    /// FULL JOIN that is a product of each table's factor. A RIGHT or FULL JOIN is a reordering
    /// barrier both ways (`required_lhs_by_table`): every table before it is in any set that holds
    /// it, so the rows before it are a set of their own, combined with it; a filter of those
    /// tables completed by a later one then counts its share where it becomes ready.
    fn compute(&self, set: &TableMask) -> f64 {
        let mut rows = self.initial;
        let mut done = TableMask::default();
        let mut barrier: Option<usize> = None;
        // One pass for each RIGHT or FULL JOIN in the set and one for the rest, each over the
        // set's tables: bounded by the tables a mask holds.
        let barriers: SmallVec<[usize; 2]> = set
            .iter()
            .filter(|t| {
                self.tables
                    .get(*t)
                    .is_some_and(|t| matches!(t.kind, Kind::Right | Kind::Full))
            })
            .collect();
        for end in barriers
            .iter()
            .map(|b| Some(*b))
            .chain(std::iter::once(None))
        {
            let mut target = TableMask::default();
            for t in set.iter().filter(|t| end.is_none_or(|end| *t < end)) {
                if target.set(t).is_err() {
                    return rows;
                }
            }
            if let Some(b) = barrier {
                rows = self.combine(rows, b, &target);
                for u in done.iter() {
                    let before = self.selectivity(u, Class::Filter, &done);
                    if before > 0.0 {
                        let after = self.selectivity(u, Class::Filter, &target);
                        rows *= (after / before).clamp(0.0, 1.0);
                    }
                }
                if done.set(b).is_err() {
                    return rows;
                }
            }
            for t in target.iter().filter(|t| !done.get(*t)) {
                rows *= self.factor(t, &target);
            }
            for t in target.iter() {
                if done.set(t).is_err() {
                    return rows;
                }
            }
            barrier = end;
        }
        rows
    }
}

/// `col IS NULL`, however written (`NULL IS col`, `col IS NOT DISTINCT FROM NULL`), where `col` is
/// never NULL in a row of the table (the rowid or a NOT NULL column): it holds only in a row a join
/// null-extended.
fn null_test_of_never_null(
    constraint: &Constraint,
    where_clause: &[WhereTerm],
    table: &JoinedTable,
) -> bool {
    if !matches!(constraint.operator.as_ast_operator(), Some(Operator::Is)) {
        return false;
    }
    matches!(
        constraint.get_constraining_expr_ref(where_clause),
        ast::Expr::Literal(ast::Literal::Null)
    ) && constraint.table_col_pos.is_some_and(|column_pos| {
        table
            .table
            .get_column_at(column_pos)
            .is_some_and(|column| column.is_rowid_alias() || column.notnull())
    })
}

/// Whether no row passes a whole term: a NULL; one that settles to false as SQLite settles an AND
/// or OR of literals ([simplified_and_or], [literal_truth]); a comparison with a NULL literal by
/// `=`, `<>`, `<`, `<=`, `>` or `>=` (always NULL; `IS` is not one); or an AND with such a side.
/// Only as a whole term: `NOT NULL` is NULL. Flat, on a stack of its own (CLAUDE.md section 11).
fn never_true(expr: &ast::Expr) -> bool {
    fn unwrap(mut e: &ast::Expr) -> &ast::Expr {
        while let ast::Expr::Parenthesized(exprs) = e {
            let [only] = exprs.as_slice() else {
                break;
            };
            e = only;
        }
        e
    }
    let is_null = |e: &ast::Expr| matches!(unwrap(e), ast::Expr::Literal(ast::Literal::Null));
    let mut todo: SmallVec<[&ast::Expr; 4]> = SmallVec::new();
    todo.push(expr);
    // An AND's sides, each a term of its own: at most as many as the expression has nodes.
    while let Some(e) = todo.pop() {
        let inner = unwrap(simplified_and_or(e));
        if is_null(inner) || literal_truth(inner) == Some(false) {
            return true;
        }
        match inner {
            ast::Expr::Binary(lhs, Operator::And, rhs) => {
                todo.push(lhs);
                todo.push(rhs);
            }
            ast::Expr::Binary(lhs, op, rhs)
                if matches!(
                    op,
                    Operator::Equals
                        | Operator::NotEquals
                        | Operator::Less
                        | Operator::LessEquals
                        | Operator::Greater
                        | Operator::GreaterEquals
                ) && (is_null(lhs) || is_null(rhs)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// For a derived table grouped by columns, one plain `=` constraint (not a NULL-matching `IS`)
/// equating each of them to another table's value, by its index in the table's constraints: a
/// GROUP BY's output has one row for each value of its columns, so at most one row meets each
/// binding of them. Empty when the table isn't such, or a column has no such constraint.
fn grouped_by_keys(
    table_idx: usize,
    table: &JoinedTable,
    constraints: &[TableConstraints],
    where_clause: &[WhereTerm],
) -> SmallVec<[usize; 2]> {
    let mut keys = SmallVec::new();
    let SchemaTable::FromClauseSubquery(subquery) = &table.table else {
        return keys;
    };
    let Plan::Select(select) = subquery.plan.as_ref() else {
        return keys;
    };
    let (Some(group_by), Some(table_constraints)) =
        (select.group_by.as_ref(), constraints.get(table_idx))
    else {
        return keys;
    };
    for key in &group_by.exprs {
        let found = table_constraints.constraints.iter().position(|constraint| {
            matches!(
                constraint.operator.as_ast_operator(),
                Some(Operator::Equals)
            ) && !constraint.null_matching
                && !constraint.lhs_mask.is_empty()
                && where_clause
                    .get(constraint.where_clause_pos.0)
                    .is_some_and(|term| !term.consumed)
                && constraint
                    .table_col_pos
                    .and_then(|pos| select.result_columns.get(pos))
                    .is_some_and(|column| column.expr == *key)
        });
        match found {
            Some(idx) => keys.push(idx),
            None => return SmallVec::new(),
        }
    }
    keys
}
