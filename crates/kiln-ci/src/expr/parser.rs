//! The lexer and parser, GitHub-exact (`expressions/src/lexer.ts`, `parser.ts`, `errors.ts`,
//! `funcs.ts`): the same tokens, precedence, depth accounting, limits and error messages.
//! Positions are offsets in UTF-16 code units from the start of the expression, as GitHub's
//! runner reports them (`Located at position N`, 1-based).

use super::value::{Value, js_string_to_number, js_whitespace};
use super::{Dialect, ExprError};

/// GitHub's limits (`errors.ts`).
pub const MAX_EXPRESSION_LENGTH: usize = 21_000;
pub const MAX_PARSER_DEPTH: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok {
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Comma,
    Dot,
    Bang,
    BangEqual,
    EqualEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    And,
    Or,
    Star,
    Number,
    String,
    Identifier,
    True,
    False,
    Null,
    Eof,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub kind: Tok,
    pub lexeme: String,
    pub number: f64,
    pub string: String,
    /// 0-based offset in UTF-16 code units.
    pub pos: u32,
}

impl Token {
    /// How errors name the token (`tokenString`).
    fn display(&self) -> String {
        match self.kind {
            Tok::Eof => "EOF".to_owned(),
            Tok::String => self.string.clone(),
            _ => self.lexeme.clone(),
        }
    }
}

fn err_at(code: &'static str, message: String, pos: u32) -> ExprError {
    ExprError { code, message, column: Some(pos + 1) }
}

fn is_boundary(c: char) -> bool {
    matches!(c, '(' | '[' | ')' | ']' | ',' | '.' | '!' | '>' | '<' | '=' | '&' | '|') || js_whitespace(c)
}

fn is_legal_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        _ => false,
    }
}

struct Lexer {
    chars: Vec<char>,
    /// UTF-16 offset of each char (and one past the end).
    pos16: Vec<u32>,
    start: usize,
    offset: usize,
    tokens: Vec<Token>,
    dialect: Dialect,
}

pub(crate) fn lex(src: &str, dialect: Dialect) -> Result<Vec<Token>, ExprError> {
    if src.encode_utf16().count() > MAX_EXPRESSION_LENGTH {
        return Err(ExprError { code: "KCI2001", message: format!("Exceeded max expression length {MAX_EXPRESSION_LENGTH}"), column: None });
    }
    let chars: Vec<char> = src.chars().collect();
    let mut pos16 = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0u32;
    for c in &chars {
        pos16.push(acc);
        acc += u32::try_from(c.len_utf16()).unwrap_or(1);
    }
    pos16.push(acc);
    let mut l = Lexer { chars, pos16, start: 0, offset: 0, tokens: Vec::new(), dialect };
    l.run()?;
    Ok(l.tokens)
}

impl Lexer {
    fn at_end(&self) -> bool {
        self.offset >= self.chars.len()
    }

    fn peek(&self) -> char {
        self.chars.get(self.offset).copied().unwrap_or('\0')
    }

    fn peek_next(&self) -> char {
        self.chars.get(self.offset + 1).copied().unwrap_or('\0')
    }

    fn next(&mut self) -> char {
        let c = self.peek();
        self.offset += 1;
        c
    }

    fn matches(&mut self, c: char) -> bool {
        if !self.at_end() && self.peek() == c {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn previous(&self) -> Tok {
        self.tokens.last().map_or(Tok::Eof, |t| t.kind)
    }

    fn pos(&self) -> u32 {
        self.pos16.get(self.start).copied().unwrap_or(0)
    }

    fn text(&self, from: usize, to: usize) -> String {
        self.chars.get(from..to).map(|s| s.iter().collect()).unwrap_or_default()
    }

    fn add(&mut self, kind: Tok) {
        let lexeme = self.text(self.start, self.offset);
        self.tokens.push(Token { kind, lexeme, number: 0.0, string: String::new(), pos: self.pos() });
    }

    fn unexpected(&self, lexeme: &str) -> ExprError {
        err_at("KCI2003", format!("Unexpected symbol: '{lexeme}'"), self.pos())
    }

    fn run(&mut self) -> Result<(), ExprError> {
        while !self.at_end() {
            self.start = self.offset;
            let c = self.next();
            match c {
                '(' => self.add(Tok::LeftParen),
                ')' => self.add(Tok::RightParen),
                '[' => self.add(Tok::LeftBracket),
                ']' => self.add(Tok::RightBracket),
                ',' => self.add(Tok::Comma),
                '.' => {
                    if matches!(self.previous(), Tok::Identifier | Tok::RightBracket | Tok::RightParen | Tok::Star) {
                        self.add(Tok::Dot);
                    } else {
                        self.number()?;
                    }
                }
                '-' | '+' => self.number()?,
                '!' => {
                    let k = if self.matches('=') { Tok::BangEqual } else { Tok::Bang };
                    self.add(k);
                }
                '=' => {
                    if self.matches('=') {
                        self.add(Tok::EqualEqual);
                    } else {
                        self.identifier()?;
                    }
                }
                '<' => {
                    let k = if self.matches('=') { Tok::LessEqual } else { Tok::Less };
                    self.add(k);
                }
                '>' => {
                    let k = if self.matches('=') { Tok::GreaterEqual } else { Tok::Greater };
                    self.add(k);
                }
                '&' => {
                    if self.matches('&') {
                        self.add(Tok::And);
                    } else {
                        self.identifier()?;
                    }
                }
                '|' => {
                    if self.matches('|') {
                        self.add(Tok::Or);
                    } else {
                        self.identifier()?;
                    }
                }
                '*' => self.add(Tok::Star),
                ' ' | '\r' | '\t' | '\n' => {}
                '\'' => self.string('\'')?,
                '"' if self.dialect == Dialect::Kiln => self.string('"')?,
                c if c.is_ascii_digit() => self.number()?,
                _ => self.identifier()?,
            }
        }
        // The end token sits where the last character or token started (as GitHub's does).
        self.tokens.push(Token { kind: Tok::Eof, lexeme: String::new(), number: 0.0, string: String::new(), pos: self.pos() });
        Ok(())
    }

    fn number(&mut self) -> Result<(), ExprError> {
        while !self.at_end() && (!is_boundary(self.peek()) || self.peek() == '.') {
            self.next();
        }
        let lexeme = self.text(self.start, self.offset);
        let value = js_string_to_number(&lexeme);
        if value.is_nan() {
            return Err(self.unexpected(&lexeme));
        }
        self.tokens.push(Token { kind: Tok::Number, lexeme, number: value, string: String::new(), pos: self.pos() });
        Ok(())
    }

    fn string(&mut self, quote: char) -> Result<(), ExprError> {
        while (self.peek() != quote || self.peek_next() == quote) && !self.at_end() {
            if self.peek() == quote && self.peek_next() == quote {
                self.next();
            }
            self.next();
        }
        if self.at_end() {
            let rest = self.text(self.start, self.chars.len());
            return Err(self.unexpected(&rest));
        }
        self.next();
        let inner = self.text(self.start + 1, self.offset - 1);
        let doubled: String = [quote, quote].iter().collect();
        let value = inner.replace(&doubled, &quote.to_string());
        let lexeme = self.text(self.start, self.offset);
        self.tokens.push(Token { kind: Tok::String, lexeme, number: 0.0, string: value, pos: self.pos() });
        Ok(())
    }

    fn identifier(&mut self) -> Result<(), ExprError> {
        while !self.at_end() && !is_boundary(self.peek()) {
            self.next();
        }
        let lexeme = self.text(self.start, self.offset);
        let mut kind = Tok::Identifier;
        let mut number = 0.0;
        if self.previous() != Tok::Dot {
            match lexeme.as_str() {
                "true" => kind = Tok::True,
                "false" => kind = Tok::False,
                "null" => kind = Tok::Null,
                "NaN" => {
                    kind = Tok::Number;
                    number = f64::NAN;
                }
                "Infinity" => {
                    kind = Tok::Number;
                    number = f64::INFINITY;
                }
                _ => {}
            }
        }
        if !is_legal_identifier(&lexeme) {
            return Err(self.unexpected(&lexeme));
        }
        self.tokens.push(Token { kind, lexeme, number, string: String::new(), pos: self.pos() });
        Ok(())
    }
}

// ------------------------------------------------------------------ the tree

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl BinOp {
    fn symbol(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
        }
    }
}

/// Every function the language knows (lower-case names; case-insensitive in source).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Func {
    Contains,
    StartsWith,
    EndsWith,
    Format,
    Join,
    ToJson,
    FromJson,
    Case,
    HashFiles,
    Success,
    Always,
    Cancelled,
    Failure,
    // kiln dialect (W01-R14)
    Changed,
    Matches,
    Coalesce,
    Split,
    Replace,
    Lower,
    Upper,
    Trim,
    FromYaml,
    ToYaml,
    // kiln dialect (W24-R17)
    Affected,
}

impl Func {
    pub fn lookup(name: &str) -> Option<Func> {
        Some(match name.to_ascii_lowercase().as_str() {
            "contains" => Self::Contains,
            "startswith" => Self::StartsWith,
            "endswith" => Self::EndsWith,
            "format" => Self::Format,
            "join" => Self::Join,
            "tojson" => Self::ToJson,
            "fromjson" => Self::FromJson,
            "case" => Self::Case,
            "hashfiles" => Self::HashFiles,
            "success" => Self::Success,
            "always" => Self::Always,
            "cancelled" => Self::Cancelled,
            "failure" => Self::Failure,
            "changed" => Self::Changed,
            "matches" => Self::Matches,
            "coalesce" => Self::Coalesce,
            "split" => Self::Split,
            "replace" => Self::Replace,
            "lower" => Self::Lower,
            "upper" => Self::Upper,
            "trim" => Self::Trim,
            "fromyaml" => Self::FromYaml,
            "toyaml" => Self::ToYaml,
            "affected" => Self::Affected,
            _ => return None,
        })
    }

    /// The name as documentation writes it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::StartsWith => "startsWith",
            Self::EndsWith => "endsWith",
            Self::Format => "format",
            Self::Join => "join",
            Self::ToJson => "toJSON",
            Self::FromJson => "fromJSON",
            Self::Case => "case",
            Self::HashFiles => "hashFiles",
            Self::Success => "success",
            Self::Always => "always",
            Self::Cancelled => "cancelled",
            Self::Failure => "failure",
            Self::Changed => "changed",
            Self::Matches => "matches",
            Self::Coalesce => "coalesce",
            Self::Split => "split",
            Self::Replace => "replace",
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Trim => "trim",
            Self::FromYaml => "fromYAML",
            Self::ToYaml => "toYAML",
            Self::Affected => "affected",
        }
    }

    /// (min, max) arguments.
    fn arity(self) -> (usize, usize) {
        match self {
            Self::Contains | Self::StartsWith | Self::EndsWith | Self::Matches | Self::Split => (2, 2),
            Self::Format | Self::HashFiles | Self::Changed => (1, 255),
            Self::Join => (1, 2),
            Self::ToJson | Self::FromJson | Self::Lower | Self::Upper | Self::Trim | Self::FromYaml | Self::ToYaml | Self::Affected => (1, 1),
            Self::Case => (3, usize::MAX),
            Self::Success | Self::Always | Self::Cancelled | Self::Failure => (0, 0),
            Self::Coalesce => (2, 255),
            Self::Replace => (3, 3),
        }
    }

    pub fn is_status(self) -> bool {
        matches!(self, Self::Success | Self::Always | Self::Cancelled | Self::Failure)
    }

    /// Only in the kiln dialect.
    pub fn is_kiln(self) -> bool {
        matches!(self, Self::Changed | Self::Matches | Self::Coalesce | Self::Split | Self::Replace | Self::Lower | Self::Upper | Self::Trim | Self::FromYaml | Self::ToYaml | Self::Affected)
    }

    /// Built into GitHub's expression language itself (not added by the workflow schema).
    fn is_builtin(self) -> bool {
        matches!(self, Self::Contains | Self::StartsWith | Self::EndsWith | Self::Format | Self::Join | Self::ToJson | Self::FromJson | Self::Case)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Index {
    Star,
    /// `.name`
    Prop(String),
    /// `[expr]`
    Expr(Box<Node>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Literal(Value),
    Context(String),
    Index(Box<Node>, Index),
    Not(Box<Node>),
    Binary(BinOp, Box<Node>, Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
    Group(Box<Node>),
    Call(Func, Vec<Node>),
    /// No tokens at all: evaluates to null.
    Empty,
}

/// What a parse accepts besides the language itself (GitHub's "named-values" and extension
/// functions, which depend on where the expression is: docs/ci.md Appendix B).
#[derive(Debug, Clone)]
pub struct ParseOptions {
    pub dialect: Dialect,
    /// Context names allowed; `None` accepts any name.
    pub contexts: Option<Vec<String>>,
    /// `success()`, `always()`, `cancelled()`, `failure()`.
    pub status_functions: bool,
    /// `hashFiles(…)`.
    pub hash_files: bool,
}

impl ParseOptions {
    /// Anything the dialect has: any context name, every function.
    pub fn permissive(dialect: Dialect) -> Self {
        Self { dialect, contexts: None, status_functions: true, hash_files: true }
    }
}

struct Parser<'a> {
    tokens: &'a [Token],
    offset: usize,
    depth: u32,
    opts: &'a ParseOptions,
}

pub(crate) fn parse(tokens: &[Token], opts: &ParseOptions) -> Result<Node, ExprError> {
    let mut p = Parser { tokens, offset: 0, depth: 0, opts };
    if p.at_end() {
        return Ok(Node::Empty);
    }
    let node = p.expression()?;
    if !p.at_end() {
        return Err(p.error("KCI2003", "Unexpected symbol", p.peek()));
    }
    Ok(node)
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        self.tokens.get(self.offset).or_else(|| self.tokens.last()).unwrap_or(&EOF)
    }

    fn previous(&self) -> &Token {
        self.offset.checked_sub(1).and_then(|i| self.tokens.get(i)).unwrap_or(&EOF)
    }

    fn at_end(&self) -> bool {
        self.peek().kind == Tok::Eof
    }

    fn check(&self, k: Tok) -> bool {
        !self.at_end() && self.peek().kind == k
    }

    fn next(&mut self) -> Token {
        if !self.at_end() {
            self.offset += 1;
        }
        self.previous().clone()
    }

    fn matches(&mut self, kinds: &[Tok]) -> bool {
        for k in kinds {
            if self.check(*k) {
                self.next();
                return true;
            }
        }
        false
    }

    fn consume(&mut self, k: Tok) -> Result<(), ExprError> {
        if self.check(k) {
            self.next();
            return Ok(());
        }
        Err(self.error("KCI2003", "Unexpected symbol", self.peek()))
    }

    fn error(&self, code: &'static str, what: &str, tok: &Token) -> ExprError {
        err_at(code, format!("{what}: '{}'", tok.display()), tok.pos)
    }

    fn incr(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_PARSER_DEPTH {
            return Err(self.error("KCI2012", &format!("Exceeded max expression depth {MAX_PARSER_DEPTH}"), self.peek()));
        }
        Ok(())
    }

    fn decr(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn expression(&mut self) -> Result<Node, ExprError> {
        self.incr()?;
        let r = self.logical_or();
        self.decr();
        r
    }

    fn logical_or(&mut self) -> Result<Node, ExprError> {
        let first = self.logical_and()?;
        if !self.check(Tok::Or) {
            return Ok(first);
        }
        self.incr()?;
        let mut args = vec![first];
        let r = (|| {
            while self.matches(&[Tok::Or]) {
                args.push(self.logical_and()?);
            }
            Ok::<(), ExprError>(())
        })();
        self.decr();
        r.map(|()| Node::Or(args))
    }

    fn logical_and(&mut self) -> Result<Node, ExprError> {
        let first = self.equality()?;
        if !self.check(Tok::And) {
            return Ok(first);
        }
        self.incr()?;
        let mut args = vec![first];
        let r = (|| {
            while self.matches(&[Tok::And]) {
                args.push(self.equality()?);
            }
            Ok::<(), ExprError>(())
        })();
        self.decr();
        r.map(|()| Node::And(args))
    }

    fn equality(&mut self) -> Result<Node, ExprError> {
        let mut expr = self.comparison()?;
        while self.matches(&[Tok::BangEqual, Tok::EqualEqual]) {
            let op = if self.previous().kind == Tok::BangEqual { BinOp::Ne } else { BinOp::Eq };
            let right = self.comparison()?;
            expr = Node::Binary(op, Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    fn comparison(&mut self) -> Result<Node, ExprError> {
        let mut expr = self.unary()?;
        while self.matches(&[Tok::Greater, Tok::GreaterEqual, Tok::Less, Tok::LessEqual]) {
            let op = match self.previous().kind {
                Tok::Greater => BinOp::Gt,
                Tok::GreaterEqual => BinOp::Ge,
                Tok::Less => BinOp::Lt,
                _ => BinOp::Le,
            };
            let right = self.unary()?;
            expr = Node::Binary(op, Box::new(expr), Box::new(right));
        }
        Ok(expr)
    }

    fn unary(&mut self) -> Result<Node, ExprError> {
        if self.matches(&[Tok::Bang]) {
            self.incr()?;
            let r = self.unary();
            self.decr();
            return r.map(|n| Node::Not(Box::new(n)));
        }
        self.index()
    }

    fn index(&mut self) -> Result<Node, ExprError> {
        let mut expr = self.call()?;
        if !matches!(expr, Node::Group(_) | Node::Call(..) | Node::Context(_)) {
            return Ok(expr);
        }
        let mut increased = 0u32;
        let r = (|| {
            loop {
                if self.matches(&[Tok::LeftBracket]) {
                    let idx = if self.matches(&[Tok::Star]) { Index::Star } else { Index::Expr(Box::new(self.expression()?)) };
                    self.consume(Tok::RightBracket)?;
                    self.incr()?;
                    increased += 1;
                    expr = Node::Index(Box::new(std::mem::replace(&mut expr, Node::Empty)), idx);
                } else if self.matches(&[Tok::Dot]) {
                    self.incr()?;
                    increased += 1;
                    let idx = if self.matches(&[Tok::Identifier]) {
                        Index::Prop(self.previous().lexeme.clone())
                    } else if self.matches(&[Tok::Star]) {
                        Index::Star
                    } else {
                        return Err(self.error("KCI2003", "Unexpected symbol", self.peek()));
                    };
                    expr = Node::Index(Box::new(std::mem::replace(&mut expr, Node::Empty)), idx);
                } else {
                    return Ok(());
                }
            }
        })();
        for _ in 0..increased {
            self.decr();
        }
        r.map(|()| expr)
    }

    fn call(&mut self) -> Result<Node, ExprError> {
        if !self.check(Tok::Identifier) {
            return self.primary();
        }
        let ident = self.next();
        if !self.matches(&[Tok::LeftParen]) {
            if let Some(allowed) = &self.opts.contexts
                && !allowed.iter().any(|c| c.eq_ignore_ascii_case(&ident.lexeme))
            {
                return Err(self.error("KCI3001", "Unrecognized named-value", &ident));
            }
            return Ok(Node::Context(ident.lexeme));
        }
        let mut args = Vec::new();
        while !self.matches(&[Tok::RightParen]) {
            args.push(self.expression()?);
            if !self.check(Tok::RightParen) {
                self.consume(Tok::Comma)?;
            }
        }
        let f = self.validate_function(&ident, args.len())?;
        Ok(Node::Call(f, args))
    }

    fn validate_function(&self, ident: &Token, argc: usize) -> Result<Func, ExprError> {
        let known = Func::lookup(&ident.lexeme).filter(|f| {
            f.is_builtin()
                || (f.is_status() && self.opts.status_functions)
                || (*f == Func::HashFiles && self.opts.hash_files)
                || (f.is_kiln() && self.opts.dialect == Dialect::Kiln)
        });
        let Some(f) = known else {
            if let Some(f) = Func::lookup(&ident.lexeme).filter(|f| f.is_kiln()) {
                return Err(err_at(
                    "KCI5001",
                    format!("Unrecognized function: '{}' ({}() is a kiln-dialect function: use it in .kiln/ workflows or overlays)", ident.lexeme, f.name()),
                    ident.pos,
                ));
            }
            return Err(self.error("KCI2006", "Unrecognized function", ident));
        };
        let (min, max) = f.arity();
        if argc < min {
            return Err(self.error("KCI2007", "Too few parameters supplied", ident));
        }
        if argc > max {
            return Err(self.error("KCI2008", "Too many parameters supplied", ident));
        }
        if f == Func::Case && argc.is_multiple_of(2) {
            return Err(self.error("KCI2009", "Even number of parameters supplied, requires an odd number of parameters", ident));
        }
        Ok(f)
    }

    fn primary(&mut self) -> Result<Node, ExprError> {
        if self.matches(&[Tok::False]) {
            return Ok(Node::Literal(Value::Bool(false)));
        }
        if self.matches(&[Tok::True]) {
            return Ok(Node::Literal(Value::Bool(true)));
        }
        if self.matches(&[Tok::Null]) {
            return Ok(Node::Literal(Value::Null));
        }
        if self.matches(&[Tok::Number]) {
            return Ok(Node::Literal(Value::Number(self.previous().number)));
        }
        if self.matches(&[Tok::String]) {
            return Ok(Node::Literal(Value::String(self.previous().string.as_str().into())));
        }
        if self.matches(&[Tok::LeftParen]) {
            let e = self.expression()?;
            if self.at_end() {
                return Err(self.error("KCI2004", "Unexpected end of expression", self.previous()));
            }
            self.consume(Tok::RightParen)?;
            return Ok(Node::Group(Box::new(e)));
        }
        if self.at_end() {
            return Err(self.error("KCI2004", "Unexpected end of expression", self.previous()));
        }
        Err(self.error("KCI2003", "Unexpected symbol", self.peek()))
    }
}

static EOF: Token = Token { kind: Tok::Eof, lexeme: String::new(), number: 0.0, string: String::new(), pos: 0 };

// ------------------------------------------------------------------ printing

/// Source text for a tree that parses back to the same tree (W01-AC03).
pub fn print(n: &Node) -> String {
    let mut out = String::new();
    write(n, &mut out);
    out
}

fn write(n: &Node, out: &mut String) {
    match n {
        Node::Empty => {}
        Node::Literal(v) => match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(x) => out.push_str(&super::value::js_number_to_string(*x)),
            Value::String(s) => {
                out.push('\'');
                out.push_str(&s.replace('\'', "''"));
                out.push('\'');
            }
            other => out.push_str(&super::value::json_stringify(other, 0)),
        },
        Node::Context(name) => out.push_str(name),
        Node::Index(obj, idx) => {
            write(obj, out);
            match idx {
                Index::Star => out.push_str(".*"),
                Index::Prop(p) => {
                    out.push('.');
                    out.push_str(p);
                }
                Index::Expr(e) => {
                    out.push('[');
                    write(e, out);
                    out.push(']');
                }
            }
        }
        Node::Not(e) => {
            out.push('!');
            write(e, out);
        }
        Node::Binary(op, l, r) => {
            write(l, out);
            out.push(' ');
            out.push_str(op.symbol());
            out.push(' ');
            write(r, out);
        }
        Node::And(args) | Node::Or(args) => {
            let sep = if matches!(n, Node::And(_)) { " && " } else { " || " };
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                write(a, out);
            }
        }
        Node::Group(e) => {
            out.push('(');
            write(e, out);
            out.push(')');
        }
        Node::Call(f, args) => {
            out.push_str(f.name());
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(a, out);
            }
            out.push(')');
        }
    }
}
