//! GraphQL documents (docs/ci.md W14-R11): the facade's GraphQL takes the queries and mutations
//! GitHub's clients send (`gh`, and the actions that ask GitHub's GraphQL API). This parses an
//! executable document (operations and fragments, with variables, aliases, arguments, directives,
//! fragment spreads and inline fragments) and collects the fields an object answers, as the
//! specification's CollectFields does (October 2021 edition, §6.3.2). The kiln crate resolves them.

use serde_json::{Map, Value as Json};

/// The deepest a selection set or a value may nest: a document past it is refused.
const DEEPEST: usize = 64;

/// A value in a document: a constant, or a variable's.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Variable(String),
    Int(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
    Enum(String),
    List(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub name: String,
    pub args: Vec<(String, Value)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub alias: Option<String>,
    pub name: String,
    pub args: Vec<(String, Value)>,
    pub directives: Vec<Directive>,
    pub selection: Vec<Selection>,
}

impl Field {
    /// The key its value goes under in the response.
    pub fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Field(Field),
    Spread { name: String, directives: Vec<Directive> },
    Inline { on: Option<String>, directives: Vec<Directive>, selection: Vec<Selection> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Query,
    Mutation,
    Subscription,
}

/// A variable an operation declares.
#[derive(Debug, Clone, PartialEq)]
pub struct VarDef {
    pub name: String,
    /// As written: `String!`, `[ID!]`.
    pub ty: String,
    pub default: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    pub kind: Kind,
    pub name: Option<String>,
    pub vars: Vec<VarDef>,
    pub directives: Vec<Directive>,
    pub selection: Vec<Selection>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
    pub name: String,
    pub on: String,
    pub directives: Vec<Directive>,
    pub selection: Vec<Selection>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    pub operations: Vec<Operation>,
    pub fragments: Vec<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Punct(char),
    Spread,
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
}

/// The tokens of `src`, each with its line and column; commas, white space and comments go.
fn lex(src: &str) -> Result<Vec<(Tok, usize, usize)>, String> {
    let chars: Vec<char> = src.chars().collect();
    let (mut i, mut line, mut col) = (0usize, 1usize, 1usize);
    let mut out = Vec::new();
    let at = |i: usize| chars.get(i).copied();
    macro_rules! bump {
        ($n:expr) => {
            for _ in 0..$n {
                if at(i) == Some('\n') {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                i += 1;
            }
        };
    }
    while let Some(c) = at(i) {
        let (l, cl) = (line, col);
        match c {
            '\u{feff}' | ' ' | '\t' | '\n' | '\r' | ',' => bump!(1),
            '#' => {
                while at(i).is_some_and(|c| c != '\n' && c != '\r') {
                    bump!(1);
                }
            }
            '!' | '$' | '&' | '(' | ')' | ':' | '=' | '@' | '[' | ']' | '{' | '|' | '}' => {
                out.push((Tok::Punct(c), l, cl));
                bump!(1);
            }
            '.' => {
                if at(i + 1) == Some('.') && at(i + 2) == Some('.') {
                    out.push((Tok::Spread, l, cl));
                    bump!(3);
                } else {
                    return Err(format!("{l}:{cl}: `.` where `...` belongs"));
                }
            }
            '"' => {
                if at(i + 1) == Some('"') && at(i + 2) == Some('"') {
                    bump!(3);
                    let mut raw = String::new();
                    loop {
                        match at(i) {
                            None => return Err(format!("{l}:{cl}: a block string isn't closed")),
                            Some('"') if at(i + 1) == Some('"') && at(i + 2) == Some('"') => {
                                bump!(3);
                                break;
                            }
                            Some('\\') if at(i + 1) == Some('"') && at(i + 2) == Some('"') && at(i + 3) == Some('"') => {
                                raw.push_str("\"\"\"");
                                bump!(4);
                            }
                            Some(ch) => {
                                raw.push(ch);
                                bump!(1);
                            }
                        }
                    }
                    out.push((Tok::Str(block_string(&raw)), l, cl));
                } else {
                    bump!(1);
                    let mut s = String::new();
                    loop {
                        match at(i) {
                            None | Some('\n') | Some('\r') => return Err(format!("{l}:{cl}: a string isn't closed")),
                            Some('"') => {
                                bump!(1);
                                break;
                            }
                            Some('\\') => {
                                let e = at(i + 1).ok_or_else(|| format!("{l}:{cl}: a string isn't closed"))?;
                                match e {
                                    '"' | '\\' | '/' => s.push(e),
                                    'b' => s.push('\u{8}'),
                                    'f' => s.push('\u{c}'),
                                    'n' => s.push('\n'),
                                    'r' => s.push('\r'),
                                    't' => s.push('\t'),
                                    'u' => {
                                        let hex: String = (2..6).filter_map(|k| at(i + k)).collect();
                                        let n = u32::from_str_radix(&hex, 16).map_err(|_| format!("{line}:{col}: `\\u{hex}` isn't an escape"))?;
                                        s.push(char::from_u32(n).unwrap_or('\u{fffd}'));
                                        bump!(4);
                                    }
                                    other => return Err(format!("{line}:{col}: `\\{other}` isn't an escape")),
                                }
                                bump!(2);
                            }
                            Some(ch) => {
                                s.push(ch);
                                bump!(1);
                            }
                        }
                    }
                    out.push((Tok::Str(s), l, cl));
                }
            }
            c if c == '-' || c.is_ascii_digit() => {
                let start = i;
                bump!(1);
                let mut float = false;
                while let Some(d) = at(i) {
                    if d.is_ascii_digit() {
                        bump!(1);
                    } else if d == '.' || d == 'e' || d == 'E' || ((d == '+' || d == '-') && matches!(at(i.wrapping_sub(1)), Some('e' | 'E'))) {
                        float = true;
                        bump!(1);
                    } else {
                        break;
                    }
                }
                let text: String = chars.get(start..i).unwrap_or_default().iter().collect();
                let tok = if float {
                    Tok::Float(text.parse().map_err(|_| format!("{l}:{cl}: `{text}` isn't a number"))?)
                } else {
                    Tok::Int(text.parse().map_err(|_| format!("{l}:{cl}: `{text}` isn't a number"))?)
                };
                out.push((tok, l, cl));
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let start = i;
                while at(i).is_some_and(|c| c == '_' || c.is_ascii_alphanumeric()) {
                    bump!(1);
                }
                out.push((Tok::Name(chars.get(start..i).unwrap_or_default().iter().collect()), l, cl));
            }
            other => return Err(format!("{l}:{cl}: `{other}` has no place here")),
        }
    }
    Ok(out)
}

/// A block string's value: its common indentation and its blank first and last lines gone.
fn block_string(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    let indent = lines.iter().skip(1).filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start_matches([' ', '\t']).len()).min().unwrap_or(0);
    let mut out: Vec<String> = lines.iter().enumerate().map(|(n, l)| if n == 0 { (*l).to_owned() } else { l.get(indent..).unwrap_or("").to_owned() }).collect();
    while out.first().is_some_and(|l| l.trim().is_empty()) {
        out.remove(0);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

struct Parser {
    toks: Vec<(Tok, usize, usize)>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at).map(|(t, _, _)| t)
    }

    fn here(&self) -> String {
        match self.toks.get(self.at) {
            Some((_, l, c)) => format!("{l}:{c}"),
            None => "the end".to_owned(),
        }
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).map(|(t, _, _)| t.clone());
        self.at += 1;
        t
    }

    fn is(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.is(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("{}: `{c}` expected", self.here()))
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Tok::Name(n)) => Ok(n),
            _ => {
                self.at -= 1;
                Err(format!("{}: a name expected", self.here()))
            }
        }
    }

    fn keyword(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Name(n)) if n == k)
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > DEEPEST { Err(format!("{}: nested deeper than {DEEPEST}", self.here())) } else { Ok(()) }
    }

    fn document(&mut self) -> Result<Document, String> {
        let mut doc = Document::default();
        while self.peek().is_some() {
            if self.is('{') {
                let selection = self.selection_set()?;
                doc.operations.push(Operation { kind: Kind::Query, name: None, vars: Vec::new(), directives: Vec::new(), selection });
                continue;
            }
            let word = self.name()?;
            match word.as_str() {
                "query" | "mutation" | "subscription" => {
                    let kind = match word.as_str() {
                        "query" => Kind::Query,
                        "mutation" => Kind::Mutation,
                        _ => Kind::Subscription,
                    };
                    let name = if matches!(self.peek(), Some(Tok::Name(_))) { Some(self.name()?) } else { None };
                    let vars = if self.is('(') { self.var_defs()? } else { Vec::new() };
                    let directives = self.directives(false)?;
                    let selection = self.selection_set()?;
                    doc.operations.push(Operation { kind, name, vars, directives, selection });
                }
                "fragment" => {
                    let name = self.name()?;
                    if name == "on" {
                        return Err(format!("{}: a fragment can't be called `on`", self.here()));
                    }
                    if !self.keyword("on") {
                        return Err(format!("{}: `on` expected", self.here()));
                    }
                    self.at += 1;
                    let on = self.name()?;
                    let directives = self.directives(false)?;
                    let selection = self.selection_set()?;
                    doc.fragments.push(Fragment { name, on, directives, selection });
                }
                other => return Err(format!("{}: `{other}` is no definition (query, mutation, subscription or fragment)", self.here())),
            }
        }
        if doc.operations.is_empty() {
            return Err("the document has no operation".to_owned());
        }
        Ok(doc)
    }

    fn var_defs(&mut self) -> Result<Vec<VarDef>, String> {
        self.expect('(')?;
        let mut out = Vec::new();
        while !self.is(')') {
            self.expect('$')?;
            let name = self.name()?;
            self.expect(':')?;
            let ty = self.ty()?;
            let default = if self.is('=') {
                self.at += 1;
                Some(self.value(true)?)
            } else {
                None
            };
            self.directives(true)?;
            out.push(VarDef { name, ty, default });
        }
        self.expect(')')?;
        Ok(out)
    }

    fn ty(&mut self) -> Result<String, String> {
        self.deeper()?;
        let mut t = if self.is('[') {
            self.at += 1;
            let inner = self.ty()?;
            self.expect(']')?;
            format!("[{inner}]")
        } else {
            self.name()?
        };
        if self.is('!') {
            self.at += 1;
            t.push('!');
        }
        self.depth -= 1;
        Ok(t)
    }

    fn directives(&mut self, constant: bool) -> Result<Vec<Directive>, String> {
        let mut out = Vec::new();
        while self.is('@') {
            self.at += 1;
            let name = self.name()?;
            let args = if self.is('(') { self.arguments(constant)? } else { Vec::new() };
            out.push(Directive { name, args });
        }
        Ok(out)
    }

    fn arguments(&mut self, constant: bool) -> Result<Vec<(String, Value)>, String> {
        self.expect('(')?;
        let mut out = Vec::new();
        while !self.is(')') {
            let name = self.name()?;
            self.expect(':')?;
            out.push((name, self.value(constant)?));
        }
        self.expect(')')?;
        Ok(out)
    }

    fn value(&mut self, constant: bool) -> Result<Value, String> {
        self.deeper()?;
        let here = self.here();
        let v = match self.next() {
            Some(Tok::Punct('$')) if !constant => Value::Variable(self.name()?),
            Some(Tok::Punct('$')) => return Err(format!("{here}: a variable where a constant belongs")),
            Some(Tok::Int(n)) => Value::Int(n),
            Some(Tok::Float(f)) => Value::Float(f),
            Some(Tok::Str(s)) => Value::String(s),
            Some(Tok::Name(n)) => match n.as_str() {
                "true" => Value::Boolean(true),
                "false" => Value::Boolean(false),
                "null" => Value::Null,
                _ => Value::Enum(n),
            },
            Some(Tok::Punct('[')) => {
                let mut items = Vec::new();
                while !self.is(']') {
                    items.push(self.value(constant)?);
                }
                self.expect(']')?;
                Value::List(items)
            }
            Some(Tok::Punct('{')) => {
                let mut fields = Vec::new();
                while !self.is('}') {
                    let k = self.name()?;
                    self.expect(':')?;
                    fields.push((k, self.value(constant)?));
                }
                self.expect('}')?;
                Value::Object(fields)
            }
            _ => return Err(format!("{here}: a value expected")),
        };
        self.depth -= 1;
        Ok(v)
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, String> {
        self.deeper()?;
        self.expect('{')?;
        let mut out = Vec::new();
        while !self.is('}') {
            if self.peek() == Some(&Tok::Spread) {
                self.at += 1;
                if self.keyword("on") || self.is('{') || self.is('@') {
                    let on = if self.keyword("on") {
                        self.at += 1;
                        Some(self.name()?)
                    } else {
                        None
                    };
                    let directives = self.directives(false)?;
                    let selection = self.selection_set()?;
                    out.push(Selection::Inline { on, directives, selection });
                } else {
                    let name = self.name()?;
                    let directives = self.directives(false)?;
                    out.push(Selection::Spread { name, directives });
                }
                continue;
            }
            let first = self.name()?;
            let (alias, name) = if self.is(':') {
                self.at += 1;
                (Some(first), self.name()?)
            } else {
                (None, first)
            };
            let args = if self.is('(') { self.arguments(false)? } else { Vec::new() };
            let directives = self.directives(false)?;
            let selection = if self.is('{') { self.selection_set()? } else { Vec::new() };
            out.push(Selection::Field(Field { alias, name, args, directives, selection }));
        }
        self.expect('}')?;
        if out.is_empty() {
            return Err(format!("{}: an empty selection", self.here()));
        }
        self.depth -= 1;
        Ok(out)
    }
}

/// `src` as an executable document.
pub fn parse(src: &str) -> Result<Document, String> {
    let toks = lex(src)?;
    Parser { toks, at: 0, depth: 0 }.document()
}

impl Document {
    /// The operation to run: the one named, or the only one.
    pub fn operation(&self, name: Option<&str>) -> Result<&Operation, String> {
        match name.filter(|n| !n.is_empty()) {
            Some(n) => self.operations.iter().find(|o| o.name.as_deref() == Some(n)).ok_or_else(|| format!("no operation named `{n}`")),
            None => match self.operations.as_slice() {
                [one] => Ok(one),
                _ => Err("the document has several operations: say which (`operationName`)".to_owned()),
            },
        }
    }

    pub fn fragment(&self, name: &str) -> Option<&Fragment> {
        self.fragments.iter().find(|f| f.name == name)
    }
}

impl Operation {
    /// The variables given, with the operation's defaults for those not given.
    pub fn variables(&self, given: &Map<String, Json>) -> Map<String, Json> {
        let mut out = given.clone();
        for v in &self.vars {
            if !out.contains_key(&v.name)
                && let Some(d) = &v.default
            {
                out.insert(v.name.clone(), json(d, &Map::new()));
            }
        }
        out
    }
}

/// A document value as JSON: an enum as its name, a variable as its value (null if not given).
pub fn json(v: &Value, vars: &Map<String, Json>) -> Json {
    match v {
        Value::Variable(n) => vars.get(n).cloned().unwrap_or(Json::Null),
        Value::Int(n) => Json::from(*n),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::String(s) | Value::Enum(s) => Json::String(s.clone()),
        Value::Boolean(b) => Json::Bool(*b),
        Value::Null => Json::Null,
        Value::List(items) => Json::Array(items.iter().map(|x| json(x, vars)).collect()),
        Value::Object(fields) => Json::Object(fields.iter().map(|(k, x)| (k.clone(), json(x, vars))).collect()),
    }
}

/// A field's arguments as JSON.
pub fn args(f: &Field, vars: &Map<String, Json>) -> Map<String, Json> {
    f.args.iter().map(|(k, v)| (k.clone(), json(v, vars))).collect()
}

/// Whether `@skip` and `@include` let a selection in.
pub fn included(directives: &[Directive], vars: &Map<String, Json>) -> bool {
    let cond = |name: &str| directives.iter().find(|d| d.name == name).and_then(|d| d.args.iter().find(|(k, _)| k == "if")).map(|(_, v)| json(v, vars) == Json::Bool(true));
    cond("skip") != Some(true) && cond("include") != Some(false)
}

/// The fields an object of type `ty` answers for `selection`, by response key in order, each
/// key's fields merged (the spec's CollectFields). `is_a(ty, cond)` says whether an object of
/// `ty` takes a fragment on `cond` (its own type, or an interface or union it is in).
pub fn collect<'a>(doc: &'a Document, selection: &'a [Selection], ty: &str, is_a: &dyn Fn(&str, &str) -> bool, vars: &Map<String, Json>) -> Vec<(String, Vec<&'a Field>)> {
    let mut out: Vec<(String, Vec<&'a Field>)> = Vec::new();
    let mut visited: Vec<&str> = Vec::new();
    gather(doc, selection, ty, is_a, vars, &mut out, &mut visited);
    out
}

fn gather<'a>(
    doc: &'a Document,
    selection: &'a [Selection],
    ty: &str,
    is_a: &dyn Fn(&str, &str) -> bool,
    vars: &Map<String, Json>,
    out: &mut Vec<(String, Vec<&'a Field>)>,
    visited: &mut Vec<&'a str>,
) {
    for s in selection {
        match s {
            Selection::Field(f) => {
                if !included(&f.directives, vars) {
                    continue;
                }
                match out.iter_mut().find(|(k, _)| k == f.key()) {
                    Some((_, fs)) => fs.push(f),
                    None => out.push((f.key().to_owned(), vec![f])),
                }
            }
            Selection::Spread { name, directives } => {
                if !included(directives, vars) || visited.contains(&name.as_str()) {
                    continue;
                }
                visited.push(name);
                if let Some(frag) = doc.fragment(name)
                    && is_a(ty, &frag.on)
                {
                    gather(doc, &frag.selection, ty, is_a, vars, out, visited);
                }
            }
            Selection::Inline { on, directives, selection } => {
                if included(directives, vars) && on.as_deref().is_none_or(|c| is_a(ty, c)) {
                    gather(doc, selection, ty, is_a, vars, out, visited);
                }
            }
        }
    }
}

/// The selections under a key's merged fields, together.
pub fn sub(fields: &[&Field]) -> Vec<Selection> {
    fields.iter().flat_map(|f| f.selection.iter().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json as j;

    fn obj(v: Json) -> Map<String, Json> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn first_field(sel: &[Selection]) -> Option<&Field> {
        match sel.first() {
            Some(Selection::Field(f)) => Some(f),
            _ => None,
        }
    }

    #[test]
    fn w14_r11_gh_s_queries_parse() -> Result<(), String> {
        // gh's pull request lookup, with a fragment, a variable default and a directive.
        let src = r#"
            # gh pr view
            query PullRequestByNumber($owner: String!, $repo: String!, $pr_number: Int!, $withLabels: Boolean = true) {
              repository(owner: $owner, name: $repo) {
                pullRequest(number: $pr_number) { ...pr labels(first: 100) @include(if: $withLabels) { nodes { name } } }
              }
            }
            fragment pr on PullRequest { id number title state isDraft author { login ... on Bot { id } } headRefName baseRefName }
        "#;
        let doc = parse(src)?;
        let op = doc.operation(None)?;
        assert_eq!((op.kind, op.name.as_deref(), op.vars.len()), (Kind::Query, Some("PullRequestByNumber"), 4));
        assert_eq!(op.vars.get(2).map(|v| v.ty.as_str()), Some("Int!"));
        let vars = op.variables(&obj(j!({ "owner": "acme", "repo": "app", "pr_number": 3 })));
        assert_eq!(vars.get("withLabels"), Some(&j!(true)));
        let is_a = |ty: &str, c: &str| ty == c || (c == "Actor" && matches!(ty, "User" | "Bot"));
        let top = collect(&doc, &op.selection, "Query", &is_a, &vars);
        assert_eq!(top.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), ["repository"]);
        let repo = top.first().map(|(_, f)| f.clone()).unwrap_or_default();
        assert_eq!(repo.first().map(|f| args(f, &vars)), Some(obj(j!({ "owner": "acme", "name": "app" }))));
        let inner = sub(&repo);
        let pr_fields = collect(&doc, &inner, "Repository", &is_a, &vars);
        let pr = sub(&pr_fields.first().map(|(_, f)| f.clone()).unwrap_or_default());
        let keys: Vec<String> = collect(&doc, &pr, "PullRequest", &is_a, &vars).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["id", "number", "title", "state", "isDraft", "author", "headRefName", "baseRefName", "labels"]);
        // Skipped, the labels go.
        let off = op.variables(&obj(j!({ "withLabels": false })));
        assert!(!collect(&doc, &pr, "PullRequest", &is_a, &off).iter().any(|(k, _)| k == "labels"));
        // A fragment on another type doesn't apply; aliases key the response.
        let doc = parse("{ a: viewer { login } b: viewer { ... on Organization { name } login } }")?;
        let op = doc.operation(None)?;
        let keys: Vec<(String, usize)> = collect(&doc, &op.selection, "Query", &is_a, &Map::new()).into_iter().map(|(k, f)| (k, f.len())).collect();
        assert_eq!(keys, [("a".to_owned(), 1), ("b".to_owned(), 1)]);
        Ok(())
    }

    #[test]
    fn w14_r11_mutations_values_and_errors() -> Result<(), String> {
        let src = r#"mutation CommentCreate($input: AddCommentInput!) { addComment(input: $input) { commentEdge { node { url } } } }
                     mutation { addLabelsToLabelable(input: { labelableId: "PR_1", labelIds: ["L_1", "L_2"], clientMutationId: null, n: -1.5e2, k: OPEN }) { clientMutationId } }"#;
        let doc = parse(src)?;
        assert!(doc.operation(None).is_err(), "two operations: say which");
        assert_eq!(doc.operation(Some("CommentCreate"))?.kind, Kind::Mutation);
        let anon = doc.operations.get(1).map(|o| o.selection.clone()).unwrap_or_default();
        let f = first_field(&anon).map(|f| args(f, &Map::new())).unwrap_or_default();
        assert_eq!(f, obj(j!({ "input": { "labelableId": "PR_1", "labelIds": ["L_1", "L_2"], "clientMutationId": null, "n": -150.0, "k": "OPEN" } })));
        // Strings: escapes and block strings.
        let doc = parse("{ a(s: \"tab\\there \\u00e9 \\\"q\\\"\", b: \"\"\"\n    first\n      second\n  \"\"\") }")?;
        let sel = doc.operations.first().map(|o| o.selection.clone()).unwrap_or_default();
        let f = first_field(&sel).map(|f| args(f, &Map::new())).unwrap_or_default();
        assert_eq!(f.get("s"), Some(&j!("tab\there é \"q\"")));
        assert_eq!(f.get("b"), Some(&j!("first\n  second")));
        // Refused, with where.
        assert!(parse("{ a").is_err_and(|e| e.contains("expected")));
        assert!(parse("query Q($x: Int = $y) { a }").is_err_and(|e| e.contains("constant")));
        assert!(parse("{ }").is_err_and(|e| e.contains("empty")));
        assert!(parse("fragment f on T { a }").is_err_and(|e| e.contains("no operation")));
        let deep = format!("{}{}", "{ a ".repeat(70), "}".repeat(70));
        assert!(parse(&deep).is_err_and(|e| e.contains("deeper")));
        // A fragment that spreads itself is followed once.
        let doc = parse("query { viewer { ...f } } fragment f on User { login ...f }")?;
        let op = doc.operation(None)?;
        let top = collect(&doc, &op.selection, "Query", &|a, b| a == b, &Map::new());
        let viewer = sub(&top.first().map(|(_, f)| f.clone()).unwrap_or_default());
        assert_eq!(collect(&doc, &viewer, "User", &|a, b| a == b, &Map::new()).len(), 1);
        Ok(())
    }
}
