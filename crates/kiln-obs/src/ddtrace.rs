//! Datadog's traces as its Agent sends them (`docs/observe.md` §8.6, §8.7). What an Agent 7.84
//! (and a ddtrace 4.16 app behind it) was recorded sending is what the tests hold these to:
//!
//! - `POST /api/v0.2/traces`: an `AgentPayload` (protobuf, the Agent's `datadog/trace`
//!   `agent_payload.proto`) of its tracers' payloads ([`Decoder::decode`]);
//! - `POST /api/v0.2/stats`: a `StatsPayload` (MessagePack, `stats.proto` as Go's msgp writes it:
//!   each message a map by its fields' names) ([`stats`]).
//!
//! **Spans.** An `AgentPayload` holds its tracers' payloads in one of two forms: indexed, as an
//! Agent 7.84 sends them (`idxTracerPayloads`, field 11: each payload's strings once, in a table
//! it writes after what refers to it, every string a reference into it), or as Agents before it
//! did (`tracerPayloads`, field 5: strings inline, a span's tags in `meta` and its numbers in
//! `metrics`). Either is read in place and written straight into the span store's rows, as OTLP's
//! spans are (`crate::otlp::traces`), the decoder's room kept from one payload to the next. A span
//! is a record:
//! - its time the span's start, `duration` its duration (ns);
//! - its message its resource (else its operation's name); `service` its service; `host` its
//!   tracer's (`_dd.hostname`, else its payload's), else the Agent's; `source` [`SOURCE`];
//! - severity 17 (`status:error`) for an error, else 9;
//! - its trace id the 128 bits W3C's `traceparent` carries (the indexed form's chunk's; the older
//!   form's 64, and above them the 64 its chunk's `_dd.p.tid` says), its span id its 64, both
//!   big-endian: a trace that crosses from a Datadog tracer to an OpenTelemetry SDK is one trace;
//! - its attributes kiln's own first: `span.kind` (its kind, else its `span.kind` tag, else
//!   internal), `duration`, `parent_id` (hex), `operation_name`, `span.type` and `component`; then
//!   the span's tags and numbers, and its chunk's and its payload's where it has none of the name
//!   (`_dd.tags.container` and `_dd.tags.process` each of their `key:value`s), as an OTLP span's
//!   over its resource's (`crate::otlp::traces::merge_under`); then, where the span says none,
//!   `env` (its own, else its payload's, else the Agent's) and `version` (its own: a tracer sets
//!   none on a span of a service not its own), `error.message` (older tracers' `error.msg`),
//!   `http.status_code` (from `http.response.status_code`), `peer.service` on a client's or a
//!   producer's span ([`PEER_SOURCES`]), and its events and links as JSON, as OTLP's.
//!
//! **Stats.** The Agent counts every span it is sent before it samples (its concentrator), and
//! sends on only the traces it keeps (ten a second by default, `apm_config.target_traces_per_second`),
//! so Datadog counts its trace metrics from its Agents' stats, not from the spans it keeps, and so
//! does kiln: [`stats`] reads them as kiln's trace metrics (`crate::apm::trace_metrics`), which
//! count none of the spans an Agent sends (`source:datadog-agent`). Each grouped stat (a service's
//! spans of one resource, kind, status and so on, in a bucket of ten seconds) counts:
//! - as its service's entries (`trace.*`): a server's or a consumer's, or another kind's (internal,
//!   or none said) that are top-level (a trace's root, or a span whose parent is another service's:
//!   `TopLevelHits`, and of its counts the share its top-level spans are of its hits);
//! - as its service's exits (`trace.outbound.*`): a client's or a producer's, by the peer its peer
//!   tags name ([`PEER_SOURCES`], as a span's `peer.service` is found);
//! - not at all, of spans inside a service (measured ones), of which kiln counts none.
//!
//! Its hits and errors are the metrics' counts, its durations' total their `duration.sum` (s), and
//! its two sketches (DDSketch: Masson, Rim and Lee, VLDB 2019; `sketches-go`'s encoding, a mapping
//! of γ, an offset and an interpolation, then bins' counts by index) their buckets: each bin
//! counted into kiln's bucket of the value the sketch gives it (within its relative accuracy, 1%
//! for the Agent's, of the durations in it), so a percentile is within that and kiln's 1.09%
//! (§4.4); `duration.min` and `.max` its least and greatest bins' values. A series has the
//! span-counted one's tags and its `host` (the tracer's, else the Agent's): two hosts' stats of a
//! service are two series, which queries sum, never one written over the other. A payload's
//! points are at its buckets' starts and an offset within the bucket its bytes' checksum gives:
//! an Agent's retry (the same payload) writes over itself, and two payloads that split a series'
//! bucket between them (`SplitPayload`) add.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::batch::{Kind, MAX_NAME_BYTES, MAX_TAGS, SeriesKey};
use crate::dist::{self, Bucket};
use crate::msgpack::{MsgpackError, Reader};
use crate::otlp::metrics::{Decoded, Keys, MAX_DEPTH, OtlpError, push_f64, push_json_str};
use crate::otlp::pb::{Fields, PbError, Wire, doubles};
use crate::otlp::traces::{
    Attr, KINDS, MAX_VALUE_BYTES, PEER_SOURCES, SEVERITY_ERROR, SEVERITY_OK, SPARES, Seg, Spans, Val, attrs_for, cut, find, finish_segment, id, merge_under, parts_for, put_issue, resolve, write_row,
};

/// The `source` of a span an Agent sent.
pub const SOURCE: &str = "datadog-agent";

/// kiln's own attributes on a Datadog span at the most (`span.kind`, `duration`, `parent_id`,
/// `operation_name`, `span.type`, `component`, `env`, `version`, `error.type`, `error.message`,
/// `error.stack`, `http.status_code`, `peer.service`, `events`, `links`, `issue.id`): room kept for
/// them under [`MAX_TAGS`].
const OWN: usize = 16;
/// kiln's own that a tag of the same name never stands for (an issue's id kiln's alone), and the
/// tags read into a row's fields.
const FIXED: [&str; 9] = ["span.kind", "duration", "parent_id", "operation_name", "span.type", "component", "_dd.p.tid", "_dd.hostname", "issue.id"];
/// Tags that hold others, each `key:value` and comma-separated: the Agent's container's and the
/// tracer's process's.
const PACKED: [&str; 2] = ["_dd.tags.container", "_dd.tags.process"];

/// A span's parts of these kinds, kept to be written after its tags.
const LINK: u32 = 1;
const EVENT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DdError {
    #[error("the payload is malformed: {0}")]
    Pb(#[from] PbError),
    #[error("the payload is malformed: {0}")]
    Msgpack(#[from] MsgpackError),
}

impl From<OtlpError> for DdError {
    fn from(e: OtlpError) -> Self {
        match e {
            OtlpError::Pb(p) => Self::Pb(p),
        }
    }
}

/// Which form a tracer's payload is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Indexed,
    Inline,
}

/// Decodes payloads, its room kept from one to the next.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Values written (a payload's first, then its chunk's, then a span's).
    text: String,
    /// A value rendered, before it is escaped into an event's or a link's JSON.
    tmp: String,
    /// The indexed form's string table.
    strings: Vec<&'static str>,
    payload: Vec<Attr<'static>>,
    chunk: Vec<Attr<'static>>,
    /// A span's tags and numbers, as it holds them.
    tags: Vec<Attr<'static>>,
    attrs: Vec<Attr<'static>>,
    /// A span's links and events, each with its kind.
    parts: Vec<(u32, &'static [u8])>,
    /// A tracer's chunk's spans' ids and services, to tell its top-level spans.
    ids: Vec<(u64, &'static str)>,
    /// A chunk's spans as its first pass reads them, and those whose errors are issues.
    briefs: Vec<Brief<'static>>,
    carriers: Vec<u64>,
    /// Row buffers handed back ([`Decoder::give`]).
    spare: Vec<Vec<u8>>,
}

fn strs_for<'b>(mut v: Vec<&str>) -> Vec<&'b str> {
    v.clear();
    v.into_iter().map(|_| "").collect()
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Row buffers back, to write the next payload's rows into.
    pub fn give(&mut self, spans: Spans) {
        for (rows, _) in spans.segments {
            self.give_rows(rows);
        }
    }

    /// A buffer to write rows into (`rows` emptied); past [`SPARES`] kept, it goes.
    pub fn give_rows(&mut self, mut rows: Vec<u8>) {
        if self.spare.len() < SPARES {
            rows.clear();
            self.spare.push(rows);
        }
    }

    /// Buffers kept to write rows into.
    pub fn spares(&self) -> usize {
        self.spare.len()
    }

    /// The spans of an `AgentPayload` into `out`; `now_ns` stands for a span that says no time.
    pub fn decode(&mut self, body: &[u8], now_ns: i64, out: &mut Spans) -> Result<(), DdError> {
        self.run(out, |cx, out| -> Result<(), OtlpError> {
            // The Agent's own first: what its tracers' payloads fall back on.
            let mut agent = Agent { host: "", env: "" };
            for f in Fields::new(body) {
                match f? {
                    (1, w) => agent.host = w.str().unwrap_or(""),
                    (2, w) => agent.env = w.str().unwrap_or(""),
                    _ => {}
                }
            }
            for f in Fields::new(body) {
                match f? {
                    (5, w) => cx.payload(Form::Inline, w.bytes().unwrap_or(&[]), &agent, now_ns, out)?,
                    (11, w) => cx.payload(Form::Indexed, w.bytes().unwrap_or(&[]), &agent, now_ns, out)?,
                    _ => {}
                }
            }
            Ok(())
        })
    }

    /// A tracer's own payload, as it sends its Agent (kiln in the Agent's place):
    /// `/v0.4/traces` (`v5` false: MessagePack, each trace an array of spans, each span a map) or
    /// `/v0.5/traces` (its strings first, in a table, each span an array of twelve). Every span is
    /// counted into `stats` first, as the Agent's concentrator counts them; a trace its tracer
    /// dropped (its priority below zero, or zero and no error in it) is counted and not kept.
    /// Each trace's root into `roots`, as the sampler counts it.
    #[allow(clippy::too_many_arguments)]
    pub fn decode_tracer<'a>(
        &mut self,
        v5: bool,
        body: &'a [u8],
        tracer: &Tracer<'a>,
        now_ns: i64,
        out: &mut Spans,
        stats: &mut Vec<SpanStat<'a>>,
        roots: &mut Vec<Root<'a>>,
    ) -> Result<TracerTaken, DdError> {
        let mut taken = TracerTaken::default();
        self.run(out, |cx, out| cx.tracer(v5, body, tracer, now_ns, out, stats, roots, &mut taken))?;
        Ok(taken)
    }

    /// Runs `f` with the decoder's room lent for one payload's borrows, its last segment then done.
    fn run<'a, E>(&mut self, out: &mut Spans, f: impl FnOnce(&mut Cx<'a, '_>, &mut Spans) -> Result<(), E>) -> Result<(), DdError>
    where
        DdError: From<E>,
    {
        let mut cx = Cx {
            text: std::mem::take(&mut self.text),
            tmp: std::mem::take(&mut self.tmp),
            strings: strs_for(std::mem::take(&mut self.strings)),
            payload: attrs_for(std::mem::take(&mut self.payload)),
            chunk: attrs_for(std::mem::take(&mut self.chunk)),
            tags: attrs_for(std::mem::take(&mut self.tags)),
            attrs: attrs_for(std::mem::take(&mut self.attrs)),
            parts: parts_for(std::mem::take(&mut self.parts)),
            ids: ids_for(std::mem::take(&mut self.ids)),
            briefs: briefs_for(std::mem::take(&mut self.briefs)),
            carriers: std::mem::take(&mut self.carriers),
            spare: &mut self.spare,
            seg: None,
        };
        let r = f(&mut cx, out);
        finish_segment(&mut cx.seg, out);
        let Cx { mut text, mut tmp, strings, payload, chunk, tags, attrs, parts, ids, briefs, mut carriers, .. } = cx;
        text.clear();
        tmp.clear();
        (self.text, self.tmp) = (text, tmp);
        self.strings = strs_for(strings);
        self.payload = attrs_for(payload);
        self.chunk = attrs_for(chunk);
        self.tags = attrs_for(tags);
        self.attrs = attrs_for(attrs);
        self.parts = parts_for(parts);
        self.ids = ids_for(ids);
        self.briefs = briefs_for(briefs);
        carriers.clear();
        self.carriers = carriers;
        r.map_err(DdError::from)
    }
}

fn ids_for<'b>(mut v: Vec<(u64, &str)>) -> Vec<(u64, &'b str)> {
    v.clear();
    v.into_iter().map(|_| (0, "")).collect()
}

fn briefs_for<'b>(mut v: Vec<Brief<'_>>) -> Vec<Brief<'b>> {
    v.clear();
    v.into_iter().map(|_| Brief::default()).collect()
}

/// What a chunk's first pass reads of a span: its id, its parent's, its service, whether it erred,
/// and whether with its error's details (its type, message and stack, as Datadog's Error Tracking
/// asks of a span).
#[derive(Debug, Clone, Copy, Default)]
struct Brief<'a> {
    span: u64,
    parent: u64,
    service: &'a str,
    error: bool,
    detailed: bool,
}

/// How far up its service's spans a span's error is followed to its entry at the most.
const WALK: u32 = 64;

/// Which of an error's details a tag says (Datadog's Error Tracking's: `error.type` 1,
/// `error.message` or older tracers' `error.msg` 2, `error.stack` 4), a blank one none.
fn error_detail(k: &str, v: &str) -> u8 {
    let bit = match k {
        "error.type" => 1,
        "error.message" | "error.msg" => 2,
        "error.stack" => 4,
        _ => return 0,
    };
    if v.trim().is_empty() { 0 } else { bit }
}

/// A chunk's spans (`briefs`) whose errors are issues, their ids into `out` (sorted): of each
/// service's segment of the trace (its entry span, a root or a span whose parent is another
/// service's or none of the chunk's, and its service's spans under it) whose entry erred, the
/// top-most span with its error's details. A tracer may set them on its entry (OpenTelemetry's SDKs
/// do) or below it (ddtrace's Flask: on the request's dispatching span, its entry only a 500); an
/// error reported by many spans of a service is one, its top-most (Datadog's). A span more than
/// [`WALK`] spans of its service below its entry is passed over.
fn carriers(briefs: &mut [Brief<'_>], out: &mut Vec<u64>) {
    out.clear();
    if !briefs.iter().any(|b| b.error && b.detailed) {
        return;
    }
    briefs.sort_unstable_by_key(|b| b.span);
    let find = |id: u64| briefs.binary_search_by_key(&id, |b| b.span).ok().and_then(|i| briefs.get(i));
    // Each entry's: its top-most such span's depth, and its id.
    let mut best: Vec<(u64, u32, u64)> = Vec::new();
    for b in briefs.iter().filter(|b| b.error && b.detailed) {
        let (mut entry, mut depth) = (*b, 0u32);
        while let Some(p) = find(entry.parent).filter(|p| p.service == entry.service && p.span != entry.span) {
            entry = *p;
            depth += 1;
            if depth > WALK {
                break;
            }
        }
        if depth > WALK || !entry.error {
            continue;
        }
        match best.iter_mut().find(|x| x.0 == entry.span) {
            Some(x) if (depth, b.span) < (x.1, x.2) => (x.1, x.2) = (depth, b.span),
            Some(_) => {}
            None => best.push((entry.span, depth, b.span)),
        }
    }
    out.extend(best.iter().map(|x| x.2));
    out.sort_unstable();
}

/// What a tracer's request says of it beyond its spans: its headers, and where it came.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tracer<'a> {
    /// `Datadog-Meta-Lang`.
    pub language: &'a str,
    /// `Datadog-Container-ID`.
    pub container: &'a str,
    /// The host its Agent is: the machine kiln took it on.
    pub host: &'a str,
}

/// What a tracer's payload held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TracerTaken {
    pub traces: u64,
    /// Traces its tracer dropped: counted, not kept.
    pub dropped: u64,
}

/// A span as the Agent's concentrator counts it ([`Concentrator`]): of a service's entries or its
/// exits, by what its trace metrics are by. Its strings the payload's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpanStat<'a> {
    pub outbound: bool,
    pub service: &'a str,
    pub resource: &'a str,
    pub kind: &'static str,
    pub env: &'a str,
    /// An entry's version, an exit's peer.
    pub third: &'a str,
    pub status: u64,
    pub start_ns: i64,
    pub duration_ns: u64,
    pub error: bool,
}

/// One payload's decoding: the decoder's room, lent for its borrows.
struct Cx<'a, 'd> {
    text: String,
    tmp: String,
    strings: Vec<&'a str>,
    payload: Vec<Attr<'a>>,
    chunk: Vec<Attr<'a>>,
    tags: Vec<Attr<'a>>,
    attrs: Vec<Attr<'a>>,
    parts: Vec<(u32, &'a [u8])>,
    ids: Vec<(u64, &'a str)>,
    briefs: Vec<Brief<'a>>,
    carriers: Vec<u64>,
    spare: &'d mut Vec<Vec<u8>>,
    seg: Option<Seg>,
}

/// The Agent's host and env.
struct Agent<'a> {
    host: &'a str,
    env: &'a str,
}

/// What a tracer's payload's spans share.
#[derive(Default)]
struct Payload<'a> {
    host: &'a str,
    env: &'a str,
}

/// A span's fields, read.
#[derive(Default)]
struct Span<'a> {
    service: &'a str,
    name: &'a str,
    resource: &'a str,
    ty: &'a str,
    env: &'a str,
    version: &'a str,
    component: &'a str,
    trace: Option<[u8; 16]>,
    span: Option<[u8; 8]>,
    parent: u64,
    start: u64,
    duration: u64,
    error: bool,
    /// `SpanKind` (the indexed form's), 0 where none is said.
    kind: u64,
}

/// A packed tag's `key:value`s to `each`; whether it is one.
fn unpacked<'a>(a: Attr<'a>, mut each: impl FnMut(Attr<'a>)) -> bool {
    let (true, Val::Str(s)) = (PACKED.contains(&a.key), a.val) else { return false };
    for part in s.split(',') {
        if let Some((k, v)) = part.split_once(':')
            && !k.trim().is_empty()
        {
            each(Attr { key: cut(k.trim(), MAX_NAME_BYTES), val: Val::Str(v.trim()) });
        }
    }
    true
}

/// A 64-bit id as kiln holds it: big-endian, none if zero.
fn id64(v: u64) -> Option<[u8; 8]> {
    (v != 0).then(|| v.to_be_bytes())
}

/// A trace's id of its high and low 64 bits, none if both are zero.
fn id128(hi: u64, lo: u64) -> Option<[u8; 16]> {
    if hi == 0 && lo == 0 {
        return None;
    }
    let mut t = [0u8; 16];
    let (h, l) = t.split_at_mut(8);
    h.copy_from_slice(&hi.to_be_bytes());
    l.copy_from_slice(&lo.to_be_bytes());
    Some(t)
}

/// `_dd.p.tid`'s 16 hex digits.
fn hex64(s: &str) -> Option<u64> {
    (s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u64::from_str_radix(s, 16).ok()).flatten()
}

fn push_hex(out: &mut String, b: &[u8]) {
    for x in b {
        let _ = write!(out, "{x:02x}");
    }
}

/// A `map<string, string>` entry's key and value.
fn entry(msg: &[u8]) -> Result<(&str, &str), OtlpError> {
    let (mut k, mut v) = ("", "");
    for f in Fields::new(msg) {
        match f? {
            (1, w) => k = w.str().unwrap_or(""),
            (2, w) => v = w.str().unwrap_or(""),
            _ => {}
        }
    }
    Ok((k, v))
}

impl<'a> Cx<'a, '_> {
    /// A reference into the string table, read.
    fn string(&self, w: Wire<'_>) -> &'a str {
        w.u64().and_then(|i| usize::try_from(i).ok()).and_then(|i| self.strings.get(i)).copied().unwrap_or("")
    }

    /// A name a form holds: the indexed form's by reference, the other's inline.
    fn name_of(&self, form: Form, w: Wire<'a>) -> &'a str {
        match form {
            Form::Indexed => self.string(w),
            Form::Inline => w.str().unwrap_or(""),
        }
    }

    /// An indexed attributes map's entry (`map<uint32, AnyValue>`) as an attribute.
    fn indexed_attr(&mut self, msg: &'a [u8]) -> Result<Option<Attr<'a>>, OtlpError> {
        let (mut key, mut value) = ("", None);
        for f in Fields::new(msg) {
            match f? {
                (1, w) => key = self.string(w),
                (2, w) => value = w.bytes(),
                _ => {}
            }
        }
        if key.is_empty() {
            return Ok(None);
        }
        let val = match value {
            Some(v) => self.indexed_value(v)?,
            None => Val::Str(""),
        };
        Ok(Some(Attr { key: cut(key, MAX_NAME_BYTES), val }))
    }

    /// An indexed `AnyValue`: a string the payload's own, a bool's name, anything else written.
    fn indexed_value(&mut self, v: &'a [u8]) -> Result<Val<'a>, OtlpError> {
        for f in Fields::new(v) {
            match f? {
                (1, w) => return Ok(Val::Str(cut(self.string(w), MAX_VALUE_BYTES))),
                (2, w) => return Ok(Val::Str(if w.u64().is_some_and(|b| b != 0) { "true" } else { "false" })),
                (3..=7, _) => {
                    let at = self.text.len();
                    indexed_value_push(v, &self.strings, MAX_VALUE_BYTES, &mut self.text)?;
                    return Ok(Val::Text(at, self.text.len()));
                }
                _ => {}
            }
        }
        Ok(Val::Str(""))
    }

    /// A payload's or a chunk's attribute into `list`: a packed one as each of its `key:value`s
    /// ([`unpacked`]), a name once, and none past [`MAX_TAGS`] (what a row could hold).
    fn under(list: &mut Vec<Attr<'a>>, a: Attr<'a>, dropped: &mut u64) {
        let mut put = |a: Attr<'a>| {
            if list.len() >= MAX_TAGS {
                *dropped += 1;
            } else if !list.iter().any(|x| x.key == a.key) {
                list.push(a);
            }
        };
        if !unpacked(a, &mut put) {
            put(a);
        }
    }

    /// A tracer's payload, of either form.
    fn payload(&mut self, form: Form, msg: &'a [u8], agent: &Agent<'a>, now_ns: i64, out: &mut Spans) -> Result<(), OtlpError> {
        self.text.clear();
        self.payload.clear();
        self.strings.clear();
        if form == Form::Indexed {
            // The string table first: an Agent 7.84 writes it after what refers to it.
            for f in Fields::new(msg) {
                if let (1, w) = f? {
                    self.strings.push(w.str().unwrap_or(""));
                }
            }
        }
        let mut pl = Payload::default();
        let (mut container, mut language) = ("", "");
        let mut payload = std::mem::take(&mut self.payload);
        for f in Fields::new(msg) {
            match (form, f?) {
                (Form::Indexed, (2, w)) | (Form::Inline, (1, w)) => container = self.name_of(form, w),
                (Form::Indexed, (3, w)) | (Form::Inline, (2, w)) => language = self.name_of(form, w),
                (Form::Indexed, (7, w)) | (Form::Inline, (8, w)) => pl.env = self.name_of(form, w),
                (Form::Indexed, (8, w)) | (Form::Inline, (9, w)) => pl.host = self.name_of(form, w),
                (Form::Indexed, (10, w)) => {
                    if let Some(a) = self.indexed_attr(w.bytes().unwrap_or(&[]))? {
                        Self::under(&mut payload, a, &mut out.dropped_attrs);
                    }
                }
                (Form::Inline, (7, w)) => {
                    let (k, v) = entry(w.bytes().unwrap_or(&[]))?;
                    if !k.is_empty() {
                        Self::under(&mut payload, Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Str(cut(v, MAX_VALUE_BYTES)) }, &mut out.dropped_attrs);
                    }
                }
                _ => {}
            }
        }
        for (key, v) in [("container_id", container), ("language", language)] {
            if !v.is_empty() {
                Self::under(&mut payload, Attr { key, val: Val::Str(cut(v, MAX_VALUE_BYTES)) }, &mut out.dropped_attrs);
            }
        }
        self.payload = payload;
        let at = self.text.len();
        let chunks = if form == Form::Indexed { 11 } else { 6 };
        for f in Fields::new(msg) {
            match f? {
                (n, w) if n == chunks => self.chunk(form, w.bytes().unwrap_or(&[]), &pl, agent, at, now_ns, out)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// A chunk: a trace's spans (the indexed form's under its trace's id).
    #[allow(clippy::too_many_arguments)]
    fn chunk(&mut self, form: Form, msg: &'a [u8], pl: &Payload<'a>, agent: &Agent<'a>, text_at: usize, now_ns: i64, out: &mut Spans) -> Result<(), OtlpError> {
        self.text.truncate(text_at);
        self.chunk.clear();
        let mut chunk = std::mem::take(&mut self.chunk);
        let (mut trace, mut hi, mut origin) = (None, None, "");
        for f in Fields::new(msg) {
            match (form, f?) {
                (_, (2, w)) => origin = self.name_of(form, w),
                (Form::Indexed, (3, w)) => {
                    if let Some(a) = self.indexed_attr(w.bytes().unwrap_or(&[]))? {
                        Self::under(&mut chunk, a, &mut out.dropped_attrs);
                    }
                }
                (Form::Indexed, (6, w)) => trace = id::<16>(w.bytes()),
                (Form::Inline, (4, w)) => {
                    let (k, v) = entry(w.bytes().unwrap_or(&[]))?;
                    if k == "_dd.p.tid" {
                        hi = hex64(v);
                    } else if !k.is_empty() {
                        Self::under(&mut chunk, Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Str(cut(v, MAX_VALUE_BYTES)) }, &mut out.dropped_attrs);
                    }
                }
                _ => {}
            }
        }
        if !origin.is_empty() {
            Self::under(&mut chunk, Attr { key: "_dd.origin", val: Val::Str(cut(origin, MAX_VALUE_BYTES)) }, &mut out.dropped_attrs);
        }
        self.chunk = chunk;
        if form == Form::Inline && hi.is_none() {
            hi = trace_high(msg)?;
        }
        let at = self.text.len();
        let spans = if form == Form::Indexed { 4 } else { 3 };
        // The spans whose errors are issues: a first pass, of each span its brief.
        self.briefs.clear();
        for f in Fields::new(msg) {
            if let (n, w) = f?
                && n == spans
            {
                let b = self.brief(form, w.bytes().unwrap_or(&[]))?;
                self.briefs.push(b);
            }
        }
        carriers(&mut self.briefs, &mut self.carriers);
        for f in Fields::new(msg) {
            match f? {
                (n, w) if n == spans => {
                    let s = match form {
                        Form::Indexed => self.indexed_span(w.bytes().unwrap_or(&[]), trace, at)?,
                        Form::Inline => self.inline_span(w.bytes().unwrap_or(&[]), hi.unwrap_or(0), at)?,
                    };
                    let issue = self.carries(&s);
                    self.row(&s, form, pl, agent, now_ns, issue, out)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether span `s` carries its service's segment's error ([`carriers`]).
    fn carries(&self, s: &Span<'a>) -> bool {
        s.span.is_some_and(|id| self.carriers.binary_search(&u64::from_be_bytes(id)).is_ok())
    }

    /// An Agent's chunk's span, as the first pass reads it ([`Brief`]).
    fn brief(&self, form: Form, msg: &'a [u8]) -> Result<Brief<'a>, OtlpError> {
        let mut b = Brief::default();
        let mut details = 0u8;
        for f in Fields::new(msg) {
            match (form, f?) {
                (Form::Indexed, (1, w)) => b.service = self.string(w),
                (Form::Indexed, (4, w)) | (Form::Inline, (5, w)) => b.span = w.u64().unwrap_or(0),
                (Form::Indexed, (5, w)) | (Form::Inline, (6, w)) => b.parent = w.u64().unwrap_or(0),
                (Form::Indexed, (8, w)) | (Form::Inline, (9, w)) => b.error = w.u64().is_some_and(|e| e != 0),
                (Form::Inline, (1, w)) => b.service = w.str().unwrap_or(""),
                (Form::Indexed, (9, w)) => {
                    let (mut k, mut v) = ("", "");
                    for g in Fields::new(w.bytes().unwrap_or(&[])) {
                        match g? {
                            (1, x) => k = self.string(x),
                            (2, x) => {
                                for h in Fields::new(x.bytes().unwrap_or(&[])) {
                                    if let (1, y) = h? {
                                        v = self.string(y);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    details |= error_detail(k, v);
                }
                (Form::Inline, (10, w)) => {
                    let (k, v) = entry(w.bytes().unwrap_or(&[]))?;
                    details |= error_detail(k, v);
                }
                _ => {}
            }
        }
        b.detailed = details == 7;
        Ok(b)
    }

    /// An indexed span's fields, its attributes into `tags`.
    fn indexed_span(&mut self, msg: &'a [u8], trace: Option<[u8; 16]>, text_at: usize) -> Result<Span<'a>, OtlpError> {
        self.text.truncate(text_at);
        self.tags.clear();
        self.parts.clear();
        let mut s = Span { trace, ..Span::default() };
        let mut tags = std::mem::take(&mut self.tags);
        for f in Fields::new(msg) {
            match f? {
                (1, w) => s.service = self.string(w),
                (2, w) => s.name = self.string(w),
                (3, w) => s.resource = self.string(w),
                (4, w) => s.span = w.u64().and_then(id64),
                (5, w) => s.parent = w.u64().unwrap_or(0),
                (6, w) => s.start = w.u64().unwrap_or(0),
                (7, w) => s.duration = w.u64().unwrap_or(0),
                (8, w) => s.error = w.u64().is_some_and(|b| b != 0),
                (9, w) => {
                    if let Some(a) = self.indexed_attr(w.bytes().unwrap_or(&[]))? {
                        tags.push(a);
                    }
                }
                (10, w) => s.ty = self.string(w),
                (11, w) => self.parts.push((LINK, w.bytes().unwrap_or(&[]))),
                (12, w) => self.parts.push((EVENT, w.bytes().unwrap_or(&[]))),
                (13, w) => s.env = self.string(w),
                (14, w) => s.version = self.string(w),
                (15, w) => s.component = self.string(w),
                (16, w) => s.kind = w.u64().unwrap_or(0),
                _ => {}
            }
        }
        self.tags = tags;
        Ok(s)
    }

    /// An inline span's fields, its `meta` and `metrics` into `tags`.
    fn inline_span(&mut self, msg: &'a [u8], hi: u64, text_at: usize) -> Result<Span<'a>, OtlpError> {
        self.text.truncate(text_at);
        self.tags.clear();
        self.parts.clear();
        let (mut s, mut lo) = (Span::default(), 0u64);
        for f in Fields::new(msg) {
            match f? {
                (1, w) => s.service = w.str().unwrap_or(""),
                (2, w) => s.name = w.str().unwrap_or(""),
                (3, w) => s.resource = w.str().unwrap_or(""),
                (4, w) => lo = w.u64().unwrap_or(0),
                (5, w) => s.span = w.u64().and_then(id64),
                (6, w) => s.parent = w.u64().unwrap_or(0),
                (7, w) => s.start = w.i64().and_then(|t| u64::try_from(t).ok()).unwrap_or(0),
                (8, w) => s.duration = w.i64().and_then(|t| u64::try_from(t).ok()).unwrap_or(0),
                (9, w) => s.error = w.u64().is_some_and(|e| e != 0),
                (10, w) => {
                    let (k, v) = entry(w.bytes().unwrap_or(&[]))?;
                    if !k.is_empty() {
                        self.tags.push(Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Str(cut(v, MAX_VALUE_BYTES)) });
                    }
                }
                (11, w) => {
                    let (mut k, mut v) = ("", f64::NAN);
                    for g in Fields::new(w.bytes().unwrap_or(&[])) {
                        match g? {
                            (1, x) => k = x.str().unwrap_or(""),
                            (2, x) => v = x.f64().unwrap_or(f64::NAN),
                            _ => {}
                        }
                    }
                    if !k.is_empty() {
                        let at = self.text.len();
                        push_f64(&mut self.text, v);
                        self.tags.push(Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Text(at, self.text.len()) });
                    }
                }
                (12, w) => s.ty = w.str().unwrap_or(""),
                (14, w) => self.parts.push((LINK, w.bytes().unwrap_or(&[]))),
                (15, w) => self.parts.push((EVENT, w.bytes().unwrap_or(&[]))),
                _ => {}
            }
        }
        s.trace = id128(hi, lo);
        Ok(s)
    }

    /// A span's row: kiln's own attributes, the span's, its chunk's and payload's, what kiln adds.
    #[allow(clippy::too_many_arguments)]
    fn row(&mut self, s: &Span<'a>, form: Form, pl: &Payload<'a>, agent: &Agent<'a>, now_ns: i64, issue: bool, out: &mut Spans) -> Result<(), OtlpError> {
        let (Some(trace), Some(span)) = (s.trace, s.span) else {
            out.invalid += 1;
            return Ok(());
        };
        let tag_kind = || find(&self.tags, "span.kind").map(|v| resolve(v, &self.text).trim());
        let kind: &'static str = match s.kind {
            k @ 1..=5 => usize::try_from(k).ok().and_then(|k| KINDS.get(k)).copied().unwrap_or("internal"),
            _ => tag_kind().and_then(|t| KINDS.iter().find(|k| k.eq_ignore_ascii_case(t)).copied()).unwrap_or("internal"),
        };
        let component = if s.component.is_empty() { find(&self.tags, "component").filter(|v| !resolve(*v, &self.text).is_empty()) } else { Some(Val::Str(s.component)) };
        self.attrs.clear();
        self.attrs.push(Attr { key: "span.kind", val: Val::Str(kind) });
        let at = self.text.len();
        let _ = write!(self.text, "{}", s.duration);
        self.attrs.push(Attr { key: "duration", val: Val::Text(at, self.text.len()) });
        if s.parent != 0 {
            let at = self.text.len();
            push_hex(&mut self.text, &s.parent.to_be_bytes());
            self.attrs.push(Attr { key: "parent_id", val: Val::Text(at, self.text.len()) });
        }
        for (key, v) in [("operation_name", s.name), ("span.type", s.ty)] {
            if !v.is_empty() {
                self.attrs.push(Attr { key, val: Val::Str(cut(v, MAX_VALUE_BYTES)) });
            }
        }
        if let Some(c) = component {
            self.attrs.push(Attr { key: "component", val: c });
        }
        let own = self.attrs.len();
        for i in 0..self.tags.len() {
            let Some(&a) = self.tags.get(i) else { continue };
            let attrs = &mut self.attrs;
            let mut put = |a: Attr<'a>| {
                if !FIXED.contains(&a.key) {
                    attrs.push(a);
                }
            };
            if !unpacked(a, &mut put) {
                put(a);
            }
        }
        merge_under(&mut self.attrs, own, MAX_TAGS - OWN, &[self.chunk.as_slice(), self.payload.as_slice()], &|k| FIXED.contains(&k), &mut out.dropped_attrs);
        // Where the span does not say them: the unified service tags, the error, the status, the
        // peer.
        let has = |attrs: &[Attr<'_>], k: &str| attrs.iter().any(|a| a.key == k);
        if !has(&self.attrs, "env")
            && let Some(e) = [s.env, pl.env, agent.env].into_iter().find(|e| !e.is_empty())
        {
            self.attrs.push(Attr { key: "env", val: Val::Str(cut(e, MAX_VALUE_BYTES)) });
        }
        // A span's own version only: a tracer sets none on a span of a service not its own.
        if !has(&self.attrs, "version") && !s.version.is_empty() {
            self.attrs.push(Attr { key: "version", val: Val::Str(cut(s.version, MAX_VALUE_BYTES)) });
        }
        if !has(&self.attrs, "error.message")
            && let Some(v) = find(&self.attrs, "error.msg")
        {
            self.attrs.push(Attr { key: "error.message", val: v });
        }
        if !has(&self.attrs, "http.status_code")
            && let Some(v) = find(&self.attrs, "http.response.status_code")
        {
            self.attrs.push(Attr { key: "http.status_code", val: v });
        }
        if matches!(kind, "client" | "producer")
            && !has(&self.attrs, "peer.service")
            && let Some(v) = PEER_SOURCES.iter().find_map(|k| find(&self.attrs, k).filter(|v| !resolve(*v, &self.text).is_empty()))
        {
            self.attrs.push(Attr { key: "peer.service", val: v });
        }
        self.events_and_error(form, s.start)?;
        self.links(form)?;
        // An error's issue (`crate::errors`): its service's segment's, where this span carries it
        // ([`carriers`]).
        if s.error && issue {
            put_issue(&mut self.attrs, &mut self.text, Some(Val::Str(s.service)), true);
        }
        // The row.
        let ts = if s.start != 0 { i64::try_from(s.start).unwrap_or(i64::MAX) } else { now_ns };
        let host = find(&self.tags, "_dd.hostname").map(|v| resolve(v, &self.text)).filter(|h| !h.is_empty()).unwrap_or(if pl.host.is_empty() { agent.host } else { pl.host });
        let severity = if s.error { SEVERITY_ERROR } else { SEVERITY_OK };
        let name = if s.resource.is_empty() { s.name } else { s.resource };
        write_row(&mut self.seg, self.spare, out, &mut self.attrs, &self.text, ts, severity, [s.service, host, SOURCE], name, trace, span);
        Ok(())
    }

    /// An attribute's value in an event's or a link's JSON, rendered into `tmp`.
    fn value_into_tmp(&mut self, form: Form, v: &[u8]) -> Result<(), OtlpError> {
        self.tmp.clear();
        match form {
            Form::Indexed => indexed_value_push(v, &self.strings, MAX_VALUE_BYTES, &mut self.tmp),
            Form::Inline => inline_value_push(v, MAX_VALUE_BYTES, &mut self.tmp),
        }
    }

    /// An event's attribute's value if it is a string.
    fn string_value(&self, form: Form, v: &'a [u8]) -> Option<&'a str> {
        match form {
            Form::Indexed => Fields::new(v).find_map(|f| match f {
                Ok((1, w)) => Some(self.string(w)),
                _ => None,
            }),
            Form::Inline => {
                let (mut ty, mut s) = (0, None);
                for f in Fields::new(v).flatten() {
                    match f {
                        (1, w) => ty = w.u64().unwrap_or(0),
                        (2, w) => s = w.str(),
                        _ => {}
                    }
                }
                s.filter(|_| ty == 0)
            }
        }
    }

    /// The span's events as `events` (JSON: each one's name, its time from the span's start, its
    /// attributes), and its error as `error.type`, `error.message` and `error.stack` from its first
    /// exception event: each where the span has none.
    fn events_and_error(&mut self, form: Form, start: u64) -> Result<(), OtlpError> {
        let (mut ty, mut message, mut stack) = (None, None, None);
        let at = self.text.len();
        let (mut any, mut excepted) = (false, false);
        for i in 0..self.parts.len() {
            let Some(&(EVENT, ev)) = self.parts.get(i) else { continue };
            let (mut time, mut name) = (0u64, "");
            for f in Fields::new(ev) {
                match f? {
                    (1, w) => time = w.u64().unwrap_or(0),
                    (2, w) => name = self.name_of(form, w),
                    _ => {}
                }
            }
            let exception = name == "exception" && !excepted;
            excepted |= exception;
            let before = self.text.len();
            self.text.push(if any { ',' } else { '[' });
            self.text.push_str("{\"name\":");
            push_json_str(&mut self.text, cut(name, MAX_NAME_BYTES));
            let _ = write!(self.text, ",\"offset_ns\":{}", i128::from(time) - i128::from(start));
            self.text.push_str(",\"attributes\":{");
            let mut first = true;
            for f in Fields::new(ev) {
                let (3, kv) = f? else { continue };
                let (mut k, mut v) = ("", None);
                for f in Fields::new(kv.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => k = self.name_of(form, w),
                        (2, w) => v = w.bytes(),
                        _ => {}
                    }
                }
                if k.is_empty() {
                    continue;
                }
                self.value_into_tmp(form, v.unwrap_or(&[]))?;
                if !first {
                    self.text.push(',');
                }
                first = false;
                push_json_str(&mut self.text, cut(k, MAX_NAME_BYTES));
                self.text.push(':');
                push_json_str(&mut self.text, &self.tmp);
                if exception {
                    let slot = match k {
                        "exception.type" => &mut ty,
                        "exception.message" => &mut message,
                        "exception.stacktrace" => &mut stack,
                        _ => continue,
                    };
                    *slot = v.and_then(|v| self.string_value(form, v));
                }
            }
            self.text.push_str("}}");
            any = true;
            if self.text.len() - at > MAX_VALUE_BYTES {
                self.text.truncate(before);
                any = before > at;
                break;
            }
        }
        let mut events = None;
        if any {
            self.text.push(']');
            events = Some(Val::Text(at, self.text.len()));
        }
        for (key, v) in [("error.type", ty), ("error.message", message), ("error.stack", stack)] {
            if let Some(v) = v
                && !self.attrs.iter().any(|a| a.key == key)
            {
                self.attrs.push(Attr { key, val: Val::Str(cut(v, MAX_VALUE_BYTES)) });
            }
        }
        if let Some(e) = events
            && !self.attrs.iter().any(|a| a.key == "events")
        {
            self.attrs.push(Attr { key: "events", val: e });
        }
        Ok(())
    }

    /// The span's links as `links` (JSON: each one's trace id, span id and attributes).
    fn links(&mut self, form: Form) -> Result<(), OtlpError> {
        let at = self.text.len();
        let mut any = false;
        for i in 0..self.parts.len() {
            let Some(&(LINK, link)) = self.parts.get(i) else { continue };
            let (mut trace, mut span, mut hi, mut lo) = (None, None, 0u64, 0u64);
            for f in Fields::new(link) {
                match (form, f?) {
                    (Form::Indexed, (1, w)) => trace = id::<16>(w.bytes()),
                    (Form::Indexed, (2, w)) | (Form::Inline, (3, w)) => span = w.u64().and_then(id64),
                    (Form::Inline, (1, w)) => lo = w.u64().unwrap_or(0),
                    (Form::Inline, (2, w)) => hi = w.u64().unwrap_or(0),
                    _ => {}
                }
            }
            if form == Form::Inline {
                trace = id128(hi, lo);
            }
            let (Some(trace), Some(span)) = (trace, span) else { continue };
            let before = self.text.len();
            self.text.push(if any { ',' } else { '[' });
            self.text.push_str("{\"trace_id\":\"");
            push_hex(&mut self.text, &trace);
            self.text.push_str("\",\"span_id\":\"");
            push_hex(&mut self.text, &span);
            self.text.push_str("\",\"attributes\":{");
            let mut first = true;
            let attributes = if form == Form::Indexed { 3 } else { 4 };
            for f in Fields::new(link) {
                let (n, kv) = f?;
                if n != attributes {
                    continue;
                }
                let (mut k, mut v) = ("", None);
                for f in Fields::new(kv.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => k = self.name_of(form, w),
                        (2, w) => v = Some(w),
                        _ => {}
                    }
                }
                if k.is_empty() {
                    continue;
                }
                match (form, v) {
                    (Form::Indexed, Some(w)) => self.value_into_tmp(form, w.bytes().unwrap_or(&[]))?,
                    (_, w) => {
                        self.tmp.clear();
                        self.tmp.push_str(cut(w.and_then(|w| w.str()).unwrap_or(""), MAX_VALUE_BYTES));
                    }
                }
                if !first {
                    self.text.push(',');
                }
                first = false;
                push_json_str(&mut self.text, cut(k, MAX_NAME_BYTES));
                self.text.push(':');
                push_json_str(&mut self.text, &self.tmp);
            }
            self.text.push_str("}}");
            any = true;
            if self.text.len() - at > MAX_VALUE_BYTES {
                self.text.truncate(before);
                any = before > at;
                break;
            }
        }
        if any && !self.attrs.iter().any(|a| a.key == "links") {
            self.text.push(']');
            self.attrs.push(Attr { key: "links", val: Val::Text(at, self.text.len()) });
        }
        Ok(())
    }
}

/// A string a tracer's MessagePack holds, as a string or as bytes (some tracers send them so); one
/// not UTF-8 is none.
fn text<'a>(r: &mut Reader<'a>) -> Result<&'a str, MsgpackError> {
    Ok(std::str::from_utf8(r.bin()?).unwrap_or(""))
}

/// What a tracer's span's first reading finds: what its chunk's keeping, its spans' tops and its
/// root are decided by.
#[derive(Debug, Default)]
struct Head<'a> {
    span: u64,
    parent: u64,
    service: &'a str,
    env: &'a str,
    error: bool,
    priority: Option<f64>,
    /// What its tracer sampled it at (`_sample_rate`, `_dd1.sr.rapre`): its weight one over them.
    rates: (Option<f64>, Option<f64>),
    /// The trace's high 64 bits (`_dd.p.tid`), 0 if not said.
    hi: u64,
    /// Which of its error's details it says: its type (1), message (2), stack (4).
    details: u8,
}

impl<'a> Head<'a> {
    fn meta(&mut self, k: &str, v: &'a str) {
        match k {
            "_dd.p.tid" => self.hi = hex64(v).unwrap_or(0),
            "env" => self.env = v,
            _ => self.details |= error_detail(k, v),
        }
    }

    fn brief(&self) -> Brief<'a> {
        Brief { span: self.span, parent: self.parent, service: self.service, error: self.error, detailed: self.details == 7 }
    }

    fn metric(&mut self, k: &str, v: f64) {
        match k {
            "_sampling_priority_v1" => self.priority = Some(v),
            "_sample_rate" => self.rates.0 = Some(v),
            "_dd1.sr.rapre" => self.rates.1 = Some(v),
            _ => {}
        }
    }

    /// One over what its tracer sampled it at, each rate taken only from (0, 1].
    fn weight(&self) -> f64 {
        let r = |v: Option<f64>| v.filter(|r| *r > 0.0 && *r <= 1.0).unwrap_or(1.0);
        1.0 / (r(self.rates.0) * r(self.rates.1))
    }
}

impl<'a> Cx<'a, '_> {
    /// A `/v0.5/traces` string by its place in the table.
    fn nth(&self, r: &mut Reader<'a>) -> Result<&'a str, MsgpackError> {
        Ok(usize::try_from(r.u64()?).ok().and_then(|i| self.strings.get(i)).copied().unwrap_or(""))
    }

    /// A tracer's payload: its traces, each counted and kept or not.
    #[allow(clippy::too_many_arguments)]
    fn tracer(
        &mut self,
        v5: bool,
        body: &'a [u8],
        t: &Tracer<'a>,
        now_ns: i64,
        out: &mut Spans,
        stats: &mut Vec<SpanStat<'a>>,
        roots: &mut Vec<Root<'a>>,
        taken: &mut TracerTaken,
    ) -> Result<(), DdError> {
        let mut r = Reader::new(body);
        self.strings.clear();
        if v5 {
            if r.array()? != 2 {
                return Err(MsgpackError::Unexpected { found: "an array not of two", wanted: "a table of strings and the traces" }.into());
            }
            for _ in 0..r.array()? {
                let s = text(&mut r)?;
                self.strings.push(s);
            }
        }
        self.text.clear();
        self.payload.clear();
        self.chunk.clear();
        for (key, v) in [("language", t.language), ("container_id", t.container)] {
            if !v.is_empty() {
                self.payload.push(Attr { key, val: Val::Str(cut(v, MAX_VALUE_BYTES)) });
            }
        }
        let (pl, agent) = (Payload { host: t.host, env: "" }, Agent { host: t.host, env: "" });
        for _ in 0..r.array()? {
            taken.traces += 1;
            // The trace's spans' ids and services, its priority, its high bits, whether it erred.
            let at = r.pos();
            let n = r.array()?;
            self.ids.clear();
            self.briefs.clear();
            let (mut priority, mut erred, mut hi) = (None, false, 0u64);
            // The trace's root: its first span of no parent, else its first.
            let mut root: Option<(bool, Root<'a>)> = None;
            for _ in 0..n {
                let h = self.head(&mut r, v5)?;
                self.ids.push((h.span, h.service));
                self.briefs.push(h.brief());
                priority = priority.or(h.priority);
                erred |= h.error;
                if hi == 0 {
                    hi = h.hi;
                }
                let parentless = h.parent == 0;
                if root.is_none_or(|(was, _)| parentless && !was) {
                    root = Some((parentless, Root { service: h.service, env: h.env, priority: None, weight: h.weight() }));
                }
            }
            if let Some((_, mut x)) = root {
                x.priority = priority;
                roots.push(x);
            }
            self.ids.sort_unstable_by_key(|x| x.0);
            carriers(&mut self.briefs, &mut self.carriers);
            // Dropped by its tracer: a user's rejection, or an automatic one with no error in it
            // (the Agent's error sampler keeps those; kiln keeps every one).
            let keep = match priority {
                Some(p) if p < 0.0 => false,
                Some(p) if p < 0.5 => erred,
                _ => true,
            };
            taken.dropped += u64::from(!keep);
            let mut again = r.at(at);
            again.array()?;
            for _ in 0..n {
                let (span, marked) = self.tracer_span(&mut again, v5, hi)?;
                let top = self.top_level(&span, marked);
                if let Some(st) = self.stat_of(&span, top) {
                    stats.push(st);
                }
                if keep {
                    let issue = self.carries(&span);
                    self.row(&span, Form::Inline, &pl, &agent, now_ns, issue, out)?;
                }
            }
        }
        Ok(())
    }

    /// A span's id, service, error, priority and trace's high bits, the rest passed over.
    fn head(&self, r: &mut Reader<'a>, v5: bool) -> Result<Head<'a>, MsgpackError> {
        let mut h = Head::default();
        if v5 {
            if r.array()? != 12 {
                return Err(MsgpackError::Unexpected { found: "a span not of twelve", wanted: "a v0.5 span" });
            }
            h.service = self.nth(r)?;
            r.skip()?;
            r.skip()?;
            r.skip()?;
            h.span = r.u64()?;
            h.parent = r.u64()?;
            r.skip()?;
            r.skip()?;
            h.error = r.i64()? != 0;
            for _ in 0..r.map()? {
                let k = self.nth(r)?;
                let v = self.nth(r)?;
                h.meta(k, v);
            }
            for _ in 0..r.map()? {
                let k = self.nth(r)?;
                let v = r.f64()?;
                h.metric(k, v);
            }
            r.skip()?;
            return Ok(h);
        }
        for _ in 0..r.map()? {
            match text(r)? {
                "service" => h.service = text(r)?,
                "span_id" => h.span = r.u64()?,
                "parent_id" => h.parent = r.u64()?,
                "error" => h.error = r.i64()? != 0,
                "meta" => {
                    for _ in 0..r.map()? {
                        let k = text(r)?;
                        let v = text(r)?;
                        h.meta(k, v);
                    }
                }
                "metrics" => {
                    for _ in 0..r.map()? {
                        let k = text(r)?;
                        let v = r.f64()?;
                        h.metric(k, v);
                    }
                }
                _ => r.skip()?,
            }
        }
        Ok(h)
    }

    /// A tracer's span's fields, its `meta` and `metrics` into `tags`; and whether its tracer
    /// marked it top-level (`_dd.top_level`, or the Agent's `_top_level`).
    fn tracer_span(&mut self, r: &mut Reader<'a>, v5: bool, hi: u64) -> Result<(Span<'a>, bool), DdError> {
        self.text.clear();
        self.tags.clear();
        self.parts.clear();
        let (mut s, mut lo, mut marked) = (Span::default(), 0u64, false);
        let meta = |cx: &mut Self, k: &'a str, v: &'a str| {
            if !k.is_empty() {
                cx.tags.push(Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Str(cut(v, MAX_VALUE_BYTES)) });
            }
        };
        let metric = |cx: &mut Self, k: &'a str, v: f64, marked: &mut bool| {
            if k.is_empty() {
                return;
            }
            *marked |= (k == "_dd.top_level" || k == "_top_level") && v == 1.0;
            let at = cx.text.len();
            push_f64(&mut cx.text, v);
            cx.tags.push(Attr { key: cut(k, MAX_NAME_BYTES), val: Val::Text(at, cx.text.len()) });
        };
        let time = |v: i64| u64::try_from(v).unwrap_or(0);
        if v5 {
            if r.array()? != 12 {
                return Err(MsgpackError::Unexpected { found: "a span not of twelve", wanted: "a v0.5 span" }.into());
            }
            s.service = self.nth(r)?;
            s.name = self.nth(r)?;
            s.resource = self.nth(r)?;
            lo = r.u64()?;
            s.span = id64(r.u64()?);
            s.parent = r.u64()?;
            s.start = time(r.i64()?);
            s.duration = time(r.i64()?);
            s.error = r.i64()? != 0;
            for _ in 0..r.map()? {
                let (k, v) = (self.nth(r)?, self.nth(r)?);
                meta(self, k, v);
            }
            for _ in 0..r.map()? {
                let k = self.nth(r)?;
                let v = r.f64()?;
                metric(self, k, v, &mut marked);
            }
            s.ty = self.nth(r)?;
        } else {
            for _ in 0..r.map()? {
                match text(r)? {
                    "service" => s.service = text(r)?,
                    "name" => s.name = text(r)?,
                    "resource" => s.resource = text(r)?,
                    "type" => s.ty = text(r)?,
                    "trace_id" => lo = r.u64()?,
                    "span_id" => s.span = id64(r.u64()?),
                    "parent_id" => s.parent = r.u64()?,
                    "start" => s.start = time(r.i64()?),
                    "duration" => s.duration = time(r.i64()?),
                    "error" => s.error = r.i64()? != 0,
                    "meta" => {
                        for _ in 0..r.map()? {
                            let (k, v) = (text(r)?, text(r)?);
                            meta(self, k, v);
                        }
                    }
                    "metrics" => {
                        for _ in 0..r.map()? {
                            let k = text(r)?;
                            let v = r.f64()?;
                            metric(self, k, v, &mut marked);
                        }
                    }
                    _ => r.skip()?,
                }
            }
        }
        s.trace = id128(hi, lo);
        Ok((s, marked))
    }

    /// Whether a tracer's span is top-level: marked so, or its parent none of its trace's, or
    /// another service's (the Agent's `traceutil.ComputeTopLevel`).
    fn top_level(&self, s: &Span<'a>, marked: bool) -> bool {
        marked
            || s.parent == 0
            || match self.ids.binary_search_by_key(&s.parent, |x| x.0) {
                Ok(i) => self.ids.get(i).is_some_and(|x| x.1 != s.service),
                Err(_) => true,
            }
    }

    /// A span as the concentrator counts it, if it does: a server's or a consumer's (an entry), a
    /// client's or a producer's (an exit), or another kind's that is top-level (`top_level`).
    fn stat_of(&self, s: &Span<'a>, top: bool) -> Option<SpanStat<'a>> {
        let tag = |k: &str| {
            self.tags.iter().find(|a| a.key == k).and_then(|a| match a.val {
                Val::Str(v) => Some(v),
                Val::Text(..) => None,
            })
        };
        let kind = tag("span.kind").and_then(|t| KINDS.iter().find(|k| k.eq_ignore_ascii_case(t.trim())).copied()).unwrap_or("internal");
        let outbound = match kind {
            "server" | "consumer" => false,
            "client" | "producer" => true,
            _ if top => false,
            _ => return None,
        };
        let third = if outbound { PEER_SOURCES.iter().find_map(|k| tag(k).filter(|v| !v.is_empty())).unwrap_or("") } else { tag("version").unwrap_or("") };
        Some(SpanStat {
            outbound,
            service: s.service,
            resource: if s.resource.is_empty() { s.name } else { s.resource },
            kind,
            env: tag("env").unwrap_or(""),
            third,
            status: tag("http.status_code").and_then(|c| c.trim().parse().ok()).unwrap_or(0),
            start_ns: i64::try_from(s.start).unwrap_or(i64::MAX),
            duration_ns: s.duration,
            error: s.error,
        })
    }
}

/// The high 64 bits of an inline chunk's trace id: the `_dd.p.tid` a tracer sets on one span of it
/// (its local root's).
fn trace_high(chunk: &[u8]) -> Result<Option<u64>, OtlpError> {
    for f in Fields::new(chunk) {
        let (3, span) = f? else { continue };
        for g in Fields::new(span.bytes().unwrap_or(&[])) {
            let (10, e) = g? else { continue };
            let (k, v) = entry(e.bytes().unwrap_or(&[]))?;
            if k == "_dd.p.tid" {
                return Ok(hex64(v));
            }
        }
    }
    Ok(None)
}

/// An indexed `AnyValue` as text, appended to `out` (at most `max` bytes of it): scalars as
/// themselves, bytes as hex, arrays and maps as compact JSON, as OTLP's are
/// (`crate::otlp::metrics::any_value_push`), nested at most [`MAX_DEPTH`] deep.
fn indexed_value_push(msg: &[u8], strings: &[&str], max: usize, out: &mut String) -> Result<(), OtlpError> {
    enum Frame<'a> {
        Array(Fields<'a>, bool),
        Map(Fields<'a>, bool),
    }
    let string = |w: Wire<'_>| w.u64().and_then(|i| usize::try_from(i).ok()).and_then(|i| strings.get(i)).copied().unwrap_or("");
    let start = out.len();
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut pending: Option<&[u8]> = Some(msg);
    loop {
        if let Some(v) = pending.take() {
            let mut done = false;
            for f in Fields::new(v) {
                match f? {
                    (1, w) => {
                        if stack.is_empty() {
                            out.push_str(string(w));
                        } else {
                            push_json_str(out, string(w));
                        }
                        done = true;
                    }
                    (2, w) => {
                        out.push_str(if w.u64().is_some_and(|b| b != 0) { "true" } else { "false" });
                        done = true;
                    }
                    (3, w) => {
                        push_f64(out, w.f64().unwrap_or(f64::NAN));
                        done = true;
                    }
                    (4, w) => {
                        let _ = write!(out, "{}", w.i64().unwrap_or(0));
                        done = true;
                    }
                    (5, w) => {
                        push_hex(out, w.bytes().unwrap_or(&[]));
                        done = true;
                    }
                    (6 | 7, _) if stack.len() >= MAX_DEPTH => {
                        out.push('…');
                        done = true;
                    }
                    (6, w) => {
                        out.push('[');
                        stack.push(Frame::Array(Fields::new(w.bytes().unwrap_or(&[])), false));
                        done = true;
                    }
                    (7, w) => {
                        out.push('{');
                        stack.push(Frame::Map(Fields::new(w.bytes().unwrap_or(&[])), false));
                        done = true;
                    }
                    _ => {}
                }
                if done {
                    break;
                }
            }
            if !done && !stack.is_empty() {
                out.push_str("null");
            }
            if out.len() - start > max {
                break;
            }
        }
        let Some(frame) = stack.last_mut() else { break };
        match frame {
            Frame::Array(fields, any) => match fields.next() {
                Some(f) => {
                    if let (1, w) = f? {
                        if *any {
                            out.push(',');
                        }
                        *any = true;
                        pending = Some(w.bytes().unwrap_or(&[]));
                    }
                }
                None => {
                    out.push(']');
                    stack.pop();
                }
            },
            Frame::Map(fields, any) => match fields.next() {
                Some(f) => {
                    if let (1, w) = f? {
                        let (mut k, mut v) = ("", None);
                        for f in Fields::new(w.bytes().unwrap_or(&[])) {
                            match f? {
                                (1, kw) => k = string(kw),
                                (2, vw) => v = vw.bytes(),
                                _ => {}
                            }
                        }
                        if *any {
                            out.push(',');
                        }
                        *any = true;
                        push_json_str(out, k);
                        out.push(':');
                        pending = Some(v.unwrap_or(&[]));
                    }
                }
                None => {
                    out.push('}');
                    stack.pop();
                }
            },
        }
    }
    cut_to(out, start, max);
    Ok(())
}

/// An inline span event's `AttributeAnyValue` as text (its `type` says which value it holds:
/// 0 a string, 1 a bool, 2 an int, 3 a double, 4 an array of those), at most `max` bytes.
fn inline_value_push(msg: &[u8], max: usize, out: &mut String) -> Result<(), OtlpError> {
    fn scalar(msg: &[u8], quoted: bool, out: &mut String) -> Result<(), OtlpError> {
        let (mut ty, mut s, mut b, mut i, mut d) = (0, "", false, 0i64, f64::NAN);
        for f in Fields::new(msg) {
            match f? {
                (1, w) => ty = w.u64().unwrap_or(0),
                (2, w) => s = w.str().unwrap_or(""),
                (3, w) => b = w.u64().is_some_and(|b| b != 0),
                (4, w) => i = w.i64().unwrap_or(0),
                (5, w) => d = w.f64().unwrap_or(f64::NAN),
                _ => {}
            }
        }
        match ty {
            1 => out.push_str(if b { "true" } else { "false" }),
            2 => {
                let _ = write!(out, "{i}");
            }
            3 => push_f64(out, d),
            _ if quoted => push_json_str(out, s),
            _ => out.push_str(s),
        }
        Ok(())
    }
    let start = out.len();
    let (mut ty, mut array) = (0, None);
    for f in Fields::new(msg) {
        match f? {
            (1, w) => ty = w.u64().unwrap_or(0),
            (6, w) => array = w.bytes(),
            _ => {}
        }
    }
    if ty == 4 {
        out.push('[');
        let mut any = false;
        for f in Fields::new(array.unwrap_or(&[])) {
            let (1, w) = f? else { continue };
            if any {
                out.push(',');
            }
            any = true;
            scalar(w.bytes().unwrap_or(&[]), true, out)?;
            if out.len() - start > max {
                break;
            }
        }
        out.push(']');
    } else {
        scalar(msg, false, out)?;
    }
    cut_to(out, start, max);
    Ok(())
}

/// What was appended to `out` past `start`, at most `max` bytes, cut where a character starts.
fn cut_to(out: &mut String, start: usize, max: usize) {
    if out.len() - start > max {
        let mut at = start + max;
        while !out.is_char_boundary(at) {
            at -= 1;
        }
        out.truncate(at);
    }
}

// ---------------------------------------------------------------------------------------------
// Stats.

/// What a `StatsPayload` came to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatsTaken {
    /// Grouped stats counted: of a service's entries or of its exits.
    pub groups: u64,
    /// Grouped stats of spans inside a service: passed over.
    pub inner: u64,
    /// Sketches kiln cannot read (a mapping it does not know): their groups' counts are taken,
    /// their durations' buckets not.
    pub unread: u64,
}

/// A sketch's index mapping (`sketches-go`'s `mapping`, `sketches-java`'s `LogLikeIndexMapping`):
/// bin `i` holds the values from `lower(i)` to `lower(i + 1)`, and is read as `lower(i)·(1 + α)`,
/// within `α` of each (its relative accuracy).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    offset: f64,
    /// Bins an octave (or an e-fold, logarithmic) of the base's logarithm: `ln base / ln γ`.
    multiplier: f64,
    alpha: f64,
    interpolation: u64,
}

/// The cubic interpolation's coefficients (`sketches-go`'s `CubicallyInterpolatedMapping`).
const CUBIC_A: f64 = 6.0 / 35.0;
const CUBIC_B: f64 = -3.0 / 5.0;
const CUBIC_C: f64 = 10.0 / 7.0;

impl Mapping {
    /// `IndexMapping`'s γ, offset and interpolation (0 none, the logarithm itself; 1 linear, 2
    /// quadratic, 3 cubic: the base-2 logarithm interpolated between powers of two, as the float's
    /// bits give them). None for a γ not above 1, or an interpolation not known.
    pub fn new(gamma: f64, offset: f64, interpolation: u64) -> Option<Self> {
        if !(gamma > 1.0 && gamma.is_finite() && offset.is_finite()) {
            return None;
        }
        let ln2 = std::f64::consts::LN_2;
        // The base's logarithm, and the correcting factor that gives the relative accuracy
        // (`LogLikeIndexMapping`: α = (γ^c - 1)/(γ^c + 1)).
        let (ln_base, c) = match interpolation {
            0 => (1.0, 1.0),
            1 => (ln2, 1.0 / ln2),
            2 => (ln2, 3.0 / (4.0 * ln2)),
            3 => (ln2, 7.0 / (10.0 * ln2)),
            _ => return None,
        };
        let g = gamma.powf(c);
        Some(Self { offset, multiplier: ln_base / gamma.ln(), alpha: (g - 1.0) / (g + 1.0), interpolation })
    }

    /// Its relative accuracy.
    pub fn alpha(&self) -> f64 {
        self.alpha
    }

    /// The least value bin `index` holds.
    pub fn lower(&self, index: i64) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let x = (index as f64 - self.offset) / self.multiplier;
        if self.interpolation == 0 {
            return x.exp();
        }
        let e = x.floor();
        let t = x - e;
        let significand = match self.interpolation {
            1 => t + 1.0,
            2 => 3.0 - (4.0 - 3.0 * t).sqrt(),
            _ => {
                // The cubic's inverse by Cardano's formula, as sketches-go has it.
                let d0 = CUBIC_B * CUBIC_B - 3.0 * CUBIC_A * CUBIC_C;
                let d1 = 2.0 * CUBIC_B * CUBIC_B * CUBIC_B - 9.0 * CUBIC_A * CUBIC_B * CUBIC_C - 27.0 * CUBIC_A * CUBIC_A * t;
                let p = ((d1 - (d1 * d1 - 4.0 * d0 * d0 * d0).sqrt()) / 2.0).cbrt();
                -(CUBIC_B + p + d0 / p) / (3.0 * CUBIC_A) + 1.0
            }
        };
        build_f64(e, significand)
    }

    /// The value bin `index` is read as.
    pub fn value(&self, index: i64) -> f64 {
        self.lower(index) * (1.0 + self.alpha)
    }
}

/// `2^exponent · significand` (the significand in [1, 2)), as its bits: what sketches-go's
/// `buildFloat64` makes; past a normal float's exponents, zero or infinity.
fn build_f64(exponent: f64, significand: f64) -> f64 {
    const BIAS: f64 = 1023.0;
    let biased = exponent + BIAS;
    if biased < 1.0 {
        return 0.0;
    }
    if biased > 2046.0 {
        return f64::INFINITY;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bits = ((biased as u64) << 52) | (significand.to_bits() & 0x000f_ffff_ffff_ffff);
    f64::from_bits(bits)
}

/// A sketch's bins (`DDSketch`: field 1 its mapping, 2 its positive values' store, 4 its zeros'
/// count; a store's field 1 its bins as a map of index to count, 2 a run of counts from its field
/// 3's index), each as its value and count to `each`. False for a sketch of a mapping not known.
/// Negative values, which no duration is, are passed over.
fn sketch_bins(sketch: &[u8], counts: &mut Vec<f64>, mut each: impl FnMut(f64, f64)) -> Result<bool, PbError> {
    if sketch.is_empty() {
        return Ok(true);
    }
    let (mut mapping, mut store, mut zero) = (None, None, 0.0);
    for f in Fields::new(sketch) {
        match f? {
            (1, w) => {
                let (mut gamma, mut offset, mut interpolation) = (0.0, 0.0, 0);
                for g in Fields::new(w.bytes().unwrap_or(&[])) {
                    match g? {
                        (1, x) => gamma = x.f64().unwrap_or(0.0),
                        (2, x) => offset = x.f64().unwrap_or(0.0),
                        (3, x) => interpolation = x.u64().unwrap_or(0),
                        _ => {}
                    }
                }
                mapping = Mapping::new(gamma, offset, interpolation);
            }
            (2, w) => store = w.bytes(),
            (4, w) => zero = w.f64().unwrap_or(0.0),
            _ => {}
        }
    }
    if zero > 0.0 {
        each(0.0, zero);
    }
    let Some(store) = store else { return Ok(true) };
    let Some(m) = mapping else { return Ok(false) };
    let mut start = 0i64;
    counts.clear();
    for f in Fields::new(store) {
        match f? {
            (1, w) => {
                let (mut index, mut count) = (0i64, 0.0);
                for g in Fields::new(w.bytes().unwrap_or(&[])) {
                    match g? {
                        (1, x) => index = x.sint().unwrap_or(0),
                        (2, x) => count = x.f64().unwrap_or(0.0),
                        _ => {}
                    }
                }
                if count > 0.0 {
                    each(m.value(index), count);
                }
            }
            (2, w) => doubles(&w, counts)?,
            (3, w) => start = w.sint().unwrap_or(0),
            _ => {}
        }
    }
    for (i, &count) in counts.iter().enumerate() {
        if count > 0.0 {
            each(m.value(start.saturating_add(i64::try_from(i).unwrap_or(i64::MAX))), count);
        }
    }
    Ok(true)
}

/// A series of trace metrics: what it is of, and its point's time (ms).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Group<'a> {
    outbound: bool,
    service: &'a str,
    resource: &'a str,
    kind: &'static str,
    env: &'a str,
    /// An entry's version, an exit's peer.
    third: &'a str,
    status: u64,
    host: &'a str,
    ms: i64,
}

/// A group's counts in a bucket.
#[derive(Debug)]
struct Acc {
    hits: f64,
    errors: f64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Acc {
    fn new() -> Self {
        Self { hits: 0.0, errors: 0.0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY }
    }
}

/// A payload's groups: each one's place in `accs`, and every group's buckets' counts in one list
/// (its place, the bucket), so a group allocates nothing of its own.
#[derive(Debug, Default)]
struct Groups<'a> {
    places: BTreeMap<Group<'a>, u32>,
    accs: Vec<Acc>,
    bins: Vec<(u32, Bucket, f64)>,
    /// A sketch's run of counts, read.
    counts: Vec<f64>,
}

/// What a client's payload's stats share.
struct Client<'a> {
    host: &'a str,
    env: &'a str,
    version: &'a str,
    service: &'a str,
}

/// The trace metrics' names: an entry's, then an exit's (`hits`, `errors`, and `duration`'s
/// `count`, `sum`, `min`, `max` and buckets).
const NAMES: [[&str; 7]; 2] = [
    ["trace.hits", "trace.errors", "trace.duration.count", "trace.duration.sum", "trace.duration.min", "trace.duration.max", "trace.duration.dist"],
    [
        "trace.outbound.hits",
        "trace.outbound.errors",
        "trace.outbound.duration.count",
        "trace.outbound.duration.sum",
        "trace.outbound.duration.min",
        "trace.outbound.duration.max",
        "trace.outbound.duration.dist",
    ],
];

/// A `StatsPayload` as kiln's trace metrics into `out` (cleared, its room kept), each series' key
/// found in or made into `keys`: a series seen before allocates nothing.
pub fn stats_into(body: &[u8], keys: &Keys, out: &mut Decoded) -> Result<StatsTaken, DdError> {
    out.clear();
    let mut taken = StatsTaken::default();
    let mut groups = Groups::default();
    // Where in its bucket this payload's points are: the same for the same payload, on any node.
    let crc = i64::from(crate::crc32c::checksum(body));
    let mut r = Reader::new(body);
    let (mut host, mut env, mut at) = ("", "", None);
    for _ in 0..r.map()? {
        match r.str()? {
            "AgentHostname" => host = r.str()?,
            "AgentEnv" => env = r.str()?,
            "Stats" => {
                at = Some(r.pos());
                r.skip()?;
            }
            _ => r.skip()?,
        }
    }
    if let Some(at) = at {
        let mut r = r.at(at);
        for _ in 0..r.array()? {
            client_stats(&mut r, host, env, crc, &mut groups, &mut taken)?;
        }
    }
    points_into(&mut groups, keys, out);
    Ok(taken)
}

fn client_stats<'a>(r: &mut Reader<'a>, agent_host: &'a str, agent_env: &'a str, crc: i64, groups: &mut Groups<'a>, taken: &mut StatsTaken) -> Result<(), DdError> {
    let mut c = Client { host: "", env: "", version: "", service: "" };
    let mut at = None;
    for _ in 0..r.map()? {
        match r.str()? {
            "Hostname" => c.host = r.str()?,
            "Env" => c.env = r.str()?,
            "Version" => c.version = r.str()?,
            "Service" => c.service = r.str()?,
            "Stats" => {
                at = Some(r.pos());
                r.skip()?;
            }
            _ => r.skip()?,
        }
    }
    if c.host.is_empty() {
        c.host = agent_host;
    }
    if c.env.is_empty() {
        c.env = agent_env;
    }
    let Some(at) = at else { return Ok(()) };
    let mut r = r.at(at);
    for _ in 0..r.array()? {
        let (mut start, mut duration, mut at) = (0u64, 0u64, None);
        for _ in 0..r.map()? {
            match r.str()? {
                "Start" => start = r.u64()?,
                "Duration" => duration = r.u64()?,
                "Stats" => {
                    at = Some(r.pos());
                    r.skip()?;
                }
                _ => r.skip()?,
            }
        }
        let Some(at) = at else { continue };
        // The bucket's start, and the payload's offset within it (ms).
        let width = i64::try_from(duration / 1_000_000).unwrap_or(i64::MAX).clamp(1, 10_000);
        let ms = i64::try_from(start / 1_000_000).unwrap_or(i64::MAX).saturating_add(crc.rem_euclid(width));
        let mut g = r.at(at);
        for _ in 0..g.array()? {
            grouped(&mut g, &c, ms, groups, taken)?;
        }
    }
    Ok(())
}

/// A peer tag's value of the source [`PEER_SOURCES`] puts first.
fn peer_of<'a>(r: &mut Reader<'a>) -> Result<&'a str, MsgpackError> {
    let mut best: Option<(usize, &'a str)> = None;
    for _ in 0..r.array()? {
        let t = r.str()?;
        if let Some((k, v)) = t.split_once(':')
            && !v.is_empty()
            && let Some(rank) = PEER_SOURCES.iter().position(|s| *s == k)
            && best.is_none_or(|(b, _)| rank < b)
        {
            best = Some((rank, v));
        }
    }
    Ok(best.map_or("", |(_, v)| v))
}

/// One grouped stat into its group (its points at `ms`): its kind says whether it is an entry's or
/// an exit's.
#[allow(clippy::cast_precision_loss)]
fn grouped<'a>(r: &mut Reader<'a>, c: &Client<'a>, ms: i64, groups: &mut Groups<'a>, taken: &mut StatsTaken) -> Result<(), DdError> {
    let (mut service, mut name, mut resource, mut kind, mut peer) = ("", "", "", "", "");
    let (mut status, mut hits, mut errors, mut duration, mut top) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let (mut ok, mut err): (&[u8], &[u8]) = (&[], &[]);
    for _ in 0..r.map()? {
        match r.str()? {
            "Service" => service = r.str()?,
            "Name" => name = r.str()?,
            "Resource" => resource = r.str()?,
            "HTTPStatusCode" => status = r.u64()?,
            "SpanKind" => kind = r.str()?,
            "Hits" => hits = r.u64()?,
            "Errors" => errors = r.u64()?,
            "Duration" => duration = r.u64()?,
            "OkSummary" => ok = r.bin()?,
            "ErrorSummary" => err = r.bin()?,
            "TopLevelHits" => top = r.u64()?,
            "PeerTags" => peer = peer_of(r)?,
            _ => r.skip()?,
        }
    }
    let kind: &'static str = KINDS.iter().find(|k| k.eq_ignore_ascii_case(kind.trim())).copied().unwrap_or("internal");
    let (outbound, share) = match kind {
        "server" | "consumer" => (false, 1.0),
        "client" | "producer" => (true, 1.0),
        // Another kind's: an entry's as far as its spans are top-level.
        _ if top > 0 && hits > 0 => (false, (top.min(hits) as f64) / hits as f64),
        _ => {
            taken.inner += 1;
            return Ok(());
        }
    };
    if hits == 0 {
        return Ok(());
    }
    taken.groups += 1;
    let key = Group {
        outbound,
        service: if service.is_empty() { c.service } else { service },
        resource: if resource.is_empty() { name } else { resource },
        kind,
        env: c.env,
        third: if outbound { peer } else { c.version },
        status: if outbound { 0 } else { status },
        host: c.host,
        ms,
    };
    let next = u32::try_from(groups.accs.len()).unwrap_or(u32::MAX);
    let place = *groups.places.entry(key).or_insert(next);
    if place == next {
        groups.accs.push(Acc::new());
    }
    let Groups { accs, bins, counts, .. } = groups;
    let Some(acc) = accs.get_mut(place as usize) else { return Ok(()) };
    acc.hits += hits as f64 * share;
    acc.errors += errors as f64 * share;
    acc.sum += duration as f64 / 1e9 * share;
    for sketch in [ok, err] {
        let read = sketch_bins(sketch, counts, |ns, n| {
            let s = ns / 1e9;
            if let Some(b) = dist::bucket_of(s, dist::SCALE) {
                acc.min = acc.min.min(s);
                acc.max = acc.max.max(s);
                bins.push((place, b, n * share));
            }
        })?;
        if !read {
            taken.unread += 1;
        }
    }
    Ok(())
}

/// The groups' points into `out`: each group's tags (sorted by key, as [`Keys::key`] finds a key
/// by them) and its buckets' counts summed by bucket. The text of each status and bucket the
/// payload has is made once; a group's tags are on the stack, so a series seen before allocates
/// nothing.
fn points_into(groups: &mut Groups<'_>, keys: &Keys, out: &mut Decoded) {
    points_more(groups, keys, out);
}

/// [`points_into`]'s, appended to what `out` holds.
fn points_more(groups: &mut Groups<'_>, keys: &Keys, out: &mut Decoded) {
    use crate::logs::metrics::{NONE, tag_value};
    groups.bins.sort_unstable_by_key(|b| (b.0, b.1.order()));
    let mut statuses: BTreeMap<u64, String> = BTreeMap::new();
    for g in groups.places.keys().filter(|g| g.status != 0) {
        statuses.entry(g.status).or_insert_with(|| g.status.to_string());
    }
    let mut labels: BTreeMap<(u8, i64), String> = BTreeMap::new();
    for &(_, b, _) in &groups.bins {
        labels.entry(b.order()).or_insert_with(|| dist::tag_value(dist::SCALE, b));
    }
    // Each place's run of the sorted bins.
    let mut runs = vec![(0usize, 0usize); groups.accs.len()];
    let mut i = 0;
    while let Some(&(place, ..)) = groups.bins.get(i) {
        let n = groups.bins.get(i..).unwrap_or_default().iter().take_while(|b| b.0 == place).count().max(1);
        if let Some(r) = runs.get_mut(place as usize) {
            *r = (i, i + n);
        }
        i += n;
    }
    let mut key = Vec::new();
    for (g, &place) in &groups.places {
        let (Some(acc), Some(names)) = (groups.accs.get(place as usize), NAMES.get(usize::from(g.outbound))) else { continue };
        let (env, host, resource, service, third) = (tag_value(Some(g.env)), tag_value(Some(g.host)), tag_value(Some(g.resource)), tag_value(Some(g.service)), tag_value(Some(g.third)));
        let status = statuses.get(&g.status).map_or(NONE, String::as_str);
        let exit = [("env", env), ("host", host), ("peer.service", third), ("resource_name", resource), ("service", service), ("span.kind", g.kind)];
        let entry = [("env", env), ("host", host), ("http.status_code", status), ("resource_name", resource), ("service", service), ("span.kind", g.kind), ("version", third)];
        let tags: &[(&str, &str)] = if g.outbound { &exit } else { &entry };
        let mut put = |name: &str, tags: &[(&str, &str)], kind: Kind, v: f64| {
            if let Some(k) = keys.key(name, tags, kind, &mut key) {
                let at = u32::try_from(out.keys.len()).unwrap_or(u32::MAX);
                out.keys.push(k);
                out.samples.push((at, g.ms, v));
                out.points += 1;
            }
        };
        let [hits, errors, count, sum, min, max, dist_name] = *names;
        put(hits, tags, Kind::Count, acc.hits);
        if acc.errors > 0.0 {
            put(errors, tags, Kind::Count, acc.errors);
        }
        put(count, tags, Kind::Count, acc.hits);
        put(sum, tags, Kind::Count, acc.sum);
        if acc.min.is_finite() && acc.max.is_finite() {
            put(min, tags, Kind::Gauge, acc.min);
            put(max, tags, Kind::Gauge, acc.max);
        }
        // This group's buckets, its place's run: each one's counts summed, its tag first
        // (`dist_bucket` sorts before every other).
        let (from, to) = runs.get(place as usize).copied().unwrap_or_default();
        let mine = groups.bins.get(from..to).unwrap_or_default();
        let mut with = [("", ""); 8];
        for (slot, t) in with.iter_mut().skip(1).zip(tags) {
            *slot = *t;
        }
        let mut i = 0;
        while let Some(&(_, b, _)) = mine.get(i) {
            let run = mine.get(i..).unwrap_or_default().iter().take_while(|x| x.1 == b);
            let (same, total) = run.fold((0usize, 0.0), |(c, t), x| (c + 1, t + x.2));
            i += same.max(1);
            if let (Some(label), Some(first)) = (labels.get(&b.order()), with.first_mut()) {
                *first = (dist::BUCKET_TAG, label.as_str());
                put(dist_name, with.get(..=tags.len()).unwrap_or_default(), Kind::Count, total);
            }
        }
    }
}

/// A trace a tracer sent, as the sampler counts it: its root's service and env, its sampling
/// priority (none where it says none), its weight (one over what its tracer sampled it at, `_sample_rate`
/// and the pre-sampler's `_dd1.sr.rapre`, as the Agent's `weightRoot` reads them).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Root<'a> {
    pub service: &'a str,
    pub env: &'a str,
    pub priority: Option<f64>,
    pub weight: f64,
}

/// A bucket of the sampler's: the Agent's (`pkg/trace/sampler/coresampler.go`).
const SAMPLER_BUCKET_MS: i64 = 5_000;
/// Buckets it keeps: the busiest of them is a signature's rate (a moving max, against oscillation).
const SAMPLER_BUCKETS: usize = 6;
/// A rate rises at most this much an update (thirteen to rise tenfold, about a minute).
const MAX_RATE_INCREASE: f64 = 1.2;
/// Signatures kept at the most (OpenTelemetry's cardinality limit, as metrics from logs keep
/// groups): past it, a trace of a new one is counted with all, and given the default rate.
pub const SIGNATURES: usize = 2_000;

/// The Agent's priority sampler (`pkg/trace/sampler`), for tracers that send to kiln: each
/// signature's (a trace's root's service and env) traces counted in buckets of five seconds, six
/// kept; at each new bucket, a rate for each so that some `target` traces a second are kept in all:
/// the target spread evenly over the signatures, what a quiet one leaves spread over the rest
/// (water-filling), each against its busiest bucket; a rate rising at most 1.2 times an update; a
/// signature quiet for the whole window forgotten. A tracer applies a rate to the traces it decides
/// itself (priority 0 or 1), and only those are counted. A signature not known gets the rate the
/// whole traffic would, but never above the lowest rate given.
#[derive(Debug)]
pub struct Sampler {
    target: f64,
    /// Each signature's counts by bucket (`bucket % SAMPLER_BUCKETS`), and its rate.
    sigs: std::collections::HashMap<(String, String), ([f64; SAMPLER_BUCKETS], Option<f64>)>,
    all: [f64; SAMPLER_BUCKETS],
    bucket: i64,
    lowest: f64,
}

impl Sampler {
    /// A sampler keeping `target` traces a second (0: every trace, no rate below 1).
    pub fn new(target: f64) -> Self {
        Self { target: target.max(0.0), sigs: std::collections::HashMap::new(), all: [0.0; SAMPLER_BUCKETS], bucket: i64::MIN, lowest: 1.0 }
    }

    fn slot(b: i64) -> usize {
        usize::try_from(b.rem_euclid(SAMPLER_BUCKETS as i64)).unwrap_or(0)
    }

    /// A trace counted at `now_ms` (one its tracer decided itself, priority 0 or 1; others are
    /// not), the rates made again when a new bucket begins.
    pub fn count(&mut self, root: &Root<'_>, now_ms: i64) {
        if !matches!(root.priority, Some(p) if (0.0..=1.0).contains(&p)) {
            return;
        }
        let b = now_ms.div_euclid(SAMPLER_BUCKET_MS);
        if b != self.bucket {
            self.update(b);
        }
        let w = if root.weight.is_finite() && root.weight >= 1.0 { root.weight } else { 1.0 };
        if let Some(c) = self.all.get_mut(Self::slot(b)) {
            *c += w;
        }
        let key = (root.service.to_owned(), root.env.to_owned());
        let full = self.sigs.len() >= SIGNATURES;
        let entry = match self.sigs.get_mut(&key) {
            Some(e) => Some(e),
            None if full => None,
            None => Some(self.sigs.entry(key).or_insert(([0.0; SAMPLER_BUCKETS], None))),
        };
        if let Some((counts, _)) = entry
            && let Some(c) = counts.get_mut(Self::slot(b))
        {
            *c += w;
        }
    }

    /// The buckets between the last one and `to` emptied, each signature's rate made from the
    /// busiest of its buckets (`zeroAndGetMax`, `updateRates`).
    fn update(&mut self, to: i64) {
        let from = self.bucket;
        self.bucket = to;
        if from == i64::MIN {
            return;
        }
        // Each bucket past `from` up to `to`: a whole rotation or more empties them all; the new
        // one is emptied after its old count is read (the window's last).
        let rotate = |counts: &mut [f64; SAMPLER_BUCKETS]| -> f64 {
            let mut most = 0.0f64;
            for i in from.saturating_add(1)..=from.saturating_add(SAMPLER_BUCKETS as i64) {
                let slot = Self::slot(i);
                let Some(c) = counts.get_mut(slot) else { continue };
                if i < to {
                    *c = 0.0;
                    continue;
                }
                most = most.max(*c);
                if i == to {
                    *c = 0.0;
                }
            }
            most
        };
        let mut seen: Vec<((String, String), f64)> = Vec::with_capacity(self.sigs.len());
        for (sig, (counts, _)) in self.sigs.iter_mut() {
            seen.push((sig.clone(), rotate(counts) / (SAMPLER_BUCKET_MS as f64 / 1000.0)));
        }
        rotate(&mut self.all);
        let per_sig = tps_per_signature(self.target, seen.iter().map(|s| s.1));
        self.lowest = 1.0;
        for (sig, tps) in seen {
            let Some((_, rate)) = self.sigs.get_mut(&sig) else { continue };
            let mut r = if self.target > 0.0 && per_sig < tps && tps > 0.0 { per_sig / tps } else { 1.0 };
            if let Some(prev) = *rate
                && prev > 0.0
                && r / prev > MAX_RATE_INCREASE
            {
                r = prev * MAX_RATE_INCREASE;
            }
            let r = r.min(1.0);
            if r >= 1.0 && tps == 0.0 {
                // No traffic the whole window: forgotten.
                self.sigs.remove(&sig);
                continue;
            }
            self.lowest = self.lowest.min(r);
            *rate = Some(r);
        }
    }

    /// The rate for a signature not known: the whole traffic's against the target, never above the
    /// lowest given (`defaultRate`).
    pub fn default_rate(&self) -> f64 {
        if self.target <= 0.0 {
            return 1.0;
        }
        let seen = self.all.iter().copied().fold(0.0, f64::max) / (SAMPLER_BUCKET_MS as f64 / 1000.0);
        let rate = if self.target < seen && seen > 0.0 { self.target / seen } else { 1.0 };
        if self.lowest < rate && self.lowest > 0.0 { self.lowest } else { rate }
    }

    /// Each known signature's rate, as tracers read them (`rate_by_service`: `service:<s>,env:<e>`),
    /// and the default's (`service:,env:`).
    pub fn rates(&self) -> impl Iterator<Item = (String, f64)> + '_ {
        let known = self.sigs.iter().filter_map(|((service, env), (_, rate))| rate.map(|r| (format!("service:{service},env:{env}"), r)));
        known.chain(std::iter::once(("service:,env:".to_owned(), self.default_rate())))
    }

    /// Signatures counted.
    pub fn signatures(&self) -> usize {
        self.sigs.len()
    }
}

/// The traces a second each signature may keep (`computeTPSPerSig`): `target` spread evenly, what a
/// signature seen below its share leaves spread over the rest.
fn tps_per_signature(target: f64, seen: impl Iterator<Item = f64>) -> f64 {
    let mut sorted: Vec<f64> = seen.collect();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return target;
    }
    let mut left = target;
    #[allow(clippy::cast_precision_loss)]
    let mut each = left / n as f64;
    for (i, &c) in sorted.iter().enumerate() {
        if c >= each || i == n - 1 {
            break;
        }
        left -= c;
        #[allow(clippy::cast_precision_loss)]
        let rest = (n - i - 1) as f64;
        each = left / rest;
    }
    each
}

/// A bucket's group as the concentrator keeps it, its strings its own.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Kept {
    outbound: bool,
    service: String,
    resource: String,
    kind: &'static str,
    env: String,
    third: String,
    status: u64,
}

impl Kept {
    fn of(s: &SpanStat<'_>) -> Self {
        let own = |v: &str| tag_cut(v).to_owned();
        Self { outbound: s.outbound, service: own(s.service), resource: own(s.resource), kind: s.kind, env: own(s.env), third: own(s.third), status: s.status }
    }

    fn is(&self, s: &SpanStat<'_>) -> bool {
        self.outbound == s.outbound
            && self.kind == s.kind
            && self.status == s.status
            && self.service == tag_cut(s.service)
            && self.resource == tag_cut(s.resource)
            && self.env == tag_cut(s.env)
            && self.third == tag_cut(s.third)
    }
}

/// A group's hash: its kind of series, its status, its tags' values as a series keeps them.
fn group_hash(h: &crate::hash::Keyed, outbound: bool, status: u64, fields: [&str; 5]) -> u64 {
    fields.iter().fold(h.word(u64::from(outbound) ^ status.rotate_left(17)), |acc, v| h.word(acc ^ h.bytes(tag_cut(v).as_bytes())))
}

/// A tag's value as a series keeps it ([`crate::logs::metrics::TAG_CHARS`] at the most).
fn tag_cut(v: &str) -> &str {
    match v.char_indices().nth(crate::logs::metrics::TAG_CHARS) {
        Some((i, _)) => v.get(..i).unwrap_or(v),
        None => v,
    }
}

/// A bucket being counted.
#[derive(Debug, Default)]
struct Open {
    table: hashbrown::HashTable<u32>,
    groups: Vec<(Kept, Acc, Vec<(Bucket, f64)>)>,
    /// A group for spans past [`crate::logs::metrics::GROUPS`] groups (its tags `N/A`).
    overflow: Option<u32>,
}

/// The trace metrics of spans tracers sent kiln itself (kiln in their Agent's place), counted as
/// the Agent's concentrator counts them (`pkg/trace/stats`): in buckets of ten seconds of their
/// ends (the Agent's `start + duration`), each written once twenty seconds past its own end (the
/// Agent's buffer of two buckets), a span later than that (its bucket's wait over) counted in the
/// oldest bucket not yet due, as the Agent shifts it; a span ending more than a minute ahead of now
/// in the bucket of now. So nine buckets are open at the most, whatever times the spans say; each
/// series is written once a bucket by its one writer, this node (its `host`), and nothing is held
/// past half a minute. Its points are at their buckets' starts and an offset within the bucket drawn
/// as it starts, so a node started again writes a bucket it wrote before beside that write (the two
/// add) and not over it (a store keeps the later of two writes at one time). A bucket keeps
/// [`crate::logs::metrics::GROUPS`] groups at the most (OpenTelemetry's cardinality limit), past them
/// one whose tags are `N/A`.
#[derive(Debug)]
pub struct Concentrator {
    host: String,
    hash: crate::hash::Keyed,
    open: BTreeMap<i64, Open>,
    /// Buckets that begin before this (ms) are written.
    written: i64,
    /// Where in its bucket each point is (ms), drawn as it starts (the operating system's
    /// randomness: two starts of one node share one a time in ten thousand).
    offset: i64,
}

/// A bucket's width (ms): the Agent's.
pub const BUCKET_MS: i64 = 10_000;
/// How long past its end a bucket waits for its spans: the Agent's two buckets.
pub const WAIT_MS: i64 = 20_000;
/// How far ahead of now a span's end is taken as it says.
const AHEAD_MS: i64 = 60_000;

impl Concentrator {
    /// A concentrator of this node (`host`, the `host` its series carry).
    pub fn new(host: &str) -> Self {
        let offset = std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), host) % BUCKET_MS.unsigned_abs();
        Self { host: host.to_owned(), hash: crate::hash::Keyed::new(), open: BTreeMap::new(), written: i64::MIN, offset: i64::try_from(offset).unwrap_or(0) }
    }

    /// Buckets open.
    pub fn open(&self) -> usize {
        self.open.len()
    }

    /// Where in its bucket each point is written (ms past the bucket's start).
    pub fn offset(&self) -> i64 {
        self.offset
    }

    /// The oldest bucket a span is counted in at `now_ms`: the first neither due nor written.
    fn oldest(&self, now_ms: i64) -> i64 {
        let due = now_ms.saturating_sub(BUCKET_MS + WAIT_MS);
        (due - due.rem_euclid(BUCKET_MS)).saturating_add(BUCKET_MS).max(self.written)
    }

    /// One span counted, at `now_ms`.
    #[allow(clippy::cast_precision_loss)]
    pub fn add(&mut self, s: &SpanStat<'_>, now_ms: i64) {
        let end = s.start_ns.saturating_add(i64::try_from(s.duration_ns).unwrap_or(i64::MAX)).div_euclid(1_000_000);
        let mut at = if end > now_ms.saturating_add(AHEAD_MS) { now_ms } else { end };
        at -= at.rem_euclid(BUCKET_MS);
        // Late (its bucket's wait over, written or not): the oldest not yet due.
        at = at.max(self.oldest(now_ms));
        let h = group_hash(&self.hash, s.outbound, s.status, [s.service, s.resource, s.env, s.third, s.kind]);
        let open = self.open.entry(at).or_default();
        let found = open.table.find(h, |&i| open.groups.get(i as usize).is_some_and(|(k, ..)| k.is(s))).copied();
        let place = match found {
            Some(i) => i,
            None if open.groups.len() >= crate::logs::metrics::GROUPS => match open.overflow {
                Some(i) => i,
                None => {
                    let i = u32::try_from(open.groups.len()).unwrap_or(u32::MAX);
                    open.groups.push((
                        Kept { outbound: s.outbound, service: String::new(), resource: String::new(), kind: s.kind, env: String::new(), third: String::new(), status: 0 },
                        Acc::new(),
                        Vec::new(),
                    ));
                    open.overflow = Some(i);
                    i
                }
            },
            None => {
                let i = u32::try_from(open.groups.len()).unwrap_or(u32::MAX);
                open.groups.push((Kept::of(s), Acc::new(), Vec::new()));
                let hash = &self.hash;
                open.table.insert_unique(h, i, |&j| open.groups.get(j as usize).map_or(0, |(k, ..)| group_hash(hash, k.outbound, k.status, [&k.service, &k.resource, &k.env, &k.third, k.kind])));
                i
            }
        };
        if let Some((_, acc, bins)) = open.groups.get_mut(place as usize) {
            let d = s.duration_ns as f64 / 1e9;
            acc.hits += 1.0;
            acc.errors += f64::from(u8::from(s.error));
            acc.sum += d;
            if let Some(b) = dist::bucket_of(d, dist::SCALE) {
                acc.min = acc.min.min(d);
                acc.max = acc.max.max(d);
                match bins.iter_mut().find(|(x, _)| *x == b) {
                    Some((_, n)) => *n += 1.0,
                    None => bins.push((b, 1.0)),
                }
            }
        }
    }

    /// The buckets due at `now_ms` (twenty seconds past their ends; every one with `all`, as a node
    /// stops) written into `out` (cleared), each series' key found in or made into `keys`; how many.
    pub fn flush_into(&mut self, now_ms: i64, all: bool, keys: &Keys, out: &mut Decoded) -> usize {
        out.clear();
        let due: Vec<i64> = self.open.keys().copied().filter(|&at| all || at.saturating_add(BUCKET_MS + WAIT_MS) <= now_ms).collect();
        for &at in &due {
            let Some(open) = self.open.remove(&at) else { continue };
            self.written = self.written.max(at.saturating_add(BUCKET_MS));
            let mut groups = Groups::default();
            for (i, (k, acc, bins)) in open.groups.iter().enumerate() {
                let place = u32::try_from(i).unwrap_or(u32::MAX);
                let g = Group {
                    outbound: k.outbound,
                    service: &k.service,
                    resource: &k.resource,
                    kind: k.kind,
                    env: &k.env,
                    third: &k.third,
                    status: k.status,
                    host: &self.host,
                    ms: at.saturating_add(self.offset),
                };
                groups.places.insert(g, place);
                groups.accs.push(Acc { hits: acc.hits, errors: acc.errors, sum: acc.sum, min: acc.min, max: acc.max });
                groups.bins.extend(bins.iter().map(|&(b, n)| (place, b, n)));
            }
            points_more(&mut groups, keys, out);
        }
        due.len()
    }
}

/// A `StatsPayload`'s series, each with its points: [`stats_into`]'s, gathered by series.
pub fn stats(body: &[u8]) -> Result<(Vec<(SeriesKey, Vec<(i64, f64)>)>, StatsTaken), DdError> {
    let keys = Keys::new();
    let mut d = Decoded::default();
    let taken = stats_into(body, &keys, &mut d)?;
    let mut out: Vec<(SeriesKey, Vec<(i64, f64)>)> = d.keys.iter().cloned().map(|k| (k, Vec::new())).collect();
    for &(at, ts, v) in &d.samples {
        if let Some((_, p)) = out.get_mut(at as usize) {
            p.push((ts, v));
        }
    }
    Ok((out, taken))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{RecordRef, Rows};
    use crate::otlp::pb::Writer;

    /// What an Agent 7.84.4 sent of a ddtrace 4.16 Flask app's traces (`/api/v0.2/traces`,
    /// unzipped): its first trace, a request answered 200, and its first with an error, a 500
    /// (its database span raising), the rest of the ten it sent left out, its bytes as they came.
    const TRACES: &[u8] = include_bytes!("fixtures/datadog/traces-7.84.pb");
    /// The stats it sent of them (`/api/v0.2/stats`, unzipped).
    const STATS: &[u8] = include_bytes!("fixtures/datadog/stats-7.84.msgpack");

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

    fn span<'r>(rows: &'r [RecordRef<'r>], id: &str) -> &'r RecordRef<'r> {
        rows.iter().find(|r| r.span_id.map(|s| hex(&s)).as_deref() == Some(id)).unwrap()
    }

    #[test]
    fn an_agent_s_indexed_traces_read_as_the_spans_its_tracer_made() {
        let mut d = Decoder::new();
        let mut out = Spans::default();
        d.decode(TRACES, 0, &mut out).unwrap();
        assert_eq!((out.spans, out.invalid, out.cut), (22, 0, 0));
        let rows = rows(&out);
        // The request's own span: a server's, its trace's 128 bits as the chunk says them.
        let root = span(&rows, "dd0bda33a148579d");
        assert_eq!(hex(&root.trace_id.unwrap()), "6ad6d85600000000ac1c1714b3edf227");
        assert_eq!((root.ts, root.severity, root.service, root.host, root.source, root.message), (1_792_464_982_201_146_843, SEVERITY_OK, "shop", "kiln-apm-cap", SOURCE, "GET /cart/<int:n>"));
        for (k, v) in [
            ("span.kind", "server"),
            ("duration", "14707511"),
            ("operation_name", "flask.request"),
            ("span.type", "web"),
            ("component", "flask"),
            ("env", "e2e"),
            ("version", "1.2.3"),
            ("http.status_code", "200"),
            ("http.route", "/cart/<int:n>"),
            ("_sampling_priority_v1", "1"),
            ("flask.view_args.n", "0"),
            // Its payload's: the process's tags, each of them, and the language.
            ("entrypoint.name", "app"),
            ("svc.user", "true"),
            ("language", "python"),
        ] {
            assert_eq!(root.attr(k), Some(v), "{k}");
        }
        for gone in ["parent_id", "_dd.p.tid", "_dd.tags.process", "peer.service"] {
            assert_eq!(root.attr(gone), None, "{gone}");
        }
        // No name twice.
        let mut keys: Vec<&str> = root.attrs().map(|(k, _)| k).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n);
        // A span of a service the tracer named another (`shop-db`): its kind none said, so
        // internal; no version, as its tracer gave it none; its parent the handler's span.
        let db = span(&rows, "5dac8db6ef1dbebb");
        assert_eq!((db.service, db.message, db.severity), ("shop-db", "SELECT carts", SEVERITY_OK));
        for (k, v) in [
            ("span.kind", "internal"),
            ("span.type", "sql"),
            ("operation_name", "postgres.query"),
            ("parent_id", "ced8fd0db0f95f19"),
            ("db.system", "postgresql"),
            ("_dd.base_service", "shop"),
            ("env", "e2e"),
        ] {
            assert_eq!(db.attr(k), Some(v), "{k}");
        }
        assert_eq!(db.attr("version"), None);
        // The 500: its request's span an error, and the database span that raised, its error's
        // type, message and stack as the tracer set them.
        let failed = span(&rows, "9b132bbe41d2501c");
        assert_eq!((failed.severity, failed.attr("http.status_code")), (SEVERITY_ERROR, Some("500")));
        assert_eq!(hex(&failed.trace_id.unwrap()), "6ad6d85600000000b2a1b6154bdbfb01");
        let raised = span(&rows, "a29018e06517f968");
        assert_eq!((raised.severity, raised.attr("error.message"), raised.attr("error.type")), (SEVERITY_ERROR, Some("cart 3 is locked"), Some("builtins.ValueError")));
        assert!(raised.attr("error.stack").unwrap().starts_with("Traceback (most recent call last):"));
        // Its error an issue (`crate::errors`): the span that raised its service's entry (the
        // database's own, top-level), its error's type, message and stack said; the request's 500,
        // its error said with no stack, none.
        let e = crate::errors::Error::of(raised.attrs());
        let id = crate::errors::span_issue(raised.service, true, &e).map(crate::errors::hex);
        assert!(id.is_some());
        assert_eq!(raised.attr("issue.id"), id.as_deref());
        assert_eq!(failed.attr("issue.id"), None, "{:?}", failed.attrs().collect::<Vec<_>>());
        // The service whose request failed (its entry a 500, its error's details on the spans its
        // Flask integration made under it): one issue, its top-most such span's, of its request.
        let carried: Vec<&RecordRef<'_>> = rows.iter().filter(|r| r.service == "shop" && r.attr("issue.id").is_some()).collect();
        assert_eq!(carried.len(), 1, "one error of the request, once");
        let c = carried[0];
        // Its entry a 500 (`9b132bbe41d2501c`), under it Flask's request span (`4927ac6e83c83aff`, no
        // error), under that the two that say the error (the request's dispatching and its handling of
        // the exception): the top-most of them carries it, not the view's span below.
        assert!(matches!(c.message, "flask.dispatch_request" | "flask.handle_user_exception"), "{}", c.message);
        assert_eq!(c.attr("parent_id"), Some("4927ac6e83c83aff"));
        assert_eq!(c.attr("error.type"), Some("builtins.ValueError"));
        // Every span of a chunk is of its trace, and every parent named is a span sent.
        let ids: std::collections::HashSet<String> = rows.iter().map(|r| hex(&r.span_id.unwrap())).collect();
        for r in &rows {
            if let Some(p) = r.attr("parent_id") {
                assert!(ids.contains(p), "{p}");
            }
        }
        // A second payload decoded with the room the first left allocates no more than rows.
        d.give(out);
        let mut again = Spans::default();
        d.decode(TRACES, 0, &mut again).unwrap();
        assert_eq!(again.spans, 22);
    }

    /// An `AgentPayload` of the inline form, as Agents before 7.84 send it: one tracer's payload
    /// of a request's server span and the cache call it made.
    fn inline_payload() -> Vec<u8> {
        let meta = |m: &mut Writer, k: &str, v: &str| {
            m.message(10, |e| {
                e.str(1, k);
                e.str(2, v);
            })
        };
        let mut w = Writer::default();
        w.str(1, "agent-host");
        w.str(2, "none");
        w.message(5, |p| {
            p.str(1, "c0ffee");
            p.str(2, "go");
            p.message(6, |c| {
                c.varint(1, 2);
                c.str(2, "synthetics");
                c.message(3, |s| {
                    s.str(1, "web");
                    s.str(2, "http.request");
                    s.str(3, "GET /");
                    s.varint(4, 0x1122_3344_5566_7788);
                    s.varint(5, 1);
                    s.varint(7, 1_700_000_000_000_000_000);
                    s.varint(8, 2_000_000);
                    meta(s, "span.kind", "server");
                    meta(s, "_dd.p.tid", "aabbccdd00000000");
                    meta(s, "http.status_code", "200");
                    meta(s, "version", "2.0");
                    s.message(11, |e| {
                        e.str(1, "_sampling_priority_v1");
                        e.double(2, 2.0);
                    });
                    s.str(12, "web");
                });
                c.message(3, |s| {
                    s.str(1, "web");
                    s.str(2, "redis.command");
                    s.str(3, "GET");
                    s.varint(4, 0x1122_3344_5566_7788);
                    s.varint(5, 2);
                    s.varint(6, 1);
                    s.varint(7, 1_700_000_000_000_500_000);
                    s.varint(8, 1_000_000);
                    s.varint(9, 1);
                    meta(s, "span.kind", "client");
                    meta(s, "out.host", "cache-1");
                    meta(s, "error.msg", "timeout");
                    s.str(12, "redis");
                    s.message(14, |l| {
                        l.varint(1, 9);
                        l.varint(2, 8);
                        l.varint(3, 7);
                        l.message(4, |e| {
                            e.str(1, "link.kind");
                            e.str(2, "retry");
                        });
                    });
                    s.message(15, |e| {
                        e.fixed64(1, 1_700_000_000_000_900_000);
                        e.str(2, "exception");
                        for (k, v) in [("exception.type", "Timeout"), ("exception.message", "no reply in 1ms")] {
                            e.message(3, |a| {
                                a.str(1, k);
                                a.message(2, |v2| {
                                    v2.varint(1, 0);
                                    v2.str(2, v);
                                });
                            });
                        }
                        e.message(3, |a| {
                            a.str(1, "attempt");
                            a.message(2, |v2| {
                                v2.varint(1, 2);
                                v2.varint(4, 3);
                            });
                        });
                    });
                });
                c.message(4, |e| {
                    e.str(1, "_dd.p.dm");
                    e.str(2, "-1");
                });
            });
            p.message(7, |e| {
                e.str(1, "_dd.tags.container");
                e.str(2, "container_name:web,kube_namespace:prod");
            });
            p.str(8, "prod");
            p.str(9, "web-1");
            p.str(10, "2.0");
        });
        w.buf
    }

    #[test]
    fn the_inline_form_reads_alike_its_trace_s_high_bits_from_its_chunk() {
        let mut out = Spans::default();
        Decoder::new().decode(&inline_payload(), 0, &mut out).unwrap();
        assert_eq!((out.spans, out.invalid), (2, 0));
        let rows = rows(&out);
        let (root, call) = (span(&rows, "0000000000000001"), span(&rows, "0000000000000002"));
        // `_dd.p.tid` on the root's tags is the whole chunk's: the call's trace is the same.
        for r in [root, call] {
            assert_eq!(hex(&r.trace_id.unwrap()), "aabbccdd000000001122334455667788");
            assert_eq!((r.host, r.source, r.attr("env")), ("web-1", SOURCE, Some("prod")));
            for (k, v) in [("container_name", "web"), ("kube_namespace", "prod"), ("container_id", "c0ffee"), ("language", "go"), ("_dd.origin", "synthetics"), ("_dd.p.dm", "-1")] {
                assert_eq!(r.attr(k), Some(v), "{k}");
            }
        }
        assert_eq!((root.attr("span.kind"), root.attr("version"), root.attr("_sampling_priority_v1"), root.severity), (Some("server"), Some("2.0"), Some("2"), SEVERITY_OK));
        assert_eq!(root.attr("_dd.p.tid"), None);
        // The call: a client's, its peer its host, its error's message an older tracer's
        // `error.msg`, its type its exception event's.
        assert_eq!(call.severity, SEVERITY_ERROR);
        for (k, v) in [
            ("span.kind", "client"),
            ("parent_id", "0000000000000001"),
            ("peer.service", "cache-1"),
            ("error.message", "timeout"),
            ("error.type", "Timeout"),
            ("span.type", "redis"),
            ("duration", "1000000"),
        ] {
            assert_eq!(call.attr(k), Some(v), "{k}");
        }
        assert_eq!(call.attr("version"), None);
        let events: serde_json::Value = serde_json::from_str(call.attr("events").unwrap()).unwrap();
        assert_eq!(events, serde_json::json!([{ "name": "exception", "offset_ns": 400_000, "attributes": { "exception.type": "Timeout", "exception.message": "no reply in 1ms", "attempt": "3" } }]));
        let links: serde_json::Value = serde_json::from_str(call.attr("links").unwrap()).unwrap();
        assert_eq!(links, serde_json::json!([{ "trace_id": "00000000000000080000000000000009", "span_id": "0000000000000007", "attributes": { "link.kind": "retry" } }]));
    }

    #[test]
    fn a_span_without_both_ids_is_counted_and_not_kept() {
        let mut w = Writer::default();
        w.message(5, |p| {
            p.message(6, |c| {
                // No trace id, then no span id.
                c.message(3, |s| {
                    s.str(1, "a");
                    s.varint(5, 1);
                });
                c.message(3, |s| {
                    s.str(1, "a");
                    s.varint(4, 1);
                });
            });
        });
        let mut out = Spans::default();
        Decoder::new().decode(&w.buf, 0, &mut out).unwrap();
        assert_eq!((out.spans, out.invalid), (0, 2));
    }

    #[test]
    fn a_span_past_a_row_s_bound_keeps_its_names_and_is_cut_to_fit() {
        let big = "x".repeat(20_000);
        let mut w = Writer::default();
        w.message(5, |p| {
            p.message(6, |c| {
                c.message(3, |s| {
                    s.str(1, "svc");
                    s.varint(4, 5);
                    s.varint(5, 6);
                    for i in 0..40 {
                        s.message(10, |e| {
                            e.str(1, &format!("k{i:02}"));
                            e.str(2, &big);
                        });
                    }
                });
            });
        });
        let mut out = Spans::default();
        Decoder::new().decode(&w.buf, 0, &mut out).unwrap();
        assert_eq!((out.spans, out.cut), (1, 1));
        let rows = rows(&out);
        let r = rows.first().unwrap();
        assert_eq!(r.attrs().filter(|(k, _)| k.starts_with('k')).count(), 40);
        assert!(out.bytes() <= crate::otlp::traces::MAX_ROW_BYTES + 64);
    }

    #[test]
    fn a_cut_or_garbled_payload_is_refused_or_read_never_a_panic() {
        let mut d = Decoder::new();
        for n in 0..TRACES.len() {
            let mut out = Spans::default();
            let _ = d.decode(TRACES.get(..n).unwrap(), 0, &mut out);
            assert!(out.spans <= 22);
        }
        // Bytes flipped one at a time, a sample of them.
        let mut garbled = TRACES.to_vec();
        for i in (0..garbled.len()).step_by(7) {
            let was = garbled[i];
            garbled[i] = was ^ 0x5a;
            let mut out = Spans::default();
            let _ = d.decode(&garbled, 0, &mut out);
            garbled[i] = was;
        }
        for n in 0..STATS.len() {
            assert!(stats(STATS.get(..n).unwrap()).is_err(), "cut at {n}");
        }
        let mut garbled = STATS.to_vec();
        for i in 0..garbled.len() {
            let was = garbled[i];
            garbled[i] = was ^ 0xa5;
            let _ = stats(&garbled);
            garbled[i] = was;
        }
    }

    /// A canonical key's metric and tags (`crate::batch::put_canonical`).
    fn parts(key: &[u8]) -> (String, Vec<(String, String)>) {
        let mut at = key;
        let s = |at: &mut &[u8]| {
            let n = crate::varint::get(at).unwrap() as usize;
            let (b, rest) = at.split_at(n);
            *at = rest;
            String::from_utf8(b.to_vec()).unwrap()
        };
        let metric = s(&mut at);
        let n = crate::varint::get(&mut at).unwrap();
        let tags = (0..n).map(|_| (s(&mut at), s(&mut at))).collect();
        (metric, tags)
    }

    fn point(series: &[(SeriesKey, Vec<(i64, f64)>)], name: &str, tags: &[(&str, &str)]) -> Option<(i64, f64)> {
        let want = SeriesKey::new(name, tags.iter().copied(), Kind::Count).ok()?;
        let gauge = SeriesKey::new(name, tags.iter().copied(), Kind::Gauge).ok()?;
        series.iter().find(|(k, _)| k.key == want.key || k.key == gauge.key).and_then(|(_, p)| p.first().copied())
    }

    #[test]
    fn an_agent_s_stats_read_as_kiln_s_trace_metrics() {
        let (series, taken) = stats(STATS).unwrap();
        assert_eq!(taken, StatsTaken { groups: 3, inner: 0, unread: 0 });
        let tags = |service, resource, kind, status| {
            vec![("service", service), ("resource_name", resource), ("span.kind", kind), ("env", "e2e"), ("version", "1.2.3"), ("http.status_code", status), ("host", "kiln-apm-cap")]
        };
        let ok = tags("shop", "GET /cart/<int:n>", "server", "200");
        let failed = tags("shop", "GET /cart/<int:n>", "server", "500");
        // A span of a service the tracer named another, of no kind said, top-level: an entry of it.
        let db = tags("shop-db", "SELECT carts", "internal", "N/A");
        let (at, hits) = point(&series, "trace.hits", &ok).unwrap();
        // The bucket's start (1792464980 s) and the payload's offset within its ten seconds.
        assert_eq!(at - at.rem_euclid(10_000), 1_792_464_980_000);
        assert_eq!(hits, 8.0);
        assert_eq!(point(&series, "trace.errors", &ok), None);
        assert_eq!(point(&series, "trace.hits", &failed).map(|p| p.1), Some(2.0));
        assert_eq!(point(&series, "trace.errors", &failed).map(|p| p.1), Some(2.0));
        assert_eq!(point(&series, "trace.hits", &db).map(|p| p.1), Some(10.0));
        assert_eq!(point(&series, "trace.errors", &db).map(|p| p.1), Some(2.0));
        assert_eq!(point(&series, "trace.duration.count", &ok).map(|p| p.1), Some(8.0));
        let sum = point(&series, "trace.duration.sum", &ok).unwrap().1;
        assert!((sum - 0.095_205_695).abs() < 1e-12, "{sum}");
        // The durations' buckets hold every hit, the errors' sketch's too.
        let buckets = |tags: &[(&str, &str)]| -> Vec<(f64, f64)> {
            series
                .iter()
                .filter_map(|(k, p)| {
                    let (metric, kv) = parts(&k.key);
                    let has = |t: &str, v: &str| kv.iter().any(|(a, b)| a == t && b == v);
                    if metric != "trace.duration.dist" || !tags.iter().all(|(t, v)| has(t, v)) {
                        return None;
                    }
                    let (_, b) = kv.iter().find(|(a, _)| a == dist::BUCKET_TAG)?;
                    let (scale, bucket) = dist::parse_tag_value(b)?;
                    Some((bucket.value(scale), p.first()?.1))
                })
                .collect()
        };
        let ok_buckets = buckets(&[("http.status_code", "200"), ("service", "shop")]);
        assert_eq!(ok_buckets.iter().map(|(_, n)| n).sum::<f64>(), 8.0);
        // The handler sleeps 10 ms: every request took 10 ms and some, under 20.
        assert!(ok_buckets.iter().all(|(v, _)| (0.0105..0.02).contains(v)), "{ok_buckets:?}");
        let min = point(&series, "trace.duration.min", &ok).unwrap().1;
        let max = point(&series, "trace.duration.max", &ok).unwrap().1;
        assert!(min <= max && (0.0105..0.02).contains(&min) && (0.0105..0.02).contains(&max), "{min} {max}");
        assert_eq!(buckets(&[("http.status_code", "500")]).iter().map(|(_, n)| n).sum::<f64>(), 2.0);
        // A payload sent again (a retry) lands on the same points; another payload's elsewhere in
        // the bucket.
        assert_eq!(stats(STATS).unwrap().0.first().map(|(_, p)| p.clone()), series.first().map(|(_, p)| p.clone()));
    }

    /// The mappings' index functions, as sketches-go and sketches-java compute them: a sketch's
    /// bin of a value, which [`Mapping::value`] must read back within its relative accuracy.
    fn reference_index(interpolation: u64, gamma: f64, offset: f64, v: f64) -> i64 {
        let bits = v.to_bits();
        let e = ((bits >> 52) & 0x7ff) as f64 - 1023.0;
        let s1 = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
        let (log, multiplier) = match interpolation {
            0 => (v.ln(), 1.0 / gamma.ln()),
            1 => (e + s1 - 1.0, 1.0 / gamma.log2()),
            2 => (e - (s1 - 5.0) * (s1 - 1.0) / 3.0, 1.0 / gamma.log2()),
            _ => {
                let s = s1 - 1.0;
                (((CUBIC_A * s + CUBIC_B) * s + CUBIC_C) * s + e, 1.0 / gamma.log2())
            }
        };
        (log * multiplier + offset).floor() as i64
    }

    #[test]
    fn each_mapping_reads_a_bin_back_within_its_relative_accuracy() {
        let ln2 = std::f64::consts::LN_2;
        for (interpolation, c) in [(0, 1.0), (1, 1.0 / ln2), (2, 3.0 / (4.0 * ln2)), (3, 7.0 / (10.0 * ln2))] {
            for accuracy in [0.01f64, 0.0075, 0.02] {
                // γ for that accuracy (`LogLikeIndexMapping.gamma`), and an offset as Java's linear
                // mapping's backward-compatible one is (1/log2 γ), or none.
                let gamma = ((1.0 + accuracy) / (1.0 - accuracy)).powf(1.0 / c);
                for offset in [0.0, 1.0 / gamma.log2()] {
                    let m = Mapping::new(gamma, offset, interpolation).unwrap();
                    assert!((m.alpha() - accuracy).abs() < 1e-9, "{interpolation} {accuracy}: {}", m.alpha());
                    let mut v = 1e-9;
                    while v < 1e12 {
                        let i = reference_index(interpolation, gamma, offset, v);
                        let read = m.value(i);
                        assert!((read - v).abs() <= accuracy * v * (1.0 + 1e-9), "{interpolation} {accuracy} {offset}: {v} read {read}");
                        assert!(m.lower(i) <= v * (1.0 + 1e-12) && v < m.lower(i + 1) * (1.0 + 1e-12), "{interpolation}: {v} in bin {i}");
                        v *= 1.003_7;
                    }
                }
            }
        }
        assert_eq!(Mapping::new(1.0, 0.0, 0), None);
        assert_eq!(Mapping::new(1.02, 0.0, 4), None);
    }

    #[test]
    fn a_sketch_s_bins_are_read_from_its_map_and_its_run_alike() {
        // γ of 1% accuracy; the bins 10 and 12 by the map, 20..22 by the run, and two zeros.
        let gamma = 1.01 / 0.99;
        let mut w = Writer::default();
        w.message(1, |m| m.double(1, gamma));
        w.message(2, |s| {
            for (i, n) in [(10u64, 1.0), (12, 2.0)] {
                s.message(1, |e| {
                    e.varint(1, i << 1);
                    e.double(2, n);
                });
            }
            s.bytes(2, &[3.0f64, 0.0, 4.0].iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
            s.varint(3, 20 << 1);
        });
        w.double(4, 2.0);
        let mut got = Vec::new();
        assert!(sketch_bins(&w.buf, &mut Vec::new(), |v, n| got.push((v, n))).unwrap());
        let m = Mapping::new(gamma, 0.0, 0).unwrap();
        assert_eq!(got, vec![(0.0, 2.0), (m.value(10), 1.0), (m.value(12), 2.0), (m.value(20), 3.0), (m.value(22), 4.0)]);
        // A mapping not known: its bins are not read, and it says so.
        let mut w = Writer::default();
        w.message(1, |m| {
            m.double(1, gamma);
            m.varint(3, 9);
        });
        w.message(2, |s| s.varint(3, 2));
        assert!(!sketch_bins(&w.buf, &mut Vec::new(), |_, _| {}).unwrap());
    }

    #[test]
    fn spans_inside_a_service_are_not_counted_and_a_client_s_are_its_calls_out_by_peer() {
        // Built as the Agent's msgp writes a payload: maps by field name.
        fn s(out: &mut Vec<u8>, v: &str) {
            out.push(0xd9);
            out.push(v.len() as u8);
            out.extend_from_slice(v.as_bytes());
        }
        fn u(out: &mut Vec<u8>, v: u64) {
            out.push(0xcf);
            out.extend_from_slice(&v.to_be_bytes());
        }
        let group = |out: &mut Vec<u8>, kind: &str, top: u64, peer: &[&str]| {
            out.push(0x87);
            for (k, v) in [("Service", "api"), ("Resource", "GET /x"), ("SpanKind", kind)] {
                s(out, k);
                s(out, v);
            }
            s(out, "Hits");
            u(out, 4);
            s(out, "Errors");
            u(out, 1);
            s(out, "TopLevelHits");
            u(out, top);
            s(out, "PeerTags");
            out.push(0x90 | peer.len() as u8);
            for p in peer {
                s(out, p);
            }
        };
        let mut b = Vec::new();
        b.push(0x82);
        s(&mut b, "AgentHostname");
        s(&mut b, "h1");
        s(&mut b, "Stats");
        b.push(0x91);
        b.push(0x82);
        s(&mut b, "Env");
        s(&mut b, "prod");
        s(&mut b, "Stats");
        b.push(0x91);
        b.push(0x83);
        s(&mut b, "Start");
        u(&mut b, 1_700_000_000_000_000_000);
        s(&mut b, "Duration");
        u(&mut b, 10_000_000_000);
        s(&mut b, "Stats");
        b.push(0x94);
        group(&mut b, "client", 0, &["db.system:postgresql", "peer.service:billing-db", "out.host:10.0.0.9"]);
        group(&mut b, "internal", 0, &[]);
        group(&mut b, "", 2, &[]);
        group(&mut b, "consumer", 0, &[]);
        let (series, taken) = stats(&b).unwrap();
        assert_eq!(taken, StatsTaken { groups: 3, inner: 1, unread: 0 });
        let out_tags = [("service", "api"), ("resource_name", "GET /x"), ("span.kind", "client"), ("env", "prod"), ("peer.service", "billing-db"), ("host", "h1")];
        assert_eq!(point(&series, "trace.outbound.hits", &out_tags).map(|p| p.1), Some(4.0));
        assert_eq!(point(&series, "trace.outbound.errors", &out_tags).map(|p| p.1), Some(1.0));
        // Of no kind said, half of it top-level: half its counts its service's entries.
        let half = [("service", "api"), ("resource_name", "GET /x"), ("span.kind", "internal"), ("env", "prod"), ("version", "N/A"), ("http.status_code", "N/A"), ("host", "h1")];
        assert_eq!(point(&series, "trace.hits", &half).map(|p| p.1), Some(2.0));
        assert_eq!(point(&series, "trace.errors", &half).map(|p| p.1), Some(0.5));
        let consumer = [("service", "api"), ("resource_name", "GET /x"), ("span.kind", "consumer"), ("env", "prod"), ("version", "N/A"), ("http.status_code", "N/A"), ("host", "h1")];
        assert_eq!(point(&series, "trace.hits", &consumer).map(|p| p.1), Some(4.0));
    }

    /// MessagePack as tracers write it, for the tests.
    #[derive(Default)]
    struct Mp(Vec<u8>);

    impl Mp {
        fn head(&mut self, fix: u8, wide: u8, n: usize) -> &mut Self {
            if n < 16 {
                self.0.push(fix | n as u8);
            } else {
                self.0.push(wide);
                self.0.extend_from_slice(&(n as u32).to_be_bytes());
            }
            self
        }
        fn map(&mut self, n: usize) -> &mut Self {
            self.head(0x80, 0xdf, n)
        }
        fn arr(&mut self, n: usize) -> &mut Self {
            self.head(0x90, 0xdd, n)
        }
        fn s(&mut self, v: &str) -> &mut Self {
            self.0.push(0xd9);
            self.0.push(v.len() as u8);
            self.0.extend_from_slice(v.as_bytes());
            self
        }
        fn u(&mut self, v: u64) -> &mut Self {
            self.0.push(0xcf);
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn i(&mut self, v: i64) -> &mut Self {
            self.0.push(0xd3);
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn f(&mut self, v: f64) -> &mut Self {
            self.0.push(0xcb);
            self.0.extend_from_slice(&v.to_be_bytes());
            self
        }
    }

    /// A tracer's span: its fields, its meta and its metrics.
    struct TSpan {
        service: &'static str,
        name: &'static str,
        resource: &'static str,
        trace: u64,
        span: u64,
        parent: u64,
        start: i64,
        duration: i64,
        error: i64,
        meta: Vec<(&'static str, &'static str)>,
        metrics: Vec<(&'static str, f64)>,
        ty: &'static str,
    }

    /// Two traces: one kept (a request, its call out, and the span of the service it calls),
    /// one its tracer dropped (`manual.drop`), failing.
    fn tracer_traces() -> Vec<Vec<TSpan>> {
        let t0 = 1_700_000_000_000_000_000;
        vec![
            vec![
                TSpan {
                    service: "web",
                    name: "flask.request",
                    resource: "GET /",
                    trace: 0x11,
                    span: 1,
                    parent: 0,
                    start: t0,
                    duration: 30_000_000,
                    error: 0,
                    meta: vec![("span.kind", "server"), ("env", "prod"), ("version", "1.0"), ("http.status_code", "200"), ("_dd.p.tid", "aabbccdd00000000")],
                    metrics: vec![("_sampling_priority_v1", 1.0), ("_dd.top_level", 1.0)],
                    ty: "web",
                },
                TSpan {
                    service: "web",
                    name: "requests.request",
                    resource: "GET",
                    trace: 0x11,
                    span: 2,
                    parent: 1,
                    start: t0 + 1_000_000,
                    duration: 20_000_000,
                    error: 0,
                    meta: vec![("span.kind", "client"), ("env", "prod"), ("out.host", "db-1")],
                    metrics: vec![],
                    ty: "http",
                },
                TSpan {
                    service: "db",
                    name: "postgres.query",
                    resource: "SELECT 1",
                    trace: 0x11,
                    span: 3,
                    parent: 2,
                    start: t0 + 2_000_000,
                    duration: 10_000_000,
                    error: 0,
                    meta: vec![("env", "prod")],
                    metrics: vec![],
                    ty: "sql",
                },
            ],
            vec![TSpan {
                service: "web",
                name: "flask.request",
                resource: "GET /",
                trace: 0x22,
                span: 9,
                parent: 0,
                start: t0,
                duration: 5_000_000,
                error: 1,
                meta: vec![("span.kind", "server"), ("env", "prod"), ("version", "1.0"), ("http.status_code", "500")],
                metrics: vec![("_sampling_priority_v1", -1.0)],
                ty: "web",
            }],
        ]
    }

    fn v04(traces: &[Vec<TSpan>]) -> Vec<u8> {
        let mut m = Mp::default();
        m.arr(traces.len());
        for t in traces {
            m.arr(t.len());
            for s in t {
                m.map(12);
                m.s("service").s(s.service).s("name").s(s.name).s("resource").s(s.resource).s("type").s(s.ty);
                m.s("trace_id").u(s.trace).s("span_id").u(s.span).s("parent_id").u(s.parent);
                m.s("start").i(s.start).s("duration").i(s.duration).s("error").i(s.error);
                m.s("meta").map(s.meta.len());
                for (k, v) in &s.meta {
                    m.s(k).s(v);
                }
                m.s("metrics").map(s.metrics.len());
                for (k, v) in &s.metrics {
                    m.s(k).f(*v);
                }
            }
        }
        m.0
    }

    fn v05(traces: &[Vec<TSpan>]) -> Vec<u8> {
        let mut strings: Vec<&str> = vec![""];
        let at = |v: &'static str, strings: &mut Vec<&'static str>| -> u64 {
            match strings.iter().position(|s| *s == v) {
                Some(i) => i as u64,
                None => {
                    strings.push(v);
                    (strings.len() - 1) as u64
                }
            }
        };
        let mut body = Mp::default();
        body.arr(traces.len());
        for t in traces {
            body.arr(t.len());
            for s in t {
                body.arr(12);
                for v in [s.service, s.name, s.resource] {
                    let i = at(v, &mut strings);
                    body.u(i);
                }
                body.u(s.trace).u(s.span).u(s.parent).i(s.start).i(s.duration).i(s.error);
                body.map(s.meta.len());
                for &(k, v) in &s.meta {
                    let (k, v) = (at(k, &mut strings), at(v, &mut strings));
                    body.u(k).u(v);
                }
                body.map(s.metrics.len());
                for &(k, v) in &s.metrics {
                    let k = at(k, &mut strings);
                    body.u(k).f(v);
                }
                let ty = at(s.ty, &mut strings);
                body.u(ty);
            }
        }
        let mut m = Mp::default();
        m.arr(2).arr(strings.len());
        for v in &strings {
            m.s(v);
        }
        m.0.extend_from_slice(&body.0);
        m.0
    }

    #[test]
    fn a_tracer_s_traces_are_kept_or_dropped_as_it_says_and_every_span_counted() {
        for (form, body) in [("v0.4", v04(&tracer_traces())), ("v0.5", v05(&tracer_traces()))] {
            let tracer = Tracer { language: "python", container: "", host: "web-1" };
            let (mut out, mut stats) = (Spans::default(), Vec::new());
            let mut roots = Vec::new();
            let taken = Decoder::new().decode_tracer(form == "v0.5", &body, &tracer, 0, &mut out, &mut stats, &mut roots).unwrap();
            // Each trace's root: the request's span, its priority the trace's.
            let r: Vec<(&str, &str, Option<f64>)> = roots.iter().map(|r| (r.service, r.env, r.priority)).collect();
            assert_eq!(r, vec![("web", "prod", Some(1.0)), ("web", "prod", Some(-1.0))], "{form}");
            assert_eq!(taken, TracerTaken { traces: 2, dropped: 1 }, "{form}");
            // The kept trace's three spans, none of the dropped one's.
            assert_eq!(out.spans, 3, "{form}");
            let rows = rows(&out);
            let root = span(&rows, "0000000000000001");
            assert_eq!(hex(&root.trace_id.unwrap()), "aabbccdd000000000000000000000011", "{form}: the high bits of the chunk's _dd.p.tid");
            assert_eq!((root.host, root.source, root.message, root.attr("language")), ("web-1", SOURCE, "GET /", Some("python")), "{form}");
            assert_eq!((root.attr("span.kind"), root.attr("operation_name"), root.attr("_dd.top_level")), (Some("server"), Some("flask.request"), Some("1")), "{form}");
            let call = span(&rows, "0000000000000002");
            assert_eq!((call.attr("span.kind"), call.attr("peer.service"), call.attr("parent_id")), (Some("client"), Some("db-1"), Some("0000000000000001")), "{form}");
            // Counted: the request (an entry), its call out (an exit, by its peer), the span of the
            // service it called (no kind said, its parent another service's: top-level, an entry),
            // and the dropped trace's failed request.
            let got: Vec<(bool, &str, &str, &str, u64, bool)> = stats.iter().map(|s| (s.outbound, s.service, s.kind, s.third, s.status, s.error)).collect();
            assert_eq!(
                got,
                vec![(false, "web", "server", "1.0", 200, false), (true, "web", "client", "db-1", 0, false), (false, "db", "internal", "", 0, false), (false, "web", "server", "1.0", 500, true)],
                "{form}"
            );
        }
    }

    fn stat(service: &'static str, start_ms: i64, error: bool) -> SpanStat<'static> {
        SpanStat { outbound: false, service, resource: "GET /", kind: "server", env: "prod", third: "1.0", status: 200, start_ns: start_ms * 1_000_000, duration_ns: 20_000_000, error }
    }

    #[test]
    fn a_concentrator_writes_each_bucket_once_when_due_a_late_span_in_the_oldest_open() {
        let keys = Keys::new();
        let mut d = Decoded::default();
        let mut c = Concentrator::new("web-1");
        let o = c.offset();
        assert!((0..BUCKET_MS).contains(&o), "{o}");
        let t = 1_700_000_000_000; // a bucket's start (ms)
        // Counted in the bucket of their ends, as the Agent counts them: the first two ending in the
        // first bucket, the third (begun in it) in the next.
        c.add(&stat("web", t + 1_000, false), t + 1_000);
        c.add(&stat("web", t + 9_979, true), t + 9_999);
        c.add(&stat("web", t + 9_990, false), t + 10_010);
        // Not due until twenty seconds past its end.
        assert_eq!(c.flush_into(t + 29_999, false, &keys, &mut d), 0);
        assert_eq!(c.flush_into(t + 30_000, false, &keys, &mut d), 1);
        let hits =
            |d: &Decoded| -> Vec<(i64, f64)> { d.samples.iter().filter(|(i, ..)| d.keys.get(*i as usize).is_some_and(|k| parts(&k.key).0 == "trace.hits")).map(|&(_, ts, v)| (ts, v)).collect() };
        assert_eq!(hits(&d), vec![(t + o, 2.0)]);
        let tags = parts(&d.keys.first().unwrap().key).1;
        assert!(tags.contains(&("host".to_owned(), "web-1".to_owned())) && tags.contains(&("service".to_owned(), "web".to_owned())), "{tags:?}");
        let errors: f64 = d.samples.iter().filter(|(i, ..)| d.keys.get(*i as usize).is_some_and(|k| parts(&k.key).0 == "trace.errors")).map(|s| s.2).sum();
        assert_eq!(errors, 1.0);
        // A span of the bucket written comes late: into the oldest not yet due (the next one).
        c.add(&stat("web", t + 5_000, false), t + 30_500);
        assert_eq!(c.flush_into(t + 40_000, false, &keys, &mut d), 1);
        assert_eq!(hits(&d), vec![(t + 10_000 + o, 2.0)]);
        // One more late, with no bucket open: the first not due. A span a day ahead: now's.
        c.add(&stat("web", t, false), t + 41_000);
        c.add(&stat("web", t + 86_400_000, false), t + 41_000);
        assert_eq!(c.open(), 2);
        assert_eq!(c.flush_into(t + 41_000, true, &keys, &mut d), 2, "every one as the node stops");
        assert_eq!(hits(&d), vec![(t + 20_000 + o, 1.0), (t + 40_000 + o, 1.0)]);
        assert_eq!(c.open(), 0);
    }

    /// Spans saying any time at all (a tracer's clock wrong, a payload made to hurt) open nine
    /// buckets at the most, from the oldest not yet due to the one a minute ahead, and every one of
    /// them is counted; nothing waits for a quiet spell's buckets to be written first.
    #[test]
    fn spans_of_any_time_open_nine_buckets_at_the_most_and_are_all_counted() {
        let keys = Keys::new();
        let mut d = Decoded::default();
        let mut c = Concentrator::new("web-1");
        let now = 1_700_000_005_000;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Half at any time at all, of any length; half within two minutes of now.
            let (start_ns, duration_ns) = if x & 1 == 0 { (x as i64, x >> 1) } else { ((now - 120_000 + ((x >> 1) % 240_000) as i64) * 1_000_000, x >> 44) };
            c.add(&SpanStat { start_ns, duration_ns, ..stat("web", 0, false) }, now);
            assert!(c.open() <= 9, "{} buckets open", c.open());
        }
        assert_eq!(c.flush_into(now, true, &keys, &mut d), 9);
        let hits: f64 = d.samples.iter().filter(|(i, ..)| d.keys.get(*i as usize).is_some_and(|k| parts(&k.key).0 == "trace.hits")).map(|s| s.2).sum();
        assert_eq!(hits, 100_000.0);
        let first = d.samples.iter().map(|s| s.1 - c.offset()).min().unwrap();
        let last = d.samples.iter().map(|s| s.1 - c.offset()).max().unwrap();
        assert_eq!((first, last), (now - 25_000, now + 55_000), "the oldest not yet due, and now's bucket a minute on");
    }

    /// A node started again writes a bucket it wrote before at another point within it: the two
    /// add, as a store keeps the later of two writes at one time. (Two starts share an offset one
    /// time in ten thousand: of eight, not all one.)
    #[test]
    fn a_node_started_again_writes_beside_what_it_wrote_not_over_it() {
        let offsets: std::collections::BTreeSet<i64> = (0..8).map(|_| Concentrator::new("web-1").offset()).collect();
        assert!(offsets.len() > 1, "{offsets:?}");
        assert!(offsets.iter().all(|o| (0..BUCKET_MS).contains(o)));
    }

    #[test]
    fn a_bucket_past_its_groups_counts_the_rest_in_one_of_no_names() {
        let keys = Keys::new();
        let mut d = Decoded::default();
        let mut c = Concentrator::new("web-1");
        let t = 1_700_000_000_000;
        let names: Vec<String> = (0..crate::logs::metrics::GROUPS + 10).map(|i| format!("svc-{i}")).collect();
        for n in &names {
            c.add(&SpanStat { service: n, ..stat("x", t, false) }, t);
        }
        c.flush_into(t + 30_000, false, &keys, &mut d);
        let hits: Vec<(String, f64)> = d
            .samples
            .iter()
            .filter_map(|&(i, _, v)| {
                let (metric, tags) = parts(&d.keys.get(i as usize)?.key);
                (metric == "trace.hits").then(|| (tags.into_iter().find(|(k, _)| k == "service").map(|(_, v)| v).unwrap_or_default(), v))
            })
            .collect();
        assert_eq!(hits.len(), crate::logs::metrics::GROUPS + 1);
        assert_eq!(hits.iter().find(|(s, _)| s == "N/A").map(|h| h.1), Some(10.0));
        assert_eq!(hits.iter().map(|h| h.1).sum::<f64>(), names.len() as f64);
    }

    /// What ddtrace 4.16 (Python, its libdatadog writer) sent its Agent's port (`/v0.5/traces`),
    /// recorded: a Flask service's requests, a dropped one among them.
    const TRACER_A: &[u8] = include_bytes!("fixtures/datadog/tracer-v05-4.16-a.msgpack");
    const TRACER_B: &[u8] = include_bytes!("fixtures/datadog/tracer-v05-4.16-b.msgpack");

    #[test]
    fn ddtrace_s_own_v0_5_payloads_read_whole() {
        let tracer = Tracer { language: "python", container: "", host: "web-1" };
        // The cart's: its flush's trace, then four orders' (the odd ones dropped by the shop's
        // tracer, its decision carried in the request): each order its request and its database's
        // span (a service the tracer named, top-level).
        let (mut out, mut stats) = (Spans::default(), Vec::new());
        let taken = Decoder::new().decode_tracer(true, TRACER_A, &tracer, 0, &mut out, &mut stats, &mut Vec::new()).unwrap();
        assert_eq!((taken, out.spans, out.invalid), (TracerTaken { traces: 5, dropped: 2 }, 29, 0));
        let rows = rows(&out);
        assert!(rows.iter().all(|r| r.source == SOURCE && r.host == "web-1" && r.attr("language") == Some("python")));
        assert!(rows.iter().all(|r| r.trace_id.is_some_and(|t| hex(&t).starts_with("6ad6efc0"))), "the high bits of each chunk's _dd.p.tid");
        let db = rows.iter().find(|r| r.service == "cart-db").unwrap();
        assert_eq!((db.message, db.attr("span.type"), db.attr("span.kind"), db.attr("db.system")), ("SELECT carts", Some("sql"), Some("internal"), Some("postgresql")));
        let counted: Vec<(&str, &str, &str, bool)> = stats.iter().map(|s| (s.service, s.resource, s.kind, s.outbound)).collect();
        assert_eq!(counted.len(), 9);
        assert_eq!(counted.first(), Some(&("cart", "GET /flush", "server", false)));
        assert_eq!(counted.iter().filter(|c| **c == ("cart", "GET /cart/<int:order>", "server", false)).count(), 4);
        assert_eq!(counted.iter().filter(|c| **c == ("cart-db", "SELECT carts", "internal", false)).count(), 4, "top-level: its parent another service's");
        // The shop's: its requests' spans, and its requests integration's calls out (a service of its
        // own, `requests`), their peer the address they called.
        let (mut out, mut stats) = (Spans::default(), Vec::new());
        let taken = Decoder::new().decode_tracer(true, TRACER_B, &tracer, 0, &mut out, &mut stats, &mut Vec::new()).unwrap();
        assert_eq!((taken, out.spans, out.invalid), (TracerTaken { traces: 5, dropped: 2 }, 31, 0));
        let calls: Vec<&SpanStat<'_>> = stats.iter().filter(|s| s.outbound).collect();
        assert_eq!(calls.len(), 4);
        assert!(calls.iter().all(|c| (c.service, c.kind, c.third, c.status) == ("requests", "client", "127.0.0.1", 200)), "{calls:?}");
        assert_eq!(stats.iter().filter(|s| (s.service, s.resource, s.third) == ("shop", "GET /checkout/<int:order>", "1.2.3")).count(), 4);
    }

    #[test]
    fn the_target_is_spread_evenly_and_what_a_quiet_signature_leaves_goes_to_the_rest() {
        assert!((tps_per_signature(10.0, [1.0, 2.0, 100.0].into_iter()) - 7.0).abs() < 1e-12);
        assert!((tps_per_signature(10.0, [50.0, 50.0].into_iter()) - 5.0).abs() < 1e-12);
        assert!((tps_per_signature(10.0, std::iter::empty()) - 10.0).abs() < 1e-12);
    }

    fn root(service: &str, priority: f64) -> Root<'_> {
        Root { service, env: "prod", priority: Some(priority), weight: 1.0 }
    }

    fn rate_of(s: &Sampler, key: &str) -> Option<f64> {
        s.rates().find(|(k, _)| k == key).map(|r| r.1)
    }

    #[test]
    fn a_busy_service_is_sampled_to_its_share_and_a_quiet_one_kept_whole() {
        let mut s = Sampler::new(10.0);
        let t0 = 1_700_000_000_000i64;
        // Ten seconds: `busy` at 100 traces a second, `quiet` at 1; a user's decisions not counted.
        for ms in (0..10_000).step_by(10) {
            s.count(&root("busy", 1.0), t0 + ms);
            if ms % 1_000 == 0 {
                s.count(&root("quiet", 0.0), t0 + ms);
                s.count(&root("ruled", 2.0), t0 + ms);
                s.count(&root("ruled", -1.0), t0 + ms);
            }
        }
        // The next bucket begins: the rates made.
        s.count(&root("busy", 1.0), t0 + 10_000);
        let busy = rate_of(&s, "service:busy,env:prod").unwrap();
        // Its share: 10 less the quiet one's 1, of its 100 a second.
        assert!((busy - 0.09).abs() < 1e-9, "{busy}");
        assert_eq!(rate_of(&s, "service:quiet,env:prod"), Some(1.0));
        assert_eq!(rate_of(&s, "service:ruled,env:prod"), None, "a user's decisions are not counted");
        assert_eq!(s.signatures(), 2);
        // A service not known: the whole traffic's rate, but never above the lowest given.
        let d = rate_of(&s, "service:,env:").unwrap();
        assert!((d - busy).abs() < 1e-9, "{d}");
        // `busy` quiets to 5 a second: its rate holds while its busy buckets are in the window (six
        // updates), then rises a fifth an update at the most: from 0.09 to all in some fourteen.
        let mut prev = busy;
        for step in 0..22 {
            let base = t0 + 15_000 + step * 5_000;
            for ms in (0..5_000).step_by(200) {
                s.count(&root("busy", 1.0), base + ms);
            }
            let r = rate_of(&s, "service:busy,env:prod").unwrap();
            assert!(r <= prev * MAX_RATE_INCREASE + 1e-12 && r >= prev - 1e-12, "step {step}: {prev} to {r}");
            prev = r;
        }
        assert!((prev - 1.0).abs() < 1e-12, "back to all once quiet: {prev}");
        // A whole window of no traffic: forgotten.
        s.count(&root("other", 1.0), t0 + 200_000);
        assert_eq!(rate_of(&s, "service:busy,env:prod"), None);
    }

    #[test]
    fn a_trace_weighs_what_its_tracer_sampled_it_at_and_no_target_keeps_everything() {
        let mut s = Sampler::new(10.0);
        let t0 = 1_700_000_000_000i64;
        // Ten a second, each sampled at a tenth by its tracer: a hundred a second.
        for ms in (0..5_000).step_by(100) {
            s.count(&Root { weight: 10.0, ..root("pre", 1.0) }, t0 + ms);
        }
        s.count(&root("pre", 1.0), t0 + 5_000);
        let r = rate_of(&s, "service:pre,env:prod").unwrap();
        assert!((r - 0.1).abs() < 1e-9, "{r}");
        let mut all = Sampler::new(0.0);
        for ms in (0..10_000).step_by(1) {
            all.count(&root("busy", 1.0), t0 + ms);
        }
        assert!(all.rates().all(|(_, r)| r == 1.0));
    }
}
