//! What a model's call costs (`docs/observe.md` §8.12): pydantic's genai-prices catalog (MIT, its
//! licence beside it; the catalog Datadog's LLM Observability names for its estimates), a snapshot of
//! its v2 `data_slim.json` kept in kiln, and its own rules for reading it, as its Python package reads
//! it (held to that package's answers by a fixture of them):
//!
//! - **A provider** by its id (the catalog's own, else the first whose `provider_match` takes it:
//!   OpenTelemetry's `aws.bedrock`, `gcp.vertex_ai` and the like), else by its API's address (the
//!   first `api_pattern` matching from its start), else by the model's name (the first
//!   `model_match`). LiteLLM's `litellm` with a model of `provider/model` names its provider so.
//! - **A model** of the provider: the first whose rule takes the name lower-cased (equal, a prefix, a
//!   suffix, a part, a pattern searched for; any or all of a list); else a compact date in it
//!   (`-20251211`) read as a dashed one (`-2025-12-11`); else the first of a provider it falls back
//!   to (one step: Azure's OpenAI models are OpenAI's).
//! - **Its prices** at the call's time: of a list of prices, the last whose constraint holds (a date
//!   it starts, a time of day it holds), else the first; then a variant's (a batch's, a service
//!   tier's) where the call's context is the variant's, price by price, the first variant giving
//!   each (none for a model found through a fallback).
//! - **The cost**: each priced unit (59 of them, by their dimensions: tokens by direction, modality,
//!   cache and its time to live, reasoning, citations; characters, seconds, pixels, pages, searches,
//!   requests) billed on what is its own, its count less its priced descendants' (cache reads and
//!   writes out of the input, billed at theirs); a tiered price at its tier past the call's input
//!   tokens, every token at it (the catalog's cliff); a unit not said and not to be inferred is 0, one
//!   that would hide a said one refused, as the package refuses it.
//!
//! Prices are read as `f64`, the package's as decimals: kiln's cost is its own within 1e-12 of a
//! dollar at the size of a call's (the fixture's tolerance).

use std::borrow::Cow;
use std::sync::{Mutex, OnceLock, PoisonError};

use hashbrown::HashTable;
use regex::Regex;
use serde_json::Value;

use crate::hash::Keyed;

/// The catalog kept in kiln: genai-prices' `prices/new_data/v2/data_slim.json` at commit `b267c2e`
/// (2026-10-23), zstd'd.
const SNAPSHOT: &[u8] = include_bytes!("genai-prices.json.zst");
pub const SNAPSHOT_COMMIT: &str = "b267c2e";
pub const SNAPSHOT_DATE: &str = "2026-10-23";
/// The catalog inflated, at the most.
const SNAPSHOT_MAX: usize = 16 << 20;
/// Lookups (a provider and a model's name) remembered, at the most: past them, forgotten together.
const REMEMBERED: usize = 4_096;

/// A unit priced: its usage's key, its price's key, the units a price is of, its dimensions
/// (genai-prices' `prices/units.yml`).
struct Unit {
    usage: &'static str,
    price: &'static str,
    per: f64,
    dims: &'static [(&'static str, &'static str)],
}

macro_rules! unit {
    ($usage:literal, $price:literal, $per:literal, [$(($k:literal, $v:literal)),* $(,)?]) => {
        Unit { usage: $usage, price: $price, per: $per, dims: &[$(($k, $v)),*] }
    };
}

const UNITS: [Unit; 59] = [
    unit!("input_tokens", "input_mtok", 1e6, [("family", "tokens"), ("direction", "input")]),
    unit!("output_tokens", "output_mtok", 1e6, [("family", "tokens"), ("direction", "output")]),
    unit!("cache_read_tokens", "cache_read_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("token_type", "cache_read")]),
    unit!("cache_write_tokens", "cache_write_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("token_type", "cache_write")]),
    unit!("cache_write_5m_tokens", "cache_write_5m_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("token_type", "cache_write"), ("cache_ttl", "5m")]),
    unit!("cache_write_1h_tokens", "cache_write_1h_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("token_type", "cache_write"), ("cache_ttl", "1h")]),
    unit!("input_text_tokens", "input_text_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text")]),
    unit!("output_text_tokens", "output_text_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "text")]),
    unit!("cache_text_read_tokens", "cache_text_read_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text"), ("token_type", "cache_read")]),
    unit!("cache_text_write_tokens", "cache_text_write_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text"), ("token_type", "cache_write")]),
    unit!("cache_text_write_5m_tokens", "cache_text_write_5m_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text"), ("token_type", "cache_write"), ("cache_ttl", "5m")]),
    unit!("cache_text_write_1h_tokens", "cache_text_write_1h_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text"), ("token_type", "cache_write"), ("cache_ttl", "1h")]),
    unit!("input_audio_tokens", "input_audio_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio")]),
    unit!("output_audio_tokens", "output_audio_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "audio")]),
    unit!("cache_audio_read_tokens", "cache_audio_read_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio"), ("token_type", "cache_read")]),
    unit!("cache_audio_write_tokens", "cache_audio_write_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio"), ("token_type", "cache_write")]),
    unit!("cache_audio_write_5m_tokens", "cache_audio_write_5m_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio"), ("token_type", "cache_write"), ("cache_ttl", "5m")]),
    unit!("cache_audio_write_1h_tokens", "cache_audio_write_1h_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio"), ("token_type", "cache_write"), ("cache_ttl", "1h")]),
    unit!("input_image_tokens", "input_image_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image")]),
    unit!("output_image_tokens", "output_image_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "image")]),
    unit!("cache_image_read_tokens", "cache_image_read_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image"), ("token_type", "cache_read")]),
    unit!("cache_image_write_tokens", "cache_image_write_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image"), ("token_type", "cache_write")]),
    unit!("cache_image_write_5m_tokens", "cache_image_write_5m_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image"), ("token_type", "cache_write"), ("cache_ttl", "5m")]),
    unit!("cache_image_write_1h_tokens", "cache_image_write_1h_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image"), ("token_type", "cache_write"), ("cache_ttl", "1h")]),
    unit!("input_video_tokens", "input_video_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video")]),
    unit!("output_video_tokens", "output_video_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "video")]),
    unit!("cache_video_read_tokens", "cache_video_read_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video"), ("token_type", "cache_read")]),
    unit!("cache_video_write_tokens", "cache_video_write_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video"), ("token_type", "cache_write")]),
    unit!("cache_video_write_5m_tokens", "cache_video_write_5m_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video"), ("token_type", "cache_write"), ("cache_ttl", "5m")]),
    unit!("cache_video_write_1h_tokens", "cache_video_write_1h_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video"), ("token_type", "cache_write"), ("cache_ttl", "1h")]),
    unit!("input_tool_tokens", "input_tool_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("token_type", "tool")]),
    unit!("input_text_tool_tokens", "input_text_tool_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "text"), ("token_type", "tool")]),
    unit!("input_audio_tool_tokens", "input_audio_tool_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "audio"), ("token_type", "tool")]),
    unit!("input_image_tool_tokens", "input_image_tool_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "image"), ("token_type", "tool")]),
    unit!("input_video_tool_tokens", "input_video_tool_mtok", 1e6, [("family", "tokens"), ("direction", "input"), ("modality", "video"), ("token_type", "tool")]),
    unit!("output_reasoning_tokens", "output_reasoning_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("token_type", "reasoning")]),
    unit!("output_text_reasoning_tokens", "output_text_reasoning_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "text"), ("token_type", "reasoning")]),
    unit!("output_audio_reasoning_tokens", "output_audio_reasoning_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "audio"), ("token_type", "reasoning")]),
    unit!("output_image_reasoning_tokens", "output_image_reasoning_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "image"), ("token_type", "reasoning")]),
    unit!("output_video_reasoning_tokens", "output_video_reasoning_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "video"), ("token_type", "reasoning")]),
    unit!("output_citation_tokens", "output_citation_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("token_type", "citation")]),
    unit!("output_text_citation_tokens", "output_text_citation_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "text"), ("token_type", "citation")]),
    unit!("output_audio_citation_tokens", "output_audio_citation_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "audio"), ("token_type", "citation")]),
    unit!("output_image_citation_tokens", "output_image_citation_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "image"), ("token_type", "citation")]),
    unit!("output_video_citation_tokens", "output_video_citation_mtok", 1e6, [("family", "tokens"), ("direction", "output"), ("modality", "video"), ("token_type", "citation")]),
    unit!("input_characters", "input_mchars", 1e6, [("family", "characters"), ("direction", "input")]),
    unit!("input_text_messages", "input_text_messages_kcount", 1e3, [("family", "messages"), ("direction", "input"), ("modality", "text")]),
    unit!("audio_seconds", "audio_hours", 3600.0, [("family", "durations"), ("modality", "audio")]),
    unit!("input_audio_seconds", "input_audio_hours", 3600.0, [("family", "durations"), ("direction", "input"), ("modality", "audio")]),
    unit!("output_audio_seconds", "output_audio_hours", 3600.0, [("family", "durations"), ("direction", "output"), ("modality", "audio")]),
    unit!("input_pixels", "input_gpixels", 1e9, [("family", "pixels"), ("direction", "input")]),
    unit!("input_document_pages", "input_document_kpages", 1e3, [("family", "document_pages"), ("direction", "input")]),
    unit!("input_annotated_document_pages", "input_annotated_document_kpages", 1e3, [("family", "document_pages"), ("direction", "input"), ("page_type", "annotated")]),
    unit!("rerank_searches", "rerank_searches_kcount", 1e3, [("family", "rerank")]),
    unit!("web_searches", "web_searches_kcount", 1e3, [("family", "tool_calls"), ("tool_type", "web_search")]),
    unit!("social_searches", "social_searches_kcount", 1e3, [("family", "tool_calls"), ("tool_type", "social_search")]),
    unit!("storage_searches", "storage_searches_kcount", 1e3, [("family", "tool_calls"), ("tool_type", "storage_search")]),
    unit!("code_executions", "code_executions_kcount", 1e3, [("family", "tool_calls"), ("tool_type", "code_execution")]),
    unit!("requests", "requests_kcount", 1e3, [("family", "requests")]),
];

/// The units by place: their usage's.
pub const INPUT_TOKENS: usize = 0;
pub const OUTPUT_TOKENS: usize = 1;
pub const CACHE_READ_TOKENS: usize = 2;
pub const CACHE_WRITE_TOKENS: usize = 3;
pub const CACHE_WRITE_5M_TOKENS: usize = 4;
pub const CACHE_WRITE_1H_TOKENS: usize = 5;
pub const OUTPUT_REASONING_TOKENS: usize = 35;
const REQUESTS: usize = 58;

/// The place of the unit of usage key `key`.
pub fn unit_of(key: &str) -> Option<usize> {
    UNITS.iter().position(|u| u.usage == key)
}

fn unit_priced(key: &str) -> Option<usize> {
    UNITS.iter().position(|u| u.price == key)
}

/// Whether `a`'s dimensions are among `b`'s: `a` an ancestor of `b`, or `b` itself.
fn under(a: &Unit, b: &Unit) -> bool {
    a.dims.iter().all(|d| b.dims.contains(d))
}

/// Whether `a` and `b` say nothing of a dimension that the other says otherwise.
fn compatible(a: &Unit, b: &Unit) -> bool {
    a.dims.iter().all(|(k, v)| b.dims.iter().find(|(bk, _)| bk == k).is_none_or(|(_, bv)| bv == v))
}

fn dim(u: &Unit, key: &str) -> Option<&'static str> {
    u.dims.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// What a call used: each unit's count where it was said (`said`: by place, a bit each).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    values: [f64; 59],
    said: u64,
}

impl Default for Usage {
    fn default() -> Self {
        Self { values: [0.0; 59], said: 0 }
    }
}

impl Usage {
    /// Unit `unit`'s count said (finite and not negative, else not said).
    pub fn set(&mut self, unit: usize, count: f64) -> &mut Self {
        if let Some(v) = self.values.get_mut(unit)
            && count.is_finite()
            && count >= 0.0
        {
            *v = count;
            self.said |= 1 << unit;
        }
        self
    }

    fn said(&self, unit: usize) -> Option<f64> {
        (self.said & (1 << unit) != 0).then(|| self.values.get(unit).copied().unwrap_or(0.0))
    }

    /// Unit `unit`'s count: said, or 0 where nothing said stands for it; none where a unit said
    /// would be hidden (one under it said, or two said that make it up), as genai-prices refuses.
    fn value(&self, unit: usize) -> Option<f64> {
        if let Some(v) = self.said(unit) {
            return Some(v);
        }
        let want = UNITS.get(unit)?;
        // The units said, of counts above 0, a bit each.
        let positive = (0..UNITS.len()).filter(|&i| self.said(i).is_some_and(|v| v > 0.0)).fold(0u64, |b, i| b | 1 << i);
        let units = |bits: u64| (0..UNITS.len()).filter(move |&i| bits & (1 << i) != 0);
        if units(positive).any(|i| i != unit && UNITS.get(i).is_some_and(|u| under(want, u))) {
            return None;
        }
        // Two said, of no dimension at odds, neither under the other, that together are it.
        for l in units(positive) {
            for r in units(positive & !((2u64 << l) - 1)) {
                let (Some(lu), Some(ru)) = (UNITS.get(l), UNITS.get(r)) else { continue };
                if !compatible(lu, ru) || under(lu, ru) || under(ru, lu) {
                    continue;
                }
                let joined = |(k, v): &(&str, &str)| lu.dims.iter().chain(ru.dims).any(|d| d.0 == *k && d.1 == *v);
                if want.dims.iter().all(joined) && lu.dims.iter().chain(ru.dims).all(|d| want.dims.contains(d)) {
                    return None;
                }
            }
        }
        Some(0.0)
    }
}

/// A price: one for every unit, or tiered by the call's input tokens (each tier's from its start,
/// past it: every unit at it).
#[derive(Debug, Clone, PartialEq)]
enum Price {
    Flat(f64),
    Tiered { base: f64, tiers: Vec<(f64, f64)> },
}

impl Price {
    fn at(&self, input_tokens: f64) -> f64 {
        match self {
            Self::Flat(p) => *p,
            Self::Tiered { base, tiers } => tiers.iter().rev().find(|(start, _)| input_tokens > *start).map_or(*base, |&(_, p)| p),
        }
    }

    fn of(v: &Value) -> Option<Self> {
        if let Some(p) = v.as_f64() {
            return (p.is_finite() && p >= 0.0).then_some(Self::Flat(p));
        }
        let base = v.get("base")?.as_f64()?;
        let mut tiers = Vec::new();
        for t in v.get("tiers")?.as_array()? {
            tiers.push((t.get("start")?.as_f64()?, t.get("price")?.as_f64()?));
        }
        tiers.sort_by(|a, b| a.0.total_cmp(&b.0));
        Some(Self::Tiered { base, tiers })
    }
}

/// A model's prices: a unit's price each.
type Prices = Vec<(usize, Price)>;

fn prices_of(v: &Value) -> Prices {
    let mut out = Prices::new();
    for (key, price) in v.as_object().into_iter().flatten() {
        if let (Some(u), Some(p)) = (unit_priced(key), Price::of(price)) {
            out.push((u, p));
        }
    }
    out
}

/// When a price holds: from a date (UTC), or a time of day (seconds, UTC; past midnight where it
/// ends before it starts).
#[derive(Debug, Clone, PartialEq)]
enum When {
    From(i64),
    Daily(i64, i64),
}

impl When {
    fn of(v: &Value) -> Option<Self> {
        if let Some(d) = v.get("start_date").and_then(Value::as_str) {
            return day_of(d).map(Self::From);
        }
        let t = |k: &str| v.get(k).and_then(Value::as_str).and_then(seconds_of);
        Some(Self::Daily(t("start_time")?, t("end_time")?))
    }

    fn holds(&self, at_ns: i64) -> bool {
        let secs = at_ns.div_euclid(1_000_000_000);
        match *self {
            Self::From(day) => secs.div_euclid(86_400) >= day,
            Self::Daily(start, end) => {
                let t = secs.rem_euclid(86_400);
                if end < start { t >= start || t < end } else { start <= t && t < end }
            }
        }
    }
}

/// Days since the epoch of a date `YYYY-MM-DD`.
fn day_of(s: &str) -> Option<i64> {
    let mut it = s.trim().splitn(3, '-');
    let (y, m, d) = (it.next()?.parse::<i64>().ok()?, it.next()?.parse::<i64>().ok()?, it.next()?.get(..2)?.parse::<i64>().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Seconds of the day of a time `HH:MM[:SS][Z|+HH:MM|-HH:MM]`, in UTC.
fn seconds_of(s: &str) -> Option<i64> {
    let s = s.trim();
    let (clock, offset) = match s.find(['Z', '+', '-']) {
        Some(i) => (s.get(..i)?, s.get(i..)?),
        None => (s, ""),
    };
    let mut parts = clock.split(':');
    let (h, m) = (parts.next()?.parse::<i64>().ok()?, parts.next()?.parse::<i64>().ok()?);
    let sec = parts.next().map_or(Some(0.0), |x| x.parse::<f64>().ok())? as i64;
    let local = h * 3600 + m * 60 + sec;
    let shift = match offset.as_bytes().first() {
        None | Some(b'Z') => 0,
        Some(&sign) => {
            let mut o = offset.get(1..)?.split(':');
            let (oh, om) = (o.next()?.parse::<i64>().ok()?, o.next().map_or(Some(0), |x| x.parse::<i64>().ok())?);
            let off = oh * 3600 + om * 60;
            if sign == b'+' { off } else { -off }
        }
    };
    Some((local - shift).rem_euclid(86_400))
}

/// A rule a name is matched by (lower-cased, as its text is).
#[derive(Debug)]
enum Rule {
    Equals(String),
    Starts(String),
    Ends(String),
    Contains(String),
    Search(Regex),
    Not(Box<Rule>),
    Any(Vec<Rule>),
    All(Vec<Rule>),
    /// A pattern that does not compile here: it takes nothing.
    Never,
}

impl Rule {
    fn of(v: &Value) -> Self {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_lowercase);
        if let Some(s) = text("equals") {
            return Self::Equals(s);
        }
        if let Some(s) = text("starts_with") {
            return Self::Starts(s);
        }
        if let Some(s) = text("ends_with") {
            return Self::Ends(s);
        }
        if let Some(s) = text("contains") {
            return Self::Contains(s);
        }
        if let Some(p) = v.get("regex").and_then(Value::as_str) {
            return search(p);
        }
        if let Some(list) = v.get("or").and_then(Value::as_array) {
            return Self::Any(list.iter().map(Self::of).collect());
        }
        if let Some(list) = v.get("and").and_then(Value::as_array) {
            return Self::All(list.iter().map(Self::of).collect());
        }
        Self::Never
    }

    fn takes(&self, s: &str) -> bool {
        match self {
            Self::Equals(x) => s == x,
            Self::Starts(x) => s.starts_with(x.as_str()),
            Self::Ends(x) => s.ends_with(x.as_str()),
            Self::Contains(x) => s.contains(x.as_str()),
            Self::Search(re) => re.is_match(s),
            Self::Not(r) => !r.takes(s),
            Self::Any(rs) => rs.iter().any(|r| r.takes(s)),
            Self::All(rs) => rs.iter().all(|r| r.takes(s)),
            Self::Never => false,
        }
    }
}

/// A pattern searched for, as Python's `re.search` does: one that begins with a lookahead the regex
/// crate has none of (`^(?!A)B`) read as its meaning (not `^A`, and `^B`).
fn search(p: &str) -> Rule {
    if let Some(rest) = p.strip_prefix("^(?!")
        && let Some(close) = group_end(rest)
    {
        let (inside, after) = (rest.get(..close).unwrap_or(""), rest.get(close + 1..).unwrap_or(""));
        let not = match Regex::new(&format!("^(?:{inside})")) {
            Ok(re) => Rule::Not(Box::new(Rule::Search(re))),
            Err(_) => return Rule::Never,
        };
        if after.is_empty() {
            return not;
        }
        return match Regex::new(&format!("^(?:{after})")) {
            Ok(re) => Rule::All(vec![not, Rule::Search(re)]),
            Err(_) => Rule::Never,
        };
    }
    Regex::new(p).map_or(Rule::Never, Rule::Search)
}

/// Where the group a pattern's text begins inside ends (its `)`), escapes and classes passed over.
fn group_end(p: &str) -> Option<usize> {
    let (mut depth, mut class, mut escaped) = (0usize, false, false);
    for (i, c) in p.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '[' if !class => class = true,
            ']' if class => class = false,
            '(' if !class => depth += 1,
            ')' if !class => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// A variant's prices: where a call's context says its every pair, from a time where said.
#[derive(Debug)]
struct Variant {
    when: Vec<(String, String)>,
    prices: Prices,
    from: Option<When>,
}

#[derive(Debug)]
struct Model {
    id: String,
    rule: Rule,
    /// Its prices, each from a time (the last that holds wins; none holding, the first).
    prices: Vec<(Option<When>, Prices)>,
    variants: Vec<Variant>,
}

#[derive(Debug)]
struct Provider {
    id: String,
    api: Option<Regex>,
    provider_match: Option<Rule>,
    model_match: Option<Rule>,
    fallback: Vec<String>,
    models: Vec<Model>,
}

/// What a call is priced by: its provider (as a span names it), its API's address, its model's
/// name, its time, its context (`processing: batch`, `service_tier: flex`, ...).
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub provider: Option<&'a str>,
    pub api_url: Option<&'a str>,
    pub model: &'a str,
    pub at_ns: i64,
    pub context: &'a [(&'a str, &'a str)],
}

/// A call priced (USD): its input's, its output's, all of it, and the parts Datadog shows (the input
/// read from a cache, written to one, the rest of it; the output's reasoning); the catalog's
/// provider and model it was priced as.
#[derive(Debug, Clone, PartialEq)]
pub struct Priced<'c> {
    pub provider: &'c str,
    pub model: &'c str,
    pub input: f64,
    pub output: f64,
    pub total: f64,
    pub non_cached_input: f64,
    pub cache_read_input: f64,
    pub cache_write_input: f64,
    pub reasoning_output: f64,
}

/// What a call's units cost (USD), by the parts of [`Priced`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Parts {
    input: f64,
    output: f64,
    total: f64,
    non_cached_input: f64,
    cache_read_input: f64,
    cache_write_input: f64,
    reasoning_output: f64,
}

/// Why a call has no price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unpriced {
    #[error("no provider in the catalog is its")]
    Provider,
    #[error("the catalog holds no such model of its provider")]
    Model,
    #[error("its usage does not say what its prices need")]
    Usage,
}

/// Where a call's model is: (the provider it was asked of, the model's provider, its place, by a
/// fallback); or why it has none.
type Found = Result<(usize, usize, usize, bool), Unpriced>;

/// Lookups remembered: a call's provider, API address and model's name, and where its model is,
/// found by their hash (a lookup remembered allocates nothing).
#[derive(Debug, Default)]
struct Remembered {
    keyed: Keyed,
    table: HashTable<(String, String, String, Found)>,
}

fn lookup_hash(k: &Keyed, provider: &str, api: &str, model: &str) -> u64 {
    k.word(k.bytes(provider.as_bytes()) ^ k.word(k.bytes(api.as_bytes()) ^ k.word(k.bytes(model.as_bytes()))))
}

/// The catalog: providers, their models and prices.
#[derive(Debug, Default)]
pub struct Catalog {
    providers: Vec<Provider>,
    found: Mutex<Remembered>,
}

/// The catalog kept in kiln, read once (an empty one where it does not read).
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut json = Vec::new();
        match crate::zstd::decompress(SNAPSHOT, &mut json, SNAPSHOT_MAX) {
            Ok(()) => Catalog::read(&json).unwrap_or_default(),
            Err(_) => Catalog::default(),
        }
    })
}

impl Catalog {
    /// A catalog of genai-prices' v2 JSON (an array of providers).
    pub fn read(json: &[u8]) -> Option<Self> {
        let v: Value = serde_json::from_slice(json).ok()?;
        let mut providers = Vec::new();
        for p in v.as_array()? {
            let id = p.get("id")?.as_str()?.to_owned();
            let api = p.get("api_pattern").and_then(Value::as_str).and_then(|a| Regex::new(&format!("^(?:{a})")).ok());
            let fallback = p.get("fallback_model_providers").and_then(Value::as_array).map(|f| f.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            let mut models = Vec::new();
            for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
                let Some(mid) = m.get("id").and_then(Value::as_str) else { continue };
                let rule = m.get("match").map_or(Rule::Never, Rule::of);
                let prices = match m.get("prices") {
                    Some(Value::Array(list)) => list.iter().map(|c| (c.get("constraint").and_then(When::of), c.get("prices").map(prices_of).unwrap_or_default())).collect(),
                    Some(one) => vec![(None, prices_of(one))],
                    None => Vec::new(),
                };
                let variants = m
                    .get("price_variants")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|x| Variant {
                        when: x.get("when").and_then(Value::as_object).into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect(),
                        prices: x.get("prices").map(prices_of).unwrap_or_default(),
                        from: x.get("constraint").and_then(When::of),
                    })
                    .collect();
                models.push(Model { id: mid.to_owned(), rule, prices, variants });
            }
            providers.push(Provider { id, api, provider_match: p.get("provider_match").map(Rule::of), model_match: p.get("model_match").map(Rule::of), fallback, models });
        }
        Some(Self { providers, found: Mutex::new(Remembered::default()) })
    }

    /// Providers and models it holds.
    pub fn size(&self) -> (usize, usize) {
        (self.providers.len(), self.providers.iter().map(|p| p.models.len()).sum())
    }

    fn provider(&self, id: Option<&str>, api_url: Option<&str>, model: &str) -> Option<usize> {
        if let Some(id) = id {
            let id = id.trim().to_lowercase();
            if let Some(i) = self.providers.iter().position(|p| p.id == id) {
                return Some(i);
            }
            if let Some(i) = self.providers.iter().position(|p| p.provider_match.as_ref().is_some_and(|r| r.takes(&id))) {
                return Some(i);
            }
            if id != "litellm" {
                return None;
            }
        }
        if let Some(url) = api_url {
            return self.providers.iter().position(|p| p.api.as_ref().is_some_and(|re| re.is_match(url)));
        }
        self.providers.iter().position(|p| p.model_match.as_ref().is_some_and(|r| r.takes(model)))
    }

    /// A provider's model of name `model` (lower-cased): (its provider, its place, by a fallback).
    fn model(&self, provider: usize, model: &str, fallback: bool) -> Option<(usize, usize, bool)> {
        let p = self.providers.get(provider)?;
        if let Some(i) = p.models.iter().position(|m| m.rule.takes(model)) {
            return Some((provider, i, false));
        }
        if !fallback {
            return None;
        }
        for id in &p.fallback {
            if let Some(f) = self.providers.iter().position(|q| &q.id == id)
                && let Some((f, i, _)) = self.model(f, model, false)
            {
                return Some((f, i, true));
            }
        }
        None
    }

    /// Where a call's model is ([`Found`]), remembered.
    fn find(&self, call: &Call<'_>) -> Found {
        let (p, a, m) = (call.provider.unwrap_or_default(), call.api_url.unwrap_or_default(), call.model);
        {
            let r = self.found.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(e) = r.table.find(lookup_hash(&r.keyed, p, a, m), |e| e.0 == p && e.1 == a && e.2 == m) {
                return e.3;
            }
        }
        let found = self.look_up(call);
        let mut r = self.found.lock().unwrap_or_else(PoisonError::into_inner);
        if r.table.len() >= REMEMBERED {
            r.table.clear();
        }
        let k = r.keyed;
        let h = lookup_hash(&k, p, a, m);
        if r.table.find(h, |e| e.0 == p && e.1 == a && e.2 == m).is_none() {
            r.table.insert_unique(h, (p.to_owned(), a.to_owned(), m.to_owned(), found), |e| lookup_hash(&k, &e.0, &e.1, &e.2));
        }
        found
    }

    fn look_up(&self, call: &Call<'_>) -> Found {
        let mut model = call.model.to_lowercase();
        let mut provider_id = call.provider;
        // LiteLLM's: a model of `provider/model` names its provider.
        let split;
        if call.provider.is_some_and(|p| p.trim().eq_ignore_ascii_case("litellm"))
            && let Some((p, m)) = model.split_once('/')
            && self.provider(Some(p), None, "").is_some()
        {
            split = (p.to_owned(), m.to_owned());
            provider_id = Some(split.0.as_str());
            model = split.1.clone();
        }
        let asked = self.provider(provider_id, call.api_url, &model).ok_or(Unpriced::Provider)?;
        let found = self.model(asked, &model, true).or_else(|| {
            let dashed = dashed_dates(&model);
            (dashed != model).then(|| self.model(asked, &dashed, true)).flatten()
        });
        found.map(|(p, m, fb)| (asked, p, m, fb)).ok_or(Unpriced::Model)
    }

    /// What `call` cost, of `usage`.
    pub fn price(&self, call: &Call<'_>, usage: &Usage) -> Result<Priced<'_>, Unpriced> {
        let (asked, provider, model, by_fallback) = self.find(call)?;
        let m = self.providers.get(provider).and_then(|p| p.models.get(model)).ok_or(Unpriced::Model)?;
        let prices = resolve(m, call.at_ns, if by_fallback { &[] } else { call.context });
        let c = cost(&prices, usage).ok_or(Unpriced::Usage)?;
        let asked_id = self.providers.get(asked).map_or("", |p| p.id.as_str());
        Ok(Priced {
            provider: asked_id,
            model: m.id.as_str(),
            input: c.input,
            output: c.output,
            total: c.total,
            non_cached_input: c.non_cached_input,
            cache_read_input: c.cache_read_input,
            cache_write_input: c.cache_write_input,
            reasoning_output: c.reasoning_output,
        })
    }
}

/// A compact date in a name (`-20251211`) dashed (`-2025-12-11`), where it is a date and is
/// followed by `-`, `:` or the end, every one: LiteLLM's and OpenRouter's names, the catalog's.
fn dashed_dates(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + 4);
    let mut i = 0;
    while i < b.len() {
        if b.get(i) == Some(&b'-')
            && let Some(digits) = b.get(i + 1..i + 9)
            && digits.iter().all(u8::is_ascii_digit)
            && digits.first() == Some(&b'2')
            && digits.get(1) == Some(&b'0')
            && matches!(b.get(i + 9), None | Some(b'-' | b':'))
        {
            let num = |r: std::ops::Range<usize>| std::str::from_utf8(digits.get(r).unwrap_or_default()).ok().and_then(|t| t.parse::<u32>().ok()).unwrap_or(0);
            let (y, m, d) = (num(0..4), num(4..6), num(6..8));
            let days = [31, if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
            if (1..=12).contains(&m) && d >= 1 && days.get((m - 1) as usize).is_some_and(|&n| d <= n) {
                out.push_str(&format!("-{y:04}-{m:02}-{d:02}"));
                i += 9;
                continue;
            }
        }
        if let Some(c) = s.get(i..).and_then(|r| r.chars().next()) {
            out.push(c);
            i += c.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// A model's prices at `at_ns`, a context's variants over them (each unit's from the first variant
/// that gives it): borrowed where no variant changes them.
fn resolve<'m>(m: &'m Model, at_ns: i64, context: &[(&str, &str)]) -> Cow<'m, Prices> {
    let Some(standard) = m.prices.iter().rev().find(|(when, _)| when.as_ref().is_none_or(|w| w.holds(at_ns))).or_else(|| m.prices.first()).map(|(_, p)| p) else {
        return Cow::Owned(Prices::new());
    };
    if context.is_empty() {
        return Cow::Borrowed(standard);
    }
    let (mut out, mut given): (Option<Prices>, u64) = (None, 0);
    for v in &m.variants {
        if v.from.as_ref().is_some_and(|w| !w.holds(at_ns)) || !v.when.iter().all(|(k, val)| context.iter().any(|(ck, cv)| ck == k && cv == val)) {
            continue;
        }
        for (u, p) in &v.prices {
            if given & (1u64 << u) != 0 {
                continue;
            }
            given |= 1u64 << u;
            let o = out.get_or_insert_with(|| standard.clone());
            match o.iter_mut().find(|(x, _)| x == u) {
                Some(slot) => slot.1 = p.clone(),
                None => o.push((*u, p.clone())),
            }
        }
    }
    out.map_or(Cow::Borrowed(standard), Cow::Owned)
}

/// What `usage` costs at `prices` (USD); none where its usage refuses it.
fn cost(prices: &Prices, usage: &Usage) -> Option<Parts> {
    // Each priced unit's own count: its count less its priced descendants', most specific first
    // (on the stack: a unit is priced once at the most).
    let mut priced = [0usize; UNITS.len()];
    let mut n = 0;
    for (u, _) in prices.iter().filter(|(u, _)| *u != REQUESTS) {
        *priced.get_mut(n)? = *u;
        n += 1;
    }
    let priced = priced.get_mut(..n)?;
    priced.sort_unstable_by(|&a, &b| {
        let (ua, ub) = (UNITS.get(a), UNITS.get(b));
        ub.map_or(0, |u| u.dims.len()).cmp(&ua.map_or(0, |u| u.dims.len())).then_with(|| ua.map_or("", |u| u.usage).cmp(ub.map_or("", |u| u.usage)))
    });
    let mut leaf = [(0usize, 0.0f64); UNITS.len()];
    let mut ln = 0;
    for &u in priced.iter() {
        let unit = UNITS.get(u)?;
        let below: f64 = leaf.get(..ln)?.iter().filter(|(d, _)| *d != u && UNITS.get(*d).is_some_and(|du| under(unit, du))).map(|(_, v)| v).sum();
        let own = usage.value(u)? - below;
        if own < 0.0 {
            return None;
        }
        *leaf.get_mut(ln)? = (u, own);
        ln += 1;
    }
    let leaf = leaf.get(..ln)?;
    let tiered = prices.iter().any(|(_, p)| matches!(p, Price::Tiered { .. }));
    let input_tokens = if tiered { usage.value(INPUT_TOKENS)? } else { 0.0 };
    let mut c = Parts::default();
    for (u, p) in prices {
        let unit = UNITS.get(*u)?;
        let count = if *u == REQUESTS { 1.0 } else { leaf.iter().find(|(l, _)| l == u).map_or(0.0, |(_, v)| *v) };
        let amount = p.at(input_tokens) * count / unit.per;
        c.total += amount;
        match (dim(unit, "direction"), dim(unit, "token_type")) {
            (Some("input"), Some("cache_read")) => (c.input, c.cache_read_input) = (c.input + amount, c.cache_read_input + amount),
            (Some("input"), Some("cache_write")) => (c.input, c.cache_write_input) = (c.input + amount, c.cache_write_input + amount),
            (Some("input"), _) => (c.input, c.non_cached_input) = (c.input + amount, c.non_cached_input + amount),
            (Some("output"), Some("reasoning")) => (c.output, c.reasoning_output) = (c.output + amount, c.reasoning_output + amount),
            (Some("output"), _) => c.output += amount,
            _ => {}
        }
    }
    Some(c)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::float_cmp)]
mod tests {
    use super::*;

    /// The catalog kept in kiln reads whole: its providers, its models, Anthropic's and OpenAI's
    /// among them.
    #[test]
    fn the_catalog_kept_reads() {
        let c = catalog();
        let (providers, models) = c.size();
        assert!(providers >= 40 && models >= 2_000, "{providers} providers, {models} models");
        let at = 1_792_800_000_000_000_000; // 2026-10-23
        let mut u = Usage::default();
        u.set(INPUT_TOKENS, 1_000_000.0).set(OUTPUT_TOKENS, 1_000_000.0);
        let p = c.price(&Call { provider: Some("anthropic"), api_url: None, model: "claude-opus-5-5", at_ns: at, context: &[] }, &u).unwrap();
        assert_eq!((p.provider, p.model, p.input, p.output), ("anthropic", "claude-opus-5-5", 4.0, 20.0));
    }

    /// Units' own counts: cache reads and writes out of the input, each at its price; a 1-hour
    /// write out of the writes.
    #[test]
    fn each_unit_is_billed_on_its_own_count() {
        let prices: Prices = vec![
            (INPUT_TOKENS, Price::Flat(4.0)),
            (CACHE_READ_TOKENS, Price::Flat(0.2)),
            (CACHE_WRITE_TOKENS, Price::Flat(5.0)),
            (CACHE_WRITE_1H_TOKENS, Price::Flat(8.0)),
            (OUTPUT_TOKENS, Price::Flat(20.0)),
        ];
        let mut u = Usage::default();
        u.set(INPUT_TOKENS, 1_000_000.0).set(CACHE_READ_TOKENS, 500_000.0).set(CACHE_WRITE_TOKENS, 300_000.0).set(CACHE_WRITE_1H_TOKENS, 100_000.0).set(OUTPUT_TOKENS, 10_000.0);
        let c = cost(&prices, &u).unwrap();
        let want_in = 0.2 * 4.0 + 0.5 * 0.2 + 0.2 * 5.0 + 0.1 * 8.0;
        assert!((c.input - want_in).abs() < 1e-12 && (c.output - 0.2).abs() < 1e-12 && (c.total - c.input - c.output).abs() < 1e-12, "{c:?}");
        // Datadog's parts: the input read from the cache, written to it (both writes), the rest.
        assert!((c.cache_read_input - 0.1).abs() < 1e-12 && (c.cache_write_input - 1.8).abs() < 1e-12 && (c.non_cached_input - 0.8).abs() < 1e-12, "{c:?}");
        // Reads past the input: refused, not priced below nothing.
        u.set(CACHE_READ_TOKENS, 2_000_000.0);
        assert_eq!(cost(&prices, &u), None);
    }

    /// The cliff: a call past a tier's start pays every token at the tier's price.
    #[test]
    fn a_tier_prices_every_token_past_its_start() {
        let prices: Prices = vec![(INPUT_TOKENS, Price::Tiered { base: 0.1, tiers: vec![(100_000.0, 0.5)] }), (OUTPUT_TOKENS, Price::Tiered { base: 0.5, tiers: vec![(100_000.0, 2.5)] })];
        let mut u = Usage::default();
        u.set(INPUT_TOKENS, 100_000.0).set(OUTPUT_TOKENS, 1_000.0);
        assert!((cost(&prices, &u).unwrap().total - (0.1 * 0.1 + 0.001 * 0.5)).abs() < 1e-15, "at its start: the base");
        u.set(INPUT_TOKENS, 100_001.0);
        assert!((cost(&prices, &u).unwrap().total - (0.100_001 * 0.5 + 0.001 * 2.5)).abs() < 1e-15, "past it: every token");
    }

    /// Names: compact dates dashed where they are dates; lookaheads read as their meaning.
    #[test]
    fn names_are_matched_as_the_catalog_s_package_matches_them() {
        assert_eq!(dashed_dates("gpt-5.2-20251211"), "gpt-5.2-2025-12-11");
        assert_eq!(dashed_dates("x-20250230"), "x-20250230", "no 30th of February");
        assert_eq!(dashed_dates("x-20250101a"), "x-20250101a");
        let r = search("^(?!.*-[tT][tT][sS](?:$|-))");
        assert!(r.takes("gemini-2.5-flash") && !r.takes("gemini-2.5-flash-tts") && !r.takes("gemini-tts-preview"));
        let r = search("^(?![^/]+/)(?:labs-)?(?:(?:mi|code|dev|magi|mini)stral|mixtral)");
        assert!(r.takes("mistral-large") && r.takes("labs-codestral") && !r.takes("mistralai/mistral-large") && !r.takes("gpt-5"));
        assert_eq!(group_end("a[)](b)c)d"), Some(8));
    }

    /// The catalog's own Python package's answers (`genai-prices-oracle.jsonl.zst`: what its
    /// `calc_price` said of 4,000 calls drawn from a run of 29,196, every model of the main
    /// providers and three of each other's, each asked under the ids spans name its provider by
    /// (the catalog's, OpenTelemetry's, none), with tokens of every kind, under and past tiers,
    /// dated across price changes, in each price context: `llmobs/oracle/gen.py`;
    /// `KILN_GENAI_ORACLE` names a whole run's file): kiln's the same model, priced the same to the
    /// nanodollar, or refused alike.
    #[test]
    fn prices_are_the_catalog_s_own_package_s() {
        const ORACLE: &[u8] = include_bytes!("genai-prices-oracle.jsonl.zst");
        let mut json = Vec::new();
        crate::zstd::decompress(ORACLE, &mut json, 64 << 20).unwrap();
        if let Ok(path) = std::env::var("KILN_GENAI_ORACLE") {
            json = std::fs::read(path).unwrap();
        }
        let c = catalog();
        let (mut cases, mut differ, mut wrong) = (0usize, 0usize, Vec::new());
        for line in json.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let case: Value = serde_json::from_slice(line).unwrap();
            let mut u = Usage::default();
            for (k, v) in case["usage"].as_object().unwrap() {
                u.set(unit_of(k).unwrap(), v.as_f64().unwrap());
            }
            let ts = case["ts"].as_str().unwrap();
            let (date, time) = ts.trim_end_matches('Z').split_once('T').unwrap();
            let at_ns = (day_of(date).unwrap() * 86_400 + seconds_of(time).unwrap()) * 1_000_000_000;
            let ctx: Vec<(&str, &str)> = case["ctx"].as_object().into_iter().flatten().map(|(k, v)| (k.as_str(), v.as_str().unwrap())).collect();
            let call = Call { provider: case["provider_id"].as_str(), api_url: None, model: case["model_ref"].as_str().unwrap(), at_ns, context: &ctx };
            let got = c.price(&call, &u);
            let ok = match (&case["found"], &got) {
                (Value::Object(f), Ok(p)) => {
                    let near = |k: &str, v: f64| (f[k].as_str().unwrap().parse::<f64>().unwrap() - v).abs() <= 1e-9;
                    f["provider"] == p.provider && f["model"] == p.model && near("input", p.input) && near("output", p.output) && near("total", p.total)
                }
                (Value::Null, Err(Unpriced::Provider | Unpriced::Model)) => case["error"] == "LookupError",
                _ => false,
            };
            cases += 1;
            if !ok {
                differ += 1;
                if wrong.len() < 20 {
                    wrong.push(format!("{} -> {got:?}", String::from_utf8_lossy(line)));
                }
            }
        }
        assert!(cases >= 4_000, "{cases} cases");
        assert!(differ == 0, "{differ} of {cases} differ, the first:\n{}", wrong.join("\n"));
    }

    /// Times: a date's day, a time of day across midnight.
    #[test]
    fn dates_and_times_of_day_hold_as_said() {
        assert_eq!(day_of("1970-01-01"), Some(0));
        assert_eq!(day_of("2026-10-14"), Some(20_740));
        let night = When::Daily(seconds_of("16:30:00Z").unwrap(), seconds_of("00:30:00Z").unwrap());
        let at = |h: i64, m: i64| (20_740 * 86_400 + h * 3600 + m * 60) * 1_000_000_000;
        assert!(night.holds(at(23, 0)) && night.holds(at(0, 10)) && !night.holds(at(12, 0)));
        assert_eq!(seconds_of("08:00:00+08:00"), Some(0));
    }
}
