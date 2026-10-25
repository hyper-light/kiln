//! Grok as Datadog's grok parser reads it (`docs/observe.md` §8.2): a rule is literal text (a
//! regular expression) and `%{matcher(args):attribute:filter(args)}` tokens, and must match the
//! value whole; the match rules are tried in order, the first that matches extracting its
//! attributes. A token's matcher is one of Datadog's (`notSpace`, `word`, `integer`, `number`,
//! `ipOrHost`, `date("dd/MMM/yyyy:HH:mm:ss Z")`, `regex("…")`, …) or a helper rule (a support
//! rule, or a match rule given before), inlined.
//!
//! Rust's regular expressions do not backtrack: an atomic group `(?>…)` is read as `(?:…)`, which
//! matches the same where an atomic group would not have to give anything back.
//!
//! A rule is read by grok's own matcher (`super::matcher`), its groups in one pass: `regex` reads
//! what that leaves to it (a loop that may match nothing, a search past the matcher's bound), and
//! finds the same.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use regex::Regex;
use serde_json::Value;

use super::{matcher, spare};

use kiln_obs::logs::buf::Attrs;

use kiln_obs::query::calendar::Zone;

/// How deep helper rules may nest (a rule that names itself stops here).
const DEPTH: usize = 16;
/// Attributes a match extracts at the most.
pub(crate) const ATTRS: usize = 256;
/// Rules a thread keeps capture locations for before it forgets them all (pipelines changed
/// many times over: their old rules' are of rules no longer read).
const READING: usize = 4096;

/// Each rule's own number, never another's, for what a thread keeps of it ([`Reading`]).
static RULES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GrokError {
    #[error("a rule is `name pattern`: {0}")]
    Rule(String),
    #[error("rule {0}: {1}")]
    In(String, String),
}

/// How an extracted value is read before its filter.
#[derive(Debug, Clone)]
enum Conv {
    Str,
    Int,
    Num,
    /// What reads as true (anything else of what matched, false).
    Bool(Vec<String>),
    Date(DateFmt),
    /// A quoted string: its quotes taken off.
    Quoted,
}

#[derive(Debug, Clone)]
enum Filter {
    Number,
    Integer,
    Boolean,
    NullIf(String),
    Json,
    KeyValue { sep: String },
    Lowercase,
    Uppercase,
    DecodeUri,
    QueryString,
    Url,
    Scale(f64),
    UserAgent,
}

#[derive(Debug, Clone)]
struct Cap {
    group: String,
    /// Its group's place in its rule's expression, found once (a name looked up per match hashes it).
    index: usize,
    name: Option<String>,
    conv: Conv,
    filter: Option<Filter>,
}

#[derive(Debug)]
struct Rule {
    id: u64,
    name: String,
    re: Regex,
    /// Its program, grok's own matcher's (none: `regex` reads it).
    prog: Option<matcher::Program>,
    caps: Vec<Cap>,
}

/// What reading values keeps on one thread: the matcher's room, and each rule's capture locations
/// for what `regex` reads, made once, a value's groups read into them with nothing allocated.
#[derive(Debug, Default)]
pub struct Reading {
    locs: HashMap<u64, regex::CaptureLocations>,
    matcher: matcher::Scratch,
    /// A value as its rule converts it, before a filter reads it.
    val: String,
}

/// Rules compiled.
#[derive(Debug)]
pub struct Grok {
    rules: Vec<Rule>,
}

/// `name pattern` lines (blank ones and `#` comments skipped).
fn rules_of(text: &str) -> Result<Vec<(String, String)>, GrokError> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, pattern) = line.split_once(char::is_whitespace).ok_or_else(|| GrokError::Rule(line.to_owned()))?;
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || "_.-".contains(c)) {
            return Err(GrokError::Rule(line.to_owned()));
        }
        out.push((name.to_owned(), pattern.trim_start().to_owned()));
    }
    Ok(out)
}

/// A token's parts: `matcher(args):name:filter(args)`.
#[derive(Debug, Default, PartialEq)]
struct Token {
    matcher: String,
    args: Vec<String>,
    name: Option<String>,
    filter: Option<(String, Vec<String>)>,
}

/// A quoted argument list `("a", "b\"c", 3)`: its values, and the bytes it took.
fn args(s: &str) -> Option<(Vec<String>, usize)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') {
        return Some((Vec::new(), 0));
    }
    let mut out = Vec::new();
    let mut i = 1;
    loop {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        match b.get(i)? {
            b')' => return Some((out, i + 1)),
            b',' => i += 1,
            q @ (b'"' | b'\'') => {
                let q = *q;
                let mut v = String::new();
                i += 1;
                loop {
                    match b.get(i)? {
                        b'\\' => {
                            // `\"` and `\\` are the quote and the backslash; any other escape is
                            // kept as it is (a regular expression's).
                            match b.get(i + 1)? {
                                c if *c == q || *c == b'\\' => v.push(char::from(*c)),
                                _ => {
                                    v.push('\\');
                                    let c = s.get(i + 1..)?.chars().next()?;
                                    v.push(c);
                                    i += c.len_utf8() - 1;
                                }
                            }
                            i += 2;
                        }
                        c if *c == q => {
                            i += 1;
                            break;
                        }
                        _ => {
                            let c = s.get(i..)?.chars().next()?;
                            v.push(c);
                            i += c.len_utf8();
                        }
                    }
                }
                out.push(v);
            }
            _ => {
                // A bare number or word.
                let end = s.get(i..)?.find([',', ')']).map(|k| i + k)?;
                out.push(s.get(i..end)?.trim().to_owned());
                i = end;
            }
        }
    }
}

/// The token at the start of `s` (after `%{`): its parts and the bytes it took, its `}` too.
fn token(s: &str) -> Option<(Token, usize)> {
    let mut t = Token::default();
    let end_name = s.find(['(', ':', '}'])?;
    t.matcher = s.get(..end_name)?.to_owned();
    let mut i = end_name;
    let (a, used) = args(s.get(i..)?)?;
    t.args = a;
    i += used;
    if s.get(i..)?.starts_with(':') {
        i += 1;
        let end = s.get(i..)?.find([':', '}']).map(|k| i + k)?;
        let name = s.get(i..end)?;
        t.name = (!name.is_empty()).then(|| name.to_owned());
        i = end;
        if s.get(i..)?.starts_with(':') {
            i += 1;
            let end = s.get(i..)?.find(['(', '}']).map(|k| i + k)?;
            let f = s.get(i..end)?.to_owned();
            i = end;
            let (a, used) = args(s.get(i..)?)?;
            i += used;
            t.filter = Some((f, a));
        }
    }
    s.get(i..)?.starts_with('}').then_some((t, i + 1))
}

fn matcher(name: &str, args: &[String]) -> Result<Option<(String, Conv)>, String> {
    let num = r"[+-]?(?:\d+(?:\.\d*)?|\.\d+)";
    Ok(Some(match name {
        "notSpace" => (r"\S+".into(), Conv::Str),
        "data" => (r".*?".into(), Conv::Str),
        "word" => (r"\b\w+\b".into(), Conv::Str),
        "integer" => (r"[+-]?\d+".into(), Conv::Int),
        "integerStr" => (r"[+-]?\d+".into(), Conv::Str),
        "integerExt" => (r"[+-]?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?".into(), Conv::Num),
        "integerExtStr" => (r"[+-]?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?".into(), Conv::Str),
        "number" => (num.into(), Conv::Num),
        "numberStr" => (num.into(), Conv::Str),
        "numberExt" => (format!(r"{num}(?:[eE][+-]?\d+)?"), Conv::Num),
        "numberExtStr" => (format!(r"{num}(?:[eE][+-]?\d+)?"), Conv::Str),
        "boolean" => match args {
            [t, f] => (format!("(?:{}|{})", regex::escape(t), regex::escape(f)), Conv::Bool(vec![t.clone()])),
            _ => ("(?i:true|false)".into(), Conv::Bool(vec!["true".into()])),
        },
        "doubleQuotedString" => (r#""(?:[^"\\]|\\.)*""#.into(), Conv::Quoted),
        "singleQuotedString" => (r"'(?:[^'\\]|\\.)*'".into(), Conv::Quoted),
        "quotedString" => (r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#.into(), Conv::Quoted),
        "uuid" => (r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}".into(), Conv::Str),
        "mac" => (r"(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}".into(), Conv::Str),
        "ipv4" => (IPV4.into(), Conv::Str),
        "ipv6" => (IPV6.into(), Conv::Str),
        "ip" => (format!("(?:{IPV4}|{IPV6})"), Conv::Str),
        "hostname" => (HOSTNAME.into(), Conv::Str),
        "ipOrHost" => (format!("(?:{IPV4}|{IPV6}|{HOSTNAME})"), Conv::Str),
        "port" => (r"\d{1,5}".into(), Conv::Str),
        "regex" => match args {
            [r] => (atomic(r).into_owned(), Conv::Str),
            _ => return Err("regex(\"…\"): one pattern".into()),
        },
        "date" => match args {
            [f, rest @ ..] => {
                let zone = match rest.first() {
                    Some(z) => Zone::named(z)?,
                    None => Zone::UTC,
                };
                let d = DateFmt::compile(f, zone)?;
                (d.regex.clone(), Conv::Date(d))
            }
            _ => return Err("date(\"pattern\"[, \"timezone\"]): a pattern".into()),
        },
        _ => return Ok(None),
    }))
}

const IPV4: &str = r"(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)";
const IPV6: &str = r"(?:[0-9A-Fa-f]{0,4}:){2,7}(?:[0-9A-Fa-f]{0,4}|(?:\d{1,3}\.){3}\d{1,3})";
const HOSTNAME: &str = r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*\.?";

/// A regular expression of Java's, as Rust's reads it: atomic groups as plain ones (a `(?>`
/// escaped, or in a class, is left as it is).
fn atomic(r: &str) -> Cow<'_, str> {
    if !r.contains("(?>") {
        return Cow::Borrowed(r);
    }
    let b = r.as_bytes();
    let mut out = String::with_capacity(r.len());
    // How deep in classes, and where the one open began its members (a `]` there is one).
    let (mut depth, mut members) = (0usize, 0usize);
    let (mut i, mut from) = (0usize, 0usize);
    while let Some(&c) = b.get(i) {
        match c {
            b'\\' => i += 1,
            b'[' => {
                depth += 1;
                members = i + 1 + usize::from(b.get(i + 1) == Some(&b'^'));
                i = members - 1;
            }
            b']' if depth > 0 && i != members => depth -= 1,
            b'(' if depth == 0 && b.get(i + 1..i + 3) == Some(b"?>") => {
                out.push_str(r.get(from..i).unwrap_or_default());
                out.push_str("(?:");
                from = i + 3;
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    out.push_str(r.get(from..).unwrap_or_default());
    Cow::Owned(out)
}

fn filter_of(name: &str, args: &[String]) -> Result<Filter, String> {
    Ok(match name {
        "number" => Filter::Number,
        "integer" => Filter::Integer,
        "boolean" => Filter::Boolean,
        "nullIf" => Filter::NullIf(args.first().cloned().unwrap_or_default()),
        "json" => Filter::Json,
        "keyvalue" => Filter::KeyValue { sep: args.first().cloned().unwrap_or_else(|| "=".into()) },
        "lowercase" => Filter::Lowercase,
        "uppercase" => Filter::Uppercase,
        "decodeuricomponent" => Filter::DecodeUri,
        "querystring" => Filter::QueryString,
        "url" => Filter::Url,
        "useragent" => Filter::UserAgent,
        "scale" => Filter::Scale(args.first().and_then(|a| a.parse().ok()).ok_or("scale(factor): a number")?),
        other => return Err(format!("a filter kiln does not have: {other}")),
    })
}

struct Compiler<'a> {
    helpers: Vec<(String, String)>,
    out: &'a mut String,
    caps: &'a mut Vec<Cap>,
    next: usize,
}

impl Compiler<'_> {
    /// `pattern` appended to the regular expression, its tokens' captures named afresh.
    fn pattern(&mut self, pattern: &str, depth: usize) -> Result<(), String> {
        if depth > DEPTH {
            return Err("helper rules nest too deep (one names itself?)".into());
        }
        let mut rest = pattern;
        while let Some(at) = rest.find("%{") {
            self.out.push_str(&atomic(rest.get(..at).unwrap_or_default()));
            let after = rest.get(at + 2..).unwrap_or_default();
            let (t, used) = token(after).ok_or_else(|| format!("a token that does not close: %{{{}", after.chars().take(40).collect::<String>()))?;
            rest = after.get(used..).unwrap_or_default();
            let filter = t.filter.as_ref().map(|(f, a)| filter_of(f, a)).transpose()?;
            match matcher(&t.matcher, &t.args)? {
                Some((fragment, conv)) => match &t.name {
                    Some(name) => {
                        let group = format!("g{}", self.next);
                        self.next += 1;
                        let _ = write!(self.out, "(?P<{group}>{fragment})");
                        self.caps.push(Cap { group, index: 0, name: Some(name.clone()), conv, filter });
                    }
                    // Nothing extracted: matched, and its filter (keyvalue, json) still read at
                    // the root.
                    None if filter.is_some() => {
                        let group = format!("g{}", self.next);
                        self.next += 1;
                        let _ = write!(self.out, "(?P<{group}>{fragment})");
                        self.caps.push(Cap { group, index: 0, name: None, conv, filter });
                    }
                    None => {
                        let _ = write!(self.out, "(?:{fragment})");
                    }
                },
                None => {
                    let helper = self.helpers.iter().find(|(n, _)| *n == t.matcher).map(|(_, p)| p.clone()).ok_or_else(|| format!("a matcher or rule kiln does not know: {}", t.matcher))?;
                    match &t.name {
                        Some(name) => {
                            let group = format!("g{}", self.next);
                            self.next += 1;
                            let _ = write!(self.out, "(?P<{group}>");
                            self.pattern(&helper, depth + 1)?;
                            self.out.push(')');
                            self.caps.push(Cap { group, index: 0, name: Some(name.clone()), conv: Conv::Str, filter });
                        }
                        None => {
                            self.out.push_str("(?:");
                            self.pattern(&helper, depth + 1)?;
                            self.out.push(')');
                        }
                    }
                }
            }
        }
        self.out.push_str(&atomic(rest));
        Ok(())
    }
}

impl Grok {
    pub fn compile(match_rules: &str, support_rules: &str) -> Result<Self, GrokError> {
        let support = rules_of(support_rules)?;
        let matches = rules_of(match_rules)?;
        let mut rules = Vec::new();
        for (i, (name, pattern)) in matches.iter().enumerate() {
            // Its helpers: the support rules, and the match rules given before it.
            let helpers: Vec<(String, String)> = support.iter().chain(matches.get(..i).unwrap_or_default()).cloned().collect();
            // The value whole: `^(?:…)$` (a rule's alternatives each the whole of it).
            let (mut out, mut caps) = (String::from("^(?:"), Vec::new());
            Compiler { helpers, out: &mut out, caps: &mut caps, next: 0 }.pattern(pattern, 0).map_err(|e| GrokError::In(name.clone(), e))?;
            out.push_str(")$");
            let re = regex::RegexBuilder::new(&out).size_limit(4 << 20).build().map_err(|e| GrokError::In(name.clone(), format!("{e}")))?;
            for cap in &mut caps {
                cap.index = re.capture_names().position(|n| n == Some(cap.group.as_str())).unwrap_or(0);
            }
            // The same expression as regex's own parser reads it (as `regex` read it above), and a
            // program of it that keeps the groups the rule reads.
            let prog = regex_syntax::ParserBuilder::new().build().parse(&out).ok().and_then(|hir| matcher::Program::compile(&hir, &|i| caps.iter().any(|c| c.index == i as usize)));
            rules.push(Rule { id: RULES.fetch_add(1, Ordering::Relaxed), name: name.clone(), re, prog, caps });
        }
        Ok(Self { rules })
    }

    /// The same rules read by `regex` alone: what the matcher is compared with.
    pub fn regex_only(mut self) -> Self {
        for r in &mut self.rules {
            r.prog = None;
        }
        self
    }

    /// The first rule that matches `value` whole: its name, and its attributes appended to `out`.
    pub fn parse(&self, value: &str, now_ns: i64, out: &mut Vec<(String, String)>) -> Option<&str> {
        let mut made = Attrs::default();
        let name = self.parse_with(value, now_ns, &mut Reading::default(), &mut made);
        out.extend(made.iter().map(|(k, v)| (k.to_owned(), v.to_owned())));
        name
    }

    /// [`Grok::parse`], with what this thread keeps of the rules it reads: its attributes written
    /// side by side into `out` (no string a value).
    pub fn parse_with(&self, value: &str, now_ns: i64, reading: &mut Reading, out: &mut Attrs) -> Option<&str> {
        if reading.locs.len() > READING {
            reading.locs.clear();
        }
        // Each rule's program reads its groups as it matches, in one pass; what it leaves to
        // `regex`, `regex` asks for its groups at once (its lazy DFA finds the match, its
        // backtracker reads the groups of that match alone).
        for rule in &self.rules {
            let found = match &rule.prog {
                Some(p) => p.search(value, &mut reading.matcher),
                None => matcher::Found::Unsure,
            };
            match found {
                matcher::Found::No => continue,
                matcher::Found::Yes => {
                    for cap in &rule.caps {
                        let Some(text) = reading.matcher.group(cap.index).and_then(|(a, b)| value.get(a..b)) else { continue };
                        if out.len() >= ATTRS {
                            break;
                        }
                        extract(cap, text, now_ns, out, &mut reading.val);
                    }
                }
                matcher::Found::Unsure => {
                    let locs = reading.locs.entry(rule.id).or_insert_with(|| rule.re.capture_locations());
                    if rule.re.captures_read(locs, value).is_none() {
                        continue;
                    }
                    for cap in &rule.caps {
                        let Some(text) = locs.get(cap.index).and_then(|(a, b)| value.get(a..b)) else { continue };
                        if out.len() >= ATTRS {
                            break;
                        }
                        extract(cap, text, now_ns, out, &mut reading.val);
                    }
                }
            }
            return Some(&rule.name);
        }
        None
    }
}

/// A number's text as grok writes one onto `out`: a whole one an integer, else as it is; none (false)
/// for one not finite.
fn number_into(f: f64, out: &mut String) -> bool {
    if !f.is_finite() {
        return false;
    }
    if f.fract() == 0.0 && f.abs() < 9.0e15 {
        int_into(f as i64, out);
    } else {
        let _ = write!(out, "{f}");
    }
    true
}

/// `{prefix}.{key}` onto `t` (`key` alone without a prefix).
fn key_into(t: &mut String, prefix: Option<&str>, key: &str) {
    if let Some(p) = prefix {
        t.push_str(p);
        t.push('.');
    }
    t.push_str(key);
}

/// A token's value, read and filtered, into `out` (under its name; a filter that makes fields,
/// under the name too, or at the root without one): written where it is kept, a value converted
/// first into `val` where a filter reads it.
fn extract(cap: &Cap, raw: &str, now_ns: i64, out: &mut Attrs, val: &mut String) {
    // A text the filter drops (`nullIf("-")`): nothing made of it.
    if let (Conv::Str, Some(Filter::NullIf(x))) = (&cap.conv, &cap.filter)
        && raw == x
    {
        return;
    }
    // The commonest, a text kept as it is: written once, where it is kept.
    if let (Conv::Str, None) = (&cap.conv, &cap.filter) {
        if let Some(n) = &cap.name {
            out.push(n, raw);
        }
        return;
    }
    val.clear();
    let converted = match &cap.conv {
        Conv::Str => {
            val.push_str(raw);
            true
        }
        Conv::Int => int_canonical_into(raw, val),
        Conv::Num => raw.parse::<f64>().ok().is_some_and(|f| number_into(f, val)),
        Conv::Bool(t) => {
            val.push_str(if t.iter().any(|x| x.eq_ignore_ascii_case(raw)) { "true" } else { "false" });
            true
        }
        Conv::Date(d) => d.read(raw, now_ns).map(|ns| int_into(ns, val)).is_some(),
        Conv::Quoted => {
            let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
            if inner.contains('\\') {
                val.push_str(&inner.replace("\\\"", "\"").replace("\\'", "'"));
            } else {
                val.push_str(inner);
            }
            true
        }
    };
    if !converted {
        return;
    }
    let value = val.as_str();
    let name = cap.name.as_deref();
    // The value under its name, as `write` writes it (false: none).
    let named = |out: &mut Attrs, write: &dyn Fn(&mut String) -> bool| {
        if let Some(n) = name {
            out.push_with(|t| t.push_str(n), write);
        }
    };
    match &cap.filter {
        None => named(out, &|t| {
            t.push_str(value);
            true
        }),
        Some(Filter::Number) => named(out, &|t| value.parse::<f64>().ok().is_some_and(|f| number_into(f, t))),
        Some(Filter::Integer) => named(out, &|t| value.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| int_into(f as i64, t)).is_some()),
        Some(Filter::Scale(k)) => named(out, &|t| value.parse::<f64>().ok().is_some_and(|f| number_into(f * k, t))),
        Some(Filter::Boolean) => named(out, &|t| {
            t.push_str(if value.eq_ignore_ascii_case("true") { "true" } else { "false" });
            true
        }),
        Some(Filter::NullIf(x)) => {
            if value != *x {
                named(out, &|t| {
                    t.push_str(value);
                    true
                });
            }
        }
        Some(Filter::Lowercase) => named(out, &|t| {
            t.push_str(&value.to_lowercase());
            true
        }),
        Some(Filter::Uppercase) => named(out, &|t| {
            t.push_str(&value.to_uppercase());
            true
        }),
        Some(Filter::DecodeUri) => named(out, &|t| {
            decode_uri_into(value, t);
            true
        }),
        Some(Filter::Json) => {
            if let Ok(v) = serde_json::from_str::<Value>(value) {
                flatten_into(name, &v, out);
            }
        }
        Some(Filter::KeyValue { sep }) => key_values(value, sep, name, out),
        Some(Filter::QueryString) => query_pairs(value.trim_start_matches('?'), name, out),
        Some(Filter::Url) => url_parts(value, name, false, out),
        Some(Filter::UserAgent) => user_agent(value, name, out),
    }
}

/// JSON's fields as attributes, nested keys joined by dots, arrays as JSON.
pub(crate) fn flatten(prefix: Option<&str>, v: Value, depth: usize, out: &mut Vec<(String, String)>) {
    if out.len() >= ATTRS {
        return;
    }
    match v {
        Value::Object(o) if depth < 6 => {
            for (k, v) in o {
                let key = match prefix {
                    Some(p) => format!("{p}.{k}"),
                    None => k,
                };
                flatten(Some(&key), v, depth + 1, out);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            if let Some(p) = prefix {
                out.push((p.to_owned(), s));
            }
        }
        other => {
            if let Some(p) = prefix {
                out.push((p.to_owned(), other.to_string()));
            }
        }
    }
}

/// [`flatten`] into attributes side by side, under `prefix` where there is one.
fn flatten_into(prefix: Option<&str>, v: &Value, out: &mut Attrs) {
    let mut key = spare::take();
    if let Some(p) = prefix {
        key.push_str(p);
    }
    flatten_at(&mut key, prefix.is_some(), v, 0, out);
    spare::give(key);
}

/// [`flatten_into`]'s value at `key` (named: it is not the root).
fn flatten_at(key: &mut String, named: bool, v: &Value, depth: usize, out: &mut Attrs) {
    if out.len() >= ATTRS {
        return;
    }
    match v {
        Value::Object(o) if depth < 6 => {
            for (k, v) in o {
                let at = key.len();
                if named {
                    key.push('.');
                }
                key.push_str(k);
                flatten_at(key, true, v, depth + 1, out);
                key.truncate(at);
            }
        }
        Value::Null => {}
        Value::String(s) => {
            if named {
                out.push(key, s);
            }
        }
        other => {
            if named {
                out.push_with(
                    |t| t.push_str(key),
                    |t| {
                        let _ = write!(t, "{other}");
                        true
                    },
                );
            }
        }
    }
}

/// `key<sep>value` pairs (a value quoted, or up to a space, a comma or a semicolon) into `out`, each
/// key under `prefix` where there is one.
fn key_values(s: &str, sep: &str, prefix: Option<&str>, out: &mut Attrs) {
    let mut rest = s;
    let mut n = 0;
    while let Some(at) = rest.find(sep) {
        let before = rest.get(..at).unwrap_or_default();
        let key = before.rsplit(|c: char| c.is_whitespace() || c == ',' || c == ';').next().unwrap_or_default();
        let after = rest.get(at + sep.len()..).unwrap_or_default();
        let (value, used) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let body = after.get(1..).unwrap_or_default();
                let end = body.find(q).unwrap_or(body.len());
                (body.get(..end).unwrap_or_default(), (end + 2).min(after.len()))
            }
            _ => {
                let end = after.find(|c: char| c.is_whitespace() || c == ',' || c == ';').unwrap_or(after.len());
                (after.get(..end).unwrap_or_default(), end)
            }
        };
        if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || "_.@-".contains(c)) && n < ATTRS {
            out.push_with(
                |t| key_into(t, prefix, key),
                |t| {
                    t.push_str(value);
                    true
                },
            );
            n += 1;
        }
        rest = after.get(used..).unwrap_or_default();
        if used == 0 {
            break;
        }
    }
}

/// `s` with its `%XX` escapes and `+`s decoded, onto `t`: what decodes to no UTF-8 written as
/// `from_utf8_lossy` writes it.
pub(crate) fn decode_uri_into(s: &str, t: &mut String) {
    if !s.bytes().any(|b| b == b'%' || b == b'+') {
        t.push_str(s);
        return;
    }
    let b = s.as_bytes();
    let start = t.len();
    let mut out = std::mem::take(t).into_bytes();
    out.reserve(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'%' => match b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 3;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    *t = match String::from_utf8(out) {
        Ok(s) => s,
        // What was there before was text: the rest made text as `from_utf8_lossy` makes it.
        Err(e) => {
            let mut bytes = e.into_bytes();
            let decoded = bytes.split_off(start.min(bytes.len()));
            let mut s = String::from_utf8(bytes).unwrap_or_default();
            s.push_str(&String::from_utf8_lossy(&decoded));
            s
        }
    };
}

/// A query string's pairs, decoded, into `out` (under `prefix` where there is one).
pub(crate) fn query_pairs(q: &str, prefix: Option<&str>, out: &mut Attrs) {
    for p in q.split('&').filter(|p| !p.is_empty()).take(ATTRS) {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        out.push_with(
            |t| {
                if let Some(p) = prefix {
                    t.push_str(p);
                    t.push('.');
                }
                decode_uri_into(k, t);
            },
            |t| {
                decode_uri_into(v, t);
                true
            },
        );
    }
}

/// A URL's parts, as Datadog's URL parser names them, into `out` (under `prefix` where there is
/// one): `scheme`, `host`, `port`, `path` (its ending slashes trimmed where `trim_path` says, as the
/// parser's `normalize_ending_slashes` does), `queryString.<key>`.
/// Where `://` first is in `url`: from each `:`, whether two `/` follow (the two-way searcher
/// `str::find` makes for it costs more to set up than a URL takes to read, §12.17's profile).
fn scheme_end(url: &str) -> Option<usize> {
    let b = url.as_bytes();
    let mut from = 0;
    while let Some(i) = memchr::memchr(b':', b.get(from..)?) {
        let at = from + i;
        if b.get(at + 1..at + 3) == Some(b"//") {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

pub(crate) fn url_parts(url: &str, prefix: Option<&str>, trim_path: bool, out: &mut Attrs) {
    let put = |out: &mut Attrs, k: &str, v: &str| {
        out.push_with(
            |t| key_into(t, prefix, k),
            |t| {
                t.push_str(v);
                true
            },
        );
    };
    let (scheme, rest) = match scheme_end(url).and_then(|i| Some((url.get(..i)?, url.get(i + 3..)?))) {
        Some((s, r)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) => (Some(s), r),
        _ => (None, url),
    };
    if let Some(s) = scheme {
        put(out, "scheme", s);
    }
    let (authority, path_query) = if scheme.is_some() {
        match rest.find(['/', '?', '#']) {
            Some(i) => (rest.get(..i).unwrap_or_default(), rest.get(i..).unwrap_or_default()),
            None => (rest, ""),
        }
    } else {
        ("", rest)
    };
    if !authority.is_empty() {
        let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        match host_port.rsplit_once(':').filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
            Some((h, p)) => {
                put(out, "host", h.trim_start_matches('[').trim_end_matches(']'));
                put(out, "port", p);
            }
            None => put(out, "host", host_port),
        }
    }
    let path_query = path_query.split('#').next().unwrap_or_default();
    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
    if !path.is_empty() {
        let kept = if trim_path && path.len() > 1 { path.get(..path.trim_end_matches('/').len()).unwrap_or(path) } else { path };
        put(out, "path", kept);
    }
    for p in query.split('&').filter(|p| !p.is_empty()).take(ATTRS) {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        out.push_with(
            |t| {
                key_into(t, prefix, "queryString.");
                decode_uri_into(k, t);
            },
            |t| {
                decode_uri_into(v, t);
                true
            },
        );
    }
}

/// The words a user agent is read by, their case as written: the first eight the keys a version
/// follows (their first places kept).
const UA_WORDS: [&str; 18] =
    ["Edg/", "OPR/", "Firefox/", "Chrome/", "Version/", "curl/", "Chromium/", "Safari/", "Windows", "iPhone", "iPad", "iOS", "Mac OS X", "Macintosh", "Android", "Linux", "Mobile", "Tablet"];
/// A bot's words, whatever their case.
const BOT_WORDS: [&str; 9] = ["bot", "crawler", "spider", "slurp", "curl/", "wget/", "python-requests", "go-http-client", "okhttp"];
/// The keys a version follows: the first of [`UA_WORDS`].
const UA_KEYS: usize = 6;

/// For each byte, the words (bits: [`UA_WORDS`]'s, then [`BOT_WORDS`]'s) that may begin with it.
#[allow(clippy::indexing_slicing)]
const fn ua_starts() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut k = 0;
    while k < UA_WORDS.len() {
        t[UA_WORDS[k].as_bytes()[0] as usize] |= 1 << k;
        k += 1;
    }
    let mut j = 0;
    while j < BOT_WORDS.len() {
        let b = BOT_WORDS[j].as_bytes()[0];
        t[b as usize] |= 1 << (UA_WORDS.len() + j);
        t[b.to_ascii_uppercase() as usize] |= 1 << (UA_WORDS.len() + j);
        j += 1;
    }
    t
}

static UA_STARTS: [u32; 256] = ua_starts();

/// A word's place among [`UA_WORDS`], found as the program is compiled (past them for a word that
/// is none of them: a test holds that each named below is one).
#[allow(clippy::indexing_slicing)]
const fn ua_word(w: &str) -> usize {
    let w = w.as_bytes();
    let mut k = 0;
    while k < UA_WORDS.len() {
        let x = UA_WORDS[k].as_bytes();
        if x.len() == w.len() {
            let mut i = 0;
            while i < x.len() && x[i] == w[i] {
                i += 1;
            }
            if i == x.len() {
                return k;
            }
        }
        k += 1;
    }
    UA_WORDS.len()
}

const EDG: usize = ua_word("Edg/");
const OPR: usize = ua_word("OPR/");
const FIREFOX: usize = ua_word("Firefox/");
const CHROME: usize = ua_word("Chrome/");
const VERSION: usize = ua_word("Version/");
const CURL: usize = ua_word("curl/");
const CHROMIUM: usize = ua_word("Chromium/");
const SAFARI: usize = ua_word("Safari/");
const WINDOWS: usize = ua_word("Windows");
const IPHONE: usize = ua_word("iPhone");
const IPAD: usize = ua_word("iPad");
const IOS: usize = ua_word("iOS");
const MAC_OS_X: usize = ua_word("Mac OS X");
const MACINTOSH: usize = ua_word("Macintosh");
const ANDROID: usize = ua_word("Android");
const LINUX: usize = ua_word("Linux");
const MOBILE: usize = ua_word("Mobile");
const TABLET: usize = ua_word("Tablet");

/// Whether `b` begins with `w`, a byte at a time: a word of a few bytes, most told apart at its
/// second (fewer than a call to `memcmp` costs, §12.17's profile).
#[inline]
fn begins_with(b: &[u8], w: &[u8]) -> bool {
    b.len() >= w.len() && b.iter().zip(w).all(|(x, y)| x == y)
}

/// A user agent's browser, operating system and device, as far as its words tell (the families
/// Datadog's parser names, not its full database). Its words found in one pass: at each byte, the
/// words that may begin with it (a word found is not looked for again).
pub(crate) fn user_agent(ua: &str, prefix: Option<&str>, out: &mut Attrs) {
    let put = |out: &mut Attrs, k: &str, v: &str| {
        out.push_with(
            |t| key_into(t, prefix, k),
            |t| {
                t.push_str(v);
                true
            },
        );
    };
    let b = ua.as_bytes();
    let mut found = 0u32;
    let mut first = [0usize; UA_KEYS];
    for (i, &c) in b.iter().enumerate() {
        let mut m = UA_STARTS.get(usize::from(c)).copied().unwrap_or(0) & !found;
        while m != 0 {
            let k = m.trailing_zeros() as usize;
            m &= m - 1;
            let rest = b.get(i..).unwrap_or_default();
            let hit = match UA_WORDS.get(k) {
                Some(w) => begins_with(rest, w.as_bytes()),
                None => BOT_WORDS.get(k - UA_WORDS.len()).is_some_and(|w| rest.get(..w.len()).is_some_and(|r| r.eq_ignore_ascii_case(w.as_bytes()))),
            };
            if hit {
                found |= 1 << k;
                if let Some(f) = first.get_mut(k) {
                    *f = i;
                }
            }
        }
    }
    let has = |k: usize| k < UA_WORDS.len() && found & (1 << k) != 0;
    // A version's major part: of its digits and dots, those before the first dot (none where
    // neither follows its key).
    let version = |k: usize| -> Option<&str> {
        if !has(k) {
            return None;
        }
        let rest = ua.get(first.get(k)? + UA_WORDS.get(k)?.len()..)?;
        let run = rest.bytes().take_while(|c| c.is_ascii_digit() || *c == b'.').count();
        let major = rest.bytes().take_while(u8::is_ascii_digit).count();
        if run == 0 { None } else { rest.get(..major) }
    };
    let bot = found >> UA_WORDS.len() != 0;
    let browser = if has(EDG) {
        Some(("Edge", version(EDG)))
    } else if has(OPR) {
        Some(("Opera", version(OPR)))
    } else if has(FIREFOX) {
        Some(("Firefox", version(FIREFOX)))
    } else if has(CHROME) && !has(CHROMIUM) {
        Some(("Chrome", version(CHROME)))
    } else if has(SAFARI) && has(VERSION) {
        Some(("Safari", version(VERSION)))
    } else if ua.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("curl/")) {
        Some(("curl", version(CURL)))
    } else {
        None
    };
    if let Some((family, v)) = browser {
        put(out, "browser.family", family);
        if let Some(v) = v {
            put(out, "browser.major", v);
        }
    }
    let os = if has(WINDOWS) {
        Some("Windows")
    } else if has(IPHONE) || has(IPAD) || has(IOS) {
        Some("iOS")
    } else if has(MAC_OS_X) || has(MACINTOSH) {
        Some("Mac OS X")
    } else if has(ANDROID) {
        Some("Android")
    } else if has(LINUX) {
        Some("Linux")
    } else {
        None
    };
    if let Some(os) = os {
        put(out, "os.family", os);
    }
    let device = if bot {
        "Bot"
    } else if has(MOBILE) || has(IPHONE) || has(ANDROID) {
        "Mobile"
    } else if has(IPAD) || has(TABLET) {
        "Tablet"
    } else {
        "Desktop"
    };
    put(out, "device.category", device);
}

/// An integer's decimal text onto `out`, without the formatter's machinery.
pub(crate) fn int_into(n: i64, out: &mut String) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    let mut m = n.unsigned_abs();
    loop {
        at -= 1;
        if let Some(d) = buf.get_mut(at) {
            *d = b'0' + (m % 10) as u8;
        }
        m /= 10;
        if m == 0 {
            break;
        }
    }
    let digits = buf.get(at..).unwrap_or_default();
    if n < 0 {
        out.push('-');
    }
    // ASCII digits, written at once (not a character at a time).
    out.push_str(std::str::from_utf8(digits).unwrap_or_default());
}

/// An integer's text as `i64`'s parse and its text say it (a `+` and leading zeros gone) onto
/// `out`: the text as it is when that is already it; none (false) where it is no integer.
fn int_canonical_into(raw: &str, out: &mut String) -> bool {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let plain = !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()) && (digits.len() == 1 || !digits.starts_with('0')) && raw != "-0";
    if plain {
        out.push_str(raw);
        return true;
    }
    raw.trim_start_matches('+').parse::<i64>().ok().map(|n| int_into(n, out)).is_some()
}

// ------------------------------------------------------------------------------------- dates

const MONTH_NAMES: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Part {
    Year,
    Year2,
    Month,
    MonthName,
    Day,
    Hour,
    Hour12,
    Minute,
    Second,
    Fraction,
    AmPm,
    Offset,
    ZoneId,
    Skip,
}

/// The most parts a date pattern may have (a date has a dozen at the most).
const DATE_PARTS: usize = 24;

/// What text one of a date's parts takes, as its expression reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Shape {
    Digits(u8, u8),
    Letters(u8, u8),
    /// `[AaPp][Mm]`
    AmPm,
    /// `Z|[+-]\d{4}|[+-]\d{2}`
    Offset,
    /// `Z|[+-]\d{2}:\d{2}`
    OffsetColon,
    /// `Z|[+-]\d{2}:?\d{2}|[+-]\d{2}`
    OffsetEither,
    /// `[A-Za-z_]+(?:/[A-Za-z_+-]+)*`
    ZoneName,
}

impl Shape {
    /// How many of `b`'s first bytes it takes as its expression would, first choice first, each
    /// part as long as it may be (none: it takes none).
    fn take(self, b: &[u8]) -> Option<usize> {
        let run = |from: usize, max: usize, ok: fn(&u8) -> bool| b.get(from..).map_or(0, |r| r.iter().take(max).take_while(|c| ok(c)).count());
        let digits_at = |from: usize, n: usize| run(from, n, u8::is_ascii_digit) == n;
        let sign = matches!(b.first(), Some(b'+' | b'-'));
        match self {
            Self::Digits(min, max) | Self::Letters(min, max) => {
                let ok: fn(&u8) -> bool = if let Self::Digits(..) = self { u8::is_ascii_digit } else { u8::is_ascii_alphabetic };
                let n = run(0, usize::from(max), ok);
                (n >= usize::from(min)).then_some(n)
            }
            Self::AmPm => (matches!(b.first(), Some(b'A' | b'a' | b'P' | b'p')) && matches!(b.get(1), Some(b'M' | b'm'))).then_some(2),
            Self::Offset | Self::OffsetColon | Self::OffsetEither if b.first() == Some(&b'Z') => Some(1),
            Self::Offset if sign && digits_at(1, 4) => Some(5),
            Self::Offset => (sign && digits_at(1, 2)).then_some(3),
            Self::OffsetColon => (sign && digits_at(1, 2) && b.get(3) == Some(&b':') && digits_at(4, 2)).then_some(6),
            Self::OffsetEither if sign && digits_at(1, 2) && b.get(3) == Some(&b':') && digits_at(4, 2) => Some(6),
            Self::OffsetEither if sign && digits_at(1, 4) => Some(5),
            Self::OffsetEither => (sign && digits_at(1, 2)).then_some(3),
            Self::ZoneName => {
                let name = |c: &u8| c.is_ascii_alphabetic() || *c == b'_';
                let mut at = run(0, usize::MAX, name);
                if at == 0 {
                    return None;
                }
                while b.get(at) == Some(&b'/') {
                    let more = run(at + 1, usize::MAX, |c| c.is_ascii_alphabetic() || matches!(c, b'_' | b'+' | b'-'));
                    if more == 0 {
                        break;
                    }
                    at += 1 + more;
                }
                Some(at)
            }
        }
    }
}

/// A date pattern's pieces, to read a date without its expression: literal text, or a part.
#[derive(Debug, Clone)]
enum Piece {
    Text(String),
    Part(Shape),
}

/// A date pattern of Java's (`dd/MMM/yyyy:HH:mm:ss Z`): a regular expression of its parts, in
/// order, and how each is read; and its pieces, which read most dates without the expression.
#[derive(Debug, Clone)]
pub(crate) struct DateFmt {
    pub regex: String,
    parts: Vec<Part>,
    pieces: Vec<Piece>,
    re: Regex,
    zone: Zone,
}

impl DateFmt {
    pub(crate) fn compile(pattern: &str, zone: Zone) -> Result<Self, String> {
        // Its own expression (a group a part, to read them) and the one a rule holds (no groups).
        let (mut own, mut regex) = (String::new(), String::new());
        let mut parts = Vec::new();
        let mut pieces: Vec<Piece> = Vec::new();
        let text = |pieces: &mut Vec<Piece>, t: &str| match pieces.last_mut() {
            Some(Piece::Text(last)) => last.push_str(t),
            _ => pieces.push(Piece::Text(t.to_owned())),
        };
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = 0;
        while let Some(&c) = chars.get(i) {
            if c == '\'' {
                // A quoted literal; `''` a quote.
                let mut j = i + 1;
                let mut lit = String::new();
                while let Some(&d) = chars.get(j) {
                    if d == '\'' {
                        if chars.get(j + 1) == Some(&'\'') {
                            lit.push('\'');
                            j += 2;
                            continue;
                        }
                        break;
                    }
                    lit.push(d);
                    j += 1;
                }
                if lit.is_empty() && chars.get(i + 1) == Some(&'\'') {
                    lit.push('\'');
                }
                own.push_str(&regex::escape(&lit));
                regex.push_str(&regex::escape(&lit));
                text(&mut pieces, &lit);
                i = j + 1;
                continue;
            }
            if !c.is_ascii_alphabetic() {
                own.push_str(&regex::escape(&c.to_string()));
                regex.push_str(&regex::escape(&c.to_string()));
                text(&mut pieces, &c.to_string());
                i += 1;
                continue;
            }
            let mut n = 1;
            while chars.get(i + n) == Some(&c) {
                n += 1;
            }
            let (re, part, shape) = match (c, n) {
                ('y' | 'u', 2) => (r"\d{2}", Part::Year2, Shape::Digits(2, 2)),
                ('y' | 'u', _) => (r"\d{4}", Part::Year, Shape::Digits(4, 4)),
                ('M', 1 | 2) => (r"\d{1,2}", Part::Month, Shape::Digits(1, 2)),
                ('M', 3) => (r"[A-Za-z]{3}", Part::MonthName, Shape::Letters(3, 3)),
                ('M', _) => (r"[A-Za-z]{3,9}", Part::MonthName, Shape::Letters(3, 9)),
                ('d', _) => (r"\d{1,2}", Part::Day, Shape::Digits(1, 2)),
                ('H' | 'k', _) => (r"\d{1,2}", Part::Hour, Shape::Digits(1, 2)),
                ('h' | 'K', _) => (r"\d{1,2}", Part::Hour12, Shape::Digits(1, 2)),
                ('m', _) => (r"\d{1,2}", Part::Minute, Shape::Digits(1, 2)),
                ('s', _) => (r"\d{1,2}", Part::Second, Shape::Digits(1, 2)),
                ('S', _) => (r"\d{1,9}", Part::Fraction, Shape::Digits(1, 9)),
                ('a', _) => (r"[AaPp][Mm]", Part::AmPm, Shape::AmPm),
                ('E', 1..=3) => (r"[A-Za-z]{3}", Part::Skip, Shape::Letters(3, 3)),
                ('E', _) => (r"[A-Za-z]{3,9}", Part::Skip, Shape::Letters(3, 9)),
                ('Z', 1) | ('x', 1 | 2) => (r"Z|[+-]\d{4}|[+-]\d{2}", Part::Offset, Shape::Offset),
                ('Z', 2) | ('X' | 'x', 3) => (r"Z|[+-]\d{2}:\d{2}", Part::Offset, Shape::OffsetColon),
                ('X', _) => (r"Z|[+-]\d{2}:?\d{2}|[+-]\d{2}", Part::Offset, Shape::OffsetEither),
                ('Z', _) | ('V', _) => (r"[A-Za-z_]+(?:/[A-Za-z_+-]+)*", Part::ZoneId, Shape::ZoneName),
                ('z', _) => (r"[A-Za-z]{1,6}", Part::ZoneId, Shape::Letters(1, 6)),
                _ => return Err(format!("a date pattern's letter kiln does not read: {}", c.to_string().repeat(n))),
            };
            let _ = write!(own, "({re})");
            let _ = write!(regex, "(?:{re})");
            parts.push(part);
            pieces.push(Piece::Part(shape));
            i += n;
        }
        if parts.len() > DATE_PARTS {
            return Err(format!("a date pattern of {} parts: {DATE_PARTS} at the most", parts.len()));
        }
        let re = Regex::new(&format!("^{own}$")).map_err(|e| e.to_string())?;
        Ok(Self { regex, parts, pieces, re, zone })
    }

    /// Each part's text in `s`, read by the pieces the way its expression reads them first (each
    /// part as long as it may be, an alternative's first choice first): where that reads `s` whole,
    /// the expression's own first reading, the same; else false (the expression is asked).
    fn scan<'s>(&self, s: &'s str, out: &mut [&'s str; DATE_PARTS]) -> bool {
        let b = s.as_bytes();
        let (mut at, mut k) = (0usize, 0usize);
        for piece in &self.pieces {
            let rest = b.get(at..).unwrap_or_default();
            let n = match piece {
                Piece::Text(t) if rest.starts_with(t.as_bytes()) => t.len(),
                Piece::Text(_) => return false,
                Piece::Part(shape) => {
                    let Some(n) = shape.take(rest) else { return false };
                    let (Some(slot), Some(text)) = (out.get_mut(k), s.get(at..at + n)) else { return false };
                    *slot = text;
                    k += 1;
                    n
                }
            };
            at += n;
        }
        at == b.len()
    }

    /// A date read: ms since the epoch. A year left out is the one it is (or the last, should that
    /// put it more than a day ahead); a zone left out is the pattern's timezone.
    pub(crate) fn read(&self, s: &str, now_ns: i64) -> Option<i64> {
        let mut vals: [&str; DATE_PARTS] = [""; DATE_PARTS];
        if !self.scan(s, &mut vals) {
            self.read_expression(s, &mut vals)?;
        }
        self.reckon(&vals, now_ns)
    }

    /// Each part's text in `s` as its expression reads it.
    fn read_expression<'s>(&self, s: &'s str, vals: &mut [&'s str; DATE_PARTS]) -> Option<()> {
        let c = self.re.captures(s)?;
        for (k, v) in vals.iter_mut().enumerate().take(self.parts.len()) {
            *v = c.get(k + 1)?.as_str();
        }
        Some(())
    }

    /// The date its parts' texts say: ms since the epoch.
    fn reckon(&self, vals: &[&str; DATE_PARTS], now_ns: i64) -> Option<i64> {
        let (mut y, mut mo, mut d, mut h, mut mi, mut sec, mut nanos) = (None, 1i8, 1i8, 0i8, 0i8, 0i8, 0i32);
        let (mut pm, mut twelve) = (None, false);
        let mut offset: Option<i64> = None;
        let mut zone = self.zone;
        for (k, part) in self.parts.iter().enumerate() {
            let v = *vals.get(k)?;
            match part {
                Part::Year => y = Some(v.parse::<i16>().ok()?),
                Part::Year2 => y = Some(2000 + v.parse::<i16>().ok()?),
                Part::Month => mo = v.parse().ok()?,
                Part::MonthName => {
                    let named = |m: &&str| v.len() >= 3 && m.as_bytes().get(..v.len()).is_some_and(|p| p.eq_ignore_ascii_case(v.as_bytes()));
                    mo = i8::try_from(MONTH_NAMES.iter().position(named)? + 1).ok()?;
                }
                Part::Day => d = v.parse().ok()?,
                Part::Hour => h = v.parse().ok()?,
                Part::Hour12 => {
                    h = v.parse().ok()?;
                    twelve = true;
                }
                Part::Minute => mi = v.parse().ok()?,
                Part::Second => sec = v.parse().ok()?,
                Part::Fraction => nanos = v.parse::<i32>().ok()? * 10i32.pow(9u32.saturating_sub(u32::try_from(v.len()).ok()?)),
                Part::AmPm => pm = Some(v.eq_ignore_ascii_case("pm")),
                Part::Offset => {
                    offset = Some(if v == "Z" {
                        0
                    } else {
                        let sign = if v.starts_with('-') { -1 } else { 1 };
                        // Its digits, the colon left out: hours, then minutes when there are more.
                        let (mut n, mut count, mut hh) = (0i64, 0usize, None);
                        for d in v.bytes().filter(u8::is_ascii_digit) {
                            n = n * 10 + i64::from(d - b'0');
                            count += 1;
                            if count == 2 {
                                hh = Some(n);
                                n = 0;
                            }
                        }
                        let hh = hh?;
                        let mm = if count > 2 { n } else { 0 };
                        sign * (hh * 3600 + mm * 60)
                    });
                }
                Part::ZoneId => {
                    if ["utc", "gmt", "z"].iter().any(|z| v.eq_ignore_ascii_case(z)) {
                        offset = Some(0);
                    } else if let Ok(z) = Zone::named(v) {
                        zone = z;
                    }
                }
                Part::Skip => {}
            }
        }
        if twelve || pm.is_some() {
            h %= 12;
            if pm == Some(true) {
                h += 12;
            }
        }
        // UTC, and a time with its offset, reckoned here: only a named zone's rules are looked up.
        let utc = |year: i16| -> Option<i64> {
            let days_in = match mo {
                2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                1..=12 => 31,
                _ => return None,
            };
            if d < 1 || d > days_in || h > 23 || mi > 59 || sec > 60 {
                return None;
            }
            let days = kiln_obs::query::calendar::days_from_civil(i64::from(year), i64::from(mo), i64::from(d));
            // Past what nanoseconds since the epoch hold (the year 2262 on): no date.
            let seconds = days.checked_mul(86_400)?.checked_add(i64::from(h) * 3_600 + i64::from(mi) * 60 + i64::from(sec))?;
            seconds.checked_mul(1_000_000_000)?.checked_add(i64::from(nanos))
        };
        let at = |year: i16| -> Option<i64> {
            match offset {
                Some(off) => utc(year)?.checked_sub(off.checked_mul(1_000_000_000)?),
                None if zone == Zone::UTC => utc(year),
                None => zone.civil_ns(year, mo, d, h, mi, sec, nanos),
            }
        };
        let ns = match y {
            Some(year) => at(year)?,
            None => {
                let year = if zone == Zone::UTC && offset.is_none_or(|o| o == 0) {
                    i16::try_from(kiln_obs::query::calendar::civil_from_days(now_ns.div_euclid(86_400_000_000_000)).0).ok()?
                } else {
                    zone.year_at(now_ns)?
                };
                let t = at(year)?;
                if t > now_ns + 86_400_000_000_000 { at(year - 1)? } else { t }
            }
        };
        Some(ns.div_euclid(1_000_000))
    }
}

/// The code the extraction side by side replaced (`docs/observe.md` §12.17), each value a string of
/// its own: what it is held to.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod before {
    use std::borrow::Cow;
    use std::fmt::Write as _;

    use serde_json::Value;

    use super::{ATTRS, Attrs, Cap, Conv, Filter, Grok, READING, Reading, flatten, matcher, spare};

    /// [`Grok::parse_with`] as it was: each value extracted as a string of its own.
    pub(crate) fn parse<'g>(g: &'g Grok, value: &str, now_ns: i64, reading: &mut Reading, out: &mut Vec<(String, String)>) -> Option<&'g str> {
        if reading.locs.len() > READING {
            reading.locs.clear();
        }
        for rule in &g.rules {
            let found = match &rule.prog {
                Some(p) => p.search(value, &mut reading.matcher),
                None => matcher::Found::Unsure,
            };
            match found {
                matcher::Found::No => continue,
                matcher::Found::Yes => {
                    for cap in &rule.caps {
                        let Some(text) = reading.matcher.group(cap.index).and_then(|(a, b)| value.get(a..b)) else { continue };
                        if out.len() >= ATTRS {
                            break;
                        }
                        extract(cap, text, now_ns, out);
                    }
                }
                matcher::Found::Unsure => {
                    let locs = reading.locs.entry(rule.id).or_insert_with(|| rule.re.capture_locations());
                    if rule.re.captures_read(locs, value).is_none() {
                        continue;
                    }
                    for cap in &rule.caps {
                        let Some(text) = locs.get(cap.index).and_then(|(a, b)| value.get(a..b)) else { continue };
                        if out.len() >= ATTRS {
                            break;
                        }
                        extract(cap, text, now_ns, out);
                    }
                }
            }
            return Some(&rule.name);
        }
        None
    }

    fn number_text(f: f64) -> Option<String> {
        if !f.is_finite() {
            return None;
        }
        if f.fract() == 0.0 && f.abs() < 9.0e15 {
            return Some(int_text(f as i64));
        }
        let mut s = spare::take();
        let _ = write!(s, "{f}");
        Some(s)
    }

    /// A token's value, read and filtered, into `out` (under its name; a filter that makes fields,
    /// under the name too, or at the root without one).
    fn extract(cap: &Cap, raw: &str, now_ns: i64, out: &mut Vec<(String, String)>) {
        // A text the filter drops (`nullIf("-")`): nothing made of it.
        if let (Conv::Str, Some(Filter::NullIf(x))) = (&cap.conv, &cap.filter)
            && raw == x
        {
            return;
        }
        // Its strings taken where room was handed back (`spare`), and handed back when a filter makes
        // another of them.
        let value: Option<String> = match &cap.conv {
            Conv::Str => Some(spare::copy(raw)),
            Conv::Int => int_canonical(raw),
            Conv::Num => raw.parse::<f64>().ok().and_then(number_text),
            Conv::Bool(t) => Some(spare::copy(if t.iter().any(|x| x.eq_ignore_ascii_case(raw)) { "true" } else { "false" })),
            Conv::Date(d) => d.read(raw, now_ns).map(int_text),
            Conv::Quoted => {
                let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
                Some(if inner.contains('\\') { inner.replace("\\\"", "\"").replace("\\'", "'") } else { spare::copy(inner) })
            }
        };
        let Some(value) = value else { return };
        let key = |k: &str| match &cap.name {
            Some(n) => dotted(n, k),
            None => spare::copy(k),
        };
        let named = |v: String, out: &mut Vec<(String, String)>| match &cap.name {
            Some(n) => out.push((spare::copy(n), v)),
            None => spare::give(v),
        };
        match &cap.filter {
            None => return named(value, out),
            Some(Filter::Number) => {
                if let Some(v) = value.parse::<f64>().ok().and_then(number_text) {
                    named(v, out);
                }
            }
            Some(Filter::Integer) => {
                if let Some(v) = value.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| int_text(f as i64)) {
                    named(v, out);
                }
            }
            Some(Filter::Scale(k)) => {
                if let Some(v) = value.parse::<f64>().ok().and_then(|f| number_text(f * k)) {
                    named(v, out);
                }
            }
            Some(Filter::Boolean) => named(spare::copy(if value.eq_ignore_ascii_case("true") { "true" } else { "false" }), out),
            Some(Filter::NullIf(x)) => {
                if value != *x {
                    return named(value, out);
                }
            }
            Some(Filter::Lowercase) => named(value.to_lowercase(), out),
            Some(Filter::Uppercase) => named(value.to_uppercase(), out),
            Some(Filter::DecodeUri) => named(decode_uri(&value), out),
            Some(Filter::Json) => {
                if let Ok(v) = serde_json::from_str::<Value>(&value) {
                    flatten(cap.name.as_deref(), v, 0, out);
                }
            }
            Some(Filter::KeyValue { sep }) => key_values(&value, sep, cap.name.as_deref(), out),
            Some(Filter::QueryString) => {
                let mut pairs = Vec::new();
                query_pairs(value.trim_start_matches('?'), &mut pairs);
                for (k, v) in pairs {
                    out.push((key(&k), v));
                    spare::give(k);
                }
            }
            Some(Filter::Url) => {
                let mut parts = Vec::new();
                url_parts(&value, &mut parts);
                for (k, v) in parts {
                    out.push((key(&k), v));
                    if let Cow::Owned(k) = k {
                        spare::give(k);
                    }
                }
            }
            Some(Filter::UserAgent) => {
                let mut parts = Vec::new();
                user_agent(&value, &mut parts);
                for (k, v) in parts {
                    out.push((key(k), v));
                }
            }
        }
        spare::give(value);
    }

    /// `key<sep>value` pairs (a value quoted, or up to a space, a comma or a semicolon) into `out`, each
    /// key under `prefix` where there is one.
    fn key_values(s: &str, sep: &str, prefix: Option<&str>, out: &mut Vec<(String, String)>) {
        let mut rest = s;
        let mut n = 0;
        while let Some(at) = rest.find(sep) {
            let before = rest.get(..at).unwrap_or_default();
            let key = before.rsplit(|c: char| c.is_whitespace() || c == ',' || c == ';').next().unwrap_or_default();
            let after = rest.get(at + sep.len()..).unwrap_or_default();
            let (value, used) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => {
                    let body = after.get(1..).unwrap_or_default();
                    let end = body.find(q).unwrap_or(body.len());
                    (body.get(..end).unwrap_or_default(), (end + 2).min(after.len()))
                }
                _ => {
                    let end = after.find(|c: char| c.is_whitespace() || c == ',' || c == ';').unwrap_or(after.len());
                    (after.get(..end).unwrap_or_default(), end)
                }
            };
            if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || "_.@-".contains(c)) && n < ATTRS {
                let key = match prefix {
                    Some(p) => dotted(p, key),
                    None => spare::copy(key),
                };
                out.push((key, spare::copy(value)));
                n += 1;
            }
            rest = after.get(used..).unwrap_or_default();
            if used == 0 {
                break;
            }
        }
    }

    /// `s` with its `%XX` escapes and `+`s decoded.
    pub(crate) fn decode_uri(s: &str) -> String {
        if !s.bytes().any(|b| b == b'%' || b == b'+') {
            return spare::copy(s);
        }
        let b = s.as_bytes();
        let mut out = spare::take().into_bytes();
        out.reserve(b.len());
        let mut i = 0;
        while let Some(&c) = b.get(i) {
            match c {
                b'%' => match b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(c);
                        i += 1;
                    }
                },
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        match String::from_utf8(out) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    /// A query string's pairs, decoded, into `out`.
    fn query_pairs(q: &str, out: &mut Vec<(String, String)>) {
        for p in q.split('&').filter(|p| !p.is_empty()).take(ATTRS) {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            out.push((decode_uri(k), decode_uri(v)));
        }
    }

    /// A URL's parts, as Datadog's URL parser names them, into `out`: `scheme`, `host`, `port`, `path`,
    /// `queryString.<key>` (the names but a query's key, written in once).
    pub(crate) fn url_parts(url: &str, out: &mut Vec<(Cow<'static, str>, String)>) {
        let (scheme, rest) = match url.split_once("://") {
            Some((s, r)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) => (Some(s), r),
            _ => (None, url),
        };
        if let Some(s) = scheme {
            out.push((Cow::Borrowed("scheme"), spare::copy(s)));
        }
        let (authority, path_query) = if scheme.is_some() {
            match rest.find(['/', '?', '#']) {
                Some(i) => (rest.get(..i).unwrap_or_default(), rest.get(i..).unwrap_or_default()),
                None => (rest, ""),
            }
        } else {
            ("", rest)
        };
        if !authority.is_empty() {
            let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            match host_port.rsplit_once(':').filter(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
                Some((h, p)) => {
                    out.push((Cow::Borrowed("host"), spare::copy(h.trim_start_matches('[').trim_end_matches(']'))));
                    out.push((Cow::Borrowed("port"), spare::copy(p)));
                }
                None => out.push((Cow::Borrowed("host"), spare::copy(host_port))),
            }
        }
        let path_query = path_query.split('#').next().unwrap_or_default();
        let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
        if !path.is_empty() {
            out.push((Cow::Borrowed("path"), spare::copy(path)));
        }
        for p in query.split('&').filter(|p| !p.is_empty()).take(ATTRS) {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let k = decode_uri(k);
            out.push((Cow::Owned(dotted("queryString", &k)), decode_uri(v)));
            spare::give(k);
        }
    }

    /// `{prefix}.{key}`, made with its room once.
    pub(crate) fn dotted(prefix: &str, key: &str) -> String {
        let mut s = spare::take();
        s.reserve(prefix.len() + 1 + key.len());
        s.push_str(prefix);
        s.push('.');
        s.push_str(key);
        s
    }

    /// An integer's decimal text, without the formatter's machinery.
    pub(crate) fn int_text(n: i64) -> String {
        let mut buf = [0u8; 20];
        let mut at = buf.len();
        let mut m = n.unsigned_abs();
        loop {
            at -= 1;
            if let Some(d) = buf.get_mut(at) {
                *d = b'0' + (m % 10) as u8;
            }
            m /= 10;
            if m == 0 {
                break;
            }
        }
        let digits = buf.get(at..).unwrap_or_default();
        let mut s = spare::take();
        if n < 0 {
            s.push('-');
        }
        s.extend(digits.iter().map(|&d| char::from(d)));
        s
    }

    /// An integer's text as `i64`'s parse and its text say it (a `+` and leading zeros gone): the text
    /// as it is when that is already it.
    fn int_canonical(raw: &str) -> Option<String> {
        let digits = raw.strip_prefix('-').unwrap_or(raw);
        let plain = !digits.is_empty() && digits.len() <= 18 && digits.bytes().all(|b| b.is_ascii_digit()) && (digits.len() == 1 || !digits.starts_with('0')) && raw != "-0";
        if plain {
            return Some(spare::copy(raw));
        }
        raw.trim_start_matches('+').parse::<i64>().ok().map(int_text)
    }

    /// The user agent's parts as the code before wrote them: through today's (held to
    /// `user_agent_before` by its own test), as strings of their own.
    pub(crate) fn user_agent(ua: &str, out: &mut Vec<(&'static str, String)>) {
        let mut a = Attrs::default();
        super::user_agent(ua, None, &mut a);
        for (k, v) in a.iter() {
            let key: &'static str = ["browser.family", "browser.major", "os.family", "device.category"].into_iter().find(|w| *w == k).unwrap_or("");
            out.push((key, v.to_owned()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const NGINX_SUPPORT: &str = r#"
_auth %{notSpace:http.auth:nullIf("-")}
_bytes_written %{integer:network.bytes_written}
_client_ip %{ipOrHost:network.client.ip}
_version HTTP\/%{regex("\\d+\\.\\d+"):http.version}
_url %{notSpace:http.url}
_ident %{notSpace:http.ident:nullIf("-")}
_user_agent %{regex("[^\\\"]*"):http.useragent}
_referer %{notSpace:http.referer}
_status_code %{integer:http.status_code}
_method %{word:http.method}
_date_access %{date("dd/MMM/yyyy:HH:mm:ss Z"):date_access}
_x_forwarded_for %{regex("[^\\\"]*"):http._x_forwarded_for:nullIf("-")}
"#;

    const NGINX_MATCH: &str = r#"
access.common %{_client_ip} %{_ident} %{_auth} \[%{_date_access}\] "(?>%{_method} |)%{_url}(?> %{_version}|)" %{_status_code} (?>%{_bytes_written}|-)
access.combined %{access.common} (%{number:duration:scale(1000000000)} )?"%{_referer}" "%{_user_agent}"( "%{_x_forwarded_for}")?.*
"#;

    fn get<'a>(out: &'a [(String, String)], k: &str) -> Option<&'a str> {
        out.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    /// Datadog's own nginx rules read a combined access line as its integration does.
    #[test]
    fn datadog_s_nginx_rules_read_an_access_line() {
        let g = Grok::compile(NGINX_MATCH, NGINX_SUPPORT).unwrap();
        let line = r#"172.17.0.1 - - [06/Jan/2017:16:16:37 +0000] "GET /datadoghq/company?test=var1%20Pl HTTP/1.1" 200 612 "http://www.perdu.com/" "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/55.0.2883.87 Safari/537.36" "-""#;
        let mut out = Vec::new();
        assert_eq!(g.parse(line, 0, &mut out), Some("access.combined"));
        assert_eq!(get(&out, "network.client.ip"), Some("172.17.0.1"));
        assert_eq!(get(&out, "http.ident"), None, "nullIf(\"-\")");
        assert_eq!(get(&out, "date_access"), Some("1483719397000"));
        assert_eq!((get(&out, "http.method"), get(&out, "http.url"), get(&out, "http.version")), (Some("GET"), Some("/datadoghq/company?test=var1%20Pl"), Some("1.1")));
        assert_eq!((get(&out, "http.status_code"), get(&out, "network.bytes_written")), (Some("200"), Some("612")));
        assert_eq!(get(&out, "http.referer"), Some("http://www.perdu.com/"));
        assert!(get(&out, "http.useragent").is_some_and(|u| u.starts_with("Mozilla/5.0")));
        assert_eq!(get(&out, "http._x_forwarded_for"), None);
        // The common format (no referer, no agent) matches the first rule.
        let mut out = Vec::new();
        assert_eq!(g.parse(r#"10.0.0.2 - bob [06/Jan/2017:16:16:37 -0500] "POST /api HTTP/2.0" 404 -"#, 0, &mut out), Some("access.common"));
        assert_eq!((get(&out, "http.auth"), get(&out, "date_access"), get(&out, "network.bytes_written")), (Some("bob"), Some("1483737397000"), None));
        assert_eq!(g.parse("not an access line", 0, &mut Vec::new()), None);
    }

    /// kiln's access and error rules read by grok's own matcher and by `regex` alone: the same rule
    /// and the same attributes, on access lines of every part's shapes (addresses of both kinds and
    /// names, dates whole and not, requests with and without a method or version, Unicode in them),
    /// error lines, and those cut short or changed.
    #[test]
    fn the_matcher_reads_what_regex_reads() {
        const APACHE_ERROR: &str = r#"error.format \[%{date("EEE MMM dd HH:mm:ss.SSSSSS yyyy"):date_access}\] \[(%{word:apache.module}:)?%{word:level}\] \[pid %{integer:process.pid}(:tid %{integer:process.thread_id})?\]( \[client %{ipOrHost:network.client.ip}(:%{port:network.client.port})?\])? %{data:error.message}"#;
        const NGINX_ERROR: &str = r#"error.format %{date("yyyy/MM/dd HH:mm:ss"):date_access} \[%{word:level}\] %{data:error.message}(, %{data::keyvalue(": ",",")})?"#;
        let rules = format!("{NGINX_MATCH}\n{NGINX_ERROR}\n{}", APACHE_ERROR.replace("error.format", "apache.error"));
        let ours = Grok::compile(&rules, NGINX_SUPPORT).unwrap();
        let theirs = Grok::compile(&rules, NGINX_SUPPORT).unwrap().regex_only();
        assert!(ours.rules.iter().all(|r| r.prog.is_some()), "every rule its own program");
        let mut rng = kiln_obs::testkit::Rng::new(21);
        let mut pick = |xs: &[&str]| xs[rng.below(xs.len() as u64) as usize].to_owned();
        let mut lines = Vec::new();
        for _ in 0..3_000 {
            let client = pick(&["172.17.0.1", "10.0.0.255", "256.1.1.1", "2001:db8::1", "::1", "fe80::1:2:3:4", "web-1.example.com", "localhost", "a..b", "-"]);
            let date = pick(&["06/Jan/2017:16:16:37 +0000", "18/Oct/2026:12:00:00 -0700", "1/Feb/2026:1:2:3 Z", "32/Foo/2026:99:99:99 +05", "06/Jan/2017:16:16:37"]);
            let request =
                pick(&["GET /a?b=c HTTP/1.1", "POST /api/v1 HTTP/2.0", "-", "GET /é?q=日本 HTTP/1.1", "/no-method HTTP/1.0", "GET /no-version", "GET /a\"b HTTP/1.1", "OPTIONS * HTTP/1.1", ""]);
            let tail = pick(&[
                "",
                " \"-\" \"curl/8.5.0\"",
                " 0.004 \"http://x/\" \"Mozilla/5.0 (X11; Linux x86_64) Firefox/131.0\"",
                " \"-\" \"-\" \"203.0.113.9, 10.0.0.1\"",
                " \"-\" \"agent\" \"-\" rest of it",
                " \"ref\" \"ua with é\" extra",
            ]);
            let status = pick(&["200", "404", "-1", "+5", "x"]);
            let bytes = pick(&["612", "-", "0", "1e3"]);
            lines.push(format!("{client} - {} [{date}] \"{request}\" {status} {bytes}{tail}", pick(&["-", "bob", "é"])));
            lines.push(format!(
                "{} [{}] {}{}",
                pick(&["2026/10/18 12:00:00", "2026/13/18 12:00:00", "18/10/2026"]),
                pick(&["error", "warn", "crit", "x y"]),
                pick(&["open() \"/x\" failed (2: No such file or directory)", "upstream timed out", "", "a, b"]),
                pick(&[", client: 192.0.2.10, server: localhost, request: \"GET / HTTP/1.1\"", "", ", host: \"example.com\"", ", odd"])
            ));
            lines.push(format!(
                "[{}] [{}{}] [pid {}{}]{} {}",
                pick(&["Sun Oct 18 12:00:00.123456 2026", "Mon Jan 01 00:00:00.000000 2024", "Xyz Oct 18 12:00:00 2026"]),
                pick(&["", "core:", "ssl:"]),
                pick(&["error", "notice", "warn"]),
                pick(&["1234", "x"]),
                pick(&["", ":tid 5678"]),
                pick(&["", " [client 203.0.113.5:51234]", " [client ::1]", " [client host.example:8]"]),
                pick(&["AH00128: File does not exist: /var/www/html/favicon.ico", "", "é"])
            ));
        }
        // And each cut short at a random place.
        let cut: Vec<String> = lines
            .iter()
            .map(|l| {
                let mut i = rng.below(l.len() as u64 + 1) as usize;
                while !l.is_char_boundary(i) {
                    i -= 1;
                }
                l[..i].to_owned()
            })
            .collect();
        let (mut reading, mut matched) = (Reading::default(), 0);
        for line in lines.iter().chain(&cut) {
            let (mut a, mut b) = (Attrs::default(), Vec::new());
            let ra = ours.parse_with(line, 1_792_324_800_000_000_000, &mut reading, &mut a).map(str::to_owned);
            let rb = theirs.parse(line, 1_792_324_800_000_000_000, &mut b).map(str::to_owned);
            let a: Vec<(String, String)> = a.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
            assert_eq!((&ra, &a), (&rb, &b), "{line}");
            matched += usize::from(ra.is_some());
        }
        assert!(matched > 4_000, "{matched} of {} matched", lines.len() * 2);
    }

    /// The user agent's words read in one pass say what they said looked for one by one: on real
    /// agents, and on random ones of their words (in any case, cut, run together) and other text.
    #[test]
    fn a_user_agent_reads_as_it_did() {
        /// Whether `hay` holds `needle` (lowercase ASCII), whatever its case: as a lowered copy's
        /// `contains` would say, with no copy.
        fn holds_any_case(hay: &str, needle: &str) -> bool {
            let n = needle.as_bytes();
            n.is_empty() || hay.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
        }

        /// A user agent's browser, operating system and device, as far as its words tell (the families
        /// Datadog's parser names, not its full database).
        fn user_agent_before(ua: &str) -> Vec<(&'static str, String)> {
            let mut out = Vec::new();
            let has = |s: &str| ua.contains(s);
            // A version's major part: of its digits and dots, those before the first dot (none where
            // neither follows its key).
            let version = |key: &str| -> Option<&str> {
                let at = ua.find(key)? + key.len();
                let rest = ua.get(at..)?;
                let run = rest.bytes().take_while(|c| c.is_ascii_digit() || *c == b'.').count();
                let major = rest.bytes().take_while(u8::is_ascii_digit).count();
                if run == 0 { None } else { rest.get(..major) }
            };
            let bot = ["bot", "crawler", "spider", "slurp", "curl/", "wget/", "python-requests", "go-http-client", "okhttp"].iter().any(|b| holds_any_case(ua, b));
            let browser = if has("Edg/") {
                Some(("Edge", version("Edg/")))
            } else if has("OPR/") {
                Some(("Opera", version("OPR/")))
            } else if has("Firefox/") {
                Some(("Firefox", version("Firefox/")))
            } else if has("Chrome/") && !has("Chromium/") {
                Some(("Chrome", version("Chrome/")))
            } else if has("Safari/") && has("Version/") {
                Some(("Safari", version("Version/")))
            } else if ua.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("curl/")) {
                Some(("curl", version("curl/")))
            } else {
                None
            };
            if let Some((family, v)) = browser {
                out.push(("browser.family", family.to_owned()));
                if let Some(v) = v {
                    out.push(("browser.major", v.to_owned()));
                }
            }
            let os = if has("Windows") {
                Some("Windows")
            } else if has("iPhone") || has("iPad") || has("iOS") {
                Some("iOS")
            } else if has("Mac OS X") || has("Macintosh") {
                Some("Mac OS X")
            } else if has("Android") {
                Some("Android")
            } else if has("Linux") {
                Some("Linux")
            } else {
                None
            };
            if let Some(os) = os {
                out.push(("os.family", os.to_owned()));
            }
            let device = if bot {
                "Bot"
            } else if has("Mobile") || has("iPhone") || has("Android") {
                "Mobile"
            } else if has("iPad") || has("Tablet") {
                "Tablet"
            } else {
                "Desktop"
            };
            out.push(("device.category", device.to_owned()));
            out
        }

        let real = [
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/55.0.2883.87 Safari/537.36",
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "curl/8.5.0",
            "CURL/7.1",
            "python-requests/2.31.0",
            "Mozilla/5.0 (Linux; Android 14) Firefox/.5",
            "",
        ];
        let words: Vec<&str> = UA_WORDS.iter().chain(BOT_WORDS.iter()).copied().chain(["Mozilla/5.0 ", "(", "; ", ") ", "1", "2.0", ".", "x", "BOT", "Bot", "Curl/", "é", " "]).collect();
        let mut rng = kiln_obs::testkit::Rng::new(17);
        let mut uas: Vec<String> = real.iter().map(|s| (*s).to_owned()).collect();
        for _ in 0..20_000 {
            let mut ua = String::new();
            for _ in 0..rng.below(9) {
                let w = words[rng.below(words.len() as u64) as usize];
                match rng.below(5) {
                    0 => ua.push_str(&w.to_ascii_uppercase()),
                    1 => ua.push_str(&w[..w.floor_char_boundary(w.len() / 2)]),
                    _ => ua.push_str(w),
                }
                if rng.below(2) == 0 {
                    ua.push_str(["1", "17.0", "/", ".", "x"][rng.below(5) as usize]);
                }
            }
            uas.push(ua);
        }
        for ua in &uas {
            let mut ours = Attrs::default();
            user_agent(ua, None, &mut ours);
            let before = user_agent_before(ua);
            assert_eq!(ours.iter().collect::<Vec<_>>(), before.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(), "{ua:?}");
        }
    }

    /// A URL's scheme ends where `str::find` says `://` first is: on random text of colons, slashes
    /// and what is neither.
    #[test]
    fn a_url_s_scheme_ends_where_find_says() {
        let mut rng = kiln_obs::testkit::Rng::new(5);
        let parts = [":", "/", "//", "://", "a", "é", ":/", "x:", "::"];
        for _ in 0..20_000 {
            let s: String = (0..rng.below(8)).map(|_| parts[rng.below(parts.len() as u64) as usize]).collect();
            assert_eq!(scheme_end(&s), s.find("://"), "{s:?}");
        }
    }

    /// Each word the user agent's reading asks about by its place is the word at that place.
    #[test]
    fn a_user_agent_s_words_are_at_their_places() {
        let named = [
            (EDG, "Edg/"),
            (OPR, "OPR/"),
            (FIREFOX, "Firefox/"),
            (CHROME, "Chrome/"),
            (VERSION, "Version/"),
            (CURL, "curl/"),
            (CHROMIUM, "Chromium/"),
            (SAFARI, "Safari/"),
            (WINDOWS, "Windows"),
            (IPHONE, "iPhone"),
            (IPAD, "iPad"),
            (IOS, "iOS"),
            (MAC_OS_X, "Mac OS X"),
            (MACINTOSH, "Macintosh"),
            (ANDROID, "Android"),
            (LINUX, "Linux"),
            (MOBILE, "Mobile"),
            (TABLET, "Tablet"),
        ];
        for (k, w) in named {
            assert_eq!(UA_WORDS.get(k), Some(&w), "{w}");
        }
        assert_eq!(ua_word("Netscape"), UA_WORDS.len());
        assert!(begins_with(b"Firefox/131", b"Firefox/") && !begins_with(b"Fire", b"Firefox/") && begins_with(b"x", b"") && !begins_with(b"Firefax/", b"Firefox/"));
    }

    #[test]
    fn integers_read_as_parse_and_text_say() {
        for raw in [
            "0",
            "7",
            "-7",
            "+7",
            "007",
            "-0",
            "+0",
            "-007",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "123456789012345678",
            "1234567890123456789",
            "",
            "-",
            "+",
            "1a",
            "٣",
        ] {
            let mut s = String::new();
            let read = int_canonical_into(raw, &mut s);
            assert_eq!(read.then_some(s), raw.trim_start_matches('+').parse::<i64>().ok().map(|i| i.to_string()), "{raw}");
        }
        for n in [0, 1, -1, 9, 10, -10, i64::MAX, i64::MIN, 1_483_719_397_000] {
            let mut s = String::new();
            int_into(n, &mut s);
            assert_eq!(s, n.to_string());
        }
    }

    /// An atomic group read as a plain one only where it opens one: one escaped, or in a class, is
    /// left as it is.
    #[test]
    fn only_atomic_groups_are_read_as_plain_ones() {
        for (java, rust) in [
            ("(?>a|b)c", "(?:a|b)c"),
            (r"\(?>", r"\(?>"),
            ("[(?>]", "[(?>]"),
            ("[]](?>x)", "[]](?:x)"),
            ("[^]](?>x)", "[^]](?:x)"),
            (r"[\]](?>x)", r"[\]](?:x)"),
            ("[a[b]](?>x)", "[a[b]](?:x)"),
            (r"\\(?>x)", r"\\(?:x)"),
            ("abc", "abc"),
        ] {
            assert_eq!(atomic(java), rust, "{java}");
        }
    }

    #[test]
    fn matchers_and_filters_read_what_they_say() {
        let g = Grok::compile(
            r#"r %{word:user} id=%{uuid:id} %{boolean:ok} %{numberExt:n} "%{data:rest:keyvalue}" %{quotedString:q} %{data:j:json} %{notSpace:u:url} %{notSpace:qs:querystring} %{integer:ms:scale(0.001)} %{word:lc:lowercase}"#,
            "",
        )
        .unwrap();
        let line = r#"ada id=0af76519-16cd-43dd-8448-eb211c80319c TRUE 1.5e3 "a=1 b='two words' c=3" "quoted \"x\"" {"k":{"v":2}} https://h.example:8443/p/q?x=1&y=a%20b ?s=1&t=2 2500 LOUD"#;
        let mut out = Vec::new();
        assert_eq!(g.parse(line, 0, &mut out), Some("r"), "{out:?}");
        for (k, v) in [
            ("user", "ada"),
            ("id", "0af76519-16cd-43dd-8448-eb211c80319c"),
            ("ok", "true"),
            ("n", "1500"),
            ("rest.a", "1"),
            ("rest.b", "two words"),
            ("rest.c", "3"),
            ("q", "quoted \"x\""),
            ("j.k.v", "2"),
            ("u.scheme", "https"),
            ("u.host", "h.example"),
            ("u.port", "8443"),
            ("u.path", "/p/q"),
            ("u.queryString.y", "a b"),
            ("qs.s", "1"),
            ("qs.t", "2"),
            ("ms", "2.5"),
            ("lc", "loud"),
        ] {
            assert_eq!(get(&out, k), Some(v), "{k}: {out:?}");
        }
        // A token that extracts nothing still matches; a rule naming itself is refused.
        let g = Grok::compile("r %{word} %{word:w}", "").unwrap();
        let mut out = Vec::new();
        assert!(g.parse("a b", 0, &mut out).is_some() && out == vec![("w".to_owned(), "b".to_owned())]);
        assert!(Grok::compile("r %{loop}", "loop %{loop}").is_err());
        assert!(Grok::compile("r %{nonesuch:x}", "").is_err());
        assert!(Grok::compile("r %{word:x:nofilter}", "").is_err());
        assert!(Grok::compile("r %{word:x", "").is_err());
    }

    #[test]
    fn a_date_read_by_its_pieces_is_the_one_its_expression_reads() {
        // Datadog's parsers' patterns, and texts made of each one's pieces: what a part may hold,
        // near misses (a digit too many or too few, an offset half written, a zone's slash
        // dangling), and literal text now and then wrong.
        let patterns = [
            "dd/MMM/yyyy:HH:mm:ss Z",
            "yyyy/MM/dd HH:mm:ss",
            "EEE MMM dd HH:mm:ss.SSSSSS yyyy",
            "yyyy-MM-dd'T'HH:mm:ss.SSSXXX",
            "yyyy-MM-dd HH:mm:ss,SSS",
            "MMM d HH:mm:ss",
            "dd/MM/yy h:mm a",
            "yyyy-MM-dd'T'HH:mm:ssZZ",
            "yyyy-MM-dd HH:mm:ss z",
            "yyyy-MM-dd HH:mm:ss VV",
            "HHmmss",
            "yyyyMMdd'T'HHmmssX",
            "MMMM d, yyyy h:mm:ss a xx",
        ];
        let mut rng = kiln_obs::testkit::Rng::new(9);
        let mut pick = |n: usize| rng.below(n as u64) as usize;
        const NOW: i64 = 1_792_324_800_000_000_000;
        let (mut read, mut scanned) = (0, 0);
        for p in patterns {
            let d = DateFmt::compile(p, Zone::UTC).unwrap();
            for _ in 0..3000 {
                let mut s = String::new();
                for piece in &d.pieces {
                    match piece {
                        Piece::Text(t) if pick(20) == 0 => s.push_str(&t[..t.len() / 2]),
                        Piece::Text(t) => s.push_str(t),
                        Piece::Part(shape) => {
                            // Seven times in eight a text the part may hold, else a near miss.
                            let fits = pick(8) != 0;
                            let from: &[&str] = match (shape, fits) {
                                (Shape::Digits(4, 4), true) => &["2026", "1999", "2261", "0001"],
                                (Shape::Digits(1, 9), true) => &["5", "123", "123456", "123456789"],
                                (Shape::Digits(..), true) => &["1", "07", "12", "09"],
                                (Shape::Digits(..), false) => &["", "0", "123", "20261", "1234567891"],
                                (Shape::Letters(1, 6), true) => &["UTC", "PST", "GMT", "Z"],
                                (Shape::Letters(3, 3), true) => &["Oct", "mon", "JAN", "Tue"],
                                (Shape::Letters(..), true) => &["October", "Tuesday", "Oct", "Sept"],
                                (Shape::Letters(..), false) => &["", "x", "Mo", "Septembers"],
                                (Shape::AmPm, true) => &["AM", "pm", "Pm"],
                                (Shape::AmPm, false) => &["A", "xm", ""],
                                (Shape::Offset, true) => &["Z", "+0530", "-05", "+0000"],
                                (Shape::OffsetColon, true) => &["Z", "+05:30", "-08:00"],
                                (Shape::OffsetEither, true) => &["Z", "+0530", "-05:30", "+05"],
                                (Shape::Offset | Shape::OffsetColon | Shape::OffsetEither, false) => &["-1", "+05:3", "+05:", "Zulu", "+053", ""],
                                (Shape::ZoneName, true) => &["UTC", "Europe/Paris", "America/Argentina/Buenos_Aires", "Etc/GMT+5"],
                                (Shape::ZoneName, false) => &["Z", "_", "a/", "/b", "Europe/", "Zulu", ""],
                            };
                            s.push_str(from[pick(from.len())]);
                        }
                    }
                }
                let (mut a, mut b) = ([""; DATE_PARTS], [""; DATE_PARTS]);
                let by_pieces = d.scan(&s, &mut a);
                let by_expression = d.read_expression(&s, &mut b).is_some();
                if by_pieces {
                    scanned += 1;
                    assert!(by_expression && a == b, "{p} on {s:?}: {a:?} by its pieces, {b:?} by its expression");
                }
                let via_expression = if by_expression { d.reckon(&b, NOW) } else { None };
                assert_eq!(d.read(&s, NOW), via_expression, "{p} on {s:?}");
                read += usize::from(via_expression.is_some());
            }
        }
        // Enough of them dates, and read by their pieces, for the comparison to say something.
        assert!(read > 5_000 && scanned > 8_000, "{read} read, {scanned} by their pieces");
    }

    #[test]
    fn dates_read_as_java_patterns_say() {
        let now = 1_792_324_800_000_000_000; // 2026-10-18T12:00:00Z
        for (pattern, text, ms) in [
            ("yyyy-MM-dd'T'HH:mm:ss.SSSZ", "2026-10-18T12:00:00.250+0200", 1_792_317_600_250),
            ("yyyy-MM-dd HH:mm:ss,SSS", "2026-10-18 12:00:00,5", 1_792_324_800_500),
            ("dd/MMM/yyyy:HH:mm:ss Z", "18/Oct/2026:12:00:00 -0700", 1_792_350_000_000),
            ("MMM dd HH:mm:ss", "Oct 18 11:00:00", 1_792_321_200_000),
            ("EEE, dd MMM yyyy hh:mm:ss a z", "Sun, 18 Oct 2026 01:30:00 PM GMT", 1_792_330_200_000),
            ("yyyy-MM-dd'T'HH:mm:ssXXX", "2026-10-18T12:00:00Z", 1_792_324_800_000),
            ("yy/M/d H:m:s", "26/10/18 9:5:3", 1_792_314_303_000),
        ] {
            let d = DateFmt::compile(pattern, Zone::UTC).unwrap();
            assert_eq!(d.read(text, now), Some(ms), "{pattern} {text}");
        }
        // A zone given to the matcher, for times without one.
        let la = DateFmt::compile("yyyy-MM-dd HH:mm:ss", Zone::named("America/Los_Angeles").unwrap()).unwrap();
        assert_eq!(la.read("2026-10-18 05:00:00", now), Some(1_792_324_800_000));
        assert!(DateFmt::compile("yyyy-MM-dd qq", Zone::UTC).is_err());
        // A year past what nanoseconds since the epoch hold (from 2262): no date, not a wrong one.
        let far = DateFmt::compile("yyyyMMdd HHmmss Z", Zone::UTC).unwrap();
        assert_eq!((far.read("22620412 000000 +0000", 0), far.read("71250101 000000 +0000", 0)), (None, None));
        assert!(far.read("22610101 000000 +0000", 0).is_some());
    }

    /// What a rule's tokens make, written side by side, is what they made as strings of their own
    /// (`before`, the code it replaced), to the byte and in the same order: every conversion and
    /// every filter, named and at the root, on values made to trip them (escapes, broken UTF-8 in
    /// a URL's escapes, JSON past its depth, keys repeated, quotes, URLs of every part).
    #[test]
    fn values_extracted_side_by_side_are_what_they_were() {
        let rules = [
            "r %{data:a}",
            "r %{notSpace:a.b} %{data:c}",
            "r %{number:a}",
            "r %{numberExt:a}",
            "r %{integer:a}",
            "r %{quotedString:a}",
            "r %{boolean:a}",
            r#"r %{boolean("yes","no"):a}"#,
            r#"r %{date("yyyy-MM-dd HH:mm:ss"):a}"#,
            r#"r %{date("dd/MMM/yyyy:HH:mm:ss Z"):a}"#,
            "r %{data:a:number}",
            "r %{data:a:integer}",
            "r %{data:a:scale(10)}",
            "r %{data:a:scale(0.001)}",
            "r %{data:a:boolean}",
            r#"r %{data:a:nullIf("-")}"#,
            r#"r %{notSpace:a:nullIf("-")}"#,
            "r %{data:a:lowercase}",
            "r %{data:a:uppercase}",
            "r %{data:a:decodeuricomponent}",
            "r %{data:a:json}",
            "r %{data::json}",
            "r %{data:a:keyvalue}",
            "r %{data::keyvalue}",
            r#"r %{data:a:keyvalue(":")}"#,
            "r %{data:a:querystring}",
            "r %{data::querystring}",
            "r %{data:a:url}",
            "r %{data::url}",
            "r %{data:a:useragent}",
            "r %{data::useragent}",
            "r %{quotedString:a:keyvalue}",
            "r %{integer:a:scale(2)} %{data:b:json}",
        ];
        let fixed = [
            "",
            "-",
            "0",
            "-0",
            "+7",
            "007",
            "12.50",
            "1e3",
            "9223372036854775808",
            "true",
            "FALSE",
            "yes",
            "\"a \\\"quoted\\\" \\'one\\'\"",
            "'single'",
            "2026-10-18 12:00:00",
            "18/Oct/2026:12:00:00 +0200",
            "Hello%20W%C3%B6rld+x%FF%C3%28%",
            r#"{"a":{"b":{"c":{"d":{"e":{"f":{"g":1}}}}}},"n":null,"x":[1,"two",{"t":3}],"s":"str","f":1.5,"t":true}"#,
            r#"{"k":"v","k":"w"}"#,
            "[1,2]",
            "k=v k2=\"quoted value\" k3='q' bad key=x k=again a.b=c;d=e,f=g",
            "k:v other:w",
            "?a=1&b=%20two&a=3&&c&d=%FF",
            "https://user:pw@[::1]:8443/a/b//?q=1&r=%2F#frag",
            "http://example.com",
            "/just/a/path/?x=y",
            "ftp+x://host:21",
            "Mozilla/5.0 (Windows NT 10.0) AppleWebKit/537.36 Chrome/131.0 Safari/537.36",
            "curl/8.5.0",
            "Googlebot/2.1 (+http://www.google.com/bot.html)",
            "ΣΑΣ ÉÇ ß",
        ];
        let alphabet = ["a", "Z", "0", "9", " ", "=", "&", "?", "/", ":", "%", "%2", "%41", "%FF", "+", "\"", "'", "\\", "{", "}", "[", "]", ",", ";", ".", "-", "é", "Σ", "http://", "k=", "\"x\":"];
        let mut rng = kiln_obs::testkit::Rng::new(41);
        let mut values: Vec<String> = fixed.iter().map(|v| (*v).to_owned()).collect();
        for _ in 0..3_000 {
            let mut v = String::new();
            for _ in 0..rng.below(24) {
                v.push_str(alphabet[rng.below(alphabet.len() as u64) as usize]);
            }
            values.push(v);
        }
        let now_ns = 1_792_324_800_000_000_000;
        let mut reading = Reading::default();
        let (mut ours, mut theirs) = (Attrs::default(), Vec::new());
        let mut made = 0;
        for rule in rules {
            let g = Grok::compile(rule, "").unwrap();
            for v in &values {
                for line in [v.clone(), format!("{v} {v}")] {
                    ours.clear();
                    theirs.clear();
                    let a = g.parse_with(&line, now_ns, &mut reading, &mut ours).map(str::to_owned);
                    let b = before::parse(&g, &line, now_ns, &mut Reading::default(), &mut theirs).map(str::to_owned);
                    let ours: Vec<(String, String)> = ours.iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
                    assert_eq!((&a, &ours), (&b, &theirs), "{rule} on {line:?}");
                    made += ours.len();
                }
            }
        }
        assert!(made > 20_000, "{made} attributes made");
    }
}
