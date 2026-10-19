//! Grok as Datadog's grok parser reads it (`docs/observe.md` §8.2): a rule is literal text (a
//! regular expression) and `%{matcher(args):attribute:filter(args)}` tokens, and must match the
//! value whole; the match rules are tried in order, the first that matches extracting its
//! attributes. A token's matcher is one of Datadog's (`notSpace`, `word`, `integer`, `number`,
//! `ipOrHost`, `date("dd/MMM/yyyy:HH:mm:ss Z")`, `regex("…")`, …) or a helper rule (a support
//! rule, or a match rule given before), inlined.
//!
//! Rust's regular expressions do not backtrack: an atomic group `(?>…)` is read as `(?:…)`, which
//! matches the same where an atomic group would not have to give anything back.

use std::fmt::Write as _;

use regex::Regex;
use serde_json::Value;

use kiln_obs::query::calendar::Zone;

/// How deep helper rules may nest (a rule that names itself stops here).
const DEPTH: usize = 16;
/// Attributes a match extracts at the most.
const ATTRS: usize = 256;

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
    name: String,
    re: Regex,
    caps: Vec<Cap>,
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
            [r] => (atomic(r), Conv::Str),
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

/// A regular expression of Java's, as Rust's reads it: atomic groups as plain ones.
fn atomic(r: &str) -> String {
    r.replace("(?>", "(?:")
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
            let (mut out, mut caps) = (String::from("^"), Vec::new());
            Compiler { helpers, out: &mut out, caps: &mut caps, next: 0 }.pattern(pattern, 0).map_err(|e| GrokError::In(name.clone(), e))?;
            out.push('$');
            let re = regex::RegexBuilder::new(&out).size_limit(4 << 20).build().map_err(|e| GrokError::In(name.clone(), format!("{e}")))?;
            for cap in &mut caps {
                cap.index = re.capture_names().position(|n| n == Some(cap.group.as_str())).unwrap_or(0);
            }
            rules.push(Rule { name: name.clone(), re, caps });
        }
        Ok(Self { rules })
    }

    /// The first rule that matches `value` whole: its name, and its attributes appended to `out`.
    pub fn parse(&self, value: &str, now_ns: i64, out: &mut Vec<(String, String)>) -> Option<&str> {
        // Each rule told by the fast matcher (a lazy DFA) before its groups are read by the slower
        // one; the last asked for its groups at once, which tells a value it does not match as fast
        // (measured: a set of the rules, one pass for all, was no faster than this).
        let last = self.rules.len().saturating_sub(1);
        for (i, rule) in self.rules.iter().enumerate() {
            if i < last && !rule.re.is_match(value) {
                continue;
            }
            let Some(c) = rule.re.captures(value) else { continue };
            for cap in &rule.caps {
                let Some(m) = c.get(cap.index) else { continue };
                if out.len() >= ATTRS {
                    break;
                }
                extract(cap, m.as_str(), now_ns, out);
            }
            return Some(&rule.name);
        }
        None
    }
}

fn number_text(f: f64) -> Option<String> {
    f.is_finite().then(|| if f.fract() == 0.0 && f.abs() < 9.0e15 { format!("{}", f as i64) } else { format!("{f}") })
}

/// A token's value, read and filtered, into `out` (under its name; a filter that makes fields,
/// under the name too, or at the root without one).
fn extract(cap: &Cap, raw: &str, now_ns: i64, out: &mut Vec<(String, String)>) {
    let value: Option<String> = match &cap.conv {
        Conv::Str => Some(raw.to_owned()),
        Conv::Int => raw.trim_start_matches('+').parse::<i64>().ok().map(|i| i.to_string()),
        Conv::Num => raw.parse::<f64>().ok().and_then(number_text),
        Conv::Bool(t) => Some(if t.iter().any(|x| x.eq_ignore_ascii_case(raw)) { "true" } else { "false" }.to_owned()),
        Conv::Date(d) => d.read(raw, now_ns).map(|ms| ms.to_string()),
        Conv::Quoted => Some(raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default().replace("\\\"", "\"").replace("\\'", "'")),
    };
    let Some(value) = value else { return };
    let key = |k: &str| match &cap.name {
        Some(n) => format!("{n}.{k}"),
        None => k.to_owned(),
    };
    let named = |v: String, out: &mut Vec<(String, String)>| {
        if let Some(n) = &cap.name {
            out.push((n.clone(), v));
        }
    };
    match &cap.filter {
        None => named(value, out),
        Some(Filter::Number) => {
            if let Some(v) = value.parse::<f64>().ok().and_then(number_text) {
                named(v, out);
            }
        }
        Some(Filter::Integer) => {
            if let Some(v) = value.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| (f as i64).to_string()) {
                named(v, out);
            }
        }
        Some(Filter::Scale(k)) => {
            if let Some(v) = value.parse::<f64>().ok().and_then(|f| number_text(f * k)) {
                named(v, out);
            }
        }
        Some(Filter::Boolean) => named(if value.eq_ignore_ascii_case("true") { "true" } else { "false" }.to_owned(), out),
        Some(Filter::NullIf(x)) => {
            if value != *x {
                named(value, out);
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
        Some(Filter::KeyValue { sep }) => {
            for (k, v) in key_values(&value, sep) {
                out.push((key(&k), v));
            }
        }
        Some(Filter::QueryString) => {
            for (k, v) in query_pairs(value.trim_start_matches('?')) {
                out.push((key(&k), v));
            }
        }
        Some(Filter::Url) => {
            for (k, v) in url_parts(&value) {
                out.push((key(&k), v));
            }
        }
        Some(Filter::UserAgent) => {
            for (k, v) in user_agent(&value) {
                out.push((key(k), v));
            }
        }
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

/// `key<sep>value` pairs (a value quoted, or up to a space, a comma or a semicolon).
fn key_values(s: &str, sep: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
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
        if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || "_.@-".contains(c)) && out.len() < ATTRS {
            out.push((key.to_owned(), value.to_owned()));
        }
        rest = after.get(used..).unwrap_or_default();
        if used == 0 {
            break;
        }
    }
    out
}

pub(crate) fn decode_uri(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
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
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn query_pairs(q: &str) -> Vec<(String, String)> {
    q.split('&').filter(|p| !p.is_empty()).take(ATTRS).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode_uri(k), decode_uri(v))).collect()
}

/// A URL's parts, as Datadog's URL parser names them: `scheme`, `host`, `port`, `path`,
/// `queryString.<key>`.
pub(crate) fn url_parts(url: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) => (Some(s), r),
        _ => (None, url),
    };
    if let Some(s) = scheme {
        out.push(("scheme".into(), s.to_owned()));
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
                out.push(("host".into(), h.trim_start_matches('[').trim_end_matches(']').to_owned()));
                out.push(("port".into(), p.to_owned()));
            }
            None => out.push(("host".into(), host_port.to_owned())),
        }
    }
    let path_query = path_query.split('#').next().unwrap_or_default();
    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
    if !path.is_empty() {
        out.push(("path".into(), path.to_owned()));
    }
    for (k, v) in query_pairs(query) {
        out.push((format!("queryString.{k}"), v));
    }
    out
}

/// A user agent's browser, operating system and device, as far as its words tell (the families
/// Datadog's parser names, not its full database).
pub(crate) fn user_agent(ua: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let has = |s: &str| ua.contains(s);
    let version = |key: &str| -> Option<String> {
        let at = ua.find(key)? + key.len();
        let v: String = ua.get(at..)?.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        (!v.is_empty()).then_some(v)
    };
    let lower = ua.to_ascii_lowercase();
    let bot = ["bot", "crawler", "spider", "slurp", "curl/", "wget/", "python-requests", "go-http-client", "okhttp"].iter().any(|b| lower.contains(b));
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
    } else if lower.starts_with("curl/") {
        Some(("curl", version("curl/")))
    } else {
        None
    };
    if let Some((family, v)) = browser {
        out.push(("browser.family", family.to_owned()));
        if let Some(v) = v {
            out.push(("browser.major", v.split('.').next().unwrap_or_default().to_owned()));
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

/// A date pattern of Java's (`dd/MMM/yyyy:HH:mm:ss Z`): a regular expression of its parts, in
/// order, and how each is read.
#[derive(Debug, Clone)]
pub(crate) struct DateFmt {
    pub regex: String,
    parts: Vec<Part>,
    re: Regex,
    zone: Zone,
}

impl DateFmt {
    pub(crate) fn compile(pattern: &str, zone: Zone) -> Result<Self, String> {
        // Its own expression (a group a part, to read them) and the one a rule holds (no groups).
        let (mut own, mut regex) = (String::new(), String::new());
        let mut parts = Vec::new();
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
                i = j + 1;
                continue;
            }
            if !c.is_ascii_alphabetic() {
                own.push_str(&regex::escape(&c.to_string()));
                regex.push_str(&regex::escape(&c.to_string()));
                i += 1;
                continue;
            }
            let mut n = 1;
            while chars.get(i + n) == Some(&c) {
                n += 1;
            }
            let (re, part) = match (c, n) {
                ('y' | 'u', 2) => (r"\d{2}", Part::Year2),
                ('y' | 'u', _) => (r"\d{4}", Part::Year),
                ('M', 1 | 2) => (r"\d{1,2}", Part::Month),
                ('M', 3) => (r"[A-Za-z]{3}", Part::MonthName),
                ('M', _) => (r"[A-Za-z]{3,9}", Part::MonthName),
                ('d', _) => (r"\d{1,2}", Part::Day),
                ('H' | 'k', _) => (r"\d{1,2}", Part::Hour),
                ('h' | 'K', _) => (r"\d{1,2}", Part::Hour12),
                ('m', _) => (r"\d{1,2}", Part::Minute),
                ('s', _) => (r"\d{1,2}", Part::Second),
                ('S', _) => (r"\d{1,9}", Part::Fraction),
                ('a', _) => (r"[AaPp][Mm]", Part::AmPm),
                ('E', 1..=3) => (r"[A-Za-z]{3}", Part::Skip),
                ('E', _) => (r"[A-Za-z]{3,9}", Part::Skip),
                ('Z', 1) | ('x', 1 | 2) => (r"Z|[+-]\d{4}|[+-]\d{2}", Part::Offset),
                ('Z', 2) | ('X' | 'x', 3) => (r"Z|[+-]\d{2}:\d{2}", Part::Offset),
                ('X', _) => (r"Z|[+-]\d{2}:?\d{2}|[+-]\d{2}", Part::Offset),
                ('Z', _) | ('V', _) => (r"[A-Za-z_]+(?:/[A-Za-z_+-]+)*", Part::ZoneId),
                ('z', _) => (r"[A-Za-z]{1,6}", Part::ZoneId),
                _ => return Err(format!("a date pattern's letter kiln does not read: {}", c.to_string().repeat(n))),
            };
            let _ = write!(own, "({re})");
            let _ = write!(regex, "(?:{re})");
            parts.push(part);
            i += n;
        }
        let re = Regex::new(&format!("^{own}$")).map_err(|e| e.to_string())?;
        Ok(Self { regex, parts, re, zone })
    }

    /// A date read: ms since the epoch. A year left out is the one it is (or the last, should that
    /// put it more than a day ahead); a zone left out is the pattern's timezone.
    pub(crate) fn read(&self, s: &str, now_ns: i64) -> Option<i64> {
        let c = self.re.captures(s)?;
        let (mut y, mut mo, mut d, mut h, mut mi, mut sec, mut nanos) = (None, 1i8, 1i8, 0i8, 0i8, 0i8, 0i32);
        let (mut pm, mut twelve) = (None, false);
        let mut offset: Option<i64> = None;
        let mut zone = self.zone;
        for (k, part) in self.parts.iter().enumerate() {
            let v = c.get(k + 1)?.as_str();
            match part {
                Part::Year => y = Some(v.parse::<i16>().ok()?),
                Part::Year2 => y = Some(2000 + v.parse::<i16>().ok()?),
                Part::Month => mo = v.parse().ok()?,
                Part::MonthName => {
                    let l = v.to_ascii_lowercase();
                    mo = i8::try_from(MONTH_NAMES.iter().position(|m| m.starts_with(&l) && l.len() >= 3)? + 1).ok()?;
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
                        let digits: String = v.chars().filter(char::is_ascii_digit).collect();
                        let (hh, mm) = (digits.get(..2)?.parse::<i64>().ok()?, digits.get(2..).filter(|m| !m.is_empty()).map_or(Some(0), |m| m.parse::<i64>().ok())?);
                        sign * (hh * 3600 + mm * 60)
                    });
                }
                Part::ZoneId => {
                    if ["utc", "gmt", "z"].contains(&v.to_ascii_lowercase().as_str()) {
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
            Some((days * 86_400 + i64::from(h) * 3_600 + i64::from(mi) * 60 + i64::from(sec)) * 1_000_000_000 + i64::from(nanos))
        };
        let at = |year: i16| -> Option<i64> {
            match offset {
                Some(off) => utc(year).map(|ns| ns - off * 1_000_000_000),
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
    }
}
