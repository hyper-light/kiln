//! OTLP traces into spans (`docs/observe.md` §3, §8.7): an `ExportTraceServiceRequest`
//! (opentelemetry-proto `collector/trace/v1`, v1.5.0) read in place and written straight into the
//! span store's rows (the log rows' encoding, `crate::logs::record`): no record is made of a span on
//! the way, and a [`Decoder`] that has decoded a request allocates nothing for the next but rows.
//!
//! A span is a record:
//! - its time the span's start, and `duration` (an attribute, in ns, as Datadog's `@duration`) its
//!   end less its start;
//! - its message the span's name, which OpenTelemetry has be "the most general string that
//!   identifies a (statistically) interesting class of Spans" (`GET /users/{id}`): Datadog's
//!   resource;
//! - `service` and `host` the resource's `service.name` and `host.name`, `source` the
//!   instrumentation scope's name (else `otlp`), as OTLP's logs have them;
//! - severity 17 (`status:error`) for status ERROR, else 9 (`status:ok`);
//! - its attributes the span's over its resource's (but the two named above), and kiln's own: first,
//!   so that no attribute of the span's stands for them, `span.kind`, `duration`, `parent_id` (hex)
//!   and `otel.status_code` (`OK` or `ERROR`, when set); then, where the span does not say them
//!   itself, Datadog's unified service tags `env` and `version` (from `deployment.environment[.name]`
//!   and `service.version`), its exception event's type, message and stack (else its status's
//!   message) as `error.type`, `error.message` and `error.stack` (as Datadog maps OpenTelemetry's
//!   `exception.*`), `peer.service` on a client's or a producer's span (the system or the address it
//!   calls, where not given: [`PEER_SOURCES`], Datadog's tracers' `out.host` among them), Datadog's `http.status_code` (from the semantic
//!   conventions' `http.response.status_code`), and its events and links as JSON; an LLM span's
//!   Datadog's LLM fields, tokens and cost (`crate::llm::spans`: read whole before the span's
//!   attributes are bounded, room kept for them);
//! - its trace id and span id. A span without a valid one of each (16 and 8 bytes, not all zero, as
//!   the specification has them) is not kept, and counted.
//!
//! Limits are Datadog's trace agent's (`pkg/trace/traceutil/truncate.go`): a value at 25,000 bytes
//! (`MaxMetaValLen`), a name at 5,000 (`MaxResourceLen`); attributes [`MAX_TAGS`] in all,
//! OpenTelemetry's default count limit, the span's past what kiln's own leave dropped in key order,
//! its resource's added while room is left ([`merge_under`]).
//! A span's row is at most [`MAX_ROW_BYTES`]: past it, its longest values are cut to one length,
//! the least that brings it within (the others whole), so a write always holds it.

use std::fmt::Write as _;

use super::metrics::{OtlpError, any_value_push, push_json_str};
use super::pb::Fields;
use crate::batch::{MAX_NAME_BYTES, MAX_TAGS};
use crate::logs::record::put_fields;

/// A span's attribute's value at the most: the trace agent's `MaxMetaValLen`.
pub const MAX_VALUE_BYTES: usize = 25_000;
/// A span's name at the most: the trace agent's `MaxResourceLen`.
pub const MAX_NAME: usize = 5_000;
/// The rows of a segment at the most (its last row may pass it): a log block's text
/// (`crate::logs::block::BLOCK_TEXT`), the size at LZ4's ratio plateau (§12.6), so a segment packs
/// as well as a block does.
pub const SEGMENT_BYTES: usize = crate::logs::block::BLOCK_TEXT;
/// Row buffers a decoder keeps at the most: a request of a batch span processor's 512 spans takes
/// one or two segments, one of 20 MiB (the largest taken) some eighty.
pub const SPARES: usize = 8;
/// A span's row at the most: a segment's bytes, so a segment (its rows before its last under
/// [`SEGMENT_BYTES`], and its last) is at most twice them, under half a write's entry
/// (`crate::group::ENTRY_BYTES`) with LZ4's worst case (n + n/255 + 16) on it.
pub const MAX_ROW_BYTES: usize = SEGMENT_BYTES;
/// What a row's fields take beside its values, at the most: its time and severity (11), its
/// flags and ids (25), each string's length (a varint of 3 bytes for a string under 2 MiB).
const ROW_OVERHEAD: usize = 64;

/// kiln's own attributes on a span at the most (`span.kind`, `duration`, `parent_id`,
/// `otel.status_code`, `env`, `version`, `error.type`, `error.message`, `error.stack`,
/// `peer.service`, `http.status_code`, `events`, `links`, `issue.id`): room kept for them under
/// [`MAX_TAGS`].
const OWN: usize = 14;
/// kiln's own that a span's attribute of the same name never stands for (an issue's id is kiln's
/// alone: one a span came with is not taken).
const FIXED: [&str; 5] = ["span.kind", "duration", "parent_id", "otel.status_code", "issue.id"];
/// Where a client's or a producer's span's `peer.service` comes from when it does not say one: the
/// first of these it has, as Datadog infers a service from its peer tags (the database's system, the
/// messaging system, the RPC service, the address called).
pub const PEER_SOURCES: [&str; 9] = ["peer.service", "db.system.name", "db.system", "messaging.system", "rpc.service", "server.address", "net.peer.name", "out.host", "network.destination.name"];

/// `Span.SpanKind`'s names by number; UNSPECIFIED read as INTERNAL, as the specification allows.
pub const KINDS: [&str; 6] = ["internal", "internal", "server", "client", "producer", "consumer"];
const KIND_SERVER: u64 = 2;
const KIND_CLIENT: u64 = 3;
const KIND_PRODUCER: u64 = 4;
const KIND_CONSUMER: u64 = 5;
/// `Status.StatusCode`.
const STATUS_OK: u64 = 1;
const STATUS_ERROR: u64 = 2;
/// A span's severity: an error's (`status:error`), else info's (`status:ok`).
pub const SEVERITY_ERROR: u8 = 17;
pub const SEVERITY_OK: u8 = 9;

/// What a request's spans came to.
#[derive(Debug, Default)]
pub struct Spans {
    /// Rows, each segment's times chained from 0, of at most [`SEGMENT_BYTES`] (but its last row),
    /// and how many each holds.
    pub segments: Vec<(Vec<u8>, u32)>,
    /// Spans kept.
    pub spans: u64,
    /// Spans without a valid trace id or span id: not kept.
    pub invalid: u64,
    /// The spans' attributes past [`MAX_TAGS`], dropped in key order.
    pub dropped_attrs: u64,
    /// Spans past [`MAX_ROW_BYTES`], their longest values cut.
    pub cut: u64,
}

impl Spans {
    /// Bytes of rows.
    pub fn bytes(&self) -> usize {
        self.segments.iter().map(|(r, _)| r.len()).sum()
    }
}

/// A value: the request's own text, or text the decoder wrote (`Text(start, end)` in it).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Val<'a> {
    Str(&'a str),
    Text(usize, usize),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Attr<'a> {
    pub(crate) key: &'a str,
    pub(crate) val: Val<'a>,
}

/// Decodes requests, its room kept from one to the next.
#[derive(Debug, Default)]
pub struct Decoder {
    /// Values written (the resource's first, then a span's).
    text: String,
    /// A value rendered, before it is escaped into an event's or a link's JSON.
    tmp: String,
    resource: Vec<Attr<'static>>,
    attrs: Vec<Attr<'static>>,
    /// A span's attributes', events' and links' messages, each with its field.
    parts: Vec<(u32, &'static [u8])>,
    /// Row buffers handed back ([`Decoder::give`]).
    spare: Vec<Vec<u8>>,
}

/// A vector emptied, as one of borrows of another lifetime, its room kept (collected in place: the
/// same layout, nothing allocated; the map is of nothing).
pub(crate) fn attrs_for<'b>(mut v: Vec<Attr<'_>>) -> Vec<Attr<'b>> {
    v.clear();
    v.into_iter().map(|_| Attr { key: "", val: Val::Str("") }).collect()
}

pub(crate) fn parts_for<'b>(mut v: Vec<(u32, &[u8])>) -> Vec<(u32, &'b [u8])> {
    v.clear();
    v.into_iter().map(|_| (0, &[][..])).collect()
}

/// `s`, at most `max` bytes of it, cut where a character starts.
pub(crate) fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut at = max;
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    s.get(..at).unwrap_or("")
}

pub(crate) fn id<const N: usize>(b: Option<&[u8]>) -> Option<[u8; N]> {
    b.and_then(|b| <[u8; N]>::try_from(b).ok()).filter(|i| i.iter().any(|&x| x != 0))
}

/// A `KeyValue` as an attribute: its key and its value (a string the request's own, anything else
/// written into `text`).
fn attr<'a>(msg: &'a [u8], text: &mut String) -> Result<Option<Attr<'a>>, OtlpError> {
    let (mut key, mut value) = (None, None);
    for f in Fields::new(msg) {
        match f? {
            (1, w) => key = w.str(),
            (2, w) => value = Some(w.bytes().unwrap_or(&[])),
            _ => {}
        }
    }
    let Some(key) = key.filter(|k| !k.is_empty()) else { return Ok(None) };
    let val = match value {
        Some(v) => value_of(v, text)?,
        None => Val::Str(""),
    };
    Ok(Some(Attr { key: cut(key, MAX_NAME_BYTES), val }))
}

fn value_of<'a>(v: &'a [u8], text: &mut String) -> Result<Val<'a>, OtlpError> {
    for f in Fields::new(v) {
        match f? {
            (1, w) => return Ok(Val::Str(cut(w.str().unwrap_or(""), MAX_VALUE_BYTES))),
            (2..=7, _) => {
                let start = text.len();
                any_value_push(v, MAX_VALUE_BYTES, text)?;
                return Ok(Val::Text(start, text.len()));
            }
            _ => {}
        }
    }
    Ok(Val::Str(""))
}

pub(crate) fn resolve<'a>(v: Val<'a>, text: &'a str) -> &'a str {
    match v {
        Val::Str(s) => s,
        Val::Text(a, b) => text.get(a..b).unwrap_or(""),
    }
}

/// A span's error's issue as `issue.id` (sixteen hex digits), where it is one
/// ([`crate::errors::span_issue`]).
pub(crate) fn put_issue(attrs: &mut Vec<Attr<'_>>, text: &mut String, service: Option<Val<'_>>, entry: bool) {
    let id = {
        let t: &str = text;
        let service = service.map_or("", |v| resolve(v, t));
        crate::errors::span_issue(service, entry, &crate::errors::Error::of(attrs.iter().map(|a| (a.key, resolve(a.val, t)))))
    };
    if let Some(id) = id {
        let at = text.len();
        let _ = write!(text, "{id:016x}");
        attrs.push(Attr { key: "issue.id", val: Val::Text(at, text.len()) });
    }
}

pub(crate) fn find<'a>(attrs: &[Attr<'a>], key: &str) -> Option<Val<'a>> {
    attrs.iter().find(|a| a.key == key).map(|a| a.val)
}

/// A segment being written: its rows, how many, the last one's time.
pub(crate) struct Seg {
    rows: Vec<u8>,
    count: u32,
    prev: i64,
}

/// A span's row written into the segment being written (a new one past [`SEGMENT_BYTES`]), its
/// attributes first brought within [`MAX_ROW_BYTES`] (`names`: its service, host and source).
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_row(
    seg: &mut Option<Seg>,
    spare: &mut Vec<Vec<u8>>,
    out: &mut Spans,
    attrs: &mut [Attr<'_>],
    text: &str,
    ts: i64,
    severity: u8,
    names: [&str; 3],
    name: &str,
    trace: [u8; 16],
    span: [u8; 8],
) {
    let seg = match seg {
        Some(s) if s.rows.len() < SEGMENT_BYTES => s,
        slot => {
            if let Some(full) = slot.take() {
                out.segments.push((full.rows, full.count));
            }
            slot.insert(Seg { rows: spare.pop().unwrap_or_default(), count: 0, prev: 0 })
        }
    };
    let name = cut(name, MAX_NAME);
    let [service, host, source] = names;
    if fit(attrs, text, service.len() + host.len() + source.len() + name.len() + ROW_OVERHEAD) {
        out.cut += 1;
    }
    let pairs = attrs.iter().map(|a| (a.key, resolve(a.val, text)));
    put_fields(&mut seg.rows, ts, severity, names, name, pairs, attrs.len(), Some(trace), Some(span), seg.prev, MAX_VALUE_BYTES);
    seg.prev = ts;
    seg.count += 1;
    out.spans += 1;
}

/// A span's attributes (`attrs[own..]`, its own) within `room`, past it dropped in key order; then
/// those `under` it (its resource's; a Datadog span's chunk's and payload's) where it has none of
/// the name and `skip` does not say no, while room is left, and past it dropped; each drop counted.
/// Each list `under` holds [`MAX_TAGS`] at the most, so this takes `MAX_TAGS × room` comparisons
/// at the most, whatever a request holds.
pub(crate) fn merge_under<'a>(attrs: &mut Vec<Attr<'a>>, own: usize, room: usize, under: &[&[Attr<'a>]], skip: &dyn Fn(&str) -> bool, dropped: &mut u64) {
    let theirs = attrs.len() - own;
    if theirs > room {
        if let Some(user) = attrs.get_mut(own..) {
            user.sort_unstable_by(|a, b| a.key.cmp(b.key));
        }
        *dropped += (theirs - room) as u64;
        attrs.truncate(own + room);
    }
    for list in under {
        for (i, u) in list.iter().enumerate() {
            if skip(u.key) || attrs.get(own..).unwrap_or_default().iter().any(|a| a.key == u.key) {
                continue;
            }
            if attrs.len() - own >= room {
                *dropped += (list.len() - i) as u64;
                break;
            }
            attrs.push(*u);
        }
    }
}

/// A resource's attributes at most [`MAX_TAGS`] (what a row could hold), past it dropped in key
/// order and counted.
pub(crate) fn bound_under(attrs: &mut Vec<Attr<'_>>, dropped: &mut u64) {
    if attrs.len() > MAX_TAGS {
        attrs.sort_unstable_by(|a, b| a.key.cmp(b.key));
        *dropped += (attrs.len() - MAX_TAGS) as u64;
        attrs.truncate(MAX_TAGS);
    }
}

/// The segment being written, done: into `out` if it holds a row.
pub(crate) fn finish_segment(seg: &mut Option<Seg>, out: &mut Spans) {
    if let Some(s) = seg.take()
        && s.count > 0
    {
        out.segments.push((s.rows, s.count));
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Row buffers back, to write the next request's rows into.
    pub fn give(&mut self, spans: Spans) {
        for (rows, _) in spans.segments {
            self.give_rows(rows);
        }
    }

    /// A buffer to write rows into (one a write held and handed back, `rows` emptied); past
    /// [`SPARES`] kept, it goes.
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

    /// The spans of an `ExportTraceServiceRequest` (or a `TracesData`: the same message) into
    /// `out`; `now_ns` stands for a span that says no time.
    pub fn decode(&mut self, body: &[u8], now_ns: i64, out: &mut Spans) -> Result<(), OtlpError> {
        let mut cx = Cx {
            text: std::mem::take(&mut self.text),
            tmp: std::mem::take(&mut self.tmp),
            resource: attrs_for(std::mem::take(&mut self.resource)),
            attrs: attrs_for(std::mem::take(&mut self.attrs)),
            parts: parts_for(std::mem::take(&mut self.parts)),
            spare: &mut self.spare,
            seg: None,
        };
        let r = (|| {
            for f in Fields::new(body) {
                if let (1, w) = f? {
                    cx.resource_spans(w.bytes().unwrap_or(&[]), now_ns, out)?;
                }
            }
            Ok(())
        })();
        finish_segment(&mut cx.seg, out);
        let Cx { mut text, mut tmp, resource, attrs, parts, .. } = cx;
        text.clear();
        tmp.clear();
        (self.text, self.tmp) = (text, tmp);
        self.resource = attrs_for(resource);
        self.attrs = attrs_for(attrs);
        self.parts = parts_for(parts);
        r
    }
}

/// One request's decoding: the decoder's room, lent for its borrows.
struct Cx<'a, 'd> {
    text: String,
    tmp: String,
    resource: Vec<Attr<'a>>,
    attrs: Vec<Attr<'a>>,
    parts: Vec<(u32, &'a [u8])>,
    spare: &'d mut Vec<Vec<u8>>,
    seg: Option<Seg>,
}

/// What a resource's spans share.
struct Res<'a> {
    service: Option<Val<'a>>,
    host: Option<Val<'a>>,
    /// Bytes of `text` the resource's values take.
    text: usize,
}

impl<'a> Cx<'a, '_> {
    fn resource_spans(&mut self, msg: &'a [u8], now_ns: i64, out: &mut Spans) -> Result<(), OtlpError> {
        self.text.clear();
        self.resource.clear();
        for f in Fields::new(msg) {
            if let (1, w) = f? {
                for f in Fields::new(w.bytes().unwrap_or(&[])) {
                    if let (1, kv) = f?
                        && let Some(a) = attr(kv.bytes().unwrap_or(&[]), &mut self.text)?
                    {
                        self.resource.push(a);
                    }
                }
            }
        }
        let res = Res { service: find(&self.resource, "service.name"), host: find(&self.resource, "host.name"), text: self.text.len() };
        self.resource.retain(|a| a.key != "service.name" && a.key != "host.name");
        bound_under(&mut self.resource, &mut out.dropped_attrs);
        for f in Fields::new(msg) {
            if let (2, w) = f? {
                let scope = w.bytes().unwrap_or(&[]);
                let mut name = "";
                for f in Fields::new(scope) {
                    if let (1, s) = f? {
                        for f in Fields::new(s.bytes().unwrap_or(&[])) {
                            if let (1, n) = f? {
                                name = n.str().unwrap_or("");
                            }
                        }
                    }
                }
                let source = if name.is_empty() { "otlp" } else { cut(name, MAX_NAME_BYTES) };
                for f in Fields::new(scope) {
                    if let (2, s) = f? {
                        self.span(s.bytes().unwrap_or(&[]), &res, source, now_ns, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn span(&mut self, msg: &'a [u8], res: &Res<'a>, source: &'a str, now_ns: i64, out: &mut Spans) -> Result<(), OtlpError> {
        let (mut trace, mut span, mut parent) = (None, None, None::<[u8; 8]>);
        let (mut name, mut kind, mut start, mut end) = ("", 0u64, 0u64, 0u64);
        let (mut status, mut status_msg) = (0u64, "");
        self.parts.clear();
        for f in Fields::new(msg) {
            match f? {
                (1, w) => trace = id::<16>(w.bytes()),
                (2, w) => span = id::<8>(w.bytes()),
                (4, w) => parent = id::<8>(w.bytes()),
                (5, w) => name = w.str().unwrap_or(""),
                (6, w) => kind = w.u64().unwrap_or(0),
                (7, w) => start = w.u64().unwrap_or(0),
                (8, w) => end = w.u64().unwrap_or(0),
                (n @ (9 | 11 | 13), w) => self.parts.push((n, w.bytes().unwrap_or(&[]))),
                (15, w) => {
                    for f in Fields::new(w.bytes().unwrap_or(&[])) {
                        match f? {
                            (2, m) => status_msg = m.str().unwrap_or(""),
                            (3, c) => status = c.u64().unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let (Some(trace), Some(span)) = (trace, span) else {
            out.invalid += 1;
            return Ok(());
        };
        self.text.truncate(res.text);
        self.attrs.clear();
        // kiln's own first.
        let kind_name = usize::try_from(kind).ok().and_then(|k| KINDS.get(k)).copied().unwrap_or("internal");
        self.attrs.push(Attr { key: "span.kind", val: Val::Str(kind_name) });
        let at = self.text.len();
        let _ = write!(self.text, "{}", end.saturating_sub(start));
        self.attrs.push(Attr { key: "duration", val: Val::Text(at, self.text.len()) });
        if let Some(p) = parent {
            let at = self.text.len();
            for b in p {
                let _ = write!(self.text, "{b:02x}");
            }
            self.attrs.push(Attr { key: "parent_id", val: Val::Text(at, self.text.len()) });
        }
        match status {
            STATUS_OK => self.attrs.push(Attr { key: "otel.status_code", val: Val::Str("OK") }),
            STATUS_ERROR => self.attrs.push(Attr { key: "otel.status_code", val: Val::Str("ERROR") }),
            _ => {}
        }
        let own = self.attrs.len();
        // The span's, then its resource's where the span has none of the name.
        for i in 0..self.parts.len() {
            let Some(&(9, p)) = self.parts.get(i) else { continue };
            if let Some(a) = attr(p, &mut self.text)?
                && !FIXED.contains(&a.key)
            {
                self.attrs.push(a);
            }
        }
        // An LLM span (`crate::llm::spans`), read whole before its attributes are bounded: room kept
        // for kiln's LLM attributes.
        let llm = crate::llm::spans::read_attrs(name, self.attrs.get(own..).unwrap_or_default(), &self.resource, &self.text);
        let room = MAX_TAGS - OWN - llm.as_ref().map_or(0, |_| crate::llm::spans::OWN);
        merge_under(&mut self.attrs, own, room, &[self.resource.as_slice()], &|k| FIXED.contains(&k), &mut out.dropped_attrs);
        // Where the span does not say them: the unified service tags, the error, the peer.
        let has = |attrs: &[Attr<'_>], k: &str| attrs.iter().any(|a| a.key == k);
        if !has(&self.attrs, "env")
            && let Some(v) = find(&self.attrs, "deployment.environment.name").or_else(|| find(&self.attrs, "deployment.environment"))
        {
            self.attrs.push(Attr { key: "env", val: v });
        }
        if !has(&self.attrs, "version")
            && let Some(v) = find(&self.attrs, "service.version")
        {
            self.attrs.push(Attr { key: "version", val: v });
        }
        if !has(&self.attrs, "http.status_code")
            && let Some(v) = find(&self.attrs, "http.response.status_code")
        {
            self.attrs.push(Attr { key: "http.status_code", val: v });
        }
        if (kind == KIND_CLIENT || kind == KIND_PRODUCER)
            && !has(&self.attrs, "peer.service")
            && let Some(v) = PEER_SOURCES.iter().find_map(|k| find(&self.attrs, k).filter(|v| !resolve(*v, &self.text).is_empty()))
        {
            self.attrs.push(Attr { key: "peer.service", val: v });
        }
        self.events_and_error(status, status_msg, start)?;
        self.links()?;
        // An error's issue (`crate::errors`): a service's entry span's (its first, a server's, a
        // consumer's).
        if status == STATUS_ERROR {
            let entry = parent.is_none() || kind == KIND_SERVER || kind == KIND_CONSUMER;
            put_issue(&mut self.attrs, &mut self.text, res.service, entry);
        }
        // The row.
        let ts = if start != 0 {
            start
        } else if end != 0 {
            end
        } else {
            u64::try_from(now_ns).unwrap_or(0)
        };
        let ts = i64::try_from(ts).unwrap_or(i64::MAX);
        if let Some(r) = &llm {
            crate::llm::spans::put(&mut self.attrs, &mut self.text, r, res.service, ts);
        }
        let (service, host) = (res.service.map_or("", |v| resolve(v, &self.text)), res.host.map_or("", |v| resolve(v, &self.text)));
        let severity = if status == STATUS_ERROR { SEVERITY_ERROR } else { SEVERITY_OK };
        write_row(&mut self.seg, self.spare, out, &mut self.attrs, &self.text, ts, severity, [service, host, source], name, trace, span);
        Ok(())
    }

    /// The span's events as `events` (JSON: each one's name, its time from the span's start, its
    /// attributes), and its error as `error.type`, `error.message` and `error.stack`, from its first
    /// exception event, else (an error's) its status's message: each where the span has none.
    fn events_and_error(&mut self, status: u64, status_msg: &'a str, start: u64) -> Result<(), OtlpError> {
        let (mut ty, mut message, mut stack) = (None, None, None);
        let at = self.text.len();
        let (mut any, mut excepted) = (false, false);
        for i in 0..self.parts.len() {
            let Some(&(11, ev)) = self.parts.get(i) else { continue };
            let (mut time, mut name) = (0u64, "");
            for f in Fields::new(ev) {
                match f? {
                    (1, w) => time = w.u64().unwrap_or(0),
                    (2, w) => name = w.str().unwrap_or(""),
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
                        (1, w) => k = w.str().unwrap_or(""),
                        (2, w) => v = w.bytes(),
                        _ => {}
                    }
                }
                if k.is_empty() {
                    continue;
                }
                self.tmp.clear();
                any_value_push(v.unwrap_or(&[]), MAX_VALUE_BYTES, &mut self.tmp)?;
                if !first {
                    self.text.push(',');
                }
                first = false;
                push_json_str(&mut self.text, cut(k, MAX_NAME_BYTES));
                self.text.push(':');
                push_json_str(&mut self.text, &self.tmp);
                if exception {
                    // The exception's own (text, as the semantic conventions have them), the
                    // request's.
                    let slot = match k {
                        "exception.type" => &mut ty,
                        "exception.message" => &mut message,
                        "exception.stacktrace" => &mut stack,
                        _ => continue,
                    };
                    *slot = v.and_then(string_of);
                }
            }
            self.text.push_str("}}");
            any = true;
            if self.text.len() - at > MAX_VALUE_BYTES {
                // Past a value's bound: the events before it.
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
        // A stack whole: its row keeps its ends (`crate::errors::stack_ends`).
        let error = |key: &'static str, v: Option<&'a str>, attrs: &mut Vec<Attr<'a>>| {
            if let Some(v) = v
                && !attrs.iter().any(|a| a.key == key)
            {
                attrs.push(Attr { key, val: Val::Str(if key == "error.stack" { v } else { cut(v, MAX_VALUE_BYTES) }) });
            }
        };
        let message = message.or_else(|| (status == STATUS_ERROR && !status_msg.is_empty()).then_some(status_msg));
        error("error.type", ty, &mut self.attrs);
        error("error.message", message, &mut self.attrs);
        error("error.stack", stack, &mut self.attrs);
        if let Some(e) = events
            && !self.attrs.iter().any(|a| a.key == "events")
        {
            self.attrs.push(Attr { key: "events", val: e });
        }
        Ok(())
    }

    /// The span's links as `links` (JSON: each one's trace id, span id and attributes).
    fn links(&mut self) -> Result<(), OtlpError> {
        let at = self.text.len();
        let mut any = false;
        for i in 0..self.parts.len() {
            let Some(&(13, link)) = self.parts.get(i) else { continue };
            let (mut trace, mut span) = (None, None);
            for f in Fields::new(link) {
                match f? {
                    (1, w) => trace = id::<16>(w.bytes()),
                    (2, w) => span = id::<8>(w.bytes()),
                    _ => {}
                }
            }
            let (Some(trace), Some(span)) = (trace, span) else { continue };
            let before = self.text.len();
            self.text.push(if any { ',' } else { '[' });
            self.text.push_str("{\"trace_id\":\"");
            for b in trace {
                let _ = write!(self.text, "{b:02x}");
            }
            self.text.push_str("\",\"span_id\":\"");
            for b in span {
                let _ = write!(self.text, "{b:02x}");
            }
            self.text.push_str("\",\"attributes\":{");
            let mut first = true;
            for f in Fields::new(link) {
                let (4, kv) = f? else { continue };
                let (mut k, mut v) = ("", None);
                for f in Fields::new(kv.bytes().unwrap_or(&[])) {
                    match f? {
                        (1, w) => k = w.str().unwrap_or(""),
                        (2, w) => v = w.bytes(),
                        _ => {}
                    }
                }
                if k.is_empty() {
                    continue;
                }
                self.tmp.clear();
                any_value_push(v.unwrap_or(&[]), MAX_VALUE_BYTES, &mut self.tmp)?;
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

/// A value's length.
fn len_of(v: Val<'_>) -> usize {
    match v {
        Val::Str(s) => s.len(),
        Val::Text(a, b) => b.saturating_sub(a),
    }
}

/// `v` at most `max` bytes, cut where a character starts.
fn cut_val<'a>(v: Val<'a>, max: usize, text: &str) -> Val<'a> {
    match v {
        Val::Str(s) => Val::Str(cut(s, max)),
        Val::Text(a, b) => Val::Text(a, a + text.get(a..b).map_or(0, |t| cut(t, max).len())),
    }
}

/// Brings a row of these attributes and `fixed` bytes besides within [`MAX_ROW_BYTES`]: the values'
/// longest cut to the one length that does (water-filling); whether any was.
fn fit(attrs: &mut [Attr<'_>], text: &str, fixed: usize) -> bool {
    let keys: usize = attrs.iter().map(|a| a.key.len() + 6).sum();
    let values: usize = attrs.iter().map(|a| len_of(a.val)).sum();
    let Some(mut room) = MAX_ROW_BYTES.checked_sub(fixed + keys) else {
        // Keys alone past it: every value cut to nothing (keys are 800 bytes and 128 at the most:
        // 103 KB, so this is a name or a host past reason).
        for a in attrs.iter_mut() {
            a.val = cut_val(a.val, 0, text);
        }
        return true;
    };
    if values <= room {
        return false;
    }
    let mut lens: Vec<usize> = attrs.iter().map(|a| len_of(a.val)).collect();
    lens.sort_unstable();
    let mut cap = 0;
    for (i, &l) in lens.iter().enumerate() {
        let left = lens.len() - i;
        if l.saturating_mul(left) > room {
            cap = room / left;
            break;
        }
        room -= l;
    }
    for a in attrs.iter_mut() {
        if len_of(a.val) > cap {
            a.val = cut_val(a.val, cap, text);
        }
    }
    true
}

/// An `AnyValue`'s string, if it is one.
fn string_of(v: &[u8]) -> Option<&str> {
    Fields::new(v).find_map(|f| match f {
        Ok((1, w)) => w.str(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::record::{RecordRef, Rows};
    use crate::otlp::pb::Writer;

    fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
        w.message(field, |m| {
            m.str(1, k);
            m.message(2, |a| a.str(1, v));
        });
    }

    fn kv_int(w: &mut Writer, field: u32, k: &str, v: u64) {
        w.message(field, |m| {
            m.str(1, k);
            m.message(2, |a| a.varint(3, v));
        });
    }

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

    /// A checkout's server span with its database call (failed) under it.
    fn request() -> Writer {
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(1, |res| {
                kv(res, 1, "service.name", "checkout");
                kv(res, 1, "host.name", "web-1");
                kv(res, 1, "deployment.environment.name", "prod");
                kv(res, 1, "service.version", "1.4.2");
                kv(res, 1, "telemetry.sdk.language", "python");
            });
            rs.message(2, |ss| {
                ss.message(1, |scope| scope.str(1, "opentelemetry.instrumentation.flask"));
                ss.message(2, |s| {
                    s.bytes(1, &[0xab; 16]);
                    s.bytes(2, &[1; 8]);
                    s.str(5, "POST /cart/{id}");
                    s.varint(6, 2);
                    s.fixed64(7, 1_700_000_000_000_000_000);
                    s.fixed64(8, 1_700_000_000_250_000_000);
                    kv(s, 9, "http.route", "/cart/{id}");
                    kv_int(s, 9, "http.response.status_code", 500);
                    // The span's own over its resource's.
                    kv(s, 9, "telemetry.sdk.language", "python3");
                    // Not standing for kiln's own.
                    kv(s, 9, "duration", "forever");
                    s.message(15, |st| {
                        st.str(2, "internal error");
                        st.varint(3, 2);
                    });
                });
                ss.message(2, |s| {
                    s.bytes(1, &[0xab; 16]);
                    s.bytes(2, &[2; 8]);
                    s.bytes(4, &[1; 8]);
                    s.str(5, "SELECT carts");
                    s.varint(6, 3);
                    s.fixed64(7, 1_700_000_000_010_000_000);
                    s.fixed64(8, 1_700_000_000_240_000_000);
                    kv(s, 9, "db.system", "postgresql");
                    kv(s, 9, "server.address", "db-1");
                    s.message(11, |e| {
                        e.fixed64(1, 1_700_000_000_239_000_000);
                        e.str(2, "exception");
                        kv(e, 3, "exception.type", "psycopg.errors.QueryCanceled");
                        kv(e, 3, "exception.message", "canceling statement due to statement timeout");
                        kv(e, 3, "exception.stacktrace", "Traceback (most recent call last):\n  File \"app.py\", line 9");
                    });
                    s.message(13, |l| {
                        l.bytes(1, &[0xcd; 16]);
                        l.bytes(2, &[3; 8]);
                        kv(l, 4, "link.kind", "retry-of");
                    });
                    s.message(15, |st| st.varint(3, 2));
                });
                // No trace id: not kept.
                ss.message(2, |s| {
                    s.bytes(2, &[4; 8]);
                    s.str(5, "lost");
                });
            });
        });
        req
    }

    /// An error is an issue at its service's entry (`crate::errors`): a root's or a server's span
    /// in error with its exception's type, message and stack; not a call's, nor a span not in error
    /// (an exception handled), and an `issue.id` a span came with is never kept.
    #[test]
    fn an_entry_span_s_error_is_an_issue_and_a_call_s_is_not() {
        let text = include_str!("../fixtures/errors/python.txt");
        let stack = text.split("---- app\n").nth(1).and_then(|t| t.split("---- ").next()).unwrap();
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(1, |res| kv(res, 1, "service.name", "checkout"));
            rs.message(2, |ss| {
                for (id, parent, kind, status, exception) in [(1u8, None, 2u64, 2u64, true), (2, Some(1u8), 3, 2, true), (3, None, 2, 1, true), (4, None, 2, 2, false)] {
                    ss.message(2, |s| {
                        s.bytes(1, &[0xab; 16]);
                        s.bytes(2, &[id; 8]);
                        if let Some(p) = parent {
                            s.bytes(4, &[p; 8]);
                        }
                        s.str(5, "POST /cart");
                        s.varint(6, kind);
                        s.fixed64(7, 1_700_000_000_000_000_000);
                        s.fixed64(8, 1_700_000_000_001_000_000);
                        kv(s, 9, "issue.id", "deadbeefdeadbeef");
                        if exception {
                            s.message(11, |e| {
                                e.str(2, "exception");
                                kv(e, 3, "exception.type", "ValueError");
                                kv(e, 3, "exception.message", "bad item -42 in cart 'c--294'");
                                kv(e, 3, "exception.stacktrace", stack);
                            });
                        }
                        s.message(15, |st| st.varint(3, status));
                    });
                }
            });
        });
        let mut d = Decoder::new();
        let mut out = Spans::default();
        d.decode(&req.buf, 0, &mut out).unwrap();
        let r = rows(&out);
        let want =
            crate::errors::span_issue("checkout", true, &crate::errors::Error { kind: Some("ValueError"), message: Some("bad item -42 in cart 'c--294'"), stack: Some(stack), fingerprint: None })
                .unwrap();
        let ids: Vec<Option<&str>> = r.iter().map(|x| x.attr("issue.id")).collect();
        assert_eq!(ids, vec![Some(crate::errors::hex(want).as_str()), None, None, None], "the entry's in error alone; none of the span's own");
    }

    #[test]
    fn spans_are_rows_with_their_resource_s_service_and_kiln_s_attributes() {
        let req = request();
        let mut d = Decoder::new();
        let mut s = Spans::default();
        d.decode(&req.buf, 42, &mut s).unwrap();
        assert_eq!((s.spans, s.invalid, s.segments.len()), (2, 1, 1));
        let r = rows(&s);
        let (server, db) = (&r[0], &r[1]);
        assert_eq!(
            (server.ts, server.severity, server.service, server.host, server.source, server.message),
            (1_700_000_000_000_000_000, 17, "checkout", "web-1", "opentelemetry.instrumentation.flask", "POST /cart/{id}")
        );
        assert_eq!((server.trace_id, server.span_id), (Some([0xab; 16]), Some([1; 8])));
        let a = |r: &RecordRef<'_>, k: &str| r.attr(k).map(str::to_owned);
        assert_eq!(a(server, "span.kind").as_deref(), Some("server"));
        assert_eq!(a(server, "duration").as_deref(), Some("250000000"));
        assert_eq!(a(server, "parent_id"), None);
        assert_eq!(a(server, "otel.status_code").as_deref(), Some("ERROR"));
        assert_eq!(a(server, "http.response.status_code").as_deref(), Some("500"));
        assert_eq!(a(server, "http.status_code").as_deref(), Some("500"), "Datadog's name for it");
        assert_eq!(a(server, "telemetry.sdk.language").as_deref(), Some("python3"));
        assert_eq!((a(server, "env").as_deref(), a(server, "version").as_deref()), (Some("prod"), Some("1.4.2")));
        assert_eq!(a(server, "error.message").as_deref(), Some("internal error"));
        assert_eq!(a(server, "service.name"), None);
        assert_eq!(server.attrs().filter(|(k, _)| *k == "duration").count(), 1, "the span's own duration does not stand for kiln's");
        assert_eq!(a(server, "peer.service"), None, "a server's span names no peer");
        assert_eq!((db.ts, db.severity, db.message, db.span_id), (1_700_000_000_010_000_000, 17, "SELECT carts", Some([2; 8])));
        assert_eq!(a(db, "parent_id").as_deref(), Some("0101010101010101"));
        assert_eq!(a(db, "span.kind").as_deref(), Some("client"));
        assert_eq!(a(db, "peer.service").as_deref(), Some("postgresql"), "the database's system before the address");
        assert_eq!(a(db, "error.type").as_deref(), Some("psycopg.errors.QueryCanceled"));
        assert_eq!(a(db, "error.message").as_deref(), Some("canceling statement due to statement timeout"));
        assert!(a(db, "error.stack").is_some_and(|s| s.starts_with("Traceback")));
        let events: serde_json::Value = serde_json::from_str(&a(db, "events").unwrap()).unwrap();
        assert_eq!(events[0]["name"], "exception");
        assert_eq!(events[0]["offset_ns"], 229_000_000);
        assert_eq!(events[0]["attributes"]["exception.type"], "psycopg.errors.QueryCanceled");
        let links: serde_json::Value = serde_json::from_str(&a(db, "links").unwrap()).unwrap();
        assert_eq!(links[0]["trace_id"], "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd");
        assert_eq!(links[0]["span_id"], "0303030303030303");
        assert_eq!(links[0]["attributes"]["link.kind"], "retry-of");
    }

    #[test]
    fn attributes_past_the_limit_go_in_key_order_and_kiln_s_stay() {
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(2, |ss| {
                ss.message(2, |s| {
                    s.bytes(1, &[1; 16]);
                    s.bytes(2, &[1; 8]);
                    s.str(5, "many");
                    s.varint(6, 3);
                    for i in 0..200 {
                        kv(s, 9, &format!("a{i:03}"), "v");
                    }
                    kv(s, 9, "server.address", "api.example.com");
                });
            });
        });
        let mut s = Spans::default();
        Decoder::new().decode(&req.buf, 7, &mut s).unwrap();
        let r = rows(&s);
        let row = &r[0];
        assert_eq!(row.attr_count, 2 + MAX_TAGS - OWN, "kiln's own (kind and duration) and the span's up to the room");
        assert_eq!(s.dropped_attrs, 201 - (MAX_TAGS - OWN) as u64);
        assert_eq!(row.attr("a000"), Some("v"));
        assert_eq!(row.attr("a199"), None, "dropped in key order");
        assert_eq!(row.attr("span.kind"), Some("client"));
        assert_eq!(row.ts, 7, "no time: the time it came");
        assert_eq!(row.source, "otlp");
        // The address went with the rest (in key order, past the room): no peer of it.
        assert_eq!(row.attr("peer.service"), None);
    }

    #[test]
    fn a_resource_and_its_span_of_huge_attribute_lists_merge_in_bounded_work() {
        // 40,000 attributes on the resource and on each of its spans: merged by comparing every
        // one with every other, a request's 3.2 billion comparisons; bounded, some thousands.
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(1, |res| {
                for i in 0..40_000 {
                    kv(res, 1, &format!("r{i:05}"), "v");
                }
                kv(res, 1, "service.name", "wide");
            });
            rs.message(2, |ss| {
                for n in 0..2u8 {
                    ss.message(2, |s| {
                        s.bytes(1, &[1; 16]);
                        s.bytes(2, &[n + 1; 8]);
                        s.str(5, "wide");
                        for i in 0..40_000 {
                            kv(s, 9, &format!("s{i:05}"), "v");
                        }
                    });
                }
            });
        });
        let started = std::time::Instant::now();
        let mut s = Spans::default();
        Decoder::new().decode(&req.buf, 7, &mut s).unwrap();
        let took = started.elapsed();
        let r = rows(&s);
        assert_eq!(r.len(), 2);
        for row in &r {
            assert_eq!((row.service, row.attr_count), ("wide", 2 + MAX_TAGS - OWN), "kiln's own (kind and duration) and the span's up to the room");
            assert_eq!(row.attr("s00000"), Some("v"));
            assert_eq!(row.attr("r00000"), None, "the span's own fill the room first");
        }
        // The resource's past MAX_TAGS once, and each span's past the room, and the resource's
        // that no longer fit.
        assert_eq!(s.dropped_attrs, (40_000 - MAX_TAGS as u64) + 2 * ((40_000 - (MAX_TAGS - OWN) as u64) + MAX_TAGS as u64));
        assert!(took < std::time::Duration::from_secs(20), "{took:?}");
    }

    #[test]
    fn a_span_past_its_row_s_bound_has_its_longest_values_cut_to_fit() {
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(2, |ss| {
                ss.message(2, |s| {
                    s.bytes(1, &[1; 16]);
                    s.bytes(2, &[1; 8]);
                    s.str(5, "huge");
                    // 20 values of 25,000 bytes (500 KB) and a few short ones.
                    for i in 0..20 {
                        kv(s, 9, &format!("big{i:02}"), &"é".repeat(12_500));
                    }
                    kv(s, 9, "short", "kept whole");
                });
            });
        });
        let mut s = Spans::default();
        Decoder::new().decode(&req.buf, 0, &mut s).unwrap();
        assert_eq!(s.cut, 1);
        let (raw, _) = &s.segments[0];
        assert!(raw.len() <= MAX_ROW_BYTES, "{}", raw.len());
        assert!(raw.len() > MAX_ROW_BYTES - 1024, "the cap is the least cut: {}", raw.len());
        let r = rows(&s);
        assert_eq!(r[0].attr("short"), Some("kept whole"));
        let bigs: Vec<usize> = r[0].attrs().filter(|(k, _)| k.starts_with("big")).map(|(_, v)| v.len()).collect();
        assert_eq!(bigs.len(), 20);
        assert!(bigs.iter().all(|&l| l == bigs[0] && l < 25_000 && l % 2 == 0), "one length, cut where a character starts: {bigs:?}");
    }

    #[test]
    fn many_spans_go_in_segments_each_from_zero() {
        let mut req = Writer::default();
        req.message(1, |rs| {
            rs.message(2, |ss| {
                for i in 0..3000u64 {
                    ss.message(2, |s| {
                        s.bytes(1, &(u128::from(i) + 1).to_le_bytes());
                        s.bytes(2, &(i + 1).to_le_bytes());
                        s.str(5, "GET /items");
                        s.fixed64(7, 1_700_000_000_000_000_000 + i * 1_000);
                        s.fixed64(8, 1_700_000_000_000_000_000 + i * 1_000 + 500);
                        kv(s, 9, "payload", &"x".repeat(200));
                    });
                }
            });
        });
        let mut d = Decoder::new();
        let mut s = Spans::default();
        d.decode(&req.buf, 0, &mut s).unwrap();
        assert!(s.segments.len() >= 2, "{}", s.segments.len());
        assert!(s.segments.iter().all(|(r, _)| r.len() < SEGMENT_BYTES + 1024));
        let r = rows(&s);
        assert_eq!(r.len(), 3000);
        assert!(r.iter().enumerate().all(|(i, row)| row.ts == 1_700_000_000_000_000_000 + i as i64 * 1_000 && row.attr("duration") == Some("500")));
        // Decoded again into buffers handed back: the same rows.
        let first: Vec<Vec<u8>> = s.segments.iter().map(|(r, _)| r.clone()).collect();
        d.give(s);
        let mut again = Spans::default();
        d.decode(&req.buf, 0, &mut again).unwrap();
        assert_eq!(again.segments.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(), first);
    }

    #[test]
    fn malformed_requests_are_errors_never_panics() {
        let req = request();
        let mut d = Decoder::new();
        let mut rng = crate::testkit::Rng::new(9);
        for _ in 0..20_000 {
            let mut bad = req.buf.clone();
            for _ in 0..=rng.below(3) {
                let i = rng.below(bad.len() as u64) as usize;
                bad[i] = rng.next_u64() as u8;
            }
            let cut = rng.below(bad.len() as u64 + 1) as usize;
            let mut s = Spans::default();
            if d.decode(&bad[..cut], 0, &mut s).is_ok() {
                // Whatever came of it reads back as rows.
                let _ = rows(&s).len();
            }
        }
    }
}
