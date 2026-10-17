//! YAML read into a tree that remembers where every node is (docs/ci.md W02-R03), so every
//! diagnostic points at a line and column. YAML 1.2 core schema (`on` is a string key, not a
//! boolean), anchors and aliases resolved, duplicate keys an error, and bounds against hostile
//! input: file size, nesting, node count and alias expansion.

use std::collections::HashMap;

use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};

use super::diag::{Diag, Pos, Range};

pub const MAX_FILE_BYTES: usize = 1 << 20;
const MAX_DEPTH: usize = 256;
const MAX_NODES: usize = 200_000;
const MAX_ALIAS_EXPANSION: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Scalar(Scalar, Style),
    Seq(Vec<Node>),
    Map(Vec<(Node, Node)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Plain,
    Quoted,
    Block,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: Kind,
    pub range: Range,
}

impl Node {
    pub fn as_map(&self) -> Option<&[(Node, Node)]> {
        match &self.kind {
            Kind::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_seq(&self) -> Option<&[Node]> {
        match &self.kind {
            Kind::Seq(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_scalar(&self) -> Option<&Scalar> {
        match &self.kind {
            Kind::Scalar(s, _) => Some(s),
            _ => None,
        }
    }

    /// The scalar as text (numbers and booleans as written in canonical form), `None` for
    /// sequences and mappings.
    pub fn text(&self) -> Option<String> {
        match self.as_scalar()? {
            Scalar::Null => Some(String::new()),
            Scalar::Bool(b) => Some(b.to_string()),
            Scalar::Int(i) => Some(i.to_string()),
            Scalar::Float(f) => Some(crate::expr::js_number_to_string(*f)),
            Scalar::Str(s) => Some(s.clone()),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self.kind, Kind::Scalar(Scalar::Null, _))
    }

    /// As JSON (for schema checks).
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match &self.kind {
            Kind::Scalar(Scalar::Null, _) => J::Null,
            Kind::Scalar(Scalar::Bool(b), _) => J::Bool(*b),
            Kind::Scalar(Scalar::Int(i), _) => J::from(*i),
            Kind::Scalar(Scalar::Float(f), _) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
            Kind::Scalar(Scalar::Str(s), _) => J::String(s.clone()),
            Kind::Seq(items) => J::Array(items.iter().map(Node::to_json).collect()),
            Kind::Map(items) => J::Object(items.iter().map(|(k, v)| (k.text().unwrap_or_default(), v.to_json())).collect()),
        }
    }

    /// A mapping's value for `key`.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_map()?.iter().find(|(k, _)| k.text().as_deref() == Some(key)).map(|(_, v)| v)
    }

    /// A short description of the node's type, for "expected … but found …".
    pub fn type_name(&self) -> &'static str {
        match &self.kind {
            Kind::Scalar(Scalar::Null, _) => "null",
            Kind::Scalar(Scalar::Bool(_), _) => "a boolean",
            Kind::Scalar(Scalar::Int(_) | Scalar::Float(_), _) => "a number",
            Kind::Scalar(Scalar::Str(_), _) => "a string",
            Kind::Seq(_) => "a sequence",
            Kind::Map(_) => "a mapping",
        }
    }
}

fn pos(m: Marker) -> Pos {
    Pos { line: u32::try_from(m.line()).unwrap_or(u32::MAX), col: u32::try_from(m.col()).unwrap_or(u32::MAX).saturating_add(1), offset: u32::try_from(m.index()).unwrap_or(u32::MAX) }
}

/// YAML 1.2 core schema resolution of a plain scalar.
fn resolve(s: &str) -> Scalar {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Scalar::Null,
        "true" | "True" | "TRUE" => return Scalar::Bool(true),
        "false" | "False" | "FALSE" => return Scalar::Bool(false),
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => return Scalar::Float(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => return Scalar::Float(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Scalar::Float(f64::NAN),
        _ => {}
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(i) = s.parse::<i64>() {
            return Scalar::Int(i);
        }
        if let Ok(f) = s.parse::<f64>() {
            return Scalar::Float(f);
        }
    }
    if let Some(h) = s.strip_prefix("0x")
        && !h.is_empty()
        && let Ok(i) = i64::from_str_radix(h, 16)
    {
        return Scalar::Int(i);
    }
    if let Some(o) = s.strip_prefix("0o")
        && !o.is_empty()
        && let Ok(i) = i64::from_str_radix(o, 8)
    {
        return Scalar::Int(i);
    }
    // [-+]? ( \. [0-9]+ | [0-9]+ ( \. [0-9]* )? ) ( [eE] [-+]? [0-9]+ )?
    let looks_float = {
        let b = body.as_bytes();
        let mut i = 0;
        let mut digits = 0;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
            digits += 1;
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
                digits += 1;
            }
        }
        let mut ok = digits > 0;
        if ok && matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            let e = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            ok = i > e;
        }
        ok && i == b.len()
    };
    if looks_float && let Ok(f) = s.parse::<f64>() {
        return Scalar::Float(f);
    }
    Scalar::Str(s.to_owned())
}

enum Frame {
    Seq { start: Pos, anchor: usize, items: Vec<Node> },
    Map { start: Pos, anchor: usize, items: Vec<(Node, Node)>, key: Option<Node> },
}

struct Builder<'a> {
    stack: Vec<Frame>,
    docs: Vec<Node>,
    anchors: HashMap<usize, Node>,
    diags: &'a mut Vec<Diag>,
    nodes: usize,
    expanded: usize,
    failed: bool,
}

impl Builder<'_> {
    fn push(&mut self, node: Node, anchor: usize) {
        self.nodes += 1;
        if self.nodes > MAX_NODES && !self.failed {
            self.failed = true;
            self.diags.push(Diag::error("KCI1003", format!("the file has more than {MAX_NODES} YAML nodes"), node.range));
        }
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            Some(Frame::Seq { items, .. }) => items.push(node),
            Some(Frame::Map { items, key, .. }) => match key.take() {
                None => *key = Some(node),
                Some(k) => {
                    let dup = k.text().filter(|t| items.iter().any(|(e, _)| e.text().as_deref() == Some(t)));
                    if let Some(name) = dup {
                        self.diags.push(Diag::error("KCI1002", format!("duplicate key `{name}`"), k.range));
                    } else {
                        items.push((k, node));
                    }
                }
            },
            None => self.docs.push(node),
        }
    }
}

impl MarkedEventReceiver for Builder<'_> {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        if self.failed {
            return;
        }
        let at = pos(mark);
        match ev {
            Event::Scalar(value, style, anchor, _tag) => {
                let (scalar, st) = match style {
                    TScalarStyle::Plain => (resolve(&value), Style::Plain),
                    TScalarStyle::SingleQuoted | TScalarStyle::DoubleQuoted => (Scalar::Str(value.clone()), Style::Quoted),
                    TScalarStyle::Literal | TScalarStyle::Folded => (Scalar::Str(value.clone()), Style::Block),
                };
                // A quoted scalar's text starts after its quote, so positions inside it line up.
                let at = if st == Style::Quoted { Pos { line: at.line, col: at.col.saturating_add(1), offset: at.offset.saturating_add(1) } } else { at };
                let end = Pos { line: at.line, col: at.col.saturating_add(u32::try_from(value.chars().count()).unwrap_or(0)), offset: at.offset.saturating_add(u32::try_from(value.len()).unwrap_or(0)) };
                self.push(Node { kind: Kind::Scalar(scalar, st), range: Range { start: at, end } }, anchor);
            }
            Event::SequenceStart(anchor, _) => {
                if self.stack.len() >= MAX_DEPTH {
                    self.failed = true;
                    self.diags.push(Diag::error("KCI1003", format!("YAML nested deeper than {MAX_DEPTH} levels"), Range { start: at, end: at }));
                    return;
                }
                self.stack.push(Frame::Seq { start: at, anchor, items: Vec::new() });
            }
            Event::MappingStart(anchor, _) => {
                if self.stack.len() >= MAX_DEPTH {
                    self.failed = true;
                    self.diags.push(Diag::error("KCI1003", format!("YAML nested deeper than {MAX_DEPTH} levels"), Range { start: at, end: at }));
                    return;
                }
                self.stack.push(Frame::Map { start: at, anchor, items: Vec::new(), key: None });
            }
            Event::SequenceEnd => {
                if let Some(Frame::Seq { start, anchor, items }) = self.stack.pop() {
                    self.push(Node { kind: Kind::Seq(items), range: Range { start, end: at } }, anchor);
                }
            }
            Event::MappingEnd => {
                if let Some(Frame::Map { start, anchor, items, .. }) = self.stack.pop() {
                    // A block mapping is noticed at its first `:`; it starts at its first key.
                    let start = items.first().map_or(start, |(k, _)| if k.range.start < start { k.range.start } else { start });
                    self.push(Node { kind: Kind::Map(items), range: Range { start, end: at } }, anchor);
                }
            }
            Event::Alias(id) => match self.anchors.get(&id).cloned() {
                Some(mut n) => {
                    let size = approx_size(&n);
                    self.expanded = self.expanded.saturating_add(size);
                    if self.expanded > MAX_ALIAS_EXPANSION {
                        self.failed = true;
                        self.diags.push(Diag::error("KCI1003", "aliases expand to more than 1 MiB".to_owned(), Range { start: at, end: at }));
                        return;
                    }
                    n.range = Range { start: at, end: at };
                    self.push(n, 0);
                }
                None => self.diags.push(Diag::error("KCI1001", "unknown alias".to_owned(), Range { start: at, end: at })),
            },
            Event::Nothing | Event::StreamStart | Event::StreamEnd | Event::DocumentStart | Event::DocumentEnd => {}
        }
    }
}

fn approx_size(n: &Node) -> usize {
    match &n.kind {
        Kind::Scalar(Scalar::Str(s), _) => 8 + s.len(),
        Kind::Scalar(..) => 8,
        Kind::Seq(items) => 8 + items.iter().map(approx_size).sum::<usize>(),
        Kind::Map(items) => 8 + items.iter().map(|(k, v)| approx_size(k) + approx_size(v)).sum::<usize>(),
    }
}

/// Parses one YAML document. Problems are diagnostics; the tree is `None` when there is none.
pub fn parse(src: &str, diags: &mut Vec<Diag>) -> Option<Node> {
    if src.len() > MAX_FILE_BYTES {
        diags.push(Diag::error("KCI1003", format!("the file is larger than {} KiB", MAX_FILE_BYTES >> 10), Range::default()));
        return None;
    }
    let mut b = Builder { stack: Vec::new(), docs: Vec::new(), anchors: HashMap::new(), diags, nodes: 0, expanded: 0, failed: false };
    let mut parser = Parser::new_from_str(src);
    if let Err(e) = parser.load(&mut b, true) {
        let at = pos(*e.marker());
        // Some YAML GitHub accepts is stricter in this parser (a flow sequence closed at its key's
        // indentation, for one). Read it with a lenient parser instead, without positions.
        if let Some(n) = lenient(src) {
            b.diags.push(Diag::info("KCI1004", format!("read with a lenient YAML parser ({}): positions in this file's diagnostics are approximate", e.info()), Range { start: at, end: at }));
            return Some(n);
        }
        b.diags.push(Diag::error("KCI1001", format!("YAML: {}", e.info()), Range { start: at, end: at }));
        return None;
    }
    if b.failed {
        return None;
    }
    let mut docs = std::mem::take(&mut b.docs);
    if docs.len() > 1
        && let Some(second) = docs.get(1)
    {
        b.diags.push(Diag::error("KCI1001", "a workflow file holds one YAML document".to_owned(), second.range));
    }
    if docs.is_empty() {
        return None;
    }
    Some(docs.swap_remove(0))
}

fn lenient(src: &str) -> Option<Node> {
    let v: serde_yaml::Value = serde_yaml::from_str(src).ok()?;
    Some(from_serde(&v, 0))
}

fn from_serde(v: &serde_yaml::Value, depth: usize) -> Node {
    let range = Range::default();
    if depth > MAX_DEPTH {
        return Node { kind: Kind::Scalar(Scalar::Null, Style::Plain), range };
    }
    let kind = match v {
        serde_yaml::Value::Null => Kind::Scalar(Scalar::Null, Style::Plain),
        serde_yaml::Value::Bool(b) => Kind::Scalar(Scalar::Bool(*b), Style::Plain),
        serde_yaml::Value::Number(n) => Kind::Scalar(n.as_i64().map_or_else(|| Scalar::Float(n.as_f64().unwrap_or(f64::NAN)), Scalar::Int), Style::Plain),
        serde_yaml::Value::String(s) => Kind::Scalar(Scalar::Str(s.clone()), Style::Quoted),
        serde_yaml::Value::Sequence(items) => Kind::Seq(items.iter().map(|x| from_serde(x, depth + 1)).collect()),
        serde_yaml::Value::Mapping(m) => Kind::Map(m.iter().map(|(k, x)| (from_serde(k, depth + 1), from_serde(x, depth + 1))).collect()),
        serde_yaml::Value::Tagged(t) => return from_serde(&t.value, depth),
    };
    Node { kind, range }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_types_and_on() {
        let mut d = Vec::new();
        let n = parse("on: push\nname: ci\njobs:\n  a:\n    runs-on: [x, 'y']\n    n: 3\n    f: 1.5\n    t: true\n    s: \"true\"\n", &mut d);
        assert!(d.is_empty(), "{d:?}");
        let Some(n) = n else { return };
        assert_eq!(n.get("on").and_then(Node::text).as_deref(), Some("push"), "`on` is a string key");
        let job = n.get("jobs").and_then(|j| j.get("a"));
        let runs = job.and_then(|j| j.get("runs-on"));
        assert_eq!(runs.map(|r| (r.range.start.line, r.range.start.col)), Some((5, 14)));
        assert_eq!(job.and_then(|j| j.get("n")).and_then(Node::as_scalar), Some(&Scalar::Int(3)));
        assert_eq!(job.and_then(|j| j.get("f")).and_then(Node::as_scalar), Some(&Scalar::Float(1.5)));
        assert_eq!(job.and_then(|j| j.get("t")).and_then(Node::as_scalar), Some(&Scalar::Bool(true)));
        assert_eq!(job.and_then(|j| j.get("s")).and_then(Node::as_scalar), Some(&Scalar::Str("true".into())));
    }

    #[test]
    fn duplicates_aliases_and_bombs() {
        let mut d = Vec::new();
        let _ = parse("a: 1\nb: 2\na: 3\n", &mut d);
        assert_eq!(d.first().map(|x| (x.code, x.range.start.line)), Some(("KCI1002", 3)));
        let mut d = Vec::new();
        let n = parse("x: &v {k: 1}\ny: *v\n", &mut d);
        assert!(d.is_empty());
        assert_eq!(n.and_then(|n| n.get("y").and_then(|y| y.get("k")).and_then(Node::text)).as_deref(), Some("1"));
        let mut bomb = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 0..12 {
            bomb.push_str(&format!("b{i}: &b{i} [*{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}, *{p}]\n", p = if i == 0 { "a".to_owned() } else { format!("b{}", i - 1) }));
        }
        let mut d = Vec::new();
        assert!(parse(&bomb, &mut d).is_none());
        assert_eq!(d.first().map(|x| x.code), Some("KCI1003"));
        let mut d = Vec::new();
        assert!(parse("a: [1, 2\n", &mut d).is_none());
        assert_eq!(d.first().map(|x| x.code), Some("KCI1001"));
    }
}
