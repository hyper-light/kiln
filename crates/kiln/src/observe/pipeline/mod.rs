//! Log pipelines (`docs/observe.md` §8.2), Datadog's: each a filter (a search) and processors run
//! in order on every record the filter matches, the pipelines themselves in order, so a pipeline
//! exported from Datadog reads here. The processors are Datadog's: the grok parser, the date,
//! status, service, message, trace id and attribute remappers, the URL and user-agent parsers, the
//! category, arithmetic, string builder and lookup processors, and a pipeline nested in one.

pub mod builtin;
pub mod expr;
pub mod grok;
pub mod kept;
mod matcher;
pub(crate) mod spare;

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use kiln_obs::logs::record::{Record, severities};
use kiln_obs::logs::search::{RowScratch, Search};

/// Processors a pipeline holds at the most (Datadog's 20, and its nesting one deep: so here).
pub const PROCESSORS: usize = 50;
/// A lookup table's rows at the most.
pub const LOOKUP_ROWS: usize = 10_000;

fn yes() -> bool {
    true
}

fn message() -> String {
    "message".into()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterDef {
    #[serde(default)]
    pub query: String,
}

/// A pipeline as Datadog's API gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineDef {
    #[serde(default)]
    pub name: String,
    #[serde(default = "yes")]
    pub is_enabled: bool,
    #[serde(default)]
    pub filter: FilterDef,
    #[serde(default)]
    pub processors: Vec<ProcessorDef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrokDef {
    #[serde(default)]
    pub support_rules: String,
    #[serde(default)]
    pub match_rules: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryDef {
    pub filter: FilterDef,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessorDef {
    #[serde(default)]
    pub name: String,
    #[serde(default = "yes")]
    pub is_enabled: bool,
    #[serde(flatten)]
    pub kind: KindDef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum KindDef {
    GrokParser {
        #[serde(default = "message")]
        source: String,
        #[serde(default)]
        samples: Vec<String>,
        grok: GrokDef,
    },
    DateRemapper {
        sources: Vec<String>,
    },
    StatusRemapper {
        sources: Vec<String>,
    },
    ServiceRemapper {
        sources: Vec<String>,
    },
    MessageRemapper {
        sources: Vec<String>,
    },
    TraceIdRemapper {
        sources: Vec<String>,
    },
    AttributeRemapper {
        sources: Vec<String>,
        target: String,
        #[serde(default)]
        target_format: Option<String>,
        #[serde(default)]
        preserve_source: bool,
        #[serde(default)]
        override_on_conflict: bool,
        /// Datadog's `attribute` or `tag`: a record's tags are its attributes here.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_type: Option<String>,
    },
    UrlParser {
        sources: Vec<String>,
        target: String,
        #[serde(default)]
        normalize_ending_slashes: bool,
    },
    UserAgentParser {
        sources: Vec<String>,
        target: String,
        #[serde(default)]
        is_encoded: bool,
    },
    CategoryProcessor {
        target: String,
        categories: Vec<CategoryDef>,
    },
    ArithmeticProcessor {
        expression: String,
        target: String,
        #[serde(default)]
        is_replace_missing: bool,
    },
    StringBuilderProcessor {
        template: String,
        target: String,
        #[serde(default)]
        is_replace_missing: bool,
    },
    LookupProcessor {
        source: String,
        target: String,
        lookup_table: Vec<String>,
        #[serde(default)]
        default_lookup: Option<String>,
    },
    Pipeline {
        #[serde(default)]
        filter: FilterDef,
        #[serde(default)]
        processors: Vec<ProcessorDef>,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PipelineError {
    #[error("{0}")]
    Invalid(String),
}

fn invalid(s: impl Into<String>) -> PipelineError {
    PipelineError::Invalid(s.into())
}

/// A processor compiled.
#[derive(Debug)]
enum Proc {
    Grok { source: String, grok: grok::Grok },
    Date(Vec<String>),
    Status(Vec<String>),
    Service(Vec<String>),
    Message(Vec<String>),
    TraceId(Vec<String>),
    Attr { sources: Vec<String>, target: String, format: Format, preserve: bool, overwrite: bool },
    Url { sources: Vec<String>, target: String, trim_slashes: bool },
    UserAgent { sources: Vec<String>, target: String, encoded: bool },
    Category { target: String, categories: Vec<(Search, String)> },
    Arithmetic { expr: expr::Expr, target: String, replace_missing: bool },
    Builder { template: Vec<Piece>, target: String, replace_missing: bool },
    Lookup { source: String, target: String, table: HashMap<String, String>, default: Option<String> },
    Nested(Pipeline),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Auto,
    String,
    Integer,
    Double,
}

/// A string builder's template: text, and `%{attribute}`s.
#[derive(Debug)]
enum Piece {
    Text(String),
    Attr(String),
}

/// A pipeline compiled: its filter (none: every record) and its processors.
#[derive(Debug)]
pub struct Pipeline {
    pub name: String,
    filter: Option<Search>,
    procs: Vec<Proc>,
}

fn filter_of(f: &FilterDef) -> Result<Option<Search>, PipelineError> {
    let q = f.query.trim();
    if q.is_empty() || q == "*" {
        return Ok(None);
    }
    kiln_obs::logs::search::parse(q).map(Some).map_err(|e| invalid(format!("a filter: {e}")))
}

fn template(t: &str) -> Result<Vec<Piece>, PipelineError> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(at) = rest.find("%{") {
        if at > 0 {
            out.push(Piece::Text(rest.get(..at).unwrap_or_default().to_owned()));
        }
        let after = rest.get(at + 2..).unwrap_or_default();
        let end = after.find('}').ok_or_else(|| invalid(format!("a template's %{{ does not close: {t}")))?;
        let name = after.get(..end).unwrap_or_default().trim();
        if name.is_empty() {
            return Err(invalid(format!("a template's %{{}} names no attribute: {t}")));
        }
        out.push(Piece::Attr(name.to_owned()));
        rest = after.get(end + 1..).unwrap_or_default();
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest.to_owned()));
    }
    Ok(out)
}

impl PipelineDef {
    /// The pipeline compiled: its filter, rules, expressions and tables checked.
    pub fn compile(&self) -> Result<Pipeline, PipelineError> {
        compile(&self.name, &self.filter, &self.processors, 0)
    }
}

fn compile(name: &str, filter: &FilterDef, processors: &[ProcessorDef], depth: usize) -> Result<Pipeline, PipelineError> {
    if processors.len() > PROCESSORS {
        return Err(invalid(format!("{PROCESSORS} processors a pipeline at the most")));
    }
    let mut procs = Vec::new();
    for (i, p) in processors.iter().enumerate() {
        if !p.is_enabled {
            continue;
        }
        let at = |e: String| invalid(format!("processor {} ({}): {e}", i + 1, if p.name.is_empty() { "unnamed" } else { &p.name }));
        let nonempty = |v: &[String], what: &str| if v.iter().any(|s| !s.trim().is_empty()) { Ok(v.to_vec()) } else { Err(at(format!("{what}: an attribute at the least"))) };
        procs.push(match &p.kind {
            KindDef::GrokParser { source, grok, .. } => Proc::Grok { source: source.clone(), grok: grok::Grok::compile(&grok.match_rules, &grok.support_rules).map_err(|e| at(e.to_string()))? },
            KindDef::DateRemapper { sources } => Proc::Date(nonempty(sources, "sources")?),
            KindDef::StatusRemapper { sources } => Proc::Status(nonempty(sources, "sources")?),
            KindDef::ServiceRemapper { sources } => Proc::Service(nonempty(sources, "sources")?),
            KindDef::MessageRemapper { sources } => Proc::Message(nonempty(sources, "sources")?),
            KindDef::TraceIdRemapper { sources } => Proc::TraceId(nonempty(sources, "sources")?),
            KindDef::AttributeRemapper { sources, target, target_format, preserve_source, override_on_conflict, .. } => Proc::Attr {
                sources: nonempty(sources, "sources")?,
                target: if target.trim().is_empty() { return Err(at("a target".into())) } else { target.clone() },
                format: match target_format.as_deref().unwrap_or("auto") {
                    "auto" => Format::Auto,
                    "string" => Format::String,
                    "integer" => Format::Integer,
                    "double" => Format::Double,
                    other => return Err(at(format!("a target format: auto, string, integer or double, not {other}"))),
                },
                preserve: *preserve_source,
                overwrite: *override_on_conflict,
            },
            KindDef::UrlParser { sources, target, normalize_ending_slashes } => Proc::Url { sources: nonempty(sources, "sources")?, target: target.clone(), trim_slashes: *normalize_ending_slashes },
            KindDef::UserAgentParser { sources, target, is_encoded } => Proc::UserAgent { sources: nonempty(sources, "sources")?, target: target.clone(), encoded: *is_encoded },
            KindDef::CategoryProcessor { target, categories } => {
                let mut cats = Vec::new();
                for c in categories {
                    cats.push((filter_of(&c.filter).map_err(|e| at(e.to_string()))?.unwrap_or_else(Search::all), c.name.clone()));
                }
                Proc::Category { target: target.clone(), categories: cats }
            }
            KindDef::ArithmeticProcessor { expression, target, is_replace_missing } => Proc::Arithmetic { expr: expr::Expr::parse(expression).map_err(at)?, target: target.clone(), replace_missing: *is_replace_missing },
            KindDef::StringBuilderProcessor { template: t, target, is_replace_missing } => Proc::Builder { template: template(t).map_err(|e| at(e.to_string()))?, target: target.clone(), replace_missing: *is_replace_missing },
            KindDef::LookupProcessor { source, target, lookup_table, default_lookup } => {
                if lookup_table.len() > LOOKUP_ROWS {
                    return Err(at(format!("{LOOKUP_ROWS} rows a lookup table at the most")));
                }
                let mut table = HashMap::new();
                for row in lookup_table {
                    let (k, v) = row.split_once(',').ok_or_else(|| at(format!("a lookup row is key,value: {row}")))?;
                    table.insert(k.trim().to_owned(), v.trim().to_owned());
                }
                Proc::Lookup { source: source.clone(), target: target.clone(), table, default: default_lookup.clone() }
            }
            KindDef::Pipeline { filter, processors } => {
                if depth > 0 {
                    return Err(at("a pipeline in a pipeline in a pipeline: one level deep at the most".into()));
                }
                Proc::Nested(compile(&p.name, filter, processors, depth + 1)?)
            }
        });
    }
    Ok(Pipeline { name: name.to_owned(), filter: filter_of(filter)?, procs })
}

/// An attribute's value: a record's own fields by their names, else its attributes.
fn get<'a>(r: &'a Record, key: &str) -> Option<&'a str> {
    match key {
        "message" => Some(&r.message),
        "service" => Some(&r.service),
        "host" => Some(&r.host),
        "source" => Some(&r.source),
        _ => r.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
    }
}

fn first<'a>(r: &'a Record, sources: &[String]) -> Option<(usize, &'a str)> {
    sources.iter().enumerate().find_map(|(i, s)| get(r, s).filter(|v| !v.is_empty()).map(|v| (i, v)))
}

/// An attribute set: the first of its name replaced, or added.
fn set(r: &mut Record, key: &str, value: String) {
    put(r, Cow::Borrowed(key), value);
}

/// [`set`], its name moved in when it comes owned (copied only when it is borrowed and new). What
/// it replaces, and a name it does not keep, are handed back (`spare`).
fn put(r: &mut Record, key: Cow<'_, str>, value: String) {
    let slot = match key.as_ref() {
        "message" => &mut r.message,
        "service" => &mut r.service,
        "host" => &mut r.host,
        "source" => &mut r.source,
        k => match r.attrs.iter().position(|(a, _)| a == k) {
            Some(i) => match r.attrs.get_mut(i) {
                Some(slot) => &mut slot.1,
                None => return,
            },
            None => {
                let key = match key {
                    Cow::Owned(k) => k,
                    Cow::Borrowed(k) => spare::copy(k),
                };
                r.attrs.push((key, value));
                return;
            }
        },
    };
    spare::give(std::mem::replace(slot, value));
    if let Cow::Owned(k) = key {
        spare::give(k);
    }
}

fn remove(r: &mut Record, key: &str) {
    if let Some(i) = r.attrs.iter().position(|(k, _)| k == key) {
        let (k, v) = r.attrs.remove(i);
        spare::give(k);
        spare::give(v);
    }
}

/// A status as Datadog's status remapper reads it: syslog's 0–7, or a word by its start
/// (`emerg`/`f…` fatal, `a…` alert, `c…` critical, `e…`/`err` error, `w…` warning, `n…` notice,
/// `i…` info, `d…`/`trace`/`verbose` debug, `o…`/`s…`/`ok`/`success` ok); anything else info.
pub fn status_of(v: &str) -> u8 {
    let t = v.trim();
    if let Ok(n) = t.parse::<u8>()
        && n <= 7
    {
        return match n {
            0..=2 => 21,
            3 => 17,
            4 => 13,
            5 => 10,
            6 => 9,
            _ => 5,
        };
    }
    if let Some(r) = severities(t) {
        return *r.start();
    }
    let l = t.to_ascii_lowercase();
    if l.starts_with("emerg") || l.starts_with('f') {
        24
    } else if l.starts_with('a') {
        23
    } else if l.starts_with('c') {
        21
    } else if l.starts_with('e') {
        17
    } else if l.starts_with('w') {
        13
    } else if l.starts_with('n') {
        10
    } else if l.starts_with('d') || l.starts_with("trace") || l.starts_with("verbose") {
        5
    } else {
        9
    }
}

/// A time as a date remapper reads it: RFC 3339, or seconds, milliseconds, microseconds or
/// nanoseconds since the epoch (told apart by their size), or `yyyy-MM-dd HH:mm:ss[.fff]` (UTC).
pub fn time_of(v: &str) -> Option<i64> {
    let t = v.trim();
    if let Ok(i) = t.parse::<i64>() {
        return match i {
            i64::MIN..=0 => None,
            1..100_000_000_000 => i.checked_mul(1_000_000_000),
            100_000_000_000..100_000_000_000_000 => i.checked_mul(1_000_000),
            100_000_000_000_000..100_000_000_000_000_000 => i.checked_mul(1_000),
            _ => Some(i),
        };
    }
    if let Ok(f) = t.parse::<f64>()
        && f.is_finite()
        && f > 0.0
    {
        let ns = if f < 1e11 {
            f * 1e9
        } else if f < 1e14 {
            f * 1e6
        } else if f < 1e17 {
            f * 1e3
        } else {
            f
        };
        return (ns < 9.2e18).then_some(ns as i64);
    }
    if let Ok(d) = time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339) {
        return i64::try_from(d.unix_timestamp_nanos()).ok();
    }
    let fmt = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    let (whole, frac) = t.split_once(['.', ',']).unwrap_or((t, ""));
    let d = time::PrimitiveDateTime::parse(whole, &fmt).ok()?;
    let mut ns = i64::try_from(d.assume_utc().unix_timestamp_nanos()).ok()?;
    if !frac.is_empty() && frac.len() <= 9 && frac.bytes().all(|b| b.is_ascii_digit()) {
        ns += frac.parse::<i64>().ok()? * 10i64.pow(9 - u32::try_from(frac.len()).ok()?);
    }
    Some(ns)
}

fn trace_id_of(v: &str) -> Option<[u8; 16]> {
    let t = v.trim();
    let mut out = [0u8; 16];
    if t.len() == 32 && hex_into(t, &mut out) {
        return out.iter().any(|&b| b != 0).then_some(out);
    }
    // Datadog's 64-bit ids, in decimal: the low half of a 128-bit id.
    let n: u64 = t.parse().ok()?;
    out.get_mut(8..)?.copy_from_slice(&n.to_be_bytes());
    (n != 0).then_some(out)
}

fn hex_into(s: &str, out: &mut [u8]) -> bool {
    let b = s.as_bytes();
    if b.len() != out.len() * 2 {
        return false;
    }
    for (i, o) in out.iter_mut().enumerate() {
        let Some(h) = b.get(i * 2..i * 2 + 2).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) else { return false };
        *o = h;
    }
    true
}

/// What applying pipelines keeps between records.
#[derive(Default)]
pub struct Scratch {
    search: RowScratch,
    attrs: Vec<(String, String)>,
    grok: grok::Reading,
    /// A URL's parts, and a user agent's, as each parser reads them.
    url: Vec<(Cow<'static, str>, String)>,
    agent: Vec<(&'static str, String)>,
}

impl Pipeline {
    /// Whether it would read what has these fields (a line's, before a record is made of it): its
    /// filter matches them, or it has none.
    pub fn reads<F: kiln_obs::logs::search::Fields>(&self, r: &F, s: &mut Scratch) -> bool {
        self.filter.as_ref().is_none_or(|f| f.fields(r, &mut s.search))
    }

    /// Its processors run on `r` if its filter matches it: whether it did.
    pub fn apply(&self, r: &mut Record, now_ns: i64, s: &mut Scratch) -> bool {
        if let Some(f) = &self.filter
            && !f.record(r, &mut s.search)
        {
            return false;
        }
        for p in &self.procs {
            run(p, r, now_ns, s);
        }
        true
    }
}

fn run(p: &Proc, r: &mut Record, now_ns: i64, s: &mut Scratch) {
    match p {
        Proc::Grok { source, grok } => {
            s.attrs.clear();
            // Read where it is: what a rule extracts is its own, every attribute of it read before
            // any is set (one may be the attribute it read).
            let Some(value) = get(r, source) else { return };
            if grok.parse_with(value, now_ns, &mut s.grok, &mut s.attrs).is_some() {
                r.attrs.reserve(s.attrs.len());
                for (k, v) in s.attrs.drain(..) {
                    put(r, Cow::Owned(k), v);
                }
            }
        }
        Proc::Date(sources) => {
            if let Some(ts) = first(r, sources).and_then(|(_, v)| time_of(v)) {
                r.ts = ts;
            }
        }
        Proc::Status(sources) => {
            if let Some((_, v)) = first(r, sources) {
                r.severity = status_of(v);
            }
        }
        Proc::Service(sources) => {
            if let Some((_, v)) = first(r, sources) {
                let v = spare::copy(v);
                spare::give(std::mem::replace(&mut r.service, v));
            }
        }
        Proc::Message(sources) => {
            if let Some((_, v)) = first(r, sources) {
                let v = spare::copy(v);
                spare::give(std::mem::replace(&mut r.message, v));
            }
        }
        Proc::TraceId(sources) => {
            if let Some(id) = first(r, sources).and_then(|(_, v)| trace_id_of(v)) {
                r.trace_id = Some(id);
            }
        }
        Proc::Attr { sources, target, format, preserve, overwrite } => {
            let Some((i, v)) = first(r, sources) else { return };
            if !overwrite && get(r, target).is_some() {
                return;
            }
            let value = match format {
                Format::Auto | Format::String => Some(spare::copy(v)),
                Format::Integer => v.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| grok::int_text(f as i64)),
                Format::Double => v.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| {
                    let mut s = spare::take();
                    let _ = write!(s, "{f}");
                    s
                }),
            };
            let source = sources.get(i).map_or("", String::as_str);
            if let Some(value) = value {
                set(r, target, value);
                if !preserve && source != target {
                    remove(r, source);
                }
            }
        }
        Proc::Url { sources, target, trim_slashes } => {
            let Some((_, v)) = first(r, sources) else { return };
            s.url.clear();
            grok::url_parts(v, &mut s.url);
            if *trim_slashes {
                for (k, v) in &mut s.url {
                    if k == "path" && v.len() > 1 {
                        let kept = v.trim_end_matches('/').len();
                        v.truncate(kept);
                    }
                }
            }
            r.attrs.reserve(s.url.len());
            for (k, v) in s.url.drain(..) {
                put(r, Cow::Owned(grok::dotted(target, &k)), v);
                if let Cow::Owned(k) = k {
                    spare::give(k);
                }
            }
        }
        Proc::UserAgent { sources, target, encoded } => {
            // Read where it is (decoded first when it says): its parts are their own strings.
            s.agent.clear();
            match first(r, sources) {
                Some((_, v)) if *encoded => {
                    let decoded = grok::decode_uri(v);
                    grok::user_agent(&decoded, &mut s.agent);
                    spare::give(decoded);
                }
                Some((_, v)) => grok::user_agent(v, &mut s.agent),
                None => return,
            }
            r.attrs.reserve(s.agent.len());
            for (k, v) in s.agent.drain(..) {
                put(r, Cow::Owned(grok::dotted(target, k)), v);
            }
        }
        Proc::Category { target, categories } => {
            if let Some((_, name)) = categories.iter().find(|(f, _)| f.record(r, &mut s.search)) {
                set(r, target, spare::copy(name));
            }
        }
        Proc::Arithmetic { expr, target, replace_missing } => {
            if let Some(v) = expr.eval(&|k| get(r, k).and_then(|v| v.trim().parse::<f64>().ok()), *replace_missing)
                && v.is_finite()
            {
                let text = if v.fract() == 0.0 && v.abs() < 9.0e15 {
                    grok::int_text(v as i64)
                } else {
                    let mut s = spare::take();
                    let _ = write!(s, "{v}");
                    s
                };
                set(r, target, text);
            }
        }
        Proc::Builder { template, target, replace_missing } => {
            let mut out = spare::take();
            for piece in template {
                match piece {
                    Piece::Text(t) => out.push_str(t),
                    Piece::Attr(k) => match get(r, k) {
                        Some(v) => out.push_str(v),
                        None if *replace_missing => {}
                        None => return spare::give(out),
                    },
                }
            }
            set(r, target, out);
        }
        Proc::Lookup { source, target, table, default } => {
            let Some(v) = get(r, source) else { return };
            if let Some(found) = table.get(v.trim()).or(default.as_ref()) {
                let found = spare::copy(found);
                set(r, target, found);
            }
        }
        Proc::Nested(p) => {
            p.apply(r, now_ns, s);
        }
    }
}

/// Every enabled pipeline, in order, on `r`.
pub fn apply_all(pipelines: &[Pipeline], r: &mut Record, now_ns: i64, s: &mut Scratch) {
    for p in pipelines {
        p.apply(r, now_ns, s);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn rec(message: &str) -> Record {
        Record { ts: 1, severity: 9, service: "web".into(), host: "h".into(), source: "nginx".into(), message: message.into(), ..Record::default() }
    }

    fn attr<'a>(r: &'a Record, k: &str) -> Option<&'a str> {
        get(r, k)
    }

    /// A pipeline as Datadog exports one reads here, and does what it says: its parser's
    /// attributes, then its remappers, category, arithmetic, builder and lookup.
    #[test]
    fn a_pipeline_exported_from_datadog_reads_and_runs() {
        let json = r#"{
          "name": "nginx", "is_enabled": true, "filter": {"query": "source:nginx"},
          "processors": [
            {"type": "grok-parser", "name": "access", "is_enabled": true, "source": "message", "samples": [],
             "grok": {"support_rules": "", "match_rules": "access %{ipOrHost:network.client.ip} \\[%{date(\"dd/MMM/yyyy:HH:mm:ss Z\"):date_access}\\] \"%{word:http.method} %{notSpace:http.url}\" %{integer:http.status_code} %{number:duration} lvl=%{word:level} trace=%{notSpace:trace}"}},
            {"type": "date-remapper", "name": "time", "sources": ["date_access"]},
            {"type": "status-remapper", "name": "level", "sources": ["level"]},
            {"type": "category-processor", "name": "class", "target": "http.status_category", "categories": [
               {"filter": {"query": "@http.status_code:[500 TO 599]"}, "name": "error"},
               {"filter": {"query": "@http.status_code:[400 TO 499]"}, "name": "warning"},
               {"filter": {"query": "*"}, "name": "ok"}]},
            {"type": "arithmetic-processor", "name": "ms", "expression": "duration * 1000", "target": "duration_ms", "is_replace_missing": false},
            {"type": "string-builder-processor", "name": "route", "template": "%{http.method} %{http.url_details.path}", "target": "route", "is_replace_missing": false},
            {"type": "url-parser", "name": "url", "sources": ["http.url"], "target": "http.url_details"},
            {"type": "string-builder-processor", "name": "route2", "template": "%{http.method} %{http.url_details.path}", "target": "route", "is_replace_missing": false},
            {"type": "lookup-processor", "name": "team", "source": "http.url_details.path", "target": "team", "lookup_table": ["/pay,payments", "/cart,checkout"], "default_lookup": "web"},
            {"type": "trace-id-remapper", "name": "trace", "sources": ["trace"]},
            {"type": "attribute-remapper", "name": "client", "sources": ["network.client.ip"], "target": "client", "preserve_source": false, "override_on_conflict": false},
            {"type": "service-remapper", "name": "svc", "sources": ["team"]},
            {"type": "pipeline", "name": "inner", "filter": {"query": "@http.status_code:>=500"}, "processors": [
               {"type": "string-builder-processor", "template": "page", "target": "alert"}]}
          ]
        }"#;
        let def: PipelineDef = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::from_str::<PipelineDef>(&serde_json::to_string(&def).unwrap()).unwrap(), def, "it writes back as it read");
        let p = def.compile().unwrap();
        let mut r = rec(r#"10.0.0.7 [06/Jan/2017:16:16:37 +0000] "GET /pay?x=1" 503 0.25 lvl=ERROR trace=0af7651916cd43dd8448eb211c80319c"#);
        assert!(p.apply(&mut r, 0, &mut Scratch::default()));
        assert_eq!(r.ts, 1_483_719_397_000_000_000);
        assert_eq!(r.severity, 17);
        for (k, v) in [("http.method", "GET"), ("http.status_code", "503"), ("http.status_category", "error"), ("duration_ms", "250"), ("route", "GET /pay"), ("http.url_details.path", "/pay"), ("http.url_details.queryString.x", "1"), ("team", "payments"), ("client", "10.0.0.7"), ("alert", "page")] {
            assert_eq!(attr(&r, k), Some(v), "{k}: {:?}", r.attrs);
        }
        assert_eq!(attr(&r, "network.client.ip"), None, "remapped, not preserved");
        assert!(r.trace_id.is_some() && r.service == "payments");
        // Another source: not matched, untouched.
        let mut other = Record { source: "apache".into(), ..rec("x") };
        assert!(!p.apply(&mut other, 0, &mut Scratch::default()) && other.attrs.is_empty());
    }

    #[test]
    fn statuses_and_times_read_as_datadog_s_remappers_read_them() {
        for (v, s) in [("0", 21), ("3", 17), ("4", 13), ("6", 9), ("7", 5), ("ERROR", 17), ("warn", 13), ("Fatal", 21), ("crit", 21), ("alert", 21), ("notice", 9), ("debug", 5), ("verbose", 5), ("OK", 9), ("xyzzy", 9), ("whatever", 13)] {
            assert_eq!(status_of(v), s, "{v}");
        }
        for (v, ns) in [("1792324800", 1_792_324_800_000_000_000i64), ("1792324800500", 1_792_324_800_500_000_000), ("2026-10-18T12:00:00.5Z", 1_792_324_800_500_000_000), ("2026-10-18 12:00:00.250", 1_792_324_800_250_000_000), ("1792324800.5", 1_792_324_800_500_000_000)] {
            assert_eq!(time_of(v), Some(ns), "{v}");
        }
        assert_eq!(time_of("yesterday"), None);
    }

    #[test]
    fn what_is_wrong_is_named() {
        for json in [
            r#"{"name":"x","processors":[{"type":"grok-parser","grok":{"match_rules":"r %{nonesuch:a}"}}]}"#,
            r#"{"name":"x","processors":[{"type":"arithmetic-processor","expression":"a +","target":"t"}]}"#,
            r#"{"name":"x","processors":[{"type":"lookup-processor","source":"a","target":"t","lookup_table":["no comma"]}]}"#,
            r#"{"name":"x","filter":{"query":"service:("}}"#,
            r#"{"name":"x","processors":[{"type":"date-remapper","sources":[]}]}"#,
            r#"{"name":"x","processors":[{"type":"pipeline","processors":[{"type":"pipeline","processors":[]}]}]}"#,
        ] {
            let def: PipelineDef = serde_json::from_str(json).unwrap();
            assert!(def.compile().is_err(), "{json}");
        }
        // A disabled processor is not compiled, so not checked either: it does nothing.
        let def: PipelineDef = serde_json::from_str(r#"{"name":"x","processors":[{"type":"arithmetic-processor","is_enabled":false,"expression":"(","target":"t"}]}"#).unwrap();
        assert!(def.compile().is_ok());
    }
}
