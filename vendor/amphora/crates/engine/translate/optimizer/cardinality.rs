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
    constraints::{Constraint, ConstraintOperator, TableConstraints},
    cost::{constraints_selectivity, RowCountEstimate},
    cost_params::CostModelParams,
};
use crate::schema::{Schema, Table as SchemaTable};
use crate::translate::{
    expr::{literal_truth, simplified_and_or},
    plan::{JoinOrigin, JoinType, JoinedTable, Plan, WhereTerm, WhereTermOrigin},
    planner::TableMask,
};

/// How many sets the memo keeps. The search asks for at most its own subsets: the dynamic
/// programming search's `JOIN_SEARCH_BUDGET` (10,000) and its single tables, or past it the greedy
/// search's n² sets (at most 63² = 3,969), and the prefixes before each RIGHT or FULL JOIN. A set
/// asked for past the bound is computed again each time, to the same number.
const MEMO_SETS: usize = 1 << 14;

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

/// A constraint a table's factor counts.
#[derive(Clone, Copy)]
struct Counted {
    /// Its index in the table's constraints.
    constraint: usize,
    class: Class,
    /// For an `=` between columns of two tables that no equivalence class takes (an outer join's
    /// ON term, a term reading a LEFT JOIN's right table), the pair's own class divisor's inverse,
    /// in place of the constraint's selectivity, which takes a fixed one where no index leads its
    /// column (decision 18).
    pair: Option<f64>,
}

/// A column an `=` join term reads, with its domain: the distinct values it can join on.
struct Domain {
    table: usize,
    /// The column's place in its table; `None` for the rowid, however named.
    column: Option<usize>,
    /// At least one.
    values: f64,
    /// Whether `values` is a count (a unique key's rows, `sqlite_stat1`'s distinct values) rather
    /// than the table's rows standing in for one.
    known: bool,
}

/// A unique key of more than one column, each column joined by some `=` term: once every column
/// is in a class with another table's column, each row of the rest meets at most one of its rows.
struct UniqueKey {
    table: usize,
    /// Its columns' places in [JoinedSetRows::columns].
    columns: SmallVec<[usize; 4]>,
}

/// A class's divisor, built a column at a time: the product of its columns' domains but one. Under
/// containment (the smaller domain's values all in the larger's) a class of counted domains joins
/// its rows on all but the smallest; a column with no count stands for a key of a table at most its
/// rows, and the largest such one is left out, so a foreign key with no statistics joins its fact
/// table's rows to the key's table once each (Ebergen's rule). Each column multiplies the divisor by
/// at least one and a merge of two classes by at least the smaller one's left-out domain, so no
/// added term lowers it (decision 18).
#[derive(Clone, Copy)]
struct Divisor {
    product: f64,
    /// The largest domain with no count, 0 if none.
    largest_unknown: f64,
    smallest: f64,
}

impl Divisor {
    const EMPTY: Self = Self {
        product: 1.0,
        largest_unknown: 0.0,
        smallest: f64::INFINITY,
    };

    fn with(self, domain: &Domain) -> Self {
        Self {
            product: self.product * domain.values,
            largest_unknown: if domain.known {
                self.largest_unknown
            } else {
                self.largest_unknown.max(domain.values)
            },
            smallest: self.smallest.min(domain.values),
        }
    }

    fn value(self) -> f64 {
        let left_out = if self.largest_unknown > 0.0 {
            self.largest_unknown
        } else if self.smallest.is_finite() {
            self.smallest
        } else {
            1.0
        };
        (self.product / left_out).max(1.0)
    }
}

struct Table {
    kind: Kind,
    rows: f64,
    /// The constraints its factor counts.
    counted: SmallVec<[Counted; 4]>,
    /// For a derived table grouped by columns that `=` terms equate to other tables' values (a
    /// scalar subquery unnested, `unnest.rs`), one such constraint for each of its GROUP BY's
    /// columns: once all are ready, one row for each row of the rest, as the correlated subquery
    /// it replaced gives; else empty.
    keys: SmallVec<[usize; 2]>,
    /// A join's right table with an ON term no row passes: it matches no row.
    matches_none: bool,
    /// Its filter terms its sample ran and the share of the sample they keep (decision 21).
    sampled: Option<Sampled>,
    /// The same for the ON terms of its own outer join, and for those and its filters together:
    /// an outer join's matches that its filters keep then read the sample as the inner join it
    /// restricts to reads it for the same terms (CERT).
    sampled_on: Option<Sampled>,
    sampled_both: Option<Sampled>,
    /// The ON terms of its own outer join that read none of its columns (`y.c = 3` in `y LEFT JOIN
    /// x ON y.c = 3`, which `x RIGHT JOIN y ON y.c = 3` becomes, PL.8), found by their origin among
    /// the WHERE terms and counted by the earlier tables' `on_elsewhere` constraints: they decide
    /// which left rows match, as its own ON terms do.
    on_elsewhere: SmallVec<[Elsewhere; 1]>,
}

/// The ON terms of a later outer join that one earlier table's `on_elsewhere` constraints count.
struct Elsewhere {
    table: usize,
    /// Each constraint's index in the table's `on_elsewhere`, with its pair selectivity as in
    /// [Counted].
    counted: SmallVec<[(usize, Option<f64>); 2]>,
    /// Those terms its sample ran, as a right table's ON terms are (decision 21).
    sampled: Option<Sampled>,
    /// One of them is `col IS NULL` of a column never NULL in a row of the table, which no join
    /// has null-extended where the ON terms run: no left row matches, as with a right table's
    /// `matches_none`.
    never: bool,
}

/// Which of a table's terms its sample runs: its filters, or its own outer join's ON terms.
#[derive(Clone, Copy)]
enum Which {
    Filter,
    On,
    Both,
}

/// A table's WHERE terms run over its sample together (decision 21).
struct Sampled {
    /// The terms' places in the WHERE clause.
    terms: SmallVec<[usize; 8]>,
    selectivity: f64,
}

pub(super) struct JoinedSetRows<'a> {
    tables: Vec<Table>,
    /// The columns the equivalence classes' terms read, at most two a WHERE term.
    columns: Vec<Domain>,
    /// Each `=` term an equivalence class takes, by its two columns' places in `columns`: in a set
    /// that holds both tables it puts them in one class.
    equalities: Vec<(usize, usize)>,
    unique_keys: Vec<UniqueKey>,
    constraints: &'a [TableConstraints],
    params: &'a CostModelParams,
    initial: f64,
    memo: HashMap<TableMask, f64>,
    /// Each set's proven upper bound, which its estimate never passes (PL.11).
    bound: DegreeBound,
}

impl<'a> JoinedSetRows<'a> {
    /// `homes` is each WHERE term's home table (`WhereTermInfo::home_table`): a term that isn't an
    /// outer join's ON term counts there and only there, once in every set that holds the tables
    /// it reads.
    #[expect(clippy::too_many_arguments)]
    pub(super) fn new(
        joined_tables: &[JoinedTable],
        constraints: &'a [TableConstraints],
        where_clause: &[WhereTerm],
        homes: impl Fn(usize) -> Option<usize>,
        base_table_rows: &[RowCountEstimate],
        initial: f64,
        params: &'a CostModelParams,
        schema: &Schema,
    ) -> Self {
        let mut tables = Vec::with_capacity(joined_tables.len());
        let kinds: SmallVec<[Kind; 8]> = joined_tables
            .iter()
            .map(
                |joined| match joined.join_info.as_ref().map(|info| info.join_type) {
                    Some(JoinType::LeftOuter) => Kind::Left,
                    Some(JoinType::RightOuter) => Kind::Right,
                    Some(JoinType::FullOuter) => Kind::Full,
                    Some(JoinType::Semi | JoinType::Anti) => Kind::SemiOrAnti,
                    _ => Kind::Inner,
                },
            )
            .collect();
        let table_rows = |table_idx: usize| {
            base_table_rows
                .get(table_idx)
                .map_or(params.rows_per_table_fallback, |rows| **rows)
        };
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
        // The equivalence classes' terms (decision 18): an `=` of two inner tables' columns, from
        // WHERE or an inner join's ON. A RIGHT or FULL JOIN null-extends the tables before it, so
        // a query with one keeps each term's own selectivity, as does a term reading an outer
        // join's right table or a semi or anti join's.
        let barrier = kinds.iter().any(|k| matches!(k, Kind::Right | Kind::Full));
        let mut columns: Vec<Domain> = Vec::new();
        let mut equalities: Vec<(usize, usize)> = Vec::new();
        let mut class_terms: SmallVec<[usize; 8]> = SmallVec::new();
        let mut pair_terms: SmallVec<[(usize, f64); 4]> = SmallVec::new();
        for (pos, term) in where_clause.iter().enumerate() {
            if term.consumed || key_terms.contains(&pos) {
                continue;
            }
            let Some((a, b)) = column_equality(&term.expr, joined_tables) else {
                continue;
            };
            let kind_of = |c: &ColumnRef| kinds.get(c.0).copied();
            if [kind_of(&a), kind_of(&b)].contains(&Some(Kind::SemiOrAnti)) {
                continue;
            }
            let domain = |(table, column): ColumnRef| {
                let (values, known) =
                    column_domain(joined_tables, table, column, table_rows(table), schema);
                Domain {
                    table,
                    column,
                    values,
                    known,
                }
            };
            let joins_inner = !barrier
                && matches!(
                    term.origin,
                    WhereTermOrigin::Where | WhereTermOrigin::Join(_)
                )
                && !term.origin.is_outer_join()
                && kind_of(&a) == Some(Kind::Inner)
                && kind_of(&b) == Some(Kind::Inner);
            if !joins_inner {
                let pair = Divisor::EMPTY.with(&domain(a)).with(&domain(b));
                pair_terms.push((pos, 1.0 / pair.value()));
                continue;
            }
            let mut place = |c: ColumnRef| {
                if let Some(at) = columns
                    .iter()
                    .position(|d| d.table == c.0 && d.column == c.1)
                {
                    return at;
                }
                let at = columns.len();
                columns.push(domain(c));
                at
            };
            let (a, b) = (place(a), place(b));
            equalities.push((a, b));
            class_terms.push(pos);
        }
        let unique_keys = composite_keys(joined_tables, &kinds, &columns, schema);
        let bound = DegreeBound::new(
            joined_tables,
            &kinds,
            &columns,
            &equalities,
            &table_rows,
            schema,
        );
        let on_elsewhere: Vec<SmallVec<[Elsewhere; 1]>> = joined_tables
            .iter()
            .map(|joined| {
                on_elsewhere(
                    joined,
                    joined_tables,
                    constraints,
                    where_clause,
                    &pair_terms,
                    schema,
                    params,
                )
            })
            .collect();
        for (((table_idx, joined), keys), on_elsewhere) in joined_tables
            .iter()
            .enumerate()
            .zip(keys_by_table)
            .zip(on_elsewhere)
        {
            let kind = kinds.get(table_idx).copied().unwrap_or(Kind::Inner);
            let rows = table_rows(table_idx);
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
                    || class_terms.contains(&constraint.where_clause_pos.0)
                {
                    continue;
                }
                // A term with this table on both sides gives a constraint for each side, one
                // predicate: it counts once, by its most selective constraint.
                let same_term = |slot: &&mut Counted| {
                    table_constraints
                        .get(slot.constraint)
                        .is_some_and(|c| c.where_clause_pos.0 == constraint.where_clause_pos.0)
                };
                if let Some(slot) = counted.iter_mut().find(same_term) {
                    if table_constraints
                        .get(slot.constraint)
                        .is_some_and(|c| constraint.selectivity < c.selectivity)
                    {
                        slot.constraint = constraint_idx;
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
                let pair = pair_terms
                    .iter()
                    .find(|(pos, _)| *pos == constraint.where_clause_pos.0)
                    .map(|(_, selectivity)| *selectivity);
                counted.push(Counted {
                    constraint: constraint_idx,
                    class,
                    pair,
                });
            }
            let matches_none = on_never_true
                || where_clause.iter().any(|term| {
                    term.origin
                        .join_origin()
                        .is_some_and(|origin| origin.right_table() == joined.internal_id)
                        && never_true(&term.expr)
                });
            // A term whose constraint counts as other than a filter (`IS NULL` of a column no
            // row of the table holds NULL) keeps its class's count.
            let not_a_filter = |pos: usize| {
                counted.iter().any(|c: &Counted| {
                    c.class != Class::Filter
                        && c.class != Class::On
                        && table_constraints
                            .get(c.constraint)
                            .is_some_and(|c| c.where_clause_pos.0 == pos)
                })
            };
            let (sampled, sampled_on, sampled_both) = if kind == Kind::SemiOrAnti {
                (None, None, None)
            } else {
                let filter = |pos: usize| {
                    let term = where_clause.get(pos)?;
                    let home = homes(pos);
                    (!not_a_filter(pos)
                        && matches!(
                            term.origin,
                            WhereTermOrigin::Where | WhereTermOrigin::Join(_)
                        )
                        && !term.origin.is_outer_join()
                        && home.is_none_or(|home| home == table_idx))
                    .then(|| home.is_some())
                };
                let on = |pos: usize| {
                    let term = where_clause.get(pos)?;
                    if term.origin.is_outer_join()
                        && term
                            .origin
                            .join_origin()
                            .is_some_and(|origin| origin.right_table() == joined.internal_id)
                    {
                        Some(false)
                    } else {
                        None
                    }
                };
                let sample = |which| {
                    sampled_filters(
                        joined,
                        table_constraints,
                        where_clause,
                        &|pos| match which {
                            Which::Filter => filter(pos),
                            Which::On => on(pos),
                            Which::Both => filter(pos).or_else(|| on(pos)),
                        },
                        schema,
                        params,
                    )
                };
                let on_terms = where_clause
                    .iter()
                    .enumerate()
                    .any(|(pos, _)| on(pos).is_some());
                (
                    sample(Which::Filter),
                    sample(Which::On),
                    on_terms.then(|| sample(Which::Both)).flatten(),
                )
            };
            tables.push(Table {
                kind,
                rows,
                counted,
                keys,
                matches_none,
                sampled,
                sampled_on,
                sampled_both,
                on_elsewhere,
            });
        }
        // PL.8: an outer join's ON terms that read only an earlier table decide which of its rows
        // match, among the rows its filters keep: their share is the earlier table's sample run
        // with its filters and them together, over its filters alone (the filters' share counts at
        // the earlier table). Run apart they would count as independent of the filters, and a
        // correlated pair would put the outer join below its inner join, which runs them together
        // on the same sample (CERT, decision 21).
        let mut conditional: Vec<(usize, usize, f64)> = Vec::new();
        for (later, table) in tables.iter().enumerate() {
            for (entry, e) in table.on_elsewhere.iter().enumerate() {
                let (Some(on), Some(filters), Some(earlier), Some(table_constraints)) = (
                    e.sampled.as_ref(),
                    tables.get(e.table).and_then(|t| t.sampled.as_ref()),
                    joined_tables.get(e.table),
                    constraints.get(e.table),
                ) else {
                    continue;
                };
                if filters.selectivity <= 0.0 {
                    continue;
                }
                let both_constraints: Vec<Constraint> = table_constraints
                    .constraints
                    .iter()
                    .chain(table_constraints.on_elsewhere.iter())
                    .cloned()
                    .collect();
                let together = sampled_filters(
                    earlier,
                    &both_constraints,
                    where_clause,
                    &|pos| {
                        if filters.terms.contains(&pos) {
                            Some(true)
                        } else {
                            on.terms.contains(&pos).then_some(false)
                        }
                    },
                    schema,
                    params,
                );
                if let Some(together) = together {
                    conditional.push((
                        later,
                        entry,
                        (together.selectivity / filters.selectivity).clamp(0.0, 1.0),
                    ));
                }
            }
        }
        for (later, entry, share) in conditional {
            if let Some(sampled) = tables
                .get_mut(later)
                .and_then(|t| t.on_elsewhere.get_mut(entry))
                .and_then(|e| e.sampled.as_mut())
            {
                sampled.selectivity = share;
            }
        }
        // A WHERE term no row passes (`WHERE NULL`) leaves no rows in any set, whether or not
        // the parser folded it into a constant-false WHERE, which plans no table at all; an ON
        // term so makes its join's right table match nothing (`matches_none`).
        let none_pass = where_clause
            .iter()
            .any(|term| term.origin.join_origin().is_none() && never_true(&term.expr));
        Self {
            tables,
            columns,
            equalities,
            unique_keys,
            constraints,
            params,
            initial: if none_pass { 0.0 } else { initial },
            memo: HashMap::default(),
            bound,
        }
    }

    /// The rows the join of `set` returns, whatever order its tables are read in.
    pub(super) fn rows(&mut self, set: &TableMask) -> f64 {
        if let Some(rows) = self.memo.get(set) {
            return *rows;
        }
        // PL.11: an estimate above the set's proven bound is cut to it; one below is left.
        // The bound is a join's for one row of its input, as `initial` counts them.
        let rows = self.bound.cap(set, self.capped(set), self.initial);
        if self.memo.len() < MEMO_SETS {
            if let Ok(key) = crate::alloc::TryClone::try_clone(set) {
                self.memo.insert(key, rows);
            }
        }
        rows
    }

    /// [Self::compute], held under each covered unique key of more than one column (the key cap):
    /// each row of the set's other tables meets at most one row of the key's table, which then
    /// passes its own filters, so the set has at most the rows of the rest times those filters'
    /// share. A cap is the rest's estimate, not this one's, so a term that covers a key only lowers
    /// the set's rows, as CERT requires.
    fn capped(&self, set: &TableMask) -> f64 {
        let mut rows = self.compute(set);
        if self.unique_keys.is_empty() {
            return rows;
        }
        let roots = self.class_roots(set);
        let paired = |column: &usize| {
            roots
                .get(*column)
                .is_some_and(|root| root.is_some_and(|(_, paired)| paired))
        };
        for key in &self.unique_keys {
            if !set.get(key.table) || !key.columns.iter().all(paired) {
                continue;
            }
            let Ok(mut rest) = crate::alloc::TryClone::try_clone(set) else {
                continue;
            };
            rest.clear(key.table);
            let cap = self.compute(&rest) * self.selectivity(key.table, Class::Filter, set);
            rows = rows.min(cap);
        }
        rows
    }

    /// For each of [Self::columns], its class's root in `set` and whether an `=` term of the set
    /// joins it to another column; `None` for a column of a table outside `set`. A union-find over
    /// the set's terms, each find a loop of at most as many steps as there are columns.
    fn class_roots(&self, set: &TableMask) -> SmallVec<[Option<(usize, bool)>; 16]> {
        let mut parent: SmallVec<[usize; 16]> = (0..self.columns.len()).collect();
        let mut paired: SmallVec<[bool; 16]> = self.columns.iter().map(|_| false).collect();
        let find = |parent: &[usize], mut at: usize| {
            for _ in 0..parent.len() {
                match parent.get(at) {
                    Some(&up) if up != at => at = up,
                    _ => break,
                }
            }
            at
        };
        for &(a, b) in &self.equalities {
            let in_set = |c: usize| self.columns.get(c).is_some_and(|d| set.get(d.table));
            if !in_set(a) || !in_set(b) {
                continue;
            }
            let (root_a, root_b) = (find(&parent, a), find(&parent, b));
            if let Some(up) = parent.get_mut(root_a) {
                *up = root_b;
            }
            for c in [a, b] {
                if let Some(p) = paired.get_mut(c) {
                    *p = true;
                }
            }
        }
        self.columns
            .iter()
            .enumerate()
            .map(|(c, d)| {
                set.get(d.table)
                    .then(|| (find(&parent, c), paired.get(c).copied().unwrap_or(false)))
            })
            .collect()
    }

    /// What the set's equivalence classes divide its tables' rows by: each class's [Divisor], over
    /// the columns of the set's tables its terms join.
    fn class_divisor(&self, set: &TableMask) -> f64 {
        let roots = self.class_roots(set);
        let mut by_root: SmallVec<[Divisor; 16]> =
            self.columns.iter().map(|_| Divisor::EMPTY).collect();
        for (domain, root) in self.columns.iter().zip(&roots) {
            if let Some((root, true)) = root {
                if let Some(divisor) = by_root.get_mut(*root) {
                    *divisor = divisor.with(domain);
                }
            }
        }
        by_root.iter().map(|divisor| divisor.value()).product()
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
            && t.counted.iter().any(|counted| {
                counted.class == Class::Never
                    && table_constraints
                        .constraints
                        .get(counted.constraint)
                        .is_some_and(ready_in)
            })
        {
            return 0.0;
        }
        let counted = t
            .counted
            .iter()
            .filter(|counted| classes.contains(&counted.class))
            .filter_map(|counted| {
                table_constraints
                    .constraints
                    .get(counted.constraint)
                    .map(|constraint| (constraint, counted.pair))
            })
            .filter(|(constraint, _)| ready_in(constraint));
        let reads_a_table = |c: &(&Constraint, Option<f64>)| !c.0.lhs_mask.is_empty();
        let alone: f64 = counted
            .clone()
            .filter(reads_a_table)
            .map(|(c, pair)| pair.unwrap_or(c.selectivity))
            .product();
        // The terms its sample ran count once, together (decision 21).
        let (on, filter) = (
            classes.contains(&Class::On),
            classes.contains(&Class::Filter),
        );
        let sampled: SmallVec<[&Sampled; 2]> = match (on, filter) {
            (true, true) if t.sampled_both.is_some() => t.sampled_both.iter().collect(),
            _ => [(filter, t.sampled.as_ref()), (on, t.sampled_on.as_ref())]
                .into_iter()
                .filter(|(asked, _)| *asked)
                .filter_map(|(_, sampled)| sampled)
                .collect(),
        };
        let in_sampled = |c: &Constraint| {
            sampled
                .iter()
                .any(|s| s.terms.contains(&c.where_clause_pos.0))
        };
        let shares: SmallVec<[f64; 2]> = sampled.iter().map(|s| s.selectivity).collect();
        alone
            * constraints_selectivity(
                counted
                    .filter(|c| !reads_a_table(c))
                    .map(|(constraint, _)| constraint)
                    .filter(|constraint| !in_sampled(constraint)),
                &shares,
                self.params,
            )
    }

    /// The share of the pairs of an outer join's left rows and `table`'s rows its ON terms keep:
    /// its own constraints' and those of its ON terms that read only earlier tables, each counted
    /// once its tables are ready, as a right table's ON terms are ([Self::selectivity_of]).
    fn on_selectivity(&self, table: usize, ready: &TableMask) -> f64 {
        let own = self.selectivity(table, Class::On, ready);
        let Some(t) = self.tables.get(table) else {
            return own;
        };
        let mut selectivity = own;
        for e in t.on_elsewhere.iter().filter(|e| ready.get(e.table)) {
            if e.never {
                return 0.0;
            }
            let Some(table_constraints) = self.constraints.get(e.table) else {
                continue;
            };
            let counted = e
                .counted
                .iter()
                .filter_map(|(at, pair)| {
                    table_constraints.on_elsewhere.get(*at).map(|c| (c, *pair))
                })
                .filter(|(c, _)| {
                    c.lhs_mask
                        .iter()
                        .all(|other| other == e.table || ready.get(other))
                });
            let reads_a_table = |c: &(&Constraint, Option<f64>)| !c.0.lhs_mask.is_empty();
            let alone: f64 = counted
                .clone()
                .filter(reads_a_table)
                .map(|(c, pair)| pair.unwrap_or(c.selectivity))
                .product();
            let in_sampled = |c: &Constraint| {
                e.sampled
                    .as_ref()
                    .is_some_and(|s| s.terms.contains(&c.where_clause_pos.0))
            };
            let shares: SmallVec<[f64; 1]> = e.sampled.iter().map(|s| s.selectivity).collect();
            selectivity *= alone
                * constraints_selectivity(
                    counted
                        .filter(|c| !reads_a_table(c))
                        .map(|(c, _)| c)
                        .filter(|c| !in_sampled(c)),
                    &shares,
                    self.params,
                );
        }
        selectivity
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
                let matches = t.rows * self.on_selectivity(table, ready);
                let unmatched = (-matches).exp();
                let only_unmatched = t
                    .counted
                    .iter()
                    .any(|counted| counted.class == Class::Unmatched);
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
        let pair = self.on_selectivity(table, ready);
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
        rows / self.class_divisor(set)
    }
}

/// How a table's rows enter a set's upper bound (PL.11).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// An inner join's table: the set has at most the rest's rows times what each of them meets.
    Inner,
    /// A semi or anti join's table: it keeps or drops rows, never adds one.
    Filter,
    /// An outer join's table: it keeps rows that match nothing, which this bound doesn't count,
    /// so a set holding one has none.
    Unbounded,
}

/// A prefix of an index's columns, each one of the bound's columns, and its degree norms: the
/// most rows of the table one binding of them meets, and the ℓ2 norm of its degree sequence.
#[derive(Clone, Debug)]
struct BoundKey {
    columns: SmallVec<[usize; 4]>,
    max: f64,
    l2: f64,
}

#[derive(Clone, Debug)]
struct BoundTable {
    role: Role,
    /// The table's rows as ANALYZE counted them, raised by the rows added since; `None` where no
    /// count exists, and no bound counts the table's rows whole.
    rows: Option<f64>,
    /// Its index prefixes with degree norms (each raised by the rows added since, which may all
    /// hold one value), and the rowid's: one row a value.
    keys: SmallVec<[BoundKey; 4]>,
}

/// Each joined set's proven upper bound from its tables' degree sequences (docs/design/planner.md,
/// decision (lane pl11); PL.11; LpBound, Zhang et al., SIGMOD 2025; the degree-sequence bound of
/// Deeds, Suciu and Balazinska, SafeBound, SIGMOD 2023). One table's rows, then each other table
/// attached along the `=` terms by the most rows one binding of its joined columns holds (the ℓ∞
/// norm of that prefix's degree sequence), or by its rows where no term attaches it; or, for two
/// tables joined on one column, the product of their columns' ℓ2 norms (Cauchy-Schwarz, the
/// (2, 2) Hölder pair) as the start. Every such order gives a bound; the least of the orders this
/// greedy search tries is kept. An estimate above it is cut to it and one below left as it is.
#[derive(Clone, Debug, Default)]
pub(super) struct DegreeBound {
    tables: Vec<BoundTable>,
    /// The table of each of [JoinedSetRows::columns].
    column_tables: Vec<usize>,
    /// The `=` terms the bound may follow, by their columns' places: those that compare as the
    /// index orders them, both columns of one affinity (no conversion) and BINARY collation.
    equalities: Vec<(usize, usize)>,
}

impl DegreeBound {
    fn new(
        joined_tables: &[JoinedTable],
        kinds: &[Kind],
        columns: &[Domain],
        equalities: &[(usize, usize)],
        table_rows: &impl Fn(usize) -> f64,
        schema: &Schema,
    ) -> Self {
        let binary = |c: crate::translate::collate::CollationSeq| {
            matches!(
                c,
                crate::translate::collate::CollationSeq::Binary
                    | crate::translate::collate::CollationSeq::Unset
            )
        };
        // A column's affinity, `None` where it doesn't compare as BINARY.
        let comparison = |d: &Domain| {
            let joined = joined_tables.get(d.table)?;
            match d.column {
                None => Some(crate::vdbe::affinity::Affinity::Integer),
                Some(pos) => {
                    let column = joined.table.get_column_at(pos)?;
                    binary(column.collation()).then(|| column.affinity())
                }
            }
        };
        let equalities = equalities
            .iter()
            .copied()
            .filter(|(a, b)| {
                let (Some(a), Some(b)) = (columns.get(*a), columns.get(*b)) else {
                    return false;
                };
                matches!((comparison(a), comparison(b)), (Some(x), Some(y)) if x == y)
            })
            .collect();
        let tables = joined_tables
            .iter()
            .enumerate()
            .map(|(table, joined)| {
                let role = match kinds.get(table) {
                    Some(Kind::Inner) => Role::Inner,
                    Some(Kind::SemiOrAnti) => Role::Filter,
                    _ => Role::Unbounded,
                };
                let mut bound = BoundTable {
                    role,
                    rows: None,
                    keys: SmallVec::new(),
                };
                let Some(btree) = joined.table.btree() else {
                    return bound;
                };
                let Some(stats) = schema.analyze_stats.table_stats(&btree.name) else {
                    return bound;
                };
                let Some(analyzed) = stats
                    .row_count
                    .or_else(|| stats.index_stats.values().find_map(|stat| stat.total_rows))
                else {
                    return bound;
                };
                let analyzed = analyzed as f64;
                // PL.9's live count: rows added since ANALYZE raise every count and norm by at
                // most their number (‖d + e‖p ≤ ‖d‖p + ‖e‖1).
                let growth = (table_rows(table) - analyzed).max(0.0);
                let rows = analyzed + growth;
                bound.rows = Some(rows);
                let place = |column: Option<usize>| {
                    columns
                        .iter()
                        .position(|d| d.table == table && d.column == column)
                };
                if let Some(rowid) = place(None) {
                    bound.keys.push(BoundKey {
                        columns: smallvec::smallvec![rowid],
                        max: 1.0,
                        l2: rows.sqrt(),
                    });
                }
                for index in schema
                    .get_indices(&btree.name)
                    .filter(|index| index.is_public() && index.where_clause.is_none())
                {
                    let Some(stat) = stats.index_stats.get(&index.name) else {
                        continue;
                    };
                    if stat.degrees.len() != index.columns.len() {
                        continue;
                    }
                    let mut prefix: SmallVec<[usize; 4]> = SmallVec::new();
                    for (column, degrees) in index.columns.iter().zip(&stat.degrees) {
                        let orders_as_compared = column.expr.is_none()
                            && column.collation.is_none_or(binary)
                            && btree
                                .columns()
                                .get(column.pos_in_table)
                                .is_some_and(|c| !c.is_rowid_alias() && binary(c.collation()));
                        let Some(at) =
                            place(Some(column.pos_in_table)).filter(|_| orders_as_compared)
                        else {
                            break;
                        };
                        prefix.push(at);
                        bound.keys.push(BoundKey {
                            columns: prefix.clone(),
                            max: degrees.max as f64 + growth,
                            l2: degrees.l2() + growth,
                        });
                    }
                }
                bound
            })
            .collect();
        Self {
            tables,
            column_tables: columns.iter().map(|d| d.table).collect(),
            equalities,
        }
    }

    /// `estimate`, for `scale` rows of input, cut to the set's bound for that many.
    fn cap(&self, set: &TableMask, estimate: f64, scale: f64) -> f64 {
        let bound = self.upper_bound(set);
        if bound.is_finite() {
            estimate.min(bound * scale)
        } else {
            estimate
        }
    }

    /// The most rows the join of `set` can return for one row of input, infinite where its
    /// statistics prove none. Each start (a table, or two tables joined on one column) extends
    /// greedily over the set's other inner tables: at most (tables + terms) starts, each at most
    /// tables² steps over their keys, all bounded by the join's size.
    fn upper_bound(&self, set: &TableMask) -> f64 {
        let in_set: SmallVec<[usize; 16]> = set.iter().collect();
        if in_set.iter().any(|t| {
            self.tables
                .get(*t)
                .is_none_or(|t| t.role == Role::Unbounded)
        }) {
            return f64::INFINITY;
        }
        let inner: SmallVec<[usize; 16]> = in_set
            .iter()
            .copied()
            .filter(|t| self.tables.get(*t).is_some_and(|t| t.role == Role::Inner))
            .collect();
        if inner.is_empty() {
            return f64::INFINITY;
        }
        // The set's classes, over the terms the bound may follow: a union-find whose every find
        // is a loop of at most as many steps as there are columns.
        let mut parent: SmallVec<[usize; 16]> = (0..self.column_tables.len()).collect();
        let find = |parent: &[usize], mut at: usize| {
            for _ in 0..parent.len() {
                match parent.get(at) {
                    Some(&up) if up != at => at = up,
                    _ => break,
                }
            }
            at
        };
        let follows = |&(a, b): &(usize, usize)| {
            let in_set = |c: usize| self.column_tables.get(c).is_some_and(|t| set.get(*t));
            in_set(a) && in_set(b)
        };
        for &(a, b) in self.equalities.iter().filter(|e| follows(e)) {
            let (root_a, root_b) = (find(&parent, a), find(&parent, b));
            if let Some(up) = parent.get_mut(root_a) {
                *up = root_b;
            }
        }
        let roots: SmallVec<[usize; 16]> = (0..parent.len()).map(|c| find(&parent, c)).collect();
        let mut best = f64::INFINITY;
        for &start in &inner {
            if let Some(rows) = self.tables.get(start).and_then(|t| t.rows) {
                best = best.min(self.extend(&inner, &roots, &[start], rows));
            }
        }
        for &(a, b) in self.equalities.iter().filter(|e| follows(e)) {
            let (Some(&ta), Some(&tb)) = (self.column_tables.get(a), self.column_tables.get(b))
            else {
                continue;
            };
            if ta == tb || !inner.contains(&ta) || !inner.contains(&tb) {
                continue;
            }
            let l2 = |table: usize, column: usize| {
                self.tables
                    .get(table)
                    .into_iter()
                    .flat_map(|t| &t.keys)
                    .filter(|k| k.columns.as_slice() == [column])
                    .map(|k| k.l2)
                    .fold(f64::INFINITY, f64::min)
            };
            let pair = l2(ta, a) * l2(tb, b);
            if pair.is_finite() {
                best = best.min(self.extend(&inner, &roots, &[ta, tb], pair));
            }
        }
        best
    }

    /// `start`'s tables, holding at most `rows` rows, joined with the rest of `inner` one table at
    /// a time: each the one meeting the fewest rows of it, by the key whose every column's class
    /// holds a table already joined, else by its rows. Infinite where a table has neither.
    fn extend(&self, inner: &[usize], roots: &[usize], start: &[usize], rows: f64) -> f64 {
        let mut joined: SmallVec<[usize; 16]> = start.iter().copied().collect();
        let mut bound = rows;
        // Whether each class root holds a column of a joined table.
        let mut reached: SmallVec<[bool; 16]> = roots.iter().map(|_| false).collect();
        let reach = |reached: &mut SmallVec<[bool; 16]>, table: usize| {
            for (column, root) in roots.iter().enumerate() {
                if self.column_tables.get(column) == Some(&table) {
                    if let Some(r) = reached.get_mut(*root) {
                        *r = true;
                    }
                }
            }
        };
        for table in start {
            reach(&mut reached, *table);
        }
        for _ in 0..inner.len() {
            let next = inner
                .iter()
                .filter(|t| !joined.contains(t))
                .filter_map(|&t| {
                    let table = self.tables.get(t)?;
                    let by_key = table
                        .keys
                        .iter()
                        .filter(|k| {
                            k.columns.iter().all(|c| {
                                roots
                                    .get(*c)
                                    .and_then(|root| reached.get(*root))
                                    .copied()
                                    .unwrap_or(false)
                            })
                        })
                        .map(|k| k.max)
                        .fold(f64::INFINITY, f64::min);
                    Some((t, by_key.min(table.rows.unwrap_or(f64::INFINITY))))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some((table, factor)) = next else {
                break;
            };
            if !factor.is_finite() {
                return f64::INFINITY;
            }
            bound *= factor;
            joined.push(table);
            reach(&mut reached, table);
        }
        bound
    }
}

/// A column by its table's place in the join and its place in the table, `None` for the rowid,
/// however named.
type ColumnRef = (usize, Option<usize>);

/// The two columns `expr` equates, when it is `a = b` of plain columns of two of the join's tables.
fn column_equality(
    expr: &ast::Expr,
    joined_tables: &[JoinedTable],
) -> Option<(ColumnRef, ColumnRef)> {
    let column = |e: &ast::Expr| {
        let (table, column) = match unparenthesized(e) {
            ast::Expr::Column {
                table,
                column,
                is_rowid_alias,
                ..
            } => (*table, if *is_rowid_alias { None } else { Some(*column) }),
            ast::Expr::RowId { table, .. } => (*table, None),
            _ => return None,
        };
        let at = joined_tables.iter().position(|t| t.internal_id == table)?;
        let column = column.filter(|pos| {
            joined_tables
                .get(at)
                .and_then(|t| t.table.get_column_at(*pos))
                .is_none_or(|c| !c.is_rowid_alias())
        });
        Some((at, column))
    };
    let ast::Expr::Binary(lhs, Operator::Equals, rhs) = unparenthesized(expr) else {
        return None;
    };
    let (a, b) = (column(lhs)?, column(rhs)?);
    if a.0 != b.0 {
        Some((a, b))
    } else {
        None
    }
}

/// The terms of `joined` its sample runs, those `takes` names, and the share of the sample they
/// keep together (docs/design/planner.md, decision 21; PL.4): each whose only columns are the
/// table's sampled ones, but for an `=` or IN list its column statistics estimate (decision 20),
/// compiled once and run on each sample row: its length times the sample's rows, no more than a
/// scan of the table takes to run it (decision 21). Where at
/// least [SAMPLE_HITS] rows pass them all, their share of the sample; where fewer do, the share of
/// half a row or of the rows that pass, but no more than the terms' own estimates give by
/// exponential backoff, so a rare value keeps its distinct count's estimate. Each part falls as a
/// term is added, so the whole never rises (CERT). `None` where the table has no sample or no
/// term runs on it.
///
/// [SAMPLE_HITS]: crate::column_stats::SAMPLE_HITS
fn sampled_filters(
    joined: &JoinedTable,
    constraints: &[Constraint],
    where_clause: &[WhereTerm],
    takes: &impl Fn(usize) -> Option<bool>,
    schema: &Schema,
    params: &CostModelParams,
) -> Option<Sampled> {
    let btree = joined.table.btree()?;
    let stats = schema.analyze_stats.table_stats(&btree.name)?;
    let sample = stats.sample.as_deref()?;
    let n = sample.rows.len();
    if n == 0 {
        return None;
    }
    let columns = joined.table.columns();
    let in_sample: SmallVec<[Option<usize>; 16]> = columns
        .iter()
        .map(|c| {
            let name = crate::util::normalize_ident(c.name.as_deref()?);
            sample.columns.iter().position(|s| *s == name)
        })
        .collect();
    let target = super::sample_filter::Target {
        table: joined.internal_id,
        columns,
        in_sample: &in_sample,
        values: params.plan_values.as_deref(),
    };
    let mut terms: SmallVec<[usize; 8]> = SmallVec::new();
    let mut own: SmallVec<[f64; 8]> = SmallVec::new();
    let mut passing: Vec<bool> = vec![true; n];
    for (pos, term) in where_clause.iter().enumerate() {
        // `takes` says whether the term is one of those sampled here, and whether a constraint
        // homes it; one no constraint homes (an OR of conjunctions, say) is this table's when it
        // reads only this table's columns, which its program then holds.
        if term.consumed {
            continue;
        }
        let Some(homed) = takes(pos) else {
            continue;
        };
        // An `=` or IN list of constants on a column with a sketch keeps the estimate its column
        // statistics give it (decision 20), which tells a common value from a rare one where a
        // sample of 1,024 rows only counts it.
        let by_column_stats = constraints.iter().any(|c| {
            c.where_clause_pos.0 == pos
                && c.lhs_mask.is_empty()
                && matches!(
                    c.operator,
                    ConstraintOperator::AstNativeOperator(ast::Operator::Equals)
                        | ConstraintOperator::In { not: false, .. }
                )
                && c.table_col_pos
                    .and_then(|at| columns.get(at)?.name.as_deref())
                    .is_some_and(|name| {
                        stats
                            .columns
                            .contains_key(&crate::util::normalize_ident(name))
                    })
        });
        if by_column_stats {
            continue;
        }
        let Some(program) = target
            .compile(&term.expr)
            .filter(|program| homed || program.reads_a_column())
        else {
            continue;
        };
        let mut passes: Vec<bool> = Vec::with_capacity(n);
        for row in &sample.rows {
            match program.passes(row) {
                Some(pass) => passes.push(pass),
                None => break,
            }
        }
        if passes.len() != n {
            continue;
        }
        let hits = passes.iter().filter(|p| **p).count();
        // The term's own estimate: its constraints' where it gives any, else its sample share.
        let constrained = constraints
            .iter()
            .filter(|c| c.where_clause_pos.0 == pos && c.lhs_mask.is_empty() && !c.implied)
            .map(|c| c.selectivity)
            .reduce(f64::min);
        own.push(constrained.unwrap_or_else(|| (hits as f64).max(0.5) / n as f64));
        for (all, pass) in passing.iter_mut().zip(passes) {
            *all &= pass;
        }
        terms.push(pos);
    }
    if terms.is_empty() {
        return None;
    }
    let hits = passing.iter().filter(|p| **p).count();
    let share = (hits as f64).max(0.5) / n as f64;
    let selectivity = if hits >= crate::column_stats::SAMPLE_HITS {
        share
    } else {
        share.min(super::sample_filter::backoff(&mut own))
    };
    Some(Sampled { terms, selectivity })
}

/// The ON terms of `joined`'s own outer join that read none of its columns, found by their origin
/// among the WHERE terms, grouped by the earlier table whose `on_elsewhere` constraint counts each:
/// a term reading several earlier tables counts once, by its most selective constraint, as a term
/// with a table on both sides does ([JoinedSetRows::new]). Empty for a table no outer join reaches.
fn on_elsewhere(
    joined: &JoinedTable,
    joined_tables: &[JoinedTable],
    constraints: &[TableConstraints],
    where_clause: &[WhereTerm],
    pair_terms: &[(usize, f64)],
    schema: &Schema,
    params: &CostModelParams,
) -> SmallVec<[Elsewhere; 1]> {
    let mut found: SmallVec<[Elsewhere; 1]> = SmallVec::new();
    // The terms no table holds a constraint of (`y.b ISNULL`): only a sample counts them, that of
    // the one table whose columns they read, as a right table's sample counts its own.
    let mut loose: SmallVec<[usize; 2]> = SmallVec::new();
    let own = constraints
        .iter()
        .find(|c| c.table_id == joined.internal_id)
        .map_or(&[][..], |c| &c.constraints[..]);
    for (pos, term) in where_clause.iter().enumerate() {
        let of_this_join = term
            .origin
            .join_origin()
            .and_then(JoinOrigin::outer_table)
            .is_some_and(|outer| outer == joined.internal_id);
        if !of_this_join || term.consumed || own.iter().any(|c| c.where_clause_pos.0 == pos) {
            continue;
        }
        // The earlier table and constraint that count the term: its most selective.
        let mut best: Option<(usize, usize, f64)> = None;
        for (table, table_constraints) in constraints.iter().enumerate() {
            for (at, c) in table_constraints.on_elsewhere.iter().enumerate() {
                if c.where_clause_pos.0 == pos
                    && !c.implied
                    && best.is_none_or(|(_, _, s)| c.selectivity < s)
                {
                    best = Some((table, at, c.selectivity));
                }
            }
        }
        let Some((table, at, _)) = best else {
            loose.push(pos);
            continue;
        };
        let pair = pair_terms
            .iter()
            .find(|(p, _)| *p == pos)
            .map(|(_, selectivity)| *selectivity);
        match found.iter_mut().find(|e| e.table == table) {
            Some(e) => e.counted.push((at, pair)),
            None => {
                let mut counted = SmallVec::new();
                counted.push((at, pair));
                found.push(Elsewhere {
                    table,
                    counted,
                    sampled: None,
                    never: false,
                });
            }
        }
    }
    let at = joined_tables
        .iter()
        .position(|t| t.internal_id == joined.internal_id)
        .unwrap_or(joined_tables.len());
    if !loose.is_empty() {
        for table in 0..at {
            if !found.iter().any(|e| e.table == table) {
                found.push(Elsewhere {
                    table,
                    counted: SmallVec::new(),
                    sampled: None,
                    never: false,
                });
            }
        }
    }
    for e in found.iter_mut() {
        let (Some(earlier), Some(table_constraints)) =
            (joined_tables.get(e.table), constraints.get(e.table))
        else {
            continue;
        };
        let terms: SmallVec<[usize; 4]> = e
            .counted
            .iter()
            .filter_map(|(at, _)| table_constraints.on_elsewhere.get(*at))
            .map(|c| c.where_clause_pos.0)
            .chain(loose.iter().copied())
            .collect();
        // Whether a join null-extends the earlier table before this join's ON terms run: its own
        // LEFT or FULL JOIN, or a RIGHT or FULL JOIN after it and before this one.
        let join_type = |t: &JoinedTable| t.join_info.as_ref().map(|info| info.join_type);
        let null_extended = matches!(
            join_type(earlier),
            Some(JoinType::LeftOuter | JoinType::FullOuter)
        ) || joined_tables.iter().take(at).skip(e.table + 1).any(|t| {
            matches!(
                join_type(t),
                Some(JoinType::RightOuter | JoinType::FullOuter)
            )
        });
        e.never = !null_extended
            && e.counted.iter().any(|(at, _)| {
                table_constraints
                    .on_elsewhere
                    .get(*at)
                    .is_some_and(|c| null_test_of_never_null(c, where_clause, earlier))
            });
        e.sampled = sampled_filters(
            earlier,
            &table_constraints.on_elsewhere,
            where_clause,
            &|pos| terms.contains(&pos).then_some(false),
            schema,
            params,
        );
    }
    found.retain(|e| !e.counted.is_empty() || e.sampled.is_some());
    found
}

/// A column's domain, at least one and at most its table's `rows`, and whether it is a count: a
/// unique key's rows (the rowid, a primary key of one column, a unique index of one column and no
/// WHERE), else the distinct values `sqlite_stat1` gives where an index without a WHERE leads it,
/// else its sketch's (decision 20); else the table's rows, its column taken for a key.
fn column_domain(
    joined_tables: &[JoinedTable],
    table: usize,
    column: Option<usize>,
    rows: f64,
    schema: &Schema,
) -> (f64, bool) {
    let rows = rows.max(1.0);
    let Some(joined) = joined_tables.get(table) else {
        return (rows, false);
    };
    let Some(column) = column else {
        return (rows, true);
    };
    let name = joined.table.get_name();
    if let Some(btree) = joined.table.btree() {
        if let [(key, _)] = btree.primary_key_columns.as_slice() {
            if btree.get_column(key).is_some_and(|(pos, _)| pos == column) {
                return (rows, true);
            }
        }
    }
    let leads = |index: &crate::schema::Index| {
        index.where_clause.is_none()
            && index
                .columns
                .first()
                .is_some_and(|c| c.expr.is_none() && c.pos_in_table == column)
    };
    let stats = schema.analyze_stats.table_stats(name);
    let mut distinct = None;
    for index in schema
        .get_indices(name)
        .filter(|index| index.is_public() && leads(index))
    {
        if index.unique && index.columns.len() == 1 {
            return (rows, true);
        }
        let counted = stats
            .and_then(|s| s.index_stats.get(&index.name))
            .and_then(
                |stat| match (stat.total_rows, stat.avg_rows_per_distinct_prefix.first()) {
                    (Some(total), Some(&per_key)) if total > 0 && per_key > 0 => {
                        Some(total as f64 / per_key as f64)
                    }
                    _ => None,
                },
            );
        distinct = distinct.or(counted);
    }
    // Where no index leads it, the column's sketch (decision 20).
    let sketched = || {
        let column = joined.table.get_column_at(column)?.name.as_deref()?;
        let counted = stats?.columns.get(&crate::util::normalize_ident(column))?;
        if counted.non_null > 0 {
            Some(counted.distinct)
        } else {
            None
        }
    };
    match distinct.or_else(sketched) {
        Some(values) => (values.clamp(1.0, rows), true),
        None => (rows, false),
    }
}

/// Each inner table's unique keys of more than one column (its primary key, its unique indexes
/// without a WHERE) whose every column an equivalence class's term reads, by their places in
/// `columns`.
fn composite_keys(
    joined_tables: &[JoinedTable],
    kinds: &[Kind],
    columns: &[Domain],
    schema: &Schema,
) -> Vec<UniqueKey> {
    let mut keys = Vec::new();
    for (table, joined) in joined_tables.iter().enumerate() {
        if kinds.get(table) != Some(&Kind::Inner) || !columns.iter().any(|d| d.table == table) {
            continue;
        }
        let place = |pos: usize| {
            let column = if joined
                .table
                .get_column_at(pos)
                .is_none_or(|c| !c.is_rowid_alias())
            {
                Some(pos)
            } else {
                None
            };
            columns
                .iter()
                .position(|d| d.table == table && d.column == column)
        };
        let mut found: Vec<SmallVec<[usize; 4]>> = Vec::new();
        if let Some(btree) = joined.table.btree() {
            if btree.primary_key_columns.len() > 1 {
                let key: Option<SmallVec<[usize; 4]>> = btree
                    .primary_key_columns
                    .iter()
                    .map(|(name, _)| btree.get_column(name).and_then(|(pos, _)| place(pos)))
                    .collect();
                found.extend(key);
            }
        }
        for index in schema.get_indices(joined.table.get_name()) {
            if !index.is_public()
                || !index.unique
                || index.where_clause.is_some()
                || index.columns.len() < 2
            {
                continue;
            }
            let key: Option<SmallVec<[usize; 4]>> = index
                .columns
                .iter()
                .map(|c| {
                    if c.expr.is_none() {
                        place(c.pos_in_table)
                    } else {
                        None
                    }
                })
                .collect();
            found.extend(key);
        }
        keys.extend(
            found
                .into_iter()
                .map(|columns| UniqueKey { table, columns }),
        );
    }
    keys
}

/// `e` without the parentheses around it.
fn unparenthesized(mut e: &ast::Expr) -> &ast::Expr {
    while let ast::Expr::Parenthesized(exprs) = e {
        let [only] = exprs.as_slice() else {
            break;
        };
        e = only;
    }
    e
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
            .is_some_and(|column| column.is_rowid_alias() || column.trusted_notnull())
    })
}

/// Whether no row passes a whole term: a NULL; one that settles to false as SQLite settles an AND
/// or OR of literals ([simplified_and_or], [literal_truth]); a comparison with a NULL literal by
/// `=`, `<>`, `<`, `<=`, `>` or `>=` (always NULL; `IS` is not one); or an AND with such a side.
/// Only as a whole term: `NOT NULL` is NULL. Flat, on a stack of its own (CLAUDE.md section 11).
fn never_true(expr: &ast::Expr) -> bool {
    let unwrap = unparenthesized;
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

#[cfg(test)]
mod degree_bound_tests {
    //! PL.11's bound against counted joins of random tables (falsified by a prefix counted above
    //! its bound, docs/research/planner.md section 9 rank 11), and its cap (AC-PL.11.1).
    use super::*;
    use crate::stats::StatAccum;

    /// splitmix64 (Steele, Lea and Flood, OOPSLA 2014): the same tables for the same seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
    }

    type Row = [Option<i64>; 2];
    /// An `=` term: `(table, column) = (table, column)`.
    type Term = ((usize, usize), (usize, usize));

    /// A table of two columns, its values skewed by squaring a draw over a small domain, some
    /// NULL, so degrees range from one to most of the table.
    fn table(rng: &mut Rng) -> Vec<Row> {
        let rows = rng.below(24);
        let domain = 1 + rng.below(8);
        (0..rows)
            .map(|_| {
                let mut value = || {
                    if rng.below(10) == 0 {
                        None
                    } else {
                        let d = rng.below(domain);
                        Some(((d * d) % domain) as i64)
                    }
                };
                [value(), value()]
            })
            .collect()
    }

    /// ANALYZE's ordered scan of an index on `columns`, pushed into its accumulator as
    /// `emit_index_stats` pushes it (NULLs compared equal, as its `Ne` with `null_eq` does).
    fn degrees_of(rows: &[Row], columns: &[usize]) -> Vec<crate::stats::Degrees> {
        let mut keys: Vec<Vec<Option<i64>>> = rows
            .iter()
            .map(|r| columns.iter().map(|c| r[*c]).collect())
            .collect();
        keys.sort();
        let mut accum = StatAccum::new(columns.len());
        let mut prev: Option<&Vec<Option<i64>>> = None;
        for key in &keys {
            let chng = match prev {
                None => 0,
                Some(p) => p
                    .iter()
                    .zip(key)
                    .position(|(a, b)| a != b)
                    .unwrap_or(columns.len()),
            };
            accum.push(chng);
            prev = Some(key);
        }
        let back = StatAccum::from_bytes(&accum.to_bytes()).unwrap();
        assert_eq!(back.get_degrees(), accum.get_degrees());
        accum.get_degrees()
    }

    /// The bound of `tables`, its terms `(table, column) = (table, column)`, each table with an
    /// index on each column and one on both, its count exact.
    fn bound_of(tables: &[Vec<Row>], terms: &[Term]) -> DegreeBound {
        let mut column_tables = Vec::new();
        let mut places: Vec<(usize, usize)> = Vec::new();
        let mut place = |c: (usize, usize)| {
            places.iter().position(|p| *p == c).unwrap_or_else(|| {
                places.push(c);
                column_tables.push(c.0);
                places.len() - 1
            })
        };
        let equalities: Vec<(usize, usize)> =
            terms.iter().map(|(a, b)| (place(*a), place(*b))).collect();
        let bound_tables = tables
            .iter()
            .enumerate()
            .map(|(t, rows)| {
                let mut keys = SmallVec::new();
                for index in [vec![0], vec![1], vec![0, 1]] {
                    let degrees = degrees_of(rows, &index);
                    let mut prefix: SmallVec<[usize; 4]> = SmallVec::new();
                    for (column, d) in index.iter().zip(&degrees) {
                        let Some(at) = places.iter().position(|p| *p == (t, *column)) else {
                            break;
                        };
                        prefix.push(at);
                        keys.push(BoundKey {
                            columns: prefix.clone(),
                            max: d.max as f64,
                            l2: d.l2(),
                        });
                    }
                }
                BoundTable {
                    role: Role::Inner,
                    rows: Some(rows.len() as f64),
                    keys,
                }
            })
            .collect();
        DegreeBound {
            tables: bound_tables,
            column_tables,
            equalities,
        }
    }

    /// The rows the join of the tables of `set` returns under the terms both of whose tables it
    /// holds, counted by enumerating every combination (at most 23^4).
    fn count(tables: &[Vec<Row>], terms: &[Term], set: &[usize]) -> u64 {
        let mut at = vec![0usize; set.len()];
        if set.iter().any(|t| tables[*t].is_empty()) {
            return 0;
        }
        let mut n = 0;
        loop {
            let row = |t: usize| {
                let i = set.iter().position(|s| *s == t).unwrap();
                tables[t][at[i]]
            };
            let holds = terms.iter().all(|((ta, ca), (tb, cb))| {
                if !set.contains(ta) || !set.contains(tb) {
                    return true;
                }
                matches!((row(*ta)[*ca], row(*tb)[*cb]), (Some(x), Some(y)) if x == y)
            });
            n += u64::from(holds);
            let mut i = 0;
            loop {
                if i == set.len() {
                    return n;
                }
                at[i] += 1;
                if at[i] < tables[set[i]].len() {
                    break;
                }
                at[i] = 0;
                i += 1;
            }
        }
    }

    fn mask(set: &[usize]) -> TableMask {
        let mut m = TableMask::default();
        for t in set {
            m.set(*t).unwrap();
        }
        m
    }

    /// Do: 400 seeds of two to four random tables, joined by random `=` terms (chains, stars,
    /// cycles, two terms between one pair), and count every subset's join. Expect: no count above
    /// its bound, and the bound finite for every connected set (T-PL.11.1).
    #[test]
    fn pl11_no_counted_join_exceeds_its_bound() {
        let mut checked = 0u64;
        let mut tight = 0u64;
        for seed in 0..400u64 {
            let mut rng = Rng(seed);
            let n = 2 + rng.below(3) as usize;
            let tables: Vec<Vec<Row>> = (0..n).map(|_| table(&mut rng)).collect();
            let mut terms = Vec::new();
            for t in 1..n {
                let other = rng.below(t as u64) as usize;
                terms.push(((other, rng.below(2) as usize), (t, rng.below(2) as usize)));
            }
            for _ in 0..rng.below(3) {
                let (a, b) = (rng.below(n as u64) as usize, rng.below(n as u64) as usize);
                if a != b {
                    terms.push(((a, rng.below(2) as usize), (b, rng.below(2) as usize)));
                }
            }
            let bound = bound_of(&tables, &terms);
            for subset in 1u32..(1 << n) {
                let set: Vec<usize> = (0..n).filter(|t| subset & (1 << t) != 0).collect();
                let b = bound.upper_bound(&mask(&set));
                let counted = count(&tables, &terms, &set) as f64;
                assert!(
                    counted <= b * (1.0 + 1e-12),
                    "seed {seed}: set {set:?} counted {counted} above its bound {b}; terms {terms:?}"
                );
                assert!(b.is_finite(), "seed {seed}: set {set:?} has no bound");
                checked += 1;
                tight += u64::from(counted == b);
            }
        }
        // Non-vacuity: the sets checked, and some bound met exactly (single tables at least).
        assert!(checked > 2_000, "{checked}");
        assert!(tight > 0);
    }

    /// Do: two tables of 100 and 50 rows joined on a column whose most common value holds 2 of
    /// the second's rows and 4 of the first's: the bound is min(100 * 2, 50 * 4, ℓ2 * ℓ2) = 200.
    /// Expect: an estimate of 2,000 cut to 200, one of 20 left at 20, and a set holding an outer
    /// join's table left as estimated (AC-PL.11.1).
    #[test]
    fn pl11_an_estimate_above_its_bound_is_capped_and_one_below_is_unchanged() {
        let key = |column: usize, max: f64, l2: f64| BoundKey {
            columns: smallvec::smallvec![column],
            max,
            l2,
        };
        let table = |role, rows: f64, keys| BoundTable {
            role,
            rows: Some(rows),
            keys,
        };
        let mut bound = DegreeBound {
            tables: vec![
                table(Role::Inner, 100.0, smallvec::smallvec![key(0, 4.0, 1e6)]),
                table(Role::Inner, 50.0, smallvec::smallvec![key(1, 2.0, 1e6)]),
            ],
            column_tables: vec![0, 1],
            equalities: vec![(0, 1)],
        };
        let both = mask(&[0, 1]);
        assert_eq!(bound.upper_bound(&both), 200.0);
        assert_eq!(bound.cap(&both, 2_000.0, 1.0), 200.0);
        assert_eq!(bound.cap(&both, 20.0, 1.0), 20.0);
        // For three rows of input, three times the bound.
        assert_eq!(bound.cap(&both, 2_000.0, 3.0), 600.0);
        // ℓ2 norms of 10 and 5 give Cauchy-Schwarz's 50, below either key's order.
        bound.tables[0].keys[0].l2 = 10.0;
        bound.tables[1].keys[0].l2 = 5.0;
        assert_eq!(bound.cap(&both, 2_000.0, 1.0), 50.0);
        // A term the bound may not follow (another collation or affinity): the cross product.
        bound.equalities.clear();
        assert_eq!(bound.upper_bound(&both), 5_000.0);
        bound.tables[1].role = Role::Unbounded;
        assert_eq!(bound.cap(&both, 1e9, 1.0), 1e9);
    }

    /// Do: push an index's sorted values (a: 1 1 1 2 3 3; b distinct under each a) through
    /// ANALYZE's accumulator. Expect: prefix (a) holds max 3 and squares 9 + 1 + 4 = 14, prefix
    /// (a, b) max 1 and 6, and the encoding reads back (T-PL.11.1).
    #[test]
    fn pl11_the_accumulator_keeps_each_prefixs_degree_norms() {
        let mut accum = StatAccum::new(2);
        for chng in [0, 1, 1, 0, 0, 1] {
            accum.push(chng);
        }
        let degrees = accum.get_degrees();
        assert_eq!(degrees[0].max, 3);
        assert_eq!(degrees[0].sum_sq, 14);
        assert_eq!(degrees[1].max, 1);
        assert_eq!(degrees[1].sum_sq, 6);
        let encoded = crate::stats::Degrees::encode_all(&degrees);
        assert_eq!(
            crate::stats::Degrees::decode_all(&encoded).unwrap(),
            degrees
        );
        assert!(crate::stats::Degrees::decode_all(&encoded[1..]).is_none());
        assert_eq!(accum.get_stat1(), "6 2 1");
    }
}
