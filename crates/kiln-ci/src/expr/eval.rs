//! Evaluation, GitHub-exact (`expressions/src/evaluator.ts`, `funcs/`), with kiln's bounds:
//! a step budget and GitHub's memory bound ("The maximum allowed memory size was exceeded").

use std::sync::Arc;

use super::parser::{BinOp, Func, Index, Node};
use super::value::{Map, Value, ci_eq, equals, greater_than, json_parse, json_stringify, less_than, upper_special};
use super::{Budget, Contexts, Dialect, ExprError};

/// GitHub's memory bound for one evaluation, in bytes.
pub const MAX_MEMORY: usize = 1 << 20;

pub(crate) struct Evaluator<'a> {
    pub cx: &'a dyn Contexts,
    pub budget: &'a mut Budget,
    pub dialect: Dialect,
    pub memory: usize,
}

fn eval_err(message: impl Into<String>) -> ExprError {
    ExprError { code: "KCI2020", message: message.into(), column: None }
}

impl Evaluator<'_> {
    fn step(&mut self) -> Result<(), ExprError> {
        if self.budget.steps == 0 {
            return Err(ExprError { code: "KCI2021", message: "expression evaluation exceeded its step budget".into(), column: None });
        }
        self.budget.steps -= 1;
        Ok(())
    }

    /// Counts a value the evaluation created against the memory bound.
    fn charge(&mut self, v: &Value) -> Result<(), ExprError> {
        self.memory = self.memory.saturating_add(v.mem_size());
        if self.memory > MAX_MEMORY {
            return Err(eval_err("The maximum allowed memory size was exceeded"));
        }
        Ok(())
    }

    pub fn eval(&mut self, n: &Node) -> Result<Value, ExprError> {
        self.step()?;
        match n {
            Node::Empty => Ok(Value::Null),
            Node::Literal(v) => Ok(v.clone()),
            Node::Group(e) => self.eval(e),
            Node::Not(e) => Ok(Value::Bool(self.eval(e)?.falsy())),
            Node::Binary(op, l, r) => {
                let l = self.eval(l)?;
                let r = self.eval(r)?;
                Ok(Value::Bool(match op {
                    BinOp::Eq => equals(&l, &r),
                    BinOp::Ne => !equals(&l, &r),
                    BinOp::Gt => greater_than(&l, &r),
                    BinOp::Ge => equals(&l, &r) || greater_than(&l, &r),
                    BinOp::Lt => less_than(&l, &r),
                    BinOp::Le => equals(&l, &r) || less_than(&l, &r),
                }))
            }
            Node::And(args) => {
                let mut result = Value::Null;
                for a in args {
                    result = self.eval(a)?;
                    if result.falsy() {
                        break;
                    }
                }
                Ok(result)
            }
            Node::Or(args) => {
                let mut result = Value::Null;
                for a in args {
                    result = self.eval(a)?;
                    if result.truthy() {
                        break;
                    }
                }
                Ok(result)
            }
            Node::Context(name) => Ok(self.context(name)),
            Node::Index(obj, idx) => self.index(obj, idx),
            Node::Call(f, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a)?);
                }
                let v = self.call(*f, &vals)?;
                if matches!(v, Value::String(_) | Value::Array(_) | Value::Object(_)) && !vals.iter().any(|a| same_value(a, &v)) {
                    self.charge(&v)?;
                }
                Ok(v)
            }
        }
    }

    fn context(&self, name: &str) -> Value {
        if let Some(v) = self.cx.get(name) {
            return v;
        }
        // The v1 bare names of the kiln dialect (W01-R14).
        if self.dialect == Dialect::Kiln {
            let (ctx, key) = match name.to_ascii_lowercase().as_str() {
                "event" => ("github", "event_name"),
                "branch" => ("github", "ref_name"),
                "repo" => ("github", "repository"),
                "sha" => ("github", "sha"),
                _ => return Value::Null,
            };
            if let Some(Value::Object(o)) = self.cx.get(ctx) {
                return o.get(key).cloned().unwrap_or(Value::Null);
            }
        }
        Value::Null
    }

    fn index(&mut self, obj: &Node, idx: &Index) -> Result<Value, ExprError> {
        let key = match idx {
            Index::Star => Key::Star,
            Index::Prop(p) => Key::of(&Value::str(p)),
            Index::Expr(e) => {
                let v = self.eval(e)?;
                Key::of(&v)
            }
        };
        let target = self.eval(obj)?;
        let out = match &target {
            Value::Filtered(items) => {
                let mut result = Vec::new();
                for item in items.iter() {
                    match item {
                        Value::Object(o) => match &key {
                            Key::Star => result.extend(o.values().cloned()),
                            Key::Idx { str: Some(s), .. } => {
                                if let Some(v) = o.get(s) {
                                    result.push(v.clone());
                                }
                            }
                            Key::Idx { .. } => {}
                        },
                        Value::Array(a) | Value::Filtered(a) => match &key {
                            Key::Star => result.extend(a.iter().cloned()),
                            Key::Idx { int: Some(i), .. } => {
                                if let Some(v) = a.get(*i) {
                                    result.push(v.clone());
                                }
                            }
                            Key::Idx { .. } => {}
                        },
                        _ => {}
                    }
                }
                Value::Filtered(Arc::new(result))
            }
            Value::Array(a) => match &key {
                Key::Star => Value::Filtered(Arc::new(a.iter().cloned().collect())),
                Key::Idx { int: Some(i), .. } => a.get(*i).cloned().unwrap_or(Value::Null),
                Key::Idx { .. } => Value::Null,
            },
            Value::Object(o) => match &key {
                Key::Star => Value::Filtered(Arc::new(o.values().cloned().collect())),
                Key::Idx { str: Some(s), .. } => o.get(s).cloned().unwrap_or(Value::Null),
                Key::Idx { .. } => Value::Null,
            },
            _ => match key {
                Key::Star => Value::Filtered(Arc::new(Vec::new())),
                Key::Idx { .. } => Value::Null,
            },
        };
        if let Value::Filtered(items) = &out
            && !items.is_empty()
        {
            self.memory = self.memory.saturating_add(16 + 8 * items.len());
            if self.memory > MAX_MEMORY {
                return Err(eval_err("The maximum allowed memory size was exceeded"));
            }
        }
        Ok(out)
    }

    fn call(&mut self, f: Func, args: &[Value]) -> Result<Value, ExprError> {
        let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
        Ok(match f {
            Func::Contains => {
                let (l, r) = (arg(0), arg(1));
                let found = if l.is_primitive() {
                    r.is_primitive() && l.coerce_string().to_lowercase().contains(&r.coerce_string().to_lowercase())
                } else if let Some(items) = l.items() {
                    items.iter().any(|v| equals(&r, v))
                } else {
                    false
                };
                Value::Bool(found)
            }
            Func::StartsWith | Func::EndsWith => {
                let (l, r) = (arg(0), arg(1));
                if !l.is_primitive() || !r.is_primitive() {
                    return Ok(Value::Bool(false));
                }
                let (ls, rs) = (upper_special(&l.coerce_string()), upper_special(&r.coerce_string()));
                Value::Bool(if f == Func::StartsWith { ls.starts_with(&rs) } else { ls.ends_with(&rs) })
            }
            Func::Format => Value::String(format(args)?.into()),
            Func::Join => {
                let a = arg(0);
                if a.is_primitive() {
                    return Ok(Value::String(a.coerce_string().into()));
                }
                match a.items() {
                    Some(items) => {
                        let sep = match args.get(1) {
                            Some(s) if s.is_primitive() => s.coerce_string(),
                            _ => ",".to_owned(),
                        };
                        Value::String(items.iter().map(Value::coerce_string).collect::<Vec<_>>().join(&sep).into())
                    }
                    None => Value::str(""),
                }
            }
            Func::ToJson => Value::String(json_stringify(&arg(0), 2).into()),
            Func::FromJson => {
                let s = arg(0).coerce_string();
                if s.trim().is_empty() {
                    return Err(eval_err("empty input"));
                }
                json_parse(&s).map_err(|e| eval_err(format!("Error parsing JSON when evaluating fromJson: {e}")))?
            }
            Func::Case => {
                let mut i = 0usize;
                while i + 1 < args.len() {
                    match args.get(i) {
                        Some(Value::Bool(true)) => return Ok(arg(i + 1)),
                        Some(Value::Bool(false)) => {}
                        _ => return Err(eval_err("case predicate must evaluate to a boolean value")),
                    }
                    i += 2;
                }
                args.last().cloned().unwrap_or(Value::Null)
            }
            Func::HashFiles => {
                let patterns: Vec<String> = args.iter().map(Value::coerce_string).collect();
                Value::String(self.cx.hash_files(&patterns)?.into())
            }
            Func::Success => {
                let s = self.cx.status();
                Value::Bool(!s.failed && !s.cancelled && !s.skipped)
            }
            Func::Always => Value::Bool(true),
            Func::Cancelled => Value::Bool(self.cx.status().cancelled),
            Func::Failure => {
                let s = self.cx.status();
                Value::Bool(s.failed && !s.cancelled)
            }
            Func::Changed => {
                let globs: Vec<String> = args.iter().map(Value::coerce_string).collect();
                Value::Bool(self.cx.changed(&globs)?)
            }
            Func::Matches => {
                let re = regex::Regex::new(&arg(1).coerce_string()).map_err(|e| eval_err(format!("matches(): invalid regular expression: {e}")))?;
                Value::Bool(re.is_match(&arg(0).coerce_string()))
            }
            Func::Coalesce => {
                for a in args {
                    match a {
                        Value::Null => {}
                        Value::String(s) if s.is_empty() => {}
                        v => return Ok(v.clone()),
                    }
                }
                args.last().cloned().unwrap_or(Value::Null)
            }
            Func::Split => {
                let (s, sep) = (arg(0).coerce_string(), arg(1).coerce_string());
                let parts: Vec<Value> = if sep.is_empty() { s.chars().map(|c| Value::String(c.to_string().into())).collect() } else { s.split(sep.as_str()).map(Value::str).collect() };
                Value::array(parts)
            }
            Func::Replace => {
                let (s, from, to) = (arg(0).coerce_string(), arg(1).coerce_string(), arg(2).coerce_string());
                if from.is_empty() { Value::String(s.into()) } else { Value::String(s.replace(&from, &to).into()) }
            }
            Func::Lower => Value::String(arg(0).coerce_string().to_lowercase().into()),
            Func::Upper => Value::String(arg(0).coerce_string().to_uppercase().into()),
            Func::Trim => Value::String(arg(0).coerce_string().trim().into()),
            Func::FromYaml => {
                let s = arg(0).coerce_string();
                let y: serde_json::Value = serde_yaml::from_str(&s).map_err(|e| eval_err(format!("Error parsing YAML when evaluating fromYAML: {e}")))?;
                Value::from_json(&y)
            }
            Func::ToYaml => {
                let y = serde_yaml::to_string(&arg(0).to_json()).map_err(|e| eval_err(format!("toYAML(): {e}")))?;
                Value::String(y.into())
            }
            Func::Affected => Value::array(self.cx.affected(&arg(0).coerce_string())?.iter().map(|name| Value::str(name)).collect()),
        })
    }
}

/// Whether `b` is one of the arguments passed through unchanged (so it was not created here).
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => Arc::ptr_eq(x, y),
        (Value::Array(x) | Value::Filtered(x), Value::Array(y) | Value::Filtered(y)) => Arc::ptr_eq(x, y),
        (Value::Object(x), Value::Object(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

/// An index, as GitHub's `idxHelper` reads one: a primitive gives a string key and, when it is a
/// finite non-negative number, an integer index too.
enum Key {
    Star,
    Idx { str: Option<String>, int: Option<usize> },
}

impl Key {
    fn of(v: &Value) -> Key {
        let str = v.is_primitive().then(|| v.coerce_string());
        let f = v.number();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let int = (f.is_finite() && f >= 0.0).then(|| f.floor()).filter(|f| *f <= 9.007_199_254_740_991e15).map(|f| f as usize);
        Key::Idx { str, int }
    }
}

/// `format(fmt, …)`, GitHub-exact (`funcs/format.ts`).
fn format(args: &[Value]) -> Result<String, ExprError> {
    let fs: Vec<char> = args.first().map(Value::coerce_string).unwrap_or_default().chars().collect();
    let fs_text: String = fs.iter().collect();
    let invalid = || eval_err(format!("The following format string is invalid: {fs_text}"));
    let find = |c: char, from: usize| fs.iter().skip(from).position(|x| *x == c).map(|p| p + from);
    let slice = |a: usize, b: usize| -> String { fs.get(a..b.min(fs.len())).map(|s| s.iter().collect()).unwrap_or_default() };
    let mut out = String::new();
    let mut index = 0usize;
    while index < fs.len() {
        let lbrace = find('{', index);
        let rbrace = find('}', index);
        match (lbrace, rbrace) {
            (Some(l), r) if r.is_none_or(|r| r > l) => {
                if fs.get(l + 1) == Some(&'{') {
                    out.push_str(&slice(index, l + 1));
                    index = l + 2;
                    continue;
                }
                if let Some(r) = r
                    && r > l + 1
                {
                    let mut len = 0usize;
                    while fs.get(l + 1 + len).is_some_and(char::is_ascii_digit) {
                        len += 1;
                    }
                    if len >= 1 {
                        let n: usize = slice(l + 1, l + 1 + len).parse().unwrap_or(usize::MAX);
                        if n.saturating_add(1) > args.len().saturating_sub(1) {
                            return Err(eval_err(format!("The following format string references more arguments than were supplied: {fs_text}")));
                        }
                        if l > index {
                            out.push_str(&slice(index, l));
                        }
                        out.push_str(&args.get(n + 1).map(Value::coerce_string).unwrap_or_default());
                        index = r + 1;
                        continue;
                    }
                }
                return Err(invalid());
            }
            (_, Some(r)) => {
                if fs.get(r + 1) == Some(&'}') {
                    out.push_str(&slice(index, r + 1));
                    index = r + 2;
                } else {
                    return Err(invalid());
                }
            }
            _ => {
                out.push_str(&slice(index, fs.len()));
                break;
            }
        }
    }
    Ok(out)
}

/// A map from a list of `(name, value)` pairs, first name wins (case-insensitive).
pub fn map_of(pairs: Vec<(String, Value)>) -> Map {
    let mut m = Map::default();
    for (k, v) in pairs {
        if !m.0.iter().any(|(e, _)| ci_eq(e, &k)) {
            m.0.push((Arc::from(k.as_str()), v));
        }
    }
    m
}
