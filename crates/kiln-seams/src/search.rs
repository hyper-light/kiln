//! `Search`: a tree's files, searched by their text or by the shape of their code, each hit with the
//! definition it sits in (docs/ci.md W26-R07, the search across a change). A tree is built once (its
//! files read into an index, under the tree's id), searched as often as asked, and forgotten; a tree
//! not built is refused, typed. vorpal is the real one (`kiln_search::VorpalSearch`, vendored at
//! vendor/vorpal); [`SimSearch`] keeps trees in memory and reads definitions and code patterns line
//! by line, so a run is the same every time.
//!
//! What a query means is the same in both: text is its words as written when they hold a capital
//! and in any case when they don't, or the regex between its slashes ([`text_pattern`]); code is an
//! ast-grep pattern, `$A` one node and `$$$` any run of them, matched in each language the tree's
//! files are in (or one, by name). A hit is the line a match starts on.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::Serialize;

use crate::conformance::{Case, Failure, ensure, fine, refused, same};

/// Most bytes a file of a tree may have: past it, the file is left out of the index (skipped).
pub const FILE_MAX: usize = 1 << 20;
/// Most files a tree may have.
pub const TREE_FILES_MAX: usize = 200_000;
/// Most bytes a tree's files may have in all.
pub const TREE_BYTES_MAX: u64 = 512 << 20;
/// Most bytes a query may have.
pub const QUERY_MAX: usize = 1000;
/// Most hits a search answers; past them, `total` counts the rest.
pub const HITS_MAX: usize = 1000;
/// Most characters of its line a hit carries.
pub const LINE_MAX: usize = 500;
/// Most bytes a tree's id may have: 1 to this many of `a-z 0-9 - _`.
pub const TREE_ID_MAX: usize = 128;
/// A file holding a NUL in its first this many bytes is binary, and left out (as one that isn't
/// UTF-8 is).
pub const BINARY_PROBE: usize = 8000;

/// One file of a tree, as the index takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    /// Its path in the tree: relative, `/`-separated.
    pub path: String,
    pub bytes: Vec<u8>,
}

/// How a query reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Words (or a regex between slashes), matched line by line.
    Text,
    /// An ast-grep pattern, matched against the code's syntax tree.
    Code,
}

/// A search of one tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub mode: Mode,
    pub q: String,
    /// Code: only the files of this language (`rust`, `python`, `typescript`, …, or an alias).
    pub lang: Option<String>,
    /// Most hits answered: 1 to [`HITS_MAX`].
    pub limit: usize,
}

/// The definition a hit sits in: its name and its kind (`function`, `method`, `class`, `struct`,
/// `object` for an `impl`, …, as vorpal's outline names them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
}

/// One hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub path: String,
    /// Where the match starts: its line and column (in characters), from 1.
    pub line: u32,
    pub column: u32,
    /// The line it ends on: a code match may run over several.
    pub end_line: u32,
    /// The line it starts on, at most [`LINE_MAX`] characters of it.
    pub text: String,
    /// The innermost definition holding its start, when one does.
    pub symbol: Option<Symbol>,
}

/// What building a tree made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Built {
    /// Files in the index.
    pub files: u64,
    /// Files left out: binary, not UTF-8, or past [`FILE_MAX`].
    pub skipped: u64,
    /// The indexed files' bytes.
    pub bytes: u64,
}

/// What a search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Found {
    /// The first hits, at most the query's limit, by path, then line, then column.
    pub hits: Vec<Hit>,
    /// Every hit, those past the limit too.
    pub total: u64,
    /// Files with at least one hit.
    pub matched_files: u64,
    /// Files in the index.
    pub files: u64,
}

impl Found {
    /// Hits were left out past the limit.
    pub fn truncated(&self) -> bool {
        self.total > self.hits.len() as u64
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The tree has no index: build it first.
    #[error("tree {tree} isn't built")]
    NotBuilt { tree: String },
    /// The query can't be read: empty, too long, a regex or a pattern that doesn't parse, a
    /// language no implementation knows.
    #[error("the query can't be read: {why}")]
    BadQuery { why: String },
    /// A tree past a bound.
    #[error("{what} is past its bound of {limit}")]
    TooBig { what: String, limit: u64 },
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("search is unavailable: {0}")]
    Unavailable(String),
}

/// A tree's files, searched.
pub trait Search: Send + Sync + 'static {
    /// Indexes `files` as tree `tree`'s. A tree already built is kept as it is (a tree's id names
    /// its files), and answered as it was built. Past its bound of trees, the one searched least
    /// recently is forgotten to make room.
    fn build<'a>(&'a self, tree: &'a str, files: Vec<File>) -> BoxFuture<'a, Result<Built, Refusal>>;
    /// What `tree` was built with, if it is built.
    fn built<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<Option<Built>, Refusal>>;
    /// Searches `tree`.
    fn search<'a>(&'a self, tree: &'a str, query: Query) -> BoxFuture<'a, Result<Found, Refusal>>;
    /// Drops `tree`'s index; a tree not built is forgotten already.
    fn forget<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
}

// ------------------------------------------------------------------------------------ shared

/// A tree's id, checked: 1 to [`TREE_ID_MAX`] bytes of `a-z 0-9 - _` (a git tree's SHA is one).
pub fn check_tree(tree: &str) -> Result<(), Refusal> {
    let ok = !tree.is_empty() && tree.len() <= TREE_ID_MAX && tree.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    ensure_or(ok, || Refusal::Invalid { what: "a tree's id", why: format!("1 to {TREE_ID_MAX} bytes of a-z, 0-9, - and _, not {tree:?}") })
}

/// A file's path, checked: relative, `/`-separated, no empty, `.` or `..` part, no NUL, at most
/// 4096 bytes.
pub fn check_path(path: &str) -> Result<(), Refusal> {
    let ok = !path.is_empty() && path.len() <= 4096 && !path.contains('\0') && !path.starts_with('/') && path.split('/').all(|p| !p.is_empty() && p != "." && p != "..");
    ensure_or(ok, || Refusal::Invalid { what: "a file's path", why: format!("a relative path in the tree, not {path:?}") })
}

/// A query's bounds, checked: what isn't its words or its pattern.
pub fn check_query(query: &Query) -> Result<(), Refusal> {
    if query.limit == 0 || query.limit > HITS_MAX {
        return Err(Refusal::Invalid { what: "a query's limit", why: format!("1 to {HITS_MAX}, not {}", query.limit) });
    }
    if query.q.trim().is_empty() {
        return Err(Refusal::BadQuery { why: "it's empty".into() });
    }
    if query.q.len() > QUERY_MAX {
        return Err(Refusal::BadQuery { why: format!("it's {} bytes; at most {QUERY_MAX}", query.q.len()) });
    }
    Ok(())
}

/// Whether a file is left out as binary: a NUL in its first [`BINARY_PROBE`] bytes, or not UTF-8.
pub fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_PROBE).any(|b| *b == 0) || std::str::from_utf8(bytes).is_err()
}

/// A text query read: the regex a line is matched with, and the words every hit holds exactly
/// (none when they match in any case, or for a regex).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPattern {
    pub regex: String,
    pub exact: Option<String>,
    pub is_regex: bool,
}

/// What a text query matches: between slashes (`/fn \w+_of/`), a regex (Rust's `regex` syntax);
/// otherwise its words, as written when they hold a capital and in any case when they don't.
pub fn text_pattern(q: &str) -> Result<TextPattern, Refusal> {
    if let Some(inner) = q.strip_prefix('/').and_then(|r| r.strip_suffix('/')).filter(|i| !i.is_empty()) {
        regex::Regex::new(inner).map_err(|e| Refusal::BadQuery { why: format!("the regex doesn't parse: {e}") })?;
        return Ok(TextPattern { regex: inner.to_owned(), exact: None, is_regex: true });
    }
    let escaped = regex::escape(q);
    Ok(if q.chars().any(char::is_uppercase) {
        TextPattern { regex: escaped, exact: Some(q.to_owned()), is_regex: false }
    } else {
        TextPattern { regex: format!("(?i){escaped}"), exact: None, is_regex: false }
    })
}

/// A line's text as a hit carries it: no line ending, at most [`LINE_MAX`] characters.
pub fn hit_text(line: &str) -> String {
    line.trim_end_matches(['\r', '\n']).chars().take(LINE_MAX).collect()
}

/// `hits` cut to `limit`, with what a search reports of them.
pub fn found(mut hits: Vec<Hit>, limit: usize, files: u64) -> Found {
    hits.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
    let total = hits.len() as u64;
    let matched_files = hits.iter().map(|h| h.path.as_str()).collect::<BTreeSet<_>>().len() as u64;
    hits.truncate(limit);
    Found { hits, total, matched_files, files }
}

fn ensure_or(ok: bool, refusal: impl FnOnce() -> Refusal) -> Result<(), Refusal> {
    if ok { Ok(()) } else { Err(refusal()) }
}

// ---------------------------------------------------------------------------------- simulator

/// Most trees a [`SimSearch`] holds; past them, building another forgets the least recently
/// searched.
pub const SIM_TREES: usize = 64;

/// Trees in memory. Clones share them.
#[derive(Clone)]
pub struct SimSearch {
    inner: Arc<Mutex<SimState>>,
}

struct SimState {
    /// Least recently searched (or built) first.
    trees: Vec<(String, SimTree)>,
    max: usize,
}

struct SimTree {
    files: Vec<SimFile>,
    built: Built,
}

struct SimFile {
    path: String,
    text: String,
    lang: Option<&'static str>,
    defs: Vec<Def>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Def {
    start: usize,
    end: usize,
    name: String,
    kind: &'static str,
}

impl Default for SimSearch {
    fn default() -> Self {
        Self::with_trees(SIM_TREES)
    }
}

impl SimSearch {
    /// One that holds at most `max` trees (at least one).
    pub fn with_trees(max: usize) -> Self {
        Self { inner: Arc::new(Mutex::new(SimState { trees: Vec::new(), max: max.max(1) })) }
    }
}

/// The language the simulator reads a file in, by its name, as vorpal names it.
pub fn sim_language_of(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "sh" | "bash" => "bash",
        _ => return None,
    })
}

/// A language's name from what a query calls it (an alias or the name), as vorpal names it.
fn sim_language_named(name: &str) -> Option<&'static str> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "python" | "py" => "python",
        "typescript" | "ts" => "typescript",
        "tsx" => "tsx",
        "javascript" | "js" | "jsx" => "javascript",
        "go" | "golang" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "c++" | "cc" | "cxx" => "cpp",
        "bash" | "sh" => "bash",
        _ => return None,
    })
}

/// Byte offsets where each line starts.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i.saturating_add(1))).collect()
}

/// The line (from 1) holding byte `at`, and where it starts.
fn line_at(starts: &[usize], at: usize) -> (usize, usize) {
    let i = starts.partition_point(|s| *s <= at).saturating_sub(1);
    (i.saturating_add(1), starts.get(i).copied().unwrap_or(0))
}

fn line_text(text: &str, start: usize) -> &str {
    let rest = text.get(start..).unwrap_or_default();
    rest.split('\n').next().unwrap_or_default()
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The identifier `rest` starts with.
fn ident(rest: &str) -> &str {
    let end = rest.char_indices().find(|(_, c)| !is_ident(*c)).map_or(rest.len(), |(i, _)| i);
    rest.get(..end).unwrap_or_default()
}

/// The definitions of a brace language's file: each `fn`, `func`, `function`, `class`, `struct`,
/// `impl`, `trait`, `enum` or `interface` line, to where the braces it opens close (or to its end
/// with none).
fn brace_defs(text: &str) -> Vec<Def> {
    let mut out: Vec<Def> = Vec::new();
    let mut at = 0usize;
    for line in text.split_inclusive('\n') {
        let start = at;
        at = at.saturating_add(line.len());
        let mut rest = line.trim_start();
        for prefix in ["pub(crate) ", "pub(super) ", "pub ", "export ", "default ", "async ", "unsafe ", "static ", "abstract "] {
            rest = rest.strip_prefix(prefix).unwrap_or(rest);
        }
        let Some((kw, kind)) = [("fn ", "function"), ("func ", "function"), ("function ", "function"), ("class ", "class"), ("struct ", "struct"), ("impl", "object"), ("trait ", "interface"), ("enum ", "enum"), ("interface ", "interface")]
            .into_iter()
            .find(|(kw, _)| rest.starts_with(kw))
        else {
            continue;
        };
        let mut after = rest.get(kw.len()..).unwrap_or_default();
        if kw == "impl" {
            // `impl Matrix`, `impl<T> Matrix<T>`: the type is what follows the generics.
            if let Some(generics) = after.strip_prefix('<') {
                let mut depth = 1i32;
                let cut = generics.char_indices().find(|(_, c)| {
                    depth = match c {
                        '<' => depth.saturating_add(1),
                        '>' => depth.saturating_sub(1),
                        _ => depth,
                    };
                    depth == 0
                });
                after = cut.and_then(|(i, _)| generics.get(i.saturating_add(1)..)).unwrap_or_default();
            } else if !after.starts_with(' ') {
                continue;
            }
        }
        let name = ident(after.trim_start());
        if name.is_empty() {
            continue;
        }
        let opener = start.saturating_add(line.len().saturating_sub(rest.len()));
        let body = text.get(opener..).unwrap_or_default();
        let end = match (body.find('{'), body.find(';')) {
            (Some(open), semi) if semi.is_none_or(|s| open < s) => {
                let mut depth = 0i32;
                let close = body.get(open..).unwrap_or_default().char_indices().find(|(_, c)| {
                    depth = match c {
                        '{' => depth.saturating_add(1),
                        '}' => depth.saturating_sub(1),
                        _ => depth,
                    };
                    depth == 0
                });
                close.map_or(text.len(), |(i, _)| opener.saturating_add(open).saturating_add(i).saturating_add(1))
            }
            _ => at,
        };
        out.push(Def { start, end, name: name.to_owned(), kind });
    }
    // A function inside an `impl`, a class, a trait or an interface is a method.
    let holders: Vec<(usize, usize)> = out.iter().filter(|d| matches!(d.kind, "object" | "class" | "interface")).map(|d| (d.start, d.end)).collect();
    for d in &mut out {
        if d.kind == "function" && holders.iter().any(|(s, e)| *s < d.start && d.end <= *e) {
            d.kind = "method";
        }
    }
    out
}

/// The definitions of a Python file: each `def` or `class`, to the last line indented under it.
fn python_defs(text: &str) -> Vec<Def> {
    let lines: Vec<(usize, &str)> = {
        let mut at = 0usize;
        text.split_inclusive('\n')
            .map(|l| {
                let s = at;
                at = at.saturating_add(l.len());
                (s, l)
            })
            .collect()
    };
    let indent = |l: &str| l.len().saturating_sub(l.trim_start().len());
    let mut out: Vec<(Def, usize)> = Vec::new();
    for (i, (start, line)) in lines.iter().enumerate() {
        let rest = line.trim_start();
        let rest = rest.strip_prefix("async ").unwrap_or(rest);
        let Some((kw, kind)) = [("def ", "function"), ("class ", "class")].into_iter().find(|(kw, _)| rest.starts_with(kw)) else { continue };
        let name = ident(rest.get(kw.len()..).unwrap_or_default().trim_start());
        if name.is_empty() {
            continue;
        }
        let own = indent(line);
        let mut end = start.saturating_add(line.trim_end_matches(['\r', '\n']).len());
        for (s, l) in lines.iter().skip(i.saturating_add(1)) {
            if l.trim().is_empty() {
                continue;
            }
            if indent(l) <= own {
                break;
            }
            end = s.saturating_add(l.trim_end_matches(['\r', '\n']).len());
        }
        out.push((Def { start: *start, end, name: name.to_owned(), kind }, own));
    }
    let classes: Vec<(usize, usize, usize)> = out.iter().filter(|(d, _)| d.kind == "class").map(|(d, ind)| (d.start, d.end, *ind)).collect();
    out.into_iter()
        .map(|(mut d, ind)| {
            if d.kind == "function" && classes.iter().any(|(s, e, ci)| *s < d.start && d.end <= *e && *ci < ind) {
                d.kind = "method";
            }
            d
        })
        .collect()
}

fn defs_of(lang: Option<&str>, text: &str) -> Vec<Def> {
    match lang {
        Some("python") => python_defs(text),
        Some(_) => brace_defs(text),
        None => Vec::new(),
    }
}

fn innermost(defs: &[Def], at: usize) -> Option<Symbol> {
    defs.iter().filter(|d| d.start <= at && at < d.end).min_by_key(|d| d.end.saturating_sub(d.start)).map(|d| Symbol { name: d.name.clone(), kind: d.kind.to_owned() })
}

/// A piece of a code pattern, as the simulator reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// Characters matched as written (whitespace anywhere between them).
    Lit(Vec<char>),
    /// `$A`: one balanced run with no top-level comma.
    One,
    /// `$$$` (`$$$ARGS`): any balanced run, commas too, maybe none.
    Many,
}

/// A code pattern read into pieces, or why not: empty, or brackets that don't balance.
fn pieces(pattern: &str) -> Result<Vec<Piece>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut lit: Vec<char> = Vec::new();
    let mut depth = 0i32;
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c == '$' {
            let many = chars.get(i..i.saturating_add(3)).is_some_and(|s| s == ['$', '$', '$']);
            let mut j = if many { i.saturating_add(3) } else { i.saturating_add(1) };
            while chars.get(j).is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_') {
                j = j.saturating_add(1);
            }
            if many || j > i.saturating_add(1) {
                if !lit.is_empty() {
                    out.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                out.push(if many { Piece::Many } else { Piece::One });
                i = j;
                continue;
            }
        }
        match c {
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth < 0 {
            return Err("its brackets don't balance".into());
        }
        if !c.is_whitespace() {
            lit.push(c);
        }
        i = i.saturating_add(1);
    }
    if !lit.is_empty() {
        out.push(Piece::Lit(lit));
    }
    if depth != 0 {
        return Err("its brackets don't balance".into());
    }
    if !out.iter().any(|p| matches!(p, Piece::Lit(_))) {
        return Err("it has nothing but metavariables".into());
    }
    Ok(out)
}

fn skip_ws(t: &[char], mut i: usize) -> usize {
    while t.get(i).is_some_and(|c| c.is_whitespace()) {
        i = i.saturating_add(1);
    }
    i
}

/// Where a balanced run from `i` ends: at a closing bracket of its own level, a `;`, or (unless
/// `commas`) a comma of its own level.
fn balanced(t: &[char], mut i: usize, commas: bool) -> usize {
    let mut depth = 0i32;
    while let Some(&c) = t.get(i) {
        match c {
            '(' | '[' | '{' => depth = depth.saturating_add(1),
            ')' | ']' | '}' if depth == 0 => break,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 && !commas => break,
            ';' if depth == 0 => break,
            _ => {}
        }
        i = i.saturating_add(1);
    }
    i
}

/// Where `ps` matched from `i` ends, if they match there.
fn match_at(ps: &[Piece], t: &[char], i: usize) -> Option<usize> {
    let Some((first, rest)) = ps.split_first() else { return Some(i) };
    match first {
        Piece::Lit(lit) => {
            let mut j = i;
            for c in lit {
                j = skip_ws(t, j);
                if t.get(j) != Some(c) {
                    return None;
                }
                j = j.saturating_add(1);
            }
            match_at(rest, t, j)
        }
        Piece::One => {
            let start = skip_ws(t, i);
            let end = balanced(t, start, false);
            let run = t.get(start..end).unwrap_or_default();
            if run.iter().all(|c| c.is_whitespace()) {
                return None;
            }
            match_at(rest, t, end)
        }
        Piece::Many => match_at(rest, t, balanced(t, skip_ws(t, i), true)),
    }
}

/// Every place `ps` matches in `text`: (start byte, end byte), never a definition's own name.
fn code_matches(ps: &[Piece], text: &str) -> Vec<(usize, usize)> {
    let t: Vec<char> = text.chars().collect();
    let bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).chain(std::iter::once(text.len())).collect();
    let first = ps.first().and_then(|p| match p {
        Piece::Lit(l) => l.first().copied(),
        _ => None,
    });
    let mut out = Vec::new();
    for (i, c) in t.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|p| t.get(p)).copied();
        let starts = match first {
            Some(f) => *c == f && !(is_ident(f) && prev.is_some_and(is_ident)),
            None => !c.is_whitespace() && !prev.is_some_and(is_ident),
        };
        if !starts {
            continue;
        }
        let Some(end) = match_at(ps, &t, i) else { continue };
        let at = bytes.get(i).copied().unwrap_or(0);
        let before = text.get(..at).unwrap_or_default();
        let line_before = before.rsplit('\n').next().unwrap_or_default();
        let word = line_before.trim_end().rsplit(|c: char| !is_ident(c)).next().unwrap_or_default();
        if matches!(word, "fn" | "def" | "function" | "func") {
            continue;
        }
        out.push((at, bytes.get(end).copied().unwrap_or(text.len())));
    }
    out
}

impl SimState {
    fn touch(&mut self, tree: &str) -> Option<&SimTree> {
        let pos = self.trees.iter().position(|(t, _)| t == tree)?;
        let entry = self.trees.remove(pos);
        self.trees.push(entry);
        self.trees.last().map(|(_, t)| t)
    }
}

fn sim_build(files: Vec<File>) -> Result<SimTree, Refusal> {
    if files.len() > TREE_FILES_MAX {
        return Err(Refusal::TooBig { what: format!("a tree of {} files", files.len()), limit: TREE_FILES_MAX as u64 });
    }
    let total: u64 = files.iter().map(|f| f.bytes.len() as u64).fold(0, u64::saturating_add);
    if total > TREE_BYTES_MAX {
        return Err(Refusal::TooBig { what: format!("a tree of {total} bytes"), limit: TREE_BYTES_MAX });
    }
    let mut seen = BTreeSet::new();
    let mut built = Built { files: 0, skipped: 0, bytes: 0 };
    let mut kept = Vec::new();
    for f in files {
        check_path(&f.path)?;
        if !seen.insert(f.path.clone()) {
            return Err(Refusal::Invalid { what: "a file's path", why: format!("{} comes twice", f.path) });
        }
        if f.bytes.len() > FILE_MAX || is_binary(&f.bytes) {
            built.skipped = built.skipped.saturating_add(1);
            continue;
        }
        let text = String::from_utf8_lossy(&f.bytes).into_owned();
        let lang = sim_language_of(&f.path);
        built.files = built.files.saturating_add(1);
        built.bytes = built.bytes.saturating_add(f.bytes.len() as u64);
        kept.push(SimFile { defs: defs_of(lang, &text), path: f.path, text, lang });
    }
    kept.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(SimTree { files: kept, built })
}

fn sim_search(tree: &SimTree, query: &Query) -> Result<Found, Refusal> {
    let mut hits = Vec::new();
    match query.mode {
        Mode::Text => {
            let p = text_pattern(&query.q)?;
            let re = regex::Regex::new(&p.regex).map_err(|e| Refusal::BadQuery { why: format!("the regex doesn't parse: {e}") })?;
            for f in &tree.files {
                let starts = line_starts(&f.text);
                for (n, start) in starts.iter().enumerate() {
                    if *start == f.text.len() && n > 0 {
                        // After the last line's ending: no line.
                        continue;
                    }
                    let line = line_text(&f.text, *start).trim_end_matches('\r');
                    let Some(m) = re.find(line) else { continue };
                    let col = line.get(..m.start()).unwrap_or_default().chars().count();
                    let no = u32::try_from(n.saturating_add(1)).unwrap_or(u32::MAX);
                    hits.push(Hit { path: f.path.clone(), line: no, column: u32::try_from(col.saturating_add(1)).unwrap_or(u32::MAX), end_line: no, text: hit_text(line), symbol: innermost(&f.defs, start.saturating_add(m.start())) });
                }
            }
        }
        Mode::Code => {
            let only = match query.lang.as_deref() {
                Some(name) => Some(sim_language_named(name).ok_or_else(|| Refusal::BadQuery { why: format!("no language `{name}`") })?),
                None => None,
            };
            let files: Vec<&SimFile> = tree.files.iter().filter(|f| f.lang.is_some() && (only.is_none() || f.lang == only)).collect();
            if files.is_empty() {
                return Ok(found(Vec::new(), query.limit, tree.built.files));
            }
            let ps = pieces(&query.q).map_err(|why| Refusal::BadQuery { why: format!("the pattern doesn't parse: {why}") })?;
            for f in files {
                let starts = line_starts(&f.text);
                for (at, end) in code_matches(&ps, &f.text) {
                    let (line, line_start) = line_at(&starts, at);
                    let (end_line, _) = line_at(&starts, end.saturating_sub(1).max(at));
                    let col = f.text.get(line_start..at).unwrap_or_default().chars().count();
                    hits.push(Hit {
                        path: f.path.clone(),
                        line: u32::try_from(line).unwrap_or(u32::MAX),
                        column: u32::try_from(col.saturating_add(1)).unwrap_or(u32::MAX),
                        end_line: u32::try_from(end_line).unwrap_or(u32::MAX),
                        text: hit_text(line_text(&f.text, line_start)),
                        symbol: innermost(&f.defs, at),
                    });
                }
            }
        }
    }
    Ok(found(hits, query.limit, tree.built.files))
}

impl Search for SimSearch {
    fn build<'a>(&'a self, tree: &'a str, files: Vec<File>) -> BoxFuture<'a, Result<Built, Refusal>> {
        let out = (|| {
            check_tree(tree)?;
            let mut st = self.inner.lock();
            if let Some(t) = st.touch(tree) {
                return Ok(t.built);
            }
            let made = sim_build(files)?;
            let built = made.built;
            while st.trees.len() >= st.max {
                st.trees.remove(0);
            }
            st.trees.push((tree.to_owned(), made));
            Ok(built)
        })();
        Box::pin(async move { out })
    }

    fn built<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<Option<Built>, Refusal>> {
        let out = check_tree(tree).map(|()| self.inner.lock().trees.iter().find(|(t, _)| t == tree).map(|(_, t)| t.built));
        Box::pin(async move { out })
    }

    fn search<'a>(&'a self, tree: &'a str, query: Query) -> BoxFuture<'a, Result<Found, Refusal>> {
        let out = (|| {
            check_tree(tree)?;
            check_query(&query)?;
            let mut st = self.inner.lock();
            let t = st.touch(tree).ok_or_else(|| Refusal::NotBuilt { tree: tree.to_owned() })?;
            sim_search(t, &query)
        })();
        Box::pin(async move { out })
    }

    fn forget<'a>(&'a self, tree: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = check_tree(tree).map(|()| self.inner.lock().trees.retain(|(t, _)| t != tree));
        Box::pin(async move { out })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Search` conformance suite runs against: a fresh implementation per case, holding no
/// tree, that keeps at most [`SearchFixture::trees_max`] trees.
pub trait SearchFixture: Send + Sync {
    fn search(&self) -> &dyn Search;
    /// The most trees it keeps before it forgets one.
    fn trees_max(&self) -> usize;
}

/// The fixture for a [`SimSearch`].
#[derive(Default)]
pub struct SimSearchFixture(pub SimSearch);

impl SearchFixture for SimSearchFixture {
    fn search(&self) -> &dyn Search {
        &self.0
    }
    fn trees_max(&self) -> usize {
        SIM_TREES
    }
}

/// The files every case builds, as tree `t1`: a matrix in Rust, a report in Python, a note in no
/// language, a picture and a file past [`FILE_MAX`].
pub fn conformance_files() -> Vec<File> {
    let f = |path: &str, text: &str| File { path: path.to_owned(), bytes: text.as_bytes().to_vec() };
    vec![
        f(
            "src/expand.rs",
            "use crate::matrix::Matrix;\n\n/// Expands a matrix into its values, row by row.\npub fn expand(m: &Matrix) -> Vec<f64> {\n    // an empty matrix expands to nothing\n    if m.is_empty() {\n        return Vec::new();\n    }\n    m.values()\n}\n",
        ),
        f(
            "src/matrix.rs",
            "pub struct Matrix {\n    rows: Vec<Vec<f64>>,\n}\n\nimpl Matrix {\n    pub fn matrix_of(rows: Vec<Vec<f64>>) -> Matrix {\n        // the empty matrix has no rows\n        Matrix { rows }\n    }\n\n    pub fn is_empty(&self) -> bool {\n        self.rows.is_empty()\n    }\n\n    pub fn values(&self) -> Vec<f64> {\n        self.rows.concat()\n    }\n}\n",
        ),
        f(
            "src/main.rs",
            "use crate::expand::expand;\nuse crate::matrix::Matrix;\n\nfn main() {\n    let m = Matrix::matrix_of(vec![vec![1.0, 2.0]]);\n    let all = expand(&m);\n    let n = expand(&m).len();\n    report(all, n);\n}\n",
        ),
        f("tools/report.py", "def report(m):\n    # an Empty Matrix reports nothing\n    return expand(m)\n\n\nclass Report:\n    def render(self, m):\n        return expand(m)\n"),
        f("notes/README.txt", "An empty matrix is a matrix with no rows.\n"),
        File { path: "assets/logo.png".into(), bytes: vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13] },
        File { path: "big/blob.txt".into(), bytes: vec![b'a'; FILE_MAX.saturating_add(1)] },
    ]
}

fn text(q: &str, limit: usize) -> Query {
    Query { mode: Mode::Text, q: q.to_owned(), lang: None, limit }
}

fn code(q: &str, lang: Option<&str>) -> Query {
    Query { mode: Mode::Code, q: q.to_owned(), lang: lang.map(str::to_owned), limit: HITS_MAX }
}

async fn built_t1(s: &dyn Search) -> Result<Built, Failure> {
    fine("build t1", s.build("t1", conformance_files()).await)
}

/// Each hit as `path:line`.
fn places(f: &Found) -> Vec<String> {
    f.hits.iter().map(|h| format!("{}:{}", h.path, h.line)).collect()
}

/// Each hit's definition as `kind name`, or `-`.
fn symbols(f: &Found) -> Vec<String> {
    f.hits.iter().map(|h| h.symbol.as_ref().map_or_else(|| "-".to_owned(), |s| format!("{} {}", s.kind, s.name))).collect()
}

fn c1<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let found = fine("search", s.search("t1", text("empty matrix", HITS_MAX)).await)?;
        same("where the words are", places(&found), vec!["notes/README.txt:1".into(), "src/expand.rs:5".into(), "src/matrix.rs:7".into(), "tools/report.py:2".into()])?;
        let files = conformance_files();
        for h in &found.hits {
            let file = files.iter().find(|x| x.path == h.path).ok_or_else(|| format!("a hit in a file the tree hasn't: {}", h.path))?;
            let body = String::from_utf8_lossy(&file.bytes);
            let line = body.lines().nth(usize::try_from(h.line).unwrap_or(usize::MAX).saturating_sub(1)).unwrap_or_default();
            same(&format!("{}:{}'s text", h.path, h.line), h.text.as_str(), line)?;
            let col = line.to_lowercase().find("empty matrix").map(|b| line.get(..b).unwrap_or_default().chars().count().saturating_add(1));
            same(&format!("{}:{}'s column", h.path, h.line), Some(h.column as usize), col)?;
            same(&format!("{}:{}'s last line", h.path, h.line), h.end_line, h.line)?;
        }
        same("its counts (total, files with hits, files)", (found.total, found.matched_files, found.files), (4, 4, 5))
    })
}

fn c2<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let found = fine("search", s.search("t1", text("empty matrix", HITS_MAX)).await)?;
        same("the definitions they sit in", symbols(&found), vec!["-".into(), "function expand".into(), "method matrix_of".into(), "function report".into()])?;
        let outside = fine("search", s.search("t1", text("use crate::matrix", HITS_MAX)).await)?;
        same("lines in no definition", (places(&outside), symbols(&outside)), (vec!["src/expand.rs:1".into(), "src/main.rs:2".into()], vec!["-".into(), "-".into()]))
    })
}

fn c3<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let exact = fine("search", s.search("t1", text("Empty Matrix", HITS_MAX)).await)?;
        same("words with capitals, as written", places(&exact), vec!["tools/report.py:2".into()])?;
        let any = fine("search", s.search("t1", text("matrix_OF", HITS_MAX)).await)?;
        same("as written, no other case", places(&any), Vec::<String>::new())?;
        let re = fine("search", s.search("t1", text("/fn [a-z_]+_of/", HITS_MAX)).await)?;
        same("a regex between slashes", places(&re), vec!["src/matrix.rs:6".into()])
    })
}

fn c4<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let found = fine("search", s.search("t1", code("expand($A)", None)).await)?;
        same("each call", places(&found), vec!["src/main.rs:6".into(), "src/main.rs:7".into(), "tools/report.py:3".into(), "tools/report.py:8".into()])?;
        same("the definitions they sit in", symbols(&found), vec!["function main".into(), "function main".into(), "function report".into(), "method render".into()])?;
        let columns: Vec<u32> = found.hits.iter().map(|h| h.column).collect();
        same("where each starts", columns, vec![15, 13, 12, 16])?;
        let texts: Vec<&str> = found.hits.iter().map(|h| h.text.as_str()).collect();
        same("their lines", texts, vec!["    let all = expand(&m);", "    let n = expand(&m).len();", "    return expand(m)", "        return expand(m)"])
    })
}

fn c5<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let none = fine("search", s.search("t1", text("no such words anywhere", HITS_MAX)).await)?;
        same("words no line holds", (none.hits.len(), none.total, none.matched_files), (0, 0, 0))?;
        let never = fine("search", s.search("t1", code("never_called($A)", None)).await)?;
        same("a shape nothing has", (never.hits.len(), never.total), (0, 0))
    })
}

fn c6<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        for (what, q) in [("an empty query", text("   ", 10)), ("a regex that doesn't parse", text("/(unclosed/", 10)), ("a query past its bound", text(&"x".repeat(QUERY_MAX.saturating_add(1)), 10)), ("a pattern no language reads", code("expand(($A)", None)), ("a language no one knows", code("expand($A)", Some("cobol")))] {
            match s.search("t1", q).await {
                Err(Refusal::BadQuery { why }) => ensure(!why.is_empty(), || format!("{what}: refused without saying why"))?,
                other => return Err(format!("{what}: {other:?}, not refused BadQuery")),
            }
        }
        let zero = refused("a limit of 0", s.search("t1", text("matrix", 0)).await)?;
        ensure(matches!(zero, Refusal::Invalid { .. }), || format!("a limit of 0: {zero:?}"))
    })
}

fn c7<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let all = fine("search", s.search("t1", text("expand", HITS_MAX)).await)?;
        let two = fine("search", s.search("t1", text("expand", 2)).await)?;
        same("every hit", all.total, 8)?;
        same("the first two", places(&two), places(&all).into_iter().take(2).collect::<Vec<_>>())?;
        same("the total past the limit", (two.total, two.truncated(), all.truncated()), (8, true, false))
    })
}

fn c8<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        let never = refused("a tree never built", s.search("never-built", text("matrix", 10)).await)?;
        same("its refusal", never, Refusal::NotBuilt { tree: "never-built".into() })?;
        same("a tree never built", fine("built", s.built("never-built").await)?, None)?;
        built_t1(s).await?;
        fine("forget", s.forget("t1").await)?;
        same("a forgotten tree", fine("built", s.built("t1").await)?, None)?;
        ensure(matches!(s.search("t1", text("matrix", 10)).await, Err(Refusal::NotBuilt { .. })), || "a forgotten tree was searched".into())?;
        fine("forgetting it again", s.forget("t1").await)
    })
}

fn c9<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        fine("build t2", s.build("t2", vec![File { path: "src/expand.rs".into(), bytes: b"// a full matrix\n".to_vec() }]).await)?;
        let two = fine("search t2", s.search("t2", text("matrix", HITS_MAX)).await)?;
        same("t2's own lines", (places(&two), two.hits.first().map(|h| h.text.clone())), (vec!["src/expand.rs:1".into()], Some("// a full matrix".into())))?;
        let one = fine("search t1", s.search("t1", text("full matrix", HITS_MAX)).await)?;
        same("t1 holds none of t2's lines", one.total, 0)
    })
}

fn c10<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        let b = built_t1(s).await?;
        same("files indexed and left out", (b.files, b.skipped), (5, 2))?;
        same("what it says it was built with", fine("built", s.built("t1").await)?, Some(b))?;
        let png = fine("search", s.search("t1", text("PNG", HITS_MAX)).await)?;
        let big = fine("search", s.search("t1", text("aaaa", HITS_MAX)).await)?;
        same("hits in the files left out", (png.total, big.total), (0, 0))
    })
}

fn c11<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        let first = built_t1(s).await?;
        let again = fine("build t1 again", s.build("t1", Vec::new()).await)?;
        same("built again", again, first)?;
        let found = fine("search", s.search("t1", text("empty matrix", HITS_MAX)).await)?;
        same("its hits", found.total, 4)
    })
}

fn c12<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        let max = f.trees_max();
        let one = |i: usize| vec![File { path: "a.txt".into(), bytes: format!("tree {i}\n").into_bytes() }];
        for i in 0..max {
            fine("build", s.build(&format!("b{i}"), one(i)).await)?;
        }
        fine("search b0", s.search("b0", text("tree", 10)).await)?;
        fine("build one more", s.build("bnew", one(max)).await)?;
        same("the tree searched least recently", fine("built", s.built("b1").await)?.is_some(), false)?;
        same("the tree searched last", fine("built", s.built("b0").await)?.is_some(), true)?;
        same("the tree built last", fine("built", s.built("bnew").await)?.is_some(), true)
    })
}

fn c13<F: SearchFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let s = f.search();
        built_t1(s).await?;
        let py = fine("search python", s.search("t1", code("expand($A)", Some("python"))).await)?;
        same("python's files only", places(&py), vec!["tools/report.py:3".into(), "tools/report.py:8".into()])?;
        let rs = fine("search rs", s.search("t1", code("expand($A)", Some("rs"))).await)?;
        same("rust's, by its alias", places(&rs), vec!["src/main.rs:6".into(), "src/main.rs:7".into()])?;
        let go = fine("search go", s.search("t1", code("expand($A)", Some("go"))).await)?;
        same("a language no file is in", go.total, 0)
    })
}

/// The `Search` contract, clause by clause (docs/seams.md, "Search").
pub fn cases<F: SearchFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "SEARCH-1", says: "a built tree is searched by its words: each hit is a line holding them, with its file, line, column and text, by path then line", check: c1::<F> },
        Case { id: "SEARCH-2", says: "each hit names the innermost definition it sits in, by name and kind, and a line in none names none", check: c2::<F> },
        Case { id: "SEARCH-3", says: "words with a capital match as written, words without one in any case, and a query between slashes is a regex", check: c3::<F> },
        Case { id: "SEARCH-4", says: "a code pattern finds each place of its shape, never a definition's own name, each with the definition it sits in", check: c4::<F> },
        Case { id: "SEARCH-5", says: "a query nothing answers finds nothing, and isn't refused", check: c5::<F> },
        Case { id: "SEARCH-6", says: "an empty or long query, a regex or a pattern that doesn't parse, or a language no one knows is refused BadQuery, saying why; a limit of 0 is refused", check: c6::<F> },
        Case { id: "SEARCH-7", says: "hits past the limit are left out, and the total counts them", check: c7::<F> },
        Case { id: "SEARCH-8", says: "a tree not built is refused NotBuilt; a forgotten tree reads as never built, and forgetting it again is fine", check: c8::<F> },
        Case { id: "SEARCH-9", says: "trees are apart: one path in two trees answers from each tree's own files", check: c9::<F> },
        Case { id: "SEARCH-10", says: "a binary file, one that isn't UTF-8, or one past the bound is left out and counted as skipped", check: c10::<F> },
        Case { id: "SEARCH-11", says: "building a tree already built keeps it as it was built", check: c11::<F> },
        Case { id: "SEARCH-12", says: "past its bound of trees, building another forgets the one searched least recently", check: c12::<F> },
        Case { id: "SEARCH-13", says: "a code pattern given a language, by name or alias, searches only that language's files", check: c13::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// W26-R07: the simulator and its suite.
    #[test]
    fn w26_r07_the_simulated_search_passes_the_search_suite() -> Result<(), String> {
        futures::executor::block_on(run("Search", &cases::<SimSearchFixture>(), || async { SimSearchFixture::default() })).into_result().map(|_| ())
    }

    /// W26-R07: what a text query means.
    #[test]
    fn w26_r07_a_text_query_is_words_or_a_regex() {
        assert_eq!(text_pattern("empty matrix").map(|p| p.regex), Ok("(?i)empty matrix".into()));
        assert_eq!(text_pattern("Matrix::of").map(|p| (p.regex, p.exact)), Ok(("Matrix::of".into(), Some("Matrix::of".into()))));
        assert_eq!(text_pattern("/a+b/").map(|p| (p.regex, p.is_regex)), Ok(("a+b".into(), true)));
        assert_eq!(text_pattern("/").map(|p| p.regex), Ok("(?i)/".into()), "a lone slash is a word");
        assert!(matches!(text_pattern("/(/"), Err(Refusal::BadQuery { .. })));
    }

    /// W26-R07: the simulator's reading of definitions and patterns.
    #[test]
    fn w26_r07_the_simulator_reads_definitions_and_patterns() {
        let defs = brace_defs("impl<T> Grid<T> {\n    fn cell(&self) {}\n}\nfn free() { }\nstruct S;\n");
        let got: Vec<(String, &str)> = defs.iter().map(|d| (d.name.clone(), d.kind)).collect();
        assert_eq!(got, vec![("Grid".into(), "object"), ("cell".into(), "method"), ("free".into(), "function"), ("S".into(), "struct")]);
        assert_eq!(pieces("f($A, $$$REST)"), Ok(vec![Piece::Lit(vec!['f', '(']), Piece::One, Piece::Lit(vec![',']), Piece::Many, Piece::Lit(vec![')'])]));
        assert!(pieces("f(($A)").is_err());
        assert!(pieces("$A").is_err());
        let ps = pieces("f($A)").unwrap_or_default();
        assert_eq!(code_matches(&ps, "fn f(x: u8) {}\nlet y = f(g(1, 2));\nlet z = xf(3);\n").len(), 1);
    }
}
