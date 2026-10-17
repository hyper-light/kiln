//! The `${{ }}` expression language (docs/ci.md W01): GitHub-exact in the GHA dialect, with
//! kiln's additions in the kiln dialect. One implementation serves the server (job-level keys)
//! and the runner (step-level keys), so both sides agree on every value.
//!
//! GitHub's reference is `github.com/actions/languageservices` (`expressions/`), whose test
//! vectors run against this module (`tests/oracle.rs`). Where GitHub's TypeScript and C#
//! implementations differ, kiln follows the C# runner that actually runs workflows (error
//! positions are offsets from the start of the expression; `''` escapes are all replaced).
//!
//! - [`value`]: values, coercion, comparison, JavaScript numbers, JSON.
//! - [`parser`]: tokens, the tree, limits and errors.
//! - [`eval`]: evaluation and the functions.
//! - [`files`]: the `hashFiles` function.

pub mod eval;
pub mod files;
pub mod parser;
pub mod value;

pub use parser::{Func, Node, ParseOptions};
pub use value::{Kind, Map, Value, js_number_to_string, json_parse, json_stringify};

/// Which language a file is written in: GitHub's (`.github/workflows`), or kiln's superset
/// (`.kiln/*.yaml`, overlays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Gha,
    Kiln,
}

/// Why parsing or evaluating failed: a stable code (`KCI2xxx`, `KCI3xxx`, `KCI5xxx`; docs/ci.md
/// W02-R08), GitHub's message, and the 1-based position (in UTF-16 code units from the start of
/// the expression) when there is one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ExprError {
    pub code: &'static str,
    pub message: String,
    pub column: Option<u32>,
}

impl ExprError {
    pub fn new(code: &'static str, message: impl Into<String>, column: Option<u32>) -> Self {
        Self { code, message: message.into(), column }
    }

    /// The message as GitHub prints it, with where it happened.
    pub fn located(&self, expr: &str) -> String {
        match self.column {
            Some(c) => format!("{}. Located at position {c} within expression: {expr}", self.message),
            None => format!("{}. Located within expression: {expr}", self.message),
        }
    }
}

/// One step of a context path: `secrets.NPM_TOKEN` is `[Name("secrets"), Name("NPM_TOKEN")]`;
/// `a.*.b` and computed indexes give [`PathSeg::Any`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub enum PathSeg {
    Name(String),
    Index(u64),
    Any,
}

/// A context path an expression reads (docs/ci.md W01-R12).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContextPath(pub Vec<PathSeg>);

impl ContextPath {
    /// The context it starts in (`github`, `secrets`, …), lower-cased; empty when it has none.
    pub fn root(&self) -> String {
        match self.0.first() {
            Some(PathSeg::Name(n)) => n.to_ascii_lowercase(),
            _ => String::new(),
        }
    }

    /// The name at depth `i` (0 is the context), if it is a name.
    pub fn name(&self, i: usize) -> Option<&str> {
        match self.0.get(i) {
            Some(PathSeg::Name(n)) => Some(n),
            _ => None,
        }
    }
}

/// What the status functions see (W01-R11): for a step, the earlier steps of its leg; for a job,
/// its needs, transitively.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusView {
    /// Something before (a step, or a needed job) failed.
    pub failed: bool,
    /// The run was cancelled.
    pub cancelled: bool,
}

/// Supplies contexts lazily (the `github.event` payload can be large).
pub trait Contexts {
    /// A context by name (`github`, `matrix`, `steps`, …), case-insensitive; `None` when it has
    /// no value here.
    fn get(&self, name: &str) -> Option<Value>;
    fn status(&self) -> StatusView;
    /// `hashFiles(…)`, only on the runner (W01-R09; see [`files::hash_files`]).
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError> {
        let _ = patterns;
        Err(ExprError::new("KCI2010", "hashFiles is only available on the runner", None))
    }
    /// `changed(…)`, kiln dialect (W01-R14, W03-R12).
    fn changed(&self, globs: &[String]) -> Result<bool, ExprError> {
        let _ = globs;
        Err(ExprError::new("KCI2011", "changed() needs the trigger's changed files, which are not known here", None))
    }
}

/// Plain contexts in a list, for tests and simple callers.
#[derive(Debug, Clone, Default)]
pub struct MapContexts {
    pub values: Vec<(String, Value)>,
    pub status: StatusView,
}

impl MapContexts {
    pub fn with(mut self, name: &str, v: Value) -> Self {
        self.values.push((name.to_owned(), v));
        self
    }
}

impl Contexts for MapContexts {
    fn get(&self, name: &str) -> Option<Value> {
        self.values.iter().find(|(k, _)| value::ci_eq(k, name)).map(|(_, v)| v.clone())
    }

    fn status(&self) -> StatusView {
        self.status
    }
}

/// Bounds on one evaluation (W01-R13). GitHub's own bounds (length, depth, memory) always apply.
#[derive(Debug, Clone)]
pub struct Budget {
    /// Evaluation steps left.
    pub steps: u64,
    /// Bytes of result allowed.
    pub result_bytes: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self { steps: 100_000, result_bytes: 1 << 20 }
    }
}

/// A parsed expression (the inside of `${{ }}`, or a bare `if:`).
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    src: String,
    dialect: Dialect,
    ast: Node,
}

impl Expr {
    /// Parses `src` in `dialect`, accepting any context name and every function of the dialect.
    pub fn parse(src: &str, dialect: Dialect) -> Result<Expr, ExprError> {
        Self::parse_with(src, &ParseOptions::permissive(dialect))
    }

    /// Parses `src` accepting only what `opts` allows (where a key's contexts and functions are
    /// limited, docs/ci.md Appendix B).
    pub fn parse_with(src: &str, opts: &ParseOptions) -> Result<Expr, ExprError> {
        let tokens = parser::lex(src, opts.dialect)?;
        let ast = parser::parse(&tokens, opts)?;
        Ok(Expr { src: src.trim().to_owned(), dialect: opts.dialect, ast })
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn ast(&self) -> &Node {
        &self.ast
    }

    /// Source text for this expression's tree (normalized spacing; parses to the same tree).
    pub fn print(&self) -> String {
        parser::print(&self.ast)
    }

    /// Whether it calls `success()`, `always()`, `cancelled()` or `failure()` anywhere (W01-R10).
    pub fn uses_status_function(&self) -> bool {
        self.functions().iter().any(|f| f.is_status())
    }

    /// Every function it calls.
    pub fn functions(&self) -> Vec<Func> {
        let mut out = Vec::new();
        walk(&self.ast, &mut |n| {
            if let Node::Call(f, _) = n
                && !out.contains(f)
            {
                out.push(*f);
            }
        });
        out
    }

    /// Every context path it reads (W01-R12): the longest path of each access chain, with
    /// [`PathSeg::Any`] where a filter or a computed index stands.
    pub fn references(&self) -> Vec<ContextPath> {
        let mut out = Vec::new();
        refs(&self.ast, &mut out);
        out.sort();
        out.dedup();
        out
    }
}

fn walk(n: &Node, f: &mut dyn FnMut(&Node)) {
    f(n);
    match n {
        Node::Index(o, i) => {
            walk(o, f);
            if let parser::Index::Expr(e) = i {
                walk(e, f);
            }
        }
        Node::Not(e) | Node::Group(e) => walk(e, f),
        Node::Binary(_, l, r) => {
            walk(l, f);
            walk(r, f);
        }
        Node::And(a) | Node::Or(a) | Node::Call(_, a) => {
            for x in a {
                walk(x, f);
            }
        }
        Node::Literal(_) | Node::Context(_) | Node::Empty => {}
    }
}

/// The path an access chain reads, if it starts at a context.
fn chain(n: &Node, out: &mut Vec<ContextPath>) -> Option<Vec<PathSeg>> {
    match n {
        Node::Context(name) => Some(vec![PathSeg::Name(name.clone())]),
        Node::Group(e) => chain(e, out),
        Node::Index(obj, idx) => {
            let mut p = chain(obj, out)?;
            p.push(match idx {
                parser::Index::Star => PathSeg::Any,
                parser::Index::Prop(s) => PathSeg::Name(s.clone()),
                parser::Index::Expr(e) => match e.as_ref() {
                    Node::Literal(Value::String(s)) => PathSeg::Name(s.to_string()),
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    Node::Literal(Value::Number(x)) if *x >= 0.0 && x.fract() == 0.0 && *x < 9e15 => PathSeg::Index(*x as u64),
                    other => {
                        refs(other, out);
                        PathSeg::Any
                    }
                },
            });
            Some(p)
        }
        _ => None,
    }
}

fn refs(n: &Node, out: &mut Vec<ContextPath>) {
    if let Some(p) = chain(n, out) {
        out.push(ContextPath(p));
        return;
    }
    match n {
        Node::Index(o, i) => {
            refs(o, out);
            if let parser::Index::Expr(e) = i {
                refs(e, out);
            }
        }
        Node::Not(e) | Node::Group(e) => refs(e, out),
        Node::Binary(_, l, r) => {
            refs(l, out);
            refs(r, out);
        }
        Node::And(a) | Node::Or(a) | Node::Call(_, a) => {
            for x in a {
                refs(x, out);
            }
        }
        Node::Literal(_) | Node::Context(_) | Node::Empty => {}
    }
}

/// One part of a template: literal text, or an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Expr(Expr),
}

/// A string that may hold `${{ … }}` parts (W01-R08).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub parts: Vec<Part>,
}

impl Template {
    /// Splits `src` into literal text and expressions, accepting what the dialect has.
    pub fn parse(src: &str, dialect: Dialect) -> Result<Template, ExprError> {
        Self::parse_with(src, &ParseOptions::permissive(dialect))
    }

    /// Splits `src` into literal text and expressions parsed with `opts`. A `}}` inside a
    /// string literal does not close the expression; an unclosed `${{` is an error at its column.
    pub fn parse_with(src: &str, opts: &ParseOptions) -> Result<Template, ExprError> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = src;
        let mut offset = 0usize;
        while let Some(start) = rest.find("${{") {
            text.push_str(rest.get(..start).unwrap_or_default());
            let body_start = start + 3;
            let body = rest.get(body_start..).unwrap_or_default();
            let Some(end) = find_close(body, opts.dialect) else {
                let col = u32::try_from(offset + start + 1).unwrap_or(u32::MAX);
                return Err(ExprError::new("KCI2002", "`${{` is not closed by `}}`", Some(col)));
            };
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Expr(Expr::parse_with(body.get(..end).unwrap_or_default(), opts)?));
            let consumed = body_start + end + 2;
            offset += consumed;
            rest = rest.get(consumed..).unwrap_or_default();
        }
        text.push_str(rest);
        if !text.is_empty() || parts.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Template { parts })
    }

    /// No expressions at all.
    pub fn is_literal(&self) -> bool {
        self.parts.iter().all(|p| matches!(p, Part::Text(_)))
    }

    /// Exactly one expression, with nothing but whitespace around it.
    pub fn single_expr(&self) -> Option<&Expr> {
        let mut found = None;
        for p in &self.parts {
            match p {
                Part::Expr(e) if found.is_none() => found = Some(e),
                Part::Expr(_) => return None,
                Part::Text(t) if t.trim().is_empty() => {}
                Part::Text(_) => return None,
            }
        }
        found
    }

    pub fn expressions(&self) -> impl Iterator<Item = &Expr> {
        self.parts.iter().filter_map(|p| match p {
            Part::Expr(e) => Some(e),
            Part::Text(_) => None,
        })
    }

    /// Every context path its expressions read.
    pub fn references(&self) -> Vec<ContextPath> {
        let mut out: Vec<ContextPath> = self.expressions().flat_map(Expr::references).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Where each `${{ … }}` body is in a template: (start, end) byte offsets of the text between
/// `${{` and `}}`. For mapping errors inside a template back to their place in a file.
pub fn template_spans(src: &str, dialect: Dialect) -> Result<Vec<(usize, usize)>, ExprError> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(start) = src.get(from..).and_then(|r| r.find("${{")) {
        let body_start = from + start + 3;
        let body = src.get(body_start..).unwrap_or_default();
        let Some(end) = find_close(body, dialect) else {
            return Err(ExprError::new("KCI2002", "`${{` is not closed by `}}`", Some(u32::try_from(from + start + 1).unwrap_or(u32::MAX))));
        };
        out.push((body_start, body_start + end));
        from = body_start + end + 2;
    }
    Ok(out)
}

/// The byte offset of the `}}` that closes an expression body, skipping string literals.
fn find_close(body: &str, dialect: Dialect) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while let Some(&b) = bytes.get(i) {
        match quote {
            Some(q) => {
                if b == q {
                    if bytes.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            None => {
                if b == b'\'' || (b == b'"' && dialect == Dialect::Kiln) {
                    quote = Some(b);
                } else if b == b'}' && bytes.get(i + 1) == Some(&b'}') {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// An `if:` value as GitHub reads it (W01-R10): a bare expression, a single `${{ }}`, or text
/// with expressions in it (a string, truthy when not empty: the pitfall `KCI6001` warns about).
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    Expr(Expr),
    Template(Template),
}

impl Condition {
    pub fn parse(src: &str, opts: &ParseOptions) -> Result<Condition, ExprError> {
        let t = src.trim();
        if t.contains("${{") {
            let tpl = Template::parse_with(t, opts)?;
            if let Some(e) = tpl.single_expr() {
                return Ok(Condition::Expr(e.clone()));
            }
            return Ok(Condition::Template(tpl));
        }
        Ok(Condition::Expr(Expr::parse_with(t, opts)?))
    }

    /// Literal text around an expression (`KCI6001`).
    pub fn has_literal_text(&self) -> bool {
        matches!(self, Condition::Template(_))
    }

    pub fn uses_status_function(&self) -> bool {
        match self {
            Condition::Expr(e) => e.uses_status_function(),
            Condition::Template(t) => t.expressions().any(Expr::uses_status_function),
        }
    }

    pub fn references(&self) -> Vec<ContextPath> {
        match self {
            Condition::Expr(e) => e.references(),
            Condition::Template(t) => t.references(),
        }
    }
}

/// Evaluates `e`.
pub fn eval(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<Value, ExprError> {
    let mut ev = eval::Evaluator { cx, budget: b, dialect: e.dialect, memory: 0 };
    let v = ev.eval(&e.ast)?;
    if v.mem_size() > ev.budget.result_bytes.saturating_add(64) && !matches!(v, Value::Null | Value::Bool(_) | Value::Number(_)) {
        return Err(ExprError::new("KCI2022", "the expression's result is larger than allowed", None));
    }
    Ok(v)
}

/// Evaluates an `if:` expression, with `success() &&` implied when no status function is called
/// (W01-R10).
pub fn eval_if(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<bool, ExprError> {
    if !e.uses_status_function() {
        let s = cx.status();
        if s.failed || s.cancelled {
            return Ok(false);
        }
    }
    Ok(eval(e, cx, b)?.truthy())
}

/// Evaluates an `if:` value of any form.
pub fn eval_condition(c: &Condition, cx: &dyn Contexts, b: &mut Budget) -> Result<bool, ExprError> {
    match c {
        Condition::Expr(e) => eval_if(e, cx, b),
        Condition::Template(t) => {
            if !c.uses_status_function() {
                let s = cx.status();
                if s.failed || s.cancelled {
                    return Ok(false);
                }
            }
            Ok(!render(t, cx, b)?.is_empty())
        }
    }
}

/// Renders a template to a string (W01-R07).
pub fn render(t: &Template, cx: &dyn Contexts, b: &mut Budget) -> Result<String, ExprError> {
    let mut out = String::new();
    for p in &t.parts {
        match p {
            Part::Text(s) => out.push_str(s),
            Part::Expr(e) => out.push_str(&eval(e, cx, b)?.coerce_string()),
        }
    }
    Ok(out)
}

/// A template's value: the expression's own value when the template is a single `${{ }}`
/// (so `${{ fromJSON(…) }}` gives an array where a key takes one), else the rendered string.
pub fn eval_value(t: &Template, cx: &dyn Contexts, b: &mut Budget) -> Result<Value, ExprError> {
    match t.single_expr() {
        Some(e) => eval(e, cx, b),
        None => render(t, cx, b).map(|s| Value::String(s.into())),
    }
}

#[cfg(test)]
mod tests;
