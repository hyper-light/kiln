//! The Sensitive Data Scanner (`docs/observe.md` §8.9), Datadog's: scanning groups, each a filter (a
//! log search: the records it scans) and rules, each a pattern (one of the standard library's,
//! [`patterns`], or a regular expression of its own) matched in a record's message and attributes
//! (every one, or those its namespaces name, but those it excludes); what a rule matches is
//! replaced by a string, partly hidden, hashed (keyed: the same value the same hash on every machine
//! of this kiln, and no value found again by trying values) or left, and the record tagged with the
//! rule's tags. Run on every machine where records are taken, after the pipelines (a rule reads
//! what they parsed), before anything is kept: what a rule replaces is never stored, replicated or
//! archived.
//!
//! A record's tags are attributes (kiln keeps no tags apart): a key several matched rules tag holds
//! their values joined by commas (`sensitive_data:email_address,visa_card`).

pub mod kept;
pub mod patterns;

use kiln_obs::logs::record::Record;
use kiln_obs::logs::search::{Fields, RowScratch, Search};
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};

use patterns::{Standard, Validator};

/// Scanning groups at the most, rules a group holds at the most (Datadog's 20 and 50 a group).
pub const GROUPS: usize = 20;
pub const RULES: usize = 50;
/// A pattern's length at the most, and what its program may take (`regex`'s size limit: a pattern
/// that would grow past it is refused, as a group's set past [`SET_BYTES`]).
const PATTERN_BYTES: usize = 2_000;
const PROGRAM_BYTES: usize = 1 << 20;
const SET_BYTES: usize = 8 << 20;
/// Keywords a rule holds at the most, each this long at the most; how many characters before a
/// match one is looked for in (Datadog's 30, 50 at the most).
const KEYWORDS: usize = 30;
const KEYWORD_BYTES: usize = 100;
const WINDOW: u32 = 30;
const WINDOW_MAX: u32 = 50;
/// A replacement's length at the most.
const REPLACEMENT_BYTES: usize = 200;
/// What a rule replacing with a string puts in place of a match unless it says.
pub const REDACTED: &str = "[sensitive_data]";

fn yes() -> bool {
    true
}

fn priority() -> u8 {
    3
}

fn window() -> u32 {
    WINDOW
}

/// A group's filter: a log search, empty for every record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterDef {
    #[serde(default)]
    pub query: String,
}

/// A scanning group as Datadog's API gives one, its rules within.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub filter: FilterDef,
    #[serde(default = "yes")]
    pub is_enabled: bool,
    #[serde(default)]
    pub rules: Vec<RuleDef>,
}

/// What replaces what a rule matches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementKind {
    /// Nothing: the record tagged alone.
    #[default]
    None,
    /// A hash of it (keyed): the same value, the same hash.
    Hash,
    /// A string (`replacement_string`; [`REDACTED`] unless said).
    ReplacementString,
    /// Its first `number_of_chars` characters, each a `*`.
    PartialReplacementFromBeginning,
    /// Its last `number_of_chars` characters, each a `*`.
    PartialReplacementFromEnd,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    #[serde(rename = "type", default)]
    pub kind: ReplacementKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_string: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number_of_chars: Option<u32>,
}

/// Keywords one of which must be within `character_count` characters before a match for it to
/// count; or a standard pattern's own (`use_recommended_keywords`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordsDef {
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "window")]
    pub character_count: u32,
    #[serde(default)]
    pub use_recommended_keywords: bool,
}

/// A rule as Datadog's API gives one (its `standard_pattern` named by id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleDef {
    /// Given by the server; kept as a rule is edited.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// A standard pattern's id ([`patterns::STANDARD`]); else `pattern`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard_pattern_id: Option<String>,
    /// A regular expression (Rust's `regex`: no look-around or back-references); a group named `v`
    /// names what of a match is replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Where it looks: `message` and attributes by path (an attribute's own beneath it too); none,
    /// everywhere. `excluded_namespaces` where it does not.
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub excluded_namespaces: Vec<String>,
    /// `key:value` tags a match puts on its record: a standard pattern's unless said.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "yes")]
    pub is_enabled: bool,
    #[serde(default)]
    pub text_replacement: Replacement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub included_keyword_configuration: Option<KeywordsDef>,
    /// 1 (critical) to 5 (info): of two matches that overlap, the higher's is replaced.
    #[serde(default = "priority")]
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ScannerError(pub String);

fn invalid(s: impl Into<String>) -> ScannerError {
    ScannerError(s.into())
}

/// What a match's text becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    None,
    Hash,
    Text(String),
    Partial { from_end: bool, n: usize },
}

#[derive(Debug)]
struct Rule {
    regex: Regex,
    /// The fewest bytes it matches: a shorter text holds no match of it.
    min_len: usize,
    /// Whether a group `v` names what is replaced.
    sub: bool,
    validator: Validator,
    /// Lowercased; none, a match counts wherever.
    keywords: Vec<String>,
    window: usize,
    namespaces: Vec<String>,
    excluded: Vec<String>,
    tags: Vec<(String, String)>,
    action: Action,
    priority: u8,
}

/// Whether attribute `key` is at or beneath `path` (`http` holds `http.url`).
fn under(key: &str, path: &str) -> bool {
    key.strip_prefix(path).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

impl Rule {
    fn looks_in(&self, key: &str) -> bool {
        (self.namespaces.is_empty() || self.namespaces.iter().any(|n| under(key, n))) && !self.excluded.iter().any(|n| under(key, n))
    }

    /// Whether a keyword is within its window before byte `start` of `text`.
    fn near_keyword(&self, text: &str, start: usize) -> bool {
        let before = text.get(..start).unwrap_or_default();
        let from = before.char_indices().rev().nth(self.window.saturating_sub(1)).map_or(0, |(i, _)| i);
        let w = before.get(from..).unwrap_or_default().as_bytes();
        self.keywords.iter().any(|k| w.windows(k.len()).any(|x| x.eq_ignore_ascii_case(k.as_bytes())))
    }
}

#[derive(Debug)]
struct Group {
    filter: Option<Search>,
    /// The fewest bytes a rule of it matches.
    min_len: usize,
    rules: Vec<Rule>,
    /// The rules without keywords: one set of them (what may match is known in one pass), or the
    /// one there is (its own regex, its literal prefilter used: a set's all-matches search uses
    /// none); and their places in `rules`.
    plain: Plain,
    plain_rules: Vec<usize>,
    /// Any keyword of the rules that have them (case aside): none found, none of them matches.
    keywords: Option<Regex>,
    keyword_rules: Vec<usize>,
}

#[derive(Debug)]
enum Plain {
    None,
    One(Regex),
    Set(RegexSet),
}

impl Plain {
    fn is_match(&self, text: &str) -> bool {
        match self {
            Self::None => false,
            Self::One(r) => r.is_match(text),
            Self::Set(s) => s.is_match(text),
        }
    }
}

impl Group {
    fn looks_in(&self, key: &str) -> bool {
        self.rules.iter().any(|r| r.looks_in(key))
    }

    /// Whether a rule of it may match `text` (a pass of its set and of its keywords: none, none;
    /// none in a text shorter than any match of it).
    fn may_match(&self, text: &str) -> bool {
        text.len() >= self.min_len && (self.plain.is_match(text) || self.keywords.as_ref().is_some_and(|k| k.is_match(text)))
    }
}

/// Scanning groups compiled, and the key their hashes are made with.
#[derive(Debug, Default)]
pub struct Scanner {
    groups: Vec<Group>,
    key: Vec<u8>,
}

/// What scanning reuses from record to record.
#[derive(Debug, Default)]
pub struct Scratch {
    search: RowScratch,
    /// Matches of a text: its span, its rule's place.
    found: Vec<(usize, usize, usize)>,
    out: String,
    /// The rules that matched a record: their group's and their own places.
    hits: Vec<(usize, usize)>,
    /// Their tags: each its group's, its rule's and its own place.
    tags: Vec<(usize, usize, usize)>,
    joined: String,
}

/// The fewest bytes `p` matches, as `regex` reads it (none known: 0).
fn minimum_len(p: &str) -> usize {
    regex_syntax::ParserBuilder::new().build().parse(p).ok().and_then(|h| h.properties().minimum_len()).unwrap_or(0)
}

/// A pattern compiled within a rule's bound.
fn build(p: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(p).size_limit(PROGRAM_BYTES).build()
}

/// Patterns compiled as one set within a set's bound, its DFA's cache as large (a set of many
/// patterns has many states: a cache too small is cleared, and cleared again, as text is read).
fn build_set<S: AsRef<str>>(ps: &[S]) -> Result<RegexSet, regex::Error> {
    RegexSetBuilder::new(ps.iter().map(AsRef::as_ref)).size_limit(SET_BYTES).dfa_size_limit(SET_BYTES).build()
}

fn compile_rule(def: &RuleDef) -> Result<Rule, ScannerError> {
    let name = def.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(invalid("a rule's name: 1 to 200 characters"));
    }
    let standard: Option<&Standard> = match def.standard_pattern_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Some(patterns::find(id).ok_or_else(|| invalid(format!("rule {name}: no standard pattern {id:?}")))?),
        None => None,
    };
    let own = def.pattern.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let pattern: std::borrow::Cow<'_, str> = match (standard, own) {
        (Some(s), None) => patterns::ascii(s.pattern).into(),
        (None, Some(p)) => p.into(),
        (Some(_), Some(_)) => return Err(invalid(format!("rule {name}: a standard pattern or a pattern of its own, not both"))),
        (None, None) => return Err(invalid(format!("rule {name}: a standard pattern or a pattern of its own"))),
    };
    if pattern.len() > PATTERN_BYTES {
        return Err(invalid(format!("rule {name}: a pattern of {PATTERN_BYTES} bytes at the most")));
    }
    let regex = build(&pattern).map_err(|e| invalid(format!("rule {name}: its pattern: {e}")))?;
    if own.is_some() && regex.is_match("") {
        return Err(invalid(format!("rule {name}: a pattern that matches nothing at all would match everywhere")));
    }
    let sub = regex.capture_names().any(|n| n == Some("v"));
    // Keywords: its own; a standard pattern's when asked, or when the pattern needs them.
    let conf = def.included_keyword_configuration.clone().unwrap_or_default();
    let mut keywords: Vec<String> = conf.keywords.iter().map(|k| k.trim().to_lowercase()).filter(|k| !k.is_empty()).collect();
    if let Some(s) = standard
        && (conf.use_recommended_keywords || (s.keywords_required && keywords.is_empty()))
    {
        keywords.extend(s.keywords.iter().map(|k| k.to_lowercase()));
    }
    keywords.sort_unstable();
    keywords.dedup();
    if keywords.len() > KEYWORDS || keywords.iter().any(|k| k.len() > KEYWORD_BYTES) {
        return Err(invalid(format!("rule {name}: {KEYWORDS} keywords at the most, each {KEYWORD_BYTES} bytes at the most")));
    }
    let window = if def.included_keyword_configuration.is_some() { conf.character_count } else { WINDOW };
    if window == 0 || window > WINDOW_MAX {
        return Err(invalid(format!("rule {name}: keywords looked for 1 to {WINDOW_MAX} characters before a match")));
    }
    let path = |n: &String| n.trim().trim_start_matches('@').to_owned();
    let namespaces: Vec<String> = def.namespaces.iter().map(path).filter(|n| !n.is_empty()).collect();
    let excluded: Vec<String> = def.excluded_namespaces.iter().map(path).filter(|n| !n.is_empty()).collect();
    let mut tags = Vec::new();
    let given: Vec<String> = if def.tags.is_empty() {
        match standard {
            Some(s) => s.tags().to_vec(),
            None => vec![format!("sensitive_data:{}", slug(name))],
        }
    } else {
        def.tags.clone()
    };
    for t in &given {
        let t = t.trim();
        let (k, v) = t.split_once(':').ok_or_else(|| invalid(format!("rule {name}: a tag is key:value, not {t:?}")))?;
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || v.is_empty() || k.contains(',') || v.contains(',') {
            return Err(invalid(format!("rule {name}: a tag is key:value, neither empty nor holding a comma, not {t:?}")));
        }
        tags.push((k.to_owned(), v.to_owned()));
    }
    let r = &def.text_replacement;
    let action = match r.kind {
        ReplacementKind::None => Action::None,
        ReplacementKind::Hash => Action::Hash,
        ReplacementKind::ReplacementString => {
            let s = r.replacement_string.clone().unwrap_or_else(|| REDACTED.to_owned());
            if s.len() > REPLACEMENT_BYTES {
                return Err(invalid(format!("rule {name}: a replacement of {REPLACEMENT_BYTES} bytes at the most")));
            }
            Action::Text(s)
        }
        ReplacementKind::PartialReplacementFromBeginning | ReplacementKind::PartialReplacementFromEnd => {
            let n = r.number_of_chars.filter(|n| *n > 0).ok_or_else(|| invalid(format!("rule {name}: a partial replacement's number_of_chars, more than 0")))?;
            Action::Partial { from_end: r.kind == ReplacementKind::PartialReplacementFromEnd, n: n as usize }
        }
    };
    if !(1..=5).contains(&def.priority) {
        return Err(invalid(format!("rule {name}: a priority of 1 (critical) to 5 (info)")));
    }
    Ok(Rule {
        min_len: minimum_len(&pattern),
        regex,
        sub,
        validator: standard.map_or(Validator::None, |s| s.validator),
        keywords,
        window: window as usize,
        namespaces,
        excluded,
        tags,
        action,
        priority: def.priority,
    })
}

/// A name as a tag's value: lowercase letters, digits and underscores.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_owned()
}

/// A group checked and compiled (its rules that are on).
fn compile_group(def: &GroupDef) -> Result<Group, ScannerError> {
    let name = def.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(invalid("a scanning group's name: 1 to 200 characters"));
    }
    if def.rules.len() > RULES {
        return Err(invalid(format!("group {name}: {RULES} rules at the most")));
    }
    let q = def.filter.query.trim();
    let filter = if q.is_empty() || q == "*" { None } else { Some(kiln_obs::logs::search::parse(q).map_err(|e| invalid(format!("group {name}: its filter: {e}")))?) };
    let mut rules = Vec::new();
    for r in &def.rules {
        let rule = compile_rule(r)?;
        if r.is_enabled {
            rules.push(rule);
        }
    }
    let plain_rules: Vec<usize> = rules.iter().enumerate().filter(|(_, r)| r.keywords.is_empty()).map(|(i, _)| i).collect();
    let keyword_rules: Vec<usize> = rules.iter().enumerate().filter(|(_, r)| !r.keywords.is_empty()).map(|(i, _)| i).collect();
    let plain = match plain_rules.as_slice() {
        [] => Plain::None,
        [one] => rules.get(*one).map_or(Plain::None, |r| Plain::One(r.regex.clone())),
        _ => {
            let set: Vec<&str> = plain_rules.iter().filter_map(|&i| rules.get(i)).map(|r| r.regex.as_str()).collect();
            Plain::Set(build_set(&set).map_err(|e| invalid(format!("group {name}: its rules together: {e}")))?)
        }
    };
    let keywords = if keyword_rules.is_empty() {
        None
    } else {
        let mut all: Vec<String> = keyword_rules.iter().filter_map(|&i| rules.get(i)).flat_map(|r| r.keywords.iter().map(|k| regex::escape(k))).collect();
        all.sort_unstable();
        all.dedup();
        Some(RegexBuilder::new(&all.join("|")).case_insensitive(true).size_limit(SET_BYTES).build().map_err(|e| invalid(format!("group {name}: its keywords: {e}")))?)
    };
    let min_len = rules.iter().map(|r| r.min_len).min().unwrap_or(0);
    Ok(Group { filter, min_len, rules, plain, plain_rules, keywords, keyword_rules })
}

/// A group checked: named, compiling (its filter, every rule, on or not).
pub fn check(def: &GroupDef) -> Result<(), ScannerError> {
    compile_group(def).map(drop)
}

impl Scanner {
    /// The groups that are on, compiled, hashing with `key`; one that does not compile is skipped
    /// (said) so the rest still scan.
    pub fn compile(defs: &[GroupDef], key: &[u8]) -> Self {
        let mut groups = Vec::new();
        for d in defs.iter().filter(|d| d.is_enabled).take(GROUPS) {
            match compile_group(d) {
                Ok(g) if !g.rules.is_empty() => groups.push(g),
                Ok(_) => {}
                Err(e) => tracing::warn!(group = %d.name, error = %e, "a scanning group does not compile here; it is skipped"),
            }
        }
        Self { groups, key: key.to_vec() }
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Whether a group would scan what has these fields (a line's, before a record is made of it)
    /// and may find something in it: its filter matches them, and a text where a rule of it looks
    /// may match one (`attrs`, its attributes, asked once a group). What it says no to, no rule
    /// matches; what it says yes to, [`Scanner::scan`] reads.
    pub fn reads<'a, F: Fields, I: Iterator<Item = (&'a str, &'a str)>>(&self, view: &F, attrs: impl Fn() -> I, s: &mut Scratch) -> bool {
        self.groups.iter().any(|g| {
            g.filter.as_ref().is_none_or(|f| f.fields(view, &mut s.search)) && ((g.looks_in("message") && g.may_match(view.message())) || attrs().any(|(k, v)| g.looks_in(k) && g.may_match(v)))
        })
    }

    /// Every group whose filter matches `r` run on it: each match of a rule (its check passed, a
    /// keyword near it when it needs one) replaced as its rule says, and `r` tagged with each
    /// matching rule's tags. Whether anything matched.
    pub fn scan(&self, r: &mut Record, s: &mut Scratch) -> bool {
        s.hits.clear();
        for (gi, g) in self.groups.iter().enumerate() {
            if let Some(f) = &g.filter
                && !f.record(r, &mut s.search)
            {
                continue;
            }
            if g.looks_in("message") {
                scan_text(g, gi, "message", &mut r.message, &self.key, s);
            }
            for (k, v) in &mut r.attrs {
                if g.looks_in(k) {
                    scan_text(g, gi, k, v, &self.key, s);
                }
            }
        }
        if s.hits.is_empty() {
            return false;
        }
        s.hits.sort_unstable();
        s.hits.dedup();
        self.tag(r, s);
        true
    }

    /// `r` tagged with the tags of the rules `s.hits` names: each key its values, those it held
    /// before first, joined by commas.
    fn tag(&self, r: &mut Record, s: &mut Scratch) {
        let tag = |&(g, i, t): &(usize, usize, usize)| self.groups.get(g).and_then(|g| g.rules.get(i)).and_then(|r| r.tags.get(t)).map(|(k, v)| (k.as_str(), v.as_str()));
        s.tags.clear();
        for &(g, i) in &s.hits {
            let n = self.groups.get(g).and_then(|g| g.rules.get(i)).map_or(0, |r| r.tags.len());
            s.tags.extend((0..n).map(|t| (g, i, t)));
        }
        // By key then value, each pair once: a key's values a run.
        s.tags.sort_unstable_by(|a, b| tag(a).cmp(&tag(b)));
        s.tags.dedup_by(|a, b| tag(a) == tag(b));
        let mut at = 0;
        while let Some((key, _)) = s.tags.get(at).and_then(tag) {
            let run = s.tags.get(at..).map_or(0, |rest| rest.iter().take_while(|x| tag(x).is_some_and(|(k, _)| k == key)).count()).max(1);
            let values = s.tags.get(at..at + run).unwrap_or_default().iter().filter_map(tag).map(|(_, v)| v);
            s.joined.clear();
            match r.attrs.iter_mut().find(|(k, _)| k == key) {
                // What it held first, then each value it did not.
                Some((_, held)) => {
                    s.joined.push_str(held);
                    for v in values {
                        if !held.split(',').any(|h| h == v) {
                            if !s.joined.is_empty() {
                                s.joined.push(',');
                            }
                            s.joined.push_str(v);
                        }
                    }
                    std::mem::swap(held, &mut s.joined);
                }
                None => {
                    for v in values {
                        if !s.joined.is_empty() {
                            s.joined.push(',');
                        }
                        s.joined.push_str(v);
                    }
                    r.attrs.push((key.to_owned(), s.joined.clone()));
                }
            }
            at += run;
        }
    }
}

/// `text` (the value at `key`) scanned by group `g`: its rules' matches found, those replacing
/// applied; the rules that matched noted in `s.hits`.
fn scan_text(g: &Group, gi: usize, key: &str, text: &mut String, hash_key: &[u8], s: &mut Scratch) {
    s.found.clear();
    if text.len() < g.min_len {
        return;
    }
    // The rules without keywords that match somewhere (one pass of the set: none, rarely more).
    match &g.plain {
        Plain::None => {}
        // A match looked for (its captures' room made) only where there is one.
        Plain::One(r) => {
            if r.is_match(text)
                && let Some(&ri) = g.plain_rules.first()
            {
                find(g, ri, key, text, s);
            }
        }
        Plain::Set(set) => {
            if set.is_match(text) {
                for m in set.matches(text).iter() {
                    if let Some(&ri) = g.plain_rules.get(m) {
                        find(g, ri, key, text, s);
                    }
                }
            }
        }
    }
    if let Some(kw) = &g.keywords
        && kw.is_match(text)
    {
        for &ri in &g.keyword_rules {
            find(g, ri, key, text, s);
        }
    }
    if s.found.is_empty() {
        return;
    }
    s.hits.extend(s.found.iter().map(|&(_, _, ri)| (gi, ri)));
    // Those that replace, left to right; of two that overlap, the one that starts first (then the
    // rule of higher priority, then the longer match).
    let rule = |ri: usize| g.rules.get(ri);
    s.found.retain(|&(_, _, ri)| rule(ri).is_some_and(|r| r.action != Action::None));
    if s.found.is_empty() {
        return;
    }
    s.found.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| rule(a.2).map(|r| r.priority).cmp(&rule(b.2).map(|r| r.priority))).then_with(|| b.1.cmp(&a.1)));
    s.out.clear();
    let mut at = 0;
    for &(a, b, ri) in &s.found {
        if a < at {
            continue;
        }
        let (Some(before), Some(matched), Some(r)) = (text.get(at..a), text.get(a..b), rule(ri)) else { continue };
        s.out.push_str(before);
        replace(&r.action, matched, hash_key, &mut s.out);
        at = b;
    }
    s.out.push_str(text.get(at..).unwrap_or_default());
    std::mem::swap(text, &mut s.out);
}

/// Rule `ri`'s matches in `text` (at `key`, where it looks) into `s.found`: those its check passes
/// and, where it needs one, with a keyword near.
fn find(g: &Group, ri: usize, key: &str, text: &str, s: &mut Scratch) {
    let Some(rule) = g.rules.get(ri) else { return };
    if !rule.looks_in(key) {
        return;
    }
    let mut keep = |a: usize, b: usize, start: usize| {
        let ok = text.get(a..b).is_some_and(|m| rule.validator.ok(m)) && (rule.keywords.is_empty() || rule.near_keyword(text, start));
        if ok && a < b {
            s.found.push((a, b, ri));
        }
    };
    if rule.sub {
        for c in rule.regex.captures_iter(text) {
            if let (Some(whole), Some(v)) = (c.get(0), c.name("v")) {
                keep(v.start(), v.end(), whole.start());
            }
        }
    } else {
        for m in rule.regex.find_iter(text) {
            keep(m.start(), m.end(), m.start());
        }
    }
}

/// What `matched` becomes onto `out`.
fn replace(action: &Action, matched: &str, key: &[u8], out: &mut String) {
    match action {
        Action::None => out.push_str(matched),
        Action::Text(t) => out.push_str(t),
        Action::Hash => {
            // HMAC-SHA256 under this kiln's key, its first 8 bytes in hex (written in place).
            use hmac::{KeyInit as _, Mac as _};
            if let Ok(mut m) = hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
                m.update(matched.as_bytes());
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for b in m.finalize().into_bytes().iter().take(8) {
                    out.push(char::from(HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
                    out.push(char::from(HEX.get(usize::from(b & 15)).copied().unwrap_or(b'0')));
                }
            }
        }
        Action::Partial { from_end, n } => {
            let count = matched.chars().count();
            let hide = |i: usize| if *from_end { i >= count.saturating_sub(*n) } else { i < *n };
            for (i, c) in matched.chars().enumerate() {
                out.push(if hide(i) { '*' } else { c });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn rule(json: serde_json::Value) -> RuleDef {
        let mut v = json;
        if v.get("name").is_none() {
            v["name"] = "r".into();
        }
        serde_json::from_value(v).unwrap()
    }

    fn scanner(rules: Vec<RuleDef>, query: &str) -> Scanner {
        Scanner::compile(&[GroupDef { name: "g".into(), description: String::new(), filter: FilterDef { query: query.into() }, is_enabled: true, rules }], b"kiln-test-key")
    }

    fn rec(message: &str, attrs: &[(&str, &str)]) -> Record {
        Record { ts: 1, severity: 9, service: "web".into(), message: message.into(), attrs: attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(), ..Record::default() }
    }

    fn attr<'a>(r: &'a Record, k: &str) -> Option<&'a str> {
        r.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    /// Each standard pattern finds what its kind looks like, in a message as a log writes it, and
    /// passes over its near misses (a card one digit off, an SSN out of range, a key with no
    /// keyword near, a sum, a hash).
    #[test]
    fn the_standard_patterns_find_what_they_say_and_pass_over_near_misses() {
        let hits: &[(&str, &str, &str)] = &[
            ("visa_card", "paid with 4111 1111 1111 1111 today", "4111 1111 1111 1111"),
            ("visa_card", "card=4012888888881881;", "4012888888881881"),
            ("mastercard", "mc 5555-5555-5555-4444 ok", "5555-5555-5555-4444"),
            ("mastercard", "mc 2223003122003222", "2223003122003222"),
            ("amex_card", "amex 3782 822463 10005", "3782 822463 10005"),
            ("discover_card", "d 6011111111111117", "6011111111111117"),
            ("jcb_card", "j 3530111333300000", "3530111333300000"),
            ("diners_card", "dc 30569309025904", "30569309025904"),
            ("iban", "pay to GB82 WEST 1234 5698 7654 32 please", "GB82 WEST 1234 5698 7654 32"),
            ("us_ssn", "ssn 078-05-1120", "078-05-1120"),
            ("email_address", "from <jane.doe+tag@mail.example.co.uk>", "jane.doe+tag@mail.example.co.uk"),
            ("international_phone_number", "call +442071838750 now", "+442071838750"),
            ("us_phone_number", "phone (555) 123-4567", "(555) 123-4567"),
            ("ipv4_address", "from 192.168.10.254:443", "192.168.10.254"),
            ("ipv6_address", "peer 2001:db8::ff00:42:8329 up", "2001:db8::ff00:42:8329"),
            ("mac_address", "nic 00:1A:2b:3C:4d:5E", "00:1A:2b:3C:4d:5E"),
            ("aws_access_key_id", "key AKIAIOSFODNN7EXAMPLE used", "AKIAIOSFODNN7EXAMPLE"),
            ("aws_secret_access_key", "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            ("github_token", "token ghp_0123456789abcdefghijABCDEFGHIJ012345 x", "ghp_0123456789abcdefghijABCDEFGHIJ012345"),
            ("gitlab_token", "glpat-abcdefghij0123456789", "glpat-abcdefghij0123456789"),
            ("slack_token", "xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx", "xoxb-123456789012-1234567890123-AbCdEfGhIjKlMnOpQrStUvWx"),
            (
                "slack_webhook_url",
                "hook https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX done",
                "https://hooks.slack.com/services/T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX",
            ),
            ("stripe_secret_key", "sk_live_abcdefghijklmnopqrstuvwx1234", "sk_live_abcdefghijklmnopqrstuvwx1234"),
            ("google_api_key", "AIzaSyA1234567890abcdefghijklmnopqrstuv", "AIzaSyA1234567890abcdefghijklmnopqrstuv"),
            ("npm_token", "npm_abcdefghijklmnopqrstuvwxyz0123456789", "npm_abcdefghijklmnopqrstuvwxyz0123456789"),
            ("sendgrid_api_key", "SG.abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG", "SG.abcdefghijklmnopqrstuv.abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG"),
            (
                "json_web_token",
                "auth eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            ),
            ("private_key", "k=-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY----- end", "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----"),
            ("password_in_url", "dsn postgres://app:s3cr3t-pw@db:5432/x", "s3cr3t-pw"),
            ("bearer_token", "Authorization: Bearer abcDEF123456ghiJKL789.mnoPQR", "abcDEF123456ghiJKL789.mnoPQR"),
            ("secret_assignment", r#"{"api_key": "k-0123456789"}"#, "k-0123456789"),
            ("secret_assignment", "login password=hunter22 ok", "hunter22"),
            (
                "azure_storage_key",
                "DefaultEndpointsProtocol=https;AccountName=a;AccountKey=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwx==;EndpointSuffix=x",
                "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwx==",
            ),
            ("datadog_api_key", "dd_api_key: 0123456789abcdef0123456789abcdef", "0123456789abcdef0123456789abcdef"),
        ];
        for (id, text, found) in hits {
            let s = scanner(vec![rule(serde_json::json!({ "standard_pattern_id": id, "text_replacement": { "type": "replacement_string", "replacement_string": "<X>" } }))], "*");
            let mut r = rec(text, &[]);
            assert!(s.scan(&mut r, &mut Scratch::default()), "{id} found nothing in {text:?}");
            assert_eq!(r.message, text.replacen(found, "<X>", 1), "{id}");
            assert_eq!(attr(&r, "sensitive_data"), Some(*id), "{id}'s tag");
        }
        let misses: &[(&str, &str)] = &[
            ("visa_card", "paid with 4111 1111 1111 1112 today"),
            ("visa_card", "order 41111111111111111111"),
            ("mastercard", "5555-5555-5555-4445"),
            ("us_ssn", "000-12-3456 and 666-01-0001 and 901-12-3456"),
            ("iban", "GB82 WEST 1234 5698 7654 33"),
            ("email_address", "user@localhost and a@b"),
            ("ipv4_address", "version 256.1.1.1 and 1.2.3"),
            ("aws_secret_access_key", "sha 0123456789012345678901234567890123456789 and wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            ("datadog_api_key", "md5 0123456789abcdef0123456789abcdef"),
            ("github_token", "ghp_short"),
            ("password_in_url", "https://example.com/a:b@c"),
        ];
        for (id, text) in misses {
            let s = scanner(vec![rule(serde_json::json!({ "standard_pattern_id": id, "text_replacement": { "type": "replacement_string" } }))], "*");
            let mut r = rec(text, &[]);
            assert!(!s.scan(&mut r, &mut Scratch::default()), "{id} found something in {text:?}: {}", r.message);
            assert_eq!(r.message, *text);
        }
    }

    /// What replaces a match: a string, its first or last characters hidden, a hash (keyed: the same
    /// value the same hash under one key, another under another), or nothing but the tags.
    #[test]
    fn a_match_is_replaced_as_its_rule_says() {
        let text = "card 4111111111111111 end";
        let with = |t: serde_json::Value| {
            let s = scanner(vec![rule(serde_json::json!({ "standard_pattern_id": "visa_card", "text_replacement": t }))], "");
            let mut r = rec(text, &[]);
            s.scan(&mut r, &mut Scratch::default());
            r.message
        };
        assert_eq!(with(serde_json::json!({ "type": "replacement_string" })), format!("card {REDACTED} end"));
        assert_eq!(with(serde_json::json!({ "type": "replacement_string", "replacement_string": "[card]" })), "card [card] end");
        assert_eq!(with(serde_json::json!({ "type": "partial_replacement_from_beginning", "number_of_chars": 12 })), "card ************1111 end");
        assert_eq!(with(serde_json::json!({ "type": "partial_replacement_from_end", "number_of_chars": 4 })), "card 411111111111**** end");
        assert_eq!(with(serde_json::json!({ "type": "none" })), text);
        let hashed = with(serde_json::json!({ "type": "hash" }));
        let h = hashed.strip_prefix("card ").and_then(|h| h.strip_suffix(" end")).unwrap();
        assert_eq!(h.len(), 16);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(hashed, with(serde_json::json!({ "type": "hash" })), "the same value, the same hash");
        let other = Scanner::compile(
            &[GroupDef {
                name: "g".into(),
                description: String::new(),
                filter: FilterDef::default(),
                is_enabled: true,
                rules: vec![rule(serde_json::json!({ "standard_pattern_id": "visa_card", "text_replacement": { "type": "hash" } }))],
            }],
            b"another-key",
        );
        let mut r = rec(text, &[]);
        other.scan(&mut r, &mut Scratch::default());
        assert_ne!(r.message, hashed, "another key, another hash");
        // Of a match with a group `v`, the group alone.
        let s = scanner(vec![rule(serde_json::json!({ "pattern": "user=(?P<v>[a-z]+)", "text_replacement": { "type": "replacement_string", "replacement_string": "?" } }))], "");
        let mut r = rec("login user=ada ok", &[]);
        s.scan(&mut r, &mut Scratch::default());
        assert_eq!(r.message, "login user=? ok");
        assert_eq!(attr(&r, "sensitive_data"), Some("r"), "a pattern's own rule tagged by its name");
    }

    /// Where a rule looks: everywhere unless its namespaces say (an attribute's own beneath it too,
    /// `message` the message), never where it excludes; a group's filter picks its records.
    #[test]
    fn rules_look_where_their_namespaces_say_in_the_records_their_group_picks() {
        let email = |ns: serde_json::Value, ex: serde_json::Value| {
            rule(
                serde_json::json!({ "standard_pattern_id": "email_address", "namespaces": ns, "excluded_namespaces": ex, "text_replacement": { "type": "replacement_string", "replacement_string": "E" } }),
            )
        };
        let r0 = rec("from a@x.io", &[("usr.email", "b@x.io"), ("usr", "c@x.io"), ("http.referer", "d@x.io"), ("user", "e@x.io")]);
        let run = |s: &Scanner| {
            let mut r = r0.clone();
            s.scan(&mut r, &mut Scratch::default());
            (r.message.clone(), r.attrs.iter().filter(|(k, _)| k != "sensitive_data" && k != "sensitive_data_category").map(|(_, v)| v.clone()).collect::<Vec<_>>())
        };
        assert_eq!(run(&scanner(vec![email(serde_json::json!([]), serde_json::json!([]))], "")), ("from E".to_owned(), vec!["E".to_owned(); 4]));
        assert_eq!(
            run(&scanner(vec![email(serde_json::json!(["@usr"]), serde_json::json!([]))], "")),
            ("from a@x.io".to_owned(), vec!["E".into(), "E".into(), "d@x.io".into(), "e@x.io".into()]),
            "usr and what is beneath it, not user"
        );
        assert_eq!(
            run(&scanner(vec![email(serde_json::json!([]), serde_json::json!(["message", "usr.email"]))], "")),
            ("from a@x.io".to_owned(), vec!["b@x.io".into(), "E".into(), "E".into(), "E".into()])
        );
        // The group's filter: web's records alone.
        let s = scanner(vec![email(serde_json::json!([]), serde_json::json!([]))], "service:api");
        let mut r = r0.clone();
        assert!(!s.scan(&mut r, &mut Scratch::default()));
        assert_eq!(r, r0);
    }

    /// A rule with keywords counts a match only with one of them (case aside) within its window
    /// before it; a standard pattern's own when asked.
    #[test]
    fn keywords_near_a_match_make_it_count() {
        let ssn = |k: serde_json::Value| {
            scanner(
                vec![rule(
                    serde_json::json!({ "standard_pattern_id": "us_ssn", "included_keyword_configuration": k, "text_replacement": { "type": "replacement_string", "replacement_string": "S" } }),
                )],
                "",
            )
        };
        let s = ssn(serde_json::json!({ "keywords": ["SSN"], "character_count": 10 }));
        for (text, want) in [
            ("ssn: 078-05-1120", "ssn: S"),
            ("Customer SSN 078-05-1120", "Customer SSN S"),
            ("ssn and then some words 078-05-1120", "ssn and then some words 078-05-1120"),
            ("078-05-1120 ssn", "078-05-1120 ssn"),
        ] {
            let mut r = rec(text, &[]);
            s.scan(&mut r, &mut Scratch::default());
            assert_eq!(r.message, want, "{text}");
        }
        let s = ssn(serde_json::json!({ "use_recommended_keywords": true }));
        let mut r = rec("social security 078-05-1120", &[]);
        s.scan(&mut r, &mut Scratch::default());
        assert_eq!(r.message, "social security S");
    }

    /// Overlapping matches: the one that starts first replaced (then the rule of higher priority);
    /// a rule that only tags never keeps another from replacing; every matching rule's tags put on,
    /// a key several tag holding their values joined, what it held kept first; scanning again
    /// changes nothing.
    #[test]
    fn overlaps_tags_and_a_second_scan() {
        let s = scanner(
            vec![
                rule(serde_json::json!({ "name": "emails", "standard_pattern_id": "email_address", "text_replacement": { "type": "none" }, "priority": 1 })),
                rule(
                    serde_json::json!({ "name": "names", "pattern": "jane[a-z.]*", "tags": ["sensitive_data:name", "team:id"], "text_replacement": { "type": "replacement_string", "replacement_string": "N" }, "priority": 5 }),
                ),
                rule(serde_json::json!({ "name": "cards", "standard_pattern_id": "visa_card", "text_replacement": { "type": "replacement_string", "replacement_string": "C" } })),
            ],
            "",
        );
        let mut r = rec("jane.doe@x.io paid 4111111111111111", &[("sensitive_data", "earlier")]);
        assert!(s.scan(&mut r, &mut Scratch::default()));
        assert_eq!(r.message, "N@x.io paid C");
        assert_eq!(attr(&r, "sensitive_data"), Some("earlier,email_address,name,visa_card"));
        assert_eq!(attr(&r, "sensitive_data_category"), Some("credit_card,pii"));
        assert_eq!(attr(&r, "team"), Some("id"));
        let once = r.clone();
        s.scan(&mut r, &mut Scratch::default());
        assert_eq!(r.message, once.message, "a second scan finds nothing more to replace");
        assert_eq!(r.attrs, once.attrs, "and its tags are not doubled");
    }

    /// `reads` never says no to what `scan` would change: of random texts mixing what the patterns
    /// match with their near misses, every record a scan changes or tags was read.
    #[test]
    fn what_reads_passes_over_no_scan_would_touch() {
        let s = Scanner::compile(
            &[GroupDef {
                name: "all".into(),
                description: String::new(),
                filter: FilterDef::default(),
                is_enabled: true,
                rules: patterns::STANDARD.iter().map(|p| rule(serde_json::json!({ "name": p.id, "standard_pattern_id": p.id, "text_replacement": { "type": "hash" } }))).collect(),
            }],
            b"k",
        );
        assert!(!s.is_empty(), "the whole library compiles as one group");
        let pieces = [
            "4111 1111 1111 1111",
            "4111 1111 1111 1112",
            "a@b.io",
            "078-05-1120",
            "ssn",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_secret",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "10.0.0.1",
            "GET /x 200",
            " ",
            "token=",
            "Bearer ",
            "abcdefghij0123456789xy",
        ];
        let mut rng = 7u64;
        let mut next = |n: usize| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % n as u64) as usize
        };
        let (mut read, mut touched) = (0, 0);
        for _ in 0..3_000 {
            let msg: String = (0..1 + next(5)).map(|_| pieces[next(pieces.len())]).collect::<Vec<_>>().join(" ");
            let attr_v: String = (0..next(3)).map(|_| pieces[next(pieces.len())]).collect::<Vec<_>>().join("");
            let r0 = rec(&msg, &[("detail", &attr_v)]);
            let view = r0.clone();
            let said = s.reads(&view, || view.attrs.iter().map(|(k, v)| (k.as_str(), v.as_str())), &mut Scratch::default());
            let mut r = r0.clone();
            let changed = s.scan(&mut r, &mut Scratch::default());
            assert!(said || !changed, "a record reads passed over was changed: {msg:?} {attr_v:?}");
            read += usize::from(said);
            touched += usize::from(changed);
        }
        assert!(touched > 500 && read >= touched, "{touched} changed, {read} read");
    }

    /// What does not compile, or is not a rule, is refused, saying why.
    #[test]
    fn rules_that_are_not_rules_are_refused() {
        let group = |rules: Vec<RuleDef>| GroupDef { name: "g".into(), description: String::new(), filter: FilterDef::default(), is_enabled: true, rules };
        for (r, says) in [
            (serde_json::json!({ "pattern": "(" }), "its pattern"),
            (serde_json::json!({}), "a standard pattern or a pattern of its own"),
            (serde_json::json!({ "standard_pattern_id": "nope" }), "no standard pattern"),
            (serde_json::json!({ "standard_pattern_id": "visa_card", "pattern": "x" }), "not both"),
            (serde_json::json!({ "pattern": "a*" }), "would match everywhere"),
            (serde_json::json!({ "pattern": "x", "tags": ["nokey"] }), "key:value"),
            (serde_json::json!({ "pattern": "x", "priority": 9 }), "priority"),
            (serde_json::json!({ "pattern": "x", "text_replacement": { "type": "partial_replacement_from_end" } }), "number_of_chars"),
            (serde_json::json!({ "pattern": "x", "included_keyword_configuration": { "keywords": ["k"], "character_count": 99 } }), "characters before a match"),
            (serde_json::json!({ "pattern": "x{1000}{1000}" }), "its pattern"),
        ] {
            let e = check(&group(vec![rule(r.clone())])).unwrap_err();
            assert!(e.0.contains(says), "{r}: {e}");
        }
        assert!(check(&GroupDef { filter: FilterDef { query: "(".into() }, ..group(vec![]) }).unwrap_err().0.contains("its filter"));
    }
}
