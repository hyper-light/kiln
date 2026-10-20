//! Log search (`docs/observe.md` §6): Datadog's log search syntax, parsed without recursion into an
//! arena, evaluated on a block's summary first (three-valued: a block that cannot match is never
//! decompressed) and then on its rows.
//!
//! ```text
//! service:web host:db-* status:error @http.status_code:>=500 @route:/api/* "connection reset"
//! timeout -env:dev (a OR b) NOT @user:* @latency:[100 TO 500]
//! ```
//! Juxtaposition is AND; `OR`, `AND`, `NOT` and a leading `-` as in Datadog; a bare word matches a
//! token of the message, a quoted phrase a substring of it (case-insensitively); `service`, `host`,
//! `source` and `status` (or `level`) are the record's own fields, `trace_id` and `span_id` its ids
//! (hex, as OpenTelemetry writes them), `@key` an attribute.
//!
//! Spans are searched as Datadog's trace explorer has them ([`parse_spans`]): the same, and
//! `resource_name` (or `resource`) the span's name, its message; `@duration` (ns, as stored) takes a
//! unit too (`@duration:>250ms`, `@duration:[1s TO 5s]`).

use super::block::LogBlock;
use super::bloom::{token_hashes, tokens};
use super::record::{RecordRef, severities};
use crate::index::glob;

#[derive(Debug, Clone, PartialEq)]
pub enum Text {
    Exact(String),
    Glob(String),
}

impl Text {
    fn of(v: &str) -> Self {
        if v.contains(['*', '?']) { Self::Glob(v.to_owned()) } else { Self::Exact(v.to_owned()) }
    }

    fn matches(&self, s: &str) -> bool {
        match self {
            Self::Exact(x) => x == s,
            Self::Glob(g) => glob(g, s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Service,
    Host,
    Source,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttrMatch {
    Exists,
    Text(Text),
    /// `>`, `>=`, `<`, `<=` against a number.
    Cmp(Cmp, f64),
    /// `[lo TO hi]`, inclusive.
    Range(f64, f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Leaf {
    Field(Field, Text),
    Status(u8, u8),
    Attr(String, AttrMatch),
    /// A record's trace id, or its span id.
    TraceId([u8; 16]),
    SpanId([u8; 8]),
    /// A span's name (its message), whole.
    Name(Text),
    /// A token, lowercased.
    Word(String),
    /// A phrase, lowercased.
    Phrase(String),
    /// Everything (`*` or an empty query).
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf(Leaf),
    Not(usize),
    And(usize, usize),
    Or(usize, usize),
}

/// A parsed search: nodes, children before parents.
#[derive(Debug, Clone, PartialEq)]
pub struct Search {
    pub nodes: Vec<Node>,
    pub root: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("{what} at byte {at}")]
    Syntax { at: usize, what: String },
    #[error("the search is {0} bytes (at most {1})")]
    TooLong(usize, usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Open,
    Close,
    And,
    Or,
    Not,
    Term(Leaf),
}

fn syntax(at: usize, what: impl Into<String>) -> SearchError {
    SearchError::Syntax { at, what: what.into() }
}

fn number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|x| x.is_finite())
}

/// A duration with its unit (`250ms`, `1.5s`, `2m`; none: ns), in ns.
fn duration_ns(s: &str) -> Option<f64> {
    let s = s.trim();
    let at = s.find(|c: char| c.is_ascii_alphabetic() || c == 'µ').unwrap_or(s.len());
    let (n, unit) = s.split_at(at);
    let scale = match unit {
        "" | "ns" => 1.0,
        "us" | "µs" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        "m" | "min" => 60e9,
        "h" => 3_600e9,
        _ => return None,
    };
    number(n).map(|x| x * scale)
}

/// What a value of `key` is as a number: a span's duration with its unit, else the number it says.
fn number_of(key: &str, v: &str, spans: bool) -> Option<f64> {
    if spans && key == "duration" { duration_ns(v) } else { number(v) }
}

/// An id in hex, `N` bytes of it.
fn hex_id<const N: usize>(v: &str) -> Option<[u8; N]> {
    let v = v.trim();
    if v.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(v.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// One term's leaf: `key:value`, `@key:value`, a quoted phrase, or a word; `spans`, as the trace
/// explorer reads it.
fn term(text: &str, quoted: bool, at: usize, spans: bool) -> Result<Leaf, SearchError> {
    if quoted {
        return Ok(Leaf::Phrase(text.to_lowercase()));
    }
    if text == "*" {
        return Ok(Leaf::All);
    }
    let (attr, rest) = match text.strip_prefix('@') {
        Some(r) => (true, r),
        None => (false, text),
    };
    let Some((key, value)) = rest.split_once(':') else {
        return Ok(Leaf::Word(text.to_lowercase()));
    };
    if key.is_empty() {
        return Err(syntax(at, "a key before ':'"));
    }
    if !attr {
        match key.to_ascii_lowercase().as_str() {
            "service" => return Ok(Leaf::Field(Field::Service, Text::of(value))),
            "host" => return Ok(Leaf::Field(Field::Host, Text::of(value))),
            "source" => return Ok(Leaf::Field(Field::Source, Text::of(value))),
            "status" | "level" => {
                let r = severities(value).ok_or_else(|| syntax(at, format!("no status {value:?}")))?;
                return Ok(Leaf::Status(*r.start(), *r.end()));
            }
            "trace_id" => return hex_id::<16>(value).map(Leaf::TraceId).ok_or_else(|| syntax(at, "a trace id: 32 hex digits")),
            "span_id" => return hex_id::<8>(value).map(Leaf::SpanId).ok_or_else(|| syntax(at, "a span id: 16 hex digits")),
            "resource_name" | "resource" if spans => return Ok(Leaf::Name(Text::of(value))),
            _ => {}
        }
    }
    let n = |v: &str, what: &'static str| number_of(key, v, spans).ok_or_else(|| syntax(at, what));
    let m = if value == "*" {
        AttrMatch::Exists
    } else if let Some(v) = value.strip_prefix(">=") {
        AttrMatch::Cmp(Cmp::Ge, n(v, "a number after >=")?)
    } else if let Some(v) = value.strip_prefix("<=") {
        AttrMatch::Cmp(Cmp::Le, n(v, "a number after <=")?)
    } else if let Some(v) = value.strip_prefix('>') {
        AttrMatch::Cmp(Cmp::Gt, n(v, "a number after >")?)
    } else if let Some(v) = value.strip_prefix('<') {
        AttrMatch::Cmp(Cmp::Lt, n(v, "a number after <")?)
    } else {
        AttrMatch::Text(Text::of(value))
    };
    Ok(Leaf::Attr(key.to_owned(), m))
}

/// The longest search: the request-line bound every HTTP implementation accepts (RFC 9110 §4.1).
pub const MAX_SEARCH_BYTES: usize = 8000;

fn lex(q: &str, spans: bool) -> Result<Vec<(usize, Tok)>, SearchError> {
    if q.len() > MAX_SEARCH_BYTES {
        return Err(SearchError::TooLong(q.len(), MAX_SEARCH_BYTES));
    }
    let mut out = Vec::new();
    let mut it = q.char_indices().peekable();
    while let Some(&(i, c)) = it.peek() {
        match c {
            c if c.is_whitespace() => {
                it.next();
            }
            '(' => {
                it.next();
                out.push((i, Tok::Open));
            }
            ')' => {
                it.next();
                out.push((i, Tok::Close));
            }
            // A `-` that starts a term is NOT (`timeout -env:dev`, `(a) -b`); one inside a term is
            // the term's own (`web-1`: the term's branch below reads it).
            '-' if is_term_start(&out) || q.get(i + 1..).is_some_and(|r| r.starts_with(|c: char| !c.is_whitespace())) => {
                it.next();
                out.push((i, Tok::Not));
            }
            '"' => {
                it.next();
                let mut s = String::new();
                let mut closed = false;
                while let Some((_, c)) = it.next() {
                    match c {
                        '\\' => {
                            if let Some((_, n)) = it.next() {
                                s.push(n);
                            }
                        }
                        '"' => {
                            closed = true;
                            break;
                        }
                        c => s.push(c),
                    }
                }
                if !closed {
                    return Err(syntax(i, "an unterminated quote"));
                }
                out.push((i, Tok::Term(term(&s, true, i, spans)?)));
            }
            _ => {
                // A term runs to whitespace or a bracket; `key:"quoted value"` and `[a TO b]`
                // continue it.
                let start = i;
                let mut text = String::new();
                while let Some(&(j, c)) = it.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' {
                        break;
                    }
                    if c == '"' && text.ends_with(':') {
                        // `\"` and `\\` within: a value that holds a quote, or a backslash.
                        it.next();
                        let mut closed = false;
                        while let Some((_, c)) = it.next() {
                            match c {
                                '\\' => {
                                    if let Some((_, n)) = it.next() {
                                        text.push(n);
                                    }
                                }
                                '"' => {
                                    closed = true;
                                    break;
                                }
                                c => text.push(c),
                            }
                        }
                        if !closed {
                            return Err(syntax(j, "an unterminated quote"));
                        }
                        continue;
                    }
                    if c == '[' && text.ends_with(':') {
                        // A range: up to the closing bracket, spaces included.
                        let close = q.get(j..).and_then(|r| r.find(']')).ok_or_else(|| syntax(j, "an unclosed ["))?;
                        let inner = q.get(j + 1..j + close).unwrap_or("");
                        for _ in 0..inner.chars().count() + 2 {
                            it.next();
                        }
                        let parts: Vec<&str> = inner.split_whitespace().collect();
                        let (Some(lo), Some(to), Some(hi)) = (parts.first(), parts.get(1), parts.get(2)) else { return Err(syntax(j, "a range is [a TO b]")) };
                        if !to.eq_ignore_ascii_case("to") || parts.len() != 3 {
                            return Err(syntax(j, "a range is [a TO b]"));
                        }
                        let key = text.trim_end_matches(':').trim_start_matches('@').to_owned();
                        let (lo, hi) = (number_of(&key, lo, spans).ok_or_else(|| syntax(j, "a range's numbers"))?, number_of(&key, hi, spans).ok_or_else(|| syntax(j, "a range's numbers"))?);
                        out.push((start, Tok::Term(Leaf::Attr(key, AttrMatch::Range(lo, hi)))));
                        text.clear();
                        break;
                    }
                    text.push(c);
                    it.next();
                }
                if text.is_empty() {
                    continue;
                }
                match text.as_str() {
                    "AND" => out.push((start, Tok::And)),
                    "OR" => out.push((start, Tok::Or)),
                    "NOT" => out.push((start, Tok::Not)),
                    _ => out.push((start, Tok::Term(term(&text, false, start, spans)?))),
                }
            }
        }
    }
    Ok(out)
}

fn is_term_start(out: &[(usize, Tok)]) -> bool {
    out.last().is_none_or(|(_, t)| !matches!(t, Tok::Term(_) | Tok::Close))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    And,
    Or,
    Not,
    Open(usize),
}

fn prec(op: Op) -> u8 {
    match op {
        Op::Or => 1,
        Op::And => 2,
        Op::Not => 3,
        Op::Open(_) => 0,
    }
}

/// Parses a search; an empty one matches everything.
pub fn parse(q: &str) -> Result<Search, SearchError> {
    parse_as(q, false)
}

/// Parses a search of spans, as Datadog's trace explorer reads one.
pub fn parse_spans(q: &str) -> Result<Search, SearchError> {
    parse_as(q, true)
}

fn parse_as(q: &str, spans: bool) -> Result<Search, SearchError> {
    let toks = lex(q, spans)?;
    let mut nodes: Vec<Node> = Vec::new();
    let mut vals: Vec<usize> = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    let reduce = |op: Op, nodes: &mut Vec<Node>, vals: &mut Vec<usize>, at: usize| -> Result<(), SearchError> {
        let n = match op {
            Op::Not => Node::Not(vals.pop().ok_or_else(|| syntax(at, "NOT without a term"))?),
            Op::And | Op::Or => {
                let (Some(r), Some(l)) = (vals.pop(), vals.pop()) else { return Err(syntax(at, "AND/OR without two terms")) };
                if op == Op::And { Node::And(l, r) } else { Node::Or(l, r) }
            }
            Op::Open(o) => return Err(syntax(o, "an unclosed (")),
        };
        vals.push(nodes.len());
        nodes.push(n);
        Ok(())
    };
    let mut expect_term = true;
    for (at, t) in toks {
        // Juxtaposed terms: an implicit AND.
        if !expect_term && matches!(t, Tok::Term(_) | Tok::Open | Tok::Not) {
            while let Some(&top) = ops.last() {
                if top == Op::Open(0) || matches!(top, Op::Open(_)) || prec(top) < prec(Op::And) {
                    break;
                }
                ops.pop();
                reduce(top, &mut nodes, &mut vals, at)?;
            }
            ops.push(Op::And);
            expect_term = true;
        }
        match t {
            Tok::Term(l) => {
                if !expect_term {
                    return Err(syntax(at, "a term where an operator is expected"));
                }
                vals.push(nodes.len());
                nodes.push(Node::Leaf(l));
                expect_term = false;
            }
            Tok::Not => {
                if !expect_term {
                    return Err(syntax(at, "NOT where an operator is expected"));
                }
                ops.push(Op::Not);
            }
            Tok::Open => {
                if !expect_term {
                    return Err(syntax(at, "( where an operator is expected"));
                }
                ops.push(Op::Open(at));
            }
            Tok::Close => {
                if expect_term {
                    return Err(syntax(at, ") where a term is expected"));
                }
                loop {
                    match ops.pop() {
                        Some(Op::Open(_)) => break,
                        Some(op) => reduce(op, &mut nodes, &mut vals, at)?,
                        None => return Err(syntax(at, "an unopened )")),
                    }
                }
            }
            Tok::And | Tok::Or => {
                if expect_term {
                    return Err(syntax(at, "AND/OR where a term is expected"));
                }
                let op = if t == Tok::And { Op::And } else { Op::Or };
                while let Some(&top) = ops.last() {
                    if matches!(top, Op::Open(_)) || prec(top) < prec(op) {
                        break;
                    }
                    ops.pop();
                    reduce(top, &mut nodes, &mut vals, at)?;
                }
                ops.push(op);
                expect_term = true;
            }
        }
    }
    if nodes.is_empty() {
        return Ok(Search { nodes: vec![Node::Leaf(Leaf::All)], root: 0 });
    }
    if expect_term {
        return Err(syntax(q.len(), "the search ends where a term is expected"));
    }
    while let Some(op) = ops.pop() {
        reduce(op, &mut nodes, &mut vals, q.len())?;
    }
    match vals.as_slice() {
        [root] => Ok(Search { nodes, root: *root }),
        _ => Err(syntax(0, "more than one expression")),
    }
}

/// Three-valued: whether a block matches, from its summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Maybe {
    Yes,
    No,
    Unknown,
}

/// Buffers a search keeps across rows.
#[derive(Debug, Default)]
pub struct RowScratch {
    lower: String,
    tok: String,
    vals: Vec<bool>,
}

impl Search {
    /// The search that matches every record.
    pub fn all() -> Self {
        Self { nodes: vec![Node::Leaf(Leaf::All)], root: 0 }
    }

    /// The search for one trace's spans, or records.
    pub fn trace(id: [u8; 16]) -> Self {
        Self { nodes: vec![Node::Leaf(Leaf::TraceId(id))], root: 0 }
    }

    /// Whether a block may hold a match, from its summary alone.
    pub fn block(&self, b: &LogBlock) -> Maybe {
        self.block_in(b, false)
    }

    /// As [`Search::block`], of a block of spans when `spans` (its filter holds trace ids, and not
    /// [`super::block::UNTOKENIZED`]'s values).
    pub fn block_in(&self, b: &LogBlock, spans: bool) -> Maybe {
        let mut v: Vec<Maybe> = Vec::with_capacity(self.nodes.len());
        let mut buf = String::new();
        for n in &self.nodes {
            let get = |i: usize| v.get(i).copied().unwrap_or(Maybe::Unknown);
            let m = match n {
                Node::Leaf(l) => leaf_block(l, b, &mut buf, spans),
                Node::Not(x) => match get(*x) {
                    Maybe::Yes => Maybe::No,
                    Maybe::No => Maybe::Yes,
                    Maybe::Unknown => Maybe::Unknown,
                },
                Node::And(a, c) => match (get(*a), get(*c)) {
                    (Maybe::No, _) | (_, Maybe::No) => Maybe::No,
                    (Maybe::Yes, Maybe::Yes) => Maybe::Yes,
                    _ => Maybe::Unknown,
                },
                Node::Or(a, c) => match (get(*a), get(*c)) {
                    (Maybe::Yes, _) | (_, Maybe::Yes) => Maybe::Yes,
                    (Maybe::No, Maybe::No) => Maybe::No,
                    _ => Maybe::Unknown,
                },
            };
            v.push(m);
        }
        v.get(self.root).copied().unwrap_or(Maybe::Unknown)
    }

    /// Whether a row matches.
    pub fn row(&self, r: &RecordRef<'_>, s: &mut RowScratch) -> bool {
        self.matches(r, s)
    }

    /// Whether a record not yet stored matches (a pipeline's filter's): as its row would.
    pub fn record(&self, r: &crate::logs::record::Record, s: &mut RowScratch) -> bool {
        self.matches(r, s)
    }

    /// Whether what has a record's fields matches, as a record of them would (an Error Tracking
    /// monitor's issue's errors of an env and a version).
    pub fn fields<F: Fields>(&self, r: &F, s: &mut RowScratch) -> bool {
        self.matches(r, s)
    }

    fn matches<F: Fields>(&self, r: &F, s: &mut RowScratch) -> bool {
        s.lower.clear();
        let mut lowered = false;
        s.vals.clear();
        for n in &self.nodes {
            let get = |vals: &Vec<bool>, i: usize| vals.get(i).copied().unwrap_or(false);
            let m = match n {
                Node::Leaf(l) => leaf_row(l, r, &mut s.lower, &mut lowered, &mut s.tok),
                Node::Not(x) => !get(&s.vals, *x),
                Node::And(a, c) => get(&s.vals, *a) && get(&s.vals, *c),
                Node::Or(a, c) => get(&s.vals, *a) || get(&s.vals, *c),
            };
            s.vals.push(m);
        }
        s.vals.get(self.root).copied().unwrap_or(false)
    }
}

fn dict(list: &[String], t: &Text) -> Maybe {
    let any = list.iter().any(|s| t.matches(s));
    let all = list.iter().all(|s| t.matches(s));
    if !any {
        Maybe::No
    } else if all {
        Maybe::Yes
    } else {
        Maybe::Unknown
    }
}

fn leaf_block(l: &Leaf, b: &LogBlock, buf: &mut String, spans: bool) -> Maybe {
    // A block of spans' filter is salted by the block (`super::bloom::salted`).
    let salt = spans.then(|| b.salt());
    let has = |h: (u64, u64)| b.bloom.may_contain(salt.map_or(h, |s| super::bloom::salted(h, s)));
    let all_tokens_in = |text: &str, buf: &mut String| {
        let mut ok = true;
        tokens(text, buf, |t| {
            if !has(token_hashes(t)) {
                ok = false;
            }
        });
        ok
    };
    match l {
        Leaf::All => Maybe::Yes,
        Leaf::Field(Field::Service, t) => dict(&b.services, t),
        Leaf::Field(Field::Host, t) => dict(&b.hosts, t),
        Leaf::Field(Field::Source, t) => dict(&b.sources, t),
        Leaf::Status(lo, hi) => {
            let mask: u32 = (u32::from(*lo)..=u32::from(*hi).min(24)).fold(0, |m, s| m | (1 << s));
            if b.severities & mask == 0 {
                Maybe::No
            } else if b.severities & !mask == 0 {
                Maybe::Yes
            } else {
                Maybe::Unknown
            }
        }
        Leaf::Attr(k, m) => {
            if b.attr_keys.binary_search(k).is_err() {
                return Maybe::No;
            }
            match m {
                AttrMatch::Text(Text::Exact(v)) if !(spans && super::block::UNTOKENIZED.contains(&k.as_str())) && !all_tokens_in(v, buf) => Maybe::No,
                _ => Maybe::Unknown,
            }
        }
        // A block of spans' filter holds its trace ids; one of logs', none.
        Leaf::TraceId(t) => {
            if spans && !has(super::bloom::id_hashes(t)) {
                Maybe::No
            } else {
                Maybe::Unknown
            }
        }
        Leaf::SpanId(_) => Maybe::Unknown,
        Leaf::Name(Text::Exact(n)) => {
            if all_tokens_in(n, buf) {
                Maybe::Unknown
            } else {
                Maybe::No
            }
        }
        Leaf::Name(Text::Glob(_)) => Maybe::Unknown,
        Leaf::Word(w) | Leaf::Phrase(w) => {
            if all_tokens_in(w, buf) {
                Maybe::Unknown
            } else {
                Maybe::No
            }
        }
    }
}

/// What a search reads of a record: a stored row's, or a record's not yet stored. An attribute
/// given twice is its first.
pub trait Fields {
    fn message(&self) -> &str;
    fn service(&self) -> &str;
    fn host(&self) -> &str;
    fn source(&self) -> &str;
    fn severity(&self) -> u8;
    fn attr(&self, key: &str) -> Option<&str>;
    fn trace_id(&self) -> Option<[u8; 16]>;
    fn span_id(&self) -> Option<[u8; 8]>;
}

impl Fields for RecordRef<'_> {
    fn message(&self) -> &str {
        self.message
    }
    fn service(&self) -> &str {
        self.service
    }
    fn host(&self) -> &str {
        self.host
    }
    fn source(&self) -> &str {
        self.source
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn attr(&self, key: &str) -> Option<&str> {
        RecordRef::attr(self, key)
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
}

impl Fields for crate::logs::record::Record {
    fn message(&self) -> &str {
        &self.message
    }
    fn service(&self) -> &str {
        &self.service
    }
    fn host(&self) -> &str {
        &self.host
    }
    fn source(&self) -> &str {
        &self.source
    }
    fn severity(&self) -> u8 {
        self.severity
    }
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        self.trace_id
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        self.span_id
    }
}

fn leaf_row<F: Fields>(l: &Leaf, r: &F, lower: &mut String, lowered: &mut bool, tok: &mut String) -> bool {
    let lower_msg = |lower: &mut String, lowered: &mut bool| {
        if !*lowered {
            lower.clear();
            // Most lines are ASCII: lowered a byte at a time (what `char::to_lowercase` gives
            // there), four to five times faster than a char at a time (§12.14).
            if r.message().is_ascii() {
                lower.push_str(r.message());
                lower.make_ascii_lowercase();
            } else {
                for c in r.message().chars() {
                    lower.extend(c.to_lowercase());
                }
            }
            *lowered = true;
        }
    };
    match l {
        Leaf::All => true,
        Leaf::Field(Field::Service, t) => t.matches(r.service()),
        Leaf::Field(Field::Host, t) => t.matches(r.host()),
        Leaf::Field(Field::Source, t) => t.matches(r.source()),
        Leaf::Status(lo, hi) => (*lo..=*hi).contains(&r.severity()),
        Leaf::TraceId(t) => r.trace_id() == Some(*t),
        Leaf::SpanId(s) => r.span_id() == Some(*s),
        Leaf::Name(t) => t.matches(r.message()),
        Leaf::Attr(k, m) => match r.attr(k) {
            None => false,
            Some(v) => match m {
                AttrMatch::Exists => true,
                AttrMatch::Text(t) => t.matches(v),
                AttrMatch::Cmp(c, x) => number(v).is_some_and(|n| match c {
                    Cmp::Gt => n > *x,
                    Cmp::Ge => n >= *x,
                    Cmp::Lt => n < *x,
                    Cmp::Le => n <= *x,
                }),
                AttrMatch::Range(lo, hi) => number(v).is_some_and(|n| n >= *lo && n <= *hi),
            },
        },
        Leaf::Word(w) => {
            let mut found = false;
            tokens(r.message(), tok, |t| {
                if t == w {
                    found = true;
                }
            });
            found
        }
        Leaf::Phrase(p) => {
            lower_msg(lower, lowered);
            lower.contains(p.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::block::LogBlock;
    use crate::logs::record::Record;

    fn rec(service: &str, sev: u8, msg: &str, attrs: &[(&str, &str)]) -> Record {
        Record { ts: 1, severity: sev, service: service.into(), host: "h1".into(), source: "kiln".into(), message: msg.into(), attrs: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(), trace_id: None, span_id: None }
    }

    fn matches(q: &str, r: &Record) -> bool {
        let s = parse(q).unwrap();
        let mut raw = Vec::new();
        crate::logs::record::put_row(&mut raw, r, 0);
        let mut rows = crate::logs::record::Rows::new(&raw, 1);
        let rr = rows.next_row().unwrap().unwrap();
        s.row(&rr, &mut RowScratch::default())
    }

    /// A phrase matches whatever the case, the line ASCII (lowered a byte at a time) or not (a char
    /// at a time), and the two agree where both apply.
    #[test]
    fn phrases_match_whatever_the_case_ascii_or_not() {
        let ascii = rec("web", 9, "Connection REFUSED by db-3:5432", &[]);
        let wide = rec("web", 9, "ÜBER-Fehler: Connection Refused by db-3", &[]);
        for (q, r, want) in [
            ("\"connection refused\"", &ascii, true),
            ("\"CONNECTION refused\"", &ascii, true),
            ("\"connection refused\"", &wide, true),
            ("\"über-fehler\"", &wide, true),
            ("\"ÜBER-FEHLER: connection\"", &wide, true),
            ("\"refused by db-4\"", &ascii, false),
            ("\"über\"", &ascii, false),
        ] {
            assert_eq!(matches(q, r), want, "{q} on {:?}", r.message);
        }
    }

    #[test]
    fn rows_match_as_datadog_says() {
        let r = rec("web", 17, "Connection reset by peer while reading /api/runs", &[("http.status_code", "503"), ("route", "/api/runs")]);
        for (q, want) in [
            ("service:web", true),
            ("service:api", false),
            ("service:w*", true),
            ("status:error", true),
            ("status:info", false),
            ("level:ERROR", true),
            ("@http.status_code:>=500", true),
            ("@http.status_code:<500", false),
            ("@http.status_code:[500 TO 599]", true),
            ("@route:/api/*", true),
            ("@route:*", true),
            ("@missing:*", false),
            ("reset", true),
            ("RESET", true),
            ("reseT peer", true),
            ("\"reset by peer\"", true),
            ("\"peer by reset\"", false),
            ("timeout", false),
            ("timeout OR reset", true),
            ("timeout AND reset", false),
            ("-timeout reset", true),
            ("NOT reset", false),
            ("service:web (timeout OR \"reading /api\")", true),
            ("service:web -(timeout OR \"reading /api\")", false),
            ("reset -timeout", true),
            ("reset -peer", false),
            ("service:web -status:info", true),
            ("service:web -status:error", false),
            ("(reset OR timeout) -@route:/api/jobs", true),
            ("(reset OR timeout) -@route:/api/runs", false),
            ("timeout -env:dev", false),
            ("*", true),
            ("", true),
            ("@route:\"/api/runs\"", true),
        ] {
            assert_eq!(matches(q, &r), want, "{q:?}");
        }
        // A quoted value holds anything, a quote and a backslash escaped.
        let odd = rec("my \"odd\" app\\x", 9, "m", &[("note", "say \"hi\"")]);
        for (q, want) in [
            ("service:\"my \\\"odd\\\" app\\\\x\"", true),
            ("service:\"my \\\"odd\\\" app\"", false),
            ("@note:\"say \\\"hi\\\"\"", true),
            ("@note:\"say hi\"", false),
        ] {
            assert_eq!(matches(q, &odd), want, "{q:?}");
        }
        assert!(parse("service:\"open").is_err(), "an unterminated quoted value");
    }

    #[test]
    fn blocks_are_ruled_out_by_their_summary_never_wrongly() {
        let rows: Vec<Record> = (0..300)
            .map(|i| rec(["api", "web"][i % 2], [9, 17][i % 2], &format!("request {i} handled by worker-{}", i % 7), &[("http.status_code", ["200", "503"][i % 2])]))
            .collect();
        let b = LogBlock::seal_owned(&rows);
        let mut raw = Vec::new();
        b.decompress(&mut raw).unwrap();
        for q in ["service:nope", "status:fatal", "@nokey:*", "absentword", "\"worker absentword\"", "service:api status:error", "service:api", "worker", "-worker", "service:api OR service:web", "NOT service:nope", "@http.status_code:503 worker"] {
            let s = parse(q).unwrap();
            let verdict = s.block(&b);
            let mut rows_it = b.rows(&raw);
            let mut any = false;
            let mut all = true;
            let mut scratch = RowScratch::default();
            while let Some(r) = rows_it.next_row().unwrap() {
                let m = s.row(&r, &mut scratch);
                any |= m;
                all &= m;
            }
            match verdict {
                Maybe::No => assert!(!any, "{q}: ruled out but a row matches"),
                Maybe::Yes => assert!(all, "{q}: said all match but a row does not"),
                Maybe::Unknown => {}
            }
        }
        assert_eq!(parse("service:nope").unwrap().block(&b), Maybe::No);
        assert_eq!(parse("absentword").unwrap().block(&b), Maybe::No);
        // Per-leaf three-valued logic cannot see that two halves cover a block (each is Unknown),
        // so such a block is scanned; what it can see, it says.
        assert_eq!(parse("service:api OR service:web").unwrap().block(&b), Maybe::Unknown);
        assert_eq!(parse("service:*").unwrap().block(&b), Maybe::Yes);
        assert_eq!(parse("NOT service:nope").unwrap().block(&b), Maybe::Yes);
    }

    #[test]
    fn mistakes_are_errors_and_nesting_costs_no_stack() {
        for bad in ["(a", "a)", "a AND", "OR a", "\"open", "status:nonsense", "@a:>=x", "@a:[1 TO]", ":v"] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep = format!("{}a{}", "(".repeat(3000), ")".repeat(3000));
        assert!(parse(&deep).is_ok());
        let mut rng = crate::testkit::Rng::new(5);
        let words = ["a", "b", "(", ")", "OR", "AND", "NOT", "-", "\"", "service:x", "@k:v", "@k:>1", "@k:[1 TO 2]", " ", "*"];
        for _ in 0..20_000 {
            let q: String = (0..rng.below(14)).map(|_| words[rng.below(words.len() as u64) as usize]).collect::<Vec<_>>().join(" ");
            let _ = parse(&q);
        }
    }
}
