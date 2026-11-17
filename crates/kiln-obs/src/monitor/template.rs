//! A monitor's message (`docs/observe.md` §9): Datadog's template syntax, as its "Variables" page
//! states it (docs.datadoghq.com/monitors/notify/variables.md) and its "Template Variable
//! Evaluation" guide's `{{eval}}` (docs.datadoghq.com/monitors/guide/template-variable-evaluation.md).
//!
//! - Variables: `{{value}}`, `{{threshold}}`, `{{warn_threshold}}`, `{{alert_recovery_threshold}}`,
//!   `{{warn_recovery_threshold}}`, `{{ok_threshold}}`, `{{comparator}}`, `{{first_triggered_at}}`,
//!   `{{last_triggered_at}}` (and their `_epoch`s), `{{triggered_duration_sec}}`, `{{monitor.name}}`,
//!   `{{group}}`, `{{link}}`, a group's tag (`{{host.name}}`, `{{[dot.key].name}}`, a facet's
//!   `{{@machine_id.name}}`), and what [`Vars::extra`] names.
//! - Sections, nested: `{{#is_alert}}…{{else}}…{{/is_alert}}` (`^` for not) of the transition's kind,
//!   `is_renotify`, `is_unknown`, `is_priority 'P1'`, and `is_match` / `is_exact_match` of a variable
//!   (or of an `(eval "…")`) against strings.
//! - Helpers: `{{eval "…"}}`, `{{local_time 'last_triggered_at' 'Asia/Tokyo'}}`,
//!   `{{override_priority 'P1'}}` (the notification's priority), `{{urlencode "service.name"}}`.
//! - What a variable or a helper says is HTML-encoded (`&`, `<`, `>`, `"`, `'`), and `{{{var}}}` says
//!   it raw (Datadog's "Character escape": `&` written `&amp;`); a link kiln makes (`{{link}}`,
//!   `{{log.link}}`, ...) is a URL of its own, said as it is.
//! - A group's tag of several values says them all, sorted, by commas (Datadog's: "a
//!   comma-separated string of all values, in lexicographic order").
//! - Comments (`{{!-- … --}}`, `{{! … }}`), and raw blocks (`{{{{raw}}}}…{{{{/raw}}}}`, or a
//!   section's, `{{{{is_match "host.name" "db"}}}}`), their bodies as written.
//!
//! A variable of a record the message reads ([`Vars::blank`]: `{{log.attributes.<key>}}`, ...) that
//! the record lacks says nothing (Datadog's: "the variable renders empty"); anything else it does not
//! know it leaves as it was written. Nothing here recurses: sections are a stack, and `{{eval}}` is
//! read by shunting-yard, so a message's depth costs no stack.

use std::fmt::Write as _;

use super::{Op, Transition};
use crate::query::calendar::Zone;

/// What a message's variables stand for.
#[derive(Debug, Clone, Default)]
pub struct Vars<'a> {
    pub monitor: &'a str,
    pub value: Option<f64>,
    pub threshold: f64,
    pub warn_threshold: Option<f64>,
    /// Its recovery thresholds, as its options say them (none: it recovers past the threshold
    /// itself, its `{{alert_recovery_threshold}}` then).
    pub critical_recovery: Option<f64>,
    pub warning_recovery: Option<f64>,
    /// A service check monitor's OK runs in a row.
    pub ok_threshold: Option<f64>,
    /// How its value is compared with its thresholds.
    pub comparator: Option<Op>,
    pub group: &'a [(String, String)],
    pub link: &'a str,
    /// Variables besides, by their whole name: an Error Tracking monitor's issue's
    /// (`issue.attributes.error.type`, ...), a service check's, a composite's monitors' (`a.value`),
    /// the record a log, span or event monitor's group matched (`log.message`).
    pub extra: &'a [(String, String)],
    /// Unix ms: when the group last went from OK into trouble, and last went into a state of
    /// trouble (Datadog's "first" and "last" triggered); and now.
    pub first_triggered: Option<i64>,
    pub last_triggered: Option<i64>,
    pub now: i64,
    /// Its monitor's priority, 1 (P1) to 5.
    pub priority: Option<u8>,
    /// Whether the notification is a renotification.
    pub renotify: bool,
    /// Whether the group is in NO DATA by a service check's UNKNOWN runs (`checks::state`).
    pub unknown: bool,
    /// The prefixes of the records the message reads (`log`, `span`, `event`, a composite's
    /// monitor's `a.log`): a variable of one that [`Vars::extra`] does not have says nothing.
    pub blank: &'a [String],
}

/// A message rendered: its text, and the priority it asks its notification be sent at (Datadog's
/// `{{override_priority 'P1'}}`), 1 to 5.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub priority: Option<u8>,
}

/// A message's text ([`render_message`]).
pub fn render(template: &str, t: Transition, v: &Vars<'_>) -> String {
    render_message(template, t, v).text
}

/// A message rendered from Datadog's template syntax (the module's head).
pub fn render_message(template: &str, t: Transition, v: &Vars<'_>) -> Rendered {
    let mut out = Rendered { text: String::with_capacity(template.len()), priority: None };
    // Whether each open section shows its body.
    let mut shown: Vec<bool> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let visible = shown.iter().all(|&s| s);
        if visible {
            out.text.push_str(rest.get(..open).unwrap_or(""));
        }
        let at = rest.get(open..).unwrap_or("");
        let Some((piece, after)) = piece_of(at) else {
            if visible {
                out.text.push_str(at);
            }
            return out;
        };
        rest = after;
        let written = at.get(..at.len().saturating_sub(after.len())).unwrap_or("");
        match piece {
            Piece::Comment => {}
            Piece::Raw { tag, body } => {
                if visible && (tag == "raw" || condition(tag, t, v).unwrap_or(false)) {
                    out.text.push_str(body);
                }
            }
            Piece::Tag { tag, raw } => {
                if let Some(section) = tag.strip_prefix('#').or_else(|| tag.strip_prefix('^')) {
                    let on = condition(section, t, v).unwrap_or(false);
                    shown.push(if tag.starts_with('^') { !on } else { on });
                } else if tag == "else" {
                    if let Some(last) = shown.last_mut() {
                        *last = !*last;
                    }
                } else if tag.starts_with('/') {
                    shown.pop();
                } else if visible {
                    match helper(tag, v, &mut out.priority) {
                        Some(text) if raw || own_link(tag) => out.text.push_str(&text),
                        Some(text) => html(&mut out.text, &text),
                        None => out.text.push_str(written),
                    }
                }
            }
        }
    }
    if shown.iter().all(|&s| s) {
        out.text.push_str(rest);
    }
    out
}

/// What a `{{` begins.
enum Piece<'a> {
    Comment,
    /// A raw block: its tag (`raw`, or a section's) and its body as written.
    Raw { tag: &'a str, body: &'a str },
    /// What `{{…}}` holds, trimmed; `raw` of `{{{…}}}`, whose value is not HTML-encoded.
    Tag { tag: &'a str, raw: bool },
}

/// `s` HTML-encoded onto `out`: Datadog's "Variable content is HTML-encoded by default".
fn html(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Whether a tag is a link kiln makes (`{{link}}`, `{{log.link}}`, `{{issue.link}}`): its own URL,
/// percent-encoded where it holds a search, whose `&` joins its parameters.
fn own_link(tag: &str) -> bool {
    split(tag).is_some_and(|(name, args)| args.is_empty() && (name == "link" || name.ends_with(".link")))
}

/// The piece `s` (at a `{{`) begins with, and what follows it; none where it does not close.
fn piece_of(s: &str) -> Option<(Piece<'_>, &str)> {
    if let Some(inner) = s.strip_prefix("{{{{") {
        let close = inner.find("}}}}")?;
        let tag = inner.get(..close)?.trim();
        let name = tag.split_whitespace().next().unwrap_or("");
        let body = inner.get(close + 4..)?;
        let end = ["{{{{/", name, "}}}}"].concat();
        let at = body.find(&end)?;
        return Some((Piece::Raw { tag, body: body.get(..at)? }, body.get(at + end.len()..)?));
    }
    if let Some(inner) = s.strip_prefix("{{!") {
        // To its `--}}`, or its first `}}` (Datadog's page writes `{{!-- this is a comment }}` too).
        let close: Option<usize> = if inner.starts_with("--") { inner.find("--}}").map(|i| i + 4).or_else(|| inner.find("}}").map(|i| i + 2)) } else { inner.find("}}").map(|i| i + 2) };
        return Some((Piece::Comment, inner.get(close?..)?));
    }
    if let Some(inner) = s.strip_prefix("{{{") {
        let close = inner.find("}}}")?;
        return Some((Piece::Tag { tag: inner.get(..close)?.trim(), raw: true }, inner.get(close + 3..)?));
    }
    let inner = s.get(2..)?;
    let close = inner.find("}}")?;
    Some((Piece::Tag { tag: inner.get(..close)?.trim(), raw: false }, inner.get(close + 2..)?))
}

/// An argument of a section or a helper.
enum Arg<'a> {
    Quoted(String),
    /// A subexpression, `(eval "…")`: what its parentheses hold.
    Sub(&'a str),
    Word(&'a str),
}

impl Arg<'_> {
    /// Read as a variable's name: what it stands for (a subexpression's value).
    fn resolve(&self, v: &Vars<'_>) -> Option<String> {
        match self {
            Self::Quoted(s) => lookup(s, v),
            Self::Word(w) => lookup(w, v),
            Self::Sub(e) => sub(e, v),
        }
    }

    /// Read as a value: a string as it is, a word as written, a subexpression's value.
    fn text(&self, v: &Vars<'_>) -> Option<String> {
        match self {
            Self::Quoted(s) => Some(s.clone()),
            Self::Word(w) => Some((*w).to_owned()),
            Self::Sub(e) => sub(e, v),
        }
    }
}

/// A subexpression's value: Datadog's `(eval "…")`.
fn sub(e: &str, v: &Vars<'_>) -> Option<String> {
    match split(e)? {
        ("eval", args) => match args.as_slice() {
            [Arg::Quoted(x)] => eval(x, v),
            _ => None,
        },
        _ => None,
    }
}

/// A tag's name (its first word) and its arguments: strings quoted `"…"` or `'…'` (`\` escaping),
/// subexpressions in parentheses, words.
fn split(tag: &str) -> Option<(&str, Vec<Arg<'_>>)> {
    let tag = tag.trim();
    let (name, mut rest) = tag.split_at_checked(tag.find(char::is_whitespace).unwrap_or(tag.len()))?;
    let mut args = Vec::new();
    loop {
        rest = rest.trim_start();
        let Some(c) = rest.chars().next() else { break };
        match c {
            '"' | '\'' => {
                let (s, after) = quoted(rest)?;
                args.push(Arg::Quoted(s));
                rest = after;
            }
            '(' => {
                let close = closing(rest)?;
                args.push(Arg::Sub(rest.get(1..close)?.trim()));
                rest = rest.get(close + 1..)?;
            }
            _ => {
                let (w, after) = rest.split_at_checked(rest.find(char::is_whitespace).unwrap_or(rest.len()))?;
                args.push(Arg::Word(w));
                rest = after;
            }
        }
    }
    Some((name, args))
}

/// The string `s` begins with, quoted by its first character (`\` escaping the next), and what
/// follows its closing quote.
fn quoted(s: &str) -> Option<(String, &str)> {
    let q = s.chars().next()?;
    let body = s.get(q.len_utf8()..)?;
    let mut out = String::new();
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            out.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == q {
            return Some((out, body.get(i + c.len_utf8()..)?));
        } else {
            out.push(c);
        }
    }
    None
}

/// Where the `(` that `s` begins with closes, past quoted strings.
fn closing(s: &str) -> Option<usize> {
    let (mut depth, mut quote, mut escaped) = (0usize, None::<char>, false);
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whether a section's condition holds: the transition's kind (`is_alert`, ...), `is_renotify`,
/// `is_unknown`, `is_priority 'P1'`, or `is_match` (the variable holds one of the strings) and
/// `is_exact_match` (is one of them); none for a name it does not know.
fn condition(tag: &str, t: Transition, v: &Vars<'_>) -> Option<bool> {
    let (name, args) = split(tag)?;
    match name {
        "is_renotify" => Some(v.renotify),
        "is_unknown" => Some(v.unknown),
        "is_priority" => Some(v.priority == Some(priority_of(&args.first()?.text(v)?)?)),
        "is_match" | "is_exact_match" => {
            let (context, against) = args.split_first()?;
            // A variable a group or its record does not have reads as empty (Datadog's: "the
            // variable renders empty").
            let context = context.resolve(v).unwrap_or_default();
            let exact = name == "is_exact_match";
            Some(against.iter().filter_map(|a| a.text(v)).any(|s| if exact { same(&context, &s) } else { context.contains(s.as_str()) }))
        }
        other if args.is_empty() => t.is(other),
        _ => None,
    }
}

/// Datadog's `is_exact_match`: the same text, or the same number however written (`5`, `5.0`).
fn same(a: &str, b: &str) -> bool {
    a == b || matches!((a.trim().parse::<f64>(), b.trim().parse::<f64>()), (Ok(x), Ok(y)) if x == y)
}

/// `P1` to `P5` (or `1` to `5`), as 1 to 5.
fn priority_of(s: &str) -> Option<u8> {
    let s = s.trim();
    s.strip_prefix(['P', 'p']).unwrap_or(s).parse::<u8>().ok().filter(|p| (1..=5).contains(p))
}

/// A tag that opens no section: a helper's text, or a variable's; none for what it does not know.
fn helper(tag: &str, v: &Vars<'_>, priority: &mut Option<u8>) -> Option<String> {
    let (name, args) = split(tag)?;
    match (name, args.as_slice()) {
        (_, []) => lookup(name, v),
        ("eval", [Arg::Quoted(e)]) => eval(e, v),
        ("local_time", [time, zone]) => local_time(epoch_of(&time.text(v)?, v)?, Zone::named(&zone.text(v)?).ok()?),
        ("override_priority", [p]) => {
            *priority = Some(priority_of(&p.text(v)?)?);
            Some(String::new())
        }
        ("urlencode", [x]) => Some(urlencode(&x.resolve(v)?)),
        _ => None,
    }
}

/// The instant a time variable stands for (unix ms): `first_triggered_at`, `last_triggered_at`
/// (their `_epoch`s too), or a variable whose value is one.
fn epoch_of(name: &str, v: &Vars<'_>) -> Option<i64> {
    match name.trim().trim_end_matches("_epoch") {
        "first_triggered_at" => v.first_triggered,
        "last_triggered_at" => v.last_triggered,
        other => lookup(other, v)?.trim().parse().ok(),
    }
}

/// What a variable stands for, by its name: Datadog's own, one [`Vars::extra`] names, or a group's
/// tag (`host.name`; a bare `env` too, as Datadog's `is_match` example reads one).
fn lookup(name: &str, v: &Vars<'_>) -> Option<String> {
    // A key with dots, bracketed: `[dot.key.test].name`, `log.attributes.[error.message]`.
    let name = name.trim().replace(".[", ".").replace(['[', ']'], "");
    let n = name.as_str();
    let time = |ms: Option<i64>| ms.and_then(|ms| local_time(ms, Zone::UTC)).unwrap_or_default();
    let epoch = |ms: Option<i64>| ms.map(|ms| ms.to_string()).unwrap_or_default();
    Some(match n {
        "value" => v.value.map_or_else(|| "no data".to_owned(), num),
        "threshold" => num(v.threshold),
        "warn_threshold" => v.warn_threshold.map(num).unwrap_or_default(),
        "alert_recovery_threshold" => num(v.critical_recovery.unwrap_or(v.threshold)),
        "warn_recovery_threshold" => v.warning_recovery.or(v.warn_threshold).map(num).unwrap_or_default(),
        "ok_threshold" => v.ok_threshold.map(num).unwrap_or_default(),
        "comparator" => v.comparator.map(|o| o.as_str().to_owned()).unwrap_or_default(),
        "first_triggered_at" => time(v.first_triggered),
        "first_triggered_at_epoch" => epoch(v.first_triggered),
        "last_triggered_at" => time(v.last_triggered),
        "last_triggered_at_epoch" => epoch(v.last_triggered),
        // "The number of seconds the monitor has been in a triggered state", as of this event.
        "triggered_duration_sec" => v.first_triggered.map_or(0, |f| v.now.saturating_sub(f).max(0) / 1000).to_string(),
        "monitor.name" | "name" => v.monitor.to_owned(),
        "group" => v.group.iter().map(|(k, gv)| format!("{k}:{gv}")).collect::<Vec<_>>().join(","),
        "link" => v.link.to_owned(),
        _ => {
            return v
                .extra
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, x)| x.clone())
                .or_else(|| n.strip_suffix(".name").and_then(|k| tag(v.group, k)))
                .or_else(|| tag(v.group, n))
                .or_else(|| v.blank.iter().any(|p| n.strip_prefix(p.as_str()).is_some_and(|r| r.starts_with('.'))).then(String::new));
        }
    })
}

/// A group's tag's values, by its key (an attribute's with or without its `@`), sorted and joined by
/// commas.
fn tag(group: &[(String, String)], key: &str) -> Option<String> {
    let bare = key.trim_start_matches('@');
    let mut values: Vec<&str> = group.iter().filter(|(k, _)| k == key || k.trim_start_matches('@') == bare).map(|(_, x)| x.as_str()).collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.dedup();
    Some(values.join(","))
}

/// A number as a message writes one: whole numbers without a point, others to four places.
pub fn num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 { format!("{v:.0}") } else { format!("{v:.4}").trim_end_matches('0').trim_end_matches('.').to_owned() }
}

/// `ms` in `zone`, as Datadog's `local_time` writes it: ISO 8601's `yyyy-MM-dd HH:mm:ss±HH:mm`
/// (its page's `2021-05-31 23:43:27+09:00`). `{{first_triggered_at}}` and `{{last_triggered_at}}`
/// are "the UTC date and time", written so in UTC, Datadog's page showing no form of their own.
pub fn local_time(ms: i64, zone: Zone) -> Option<String> {
    let z = jiff::Timestamp::from_millisecond(ms).ok()?.to_zoned(zone.tz());
    let off = z.offset().seconds();
    let (sign, off) = if off < 0 { ('-', -off) } else { ('+', off) };
    Some(format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}{sign}{:02}:{:02}", z.year(), z.month(), z.day(), z.hour(), z.minute(), z.second(), off / 3600, off % 3600 / 60))
}

/// Percent-encoded as a part of a URL is (RFC 3986: its unreserved characters as they are).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Datadog's `{{eval "…"}}`: its operators (`+ - * / % ^`) and functions over the template's
/// variables; a number written as `{{value}}` is; none where it does not read.
fn eval(expr: &str, v: &Vars<'_>) -> Option<String> {
    match calc(expr, v)? {
        Ev::Num(x) if x.is_finite() => Some(num(x)),
        Ev::Num(_) => None,
        Ev::Str(s) => Some(s),
    }
}

/// A value an `{{eval}}` reads or makes.
#[derive(Debug, Clone, PartialEq)]
enum Ev {
    Num(f64),
    Str(String),
}

impl Ev {
    fn num(&self) -> Option<f64> {
        match self {
            Self::Num(x) => Some(*x),
            Self::Str(s) => s.trim().parse().ok(),
        }
    }

    fn text(&self) -> String {
        match self {
            Self::Num(x) => num(*x),
            Self::Str(s) => s.clone(),
        }
    }
}

enum Tok {
    Val(Ev),
    Name(String),
    Op(char),
    Open,
    Close,
    Comma,
}

/// An expression's tokens: numbers, strings (`'…'`, `"…"`), names (a variable's: `value`,
/// `last_triggered_at_epoch`, `host.name`; or a function's), operators, parentheses, commas.
fn toks(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
        let rest = s.get(i..)?;
        let (tok, len) = match c {
            c if c.is_whitespace() => {
                i += c.len_utf8();
                continue;
            }
            '(' => (Tok::Open, 1),
            ')' => (Tok::Close, 1),
            ',' => (Tok::Comma, 1),
            '+' | '-' | '*' | '/' | '%' | '^' => (Tok::Op(c), 1),
            '\'' | '"' => {
                let (text, after) = quoted(rest)?;
                (Tok::Val(Ev::Str(text)), rest.len() - after.len())
            }
            c if c.is_ascii_digit() || c == '.' => {
                let len = rest.find(|d: char| !(d.is_ascii_digit() || d == '.')).unwrap_or(rest.len());
                (Tok::Val(Ev::Num(rest.get(..len)?.parse().ok()?)), len)
            }
            c if c.is_alphabetic() || matches!(c, '_' | '@' | '[') => {
                let len = rest.find(|d: char| !(d.is_alphanumeric() || matches!(d, '_' | '.' | '@' | '[' | ']'))).unwrap_or(rest.len());
                (Tok::Name(rest.get(..len)?.to_owned()), len)
            }
            _ => return None,
        };
        out.push(tok);
        i += len;
    }
    Some(out)
}

/// An expression's postfix.
enum Step {
    Val(Ev),
    Var(String),
    Bin(char),
    Neg,
    Call(String, usize),
}

/// What waits on the operators' stack.
enum Held {
    Bin(char),
    Neg,
    Open,
    Call(String),
}

/// A binary operator's precedence, and whether it groups to the right (`^`); a sign's is 3, below
/// `^` (`-2^2` is -4) and above the rest.
fn rank(op: char) -> (u8, bool) {
    match op {
        '^' => (4, true),
        '*' | '/' | '%' => (2, false),
        _ => (1, false),
    }
}

/// An `{{eval}}` expression's value: Dijkstra's shunting-yard into postfix, then its stack.
fn calc(expr: &str, v: &Vars<'_>) -> Option<Ev> {
    let toks = toks(expr)?;
    let unheld = |h: Held| match h {
        Held::Bin(o) => Some(Step::Bin(o)),
        Held::Neg => Some(Step::Neg),
        Held::Open | Held::Call(_) => None,
    };
    let (mut out, mut held, mut argc): (Vec<Step>, Vec<Held>, Vec<usize>) = (Vec::new(), Vec::new(), Vec::new());
    // Whether an operand comes next (else an operator, a comma or a closing parenthesis).
    let mut operand = true;
    let mut i = 0;
    while let Some(tok) = toks.get(i) {
        i += 1;
        match tok {
            Tok::Val(e) if operand => {
                out.push(Step::Val(e.clone()));
                operand = false;
            }
            Tok::Name(n) if operand => {
                if matches!(toks.get(i), Some(Tok::Open)) {
                    held.push(Held::Call(n.clone()));
                    held.push(Held::Open);
                    argc.push(usize::from(!matches!(toks.get(i + 1), Some(Tok::Close))));
                    i += 1;
                } else {
                    out.push(Step::Var(n.clone()));
                    operand = false;
                }
            }
            Tok::Open if operand => held.push(Held::Open),
            Tok::Close => {
                loop {
                    match held.pop()? {
                        Held::Open => break,
                        h => out.push(unheld(h)?),
                    }
                }
                if matches!(held.last(), Some(Held::Call(_))) {
                    let Some(Held::Call(f)) = held.pop() else { return None };
                    out.push(Step::Call(f, argc.pop()?));
                } else if operand {
                    return None;
                }
                operand = false;
            }
            Tok::Comma if !operand => {
                while !matches!(held.last()?, Held::Open) {
                    out.push(unheld(held.pop()?)?);
                }
                // A call's comma: its `(` above its name.
                if !matches!(held.len().checked_sub(2).and_then(|k| held.get(k)), Some(Held::Call(_))) {
                    return None;
                }
                *argc.last_mut()? += 1;
                operand = true;
            }
            Tok::Op('-') if operand => held.push(Held::Neg),
            Tok::Op('+') if operand => {}
            Tok::Op(o) if !operand => {
                let (p, right) = rank(*o);
                while let Some(top) = held.last() {
                    let q = match top {
                        Held::Bin(t) => rank(*t).0,
                        Held::Neg => 3,
                        Held::Open | Held::Call(_) => break,
                    };
                    if q > p || (q == p && !right) {
                        out.push(unheld(held.pop()?)?);
                    } else {
                        break;
                    }
                }
                held.push(Held::Bin(*o));
                operand = true;
            }
            _ => return None,
        }
    }
    if operand {
        return None;
    }
    while let Some(h) = held.pop() {
        out.push(unheld(h)?);
    }
    run(&out, v)
}

/// A postfix expression's value; a variable a number where it reads as one.
fn run(steps: &[Step], v: &Vars<'_>) -> Option<Ev> {
    let mut st: Vec<Ev> = Vec::new();
    for step in steps {
        let x = match step {
            Step::Val(e) => e.clone(),
            Step::Var(n) => {
                let said = lookup(n, v)?;
                match said.trim().parse::<f64>() {
                    Ok(x) if x.is_finite() => Ev::Num(x),
                    _ => Ev::Str(said),
                }
            }
            Step::Neg => Ev::Num(-st.pop()?.num()?),
            Step::Bin(o) => {
                let b = st.pop()?.num()?;
                let a = st.pop()?.num()?;
                Ev::Num(match o {
                    '+' => a + b,
                    '-' => a - b,
                    '*' => a * b,
                    '/' => a / b,
                    // Its sign the divisor's, as Python's `%` has it (`-1 % 24` is 23).
                    '%' => a - b * (a / b).floor(),
                    _ => a.powf(b),
                })
            }
            Step::Call(f, n) => {
                let from = st.len().checked_sub(*n)?;
                let args = st.split_off(from);
                call(f, &args)?
            }
        };
        st.push(x);
    }
    if st.len() == 1 { st.pop() } else { None }
}

/// Datadog's evaluation functions, of numbers and of strings.
fn call(f: &str, a: &[Ev]) -> Option<Ev> {
    let x = |i: usize| a.get(i).and_then(Ev::num);
    let s = |i: usize| a.get(i).map(Ev::text);
    let unary = |g: fn(f64) -> f64| if a.len() == 1 { Some(Ev::Num(g(x(0)?))) } else { None };
    match (f, a.len()) {
        ("humanize_bytes", 1) => Some(Ev::Str(humanize(x(0)?, "Byte", "Bytes", &["kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"]))),
        ("humanize_bits", 1) => Some(Ev::Str(humanize(x(0)?, "bit", "bits", &["kb", "Mb", "Gb", "Tb", "Pb", "Eb", "Zb", "Yb"]))),
        ("abs", _) => unary(f64::abs),
        // "The numbers to the left of the decimal point" (12.345: 12), as Python's `int()`.
        ("int" | "trunc", _) => unary(f64::trunc),
        ("float", _) => unary(|y| y),
        ("dec", _) => unary(f64::fract),
        ("round", 1) => unary(f64::round),
        ("round", 2) => {
            let d = 10f64.powf(x(1)?.trunc());
            Some(Ev::Num((x(0)? * d).round() / d))
        }
        ("ceil", _) => unary(f64::ceil),
        ("floor", _) => unary(f64::floor),
        ("sgn", _) => unary(|y| if y == 0.0 { 0.0 } else { y.signum() }),
        ("to_bool", 1) => match x(0)? {
            y if y == 1.0 => Some(Ev::Str("true".into())),
            y if y == 0.0 => Some(Ev::Str("false".into())),
            _ => None,
        },
        ("exp", _) => unary(f64::exp),
        ("log10", _) => unary(f64::log10),
        ("sin", _) => unary(f64::sin),
        ("sinh", _) => unary(f64::sinh),
        ("asin", _) => unary(f64::asin),
        ("asinh", _) => unary(f64::asinh),
        ("cos", _) => unary(f64::cos),
        ("cosh", _) => unary(f64::cosh),
        ("acos", _) => unary(f64::acos),
        ("acosh", _) => unary(f64::acosh),
        ("tan", _) => unary(f64::tan),
        ("tanh", _) => unary(f64::tanh),
        ("atan", _) => unary(f64::atan),
        ("atanh", _) => unary(f64::atanh),
        ("atan2", 2) => Some(Ev::Num(x(0)?.atan2(x(1)?))),
        ("upper", 1) => Some(Ev::Str(s(0)?.to_uppercase())),
        ("lower", 1) => Some(Ev::Str(s(0)?.to_lowercase())),
        // Characters from `start` to `end` (to the end unless said): `substring("host:D", 5)` is `D`.
        ("substring", 2 | 3) => {
            let text = s(0)?;
            let n = text.chars().count();
            let place = |k: f64| if k <= 0.0 { 0 } else { (k.trunc() as usize).min(n) };
            let from = place(x(1)?);
            let to = if a.len() == 3 { place(x(2)?) } else { n };
            Some(Ev::Str(text.chars().skip(from).take(to.saturating_sub(from)).collect()))
        }
        ("strip", 1) => Some(Ev::Str(s(0)?.trim().to_owned())),
        ("strip", 2) => {
            let chars = s(1)?;
            Some(Ev::Str(s(0)?.trim_matches(|c| chars.contains(c)).to_owned()))
        }
        _ => None,
    }
}

/// A count of bytes (or bits) as Python's `humanize.naturalsize` writes one, in decimal units with
/// one decimal place (`1.5 MB`, `42 Bytes`): Datadog's guide shows no output of `humanize_bytes`.
fn humanize(x: f64, one: &str, many: &str, units: &[&str]) -> String {
    if x.abs() == 1.0 {
        return format!("{x:.0} {one}");
    }
    if x.abs() < 1000.0 {
        return format!("{:.0} {many}", x.trunc());
    }
    let mut scale = 1000.0;
    for (i, u) in units.iter().enumerate() {
        let next = scale * 1000.0;
        if x.abs() < next || i + 1 == units.len() {
            return format!("{:.1} {u}", x / scale);
        }
        scale = next;
    }
    format!("{x} {many}")
}
