//! The `${{ }}` expression language (docs/ci.md W01): GitHub-exact in the GHA dialect, with
//! kiln's additions in the kiln dialect. One implementation serves the server (job-level keys)
//! and the runner (step-level keys), so both sides agree on every value.
//!
//! This file fixes the public interface that the other workstreams build against (docs/ci.md
//! §5.3). W01 fills in the parser and evaluator behind it; until then [`Expr::parse`] keeps the
//! source without checking it, evaluation answers `KCI2000`, and [`Template::parse`] already
//! splits literal text from `${{ … }}` parts exactly, string literals included.

use std::collections::BTreeMap;
use std::sync::Arc;

/// Which language a file is written in. GitHub's (`.github/workflows`), or kiln's superset
/// (`.kiln/*.yaml`, overlays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Gha,
    Kiln,
}

/// Why parsing or evaluating failed: a stable code (`KCI2xxx`, docs/ci.md W02-R08), a message,
/// and the 1-based column in the expression's source when there is one.
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
}

/// An object's members, in insertion order. Lookup by name is case-insensitive (W01-R03); the
/// written case is kept for `toJSON`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Map(pub Vec<(Arc<str>, Value)>);

impl Map {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v)
    }
}

/// A value: JSON's types, shared cheaply. `Send + Sync`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(Arc<str>),
    Array(Arc<[Value]>),
    Object(Arc<Map>),
}

impl Value {
    pub fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => Self::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Self::String(s.as_str().into()),
            serde_json::Value::Array(a) => Self::Array(a.iter().map(Self::from_json).collect()),
            serde_json::Value::Object(o) => {
                Self::Object(Arc::new(Map(o.iter().map(|(k, v)| (Arc::from(k.as_str()), Self::from_json(v))).collect())))
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Number(n) => serde_json::Number::from_f64(*n).map_or(serde_json::Value::Null, serde_json::Value::Number),
            Self::String(s) => serde_json::Value::String(s.to_string()),
            Self::Array(a) => serde_json::Value::Array(a.iter().map(Self::to_json).collect()),
            Self::Object(o) => {
                let mut m = serde_json::Map::new();
                for (k, v) in &o.0 {
                    m.insert(k.to_string(), v.to_json());
                }
                serde_json::Value::Object(m)
            }
        }
    }

    pub fn str(s: &str) -> Self {
        Self::String(s.into())
    }

    pub fn object(members: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self::Object(Arc::new(Map(members.into_iter().map(|(k, v)| (Arc::from(k.as_str()), v)).collect())))
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
    /// A context by name (`github`, `matrix`, `steps`, …); `None` when it has no value here.
    fn get(&self, name: &str) -> Option<Value>;
    fn status(&self) -> StatusView;
    /// `hashFiles(…)`, only on the runner (W01-R09).
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

/// Plain contexts in a map, for tests and simple callers.
#[derive(Debug, Clone, Default)]
pub struct MapContexts {
    pub values: BTreeMap<String, Value>,
    pub status: StatusView,
}

impl Contexts for MapContexts {
    fn get(&self, name: &str) -> Option<Value> {
        self.values.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
    }

    fn status(&self) -> StatusView {
        self.status
    }
}

/// Bounds on one evaluation (W01-R13).
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
}

impl Expr {
    /// Parses `src` in `dialect`.
    pub fn parse(src: &str, dialect: Dialect) -> Result<Expr, ExprError> {
        if src.len() > 64 << 10 {
            return Err(ExprError::new("KCI2001", "expression longer than 64 KiB", None));
        }
        Ok(Expr { src: src.trim().to_owned(), dialect })
    }

    pub fn source(&self) -> &str {
        &self.src
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Whether it calls `success()`, `always()`, `cancelled()` or `failure()` anywhere (W01-R10).
    pub fn uses_status_function(&self) -> bool {
        let s = self.src.to_ascii_lowercase();
        ["success(", "always(", "cancelled(", "failure("].iter().any(|f| s.contains(f))
    }

    /// Every context path it reads (W01-R12).
    pub fn references(&self) -> Vec<ContextPath> {
        Vec::new()
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
    /// Splits `src` into literal text and expressions. A `}}` inside a single-quoted string
    /// literal (or, in the kiln dialect, a double-quoted one) does not close the expression; an
    /// unclosed `${{` is an error with its column.
    pub fn parse(src: &str, dialect: Dialect) -> Result<Template, ExprError> {
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut rest = src;
        let mut offset = 0usize;
        while let Some(start) = rest.find("${{") {
            text.push_str(rest.get(..start).unwrap_or_default());
            let body_start = start + 3;
            let body = rest.get(body_start..).unwrap_or_default();
            let Some(end) = find_close(body, dialect) else {
                let col = u32::try_from(offset + start + 1).unwrap_or(u32::MAX);
                return Err(ExprError::new("KCI2002", "`${{` is not closed by `}}`", Some(col)));
            };
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Expr(Expr::parse(body.get(..end).unwrap_or_default(), dialect)?));
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

    /// Exactly one expression and no literal text around it.
    pub fn single_expr(&self) -> Option<&Expr> {
        match self.parts.as_slice() {
            [Part::Expr(e)] => Some(e),
            _ => None,
        }
    }

    pub fn expressions(&self) -> impl Iterator<Item = &Expr> {
        self.parts.iter().filter_map(|p| match p {
            Part::Expr(e) => Some(e),
            Part::Text(_) => None,
        })
    }
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
                    // `''` inside a single-quoted string is an escaped quote.
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

fn unimplemented_yet() -> ExprError {
    ExprError::new("KCI2000", "expression evaluation is not implemented yet (docs/ci.md W01)", None)
}

/// Evaluates `e`.
pub fn eval(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<Value, ExprError> {
    let _ = (e, cx, b);
    Err(unimplemented_yet())
}

/// Evaluates an `if:` condition, with `success() &&` implied when no status function is called
/// (W01-R10).
pub fn eval_if(e: &Expr, cx: &dyn Contexts, b: &mut Budget) -> Result<bool, ExprError> {
    let _ = (e, cx, b);
    Err(unimplemented_yet())
}

/// Renders a template to a string (W01-R07).
pub fn render(t: &Template, cx: &dyn Contexts, b: &mut Budget) -> Result<String, ExprError> {
    let mut out = String::new();
    for p in &t.parts {
        match p {
            Part::Text(s) => out.push_str(s),
            Part::Expr(e) => {
                let _ = eval(e, cx, b)?;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_split_text_and_expressions() -> Result<(), ExprError> {
        let t = Template::parse("a ${{ x }} b ${{ 'y}}' }}", Dialect::Gha)?;
        let exprs: Vec<&str> = t.expressions().map(Expr::source).collect();
        assert_eq!(exprs, vec!["x", "'y}}'"]);
        assert!(Template::parse("plain", Dialect::Gha)?.is_literal());
        assert!(Template::parse("${{ a }}", Dialect::Gha)?.single_expr().is_some());
        let err = Template::parse("x ${{ open", Dialect::Gha).err();
        assert_eq!(err.map(|e| (e.code, e.column)), Some(("KCI2002", Some(3))));
        Ok(())
    }

    #[test]
    fn escaped_quotes_stay_inside_the_literal() -> Result<(), ExprError> {
        let t = Template::parse("${{ 'it''s }} fine' }}", Dialect::Gha)?;
        assert_eq!(t.expressions().map(Expr::source).collect::<Vec<_>>(), vec!["'it''s }} fine'"]);
        Ok(())
    }
}
