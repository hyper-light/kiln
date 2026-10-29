//! Datadog's LLM Observability intake (`docs/observe.md` §8.12): what its tracers send of an LLM
//! app's spans and their evaluations, as dd-trace-py 4.16 was recorded sending them (the fixtures
//! under `fixtures/llmobs/`):
//!
//! - **Spans** (`POST /api/v2/llmobs`, an Agent's `/evp_proxy/v2/api/v2/llmobs`): a JSON array of
//!   events, each `{"_dd.stage", "_dd.tracer_version", "event_type": "span", "spans": [...]}` (an
//!   event alone read too), or the documented spans API's (`/api/intake/llm-obs/v1/trace/spans`:
//!   `{"data": {"type": "span", "attributes": {"ml_app", "session_id", "tags", "spans": [...]}}}`,
//!   its `ml_app`, `session_id` and tags each span's where it says none). Each span a row of the
//!   span store, source `llmobs`, of its LLM trace (`trace_id`: Datadog keeps an app's LLM traces
//!   apart from its APM traces, whose spans its tracer sends on beside them, marked
//!   `_dd.llmobs.submitted`; `_dd.apm_trace_id` the APM trace's id):
//!   - its time `start_ns`, `duration` its duration (ns); its message its name; its service its
//!     `service:` tag, else its `ml_app`; its host the Agent's that sent it on, else none; severity
//!     an error's for status `error`;
//!   - its attributes kiln's own (`span.kind` internal, `duration`, `parent_id` hex), then its
//!     fields under Datadog's names: `meta.span.kind` (the documented API's `meta.kind` too),
//!     `meta.model_name`, `meta.model_provider`, `meta.input.value`, `meta.input.messages`,
//!     `meta.input.documents`, `meta.output.value`, `meta.output.messages`,
//!     `meta.output.documents` (each a value, else its JSON), `meta.metadata.<key>` (each of its
//!     metadata's), `meta.tool_definitions`, `meta.error.type`, `.message`, `.stack` (as
//!     `error.type`, `error.message`, `error.stack` too, as kiln's errors read them),
//!     `metrics.<key>`, `session_id`, `ml_app`, `_dd.apm_trace_id`, and each tag's `key:value`
//!     (its `source` as `llmobs.source`: a row's source is its intake's); then kiln's LLM
//!     attributes ([`crate::llm::spans::put`]: its non-cached tokens, its cost).
//! - **Evaluations** (`/api/intake/llm-obs/v2/eval-metric`, v1's, the Agent's EVP proxy's):
//!   records, source `llmobs.eval`, each its span's where it says one (`join_on.span`: its trace's
//!   and its own id), at its `timestamp_ms`, service its `ml_app`: its label, type, value,
//!   assessment, reasoning, scope, kind, status and error, what it joins on, and its tags, as
//!   attributes; its message `label: value`.
//!
//! A value is at most [`MAX_VALUE_BYTES`] (an LLM's messages past it cut), a span's attributes
//! [`MAX_TAGS`] (past them dropped in key order, kiln's LLM attributes' room kept), as a span's of
//! any intake.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock, PoisonError};

use serde_json::Value;

use crate::batch::{MAX_NAME_BYTES, MAX_TAGS};
use crate::logs::record::Record;
use crate::msgpack::{MsgpackError, Reader, Value as MpValue};
use crate::otlp::traces::{Attr, MAX_VALUE_BYTES, SEVERITY_ERROR, SEVERITY_OK, Seg, Spans, Val, cut, finish_segment, merge_under, resolve, write_row};

/// A row's source.
pub const SOURCE: &str = "llmobs";
/// An evaluation's source.
pub const EVAL_SOURCE: &str = "llmobs.eval";
/// kiln's own attributes on a span (`span.kind`, `duration`, `parent_id`).
const OWN: usize = 3;
/// Names made of a group's and a field's ([`joined_key`]) kept at the most, and their length.
const KEYS_MAX: usize = 2_048;
const KEY_BYTES: usize = 128;

/// The name of a field a payload holds apart from its group (`metrics.` and a metric's,
/// `meta.metadata.` and a key's): made once and kept for the process, so a row's attribute borrows
/// it as it borrows the payload's own; [`KEYS_MAX`] at the most, of [`KEY_BYTES`] (a field past
/// them is kept in its group's JSON alone).
fn joined_key(group: &'static str, field: &str) -> Option<&'static str> {
    static KEYS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    if group.len() + field.len() > KEY_BYTES || field.is_empty() {
        return None;
    }
    let mut want = String::with_capacity(group.len() + field.len());
    want.push_str(group);
    want.push_str(field);
    let mut keys = KEYS.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(k) = keys.get(want.as_str()) {
        return Some(k);
    }
    if keys.len() >= KEYS_MAX {
        return None;
    }
    let k: &'static str = Box::leak(want.into_boxed_str());
    keys.insert(k);
    Some(k)
}

#[derive(Debug, thiserror::Error)]
pub enum IntakeError {
    #[error("the body is not JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a payload of Datadog's LLM Observability: {0}")]
    Shape(&'static str),
}

/// What an envelope gives each span that says none of its own (the documented spans API's).
#[derive(Debug, Clone, Copy, Default)]
struct Defaults<'v> {
    ml_app: Option<&'v str>,
    session: Option<&'v str>,
    tags: Option<&'v Value>,
}

/// Decodes payloads, its room kept from one to the next.
#[derive(Debug, Default)]
pub struct Decoder {
    text: String,
    attrs: Vec<Attr<'static>>,
    spare: Vec<Vec<u8>>,
}

struct Cx<'v, 'd> {
    text: String,
    attrs: Vec<Attr<'v>>,
    spare: &'d mut Vec<Vec<u8>>,
    seg: Option<Seg>,
    host: &'v str,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer to write rows into (one a write held and handed back).
    pub fn give_rows(&mut self, mut rows: Vec<u8>) {
        if self.spare.len() < crate::otlp::traces::SPARES {
            rows.clear();
            self.spare.push(rows);
        }
    }

    /// Buffers kept to write rows into.
    pub fn spares(&self) -> usize {
        self.spare.len()
    }

    /// The spans of a payload of the intake into `out`; `host` the Agent's that sent it on (none:
    /// the app's own, sent straight to kiln).
    pub fn spans(&mut self, body: &[u8], host: &str, out: &mut Spans) -> Result<(), IntakeError> {
        let v: Value = serde_json::from_slice(body)?;
        let attrs = crate::otlp::traces::attrs_for(std::mem::take(&mut self.attrs));
        let mut cx = Cx { text: std::mem::take(&mut self.text), attrs, spare: &mut self.spare, seg: None, host };
        let r = (|| {
            let events: &[Value] = match &v {
                Value::Array(a) => a,
                Value::Object(_) => std::slice::from_ref(&v),
                _ => return Err(IntakeError::Shape("an array of events, or an event")),
            };
            for e in events {
                if let Some(a) = e.pointer("/data/attributes") {
                    let d = Defaults { ml_app: a.get("ml_app").and_then(Value::as_str), session: a.get("session_id").and_then(Value::as_str), tags: a.get("tags") };
                    for s in a.get("spans").and_then(Value::as_array).into_iter().flatten() {
                        cx.span(s, d, out);
                    }
                } else if let Some(spans) = e.get("spans").and_then(Value::as_array) {
                    for s in spans {
                        cx.span(s, Defaults::default(), out);
                    }
                } else if e.get("span_id").is_some() {
                    cx.span(e, Defaults::default(), out);
                }
            }
            Ok(())
        })();
        finish_segment(&mut cx.seg, out);
        let Cx { mut text, attrs, .. } = cx;
        text.clear();
        self.text = text;
        self.attrs = crate::otlp::traces::attrs_for(attrs);
        r
    }
}

/// A time or a duration in nanoseconds, not negative: an integer as it is (a float's 53 bits would
/// round a time of now by some 64 ns), else a number rounded.
fn nanos(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_u64().map(|u| i64::try_from(u).unwrap_or(i64::MAX))).or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64)).filter(|t| *t >= 0)
}

/// A span's id: decimal (Datadog's), else sixteen hex digits.
fn span_id(v: &Value) -> Option<[u8; 8]> {
    let n = match v {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => {
            let s = s.trim();
            s.parse::<u64>().ok().or_else(|| (s.len() == 16).then(|| u64::from_str_radix(s, 16).ok()).flatten())?
        }
        _ => return None,
    };
    (n != 0).then(|| n.to_be_bytes())
}

/// A trace's id: 32 hex digits (Datadog's own), else decimal (its low 64 bits).
fn trace_id(v: &Value) -> Option<[u8; 16]> {
    let n = match v {
        Value::Number(n) => u128::from(n.as_u64()?),
        Value::String(s) => {
            let s = s.trim();
            if s.len() == 32 { u128::from_str_radix(s, 16).ok()? } else { s.parse::<u128>().ok()? }
        }
        _ => return None,
    };
    (n != 0).then(|| n.to_be_bytes())
}

impl<'v> Cx<'v, '_> {
    /// `v` written into `text` (a string as it is, anything else its JSON), cut to a value's most.
    fn written(&mut self, v: &Value) -> Val<'v> {
        let at = self.text.len();
        let _ = match v {
            Value::String(s) => write!(self.text, "{s}"),
            other => write!(self.text, "{other}"),
        };
        let end = at + cut(self.text.get(at..).unwrap_or(""), MAX_VALUE_BYTES).len();
        self.text.truncate(end);
        Val::Text(at, end)
    }

    /// `v` as a value: a string the payload's own, anything else written.
    fn val(&mut self, v: &'v Value) -> Option<Val<'v>> {
        match v {
            Value::Null => None,
            Value::String(s) => Some(Val::Str(cut(s, MAX_VALUE_BYTES))),
            other => Some(self.written(other)),
        }
    }

    fn put(&mut self, key: &'v str, v: Option<&'v Value>) {
        if let Some(v) = v
            && let Some(val) = self.val(v)
        {
            self.attrs.push(Attr { key: cut(key, MAX_NAME_BYTES), val });
        }
    }

    fn span(&mut self, s: &'v Value, d: Defaults<'v>, out: &mut Spans) {
        let (Some(trace), Some(span)) = (s.get("trace_id").and_then(trace_id), s.get("span_id").and_then(span_id)) else {
            out.invalid += 1;
            return;
        };
        let parent = s.get("parent_id").and_then(span_id);
        let start = s.get("start_ns").and_then(nanos).filter(|t| *t > 0).unwrap_or(0);
        let duration = s.get("duration").and_then(nanos).unwrap_or(0);
        self.text.clear();
        self.attrs.clear();
        self.attrs.push(Attr { key: "span.kind", val: Val::Str("internal") });
        let at = self.text.len();
        let _ = write!(self.text, "{duration}");
        self.attrs.push(Attr { key: "duration", val: Val::Text(at, self.text.len()) });
        if let Some(p) = parent {
            let at = self.text.len();
            for b in p {
                let _ = write!(self.text, "{b:02x}");
            }
            self.attrs.push(Attr { key: "parent_id", val: Val::Text(at, self.text.len()) });
        }
        // Its fields, under Datadog's names.
        let meta = s.get("meta");
        let m = |k: &str| meta.and_then(|m| m.get(k));
        self.put("meta.span.kind", m("span").and_then(|x| x.get("kind")).or_else(|| m("kind")));
        self.put("meta.model_name", m("model_name"));
        self.put("meta.model_provider", m("model_provider"));
        self.put("meta.model_version", m("model_version"));
        let (input, output) = (m("input"), m("output"));
        for (k, key) in [("value", "meta.input.value"), ("messages", "meta.input.messages"), ("documents", "meta.input.documents"), ("prompt", "meta.input.prompt")] {
            self.put(key, input.and_then(|x| x.get(k)));
        }
        for (k, key) in [("value", "meta.output.value"), ("messages", "meta.output.messages"), ("documents", "meta.output.documents")] {
            self.put(key, output.and_then(|x| x.get(k)));
        }
        // Its metadata whole, and each of its keys.
        self.put("meta.metadata", m("metadata").filter(|md| md.as_object().is_some_and(|o| !o.is_empty())));
        for (k, v) in m("metadata").and_then(Value::as_object).into_iter().flatten() {
            if let Some(key) = joined_key("meta.metadata.", k) {
                self.put(key, Some(v));
            }
        }
        self.put("meta.tool_definitions", m("tool_definitions"));
        if let Some(e) = m("error") {
            for (k, key, kiln) in [("type", "meta.error.type", "error.type"), ("message", "meta.error.message", "error.message"), ("stack", "meta.error.stack", "error.stack")] {
                self.put(key, e.get(k));
                self.put(kiln, e.get(k));
            }
        }
        for (k, v) in s.get("metrics").and_then(Value::as_object).into_iter().flatten().filter(|(_, v)| v.is_number()) {
            match joined_key("metrics.", k) {
                Some(key) => self.put(key, Some(v)),
                None => out.dropped_attrs += 1,
            }
        }
        self.put("session_id", s.get("session_id"));
        self.put("ml_app", s.get("ml_app"));
        self.put("_dd.apm_trace_id", s.get("_dd").and_then(|x| x.get("apm_trace_id")));
        // Its tags, `key:value` each, then the envelope's.
        let mut service = None;
        for list in [s.get("tags"), d.tags] {
            for t in list.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let (k, v) = t.split_once(':').unwrap_or((t, ""));
                match k {
                    "service" => service = service.or(Some(v)),
                    "error" | "error_type" | "host" => {}
                    "source" => self.attrs.push(Attr { key: "llmobs.source", val: Val::Str(cut(v, MAX_VALUE_BYTES)) }),
                    _ => self.attrs.push(Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Str(cut(v, MAX_VALUE_BYTES)) }),
                }
            }
        }
        if let Some(app) = d.ml_app {
            self.attrs.push(Attr { key: "ml_app", val: Val::Str(app) });
        }
        if let Some(sess) = d.session {
            self.attrs.push(Attr { key: "session_id", val: Val::Str(sess) });
        }
        // Each name once, the first said (the span's over its tags', its tags' over its envelope's).
        dedupe(&mut self.attrs, OWN);
        // Its LLM fields read whole, then its attributes bounded, room kept for kiln's.
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        let llm = crate::llm::spans::read_attrs(name, self.attrs.get(OWN..).unwrap_or_default(), &[], &self.text);
        let room = MAX_TAGS - OWN - crate::llm::spans::OWN;
        merge_under(&mut self.attrs, OWN, room, &[], &|_| false, &mut out.dropped_attrs);
        let ts = start;
        let ml_app = self.attrs.iter().find(|a| a.key == "ml_app").map(|a| a.val);
        if let Some(r) = &llm {
            crate::llm::spans::put(&mut self.attrs, &mut self.text, r, ml_app, ts);
        }
        let service = service.or_else(|| ml_app.map(|v| resolve(v, &self.text))).unwrap_or("");
        let severity = if s.get("status").and_then(Value::as_str) == Some("error") { SEVERITY_ERROR } else { SEVERITY_OK };
        write_row(&mut self.seg, self.spare, out, &mut self.attrs, &self.text, ts, severity, [service, self.host, SOURCE], name, trace, span);
    }
}

/// Each attribute name once past `own`, its first kept.
fn dedupe(attrs: &mut Vec<Attr<'_>>, own: usize) {
    let mut i = own;
    while i < attrs.len() {
        let k = attrs.get(i).map_or("", |a| a.key);
        if attrs.get(own..i).unwrap_or_default().iter().any(|a| a.key == k) {
            attrs.remove(i);
        } else {
            i += 1;
        }
    }
}

/// The evaluations of a payload of the intake, as records.
pub fn evaluations(body: &[u8], now_ns: i64) -> Result<Vec<Record>, IntakeError> {
    let v: Value = serde_json::from_slice(body)?;
    let a = v.pointer("/data/attributes").ok_or(IntakeError::Shape("{\"data\": {\"attributes\": {\"metrics\": [...]}}}"))?;
    let shared: Vec<&str> = a.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let mut out = Vec::new();
    for e in a.get("metrics").and_then(Value::as_array).into_iter().flatten() {
        let text = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("");
        let label = text("label");
        let ty = text("metric_type");
        let value = e.get(format!("{ty}_value")).or_else(|| e.get("value")).cloned().unwrap_or(Value::Null);
        let shown = match &value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut put = |k: &str, v: String| {
            if !v.is_empty() && !attrs.iter().any(|(x, _)| x == k) {
                attrs.push((k.to_owned(), cut(&v, MAX_VALUE_BYTES).to_owned()));
            }
        };
        put("label", label.to_owned());
        put("metric_type", ty.to_owned());
        put("value", shown.clone());
        for k in ["assessment", "reasoning", "eval_scope", "event_kind", "ml_app", "status", "session_id", "feedback_join_key"] {
            put(k, text(k).to_owned());
        }
        if let Some(err) = e.get("error") {
            put("error.type", err.get("type").and_then(Value::as_str).unwrap_or("").to_owned());
            put("error.message", err.get("message").and_then(Value::as_str).unwrap_or("").to_owned());
        }
        let span = e.pointer("/join_on/span");
        if let Some(tag) = e.pointer("/join_on/tag") {
            put("join_on.tag.key", tag.get("key").and_then(Value::as_str).unwrap_or("").to_owned());
            put("join_on.tag.value", tag.get("value").and_then(Value::as_str).unwrap_or("").to_owned());
        }
        if let Some(m) = e.get("metadata").filter(|m| !m.is_null()) {
            put("metadata", m.to_string());
        }
        for t in e.get("tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).chain(shared.iter().copied()) {
            let (k, v) = t.split_once(':').unwrap_or((t, ""));
            if !matches!(k, "service" | "host" | "source") {
                put(k, v.to_owned());
            }
        }
        let ts = e.get("timestamp_ms").and_then(Value::as_i64).map_or(now_ns, |ms| ms.saturating_mul(1_000_000));
        let ml_app = text("ml_app");
        out.push(Record {
            ts,
            severity: SEVERITY_OK,
            service: ml_app.to_owned(),
            host: String::new(),
            source: EVAL_SOURCE.to_owned(),
            message: format!("{label}: {shown}"),
            attrs,
            trace_id: span.and_then(|s| s.get("trace_id")).and_then(trace_id),
            span_id: span.and_then(|s| s.get("span_id")).and_then(span_id),
        });
    }
    Ok(out)
}

/// The next value's map's length, or none (a value of another kind passed whole).
fn map_of(r: &mut Reader<'_>) -> Result<Option<u32>, MsgpackError> {
    let at = r.pos();
    if let MpValue::Map(n) = r.value()? {
        return Ok(Some(n));
    }
    *r = r.at(at);
    r.skip()?;
    Ok(None)
}

/// The next value as an attribute's: a string the payload's own, anything else its JSON written
/// into `text` (cut to a value's most); nil none.
fn val_of<'a>(r: &mut Reader<'a>, text: &mut String) -> Result<Option<Val<'a>>, MsgpackError> {
    let at = r.pos();
    match r.value()? {
        MpValue::Nil => Ok(None),
        MpValue::Str(s) => Ok(Some(Val::Str(cut(s, MAX_VALUE_BYTES)))),
        _ => {
            *r = r.at(at);
            let start = text.len();
            r.json_into(text)?;
            let end = start + cut(text.get(start..).unwrap_or(""), MAX_VALUE_BYTES).len();
            text.truncate(end);
            Ok(Some(Val::Text(start, end)))
        }
    }
}

/// `key`'s value into `tags`, where they hold none of the name.
fn put_tag<'a>(tags: &mut Vec<Attr<'a>>, key: &'a str, val: Option<Val<'a>>) {
    if let Some(val) = val
        && !tags.iter().any(|a| a.key == key)
    {
        tags.push(Attr { key, val });
    }
}

/// A Datadog tracer's LLM event as it rides its APM span (`meta_struct["_llmobs"]`, MessagePack:
/// dd-trace-py's, its APM traces sent to an Agent): its fields into the span's `tags`, under the
/// names an event of the intake is given ([`Decoder::spans`]), each where the span says none; its
/// LLM trace's id and its parent in it as `llmobs.trace_id` and `llmobs.parent_id` (hex), its name
/// `llmobs.name`. A struct that does not read leaves what it read.
pub(crate) fn llmobs_struct<'a>(bin: &'a [u8], tags: &mut Vec<Attr<'a>>, text: &mut String) -> Result<(), MsgpackError> {
    let mut r = Reader::new(bin);
    let Some(n) = map_of(&mut r)? else { return Ok(()) };
    for _ in 0..n {
        let k = r.str()?;
        match k {
            "meta" => {
                let Some(m) = map_of(&mut r)? else { continue };
                for _ in 0..m {
                    match r.str()? {
                        "span" => {
                            let Some(s) = map_of(&mut r)? else { continue };
                            for _ in 0..s {
                                if r.str()? == "kind" {
                                    let v = val_of(&mut r, text)?;
                                    put_tag(tags, "meta.span.kind", v);
                                } else {
                                    r.skip()?;
                                }
                            }
                        }
                        "kind" => {
                            let v = val_of(&mut r, text)?;
                            put_tag(tags, "meta.span.kind", v);
                        }
                        f @ ("model_name" | "model_provider" | "model_version" | "tool_definitions") => {
                            let key = match f {
                                "model_name" => "meta.model_name",
                                "model_provider" => "meta.model_provider",
                                "model_version" => "meta.model_version",
                                _ => "meta.tool_definitions",
                            };
                            let v = val_of(&mut r, text)?;
                            put_tag(tags, key, v);
                        }
                        side @ ("input" | "output") => {
                            let Some(io) = map_of(&mut r)? else { continue };
                            for _ in 0..io {
                                let key = match (side, r.str()?) {
                                    ("input", "value") => "meta.input.value",
                                    ("input", "messages") => "meta.input.messages",
                                    ("input", "documents") => "meta.input.documents",
                                    ("input", "prompt") => "meta.input.prompt",
                                    ("output", "value") => "meta.output.value",
                                    ("output", "messages") => "meta.output.messages",
                                    ("output", "documents") => "meta.output.documents",
                                    _ => {
                                        r.skip()?;
                                        continue;
                                    }
                                };
                                let v = val_of(&mut r, text)?;
                                put_tag(tags, key, v);
                            }
                        }
                        "metadata" => {
                            // Whole, then each of its keys.
                            let whole = val_of(&mut r.clone(), text)?;
                            put_tag(tags, "meta.metadata", whole);
                            let Some(md) = map_of(&mut r)? else { continue };
                            for _ in 0..md {
                                let field = r.str()?;
                                let v = val_of(&mut r, text)?;
                                if let Some(key) = joined_key("meta.metadata.", field) {
                                    put_tag(tags, key, v);
                                }
                            }
                        }
                        "error" => {
                            let Some(e) = map_of(&mut r)? else { continue };
                            for _ in 0..e {
                                let (meta, kiln) = match r.str()? {
                                    "type" => ("meta.error.type", "error.type"),
                                    "message" => ("meta.error.message", "error.message"),
                                    "stack" => ("meta.error.stack", "error.stack"),
                                    _ => {
                                        r.skip()?;
                                        continue;
                                    }
                                };
                                let v = val_of(&mut r, text)?;
                                put_tag(tags, meta, v);
                                put_tag(tags, kiln, v);
                            }
                        }
                        _ => r.skip()?,
                    }
                }
            }
            "metrics" => {
                let Some(m) = map_of(&mut r)? else { continue };
                for _ in 0..m {
                    let field = r.str()?;
                    let x = match r.value()? {
                        MpValue::Int(i) => i as f64,
                        MpValue::Uint(u) => u as f64,
                        MpValue::F32(f) => f64::from(f),
                        MpValue::F64(f) => f,
                        MpValue::Array(n) => {
                            for _ in 0..n {
                                r.skip()?;
                            }
                            continue;
                        }
                        MpValue::Map(n) => {
                            for _ in 0..2 * u64::from(n) {
                                r.skip()?;
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    if let Some(key) = joined_key("metrics.", field)
                        && x.is_finite()
                    {
                        let at = text.len();
                        crate::otlp::metrics::push_f64(text, x);
                        put_tag(tags, key, Some(Val::Text(at, text.len())));
                    }
                }
            }
            "tags" => {
                let Some(m) = map_of(&mut r)? else { continue };
                for _ in 0..m {
                    let key = cut(r.str()?, MAX_NAME_BYTES);
                    let v = val_of(&mut r, text)?;
                    match key {
                        // The span's own, or kiln's.
                        "service" | "host" | "env" | "version" | "language" | "error" | "error_type" => {}
                        "source" => put_tag(tags, "llmobs.source", v),
                        _ => put_tag(tags, key, v),
                    }
                }
            }
            "ml_app" | "session_id" => {
                let v = val_of(&mut r, text)?;
                put_tag(tags, if k == "ml_app" { "ml_app" } else { "session_id" }, v);
            }
            "name" => {
                let v = val_of(&mut r, text)?;
                put_tag(tags, "llmobs.name", v);
            }
            "trace_id" => {
                let v = val_of(&mut r, text)?;
                put_tag(tags, "llmobs.trace_id", v);
            }
            "parent_id" => {
                // Decimal (none: "undefined"), kept as kiln keeps a parent's: hex.
                let id = r.str()?.trim().parse::<u64>().ok().filter(|p| *p != 0);
                if let Some(p) = id {
                    let at = text.len();
                    let _ = write!(text, "{p:016x}");
                    put_tag(tags, "llmobs.parent_id", Some(Val::Text(at, text.len())));
                }
            }
            _ => r.skip()?,
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::logs::record::{RecordRef, Rows};

    /// What dd-trace-py 4.16 sent of an LLM app (`fixtures/llmobs/README.txt`): its events through
    /// the Agent's proxy, its evaluation, and its APM spans carrying their events.
    const EVENTS: &[u8] = include_bytes!("../fixtures/llmobs/dd-trace-py-4.16.0-llmobs.json");
    const EVALS: &[u8] = include_bytes!("../fixtures/llmobs/dd-trace-py-4.16.0-eval-metric.json");
    const APM: &[u8] = include_bytes!("../fixtures/llmobs/dd-trace-py-4.16.0-apm-traces.msgpack");
    /// What Datadog's Agent 7.84.4 forwarded of those APM spans (its indexed form, unzipped).
    const AGENT: &[u8] = include_bytes!("../fixtures/llmobs/datadog-agent-7.84.4-traces.pb");

    fn rows(s: &Spans) -> Vec<RecordRef<'_>> {
        let mut out = Vec::new();
        for (raw, n) in &s.segments {
            let mut r = Rows::new(raw, *n as usize);
            while let Some(row) = r.next_row().unwrap() {
                out.push(row);
            }
        }
        out
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn get<'r>(r: &RecordRef<'r>, k: &str) -> Option<&'r str> {
        r.attrs().find(|(x, _)| *x == k).map(|(_, v)| v)
    }

    fn named<'r>(rows: &'r [RecordRef<'r>], name: &str) -> &'r RecordRef<'r> {
        rows.iter().find(|r| r.message == name).unwrap()
    }

    /// Its events are spans of their LLM trace (its own id, the APM trace's said), each its kind,
    /// its parent in the trace, its session, app and tags; its calls priced as the catalog prices
    /// them, found by their models' names (its tracer named no provider of a client of another
    /// address: `unknown`): the chat's cache reads at theirs, Anthropic's 1-hour writes at theirs.
    #[test]
    fn dd_trace_py_s_events_read_as_spans_of_their_llm_trace() {
        let mut out = Spans::default();
        Decoder::new().spans(EVENTS, "web-1", &mut out).unwrap();
        let rows = rows(&out);
        assert_eq!((rows.len(), out.invalid, out.dropped_attrs), (6, 0, 0));
        let root = named(&rows, "answer");
        assert_eq!((root.service, root.host, root.source, root.severity), ("kiln-capture-svc", "web-1", SOURCE, SEVERITY_OK));
        assert_eq!(hex(&root.trace_id.unwrap()), "6adc37bc00000000e2faff95a447a9f1");
        for (k, v) in [
            ("meta.span.kind", "workflow"),
            ("session_id", "sess-kiln-1"),
            ("ml_app", "kiln-capture"),
            ("team", "kiln"),
            ("env", "capture"),
            ("llmobs.source", "integration"),
            ("meta.input.value", "Weather in Paris?"),
        ] {
            assert_eq!(get(root, k), Some(v), "{k}");
        }
        assert_eq!((get(root, "parent_id"), get(root, "_dd.apm_trace_id")), (None, Some("6adc37bc00000000d9598f1d1378c24e")));
        let chat = named(&rows, "OpenAI.createChatCompletion");
        assert_eq!(get(chat, "parent_id"), Some(format!("{:016x}", 4_797_628_901_057_270_574u64).as_str()));
        for (k, v) in [
            ("meta.span.kind", "llm"),
            ("meta.model_name", "gpt-4o-mini-2024-07-18"),
            ("meta.model_provider", "unknown"),
            ("meta.metadata.temperature", "0.2"),
            ("metrics.input_tokens", "1200"),
            ("metrics.non_cached_input_tokens", "200"),
            ("metrics.cache_read_input_tokens", "1000"),
            ("metrics.reasoning_output_tokens", "10"),
            ("metrics.estimated_total_cost", "129000"),
            ("metrics.estimated_cache_read_input_cost", "75000"),
            ("matched_model_provider", "openai"),
            ("matched_model_name", "gpt-4o-mini"),
            ("cost_source", "auto"),
        ] {
            assert_eq!(get(chat, k), Some(v), "{k}");
        }
        assert!(get(chat, "meta.input.messages").is_some_and(|m| m.contains("\"role\":\"system\"")));
        let claude = named(&rows, "anthropic.request");
        for (k, v) in [
            ("metrics.input_tokens", "1120"),
            ("metrics.estimated_input_cost", "900000"),
            ("metrics.estimated_cache_write_input_cost", "620000"),
            ("metrics.estimated_total_cost", "1700000"),
            ("matched_model_name", "claude-opus-5-5"),
        ] {
            assert_eq!(get(claude, k), Some(v), "{k}");
        }
        // The embedding's provider named by none (`unknown`) and its model the catalog's rules find
        // no provider of (OpenAI's and Azure's hold it): not priced, and said so, not guessed.
        let embed = named(&rows, "OpenAI.createEmbedding");
        assert_eq!((get(embed, "metrics.estimated_total_cost"), get(embed, "cost_unavailable")), (None, Some("unsupported model provider")));
        assert_eq!((get(named(&rows, "weather"), "meta.span.kind"), get(named(&rows, "weather"), "cost_source")), (Some("tool"), None));
    }

    /// The documented spans API's envelope gives its spans its app, session and tags; a span with
    /// no ids is not kept, and counted.
    #[test]
    fn the_spans_api_s_envelope_is_its_spans_own() {
        let body = br#"{"data": {"type": "span", "attributes": {"ml_app": "bot", "session_id": "s1", "tags": ["service:bot-svc", "team:x"], "spans": [
            {"parent_id": "undefined", "trace_id": "123", "span_id": "456", "name": "health_coach", "meta": {"kind": "agent", "input": {"value": "hi"}, "error": {"type": "Timeout", "message": "slow"}}, "start_ns": 1713889389104152000, "duration": 1e9, "status": "error"},
            {"name": "no ids", "meta": {"kind": "task"}}]}}}"#;
        let mut out = Spans::default();
        Decoder::new().spans(body, "", &mut out).unwrap();
        let rows = rows(&out);
        assert_eq!((rows.len(), out.invalid), (1, 1));
        let r = &rows[0];
        assert_eq!((r.service, r.severity, r.ts), ("bot-svc", SEVERITY_ERROR, 1_713_889_389_104_152_000));
        for (k, v) in [("meta.span.kind", "agent"), ("ml_app", "bot"), ("session_id", "s1"), ("team", "x"), ("duration", "1000000000"), ("error.type", "Timeout"), ("meta.error.message", "slow")] {
            assert_eq!(get(r, k), Some(v), "{k}");
        }
        assert_eq!(hex(&r.trace_id.unwrap()), format!("{:032x}", 123));
        assert!(Decoder::new().spans(b"7", "", &mut out).is_err() && Decoder::new().spans(b"{", "", &mut out).is_err());
    }

    /// Its evaluation: a record of its span (its LLM trace's id and its own), its label, type,
    /// value, assessment and reasoning.
    #[test]
    fn an_evaluation_is_a_record_of_its_span() {
        let r = evaluations(EVALS, 0).unwrap();
        assert_eq!(r.len(), 1);
        let e = &r[0];
        assert_eq!((e.source.as_str(), e.service.as_str(), e.message.as_str()), (EVAL_SOURCE, "kiln-capture", "accuracy: 0.8"));
        assert_eq!(hex(&e.span_id.unwrap()), format!("{:016x}", 927_929_655_401_069_197u64));
        assert_eq!(hex(&e.trace_id.unwrap()), "6adc37bc00000000e2faff95a447a9f1");
        for (k, v) in [("label", "accuracy"), ("metric_type", "score"), ("value", "0.8"), ("assessment", "pass"), ("reasoning", "matches the forecast"), ("eval_scope", "span")] {
            assert!(e.attrs.iter().any(|(x, y)| x == k && y == v), "{k}: {:?}", e.attrs);
        }
    }

    /// Its APM spans carrying their events (`meta_struct`): each the LLM span it is, read in
    /// place, its event's fields over the copy its tracer made of them as tags; its LLM trace and
    /// parent said beside its APM ones.
    #[test]
    fn apm_spans_carrying_their_events_are_llm_spans_themselves() {
        let tracer = crate::ddtrace::Tracer { language: "python", container: "", host: "web-1" };
        let (mut out, mut stats, mut roots) = (Spans::default(), Vec::new(), Vec::new());
        crate::ddtrace::Decoder::new().decode_tracer(false, APM, &tracer, 0, &mut out, &mut stats, &mut roots).unwrap();
        let rows = rows(&out);
        assert_eq!(rows.len(), 6);
        let chat = named(&rows, "createChatCompletion");
        assert_eq!(hex(&chat.span_id.unwrap()), "90c7e670d75fca0c");
        for (k, v) in [
            ("meta.span.kind", "llm"),
            ("llmobs.name", "OpenAI.createChatCompletion"),
            ("llmobs.trace_id", "6adc37b40000000098d883192b31af5b"),
            ("llmobs.parent_id", "c3d8a8f1a2de5721"),
            ("metrics.input_tokens", "1200"),
            ("metrics.estimated_total_cost", "129000"),
            ("ml_app", "kiln-capture"),
            ("integration", "openai"),
        ] {
            assert_eq!(get(chat, k), Some(v), "{k}");
        }
        assert_eq!(get(named(&rows, "tool"), "meta.span.kind"), Some("tool"));
        assert_eq!(get(named(&rows, "Messages.create"), "metrics.estimated_total_cost"), Some("1700000"));
    }

    /// The same APM spans as Datadog's Agent 7.84 forwards them (each span's LLM event an attribute
    /// of bytes, `_llmobs`): read as the tracer's own are, the event not kept as its bytes.
    #[test]
    fn an_agent_s_spans_carrying_their_events_are_llm_spans_themselves() {
        let mut out = Spans::default();
        crate::ddtrace::Decoder::new().decode(AGENT, 0, &mut out).unwrap();
        let rows = rows(&out);
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().all(|r| get(r, "_llmobs").is_none() && get(r, "meta.span.kind").is_some()));
        let chat = named(&rows, "createChatCompletion");
        for (k, v) in
            [("meta.span.kind", "llm"), ("llmobs.name", "OpenAI.createChatCompletion"), ("metrics.input_tokens", "1200"), ("metrics.estimated_total_cost", "129000"), ("ml_app", "kiln-capture")]
        {
            assert_eq!(get(chat, k), Some(v), "{k}");
        }
        assert_eq!(get(named(&rows, "Messages.create"), "metrics.estimated_total_cost"), Some("1700000"));
    }
}
