//! Amphora (docs/design/planner.md, decision 21; PL.4): a table's filter terms run over its sample
//! (decision 20), so ranges, LIKE, ORs and correlated columns are estimated together, by the share
//! of the sample's rows that pass them all. A pure core: each term is compiled once into a flat
//! program (a loop over an explicit stack, CLAUDE.md §11), then run a row at a time (§2: an
//! operation for each node of the term, on each of the sample's rows, so the work is the term's
//! length times the sample's rows, never more than a scan of the table runs the term). A term this
//! evaluator doesn't run (a function, a subquery, another table's column, a parameter with no value
//! bound) or that raises an error on a sample row is left to the planner's other estimates. A
//! parameter's value is read where the statement is planned with its run's values (a custom plan,
//! PL.12).

use crate::numeric::Numeric;
use crate::schema::Column;
use crate::translate::collate::CollationSeq;
use crate::types::compare_immutable_single;
use crate::vdbe::affinity::Affinity;
use crate::Value;
use amphora_engine_parser::ast::{self, TableInternalId};
use std::cmp::Ordering;

#[derive(Clone, Copy)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
    IsNot,
}

/// How two operands compare: the affinity applied to both first, and the collation of text.
#[derive(Clone, Copy)]
struct Compare {
    affinity: Affinity,
    collation: CollationSeq,
}

enum Op {
    Value(Value),
    /// The value of the sample's column at this place.
    Column(usize),
    Compare(Cmp, Compare),
    And,
    Or,
    Not,
    Between {
        not: bool,
        compare: Compare,
    },
    In {
        not: bool,
        values: usize,
        compare: Compare,
    },
    Like {
        not: bool,
        glob: bool,
        escape: Option<char>,
    },
    IsNull {
        not: bool,
    },
}

/// A filter term compiled for one table's sample.
pub(crate) struct Program(Vec<Op>);

/// The table a term is compiled against: its id in the query, its columns, and each column's
/// place in the sample (`None` for one the sample doesn't hold).
pub(crate) struct Target<'a> {
    pub table: TableInternalId,
    pub columns: &'a [Column],
    pub in_sample: &'a [Option<usize>],
    /// The values the statement is planned with (PL.12), where its parameters' values are read.
    pub values: Option<&'a crate::plan_cache::PlanValues>,
}

enum Visit<'a> {
    Enter(&'a ast::Expr),
    Emit(Op),
}

/// `expr` with its parentheses of one expression and its COLLATE taken off, and the collation
/// named by the outermost COLLATE, if any: at most as many as the parser lets a tree be high.
fn operand(mut expr: &ast::Expr) -> (&ast::Expr, Option<&ast::Name>) {
    let mut collate = None;
    for _ in 0..amphora_engine_parser::MAX_EXPR_DEPTH {
        match expr {
            ast::Expr::Parenthesized(exprs) => match exprs.as_slice() {
                [only] => expr = only,
                _ => break,
            },
            ast::Expr::Collate(inner, name) => {
                collate = collate.or(Some(name));
                expr = inner;
            }
            _ => break,
        }
    }
    (expr, collate)
}

impl Target<'_> {
    fn column(&self, expr: &ast::Expr) -> Option<&Column> {
        match operand(expr).0 {
            ast::Expr::Column { table, column, .. } if *table == self.table => {
                self.columns.get(*column)
            }
            _ => None,
        }
    }

    /// SQLite's `sqlite3CompareAffinity`: numeric where either side is, else the side that has
    /// one; a column has its declared affinity, any other operand none.
    fn compare(&self, lhs: &ast::Expr, rhs: &ast::Expr) -> Option<Compare> {
        let (l, r) = (self.column(lhs), self.column(rhs));
        let affinity = match (l.map(Column::affinity), r.map(Column::affinity)) {
            (Some(a), Some(b)) if a.is_numeric() || b.is_numeric() => Affinity::Numeric,
            (Some(_), Some(_)) => Affinity::Blob,
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => Affinity::Blob,
        };
        let named = operand(lhs).1.or_else(|| operand(rhs).1);
        let collation = match named {
            Some(name) => CollationSeq::new(name.as_str()).ok()?,
            None => l.or(r).map_or(CollationSeq::Binary, Column::collation),
        };
        matches!(
            collation,
            CollationSeq::Binary | CollationSeq::NoCase | CollationSeq::Rtrim
        )
        .then(|| Compare {
            affinity,
            collation,
        })
    }

    /// `expr` compiled into a program over the sample's rows, `None` where it reads anything but
    /// this table's sampled columns and constants. Each node of the term is entered once and
    /// emits at most one operation, so the loop ends within the term's nodes.
    /// A parameter with no value bound leaves the term unrun, but a term that would compile with
    /// one notes that its estimate asked for it (PL.12): a plan made with a value could differ.
    pub(crate) fn compile(&self, expr: &ast::Expr) -> Option<Program> {
        let mut parameters = false;
        let mut unbound = false;
        let mut ops = Vec::new();
        let mut stack = vec![Visit::Enter(expr)];
        while let Some(visit) = stack.pop() {
            let expr = match visit {
                Visit::Emit(op) => {
                    ops.push(op);
                    continue;
                }
                Visit::Enter(expr) => expr,
            };
            match expr {
                ast::Expr::Parenthesized(exprs) => match exprs.as_slice() {
                    [only] => stack.push(Visit::Enter(only)),
                    _ => return None,
                },
                ast::Expr::Collate(inner, _) => stack.push(Visit::Enter(inner)),
                ast::Expr::Literal(literal) => ops.push(Op::Value(literal_value(literal)?)),
                ast::Expr::Variable(variable) => {
                    let value = self.values.and_then(|values| values.value(variable));
                    parameters = true;
                    unbound |= value.is_none();
                    ops.push(Op::Value(value.cloned().unwrap_or(Value::Null)));
                }
                ast::Expr::Unary(ast::UnaryOperator::Negative, inner) => {
                    let ast::Expr::Literal(literal) = operand(inner).0 else {
                        return None;
                    };
                    let negated = match literal_value(literal)? {
                        Value::Numeric(Numeric::Integer(i)) => Value::from_i64(i.checked_neg()?),
                        Value::Numeric(Numeric::Float(f)) => Value::from_f64(-f64::from(f)),
                        _ => return None,
                    };
                    ops.push(Op::Value(negated));
                }
                ast::Expr::Unary(ast::UnaryOperator::Not, inner) => {
                    stack.push(Visit::Emit(Op::Not));
                    stack.push(Visit::Enter(inner));
                }
                ast::Expr::Column { table, column, .. } if *table == self.table => {
                    ops.push(Op::Column((*self.in_sample.get(*column)?)?));
                }
                ast::Expr::Binary(lhs, op, rhs) => {
                    let cmp = match op {
                        ast::Operator::And => None,
                        ast::Operator::Or => None,
                        ast::Operator::Equals => Some(Cmp::Eq),
                        ast::Operator::NotEquals => Some(Cmp::Ne),
                        ast::Operator::Less => Some(Cmp::Lt),
                        ast::Operator::LessEquals => Some(Cmp::Le),
                        ast::Operator::Greater => Some(Cmp::Gt),
                        ast::Operator::GreaterEquals => Some(Cmp::Ge),
                        ast::Operator::Is => Some(Cmp::Is),
                        ast::Operator::IsNot => Some(Cmp::IsNot),
                        _ => return None,
                    };
                    let op = match (cmp, op) {
                        (Some(cmp), _) => Op::Compare(cmp, self.compare(lhs, rhs)?),
                        (None, ast::Operator::And) => Op::And,
                        (None, _) => Op::Or,
                    };
                    stack.push(Visit::Emit(op));
                    stack.push(Visit::Enter(rhs));
                    stack.push(Visit::Enter(lhs));
                }
                ast::Expr::Between {
                    lhs,
                    not,
                    start,
                    end,
                } => {
                    let compare = self.compare(lhs, start)?;
                    stack.push(Visit::Emit(Op::Between { not: *not, compare }));
                    stack.push(Visit::Enter(end));
                    stack.push(Visit::Enter(start));
                    stack.push(Visit::Enter(lhs));
                }
                ast::Expr::InList { lhs, not, rhs } => {
                    let compare = self.compare(lhs, rhs.first()?)?;
                    stack.push(Visit::Emit(Op::In {
                        not: *not,
                        values: rhs.len(),
                        compare,
                    }));
                    for value in rhs.iter().rev() {
                        stack.push(Visit::Enter(value));
                    }
                    stack.push(Visit::Enter(lhs));
                }
                ast::Expr::Like {
                    lhs,
                    not,
                    op,
                    rhs,
                    escape,
                } => {
                    let glob = match op {
                        ast::LikeOperator::Like => false,
                        ast::LikeOperator::Glob => true,
                        _ => return None,
                    };
                    let escape = match escape.as_deref().map(|e| operand(e).0) {
                        None => None,
                        Some(ast::Expr::Literal(literal)) => match literal_value(literal)? {
                            Value::Text(t) => {
                                let mut chars = t.as_str().chars();
                                match (chars.next(), chars.next()) {
                                    (Some(c), None) => Some(c),
                                    _ => return None,
                                }
                            }
                            _ => return None,
                        },
                        Some(_) => return None,
                    };
                    stack.push(Visit::Emit(Op::Like {
                        not: *not,
                        glob,
                        escape,
                    }));
                    stack.push(Visit::Enter(rhs));
                    stack.push(Visit::Enter(lhs));
                }
                ast::Expr::IsNull(inner) => {
                    stack.push(Visit::Emit(Op::IsNull { not: false }));
                    stack.push(Visit::Enter(inner));
                }
                ast::Expr::NotNull(inner) => {
                    stack.push(Visit::Emit(Op::IsNull { not: true }));
                    stack.push(Visit::Enter(inner));
                }
                _ => return None,
            }
        }
        if parameters {
            if let Some(values) = self.values {
                values.note_asked();
            }
        }
        if unbound {
            return None;
        }
        Some(Program(ops))
    }
}

/// The value a literal is, `None` for one this evaluator doesn't read (a BLOB, a date keyword).
fn literal_value(literal: &ast::Literal) -> Option<Value> {
    Some(match literal {
        ast::Literal::Numeric(n) => match n.parse::<i64>() {
            Ok(i) => Value::from_i64(i),
            Err(_) => Value::from_f64(n.parse::<f64>().ok()?),
        },
        ast::Literal::String(s) => Value::build_text(crate::translate::expr::sanitize_string(s)),
        ast::Literal::Null => Value::Null,
        ast::Literal::True => Value::from_i64(1),
        ast::Literal::False => Value::from_i64(0),
        _ => return None,
    })
}

fn truth(value: Option<bool>) -> Value {
    value.map_or(Value::Null, |b| Value::from_i64(i64::from(b)))
}

fn truth_of(value: &Value) -> Option<bool> {
    Numeric::from_value(value).map(|n| n.to_bool())
}

/// `value` with `affinity` applied as a comparison applies it.
fn converted(value: Value, affinity: Affinity) -> Value {
    match affinity.convert_for_compare(&value) {
        Some(either::Either::Left(converted)) => converted.to_owned().unwrap_or(value),
        Some(either::Either::Right(converted)) => converted,
        None => value,
    }
}

/// How `l` compares to `r`, `None` where either is NULL.
fn order(l: &Value, r: &Value, compare: Compare) -> Option<Ordering> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return None;
    }
    let l = converted(l.clone(), compare.affinity);
    let r = converted(r.clone(), compare.affinity);
    Some(compare_immutable_single(&l, &r, compare.collation))
}

/// `value` as text, as LIKE and GLOB read an operand.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::Text(t) => Some(t.as_str().to_owned()),
        other => match other.exec_cast("TEXT").ok()? {
            Value::Text(t) => Some(t.as_str().to_owned()),
            _ => None,
        },
    }
}

impl Program {
    /// Whether the term reads a column, so it is a filter of its table's rows.
    pub(crate) fn reads_a_column(&self) -> bool {
        self.0.iter().any(|op| matches!(op, Op::Column(_)))
    }

    /// Whether `row` passes the term: `Some(true)` where it's true, `Some(false)` where it's false
    /// or NULL, `None` where it raised an error.
    pub(crate) fn passes(&self, row: &[Value]) -> Option<bool> {
        let mut stack: Vec<Value> = Vec::with_capacity(self.0.len());
        for op in &self.0 {
            let value =
                match op {
                    Op::Value(v) => v.clone(),
                    Op::Column(at) => row.get(*at)?.clone(),
                    Op::Compare(cmp, compare) => {
                        let r = stack.pop()?;
                        let l = stack.pop()?;
                        let nulls = (matches!(l, Value::Null), matches!(r, Value::Null));
                        truth(match (cmp, nulls) {
                            (Cmp::Is, (true, true))
                            | (Cmp::IsNot, (false, true) | (true, false)) => Some(true),
                            (Cmp::Is, (true, false) | (false, true))
                            | (Cmp::IsNot, (true, true)) => Some(false),
                            _ => order(&l, &r, *compare).map(|o| match cmp {
                                Cmp::Eq | Cmp::Is => o == Ordering::Equal,
                                Cmp::Ne | Cmp::IsNot => o != Ordering::Equal,
                                Cmp::Lt => o == Ordering::Less,
                                Cmp::Le => o != Ordering::Greater,
                                Cmp::Gt => o == Ordering::Greater,
                                Cmp::Ge => o != Ordering::Less,
                            }),
                        })
                    }
                    Op::And => {
                        let r = stack.pop()?;
                        stack.pop()?.exec_and(&r)
                    }
                    Op::Or => {
                        let r = stack.pop()?;
                        stack.pop()?.exec_or(&r)
                    }
                    Op::Not => stack.pop()?.exec_boolean_not(),
                    Op::Between { not, compare } => {
                        let end = stack.pop()?;
                        let start = stack.pop()?;
                        let lhs = stack.pop()?;
                        let ge = order(&lhs, &start, *compare).map(|o| o != Ordering::Less);
                        let le = order(&lhs, &end, *compare).map(|o| o != Ordering::Greater);
                        let within = truth(ge).exec_and(&truth(le));
                        if *not {
                            within.exec_boolean_not()
                        } else {
                            within
                        }
                    }
                    Op::In {
                        not,
                        values,
                        compare,
                    } => {
                        let at = stack.len().checked_sub(*values)?;
                        let list = stack.split_off(at);
                        let lhs = stack.pop()?;
                        let found = if matches!(lhs, Value::Null) {
                            None
                        } else if list
                            .iter()
                            .any(|v| order(&lhs, v, *compare) == Some(Ordering::Equal))
                        {
                            Some(true)
                        } else if list.iter().any(|v| matches!(v, Value::Null)) {
                            None
                        } else {
                            Some(false)
                        };
                        truth(found.map(|f| f != *not))
                    }
                    Op::Like { not, glob, escape } => {
                        let pattern = stack.pop()?;
                        let text = stack.pop()?;
                        if matches!(pattern, Value::Null) || matches!(text, Value::Null) {
                            Value::Null
                        } else {
                            let (pattern, text) = (as_text(&pattern)?, as_text(&text)?);
                            let matched = if *glob {
                                Value::exec_glob(&pattern, &text).ok()?
                            } else {
                                Value::exec_like(&pattern, &text, *escape).ok()?
                            };
                            truth(Some(matched != *not))
                        }
                    }
                    Op::IsNull { not } => truth(Some(matches!(stack.pop()?, Value::Null) != *not)),
                };
            stack.push(value);
        }
        match stack.as_slice() {
            [result] => Some(truth_of(result) == Some(true)),
            _ => None,
        }
    }
}

/// Exponential backoff (SQL Server's cardinality estimator since 2014, docs/research/planner.md
/// f17): a table's filters' selectivities, the most selective first, combine as
/// s1 * s2^(1/2) * s3^(1/4) * s4^(1/8), the rest taken for correlated with these. An added
/// selectivity s never raises the result, as CERT requires: placed k-th, it multiplies by s^(e_k),
/// and each one it moves down a place, no more selective than s, by at most s raised to the
/// exponent that one loses; those losses sum to at most e_k (decision 21).
pub(crate) fn backoff(factors: &mut [f64]) -> f64 {
    factors.sort_unstable_by(|a, b| a.total_cmp(b));
    factors
        .iter()
        .take(4)
        .zip([1.0, 0.5, 0.25, 0.125])
        .map(|(s, e)| s.clamp(0.0, 1.0).powf(e))
        .product()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: combine selectivities by backoff, then add one more of every size. Expect: the most
    /// selective counts whole and the next at its square root; an added one never raises the
    /// result (CERT; T-PL.4.1, decision 21).
    #[test]
    fn backoff_never_rises_with_an_added_selectivity() {
        assert_eq!(backoff(&mut [0.25]), 0.25);
        assert!((backoff(&mut [0.25, 0.01]) - 0.01 * 0.5).abs() < 1e-12);
        let base = [0.3, 0.05, 0.9, 0.5, 0.7];
        for n in 0..=base.len() {
            let before = backoff(&mut base[..n].to_vec());
            for added in [0.0, 0.001, 0.04, 0.2, 0.5, 0.8, 1.0] {
                let mut more = base[..n].to_vec();
                more.push(added);
                assert!(backoff(&mut more) <= before * (1.0 + 1e-12), "{n} {added}");
            }
        }
    }
}
