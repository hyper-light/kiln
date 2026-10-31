use super::*;

pub fn maybe_apply_affinity(col_type: Type, target_register: usize, program: &mut ProgramBuilder) {
    if col_type == Type::Real {
        program.emit_insn(Insn::RealAffinity {
            register: target_register,
        })
    }
}

/// Sanitizes a string literal by removing single quote at front and back
/// and escaping double single quotes
pub fn sanitize_string(input: &str) -> String {
    let inner = &input[1..input.len() - 1];

    // Fast path, avoid replacing.
    if !inner.contains("''") {
        return inner.to_string();
    }

    inner.replace("''", "'")
}

/// Returns the components of a binary expression
/// e.g. t.x = 5 -> Some((t.x, =, 5))
pub fn as_binary_components(
    expr: &ast::Expr,
) -> Result<Option<(&ast::Expr, ConstraintOperator, &ast::Expr)>> {
    match unwrap_parens(expr)? {
        ast::Expr::Binary(lhs, operator, rhs)
            if matches!(
                operator,
                ast::Operator::Equals
                    | ast::Operator::NotEquals
                    | ast::Operator::Greater
                    | ast::Operator::Less
                    | ast::Operator::GreaterEquals
                    | ast::Operator::LessEquals
                    | ast::Operator::Is
                    | ast::Operator::IsNot
            ) =>
        {
            // Row-valued binary comparisons are translated directly in expression codegen.
            // They are not safe to expose as scalar binary constraints in optimizer paths.
            if expr_vector_size(lhs)? > 1 || expr_vector_size(rhs)? > 1 {
                return Ok(None);
            }
            Ok(Some((lhs.as_ref(), (*operator).into(), rhs.as_ref())))
        }
        ast::Expr::Like { lhs, not, rhs, .. } => Ok(Some((
            lhs.as_ref(),
            ConstraintOperator::Like { not: *not },
            rhs.as_ref(),
        ))),
        _ => Ok(None),
    }
}

/// Recursively unwrap parentheses from an expression
/// e.g. (((t.x > 5))) -> t.x > 5
pub fn unwrap_parens(expr: &ast::Expr) -> Result<&ast::Expr> {
    match expr {
        ast::Expr::Column { .. } => Ok(expr),
        ast::Expr::Parenthesized(exprs) => match exprs.len() {
            1 => unwrap_parens(exprs.first().unwrap()),
            _ => Ok(expr), // If the expression is e.g. (x, y), as used in e.g. (x, y) IN (SELECT ...), return as is.
        },
        _ => Ok(expr),
    }
}

/// If the right-hand side of `IS` / `IS NOT` is the TRUE or FALSE keyword —
/// possibly inside parentheses or under COLLATE — return its boolean value.
/// `x IS TRUE` and friends then test the truth of x instead of comparing
/// values. SQLite behaves the same way: it resolves the keyword after
/// skipping COLLATE, and its grammar collapses parentheses (resolve.c,
/// `sqlite3ExprSkipCollateAndLikely`). A unary `+` blocks the keyword there
/// too, so `x IS +TRUE` stays an ordinary comparison with 1.
///
/// Every place that decides "is this a truth test?" — the expression
/// evaluator, the condition emitter, and the index-seek guard in the
/// optimizer — must call this one function, so they cannot drift apart.
pub fn truth_test_rhs(expr: &ast::Expr) -> Option<bool> {
    match expr {
        ast::Expr::Literal(ast::Literal::True) => Some(true),
        ast::Expr::Literal(ast::Literal::False) => Some(false),
        ast::Expr::Parenthesized(exprs) if exprs.len() == 1 => truth_test_rhs(&exprs[0]),
        ast::Expr::Collate(inner, _) => truth_test_rhs(inner),
        _ => None,
    }
}

/// The truth SQLite's parser gives a literal of its own (EP_IsTrue and EP_IsFalse): TRUE, FALSE,
/// or an integer that fits in 32 bits, inside any parentheses; and `x ISNULL`, `x NOTNULL`,
/// `x IS NULL` and `x IS NOT NULL` with x a number, a string or a BLOB (behind unary + and -),
/// which its parser makes the integer 0 or 1 (sqlite3PExprIsNull). `None` for anything else, a
/// REAL and a negated integer among them.
pub fn literal_truth(expr: &ast::Expr) -> Option<bool> {
    let mut expr = expr;
    while let ast::Expr::Parenthesized(exprs) = expr {
        let [only] = exprs.as_slice() else {
            return None;
        };
        expr = only;
    }
    let is_constant_value = |mut operand: &ast::Expr| {
        while let ast::Expr::Unary(
            ast::UnaryOperator::Positive | ast::UnaryOperator::Negative,
            inner,
        ) = operand
        {
            operand = inner;
        }
        matches!(
            operand,
            ast::Expr::Literal(
                ast::Literal::Numeric(_) | ast::Literal::String(_) | ast::Literal::Blob(_)
            )
        )
    };
    // A NULL test the parser folds is the integer 0 or 1, so a NULL test of it folds too:
    // `(2.0 NOTNULL) NOTNULL` is 1.
    let folds = |mut operand: &ast::Expr| loop {
        while let ast::Expr::Parenthesized(exprs) = operand {
            let [only] = exprs.as_slice() else {
                return false;
            };
            operand = only;
        }
        match operand {
            ast::Expr::IsNull(inner) | ast::Expr::NotNull(inner) => operand = inner,
            ast::Expr::Binary(inner, ast::Operator::Is | ast::Operator::IsNot, null)
                if matches!(null.as_ref(), ast::Expr::Literal(ast::Literal::Null)) =>
            {
                operand = inner
            }
            _ => return is_constant_value(operand),
        }
    };
    match expr {
        ast::Expr::IsNull(operand) if folds(operand) => return Some(false),
        ast::Expr::NotNull(operand) if folds(operand) => return Some(true),
        ast::Expr::Binary(operand, op @ (ast::Operator::Is | ast::Operator::IsNot), null)
            if matches!(null.as_ref(), ast::Expr::Literal(ast::Literal::Null))
                && folds(operand) =>
        {
            return Some(matches!(op, ast::Operator::IsNot));
        }
        _ => {}
    }
    match expr {
        ast::Expr::Literal(ast::Literal::True) => Some(true),
        ast::Expr::Literal(ast::Literal::False) => Some(false),
        ast::Expr::Literal(ast::Literal::Numeric(digits)) => {
            let value = match digits
                .strip_prefix("0x")
                .or_else(|| digits.strip_prefix("0X"))
            {
                Some(hex) => i64::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<i64>().ok()?,
            };
            i32::try_from(value).ok().map(|value| value != 0)
        }
        _ => None,
    }
}

/// What SQLite's sqlite3ExprSimplifiedAndOr makes of an AND or an OR: an operand always true or
/// always false (a literal's truth, [literal_truth]) settles it, its own ANDs and ORs simplified
/// first, so `x OR (1 OR y)` is `1` and `x AND (y AND 0)` is `0`; `x AND 1` is `x`. Anything else
/// is the expression itself. What it drops is never read, so an error it would raise never comes
/// up. Children go before parents on a stack of its own (no recursion, CLAUDE.md section 11).
pub fn simplified_and_or(expr: &ast::Expr) -> &ast::Expr {
    let mut todo: Vec<(&ast::Expr, bool)> = vec![(expr, false)];
    let mut done: Vec<&ast::Expr> = Vec::new();
    while let Some((node, children_done)) = todo.pop() {
        let mut inner = node;
        while let ast::Expr::Parenthesized(exprs) = inner {
            let [only] = exprs.as_slice() else {
                break;
            };
            inner = only;
        }
        let ast::Expr::Binary(lhs, op @ (ast::Operator::And | ast::Operator::Or), rhs) = inner
        else {
            done.push(node);
            continue;
        };
        if !children_done {
            todo.push((node, true));
            todo.push((rhs, false));
            todo.push((lhs, false));
            continue;
        }
        let (Some(rhs), Some(lhs)) = (done.pop(), done.pop()) else {
            return expr;
        };
        let and = matches!(op, ast::Operator::And);
        done.push(
            if literal_truth(lhs) == Some(true) || literal_truth(rhs) == Some(false) {
                if and {
                    rhs
                } else {
                    lhs
                }
            } else if literal_truth(rhs) == Some(true) || literal_truth(lhs) == Some(false) {
                if and {
                    lhs
                } else {
                    rhs
                }
            } else {
                node
            },
        );
    }
    done.pop().unwrap_or(expr)
}

/// Whether `expr` holds a subquery anywhere in it: SQLite's EP_Subquery, which a parent takes from
/// its children and by which 3.54 orders what it computes (exprEvalRhsFirst, exprComputeOperands).
pub fn has_subquery(expr: &ast::Expr) -> bool {
    let mut found = false;
    let _ = walk_expr(expr, &mut |e: &ast::Expr| -> Result<WalkControl> {
        if matches!(
            e,
            ast::Expr::SubqueryResult { .. }
                | ast::Expr::Exists(_)
                | ast::Expr::Subquery(_)
                | ast::Expr::InSelect { .. }
        ) {
            found = true;
            return Ok(WalkControl::SkipChildren);
        }
        Ok(WalkControl::Continue)
    });
    found
}

/// Makes each AND that SQLite's parser makes the integer 0 that 0 (sqlite3ExprAnd), from the
/// innermost out: one with a side that is that 0 already, neither side calling a function. The
/// parser does it before anything reads the statement, so a name on the side it drops is never
/// resolved, nor a subquery there planned; and an ORDER BY term it makes 0 is a column number.
pub fn fold_false_ands(expr: &mut ast::Expr) -> Result<()> {
    walk_expr_mut(expr, &mut |node: &mut ast::Expr| -> Result<WalkControl> {
        if !matches!(node, ast::Expr::Binary(_, ast::Operator::And, _)) || !folds_to_zero(node) {
            return Ok(WalkControl::Continue);
        }
        *node = ast::Expr::Literal(ast::Literal::Numeric("0".to_owned()));
        Ok(WalkControl::SkipChildren)
    })?;
    Ok(())
}

/// `expr` with its ANDs folded ([fold_false_ands]) if any folds, for a caller that only reads it.
pub fn folded_false_ands(expr: &ast::Expr) -> Result<Option<ast::Expr>> {
    let mut folds = false;
    walk_expr(expr, &mut |node: &ast::Expr| -> Result<WalkControl> {
        if matches!(node, ast::Expr::Binary(_, ast::Operator::And, _)) && folds_to_zero(node) {
            folds = true;
            return Ok(WalkControl::SkipChildren);
        }
        Ok(WalkControl::Continue)
    })?;
    if !folds {
        return Ok(None);
    }
    let mut folded = expr.clone();
    fold_false_ands(&mut folded)?;
    Ok(Some(folded))
}

/// Whether SQLite's parser makes `expr` the integer 0 (sqlite3ExprAnd): an AND, inside any
/// parentheses, with a side that is that 0 (a literal 0 or a NULL test the parser folds,
/// [literal_truth], or such an AND itself) and no function called on either side (EP_HasFunc,
/// which a subquery's own calls don't give). FALSE isn't that 0: SQLite reads it as a name until
/// names are resolved. Children go before parents on a stack of its own (CLAUDE.md section 11).
fn folds_to_zero(expr: &ast::Expr) -> bool {
    let mut todo: Vec<(&ast::Expr, bool)> = vec![(expr, false)];
    let mut zero: Vec<bool> = Vec::new();
    while let Some((node, children_done)) = todo.pop() {
        let mut inner = node;
        while let ast::Expr::Parenthesized(exprs) = inner {
            let [only] = exprs.as_slice() else {
                break;
            };
            inner = only;
        }
        let ast::Expr::Binary(lhs, ast::Operator::And, rhs) = inner else {
            zero.push(
                literal_truth(inner) == Some(false)
                    && !matches!(inner, ast::Expr::Literal(ast::Literal::False)),
            );
            continue;
        };
        if !children_done {
            todo.push((node, true));
            todo.push((rhs, false));
            todo.push((lhs, false));
            continue;
        }
        let (Some(rhs_zero), Some(lhs_zero)) = (zero.pop(), zero.pop()) else {
            return false;
        };
        zero.push((lhs_zero || rhs_zero) && !calls_function(lhs) && !calls_function(rhs));
    }
    zero.pop().unwrap_or(false)
}

/// Whether `expr` calls a function outside its subqueries, as SQLite's parser sees it: LIKE,
/// GLOB, REGEXP, MATCH, -> and ->> and CURRENT_DATE, CURRENT_TIME and CURRENT_TIMESTAMP are calls
/// there, and a call under a COLLATE isn't.
fn calls_function(expr: &ast::Expr) -> bool {
    let mut found = false;
    let _ = walk_expr(expr, &mut |e: &ast::Expr| -> Result<WalkControl> {
        match e {
            ast::Expr::FunctionCall { .. }
            | ast::Expr::FunctionCallStar { .. }
            | ast::Expr::Like { .. }
            | ast::Expr::Binary(_, ast::Operator::ArrowRight | ast::Operator::ArrowRightShift, _)
            | ast::Expr::Literal(
                ast::Literal::CurrentDate
                | ast::Literal::CurrentTime
                | ast::Literal::CurrentTimestamp,
            ) => {
                found = true;
                Ok(WalkControl::SkipChildren)
            }
            ast::Expr::Exists(_) | ast::Expr::Subquery(_) => Ok(WalkControl::SkipChildren),
            // SQLite's COLLATE node takes no flag from its operand: sqlite3ExprAddCollateToken sets
            // only EP_Collate and EP_Skip (sqlite3.c 3.54.0, line 112995), so a call under a
            // COLLATE gives the AND no EP_HasFunc and it still folds.
            ast::Expr::Collate(..) => Ok(WalkControl::SkipChildren),
            _ => Ok(WalkControl::Continue),
        }
    });
    found
}

/// Recursively unwrap parentheses from an owned Expr.
/// Returns how many pairs of parentheses were removed.
pub fn unwrap_parens_owned(expr: ast::Expr) -> Result<(ast::Expr, usize)> {
    let mut paren_count = 0;
    match expr {
        ast::Expr::Parenthesized(mut exprs) => match exprs.len() {
            1 => {
                paren_count += 1;
                let (expr, count) = unwrap_parens_owned(*exprs.pop().unwrap())?;
                paren_count += count;
                Ok((expr, paren_count))
            }
            _ => crate::bail_parse_error!("expected single expression in parentheses"),
        },
        _ => Ok((expr, paren_count)),
    }
}
